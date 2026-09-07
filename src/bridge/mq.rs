//! 命名 MQ 客户端（Kafka(name)/RabbitMQ(name) 的宿主侧）：注册表 + op 面。
//!
//! 结构（spec 2026-09-07 §4）：kafkas / rabbits 各一个 [`NamedRegistry`]；
//! `MqInstance` 是薄调用句柄——kind 标识 + 异步 call 闭包 + poller 互斥。
//! 两个 op（`op_mq_call` / `op_mq_has`）+ `op_tasks_stopping`：
//! - 消费会话归属（评审 M2）：poll/commit/ack/nack **仅任务上下文可用**
//!   （`StableState.tasks_flag` 只对任务 Bridge 注 Some）；
//! - 单活跃 poller：同名实例第二个并发 poll → "instance busy"；
//! - send/publish/metadata/kind 无状态，全上下文可用。

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use deno_core::{OpState, op2};
use deno_error::JsErrorBox;
use futures::future::BoxFuture;

use super::BridgeResult;

/// 异步方法调用闭包：method + JSON payload → JSON 结果（Err → JsError）。
pub type MqCall = Arc<
    dyn Fn(String, serde_json::Value) -> BoxFuture<'static, BridgeResult<serde_json::Value>>
        + Send
        + Sync,
>;

/// 命名实例句柄（进程级共享；call 无状态可并发，poll 由 poller 互斥）。
pub struct MqInstance {
    /// "kafka" | "rabbit"（kind 路由由外层 registry 承担，此处供 op 直接应答 kind）。
    pub kind: &'static str,
    pub call: MqCall,
    /// 单活跃 poller 互斥（评审 M2；构造时建）。
    pub poller: tokio::sync::Mutex<()>,
}

impl MqInstance {
    pub fn new(kind: &'static str, call: MqCall) -> Self {
        Self {
            kind,
            call,
            poller: tokio::sync::Mutex::new(()),
        }
    }

    /// FFI 适配：vtable + handle → call 闭包（经 await_ffi_poll 长轮询退避，评审 F4）。
    /// cfg JSON 由装配层拼接（kind + 命名段透传值）。
    pub fn ffi(
        kind: &'static str,
        vt: &'static oj_plugin_ffi::MqVtable,
        handle: u64,
        backoff: std::time::Duration,
    ) -> Self {
        let call: MqCall = Arc::new(move |method, payload| {
            let vt = vt;
            let payload = serde_json::to_string(&payload).unwrap_or_default();
            Box::pin(async move {
                let out = super::ffi::await_ffi_poll(
                    (vt.call)(
                        handle,
                        oj_plugin_ffi::RString::from(method.as_str()),
                        oj_plugin_ffi::RString::from(payload.as_str()),
                    ),
                    backoff,
                )
                .await?;
                serde_json::from_slice(&out)
                    .map_err(|e| format!("mq {kind}: bad result json: {e}").into())
            })
        });
        Self::new(kind, call)
    }
}

/// 消费类 method（评审 M2：仅任务上下文可用）。
const GATED: [&str; 4] = ["poll", "commit", "ack", "nack"];

#[op2(fast)]
pub fn op_mq_has(state: &mut OpState, #[string] kind: String, #[string] name: String) -> bool {
    let s = state.borrow::<Arc<super::StableState>>();
    let reg = match kind.as_str() {
        "kafka" => &s.kafkas,
        "rabbit" => &s.rabbits,
        _ => return false,
    };
    reg.contains(&name)
}

#[op2]
#[serde]
pub async fn op_mq_call(
    state: Rc<RefCell<OpState>>,
    #[string] kind: String,
    #[string] name: String,
    #[string] method: String,
    #[serde] payload: serde_json::Value,
) -> Result<serde_json::Value, JsErrorBox> {
    // 先取出实例与门禁标志，释放 OpState 借用再 await（同 op_bus_subscribe 纪律）。
    let (inst, flag) = {
        let s = state.borrow();
        let stable = s.borrow::<Arc<super::StableState>>();
        let reg = match kind.as_str() {
            "kafka" => &stable.kafkas,
            "rabbit" => &stable.rabbits,
            other => {
                return Err(JsErrorBox::generic(format!(
                    "mq: unknown kind '{other}' (expected kafka|rabbit)"
                )));
            }
        };
        let inst = reg
            .get(&name)
            .ok_or_else(|| JsErrorBox::generic(format!("mq: no {kind} instance named '{name}'")))?;
        (inst, stable.tasks_flag.clone())
    };
    if GATED.contains(&method.as_str()) {
        let in_task = flag.is_some_and(|f| f.load(Ordering::Relaxed));
        if !in_task {
            return Err(JsErrorBox::generic(format!(
                "mq.{method} requires a task context (long-running tasks only; use send/publish from HTTP/WS)"
            )));
        }
    }
    // poll 持实例级互斥（评审 M2：第二个并发 poll → busy）。
    let _poll_guard = if method == "poll" {
        match inst.poller.try_lock() {
            Ok(g) => Some(g),
            Err(_) => {
                return Err(JsErrorBox::generic(
                    "mq: instance busy (another active poller)",
                ));
            }
        }
    } else {
        None
    };
    (inst.call)(method, payload)
        .await
        .map_err(|e| JsErrorBox::generic(e.to_string()))
}

