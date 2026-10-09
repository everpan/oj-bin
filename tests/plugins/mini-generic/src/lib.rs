//! 泛型轴夹具：轴名 "greet" 不在宿主 9 个类型化轴内 → 泛型通道。
//! greet op：`call("greet", args)` 返回 `{"hello": <args[0].name>}`（name 缺省 "world"）。

use oj_plugin_ffi::{FfiFuture, HostContext, RArc, RResult, RString, oj_plugin_entry};

extern "C" fn greet(op: RString, args: RString) -> FfiFuture {
    if &op[..] != "greet" {
        return oj_plugin_ffi::ready_err(format!("greet axis: unknown op '{}'", &op[..]));
    }
    let name = serde_json::from_slice::<serde_json::Value>(args[..].as_bytes())
        .ok()
        .and_then(|v| v.as_array().and_then(|a| a.first().and_then(|v| v.as_str()).map(String::from)))
        .unwrap_or_else(|| "world".to_string());
    oj_plugin_ffi::ready_ok(serde_json::json!({ "hello": name }).to_string().into_bytes())
}

static GREET_VT: oj_plugin_ffi::GenericVtable = oj_plugin_ffi::GenericVtable { call: greet };

fn init(
    _host: RArc<HostContext>,
    _cfg: RString,
) -> RResult<oj_plugin_ffi::PluginDescriptor, RString> {
    RResult::Ok(oj_plugin_ffi::PluginDescriptor {
        name: RString::from("mini-generic"),
        semver: RString::from(env!("CARGO_PKG_VERSION")),
        abi_version: oj_plugin_ffi::ABI_VERSION,
        fingerprint: RString::from(oj_plugin_ffi::HOST_FINGERPRINT),
        desc: RString::from("loader 测试夹具（泛型轴 greet）"),
    })
}

oj_plugin_entry!(init, greet => &GREET_VT);
