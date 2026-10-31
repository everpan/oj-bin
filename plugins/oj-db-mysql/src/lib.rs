//! oj-db-mysql：db 轴 mysql cdylib 插件（spec §3 试点成型；plan Task 4.1）。
//! 迁移 core `SqlxAccessor` 的 sqlx 逻辑（`sqlx::Any` + 单方言 mysql feature），
//! 自建 tokio runtime；vtable `connect` 收 DSN（装配层按 scheme 路由到本插件），
//! handle 查表。事务句柄化（tx_id → Tx，spec §3 难点特判消嵌套 trait object）。
//!
//! 决策记录（plan Task 4.1 Step 3）：db 双插件（mysql/postgres）接受复制——各自
//! 自包含、独立编译，sqlx 驱动 feature 各管各的；不抽共享 crate（spec §3 插件自包含
//! 哲学优先于 DRY；复制的 bind/row 逻辑来自 core 全量测试过的 accessor_sqlx.rs）。
//!
//! cfg 契约：init cfg = `{}`（db 插件无装配期配置；DSN 在 connect 按值传入）。
//! 句柄约定：connect 分配 handle（AtomicU64）；tx 分配 tx_id（每 client AtomicU64）。

use oj_plugin_ffi::{
    ABI_VERSION, DataAccessorVtable, FfiFuture, HostContext, PluginDescriptor, RArc, RResult,
    RString, RVec,
};
use sqlx::any::{Any, AnyArguments, AnyRow};
use sqlx::error::ErrorKind;
use sqlx::mysql::{MySql, MySqlArguments, MySqlColumn, MySqlConnection, MySqlRow};
use sqlx::pool::{Pool, PoolOptions};
use sqlx::query::Query;
use sqlx::types::BigDecimal;
use sqlx::types::Json;
use sqlx::types::chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use sqlx::{Column, Connection as _, Row, TypeInfo};
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
            // MySQL 同样给出 SQLSTATE）——否则 syntax/timeout/capacity/cancelled 永远没有生产者。
            _ => sqlstate_class(db.code().as_deref()).unwrap_or(("internal", false, "keep")),
        },
        _ => ("internal", false, "keep"),
    }
}

/// SQLSTATE 类别 → (category, retryable, session_disposition)。
/// 只认标准类（MySQL 与 PG 共用）；未知类返回 None → 调用方落 internal（禁止猜）。
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
    match e {
        sqlx::Error::Database(db) => {
            // mysql 生产路径走 typed 驱动：原生 errno（u16，如 1062 重复键）比 SQLSTATE
            // 更贴近下游判断，原样透传；非 mysql 驱动（离线 Any 路径）回落 SQLSTATE 串。
            if let Some(m) = db.try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>() {
                return serde_json::Value::from(m.number() as i64);
            }
            // 非 typed 驱动（离线 Any 路径）：只有 SQLSTATE 可用，**原样透传为字符串**。
            // 不做 `parse::<i64>()`——那会把 `23000` 之类包装成数字，与原生 errno 无法区分，
            // 违反「未知码禁止猜」。
            match db.code() {
                Some(code) => serde_json::Value::from(code.as_ref()),
                None => serde_json::Value::Null,
            }
        }
        _ => serde_json::Value::Null,
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

/// DSN 前缀 → 方言（本插件只接 mysql；dialect 上送 host 选 sea-query builder）。
fn dialect_of(dsn: &str) -> Dialect {
    if dsn.starts_with("mysql://") {
        Dialect::MySql
    } else if dsn.starts_with("postgres://") || dsn.starts_with("postgresql://") {
        Dialect::Postgres
    } else {
        Dialect::Sqlite
    }
}

fn dialect_str(d: Dialect) -> &'static str {
    match d {
        Dialect::Sqlite => "sqlite",
        Dialect::MySql => "mysql",
        Dialect::Postgres => "postgres",
    }
}

/// 插件共享状态（进程级单例，init 建立）。
struct DbPluginState {
    rt: tokio::runtime::Runtime,
    clients: Mutex<HashMap<u64, Arc<Client>>>,
    next_handle: AtomicU64,
}

