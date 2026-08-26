//! json.ok/fail/header 绑定。

use deno_core::{OpState, op2};

use super::{ReqState, envelope};

/// json.ok(data)：写成功信封（status=200）并标记会话完成。
/// data 由 JS 侧 JSON.stringify 为 JSON 文本传入（fast op，避免 serde_v8 反序列化 + 二次序列化）。
#[op2(fast)]
pub fn op_json_ok(state: &mut OpState, #[string] data_json: String) {
    let s = state.borrow_mut::<ReqState>();
    s.response = Some(envelope::ok_raw(&data_json));
    s.status = 200;
    s.done = true;
}

/// json.fail(code, msg, data?)：写失败信封，code<=0 映射 500。
#[op2]
pub fn op_json_fail(state: &mut OpState, code: i32, #[string] msg: String, #[serde] data: serde_json::Value) {
    let s = state.borrow_mut::<ReqState>();
    let (body, status) = envelope::fail(code, &msg, &data);
    s.response = Some(body);
    s.status = status;
    s.done = true;
}

/// json.header(name, value)：设置返回头（覆盖语义：同名后写覆盖先写），空名忽略。
#[op2(fast)]
pub fn op_json_header(state: &mut OpState, #[string] name: String, #[string] value: String) {
    if name.is_empty() {
        return;
    }
    state
        .borrow_mut::<ReqState>()
        .headers
        .insert(name, value);
}
