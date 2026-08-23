//! oj build：src → dist（按模块）。转译 .ts（默认 minify，`--no-minify` 关）、剥 `.route`、
//! 补相对 import 后缀；产物保留原名与目录结构（api.ts → 同目录 api.js），
//! 产出 `dist/<module>-<version>/`（routes.js + manifest.yaml）
//! 与 `dist/manifests.yaml`、`<module>-<version>.tgz`（spec §2）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use mdm_base_rust::bridge::{transpile, Bridge, InMemoryKV, LoaderShared, SchemaRegistry, SqlxAccessor};
use mdm_server::routes;

use crate::args::BuildArgs;

/// 构建入口：单模块或全部（None）。内省器自带独立线程 runtime；async 仅因内存库初始化。
pub async fn run(a: &BuildArgs) -> Result<(), String> {
    let src = PathBuf::from(&a.dir)
        .canonicalize()
        .map_err(|e| format!("src dir '{}': {e}", a.dir))?;
    let out = PathBuf::from(&a.out);
    // 跨模块导入的版本视图：单模块 = 锁；全量 = 锁 ∪ src 各模块 manifest（src 在建，覆盖锁）。
    let mut view = crate::manifest::load_lock(&out.join("manifests.yaml"))?;
    let mut names: Vec<String> = match &a.module {
        Some(m) => {
            crate::manifest::validate_module(m)?;
            let mf = src.join(m).join("manifest.yaml");
            if !mf.is_file() {
                return Err(format!("module {m:?}: no manifest.yaml under {}", src.display()));
            }
            view.insert(m.clone(), crate::manifest::parse_one(&mf)?.version);
            vec![m.clone()]
        }
        None => crate::manifest::load_modules(&src)?
            .into_iter()
            .map(|m| {
                view.insert(m.name.clone(), m.version);
                m.name
            })
            .collect(),
    };
    names.sort(); // read_dir 顺序不定；构建顺序确定 → 控制台/lock 写入顺序稳定
    // view 全量（锁∪计划）{m}-{v} 不单射（a v1-x 与 a-1 vx 同落一个版本目录，后者清场前者；锁内陈旧条目同理）→ fail-fast
    let mut vdirs = std::collections::HashSet::new();
    for (m, v) in &view {
        let vd = format!("{m}-{v}");
        if !vdirs.insert(vd.clone()) {
            return Err(format!("version dir collision: {vd}"));
        }
    }
    for name in &names {
        build_one(&src, &out, name, &view, a.minify).await?;
    }
    println!("oj build: {} module(s) → {}", names.len(), out.display());
    Ok(())
}

