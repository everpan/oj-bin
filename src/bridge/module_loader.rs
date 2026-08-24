//! oj 的 ESM 模块加载器：相对导入（Deno 风格补全）+ 裸 specifier（node_modules，T8）
//! 与 CJS 包装互操作。`?v=<mtime>` 版本化 specifier 让 V8 模块缓存天然按内容失效。
//! 注意：旧版本模块不可卸载，按编辑次数缓慢积累（dev 重启清零，release 有界）。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use deno_core::ModuleSpecifier;
use deno_core::error::ModuleLoaderError;
use deno_core::{OpState, op2};
use deno_error::JsErrorBox;
// 0.410：`modules` 模块私有，loader 相关类型全部经 crate 根再导出（lib.rs:137-160）。
use deno_core::{
    ModuleLoadOptions, ModuleLoadReferrer, ModuleLoadResponse, ModuleResolveResponse,
    ModuleSource, ModuleSourceCode, ModuleType, ResolutionKind,
};

use super::StableState;
use super::transpile::cached_transpile;

/// loader 共享配置（project_root 用于 node_modules 回溯上界与 CJS require）。
pub struct LoaderShared {
    pub project_root: PathBuf,
    /// dev 模式（.ts 可达）。release 下 .ts 仍可被 import（dist 一般没有）。
    pub ts: bool,
}

/// deno_core ModuleLoader 实现。Rc<dyn ModuleLoader> 挂 RuntimeOptions，
/// 内部状态经 Arc 跨 actor 共享（转译缓存在 transpile 模块全局）。
pub struct OjModuleLoader {
    pub inner: Arc<LoaderShared>,
}

impl deno_core::ModuleLoader for OjModuleLoader {
    fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        _kind: ResolutionKind,
    ) -> ModuleResolveResponse {
        self.resolve_inner(specifier, referrer)
            .map_err(ModuleLoaderError::generic)
    }

    fn load(
        &self,
        module_specifier: &ModuleSpecifier,
        _referrer: Option<&ModuleLoadReferrer>,
        _options: ModuleLoadOptions,
    ) -> ModuleLoadResponse {
        ModuleLoadResponse::Sync(Self::load_specifier(module_specifier))
    }
}

impl OjModuleLoader {
    fn resolve_inner(&self, specifier: &str, referrer: &str) -> Result<ModuleSpecifier, String> {
        if let Ok(url) = ModuleSpecifier::parse(specifier) {
            // 绝对 file:// URL（driver 对 api 模块的 import）：原样通过。
            if url.scheme() == "file" {
                return Ok(url);
            }
            return Err(format!("unsupported scheme: {specifier}"));
        }
        let ref_dir = referrer_dir(referrer)?;
        let p = if specifier.starts_with("./") || specifier.starts_with("../") {
            resolve_relative(&ref_dir, specifier, self.inner.ts)?
        } else {
            resolve_bare(specifier, &ref_dir, &self.inner.project_root)?
        };
        ensure_within(&p, &self.inner.project_root)?;
        versioned_specifier(&p)
    }

    /// load：剥 ?v= → 读盘（.ts 走缓存转译）→ CJS 则包装 → ModuleSource。
    fn load_specifier(spec: &ModuleSpecifier) -> Result<ModuleSource, ModuleLoaderError> {
        let path = spec
            .to_file_path()
            .map_err(|_| ModuleLoaderError::generic(format!("not a file url: {spec}")))?;
        let src = cached_transpile(&path).map_err(ModuleLoaderError::generic)?;
        let code = if looks_cjs(&src) {
            wrap_cjs(&src, &path.display().to_string())
        } else {
            src
        };
        Ok(ModuleSource::new(
            ModuleType::JavaScript,
            ModuleSourceCode::String(code.into()),
            spec,
            None,
        ))
    }
}