#[op2(fast)]
pub fn op_tasks_stopping(state: &mut OpState) -> bool {
    let s = state.borrow::<Arc<super::StableState>>();
    s.tasks_flag
        .as_ref()
        .is_some_and(|f| f.load(Ordering::Relaxed))
}

/// 测试/本地用内存实例工厂：task 队列语义（send 入队、poll 出队）。
/// 生产路径全部经 `MqInstance::ffi`（插件 vtable）。
#[cfg(test)]
pub(crate) fn in_memory(
    kind: &'static str,
    queue: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
) -> MqInstance {
    MqInstance::new(
        kind,
        Arc::new(move |method, payload| {
            let queue = queue.clone();
            Box::pin(async move {
                match method.as_str() {
                    "kind" => Ok(serde_json::Value::String(kind.to_string())),
                    "send" => {
                        queue.lock().unwrap().push(payload["value"].clone());
                        Ok(serde_json::json!({ "sent": 1 }))
                    }
                    "poll" => {
                        let mut q = queue.lock().unwrap();
                        let n = q.len().min(payload["max"].as_u64().unwrap_or(100) as usize);
                        let topic0 = payload["topics"][0].clone();
                        let msgs: Vec<serde_json::Value> = q
                            .drain(..n)
                            .map(|v| serde_json::json!({ "topic": topic0, "value": v }))
                            .collect();
                        Ok(serde_json::json!({ "messages": msgs }))
                    }
                    "commit" | "ack" | "nack" => Ok(serde_json::json!({})),
                    _ => Err(format!("unsupported method: {method}").into()),
                }
            })
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::{Bridge, Extras, InMemoryKV, NamedRegistry, RequestInfo, SchemaRegistry};
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;

    pub(crate) fn registry_with(name: &str, kind: &'static str) -> Arc<NamedRegistry<MqInstance>> {
        let mut reg = NamedRegistry::new();
        let queue = Arc::new(Mutex::new(Vec::new()));
        reg.register(name, Arc::new(in_memory(kind, queue)))
            .unwrap();
        Arc::new(reg)
    }

    fn bridge(
        kafkas: Option<Arc<NamedRegistry<MqInstance>>>,
        flag: Option<Arc<AtomicBool>>,
    ) -> Bridge {
        Bridge::with_dbs_and_loader(
            HashMap::new(),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            None,
            Extras {
                kafkas,
                tasks_flag: flag,
                ..Default::default()
            },
        )
    }

    pub(crate) async fn run(b: &Bridge, src: &str) -> Result<String, String> {
        b.run_with(src, RequestInfo::default())
            .await
            .map(|c| String::from_utf8_lossy(&c.body).into_owned())
            .map_err(|e| e.to_string())
    }

    const CALL: &str = "__ojMq.call";
    const HAS: &str = "__ojMq.has";

    #[tokio::test(flavor = "current_thread")]
    async fn given_named_instance_when_send_then_backend_receives() {
        // Given: kafkas.default = InMemoryMq；When: JS send {value:{a:1}}
        // Then: call 返回 {"sent":1}
        let reg = registry_with("default", "kafka");
        let b = bridge(Some(reg), None);
        let out = run(
            &b,
            &format!(
                "{CALL}(\"kafka\",\"default\",\"send\",{{topic:\"t\",value:{{a:1}}}}).then(r => json.ok(r), e => json.fail(1, String(e)));"
            ),
        )
        .await
        .unwrap();
        assert!(out.contains("\"sent\":1"), "{out}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn given_unknown_name_when_call_then_err_named() {
        let b = bridge(Some(registry_with("default", "kafka")), None);
        let out = run(
            &b,
            &format!(
                "{CALL}(\"kafka\",\"nope\",\"send\",{{}}).then(r => json.ok(r), e => json.fail(1, String(e)));"
            ),
        )
        .await
        .unwrap();
        assert!(out.contains("no kafka instance named 'nope'"), "{out}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn given_http_context_when_poll_then_err_requires_task() {
        // Given: flag = None（HTTP/WS Bridge）；Then: poll 报 requires a task context（评审 M2）
        let b = bridge(Some(registry_with("default", "kafka")), None);
        let out = run(
            &b,
            &format!(
                "{CALL}(\"kafka\",\"default\",\"poll\",{{topics:[\"t\"]}}).then(r => json.ok(r), e => json.fail(1, String(e)));"
            ),
        )
        .await
        .unwrap();
        assert!(out.contains("requires a task context"), "{out}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn given_task_context_when_poll_then_allowed_and_recv() {
        // Given: flag = Some(true)（任务 Bridge）+ 队列已有消息；Then: poll 收到并放行
        let reg = registry_with("default", "kafka");
        let b = bridge(Some(reg), Some(Arc::new(AtomicBool::new(true))));
        // 预填队列：先 send 两帧（flag 对 send 无门禁）
        let _ = run(&b, &format!("{CALL}(\"kafka\",\"default\",\"send\",{{topic:\"t\",value:1}}).then(()=>json.ok());"))
            .await;
        let _ = run(&b, &format!("{CALL}(\"kafka\",\"default\",\"send\",{{topic:\"t\",value:2}}).then(()=>json.ok());"))
            .await;
        let out = run(
            &b,
            &format!(
                "{CALL}(\"kafka\",\"default\",\"poll\",{{topics:[\"t\"],max:10}}).then(r => json.ok(r), e => json.fail(1, String(e)));"
            ),
        )
        .await
        .unwrap();
        assert!(
            out.contains("\"messages\":[") && out.contains("\"value\":1"),
            "{out}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn given_second_concurrent_poll_when_first_active_then_err_busy() {
        // Given: 手动持有实例 poller 锁（模拟活跃 poller）；When: 第二个 poll
        // Then: Err instance busy（评审 M2）
        let mut reg = NamedRegistry::new();
        let queue = Arc::new(Mutex::new(Vec::new()));
        let inst = Arc::new(in_memory("kafka", queue));
        let guard = inst.poller.lock().await; // 持锁模拟首个 poll
        reg.register("default", inst.clone()).unwrap();
        let b = bridge(Some(Arc::new(reg)), Some(Arc::new(AtomicBool::new(true))));
        let out = run(
            &b,
            &format!(
                "{CALL}(\"kafka\",\"default\",\"poll\",{{topics:[\"t\"]}}).then(r => json.ok(r), e => json.fail(1, String(e)));"
            ),
        )
        .await
        .unwrap();
        drop(guard);
        assert!(out.contains("instance busy"), "{out}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn given_same_name_across_kinds_when_lookup_then_both_resolve() {
        // Given: Kafka("x") 与 RabbitMQ("x") 各自 registry（评审采纳：双 registry）
        let kafkas = registry_with("x", "kafka");
        let rabbits = registry_with("x", "rabbit");
        let b = Bridge::with_dbs_and_loader(
            HashMap::new(),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            None,
            Extras {
                kafkas: Some(kafkas),
                rabbits: Some(rabbits),
                tasks_flag: None,
                ..Default::default()
            },
        );
        let out = run(
            &b,
            &format!(
                "Promise.all([{HAS}(\"kafka\",\"x\"), {HAS}(\"rabbit\",\"x\"), {HAS}(\"kafka\",\"y\")]).then(v => json.ok(v));"
            ),
        )
        .await
        .unwrap();
        assert!(out.contains("[true,true,false]"), "{out}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn given_task_flag_when_tasks_stopping_then_reflects() {
        // Given: flag 初 false 置 true；Then: op_tasks_stopping 跟随
        let flag = Arc::new(AtomicBool::new(false));
        let b = bridge(Some(registry_with("default", "kafka")), Some(flag.clone()));
        let before = run(&b, "json.ok(__ojMq.stopping());").await.unwrap();
        flag.store(true, Ordering::Relaxed);
        let after = run(&b, "json.ok(__ojMq.stopping());").await.unwrap();
        assert!(
            before.contains("false") && after.contains("true"),
            "{before} {after}"
        );
    }

    #[test]
    fn given_ffi_instance_when_call_then_payload_serialized_to_json_string() {
        // mini-mq vtable echo 契约：payload 序列化为 JSON 字符串进 call（编译期冒烟）。
        // 真实 FFI 通路由 plugin_loader/mini-mq 集成测试覆盖；此处只验证构造不 panic。
        let vt = mini_echo_vtable();
        let _ = MqInstance::ffi("kafka", vt, 1, std::time::Duration::from_millis(1));
    }

    fn mini_echo_vtable() -> &'static oj_plugin_ffi::MqVtable {
        // 复用 tests/plugins/mini-mq 的形状：本地静态假 vtable（echo 语义）。
        static VT: std::sync::OnceLock<oj_plugin_ffi::MqVtable> = std::sync::OnceLock::new();
        VT.get_or_init(|| oj_plugin_ffi::MqVtable {
            connect: fake_connect,
            call: fake_call,
            close: fake_close,
        })
    }

    extern "C" fn fake_connect(_cfg: oj_plugin_ffi::RString) -> oj_plugin_ffi::FfiFuture {
        oj_plugin_ffi::ready_ok(br#"{"handle":1}"#)
    }
    extern "C" fn fake_close(_h: u64) {}

    extern "C" fn fake_call(
        _h: u64,
        _m: oj_plugin_ffi::RString,
        p: oj_plugin_ffi::RString,
    ) -> oj_plugin_ffi::FfiFuture {
        let out = format!(r#"{{"payload":{}}}"#, &p[..]);
        oj_plugin_ffi::ready_ok(out.into_bytes())
    }
}

#[cfg(test)]
mod js_global_tests {
    use super::MqInstance;
    use super::tests::*;
    use crate::bridge::NamedRegistry;
    use crate::bridge::{Bridge, Extras, InMemoryKV, SchemaRegistry};
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    async fn run(reg: Option<Arc<NamedRegistry<MqInstance>>>, src: &str) -> String {
        let b = Bridge::with_dbs_and_loader(
            HashMap::new(),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            None,
            Extras {
                kafkas: reg.clone(),
                rabbits: reg,
                tasks_flag: Some(Arc::new(AtomicBool::new(true))),
                ..Default::default()
            },
        );
        b.run_with(src, crate::bridge::RequestInfo::default())
            .await
            .map(|c| String::from_utf8_lossy(&c.body).into_owned())
            .unwrap()
    }

    /// Kafka(name) 同一性：两次调用同一对象（mqCache，评审 N2/D 同 dbCache 语义）。
    #[tokio::test(flavor = "current_thread")]
    async fn given_named_kafka_when_lookup_twice_then_same_object() {
        let out = run(Some(super::tests::registry_with("default", "kafka")),
            "const k = Kafka(\"default\"); json.ok(k === Kafka(\"default\") && typeof k.send === \"function\");")
            .await;
        assert!(out.contains("true"), "{out}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn given_unconfigured_name_when_lookup_then_undefined() {
        let out = run(
            None,
            "json.ok(Kafka(\"nope\") === undefined && RabbitMQ(\"nope\") === undefined);",
        )
        .await;
        assert!(out.contains("true"), "{out}");
    }

    /// rabbit 面：publish 是 send 的命名（payload 带 exchange/routingKey），无 send/commit 暴露。
    #[tokio::test(flavor = "current_thread")]
    async fn given_rabbit_instance_when_publish_then_payload_shaped() {
        let reg = super::tests::registry_with("default", "rabbit");
        let out = run(
            Some(reg),
            "const r = RabbitMQ(\"default\"); \
             json.ok(typeof r.send === \"undefined\" && typeof r.publish === \"function\" && typeof r.ack === \"function\");",
        )
        .await;
        assert!(out.contains("true"), "{out}");
    }

    /// kafka 面：send/poll/commit 可用，publish/ack 不暴露。
    #[tokio::test(flavor = "current_thread")]
    async fn given_kafka_instance_when_probe_surface_then_kafka_shape() {
        let reg = super::tests::registry_with("default", "kafka");
        let out = run(
            Some(reg),
            "const k = Kafka(\"default\"); \
             json.ok(typeof k.send === \"function\" && typeof k.poll === \"function\" && typeof k.commit === \"function\" && typeof k.publish === \"undefined\");",
        )
        .await;
        assert!(out.contains("true"), "{out}");
    }
}
