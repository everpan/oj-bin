//! oj build：src → dist。转译全部 .ts、剥离 `.route`、补相对 import 后缀，
//! 并生成 `dist/routes.js`（release 直载的唯一路由来源，见设计 §4.1/§4.2）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use mdm_base_rust::bridge::{transpile, Bridge, InMemoryKV, LoaderShared, SchemaRegistry, SqlxAccessor};
use mdm_server::routes::{self, RouteTable};

use crate::args::BuildArgs;

/// 构建入口（内省器自带独立线程 runtime；async 仅因内存库初始化）。
pub async fn run(a: &BuildArgs) -> Result<(), String> {
    let src = PathBuf::from(&a.dir)
        .canonicalize()
        .map_err(|e| format!("src dir '{}': {e}", a.dir))?;
    let out = PathBuf::from(&a.out);
    // 1. 全量落盘：.ts → 转译 + 剥 .route + 补相对 import 后缀；manifest.yaml 原样复制。
    let mut files = Vec::new();
    collect(&src, &src, &mut files)?;
    for rel in &files {
        let dst = out.join(rel);
        std::fs::create_dir_all(dst.parent().unwrap()).map_err(|e| format!("mkdir {}: {e}", dst.display()))?;
        if rel.extension().is_some_and(|e| e == "yaml") {
            std::fs::copy(src.join(rel), &dst).map_err(|e| format!("copy {}: {e}", dst.display()))?;
        } else {
            // .ts → .js（同名）
            let js = rel.with_extension("js");
            let dst = out.join(&js);
            let code = transpile::cached_transpile(&src.join(rel))
                .map_err(|e| format!("transpile {}: {e}", rel.display()))?;
            std::fs::write(&dst, fix_relative_imports(&strip_route_decls(&code)))
                .map_err(|e| format!("write {}: {e}", dst.display()))?;
        }
    }
    // 2. 建表（复用 dev 内省：含 .route 行与镜像行）→ 3. routes.js。
    let (table, failures) = build_table(&a.base, &src).await?;
    for f in &failures {
        eprintln!("error: route: {f}");
    }
    let mut js = String::from("// 由 oj build 生成；勿手改（release 模式直载注册，见设计 §4.1）。\nexport default [\n");
    for r in table.listing() {
        let rel = r
            .file
            .strip_prefix(&src)
            .map_err(|_| format!("route file outside src: {}", r.file.display()))?
            .with_extension("js");
        js.push_str(&format!(
            "  {{ method: {}, pattern: {}, file: {} }},\n",
            q(&r.method),
            q(&r.pattern),
            q(&rel.to_string_lossy().replace('\\', "/"))
        ));
    }
    js.push_str("];\n");
    let routes_js = out.join("routes.js");
    std::fs::write(&routes_js, js).map_err(|e| format!("write {}: {e}", routes_js.display()))?;
    println!(
        "oj build: {} module file(s) → {} ({} route row(s))",
        files.len(),
        out.display(),
        table.listing().len()
    );
    Ok(())
}

