//! dynptr 评估用共享契约（真实方案 = oj-plugin-ffi 同一 crate 两侧依赖）。

use stabby::result::Result as RResult;
use stabby::string::String as RString;
use std::ffi::c_void;

pub type RBytes = stabby::vec::Vec<u8>;

/// FfiFuture（与 ffi-async S.2 定稿同形），加 #[stabby::stabby] 让它过 checked vtable 校验。
#[stabby::stabby]
#[repr(C)]
pub struct FfiFuture {
    pub state: *mut c_void,
    pub poll: extern "C" fn(*mut c_void) -> i32,
    pub take: extern "C" fn(*mut c_void) -> RResult<RBytes, RString>,
    pub free: extern "C" fn(*mut c_void),
}

/// 最小 dynptr 评估 trait：一个同步方法 + 一个返回 FfiFuture 的方法 + 一个 panic 方法。
/// panic 收敛在 impl 侧 catch_unwind（与 vtable 形态同一宏思路）。
#[stabby::stabby(checked)]
pub trait Pinger {
    extern "C" fn ping(&self) -> RString;
    extern "C" fn ping_async(&self) -> FfiFuture;
    extern "C" fn boom(&self) -> RResult<RString, RString>;
}
