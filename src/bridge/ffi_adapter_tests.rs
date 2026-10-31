// `T_LOCK` 是**测试串行化**锁（保护进程级共享 statics：FAIL_NEXT / LAST_SEARCH / FREED），
// 不是被 await 的临界区数据锁：这些用例跑在 `multi_thread` runtime 上，持锁跨 await 只是
// 让同批用例排队，不会与其它 lock 形成环，故无死锁风险；改成 drop 再 await 反而会失去串行化。
// 与 `oj/tests/e2e.rs`、`server/src/ws.rs` 的同类豁免同理（注释互指）。

use super::*;
use async_trait::async_trait;
use oj_plugin_ffi::{RBytes, ready_err, ready_ok};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

// ---- mock vtable：Rust 函数指针填充，预置 ready 的 FfiFuture ----

struct ReadyState {
    result: Option<Result<Vec<u8>, String>>,
}

extern "C" fn mock_poll(state: *mut c_void) -> i32 {
    let s = unsafe { &mut *(state as *mut ReadyState) };
    match &s.result {
        Some(Ok(_)) => 1,
        Some(Err(_)) => -1,
        None => 0,
    }
}

extern "C" fn mock_take(state: *mut c_void) -> RResult<RBytes, RString> {
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

extern "C" fn mock_free(state: *mut c_void) {
    if !state.is_null() {
        drop(unsafe { Box::from_raw(state as *mut ReadyState) });
    }
}

fn ready(r: Result<Vec<u8>, String>) -> FfiFuture {
    let state = Box::into_raw(Box::new(ReadyState { result: Some(r) }));
    FfiFuture {
        state: state.cast(),
        poll: mock_poll,
        take: mock_take,
        free: mock_free,
    }
}

/// 共享 statics 串行化（并行测试互踩 FAIL_NEXT/LAST_SEARCH）。
static T_LOCK: Mutex<()> = Mutex::new(());
static LAST_SEARCH: Mutex<(u64, String, String)> = Mutex::new((0, String::new(), String::new()));
static CLOSED: AtomicBool = AtomicBool::new(false);
static FAIL_NEXT: AtomicBool = AtomicBool::new(false);
static FREED: AtomicU64 = AtomicU64::new(0);

/// 模式开关守卫：置位 mock 的行为开关，作用域结束（含 panic）自动复位为 0，
/// 防止失败用例污染同轴后续用例。用法：`let _m = Mode::set(&ES_MODE, 2);`。
struct Mode<'a>(&'a Mutex<u8>);
impl Mode<'_> {
    fn set(m: &'static Mutex<u8>, v: u8) -> Self {
        *m.lock().unwrap() = v;
        Self(m)
    }
}
impl Drop for Mode<'_> {
    fn drop(&mut self) {
        *self.0.lock().unwrap() = 0;
    }
}

/// es 行为开关：2=search 返回非 JSON；3=index_doc 报错；4=delete_doc 报错。
static ES_MODE: Mutex<u8> = Mutex::new(0);

