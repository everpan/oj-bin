//! spike 插件：tx 句柄化 DataAccessor FFI 原型（内存实现，不依赖 sqlx）。
//! vtable 形态：connect -> u64 handle；begin -> tx_id(u64)；tx_* 携带 tx_id。
//! close(handle) 时未 commit 的 tx 全部 rollback（= drop-rollback 语义的 FFI 映射）。
//! 内存操作无真实异步，FfiFuture 一律预置 ready（S.2 已实证真实异步路径）。

use stabby::result::Result as RResult;
use stabby::string::String as RString;
use stabby::vec::Vec as RVec;
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

type RBytes = RVec<u8>;

#[repr(C)]
pub struct FfiFuture {
    pub state: *mut c_void,
    pub poll: extern "C" fn(*mut c_void) -> i32,
    pub take: extern "C" fn(*mut c_void) -> RResult<RBytes, RString>,
    pub free: extern "C" fn(*mut c_void),
}

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

fn ready(result: Result<Vec<u8>, String>) -> FfiFuture {
    let state = Box::into_raw(Box::new(ReadyState { result: Some(result) }));
    FfiFuture { state: state.cast(), poll, take, free }
}

fn ready_ok(s: String) -> FfiFuture {
    ready(Ok(s.into_bytes()))
}

fn ready_err(e: impl Into<String>) -> FfiFuture {
    ready(Err(e.into()))
}

// ---- 连接/事务表 ----

#[derive(Default)]
struct Conn {
    open_txs: HashSet<u64>,
}

fn conns() -> &'static Mutex<HashMap<u64, Conn>> {
    static CONNS: OnceLock<Mutex<HashMap<u64, Conn>>> = OnceLock::new();
    CONNS.get_or_init(|| Mutex::new(HashMap::new()))
}

static NEXT_HANDLE: AtomicU64 = AtomicU64::new(1);
static NEXT_TX: AtomicU64 = AtomicU64::new(1);
static ROLLED_BACK: AtomicU64 = AtomicU64::new(0);

#[no_mangle]
extern "C" fn oj_plugin_abi_version() -> u32 {
    1
}

/// connect(cfg_json) -> handle（0 保留为无效）。
#[no_mangle]
extern "C" fn connect(_cfg_json: RString) -> u64 {
    let h = NEXT_HANDLE.fetch_add(1, Ordering::SeqCst);
    conns().lock().unwrap().insert(h, Conn::default());
    h
}

#[no_mangle]
extern "C" fn query(handle: u64, sql: RString, _params_json: RString) -> FfiFuture {
    if !conns().lock().unwrap().contains_key(&handle) {
        return ready_err(format!("bad handle {handle}"));
    }
    ready_ok(format!("rows for [{sql}]"))
}

#[no_mangle]
extern "C" fn begin(handle: u64) -> FfiFuture {
    let mut m = conns().lock().unwrap();
    let Some(c) = m.get_mut(&handle) else {
        return ready_err(format!("bad handle {handle}"));
    };
    let tx = NEXT_TX.fetch_add(1, Ordering::SeqCst);
    c.open_txs.insert(tx);
    ready_ok(tx.to_string()) // payload = tx_id 的 ascii
}

fn with_tx(handle: u64, tx_id: u64, f: impl FnOnce(&mut Conn) -> FfiFuture) -> FfiFuture {
    let mut m = conns().lock().unwrap();
    let Some(c) = m.get_mut(&handle) else {
        return ready_err(format!("bad handle {handle}"));
    };
    if !c.open_txs.contains(&tx_id) {
        return ready_err(format!("unknown tx {tx_id}"));
    }
    f(c)
}

#[no_mangle]
extern "C" fn tx_exec(handle: u64, tx_id: u64, sql: RString, _params_json: RString) -> FfiFuture {
    with_tx(handle, tx_id, |_| ready_ok(format!("exec ok [{sql}]")))
}

#[no_mangle]
extern "C" fn tx_commit(handle: u64, tx_id: u64) -> FfiFuture {
    with_tx(handle, tx_id, |c| {
        c.open_txs.remove(&tx_id);
        ready_ok("committed".into())
    })
}

#[no_mangle]
extern "C" fn tx_rollback(handle: u64, tx_id: u64) -> FfiFuture {
    with_tx(handle, tx_id, |c| {
        c.open_txs.remove(&tx_id);
        ROLLED_BACK.fetch_add(1, Ordering::SeqCst);
        ready_ok("rolled back".into())
    })
}

/// close：未 commit 的 tx 全部 rollback 后移除连接。
#[no_mangle]
extern "C" fn close(handle: u64) {
    let mut m = conns().lock().unwrap();
    if let Some(c) = m.remove(&handle) {
        ROLLED_BACK.fetch_add(c.open_txs.len() as u64, Ordering::SeqCst);
    }
}

/// 观测用：累计 rollback 数（含 close 隐式 rollback）。
#[no_mangle]
extern "C" fn rolled_back_count() -> u64 {
    ROLLED_BACK.load(Ordering::SeqCst)
}
