
use super::*;
use oj_plugin_ffi::{RBytes, RResult};
use std::ffi::c_void;
use std::time::Duration;

struct CountedState {
    left: u32,
}

extern "C" fn counted_poll(state: *mut c_void) -> i32 {
    let s = unsafe { &mut *(state as *mut CountedState) };
    if s.left == 0 {
        1
    } else {
        s.left -= 1;
        0
    }
}

extern "C" fn counted_take(state: *mut c_void) -> RResult<RBytes, RString> {
    let s = unsafe { &*(state as *mut CountedState) };
    let _ = s; // 只读；释放由 free 统一负责（await_ffi_poll take→free 配对）
    let mut v = RBytes::new();
    for b in b"ok" {
        v.push(*b);
    }
    RResult::Ok(v)
}

extern "C" fn counted_free(state: *mut c_void) {
    if !state.is_null() {
        drop(unsafe { Box::from_raw(state as *mut CountedState) });
    }
}

fn counted_future(left: u32) -> FfiFuture {
    let state = Box::into_raw(Box::new(CountedState { left }));
    FfiFuture {
        state: state.cast(),
        poll: counted_poll,
        take: counted_take,
        free: counted_free,
    }
}

/// 20 次 pending × 10ms 退避：总耗时 ≥ 180ms 证明在睡眠而非 yield_now 空转。
#[tokio::test]
async fn given_many_pending_polls_when_await_ffi_poll_then_backs_off_not_spins() {
    let t0 = std::time::Instant::now();
    let out = await_ffi_poll(counted_future(20), Duration::from_millis(10))
        .await
        .unwrap();
    assert_eq!(out, b"ok".to_vec());
    assert!(
        t0.elapsed().as_millis() >= 180,
        "no backoff: {:?}",
        t0.elapsed()
    );
}

/// 就绪 future 不额外睡眠（退避只发生在 pending 间隙）。
#[tokio::test]
async fn given_ready_future_when_await_ffi_poll_then_returns_without_sleep() {
    let t0 = std::time::Instant::now();
    let out = await_ffi_poll(counted_future(0), Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(out, b"ok".to_vec());
    assert!(t0.elapsed().as_millis() < 100, "{:?}", t0.elapsed());
}

/// await_ffi 自适应退避：前 64 次 poll 走 yield_now（快操作零延迟），此后每次
/// 睡 500µs——96 次 pending ⇒ ≥32 次睡眠，墙钟证明不再纯 yield 空转烧核。
#[tokio::test]
async fn given_many_pending_polls_when_await_ffi_then_sleeps_after_yield_budget() {
    let t0 = std::time::Instant::now();
    let out = await_ffi(counted_future(96)).await.unwrap();
    assert_eq!(out, b"ok".to_vec());
    assert!(
        t0.elapsed().as_millis() >= 12,
        "still spinning past yield budget: {:?}",
        t0.elapsed()
    );
}

/// yield 预算内完成的 future 不引入睡眠尾延迟（64 次 pending 走快路径）。
#[tokio::test]
async fn given_pending_within_yield_budget_when_await_ffi_then_no_sleep_tail() {
    let t0 = std::time::Instant::now();
    let out = await_ffi(counted_future(64)).await.unwrap();
    assert_eq!(out, b"ok".to_vec());
    assert!(t0.elapsed().as_millis() < 100, "{:?}", t0.elapsed());
}

// ---- poll=-1（错误码）但 take 结果各异的协议边界 ----

extern "C" fn err_poll(_state: *mut c_void) -> i32 {
    -1
}
extern "C" fn ok_take(_state: *mut c_void) -> RResult<RBytes, RString> {
    let mut v = RBytes::new();
    for b in b"ok" {
        v.push(*b);
    }
    RResult::Ok(v)
}
extern "C" fn err_take(_state: *mut c_void) -> RResult<RBytes, RString> {
    RResult::Err(RString::from("late boom"))
}
fn code_future(take: extern "C" fn(*mut c_void) -> RResult<RBytes, RString>) -> FfiFuture {
    FfiFuture {
        state: std::ptr::null_mut(),
        poll: err_poll,
        take,
        free: counted_free,
    }
}

/// poll=-1 但 take=Ok → 协议违约兜底文案（不把成功结果误当错误细节）。
#[tokio::test]
async fn given_error_code_but_ok_take_when_await_ffi_then_reports_protocol_violation() {
    let e = await_ffi(code_future(ok_take)).await.unwrap_err();
    assert!(
        e.contains("ffi poll reported error but take succeeded"),
        "{e}"
    );
}

/// await_ffi_poll 同款兜底（mq 长轮询变体保持同一协议语义）。
#[tokio::test]
async fn given_error_code_but_ok_take_when_await_ffi_poll_then_reports_protocol_violation() {
    let e = await_ffi_poll(code_future(ok_take), Duration::from_millis(1))
        .await
        .unwrap_err();
    assert!(
        e.contains("ffi poll reported error but take succeeded"),
        "{e}"
    );
}

/// poll=-1 且 take=Err → 插件错误文案透传（await_ffi_poll 的错误臂）。
#[tokio::test]
async fn given_error_code_and_err_take_when_await_ffi_poll_then_err_carries_plugin_message() {
    let e = await_ffi_poll(code_future(err_take), Duration::from_millis(1))
        .await
        .unwrap_err();
    assert_eq!(e, "late boom");
}
