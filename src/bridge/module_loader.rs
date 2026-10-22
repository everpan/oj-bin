//! oj 的 ESM 模块加载器：相对导入（Deno 风格补全）+ 裸 specifier（node_modules，T8）
//! 与 CJS 包装互操作。`?v=<mtime>` 版本化 specifier 让 V8 模块缓存天然按内容失效。
//! 注意：旧版本模块不可卸载，按编辑次数缓慢积累（dev 重启清零，release 有界）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
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
use super::module_scan::ModuleIndex;
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
    /// 模块索引（`--module-root` 等可配置模块根布局下的发现结果）。空 = 今日布局
    /// （源码树首层子目录 = 模块根），别名解析回落到「首段即模块目录名」。
    pub module_index: Option<Arc<ModuleIndex>>,
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
            resolve_alias(
                specifier,
                &ref_dir,
                &self.inner.project_root,
                self.inner.ts,
                self.module_index.as_deref(),
            )?
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
        let code: std::sync::Arc<str> = if looks_cjs(&src) {
            std::sync::Arc::from(wrap_cjs(&src, &path.display().to_string()).as_str())
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
///
/// **UNC 专项**：canonicalize 对网络共享返回 `\\?\UNC\server\share\...`。盲目剥
/// `\\?\` 会得到 `UNC\server\share\...`——一个没有根锚的坏路径（`from_file_path`
/// 建不出 URL、`starts_with` 全线失配）。verbatim UNC 的正确还原是 `\\server\share\...`。
pub fn strip_verbatim(p: &Path) -> PathBuf {
    let s = p.as_os_str().to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        p.to_path_buf()
    }
}

