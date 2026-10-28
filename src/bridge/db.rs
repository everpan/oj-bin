//! db/DB(name) 数据访问绑定。
//!
//! db === DB("default")（引用相等由 bootstrap.js 侧的实例缓存保证），
//! 未配置的名字 DB(name) 返回 undefined。实例存在性检查在 Rust 侧（op_db_has）。
//! DataAccessor 的可变参数 args ...any 在 bridge 层从未被使用，故本版只传 SQL。
//!
//! 安全性：新增 `query_with_params` / `exec_with_params` 以支持绑定参数，杜绝 JS 侧字符串拼接
//! （原始 `query(sql)` 仅保留为无参便捷形式；真实 SQL 实现应优先用 *_with_params 或 query.rs 构造器）。

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

use async_trait::async_trait;
use deno_core::{OpState, op2};
use deno_error::JsErrorBox;
use futures::stream::Stream;
use serde_json::Value;
use tokio::sync::{Notify, mpsc};

use super::{BridgeResult, StableState};
use crate::bridge::db_error::{BridgeError, unsupported};
use crate::bridge::dialect_caps::{DialectCaps, DialectInfo, IdStrategy, PlaceholderStyle};

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

/// 方言名 → known 子集（`mysql`/`postgres`/`sqlite`）；未知方言返回 `None`
///（由上层门控点拦截，绝不冒充已知方言）。T5/T6 两层方言模型的核心查询。
pub fn known_dialect(name: &str) -> Option<Dialect> {
    match name {
        "mysql" => Some(Dialect::MySql),
        "postgres" | "postgresql" => Some(Dialect::Postgres),
        "sqlite" => Some(Dialect::Sqlite),
        _ => None,
    }
}

/// 门控：方言名不在 builder-known 子集时，一律结构化 `unsupported`，绝不回落 Sqlite。
/// 设计 §5.2：未知方言是开放契约下的有效状态，oj 生成的 SQL 路径（builder/list_tables/
/// next_seq/migrate/seed）必须先过此门控；raw SQL 路径（db.query/exec 由插件承载方言）不受限。
pub(crate) fn require_known_dialect(
    info: &DialectInfo,
    capability: &str,
) -> Result<Dialect, crate::bridge::db_error::BridgeError> {
    match Dialect::known(&info.name) {
        Some(d) => Ok(d),
        None => Err(crate::bridge::db_error::unsupported(&info.name, capability)),
    }
}

impl Dialect {
    /// 见 [`known_dialect`]：方言名 → known 子集。
    pub fn known(name: &str) -> Option<Dialect> {
        known_dialect(name)
    }

    /// 方言名（用于描述符 name 字段）。
    pub fn as_str(&self) -> &'static str {
        match self {
            Dialect::Sqlite => "sqlite",
            Dialect::MySql => "mysql",
            Dialect::Postgres => "postgres",
        }
    }
}

/// 内置 accessor（sqlite/memory）的默认方言描述符（T6：两层模型 default 锚点）。
/// 占位 `?` + 两步取 ID（`last_insert_rowid`）+ 支持流式（sqlx 游标）+ 无 cancel。
static SQLITE_DIALECT_INFO: OnceLock<DialectInfo> = OnceLock::new();

/// 取默认 sqlite 方言描述符的 `&'static` 引用（懒初始化）。
pub fn sqlite_dialect_info() -> &'static DialectInfo {
    SQLITE_DIALECT_INFO.get_or_init(|| DialectInfo {
        name: "sqlite".to_string(),
        caps: DialectCaps {
            placeholder: PlaceholderStyle::Q,
            id: IdStrategy::TwoStep,
            stream: true,
            cancel: false,
        },
    })
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
    /// 取消能力（与 DataAccessor::cancel 同源默认哨兵；事务级取消沿用 accessor 的默认 unsupported）。
    async fn cancel(&self, _stream_id: u64) -> BridgeResult<()> {
        Err(unsupported(&self.dialect_info().name, "cancel").into())
    }
    /// 事务级方言描述符（T6：begin 时从 accessor 完整快照，in-tx 改写/exec 用此而非再次查询）。
    fn dialect_info(&self) -> &DialectInfo {
        sqlite_dialect_info()
    }
}

/// 数据访问统一契约（接口隔离 + 依赖倒置）。
/// M0 用内存 fake；后续 sqlx 以同接口接入（`query_with_params` 参数化），handler 无需改动。
#[async_trait]
pub trait DataAccessor: Send + Sync {
    /// 库方言（构造器选 builder 用；默认 sqlite，fake/未知驱动走默认）。
    fn dialect(&self) -> Dialect {
        Dialect::Sqlite
    }

