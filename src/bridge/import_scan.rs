//! 源码侧 specifier 扫描：抠出**导入位置**的字面量 specifier（字节 span）。
//!
//! 放在 core 而非 `oj` 的构建管线里：`oj build`（改写）与 `oj/src/checks.rs`（S008 校验）
//! 必须共用同一份口径——否则会出现「检查放行、构建改不动」或反向的分叉。同
//! `bridge::guard::extract_tables` 的先例（运行时守卫与结构检查同实现）。
//!
//! ponytail: 字符级扫描，不识别正则字面量（`/"/` 会与字符串错位）；非字面量实参的动态
//! import（`import(v)`、`import("./a" + x)`）不作导入处理。漏检后果由 `oj build` 的
//! 产物自洽断言兜底（运行期静默炸 → 构建期显式失败）。需要更强保真时换 deno_ast。

/// 导入位置的 specifier span：`(内容起, 内容止, spec)`（字节偏移，不含引号）。
/// 覆盖静态 `from "…"`、副作用 `import "…"`、动态 `import("…")`、`require("…")`；
/// 跳过 `//` 与 `/* */` 注释、普通字符串与模板串——注释里长得像导入的文本不会被误改。
pub fn specifier_spans(src: &str) -> Vec<(usize, usize, String)> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => i = skip_block_comment(b, i),
            b'\'' | b'"' | b'`' => i = skip_string(b, i),
            c if c.is_ascii_alphabetic() || c == b'_' || c == b'$' => {
                let start = i;
                let mut j = i;
                while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_' || b[j] == b'$')
                {
                    j += 1;
                }
                let word = &src[start..j];
                let mut next = j;
                if matches!(word, "from" | "import" | "require")
                    && let Some((s, e)) = specifier_after(src, word, j)
                {
                    out.push((s, e, src[s..e].to_string()));
                    next = e + 1; // 跳过闭合引号
                }
                i = next;
            }
            _ => i += 1,
        }
    }
    out
}

/// 就地改写全部导入位置 specifier：`f` 返回 `Some(新串)` 则替换、`None` 原样保留。
/// 先按源码顺序求值（错误确定性：首个出错者先报），再逆序拼接 span，
/// 天然支持一行多个 specifier。
pub fn rewrite_specifiers(
    src: &str,
    mut f: impl FnMut(&str) -> Result<Option<String>, String>,
) -> Result<String, String> {
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    for (s, e, spec) in specifier_spans(src) {
        if let Some(new) = f(&spec)? {
            edits.push((s, e, new));
        }
    }
    let mut out = src.to_string();
    for (s, e, new) in edits.into_iter().rev() {
        out.replace_range(s..e, &new);
    }
    Ok(out)
}

/// 本地 specifier（相对 / 别名）。npm 裸包名不在构建期改写面。
pub fn is_local(spec: &str) -> bool {
    is_alias(spec) || is_relative(spec)
}

/// 别名 specifier（`#` 模块根 / `#/` src 根）。
pub fn is_alias(spec: &str) -> bool {
    spec.starts_with('#')
}

/// 相对 specifier。
pub fn is_relative(spec: &str) -> bool {
    spec.starts_with("./") || spec.starts_with("../")
}

/// 跳过块注释，返回 `*/` 之后的下标（未闭合则到末尾）。
fn skip_block_comment(b: &[u8], start: usize) -> usize {
    let mut i = start + 2;
    while i < b.len() && !(b[i] == b'*' && b.get(i + 1) == Some(&b'/')) {
        i += 1;
    }
    (i + 2).min(b.len())
}

/// 跳过字符串/模板串字面量，返回闭合引号之后的下标（未闭合则到末尾）。
fn skip_string(b: &[u8], start: usize) -> usize {
    let q = b[start];
    let mut i = start + 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            c if c == q => return i + 1,
            _ => i += 1,
        }
    }
    b.len()
}

/// 跳过空白与注释（动态 import 的实参前常带 `/* webpackChunkName */`）。
fn skip_trivia(b: &[u8], mut i: usize) -> usize {
    loop {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if b.get(i) == Some(&b'/') && b.get(i + 1) == Some(&b'/') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if b.get(i) == Some(&b'/') && b.get(i + 1) == Some(&b'*') {
            i = skip_block_comment(b, i);
            continue;
        }
        return i;
    }
}

