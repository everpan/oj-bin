//! db/DB(name) 数据访问绑定。
//!
//! db === DB("default")（引用相等由 bootstrap.js 侧的实例缓存保证），
//! 未配置的名字 DB(name) 返回 undefined。实例存在性检查在 Rust 侧（op_db_has）。
//! DataAccessor 的可变参数 args ...any 在 bridge 层从未被使用，故本版只传 SQL。
//!
//! 安全性：新增 `query_with_params` / `exec_with_params` 以支持绑定参数，杜绝 JS 侧字符串拼接
//! （原始 `query(sql)` 仅保留为无参便捷形式；真实 SQL 实现应优先用 *_with_params 或 query.rs 构造器）。

use std::cell::RefCell;
use std::collections::HashMap;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use deno_core::{OpState, op2};
use deno_error::JsErrorBox;
use futures::stream::Stream;
use serde_json::Value;
use tokio::sync::{Notify, mpsc};

use super::{BridgeResult, StableState};

/// 数据访问返回的单行（JSON 对象）。
pub type Row = Value;

/// SQL 方言（构造器按此选 sea_query QueryBuilder；裸 SQL 占位符方言归业务）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    Sqlite,
    MySql,
    Postgres,
}

/// DSN 前缀 → 方言。未知前缀归 Sqlite（非法 DSN 由装配层 fail-fast）。
pub fn dialect_of(dsn: &str) -> Dialect {
    if dsn.starts_with("mysql://") {
        Dialect::MySql
    } else if dsn.starts_with("postgres://") || dsn.starts_with("postgresql://") {
        Dialect::Postgres
    } else {
        Dialect::Sqlite
    }
}

/// 活跃事务会话（`DataAccessor::begin` 产出的运行期事务句柄）。
/// commit/rollback 取 `&self`（内部 take，二次调用报 "tx finished"）——
/// 会话存于 per-request 状态并被 `tokio::sync::Mutex` 串行化，不能按值交出。
#[async_trait]
pub trait TxSession: Send {
    async fn query(&self, sql: &str, params: &[Value]) -> BridgeResult<Vec<Row>>;
    async fn exec(&self, sql: &str, params: &[Value]) -> BridgeResult<i64>;
    async fn commit(&self) -> BridgeResult<()>;
    async fn rollback(&self) -> BridgeResult<()>;
}

/// 数据访问统一契约（接口隔离 + 依赖倒置）。
/// M0 用内存 fake；后续 sqlx 以同接口接入（`query_with_params` 参数化），handler 无需改动。
#[async_trait]
pub trait DataAccessor: Send + Sync {
    /// 库方言（构造器选 builder 用；默认 sqlite，fake/未知驱动走默认）。
    fn dialect(&self) -> Dialect {
        Dialect::Sqlite
    }

    /// 开启事务（不支持事务的 accessor 走默认 Err）。
    async fn begin(&self) -> BridgeResult<Box<dyn TxSession>> {
        let _: &Self = self;
        Err("transactions not supported by this accessor".into())
    }

    /// 无参查询（便捷形式）。
    async fn query(&self, sql: &str) -> BridgeResult<Vec<Row>> {
        self.query_with_params(sql, &[]).await
    }
    /// 无参执行（便捷形式）。
    async fn exec(&self, sql: &str) -> BridgeResult<i64> {
        self.exec_with_params(sql, &[]).await
    }
    /// 参数化查询（值经绑定，杜绝拼接注入）。
    async fn query_with_params(&self, sql: &str, params: &[Value]) -> BridgeResult<Vec<Row>>;
    /// 参数化执行，返回受影响行数。
    async fn exec_with_params(&self, sql: &str, params: &[Value]) -> BridgeResult<i64>;

    /// 流式查询：返回逐行流，调用方逐批/逐行消费，避免 `fetch_all` 全量入内存
    /// （大表导出 / 游标回传）。核心 `SqlxAccessor` / `InMemoryAccessor` 与 ABI 10 起
    /// 的 `FfiDataAccessor`（vtable `stream_open/next/cancel/close` 批量 pull）均实现；
    /// 未覆盖的进程内后端走默认实现报「backend does not support streaming」。
    /// 行以 `Result<Row, String>` 传递：Ok = 一行，Err = 流中途错误（pump 转为错误终态）。
    async fn stream_query(
        &self,
        _sql: &str,
        _params: &[Value],
    ) -> BridgeResult<Pin<Box<dyn Stream<Item = Result<Row, String>> + Send>>> {
        Err("backend does not support streaming".into())
    }
}

/// DataAccessor 的内存实现（fake）。接口与 sqlx 实现一致（Liskov 可替换）。
/// inner 用 Arc 共享：begin() 派生的假事务与其读写同一存储。
#[derive(Default)]
pub struct InMemoryAccessor {
    inner: Arc<RwLock<Inner>>,
}

#[derive(Default)]
struct Inner {
    rows: Vec<Row>,
    err: Option<String>,
}

impl InMemoryAccessor {
    pub fn new() -> Self {
        Self::default()
    }

    /// 预置数据（测试/演示用）。
    pub fn seed(&self, rows: impl IntoIterator<Item = Row>) {
        self.inner.write().unwrap().rows.extend(rows);
    }

    /// 注入查询错误（测试错误传播路径）。
    pub fn set_error(&self, msg: impl Into<String>) {
        self.inner.write().unwrap().err = Some(msg.into());
    }
}

#[async_trait]
impl DataAccessor for InMemoryAccessor {
    async fn begin(&self) -> BridgeResult<Box<dyn TxSession>> {
        Ok(Box::new(InMemoryTx {
            inner: self.inner.clone(),
        }))
    }

    async fn query_with_params(&self, _sql: &str, _params: &[Value]) -> BridgeResult<Vec<Row>> {
        let g = self.inner.read().unwrap();
        if let Some(e) = &g.err {
            return Err(e.clone().into());
        }
        Ok(g.rows.clone())
    }

    async fn exec_with_params(&self, _sql: &str, _params: &[Value]) -> BridgeResult<i64> {
        let g = self.inner.read().unwrap();
        if let Some(e) = &g.err {
            return Err(e.clone().into());
        }
        Ok(g.rows.len() as i64)
    }

    async fn stream_query(
        &self,
        _sql: &str,
        _params: &[Value],
    ) -> BridgeResult<Pin<Box<dyn Stream<Item = Result<Row, String>> + Send>>> {
        let g = self.inner.read().unwrap();
        if let Some(e) = &g.err {
            return Err(e.clone().into());
        }
        let rows = g.rows.clone();
        drop(g);
        Ok(Box::pin(futures::stream::iter(
            rows.into_iter().map(Ok::<Row, String>),
        )))
    }
}

/// 内存假事务：与父 accessor 读写同一 inner；commit/rollback no-op。
struct InMemoryTx {
    inner: Arc<RwLock<Inner>>,
}

