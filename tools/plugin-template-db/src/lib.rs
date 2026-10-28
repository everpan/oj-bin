//! db 轴插件开发模板：最小可运行骨架（进程内 demo 驱动，仅作范式载体）。
//!
//! 目标不是「能用的数据库驱动」，而是把**契约面**摆全，让新方言插件照着填：
//! 能力位自报（`describe`）、结构化错误信封、流式三态信封、句柄 / 事务 / 游标生命周期，
//! 以及「声明与行为不许漂移」的结构化写法。真实驱动替换 12 个 `do_*`（connect / query /
//! exec / begin / tx_* / stream_*）即可，vtable / init / describe / 信封都不必动。
//!
//! 配套文档（先读）：
//! - `docs/plugins/plugin-development.md` §15「新增数据库方言 conformance 清单」
//!   ——§15.2 是能力位注册表（每能力位一正一反），§15.6 是可直接拷进 PR 的自查表；
//! - `docs/devkit/db-guide.md` §10.6「类型映射表与值契约」——六类 schema 类型的 DDL 映射与
//!   读侧 JS 值契约（超精度、布尔、blob、未识别列类型必须报错）。
//!
//! # db 轴四契约（违反任一条 = 加载失败 / 调用出错 / UB）
//!
//! 1. vtable 必须**静态可寻址**（`static` + `oj_plugin_entry!` 取地址）；
//! 2. 每个 vtable 方法经 [`catch_future`] / [`catch_void`] / [`catch_value`] 包一层——
//!    跨界 unwind 是 UB，`oj_plugin_entry!` 只兜 `init`；
//! 3. 错误一律走 `Err(RString)`，载荷是**信封 JSON**
//!    `{category,retryable,session_disposition,message,data}`（宿主 `parse_raw` 单点解析）。
//!    只给纯文案会让 JS 侧 `e.code` / `e.retryable` / `e.sessionDisposition` 全退化成
//!    `undefined`，重试决策只能靠正则扒文本；
//! 4. `describe()` 声明的能力位必须与实现一致：声明 `stream` 却返回
//!    `{"unsupported":true}` = 插件说谎（宿主不回退，大结果集直接失败）；未声明却在实现
//!    = 宿主告警后采纳。本模板用 [`SUPPORTS_STREAM`] / [`SUPPORTS_CANCEL`] 同时驱动两侧，
//!    从结构上防漂移（见 [`capability_list`] 与 [`do_stream_open`]）。
//!
//! 另外两条与「能不能跑」无关、但决定「会不会在真库上错」：
//! - 占位符改写归属：`placeholder_*` 只声明风格，**改写归本插件内部**（宿主对 `colon`
//!   透传、对 `q`/`dollar` 也只在构造器路径替换）；
//! - 读侧未识别列类型必须**响亮报错**（点名 列名 + 类型 + cast 指引），不许静默 `null`——
//!   静默 null 会让租户守卫把 `tenant_id` 绑成文本，真库回一句
//!   `operator does not exist: bigint = text`，排障方向完全错。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

use oj_plugin_ffi::{
    ABI_VERSION, DataAccessorVtable, FfiFuture, HostContext, PluginDescriptor, RArc, RResult,
    RString, RVec,
};
use serde_json::{Value, json};

// ---- 身份常量（三方对齐，见 README「命名三方对齐」）----

/// 方言名：`describe().dialect` 与 [`dialect`]（handle 级）必须是同一个字符串；
/// 它同时出现在 JS 的 `e.data.dialect`、启动横幅与 `GET {base}/plugins` 的 `caps` 列。
const DIALECT: &str = "demo";

/// 认领的 DSN scheme 前缀（装配期据此把 connect 路由到本插件；宿主不硬编码白名单）。
const SCHEME: &str = "demo://";

// ---- 能力位（8 个闭集：两组互斥各选一 + 两个可选；串值由宿主常量定义，禁止自创）----

/// 占位符风格：`placeholder_q`（`?`）/ `placeholder_dollar`（`$1`）/ `placeholder_colon`
/// （`:name`）。选错 = 宿主构造器路径生成的 SQL 在本方言上语法错。
const CAP_PLACEHOLDER: &str = "placeholder_colon";

/// 取 ID 策略：`id_returning`（`RETURNING` 单语句）/ `id_two_step`（`LAST_INSERT_ID` 两步）
/// / `id_none`（无自增）。选错 = `db.table(..).insert()` 拿不到主键，静默返回 undefined。
const CAP_ID: &str = "id_none";

/// 是否实现流式（[`stream_open`] / [`stream_next`] / [`stream_cancel`] / [`stream_close`]）。
const SUPPORTS_STREAM: bool = true;

