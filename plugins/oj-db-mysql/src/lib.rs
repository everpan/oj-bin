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
use sqlx::mysql::{MySql, MySqlArguments, MySqlRow};
use sqlx::pool::{Pool, PoolOptions};
use sqlx::query::Query;
use sqlx::{Column, Row, TypeInfo};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dialect {
    Sqlite,
    MySql,
    Postgres,
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

/// 单行 MySqlRow -> serde_json::Value（typed 路径）。
///
/// 出现**本插件还不会解码的列类型**时**响亮报错**（`Err`），绝不落成静默 `null`：
/// `sqlx::Any` 时代这类列（`DECIMAL` / 时间 / `JSON` / `BIT` …）会抛 `AnyDriverError`，
/// typed 化后若沿用 `unwrap_or(Null)` 就把「报错」退化成「错值」，踩本仓红线。
fn row_to_json_mysql(row: &MySqlRow) -> Result<serde_json::Value, String> {
    let mut obj = serde_json::Map::new();
    for col in row.columns() {
        let name = col.name().to_string();
        let val = column_json_mysql(row, col.ordinal()).ok_or_else(|| {
            format!(
                "db(mysql): column '{name}' has MySQL type '{}' which this plugin does not \
                 decode yet — select explicit columns and cast it in SQL (e.g. \
                 `cast({name} as char) as {name}`); see docs/numeric-limits.md §4.3",
                col.type_info().name()
            )
        })?;
        obj.insert(name, val);
    }
    Ok(serde_json::Value::Object(obj))
}

/// 逐列按**固定的安全顺序**探测（typed 路径）：
/// `u64 → i64 → bool → f64 → String → Vec<u8>`。
///
/// 顺序为什么是安全的（Any 侧做不到这一点）：
/// - `u64::compatible` 只接受 `BIGINT UNSIGNED`（`sqlx-mysql/src/types/uint.rs`）→ 放最前，
///   `> i64::MAX` 的无符号值**不再回绕成负数**（Any 的 `AnyValueKind::BigInt(i64)` 会）；
/// - `i64` 只接受非 unsigned 整数；
/// - `bool::compatible` 与 `f64::compatible` 都**接受整型**（`types/bool.rs` / `float.rs`），
///   若把它们排在整数之前，任意 BIGINT 列会被当成 bool（值 256 → `false`）——这是必须避免的
///   静默错值，也正是本函数不能沿用「`if let Ok` 盲探测」的原因。
///
/// 两条**能力边界**（真库用例 `real_mysql_unsupported_column_types_error_loudly` 钉住）：
/// - `bool` 分支实际上**不可达**（`bool::compatible` 的整型集合是 `int_compatible` 的子集，
///   而 `i64` 已在前面命中）→ MySQL 的 `BOOLEAN` / `TINYINT(1)` 读出 **1/0（number）**，
///   不是 `true`/`false`。保留该分支是作为顺序被重排时的显式意图标记，勿删；
/// - `String` / `Vec<u8>` 的 `compatible` 只认 `VarChar|*Blob|String|VarString|Enum`
///   （`str` 另需非 BINARY collation）→ **`DECIMAL`/`NEWDECIMAL`/`JSON`/`DATE`/`TIME`/
///   `DATETIME`/`TIMESTAMP`/`YEAR`/`BIT`/`GEOMETRY` 一律走到 `None` → 由调用方报错**。
///   （说明：MySQL 的 `TEXT` 家族在协议里就是 `Blob` + 非 BINARY collation，故可正常读出。）
fn column_json_mysql(row: &MySqlRow, ordinal: usize) -> Option<serde_json::Value> {
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
            Some(b) => serde_json::Value::String(String::from_utf8_lossy(&b).into_owned()),
            None => serde_json::Value::Null,
        });
    }
    None
}

