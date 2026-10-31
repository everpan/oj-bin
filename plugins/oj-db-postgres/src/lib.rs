//! oj-db-postgres：db 轴 postgres cdylib 插件（spec §3 试点成型；plan Task 4.1）。
//! 与 oj-db-mysql 同构：迁移 core `SqlxAccessor` 的 sqlx 逻辑（`sqlx::Any` + 单方言
//! postgres feature），自建 tokio runtime；vtable `connect` 收 DSN，handle 查表。
//! 事务句柄化（tx_id → Tx）。决策记录见 oj-db-mysql（复制 vs 共享 crate：接受复制）。
//!
//! cfg 契约：init cfg = `{}`；DSN 在 connect 按值传入。
//! 句柄约定：connect 分配 handle；tx 分配 tx_id（每 client AtomicU64）。

use oj_plugin_ffi::{
    ABI_VERSION, DataAccessorVtable, FfiFuture, HostContext, PluginDescriptor, RArc, RResult,
    RString, RVec,
};
use sqlx::Connection as _;
use sqlx::any::{Any, AnyArguments, AnyRow};
use sqlx::error::ErrorKind;
use sqlx::pool::{Pool, PoolOptions};
use sqlx::postgres::{PgArguments, PgColumn, PgConnection, PgRow, Postgres};
use sqlx::query::Query;
use sqlx::types::BigDecimal;
use sqlx::types::Json;
use sqlx::types::Uuid;
use sqlx::types::chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use sqlx::{Column, Row, TypeInfo};
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dialect {
    Sqlite,
    MySql,
    Postgres,
}

/// 流式游标的行流类型（typed 专用连接 / Any 回退路径共用同一形状）。
type DbStream = Pin<Box<dyn futures::Stream<Item = Result<serde_json::Value, String>> + Send>>;

/// sqlx 错误 → 结构化信封（T11）。vtable `Err(RString)` 载荷恒为信封 JSON：
/// `{category, retryable, session_disposition, message, data}`。宿主 ffi.rs `parse_raw`
/// 单点解析。`vendor_code`：mysql 数值错误码（如 1062）/ pg SQLSTATE 串，原样透传，禁止猜。
fn sqlx_category(e: &sqlx::Error) -> (&'static str, bool, &'static str) {
    match e {
        sqlx::Error::Io(_) | sqlx::Error::PoolClosed | sqlx::Error::Tls(_) => {
            ("connection", true, "close")
        }
        // 池排队超时是**超时**不是连接故障（连接本身可用）——分错类会让告警打到 infra。
        sqlx::Error::PoolTimedOut => ("timeout", true, "keep"),
        sqlx::Error::Database(db) => match db.kind() {
            ErrorKind::UniqueViolation
            | ErrorKind::ForeignKeyViolation
            | ErrorKind::NotNullViolation
            | ErrorKind::CheckViolation
            | ErrorKind::ExclusionViolation => ("constraint", false, "keep"),
            // sqlx 的 kind 只覆盖常见几类；其余按 SQLSTATE 前两位分类（标准五字符码，
            // PG 全量实现）——否则 syntax/timeout/capacity/cancelled 永远没有生产者。
            _ => sqlstate_class(db.code().as_deref()).unwrap_or(("internal", false, "keep")),
        },
        _ => ("internal", false, "keep"),
    }
}

/// SQLSTATE 类别 → (category, retryable, session_disposition)。
/// 只认标准类（PG 与 MySQL 共用）；未知类返回 None → 调用方落 internal（禁止猜）。
pub(crate) fn sqlstate_class(code: Option<&str>) -> Option<(&'static str, bool, &'static str)> {
    let class = code?.get(..2)?;
    Some(match class {
        "08" => ("connection", true, "close"),
        "23" => ("constraint", false, "keep"),
        "40" => ("timeout", true, "keep"),
        "42" => ("syntax", false, "keep"),
        "53" => ("capacity", true, "keep"),
        "57" => ("cancelled", false, "keep"),
        _ => return None,
    })
}

fn sqlx_vendor_code(e: &sqlx::Error) -> serde_json::Value {
    // PG 的厂商码即 SQLSTATE（5 位定长串，如除法溢出 `22012`、唯一违例 `23505`）——
    // 原样透传为字符串，绝不解析成数字（否则 `23505` 等会被误当整型，下游字符串匹配失效）。
    let code = match e {
        sqlx::Error::Database(db) => db.code(),
        _ => None,
    };
    match code {
        Some(c) => serde_json::Value::from(c.as_ref()),
        None => serde_json::Value::Null,
    }
}

/// 构造 db 轴结构化错误信封（op 阶段名保留在 message 中，宿主 parse_raw 不丢弃）。
fn map_sqlx_err(op: &str, e: &sqlx::Error) -> String {
    let (category, retryable, disposition) = sqlx_category(e);
    let mut data = serde_json::Map::new();
    let vendor = sqlx_vendor_code(e);
    if !vendor.is_null() {
        data.insert("vendor_code".to_string(), vendor);
    }
    serde_json::json!({
        "category": category,
        "retryable": retryable,
        "session_disposition": disposition,
        "message": format!("{op}: {e}"),
        "data": serde_json::Value::Object(data),
    })
    .to_string()
}

/// 给 SQL **前置**参数形态签名（v0.1.24，债务①「同文本换参数类型 → 协议级错误」）。
///
/// **为什么在插件层而不是宿主**：这是 sqlx 驱动的缓存行为（key 只有 SQL 文本，命中后复用旧
/// `param OIDs`），插件才是 sqlx 的拥有者；宿主不该知道驱动细节。且插件层是本插件的**单一
/// 咽喉**（4 个执行点都在本文件），测试也能直接打 vtable 验证。
///
/// **形态字母表**：`t`=text/null（`None::<String>` 的 type_info 就是 TEXT，同理合键安全）、
/// `i`=i64、`f`=f64、`b`=bool、`m`=`$oj$i64` 标记、`u`=`$oj$u64` 标记。
///
/// **必须前置**：`…; /*sig*/` 后置在 PG 扩展协议下会被判「多语句」（`cannot insert multiple
/// commands into a prepared statement`），SQL 以 `--` 行注释结尾时签名还会被吞掉。前置块注释
/// PG 词法层接受（已用 `PREPARE … AS /*oj:i*/ …` 实测）。`toSQL()` 不进本函数 → 用户可见
/// 的 SQL 保持干净；DBA 视角会看到前缀（见 `docs/numeric-limits.md` §4.5）。
///
/// **MySQL 不需要**：它每次 execute 都重发参数类型（`new_params_bound_flag=1`），病不在同一处；
/// SQLite 无声明参数类型。二者都不做。
fn shape_tag(sql: &str, params: &[serde_json::Value]) -> String {
    if params.is_empty() {
        return sql.to_string();
    }
    let mut sig = String::with_capacity(params.len());
    for p in params {
        sig.push(match p {
            serde_json::Value::Null | serde_json::Value::String(_) => 't',
            serde_json::Value::Bool(_) => 'b',
            serde_json::Value::Number(n) => {
                if n.as_i64().is_some() {
                    'i'
                } else {
                    'f'
                }
            }
            other => {
                if oj_plugin_ffi::jsint::marker_i64(other).is_some() {
                    'm'
                } else if oj_plugin_ffi::jsint::marker_u64(other).is_some() {
                    // 当前**不可达**：4 个执行点都先跑 `reject_u64_markers`（PG 的 bigint 就是 i64，
                    // 绑不了 u64）。保留分支是为了「万一 reject 被挪到后面」时缓存键仍然区分得开；
                    // 若真要放开 u64，必须同步给 PG 侧 `bind_value` 加对应绑定（现在没有）。
                    'u'
                } else {
                    // 其余对象/数组在 bind_value 里走 `to_string()` 落成文本。
                    't'
                }
            }
        });
    }
    format!("/*oj:{sig}*/{sql}")
}

