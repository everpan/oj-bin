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
/// data 由 JS 侧 `ojStringify` 序列化后传入（BigInt 安全，v0.1.22；serde_v8 会把超界
/// 整数转成 BigInt 而 `#[serde] Value` 反序列化直接报 unsupported type）。
#[op2(fast)]
pub fn op_json_fail(
    state: &mut OpState,
    code: i32,
    #[string] msg: String,
    #[string] data_json: String,
) {
    // 非法 JSON 回退 null（JS 侧已是 JSON.stringify 产物，正常不会走到）。
    let data: serde_json::Value =
        serde_json::from_str(&data_json).unwrap_or(serde_json::Value::Null);
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

    /// BigInt 容忍面（v0.1.22）：信封/裸 JSON 里的 BigInt 一律序列化为十进制字符串。
    /// 旧行为是 `JSON.stringify(1n)` 抛 TypeError → 任何含大整数的响应直接 500。
    /// 读取路径的超大整数已由 `jsnum` 降为字符串；这里覆盖 JS 侧自造 BigInt
    /// （`toBigInt()` / `BigInt()`）—— `toBigInt` 的返回值必然经这些边界出去。
    #[tokio::test(flavor = "current_thread")]
    async fn bigint_in_envelope_serializes_as_decimal_string() {
        let b = Bridge::new(
            Arc::new(InMemoryAccessor::new()),
            Arc::new(InMemoryKV::new()),
        );
        // ok：嵌套 + 兄弟字段不受影响
        let cap = b
            .run(r#"json.ok({ id: 4886674138783273204n, n: 1, arr: [9007199254740993n] });"#)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(v["data"]["id"], Value::from("4886674138783273204"));
        assert_eq!(v["data"]["arr"][0], Value::from("9007199254740993"));
        assert_eq!(v["data"]["n"], 1);

        // raw：裸 JSON 200 同款
        let cap = b
            .run(r#"json.raw({ id: 9007199254740993n });"#)
            .await
            .unwrap();
        assert_eq!(cap.status, 200);
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["id"], Value::from("9007199254740993"));

        // fail：data 现走 JS 侧 stringify + `#[string]`（原 `#[serde]` 会 unsupported type）
        let cap = b
            .run(r#"json.fail(400, "bad", { id: 9223372036854775807n });"#)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 400);
        assert_eq!(v["msg"], "bad");
        assert_eq!(v["data"]["id"], Value::from("9223372036854775807"));

        // log 结构化字段同理（不得因 BigInt 抛错）
        let cap = b
            .run(r#"log.info("m", "id", 1n); json.ok({});"#)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");

        // 两个转换助手在运行时（HTTP 池 / 任务池 / `oj test` 共用 bridge_ext）可用
        let cap = b
            .run(r#"json.ok({ tb: typeof toBigInt, td: typeof toDouble, tf: typeof toFloat });"#)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["data"]["tb"], "function");
        assert_eq!(v["data"]["td"], "function");
        assert_eq!(v["data"]["tf"], "undefined", "不做 toFloat（无独立语义）");
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
