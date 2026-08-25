//! 测试夹具插件：固定 descriptor；`MINI_FAKE_ABI` 运行时环境变量可伪造 abi_version
//! （供宿主侧 AbiMismatch 门禁测试，无需重编译）。

use oj_plugin_ffi::{
    ABI_VERSION, HostContext, PluginDescriptor, PluginRegistrations, RArc, RResult, RString,
    oj_plugin_entry,
};

extern "C" fn no_registrations() -> PluginRegistrations {
    PluginRegistrations::none()
}

fn init(_host: RArc<HostContext>, _cfg: RString) -> RResult<PluginDescriptor, RString> {
    let abi = std::env::var("MINI_FAKE_ABI")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(ABI_VERSION);
    RResult::Ok(PluginDescriptor {
        name: RString::from("mini"),
        semver: RString::from("0.1.0"),
        abi_version: abi,
        fingerprint: RString::from(oj_plugin_ffi::HOST_FINGERPRINT),
        register: no_registrations,
    })
}

oj_plugin_entry!(init);
