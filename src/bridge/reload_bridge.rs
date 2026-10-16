//! `server.reloadConfig()` 下沉类型与 op（ojm 批次 PR-3，设计稿 §3）。
//!
//! 类型下沉（架构评审阻塞项 1 修复）：[`ReloadOutcome`] 定义在根 crate，
//! serve 的 `ReloadOutcome` 改为 re-export——根 crate 不可反向依赖 serve
//! （`cargo tree` 单向：serve → only_js），HTTP `POST {base}/_system/config`
//! 的序列化与 JS 返回共用同一结构，真源唯一。
//!
//! 注入走槽位后填（架构意见 2 / 工程意见 1 修复）：[`StableState::reload`]
//! 是共享可填充槽 `Arc<OnceLock<ReloadExecutorLike>>`——槽指针构造期冻结
//! （Extras→StableState 注入同一 Arc，桥池多实例共享同一槽）、内容后填
//! （`oj serve` 装配 ReloadState 后 `slot.set`），照搬 `ReloadState::set_executor`
//! 的既有解环惯例；不触 `Arc::get_mut` 红线。`oj test` / 嵌入式不填槽 →
//! op 报 `reload not available`。

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use deno_core::OpState;
use deno_core::op2;
use deno_error::JsErrorBox;
use serde::Serialize;

use super::StableState;

/// 热更执行结果（单一真源）：HTTP POST 成功返回体与 JS
/// `server.reloadConfig()` 的 resolve 值共用。形状恒定——`unchanged=true`
/// 时三清单为空数组（JS 无需分支，工程师意见 11 修复）。
#[derive(Clone, Debug, Serialize)]
pub struct ReloadOutcome {
    /// 代际 id（每次成功 swap +1；unchanged 时不变）。
    pub generation: u64,
    /// 归一后 cfg 与当前代相同（幂等）。
    pub unchanged: bool,
    /// 本代已生效段。
    pub applied: Vec<String>,
    /// 已验证合法但本代沿用旧值、重启后生效。
    pub restart_pending: Vec<String>,
    /// listener 级沿旧（port/host/certificate_path/public_key_path）。
    pub kept_listener: Vec<String>,
}

/// 热更执行闭包类型：`oj serve` 装配期（ReloadState 构造 + set_executor 之后）
/// 填进共享槽。同步执行——内部独立线程 + 多线 runtime（`from_config` 含 DB
/// verify / 模块转译，current_thread 直调会死锁）；op 侧经 `spawn_blocking` 调用。
/// `Err((status, msg))` 的 status 沿用 HTTP 语义：409 = 撞 `reload_lock`
/// （busy，不静默排队）、400/500 = 归一/装配失败。
pub type ReloadExecutorLike = Arc<dyn Fn() -> Result<ReloadOutcome, (u16, String)> + Send + Sync>;

/// 共享可填充槽（[`StableState::reload`] 的元素类型）：槽指针构造期冻结、
/// 内容后填（`slot.set` 一次，照搬 `ReloadState::set_executor` 解环惯例）。
/// 桥池多实例共享同一 `Arc<OnceLock>`，每代热更重建桥后依旧有效。
pub type ReloadSlot = std::sync::OnceLock<ReloadExecutorLike>;

// ----- op_server_reload_config（Task 5.1） -----

