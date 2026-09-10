//! WebSocket 层（P5a echo + P5b JS 帧循环，v0.1.10 帧池化）。
//!
//! Go 模式：WS 路由注册在 catch-all 之前；Reader/Processor/Writer 三任务流水线，
//! msgChan/respChan 各 cap 64（背压保护）。JS 帧经 RoutePool 调度（per-conn 串行
//! 保序、W 个无状态 worker、毒化半径 = 1）——`JsRuntime` !Send 的约束沉到池的
//! worker 线程（current_thread runtime），连接侧 frame_loop 全 Send 跑在 axum
//! runtime 上，不再每连接起专用线程。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::extract::ws::{Message, WebSocket};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;

use only_js::bridge::frame_pool::{FrameError, RoutePool};
use only_js::bridge::{Bridge, WsOutcome};

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

/// 闸门计数守卫：连接无论正常收尾、panic 还是被取消（任务被 drop，如测试
/// runtime 关停），结束时都递减 WS_LIVE——与 gate_enter 1:1 配对，不漏减。
/// 以 Arc 双持：upgrade 失败（on_failed_upgrade）与连接结束（on_upgrade 闭包
/// drop）两条路径共享一份计数，任一路径终结时 GateGuard 恰好 drop 一次。
struct GateGuard;
impl Drop for GateGuard {
    fn drop(&mut self) {
        WS_LIVE.fetch_sub(1, Ordering::Relaxed);
    }
}

/// WS 装配选项（来自 config ws 段；server 不依赖 config crate）。
#[derive(Clone, Copy)]
pub struct WsOptions {
    /// 全局并发连接上限：超限 upgrade 直接 503；0 = 不限。
    pub max_connections: u64,
    /// 每路由 Worker 数（无状态，可小于并发连接数）。
    pub workers_per_route: usize,
    /// 路由连接归零后 Worker 池保活毫秒数（0 = 立即退役）。
    pub idle_linger_ms: u64,
}

impl Default for WsOptions {
    fn default() -> Self {
        Self {
            max_connections: 1000,
            workers_per_route: 2,
            idle_linger_ms: 0,
        }
    }
}

/// 挂载最小 echo 路由（GET /ws）——P5a 链路验证用。
pub fn echo_route() -> axum::Router {
    axum::Router::new().route("/ws", axum::routing::get(upgrade))
}

/// 挂载 JS handler 驻留会话路由：connection/message/close 逐帧经 pool 调度
/// （per-conn 串行保序），json.ok 信封与 ws.send 逐事件写回；单事件熔断超时
/// 由池内看门狗承担（RoutePool 构造时注入，超时必断连）；
/// opts.max_connections 为全局并发连接上限（0 = 不限），超限 upgrade 直接 503。
pub fn js_route(path: &str, pool: Arc<RoutePool>, opts: WsOptions) -> axum::Router {
    axum::Router::new().route(
        path,
        axum::routing::get(move |ws: axum::extract::WebSocketUpgrade| {
            let pool = pool.clone();
            async move {
                if !gate_enter(opts.max_connections) {
                    return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
                }
                // 槽位归还两条路径共享一份 Arc<GateGuard>：upgrade 半路失败
                // （客户端握手后即 RST，axum 走 on_failed_upgrade、永不 spawn
                // on_upgrade 回调）与连接真实结束各持一个克隆，GateGuard 恰 drop
                // 一次——缺了 failed 路径，反复 connect+RST 可永久打满闸门（DoS）。
                let gate = Arc::new(GateGuard);
                let failed_gate = Arc::clone(&gate);
                ws.on_failed_upgrade(move |_err| drop(failed_gate))
                    .on_upgrade(move |socket| async move {
                        // 连接真实结束（正常收尾/panic/任务取消）才递减闸门计数。
                        let _gate = gate;
                        frame_loop(socket, pool).await;
                    })
            }
        }),
    )
}

/// 生产目录镜像 WS 挂载（oj server）：<root>/<dir>/ws.ts（优先）/ws.js → GET {base}/<dir>/ws；
/// 根级 ws.ts → {base}/ws；无 WS 文件返回空 Router（merge 无副作用）。
/// release 下 root=dist，URL 含模块版本段（news-0.1.0/ws）——v0.2 已知限制，见 user-manual。
pub fn mirror_routes(
    base: &str,
    root: &Path,
    timeout: std::time::Duration,
    make_bridge: impl Fn() -> Bridge + Send + Sync + 'static,
    opts: WsOptions,
) -> axum::Router {
    let make = Arc::new(make_bridge);
    let base = format!("/{}/", base.trim_matches('/'));
    let mut seen = std::collections::HashSet::new();
    let mut router = axum::Router::new();
    for file in ws_files(root) {
        let rel = file
            .parent()
            .and_then(|p| p.strip_prefix(root).ok())
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        // rel 为空 = 根级 ws.ts → {base}/ws（不得拼成 {base}//ws 双斜杠）。
        let path = if rel.is_empty() {
            format!("{base}ws")
        } else {
            format!("{base}{rel}/ws")
        };
        if !seen.insert(path.clone()) {
            continue; // 同目录 ws.ts 与 ws.js 并存：先到者（.ts）胜
        }
        // 每路由一池：worker 数与空池保活来自 ws 配置段。
        let pool = RoutePool::new(
            file,
            make.clone(),
            timeout,
            opts.workers_per_route,
            opts.idle_linger_ms,
        );
        router = router.merge(js_route(&path, pool, opts));
    }
    router
}

/// root 下全部 WS 处理器：ws.ts 全部在前（优先），ws.js 在后；各自排序保证注册序确定。
fn ws_files(root: &Path) -> Vec<PathBuf> {
    let mut ts = Vec::new();
    crate::routes::walk_files(root, "ws.ts", &mut ts);
    let mut js = Vec::new();
    crate::routes::walk_files(root, "ws.js", &mut js);
    ts.sort();
    js.sort();
    ts.extend(js);
    ts
}