    /// 方言描述符（T6：两层模型——边界描述符，装配期冻结，运行期纯查表）。
    /// 默认 sqlite 静态描述符；FfiDataAccessor/SqlxAccessor 覆盖为各自真实描述符。
    fn dialect_info(&self) -> &DialectInfo {
        sqlite_dialect_info()
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

    /// 取消能力（M2 Oracle/DM 覆盖之；宿主侧仅在此声明默认哨兵）。
    /// 默认 `unsupported`——能力矩阵（ffi.rs `stream_query` / `cancel`）据声明能力决定
    /// 采纳或 fail-loud；声明了却未提供该能力的 accessor 必须 fail-loud，不可静默降级。
    /// `stream_id` 为协作式取消目标（缺失/未知时为 0，由插件自决）。
    async fn cancel(&self, _stream_id: u64) -> BridgeResult<()> {
        Err(unsupported(&self.dialect_info().name, "cancel").into())
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
    let st = state.borrow::<Arc<StableState>>();
    st.dbs.contains_key(name) || st.client_dbs.contains(name)
}

/// 每请求活跃事务（存 ReqState；Mutex 串行并发 op，Arc 跨 await 共享——
/// 不得跨 await 持 OpState/RefCell 借用）。
pub struct ActiveTx {
    pub db: String,
    pub session: tokio::sync::Mutex<Box<dyn TxSession>>,
    /// 连接方言描述符（T6：begin 时从 accessor 完整快照，in-tx 改写/exec 据此查表）。
    /// 只存描述符、不存 known 枚举：开放方言下枚举会 panic（未知名不在子集），
    /// 而事务是裸 SQL 路径的一部分，必须对新方言可用（设计 §5.5）。
    pub dialect_info: DialectInfo,
}

/// 当前请求的活跃事务句柄（无则 None）。借用即取即还。
pub(crate) fn current_tx(state: &Rc<RefCell<OpState>>) -> Option<Arc<ActiveTx>> {
    state.borrow().borrow::<super::ReqState>().tx.clone()
}

/// 查询/执行的目标：会话（跨请求事务）→ 本库活跃事务 → 池。
pub(crate) enum Target {
    Tx(Arc<ActiveTx>),
    Pool(Arc<dyn DataAccessor>),
}

pub(crate) fn resolve_target(
    state: &Rc<RefCell<OpState>>,
    name: &str,
) -> Result<Target, JsErrorBox> {
    // 跨请求事务会话优先：一旦 attach，本请求所有裸 SQL 与构造器都必须落在同一条连接上
    // （否则「会话里读到的是别的连接」＝静默错误）。会话与 db.tx 互斥（begin/attach 双向拒绝）。
    if let Some(t) = super::db_session::attached_target(state, name)? {
        return Ok(Target::Tx(t));
    }
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

/// 在事务/会话上跑一次调用，并按**会话剩余寿命**封顶（无会话绑定 = 无额外超时）。
///
/// 为什么需要：后台回收器只用 `try_lock` 跳过「正在跑」的会话（绝不阻塞运行时），
/// 于是长查询会让过期回收永远够不着它——必须由在飞侧自己按剩余寿命判死（评审 S5）。
/// 命中超时即判死会话：连接状态不可知，不再复用（宁可让调用方重开）。
async fn on_tx<T, F>(state: &Rc<RefCell<OpState>>, what: &str, f: F) -> BridgeResult<T>
where
    F: std::future::Future<Output = BridgeResult<T>>,
{
    match super::db_session::attached_deadline(state) {
        Some(left) => match tokio::time::timeout(left, f).await {
            Ok(r) => r,
            Err(_) => {
                super::db_session::kill_attached(state, "lifetime_exceeded");
                Err(format!(
                    "db session lifetime exhausted during {what}; the session was discarded \
                     (transaction rolled back)"
                )
                .into())
            }
        },
        None => f.await,
    }
}

/// 占位符统一改写（0.1.67，design docs/plans/2026-10-13-unified-db-placeholder-design.md）：
/// `?` 是 oj 唯一占位符，postgres 方言由宿主改写为 `$n`；sqlite/mysql/空参透传零开销。
/// 三个裸 SQL op（query/exec/stream_open）的唯一插桩点（DRY）；守护测试钉死出口白名单。
/// fail-loud（计数不匹配）同样记 trace（ok=false），不留画像盲区（评审 E6）；
/// trace 一律记**原始** SQL（画像定位开发者所写，非引擎所收）。
#[allow(clippy::too_many_arguments)]
fn normalize_and_trace<'a>(
    state: &Rc<RefCell<OpState>>,
    placeholder: PlaceholderStyle,
    in_tx: bool,
    name: &str,
    sql: &'a str,
    params: &[Value],
) -> Result<Cow<'a, str>, JsErrorBox> {
    match super::placeholder::normalize_sql(placeholder, sql, params) {
        Ok(norm) => Ok(norm),
        Err(e) => {
            trace_sql(
                state,
                sql,
                params,
                name,
                in_tx,
                0.0,
                None,
                false,
                Some(e.clone()),
                None,
                None,
            );
            Err(JsErrorBox::generic(e))
        }
    }
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
    /// 所属库别名（限额日志用）。
    pub alias: String,
    /// db 访问控制的 `max_rows`（v0.1.89）：达到即提前终止（真省资源，区别于 raw 的
    /// 「取回后截断」）。None = 不限。
    pub max_rows: Option<u32>,
    /// 已交付行数（与 `max_rows` 配合做提前终止）。
    pub delivered: AtomicU32,
    /// 达 `max_rows` 后置位：下一次 `next` 直接 done。
    ///
    /// 为什么需要独立标志位：`abort` 只是通知 pump 停止拉取，**通道里已缓冲的行
    /// 仍会被 recv 出来**（有界背压使然），故不能在达上限时把第 cap 行换成 done
    /// （少给一行），也不能指望通道立刻见底。done 由本标志显式钉住。
    pub done: AtomicBool,
    /// 跨请求取消标志（U-5 v0.1.96）：`db.cancel(executionId)` 置位。`next` 在 recv 前
    /// 检查——取消必须抛错而非静默 done（消费方不得误以为数据取完）。
    pub cancelled: AtomicBool,
    /// 反向句柄：本流在 `StableState.exec_sessions` 的执行 id（open 时登记，close 摘除）。
    pub exec_id: OnceLock<String>,
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
    if state.borrow().borrow::<super::ReqState>().session.is_some() {
        return Err(JsErrorBox::generic(
            "a db session is attached to this request; db.tx cannot nest inside it (detach first)",
        ));
    }
    let da = super::query::lookup(&state, &name)?;
    let session = da.begin().await.map_err(to_js_err)?;
    // 方言快照进 ActiveTx（而非 TxSession trait 加方法 / FFI ABI 演进）：方言是
    // accessor 静态属性，begin 时可从 da.dialect_info() 拿到；前者要改 SqlxTx/InMemoryTx
    // 及全部夹具，后者要 bump ABI_VERSION（严格相等门禁）——代价与收益不成比例。
    // 注意：**不调用 da.dialect()**（开放方言下它会 panic），只存描述符。
    state.borrow_mut().borrow_mut::<super::ReqState>().tx = Some(Arc::new(ActiveTx {
        db: name,
        session: tokio::sync::Mutex::new(session),
        dialect_info: da.dialect_info().clone(),
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
    t.session.lock().await.commit().await.map_err(to_js_err)?;
    Ok(true)
}

/// db.tx 回滚。
#[op2]
pub async fn op_db_tx_rollback(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
) -> Result<bool, JsErrorBox> {
    let t = take_tx(&state, &name)?;
    t.session.lock().await.rollback().await.map_err(to_js_err)?;
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
    // 请求中途改了 req → http 快照 memo（bootstrap 按 epoch 判旧）随之失效。
    rs.epoch = rs.epoch.wrapping_add(1);
    Ok(true)
}

/// accessor 抛出的 `Box<dyn Error>` 可能内含 `BridgeError`（来自 ffi 侧 `parse_raw` 单点解析的
/// 结构化信封）。优先还原 `code`/`data` 透传到 JS；否则退化为 generic（保留原文），不丢信息。
pub(crate) fn to_js_err(e: Box<dyn std::error::Error + Send + Sync>) -> JsErrorBox {
    match e.downcast::<BridgeError>() {
        Ok(be) => (*be).into(),
        Err(e) => JsErrorBox::generic(e.to_string()),
    }
}

/// 从 `BridgeResult` 错误抽取结构化 `(category, retryable)`（T12，供 `trace_sql` 落画像）。
/// 非 `BridgeError`（占位符/JS 层错误）返回 `(None, None)`。调用方须在 `.map_err(to_js_err)`
/// 之前调用——转换后 `BridgeError` 被包进 `JsErrorBox`，无法再还原 category。
pub(crate) fn err_meta<T>(res: &BridgeResult<T>) -> (Option<String>, Option<bool>) {
    match res {
        Err(e) => err_meta_of(e.as_ref()),
        Ok(_) => (None, None),
    }
}

/// 单错版 `err_meta`：只拿到 `Box<dyn Error>` 的调用点（builder 路径）用。
/// `BridgeError` → `(Some(category), Some(retryable))`；其余 `(None, None)`。
pub(crate) fn err_meta_of(
    e: &(dyn std::error::Error + Send + Sync + 'static),
) -> (Option<String>, Option<bool>) {
    e.downcast_ref::<BridgeError>().map_or((None, None), |b| {
        (Some(b.category.as_str().to_string()), Some(b.retryable))
    })
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
    query_impl(state, name, sql, params.unwrap_or_default()).await
}

/// `db.query` 的实体：`op_db_query` 与跨请求会话的显式形态 `db.session.query(id, …)`
/// 共用同一条通路（守卫/授权/占位符改写/追踪/行数整形一处生效，不复制）。
///
/// 绑定会话时额外做两件事（评审 S1/S5）：会话内语句禁令（`BEGIN`/`SET` 等会隐式提交，
/// 制造假回滚）；按会话剩余寿命给在飞查询封顶。
pub(crate) async fn query_impl(
    state: Rc<RefCell<OpState>>,
    name: String,
    sql: String,
    params: Vec<Value>,
) -> Result<Vec<Row>, JsErrorBox> {
    super::db_session::forbid_in_session(&state, &sql)?;
    super::db_session::readonly_gate(&state, &sql)?; // G9 只读会话位（attach 路径）
    super::guard::check_raw(&state, &sql)?; // 表归属守卫（§5.3，无模块上下文不设防）
    super::guard::check_tenant_raw(&state, &sql, &params)?; // 多租户防护（独立于 module_ctx）
    // db 访问控制（v0.1.89）：授权 → 风险分类 → 策略位；返回本库生效的行数上限。
    let alias = super::guard::bound_db(&state, &name);
    let row_cap = super::db_policy::check_access(&state, &alias, &sql)?;

    let target = resolve_target(&state, &name)?;
    let in_tx = matches!(&target, Target::Tx(_));
    let placeholder = match &target {
        Target::Tx(t) => t.dialect_info.caps.placeholder,
        Target::Pool(da) => da.dialect_info().caps.placeholder,
    };
    let norm = normalize_and_trace(&state, placeholder, in_tx, &name, &sql, &params)?;
    let start = std::time::Instant::now();
    let res: BridgeResult<Vec<Row>> = match target {
        Target::Pool(da) => da.query_with_params(&norm, &params).await,
        Target::Tx(t) => {
            on_tx(&state, "query", async {
                t.session.lock().await.query(&norm, &params).await
            })
            .await
        }
    };
    let (cat, retry) = err_meta(&res);
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    trace_sql(
        &state,
        &sql,
        &params,
        &name,
        in_tx,
        ms,
        res.as_ref().ok().map(|r| r.len() as i64),
        res.is_ok(),
        res.as_ref().err().map(|e| e.to_string()),
        cat,
        retry,
    );

    let mut rows = res.map_err(to_js_err)?;
    // db 访问控制的行数上限（raw 路径 = **取回后截断**：全量扫描/传输已发生，这是响应整形
    // 而非 DoS 防线，边界写进 db-guide）。
    if let Some(cap) = row_cap
        && rows.len() as u32 > cap
    {
        rows.truncate(cap as usize);
        let mut g = state.borrow_mut();
        g.borrow_mut::<super::ReqState>()
            .headers
            .push(("X-OJ-Row-Limit".into(), cap.to_string()));
    }
    // 出口护栏：超界整数降十进制字符串，否则 JS 侧拿到 BigInt、json.ok 必 500（见 jsnum）。
    super::jsnum::sanitize_rows(&mut rows);
    Ok(rows)
}

/// `Instant` → 毫秒（trace 计时用）。单拧出来是为了变异测试：`* 1000.0` 的量纲换算
/// 有自己的时钟钉子测试（`elapsed_ms_scales_to_millis`）——内联在 op 里时 `*`→`+`/`/`
/// 的变异体只是改了「记出来的数字大小」，运行期不可断言，必然静默存活。
fn elapsed_ms(start: std::time::Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

/// 在 db op 内调用：构造 `SqlEvent` 并交给 `sql_trace::record_sql`（配置未开则零开销直接返回）。
/// `rows` 为影响/返回行数（stream/未知传 None）；`ok`/`err` 描述执行结果。
///
/// 画像/信封永远脱敏（只记「参数个数」）；仅 `redact_params=false` 时 dev 日志记参数原值。
#[allow(clippy::too_many_arguments)]
pub(crate) fn trace_sql(
    state: &Rc<RefCell<OpState>>,
    sql: &str,
    params: &[Value],
    db: &str,
    in_tx: bool,
    ms: f64,
    rows: Option<i64>,
    ok: bool,
    err: Option<String>,
    category: Option<String>,
    retryable: Option<bool>,
) {
    // 关闭即零开销：先判 enabled，再算任何东西（含 module clone / 参数摘要）。
    let enabled = state
        .borrow()
        .borrow::<std::sync::Arc<super::StableState>>()
        .sql_trace
        .enabled;
    if !enabled {
        return;
    }
    let redact = state
        .borrow()
        .borrow::<std::sync::Arc<super::StableState>>()
        .sql_trace
        .redact_params;
    let src = state
        .borrow()
        .borrow::<super::ReqState>()
        .module
        .clone()
        .unwrap_or_else(|| "anon".into());
    // 画像/信封永远脱敏；dev 日志在 redact=false 时记原值。
    let redacted = super::sql_trace::summarize_params(params, true);
    let log_params = if redact {
        redacted.clone()
    } else {
        super::sql_trace::summarize_params(params, false)
    };
    super::sql_trace::record_sql(
        state,
        super::sql_trace::SqlEvent {
            sql: sql.to_string(),
            params: redacted,
            db: db.to_string(),
            in_tx,
            source: src,
            ms,
            rows,
            ok,
            error: err,
            // db 访问控制（v0.1.89）：操作者与风险分类。仅在 trace 开启时计算（上面已
            // 早退，关闭态零开销）；`operator` 取 `user.id`（无身份记 None）。
            operator: {
                let g = state.borrow();
                super::db_policy::operator_of(g.borrow::<super::ReqState>().req.user.as_ref())
            },
            risk: Some(super::db_policy::classify(sql).risk.as_str().to_string()),
            category,
            retryable,
        },
        log_params,
    );
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
    exec_impl(state, name, sql, params.unwrap_or_default()).await
}

/// `db.exec` 的实体（与 `db.session.exec` 共用；理由同 [`query_impl`]）。
pub(crate) async fn exec_impl(
    state: Rc<RefCell<OpState>>,
    name: String,
    sql: String,
    params: Vec<Value>,
) -> Result<i64, JsErrorBox> {
    super::db_session::forbid_in_session(&state, &sql)?;
    super::db_session::readonly_gate(&state, &sql)?; // G9 只读会话位（attach 路径）
    // 裸 SQL 路径（query/exec/stream）对开放方言**全量可用**，不做 known 子集门控
    // （设计 §5.5：门控只覆盖 builder / exec_route / list_tables / nextSeq /
    // search.* / CLI 六处）——方言知识只体现在占位符能力位。
    super::guard::check_raw(&state, &sql)?; // 表归属守卫（§5.3，无模块上下文不设防）
    super::guard::check_tenant_raw(&state, &sql, &params)?; // 多租户防护（独立于 module_ctx）
    // db 访问控制（v0.1.89）：写类语句在此被 read_only / 危险语句门拦下。
    let alias = super::guard::bound_db(&state, &name);
    super::db_policy::check_access(&state, &alias, &sql)?;

    let target = resolve_target(&state, &name)?;
    let in_tx = matches!(&target, Target::Tx(_));
    let placeholder = match &target {
        Target::Tx(t) => t.dialect_info.caps.placeholder,
        Target::Pool(da) => da.dialect_info().caps.placeholder,
    };
    let norm = normalize_and_trace(&state, placeholder, in_tx, &name, &sql, &params)?;
    let start = std::time::Instant::now();
    let res: BridgeResult<i64> = match target {
        Target::Pool(da) => da.exec_with_params(&norm, &params).await,
        Target::Tx(t) => {
            on_tx(&state, "exec", async {
                t.session.lock().await.exec(&norm, &params).await
            })
            .await
        }
    };
    let (cat, retry) = err_meta(&res);
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    trace_sql(
        &state,
        &sql,
        &params,
        &name,
        in_tx,
        ms,
        res.as_ref().ok().copied(),
        res.is_ok(),
        res.as_ref().err().map(|e| e.to_string()),
        cat,
        retry,
    );

    res.map_err(to_js_err)
}

/// db.stream 打开（PR-2 Phase A）：先 `guard::check_raw` / `check_tenant_raw`（open 前校验，同
/// `op_db_query`）→ 取 accessor → `stream_query` 开流 → spawn **脱离式 pump**（current_thread 运行时
/// 内）逐行推入通道；op 立即返回 `{id, executionId}`（后者是 U-5 跨请求取消的句柄）。
/// 插件后端（FfiDataAccessor）走默认实现报错。
#[op2]
#[serde]
pub async fn op_db_stream_open(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[string] sql: String,
    #[serde] params: Option<Vec<Value>>,
) -> Result<serde_json::Value, JsErrorBox> {
    let params = params.unwrap_or_default();
    super::guard::check_raw(&state, &sql)?; // 表归属守卫（§5.3，无模块上下文不设防）
    super::guard::check_tenant_raw(&state, &sql, &params)?; // 多租户防护（独立于 module_ctx）
    // db 访问控制（v0.1.89）：与 query/exec 同一入口；行数上限走「提前终止」而非截断。
    let alias = super::guard::bound_db(&state, &name);
    let row_cap = super::db_policy::check_access(&state, &alias, &sql)?;
    let da = match resolve_target(&state, &name)? {
        Target::Pool(da) => da,
        Target::Tx(_) => {
            return Err(JsErrorBox::generic(
                "db.stream within an active transaction is not supported (streaming queries run on the connection pool only)",
            ));
        }
    };
    // U-7：动态库全局流预算——开流前占名额（满了拒，不排队）；permit 由 pump task 持有，
    // 随流结束（自然/abort/close 任一路）释放。
    let stream_permit = {
        let st = state.borrow().borrow::<Arc<StableState>>().clone();
        if !st.dbs.contains_key(&alias) && st.client_dbs.contains(&alias) {
            match st.client_dbs.try_acquire_stream() {
                Ok(p) => Some(p),
                Err(msg) => {
                    super::client_db::deny_log(&state, "stream", "budget_exhausted", "-", &alias);
                    return Err(JsErrorBox::generic(msg.to_string()));
                }
            }
        } else {
            None
        }
    };
    // stream 明确拒绝事务，占位风格直接取 accessor（无需 ActiveTx 快照）。
    let norm = normalize_and_trace(
        &state,
        da.dialect_info().caps.placeholder,
        false,
        &name,
        &sql,
        &params,
    )?;
    let start = std::time::Instant::now();
    let stream_res = da.stream_query(&norm, &params).await;
    let (cat, retry) = err_meta(&stream_res);
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    // 流式只追踪「打开」（首行拉取开始）；整条流耗时由 `db.stream` 的逐行消费决定，未计入。
    trace_sql(
        &state,
        &sql,
        &params,
        &name,
        false,
        ms,
        None,
        stream_res.is_ok(),
        stream_res.as_ref().err().map(|e| e.to_string()),
        cat,
        retry,
    );
    let mut stream = stream_res.map_err(to_js_err)?;
    let (tx, rx) = mpsc::channel::<Result<Row, String>>(DB_STREAM_CHANNEL_CAP);
    let shared = Arc::new(DbStreamShared {
        rx: tokio::sync::Mutex::new(rx),
        abort: Arc::new(Notify::new()),
        alias: alias.clone(),
        max_rows: row_cap,
        delivered: AtomicU32::new(0),
        done: AtomicBool::new(false),
        cancelled: AtomicBool::new(false),
        exec_id: std::sync::OnceLock::new(),
    });
    let id = NEXT_STREAM_ID.fetch_add(1, Ordering::SeqCst);
    let reg = state
        .borrow()
        .borrow::<super::ReqState>()
        .db_streams
        .clone();
    reg.lock().await.insert(id, shared.clone());
    // U-5：登记进进程级执行表（`db.cancel` 的跨请求路由），id 写回共享句柄供 close 摘除。
    let exec_id = {
        let exec = state
            .borrow()
            .borrow::<Arc<StableState>>()
            .exec_sessions
            .clone();
        let owner = super::db_session::owner_of(&state.borrow());
        exec.register(owner, alias.clone(), shared.clone())
            .map_err(JsErrorBox::generic)?
    };
    let _ = shared.exec_id.set(exec_id.clone());
    // 脱离式 pump：监听流与取消信号，逐行推入通道（通道关闭 = done）。
    // **有界通道 + 阻塞投递**：消费端不拉取时 pump 停在 send 上——abort 一到即
    // 停止后端拉取（真取消）；unbounded 会把结果集提前灌满、取消扑空（探测
    // probe_stream_abort_releases_connection 抓到的回归）。
    let abort = shared.abort.clone();
    tokio::spawn(async move {
        // U-7 流预算 permit：随本 task 结束（任一退出路径）自动释放。
        let _stream_permit = stream_permit;
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
    Ok(serde_json::json!({ "id": id as f64, "executionId": exec_id }))
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
    // 达上限后的终态：返回 done 而不是「再 recv 一次」（通道里可能还有缓冲行）。
    if shared.done.load(Ordering::SeqCst) {
        return Ok(Row::Null);
    }
    // U-5 跨请求取消：置位即抛错——取消必须可辨（静默 done 会让消费方误以为数据取完）。
    // recv 前检查：即使通道里还有缓冲行/已见底，取消语义优先。
    if shared.cancelled.load(Ordering::SeqCst) {
        let shown = super::handle_core::id_prefix(
            super::db_exec::PREFIX,
            shared.exec_id.get().map(|s| s.as_str()).unwrap_or(""),
        );
        return Err(JsErrorBox::generic(format!(
            "db stream {shown} was cancelled via db.cancel (cursor closed; rows already delivered are kept)"
        )));
    }
    let mut rx = shared.rx.lock().await;
    match rx.recv().await {
        Some(Ok(row)) => {
            // db 访问控制的行数上限：交付第 cap 行后即停（关闭游标、不再向后端拉取）。
            // 与 raw 的「取回后截断」不同，stream 提前终止是真省资源。
            let n = shared.delivered.fetch_add(1, Ordering::SeqCst) + 1;
            if let Some(cap) = shared.max_rows
                && n >= cap
            {
                shared.done.store(true, Ordering::SeqCst);
                drop(rx);
                super::db_policy::log_row_limit(&shared.alias, cap);
                // 只通知 pump 停止；**不摘注册表条目**——下一次 next 需经它返回 done。
                shared.abort.notify_waiters();
            }
            Ok(row)
        }
        Some(Err(e)) => Err(JsErrorBox::generic(e)),
        None => Ok(Row::Null), // done
    }
}

/// db 访问控制自省（v0.1.89）：当前主体可用的库列表 + 每库能力位。
///
/// dbx 形态的首屏是连接树——没有这个出口，前端只能列出全部连接、点了才报错（= 假入口）。
#[op2]
#[serde]
pub fn op_db_allowed(state: &mut OpState) -> Result<Vec<Value>, JsErrorBox> {
    let st = state.borrow::<Arc<super::StableState>>().clone();
    let mut roles: Vec<String> = Vec::new();
    let rs = state.borrow::<super::ReqState>();
    let module = rs.module.clone();
    if let Some(a) = rs
        .req
        .user
        .as_ref()
        .and_then(|u| u.get("roles"))
        .and_then(|u| u.as_array())
    {
        roles = a
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect();
    }
    let tenant: Option<String> = if rs.req.anonymous {
        None
    } else {
        rs.req.tenant_id.clone()
    };
    let source = if st.tasks_flag.is_some() {
        "task"
    } else {
        "http"
    };
    let ctx = super::db_policy::AccessCtx {
        module: module.as_deref(),
        roles,
        tenant: tenant.as_deref(),
        source,
    };
    let mut out: Vec<Value> = Vec::new();
    let mut names: Vec<String> = st.dbs.keys().cloned().collect();
    // L3：动态登记库仅在 `db_access.<name>` 显式声明（或 U-1 运行期 grant）时才进连接树
    // （避免「假入口」——未声明的动态库默认拒所有人，不该出现在「可用连接」列表里）。
    for n in st.client_dbs.names() {
        if st.db_access.by_db.contains_key(&n) || super::db_policy::has_runtime_grant(&st, &n) {
            names.push(n);
        }
    }
    names.sort();
    for alias in &names {
        // 必须走含运行期 grant 的归一策略，否则运行期授权的库在 `db.allowed()` 里仍是假入口。
        let policy = super::db_policy::policy_for_state(&st, alias);
        if !super::db_policy::granted(&policy.allow, &ctx) {
            continue;
        }
        let dialect = st
            .dbs
            .get(alias)
            .map(|d| d.dialect_info().name.clone())
            .unwrap_or_default();
        out.push(serde_json::json!({
            "alias": alias,
            "dialect": dialect,
            "read_only": policy.read_only,
            "allow_ddl": policy.allow_ddl,
            "allow_dangerous": policy.allow_dangerous,
            "max_rows": policy.max_rows,
            "enforce": match policy.enforce {
                super::db_policy::Enforce::Off => "off",
                super::db_policy::Enforce::Warn => "warn",
                super::db_policy::Enforce::Deny => "deny",
            },
        }));
    }
    Ok(out)
}

/// db.stream 关闭（PR-2 Phase A）：触发中止并移除注册表条目（条目移除后 Arc 仍被 pump / 进行中的
/// next 持有至排空，无泄漏）；不强制 await，尽快归还。U-5：顺带摘除进程级执行登记。
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
        if let Some(xid) = s.exec_id.get() {
            state
                .borrow()
                .borrow::<Arc<StableState>>()
                .exec_sessions
                .remove(xid);
        }
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

/// db.catalog（U-14 批 a，v0.1.100）：目录类内省——views / routines / sequences / ddl /
/// comment / constraints / stats，信封 `{kind, dialect, items}`。与 L2 三件（listColumns/
/// Indexes/ForeignKeys，裸数组）分代：旧件不动（兼容），新 kind 一律走本 op。
/// 授权链与 L2 同（`granted_only`）；kind × 方言 unsupported → 结构化 `OJ_DB_UNSUPPORTED`
/// （能力矩阵见 db-guide §10，报错不静默）。
#[op2]
#[serde]
pub async fn op_db_catalog(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[string] kind: String,
    #[string] arg: Option<String>,
) -> Result<serde_json::Value, JsErrorBox> {
    let kind = super::introspect::Kind::parse_catalog(&kind).ok_or_else(|| {
        JsErrorBox::generic(format!(
            "db catalog: unknown kind {kind:?} (views|routines|sequences|ddl|comment|constraints|stats)"
        ))
    })?;
    let target = resolve_target(&state, &name)?;
    let in_tx = matches!(&target, Target::Tx(_));
    let dialect = match &target {
        Target::Tx(t) => require_known_dialect(&t.dialect_info, kind.capability())?,
        Target::Pool(da) => require_known_dialect(da.dialect_info(), kind.capability())?,
    };
    let alias = super::guard::bound_db(&state, &name);
    super::db_policy::granted_only(&state, &alias)?;

    let mut sql = match super::introspect::catalog_sql(kind, dialect) {
        Some(s) => s.to_string(),
        None => {
            return Err(to_js_err(Box::new(crate::bridge::db_error::unsupported(
                dialect.as_str(),
                kind.capability(),
            ))));
        }
    };
    if kind.catalog_limit() {
        sql.push_str(&format!(" LIMIT {}", super::introspect::MAX_ROWS));
    }
    // mysql SHOW 族 = 零内联铁律的唯一成文例外（introspect.rs 模块注释），白名单校验在 helper。
    let params = if kind == super::introspect::Kind::Ddl && dialect == Dialect::MySql {
        match super::introspect::mysql_show_create(arg.as_deref().unwrap_or("")) {
            Ok(s) => {
                sql = s;
                vec![]
            }
            Err(e) => return Err(JsErrorBox::generic(e)),
        }
    } else if kind.catalog_param() {
        let t = arg.as_deref().unwrap_or("");
        if !valid_introspect_table(dialect, t) {
            // 畸形 / 跨库前缀 → 空结果（与 L2 同口径：不报错、不泄漏）。
            return Ok(serde_json::json!({
                "kind": kind.capability(), "dialect": dialect.as_str(), "items": []
            }));
        }
        vec![Value::String(t.to_string())]
    } else {
        vec![]
    };

    let placeholder = match &target {
        Target::Tx(t) => t.dialect_info.caps.placeholder,
        Target::Pool(da) => da.dialect_info().caps.placeholder,
    };
    let norm = normalize_and_trace(&state, placeholder, in_tx, &name, &sql, &params)?;
    let start = std::time::Instant::now();
    let trace_params = &[Value::String("<db introspection>".to_string())];
    let res = match &target {
        Target::Pool(da) => da.query_with_params(&norm, &params).await,
        Target::Tx(t) => t.session.lock().await.query(&norm, &params).await,
    };
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    match res {
        Ok(rows) => {
            let items = super::introspect::catalog(kind, rows);
            trace_sql(
                &state,
                &sql,
                trace_params,
                &name,
                in_tx,
                ms,
                Some(items.len() as i64),
                true,
                None,
                None,
                None,
            );
            Ok(serde_json::json!({
                "kind": kind.capability(), "dialect": dialect.as_str(), "items": items
            }))
        }
        Err(e) => {
            let (cat, retry) = err_meta_of(e.as_ref());
            let ms = start.elapsed().as_secs_f64() * 1000.0;
            trace_sql(
                &state,
                &sql,
                trace_params,
                &name,
                in_tx,
                ms,
                None,
                false,
                Some(e.to_string()),
                cat,
                retry,
            );
            Err(to_js_err(e))
        }
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
/// db.sqlProfile()：本请求 SQL 执行画像快照（v0.1.51，dev 默认开）。
/// 返回 `{ count, totalMs, slow, byDb, events }`；追踪未开启（release 默认 / config 关）时返回空对象。
#[op2]
#[serde]
pub fn op_db_sql_profile(state: Rc<RefCell<OpState>>) -> Result<serde_json::Value, JsErrorBox> {
    let enabled = state
        .borrow()
        .borrow::<std::sync::Arc<StableState>>()
        .sql_trace
        .enabled;
    if !enabled {
        return Ok(Value::Object(Default::default()));
    }
    let state_ref = state.borrow();
    let rs = state_ref.borrow::<super::ReqState>();
    Ok(rs.sql_profile.snapshot())
}

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
    // 门控（T8）：未知方言（不在 builder-known 子集）一律结构化 unsupported，绝不回落 Sqlite。
    let _dialect = require_known_dialect(da.dialect_info(), "next_seq")?;
    // ① **先确保序列表存在**（每库一次，Bridge 级缓存）。DDL 走池、绝不在调用方事务里做：
    //    MySQL 的 DDL 会隐式提交调用方事务；PG 里事务内失败语句会让事务进入 aborted 态
    //    （后续一律 25P02）——若把建表放在「先试后建」的失败路径上，**首次在 db.tx 里用新序列名
    //    必然失败并毒化调用方事务**（架构评审 P1-2）。
    //    键用**物理库名**（`bound_db` 的结果）而非 JS 可见名：同一 Bridge 下不同模块可把字面
    //    `"default"` 绑到不同物理库（`manifest db:` / `db_override`），用可见名当键会让后一个库
    //    命中前一个库的缓存、跳过建表（池路径多付一次失败+DDL，事务路径直接失败）。
    let phys = super::guard::bound_db(&state, &name);
    ensure_seq_once(&state, &phys, &*da).await.map_err(err)?;
    let target = resolve_target(&state, &name)?;
    let in_tx = matches!(&target, Target::Tx(_));
    let start = std::time::Instant::now();
    let alloc_res: Result<Row, JsErrorBox> = match target {
        // 池路径：表刚确保过，正常一次成功；仍保留「失败→再建表→重试」兜底（外部 drop 等）。
        Target::Pool(da) => next_seq_via_pool(&da, &seq).await.map_err(err),
        // tx 路径：搭车调用方事务的连接（建表已在池上完成，此处不再触碰 DDL）。
        Target::Tx(t) => {
            let dial = da.dialect();
            let mut s = t.session.lock().await;
            seq_next(&mut **s, dial, &seq).await.map_err(err)
        }
    };
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    // 序列分配本质是一次取号 SQL，按合成名追踪（不含业务表名）。
    let sql = format!("db.nextSeq(`{seq}`)");
    trace_sql(
        &state,
        &sql,
        &[],
        &name,
        in_tx,
        ms,
        None,
        alloc_res.is_ok(),
        alloc_res.as_ref().err().map(|e| e.to_string()),
        None,
        None,
    );
    let mut out = alloc_res?;
    // 出口护栏：超界整数降十进制字符串（与 DB 读值同契约，见 jsnum）。
    super::jsnum::sanitize_js_numbers(&mut out);
    Ok(out)
}

/// JS `db.dialect()`：报连接引擎名（"sqlite" | "mysql" | "postgres"）。方言是 accessor
/// 属性、与事务无关，不走 resolve_target（v0.1.61，`_shared/search` 等共享模块按此分派
/// 方言 SQL；业务 handler 慎用——分支方言即放弃跨引擎可移植承诺）。
#[op2]
#[string]
pub fn op_db_dialect(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
) -> Result<String, JsErrorBox> {
    // 直接报描述符声明名（开放契约下可能是 mysql/postgres/sqlite 之外的方言名，
    // 不再经 known 子集映射——消除「未知方言静默回落 sqlite」的旧行为）。
    let da = super::query::lookup(&state, &name)?;
    Ok(da.dialect_info().name.clone())
}

/// db.listTables(prefix?)（v0.1.82）：方言无关的目录自省，返回当前库业务可见表名
/// （含未登记进 SchemaRegistry 的表——api-only 方案 / 历史遗留表），消除「按方言分支
/// information_schema / sqlite_master」的裸 db.query。SQL 文本一律引擎常量、前缀经
/// `escape_like` 转义后走绑定参数（LIKE ? ESCAPE '|'——'|' 不与任何方言的字符串转义
/// 规则冲突，避开 mysql 反斜杠转义 / PG standard_conforming_strings 的分叉）。能力面
/// 严格弱于 db.query，价值纯在跨方言可移植；表名是返回的数据，不进 SQL 拼接。
#[op2]
#[serde]
pub async fn op_db_list_tables(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[string] prefix: Option<String>,
) -> Result<Vec<String>, JsErrorBox> {
    let target = resolve_target(&state, &name)?;
    let in_tx = matches!(&target, Target::Tx(_));
    // 门控（T8）：未知方言（不在 builder-known 子集）一律结构化 unsupported，绝不回落 Sqlite。
    let dialect = match &target {
        Target::Tx(t) => require_known_dialect(&t.dialect_info, "list_tables")?,
        Target::Pool(da) => require_known_dialect(da.dialect_info(), "list_tables")?,
    };
    // 授权维（安全 B1，v0.1.90）：内省也须过授权，否则 db.allowed() 的
    // 「只列已授权库」是假承诺——未授权主体不该靠 listTables 枚举出别的库的表结构。
    let alias = super::guard::bound_db(&state, &name);
    super::db_policy::granted_only(&state, &alias)?;
    // 投影列统一叫 `name`：sqlite_master 本就叫 name；information_schema 的
    // table_name 取别名。系统表排除只在 sqlite 需要（mysql/pg 已被
    // DATABASE()/current_schema() 的范围天然排除）。
    let mut sql = match dialect {
        Dialect::Sqlite => "SELECT name FROM sqlite_master WHERE type = 'table' AND substr(name, 1, 7) <> 'sqlite_'".to_string(),
        Dialect::MySql => {
            "SELECT table_name AS name FROM information_schema.tables WHERE table_schema = DATABASE()"
                .to_string()
        }
        Dialect::Postgres => {
            // `table_name` 是 `sql_identifier`（`name`）域，sqlx Any 驱动解不了该 OID，
            // 且「空库不触发、建表后才崩」——必须 `::text`（与 introspect.rs 同口径）。
            "SELECT table_name::text AS name FROM information_schema.tables WHERE table_schema = current_schema()"
                .to_string()
        }
    };
    let mut params: Vec<Value> = Vec::new();
    if let Some(p) = prefix {
        // 前缀列不能用投影别名（mysql 的 WHERE 不认 SELECT 别名），按方言落真列名。
        let col = match dialect {
            Dialect::Sqlite => "name",
            _ => "table_name",
        };
        sql.push_str(&format!(" AND {col} LIKE ? ESCAPE '|'"));
        params.push(Value::String(format!(
            "{}%",
            super::query::escape_like(&p, '|')
        )));
    }
    sql.push_str(" ORDER BY name");
    // 占位符统一改写（0.1.67）：`?` 是 oj 唯一占位符，PG 由宿主改写 `$n`——
    // 与三个裸 SQL op 同一插桩点，不许绕过。
    let placeholder = match &target {
        Target::Tx(t) => t.dialect_info.caps.placeholder,
        Target::Pool(da) => da.dialect_info().caps.placeholder,
    };
    let norm = normalize_and_trace(&state, placeholder, in_tx, &name, &sql, &params)?;
    let start = std::time::Instant::now();
    let res = match target {
        Target::Pool(da) => da.query_with_params(&norm, &params).await,
        Target::Tx(t) => t.session.lock().await.query(&norm, &params).await,
    };
    let ms = elapsed_ms(start);
    match res {
        Ok(rows) => {
            trace_sql(
                &state,
                &sql,
                &params,
                &name,
                in_tx,
                ms,
                Some(rows.len() as i64),
                true,
                None,
                None,
                None,
            );
            // 契约是 Promise<string[]>：投影列固定叫 name，抽出来摊平（防驱动把
            // 表名包成对象/带 schema 前缀——只取纯名字）。
            Ok(rows
                .into_iter()
                .filter_map(|r| r.get("name").and_then(|v| v.as_str()).map(String::from))
                .collect())
        }
        Err(e) => {
            let em = e.to_string();
            trace_sql(
                &state,
                &sql,
                &params,
                &name,
                in_tx,
                ms,
                None,
                false,
                Some(em.clone()),
                None,
                None,
            );
            Err(JsErrorBox::generic(em))
        }
    }
}

/// 表名入参校验（安全 B3 / B4）：长度 > 128、含 NUL / 控制字符、或（sqlite）
/// 含 `.` → 视为非法，内省 op 返回空数组而非把畸形输入交给驱动（避免 500，也挡
/// `other.secrets` 形式的跨库内省）。
fn valid_introspect_table(dialect: Dialect, table: &str) -> bool {
    if table.len() > 128 {
        return false;
    }
    if table.bytes().any(|b| b == 0 || b < 0x20) {
        return false;
    }
    if dialect == Dialect::Sqlite && table.contains('.') {
        return false;
    }
    true
}

/// `db.listColumns(table)` / `listIndexes(table)` / `listForeignKeys(table)`（v0.1.90，L2）。
///
/// 三者共用 [`introspect`] 的方言 SQL 常量（表名**只经绑定参数**，零标识符内联），
/// 由宿主按方言固定 `LIMIT` 截断到 [`introspect::MAX_ROWS`]。返回结构与排序三方言一致
/// （见 `introspect.rs` 的 `columns` / `indexes` / `foreign_keys`）。
///
/// 授权：只过 [`db_policy::granted_only`]（内省恒只读，但授权维必须过——见该 fn 文档）。
/// 未知方言 → 结构化 `OJ_DB_UNSUPPORTED`（`data.capability` 点名），绝不回落 sqlite。
async fn introspect(
    state: Rc<RefCell<OpState>>,
    name: String,
    table: String,
    kind: super::introspect::Kind,
) -> Result<Vec<Value>, JsErrorBox> {
    let target = resolve_target(&state, &name)?;
    let in_tx = matches!(&target, Target::Tx(_));
    let dialect = match &target {
        Target::Tx(t) => require_known_dialect(&t.dialect_info, kind.capability())?,
        Target::Pool(da) => require_known_dialect(da.dialect_info(), kind.capability())?,
    };
    // 授权维（安全 B1）：未授权主体不该枚举出别的库的表结构。
    let alias = super::guard::bound_db(&state, &name);
    super::db_policy::granted_only(&state, &alias)?;

    // 表名入参校验：畸形 / 跨库前缀 → 空数组（不报错、不泄漏）。
    if !valid_introspect_table(dialect, &table) {
        return Ok(Vec::new());
    }

    let mut sql = super::introspect::sql(kind, dialect).to_string();
    sql.push_str(&format!(" LIMIT {}", super::introspect::MAX_ROWS));
    let params = vec![Value::String(table)];

    let placeholder = match &target {
        Target::Tx(t) => t.dialect_info.caps.placeholder,
        Target::Pool(da) => da.dialect_info().caps.placeholder,
    };
    let norm = normalize_and_trace(&state, placeholder, in_tx, &name, &sql, &params)?;
    let start = std::time::Instant::now();
    // 强制脱敏（安全 B6）：trace 的 params 记为占位，表名不因 `redact=false` 进日志。
    let trace_params = &[Value::String("<db introspection>".to_string())];
    let res = match &target {
        Target::Pool(da) => da.query_with_params(&norm, &params).await,
        Target::Tx(t) => t.session.lock().await.query(&norm, &params).await,
    };
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    match res {
        Ok(rows) => {
            let out = match kind {
                super::introspect::Kind::Columns => super::introspect::columns(rows),
                super::introspect::Kind::Indexes => super::introspect::indexes(rows),
                super::introspect::Kind::ForeignKeys => super::introspect::foreign_keys(rows),
                // catalog 类只从 op_db_catalog 进（parse_catalog 白名单），L2 通路不可达。
                _ => unreachable!("catalog kinds do not flow through the L2 introspect path"),
            };
            trace_sql(
                &state,
                &sql,
                trace_params,
                &name,
                in_tx,
                ms,
                Some(out.len() as i64),
                true,
                None,
                None,
                None,
            );
            Ok(out)
        }
        Err(e) => {
            let em = e.to_string();
            trace_sql(
                &state,
                &sql,
                trace_params,
                &name,
                in_tx,
                ms,
                None,
                false,
                Some(em.clone()),
                None,
                None,
            );
            Err(JsErrorBox::generic(em))
        }
    }
}

/// db.listColumns(table)（v0.1.90）：列结构内省。
/// 返回 `{name, nativeType, nullable, default, primaryKey}[]`。
#[op2]
#[serde]
pub async fn op_db_list_columns(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[string] table: String,
) -> Result<Vec<Value>, JsErrorBox> {
    introspect(state, name, table, super::introspect::Kind::Columns).await
}

/// db.listIndexes(table)（v0.1.90）：索引内省。
/// 返回 `{name, unique, primary, origin, columns}[]`（origin ∈ `c`/`u`/`pk`）。
#[op2]
#[serde]
pub async fn op_db_list_indexes(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[string] table: String,
) -> Result<Vec<Value>, JsErrorBox> {
    introspect(state, name, table, super::introspect::Kind::Indexes).await
}

/// db.listForeignKeys(table)（v0.1.90）：外键内省。
/// 返回 `{name, columns, refTable, refColumns}[]`（sqlite 无名 → `name` 为 null）。
#[op2]
#[serde]
pub async fn op_db_list_foreign_keys(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[string] table: String,
) -> Result<Vec<Value>, JsErrorBox> {
    introspect(state, name, table, super::introspect::Kind::ForeignKeys).await
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

    /// 量纲钉子（变异测试）：`elapsed_ms` 的 `*`→`+`/`/` 变异必死。
    /// sleep(3ms) 的下界由时钟语义保证、上界千倍宽松——这不是「固定 sleep 当同步」
    /// 的 flake 形态，被测对象就是计时本身。
    #[test]
    fn elapsed_ms_scales_to_millis() {
        let start = std::time::Instant::now();
        std::thread::sleep(std::time::Duration::from_millis(3));
        let ms = elapsed_ms(start);
        assert!(ms >= 3.0, "ms {ms}");
        assert!(ms < 1_000.0, "ms {ms}");
    }
    use crate::bridge::Extras;
    use crate::bridge::client_db::ClientDbRegistry;
    use crate::bridge::db_backend::DbBackendRegistry;
    use crate::bridge::db_policy::{
        AllowList, DbAccess, DbAccessOverride, DbAccessSection, DbClientSection,
    };
    use crate::bridge::{Bridge, InMemoryAccessor, InMemoryKV, SchemaRegistry};
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::Arc;

    /// 守护测试（0.1.67，评审 A1）：把「裸 SQL 出口 ⊆ {query, exec, stream_open}」
    /// 从文档承诺变成 CI 资产——枚举 op 注册表的全部 op_db_*，新增 op 必须先过
    /// 本白名单（属裸 SQL 出口则插桩 normalize_and_trace，否则更新此表并说明）。
    ///
    /// `op_db_allowed`（v0.1.89）：访问控制自省出口，**不执行** SQL，故无需插桩
    /// normalize_and_trace——登记为已知非裸 SQL 出口。
    #[test]
    fn raw_sql_op_exits_are_pinned() {
        const WHITELIST: [&str; 23] = [
            "op_db_allowed",
            "op_db_as_system",
            "op_db_as_tenant",
            "op_db_catalog",
            "op_db_dialect",
            "op_db_exec",
            "op_db_has",
            "op_db_list_columns",
            "op_db_list_foreign_keys",
            "op_db_list_indexes",
            "op_db_list_tables",
            "op_db_next_seq",
            "op_db_query",
            "op_db_query_build",
            "op_db_query_sql",
            "op_db_sql_profile",
            "op_db_stream_abort",
            "op_db_stream_close",
            "op_db_stream_next",
            "op_db_stream_open",
            "op_db_tx_begin",
            "op_db_tx_commit",
            "op_db_tx_rollback",
        ];
        let src = include_str!("mod.rs");
        let mut found: Vec<&str> = Vec::new();
        for line in src.lines() {
            let t = line.trim();
            let Some((module, op)) = t.split_once("::") else {
                continue;
            };
            if module != "db" && module != "query" {
                continue;
            }
            let op = op.trim_end_matches(',');
            if op.starts_with("op_db_") {
                found.push(op);
            }
        }
        found.sort_unstable();
        found.dedup();
        assert_eq!(
            found, WHITELIST,
            "op 注册表出现新 op_db_*：属裸 SQL 出口必须插桩 normalize_and_trace（占位符统一），否则更新白名单"
        );
    }

    /// `db.dialect()`：报引擎名；tx 回调对象同面（v0.1.61，`_shared/search` 依赖此分派）。
    #[tokio::test(flavor = "current_thread")]
    async fn db_dialect_reports_engine_name() {
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
                     const inTx = await db.tx(async (tx) => tx.dialect());
                     json.ok({ d: await db.dialect(), inTx });
                   })().catch(e => json.fail(500, String(e)));"#,
                crate::bridge::RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(v["data"]["d"], "sqlite", "{v}");
        assert_eq!(v["data"]["inTx"], "sqlite", "tx 对象同样暴露 dialect：{v}");
    }

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
    /// flavor 必须 current_thread：deno op 内 deno_unsync::spawn 断言 CurrentThread，
    /// multi_thread 下 env 一设即 SIGABRT（该用例此前从未真跑过——env 未设静默 skip 假绿）。
    #[tokio::test(flavor = "current_thread")]
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
    /// flavor 同上必须 current_thread（multi_thread 即 SIGABRT，存量假绿已修）。
    #[tokio::test(flavor = "current_thread")]
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

    // —— §3④ `_shared/search.ts` 三方言真库回归（v0.1.68 修复 pg 占位符）——
    //
    // search 共享模块此前自写 `$1`（likeFallback 多字段 OR、pg FTS 非 highlight），与
    // 宿主统一占位符改写的参数计数校验冲突 → pg 上 `search.query` 一律报错
    // `placeholder count 1 != params N`。修复后统一 `?` 交宿主改写。本用例把
    // `sample/src/_platform/_shared/search.ts` 转译后内联进 bridge，三种数据库跑同一
    // 脚本：多字段表（≥2 字段，验证绑定数量正确）× FTS 非 highlight/highlight ×
    // 2 字/1 字 LIKE 回退 × remove 后零命中 × 大小写混合查询（LIKE 回退与 FTS 两路）。
    // 需设 OJ_TEST_PG / OJ_TEST_MYSQL 才跑对应数据库分支（sqlite 内存库分支常跑）。

    /// 转译 sample 的 search 共享模块为可内联 JS（script 上下文无模块系统：剥 export，
    /// type/interface 已被转译剥离）。
    fn search_shared_js() -> String {
        let root = crate::bridge::ffi::workspace_root();
        let path = root.join("sample/src/_platform/_shared/search.ts");
        let src = std::fs::read_to_string(&path).expect("read search.ts");
        let js = crate::bridge::transpile::transpile_src(&path, &src).expect("transpile search.ts");
        js.replace("export ", "")
    }

    /// search 三方言统一脚本：ensure/index/query(FTS+回退+高亮)/remove 全链路。
    const SEARCH_SHARED_HARNESS: &str = r#"
(async () => {
  const T = "_oj_search_real";
  await db.exec(`drop table if exists ${T}`, []);
  await db.exec(`create table ${T} (id integer primary key, title text, body text)`, []);
  const search = createSearch(db);
  await search.ensureIndex(T, { fields: ["title", "body"] });
  await search.index(T, { id: 1, title: "部署手册", body: "生产 环境" });
  await search.index(T, { id: 2, title: "公告", body: "本周例会" });
  await search.index(T, { id: 3, title: "Release Notes", body: "Kafka Cluster" });
  const r = {};
  // ① FTS ≥3 字非 highlight（pg 非高亮路径修复前 count 1 != 2）
  r.fts = (await search.query(T, "部署手册")).map((h) => Number(h.id));
  // ② FTS highlight（pg ts_headline 为第三处占位符）
  const hl = await search.query(T, "部署手册", { highlight: true });
  r.hlIds = hl.map((h) => Number(h.id));
  r.hlMark = JSON.stringify((hl[0] && hl[0].fragments) || {}).includes("<mark>");
  // ③ 2 字 LIKE 回退，多字段 OR（修复前 pg count 1 != 2）
  r.two = (await search.query(T, "部署")).map((h) => Number(h.id));
  // ④ 1 字 LIKE 回退
  r.one = (await search.query(T, "公")).map((h) => Number(h.id));
  // ⑤ remove 后 FTS 不再命中
  await search.remove(T, 2);
  r.afterRemove = (await search.query(T, "本周例会")).map((h) => Number(h.id));
  // ⑥ 大小写混合：2 字回退（小写查大写原文 / 大写查小写查询词）——pg 用
  //    ILIKE 对齐 sqlite/mysql 的不敏感语义（修复前 pg 上两者均空）
  r.kaLower = (await search.query(T, "ka")).map((h) => Number(h.id));
  r.kaUpper = (await search.query(T, "KA")).map((h) => Number(h.id));
  // ⑦ 大小写混合：FTS 路径（trigram/'simple'/ngram 皆折叠大小写，大写查询词命中）
  r.ftsCase = (await search.query(T, "RELEASE")).map((h) => Number(h.id));
  json.ok(r);
})().catch(e => json.fail(500, String(e)));"#;

    /// 单个数据库分支执行 + 断言（sqlite 内存库 / pg / mysql 共用）。
    async fn search_shared_check(js: &str, url: &str, tag: &str) {
        let db = crate::bridge::SqlxAccessor::arc(url).await.unwrap();
        let b = Bridge::with_dbs_and_loader(
            std::collections::HashMap::from([("default".to_string(), db as _)]),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            None,
            Default::default(),
        );
        let src = format!("{js}\n{SEARCH_SHARED_HARNESS}");
        let cap = b
            .run_with(&src, crate::bridge::RequestInfo::default())
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "[{tag}] search harness failed: {v}");
        assert_eq!(v["data"]["fts"], json!([1]), "[{tag}] FTS ≥3字: {v}");
        assert_eq!(v["data"]["hlIds"], json!([1]), "[{tag}] FTS highlight: {v}");
        assert_eq!(
            v["data"]["hlMark"],
            json!(true),
            "[{tag}] highlight <mark>: {v}"
        );
        assert_eq!(v["data"]["two"], json!([1]), "[{tag}] 2字 LIKE 回退: {v}");
        assert_eq!(v["data"]["one"], json!([2]), "[{tag}] 1字 LIKE 回退: {v}");
        assert_eq!(
            v["data"]["afterRemove"],
            json!([]),
            "[{tag}] remove 后: {v}"
        );
        // ⑥ 大小写：row3 body "Kafka Cluster"，row2 已 remove 不干扰
        assert_eq!(
            v["data"]["kaLower"],
            json!([3]),
            "[{tag}] 回退小写查询词: {v}"
        );
        assert_eq!(
            v["data"]["kaUpper"],
            json!([3]),
            "[{tag}] 回退大写查询词: {v}"
        );
        // ⑦ FTS 路径大小写折叠（trigram/'simple'/ngram）
        assert_eq!(
            v["data"]["ftsCase"],
            json!([3]),
            "[{tag}] FTS 大写查询词: {v}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn search_shared_module_on_real_dbs() {
        let js = search_shared_js();
        // sqlite 内存库分支：无 env 依赖，验证 JS 本体与三方言共用路径。
        search_shared_check(&js, "sqlite::memory:", "sqlite").await;
        let pg = std::env::var("OJ_TEST_PG").ok().filter(|s| !s.is_empty());
        let my = std::env::var("OJ_TEST_MYSQL")
            .ok()
            .filter(|s| !s.is_empty());
        if pg.is_none() && my.is_none() {
            eprintln!("skip: OJ_TEST_PG / OJ_TEST_MYSQL unset");
        }
        if let Some(url) = pg {
            search_shared_check(&js, &url, "pg").await;
        }
        if let Some(url) = my {
            search_shared_check(&js, &url, "mysql").await;
        }
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

    // ── T6：方言能力模型 —— known() 边界 + 两层模型 dialect_info() ──
    #[test]
    fn known_dialect_rejects_unknown_and_accepts_known() {
        // Given known("oracle") → None（未进入 known 子集即为开放契约下的「未知」）
        assert_eq!(Dialect::known("oracle"), None);
        assert_eq!(Dialect::known("sqlserver"), None);
        // Given known("postgres") → Some(Postgres) 等已知子集
        assert_eq!(Dialect::known("postgres"), Some(Dialect::Postgres));
        assert_eq!(Dialect::known("postgresql"), Some(Dialect::Postgres));
        assert_eq!(Dialect::known("mysql"), Some(Dialect::MySql));
        assert_eq!(Dialect::known("sqlite"), Some(Dialect::Sqlite));
    }

    #[test]
    fn sqlite_dialect_info_is_default_anchor() {
        let info = sqlite_dialect_info();
        assert_eq!(info.name, "sqlite");
        assert_eq!(info.caps.placeholder, PlaceholderStyle::Q);
        assert_eq!(info.caps.id, IdStrategy::TwoStep);
        assert!(info.caps.stream);
        assert!(!info.caps.cancel);
        // 懒初始化幂等：多次取是同一 &'static
        assert!(std::ptr::eq(info, sqlite_dialect_info()));
    }

    #[test]
    fn inmemory_accessor_and_tx_default_dialect_info_is_sqlite() {
        // Given 默认 accessor（InMemoryAccessor）When dialect_info() Then 与 sqlite 默认锚一致
        let da = InMemoryAccessor::new();
        assert_eq!(da.dialect_info().name, "sqlite");
        assert_eq!(da.dialect_info().caps, sqlite_dialect_info().caps);
        // 默认 TxSession（begin 产出的 InMemoryTx）走 default → 同样 sqlite 锚
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let tx = rt.block_on(da.begin()).unwrap();
        assert_eq!(tx.dialect_info().name, "sqlite");
        assert_eq!(tx.dialect_info().caps, sqlite_dialect_info().caps);
    }

    #[test]
    fn active_tx_carries_dialect_info_snapshot() {
        // Given tx begin When 后续 in-tx 改写/exec 路径 Then 使用 ActiveTx.dialect_info
        // （begin 时已快照，而非再次查询 accessor）。此处钉死「快照随 ActiveTx 走」的契约。
        let info = DialectInfo {
            name: "postgres".to_string(),
            caps: DialectCaps {
                placeholder: PlaceholderStyle::Dollar,
                id: IdStrategy::Returning,
                stream: true,
                cancel: true,
            },
        };
        let tx = ActiveTx {
            db: "default".to_string(),
            session: tokio::sync::Mutex::new(Box::new(InMemoryTx {
                inner: Arc::new(RwLock::new(Default::default())),
            })),
            dialect_info: info.clone(),
        };
        // in-tx 路径（db.rs:501/621/918 的 Target::Tx(t) => t.dialect_info）读此快照
        assert_eq!(tx.dialect_info.name, "postgres");
        assert_eq!(tx.dialect_info.caps.placeholder, PlaceholderStyle::Dollar);
        assert_eq!(tx.dialect_info.caps.id, IdStrategy::Returning);
    }

    // ── T8：六门控点 —— 未知方言（不在 builder-known 子集）一律结构化 unsupported ──

    /// 门控核心：oracle 不在 known 子集 → Err(BridgeError::unsupported)，
    /// code==OJ_DB_UNSUPPORTED、data.capability==对应能力名；已知方言透传枚举。
    #[test]
    fn require_known_dialect_gates_unknown_and_passes_known() {
        let oracle = DialectInfo {
            name: "oracle".to_string(),
            caps: DialectCaps {
                placeholder: PlaceholderStyle::Colon,
                id: IdStrategy::None,
                stream: false,
                cancel: false,
            },
        };
        let err = require_known_dialect(&oracle, "query_build").unwrap_err();
        assert_eq!(
            err.category,
            crate::bridge::db_error::DbErrorCategory::Unsupported
        );
        assert_eq!(
            crate::bridge::db_error::code_of(err.category),
            "OJ_DB_UNSUPPORTED"
        );
        assert_eq!(
            err.data.get("capability").and_then(Value::as_str),
            Some("query_build")
        );
        assert_eq!(
            err.data.get("dialect").and_then(Value::as_str),
            Some("oracle")
        );
        assert!(err.message.contains("oracle"), "{}", err.message);
        assert!(err.message.contains("query_build"), "{}", err.message);

        // 已知方言正常透传
        let pg = sqlite_dialect_info().clone();
        assert_eq!(
            require_known_dialect(&pg, "query_build").unwrap(),
            Dialect::Sqlite
        );
        let my = DialectInfo {
            name: "mysql".to_string(),
            caps: DialectCaps {
                placeholder: PlaceholderStyle::Q,
                id: IdStrategy::TwoStep,
                stream: true,
                cancel: false,
            },
        };
        assert_eq!(
            require_known_dialect(&my, "query_build").unwrap(),
            Dialect::MySql
        );
    }

    /// 测试用「未知方言」accessor：声明 oracle，但 oj 不认识它的 builder（无 sea-query 构造器）。
    struct OracleAccessor;
    #[async_trait]
    impl DataAccessor for OracleAccessor {
        async fn query_with_params(&self, _sql: &str, _params: &[Value]) -> BridgeResult<Vec<Row>> {
            // 裸 SQL 路径对开放方言可用（§5.5）——mock 如实返回空结果集。
            Ok(Vec::new())
        }
        async fn exec_with_params(&self, _sql: &str, _params: &[Value]) -> BridgeResult<i64> {
            // 同上：裸 exec 不打 known 子集门控，调用必须能到达 accessor。
            Ok(7)
        }
        fn dialect_info(&self) -> &DialectInfo {
            static INFO: OnceLock<DialectInfo> = OnceLock::new();
            INFO.get_or_init(|| DialectInfo {
                name: "oracle".to_string(),
                caps: DialectCaps {
                    placeholder: PlaceholderStyle::Colon,
                    id: IdStrategy::None,
                    stream: false,
                    cancel: false,
                },
            })
        }
    }

    /// Given oracle accessor When db.table(...).select().all()（query_build 路径）
    /// Then 结构化 unsupported，message 点名 oracle 与能力名（不回落 Sqlite、不静默执行）。
    #[tokio::test(flavor = "current_thread")]
    async fn oracle_query_build_is_unsupported() {
        let b = Bridge::new(Arc::new(OracleAccessor), Arc::new(InMemoryKV::new()));
        let cap = b
            .run(r#"db.table("t").select(["id"]).all().then(r => json.ok(r)).catch(e => json.fail(500, String(e)));"#)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 500);
        let msg = v["msg"].as_str().unwrap();
        assert!(msg.contains("oracle"), "{msg}");
        assert!(msg.contains("query_build"), "{msg}");
    }

    /// Given oracle accessor When db.listTables()（list_tables 门控）
    /// Then 结构化 unsupported，message 点名 oracle 与 list_tables。
    #[tokio::test(flavor = "current_thread")]
    async fn oracle_list_tables_is_unsupported() {
        let b = Bridge::new(Arc::new(OracleAccessor), Arc::new(InMemoryKV::new()));
        let cap = b
            .run(r#"db.listTables().then(r => json.ok(r)).catch(e => json.fail(500, String(e)));"#)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 500);
        let msg = v["msg"].as_str().unwrap();
        assert!(msg.contains("oracle"), "{msg}");
        assert!(msg.contains("list_tables"), "{msg}");
    }

    /// 未知方言（oracle）下，三个结构内省 op 同样走 known 子集门控 → 结构化
    /// `OJ_DB_UNSUPPORTED`、`data.capability` 点名对应能力（绝不回落 sqlite）。
    #[tokio::test(flavor = "current_thread")]
    async fn oracle_introspect_ops_are_unsupported() {
        let b = Bridge::new(Arc::new(OracleAccessor), Arc::new(InMemoryKV::new()));
        for (js, cap) in [
            (
                r#"db.listColumns('t').then(r => json.ok(r)).catch(e => json.fail(500, String(e)));"#,
                "list_columns",
            ),
            (
                r#"db.listIndexes('t').then(r => json.ok(r)).catch(e => json.fail(500, String(e)));"#,
                "list_indexes",
            ),
            (
                r#"db.listForeignKeys('t').then(r => json.ok(r)).catch(e => json.fail(500, String(e)));"#,
                "list_foreign_keys",
            ),
        ] {
            let resp = b.run(js).await.unwrap();
            let v: Value = serde_json::from_slice(&resp.body).unwrap();
            assert_eq!(v["code"], 500, "{v}");
            let msg = v["msg"].as_str().unwrap();
            assert!(msg.contains("oracle"), "{msg}");
            assert!(msg.contains(cap), "{cap} not in {msg}");
        }
    }

    /// Given oracle accessor When db.nextSeq("s")（next_seq 门控）
    /// Then 结构化 unsupported，message 点名 oracle 与 next_seq。
    #[tokio::test(flavor = "current_thread")]
    async fn oracle_next_seq_is_unsupported() {
        let b = Bridge::new(Arc::new(OracleAccessor), Arc::new(InMemoryKV::new()));
        let cap = b
            .run(r#"db.nextSeq("s").then(r => json.ok(r)).catch(e => json.fail(500, String(e)));"#)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 500);
        let msg = v["msg"].as_str().unwrap();
        assert!(msg.contains("oracle"), "{msg}");
        assert!(msg.contains("next_seq"), "{msg}");
    }

    /// Given oracle accessor When db.exec / db.query（裸 SQL 路径）
    /// Then 不门控、直达 accessor（设计 §5.5：raw 路径对开放方言全量可用）。
    #[tokio::test(flavor = "current_thread")]
    async fn oracle_raw_exec_and_query_are_available() {
        let b = Bridge::new(Arc::new(OracleAccessor), Arc::new(InMemoryKV::new()));
        let cap = b
            .run(r#"db.exec("update t set a = 1").then(r => json.ok(r)).catch(e => json.fail(500, String(e)));"#)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(v["data"], 7, "exec 必须直达 accessor: {v}");

        let cap = b
            .run(r#"db.query("select 1 from dual").then(r => json.ok(r)).catch(e => json.fail(500, String(e)));"#)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert!(v["data"].as_array().is_some_and(|a| a.is_empty()), "{v}");
    }

    /// Given oracle accessor When db.nextSeq 抛结构化 unsupported Then JS 侧 `e.code` /
    /// `e.data` / `e.retryable` / `e.sessionDisposition` 透传（data 经 normDbErr 从 JSON 串
    /// 解析为对象、retryable 从 1/0 归一为布尔），handler 可据此结构化判别能力缺失，
    /// 而非只能靠 message 文本匹配。
    #[tokio::test(flavor = "current_thread")]
    async fn oracle_error_exposes_code_and_data_to_js() {
        let b = Bridge::new(Arc::new(OracleAccessor), Arc::new(InMemoryKV::new()));
        let cap = b
            .run(
                r#"db.nextSeq("s").then(r => json.ok(r)).catch(e => json.ok({ code: e.code, data: e.data, retryable: e.retryable, sd: e.sessionDisposition, msg: e.message }));"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        // 信封 code 为 0（handler 成功返回）；结构化信息在 data 载荷内。
        assert_eq!(v["code"], 0, "{v}");
        let p = &v["data"];
        assert_eq!(p["code"], "OJ_DB_UNSUPPORTED");
        // e.data 已被 normDbErr 解析为对象（capability / dialect）。
        assert_eq!(p["data"]["capability"], "next_seq");
        assert_eq!(p["data"]["dialect"], "oracle");
        // retryable 从 1/0 归一为布尔；sessionDisposition 为小写串。
        assert_eq!(p["retryable"], false, "e.retryable 必须是布尔: {v}");
        assert_eq!(p["sd"], "keep", "e.sessionDisposition: {v}");
        assert!(p["msg"].as_str().unwrap().contains("oracle"), "{v}");
    }

    /// T9 文本冻结快照：既有方言的 db 错误抵达 JS 时 `e.message` **逐字节**等于插件给出的
    /// message 原文。宿主不再叠 `ffi <op>: ` 前缀（设计 §5.1）——ABI 11 及以前该前缀与插件
    /// 自身的 `db query: ` 叠加成 `ffi db query: db query: …` 双前缀；本条钉死去重后的文本，
    /// 任何再改（含重新加前缀）都必须显式更新此断言并写 CHANGELOG。
    #[test]
    fn db_error_text_snapshot_is_frozen() {
        use deno_error::JsErrorClass;
        let raw = r#"{"category":"constraint","retryable":false,"session_disposition":"keep","message":"db query: Duplicate entry 'neo' for key 'PRIMARY'","data":{"vendor_code":1062}}"#;
        let js = to_js_err(Box::new(crate::bridge::db_error::parse_raw(raw)));
        let msg = JsErrorClass::get_message(&js);
        assert_eq!(
            msg, "db query: Duplicate entry 'neo' for key 'PRIMARY'",
            "既有 db 错误文本必须逐字节冻结（设计 §5.1：宿主不叠 ffi 前缀）"
        );
        // 反向断言：宿主前缀一旦回流，上面那条会红；这条把「为什么红」写清楚。
        assert!(!msg.starts_with("ffi "), "宿主不得再叠 ffi 前缀: {msg}");
        // 非信封（裸文本）路径同样原样透传，不额外加壳。
        let js = to_js_err("dialect not supported".into());
        assert_eq!(JsErrorClass::get_message(&js), "dialect not supported");
    }

    /// 回归：已知三方言的既有行为零改动（sqlite builder 正常出结果）。
    /// 构造器走 with_opts + SchemaRegistry（db.table 查询构造器按白名单解析表/列）。
    #[tokio::test(flavor = "current_thread")]
    async fn known_sqlite_query_build_still_works() {
        let db = Arc::new(InMemoryAccessor::new());
        db.seed([json!({"id": 1, "name": "neo"})]);
        let registry = SchemaRegistry::new().table("t", &["id"], &["id", "name"]);
        let b = Bridge::with_opts(db, Arc::new(InMemoryKV::new()), registry, false);
        let cap = b
            .run(r#"db.table("t").select(["id","name"]).all().then(r => json.ok(r)).catch(e => json.fail(500, String(e)));"#)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(v["data"][0]["name"], "neo");
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

    /// SQL 执行追踪（v0.1.51）：dev 开启后，本请求画像累计 SQL 事件，`db.sqlProfile()` 可读取，
    /// 且参数默认脱敏（params 只记个数）。
    #[tokio::test(flavor = "current_thread")]
    async fn sql_trace_records_profile_and_redacts_params() {
        let db = crate::bridge::SqlxAccessor::arc("sqlite::memory:")
            .await
            .unwrap();
        db.exec_with_params("create table t (id integer primary key, v text)", &[])
            .await
            .unwrap();
        let extras = crate::bridge::Extras {
            sql_trace: crate::bridge::sql_trace::SqlTraceConfig {
                enabled: true,
                redact_params: true,
                slow_ms: 0.0,
                to_log: false,
            },
            ..Default::default()
        };
        let b = Bridge::with_dbs_and_loader(
            std::collections::HashMap::from([("default".to_string(), db as _)]),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            None,
            extras,
        );
        let cap = b
            .run_with(
                r#"(async () => {
                     await db.exec("insert into t (v) values (?)", ["secret"]);
                     const rows = await db.query("select * from t where v = ?", ["secret"]);
                     json.ok({ n: rows.length, prof: db.sqlProfile() });
                   })().catch(e => json.fail(500, String(e)));"#,
                crate::bridge::RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        // 2 条 SQL：insert + select。
        assert_eq!(
            v["data"]["prof"]["count"],
            json!(2),
            "画像应含 2 条 SQL: {v}"
        );
        // 参数脱敏：只记个数，不记 "secret"。
        assert_eq!(
            v["data"]["prof"]["events"][0]["params"],
            json!("1 params"),
            "参数默认脱敏: {v}"
        );
        assert!(
            v["data"]["prof"]["events"][0]["sql"]
                .as_str()
                .unwrap()
                .contains("insert into t"),
            "事件含 SQL 文本: {v}"
        );
    }

    /// SQL 执行追踪关闭（release 默认）时不累计画像，`db.sqlProfile()` 返回空对象。
    #[tokio::test(flavor = "current_thread")]
    async fn sql_trace_off_yields_empty_profile() {
        let db = crate::bridge::SqlxAccessor::arc("sqlite::memory:")
            .await
            .unwrap();
        db.exec_with_params("create table t (id integer primary key)", &[])
            .await
            .unwrap();
        // Extras 默认 sql_trace.enabled = false（release 行为）。
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
                     await db.exec("insert into t (id) values (1)");
                     json.ok({ prof: db.sqlProfile() });
                   })().catch(e => json.fail(500, String(e)));"#,
                crate::bridge::RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(v["data"]["prof"], json!({}), "关闭时画像为空对象: {v}");
    }

    /// 红线探针：即使 `redact_params: false`，画像与响应信封 `_sql` 也永远脱敏（只记个数），
    /// 参数原值绝不外泄到客户端（仅服务端 dev 日志可记）。
    #[tokio::test(flavor = "current_thread")]
    async fn sql_trace_envelope_always_redacts_even_when_redact_false() {
        let db = crate::bridge::SqlxAccessor::arc("sqlite::memory:")
            .await
            .unwrap();
        db.exec_with_params("create table t (id integer primary key, v text)", &[])
            .await
            .unwrap();
        let extras = crate::bridge::Extras {
            sql_trace: crate::bridge::sql_trace::SqlTraceConfig {
                enabled: true,
                redact_params: false, // 故意关脱敏：仅 dev 日志可记原值
                slow_ms: 0.0,
                to_log: false,
            },
            ..Default::default()
        };
        let b = Bridge::with_dbs_and_loader(
            std::collections::HashMap::from([("default".to_string(), db as _)]),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            None,
            extras,
        );
        let cap = b
            .run_with(
                r#"(async () => {
                     await db.exec("insert into t (v) values (?)", ["SECRET_PLAINTEXT"]);
                     json.ok({ prof: db.sqlProfile() });
                   })().catch(e => json.fail(500, String(e)));"#,
                crate::bridge::RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        let params = v["data"]["prof"]["events"][0]["params"].as_str().unwrap();
        // 画像永远脱敏：只记个数，绝不出现原值。
        assert_eq!(params, "1 params", "画像应永远脱敏: {v}");
        let body = String::from_utf8_lossy(&cap.body);
        assert!(
            !body.contains("SECRET_PLAINTEXT"),
            "响应体/信封永不泄露参数原值: {body}"
        );
    }

    /// 红线探针：db.table().select().all() 等构造器路径也须进画像（engineer P1-1）。
    #[tokio::test(flavor = "current_thread")]
    async fn sql_trace_records_query_builder_path() {
        let db = crate::bridge::SqlxAccessor::arc("sqlite::memory:")
            .await
            .unwrap();
        db.exec_with_params("create table t (id integer primary key, v text)", &[])
            .await
            .unwrap();
        let extras = crate::bridge::Extras {
            sql_trace: crate::bridge::sql_trace::SqlTraceConfig {
                enabled: true,
                redact_params: true,
                slow_ms: 0.0,
                to_log: false,
            },
            ..Default::default()
        };
        let b = Bridge::with_dbs_and_loader(
            std::collections::HashMap::from([("default".to_string(), db as _)]),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new().table("t", &["id"], &["id", "v"]),
            false,
            None,
            extras,
        );
        let cap = b
            .run_with(
                r#"(async () => {
                     await db.table("t").select().all();
                     json.ok({ prof: db.sqlProfile() });
                   })().catch(e => json.fail(500, String(e)));"#,
                crate::bridge::RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert!(
            v["data"]["prof"]["count"].as_u64().unwrap() >= 1,
            "构造器路径应进画像: {v}"
        );
        assert!(
            v["data"]["prof"]["events"][0]["sql"]
                .as_str()
                .unwrap()
                .to_lowercase()
                .contains("select"),
            "画像应含 SELECT: {v}"
        );
    }

    // —— §3③ 真库 e2e（design §3③）：占位符统一改写 × 真实 postgres，经
    // oj-db-postgres 插件后端（生产路径）。env-gated：OJ_TEST_PG 未设则静默 skip。
    // 注意：改写发生在宿主 op 层（normalize_and_trace），对 DataAccessor 实现透明——
    // 故插件后端与 core SqlxAccessor 走同一改写面；插件特有的方言缓存由 ffi mock 钉死。

    /// 编译并加载 oj-db-postgres cdylib → DbBackend（进程内一次；DSN 未设返回 None）。
    fn pg_backend() -> Option<Arc<dyn crate::bridge::DbBackend>> {
        let Ok(_dsn) = std::env::var("OJ_TEST_PG") else {
            eprintln!("skip: OJ_TEST_PG unset");
            return None;
        };
        static ONCE: std::sync::OnceLock<Option<Arc<dyn crate::bridge::DbBackend>>> =
            std::sync::OnceLock::new();
        ONCE.get_or_init(|| {
            let root = crate::bridge::ffi::workspace_root();
            let status = std::process::Command::new("cargo")
                .args(["build", "-p", "oj-db-postgres"])
                .current_dir(&root)
                .status()
                .expect("invoke cargo build for oj-db-postgres");
            assert!(status.success(), "oj-db-postgres build failed");
            let (prefix, ext) = if cfg!(target_os = "windows") {
                ("", "dll")
            } else if cfg!(target_os = "macos") {
                ("lib", "dylib")
            } else {
                ("lib", "so")
            };
            let built = root
                .join("target/debug")
                .join(format!("{prefix}oj_db_postgres.{ext}"));
            let dir = root
                .join("target/pg-e2e-plugins")
                .join(crate::bridge::ffi::triple());
            std::fs::create_dir_all(&dir).unwrap();
            let dst = dir.join(crate::bridge::ffi::plugin_file_name("db-postgres"));
            if !dst.exists() {
                std::fs::copy(&built, &dst).expect("copy oj-db-postgres artifact");
            }
            let loaded = crate::bridge::plugin_loader::load_scanned(
                &dir,
                crate::bridge::plugin_loader::host_context(),
                &|_, _| "{}".to_string(),
            )
            .expect("load oj-db-postgres plugin");
            loaded
                .iter()
                .find_map(|p| crate::bridge::plugin_loader::db_backend(p).ok().flatten())
        })
        .clone()
    }

    /// connect 真库（每测试一次连接，供 Bridge 注入）。
    async fn pg_accessor() -> Option<Arc<dyn DataAccessor>> {
        let dsn = std::env::var("OJ_TEST_PG").ok()?;
        let be = pg_backend()?;
        let cfg_dir = crate::bridge::ffi::workspace_root().join("target/pg-e2e-cfg");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        Some(be.connect(&dsn, &cfg_dir).await.expect("connect pg"))
    }

    /// 原始报障场景 + 字面量不误伤 + 方言自报：裸 `?` 写一次，PG 跑通。
    #[tokio::test(flavor = "current_thread")]
    async fn pg_e2e_raw_question_mark_roundtrip() {
        let Some(da) = pg_accessor().await else {
            return;
        };
        let registry = SchemaRegistry::new().table("ph_e2e_a", &["id"], &["id", "name"]);
        let b = Bridge::with_opts(da, Arc::new(InMemoryKV::new()), registry, false);
        let cap = b
            .run_with(
                r#"(async () => {
                     await db.exec("create table if not exists ph_e2e_a (id integer primary key, name text)", []);
                     await db.exec("delete from ph_e2e_a", []);
                     await db.exec("insert into ph_e2e_a (id, name) values (?, ?)", [1, "a"]);
                     await db.exec("insert into ph_e2e_a (id, name) values (?, 'a?b?')", [2]);
                     const rows = await db.query("select id, name from ph_e2e_a where id = ? and name = ?", [1, "a"]);
                     return json.ok({ dialect: db.dialect(), rows });
                   })().catch((e) => json.fail(500, String(e)));"#,
                crate::bridge::RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "裸 `?` 在 PG 必须跑通（原始报障场景）：{v}");
        assert_eq!(v["data"]["dialect"], json!("postgres"), "{v}");
        assert_eq!(v["data"]["rows"], json!([{"id": 1, "name": "a"}]), "{v}");
        // 字面量 `a?b?` 逐字相等——SQL 文本里的 `?` 未被改写误伤
        let cap = b
            .run_with(
                r#"db.query("select name from ph_e2e_a where id = ?", [2])
                     .then((rows) => json.ok({ name: rows[0].name }))
                     .catch((e) => json.fail(500, String(e)));"#,
                crate::bridge::RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(v["data"]["name"], json!("a?b?"), "字面量 `?` 不得误伤：{v}");
    }

    /// `$1` + `?` 混用续号；tx 内裸 `?` 同场景（评审 P4）。
    #[tokio::test(flavor = "current_thread")]
    async fn pg_e2e_mixed_numbering_and_tx() {
        let Some(da) = pg_accessor().await else {
            return;
        };
        let registry = SchemaRegistry::new().table("ph_e2e_b", &["id"], &["id", "name"]);
        let b = Bridge::with_opts(da, Arc::new(InMemoryKV::new()), registry, false);
        let cap = b
            .run_with(
                r#"(async () => {
                     await db.exec("create table if not exists ph_e2e_b (id integer primary key, name text)", []);
                     await db.exec("delete from ph_e2e_b", []);
                     await db.exec("insert into ph_e2e_b (id, name) values (?, ?)", [1, "a"]);
                     await db.exec("insert into ph_e2e_b (id, name) values (?, ?)", [2, "b"]);
                     // `$1` 引 params[0]，`?` 续号为 $2 引 params[1]（vendored P() 兼容面）
                     const mixed = (await db.query("select name from ph_e2e_b where id = $1 or name = ? order by name", [2, "a"])).map((r) => r.name);
                     await db.tx(async (tx) => {
                       await tx.exec("insert into ph_e2e_b (id, name) values (?, ?)", [9, "tx"]);
                       const rows = await tx.query("select name from ph_e2e_b where id = ?", [9]);
                       if (rows[0].name !== "tx") throw new Error("tx query failed");
                     });
                     const after = await db.query("select count(*) c from ph_e2e_b where name = ?", ["tx"]);
                     return json.ok({ mixed, txCount: after[0].c });
                   })().catch((e) => json.fail(500, String(e)));"#,
                crate::bridge::RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(v["data"]["mixed"], json!(["a", "b"]), "{v}");
        assert_eq!(
            v["data"]["txCount"],
            json!(1),
            "tx 内裸 `?` 写入未提交/已提交面: {v}"
        );
    }

    /// stream 出口带 `?` 带参（评审 P4：改写覆盖三出口之一）。
    #[tokio::test(flavor = "current_thread")]
    async fn pg_e2e_stream_with_params() {
        let Some(da) = pg_accessor().await else {
            return;
        };
        let registry = SchemaRegistry::new().table("ph_e2e_c", &["id"], &["id", "name"]);
        let b = Bridge::with_opts(da, Arc::new(InMemoryKV::new()), registry, false);
        let cap = b
            .run_with(
                r#"(async () => {
                     await db.exec("create table if not exists ph_e2e_c (id integer primary key, name text)", []);
                     await db.exec("delete from ph_e2e_c", []);
                     await db.exec("insert into ph_e2e_c (id, name) values (?, ?)", [1, "a"]);
                     await db.exec("insert into ph_e2e_c (id, name) values (?, ?)", [2, "b"]);
                     const ids = [];
                     // INT4 列是插件已知不可解码面，cast text 规避（与占位符改写无关）
                     await db.stream("select cast(id as text) as id from ph_e2e_c where id >= ? order by id", [1], {
                       onRow: (r) => ids.push(r.id),
                     });
                     return json.ok({ ids });
                   })().catch((e) => json.fail(500, String(e)));"#,
                crate::bridge::RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(
            v["data"]["ids"],
            json!(["1", "2"]),
            "stream 出口裸 `?` 必须跑通: {v}"
        );
    }

    /// 计数不匹配 fail-loud：文案含两个数字 + 正解 hint（击穿验证报错本身，评审 P3/P4）。
    #[tokio::test(flavor = "current_thread")]
    async fn pg_e2e_count_mismatch_message() {
        let Some(da) = pg_accessor().await else {
            return;
        };
        let registry = SchemaRegistry::new().table("ph_e2e_d", &["id"], &["id", "name"]);
        let b = Bridge::with_opts(da, Arc::new(InMemoryKV::new()), registry, false);
        let cap = b
            .run_with(
                r#"db.query("insert into ph_e2e_d (id, name) values (?, ?)", [1])
                     .then(() => json.ok({}))
                     .catch((e) => json.fail(500, String(e)));"#,
                crate::bridge::RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        let msg = v["msg"].as_str().unwrap_or_default();
        assert_ne!(v["code"], 0, "计数不匹配必须报错: {v}");
        assert!(
            msg.contains("placeholder count 2 != params 1"),
            "文案须含两个数字: {msg}"
        );
        assert!(
            msg.contains("hint") && msg.contains("jsonb_exists"),
            "文案须含正解 hint: {msg}"
        );
    }

    /// sql_trace 记原始 SQL：改写后执行，画像里仍是 `?` 而非 `$n`（评审 P6）。
    #[tokio::test(flavor = "current_thread")]
    async fn pg_e2e_trace_records_original_sql() {
        let Some(da) = pg_accessor().await else {
            return;
        };
        let extras = crate::bridge::Extras {
            sql_trace: crate::bridge::sql_trace::SqlTraceConfig {
                enabled: true,
                redact_params: true,
                slow_ms: 0.0,
                to_log: false,
            },
            ..Default::default()
        };
        let registry = SchemaRegistry::new().table("ph_e2e", &["id"], &["id", "name"]);
        let b = Bridge::with_dbs_and_loader(
            std::collections::HashMap::from([("default".to_string(), da as _)]),
            Arc::new(InMemoryKV::new()),
            registry,
            false,
            None,
            extras,
        );
        let cap = b
            .run_with(
                r#"(async () => {
                     await db.query("select ? as v", [7]);
                     const p = db.sqlProfile();
                     return json.ok({ sqls: p.events.map((e) => e.sql) });
                   })().catch((e) => json.fail(500, String(e)));"#,
                crate::bridge::RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        let sqls = v["data"]["sqls"].as_array().expect("profile events");
        assert!(
            sqls.iter().any(|s| s.as_str() == Some("select ? as v")),
            "trace 须记原始 `?` SQL: {v}"
        );
        assert!(
            sqls.iter()
                .all(|s| !s.as_str().unwrap_or("").contains("$1")),
            "trace 不得记改写后 SQL: {v}"
        );
    }

    // ---------- L3 动态数据源登记（client 轴，v0.1.91）行为测试 ----------

    /// 构造一个带 `db_client` 配置段的 Bridge（default 库存在 ⇒ `globalThis.db.client` 已挂载）。
    fn client_bridge_with(
        enabled: bool,
        allow: AllowList,
        max: usize,
        db_access: Option<Arc<DbAccessSection>>,
    ) -> Bridge {
        let def = Arc::new(InMemoryAccessor::new());
        Bridge::with_dbs_and_loader(
            HashMap::from([("default".to_string(), def as Arc<dyn DataAccessor>)]),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            None,
            Extras {
                db_client: Some(Arc::new(DbClientSection {
                    enabled,
                    max_entries: max,
                    max_per_owner: 8,
                    max_streams: 32,
                    allow,
                })),
                client_dbs: Some(Arc::new(ClientDbRegistry::new(max))),
                db_backend: Some(Arc::new(DbBackendRegistry::builtin())),
                config_dir: Some(Arc::new(PathBuf::from("."))),
                db_access,
                ..Default::default()
            },
        )
    }

    async fn run_json(b: &Bridge, js: &str) -> serde_json::Value {
        let cap = b.run(js).await.expect("run must not transport-error");
        let v: serde_json::Value =
            serde_json::from_slice(&cap.body).expect("capsule body must be json");
        // 运行响应统一包成 {code,msg,data}；这里取下 data 层，测试断言更直读。
        v.get("data").cloned().unwrap_or(serde_json::Value::Null)
    }

    fn unrestricted() -> AllowList {
        AllowList::default()
    }

    // 登记门关闭（默认 off）：register 必须结构化拒绝，不得静默放行。
    #[tokio::test(flavor = "current_thread")]
    async fn client_disabled_gate_blocks_register() {
        let b = client_bridge_with(false, unrestricted(), 8, None);
        let v = run_json(
            &b,
            r#"(async () => {
                try { await db.client.register("rep", "memory://"); return json.ok({ok:true}); }
                catch (e) { return json.ok({ok:false, err:String(e)}); }
            })()"#,
        )
        .await;
        assert_eq!(v["ok"], false, "{v}");
        assert!(
            v["err"].as_str().unwrap().contains("db_client_disabled"),
            "{v}"
        );
    }

    // 全生命周期：register → has → list(脱敏) → delete → has=false。
    #[tokio::test(flavor = "current_thread")]
    async fn client_register_has_list_delete_lifecycle() {
        let b = client_bridge_with(true, unrestricted(), 8, None);
        let v = run_json(
            &b,
            r#"(async () => {
                const r = await db.client.register("rep", "memory://");
                const has = db.client.has("rep");
                const list = await db.client.list();
                const names = list.map(x => x.alias);
                const del = await db.client.delete("rep");
                const has2 = db.client.has("rep");
                return json.ok({ dialect: r.dialect, dsn: r.dsn, has, names, del, has2 });
            })()"#,
        )
        .await;
        assert_eq!(v["dialect"], "sqlite", "{v}");
        assert_eq!(v["has"], true, "{v}");
        assert!(
            v["names"].as_array().unwrap().contains(&json!("rep")),
            "{v}"
        );
        assert_eq!(v["del"], true, "{v}");
        assert_eq!(v["has2"], false, "{v}");
        // DSN 脱敏：memory:// 无凭据，且绝不回显任何 host/库名外的秘密段。
        let dsn = v["dsn"].as_str().unwrap();
        assert!(dsn.starts_with("memory://"), "{dsn}");
    }

    // 重名登记必须拒绝（不得覆盖）。
    #[tokio::test(flavor = "current_thread")]
    async fn client_duplicate_register_rejected() {
        let b = client_bridge_with(true, unrestricted(), 8, None);
        let v = run_json(
            &b,
            r#"(async () => {
                await db.client.register("rep", "memory://");
                try { await db.client.register("rep", "memory://"); return json.ok({dup:false}); }
                catch (e) { return json.ok({dup:true, err:String(e)}); }
            })()"#,
        )
        .await;
        assert_eq!(v["dup"], true, "{v}");
        assert!(
            v["err"].as_str().unwrap().contains("already registered"),
            "{v}"
        );
    }

    // 超配额必须拒绝（连接池数封顶）。
    #[tokio::test(flavor = "current_thread")]
    async fn client_over_capacity_rejected() {
        let b = client_bridge_with(true, unrestricted(), 1, None);
        let v = run_json(
            &b,
            r#"(async () => {
                await db.client.register("a", "memory://");
                try { await db.client.register("b", "memory://"); return json.ok({full:false}); }
                catch (e) { return json.ok({full:true, err:String(e)}); }
            })()"#,
        )
        .await;
        assert_eq!(v["full"], true, "{v}");
        assert!(v["err"].as_str().unwrap().contains("registry full"), "{v}");
    }

