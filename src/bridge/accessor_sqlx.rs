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
            .map_err(|e| format!("sqlx connect: {e}"))?;
        Ok(Self {
            pool,
            dialect: dialect_of(url),
        })
    }

    /// 便捷构造 Arc 句柄。
    pub async fn arc(url: &str) -> BridgeResult<Arc<dyn DataAccessor>> {
        Ok(Arc::new(Self::connect(url).await?) as Arc<dyn DataAccessor>)
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
            Some(f) => serde_json::Number::from_f64(f)
                .map(Value::Number)
                .unwrap_or(Value::Null),
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
        Ok(Box::new(SqlxTx {
            tx: tokio::sync::Mutex::new(Some(tx)),
        }))
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
        let db = SqlxAccessor::arc("sqlite::memory:").await.expect("connect");
        db.exec_with_params("create table t (id integer primary key, v text)", &[])
            .await
            .unwrap();
        // commit 路径
        let tx = db.begin().await.unwrap();
        tx.exec("insert into t (v) values (?)", &[json!("a")])
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(
            db.query_with_params("select count(*) c from t", &[])
                .await
                .unwrap()[0]["c"],
            json!(1)
        );
        // rollback 路径
        let tx = db.begin().await.unwrap();
        tx.exec("insert into t (v) values (?)", &[json!("b")])
            .await
            .unwrap();
        tx.rollback().await.unwrap();
        assert_eq!(
            db.query_with_params("select count(*) c from t", &[])
                .await
                .unwrap()[0]["c"],
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

        let registry = SchemaRegistry::new().table("user", &["id"], &["id", "name", "age"]);
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

    #[tokio::test(flavor = "current_thread")]
    async fn tx_query_and_typed_columns() {
        let db = SqlxAccessor::arc("sqlite::memory:").await.expect("connect");
        db.exec_with_params(
            "create table q (id integer primary key, b integer, f real, blobf blob, t text)",
            &[],
        )
        .await
        .unwrap();
        db.exec_with_params(
            "insert into q (b, f, blobf, t) values (?, ?, ?, ?)",
            &[json!(true), json!(1.5), json!("raw"), json!("hi")],
        )
        .await
        .unwrap();

        let tx = db.begin().await.unwrap();
        // SqlxTx::query（事务内查询）+ column_json 的 i64/f64/bytes/text 分支
        let rows = tx.query("select b, f, blobf, t from q", &[]).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].get("b").is_some());
        assert!((rows[0]["f"].as_f64().unwrap() - 1.5).abs() < 1e-9);
        assert_eq!(rows[0]["blobf"], json!("raw"));
        assert_eq!(rows[0]["t"], json!("hi"));

        // bind_value 的 other 分支：绑定对象（Any 接受其字符串化）
        let r = tx.query("select ? as v", &[json!({"x": 1})]).await.unwrap();
        assert_eq!(r[0]["v"], json!("{\"x\":1}"));

        tx.commit().await.unwrap();
        // 已完结的事务再次使用 → "tx finished"
        assert!(tx.query("select 1", &[]).await.is_err());
    }

    /// 读侧大整数护栏（v0.1.22）：超界整数以**十进制字符串**交给 JS。
    /// 旧行为是 v8 BigInt → `json.ok` 直接 500（`TypeError: Do not know how to serialize a BigInt`）。
    #[tokio::test(flavor = "current_thread")]
    async fn bigint_reads_cross_as_decimal_strings() {
        let db = SqlxAccessor::arc("sqlite::memory:").await.expect("connect");
        db.exec_with_params(
            "create table big (id integer primary key, f real, t text)",
            &[],
        )
        .await
        .unwrap();
        // 边界：i64::MIN、2^53-1（安全上界）、2^53、2^53+1、i64::MAX
        for id in [
            i64::MIN,
            9007199254740991i64,
            9007199254740992,
            9007199254740993,
            i64::MAX,
        ] {
            db.exec_with_params(
                "insert into big (id, f, t) values (?, 1.5, ?)",
                &[json!(id), json!(id.to_string())],
            )
            .await
            .unwrap();
        }
        let registry = SchemaRegistry::new().table("big", &["id"], &["id", "f", "t"]);
        let b = Bridge::with_opts(db, Arc::new(InMemoryKV::new()), registry, false);

        // ① db.query：超界 → string（精确），安全范围 → number，REAL 列仍是 number。
        let cap = b
            .run(
                r#"
                db.query("select id, f from big order by id")
                  .then((rows) => json.ok({ ids: rows.map((r) => r.id), types: rows.map((r) => typeof r.id), f: rows[0].f }))
                  .catch((e) => json.fail(500, String(e)));
                "#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "大整数读取不得再 500：{v}");
        assert_eq!(
            v["data"]["ids"],
            json!([
                "-9223372036854775808",
                9007199254740991i64,
                "9007199254740992",
                "9007199254740993",
                "9223372036854775807"
            ]),
            "逐字精确且阈值 = 2^53-1"
        );
        assert_eq!(
            v["data"]["types"],
            json!(["string", "number", "string", "string", "string"])
        );
        assert_eq!(v["data"]["f"], json!(1.5), "REAL 列仍是 number");

        // ② 构造器路径（op_db_query_build）同一护栏。
        let cap = b
            .run(
                r#"
                db.table("big").select(["id"]).orderBy([{field:"id",dir:"asc"}]).limit(10).all()
                  .then((rows) => json.ok({ ids: rows.map((r) => r.id) }))
                  .catch((e) => json.fail(500, String(e)));
                "#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(v["data"]["ids"][4], json!("9223372036854775807"));

        // ③ tx 内查询同护栏。
        let cap = b
            .run(
                r#"
                db.tx(async (tx) => {
                  const rows = await tx.query("select id from big where id = ?", ["9223372036854775807"]);
                  return json.ok({ id: rows[0].id, type: typeof rows[0].id });
                }).catch((e) => json.fail(500, String(e)));
                "#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(
            v["data"],
            json!({"id": "9223372036854775807", "type": "string"})
        );
    }

    /// 写侧大整数通道（v0.1.22）：`toBigInt()` 的 BigInt 经保留标记 → 宿主绑 i64，精确落库。
    #[tokio::test(flavor = "current_thread")]
    async fn bigint_writes_round_trip_exactly() {
        let db = SqlxAccessor::arc("sqlite::memory:").await.expect("connect");
        db.exec_with_params("create table w (id integer primary key, note text)", &[])
            .await
            .unwrap();
        let registry = SchemaRegistry::new().table("w", &["id"], &["id", "note"]);
        let b = Bridge::with_opts(db.clone(), Arc::new(InMemoryKV::new()), registry, false);

        // ① db.exec 参数 + toBigInt 三种入参形态（十进制串 / safe number / bigint）
        let cap = b
            .run(
                r#"
                const ids = [toBigInt("4886674138783273204"), toBigInt(42), toBigInt(7n)];
                Promise.all(ids.map((id, i) => db.exec("insert into w (id, note) values (?, ?)", [id, "n" + i])))
                  .then(() => json.ok({ types: ids.map((v) => typeof v) }))
                  .catch((e) => json.fail(500, String(e)));
                "#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "bigint 参数必须可写：{v}");
        assert_eq!(v["data"]["types"], json!(["bigint", "bigint", "bigint"]));
        // 直接读库（Rust 侧）核对逐字精确
        let rows = db
            .query_with_params("select id, note from w order by id", &[])
            .await
            .unwrap();
        assert_eq!(rows[0]["id"], json!(7));
        assert_eq!(rows[1]["id"], json!(42));
        assert_eq!(rows[2]["id"], json!(4886674138783273204i64), "雪花量级精确");

        // ② 构造器：insert 行值 + where 值 + update sets（嵌套值均须编码）
        let cap = b
            .run(
                r#"
                db.table("w").insert({ id: toBigInt("9007199254740993"), note: "ins" }).run()
                  .then(() => db.table("w").update({ note: "upd" })
                    .where({ field: "id", op: "eq", value: toBigInt("9007199254740993") }).run())
                  .then(() => db.table("w").select(["note"])
                    .where({ field: "id", op: "eq", value: toBigInt("9007199254740993") }).all())
                  .then((rows) => json.ok({ row: rows[0] }))
                  .catch((e) => json.fail(500, String(e)));
                "#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(v["data"]["row"]["note"], json!("upd"));

        // ③ in 数组里的 bigint
        let cap = b
            .run(
                r#"
                db.table("w").select(["id"]).where({ field: "id", op: "in", value: [toBigInt("7"), toBigInt("9007199254740993")] })
                  .orderBy([{field:"id",dir:"asc"}]).all()
                  .then((rows) => json.ok({ ids: rows.map((r) => r.id) }))
                  .catch((e) => json.fail(500, String(e)));
                "#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(v["data"]["ids"], json!([7, "9007199254740993"]));
    }

    /// `toBigInt` 的 fail-loud 与保留形状边界（v0.1.22）。
    #[tokio::test(flavor = "current_thread")]
    async fn to_bigint_fails_loud_and_marker_is_strict() {
        let db = SqlxAccessor::arc("sqlite::memory:").await.expect("connect");
        db.exec_with_params("create table s (t text)", &[])
            .await
            .unwrap();
        let registry = SchemaRegistry::new().table("s", &["t"], &["t"]);
        let b = Bridge::with_opts(db.clone(), Arc::new(InMemoryKV::new()), registry, false);

        // 非法入参一律 throw（**不**静默 coerce）——尤其 Number("<超界串>") 这一 U38 陷阱
        let cap = b
            .run(
                r#"
                const cases = [
                  [9007199254740992, "unsafe number"],   // 已坍缩的 f64
                  [Number("4886674138783273204"), "Number(bigString)"],
                  [1.5, "float"], ["1.5", "float string"], ["abc", "non-numeric"],
                  ["007", "leading zero"], ["+1", "plus"], [" 1", "space"], ["-0", "neg zero"],
                  ["9223372036854775808", "out of i64"], [null, "null"], [true, "bool"], [{}, "object"],
                ];
                const thrown = cases.map(([v]) => { try { toBigInt(v); return "NO-THROW"; } catch (e) { return e.constructor.name; } });
                const dbl = ["abc", null, {}, true].map((v) => { try { toDouble(v); return "NO-THROW"; } catch (e) { return e.constructor.name; } });
                json.ok({ thrown, dbl, ok: [String(toBigInt("9223372036854775807")), toDouble("1.5"), toDouble(3n)] });
                "#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        for (i, t) in v["data"]["thrown"].as_array().unwrap().iter().enumerate() {
            assert_ne!(t, "NO-THROW", "toBigInt 第 {i} 例必须抛错");
        }
        for (i, t) in v["data"]["dbl"].as_array().unwrap().iter().enumerate() {
            assert_ne!(t, "NO-THROW", "toDouble 第 {i} 例必须抛错");
        }
        assert_eq!(v["data"]["ok"], json!(["9223372036854775807", 1.5, 3]));

        // 畸形标记（非规范十进制）**不**被识别为整数：按普通对象串化绑定，而非静默绑 7。
        b.run(
            r#"db.exec("insert into s (t) values (?)", [{ "$oj$i64": "007" }]).then(() => json.ok({}));"#,
        )
        .await
        .unwrap();
        // 规范标记则绑 i64（写进 TEXT 列由 DB 自行转文本，值精确）
        b.run(
            r#"db.exec("insert into s (t) values (?)", [{ "$oj$i64": "42" }]).then(() => json.ok({}));"#,
        )
        .await
        .unwrap();
        let rows = db
            .query_with_params("select t from s order by t", &[])
            .await
            .unwrap();
        assert_eq!(rows[0]["t"], json!("42"), "规范标记按 i64 绑定");
        assert_eq!(
            rows[1]["t"],
            json!("{\"$oj$i64\":\"007\"}"),
            "畸形标记按普通值处理（不静默绑 7）"
        );
    }

    /// U38 回归（下游真实事故）：`Number(max(id)) + 1` 生成下一序号 → 静默坍缩 → dup 500。
    /// 范式 `toBigInt(max) + 1n` 必须**精确且可重复**；同时把旧写法的坍缩钉成反证。
    #[tokio::test(flavor = "current_thread")]
    async fn u38_max_plus_one_sequence_stays_exact() {
        let db = SqlxAccessor::arc("sqlite::memory:").await.expect("connect");
        db.exec_with_params("create table seq (id integer primary key, note text)", &[])
            .await
            .unwrap();
        // 预置雪花量级起点（> 2^53），复刻事故表的初始状态
        db.exec_with_params(
            "insert into seq (id, note) values (?, 'seed')",
            &[json!(4886674138783273204i64)],
        )
        .await
        .unwrap();
        let registry = SchemaRegistry::new().table("seq", &["id"], &["id", "note"]);
        let b = Bridge::with_opts(db.clone(), Arc::new(InMemoryKV::new()), registry, false);

        // ① 范式：连续两次分配都精确推进（旧写法第二次必然撞主键）
        let good = r#"
            (async () => {
              const rows = await db.query("select max(id) as m from seq");
              const next = toBigInt(rows[0].m) + 1n;
              await db.exec("insert into seq (id, note) values (?, ?)", [next, "auto"]);
              return json.ok({ next: next.toString(), readType: typeof rows[0].m });
            })().catch((e) => json.fail(500, String(e)));
        "#;
        for i in 0..2i64 {
            let cap = b.run(good).await.unwrap();
            let v: Value = serde_json::from_slice(&cap.body).unwrap();
            assert_eq!(v["code"], 0, "第 {i} 次分配不得失败：{v}");
            assert_eq!(v["data"]["readType"], "string", "读侧超大整数以字符串交付");
            assert_eq!(
                v["data"]["next"],
                Value::from((4886674138783273204i64 + 1 + i).to_string()),
                "必须逐字等于 max+1"
            );
        }
        let n = db
            .query_with_params("select count(*) c from seq", &[])
            .await
            .unwrap();
        assert_eq!(n[0]["c"], json!(3), "3 行 = 种子 + 两次分配");

        // ② 反证：旧写法（`Number(m) + 1`）算出的值已不是 max+1（静默坍缩）
        let bad = r#"
            (async () => {
              const rows = await db.query("select max(id) as m from seq");
              const m = toBigInt(rows[0].m);
              const collapsed = Number(rows[0].m) + 1;            // U38 的写法
              return json.ok({ collapsed, isExact: BigInt(collapsed) === m + 1n });
            })().catch((e) => json.fail(500, String(e)));
        "#;
        let cap = b.run(bad).await.unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(
            v["data"]["isExact"], false,
            "旧写法必须被证明不等值（这正是事故起点）：{v}"
        );
    }

    /// 评审补测（v0.1.22）：其余到达参数的 JS 入口与快照 API 的 bigint 编码。
    /// 覆盖 `db.query` 参数、`toSQL().params`、`toJSON/fromJSON` 往返、`having`、
    /// CASE `then`、`union/with` 子查询（unwrapSub）、Date 的 JSON 语义不变。
    #[tokio::test(flavor = "current_thread")]
    async fn bigint_covers_remaining_js_entry_points() {
        let db = SqlxAccessor::arc("sqlite::memory:").await.expect("connect");
        db.exec_with_params("create table c (id integer primary key, t text)", &[])
            .await
            .unwrap();
        db.exec_with_params(
            "insert into c (id, t) values (?, ?)",
            &[json!(9007199254740993i64), json!("n")],
        )
        .await
        .unwrap();
        let registry = SchemaRegistry::new().table("c", &["id"], &["id", "t"]);
        let b = Bridge::with_opts(db, Arc::new(InMemoryKV::new()), registry, false);

        let cap = b
            .run(
                r#"
                (async () => {
                  // ① db.query 的参数也走编码（含未经 toBigInt 的裸 bigint 字面量）
                  const r1 = await db.query("select ? as v", [9007199254740993n]);
                  const r2 = await db.query("select t from c where id = ?", [toBigInt("9007199254740993")]);
                  // ② toSQL().params：标记 → i64 → 出线又是字符串（读侧护栏同款）
                  const ts = db.table("c").select(["t"]).where({field:"id",op:"eq",value: toBigInt("9007199254740993")}).toSQL();
                  // ③ toJSON/fromJSON 往返：标记必须存活，且 fromJSON 后仍可执行
                  const snap = db.table("c").where({field:"id",op:"eq",value: toBigInt("9007199254740993")}).toJSON();
                  const viaSnap = await db.fromJSON(snap).select(["t"]).all();
                  // ④ having + CASE then 的编码进快照可见
                  const hv = db.table("c").select(["t"]).having({field:"id",op:"eq",value: toBigInt("9007199254740993")}).toJSON();
                  const cw = db.table("c").select([{case:{when:[{cond:{field:"id",op:"eq",value: toBigInt("7")}, then: toBigInt("9007199254740993")}], else: toBigInt("1")}, as:"x"}]).toJSON();
                  // ⑤ union/with 子查询（unwrapSub）：标记跨嵌套存活
                  const un = db.table("c").select(["id"]).union(
                    db.table("c").select(["id"]).where({field:"id",op:"eq",value: toBigInt("9007199254740993")})
                  ).toJSON();
                  // ⑥ Date 的 JSON 语义未被破坏（快照里应是 ISO 串而非 {}）
                  const dt = db.table("c").where({field:"t",op:"gt",value: new Date(0)}).toJSON();
                  return json.ok({ r1: r1[0].v, r2: r2[0]?.t, tsParam0: ts.params[0], viaSnap: viaSnap[0]?.t,
                                   hv: hv.having, cw: cw.columns[0], un: un.unions[0].query.conditions[0].value,
                                   dt: dt.conditions[0].value });
                })().catch((e) => json.fail(500, String(e)));
                "#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        // ① 写侧编码 + 读侧护栏：写进去的 i64 精确，读出来是字符串
        assert_eq!(v["data"]["r1"], json!("9007199254740993"));
        assert_eq!(v["data"]["r2"], json!("n"));
        // ② toSQL().params 的超界整数同样是字符串（sanitize 覆盖 Qv::BigInt 路径）
        assert_eq!(v["data"]["tsParam0"], json!("9007199254740993"));
        // ③ 快照往返后仍能精确命中
        assert_eq!(v["data"]["viaSnap"], json!("n"));
        // ④ having / CASE then 都被编码为标记（having 存的是整棵条件树）
        assert_eq!(
            v["data"]["hv"]["value"]["$oj$i64"],
            json!("9007199254740993")
        );
        assert_eq!(
            v["data"]["cw"]["case"]["when"][0]["then"]["$oj$i64"],
            json!("9007199254740993")
        );
        assert_eq!(v["data"]["cw"]["case"]["else"]["$oj$i64"], json!("1"));
        // ⑤ 子查询里的标记存活
        assert_eq!(v["data"]["un"]["$oj$i64"], json!("9007199254740993"));
        // ⑥ Date 仍是 ISO 串（不是 {}）——快照走 JSON 语义
        assert_eq!(v["data"]["dt"], json!("1970-01-01T00:00:00.000Z"));
    }

    /// 评审核对：`toDouble` 按 `Number()` 语义（文档须与实现一致）；二进制参数显式拒绝。
    #[tokio::test(flavor = "current_thread")]
    async fn to_double_follows_number_semantics_and_binary_params_rejected() {
        let db = SqlxAccessor::arc("sqlite::memory:").await.expect("connect");
        let b = Bridge::with_opts(
            db,
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
        );
        let cap = b
            .run(
                r#"
                (async () => {
                  const d = [toDouble("1.5"), toDouble("1e3"), toDouble("0x10"), toDouble("Infinity"), toDouble(" 2 ")];
                  let bin = "NO-THROW";
                  try { await db.query("select ? as v", [new Uint8Array([1, 2])]); bin = "db.query:NO-THROW"; } catch (e) { bin = "db.query:" + e.constructor.name; }
                  return json.ok({ d, bin });
                })().catch((e) => json.fail(500, String(e)));
                "#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        // `Number()` 语义：十六进制/Infinity/前后空白照旧接受（文档已按此描述）
        assert_eq!(v["data"]["d"][0], json!(1.5));
        assert_eq!(v["data"]["d"][1], json!(1000));
        assert_eq!(v["data"]["d"][2], json!(16));
        assert!(v["data"]["d"][3].is_null(), "Infinity 不能进 JSON → null");
        assert_eq!(v["data"]["d"][4], json!(2));
        // 二进制参数：改动前后都是 TypeError（旧为 serde_v8 类型错，新为显式检查 + 更清晰的消息）
        assert_eq!(
            v["data"]["bin"],
            json!("db.query:TypeError"),
            "二进制参数必须报错"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn bad_sql_errors_and_tx_exec() {
        let db = SqlxAccessor::arc("sqlite::memory:").await.expect("connect");
        // query_with_params 错误路径
        assert!(
            db.query_with_params("select * from nope", &[])
                .await
                .is_err()
        );
        assert!(
            db.exec_with_params("insert into nope (x) values (1)", &[])
                .await
                .is_err()
        );

        db.exec_with_params("create table e (id integer primary key, v text)", &[])
            .await
            .unwrap();
        let tx = db.begin().await.unwrap();
        assert!(
            tx.exec("insert into e (v) values (?)", &[json!("a")])
                .await
                .is_ok()
        );
        // 事务内查询错误路径
        assert!(tx.query("select * from missing", &[]).await.is_err());
        tx.commit().await.unwrap();

        // begin 后 rollback 再查不到
        let tx = db.begin().await.unwrap();
        tx.exec("insert into e (v) values (?)", &[json!("b")])
            .await
            .unwrap();
        tx.rollback().await.unwrap();
        let n = db
            .query_with_params("select count(*) c from e", &[])
            .await
            .unwrap();
        assert_eq!(n[0]["c"], json!(1));
    }
}
