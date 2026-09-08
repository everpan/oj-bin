//! plugin_loader 测试：四级路径解析 / 清单模式失败分类 / 扫描模式。
//! 依赖夹具插件 oj-plugin-test-mini（tests/plugins/mini，cdylib），首次测试时编译。

use super::*;
use std::sync::{Mutex, OnceLock};

/// env 相关测试串行化（同进程并行测试会互踩环境变量）。
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// 编译夹具插件（debug profile）并把产物拷入 target/<subdir>/<triple>/<short_name> 库文件，
/// 返回该目录。全进程每夹具只做一次；拷贝幂等（dest 不旧于 src 则跳过——Windows 上
/// 已加载的 dll 不可覆写，并行测试/子进程场景重拷会撞 sharing violation，code 32）。
fn fixture_plugin_dir(pkg: &str, artifact: &str, short_name: &str, subdir: &str) -> PathBuf {
    let root = ffi::workspace_root();
    let status = std::process::Command::new("cargo")
        .args(["build", "-p", pkg])
        .current_dir(&root)
        .status()
        .expect("invoke cargo build for test plugin");
    assert!(status.success(), "test plugin build failed: {pkg}");
    let (prefix, ext) = if cfg!(target_os = "windows") {
        ("", "dll")
    } else if cfg!(target_os = "macos") {
        ("lib", "dylib")
    } else {
        ("lib", "so")
    };
    let built = root
        .join("target/debug")
        .join(format!("{prefix}{artifact}.{ext}"));
    let dir = root.join("target").join(subdir).join(ffi::triple());
    std::fs::create_dir_all(&dir).unwrap();
    let dst = dir.join(ffi::plugin_file_name(short_name));
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
}

fn mini_plugin_dir() -> PathBuf {
    static ONCE: OnceLock<PathBuf> = OnceLock::new();
    ONCE.get_or_init(|| {
        fixture_plugin_dir(
            "oj-plugin-test-mini",
            "oj_plugin_test_mini",
            "mini",
            "test-plugins",
        )
    })
    .clone()
}

/// mini-kv（单轴 kv 夹具）。与 mini 分目录存放：共享目录会让 scan_loads_mini 的
/// 「目录内恰一个插件」计数断言翻倍。
fn mini_kv_plugin_dir() -> PathBuf {
    static ONCE: OnceLock<PathBuf> = OnceLock::new();
    ONCE.get_or_init(|| {
        fixture_plugin_dir(
            "oj-plugin-test-mini-kv",
            "oj_plugin_test_mini_kv",
            "mini-kv",
            "test-plugins-kv",
        )
    })
    .clone()
}

fn no_cfg(_: &str) -> String {
    "{}".to_string()
}

/// mini-mq（单轴 mq 夹具；与 mini/mini-kv 各占独立目录，避免 scan 计数断言翻倍）。
fn mini_mq_plugin_dir() -> PathBuf {
    static ONCE: OnceLock<PathBuf> = OnceLock::new();
    ONCE.get_or_init(|| {
        fixture_plugin_dir(
            "oj-plugin-test-mini-mq",
            "oj_plugin_test_mini_mq",
            "mini-mq",
            "test-plugins-mq",
        )
    })
    .clone()
}

