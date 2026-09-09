# WS 帧池运行时（v0.1.10）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** WS 执行模型从「每连接独占 runtime」改为「每路由帧队列 + W 个无状态 Worker 池 + sess.state 外置 Rust 会话表」——毒化半径回归 1 连接、内存与连接数解耦。

**Architecture:** 每帧是无状态作业：Reader 投帧进路由 Scheduler（per-conn 在飞=1 保序），W 个 Worker（各 1 线程 + 1 个预载路由模块的暖 JsRuntime）拉帧执行，经 `op_ws_sess_set` 把 `__sess` 快照回传 ReqState，Worker 直读 ReqState 收 `ws_sends`/`ws_close`/信封后经 oneshot 回连接。毒化（帧超时 terminate）只杀执行帧的 Worker，池补员，其它连接无感。

**Tech Stack:** Rust（deno_core 0.411 / axum / tokio）、JS ESM。Spec：`docs/superpowers/specs/2026-09-09-ws-frame-pool-design.md`。

## 对 spec 的两处工程澄清（已核实，按此执行）

1. **done-op 简化**：spec 写「done-op 带回 {sends, close, capture, sess_state}」。帧池下 Worker 自己就是执行者（执行完才知道完成，无需通知宿主），sends/close/capture 沿用 Worker 直读 ReqState 的现有模式；**仅新增 `op_ws_sess_set`**（dispatcher `finally` 把 `__sess` 交还 `ReqState.ws_sess`）+ `ReqState.ws_sess` 一个新字段。
2. **懒启动 + linger**：路由 Worker 池**不在装配期创建**，首次 attach 时懒启动（spawn+boot ≈9ms 每空闲周期一次）；`idle_linger_ms` 从「连接归零」起算，默认 0 = 立即退役（用户规则），运维调大吃暖启动。

## Global Constraints

- 所有 cargo 命令一律 `--release`；lint：`cargo clippy --release --all-targets -- -D warnings`。
- Commit message 末行 trailer：`unix@vip.qq.com ai`。
- `JsRuntime` `!Send`：Worker 线程内自持，句柄（ConnHandle/队列）必须 `Send`。
- 失败 runtime 不还池；毒化 Worker 连 runtime 一起丢弃。
- `bootstrap.js` 保持 7-bit ASCII（不改它）。
- 钩子契约不变：`export default { connection, message, close, error }`；返回值忽略；error(e) 后连接继续；**帧超时断开该连接（回归 v0.1.9 契约）**；至少导出一个钩子。
- SOLID/DRY：新逻辑独立成 `src/bridge/frame_pool.rs`（单一职责）；既有捕获链（ReqState 读取、emit 顺序契约）复用不复制。
- **执行纪律（用户指令）**：分阶段推进，每阶段完成 → 更新任务状态 + 输出一行小结；TDD（先测后码）；**不逐任务评审，全部完成后集中终审一次**。
- 最终版本 **v0.1.10**（`oj/Cargo.toml` bump + CHANGELIST v0.1.10 段为发布点提交）。

---

# 阶段一：连接闸门（独立可交付）

### Task 1: config `WsCfg` 段

**Files:**
- Modify: `src/config.rs`（`TasksCfg` 之后加 `WsCfg`；`Config` 加 `ws` 字段；tests 加默认值断言）
- Test: `src/config.rs` tests

**Interfaces:**
- Produces: `pub struct WsCfg { pub max_connections: u64, pub workers_per_route: usize, pub idle_linger_ms: u64 }`（Default：1000 / 2 / 0）；`Config.ws: WsCfg`（`#[serde(default)]`）。

- [ ] **Step 1: 失败测试**（`src/config.rs` tests 内，`defaults_when_no_file` 之后）

```rust
#[test]
fn ws_section_defaults_and_override() {
    let c = load_from(std::path::Path::new("/nonexistent-dir"), None).unwrap();
    assert_eq!((c.ws.max_connections, c.ws.workers_per_route, c.ws.idle_linger_ms), (1000, 2, 0));
    let dir = std::env::temp_dir().join(format!("oj-wscfg-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.yaml"), "ws:\n  max_connections: 5\n  workers_per_route: 3\n  idle_linger_ms: 60000\n").unwrap();
    let c = load_from(&dir, None).unwrap();
    assert_eq!((c.ws.max_connections, c.ws.workers_per_route, c.ws.idle_linger_ms), (5, 3, 60000));
}
```

- [ ] **Step 2: 跑测试确认失败**：`cargo test --release -p only-js ws_section` → 编译失败（无 `ws` 字段）。
- [ ] **Step 3: 实现**（`TasksCfg` impl Default 之后）：

```rust
/// WS 运行时配置（spec 2026-09-09 帧池）。段缺省 = 全默认。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WsCfg {
    /// 全局并发连接上限：超限 upgrade 直接 503；0 = 不限制。
    pub max_connections: u64,
    /// 每路由 Worker 数（无状态，可小于并发连接数）。
    pub workers_per_route: usize,
    /// 路由连接归零后 Worker 池保活毫秒数（0 = 立即退役；调大吃暖启动收益）。
    pub idle_linger_ms: u64,
}

impl Default for WsCfg {
    fn default() -> Self {
        Self { max_connections: 1000, workers_per_route: 2, idle_linger_ms: 0 }
    }
}
```

`Config` 结构体（`tasks` 字段后）加：

```rust
    /// WS 运行时（spec 2026-09-09 帧池）：闸门 / Worker 数 / 空闲退役。
    #[serde(default)]
    pub ws: WsCfg,
```

- [ ] **Step 4: 跑过 + 全量回归**：`cargo test --release -p only-js config` 全 PASS。
- [ ] **Step 5: Commit** `feat(config): ws 段——max_connections/workers_per_route/idle_linger_ms（v0.1.10 帧池 §阶段一）`

### Task 2: server 连接闸门（503）

**Files:**
- Modify: `server/src/ws.rs`（`js_route` 增闸门；静态计数器）
- Test: `server/src/ws.rs` tests

