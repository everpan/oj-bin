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
    ModuleLoadOptions, ModuleLoadReferrer, ModuleLoadResponse, ModuleResolveResponse, ModuleSource,
    ModuleSourceCode, ModuleType, ResolutionKind,
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
        } else if specifier.starts_with('#') && !under_node_modules(&ref_dir) {
            // 别名（`#` 模块根 / `#/` src 根）。node_modules 内的文件不启用——那是
            // 第三方包自己的 `#`（Node package.json#imports）语义，不劫持。
            resolve_alias(specifier, &ref_dir, &self.inner.project_root, self.inner.ts)?
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

/// 剥除 Windows verbatim 前缀 `\\?\`（与 `oj-plugin-ffi/src/path_util.rs` 的
/// `dunce::simplified` 同效，但本 crate 不引 dunce）。`std::fs::canonicalize` 在
/// Windows 返回 `\\?\C:\...`，而 `ModuleSpecifier::to_file_path` 还原时**剥掉**该前缀，
/// 二者词法前缀不一致会让 `module_root_of` 的 `starts_with(project_root)` 误判——此处
/// 归一后再比。非 Windows 为 no-op。
pub fn strip_verbatim(p: &Path) -> PathBuf {
    let s = p.as_os_str().to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        p.to_path_buf()
    }
}

/// 模块根：referrer 所在目录**向上最近的含 manifest.yaml 的祖先目录**（上溯以
/// project_root 为界——模块只可能落在它内部，同时避免为越界路径走到文件系统根）。
/// 锚点由文件自身位置派生，故 dev（`src/<m>/`）、release 产物（`dist/<m>-<v>/`，
/// manifest.yaml 原样复制）、tasks 镜像（无）三处语义自动一致，无需把 api 根路径
/// 穿透到各装配点。find 不到 = 该文件不在任何模块内（tasks 池 / tests 目录）。
pub fn module_root_of(from_dir: &Path, root: &Path) -> Option<PathBuf> {
    // Windows：`canonicalize` 给出的 referrer 带 `\\?\`，project_root 可能不带（或反之），
    // 先归一再比，避免词法前缀不一致误判「未找到模块根」（见 alias_build_materializes_to_versioned_relative_paths）。
    let from_dir = strip_verbatim(from_dir);
    let root = strip_verbatim(root);
    let mut cur = Some(from_dir.as_path());
    while let Some(d) = cur {
        if !d.starts_with(&root) {
            break;
        }
        if d.join("manifest.yaml").is_file() {
            return Some(d.to_path_buf());
        }
        if d == root {
            break;
        }
        cur = d.parent();
    }
    None
}

/// 路径是否落在 node_modules 内（第三方包文件）。
fn under_node_modules(p: &Path) -> bool {
    p.components().any(|c| c.as_os_str() == "node_modules")
}