fn dialect_of(dsn: &str) -> Dialect {
    if dsn.starts_with("mysql://") {
        Dialect::MySql
    } else if dsn.starts_with("postgres://") || dsn.starts_with("postgresql://") {
        Dialect::Postgres
    } else {
        Dialect::Sqlite
    }
}

/// 给 PG DSN 补 sqlx 的 `statement-cache-capacity`（仅在未显式设置时）。
///
/// 背景（v0.1.24 债务①）：宿主在 PG 方言下给 SQL 前置参数形态签名（`/*oj:<形态>*/`），
/// 于是「一条 SQL × 若干参数形态」各自成为缓存条目。默认容量 100 在这种乘性增长下会频繁
/// LRU 淘汰，而**淘汰要发 `Close` + `Sync` 并等回包**（多一次往返）。512 留出余量，又不至于
/// 让服务端 prepared statement 无界堆积；用户显式写了该参数则以用户为准。
///
/// 只对 `postgres://` / `postgresql://` 生效：本插件的离线测试走 sqlite DSN，sqlite 的
/// DSN 解析器会拒绝未知 query 参数（`unknown query parameter …`）。
fn with_stmt_cache_capacity(dsn: &str, cap: u32) -> String {
    if !(dsn.starts_with("postgres://") || dsn.starts_with("postgresql://")) {
        return dsn.to_string();
    }
    if dsn.contains("statement-cache-capacity") {
        return dsn.to_string();
    }
    let sep = if dsn.contains('?') { '&' } else { '?' };
    format!("{dsn}{sep}statement-cache-capacity={cap}")
}

fn dialect_str(d: Dialect) -> &'static str {
    match d {
        Dialect::Sqlite => "sqlite",
        Dialect::MySql => "mysql",
        Dialect::Postgres => "postgres",
    }
}

struct DbPluginState {
    rt: tokio::runtime::Runtime,
    clients: Mutex<HashMap<u64, Arc<Client>>>,
    next_handle: AtomicU64,
}

struct Client {
    pool: Pooled,
    /// 原始 DSN（PR-2 真取消：流式游标按方言单开专用 typed 连接用）。
    dsn: String,
    dialect: Dialect,
    next_tx: AtomicU64,
    txs: Mutex<HashMap<u64, Arc<Tx>>>,
    /// ABI 10 流式游标（stream_id → 行流 + 协作式取消标志）。
    streams: Mutex<HashMap<u64, Arc<StreamHandle>>>,
    next_stream: AtomicU64,
    /// 流式专用连接闸（v0.1.84，`STREAM_MAX_CONNS`）：typed 游标先占配额再拨号，
    /// 条目移除（done/err/cancel/close）随 handle drop 释放。
    stream_gate: Arc<tokio::sync::Semaphore>,
}

/// 池的二选一（v0.1.3 / L6）：**生产 DSN（`postgres://`）走 typed**，其余（离线测试的 sqlite）
/// 走 Any。
///
/// 为什么必须 typed（L6 评审阻塞项）：`sqlx::Any` 只实现 `bool/i16/i32/i64/f32/f64/String/
/// Vec<u8>` 的解码（`sqlx-core/src/any/types/`），`NUMERIC`/`JSON`/`UUID`/数组在 Any 上
/// **根本取不出来**——只在 `column_json_pg` 里补类型毫无意义，`query`/`exec`/`tx_*` 必须
/// 一起走 typed，否则同一张表 `db.query` 报错、`db.stream` 成功。
///
/// `Pooled::Any` 分支是**仅测试用**的（不是受支持的运行时路径）：它只在「非 postgres DSN」
/// 时出现，而生产装配按 scheme 路由——`postgres://` 必进本插件且必走 typed。故它只覆盖
/// 「编排/绑定通路」；PG 行形状的回归靠 env-gated 真库用例（见 `CHANGELOG` 的 CI 挂账）。
enum Pooled {
    Pg(Pool<Postgres>),
    Any(Pool<Any>),
}

/// 事务内层（与池同型分流）。
enum TxInner {
    Pg(tokio::sync::Mutex<Option<sqlx::Transaction<'static, Postgres>>>),
    Any(tokio::sync::Mutex<Option<sqlx::Transaction<'static, Any>>>),
}

/// 流式游标句柄：行流（async-stream 自持 SQL/参数/连接）独立成 tokio Mutex（批量
/// 推进期间独占），取消标志在外层无锁置位。方言支持矩阵（v0.1.42）：
/// - **postgres DSN（typed）**：游标单开一条 `PgConnection`，`stream_cancel` = **移除
///   条目并断开连接**（TCP 断开即服务端终止查询——真取消）；
/// - **其它（Any 回退，如 sqlite）**：池路径 + 协作式取消（置标志，下一批返回
///   `{"error":"cancelled"}`；`stream_close` 移除条目，行流 drop = 连接回池）。
struct StreamHandle {
    rows: tokio::sync::Mutex<DbStream>,
    cancelled: AtomicBool,
    /// true = typed 专用连接（真取消：cancel = pg_cancel_backend + 断连）；false = Any 回退。
    typed: bool,
    /// typed 连接的后端 pid（pg_cancel_backend 定位用；连接建立时查询）。
    backend_pid: Option<i32>,
    /// 错误延迟上报：错误发生时本批已有行 → 行先交付，错误下一拍给出。
    pending_error: std::sync::Mutex<Option<String>>,
    /// 流式专用连接闸配额（v0.1.84）：typed 游标持有，条目移除（done/err/cancel/close）
    /// 即随 handle drop 释放；Any 回退路径无专用连接，为 None。（仅 Drop 有意义，故 `_` 前缀。）
    _gate: Option<tokio::sync::OwnedSemaphorePermit>,
}

/// 每次 stream_next 的批量行数上限（spec §1.4：砍逐行 FFI 往返）。
const STREAM_BATCH: usize = 100;

/// 流式游标专用连接并发上限（v0.1.84）：typed 路径每游标单开一条池外连接，原无上限——
/// 并发大结果集打满服务端 max_connections 后连池路径也拿不到连接（全库不可用）。
/// 上限 8（sqlx 池缺省 10，留余量）；超限 fail-loud。
const STREAM_MAX_CONNS: usize = 8;

struct Tx {
    tx: TxInner,
}

static PLUGIN: OnceLock<DbPluginState> = OnceLock::new();

fn state() -> &'static DbPluginState {
    PLUGIN.get().expect("oj-db-postgres: init not called")
}

// ---- FfiFuture 桥（统一走 oj-plugin-ffi 的 catch_unwind 安全工厂：spawn_ffi_future / catch_future）----

// ---- sqlx 逻辑（迁移自 core accessor_sqlx.rs，与 oj-db-mysql 逐字同构）----

fn bind_value<'q>(
    q: Query<'q, Any, AnyArguments>,
    v: &serde_json::Value,
) -> Query<'q, Any, AnyArguments> {
    // 大整数标记（v0.1.22，`toBigInt()` 的返回值）：绑 i64。必须在对象分支之前——
    // 否则会被 `other => to_string()` 串化成文本，而 PG 拒绝 text → bigint。
    if let Some(i) = oj_plugin_ffi::jsint::marker_i64(v) {
        return q.bind(i);
    }
    match v {
        serde_json::Value::Null => q.bind(None::<String>),
        serde_json::Value::Bool(b) => q.bind(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                q.bind(i)
            } else if let Some(f) = n.as_f64() {
                q.bind(f)
            } else {
                q.bind(None::<String>)
            }
        }
        serde_json::Value::String(s) => q.bind(s.clone()),
        other => q.bind(other.to_string()),
    }
}