/// 词法归一化 `..`/`.`：stat 会逐组件进目录，中间目录不存在（如未创建的 referrer
/// 所在目录）时 `a/../b` 误报不存在。URL join 本就词法消解 `..`，此处对齐。
fn normalize_lexically(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                // 前面无可弹（相对路径溢出）时原样保留。
                if !out.pop() {
                    out.push("..");
                }
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// referrer（file URL，可能带 ?v=）→ 所在目录。
fn referrer_dir(referrer: &str) -> Result<PathBuf, String> {
    let url =
        ModuleSpecifier::parse(referrer).map_err(|e| format!("bad referrer {referrer}: {e}"))?;
    let path = url
        .to_file_path()
        .map_err(|_| format!("referrer not a file url: {referrer}"))?;
    Ok(path.parent().map(|p| p.to_path_buf()).unwrap_or_default())
}

/// 相对导入解析：as-is → +.ts → +.js → /index.ts → /index.js（存在即命中）。
pub fn resolve_relative(base_dir: &Path, spec: &str, ts: bool) -> Result<PathBuf, String> {
    let mut tried = Vec::new();
    let stem = normalize_lexically(&base_dir.join(spec));
    let mut candidates: Vec<PathBuf> = vec![stem.clone()];
    if ts {
        candidates.push(stem.with_extension("ts"));
    }
    candidates.push(stem.with_extension("js"));
    if ts {
        candidates.push(stem.join("index.ts"));
    }
    candidates.push(stem.join("index.js"));
    for c in &candidates {
        tried.push(c.display().to_string());
        if c.is_file() {
            return Ok(c.clone());
        }
    }
    Err(format!(
        "cannot resolve '{spec}' from '{}': tried [{}]",
        base_dir.display(),
        tried.join(", ")
    ))
}

/// 裸 specifier 解析（Node 算法简化版）：
/// pkg → <dir>/node_modules/<pkg>（从 from_dir 逐级向上至 root）→ package.json
/// 的 module → main → index.js；subpath（pkg/a.js）直映射包内文件。
/// ponytail: 不做 exports/conditions 映射与 pnpm 布局；主流简单包可用。
pub fn resolve_bare(spec: &str, from_dir: &Path, root: &Path) -> Result<PathBuf, String> {
    // pkg 名：@scope/name 占两段。
    let mut parts: Vec<&str> = spec.split('/').collect();
    let pkg = if parts.first().is_some_and(|s| s.starts_with('@')) && parts.len() >= 2 {
        format!("{}/{}", parts[0], parts[1])
    } else {
        parts[0].to_string()
    };
    let sub: Vec<&str> = if pkg.contains('/') { parts.split_off(2) } else { parts.split_off(1) };

    let mut tried = Vec::new();
    let mut dir = Some(from_dir);
    while let Some(d) = dir {
        let nm = d.join("node_modules").join(&pkg);
        if nm.is_dir() {
            if sub.is_empty() {
                let p = pkg_entry(&nm)?;
                return Ok(p);
            }
            let p = nm.join(sub.join("/"));
            if p.is_file() {
                return Ok(p);
            }
            tried.push(p.display().to_string());
        } else {
            tried.push(nm.display().to_string());
        }
        if d == root {
            break;
        }
        dir = d.parent();
    }
    Err(format!(
        "cannot resolve '{spec}' from '{}' (node_modules installed?): tried [{}]",
        from_dir.display(),
        tried.join(", ")
    ))
}

/// 包入口：package.json 的 module → main → index.js。
fn pkg_entry(pkg_dir: &Path) -> Result<PathBuf, String> {
    let pj = pkg_dir.join("package.json");
    if pj.is_file() {
        let text = std::fs::read_to_string(&pj).map_err(|e| format!("read {pj:?}: {e}"))?;
        let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
        for field in ["module", "main"] {
            if let Some(m) = v[field].as_str() {
                let p = pkg_dir.join(m.trim_start_matches("./"));
                if p.is_file() {
                    return Ok(p);
                }
            }
        }
    }
    let idx = pkg_dir.join("index.js");
    if idx.is_file() {
        return Ok(idx);
    }
    Err(format!("package '{}' has no entry (module/main/index.js)", pkg_dir.display()))
}

/// 版本化 specifier：file://<abs>?v=<mtime nanos>（mtime 变 → 新模块 → 热重载）。
pub fn versioned_specifier(path: &Path) -> Result<ModuleSpecifier, String> {
    let mtime = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map_err(|e| format!("stat {}: {e}", path.display()))?;
    let nanos = mtime
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|e| format!("bad mtime on {}: {e}", path.display()))?;
    let abs = std::fs::canonicalize(path)
        .map_err(|e| format!("canonicalize {}: {e}", path.display()))?;
    let mut url = ModuleSpecifier::from_file_path(abs)
        .map_err(|_| format!("cannot build file url from {}", path.display()))?;
    url.set_query(Some(&format!("v={}", nanos.as_nanos())));
    Ok(url)
}