/// upgrade 后的连接处理：整个连接搬到专用 OS 线程（current_thread runtime）。
async fn echo_on_pinned(socket: WebSocket) {
    std::thread::Builder::new()
        .name("ws-conn".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("ws conn runtime init");
            rt.block_on(async move {
                let mut socket = socket;
                while let Some(Ok(msg)) = socket.recv().await {
                    if matches!(msg, Message::Close(_)) {
                        break;
                    }
                    // 仅回显文本/二进制帧（ping/pong 由 axum 自动处理）。
                    if socket.send(msg).await.is_err() {
                        break;
                    }
                }
            });
        })
        .expect("spawn ws-conn thread");
}

/// axum upgrade handler（echo 路由用，不占用 axum 线程做任何帧处理）。
async fn upgrade(ws: axum::extract::WebSocketUpgrade) -> Response {
    ws.on_upgrade(echo_on_pinned)
}

/// 事件结果写出：ws.send 集合先于信封帧（顺序契约，原 run_ws 消费端逐行等价）。
fn emit(resp_tx: &mpsc::Sender<String>, o: WsOutcome) {
    for s in o.sends {
        let _ = resp_tx.try_send(s); // 满则丢弃
    }
    if !o.capture.body.is_empty() {
        let _ = resp_tx.try_send(String::from_utf8_lossy(&o.capture.body).into_owned());
    }
}

/// 帧池连接循环（全 Send，跑在 axum runtime 上）：
/// Reader(stream→msgChan) / Writer(respChan→sink) / Bus forwarder 三任务与 v0.1.9
/// 逐字相同；事件循环把 connection/message/close 逐帧投给 pool（per-conn 串行、
/// 会话状态由池的 Rust 会话表持有，worker 无状态可换人；单帧超时熔断在池内
/// 看门狗，本函数不再持有 timeout）。语义：
/// Timeout = 必断连（V8 已 terminate，worker 弃会话）；Core = 丢帧继续；
/// PoolClosed = 干净断连；客户端 Close/socket 断 = 正常收尾。
/// 所有退出路径都先 detach（删会话条目 + 丢排队帧 + 空池退役计时）再收尾写出。
async fn frame_loop(socket: WebSocket, pool: Arc<RoutePool>) {
    let (msg_tx, mut msg_rx) = mpsc::channel::<Vec<u8>>(64);
    let (resp_tx, mut resp_rx) = mpsc::channel::<String>(64);
    // bus 会话端：发送端经 attach 存入池会话表（worker publish 用）；收到的广播帧转写回 socket。
    let (bus_tx, mut bus_rx) = mpsc::unbounded_channel::<String>();

    let (mut sink, mut stream) = socket.split();

    // Reader：读帧 → msgChan（满则背压至 TCP 层）。
    tokio::spawn(async move {
        while let Some(Ok(msg)) = stream.next().await {
            let bytes = match msg {
                Message::Text(t) => t.as_bytes().to_vec(),
                Message::Binary(b) => b.to_vec(),
                Message::Close(_) => break,
                _ => continue, // ping/pong 自动处理
            };
            if msg_tx.send(bytes).await.is_err() {
                break;
            }
        }
    });

    // Bus forwarder：订阅的广播帧 → 同一写出通道（与 ws.send 天然保序）。
    // 连接结束由 frame_loop abort 收尾——bus_tx 会滞留 Bus 表，不 abort 则 Writer 永不排空。
    let forwarder = tokio::spawn({
        let resp_tx = resp_tx.clone();
        async move {
            while let Some(frame) = bus_rx.recv().await {
                let _ = resp_tx.try_send(frame);
            }
        }
    });

    // 预载失败（文件缺失/无钩子）已锁存 → attach 后 fire 恒 PoolClosed。
    let handle = pool.attach(bus_tx);
    // 超时毒化后该连接的会话已随 worker 丢弃：跳过后续一切 fire（含 close）。
    let mut alive = true;
    match handle.fire("connection", Vec::new()).await {
        Ok(o) => emit(&resp_tx, o),
        Err(FrameError::Timeout) => alive = false,
        // 预载失败 → 干净断连（先发 Close 帧再丢弃，避免未读数据触发 TCP RST）。
        Err(FrameError::PoolClosed) => {
            let _ = sink.send(Message::Close(None)).await;
            handle.detach();
            return;
        }
        Err(e) => eprintln!("ws connection: {e}"),
    }

    // Writer：respChan → 串行写回；通道排空（连接结束）后发 Close 帧干净关闭。
    // （置后启动：PoolClosed 断连路径要在 sink 被 writer 接走前直发 Close。）
    let writer = tokio::spawn(async move {
        while let Some(text) = resp_rx.recv().await {
            if sink.send(Message::Text(text.into())).await.is_err() {
                return;
            }
        }
        let _ = sink.send(Message::Close(None)).await;
    });

    while alive {
        let Some(msg) = msg_rx.recv().await else {
            break; // 客户端 Close / socket 断
        };
        match handle.fire("message", msg).await {
            Ok(o) => {
                let closing = o.close;
                emit(&resp_tx, o);
                if closing {
                    break;
                }
            }
            // 帧超时 = 必断连（V8 已 terminate，worker 已弃会话）。
            Err(FrameError::Timeout) => {
                eprintln!("ws frame timeout");
                alive = false;
                break;
            }
            // 池退役/预载失败锁存 → 干净断连。
            Err(FrameError::PoolClosed) => {
                alive = false;
                break;
            }
            // error() 缺失或自身抛：丢帧继续（与「error 后连接继续」决策一致）。
            Err(e) => eprintln!("ws frame error: {e}"),
        }
    }
    if alive {
        // close 恰好一次，尽力而为：钩子可 ws.send 离帧，失败只记日志。
        match handle.fire("close", Vec::new()).await {
            Ok(o) => emit(&resp_tx, o),
            Err(e) => eprintln!("ws close: {e}"),
        }
    }
    handle.detach(); // 所有退出路径汇合：删会话条目 + 丢排队帧 + 空池退役计时
    drop(resp_tx); // Writer 排空后自然退出
    forwarder.abort(); // 释放 bus_rx 与 resp_tx 克隆，Writer 才能排空退出
    let _ = writer.await;
}