/// 流式游标专用连接并发上限（v0.1.84）：typed 路径每游标单开一条池外连接，原无上限——
/// 并发大结果集打满服务端 max_connections 后连池路径也拿不到连接（全库不可用）。
/// 上限 8（sqlx 池缺省 10，留余量）；超限 fail-loud。
const STREAM_MAX_CONNS: usize = 8;

// ---- MySql typed 路径（v0.1.24）：只有它能精确承载 u64 / `BIGINT UNSIGNED` ----

/// 参数绑定（MySql）：与 Any 侧同构，外加 `$oj$u64` → `u64`（sqlx-mysql 原生支持）。
fn bind_value_mysql<'q>(
    q: Query<'q, MySql, MySqlArguments>,
    v: &serde_json::Value,
) -> Query<'q, MySql, MySqlArguments> {
    // 标记分支必须在对象兜底之前（否则被串化成文本）。i64 先判、u64 后判：两者键名不同，互不误认。
    if let Some(i) = oj_plugin_ffi::jsint::marker_i64(v) {
        return q.bind(i);
    }
    if let Some(u) = oj_plugin_ffi::jsint::marker_u64(v) {
        return q.bind(u);
    }
    match v {
        serde_json::Value::Null => q.bind(None::<String>),
        serde_json::Value::Bool(b) => q.bind(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                q.bind(i)
            } else if let Some(u) = n.as_u64() {
                // serde_json 的 PosInt 到 u64::MAX：绑 u64（Any 侧无此能力，会先落 f64 丢精度）。
                q.bind(u)
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

/// 单行 MySqlRow -> serde_json::Value（typed 路径）。
///
/// 出现**本插件还不会解码的列类型**时**响亮报错**（`Err`），绝不落成静默 `null`：
/// `sqlx::Any` 时代这类列（`DECIMAL` / 时间 / `JSON` / `BIT` …）会抛 `AnyDriverError`，
/// typed 化后若沿用 `unwrap_or(Null)` 就把「报错」退化成「错值」，踩本仓红线。
///
/// REQ-07a 降级（`$<type>:<base64>`）在 PG 已落地；MySQL 侧暂不可行——sqlx-mysql 0.9
/// 把 `MySqlValueRef::as_bytes` 收为 `pub(crate)`（0.7 的 `ValueRef::as_bytes` 公开 API
/// 在 0.9 被移除），插件侧无公开原始字节出口；`Vec<u8>` 解码的 compatible 面不含
/// `GEOMETRY`。维持 fail-loud 是 status quo（无回归）；降级需等 sqlx 上游恢复出口或
/// REQ-07b 另立机制（见 ledger Task 4 ruling）。
fn row_to_json_mysql(row: &MySqlRow) -> Result<serde_json::Value, String> {
    let mut obj = serde_json::Map::new();
    for col in row.columns() {
        let name = safe_ident(col.name());
        let ty = col.type_info().name();
        let val = column_json_mysql(row, col).ok_or_else(|| {
            let hint = if matches!(ty, "GEOMETRY" | "POINT" | "LINESTRING" | "POLYGON") {
                format!("`ST_AsText({name}) as {name}`")
            } else {
                format!("`cast({name} as char) as {name}`")
            };
            format!(
                "db(mysql): column '{name}' has MySQL type '{ty}' which this plugin does not \
                 decode yet — select explicit columns and cast it in SQL (e.g. {hint}); \
                 see docs/devkit/db-guide.md §10.6"
            )
        })?;
        obj.insert(name, val);
    }
    Ok(serde_json::Value::Object(obj))
}

/// 逐列按**固定的安全顺序**探测（typed 路径）：
/// `BIT/YEAR 类型名短路 → u64 → i64 → bool → f64 → String → Vec<u8> → 受限类型分派`。
///
/// 顺序为什么是安全的（Any 侧做不到这一点）：
/// - `BIT` / `YEAR` 必须最先按**类型名**短路：`bool::compatible` 无条件包含
///   `ColumnType::Bit`、`u64::compatible` 在 UNSIGNED 时也含 `Bit`/`Year`，若走探测链
///   `BIT(5)` 会被当成 bool/number **静默返回错值**（L6 修真，见 `CHANGELOG` v0.1.94）；
/// - `u64::compatible` 只接受 `BIGINT UNSIGNED`（`sqlx-mysql/src/types/uint.rs`）→ 放最前，
///   `> i64::MAX` 的无符号值**不再回绕成负数**（Any 的 `AnyValueKind::BigInt(i64)` 会）；
/// - `i64` 只接受非 unsigned 整数；
/// - `bool::compatible` 与 `f64::compatible` 都**接受整型**（`types/bool.rs` / `float.rs`），
///   若把它们排在整数之前，任意 BIGINT 列会被当成 bool（值 256 → `false`）——这是必须避免的
///   静默错值，也正是本函数不能沿用「`if let Ok` 盲探测」的原因。
///
/// 能力边界（真库用例 `real_mysql_unsupported_column_types_error_loudly` 钉住）：
/// - `bool` 分支实际上**不可达**（`bool::compatible` 的整型集合是 `int_compatible` 的子集，
///   而 `i64` 已在前面命中）→ MySQL 的 `BOOLEAN` / `TINYINT(1)` 读出 **1/0（number）**，
///   不是 `true`/`false`。保留该分支是作为顺序被重排时的显式意图标记，勿删；
/// - `String` / `Vec<u8>` 的 `compatible` 只认 `VarChar|*Blob|String|VarString|Enum`
///   （`str` 另需非 BINARY collation）；
/// - 剩下的 `DECIMAL|NEWDECIMAL|DATE|TIME|DATETIME|TIMESTAMP|JSON` 交给
///   [`column_json_mysql_restricted`]（v0.1.3 / L6 / U-6 补齐）；
///   **仍未覆盖**的（`GEOMETRY` 家族、`BIT` 之外的位类型、`SET` 等）返回 `None` →
///   由调用方响亮报错，绝不静默落 `null`。
///   （说明：MySQL 的 `TEXT` 家族在协议里就是 `Blob` + 非 BINARY collation，故可正常读出。）
fn column_json_mysql(row: &MySqlRow, col: &MySqlColumn) -> Option<serde_json::Value> {
    let ordinal = col.ordinal();
    // v0.1.3（L6 / U-6）：`BIT` / `YEAR` 必须在通用探测链**之前**按类型名短路。
    // 原因：`bool::compatible` 无条件包含 `ColumnType::Bit`、`u64::compatible` 在 UNSIGNED
    // 时也含 Bit —— BIT(n>1) 会被前两条探测吃掉，**静默返回 true/数字（错值）**。
    // `YEAR` 同理：`int.rs` 没有 Year 分支，只有 UNSIGNED 的 `u16` 能匹配。
    match col.type_info().name() {
        "BIT" => {
            return row
                .try_get::<Option<u64>, _>(ordinal)
                .ok()
                .map(|v| match v {
                    // 位串文本（无前导零——sqlx 不暴露 BIT 宽度）。
                    Some(bits) => serde_json::Value::String(format!("{bits:b}")),
                    None => serde_json::Value::Null,
                });
        }
        "YEAR" => {
            return row
                .try_get::<Option<u16>, _>(ordinal)
                .ok()
                .map(|v| match v {
                    Some(y) => serde_json::Value::from(y),
                    None => serde_json::Value::Null,
                });
        }
        _ => {}
    }
    if let Ok(v) = row.try_get::<Option<u64>, _>(ordinal) {
        return Some(match v {
            Some(u) => serde_json::Value::from(u),
            None => serde_json::Value::Null,
        });
    }
    if let Ok(v) = row.try_get::<Option<i64>, _>(ordinal) {
        return Some(match v {
            Some(i) => serde_json::Value::from(i),
            None => serde_json::Value::Null,
        });
    }
    if let Ok(v) = row.try_get::<Option<bool>, _>(ordinal) {
        return Some(serde_json::Value::from(v));
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
            // REQ-07b：Vec<u8> 探针臂恒定 cellenv 信封（含合法 UTF-8 内容，breaking）。
            // 判别依据：sqlx 对 binary charset 拒 String 解码，String 探针先行失败——
            // TEXT 家族（非 BINARY collation）永不落入本臂，无需前置类型名短路。
            Some(b) => serde_json::Value::String(oj_plugin_ffi::cellenv::encode(
                col.type_info().name(),
                &b,
            )),
            None => serde_json::Value::Null,
        });
    }
    // L6（U-6）：既有六类全部落空后才进受限类型分派——不改上面任何既有列的判定顺序。
    column_json_mysql_restricted(row, ordinal, col.type_info().name())
}

