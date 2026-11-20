//! NDJSON 帧编解码表驱动测试（design spec §5.3 / 测试计划 §6.1）。

use crate::codec::{FrameError, FrameReader, Line, MAX_LINE, encode_request, parse_response};

/// 分片到达：逐字节 push，行只在 `\n` 齐时吐出。
#[test]
fn fragmented_delivery() {
    let mut r = FrameReader::new();
    let line = br#"{"id":1,"ok":{"handle":1}}"#;
    for (i, b) in line.iter().enumerate() {
        let got = r.push(&[*b]).expect("per-byte push");
        assert!(got.is_empty(), "no newline yet @byte {i}");
    }
    let got = r.push(b"\n").expect("terminator");
    assert_eq!(got.len(), 1);
    assert_eq!(got[0], r#"{"id":1,"ok":{"handle":1}}"#);
}

/// 粘包：一次 push 多行，全部按序吐出。
#[test]
fn coalesced_delivery() {
    let mut r = FrameReader::new();
    let got = r
        .push(b"{\"id\":1,\"ok\":{}}\n{\"id\":2,\"err\":\"boom\"}\n")
        .expect("coalesced");
    assert_eq!(got.len(), 2);
    assert_eq!(got[0], "{\"id\":1,\"ok\":{}}");
    assert_eq!(got[1], "{\"id\":2,\"err\":\"boom\"}");
}

/// 行内残留（无新行尾的分片）留在缓冲，不提前吐也不报错。
#[test]
fn partial_line_stays_buffered() {
    let mut r = FrameReader::new();
    assert!(r.push(b"{\"id\":1").expect("partial").is_empty());
    let got = r.push(b",\"ok\":1}\n").expect("completion");
    assert_eq!(got.len(), 1);
}

/// CRLF：`\r` 是行尾装饰，剥掉。
#[test]
fn crlf_stripped() {
    let mut r = FrameReader::new();
    let got = r.push(b"{\"id\":1,\"ok\":1}\r\n").expect("crlf");
    assert_eq!(got[0], "{\"id\":1,\"ok\":1}");
}

/// 非 UTF8 字节 → FrameError（不 panic、不 lossy 替换——协议损坏显式可见）。
#[test]
fn non_utf8_rejected() {
    let mut r = FrameReader::new();
    assert_eq!(r.push(b"\xff\xfe\n"), Err(FrameError::NotUtf8));
}

/// 64 MiB + 1 的超长行（含换行终止）判 TooLong——T9 据此刻 sidecar 判死。
#[test]
fn over_max_line_too_long() {
    let mut r = FrameReader::new();
    let mut big = vec![b'{'; MAX_LINE + 1];
    big.push(b'\n');
    assert_eq!(r.push(&big), Err(FrameError::TooLong));
    assert_eq!(MAX_LINE, 64 * 1024 * 1024);
}

/// 无换行尾的半行超长同样判 TooLong（不能等一个永不到来的 `\n`）。
#[test]
fn unterminated_over_max_line_too_long() {
    let mut r = FrameReader::with_max(8);
    assert_eq!(r.push(b"123456789"), Err(FrameError::TooLong));
}

/// 小上限边界：恰好上限长度 + `\n` 通过，超限失败。
#[test]
fn max_line_boundary() {
    let mut r = FrameReader::with_max(4);
    assert_eq!(r.push(b"1234\n").expect("at limit").len(), 1);
    let mut r = FrameReader::with_max(4);
    assert_eq!(r.push(b"12345\n"), Err(FrameError::TooLong));
}

/// 出错后 reader 不复用（TooLong 是终态，T9 直接判死 sidecar）。
#[test]
fn reader_poisoned_after_error() {
    let mut r = FrameReader::with_max(4);
    assert!(r.push(b"12345\n").is_err());
}

/// parse_response：ok 帧（value 原样 JSON）。
#[test]
fn parse_ok_response() {
    let resp = parse_response(r#"{"id":7,"ok":{"handle":1}}"#).expect("ok");
    match resp {
        crate::codec::Response::Ok { id, value } => {
            assert_eq!(id, 7);
            assert_eq!(value["handle"], serde_json::json!(1));
        }
        other => panic!("expected Ok, got {other:?}"),
    }
}

/// parse_response：err 帧——字符串形态与结构化形态（§5.8 sql_state/error_code）。
#[test]
fn parse_err_response() {
    use crate::codec::Response;
    match parse_response(r#"{"id":9,"err":"boom"}"#).expect("err") {
        Response::Err {
            id,
            message,
            sql_state,
            error_code,
        } => {
            assert_eq!((id, message.as_str()), (9, "boom"));
            assert_eq!(sql_state, None);
            assert_eq!(error_code, None);
        }
        other => panic!("expected Err, got {other:?}"),
    }
    match parse_response(
        r#"{"id":10,"err":{"message":"conn reset","sql_state":"08S01","error_code":123}}"#,
    )
    .expect("structured err")
    {
        Response::Err {
            id,
            message,
            sql_state,
            error_code,
        } => {
            assert_eq!(id, 10);
            assert_eq!(message, "conn reset");
            assert_eq!(sql_state.as_deref(), Some("08S01"));
            assert_eq!(error_code, Some(123));
        }
        other => panic!("expected Err, got {other:?}"),
    }
}

/// 未知键一律忽略（协议演进契约）：顶层与 ok/err 载荷内的未知键都不影响解析。
#[test]
fn unknown_keys_ignored() {
    use crate::codec::Response;
    match parse_response(r#"{"id":1,"ok":{"a":1},"future":"x","v":99}"#).expect("ok") {
        Response::Ok { value, .. } => {
            assert_eq!(value["a"], serde_json::json!(1));
            assert!(!value.as_object().unwrap().contains_key("future"));
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    match parse_response(r#"{"id":1,"err":{"message":"m","trace":"t"}}"#).expect("err") {
        Response::Err { message, .. } => assert_eq!(message, "m"),
        other => panic!("expected Err, got {other:?}"),
    }
}

/// 坏行（非 JSON / 缺 id）→ FrameError，不 panic。
#[test]
fn malformed_response_rejected() {
    assert!(parse_response("not json").is_err());
    assert!(parse_response(r#"{"ok":1}"#).is_err());
    assert!(parse_response(r#"{"id":1}"#).is_err()); // 既无 ok 也无 err
}

/// encode_request：单行 JSON，id/op/params 就位，行尾 `\n` 由编码器保证。
#[test]
fn encode_request_roundtrip() {
    let mut params = serde_json::Map::new();
    params.insert("url".into(), serde_json::json!("jdbc:dm://h:5237"));
    params.insert("user".into(), serde_json::json!("u"));
    let line: Line = encode_request(1, "connect", &params);
    assert!(line.ends_with('\n'), "NDJSON terminator included");
    assert!(!line[..line.len() - 1].contains('\n'), "single line");
    let v: serde_json::Value = serde_json::from_str(line.trim_end()).expect("valid json");
    assert_eq!(v["id"], serde_json::json!(1));
    assert_eq!(v["op"], serde_json::json!("connect"));
    assert_eq!(v["url"], serde_json::json!("jdbc:dm://h:5237"));
    // 编码产物能过自己的解码器与解析器（协议自洽）。
    let mut r = FrameReader::new();
    let frames = r.push(line.as_bytes()).expect("self-decodable");
    assert_eq!(frames.len(), 1);
    // 请求帧无 ok/err 键——parse_response 拒收，协议方向不自混。
    assert!(parse_response(&frames[0]).is_err());
}

/// 非 JSON 长行含多字节码点：bad() 截断按 char 切，不得在码点中间 panic
/// （sidecar 是不可信输入源，不 panic 是红线）。
#[test]
fn malformed_multibyte_line_no_panic() {
    let line = format!("{}🀄", "中文".repeat(40)); // >64 字节，多字节码点
    match parse_response(&line) {
        Err(FrameError::Json(m)) => assert!(m.contains("中文"), "snippet kept"),
        other => panic!("expected Json error, got {other:?}"),
    }
}