/// 是否实现方言级取消（协作式：批间生效；声明了就要真能停服务端查询，见 README 的
/// cancel 探针）。
const SUPPORTS_CANCEL: bool = true;

/// 流式单批上限（协议约定 ≤100 行/次；宿主按此假设做背压）。
const STREAM_BATCH: usize = 100;

/// 能力串数组的唯一来源：`describe()` 与单测共用，禁止各写一份。
fn capability_list() -> Vec<&'static str> {
    let mut caps = vec![CAP_PLACEHOLDER, CAP_ID];
    if SUPPORTS_STREAM {
        caps.push("stream");
    }
    if SUPPORTS_CANCEL {
        caps.push("cancel");
    }
    caps
}

/// 工厂级方言描述符 JSON（ABI 12，装配期读一次）。
///
/// 宿主用闭集解析器校验：两组各恰好一枚、未知串 fail-loud（点名违规串）、
/// `stream`/`cancel` 出现即 true。所以这里拼错一个字母 = 启动失败，而不是运行期才炸。
fn describe_json() -> String {
    json!({ "dialect": DIALECT, "capabilities": capability_list() }).to_string()
}

// ---- 错误信封（ABI 12）----

/// 错误类别闭集（8 个）。用枚举而非字符串：typo 编译期就挂，不用等真库回错。
/// 模板只用到其中几个，但闭集必须完整——你的方言要按驱动真实错误码挑（§15.3）。
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Category {
    Syntax,
    Constraint,
    Timeout,
    Connection,
    Capacity,
    Unsupported,
    Cancelled,
    Internal,
}

impl Category {
    const fn as_str(self) -> &'static str {
        match self {
            Category::Syntax => "syntax",
            Category::Constraint => "constraint",
            Category::Timeout => "timeout",
            Category::Connection => "connection",
            Category::Capacity => "capacity",
            Category::Unsupported => "unsupported",
            Category::Cancelled => "cancelled",
            Category::Internal => "internal",
        }
    }
}

/// 会话处置建议：`keep`（连接可继续）/ `reset`（回滚/清状态后可继续）/ `close`（应丢弃）。
/// 判错会把「连接池还在排队」误判成「连接坏了」，让告警整体打到 infra。
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Disposition {
    Keep,
    Reset,
    Close,
}

impl Disposition {
    const fn as_str(self) -> &'static str {
        match self {
            Disposition::Keep => "keep",
            Disposition::Reset => "reset",
            Disposition::Close => "close",
        }
    }
}

/// 结构化错误信封。**vtable 的 `Err` 载荷恒为此 JSON**（不能是纯文案）。
///
/// - `message` 以方言操作名开头（`db query: ...`），与内置方言文案同构；
/// - `data` 只放机读补充（`capability` / `vendor_code` / 列名…），不放给人看的叙述。
fn envelope(
    cat: Category,
    retryable: bool,
    disp: Disposition,
    message: impl Into<String>,
    data: Value,
) -> String {
    json!({
        "category": cat.as_str(),
        "retryable": retryable,
        "session_disposition": disp.as_str(),
        "message": message.into(),
        "data": data,
    })
    .to_string()
}

fn failure(
    cat: Category,
    retryable: bool,
    disp: Disposition,
    message: impl Into<String>,
    data: Value,
) -> Result<Vec<u8>, String> {
    Err(envelope(cat, retryable, disp, message, data))
}

fn lock<'a, T>(m: &'a Mutex<T>, op: &str) -> Result<MutexGuard<'a, T>, String> {
    m.lock().map_err(|_| {
        envelope(
            Category::Internal,
            false,
            Disposition::Keep,
            format!("{op}: plugin state poisoned"),
            Value::Null,
        )
    })
}

// ---- 连接 / 事务 / 游标状态 ----

/// 每连接状态。真实插件在这里持 driver 的 session；`close()` 释放。
struct Conn {
    /// `tx_id` → 事务句柄（模板只记存在性，真实驱动存 driver 的 transaction 对象）。
    txs: Mutex<HashMap<u64, ()>>,
    /// `stream_id` → 游标。
    streams: Mutex<HashMap<u64, Cursor>>,
}

/// 流式游标。**必须由 `stream_close` 显式回收**——`stream_cancel` 只置标志，
/// 用它兼任清理会让每个被中止的查询泄漏一个条目。
struct Cursor {
    rows: Vec<Value>,
    pos: usize,
    cancelled: bool,
}