/// 受限类型分派（v0.1.3 / L6，PRD U-6）：`None` = 未覆盖 → 调用方响亮报错。
///
/// 输出口径见 `docs/devkit/db-guide.md` §10.6：
/// 十进制字符串 / ISO 8601 / JSON 文本；**只读**（写侧仍须显式 cast）。
fn column_json_mysql_restricted(
    row: &MySqlRow,
    ordinal: usize,
    ty: &str,
) -> Option<serde_json::Value> {
    fn s(v: String) -> serde_json::Value {
        serde_json::Value::String(v)
    }
    match ty {
        "DECIMAL" | "NEWDECIMAL" => row
            .try_get::<Option<BigDecimal>, _>(ordinal)
            .ok()
            .map(|v| v.map_or(serde_json::Value::Null, |d| s(d.to_string()))),
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
        "DATETIME" => row
            .try_get::<Option<NaiveDateTime>, _>(ordinal)
            .ok()
            .map(|v| {
                v.map_or(serde_json::Value::Null, |t| {
                    s(t.format("%Y-%m-%dT%H:%M:%S%.f").to_string())
                })
            }),
        // MySQL TIMESTAMP 是 UTC 语义；`NaiveDateTime::compatible` 只认 Datetime，
        // 必须用 `DateTime<Utc>`（见 sqlx-mysql `types/chrono.rs`）。
        "TIMESTAMP" => row
            .try_get::<Option<DateTime<Utc>>, _>(ordinal)
            .ok()
            .map(|v| {
                // 与 DATETIME 同形，仅多一个 UTC 时区后缀（`sqlx::types::chrono`
                // 只再导出类型、不含 `SecondsFormat`，故用显式格式串而非 `to_rfc3339_opts`）。
                v.map_or(serde_json::Value::Null, |t| {
                    s(t.format("%Y-%m-%dT%H:%M:%S%.fZ").to_string())
                })
            }),
        // `String` 不兼容 `ColumnType::Json`，只能走 `Json<T>`。
        "JSON" => row
            .try_get::<Option<Json<serde_json::Value>>, _>(ordinal)
            .ok()
            .map(|v| v.map_or(serde_json::Value::Null, |j| s(j.0.to_string()))),
        _ => None,
    }
}