#[async_trait]
impl TxSession for InMemoryTx {
    async fn query(&self, _sql: &str, _params: &[Value]) -> BridgeResult<Vec<Row>> {
        let g = self.inner.read().unwrap();
        if let Some(e) = &g.err {
            return Err(e.clone().into());
        }
        Ok(g.rows.clone())
    }

    async fn exec(&self, _sql: &str, _params: &[Value]) -> BridgeResult<i64> {
        let g = self.inner.read().unwrap();
        if let Some(e) = &g.err {
            return Err(e.clone().into());
        }
        Ok(g.rows.len() as i64)
    }

    async fn commit(&self) -> BridgeResult<()> {
        Ok(())
    }

    async fn rollback(&self) -> BridgeResult<()> {
        Ok(())
    }
}

/// DB(name) 存在性检查：bootstrap 的 DB 构造器用，未配置则 JS 侧返回 undefined。
#[op2(fast)]
pub fn op_db_has(state: &mut OpState, #[string] name: &str) -> bool {
    state.borrow::<Arc<StableState>>().dbs.contains_key(name)
}

/// 每请求活跃事务（存 ReqState；Mutex 串行并发 op，Arc 跨 await 共享——
/// 不得跨 await 持 OpState/RefCell 借用）。
pub struct ActiveTx {
    pub db: String,
    pub session: tokio::sync::Mutex<Box<dyn TxSession>>,
}

/// 当前请求的活跃事务句柄（无则 None）。借用即取即还。
pub(crate) fn current_tx(state: &Rc<RefCell<OpState>>) -> Option<Arc<ActiveTx>> {
    state.borrow().borrow::<super::ReqState>().tx.clone()
}

/// 查询/执行的目标：本库活跃事务 → 会话；他库活跃事务 → 报错；无 → 池。
pub(crate) enum Target {
    Tx(Arc<ActiveTx>),
    Pool(Arc<dyn DataAccessor>),
}

pub(crate) fn resolve_target(
    state: &Rc<RefCell<OpState>>,
    name: &str,
) -> Result<Target, JsErrorBox> {
    if let Some(t) = current_tx(state) {
        if t.db == name {
            return Ok(Target::Tx(t));
        }
        return Err(JsErrorBox::generic(format!(
            "transaction active on db '{}' (finish it before touching db '{name}')",
            t.db
        )));
    }
    Ok(Target::Pool(super::query::lookup(state, name)?))
}

/// PR-2 Phase A：`db.stream` 每请求流注册表（keyed by stream_id）。pump 持有 `Arc<DbStreamShared>`
/// 克隆推数据，`op_db_stream_next` 拉取；请求结束 `ReqState::reset` 换全新 Arc，旧 Arc 由仍存活的
/// pump 持有至排空，互不串号。
pub(crate) type DbStreamRegistry = tokio::sync::Mutex<HashMap<u32, Arc<DbStreamShared>>>;

/// 单条 db.stream 的共享状态：行/错误经有界通道推给接收端（`rx`）；通道关闭 = 流结束（done）。
/// 有界（`DB_STREAM_CHANNEL_CAP`）= 消费端不拉取时 pump 停在 send 上，abort 立即停止后端
/// 拉取（真取消；unbounded 会提前灌满结果集、取消扑空）。`abort` 由 `op_db_stream_abort`
/// 触发，pump 的 `select!` 观测后中止。
pub(crate) struct DbStreamShared {
    pub rx: tokio::sync::Mutex<mpsc::Receiver<Result<Row, String>>>,
    pub abort: Arc<Notify>,
}

/// pump → 消费端的行缓冲上限（有界背压；abort 后缓冲行继续交付 = "流提前结束"）。
const DB_STREAM_CHANNEL_CAP: usize = 64;

/// stream_id 全局自增（每请求注册表独立，跨请求重复无妨；u32 足以覆盖请求生命周期内的并发流）。
static NEXT_STREAM_ID: AtomicU32 = AtomicU32::new(1);

/// 取走并校验活跃事务（commit/rollback 收尾用；不匹配/缺失报错）。
fn take_tx(state: &Rc<RefCell<OpState>>, name: &str) -> Result<Arc<ActiveTx>, JsErrorBox> {
    let mut g = state.borrow_mut();
    let rs = g.borrow_mut::<super::ReqState>();
    match rs.tx.take() {
        Some(t) if t.db == name => Ok(t),
        Some(t) => {
            rs.tx = Some(t.clone()); // 放回（别人的事务不动）
            Err(JsErrorBox::generic(format!(
                "transaction belongs to db '{}', not '{name}'",
                t.db
            )))
        }
        None => Err(JsErrorBox::generic("no active transaction")),
    }
}

/// db.tx 开始（JS wrapper 调用）：嵌套（已有活跃事务）报错。
#[op2]
pub async fn op_db_tx_begin(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
) -> Result<bool, JsErrorBox> {
    if current_tx(&state).is_some() {
        return Err(JsErrorBox::generic(
            "transaction already active (nested tx not supported)",
        ));
    }
    let da = super::query::lookup(&state, &name)?;
    let session = da
        .begin()
        .await
        .map_err(|e| JsErrorBox::generic(e.to_string()))?;
    state.borrow_mut().borrow_mut::<super::ReqState>().tx = Some(Arc::new(ActiveTx {
        db: name,
        session: tokio::sync::Mutex::new(session),
    }));
    Ok(true)
}

/// db.tx 提交。
#[op2]
pub async fn op_db_tx_commit(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
) -> Result<bool, JsErrorBox> {
    let t = take_tx(&state, &name)?;
    t.session
        .lock()
        .await
        .commit()
        .await
        .map_err(|e| JsErrorBox::generic(e.to_string()))?;
    Ok(true)
}

/// db.tx 回滚。
#[op2]
pub async fn op_db_tx_rollback(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
) -> Result<bool, JsErrorBox> {
    let t = take_tx(&state, &name)?;
    t.session
        .lock()
        .await
        .rollback()
        .await
        .map_err(|e| JsErrorBox::generic(e.to_string()))?;
    Ok(true)
}

/// db.asSystem()：本请求以系统身份绕过租户防护（请求级，ReqState 重置即失效；
/// 每次调用记审计日志）。多租户 sql_guard 下的显式逃生口——系统任务/跨租户报表用，
/// 业务 handler 不得使用。
#[op2(fast)]
pub fn op_db_as_system(state: &mut OpState) -> bool {
    state.borrow_mut::<super::ReqState>().system = true;
    eprintln!("warn: db.asSystem() invoked (tenant guard bypass for this request)");
    true
}

