//! FfiFuture：repr(C) future 句柄，插件侧 runtime 驱动的 oneshot 共享状态。
//! 形态经 spike S.2 实证定稿（spikes/ffi-async/NOTES.md）：
//! - poll：非阻塞查询 0 pending / 1 ready / -1 error（错误细节在 take 的 Err 里取）。
//! - take：ready 后调一次取结果；宿主 take→free 后必须 state 置 null（防 Drop 二次 free）。
//! - free：释放 state，null 安全；宿主 drop 句柄 = 放弃结果，插件任务允许跑完，不保证取消。
//! 插件侧注意：tokio oneshot try_recv 是消费式的，poll 取到值必须暂存进 state。

use crate::{RBytes, RResult, RString};
use std::ffi::c_void;

#[repr(C)]
pub struct FfiFuture {
    /// 插件侧共享状态（opaque）。
    pub state: *mut c_void,
    /// 0 pending / 1 ready / -1 error。
    pub poll: extern "C" fn(*mut c_void) -> i32,
    /// ready 后取结果，调用一次。
    pub take: extern "C" fn(*mut c_void) -> RResult<RBytes, RString>,
    /// 释放 state（null 安全）。
    pub free: extern "C" fn(*mut c_void),
}

// 字段均为 raw pointer / fn pointer，跨线程传递安全（所有权语义由契约约束：
// state 同一时刻只由一侧操作，poll/take/free 由宿主串行调用）。
unsafe impl Send for FfiFuture {}
unsafe impl Sync for FfiFuture {}