/// （仅测试）建符号链接；返回是否成功。Windows 无符号链接特权（未开 Developer
/// Mode）时失败 → 调用方 skip 而非 panic，让逃逸类用例在有特权的主机（含 CI）
/// 真跑、无特权的主机安静让路。
#[cfg(test)]
pub(crate) fn try_symlink(target: &Path, link: &Path, dir: bool) -> bool {
    #[cfg(unix)]
    {
        let _ = dir;
        std::os::unix::fs::symlink(target, link).is_ok()
    }
    #[cfg(windows)]
    {
        if dir {
            std::os::windows::fs::symlink_dir(target, link).is_ok()
        } else {
            std::os::windows::fs::symlink_file(target, link).is_ok()
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (target, link, dir);
        false
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
    // 同理（且更常见）：referrer 来自 file URL（`versioned_specifier` 对模块路径先
    // canonicalize 再建 URL，url crate 的 to_file_path 还原时剥掉 `\\?\`；macOS 上
    // `/tmp` → `/private/tmp` 即此路径），而 project_root 常是原样 CLI/config 路径——
    // **双侧 canonical 化**后比较，否则「明明在模块内」也会误判未找到。canonicalize 失败
    // （目录不存在，如纯静态占位根 / 推演用的虚拟目录）→ 回落词法口径；此时若 root
    // 已 canonical 化，把 from_dir 的词法前缀换成 canonical root，保证两侧同形态。
    //
    // **大小写**：Rust 的 starts_with/strip_prefix 按分量大小写敏感比较，而 Windows
    // 文件系统大小写不敏感。本函数无需额外处理：成功侧两侧都来自 canonicalize
    // （GetFinalPathNameByHandleW 给出磁盘真名，大小写天然一致）；词法回落侧 root 与
    // from_dir 同源（同一 CLI/config 值派生的路径串，大小写逐字相同）。canonicalize
    // 亦顺带归一 8.3 短名（`RUNNER~1` → 长名），两侧同走该口径。
    let croot = std::fs::canonicalize(root)
        .map(|p| strip_verbatim(&p))
        .unwrap_or_else(|_| strip_verbatim(root));
    let from_dir = match std::fs::canonicalize(from_dir) {
        Ok(p) => strip_verbatim(&p),
        Err(_) => strip_verbatim(from_dir)
            .strip_prefix(strip_verbatim(root))
            .map(|tail| croot.join(tail))
            .unwrap_or_else(|_| strip_verbatim(from_dir)),
    };
    let root = croot;
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

/// 别名解析：`#<path>` = 本模块根锚点；`#/<path>` = 模块索引锚点（首段 = **`manifest.name`**）。
/// 后缀探针与相对导入同口径（复用 resolve_relative）。别名路径禁 `..`/空段/`\`
/// ——纵深防御：别名不得借路径段逃逸锚点，也不与「相对导入逃逸即报错」语义分叉。
///
/// `index`：模块索引（可配置模块根布局下的发现结果）。**空 = 今日布局**，此时首段按
/// 「src 根下的模块目录名」探测（行为逐字不变）；非空则首段一律按 `manifest.name` 解析
/// ——模块根可不在首层后，「目录名 == 模块名」不再成立，锚点只能由索引给出。
pub fn resolve_alias(
    spec: &str,
    from_dir: &Path,
    root: &Path,
    ts: bool,
    index: Option<&ModuleIndex>,
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
    // `#/m/x` 的剩余段：锚点落在目标模块根时首段（模块名）已被消费，只留其后的路径；
    // 锚点在 src 根时首段是目录名，须保留（今日布局下两者同值）。
    let mut tail: &str = &segs.join("/");
    let anchor = if from_src_root {
        let first = segs[0];
        // 索引优先：首段是模块名（`manifest.name`），锚点即该模块的模块根。
        let by_name = index.and_then(|idx| idx.get(first));
        let known = known_module_names(index, &module_root);
        match by_name {
            Some(e) => {
                tail = rel[first.len()..].trim_start_matches('/');
                if tail.is_empty() {
                    return Err(format!(
                        "cannot resolve alias '{spec}': `#/{first}` 只给了模块名，未指向模块内文件\n  \
                         下一步：`#/{first}/<模块内相对路径>`"
                    ));
                }
                e.root.clone()
            }
            // 无索引（今日布局）→ 沿用「首段 = src 根下的模块目录名」探测。
            None if index.is_none() => {
                let src_root = module_root.parent().ok_or_else(|| {
                    format!(
                        "module root {} has no parent (src root)",
                        module_root.display()
                    )
                })?;
                if !src_root.join(first).join("manifest.yaml").is_file() {
                    return Err(unknown_module_err(
                        spec,
                        first,
                        &src_root.display().to_string(),
                        &known,
                    ));
                }
                src_root.to_path_buf()
            }
            None => return Err(unknown_module_err(spec, first, "模块索引", &known)),
        }
    } else {
        module_root
    };
    resolve_relative(&anchor, tail, ts)
}

/// 别名报错用的已知模块名：有索引取索引（= `manifest.name`），无索引回落到 src 根扫描。
fn known_module_names(index: Option<&ModuleIndex>, module_root: &Path) -> Vec<String> {
    if let Some(idx) = index {
        return idx.names().cloned().collect();
    }
    module_root.parent().map(module_names).unwrap_or_default()
}

/// 「首段不是模块」的统一报错（下一步提示 + 现有模块清单）。
fn unknown_module_err(spec: &str, first: &str, scope: &str, known: &[String]) -> String {
    format!(
        "cannot resolve alias '{spec}': 首段 {first:?} 不是模块名（{scope} 中没有它）{}\n  \
         下一步：改用已存在的模块名（manifest.yaml 的 name），或把共享代码放进某个模块",
        if known.is_empty() {
            String::new()
        } else {
            format!("；现有模块：[{}]", known.join(", "))
        }
    )
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

/// node: 内建模块名（require('path') 报错提示用，区别于「包未安装」）。
/// ponytail: 常用 38 个；新增内建名按需补（漏网者落入「包未安装」提示，不静默）。
const NODE_BUILTINS: &[&str] = &[
    "assert",
    "buffer",
    "child_process",
    "cluster",
    "console",
    "constants",
    "crypto",
    "dgram",
    "dns",
    "domain",
    "events",
    "fs",
    "http",
    "http2",
    "https",
    "inspector",
    "module",
    "net",
    "os",
    "path",
    "perf_hooks",
    "process",
    "punycode",
    "querystring",
    "readline",
    "repl",
    "stream",
    "string_decoder",
    "sys",
    "timers",
    "tls",
    "tty",
    "url",
    "util",
    "v8",
    "vm",
    "worker_threads",
    "zlib",
];

/// 裸 specifier 解析（Node 算法简化版）：
/// pkg → <dir>/node_modules/<pkg>（从 from_dir 逐级向上至 root）→ package.json
/// 的 exports（有 exports 即完全接管，Node 语义：未命中报错，不回落 legacy）→
/// 否则 legacy module → main → index.js；subpath（pkg/a.js）直映射包内文件。
/// require = true 时条件优先级走 require（CJS require 路径）。
pub fn resolve_bare(spec: &str, from_dir: &Path, root: &Path) -> Result<PathBuf, String> {
    resolve_bare_mode(spec, from_dir, root, false)
}

fn resolve_bare_mode(
    spec: &str,
    from_dir: &Path,
    root: &Path,
    require: bool,
) -> Result<PathBuf, String> {
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
            // exports 优先：包已找到即定型，未命中不继续向上走（Node ERR_PACKAGE_PATH_NOT_EXPORTED）。
            if let Some(exports) = read_pkg_json(&nm).and_then(|pj| pj.get("exports").cloned()) {
                let subpath = if sub.is_empty() {
                    ".".to_string()
                } else {
                    format!("./{}", sub.join("/"))
                };
                return resolve_exports(&nm, &exports, &subpath, require);
            }
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

/// 读取包目录 package.json（不存在/非法 JSON → None，回落 legacy 解析）。
fn read_pkg_json(pkg_dir: &Path) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(pkg_dir.join("package.json")).ok()?;
    serde_json::from_str(&text).ok()
}

/// exports 字段解析（Node 算法子集）：返回包内目标文件的绝对路径。
/// subpath = "."（裸包名）或 "./a/b"。require 决定活跃条件
/// （ESM: import/node/default；CJS: require/node/default；"node" 恒命中——oj 即 node 类宿主）。
fn resolve_exports(
    pkg_dir: &Path,
    exports: &serde_json::Value,
    subpath: &str,
    require: bool,
) -> Result<PathBuf, String> {
    // 数组 fallback 的存在性判定：归一后在包内且是文件（逃逸检查在终选后仍做）。
    let pd = normalize_lexically(pkg_dir);
    let exists = |t: &str| -> bool {
        let p = normalize_lexically(&pkg_dir.join(t.trim_start_matches("./")));
        p.starts_with(&pd) && p.is_file()
    };
    let target = if exports
        .as_object()
        .is_some_and(|o| o.keys().any(|k| k.starts_with('.')))
    {
        // 子路径映射表：精确键 → "./x/*" 模式键（Node 亦支持裸 "*" 键）。
        let obj = exports.as_object().unwrap();
        let mut hit: Option<(&serde_json::Value, String)> =
            obj.get(subpath).map(|v| (v, String::new()));
        if hit.is_none() {
            for (k, v) in obj {
                let cap = if k == "*" {
                    subpath.strip_prefix("./").map(str::to_string)
                } else if let Some(prefix) = k.strip_suffix("/*") {
                    subpath
                        .strip_prefix(&format!("{prefix}/"))
                        .map(str::to_string)
                } else {
                    None
                };
                if let Some(cap) = cap {
                    hit = Some((v, cap));
                    break;
                }
            }
        }
        let (val, cap) = hit.ok_or_else(|| not_exported_err(pkg_dir, subpath, obj.keys()))?;
        // 数组 fallback 需感知文件存在（Node 语义：项解析不到文件 → 试下一项）。
        pick_target(val, require, &cap, &exists, false)
            .ok_or_else(|| not_exported_err(pkg_dir, subpath, obj.keys()))?
    } else {
        // 无子路径键：整个 exports = "." 的目标（字符串/数组/条件对象）。
        pick_target(exports, require, "", &exists, false)
            .ok_or_else(|| not_exported_err(pkg_dir, subpath, std::iter::empty()))?
    };
    // 目标必须是 "./" 开头的包内路径，且词法上不逃逸包目录。
    let p = normalize_lexically(&pkg_dir.join(target.trim_start_matches("./")));
    let pd = normalize_lexically(pkg_dir);
    if !target.starts_with("./") || !p.starts_with(&pd) {
        return Err(format!(
            "\"exports\" target '{target}' escapes package '{}'",
            pkg_dir.display()
        ));
    }
    if !p.is_file() {
        return Err(format!(
            "\"exports\" target '{target}' not found ({}); check the package's files",
            p.display()
        ));
    }
    Ok(p)
}

/// ERR_PACKAGE_PATH_NOT_EXPORTED 风格报错（含实际导出的子路径，可定位）。
fn not_exported_err<'a>(
    pkg_dir: &Path,
    subpath: &str,
    keys: impl Iterator<Item = &'a String>,
) -> String {
    let keys: Vec<&str> = keys.map(|k| k.as_str()).collect();
    format!(
        "Package subpath '{subpath}' is not defined by \"exports\" in '{}'/package.json{}",
        pkg_dir.display(),
        if keys.is_empty() {
            String::new()
        } else {
            format!("；导出的子路径：[{}]", keys.join(", "))
        }
    )
}

/// 条件目标选择：字符串 → shape 合法即命中（单目标的存在性由调用方终检，报
/// "not found"）；数组 → 首个 shape 合法**且文件存在**的项（Node fallback 语义：
/// 解析不到文件试下一项）；对象 → **按 key 顺序**取第一个命中的活跃条件
/// （serde_json preserve_order 保插入序；node/default 恒命中，import/require 按
/// require 取一，types/browser 等未实现条件一律不命中）。
/// 命中值中的 `*` 用 cap（模式捕获）替换。
/// ponytail: 数组内非法项跳过而非报错（Node 对非法 target 更严格）；够用再收紧。
fn pick_target(
    v: &serde_json::Value,
    require: bool,
    cap: &str,
    exists: &impl Fn(&str) -> bool,
    check_exists: bool,
) -> Option<String> {
    match v {
        serde_json::Value::String(s) => {
            let t = s.replace('*', cap);
            if s.starts_with("./") && (!check_exists || exists(&t)) {
                Some(t)
            } else {
                None
            }
        }
        serde_json::Value::Array(a) => a
            .iter()
            .find_map(|e| pick_target(e, require, cap, exists, true)),
        serde_json::Value::Object(o) => {
            for (k, sub) in o {
                let active = match k.as_str() {
                    "default" | "node" => true,
                    "require" => require,
                    "import" => !require,
                    _ => false,
                };
                if active && let Some(s) = pick_target(sub, require, cap, exists, check_exists) {
                    return Some(s);
                }
            }
            None
        }
        _ => None,
    }
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

/// release 模式（ts=false）的 driver spec 冻结缓存（perf F3）：每路径首请求算一次
/// （stat + canonicalize），之后纯内存命中。release 服务 manifests 聚合产物、热重载
/// 不适用（改文件须重跑 `oj build`），per-request 的 mtime 探测纯属白付——
/// versioned_specifier 的 stat + canonicalize 在深层路径上是 7-11 次 syscall。
/// dev（ts=true）保持逐请求版本化（热重载语义）。
pub(crate) fn driver_spec(path: &Path, ts: bool) -> Result<ModuleSpecifier, String> {
    if ts {
        return versioned_specifier(path);
    }
    static FROZEN: OnceLock<Mutex<HashMap<PathBuf, ModuleSpecifier>>> = OnceLock::new();
    let mut m = FROZEN
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap();
    if let Some(s) = m.get(path) {
        return Ok(s.clone());
    }
    let s = versioned_specifier(path)?;
    m.insert(path.to_path_buf(), s.clone());
    Ok(s)
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

/// CJS require 底座：相对（./ ../）+ 裸 specifier 解析（require 条件）、JSON 模块、
/// 读源码（JS 侧 __ojRequire eval 执行）。project_root 取 StableState.loader
/// （T9 oj 装配注入；未配置时报错）。
/// 循环 require：op 返回的包装体自带 globalThis.__ojCjsRun 注册表——同一绝对路径
/// 只执行一次，环上重复进入返回部分 exports（Node 语义），bootstrap 侧无改动。
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
    let p = if name.starts_with("./") || name.starts_with("../") {
        resolve_cjs_relative(&from, &name).map_err(JsErrorBox::generic)?
    } else {
        resolve_bare_mode(&name, &from, &root, true).map_err(|e| {
            if is_node_builtin(&name) {
                JsErrorBox::generic(format!(
                    "Node builtin '{name}' is not available in oj runtime"
                ))
            } else {
                JsErrorBox::generic(e)
            }
        })?
    };
    // 相对 require 可经 ../../ 越出 project root——载入侧钳制（ESM 路径 resolve_inner 已有）。
    let p = ensure_within(&p, &root).map_err(JsErrorBox::generic)?;
    let raw = std::fs::read_to_string(&p)
        .map_err(|e| JsErrorBox::generic(format!("read {}: {e}", p.display())))?;
    let code = if p.extension().is_some_and(|e| e == "json") {
        json_module_code(&raw).map_err(JsErrorBox::generic)?
    } else {
        cjs_registry_wrap(&raw, &p.display().to_string())
    };
    Ok(serde_json::json!({ "path": p.display().to_string(), "code": code }))
}

/// CJS 相对 require 解析：./ ../ → 候选依次 <p>、<p>.js、<p>.json、<p>/index.js、
/// <p>/index.json（存在即命中）。
fn resolve_cjs_relative(base_dir: &Path, spec: &str) -> Result<PathBuf, String> {
    let stem = normalize_lexically(&base_dir.join(spec));
    let candidates = [
        stem.clone(),
        stem.with_extension("js"),
        stem.with_extension("json"),
        stem.join("index.js"),
        stem.join("index.json"),
    ];
    for c in &candidates {
        if c.is_file() {
            return Ok(c.clone());
        }
    }
    let tried: Vec<String> = candidates.iter().map(|c| c.display().to_string()).collect();
    Err(format!(
        "cannot resolve '{spec}' from '{}': tried [{}]",
        base_dir.display(),
        tried.join(", ")
    ))
}

/// 是否 node: 内建模块名（剥 "node:" 前缀后取首段，兼容 'node:path' 与 'path/...' 误形）。
fn is_node_builtin(name: &str) -> bool {
    let head = name.split('/').next().unwrap_or(name);
    let head = head.strip_prefix("node:").unwrap_or(head);
    NODE_BUILTINS.contains(&head)
}

/// CJS 执行注册表包装体：首次 eval 注册并执行，后续（含循环 require）直接取
/// exports；module.exports 绑到 __ojRequire 的外层 m（缓存键 referrer::name 的载体）。
fn cjs_registry_wrap(src: &str, path: &str) -> String {
    // JSON 编码即合法 JS 字符串字面量（处理引号/反斜杠）。
    let p = serde_json::to_string(path).unwrap_or_else(|_| "\"\"".into());
    format!(
        "const __r = (globalThis.__ojCjsRun ??= {{}});\nif (!__r[{p}]) {{\nconst __m = {{ exports: {{}} }};\n__r[{p}] = __m;\n(function (module, exports, require) {{\n{src}\n}})(__m, __m.exports, (n) => globalThis.__ojRequire(n, {p}));\n}}\nmodule.exports = __r[{p}].exports;\n"
    )
}

/// JSON require：校验原文为合法 JSON 后包成 module.exports（值语义 = Node）。
fn json_module_code(raw: &str) -> Result<String, String> {
    serde_json::from_str::<serde_json::Value>(raw)
        .map_err(|e| format!("invalid JSON module: {e}"))?;
    Ok(format!("module.exports = {raw};"))
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
            resolve_alias("#_shared/validate", &deep, &root, true, None)
                .unwrap()
                .ends_with("src/m1/_shared/validate.ts")
        );
        // 显式后缀（用户原话的 `import 'xxx.ts'` 形态）。
        assert!(
            resolve_alias("#_shared/validate.ts", &deep, &root, true, None)
                .unwrap()
                .ends_with("validate.ts")
        );
        // 目录索引。
        assert!(
            resolve_alias("#_shared/mod", &deep, &root, true, None)
                .unwrap()
                .ends_with("mod/index.ts")
        );
        // src 根锚点（跨模块）。
        assert!(
            resolve_alias("#/user/_shared/util", &deep, &root, true, None)
                .unwrap()
                .ends_with("src/user/_shared/util.ts")
        );
        // 深目录与模块根等价（同模块）。
        assert_eq!(
            resolve_alias("#_shared/validate", &deep, &root, true, None).unwrap(),
            resolve_alias("#_shared/validate", &root.join("src/m1"), &root, true, None).unwrap()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 嵌套布局夹具：`src/foo/api`（name=m1）、`src/bar/api`（name=m2）。
    /// 返回 (src 根, m1 内的深层目录)。
    fn alias_fx_nested(tag: &str) -> (PathBuf, PathBuf) {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "oj-alias-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let files: Vec<(String, String)> = vec![
            (
                "src/foo/api/manifest.yaml".into(),
                "name: m1\ndesc: d\nversion: 0.1.0\n".into(),
            ),
            (
                "src/bar/api/manifest.yaml".into(),
                "name: m2\ndesc: d\nversion: 0.1.0\n".into(),
            ),
            (
                "src/bar/api/_shared/util.ts".into(),
                "export const u = 1;\n".into(),
            ),
        ];
        for (rel, content) in files {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        }
        let deep = root.join("src/foo/api/a/b/c");
        std::fs::create_dir_all(&deep).unwrap();
        (root, deep)
    }

    /// BDD：模块根可不在首层（目录名 ≠ 模块名）时，`#/m/x` 仍按 `manifest.name` 解析——
    /// 旧实现取 `module_root.parent()` 当 src 根，在嵌套布局下会算出 `src/apps` 而非
    /// `src`，跨模块别名必然落空。
    #[test]
    fn given_nested_module_roots_when_alias_cross_module_then_resolves_by_manifest_name() {
        // Given: 模块根在第二层，目录名与模块名无关
        let (root, deep) = alias_fx_nested("nested");
        let idx = crate::bridge::resolve_roots(&root.join("src"), true, None).unwrap();

        // When: 从 m1 深层目录跨模块引用 m2（按 manifest.name，不是目录名）
        let got = resolve_alias("#/m2/_shared/util", &deep, &root, true, Some(&idx)).unwrap();

        // Then: 命中 m2 的模块根（目录名 foo/bar 不参与解析）
        assert!(got.ends_with("bar/api/_shared/util.ts"), "{got:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Given 同上一例，但**不给索引**（今日布局）：`src/m1` 首段即模块名，行为不变。
    #[test]
    fn given_flat_tree_without_index_when_alias_cross_module_then_today_semantics() {
        let (root, deep) = alias_fx("flat");
        let got = resolve_alias("#/user/_shared/util", &deep, &root, true, None).unwrap();
        assert!(got.ends_with("src/user/_shared/util.ts"), "{got:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn alias_errors_are_actionable() {
        let (root, deep) = alias_fx("err");
        // 未知模块名 → 列出实际存在的模块 + 下一步。
        let e = resolve_alias("#/nope/x", &deep, &root, true, None).unwrap_err();
        assert!(
            e.contains("nope") && e.contains("m1") && e.contains("下一步"),
            "{e}"
        );
        // 目标不存在 → 列出尝试过的候选（resolve_relative 口径）。
        let e = resolve_alias("#_shared/nope", &deep, &root, true, None).unwrap_err();
        assert!(e.contains("tried"), "{e}");
        // 别名路径不得含空段 / `.` / `..` / `\`（纵深防御）。
        for bad in ["#../x", "#/user/../x", "#//x", "#_shared//x", "#a\\b", "#."] {
            let e = resolve_alias(bad, &deep, &root, true, None).unwrap_err();
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
        let e = resolve_alias("#_shared/validate", &outside, &root, true, None).unwrap_err();
        assert!(e.contains("manifest.yaml") && e.contains("相对路径"), "{e}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `strip_verbatim` 是**纯字符串函数**，此前只被下面的 `#[cfg(windows)]` 用例
    /// 覆盖（本机编不到也跑不了）。这里补一组平台无关断言：unix 上也编译运行，
    /// 让该函数的语义不再只能靠 Windows CI 兜底。
    ///
    /// UNC 专项（v0.1.85 销账）：`\\?\UNC\server\share` 曾盲目剥成
    /// `UNC\server\share`（无根锚的坏路径，此前只在本用例的文档注释里登记为已知
    /// 缺口）。`module_root_of` 双侧 canonicalize 后该形态走上主路径（UNC 部署 +
    /// `#` 别名必踩），故本期修掉。
    #[test]
    fn strip_verbatim_is_prefix_only_and_platform_agnostic() {
        // 剥 verbatim 前缀（`\\?\`）。
        assert_eq!(
            strip_verbatim(Path::new(r"\\?\C:\a\b")),
            PathBuf::from(r"C:\a\b")
        );
        // UNC verbatim → 还原成标准 UNC（`\\server\share`），不是 `UNC\server\share`。
        assert_eq!(
            strip_verbatim(Path::new(r"\\?\UNC\server\share\proj\src")),
            PathBuf::from(r"\\server\share\proj\src")
        );
        // 已是标准 UNC → 原样（不递归剥）。
        assert_eq!(
            strip_verbatim(Path::new(r"\\server\share\a")),
            PathBuf::from(r"\\server\share\a")
        );
        // 无前缀 / unix 形态 → 原样返回。
        assert_eq!(
            strip_verbatim(Path::new(r"C:\a\b")),
            PathBuf::from(r"C:\a\b")
        );
        assert_eq!(strip_verbatim(Path::new("/tmp/a")), PathBuf::from("/tmp/a"));
        // 幂等：只剥一层，剥过之后再调用不变（不递归剥、不剥中段）。
        let once = strip_verbatim(Path::new(r"\\?\C:\a"));
        assert_eq!(once, PathBuf::from(r"C:\a"));
        assert_eq!(strip_verbatim(&once), once);
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
        let canon = root.canonicalize().unwrap(); // 项目根（长名 + `\\?\`）
        let proj_root_prefix = canon.clone(); // project_root 带前缀
        let proj_root_plain = strip_verbatim(&canon); // project_root 无前缀
        let with_prefix = canon.join("src/m1/a/b"); // referrer（canonical，带前缀）
        let no_prefix = strip_verbatim(&with_prefix); // referrer 无前缀（模拟 to_file_path）
        let want = strip_verbatim(&canon.join("src/m1")); // 模块根（归一后必然无前缀）

        // 方向一：referrer 无前缀 vs project_root 带前缀。
        assert_eq!(
            module_root_of(&no_prefix, &proj_root_prefix),
            Some(want.clone()),
            "no-prefix from_dir vs \\?\\-prefixed root 应命中模块根"
        );
        // 方向二：referrer 带前缀 vs project_root 无前缀。
        // 函数对两者均 `strip_verbatim` 再比，故返回同样是无前缀的模块根。
        assert_eq!(
            module_root_of(&with_prefix, &proj_root_plain),
            Some(want),
            "\\?\\-prefixed from_dir vs no-prefix root 应命中模块根"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn module_root_of_tolerates_symlinked_project_root() {
        // 生产形态：referrer 来自 file URL（deno_core 已 realpath → /private/var/…），
        // 而 project_root 是原样 CLI/config 路径（macOS 上含 /var → /private/var symlink，
        // Windows 上可能是 subst/ junctions）——词法前缀比较会把「明明在模块内」误判成
        // 未找到模块根（oj openapi --module-root 实测复现）。双侧 canonicalize 修复。
        let (root, _deep) = alias_fx("symlink-root");
        let canon = root.canonicalize().unwrap();
        // 经符号链接访问项目根（链接名 ≠ 真实路径的最后一段）。
        let link = std::env::temp_dir().join(format!(
            "oj-alias-symlinked-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        if !try_symlink(&canon, &link, true) {
            eprintln!("skip: symlink unsupported on this host");
            return;
        }
        let referrer_dir = canon.join("src/m1/a/b"); // deno_core 侧：realpath 形态
        let project_root = link.clone(); // CLI/config 侧：经 symlink 的形态
        let want = canon.join("src/m1");

        assert_eq!(
            module_root_of(&referrer_dir, &project_root),
            Some(want),
            "symlinked project_root vs realpath referrer 应命中模块根"
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&link);
    }

    #[test]
    fn module_root_of_tolerates_virtual_dir_under_symlinked_root() {
        // 纯路径推演：referrer 目录**可以不存在**（`oj build` 的导入改写按目录串计算，
        // 不落盘）——canonicalize(from_dir) 失败时不能直接把词法路径与已 canonical 化的
        // root 混比（形态不一致 → 误判未找到）。按词法前缀换根，两侧保持同形态。
        let (root, _deep) = alias_fx("virtual");
        let canon = root.canonicalize().unwrap();
        let link = std::env::temp_dir().join(format!(
            "oj-alias-virtual-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        if !try_symlink(&canon, &link, true) {
            eprintln!("skip: symlink unsupported on this host");
            return;
        }
        // 经 symlink 的、且**不存在**的深层目录。
        let virtual_dir = link.join("src/m1/a/b/not/created/yet");
        assert_eq!(
            module_root_of(&virtual_dir, &link),
            Some(canon.join("src/m1")),
            "虚拟目录 + symlink 根应仍命中模块根"
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&link);
    }

    #[test]
    fn resolve_inner_enables_alias_only_outside_node_modules() {
        let (root, deep) = alias_fx("nm");
        let loader = OjModuleLoader {
            module_index: None,
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
        let e = resolve_alias("#_shared/validate", &deep, &root, false, None).unwrap_err();
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
            module_index: None,
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

    /// design §9/§11「路径穿越：`ensure_within` **双侧 canonicalize**，符号链接在覆盖内」——
    /// 词法 `..` 之外的第二条逃逸面：根**内**的符号链接指向根**外**文件（路径字符串完全在
    /// 根内，只有 canonicalize 能识破）。附件 `{path}` 与模块加载共用本函数。
    /// Windows 无符号链接特权（未开 Developer Mode）时跳过。
    #[test]
    fn ensure_within_rejects_symlink_escape() {
        let base = fx(&[("outside.txt", "secret\n"), ("proj/inside.txt", "ok\n")]);
        let root = base.join("proj");
        // 根内文件：放行（对照组，防「一律拒绝」的变异蒙混）。
        assert!(ensure_within(&root.join("inside.txt"), &root).is_ok());

        // 根内符号链接 → 根外文件。
        let file_link = root.join("link.txt");
        if !try_symlink(&base.join("outside.txt"), &file_link, false) {
            eprintln!("skip ensure_within_rejects_symlink_escape: symlink unavailable");
            let _ = std::fs::remove_dir_all(&base);
            return;
        }
        let e = ensure_within(&file_link, &root).unwrap_err();
        assert!(e.contains("escapes project root"), "{e}");

        // 根内符号链接 → 根外目录，再穿透一层。
        let dir_link = root.join("dirlink");
        assert!(try_symlink(&base, &dir_link, true), "symlink_dir failed");
        assert!(
            ensure_within(&dir_link.join("outside.txt"), &root).is_err(),
            "经目录符号链接穿透到根外也必须拒"
        );

        let _ = std::fs::remove_dir_all(&base);
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

    // ---------- exports / conditions（oj-1） ----------

    #[test]
    fn bare_resolves_exports_variants() {
        let root = fx(&[
            // 字符串形态 + 无 main 的现代包。
            (
                "node_modules/strpkg/package.json",
                r#"{"name":"strpkg","exports":"./index.js"}"#,
            ),
            ("node_modules/strpkg/index.js", "export const x = 1;\n"),
            // 条件对象：import/require 指向不同文件。
            (
                "node_modules/condpkg/package.json",
                r#"{
                "name":"condpkg",
                "exports":{".":{"import":"./esm.js","require":"./cjs.js","default":"./fallback.js"}}
            }"#,
            ),
            ("node_modules/condpkg/esm.js", "export const m = 'esm';\n"),
            ("node_modules/condpkg/cjs.js", "module.exports = 'cjs';\n"),
            (
                "node_modules/condpkg/fallback.js",
                "export const m = 'fb';\n",
            ),
            // 子路径键 + "./x/*" 模式。
            (
                "node_modules/subpkg/package.json",
                r#"{
                "name":"subpkg",
                "exports":{
                    "./feat": "./lib/feat.js",
                    "./deep/*": "./dist/*",
                    "./arr": ["./nope.js", "./arr.js"]
                }
            }"#,
            ),
            ("node_modules/subpkg/lib/feat.js", "export const f = 1;\n"),
            ("node_modules/subpkg/dist/a/b.js", "export const d = 1;\n"),
            ("node_modules/subpkg/arr.js", "export const a = 1;\n"),
        ]);
        let from = root.join("src/user");
        // 字符串形态（exports 无 "." 键，整体即 "." 目标）。
        assert!(
            resolve_bare("strpkg", &from, &root)
                .unwrap()
                .ends_with("strpkg/index.js")
        );
        // 条件：ESM 走 import，CJS require 走 require。
        assert!(
            resolve_bare("condpkg", &from, &root)
                .unwrap()
                .ends_with("condpkg/esm.js")
        );
        assert!(
            resolve_bare_mode("condpkg", &from, &root, true)
                .unwrap()
                .ends_with("condpkg/cjs.js")
        );
        // 子路径键与模式。
        assert!(
            resolve_bare("subpkg/feat", &from, &root)
                .unwrap()
                .ends_with("lib/feat.js")
        );
        assert!(
            resolve_bare("subpkg/deep/a/b.js", &from, &root)
                .unwrap()
                .ends_with("dist/a/b.js")
        );
        // 数组 fallback：首个不可解析项跳过，命中第二项。
        assert!(
            resolve_bare("subpkg/arr", &from, &root)
                .unwrap()
                .ends_with("subpkg/arr.js")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn bare_exports_unexported_subpath_errors_with_keys() {
        let root = fx(&[
            (
                "node_modules/expkg/package.json",
                r#"{
                "name":"expkg",
                "exports":{".":"./index.js","./ok":"./ok.js"}
            }"#,
            ),
            ("node_modules/expkg/index.js", "export const x = 1;\n"),
            ("node_modules/expkg/ok.js", "export const o = 1;\n"),
            // 显式 exports 之外的文件存在也不可见（Node 封闭语义）。
            ("node_modules/expkg/internal.js", "export const i = 1;\n"),
        ]);
        let from = root.join("src/user");
        assert!(resolve_bare("expkg/ok", &from, &root).is_ok());
        for spec in ["expkg/internal", "expkg/nope"] {
            let e = resolve_bare(spec, &from, &root).unwrap_err();
            assert!(e.contains("not defined by \"exports\""), "{spec}: {e}");
            assert!(e.contains("./ok"), "{spec}: {e}");
        }
        // 目标文件缺失：命中映射但文件不在 → 报错（而非回落 legacy）。
        let root2 = fx(&[(
            "node_modules/misspkg/package.json",
            r#"{"exports":{".":"./gone.js"}}"#,
        )]);
        let from2 = root2.join("src/user");
        let e = resolve_bare("misspkg", &from2, &root2).unwrap_err();
        assert!(e.contains("not found"), "{e}");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&root2);
    }

    /// pnpm 布局：node_modules/<pkg> 是指向 .pnpm/<pkg>@<v>/node_modules/<pkg> 的
    /// 符号链接；包的真实位置旁有同伴依赖（peer/dep 链接在 .pnpm/<pkg>@<v>/
    /// node_modules/ 下）。解析沿符号链接工作、且不得 realpath 中断相对回溯。
    /// Windows 无符号链接特权（未开 Developer Mode）时跳过。
    #[test]
    fn bare_resolves_pnpm_symlink_layout() {
        let root = fx(&[
            (
                "node_modules/.pnpm/pkg@1.0.0/node_modules/pkg/package.json",
                r#"{"name":"pkg","version":"1.0.0","exports":{".":"./index.js"}}"#,
            ),
            (
                "node_modules/.pnpm/pkg@1.0.0/node_modules/pkg/index.js",
                "export const x = 1;\n",
            ),
            (
                "node_modules/.pnpm/pkgA@1.0.0/node_modules/pkgA/package.json",
                r#"{"name":"pkgA","version":"1.0.0","main":"index.js"}"#,
            ),
            (
                "node_modules/.pnpm/pkgA@1.0.0/node_modules/pkgA/index.js",
                "module.exports = 1;\n",
            ),
            (
                "node_modules/.pnpm/pkgA@1.0.0/node_modules/depB/package.json",
                r#"{"name":"depB","version":"2.0.0","main":"index.js"}"#,
            ),
            (
                "node_modules/.pnpm/pkgA@1.0.0/node_modules/depB/index.js",
                "module.exports = 2;\n",
            ),
        ]);
        let nm = root.join("node_modules");
        // pnpm 风格的平铺链接视图。
        if !try_symlink(
            &root.join("node_modules/.pnpm/pkg@1.0.0/node_modules/pkg"),
            &nm.join("pkg"),
            true,
        ) {
            eprintln!("skip bare_resolves_pnpm_symlink_layout: symlink unavailable");
            let _ = std::fs::remove_dir_all(&root);
            return;
        }
        assert!(
            try_symlink(
                &root.join("node_modules/.pnpm/pkgA@1.0.0/node_modules/pkgA"),
                &nm.join("pkgA"),
                true
            ),
            "symlink_dir failed"
        );
        let from = root.join("src/user");
        // 平铺视图：经符号链接命中 exports。
        assert!(
            resolve_bare("pkg", &from, &root)
                .unwrap()
                .ends_with("pkg/index.js")
        );
        // 真实路径深处（versioned_specifier canonicalize 后的 referrer 形态）：
        // 向上回溯命中 .pnpm/<pkgA>@<v>/node_modules/ 下的同伴依赖。
        let deep = root.join("node_modules/.pnpm/pkgA@1.0.0/node_modules/pkgA/sub");
        assert!(
            resolve_bare_mode("depB", &deep, &root, true)
                .unwrap()
                .ends_with("depB/index.js")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---------- CJS 相对 require / JSON / 循环（oj-2） ----------

    #[test]
    fn cjs_relative_probes_js_json_and_index() {
        let root = fx(&[
            ("src/user/sib.js", "module.exports = 1;\n"),
            ("src/user/data.json", r#"{"k": 1}"#),
            ("src/user/mod/index.js", "module.exports = 2;\n"),
        ]);
        let dir = root.join("src/user");
        assert!(
            resolve_cjs_relative(&dir, "./sib")
                .unwrap()
                .ends_with("sib.js")
        );
        assert!(
            resolve_cjs_relative(&dir, "./data")
                .unwrap()
                .ends_with("data.json")
        );
        assert!(
            resolve_cjs_relative(&dir, "./mod")
                .unwrap()
                .ends_with("mod/index.js")
        );
        let e = resolve_cjs_relative(&dir, "./nope").unwrap_err();
        assert!(e.contains("tried"), "{e}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cjs_registry_wrap_is_idempotent_and_builtin_named() {
        // 注册表守卫：重复 eval 只执行一次（环上第二次进入跳过 src 执行）。
        let w = cjs_registry_wrap("module.exports.a = 1;\n", "/nm/p/a.js");
        assert!(w.contains("globalThis.__ojCjsRun"), "{w}");
        assert!(w.contains("if (!__r[\"/nm/p/a.js\"])"), "{w}");
        // 嵌套 require 的 referrer = 本模块绝对路径。
        assert!(
            w.contains(r#"globalThis.__ojRequire(n, "/nm/p/a.js")"#),
            "{w}"
        );
        // 最终 exports 恒绑注册表（外层 m 拿到的是注册表 exports 引用）。
        assert!(
            w.contains("module.exports = __r[\"/nm/p/a.js\"].exports"),
            "{w}"
        );
        // node: 内建识别（首段匹配，含 node: 前缀形态）。
        assert!(is_node_builtin("path"));
        assert!(is_node_builtin("node:fs"));
        assert!(is_node_builtin("fs/promises"));
        assert!(!is_node_builtin("fsx"));
    }

    #[test]
    fn cjs_json_module_code_validates_and_wraps() {
        let ok = json_module_code(r#"{"k": 1}"#).unwrap();
        assert_eq!(ok, r#"module.exports = {"k": 1};"#);
        let e = json_module_code("{not json").unwrap_err();
        assert!(e.contains("invalid JSON module"), "{e}");
    }
}