**Interfaces:**
- Consumes: `WsOutcome`/`RoutePool`（阶段二接入；本任务先以闭包参数 `max_conns: u64` 落闸门，Task 6 汇合）
- Produces: `static WS_LIVE: AtomicU64`；`js_route(path, timeout, make_bridge, max_conns: u64)`（本任务临时加参；Task 6 改为 pool 参数时一并保留 max_conns）；mirror_routes 同步加 `max_conns` 透传。

- [ ] **Step 1: 失败测试**（server tests，`WsClient::connect` 返回含状态行——改造 `connect` 断言 101 之外，新增 `connect_expect` 变体，超限时返回原始响应头）

```rust
/// 闸门：max=1 时第 2 条连接被拒（503），存量连接不受影响；max=0 不限。
#[tokio::test]
async fn gate_rejects_over_limit_with_503() {
    let t = crate::tests::routes(&[]);
    let handler = t.0.join("ws.js");
    std::fs::write(&handler, r#"export default { message() { json.ok({ pong: 1 }); } };"#).unwrap();
    let addr = spawn(
        app(/* 拼装同 js_route_runs_handler_per_frame，js_route 最后一个实参传 1 */),
    )
    .await;
    let mut c1 = WsClient::connect(addr, "/ws/gate").await; // 第 1 条：占满
    c1.send_text("hi").await;
    assert!(c1.read_text().await.contains("\"pong\":1"));
    // 第 2 条：upgrade 被拒（非 101）
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    use tokio::io::AsyncWriteExt;
    s.write_all(b"GET /ws/gate HTTP/1.1\r\nHost: t\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n").await.unwrap();
    let mut buf = vec![0u8; 256];
    let n = s.read(&mut buf).await.unwrap();
    assert!(String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 503"), "expected 503");
    // 存量连接不受影响
    c1.send_text("again").await;
    assert!(c1.read_text().await.contains("\"pong\":1"));
}
```

（`app(...)` 拼装与相邻用例逐字相同；`js_route(..., 1)` 末参 = max_conns。）

- [ ] **Step 2: 确认失败**（编译失败：js_route 无第 4 参）。
- [ ] **Step 3: 实现**（`server/src/ws.rs` 顶部）：

```rust
use std::sync::atomic::{AtomicU64, Ordering};

/// 全局 WS 并发连接闸门（config ws.max_connections；0 = 不限）。
static WS_LIVE: AtomicU64 = AtomicU64::new(0);

fn gate_enter(max: u64) -> bool {
    loop {
        let live = WS_LIVE.load(Ordering::Relaxed);
        if max != 0 && live >= max {
            return false;
        }
        if WS_LIVE
            .compare_exchange(live, live + 1, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return true;
        }
    }
}
```

`js_route` 签名加 `max_conns: u64`，upgrade 闭包改：

```rust
        axum::Router::new().route(
            path,
            axum::routing::get(move |ws: axum::extract::WebSocketUpgrade| {
                let file = file.clone();
                let make = make.clone();
                async move {
                    if !gate_enter(max_conns) {
                        return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
                    }
                    ws.on_upgrade(move |socket| async move {
                        conn_on_pinned(socket, file, timeout, make).await;
                        WS_LIVE.fetch_sub(1, Ordering::Relaxed);
                    })
                    .await
                }
            }),
        )
```

`mirror_routes` 加 `max_conns: u64` 透传给 js_route。`conn_on_pinned` 暂保持现有「线程 + frame_loop」形态（Task 6 拆线程）；因 `conn_on_pinned` 当前是 `async fn` 且 `.await` 它即可，无需改内部。
- [ ] **Step 4: 过测 + 全量**：`cargo test --release -p server gate_rejects` PASS；存量用例需为 js_route/mirror_routes 调用补实参（mirror 传 0 = 不限，不影响既有断言）。
- [ ] **Step 5: Commit** `feat(server): WS 全局连接闸门——超限 503、存量不受影响、0=不限（v0.1.10 §阶段一）`

**阶段一小结（完成后输出）**：config ws 段 + 闸门落地，用例 N 过；这一层与帧池正交，先行独立可交付。

---

# 阶段二：bridge 帧池内核（`src/bridge/frame_pool.rs`，新文件）

### Task 3: Scheduler（纯 Rust，无 V8，TDD 快速循环）

**Files:**
- Create: `src/bridge/frame_pool.rs`
- Modify: `src/bridge/mod.rs`（`pub mod frame_pool;` + `pub use frame_pool::{ConnHandle, FrameError, RoutePool};`）
- Test: `src/bridge/frame_pool.rs` `#[cfg(test)]`

**Interfaces:**
- Produces（Task 4/6 依赖，逐字）：`struct Frame { conn: u64, ev: &'static str, body: Vec<u8>, done: oneshot::Sender<Result<WsOutcome, FrameError>> }`；`enum FrameError { Timeout, Core(CoreError), Dropped, PoolClosed }`；`struct Scheduler` + `fn new() -> Arc<Self>`、`fn submit(&self, f: Frame)`、`async fn pull(&self) -> Option<Frame>`、`fn complete(&self, conn: u64)`、`fn drop_conn(&self, conn: u64)`、`fn close(&self)`、`fn reopen(&self)`。

- [ ] **Step 1: 失败测试**（纯 tokio，`#[tokio::test(flavor = "current_thread")]`）

```rust
fn frame(conn: u64) -> (Frame, tokio::sync::oneshot::Receiver<Result<WsOutcome, FrameError>>) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    (Frame { conn, ev: "message", body: vec![], done: tx }, rx)
}

#[tokio::test(flavor = "current_thread")]
async fn scheduler_serializes_per_conn_and_releases_after_complete() {
    let s = Scheduler::new();
    let (f1, r1) = frame(7);
    let (f2, r2) = frame(7);
    s.submit(f1);
    s.submit(f2); // conn7 在飞 → f2 进 waiting
    let got = s.pull().await.unwrap();
    assert_eq!(got.conn, 7);
    s.submit(f2); // （重提交语义已由上一行覆盖，此处仅保证 ready 有下一帧）
    s.complete(7); // 释放 waiting 里的 f2
    let got2 = s.pull().await.unwrap();
    assert_eq!(got2.conn, 7);
    s.complete(7);
}

#[tokio::test(flavor = "current_thread")]
async fn scheduler_drop_conn_discards_queued_and_close_resolves_rest() {
    let s = Scheduler::new();
    let (fa, ra) = frame(1);
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
    let (fd, rd) = frame(3);
    s.submit(fd);
    assert!(s.pull().await.is_some());
}
```

