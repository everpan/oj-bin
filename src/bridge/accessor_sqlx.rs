//! SqlxAccessor：以 sqlx（Any 驱动，driver-agnostic）实现 DataAccessor。
//!
//! 与 sea-query 构造器（query.rs）协同：构造器产出参数化 SQL + `Vec<Value>` 参数，
//! 本实现把 `Value` 绑定到 sqlx 语句，并把结果行转回 `serde_json::Value`，匹配 Value 边界。
//! 真实 handler 无需感知底层驱动——`db.query_with_params` / `db.table(...)` 统一经此路径。

use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::Stream;
use serde_json::{Map, Value};
use sqlx::any::{Any, AnyArguments};
use sqlx::pool::{Pool, PoolOptions};
use sqlx::query::Query;
use sqlx::{Column, Row, TypeInfo};

use super::db::{Dialect, dialect_of};
use super::db_error::{BridgeError, DbErrorCategory, SessionDisposition};
use super::dialect_caps::{DialectCaps, DialectInfo, IdStrategy, PlaceholderStyle};
use super::{BridgeResult, DataAccessor, Row as JsRow};
use crate::secret::redact;

/// 基于 sqlx AnyPool 的 DataAccessor 实现。
pub struct SqlxAccessor {
    pool: Pool<Any>,
    dialect: Dialect,
    /// 方言描述符（T6：两层模型；镜像当前 SqlxAccessor 行为，随真实实现推进）。
    dialect_info: DialectInfo,
}

impl SqlxAccessor {
    /// 从连接串构池（sqlite:///path、postgres://..、mysql://..）。
    /// Any 驱动须先安装（幂等，调用方无感知）；sqlite 每连接独立库（尤其 `:memory:`），
    /// 单连接写锁语义对齐 `SetMaxOpenConns(1)`。
    pub async fn connect(url: &str) -> BridgeResult<Self> {
        sqlx::any::install_default_drivers();
        let mut opts = PoolOptions::<Any>::new();
        if url.starts_with("sqlite") {
            opts = opts.max_connections(1);
        }
        let pool = opts
            .connect(url)
            .await
            .map_err(|e| sqlite_err(&e, "connect"))?;
        let dialect = dialect_of(url);
        Ok(Self {
            pool,
            dialect,
            dialect_info: Self::dialect_info_for(dialect),
        })
    }

    /// 便捷构造 Arc 句柄。
    pub async fn arc(url: &str) -> BridgeResult<Arc<dyn DataAccessor>> {
        Ok(Arc::new(Self::connect(url).await?) as Arc<dyn DataAccessor>)
    }

    /// SqlxAccessor 的方言描述符（T6）：镜像当前 sqlx 行为——
    /// 占位 `?`（sqlite/mysql）/ `$n`（postgres），取 ID 两步（sqlite/mysql）/ RETURNING（postgres），
    /// 支持流式（stream_query 已实现），无 cancel（sqlx Any 无语句级取消）。
    fn dialect_info_for(d: Dialect) -> DialectInfo {
        let (placeholder, id) = match d {
            Dialect::Postgres => (PlaceholderStyle::Dollar, IdStrategy::Returning),
            _ => (PlaceholderStyle::Q, IdStrategy::TwoStep),
        };
        DialectInfo {
            name: d.as_str().to_string(),
            caps: DialectCaps {
                placeholder,
                id,
                stream: true,
                cancel: false,
            },
        }
    }
}

/// 将单个 JSON 值绑定到 sqlx 语句（按类型选择可 Encode 的具体类型）。
fn bind_value<'q>(q: Query<'q, Any, AnyArguments>, v: &Value) -> Query<'q, Any, AnyArguments> {
    // 大整数标记（v0.1.22，`toBigInt()` 的返回值）：绑 i64。必须在对象分支之前——
    // 否则会被 `other => to_string()` 串化成文本（PG 拒绝 text → bigint）。
    if let Some(i) = oj_plugin_ffi::jsint::marker_i64(v) {
        return q.bind(i);
    }
    match v {
        Value::Null => q.bind(None::<String>),
        Value::Bool(b) => q.bind(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                q.bind(i)
            } else if let Some(f) = n.as_f64() {
                q.bind(f)
            } else {
                q.bind(None::<String>)
            }
        }
        Value::String(s) => q.bind(s.clone()),
        other => q.bind(other.to_string()),
    }
}

