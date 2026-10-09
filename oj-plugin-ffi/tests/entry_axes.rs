//! 入口宏：轴清单自报 + config_key（独立集成测试 crate，避免 no_mangle 符号冲突）。

use oj_plugin_ffi::{RArc, RResult, RString, oj_plugin_entry};

#[repr(C)]
struct FakeVt {
    _pad: u8,
}

mod fake {
    use super::*;

    pub static VT_A: FakeVt = FakeVt { _pad: 0 };
    pub static VT_B: FakeVt = FakeVt { _pad: 1 };

    oj_plugin_entry!(init, config: "cache", cache => &VT_A, search => &VT_B);

    fn init(
        _host: RArc<oj_plugin_ffi::HostContext>,
        _cfg: RString,
    ) -> RResult<oj_plugin_ffi::PluginDescriptor, RString> {
        unreachable!()
    }
}

#[test]
fn entry_generates_axes_list_and_config_key() {
    let axes = fake::oj_plugin_axes();
    let names: Vec<String> = axes.iter().map(|d| d.name[..].to_string()).collect();
    assert_eq!(names, vec!["cache".to_string(), "search".to_string()]);
    assert_eq!(
        axes.iter().next().unwrap().vtable,
        &fake::VT_A as *const FakeVt as *const core::ffi::c_void
    );
    assert!(!fake::oj_plugin_axis_cache().is_null());
    assert_eq!(&fake::oj_plugin_config_key()[..], "cache");
}
