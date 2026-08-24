//! SqlxAccessor：以 sqlx（Any 驱动，driver-agnostic）实现 DataAccessor。
//!
//! 与 sea-query 构造器（query.rs）协同：构造器产出参数化 SQL + `Vec<Value>` 参数，
//! 本实现把 `Value` 绑定到 sqlx 语句，并把结果行转回 `serde_json::Value`，匹配 Value 边界。
//! 真实 handler 无需感知底层驱动——`db.query_with_params` / `db.table(...)` 统一经此路径。

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use sqlx::any::{Any, AnyArguments};
use sqlx::pool::{Pool, PoolOptions};
use sqlx::query::Query;
use sqlx::{Column, Row};

use super::db::{Dialect, dialect_of};
use super::{BridgeResult, DataAccessor, Row as JsRow};

/// 基于 sqlx AnyPool 的 DataAccessor 实现。
pub struct SqlxAccessor {
    pool: Pool<Any>,
    dialect: Dialect,
}

impl SqlxAccessor {
    /// 从连接串构池（sqlite:///path、postgres://..、mysql://..）。
    /// Any 驱动须先安装（幂等，调用方无感知）；sqlite 每连接独立库（尤其 `:memory:`），
    /// 单连接亦对齐 Go 的 `SetMaxOpenConns(1)` 写锁语义。
    pub async fn connect(url: &str) -> BridgeResult<Self> {
        sqlx::any::install_default_drivers();
        let mut opts = PoolOptions::<Any>::new();
        if url.starts_with("sqlite") {
            opts = opts.max_connections(1);
        }
        let pool = opts
            .connect(url)
            .await
            .map_err(|e| format!("sqlx connect: {e}"))?;
        Ok(Self { pool, dialect: dialect_of(url) })
    }

    /// 便捷构造 Arc 句柄。
    pub async fn arc(url: &str) -> BridgeResult<Arc<dyn DataAccessor>> {
        Ok(Arc::new(Self::connect(url).await?) as Arc<dyn DataAccessor>)
    }
}

/// 将单个 JSON 值绑定到 sqlx 语句（按类型选择可 Encode 的具体类型）。
fn bind_value<'q>(q: Query<'q, Any, AnyArguments>, v: &Value) -> Query<'q, Any, AnyArguments> {
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

/// 单行 AnyRow -> serde_json::Value（逐列按类型探测）。
fn row_to_json(row: &sqlx::any::AnyRow) -> Value {
    let mut obj = serde_json::Map::new();
    for col in row.columns() {
        let name = col.name().to_string();
        let ordinal = col.ordinal();
        let val = column_json(row, ordinal).unwrap_or(Value::Null);
        obj.insert(name, val);
    }
    Value::Object(obj)
}

/// 逐列尝试常见类型，首个成功者转 JSON。
fn column_json(row: &sqlx::any::AnyRow, ordinal: usize) -> Option<Value> {
    if let Ok(v) = row.try_get::<Option<bool>, _>(ordinal) {
        return Some(Value::from(v));
    }
    if let Ok(v) = row.try_get::<Option<i64>, _>(ordinal) {
        return Some(match v {
            Some(i) => Value::from(i),
            None => Value::Null,
        });
    }
    if let Ok(v) = row.try_get::<Option<f64>, _>(ordinal) {
        return Some(match v {
            Some(f) => serde_json::Number::from_f64(f).map(Value::Number).unwrap_or(Value::Null),
            None => Value::Null,
        });
    }
    if let Ok(v) = row.try_get::<Option<String>, _>(ordinal) {
        return Some(match v {
            Some(s) => Value::String(s),
            None => Value::Null,
        });
    }
    if let Ok(v) = row.try_get::<Option<Vec<u8>>, _>(ordinal) {
        return Some(match v {
            Some(b) => Value::String(String::from_utf8_lossy(&b).into_owned()),
            None => Value::Null,
        });
    }
    None
}

/// sqlx 事务会话：`Pool::begin` 产 `Transaction<'static, Any>`；Mutex 串行并发 op，
/// Option 被 take 后（已完结）再调用报 "tx finished"。
struct SqlxTx {
    tx: tokio::sync::Mutex<Option<sqlx::Transaction<'static, Any>>>,
}

#[async_trait]
impl super::db::TxSession for SqlxTx {
    async fn query(&self, sql: &str, params: &[Value]) -> BridgeResult<Vec<JsRow>> {
        let mut g = self.tx.lock().await;
        let Some(tx) = g.as_mut() else {
            return Err("tx finished".into());
        };
        let mut q: Query<'_, Any, AnyArguments> = sqlx::query(sqlx::AssertSqlSafe(sql));
        for p in params {
            q = bind_value(q, p);
        }
        let rows = q
            .fetch_all(&mut **tx)
            .await
            .map_err(|e| format!("sqlx tx query: {e}"))?;
        Ok(rows.iter().map(row_to_json).collect())
    }

    async fn exec(&self, sql: &str, params: &[Value]) -> BridgeResult<i64> {
        let mut g = self.tx.lock().await;
        let Some(tx) = g.as_mut() else {
            return Err("tx finished".into());
        };
        let mut q: Query<'_, Any, AnyArguments> = sqlx::query(sqlx::AssertSqlSafe(sql));
        for p in params {
            q = bind_value(q, p);
        }
        let res = q
            .execute(&mut **tx)
            .await
            .map_err(|e| format!("sqlx tx exec: {e}"))?;
        Ok(res.rows_affected() as i64)
    }

    async fn commit(&self) -> BridgeResult<()> {
        let Some(tx) = self.tx.lock().await.take() else {
            return Err("tx finished".into());
        };
        tx.commit()
            .await
            .map_err(|e| format!("sqlx tx commit: {e}"))?;
        Ok(())
    }

