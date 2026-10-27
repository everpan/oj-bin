//! 统一占位符改写（0.1.67，design docs/plans/2026-10-13-unified-db-placeholder-design.md）：
//! `?` 是 oj 的唯一绑定参数占位符，postgres 方言在宿主 op 层自动改写为 `$n`。
//!
//! `normalize_placeholders`：纯扫描器（单遍，边扫边记 `?` 位置与 max-n，末段一次改写）。
//! 规则：跳过引用区（`'...'` 含 `''` 翻倍与 E'' 反斜杠转义、`"..."`、`--`/`/* */`
//! 非嵌套注释、`$tag$...$tag$` 含空 tag，tag 识别优先于参数识别）；`?` 后紧跟
//! `[|&<>=#~\-]`（jsonb/几何运算符）不改写；`$n` 仅在 `$` 前非 `[A-Za-z0-9_$]`
//! 且 n≥1 时识别（`col$1`/`$0` 不算）；续号从 max+1 起；计数 = 改写后最大 n
//! （`$1,$1` 复用合法、`$2` 缺口合法，与 PG prepare 语义一致）；未闭合 dollar-quote
//! 报专用错误。
//!
//! `normalize_sql`：「何时改写」的决策面——仅 `placeholder == Dollar`（postgres 等）
//! 且 `!params.is_empty()` 触发；改写后占位符数 ≠ params.len() 时 fail-loud（文案进单测断言）。
//!
//! 已知限制（devkit 限制矩阵同源）：JSONB 裸 `?` + 非空参无法区分（一律 jsonb_exists）。

use std::borrow::Cow;

use super::dialect_caps::PlaceholderStyle;
use serde_json::Value;