impl Cursor {
    /// 三态信封，**绝不裸 `null`**（单列 null 的行会与「结束」冲突，spec §1.4）：
    /// `{"rows":[...]}` / `{"done":true}` / `{"error":"cancelled"}`。
    fn next_batch(&mut self) -> String {
        if self.cancelled {
            return json!({ "error": "cancelled" }).to_string();
        }
        if self.pos >= self.rows.len() {
            // done 幂等：宿主可能因背压多问一次，第二次仍须是 done 而不是 panic/空。
            return json!({ "done": true }).to_string();
        }
        let end = (self.pos + STREAM_BATCH).min(self.rows.len());
        let batch = self.rows[self.pos..end].to_vec();
        self.pos = end;
        json!({ "rows": batch }).to_string()
    }
}

struct State {
    rt: tokio::runtime::Runtime,
    conns: Mutex<HashMap<u64, Conn>>,
    next_handle: AtomicU64,
    next_tx: AtomicU64,
    next_stream: AtomicU64,
}

static PLUGIN: OnceLock<State> = OnceLock::new();

fn state() -> &'static State {
    PLUGIN.get().expect("oj-db-demo: init not called")
}

fn handle_err(op: &str, handle: u64) -> String {
    // 连接已 close / 从未 connect：会话不可继续，建议宿主丢弃该连接。
    envelope(
        Category::Connection,
        false,
        Disposition::Close,
        format!("{op}: handle {handle} not found（已 close 或从未 connect）"),
        json!({ "handle": handle }),
    )
}

fn with_conn<T>(
    op: &str,
    handle: u64,
    f: impl FnOnce(&Conn) -> Result<T, String>,
) -> Result<T, String> {
    let conns = lock(&state().conns, op)?;
    match conns.get(&handle) {
        Some(c) => f(c),
        None => Err(handle_err(op, handle)),
    }
}

// ---- 驱动接缝：新方言只改这十几个 do_*（connect/query/exec/begin/tx_*/stream_*）（真实驱动在此 await / 映射厂商错误码）----

fn parse_params(op: &str, params: &str) -> Result<Vec<Value>, String> {
    // 宿主永远发 JSON 数组；这里不合法 = 宿主 bug，归 internal，别赖到用户 SQL 上。
    match serde_json::from_str::<Value>(params) {
        Ok(Value::Array(a)) => Ok(a),
        Ok(_) => Err(envelope(
            Category::Internal,
            false,
            Disposition::Keep,
            format!("{op}: params 必须是 JSON 数组"),
            Value::Null,
        )),
        Err(e) => Err(envelope(
            Category::Internal,
            false,
            Disposition::Keep,
            format!("{op}: params 不是合法 JSON: {e}"),
            Value::Null,
        )),
    }
}

fn do_connect(dsn: &str) -> Result<Vec<u8>, String> {
    if !dsn.starts_with(SCHEME) {
        // 装配期按 `schemes()` 路由，走到这里说明 DSN 写错或 scheme 认领漂移。
        return failure(
            Category::Unsupported,
            false,
            Disposition::Keep,
            format!("db connect: 不支持的 DSN scheme（本插件只认 {SCHEME}）"),
            json!({ "scheme": SCHEME, "dialect": DIALECT }),
        );
    }
    let handle = state().next_handle.fetch_add(1, Ordering::SeqCst) + 1;
    lock(&state().conns, "db connect")?.insert(
        handle,
        Conn {
            txs: Mutex::new(HashMap::new()),
            streams: Mutex::new(HashMap::new()),
        },
    );
    Ok(json!({ "handle": handle }).to_string().into_bytes())
}

fn do_query(handle: u64, sql: &str, params: &str) -> Result<Vec<u8>, String> {
    let params = parse_params("db query", params)?;
    with_conn("db query", handle, |_c| {
        // 真实驱动在此 `fetch_all(sql, &params).await`。行转换三条硬要求
        // （`docs/devkit/db-guide.md` §10.6）：
        //   * 未识别的列类型 → **报错**（列名 + 类型 + cast 指引），不许静默 null；
        //   * i64 超出 2^53-1 → 十进制字符串（JS number 会掉精度），不是 JSON number；
        //   * 布尔统一出 true/false；blob 按 UTF-8 lossy 出串。
        //
        // 模板回显 SQL 与绑定顺序：占位符改写器写完后，用这条回显对账最省事。
        Ok(
            json!([{ "dialect": DIALECT, "sql": sql, "params": params }])
                .to_string()
                .into_bytes(),
        )
    })
}

fn do_exec(handle: u64, sql: &str, params: &str) -> Result<Vec<u8>, String> {
    let params = parse_params("db exec", params)?;
    let _ = (sql, params);
    with_conn("db exec", handle, |_c| {
        // 真实驱动返回 **匹配行数**（不是变更行数）。oj 栈在 MySQL 上协商了
        // CLIENT_FOUND_ROWS，下游按裸 MySQL「变更行」推演 affected-rows 会差 0/N。
        Ok(json!(0).to_string().into_bytes())
    })
}