/// 池的二选一（v0.1.24）：**生产 DSN（`mysql://`）走 typed**，其余（离线测试的 sqlite）走 Any。
///
/// `Pooled::Any` 分支是**仅测试用**的（不是受支持的运行时路径）：它只在「非 mysql DSN」时出现，
/// 而生产装配按 scheme 路由——`mysql://` 必进本插件且必走 typed。**Any 路径验证不到生产 MySQL
/// 的行解码**（`Any` 的 `TryFrom<MySqlTypeInfo>` 缺 `Tiny` 等分支，Any+MySQL 组合本就不可用），
/// 故它只覆盖「编排/绑定通路」；MySQL 行形状的回归靠 env-gated 真库用例（见 `CHANGELOG` 的 CI 挂账）。
///
/// 为什么保留 Any：本插件的离线全路径 roundtrip 是 CI 里唯一不依赖 MySQL 服务的覆盖
/// （CI 无 MySQL service，见 `CHANGELOG` 的 P1-3 挂账；typed 化后 sqlite DSN 无法用 `MySql` 池，
/// 故用枚举分流而不是删掉那条用例）。**本机已用 macOS `container` 起 mysql:8.4 实跑过
/// `real_mysql_*` 全部用例**（v0.1.24），但那不是 CI 覆盖。
enum Pooled {
    MySql(Pool<MySql>),
    Any(Pool<Any>),
}

