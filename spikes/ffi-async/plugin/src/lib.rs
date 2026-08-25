//! spike 插件：FfiFuture + 插件自建 tokio runtime。
//! 验证：插件内真实 tokio::time::sleep 不 panic（插件 TLS 挂在插件自己的 runtime 上）；
//! host 提前 drop FfiFuture 时插件任务仍跑完（drop = 放弃结果，不保证取消）。

use stabby::result::Result as RResult;
use stabby::string::String as RString;
type RBytes = stabby::vec::Vec<u8>;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

/// repr(C) future 句柄：插件侧 runtime 驱动的 oneshot 共享状态。
/// poll：非阻塞查询 0 pending / 1 ready / -1 error（error 细节在 take 里取）。
/// take：ready 后取结果，调用一次；之后 state 失效语义由 free 兜底。
/// free：释放 state。drop 语义 = host 调 free 而不 take。
#[repr(C)]
pub struct FfiFuture {
    pub state: *mut c_void,
    pub poll: extern "C" fn(*mut c_void) -> i32,
    pub take: extern "C" fn(*mut c_void) -> RResult<RBytes, RString>,
    pub free: extern "C" fn(*mut c_void),
}

/// 插件侧共享状态：oneshot 接结果；poll 收到后暂存（try_recv 是消费式的）。
struct SleepState {
    rx: tokio::sync::oneshot::Receiver<Result<Vec<u8>, String>>,
    result: Option<Result<Vec<u8>, String>>,
}

extern "C" fn poll(state: *mut c_void) -> i32 {
    let s = unsafe { &mut *(state as *mut SleepState) };
    if let Some(r) = &s.result {
        return if r.is_ok() { 1 } else { -1 };
    }
    match s.rx.try_recv() {
        Ok(r) => {
            let code = if r.is_ok() { 1 } else { -1 };
            s.result = Some(r);
            code
        }
        Err(tokio::sync::oneshot::error::TryRecvError::Empty) => 0,
        // sender 被 drop 却没 send：视为 error
        Err(tokio::sync::oneshot::error::TryRecvError::Closed) => -1,
    }
}

extern "C" fn take(state: *mut c_void) -> RResult<RBytes, RString> {
    let s = unsafe { &mut *(state as *mut SleepState) };
    match s.result.take() {
        Some(Ok(bytes)) => {
            let mut v = RBytes::new();
            for b in bytes {
                v.push(b);
            }
            RResult::Ok(v)
        }
        Some(Err(e)) => RResult::Err(RString::from(e.as_str())),
        None => RResult::Err(RString::from("take before ready or twice")),
    }
}

extern "C" fn free(state: *mut c_void) {
    if !state.is_null() {
        drop(unsafe { Box::from_raw(state as *mut SleepState) });
    }
}

/// 插件自建 runtime（跨 FFI 不共享宿主 tokio，规避 TLS 双副本问题）。
fn rt() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("plugin tokio runtime")
    })
}

static COMPLETED: AtomicU64 = AtomicU64::new(0);

#[no_mangle]
extern "C" fn oj_plugin_abi_version() -> u32 {
    1
}

/// 导出：sleep ms 毫秒后回 "slept:<ms>" 的字节。
#[no_mangle]
extern "C" fn sleep_ms(ms: u64) -> FfiFuture {
    let (tx, rx) = tokio::sync::oneshot::channel();
    rt().spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
        let _ = tx.send(Ok(format!("slept:{ms}").into_bytes()));
        COMPLETED.fetch_add(1, Ordering::SeqCst);
    });
    let state = Box::into_raw(Box::new(SleepState { rx, result: None }));
    FfiFuture { state: state.cast(), poll, take, free }
}

/// 观测用：插件侧已跑完的任务数（drop 语义断言）。
#[no_mangle]
extern "C" fn tasks_completed() -> u64 {
    COMPLETED.load(Ordering::SeqCst)
}