extern "C" fn mock_search(handle: u64, index: RString, body: RString) -> FfiFuture {
    *LAST_SEARCH.lock().unwrap() = (handle, index[..].to_string(), body[..].to_string());
    if FAIL_NEXT.swap(false, Ordering::SeqCst) {
        return ready(Err("boom from plugin".into()));
    }
    if *ES_MODE.lock().unwrap() == 2 {
        return ready(Ok(b"gibberish".to_vec()));
    }
    ready(Ok(br#"{"hits":[]}"#.to_vec()))
}

extern "C" fn mock_index_doc(
    _handle: u64,
    _index: RString,
    _id: RString,
    _body: RString,
) -> FfiFuture {
    if *ES_MODE.lock().unwrap() == 3 {
        return ready(Err("index boom".into()));
    }
    ready(Ok(br#"{"result":"created"}"#.to_vec()))
}

extern "C" fn mock_delete_doc(_handle: u64, _index: RString, _id: RString) -> FfiFuture {
    if *ES_MODE.lock().unwrap() == 4 {
        return ready(Err("delete boom".into()));
    }
    ready(Ok(br#"{"result":"deleted"}"#.to_vec()))
}

extern "C" fn mock_close(_handle: u64) {
    CLOSED.store(true, Ordering::SeqCst);
}

use std::sync::Mutex;

fn mock_vtable() -> &'static EsBackendVtable {
    Box::leak(Box::new(EsBackendVtable {
        search: mock_search,
        index_doc: mock_index_doc,
        delete_doc: mock_delete_doc,
        close: mock_close,
    }))
}

#[tokio::test(flavor = "multi_thread")]
async fn search_forwards_params_and_decodes_response() {
    let _g = T_LOCK.lock().unwrap();
    let b = FfiEsBackend::new(42, mock_vtable());
    let v = b.search("idx1", serde_json::json!({"q": 1})).await.unwrap();
    assert_eq!(v, serde_json::json!({"hits": []}));
    let (h, i, body) = LAST_SEARCH.lock().unwrap().clone();
    assert_eq!(h, 42);
    assert_eq!(i, "idx1");
    assert_eq!(body, r#"{"q":1}"#);
}

#[tokio::test(flavor = "multi_thread")]
async fn plugin_error_maps_to_bridge_err() {
    let _g = T_LOCK.lock().unwrap();
    FAIL_NEXT.store(true, Ordering::SeqCst);
    let b = FfiEsBackend::new(1, mock_vtable());
    let err = b.search("i", serde_json::json!({})).await.unwrap_err();
    assert!(err.to_string().contains("boom from plugin"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn index_and_delete_roundtrip() {
    let _g = T_LOCK.lock().unwrap();
    let b = FfiEsBackend::new(1, mock_vtable());
    let v = b
        .index_doc("i", "7", serde_json::json!({"a":1}))
        .await
        .unwrap();
    assert_eq!(v["result"], "created");
    let v = b.delete_doc("i", "7").await.unwrap();
    assert_eq!(v["result"], "deleted");
}

#[test]
fn drop_calls_close() {
    let _g = T_LOCK.lock().unwrap();
    CLOSED.store(false, Ordering::SeqCst);
    drop(FfiEsBackend::new(9, mock_vtable()));
    assert!(CLOSED.load(Ordering::SeqCst));
}

extern "C" fn counting_free(state: *mut c_void) {
    FREED.fetch_add(1, Ordering::SeqCst);
    mock_free(state);
}

#[tokio::test(flavor = "multi_thread")]
async fn guard_drop_frees_without_take() {
    let before = FREED.load(Ordering::SeqCst);
    {
        let _g = FfiGuard(Some(FfiFuture {
            state: Box::into_raw(Box::new(ReadyState {
                result: Some(Ok(vec![])),
            }))
            .cast(),
            poll: mock_poll,
            take: mock_take,
            free: counting_free,
        }));
    }
    assert_eq!(FREED.load(Ordering::SeqCst), before + 1);
}

// ---- db 轴 mock vtable（Task 4.1；共享 statics 串行化互踩）----

use oj_plugin_ffi::{RResult, RVec};
use std::sync::atomic::Ordering as AtomicOrdering;

static DB_CONNECTED_CFG: Mutex<String> = Mutex::new(String::new());
static DB_QUERY: Mutex<(u64, String, String)> = Mutex::new((0, String::new(), String::new()));
static DB_TX_QUERY: Mutex<(u64, u64, String, String)> =
    Mutex::new((0, 0, String::new(), String::new()));
static DB_COMMITTED: AtomicU64 = AtomicU64::new(0);
static DB_ROLLED_BACK: AtomicU64 = AtomicU64::new(0);
static DB_CLOSED: AtomicU64 = AtomicU64::new(0);
/// db connect 行为开关：1=报错；2=非 JSON；3=缺 handle。
static DB_CONNECT_MODE: Mutex<u8> = Mutex::new(0);
/// begin 行为开关：1=报错；2=非 JSON；3=缺 tx_id。
static DB_BEGIN_MODE: Mutex<u8> = Mutex::new(0);
/// query/exec/tx_query/tx_exec 行为开关：1=报错；2=非 JSON。
static DB_CALL_MODE: Mutex<u8> = Mutex::new(0);
/// commit/rollback 行为开关：1=报错（记录 tx_id 后再报，模拟插件侧已动作）。
static DB_TX_END_MODE: Mutex<u8> = Mutex::new(0);
/// vtable 自报方言：0=postgres；1=mysql；2=未知方言。
static DB_DIALECT_MODE: Mutex<u8> = Mutex::new(0);

/// DB_CALL_MODE 驱动的 query 类返回体。`stage` 进信封 message（宿主 parse_raw 单点解析，
/// 既保留阶段名又结构化为 BridgeError——T11 起插件 Err 载荷恒为信封）。
fn db_call_result(stage: &str, ok: &[u8]) -> FfiFuture {
    match *DB_CALL_MODE.lock().unwrap() {
        1 => ready(Err(format!(
            r#"{{"category":"internal","retryable":false,"session_disposition":"keep","message":"{}: call down","data":{{}}}}"#,
            stage
        ))),
        2 => ready(Ok(b"gibberish".to_vec())),
        _ => ready(Ok(ok.to_vec())),
    }
}

/// 阶段化错误信封（mock 用）：message = "stage: msg"，经 ffi.rs 的 parse_raw 单点解析为 BridgeError。
fn stage_err(stage: &str, msg: &str) -> String {
    format!(
        r#"{{"category":"internal","retryable":false,"session_disposition":"keep","message":"{}: {}","data":{{}}}}"#,
        stage, msg
    )
}

extern "C" fn mock_db_connect(cfg: RString) -> FfiFuture {
    *DB_CONNECTED_CFG.lock().unwrap() = cfg[..].to_string();
    match *DB_CONNECT_MODE.lock().unwrap() {
        1 => ready(Err(stage_err("db connect", "db down"))),
        2 => ready(Ok(b"gibberish".to_vec())),
        3 => ready(Ok(br#"{}"#.to_vec())),
        _ => ready(Ok(br#"{"handle":42}"#.to_vec())),
    }
}
extern "C" fn mock_db_query(handle: u64, sql: RString, params: RString) -> FfiFuture {
    *DB_QUERY.lock().unwrap() = (handle, sql[..].to_string(), params[..].to_string());
    db_call_result("db query", br#"[{"c":1,"t":"a"}]"#)
}
extern "C" fn mock_db_exec(_h: u64, _s: RString, _p: RString) -> FfiFuture {
    db_call_result("db exec", br#"3"#)
}
extern "C" fn mock_db_begin(_handle: u64) -> FfiFuture {
    match *DB_BEGIN_MODE.lock().unwrap() {
        1 => ready(Err(stage_err("db begin", "begin down"))),
        2 => ready(Ok(b"gibberish".to_vec())),
        3 => ready(Ok(br#"{}"#.to_vec())),
        _ => ready(Ok(br#"{"tx_id":7}"#.to_vec())),
    }
}
extern "C" fn mock_db_tx_query(
    handle: u64,
    tx_id: u64,
    sql: RString,
    params: RString,
) -> FfiFuture {
    *DB_TX_QUERY.lock().unwrap() = (handle, tx_id, sql[..].to_string(), params[..].to_string());
    db_call_result("db tx_query", br#"[{"c":9}]"#)
}
extern "C" fn mock_db_tx_exec(_h: u64, _t: u64, _s: RString, _p: RString) -> FfiFuture {
    db_call_result("db tx_exec", br#"1"#)
}
extern "C" fn mock_db_tx_commit(_h: u64, tx_id: u64) -> FfiFuture {
    DB_COMMITTED.store(tx_id, AtomicOrdering::SeqCst);
    if *DB_TX_END_MODE.lock().unwrap() == 1 {
        return ready(Err(stage_err("db tx_commit", "commit down")));
    }
    ready(Ok(b"".to_vec()))
}
extern "C" fn mock_db_tx_rollback(_h: u64, tx_id: u64) -> FfiFuture {
    DB_ROLLED_BACK.store(tx_id, AtomicOrdering::SeqCst);
    if *DB_TX_END_MODE.lock().unwrap() == 1 {
        return ready(Err(stage_err("db tx_rollback", "rollback down")));
    }
    ready(Ok(b"".to_vec()))
}
extern "C" fn mock_db_dialect(_handle: u64) -> RString {
    DB_DIALECT_CALLS.fetch_add(1, AtomicOrdering::SeqCst);
    RString::from(match *DB_DIALECT_MODE.lock().unwrap() {
        1 => "mysql",
        2 => "weirddb",
        _ => "postgres",
    })
}
/// dialect(handle) 调用计数（T5：验证 FfiDataAccessor::new 不再 per-connect 派生方言）。
static DB_DIALECT_CALLS: AtomicU64 = AtomicU64::new(0);
extern "C" fn mock_db_close(handle: u64) {
    DB_CLOSED.store(handle, AtomicOrdering::SeqCst);
}
extern "C" fn mock_db_schemes() -> RVec<RString> {
    let mut v = RVec::new();
    v.push(RString::from("mysql://"));
    v.push(RString::from("mariadb://"));
    v
}
extern "C" fn mock_db_describe() -> RString {
    // 与 mock_db_dialect 默认（postgres）一致；DB_DIALECT_MODE 仅测连接级校验用。
    RString::from(
        r#"{"dialect":"postgres","capabilities":["placeholder_dollar","id_returning","stream","cancel"]}"#,
    )
}
fn mock_dialect_info() -> crate::bridge::dialect_caps::DialectInfo {
    crate::bridge::dialect_caps::caps_from_json(&mock_db_describe()[..]).unwrap()
}

// ---- db 流式 mock（ABI 10）：MODE 0=unsupported（回落 fetch_all）；1=真游标 ----
static DB_STREAM_MODE: Mutex<u8> = Mutex::new(0);
static DB_STREAM_NEXT_CALLS: AtomicU64 = AtomicU64::new(0);
static DB_STREAM_CLOSED: AtomicU64 = AtomicU64::new(0);
static DB_STREAM_CANCELLED: AtomicU64 = AtomicU64::new(0);

extern "C" fn mock_db_stream_open(handle: u64, _sql: RString, _params: RString) -> FfiFuture {
    match *DB_STREAM_MODE.lock().unwrap() {
        0 => ready(Ok(br#"{"unsupported":true}"#.to_vec())),
        _ => ready(Ok(
            format!(r#"{{"stream_id":{}}}"#, handle + 100).into_bytes()
        )),
    }
}
extern "C" fn mock_db_stream_next(handle: u64, stream_id: u64) -> FfiFuture {
    let _ = (handle, stream_id);
    let n = DB_STREAM_NEXT_CALLS.fetch_add(1, AtomicOrdering::SeqCst);
    match n {
        0 => ready(Ok(br#"{"rows":[{"c":1},{"c":2}]}"#.to_vec())),
        _ => ready(Ok(br#"{"done":true}"#.to_vec())),
    }
}
// cancel 行为开关：0 = 返回 unsupported 哨兵；其它 = 正常取消（空对象）。
static DB_CANCEL_MODE: Mutex<u8> = Mutex::new(1);
extern "C" fn mock_db_stream_cancel(_h: u64, _sid: u64) -> FfiFuture {
    DB_STREAM_CANCELLED.fetch_add(1, AtomicOrdering::SeqCst);
    match *DB_CANCEL_MODE.lock().unwrap() {
        0 => ready(Ok(br#"{"unsupported":true}"#.to_vec())),
        // 2 = 真插件形态：取消成功且无载荷（空体）。宿主不得当成解码失败。
        2 => ready(Ok(Vec::new())),
        _ => ready(Ok(b"{}".to_vec())),
    }
}
extern "C" fn mock_db_stream_close(_h: u64, _sid: u64) -> FfiFuture {
    DB_STREAM_CLOSED.fetch_add(1, AtomicOrdering::SeqCst);
    ready(Ok(b"".to_vec()))
}

fn mock_db_vtable() -> &'static DataAccessorVtable {
    Box::leak(Box::new(DataAccessorVtable {
        connect: mock_db_connect,
        query: mock_db_query,
        exec: mock_db_exec,
        begin: mock_db_begin,
        tx_query: mock_db_tx_query,
        tx_exec: mock_db_tx_exec,
        tx_commit: mock_db_tx_commit,
        tx_rollback: mock_db_tx_rollback,
        dialect: mock_db_dialect,
        close: mock_db_close,
        schemes: mock_db_schemes,
        stream_open: mock_db_stream_open,
        stream_next: mock_db_stream_next,
        stream_cancel: mock_db_stream_cancel,
        stream_close: mock_db_stream_close,
        describe: mock_db_describe,
    }))
}

#[tokio::test(flavor = "multi_thread")]
async fn db_backend_schemes_and_connect_dispatch() {
    let _g = T_LOCK.lock().unwrap();
    let be = FfiDbBackend::new("db-mysql", mock_db_vtable(), mock_dialect_info());
    assert_eq!(be.name(), "db-mysql");
    assert_eq!(be.schemes(), vec!["mysql://", "mariadb://"]);
    let da = be
        .connect("mysql://u:p@h/d", std::path::Path::new("/tmp"))
        .await
        .unwrap();
    assert_eq!(*DB_CONNECTED_CFG.lock().unwrap(), "mysql://u:p@h/d");
    assert_eq!(da.dialect(), Dialect::Postgres);
    assert_eq!(
        da.query_with_params("select 1", &[]).await.unwrap()[0]["c"],
        serde_json::json!(1)
    );
}

// T5：FfiDataAccessor::new 不再 per-connect 调 dialect(handle)（方言来自装配期冻结的描述符）。
#[tokio::test(flavor = "multi_thread")]
async fn db_accessor_new_does_not_call_dialect() {
    let _g = T_LOCK.lock().unwrap();
    let before = DB_DIALECT_CALLS.load(AtomicOrdering::SeqCst);
    let _da = FfiDataAccessor::new(42, mock_db_vtable(), mock_dialect_info());
    assert_eq!(
        DB_DIALECT_CALLS.load(AtomicOrdering::SeqCst),
        before,
        "FfiDataAccessor::new must not call dialect(handle); descriptor is factory-cached"
    );
}

// T6：两层模型 —— FfiDataAccessor.dialect_info() 返回装配期注入的描述符（运行期纯查表）。
#[tokio::test(flavor = "multi_thread")]
async fn ffi_data_accessor_dialect_info_returns_injected() {
    let _g = T_LOCK.lock().unwrap();
    let info = crate::bridge::dialect_caps::caps_from_json(
        r#"{"dialect":"mysql","capabilities":["placeholder_q","id_two_step","stream","cancel"]}"#,
    )
    .unwrap();
    let da = FfiDataAccessor::new(42, mock_db_vtable(), info.clone());
    assert_eq!(da.dialect_info().name, "mysql");
    assert_eq!(da.dialect_info().caps, info.caps);
}

// T5：connect 校验 describe().dialect 与运行时 dialect(handle) 一致，不一致 fail-fast 拒连接。
#[tokio::test(flavor = "multi_thread")]
async fn db_connect_rejects_dialect_mismatch() {
    let _g = T_LOCK.lock().unwrap();
    let oracle = crate::bridge::dialect_caps::caps_from_json(
        r#"{"dialect":"oracle","capabilities":["placeholder_colon","id_none"]}"#,
    )
    .unwrap();
    let be = FfiDbBackend::new("db-oracle-mismatch", mock_db_vtable(), oracle);
    let res = be
        .connect("mysql://u:p@h/d", std::path::Path::new("/tmp"))
        .await;
    assert!(
        res.is_err(),
        "dialect mismatch must fail-fast the connection"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn db_query_and_exec_forward_params_and_decode() {
    let _g = T_LOCK.lock().unwrap();
    let da = FfiDataAccessor::new(42, mock_db_vtable(), mock_dialect_info());
    let rows = da
        .query_with_params(
            "select ? as c",
            &[serde_json::json!(1), serde_json::json!("x")],
        )
        .await
        .unwrap();
    assert_eq!(rows[0]["c"], serde_json::json!(1));
    let (h, sql, params) = DB_QUERY.lock().unwrap().clone();
    assert_eq!((h, sql.as_str()), (42, "select ? as c"));
    assert_eq!(params, r#"[1,"x"]"#);
    assert_eq!(da.exec_with_params("delete from t", &[]).await.unwrap(), 3);
}

/// ABI 10 / T10：插件报 `{"unsupported":true}` 哨兵且**未声明** stream → 回落 fetch_all（db.query 全量）。
#[tokio::test(flavor = "multi_thread")]
async fn db_stream_unsupported_sentinel_falls_back_to_fetch_all() {
    let _g = T_LOCK.lock().unwrap();
    *DB_STREAM_MODE.lock().unwrap() = 0;
    DB_STREAM_CLOSED.store(0, AtomicOrdering::SeqCst);
    DB_STREAM_CANCELLED.store(0, AtomicOrdering::SeqCst);
    // 未声明 stream → 标准兜底 fetch_all（保留既有行为，兼容旧插件）。
    let info = crate::bridge::dialect_caps::caps_from_json(
        r#"{"dialect":"postgres","capabilities":["placeholder_dollar","id_returning"]}"#,
    )
    .unwrap();
    let da = FfiDataAccessor::new(42, mock_db_vtable(), info);
    let mut s = da.stream_query("select 1", &[]).await.unwrap();
    let mut rows = Vec::new();
    while let Some(r) = futures::StreamExt::next(&mut s).await {
        rows.push(r.unwrap());
    }
    assert_eq!(rows, vec![serde_json::json!({"c":1,"t":"a"})]);
    // 回落路径不碰游标
    assert_eq!(DB_STREAM_CLOSED.load(AtomicOrdering::SeqCst), 0);
}

/// ABI 10：真游标路径——批量 pull 逐行 yield；走完后 guard 只 close 不 cancel。
#[tokio::test(flavor = "multi_thread")]
async fn db_stream_batches_pull_and_close_on_completion() {
    let _g = T_LOCK.lock().unwrap();
    *DB_STREAM_MODE.lock().unwrap() = 1;
    DB_STREAM_NEXT_CALLS.store(0, AtomicOrdering::SeqCst);
    DB_STREAM_CLOSED.store(0, AtomicOrdering::SeqCst);
    DB_STREAM_CANCELLED.store(0, AtomicOrdering::SeqCst);
    let da = FfiDataAccessor::new(42, mock_db_vtable(), mock_dialect_info());
    let mut s = da.stream_query("select 1", &[]).await.unwrap();
    let mut rows = Vec::new();
    while let Some(r) = futures::StreamExt::next(&mut s).await {
        rows.push(r.unwrap());
    }
    assert_eq!(
        rows,
        vec![serde_json::json!({"c":1}), serde_json::json!({"c":2})]
    );
    // 生成器走完 → guard Drop：只 close（游标已尽，无需 cancel），不泄漏条目
    assert_eq!(DB_STREAM_CLOSED.load(AtomicOrdering::SeqCst), 1);
    assert_eq!(DB_STREAM_CANCELLED.load(AtomicOrdering::SeqCst), 0);
}

/// ABI 10：abort（pump 丢弃生成器）→ guard Drop 先 cancel（真取消）再 close（reclaim）。
#[tokio::test(flavor = "multi_thread")]
async fn db_stream_abort_fires_cancel_then_close() {
    let _g = T_LOCK.lock().unwrap();
    *DB_STREAM_MODE.lock().unwrap() = 1;
    DB_STREAM_NEXT_CALLS.store(0, AtomicOrdering::SeqCst);
    DB_STREAM_CLOSED.store(0, AtomicOrdering::SeqCst);
    DB_STREAM_CANCELLED.store(0, AtomicOrdering::SeqCst);
    let da = FfiDataAccessor::new(42, mock_db_vtable(), mock_dialect_info());
    let s = da.stream_query("select 1", &[]).await.unwrap();
    drop(s); // 中途放弃 = abort 语义
    assert_eq!(DB_STREAM_CANCELLED.load(AtomicOrdering::SeqCst), 1);
    assert_eq!(DB_STREAM_CLOSED.load(AtomicOrdering::SeqCst), 1);
}

// ---- T10：能力一致性矩阵四象限（mock 可切换 caps 与行为）----

fn matrix_info(caps_json: &str) -> DialectInfo {
    crate::bridge::dialect_caps::caps_from_json(caps_json).unwrap()
}

// Q1：声明 stream 且插件返回 unsupported → op 级 unsupported（不回退 fetch_all）。
#[tokio::test(flavor = "multi_thread")]
async fn matrix_stream_declared_but_unsupported_is_op_unsupported() {
    let _g = T_LOCK.lock().unwrap();
    *DB_STREAM_MODE.lock().unwrap() = 0;
    let info = matrix_info(
        r#"{"dialect":"postgres","capabilities":["placeholder_dollar","id_returning","stream"]}"#,
    );
    let da = FfiDataAccessor::new(42, mock_db_vtable(), info);
    let res = da.stream_query("select 1", &[]).await;
    assert!(
        res.is_err(),
        "declared stream + unsupported must be op-level unsupported, not fallback"
    );
    assert!(
        res.err()
            .unwrap()
            .to_string()
            .contains("stream is not supported")
    );
}

// Q2：未声明 stream 且插件返回 unsupported → 标准兜底 fetch_all（保留既有行为）。
#[tokio::test(flavor = "multi_thread")]
async fn matrix_stream_undeclared_and_unsupported_falls_back() {
    let _g = T_LOCK.lock().unwrap();
    *DB_STREAM_MODE.lock().unwrap() = 0;
    DB_STREAM_CLOSED.store(0, AtomicOrdering::SeqCst);
    let info = matrix_info(
        r#"{"dialect":"postgres","capabilities":["placeholder_dollar","id_returning"]}"#,
    );
    let da = FfiDataAccessor::new(42, mock_db_vtable(), info);
    let mut s = da.stream_query("select 1", &[]).await.unwrap();
    let mut rows = Vec::new();
    while let Some(r) = futures::StreamExt::next(&mut s).await {
        rows.push(r.unwrap());
    }
    assert_eq!(rows, vec![serde_json::json!({"c":1,"t":"a"})]);
    assert_eq!(DB_STREAM_CLOSED.load(AtomicOrdering::SeqCst), 0);
}

// 矩阵分支：未声明 stream 却提供了流 → 采信（成功返回行）。
#[tokio::test(flavor = "multi_thread")]
async fn matrix_stream_provided_but_undeclared_is_accepted() {
    let _g = T_LOCK.lock().unwrap();
    *DB_STREAM_MODE.lock().unwrap() = 1;
    DB_STREAM_NEXT_CALLS.store(0, AtomicOrdering::SeqCst);
    DB_STREAM_CLOSED.store(0, AtomicOrdering::SeqCst);
    let info = matrix_info(
        r#"{"dialect":"postgres","capabilities":["placeholder_dollar","id_returning"]}"#,
    );
    let da = FfiDataAccessor::new(42, mock_db_vtable(), info);
    let mut s = da.stream_query("select 1", &[]).await.unwrap();
    let mut rows = Vec::new();
    while let Some(r) = futures::StreamExt::next(&mut s).await {
        rows.push(r.unwrap());
    }
    assert_eq!(
        rows,
        vec![serde_json::json!({"c":1}), serde_json::json!({"c":2})]
    );
}

// Q3：未声明 cancel 且插件实现了 cancel（可用）→ 成功（warn 由 ffi.rs 侧发出）。
#[tokio::test(flavor = "multi_thread")]
async fn matrix_cancel_undeclared_but_provided_succeeds() {
    let _g = T_LOCK.lock().unwrap();
    *DB_CANCEL_MODE.lock().unwrap() = 1;
    let info = matrix_info(
        r#"{"dialect":"postgres","capabilities":["placeholder_dollar","id_returning"]}"#,
    );
    let da = FfiDataAccessor::new(42, mock_db_vtable(), info);
    let res = da.cancel(0).await;
    assert!(
        res.is_ok(),
        "undeclared cancel but provided must succeed: {res:?}"
    );
}

// Q4 续：声明 cancel **且** 插件真提供取消、回执为空体（mysql/pg 插件形态）→ 成功。
#[tokio::test(flavor = "current_thread")]
async fn matrix_cancel_declared_and_provided_with_empty_body_succeeds() {
    let _g = T_LOCK.lock().unwrap();
    *DB_CANCEL_MODE.lock().unwrap() = 2;
    let info = matrix_info(
        r#"{"dialect":"postgres","capabilities":["placeholder_dollar","id_returning","cancel"]}"#,
    );
    let da = FfiDataAccessor::new(42, mock_db_vtable(), info);
    let res = da.cancel(0).await;
    assert!(
        res.is_ok(),
        "declared+provided cancel with empty body must succeed: {res:?}"
    );
    *DB_CANCEL_MODE.lock().unwrap() = 0;
}

// Q4：声明 cancel 且 accessor 为默认实现 → unsupported 错误（default-throw 哨兵）。
struct MatrixMockDA {
    info: DialectInfo,
}
#[async_trait]
impl DataAccessor for MatrixMockDA {
    fn dialect_info(&self) -> &DialectInfo {
        &self.info
    }
    async fn query_with_params(
        &self,
        _s: &str,
        _p: &[serde_json::Value],
    ) -> BridgeResult<Vec<Row>> {
        Ok(vec![])
    }
    async fn exec_with_params(&self, _s: &str, _p: &[serde_json::Value]) -> BridgeResult<i64> {
        Ok(0)
    }
}
#[tokio::test(flavor = "multi_thread")]
async fn matrix_cancel_declared_but_default_impl_is_unsupported() {
    let _g = T_LOCK.lock().unwrap();
    let info = matrix_info(
        r#"{"dialect":"oracle","capabilities":["placeholder_colon","id_none","cancel"]}"#,
    );
    let da = MatrixMockDA { info };
    let res = da.cancel(0).await;
    assert!(
        res.is_err(),
        "declared cancel + default impl must be unsupported"
    );
    assert!(
        res.err()
            .unwrap()
            .to_string()
            .contains("cancel is not supported")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn db_tx_commit_and_drop_does_not_rollback() {
    let _g = T_LOCK.lock().unwrap();
    DB_COMMITTED.store(0, AtomicOrdering::SeqCst);
    DB_ROLLED_BACK.store(0, AtomicOrdering::SeqCst);
    let da = FfiDataAccessor::new(42, mock_db_vtable(), mock_dialect_info());
    let tx = da.begin().await.unwrap();
    let rows = tx.query("select 1", &[]).await.unwrap();
    assert_eq!(rows[0]["c"], serde_json::json!(9));
    let (h, tid, sql, _) = DB_TX_QUERY.lock().unwrap().clone();
    assert_eq!((h, tid, sql.as_str()), (42, 7, "select 1"));
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(DB_COMMITTED.load(AtomicOrdering::SeqCst), 7);
    assert_eq!(DB_ROLLED_BACK.load(AtomicOrdering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn db_tx_drop_without_finish_fires_rollback() {
    let _g = T_LOCK.lock().unwrap();
    DB_ROLLED_BACK.store(0, AtomicOrdering::SeqCst);
    let da = FfiDataAccessor::new(42, mock_db_vtable(), mock_dialect_info());
    let tx = da.begin().await.unwrap();
    drop(tx); // 未 commit/rollback → drop 保底回滚
    assert_eq!(DB_ROLLED_BACK.load(AtomicOrdering::SeqCst), 7);
}

#[test]
fn db_accessor_drop_closes_handle() {
    let _g = T_LOCK.lock().unwrap();
    DB_CLOSED.store(0, AtomicOrdering::SeqCst);
    drop(FfiDataAccessor::new(
        42,
        mock_db_vtable(),
        mock_dialect_info(),
    ));
    assert_eq!(DB_CLOSED.load(AtomicOrdering::SeqCst), 42);
}

// ---- blob 轴 mock vtable（Task 4.2；五方法转发 + Drop close）----

use crate::bridge::{BlobBackend, BlobServed};

static BLOB_PUT: Mutex<(u64, String, Vec<u8>, String)> =
    Mutex::new((0, String::new(), Vec::new(), String::new()));
static BLOB_GET: Mutex<(u64, String)> = Mutex::new((0, String::new()));
static BLOB_DEL: Mutex<(u64, String)> = Mutex::new((0, String::new()));
static BLOB_URL: Mutex<(u64, String)> = Mutex::new((0, String::new()));
static BLOB_CT: Mutex<(u64, String)> = Mutex::new((0, String::new()));
static BLOB_CLOSED: AtomicU64 = AtomicU64::new(0);
static BLOB_CT_EMPTY: AtomicBool = AtomicBool::new(false);
/// blob 行为开关：1=报错；2=url/content_type 返回坏 UTF-8 字节。
static BLOB_MODE: Mutex<u8> = Mutex::new(0);

extern "C" fn mock_blob_connect(_name: RString, _cfg: RString) -> FfiFuture {
    ready(Ok(br#"{"handle":42}"#.to_vec()))
}
extern "C" fn mock_blob_put(handle: u64, key: RString, bytes: RBytes, ct: RString) -> FfiFuture {
    let mut b = Vec::with_capacity(bytes.len());
    for x in &bytes {
        b.push(*x);
    }
    *BLOB_PUT.lock().unwrap() = (handle, key[..].to_string(), b, ct[..].to_string());
    if *BLOB_MODE.lock().unwrap() == 1 {
        return ready(Err("put down".into()));
    }
    ready(Ok(b"".to_vec()))
}
extern "C" fn mock_blob_get(handle: u64, key: RString) -> FfiFuture {
    *BLOB_GET.lock().unwrap() = (handle, key[..].to_string());
    if *BLOB_MODE.lock().unwrap() == 1 {
        return ready(Err("get down".into()));
    }
    ready(Ok(b"blobdata".to_vec()))
}
extern "C" fn mock_blob_del(handle: u64, key: RString) -> FfiFuture {
    *BLOB_DEL.lock().unwrap() = (handle, key[..].to_string());
    if *BLOB_MODE.lock().unwrap() == 1 {
        return ready(Err("del down".into()));
    }
    ready(Ok(b"".to_vec()))
}
extern "C" fn mock_blob_url(handle: u64, key: RString) -> FfiFuture {
    *BLOB_URL.lock().unwrap() = (handle, key[..].to_string());
    match *BLOB_MODE.lock().unwrap() {
        1 => ready(Err("url down".into())),
        2 => ready(Ok(vec![0xff, 0xfe])),
        _ => ready(Ok(b"https://b.s3/presign".to_vec())),
    }
}
extern "C" fn mock_blob_content_type(handle: u64, key: RString) -> FfiFuture {
    *BLOB_CT.lock().unwrap() = (handle, key[..].to_string());
    match *BLOB_MODE.lock().unwrap() {
        1 => ready(Err("ct down".into())),
        2 => ready(Ok(vec![0xff, 0xfe])),
        _ if BLOB_CT_EMPTY.swap(false, AtomicOrdering::SeqCst) => ready(Ok(b"".to_vec())),
        _ => ready(Ok(b"image/png".to_vec())),
    }
}
extern "C" fn mock_blob_close(handle: u64) {
    BLOB_CLOSED.store(handle, AtomicOrdering::SeqCst);
}

extern "C" fn mock_blob_upload_url(_h: u64, key: RString, op: RString) -> FfiFuture {
    let k: &str = &key;
    let o: &str = &op;
    if k == "a/b.png" && o.contains("put") {
        return ready_ok(br#"{"url":"https://s3.example.com/up"}"#.to_vec());
    }
    ready_err("stub")
}

// ---- blob 流式 mock（ABI 10）----
static BLOB_STREAM_OPENED: Mutex<(u64, String, String)> =
    Mutex::new((0, String::new(), String::new()));
static BLOB_STREAM_CHUNK: Mutex<(u64, Vec<u8>)> = Mutex::new((0, Vec::new()));
static BLOB_STREAM_FINISHED: AtomicU64 = AtomicU64::new(0);
static BLOB_STREAM_ABORTED: AtomicU64 = AtomicU64::new(0);

extern "C" fn mock_blob_put_stream_open(handle: u64, key: RString, ct: RString) -> FfiFuture {
    *BLOB_STREAM_OPENED.lock().unwrap() = (handle, key[..].to_string(), ct[..].to_string());
    ready(Ok(br#"{"upload_id":77}"#.to_vec()))
}
extern "C" fn mock_blob_put_stream_chunk(_h: u64, id: u64, bytes: RBytes) -> FfiFuture {
    let mut b = Vec::with_capacity(bytes.len());
    for x in &bytes {
        b.push(*x);
    }
    *BLOB_STREAM_CHUNK.lock().unwrap() = (id, b);
    ready(Ok(b"".to_vec()))
}
extern "C" fn mock_blob_put_stream_finish(_h: u64, id: u64) -> FfiFuture {
    BLOB_STREAM_FINISHED.fetch_add(1, AtomicOrdering::SeqCst);
    let _ = id;
    ready(Ok(b"".to_vec()))
}
extern "C" fn mock_blob_put_stream_abort(_h: u64, id: u64) -> FfiFuture {
    BLOB_STREAM_ABORTED.fetch_add(1, AtomicOrdering::SeqCst);
    let _ = id;
    ready(Ok(b"".to_vec()))
}

// ---- blob 搬运 / 区间读 mock（ABI 11；mode 3 = 三槽「不支持」→ 触发宿主回落）----
static BLOB_COPY: Mutex<(u64, String, String)> = Mutex::new((0, String::new(), String::new()));
static BLOB_MOVE: Mutex<(u64, String, String)> = Mutex::new((0, String::new(), String::new()));
static BLOB_RANGE: Mutex<(u64, String, u64, u64)> = Mutex::new((0, String::new(), 0, 0));

extern "C" fn mock_blob_copy(handle: u64, src: RString, dst: RString) -> FfiFuture {
    *BLOB_COPY.lock().unwrap() = (handle, src[..].to_string(), dst[..].to_string());
    let m = *BLOB_MODE.lock().unwrap();
    if m == 1 || m == 3 {
        return ready(Err("copy unsupported".into()));
    }
    ready(Ok(b"".to_vec()))
}
extern "C" fn mock_blob_move_to(handle: u64, src: RString, dst: RString) -> FfiFuture {
    *BLOB_MOVE.lock().unwrap() = (handle, src[..].to_string(), dst[..].to_string());
    let m = *BLOB_MODE.lock().unwrap();
    if m == 1 || m == 3 {
        return ready(Err("move unsupported".into()));
    }
    ready(Ok(b"".to_vec()))
}
extern "C" fn mock_blob_read_range(handle: u64, key: RString, offset: u64, len: u64) -> FfiFuture {
    *BLOB_RANGE.lock().unwrap() = (handle, key[..].to_string(), offset, len);
    let m = *BLOB_MODE.lock().unwrap();
    if m == 1 || m == 3 {
        return ready(Err("readRange unsupported".into()));
    }
    // 短读截断（契约语义）：mock 体为 8 字节 blobdata。
    let all = b"blobdata";
    let size = all.len() as u64;
    let s = offset.min(size) as usize;
    let e = offset.saturating_add(len).min(size) as usize;
    ready(Ok(all[s..e].to_vec()))
}

fn mock_blob_vtable() -> &'static BlobBackendVtable {
    Box::leak(Box::new(BlobBackendVtable {
        connect: mock_blob_connect,
        put: mock_blob_put,
        get: mock_blob_get,
        del: mock_blob_del,
        url: mock_blob_url,
        upload_url: mock_blob_upload_url,
        content_type: mock_blob_content_type,
        close: mock_blob_close,
        put_stream_open: mock_blob_put_stream_open,
        put_stream_chunk: mock_blob_put_stream_chunk,
        put_stream_finish: mock_blob_put_stream_finish,
        put_stream_abort: mock_blob_put_stream_abort,
        copy: mock_blob_copy,
        move_to: mock_blob_move_to,
        read_range: mock_blob_read_range,
    }))
}

#[tokio::test(flavor = "multi_thread")]
async fn blob_put_forwards_key_bytes_and_ct() {
    let _g = T_LOCK.lock().unwrap();
    let b = FfiBlobBackend::new(42, mock_blob_vtable());
    b.put("a/b.png", b"hello", Some("image/png")).await.unwrap();
    let (h, key, bytes, ct) = BLOB_PUT.lock().unwrap().clone();
    assert_eq!(
        (h, key.as_str(), bytes.as_slice()),
        (42, "a/b.png", &b"hello"[..])
    );
    assert_eq!(ct, "image/png");
    // None ct → 空串过线
    b.put("x", b"y", None).await.unwrap();
    let (_, _, _, ct) = BLOB_PUT.lock().unwrap().clone();
    assert_eq!(ct, "");
}

#[tokio::test(flavor = "multi_thread")]
async fn blob_get_returns_bytes() {
    let _g = T_LOCK.lock().unwrap();
    let b = FfiBlobBackend::new(42, mock_blob_vtable());
    assert_eq!(b.get("k").await.unwrap(), b"blobdata");
    let (h, key) = BLOB_GET.lock().unwrap().clone();
    assert_eq!((h, key.as_str()), (42, "k"));
}

/// ABI 10：put_stream_* 四槽转发（open 产 upload_id、chunk 字节、finish/abort 计数）。
#[tokio::test(flavor = "multi_thread")]
async fn blob_put_stream_forwards_all_slots() {
    let _g = T_LOCK.lock().unwrap();
    BLOB_STREAM_FINISHED.store(0, AtomicOrdering::SeqCst);
    BLOB_STREAM_ABORTED.store(0, AtomicOrdering::SeqCst);
    let b = FfiBlobBackend::new(42, mock_blob_vtable());
    let id = b
        .put_stream_open("a/big.bin", Some("video/mp4"))
        .await
        .unwrap();
    assert_eq!(id, 77);
    let (h, key, ct) = BLOB_STREAM_OPENED.lock().unwrap().clone();
    assert_eq!(
        (h, key.as_str(), ct.as_str()),
        (42, "a/big.bin", "video/mp4")
    );
    b.put_stream_chunk(id, b"chunk-bytes").await.unwrap();
    let (cid, bytes) = BLOB_STREAM_CHUNK.lock().unwrap().clone();
    assert_eq!((cid, bytes.as_slice()), (77, &b"chunk-bytes"[..]));
    b.put_stream_finish(id).await.unwrap();
    assert_eq!(BLOB_STREAM_FINISHED.load(AtomicOrdering::SeqCst), 1);
    b.put_stream_abort(id).await.unwrap();
    assert_eq!(BLOB_STREAM_ABORTED.load(AtomicOrdering::SeqCst), 1);
}

/// ABI 9：upload_url 转发（op JSON 透传、响应 JSON 原样解析）。
#[tokio::test(flavor = "multi_thread")]
async fn blob_upload_url_forwards_key_and_op() {
    let _g = T_LOCK.lock().unwrap();
    let b = FfiBlobBackend::new(42, mock_blob_vtable());
    let v = b.upload_url("a/b.png", r#"{"kind":"put"}"#).await.unwrap();
    assert_eq!(v["url"], "https://s3.example.com/up");
    // mock 的 miss 臂 → Err 透传
    assert!(b.upload_url("other", r#"{"kind":"put"}"#).await.is_err());
    assert!(
        b.upload_url("a/b.png", r#"{"kind":"multipart_initiate"}"#)
            .await
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn blob_del_succeeds() {
    let _g = T_LOCK.lock().unwrap();
    let b = FfiBlobBackend::new(42, mock_blob_vtable());
    b.del("k").await.unwrap();
    let (h, key) = BLOB_DEL.lock().unwrap().clone();
    assert_eq!((h, key.as_str()), (42, "k"));
}

#[tokio::test(flavor = "multi_thread")]
async fn blob_url_and_content_type_forward() {
    let _g = T_LOCK.lock().unwrap();
    let b = FfiBlobBackend::new(42, mock_blob_vtable());
    assert_eq!(b.url("k").await.unwrap(), "https://b.s3/presign");
    let (h, key) = BLOB_URL.lock().unwrap().clone();
    assert_eq!((h, key.as_str()), (42, "k"));
    assert_eq!(
        b.content_type("k").await.unwrap(),
        Some("image/png".to_string())
    );
    // 空串 → None
    BLOB_CT_EMPTY.store(true, AtomicOrdering::SeqCst);
    assert_eq!(b.content_type("k2").await.unwrap(), None);
}

/// ABI 11：copy / move_to / read_range 三槽转发（src+dst 与 offset+len 原样过线）。
#[tokio::test(flavor = "multi_thread")]
async fn blob_copy_move_read_range_forward_all_slots() {
    let _g = T_LOCK.lock().unwrap();
    *BLOB_MODE.lock().unwrap() = 0;
    let b = FfiBlobBackend::new(42, mock_blob_vtable());
    b.copy("tmp/a.bin", "final/a.bin").await.unwrap();
    let (h, src, dst) = BLOB_COPY.lock().unwrap().clone();
    assert_eq!(
        (h, src.as_str(), dst.as_str()),
        (42, "tmp/a.bin", "final/a.bin")
    );
    b.move_to("tmp/b.bin", "final/b.bin").await.unwrap();
    let (h, src, dst) = BLOB_MOVE.lock().unwrap().clone();
    assert_eq!(
        (h, src.as_str(), dst.as_str()),
        (42, "tmp/b.bin", "final/b.bin")
    );
    assert_eq!(b.read_range("k", 2, 3).await.unwrap(), b"obd");
    let (h, key, off, len) = BLOB_RANGE.lock().unwrap().clone();
    assert_eq!((h, key.as_str(), off, len), (42, "k", 2, 3));
    // 短读截断：offset 过尾 → 空
    assert_eq!(b.read_range("k", 99, 4).await.unwrap(), b"");
}

/// ABI 11 能力保险：后端无服务端复制 → 回落 `get` + `put`（dst 拿到 src 的字节与 ct）；
/// move 回落 = copy + del；readRange 回落 = get 全量再切片。
#[tokio::test(flavor = "multi_thread")]
async fn blob_copy_move_read_range_fall_back_when_backend_lacks_them() {
    let _g = T_LOCK.lock().unwrap();
    BLOB_CT_EMPTY.store(false, AtomicOrdering::SeqCst);
    *BLOB_MODE.lock().unwrap() = 3;
    let b = FfiBlobBackend::new(42, mock_blob_vtable());
    b.copy("s", "d").await.unwrap();
    assert_eq!(BLOB_GET.lock().unwrap().1, "s");
    let (_, dkey, bytes, ct) = BLOB_PUT.lock().unwrap().clone();
    assert_eq!(
        (dkey.as_str(), bytes.as_slice(), ct.as_str()),
        ("d", &b"blobdata"[..], "image/png")
    );
    b.move_to("s", "d").await.unwrap();
    assert_eq!(BLOB_DEL.lock().unwrap().1, "s");
    assert_eq!(b.read_range("k", 2, 3).await.unwrap(), b"obd");
    *BLOB_MODE.lock().unwrap() = 0;
}

/// 后端与回落双双失败 → 抛 **后端原错误**（回落的错误不盖住根因）。
#[tokio::test(flavor = "multi_thread")]
async fn blob_copy_errors_with_backend_reason_when_fallback_also_fails() {
    let _g = T_LOCK.lock().unwrap();
    *BLOB_MODE.lock().unwrap() = 1;
    let b = FfiBlobBackend::new(42, mock_blob_vtable());
    let e = b
        .copy("s", "d")
        .await
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(e.contains("copy"), "{e}");
    let e = b
        .read_range("k", 0, 4)
        .await
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(e.contains("readRange"), "{e}");
    *BLOB_MODE.lock().unwrap() = 0;
}

#[tokio::test(flavor = "multi_thread")]
async fn blob_serve_redirects_to_url() {
    let _g = T_LOCK.lock().unwrap();
    let b = FfiBlobBackend::new(42, mock_blob_vtable());
    assert!(
        matches!(b.serve("k").await.unwrap(), BlobServed::Redirect(u) if u == "https://b.s3/presign")
    );
}

#[test]
fn blob_drop_calls_close() {
    let _g = T_LOCK.lock().unwrap();
    BLOB_CLOSED.store(0, AtomicOrdering::SeqCst);
    drop(FfiBlobBackend::new(42, mock_blob_vtable()));
    assert_eq!(BLOB_CLOSED.load(AtomicOrdering::SeqCst), 42);
}

// ---- bus 轴 mock vtable（Task 4.3；publish 转发 + deliver 扇出 + Drop close）----

use crate::bridge::{BusBackend, EventBroker};
use crate::config::BrokerCfg;

static BUS_CONNECTED_CFG: Mutex<String> = Mutex::new(String::new());
static BUS_PUBLISHED: Mutex<(u64, String, Vec<u8>)> = Mutex::new((0, String::new(), Vec::new()));
static BUS_SUBSCRIBES: Mutex<Vec<(u64, String)>> = Mutex::new(Vec::new());
static BUS_CLOSED: AtomicU64 = AtomicU64::new(0);
/// TDD 开关：置位时 mock_bus_subscribe 先记录（模拟消费循环已起）再返回 Err，
/// 用于验证 I-1 失败回滚（无僵尸注册）。
static BUS_SUBSCRIBE_FAIL: AtomicBool = AtomicBool::new(false);
/// bus connect 行为开关：1=报错；2=非 JSON；3=缺 handle。
static BUS_CONNECT_MODE: Mutex<u8> = Mutex::new(0);
/// TDD 开关：置位时 publish 报错（插件侧投递失败透传）。
static BUS_PUBLISH_FAIL: AtomicBool = AtomicBool::new(false);

extern "C" fn mock_bus_connect(cfg: RString) -> FfiFuture {
    *BUS_CONNECTED_CFG.lock().unwrap() = cfg[..].to_string();
    match *BUS_CONNECT_MODE.lock().unwrap() {
        1 => ready(Err("bus down".into())),
        2 => ready(Ok(b"gibberish".to_vec())),
        3 => ready(Ok(br#"{}"#.to_vec())),
        _ => ready(Ok(br#"{"handle":42}"#.to_vec())),
    }
}
extern "C" fn mock_bus_publish(handle: u64, topic: RString, data: RBytes) -> FfiFuture {
    *BUS_PUBLISHED.lock().unwrap() = (handle, topic[..].to_string(), data[..].to_vec());
    if BUS_PUBLISH_FAIL.swap(false, AtomicOrdering::SeqCst) {
        return ready(Err("publish down".into()));
    }
    ready(Ok(b"".to_vec()))
}
extern "C" fn mock_bus_subscribe(handle: u64, topic: RString) -> FfiFuture {
    // 先记录（模拟插件侧已起消费循环），再按开关返回失败。
    BUS_SUBSCRIBES
        .lock()
        .unwrap()
        .push((handle, topic[..].to_string()));
    if BUS_SUBSCRIBE_FAIL.swap(false, AtomicOrdering::SeqCst) {
        return ready(Err("injected subscribe failure".into()));
    }
    ready(Ok(b"".to_vec()))
}
extern "C" fn mock_bus_close(handle: u64) {
    BUS_CLOSED.store(handle, AtomicOrdering::SeqCst);
}

fn mock_bus_vtable() -> &'static EventBrokerVtable {
    Box::leak(Box::new(EventBrokerVtable {
        connect: mock_bus_connect,
        publish: mock_bus_publish,
        subscribe: mock_bus_subscribe,
        close: mock_bus_close,
    }))
}

fn deliver_clear() {
    // 经自愈入口：毒化用例跑过后（std Mutex 毒化标记不可消除）其余用例照常清场。
    deliver_targets().clear();
}

/// 锁毒化自愈（评审 B2）：持锁 panic 毒化 DELIVER_TARGETS 后，deliver 路径
/// （fanout_targets → deliver_targets）恢复内层数据继续服务，不再连环 panic → abort。
#[test]
fn deliver_targets_poisoned_lock_does_not_kill_deliver() {
    let _g = T_LOCK.lock().unwrap();
    deliver_clear();
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = DELIVER_TARGETS.lock().unwrap();
        panic!("poison the deliver lock");
    }));
    // 毒化后的锁照常可用：注册 + 扇出 + 惰性清理全链路不 panic。
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    deliver_targets()
        .entry("poison-t".to_string())
        .or_default()
        .push(tx);
    assert_eq!(fanout_targets("poison-t", WsSend::Text("x".into())), 1);
    assert_eq!(rx.try_recv().unwrap(), WsSend::Text("x".into()));
    // 经自愈入口清场：毒化标记随 into_inner 消除，不把毒化带给同批用例。
    deliver_targets().clear();
}

#[tokio::test(flavor = "multi_thread")]
async fn bus_backend_kind_and_connect() {
    let _g = T_LOCK.lock().unwrap();
    let be = FfiBusBackend::new("bus-kafka", mock_bus_vtable());
    assert_eq!(be.kind(), "kafka");
    let cfg = BrokerCfg {
        kind: "kafka".into(),
        brokers: vec!["b1:9092".into()],
        ..Default::default()
    };
    let broker = be.connect(&cfg).await.unwrap();
    assert_eq!(broker.kind(), "kafka");
    // 插件收到的 cfg JSON = BrokerCfg 序列化（brokers 数组）。
    let c = BUS_CONNECTED_CFG.lock().unwrap().clone();
    assert!(c.contains("b1:9092") && c.contains("kind"), "{c}");
    drop(broker);
    assert_eq!(BUS_CLOSED.load(AtomicOrdering::SeqCst), 42);
}

#[tokio::test(flavor = "multi_thread")]
async fn bus_publish_forwards_topic_and_frame_returns_zero() {
    let _g = T_LOCK.lock().unwrap();
    let broker = FfiEventBroker::new("kafka", 42, mock_bus_vtable());
    let n = broker
        .publish("news", &BusPayload::Json(serde_json::json!({"a": 1})))
        .await
        .unwrap();
    assert_eq!(n, 0); // 远程 broker 本地 fan-out 恒 0
    let (h, topic, data) = BUS_PUBLISHED.lock().unwrap().clone();
    assert_eq!(h, 42);
    assert_eq!(topic, "news");
    // JSON 载荷 → wire = {"topic","data"} 信封 UTF-8。
    let v: serde_json::Value = serde_json::from_slice(&data).unwrap();
    assert_eq!(v["data"]["a"], 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn bus_subscribe_registers_tx_and_deliver_fans_out() {
    let _g = T_LOCK.lock().unwrap();
    deliver_clear();
    BUS_SUBSCRIBES.lock().unwrap().clear();
    let broker = FfiEventBroker::new("kafka", 42, mock_bus_vtable());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    broker.subscribe("t", tx).await.unwrap();
    // vtable.subscribe 只起一次（每 topic 至多一个消费循环）。
    assert_eq!(BUS_SUBSCRIBES.lock().unwrap().len(), 1);
    // 模拟插件消费循环经 host.deliver 上送 → 扇出到本地 tx。
    host_deliver(
        RString::from("t"),
        to_rbytes(br#"{"topic":"t","data":{"v":42}}"#),
    );
    let frame = rx.try_recv().unwrap();
    let WsSend::Text(env) = frame else {
        panic!("envelope payload must deliver as text frame");
    };
    let v: serde_json::Value = serde_json::from_str(&env).unwrap();
    assert_eq!(v["data"]["v"], 42);
    // 关闭接收端 → 后续 deliver 惰性清理（不 panic）。
    drop(rx);
    host_deliver(RString::from("t"), to_rbytes(br#"{}"#));
}

/// 非 UTF-8 载荷（不可为信封）→ Binary 帧原字节透传（v0.1.16 bus 字节化）。
#[tokio::test(flavor = "multi_thread")]
async fn bus_deliver_non_utf8_payload_arrives_as_binary_frame() {
    let _g = T_LOCK.lock().unwrap();
    deliver_clear();
    let broker = FfiEventBroker::new("kafka", 42, mock_bus_vtable());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    broker.subscribe("t", tx).await.unwrap();
    let raw = vec![0x00, 0xA9, 0xFF, 0xFE]; // 非法 UTF-8
    host_deliver(RString::from("t"), to_rbytes(&raw));
    assert_eq!(rx.try_recv().unwrap(), WsSend::Binary(raw));
}

#[tokio::test(flavor = "multi_thread")]
async fn bus_subscribe_dedupes_same_channel_and_single_consumer_per_topic() {
    let _g = T_LOCK.lock().unwrap();
    deliver_clear();
    BUS_SUBSCRIBES.lock().unwrap().clear();
    let broker = FfiEventBroker::new("kafka", 42, mock_bus_vtable());
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    broker.subscribe("t", tx.clone()).await.unwrap();
    broker.subscribe("t", tx).await.unwrap(); // 同 channel 去重
    let (tx2, _rx2) = tokio::sync::mpsc::unbounded_channel();
    broker.subscribe("u", tx2).await.unwrap(); // 不同 topic → 新消费
    assert_eq!(BUS_SUBSCRIBES.lock().unwrap().len(), 2); // t 一次 + u 一次
}

/// I-1 TDD：首次订阅 vtable 失败 → 返回 Err 且无僵尸注册（host_deliver 后通道空）。
#[tokio::test(flavor = "multi_thread")]
async fn bus_subscribe_failure_rolls_back_no_zombie() {
    let _g = T_LOCK.lock().unwrap();
    deliver_clear();
    BUS_SUBSCRIBES.lock().unwrap().clear();
    BUS_SUBSCRIBE_FAIL.store(false, AtomicOrdering::SeqCst);
    let broker = FfiEventBroker::new("kafka", 42, mock_bus_vtable());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    BUS_SUBSCRIBE_FAIL.store(true, AtomicOrdering::SeqCst); // 注入失败
    let res = broker.subscribe("t", tx).await;
    assert!(res.is_err(), "subscribe must propagate vtable error");
    // 无僵尸：回滚后该 topic 不应仍注册，host_deliver 不应扇出到通道。
    host_deliver(RString::from("t"), to_rbytes(br#"{"x":1}"#));
    assert!(
        rx.try_recv().is_err(),
        "zombie subscription must not deliver"
    );
    let g = deliver_targets();
    let empty = match g.get("t") {
        None => true,
        Some(list) => list.is_empty(),
    };
    assert!(empty, "rolled-back topic must leave no deliver target");
}

/// I-1 TDD：失败重试成功 → 消费循环被重新起（BUS_SUBSCRIBES 记录两次）+ 扇出可达。
#[tokio::test(flavor = "multi_thread")]
async fn bus_subscribe_retries_after_failure_and_fanout() {
    let _g = T_LOCK.lock().unwrap();
    deliver_clear();
    BUS_SUBSCRIBES.lock().unwrap().clear();
    BUS_SUBSCRIBE_FAIL.store(false, AtomicOrdering::SeqCst);
    let broker = FfiEventBroker::new("kafka", 42, mock_bus_vtable());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    // 首次失败（回滚本地注册，但插件侧消费循环已起 → 记一次）。
    BUS_SUBSCRIBE_FAIL.store(true, AtomicOrdering::SeqCst);
    assert!(broker.subscribe("t", tx.clone()).await.is_err());
    // 重试成功。
    BUS_SUBSCRIBE_FAIL.store(false, AtomicOrdering::SeqCst);
    assert!(broker.subscribe("t", tx).await.is_ok());
    // 消费循环被重新起：两次 subscribe 各记一次。
    assert_eq!(BUS_SUBSCRIBES.lock().unwrap().len(), 2);
    // 扇出可达：host.deliver 经 DELIVER_TARGETS 扇到本通道。
    host_deliver(RString::from("t"), to_rbytes(br#"{"x":1}"#));
    let frame = rx.recv().await.unwrap();
    // {"x":1} 非信封 → 原字节按 Binary 帧透传（启发式约定）。
    assert_eq!(frame, WsSend::Binary(br#"{"x":1}"#.to_vec()));
}

#[test]
fn bus_drop_closes_handle() {
    let _g = T_LOCK.lock().unwrap();
    BUS_CLOSED.store(0, AtomicOrdering::SeqCst);
    drop(FfiEventBroker::new("kafka", 42, mock_bus_vtable()));
    assert_eq!(BUS_CLOSED.load(AtomicOrdering::SeqCst), 42);
}

/// FFI broker 下"同一实例跨 actor/WS 共享"回归（Task 0.5 的插件 broker 形态）：
/// 两个 Bridge 注入同一 FfiEventBroker，A 侧订阅、B 侧发布；远端经 deliver 回调
/// 把消息扇回 → A 侧 tx 收到（全局 DELIVER_TARGETS 保持跨实例共享语义）。
#[tokio::test(flavor = "multi_thread")]
async fn ffi_broker_shared_across_bridges() {
    let _g = T_LOCK.lock().unwrap();
    deliver_clear();
    BUS_SUBSCRIBES.lock().unwrap().clear();
    let broker = Arc::new(FfiEventBroker::new("kafka", 42, mock_bus_vtable()));
    // A 侧订阅（如 WS 连接 A）。
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    broker.subscribe("t", tx).await.unwrap();
    // B 侧发布（同一实例）→ vtable.publish 转发（记录）。
    broker
        .publish("t", &BusPayload::Json(serde_json::json!({"v": 7})))
        .await
        .unwrap();
    let (h, topic, _) = BUS_PUBLISHED.lock().unwrap().clone();
    assert_eq!((h, topic.as_str()), (42, "t"));
    // 模拟远端回程：插件消费循环经 host.deliver 上送 → A 侧 tx 收到（跨实例仍成立）。
    host_deliver(
        RString::from("t"),
        to_rbytes(br#"{"topic":"t","data":{"v":7}}"#),
    );
    let WsSend::Text(env) = rx.recv().await.unwrap() else {
        panic!("envelope payload must deliver as text frame");
    };
    let v: serde_json::Value = serde_json::from_str(&env).unwrap();
    assert_eq!(v["data"]["v"], 7, "{v}");
}

// ---- kv 轴 mock vtable（Task 4.4；五方法转发 + expire 秒取整 + Drop close）----

use crate::bridge::KVStore;

static KV_GET: Mutex<(u64, String)> = Mutex::new((0, String::new()));
static KV_SET: Mutex<(u64, String, String)> = Mutex::new((0, String::new(), String::new()));
static KV_DEL: Mutex<(u64, String)> = Mutex::new((0, String::new()));
static KV_EXPIRE: Mutex<(u64, String, u64)> = Mutex::new((0, String::new(), 0));
static KV_INCR: Mutex<(u64, String)> = Mutex::new((0, String::new()));
static KV_CLOSED: AtomicU64 = AtomicU64::new(0);
/// kv 行为开关：1=报错；2=get/expire/incr 返回非 JSON。
static KV_MODE: Mutex<u8> = Mutex::new(0);

extern "C" fn mock_kv_connect(_cfg: RString) -> FfiFuture {
    ready(Ok(br#"{"handle":42}"#.to_vec()))
}
extern "C" fn mock_kv_get(handle: u64, key: RString) -> FfiFuture {
    *KV_GET.lock().unwrap() = (handle, key[..].to_string());
    match *KV_MODE.lock().unwrap() {
        1 => ready(Err("get down".into())),
        2 => ready(Ok(b"{nope".to_vec())),
        _ => ready(Ok(br#""blobdata""#.to_vec())), // JSON Option<String>
    }
}
extern "C" fn mock_kv_set(handle: u64, key: RString, value: RString) -> FfiFuture {
    *KV_SET.lock().unwrap() = (handle, key[..].to_string(), value[..].to_string());
    if *KV_MODE.lock().unwrap() == 1 {
        return ready(Err("set down".into()));
    }
    ready(Ok(b"".to_vec()))
}
extern "C" fn mock_kv_del(handle: u64, key: RString) -> FfiFuture {
    *KV_DEL.lock().unwrap() = (handle, key[..].to_string());
    if *KV_MODE.lock().unwrap() == 1 {
        return ready(Err("del down".into()));
    }
    ready(Ok(b"".to_vec()))
}
extern "C" fn mock_kv_expire(handle: u64, key: RString, secs: u64) -> FfiFuture {
    *KV_EXPIRE.lock().unwrap() = (handle, key[..].to_string(), secs);
    match *KV_MODE.lock().unwrap() {
        1 => ready(Err("expire down".into())),
        2 => ready(Ok(b"{nope".to_vec())),
        _ => ready(Ok(b"true".to_vec())),
    }
}
extern "C" fn mock_kv_incr(handle: u64, key: RString) -> FfiFuture {
    *KV_INCR.lock().unwrap() = (handle, key[..].to_string());
    match *KV_MODE.lock().unwrap() {
        1 => ready(Err("incr down".into())),
        2 => ready(Ok(b"{nope".to_vec())),
        _ => ready(Ok(b"42".to_vec())),
    }
}
extern "C" fn mock_kv_close(handle: u64) {
    KV_CLOSED.store(handle, AtomicOrdering::SeqCst);
}

fn mock_kv_vtable() -> &'static KVStoreVtable {
    Box::leak(Box::new(KVStoreVtable {
        connect: mock_kv_connect,
        get: mock_kv_get,
        set: mock_kv_set,
        del: mock_kv_del,
        expire: mock_kv_expire,
        incr: mock_kv_incr,
        close: mock_kv_close,
    }))
}

#[tokio::test(flavor = "multi_thread")]
async fn kv_get_returns_option_decoded_from_json() {
    let _g = T_LOCK.lock().unwrap();
    let kv = FfiKVStore::new(42, mock_kv_vtable());
    assert_eq!(kv.get("k").await.unwrap().as_deref(), Some("blobdata"));
    let (h, key) = KV_GET.lock().unwrap().clone();
    assert_eq!((h, key.as_str()), (42, "k"));
}

#[tokio::test(flavor = "multi_thread")]
async fn kv_set_and_del_forward() {
    let _g = T_LOCK.lock().unwrap();
    let kv = FfiKVStore::new(42, mock_kv_vtable());
    kv.set("k", "v").await.unwrap();
    let (h, key, value) = KV_SET.lock().unwrap().clone();
    assert_eq!((h, key.as_str(), value.as_str()), (42, "k", "v"));
    kv.del("k").await.unwrap();
    let (h, key) = KV_DEL.lock().unwrap().clone();
    assert_eq!((h, key.as_str()), (42, "k"));
}

/// expire 的 Duration 在宿主侧经 kv::expire_secs 向上取整到整秒再过线。
#[tokio::test(flavor = "multi_thread")]
async fn kv_expire_converts_duration_to_whole_seconds() {
    let _g = T_LOCK.lock().unwrap();
    let kv = FfiKVStore::new(42, mock_kv_vtable());
    // 500ms → 1s（Redis EXPIRE 不接受 0s——与 InMemoryKV 毫秒语义对齐）。
    assert!(
        kv.expire("k", std::time::Duration::from_millis(500))
            .await
            .unwrap()
    );
    let (h, key, secs) = KV_EXPIRE.lock().unwrap().clone();
    assert_eq!((h, key.as_str(), secs), (42, "k", 1));
    assert!(
        kv.expire("k", std::time::Duration::from_secs(2))
            .await
            .unwrap()
    );
    assert_eq!(KV_EXPIRE.lock().unwrap().2, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn kv_incr_returns_i64_decoded_from_json() {
    let _g = T_LOCK.lock().unwrap();
    let kv = FfiKVStore::new(42, mock_kv_vtable());
    assert_eq!(kv.incr("k").await.unwrap(), 42);
    let (h, key) = KV_INCR.lock().unwrap().clone();
    assert_eq!((h, key.as_str()), (42, "k"));
}

#[test]
fn kv_drop_calls_close() {
    let _g = T_LOCK.lock().unwrap();
    KV_CLOSED.store(0, AtomicOrdering::SeqCst);
    drop(FfiKVStore::new(42, mock_kv_vtable()));
    assert_eq!(KV_CLOSED.load(AtomicOrdering::SeqCst), 42);
}

// ---- auth 轴 mock vtable（Task auth-1；同步 RResult 直返，无 FfiFuture）----

/// auth verify 行为开关：0=对象用户；1=null（匿名放行）；2=标量；3=坏 JSON；4=Err。
static AUTH_MODE: Mutex<u8> = Mutex::new(0);
static AUTH_GOT: Mutex<(String, String, String, String)> =
    Mutex::new((String::new(), String::new(), String::new(), String::new()));

extern "C" fn mock_auth_verify(
    path: RString,
    method: RString,
    authorization: RString,
    headers: RString,
) -> RResult<RString, RString> {
    *AUTH_GOT.lock().unwrap() = (
        path[..].to_string(),
        method[..].to_string(),
        authorization[..].to_string(),
        headers[..].to_string(),
    );
    match *AUTH_MODE.lock().unwrap() {
        1 => RResult::Ok(RString::from("null")),
        2 => RResult::Ok(RString::from("5")),
        3 => RResult::Ok(RString::from("{nope")),
        4 => RResult::Err(RString::from("token expired")),
        _ => RResult::Ok(RString::from(r#"{"id":"u1"}"#)),
    }
}

fn mock_auth_vtable() -> &'static oj_plugin_ffi::AuthGuardVtable {
    Box::leak(Box::new(oj_plugin_ffi::AuthGuardVtable {
        verify: mock_auth_verify,
    }))
}

// ---- 补测：await_ffi 协议边界 / auth 守卫契约 / 各轴错误臂 ----

use crate::bridge::AuthGuard;

/// 断言 BridgeResult 为 Err 并取错误文案（Ok 型多为非 Debug 的 dyn Trait 适配器，无法 unwrap_err）。
fn expect_err<T>(r: BridgeResult<T>) -> String {
    match r {
        Ok(_) => panic!("expected error"),
        Err(e) => e.to_string(),
    }
}

/// loader 错误文本 → 分类启发式的全关键词臂：平台/架构类逐词命中
/// PlatformMismatch，其余落 DependencyResolution（分类只影响文案，不影响 fail-fast）。
#[test]
fn given_loader_error_texts_when_classified_then_platform_or_dependency() {
    let p = Path::new("/x/plugin.dylib");
    for text in [
        "architecture mismatch",
        "incompatible architecture",
        "mach-o, but wrong file type",
        "wrong ELF class: ELFCLASS32",
        "wrong ELF data format",
        "version `GLIBC_2.38' not found",
        "not a mach-o image",
        "file too short",
        "%1 is not a valid Win32 application",
    ] {
        assert!(
            matches!(
                classify_load_error(p, text),
                PluginLoadError::PlatformMismatch { .. }
            ),
            "{text}"
        );
    }
    assert!(matches!(
        classify_load_error(p, "undefined symbol: _xyz"),
        PluginLoadError::DependencyResolution { .. }
    ));
}

/// es search 返回非 JSON → decode 错误臂；index_doc/delete_doc 插件报错 → 透传点名调用。
#[tokio::test(flavor = "multi_thread")]
async fn given_es_error_variants_when_search_index_delete_then_errs_name_the_call() {
    let _g = T_LOCK.lock().unwrap();
    let b = FfiEsBackend::new(1, mock_vtable());
    {
        let _m = Mode::set(&ES_MODE, 2);
        let e = b
            .search("i", serde_json::json!({}))
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("search decode"), "{e}");
    }
    {
        let _m = Mode::set(&ES_MODE, 3);
        let e = b
            .index_doc("i", "7", serde_json::json!({}))
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("index_doc") && e.contains("index boom"), "{e}");
    }
    {
        let _m = Mode::set(&ES_MODE, 4);
        let e = b.delete_doc("i", "7").await.unwrap_err().to_string();
        assert!(e.contains("delete_doc") && e.contains("delete boom"), "{e}");
    }
}

// ---- auth 守卫契约（匿名/用户/坏插件输出/拒签）----

#[test]
fn given_auth_plugin_returns_user_object_when_verify_then_user_parsed_and_header_forwarded() {
    let _g = T_LOCK.lock().unwrap();
    let _m = Mode::set(&AUTH_MODE, 0);
    let guard = FfiAuthGuard::new(mock_auth_vtable());
    let user = guard
        .verify(
            "/v1/api/u/",
            "POST",
            Some("Bearer tok1"),
            Some(r#"{"cookie":"oj_sess=abc"}"#),
        )
        .unwrap();
    assert_eq!(user.unwrap()["id"], "u1");
    let (path, method, authz, headers) = AUTH_GOT.lock().unwrap().clone();
    assert_eq!(
        (
            path.as_str(),
            method.as_str(),
            authz.as_str(),
            headers.as_str()
        ),
        (
            "/v1/api/u/",
            "POST",
            "Bearer tok1",
            r#"{"cookie":"oj_sess=abc"}"#
        )
    );
}

/// 匿名契约：插件回 null → Ok(None) 放行不注入；无 Authorization 头 → 空串过线。
#[test]
fn given_anonymous_path_when_verify_then_none_and_empty_authorization_forwarded() {
    let _g = T_LOCK.lock().unwrap();
    let _m = Mode::set(&AUTH_MODE, 1);
    let guard = FfiAuthGuard::new(mock_auth_vtable());
    assert_eq!(guard.verify("/p", "GET", None, None).unwrap(), None);
    let (_, _, authz, headers) = AUTH_GOT.lock().unwrap().clone();
    assert_eq!(authz, "");
    assert_eq!(headers, "");
}

/// 坏插件输出（标量 / 非 JSON）→ Err 点名契约违约，不静默当匿名放行。
#[test]
fn given_auth_plugin_speaks_gibberish_when_verify_then_contract_violation_errs() {
    let _g = T_LOCK.lock().unwrap();
    let guard = FfiAuthGuard::new(mock_auth_vtable());
    let _m2 = Mode::set(&AUTH_MODE, 2);
    let e = guard.verify("/p", "GET", None, None).unwrap_err();
    assert!(e.contains("non-object user"), "{e}");
    let _m3 = Mode::set(&AUTH_MODE, 3);
    let e = guard.verify("/p", "GET", None, None).unwrap_err();
    assert!(e.contains("auth plugin returned bad json"), "{e}");
}

/// 插件拒签（Err 臂）→ 错误文案原样透传（401 消息契约）。
#[test]
fn given_auth_plugin_rejects_when_verify_then_err_carries_plugin_message() {
    let _g = T_LOCK.lock().unwrap();
    let _m = Mode::set(&AUTH_MODE, 4);
    let guard = FfiAuthGuard::new(mock_auth_vtable());
    let e = guard
        .verify("/p", "GET", Some("Bearer bad"), None)
        .unwrap_err();
    assert_eq!(e, "token expired");
}

// ---- db 轴错误臂 ----

/// db connect 三类失败：插件报错 / 非 JSON（decode）/ 缺 handle——文案点名阶段。
#[tokio::test(flavor = "multi_thread")]
async fn given_db_connect_failures_when_connect_then_errs_name_the_stage() {
    let _g = T_LOCK.lock().unwrap();
    let be = FfiDbBackend::new("db-mysql", mock_db_vtable(), mock_dialect_info());
    {
        let _m = Mode::set(&DB_CONNECT_MODE, 1);
        let e = expect_err(be.connect("dsn", std::path::Path::new("/tmp")).await);
        assert!(e.contains("db connect") && e.contains("db down"), "{e}");
    }
    {
        let _m = Mode::set(&DB_CONNECT_MODE, 2);
        let e = expect_err(be.connect("dsn", std::path::Path::new("/tmp")).await);
        assert!(e.contains("db connect decode"), "{e}");
    }
    {
        let _m = Mode::set(&DB_CONNECT_MODE, 3);
        let e = expect_err(be.connect("dsn", std::path::Path::new("/tmp")).await);
        assert!(e.contains("db connect: missing handle"), "{e}");
    }
}

/// begin 三类失败：插件报错 / 非 JSON（decode）/ 缺 tx_id。
#[tokio::test(flavor = "multi_thread")]
async fn given_db_begin_failures_when_begin_then_errs_name_the_stage() {
    let _g = T_LOCK.lock().unwrap();
    let da = FfiDataAccessor::new(42, mock_db_vtable(), mock_dialect_info());
    {
        let _m = Mode::set(&DB_BEGIN_MODE, 1);
        let e = expect_err(da.begin().await);
        assert!(e.contains("db begin") && e.contains("begin down"), "{e}");
    }
    {
        let _m = Mode::set(&DB_BEGIN_MODE, 2);
        let e = expect_err(da.begin().await);
        assert!(e.contains("db begin decode"), "{e}");
    }
    {
        let _m = Mode::set(&DB_BEGIN_MODE, 3);
        let e = expect_err(da.begin().await);
        assert!(e.contains("db begin: missing tx_id"), "{e}");
    }
}

/// query/exec/tx_query/tx_exec 插件报错 → 文案点名具体调用。
#[tokio::test(flavor = "multi_thread")]
async fn given_db_calls_fail_when_query_exec_tx_then_errs_name_the_call() {
    let _g = T_LOCK.lock().unwrap();
    let _m = Mode::set(&DB_CALL_MODE, 1);
    let da = FfiDataAccessor::new(42, mock_db_vtable(), mock_dialect_info());
    let e = da
        .query_with_params("s", &[])
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("db query") && e.contains("call down"), "{e}");
    let e = da.exec_with_params("s", &[]).await.unwrap_err().to_string();
    assert!(e.contains("db exec"), "{e}");
    let tx = da.begin().await.unwrap();
    let e = tx.query("s", &[]).await.unwrap_err().to_string();
    assert!(e.contains("db tx_query"), "{e}");
    let e = tx.exec("s", &[]).await.unwrap_err().to_string();
    assert!(e.contains("db tx_exec"), "{e}");
}

/// query/exec/tx_query/tx_exec 返回非 JSON → decode 错误臂。
#[tokio::test(flavor = "multi_thread")]
async fn given_db_calls_return_gibberish_when_query_exec_tx_then_decode_errs() {
    let _g = T_LOCK.lock().unwrap();
    let _m = Mode::set(&DB_CALL_MODE, 2);
    let da = FfiDataAccessor::new(42, mock_db_vtable(), mock_dialect_info());
    let e = da
        .query_with_params("s", &[])
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("db query decode"), "{e}");
    let e = da.exec_with_params("s", &[]).await.unwrap_err().to_string();
    assert!(e.contains("db exec decode"), "{e}");
    let tx = da.begin().await.unwrap();
    let e = tx.query("s", &[]).await.unwrap_err().to_string();
    assert!(e.contains("db tx_query decode"), "{e}");
    let e = tx.exec("s", &[]).await.unwrap_err().to_string();
    assert!(e.contains("db tx_exec decode"), "{e}");
}

/// T6 两层模型：dialect() / dialect_info() 由**构造期注入的 DialectInfo** 派生，
/// 不再调用 vtable `dialect(handle)`（连接级静态属性，运行期纯查表）。
/// - known 名称（postgres）→ 对应枚举 + 描述符一致；
/// - 未知名称（oracle）→ dialect() 兜底 Sqlite，但 dialect_info().name 保留原始声明
///   （「未知方言」是开放契约下的有效状态，门禁在 T8 的六处 gate 点报 structured unsupported）；
/// - dialect() 对未知方言 fail-loud（回落 Sqlite 已删除）：描述符仍然保留原始声明，
///   Dialect::known 不接受，dialect() 调用即 panic（收尾安全网，正常路径由 gate 先行拦截）。
#[test]
fn ffi_accessor_dialect_derives_from_injected_info_not_vtable() {
    use crate::bridge::dialect_caps::caps_from_json;
    let _g = T_LOCK.lock().unwrap();

    // 默认注入：postgres 描述符
    let da = FfiDataAccessor::new(42, mock_db_vtable(), mock_dialect_info());
    assert_eq!(da.dialect(), Dialect::Postgres);
    assert_eq!(da.dialect_info().name, "postgres");
    assert_eq!(
        da.dialect_info().caps.placeholder,
        crate::bridge::dialect_caps::PlaceholderStyle::Dollar
    );
    assert_eq!(
        da.dialect_info().caps.id,
        crate::bridge::dialect_caps::IdStrategy::Returning
    );

    // 未知名称：描述符 name 保留原始声明；Dialect::known 不接受，门控点据此拒绝。
    // dialect() 对未知方言 fail-loud（不再静默回落 Sqlite）——直接调用应 panic。
    let oracle =
        caps_from_json(r#"{"dialect":"oracle","capabilities":["placeholder_colon","id_none"]}"#)
            .unwrap();
    let da2 = FfiDataAccessor::new(42, mock_db_vtable(), oracle.clone());
    assert_eq!(da2.dialect_info().name, "oracle", "描述符保留原始声明");
    assert!(
        Dialect::known(&da2.dialect_info().name).is_none(),
        "oracle 不在 builder-known 子集"
    );
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = da2.dialect();
    }));
    assert!(
        r.is_err(),
        "dialect() 对未知方言必须 fail-loud（不再静默回落 Sqlite）"
    );
}

/// commit 失败 → Err 且事务视为未完结：drop 保底回滚仍触发（ReqState reset
/// 丢弃存活事务 = 保底回滚语义的 FFI 保留，见 FfiTxSession::drop）。
#[tokio::test(flavor = "multi_thread")]
async fn given_tx_commit_fails_when_dropped_then_guaranteed_rollback_still_fires() {
    let _g = T_LOCK.lock().unwrap();
    DB_COMMITTED.store(0, AtomicOrdering::SeqCst);
    DB_ROLLED_BACK.store(0, AtomicOrdering::SeqCst);
    let _m = Mode::set(&DB_TX_END_MODE, 1);
    let da = FfiDataAccessor::new(42, mock_db_vtable(), mock_dialect_info());
    let tx = da.begin().await.unwrap();
    let e = tx.commit().await.unwrap_err().to_string();
    assert!(
        e.contains("db tx_commit") && e.contains("commit down"),
        "{e}"
    );
    drop(tx);
    assert_eq!(DB_ROLLED_BACK.load(AtomicOrdering::SeqCst), 7);
}

/// rollback 失败 → Err 透传（finished 未置位，drop 会再补一次回滚尝试）。
#[tokio::test(flavor = "multi_thread")]
async fn given_tx_rollback_fails_when_rollback_then_err_names_rollback() {
    let _g = T_LOCK.lock().unwrap();
    DB_ROLLED_BACK.store(0, AtomicOrdering::SeqCst);
    let _m = Mode::set(&DB_TX_END_MODE, 1);
    let da = FfiDataAccessor::new(42, mock_db_vtable(), mock_dialect_info());
    let tx = da.begin().await.unwrap();
    let e = tx.rollback().await.unwrap_err().to_string();
    assert!(
        e.contains("db tx_rollback") && e.contains("rollback down"),
        "{e}"
    );
}

/// Drop 保底回滚对插件侧 rollback 任务失败免疫：vtable 报错（catch_future 把插件
/// 任务 panic 收敛为错误 future 的形态）时 Drop 仍恰好 fire 一次，宿主侧不 panic、
/// FfiGuard 正常 free（不二次释放）。评审 B3 宿主侧守护——条目移除不得依赖 rollback
/// 任务成功（插件侧 remove-before-await 已由 oj-db-mysql/oj-db-postgres 实现）。
#[tokio::test(flavor = "multi_thread")]
async fn given_rollback_task_fails_when_dropped_then_guaranteed_rollback_fires_once() {
    let _g = T_LOCK.lock().unwrap();
    DB_ROLLED_BACK.store(0, AtomicOrdering::SeqCst);
    let _m = Mode::set(&DB_TX_END_MODE, 1);
    let da = FfiDataAccessor::new(42, mock_db_vtable(), mock_dialect_info());
    let tx = da.begin().await.unwrap();
    drop(tx); // 保底回滚 fire-and-forget：结果放弃，不因插件失败 panic/泄漏
    assert_eq!(DB_ROLLED_BACK.load(AtomicOrdering::SeqCst), 7);
}

// ---- blob 轴错误臂 ----

/// blob 五方法插件报错 → 错误文案点名操作；serve 走 url → 错误透传。
#[tokio::test(flavor = "multi_thread")]
async fn given_blob_plugin_down_when_put_get_del_serve_then_errs_name_the_operation() {
    let _g = T_LOCK.lock().unwrap();
    let _m = Mode::set(&BLOB_MODE, 1);
    let b = FfiBlobBackend::new(42, mock_blob_vtable());
    let e = b.put("k", b"x", None).await.unwrap_err().to_string();
    assert!(e.contains("blob put") && e.contains("put down"), "{e}");
    let e = b.get("k").await.unwrap_err().to_string();
    assert!(e.contains("blob get"), "{e}");
    let e = b.del("k").await.unwrap_err().to_string();
    assert!(e.contains("blob del"), "{e}");
    let e = expect_err(b.serve("k").await);
    assert!(e.contains("blob url"), "{e}");
}

/// url/content_type 返回坏 UTF-8 字节 → decode 错误臂。
#[tokio::test(flavor = "multi_thread")]
async fn given_blob_gibberish_bytes_when_url_or_content_type_then_decode_errs() {
    let _g = T_LOCK.lock().unwrap();
    let _m = Mode::set(&BLOB_MODE, 2);
    let b = FfiBlobBackend::new(42, mock_blob_vtable());
    let e = b.url("k").await.unwrap_err().to_string();
    assert!(e.contains("blob url decode"), "{e}");
    let e = b.content_type("k").await.unwrap_err().to_string();
    assert!(e.contains("blob content_type decode"), "{e}");
}

// ---- bus 轴错误臂 ----

/// bus connect 三类失败：插件报错 / 非 JSON（decode）/ 缺 handle。
#[tokio::test(flavor = "multi_thread")]
async fn given_bus_connect_failures_when_connect_then_errs_name_the_stage() {
    let _g = T_LOCK.lock().unwrap();
    let be = FfiBusBackend::new("bus-kafka", mock_bus_vtable());
    let cfg = BrokerCfg {
        kind: "kafka".into(),
        brokers: vec![],
        ..Default::default()
    };
    {
        let _m = Mode::set(&BUS_CONNECT_MODE, 1);
        let e = expect_err(be.connect(&cfg).await);
        assert!(e.contains("bus connect") && e.contains("bus down"), "{e}");
    }
    {
        let _m = Mode::set(&BUS_CONNECT_MODE, 2);
        let e = expect_err(be.connect(&cfg).await);
        assert!(e.contains("bus connect decode"), "{e}");
    }
    {
        let _m = Mode::set(&BUS_CONNECT_MODE, 3);
        let e = expect_err(be.connect(&cfg).await);
        assert!(e.contains("bus connect: missing handle"), "{e}");
    }
}

/// publish 插件报错 → 透传点名 bus publish（本地 fan-out 0 只在成功路径）。
#[tokio::test(flavor = "multi_thread")]
async fn given_bus_publish_fails_when_publish_then_err_names_publish() {
    let _g = T_LOCK.lock().unwrap();
    BUS_PUBLISH_FAIL.store(true, AtomicOrdering::SeqCst);
    let broker = FfiEventBroker::new("kafka", 42, mock_bus_vtable());
    let e = broker
        .publish("t", &BusPayload::Json(serde_json::json!({})))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("bus publish") && e.contains("publish down"),
        "{e}"
    );
}

/// kind 推断：插件名无 "bus-" 前缀时取全名（unwrap_or 臂）。
#[test]
fn given_backend_name_without_bus_prefix_when_new_then_kind_is_full_name() {
    let _g = T_LOCK.lock().unwrap();
    let be = FfiBusBackend::new("redis", mock_bus_vtable());
    assert_eq!(be.kind(), "redis");
}

// ---- kv 轴错误臂 ----

/// kv 五方法插件报错 → 错误文案点名操作。
#[tokio::test(flavor = "multi_thread")]
async fn given_kv_plugin_down_when_ops_then_errs_name_the_operation() {
    let _g = T_LOCK.lock().unwrap();
    let _m = Mode::set(&KV_MODE, 1);
    let kv = FfiKVStore::new(42, mock_kv_vtable());
    assert!(
        kv.get("k")
            .await
            .unwrap_err()
            .to_string()
            .contains("kv get")
    );
    assert!(
        kv.set("k", "v")
            .await
            .unwrap_err()
            .to_string()
            .contains("kv set")
    );
    assert!(
        kv.del("k")
            .await
            .unwrap_err()
            .to_string()
            .contains("kv del")
    );
    assert!(
        kv.expire("k", std::time::Duration::from_secs(1))
            .await
            .unwrap_err()
            .to_string()
            .contains("kv expire")
    );
    assert!(
        kv.incr("k")
            .await
            .unwrap_err()
            .to_string()
            .contains("kv incr")
    );
}

/// kv get/expire/incr 返回非 JSON → decode 错误臂。
#[tokio::test(flavor = "multi_thread")]
async fn given_kv_gibberish_when_get_expire_incr_then_decode_errs() {
    let _g = T_LOCK.lock().unwrap();
    let _m = Mode::set(&KV_MODE, 2);
    let kv = FfiKVStore::new(42, mock_kv_vtable());
    assert!(
        kv.get("k")
            .await
            .unwrap_err()
            .to_string()
            .contains("kv get decode")
    );
    assert!(
        kv.expire("k", std::time::Duration::from_secs(1))
            .await
            .unwrap_err()
            .to_string()
            .contains("kv expire decode")
    );
    assert!(
        kv.incr("k")
            .await
            .unwrap_err()
            .to_string()
            .contains("kv incr decode")
    );
}
