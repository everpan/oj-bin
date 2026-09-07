//! 单轴测试夹具：只提供 mq 轴（JSON method dispatch 假实现）——探测「有轴/无轴」
//! 正例 + call echo 契约冒烟。真实实现见 plugins/oj-bus-kafka / oj-bus-rabbitmq。

use oj_plugin_ffi::{HostContext, MqVtable, RArc, RResult, RString, oj_plugin_entry};

extern "C" fn connect(_cfg: RString) -> oj_plugin_ffi::FfiFuture {
    oj_plugin_ffi::ready_ok(br#"{"handle":1}"#)
}

extern "C" fn call(_handle: u64, method: RString, payload: RString) -> oj_plugin_ffi::FfiFuture {
    // echo：method + payload 原样回显（宿主契约冒烟：JSON in → JSON out）
    let out = format!(
        "{{\"method\":\"{}\",\"payload\":{}}}",
        &method[..],
        &payload[..]
    );
    oj_plugin_ffi::ready_ok(out.into_bytes())
}

extern "C" fn close(_handle: u64) {}

static MQ: MqVtable = MqVtable {
    connect,
    call,
    close,
};

fn init(
    _host: RArc<HostContext>,
    _cfg: RString,
) -> RResult<oj_plugin_ffi::PluginDescriptor, RString> {
    RResult::Ok(oj_plugin_ffi::PluginDescriptor {
        name: RString::from("mini-mq"),
        semver: RString::from(env!("CARGO_PKG_VERSION")),
        abi_version: oj_plugin_ffi::ABI_VERSION,
        fingerprint: RString::from(oj_plugin_ffi::HOST_FINGERPRINT),
        desc: RString::from("loader 测试夹具（单轴 mq）"),
    })
}

oj_plugin_entry!(init, mq => oj_plugin_ffi::axis::mq(&MQ));
