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
use sqlx::any::{Any, AnyArguments, AnyRow};
use sqlx::pool::{Pool, PoolOptions};
use sqlx::query::Query;
use sqlx::{Column, Row};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dialect {
    Sqlite,
    MySql,
    Postgres,
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
    dialect: Dialect,
    next_tx: AtomicU64,
    txs: Mutex<HashMap<u64, Arc<Tx>>>,
}

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

impl Client {
    async fn connect(dsn: &str) -> Result<Self, String> {
        sqlx::any::install_default_drivers();
        let pool = PoolOptions::<Any>::new()
            .connect(dsn)
            .await
            .map_err(|e| format!("db connect: {e}"))?;
        Ok(Self {
            pool,
            dialect: dialect_of(dsn),
            next_tx: AtomicU64::new(0),
            txs: Mutex::new(HashMap::new()),
        })
    }

    async fn query(&self, sql: &str, params: &[serde_json::Value]) -> Result<Vec<u8>, String> {
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

    /// 无效 DSN 快速失败（不触网）。
    #[tokio::test(flavor = "multi_thread")]
    async fn invalid_dsn_fails_fast() {
        assert!(Client::connect("not a url").await.is_err());
        assert!(Client::connect("").await.is_err());
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

    extern "C" fn test_log(_level: u8, _msg: RString) {}
    extern "C" fn test_deliver(_topic: RString, _payload: RString) {}

    fn host() -> RArc<HostContext> {
        RArc::new(HostContext {
            log: test_log,
            deliver: test_deliver,
        })
    }

    async fn drive(fut: &mut FfiFuture) -> Result<Vec<u8>, String> {
        for _ in 0..100_000 {
            match (fut.poll)(fut.state) {
                0 => tokio::task::yield_now().await,
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
        Err("ffi drive timeout".into())
    }
}