/// 事务内层（与池同型分流）。
enum TxInner {
    MySql(tokio::sync::Mutex<Option<sqlx::Transaction<'static, MySql>>>),
    Any(tokio::sync::Mutex<Option<sqlx::Transaction<'static, Any>>>),
}

/// 单连接（vtable connect 建立）。txs 查表（事务句柄化）。
struct Client {
    pool: Pooled,
    dialect: Dialect,
    dsn: String,
    next_tx: AtomicU64,
    txs: Mutex<HashMap<u64, Arc<Tx>>>,
    /// ABI 10 流式游标（stream_id → 行流 + 协作式取消标志）。
    streams: Mutex<HashMap<u64, Arc<StreamHandle>>>,
    next_stream: AtomicU64,
    /// 流式专用连接闸（v0.1.84，`STREAM_MAX_CONNS`）：typed 游标先占配额再拨号，
    /// 条目移除（done/err/cancel/close）随 handle drop 释放。
    stream_gate: Arc<tokio::sync::Semaphore>,
}

/// 流式游标句柄：行流独立成 tokio Mutex（批量推进期间独占），取消标志在外层
/// 无锁置位。方言支持矩阵（v0.1.42，与 pg 插件同构）：
/// - **mysql:// DSN（typed）**：游标单开一条 `MySqlConnection`，`stream_cancel` = **移除
///   条目并 `KILL QUERY` 专用连接**（服务端立即中断查询——真取消）；
/// - **其它（Any 回退，如 sqlite）**：池路径 + 协作式取消（置标志，下一批返回
///   `{"error":"cancelled"}`；`stream_close` 移除条目，行流 drop = 连接回池）。
struct StreamHandle {
    rows: tokio::sync::Mutex<DbStream>,
    cancelled: AtomicBool,
    /// true = typed 专用连接（真取消：cancel = KILL QUERY + 断连）；false = Any 回退。
    typed: bool,
    /// typed 连接的 CONNECTION_ID（KILL QUERY 定位用；连接建立时查询）。
    conn_id: Option<u64>,
    /// 错误延迟上报：错误发生时本批已有行 → 行先交付，错误下一拍给出。
    pending_error: std::sync::Mutex<Option<String>>,
    /// 流式专用连接闸配额（v0.1.84）：typed 游标持有，条目移除（done/err/cancel/close）
    /// 即随 handle drop 释放；Any 回退路径无专用连接，为 None。（仅 Drop 有意义，故 `_` 前缀。）
    _gate: Option<tokio::sync::OwnedSemaphorePermit>,
}

/// 每次 stream_next 的批量行数上限（spec §1.4：砍逐行 FFI 往返）。
const STREAM_BATCH: usize = 100;

/// 事务：Option 被 take 后（已完结）再调用报 "tx finished"；Mutex 串行并发 op。
struct Tx {
    tx: TxInner,
}

static PLUGIN: OnceLock<DbPluginState> = OnceLock::new();

fn state() -> &'static DbPluginState {
    PLUGIN.get().expect("oj-db-mysql: init not called")
}

// ---- FfiFuture 桥（统一走 oj-plugin-ffi 的 catch_unwind 安全工厂：spawn_ffi_future / catch_future）----

// ---- sqlx 逻辑（迁移自 core accessor_sqlx.rs，绑定/行转换逐字对齐）----

/// 将单个 JSON 值绑定到 sqlx 语句（按类型选择可 Encode 的具体类型）。
fn bind_value_any<'q>(
    q: Query<'q, Any, AnyArguments>,
    v: &serde_json::Value,
) -> Query<'q, Any, AnyArguments> {
    // 大整数标记（v0.1.22，`toBigInt()` 的返回值）：绑 i64。必须在对象分支之前——
    // 否则会被 `other => to_string()` 串化成文本（与 pg/sqlite 同构）。
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

/// 单行 AnyRow -> serde_json::Value（离线/测试路径；列类型信息已被 Any 抹平）。
fn row_to_json_any(row: &AnyRow) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    for col in row.columns() {
        let name = col.name().to_string();
        let ordinal = col.ordinal();
        let val = column_json_any(row, ordinal).unwrap_or(serde_json::Value::Null);
        obj.insert(name, val);
    }
    serde_json::Value::Object(obj)
}