fn do_begin(handle: u64) -> Result<Vec<u8>, String> {
    with_conn("db begin", handle, |c| {
        let tx_id = state().next_tx.fetch_add(1, Ordering::SeqCst) + 1;
        lock(&c.txs, "db begin")?.insert(tx_id, ());
        Ok(json!({ "tx_id": tx_id }).to_string().into_bytes())
    })
}

fn check_tx(c: &Conn, op: &str, tx_id: u64) -> Result<(), String> {
    if lock(&c.txs, op)?.contains_key(&tx_id) {
        Ok(())
    } else {
        // 事务已 commit/rollback 或从未 begin：JS 侧 `db.tx` 的使用错误，不是库故障。
        Err(tx_err(op, tx_id))
    }
}

fn tx_err(op: &str, tx_id: u64) -> String {
    envelope(
        Category::Internal,
        false,
        Disposition::Keep,
        format!("{op}: tx {tx_id} 不存在（已 commit/rollback？）"),
        json!({ "tx_id": tx_id }),
    )
}

fn do_tx_query(handle: u64, tx_id: u64, sql: &str, params: &str) -> Result<Vec<u8>, String> {
    let params = parse_params("db tx query", params)?;
    with_conn("db tx query", handle, |c| {
        check_tx(c, "db tx query", tx_id)?;
        // 真实驱动用事务对象执行（同一连接，语句走未提交快照）。
        Ok(
            json!([{ "dialect": DIALECT, "tx_id": tx_id, "sql": sql, "params": params }])
                .to_string()
                .into_bytes(),
        )
    })
}

fn do_tx_exec(handle: u64, tx_id: u64, sql: &str, params: &str) -> Result<Vec<u8>, String> {
    let params = parse_params("db tx exec", params)?;
    let _ = (sql, params);
    with_conn("db tx exec", handle, |c| {
        check_tx(c, "db tx exec", tx_id)?;
        Ok(json!(0).to_string().into_bytes())
    })
}

fn do_tx_commit(handle: u64, tx_id: u64) -> Result<Vec<u8>, String> {
    with_conn("db tx commit", handle, |c| {
        if lock(&c.txs, "db tx commit")?.remove(&tx_id).is_none() {
            return Err(tx_err("db tx commit", tx_id));
        }
        Ok(json!({ "ok": true }).to_string().into_bytes())
    })
}

fn do_tx_rollback(handle: u64, tx_id: u64) -> Result<Vec<u8>, String> {
    with_conn("db tx rollback", handle, |c| {
        // rollback 幂等：错误路径上可能先 rollback 再被 drop 路径重试一次。
        lock(&c.txs, "db tx rollback")?.remove(&tx_id);
        Ok(json!({ "ok": true }).to_string().into_bytes())
    })
}

fn do_stream_open(handle: u64, sql: &str, params: &str) -> Result<Vec<u8>, String> {
    if !SUPPORTS_STREAM {
        // 未声明 stream 能力时的**规定回退**：Ok({"unsupported":true})，宿主改走有界
        // fetch_all（与旧插件同行为）。返回 Err 会让带大结果集的查询直接失败。
        return Ok(json!({ "unsupported": true }).to_string().into_bytes());
    }
    let params = parse_params("db stream", params)?;
    with_conn("db stream", handle, |c| {
        // 真实驱动在此拿 driver 的 stream / 服务端游标；模板把结果一次性物化。
        let rows = vec![json!({ "dialect": DIALECT, "sql": sql, "params": params })];
        let stream_id = state().next_stream.fetch_add(1, Ordering::SeqCst) + 1;
        lock(&c.streams, "db stream")?.insert(
            stream_id,
            Cursor {
                rows,
                pos: 0,
                cancelled: false,
            },
        );
        Ok(json!({ "stream_id": stream_id }).to_string().into_bytes())
    })
}

fn do_stream_next(handle: u64, stream_id: u64) -> Result<Vec<u8>, String> {
    with_conn("db stream_next", handle, |c| {
        let mut streams = lock(&c.streams, "db stream_next")?;
        let cur = streams.get_mut(&stream_id).ok_or_else(|| {
            envelope(
                Category::Internal,
                false,
                Disposition::Keep,
                format!("db stream_next: stream {stream_id} 不存在（已 close？）"),
                json!({ "stream_id": stream_id }),
            )
        })?;
        Ok(cur.next_batch().into_bytes())
    })
}