/// 解不出类型/值时的响亮报错（含列名/类型线索）——绝不静默 `null`（静默 null =
/// 返回错值）。对齐 oj-db-mysql `row_to_json_mysql` 纪律（见 `docs/numeric-limits.md`、
/// 读侧口径见 `docs/devkit/db-guide.md` §10.6）。
fn undecodable_column(name: &str, type_name: &str) -> String {
    format!(
        "db(postgres): column '{name}' has type '{type_name}' which this plugin does not \
         decode yet — cast it in SQL (e.g. `cast({name} as text) as {name}`); \
         see docs/devkit/db-guide.md §10.6"
    )
}

/// 错误文案里的标识符消毒（v0.1.3 / L6 安全评审）：过滤控制字符（防日志注入）并截断。
fn safe_ident(s: &str) -> String {
    const MAX: usize = 64;
    let mut out = String::new();
    let mut n = 0usize;
    let mut truncated = false;
    for c in s.chars() {
        if c.is_control() {
            continue;
        }
        if n == MAX {
            truncated = true;
            break;
        }
        out.push(c);
        n += 1;
    }
    if truncated {
        out.push('…');
    }
    out
}

fn row_to_json(row: &AnyRow) -> Result<serde_json::Value, String> {
    let mut obj = serde_json::Map::new();
    for col in row.columns() {
        let name = col.name().to_string();
        let ordinal = col.ordinal();
        // REQ-07b：Any 路径同款 BYTEA 短路（type_info 在 Any 行可用）。
        let val = if col.type_info().name() == "BYTEA" {
            row.try_get::<Option<Vec<u8>>, _>(ordinal)
                .ok()
                .map(|v| match v {
                    Some(b) => serde_json::Value::String(oj_plugin_ffi::cellenv::encode(
                        col.type_info().name(),
                        &b,
                    )),
                    None => serde_json::Value::Null,
                })
        } else {
            column_json(row, ordinal)
        }
        .ok_or_else(|| undecodable_column(&name, col.type_info().name()))?;
        obj.insert(name, val);
    }
    Ok(serde_json::Value::Object(obj))
}

fn column_json(row: &AnyRow, ordinal: usize) -> Option<serde_json::Value> {
    if let Ok(v) = row.try_get::<Option<bool>, _>(ordinal) {
        return Some(serde_json::Value::from(v));
    }
    if let Ok(v) = row.try_get::<Option<i64>, _>(ordinal) {
        return Some(match v {
            Some(i) => serde_json::Value::from(i),
            None => serde_json::Value::Null,
        });
    }
    if let Ok(v) = row.try_get::<Option<f64>, _>(ordinal) {
        return Some(match v {
            Some(f) => serde_json::Number::from_f64(f)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null),
            None => serde_json::Value::Null,
        });
    }
    if let Ok(v) = row.try_get::<Option<String>, _>(ordinal) {
        return Some(match v {
            Some(s) => serde_json::Value::String(s),
            None => serde_json::Value::Null,
        });
    }
    if let Ok(v) = row.try_get::<Option<Vec<u8>>, _>(ordinal) {
        return Some(match v {
            Some(b) => serde_json::Value::String(String::from_utf8_lossy(&b).into_owned()),
            None => serde_json::Value::Null,
        });
    }
    None
}

/// typed PG 版绑定（与 Any 版 `bind_value` 同链；typed 连接用 `PgArguments`）。
fn bind_value_pg<'q>(
    q: Query<'q, Postgres, PgArguments>,
    v: &serde_json::Value,
) -> Query<'q, Postgres, PgArguments> {
    if let Some(i) = oj_plugin_ffi::jsint::marker_i64(v) {
        return q.bind(i);
    }
    match v {
        serde_json::Value::Null => q.bind(None::<String>),
        serde_json::Value::Bool(b) => q.bind(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                q.bind(i)
            } else if let Some(f) = n.as_f64() {
                q.bind(f)
            } else {
                q.bind(None::<String>)
            }
        }
        serde_json::Value::String(s) => q.bind(s.clone()),
        other => q.bind(other.to_string()),
    }
}

/// typed PG 版行转换（镜像 `row_to_json` 的 try 链；未解码类型降级为 cellenv 前缀
/// 标量，拿原始字节失败才响亮报错）。
fn row_to_json_pg(row: &PgRow) -> Result<serde_json::Value, String> {
    let mut obj = serde_json::Map::new();
    for col in row.columns() {
        let name = safe_ident(col.name());
        let val = match column_json_pg(row, col) {
            Some(v) => v,
            None => {
                // REQ-07a：未解码类型降级为 `$<type>:<base64>`（type 名由 encode 小写化）。
                // 语句级权威降级清单由宿主扫描外发（X-OJ-Degraded）；原始字节也拿不到
                // 时才维持 `undecodable_column` 整行错误——绝不产空信封（红线）。
                let raw = row
                    .try_get_raw(col.ordinal())
                    .ok()
                    .and_then(|v| v.as_bytes().ok());
                match raw {
                    Some(b) => serde_json::Value::String(oj_plugin_ffi::cellenv::encode(
                        col.type_info().name(),
                        b,
                    )),
                    None => return Err(undecodable_column(&name, col.type_info().name())),
                }
            }
        };
        obj.insert(name, val);
    }
    Ok(serde_json::Value::Object(obj))
}