/// JSON 字符串字面量（转义交给 serde_json，pattern 里可安全含引号）。
fn q(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

/// 建表：与 dev server 同构（bridge_introspector 独立线程内省 src 的 api.ts）。
/// project_root 取 src 父目录（= 项目根，同 dev 的 config_dir）：bare import 要沿
/// node_modules 向上解析到项目根（src 下没有 node_modules）。
/// db 用内存库：构建零磁盘副作用，模块顶层建表/查询语句照常执行后即弃。
async fn build_table(base: &str, src: &Path) -> Result<(RouteTable, Vec<String>), String> {
    let mut dbs: HashMap<String, Arc<dyn mdm_base_rust::bridge::DataAccessor>> = HashMap::new();
    dbs.insert(
        "default".into(),
        SqlxAccessor::arc("sqlite::memory:").await.map_err(|e| format!("open build db: {e}"))?,
    );
    let root = src.parent().unwrap_or(src).to_path_buf();
    let make = {
        let dbs = dbs.clone();
        move || {
            Bridge::with_dbs_and_loader(
                dbs.clone(),
                Arc::new(InMemoryKV::new()),
                SchemaRegistry::new(),
                false,
                Some(Arc::new(LoaderShared { project_root: root.clone(), ts: true })),
            )
        }
    };
    Ok(routes::RouteTable::build(base, src, true, routes::bridge_introspector(make)))
}

/// 递归收集 .ts 与 manifest.yaml（相对 root 的路径，确定性排序）。
fn collect(root: &Path, dir: &Path, acc: &mut Vec<PathBuf>) -> Result<(), String> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| format!("read {}: {e}", dir.display()))?
        .flatten()
        .collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        if p.is_dir() {
            collect(root, &p, acc)?;
        } else if name.ends_with(".ts") || name == "manifest.yaml" {
            acc.push(p.strip_prefix(root).unwrap().to_path_buf());
        }
    }
    Ok(())
}

/// 剥离转译产物中的 `.route` 赋值整行（`fn.route = "...";`）。
/// ponytail: 行级匹配语句起始的标准写法；表达式中间的 `.route` 读取不受影响。
fn strip_route_decls(src: &str) -> String {
    let kept: Vec<&str> = src
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            !(t.contains('=') && t.split(|c: char| c.is_whitespace() || c == '=').next().unwrap_or("").ends_with(".route"))
        })
        .collect();
    let mut out = kept.join("\n");
    out.push('\n');
    out
}

/// 静态 import/export-from 的相对裸 specifier 补 `.js`（dist 产物可直接运行的 ESM）。
/// ponytail: 逐行、仅 `from "…"` 字面量；动态 import / 别名出现时再补。
fn fix_relative_imports(src: &str) -> String {
    src.lines()
        .map(|l| {
            let Some(i) = l.find("from ") else { return l.to_string() };
            let rest = &l[i + 5..];
            let Some(quote) = rest.chars().next().filter(|c| *c == '"' || *c == '\'') else {
                return l.to_string()
            };
            let s = &rest[1..];
            let Some(end) = s.find(quote) else { return l.to_string() };
            let spec = &s[..end];
            let bare = !spec.ends_with(".js") && !spec.ends_with(".mjs") && !spec.ends_with(".json");
            if (spec.starts_with("./") || spec.starts_with("../")) && bare {
                return format!("{}{}{}.js{}", &l[..i + 5], quote, spec, &s[end..]);
            }
            l.to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_route_removes_assignments_only() {
        let src = "function get() {}\nget.route = \"{id}\";\nconst r = x.route;\nexport default { get };\n";
        let out = strip_route_decls(src);
        assert!(!out.contains(".route ="), "{out}");
        assert!(out.contains("function get()"), "{out}");
        assert!(out.contains("x.route"), "{out}"); // 读取不剥
    }

    #[test]
    fn fix_imports_appends_js_to_relative_only() {
        let src = "import { v } from \"../_shared/validate\";\nimport x from \"./a.js\";\nimport y from \"pkg\";\nimport m from \"./m.mjs\";\nimport j from \"./d.json\";\nexport { v } from \"./b\";\nconst s = \"from \\\"./nope\\\"\";\n";
        let out = fix_relative_imports(src);
        assert!(out.contains("\"../_shared/validate.js\""), "{out}");
        assert!(out.contains("\"./a.js\""), "{out}");
        assert!(out.contains("from \"pkg\""), "{out}");
        assert!(out.contains("\"./m.mjs\""), "{out}"); // 已带后缀不动（.mjs/.json 不误加 .js）
        assert!(out.contains("\"./d.json\""), "{out}");
        assert!(out.contains("\"./b.js\""), "{out}");
        assert!(out.contains("from \\\"./nope\\\""), "{out}"); // 引号未开 → 不动
    }
}
