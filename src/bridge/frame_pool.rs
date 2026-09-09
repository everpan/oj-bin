//! WS 帧池（spec 2026-09-09）：每路由 队列 + W 个无状态 Worker + Rust 会话表。
//! SOLID：本文件只管「帧的排队/调度/会话表/Worker 池生命周期」；V8 执行细节复用
//! mod.rs 的 WsSession（预载）与 ReqState 捕获链，不在此重复。

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use deno_core::error::CoreError;
use tokio::sync::mpsc;
use tokio::sync::{Notify, oneshot};

use super::{Bridge, RequestInfo, RunError, WsOutcome};

/// 一帧 = 一个无状态作业。
pub(crate) struct Frame {
    pub conn: u64,
    pub ev: &'static str,
    pub body: Vec<u8>,
    pub done: oneshot::Sender<Result<WsOutcome, FrameError>>,
}

/// 帧失败语义：Timeout/Core 对应 RunError（连接侧断连/丢帧），Dropped/PoolClosed 池侧。
#[derive(Debug)]
pub enum FrameError {
    Timeout,
    Core(CoreError),
    /// 连接已 detach：排队帧作废。
    Dropped,
    /// 池已退役/Worker 预载失败。
    PoolClosed,
}

#[derive(Default)]
struct SchedInner {
    ready: VecDeque<Frame>,
    /// per-conn 在飞=1：在飞期间后续帧在此排队（保序）。
    waiting: HashMap<u64, VecDeque<Frame>>,
    in_flight: HashSet<u64>,
    closed: bool,
}

/// 帧调度器：多生产（连接 Reader）/多消费（Worker）。
pub(crate) struct Scheduler {
    inner: Mutex<SchedInner>,
    notify: Notify,
}

impl Scheduler {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(SchedInner::default()),
            notify: Notify::new(),
        })
    }

    pub(crate) fn submit(&self, f: Frame) {
        let mut g = self.inner.lock().unwrap();
        if g.closed {
            let _ = f.done.send(Err(FrameError::PoolClosed));
            return;
        }
        if g.in_flight.contains(&f.conn) {
            g.waiting.entry(f.conn).or_default().push_back(f);
        } else {
            g.in_flight.insert(f.conn);
            g.ready.push_back(f);
        }
        drop(g);
        self.notify.notify_one();
    }

    /// 取下一帧；池关闭（退役）返回 None → Worker 退出。
    pub(crate) async fn pull(&self) -> Option<Frame> {
        loop {
            let notified = self.notify.notified();
            {
                let mut g = self.inner.lock().unwrap();
                if let Some(f) = g.ready.pop_front() {
                    return Some(f);
                }
                if g.closed {
                    return None;
                }
            }
            notified.await;
        }
    }

    /// Worker 执行完一帧：放行该连接的下一帧。
    pub(crate) fn complete(&self, conn: u64) {
        let mut g = self.inner.lock().unwrap();
        g.in_flight.remove(&conn);
        if let Some(q) = g.waiting.get_mut(&conn)
            && let Some(f) = q.pop_front()
        {
            if g.closed {
                let _ = f.done.send(Err(FrameError::PoolClosed));
            } else {
                g.in_flight.insert(conn);
                g.ready.push_back(f);
            }
        }
        drop(g);
        self.notify.notify_one();
    }

    /// 连接 detach：排队帧作废；在飞帧让它自然跑完（complete 无后续可放）。
    pub(crate) fn drop_conn(&self, conn: u64) {
        let mut g = self.inner.lock().unwrap();
        if let Some(q) = g.waiting.remove(&conn) {
            for f in q {
                let _ = f.done.send(Err(FrameError::Dropped));
            }
        }
    }

    /// 退役：拒绝新帧、排空存量（Worker pull→None 退出）。
    pub(crate) fn close(&self) {
        let mut g = self.inner.lock().unwrap();
        g.closed = true;
        for f in g.ready.drain(..) {
            let _ = f.done.send(Err(FrameError::PoolClosed));
        }
        for (_, q) in g.waiting.drain() {
            for f in q {
                let _ = f.done.send(Err(FrameError::PoolClosed));
            }
        }
        drop(g);
        self.notify.notify_waiters();
    }

    /// 复活（退役后新连接 attach）。
    pub(crate) fn reopen(&self) {
        self.inner.lock().unwrap().closed = false;
    }
}

