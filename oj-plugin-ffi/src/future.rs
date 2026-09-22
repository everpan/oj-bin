//! FfiFuture：repr(C) future 句柄，插件侧 runtime 驱动的 oneshot 共享状态。
//! 形态经 spike S.2 实证定稿（spikes/ffi-async/NOTES.md）：
//!
//! - poll：非阻塞查询 0 pending / 1 ready / -1 error（错误细节在 take 的 Err 里取）。
//! - take：ready 后调一次取结果；宿主 take→free 后必须 state 置 null（防 Drop 二次 free）。
//! - free：释放 state，null 安全；宿主 drop 句柄 = 放弃结果，插件任务允许跑完，不保证取消。

//!   插件侧注意：tokio oneshot try_recv 是消费式的，poll 取到值必须暂存进 state。
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
        state: Box::into_raw(Box::new(FfiTask {
            rx,
            result: Some(Err(msg.into())),
        }))
        .cast(),
        poll: task_poll,
        take: task_take,
        free: task_free,
    }
}

/// 立即完成的 Ok FfiFuture（夹具 / 固定响应用；与 ready_err 同款即刻完结语义）。
pub fn ready_ok(bytes: impl Into<Vec<u8>>) -> FfiFuture {
    let (tx, rx) = tokio::sync::oneshot::channel();
    drop(tx); // 已 closed → poll 直接读 result 而非 0（pending）
    FfiFuture {
        state: Box::into_raw(Box::new(FfiTask {
            rx,
            result: Some(Ok(bytes.into())),
        }))
        .cast(),
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

#[cfg(test)]
mod tests {
    use super::*;

    // current_thread：block_on 单线程驱动 spawned 任务，yield 一轮即完成，
    // 不依赖跨 worker 线程的调度时序（multi_thread 在 CPU 争抢下会偶发饿死本循环）。
    // 仅需 tokio "rt" feature，与 crate 声明一致（rt-multi-thread 并不存在于 Cargo.toml，
    // 之前靠 workspace feature 统一才能编译，单独 -p 构建直接失败）。
    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    /// ready_ok：poll=1、take=Ok、free 不 UB（task_poll/task_take/task_free 全链路）。
    #[test]
    fn ready_ok_poll_take_free_roundtrip() {
        let f = ready_ok(b"hello");
        assert_eq!((f.poll)(f.state), 1, "ready_ok 必须立即 ready");
        let r = (f.take)(f.state);
        assert!(std::result::Result::from(r).is_ok(), "take 应回 Ok");
        (f.free)(f.state);
    }

    /// ready_err：poll=-1、take=Err（错误臂）。
    #[test]
    fn ready_err_poll_take_roundtrip() {
        let f = ready_err("boom");
        assert_eq!((f.poll)(f.state), -1, "ready_err 必须立即 error");
        let r = (f.take)(f.state);
        assert!(std::result::Result::from(r).is_err(), "take 应回 Err");
        (f.free)(f.state);
    }

    /// task_free 对 null state 必须 no-op（不 UB）；任一 future 的 free 指针传 null 即可触发分支。
    #[test]
    fn free_null_is_safe() {
        let f = ready_ok(b"x");
        (f.free)(std::ptr::null_mut());
    }

    /// spawn_ffi_future：pending 时 poll=0、take 在 ready 前回 Err；完成后 poll=1、take=Ok。
    #[test]
    fn spawn_ffi_future_pending_then_ready() {
        let rt = rt();
        let pending = spawn_ffi_future(&rt, async {
            std::future::pending::<Result<Vec<u8>, String>>().await
        });
        assert_eq!(
            (pending.poll)(pending.state),
            0,
            "未完成的 future 必须 pending"
        );
        // ready 之前 take → result 仍未暂存 → Err("take before ready or twice")。
        assert!(
            std::result::Result::from((pending.take)(pending.state)).is_err(),
            "ready 前 take 必须 Err"
        );
        (pending.free)(pending.state);

        let done = spawn_ffi_future(&rt, async { Ok(b"y".to_vec()) });
        rt.block_on(async {
            for _ in 0..2000 {
                if (done.poll)(done.state) != 0 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        });
        assert_eq!((done.poll)(done.state), 1, "完成的 future 必须 ready");
        assert!(
            std::result::Result::from((done.take)(done.state)).is_ok(),
            "完成后 take 必须 Ok"
        );
        (done.free)(done.state);
    }

    /// catch_future：同步 panic → 收敛为立即错误的 future（不跨界展开）。
    #[test]
    fn catch_future_panic_returns_error_future() {
        let f = catch_future(|| -> FfiFuture { panic!("boom") });
        assert_eq!((f.poll)(f.state), -1, "panic 必须转为 error future");
        (f.free)(f.state);
    }

    /// catch_void：同步 panic 收敛为静默（不终止测试进程）。
    #[test]
    fn catch_void_swallows_panic() {
        catch_void(|| panic!("should be swallowed"));
    }

    /// catch_value：panic → fallback；正常 → 原值。
    #[test]
    fn catch_value_returns_fallback_on_panic() {
        assert_eq!(catch_value(|| panic!("boom"), 42i32), 42);
        assert_eq!(catch_value(|| 7i32, 42i32), 7);
    }
}
