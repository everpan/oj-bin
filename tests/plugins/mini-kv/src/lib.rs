//! 单轴测试夹具：只提供 kv 轴（假 vtable，方法一概 Err）——探测「有轴/无轴」的正例。
//! 真实 kv 实现见 plugins/oj-kv-redis；本夹具只服务 plugin_loader 探测测试。

use oj_plugin_ffi::{HostContext, KVStoreVtable, RArc, RResult, RString, oj_plugin_entry};

extern "C" fn connect(_cfg: RString) -> oj_plugin_ffi::FfiFuture {
    oj_plugin_ffi::ready_err("mini-kv: not a real kv")
}
extern "C" fn get(_handle: u64, _key: RString) -> oj_plugin_ffi::FfiFuture {
    oj_plugin_ffi::ready_err("mini-kv: not a real kv")
}
extern "C" fn set(_handle: u64, _key: RString, _value: RString) -> oj_plugin_ffi::FfiFuture {
    oj_plugin_ffi::ready_err("mini-kv: not a real kv")
}
extern "C" fn del(_handle: u64, _key: RString) -> oj_plugin_ffi::FfiFuture {
    oj_plugin_ffi::ready_err("mini-kv: not a real kv")
}
extern "C" fn expire(_handle: u64, _key: RString, _ttl: u64) -> oj_plugin_ffi::FfiFuture {
    oj_plugin_ffi::ready_err("mini-kv: not a real kv")
}
extern "C" fn incr(_handle: u64, _key: RString) -> oj_plugin_ffi::FfiFuture {
    oj_plugin_ffi::ready_err("mini-kv: not a real kv")
}
extern "C" fn cas(
    _handle: u64,
    _key: RString,
    _expected: oj_plugin_ffi::ROption<RString>,
    _value: oj_plugin_ffi::ROption<RString>,
    _ttl: u64,
) -> oj_plugin_ffi::FfiFuture {
    oj_plugin_ffi::ready_err("mini-kv: not a real kv")
}
extern "C" fn setnx(
    _handle: u64,
    _key: RString,
    _value: RString,
    _ttl: u64,
) -> oj_plugin_ffi::FfiFuture {
    oj_plugin_ffi::ready_err("mini-kv: not a real kv")
}
extern "C" fn get_set(
    _handle: u64,
    _key: RString,
    _value: RString,
    _ttl: u64,
) -> oj_plugin_ffi::FfiFuture {
    oj_plugin_ffi::ready_err("mini-kv: not a real kv")
}
extern "C" fn close(_handle: u64) {}

static KV: KVStoreVtable = KVStoreVtable {
    connect,
    get,
    set,
    del,
    expire,
    incr,
    cas,
    setnx,
    get_set,
    close,
};

fn init(
    _host: RArc<HostContext>,
    _cfg: RString,
) -> RResult<oj_plugin_ffi::PluginDescriptor, RString> {
    RResult::Ok(oj_plugin_ffi::PluginDescriptor {
        name: RString::from("mini-kv"),
        semver: RString::from(env!("CARGO_PKG_VERSION")),
        abi_version: oj_plugin_ffi::ABI_VERSION,
        fingerprint: RString::from(oj_plugin_ffi::HOST_FINGERPRINT),
        desc: RString::from("loader 测试夹具（单轴 kv）"),
    })
}

oj_plugin_entry!(init, kv => &KV);