/// db.asTenant(id)：匿名请求下由 handler **显式声明**本次查询的租户身份（v0.1.20）。
///
/// 与 `asSystem` 的边界（务必写进文档）：`asSystem` = 完全绕过租户防护（跨租户裸奔，
/// 仅系统任务）；`asTenant` = **仍然强制 tenant_id 条件**（构造器注入、裸 SQL 要求
/// mentions+param_has），只是身份由 handler 自己给 —— 公开页（anchor → workspace uuid
/// 查表派生）要的是后者。
///
/// 三道 fail-closed 门禁：① `tenant.allow_as_tenant` 显式开启（默认关，面向公开面且
/// id 无从校验，故不复用 asSystem 的无开关形态）；② 请求必须是**匿名请求**
/// （`RequestInfo.anonymous`，仅 server 的 anonymous_paths 豁免分支置位——WS 帧/任务桥/
/// 测试默认路径恒 false）；③ 当前尚无租户 id（防运行期切换身份）。
/// 调用记审计日志；`ReqState::reset` 后失效（请求级）。
// nofast：三道门禁的拒绝原因必须抛回 JS（fast op 无异常通道）。
#[op2(nofast)]
pub fn op_db_as_tenant(state: &mut OpState, #[string] id: String) -> Result<bool, JsErrorBox> {
    if id.trim().is_empty() {
        return Err(JsErrorBox::generic("db.asTenant: id must not be empty"));
    }
    // 平台无从校验 id 是否为真实租户（无租户目录可查）——安全完全取决于「服务端派生」
    // 这条红线，故默认关闭 + 审计日志。
    let allow = state.borrow::<Arc<super::StableState>>().allow_as_tenant;
    if !allow {
        return Err(JsErrorBox::generic(
            "db.asTenant is disabled: set tenant.allow_as_tenant=true to let anonymous handlers declare a tenant",
        ));
    }
    let rs = state.borrow_mut::<super::ReqState>();
    if !rs.req.anonymous || rs.req.tenant_id.is_some() {
        return Err(JsErrorBox::generic(
            "db.asTenant is only allowed on anonymous requests (anonymous_paths hit, no tenant header)",
        ));
    }
    eprintln!(
        "warn: db.asTenant({id}) invoked (anonymous request; tenant must be derived server-side)"
    );
    rs.req.tenant_id = Some(id);
    Ok(true)
}

/// db.query(sql, params?)：Promise<Row[]>。params 可选（无参便捷形式）。
#[op2]
#[serde]
pub async fn op_db_query(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[string] sql: String,
    #[serde] params: Option<Vec<Value>>,
) -> Result<Vec<Row>, JsErrorBox> {
    let params = params.unwrap_or_default();
    super::guard::check_raw(&state, &sql)?; // 表归属守卫（§5.3，无模块上下文不设防）
    super::guard::check_tenant_raw(&state, &sql, &params)?; // 多租户防护（独立于 module_ctx）
    let mut rows = match resolve_target(&state, &name)? {
        Target::Pool(da) => da
            .query_with_params(&sql, &params)
            .await
            .map_err(|e| JsErrorBox::generic(e.to_string()))?,
        Target::Tx(t) => t
            .session
            .lock()
            .await
            .query(&sql, &params)
            .await
            .map_err(|e| JsErrorBox::generic(e.to_string()))?,
    };
    // 出口护栏：超界整数降十进制字符串，否则 JS 侧拿到 BigInt、json.ok 必 500（见 jsnum）。
    super::jsnum::sanitize_rows(&mut rows);
    Ok(rows)
}

/// db.exec(sql, params?)：Promise<受影响行数>。
#[op2]
#[serde]
pub async fn op_db_exec(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[string] sql: String,
    #[serde] params: Option<Vec<Value>>,
) -> Result<i64, JsErrorBox> {
    let params = params.unwrap_or_default();
    super::guard::check_raw(&state, &sql)?; // 表归属守卫（§5.3，无模块上下文不设防）
    super::guard::check_tenant_raw(&state, &sql, &params)?; // 多租户防护（独立于 module_ctx）
    match resolve_target(&state, &name)? {
        Target::Pool(da) => da
            .exec_with_params(&sql, &params)
            .await
            .map_err(|e| JsErrorBox::generic(e.to_string())),
        Target::Tx(t) => t
            .session
            .lock()
            .await
            .exec(&sql, &params)
            .await
            .map_err(|e| JsErrorBox::generic(e.to_string())),
    }
}

/// db.stream 打开（PR-2 Phase A）：先 `guard::check_raw` / `check_tenant_raw`（open 前校验，同
/// `op_db_query`）→ 取 accessor → `stream_query` 开流 → spawn **脱离式 pump**（current_thread 运行时
/// 内）逐行推入通道；op 立即返回 stream_id。插件后端（FfiDataAccessor）走默认实现报错。
#[op2]
pub async fn op_db_stream_open(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[string] sql: String,
    #[serde] params: Option<Vec<Value>>,
) -> Result<f64, JsErrorBox> {
    let params = params.unwrap_or_default();
    super::guard::check_raw(&state, &sql)?; // 表归属守卫（§5.3，无模块上下文不设防）
    super::guard::check_tenant_raw(&state, &sql, &params)?; // 多租户防护（独立于 module_ctx）
    let da = match resolve_target(&state, &name)? {
        Target::Pool(da) => da,
        Target::Tx(_) => {
            return Err(JsErrorBox::generic(
                "db.stream within an active transaction is not supported (streaming queries run on the connection pool only)",
            ));
        }
    };
    let mut stream = da
        .stream_query(&sql, &params)
        .await
        .map_err(|e| JsErrorBox::generic(e.to_string()))?;
    let (tx, rx) = mpsc::channel::<Result<Row, String>>(DB_STREAM_CHANNEL_CAP);
    let shared = Arc::new(DbStreamShared {
        rx: tokio::sync::Mutex::new(rx),
        abort: Arc::new(Notify::new()),
    });
    let id = NEXT_STREAM_ID.fetch_add(1, Ordering::SeqCst);
    let reg = state
        .borrow()
        .borrow::<super::ReqState>()
        .db_streams
        .clone();
    reg.lock().await.insert(id, shared.clone());
    // 脱离式 pump：监听流与取消信号，逐行推入通道（通道关闭 = done）。
    // **有界通道 + 阻塞投递**：消费端不拉取时 pump 停在 send 上——abort 一到即
    // 停止后端拉取（真取消）；unbounded 会把结果集提前灌满、取消扑空（探测
    // probe_stream_abort_releases_connection 抓到的回归）。
    let abort = shared.abort.clone();
    tokio::spawn(async move {
        use futures::StreamExt;
        loop {
            tokio::select! {
                biased;
                _ = abort.notified() => break,
                item = stream.next() => {
                    match item {
                        Some(Ok(row)) => { if tx.send(Ok(row)).await.is_err() { break; } }
                        Some(Err(e)) => { let _ = tx.send(Err(e)).await; break; }
                        None => break,
                    }
                }
            }
        }
        // tx 丢弃 → 接收端 recv 返回 None = 流结束（done）。abort 后缓冲中的
        // 已拉行继续交付（"流提前结束"），已交付行不回滚。
    });
    Ok(id as f64)
}

