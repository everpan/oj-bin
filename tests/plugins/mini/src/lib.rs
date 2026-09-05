//! 测试夹具插件：固定 descriptor；`MINI_FAKE_ABI` 运行时环境变量可伪造 abi_version
//! （供宿主侧 AbiMismatch 门禁测试，无需重编译）；`MINI_PANIC` 使 init 期 panic
//! （供宿主侧 init panic 围堵测试——入口宏 catch_unwind 收敛为 RResult::Err）。

use oj_plugin_ffi::{
    ABI_VERSION, HostContext, PluginDescriptor, RArc, RResult, RString, oj_plugin_entry,
};

fn init(_host: RArc<HostContext>, _cfg: RString) -> RResult<PluginDescriptor, RString> {
    if std::env::var("MINI_PANIC").is_ok() {
        panic!("mini init boom");
    }
    let abi = std::env::var("MINI_FAKE_ABI")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(ABI_VERSION);
    RResult::Ok(PluginDescriptor {
        name: RString::from("mini"),
        semver: RString::from("0.1.0"),
        abi_version: abi,
        fingerprint: RString::from(oj_plugin_ffi::HOST_FINGERPRINT),
        desc: RString::from("loader 测试夹具（零轴）"),
    })
}

oj_plugin_entry!(init);
