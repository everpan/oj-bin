//! spike 插件（stabby 绑定）：echo roundtrip + ABI 版本 + panic 收敛 + descriptor。
//! 真实契约 crate（oj-plugin-ffi）的原型——入口宏内建 catch_unwind 见 `entry!` 思路：
//! 本样例手写等效包装，验证宏要收敛的行为。

use stabby::result::Result as RResult;
use stabby::string::String as RString;

/// 契约 descriptor（repr(C) + stabby 稳定布局）。
/// 注意：#[stabby::stabby] 保证布局稳定，但【字段变更不会被自动拒绝】——
/// 防线是宿主侧 ABI_VERSION 等值门禁（本文件 ABI_VERSION 常量）。
#[stabby::stabby]
#[repr(C)]
pub struct PluginDescriptor {
    pub name: RString,
    pub semver: RString,
    pub abi_version: u32,
    pub fingerprint: RString,
}

pub const ABI_VERSION: u32 = 1;

#[no_mangle]
extern "C" fn oj_plugin_abi_version() -> u32 {
    ABI_VERSION
}

/// catch_unwind 收敛：panic → RResult::Err（不跨 extern "C" 边界 unwind）。
#[no_mangle]
extern "C" fn echo(input: RString) -> RResult<RString, RString> {
    wrap(move || {
        let s: &str = &input[..];
        RString::from(format!("echo:{s}").as_str())
    })
}

/// 故意 panic 的入口：验证收敛而非进程 abort。
#[no_mangle]
extern "C" fn boom() -> RResult<RString, RString> {
    wrap(|| panic!("plugin boom"))
}

fn wrap(f: impl FnOnce() -> RString) -> RResult<RString, RString> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(r) => RResult::Ok(r),
        Err(_) => RResult::Err(RString::from("panic in plugin")),
    }
}

#[no_mangle]
extern "C" fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        name: RString::from("spike-stabby"),
        semver: RString::from("0.1.0"),
        abi_version: ABI_VERSION,
        fingerprint: RString::from(option_env!("SPIKE_FINGERPRINT").unwrap_or("dev")),
    }
}
