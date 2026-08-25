//! spike host：libloading 加载两个插件，验证 roundtrip / ABI 门禁 / panic 收敛。
//! 断言失败即非零退出（样例即测试）。

use libloading::Library;
use stabby::result::Result as SRResult;
use stabby::string::String as SRString;

fn plugin_path(name: &str) -> std::path::PathBuf {
    let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // workspace root
    let (prefix, ext) = if cfg!(target_os = "windows") {
        ("", "dll")
    } else if cfg!(target_os = "macos") {
        ("lib", "dylib")
    } else {
        ("lib", "so")
    };
    p.join("target/debug").join(format!("{prefix}{name}.{ext}"))
}

fn main() {
    // ---- stabby ----
    let lib = unsafe { Library::new(plugin_path("plugin_stabby")) }.expect("load stabby plugin");
    // 加载即 forget：进程期存活，任何路径不 dlclose（spec §决策表）。
    let lib: &'static Library = Box::leak(Box::new(lib));

    let abi: u32 = unsafe { lib.get::<extern "C" fn() -> u32>(b"oj_plugin_abi_version").unwrap()() };
    assert_eq!(abi, 1, "ABI gate must pass for same-version plugin");

    let echo = unsafe { lib.get::<extern "C" fn(SRString) -> SRResult<SRString, SRString>>(b"echo").unwrap() };
    // stabby 的 Result 是 opaque storage，匹配先转 std Result。
    match std::result::Result::from(echo(SRString::from("hello"))) {
        Ok(s) => assert_eq!(&s[..], "echo:hello"),
        Err(e) => panic!("echo failed: {e}"),
    }

    // panic 收敛：boom 返回 Err 而非 abort。
    let boom = unsafe { lib.get::<extern "C" fn() -> SRResult<SRString, SRString>>(b"boom").unwrap() };
    match std::result::Result::from(boom()) {
        Err(e) => assert!(e[..].contains("panic"), "{e}"),
        Ok(_) => panic!("boom must be converged to Err"),
    }

    // descriptor roundtrip（repr(C) struct by value 跨界）。
    type DescFn = extern "C" fn() -> plugin_descriptor::PluginDescriptor;
    let desc = unsafe { lib.get::<DescFn>(b"descriptor").unwrap()() };
    assert_eq!(&desc.name[..], "spike-stabby");
    assert_eq!(desc.abi_version, 1);
    println!("stabby: roundtrip + ABI gate + panic convergence + descriptor OK");

    // ABI 不等即拒绝（宿主侧门禁逻辑：等值才放行）。
    let host_abi = 1u32;
    assert!(abi == host_abi, "ABI mismatch must be rejected");

    // ---- abi_stable ----
    let lib2 = unsafe { Library::new(plugin_path("plugin_abistable")) }.expect("load abi_stable plugin");
    let lib2: &'static Library = Box::leak(Box::new(lib2));
    type AEcho = extern "C" fn(abi_stable::std_types::RString)
        -> abi_stable::std_types::RResult<abi_stable::std_types::RString, abi_stable::std_types::RString>;
    let echo2 = unsafe { lib2.get::<AEcho>(b"echo").unwrap() };
    match echo2(abi_stable::std_types::RString::from("hello")) {
        abi_stable::std_types::RResult::ROk(s) => assert_eq!(s.as_str(), "echo:hello"),
        abi_stable::std_types::RResult::RErr(e) => panic!("echo2 failed: {e}"),
    }
    println!("abi_stable: roundtrip OK");

    println!("ALL SPIKE S.1 CHECKS PASSED");
}

/// host 侧共享的 descriptor 布局镜像（真实方案中 = 契约 crate 同一类型，两侧同一依赖）。
mod plugin_descriptor {
    use stabby::string::String as SRString;
    #[stabby::stabby]
    #[repr(C)]
    pub struct PluginDescriptor {
        pub name: SRString,
        pub semver: SRString,
        pub abi_version: u32,
        pub fingerprint: SRString,
    }
}
