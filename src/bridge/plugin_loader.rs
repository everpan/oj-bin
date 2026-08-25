//! PluginLoader：四级路径解析 + 清单/扫描双模式装配 + 七类失败分类（spec §4/§5）。
//! 约定：ABI_VERSION 严格相等是唯一硬门禁；构建指纹不符仅告警（eprintln）。

use super::ffi;
use oj_plugin_ffi::{ABI_VERSION, HOST_FINGERPRINT, HostContext, PluginDescriptor, RArc, RString};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Once};

/// 七类加载失败（spec §4），各自独立错误文案。
#[derive(Debug)]
pub enum PluginLoadError {
    FileMissing { path: PathBuf },
    /// 含 glibc 基线不满足。
    PlatformMismatch { path: PathBuf, detail: String },
    /// 透出 loader 原始错误。
    DependencyResolution { path: PathBuf, loader_text: String },
    AbiMismatch { plugin: u32, host: u32 },
    SymbolMissing { path: PathBuf, symbol: &'static str },
    IdentityMismatch { expected: String, actual: String },
    /// init 返回错误或 panic。
    InitFailed { name: String, detail: String },
}

impl fmt::Display for PluginLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FileMissing { path } => write!(f, "plugin file missing: {}", path.display()),
            Self::PlatformMismatch { path, detail } => {
                write!(f, "plugin platform mismatch: {} ({detail})", path.display())
            }
            Self::DependencyResolution { path, loader_text } => {
                write!(f, "plugin dependency resolution failed: {} ({loader_text})", path.display())
            }
            Self::AbiMismatch { plugin, host } => {
                write!(f, "plugin ABI mismatch: plugin={plugin} host={host} (rebuild plugin against oj-plugin-ffi ABI {host})")
            }
            Self::SymbolMissing { path, symbol } => {
                write!(f, "plugin symbol missing: {} (symbol `{symbol}`)", path.display())
            }
            Self::IdentityMismatch { expected, actual } => {
                write!(f, "plugin identity mismatch: expected '{expected}', got '{actual}'")
            }
            Self::InitFailed { name, detail } => {
                write!(f, "plugin init failed: {name} ({detail})")
            }
        }
    }
}

impl std::error::Error for PluginLoadError {}

/// 清单条目：插件名 + 可选 "@semver" pin（spec §决策表）。
#[derive(Debug, Clone)]
pub struct PluginManifestEntry {
    pub name: String,
    pub semver_pin: Option<String>,
}

/// init 后宿主取得的各轴工厂槽位（未实现轴为 None；blob/bus/kv 槽位随 4.2-4.4 加入）。
#[derive(Default)]
pub struct Registrations {
    pub es: Option<&'static oj_plugin_ffi::EsBackendVtable>, // Task 3.3 起填
    pub db: Option<&'static oj_plugin_ffi::DataAccessorVtable>, // Task 4.1 起填
}

pub struct LoadedPlugin {
    pub descriptor: PluginDescriptor,
    pub registrations: Registrations,
}

impl fmt::Debug for LoadedPlugin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoadedPlugin")
            .field("name", &&self.descriptor.name[..])
            .field("semver", &&self.descriptor.semver[..])
            .field("abi_version", &self.descriptor.abi_version)
            .finish()
    }
}

/// 插件自省信息（op_plugins 输出；JS `plugins()` → [{name, semver, abi_version, fingerprint,
/// host_abi_version}]，spec §4 升级核对 + §2 注册表自省并入）。
#[derive(Clone, serde::Serialize)]
pub struct PluginInfo {
    pub name: String,
    pub semver: String,
    pub abi_version: u32,
    pub fingerprint: String,
    /// 宿主当前 ABI_VERSION（插件不必与此一致，运维据此核对升级窗口）。
    pub host_abi_version: u32,
}

impl From<&LoadedPlugin> for PluginInfo {
    fn from(p: &LoadedPlugin) -> Self {
        Self {
            name: p.descriptor.name[..].to_string(),
            semver: p.descriptor.semver[..].to_string(),
            abi_version: p.descriptor.abi_version,
            fingerprint: p.descriptor.fingerprint[..].to_string(),
            host_abi_version: ABI_VERSION,
        }
    }
}

