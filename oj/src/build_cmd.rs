//! oj build：src → dist（按模块）。转译 .ts、剥 `.route`、补相对 import 后缀；
//! api.ts 按内容哈希改名，产出 `dist/<module>-<version>/`（routes.js + manifest.yaml）
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
    let mut names: Vec<String> = match &a.module {
        Some(m) => {
            crate::manifest::validate_module(m)?;
            let mf = src.join(m).join("manifest.yaml");
            if !mf.is_file() {
                return Err(format!("module {m:?}: no manifest.yaml under {}", src.display()));
            }
            vec![m.clone()]
        }
        None => crate::manifest::load_modules(&src)?
            .into_iter()
            .map(|m| m.name)
            .collect(),
    };
    names.sort(); // read_dir 顺序不定；构建顺序确定 → 控制台/lock 写入顺序稳定
    for name in &names {
        build_one(&src, &out, name).await?;
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
async fn build_one(src: &Path, out: &Path, module: &str) -> Result<(), String> {
    let mdir = src.join(module);
    let m = crate::manifest::parse_one(&mdir.join("manifest.yaml"))?;
    if m.name != module {
        return Err(format!("manifest name {:?} != module {:?}", m.name, module));
    }
    crate::manifest::validate_version(&m.version)?;
    let vdir = out.join(format!("{module}-{}", m.version));
    // 清场：同版本重建先删（旧哈希残留根治，spec §2.3）
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

    // 2. 落盘：api.ts → api-<hash16>.js（转译+剥 .route+补后缀，哈希含变换后内容）；
    //    其余 .ts 原路径换 .js 扩展；manifest.yaml 原样复制。
    let mut hashed: HashMap<String, String> = HashMap::new(); // rel 目录 → 哈希文件名
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
            let js = fix_relative_imports(if *is_api { stripped = strip_route_decls(&js); &stripped } else { &js });
            let name = if *is_api {
                let n = format!("api-{}.js", crate::pack::hash16(&js));
                // routes.js 的 file：相对版本目录根（含目录段，正斜杠；根级 api.ts 为裸名）
                let key = rel_dir(rel);
                hashed.insert(key.clone(), if key.is_empty() { n.clone() } else { format!("{key}/{n}") });
                n
            } else {
                rel.with_extension("js").file_name().unwrap().to_string_lossy().into_owned()
            };
            let dst = dst_dir.join(&name);
            std::fs::write(&dst, js).map_err(|e| format!("write {}: {e}", dst.display()))?;
        }
    }

    // 3. 内省（内存库）→ routes.js：pattern 无 base 含模块段（rel_pattern），file 为哈希名
    let decls = introspect_module_files(src, &mdir, &files).await?;
    let mut js = String::from("// 由 oj build 生成；勿手改。\nexport default [\n");
    for (dir, rows) in decls {
        for (method, route) in rows {
            let file = hashed
                .get(&dir)
                .cloned()
                .ok_or_else(|| format!("module {module:?}: no hashed api file for {dir:?}"))?;
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

    // 4. manifests.yaml：读旧（无则空表）→ upsert → 原子写
    let lock_path = out.join("manifests.yaml");
    let mut lock = crate::manifest::load_lock(&lock_path).unwrap_or_default();
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
        hashed.len()
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
/// is_api = 文件名是 api.ts（哈希改名 + 进 routes.js）。
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

/// 哈希改名的配套防线：api.ts 只许作路由入口（spec §2.5）。
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
        let item_name = only_file(&vd.join("item"));
        assert!(item_name.starts_with("api-") && item_name.ends_with(".js"), "{item_name}");
        assert_eq!(item_name.len(), "api-".len() + 16 + ".js".len()); // 16 hex

        let routes = std::fs::read_to_string(vd.join("routes.js")).unwrap();
        assert!(routes.contains("\"user/item/{id}\""), "{routes}"); // pattern 无 base 含模块段
        assert!(routes.contains(&format!("\"item/{item_name}\"")), "{routes}"); // file 含目录段
        assert!(!routes.contains("/v1/api"), "{routes}");

        let item_js = std::fs::read_to_string(vd.join("item").join(&item_name)).unwrap();
        assert!(!item_js.contains(".route ="), "{item_js}");       // .route 已剥
        assert!(item_js.contains("../_shared/validate.js"), "{item_js}"); // import 后缀已补

        assert!(vd.join("manifest.yaml").is_file());               // 原样复制
        assert!(vd.join("_shared/validate.js").is_file());         // 非 api 原路径
        assert!(vd.join("item/api.ts").metadata().is_err());       // 不留旧名

        let lock = crate::manifest::load_lock(&t.join("dist/manifests.yaml")).unwrap();
        assert_eq!(lock.get("user").map(String::as_str), Some("0.1.0"));
        assert!(!lock.contains_key("other"));                       // 单模块构建不动他人
        assert!(t.join("dist/user-0.1.0.tgz").is_file());
        let _ = std::fs::remove_dir_all(&t);
    }

    #[tokio::test]
    async fn rebuild_same_version_stable_hash_then_wipes() {
        let t = std::env::temp_dir().join(format!("oj-build-wipe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        std::fs::create_dir_all(&t).unwrap();
        src_fixture(&t);
        run(&build_args(&t, Some("user"))).await.unwrap();
        let first = only_file(&t.join("dist/user-0.1.0/item"));
        // 内容未变 → 重建哈希稳定（转译确定性 + hash16）
        run(&build_args(&t, Some("user"))).await.unwrap();
        assert_eq!(only_file(&t.join("dist/user-0.1.0/item")), first);
        // 内容变更 → 旧哈希清场，目录内恰好 1 个 api-*.js
        std::fs::write(t.join("src/user/item/api.ts"),
            "function get(){ json.ok({v:2}); }\nget.route = \"{id}\";\nexport default { get };\n").unwrap();
        run(&build_args(&t, Some("user"))).await.unwrap();
        let second = only_file(&t.join("dist/user-0.1.0/item"));
        assert_ne!(second, first);
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
        assert!(e.contains("api.ts") || e.contains("api"), "{e}");
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
}