#[cfg(test)]
// ws_e2e_lock 故意持 std MutexGuard 跨 await：串行化 e2e 用例以隔离进程级
// WS_LIVE；单线程 runtime 各占一线程，无跨 await 死锁（见 ws_e2e_lock 注释）。
#[allow(clippy::await_holding_lock)]
mod tests {
    use super::*;
    use crate::app;
    use only_js::bridge::InMemoryKV;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// 裸 TCP WebSocket 客户端：upgrade → 掩码文本帧 → 读回帧。
    struct WsClient(tokio::net::TcpStream);

    impl WsClient {
        async fn connect(addr: std::net::SocketAddr, path: &str) -> Self {
            let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
            s.write_all(
                format!(
                    "GET {path} HTTP/1.1\r\nHost: t\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
                     Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
            let mut buf = vec![0u8; 4096];
            let n = s.read(&mut buf).await.unwrap();
            let head = String::from_utf8_lossy(&buf[..n]).into_owned();
            assert!(head.starts_with("HTTP/1.1 101"), "upgrade failed: {head}");
            Self(s)
        }

        /// 客户端帧必须掩码：FIN+text, MASK|len, 4 字节 mask, XOR payload。
        async fn send_text(&mut self, payload: &str) {
            let mask = [0x37u8, 0xfa, 0x21, 0x3d];
            let bytes = payload.as_bytes();
            let mut frame = vec![0x81, 0x80 | bytes.len() as u8];
            frame.extend_from_slice(&mask);
            frame.extend(bytes.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
            self.0.write_all(&frame).await.unwrap();
        }

        /// 读一个服务端帧（不掩码；小 payload 单字节长度足够本测试）。
        async fn read_text(&mut self) -> String {
            let mut hdr = [0u8; 2];
            self.0.read_exact(&mut hdr).await.unwrap();
            assert_eq!(hdr[0] & 0x0f, 0x01, "not a text frame: {:x?}", hdr);
            let len = (hdr[1] & 0x7f) as usize;
            let mut payload = vec![0u8; len];
            self.0.read_exact(&mut payload).await.unwrap();
            String::from_utf8(payload).unwrap()
        }
    }

    async fn spawn(router: axum::Router) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        addr
    }

    /// WS_LIVE 是进程级计数：ws e2e 用例并行跑时，彼些的在途连接会抬高读数
    /// （gate 用例对「第 1 条连接须成功」最敏感）。凡起 WS 连接的用例先持此锁
    /// 串行化，隔离进程级状态（单线程 runtime 各占一线程，无跨 await 死锁）。
    fn ws_e2e_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 等 WS_LIVE 归零：上个用例的 serve 任务在其 runtime 关停时才归还槽位，可能
    /// 滞后于锁释放落入本用例；straggler 只会递减，归零后持锁期间即稳定在 0。
    /// 闸门用例（max_connections=1）对存量槽位敏感，归零是硬前提。
    async fn ws_live_zero() {
        for _ in 0..200 {
            if WS_LIVE.load(Ordering::Relaxed) == 0 {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!(
            "WS_LIVE did not drain to 0 in 2s: {}",
            WS_LIVE.load(Ordering::Relaxed)
        );
    }

    /// ws_connect 走 ESM import：bridge 必须带模块加载器（project_root 指向临时目录）。
    fn make_bridge(root: std::path::PathBuf) -> Bridge {
        use only_js::bridge::{Extras, LoaderShared, SchemaRegistry};
        use std::collections::HashMap;
        Bridge::with_dbs_and_loader(
            HashMap::new(),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            Some(Arc::new(LoaderShared {
                project_root: root,
                ts: true,
            })),
            Extras::default(),
        )
    }

    /// 测试用路由池：make_bridge 工厂（project_root = handler 所在目录）+
    /// WsOptions::default 的 worker/linger。文件缺失用例传不存在的路径同样适用。
    fn test_pool(file: &Path, timeout: std::time::Duration) -> Arc<RoutePool> {
        let root = file.parent().unwrap_or(Path::new(".")).to_path_buf();
        RoutePool::new(
            file.to_path_buf(),
            Arc::new(move || make_bridge(root.clone())),
            timeout,
            WsOptions::default().workers_per_route,
            WsOptions::default().idle_linger_ms,
        )
    }

    async fn raw_http(addr: std::net::SocketAddr, req: &str) -> String {
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(req.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).await.unwrap();
        String::from_utf8_lossy(&buf).into_owned()
    }

    /// bus 跨会话广播：WS 帧订阅 → HTTP 打 api 路由 publish → WS 客户端收 JSON 帧。
    /// actor 与 ws make_bridge 共享同一 Bus（Extras.bus）——publish 广播到订阅的 WS 连接。
    #[tokio::test]
    async fn ws_bus_subscribe_receives_http_publish() {
        let _e2e = ws_e2e_lock();
        use crate::actor::JsActor;
        use only_js::bridge::{Bus, Extras, LoaderShared, SchemaRegistry};
        use std::collections::HashMap;
        let t = crate::tests::routes(&[(
            "pub/api.ts",
            "function post() { bus.publish(\"news\", { a: 1 }); json.ok({ sent: 1 }); }\n\
             export default { post };\n",
        )]);
        let handler = t.0.join("ws.js");
        std::fs::write(
            &handler,
            r#"export default { connection() { bus.subscribe("news"); json.ok({ sub: 1 }); } };"#,
        )
        .unwrap();
        let bus = Arc::new(Bus::new());
        let root = t.0.clone();
        let bus_actor = bus.clone();
        let actor = JsActor::pool(1, move || {
            Bridge::with_dbs_and_loader(
                HashMap::new(),
                Arc::new(InMemoryKV::new()),
                SchemaRegistry::new(),
                false,
                Some(Arc::new(LoaderShared {
                    project_root: root.clone(),
                    ts: true,
                })),
                Extras {
                    blobs: None,
                    bus: Some(bus_actor.clone()),
                    ..Default::default()
                },
            )
        });
        let make_bridge = {
            let root = t.0.clone();
            let bus = bus.clone();
            move || {
                Bridge::with_dbs_and_loader(
                    HashMap::new(),
                    Arc::new(InMemoryKV::new()),
                    SchemaRegistry::new(),
                    false,
                    Some(Arc::new(LoaderShared {
                        project_root: root.clone(),
                        ts: true,
                    })),
                    Extras {
                        blobs: None,
                        bus: Some(bus.clone()),
                        ..Default::default()
                    },
                )
            }
        };
        let addr = spawn(
            app(
                "/v1/api",
                t.0.clone(),
                true,
                crate::tests::build_table(&t.0, true, "/v1/api"),
                actor,
                None,
                None,
                "/".to_string(),
                crate::Pipeline::default(),
                Arc::new(std::sync::RwLock::new(crate::CertificateStatus::Valid)),
                Arc::new(std::sync::RwLock::new(None)),
                Arc::default(),
            )
            .merge(js_route(
                "/ws/bus",
                RoutePool::new(
                    handler,
                    Arc::new(make_bridge),
                    std::time::Duration::from_secs(1),
                    WsOptions::default().workers_per_route,
                    WsOptions::default().idle_linger_ms,
                ),
                WsOptions::default(),
            )),
        )
        .await;

        let mut c = WsClient::connect(addr, "/ws/bus").await;
        let env = c.read_text().await;
        assert!(env.contains("\"sub\":1"), "{env}");
        // HTTP 发布 → 广播到订阅的 WS 连接
        raw_http(
            addr,
            "POST /v1/api/pub HTTP/1.1\r\nHost: t\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await;
        let frame = c.read_text().await;
        let v: serde_json::Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"topic": "news", "data": {"a": 1}}),
            "{v}"
        );
    }

    #[tokio::test]
    async fn ws_echo_roundtrip_on_pinned_thread() {
        let _e2e = ws_e2e_lock();
        let t = crate::tests::routes(&[]);
        let addr = spawn(
            app(
                "/v1/api",
                t.0.clone(),
                true,
                crate::tests::build_table(&t.0, true, "/v1/api"),
                crate::tests::make_actor(t.0.clone(), true),
                None,
                None,
                "/".to_string(),
                crate::Pipeline::default(),
                Arc::new(std::sync::RwLock::new(crate::CertificateStatus::Valid)),
                Arc::new(std::sync::RwLock::new(None)),
                Arc::default(),
            )
            .merge(echo_route()),
        )
        .await;
        let mut c = WsClient::connect(addr, "/ws").await;
        c.send_text("ping").await;
        assert_eq!(c.read_text().await, "ping");
    }

    /// 移植 Go TestWSHandle_Connection_Simple：发帧 → JS 处理 → 信封回写。
    #[tokio::test]
    async fn js_route_runs_handler_per_frame() {
        let _e2e = ws_e2e_lock();
        let t = crate::tests::routes(&[]);
        let handler = t.0.join("ws.js");
        std::fs::write(
            &handler,
            r#"export default { message() { json.ok({ pong: true }); } };"#,
        )
        .unwrap();
        let addr = spawn(
            app(
                "/v1/api",
                t.0.clone(),
                true,
                crate::tests::build_table(&t.0, true, "/v1/api"),
                crate::tests::make_actor(t.0.clone(), true),
                None,
                None,
                "/".to_string(),
                crate::Pipeline::default(),
                Arc::new(std::sync::RwLock::new(crate::CertificateStatus::Valid)),
                Arc::new(std::sync::RwLock::new(None)),
                Arc::default(),
            )
            .merge(js_route(
                "/ws/js",
                test_pool(&handler, std::time::Duration::from_secs(1)),
                WsOptions::default(),
            )),
        )
        .await;

        let mut c = WsClient::connect(addr, "/ws/js").await;
        c.send_text(r#"{"hello":"world"}"#).await;
        let resp = c.read_text().await;
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["data"]["pong"], true, "{v}");

        // 第二帧复用同一 VM：仍正常回信封。
        c.send_text("again").await;
        let resp2 = c.read_text().await;
        assert!(resp2.contains("pong"), "{resp2}");
    }

    /// 闸门：max=1 时第 2 条连接被拒（503），存量连接不受影响；max=0 不限。
    #[tokio::test]
    async fn gate_rejects_over_limit_with_503() {
        let _e2e = ws_e2e_lock();
        // 闸门是进程级计数：straggler 槽位未归零时「第 1 条连接」也会吃 503。
        ws_live_zero().await;
        let t = crate::tests::routes(&[]);
        let handler = t.0.join("ws.js");
        std::fs::write(
            &handler,
            r#"export default { message() { json.ok({ pong: 1 }); } };"#,
        )
        .unwrap();
        let addr = spawn(
            app(
                "/v1/api",
                t.0.clone(),
                true,
                crate::tests::build_table(&t.0, true, "/v1/api"),
                crate::tests::make_actor(t.0.clone(), true),
                None,
                None,
                "/".to_string(),
                crate::Pipeline::default(),
                Arc::new(std::sync::RwLock::new(crate::CertificateStatus::Valid)),
                Arc::new(std::sync::RwLock::new(None)),
                Arc::default(),
            )
            .merge(js_route(
                "/ws/gate",
                test_pool(&handler, std::time::Duration::from_secs(1)),
                WsOptions {
                    max_connections: 1,
                    ..WsOptions::default()
                },
            )),
        )
        .await;
        let mut c1 = WsClient::connect(addr, "/ws/gate").await; // 第 1 条：占满
        c1.send_text("hi").await;
        assert!(c1.read_text().await.contains("\"pong\":1"));
        // 第 2 条：upgrade 被拒（非 101）
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(b"GET /ws/gate HTTP/1.1\r\nHost: t\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n")
            .await
            .unwrap();
        let mut buf = vec![0u8; 256];
        let n = s.read(&mut buf).await.unwrap();
        assert!(
            String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 503"),
            "expected 503"
        );
        // 存量连接不受影响
        c1.send_text("again").await;
        assert!(c1.read_text().await.contains("\"pong\":1"));
    }

    /// upgrade 半路失败的槽位归还钉（v0.1.10 终审 Critical #1）：gate_enter 计数
    /// 与 GateGuard 递减必须 1:1——Arc 双克隆（on_failed_upgrade / on_upgrade 各持
    /// 一份）共享一个 Guard，两份克隆都 drop 后 WS_LIVE 恰好回到原值（不多减、
    /// 不漏减）。真实握手失败需客户端 RST 竞态时序，集成层不可稳定复现；本钉锁死
    /// 修复依赖的配对语义——若有人退回「只在 on_upgrade 闭包里 new GateGuard」，
    /// failed 路径无对应克隆，本测试与 js_route 装配的共同前提即被破坏。
    #[tokio::test]
    async fn gate_slot_returned_exactly_once_via_shared_guard() {
        let _e2e = ws_e2e_lock();
        // 基线前等 straggler 槽位归零：上个用例释放锁后其 runtime 关停才归还，
        // 迟来递减会把终值拉低（Windows 复现：left 0 right 1）。
        ws_live_zero().await;
        let before = WS_LIVE.load(Ordering::Relaxed);
        assert!(gate_enter(u64::MAX));
        assert_eq!(WS_LIVE.load(Ordering::Relaxed), before + 1);
        let gate = Arc::new(GateGuard);
        let failed_gate = Arc::clone(&gate);
        drop(failed_gate); // on_failed_upgrade 路径先终结：Guard 仍被 gate 持有，不减
        assert_eq!(WS_LIVE.load(Ordering::Relaxed), before + 1);
        drop(gate); // 另一路径终结：Guard 唯一一次 drop，恰好减一
        assert_eq!(WS_LIVE.load(Ordering::Relaxed), before);
    }

    /// ws.send 先于信封写出、ws.close 结束连接（顺序契约）。
    #[tokio::test]
    async fn js_route_ws_send_order_and_close() {
        let _e2e = ws_e2e_lock();
        let t = crate::tests::routes(&[]);
        let handler = t.0.join("ws.js");
        std::fs::write(
            &handler,
            r#"export default { message() { ws.send("side"); json.ok({ done: 1 }); ws.close(); } };"#,
        )
        .unwrap();
        let addr = spawn(
            app(
                "/v1/api",
                t.0.clone(),
                true,
                crate::tests::build_table(&t.0, true, "/v1/api"),
                crate::tests::make_actor(t.0.clone(), true),
                None,
                None,
                "/".to_string(),
                crate::Pipeline::default(),
                Arc::new(std::sync::RwLock::new(crate::CertificateStatus::Valid)),
                Arc::new(std::sync::RwLock::new(None)),
                Arc::default(),
            )
            .merge(js_route(
                "/ws/close",
                test_pool(&handler, std::time::Duration::from_secs(1)),
                WsOptions::default(),
            )),
        )
        .await;

        let mut c = WsClient::connect(addr, "/ws/close").await;
        c.send_text("go").await;
        assert_eq!(c.read_text().await, "side"); // ws.send 先写
        let envelope = c.read_text().await;
        assert!(envelope.contains("\"done\":1"), "{envelope}");
        // close 后连接终止：Close 帧或 EOF（不 panic）。
        let mut buf = [0u8; 64];
        let n = c.0.read(&mut buf).await.unwrap();
        assert!(
            n == 0 || buf[0] == 0x88,
            "expected close, got {n} bytes: {:x?}",
            &buf[..n]
        );
    }

    /// 生产挂载（与 oj server 装配同构）：<root>/<dir>/ws.ts 目录镜像 → GET {base}/<dir>/ws；
    /// app().merge(mirror_routes())，WS 工厂与 actor 共享 Bus → HTTP publish 广播到订阅连接。
    /// ws.ts 经统一转译管线（类型标注可用）。
    #[tokio::test]
    async fn mirror_routes_mount_directory_ws() {
        let _e2e = ws_e2e_lock();
        use crate::actor::JsActor;
        use only_js::bridge::{Bus, Extras, LoaderShared, SchemaRegistry};
        use std::collections::HashMap;
        let t = crate::tests::routes(&[
            (
                "news/api.ts",
                "function post() { bus.publish(\"news\", { a: 1 }); json.ok({ sent: 1 }); }\n\
                 export default { post };\n",
            ),
            (
                "news/ws.ts",
                "const n: number = 1;\nexport default {\n  connection() {\n    bus.subscribe(\"news\");\n    json.ok({ sub: n });\n  },\n};\n",
            ),
        ]);
        let bus = Arc::new(Bus::new());
        let root = t.0.clone();
        let bus2 = bus.clone();
        let make = move || {
            Bridge::with_dbs_and_loader(
                HashMap::new(),
                Arc::new(InMemoryKV::new()),
                SchemaRegistry::new(),
                false,
                Some(Arc::new(LoaderShared {
                    project_root: root.clone(),
                    ts: true,
                })),
                Extras {
                    blobs: None,
                    bus: Some(bus2.clone()),
                    ..Default::default()
                },
            )
        };
        let addr = spawn(
            app(
                "/v1/api",
                t.0.clone(),
                true,
                crate::tests::build_table(&t.0, true, "/v1/api"),
                JsActor::pool(1, make.clone()),
                None,
                None,
                "/".to_string(),
                crate::Pipeline::default(),
                Arc::new(std::sync::RwLock::new(crate::CertificateStatus::Valid)),
                Arc::new(std::sync::RwLock::new(None)),
                Arc::default(),
            )
            .merge(mirror_routes(
                "/v1/api",
                &t.0,
                std::time::Duration::from_secs(2),
                make,
                WsOptions::default(),
            )),
        )
        .await;

        let mut c = WsClient::connect(addr, "/v1/api/news/ws").await;
        let env = c.read_text().await;
        assert!(env.contains("\"sub\":1"), "{env}");
        // HTTP publish（run_module 生产路径）→ 订阅连接收广播帧
        raw_http(
            addr,
            "POST /v1/api/news HTTP/1.1\r\nHost: t\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await;
        let v: serde_json::Value = serde_json::from_str(&c.read_text().await).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"topic": "news", "data": {"a": 1}}),
            "{v}"
        );
    }

