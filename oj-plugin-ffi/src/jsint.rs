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

/// 解码 i64 标记：严格识别「单键对象 + 规范十进制字面量 + 落在 i64 范围内」。
///
/// 严格性的用意是**把误伤面压到最小**：多键对象、非十进制（`"1.0"` / `"+1"` / `" 1"` /
/// `"1e3"`）、前导零（`"007"`）、超出 i64 的值一律返回 `None`（按普通值处理，不绑定为整数）。
pub fn marker_i64(v: &Value) -> Option<i64> {
    let obj = v.as_object()?;
    if obj.len() != 1 {
        return None;
    }
    let s = obj.get(I64_MARKER)?.as_str()?;
    if !is_canonical_decimal(s) {
        return None;
    }
    s.parse::<i64>().ok()
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
}
