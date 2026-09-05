//! json.ok/fail/header 绑定。

use deno_core::{OpState, op2};

use super::{ReqState, envelope};

/// json.ok/fail 默认补 content-type: application/json；JS 侧 json.header 已显式设置的优先（大小写不敏感）。
fn ensure_json_content_type(s: &mut ReqState) {
    if !s
        .headers
        .keys()
        .any(|k| k.eq_ignore_ascii_case("content-type"))
    {
        s.headers
            .insert("content-type".into(), "application/json".into());
    }
}

/// json.ok(data)：写成功信封（status=200）并标记会话完成。
/// data 由 JS 侧 JSON.stringify 为 JSON 文本传入（fast op，避免 serde_v8 反序列化 + 二次序列化）。
#[op2(fast)]
pub fn op_json_ok(state: &mut OpState, #[string] data_json: String) {
    let s = state.borrow_mut::<ReqState>();
    s.response = Some(envelope::ok_raw(&data_json));
    s.status = 200;
    ensure_json_content_type(s);
    s.done = true;
}

/// json.fail(code, msg, data?)：写失败信封，code<=0 映射 500。
#[op2]
pub fn op_json_fail(
    state: &mut OpState,
    code: i32,
    #[string] msg: String,
    #[serde] data: serde_json::Value,
) {
    let s = state.borrow_mut::<ReqState>();
    let (body, status) = envelope::fail(code, &msg, &data);
    s.response = Some(body);
    s.status = status;
    ensure_json_content_type(s);
    s.done = true;
}

/// json.header(name, value)：设置返回头（覆盖语义：同名后写覆盖先写），空名忽略。
#[op2(fast)]
pub fn op_json_header(state: &mut OpState, #[string] name: String, #[string] value: String) {
    if name.is_empty() {
        return;
    }
    state.borrow_mut::<ReqState>().headers.insert(name, value);
}

/// json.raw(data)：裸 JSON 200（无信封）。OP 对外端点说标准 OIDC JSON 用。
/// 同 ok/fail：未显式设置 content-type 时默认补 application/json。
#[op2(fast)]
pub fn op_json_raw(state: &mut OpState, #[string] data_json: String) {
    let s = state.borrow_mut::<ReqState>();
    s.response = Some(data_json.into_bytes());
    s.status = 200;
    ensure_json_content_type(s);
    s.done = true;
}

#[cfg(test)]
mod tests {
    use crate::bridge::{Bridge, InMemoryAccessor, InMemoryKV};
    use serde_json::Value;
    use std::sync::Arc;

    #[tokio::test(flavor = "current_thread")]
    async fn json_raw_writes_bare_body_with_200() {
        let b = Bridge::new(
            Arc::new(InMemoryAccessor::new()),
            Arc::new(InMemoryKV::new()),
        );
        let cap = b
            .run_with(
                r#"json.raw({ issuer: "x", bare: true });"#,
                crate::bridge::RequestInfo::default(),
            )
            .await
            .unwrap();
        assert_eq!(cap.status, 200);
        // 未显式 json.header 时默认补 content-type（同 ok/fail 语义）。
        assert_eq!(cap.headers.get("content-type").unwrap(), "application/json");
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["bare"], true);
        assert!(v.get("code").is_none());
    }

    /// json.header 显式设置的 content-type 优先，不被默认值覆盖。
    #[tokio::test(flavor = "current_thread")]
    async fn json_raw_keeps_explicit_content_type() {
        let b = Bridge::new(
            Arc::new(InMemoryAccessor::new()),
            Arc::new(InMemoryKV::new()),
        );
        let cap = b
            .run_with(
                r#"json.header("Content-Type", "application/jwt"); json.raw({ a: 1 });"#,
                crate::bridge::RequestInfo::default(),
            )
            .await
            .unwrap();
        assert_eq!(cap.headers.get("Content-Type").unwrap(), "application/jwt");
        assert!(!cap.headers.contains_key("content-type"));
    }
}