    /// 根级 ws.ts → GET {base}/ws（rel 为空时不得拼出 {base}//ws 双斜杠路径）。
    #[tokio::test]
    async fn mirror_routes_root_ws() {
        let _e2e = ws_e2e_lock();
        use crate::actor::JsActor;
        use only_js::bridge::{Extras, LoaderShared, SchemaRegistry};
        use std::collections::HashMap;
        let t = crate::tests::routes(&[(
            "ws.ts",
            "export default { message() { json.ok({ root: true }); } };\n",
        )]);
        let root = t.0.clone();
        let make = move || {
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
        };
        let addr = spawn(
            app(
                "/v1/api",
                t.0.clone(),
                true,
                crate::tests::build_table(&t.0, true, "/v1/api"),
                JsActor::pool(1, make.clone()),
                None,
                None,
                "/".to_string(),
                crate::Pipeline::default(),
                Arc::new(std::sync::RwLock::new(crate::CertificateStatus::Valid)),
                Arc::new(std::sync::RwLock::new(None)),
                Arc::default(),
            )
            .merge(mirror_routes(
                "/v1/api",
                &t.0,
                std::time::Duration::from_secs(2),
                make,
                WsOptions::default(),
            )),
        )
        .await;

        // 单斜杠 {base}/ws 必须命中（文档契约）；双斜杠路径本就不该存在。
        let mut c = WsClient::connect(addr, "/v1/api/ws").await;
        c.send_text("hi").await;
        let env = c.read_text().await;
        assert!(env.contains("\"root\":true"), "{env}");
    }