/// 逐列按**固定顺序**探测（typed 路径）：`bool → i16 → i32 → i64 → f64 → String →
/// Vec<u8> → 受限类型分派`。
///
/// 顺序要点：PG 的 `Type<Postgres>` 是**严格等值**匹配（`i64::compatible` 只认 INT8），
/// 所以 INT4/INT2 用 i64 解会失败——必须各自单列，缺一列就落到 `undecodable_column`
/// 硬报错。Any 回退路径按类型名匹配，才没有这个陷阱。
///
/// 探测链覆盖不到的类型（`NUMERIC`/日期时间/`JSON`/`UUID`/数组/`FLOAT4`）交给
/// [`column_json_pg_restricted`]（v0.1.3 / L6 / U-6 补齐）；仍未覆盖的（`TIMETZ`、
/// 范围类型、几何、复合类型等）返回 `None` → 调用方响亮报错。
fn column_json_pg(row: &PgRow, col: &PgColumn) -> Option<serde_json::Value> {
    let ordinal = col.ordinal();
    // REQ-07b：BYTEA 按类型名短路——恒定 cellenv 信封（含合法 UTF-8 内容，breaking：
    // 原 lossy 裸串）。探针链的 Vec<u8> lossy 臂随之对 BYTEA 不可达。
    if col.type_info().name() == "BYTEA" {
        return row
            .try_get::<Option<Vec<u8>>, _>(ordinal)
            .ok()
            .map(|v| match v {
                Some(b) => serde_json::Value::String(oj_plugin_ffi::cellenv::encode(
                    col.type_info().name(),
                    &b,
                )),
                None => serde_json::Value::Null,
            });
    }
    if let Ok(v) = row.try_get::<Option<bool>, _>(ordinal) {
        return Some(serde_json::Value::from(v));
    }
    if let Ok(v) = row.try_get::<Option<i16>, _>(ordinal) {
        return Some(match v {
            Some(i) => serde_json::Value::from(i),
            None => serde_json::Value::Null,
        });
    }
    if let Ok(v) = row.try_get::<Option<i32>, _>(ordinal) {
        return Some(match v {
            Some(i) => serde_json::Value::from(i),
            None => serde_json::Value::Null,
        });
    }
    if let Ok(v) = row.try_get::<Option<i64>, _>(ordinal) {
        return Some(match v {
            Some(i) => serde_json::Value::from(i),
            None => serde_json::Value::Null,
        });
    }
    if let Ok(v) = row.try_get::<Option<f64>, _>(ordinal) {
        return Some(match v {
            Some(f) => serde_json::Number::from_f64(f)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null),
            None => serde_json::Value::Null,
        });
    }
    if let Ok(v) = row.try_get::<Option<String>, _>(ordinal) {
        return Some(match v {
            Some(s) => serde_json::Value::String(s),
            None => serde_json::Value::Null,
        });
    }
    if let Ok(v) = row.try_get::<Option<Vec<u8>>, _>(ordinal) {
        return Some(match v {
            Some(b) => serde_json::Value::String(String::from_utf8_lossy(&b).into_owned()),
            None => serde_json::Value::Null,
        });
    }
    column_json_pg_restricted(row, ordinal, col.type_info().name())
}

/// 受限类型分派（v0.1.3 / L6，PRD U-6）：`None` = 未覆盖 → 调用方响亮报错。
///
/// 输出口径见 `docs/devkit/db-guide.md` §10.6：`NUMERIC` → 十进制字符串（不经 f64，
/// 保精度）/ 日期时间 → ISO 8601（`TIMESTAMP` 无时区、`TIMESTAMPTZ` 带 `Z`）/ `JSON`、
/// `JSONB` → JSON 文本 / `UUID` → 规范文本 / 数组 → **JSON 数组**（元素同口径）。
/// **只读**：这些类型没有对称的写入编码（`bind_value_pg` 只绑 str/num/bool/null），
/// 写回须显式 `::type` 转换。
fn column_json_pg_restricted(row: &PgRow, ordinal: usize, ty: &str) -> Option<serde_json::Value> {
    fn s(v: String) -> serde_json::Value {
        serde_json::Value::String(v)
    }
    match ty {
        "NUMERIC" => row
            .try_get::<Option<BigDecimal>, _>(ordinal)
            .ok()
            .map(|v| v.map_or(serde_json::Value::Null, |d| s(decimal_text(&d)))),
        "DATE" => row.try_get::<Option<NaiveDate>, _>(ordinal).ok().map(|v| {
            v.map_or(serde_json::Value::Null, |d| {
                s(d.format("%Y-%m-%d").to_string())
            })
        }),
        "TIME" => row.try_get::<Option<NaiveTime>, _>(ordinal).ok().map(|v| {
            v.map_or(serde_json::Value::Null, |t| {
                s(t.format("%H:%M:%S%.f").to_string())
            })
        }),
        "TIMESTAMP" => row
            .try_get::<Option<NaiveDateTime>, _>(ordinal)
            .ok()
            .map(|v| {
                v.map_or(serde_json::Value::Null, |t| {
                    s(t.format("%Y-%m-%dT%H:%M:%S%.f").to_string())
                })
            }),
        "TIMESTAMPTZ" => row
            .try_get::<Option<DateTime<Utc>>, _>(ordinal)
            .ok()
            .map(|v| {
                // `sqlx::types::chrono` 只再导出类型、不含 `SecondsFormat`，
                // 故用显式格式串而非 `to_rfc3339_opts`。
                v.map_or(serde_json::Value::Null, |t| {
                    s(t.format("%Y-%m-%dT%H:%M:%S%.fZ").to_string())
                })
            }),
        // PG 没有 `Type<Postgres> for serde_json::Value`，只能走 `Json<T>`；
        // `Json` 的 `compatible` 同时认 `json` 与 `jsonb`。
        "JSON" | "JSONB" => row
            .try_get::<Option<Json<serde_json::Value>>, _>(ordinal)
            .ok()
            .map(|v| v.map_or(serde_json::Value::Null, |j| s(j.0.to_string()))),
        "UUID" => row
            .try_get::<Option<Uuid>, _>(ordinal)
            .ok()
            .map(|v| v.map_or(serde_json::Value::Null, |u| s(u.to_string()))),
        // `real`：探测链里 `f64::compatible` 只认 FLOAT8，FLOAT4 必须单列。
        "FLOAT4" => row
            .try_get::<Option<f32>, _>(ordinal)
            .ok()
            .map(|v| match v {
                Some(f) => serde_json::Number::from_f64(f as f64)
                    .map(serde_json::Value::Number)
                    .unwrap_or(serde_json::Value::Null),
                None => serde_json::Value::Null,
            }),
        // ---- 数组（类型名形如 `INT4[]`；`name()` 与 `display_name()` 同口径）----
        "BOOL[]" => array_json::<bool, _>(row, ordinal, |v| serde_json::Value::from(*v)),
        "INT2[]" => array_json::<i16, _>(row, ordinal, |v| serde_json::Value::from(*v)),
        "INT4[]" => array_json::<i32, _>(row, ordinal, |v| serde_json::Value::from(*v)),
        "INT8[]" => array_json::<i64, _>(row, ordinal, |v| serde_json::Value::from(*v)),
        "FLOAT4[]" => array_json::<f32, _>(row, ordinal, |v| {
            serde_json::Number::from_f64(*v as f64)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null)
        }),
        "FLOAT8[]" => array_json::<f64, _>(row, ordinal, |v| {
            serde_json::Number::from_f64(*v)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null)
        }),
        // `String` 的数组兼容性覆盖 text/varchar/bpchar/name/char 五族
        // （`CHAR[]` 是 bpchar，`"CHAR"[]` 是 char，两者名字不同但同一实现）。
        "TEXT[]" | "VARCHAR[]" | "CHAR[]" | "\"CHAR\"[]" | "NAME[]" => {
            array_json::<String, _>(row, ordinal, |v| serde_json::Value::String(v.clone()))
        }
        "NUMERIC[]" => array_json::<BigDecimal, _>(row, ordinal, |v| s(decimal_text(v))),
        "DATE[]" => {
            array_json::<NaiveDate, _>(row, ordinal, |v| s(v.format("%Y-%m-%d").to_string()))
        }
        "TIME[]" => {
            array_json::<NaiveTime, _>(row, ordinal, |v| s(v.format("%H:%M:%S%.f").to_string()))
        }
        "TIMESTAMP[]" => array_json::<NaiveDateTime, _>(row, ordinal, |v| {
            s(v.format("%Y-%m-%dT%H:%M:%S%.f").to_string())
        }),
        "TIMESTAMPTZ[]" => array_json::<DateTime<Utc>, _>(row, ordinal, |v| {
            s(v.format("%Y-%m-%dT%H:%M:%S%.fZ").to_string())
        }),
        "UUID[]" => array_json::<Uuid, _>(row, ordinal, |v| s(v.to_string())),
        // `Json<T>` 的数组兼容性同时认 `JSON[]` 与 `JSONB[]`。
        "JSON[]" | "JSONB[]" => {
            array_json::<Json<serde_json::Value>, _>(row, ordinal, |v| s(v.0.to_string()))
        }
        _ => None,
    }
}

