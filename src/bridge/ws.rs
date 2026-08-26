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
#[op2(fast)]
pub(crate) fn op_ws_close(state: &mut OpState) {
    state.borrow_mut::<ReqState>().ws_close = true;
}
