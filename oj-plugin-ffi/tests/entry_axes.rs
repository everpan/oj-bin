//! 入口宏：轴清单自报 + config_key（独立集成测试 crate，避免 no_mangle 符号冲突）。

use oj_plugin_ffi::{AXIS_KIND_GENERIC, AXIS_KIND_TYPED, RArc, RResult, RString, oj_plugin_entry};

#[repr(C)]
struct FakeVt {
    _pad: u8,
}

mod fake {
    use super::*;

    pub static VT_A: FakeVt = FakeVt { _pad: 0 };
    pub static VT_B: FakeVt = FakeVt { _pad: 1 };

    extern "C" fn greet_stub(_op: RString, _args: RString) -> oj_plugin_ffi::FfiFuture {
        unreachable!()
    }
    pub static GREET_VT: oj_plugin_ffi::GenericVtable =
        oj_plugin_ffi::GenericVtable { call: greet_stub };

    // 混合形态：类型化臂 + 泛型臂 + config 键，一条展开全盖；尾逗号容忍也钉在此
    // （旧宏 $(,)? 语义的回归守护：基底臂 @munch 须接受条目列表后的尾逗号）。
    oj_plugin_entry!(init, config: "cache", cache => &VT_A, search => &VT_B, generic(greet) => &GREET_VT,);

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
    assert_eq!(
        names,
        vec![
            "cache".to_string(),
            "search".to_string(),
            "greet".to_string()
        ]
    );
    assert_eq!(
        axes.iter().next().unwrap().vtable,
        &fake::VT_A as *const FakeVt as *const core::ffi::c_void
    );
    let kinds: Vec<u8> = axes.iter().map(|d| d.kind).collect();
    assert_eq!(
        kinds,
        vec![AXIS_KIND_TYPED, AXIS_KIND_TYPED, AXIS_KIND_GENERIC]
    );
    assert!(!fake::oj_plugin_axis_cache().is_null());
    // 泛型臂也照常生成 per-axis 符号（旧宿主回退路径可用）。
    assert!(!fake::oj_plugin_axis_greet().is_null());
    assert_eq!(&fake::oj_plugin_config_key()[..], "cache");
}