/// 别名解析：`#<path>` = 本模块根锚点；`#/<path>` = src 根锚点（首段须为模块目录名）。
/// 后缀探针与相对导入同口径（复用 resolve_relative）。别名路径禁 `..`/空段/`\`
/// ——纵深防御：别名不得借路径段逃逸锚点，也不与「相对导入逃逸即报错」语义分叉。
pub fn resolve_alias(
    spec: &str,
    from_dir: &Path,
    root: &Path,
    ts: bool,
) -> Result<PathBuf, String> {
    // release（ts=false）下别名不应存在：`oj build` 已把它们实化为版本目录相对路径，并断言
    // 产物内无残留 `#`。走到这里若"半可解析"（同模块命中、跨模块悬空）比直接报错更坏。
    if !ts {
        return Err(format!(
            "cannot resolve alias '{spec}': release 产物不应含 `#` 别名（`oj build` 会实化为\
             相对路径）\n  下一步：重新 oj build；若该 specifier 来自 tests/ 或任务池，\
             改用相对路径（别名只能在模块内的文件里使用）"
        ));
    }
    let rest = &spec[1..]; // 去过 '#'；调用方已保证非空
    let from_src_root = rest.starts_with('/');
    let rel = if from_src_root { &rest[1..] } else { rest };
    let segs: Vec<&str> = rel.split('/').collect();
    if segs.iter().any(|s| s.is_empty() || *s == "." || *s == "..") || rel.contains('\\') {
        return Err(format!(
            "cannot resolve alias '{spec}': 别名路径不得含空段/`.`/`..`/`\\`（锚点已固定，无需上溯）"
        ));
    }
    let module_root = module_root_of(from_dir, root).ok_or_else(|| {
        format!(
            "cannot resolve alias '{spec}' from '{}': 逐级上溯未找到模块根（manifest.yaml）\
             ——别名只能在模块内的文件里使用（tests 目录与 tasks 池在模块外，请用相对路径）；\
             上溯以 project root（{}）为界，--api-path 在 project root 之外时同样不可用",
            from_dir.display(),
            root.display()
        )
    })?;
    let anchor = if from_src_root {
        let src_root = module_root.parent().ok_or_else(|| {
            format!(
                "module root {} has no parent (src root)",
                module_root.display()
            )
        })?;
        let first = segs[0];
        if !src_root.join(first).join("manifest.yaml").is_file() {
            let known = module_names(src_root);
            return Err(format!(
                "cannot resolve alias '{spec}': 首段 {first:?} 不是模块目录（{} 下无 {first}/manifest.yaml）{}\n  \
                 下一步：改用已存在的模块名，或把共享代码放进某个模块",
                src_root.display(),
                if known.is_empty() {
                    String::new()
                } else {
                    format!("；现有模块：[{}]", known.join(", "))
                }
            ));
        }
        src_root.to_path_buf()
    } else {
        module_root
    };
    resolve_relative(&anchor, &segs.join("/"), ts)
}

/// src 根下含 manifest.yaml 的目录名（别名报错提示用；仅在错误路径调用）。
fn module_names(src_root: &Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(src_root)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().join("manifest.yaml").is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    out.sort();
    out
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
    let sub: Vec<&str> = if pkg.contains('/') {
        parts.split_off(2)
    } else {
        parts.split_off(1)
    };

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
    Err(format!(
        "package '{}' has no entry (module/main/index.js)",
        pkg_dir.display()
    ))
}

/// 版本化 specifier：file://<abs>?v=<mtime nanos>（mtime 变 → 新模块 → 热重载）。
pub fn versioned_specifier(path: &Path) -> Result<ModuleSpecifier, String> {
    let mtime = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map_err(|e| format!("stat {}: {e}", path.display()))?;
    let nanos = mtime
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|e| format!("bad mtime on {}: {e}", path.display()))?;
    let abs =
        std::fs::canonicalize(path).map_err(|e| format!("canonicalize {}: {e}", path.display()))?;
    let mut url = ModuleSpecifier::from_file_path(abs)
        .map_err(|_| format!("cannot build file url from {}", path.display()))?;
    url.set_query(Some(&format!("v={}", nanos.as_nanos())));
    Ok(url)
}

/// project_root 钳制：解析结果 canonical 化后必须仍在 root 内。
/// lexical `..` 归一化可组合出根外路径（specifier 来自项目文件，属纵深防御）。
/// 双侧 canonical 化对齐符号链接（如 macOS 的 /var → /private/var），避免误伤。
///
/// 返回**已 canonical 化的句柄**：调用方（mail `{path}` 附件）按它读盘，
/// 避免「校验用的路径 ≠ 读盘用的路径」被符号链接替换（design §9 TOCTOU）。
pub(crate) fn ensure_within(p: &Path, root: &Path) -> Result<PathBuf, String> {
    let cp = std::fs::canonicalize(p).map_err(|e| format!("stat {}: {e}", p.display()))?;
    let cr = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    if cp.starts_with(&cr) {
        Ok(cp)
    } else {
        Err(format!(
            "module path {} escapes project root {}",
            p.display(),
            root.display()
        ))
    }
}

