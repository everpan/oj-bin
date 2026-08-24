//! WebSocket 层（P5a echo + P5b JS 帧循环，移植 Go internal/bridge/ws.go）。
//!
//! Go 模式：WS 路由注册在 catch-all 之前；每连接独占 VM（不进 HTTP 池）；
//! Reader/Processor/Writer 三任务流水线，msgChan/respChan 各 cap 64（背压保护）。
//! Rust 的硬约束：`JsRuntime` !Send → 整条帧循环钉在专用线程的 current_thread runtime 上，
//! axum 侧只完成 upgrade 后把 socket 整体移交（WebSocket: Send 可跨线程搬）。

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;

use mdm_base_rust::bridge::{Bridge, RequestInfo};

/// 挂载最小 echo 路由（GET /ws）——P5a 链路验证用。
pub fn echo_route() -> axum::Router {
    axum::Router::new().route("/ws", axum::routing::get(upgrade))
}

/// 挂载 JS handler 帧循环路由（对齐 Go RegisterWSJS）：每帧执行 handler_file，
/// json.ok 信封与 ws.send 逐帧写回；timeout 为单帧熔断（超时丢弃该帧，连接继续）。
pub fn js_route(
    path: &str,
    handler_file: impl Into<PathBuf>,
    timeout: std::time::Duration,
    make_bridge: impl Fn() -> Bridge + Send + Sync + 'static,
) -> axum::Router {
    let file = handler_file.into();
    let make = Arc::new(make_bridge);
    axum::Router::new().route(path, axum::routing::get(move |ws: axum::extract::WebSocketUpgrade| {
        let file = file.clone();
        let make = make.clone();
        async move { ws.on_upgrade(move |socket| conn_on_pinned(socket, file, timeout, make)) }
    }))
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

/// JS 帧循环连接处理：搬到专用线程后跑三任务流水线。
async fn conn_on_pinned(
    socket: WebSocket,
    handler_file: PathBuf,
    timeout: std::time::Duration,
    make: Arc<dyn Fn() -> Bridge + Send + Sync>,
) {
    std::thread::Builder::new()
        .name("ws-js".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("ws-js runtime init");
            rt.block_on(frame_loop(socket, handler_file, timeout, make));
        })
        .expect("spawn ws-js thread");
}