/// db.stream 拉取下一行（PR-2 Phase A）：返回一行（`Value::Object`）；流结束返回 `Value::Null`
/// （JS 侧作为终止哨兵，因行本身恒为非 null 对象）；途中错误以 `JsErrorBox` 抛出（JS `await` 即抛）。
#[op2]
#[serde]
pub async fn op_db_stream_next(
    state: Rc<RefCell<OpState>>,
    stream_id: f64,
) -> Result<Row, JsErrorBox> {
    let stream_id = stream_id as u32;
    let reg = state
        .borrow()
        .borrow::<super::ReqState>()
        .db_streams
        .clone();
    let shared = reg.lock().await.get(&stream_id).cloned().ok_or_else(|| {
        JsErrorBox::generic(format!(
            "db stream {stream_id} not found (already closed or never opened)"
        ))
    })?;
    let mut rx = shared.rx.lock().await;
    match rx.recv().await {
        Some(Ok(row)) => Ok(row),
        Some(Err(e)) => Err(JsErrorBox::generic(e)),
        None => Ok(Row::Null), // done
    }
}

/// db.stream 关闭（PR-2 Phase A）：触发中止并移除注册表条目（条目移除后 Arc 仍被 pump / 进行中的
/// next 持有至排空，无泄漏）；不强制 await，尽快归还。
#[op2]
pub async fn op_db_stream_close(state: Rc<RefCell<OpState>>, stream_id: f64) {
    let stream_id = stream_id as u32;
    let reg = state
        .borrow()
        .borrow::<super::ReqState>()
        .db_streams
        .clone();
    if let Some(s) = reg.lock().await.remove(&stream_id) {
        s.abort.notify_one();
    }
}

/// db.stream 主动中止（PR-2 Phase A）：由 bootstrap 在 `AbortSignal` 的 abort 事件上调用，
/// 触发 pump 中止（pump 经 `DbStreamShared.abort` 观测，回写错误终态）。
#[op2]
pub async fn op_db_stream_abort(state: Rc<RefCell<OpState>>, stream_id: f64) {
    let stream_id = stream_id as u32;
    let reg = state
        .borrow()
        .borrow::<super::ReqState>()
        .db_streams
        .clone();
    if let Some(s) = reg.lock().await.get(&stream_id) {
        s.abort.notify_one();
    }
}

/// 平台序列分配原语（v0.1.24，债务④）：`db.nextSeq(name)` → 下一个序号。
///
/// **要解决的问题**：业务用 `SELECT max(id) + 1` 取号在并发下会分配出相同序号（下游 U38 的
/// 事故链就始于这点）。本原语给出**单语句原子**的取号，调用方不再自担竞态。
///
/// 语义与边界：
/// - 平台表 `_oj_sequences(name, v)`，**首次使用自动建**（`create table if not exists`，不经
///   模块 schema / 迁移；`_oj_` 前缀避开业务命名空间，且未登记 → 裸 SQL 租户守卫短路放行）；
/// - 取号 SQL 按方言：
///   - PG / SQLite：`insert … on conflict(name) do update set v = v + 1 returning v`（单语句原子，
///     直接走池，不需要事务）；
///   - MySQL：`insert … values (?, last_insert_id(1)) on duplicate key update v = last_insert_id(v + 1)`
///     再 `select last_insert_id()`——**必须同一连接**（会话级变量），故无活跃事务时用一次短事务；
/// - **每库一次先确保**：序列表由 `ensure_seq_once` 在取号**之前**建好（DDL 走池、绝不进调用方
///   事务——MySQL 的 DDL 会隐式提交调用方事务；PG 里事务内失败语句会让事务进入 aborted 态，
///   后续一律 25P02）。池路径另留「失败→再建表→重试一次」兜底（表被外部 drop 等）；
///   事务路径**没有**兜底，靠 `ensure_seq_once` 先行保证（架构评审 P1-2）；
/// - 与调用方事务的关系：在 `db.tx` 内调用则搭车（推荐）。注意序列值**不随调用方回滚而回退**
///   ——按「只增不复用」理解；
/// - 返回值过 `jsnum` 规则：`≤2^53-1` 给 number，超出给十进制字符串（与 DB 读值同契约）；
/// - 序列名绑定为参数（不进 SQL 标识符），无注入面；长度 1..=128。
#[op2]
#[serde]
pub async fn op_db_next_seq(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[string] seq: String,
) -> Result<Row, JsErrorBox> {
    if seq.is_empty() || seq.len() > 128 {
        return Err(JsErrorBox::generic(
            "db.nextSeq: name must be 1..=128 characters",
        ));
    }
    let err = |e: String| JsErrorBox::generic(e);
    let da = super::query::lookup(&state, &name)?;
    // ① **先确保序列表存在**（每库一次，Bridge 级缓存）。DDL 走池、绝不在调用方事务里做：
    //    MySQL 的 DDL 会隐式提交调用方事务；PG 里事务内失败语句会让事务进入 aborted 态
    //    （后续一律 25P02）——若把建表放在「先试后建」的失败路径上，**首次在 db.tx 里用新序列名
    //    必然失败并毒化调用方事务**（架构评审 P1-2）。
    //    键用**物理库名**（`bound_db` 的结果）而非 JS 可见名：同一 Bridge 下不同模块可把字面
    //    `"default"` 绑到不同物理库（`manifest db:` / `db_override`），用可见名当键会让后一个库
    //    命中前一个库的缓存、跳过建表（池路径多付一次失败+DDL，事务路径直接失败）。
    let phys = super::guard::bound_db(&state, &name);
    ensure_seq_once(&state, &phys, &*da).await.map_err(err)?;
    let mut out = match resolve_target(&state, &name)? {
        // 池路径：表刚确保过，正常一次成功；仍保留「失败→再建表→重试」兜底（外部 drop 等）。
        Target::Pool(da) => next_seq_via_pool(&da, &seq).await.map_err(err)?,
        // tx 路径：搭车调用方事务的连接（建表已在池上完成，此处不再触碰 DDL）。
        Target::Tx(t) => {
            let dial = da.dialect();
            let mut s = t.session.lock().await;
            seq_next(&mut **s, dial, &seq).await.map_err(err)?
        }
    };
    // 出口护栏：超界整数降十进制字符串（与 DB 读值同契约，见 jsnum）。
    super::jsnum::sanitize_js_numbers(&mut out);
    Ok(out)
}

/// 每个库**一次**地确保平台序列表存在（Bridge 级缓存；DDL 幂等）。
///
/// 缓存放在 `StableState`（而非全局 static）：每个 Bridge 对应一份 DB 配置与池，全局缓存会在
/// 「同一进程里多个独立库（测试的内存库尤甚）」之间误判。并发首用时两个调用各跑一次
/// `create table if not exists` 也无害（幂等）。
async fn ensure_seq_once(
    state: &Rc<RefCell<OpState>>,
    name: &str,
    da: &dyn DataAccessor,
) -> Result<(), String> {
    // 持锁期间 panic 不能让这个 Bridge 级缓存**永久中毒**（否则此后每次 `nextSeq` 都 panic）。
    // 这里只是 HashSet 查/插，`PoisonError::into_inner()` 取回的集合本身仍是自洽的。
    if state
        .borrow()
        .borrow::<Arc<super::StableState>>()
        .seq_ensured
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(name)
    {
        return Ok(());
    }
    ensure_seq_table(da).await?;
    state
        .borrow()
        .borrow::<Arc<super::StableState>>()
        .seq_ensured
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(name.to_string());
    Ok(())
}