/// `NUMERIC` 的文本化口径（v0.1.3 / L6）：**去尾零的规范十进制字符串**。
///
/// 为什么必须规范化：sqlx 的 `TryFrom<&PgNumeric> for BigDecimal` **丢弃服务端 dscale**，
/// 把标度硬算成「4 的倍数」（`sqlx-postgres/src/types/bigdecimal.rs`：
/// `scale = (digits.len() - weight - 1) * 4`）——于是 `1.5` 会读成 `1.5000`、`0.1` 读成
/// `0.1000`。值本身是精确的（没经 f64），但字符串带假精度，比丢尾零更误导。
/// `normalized()` 只去尾零、不动有效位，故精度仍然完整。
///
/// 与 MySQL 的差异（见 `docs/devkit/db-guide.md` §10.6）：MySQL 的 `DECIMAL` 由协议文本
/// 解析而来，**保留列声明精度**（`numeric(10,2)` 的 `12.30` 读回 `12.30`）；PG 走规范
/// 形式（`12.3`）。两者数值相等。
fn decimal_text(d: &BigDecimal) -> String {
    d.normalized().to_string()
}

/// 一维数组解码（v0.1.3 / L6）：SQL NULL → JSON `null`，元素 NULL → JSON `null`。
///
/// 必须用 `Vec<Option<T>>`：`Vec<T>` 在遇到 NULL 元素时按 `T::decode` 直接报错。
/// sqlx 只支持一维数组（多维在 `array.rs` 显式拒绝）→ 多维列走到 `None` 响亮报错。
fn array_json<T, F>(row: &PgRow, ordinal: usize, conv: F) -> Option<serde_json::Value>
where
    T: for<'a> sqlx::Decode<'a, Postgres> + sqlx::Type<Postgres> + sqlx::postgres::PgHasArrayType,
    F: Fn(&T) -> serde_json::Value,
{
    let v = row.try_get::<Option<Vec<Option<T>>>, _>(ordinal).ok()?;
    Some(match v {
        Some(items) => serde_json::Value::Array(
            items
                .into_iter()
                .map(|it| it.as_ref().map_or(serde_json::Value::Null, &conv))
                .collect(),
        ),
        None => serde_json::Value::Null,
    })
}

impl Client {
    async fn connect(dsn: &str) -> Result<Self, sqlx::Error> {
        sqlx::any::install_default_drivers();
        let dialect = dialect_of(dsn);
        let dsn_with_cache = with_stmt_cache_capacity(dsn, 512);
        let pool = match dialect {
            // 生产路径：typed PG 池（唯一能解 NUMERIC/JSON/UUID/数组的路径）。
            Dialect::Postgres => Pooled::Pg(
                PoolOptions::<Postgres>::new()
                    .connect(&dsn_with_cache)
                    .await?,
            ),
            // 离线测试（sqlite）回退：Any 池——生产不会走到（装配按 scheme 路由）。
            Dialect::Sqlite | Dialect::MySql => {
                Pooled::Any(PoolOptions::<Any>::new().connect(&dsn_with_cache).await?)
            }
        };
        Ok(Self {
            pool,
            dsn: dsn.to_string(),
            dialect,
            next_tx: AtomicU64::new(0),
            txs: Mutex::new(HashMap::new()),
            streams: Mutex::new(HashMap::new()),
            next_stream: AtomicU64::new(0),
            stream_gate: Arc::new(tokio::sync::Semaphore::new(STREAM_MAX_CONNS)),
        })
    }

    /// ABI 10：打开流式游标（async-stream 自持 SQL/参数/连接——无借用逃逸）。
    /// 方言矩阵：postgres DSN → **typed 专用连接**（cancel/close = 断连 = 服务端终止
    /// 查询，真取消）；其它（sqlite 等）→ Any 池回退（协作式取消）。
    async fn stream_open(&self, sql: &str, params: &[serde_json::Value]) -> Result<u64, String> {
        oj_plugin_ffi::jsint::reject_u64_markers(
            params,
            "postgres bigint is i64 — store it as text, or use a MySQL BIGINT UNSIGNED column",
        )?;
        let sql = shape_tag(sql, params);
        let params: Vec<serde_json::Value> = params.to_vec();
        let is_pg = self.dialect == Dialect::Postgres;
        let mut backend_pid: Option<i32> = None;
        // typed 专用连接占闸（v0.1.84）：拿到配额才拨号；此后任一步失败配额随局部
        // permit drop 归还。Any 回退路径走池（池自身有界），不占闸。
        let gate = if is_pg {
            Some(self.stream_gate.clone().try_acquire_owned().map_err(|_| {
                format!(
                    "db stream_open: too many concurrent cursors (limit {STREAM_MAX_CONNS}) \
                             — close or cancel running streams first"
                )
            })?)
        } else {
            None
        };
        let rows: DbStream = if is_pg {
            // typed 专用连接：连接的所有权随生成器走；cancel = pg_cancel_backend(pid)
            // （服务端立即中断查询）。sqlx Drop 不通知服务器，cancel 必须显式发。
            let mut conn = PgConnection::connect(&self.dsn)
                .await
                .map_err(|e| map_sqlx_err("db stream_open: connect", &e))?;
            backend_pid = Some(
                sqlx::query_scalar::<_, i32>("select pg_backend_pid()")
                    .fetch_one(&mut conn)
                    .await
                    .map_err(|e| map_sqlx_err("db stream_open: backend pid", &e))?,
            );
            Box::pin(async_stream::stream! {
                let mut q: Query<'_, Postgres, PgArguments> =
                    sqlx::query(sqlx::AssertSqlSafe(sql.as_str()));
                for p in &params {
                    q = bind_value_pg(q, p);
                }
                use futures::StreamExt;
                let mut rows = q.fetch(&mut conn);
                while let Some(r) = rows.next().await {
                    match r {
                        Ok(row) => match row_to_json_pg(&row) {
                            Ok(v) => yield Ok(v),
                            Err(e) => {
                                yield Err(e);
                                break;
                            }
                        },
                        Err(e) => {
                            yield Err(format!("db stream: {e}"));
                            break;
                        }
                    }
                }
                // 生成器结束 → conn drop = 断连。
            })
        } else {
            // Any 回退（sqlite 等）：池路径（协作式取消）。
            let pool = match &self.pool {
                Pooled::Any(p) => p.clone(),
                Pooled::Pg(_) => unreachable!("is_pg false ⇒ Any pool"),
            };
            Box::pin(async_stream::stream! {
                let mut q: Query<'_, Any, AnyArguments> =
                    sqlx::query(sqlx::AssertSqlSafe(sql.as_str()));
                for p in &params {
                    q = bind_value(q, p);
                }
                use futures::StreamExt;
                let mut rows = q.fetch(&pool);
                while let Some(r) = rows.next().await {
                    match r {
                        Ok(row) => match row_to_json(&row) {
                            Ok(v) => yield Ok(v),
                            Err(e) => {
                                yield Err(e);
                                break;
                            }
                        },
                        Err(e) => {
                            yield Err(format!("db stream: {e}"));
                            break;
                        }
                    }
                }
            })
        };
        let id = self.next_stream.fetch_add(1, Ordering::SeqCst) + 1;
        self.streams.lock().unwrap().insert(
            id,
            Arc::new(StreamHandle {
                rows: tokio::sync::Mutex::new(rows),
                cancelled: AtomicBool::new(false),
                typed: is_pg,
                backend_pid,
                pending_error: std::sync::Mutex::new(None),
                _gate: gate,
            }),
        );
        Ok(id)
    }