    async fn rollback(&self) -> BridgeResult<()> {
        let Some(tx) = self.tx.lock().await.take() else {
            return Err("tx finished".into());
        };
        tx.rollback()
            .await
            .map_err(|e| format!("sqlx tx rollback: {e}"))?;
        Ok(())
    }
}

#[async_trait]
impl DataAccessor for SqlxAccessor {
    fn dialect(&self) -> Dialect {
        self.dialect
    }

    async fn begin(&self) -> BridgeResult<Box<dyn super::db::TxSession>> {
        let tx = self
            .pool
            .begin()
            .await
            .map_err(|e| format!("sqlx tx begin: {e}"))?;
        Ok(Box::new(SqlxTx { tx: tokio::sync::Mutex::new(Some(tx)) }))
    }

    async fn query_with_params(&self, sql: &str, params: &[Value]) -> BridgeResult<Vec<JsRow>> {
        let mut q: Query<'_, Any, AnyArguments> = sqlx::query(sqlx::AssertSqlSafe(sql));
        for p in params {
            q = bind_value(q, p);
        }
        let rows = q
            .fetch_all(&self.pool)
            .await
            .map_err(|e| format!("sqlx query: {e}"))?;
        Ok(rows.iter().map(row_to_json).collect())
    }

    async fn exec_with_params(&self, sql: &str, params: &[Value]) -> BridgeResult<i64> {
        let mut q: Query<'_, Any, AnyArguments> = sqlx::query(sqlx::AssertSqlSafe(sql));
        for p in params {
            q = bind_value(q, p);
        }
        let res = q
            .execute(&self.pool)
            .await
            .map_err(|e| format!("sqlx exec: {e}"))?;
        Ok(res.rows_affected() as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::db::{Dialect, dialect_of};
    use crate::bridge::{Bridge, InMemoryKV, SchemaRegistry};
    use serde_json::json;

    // 仅类型/构造检查（不连真实库）；保证 SqlxAccessor 满足 DataAccessor trait。
    fn _assert_impl() {
        fn takes<T: DataAccessor>() {}
        takes::<SqlxAccessor>();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tx_commit_and_rollback_roundtrip() {
        let db = SqlxAccessor::arc("sqlite::memory:")
            .await
            .expect("connect");
        db.exec_with_params("create table t (id integer primary key, v text)", &[])
            .await
            .unwrap();
        // commit 路径
        let tx = db.begin().await.unwrap();
        tx.exec("insert into t (v) values (?)", &[json!("a")]).await.unwrap();
        tx.commit().await.unwrap();
        assert_eq!(
            db.query_with_params("select count(*) c from t", &[]).await.unwrap()[0]["c"],
            json!(1)
        );
        // rollback 路径
        let tx = db.begin().await.unwrap();
        tx.exec("insert into t (v) values (?)", &[json!("b")]).await.unwrap();
        tx.rollback().await.unwrap();
        assert_eq!(
            db.query_with_params("select count(*) c from t", &[]).await.unwrap()[0]["c"],
            json!(1)
        );
        // 已完结的 tx 再用 → 错误（"tx finished"）
        assert!(tx.exec("select 1", &[]).await.is_err());
    }

    #[test]
    fn dialect_parsed_from_dsn_prefix() {
        assert_eq!(dialect_of("sqlite://x.sqlite"), Dialect::Sqlite);
        assert_eq!(dialect_of("sqlite::memory:"), Dialect::Sqlite);
        assert_eq!(dialect_of("mysql://u:p@h/d"), Dialect::MySql);
        assert_eq!(dialect_of("postgres://h/d"), Dialect::Postgres);
        assert_eq!(dialect_of("postgresql://h/d"), Dialect::Postgres);
        // accessor 侧：构造期解析存字段（不连库，经 connect 后才可见——这里测纯函数）。
        assert_ne!(Dialect::MySql, Dialect::Postgres);
    }

    // P1 集成测：真实 sqlite 落库 → 经 Bridge 的 db.table / db.query 读回（LSP 替换 fake）。
    #[tokio::test(flavor = "current_thread")]
    async fn sqlite_roundtrip_via_bridge() {
        let db = SqlxAccessor::arc("sqlite::memory:")
            .await
            .expect("connect must install drivers and pin sqlite pool");
        db.exec_with_params(
            "create table user (id integer primary key, name text, age integer)",
            &[],
        )
        .await
        .unwrap();
        db.exec_with_params(
            "insert into user (name, age) values (?, ?)",
            &[json!("ever"), json!(18)],
        )
        .await
        .unwrap();

        let registry = SchemaRegistry::new().table("user", Some("id"), &["id", "name", "age"]);
        let b = Bridge::with_opts(db, Arc::new(InMemoryKV::new()), registry, false);

        // 结构化查询构造器（sea-query → 真实 sqlite，占位符须为 sqlite 方言）。
        let cap = b
            .run(
                r#"
                db.table("user").select(["id","name"]).where({field:"id",op:"eq",value:1})
                  .limit(10).all()
                  .then((rows) => json.ok({ rows }))
                  .catch((e) => json.fail(500, String(e)));
                "#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "builder query failed: {v}");
        assert_eq!(v["data"]["rows"], json!([{"id": 1, "name": "ever"}]));

        // 原始参数化 SQL（? 占位）。
        let cap = b
            .run(
                r#"db.query("select name from user where age = ?", [18])
                    .then((rows) => json.ok({ rows }))
                    .catch((e) => json.fail(500, String(e)));"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "raw query failed: {v}");
        assert_eq!(v["data"]["rows"], json!([{"name": "ever"}]));
    }
}