/// mini-nosym（无 oj 符号的普通 cdylib）：dlopen 成功但缺 `oj_plugin_abi_version`。
fn mini_nosym_plugin_dir() -> PathBuf {
    static ONCE: OnceLock<PathBuf> = OnceLock::new();
    ONCE.get_or_init(|| {
        fixture_plugin_dir(
            "oj-plugin-test-mini-nosym",
            "oj_plugin_test_mini_nosym",
            "mini-nosym",
            "test-plugins-nosym",
        )
    })
    .clone()
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

/// mini（零轴）：mq 槽 None；mini-mq（单轴 mq）：mq 槽 Some——加轴零破坏回归（ABI 7 不变）。
#[test]
fn probe_finds_mq_axis_and_zero_axis_mini_misses_it() {
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
    assert!(mini.registrations.mq.is_none());
    let mmq = super::load_one(
        &mini_mq_plugin_dir().join(ffi::plugin_file_name("mini-mq")),
        None,
        host_context(),
        &no_cfg,
    )
    .unwrap();
    assert!(mmq.registrations.mq.is_some());
    assert_eq!(mmq.descriptor.abi_version, oj_plugin_ffi::ABI_VERSION);
}

/// mini-mq call echo 契约冒烟：method + payload 原样回显（JSON in → JSON out）。
#[tokio::test]
async fn given_mini_mq_when_call_echo_then_method_and_payload_roundtrip() {
    // ENV_LOCK 只护同步的 load_one（读 env 在装载期）；guard 须在 await 前释放
    // （clippy await_holding_lock）。
    let vt = {
        let _g = ENV_LOCK.lock().unwrap();
        unsafe { std::env::remove_var("MINI_FAKE_ABI") };
        unsafe { std::env::remove_var("MINI_PANIC") };
        let mmq = super::load_one(
            &mini_mq_plugin_dir().join(ffi::plugin_file_name("mini-mq")),
            None,
            host_context(),
            &no_cfg,
        )
        .unwrap();
        mmq.registrations.mq.unwrap()
    };
    let connected = crate::bridge::ffi::await_ffi((vt.connect)(RString::from("{}")))
        .await
        .unwrap();
    assert_eq!(String::from_utf8(connected).unwrap(), r#"{"handle":1}"#);
    let out = crate::bridge::ffi::await_ffi((vt.call)(
        1,
        RString::from("echo"),
        RString::from(r#"{"a":1}"#),
    ))
    .await
    .unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        r#"{"method":"echo","payload":{"a":1}}"#
    );
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

// ---- 自省 / 其余失败分类补测 ----

/// 清掉 mini 行为开关（并行测试可能已 set，防互踩）。
fn clear_mini_hooks() {
    unsafe { std::env::remove_var("MINI_FAKE_ABI") };
    unsafe { std::env::remove_var("MINI_PANIC") };
    unsafe { std::env::remove_var("MINI_FAKE_FINGERPRINT") };
}

/// Debug 自省形态（运维诊断用）：须点名插件名 / semver / abi_version。
#[test]
fn given_loaded_plugin_when_debug_format_then_names_plugin_semver_and_abi() {
    let _g = ENV_LOCK.lock().unwrap();
    clear_mini_hooks();
    let loaded = super::load_one(
        &mini_plugin_dir().join(ffi::plugin_file_name("mini")),
        None,
        host_context(),
        &no_cfg,
    )
    .unwrap();
    let dbg = format!("{loaded:?}");
    assert!(dbg.contains("LoadedPlugin"), "{dbg}");
    assert!(dbg.contains("mini"), "{dbg}");
    assert!(dbg.contains("0.1.0"), "{dbg}");
    assert!(dbg.contains("abi_version"), "{dbg}");
}

/// dlopen 成功但缺 ABI 符号（无 oj 符号的普通 cdylib）→ SymbolMissing 且点名符号；
/// 这是不走入口宏的手写库的第一道门禁（spec §4 失败分类之三）。
#[test]
fn given_library_without_abi_symbol_when_load_then_symbol_missing() {
    let dir = mini_nosym_plugin_dir();
    let err = super::load_one(
        &dir.join(ffi::plugin_file_name("mini-nosym")),
        None,
        host_context(),
        &no_cfg,
    )
    .unwrap_err();
    match err {
        PluginLoadError::SymbolMissing { symbol, .. } => {
            assert_eq!(symbol, "oj_plugin_abi_version");
        }
        other => panic!("expected SymbolMissing, got {other}"),
    }
}

/// 指纹不符仅告警不 fail（spec §3）：夹具经 MINI_FAKE_FINGERPRINT 伪造指纹，
/// 加载须成功且 descriptor 原样保留（宿主自报的指纹进 PluginInfo 供运维核对）。
#[test]
fn given_fingerprint_mismatch_when_load_then_warn_only_and_still_ok() {
    let _g = ENV_LOCK.lock().unwrap();
    clear_mini_hooks();
    let dir = mini_plugin_dir();
    unsafe { std::env::set_var("MINI_FAKE_FINGERPRINT", "rustc-999-bogus") };
    let loaded = super::load_one(
        &dir.join(ffi::plugin_file_name("mini")),
        None,
        host_context(),
        &no_cfg,
    );
    unsafe { std::env::remove_var("MINI_FAKE_FINGERPRINT") };
    let loaded = loaded.unwrap();
    assert_eq!(&loaded.descriptor.name[..], "mini");
    assert_eq!(&loaded.descriptor.fingerprint[..], "rustc-999-bogus");
}

// ---- 装配期 connect 适配（kv_backend_connect / blob_backend_connect）----

use oj_plugin_ffi::{BlobBackendVtable, FfiFuture, KVStoreVtable};

/// 断言 connect 为 Err 并取错误文案（Ok 型是非 Debug 的 Arc<dyn KVStore/BlobBackend>）。
fn connect_err<T: ?Sized>(r: Result<Arc<T>, String>) -> String {
    match r {
        Ok(_) => panic!("expected connect failure"),
        Err(e) => e,
    }
}

/// kv 连接假 vtable：0=ok {"handle":9}；1=插件报错；2=非 JSON；3=缺 handle。
static PL_KV_MODE: Mutex<u8> = Mutex::new(0);
static PL_KV_GOT_CFG: Mutex<String> = Mutex::new(String::new());

extern "C" fn pl_kv_connect(cfg: RString) -> FfiFuture {
    *PL_KV_GOT_CFG.lock().unwrap() = cfg[..].to_string();
    match *PL_KV_MODE.lock().unwrap() {
        1 => oj_plugin_ffi::ready_err("kv down"),
        2 => oj_plugin_ffi::ready_ok(b"gibberish".to_vec()),
        3 => oj_plugin_ffi::ready_ok(br#"{}"#.to_vec()),
        _ => oj_plugin_ffi::ready_ok(br#"{"handle":9}"#.to_vec()),
    }
}
extern "C" fn pl_kv_stub_get(_h: u64, _k: RString) -> FfiFuture {
    oj_plugin_ffi::ready_err("stub")
}
extern "C" fn pl_kv_stub_set(_h: u64, _k: RString, _v: RString) -> FfiFuture {
    oj_plugin_ffi::ready_err("stub")
}
extern "C" fn pl_kv_stub_expire(_h: u64, _k: RString, _t: u64) -> FfiFuture {
    oj_plugin_ffi::ready_err("stub")
}
extern "C" fn pl_kv_close(_h: u64) {}

static PL_KV_VT: KVStoreVtable = KVStoreVtable {
    connect: pl_kv_connect,
    get: pl_kv_stub_get,
    set: pl_kv_stub_set,
    del: pl_kv_stub_get,
    expire: pl_kv_stub_expire,
    incr: pl_kv_stub_get,
    close: pl_kv_close,
};

/// kv 装配期 connect 成功 + 三类失败（插件报错 / 非 JSON / 缺 handle）——
/// 合为一个用例串行驱动：模式开关是进程级 static，并行用例会在 await 点互踩。
#[tokio::test]
async fn given_kv_vtable_when_backend_connect_then_cfg_forwarded_and_errs_name_the_stage() {
    // 成功路径：url 以 `{"url":...}` JSON 过线，handle 提取成 FfiKVStore。
    *PL_KV_MODE.lock().unwrap() = 0;
    let store = kv_backend_connect(&PL_KV_VT, "redis://h:6379")
        .await
        .unwrap();
    assert_eq!(
        PL_KV_GOT_CFG.lock().unwrap().as_str(),
        r#"{"url":"redis://h:6379"}"#
    );
    // store 是 core KVStore 形态（handle 已包进适配器；方法错误臂由假 vtable 兜底）。
    assert!(store.get("k").await.is_err());
    // 失败臂：各自错误文案点名阶段（spec §3 错误透传契约）。
    *PL_KV_MODE.lock().unwrap() = 1;
    let e = connect_err(kv_backend_connect(&PL_KV_VT, "redis://h").await);
    assert!(e.contains("kv connect") && e.contains("kv down"), "{e}");
    *PL_KV_MODE.lock().unwrap() = 2;
    let e = connect_err(kv_backend_connect(&PL_KV_VT, "redis://h").await);
    assert!(e.contains("kv connect decode"), "{e}");
    *PL_KV_MODE.lock().unwrap() = 3;
    let e = connect_err(kv_backend_connect(&PL_KV_VT, "redis://h").await);
    assert!(e.contains("kv connect: missing handle"), "{e}");
    *PL_KV_MODE.lock().unwrap() = 0;
}

/// blob 连接假 vtable：0=ok {"handle":8}；1=插件报错；2=非 JSON；3=缺 handle。
static PL_BLOB_MODE: Mutex<u8> = Mutex::new(0);
static PL_BLOB_GOT: Mutex<(String, String)> = Mutex::new((String::new(), String::new()));

extern "C" fn pl_blob_connect(name: RString, cfg: RString) -> FfiFuture {
    *PL_BLOB_GOT.lock().unwrap() = (name[..].to_string(), cfg[..].to_string());
    match *PL_BLOB_MODE.lock().unwrap() {
        1 => oj_plugin_ffi::ready_err("s3 down"),
        2 => oj_plugin_ffi::ready_ok(b"gibberish".to_vec()),
        3 => oj_plugin_ffi::ready_ok(br#"{}"#.to_vec()),
        _ => oj_plugin_ffi::ready_ok(br#"{"handle":8}"#.to_vec()),
    }
}
extern "C" fn pl_blob_stub(_h: u64, _k: RString) -> FfiFuture {
    oj_plugin_ffi::ready_err("stub")
}
extern "C" fn pl_blob_put(
    _h: u64,
    _k: RString,
    _b: oj_plugin_ffi::RBytes,
    _ct: RString,
) -> FfiFuture {
    oj_plugin_ffi::ready_err("stub")
}
extern "C" fn pl_blob_close(_h: u64) {}

static PL_BLOB_VT: BlobBackendVtable = BlobBackendVtable {
    connect: pl_blob_connect,
    put: pl_blob_put,
    get: pl_blob_stub,
    del: pl_blob_stub,
    url: pl_blob_stub,
    content_type: pl_blob_stub,
    close: pl_blob_close,
};

/// blob 装配期 connect 成功 + 三类失败（同 kv：合一个用例避免并行互踩模式开关）。
/// 成功路径断言：后端名 + cfg JSON 按值原样过线（spec §3 有意的边界）。
#[tokio::test]
async fn given_blob_vtable_when_backend_connect_then_forwarded_and_errs_name_the_stage() {
    *PL_BLOB_MODE.lock().unwrap() = 0;
    blob_backend_connect(&PL_BLOB_VT, "s3", r#"{"bucket":"b"}"#)
        .await
        .unwrap();
    let (name, cfg) = PL_BLOB_GOT.lock().unwrap().clone();
    assert_eq!((name.as_str(), cfg.as_str()), ("s3", r#"{"bucket":"b"}"#));
    *PL_BLOB_MODE.lock().unwrap() = 1;
    let e = connect_err(blob_backend_connect(&PL_BLOB_VT, "s3", "{}").await);
    assert!(e.contains("blob connect") && e.contains("s3 down"), "{e}");
    *PL_BLOB_MODE.lock().unwrap() = 2;
    let e = connect_err(blob_backend_connect(&PL_BLOB_VT, "s3", "{}").await);
    assert!(e.contains("blob connect decode"), "{e}");
    *PL_BLOB_MODE.lock().unwrap() = 3;
    let e = connect_err(blob_backend_connect(&PL_BLOB_VT, "s3", "{}").await);
    assert!(e.contains("blob connect: missing handle"), "{e}");
    *PL_BLOB_MODE.lock().unwrap() = 0;
}
