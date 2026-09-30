//! 流式响应 / SSE 原语（v0.1.35）。
//!
//! - `json.stream(opts?)`：开流，返回 `{ write, end }`，handler 多次 `write` 逐块推送；
//!   绕过 `{code,msg,data}` 信封（首字节即业务内容）。
//! - `json.sse(opts?)`：`json.stream` 的便捷封装：设 `text/event-stream` 并自动 `data: ..\n\n`
//!   帧化，附带可配置心跳（默认 15s 的 `:\n\n` 注释行，穿透代理保活）。
//!
//! 数据通道（`UnboundedSender<Bytes>`）归属 `ReqState`，在 `read_capture` 时把接收端移入
//! `Capture.stream`，与 isolate 生命周期解耦：isolate 在 handler 返回后即可归还池，由 server
//! 层（axum `Body::from_stream`）继续排空接收端。客户端断开或 `end()` 触发写入端/心跳任务
//! 退出，接收端关闭，流干净终止。

use bytes::Bytes;
use deno_core::{OpState, op2};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::{Notify, mpsc};
use tokio::time::Duration;

use super::ReqState;

/// json.stream/open：开流并按 opts 设 status/content-type；SSE 时挂心跳任务。
/// opts：`{ status?, contentType?, sse?, heartbeatSecs? }`。
#[op2]
pub fn op_json_stream_open(state: &mut OpState, #[serde] opts: Option<Value>) {
    let opts = opts.unwrap_or(Value::Null);
    let status = opts
        .get("status")
        .and_then(|v| v.as_u64())
        .map(|s| s as u16)
        .unwrap_or(200);
    let sse = opts.get("sse").and_then(|v| v.as_bool()).unwrap_or(false);
    let content_type = opts
        .get("contentType")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Bytes>();
    let rs = state.borrow_mut::<ReqState>();
    rs.status = status;
    if let Some(ct) = content_type {
        rs.headers.insert("content-type".into(), ct);
    } else if sse {
        rs.headers
            .insert("content-type".into(), "text/event-stream".into());
    }
    rs.stream_tx.borrow_mut().replace(tx);
    rs.stream_rx.borrow_mut().replace(rx);

    if sse {
        // 心跳：持有 tx 克隆 + 停止信号；任一触发即退出并释放克隆，关流。
        let stop = Arc::new(Notify::new());
        let hb_tx = rs.stream_tx.borrow().as_ref().unwrap().clone();
        let hb_stop = stop.clone();
        let secs = opts
            .get("heartbeatSecs")
            .and_then(|v| v.as_u64())
            .unwrap_or(15);
        rs.stream_heartbeat_stop.borrow_mut().replace(stop);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(secs)) => {
                        if hb_tx.send(Bytes::from_static(b":\n\n")).is_err() {
                            break;
                        }
                    }
                    _ = hb_stop.notified() => break,
                }
            }
        });
    }
}

/// 写入一块（UTF-8 文本）：CSV/JSON/SSE 数据行。流未开或无接收端时静默忽略。
#[op2(fast)]
pub fn op_stream_write(state: &mut OpState, #[string] chunk: String) {
    let tx: Option<mpsc::UnboundedSender<Bytes>> =
        state.borrow_mut::<ReqState>().stream_tx.borrow().clone();
    if let Some(tx) = tx {
        // 客户端已断开时 send 返回 Err，忽略即可（连接清理由 server 层负责）。
        let _ = tx.send(Bytes::from(chunk));
    }
}

/// 显式结束流：丢弃写入端（若 SSE 一并通知心跳退出）。未调用时 `read_capture` 也会丢弃
/// 写入端，流同样在 handler 返回后关闭。
#[op2(fast)]
pub fn op_stream_end(state: &mut OpState) {
    let (tx, stop) = {
        let rs = state.borrow_mut::<ReqState>();
        (
            rs.stream_tx.borrow_mut().take(),
            rs.stream_heartbeat_stop.borrow_mut().take(),
        )
    };
    drop(tx);
    if let Some(s) = stop {
        s.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::{Bridge, InMemoryAccessor, InMemoryKV};
    use std::sync::Arc;
    use tokio::sync::mpsc::UnboundedReceiver;

    #[tokio::test(flavor = "current_thread")]
    async fn json_stream_collects_chunks_into_capture() {
        let b = Bridge::new(
            Arc::new(InMemoryAccessor::new()),
            Arc::new(InMemoryKV::new()),
        );
        let cap = b
            .run(
                r#"
                const s = json.stream({ contentType: "text/csv" });
                s.write("a,b\n");
                s.write("1,2\n");
                s.end();
                "#,
            )
            .await
            .unwrap();
        assert_eq!(cap.status, 200);
        assert_eq!(cap.headers.get("content-type").unwrap(), "text/csv");
        // 缓冲字段在流式路径不使用。
        assert!(cap.body.is_empty());
        let rx = cap.stream.expect("stream should be open");
        let got = collect(rx).await;
        assert_eq!(got, "a,b\n1,2\n");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn json_sse_sets_content_type_and_frames() {
        let b = Bridge::new(
            Arc::new(InMemoryAccessor::new()),
            Arc::new(InMemoryKV::new()),
        );
        // 不调用 end()：read_capture 应自动丢弃 tx 关流（心跳任务经 Notify 退出）。
        let cap = b
            .run(
                r#"
                const s = json.sse();
                s.write("hello");
                s.write("world");
                "#,
            )
            .await
            .unwrap();
        assert_eq!(
            cap.headers.get("content-type").unwrap(),
            "text/event-stream"
        );
        let rx = cap.stream.expect("stream should be open");
        let got = collect(rx).await;
        // 两个 SSE 帧：data: hello\n\n + data: world\n\n（心跳 15s 不在测试中触发）。
        assert_eq!(got, "data: hello\n\ndata: world\n\n");
    }

    /// 收取接收端全部字节（测试辅助）。
    async fn collect(mut rx: UnboundedReceiver<Bytes>) -> String {
        let mut out = String::new();
        while let Some(b) = rx.recv().await {
            out.push_str(std::str::from_utf8(&b).unwrap());
        }
        out
    }
}
