//! spike 插件（abi_stable 绑定）：与 stabby 版同接口，对比人体工学与版本校验。
//! abi_stable 特色：RootModule 自带库名/版本号校验 + 类型布局校验（TypeLayout）。

use abi_stable::std_types::{RResult, RString};

/// abi_stable 惯例：库根模块（RootModule）集中导出，加载时校验名称与版本。
/// 这里同时保留裸符号对比 stabby 形态。
pub const ABI_VERSION: u32 = 1;

#[no_mangle]
extern "C" fn oj_plugin_abi_version() -> u32 {
    ABI_VERSION
}

#[no_mangle]
extern "C" fn echo(input: RString) -> RResult<RString, RString> {
    wrap(move || RString::from(format!("echo:{input}").as_str()))
}

#[no_mangle]
extern "C" fn boom() -> RResult<RString, RString> {
    wrap(|| panic!("plugin boom"))
}

fn wrap(f: impl FnOnce() -> RString) -> RResult<RString, RString> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(r) => RResult::ROk(r),
        Err(_) => RResult::RErr(RString::from("panic in plugin")),
    }
}
