//! spike host：在自己的 tokio runtime 里 await 插件 FfiFuture；断言 drop 语义。

use libloading::Library;
use stabby::result::Result as RResult;
use stabby::string::String as RString;
type RBytes = stabby::vec::Vec<u8>;
use std::ffi::c_void;

/// 与插件侧布局镜像（真实方案 = 契约 crate 同一类型）。
#[repr(C)]
pub struct FfiFuture {
    pub state: *mut c_void,
    pub poll: extern "C" fn(*mut c_void) -> i32,
    pub take: extern "C" fn(*mut c_void) -> RResult<RBytes, RString>,
    pub free: extern "C" fn(*mut c_void),
}

impl FfiFuture {
    /// 宿主侧 await 桥：poll 轮询 + yield_now；ready 后 take + free。
    async fn wait(mut self) -> Result<Vec<u8>, String> {
        loop {
            match (self.poll)(self.state) {
                0 => tokio::task::yield_now().await,
                code => {
                    let r = (self.take)(self.state);
                    (self.free)(self.state);
                    self.state = std::ptr::null_mut(); // 防 Drop 二次 free
                    return match (code, std::result::Result::from(r)) {
                        (1, Ok(b)) => Ok(b.iter().copied().collect()),
                        (_, Ok(_)) => Err("poll=-1 but take ok".into()),
                        (_, Err(e)) => Err(e[..].to_string()),
                    };
                }
            }
        }
    }
}

impl Drop for FfiFuture {
    fn drop(&mut self) {
        // 宿主放弃结果：只 free 不 take（插件任务允许跑完）。wait 已 free 时 state 为 null。
        if !self.state.is_null() {
            (self.free)(self.state);
        }
    }
}

fn plugin_path() -> std::path::PathBuf {
    let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    let (prefix, ext) = if cfg!(target_os = "windows") {
        ("", "dll")
    } else if cfg!(target_os = "macos") {
        ("lib", "dylib")
    } else {
        ("lib", "so")
    };
    p.join("target/debug").join(format!("{prefix}plugin.{ext}"))
}

#[tokio::main]
async fn main() {
    let lib = unsafe { Library::new(plugin_path()) }.expect("load plugin");
    let lib: &'static Library = Box::leak(Box::new(lib));

    let abi = unsafe { lib.get::<extern "C" fn() -> u32>(b"oj_plugin_abi_version").unwrap()() };
    assert_eq!(abi, 1);

    let sleep_ms = unsafe { lib.get::<extern "C" fn(u64) -> FfiFuture>(b"sleep_ms").unwrap() };
    let tasks_completed =
        unsafe { lib.get::<extern "C" fn() -> u64>(b"tasks_completed").unwrap() };

    // 1) 正常 await：插件内真实 tokio::time::sleep 不 panic（无 "no reactor running"）。
    let out = sleep_ms(50).wait().await.expect("sleep_ms roundtrip");
    assert_eq!(out, b"slept:50");
    println!("async roundtrip OK: {}", String::from_utf8_lossy(&out));

    // 2) drop 语义：host 立即 drop（free 不 take），插件任务仍跑完。
    let before = tasks_completed();
    drop(sleep_ms(80));
    // 轮询等插件侧任务完成（不 sleep 满 80ms，防止调度抖动）。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while tasks_completed() < before + 1 {
        assert!(std::time::Instant::now() < deadline, "plugin task did not finish after host drop");
        tokio::task::yield_now().await;
    }
    println!("drop semantics OK: plugin task completed after host dropped FfiFuture");

    println!("ALL SPIKE S.2 CHECKS PASSED");
}