（Step 1 里第一次 submit(f2) 即进入 waiting，测试脚本里重复 submit 行删除——以「submit f1、submit f2、pull 得 f1、complete(7)、pull 得 f2」为准。）

- [ ] **Step 2: 确认失败**（模块不存在）。
- [ ] **Step 3: 实现**：

```rust
//! WS 帧池（spec 2026-09-09）：每路由 队列 + W 个无状态 Worker + Rust 会话表。
//! SOLID：本文件只管「帧的排队/调度/会话表/Worker 池生命周期」；V8 执行细节复用
//! mod.rs 的 WsSession（预载）与 ReqState 捕获链，不在此重复。

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, oneshot, Notify};

use super::{Bridge, WsOutcome, CoreError};

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
        Arc::new(Self { inner: Mutex::new(SchedInner::default()), notify: Notify::new() })
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
        if let Some(q) = g.waiting.get_mut(&conn) {
            if let Some(mut f) = q.pop_front() {
                if g.closed {
                    let _ = f.done.send(Err(FrameError::PoolClosed));
                } else {
                    g.in_flight.insert(conn);
                    g.ready.push_back(f);
                }
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
```

- [ ] **Step 4: 过测**：`cargo test --release -p only-js scheduler` PASS。
- [ ] **Step 5: Commit** `feat(bridge): 帧调度器——per-conn 在飞=1 保序、detach 作废、退役/复活（v0.1.10 §阶段二）`

### Task 4: RoutePool + Worker + sess 外置（内核主体验收）

**Files:**
- Modify: `src/bridge/frame_pool.rs`（Scheduler 之后追加）
- Modify: `src/bridge/mod.rs`（`ReqState` 加 `ws_sess` 字段 + reset；注册 `op_ws_sess_set`；`WsSession` 字段 `pub(crate)`；`ws_connect` 改 `pub(crate)`）
- Test: `src/bridge/frame_pool.rs` tests

**Interfaces:**
- Consumes: `Bridge::ws_connect(&Path) -> Result<WsSession, RunError>`（改 pub(crate)，worker 预载用）；`WsSession { pub(crate) rt, pub(crate) kill }`；`Bridge::read_capture`/`finalize_tx`。
- Produces: `pub struct RoutePool` + `RoutePool::new(file: PathBuf, make: Arc<dyn Fn() -> Bridge + Send + Sync>, timeout: Duration, workers_max: usize, linger_ms: u64) -> Arc<RoutePool>`、`pub fn attach(self: &Arc<Self>, bus_tx: mpsc::UnboundedSender<String>) -> ConnHandle`、`pub fn live_workers(&self) -> usize`、`pub struct ConnHandle { .. }` + `pub async fn fire(&self, ev: &'static str, body: Vec<u8>) -> Result<WsOutcome, FrameError>`、`pub fn detach(&self)`、`pub enum FrameError`、`ReqState.ws_sess: Option<serde_json::Value>`。

- [ ] **Step 1: 失败测试**（frame_pool.rs tests；`ws_session_bridge`/`ws_temp_dir` 辅助从 mod.rs tests 平移为本文件测试辅助——DRY：提为 `#[cfg(test)] pub(crate) fn` 供两处共用）

```rust
use super::super::{Bridge, InMemoryKV, LoaderShared, SchemaRegistry, Extras};
use std::collections::HashMap;

pub(crate) fn ws_test_bridge(root: &std::path::Path) -> Arc<dyn Fn() -> Bridge + Send + Sync> {
    Arc::new(move || {
        Bridge::with_dbs_and_loader(
            HashMap::new(),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            Some(Arc::new(LoaderShared { project_root: root.to_path_buf(), ts: true })),
            Extras::default(),
        )
    })
}

fn pool_dir(tag: &str) -> std::path::PathBuf { /* 同 ws_temp_dir */ }

#[tokio::test(flavor = "current_thread")]
async fn pool_routes_frames_and_externalizes_sess_state() {
    let dir = pool_dir("pool1");
    let ws_file = dir.join("ws.js");
    std::fs::write(&ws_file, r#"
let n = 0;
export default {
  connection() { sess.state.n = 0; json.ok({ hello: 1 }); },
  message() { sess.state.n = (sess.state.n ?? 0) + 1; json.ok({ n: sess.state.n, gid: sess.id }); },
};
"#).unwrap();
    let pool = RoutePool::new(ws_file.clone(), ws_test_bridge(&dir), Duration::from_secs(1), 1, 0);
    let (btx, _brx) = mpsc::unbounded_channel();
    let h = pool.attach(btx);
    let o = h.fire("connection", vec![]).await.unwrap();
    assert!(String::from_utf8_lossy(&o.capture.body).contains("\"hello\":1"));
    for expect in [1, 2] {
        let o = h.fire("message", vec![]).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&o.capture.body).unwrap();
        assert_eq!(v["data"]["n"], expect, "sess.state 跨帧持久（Rust 会话表）");
    }
    // 第二条连接：状态互不可见（同池隔离）
    let (btx2, _brx2) = mpsc::unbounded_channel();
    let h2 = pool.attach(btx2);
    let o = h2.fire("connection", vec![]).await.unwrap();
    let o = h2.fire("message", vec![]).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&o.capture.body).unwrap();
    assert_eq!(v["data"]["n"], 1, "会话表按 conn 隔离");
    h.detach();
    h2.detach();
}

#[tokio::test(flavor = "current_thread")]
async fn pool_poison_radius_is_one_frame_and_pool_respawns() {
    let dir = pool_dir("pool2");
    let ws_file = dir.join("ws.js");
    std::fs::write(&ws_file, r#"
export default {
  message() {
    if (http.body.boom) { const t = Date.now(); while (Date.now() - t < 60_000) {} }
    json.ok({ ok: 1 });
  },
};
"#).unwrap();
    // workers_max=2：毒化补员后仍有 worker 可用
    let pool = RoutePool::new(ws_file, ws_test_bridge(&dir), Duration::from_millis(300), 2, 0);
    let (b1, _) = mpsc::unbounded_channel();
    let (b2, _) = mpsc::unbounded_channel();
    let bad = pool.attach(b1);
    let good = pool.attach(b2);
    let e = bad.fire("message", br#"{"boom":true}"#.to_vec()).await.unwrap_err();
    assert!(matches!(e, FrameError::Timeout));
    // 毒化半径=1：另一连接照常（补员或第二 worker 接帧）
    let o = good.fire("message", br#"{"boom":false}"#.to_vec()).await.unwrap();
    assert!(String::from_utf8_lossy(&o.capture.body).contains("\"ok\":1"));
    bad.detach();
    good.detach();
}
```

