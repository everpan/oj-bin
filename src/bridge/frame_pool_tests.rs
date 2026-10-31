use super::super::{Bridge, Extras, InMemoryKV, LoaderShared, SchemaRegistry, WsSend};
use std::collections::HashMap;
use std::time::Duration;

use super::*;

/// RoutePool 测试桥工厂（带模块加载器，ws 文件放临时目录）。
pub(crate) fn ws_test_bridge(root: &std::path::Path) -> Arc<dyn Fn() -> Bridge + Send + Sync> {
    let root = root.to_path_buf();
    Arc::new(move || {
        Bridge::with_dbs_and_loader(
            HashMap::new(),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            Some(Arc::new(LoaderShared {
                project_root: root.clone(),
                ts: true,
            })),
            Extras::default(),
        )
    })
}

fn pool_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("oj-wspool-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn frame(
    conn: u64,
) -> (
    Frame,
    tokio::sync::oneshot::Receiver<Result<WsOutcome, FrameError>>,
) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    (
        Frame {
            conn,
            ev: "message",
            body: vec![],
            binary: false,
            done: tx,
        },
        rx,
    )
}

#[tokio::test(flavor = "current_thread")]
async fn scheduler_serializes_per_conn_and_releases_after_complete() {
    let s = Scheduler::new();
    let (f1, _r1) = frame(7);
    let (f2, _r2) = frame(7);
    s.submit(f1);
    s.submit(f2); // conn7 在飞 → f2 进 waiting
    let got = s.pull().await.unwrap();
    assert_eq!(got.conn, 7);
    s.complete(7); // 释放 waiting 里的 f2
    let got2 = s.pull().await.unwrap();
    assert_eq!(got2.conn, 7);
    s.complete(7);
}

#[tokio::test(flavor = "current_thread")]
async fn scheduler_panic_conn_errors_queued_and_releases_in_flight() {
    // Worker 帧中 panic 恢复（panic_conn）钉：在飞帧随 unwind 消亡（done 被
    // drop），同连接排队帧必须回错、在飞标记必须清掉——否则该连接后续帧永久挂起
    // （旧代码 panic 恢复只补扣 live + 补员，不清 Scheduler 状态）。
    let s = Scheduler::new();
    let (f1, r1) = frame(7);
    s.submit(f1);
    let (f2, r2) = frame(7);
    s.submit(f2); // conn7 在飞 → f2 进 waiting
    let in_flight = s.pull().await.unwrap();
    assert_eq!(in_flight.conn, 7);
    drop(in_flight); // unwind 丢帧：done sender 随之 drop
    assert!(r1.await.is_err(), "在飞帧的 fire 必须立刻得错而非挂起");
    s.panic_conn(7); // panic 恢复路径的连接清场
    assert!(matches!(r2.await, Ok(Err(FrameError::Dropped))));
    // 在飞标记已清：同连接下一帧照常被调度（旧代码此处永久挂起）。
    let (f3, _r3) = frame(7);
    s.submit(f3);
    let got = tokio::time::timeout(Duration::from_secs(2), s.pull())
        .await
        .expect("panic 清场后同连接下一帧必须可调度")
        .unwrap();
    assert_eq!(got.conn, 7);
    s.complete(7);
}

#[tokio::test(flavor = "current_thread")]
async fn scheduler_drop_conn_discards_queued_and_close_resolves_rest() {
    let s = Scheduler::new();
    let (fa, _ra) = frame(1);
    s.submit(fa); // 在飞
    let (fb, rb) = frame(1);
    s.submit(fb); // waiting
    s.drop_conn(1);
    assert!(matches!(rb.await, Ok(Err(FrameError::Dropped))));
    let (fc, rc) = frame(2);
    s.submit(fc);
    s.close();
    assert!(matches!(rc.await, Ok(Err(FrameError::PoolClosed))));
    assert!(s.pull().await.is_none()); // closed → pull None（worker 退出）
    // 复活（reopen）路径由池级用例覆盖：ws_pool_reconnect_after_idle_retire /
    // pool_retires_when_idle_after_linger（attach 锁内见 closed 即复活 + 补员）。
}

