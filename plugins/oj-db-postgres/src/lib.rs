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
use sqlx::pool::{Pool, PoolOptions};
use sqlx::postgres::{PgArguments, PgConnection, PgRow, Postgres};
use sqlx::query::Query;
use sqlx::{Column, Row};
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
    pool: Pool<Any>,
    /// 原始 DSN（PR-2 真取消：流式游标按方言单开专用 typed 连接用）。
    dsn: String,
    dialect: Dialect,
    next_tx: AtomicU64,
    txs: Mutex<HashMap<u64, Arc<Tx>>>,
    /// ABI 10 流式游标（stream_id → 行流 + 协作式取消标志）。
    streams: Mutex<HashMap<u64, Arc<StreamHandle>>>,
    next_stream: AtomicU64,
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
}

/// 每次 stream_next 的批量行数上限（spec §1.4：砍逐行 FFI 往返）。
const STREAM_BATCH: usize = 100;

struct Tx {
    tx: tokio::sync::Mutex<Option<sqlx::Transaction<'static, Any>>>,
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

fn row_to_json(row: &AnyRow) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    for col in row.columns() {
        let name = col.name().to_string();
        let ordinal = col.ordinal();
        let val = column_json(row, ordinal).unwrap_or(serde_json::Value::Null);
        obj.insert(name, val);
    }
    serde_json::Value::Object(obj)
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

/// typed PG 版行转换（镜像 `row_to_json` 的 try 链；未知类型兜底字符串化）。
fn row_to_json_pg(row: &PgRow) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    for col in row.columns() {
        let name = col.name().to_string();
        let ordinal = col.ordinal();
        let val = column_json_pg(row, ordinal).unwrap_or(serde_json::Value::Null);
        obj.insert(name, val);
    }
    serde_json::Value::Object(obj)
}