    /// 批量拉取（≤100 行），信封响应：{"rows":[...]} / {"done":true} / {"error":"..."}。
    /// done/err 时条目就地移除，行流 drop = 连接回池（close 再到即幂等 Ok）。
    async fn stream_next(&self, stream_id: u64) -> Result<Vec<u8>, String> {
        let cur = {
            let m = self.streams.lock().unwrap();
            m.get(&stream_id)
                .cloned()
                .ok_or_else(|| format!("db: unknown stream {stream_id}"))?
        };
        if cur.cancelled.load(Ordering::SeqCst) {
            return serde_json::to_vec(&serde_json::json!({ "error": "cancelled" }))
                .map_err(|e| e.to_string());
        }
        // 上拍遗留的延迟错误先行交付（条目移除，行流 drop = 连接回池）。
        if let Some(e) = cur.pending_error.lock().unwrap().take() {
            let _ = self.streams.lock().unwrap().remove(&stream_id);
            return serde_json::to_vec(&serde_json::json!({ "error": e }))
                .map_err(|e| e.to_string());
        }
        let mut rows = cur.rows.lock().await;
        use futures::StreamExt;
        let mut out: Vec<serde_json::Value> = Vec::with_capacity(STREAM_BATCH);
        let mut stream_ended = false;
        while out.len() < STREAM_BATCH {
            match rows.as_mut().next().await {
                Some(Ok(v)) => out.push(v),
                Some(Err(e)) => {
                    // 错误但本批已有行：先交付行，错误经 pending 下一拍上报。
                    if out.is_empty() {
                        let _ = self.streams.lock().unwrap().remove(&stream_id);
                        return serde_json::to_vec(&serde_json::json!({ "error": e }))
                            .map_err(|e| e.to_string());
                    }
                    cur.pending_error.lock().unwrap().replace(e);
                    stream_ended = true;
                    break;
                }
                None => {
                    stream_ended = true;
                    break;
                }
            }
        }
        if !out.is_empty() {
            // 尾批（<100 行）随行照常交付；done 由下一次 next 的空批给出。
            return serde_json::to_vec(&serde_json::json!({ "rows": out }))
                .map_err(|e| e.to_string());
        }
        let _ = self.streams.lock().unwrap().remove(&stream_id);
        if stream_ended {
            return serde_json::to_vec(&serde_json::json!({ "done": true }))
                .map_err(|e| e.to_string());
        }
        // out 为空且流未结束 = 不可达（循环必然 push 或置标志）；防御性报错。
        Err("db stream_next: empty batch without termination".into())
    }

    /// 取消：typed（postgres DSN）= **移除条目并断开专用连接**（服务端终止查询，真取消，
    /// 幂等：条目已无即 no-op）；Any 回退 = 置标志（协作式，下一批返回 cancelled）。
    fn stream_cancel(&self, stream_id: u64) -> Result<Vec<u8>, String> {
        let cur = self.streams.lock().unwrap().remove(&stream_id);
        if let Some(cur) = cur {
            if cur.typed {
                // 方言级真取消：独立短连接发 pg_cancel_backend(pid)，服务端立即中断查询
                // （sqlx Drop 不通知服务器——断连要等 keepalive，必须显式取消）。
                if let Some(pid) = cur.backend_pid {
                    let dsn = self.dsn.clone();
                    tokio::spawn(async move {
                        if let Ok(mut c) = PgConnection::connect(&dsn).await {
                            let _ = sqlx::query("select pg_cancel_backend($1)")
                                .bind(pid)
                                .execute(&mut c)
                                .await;
                            let _ = c.close_hard().await;
                        }
                    });
                }
                return Ok(b"".to_vec()); // 条目已移除（游标 drop，连接随后关闭）
            }
            cur.cancelled.store(true, Ordering::SeqCst);
            self.streams.lock().unwrap().insert(stream_id, cur);
        }
        Ok(b"".to_vec())
    }

    /// 显式 reclaim：移除条目（进行中的批量结束后行流 drop = 连接回池）。幂等。
    fn stream_close(&self, stream_id: u64) -> Result<Vec<u8>, String> {
        let _ = self.streams.lock().unwrap().remove(&stream_id);
        Ok(b"".to_vec())
    }

    async fn query(&self, sql: &str, params: &[serde_json::Value]) -> Result<Vec<u8>, String> {
        // PG 的 bigint 即 i64：`$oj$u64`（toUBigInt）装不下 → 明确拒绝，勿静默坍缩。
        oj_plugin_ffi::jsint::reject_u64_markers(
            params,
            "postgres bigint is i64 — store it as text, or use a MySQL BIGINT UNSIGNED column",
        )?;
        let sql = shape_tag(sql, params);
        match &self.pool {
            Pooled::Pg(p) => {
                let mut q: Query<'_, Postgres, PgArguments> = sqlx::query(sqlx::AssertSqlSafe(sql));
                for v in params {
                    q = bind_value_pg(q, v);
                }
                let rows = q
                    .fetch_all(p)
                    .await
                    .map_err(|e| map_sqlx_err("db query", &e))?;
                let json: Result<Vec<_>, String> = rows.iter().map(row_to_json_pg).collect();
                serde_json::to_vec(&json?).map_err(|e| format!("db query serialize: {e}"))
            }
            Pooled::Any(p) => {
                let mut q: Query<'_, Any, AnyArguments> = sqlx::query(sqlx::AssertSqlSafe(sql));
                for v in params {
                    q = bind_value(q, v);
                }
                let rows = q
                    .fetch_all(p)
                    .await
                    .map_err(|e| map_sqlx_err("db query", &e))?;
                let json: Result<Vec<_>, String> = rows.iter().map(row_to_json).collect();
                serde_json::to_vec(&json?).map_err(|e| format!("db query serialize: {e}"))
            }
        }
    }

    async fn exec(&self, sql: &str, params: &[serde_json::Value]) -> Result<Vec<u8>, String> {
        oj_plugin_ffi::jsint::reject_u64_markers(
            params,
            "postgres bigint is i64 — store it as text, or use a MySQL BIGINT UNSIGNED column",
        )?;
        let sql = shape_tag(sql, params);
        match &self.pool {
            Pooled::Pg(p) => {
                let mut q: Query<'_, Postgres, PgArguments> = sqlx::query(sqlx::AssertSqlSafe(sql));
                for v in params {
                    q = bind_value_pg(q, v);
                }
                let res = q
                    .execute(p)
                    .await
                    .map_err(|e| map_sqlx_err("db exec", &e))?;
                serde_json::to_vec(&(res.rows_affected() as i64))
                    .map_err(|e| format!("db exec serialize: {e}"))
            }
            Pooled::Any(p) => {
                let mut q: Query<'_, Any, AnyArguments> = sqlx::query(sqlx::AssertSqlSafe(sql));
                for v in params {
                    q = bind_value(q, v);
                }
                let res = q
                    .execute(p)
                    .await
                    .map_err(|e| map_sqlx_err("db exec", &e))?;
                serde_json::to_vec(&(res.rows_affected() as i64))
                    .map_err(|e| format!("db exec serialize: {e}"))
            }
        }
    }

