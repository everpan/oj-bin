//! dynptr 评估插件：经 stabby::dynptr 直出 dyn Pinger 对象。

use contract::{FfiFuture, Pinger, RBytes};
use stabby::result::Result as RResult;
use stabby::string::String as RString;
use std::ffi::c_void;

struct ReadyState {
    result: Option<Result<Vec<u8>, String>>,
}

extern "C" fn poll(state: *mut c_void) -> i32 {
    let s = unsafe { &mut *(state as *mut ReadyState) };
    match &s.result {
        Some(Ok(_)) => 1,
        Some(Err(_)) => -1,
        None => 0,
    }
}

extern "C" fn take(state: *mut c_void) -> RResult<RBytes, RString> {
    let s = unsafe { &mut *(state as *mut ReadyState) };
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
        drop(unsafe { Box::from_raw(state as *mut ReadyState) });
    }
}

fn ready_ok(s: &str) -> FfiFuture {
    let state = Box::into_raw(Box::new(ReadyState { result: Some(Ok(s.as_bytes().to_vec())) }));
    FfiFuture { state: state.cast(), poll, take, free }
}

struct MyPinger;

impl Pinger for MyPinger {
    extern "C" fn ping(&self) -> RString {
        RString::from("pong")
    }

    extern "C" fn ping_async(&self) -> FfiFuture {
        ready_ok("pong-async")
    }

    extern "C" fn boom(&self) -> RResult<RString, RString> {
        // panic 收敛：impl 侧 catch_unwind（与契约入口宏同一思路）。
        let f = || -> RString { panic!("pinger boom") };
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
            Ok(r) => RResult::Ok(r),
            Err(_) => RResult::Err(RString::from("panic in plugin")),
        }
    }
}

#[no_mangle]
extern "C" fn oj_plugin_abi_version() -> u32 {
    1
}

/// 直出 dyn 对象：宿主拿到 fat pointer，调 trait 方法走 stabby 生成的 vtable。
#[no_mangle]
extern "C" fn make_pinger() -> stabby::dynptr!(stabby::boxed::Box<dyn Pinger + Send + Sync>) {
    stabby::boxed::Box::new(MyPinger).into()
}