/// project_root 钳制：解析结果 canonical 化后必须仍在 root 内。
/// lexical `..` 归一化可组合出根外路径（specifier 来自项目文件，属纵深防御）。
/// 双侧 canonical 化对齐符号链接（如 macOS 的 /var → /private/var），避免误伤。
fn ensure_within(p: &Path, root: &Path) -> Result<(), String> {
    let cp = std::fs::canonicalize(p).map_err(|e| format!("stat {}: {e}", p.display()))?;
    let cr = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    if cp.starts_with(&cr) {
        Ok(())
    } else {
        Err(format!("module path {} escapes project root {}", p.display(), root.display()))
    }
}

/// CJS 启发式：无 ESM 顶层语法且是 .js/.cjs（node_modules 包）。
/// ponytail: 启发式覆盖主流简单包；误判时报错信息可定位（module is not defined）。
pub fn looks_cjs(src: &str) -> bool {
    !src.contains("export ") && !src.contains("export{") && !src.contains("import ")
        && !src.contains("import(")
}

/// CJS → ESM 包装：default = module.exports；require 绑定为 `__ojRequire(n, 模块自身路径)`，
/// 包内嵌套 require 从包目录解析（裸传 __ojRequire 会丢 referrer）。
pub fn wrap_cjs(src: &str, module_path: &str) -> String {
    // JSON 编码即合法 JS 字符串字面量（处理引号/反斜杠）。
    let referrer = serde_json::to_string(module_path).unwrap_or_else(|_| "\"\"".into());
    format!(
        "const __oj_cjs_module = {{ exports: {{}} }};\n(function (module, exports, require) {{\n{src}\n}})(__oj_cjs_module, __oj_cjs_module.exports, (n) => __ojRequire(n, {referrer}));\nexport default __oj_cjs_module.exports;\n"
    )
}