/// 池路径取号：先试一次，失败（表不存在）才建表并重试。
async fn next_seq_via_pool(da: &Arc<dyn DataAccessor>, seq: &str) -> Result<Row, String> {
    let dial = da.dialect();
    match next_seq_once_pool(da, dial, seq).await {
        Ok(v) => Ok(v),
        Err(e) => {
            ensure_seq_table(&**da).await?;
            next_seq_once_pool(da, dial, seq)
                .await
                .map_err(|e2| format!("{e}; retry after create table failed: {e2}"))
        }
    }
}

/// 池路径的一次取号：PG/SQLite 单语句直走池；MySQL 两条语句包一次短事务（同连接）。
async fn next_seq_once_pool(
    da: &Arc<dyn DataAccessor>,
    dial: Dialect,
    seq: &str,
) -> Result<Row, String> {
    if dial == Dialect::MySql {
        let mut s = da.begin().await.map_err(|e| e.to_string())?;
        let v = seq_next(&mut *s, dial, seq).await?;
        s.commit().await.map_err(|e| e.to_string())?;
        Ok(v)
    } else {
        let rows = da
            .query_with_params(insert_sql(dial), &[Value::String(seq.to_string())])
            .await
            .map_err(|e| e.to_string())?;
        pick_v(rows)
    }
}

/// 序列表 DDL（幂等；按方言给 varchar/text 与 bigint）。
async fn ensure_seq_table(da: &dyn DataAccessor) -> Result<(), String> {
    let ddl = match da.dialect() {
        Dialect::MySql => {
            "create table if not exists _oj_sequences (name varchar(128) primary key, v bigint not null)"
        }
        _ => "create table if not exists _oj_sequences (name text primary key, v bigint not null)",
    };
    da.exec_with_params(ddl, &[])
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// 取号 SQL（PG/SQLite；MySQL 是两条语句，见 `seq_next`）。
fn insert_sql(dial: Dialect) -> &'static str {
    if dial == Dialect::Postgres {
        "insert into _oj_sequences (name, v) values ($1, 1) \
         on conflict (name) do update set v = _oj_sequences.v + 1 returning v"
    } else {
        "insert into _oj_sequences (name, v) values (?, 1) \
         on conflict (name) do update set v = v + 1 returning v"
    }
}

/// 单次取号（在给定会话内跑完；MySQL 是两条语句，其余一条 `returning`）。
async fn seq_next(s: &mut dyn TxSession, dialect: Dialect, seq: &str) -> Result<Row, String> {
    let p = vec![Value::String(seq.to_string())];
    let rows = if dialect == Dialect::MySql {
        s.exec(
            "insert into _oj_sequences (name, v) values (?, last_insert_id(1)) \
             on duplicate key update v = last_insert_id(v + 1)",
            &p,
        )
        .await
        .map_err(|e| e.to_string())?;
        s.query("select last_insert_id() as v", &[])
            .await
            .map_err(|e| e.to_string())?
    } else {
        s.query(insert_sql(dialect), &p)
            .await
            .map_err(|e| e.to_string())?
    };
    pick_v(rows)
}