fn do_stream_cancel(handle: u64, stream_id: u64) -> Result<Vec<u8>, String> {
    if !SUPPORTS_CANCEL {
        return failure(
            Category::Unsupported,
            false,
            Disposition::Keep,
            "db stream_cancel: 本插件未声明 cancel 能力",
            json!({ "capability": "cancel", "dialect": DIALECT }),
        );
    }
    with_conn("db stream_cancel", handle, |c| {
        let mut streams = lock(&c.streams, "db stream_cancel")?;
        match streams.get_mut(&stream_id) {
            // 协作式：置标志，下一次 stream_next 回 {"error":"cancelled"}。
            // 真实驱动还应**真的**去杀服务端查询（否则超时看门狗白等），
            // 且必须能在 5s 内让服务端查询消失——这是 cancel 能力位的验收口径。
            Some(cur) => {
                cur.cancelled = true;
                Ok(json!({ "ok": true }).to_string().into_bytes())
            }
            None => Ok(json!({ "ok": false }).to_string().into_bytes()),
        }
    })
}

fn do_stream_close(handle: u64, stream_id: u64) -> Result<Vec<u8>, String> {
    with_conn("db stream_close", handle, |c| {
        // 幂等（照 oj-db-mysql）：错误/取消路径上 close 可能被调两次，
        // 二次报错只会掩盖首个错误。宿主不解析本 op 的 ok 载荷 → 返回空。
        lock(&c.streams, "db stream_close")?.remove(&stream_id);
        Ok(Vec::new())
    })
}

// ---- vtable（同步签名返回 FfiFuture；异步工作 spawn 到插件 runtime）----
//
// 每个方法两层壳：`catch_future` 收敛**同步** panic（跨界 unwind = UB），
// `spawn_ffi_future` 把 async 体送上 runtime 并用 oneshot 回传。
// 真实驱动在此把 do_* 换成 async fn 后 `.await`，其余一字不改。

extern "C" fn connect(cfg: RString) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(&st.rt, async move {
            // 延迟解析 DSN 的逻辑放 do_connect 里；async 体只做 IO 编排。
            do_connect(&cfg[..])
        })
    })
}

extern "C" fn query(handle: u64, sql: RString, params: RString) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(
            &st.rt,
            async move { do_query(handle, &sql[..], &params[..]) },
        )
    })
}

extern "C" fn exec(handle: u64, sql: RString, params: RString) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(
            &st.rt,
            async move { do_exec(handle, &sql[..], &params[..]) },
        )
    })
}

extern "C" fn begin(handle: u64) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(&st.rt, async move { do_begin(handle) })
    })
}

extern "C" fn tx_query(handle: u64, tx_id: u64, sql: RString, params: RString) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(&st.rt, async move {
            do_tx_query(handle, tx_id, &sql[..], &params[..])
        })
    })
}

extern "C" fn tx_exec(handle: u64, tx_id: u64, sql: RString, params: RString) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(&st.rt, async move {
            do_tx_exec(handle, tx_id, &sql[..], &params[..])
        })
    })
}

extern "C" fn tx_commit(handle: u64, tx_id: u64) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(&st.rt, async move { do_tx_commit(handle, tx_id) })
    })
}

extern "C" fn tx_rollback(handle: u64, tx_id: u64) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(&st.rt, async move { do_tx_rollback(handle, tx_id) })
    })
}

/// 连接级方言自报（签名 `fn(u64) -> RString` 带不了错）。
///
/// 未知 / 已关闭 handle 上送 `"unknown"`，并响亮提示——宿主内部按已知子集降级处理，
/// 但**绝不冒充**某个真方言（冒充会让宿主拿错方言造 SQL）。本槽已降级为诊断，
/// 一致性校验以工厂级 [`describe`] 为准。
extern "C" fn dialect(handle: u64) -> RString {
    oj_plugin_ffi::catch_value(
        || {
            let ok = state()
                .conns
                .lock()
                .map(|c| c.contains_key(&handle))
                .unwrap_or(false);
            if ok {
                RString::from(DIALECT)
            } else {
                eprintln!("oj-db-demo: dialect: handle {handle} not found");
                RString::from("unknown")
            }
        },
        RString::from("unknown"),
    )
}

extern "C" fn close(handle: u64) {
    oj_plugin_ffi::catch_void(|| {
        if let Ok(mut conns) = state().conns.lock() {
            // 丢弃 Conn 即丢弃其全部 tx / 游标：真实驱动在此关连接（连接池还池）。
            conns.remove(&handle);
        }
    });
}

