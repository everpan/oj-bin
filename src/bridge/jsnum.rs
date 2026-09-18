//! JS 数值边界护栏（v0.1.22）：超界整数降为十进制字符串。
//!
//! serde_v8 把 `|i64| > 2^53-1` 的整数序列化为 **v8 BigInt**（`serde_v8-0.320.0/ser.rs:402,441-444`），
//! 而 BigInt 无法 `JSON.stringify` —— `json.ok({ rows })` 会直接 500
//! （`TypeError: Do not know how to serialize a BigInt`）。故在所有**面向 JS 的 op 出口**
//! 把这类整数降为十进制字符串：无损、JSON 安全、下游可见。
//!
//! 放在 op 出口而非 `column_json`（行解码）有两处考虑：
//! ① Rust 内部消费者（migrate / seed / schema diff）读的是 accessor 原始行值，不受影响；
//! ② 不必改两个 cdylib 插件（行解码逻辑在 sqlite accessor 与 mysql/postgres 插件各有一份）。
//!
//! 配套的写侧通道见 `docs/numeric-limits.md`（`toBigInt()` + `$oj$i64` 标记）。
//!
//! **op 出口覆盖清单**（新增「返回 JSON / 行集」的 op 时照此对账）：
//!
//! 已覆盖（值来自 DB 或外部，必须过护栏）：
//! - `db.rs::op_db_query`（行集；tx 路径同一 op）
//! - `query.rs::op_db_query_build`（行集 / mysql `LAST_INSERT_ID` 合成行 / 受影响行数）
//! - `query.rs::op_db_query_sql`（`toSQL()` 的 `params`）
//! - `es.rs::op_es_search`（`_source` 里的 long）
//! - `crypto.rs::op_jwt_verify`（**外部可控**的 claims）
//! - `mail.rs::op_mail_result`（插件侧结果体）
//!
//! 有意不覆盖（量级不可达或无用户数据，不值得加分支）：
//! - `db.rs::op_db_exec` 的受影响行数（需 ≥ 2^53 行受影响）
//! - `es.rs::op_es_index` / `op_es_del`（响应只有 `_version` / `_seq_no` 等小整数）
//! - `jwt.accessDuration` / `refreshDuration`、`cert.*`（固定小整数）
//! - `kv.incr`（返回 f64，**> 2^53 时先丢精度**——属 kv 语义，见 `numeric-limits.md` §4）

use serde_json::Value;

/// 与 serde_v8 的 `MAX_SAFE_INTEGER` **逐字一致**（`(1 << 53) - 1`）。
///
/// 不可用 `2^53` 当上界：`2^53` 本身已不可安全表示，serde_v8 同样给它 BigInt ——
/// 写成 `> 2^53` 会让 `2^53` 漏转，读该值仍 500。
pub const MAX_SAFE_INT: i64 = (1 << 53) - 1;

/// 递归把超出安全范围的整数原地替换为十进制字符串；对象/数组下钻。
///
/// - `|i64| > 2^53-1` → 字符串（含 `i64::MIN`/`i64::MAX`）；
/// - `u64 > i64::MAX`（serde_json 的 `PosInt` 上界）→ 字符串（`as_i64` 为 None，走 u64 分支）；
/// - f64（REAL/DOUBLE 列）**不动**：它就是 JS number 语义，精度由列类型决定；
/// - 安全范围内的整数、字符串、bool、null 不动。
pub fn sanitize_js_numbers(v: &mut Value) {
    match v {
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                if !(-MAX_SAFE_INT..=MAX_SAFE_INT).contains(&i) {
                    *v = Value::String(i.to_string());
                }
            } else if let Some(u) = n.as_u64()
                && u > MAX_SAFE_INT as u64
            {
                *v = Value::String(u.to_string());
            }
        }
        Value::Array(a) => a.iter_mut().for_each(sanitize_js_numbers),
        Value::Object(o) => o.values_mut().for_each(sanitize_js_numbers),
        _ => {}
    }
}

/// 行集合批量 sanitize（`Vec<Row>` / 数组结果）。
pub fn sanitize_rows(rows: &mut [Value]) {
    rows.iter_mut().for_each(sanitize_js_numbers);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn safe_range_integers_are_untouched() {
        let mut v = json!({
            "zero": 0, "neg": -42, "max": MAX_SAFE_INT, "min": -MAX_SAFE_INT,
            "f": 1.5, "s": "9007199254740993", "b": true, "n": null
        });
        let before = v.clone();
        sanitize_js_numbers(&mut v);
        assert_eq!(v, before, "安全范围/非整数一律不动");
        // 2^53 与 -(2^53-1) 的边界：MAX_SAFE_INT 保留，MAX_SAFE_INT+1 转字符串。
        assert_eq!(v["max"], json!(9007199254740991i64));
        assert_eq!(v["min"], json!(-9007199254740991i64));
    }

    #[test]
    fn out_of_range_integers_become_decimal_strings() {
        // 阈值必须与 serde_v8 一致：2^53（= MAX_SAFE_INT+1）起就转 —— 写成 `> 2^53` 会漏掉它。
        let mut v = json!({
            "p53": 9007199254740992i64,
            "p53p1": 9007199254740993i64,
            "n53": -9007199254740992i64,
            "imax": i64::MAX,
            "imin": i64::MIN,
        });
        sanitize_js_numbers(&mut v);
        assert_eq!(v["p53"], json!("9007199254740992"));
        assert_eq!(v["p53p1"], json!("9007199254740993"));
        assert_eq!(v["n53"], json!("-9007199254740992"));
        assert_eq!(v["imax"], json!("9223372036854775807"));
        assert_eq!(v["imin"], json!("-9223372036854775808"));
    }

    #[test]
    fn nested_structures_and_rows_are_walked() {
        let mut v = json!({
            "rows": [{"id": 9007199254740993i64, "tags": [9223372036854775807i64, 1]}],
            "total": 9007199254740992i64
        });
        sanitize_js_numbers(&mut v);
        assert_eq!(v["rows"][0]["id"], json!("9007199254740993"));
        assert_eq!(v["rows"][0]["tags"][0], json!("9223372036854775807"));
        assert_eq!(v["rows"][0]["tags"][1], json!(1));
        assert_eq!(v["total"], json!("9007199254740992"));

        let mut rows = vec![json!({"id": 4611686018427387905i64})];
        sanitize_rows(&mut rows);
        assert_eq!(rows[0]["id"], json!("4611686018427387905"));
    }

    #[test]
    fn u64_above_i64_max_does_not_wrap_negative() {
        // serde_json 的 PosInt 可到 u64::MAX；旧实现 `as i64` 会回绕成负数（静默错值）。
        let mut v = json!({ "u": u64::MAX });
        sanitize_js_numbers(&mut v);
        assert_eq!(v["u"], json!("18446744073709551615"));
    }
}
