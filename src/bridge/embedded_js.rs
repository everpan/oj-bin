//! 扩展 JS 源的内嵌与运行期补丁。
//!
//! 见 `build.rs` 顶部注释：deno_* 依赖以构建机绝对路径声明扩展 JS（无 startup
//! snapshot 时运行期读盘）。这里用编译期内嵌的同一份源码覆写为 `Computed` 源，
//! 使二进制自带全部扩展 JS、可在任意机器运行。

use std::borrow::Cow;
use std::sync::Arc;

use deno_core::{Extension, ExtensionFileSource, ExtensionFileSourceCode};

include!(concat!(env!("OUT_DIR"), "/embedded_ext_js.rs"));

/// 按 specifier 精确查找内嵌源码。
fn embedded_source(specifier: &str) -> Option<&'static str> {
    EMBEDDED_EXT_JS
        .iter()
        .find(|(spec, _)| *spec == specifier)
        .map(|(_, source)| *source)
}

/// 覆写一个扩展的某个源列表：凡「不可在运行期加载」（即来自文件系统绝对路径）的项，
/// 改为编译期内嵌源码（`Computed`）。
fn patch_sources(files: &mut Cow<'static, [ExtensionFileSource]>, ext_name: &str) {
    for file in files.to_mut() {
        if file.is_runtime_loadable() {
            continue;
        }
        let specifier = file.specifier;
        let source = embedded_source(specifier).unwrap_or_else(|| {
            panic!(
                "扩展 `{ext_name}` 的源 `{specifier}` 未内嵌：build.rs 未收录该文件\
                 （deno 依赖升级后需同步），或该源位于 crate 子目录（本表仅收录 crate 根级 .js）"
            )
        });
        file.code = ExtensionFileSourceCode::Computed(Arc::from(source));
    }
}

/// 把 `extensions` 里所有依赖构建机路径的扩展源替换为内嵌源码。
///
/// 必须在 `JsRuntime::new` **之前**调用（扩展建好后、进 runtime 前）。
pub fn patch_fs_loaded_sources(extensions: &mut [Extension]) {
    for extension in extensions.iter_mut() {
        let name = extension.name;
        let Extension {
            js_files,
            esm_files,
            lazy_loaded_esm_files,
            lazy_loaded_js_files,
            ..
        } = extension;
        patch_sources(js_files, name);
        patch_sources(esm_files, name);
        patch_sources(lazy_loaded_esm_files, name);
        patch_sources(lazy_loaded_js_files, name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sources(ext: &Extension) -> impl Iterator<Item = &ExtensionFileSource> {
        ext.js_files
            .iter()
            .chain(ext.esm_files.iter())
            .chain(ext.lazy_loaded_esm_files.iter())
            .chain(ext.lazy_loaded_js_files.iter())
    }

    /// 回归护栏：`ws_client_extensions()` 打补丁后不得再有任何依赖构建机路径的源。
    /// 这条用例在任意机器上都成立（不依赖源文件是否存在），可拦住本缺陷复发。
    #[test]
    fn given_deno_client_extensions_when_patched_then_no_source_needs_fs() {
        let mut extensions = crate::bridge::ws_client_extensions();
        assert!(
            extensions
                .iter()
                .flat_map(sources)
                .any(|f| !f.is_runtime_loadable()),
            "前提失效：deno 扩展已全部内嵌，本护栏失去意义（可考虑删除补丁路径）"
        );
        patch_fs_loaded_sources(&mut extensions);
        for ext in &extensions {
            for file in sources(ext) {
                assert!(
                    file.is_runtime_loadable(),
                    "扩展 `{}` 仍依赖构建机路径：{}",
                    ext.name,
                    file.specifier
                );
            }
        }
    }

    /// 内嵌表本身可用：specifier 唯一且源码非空。
    #[test]
    fn given_embedded_table_then_specifiers_unique_and_sources_nonempty() {
        let mut seen = std::collections::HashSet::new();
        for (specifier, source) in EMBEDDED_EXT_JS {
            assert!(seen.insert(*specifier), "specifier 重复：{specifier}");
            assert!(!source.is_empty(), "内嵌源码为空：{specifier}");
        }
        assert!(!EMBEDDED_EXT_JS.is_empty());
    }
}