/// 生命周期 + sess 外置：connection 恰好一次、message 每帧、close 收尾；
/// sess.state 跨帧持久（Rust 会话表）、按连接隔离（同池两连接互不可见）。
#[tokio::test(flavor = "current_thread")]
async fn pool_routes_frames_and_externalizes_sess_state() {
    let dir = pool_dir("pool1");
    let ws_file = dir.join("ws.js");
    std::fs::write(
        &ws_file,
        r#"
export default {
  connection() { sess.state.n = 0; json.ok({ hello: 1 }); },
  message() { sess.state.n = (sess.state.n ?? 0) + 1; json.ok({ n: sess.state.n, gid: sess.id }); },
  close() { json.ok({ bye: sess.state.n }); },
};
"#,
    )
    .unwrap();
    let pool = RoutePool::new(
        ws_file.clone(),
        ws_test_bridge(&dir),
        Duration::from_secs(1),
        1,
        0,
    );
    let (btx, _brx) = tokio::sync::mpsc::unbounded_channel();
    let h = pool.attach(btx);
    let o = h.fire("connection", vec![], false).await.unwrap();
    assert!(String::from_utf8_lossy(&o.capture.body).contains("\"hello\":1"));
    for expect in [1, 2] {
        let o = h.fire("message", vec![], false).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&o.capture.body).unwrap();
        assert_eq!(v["data"]["n"], expect, "sess.state 跨帧持久（Rust 会话表）");
        assert_eq!(v["data"]["gid"], 1, "sess.id = 连接 id");
    }
    let o = h.fire("close", vec![], false).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&o.capture.body).unwrap();
    assert_eq!(v["data"]["bye"], 2, "close 收尾可见累计状态");
    // 第二条连接：状态互不可见（同池隔离）
    let (btx2, _brx2) = tokio::sync::mpsc::unbounded_channel();
    let h2 = pool.attach(btx2);
    h2.fire("connection", vec![], false).await.unwrap();
    let o = h2.fire("message", vec![], false).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&o.capture.body).unwrap();
    assert_eq!(v["data"]["n"], 1, "会话表按 conn 隔离");
    h.detach();
    h2.detach();
}

