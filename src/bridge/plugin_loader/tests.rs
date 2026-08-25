//! plugin_loader 测试：四级路径解析 / 清单模式失败分类 / 扫描模式。
//! 依赖夹具插件 oj-plugin-test-mini（tests/plugins/mini，cdylib），首次测试时编译。

use super::*;
use std::sync::{Mutex, OnceLock};

/// env 相关测试串行化（同进程并行测试会互踩环境变量）。
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// 编译夹具插件并拷贝到 target/test-plugins/<triple>/libmini.<ext>（全进程一次）。
fn mini_plugin_dir() -> PathBuf {
    static ONCE: OnceLock<PathBuf> = OnceLock::new();
    ONCE.get_or_init(|| {
        let root = ffi::workspace_root();
        let status = std::process::Command::new("cargo")
            .args(["build", "-p", "oj-plugin-test-mini"])
            .current_dir(&root)
            .status()
            .expect("invoke cargo build for test plugin");
        assert!(status.success(), "test plugin build failed");
        let (prefix, ext) = if cfg!(target_os = "windows") {
            ("", "dll")
        } else if cfg!(target_os = "macos") {
            ("lib", "dylib")
        } else {
            ("lib", "so")
        };
        let built = root
            .join("target/debug")
            .join(format!("{prefix}oj_plugin_test_mini.{ext}"));
        let dir = root.join("target/test-plugins").join(ffi::triple());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(&built, dir.join(ffi::plugin_file_name("mini")))
            .expect("copy test plugin artifact");
        dir
    })
    .clone()
}

fn no_cfg(_: &str) -> String {
    "{}".to_string()
}

// ---- 路径解析 ----

#[test]
fn resolve_env_overrides_toml() {
    let _g = ENV_LOCK.lock().unwrap();
    let base = tempfile::tempdir().unwrap();
    let env_dir = base.path().join("env-plugins").join(ffi::triple());
    let toml_dir = base.path().join("toml-plugins").join(ffi::triple());
    std::fs::create_dir_all(&env_dir).unwrap();
    std::fs::create_dir_all(&toml_dir).unwrap();
    unsafe { std::env::set_var("OJ_PLUGINS_DIR", base.path().join("env-plugins")) };
    let got = resolve_plugins_dir(base.path(), Some(Path::new("toml-plugins"))).unwrap();
    assert_eq!(got, Some(env_dir));
    unsafe { std::env::remove_var("OJ_PLUGINS_DIR") };
}

#[test]
fn resolve_toml_relative_to_config_dir() {
    let _g = ENV_LOCK.lock().unwrap();
    unsafe { std::env::remove_var("OJ_PLUGINS_DIR") };
    let base = tempfile::tempdir().unwrap();
    let dir = base.path().join("my-plugins").join(ffi::triple());
    std::fs::create_dir_all(&dir).unwrap();
    let got = resolve_plugins_dir(base.path(), Some(Path::new("my-plugins"))).unwrap();
    assert_eq!(got, Some(dir));
}

#[test]
fn resolve_explicit_missing_is_err() {
    let _g = ENV_LOCK.lock().unwrap();
    unsafe { std::env::remove_var("OJ_PLUGINS_DIR") };
    let base = tempfile::tempdir().unwrap();
    let err = resolve_plugins_dir(base.path(), Some(Path::new("nope"))).unwrap_err();
    assert!(err.contains("plugins dir not found"), "{err}");
}

#[test]
fn resolve_default_missing_is_none() {
    let _g = ENV_LOCK.lock().unwrap();
    unsafe { std::env::remove_var("OJ_PLUGINS_DIR") };
    // <exe>/plugins 与 workspace/plugins 均不存在时为零插件。
    // 测试进程 exe 在 target/debug/deps，workspace root 无 plugins 目录（若日后有了需改此测试）。
    let got = resolve_plugins_dir(Path::new("/nonexistent-cfg"), None).unwrap();
    if ffi::workspace_root().join("plugins").join(ffi::triple()).is_dir() {
        return; // 环境已有默认目录则跳过
    }
    assert_eq!(got, None);
}

// ---- 清单模式 ----

#[test]
fn manifest_load_ok() {
    let _g = ENV_LOCK.lock().unwrap();
    let dir = mini_plugin_dir();
    let manifest = vec![PluginManifestEntry { name: "mini".into(), semver_pin: None }];
    let loaded = load_manifest(&dir, &manifest, host_context(), &no_cfg).unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(&loaded[0].descriptor.name[..], "mini");
    assert_eq!(loaded[0].descriptor.abi_version, ABI_VERSION);
}