（毒化用例跑 V8 terminate + 300ms 看门狗，单测 ~1s，可接受；补员为异步，`good.fire` 前如有需要可 `tokio::time::sleep(50ms)` 等补员——实现里毒化线程直接同步 respawn，无需等待。）

- [ ] **Step 2: 确认失败**（RoutePool 不存在）。
- [ ] **Step 3: 实现**。frame_pool.rs 追加（核心代码，逐字基准；细节借用现有 fire() 实现 mod.rs:936-980）：

```rust
/// Rust 侧会话表条目：sess.state 真身 + 该连接 bus 发送端。
struct SessEntry {
    state: serde_json::Value,
    bus_tx: mpsc::UnboundedSender<String>,
}

enum PoolCtl {
    Respawn,
}

/// 单路由帧池：队列 + W Worker + 会话表。懒启动（首次 attach 起 Worker）。
pub struct RoutePool {
    file: PathBuf,
    timeout: Duration,
    workers_max: usize,
    linger_ms: u64,
    make: Arc<dyn Fn() -> Bridge + Send + Sync>,
    sched: Arc<Scheduler>,
    sessions: Mutex<HashMap<u64, SessEntry>>,
    conn_seq: AtomicU64,
    live: AtomicUsize,
    poisoned: AtomicUsize,
    ctl_tx: mpsc::UnboundedSender<PoolCtl>,
}

impl RoutePool {
    pub fn new(
        file: PathBuf,
        make: Arc<dyn Fn() -> Bridge + Send + Sync>,
        timeout: Duration,
        workers_max: usize,
        linger_ms: u64,
    ) -> Arc<Self> {
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
        let pool = Arc::new(Self {
            file, timeout, workers_max, linger_ms, make,
            sched: Scheduler::new(),
            sessions: Mutex::new(HashMap::new()),
            conn_seq: AtomicU64::new(0),
            live: AtomicUsize::new(0),
            poisoned: AtomicUsize::new(0),
            ctl_tx,
        });
        // 池控泵：毒化补员（独立轻线程，池生命周期与路由同长）。
        std::thread::Builder::new().name("ws-pool-ctl".into()).spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("pool ctl rt");
            rt.block_on(async {
                let mut rx = ctl_rx;
                while let Some(PoolCtl::Respawn) = rx.recv().await {}
            });
        }).expect("spawn ws-pool-ctl");
        // （Respawn 分支在 worker_main 返回 Poisoned 时由 worker 线程直接调用
        //  pool.spawn_worker()，ctl 泵当前仅保留通道形态作退役扩展点；见 Task 5。）
        pool
    }

    /// 注册会话（懒启动 worker）。永远成功；预载失败在 fire 时以 PoolClosed 显形。
    pub fn attach(self: &Arc<Self>, bus_tx: mpsc::UnboundedSender<String>) -> ConnHandle {
        if self.live.load(Ordering::SeqCst) == 0 {
            self.sched.reopen();
        }
        let conn = self.conn_seq.fetch_add(1, Ordering::Relaxed) + 1;
        self.sessions.lock().unwrap().insert(conn, SessEntry { state: serde_json::Value::Null, bus_tx });
        if self.live.load(Ordering::SeqCst) == 0 {
            let workers = self.workers_max.max(1);
            for _ in 0..workers {
                self.spawn_worker();
            }
        }
        ConnHandle { pool: Arc::clone(self), conn }
    }

    pub fn live_workers(&self) -> usize {
        self.live.load(Ordering::SeqCst)
    }

    pub(crate) fn spawn_worker(self: &Arc<Self>) {
        let pool = Arc::clone(self);
        self.live.fetch_add(1, Ordering::SeqCst);
        std::thread::Builder::new().name("ws-worker".into()).spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("ws worker rt");
            let poisoned = rt.block_on(pool.worker_main());
            pool.live.fetch_sub(1, Ordering::SeqCst);
            if poisoned {
                pool.poisoned.fetch_add(1, Ordering::SeqCst);
                // 补员：毒化 Worker 线程退出前直接再起一个（保持池容量）。
                if pool.live.load(Ordering::SeqCst) == 0 && pool.has_sessions() {
                    pool.spawn_worker();
                }
            }
        }).expect("spawn ws-worker");
    }

    fn has_sessions(&self) -> bool {
        !self.sessions.lock().unwrap().is_empty()
    }

    /// Worker 主循环：返回是否毒化退出。
    async fn worker_main(self: Arc<Self>) -> bool {
        let bridge = (self.make)();
        // 预载：模块加载 + 钩子装配（复用 ws_connect；失败 → 本 Worker 不可用，
        // 池在 attach 侧以 PoolClosed 显形，错误已由 ws_connect eprintln）。
        let mut sess = match bridge.ws_connect(&self.file).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("ws worker preload {}: {e}", self.file.display());
                // 排空队列让 fire 立刻拿到 PoolClosed（sched.close 会退役整池；
                // 预载失败 = 该文件永久不可用，退役是正确语义）。
                self.sched.close();
                return false;
            }
        };
        loop {
            let Some(f) = self.sched.pull().await else { return false };
            let state = {
                let mut g = self.sessions.lock().unwrap();
                g.get(&f.conn).map(|e| e.state.clone()).unwrap_or(serde_json::Value::Null)
            };
            let bus_tx = {
                let g = self.sessions.lock().unwrap();
                g.get(&f.conn).map(|e| e.bus_tx.clone())
            };
            let req = super::RequestInfo {
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
                Err(super::RunError::Timeout) => {
                    let _ = f.done.send(Err(FrameError::Timeout));
                    drop(sess); // runtime 已毒化，随 Worker 丢弃
                    return true;
                }
                Err(super::RunError::Core(e)) => {
                    let _ = f.done.send(Err(FrameError::Core(e)));
                }
            }
        }
    }
}

/// 单事件执行（fire() 的无状态化版本）：注入 __sess/__ws_conn → __ws_call →
/// 排空 → 读 ReqState（sends/close/capture/ws_sess）。
pub(crate) async fn ws_event(
    sess: &mut super::WsSession,
    conn: u64,
    ev: &str,
    req: &super::RequestInfo,
    state: &serde_json::Value,
    timeout: Duration,
) -> Result<(WsOutcome, serde_json::Value), super::RunError> {
    {
        let op_state = super::runtime::op_state(&sess.rt);
        let mut g = op_state.borrow_mut();
        g.borrow_mut::<super::ReqState>().reset(req.clone());
    }
    let handle = sess.rt.v8_isolate().thread_safe_handle();
    sess.kill.arm(handle, timeout);
    let ev_lit = serde_json::to_string(ev).unwrap_or_else(|_| "\"\"".into());
    let sess_lit = serde_json::to_string(state).unwrap_or_else(|_| "null".into());
    let code = format!(
        "globalThis.__sess = {sess_lit}; globalThis.__ws_conn = {conn}; globalThis.__ws_call({conn}, {ev_lit});"
    );
    let result = match sess.rt.execute_script("ws_event.js", code) {
        Ok(_) => sess.rt.run_event_loop(deno_core::PollEventLoopOptions::default()).await,
        Err(e) => Err(deno_core::error::CoreError::from(e)),
    };
    if sess.kill.disarm() {
        return Err(super::RunError::Timeout);
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
        self.pool.sched.submit(Frame { conn: self.conn, ev, body, done: tx });
        match rx.await {
            Ok(r) => r,
            Err(_) => Err(FrameError::PoolClosed), // worker 恐慌/drop
        }
    }

    /// 连接收尾：删会话表条目 + 丢排队帧 +（空池 linger 退役在 Task 5 接）。
    pub fn detach(&self) {
        self.pool.sessions.lock().unwrap().remove(&self.conn);
        self.pool.sched.drop_conn(self.conn);
    }
}
```