/// sqlite 错误 → 结构化信封（U-8，v0.1.93）。
///
/// 与 FFI 面同形（`DbErrorCategory` 八值 + `retryable` + `sessionDisposition`）。
/// 关键纪律：**凭据脱敏**——sqlx 错误含 DSN（含凭据），`message` 必须过 `redact`。
/// 映射按 SQLite 主错误码：5/6=BUSY/LOCKED（连接级、可重试、Reset）；19=CONSTRAINT；
/// 错误文本脱敏：**只有文本里真的出现凭据 URL**（`scheme://…@…`）时才替换凭据段。
///
/// 不能直接对任意错误文本调 [`redact`]：它对「不含 `://` 的串」一律返回 `***`（那是给
/// 裸密码用的语义），于是所有 sqlite 报错都退化成 "***"，排障没有任何信息（旧缺陷）。
fn redact_msg(s: &str) -> String {
    if s.contains("://") {
        redact(s)
    } else {
        s.to_string()
    }
}

/// 1=SQLITE_ERROR（语法）；Io/PoolTimedOut/WorkerCrashed=连接；其余=Internal。
/// REQ-11 G3：推入相位事件（elapsed 以 `shared.start` 为基准）。
pub(crate) fn push_event(
    shared: &super::db::PoolExecShared,
    phase: &'static str,
    rows: Option<u64>,
) {
    let elapsed_ms = shared.start.elapsed().as_millis() as u64;
    shared
        .events
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(super::db::CallEvent {
            phase,
            elapsed_ms,
            rows,
        });
}

fn sqlite_err(e: &sqlx::Error, ctx: &str) -> BridgeError {
    use DbErrorCategory as C;
    let (category, retryable, session_disposition) = match e {
        sqlx::Error::Io(_) => (C::Connection, false, SessionDisposition::Keep),
        sqlx::Error::PoolTimedOut | sqlx::Error::WorkerCrashed => {
            (C::Connection, true, SessionDisposition::Reset)
        }
        sqlx::Error::RowNotFound => (C::Internal, false, SessionDisposition::Keep),
        sqlx::Error::Database(d) => match d.code().and_then(|c| c.parse::<i32>().ok()) {
            Some(5) | Some(6) => (C::Connection, true, SessionDisposition::Reset),
            Some(19) => (C::Constraint, false, SessionDisposition::Keep),
            Some(1) => (C::Syntax, false, SessionDisposition::Keep),
            _ => (C::Internal, false, SessionDisposition::Keep),
        },
        _ => (C::Internal, false, SessionDisposition::Keep),
    };
    let message = redact_msg(&format!("sqlx {ctx}: {e}"));
    BridgeError {
        category,
        retryable,
        session_disposition,
        message,
        data: Map::new(),
    }
}

/// 单行 AnyRow -> serde_json::Value（逐列按类型探测）。
///
/// 返回 `Result`：未知列类型**显式报错**，绝不静默丢值为 `Null`（U-6 / 对抗用例）。
fn row_to_json(row: &sqlx::any::AnyRow) -> Result<Value, String> {
    let mut obj = serde_json::Map::with_capacity(row.columns().len());
    for col in row.columns() {
        let name = col.name().to_string();
        let ordinal = col.ordinal();
        let val = column_json(row, ordinal)?;
        obj.insert(name, val);
    }
    Ok(Value::Object(obj))
}