/// Rust 侧会话表条目：sess.state 真身 + 该连接 bus 发送端。
struct SessEntry {
    state: serde_json::Value,
    bus_tx: mpsc::UnboundedSender<String>,
}

/// 单路由帧池：队列 + W Worker + 会话表。懒启动（首次 attach 起 Worker）；
/// 空池 linger 退役；预载失败锁存（该文件视为永久不可用）。
pub struct RoutePool {
    file: PathBuf,
    timeout: Duration,
    workers_max: usize,
    /// 空池 linger 退役：detach 后空置超过该时长则 close（0 = 立即）。
    linger_ms: u64,
    make: Arc<dyn Fn() -> Bridge + Send + Sync>,
    sched: Arc<Scheduler>,
    sessions: Mutex<HashMap<u64, SessEntry>>,
    /// 预载失败锁存：Some(错误串) 后本池不再起 Worker，attach 的 fire 恒 PoolClosed。
    failed: Mutex<Option<String>>,
    conn_seq: AtomicU64,
    live: AtomicUsize,
    poisoned: AtomicUsize,
}

impl RoutePool {
    pub fn new(
        file: PathBuf,
        make: Arc<dyn Fn() -> Bridge + Send + Sync>,
        timeout: Duration,
        workers_max: usize,
        linger_ms: u64,
    ) -> Arc<Self> {
        Arc::new(Self {
            file,
            timeout,
            workers_max,
            linger_ms,
            make,
            sched: Scheduler::new(),
            sessions: Mutex::new(HashMap::new()),
            failed: Mutex::new(None),
            conn_seq: AtomicU64::new(0),
            live: AtomicUsize::new(0),
            poisoned: AtomicUsize::new(0),
        })
    }

    /// 注册会话（懒启动 worker）。永远成功；预载失败在 fire 时以 PoolClosed 显形。
    pub fn attach(self: &Arc<Self>, bus_tx: mpsc::UnboundedSender<String>) -> ConnHandle {
        if self.failed.lock().unwrap().is_some() {
            // 锁存：不再起 Worker；fire 经 sched closed 得 PoolClosed。
            return ConnHandle {
                pool: Arc::clone(self),
                conn: 0,
            };
        }
        if self.live.load(Ordering::SeqCst) == 0 {
            self.sched.reopen();
        }
        let conn = self.conn_seq.fetch_add(1, Ordering::Relaxed) + 1;
        self.sessions.lock().unwrap().insert(
            conn,
            SessEntry {
                state: serde_json::Value::Null,
                bus_tx,
            },
        );
        if self.live.load(Ordering::SeqCst) == 0 {
            for _ in 0..self.workers_max.max(1) {
                self.spawn_worker();
            }
        }
        ConnHandle {
            pool: Arc::clone(self),
            conn,
        }
    }

    pub fn live_workers(&self) -> usize {
        self.live.load(Ordering::SeqCst)
    }