mod.rs 侧配套（精确改动）：
1. `ReqState` 加字段 `pub ws_sess: Option<serde_json::Value>` + `reset` 加 `self.ws_sess = None;`
2. `src/bridge/ws.rs` 追加 op：
```rust
/// WS 帧收尾：dispatcher finally 把 __sess 快照交还（帧池状态外置回传）。
#[op2]
pub(crate) fn op_ws_sess_set(state: &mut OpState, #[serde] v: serde_json::Value) {
    state.borrow_mut::<ReqState>().ws_sess = Some(v);
}
```
并在 mod.rs `bridge_ext!` ops 列表加 `ws::op_ws_sess_set`。
3. `WsSession` 字段改 `pub(crate) rt`/`pub(crate) kill`；`ws_connect` 改 `pub(crate) async fn`；删除 `WsSession::fire`（其逻辑由 `ws_event` 取代）——同时把 mod.rs tests 里 4 个 `ws_session_*` 用例改写为 frame_pool 的 RoutePool 等价用例（`ws_session_lifecycle_hooks` → `pool_routes_frames_and_externalizes_sess_state` 已覆盖；`ws_session_rejects_missing_hooks`/`ws_session_error_hook_catches_frame_exception`/`ws_session_uncaught_frame_error_keeps_session_usable` 平移为 pool 版，断言不变：无钩子 → fire 得 PoolClosed；error 钩子收异常 → sends 带出；无 error 重抛 → fire 得 Err(Core) 后下一帧照常）。
4. dispatcher（`ws_connect` 的 driver 字符串）升级为 v2（conn/sess 注入 + finally 交还）：

```rust
        let code = format!(
            "import {{ op_ws_sess_set }} from \"ext:core/ops\";\n\
             const h = (await import(\"{spec}\")).default ?? {{}};\n\
             const fns = {{}};\n\
             for (const k of [\"connection\", \"message\", \"close\", \"error\"])\n\
               fns[k] = typeof h[k] === \"function\" ? h[k] : null;\n\
             globalThis.__ws_hooks = fns;\n\
             globalThis.__sess = null;\n\
             globalThis.sess = {{\n\
               get id() {{ return globalThis.__ws_conn; }},\n\
               get state() {{ return globalThis.__sess; }},\n\
               set state(v) {{ globalThis.__sess = v; }},\n\
             }};\n\
             globalThis.__ws_call = async (conn, ev) => {{\n\
               const fn = globalThis.__ws_hooks[ev];\n\
               if (!fn) return;\n\
               try {{ await fn(); }} catch (e) {{\n\
                 const onErr = globalThis.__ws_hooks.error;\n\
                 if (onErr && ev !== \"error\") await onErr(e); else throw e;\n\
               }} finally {{\n\
                 try {{ op_ws_sess_set(globalThis.__sess === undefined ? null : globalThis.__sess); }} catch ({{}})\n\
               }}\n\
             }};\n\
             json.ok({{ connection: !!fns.connection, message: !!fns.message,\n\
                        close: !!fns.close, error: !!fns.error }});\n"
        );
```

- [ ] **Step 4: 过测 + 全量**：`cargo test --release -p only-js pool_ ws_session scheduler` 全 PASS；全量 `cargo test --release -p only-js`（存量 ws_session 用例已平移，不残留编译错误）。
- [ ] **Step 5: fmt + clippy + Commit** `feat(bridge): 帧池内核——RoutePool/Worker/sess 外置 + op_ws_sess_set（v0.1.10 §阶段二）`

### Task 5: 池生命周期（退役 linger + 预载失败锁存）

