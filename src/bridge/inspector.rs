//! InspectorServer：基于 deno_core 自带的 `JsRuntimeInspector` 提供 Chrome DevTools 调试。
//!
//! 不依赖 `deno_runtime`：deno_core 已暴露 `JsRuntimeInspector::create_local_session`，
//! 本模块补上 CDP 的 WebSocket 传输（tungstenite）。每条 WS 连接对应一个 local session，
//! 双向转发 DevTools 前端与 V8 inspector 之间的 JSON 消息。

use std::net::SocketAddr;
use std::rc::Rc;

use deno_core::{InspectorMsg, InspectorSessionKind, JsRuntimeInspector};
use futures::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::accept_async;

/// 启动一个 inspector WebSocket 服务，监听 `addr`，将连接绑定到给定 runtime 的 inspector。
///
/// 该 runtime 必须以 `RuntimeOptions { inspector: true }` 创建。服务在后台 task 运行。
pub fn spawn(inspector: Rc<JsRuntimeInspector>, addr: SocketAddr) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn_local(async move {
        let listener = match TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                tracing::error!(target: "inspector", %e, "bind failed");
                return;
            }
        };
        tracing::info!(target: "inspector", %addr, "DevTools inspector listening (chrome://inspect)");
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => continue,
            };
            let ws = match accept_async(stream).await {
                Ok(w) => w,
                Err(e) => {
                    tracing::warn!(target: "inspector", %e, peer = %peer, "ws handshake failed");
                    continue;
                }
            };
            let insp = inspector.clone();
            tokio::task::spawn_local(session_loop(insp, ws));
        }
    })
}

/// 单条 DevTools 连接的消息桥接：WS 文本 <-> local session。
async fn session_loop(
    inspector: Rc<JsRuntimeInspector>,
    ws: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
) {
    let (mut ws_tx, mut ws_rx) = ws.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();

    // 出站消息经 tx 推给 WS。
    let mut sess = JsRuntimeInspector::create_local_session(
        inspector,
        Box::new(move |msg: InspectorMsg| {
            let _ = tx.send(msg.content);
        }) as Box<dyn Fn(InspectorMsg)>,
        InspectorSessionKind::Blocking,
    );

    // 出站：rx -> WS。
    let pump = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if ws_tx
                .send(tokio_tungstenite::tungstenite::Message::Text(msg.into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    // 入站：WS -> dispatch。
    while let Some(Ok(msg)) = ws_rx.next().await {
        match msg {
            tokio_tungstenite::tungstenite::Message::Text(t) => {
                sess.dispatch(t.to_string());
            }
            tokio_tungstenite::tungstenite::Message::Close(_) => break,
            _ => continue,
        }
    }
    pump.abort();
}

#[cfg(test)]
mod tests {
    use super::*;
    use deno_core::{JsRuntime, RuntimeOptions};
    use futures::{SinkExt, StreamExt};
    use std::time::Duration;
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message;

    #[tokio::test(flavor = "current_thread")]
    async fn spawn_bind_failure_logs_and_returns() {
        // rt/insp 必须在 LocalSet 之外创建，且 LocalSet（持有 spawned 任务里的 insp clone）
        // 必须在 rt 之前释放，否则 JsRuntime drop 断言 "inspector must be dropped before runtime"。
        let rt = JsRuntime::new(RuntimeOptions {
            inspector: true,
            ..Default::default()
        });
        let insp = rt.inspector();
        let ls = tokio::task::LocalSet::new();
        ls.run_until(async {
            // 端口 1 通常无绑定权限 → bind 失败分支。
            spawn(insp.clone(), "0.0.0.0:1".parse().unwrap());
            tokio::time::sleep(Duration::from_millis(40)).await;
        })
        .await;
        drop(ls);
        drop(insp);
        drop(rt);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_forwards_cdp_and_close() {
        let rt = JsRuntime::new(RuntimeOptions {
            inspector: true,
            ..Default::default()
        });
        let insp = rt.inspector();
        // 取空闲端口后释放，交给 spawn 重新绑定。
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        drop(l);
        let ls = tokio::task::LocalSet::new();
        ls.run_until(async {
            spawn(insp.clone(), addr);
            // 后台任务里的 bind 与本 async 同处 current_thread，但要等 LocalSet 轮到
            // 它才发生。固定 sleep 在高负载（整套并行跑 V8 用例）下不够 → 误报
            // ConnectionRefused。改为重试连接直至就绪（deadline 兜底防挂死）。
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            let mut ws = loop {
                match connect_async(format!("ws://{addr}")).await {
                    Ok((ws, _)) => break ws,
                    Err(_) if std::time::Instant::now() < deadline => {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                    Err(e) => panic!("inspector did not come up at {addr}: {e}"),
                }
            };
            ws.send(Message::Text(
                r#"{"id":1,"method":"Runtime.enable"}"#.into(),
            ))
            .await
            .unwrap();
            // 期望收到 CDP 响应/事件（带超时防挂死；高负载下调度抖动可吃掉数秒，
            // 与 oj-es drive 同理给足墙钟预算）。
            let got = tokio::time::timeout(Duration::from_secs(30), ws.next()).await;
            assert!(got.is_ok(), "expected a CDP frame from inspector");
            let msg = got.unwrap().unwrap().unwrap();
            assert!(
                matches!(msg, Message::Text(_)),
                "expected text frame: {msg:?}"
            );

            // Close → session_loop 走 Close 分支并中止 pump。
            ws.send(Message::Close(None)).await.unwrap();
            tokio::time::sleep(Duration::from_millis(30)).await;
        })
        .await;
        drop(ls);
        drop(insp);
        drop(rt);
    }
}
