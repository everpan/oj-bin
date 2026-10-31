use super::*;
use crate::bridge::db::TxSession;
use crate::bridge::{Bridge, DataAccessor, Extras, InMemoryKV, SchemaRegistry};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// sqlite 文件 DSN（:memory: 每连接独立库——批事务跨连接不可见，REQ-11 教训）。
async fn sqlite_file() -> (tempfile::TempDir, Arc<dyn DataAccessor>) {
    let dir = tempfile::tempdir().unwrap();
    let dsn = format!("sqlite://{}/t.db", dir.path().display());
    let db = crate::bridge::db_backend::DbBackendRegistry::builtin()
        .connect(&dsn, dir.path())
        .await
        .unwrap();
    (dir, db)
}

fn bridge_with(
    db: Arc<dyn DataAccessor>,
    registry: SchemaRegistry,
    blobs: Option<Arc<crate::bridge::blob::BlobRegistry>>,
    section: super::super::db_policy::DbImportSection,
) -> Bridge {
    Bridge::with_dbs_and_loader(
        HashMap::from([("default".to_string(), db)]),
        Arc::new(InMemoryKV::new()),
        registry,
        false,
        None,
        Extras {
            blobs,
            import_section: Some(Arc::new(section)),
            db_access: Some(Arc::new(Default::default())),
            ..Default::default()
        },
    )
}

fn tmp_blobs() -> (tempfile::TempDir, Arc<crate::bridge::blob::BlobRegistry>) {
    let d = tempfile::tempdir().unwrap();
    let local = crate::bridge::blob::LocalBlob::new(d.path(), "/v1/api").unwrap();
    (
        d,
        crate::bridge::blob::registry_with_default(Arc::new(local)),
    )
}

async fn import_js(b: &Bridge, args: &str) -> Result<Value, Value> {
    let js = format!(
        r#"(async () => {{
                 const r = await db.import({args}).then(
                   (v) => ({{ ok: true, v }}),
                   (e) => ({{ ok: false, code: e.code, msg: String(e.message || e) }}));
                 json.ok(r);
               }})().catch(e => json.fail(500, String(e)));"#,
    );
    let cap = b.run_with(&js, Default::default()).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    let r = v["data"].clone();
    if r["ok"] == true {
        Ok(r["v"].clone())
    } else {
        Err(r)
    }
}

/// 轮询直至 status.state 离开 running（deadline 有界——禁固定 sleep 当同步）。
async fn wait_terminal(b: &Bridge, task_id: &str) -> Value {
    wait_terminal_until(b, task_id, 15).await
}

async fn wait_terminal_until(b: &Bridge, task_id: &str, secs: u64) -> Value {
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    loop {
        let js = format!(
            r#"(async () => {{
                     const st = await db.importStatus({task_id:?});
                     json.ok(st);
                   }})().catch(e => json.fail(500, String(e)));"#,
        );
        let cap = b.run_with(&js, Default::default()).await.unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        let st = v["data"].clone();
        if st["state"] != "running" {
            return st;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "import task hung in running"
        );
        tokio::task::yield_now().await;
    }
}

async fn count_rows(db: &Arc<dyn DataAccessor>, table: &str) -> i64 {
    let rows = db
        .query_with_params(&format!("select count(*) as n from {table}"), &[])
        .await
        .unwrap();
    rows[0]["n"].as_i64().unwrap()
}

fn users_registry() -> SchemaRegistry {
    SchemaRegistry::new().table("users", &["id"], &["id", "name"])
}

/// blob csv 全链路：读源 → 解析 → 批事务落库 → 状态面。
#[tokio::test(flavor = "current_thread")]
async fn import_blob_csv_happy_path_committed_rows() {
    let (_d, db) = sqlite_file().await;
    db.exec_with_params(
        "create table users (id integer primary key, name text)",
        &[],
    )
    .await
    .unwrap();
    let (_bd, blobs) = tmp_blobs();
    let b = bridge_with(
        db.clone(),
        users_registry(),
        Some(blobs.clone()),
        Default::default(),
    );
    let csv = b"id,name\n1,a\n2,b\n3,c\n";
    blobs
        .default()
        .unwrap()
        .put("in/x.csv", csv, None)
        .await
        .unwrap();
    let r = import_js(
        &b,
        r#"{ table: "users", blobKey: "in/x.csv", format: "csv" }"#,
    )
    .await
    .unwrap();
    let st = wait_terminal(&b, r["taskId"].as_str().unwrap()).await;
    assert_eq!(st["state"], json!("done"), "{st}");
    assert_eq!(st["rows"], json!(3), "{st}");
    assert!(st["bytes"].as_u64().unwrap() > 0, "{st}");
    assert_eq!(st["errors"], json!(0), "{st}");
    assert_eq!(count_rows(&db, "users").await, 3);
}

