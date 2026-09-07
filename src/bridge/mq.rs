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

    /// 装配期入口：connect(cfg) → 解析 handle → 构造 ffi 实例（oj 装配层消费；
    /// 失败 = cfg 校验失败或插件拒绝 kind → 装配 fail-fast，spec §7）。
    pub async fn ffi_connect(
        kind: &'static str,
        vt: &'static oj_plugin_ffi::MqVtable,
        cfg_json: String,
        backoff: std::time::Duration,
    ) -> BridgeResult<Self> {
        let out = super::ffi::await_ffi_poll(
            (vt.connect)(oj_plugin_ffi::RString::from(cfg_json.as_str())),
            backoff,
        )
        .await?;
        let handle = serde_json::from_slice::<serde_json::Value>(&out)?["handle"]
            .as_u64()
            .ok_or_else(|| -> Box<dyn std::error::Error + Send + Sync> {
                "mq connect: missing handle in result".into()
            })?;
        Ok(Self::ffi(kind, vt, handle, backoff))
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

/// 任务等待原语：tokio sleep（本 runtime 无 timer 全局——setTimeout 不可用；
/// broker 无关的任务循环用 `await tasks.sleep(ms)` 合法睡眠）。
#[op2]
pub async fn op_tasks_sleep(#[number] ms: i64) {
    if ms > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(ms as u64)).await;
    }
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
                        // 空且指定 timeoutMs → 真实等待（夹具语义对齐真实 Kafka poll；
                        // 立即返回空会把任务 TLA 循环变成紧 promise 链，饿死 V8 的
                        // microtask checkpoint——mod_evaluate 永不返回）。
                        let mut waited = 0u64;
                        loop {
                            let msgs: Vec<serde_json::Value> = {
                                let mut q = queue.lock().unwrap();
                                let n =
                                    q.len().min(payload["max"].as_u64().unwrap_or(100) as usize);
                                let topic0 = payload["topics"][0].clone();
                                q.drain(..n)
                                    .map(|v| serde_json::json!({ "topic": topic0, "value": v }))
                                    .collect()
                            };
                            if !msgs.is_empty() {
                                break Ok(serde_json::json!({ "messages": msgs }));
                            }
                            let budget = payload["timeoutMs"].as_u64().unwrap_or(0).min(500);
                            if waited >= budget {
                                break Ok(serde_json::json!({ "messages": [] }));
                            }
                            let step = budget.saturating_sub(waited).min(10);
                            waited += step;
                            tokio::time::sleep(std::time::Duration::from_millis(step)).await;
                        }
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

    /// tasks.sleep：任务等待原语（本 runtime 无 timer 全局；broker 无关任务循环的
    /// 合法睡眠）。await 后正常 resolve（run_with 经典 script：顶层只能 async IIFE）。
    #[tokio::test(flavor = "current_thread")]
    async fn given_tasks_sleep_when_awaited_then_resolves() {
        let t0 = std::time::Instant::now();
        let out = run(
            None,
            "(async () => { await tasks.sleep(30); json.ok(typeof tasks.stopping === \"function\"); })()",
        )
        .await;
        assert!(out.contains("true"), "{out}");
        assert!(
            t0.elapsed() >= std::time::Duration::from_millis(25),
            "sleep too short"
        );
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

#[cfg(test)]
mod task_driver_tests {
    use crate::bridge::{Bridge, Extras, InMemoryKV, NamedRegistry, SchemaRegistry};
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    fn task_bridge(root: &std::path::Path) -> Bridge {
        let mut reg = NamedRegistry::new();
        let queue = Arc::new(std::sync::Mutex::new(Vec::new()));
        reg.register("default", Arc::new(super::in_memory("kafka", queue)))
            .unwrap();
        Bridge::with_dbs_and_loader(
            HashMap::new(),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            Some(Arc::new(crate::bridge::LoaderShared {
                project_root: root.to_path_buf(),
                ts: true,
            })),
            Extras {
                kafkas: Some(Arc::new(reg)),
                tasks_flag: Some(Arc::new(AtomicBool::new(true))),
                ..Default::default()
            },
        )
    }

    fn write_task(dir: &std::path::Path, name: &str, src: &str) -> std::path::PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, src).unwrap();
        p
    }

    /// TLA while 循环任务：flag 置位后自然退出 → Stopped（评审 F3 执行模型）。
    #[tokio::test(flavor = "current_thread")]
    async fn given_tla_while_loop_task_when_flag_set_then_exits_stopped() {
        let dir = std::env::temp_dir().join(format!("ojtask-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = write_task(
            &dir,
            "task_ok.js",
            "export {};\nwhile (!tasks.stopping()) { await Kafka('default').poll(['t'], { timeoutMs: 30 }); }\n",
        );
        let b = task_bridge(&dir);
        let flag = Arc::new(AtomicBool::new(false));
        let setter_flag = flag.clone();
        // Bridge 是 !Send 不能 spawn——同 current_thread task 内 select 驱动：
        // setter 100ms 后置位，随后永久 pending（绝不赢过 run_task 的自然收场）。
        let setter = async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            setter_flag.store(true, std::sync::atomic::Ordering::Relaxed);
            std::future::pending::<()>().await;
        };
        let out = tokio::time::timeout(std::time::Duration::from_secs(2), async move {
            tokio::select! {
                r = b.run_task(&path, flag, std::time::Duration::from_millis(200)) => r,
                _ = setter => unreachable!("setter must not win"),
            }
        })
        .await
        .unwrap();
        assert!(matches!(out, crate::bridge::TaskExit::Stopped), "{out:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 顶层 throw → Crashed（监督重启信号，评审 F3）。
    #[tokio::test(flavor = "current_thread")]
    async fn given_task_throws_when_run_then_crashed_with_message() {
        let dir = std::env::temp_dir().join(format!("ojtaskc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = write_task(
            &dir,
            "task_boom.ts",
            "export {};\nthrow new Error(\"boom\");\n",
        );
        let b = task_bridge(&dir);
        let out = b
            .run_task(
                &path,
                Arc::new(AtomicBool::new(false)),
                std::time::Duration::from_millis(200),
            )
            .await;
        assert!(
            matches!(&out, crate::bridge::TaskExit::Crashed(m) if m.contains("boom")),
            "{out:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// CJS 风格任务（无 ESM 标记 + 顶层 await）→ 可诊断的 Crashed（而非句法炸裂）。
    #[tokio::test(flavor = "current_thread")]
    async fn given_cjs_style_task_when_run_then_crashed_with_guidance() {
        let dir = std::env::temp_dir().join(format!("ojtaskj-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = write_task(
            &dir,
            "task_cjs.ts",
            "while (!tasks.stopping()) { await Kafka(\"default\").poll([\"t\"], { timeoutMs: 30 }); }\n",
        );
        let b = task_bridge(&dir);
        let out = b
            .run_task(
                &path,
                Arc::new(AtomicBool::new(false)),
                std::time::Duration::from_millis(200),
            )
            .await;
        assert!(
            matches!(&out, crate::bridge::TaskExit::Crashed(m) if m.contains("ESM")),
            "{out:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// grace 到期仍不退出 → Killed（terminate + event loop 兜底，评审 F5/SIGSEGV 纪律）。
    #[tokio::test(flavor = "current_thread")]
    async fn given_task_ignoring_flag_when_grace_expires_then_killed() {
        let dir = std::env::temp_dir().join(format!("ojtaskk-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = write_task(
            &dir,
            "task_stuck.ts",
            "export {};\nwhile (true) { await Kafka(\"default\").poll([\"t\"], { timeoutMs: 50 }); }\n",
        );
        let b = task_bridge(&dir);
        let out = b
            .run_task(
                &path,
                Arc::new(AtomicBool::new(true)),
                std::time::Duration::from_millis(120),
            )
            .await;
        assert!(matches!(out, crate::bridge::TaskExit::Killed), "{out:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
