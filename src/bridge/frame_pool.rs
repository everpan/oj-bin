//! WS 帧池（spec 2026-09-09）：每路由 队列 + W 个无状态 Worker + Rust 会话表。
//! SOLID：本文件只管「帧的排队/调度/会话表/Worker 池生命周期」；V8 执行细节复用
//! mod.rs 的 WsSession（预载）与 ReqState 捕获链，不在此重复。

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
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
    /// 帧型（v0.1.16）：WS binary 帧 true → `RequestInfo.body_binary`。
    pub binary: bool,
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

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::Timeout => write!(f, "frame timeout"),
            FrameError::Core(e) => write!(f, "{e}"),
            FrameError::Dropped => write!(f, "frame dropped"),
            FrameError::PoolClosed => write!(f, "ws pool closed"),
        }
    }
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
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
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
                let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
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
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
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
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(q) = g.waiting.remove(&conn) {
            for f in q {
                let _ = f.done.send(Err(FrameError::Dropped));
            }
        }
    }

    /// Worker 帧中 panic 后的连接清场（panic 恢复路径专用）：在飞帧已随 unwind
    /// 消亡（done 被 drop → fire 得 PoolClosed），复用 drop_conn 作废排队帧并清
    /// 在飞标记——同连接后续帧被正常调度而非永久挂起。
    pub(crate) fn panic_conn(&self, conn: u64) {
        self.drop_conn(conn);
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.in_flight.remove(&conn);
    }

    /// 退役：拒绝新帧、排空存量（Worker pull→None 退出）。
    pub(crate) fn close(&self) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
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
}