    pub(crate) fn spawn_worker(self: &Arc<Self>) {
        let pool = Arc::clone(self);
        self.live.fetch_add(1, Ordering::SeqCst);
        std::thread::Builder::new()
            .name("ws-worker".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("ws worker rt");
                // live 的扣减在 worker_main 内部（退出即扣，先于 close 等副作用可见）。
                let poisoned = rt.block_on(pool.clone().worker_main());
                if poisoned {
                    pool.poisoned.fetch_add(1, Ordering::SeqCst);
                    // 补员：毒化 Worker 线程退出前直接再起一个（保持池容量）。
                    if pool.live.load(Ordering::SeqCst) == 0 && pool.has_sessions() {
                        pool.spawn_worker();
                    }
                }
            })
            .expect("spawn ws-worker");
    }

    fn has_sessions(&self) -> bool {
        !self.sessions.lock().unwrap().is_empty()
    }

    /// Worker 主循环：返回是否毒化退出。live 在此扣减（每个退出路径恰一次）。
    async fn worker_main(self: Arc<Self>) -> bool {
        let bridge = (self.make)();
        // 预载：模块加载 + 钩子装配（复用 ws_connect；失败 → 本 Worker 不可用，
        // 排空队列让 fire 立刻拿到 PoolClosed——预载失败 = 该文件永久不可用，
        // 退役整池是正确语义；锁存后 attach 不再起 Worker）。
        let mut sess = match bridge.ws_connect(&self.file).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("ws worker preload {}: {e}", self.file.display());
                // 顺序即契约：先锁存（拦住新 attach），再扣 live，再关池（排空
                // 存量帧）——fire 解析时 live 已归零，不存在「死而未报」的窗口。
                *self.failed.lock().unwrap() = Some(e.to_string());
                self.live.fetch_sub(1, Ordering::SeqCst);
                self.sched.close();
                return false;
            }
        };
        loop {
            let Some(f) = self.sched.pull().await else {
                self.live.fetch_sub(1, Ordering::SeqCst);
                return false;
            };
            let (state, bus_tx) = {
                let g = self.sessions.lock().unwrap();
                match g.get(&f.conn) {
                    Some(e) => (e.state.clone(), Some(e.bus_tx.clone())),
                    None => (serde_json::Value::Null, None),
                }
            };
            let req = RequestInfo {
                method: "WS".into(),
                body: f.body,
                bus_tx,
                ..Default::default()
            };
            let r = ws_event(&mut sess, f.conn, f.ev, &req, &state, self.timeout).await;
            self.sched.complete(f.conn);
            match r {
                Ok((outcome, new_state)) => {
                    if let Some(e) = self.sessions.lock().unwrap().get_mut(&f.conn) {
                        e.state = new_state;
                    }
                    let _ = f.done.send(Ok(outcome));
                }
                Err(RunError::Timeout) => {
                    let _ = f.done.send(Err(FrameError::Timeout));
                    drop(sess); // runtime 已毒化，随 Worker 丢弃
                    self.live.fetch_sub(1, Ordering::SeqCst);
                    return true;
                }
                Err(RunError::Core(e)) => {
                    let _ = f.done.send(Err(FrameError::Core(e)));
                }
            }
        }
    }
}

/// 单事件执行（`WsSession::fire` 的无状态化版本）：重置 per-event 状态、武装看门狗、
/// 注入 __sess/__ws_conn → `__ws_call(conn, ev)` → 排空 event loop → 读 ReqState
/// （sends/close/capture/ws_sess）。超时 → runtime 已被 terminate（毒化），
/// 调用方必须丢弃会话断连。
pub(crate) async fn ws_event(
    sess: &mut super::WsSession,
    conn: u64,
    ev: &str,
    req: &RequestInfo,
    state: &serde_json::Value,
    timeout: Duration,
) -> Result<(WsOutcome, serde_json::Value), RunError> {
    {
        let op_state = super::runtime::op_state(&sess.rt);
        let mut g = op_state.borrow_mut();
        g.borrow_mut::<super::ReqState>().reset(req.clone());
    }
    let handle = sess.rt.v8_isolate().thread_safe_handle();
    sess.kill.arm(handle, timeout);
    // ev 为内部常量，仍走 JSON 字面量杜绝意外注入（同 run_module 的 method_lit）。
    let ev_lit = serde_json::to_string(ev).unwrap_or_else(|_| "\"\"".into());
    let sess_lit = serde_json::to_string(state).unwrap_or_else(|_| "null".into());
    let code = format!(
        "globalThis.__sess = {sess_lit}; globalThis.__ws_conn = {conn}; globalThis.__ws_call({conn}, {ev_lit});"
    );
    let result = match sess.rt.execute_script("ws_event.js", code) {
        Ok(_) => {
            sess.rt
                .run_event_loop(deno_core::PollEventLoopOptions::default())
                .await
        }
        Err(e) => Err(CoreError::from(e)),
    };
    if sess.kill.disarm() {
        // 同 run_ws 超时路径：不 drain，runtime 丢弃。
        return Err(RunError::Timeout);
    }
    result.map_err(RunError::Core)?;
    let (outcome, ws_sess) = {
        let op_state = super::runtime::op_state(&sess.rt);
        let g = op_state.borrow();
        let rs = g.borrow::<super::ReqState>();
        (
            WsOutcome {
                capture: super::Bridge::read_capture(&sess.rt),
                sends: rs.ws_sends.clone(),
                close: rs.ws_close,
            },
            rs.ws_sess.clone().unwrap_or(serde_json::Value::Null),
        )
    };
    super::Bridge::finalize_tx(&sess.rt).await;
    Ok((outcome, ws_sess))
}

/// 连接句柄（Send）：投帧 + 等结果。
pub struct ConnHandle {
    pool: Arc<RoutePool>,
    conn: u64,
}

