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
        let dst = dir.join(ffi::plugin_file_name("mini"));
        // 幂等拷贝：dest 已存在且不旧于源则跳过。Windows 上已加载的 dll 不可覆写，
        // 并行测试/子进程场景（父进程持有 mini.dll 时 helper 子进程重跑本函数）会撞
        // sharing violation（ERROR_SHARING_VIOLATION，code 32）。
        let dst_modified = std::fs::metadata(&dst).and_then(|m| m.modified());
        let src_modified = std::fs::metadata(&built).and_then(|m| m.modified());
        let outdated = match (dst_modified, src_modified) {
            (Ok(d), Ok(s)) => d < s,
            _ => true,
        };
        if outdated {
            std::fs::copy(&built, &dst).expect("copy test plugin artifact");
        }
        dir
    })
    .clone()
}

/// mini-kv 编译产物目录（复用 mini_plugin_dir 的按需编译 + 幂等拷贝模式，
/// 包名/产物名替换为 oj-plugin-test-mini-kv / mini-kv）。与 mini 分目录存放：
/// 共享目录会让 scan_loads_mini 的「目录内恰一个插件」计数断言翻倍。
fn mini_kv_plugin_dir() -> PathBuf {
    static ONCE: OnceLock<PathBuf> = OnceLock::new();
    ONCE.get_or_init(|| {
        let root = ffi::workspace_root();
        let status = std::process::Command::new("cargo")
            .args(["build", "-p", "oj-plugin-test-mini-kv"])
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
            .join(format!("{prefix}oj_plugin_test_mini_kv.{ext}"));
        let dir = root.join("target/test-plugins-kv").join(ffi::triple());
        std::fs::create_dir_all(&dir).unwrap();
        let dst = dir.join(ffi::plugin_file_name("mini-kv"));
        // 幂等拷贝：dest 已存在且不旧于源则跳过（同 mini_plugin_dir，Windows dll 覆写坑）。
        let dst_modified = std::fs::metadata(&dst).and_then(|m| m.modified());
        let src_modified = std::fs::metadata(&built).and_then(|m| m.modified());
        let outdated = match (dst_modified, src_modified) {
            (Ok(d), Ok(s)) => d < s,
            _ => true,
        };
        if outdated {
            std::fs::copy(&built, &dst).expect("copy test plugin artifact");
        }
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
    // <exe>/plugins 与 <workspace_root>/bin/plugins 均不存在时为零插件。
    // 测试进程 exe 在 target/debug/deps，workspace root 的 bin/plugins 无构件（若日后有了需改此测试）。
    let got = resolve_plugins_dir(Path::new("/nonexistent-cfg"), None).unwrap();
    if ffi::workspace_root()
        .join("bin")
        .join("plugins")
        .join(ffi::triple())
        .is_dir()
    {
        return; // 环境已有默认目录则跳过
    }
    assert_eq!(got, None);
}

// ---- 清单模式 ----

#[test]
fn manifest_load_ok() {
    let _g = ENV_LOCK.lock().unwrap();
    let dir = mini_plugin_dir();
    let manifest = vec![PluginManifestEntry {
        name: "mini".into(),
        semver_pin: None,
    }];
    let loaded = load_manifest(&dir, &manifest, host_context(), &no_cfg).unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(&loaded[0].descriptor.name[..], "mini");
    assert_eq!(loaded[0].descriptor.abi_version, ABI_VERSION);
}

#[test]
fn manifest_file_missing() {
    let dir = mini_plugin_dir();
    let manifest = vec![PluginManifestEntry {
        name: "ghost".into(),
        semver_pin: None,
    }];
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
    let manifest = vec![PluginManifestEntry {
        name: "mini".into(),
        semver_pin: None,
    }];
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
    // 独立临时目录摆"冒名者"，不复用共享插件目录：残留文件会污染
    // scan_loads_mini 的计数断言（shared dir 会被所有测试共享）。
    let base = mini_plugin_dir();
    let tmp = tempfile::tempdir().unwrap();
    let impostor = tmp.path().join(ffi::plugin_file_name("impostor"));
    std::fs::copy(base.join(ffi::plugin_file_name("mini")), &impostor).unwrap();
    let manifest = vec![PluginManifestEntry {
        name: "impostor".into(),
        semver_pin: None,
    }];
    let err = load_manifest(tmp.path(), &manifest, host_context(), &no_cfg).unwrap_err();
    assert!(
        matches!(err, PluginLoadError::IdentityMismatch { .. }),
        "{err}"
    );
}

#[test]
fn manifest_semver_pin_mismatch() {
    let _g = ENV_LOCK.lock().unwrap();
    let dir = mini_plugin_dir();
    let manifest = vec![PluginManifestEntry {
        name: "mini".into(),
        semver_pin: Some("9.9.9".into()),
    }];
    let err = load_manifest(&dir, &manifest, host_context(), &no_cfg).unwrap_err();
    assert!(
        matches!(err, PluginLoadError::IdentityMismatch { .. }),
        "{err}"
    );
}

/// init 期 panic → 宿主分类为 InitFailed（入口宏 catch_unwind 收敛，宿主进程不终止），
/// 错误带插件名与 panic 归因（spec §3 panic 围堵）。
#[test]
fn init_panic_is_classified_error() {
    let _g = ENV_LOCK.lock().unwrap();
    let dir = mini_plugin_dir();
    unsafe { std::env::set_var("MINI_PANIC", "1") };
    let manifest = vec![PluginManifestEntry {
        name: "mini".into(),
        semver_pin: None,
    }];
    let err = load_manifest(&dir, &manifest, host_context(), &no_cfg).unwrap_err();
    unsafe { std::env::remove_var("MINI_PANIC") };
    match err {
        PluginLoadError::InitFailed { name, detail } => {
            assert_eq!(name, "mini");
            assert!(detail.contains("panic"), "{detail}");
        }
        other => panic!("expected InitFailed, got {other}"),
    }
}

/// 宿主 panic hook 归因（spec §3）：宿主在 `CURRENT_PLUGIN` 置位窗口内 panic（此处
/// 为 load_one 的 cfg_for 参数求值期）→ hook 输出 `[oj-plugin] panic while loading
/// plugin '<name>' (host fingerprint: …)` 后透传。init 期插件侧 panic 由入口宏
/// catch_unwind 收敛（见 init_panic_is_classified_error），hook 管的是宿主可见的 panic。
/// 以子进程核对 stderr 归因行（同进程 eprintln 会被测试捕获吞掉）。
#[test]
fn panic_hook_attribution_line_emitted() {
    let exe = std::env::current_exe().unwrap();
    let out = std::process::Command::new(exe)
        .arg("panic_hook_emit_helper")
        .arg("--nocapture") // 直跑测试二进制：不过 --nocapture 则 libtest 吞掉通过测试的 stderr
        // 父进程并行 env 测试可能已 set 这些变量，子进程不得继承（否则 mini 插件
        // 行为改变，cfg_for panic 路径走不到，归因行缺失）。
        .env_remove("MINI_FAKE_ABI")
        .env_remove("MINI_PANIC")
        .output()
        .expect("run helper subprocess");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("[oj-plugin] panic while loading plugin 'mini'"),
        "attribution line missing in subprocess stderr:\n{stderr}"
    );
    assert!(
        stderr.contains(oj_plugin_ffi::HOST_FINGERPRINT),
        "host fingerprint missing in attribution:\n{stderr}"
    );
}

/// helper：宿主 cfg_for 内 panic（CURRENT_PLUGIN=Some("mini") 窗口），经 hook 打印
/// 归因后传播；catch_unwind 兜住，进程不终止。
#[test]
fn panic_hook_emit_helper() {
    let _g = ENV_LOCK.lock().unwrap();
    let dir = mini_plugin_dir();
    let manifest = vec![PluginManifestEntry {
        name: "mini".into(),
        semver_pin: None,
    }];
    let r = std::panic::catch_unwind(|| {
        let _ = load_manifest(&dir, &manifest, host_context(), &|_| panic!("cfg boom"));
    });
    assert!(r.is_err(), "host panic must propagate (not swallowed)");
}

#[test]
fn manifest_semver_pin_ok() {
    let _g = ENV_LOCK.lock().unwrap();
    let dir = mini_plugin_dir();
    let manifest = vec![PluginManifestEntry {
        name: "mini".into(),
        semver_pin: Some("0.1.0".into()),
    }];
    assert!(load_manifest(&dir, &manifest, host_context(), &no_cfg).is_ok());
}

// ---- 扫描模式 ----

#[test]
fn scan_empty_dir_is_zero_plugins() {
    let base = tempfile::tempdir().unwrap();
    let loaded = load_scanned(base.path(), host_context(), &|_| "{}".to_string()).unwrap();
    assert!(loaded.is_empty());
}

#[test]
fn scan_missing_dir_is_zero_plugins() {
    let loaded = load_scanned(
        Path::new("/nonexistent-plugins-dir"),
        host_context(),
        &|_| "{}".to_string(),
    )
    .unwrap();
    assert!(loaded.is_empty());
}

#[test]
fn scan_loads_mini() {
    let _g = ENV_LOCK.lock().unwrap();
    let dir = mini_plugin_dir();
    let loaded = load_scanned(&dir, host_context(), &|_| "{}".to_string()).unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(&loaded[0].descriptor.name[..], "mini");
}

// ---- 按轴探测（dlsym）----

/// mini（零轴）：加载成功但所有轴 None；mini-kv（单轴）：kv 有、auth 无。
#[test]
fn probe_finds_declared_axis_and_misses_undeclared() {
    let _g = ENV_LOCK.lock().unwrap();
    unsafe { std::env::remove_var("MINI_FAKE_ABI") };
    unsafe { std::env::remove_var("MINI_PANIC") };
    let mini = super::load_one(
        &mini_plugin_dir().join(ffi::plugin_file_name("mini")),
        None,
        host_context(),
        &no_cfg,
    )
    .unwrap();
    assert!(mini.registrations.kv.is_none());
    let mkv = super::load_one(
        &mini_kv_plugin_dir().join(ffi::plugin_file_name("mini-kv")),
        None,
        host_context(),
        &no_cfg,
    )
    .unwrap();
    assert!(mkv.registrations.kv.is_some());
    assert!(mkv.registrations.auth.is_none());
}

#[test]
fn scan_bad_plugin_is_err_not_skipped() {
    let base = tempfile::tempdir().unwrap();
    let bad = base.path().join(ffi::plugin_file_name("bad"));
    std::fs::write(&bad, b"not a real shared library").unwrap();
    let err = load_scanned(base.path(), host_context(), &|_| "{}".to_string()).unwrap_err();
    // loader 拒绝（分类为 PlatformMismatch 或 DependencyResolution 均可，关键是不静默跳过）。
    assert!(
        matches!(
            err,
            PluginLoadError::PlatformMismatch { .. } | PluginLoadError::DependencyResolution { .. }
        ),
        "{err}"
    );
}