**Files:**
- Modify: `src/bridge/frame_pool.rs`（`detach` 触发退役计时；预载失败锁存 `failed: Mutex<Option<String>>`）
- Test: 同文件 tests

**Interfaces:**
- Produces: `RoutePool.attach` 在池已锁存失败时直接返回的 ConnHandle fire 恒 `PoolClosed`（不重复起 Worker）；`detach` 后 `sessions.is_empty()` → 起 linger 计时（linger_ms=0 立即）→ `sched.close()`；`attach` 时若已 retired（live==0）→ `reopen`（Task 4 已做）。

- [ ] **Step 1: 失败测试**

```rust
#[tokio::test(flavor = "current_thread")]
async fn pool_retires_when_idle_after_linger() {
    let dir = pool_dir("retire");
    let ws_file = dir.join("ws.js");
    std::fs::write(&ws_file, r#"export default { message() { json.ok({ ok: 1 }); } };"#).unwrap();
    let pool = RoutePool::new(ws_file, ws_test_bridge(&dir), Duration::from_secs(1), 1, 0);
    let (btx, _) = mpsc::unbounded_channel();
    let h = pool.attach(btx);
    h.fire("message", vec![]).await.unwrap();
    assert_eq!(pool.live_workers(), 1);
    h.detach();
    // linger=0：detach 后 Worker 异步退出（等一小会儿）
    for _ in 0..50 {
        if pool.live_workers() == 0 { break; }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(pool.live_workers(), 0, "空池立即退役");
    // 新连接复活
    let (btx2, _) = mpsc::unbounded_channel();
    let h2 = pool.attach(btx2);
    h2.fire("message", vec![]).await.unwrap();
    h2.detach();
}

#[tokio::test(flavor = "current_thread")]
async fn pool_preload_failure_latches_closed() {
    let dir = pool_dir("preload-fail");
    let ws_file = dir.join("ws.js");
    std::fs::write(&ws_file, "json.ok({});\n").unwrap(); // 无钩子 → 预载失败
    let pool = RoutePool::new(ws_file, ws_test_bridge(&dir), Duration::from_secs(1), 1, 0);
    let (btx, _) = mpsc::unbounded_channel();
    let h = pool.attach(btx);
    assert!(matches!(h.fire("message", vec![]).await, Err(FrameError::PoolClosed)));
    // 锁存：后续 attach 不再反复起 worker
    let (btx2, _) = mpsc::unbounded_channel();
    let h2 = pool.attach(btx2);
    assert!(matches!(h2.fire("message", vec![]).await, Err(FrameError::PoolClosed)));
    assert_eq!(pool.live_workers(), 0);
}
```

- [ ] **Step 2: 确认失败**（detach 不触发退役；预载失败反复 spawn）。
- [ ] **Step 3: 实现**：
  - `RoutePool` 加 `failed: Mutex<Option<String>>`；`attach` 开头：`if self.failed.lock().unwrap().is_some() { return ConnHandle{..} }`（fire 走 sched→closed→PoolClosed；首次预载失败路径里 `*failed = Some(e)` 与 `sched.close()` 一起）。
  - `detach` 末尾：`if self.sessions.lock().unwrap().is_empty() { let pool = Arc::clone(self); std::thread::Builder::new().name("ws-retire".into()).spawn(move { std::thread::sleep(Duration::from_millis(pool.linger_ms)); if pool.sessions.lock().unwrap().is_empty() { pool.sched.close(); } }).expect("spawn ws-retire"); }`
  - 注意：linger=0 也走线程（sleep(0) 立即），复用同一路径（DRY）。
- [ ] **Step 4: 过测 + 全量 + clippy**。
- [ ] **Step 5: Commit** `feat(bridge): 池生命周期——空池 linger 退役、预载失败锁存（v0.1.10 §阶段二）`

**阶段二小结（完成后输出）**：帧池内核三件套（调度/Worker/生命周期）+ sess 外置落地，bridge 用例 N 过；spec 的「done-op 简化」按计划执行（仅 op_ws_sess_set + ReqState.ws_sess）。

---

# 阶段三：server 接线（frame_loop 重构 + 装配）

### Task 6: frame_loop 池化 + js_route/mirror_routes/app 装配 + 存量测试适配

**Files:**
- Modify: `server/src/ws.rs`（`conn_on_pinned` 删除；`frame_loop` 重写为 Send async；`js_route`/`mirror_routes` 签名改池；新增 `WsOptions`）
- Modify: `oj/src/app.rs:598`（装配点传 `WsOptions`）
- Test: `server/src/ws.rs` tests（存量适配 + 新钉）

**Interfaces:**
- Consumes: `RoutePool::new/attach`、`ConnHandle::fire/detach`、`FrameError`（阶段二逐字签名）。
- Produces: `pub struct WsOptions { pub max_connections: u64, pub workers_per_route: usize, pub idle_linger_ms: u64 }`；`js_route(path: &str, timeout: Duration, pool: Arc<RoutePool>, opts: &WsOptions) -> Router`；`mirror_routes(base, root, timeout, make_bridge, opts: &WsOptions) -> Router`（每 ws 文件一个 `RoutePool::new`）。

- [ ] **Step 1: 存量测试适配 + 新钉（先写）**
  - 所有 `js_route(path, file, timeout, make_bridge)` 调用改为：`let pool = RoutePool::new(file, Arc::new(make_bridge...), timeout, 2, 0); js_route(path, timeout, pool, &WsOptions::default())`——新增测试辅助 `fn test_pool(file, timeout)` 封装（DRY）。`WsOptions::default()` = {1000, 2, 0}。
  - 新钉 1（毒化半径跨连接）：

```rust
/// 帧池毒化半径=1：连接 A 死循环帧超时断连，连接 B 照常收发（他池 worker 补员）。
#[tokio::test]
async fn frame_pool_timeout_isolates_connections() {
    // ws.js: message(){ if (http.body.boom) { 死循环 60s } json.ok({ok:1}) }
    // timeout=300ms；A fire boom → A 读到 Close/EOF；B fire 正常帧 → 信封
    // 断言 A 的读符合 missing-file 用例的 clean 判定（0x88/EOF/RST 族）
}
```

  - 新钉 2（同连接保序）：handler `message(){ await new Promise(r=>setTimeout(r,30)); json.ok({n: sess.state.n = (sess.state.n??0)+1}); }`——同连接连发 3 帧，回帧序 = 1,2,3。
  - 新钉 3（闸门 e2e 已在 Task 2）。