/// 逐列尝试常见类型，首个成功者转 JSON。
///
/// 未知类型（sqlite 动态类型下极罕见）→ 显式 `Err`，由 `row_to_json` 上抛，不静默丢值。
fn column_json(row: &sqlx::any::AnyRow, ordinal: usize) -> Result<Value, String> {
    if let Ok(v) = row.try_get::<Option<bool>, _>(ordinal) {
        return Ok(Value::from(v));
    }
    if let Ok(v) = row.try_get::<Option<i64>, _>(ordinal) {
        return Ok(match v {
            Some(i) => Value::from(i),
            None => Value::Null,
        });
    }
    if let Ok(v) = row.try_get::<Option<f64>, _>(ordinal) {
        return Ok(match v {
            Some(f) => serde_json::Number::from_f64(f)
                .map(Value::Number)
                .unwrap_or(Value::Null),
            None => Value::Null,
        });
    }
    // pg REAL / mysql FLOAT = float4：sqlx 里是 f64 的独立 TypeInfo，f64 臂探不到。
    if let Ok(v) = row.try_get::<Option<f32>, _>(ordinal) {
        return Ok(match v {
            Some(f) => serde_json::Number::from_f64(f as f64)
                .map(Value::Number)
                .unwrap_or(Value::Null),
            None => Value::Null,
        });
    }
    if let Ok(v) = row.try_get::<Option<String>, _>(ordinal) {
        return Ok(match v {
            Some(s) => Value::String(s),
            None => Value::Null,
        });
    }
    if let Ok(v) = row.try_get::<Option<Vec<u8>>, _>(ordinal) {
        return Ok(match v {
            Some(b) => Value::String(String::from_utf8_lossy(&b).into_owned()),
            None => Value::Null,
        });
    }
    let ty = row.column(ordinal).type_info().name().to_string();
    Err(format!(
        "unsupported sqlite column type: {ty} (column ordinal {ordinal})"
    ))
}

/// sqlx 事务会话：`Pool::begin` 产 `Transaction<'static, Any>`；Mutex 串行并发 op，
/// Option 被 take 后（已完结）再调用报 "tx finished"。
struct SqlxTx {
    tx: tokio::sync::Mutex<Option<sqlx::Transaction<'static, Any>>>,
    /// 事务级方言描述符（T6）：begin 时从 accessor 快照。缺失会让 in-tx 路径落回默认
    /// sqlite 描述符——cancel 报错会点名错误方言，占位符查表也可能错。
    info: DialectInfo,
}

#[async_trait]
impl super::db::TxSession for SqlxTx {
    async fn query(&self, sql: &str, params: &[Value]) -> BridgeResult<Vec<JsRow>> {
        let mut g = self.tx.lock().await;
        let Some(tx) = g.as_mut() else {
            return Err("tx finished".into());
        };
        // core 走 sqlx Any：Any 层根本没有 u64 的 Encode → `$oj$u64`（toUBigInt）在这里绑不了，
        // 明确拒绝（勿静默坍缩成 f64；MySQL 的 u64 支持只在 oj-db-mysql 插件里）。
        oj_plugin_ffi::jsint::reject_u64_markers(
            params,
            "the core accessor binds through sqlx::Any, which cannot carry u64 — store it as text",
        )?;
        let mut q: Query<'_, Any, AnyArguments> = sqlx::query(sqlx::AssertSqlSafe(sql));
        for p in params {
            q = bind_value(q, p);
        }
        let rows = q
            .fetch_all(&mut **tx)
            .await
            .map_err(|e| sqlite_err(&e, "tx query"))?;
        Ok(rows
            .iter()
            .map(row_to_json)
            .collect::<Result<Vec<_>, _>>()?)
    }