impl ConnHandle {
    pub async fn fire(&self, ev: &'static str, body: Vec<u8>) -> Result<WsOutcome, FrameError> {
        let (tx, rx) = oneshot::channel();
        self.pool.sched.submit(Frame {
            conn: self.conn,
            ev,
            body,
            done: tx,
        });
        match rx.await {
            Ok(r) => r,
            Err(_) => Err(FrameError::PoolClosed), // worker 恐慌/drop
        }
    }

    /// 连接收尾：删会话表条目 + 丢排队帧 + 空池 linger 退役计时。
    pub fn detach(&self) {
        self.pool.sessions.lock().unwrap().remove(&self.conn);
        self.pool.sched.drop_conn(self.conn);
        // 空池 → linger 计时（独立线程；linger=0 走 sleep(0)，同一路径）。
        if self.pool.sessions.lock().unwrap().is_empty() {
            let pool = Arc::clone(&self.pool);
            std::thread::Builder::new()
                .name("ws-retire".into())
                .spawn(move || {
                    std::thread::sleep(Duration::from_millis(pool.linger_ms));
                    // 复查仍空才退役：linger 期间来了新连接则继续服务。
                    if pool.sessions.lock().unwrap().is_empty() {
                        pool.sched.close();
                    }
                })
                .expect("spawn ws-retire");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Bridge, Extras, InMemoryKV, LoaderShared, SchemaRegistry};
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
        s.reopen(); // 退役后新连接可复活
        let (fd, _rd) = frame(3);
        s.submit(fd);
        assert!(s.pull().await.is_some());
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
        let o = h.fire("connection", vec![]).await.unwrap();
        assert!(String::from_utf8_lossy(&o.capture.body).contains("\"hello\":1"));
        for expect in [1, 2] {
            let o = h.fire("message", vec![]).await.unwrap();
            let v: serde_json::Value = serde_json::from_slice(&o.capture.body).unwrap();
            assert_eq!(v["data"]["n"], expect, "sess.state 跨帧持久（Rust 会话表）");
            assert_eq!(v["data"]["gid"], 1, "sess.id = 连接 id");
        }
        let o = h.fire("close", vec![]).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&o.capture.body).unwrap();
        assert_eq!(v["data"]["bye"], 2, "close 收尾可见累计状态");
        // 第二条连接：状态互不可见（同池隔离）
        let (btx2, _brx2) = tokio::sync::mpsc::unbounded_channel();
        let h2 = pool.attach(btx2);
        h2.fire("connection", vec![]).await.unwrap();
        let o = h2.fire("message", vec![]).await.unwrap();
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
            .fire("message", br#"{"boom":true}"#.to_vec())
            .await
            .unwrap_err();
        assert!(matches!(e, FrameError::Timeout));
        // 毒化半径=1：另一连接照常（补员或第二 worker 接帧）
        let o = good
            .fire("message", br#"{"boom":false}"#.to_vec())
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
        let e = h.fire("message", vec![]).await.unwrap_err();
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
        let o = h.fire("message", vec![]).await.unwrap();
        assert_eq!(o.sends, vec!["err:boom".to_string()]);
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
            .fire("message", br#"{"boom":true}"#.to_vec())
            .await
            .unwrap_err();
        assert!(
            matches!(e, FrameError::Core(ref x) if !x.to_string().is_empty()),
            "重抛的异常经 FrameError::Core 带出: {e:?}"
        );
        // 会话未被毒化：下一帧照常回信封（连接继续契约的根基）。
        let o = h
            .fire("message", br#"{"boom":false}"#.to_vec())
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&o.capture.body).unwrap();
        assert_eq!(v["data"]["ok"], 1);
        h.detach();
    }

    /// 空池 linger 退役：detach 后（linger=0 立即）Worker 退出、live 归零；
    /// 新连接 attach 复活（reopen + 重新起 Worker）。
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
        h.fire("message", vec![]).await.unwrap();
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
        h2.fire("message", vec![]).await.unwrap();
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
            h.fire("message", vec![]).await,
            Err(FrameError::PoolClosed)
        ));
        // 锁存：后续 attach 不再反复起 worker
        let (btx2, _) = mpsc::unbounded_channel();
        let h2 = pool.attach(btx2);
        assert!(matches!(
            h2.fire("message", vec![]).await,
            Err(FrameError::PoolClosed)
        ));
        assert_eq!(pool.live_workers(), 0);
    }
}