/// `server.reloadConfig()`：进程内触发热更。
///
/// ① 平台特权裁决（阶段 2 地基 [`super::platform::assert_platform`]；
///    tasks/WS 上下文无归属 → 恒 NotCapable）；
/// ② 槽空（非 `oj serve` 形态：`oj test` / 嵌入式）→ 点名报错；
/// ③ `spawn_blocking` 调同步 executor——**既定语义**：触发 reload 的 handler
///    所在 actor 在热更期间被占用（秒级，DB verify + 转译），完成前不归还池；
/// ④ `Err((code, msg))` 按 code 映射 JsErrorBox：409（撞 `reload_lock`）文案
///    含 busy 语义，与 HTTP POST 共享同一把锁与同一 executor 实例。
#[op2]
#[serde]
pub async fn op_server_reload_config(
    state: Rc<RefCell<OpState>>,
) -> Result<ReloadOutcome, JsErrorBox> {
    // ① 特权裁决（借 OpState 短临界区，不跨 await 持有借用）。
    {
        let st = state.borrow();
        super::platform::assert_platform(&st)?;
    }
    // ② 槽位取 executor（槽空 = not running under oj serve）。
    let exec = {
        let st = state.borrow();
        st.borrow::<Arc<StableState>>().reload.get().cloned()
    }
    .ok_or_else(|| JsErrorBox::generic("reload not available (not running under oj serve)"))?;
    // ③ 同步 executor → 阻塞池执行（见类型注释）。
    let outcome = tokio::task::spawn_blocking(move || exec())
        .await
        .map_err(|e| JsErrorBox::generic(format!("server.reloadConfig: join failed: {e}")))?
        .map_err(|(code, msg)| {
            if code == 409 {
                // 撞 reload_lock：busy 语义点名，不静默并发排队。
                JsErrorBox::generic(format!("server.reloadConfig: {msg} (busy)"))
            } else {
                JsErrorBox::generic(format!("server.reloadConfig: HTTP {code}: {msg}"))
            }
        })?;
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// serde 形状（Task 5.1.1 单测）：恒定五键；unchanged=true 时三清单为空数组。
    #[test]
    fn serde_shape_constant_with_empty_lists_when_unchanged() {
        let o = ReloadOutcome {
            generation: 3,
            unchanged: true,
            applied: vec![],
            restart_pending: vec![],
            kept_listener: vec![],
        };
        let v = serde_json::to_value(&o).unwrap();
        let obj = v.as_object().expect("object");
        assert_eq!(
            obj.keys().collect::<Vec<_>>(),
            vec![
                "generation",
                "unchanged",
                "applied",
                "restart_pending",
                "kept_listener"
            ],
            "field order is the wire shape"
        );
        assert_eq!(v["generation"], serde_json::json!(3));
        assert_eq!(v["unchanged"], serde_json::json!(true));
        assert_eq!(v["applied"], serde_json::json!([]));
        assert_eq!(v["restart_pending"], serde_json::json!([]));
        assert_eq!(v["kept_listener"], serde_json::json!([]));

        let o2 = ReloadOutcome {
            generation: 4,
            unchanged: false,
            applied: vec!["vars".into()],
            restart_pending: vec!["platform".into()],
            kept_listener: vec!["server".into()],
        };
        let v2 = serde_json::to_value(&o2).unwrap();
        assert_eq!(v2["unchanged"], serde_json::json!(false));
        assert_eq!(v2["applied"], serde_json::json!(["vars"]));
        assert_eq!(v2["restart_pending"], serde_json::json!(["platform"]));
        assert_eq!(v2["kept_listener"], serde_json::json!(["server"]));
    }

    // ----- BDD S1–S7（bridge 级：经 run_module 归属模块，对齐 exec.rs 测试形态） -----

    use crate::bridge::module_loader::LoaderShared;
    use crate::bridge::platform::PlatformGrant;
    use crate::bridge::{Bridge, Extras, InMemoryKV, ModuleCtx, SchemaRegistry};
    use std::collections::{HashMap, HashSet};

    fn tmp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("oj-reloadtest-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 全场景 handler 表。
    fn api_src() -> String {
        r#"
export default {
  reload: async () => json.ok(await server.reloadConfig()),
  reload_catch: async () => {
    try {
      return json.ok(await server.reloadConfig());
    } catch (e) {
      return json.ok({ err: String(e) });
    }
  },
  reload_pair: async () => {
    const p1 = server.reloadConfig();
    const p2 = server.reloadConfig();
    // 先落定的一方 = 撞锁 busy 的一方（另一方持锁阻塞在 executor 中）。
    const which = await Promise.race([
      p1.then(() => "p1", () => "p1"),
      p2.then(() => "p2", () => "p2"),
    ]);
    await kv.set("phase", "one-settled:" + which);
    const fmt = (r) =>
      r.status === "fulfilled" ? r : { status: "rejected", reason: String(r.reason) };
    const [r1, r2] = await Promise.allSettled([p1, p2]);
    return json.ok([fmt(r1), fmt(r2)]);
  },
};
"#
        .to_string()
    }

    /// 双模块夹具：inst（platform-privileged）/ plain（无特权）。kv 句柄外置
    /// （S6 用 kv 作 JS↔宿主握手通道）。
    fn make_bridge(
        base: &std::path::Path,
        slot: Arc<ReloadSlot>,
        kv: Arc<dyn crate::bridge::KVStore>,
    ) -> Bridge {
        let mut map = HashMap::new();
        for (d, n) in [("inst", "inst"), ("plain", "plain")] {
            let dir = base.join(d);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("api.js"), api_src()).unwrap();
            map.insert(
                dir.to_string_lossy().into_owned(),
                ModuleCtx {
                    name: n.to_string(),
                    version: "0.1.0".into(),
                    deps: Arc::new(HashSet::new()),
                    db: None,
                },
            );
        }
        Bridge::with_dbs_and_loader(
            HashMap::new(),
            kv,
            SchemaRegistry::new(),
            false,
            Some(Arc::new(LoaderShared {
                project_root: base.to_path_buf(),
                ts: false,
            })),
            Extras {
                platform: Some(Arc::new(
                    PlatformGrant::new(vec!["inst".into()], &[], None).unwrap(),
                )),
                modules: Arc::new(map),
                reload: slot,
                ..Default::default()
            },
        )
    }

    fn run<'a>(
        b: &'a Bridge,
        base: &'a std::path::Path,
        module: &str,
        handler: &'a str,
    ) -> impl std::future::Future<Output = serde_json::Value> + 'a {
        let api = base.join(module).join("api.js");
        let t = std::time::Duration::from_secs(30);
        async move {
            let cap = b
                .run_module(&api, handler, Default::default(), t)
                .await
                .unwrap_or_else(|e| panic!("{handler} failed: {e}"));
            serde_json::from_slice(&cap.body).unwrap_or_else(|e| panic!("{handler}: {e}"))
        }
    }

    /// S1/S2（op 接线，假 executor）：特权模块 reloadConfig → 恒定五键形状透传；
    /// unchanged=true 时三清单为空数组。
    #[tokio::test(flavor = "current_thread")]
    async fn s1_s2_privileged_reload_outcome_shapes() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let base = tmp("s1").canonicalize().unwrap();
        let slot: Arc<ReloadSlot> = Arc::new(std::sync::OnceLock::new());
        let b = make_bridge(&base, slot.clone(), Arc::new(InMemoryKV::new()));
        // 槽位后填（照搬 serve 装配次序：桥构造在先，executor 在后 set）。
        let calls = Arc::new(AtomicU32::new(0));
        let calls2 = calls.clone();
        slot.set(Arc::new(move || {
            let n = calls2.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(if n == 1 {
                ReloadOutcome {
                    generation: 1,
                    unchanged: false,
                    applied: vec!["vars".into()],
                    restart_pending: vec![],
                    kept_listener: vec![],
                }
            } else {
                ReloadOutcome {
                    generation: 1,
                    unchanged: true,
                    applied: vec![],
                    restart_pending: vec![],
                    kept_listener: vec![],
                }
            })
        }))
        .is_ok()
        .then_some(())
        .expect("slot set must succeed");
        // S1：applied 含段名、generation 推进（executor 语义；代际真值由 e2e 钉死）。
        let v = run(&b, &base, "inst", "reload").await;
        assert_eq!(v["code"], 0, "{}", v["msg"]);
        let d = &v["data"];
        assert_eq!(d["generation"], serde_json::json!(1));
        assert_eq!(d["unchanged"], serde_json::json!(false));
        assert_eq!(d["applied"], serde_json::json!(["vars"]));
        assert_eq!(d["restart_pending"], serde_json::json!([]));
        assert_eq!(d["kept_listener"], serde_json::json!([]));
        // S2：unchanged=true、generation 不动、三清单空数组（形状恒定）。
        let v = run(&b, &base, "inst", "reload").await;
        let d = &v["data"];
        assert_eq!(d["generation"], serde_json::json!(1));
        assert_eq!(d["unchanged"], serde_json::json!(true));
        assert_eq!(d["applied"], serde_json::json!([]));
        assert_eq!(d["restart_pending"], serde_json::json!([]));
        assert_eq!(d["kept_listener"], serde_json::json!([]));
    }

    /// S5：非特权模块 → NotCapable: not platform-privileged。
    #[tokio::test(flavor = "current_thread")]
    async fn s5_non_privileged_module_denied() {
        let base = tmp("s5").canonicalize().unwrap();
        let slot: Arc<ReloadSlot> = Arc::new(std::sync::OnceLock::new());
        slot.set(Arc::new(|| Err((500u16, "must not be called".into()))))
            .is_ok()
            .then_some(())
            .expect("slot set must succeed");
        let b = make_bridge(&base, slot, Arc::new(InMemoryKV::new()));
        let v = run(&b, &base, "plain", "reload_catch").await;
        assert_eq!(v["code"], 0, "{}", v["msg"]);
        let err = v["data"]["err"].as_str().expect("err string");
        assert!(err.contains("NotCapable"), "{err}");
        assert!(err.contains("not platform-privileged"), "{err}");
    }

    /// S7：槽空（oj test / 嵌入式形态）→ reload not available。
    #[tokio::test(flavor = "current_thread")]
    async fn s7_empty_slot_not_available() {
        let base = tmp("s7").canonicalize().unwrap();
        let slot: Arc<ReloadSlot> = Arc::new(std::sync::OnceLock::new());
        let b = make_bridge(&base, slot, Arc::new(InMemoryKV::new()));
        let v = run(&b, &base, "inst", "reload_catch").await;
        assert_eq!(v["code"], 0, "{}", v["msg"]);
        let err = v["data"]["err"].as_str().expect("err string");
        assert!(err.contains("reload not available"), "{err}");
        assert!(err.contains("not running under oj serve"), "{err}");
    }

    /// S6：并发双触发 → 一方 busy 报错（文案点名，非静默排队）。
    /// 确定性编排（消除两路抢锁竞态）：p1 持锁阻塞在 executor → host 探到
    /// started 后经 kv 下发 "go" → JS 才发 p2（此刻锁必被 p1 持有 → busy）→
    /// JS 观测 p2 已 settle 后 kv 上报 "p2-settled" → host 放行 executor。
    #[tokio::test(flavor = "current_thread")]
    async fn s6_concurrent_triggers_one_busy() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let base = tmp("s6").canonicalize().unwrap();
        let slot: Arc<ReloadSlot> = Arc::new(std::sync::OnceLock::new());
        let kv: Arc<dyn crate::bridge::KVStore> = Arc::new(InMemoryKV::new());
        let kv_probe = kv.clone();
        let b = make_bridge(&base, slot.clone(), kv);
        let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        // std mpsc Receiver 非 Sync，executor 闭包要求 Send+Sync → Mutex 包裹。
        let release_rx = std::sync::Mutex::new(release_rx);
        let calls = Arc::new(AtomicU32::new(0));
        let calls2 = calls.clone();
        // 假 executor 复刻生产组成（serve_cmd 槽闭包 = trigger_sync）：
        // try_lock 串行化在前（撞锁 → 409 busy），executor 体在后。
        let reload_lock = Arc::new(tokio::sync::Mutex::new(()));
        let lock2 = reload_lock.clone();
        slot.set(Arc::new(move || {
            let _guard = lock2
                .try_lock()
                .map_err(|_| (409u16, "reload in progress".to_string()))?;
            let n = calls2.fetch_add(1, Ordering::SeqCst) + 1;
            if n == 1 {
                started_tx.send(()).unwrap();
                // 持锁阻塞至测试放行（阻塞池线程，不占 current_thread 运行时）。
                release_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .unwrap();
            }
            Ok(ReloadOutcome {
                generation: 9,
                unchanged: false,
                applied: vec!["vars".into()],
                restart_pending: vec![],
                kept_listener: vec![],
            })
        }))
        .is_ok()
        .then_some(())
        .expect("slot set must succeed");
        let task = run(&b, &base, "inst", "reload_pair");
        // 编排驱动（current_thread 上与 JS 交错；有界轮询，禁固定 sleep 当同步）：
        // ① 等 executor 首调持锁（started 信号）；
        // ② 等 JS 上报「一方已落定」（kv 一次性写入——busy 已落定的证据）再放行。
        //    JS 侧无轮询（紧循环会饿死事件循环），只做单次 race + 单次 kv.set。
        let driver = async move {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            loop {
                if started_rx.try_recv().is_ok() {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "executor never started"
                );
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            loop {
                if kv_probe
                    .get("phase")
                    .await
                    .unwrap()
                    .as_deref()
                    .is_some_and(|v| v.starts_with("one-settled:"))
                {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "neither trigger settled (no busy observed)"
                );
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            release_tx.send(()).unwrap();
        };
        let (v, ()) = tokio::join!(task, driver);
        assert_eq!(v["code"], 0, "{}", v["msg"]);
        let pair = v["data"].as_array().expect("allSettled pair");
        assert_eq!(pair.len(), 2);
        let ok = pair
            .iter()
            .find(|r| r["status"] == serde_json::json!("fulfilled"))
            .expect("one fulfilled");
        assert_eq!(ok["value"]["generation"], serde_json::json!(9));
        let busy = pair
            .iter()
            .find(|r| r["status"] == serde_json::json!("rejected"))
            .expect("one rejected");
        let reason = busy["reason"].to_string();
        assert!(reason.contains("busy"), "{reason}");
        assert!(reason.contains("reload in progress"), "{reason}");
        // executor 只被命中一次（败者撞锁后未触达 executor）。
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