    // 未知 scheme：结构化错误 + 凭据脱敏（不得回显 secret）。
    #[tokio::test(flavor = "current_thread")]
    async fn client_unknown_scheme_redacted_error() {
        let b = client_bridge_with(true, unrestricted(), 8, None);
        let v = run_json(
            &b,
            r#"(async () => {
                try { await db.client.register("x", "oracle://user:secret@db/host"); return json.ok({err:false}); }
                catch (e) { return json.ok({err:true, msg:String(e)}); }
            })()"#,
        )
        .await;
        assert_eq!(v["err"], true, "{v}");
        let msg = v["msg"].as_str().unwrap();
        assert!(msg.contains("unknown db scheme"), "{msg}");
        assert!(!msg.contains("secret"), "凭据不得回显: {msg}");
    }

    // 登记门授权维：不匹配 caller 必须拒绝（asSystem 不豁免）。
    #[tokio::test(flavor = "current_thread")]
    async fn client_register_requires_allow_and_as_system_no_exempt() {
        // 仅放行 source=task；http caller（默认）应被拒。
        let allow = AllowList {
            sources: Some(vec!["task".to_string()]),
            ..Default::default()
        };
        let b = client_bridge_with(true, allow, 8, None);
        let plain = run_json(
            &b,
            r#"(async () => {
                try { await db.client.register("rep", "memory://"); return json.ok({auth:true}); }
                catch (e) { return json.ok({auth:false, err:String(e)}); }
            })()"#,
        )
        .await;
        assert_eq!(plain["auth"], false, "{plain}");
        assert!(
            plain["err"]
                .as_str()
                .unwrap()
                .contains("db_client_not_authorized"),
            "{plain}"
        );
        // asSystem 显式开后仍被拒（identity 不读 ReqState.system）。
        let via_system = run_json(
            &b,
            r#"(async () => {
                try { db.asSystem(); await db.client.register("rep", "memory://"); return json.ok({auth:true}); }
                catch (e) { return json.ok({auth:false, err:String(e)}); }
            })()"#,
        )
        .await;
        assert_eq!(via_system["auth"], false, "{via_system}");
        assert!(
            via_system["err"]
                .as_str()
                .unwrap()
                .contains("db_client_not_authorized"),
            "{via_system}"
        );
    }

    // 未声明 db_access.<alias> 的动态库：fail-closed deny（登记成功但查询/内省被拒）。
    #[tokio::test(flavor = "current_thread")]
    async fn undeclared_client_alias_query_denied() {
        let b = client_bridge_with(true, unrestricted(), 8, None);
        let v = run_json(
            &b,
            r#"(async () => {
                await db.client.register("rep", "memory://");
                try { await DB("rep").query("select 1"); return json.ok({q:true}); }
                catch (e) { return json.ok({q:false, err:String(e)}); }
            })()"#,
        )
        .await;
        assert_eq!(v["q"], false, "{v}");
        let err = v["err"].as_str().unwrap();
        assert!(err.contains("db access"), "{err}");
        assert!(err.contains("db_not_granted"), "{err}");
    }

    // 显式声明 db_access.<alias> 的动态库：登记后可正常查询（复用 L1 访问控制）。
    #[tokio::test(flavor = "current_thread")]
    async fn declared_client_alias_query_allowed() {
        let db_access = Arc::new(DbAccessSection {
            defaults: DbAccess::default(),
            by_db: HashMap::from([(
                "rep".to_string(),
                DbAccessOverride {
                    allow: Some(AllowList::default()),
                    ..Default::default()
                },
            )]),
        });
        let b = client_bridge_with(true, unrestricted(), 8, Some(db_access));
        let v = run_json(
            &b,
            r#"(async () => {
                await db.client.register("rep", "memory://");
                const rows = await DB("rep").query("select 1 as v");
                return json.ok({q:true, rows});
            })()"#,
        )
        .await;
        assert_eq!(v["q"], true, "{v}");
    }

    // reRegister：已存在则替换；不存在则拒绝。
    #[tokio::test(flavor = "current_thread")]
    async fn client_re_register_swaps_and_rejects_missing() {
        let b = client_bridge_with(true, unrestricted(), 8, None);
        let v = run_json(
            &b,
            r#"(async () => {
                await db.client.register("rep", "memory://");
                await db.client.reRegister("rep", "memory://");
                try { await db.client.reRegister("nope", "memory://"); return json.ok({bad:false, has:db.client.has("rep")}); }
                catch (e) { return json.ok({bad:true, err:String(e), has:db.client.has("rep")}); }
            })()"#,
        )
        .await;
        assert_eq!(v["bad"], true, "{v}");
        assert!(v["err"].as_str().unwrap().contains("not registered"), "{v}");
        assert_eq!(v["has"], true, "{v}");
    }

    // 查询/内省经 lookup 自动累加命中计数（声明别名后才可查，故需 declared 配置）。
    #[tokio::test(flavor = "current_thread")]
    async fn client_queries_counter_increments() {
        let db_access = Arc::new(DbAccessSection {
            defaults: DbAccess::default(),
            by_db: HashMap::from([(
                "rep".to_string(),
                DbAccessOverride {
                    allow: Some(AllowList::default()),
                    ..Default::default()
                },
            )]),
        });
        let b = client_bridge_with(true, unrestricted(), 8, Some(db_access));
        let v = run_json(
            &b,
            r#"(async () => {
                await db.client.register("rep", "memory://");
                await DB("rep").query("select 1 as v");
                await DB("rep").query("select 1 as v");
                const list = await db.client.list();
                const e = list.find(x => x.alias === "rep");
                return json.ok({queries: e.queries});
            })()"#,
        )
        .await;
        assert!(
            v["queries"].as_u64().unwrap() >= 2,
            "lookup 命中计数应累加: {v}"
        );
    }
}