    async fn begin(&self) -> Result<u64, String> {
        let inner = match &self.pool {
            Pooled::Pg(p) => TxInner::Pg(tokio::sync::Mutex::new(Some(
                p.begin()
                    .await
                    .map_err(|e| map_sqlx_err("db tx begin", &e))?,
            ))),
            Pooled::Any(p) => TxInner::Any(tokio::sync::Mutex::new(Some(
                p.begin()
                    .await
                    .map_err(|e| map_sqlx_err("db tx begin", &e))?,
            ))),
        };
        let id = self.next_tx.fetch_add(1, Ordering::SeqCst) + 1;
        self.txs
            .lock()
            .unwrap()
            .insert(id, Arc::new(Tx { tx: inner }));
        Ok(id)
    }

    fn tx(&self, tx_id: u64) -> Result<Arc<Tx>, String> {
        self.txs
            .lock()
            .unwrap()
            .get(&tx_id)
            .cloned()
            .ok_or_else(|| format!("db: unknown tx {tx_id}"))
    }

    async fn tx_query(
        &self,
        tx_id: u64,
        sql: &str,
        params: &[serde_json::Value],
    ) -> Result<Vec<u8>, String> {
        let tx = self.tx(tx_id)?;
        oj_plugin_ffi::jsint::reject_u64_markers(
            params,
            "postgres bigint is i64 — store it as text, or use a MySQL BIGINT UNSIGNED column",
        )?;
        let sql = shape_tag(sql, params);
        match &tx.tx {
            TxInner::Pg(m) => {
                let mut g = m.lock().await;
                let Some(t) = g.as_mut() else {
                    return Err("tx finished".into());
                };
                let mut q: Query<'_, Postgres, PgArguments> = sqlx::query(sqlx::AssertSqlSafe(sql));
                for v in params {
                    q = bind_value_pg(q, v);
                }
                let rows = q
                    .fetch_all(&mut **t)
                    .await
                    .map_err(|e| map_sqlx_err("db tx query", &e))?;
                let json: Result<Vec<_>, String> = rows.iter().map(row_to_json_pg).collect();
                serde_json::to_vec(&json?).map_err(|e| format!("db tx query serialize: {e}"))
            }
            TxInner::Any(m) => {
                let mut g = m.lock().await;
                let Some(t) = g.as_mut() else {
                    return Err("tx finished".into());
                };
                let mut q: Query<'_, Any, AnyArguments> = sqlx::query(sqlx::AssertSqlSafe(sql));
                for v in params {
                    q = bind_value(q, v);
                }
                let rows = q
                    .fetch_all(&mut **t)
                    .await
                    .map_err(|e| map_sqlx_err("db tx query", &e))?;
                let json: Result<Vec<_>, String> = rows.iter().map(row_to_json).collect();
                serde_json::to_vec(&json?).map_err(|e| format!("db tx query serialize: {e}"))
            }
        }
    }

    async fn tx_exec(
        &self,
        tx_id: u64,
        sql: &str,
        params: &[serde_json::Value],
    ) -> Result<Vec<u8>, String> {
        let tx = self.tx(tx_id)?;
        oj_plugin_ffi::jsint::reject_u64_markers(
            params,
            "postgres bigint is i64 — store it as text, or use a MySQL BIGINT UNSIGNED column",
        )?;
        let sql = shape_tag(sql, params);
        match &tx.tx {
            TxInner::Pg(m) => {
                let mut g = m.lock().await;
                let Some(t) = g.as_mut() else {
                    return Err("tx finished".into());
                };
                let mut q: Query<'_, Postgres, PgArguments> = sqlx::query(sqlx::AssertSqlSafe(sql));
                for v in params {
                    q = bind_value_pg(q, v);
                }
                let res = q
                    .execute(&mut **t)
                    .await
                    .map_err(|e| map_sqlx_err("db tx exec", &e))?;
                serde_json::to_vec(&(res.rows_affected() as i64))
                    .map_err(|e| format!("db tx exec serialize: {e}"))
            }
            TxInner::Any(m) => {
                let mut g = m.lock().await;
                let Some(t) = g.as_mut() else {
                    return Err("tx finished".into());
                };
                let mut q: Query<'_, Any, AnyArguments> = sqlx::query(sqlx::AssertSqlSafe(sql));
                for v in params {
                    q = bind_value(q, v);
                }
                let res = q
                    .execute(&mut **t)
                    .await
                    .map_err(|e| map_sqlx_err("db tx exec", &e))?;
                serde_json::to_vec(&(res.rows_affected() as i64))
                    .map_err(|e| format!("db tx exec serialize: {e}"))
            }
        }
    }

    async fn tx_commit(&self, tx_id: u64) -> Result<Vec<u8>, String> {
        let tx = self
            .txs
            .lock()
            .unwrap()
            .remove(&tx_id)
            .ok_or_else(|| format!("db: unknown tx {tx_id}"))?;
        match &tx.tx {
            TxInner::Pg(m) => {
                let Some(t) = m.lock().await.take() else {
                    return Err("tx finished".into());
                };
                t.commit()
                    .await
                    .map_err(|e| map_sqlx_err("db tx commit", &e))?;
            }
            TxInner::Any(m) => {
                let Some(t) = m.lock().await.take() else {
                    return Err("tx finished".into());
                };
                t.commit()
                    .await
                    .map_err(|e| map_sqlx_err("db tx commit", &e))?;
            }
        }
        Ok(b"".to_vec())
    }

    async fn tx_rollback(&self, tx_id: u64) -> Result<Vec<u8>, String> {
        let tx = self
            .txs
            .lock()
            .unwrap()
            .remove(&tx_id)
            .ok_or_else(|| format!("db: unknown tx {tx_id}"))?;
        match &tx.tx {
            TxInner::Pg(m) => {
                let Some(t) = m.lock().await.take() else {
                    return Err("tx finished".into());
                };
                t.rollback()
                    .await
                    .map_err(|e| map_sqlx_err("db tx rollback", &e))?;
            }
            TxInner::Any(m) => {
                let Some(t) = m.lock().await.take() else {
                    return Err("tx finished".into());
                };
                t.rollback()
                    .await
                    .map_err(|e| map_sqlx_err("db tx rollback", &e))?;
            }
        }
        Ok(b"".to_vec())
    }
}

impl DbPluginState {
    fn client(&self, handle: u64) -> Result<Arc<Client>, String> {
        self.clients
            .lock()
            .unwrap()
            .get(&handle)
            .cloned()
            .ok_or_else(|| format!("db: unknown handle {handle}"))
    }

    async fn do_query(&self, handle: u64, sql: &str, params: &str) -> Result<Vec<u8>, String> {
        let p: Vec<serde_json::Value> =
            serde_json::from_str(params).map_err(|e| format!("db query: bad params: {e}"))?;
        self.client(handle)?.query(sql, &p).await
    }

    async fn do_exec(&self, handle: u64, sql: &str, params: &str) -> Result<Vec<u8>, String> {
        let p: Vec<serde_json::Value> =
            serde_json::from_str(params).map_err(|e| format!("db exec: bad params: {e}"))?;
        self.client(handle)?.exec(sql, &p).await
    }

