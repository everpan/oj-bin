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
    (buf, code as u16)
}

/// StatusCode 将业务 code 映射为 HTTP 状态码（code<=0 → 200）。
pub fn status_code(code: i32) -> u16 {
    if code <= 0 { 200 } else { code as u16 }
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
}