/// 池的二选一（v0.1.24）：**生产 DSN（`mysql://`）走 typed**，其余（离线测试的 sqlite）走 Any。
///
/// `Pooled::Any` 分支是**仅测试用**的（不是受支持的运行时路径）：它只在「非 mysql DSN」时出现，
/// 而生产装配按 scheme 路由——`mysql://` 必进本插件且必走 typed。**Any 路径验证不到生产 MySQL
/// 的行解码**（`Any` 的 `TryFrom<MySqlTypeInfo>` 缺 `Tiny` 等分支，Any+MySQL 组合本就不可用），
/// 故它只覆盖「编排/绑定通路」；MySQL 行形状的回归靠 env-gated 真库用例（见 `CHANGELIST` 的 CI 挂账）。
///
/// 为什么保留 Any：本插件的离线全路径 roundtrip 是 CI 里唯一不依赖 MySQL 服务的覆盖
/// （CI 无 MySQL service，见 `CHANGELIST` 的 P1-3 挂账；typed 化后 sqlite DSN 无法用 `MySql` 池，
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
    next_tx: AtomicU64,
    txs: Mutex<HashMap<u64, Arc<Tx>>>,
}

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
    async fn connect(dsn: &str) -> Result<Self, String> {
        let pool = if dsn.starts_with("mysql://") {
            // 生产路径：**typed 驱动**才能精确承载 u64 / `BIGINT UNSIGNED`
            // （sqlx 的 Any 层没有 u64 的 Encode/Decode，读会回绕、写直接编译不过）。
            Pooled::MySql(
                PoolOptions::<MySql>::new()
                    .connect(dsn)
                    .await
                    .map_err(|e| format!("db connect: {e}"))?,
            )
        } else {
            // 离线/测试路径（sqlite DSN 等）：Any 池，保证 CI 无 MySQL 服务时仍有全路径覆盖。
            sqlx::any::install_default_drivers();
            Pooled::Any(
                PoolOptions::<Any>::new()
                    .connect(dsn)
                    .await
                    .map_err(|e| format!("db connect: {e}"))?,
            )
        };
        Ok(Self {
            pool,
            dialect: dialect_of(dsn),
            next_tx: AtomicU64::new(0),
            txs: Mutex::new(HashMap::new()),
        })
    }

    async fn query(&self, sql: &str, params: &[serde_json::Value]) -> Result<Vec<u8>, String> {
        match &self.pool {
            Pooled::MySql(p) => {
                let mut q: Query<'_, MySql, MySqlArguments> = sqlx::query(sqlx::AssertSqlSafe(sql));
                for v in params {
                    q = bind_value_mysql(q, v);
                }
                let rows = q.fetch_all(p).await.map_err(|e| format!("db query: {e}"))?;
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
                let rows = q.fetch_all(p).await.map_err(|e| format!("db query: {e}"))?;
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
                let res = q.execute(p).await.map_err(|e| format!("db exec: {e}"))?;
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
                let res = q.execute(p).await.map_err(|e| format!("db exec: {e}"))?;
                serde_json::to_vec(&(res.rows_affected() as i64))
                    .map_err(|e| format!("db exec serialize: {e}"))
            }
        }
    }

    async fn begin(&self) -> Result<u64, String> {
        let inner = match &self.pool {
            Pooled::MySql(p) => TxInner::MySql(tokio::sync::Mutex::new(Some(
                p.begin().await.map_err(|e| format!("db tx begin: {e}"))?,
            ))),
            Pooled::Any(p) => TxInner::Any(tokio::sync::Mutex::new(Some(
                p.begin().await.map_err(|e| format!("db tx begin: {e}"))?,
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
                    .map_err(|e| format!("db tx query: {e}"))?;
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
                    .map_err(|e| format!("db tx query: {e}"))?;
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
                    .map_err(|e| format!("db tx exec: {e}"))?;
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
                    .map_err(|e| format!("db tx exec: {e}"))?;
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
                t.commit().await.map_err(|e| format!("db tx commit: {e}"))?;
            }
            TxInner::Any(m) => {
                let Some(t) = m.lock().await.take() else {
                    return Err("tx finished".into());
                };
                t.commit().await.map_err(|e| format!("db tx commit: {e}"))?;
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
                    .map_err(|e| format!("db tx rollback: {e}"))?;
            }
            TxInner::Any(m) => {
                let Some(t) = m.lock().await.take() else {
                    return Err("tx finished".into());
                };
                t.rollback()
                    .await
                    .map_err(|e| format!("db tx rollback: {e}"))?;
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
}

// ---- vtable（同步签名返回 FfiFuture）----

extern "C" fn connect(cfg: RString) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(&st.rt, async move {
            let dsn = cfg[..].to_string();
            let client = Client::connect(&dsn).await?;
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

extern "C" fn dialect(handle: u64) -> RString {
    oj_plugin_ffi::catch_value(
        || {
            let d = state()
                .client(handle)
                .map(|c| c.dialect)
                .unwrap_or(Dialect::Sqlite);
            RString::from(dialect_str(d))
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
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("oj-db-mysql tokio runtime")
}

oj_plugin_ffi::oj_plugin_entry!(init, db => &VTABLE);

#[cfg(test)]
mod tests {
    use super::*;
    use oj_plugin_ffi::RBytes;

    /// DSN 前缀 → 方言判定（占位符补全 / lock 句柄等都依赖它，错判即打错方言 SQL）。
    #[test]
    fn given_dsn_when_dialect_of_then_prefix_decides() {
        assert_eq!(dialect_of("mysql://u:p@h/db"), Dialect::MySql);
        assert_eq!(dialect_of("postgres://u:p@h/db"), Dialect::Postgres);
        assert_eq!(dialect_of("postgresql://u:p@h/db"), Dialect::Postgres);
        assert_eq!(dialect_of("sqlite://f.db"), Dialect::Sqlite);
        assert_eq!(dialect_of("file.db"), Dialect::Sqlite);
        assert_eq!(dialect_of(""), Dialect::Sqlite);
    }

    #[test]
    fn given_dialect_when_str_then_wire_name() {
        assert_eq!(dialect_str(Dialect::Sqlite), "sqlite");
        assert_eq!(dialect_str(Dialect::MySql), "mysql");
        assert_eq!(dialect_str(Dialect::Postgres), "postgres");
    }

    /// 离线 sqlite 全路径 roundtrip（dev 构建统一出 sqlite 驱动，生产 cdylib 仍单方言）：
    /// connect → DDL → 参数化 insert → query 行 JSON 形状 → 事务 rollback 不可见 /
    /// commit 可见 → unknown handle/tx、bad SQL、bad params 各自点名 → dialect() 线名。
    #[tokio::test(flavor = "multi_thread")]
    async fn given_sqlite_dsn_when_full_vtable_roundtrip_then_offline_green() {
        let _ = std::result::Result::from(init(host(), RString::from("{}")));
        let dir = std::env::temp_dir().join(format!("oj-dbmys-off-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Any 默认 create_if_missing=false → 先 touch（0 字节即合法空库）。
        std::fs::File::create(dir.join("t.db")).unwrap();
        // 与宿主同一入口：绝对路径走单冒号 `sqlite:` + 正斜杠（Windows 盘符不会
        // 被 Url 解析吞成 host），见 oj_plugin_ffi::path_util。
        let dsn = oj_plugin_ffi::path_util::sqlite_file_dsn(&dir.join("t.db"));

        let bytes = drive(&mut connect(RString::from(dsn.as_str())))
            .await
            .expect("connect");
        let h = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["handle"]
            .as_u64()
            .unwrap();

        drive(&mut exec(
            h,
            RString::from("create table if not exists t (id integer primary key, v text)"),
            RString::from("[]"),
        ))
        .await
        .expect("ddl");
        drive(&mut exec(
            h,
            RString::from("insert into t (id, v) values (?, ?)"),
            RString::from(r#"[1,"hi"]"#),
        ))
        .await
        .expect("insert");
        let rows = drive(&mut query(
            h,
            RString::from("select id, v from t where id = ?"),
            RString::from("[1]"),
        ))
        .await
        .expect("query");
        let v: serde_json::Value = serde_json::from_slice(&rows).unwrap();
        assert_eq!(v[0]["id"], 1, "{v}");
        assert_eq!(v[0]["v"], "hi", "{v}");

        // 事务：rollback 后不可见。
        let b = drive(&mut begin(h)).await.expect("begin");
        let tx = serde_json::from_slice::<serde_json::Value>(&b).unwrap()["tx_id"]
            .as_u64()
            .unwrap();
        drive(&mut tx_exec(
            h,
            tx,
            RString::from("insert into t (id, v) values (?, ?)"),
            RString::from(r#"[2,"tx"]"#),
        ))
        .await
        .expect("tx insert");
        drive(&mut tx_rollback(h, tx)).await.expect("rollback");
        let rows = drive(&mut query(
            h,
            RString::from("select count(*) as n from t"),
            RString::from("[]"),
        ))
        .await
        .expect("count");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&rows).unwrap()[0]["n"],
            1
        );

        // 事务：tx_query 可见未提交数据 → commit 后全局可见。
        let b = drive(&mut begin(h)).await.expect("begin2");
        let tx = serde_json::from_slice::<serde_json::Value>(&b).unwrap()["tx_id"]
            .as_u64()
            .unwrap();
        drive(&mut tx_exec(
            h,
            tx,
            RString::from("insert into t (id, v) values (?, ?)"),
            RString::from(r#"[3,"tx2"]"#),
        ))
        .await
        .expect("tx insert2");
        let rows = drive(&mut tx_query(
            h,
            tx,
            RString::from("select v from t where id = ?"),
            RString::from("[3]"),
        ))
        .await
        .expect("tx query");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&rows).unwrap()[0]["v"],
            "tx2"
        );
        drive(&mut tx_commit(h, tx)).await.expect("commit");
        let rows = drive(&mut query(
            h,
            RString::from("select count(*) as n from t"),
            RString::from("[]"),
        ))
        .await
        .expect("count2");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&rows).unwrap()[0]["n"],
            2
        );

        // 错误面各自点名。
        let e = drive(&mut query(
            999,
            RString::from("select 1"),
            RString::from("[]"),
        ))
        .await
        .unwrap_err();
        assert!(e.contains("unknown handle"), "{e}");
        let e = drive(&mut tx_commit(h, 777)).await.unwrap_err();
        assert!(e.contains("unknown tx"), "{e}");
        let e = drive(&mut exec(
            h,
            RString::from("definitely not sql"),
            RString::from("[]"),
        ))
        .await
        .unwrap_err();
        assert!(e.contains("db exec"), "{e}");
        let e = drive(&mut query(
            h,
            RString::from("select 1"),
            RString::from("not json"),
        ))
        .await
        .unwrap_err();
        assert!(e.contains("bad params"), "{e}");

        // dialect(): sqlite DSN → 线名 "sqlite"（占位符补全/幂立按键选依赖它）。
        assert_eq!(&dialect(h)[..], "sqlite");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 无效 DSN 快速失败（不触网）：scheme 未知/畸形 URL 在 sqlx 解析期即报错。
    #[tokio::test(flavor = "multi_thread")]
    async fn invalid_dsn_fails_fast() {
        assert!(Client::connect("not a url").await.is_err());
        assert!(Client::connect("").await.is_err());
    }

    /// 真实 mysql 集成（env-gated）：`OJ_TEST_MYSQL=mysql://… cargo test -p oj-db-mysql`。
    /// 未设 env → 打印 skip 直接通过（不进网络）。
    #[tokio::test(flavor = "multi_thread")]
    async fn real_mysql_roundtrip_via_vtable() {
        let Ok(url) = std::env::var("OJ_TEST_MYSQL") else {
            eprintln!("skip: OJ_TEST_MYSQL unset");
            return;
        };
        let cfg = serde_json::json!({}).to_string();
        let desc = match std::result::Result::from(init(host(), RString::from(cfg.as_str()))) {
            Ok(d) => d,
            Err(e) => panic!("init failed: {}", &e[..]),
        };
        assert_eq!(&desc.name[..], "db-mysql");

        let mut c = connect(RString::from(url.as_str()));
        let bytes = drive(&mut c).await.expect("connect");
        let handle: u64 = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["handle"]
            .as_u64()
            .unwrap();

        // 先清场：本用例只 `create if not exists` + 固定 id 插入，残留行会让重跑撞主键。
        drive(&mut exec(
            handle,
            RString::from("drop table if exists oj_plugin_t"),
            RString::from("[]"),
        ))
        .await
        .expect("drop");
        drive(&mut exec(
            handle,
            RString::from("create table if not exists oj_plugin_t (id int primary key, v text)"),
            RString::from("[]"),
        ))
        .await
        .expect("create");

        drive(&mut exec(
            handle,
            RString::from("insert into oj_plugin_t (id, v) values (?, ?)"),
            RString::from(r#"[1,"hi"]"#),
        ))
        .await
        .expect("insert");

        let rows = drive(&mut query(
            handle,
            RString::from("select v from oj_plugin_t where id = ?"),
            RString::from(r#"[1]"#),
        ))
        .await
        .expect("query");
        let v: serde_json::Value = serde_json::from_slice(&rows).unwrap();
        assert_eq!(v[0]["v"], serde_json::json!("hi"), "{v}");

        // 事务 commit
        let bytes = drive(&mut begin(handle)).await.expect("begin");
        let tx_id = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["tx_id"]
            .as_u64()
            .unwrap();
        drive(&mut tx_exec(
            handle,
            tx_id,
            RString::from("insert into oj_plugin_t (id, v) values (?, ?)"),
            RString::from(r#"[2,"tx"]"#),
        ))
        .await
        .expect("tx insert");
        drive(&mut tx_commit(handle, tx_id))
            .await
            .expect("tx commit");

        // 事务 rollback
        let bytes = drive(&mut begin(handle)).await.expect("begin2");
        let tx_id = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["tx_id"]
            .as_u64()
            .unwrap();
        drive(&mut tx_exec(
            handle,
            tx_id,
            RString::from("insert into oj_plugin_t (id, v) values (?, ?)"),
            RString::from(r#"[3,"rb"]"#),
        ))
        .await
        .expect("tx insert2");
        drive(&mut tx_rollback(handle, tx_id))
            .await
            .expect("tx rollback");

        let rows = drive(&mut query(
            handle,
            RString::from("select count(*) c from oj_plugin_t where id in (1,2,3)"),
            RString::from("[]"),
        ))
        .await
        .expect("count");
        let v: serde_json::Value = serde_json::from_slice(&rows).unwrap();
        assert_eq!(
            v[0]["c"],
            serde_json::json!(2),
            "rolled back row must be absent: {v}"
        );

        close(handle);
        drive(&mut query(
            handle,
            RString::from("select 1"),
            RString::from("[]"),
        ))
        .await
        .expect_err("unknown handle after close");
    }

    /// 大整数参数（v0.1.22）：`toBigInt()` 的标记形态 → i64 → bigint 列，精确往返。
    /// env-gated：`OJ_TEST_MYSQL=mysql://…`；未设 env 直接跳过（不进网络）。
    #[tokio::test(flavor = "multi_thread")]
    async fn real_mysql_bigint_marker_binds_i64() {
        let Ok(url) = std::env::var("OJ_TEST_MYSQL") else {
            eprintln!("skip: OJ_TEST_MYSQL unset");
            return;
        };
        let cfg = serde_json::json!({}).to_string();
        let _ = init(host(), RString::from(cfg.as_str()));
        let mut c = connect(RString::from(url.as_str()));
        let bytes = drive(&mut c).await.expect("connect");
        let handle: u64 = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["handle"]
            .as_u64()
            .unwrap();
        let ex = |sql: &str, params: &str| {
            let mut f = exec(handle, RString::from(sql), RString::from(params));
            async move { drive(&mut f).await }
        };
        let qy = |sql: &str, params: &str| {
            let mut f = query(handle, RString::from(sql), RString::from(params));
            async move { drive(&mut f).await }
        };

        ex("drop table if exists oj_bigint_t", "[]").await.unwrap();
        ex(
            "create table oj_bigint_t (id bigint primary key, note text)",
            "[]",
        )
        .await
        .unwrap();

        // 标记参数 → i64 → bigint 主键（雪花量级 + i64::MIN）
        ex(
            "insert into oj_bigint_t (id, note) values (?, ?)",
            r#"[{"$oj$i64":"4886674138783273204"},"marker"]"#,
        )
        .await
        .expect("marker param must bind as i64");
        ex(
            "insert into oj_bigint_t (id, note) values (?, ?)",
            r#"[{"$oj$i64":"-9223372036854775808"},"imin"]"#,
        )
        .await
        .expect("i64::MIN must round-trip");

        let rows = qy("select id, note from oj_bigint_t order by id", "[]")
            .await
            .expect("query");
        let v: serde_json::Value = serde_json::from_slice(&rows).unwrap();
        assert_eq!(
            v[0]["id"],
            serde_json::json!(-9223372036854775808i64),
            "插件行转换必须逐字精确：{v}"
        );
        assert_eq!(v[1]["id"], serde_json::json!(4886674138783273204i64), "{v}");

        // where 用标记参数比较 bigint 列
        let rows = qy(
            "select note from oj_bigint_t where id = ?",
            r#"[{"$oj$i64":"4886674138783273204"}]"#,
        )
        .await
        .expect("marker param must compare against bigint column");
        let v: serde_json::Value = serde_json::from_slice(&rows).unwrap();
        assert_eq!(v[0]["note"], serde_json::json!("marker"), "{v}");

        ex("drop table if exists oj_bigint_t", "[]").await.unwrap();
        close(handle);
    }

    /// u64 / `BIGINT UNSIGNED`（v0.1.24，env-gated）：typed 路径下**读侧不再回绕成负数**、
    /// 写侧能精确绑 `u64::MAX`。这是债务②的验收基线——Any 路径（老实现）会把
    /// `18446744073709551615` 读成 `-1`。
    #[tokio::test(flavor = "multi_thread")]
    async fn real_mysql_unsigned_bigint_roundtrips_as_u64() {
        let Ok(url) = std::env::var("OJ_TEST_MYSQL") else {
            eprintln!("skip: OJ_TEST_MYSQL unset");
            return;
        };
        let cfg = serde_json::json!({}).to_string();
        let _ = init(host(), RString::from(cfg.as_str()));
        let mut c = connect(RString::from(url.as_str()));
        let bytes = drive(&mut c).await.expect("connect");
        let handle: u64 = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["handle"]
            .as_u64()
            .unwrap();
        let ex = |sql: &str, params: &str| {
            let mut f = exec(handle, RString::from(sql), RString::from(params));
            async move { drive(&mut f).await }
        };
        let qy = |sql: &str, params: &str| {
            let mut f = query(handle, RString::from(sql), RString::from(params));
            async move { drive(&mut f).await }
        };

        ex("drop table if exists oj_unsigned_t", "[]")
            .await
            .unwrap();
        ex(
            "create table oj_unsigned_t (id bigint unsigned primary key, note text)",
            "[]",
        )
        .await
        .expect("create");

        // ① u64::MAX 与 i64::MAX+1 都能精确写入（`$oj$u64` 标记 → 绑 u64）。
        for (id, note) in [
            ("18446744073709551615", "umax"),
            ("9223372036854775808", "i64max_plus1"),
        ] {
            ex(
                "insert into oj_unsigned_t (id, note) values (?, ?)",
                &format!(r#"[{{"$oj$u64":"{id}"}},"{note}"]"#),
            )
            .await
            .unwrap_or_else(|e| panic!("insert {id}: {e}"));
        }
        // ② 读回来是**无符号**值：不是负数、也不是字符串（宿主 op 出口才会把 >2^53 降成字符串）。
        let rows = qy("select id, note from oj_unsigned_t order by note", "[]")
            .await
            .expect("select");
        let v: serde_json::Value = serde_json::from_slice(&rows).unwrap();
        assert_eq!(v[1]["id"], serde_json::json!(u64::MAX), "{v}");
        assert_eq!(v[0]["id"], serde_json::json!(9223372036854775808u64), "{v}");
        assert!(
            v[1]["id"].as_i64().is_none(),
            "u64::MAX 绝不能以 i64 形态出现（那意味着回绕成 -1）"
        );

        // ③ i64 范围内的无符号值也精确（走 u64 探测分支）。
        ex(
            "insert into oj_unsigned_t (id, note) values (?, ?)",
            r#"[{"$oj$u64":"42"},"small"]"#,
        )
        .await
        .expect("insert small unsigned");
        let rows = qy(
            "select id from oj_unsigned_t where note = ?",
            r#"["small"]"#,
        )
        .await
        .expect("select small");
        let v: serde_json::Value = serde_json::from_slice(&rows).unwrap();
        assert_eq!(v[0]["id"], serde_json::json!(42), "{v}");

        // ④ 越 u64 的标记必须失败（前端 i64Marker 之后端防线）。
        let e = ex(
            "insert into oj_unsigned_t (id) values (?)",
            r#"[{"$oj$u64":"18446744073709551616"}]"#,
        )
        .await
        .expect_err("越 u64 必须失败");
        assert!(e.contains("db") || e.contains("value"), "{e}");

        ex("drop table if exists oj_unsigned_t", "[]")
            .await
            .unwrap();
        close(handle);
    }

    /// 列类型能力边界（v0.1.24，env-gated；修开发侧评审 P0）：**不会解码的列类型必须响亮报错，
    /// 不能静默 null**。
    ///
    /// 背景：`sqlx::Any` 时代这类列（`DECIMAL`/时间/`JSON`/`BIT`…）在列转换就抛
    /// `AnyDriverError`（`sqlx-mysql/src/any.rs` 的 `TryFrom<&MySqlTypeInfo>` 只认
    /// Null/Short/Long/LongLong/Float/Double + str/bytes 可兼容者）。typed 化后若沿用
    /// `unwrap_or(Null)`，就把「报错」退化成「错值」——踩本仓「静默错值不可接受」的红线。
    ///
    /// 同时钉住**可读**的边界：`TEXT` 家族（协议里是 `Blob` + 非 BINARY collation）与
    /// `BOOLEAN`（= `TINYINT(1)`，按整数读成 1/0，不是 true/false）。
    #[tokio::test(flavor = "multi_thread")]
    async fn real_mysql_unsupported_column_types_error_loudly() {
        let Ok(url) = std::env::var("OJ_TEST_MYSQL") else {
            eprintln!("skip: OJ_TEST_MYSQL unset");
            return;
        };
        let cfg = serde_json::json!({}).to_string();
        let _ = init(host(), RString::from(cfg.as_str()));
        let mut c = connect(RString::from(url.as_str()));
        let bytes = drive(&mut c).await.expect("connect");
        let handle: u64 = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["handle"]
            .as_u64()
            .unwrap();
        let ex = |sql: &str, params: &str| {
            let mut f = exec(handle, RString::from(sql), RString::from(params));
            async move { drive(&mut f).await }
        };
        let qy = |sql: &str, params: &str| {
            let mut f = query(handle, RString::from(sql), RString::from(params));
            async move { drive(&mut f).await }
        };

        ex("drop table if exists oj_types_t", "[]").await.unwrap();
        ex(
            "create table oj_types_t (id int primary key, amount decimal(20,4), \
             made_at datetime, payload json, note text, flag boolean, ub bigint unsigned)",
            "[]",
        )
        .await
        .expect("create");
        ex(
            "insert into oj_types_t values (1, 12.3456, '2026-09-19 10:00:00', \
             '{\"a\":1}', 'hi', true, 18446744073709551615)",
            "[]",
        )
        .await
        .expect("insert");

        // ① 支持的列可读：整数 / TEXT / BIGINT UNSIGNED（u64）。
        let rows = qy(
            "select id, note, ub, cast(amount as char) as amount_txt from oj_types_t",
            "[]",
        )
        .await
        .expect("supported columns must read");
        let v: serde_json::Value = serde_json::from_slice(&rows).unwrap();
        assert_eq!(v[0]["id"], serde_json::json!(1), "{v}");
        assert_eq!(v[0]["note"], serde_json::json!("hi"), "{v}");
        assert_eq!(v[0]["ub"], serde_json::json!(u64::MAX), "{v}");
        assert_eq!(v[0]["amount_txt"], serde_json::json!("12.3456"), "{v}");

        // ② BOOLEAN 走整数读（1/0）——记录能力边界，不是 true/false。
        let rows = qy("select flag from oj_types_t", "[]")
            .await
            .expect("boolean reads as integer");
        let v: serde_json::Value = serde_json::from_slice(&rows).unwrap();
        assert_eq!(v[0]["flag"], serde_json::json!(1), "BOOLEAN 边界：{v}");

        // ③ 未支持的类型必须**报错**（点名列 + 类型），绝不静默 null。
        for (sql, needle) in [
            ("select amount from oj_types_t", "DECIMAL"),
            ("select made_at from oj_types_t", "DATETIME"),
            ("select payload from oj_types_t", "JSON"),
        ] {
            let e = qy(sql, "[]")
                .await
                .err()
                .unwrap_or_else(|| panic!("{sql} 必须报错而不是返回静默 null"));
            assert!(
                e.contains(needle),
                "{sql}: 错误信息应点名 MySQL 类型 {needle}，实得 {e}"
            );
        }

        ex("drop table if exists oj_types_t", "[]").await.unwrap();
        close(handle);
    }

    /// 并发驱动多个 FfiFuture（FfiFuture 非 Send）：在单个任务里轮询全部——调度已在插件
    /// runtime 上并行开始，故这就是真并发。（与 pg 插件同款；两插件按 spec 允许复制。）
    async fn drive_all(futs: &mut [FfiFuture]) -> Vec<Result<Vec<u8>, String>> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut out: Vec<Option<Result<Vec<u8>, String>>> = (0..futs.len()).map(|_| None).collect();
        let mut left = futs.len();
        while left > 0 {
            for (i, fut) in futs.iter_mut().enumerate() {
                if out[i].is_some() {
                    continue;
                }
                if (fut.poll)(fut.state) == 0 {
                    if std::time::Instant::now() >= deadline {
                        for slot in out.iter_mut() {
                            if slot.is_none() {
                                *slot = Some(Err("ffi drive timeout".into()));
                            }
                        }
                        left = 0;
                        break;
                    }
                    continue;
                }
                let r = (fut.take)(fut.state);
                (fut.free)(fut.state);
                fut.state = std::ptr::null_mut();
                out[i] = Some(match (1, std::result::Result::from(r)) {
                    (_, Err(e)) => Err(e[..].to_string()),
                    (_, Ok(b)) => Ok(b.iter().copied().collect()),
                });
                left -= 1;
            }
            if left > 0 {
                tokio::time::sleep(std::time::Duration::from_micros(200)).await;
            }
        }
        out.into_iter().flatten().collect()
    }

    /// 债④回归（env-gated，MySQL 侧）：`db.nextSeq` 的两条语句在真库上正确且原子。
    ///
    /// MySQL 没有 `RETURNING`，平台用 `insert … values (?, last_insert_id(1)) on duplicate key
    /// update v = last_insert_id(v + 1)` + `select last_insert_id()`（**必须同一连接**，宿主在
    /// 短事务里跑）。这里分两段验证：
    /// ① 顺序 3 次（同一连接）→ 1、2、3（验证 `last_insert_id` 惯用法的**首次插入/已存在**两态）；
    /// ② 12 个并发语句 → 最终 v 恰为 12（**原子性**：无丢失更新；`select max(id)+1` 会丢）。
    #[tokio::test(flavor = "multi_thread")]
    async fn real_mysql_next_seq_is_atomic_under_concurrency() {
        let Ok(url) = std::env::var("OJ_TEST_MYSQL") else {
            eprintln!("skip: OJ_TEST_MYSQL unset");
            return;
        };
        let cfg = serde_json::json!({}).to_string();
        let _ = init(host(), RString::from(cfg.as_str()));
        let mut c = connect(RString::from(url.as_str()));
        let bytes = drive(&mut c).await.expect("connect");
        let handle: u64 = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["handle"]
            .as_u64()
            .unwrap();
        let ex = |sql: &str, params: &str| {
            let mut f = exec(handle, RString::from(sql), RString::from(params));
            async move { drive(&mut f).await }
        };

        ex("drop table if exists _oj_sequences", "[]")
            .await
            .unwrap();
        ex(
            "create table _oj_sequences (name varchar(128) primary key, v bigint not null)",
            "[]",
        )
        .await
        .expect("create seq table");

        let bump = "insert into _oj_sequences (name, v) values (?, last_insert_id(1)) \
                    on duplicate key update v = last_insert_id(v + 1)";
        let read = "select last_insert_id() as v";

        // ① 顺序 3 次（同一连接 = 同一个 tx）：1 → 2 → 3。
        for expect in 1..=3 {
            let b = drive(&mut begin(handle)).await.expect("begin");
            let tx = serde_json::from_slice::<serde_json::Value>(&b).unwrap()["tx_id"]
                .as_u64()
                .unwrap();
            drive(&mut tx_exec(
                handle,
                tx,
                RString::from(bump),
                RString::from(r#"["seq_a"]"#),
            ))
            .await
            .expect("bump in tx");
            let rows = drive(&mut tx_query(
                handle,
                tx,
                RString::from(read),
                RString::from("[]"),
            ))
            .await
            .expect("read last_insert_id");
            drive(&mut tx_commit(handle, tx)).await.expect("commit");
            let v: serde_json::Value = serde_json::from_slice(&rows).unwrap();
            assert_eq!(
                v[0]["v"],
                serde_json::json!(expect),
                "顺序取号应为 {expect}：{v}"
            );
        }

        // ② 12 个并发语句（各自独立连接/语句）→ 最终值恰为 12。
        ex(
            "insert into _oj_sequences (name, v) values (?, 0)",
            r#"["seq_b"]"#,
        )
        .await
        .expect("seed seq_b");
        let mut futs: Vec<FfiFuture> = (0..12)
            .map(|_| exec(handle, RString::from(bump), RString::from(r#"["seq_b"]"#)))
            .collect();
        for o in drive_all(&mut futs).await {
            o.expect("并发 bump 应成功");
        }
        let rows = drive(&mut query(
            handle,
            RString::from("select v from _oj_sequences where name = ?"),
            RString::from(r#"["seq_b"]"#),
        ))
        .await
        .expect("read final");
        let v: serde_json::Value = serde_json::from_slice(&rows).unwrap();
        assert_eq!(
            v[0]["v"],
            serde_json::json!(12),
            "12 次并发 self-increment 不得丢更新：{v}"
        );

        ex("drop table if exists _oj_sequences", "[]")
            .await
            .unwrap();
        close(handle);
    }

    extern "C" fn test_log(_level: u8, _msg: RString) {}
    extern "C" fn test_deliver(_topic: RString, _payload: RBytes) {}

    fn host() -> RArc<HostContext> {
        RArc::new(HostContext {
            log: test_log,
            deliver: test_deliver,
        })
    }

    /// FfiFuture → 测试异步桥（等价 core await_ffi 的 poll 轮询）。
    async fn drive(fut: &mut FfiFuture) -> Result<Vec<u8>, String> {
        // 以真实墙钟时间为界轮询（同 oj-es 的 drive）：固定 10w 次 yield_now 在 CI
        // 负载/优化下会在插件 rt 的任务完成前耗尽预算，误报 "ffi drive timeout"。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            match (fut.poll)(fut.state) {
                0 => {
                    if std::time::Instant::now() >= deadline {
                        (fut.free)(fut.state); // 超时也要释放 state（防 FfiTask 泄漏）
                        fut.state = std::ptr::null_mut();
                        return Err("ffi drive timeout".into());
                    }
                    tokio::time::sleep(std::time::Duration::from_micros(100)).await;
                }
                code => {
                    let r = (fut.take)(fut.state);
                    (fut.free)(fut.state);
                    fut.state = std::ptr::null_mut();
                    return match (code, std::result::Result::from(r)) {
                        (1, Ok(b)) => Ok(b.iter().copied().collect()),
                        (_, Err(e)) => Err(e[..].to_string()),
                        _ => Err("ffi poll reported error but take succeeded".into()),
                    };
                }
            }
        }
    }
}
