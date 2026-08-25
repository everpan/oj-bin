//! FfiFuture：repr(C) future 句柄，插件侧 runtime 驱动的 oneshot 共享状态。
//! 形态经 spike S.2 实证定稿（spikes/ffi-async/NOTES.md）：
//! - poll：非阻塞查询 0 pending / 1 ready / -1 error（错误细节在 take 的 Err 里取）。
//! - take：ready 后调一次取结果；宿主 take→free 后必须 state 置 null（防 Drop 二次 free）。
//! - free：释放 state，null 安全；宿主 drop 句柄 = 放弃结果，插件任务允许跑完，不保证取消。
//! 插件侧注意：tokio oneshot try_recv 是消费式的，poll 取到值必须暂存进 state。
//!
//! I-2 修复（spec §3）：所有 vtable 方法经 [`catch_future`]/[`catch_void`] 包装，
//! 同步 panic 收敛为错误 future / 静默丢弃，不再跨界展开（UB）。poll/take/free
//! 统一走 [`spawn_ffi_future`] 提供的 task_*（同样包 catch_unwind）。

use crate::{RBytes, RResult, RString};
use std::ffi::c_void;

#[stabby::stabby]
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

// ---- 跨边界安全 FfiFuture 工厂（I-2；spec §3 统一 catch_unwind）----

use std::panic::{self, AssertUnwindSafe};

/// 插件侧共享异步任务状态（oneshot 收结果；result 暂存消费式取值）。
struct FfiTask {
    rx: tokio::sync::oneshot::Receiver<Result<Vec<u8>, String>>,
    result: Option<Result<Vec<u8>, String>>,
}

extern "C" fn task_poll(state: *mut c_void) -> i32 {
    let mut out = -1i32;
    let _ = panic::catch_unwind(AssertUnwindSafe(|| {
        let s = unsafe { &mut *(state as *mut FfiTask) };
        let code = if let Some(r) = &s.result {
            if r.is_ok() { 1 } else { -1 }
        } else {
            match s.rx.try_recv() {
                Ok(r) => {
                    let c = if r.is_ok() { 1 } else { -1 };
                    s.result = Some(r);
                    c
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => 0,
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => -1,
            }
        };
        out = code;
    }));
    out
}

extern "C" fn task_take(state: *mut c_void) -> RResult<RBytes, RString> {
    let mut out: RResult<RBytes, RString> = RResult::Err(RString::from("panic in ffi take"));
    let _ = panic::catch_unwind(AssertUnwindSafe(|| {
        let s = unsafe { &mut *(state as *mut FfiTask) };
        let res = match s.result.take() {
            Some(Ok(bytes)) => {
                let mut v = RBytes::new();
                for b in bytes {
                    v.push(b);
                }
                RResult::Ok(v)
            }
            Some(Err(e)) => RResult::Err(RString::from(e.as_str())),
            None => RResult::Err(RString::from("take before ready or twice")),
        };
        out = res;
    }));
    out
}

extern "C" fn task_free(state: *mut c_void) {
    if !state.is_null() {
        let _ = panic::catch_unwind(AssertUnwindSafe(|| unsafe {
            drop(Box::from_raw(state as *mut FfiTask));
        }));
    }
}

/// 起一个 FfiFuture：异步工作 spawn 到插件 runtime，oneshot 收结果；
/// poll/take/free 统一经 task_*（包 catch_unwind，同步 panic 不再跨界 UB）。
///
/// 插件侧以 `oj_plugin_ffi::spawn_ffi_future(&state().rt, async move { ... })` 取代
/// 原先各自重复的 `spawn_call` + `poll/take/free`。
pub fn spawn_ffi_future<F>(rt: &tokio::runtime::Runtime, work: F) -> FfiFuture
where
    F: std::future::Future<Output = Result<Vec<u8>, String>> + Send + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    rt.spawn(async move {
        let _ = tx.send(work.await);
    });
    FfiFuture {
        state: Box::into_raw(Box::new(FfiTask { rx, result: None })).cast(),
        poll: task_poll,
        take: task_take,
        free: task_free,
    }
}

/// 立即错误的 FfiFuture（同步构造失败 / panic 兜底；poll 立即 -1，take 取 Err）。
pub fn ready_err(msg: impl Into<String>) -> FfiFuture {
    let (tx, rx) = tokio::sync::oneshot::channel();
    drop(tx); // 已 closed → poll 直接读 result 而非 0（pending）
    FfiFuture {
        state: Box::into_raw(Box::new(FfiTask { rx, result: Some(Err(msg.into())) })).cast(),
        poll: task_poll,
        take: task_take,
        free: task_free,
    }
}

/// 包 catch_unwind 的 vtable 方法包装（返回 FfiFuture）：同步 panic → 立即错误 future，
/// 不再跨界展开（UB，spec §3）。用法：
/// `extern "C" fn get(h: u64, k: RString) -> FfiFuture { catch_future(|| { ... }) }`
pub fn catch_future<F>(f: F) -> FfiFuture
where
    F: FnOnce() -> FfiFuture,
{
    match panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(fut) => fut,
        Err(_) => ready_err("panic in plugin vtable method"),
    }
}

/// 包 catch_unwind 的 void vtable 方法包装（如 close）：同步 panic 收敛为静默，不跨界。
pub fn catch_void<F>(f: F)
where
    F: FnOnce(),
{
    let _ = panic::catch_unwind(AssertUnwindSafe(f));
}

/// 包 catch_unwind 的任意返回值包装（如 `register` 注册回调）。panic → `fallback`。
pub fn catch_value<T, F>(f: F, fallback: T) -> T
where
    F: FnOnce() -> T,
{
    match panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(_) => fallback,
    }
}
