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
    ABI_VERSION, DataAccessorVtable, FfiFuture, HostContext, PluginDescriptor, PluginRegistrations,
    RArc, RBytes, RResult, RString, RVec,
};
use sqlx::any::{Any, AnyArguments, AnyRow};
use sqlx::pool::{Pool, PoolOptions};
use sqlx::query::Query;
use sqlx::{Column, Row};
use std::collections::HashMap;
use std::ffi::c_void;
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

/// 单连接（vtable connect 建立）。txs 查表（事务句柄化）。
struct Client {
    pool: Pool<Any>,
    dialect: Dialect,
    next_tx: AtomicU64,
    txs: Mutex<HashMap<u64, Arc<Tx>>>,
}

/// 事务：Option 被 take 后（已完结）再调用报 "tx finished"；Mutex 串行并发 op。
struct Tx {
    tx: tokio::sync::Mutex<Option<sqlx::Transaction<'static, Any>>>,
}

static PLUGIN: OnceLock<DbPluginState> = OnceLock::new();

fn state() -> &'static DbPluginState {
    PLUGIN.get().expect("oj-db-mysql: init not called")
}

// ---- FfiFuture 桥（spike S.2 定稿：oneshot 接结果，poll 消费式暂存）----

struct CallState {
    rx: tokio::sync::oneshot::Receiver<Result<Vec<u8>, String>>,
    result: Option<Result<Vec<u8>, String>>,
}

extern "C" fn poll(state: *mut c_void) -> i32 {
    let s = unsafe { &mut *(state as *mut CallState) };
    if let Some(r) = &s.result {
        return if r.is_ok() { 1 } else { -1 };
    }
    match s.rx.try_recv() {
        Ok(r) => {
            let code = if r.is_ok() { 1 } else { -1 };
            s.result = Some(r);
            code
        }
        Err(tokio::sync::oneshot::error::TryRecvError::Empty) => 0,
        Err(tokio::sync::oneshot::error::TryRecvError::Closed) => -1,
    }
}

extern "C" fn take(state: *mut c_void) -> RResult<RBytes, RString> {
    let s = unsafe { &mut *(state as *mut CallState) };
    match s.result.take() {
        Some(Ok(bytes)) => {
            let mut v = RBytes::new();
            for b in bytes {
                v.push(b);
            }
            RResult::Ok(v)
        }
        Some(Err(e)) => RResult::Err(RString::from(e.as_str())),
        None => RResult::Err(RString::from("take before ready or twice")),
    }
}

extern "C" fn free(state: *mut c_void) {
    if !state.is_null() {
        drop(unsafe { Box::from_raw(state as *mut CallState) });
    }
}

/// 起一个 FfiFuture：异步工作 spawn 到插件 runtime，oneshot 收结果。
fn spawn_call(fut: impl std::future::Future<Output = Result<Vec<u8>, String>> + Send + 'static) -> FfiFuture {
    let (tx, rx) = tokio::sync::oneshot::channel();
    state().rt.spawn(async move {
        let _ = tx.send(fut.await);
    });
    FfiFuture {
        state: Box::into_raw(Box::new(CallState { rx, result: None })).cast(),
        poll,
        take,
        free,
    }
}

// ---- sqlx 逻辑（迁移自 core accessor_sqlx.rs，绑定/行转换逐字对齐）----

/// 将单个 JSON 值绑定到 sqlx 语句（按类型选择可 Encode 的具体类型）。
fn bind_value<'q>(q: Query<'q, Any, AnyArguments>, v: &serde_json::Value) -> Query<'q, Any, AnyArguments> {
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

/// 单行 AnyRow -> serde_json::Value（逐列按类型探测）。
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