/// CJS 启发式：无 ESM 顶层语法且是 .js/.cjs（node_modules 包）。
/// ponytail: 启发式覆盖主流简单包；误判时报错信息可定位（module is not defined）。
pub fn looks_cjs(src: &str) -> bool {
    !src.contains("export ")
        && !src.contains("export{")
        && !src.contains("import ")
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
        .ok_or_else(|| {
            JsErrorBox::generic("project root not configured (loader wiring pending)")
        })?;
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
        assert!(
            resolve_relative(&dir, "../_shared/validate", ts)
                .unwrap()
                .ends_with("validate.ts")
        );
        assert!(
            resolve_relative(&dir, "../_shared/mod", ts)
                .unwrap()
                .ends_with("mod/index.ts")
        );
        assert!(
            resolve_relative(&dir, "../plain", ts)
                .unwrap()
                .ends_with("plain.js")
        );
        let err = resolve_relative(&dir, "../nope", ts).unwrap_err();
        assert!(err.contains("tried"), "{err}");
    }

    /// 别名夹具：两个模块（m1 / user），m1 下挖 8 层深目录模拟真实 handler。
    fn alias_fx(tag: &str) -> (PathBuf, PathBuf) {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "oj-alias-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let mf = |n: &str| format!("name: {n}\ndesc: d\nversion: 0.1.0\n");
        let files: Vec<(String, String)> = vec![
            ("src/m1/manifest.yaml".into(), mf("m1")),
            (
                "src/m1/_shared/validate.ts".into(),
                "export const v = 1;\n".into(),
            ),
            (
                "src/m1/_shared/mod/index.ts".into(),
                "export const x = 1;\n".into(),
            ),
            ("src/user/manifest.yaml".into(), mf("user")),
            (
                "src/user/_shared/util.ts".into(),
                "export const u = 1;\n".into(),
            ),
        ];
        for (rel, content) in files {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        }
        let deep = root.join("src/m1/a/b/c/d/e/f/g");
        std::fs::create_dir_all(&deep).unwrap();
        (root, deep)
    }

    #[test]
    fn alias_resolves_module_root_and_src_root() {
        let (root, deep) = alias_fx("ok");
        // 本模块根锚点：与目录深度无关。
        assert!(
            resolve_alias("#_shared/validate", &deep, &root, true)
                .unwrap()
                .ends_with("src/m1/_shared/validate.ts")
        );
        // 显式后缀（用户原话的 `import 'xxx.ts'` 形态）。
        assert!(
            resolve_alias("#_shared/validate.ts", &deep, &root, true)
                .unwrap()
                .ends_with("validate.ts")
        );
        // 目录索引。
        assert!(
            resolve_alias("#_shared/mod", &deep, &root, true)
                .unwrap()
                .ends_with("mod/index.ts")
        );
        // src 根锚点（跨模块）。
        assert!(
            resolve_alias("#/user/_shared/util", &deep, &root, true)
                .unwrap()
                .ends_with("src/user/_shared/util.ts")
        );
        // 深目录与模块根等价（同模块）。
        assert_eq!(
            resolve_alias("#_shared/validate", &deep, &root, true).unwrap(),
            resolve_alias("#_shared/validate", &root.join("src/m1"), &root, true).unwrap()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn alias_errors_are_actionable() {
        let (root, deep) = alias_fx("err");
        // 未知模块名 → 列出实际存在的模块 + 下一步。
        let e = resolve_alias("#/nope/x", &deep, &root, true).unwrap_err();
        assert!(
            e.contains("nope") && e.contains("m1") && e.contains("下一步"),
            "{e}"
        );
        // 目标不存在 → 列出尝试过的候选（resolve_relative 口径）。
        let e = resolve_alias("#_shared/nope", &deep, &root, true).unwrap_err();
        assert!(e.contains("tried"), "{e}");
        // 别名路径不得含空段 / `.` / `..` / `\`（纵深防御）。
        for bad in ["#../x", "#/user/../x", "#//x", "#_shared//x", "#a\\b", "#."] {
            let e = resolve_alias(bad, &deep, &root, true).unwrap_err();
            assert!(e.contains("不得含"), "{bad}: {e}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn alias_requires_module_ancestor() {
        // 模块外（tests 目录 / tasks 池）无 manifest.yaml 祖先 → 明确报错并给下一步。
        let root = fx(&[("tests/x.test.ts", "export const x = 1;\n")]);
        let outside = root.join("tests");
        assert!(module_root_of(&outside, &root).is_none());
        let e = resolve_alias("#_shared/validate", &outside, &root, true).unwrap_err();
        assert!(e.contains("manifest.yaml") && e.contains("相对路径"), "{e}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 回归（Windows）：`canonicalize` 给 referrer 目录加 `\\?\` 前缀，而
    /// `ModuleSpecifier::to_file_path` 还原时剥掉——同一条长名路径仅差此前缀。
    /// `module_root_of` 此前用词法 `starts_with`，前缀不一致即误判「未找到模块根」
    /// （见 build_cmd 的 alias_build_materializes_to_versioned_relative_paths 在 CI 失败）。
    /// 本用例仅 Windows 编译运行（macOS/Linux canonicalize 不带前缀，无此问题）。
    #[test]
    #[cfg(windows)]
    fn module_root_of_tolerates_verbatim_prefix_mismatch() {
        let (root, _deep) = alias_fx("verbatim");
        let canon = root.canonicalize().unwrap(); // 长名 + `\\?\`
        let with_prefix = canon.join("src/m1/a/b"); // referrer（canonical，带前缀）
        let no_prefix = strip_verbatim(&with_prefix); // 长名无前缀（模拟 to_file_path）

        // 方向一：referrer 无前缀 vs project_root 带前缀。
        assert_eq!(
            module_root_of(&no_prefix, &with_prefix),
            Some(strip_verbatim(&canon.join("src/m1"))),
            "no-prefix from_dir vs \\?\\-prefixed root 应命中模块根"
        );
        // 方向二：referrer 带前缀 vs project_root 无前缀。
        assert_eq!(
            module_root_of(&with_prefix, &no_prefix),
            Some(canon.join("src/m1")),
            "\\?\\-prefixed from_dir vs no-prefix root 应命中模块根"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_inner_enables_alias_only_outside_node_modules() {
        let (root, deep) = alias_fx("nm");
        let loader = OjModuleLoader {
            inner: Arc::new(LoaderShared {
                project_root: root.clone(),
                ts: true,
            }),
        };
        let referrer = |p: &Path| ModuleSpecifier::from_file_path(p).unwrap().to_string();
        // 模块内文件：`#` 走别名，且经 ensure_within + ?v= 版本化。
        let url = loader
            .resolve_inner("#_shared/validate", &referrer(&deep.join("api.ts")))
            .unwrap();
        assert!(
            url.as_str().contains("m1/_shared/validate.ts") && url.as_str().contains("?v="),
            "{url}"
        );
        // node_modules 内的文件不启用别名（第三方包自己的 package.json#imports 语义）
        // → 回落裸 specifier 解析，报错文案指向 node_modules 而非别名。
        let nm = root.join("node_modules/pkg");
        std::fs::create_dir_all(&nm).unwrap();
        let e = loader
            .resolve_inner("#foo", &referrer(&nm.join("index.js")))
            .unwrap_err();
        assert!(e.contains("node_modules"), "{e}");
        assert!(!e.contains("逐级上溯未找到模块根"), "{e}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// release（ts=false）下别名一律拒绝：`oj build` 已实化，走到这里说明产物不干净或
    /// 用法越界（tests/ 与任务池）。半可解析（同模块命中、跨模块悬空）比直接报错更坏。
    #[test]
    fn alias_rejected_in_release_mode() {
        let (root, deep) = alias_fx("rel");
        let e = resolve_alias("#_shared/validate", &deep, &root, false).unwrap_err();
        assert!(e.contains("release") && e.contains("oj build"), "{e}");
        assert!(e.contains("相对路径"), "{e}");
        let _ = std::fs::remove_dir_all(&root);
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
            (
                "node_modules/escape-goat/package.json",
                r#"{"name":"escape-goat","version":"4.0.0","type":"module"}"#,
            ),
            (
                "node_modules/cjspkg/main.js",
                "module.exports = { n: 1 };\n",
            ),
            (
                "node_modules/cjspkg/package.json",
                r#"{"name":"cjspkg","version":"1.0.0","main":"main.js"}"#,
            ),
            (
                "node_modules/withmod/pkg/lib/util.js",
                "export const u = 1;\n",
            ),
            (
                "node_modules/withmod/pkg/package.json",
                r#"{"name":"withmod"}"#,
            ),
        ]);
        let from = root.join("src/user");
        // ESM 包：type:module → index.js。
        assert!(
            resolve_bare("escape-goat", &from, &root)
                .unwrap()
                .ends_with("escape-goat/index.js")
        );
        // CJS 包：main 字段。
        assert!(
            resolve_bare("cjspkg", &from, &root)
                .unwrap()
                .ends_with("cjspkg/main.js")
        );
        // subpath 直映射。
        assert!(
            resolve_bare("withmod/pkg/lib/util.js", &from, &root)
                .unwrap()
                .ends_with("lib/util.js")
        );
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
            inner: Arc::new(LoaderShared {
                project_root: root.clone(),
                ts: true,
            }),
        };
        let referrer = ModuleSpecifier::from_file_path(root.join("src/user/mod.js"))
            .unwrap()
            .to_string();
        // 根内相对导入不受影响。
        assert!(loader.resolve_inner("./mod.js", &referrer).is_ok());
        // lexical `..` 越过项目根 → 钳制报错（而非解析成功）。
        let e = loader
            .resolve_inner("../../../escape.js", &referrer)
            .unwrap_err();
        assert!(e.contains("escapes project root"), "{e}");
    }

    /// `ensure_within`（mail 附件 `{path}` 钳制复用）：根内路径放行且**返回 canonical 句柄**
    /// （调用方按它读盘，规避「校验用的路径 ≠ 读盘用的路径」的 TOCTOU 面）；
    /// 词法 `..` 越界 / 根外绝对路径一律拒绝。
    #[test]
    fn ensure_within_returns_canonical_and_rejects_escape() {
        let (root, _deep) = alias_fx("within");
        let inside = root.join("src/m1/_shared/validate.ts");
        let got = ensure_within(&inside, &root).unwrap();
        assert!(got.is_absolute(), "{}", got.display());
        assert_eq!(got, inside.canonicalize().unwrap());
        // 根外（上跳 + 兄弟文件）→ 拒绝。
        let outside = root
            .parent()
            .unwrap()
            .join(format!("oj-within-escape-{}.txt", std::process::id()));
        std::fs::write(&outside, b"x").unwrap();
        let e = ensure_within(&outside, &root).unwrap_err();
        assert!(e.contains("escapes project root"), "{e}");
        // 不存在的路径同样拒绝（canonicalize 失败即 Err，不会放行）。
        assert!(ensure_within(&root.join("nope.txt"), &root).is_err());
        assert!(ensure_within(std::path::Path::new("/etc/hosts"), &root).is_err());
        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cjs_detection_and_wrap() {
        assert!(looks_cjs("module.exports = { a: 1 };\n"));
        assert!(!looks_cjs("export default 1;\n"));
        assert!(!looks_cjs("import x from 'y';\nmodule.exports = x;\n"));
        let wrapped = wrap_cjs("module.exports = { a: 1 };\n", "/nm/p/main.js");
        assert!(wrapped.contains("__oj_cjs_module"), "{wrapped}");
        assert!(
            wrapped.contains("export default __oj_cjs_module.exports"),
            "{wrapped}"
        );
        // require 绑定模块自身路径（嵌套 require 的 referrer）。
        assert!(
            wrapped.contains(r#"__ojRequire(n, "/nm/p/main.js")"#),
            "{wrapped}"
        );
    }
}