/// JSON 字符串字面量（转义交给 serde_json，pattern 里可安全含引号）。
fn q(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

/// rel 路径的目录段（正斜杠；模块根下为 ""）。
fn rel_dir(rel: &Path) -> String {
    rel.parent().unwrap_or(Path::new("")).to_string_lossy().replace('\\', "/")
}

/// 单模块构建：清场同名版本目录 → 落盘 → 内省产 routes.js → lock upsert → tgz。
/// view = 跨模块导入目标的版本表（run 构建）；minify = 转译产物压缩开关。
async fn build_one(
    src: &Path,
    out: &Path,
    module: &str,
    view: &std::collections::BTreeMap<String, String>,
    minify: bool,
) -> Result<(), String> {
    let mdir = src.join(module);
    crate::manifest::validate_module(module)?; // 两路径共用的白名单（全量路径同样过）
    let m = crate::manifest::parse_one(&mdir.join("manifest.yaml"))?;
    if m.name != module {
        return Err(format!("manifest name {:?} != module {:?}", m.name, module));
    }
    crate::manifest::validate_version(&m.version)?;
    let vdir = out.join(format!("{module}-{}", m.version));
    // 清场：同版本重建先删（旧产物残留根治，spec §2.3）
    if vdir.exists() {
        std::fs::remove_dir_all(&vdir).map_err(|e| format!("clean {}: {e}", vdir.display()))?;
    }
    std::fs::create_dir_all(&vdir).map_err(|e| format!("mkdir {}: {e}", vdir.display()))?;

    // 1. 收集 + api.ts 不可被 import 守卫
    let files = collect_module(&mdir)?;
    let sources: Vec<(String, String)> = files
        .iter()
        .filter(|(rel, _)| rel.extension().is_some_and(|e| e == "ts"))
        .map(|(rel, _)| {
            let text = std::fs::read_to_string(mdir.join(rel))
                .map_err(|e| format!("read {}: {e}", rel.display()))?;
            Ok((rel.to_string_lossy().into_owned(), text))
        })
        .collect::<Result<_, String>>()?;
    guard_no_api_imports(&sources)?;

    // 2. 落盘：全部 .ts 原路径换 .js 扩展（api.ts 同名 api.js，仅多一步剥 .route）；
    //    补相对 import 后缀后按需 minify；manifest.yaml 原样复制。
    for (rel, is_api) in &files {
        let dir = rel.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new(""));
        let dst_dir = vdir.join(dir);
        std::fs::create_dir_all(&dst_dir).map_err(|e| format!("mkdir {}: {e}", dst_dir.display()))?;
        if rel.extension().is_some_and(|e| e == "yaml") {
            let dst = dst_dir.join(rel.file_name().unwrap());
            std::fs::copy(mdir.join(rel), &dst).map_err(|e| format!("copy {}: {e}", dst.display()))?;
        } else {
            let js = transpile::cached_transpile(&mdir.join(rel))
                .map_err(|e| format!("transpile {}: {e}", rel.display()))?;
            let stripped; // 生命周期：strip 产物要活过 fix_relative_imports 调用
            let js = fix_relative_imports(
                if *is_api { stripped = strip_route_decls(&js); &stripped } else { &js },
                module,
                &m.version,
                &rel_dir(rel),
                view,
            )?;
            let js = if minify {
                transpile::minify_js(&mdir.join(rel), &js)
                    .map_err(|e| format!("minify {}: {e}", rel.display()))?
            } else {
                js
            };
            let name = rel.with_extension("js").file_name().unwrap().to_string_lossy().into_owned();
            let dst = dst_dir.join(&name);
            std::fs::write(&dst, js).map_err(|e| format!("write {}: {e}", dst.display()))?;
        }
    }

    // 3. 内省（内存库）→ routes.js：pattern 无 base 含模块段（rel_pattern），
    //    file = 同名产物相对版本目录根（正斜杠；根级 api.ts 为裸 api.js）
    let decls = introspect_module_files(src, &mdir, &files).await?;
    let n_api = decls.len();
    let mut js = String::from("// 由 oj build 生成；勿手改。\nexport default [\n");
    for (dir, rows) in decls {
        let file = if dir.is_empty() { "api.js".to_string() } else { format!("{dir}/api.js") };
        for (method, route) in rows {
            js.push_str(&format!(
                "  {{ method: {}, pattern: {}, file: {} }},\n",
                q(&method),
                q(&rel_pattern(module, &dir, route.as_deref())),
                q(&file)
            ));
        }
    }
    js.push_str("];\n");
    std::fs::write(vdir.join("routes.js"), js).map_err(|e| format!("write routes.js: {e}"))?;

    // 4. manifests.yaml：读旧（缺失=空表；坏锁 Err 不静默重置）→ upsert → 原子写
    let lock_path = out.join("manifests.yaml");
    let mut lock = crate::manifest::load_lock(&lock_path)?;
    lock.insert(module.to_string(), m.version.clone());
    crate::manifest::save_lock(&lock_path, &lock)?;

    // 5. tgz
    crate::pack::write_tgz(
        &vdir,
        &out.join(format!("{module}-{}.tgz", m.version)),
        &format!("{module}-{}", m.version),
    )?;
    println!(
        "oj build: {module} v{} → {} ({} api file(s))",
        m.version,
        vdir.display(),
        n_api
    );
    Ok(())
}