extern "C" fn schemes() -> RVec<RString> {
    oj_plugin_ffi::catch_value(
        || {
            let mut v = RVec::new();
            v.push(RString::from(SCHEME));
            v
        },
        RVec::new(),
    )
}

/// ABI 12：工厂级方言描述符。
extern "C" fn describe() -> RString {
    oj_plugin_ffi::catch_value(
        || RString::from(describe_json().as_str()),
        RString::from(""),
    )
}

// 契约 1：vtable 静态可寻址。
extern "C" fn stream_open(handle: u64, sql: RString, params: RString) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(&st.rt, async move {
            do_stream_open(handle, &sql[..], &params[..])
        })
    })
}

extern "C" fn stream_next(handle: u64, stream_id: u64) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(&st.rt, async move { do_stream_next(handle, stream_id) })
    })
}

extern "C" fn stream_cancel(handle: u64, stream_id: u64) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(&st.rt, async move { do_stream_cancel(handle, stream_id) })
    })
}

extern "C" fn stream_close(handle: u64, stream_id: u64) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(&st.rt, async move { do_stream_close(handle, stream_id) })
    })
}

// 契约 1：vtable 静态可寻址（`oj_plugin_entry!` 在编译期取地址 → 启动期 `dlsym` 稳定）。
static VTABLE: DataAccessorVtable = DataAccessorVtable {
    connect,
    query,
    exec,
    begin,
    tx_query,
    tx_exec,
    tx_commit,
    tx_rollback,
    dialect,
    close,
    schemes,
    stream_open,
    stream_next,
    stream_cancel,
    stream_close,
    describe,
};

// ---- 入口 ----

fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        // 三方对齐：包名 `oj-db-demo` / `cargo xtask plugin db-demo`（或不走 xtask）/
        // descriptor.name `db-demo` → loader 文件名 `libdb-demo.dylib`。
        // `plugins:` 清单键与 `plugins.<name>` 透传段也按 descriptor.name 查。
        name: RString::from("db-demo"),
        semver: RString::from(env!("CARGO_PKG_VERSION")),
        abi_version: ABI_VERSION,
        fingerprint: RString::from(oj_plugin_ffi::HOST_FINGERPRINT),
        // desc 会出现在 `GET {base}/plugins` 与 JS `plugins()`：写明方言 + 宿主最低版本。
        desc: RString::from(
            "db 轴插件模板：demo 方言（进程内范式载体，不连真库）；需宿主 ABI 12 \
             （v0.1.88 起，以 CHANGELOG 为准）",
        ),
    }
}

fn init(host: RArc<HostContext>, cfg: RString) -> RResult<PluginDescriptor, RString> {
    // 同进程二次 init（多装配 / 测试重载同一 dylib）：以首次为准，直接复用。
    if PLUGIN.get().is_some() {
        return RResult::Ok(descriptor());
    }
    let _ = (&host, &cfg); // db 插件无装配期配置：DSN 在 connect 按值传入
    // get_or_init：并发 init 时闭包只跑一次，避免败者的 tokio Runtime 从 async 上下文
    // drop 而 panic。
    PLUGIN.get_or_init(|| State {
        rt: tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("oj-db-demo tokio runtime"),
        conns: Mutex::new(HashMap::new()),
        next_handle: AtomicU64::new(0),
        next_tx: AtomicU64::new(0),
        next_stream: AtomicU64::new(0),
    });
    RResult::Ok(descriptor())
}

// 类型化轴臂（`db => &VT` 不加括号）；泛型轴才写 `generic(axis) => &VT`。
oj_plugin_ffi::oj_plugin_entry!(init, db => &VTABLE);

#[cfg(test)]
mod tests {
    use super::*;
    use oj_plugin_ffi::RBytes;
    use std::time::{Duration, Instant};

    extern "C" fn test_log(_level: u8, _msg: RString) {}
    extern "C" fn test_deliver(_topic: RString, _payload: RBytes) {}

    fn boot() {
        let host = RArc::new(HostContext {
            log: test_log,
            deliver: test_deliver,
        });
        let _ = init(host, RString::from(""));
    }

