//! JS ↔ 宿主的大整数参数编码（v0.1.22；宿主与 db 插件共用，故落在共享契约 crate）。
//!
//! **为什么需要标记**：JS 的 `number` 只有 f64 精度，64 位整数只能以 `BigInt` 表达；
//! 而 `serde_v8` 反序列化遇到 BigInt 直接报 `unsupported type`（`de.rs:160`
//! `ValueType::BigInt => Err(UnsupportedType)`），且其 magic/transl8 trait 是 `pub(crate)`，
//! 外部无法自定义能处理 BigInt 的 serde 类型。故由 JS 侧（`bootstrap.js` 的 `encodeParams`）
//! 把 BigInt 编码为本模块约定的**保留形状**，宿主/插件在绑定参数时解码为 `i64`。
//!
//! **为什么不能靠字符串**：`toBigInt()` 若退化为字符串，PostgreSQL 会拒绝
//! （`column "x" is of type bigint but expression is of type text`）——字符串是**文本意图**，
//! BigInt 是**整数意图**，两者不可混同（详见 `docs/numeric-limits.md`）。
//!
//! **保留形状**：`{"$oj$i64": "<十进制>"}` —— 单键对象。业务数据若出现完全同形的单键对象
//! 且位于**参数位置**，会被当作整数绑定；这是本约定的已知边界（业务侧避免以此键为唯一键）。

use serde_json::Value;

/// 大整数参数的保留键（`toBigInt()` 的返回值跨 op 时的编码形状）。
pub const I64_MARKER: &str = "$oj$i64";

/// 无符号 64 位参数的保留键（v0.1.24，`toUBigInt()` 的返回值跨 op 时的编码形状）。
///
/// 为什么需要独立标记：`$oj$i64` 绑 i64，`(i64::MAX, u64::MAX]` 的值装不进去；而 MySQL 的
/// `BIGINT UNSIGNED` 列正是这一段值域。语义分派由宿主/插件按方言做——
/// - MySQL：绑 `u64`（`sqlx-mysql` 原生支持，且是唯一能精确承载 unsigned 的方言）；
/// - PG / SQLite：bigint 就是 i64，遇到本标记**明确报错**（而不是静默坍缩/回绕）。
///
/// 与 `I64_MARKER` 一样属**源码级共享**（不在任何 `#[repr(C)]` 结构里）→ 新增标记不 bump
/// `ABI_VERSION`。代价：旧插件遇到本标记会走 `bind_value` 的 `other => to_string()` 落成文本，
/// 故**插件须与宿主同批重建**。
pub const U64_MARKER: &str = "$oj$u64";

/// 解码 i64 标记：严格识别「单键对象 + 规范十进制字面量 + 落在 i64 范围内」。
///
/// 严格性的用意是**把误伤面压到最小**：多键对象、非十进制（`"1.0"` / `"+1"` / `" 1"` /
/// `"1e3"`）、前导零（`"007"`）、超出 i64 的值一律返回 `None`（按普通值处理，不绑定为整数）。
pub fn marker_i64(v: &Value) -> Option<i64> {
    let s = marker_str(v, I64_MARKER)?;
    s.parse::<i64>().ok()
}

/// 解码 u64 标记：判据同 `marker_i64`，范围放宽到 `u64`。
///
/// 注意 (i64::MAX, u64::MAX] 这一段**只有** u64 标记能承载：`$oj$i64` 的值域判定会让它落回
/// `None`（按普通值处理），故两者是互斥的、不会互相误认。
pub fn marker_u64(v: &Value) -> Option<u64> {
    let s = marker_str(v, U64_MARKER)?;
    s.parse::<u64>().ok()
}

/// 取标记对象里的十进制串（单键 + 规范十进制），不做范围判定——范围由各 `marker_*` 决定。
fn marker_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    let obj = v.as_object()?;
    if obj.len() != 1 {
        return None;
    }
    let s = obj.get(key)?.as_str()?;
    if !is_canonical_decimal(s) {
        return None;
    }
    Some(s)
}