/// 从已加载插件的 es 注册槽构造 core 适配器（handle 0 = 单 es endpoint 约定）。
/// spec §3 适配器层：插件产 vtable，core 侧 `FfiEsBackend` 包装成 core trait 供 op 消费；
/// 构造放 core（ffi 模块 pub(crate)，unsafe 不外泄），装配层只经此安全入口。
pub fn es_backend(loaded: &LoadedPlugin) -> Option<Arc<dyn crate::bridge::EsBackend>> {
    loaded.registrations.es.map(|vt| {
        Arc::new(super::ffi::FfiEsBackend::new(0, vt)) as Arc<dyn crate::bridge::EsBackend>
    })
}

/// 从已加载插件的 db 注册槽构造 core 工厂（scheme 由插件 vtable 自报，Task 4.1）。
pub fn db_backend(loaded: &LoadedPlugin) -> Option<Arc<dyn crate::bridge::DbBackend>> {
    loaded.registrations.db.map(|vt| {
        Arc::new(super::ffi::FfiDbBackend::new(&loaded.descriptor.name[..], vt))
            as Arc<dyn crate::bridge::DbBackend>
    })
}

/// 加载路径四级解析（spec §4）：OJ_PLUGINS_DIR > oj.toml plugins_dir >
/// <exe>/plugins > build.rs dev 后备（workspace root 常量）。
/// relative 一律相对 oj.toml 所在目录（config_dir）。返回最终目录 = <plugins_dir>/<host-triple>/。
/// 显式配置 1/2 而目录不存在 → Err；默认 3/4 不存在 → Ok(None)（零插件）。
pub fn resolve_plugins_dir(
    config_dir: &Path,
    toml_plugins_dir: Option<&Path>,
) -> Result<Option<PathBuf>, String> {
    let abs = |p: &Path| {
        if p.is_absolute() { p.to_path_buf() } else { config_dir.join(p) }
    };
    let explicit = match std::env::var_os("OJ_PLUGINS_DIR") {
        Some(v) => Some(abs(Path::new(&v))),
        None => toml_plugins_dir.map(|p| abs(p)),
    };
    if let Some(base) = explicit {
        let dir = base.join(ffi::triple());
        return if dir.is_dir() {
            Ok(Some(dir))
        } else {
            Err(format!("plugins dir not found: {}", dir.display()))
        };
    }
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("plugins"));
        }
    }
    candidates.push(ffi::workspace_root().join("plugins"));
    for base in candidates {
        let dir = base.join(ffi::triple());
        if dir.is_dir() {
            return Ok(Some(dir));
        }
    }
    Ok(None)
}

/// 宿主 panic hook：装配首个插件前安装一次，输出当前插件上下文与构建指纹用于归因（spec §3）。
fn install_panic_hook() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let ctx = CURRENT_PLUGIN.lock().unwrap();
            if let Some(name) = ctx.as_ref() {
                eprintln!(
                    "[oj-plugin] panic while loading plugin '{name}' (host fingerprint: {HOST_FINGERPRINT})"
                );
            }
            prev(info);
        }));
    });
}

/// 当前正在 init 的插件名（panic 归因上下文）。
static CURRENT_PLUGIN: Mutex<Option<String>> = Mutex::new(None);

/// 宿主回调集（进程级单例）。
pub fn host_context() -> RArc<HostContext> {
    static CTX: Mutex<Option<RArc<HostContext>>> = Mutex::new(None);
    let mut g = CTX.lock().unwrap();
    g.get_or_insert_with(|| RArc::new(HostContext { log: host_log })).clone()
}

extern "C" fn host_log(level: u8, msg: RString) {
    let m = &msg[..];
    match level {
        0 => tracing::trace!(target: "oj-plugin", "{m}"),
        1 => tracing::debug!(target: "oj-plugin", "{m}"),
        3 => tracing::warn!(target: "oj-plugin", "{m}"),
        4 => tracing::error!(target: "oj-plugin", "{m}"),
        _ => tracing::info!(target: "oj-plugin", "{m}"),
    }
}

