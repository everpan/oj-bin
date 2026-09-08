//! ws.* 绑定：WebSocket 帧循环内 JS 主动控制。
//!
//! 仅两个 op：send(data) 收集到 ReqState.ws_sends（帧处理器结束后按序写出）、
//! close() 置位 ReqState.ws_close（本帧结束后关连接）。HTTP 请求路径不读这两项（等价 nil 连接 no-op）。

use deno_core::{OpState, op2};

use super::ReqState;

/// ws.send(data)：记录一次主动发送（Processor 按序推给 Writer）。
#[op2(fast)]
pub(crate) fn op_ws_send(state: &mut OpState, #[string] data: String) {
    state.borrow_mut::<ReqState>().ws_sends.push(data);
}

/// ws.close()：请求关闭当前连接。
/// 名字避开 deno_websocket 内置 `op_ws_close`（出站客户端，v0.1.7 并存注册）。
#[op2(fast)]
pub(crate) fn op_ws_frame_close(state: &mut OpState) {
    state.borrow_mut::<ReqState>().ws_close = true;
}

#[cfg(test)]
mod ws_client_tests {
    use crate::bridge::{Bridge, Extras, InMemoryKV, RequestInfo, SchemaRegistry};
    use std::collections::HashMap;
    use std::sync::Arc;

    fn bridge() -> Bridge {
        Bridge::with_dbs_and_loader(
            HashMap::new(),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            None,
            Extras::default(),
        )
    }

    async fn run(b: &Bridge, src: &str) -> Result<String, String> {
        b.run_with(src, RequestInfo::default())
            .await
            .map(|c| String::from_utf8_lossy(&c.body).into_owned())
            .map_err(|e| e.to_string())
    }

    /// 全局已由 deno_websocket 扩展声明（bootstrap.js 挂载）。
    #[tokio::test(flavor = "current_thread")]
    async fn given_runtime_when_probed_then_websocket_global_declared() {
        let out = run(&bridge(), "json.ok(typeof WebSocket === \"function\");")
            .await
            .unwrap();
        assert!(out.contains("true"), "{out}");
    }

    /// 回环：in-process tokio-tungstenite 服务端推一帧，WHATWG 客户端收到。
    /// 钉住 op 链路 + allow_all 权限 + 事件循环驱动。
    #[tokio::test(flavor = "current_thread")]
    async fn given_local_ws_server_when_connected_then_frame_received() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use futures::{SinkExt, StreamExt};
            use tokio_tungstenite::tungstenite::Message;
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            ws.send(Message::text("hello-from-oj")).await.unwrap();
            let _ = ws.next().await; // 悬住连接，客户端读完帧后 close
        });
        let b = bridge();
        let src = format!(
            "(async () => {{
                const ws = new WebSocket(\"ws://{addr}/\");
                const msg = await new Promise((ok, err) => {{
                    ws.onmessage = (e) => ok(e.data);
                    ws.onerror = () => err(new Error(\"ws error\"));
                    ws.onclose = () => err(new Error(\"ws closed\"));
                }});
                ws.close();
                json.ok(String(msg));
            }})()"
        );
        let out = run(&b, &src).await.unwrap();
        assert!(out.contains("hello-from-oj"), "{out}");
    }
}