    async fn do_tx_query(
        &self,
        handle: u64,
        tx_id: u64,
        sql: &str,
        params: &str,
    ) -> Result<Vec<u8>, String> {
        let p: Vec<serde_json::Value> =
            serde_json::from_str(params).map_err(|e| format!("db tx_query: bad params: {e}"))?;
        self.client(handle)?.tx_query(tx_id, sql, &p).await
    }

    async fn do_tx_exec(
        &self,
        handle: u64,
        tx_id: u64,
        sql: &str,
        params: &str,
    ) -> Result<Vec<u8>, String> {
        let p: Vec<serde_json::Value> =
            serde_json::from_str(params).map_err(|e| format!("db tx_exec: bad params: {e}"))?;
        self.client(handle)?.tx_exec(tx_id, sql, &p).await
    }

    async fn do_stream_open(
        &self,
        handle: u64,
        sql: &str,
        params: &str,
    ) -> Result<Vec<u8>, String> {
        let p: Vec<serde_json::Value> =
            serde_json::from_str(params).map_err(|e| format!("db stream_open: bad params: {e}"))?;
        let sid = self.client(handle)?.stream_open(sql, &p).await?;
        Ok(format!(r#"{{"stream_id":{sid}}}"#).into_bytes())
    }

    async fn do_stream_next(&self, handle: u64, stream_id: u64) -> Result<Vec<u8>, String> {
        self.client(handle)?.stream_next(stream_id).await
    }

    fn do_stream_cancel(&self, handle: u64, stream_id: u64) -> Result<Vec<u8>, String> {
        self.client(handle)?.stream_cancel(stream_id)
    }

    fn do_stream_close(&self, handle: u64, stream_id: u64) -> Result<Vec<u8>, String> {
        self.client(handle)?.stream_close(stream_id)
    }
}

// ---- vtable ----

extern "C" fn connect(cfg: RString) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(&st.rt, async move {
            let dsn = cfg[..].to_string();
            let client = Client::connect(&dsn)
                .await
                .map_err(|e| map_sqlx_err("db connect", &e))?;
            let handle = st.next_handle.fetch_add(1, Ordering::SeqCst) + 1;
            st.clients.lock().unwrap().insert(handle, Arc::new(client));
            Ok(format!(r#"{{"handle":{handle}}}"#).into_bytes())
        })
    })
}

extern "C" fn query(handle: u64, sql: RString, params: RString) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(&st.rt, async move {
            st.do_query(handle, &sql[..], &params[..]).await
        })
    })
}

extern "C" fn exec(handle: u64, sql: RString, params: RString) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(&st.rt, async move {
            st.do_exec(handle, &sql[..], &params[..]).await
        })
    })
}

extern "C" fn begin(handle: u64) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(&st.rt, async move {
            let c = st.client(handle)?;
            let tx_id = c.begin().await?;
            Ok(format!(r#"{{"tx_id":{tx_id}}}"#).into_bytes())
        })
    })
}

extern "C" fn tx_query(handle: u64, tx_id: u64, sql: RString, params: RString) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(&st.rt, async move {
            st.do_tx_query(handle, tx_id, &sql[..], &params[..]).await
        })
    })
}

extern "C" fn tx_exec(handle: u64, tx_id: u64, sql: RString, params: RString) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(&st.rt, async move {
            st.do_tx_exec(handle, tx_id, &sql[..], &params[..]).await
        })
    })
}

extern "C" fn tx_commit(handle: u64, tx_id: u64) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(
            &st.rt,
            async move { st.client(handle)?.tx_commit(tx_id).await },
        )
    })
}

extern "C" fn tx_rollback(handle: u64, tx_id: u64) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(&st.rt, async move {
            st.client(handle)?.tx_rollback(tx_id).await
        })
    })
}

// ---- ABI 10：流式查询（批量 pull + 信封，spec §1.4）----

extern "C" fn stream_open(handle: u64, sql: RString, params: RString) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(&st.rt, async move {
            st.do_stream_open(handle, &sql[..], &params[..]).await
        })
    })
}

extern "C" fn stream_next(handle: u64, stream_id: u64) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(&st.rt, async move {
            st.do_stream_next(handle, stream_id).await
        })
    })
}

extern "C" fn stream_cancel(handle: u64, stream_id: u64) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(
            &st.rt,
            async move { st.do_stream_cancel(handle, stream_id) },
        )
    })
}

extern "C" fn stream_close(handle: u64, stream_id: u64) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(
            &st.rt,
            async move { st.do_stream_close(handle, stream_id) },
        )
    })
}

/// 方言自报（签名 `fn(u64) -> RString` 带不了错）：未知/closed handle 上送 `"unknown"`
/// （宿主按 Sqlite 兜底映射，见 src/bridge/ffi.rs 的 dialect 消费方），并响亮提示——
/// 绝不冒充真方言 `"sqlite"` 让宿主拿错方言造 SQL。
extern "C" fn dialect(handle: u64) -> RString {
    oj_plugin_ffi::catch_value(
        || match state().client(handle) {
            Ok(c) => RString::from(dialect_str(c.dialect)),
            Err(e) => {
                eprintln!("oj-db-postgres: dialect: {e}");
                RString::from("unknown")
            }
        },
        RString::from("unknown"),
    )
}

extern "C" fn close(handle: u64) {
    oj_plugin_ffi::catch_void(|| {
        state().clients.lock().unwrap().remove(&handle);
    })
}

extern "C" fn schemes() -> RVec<RString> {
    oj_plugin_ffi::catch_value(
        || {
            let mut v = RVec::new();
            v.push(RString::from("postgres://"));
            v.push(RString::from("postgresql://"));
            v
        },
        RVec::new(),
    )
}

/// ABI 12：工厂级方言描述符（临时终稿——真实能力声明在 M1 T11 复核映射表后钉死）。
extern "C" fn describe() -> RString {
    RString::from(
        r#"{"dialect":"postgres","capabilities":["placeholder_dollar","id_returning","stream","cancel"]}"#,
    )
}

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
        name: RString::from("db-postgres"),
        semver: RString::from(env!("CARGO_PKG_VERSION")),
        abi_version: ABI_VERSION,
        fingerprint: RString::from(oj_plugin_ffi::HOST_FINGERPRINT),
        desc: RString::from(
            "db 轴 postgres cdylib 插件：sqlx Any 单方言（postgres）迁移自 core SqlxAccessor",
        ),
    }
}

fn init(host: RArc<HostContext>, cfg: RString) -> RResult<PluginDescriptor, RString> {
    if PLUGIN.get().is_some() {
        return RResult::Ok(descriptor());
    }
    let _ = (&host, &cfg); // db 插件 init 无装配期配置（DSN 在 connect 传入）
    // get_or_init：并发 init 时闭包只跑一次（竞争方阻塞复用），不重复建 runtime，
    // 避免 `let _ = set(st)` 在竞争下把败者的 tokio Runtime 从 async 上下文 drop 崩溃。
    PLUGIN.get_or_init(|| DbPluginState {
        rt: runtime(),
        clients: Mutex::new(HashMap::new()),
        next_handle: AtomicU64::new(0),
    });
    RResult::Ok(descriptor())
}

fn runtime() -> tokio::runtime::Runtime {
    // worker 只跑 IO 转发（sqlx 连接在池内），2 足够；缺省 = num_cpus 全核白占线程。
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("oj-db-postgres tokio runtime")
}

oj_plugin_ffi::oj_plugin_entry!(init, db => &VTABLE);

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