/// 加载单个库文件并完成全部门禁。expected: 清单模式 Some(entry)，扫描模式 None。
fn load_one(
    path: &Path,
    expected: Option<&PluginManifestEntry>,
    host: RArc<HostContext>,
    cfg_for: &dyn Fn(&str) -> String,
) -> Result<LoadedPlugin, PluginLoadError> {
    install_panic_hook();
    let lib = unsafe { ffi::load_forget(path)? };

    let abi_sym = unsafe { lib.get::<extern "C" fn() -> u32>(b"oj_plugin_abi_version") }
        .map_err(|_| PluginLoadError::SymbolMissing { path: path.to_path_buf(), symbol: "oj_plugin_abi_version" })?;
    let plugin_abi = abi_sym();
    if plugin_abi != ABI_VERSION {
        return Err(PluginLoadError::AbiMismatch { plugin: plugin_abi, host: ABI_VERSION });
    }

    let init_sym = unsafe {
        lib.get::<extern "C" fn(RArc<HostContext>, RString) -> oj_plugin_ffi::RResult<PluginDescriptor, RString>>(b"oj_plugin_init")
    }
    .map_err(|_| PluginLoadError::SymbolMissing { path: path.to_path_buf(), symbol: "oj_plugin_init" })?;

    // cfg 键 = 清单名或文件 stem（去 lib 前缀）。
    let probe = expected
        .map(|e| e.name.clone())
        .unwrap_or_else(|| file_stem_name(path));
    *CURRENT_PLUGIN.lock().unwrap() = Some(probe.clone());
    let r = init_sym(host, RString::from(cfg_for(&probe).as_str()));
    *CURRENT_PLUGIN.lock().unwrap() = None;

    let descriptor = match std::result::Result::from(r) {
        Ok(d) => d,
        Err(e) => return Err(PluginLoadError::InitFailed { name: probe, detail: e[..].to_string() }),
    };

    // 硬门禁第二道：descriptor 内报告的 abi 也必须等值（防御插件绕过宏手写符号）。
    if descriptor.abi_version != ABI_VERSION {
        return Err(PluginLoadError::AbiMismatch { plugin: descriptor.abi_version, host: ABI_VERSION });
    }

    // 指纹比对：不符仅告警不 fail（spec §3）。
    if &descriptor.fingerprint[..] != HOST_FINGERPRINT {
        eprintln!(
            "[oj-plugin] fingerprint mismatch: plugin '{}' built with [{}], host [{}]",
            &descriptor.name[..], &descriptor.fingerprint[..], HOST_FINGERPRINT
        );
    }

    if let Some(entry) = expected {
        if &descriptor.name[..] != entry.name {
            return Err(PluginLoadError::IdentityMismatch {
                expected: entry.name.clone(),
                actual: descriptor.name[..].to_string(),
            });
        }
        if let Some(pin) = &entry.semver_pin {
            if &descriptor.semver[..] != pin {
                return Err(PluginLoadError::IdentityMismatch {
                    expected: format!("{}@{pin}", entry.name),
                    actual: format!("{}@{}", entry.name, &descriptor.semver[..]),
                });
            }
        }
    }

    // init 窗口内取注册槽位（spec §3：descriptor 内注册回调指针）。
    let raw = (descriptor.register)();
    let registrations = Registrations { es: raw.es(), db: raw.db() };

    Ok(LoadedPlugin { descriptor, registrations })
}

fn file_stem_name(path: &Path) -> String {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    stem.strip_prefix("lib").unwrap_or(stem).to_string()
}

/// 按清单装配：文件缺失/任何校验失败 → fail fast（Err）。
pub fn load_manifest(
    dir: &Path,
    manifest: &[PluginManifestEntry],
    host: RArc<HostContext>,
    cfg_for: &dyn Fn(&str) -> String,
) -> Result<Vec<LoadedPlugin>, PluginLoadError> {
    let mut out = Vec::with_capacity(manifest.len());
    for entry in manifest {
        let path = dir.join(ffi::plugin_file_name(&entry.name));
        out.push(load_one(&path, Some(entry), host.clone(), cfg_for)?);
    }
    Ok(out)
}

/// 扫描装配（缺省模式）：加载 dir 下全部符合命名约定的库文件（文件名排序保确定性）；
/// 目录不存在/为空 → Ok(vec![])；扫描到但校验失败 → Err（不静默跳过，spec §5）。
pub fn load_scanned(dir: &Path, host: RArc<HostContext>) -> Result<Vec<LoadedPlugin>, PluginLoadError> {
    if !dir.is_dir() {
        return Ok(vec![]);
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| PluginLoadError::DependencyResolution {
            path: dir.to_path_buf(),
            loader_text: format!("read_dir: {e}"),
        })?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| ffi::is_plugin_file(p))
        .collect();
    files.sort();
    let mut out = Vec::with_capacity(files.len());
    for path in files {
        out.push(load_one(&path, None, host.clone(), &|_| "{}".to_string())?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