/// 逐列尝试常见类型，首个成功者转 JSON。
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
        serde_json::to_vec(&(res.rows_affected() as i64)).map_err(|e| format!("db exec serialize: {e}"))
    }

    async fn begin(&self) -> Result<u64, String> {
        let tx = self
            .pool
            .begin()
            .await
            .map_err(|e| format!("db tx begin: {e}"))?;
        let id = self.next_tx.fetch_add(1, Ordering::SeqCst) + 1;
        self.txs.lock().unwrap().insert(id, Arc::new(Tx { tx: tokio::sync::Mutex::new(Some(tx)) }));
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

    async fn tx_query(&self, tx_id: u64, sql: &str, params: &[serde_json::Value]) -> Result<Vec<u8>, String> {
        let tx = self.tx(tx_id)?;
        let mut g = tx.tx.lock().await;
        let Some(t) = g.as_mut() else { return Err("tx finished".into()) };
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

    async fn tx_exec(&self, tx_id: u64, sql: &str, params: &[serde_json::Value]) -> Result<Vec<u8>, String> {
        let tx = self.tx(tx_id)?;
        let mut g = tx.tx.lock().await;
        let Some(t) = g.as_mut() else { return Err("tx finished".into()) };
        let mut q: Query<'_, Any, AnyArguments> = sqlx::query(sqlx::AssertSqlSafe(sql));
        for p in params {
            q = bind_value(q, p);
        }
        let res = q
            .execute(&mut **t)
            .await
            .map_err(|e| format!("db tx exec: {e}"))?;
        serde_json::to_vec(&(res.rows_affected() as i64)).map_err(|e| format!("db tx exec serialize: {e}"))
    }

    async fn tx_commit(&self, tx_id: u64) -> Result<Vec<u8>, String> {
        let tx = self.txs.lock().unwrap().remove(&tx_id).ok_or_else(|| format!("db: unknown tx {tx_id}"))?;
        let Some(t) = tx.tx.lock().await.take() else { return Err("tx finished".into()) };
        t.commit().await.map_err(|e| format!("db tx commit: {e}"))?;
        Ok(b"".to_vec())
    }

    async fn tx_rollback(&self, tx_id: u64) -> Result<Vec<u8>, String> {
        let tx = self.txs.lock().unwrap().remove(&tx_id).ok_or_else(|| format!("db: unknown tx {tx_id}"))?;
        let Some(t) = tx.tx.lock().await.take() else { return Err("tx finished".into()) };
        t.rollback().await.map_err(|e| format!("db tx rollback: {e}"))?;
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

    async fn do_tx_query(&self, handle: u64, tx_id: u64, sql: &str, params: &str) -> Result<Vec<u8>, String> {
        let p: Vec<serde_json::Value> =
            serde_json::from_str(params).map_err(|e| format!("db tx_query: bad params: {e}"))?;
        self.client(handle)?.tx_query(tx_id, sql, &p).await
    }

    async fn do_tx_exec(&self, handle: u64, tx_id: u64, sql: &str, params: &str) -> Result<Vec<u8>, String> {
        let p: Vec<serde_json::Value> =
            serde_json::from_str(params).map_err(|e| format!("db tx_exec: bad params: {e}"))?;
        self.client(handle)?.tx_exec(tx_id, sql, &p).await
    }
}

// ---- vtable（同步签名返回 FfiFuture）----

extern "C" fn connect(cfg: RString) -> FfiFuture {
    let st = state();
    spawn_call(async move {
        let dsn = cfg[..].to_string();
        let client = Client::connect(&dsn).await?;
        let handle = st.next_handle.fetch_add(1, Ordering::SeqCst) + 1;
        st.clients.lock().unwrap().insert(handle, Arc::new(client));
        Ok(format!(r#"{{"handle":{handle}}}"#).into_bytes())
    })
}

extern "C" fn query(handle: u64, sql: RString, params: RString) -> FfiFuture {
    let st = state();
    spawn_call(async move { st.do_query(handle, &sql[..], &params[..]).await })
}

extern "C" fn exec(handle: u64, sql: RString, params: RString) -> FfiFuture {
    let st = state();
    spawn_call(async move { st.do_exec(handle, &sql[..], &params[..]).await })
}

extern "C" fn begin(handle: u64) -> FfiFuture {
    let st = state();
    spawn_call(async move {
        let c = st.client(handle)?;
        let tx_id = c.begin().await?;
        Ok(format!(r#"{{"tx_id":{tx_id}}}"#).into_bytes())
    })
}

extern "C" fn tx_query(handle: u64, tx_id: u64, sql: RString, params: RString) -> FfiFuture {
    let st = state();
    spawn_call(async move { st.do_tx_query(handle, tx_id, &sql[..], &params[..]).await })
}

extern "C" fn tx_exec(handle: u64, tx_id: u64, sql: RString, params: RString) -> FfiFuture {
    let st = state();
    spawn_call(async move { st.do_tx_exec(handle, tx_id, &sql[..], &params[..]).await })
}

extern "C" fn tx_commit(handle: u64, tx_id: u64) -> FfiFuture {
    let st = state();
    spawn_call(async move { st.client(handle)?.tx_commit(tx_id).await })
}

extern "C" fn tx_rollback(handle: u64, tx_id: u64) -> FfiFuture {
    let st = state();
    spawn_call(async move { st.client(handle)?.tx_rollback(tx_id).await })
}

extern "C" fn dialect(handle: u64) -> RString {
    let d = state().client(handle).map(|c| c.dialect).unwrap_or(Dialect::Sqlite);
    RString::from(dialect_str(d))
}

extern "C" fn close(handle: u64) {
    state().clients.lock().unwrap().remove(&handle);
}

extern "C" fn schemes() -> RVec<RString> {
    let mut v = RVec::new();
    v.push(RString::from("mysql://"));
    v
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

extern "C" fn register() -> PluginRegistrations {
    PluginRegistrations { es: std::ptr::null(), db: &VTABLE, blob: std::ptr::null(), bus: std::ptr::null(), kv: std::ptr::null() }
}

// ---- 入口 ----

fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        name: RString::from("db-mysql"),
        semver: RString::from("0.1.0"),
        abi_version: ABI_VERSION,
        fingerprint: RString::from(oj_plugin_ffi::HOST_FINGERPRINT),
        register,
    }
}

fn init(host: RArc<HostContext>, cfg: RString) -> RResult<PluginDescriptor, RString> {
    // 同进程二次 init（多装配/测试重载同一 dylib）：cfg 以首次为准，直接复用 descriptor。
    if PLUGIN.get().is_some() {
        return RResult::Ok(descriptor());
    }
    let _ = (&host, &cfg); // db 插件 init 无装配期配置（DSN 在 connect 传入）
    let st = DbPluginState {
        rt: runtime(),
        clients: Mutex::new(HashMap::new()),
        next_handle: AtomicU64::new(0),
    };
    let _ = PLUGIN.set(st);
    RResult::Ok(descriptor())
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("oj-db-mysql tokio runtime")
}

oj_plugin_ffi::oj_plugin_entry!(init);

#[cfg(test)]
mod tests {
    use super::*;

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
            Err(e) => panic!("init failed: {}", e[..].to_string()),
        };
        assert_eq!(&desc.name[..], "db-mysql");

        let mut c = connect(RString::from(url.as_str()));
        let bytes = drive(&mut c).await.expect("connect");
        let handle: u64 = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["handle"]
            .as_u64()
            .unwrap();

        drive(&mut exec(handle, RString::from("create table if not exists oj_plugin_t (id int primary key, v text)"), RString::from("[]")))
            .await
            .expect("create");

        drive(&mut exec(handle, RString::from("insert into oj_plugin_t (id, v) values (?, ?)"), RString::from(r#"[1,"hi"]"#)))
            .await
            .expect("insert");

        let rows = drive(&mut query(handle, RString::from("select v from oj_plugin_t where id = ?"), RString::from(r#"[1]"#)))
            .await
            .expect("query");
        let v: serde_json::Value = serde_json::from_slice(&rows).unwrap();
        assert_eq!(v[0]["v"], serde_json::json!("hi"), "{v}");

        // 事务 commit
        let bytes = drive(&mut begin(handle)).await.expect("begin");
        let tx_id = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["tx_id"].as_u64().unwrap();
        drive(&mut tx_exec(handle, tx_id, RString::from("insert into oj_plugin_t (id, v) values (?, ?)"), RString::from(r#"[2,"tx"]"#)))
            .await
            .expect("tx insert");
        drive(&mut tx_commit(handle, tx_id)).await.expect("tx commit");

        // 事务 rollback
        let bytes = drive(&mut begin(handle)).await.expect("begin2");
        let tx_id = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["tx_id"].as_u64().unwrap();
        drive(&mut tx_exec(handle, tx_id, RString::from("insert into oj_plugin_t (id, v) values (?, ?)"), RString::from(r#"[3,"rb"]"#)))
            .await
            .expect("tx insert2");
        drive(&mut tx_rollback(handle, tx_id)).await.expect("tx rollback");

        let rows = drive(&mut query(handle, RString::from("select count(*) c from oj_plugin_t where id in (1,2,3)"), RString::from("[]")))
            .await
            .expect("count");
        let v: serde_json::Value = serde_json::from_slice(&rows).unwrap();
        assert_eq!(v[0]["c"], serde_json::json!(2), "rolled back row must be absent: {v}");

        close(handle);
        drive(&mut query(handle, RString::from("select 1"), RString::from("[]"))).await
            .expect_err("unknown handle after close");
    }

    extern "C" fn test_log(_level: u8, _msg: RString) {}
extern "C" fn test_deliver(_topic: RString, _payload: RString) {}

    fn host() -> RArc<HostContext> {
        RArc::new(HostContext { log: test_log, deliver: test_deliver })
    }

    /// FfiFuture → 测试异步桥（等价 core await_ffi 的 poll 轮询）。
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