/// 把裸 `?` 占位符按出现顺序改写为 `$n`。
/// 返回 (改写后 SQL, 改写后最大参数号 n)。无改动时 `Cow::Borrowed` 原样透传（零分配）。
/// 未闭合 dollar-quote 报 `Err`（评审 E5：专用文案，勿落到计数报错误导排错）。
pub fn normalize_placeholders(sql: &str) -> Result<(Cow<'_, str>, usize), String> {
    let b = sql.as_bytes();
    let n = b.len();
    let mut i = 0usize;
    let mut n_max = 0usize; // 已引用的最大参数号（max 计法，非出现次数）
    let mut qpos: Vec<usize> = Vec::new(); // 待改写 `?` 的字节偏移

    while i < n {
        match b[i] {
            // 字符串：'' 翻倍转义；E'' 前缀启用反斜杠转义感知（评审 A7/E2）。
            b'\'' => {
                let estring = i > 0
                    && (b[i - 1] == b'e' || b[i - 1] == b'E')
                    && (i < 2
                        || !(b[i - 2].is_ascii_alphanumeric()
                            || b[i - 2] == b'_'
                            || b[i - 2] == b'$'));
                i += 1;
                while i < n {
                    if estring && b[i] == b'\\' {
                        i += 2; // `\'` 不终结字符串
                        continue;
                    }
                    if b[i] == b'\'' {
                        if i + 1 < n && b[i + 1] == b'\'' {
                            i += 2; // `''` 翻倍
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            // 标识符：`""` 翻倍转义。
            b'"' => {
                i += 1;
                while i < n {
                    if b[i] == b'"' {
                        if i + 1 < n && b[i + 1] == b'"' {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            // 行注释：LF/CRLF 均终止（\r 留在注释内，评审 E10）。
            b'-' if i + 1 < n && b[i + 1] == b'-' => {
                i += 2;
                while i < n && b[i] != b'\n' {
                    i += 1;
                }
            }
            // 块注释：非嵌套（PG 语义钉死），首个 `*/` 闭合（评审 E10）。
            b'/' if i + 1 < n && b[i + 1] == b'*' => {
                i += 2;
                while i + 1 < n && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(n); // 未闭合则吞到尾
            }
            b'$' => {
                // 标识符边界（评审 E3）：`col$1` 的 `$` 属标识符。
                let boundary = i == 0
                    || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_' || b[i - 1] == b'$');
                if !boundary {
                    i += 1;
                    continue;
                }
                // tag 优先（设计 §1 规则 1）：`$tag$`，tag = [A-Za-z0-9_]*（含空 tag $$）。
                let mut j = i + 1;
                while j < n && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                    j += 1;
                }
                if j < n && b[j] == b'$' {
                    let tag = &sql[i..=j];
                    return match sql[j + 1..].find(tag) {
                        // 跳过整个 dollar-quote（内部 `?`/`$n` 一律不识别）
                        Some(off) => {
                            i = j + 1 + off + tag.len();
                            continue;
                        }
                        None => Err("unterminated dollar-quoted string".into()), // 评审 E5
                    };
                }
                // 参数引用：`$[0-9]+`（n≥1；其后不紧跟字母/下划线；`$0` 非参数）。
                let mut k = i + 1;
                while k < n && b[k].is_ascii_digit() {
                    k += 1;
                }
                if k > i + 1 && (k >= n || !(b[k].is_ascii_alphabetic() || b[k] == b'_')) {
                    let num: usize = sql[i + 1..k].parse().unwrap_or(0);
                    if num >= 1 {
                        if num > n_max {
                            n_max = num;
                        }
                        i = k;
                        continue;
                    }
                }
                i += 1; // 裸 `$` / `$0`：原样，数字由后续循环按普通文本复制
            }
            b'?' => {
                // jsonb/几何运算符豁免（评审 E4）：`?|`/`?&`/`?#`/`?-`/`?<`/`?>`/`?=`/`?~`。
                if i + 1 < n
                    && matches!(
                        b[i + 1],
                        b'|' | b'&' | b'<' | b'>' | b'=' | b'#' | b'~' | b'-'
                    )
                {
                    i += 1; // 运算符不占参数位，仅跳过 `?` 本身
                    continue;
                }
                qpos.push(i);
                i += 1;
            }
            _ => i += 1,
        }
    }

    // 无 `?` 则零分配透传（vendored P() 纯 `$n` SQL 的主路径）。
    if qpos.is_empty() {
        return Ok((Cow::Borrowed(sql), n_max));
    }
    // 末段一次改写：`?` 按 n_max+1 起连续递增（设计 §1 规则 4）。
    let mut out = String::with_capacity(sql.len() + qpos.len() * 4);
    let mut last = 0usize;
    for (idx, &pos) in qpos.iter().enumerate() {
        out.push_str(&sql[last..pos]);
        out.push_str(&format!("${}", n_max + 1 + idx));
        last = pos + 1;
    }
    out.push_str(&sql[last..]);
    Ok((Cow::Owned(out), n_max + qpos.len()))
}

/// 「何时改写」决策：仅 `placeholder == Dollar`（postgres 等）且带参时改写并做计数校验；
/// 其余（Q/Colon 占位风格或空参）原样透传。判定基于**能力位**而非方言枚举——
/// 开放契约下新增 Dollar 风格方言无需改此处（T7：字节等价改写 Postgres 行为）。
pub fn normalize_sql<'a>(
    placeholder: PlaceholderStyle,
    sql: &'a str,
    params: &[Value],
) -> Result<Cow<'a, str>, String> {
    // 触发条件（设计 §1 规则 6）：非 Dollar 占位风格 / 空参路径零扫描零分配。
    if placeholder != PlaceholderStyle::Dollar || params.is_empty() {
        return Ok(Cow::Borrowed(sql));
    }
    let (sql, count) = normalize_placeholders(sql)?;
    if count != params.len() {
        return Err(mismatch_error(count, params.len()));
    }
    Ok(sql)
}

/// fail-loud 文案（进单测断言，DevEx 交付物）：两个数字 + 正解 hint。
fn mismatch_error(count: usize, params: usize) -> String {
    format!(
        "placeholder count {count} != params {params}\n\
         hint: oj 统一占位符为 `?`（postgres 由宿主自动改写为 $n）；\
         jsonb 存在性判断请用 jsonb_exists(doc, 'key')"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// unwrap 便捷：返回 (SQL, 最大 n)。
    fn rw(sql: &str) -> (String, usize) {
        let (s, n) = normalize_placeholders(sql).expect("normalize_placeholders must succeed");
        (s.into_owned(), n)
    }

    /// 断言改写结果与计数。
    fn assert_rw(input: &str, expected: &str, expected_n: usize) {
        let (sql, n) = rw(input);
        assert_eq!(sql, expected, "input: {input:?}");
        assert_eq!(n, expected_n, "input: {input:?}");
    }

    /// —— §3① 24 条对抗性用例（每条先于实现存在，TDD 红） ——

    // 1 击穿：单引号字面量——字面量里的 `?` 保留，仅末位改写。
    #[test]
    fn t01_literal_in_single_quotes() {
        assert_rw(
            "select 'a?b' from t where id = ?",
            "select 'a?b' from t where id = $1",
            1,
        );
    }

    // 2 击穿：`''` 翻倍转义——转义后的收尾引号不得提前终结字符串。
    #[test]
    fn t02_doubled_quote_escape() {
        assert_rw(
            "where note = 'it''s ? ok' and id = ?",
            "where note = 'it''s ? ok' and id = $1",
            1,
        );
    }

    // 3 击穿：E-string 反斜杠转义——`\'` 不终结字符串（评审 A7/E2）。
    #[test]
    fn t03_e_string_backslash_escape() {
        assert_rw("select E'a\\'?b' as x, ?", "select E'a\\'?b' as x, $1", 1);
    }

    // 4 击穿：注释——字符串内的 `--` 与块注释里的 `?` 全保留。
    #[test]
    fn t04_comments_preserved() {
        assert_rw(
            "where note = '-- ?? x' /* ? */ and id = ?",
            "where note = '-- ?? x' /* ? */ and id = $1",
            1,
        );
    }

    // 5 击穿：dollar-quote（带 tag）——内部 `?` 保留。
    #[test]
    fn t05_dollar_quote_tagged() {
        assert_rw(
            "where body = $tag$? $tag$ and id = ?",
            "where body = $tag$? $tag$ and id = $1",
            1,
        );
    }

    // 6 击穿：dollar-quote 空 tag `$$...$$`（评审 E5）。
    #[test]
    fn t06_dollar_quote_empty_tag() {
        assert_rw("$$ ? $$ and id = ?", "$$ ? $$ and id = $1", 1);
    }

    // 7 击穿：纯数字 tag `$1$...$1$` 是 dollar-quote 而非参数引用（tag 优先）。
    #[test]
    fn t07_dollar_quote_numeric_tag() {
        assert_rw("$1$? $1$ and id = ?", "$1$? $1$ and id = $1", 1);
    }

    // 8 击穿：未闭合 dollar-quote——专用报错，勿落到计数文案误导排错（评审 E5）。
    #[test]
    fn t08_unterminated_dollar_quote() {
        let err = normalize_placeholders("$tag$ unclosed ? and id = ?")
            .expect_err("must reject unterminated dollar-quote");
        assert!(
            err.contains("unterminated dollar-quoted string"),
            "wrong message: {err}"
        );
    }

    // 9 击穿：双引号标识符。
    #[test]
    fn t09_double_quoted_identifier() {
        assert_rw(
            "select \"col?x\" from t where id = ?",
            "select \"col?x\" from t where id = $1",
            1,
        );
    }

    // 10 击穿：jsonb `?|` 运算符豁免（评审 E4）。豁免的运算符不占参数位
    // （'a','b' 是行内字面量非绑定参数），唯一真占位符续号 `$1`。
    #[test]
    fn t10_jsonb_or_operator_exempt() {
        assert_rw(
            "where doc ?| 'a','b' and id = ?",
            "where doc ?| 'a','b' and id = $1",
            1,
        );
    }

    // 11 击穿：几何 `?>` 运算符豁免（评审 E4）。同上，运算符不占参数位。
    #[test]
    fn t11_geometric_operator_exempt() {
        assert_rw(
            "where box1 ?> point(1,1) and id = ?",
            "where box1 ?> point(1,1) and id = $1",
            1,
        );
    }

    // 12 击穿：`$n` 标识符边界——`col$1` 的 `$1` 不计入 max（评审 E3）。
    #[test]
    fn t12_dollar_in_identifier_not_a_param() {
        assert_rw(
            "select col$1 from t where a = ?",
            "select col$1 from t where a = $1",
            1,
        );
    }

    // 13 击穿：引用区内的 `$1` 不得计入 max。
    #[test]
    fn t13_dollar_n_in_string_not_counted() {
        assert_rw(
            "select '$1' as x from t where a = ?",
            "select '$1' as x from t where a = $1",
            1,
        );
    }

    // 14 `$0` 非 PG 参数引用，不识别不改写。
    #[test]
    fn t14_dollar_zero_ignored() {
        assert_rw("where id = $0 and a = ?", "where id = $0 and a = $1", 1);
    }

    // 15 续号：已有 `$1`，`?` → `$2`。
    #[test]
    fn t15_continue_numbering() {
        assert_rw(
            "select a from t where x = $1 and b = ?",
            "select a from t where x = $1 and b = $2",
            2,
        );
    }

    // 16 续号 + 乱序：max 计法，`?` → `$4`。
    #[test]
    fn t16_continue_numbering_shuffled() {
        assert_rw(
            "select a from t where x = $3 or y = $1 or z = ?",
            "select a from t where x = $3 or y = $1 or z = $4",
            4,
        );
    }

    // 17 复用合法（评审 A2/E1）：`$1` 出现两次，max 计法 = 1，params 1 通过且不改写。
    #[test]
    fn t17_param_reuse_passes() {
        let (sql, n) = rw("where a = $1 or b = $1");
        assert_eq!(sql, "where a = $1 or b = $1");
        assert_eq!(n, 1);
        assert!(
            normalize_sql(
                PlaceholderStyle::Dollar,
                "where a = $1 or b = $1",
                &[json!(1)]
            )
            .is_ok()
        );
    }

    // 18 缺口合法：`$2` 无 `$1`，PG prepare 参数个数 = max n = 2，params 2 通过。
    #[test]
    fn t18_param_gap_passes() {
        let (sql, n) = rw("where a = $2");
        assert_eq!(sql, "where a = $2");
        assert_eq!(n, 2);
        assert!(
            normalize_sql(
                PlaceholderStyle::Dollar,
                "where a = $2",
                &[json!(1), json!(2)]
            )
            .is_ok()
        );
    }

    // 19 计数 fail-loud：占位符数 ≠ 参数数，文案含两个数字 + 正解 hint（评审 P3）。
    #[test]
    fn t19_count_mismatch_fail_loud() {
        let err = normalize_sql(
            PlaceholderStyle::Dollar,
            "insert into t (a, b) values (?, ?)",
            &[json!(1)],
        )
        .expect_err("must reject count mismatch");
        assert!(err.contains("placeholder count 2 != params 1"), "{err}");
        assert!(err.contains("hint"), "{err}");
        assert!(err.contains("jsonb_exists"), "{err}");
    }

    // 20 空参数短路（评审 A3）：无参 SQL 的 `?` 多为 jsonb 运算符，整条不改写（零分配）。
    #[test]
    fn t20_empty_params_short_circuit() {
        let r = normalize_sql(PlaceholderStyle::Dollar, "select doc ? 'a'", &[])
            .expect("empty params must pass through");
        assert!(matches!(r, Cow::Borrowed(_)), "must be zero-copy: {r:?}");
        assert_eq!(r, "select doc ? 'a'");
    }

    // 21 类型转换：改写后 `?::text` → `$1::text` 仍合法。
    #[test]
    fn t21_cast_after_placeholder() {
        assert_rw(
            "where a = ?::text and b = ?",
            "where a = $1::text and b = $2",
            2,
        );
    }

    // 22 退化输入：空串 / 无占位符 / 全 `?` 无引用。
    #[test]
    fn t22_degenerate_inputs() {
        let (sql, n) = rw("");
        assert_eq!(sql, "");
        assert_eq!(n, 0);
        let (sql, n) = rw("select 1");
        assert_eq!(sql, "select 1");
        assert_eq!(n, 0);
        assert_rw("?,?,?", "$1,$2,$3", 3);
    }

    // 23 击穿：块注释非嵌套（PG 语义）——首个 `*/` 即闭合，其后的 `?` 被改写（评审 E10）。
    #[test]
    fn t23_block_comment_not_nested() {
        assert_rw("/* /* ? */ ? */", "/* /* ? */ $1 */", 1);
    }

    // 24 击穿：注释互嵌与 CRLF——`--` 以 LF/CRLF 终止；`/* */` 内的 `--` 无效。
    #[test]
    fn t24_comment_interleaving_and_crlf() {
        assert_rw("/* -- ? */ and id = ?", "/* -- ? */ and id = $1", 1);
        assert_rw("-- /*\nwhere id = ?", "-- /*\nwhere id = $1", 1);
        assert_rw("-- c\r\nwhere id = ?", "-- c\r\nwhere id = $1", 1);
    }

    // —— §3② 集成面前置：占位风格分支钉死（非 Dollar 透传零开销） ——

    #[test]
    fn t25_q_and_colon_pass_through() {
        let r = normalize_sql(
            PlaceholderStyle::Q,
            "update account set name = ? where id = ?",
            &[json!("n"), json!(1)],
        )
        .expect("Q placeholder must pass through");
        assert!(matches!(r, Cow::Borrowed(_)), "must be zero-copy: {r:?}");
        assert_eq!(r, "update account set name = ? where id = ?");
        let r = normalize_sql(PlaceholderStyle::Colon, "a = ?", &[json!(1)])
            .expect("Colon placeholder must pass through");
        assert!(matches!(r, Cow::Borrowed(_)), "must be zero-copy: {r:?}");
    }

    // Dollar 正向路径（经决策面）：改写 + 计数对上后原样放行。
    #[test]
    fn t26_dollar_decision_path_rewrites() {
        let r = normalize_sql(
            PlaceholderStyle::Dollar,
            "select * from t where id = ? and name = ?",
            &[json!(1), json!("n")],
        )
        .expect("dollar rewrite must succeed");
        assert_eq!(r, "select * from t where id = $1 and name = $2");
    }
}
