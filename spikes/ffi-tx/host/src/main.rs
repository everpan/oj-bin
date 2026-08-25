//! spike host：走通 connect -> begin -> tx_exec -> tx_commit 全链路；
//! 再测 begin 后不 commit 直接 close（= drop-rollback 的 FFI 映射）。

use libloading::Library;
use stabby::result::Result as RResult;
use stabby::string::String as RString;
use std::ffi::c_void;

type RBytes = stabby::vec::Vec<u8>;

#[repr(C)]
pub struct FfiFuture {
    pub state: *mut c_void,
    pub poll: extern "C" fn(*mut c_void) -> i32,
    pub take: extern "C" fn(*mut c_void) -> RResult<RBytes, RString>,
    pub free: extern "C" fn(*mut c_void),
}

impl FfiFuture {
    fn block_wait(mut self) -> Result<Vec<u8>, String> {
        loop {
            match (self.poll)(self.state) {
                0 => std::thread::yield_now(),
                code => {
                    let r = (self.take)(self.state);
                    (self.free)(self.state);
                    self.state = std::ptr::null_mut();
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
        if !self.state.is_null() {
            (self.free)(self.state);
        }
    }
}

fn s(v: &[u8]) -> String {
    String::from_utf8_lossy(v).into_owned()
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

fn main() {
    let lib = unsafe { Library::new(plugin_path()) }.expect("load plugin");
    let lib: &'static Library = Box::leak(Box::new(lib));
    unsafe {
        macro_rules! sym {
            ($name:expr, $ty:ty) => {
                *lib.get::<$ty>($name).unwrap()
            };
        }
        let connect = sym!(b"connect", extern "C" fn(RString) -> u64);
        let query = sym!(b"query", extern "C" fn(u64, RString, RString) -> FfiFuture);
        let begin = sym!(b"begin", extern "C" fn(u64) -> FfiFuture);
        let tx_exec = sym!(b"tx_exec", extern "C" fn(u64, u64, RString, RString) -> FfiFuture);
        let tx_commit = sym!(b"tx_commit", extern "C" fn(u64, u64) -> FfiFuture);
        let close = sym!(b"close", extern "C" fn(u64));
        let rolled_back = sym!(b"rolled_back_count", extern "C" fn() -> u64);

        // 全链路：connect -> query -> begin -> tx_exec -> tx_commit。
        let h = connect(RString::from("{}"));
        assert!(h > 0);
        let rows = query(h, RString::from("select 1"), RString::from("[]"))
            .block_wait()
            .unwrap();
        assert_eq!(s(&rows), "rows for [select 1]");

        let tx: u64 = s(&begin(h).block_wait().unwrap()).parse().unwrap();
        let r = tx_exec(h, tx, RString::from("insert t"), RString::from("[]"))
            .block_wait()
            .unwrap();
        assert_eq!(s(&r), "exec ok [insert t]");
        assert_eq!(s(&tx_commit(h, tx).block_wait().unwrap()), "committed");
        // commit 后 tx 失效。
        assert!(tx_commit(h, tx).block_wait().is_err());
        println!("tx chain OK: connect/query/begin/exec/commit + tx invalid after commit");

        // drop-rollback 映射：begin 后不 commit 直接 close。
        let before = rolled_back();
        let tx2: u64 = s(&begin(h).block_wait().unwrap()).parse().unwrap();
        let _ = tx_exec(h, tx2, RString::from("insert t2"), RString::from("[]"))
            .block_wait()
            .unwrap();
        close(h);
        assert_eq!(rolled_back(), before + 1, "close must roll back open tx");
        println!("drop-rollback mapping OK: close(h) rolled back uncommitted tx");

        println!("ALL SPIKE S.3 CHECKS PASSED");
    }
}