- [ ] **Step 2: 确认失败**（签名不匹配编译失败）。
- [ ] **Step 3: 实现**：

```rust
/// WS 装配选项（来自 config ws 段；server 不依赖 config crate）。
#[derive(Clone, Copy)]
pub struct WsOptions {
    pub max_connections: u64,
    pub workers_per_route: usize,
    pub idle_linger_ms: u64,
}

impl Default for WsOptions {
    fn default() -> Self { Self { max_connections: 1000, workers_per_route: 2, idle_linger_ms: 0 } }
}
```

`js_route(path, timeout, pool: Arc<RoutePool>, opts: WsOptions)`：upgrade 闭包内闸门（Task 2 的 `gate_enter(opts.max_connections)`）→ on_upgrade → `frame_loop(socket, pool, timeout).await` → `WS_LIVE.fetch_sub`。

`frame_loop` 重写（删 `conn_on_pinned` 与其线程；全 Send 跑在 axum runtime 上；Reader/Writer/forwarder 三任务原样保留）：

```rust
async fn frame_loop(socket: WebSocket, pool: Arc<RoutePool>, timeout: Duration) {
    let (msg_tx, mut msg_rx) = mpsc::channel::<Vec<u8>>(64);
    let (resp_tx, mut resp_rx) = mpsc::channel::<String>(64);
    let (bus_tx, mut bus_rx) = mpsc::unbounded_channel::<String>();
    let (mut sink, mut stream) = socket.split();
    // Reader / Writer / Bus forwarder：与 v0.1.9 逐字相同（略——实现时照抄现文件 180-213 行）。
    let handle = pool.attach(bus_tx.clone());
    let mk = |n: u8| n; // 占位：无
    let mut alive = true;
    match handle.fire("connection", Vec::new()).await {
        Ok(o) => emit(&resp_tx, o),
        Err(FrameError::Timeout) => alive = false,
        Err(FrameError::PoolClosed) => { /* 预载失败（文件缺失/无钩子）→ 干净断连 */
            let _ = sink.send(Message::Close(None)).await;
            return;
        }
        Err(e) => eprintln!("ws connection: {e}"),
    }
    while alive {
        let Some(msg) = msg_rx.recv().await else { break };
        match handle.fire("message", msg).await {
            Ok(o) => { let closing = o.close; emit(&resp_tx, o); if closing { break; } }
            Err(FrameError::Timeout) => { eprintln!("ws frame timeout"); alive = false; break; }
            Err(FrameError::PoolClosed) => { alive = false; break; }
            Err(e) => eprintln!("ws frame error: {e}"),
        }
    }
    if alive {
        match handle.fire("close", Vec::new()).await {
            Ok(o) => emit(&resp_tx, o),
            Err(e) => eprintln!("ws close: {e}"),
        }
    }
    handle.detach();
    drop(resp_tx);
    forwarder.abort();
    let _ = writer.await;
}
```

（注意：PoolClosed 断连路径要先 `handle.detach()` 再 return——落码时把 detach 放到所有退出路径汇合处，参考现文件的 `alive` 收尾结构，不要中途 return 漏 detach。）

`mirror_routes(base, root, timeout, make_bridge, opts)`：每个 ws 文件 `RoutePool::new(file, make.clone(), timeout, opts.workers_per_route, opts.idle_linger_ms)` → `js_route(&path, timeout, pool, opts)`。

`oj/src/app.rs:598` 改：

```rust
        let ws_opts = server::ws::WsOptions {
            max_connections: cfg.ws.max_connections,
            workers_per_route: cfg.ws.workers_per_route,
            idle_linger_ms: cfg.ws.idle_linger_ms,
        };
        let ws_router = ws::mirror_routes(&base, &dir, timeout.unwrap_or(Duration::from_secs(30)), make_bridge, ws_opts);
```

- [ ] **Step 4: 过测**：`cargo test --release -p server` 全 PASS（毒化半径钉含 300ms 看门狗，整轮 +~1s 可接受）；`cargo test --release --workspace`；fmt + clippy。
- [ ] **Step 5: Commit** `feat(server): frame_loop 池化——每连接线程撤销、毒化半径=1、池装配（v0.1.10 §阶段三）`

**阶段三小结（完成后输出）**：server 接线完成；存量语义用例全绿 + 新钉（毒化隔离/保序/闸门）全绿。

---

# 阶段四：样例/文档/发布 v0.1.10

### Task 7: sample 迁移 + dist 重建

**Files:**
- Modify: `sample/src/news/ws.ts`、`sample/src/news/chat/ws.ts`（注释与 sess 演示；news 版本 bump 0.1.2——manifest 语义化：契约行为变化）
- Modify: `sample/dist/`（oj build 重建；news-0.1.2 替换 0.1.1，git rm 旧目录）

**Interfaces:** 无（纯 JS/文档）。

- [ ] **Step 1: `sample/src/news/ws.ts`**（全文替换；版本 demo sess.state）

```ts
// WS 帧池契约（目录镜像路由 /v1/api/news/ws）：
// export default { connection, message, close, error }——钩子语义与 v0.1.9 一致。
// v0.1.10 起执行模型为「帧池」：路由级 W 个无状态 Worker 共享执行，连接状态放
// sess.state（Rust 会话表持久，可 JSON 序列化）；模块作用域 = Worker 本地只读缓存。
export default {
  connection() {
    sess.state.ready = true; // 演示：会话状态外置（跨帧持久、按连接隔离）
    bus.subscribe("news");
    json.ok({ subscribed: true });
  },
};
```

- [ ] **Step 2: `sample/src/news/chat/ws.ts`**（全文替换；进房昵称演示 sess.state）

