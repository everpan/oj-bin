//! {code,msg,data} 统一信封。
//!
//! 错误映射（FromError/HTTPError 语义）由 server 层负责。

use serde_json::Value;

/// OK 返回成功信封（code=0,msg=ok）。单遍序列化：不构中间 Value 树，直接写 buffer。
pub fn ok(data: &Value) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(br#"{"code":0,"msg":"ok","data":"#);
    serde_json::to_writer(&mut buf, data).expect("envelope marshal");
    buf.push(b'}');
    buf
}

/// OK 信封，`data` 为 JS 侧 `JSON.stringify` 已序列化的 JSON 文本（直接拼接到信封，
/// 避免 serde_v8 反序列化再 serde_json 序列化的双重开销）。
pub fn ok_raw(data_json: &str) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64 + data_json.len());
    buf.extend_from_slice(br#"{"code":0,"msg":"ok","data":"#);
    buf.extend_from_slice(data_json.as_bytes());
    buf.push(b'}');
    buf
}

/// OK 信封扩展：在 `data` 之后追加一个 `_sql` 兄弟字段（已序列化的 JSON 文本）。
/// 仅 dev SQL 追踪开启时由 `op_json_ok` 调用，把本请求画像附进信封（生产追踪关闭则走 `ok_raw`）。
pub fn ok_raw_ext(data_json: &str, sql_json: &str) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64 + data_json.len() + sql_json.len());
    buf.extend_from_slice(br#"{"code":0,"msg":"ok","data":"#);
    buf.extend_from_slice(data_json.as_bytes());
    buf.extend_from_slice(br#","_sql":"#);
    buf.extend_from_slice(sql_json.as_bytes());
    buf.push(b'}');
    buf
}

/// 业务 code → HTTP 状态（正数直取；>u16::MAX clamp 到 u16::MAX——`as u16` 强转会
/// 静默回绕成错值，如 70000 → 4464）。
fn clamp_status(code: i32) -> u16 {
    u16::try_from(code).unwrap_or(u16::MAX)
}

/// Fail 返回失败信封，并返回应映射的 HTTP 状态码（code<=0 默认 500）。
pub fn fail(code: i32, msg: &str, data: &Value) -> (Vec<u8>, u16) {
    let code = if code <= 0 { 500 } else { code };
    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(br#"{"code":"#);
    serde_json::to_writer(&mut buf, &code).expect("envelope marshal");
    buf.extend_from_slice(br#","msg":"#);
    serde_json::to_writer(&mut buf, msg).expect("envelope marshal");
    buf.extend_from_slice(br#","data":"#);
    serde_json::to_writer(&mut buf, data).expect("envelope marshal");
    buf.push(b'}');
    (buf, clamp_status(code))
}

/// StatusCode 将业务 code 映射为 HTTP 状态码（code<=0 → 200）。
pub fn status_code(code: i32) -> u16 {
    if code <= 0 { 200 } else { clamp_status(code) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ok_and_fail_envelopes() {
        let v: Value = serde_json::from_slice(&ok(&json!({"a": 1}))).unwrap();
        assert_eq!(v, json!({"code": 0, "msg": "ok", "data": {"a": 1}}));

        let (body, status) = fail(0, "boom", &Value::Null);
        assert_eq!(status, 500);
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["code"], 500);

        assert_eq!(status_code(0), 200);
        assert_eq!(status_code(404), 404);
    }

    /// 超界业务 code 不得 `as u16` 回绕（70000 → 4464 是错值）：HTTP 状态 clamp 到
    /// u16::MAX，信封 body 保留全精度业务 code。
    #[test]
    fn oversized_codes_clamp_instead_of_wrap() {
        assert_eq!(status_code(70_000), u16::MAX);
        assert_eq!(status_code(65_535), 65_535);
        assert_eq!(status_code(i32::MAX), u16::MAX);
        let (body, status) = fail(70_000, "boom", &Value::Null);
        assert_eq!(status, u16::MAX);
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["code"], 70_000);
    }
}