/// 内联 rows 源 happy path。
#[tokio::test(flavor = "current_thread")]
async fn import_inline_rows_happy_path() {
    let (_d, db) = sqlite_file().await;
    db.exec_with_params(
        "create table users (id integer primary key, name text)",
        &[],
    )
    .await
    .unwrap();
    let b = bridge_with(db.clone(), users_registry(), None, Default::default());
    let r = import_js(
            &b,
            r#"{ table: "users", format: "jsonl", rows: [{ id: 1, name: "a" }, { id: 2, name: "b" }] }"#,
        )
        .await
        .unwrap();
    let st = wait_terminal(&b, r["taskId"].as_str().unwrap()).await;
    assert_eq!(st["state"], json!("done"), "{st}");
    assert_eq!(st["rows"], json!(2), "{st}");
    assert_eq!(count_rows(&db, "users").await, 2);
}

/// skip：坏行进 samples（绝对行号），行不落库，好行落库。
#[tokio::test(flavor = "current_thread")]
async fn import_skip_bad_rows_samples_absolute_line() {
    let (_d, db) = sqlite_file().await;
    db.exec_with_params(
        "create table users (id integer primary key, name text)",
        &[],
    )
    .await
    .unwrap();
    let (_bd, blobs) = tmp_blobs();
    let b = bridge_with(
        db.clone(),
        users_registry(),
        Some(blobs.clone()),
        Default::default(),
    );
    // header=1, d1=2, d2=3, bad=4, d4=5
    let csv = b"id,name\n1,a\n2,b\nabc,c\n4,d\n";
    blobs
        .default()
        .unwrap()
        .put("in/x.csv", csv, None)
        .await
        .unwrap();
    let r = import_js(
        &b,
        r#"{ table: "users", blobKey: "in/x.csv", format: "csv", onError: "skip" }"#,
    )
    .await
    .unwrap();
    let st = wait_terminal(&b, r["taskId"].as_str().unwrap()).await;
    assert_eq!(st["state"], json!("done"), "{st}");
    assert_eq!(st["errors"], json!(1), "{st}");
    assert_eq!(st["samples"][0]["line"], json!(4), "{st}");
    // sqlite 约束错原文（code 20: datatype mismatch）
    assert!(
        st["samples"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("mismatch"),
        "{st}"
    );
    // plan 口径「raw 含原格」：整行 values 的 JSON 序列化，须含坏格原文
    assert!(
        st["samples"][0]["raw"].as_str().unwrap().contains("abc"),
        "{st}"
    );
    assert_eq!(count_rows(&db, "users").await, 3);
}

/// abort：第二批约束冲突 → Failed；sqlite 全单事务特例（修正案②/spec §4.3）
/// → 整回滚 rows=0、库空。「已提交批保留」语义属多批事务方言，由 T7 PG env 腿覆盖。
#[tokio::test(flavor = "current_thread")]
async fn import_abort_batch_rolls_back_full_rollback_sqlite() {
    let (_d, db) = sqlite_file().await;
    db.exec_with_params(
        "create table users (id integer primary key, name text)",
        &[],
    )
    .await
    .unwrap();
    let b = bridge_with(db.clone(), users_registry(), None, Default::default());
    // 批1 = id 1..100；批2 = 51..150（51 撞已插入主键）
    let mut ids: Vec<i64> = (1..=100).collect();
    ids.extend(51..=150);
    let rows: Vec<String> = ids
        .iter()
        .map(|i| format!(r#"{{ "id": {i}, "name": "n{i}" }}"#))
        .collect();
    let args = format!(
        r#"{{ table: "users", format: "jsonl", batchSize: 100, rows: [{}] }}"#,
        rows.join(",")
    );
    let r = import_js(&b, &args).await.unwrap();
    let st = wait_terminal(&b, r["taskId"].as_str().unwrap()).await;
    assert_eq!(st["state"], json!("failed"), "{st}");
    assert_eq!(st["rows"], json!(0), "{st}");
    assert!(!st["error"].as_str().unwrap().is_empty(), "{st}");
    assert_eq!(count_rows(&db, "users").await, 0);
}

/// Gated 访问器：批 exec 前 sleep——取消落批间 → 已提交批保留、未提交不落库。
struct Gated {
    inner: Arc<dyn DataAccessor>,
    gate: Duration,
}
struct GatedTx {
    tx: tokio::sync::Mutex<Box<dyn TxSession>>,
    gate: Duration,
}
#[async_trait::async_trait]
impl TxSession for GatedTx {
    async fn query(
        &self,
        sql: &str,
        params: &[Value],
    ) -> crate::bridge::BridgeResult<Vec<super::super::db::Row>> {
        self.tx.lock().await.query(sql, params).await
    }
    async fn exec(&self, sql: &str, params: &[Value]) -> crate::bridge::BridgeResult<i64> {
        tokio::time::sleep(self.gate).await;
        self.tx.lock().await.exec(sql, params).await
    }
    async fn commit(&self) -> crate::bridge::BridgeResult<()> {
        self.tx.lock().await.commit().await
    }
    async fn rollback(&self) -> crate::bridge::BridgeResult<()> {
        self.tx.lock().await.rollback().await
    }
}
#[async_trait::async_trait]
impl DataAccessor for Gated {
    fn dialect(&self) -> super::super::db::Dialect {
        self.inner.dialect()
    }
    async fn begin(&self) -> crate::bridge::BridgeResult<Box<dyn TxSession>> {
        Ok(Box::new(GatedTx {
            tx: tokio::sync::Mutex::new(self.inner.begin().await?),
            gate: self.gate,
        }))
    }
    async fn query_with_params(
        &self,
        sql: &str,
        params: &[Value],
    ) -> crate::bridge::BridgeResult<Vec<super::super::db::Row>> {
        self.inner.query_with_params(sql, params).await
    }
    async fn exec_with_params(
        &self,
        sql: &str,
        params: &[Value],
    ) -> crate::bridge::BridgeResult<i64> {
        self.inner.exec_with_params(sql, params).await
    }
}

/// cancel 落批间：Cancelled；sqlite 全单事务特例（修正案②）→ 整回滚
/// rows=0、库空。批间等待 progress≥100 证明取消发生在执行中途。
#[tokio::test(flavor = "current_thread")]
async fn import_cancel_between_batches_full_rollback_sqlite() {
    let (_d, db) = sqlite_file().await;
    db.exec_with_params(
        "create table users (id integer primary key, name text)",
        &[],
    )
    .await
    .unwrap();
    let gated: Arc<dyn DataAccessor> = Arc::new(Gated {
        inner: db.clone(),
        gate: Duration::from_millis(120),
    });
    let b = bridge_with(gated, users_registry(), None, Default::default());
    let rows: Vec<String> = (1..=250)
        .map(|i| format!(r#"{{ "id": {i}, "name": "n{i}" }}"#))
        .collect();
    let args = format!(
        r#"{{ table: "users", format: "jsonl", batchSize: 100, rows: [{}] }}"#,
        rows.join(",")
    );
    let r = import_js(&b, &args).await.unwrap();
    let task_id = r["taskId"].as_str().unwrap().to_string();
    // 等第一批提交（rows==100）再取消
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        let js = format!(
            r#"(async () => {{ const st = await db.importStatus({task_id:?}); json.ok(st); }})().catch(e => json.fail(500, String(e)));"#,
        );
        let cap = b.run_with(&js, Default::default()).await.unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        if v["data"]["rows"].as_u64().unwrap() >= 100 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "first batch never committed"
        );
        tokio::task::yield_now().await;
    }
    let js = format!(
        r#"(async () => {{ await db.importCancel({task_id:?}); json.ok({{}}); }})().catch(e => json.fail(500, String(e)));"#,
    );
    let cap = b.run_with(&js, Default::default()).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    let st = wait_terminal(&b, &task_id).await;
    assert_eq!(st["state"], json!("cancelled"), "{st}");
    assert_eq!(st["rows"], json!(0), "{st}");
    assert_eq!(count_rows(&db, "users").await, 0);
}

/// replace：先删后插单事务——预置 2 旧行，导入 3 新行 → 库内恰 3 新行。
#[tokio::test(flavor = "current_thread")]
async fn import_replace_mode_deletes_then_inserts() {
    let (_d, db) = sqlite_file().await;
    db.exec_with_params(
        "create table users (id integer primary key, name text)",
        &[],
    )
    .await
    .unwrap();
    db.exec_with_params(
        "insert into users (id, name) values (1, 'old1'), (2, 'old2')",
        &[],
    )
    .await
    .unwrap();
    let b = bridge_with(db.clone(), users_registry(), None, Default::default());
    let r = import_js(
            &b,
            r#"{ table: "users", format: "jsonl", mode: "replace", rows: [{ id: 3, name: "n3" }, { id: 4, name: "n4" }, { id: 5, name: "n5" }] }"#,
        )
        .await
        .unwrap();
    let st = wait_terminal(&b, r["taskId"].as_str().unwrap()).await;
    assert_eq!(st["state"], json!("done"), "{st}");
    assert_eq!(st["rows"], json!(3), "{st}");
    let rows = db
        .query_with_params("select id from users order by id", &[])
        .await
        .unwrap();
    let ids: Vec<i64> = rows.iter().map(|r| r["id"].as_i64().unwrap()).collect();
    assert_eq!(ids, vec![3, 4, 5]);
}

/// sqlite rowid upsert 幂等：同数据跑两遍行数不变。
#[tokio::test(flavor = "current_thread")]
async fn import_upsert_idempotent_twice() {
    let (_d, db) = sqlite_file().await;
    db.exec_with_params(
        "create table users (id integer primary key, name text)",
        &[],
    )
    .await
    .unwrap();
    let b = bridge_with(db.clone(), users_registry(), None, Default::default());
    let args = r#"{ table: "users", format: "jsonl", mode: "upsert", conflictKeys: ["rowid"], rows: [{ rowid: 1, id: 1, name: "a" }, { rowid: 2, id: 2, name: "b" }] }"#;
    let r1 = import_js(&b, args).await.unwrap();
    let st = wait_terminal(&b, r1["taskId"].as_str().unwrap()).await;
    assert_eq!(st["state"], json!("done"), "{st}");
    let r2 = import_js(&b, args).await.unwrap();
    let st = wait_terminal(&b, r2["taskId"].as_str().unwrap()).await;
    assert_eq!(st["state"], json!("done"), "{st}");
    assert_eq!(count_rows(&db, "users").await, 2);
}

/// 终态后 cancel 不翻盘：Done 赢（cancel 幂等返回 true 但 state 保持 done）。
#[tokio::test(flavor = "current_thread")]
async fn import_cancel_vs_final_commit_race() {
    let (_d, db) = sqlite_file().await;
    db.exec_with_params(
        "create table users (id integer primary key, name text)",
        &[],
    )
    .await
    .unwrap();
    let b = bridge_with(db.clone(), users_registry(), None, Default::default());
    let r = import_js(
        &b,
        r#"{ table: "users", format: "jsonl", rows: [{ id: 1, name: "a" }] }"#,
    )
    .await
    .unwrap();
    let task_id = r["taskId"].as_str().unwrap().to_string();
    let st = wait_terminal(&b, &task_id).await;
    assert_eq!(st["state"], json!("done"), "{st}");
    let js = format!(
        r#"(async () => {{ const r = await db.importCancel({task_id:?}); json.ok(r); }})().catch(e => json.fail(500, String(e)));"#,
    );
    let cap = b.run_with(&js, Default::default()).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    let st = wait_terminal(&b, &task_id).await;
    assert_eq!(st["state"], json!("done"), "{st}");
}

/// Final-fix 1（review Critical）blob 面：数据记录带引号内换行且**后续还有记录**
/// ——字节推进 off-by-phys 会吃掉下一记录首字节（损坏/整任务失败）。
#[tokio::test(flavor = "current_thread")]
async fn import_blob_csv_quoted_newline_mid_file_keeps_following_rows() {
    let (_d, db) = sqlite_file().await;
    db.exec_with_params(
        "create table users (id integer primary key, name text)",
        &[],
    )
    .await
    .unwrap();
    let (_bd, blobs) = tmp_blobs();
    let b = bridge_with(
        db.clone(),
        users_registry(),
        Some(blobs.clone()),
        Default::default(),
    );
    // header=1；记录1（第 2-3 物理行）含引号内换行；记录2 从第 4 行起
    let csv = b"id,name\n1,\"a\nb\"\n2,c\n";
    blobs
        .default()
        .unwrap()
        .put("in/q.csv", csv, None)
        .await
        .unwrap();
    let r = import_js(
        &b,
        r#"{ table: "users", blobKey: "in/q.csv", format: "csv" }"#,
    )
    .await
    .unwrap();
    let st = wait_terminal(&b, r["taskId"].as_str().unwrap()).await;
    assert_eq!(st["state"], json!("done"), "{st}");
    assert_eq!(st["rows"], json!(2), "{st}");
    assert_eq!(count_rows(&db, "users").await, 2);
}

/// Final-fix 2（review Important）：max_bytes 读中强制——超长无换行记录不得
/// 一直缓冲到 EOF（防 OOM）；错误点名 max_bytes。
#[tokio::test(flavor = "current_thread")]
async fn blob_source_enforces_max_bytes_mid_record() {
    let (_bd, blobs) = tmp_blobs();
    // 首记录完整（open 需要首窗可解析）；第二条记录 ~1.5MiB 无换行 → 跨窗读到超闸
    let mut csv: Vec<u8> = b"1,a\n2,\"".to_vec();
    csv.extend(std::iter::repeat_n(b'x', 1536 * 1024));
    csv.extend_from_slice(b"\"\n");
    blobs
        .default()
        .unwrap()
        .put("in/huge.csv", &csv, None)
        .await
        .unwrap();
    let registry = SchemaRegistry::new(); // 未登记 → Unknown 类型，列名直通
    let mut src = super::CsvBlobSource::open(
        blobs.default().unwrap(),
        "in/huge.csv",
        false,
        &["id".to_string(), "txt".to_string()],
        &registry,
        "whatever",
        super::super::db::Dialect::Sqlite,
        1024 * 1024, // max_bytes：第二条记录必然读超
    )
    .await
    .unwrap();
    // 第一条记录正常
    assert!(matches!(src.next_row().await, Ok(Some(_))));
    // 第二条：读中拒（点名 max_bytes），而非缓冲到 EOF
    match src.next_row().await {
        Err(super::SourceErr::Io(e)) => assert!(e.contains("max_bytes"), "{e}"),
        _ => panic!("expected max_bytes Io error mid-read"),
    }
}

/// Final-fix 3（review Important）：sqlite 变量上限 32766（< PG/MySQL 65535）——
/// 7 列宽表默认批 5000 行 = 35000 变量，超 sqlite 上限；批收敛须方言感知。
#[tokio::test(flavor = "current_thread")]
async fn import_sqlite_wide_table_default_batch_converged() {
    let (_d, db) = sqlite_file().await;
    db.exec_with_params(
            "create table wide (id integer primary key, a text, b text, c text, d text, e text, f text)",
            &[],
        )
        .await
        .unwrap();
    let registry =
        SchemaRegistry::new().table("wide", &["id"], &["id", "a", "b", "c", "d", "e", "f"]);
    let b = bridge_with(db.clone(), registry, None, Default::default());
    let rows: Vec<String> = (1..=5000)
            .map(|i| format!(r#"{{ "id": {i}, "a": "a{i}", "b": "b{i}", "c": "c{i}", "d": "d{i}", "e": "e{i}", "f": "f{i}" }}"#))
            .collect();
    let args = format!(
        r#"{{ table: "wide", format: "jsonl", rows: [{}] }}"#,
        rows.join(",")
    );
    let r = import_js(&b, &args).await.unwrap();
    let st = wait_terminal(&b, r["taskId"].as_str().unwrap()).await;
    assert_eq!(st["state"], json!("done"), "{st}");
    assert_eq!(st["rows"], json!(5000), "{st}");
    assert_eq!(count_rows(&db, "wide").await, 5000);
}

/// blob 源超 max_bytes：中途 Failed，错误点名 max_bytes（不静默截断）。
#[tokio::test(flavor = "current_thread")]
async fn import_blob_over_max_bytes_fails_named() {
    let (_d, db) = sqlite_file().await;
    db.exec_with_params(
        "create table users (id integer primary key, name text)",
        &[],
    )
    .await
    .unwrap();
    let (_bd, blobs) = tmp_blobs();
    let section = super::super::db_policy::DbImportSection {
        max_bytes: 1024 * 1024, // 段有效下限
        ..Default::default()
    };
    let b = bridge_with(db.clone(), users_registry(), Some(blobs.clone()), section);
    // ~1.4MB 源（id 到 100000 → 每行 ~12B）
    let mut csv: Vec<u8> = b"id,name\n".to_vec();
    for i in 0..100_000i64 {
        csv.extend_from_slice(format!("{i},n{i}\n").as_bytes());
    }
    assert!(csv.len() as u64 > 1024 * 1024);
    blobs
        .default()
        .unwrap()
        .put("in/big.csv", &csv, None)
        .await
        .unwrap();
    let r = import_js(
        &b,
        r#"{ table: "users", blobKey: "in/big.csv", format: "csv" }"#,
    )
    .await
    .unwrap();
    let st = wait_terminal(&b, r["taskId"].as_str().unwrap()).await;
    assert_eq!(st["state"], json!("failed"), "{st}");
    assert!(st["error"].as_str().unwrap().contains("max_bytes"), "{st}");
}

// ---- env-gated 真库腿（OJ_TEST_PG / OJ_TEST_MYSQL 缺 → 该腿跳过）----

/// env-gated 真库连接；缺 env 的腿打印 skip 并略过。
async fn real_dbs() -> Vec<(&'static str, Arc<dyn DataAccessor>)> {
    let mut out = Vec::new();
    for env in ["OJ_TEST_PG", "OJ_TEST_MYSQL"] {
        match std::env::var(env) {
            Ok(url) if !url.is_empty() => {
                let db = crate::bridge::SqlxAccessor::arc(&url).await.unwrap();
                out.push((env, db));
            }
            _ => eprintln!("skip: {env} unset"),
        }
    }
    out
}

/// env 腿目标表注册：id BigInt + f1..f9 Text；shared → 租户守卫不介入
/// （这些腿验性能/幂等/取消，守卫矩阵由离线腿覆盖）。
/// 表名按测试分发（cargo 并行跑测试共享同一真库，撞名即互踩）。
fn imp_registry(table: &str) -> SchemaRegistry {
    let mut cols: Vec<(&str, ColumnType)> = vec![("id", ColumnType::BigInt)];
    cols.extend((1..=9).map(|i| {
        (
            &*Box::leak(format!("f{i}").into_boxed_str()),
            ColumnType::Text,
        )
    }));
    SchemaRegistry::new().table_owned_shared_typed("tester", table, &["id"], &cols, true)
}

/// DDL 无值可参数化（静态列清单）；表名来自测试内常量。
async fn imp_reset(db: &Arc<dyn DataAccessor>, table: &str) {
    db.exec_with_params(&format!("drop table if exists {table}"), &[])
        .await
        .unwrap();
    db.exec_with_params(
            &format!("create table {table} (id bigint primary key, f1 text, f2 text, f3 text, f4 text, f5 text, f6 text, f7 text, f8 text, f9 text)"),
            &[],
        )
        .await
        .unwrap();
}

/// 500k×10 CSV 全链路性能腿：Done、rows=500k、<60s（实测秒数入 ledger）。
#[tokio::test(flavor = "current_thread")]
async fn real_pg_mysql_import_perf_500k_under_60s() {
    const N: i64 = 500_000;
    let mut csv: Vec<u8> = b"id,f1,f2,f3,f4,f5,f6,f7,f8,f9\n".to_vec();
    csv.reserve((N as usize) * 36);
    for i in 0..N {
        csv.extend_from_slice(
            format!("{i},v{i}a,v{i}b,v{i}c,v{i}d,v{i}e,v{i}f,v{i}g,v{i}h,v{i}i\n").as_bytes(),
        );
    }
    for (env, db) in real_dbs().await {
        imp_reset(&db, "imp_perf").await;
        let (_bd, blobs) = tmp_blobs();
        blobs
            .default()
            .unwrap()
            .put("in/perf.csv", &csv, None)
            .await
            .unwrap();
        let b = bridge_with(
            db.clone(),
            imp_registry("imp_perf"),
            Some(blobs),
            Default::default(),
        );
        let started = tokio::time::Instant::now();
        let r = import_js(
            &b,
            r#"{ table: "imp_perf", blobKey: "in/perf.csv", format: "csv" }"#,
        )
        .await
        .unwrap();
        let st = wait_terminal_until(&b, r["taskId"].as_str().unwrap(), 90).await;
        let elapsed = started.elapsed();
        assert_eq!(st["state"], json!("done"), "{env}: {st}");
        assert_eq!(st["rows"], json!(N), "{env}: {st}");
        eprintln!("{env}: 500k rows import in {elapsed:.1?}");
        assert!(
            elapsed < Duration::from_secs(60),
            "{env}: {elapsed:.1?} >= 60s"
        );
        assert_eq!(count_rows(&db, "imp_perf").await, N);
        db.exec_with_params("drop table imp_perf", &[])
            .await
            .unwrap();
    }
}

/// upsert 幂等：同数据 import 两遍，行数不变（PG ON CONFLICT / MySQL ODKU）。
#[tokio::test(flavor = "current_thread")]
async fn real_pg_mysql_upsert_idempotent() {
    const N: i64 = 3_000;
    let mut csv: Vec<u8> = b"id,f1\n".to_vec();
    for i in 0..N {
        csv.extend_from_slice(format!("{i},n{i}\n").as_bytes());
    }
    for (env, db) in real_dbs().await {
        imp_reset(&db, "imp_up").await;
        let (_bd, blobs) = tmp_blobs();
        blobs
            .default()
            .unwrap()
            .put("in/up.csv", &csv, None)
            .await
            .unwrap();
        let b = bridge_with(
            db.clone(),
            imp_registry("imp_up"),
            Some(blobs),
            Default::default(),
        );
        for round in 1..=2 {
            let r = import_js(
                    &b,
                    r#"{ table: "imp_up", blobKey: "in/up.csv", format: "csv", mode: "upsert", conflictKeys: ["id"] }"#,
                )
                .await
                .unwrap();
            let st = wait_terminal(&b, r["taskId"].as_str().unwrap()).await;
            assert_eq!(st["state"], json!("done"), "{env} round{round}: {st}");
            assert_eq!(st["rows"], json!(N), "{env} round{round}: {st}");
            assert_eq!(count_rows(&db, "imp_up").await, N, "{env} round{round}");
        }
        db.exec_with_params("drop table imp_up", &[]).await.unwrap();
    }
}

/// cancel 落批间：Cancelled、已提交批保留（rows>0、库行数==rows、< 总量）。
#[tokio::test(flavor = "current_thread")]
async fn real_pg_mysql_cancel_mid_import() {
    const N: i64 = 100_000;
    let mut csv: Vec<u8> = b"id,f1\n".to_vec();
    for i in 0..N {
        csv.extend_from_slice(format!("{i},n{i}\n").as_bytes());
    }
    for (env, db) in real_dbs().await {
        imp_reset(&db, "imp_cancel").await;
        let (_bd, blobs) = tmp_blobs();
        blobs
            .default()
            .unwrap()
            .put("in/cancel.csv", &csv, None)
            .await
            .unwrap();
        let b = bridge_with(
            db.clone(),
            imp_registry("imp_cancel"),
            Some(blobs),
            Default::default(),
        );
        let r = import_js(
            &b,
            r#"{ table: "imp_cancel", blobKey: "in/cancel.csv", format: "csv", batchSize: 100 }"#,
        )
        .await
        .unwrap();
        let task_id = r["taskId"].as_str().unwrap().to_string();
        // 等第一批提交（rows>=100）再取消——批间窗口
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            let js = format!(
                r#"(async () => {{ const st = await db.importStatus({task_id:?}); json.ok(st); }})().catch(e => json.fail(500, String(e)));"#,
            );
            let cap = b.run_with(&js, Default::default()).await.unwrap();
            let v: Value = serde_json::from_slice(&cap.body).unwrap();
            if v["data"]["rows"].as_u64().unwrap() >= 100 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{env}: first batch never committed"
            );
            tokio::task::yield_now().await;
        }
        let js = format!(
            r#"(async () => {{ await db.importCancel({task_id:?}); json.ok({{}}); }})().catch(e => json.fail(500, String(e)));"#,
        );
        let cap = b.run_with(&js, Default::default()).await.unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{env}: {v}");
        let st = wait_terminal_until(&b, &task_id, 30).await;
        assert_eq!(st["state"], json!("cancelled"), "{env}: {st}");
        let committed = st["rows"].as_u64().unwrap();
        assert!(committed > 0 && committed < N as u64, "{env}: {st}");
        assert_eq!(
            count_rows(&db, "imp_cancel").await as u64,
            committed,
            "{env}: table rows != committed"
        );
        db.exec_with_params("drop table imp_cancel", &[])
            .await
            .unwrap();
    }
}