    async fn exec(&self, sql: &str, params: &[Value]) -> BridgeResult<i64> {
        let mut g = self.tx.lock().await;
        let Some(tx) = g.as_mut() else {
            return Err("tx finished".into());
        };
        oj_plugin_ffi::jsint::reject_u64_markers(
            params,
            "the core accessor binds through sqlx::Any, which cannot carry u64 — store it as text",
        )?;
        let mut q: Query<'_, Any, AnyArguments> = sqlx::query(sqlx::AssertSqlSafe(sql));
        for p in params {
            q = bind_value(q, p);
        }
        let res = q
            .execute(&mut **tx)
            .await
            .map_err(|e| sqlite_err(&e, "tx exec"))?;
        Ok(res.rows_affected() as i64)
    }

    async fn commit(&self) -> BridgeResult<()> {
        let Some(tx) = self.tx.lock().await.take() else {
            return Err("tx finished".into());
        };
        tx.commit().await.map_err(|e| sqlite_err(&e, "tx commit"))?;
        Ok(())
    }

    async fn rollback(&self) -> BridgeResult<()> {
        let Some(tx) = self.tx.lock().await.take() else {
            return Err("tx finished".into());
        };
        tx.rollback()
            .await
            .map_err(|e| sqlite_err(&e, "tx rollback"))?;
        Ok(())
    }

    fn dialect_info(&self) -> &DialectInfo {
        &self.info
    }
}

#[async_trait]
impl DataAccessor for SqlxAccessor {
    fn dialect(&self) -> Dialect {
        self.dialect
    }

    fn dialect_info(&self) -> &DialectInfo {
        &self.dialect_info
    }

    async fn begin(&self) -> BridgeResult<Box<dyn super::db::TxSession>> {
        let tx = self
            .pool
            .begin()
            .await
            .map_err(|e| sqlite_err(&e, "tx begin"))?;
        Ok(Box::new(SqlxTx {
            tx: tokio::sync::Mutex::new(Some(tx)),
            info: self.dialect_info.clone(),
        }))
    }

    fn server_cancel_on_abandon(&self) -> bool {
        // REQ-11（v0.1.114）：池路径改显式连接 + 流被弃/取消时 close_on_drop 真关
        // TCP——服务端语句即死（pg_sleep/sleep 类不再跑完），三方言同口径 true。
        // 插件轴仍 false（宿主弃 FfiFuture 只 free state，真取消 = ABI 13 车）。
        true
    }

    async fn query_with_params(&self, sql: &str, params: &[Value]) -> BridgeResult<Vec<JsRow>> {
        oj_plugin_ffi::jsint::reject_u64_markers(
            params,
            "the core accessor binds through sqlx::Any, which cannot carry u64 — store it as text",
        )?;
        let mut q: Query<'_, Any, AnyArguments> = sqlx::query(sqlx::AssertSqlSafe(sql));
        for p in params {
            q = bind_value(q, p);
        }
        let rows = q
            .fetch_all(&self.pool)
            .await
            .map_err(|e| sqlite_err(&e, "query"))?;
        Ok(rows
            .iter()
            .map(row_to_json)
            .collect::<Result<Vec<_>, _>>()?)
    }

    /// REQ-05：边拉边计、触顶早停（真 OOM 防线；区别于默认实现的「拉完再计」）。
    /// 把结果推过上限的那一行计入 `meter.counted` 但不入结果集（fail/truncate 两档
    /// 都不交付该行）；fetch 流被 drop 后 sqlx 回收连接时关闭而非复用（协议态不可知）。
    async fn query_with_params_metered(
        &self,
        sql: &str,
        params: &[Value],
        meter: &mut super::db_quota::ByteMeter,
    ) -> BridgeResult<Vec<JsRow>> {
        oj_plugin_ffi::jsint::reject_u64_markers(
            params,
            "the core accessor binds through sqlx::Any, which cannot carry u64 — store it as text",
        )?;
        let mut q: Query<'_, Any, AnyArguments> = sqlx::query(sqlx::AssertSqlSafe(sql));
        for p in params {
            q = bind_value(q, p);
        }
        let mut stream = q.fetch(&self.pool);
        let mut out = Vec::new();
        use futures::StreamExt;
        while let Some(r) = stream.next().await {
            let v = row_to_json(&r.map_err(|e| sqlite_err(&e, "query"))?)?;
            if meter.feed(&v) == super::db_quota::Feed::Stop {
                break;
            }
            out.push(v);
        }
        Ok(out)
    }