/// 逐列尝试常见类型，首个成功者转 JSON（Any 侧；u64 不可达——执行点已拒绝）。
fn column_json_any(row: &AnyRow, ordinal: usize) -> Option<serde_json::Value> {
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

impl Client {
    async fn connect(dsn: &str) -> Result<Self, sqlx::Error> {
        let pool = if dsn.starts_with("mysql://") {
            // 生产路径：**typed 驱动**才能精确承载 u64 / `BIGINT UNSIGNED`
            // （sqlx 的 Any 层没有 u64 的 Encode/Decode，读会回绕、写直接编译不过）。
            Pooled::MySql(PoolOptions::<MySql>::new().connect(dsn).await?)
        } else {
            // 离线/测试路径（sqlite DSN 等）：Any 池，保证 CI 无 MySQL 服务时仍有全路径覆盖。
            sqlx::any::install_default_drivers();
            Pooled::Any(PoolOptions::<Any>::new().connect(dsn).await?)
        };
        Ok(Self {
            pool,
            dialect: dialect_of(dsn),
            dsn: dsn.to_string(),
            next_tx: AtomicU64::new(0),
            txs: Mutex::new(HashMap::new()),
            streams: Mutex::new(HashMap::new()),
            next_stream: AtomicU64::new(0),
            stream_gate: Arc::new(tokio::sync::Semaphore::new(STREAM_MAX_CONNS)),
        })
    }

    /// ABI 10：打开流式游标（async-stream 持有自有 SQL/参数/连接——无借用逃逸）。
    /// 方言矩阵：mysql:// DSN → **typed 专用连接**（cancel = KILL QUERY，服务端立即
    /// 中断查询，真取消）；其它（sqlite 等）→ Any 池回退（协作式取消）。
    async fn stream_open(&self, sql: &str, params: &[serde_json::Value]) -> Result<u64, String> {
        let sql = sql.to_string();
        let params: Vec<serde_json::Value> = params.to_vec();
        let is_mysql = matches!(self.pool, Pooled::MySql(_));
        let mut conn_id: Option<u64> = None;
        // typed 专用连接占闸（v0.1.84）：拿到配额才拨号；此后任一步失败配额随局部
        // permit drop 归还。Any 回退路径走池（池自身有界），不占闸。
        let gate = if is_mysql {
            Some(self.stream_gate.clone().try_acquire_owned().map_err(|_| {
                format!(
                    "db stream_open: too many concurrent cursors (limit {STREAM_MAX_CONNS}) \
                             — close or cancel running streams first"
                )
            })?)
        } else {
            None
        };
        let rows: DbStream = if is_mysql {
            // typed 专用连接：连接的所有权随生成器走；cancel = KILL QUERY conn_id
            // （服务端立即中断查询）。sqlx Drop 不通知服务器，cancel 必须显式发。
            let mut conn = MySqlConnection::connect(&self.dsn)
                .await
                .map_err(|e| map_sqlx_err("db stream_open: connect", &e))?;
            conn_id = Some(
                sqlx::query_scalar::<_, u64>("select connection_id()")
                    .fetch_one(&mut conn)
                    .await
                    .map_err(|e| map_sqlx_err("db stream_open: connection id", &e))?,
            );
            Box::pin(async_stream::stream! {
                let mut q: Query<'_, MySql, MySqlArguments> =
                    sqlx::query(sqlx::AssertSqlSafe(sql.as_str()));
                for v in &params {
                    q = bind_value_mysql(q, v);
                }
                use futures::StreamExt;
                let mut rows = q.fetch(&mut conn);
                while let Some(r) = rows.next().await {
                    match r {
                        Ok(row) => yield row_to_json_mysql(&row),
                        Err(e) => {
                            yield Err(format!("db stream: {e}"));
                            break;
                        }
                    }
                }
                // 生成器结束 → conn drop = 断连（回池）。
            })
        } else {
            // Any 层无 u64：明确拒绝（勿让 `$oj$u64` 落成静默文本）。
            oj_plugin_ffi::jsint::reject_u64_markers(
                &params,
                "the sqlx::Any fallback path cannot carry u64 — use a mysql:// DSN with this plugin",
            )?;
            let pool = match &self.pool {
                Pooled::Any(p) => p.clone(),
                _ => unreachable!("is_mysql false ⇒ Any pool"),
            };
            Box::pin(async_stream::stream! {
                let mut q: Query<'_, Any, AnyArguments> =
                    sqlx::query(sqlx::AssertSqlSafe(sql.as_str()));
                for v in &params {
                    q = bind_value_any(q, v);
                }
                use futures::StreamExt;
                let mut rows = q.fetch(&pool);
                while let Some(r) = rows.next().await {
                    match r {
                        Ok(row) => yield Ok(row_to_json_any(&row)),
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
                typed: is_mysql,
                conn_id,
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

    /// 取消：typed（mysql:// DSN）= **移除条目并发 `KILL QUERY` 专用连接**（服务端立即
    /// 中断查询——真取消，幂等：条目已无即 no-op）；Any 回退 = 置标志（协作式，下一批返回
    /// cancelled）。
    fn stream_cancel(&self, stream_id: u64) -> Result<Vec<u8>, String> {
        let cur = self.streams.lock().unwrap().remove(&stream_id);
        if let Some(cur) = cur {
            if cur.typed {
                // 方言级真取消：独立短连接发 KILL QUERY conn_id，服务端立即中断查询
                // （sqlx Drop 不通知服务器——断连要等 keepalive，必须显式取消）。
                if let Some(cid) = cur.conn_id {
                    let dsn = self.dsn.clone();
                    tokio::spawn(async move {
                        if let Ok(mut c) = MySqlConnection::connect(&dsn).await {
                            // cid 来自服务端 connection_id()，非用户输入，可信；
                            // KILL QUERY 不接受绑定参数，故以 AssertSqlSafe 包裹动态串。
                            let _ = sqlx::query(sqlx::AssertSqlSafe(
                                format!("kill query {cid}").as_str(),
                            ))
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
        match &self.pool {
            Pooled::MySql(p) => {
                let mut q: Query<'_, MySql, MySqlArguments> = sqlx::query(sqlx::AssertSqlSafe(sql));
                for v in params {
                    q = bind_value_mysql(q, v);
                }
                let rows = q
                    .fetch_all(p)
                    .await
                    .map_err(|e| map_sqlx_err("db query", &e))?;
                let json: Result<Vec<_>, String> = rows.iter().map(row_to_json_mysql).collect();
                serde_json::to_vec(&json?).map_err(|e| format!("db query serialize: {e}"))
            }
            Pooled::Any(p) => {
                // Any 层无 u64：明确拒绝（勿让 `$oj$u64` 落成静默文本）。
                oj_plugin_ffi::jsint::reject_u64_markers(
                    params,
                    "the sqlx::Any fallback path cannot carry u64 — use a mysql:// DSN with this plugin",
                )?;
                let mut q: Query<'_, Any, AnyArguments> = sqlx::query(sqlx::AssertSqlSafe(sql));
                for v in params {
                    q = bind_value_any(q, v);
                }
                let rows = q
                    .fetch_all(p)
                    .await
                    .map_err(|e| map_sqlx_err("db query", &e))?;
                serde_json::to_vec(&rows.iter().map(row_to_json_any).collect::<Vec<_>>())
                    .map_err(|e| format!("db query serialize: {e}"))
            }
        }
    }

    async fn exec(&self, sql: &str, params: &[serde_json::Value]) -> Result<Vec<u8>, String> {
        match &self.pool {
            Pooled::MySql(p) => {
                let mut q: Query<'_, MySql, MySqlArguments> = sqlx::query(sqlx::AssertSqlSafe(sql));
                for v in params {
                    q = bind_value_mysql(q, v);
                }
                let res = q
                    .execute(p)
                    .await
                    .map_err(|e| map_sqlx_err("db exec", &e))?;
                serde_json::to_vec(&(res.rows_affected() as i64))
                    .map_err(|e| format!("db exec serialize: {e}"))
            }
            Pooled::Any(p) => {
                oj_plugin_ffi::jsint::reject_u64_markers(
                    params,
                    "the sqlx::Any fallback path cannot carry u64 — use a mysql:// DSN with this plugin",
                )?;
                let mut q: Query<'_, Any, AnyArguments> = sqlx::query(sqlx::AssertSqlSafe(sql));
                for v in params {
                    q = bind_value_any(q, v);
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
            Pooled::MySql(p) => TxInner::MySql(tokio::sync::Mutex::new(Some(
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
        match &tx.tx {
            TxInner::MySql(m) => {
                let mut g = m.lock().await;
                let Some(t) = g.as_mut() else {
                    return Err("tx finished".into());
                };
                let mut q: Query<'_, MySql, MySqlArguments> = sqlx::query(sqlx::AssertSqlSafe(sql));
                for v in params {
                    q = bind_value_mysql(q, v);
                }
                let rows = q
                    .fetch_all(&mut **t)
                    .await
                    .map_err(|e| map_sqlx_err("db tx query", &e))?;
                let json: Result<Vec<_>, String> = rows.iter().map(row_to_json_mysql).collect();
                serde_json::to_vec(&json?).map_err(|e| format!("db tx query serialize: {e}"))
            }
            TxInner::Any(m) => {
                let mut g = m.lock().await;
                let Some(t) = g.as_mut() else {
                    return Err("tx finished".into());
                };
                oj_plugin_ffi::jsint::reject_u64_markers(
                    params,
                    "the sqlx::Any fallback path cannot carry u64 — use a mysql:// DSN with this plugin",
                )?;
                let mut q: Query<'_, Any, AnyArguments> = sqlx::query(sqlx::AssertSqlSafe(sql));
                for v in params {
                    q = bind_value_any(q, v);
                }
                let rows = q
                    .fetch_all(&mut **t)
                    .await
                    .map_err(|e| map_sqlx_err("db tx query", &e))?;
                serde_json::to_vec(&rows.iter().map(row_to_json_any).collect::<Vec<_>>())
                    .map_err(|e| format!("db tx query serialize: {e}"))
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
        match &tx.tx {
            TxInner::MySql(m) => {
                let mut g = m.lock().await;
                let Some(t) = g.as_mut() else {
                    return Err("tx finished".into());
                };
                let mut q: Query<'_, MySql, MySqlArguments> = sqlx::query(sqlx::AssertSqlSafe(sql));
                for v in params {
                    q = bind_value_mysql(q, v);
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
                oj_plugin_ffi::jsint::reject_u64_markers(
                    params,
                    "the sqlx::Any fallback path cannot carry u64 — use a mysql:// DSN with this plugin",
                )?;
                let mut q: Query<'_, Any, AnyArguments> = sqlx::query(sqlx::AssertSqlSafe(sql));
                for v in params {
                    q = bind_value_any(q, v);
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
            TxInner::MySql(m) => {
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
            TxInner::MySql(m) => {
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

// ---- vtable（同步签名返回 FfiFuture）----

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
                eprintln!("oj-db-mysql: dialect: {e}");
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
            v.push(RString::from("mysql://"));
            v
        },
        RVec::new(),
    )
}

/// ABI 12：工厂级方言描述符（临时终稿——真实能力声明在 M1 T11 复核映射表后钉死）。
extern "C" fn describe() -> RString {
    RString::from(
        r#"{"dialect":"mysql","capabilities":["placeholder_q","id_two_step","stream","cancel"]}"#,
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
        name: RString::from("db-mysql"),
        semver: RString::from(env!("CARGO_PKG_VERSION")),
        abi_version: ABI_VERSION,
        fingerprint: RString::from(oj_plugin_ffi::HOST_FINGERPRINT),
        desc: RString::from(
            "db 轴 mysql cdylib 插件：sqlx Any 单方言（mysql）迁移自 core SqlxAccessor",
        ),
    }
}

fn init(host: RArc<HostContext>, cfg: RString) -> RResult<PluginDescriptor, RString> {
    // 同进程二次 init（多装配/测试重载同一 dylib）：cfg 以首次为准，直接复用 descriptor。
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
        .expect("oj-db-mysql tokio runtime")
}

oj_plugin_ffi::oj_plugin_entry!(init, db => &VTABLE);

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
