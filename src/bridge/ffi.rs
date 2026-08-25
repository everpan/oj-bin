//! ffi.rs：全部 unsafe 收敛于此（「加载 + forget」单一函数，spec §决策表）。
//! unsafe 审计清单：
//! - Library 句柄加载成功立即 Box::leak，进程期存活，任何路径不 dlclose；
//! - 插件必须 panic=unwind profile（契约 crate 文档约束）；
//! - 符号签名必须与 oj-plugin-ffi 契约一致（ABI_VERSION 门禁兜底）。

use crate::bridge::plugin_loader::PluginLoadError;
use libloading::Library;
use std::path::{Path, PathBuf};

/// 唯一 dlopen 点。加载成功立即泄漏句柄（进程期存活）。
pub(crate) unsafe fn load_forget(path: &Path) -> Result<&'static Library, PluginLoadError> {
    if !path.is_file() {
        return Err(PluginLoadError::FileMissing { path: path.to_path_buf() });
    }
    #[cfg(windows)]
    let loaded = {
        use libloading::os::windows::{Library as WinLibrary, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32};
        unsafe { WinLibrary::load_with_flags(path, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32) }
            .map(Library::from)
    };
    #[cfg(not(windows))]
    let loaded = unsafe { Library::new(path) };

    match loaded {
        Ok(lib) => Ok(Box::leak(Box::new(lib))),
        Err(e) => Err(classify_load_error(path, e)),
    }
}

/// loader 原始错误文本 → 错误分类（透出原文，spec §4）。
fn classify_load_error(path: &Path, e: impl std::fmt::Display) -> PluginLoadError {
    let text = e.to_string();
    let lower = text.to_lowercase();
    // 平台/架构不匹配（含 glibc 基线不满足：glibc 报错文本含 "glibc"/"version `glibc_x.y' not found"）。
    if lower.contains("architecture")
        || lower.contains("incompatible")
        || lower.contains("mach-o")
        || lower.contains("elf class")
        || lower.contains("wrong elf")
        || lower.contains("glibc")
        || lower.contains("%1 is not a valid win32")
    {
        PluginLoadError::PlatformMismatch { path: path.to_path_buf(), detail: text }
    } else {
        PluginLoadError::DependencyResolution { path: path.to_path_buf(), loader_text: text }
    }
}

/// 插件文件命名约定：unix `lib<name>.<so|dylib>`，windows `<name>.dll`。
pub(crate) fn plugin_file_name(name: &str) -> String {
    if cfg!(target_os = "windows") {
        format!("{name}.dll")
    } else if cfg!(target_os = "macos") {
        format!("lib{name}.dylib")
    } else {
        format!("lib{name}.so")
    }
}

/// 扫描模式：文件名 → 是否库文件（按本台扩展名）。
pub(crate) fn is_plugin_file(path: &Path) -> bool {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    if cfg!(target_os = "windows") {
        name.ends_with(".dll")
    } else if cfg!(target_os = "macos") {
        name.starts_with("lib") && name.ends_with(".dylib")
    } else {
        name.starts_with("lib") && name.ends_with(".so")
    }
}

pub(crate) fn triple() -> &'static str {
    env!("OJ_TARGET_TRIPLE")
}

pub(crate) fn workspace_root() -> PathBuf {
    PathBuf::from(env!("OJ_WORKSPACE_ROOT"))
}