    /// 移植 Go TestWSHandle_Connection_MissingFile：handler 文件缺失不 panic，连接直接关闭。
    #[tokio::test]
    async fn js_route_missing_handler_closes_quietly() {
        let _e2e = ws_e2e_lock();
        let t = crate::tests::routes(&[]);
        let addr = spawn(
            app(
                "/v1/api",
                t.0.clone(),
                true,
                crate::tests::build_table(&t.0, true, "/v1/api"),
                crate::tests::make_actor(t.0.clone(), true),
                None,
                None,
                "/".to_string(),
                crate::Pipeline::default(),
                Arc::new(std::sync::RwLock::new(crate::CertificateStatus::Valid)),
                Arc::new(std::sync::RwLock::new(None)),
                Arc::default(),
            )
            .merge(js_route(
                "/ws/missing",
                test_pool(&t.0.join("nope.js"), std::time::Duration::from_secs(1)),
                WsOptions::default(),
            )),
        )
        .await;
        let mut c = WsClient::connect(addr, "/ws/missing").await;
        c.send_text("any").await;
        // 服务端发 Close 帧（0x88）后关连接：读到 Close、EOF 均算干净终止，不 panic。
        // Windows 上，缺失 handler 时服务端丢弃连接且入站 "any" 仍留在接收缓冲区，
        // 关闭会触发 TCP RST（而非 FIN），read 返回 Err(ConnectionReset) 而非 Ok(0)，
        // 同样表示连接已被对端干净终止。只接受传输层终止类错误——其他错误（如
        // 服务端 panic 之外的意外 IO 失败）仍应让测试失败，保住本用例的回归护栏。
        let mut buf = [0u8; 64];
        let res = c.0.read(&mut buf).await;
        let clean = match &res {
            Ok(0) => true,            // 对端优雅关闭（EOF / FIN）
            Ok(_n) => buf[0] == 0x88, // 收到 WebSocket Close 帧
            Err(e) => matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::UnexpectedEof
            ),
        };
        assert!(clean, "expected close or reset, got {res:?}");
    }

    /// 帧内 bus.publish（教学案例 sample/src/news/chat/ws.ts 的回归钉）：
    /// 连接 A 订阅后，连接 B 发 {"text":..} 帧 → B 的帧内 publish → A 收广播帧，
    /// B 自己（同主题订阅）也自收（自回声）。同时验证：
    /// 1) publish 无上下文限制（fire-and-forget，不 await 也照常广播——
    ///    run_event_loop 会把 op future 驱动完才捕获信封）；
    /// 2) 模块作用域 = 连接状态；进房 = connection 钩子（无需 join 帧）；
    /// 3) http.body 对 JSON 文本帧自动 parse 成对象。
    #[tokio::test]
    async fn ws_frame_publish_broadcasts_to_subscribers() {
        let _e2e = ws_e2e_lock();
        use crate::actor::JsActor;
        use only_js::bridge::{Bus, Extras, LoaderShared, SchemaRegistry};
        use std::collections::HashMap;
        let t = crate::tests::routes(&[
            ("news/api.ts", "export default {};\n"),
            (
                "news/ws.ts",
                r#"export default {
  connection() {
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
"#,
            ),
        ]);
        let bus = Arc::new(Bus::new());
        let root = t.0.clone();
        let bus2 = bus.clone();
        let make = move || {
            Bridge::with_dbs_and_loader(
                HashMap::new(),
                Arc::new(InMemoryKV::new()),
                SchemaRegistry::new(),
                false,
                Some(Arc::new(LoaderShared {
                    project_root: root.clone(),
                    ts: true,
                })),
                Extras {
                    blobs: None,
                    bus: Some(bus2.clone()),
                    ..Default::default()
                },
            )
        };
        let addr = spawn(
            app(
                "/v1/api",
                t.0.clone(),
                true,
                crate::tests::build_table(&t.0, true, "/v1/api"),
                JsActor::pool(1, make.clone()),
                None,
                None,
                "/".to_string(),
                crate::Pipeline::default(),
                Arc::new(std::sync::RwLock::new(crate::CertificateStatus::Valid)),
                Arc::new(std::sync::RwLock::new(None)),
                Arc::default(),
            )
            .merge(mirror_routes(
                "/v1/api",
                &t.0,
                std::time::Duration::from_secs(2),
                make,
                WsOptions::default(),
            )),
        )
        .await;

        let broadcast = serde_json::json!({
            "topic": "chat",
            "data": {"from": "anon", "text": "hi"}
        });
        // 连接 A 进房（进房 = connection 钩子，无需 join 帧）
        let mut a = WsClient::connect(addr, "/v1/api/news/ws").await;
        assert!(a.read_text().await.contains("\"joined\":true"), "a join");
        // 连接 B 进房后发聊天帧（JSON 文本 → http.body 自动 parse）
        let mut b = WsClient::connect(addr, "/v1/api/news/ws").await;
        assert!(b.read_text().await.contains("\"joined\":true"), "b join");
        b.send_text(r#"{"text":"hi"}"#).await;
        // A 只会收到一条：广播帧（fire-and-forget publish 照常投递）
        let v: serde_json::Value = serde_json::from_str(&a.read_text().await).unwrap();
        assert_eq!(v, broadcast, "a receives broadcast");
        // B 收两条：回执信封 + 自己的广播（自回声），顺序不保证（Processor vs forwarder）
        let f1 = c_frame(&mut b).await;
        let f2 = c_frame(&mut b).await;
        let frames = [f1, f2];
        let got_broadcast = frames.contains(&broadcast);
        let got_envelope = frames
            .iter()
            .any(|f| f["data"]["sent"] == serde_json::Value::Bool(true));
        assert!(got_broadcast, "b self-echo broadcast");
        assert!(got_envelope, "b envelope");
    }

    /// 读一帧并 parse 成 JSON（测试辅助）。
    async fn c_frame(c: &mut WsClient) -> serde_json::Value {
        serde_json::from_str(&c.read_text().await).unwrap()
    }

    /// error() 兜底后连接继续：message 抛异常 → error(e) 经 ws.send 带出 → 下一帧仍正常处理。
    #[tokio::test]
    async fn js_route_error_hook_keeps_connection_alive() {
        let _e2e = ws_e2e_lock();
        let t = crate::tests::routes(&[]);
        let handler = t.0.join("ws.js");
        std::fs::write(
            &handler,
            r#"export default {
  message() {
    if (http.body.bad) throw new Error("boom");
    json.ok({ ok: 1 });
  },
  error(e) { ws.send("err:" + e.message); },
};"#,
        )
        .unwrap();
        let addr = spawn(
            app(
                "/v1/api",
                t.0.clone(),
                true,
                crate::tests::build_table(&t.0, true, "/v1/api"),
                crate::tests::make_actor(t.0.clone(), true),
                None,
                None,
                "/".to_string(),
                crate::Pipeline::default(),
                Arc::new(std::sync::RwLock::new(crate::CertificateStatus::Valid)),
                Arc::new(std::sync::RwLock::new(None)),
                Arc::default(),
            )
            .merge(js_route(
                "/ws/err",
                test_pool(&handler, std::time::Duration::from_secs(1)),
                WsOptions::default(),
            )),
        )
        .await;
        let mut c = WsClient::connect(addr, "/ws/err").await;
        c.send_text(r#"{"bad":true}"#).await;
        assert_eq!(c.read_text().await, "err:boom"); // 异常帧：error 兜底，无信封
        c.send_text(r#"{"bad":false}"#).await;
        let resp = c.read_text().await; // 连接继续：下一帧正常回信封
        assert!(resp.contains("\"ok\":1"), "{resp}");
    }

    /// close 钩子恰好一次：ws.close() 触发收尾 → close() 执行、ws.send 离帧先于 Close 写出。
    /// 注：不用客户端断连（Close 帧 / FIN）观察钩子离帧——axum/tungstenite 一旦收到对端
    /// 关闭信号即拒绝一切出站数据帧（SendAfterClosing），close 钩子的离帧无法上线
    /// （服务端仍恰好 fire 一次 close，仅输出不可见）；故以三来源统一的 ws.close() 钉钩子。
    #[tokio::test]
    async fn js_route_close_hook_fires_exactly_once() {
        let _e2e = ws_e2e_lock();
        let t = crate::tests::routes(&[]);
        let handler = t.0.join("ws.js");
        std::fs::write(
            &handler,
            r#"export default { message() { ws.close(); }, close() { ws.send("bye"); } };"#,
        )
        .unwrap();
        let addr = spawn(
            app(
                "/v1/api",
                t.0.clone(),
                true,
                crate::tests::build_table(&t.0, true, "/v1/api"),
                crate::tests::make_actor(t.0.clone(), true),
                None,
                None,
                "/".to_string(),
                crate::Pipeline::default(),
                Arc::new(std::sync::RwLock::new(crate::CertificateStatus::Valid)),
                Arc::new(std::sync::RwLock::new(None)),
                Arc::default(),
            )
            .merge(js_route(
                "/ws/bye",
                test_pool(&handler, std::time::Duration::from_secs(1)),
                WsOptions::default(),
            )),
        )
        .await;
        let mut c = WsClient::connect(addr, "/ws/bye").await;
        c.send_text("go").await;
        assert_eq!(c.read_text().await, "bye"); // close 钩子的离帧（恰好一条）
        // 随后 Close 帧或 EOF（Writer 收尾），不 panic。
        let mut buf = [0u8; 8];
        let n = c.0.read(&mut buf).await.unwrap_or(0);
        assert!(n == 0 || buf[0] == 0x88, "expected close, got {n} bytes");
    }

    /// 一刀切契约：无任何钩子导出 → 连接建立即断（Close 帧 / EOF），不静默空转。
    #[tokio::test]
    async fn js_route_no_hooks_disconnects() {
        let _e2e = ws_e2e_lock();
        let t = crate::tests::routes(&[]);
        let handler = t.0.join("ws.js");
        std::fs::write(&handler, r#"json.ok({});"#).unwrap(); // 无 default 导出
        let addr = spawn(
            app(
                "/v1/api",
                t.0.clone(),
                true,
                crate::tests::build_table(&t.0, true, "/v1/api"),
                crate::tests::make_actor(t.0.clone(), true),
                None,
                None,
                "/".to_string(),
                crate::Pipeline::default(),
                Arc::new(std::sync::RwLock::new(crate::CertificateStatus::Valid)),
                Arc::new(std::sync::RwLock::new(None)),
                Arc::default(),
            )
            .merge(js_route(
                "/ws/nohooks",
                test_pool(&handler, std::time::Duration::from_secs(1)),
                WsOptions::default(),
            )),
        )
        .await;
        let mut c = WsClient::connect(addr, "/ws/nohooks").await;
        c.send_text("any").await;
        // ws_connect 失败 → 服务端发 Close 帧后关连接；传输层终止类错误同样算干净断连
        // （Windows RST 语义，断言同 js_route_missing_handler_closes_quietly）。
        let mut buf = [0u8; 64];
        let res = c.0.read(&mut buf).await;
        let clean = match &res {
            Ok(0) => true,
            Ok(_n) => buf[0] == 0x88,
            Err(e) => matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::UnexpectedEof
            ),
        };
        assert!(clean, "expected close or reset, got {res:?}");
    }

    /// 帧池毒化半径 = 1（e2e）：连接 A 死循环帧 300ms 看门狗超时 → A 必断连；
    /// 连接 B 照常收发（同池其他 worker/补员接帧，每连接独立 ConnHandle）。
    #[tokio::test]
    async fn frame_pool_timeout_isolates_connections() {
        let _e2e = ws_e2e_lock();
        let t = crate::tests::routes(&[]);
        let handler = t.0.join("ws.js");
        std::fs::write(
            &handler,
            r#"export default {
  message() {
    if (http.body.boom) { const t = Date.now(); while (Date.now() - t < 60_000) {} }
    json.ok({ ok: 1 });
  },
};"#,
        )
        .unwrap();
        let timeout = std::time::Duration::from_millis(300);
        let addr = spawn(
            app(
                "/v1/api",
                t.0.clone(),
                true,
                crate::tests::build_table(&t.0, true, "/v1/api"),
                crate::tests::make_actor(t.0.clone(), true),
                None,
                None,
                "/".to_string(),
                crate::Pipeline::default(),
                Arc::new(std::sync::RwLock::new(crate::CertificateStatus::Valid)),
                Arc::new(std::sync::RwLock::new(None)),
                Arc::default(),
            )
            .merge(js_route(
                "/ws/iso",
                test_pool(&handler, timeout),
                WsOptions::default(),
            )),
        )
        .await;
        let mut a = WsClient::connect(addr, "/ws/iso").await;
        let mut b = WsClient::connect(addr, "/ws/iso").await;
        // A：毒帧 → 300ms 超时断连（Close/EOF/RST 族，判定同 missing-file 用例）。
        a.send_text(r#"{"boom":true}"#).await;
        let mut buf = [0u8; 64];
        let res = a.0.read(&mut buf).await;
        let clean = match &res {
            Ok(0) => true,
            Ok(_n) => buf[0] == 0x88,
            Err(e) => matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::UnexpectedEof
            ),
        };
        assert!(clean, "expected poisoned conn to close, got {res:?}");
        // B：毒化只波及执行它的 worker，B 照常收发。
        b.send_text(r#"{"boom":false}"#).await;
        let v: serde_json::Value = serde_json::from_str(&b.read_text().await).unwrap();
        assert_eq!(v["data"]["ok"], 1, "{v}");
    }

    /// 同连接保序（帧池 per-conn 串行调度钉）：三帧连发，回帧序必须 = 1,2,3。
    /// handler 忙等 30ms 制造重叠窗口——若调度不按连接串行，后帧会在前帧写回
    /// 会话状态前克隆到旧 state，断言即崩。注：本 runtime 无 timer 全局
    /// （setTimeout 不可用），以 Date.now 忙等代替 brief 草图中的 await sleep。
    #[tokio::test]
    async fn frame_pool_preserves_per_connection_order() {
        let _e2e = ws_e2e_lock();
        let t = crate::tests::routes(&[]);
        let handler = t.0.join("ws.js");
        std::fs::write(
            &handler,
            r#"export default {
  message() {
    const t = Date.now(); while (Date.now() - t < 30) {}
    sess.state.n = (sess.state.n ?? 0) + 1;
    json.ok({ n: sess.state.n });
  },
};"#,
        )
        .unwrap();
        let timeout = std::time::Duration::from_secs(1);
        let addr = spawn(
            app(
                "/v1/api",
                t.0.clone(),
                true,
                crate::tests::build_table(&t.0, true, "/v1/api"),
                crate::tests::make_actor(t.0.clone(), true),
                None,
                None,
                "/".to_string(),
                crate::Pipeline::default(),
                Arc::new(std::sync::RwLock::new(crate::CertificateStatus::Valid)),
                Arc::new(std::sync::RwLock::new(None)),
                Arc::default(),
            )
            .merge(js_route(
                "/ws/order",
                test_pool(&handler, timeout),
                WsOptions::default(),
            )),
        )
        .await;
        let mut c = WsClient::connect(addr, "/ws/order").await;
        for _ in 0..3 {
            c.send_text("go").await;
        }
        for expect in [1, 2, 3] {
            let v: serde_json::Value = serde_json::from_str(&c.read_text().await).unwrap();
            assert_eq!(v["data"]["n"], expect, "同连接回帧必须保序: {v}");
        }
    }
}