/// 从返回行里取 `v` 字段（缺列/空结果 → 报错）。
fn pick_v(rows: Vec<Row>) -> Result<Row, String> {
    rows.into_iter()
        .next()
        .and_then(|r| r.get("v").cloned())
        .ok_or_else(|| "db.nextSeq: no value returned".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::{Bridge, InMemoryAccessor, InMemoryKV, SchemaRegistry};
    use serde_json::json;
    use std::sync::Arc;

    /// `db.nextSeq`：单语句原子取号（sqlite 离线；PG/MySQL 的真库并发见 env-gated 用例）。
    #[tokio::test(flavor = "current_thread")]
    async fn next_seq_allocates_densely_and_creates_table() {
        let db = crate::bridge::SqlxAccessor::arc("sqlite::memory:")
            .await
            .unwrap();
        let b = Bridge::with_dbs_and_loader(
            std::collections::HashMap::from([("default".to_string(), db as _)]),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            None,
            Default::default(),
        );
        let cap = b
            .run_with(
                r#"(async () => {
                     const a = [];
                     for (let i = 0; i < 3; i++) a.push(await db.nextSeq("proj"));
                     const other = await db.nextSeq("other");
                     // 事务内取号：搭车同一连接，且不随回滚而回退（序列语义）
                     const inTx = await db.tx(async (tx) => {
                       const x = await tx.nextSeq("proj");
                       return x;
                     });
                     json.ok({ a, other, inTx });
                   })().catch(e => json.fail(500, String(e)));"#,
                crate::bridge::RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(v["data"]["a"], json!([1, 2, 3]), "同名序列稠密递增：{v}");
        assert_eq!(v["data"]["other"], json!(1), "不同序列互不干扰：{v}");
        assert_eq!(v["data"]["inTx"], json!(4), "事务内搭车同一序列：{v}");
    }

    /// 并发取号不重号（sqlite 单连接串行化；真库并发见 PG/MySQL env-gated）。
    #[tokio::test(flavor = "current_thread")]
    async fn next_seq_is_concurrency_safe_offline() {
        let db = crate::bridge::SqlxAccessor::arc("sqlite::memory:")
            .await
            .unwrap();
        let b = Bridge::with_dbs_and_loader(
            std::collections::HashMap::from([("default".to_string(), db as _)]),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            None,
            Default::default(),
        );
        let cap = b
            .run_with(
                r#"(async () => {
                     const ps = [];
                     for (let i = 0; i < 25; i++) ps.push(db.nextSeq("burst"));
                     const vs = await Promise.all(ps);
                     vs.sort((x, y) => x - y);
                     let dense = true;
                     for (let i = 0; i < vs.length; i++) if (vs[i] !== i + 1) dense = false;
                     json.ok({ n: vs.length, dense, first: vs[0], last: vs[vs.length - 1] });
                   })().catch(e => json.fail(500, String(e)));"#,
                crate::bridge::RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(v["data"]["n"], json!(25), "{v}");
        assert_eq!(v["data"]["dense"], json!(true), "并发取号应稠密无重复：{v}");
    }

    // 真库并发取号**不在本文件测**：经由 deno_core 连发 op 会撞既有缺陷
    // （见 CHANGELOG「已知债」：并发超过 sqlx 池上限 / await 后再发 op → op 驱动
    // `RefCell already borrowed` abort）。原子性改由插件层真库用例证明：
    // `plugins/oj-db-postgres` 的 `real_postgres_next_seq_is_atomic_under_concurrency`
    // 与 `plugins/oj-db-mysql` 的 `real_mysql_next_seq_*`（env-gated）。

    /// 债④回归（env-gated，真库）：**在 `db.tx` 内首次使用新序列名**必须成功，
    /// 且不得把调用方事务搞成 aborted（架构评审 P1-2 的验收）。
    ///
    /// 背景：早期实现是「先试 → 失败才建表 → 在同一事务会话上重试」。PG 里事务内任何失败语句都会
    /// 让事务进入 aborted 态（后续一律 `25P02`），于是**首次在 tx 内取号必然失败并毒化调用方事务**。
    /// 现改为「每库一次的先确保（DDL 走池）」——本用例先 `drop table` 强制走首次路径来钉住它。
    #[tokio::test(flavor = "multi_thread")]
    async fn next_seq_first_use_inside_tx_on_real_db() {
        let url = std::env::var("OJ_TEST_PG")
            .or_else(|_| std::env::var("OJ_TEST_MYSQL"))
            .unwrap_or_else(|_| {
                eprintln!("skip: OJ_TEST_PG / OJ_TEST_MYSQL unset");
                String::new()
            });
        if url.is_empty() {
            return;
        }
        let db = crate::bridge::SqlxAccessor::arc(&url).await.unwrap();
        // 清场：删掉平台序列表，强制本用例走「首次使用（含建表）」路径。
        let _ = db
            .exec_with_params("drop table if exists _oj_sequences", &[])
            .await;
        let b = Bridge::with_dbs_and_loader(
            std::collections::HashMap::from([("default".to_string(), db as _)]),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            None,
            Default::default(),
        );
        let cap = b
            .run_with(
                r#"(async () => {
                     const r = await db.tx(async (tx) => {
                       const a = await tx.nextSeq("in_tx_first");   // 首次：建表在池上完成
                       const b = await tx.nextSeq("in_tx_first");   // 同一事务内再取一次
                       return { a, b };
                     });
                     json.ok(r);
                   })().catch(e => json.fail(500, String(e)));"#,
                crate::bridge::RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "tx 内首次取号必须成功（不得毒化事务）：{v}");
        assert_eq!(v["data"]["a"], json!(1), "{v}");
        assert_eq!(v["data"]["b"], json!(2), "{v}");
    }

    /// PR-2 Phase A 真库回归（env-gated）：`db.stream` 在真实 PG / MySQL 上的逐行结果，与
    /// `db.query` 全量**一致**（顺序 + 内容），且流走完哨兵正常、无残留注册表条目。
    /// 仅当 `OJ_TEST_PG` / `OJ_TEST_MYSQL` 设了 DSN 才跑；都未设则 skip。
    #[tokio::test(flavor = "multi_thread")]
    async fn stream_matches_query_on_real_db() {
        let pg = std::env::var("OJ_TEST_PG").ok().filter(|s| !s.is_empty());
        let my = std::env::var("OJ_TEST_MYSQL")
            .ok()
            .filter(|s| !s.is_empty());
        if pg.is_none() && my.is_none() {
            eprintln!("skip: OJ_TEST_PG / OJ_TEST_MYSQL unset");
            return;
        }
        if let Some(url) = pg {
            stream_eq_check(&url, "pg").await;
        }
        if let Some(url) = my {
            stream_eq_check(&url, "mysql").await;
        }
    }

    /// 单库 `db.stream` == `db.query` 全量校验（真库回归辅助）。
    async fn stream_eq_check(url: &str, tag: &str) {
        let db = crate::bridge::SqlxAccessor::arc(url).await.unwrap();
        let b = Bridge::with_dbs_and_loader(
            std::collections::HashMap::from([("default".to_string(), db as _)]),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            None,
            Default::default(),
        );
        let cap = b
            .run_with(
                r#"(async () => {
                     await db.exec("drop table if exists _oj_stream_real");
                     await db.exec("create table _oj_stream_real (id integer, name text)");
                     // 内联受控值（int + 简单字符串）规避 PG 的 ? 占位符问题；_oj_ 前缀表免登记表白名单。
                     for (let i = 0; i < 20; i++) await db.exec("insert into _oj_stream_real (id, name) values (" + i + ", 'n" + i + "')");
                     const all = await db.query("select id, name from _oj_stream_real order by id");
                     const streamed = [];
                     for await (const row of db.stream("select id, name from _oj_stream_real order by id")) streamed.push(row);
                     json.ok({ n1: all.length, n2: streamed.length, eq: JSON.stringify(all) === JSON.stringify(streamed) });
                   })().catch(e => json.fail(500, String(e)));"#,
                crate::bridge::RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "[{tag}] db.stream real-db failed: {v}");
        assert_eq!(v["data"]["n1"], json!(20), "[{tag}] {v}");
        assert_eq!(v["data"]["n2"], json!(20), "[{tag}] {v}");
        assert_eq!(
            v["data"]["eq"],
            json!(true),
            "[{tag}] db.stream must equal db.query on real db: {v}"
        );
    }

    /// 序列名越界（空 / 超 128）→ 明确报错。
    #[tokio::test(flavor = "current_thread")]
    async fn next_seq_rejects_bad_name() {
        let db = crate::bridge::SqlxAccessor::arc("sqlite::memory:")
            .await
            .unwrap();
        let b = Bridge::with_dbs_and_loader(
            std::collections::HashMap::from([("default".to_string(), db as _)]),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            None,
            Default::default(),
        );
        let cap = b
            .run_with(
                r#"(async () => {
                     const e1 = await db.nextSeq("").then(() => "ok", e => String(e));
                     const e2 = await db.nextSeq("x".repeat(129)).then(() => "ok", e => String(e));
                     json.ok({ e1, e2 });
                   })().catch(e => json.fail(500, String(e)));"#,
                crate::bridge::RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert!(v["data"]["e1"].as_str().unwrap().contains("1..=128"), "{v}");
        assert!(v["data"]["e2"].as_str().unwrap().contains("1..=128"), "{v}");
    }

    #[test]
    fn dialect_of_recognizes_prefixes() {
        assert_eq!(dialect_of("sqlite://x"), Dialect::Sqlite);
        assert_eq!(dialect_of("sqlite::memory:"), Dialect::Sqlite);
        assert_eq!(dialect_of("mysql://u@h/d"), Dialect::MySql);
        assert_eq!(dialect_of("postgres://h/d"), Dialect::Postgres);
        assert_eq!(dialect_of("postgresql://h/d"), Dialect::Postgres);
        // 未知前缀回落 sqlite
        assert_eq!(dialect_of("weird://x"), Dialect::Sqlite);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn query_propagates_accessor_error() {
        let db = Arc::new(InMemoryAccessor::new());
        db.seed([json!({"id": 1})]);
        db.set_error("boom");
        let b = Bridge::new(db, Arc::new(InMemoryKV::new()));
        let cap = b
            .run(r#"db.query("select 1").then(r => json.ok(r)).catch(e => json.fail(500, String(e)));"#)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 500);
        assert!(v["msg"].as_str().unwrap().contains("boom"), "{v}");

        // exec 同理
        let cap = b
            .run(r#"db.exec("delete from t").then(r => json.ok(r)).catch(e => json.fail(500, String(e)));"#)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 500);
        assert!(v["msg"].as_str().unwrap().contains("boom"), "{v}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn commit_without_active_tx_errors() {
        use crate::bridge::ReqState;
        use deno_core::OpState;
        // 无活跃事务时收尾（take_tx 缺失分支）→ 直接报 "no active transaction"。
        // commit/rollback 共用 take_tx，仅需 ReqState，不依赖任何 DataAccessor。
        let mut op_state = OpState::new(None::<deno_core::OpStackTraceCallback>);
        op_state.put(ReqState::default());
        let state = std::rc::Rc::new(std::cell::RefCell::new(op_state));
        let r = take_tx(&state, "default");
        assert!(r.is_err(), "expected error when no tx active");
        assert!(
            r.err()
                .unwrap()
                .to_string()
                .contains("no active transaction"),
            "expected no-active-tx error"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cross_db_query_during_tx_rejected() {
        let def = Arc::new(InMemoryAccessor::new());
        def.seed([json!({"id": 1})]);
        let other = Arc::new(InMemoryAccessor::new());
        let b = Bridge::with_dbs(
            std::collections::HashMap::from([
                ("default".to_string(), def as Arc<dyn DataAccessor>),
                ("other".to_string(), other as Arc<dyn DataAccessor>),
            ]),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new().table("t", &["id"], &["id"]),
            false,
        );
        // 在 default 上开事务，再于事务内查询 other → 拒绝。
        let cap = b
            .run(
                r#"
                db.tx(async () => {
                  await DB("other").query("select 1");
                }).then(() => json.ok({})).catch(e => json.fail(409, String(e)));
            "#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 409, "{v}");
        assert!(
            v["msg"]
                .as_str()
                .unwrap()
                .contains("transaction active on db 'default'"),
            "{v}"
        );
    }

    // PR-2 Phase A：未覆盖 stream_query 的 accessor（如插件 FfiDataAccessor）走默认实现，
    // 明确报错「backend does not support streaming (ABI 10 required)」。
    struct NoStreamAccessor;
    #[async_trait]
    impl DataAccessor for NoStreamAccessor {
        async fn query_with_params(&self, _sql: &str, _params: &[Value]) -> BridgeResult<Vec<Row>> {
            Ok(vec![])
        }
        async fn exec_with_params(&self, _sql: &str, _params: &[Value]) -> BridgeResult<i64> {
            Ok(0)
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn default_stream_query_reports_unsupported() {
        let r = NoStreamAccessor.stream_query("select 1", &[]).await;
        assert!(r.is_err(), "未覆盖的 accessor 必须报错");
        let msg = r.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(
            msg.contains("does not support streaming"),
            "错误文案须指向 ABI 10: {msg}"
        );
    }

    /// sqlite 流式结果与 `db.query` 全量一致（顺序/内容）。
    #[tokio::test(flavor = "current_thread")]
    async fn sqlite_stream_matches_query_full_result() {
        let db = crate::bridge::SqlxAccessor::arc("sqlite::memory:")
            .await
            .unwrap();
        let b = Bridge::with_dbs_and_loader(
            std::collections::HashMap::from([("default".to_string(), db as _)]),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            None,
            Default::default(),
        );
        let cap = b
            .run_with(
                r#"(async () => {
                     await db.exec("create table t(id integer, name text)");
                     for (let i = 0; i < 10; i++) await db.exec("insert into t values (?, ?)", [i, "n" + i]);
                     const all = await db.query("select id, name from t order by id");
                     const streamed = [];
                     for await (const row of db.stream("select id, name from t order by id")) streamed.push(row);
                     json.ok({ n1: all.length, n2: streamed.length, eq: JSON.stringify(all) === JSON.stringify(streamed) });
                   })().catch(e => json.fail(500, String(e)));"#,
                crate::bridge::RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(v["data"]["n1"], json!(10), "{v}");
        assert_eq!(v["data"]["n2"], json!(10), "{v}");
        assert_eq!(v["data"]["eq"], json!(true), "流式结果与全量查询一致：{v}");
    }

    /// InMemoryAccessor 流式返回已 seed 的行（dev/fake 路径）。
    #[tokio::test(flavor = "current_thread")]
    async fn inmemory_stream_yields_seeded_rows() {
        let acc = Arc::new(InMemoryAccessor::new());
        acc.seed(vec![
            json!({"id": 1, "name": "a"}),
            json!({"id": 2, "name": "b"}),
        ]);
        let b = Bridge::with_dbs_and_loader(
            std::collections::HashMap::from([(
                "default".to_string(),
                acc as Arc<dyn DataAccessor>,
            )]),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            None,
            Default::default(),
        );
        let cap = b
            .run_with(
                r#"(async () => {
                     const rows = [];
                     for await (const r of db.stream("ignored")) rows.push(r);
                     json.ok({ n: rows.length, first: rows[0] });
                   })().catch(e => json.fail(500, String(e)));"#,
                crate::bridge::RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(v["data"]["n"], json!(2), "{v}");
        assert_eq!(v["data"]["first"]["id"], json!(1), "{v}");
    }

    // ===== 防护探测（probe）：守卫/资源回收不能只靠"插旗"声明，用对抗用例钉死 =====

    /// 探测辅助：跑一段 JS，返回信封 data（code 必须 0）。
    async fn run_probe(b: &Bridge, req: crate::bridge::RequestInfo, js: String) -> Value {
        let cap = b.run_with(&js, req).await.unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        v["data"].clone()
    }

    fn stream_probe_js(sql: &str, params: &str) -> String {
        format!(
            r#"(async () => {{
                 try {{
                   const rows = [];
                   for await (const r of db.stream({sql}, {params})) rows.push(r);
                   json.ok({{ n: rows.length }});
                 }} catch (e) {{ json.ok({{ err: String(e) }}); }}
               }})().catch(e => json.fail(500, String(e)));"#
        )
    }

    /// 探测：租户守卫对 db.stream 必须与 db.query **同严**（Deny 模式四态全打）。
    /// 若流式路径在守卫上抄近道（open 前漏 check_tenant_raw），下面至少一条红。
    #[tokio::test(flavor = "current_thread")]
    async fn probe_stream_tenant_guard_deny_four_states() {
        let db = crate::bridge::SqlxAccessor::arc("sqlite::memory:")
            .await
            .unwrap();
        db.exec_with_params(
            "create table t (id integer primary key, name text, tenant_id text)",
            &[],
        )
        .await
        .unwrap();
        db.exec_with_params(
            "insert into t (id, name, tenant_id) values (1,'a','t1'),(2,'b','t2')",
            &[],
        )
        .await
        .unwrap();
        let b = Bridge::with_dbs_and_loader(
            std::collections::HashMap::from([("default".to_string(), db as _)]),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new().table("t", &["id"], &["id", "name", "tenant_id"]),
            false,
            None,
            crate::bridge::Extras {
                sql_guard: crate::bridge::SqlGuard::Deny,
                ..Default::default()
            },
        );
        let run = |req, sql: &str, params: &str| {
            let js = stream_probe_js(sql, params);
            let b = &b;
            async move { run_probe(b, req, js).await }
        };
        let t1 = crate::bridge::RequestInfo {
            tenant_id: Some("t1".into()),
            ..Default::default()
        };
        // ① 租户受约束表、SQL 无 tenant_id 条件 → open 前拒绝（游标不得建立）
        let d = run(t1.clone(), r#""select * from t""#, "null").await;
        assert!(
            d["err"]
                .as_str()
                .unwrap_or_default()
                .contains("lacks tenant_id"),
            "① 流式必须同样吃租户守卫: {d}"
        );
        // ② 无租户头（含受约束表）→ require tenant context
        let d = run(
            crate::bridge::RequestInfo::default(),
            r#""select * from t where tenant_id = 't1'""#,
            "null",
        )
        .await;
        assert!(
            d["err"]
                .as_str()
                .unwrap_or_default()
                .contains("require tenant context"),
            "② 无租户头必须拒绝: {d}"
        );
        // ③ 条件 + 参数租户匹配 → 通过，且只见本租户的行
        let d = run(
            t1.clone(),
            r#""select id from t where tenant_id = ?""#,
            r#"["t1"]"#,
        )
        .await;
        assert_eq!(d["n"], json!(1), "③ 合法流式须放行且只见本租户行: {d}");
        // ④ 条件有、参数租户不符（deny 模式）→ 拒
        let d = run(t1, r#""select id from t where tenant_id = ?""#, r#"["t2"]"#).await;
        assert!(
            d["err"]
                .as_str()
                .unwrap_or_default()
                .contains("must include current tenant id"),
            "④ 参数租户不符必须拒绝: {d}"
        );
    }

    /// 探测：中途 abort 后连接必须回池——sqlite 池 max_connections(1)，游标泄漏 =
    /// 后续同池查询挂死。整条 run_with 套超时哨兵：泄漏即红（不靠人眼看日志）。
    #[tokio::test(flavor = "current_thread")]
    async fn probe_stream_abort_releases_connection() {
        let db = crate::bridge::SqlxAccessor::arc("sqlite::memory:")
            .await
            .unwrap();
        let b = Bridge::with_dbs_and_loader(
            std::collections::HashMap::from([("default".to_string(), db as _)]),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            None,
            Default::default(),
        );
        let cap = tokio::time::timeout(std::time::Duration::from_secs(15), async {
            b.run_with(
                r#"(async () => {
                     await db.exec("create table t(id integer)");
                     for (let i = 0; i < 50; i++) await db.exec("insert into t values (?)", [i]);
                     const ac = new AbortController();
                     let n = 0;
                     try {
                       await db.stream("select id from t order by id", null, {
                         signal: ac.signal,
                         onRow: (r) => { n++; if (n === 3) ac.abort(); },
                       });
                     } catch (e) { /* 取消 = 流提前终止（拒绝或正常结束皆可） */ }
                     // abort 后同池再来一发：连接没回池这里挂死（max_connections(1) 哨兵）
                     const after = await db.query("select count(*) as c from t");
                     json.ok({ streamed: n, after: after[0].c });
                   })().catch(e => json.fail(500, String(e)));"#,
                crate::bridge::RequestInfo::default(),
            )
            .await
            .unwrap()
        })
        .await
        .expect("abort 后同池查询挂死 = 连接泄漏（sqlite max_connections(1) 哨兵）");
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        let n = v["data"]["streamed"].as_u64().unwrap();
        assert!((3..=50).contains(&n), "abort 须提前终止（n={n}）: {v}");
        assert_eq!(v["data"]["after"], json!(50), "abort 后连接须可用: {v}");
    }

    /// 探测：并发游标交叉——每请求流注册表按 stream_id 隔离，两个游标交错推进
    /// 互不串行（若注册表被复用/串号，这里序列即错）。
    #[tokio::test(flavor = "current_thread")]
    async fn probe_stream_concurrent_cursors_interleave() {
        let acc = Arc::new(InMemoryAccessor::new());
        acc.seed(vec![json!({"id": 1}), json!({"id": 2})]);
        let b = Bridge::with_dbs_and_loader(
            std::collections::HashMap::from([(
                "default".to_string(),
                acc as Arc<dyn DataAccessor>,
            )]),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            None,
            Default::default(),
        );
        let cap = b
            .run_with(
                r#"(async () => {
                     const a = db.stream("q")[Symbol.asyncIterator]();
                     const b2 = db.stream("q")[Symbol.asyncIterator]();
                     const a1 = await a.next();
                     const b1 = await b2.next();
                     const a2 = await a.next();
                     const b2v = await b2.next();
                     const aDone = (await a.next()).done;
                     const bDone = (await b2.next()).done;
                     json.ok({ a: [a1.value.id, a2.value.id], b: [b1.value.id, b2v.value.id], aDone, bDone });
                   })().catch(e => json.fail(500, String(e)));"#,
                crate::bridge::RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(
            v["data"]["a"],
            json!([1, 2]),
            "游标 a 序列不得被 b 串扰: {v}"
        );
        assert_eq!(
            v["data"]["b"],
            json!([1, 2]),
            "游标 b 序列不得被 a 串扰: {v}"
        );
        assert_eq!(v["data"]["aDone"], json!(true), "{v}");
        assert_eq!(v["data"]["bDone"], json!(true), "{v}");
    }

    /// 中途 abort：流干净终止（不 panic、连接回池）；abort 经 AbortSignal 的 abort 事件触发。
    #[tokio::test(flavor = "current_thread")]
    async fn stream_abort_terminates_without_panic() {
        let db = crate::bridge::SqlxAccessor::arc("sqlite::memory:")
            .await
            .unwrap();
        let b = Bridge::with_dbs_and_loader(
            std::collections::HashMap::from([("default".to_string(), db as _)]),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            None,
            Default::default(),
        );
        let cap = b
            .run_with(
                r#"(async () => {
                     await db.exec("create table t(id integer)");
                     for (let i = 0; i < 1000; i++) await db.exec("insert into t values (?)", [i]);
                     const ac = new AbortController();
                     let count = 0;
                     try {
                       await db.stream("select * from t", [], {
                         signal: ac.signal,
                         onRow: () => { count++; if (count >= 3) ac.abort(); }
                       });
                       json.ok({ aborted: false, count });
                     } catch (e) {
                       json.ok({ aborted: true, count });
                     }
                   })().catch(e => json.fail(500, String(e)));"#,
                crate::bridge::RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        // 有界通道语义：abort = **干净提前结束**（缓冲行交付完即 done，不抛错）；
        // count 必须 < 1000（取消真的停止了后端拉取，而非灌满缓冲后照常跑完）。
        assert_eq!(
            v["data"]["aborted"],
            json!(false),
            "abort = 流提前结束（非错误）: {v}"
        );
        let count = v["data"]["count"].as_u64().unwrap();
        assert!(count >= 3, "至少已消费 3 行：{v}");
        assert!(
            count < 1000,
            "取消必须停止拉取（count={count} 不得跑完全表）: {v}"
        );
    }
}