/// Rust 侧会话表条目：sess.state 真身 + 该连接 bus 发送端。
struct SessEntry {
    state: serde_json::Value,
    bus_tx: mpsc::UnboundedSender<super::WsSend>,
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
    /// retire 线程在途标记：空池 detach 去重，不重复叠 sleep 线程。
    retiring: AtomicBool,
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
            retiring: AtomicBool::new(false),
        })
    }

    /// 注册会话（懒启动 worker）。永远成功；预载失败在 fire 时以 PoolClosed 显形。
    pub fn attach(self: &Arc<Self>, bus_tx: mpsc::UnboundedSender<super::WsSend>) -> ConnHandle {
        if self
            .failed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
        {
            // 锁存：不再起 Worker；fire 经 sched closed 得 PoolClosed。
            return ConnHandle {
                pool: Arc::clone(self),
                conn: 0,
            };
        }
        // 退役竞态：linger 线程的 sched.close() 只代表「复查时无会话」，close 落地时
        // Worker 可能尚未退出（live 未归零）。若只按 live==0 决定 reopen，此窗口内新
        // 连接的 fire 会直接吃 PoolClosed（客户端首帧即 Close）。唯一可靠信号是
        // closed 本身：见 closed 即 reopen + 无条件补员——垂死 Worker 若已见到 closed
        // 会照常退出，补员覆盖其容量；若尚未见到则继续服务，短暂超配无害。
        let spawn = {
            let mut g = self.sched.inner.lock().unwrap_or_else(|e| e.into_inner());
            if g.closed {
                g.closed = false;
                true
            } else {
                self.live.load(Ordering::SeqCst) == 0
            }
        };
        let conn = self.conn_seq.fetch_add(1, Ordering::Relaxed) + 1;
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                conn,
                SessEntry {
                    state: serde_json::Value::Null,
                    bus_tx,
                },
            );
        if spawn {
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
        // 本 Worker 当前正在执行的连接（0 = 不在帧中）：panic 恢复时据此清场。
        let cur = Arc::new(AtomicU64::new(0));
        self.live.fetch_add(1, Ordering::SeqCst);
        std::thread::Builder::new()
            .name("ws-worker".into())
            .spawn(move || {
                // worker 闭包内任何 panic（rt 构建失败、ws_event 意外 panic 等）都收敛于此：
                // 补扣 live + 补员，否则 live 永不递减 → attach 见 live != 0 不再起 Worker →
                // 该路由永久死亡。worker_main 保证 fetch_sub 之后无可 panic 代码（锁均防
                // 毒化），故 panic 只可能发生在扣减之前，这里补扣不会双重扣减。
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("ws worker rt");
                    // live 的扣减在 worker_main 内部（退出即扣，先于 close 等副作用可见）。
                    rt.block_on(pool.clone().worker_main(cur.clone()))
                }));
                match r {
                    Ok(poisoned) => {
                        if poisoned {
                            pool.poisoned.fetch_add(1, Ordering::SeqCst);
                            // 补员：毒化 Worker 线程退出前直接再起一个（保持池容量）。
                            respawn_if_needed(&pool);
                        }
                    }
                    Err(e) => {
                        eprintln!("ws worker panicked: {e:?}");
                        // 帧中 panic：清场该连接（在飞标记 + 排队帧），同连接后续帧
                        // 立刻回错而非永久挂起；cur==0 = panic 未发生在帧执行中
                        // （rt 构建/make 阶段），无需清场。
                        let conn = cur.load(Ordering::SeqCst);
                        if conn != 0 {
                            pool.sched.panic_conn(conn);
                        }
                        pool.live.fetch_sub(1, Ordering::SeqCst);
                        respawn_if_needed(&pool);
                    }
                }
            })
            .expect("spawn ws-worker");
    }

    fn has_sessions(&self) -> bool {
        !self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }

    /// Worker 主循环：返回是否毒化退出。live 在此扣减（每个退出路径恰一次）。
    /// cur = 本 Worker 在飞连接标记（panic 恢复路径据此清场）。
    async fn worker_main(self: Arc<Self>, cur: Arc<AtomicU64>) -> bool {
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
                *self.failed.lock().unwrap_or_else(|e| e.into_inner()) = Some(e.to_string());
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
                let g = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
                match g.get(&f.conn) {
                    Some(e) => (e.state.clone(), Some(e.bus_tx.clone())),
                    None => (serde_json::Value::Null, None),
                }
            };
            let req = RequestInfo {
                method: "WS".into(),
                body: f.body,
                body_binary: f.binary,
                bus_tx,
                ..Default::default()
            };
            let r = {
                cur.store(f.conn, Ordering::SeqCst);
                let r = ws_event(&mut sess, f.conn, f.ev, &req, &state, self.timeout).await;
                self.sched.complete(f.conn);
                cur.store(0, Ordering::SeqCst);
                r
            };
            match r {
                Ok((outcome, new_state)) => {
                    // new_state=None = dispatcher finally 回传失败（sess 不可 JSON 序列化，
                    // op 调用抛错被 catch）——跳过写回保留旧状态，不得用 Null 整体清空。
                    if let Some(ns) = new_state
                        && let Some(e) = self
                            .sessions
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .get_mut(&f.conn)
                    {
                        e.state = ns;
                    }
                    let _ = f.done.send(Ok(outcome));
                }
                Err(RunError::Timeout) => {
                    let _ = f.done.send(Err(FrameError::Timeout));
                    drop(sess); // runtime 已毒化，随 Worker 丢弃
                    self.live.fetch_sub(1, Ordering::SeqCst);
                    return true;
                }
                Err(RunError::Oom { limit }) => {
                    // PR-4：堆超限 = runtime 已 terminate 毒化（同 Timeout 处置：断连 + 丢弃）。
                    let _ = f.done.send(Err(FrameError::Core(deno_core::error::CoreError::from(
                        std::io::Error::other(format!(
                            "js heap limit exceeded (limit {limit} bytes; server.js_heap_limit_bytes)"
                        )),
                    ))));
                    drop(sess);
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

/// 补员判定（毒化/panic 退出共用）：仍有会话且容量未满则再起一个 Worker。
/// 容量未满即补（非「归零才补」）——W>=2 时单个 Worker 退出后 live 仍 > 0，
/// 「== 0 才补」会让池永久降容。
fn respawn_if_needed(pool: &Arc<RoutePool>) {
    if pool.live.load(Ordering::SeqCst) < pool.workers_max.max(1) && pool.has_sessions() {
        pool.spawn_worker();
    }
}

/// 单事件执行（`WsSession::fire` 的无状态化版本）：重置 per-event 状态、武装看门狗、
/// 注入 __sess/__ws_conn → `__ws_call(conn, ev)` → 排空 event loop → 读 ReqState
/// （sends/close/capture/ws_sess）。超时 → runtime 已被 terminate（毒化），
/// 调用方必须丢弃会话断连。state 回传为 Option：None = dispatcher finally 的
/// op_ws_sess_set 未执行成功（sess 含不可 JSON 序列化值），调用方须保留旧状态。
pub(crate) async fn ws_event(
    sess: &mut super::WsSession,
    conn: u64,
    ev: &str,
    req: &RequestInfo,
    state: &serde_json::Value,
    timeout: Duration,
) -> Result<(WsOutcome, Option<serde_json::Value>), RunError> {
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
            rs.ws_sess.clone(),
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
    /// 投一帧给池（binary = WS binary 帧；connection/close 事件传 (Vec::new(), false)）。
    pub async fn fire(
        &self,
        ev: &'static str,
        body: Vec<u8>,
        binary: bool,
    ) -> Result<WsOutcome, FrameError> {
        let (tx, rx) = oneshot::channel();
        self.pool.sched.submit(Frame {
            conn: self.conn,
            ev,
            body,
            binary,
            done: tx,
        });
        match rx.await {
            Ok(r) => r,
            Err(_) => Err(FrameError::PoolClosed), // worker 恐慌/drop
        }
    }

    /// 连接收尾：删会话表条目 + 丢排队帧 + 空池 linger 退役计时。
    /// 所有退出路径汇合于此，WS 房间中心随此一并摘除本连接（oj-6）。
    pub fn detach(&self) {
        super::ws::hub().remove_conn(self.conn);
        self.pool
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.conn);
        self.pool.sched.drop_conn(self.conn);
        // 空池 → linger 计时（共享单一 retire 线程；linger=0 走 sleep(0)，同一路径）。
        // 在途 retire 未落定则不重复 spawn（此前每次空 detach 都叠一个 sleep 线程）。
        // ponytail: linger 期间来了又走的连接复用在途计时，退役可能早于其完整
        // linger——空池退役是纯优化（attach 见 closed 即复活），取最简去重。
        if self
            .pool
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
            && !self.pool.retiring.swap(true, Ordering::SeqCst)
        {
            let pool = Arc::clone(&self.pool);
            std::thread::Builder::new()
                .name("ws-retire".into())
                .spawn(move || {
                    std::thread::sleep(Duration::from_millis(pool.linger_ms));
                    // 复查仍空才退役：linger 期间来了新连接则继续服务。
                    if pool
                        .sessions
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .is_empty()
                    {
                        pool.sched.close();
                    }
                    pool.retiring.store(false, Ordering::SeqCst);
                })
                .expect("spawn ws-retire");
        }
    }
}

#[cfg(test)]
#[path = "frame_pool_tests.rs"]
mod tests;