/// 模块内省：每个 api.ts 一线程 + current_thread runtime（Bridge !Send），内存库
/// 零磁盘副作用（同 dev 内省管道）。project_root 取 src 父目录（= 项目根，同 dev 的
/// config_dir）：bare import 要沿 node_modules 向上解析到项目根（src 下没有）。
/// 返回 (rel 目录, 该 api.ts 的 (method, route) 行)，顺序随 collect_module 确定。
async fn introspect_module_files(
    src: &Path,
    mdir: &Path,
    files: &[(PathBuf, bool)],
) -> Result<Vec<(String, Vec<(String, Option<String>)>)>, String> {
    let mut dbs: HashMap<String, Arc<dyn mdm_base_rust::bridge::DataAccessor>> = HashMap::new();
    dbs.insert(
        "default".into(),
        SqlxAccessor::arc("sqlite::memory:")
            .await
            .map_err(|e| format!("open build db: {e}"))?,
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
    let introspect = routes::bridge_introspector(make);
    let mut out = Vec::new();
    for (rel, is_api) in files {
        if !is_api {
            continue;
        }
        let rows = introspect(&mdir.join(rel))
            .map_err(|e| format!("introspect {}: {e}", rel.display()))?;
        out.push((rel_dir(rel), rows));
    }
    Ok(out)
}

/// 递归收集模块内 .ts 与 manifest.yaml（相对模块根，确定性排序）。
/// is_api = 文件名是 api.ts（剥 .route + 进 routes.js）。
fn collect_module(root: &Path) -> Result<Vec<(PathBuf, bool)>, String> {
    let mut acc = Vec::new();
    walk(root, root, &mut acc)?;
    Ok(acc)
}

fn walk(root: &Path, dir: &Path, acc: &mut Vec<(PathBuf, bool)>) -> Result<(), String> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| format!("read {}: {e}", dir.display()))?
        .flatten()
        .collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let p = e.path();
        if p.is_dir() {
            walk(root, &p, acc)?;
        } else {
            let name = e.file_name().to_string_lossy().into_owned();
            let is_api = name == "api.ts";
            if is_api || name.ends_with(".ts") || name == "manifest.yaml" {
                acc.push((p.strip_prefix(root).unwrap().to_path_buf(), is_api));
            }
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

/// 静态 import/export-from 的相对裸 specifier 改写为 dist 产物路径（spec §2.4）：
/// 归一解析后仍在 `src/<m>/` 内 → 模块内重算相对路径（版本目录布局下原 specifier
/// 上溯会落到无版本段的 `dist/<m>/…` 悬空）；越界 → 跨模块，查版本视图得 v_t，
/// 指向 `dist/<m_t>-<v_t>/`，视图缺 m_t fail-fast。
/// ponytail: 逐行、仅 `from "…"` 字面量；动态 import / 别名出现时再补。
fn fix_relative_imports(
    src: &str,
    module: &str,
    version: &str,
    rel_dir: &str,
    view: &std::collections::BTreeMap<String, String>,
) -> Result<String, String> {
    let mut out = Vec::new();
    for l in src.lines() {
        let Some(i) = l.find("from ") else {
            out.push(l.to_string());
            continue;
        };
        let rest = &l[i + 5..];
        let Some(quote) = rest.chars().next().filter(|c| *c == '"' || *c == '\'') else {
            out.push(l.to_string());
            continue;
        };
        let s = &rest[1..];
        let Some(end) = s.find(quote) else {
            out.push(l.to_string());
            continue;
        };
        let spec = &s[..end];
        let bare = !spec.ends_with(".js") && !spec.ends_with(".mjs") && !spec.ends_with(".json");
        if !(spec.starts_with("./") || spec.starts_with("../")) || !bare {
            out.push(l.to_string());
            continue;
        }
        let segs = resolve_spec(spec, module, rel_dir)?;
        let target_dir = if segs[0] == module {
            format!("{module}-{version}")
        } else {
            let m_t = &segs[0];
            let v_t = view.get(m_t).ok_or_else(|| {
                format!(
                    "cross-module import {spec:?} → module {m_t:?} version unknown \
                     (not in dist/manifests.yaml) — run `oj build {m_t}` first"
                )
            })?;
            format!("{m_t}-{v_t}")
        };
        let to = std::iter::once(target_dir).chain(segs[1..].iter().cloned()).collect();
        let new_spec = product_spec(module, version, rel_dir, to);
        out.push(format!("{}{}{}{}", &l[..i + 5], quote, new_spec, &s[end..]));
    }
    Ok(out.join("\n") + "\n")
}

/// 归一解析相对 specifier（相对 `src/<module>/<rel_dir>/`）为 src 下段列表。
/// `..` 越过 src 根 / 解析到 src 根本身都报错（无第一段 → 既非模块内也非跨模块）。
fn resolve_spec(spec: &str, module: &str, rel_dir: &str) -> Result<Vec<String>, String> {
    let mut segs = vec![module.to_string()];
    segs.extend(rel_dir.split('/').filter(|s| !s.is_empty()).map(str::to_string));
    for part in spec.split('/') {
        match part {
            "." | "" => {}
            ".." => {
                if segs.pop().is_none() {
                    return Err(format!(
                        "relative import {spec:?} escapes src/ (from {module}/{rel_dir})"
                    ));
                }
            }
            p => segs.push(p.to_string()),
        }
    }
    if segs.is_empty() {
        return Err(format!("relative import {spec:?} resolves to src/ root"));
    }
    Ok(segs)
}

/// 产物相对 specifier：从 `dist/<module>-<version>/<rel_dir>/`（当前产物文件位置）到
/// `to`（首段为目标版本目录）的相对路径；末段 `.ts` 改 `.js`、无后缀补 `.js`；
/// 无上溯时必须带 `./` 前缀（ESM 裸 specifier 会被当包名解析）。
fn product_spec(module: &str, version: &str, rel_dir: &str, mut to: Vec<String>) -> String {
    let from: Vec<String> = std::iter::once(format!("{module}-{version}"))
        .chain(rel_dir.split('/').filter(|s| !s.is_empty()).map(str::to_string))
        .collect();
    let mut i = 0;
    while i < from.len() && i < to.len() && from[i] == to[i] {
        i += 1;
    }
    let last = to.len() - 1;
    let stem = to[last].strip_suffix(".ts").unwrap_or(&to[last]);
    to[last] = format!("{stem}.js");
    let mut parts: Vec<String> = vec!["..".into(); from.len() - i];
    parts.extend(to[i..].iter().cloned());
    let joined = parts.join("/");
    if i == from.len() { format!("./{joined}") } else { joined }
}

/// 相对 pattern（spec §2.1）：无首斜杠无 base，含模块名段。
/// None/空 route → 目录镜像；相对声明 → 目录 + route；根级声明（/ 开头）→ 剥首斜杠不加模块段。
fn rel_pattern(module: &str, rel_dir: &str, route: Option<&str>) -> String {
    match route.map(str::trim).filter(|r| !r.is_empty()) {
        None => {
            if rel_dir.is_empty() {
                module.to_string()
            } else {
                format!("{module}/{rel_dir}")
            }
        }
        Some(r) if r.starts_with('/') => r.trim_start_matches('/').to_string(),
        Some(r) => {
            if rel_dir.is_empty() {
                format!("{module}/{r}")
            } else {
                format!("{module}/{rel_dir}/{r}")
            }
        }
    }
}

/// 静态 import/export-from 的相对 specifier（与 fix_relative_imports 同口径：行级、字面量）。
/// ponytail: 动态 import()/别名出现时再补。
fn relative_import_specifiers(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    for l in src.lines() {
        let Some(i) = l.find("from ") else { continue };
        let rest = &l[i + 5..];
        let Some(q) = rest.chars().next().filter(|c| *c == '"' || *c == '\'') else { continue };
        let s = &rest[1..];
        let Some(end) = s.find(q) else { continue };
        let spec = &s[..end];
        if spec.starts_with("./") || spec.starts_with("../") {
            out.push(spec.to_string());
        }
    }
    out
}

/// api.ts 只许作路由入口（spec §2.5）：它是 routes.js 的聚合单元而非可复用模块，
/// 被导入会把路由副作用（.route 声明、默认导出的 handler 表）拖进普通模块。
/// 目标 basename（剥扩展）== "api" 即拒绝——宁枉勿纵，报错给全部违规。
fn guard_no_api_imports(files: &[(String, String)]) -> Result<(), String> {
    let mut bad = Vec::new();
    for (rel, src) in files {
        for spec in relative_import_specifiers(src) {
            let target = spec.rsplit('/').next().unwrap_or("");
            let stem = target
                .strip_suffix(".ts")
                .or_else(|| target.strip_suffix(".js"))
                .unwrap_or(target);
            if stem == "api" {
                bad.push(format!(
                    "  {rel} imports {spec:?} (api.ts 是路由入口，不可被 import)"
                ));
            }
        }
    }
    if bad.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "api.ts 不可被模块内 import：\n{}",
            bad.join("\n")
        ))
    }
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

    /// 测试辅助：默认上下文（模块 a 0.1.0、rel_dir item、空版本视图）。
    fn fix(src: &str, rel_dir: &str, view: &std::collections::BTreeMap<String, String>) -> String {
        fix_relative_imports(src, "a", "0.1.0", rel_dir, view).unwrap()
    }

    #[test]
    fn fix_imports_appends_js_to_relative_only() {
        let src = "import { v } from \"../_shared/validate\";\nimport x from \"./a.js\";\nimport y from \"pkg\";\nimport m from \"./m.mjs\";\nimport j from \"./d.json\";\nexport { v } from \"./b\";\nconst s = \"from \\\"./nope\\\"\";\n";
        let out = fix(src, "item", &Default::default());
        assert!(out.contains("\"../_shared/validate.js\""), "{out}");
        assert!(out.contains("\"./a.js\""), "{out}");
        assert!(out.contains("from \"pkg\""), "{out}");
        assert!(out.contains("\"./m.mjs\""), "{out}"); // 已带后缀不动（.mjs/.json 不误加 .js）
        assert!(out.contains("\"./d.json\""), "{out}");
        assert!(out.contains("\"./b.js\""), "{out}");
        assert!(out.contains("from \\\"./nope\\\""), "{out}"); // 引号未开 → 不动
    }

    /// 版本视图 {b: 0.2.0}。
    fn view_b() -> std::collections::BTreeMap<String, String> {
        [("b".to_string(), "0.2.0".to_string())].into_iter().collect()
    }

    #[test]
    fn cross_module_import_rewrites_to_versioned_path() {
        // ① 模块根出发：../b/util → dist/b-0.2.0/util.js
        let out = fix("import { v } from \"../b/util\";\n", "", &view_b());
        assert!(out.contains("\"../b-0.2.0/util.js\""), "{out}");
        // ①' 子目录出发：../../b/util → 同样落到 dist/b-0.2.0/
        let out = fix("import { v } from \"../../b/util\";\n", "sub", &view_b());
        assert!(out.contains("\"../../b-0.2.0/util.js\""), "{out}");
        // ③ 嵌套 rel_dir：src/a/x/y/f.ts 导入 ../../../b/util
        let out = fix("import { v } from \"../../../b/util\";\n", "x/y", &view_b());
        assert!(out.contains("\"../../../b-0.2.0/util.js\""), "{out}");
        // 显式 .ts 后缀目标 → .js
        let out = fix("export { v } from \"../b/util.ts\";\n", "", &view_b());
        assert!(out.contains("\"../b-0.2.0/util.js\""), "{out}");
        // 模块内绕出再绕回（../../a/y/g 从 x/ 出发）→ 产物路径不悬空
        let out = fix("import { v } from \"../../a/y/g\";\n", "x", &Default::default());
        assert!(out.contains("\"../y/g.js\""), "{out}");
    }

    #[test]
    fn cross_module_import_without_version_fails_fast() {
        // ② 视图缺 b → Err 报目标模块并提示先构建
        let e = fix_relative_imports("import { v } from \"../b/util\";\n", "a", "0.1.0", "", &Default::default())
            .unwrap_err();
        assert!(e.contains("b") && e.contains("oj build"), "{e}");
        // 逃出 src/ → Err
        let e = fix_relative_imports("import { v } from \"../../b/util\";\n", "a", "0.1.0", "", &view_b())
            .unwrap_err();
        assert!(e.contains("src"), "{e}");
    }

    #[test]
    fn rel_pattern_rules() {
        // 镜像行：模块名 + 目录段
        assert_eq!(rel_pattern("user", "account", None), "user/account");
        assert_eq!(rel_pattern("user", "", None), "user");
        assert_eq!(rel_pattern("user", "profile/detail", None), "user/profile/detail");
        // 相对 .route 声明
        assert_eq!(rel_pattern("user", "item", Some("{id}")), "user/item/{id}");
        assert_eq!(rel_pattern("user", "", Some("{id}")), "user/{id}");
        // 根级声明（/ 开头）：剥首斜杠，不加模块段
        assert_eq!(rel_pattern("user", "item", Some("/v2/user/{id}")), "v2/user/{id}");
        // 空 route 视同未挂
        assert_eq!(rel_pattern("user", "item", Some("")), "user/item");
    }

    #[test]
    fn import_specifier_extraction() {
        let src = "import { v } from \"../_shared/validate\";\nimport x from './a.js';\nimport p from \"pkg\";\nexport { v } from \"./b\";\nconst s = 1;";
        assert_eq!(relative_import_specifiers(src), vec!["../_shared/validate", "./a.js", "./b"]);
    }

    #[test]
    fn guard_rejects_api_imports() {
        let files = vec![
            ("_shared/util.ts".into(), "import { g } from \"../account/api\";\n".into()),
            ("account/api.ts".into(), "import { v } from \"../_shared/validate\";\n".into()),
        ];
        let e = guard_no_api_imports(&files).unwrap_err();
        assert!(e.contains("_shared/util.ts") && e.contains("../account/api"), "{e}");
        // 无违规
        assert!(guard_no_api_imports(&[("x.ts".into(), "import m from \"pkg\";".into())]).is_ok());
    }

    /// 测试辅助：目录下唯一文件的文件名（String）。
    fn only_file(dir: &std::path::Path) -> String {
        let mut it = std::fs::read_dir(dir).unwrap();
        let name = it.next().unwrap().unwrap().file_name();
        assert!(it.next().is_none(), "expected exactly one file in {}", dir.display());
        name.to_string_lossy().into_owned()
    }

    /// 测试辅助：摆一个 src（user 带 .route + _shared，other 纯镜像）。
    fn src_fixture(t: &std::path::Path) {
        let src = t.join("src");
        for d in ["user/item", "user/_shared", "other/list"] {
            std::fs::create_dir_all(src.join(d)).unwrap();
        }
        std::fs::write(src.join("user/manifest.yaml"), "name: user\ndesc: d\nversion: 0.1.0\n").unwrap();
        std::fs::write(src.join("other/manifest.yaml"), "name: other\ndesc: d\nversion: 0.9.0\n").unwrap();
        std::fs::write(src.join("user/_shared/validate.ts"), "export const v = 1;\n").unwrap();
        std::fs::write(src.join("user/item/api.ts"),
            "import { v } from \"../_shared/validate\";\nfunction get(){ json.ok({v}); }\nget.route = \"{id}\";\nexport default { get };\n").unwrap();
        std::fs::write(src.join("other/list/api.ts"),
            "function get(){ json.ok({}); }\nexport default { get };\n").unwrap();
    }

    fn build_args(t: &std::path::Path, module: Option<&str>) -> BuildArgs {
        BuildArgs {
            module: module.map(str::to_string),
            dir: t.join("src").display().to_string(),
            out: t.join("dist").display().to_string(),
            minify: true,
        }
    }

    #[tokio::test]
    async fn build_module_emits_versioned_artifacts() {
        let t = std::env::temp_dir().join(format!("oj-build-art-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        std::fs::create_dir_all(&t).unwrap();
        src_fixture(&t);
        run(&build_args(&t, Some("user"))).await.unwrap();

        let vd = t.join("dist/user-0.1.0");
        assert_eq!(only_file(&vd.join("item")), "api.js"); // 保留原目录结构原名

        let routes = std::fs::read_to_string(vd.join("routes.js")).unwrap();
        assert!(routes.contains("\"user/item/{id}\""), "{routes}"); // pattern 无 base 含模块段
        assert!(routes.contains("\"item/api.js\""), "{routes}");    // file 含目录段
        assert!(!routes.contains("/v1/api"), "{routes}");

        let item_js = std::fs::read_to_string(vd.join("item/api.js")).unwrap();
        assert!(!item_js.contains(".route"), "{item_js}");          // .route 已剥
        assert!(item_js.contains("\"../_shared/validate.js\""), "{item_js}"); // import 后缀已补
        assert!(!item_js.contains('\n'), "{item_js}");              // 默认 minify：单行

        assert!(vd.join("manifest.yaml").is_file());               // 原样复制
        assert!(vd.join("_shared/validate.js").is_file());         // 非 api 原路径
        assert!(!item_js.contains("sourceMappingURL"), "{item_js}"); // minify 剥内联 sourcemap

        let lock = crate::manifest::load_lock(&t.join("dist/manifests.yaml")).unwrap();
        assert_eq!(lock.get("user").map(String::as_str), Some("0.1.0"));
        assert!(!lock.contains_key("other"));                       // 单模块构建不动他人
        assert!(t.join("dist/user-0.1.0.tgz").is_file());
        let _ = std::fs::remove_dir_all(&t);
    }

    #[tokio::test]
    async fn build_is_deterministic_and_wipes_on_change() {
        let t = std::env::temp_dir().join(format!("oj-build-wipe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        std::fs::create_dir_all(&t).unwrap();
        src_fixture(&t);
        run(&build_args(&t, Some("user"))).await.unwrap();
        let snap = |p: &std::path::Path| std::fs::read(p).unwrap();
        let (js1, tgz1) = (snap(&t.join("dist/user-0.1.0/item/api.js")), snap(&t.join("dist/user-0.1.0.tgz")));
        // 内容未变 → 重建字节一致（转译 + minify 确定性，落点 tgz）
        run(&build_args(&t, Some("user"))).await.unwrap();
        assert_eq!(snap(&t.join("dist/user-0.1.0/item/api.js")), js1);
        assert_eq!(snap(&t.join("dist/user-0.1.0.tgz")), tgz1, "同输入两次构建 tgz 必须字节一致");
        // 内容变更 → 产物更新，目录内仍恰好 1 个 api.js（同版本清场）
        std::fs::write(t.join("src/user/item/api.ts"),
            "function get(){ json.ok({v:2}); }\nget.route = \"{id}\";\nexport default { get };\n").unwrap();
        run(&build_args(&t, Some("user"))).await.unwrap();
        let js2 = snap(&t.join("dist/user-0.1.0/item/api.js"));
        assert_ne!(js2, js1);
        assert_eq!(only_file(&t.join("dist/user-0.1.0/item")), "api.js");
        let _ = std::fs::remove_dir_all(&t);
    }

    #[tokio::test]
    async fn no_minify_keeps_readable_output() {
        let t = std::env::temp_dir().join(format!("oj-build-nomin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        std::fs::create_dir_all(&t).unwrap();
        src_fixture(&t);
        let mut a = build_args(&t, Some("user"));
        a.minify = false;
        run(&a).await.unwrap();
        let js = std::fs::read_to_string(t.join("dist/user-0.1.0/item/api.js")).unwrap();
        assert!(js.contains('\n'), "{js}");               // 未压缩：多行可读
        assert!(js.contains("function get"), "{js}");
        let _ = std::fs::remove_dir_all(&t);
    }

    #[tokio::test]
    async fn build_rejects_api_import() {
        let t = std::env::temp_dir().join(format!("oj-build-guard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        std::fs::create_dir_all(t.join("src/user/_shared")).unwrap();
        std::fs::write(t.join("src/user/manifest.yaml"), "name: user\ndesc: d\nversion: 0.1.0\n").unwrap();
        std::fs::write(t.join("src/user/_shared/x.ts"), "import { g } from \"../item/api\";\n").unwrap();
        let e = run(&build_args(&t, Some("user"))).await.err().unwrap_or_default();
        assert!(e.contains("不可被"), "{e}"); // 守卫专属文案（"api" 子串近似恒真）
        let _ = std::fs::remove_dir_all(&t);
    }

    #[tokio::test]
    async fn build_all_modules() {
        let t = std::env::temp_dir().join(format!("oj-build-all-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        std::fs::create_dir_all(&t).unwrap();
        src_fixture(&t);
        run(&build_args(&t, None)).await.unwrap();
        assert!(t.join("dist/user-0.1.0/routes.js").is_file());
        assert!(t.join("dist/other-0.9.0/routes.js").is_file());
        let lock = crate::manifest::load_lock(&t.join("dist/manifests.yaml")).unwrap();
        assert_eq!(lock.len(), 2, "{lock:?}");
        let _ = std::fs::remove_dir_all(&t);
    }

    /// 测试辅助：单模块 src（name/version 可注入）。
    fn one_module(t: &std::path::Path, name: &str, version: &str) {
        std::fs::create_dir_all(t.join("src").join(name)).unwrap();
        std::fs::write(
            t.join("src").join(name).join("manifest.yaml"),
            format!("name: {name}\ndesc: d\nversion: {version}\n"),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn build_all_rejects_illegal_module_dir() {
        // 全量路径的模块名同样是信任边界输入（I-1）：fail-fast 且锁不被污染
        let t = std::env::temp_dir().join(format!("oj-build-illegal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        one_module(&t, "bad module", "0.1.0");
        let e = run(&build_args(&t, None)).await.err().unwrap_or_default();
        assert!(e.contains("illegal module"), "{e}");
        assert!(!t.join("dist/manifests.yaml").is_file());
        let _ = std::fs::remove_dir_all(&t);
    }

    #[tokio::test]
    async fn build_rejects_illegal_version() {
        let t = std::env::temp_dir().join(format!("oj-build-ver-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        one_module(&t, "user", "0..1");
        let e = run(&build_args(&t, Some("user"))).await.err().unwrap_or_default();
        assert!(e.contains("illegal version") && e.contains("0..1"), "{e}");
        assert!(!t.join("dist/manifests.yaml").is_file());
        let _ = std::fs::remove_dir_all(&t);
    }

    #[tokio::test]
    async fn build_all_rejects_vdir_collision() {
        // {m}-{v} 不单射：a v1-x 与 a-1 vx 同落 dist/a-1-x（后者构建清场前者）→ 计划期 Err
        let t = std::env::temp_dir().join(format!("oj-build-vdir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        one_module(&t, "a", "1-x");
        one_module(&t, "a-1", "x");
        let e = run(&build_args(&t, None)).await.err().unwrap_or_default();
        assert!(e.contains("collision") && e.contains("a-1-x"), "{e}");
        assert!(!t.join("dist/a-1-x").exists());
        let _ = std::fs::remove_dir_all(&t);
    }

    #[tokio::test]
    async fn build_rejects_collision_with_stale_lock_entry() {
        // R-1：撞名比对含锁内条目——锁 {a-1: x} 陈旧残留时，单建 a v1-x 同落 dist/a-1-x → Err
        let t = std::env::temp_dir().join(format!("oj-build-vdir2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        one_module(&t, "a", "1-x");
        std::fs::create_dir_all(t.join("dist")).unwrap();
        std::fs::write(t.join("dist/manifests.yaml"), "a-1: x\n").unwrap();
        let e = run(&build_args(&t, Some("a"))).await.err().unwrap_or_default();
        assert!(e.contains("collision") && e.contains("a-1-x"), "{e}");
        let _ = std::fs::remove_dir_all(&t);
    }

    #[tokio::test]
    async fn single_build_preserves_other_lock_entries() {
        let t = std::env::temp_dir().join(format!("oj-build-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        std::fs::create_dir_all(&t).unwrap();
        src_fixture(&t);
        std::fs::create_dir_all(t.join("dist")).unwrap();
        std::fs::write(t.join("dist/manifests.yaml"), "other: 0.9.0\n").unwrap(); // 预置他模块
        run(&build_args(&t, Some("user"))).await.unwrap();
        let lock = crate::manifest::load_lock(&t.join("dist/manifests.yaml")).unwrap();
        assert_eq!(lock.get("user").map(String::as_str), Some("0.1.0"));
        assert_eq!(lock.get("other").map(String::as_str), Some("0.9.0")); // spec §6：保留
        let _ = std::fs::remove_dir_all(&t);
    }

    #[tokio::test]
    async fn build_errs_on_corrupt_lock() {
        // 坏锁（非法 YAML）→ Err，不得 unwrap_or_default 当空表静默重置（I-2）
        let t = std::env::temp_dir().join(format!("oj-build-badlock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        std::fs::create_dir_all(&t).unwrap();
        src_fixture(&t);
        std::fs::create_dir_all(t.join("dist")).unwrap();
        std::fs::write(t.join("dist/manifests.yaml"), "user: [unclosed\n").unwrap();
        let e = run(&build_args(&t, Some("user"))).await.err().unwrap_or_default();
        assert!(e.contains("manifests.yaml"), "{e}");
        assert!(std::fs::read_to_string(t.join("dist/manifests.yaml")).unwrap().contains("unclosed"));
        let _ = std::fs::remove_dir_all(&t);
    }
}