    /// REQ-11 G1/G2：可取消查询。显式 acquire（持有 `PoolConnection` 才能
    /// `close_on_drop`）；cancel 命中 = drop 在飞 fetch 流 + `close_on_drop()` 真关
    /// TCP——服务端语句即死、连接报废绝不回池（修正案 #2 机制）。相位事件：
    /// sent（提交后）/fetching（首行）/done（settle 前，带最终行数）。
    async fn query_cancelable(
        &self,
        sql: &str,
        params: &[Value],
        meter: &mut super::db_quota::ByteMeter,
        shared: &super::db::PoolExecShared,
    ) -> BridgeResult<Vec<JsRow>> {
        use std::sync::atomic::Ordering;
        oj_plugin_ffi::jsint::reject_u64_markers(
            params,
            "the core accessor binds through sqlx::Any, which cannot carry u64 — store it as text",
        )?;
        // 开跑前 cancel 竞态：语句不执行，连接不碰
        if shared.cancel_requested.load(Ordering::SeqCst) {
            return Err(super::db_error::cancelled_error(&shared.alias).into());
        }
        let mut conn = self
            .pool
            .acquire()
            .await
            .map_err(|e| sqlite_err(&e, "query acquire"))?;
        push_event(shared, "sent", None);
        let mut q: Query<'_, Any, AnyArguments> = sqlx::query(sqlx::AssertSqlSafe(sql));
        for p in params {
            q = bind_value(q, p);
        }
        let mut stream = q.fetch(&mut *conn);
        let mut out = Vec::new();
        let mut first = true;
        use futures::StreamExt;
        loop {
            // Final-fix F2：sticky 标志循环头复查。`notify_waiters` 对每个 Notified
            // future 单发——行臂同刻赢一次 select 即吞掉唤醒（假取消）；标志先于
            // notify 置位，循环头必见，cancel 绝不丢失。
            if shared.cancel_requested.load(Ordering::SeqCst) {
                drop(stream); // 在飞 fetch 报废（协议态不可知）
                conn.close_on_drop(); // 真关 TCP；绝不回池
                return Err(super::db_error::cancelled_error(&shared.alias).into());
            }
            tokio::select! {
                row = stream.next() => {
                    let Some(r) = row else { break };
                    let v = row_to_json(&r.map_err(|e| sqlite_err(&e, "query"))?)?;
                    if first {
                        push_event(shared, "fetching", None);
                        first = false;
                    }
                    if meter.feed(&v) == super::db_quota::Feed::Stop {
                        break;
                    }
                    out.push(v);
                }
                _ = shared.notify.notified() => {
                    // fire_cancel 先置标志后 notify——非取消路径不 notify，故此处
                    // 命中即取消意图（保守再校验标志）。
                    if shared.cancel_requested.load(Ordering::SeqCst) {
                        drop(stream); // 在飞 fetch 报废（协议态不可知）
                        conn.close_on_drop(); // 真关 TCP；绝不回池
                        return Err(super::db_error::cancelled_error(&shared.alias).into());
                    }
                }
            }
        }
        push_event(shared, "done", Some(out.len() as u64));
        Ok(out)
    }