#[test]
fn manifest_file_missing() {
    let dir = mini_plugin_dir();
    let manifest = vec![PluginManifestEntry { name: "ghost".into(), semver_pin: None }];
    let err = load_manifest(&dir, &manifest, host_context(), &no_cfg).unwrap_err();
    assert!(matches!(err, PluginLoadError::FileMissing { .. }), "{err}");
}

#[test]
fn manifest_abi_mismatch() {
    let _g = ENV_LOCK.lock().unwrap();
    let dir = mini_plugin_dir();
    // 夹具读宿主进程 env 伪造 descriptor.abi_version（oj_plugin_abi_version 符号仍为真值，
    // 走第二道 descriptor 门禁）。
    unsafe { std::env::set_var("MINI_FAKE_ABI", "999") };
    let manifest = vec![PluginManifestEntry { name: "mini".into(), semver_pin: None }];
    let err = load_manifest(&dir, &manifest, host_context(), &no_cfg).unwrap_err();
    unsafe { std::env::remove_var("MINI_FAKE_ABI") };
    match err {
        PluginLoadError::AbiMismatch { plugin, host } => {
            assert_eq!(plugin, 999);
            assert_eq!(host, ABI_VERSION);
        }
        other => panic!("expected AbiMismatch, got {other}"),
    }
}

#[test]
fn manifest_identity_mismatch() {
    let _g = ENV_LOCK.lock().unwrap();
    // 文件名按"冒名者"复制一份，descriptor.name 仍是 mini → IdentityMismatch。
    let dir = mini_plugin_dir();
    let impostor = dir.join(ffi::plugin_file_name("impostor"));
    std::fs::copy(dir.join(ffi::plugin_file_name("mini")), &impostor).unwrap();
    let manifest = vec![PluginManifestEntry { name: "impostor".into(), semver_pin: None }];
    let err = load_manifest(&dir, &manifest, host_context(), &no_cfg).unwrap_err();
    std::fs::remove_file(&impostor).ok();
    assert!(matches!(err, PluginLoadError::IdentityMismatch { .. }), "{err}");
}

#[test]
fn manifest_semver_pin_mismatch() {
    let _g = ENV_LOCK.lock().unwrap();
    let dir = mini_plugin_dir();
    let manifest =
        vec![PluginManifestEntry { name: "mini".into(), semver_pin: Some("9.9.9".into()) }];
    let err = load_manifest(&dir, &manifest, host_context(), &no_cfg).unwrap_err();
    assert!(matches!(err, PluginLoadError::IdentityMismatch { .. }), "{err}");
}

#[test]
fn manifest_semver_pin_ok() {
    let _g = ENV_LOCK.lock().unwrap();
    let dir = mini_plugin_dir();
    let manifest =
        vec![PluginManifestEntry { name: "mini".into(), semver_pin: Some("0.1.0".into()) }];
    assert!(load_manifest(&dir, &manifest, host_context(), &no_cfg).is_ok());
}

// ---- 扫描模式 ----

#[test]
fn scan_empty_dir_is_zero_plugins() {
    let base = tempfile::tempdir().unwrap();
    let loaded = load_scanned(base.path(), host_context()).unwrap();
    assert!(loaded.is_empty());
}

#[test]
fn scan_missing_dir_is_zero_plugins() {
    let loaded = load_scanned(Path::new("/nonexistent-plugins-dir"), host_context()).unwrap();
    assert!(loaded.is_empty());
}

#[test]
fn scan_loads_mini() {
    let _g = ENV_LOCK.lock().unwrap();
    let dir = mini_plugin_dir();
    let loaded = load_scanned(&dir, host_context()).unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(&loaded[0].descriptor.name[..], "mini");
}

#[test]
fn scan_bad_plugin_is_err_not_skipped() {
    let base = tempfile::tempdir().unwrap();
    let bad = base.path().join(ffi::plugin_file_name("bad"));
    std::fs::write(&bad, b"not a real shared library").unwrap();
    let err = load_scanned(base.path(), host_context()).unwrap_err();
    // loader 拒绝（分类为 PlatformMismatch 或 DependencyResolution 均可，关键是不静默跳过）。
    assert!(
        matches!(
            err,
            PluginLoadError::PlatformMismatch { .. } | PluginLoadError::DependencyResolution { .. }
        ),
        "{err}"
    );
}