fn column_json_pg(row: &PgRow, ordinal: usize) -> Option<serde_json::Value> {
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
        sqlx::any::install_default_drivers();
        let pool = PoolOptions::<Any>::new()
            .connect(&with_stmt_cache_capacity(dsn, 512))
            .await
            .map_err(|e| format!("db connect: {e}"))?;
        Ok(Self {
            pool,
            dsn: dsn.to_string(),
            dialect: dialect_of(dsn),
            next_tx: AtomicU64::new(0),
            txs: Mutex::new(HashMap::new()),
            streams: Mutex::new(HashMap::new()),
            next_stream: AtomicU64::new(0),
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
        let rows: DbStream = if is_pg {
            // typed 专用连接：连接的所有权随生成器走；cancel = pg_cancel_backend(pid)
            // （服务端立即中断查询）。sqlx Drop 不通知服务器，cancel 必须显式发。
            let mut conn = PgConnection::connect(&self.dsn)
                .await
                .map_err(|e| format!("db stream_open: connect: {e}"))?;
            backend_pid = Some(
                sqlx::query_scalar::<_, i32>("select pg_backend_pid()")
                    .fetch_one(&mut conn)
                    .await
                    .map_err(|e| format!("db stream_open: backend pid: {e}"))?,
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
                        Ok(row) => yield Ok(row_to_json_pg(&row)),
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
            let pool = self.pool.clone();
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
                        Ok(row) => yield Ok(row_to_json(&row)),
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
        let mut q: Query<'_, Any, AnyArguments> = sqlx::query(sqlx::AssertSqlSafe(sql));
        for p in params {
            q = bind_value(q, p);
        }
        let rows = q
            .fetch_all(&self.pool)
            .await
            .map_err(|e| format!("db query: {e}"))?;
        serde_json::to_vec(&rows.iter().map(row_to_json).collect::<Vec<_>>())
            .map_err(|e| format!("db query serialize: {e}"))
    }

    async fn exec(&self, sql: &str, params: &[serde_json::Value]) -> Result<Vec<u8>, String> {
        oj_plugin_ffi::jsint::reject_u64_markers(
            params,
            "postgres bigint is i64 — store it as text, or use a MySQL BIGINT UNSIGNED column",
        )?;
        let sql = shape_tag(sql, params);
        let mut q: Query<'_, Any, AnyArguments> = sqlx::query(sqlx::AssertSqlSafe(sql));
        for p in params {
            q = bind_value(q, p);
        }
        let res = q
            .execute(&self.pool)
            .await
            .map_err(|e| format!("db exec: {e}"))?;
        serde_json::to_vec(&(res.rows_affected() as i64))
            .map_err(|e| format!("db exec serialize: {e}"))
    }

    async fn begin(&self) -> Result<u64, String> {
        let tx = self
            .pool
            .begin()
            .await
            .map_err(|e| format!("db tx begin: {e}"))?;
        let id = self.next_tx.fetch_add(1, Ordering::SeqCst) + 1;
        self.txs.lock().unwrap().insert(
            id,
            Arc::new(Tx {
                tx: tokio::sync::Mutex::new(Some(tx)),
            }),
        );
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
        let mut g = tx.tx.lock().await;
        let Some(t) = g.as_mut() else {
            return Err("tx finished".into());
        };
        oj_plugin_ffi::jsint::reject_u64_markers(
            params,
            "postgres bigint is i64 — store it as text, or use a MySQL BIGINT UNSIGNED column",
        )?;
        let sql = shape_tag(sql, params);
        let mut q: Query<'_, Any, AnyArguments> = sqlx::query(sqlx::AssertSqlSafe(sql));
        for p in params {
            q = bind_value(q, p);
        }
        let rows = q
            .fetch_all(&mut **t)
            .await
            .map_err(|e| format!("db tx query: {e}"))?;
        serde_json::to_vec(&rows.iter().map(row_to_json).collect::<Vec<_>>())
            .map_err(|e| format!("db tx query serialize: {e}"))
    }

    async fn tx_exec(
        &self,
        tx_id: u64,
        sql: &str,
        params: &[serde_json::Value],
    ) -> Result<Vec<u8>, String> {
        let tx = self.tx(tx_id)?;
        let mut g = tx.tx.lock().await;
        let Some(t) = g.as_mut() else {
            return Err("tx finished".into());
        };
        oj_plugin_ffi::jsint::reject_u64_markers(
            params,
            "postgres bigint is i64 — store it as text, or use a MySQL BIGINT UNSIGNED column",
        )?;
        let sql = shape_tag(sql, params);
        let mut q: Query<'_, Any, AnyArguments> = sqlx::query(sqlx::AssertSqlSafe(sql));
        for p in params {
            q = bind_value(q, p);
        }
        let res = q
            .execute(&mut **t)
            .await
            .map_err(|e| format!("db tx exec: {e}"))?;
        serde_json::to_vec(&(res.rows_affected() as i64))
            .map_err(|e| format!("db tx exec serialize: {e}"))
    }

    async fn tx_commit(&self, tx_id: u64) -> Result<Vec<u8>, String> {
        let tx = self
            .txs
            .lock()
            .unwrap()
            .remove(&tx_id)
            .ok_or_else(|| format!("db: unknown tx {tx_id}"))?;
        let Some(t) = tx.tx.lock().await.take() else {
            return Err("tx finished".into());
        };
        t.commit().await.map_err(|e| format!("db tx commit: {e}"))?;
        Ok(b"".to_vec())
    }

    async fn tx_rollback(&self, tx_id: u64) -> Result<Vec<u8>, String> {
        let tx = self
            .txs
            .lock()
            .unwrap()
            .remove(&tx_id)
            .ok_or_else(|| format!("db: unknown tx {tx_id}"))?;
        let Some(t) = tx.tx.lock().await.take() else {
            return Err("tx finished".into());
        };
        t.rollback()
            .await
            .map_err(|e| format!("db tx rollback: {e}"))?;
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
            v.push(RString::from("postgres://"));
            v.push(RString::from("postgresql://"));
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
    stream_open,
    stream_next,
    stream_cancel,
    stream_close,
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
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("oj-db-postgres tokio runtime")
}

oj_plugin_ffi::oj_plugin_entry!(init, db => &VTABLE);

#[cfg(test)]
mod tests {
    use super::*;
    use oj_plugin_ffi::RBytes;

    /// DSN 前缀 → 方言判定（占位符补全 / lock 句柄等都依赖它，错判即打错方言 SQL）。
    #[test]
    fn given_dsn_when_dialect_of_then_prefix_decides() {
        assert_eq!(dialect_of("postgres://u:p@h/db"), Dialect::Postgres);
        assert_eq!(dialect_of("postgresql://u:p@h/db"), Dialect::Postgres);
        assert_eq!(dialect_of("mysql://u:p@h/db"), Dialect::MySql);
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
        let dir = std::env::temp_dir().join(format!("oj-dbpg-off-{}", std::process::id()));
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

    /// 无效 DSN 快速失败（不触网）。
    #[tokio::test(flavor = "multi_thread")]
    async fn invalid_dsn_fails_fast() {
        assert!(Client::connect("not a url").await.is_err());
        assert!(Client::connect("").await.is_err());
    }

    /// ABI 10 流式全路径（离线 sqlite）：open → 批量 next（≤100 行/批）→ done 就地
    /// 移除条目 → 再 next 报 unknown stream；cancel → 下批 cancelled；close 幂等。
    #[tokio::test(flavor = "multi_thread")]
    async fn given_sqlite_dsn_when_stream_roundtrip_then_batched_rows_done_and_cancel() {
        let _ = std::result::Result::from(init(host(), RString::from("{}")));
        let dir = std::env::temp_dir().join(format!("oj-dbpg-stream-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::File::create(dir.join("t.db")).unwrap();
        let dsn = oj_plugin_ffi::path_util::sqlite_file_dsn(&dir.join("t.db"));

        let bytes = drive(&mut connect(RString::from(dsn.as_str())))
            .await
            .expect("connect");
        let h = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["handle"]
            .as_u64()
            .unwrap();

        drive(&mut exec(
            h,
            RString::from("create table oj_stream_t (id integer primary key, v text)"),
            RString::from("[]"),
        ))
        .await
        .expect("ddl");
        for i in 0..5 {
            drive(&mut exec(
                h,
                RString::from("insert into oj_stream_t (id, v) values (?, ?)"),
                RString::from(format!(r#"[{i},"v{i}"]"#).as_str()),
            ))
            .await
            .expect("insert");
        }

        // open → 第一批拉完 5 行（< 100 不含 done）
        let bytes = drive(&mut stream_open(
            h,
            RString::from("select id, v from oj_stream_t order by id"),
            RString::from("[]"),
        ))
        .await
        .expect("stream_open");
        let sid = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["stream_id"]
            .as_u64()
            .unwrap();
        let bytes = drive(&mut stream_next(h, sid)).await.expect("next1");
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let rows = v["rows"].as_array().expect("rows envelope");
        assert_eq!(rows.len(), 5, "{v}");
        assert_eq!(rows[0]["id"], 0, "{v}");
        assert_eq!(rows[4]["v"], "v4", "{v}");

        // 第二批 = done（条目就地移除）
        let bytes = drive(&mut stream_next(h, sid)).await.expect("next2");
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["done"], serde_json::json!(true), "{v}");

        // done 后再 next → unknown stream（点名，不静默）
        let e = drive(&mut stream_next(h, sid)).await.unwrap_err();
        assert!(e.contains("unknown stream"), "{e}");

        // cancel/close 对已移除条目幂等 Ok
        drive(&mut stream_cancel(h, sid)).await.expect("cancel");
        drive(&mut stream_close(h, sid)).await.expect("close");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ABI 10 取消路径（离线）：cancel 置标志 → 下一次 next 返回 {"error":"cancelled"}；
    /// close 收口后条目消失。
    #[tokio::test(flavor = "multi_thread")]
    async fn given_sqlite_dsn_when_stream_cancelled_then_next_returns_cancelled_envelope() {
        let _ = std::result::Result::from(init(host(), RString::from("{}")));
        let dir = std::env::temp_dir().join(format!("oj-dbpg-cancel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::File::create(dir.join("t.db")).unwrap();
        let dsn = oj_plugin_ffi::path_util::sqlite_file_dsn(&dir.join("t.db"));

        let bytes = drive(&mut connect(RString::from(dsn.as_str())))
            .await
            .expect("connect");
        let h = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["handle"]
            .as_u64()
            .unwrap();
        drive(&mut exec(
            h,
            RString::from("create table oj_cancel_t (id integer primary key)"),
            RString::from("[]"),
        ))
        .await
        .expect("ddl");
        for i in 0..3 {
            drive(&mut exec(
                h,
                RString::from("insert into oj_cancel_t (id) values (?)"),
                RString::from(format!("[{i}]").as_str()),
            ))
            .await
            .expect("insert");
        }

        let bytes = drive(&mut stream_open(
            h,
            RString::from("select id from oj_cancel_t order by id"),
            RString::from("[]"),
        ))
        .await
        .expect("stream_open");
        let sid = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["stream_id"]
            .as_u64()
            .unwrap();

        drive(&mut stream_cancel(h, sid)).await.expect("cancel");
        let bytes = drive(&mut stream_next(h, sid)).await.expect("next");
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"], "cancelled", "{v}");

        drive(&mut stream_close(h, sid)).await.expect("close");
        let e = drive(&mut stream_next(h, sid)).await.unwrap_err();
        assert!(e.contains("unknown stream"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// PR-2 真取消验收（env-gated PG）：慢查询游标 cancel → 专用连接断开 →
    /// `pg_stat_activity` 中的服务端查询秒级消失（≤5s，验收②），随后池 query 正常。
    #[tokio::test(flavor = "multi_thread")]
    async fn real_pg_stream_cancel_kills_server_query() {
        let Ok(dsn) = std::env::var("OJ_TEST_PG") else {
            eprintln!("skip: OJ_TEST_PG unset");
            return;
        };
        let _ = std::result::Result::from(init(host(), RString::from("{}")));
        let bytes = drive(&mut connect(RString::from(dsn.as_str())))
            .await
            .expect("connect");
        let h = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["handle"]
            .as_u64()
            .unwrap();
        // 第二个 client 轮询 pg_stat_activity（独立连接，不被慢查询阻塞）。
        let pbytes = drive(&mut connect(RString::from(dsn.as_str())))
            .await
            .expect("poll connect");
        let ph = serde_json::from_slice::<serde_json::Value>(&pbytes).unwrap()["handle"]
            .as_u64()
            .unwrap();

        let slow = "/*oj-cancel-test*/ select i, pg_sleep(0.02) from generate_series(1,1000) g(i)";
        let bytes = drive(&mut stream_open(
            h,
            RString::from(slow),
            RString::from("[]"),
        ))
        .await
        .expect("stream_open");
        let sid = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["stream_id"]
            .as_u64()
            .unwrap();
        // 取一批（批内已在服务端跑起来）
        drive(&mut stream_next(h, sid)).await.expect("next1");

        let poll = |ph: u64, detail: bool| async move {
            let sql = if detail {
                "select coalesce(state,'?') as state, coalesce(wait_event_type,'-') as wet, coalesce(wait_event,'-') as we from pg_stat_activity where query like '%oj-cancel-test%' and pid <> pg_backend_pid()"
            } else {
                "select count(*)::int as n from pg_stat_activity where query like '%oj-cancel-test%' and pid <> pg_backend_pid()"
            };
            let mut fut = query(ph, RString::from(sql), RString::from("[]"));
            drive(&mut fut).await
        };
        // cancel 前：服务端有该查询
        let pre = poll(ph, false).await.expect("poll pre");
        let pre_n = serde_json::from_slice::<serde_json::Value>(&pre).unwrap()[0]["n"]
            .as_i64()
            .unwrap();
        assert!(pre_n >= 1, "cancel 前服务端应有慢查询在跑: {pre_n}");

        // cancel → 专用连接断开 → 服务端查询终止
        drive(&mut stream_cancel(h, sid)).await.expect("cancel");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut cleared = false;
        while std::time::Instant::now() < deadline {
            let b = poll(ph, true).await.expect("poll");
            eprintln!(
                "DBG poll rows={:?}",
                serde_json::from_slice::<serde_json::Value>(&b).unwrap()
            );
            if serde_json::from_slice::<serde_json::Value>(&b)
                .unwrap()
                .as_array()
                .map(|a| a.is_empty())
                .unwrap_or(false)
            {
                cleared = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        assert!(cleared, "5s 内 pg_stat_activity 必须无孤儿查询（真取消）");
        // 池仍正常
        let b = drive(&mut query(
            h,
            RString::from("select 1 as ok"),
            RString::from("[]"),
        ))
        .await
        .expect("post-cancel query");
        assert!(
            serde_json::from_slice::<serde_json::Value>(&b).unwrap()[0]["ok"] == 1,
            "cancel 后池路径须正常"
        );
        close(h);
        close(ph);
    }

    /// 真实 postgres 集成（env-gated）：`OJ_TEST_PG=postgres://… cargo test -p oj-db-postgres`。
    #[tokio::test(flavor = "multi_thread")]
    async fn real_postgres_roundtrip_via_vtable() {
        let Ok(url) = std::env::var("OJ_TEST_PG") else {
            eprintln!("skip: OJ_TEST_PG unset");
            return;
        };
        let cfg = serde_json::json!({}).to_string();
        let desc = match std::result::Result::from(init(host(), RString::from(cfg.as_str()))) {
            Ok(d) => d,
            Err(e) => panic!("init failed: {}", &e[..]),
        };
        assert_eq!(&desc.name[..], "db-postgres");

        let mut c = connect(RString::from(url.as_str()));
        let bytes = drive(&mut c).await.expect("connect");
        let handle: u64 = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["handle"]
            .as_u64()
            .unwrap();

        // 先清场：本用例只 `create if not exists` + 固定 id 插入，若上一次运行的残留行还在，
        // 重跑会撞主键（PK 冲突）——env-gated 用例必须可重复执行。
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
            RString::from("insert into oj_plugin_t (id, v) values ($1, $2)"),
            RString::from(r#"[1,"hi"]"#),
        ))
        .await
        .expect("insert");

        let rows = drive(&mut query(
            handle,
            RString::from("select v from oj_plugin_t where id = $1"),
            RString::from(r#"[1]"#),
        ))
        .await
        .expect("query");
        let v: serde_json::Value = serde_json::from_slice(&rows).unwrap();
        assert_eq!(v[0]["v"], serde_json::json!("hi"), "{v}");

        let bytes = drive(&mut begin(handle)).await.expect("begin");
        let tx_id = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["tx_id"]
            .as_u64()
            .unwrap();
        drive(&mut tx_exec(
            handle,
            tx_id,
            RString::from("insert into oj_plugin_t (id, v) values ($1, $2)"),
            RString::from(r#"[2,"tx"]"#),
        ))
        .await
        .expect("tx insert");
        drive(&mut tx_commit(handle, tx_id))
            .await
            .expect("tx commit");

        let bytes = drive(&mut begin(handle)).await.expect("begin2");
        let tx_id = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["tx_id"]
            .as_u64()
            .unwrap();
        drive(&mut tx_exec(
            handle,
            tx_id,
            RString::from("insert into oj_plugin_t (id, v) values ($1, $2)"),
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

    /// 大整数参数（v0.1.22）：`toBigInt()` 的标记形态必须能写进 bigint 列并精确读回；
    /// 且**字符串参数写不进 bigint 列**（PG 严格类型）——这是"字符串=文本意图、
    /// BigInt=整数意图"契约的真库依据。env-gated：`OJ_TEST_PG=postgres://…`。
    #[tokio::test(flavor = "multi_thread")]
    async fn real_postgres_bigint_marker_binds_i64() {
        let Ok(url) = std::env::var("OJ_TEST_PG") else {
            eprintln!("skip: OJ_TEST_PG unset");
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

        // ① 标记参数（toBigInt 的编解码形态）→ 精确落入 bigint 主键
        ex(
            "insert into oj_bigint_t (id, note) values ($1, $2)",
            r#"[{"$oj$i64":"4886674138783273204"},"marker"]"#,
        )
        .await
        .expect("marker param must bind as i64");
        ex(
            "insert into oj_bigint_t (id, note) values ($1, $2)",
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

        // ② where 用标记参数比较 bigint 列（字符串形式在 PG 上是 operator does not exist）
        let rows = qy(
            "select note from oj_bigint_t where id = $1",
            r#"[{"$oj$i64":"4886674138783273204"}]"#,
        )
        .await
        .expect("marker param must compare against bigint column");
        let v: serde_json::Value = serde_json::from_slice(&rows).unwrap();
        assert_eq!(v[0]["note"], serde_json::json!("marker"), "{v}");

        // ③ 反例固化：字符串参数写 bigint 列在 PG 上必然失败（防未来"顺手加启发式"）。
        // 用**独立 SQL 文本**取全新 prepared statement，避免命中 sqlx 语句缓存里
        // 「同文本换参数 Rust 类型」的既有不一致（该问题另有登记，见 numeric-limits 手册）。
        let e = ex(
            "insert into oj_bigint_t (id) values ($1)",
            r#"["9007199254740993"]"#,
        )
        .await
        .expect_err("PG 必须拒绝 text → bigint");
        assert!(
            e.contains("bigint") && e.contains("text"),
            "报错应指明类型不匹配：{e}"
        );

        // ④ 事务路径同款（tx_query/tx_exec 共用一个 bind_value）
        let bytes = drive(&mut begin(handle)).await.expect("begin");
        let tx_id = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["tx_id"]
            .as_u64()
            .unwrap();
        drive(&mut tx_exec(
            handle,
            tx_id,
            RString::from("insert into oj_bigint_t (id, note) values ($1, $2)"),
            RString::from(r#"[{"$oj$i64":"9007199254740993"},"tx"]"#),
        ))
        .await
        .expect("tx marker param");
        drive(&mut tx_commit(handle, tx_id)).await.expect("commit");
        let rows = qy(
            "select note from oj_bigint_t where id = $1",
            r#"[{"$oj$i64":"9007199254740993"}]"#,
        )
        .await
        .expect("tx read");
        let v: serde_json::Value = serde_json::from_slice(&rows).unwrap();
        assert_eq!(v[0]["note"], serde_json::json!("tx"), "{v}");

        ex("drop table if exists oj_bigint_t", "[]").await.unwrap();
        close(handle);
    }

    /// 债①回归（env-gated）：**同一条 SQL 文本**在不同调用里绑不同 Rust 类型的参数。
    ///
    /// 根因：sqlx 的语句缓存只以 SQL 文本为 key（不含参数类型），PG 命中缓存时直接复用旧的
    /// `(StatementId, param OIDs)`，Bind 却按本次 Rust 类型编码字节 → 协议级错误
    /// （`insufficient data left in message` / `invalid byte sequence … 0x00`）。
    ///
    /// 期望终态（v0.1.24 起宿主在 PG 方言下给 SQL 前置形态签名 `/*oj:<形态>*/`，不同参数形态
    /// 落到不同缓存条目）：**两个形态都能跑通**。修前本用例必红——它是债①的验收基线。
    ///
    /// 用 **text 列**做混形态载体：`insert … (k, v) values ($1, $2)` 传 `("a","x")` 与 `(1,2)`
    /// 在 PG 上都合法（int8 → text 有赋值转换），差别只在参数形态——正是缓存 bug 的触发面。
    /// bigint 列不能当载体：字符串参数写 bigint 列本来就会被 PG 拒（那是另一条既有契约，
    /// 见 `real_postgres_bigint_marker_binds_i64` ③）。
    #[tokio::test(flavor = "multi_thread")]
    async fn real_postgres_same_sql_text_mixed_param_shapes() {
        let Ok(url) = std::env::var("OJ_TEST_PG") else {
            eprintln!("skip: OJ_TEST_PG unset");
            return;
        };
        let cfg = serde_json::json!({}).to_string();
        let desc = match std::result::Result::from(init(host(), RString::from(cfg.as_str()))) {
            Ok(d) => d,
            Err(e) => panic!("init failed: {}", &e[..]),
        };
        assert_eq!(&desc.name[..], "db-postgres");

        let mut c = connect(RString::from(url.as_str()));
        let bytes = drive(&mut c).await.expect("connect");
        let handle: u64 = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["handle"]
            .as_u64()
            .unwrap();

        drive(&mut exec(
            handle,
            RString::from("drop table if exists oj_shape_t"),
            RString::from("[]"),
        ))
        .await
        .expect("drop");
        drive(&mut exec(
            handle,
            RString::from("create table if not exists oj_shape_t (k text primary key, v text)"),
            RString::from("[]"),
        ))
        .await
        .expect("create");

        // ---- 池路径：同一文本两种形态（t,t）→（i,i）----
        let ins = "insert into oj_shape_t (k, v) values ($1, $2)";
        drive(&mut exec(
            handle,
            RString::from(ins),
            RString::from(r#"["a","x"]"#),
        ))
        .await
        .expect("池路径 text 形态（首次会 PARSE 并缓存该文本）");
        drive(&mut exec(
            handle,
            RString::from("insert into oj_shape_t (k, v) values ($1, $2)"),
            RString::from(r#"[1,2]"#),
        ))
        .await
        .expect("池路径 i64 形态（修前这里报 insufficient data left in message）");

        // ---- tx 路径：同一文本反序再来一次（i,i → t,t），并在 tx 内数缓存条目 ----
        // tx 独占一条连接，故 `pg_prepared_statements` 的计数是确定的（池路径会分散到多连接）。
        let bytes = drive(&mut begin(handle)).await.expect("begin");
        let tx_id = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["tx_id"]
            .as_u64()
            .unwrap();
        drive(&mut tx_exec(
            handle,
            tx_id,
            RString::from("insert into oj_shape_t (k, v) values ($1, $2)"),
            RString::from(r#"[3,4]"#),
        ))
        .await
        .expect("tx 路径 i64 形态");
        drive(&mut tx_exec(
            handle,
            tx_id,
            RString::from("insert into oj_shape_t (k, v) values ($1, $2)"),
            RString::from(r#"["b","y"]"#),
        ))
        .await
        .expect("tx 路径 text 形态（与上一条同文本、不同形态）");
        // 同一文本的三种形态 = 三个缓存条目（形态分键生效），且都带 `/*oj:` 前缀。
        let rows = drive(&mut tx_query(
            handle,
            tx_id,
            RString::from(
                "select count(*)::int as n from pg_prepared_statements \
                 where statement like '/*oj:%' and statement like '%oj_shape_t%'",
            ),
            RString::from("[]"),
        ))
        .await
        .expect("count prepared");
        let v: serde_json::Value = serde_json::from_slice(&rows).unwrap();
        assert!(
            v[0]["n"].as_i64().unwrap_or(0) >= 2,
            "同一条 SQL 文本的不同参数形态必须各占一个缓存条目（形态分键）：{v}"
        );
        drive(&mut tx_commit(handle, tx_id)).await.expect("commit");

        // ---- 结果正确性：四个形态写进去的行都在 ----
        let rows = drive(&mut query(
            handle,
            RString::from("select count(*)::int as n from oj_shape_t"),
            RString::from("[]"),
        ))
        .await
        .expect("count rows");
        let v: serde_json::Value = serde_json::from_slice(&rows).unwrap();
        assert_eq!(v[0]["n"], serde_json::json!(4), "四种形态都应落库：{v}");

        drive(&mut exec(
            handle,
            RString::from("drop table if exists oj_shape_t"),
            RString::from("[]"),
        ))
        .await
        .expect("cleanup");
        close(handle);
    }

    /// 并发驱动多个 FfiFuture（FfiFuture 非 Send，不能 `tokio::spawn`）：在单个任务里
    /// 轮询全部——**调度已在插件 runtime 上并行开始**，故这就是真并发。
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

    /// 债④回归（env-gated）：平台序列的单语句原子取号在**真库并发**下无重号、无空洞。
    ///
    /// 走的正是 `db.nextSeq` 在 PG 上的那条 SQL（`on conflict … do update … returning v`）；
    /// 20 个调用在插件 runtime 上并行发起，断言排序后恰为 1..20——`select max(id)+1` 做不到。
    #[tokio::test(flavor = "multi_thread")]
    async fn real_postgres_next_seq_is_atomic_under_concurrency() {
        let Ok(url) = std::env::var("OJ_TEST_PG") else {
            eprintln!("skip: OJ_TEST_PG unset");
            return;
        };
        let cfg = serde_json::json!({}).to_string();
        let _ = std::result::Result::from(init(host(), RString::from(cfg.as_str())));
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
            "create table _oj_sequences (name text primary key, v bigint not null)",
            "[]",
        )
        .await
        .expect("create seq table");

        let sql = "insert into _oj_sequences (name, v) values ($1, 1) \
                   on conflict (name) do update set v = _oj_sequences.v + 1 returning v";
        let mut futs: Vec<FfiFuture> = (0..20)
            .map(|_| query(handle, RString::from(sql), RString::from(r#"["conc"]"#)))
            .collect();
        let outs = drive_all(&mut futs).await;
        let mut vs: Vec<i64> = Vec::new();
        for o in outs {
            let rows = o.expect("并发取号应成功");
            let v: serde_json::Value = serde_json::from_slice(&rows).unwrap();
            let n = v[0]["v"].as_i64().expect("v 应是整数");
            vs.push(n);
        }
        vs.sort_unstable();
        assert_eq!(vs, (1..=20).collect::<Vec<i64>>(), "并发取号必须稠密无重复");

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
