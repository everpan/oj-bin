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
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64};
use std::sync::{Arc, OnceLock, RwLock};

use async_trait::async_trait;
use deno_core::{OpState, op2};
use deno_error::JsErrorBox;
use futures::stream::Stream;
use serde_json::Value;
use tokio::sync::{Notify, mpsc};

use super::{BridgeResult, StableState};
use crate::bridge::db_error::{BridgeError, DbErrorCategory, unsupported};
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

    /// 带字节计量器的查询（REQ-05 `max_result_bytes`）。
    ///
    /// 默认实现 = 全量拉取后逐行计量（插件轴/内存件兼容口径——行已物化，无 OOM
    /// 防线，可观测照计；truncate 档交付全量行 + 信令照打，成文限制）。**SqlxAccessor
    /// 覆盖为边拉边计、触顶早停**——把结果集推过上限的那一行计入 `meter.counted`
    /// 但不交付，调用侧按 `meter.exceeded × on_exceed` 分档（fail → 结构化
    /// `OJ_DB_CAPACITY`；truncate → 已取行 + `X-OJ-Result-Truncated` 信令）。
    async fn query_with_params_metered(
        &self,
        sql: &str,
        params: &[Value],
        meter: &mut super::db_quota::ByteMeter,
    ) -> BridgeResult<Vec<Row>> {
        let rows = self.query_with_params(sql, params).await?;
        // 回落口径：全量计入（counted = 真实结果集大小），exceeded 首行越限即置位。
        for r in &rows {
            meter.feed(r);
        }
        Ok(rows)
    }
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

    /// 语句 deadline 竞速到点后，被弃的在飞语句是否**服务端真死**（REQ-05
    /// `data.cancelled` 诚实口径）：SqlxAccessor（sqlite 内置）= true——停止客户端
    /// 拉取、嵌入式无服务端残留；插件轴 = false——宿主弃 FfiFuture 只 free state，
    /// 语句可能仍在服务端烧资源（真取消 = ABI 13 车，见执行计划批次 2 spike 结论）。
    fn server_cancel_on_abandon(&self) -> bool {
        false
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
pub(crate) fn normalize_and_trace<'a>(
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
    /// REQ-05：字节触顶（truncate 档）置位——`db.stream` 的显式截断信令（`info().truncated`）。
    pub truncated: AtomicBool,
    /// REQ-05：pump 累计的结果集字节（行 JSON 编码口径；fail 档触顶行也计入）。
    pub result_bytes: AtomicU64,
    /// REQ-07 欠账补齐（v0.1.112）：流式降级清单（`[{col,type}]`，按值去重）——
    /// `op_db_stream_next` 交付行时扫 `cellenv::degraded_of` 累积，`info().degraded`
    /// 外发（与 `exportStatus.degraded` 同形态；干净流恒 `[]`）。
    pub degraded: std::sync::Mutex<Vec<serde_json::Value>>,
    /// 反向句柄：本流在 `StableState.exec_sessions` 的执行 id（open 时登记，close 摘除）。
    pub exec_id: OnceLock<String>,
}

/// pump → 消费端的行缓冲上限（有界背压；abort 后缓冲行继续交付 = "流提前结束"）。
pub(crate) const DB_STREAM_CHANNEL_CAP: usize = 64;

/// REQ-11 G3：非流式执行的相位事件（`onProgress` 数据源；JS 经 poll 批量取走）。
pub(crate) struct CallEvent {
    pub phase: &'static str,
    pub elapsed_ms: u64,
    pub rows: Option<u64>,
}

/// REQ-11：非流式池执行的结果载体（begin/poll/take 模型的 take 源）。
pub(crate) enum PoolOutcome {
    Rows(Vec<Row>),
    Affected(i64),
    Err { category: DbErrorCategory, msg: String },
}

/// REQ-11 G1/G2：非流式池执行（db.query/exec）的共享状态。与 `DbStreamShared` 同表异型
/// 挂在 `ExecRegistry`。cancel = 置标志 + notify；执行侧 `select!` 观测后 drop 在飞
/// fetch 并 `close_on_drop()` 真关 TCP（服务端语句即死，连接报废不回池——修正案 #2）。
pub(crate) struct PoolExecShared {
    pub owner: super::handle_core::Owner,
    /// 所属库别名（cancel 返回与限额日志用）。
    pub alias: String,
    pub cancel_requested: AtomicBool,
    pub notify: Arc<Notify>,
    /// serve 挂断联动（G2）：本请求级 token；None = WS/任务/测试路径不联动。
    pub cancel_token: Option<u64>,
    /// 执行 settle（含出错）即置位——登记清扫与 take 的判据。
    pub done: AtomicBool,
    /// 相位事件队列（`sent`/`fetching`/`done`；done 事件恒在队尾）。
    pub events: std::sync::Mutex<Vec<CallEvent>>,
    /// 完成结果；take 时搬走。
    pub outcome: std::sync::Mutex<Option<PoolOutcome>>,
}

/// stream_id 全局自增（每请求注册表独立，跨请求重复无妨；u32 足以覆盖请求生命周期内的并发流）。
pub(crate) static NEXT_STREAM_ID: AtomicU32 = AtomicU32::new(1);

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

pub(crate) fn elapsed_ms(start: std::time::Instant) -> f64 {
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

#[cfg(test)]
#[path = "db_tests.rs"]
mod tests;
