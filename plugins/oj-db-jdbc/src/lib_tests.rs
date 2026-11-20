//! vtable 胶水的纯函数用例：jsint 大整数通道与参数预处理。
//! 帧序列 / 在途 close / 世代失效的集成用例在 tests/vtable.rs（假 java 断言）。
use super::*;
use serde_json::json;

#[test]
fn prep_params_passes_plain_values_through() {
    let out = prep_params(r#"[1,1.5,"a",true,null,{"k":[1,2]}]"#).expect("plain");
    assert_eq!(out, json!([1, 1.5, "a", true, null, {"k": [1, 2]}]));
}

#[test]
fn prep_params_i64_marker_becomes_number() {
    // i64 可直接过协议（JSON 数字 → Java Integer/Long → setLong）。
    assert_eq!(
        prep_params(r#"[{"$oj$i64":"-9223372036854775808"}]"#).unwrap(),
        json!([-9223372036854775808i64])
    );
    assert_eq!(
        prep_params(r#"[{"$oj$i64":"9223372036854775807"}]"#).unwrap(),
        json!([9223372036854775807i64])
    );
}

#[test]
fn prep_params_u64_marker_becomes_prefixed_decimal_string() {
    // 协议约定：u64 以 "$oj$u64:<十进制>" 字符串传 Java，RowCodec 剥前缀 setBigDecimal。
    assert_eq!(
        prep_params(r#"[{"$oj$u64":"18446744073709551615"}]"#).unwrap(),
        json!(["$oj$u64:18446744073709551615"])
    );
    // i64 段内的 u64 标记同样走字符串通道（协议形态与值域无关）。
    assert_eq!(
        prep_params(r#"[{"$oj$u64":"42"}]"#).unwrap(),
        json!(["$oj$u64:42"])
    );
}

#[test]
fn prep_params_bare_u64_beyond_i64_gets_prefixed() {
    // BigInteger 回绕防护：JS 直传超 i64 大整数（无 marker），org.json 在 Java 侧
    // 解析成 BigInteger，setLong 会静默回绕——Rust 侧预先字符串化（带 $oj$u64: 前缀）。
    assert_eq!(
        prep_params(r#"[9223372036854775808]"#).unwrap(),
        json!(["$oj$u64:9223372036854775808"])
    );
    assert_eq!(
        prep_params(r#"[18446744073709551615]"#).unwrap(),
        json!(["$oj$u64:18446744073709551615"])
    );
    // i64 值域内裸整数不动（数字形态直达）。
    assert_eq!(
        prep_params(r#"[9223372036854775807]"#).unwrap(),
        json!([9223372036854775807u64])
    );
    assert_eq!(
        prep_params(r#"[-9223372036854775808]"#).unwrap(),
        json!([-9223372036854775808i64])
    );
}

#[test]
fn prep_params_rejects_bad_input() {
    assert!(prep_params("not json").is_err());
    assert!(prep_params("{}").is_err(), "对象非数组");
    assert!(prep_params(r#"{"a":1}"#).is_err());
    assert!(prep_params(r#"[1],"#).is_err());
}