/// 三任务流水线（对齐 Go HandleWSConnection）：
/// Reader(stream→msgChan) / Processor(串行 JS) / Writer(respChan→sink)，chan 各 cap 64。
/// 读 handler 失败（文件缺失等）→ 直接结束（连接关闭，不 panic，对齐 Go）。
async fn frame_loop(
    socket: WebSocket,
    handler_file: PathBuf,
    timeout: std::time::Duration,
    make: Arc<dyn Fn() -> Bridge + Send + Sync>,
) {
    let source = match std::fs::read_to_string(&handler_file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("ws compile {}: {e}", handler_file.display());
            // 先发 Close 帧再丢弃，避免未读数据触发 TCP RST（客户端拿到干净关闭）。
            let mut socket = socket;
            let _ = socket.send(Message::Close(None)).await;
            return;
        }
    };
    let (msg_tx, mut msg_rx) = mpsc::channel::<Vec<u8>>(64);
    let (resp_tx, mut resp_rx) = mpsc::channel::<String>(64);
    // bus 会话端：订阅注册用的发送端注入每帧 RequestInfo；收到的广播帧转写回 socket。
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

    // Writer：respChan → 串行写回；通道排空（连接结束）后发 Close 帧干净关闭。
    let writer = tokio::spawn(async move {
        while let Some(text) = resp_rx.recv().await {
            if sink.send(Message::Text(text.into())).await.is_err() {
                return;
            }
        }
        let _ = sink.send(Message::Close(None)).await;
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

    // Processor：串行执行 JS（当前任务），每帧独立 ReqState，超时/出错丢弃该帧继续。
    let bridge = make();
    while let Some(msg) = msg_rx.recv().await {
        let req = RequestInfo {
            method: "WS".into(),
            body: msg,
            bus_tx: Some(bus_tx.clone()),
            ..Default::default()
        };
        match bridge.run_ws(&source, req, timeout).await {
            Ok(o) => {
                for s in o.sends {
                    let _ = resp_tx.try_send(s); // 满则丢弃（对齐 Go select+default）
                }
                if !o.capture.body.is_empty() {
                    let _ = resp_tx.try_send(String::from_utf8_lossy(&o.capture.body).into_owned());
                }
                if o.close {
                    break;
                }
            }
            Err(e) => eprintln!("ws frame error: {e}"),
        }
    }
    drop(resp_tx); // Writer 排空后自然退出
    forwarder.abort(); // 释放 bus_rx 与 resp_tx 克隆，Writer 才能排空退出
    let _ = writer.await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app;
    use mdm_base_rust::bridge::{InMemoryAccessor, InMemoryKV};
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

    fn make_bridge() -> Bridge {
        Bridge::new(
            Arc::new(InMemoryAccessor::new()),
            Arc::new(InMemoryKV::new()),
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
        use crate::actor::JsActor;
        use mdm_base_rust::bridge::{Bus, Extras, LoaderShared, SchemaRegistry};
        use std::collections::HashMap;
        let t = crate::tests::routes(&[(
            "pub/api.ts",
            "function post() { bus.publish(\"news\", { a: 1 }); json.ok({ sent: 1 }); }\n\
             export default { post };\n",
        )]);
        let handler = t.0.join("WS.js");
        std::fs::write(&handler, r#"bus.subscribe("news"); json.ok({ sub: 1 });"#).unwrap();
        let bus = Arc::new(Bus::new());
        let root = t.0.clone();
        let bus_actor = bus.clone();
        let actor = JsActor::pool(1, move || {
            Bridge::with_dbs_and_loader(
                HashMap::new(),
                Arc::new(InMemoryKV::new()),
                SchemaRegistry::new(),
                false,
                Some(Arc::new(LoaderShared { project_root: root.clone(), ts: true })),
                Extras { blob: None, bus: Some(bus_actor.clone()) },
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
                    Some(Arc::new(LoaderShared { project_root: root.clone(), ts: true })),
                    Extras { blob: None, bus: Some(bus.clone()) },
                )
            }
        };
        let addr = spawn(
            app("/v1/api", t.0.clone(), true, crate::tests::build_table(&t.0, true, "/v1/api"), actor, None, None, crate::Pipeline::default()).merge(js_route(
                "/ws/bus",
                handler,
                std::time::Duration::from_secs(1),
                make_bridge,
            )),
        )
        .await;

        let mut c = WsClient::connect(addr, "/ws/bus").await;
        c.send_text("subscribe").await;
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
        assert_eq!(v, serde_json::json!({"topic": "news", "data": {"a": 1}}), "{v}");
    }

    #[tokio::test]
    async fn ws_echo_roundtrip_on_pinned_thread() {
        let t = crate::tests::routes(&[]);
        let addr = spawn(
            app("/v1/api", t.0.clone(), true, crate::tests::build_table(&t.0, true, "/v1/api"), crate::tests::actor(t.0.clone(), true), None, None, crate::Pipeline::default()).merge(echo_route()),
        )
        .await;
        let mut c = WsClient::connect(addr, "/ws").await;
        c.send_text("ping").await;
        assert_eq!(c.read_text().await, "ping");
    }

    /// 移植 Go TestWSHandle_Connection_Simple：发帧 → JS 处理 → 信封回写。
    #[tokio::test]
    async fn js_route_runs_handler_per_frame() {
        let t = crate::tests::routes(&[]);
        let handler = t.0.join("WS.js");
        std::fs::write(&handler, r#"json.ok({ pong: true });"#).unwrap();
        let addr = spawn(
            app("/v1/api", t.0.clone(), true, crate::tests::build_table(&t.0, true, "/v1/api"), crate::tests::actor(t.0.clone(), true), None, None, crate::Pipeline::default()).merge(js_route(
                "/ws/js",
                handler.clone(),
                std::time::Duration::from_secs(1),
                make_bridge,
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

    /// ws.send 先于信封写出、ws.close 结束连接（顺序契约）。
    #[tokio::test]
    async fn js_route_ws_send_order_and_close() {
        let t = crate::tests::routes(&[]);
        let handler = t.0.join("WS.js");
        std::fs::write(&handler, r#"ws.send("side"); json.ok({ done: 1 }); ws.close();"#).unwrap();
        let addr = spawn(
            app("/v1/api", t.0.clone(), true, crate::tests::build_table(&t.0, true, "/v1/api"), crate::tests::actor(t.0.clone(), true), None, None, crate::Pipeline::default()).merge(js_route(
                "/ws/close",
                handler,
                std::time::Duration::from_secs(1),
                make_bridge,
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
        assert!(n == 0 || buf[0] == 0x88, "expected close, got {n} bytes: {:x?}", &buf[..n]);
    }

    /// 移植 Go TestWSHandle_Connection_MissingFile：handler 文件缺失不 panic，连接直接关闭。
    #[tokio::test]
    async fn js_route_missing_handler_closes_quietly() {
        let t = crate::tests::routes(&[]);
        let addr = spawn(
            app("/v1/api", t.0.clone(), true, crate::tests::build_table(&t.0, true, "/v1/api"), crate::tests::actor(t.0.clone(), true), None, None, crate::Pipeline::default()).merge(js_route(
                "/ws/missing",
                t.0.join("nope.js"),
                std::time::Duration::from_secs(1),
                make_bridge,
            )),
        )
        .await;
        let mut c = WsClient::connect(addr, "/ws/missing").await;
        c.send_text("any").await;
        // 服务端发 Close 帧（0x88）后关连接：读到 Close 或 EOF 均算干净终止，不 panic。
        let mut buf = [0u8; 64];
        let n = c.0.read(&mut buf).await.unwrap();
        assert!(n == 0 || buf[0] == 0x88, "expected close, got {n} bytes: {:x?}", &buf[..n]);
    }
}