/// 毒化半径 = 1 帧：帧超时 terminate 只杀执行它的 Worker，池补员/第二 Worker
/// 接帧，其它连接无感。
#[tokio::test(flavor = "current_thread")]
async fn pool_poison_radius_is_one_frame_and_pool_respawns() {
    let dir = pool_dir("pool2");
    let ws_file = dir.join("ws.js");
    std::fs::write(
        &ws_file,
        r#"
export default {
  message() {
    if (http.body.boom) { const t = Date.now(); while (Date.now() - t < 60_000) {} }
    json.ok({ ok: 1 });
  },
};
"#,
    )
    .unwrap();
    // workers_max=2：毒化补员后仍有 worker 可用
    let pool = RoutePool::new(
        ws_file,
        ws_test_bridge(&dir),
        Duration::from_millis(300),
        2,
        0,
    );
    let (b1, _) = tokio::sync::mpsc::unbounded_channel();
    let (b2, _) = tokio::sync::mpsc::unbounded_channel();
    let bad = pool.attach(b1);
    let good = pool.attach(b2);
    let e = bad
        .fire("message", br#"{"boom":true}"#.to_vec(), false)
        .await
        .unwrap_err();
    assert!(matches!(e, FrameError::Timeout));
    // 毒化半径=1：另一连接照常（补员或第二 worker 接帧）
    let o = good
        .fire("message", br#"{"boom":false}"#.to_vec(), false)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&o.capture.body).contains("\"ok\":1"));
    bad.detach();
    good.detach();
}

/// 一刀切契约（pool 版）：无任何钩子导出 → 预载失败 → fire 得 PoolClosed。
#[tokio::test(flavor = "current_thread")]
async fn pool_rejects_missing_hooks_with_pool_closed() {
    let dir = pool_dir("pool3");
    let ws_file = dir.join("ws.js");
    std::fs::write(&ws_file, "json.ok({});\n").unwrap();
    let pool = RoutePool::new(ws_file, ws_test_bridge(&dir), Duration::from_secs(1), 1, 0);
    let (btx, _brx) = tokio::sync::mpsc::unbounded_channel();
    let h = pool.attach(btx);
    let e = h.fire("message", vec![], false).await.unwrap_err();
    assert!(matches!(e, FrameError::PoolClosed), "{e:?}");
    h.detach();
}

/// message 抛异常 → error(e) 接住 → fire 返回 Ok，连接不断；error 可用
/// ws.send 带出信息。
#[tokio::test(flavor = "current_thread")]
async fn pool_error_hook_catches_frame_exception() {
    let dir = pool_dir("pool4");
    let ws_file = dir.join("ws.js");
    std::fs::write(
        &ws_file,
        r#"
export default {
  message() { throw new Error("boom"); },
  error(e) { ws.send("err:" + e.message); },
};
"#,
    )
    .unwrap();
    let pool = RoutePool::new(ws_file, ws_test_bridge(&dir), Duration::from_secs(1), 1, 0);
    let (btx, _brx) = tokio::sync::mpsc::unbounded_channel();
    let h = pool.attach(btx);
    let o = h.fire("message", vec![], false).await.unwrap();
    assert_eq!(o.sends, vec![WsSend::Text("err:boom".to_string())]);
    assert!(
        o.capture.body.is_empty(),
        "返回值不自动包信封，异常也不产生信封"
    );
    h.detach();
}

/// error 钩子缺失时帧异常重抛：fire 返回 Err(Core)，会话未被毒化——
/// 下一帧照常回信封（契约「丢帧继续」的池侧钉）。
#[tokio::test(flavor = "current_thread")]
async fn pool_uncaught_frame_error_keeps_session_usable() {
    let dir = pool_dir("pool5");
    let ws_file = dir.join("ws.js");
    std::fs::write(
        &ws_file,
        r#"
export default {
  message() {
    if (http.body.boom) throw new Error("boom-norer");
    json.ok({ ok: 1 });
  },
};
"#,
    )
    .unwrap();
    let pool = RoutePool::new(ws_file, ws_test_bridge(&dir), Duration::from_secs(1), 1, 0);
    let (btx, _brx) = tokio::sync::mpsc::unbounded_channel();
    let h = pool.attach(btx);
    let e = h
        .fire("message", br#"{"boom":true}"#.to_vec(), false)
        .await
        .unwrap_err();
    assert!(
        matches!(e, FrameError::Core(ref x) if !x.to_string().is_empty()),
        "重抛的异常经 FrameError::Core 带出: {e:?}"
    );
    // 会话未被毒化：下一帧照常回信封（连接继续契约的根基）。
    let o = h
        .fire("message", br#"{"boom":false}"#.to_vec(), false)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&o.capture.body).unwrap();
    assert_eq!(v["data"]["ok"], 1);
    h.detach();
}

/// 空池 linger 退役：detach 后（linger=0 立即）Worker 退出、live 归零；
/// 新连接 attach 复活（锁内见 closed 即复活 + 补员）。
#[tokio::test(flavor = "current_thread")]
async fn pool_retires_when_idle_after_linger() {
    let dir = pool_dir("retire");
    let ws_file = dir.join("ws.js");
    std::fs::write(
        &ws_file,
        r#"export default { message() { json.ok({ ok: 1 }); } };"#,
    )
    .unwrap();
    let pool = RoutePool::new(ws_file, ws_test_bridge(&dir), Duration::from_secs(1), 1, 0);
    let (btx, _) = mpsc::unbounded_channel();
    let h = pool.attach(btx);
    h.fire("message", vec![], false).await.unwrap();
    assert_eq!(pool.live_workers(), 1);
    h.detach();
    // linger=0：detach 后 Worker 异步退出（等一小会儿）
    for _ in 0..50 {
        if pool.live_workers() == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(pool.live_workers(), 0, "空池立即退役");
    // 新连接复活
    let (btx2, _) = mpsc::unbounded_channel();
    let h2 = pool.attach(btx2);
    h2.fire("message", vec![], false).await.unwrap();
    h2.detach();
}

/// 预载失败锁存：fire 得 PoolClosed 后，后续 attach 不再反复起 Worker。
#[tokio::test(flavor = "current_thread")]
async fn pool_preload_failure_latches_closed() {
    let dir = pool_dir("preload-fail");
    let ws_file = dir.join("ws.js");
    std::fs::write(&ws_file, "json.ok({});\n").unwrap(); // 无钩子 → 预载失败
    let pool = RoutePool::new(ws_file, ws_test_bridge(&dir), Duration::from_secs(1), 1, 0);
    let (btx, _) = mpsc::unbounded_channel();
    let h = pool.attach(btx);
    assert!(matches!(
        h.fire("message", vec![], false).await,
        Err(FrameError::PoolClosed)
    ));
    // 锁存：后续 attach 不再反复起 worker
    let (btx2, _) = mpsc::unbounded_channel();
    let h2 = pool.attach(btx2);
    assert!(matches!(
        h2.fire("message", vec![], false).await,
        Err(FrameError::PoolClosed)
    ));
    assert_eq!(pool.live_workers(), 0);
}

/// 毒化补员容量恢复钉（v0.1.10 终审 #2）：W=2 时一个 Worker 毒化退出后，池必须
/// 在有限时间内补回满容量（live 回到 2）——旧条件「live == 0 才补」下 W>=2 时
/// live 停在 1、池永久降容（本钉在旧代码下恒红；处方中的「W=1」笔误不触发旧
/// bug——W=1 时 live 恰归零、旧条件恰好成立，故钉在 W=2）。
#[tokio::test(flavor = "current_thread")]
async fn pool_respawns_to_full_capacity_after_poison() {
    let dir = pool_dir("respawn2");
    let ws_file = dir.join("ws.js");
    std::fs::write(
        &ws_file,
        r#"
export default {
  message() {
    if (http.body.boom) { const t = Date.now(); while (Date.now() - t < 60_000) {} }
    json.ok({ ok: 1 });
  },
};
"#,
    )
    .unwrap();
    let pool = RoutePool::new(
        ws_file,
        ws_test_bridge(&dir),
        Duration::from_millis(300),
        2,
        0,
    );
    let (b1, _) = mpsc::unbounded_channel();
    let bad = pool.attach(b1);
    assert_eq!(pool.live_workers(), 2);
    let e = bad
        .fire("message", br#"{"boom":true}"#.to_vec(), false)
        .await
        .unwrap_err();
    assert!(matches!(e, FrameError::Timeout));
    // 先等毒化落定：done 先于 live 扣减解析，直接等 live==2 会读到扣减前的
    // 旧值而空过。poisoned 在 worker_main 返回后递增，此刻 live 必已扣减。
    let mut seen = false;
    for _ in 0..100 {
        if pool.poisoned.load(Ordering::SeqCst) == 1 {
            seen = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(seen, "毒化 Worker 必须退出（poisoned=1）");
    // 再等补员：live 有限时间内回到满容量 2（旧代码停在 1）
    let mut full = false;
    for _ in 0..100 {
        if pool.live_workers() == 2 {
            full = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        full,
        "毒化后必须补员回满容量 2，实际 {}",
        pool.live_workers()
    );
    // 补员后新会话照常执行
    let (b2, _) = mpsc::unbounded_channel();
    let good = pool.attach(b2);
    let o = good
        .fire("message", br#"{"boom":false}"#.to_vec(), false)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&o.capture.body).contains("\"ok\":1"));
    bad.detach();
    good.detach();
}

/// worker panic 存活钉（v0.1.10 终审 #3）：make 闭包首次调用必 panic——修复前
/// 线程死亡、live 永不递减、attach 见 live != 0 不再补员，帧永久挂起；修复后
/// catch_unwind 收敛 + 补扣 live + 补员，帧由补员 Worker 正常执行。外层超时
/// 兜底：回归时表现为挂起而非断言失败，超时将其转为红。
#[tokio::test(flavor = "current_thread")]
async fn pool_survives_worker_panic_and_respawns() {
    let dir = pool_dir("panic");
    let ws_file = dir.join("ws.js");
    std::fs::write(
        &ws_file,
        r#"export default { message() { json.ok({ ok: 1 }); } };"#,
    )
    .unwrap();
    let inner = ws_test_bridge(&dir);
    let calls = std::sync::Arc::new(AtomicUsize::new(0));
    let make: std::sync::Arc<dyn Fn() -> Bridge + Send + Sync> = std::sync::Arc::new(move || {
        if calls.fetch_add(1, Ordering::SeqCst) == 0 {
            panic!("test-injected worker panic");
        }
        inner()
    });
    let pool = RoutePool::new(ws_file, make, Duration::from_secs(1), 1, 0);
    let (btx, _) = mpsc::unbounded_channel();
    let h = pool.attach(btx);
    let o = tokio::time::timeout(Duration::from_secs(10), h.fire("message", vec![], false))
        .await
        .expect("worker panic 后池必须自愈，不得永久挂起")
        .unwrap();
    assert!(String::from_utf8_lossy(&o.capture.body).contains("\"ok\":1"));
    assert_eq!(pool.live_workers(), 1);
    h.detach();
}

/// retire 线程去重钉：linger 在途期间的空 detach 不再叠加 retire 线程；
/// 在途 retire 落定后标记复位，后续空 detach 照常退役。
#[tokio::test(flavor = "current_thread")]
async fn retire_dedupes_while_linger_in_flight_and_resets_after() {
    let dir = pool_dir("retire-dedupe");
    let ws_file = dir.join("ws.js");
    std::fs::write(
        &ws_file,
        r#"export default { message() { json.ok({ ok: 1 }); } };"#,
    )
    .unwrap();
    // linger 取 300ms：足以让测试在在途窗口内完成去重断言，又不必等 60s 验证复位。
    let pool = RoutePool::new(
        ws_file,
        ws_test_bridge(&dir),
        Duration::from_secs(1),
        1,
        300,
    );
    let (btx, _) = mpsc::unbounded_channel();
    let h = pool.attach(btx);
    h.fire("message", vec![], false).await.unwrap();
    h.detach();
    for _ in 0..50 {
        if pool.retiring.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        pool.retiring.load(Ordering::SeqCst),
        "detach 后 retire 在途"
    );
    // linger 在途期间第二条连接来了又走：不再 spawn 第二个 retire（去重），
    // 池也未退役。
    let (btx2, _) = mpsc::unbounded_channel();
    let h2 = pool.attach(btx2);
    h2.fire("message", vec![], false).await.unwrap();
    h2.detach();
    assert!(pool.retiring.load(Ordering::SeqCst));
    assert!(
        !pool
            .sched
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .closed,
        "linger 在途期间池不得退役"
    );
    // 在途 retire 落定：仍空 → 退役 + 标记复位（后续 detach 可再排退休）。
    let mut retired = false;
    for _ in 0..100 {
        if !pool.retiring.load(Ordering::SeqCst)
            && pool
                .sched
                .inner
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .closed
        {
            retired = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(retired, "linger 到期必须退役并复位 retiring");
}

/// sess.state 不可序列化赋值不清空旧状态（v0.1.10 终审 #4）：帧2 给 sess.state
/// 写入 BigInt（serde_v8 无法序列化为 JSON Value → dispatcher finally 的
/// op_ws_sess_set 抛错被 catch → 无回传；函数属性会被 serde_v8 静默吞掉，
/// 不触发失败路径，故以 BigInt 为触发器），
/// 帧3 读回帧1 写入的状态必须仍在——旧实现 unwrap_or(Null) 写回会整体清空。
#[tokio::test(flavor = "current_thread")]
async fn pool_non_serializable_sess_assignment_keeps_old_state() {
    let dir = pool_dir("sess-keep");
    let ws_file = dir.join("ws.js");
    std::fs::write(
        &ws_file,
        r#"
export default {
  message() {
    if (http.body.step === 1) sess.state.x = 1;
    if (http.body.step === 2) sess.state.b = 1n;
    json.ok({ x: sess.state.x ?? null });
  },
};
"#,
    )
    .unwrap();
    let pool = RoutePool::new(ws_file, ws_test_bridge(&dir), Duration::from_secs(1), 1, 0);
    let (btx, _) = mpsc::unbounded_channel();
    let h = pool.attach(btx);
    let o = h
        .fire("message", br#"{"step":1}"#.to_vec(), false)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&o.capture.body).unwrap();
    assert_eq!(v["data"]["x"], 1);
    let o = h
        .fire("message", br#"{"step":2}"#.to_vec(), false)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&o.capture.body).unwrap();
    assert_eq!(v["data"]["x"], 1);
    // 关键断言：不可序列化赋值的那一帧不得清空既有状态
    let o = h
        .fire("message", br#"{"step":3}"#.to_vec(), false)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&o.capture.body).unwrap();
    assert_eq!(v["data"]["x"], 1, "不可序列化赋值后旧状态必须保留");
    h.detach();
}