    /// REQ-11 G1/G2：可取消执行（`query_cancelable` 的无行版，结果 = affected）。
    async fn exec_cancelable(
        &self,
        sql: &str,
        params: &[Value],
        shared: &super::db::PoolExecShared,
    ) -> BridgeResult<i64> {
        use std::sync::atomic::Ordering;
        oj_plugin_ffi::jsint::reject_u64_markers(
            params,
            "the core accessor binds through sqlx::Any, which cannot carry u64 — store it as text",
        )?;
        if shared.cancel_requested.load(Ordering::SeqCst) {
            return Err(super::db_error::cancelled_error(&shared.alias).into());
        }
        let mut conn = self
            .pool
            .acquire()
            .await
            .map_err(|e| sqlite_err(&e, "exec acquire"))?;
        push_event(shared, "sent", None);
        let mut q: Query<'_, Any, AnyArguments> = sqlx::query(sqlx::AssertSqlSafe(sql));
        for p in params {
            q = bind_value(q, p);
        }
        tokio::select! {
            res = q.execute(&mut *conn) => {
                let r = res.map_err(|e| sqlite_err(&e, "exec"))?;
                // Final-fix F2：execute 臂赢的当刻 cancel 同到（单发 Notified 被吞）
                // ——成功返回前复查 sticky 标志，按取消收口（语句服务端已执行的事实
                // 由调用方按取消语义吞掉，同 query 臂 drop 在流行口径）。
                if shared.cancel_requested.load(Ordering::SeqCst) {
                    conn.close_on_drop();
                    return Err(super::db_error::cancelled_error(&shared.alias).into());
                }
                push_event(shared, "done", None);
                Ok(r.rows_affected() as i64)
            }
            _ = shared.notify.notified() => {
                // execute future 已被 select 取消（协议态不可知）——无论标志与否，
                // 报废连接并按取消口径返回（当前仅 fire_cancel 会 notify）。
                conn.close_on_drop();
                Err(super::db_error::cancelled_error(&shared.alias).into())
            }
        }
    }

    async fn exec_with_params(&self, sql: &str, params: &[Value]) -> BridgeResult<i64> {
        oj_plugin_ffi::jsint::reject_u64_markers(
            params,
            "the core accessor binds through sqlx::Any, which cannot carry u64 — store it as text",
        )?;
        let mut q: Query<'_, Any, AnyArguments> = sqlx::query(sqlx::AssertSqlSafe(sql));
        for p in params {
            q = bind_value(q, p);
        }
        let res = q
            .execute(&self.pool)
            .await
            .map_err(|e| sqlite_err(&e, "exec"))?;
        Ok(res.rows_affected() as i64)
    }

    async fn stream_query(
        &self,
        sql: &str,
        params: &[Value],
    ) -> BridgeResult<Pin<Box<dyn Stream<Item = Result<JsRow, String>> + Send>>> {
        oj_plugin_ffi::jsint::reject_u64_markers(
            params,
            "the core accessor binds through sqlx::Any, which cannot carry u64 — store it as text",
        )?;
        // try_stream! 把自有 sql/params/pool 收进生成器状态机，规避 sqlx Query 借用 &str
        // 无法返回 'static 装箱流的问题（borrow 仅在生成器内部，不外逃）。
        let sql = sql.to_string();
        let params = params.to_vec();
        let pool = self.pool.clone();
        // stream! 直接 yield Result<Value,String>；borrow 仅存在于生成器内部状态机，不外逃。
        let stream = async_stream::stream! {
            let mut q: Query<'_, Any, AnyArguments> = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()));
            for p in &params {
                q = bind_value(q, p);
            }
            let mut rows = q.fetch(&pool);
            use futures::StreamExt;
            while let Some(r) = rows.next().await {
                match r {
                    Ok(row) => match row_to_json(&row) {
                        Ok(v) => yield Ok(v),
                        // 解码错误（如未知列类型）显式上报，不静默丢值。
                        Err(e) => {
                            yield Err(e);
                            break;
                        }
                    },
                    // 流内错误脱敏（sqlx 错误可能含 DSN/凭据），保持与 query 同纪律。
                    Err(e) => {
                        yield Err(redact_msg(&e.to_string()));
                        break;
                    }
                }
            }
        };
        Ok(Box::pin(stream))
    }
}

#[cfg(test)]
#[path = "accessor_sqlx_tests.rs"]
mod tests;
