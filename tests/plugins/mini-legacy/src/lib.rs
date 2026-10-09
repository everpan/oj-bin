//! 旧式插件夹具：手写入口符号（不经 oj_plugin_entry! 宏），有 oj_plugin_axis_kv
//! 但**无** oj_plugin_axes 自报清单——供宿主侧「旧插件回退逐轴 dlsym」路径测试。
//! kv vtable 与 mini-kv 同形（方法一概 ready_err，假实现）。

use oj_plugin_ffi::{HostContext, KVStoreVtable, RArc, RResult, RString};

extern "C" fn connect(_cfg: RString) -> oj_plugin_ffi::FfiFuture {
    oj_plugin_ffi::ready_err("mini-legacy: not a real kv")
}
extern "C" fn get(_handle: u64, _key: RString) -> oj_plugin_ffi::FfiFuture {
    oj_plugin_ffi::ready_err("mini-legacy: not a real kv")
}
extern "C" fn set(_handle: u64, _key: RString, _value: RString) -> oj_plugin_ffi::FfiFuture {
    oj_plugin_ffi::ready_err("mini-legacy: not a real kv")
}
extern "C" fn del(_handle: u64, _key: RString) -> oj_plugin_ffi::FfiFuture {
    oj_plugin_ffi::ready_err("mini-legacy: not a real kv")
}
extern "C" fn expire(_handle: u64, _key: RString, _ttl: u64) -> oj_plugin_ffi::FfiFuture {
    oj_plugin_ffi::ready_err("mini-legacy: not a real kv")
}
extern "C" fn incr(_handle: u64, _key: RString) -> oj_plugin_ffi::FfiFuture {
    oj_plugin_ffi::ready_err("mini-legacy: not a real kv")
}
extern "C" fn close(_handle: u64) {}

static KV: KVStoreVtable = KVStoreVtable {
    connect,
    get,
    set,
    del,
    expire,
    incr,
    close,
};

fn init(
    _host: RArc<HostContext>,
    _cfg: RString,
) -> RResult<oj_plugin_ffi::PluginDescriptor, RString> {
    RResult::Ok(oj_plugin_ffi::PluginDescriptor {
        name: RString::from("mini-legacy"),
        semver: RString::from(env!("CARGO_PKG_VERSION")),
        abi_version: oj_plugin_ffi::ABI_VERSION,
        fingerprint: RString::from(oj_plugin_ffi::HOST_FINGERPRINT),
        desc: RString::from("loader 测试夹具（旧式手写符号，单轴 kv）"),
    })
}

// ---- 手写符号（旧宏形态：init 带 catch_unwind 收敛；无 oj_plugin_axes）----

#[unsafe(no_mangle)]
pub extern "C" fn oj_plugin_abi_version() -> u32 {
    oj_plugin_ffi::ABI_VERSION
}

#[unsafe(no_mangle)]
pub extern "C" fn oj_plugin_init(
    host: RArc<HostContext>,
    cfg: RString,
) -> RResult<oj_plugin_ffi::PluginDescriptor, RString> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| init(host, cfg))) {
        Ok(r) => r,
        Err(_) => RResult::Err(RString::from("panic in plugin init")),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn oj_plugin_axis_kv() -> *const core::ffi::c_void {
    &KV as *const KVStoreVtable as *const core::ffi::c_void
}