```ts
// WS 聊天室（目录镜像路由 /v1/api/news/chat/ws）。
// connection = 进房（订阅 "chat"，每连接一次）；message = 每帧广播
// {"from":"neo","text":"hi"} 给所有订阅连接（含自己——自回声）。
// 帧池模型：sess.state 存会话态（可序列化）；模块作用域只放只读缓存。
export default {
  connection() {
    sess.state.joined = true;
    bus.subscribe("chat");
    json.ok({ joined: true });
  },
  message() {
    const frame = http.body;
    if (frame && frame.text) {
      bus.publish("chat", { from: frame.from ?? "anon", text: frame.text });
      json.ok({ sent: true });
    }
  },
};
```

- [ ] **Step 3: `sample/src/news/manifest.yaml` version → "0.1.2"**；`cargo run -q -p oj -- build -d sample/src -o sample/dist`；`git rm -r sample/dist/news-0.1.1 sample/dist/news-0.1.1.tgz`（如存在）；`git add -f sample/dist/news-0.1.2 sample/dist/news-0.1.2.tgz sample/dist/manifests.yaml`。
- [ ] **Step 4: 冒烟**：`timeout 15 cargo run -p oj -- server -c sample/config.yaml --api-path sample/src`——监听正常、无池装配报错。
- [ ] **Step 5: Commit** `feat(sample): news 迁移 sess 演示 + 版本 0.1.2，dist 重建（v0.1.10 §阶段四）`

### Task 8: 文档清扫 + CHANGELIST + v0.1.10 发布点

**Files:**
- Modify: `docs/devkit/api-manual.md`（§ws.ts：帧池模型、sess API 与可序列化约束、模块作用域新语义、配置面、超时契约回归每连接）
- Modify: `docs/websocket.md`（§1 心智模型 → 帧池；§4 图 Processor → Worker 池；毒化半径说明）
- Modify: `docs/dev-guide.md`（ws 装配/执行族描述）
- Modify: `docs/user-manual.md`、`sample/MODULES.md`、`docs/devkit/SKILL.md`（WS 相关行同步帧池口径）
- Modify: `CHANGELIST.md`（v0.1.10 段）、`oj/Cargo.toml`（0.1.9 → 0.1.10）

**Interfaces:** 无。

- [ ] **Step 1: api-manual §ws.ts 重写**（关键段落文本）：

```markdown
**执行模型（v0.1.10 帧池）**：每路由 W 个无状态 Worker（`ws.workers_per_route`，默认 2）
共享执行该路由所有连接的帧。连接状态放 `sess.state`（Rust 会话表持久，按连接隔离，
**必须可 JSON 序列化**——函数等不可序列化值静默丢失）；`sess.id` 为连接 id。
模块作用域 = Worker 本地只读缓存（W 份副本）：可变跨帧状态禁止放模块作用域，
路由级可变状态用 kv/bus。

帧超时断开**该连接**（毒化只影响执行帧的 Worker，池自动补员，其它连接无感）。
`ws.max_connections`（默认 1000，0=不限）超限 upgrade 返 503。
```

- [ ] **Step 2: websocket.md**：§1 标题「帧池运行时」；执行单元表追加「Worker 池（无状态，W/路由）」行；§4 图 Processor 行改「W × Worker（帧队列拉取）」；新增小节「sess 会话状态与帧池约束」（可序列化 / 模块作用域只读 / 毒化半径=1 / 闸门）。
- [ ] **Step 3: dev-guide / user-manual / MODULES / SKILL**：凡「每连接独占 VM/驻留会话」表述改帧池口径（grep 确认无残留：`grep -rn "独占\|驻留会话" docs/ sample/MODULES.md`）。
- [ ] **Step 4: CHANGELIST v0.1.10 段**（插在 v0.1.9 前）：

```markdown
## v0.1.10（2026-09-09）

**特性（breaking）**
- WS 执行模型改「帧池」：每路由 W 个无状态 Worker（`ws.workers_per_route`，默认 2）从帧
  队列拉帧执行——内存与连接数解耦（会话态 ≈2KB/连接 vs 独占 6.2MB），帧超时毒化半径
  回归单连接。连接状态新增 `sess.state`（Rust 会话表持久，可 JSON 序列化）与 `sess.id`；
  「模块作用域 = 连接状态」写法废弃（现为 Worker 本地只读缓存）。
- 新增连接闸门 `ws.max_connections`（默认 1000，0=不限）：超限 upgrade 返 503。

**实现**
- bridge 新增 frame_pool（Scheduler per-conn 在飞=1 保序 / Worker 池 / 会话表 / 空池 linger
  退役）；`op_ws_sess_set` + `ReqState.ws_sess` 回传会话态；每连接 ws-js 线程撤销。
```

- [ ] **Step 5: `oj/Cargo.toml` version = "0.1.10"**；`cargo build --release --workspace` + `cargo test --release -p oj`（e2e）全绿。
- [ ] **Step 6: Commit A（docs）**：`docs: WS 帧池模型手册化——api-manual/websocket/dev-guide/user-manual/MODULES/SKILL（v0.1.10）`
- [ ] **Step 7: Commit B（发布点）**：`chore(release): v0.1.10——WS 帧池运行时 + 连接闸门`

---

## Self-Review 记录

- **Spec 覆盖**：闸门→T1/T2；帧池内核→T3/T4；生命周期→T5；接线→T6；sample→T7；docs/发布→T8；毒化半径/保序/state 外置/退役/锁存钉→T4/T5/T6；验收基准（内存平坦）为运维级指标，落为 T6 毒化隔离 + T4 state 钉的组合验证 + 终审复核。两处 spec 澄清（done-op 简化、懒启动）已写入计划头，spec 文本随后续 commit 同步（见 Task 8 附带：spec「ops 层仅新增此一个 op」与「纯登记」措辞修正）。
- **占位符**：T6 新钉 1/2 给了行为描述与断言要点（实现者按相邻用例拼装惯例落地）；其余任务代码逐字。
- **类型一致性**：`FrameError`/`Frame`/`Scheduler`/`RoutePool::new(file, make, timeout, workers_max, linger_ms)`/`attach(bus_tx)`/`fire(ev, body)`/`detach`/`WsOptions` 在 T3-T6 间一致；`ReqState.ws_sess`/`op_ws_sess_set` 与 dispatcher v2 对齐。