/// CJS require 底座：node_modules 解析 + 读源码（JS 侧 __ojRequire eval 执行）。
/// project_root 取 StableState.loader（T9 oj 装配注入；未配置时报错）。
/// ponytail: 仅裸 specifier；相对 require 与 exports 映射待真实依赖出现再加。
#[op2]
#[serde]
pub fn op_resolve_cjs(
    state: &mut OpState,
    #[string] name: String,
    #[string] referrer: String,
) -> Result<serde_json::Value, JsErrorBox> {
    let root = state
        .borrow::<Arc<StableState>>()
        .loader
        .as_ref()
        .map(|l| l.project_root.clone())
        .ok_or_else(|| JsErrorBox::generic("project root not configured (loader wiring pending)"))?;
    let from = Path::new(&referrer)
        .parent()
        .unwrap_or(Path::new("."))
        .to_path_buf();
    let p = resolve_bare(&name, &from, &root).map_err(JsErrorBox::generic)?;
    let code = std::fs::read_to_string(&p)
        .map_err(|e| JsErrorBox::generic(format!("read {}: {e}", p.display())))?;
    Ok(serde_json::json!({ "path": p.display().to_string(), "code": code }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fx(files: &[(&str, &str)]) -> std::path::PathBuf {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let base = std::env::temp_dir().join(format!(
            "oj-ldr-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&base).unwrap();
        for (rel, content) in files {
            let p = base.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        }
        base
    }

    #[test]
    fn relative_resolution_completes_extensions() {
        let root = fx(&[
            ("user/_shared/validate.ts", "export function f() {}\n"),
            ("user/_shared/mod/index.ts", "export const x = 1;\n"),
            ("user/plain.js", "export const y = 2;\n"),
        ]);
        let dir = root.join("user/account");
        let ts = true;
        assert!(resolve_relative(&dir, "../_shared/validate", ts).unwrap().ends_with("validate.ts"));
        assert!(resolve_relative(&dir, "../_shared/mod", ts).unwrap().ends_with("mod/index.ts"));
        assert!(resolve_relative(&dir, "../plain", ts).unwrap().ends_with("plain.js"));
        let err = resolve_relative(&dir, "../nope", ts).unwrap_err();
        assert!(err.contains("tried"), "{err}");
    }

    #[test]
    fn versioned_specifier_roundtrip() {
        let root = fx(&[("a.ts", "export default 1;\n")]);
        let p = root.join("a.ts");
        let url = versioned_specifier(&p).unwrap();
        assert!(url.as_str().starts_with("file://"), "{url}");
        assert!(url.as_str().contains("?v="), "{url}");
    }

    #[test]
    fn bare_resolves_node_modules() {
        let root = fx(&[
            ("node_modules/escape-goat/index.js", "export const x = 1;\n"),
            ("node_modules/escape-goat/package.json",
             r#"{"name":"escape-goat","version":"4.0.0","type":"module"}"#),
            ("node_modules/cjspkg/main.js", "module.exports = { n: 1 };\n"),
            ("node_modules/cjspkg/package.json",
             r#"{"name":"cjspkg","version":"1.0.0","main":"main.js"}"#),
            ("node_modules/withmod/pkg/lib/util.js", "export const u = 1;\n"),
            ("node_modules/withmod/pkg/package.json", r#"{"name":"withmod"}"#),
        ]);
        let from = root.join("src/user");
        // ESM 包：type:module → index.js。
        assert!(resolve_bare("escape-goat", &from, &root).unwrap().ends_with("escape-goat/index.js"));
        // CJS 包：main 字段。
        assert!(resolve_bare("cjspkg", &from, &root).unwrap().ends_with("cjspkg/main.js"));
        // subpath 直映射。
        assert!(resolve_bare("withmod/pkg/lib/util.js", &from, &root).unwrap().ends_with("lib/util.js"));
        // 不存在 → 错误含提示。
        let e = resolve_bare("nope-pkg", &from, &root).unwrap_err();
        assert!(e.contains("node_modules"), "{e}");
        // 回溯：src/user/feat 深处也能找到根 node_modules。
        assert!(resolve_bare("escape-goat", &root.join("src/user/feat"), &root).is_ok());
    }

    #[test]
    fn resolve_rejects_paths_escaping_project_root() {
        // base 下 proj 是项目根；escape.js 在根外（不依赖 /etc 等系统文件）。
        let base = fx(&[
            ("escape.js", "export const e = 1;\n"),
            ("proj/src/user/mod.js", "export const m = 1;\n"),
        ]);
        let root = base.join("proj");
        let loader = OjModuleLoader {
            inner: Arc::new(LoaderShared { project_root: root.clone(), ts: true }),
        };
        let referrer =
            ModuleSpecifier::from_file_path(root.join("src/user/mod.js")).unwrap().to_string();
        // 根内相对导入不受影响。
        assert!(loader.resolve_inner("./mod.js", &referrer).is_ok());
        // lexical `..` 越过项目根 → 钳制报错（而非解析成功）。
        let e = loader.resolve_inner("../../../escape.js", &referrer).unwrap_err();
        assert!(e.contains("escapes project root"), "{e}");
    }

    #[test]
    fn cjs_detection_and_wrap() {
        assert!(looks_cjs("module.exports = { a: 1 };\n"));
        assert!(!looks_cjs("export default 1;\n"));
        assert!(!looks_cjs("import x from 'y';\nmodule.exports = x;\n"));
        let wrapped = wrap_cjs("module.exports = { a: 1 };\n", "/nm/p/main.js");
        assert!(wrapped.contains("__oj_cjs_module"), "{wrapped}");
        assert!(wrapped.contains("export default __oj_cjs_module.exports"), "{wrapped}");
        // require 绑定模块自身路径（嵌套 require 的 referrer）。
        assert!(wrapped.contains(r#"__ojRequire(n, "/nm/p/main.js")"#), "{wrapped}");
    }
}
