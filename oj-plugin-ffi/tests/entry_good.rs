//! 入口宏测试（好插件）：两符号存在、abi_version 匹配、init roundtrip。
//! 独立集成测试文件 = 独立 crate，#[no_mangle] 符号不冲突（每个插件 crate 只展开一次宏）。

use oj_plugin_ffi::{
    ABI_VERSION, HostContext, PluginDescriptor, PluginRegistrations, RArc, RResult, RString,
    oj_plugin_entry,
};

extern "C" fn no_registrations() -> PluginRegistrations {
    PluginRegistrations::none()
}

extern "C" fn noop_log(_level: u8, _msg: RString) {}

fn init(_host: RArc<HostContext>, cfg: RString) -> RResult<PluginDescriptor, RString> {
    assert_eq!(&cfg[..], "{}");
    RResult::Ok(PluginDescriptor {
        name: RString::from("test-plugin"),
        semver: RString::from("0.1.0"),
        abi_version: ABI_VERSION,
        fingerprint: RString::from("test"),
        register: no_registrations,
    })
}

oj_plugin_entry!(init);

#[test]
fn entry_macro_generates_two_symbols_and_abi_matches() {
    assert_eq!(oj_plugin_abi_version(), ABI_VERSION);
}

#[test]
fn init_roundtrip_ok() {
    let host = RArc::new(HostContext { log: noop_log });
    let r = oj_plugin_init(host, RString::from("{}"));
    match std::result::Result::from(r) {
        Ok(d) => {
            assert_eq!(&d.name[..], "test-plugin");
            assert_eq!(d.abi_version, ABI_VERSION);
            assert!((d.register)().es().is_none());
        }
        Err(e) => panic!("init failed: {e}"),
    }
}
