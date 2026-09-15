//! 单轴测试夹具：只提供 mail 轴（submit 直接回空结果信封）——探测「有轴/无轴」正例，
//! 覆盖 `probe_axes` 的 `"mail"` 臂 + `axis::mail` helper + 宏生成的
//! `oj_plugin_axis_mail` 符号名端到端配对。真实实现见 plugins/oj-mail。

use oj_plugin_ffi::{
    HostContext, MailAttachment, MailVtable, RArc, RResult, RString, RVec, oj_plugin_entry,
};

/// 统一入口：不真投递，直接回结果信封 `{}`（本夹具只验证符号/类型配对，不验证投递语义）。
extern "C" fn submit(
    _key: RString,
    _req: RString,
    _atts: RVec<MailAttachment>,
) -> oj_plugin_ffi::FfiFuture {
    oj_plugin_ffi::ready_ok(b"{}")
}

static MAIL_VT: MailVtable = MailVtable { submit };

fn init(
    _host: RArc<HostContext>,
    _cfg: RString,
) -> RResult<oj_plugin_ffi::PluginDescriptor, RString> {
    RResult::Ok(oj_plugin_ffi::PluginDescriptor {
        name: RString::from("mini-mail"),
        semver: RString::from(env!("CARGO_PKG_VERSION")),
        abi_version: oj_plugin_ffi::ABI_VERSION,
        fingerprint: RString::from(oj_plugin_ffi::HOST_FINGERPRINT),
        desc: RString::from("loader 测试夹具（单轴 mail）"),
    })
}

oj_plugin_entry!(init, mail => oj_plugin_ffi::axis::mail(&MAIL_VT));