/// 关键字之后字面量 specifier 的内容区间。两条形态：
/// - `from "…"`：语法上 `from` 之后只能是字面量。**不允许 `(`**——`Array.from("./x")`
///   这类方法调用不是导入（只认字面量形态会把它误改）。
/// - `import("…")` / `require("…")`：允许一个 `(`，但实参必须是**孤立字面量**
///   （闭合引号后只能跟 `)` 或 `,`）——否则 `import("./a" + lang)` 会被当成导入并把
///   拼接表达式改坏。
fn specifier_after(src: &str, word: &str, from: usize) -> Option<(usize, usize)> {
    let b = src.as_bytes();
    let mut i = skip_trivia(b, from);
    let call = b.get(i) == Some(&b'(');
    if call {
        if word == "from" {
            return None;
        }
        i = skip_trivia(b, i + 1);
    }
    let q = *b.get(i)?;
    if q != b'\'' && q != b'"' {
        return None;
    }
    let start = i + 1;
    let mut k = start;
    while k < b.len() {
        match b[k] {
            b'\\' => k += 2,
            c if c == q => {
                // 非字面量实参（拼接/三元/变量）：不作导入处理，交运行期。
                if call && !matches!(b.get(skip_trivia(b, k + 1)), Some(&b')') | Some(&b',')) {
                    return None;
                }
                return Some((start, k));
            }
            _ => k += 1,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn specs(src: &str) -> Vec<String> {
        specifier_spans(src)
            .into_iter()
            .map(|(_, _, s)| s)
            .collect()
    }

    #[test]
    fn covers_static_side_effect_and_dynamic_forms() {
        let src = "import { v } from \"../_shared/validate\";\nimport \"./side\";\nconst a = await import(\"./dyn\");\nconst r = require(\"./req\");\nexport * from \"./star\";\nexport * as ns from \"./star2\";\nimport type { T } from \"./t\";\nimport x, { y } from \"./multi\";\nimport.meta.url;\nconst s = \"from \\\"./str\\\"\";\n// import x from \"./comment\";\n/* import y from \"./block\" */\n";
        assert_eq!(
            specs(src),
            vec![
                "../_shared/validate",
                "./side",
                "./dyn",
                "./req",
                "./star",
                "./star2",
                "./t",
                "./multi"
            ]
        );
    }

    #[test]
    fn ignores_method_calls_and_non_literal_arguments() {
        // `Array.from("./x")` 是方法调用，不是导入（只看 `from` 关键字会误改）。
        assert!(specs("const a = Array.from(\"./x\");").is_empty());
        // 拼接/三元/变量的动态 import 不是静态可实化的导入，不能当 specifier 改写。
        assert!(specs("const m = await import(\"./locales/\" + lang + \".json\");").is_empty());
        assert!(specs("const m = await import(cond ? \"./a\" : \"./b\");").is_empty());
        assert!(specs("const m = await import(name);").is_empty());
        assert!(specs("import.meta.url;").is_empty());
        // 条件表达式里的字面量也不是导入位置（不被抠出来）
        assert!(specs("const x = cond ? \"from\" : \"./a\";").is_empty());
    }

    #[test]
    fn dynamic_import_tolerates_trivia_and_attributes() {
        assert_eq!(
            specs("const m = await import(/* webpackChunkName: \"x\" */ \"./x\");"),
            vec!["./x"]
        );
        assert_eq!(
            specs("const r = require( \"./y\" , { x: 1 });"),
            vec!["./y"]
        );
        // 字符串里含注释形状文本不影响（它不是导入位置）
        assert!(specs("const a = \"/* import x from \\\"./y\\\" */\";").is_empty());
    }

    #[test]
    fn span_edits_are_splice_safe() {
        let src = "import a from \"./x\"; import b from './y';";
        let out = rewrite_specifiers(src, |s| Ok(Some(format!("{s}.js")))).unwrap();
        assert_eq!(out, "import a from \"./x.js\"; import b from './y.js';");
        // 一行多个 specifier：逆序拼接不串位
        let out = rewrite_specifiers("import \"./a\";import \"./bb\";", |s| {
            Ok(Some(s.replace("./", "./long-prefix-")))
        })
        .unwrap();
        assert_eq!(
            out,
            "import \"./long-prefix-a\";import \"./long-prefix-bb\";"
        );
    }
}