/// 明确拒绝 `$oj$u64` 参数（方言/驱动不支持精确 u64 时），`why` 给出出路。
///
/// 为什么单独扫一遍而不在 `bind_value` 里判：`bind_value` 的形态是「`Query` 进 `Query` 出」，
/// 加 `Result` 会改动 4 个执行点的签名；预扫描只在这几处调用一次，改动面更小。
/// 返回 `Err` 而不是静默坍缩/落成文本——本仓的既定纪律是「宁可 fail loud，不可静默错值」。
pub fn reject_u64_markers(params: &[Value], why: &str) -> Result<(), String> {
    for p in params {
        if let Some(u) = marker_u64(p) {
            return Err(format!(
                "db param: u64 value {u} is not supported on this path — {why}"
            ));
        }
    }
    Ok(())
}

/// 规范十进制整数串：可带一个负号，无前导零（`"0"` 本身除外），无 `-0`。
fn is_canonical_decimal(s: &str) -> bool {
    let digits = match s.strip_prefix('-') {
        Some(rest) => {
            if rest == "0" {
                return false; // "-0" 非规范
            }
            rest
        }
        None => s,
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    digits == "0" || !digits.starts_with('0')
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn decodes_well_formed_markers() {
        assert_eq!(marker_i64(&json!({ "$oj$i64": "0" })), Some(0));
        assert_eq!(
            marker_i64(&json!({ "$oj$i64": "4886674138783273204" })),
            Some(4886674138783273204)
        );
        assert_eq!(
            marker_i64(&json!({ "$oj$i64": "-9223372036854775808" })),
            Some(i64::MIN)
        );
        assert_eq!(
            marker_i64(&json!({ "$oj$i64": "9223372036854775807" })),
            Some(i64::MAX)
        );
    }

    #[test]
    fn rejects_non_canonical_or_oversized_markers() {
        // 非十进制 / 非规范字面量
        for bad in [
            "1.0", "+1", " 1", "1 ", "1e3", "007", "-0", "", "0x10", "abc",
        ] {
            assert_eq!(
                marker_i64(&json!({ "$oj$i64": bad })),
                None,
                "{bad:?} 不应被识别"
            );
        }
        // 超出 i64
        assert_eq!(
            marker_i64(&json!({ "$oj$i64": "9223372036854775808" })),
            None
        );
        assert_eq!(
            marker_i64(&json!({ "$oj$i64": "-9223372036854775809" })),
            None
        );
        // 非字符串值 / 非对象 / 多键对象
        assert_eq!(marker_i64(&json!({ "$oj$i64": 1 })), None);
        assert_eq!(marker_i64(&json!("$oj$i64")), None);
        assert_eq!(marker_i64(&json!({ "$oj$i64": "1", "x": 2 })), None);
        assert_eq!(marker_i64(&json!({ "other": "1" })), None);
    }

    #[test]
    fn decodes_u64_marker_and_stays_disjoint_from_i64() {
        assert_eq!(marker_u64(&json!({ "$oj$u64": "0" })), Some(0));
        assert_eq!(
            marker_u64(&json!({ "$oj$u64": "9223372036854775808" })),
            Some(9223372036854775808)
        );
        assert_eq!(
            marker_u64(&json!({ "$oj$u64": "18446744073709551615" })),
            Some(u64::MAX)
        );
        // 越 u64 / 负号（u64 标记不接受负数）
        assert_eq!(
            marker_u64(&json!({ "$oj$u64": "18446744073709551616" })),
            None
        );
        assert_eq!(marker_u64(&json!({ "$oj$u64": "-1" })), None);
        // 规范性同 i64：非十进制 / 前导零 / 多键 / 非单键对象一律不认
        for bad in ["1.0", "+1", " 1", "007", "abc"] {
            assert_eq!(marker_u64(&json!({ "$oj$u64": bad })), None, "{bad:?}");
        }
        assert_eq!(marker_u64(&json!({ "$oj$u64": "1", "x": 2 })), None);
        assert_eq!(marker_u64(&json!({ "$oj$i64": "1" })), None);
        // 两个标记互不误认（键名不同 + 值域判定各自独立）
        assert_eq!(marker_i64(&json!({ "$oj$u64": "1" })), None);
        assert_eq!(marker_u64(&json!({ "$oj$i64": "1" })), None);
        assert_eq!(
            marker_u64(&json!({ "$oj$i64": "9223372036854775808" })),
            None
        );
    }
}