    /// 驱动一个 FfiFuture：以真实墙钟为界轮询（固定次数 yield 在负载下会误报超时）。
    fn drive(f: FfiFuture) -> Result<Vec<u8>, String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let state = f.state;
        loop {
            match (f.poll)(state) {
                0 => {
                    if Instant::now() >= deadline {
                        (f.free)(state); // 超时也要释放 state（防 FfiTask 泄漏）
                        return Err("ffi drive timeout".into());
                    }
                    std::thread::yield_now();
                }
                code => {
                    let r = (f.take)(state);
                    (f.free)(state);
                    return match (code, std::result::Result::from(r)) {
                        (1, Ok(b)) => Ok(b.iter().copied().collect()),
                        (_, Err(e)) => Err(e[..].to_string()),
                        _ => Err("ffi poll error but take ok".into()),
                    };
                }
            }
        }
    }

    fn ok_json(f: FfiFuture) -> Value {
        let b = drive(f).expect("op should succeed");
        serde_json::from_slice(&b).expect("ok payload must be JSON")
    }

    fn err_json(f: FfiFuture) -> Value {
        let e = drive(f).expect_err("op should fail");
        serde_json::from_str(&e).expect("Err 载荷必须是信封 JSON")
    }

    fn open_conn() -> u64 {
        boot();
        let v = ok_json(connect(RString::from("demo://t")));
        v["handle"].as_u64().expect("handle")
    }

    // ---- describe / 能力位（§15.2 与 §15.6 第 1 条）----

    #[test]
    fn describe_is_valid_capability_json() {
        let v: Value = serde_json::from_str(&describe_json()).unwrap();
        assert_eq!(v["dialect"], DIALECT, "dialect 不得为空且与 dialect() 同源");
        let caps: Vec<&str> = v["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c.as_str().unwrap())
            .collect();

        // 8 个闭集串（自校验，不依赖宿主 crate）。
        const KNOWN: [&str; 8] = [
            "placeholder_q",
            "placeholder_dollar",
            "placeholder_colon",
            "id_returning",
            "id_two_step",
            "id_none",
            "stream",
            "cancel",
        ];
        for c in &caps {
            assert!(KNOWN.contains(c), "未知能力串（宿主会 fail-loud）: {c}");
        }
        // 两组互斥各恰好一枚。
        let ph = caps
            .iter()
            .filter(|c| c.starts_with("placeholder_"))
            .count();
        let id = caps.iter().filter(|c| c.starts_with("id_")).count();
        assert_eq!(ph, 1, "placeholder 组必须恰好一枚");
        assert_eq!(id, 1, "id 组必须恰好一枚");
        assert_eq!(
            caps.contains(&"stream"),
            SUPPORTS_STREAM,
            "stream 声明必须与 SUPPORTS_STREAM 同源"
        );
        assert_eq!(
            caps.contains(&"cancel"),
            SUPPORTS_CANCEL,
            "cancel 声明必须与 SUPPORTS_CANCEL 同源"
        );
    }

    #[test]
    fn stream_open_behavior_matches_declaration() {
        // 声明与行为不许漂移：翻转 SUPPORTS_STREAM 常量后本用例仍须自洽。
        let h = open_conn();
        let v = ok_json(stream_open(
            h,
            RString::from("select 1"),
            RString::from("[]"),
        ));
        if SUPPORTS_STREAM {
            assert!(
                v["stream_id"].is_u64(),
                "声明 stream 就必须返回 stream_id: {v}"
            );
        } else {
            assert_eq!(
                v["unsupported"], true,
                "未声明 stream 必须返回 {{unsupported:true}} 让宿主回退: {v}"
            );
        }
        close(h);
    }

    #[test]
    fn cancel_never_claims_unsupported_capability() {
        let h = open_conn();
        let v = ok_json(stream_open(
            h,
            RString::from("select 1"),
            RString::from("[]"),
        ));
        if !SUPPORTS_STREAM {
            close(h);
            return;
        }
        let sid = v["stream_id"].as_u64().unwrap();
        let r = drive(stream_cancel(h, sid));
        if SUPPORTS_CANCEL {
            assert!(r.is_ok(), "声明 cancel 就必须实现: {r:?}");
        } else {
            let e = r.expect_err("未声明 cancel 时调用应报错");
            let v: Value = serde_json::from_str(&e).unwrap();
            assert_eq!(v["category"], "unsupported");
            assert_eq!(v["data"]["capability"], "cancel");
        }
        close(h);
    }

    // ---- 信封（§15.6 第 7 条）----

    #[test]
    fn envelope_has_all_five_fields_for_every_category() {
        const ALL: [Category; 8] = [
            Category::Syntax,
            Category::Constraint,
            Category::Timeout,
            Category::Connection,
            Category::Capacity,
            Category::Unsupported,
            Category::Cancelled,
            Category::Internal,
        ];
        for c in ALL {
            let s = envelope(c, false, Disposition::Keep, "db op: x", json!({"k": 1}));
            let v: Value = serde_json::from_str(&s).unwrap();
            assert_eq!(v["category"], c.as_str());
            assert_eq!(v["retryable"], false);
            assert_eq!(v["session_disposition"], "keep");
            assert_eq!(v["message"], "db op: x");
            assert_eq!(v["data"]["k"], 1);
            assert_eq!(v.as_object().unwrap().len(), 5, "信封字段集最多五个：{v}");
        }
    }

    #[test]
    fn foreign_scheme_is_unsupported_not_connection_error() {
        boot();
        let v = err_json(connect(RString::from("mysql://nope")));
        assert_eq!(v["category"], "unsupported");
        assert_eq!(v["data"]["scheme"], SCHEME);
    }

    // ---- 句柄 / 事务生命周期 ----

    #[test]
    fn closed_handle_is_connection_error() {
        let h = open_conn();
        close(h);
        let v = err_json(query(h, RString::from("select 1"), RString::from("[]")));
        assert_eq!(v["category"], "connection");
        assert_eq!(v["session_disposition"], "close");
    }

    #[test]
    fn query_echoes_bound_params_in_order() {
        let h = open_conn();
        let v = ok_json(query(
            h,
            RString::from("select ? , ?"),
            RString::from(r#"[1,"two",null]"#),
        ));
        assert_eq!(v[0]["params"], json!([1, "two", null]));
        close(h);
    }

    #[test]
    fn tx_lifecycle_and_use_after_commit() {
        let h = open_conn();
        let tx = ok_json(begin(h))["tx_id"].as_u64().unwrap();
        assert!(
            drive(tx_exec(
                h,
                tx,
                RString::from("insert ..."),
                RString::from("[]")
            ))
            .is_ok()
        );
        assert!(drive(tx_commit(h, tx)).is_ok());
        // 提交后再用同一 tx_id：必须是可诊断的错误，而不是静默跑在自动提交上。
        let v = err_json(tx_query(
            h,
            tx,
            RString::from("select 1"),
            RString::from("[]"),
        ));
        assert_eq!(v["category"], "internal");
        assert_eq!(v["data"]["tx_id"], tx);
        close(h);
    }

    // ---- 流式协议（§15.6 第 4/5 条）----

    #[test]
    fn cursor_batches_by_100_and_done_is_idempotent() {
        let rows: Vec<Value> = (0..250).map(|i| json!({ "i": i })).collect();
        let mut c = Cursor {
            rows,
            pos: 0,
            cancelled: false,
        };
        let b1: Value = serde_json::from_str(&c.next_batch()).unwrap();
        assert_eq!(b1["rows"].as_array().unwrap().len(), STREAM_BATCH);
        let b2: Value = serde_json::from_str(&c.next_batch()).unwrap();
        assert_eq!(b2["rows"].as_array().unwrap().len(), STREAM_BATCH);
        let b3: Value = serde_json::from_str(&c.next_batch()).unwrap();
        assert_eq!(b3["rows"].as_array().unwrap().len(), 50);
        // 结束语必须带 done 键——裸 null 会与「单列 null 行」冲突；且 done 幂等。
        assert_eq!(c.next_batch(), json!({ "done": true }).to_string());
        assert_eq!(c.next_batch(), json!({ "done": true }).to_string());
    }

    #[test]
    fn stream_rows_match_query_and_cancel_then_close() {
        // 本用例专测流式协议；未声明 stream 能力时由
        // `stream_open_behavior_matches_declaration` 覆盖回退分支。
        if !SUPPORTS_STREAM {
            return;
        }
        let h = open_conn();
        let q = ok_json(query(
            h,
            RString::from("select * from t where id = :id"),
            RString::from("[7]"),
        ));

        let sid = ok_json(stream_open(
            h,
            RString::from("select * from t where id = :id"),
            RString::from("[7]"),
        ))["stream_id"]
            .as_u64()
            .unwrap();
        let batch = ok_json(stream_next(h, sid));
        // §15.6：流式与 db.query 必须逐值一致（否则同一 SQL 两个答案）。
        assert_eq!(batch["rows"], q, "stream 与 query 必须逐值一致");

        assert!(drive(stream_cancel(h, sid)).is_ok());
        let cancelled = ok_json(stream_next(h, sid));
        assert_eq!(
            cancelled["error"], "cancelled",
            "取消后必须回 {{error:cancelled}}: {cancelled}"
        );

        // close 必须能回收（cancel 不兼任清理）；二次 close 幂等。
        assert!(drive(stream_close(h, sid)).is_ok());
        assert!(drive(stream_close(h, sid)).is_ok());
        let gone = err_json(stream_next(h, sid));
        assert_eq!(gone["category"], "internal");
        close(h);
    }

    #[test]
    fn dialect_reports_unknown_for_missing_handle() {
        boot();
        let s = (dialect)(u64::MAX);
        assert_eq!(&s[..], "unknown", "缺失 handle 不得冒充真方言");
    }
}
