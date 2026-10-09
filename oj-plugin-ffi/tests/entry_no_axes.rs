//! 入口宏：零轴、无 config 键形态（独立集成测试 crate）。

use oj_plugin_ffi::{RArc, RResult, RString, oj_plugin_entry};

mod fake2 {
    use super::*;

    oj_plugin_entry!(init);

    fn init(
        _host: RArc<oj_plugin_ffi::HostContext>,
        _cfg: RString,
    ) -> RResult<oj_plugin_ffi::PluginDescriptor, RString> {
        unreachable!()
    }
}

#[test]
fn entry_without_config_exports_no_config_key_symbol() {
    assert_eq!(fake2::oj_plugin_axes().iter().count(), 0);
}
