//! 降级单元格的保留编码（REQ-07a）：`$<type>:<base64>` 前缀标量。
//!
//! **为什么落在共享契约 crate**：编码方是 db 插件 cdylib（postgres/mysql），扫描/判定方是
//! 宿主——grammar 必须单点定义，否则两端漂移即契约破裂（第二方言知识源 = 漂移，同
//! introspect.rs 守卫注释的铁律）。
//!
//! **为什么是标量不是对象**：bridge 既有惯例是「非标量值 → 纯字符串」（JSON/JSONB →
//! 文本串、NUMERIC → 十进制串，见 `docs/numeric-limits.md`）；标量让 CSV/export 直通，
//! 消费侧无 typeof 分叉。设计：`docs/superpowers/specs/2026-10-30-dbx-gap-triage-design.md`。
//!
//! **判别口径**：是否降级由**列声明类型**驱动（type-driven），本模块只负责编码/识别，
//! 不做内容嗅探。语句级权威清单由宿主 `degraded_of` 扫描给出——TEXT 列字面量撞前缀时
//! 以清单为准（api-manual 写明）。
//!
//! **与 jsint 标记的边界**：`$oj$i64`/`$oj$u64` 是**参数位置**的保留形状（无冒号、含
//! `$`），type 字符集 `[a-z0-9_]` 天然拒绝之——`decode` 不会误判。

use base64::Engine;

/// type 段字符集（小写方言类型名）。
fn type_ok(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'
}

/// 编码：`$<小写 type>:<base64(STANDARD)>`。
///
/// 跨 FFI 绝不 panic：非法字符（空格/`.`/`-`/`$`…）就地替换为 `_`。调用方传
/// `col.type_info().name()`（如 `"INET"`、`"GEOMETRY"`）即可。
pub fn encode(type_name: &str, bytes: &[u8]) -> String {
    let t: String = type_name
        .chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if type_ok(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!(
        "${t}:{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}

/// 识别信封：命中返回 `(type, payload)`；非信封返回 `None`。
/// 拒绝面：无前导 `$`、type 段空/含非法字符、无 `:`、type 段再含 `$`（jsint 标记）。
pub fn decode(s: &str) -> Option<(&str, &str)> {
    let rest = s.strip_prefix('$')?;
    let (t, payload) = rest.split_once(':')?;
    if t.is_empty() || !t.chars().all(type_ok) {
        return None;
    }
    Some((t, payload))
}

/// 语句级降级清单（权威）：扫所有行所有字符串值，`decode` 命中收 `(col, type)`，
/// 按 `{col,type}` 去重（保首见序），输出 `{"col":…,"type":…}` 数组。非对象行跳过。
pub fn degraded_of(rows: &[serde_json::Value]) -> Vec<serde_json::Value> {
    let mut out: Vec<serde_json::Value> = Vec::new();
    for row in rows {
        let Some(obj) = row.as_object() else { continue };
        for (col, val) in obj {
            let Some(s) = val.as_str() else { continue };
            if let Some((t, _)) = decode(s) {
                let item = serde_json::json!({"col": col, "type": t});
                if !out.contains(&item) {
                    out.push(item);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_lowercases_and_wraps() {
        assert_eq!(encode("INET", &[1, 2]), "$inet:AQI=");
    }

    #[test]
    fn encode_never_panics_and_sanitizes() {
        // 跨 FFI 纪律：畸形 type 名（含 $/. / 空格/大写）不 panic，非法字符落 _。
        let s = encode("IN$ET.X A", &[0xff]);
        assert!(s.starts_with("$in_et_x_a:"), "{s}");
    }

    #[test]
    fn decode_roundtrip_and_rejections() {
        assert_eq!(decode("$inet:AQI="), Some(("inet", "AQI=")));
        assert_eq!(decode("plain"), None);
        assert_eq!(decode("$oj$i64"), None); // jsint 标记无冒号且含 $
        assert_eq!(decode("$IN ET:x"), None); // 非法字符
        assert_eq!(decode("$inet"), None); // 无冒号
        assert_eq!(decode("$:x"), None); // 空 type
    }

    #[test]
    fn degraded_of_dedups_by_col_and_type() {
        let rows = vec![
            serde_json::json!({"ip": "$inet:AQI=", "name": "ok"}),
            serde_json::json!({"ip": "$inet:AgM=", "name": "$inet:not-really"}),
            serde_json::json!({"g": "$geometry:AAAA"}),
            serde_json::json!([1, 2, 3]),          // 非对象行跳过
            serde_json::json!({"n": 42, "z": null}), // 非字符串跳过
        ];
        let d = degraded_of(&rows);
        assert_eq!(
            serde_json::Value::Array(d),
            serde_json::json!([
                {"col": "ip", "type": "inet"},
                {"col": "name", "type": "inet"},
                {"col": "g", "type": "geometry"}
            ])
        );
    }
}
