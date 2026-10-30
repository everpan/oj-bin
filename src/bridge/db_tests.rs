use super::*;

/// 量纲钉子（变异测试）：`elapsed_ms` 的 `*`→`+`/`/` 变异必死。
/// sleep(3ms) 的下界由时钟语义保证、上界千倍宽松——这不是「固定 sleep 当同步」
/// 的 flake 形态，被测对象就是计时本身。
#[test]
fn elapsed_ms_scales_to_millis() {
    let start = std::time::Instant::now();
    std::thread::sleep(std::time::Duration::from_millis(3));
    let ms = elapsed_ms(start);
    assert!(ms >= 3.0, "ms {ms}");
    assert!(ms < 1_000.0, "ms {ms}");
}
use crate::bridge::Extras;
use crate::bridge::client_db::ClientDbRegistry;
use crate::bridge::db_backend::DbBackendRegistry;
use crate::bridge::db_policy::{
    AllowList, DbAccess, DbAccessOverride, DbAccessSection, DbClientSection,
};
use crate::bridge::{Bridge, InMemoryAccessor, InMemoryKV, SchemaRegistry};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

/// 守护测试（0.1.67，评审 A1）：把「裸 SQL 出口 ⊆ {query, exec, stream_open}」
/// 从文档承诺变成 CI 资产——枚举 op 注册表的全部 op_db_*，新增 op 必须先过
/// 本白名单（属裸 SQL 出口则插桩 normalize_and_trace，否则更新此表并说明）。
///
/// `op_db_allowed`（v0.1.89）：访问控制自省出口，**不执行** SQL，故无需插桩
/// normalize_and_trace——登记为已知非裸 SQL 出口。
#[test]
fn raw_sql_op_exits_are_pinned() {
    const WHITELIST: [&str; 27] = [
        "op_db_allowed",
        "op_db_as_system",
        "op_db_as_tenant",
        "op_db_catalog",
        "op_db_dialect",
        "op_db_exec",
        // G3（v0.1.108）：经 db_query::query_impl 内 normalize_and_trace 插桩（占位符统一），
        // 本身不直接触驱动——登记为已插桩出口。
        "op_db_explain",
        "op_db_has",
        "op_db_list_columns",
        "op_db_list_databases",
        "op_db_list_foreign_keys",
        "op_db_list_indexes",
        "op_db_list_schemas",
        "op_db_list_tables",
        "op_db_next_seq",
        "op_db_query",
        "op_db_query_build",
        "op_db_query_sql",
        "op_db_sql_profile",
        "op_db_stream_abort",
        "op_db_stream_close",
        "op_db_stream_info",
        "op_db_stream_next",
        "op_db_stream_open",
        "op_db_tx_begin",
        "op_db_tx_commit",
        "op_db_tx_rollback",
    ];
    let src = include_str!("mod.rs");
    let mut found: Vec<&str> = Vec::new();
    for line in src.lines() {
        let t = line.trim();
        let Some((module, op)) = t.split_once("::") else {
            continue;
        };
        if !matches!(
            module,
            "db" | "db_tx"
                | "db_query"
                | "db_explain"
                | "db_stream"
                | "db_introspect"
                | "db_seq"
                | "query"
        ) {
            continue;
        }
        let op = op.trim_end_matches(',');
        if op.starts_with("op_db_") {
            found.push(op);
        }
    }
    found.sort_unstable();
    found.dedup();
    assert_eq!(
        found, WHITELIST,
        "op 注册表出现新 op_db_*：属裸 SQL 出口必须插桩 normalize_and_trace（占位符统一），否则更新白名单"
    );
}

/// `db.dialect()`：报引擎名；tx 回调对象同面（v0.1.61，`_shared/search` 依赖此分派）。
#[tokio::test(flavor = "current_thread")]
async fn db_dialect_reports_engine_name() {
    let db = crate::bridge::SqlxAccessor::arc("sqlite::memory:")
        .await
        .unwrap();
    let b = Bridge::with_dbs_and_loader(
        std::collections::HashMap::from([("default".to_string(), db as _)]),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
        None,
        Default::default(),
    );
    let cap = b
        .run_with(
            r#"(async () => {
                     const inTx = await db.tx(async (tx) => tx.dialect());
                     json.ok({ d: await db.dialect(), inTx });
                   })().catch(e => json.fail(500, String(e)));"#,
            crate::bridge::RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert_eq!(v["data"]["d"], "sqlite", "{v}");
    assert_eq!(v["data"]["inTx"], "sqlite", "tx 对象同样暴露 dialect：{v}");
}

/// `db.nextSeq`：单语句原子取号（sqlite 离线；PG/MySQL 的真库并发见 env-gated 用例）。
#[tokio::test(flavor = "current_thread")]
async fn next_seq_allocates_densely_and_creates_table() {
    let db = crate::bridge::SqlxAccessor::arc("sqlite::memory:")
        .await
        .unwrap();
    let b = Bridge::with_dbs_and_loader(
        std::collections::HashMap::from([("default".to_string(), db as _)]),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
        None,
        Default::default(),
    );
    let cap = b
        .run_with(
            r#"(async () => {
                     const a = [];
                     for (let i = 0; i < 3; i++) a.push(await db.nextSeq("proj"));
                     const other = await db.nextSeq("other");
                     // 事务内取号：搭车同一连接，且不随回滚而回退（序列语义）
                     const inTx = await db.tx(async (tx) => {
                       const x = await tx.nextSeq("proj");
                       return x;
                     });
                     json.ok({ a, other, inTx });
                   })().catch(e => json.fail(500, String(e)));"#,
            crate::bridge::RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert_eq!(v["data"]["a"], json!([1, 2, 3]), "同名序列稠密递增：{v}");
    assert_eq!(v["data"]["other"], json!(1), "不同序列互不干扰：{v}");
    assert_eq!(v["data"]["inTx"], json!(4), "事务内搭车同一序列：{v}");
}

/// 并发取号不重号（sqlite 单连接串行化；真库并发见 PG/MySQL env-gated）。
#[tokio::test(flavor = "current_thread")]
async fn next_seq_is_concurrency_safe_offline() {
    let db = crate::bridge::SqlxAccessor::arc("sqlite::memory:")
        .await
        .unwrap();
    let b = Bridge::with_dbs_and_loader(
        std::collections::HashMap::from([("default".to_string(), db as _)]),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
        None,
        Default::default(),
    );
    let cap = b
        .run_with(
            r#"(async () => {
                     const ps = [];
                     for (let i = 0; i < 25; i++) ps.push(db.nextSeq("burst"));
                     const vs = await Promise.all(ps);
                     vs.sort((x, y) => x - y);
                     let dense = true;
                     for (let i = 0; i < vs.length; i++) if (vs[i] !== i + 1) dense = false;
                     json.ok({ n: vs.length, dense, first: vs[0], last: vs[vs.length - 1] });
                   })().catch(e => json.fail(500, String(e)));"#,
            crate::bridge::RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert_eq!(v["data"]["n"], json!(25), "{v}");
    assert_eq!(v["data"]["dense"], json!(true), "并发取号应稠密无重复：{v}");
}

// 真库并发取号**不在本文件测**：经由 deno_core 连发 op 会撞既有缺陷
// （见 CHANGELOG「已知债」：并发超过 sqlx 池上限 / await 后再发 op → op 驱动
// `RefCell already borrowed` abort）。原子性改由插件层真库用例证明：
// `plugins/oj-db-postgres` 的 `real_postgres_next_seq_is_atomic_under_concurrency`
// 与 `plugins/oj-db-mysql` 的 `real_mysql_next_seq_*`（env-gated）。

/// 债④回归（env-gated，真库）：**在 `db.tx` 内首次使用新序列名**必须成功，
/// 且不得把调用方事务搞成 aborted（架构评审 P1-2 的验收）。
///
/// 背景：早期实现是「先试 → 失败才建表 → 在同一事务会话上重试」。PG 里事务内任何失败语句都会
/// 让事务进入 aborted 态（后续一律 `25P02`），于是**首次在 tx 内取号必然失败并毒化调用方事务**。
/// 现改为「每库一次的先确保（DDL 走池）」——本用例先 `drop table` 强制走首次路径来钉住它。
/// flavor 必须 current_thread：deno op 内 deno_unsync::spawn 断言 CurrentThread，
/// multi_thread 下 env 一设即 SIGABRT（该用例此前从未真跑过——env 未设静默 skip 假绿）。
#[tokio::test(flavor = "current_thread")]
async fn next_seq_first_use_inside_tx_on_real_db() {
    let url = std::env::var("OJ_TEST_PG")
        .or_else(|_| std::env::var("OJ_TEST_MYSQL"))
        .unwrap_or_else(|_| {
            eprintln!("skip: OJ_TEST_PG / OJ_TEST_MYSQL unset");
            String::new()
        });
    if url.is_empty() {
        return;
    }
    let db = crate::bridge::SqlxAccessor::arc(&url).await.unwrap();
    // 清场：删掉平台序列表，强制本用例走「首次使用（含建表）」路径。
    let _ = db
        .exec_with_params("drop table if exists _oj_sequences", &[])
        .await;
    let b = Bridge::with_dbs_and_loader(
        std::collections::HashMap::from([("default".to_string(), db as _)]),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
        None,
        Default::default(),
    );
    let cap = b
        .run_with(
            r#"(async () => {
                     const r = await db.tx(async (tx) => {
                       const a = await tx.nextSeq("in_tx_first");   // 首次：建表在池上完成
                       const b = await tx.nextSeq("in_tx_first");   // 同一事务内再取一次
                       return { a, b };
                     });
                     json.ok(r);
                   })().catch(e => json.fail(500, String(e)));"#,
            crate::bridge::RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "tx 内首次取号必须成功（不得毒化事务）：{v}");
    assert_eq!(v["data"]["a"], json!(1), "{v}");
    assert_eq!(v["data"]["b"], json!(2), "{v}");
}

/// PR-2 Phase A 真库回归（env-gated）：`db.stream` 在真实 PG / MySQL 上的逐行结果，与
/// `db.query` 全量**一致**（顺序 + 内容），且流走完哨兵正常、无残留注册表条目。
/// 仅当 `OJ_TEST_PG` / `OJ_TEST_MYSQL` 设了 DSN 才跑；都未设则 skip。
/// flavor 同上必须 current_thread（multi_thread 即 SIGABRT，存量假绿已修）。
#[tokio::test(flavor = "current_thread")]
async fn stream_matches_query_on_real_db() {
    let pg = std::env::var("OJ_TEST_PG").ok().filter(|s| !s.is_empty());
    let my = std::env::var("OJ_TEST_MYSQL")
        .ok()
        .filter(|s| !s.is_empty());
    if pg.is_none() && my.is_none() {
        eprintln!("skip: OJ_TEST_PG / OJ_TEST_MYSQL unset");
        return;
    }
    if let Some(url) = pg {
        stream_eq_check(&url, "pg").await;
    }
    if let Some(url) = my {
        stream_eq_check(&url, "mysql").await;
    }
}

/// F5（批次 4 / REQ-04，v0.1.105，env-gated 真库矩阵腿）：清单面 + forSchema 在
/// 真实 PG / MySQL 上的跨方言验收——系统库过滤、schema 清单、forSchema 跨
/// schema/database 取表、畸形 scope 空结果。未设 OJ_TEST_PG / OJ_TEST_MYSQL 则
/// skip（诚实登记）。flavor 必须 current_thread（同存量真库用例）。
#[tokio::test(flavor = "current_thread")]
async fn listing_and_for_schema_matrix_pg_mysql() {
    // ---- PG 腿：schema 维度 + 集群 database 清单 ----
    if let Ok(url) = std::env::var("OJ_TEST_PG") {
        if !url.is_empty() {
            let db = crate::bridge::SqlxAccessor::arc(&url).await.unwrap();
            for s in [
                "drop schema if exists oj_req04 cascade",
                "create schema oj_req04",
                "create table oj_req04.t_req04(id int)",
            ] {
                db.exec_with_params(s, &[]).await.unwrap();
            }
            let b = Bridge::with_dbs_and_loader(
                std::collections::HashMap::from([("default".to_string(), db as _)]),
                Arc::new(InMemoryKV::new()),
                SchemaRegistry::new(),
                false,
                None,
                Default::default(),
            );
            let cap = b
                .run_with(
                    r#"(async () => {
                         const ss = await db.listSchemas();
                         const dbs = await db.listDatabases();
                         const scoped = await db.forSchema("oj_req04").listTables();
                         const unscoped = await db.listTables("t_req04");
                         const malformed = await db.forSchema("badname").listTables();
                         const names = (a) => a.map((x) => x.name);
                         json.ok({
                           hasSchema: names(ss).includes("oj_req04"),
                           filtered: !names(ss).includes("pg_catalog") && !names(ss).some((n) => n.startsWith("pg_toast")),
                           dbHasSelf: dbs.find((d) => d.name === "ojtest")?.isDefault === true,
                           noTemplates: !names(dbs).includes("template1"),
                           scoped, unscoped, malformed,
                         });
                       })().catch(e => json.fail(500, String(e)))"#,
                    crate::bridge::RequestInfo::default(),
                )
                .await
                .unwrap();
            let v: Value = serde_json::from_slice(&cap.body).unwrap();
            assert_eq!(v["code"], 0, "{v}");
            assert_eq!(
                v["data"]["hasSchema"],
                json!(true),
                "listSchemas 须含 oj_req04: {v}"
            );
            assert_eq!(v["data"]["filtered"], json!(true), "系统目录须过滤: {v}");
            assert_eq!(v["data"]["dbHasSelf"], json!(true), "{v}");
            assert_eq!(v["data"]["noTemplates"], json!(true), "{v}");
            assert_eq!(
                v["data"]["scoped"],
                json!(["t_req04"]),
                "forSchema 跨 schema 取表: {v}"
            );
            assert_eq!(
                v["data"]["unscoped"],
                json!([]),
                "默认 listTables 不得越 schema: {v}"
            );
            assert_eq!(
                v["data"]["malformed"],
                json!([]),
                "畸形 scope → 空结果: {v}"
            );
        }
    } else {
        eprintln!("skip: OJ_TEST_PG unset");
    }
    // ---- MySQL 腿：database 维度（schema≡database，两腿同源）----
    if let Ok(url) = std::env::var("OJ_TEST_MYSQL") {
        if !url.is_empty() {
            let db = crate::bridge::SqlxAccessor::arc(&url).await.unwrap();
            for s in [
                "drop database if exists oj_req04",
                "create database oj_req04",
                "create table oj_req04.t_req04(id int)",
            ] {
                db.exec_with_params(s, &[]).await.unwrap();
            }
            let b = Bridge::with_dbs_and_loader(
                std::collections::HashMap::from([("default".to_string(), db as _)]),
                Arc::new(InMemoryKV::new()),
                SchemaRegistry::new(),
                false,
                None,
                Default::default(),
            );
            let cap = b
                .run_with(
                    r#"(async () => {
                         const dbs = await db.listDatabases();
                         const ss = await db.listSchemas();
                         const scoped = await db.forSchema("oj_req04").listTables();
                         const unscoped = await db.listTables("t_req04");
                         const names = (a) => a.map((x) => x.name);
                         json.ok({
                           hasDb: names(dbs).includes("oj_req04"),
                           sysFiltered: !names(dbs).some((n) =>
                             ["information_schema", "mysql", "performance_schema", "sys"].includes(n)),
                           sameSource: JSON.stringify(names(dbs)) === JSON.stringify(names(ss)),
                           hasSelf: dbs.find((d) => d.name === "ojtest")?.isDefault === true,
                           scoped, unscoped,
                         });
                       })().catch(e => json.fail(500, String(e)))"#,
                    crate::bridge::RequestInfo::default(),
                )
                .await
                .unwrap();
            let v: Value = serde_json::from_slice(&cap.body).unwrap();
            assert_eq!(v["code"], 0, "{v}");
            assert_eq!(
                v["data"]["hasDb"],
                json!(true),
                "listDatabases 须含 oj_req04: {v}"
            );
            assert_eq!(
                v["data"]["sysFiltered"],
                json!(true),
                "mysql 系统库须过滤: {v}"
            );
            assert_eq!(
                v["data"]["sameSource"],
                json!(true),
                "schema≡database 同源: {v}"
            );
            assert_eq!(v["data"]["hasSelf"], json!(true), "{v}");
            assert_eq!(
                v["data"]["scoped"],
                json!(["t_req04"]),
                "forSchema 跨库取表: {v}"
            );
            assert_eq!(
                v["data"]["unscoped"],
                json!([]),
                "默认 listTables 不得越库: {v}"
            );
        }
    } else {
        eprintln!("skip: OJ_TEST_MYSQL unset");
    }
}

/// REQ-08 §3.1–3.3（env-gated 真库，走完整 op 链）：routines 元数据补全 +
/// `catalog("ddl")` 对象路由——PG 例程回退（pg_get_functiondef）+ 视图补壳；
/// MySQL 对象探测 → SHOW CREATE VIEW/FUNCTION。
#[tokio::test(flavor = "current_thread")]
async fn real_catalog_routines_and_ddl_routing_pg_mysql() {
    // ---- PG 腿 ----
    if let Ok(url) = std::env::var("OJ_TEST_PG") {
        if !url.is_empty() {
            let db = crate::bridge::SqlxAccessor::arc(&url).await.unwrap();
            for s in [
                "drop function if exists f_req08(int,int)",
                "drop view if exists v_req08",
                "create function f_req08(a int, b int) returns int language sql as $$ select a + b $$",
                "create view v_req08 as select 1 as x",
            ] {
                db.exec_with_params(s, &[]).await.unwrap();
            }
            let b = Bridge::with_dbs_and_loader(
                std::collections::HashMap::from([("default".to_string(), db as _)]),
                Arc::new(InMemoryKV::new()),
                SchemaRegistry::new(),
                false,
                None,
                Default::default(),
            );
            let cap = b
                .run_with(
                    r#"(async () => {
                         const fnDdl = await db.catalog("ddl", "f_req08");
                         const vw = await db.catalog("ddl", "v_req08");
                         const routines = await db.catalog("routines");
                         const r = (routines.items || []).find((x) => x.name === "f_req08");
                         json.ok({ fn: fnDdl.items[0], vw: vw.items[0], r });
                       })().catch(e => json.fail(500, String(e)))"#,
                    crate::bridge::RequestInfo::default(),
                )
                .await
                .unwrap();
            let v: Value = serde_json::from_slice(&cap.body).unwrap();
            assert_eq!(v["code"], 0, "{v}");
            let ddl = v["data"]["fn"]["ddl"].as_str().unwrap_or("");
            assert!(
                ddl.starts_with("CREATE OR REPLACE FUNCTION"),
                "例程 DDL 须可执行: {ddl}"
            );
            assert_eq!(v["data"]["fn"]["objectType"], json!("function"));
            let vddl = v["data"]["vw"]["ddl"].as_str().unwrap_or("");
            assert!(
                vddl.starts_with("CREATE OR REPLACE VIEW"),
                "视图 DDL 须补壳: {vddl}"
            );
            let r = &v["data"]["r"];
            assert!(r["args"].as_str().unwrap_or("").contains("a"), "{r}");
            assert_eq!(r["returns"], json!("integer"));
            assert!(r["source"].as_str().unwrap_or("").contains("a + b"), "{r}");
            assert_eq!(r["language"], json!("sql"));
        }
    } else {
        eprintln!("skip: OJ_TEST_PG unset");
    }
    // ---- MySQL 腿 ----
    if let Ok(url) = std::env::var("OJ_TEST_MYSQL") {
        if !url.is_empty() {
            let db = crate::bridge::SqlxAccessor::arc(&url).await.unwrap();
            db.exec_with_params("drop view if exists v_req08", &[])
                .await
                .unwrap();
            // CREATE FUNCTION 不在 MySQL prepared 协议支持集（1295）——setup 走原始
            // 文本协议；SHOW 族经 prepared 协议可用（同用户真实路径的读侧）。
            let pool = sqlx::MySqlPool::connect(&url).await.unwrap();
            for s in [
                "drop function if exists f_req08",
                "create function f_req08(a int) returns int deterministic return a",
            ] {
                sqlx::raw_sql(s).execute(&pool).await.unwrap();
            }
            db.exec_with_params("create view v_req08 as select 1 as x", &[])
                .await
                .unwrap();
            let b = Bridge::with_dbs_and_loader(
                std::collections::HashMap::from([("default".to_string(), db as _)]),
                Arc::new(InMemoryKV::new()),
                SchemaRegistry::new(),
                false,
                None,
                Default::default(),
            );
            let cap = b
                .run_with(
                    r#"(async () => {
                         const fnDdl = await db.catalog("ddl", "f_req08");
                         const vw = await db.catalog("ddl", "v_req08");
                         const routines = await db.catalog("routines");
                         const r = (routines.items || []).find((x) => x.name === "f_req08");
                         json.ok({ fn: fnDdl.items[0], vw: vw.items[0], r });
                       })().catch(e => json.fail(500, String(e)))"#,
                    crate::bridge::RequestInfo::default(),
                )
                .await
                .unwrap();
            let v: Value = serde_json::from_slice(&cap.body).unwrap();
            assert_eq!(v["code"], 0, "{v}");
            let ddl = v["data"]["fn"]["ddl"].as_str().unwrap_or("");
            assert!(
                ddl.to_uppercase().starts_with("CREATE") && ddl.to_uppercase().contains("FUNCTION"),
                "函数 DDL 须可执行: {ddl}"
            );
            let vddl = v["data"]["vw"]["ddl"].as_str().unwrap_or("");
            assert!(
                vddl.to_uppercase().starts_with("CREATE") && vddl.to_uppercase().contains("VIEW"),
                "视图 DDL 不再 400: {vddl}"
            );
            let r = &v["data"]["r"];
            assert_eq!(r["volatility"], json!("deterministic"));
            // 未显式声明 SECURITY INVOKER → MySQL 默认 DEFINER → true（如实映射）。
            assert_eq!(r["securityDefiner"], json!(true));
        }
    } else {
        eprintln!("skip: OJ_TEST_MYSQL unset");
    }
}

/// REQ-08 收口 + REQ-10 一期（env-gated 真库，PG 腿）：`db.catalog("triggers")`
/// timing/events 从 pg_get_triggerdef 令牌扫（BEFORE + INSERT OR UPDATE）；processlist /
/// users / roles 形状 + truncated 旗标；四类 kind 拒绝 scope（for_schema unsupported）。
#[tokio::test(flavor = "current_thread")]
async fn real_pg_triggers_and_ops_kinds() {
    let Ok(url) = std::env::var("OJ_TEST_PG") else {
        eprintln!("skip: OJ_TEST_PG unset");
        return;
    };
    if url.is_empty() {
        eprintln!("skip: OJ_TEST_PG empty");
        return;
    }
    let db = crate::bridge::SqlxAccessor::arc(&url).await.unwrap();
    for s in [
        "drop trigger if exists trg_req08b on t_req08b",
        "drop table if exists t_req08b",
        "drop function if exists f_req08b()",
        "create table t_req08b(id int)",
        "create function f_req08b() returns trigger language plpgsql as $$ begin return new; end $$",
        "create trigger trg_req08b before insert or update on t_req08b for each row execute function f_req08b()",
    ] {
        db.exec_with_params(s, &[]).await.unwrap();
    }
    let b = Bridge::with_dbs_and_loader(
        std::collections::HashMap::from([("default".to_string(), db.clone() as _)]),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
        None,
        Default::default(),
    );
    let cap = b
        .run_with(
            r#"(async () => {
                 const trg = await db.catalog("triggers");
                 const t = (trg.items || []).find((x) => x.name === "trg_req08b");
                 const pl = await db.catalog("processlist");
                 const us = await db.catalog("users");
                 const roles = await db.catalog("roles");
                 const scoped = await db.forSchema("public").catalog("processlist")
                   .then(() => null, (e) => ({ code: e.code, capability: e.data && e.data.capability }));
                 json.ok({ t, plN: (pl.items || []).length, plTrunc: pl.truncated,
                           users: (us.items || []).map((x) => x.name),
                           rolesN: (roles.items || []).length, scoped });
               })().catch(e => json.fail(500, String(e)))"#,
            crate::bridge::RequestInfo::default(),
        )
        .await
        .unwrap();
    for s in [
        "drop trigger if exists trg_req08b on t_req08b",
        "drop table if exists t_req08b",
        "drop function if exists f_req08b()",
    ] {
        db.exec_with_params(s, &[]).await.unwrap();
    }
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    let d = &v["data"];
    let t = &d["t"];
    assert_eq!(t["table"], json!("t_req08b"), "{t}");
    assert_eq!(t["timing"], json!("BEFORE"), "{t}");
    let mut ev = t["events"].as_array().expect("events").clone();
    ev.sort_by_key(|e| e.as_str().unwrap_or("").to_string());
    assert_eq!(Value::Array(ev), json!(["INSERT", "UPDATE"]), "{t}");
    assert_eq!(t["enabled"], json!(true), "{t}");
    assert!(d["plN"].as_u64().unwrap_or(0) >= 1, "{d}");
    assert_eq!(d["plTrunc"], json!(false), "{d}");
    let users = d["users"].as_array().expect("users");
    assert!(
        users.iter().any(|u| u == "postgres"),
        "users 须含 postgres: {d}"
    );
    assert!(d["scoped"]["code"] == "OJ_DB_UNSUPPORTED", "{d}");
    assert_eq!(d["scoped"]["capability"], json!("for_schema"), "{d}");
}

/// REQ-08 收口 + REQ-10 一期（env-gated 真库，MySQL 腿）：triggers 直映
/// information_schema 列（AFTER + INSERT，body/order 有值）；processlist / users /
/// roles 形状；scope 拒绝同 PG 腿。
#[tokio::test(flavor = "current_thread")]
async fn real_mysql_triggers_and_ops_kinds() {
    let Ok(url) = std::env::var("OJ_TEST_MYSQL") else {
        eprintln!("skip: OJ_TEST_MYSQL unset");
        return;
    };
    if url.is_empty() {
        eprintln!("skip: OJ_TEST_MYSQL empty");
        return;
    }
    let db = crate::bridge::SqlxAccessor::arc(&url).await.unwrap();
    for s in [
        "drop table if exists t_req08b",
        "create table t_req08b(id int)",
    ] {
        db.exec_with_params(s, &[]).await.unwrap();
    }
    // CREATE/DROP TRIGGER 同 CREATE FUNCTION 不在 prepared 支持集——setup 走原始文本协议。
    let pool = sqlx::MySqlPool::connect(&url).await.unwrap();
    sqlx::raw_sql(
        "drop trigger if exists trg_req08b; \
         create trigger trg_req08b after insert on t_req08b for each row set @oj_req08b = 1",
    )
    .execute(&pool)
    .await
    .unwrap();
    let b = Bridge::with_dbs_and_loader(
        std::collections::HashMap::from([("default".to_string(), db.clone() as _)]),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
        None,
        Default::default(),
    );
    let cap = b
        .run_with(
            r#"(async () => {
                 const trg = await db.catalog("triggers");
                 const t = (trg.items || []).find((x) => x.name === "trg_req08b");
                 const pl = await db.catalog("processlist");
                 const us = await db.catalog("users");
                 const roles = await db.catalog("roles");
                 const scoped = await db.forSchema("ojtest").catalog("triggers")
                   .then(() => null, (e) => ({ code: e.code, capability: e.data && e.data.capability }));
                 json.ok({ t, plN: (pl.items || []).length, plTrunc: pl.truncated,
                           users: (us.items || []).map((x) => x.name),
                           rolesOk: Array.isArray(roles.items), scoped });
               })().catch(e => json.fail(500, String(e)))"#,
            crate::bridge::RequestInfo::default(),
        )
        .await
        .unwrap();
    sqlx::raw_sql("drop trigger if exists trg_req08b")
        .execute(&pool)
        .await
        .unwrap();
    db.exec_with_params("drop table if exists t_req08b", &[])
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    let d = &v["data"];
    let t = &d["t"];
    assert_eq!(t["table"], json!("t_req08b"), "{t}");
    assert_eq!(t["timing"], json!("AFTER"), "{t}");
    assert_eq!(t["events"], json!(["INSERT"]), "{t}");
    assert!(
        t["body"].as_str().unwrap_or("").contains("@oj_req08b"),
        "{t}"
    );
    assert_eq!(t["enabled"], Value::Null, "{t}");
    assert!(d["plN"].as_u64().unwrap_or(0) >= 1, "{d}");
    assert_eq!(d["plTrunc"], json!(false), "{d}");
    let users = d["users"].as_array().expect("users");
    assert!(users.iter().any(|u| u == "root"), "users 须含 root: {d}");
    assert_eq!(d["rolesOk"], json!(true), "{d}");
    assert_eq!(d["scoped"]["code"], json!("OJ_DB_UNSUPPORTED"), "{d}");
    assert_eq!(d["scoped"]["capability"], json!("for_schema"), "{d}");
}

/// 缺陷修复（env-gated 真库矩阵腿）：listIndexes / listForeignKeys 的投影别名含
/// 保留字（column / unique / primary），MySQL 8 / PG 下裸投影必现语法错误；以及
/// scoped（forSchema）下 PG 的 scope 占位符序错致静默空结果
/// （BUG-pg-scoped-introspect-param-binding）。真库各建一对带普通索引 + PK +
/// 外键的表（默认库 + 独立 schema/库 两份），断言两件不报错且形状正确。
/// 未设 OJ_TEST_PG / OJ_TEST_MYSQL 则 skip（诚实登记；CI 离线防线 =
/// introspect::tests::reserved_word_aliases_quoted_per_dialect 与
/// scope_predicate_precedes_first_param）。
#[tokio::test(flavor = "current_thread")]
async fn list_indexes_and_foreign_keys_real_pg_mysql() {
    for (env, tag) in [("OJ_TEST_PG", "pg"), ("OJ_TEST_MYSQL", "mysql")] {
        let Ok(url) = std::env::var(env) else {
            eprintln!("skip: {env} unset");
            continue;
        };
        if url.is_empty() {
            eprintln!("skip: {env} empty");
            continue;
        }
        let db = crate::bridge::SqlxAccessor::arc(&url).await.unwrap();
        for s in [
            "drop table if exists oj_ixfk_c",
            "drop table if exists oj_ixfk_p",
            "create table oj_ixfk_p(id int primary key)",
            "create table oj_ixfk_c(id int primary key, p_id int, \
             foreign key (p_id) references oj_ixfk_p(id))",
            "create index oj_ixfk_c_ix on oj_ixfk_c(p_id)",
        ] {
            db.exec_with_params(s, &[]).await.unwrap();
        }
        // scoped 腿：独立 schema（pg）/ 独立库（mysql）再建同构一对表。
        let scoped: &[&str] = if tag == "pg" {
            &[
                "drop schema if exists oj_ixfk cascade",
                "create schema oj_ixfk",
                "create table oj_ixfk.s_ixfk_p(id int primary key)",
                "create table oj_ixfk.s_ixfk_c(id int primary key, p_id int, \
                 foreign key (p_id) references oj_ixfk.s_ixfk_p(id))",
                "create index s_c_ix on oj_ixfk.s_ixfk_c(p_id)",
            ]
        } else {
            &[
                "drop database if exists oj_ixfk",
                "create database oj_ixfk",
                "create table oj_ixfk.s_ixfk_p(id int primary key)",
                "create table oj_ixfk.s_ixfk_c(id int primary key, p_id int, \
                 foreign key (p_id) references oj_ixfk.s_ixfk_p(id))",
                "create index s_c_ix on oj_ixfk.s_ixfk_c(p_id)",
            ]
        };
        for s in scoped {
            db.exec_with_params(s, &[]).await.unwrap();
        }
        let b = Bridge::with_dbs_and_loader(
            std::collections::HashMap::from([("default".to_string(), db as _)]),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            None,
            Default::default(),
        );
        let cap = b
            .run_with(
                r#"(async () => {
                     const idx = await db.listIndexes('oj_ixfk_c');
                     const fks = await db.listForeignKeys('oj_ixfk_c');
                     const ix = idx.find((i) => i.name === 'oj_ixfk_c_ix');
                     const pk = idx.find((i) => i.primary === true);
                     const sIdx = await db.forSchema('oj_ixfk').listIndexes('s_ixfk_c');
                     const sFks = await db.forSchema('oj_ixfk').listForeignKeys('s_ixfk_c');
                     const sIx = sIdx.find((i) => i.name === 's_c_ix');
                     json.ok({
                       ixCols: ix && ix.columns,
                       pkCols: pk && pk.columns,
                       fkLen: fks.length,
                       fkTable: fks[0] && fks[0].refTable,
                       fkCols: fks[0] && fks[0].columns,
                       fkRefs: fks[0] && fks[0].refColumns,
                       sIxCols: sIx && sIx.columns,
                       sFkLen: sFks.length,
                       sFkTable: sFks[0] && sFks[0].refTable,
                     });
                   })().catch(e => json.fail(500, String(e)))"#,
                crate::bridge::RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{tag}: 内省两件须不再语法错误: {v}");
        assert_eq!(v["data"]["ixCols"], json!(["p_id"]), "{tag}: {v}");
        assert_eq!(v["data"]["pkCols"], json!(["id"]), "{tag}: {v}");
        assert_eq!(v["data"]["fkLen"], json!(1), "{tag}: {v}");
        assert_eq!(v["data"]["fkTable"], json!("oj_ixfk_p"), "{tag}: {v}");
        assert_eq!(v["data"]["fkCols"], json!(["p_id"]), "{tag}: {v}");
        assert_eq!(v["data"]["fkRefs"], json!(["id"]), "{tag}: {v}");
        assert_eq!(
            v["data"]["sIxCols"],
            json!(["p_id"]),
            "{tag}: scoped 索引: {v}"
        );
        assert_eq!(v["data"]["sFkLen"], json!(1), "{tag}: scoped 外键: {v}");
        assert_eq!(
            v["data"]["sFkTable"],
            json!("s_ixfk_p"),
            "{tag}: scoped 外键引用表: {v}"
        );
    }
}

/// REQ-05 Q7（env-gated，三方言矩阵的 pg/mysql 腿）：语句级超时在**网络型后端**
/// 准点触发——5s 级慢语句（pg_sleep/sleep）+ 500ms 上限 → ~500ms 拿
/// OJ_DB_TIMEOUT（data.cancelled=false：网络方言弃 future ≠ 服务端取消，内置
/// SqlxAccessor 与插件轴同口径；真取消 = ABI 13 车）。
/// 未设 OJ_TEST_PG / OJ_TEST_MYSQL 则 skip（本地诚实登记：此腿未经亲跑）。
/// flavor 必须 current_thread（同存量真库用例：multi_thread 下 op 内 spawn 断言 SIGABRT）。
#[tokio::test(flavor = "current_thread")]
async fn statement_timeout_matrix_pg_mysql() {
    let cases = [
        ("OJ_TEST_PG", "select pg_sleep(5) as s"),
        ("OJ_TEST_MYSQL", "select sleep(5) as s"),
    ];
    let mut ran = 0;
    for (env, sleep_sql) in cases {
        let Ok(url) = std::env::var(env) else {
            eprintln!("skip: {env} unset");
            continue;
        };
        if url.is_empty() {
            eprintln!("skip: {env} empty");
            continue;
        }
        ran += 1;
        let db = crate::bridge::SqlxAccessor::arc(&url).await.unwrap();
        let b = Bridge::with_dbs_and_loader(
            std::collections::HashMap::from([("default".to_string(), db as _)]),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            None,
            crate::bridge::Extras {
                db_access: Some(Arc::new(crate::bridge::db_policy::DbAccessSection {
                    defaults: crate::bridge::db_policy::DbAccess {
                        statement_timeout_ms: Some(60_000),
                        ..Default::default()
                    },
                    ..Default::default()
                })),
                ..Default::default()
            },
        );
        // 配置 60s 兜底；调用侧收紧到 500ms（只能收紧阀的单向验证同在路上）。
        let js = format!(
            r#"(async () => {{
                     const t0 = Date.now();
                     try {{
                       await db.query({sleep_sql:?}, null, {{timeoutMs: 500}});
                       json.ok({{err: "no-timeout"}});
                     }} catch (e) {{
                       json.ok({{code: e.code, ms: Date.now() - t0, cancelled: e.data && e.data.cancelled}});
                     }}
                   }})().catch(e => json.fail(500, String(e)));"#,
        );
        let cap = b
            .run_with(&js, crate::bridge::RequestInfo::default())
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(
            v["data"]["code"],
            json!("OJ_DB_TIMEOUT"),
            "[{env}] 慢语句必须在 deadline 触发结构化超时：{v}"
        );
        let ms = v["data"]["ms"].as_i64().unwrap_or(999_999);
        assert!(
            ms < 4_000,
            "[{env}] 竞速必须在 ~500ms 触发而非 5s 跑完：{ms}ms"
        );
        // 插件轴诚实位：服务端真取消待 ABI 13 车（执行计划 spike 结论）。
        assert_eq!(v["data"]["cancelled"], json!(false), "[{env}] {v}");
    }
    if ran == 0 {
        eprintln!("statement_timeout_matrix_pg_mysql: 双腿皆 skip（env 未设）");
    }
}

/// 单库 `db.stream` == `db.query` 全量校验（真库回归辅助）。
async fn stream_eq_check(url: &str, tag: &str) {
    let db = crate::bridge::SqlxAccessor::arc(url).await.unwrap();
    let b = Bridge::with_dbs_and_loader(
        std::collections::HashMap::from([("default".to_string(), db as _)]),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
        None,
        Default::default(),
    );
    let cap = b
            .run_with(
                r#"(async () => {
                     await db.exec("drop table if exists _oj_stream_real");
                     await db.exec("create table _oj_stream_real (id integer, name text)");
                     // 内联受控值（int + 简单字符串）规避 PG 的 ? 占位符问题；_oj_ 前缀表免登记表白名单。
                     for (let i = 0; i < 20; i++) await db.exec("insert into _oj_stream_real (id, name) values (" + i + ", 'n" + i + "')");
                     const all = await db.query("select id, name from _oj_stream_real order by id");
                     const streamed = [];
                     for await (const row of db.stream("select id, name from _oj_stream_real order by id")) streamed.push(row);
                     json.ok({ n1: all.length, n2: streamed.length, eq: JSON.stringify(all) === JSON.stringify(streamed) });
                   })().catch(e => json.fail(500, String(e)));"#,
                crate::bridge::RequestInfo::default(),
            )
            .await
            .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "[{tag}] db.stream real-db failed: {v}");
    assert_eq!(v["data"]["n1"], json!(20), "[{tag}] {v}");
    assert_eq!(v["data"]["n2"], json!(20), "[{tag}] {v}");
    assert_eq!(
        v["data"]["eq"],
        json!(true),
        "[{tag}] db.stream must equal db.query on real db: {v}"
    );
}

// —— §3④ `_shared/search.ts` 三方言真库回归（v0.1.68 修复 pg 占位符）——
//
// search 共享模块此前自写 `$1`（likeFallback 多字段 OR、pg FTS 非 highlight），与
// 宿主统一占位符改写的参数计数校验冲突 → pg 上 `search.query` 一律报错
// `placeholder count 1 != params N`。修复后统一 `?` 交宿主改写。本用例把
// `sample/src/_platform/_shared/search.ts` 转译后内联进 bridge，三种数据库跑同一
// 脚本：多字段表（≥2 字段，验证绑定数量正确）× FTS 非 highlight/highlight ×
// 2 字/1 字 LIKE 回退 × remove 后零命中 × 大小写混合查询（LIKE 回退与 FTS 两路）。
// 需设 OJ_TEST_PG / OJ_TEST_MYSQL 才跑对应数据库分支（sqlite 内存库分支常跑）。

/// 转译 sample 的 search 共享模块为可内联 JS（script 上下文无模块系统：剥 export，
/// type/interface 已被转译剥离）。
fn search_shared_js() -> String {
    let root = crate::bridge::ffi::workspace_root();
    let path = root.join("sample/src/_platform/_shared/search.ts");
    let src = std::fs::read_to_string(&path).expect("read search.ts");
    let js = crate::bridge::transpile::transpile_src(&path, &src).expect("transpile search.ts");
    js.replace("export ", "")
}

/// search 三方言统一脚本：ensure/index/query(FTS+回退+高亮)/remove 全链路。
const SEARCH_SHARED_HARNESS: &str = r#"
(async () => {
  const T = "_oj_search_real";
  await db.exec(`drop table if exists ${T}`, []);
  await db.exec(`create table ${T} (id integer primary key, title text, body text)`, []);
  const search = createSearch(db);
  await search.ensureIndex(T, { fields: ["title", "body"] });
  await search.index(T, { id: 1, title: "部署手册", body: "生产 环境" });
  await search.index(T, { id: 2, title: "公告", body: "本周例会" });
  await search.index(T, { id: 3, title: "Release Notes", body: "Kafka Cluster" });
  const r = {};
  // ① FTS ≥3 字非 highlight（pg 非高亮路径修复前 count 1 != 2）
  r.fts = (await search.query(T, "部署手册")).map((h) => Number(h.id));
  // ② FTS highlight（pg ts_headline 为第三处占位符）
  const hl = await search.query(T, "部署手册", { highlight: true });
  r.hlIds = hl.map((h) => Number(h.id));
  r.hlMark = JSON.stringify((hl[0] && hl[0].fragments) || {}).includes("<mark>");
  // ③ 2 字 LIKE 回退，多字段 OR（修复前 pg count 1 != 2）
  r.two = (await search.query(T, "部署")).map((h) => Number(h.id));
  // ④ 1 字 LIKE 回退
  r.one = (await search.query(T, "公")).map((h) => Number(h.id));
  // ⑤ remove 后 FTS 不再命中
  await search.remove(T, 2);
  r.afterRemove = (await search.query(T, "本周例会")).map((h) => Number(h.id));
  // ⑥ 大小写混合：2 字回退（小写查大写原文 / 大写查小写查询词）——pg 用
  //    ILIKE 对齐 sqlite/mysql 的不敏感语义（修复前 pg 上两者均空）
  r.kaLower = (await search.query(T, "ka")).map((h) => Number(h.id));
  r.kaUpper = (await search.query(T, "KA")).map((h) => Number(h.id));
  // ⑦ 大小写混合：FTS 路径（trigram/'simple'/ngram 皆折叠大小写，大写查询词命中）
  r.ftsCase = (await search.query(T, "RELEASE")).map((h) => Number(h.id));
  json.ok(r);
})().catch(e => json.fail(500, String(e)));"#;

/// 单个数据库分支执行 + 断言（sqlite 内存库 / pg / mysql 共用）。
async fn search_shared_check(js: &str, url: &str, tag: &str) {
    let db = crate::bridge::SqlxAccessor::arc(url).await.unwrap();
    let b = Bridge::with_dbs_and_loader(
        std::collections::HashMap::from([("default".to_string(), db as _)]),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
        None,
        Default::default(),
    );
    let src = format!("{js}\n{SEARCH_SHARED_HARNESS}");
    let cap = b
        .run_with(&src, crate::bridge::RequestInfo::default())
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "[{tag}] search harness failed: {v}");
    assert_eq!(v["data"]["fts"], json!([1]), "[{tag}] FTS ≥3字: {v}");
    assert_eq!(v["data"]["hlIds"], json!([1]), "[{tag}] FTS highlight: {v}");
    assert_eq!(
        v["data"]["hlMark"],
        json!(true),
        "[{tag}] highlight <mark>: {v}"
    );
    assert_eq!(v["data"]["two"], json!([1]), "[{tag}] 2字 LIKE 回退: {v}");
    assert_eq!(v["data"]["one"], json!([2]), "[{tag}] 1字 LIKE 回退: {v}");
    assert_eq!(
        v["data"]["afterRemove"],
        json!([]),
        "[{tag}] remove 后: {v}"
    );
    // ⑥ 大小写：row3 body "Kafka Cluster"，row2 已 remove 不干扰
    assert_eq!(
        v["data"]["kaLower"],
        json!([3]),
        "[{tag}] 回退小写查询词: {v}"
    );
    assert_eq!(
        v["data"]["kaUpper"],
        json!([3]),
        "[{tag}] 回退大写查询词: {v}"
    );
    // ⑦ FTS 路径大小写折叠（trigram/'simple'/ngram）
    assert_eq!(
        v["data"]["ftsCase"],
        json!([3]),
        "[{tag}] FTS 大写查询词: {v}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn search_shared_module_on_real_dbs() {
    let js = search_shared_js();
    // sqlite 内存库分支：无 env 依赖，验证 JS 本体与三方言共用路径。
    search_shared_check(&js, "sqlite::memory:", "sqlite").await;
    let pg = std::env::var("OJ_TEST_PG").ok().filter(|s| !s.is_empty());
    let my = std::env::var("OJ_TEST_MYSQL")
        .ok()
        .filter(|s| !s.is_empty());
    if pg.is_none() && my.is_none() {
        eprintln!("skip: OJ_TEST_PG / OJ_TEST_MYSQL unset");
    }
    if let Some(url) = pg {
        search_shared_check(&js, &url, "pg").await;
    }
    if let Some(url) = my {
        search_shared_check(&js, &url, "mysql").await;
    }
}

/// 序列名越界（空 / 超 128）→ 明确报错。
#[tokio::test(flavor = "current_thread")]
async fn next_seq_rejects_bad_name() {
    let db = crate::bridge::SqlxAccessor::arc("sqlite::memory:")
        .await
        .unwrap();
    let b = Bridge::with_dbs_and_loader(
        std::collections::HashMap::from([("default".to_string(), db as _)]),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
        None,
        Default::default(),
    );
    let cap = b
        .run_with(
            r#"(async () => {
                     const e1 = await db.nextSeq("").then(() => "ok", e => String(e));
                     const e2 = await db.nextSeq("x".repeat(129)).then(() => "ok", e => String(e));
                     json.ok({ e1, e2 });
                   })().catch(e => json.fail(500, String(e)));"#,
            crate::bridge::RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert!(v["data"]["e1"].as_str().unwrap().contains("1..=128"), "{v}");
    assert!(v["data"]["e2"].as_str().unwrap().contains("1..=128"), "{v}");
}

#[test]
fn dialect_of_recognizes_prefixes() {
    assert_eq!(dialect_of("sqlite://x"), Dialect::Sqlite);
    assert_eq!(dialect_of("sqlite::memory:"), Dialect::Sqlite);
    assert_eq!(dialect_of("mysql://u@h/d"), Dialect::MySql);
    assert_eq!(dialect_of("postgres://h/d"), Dialect::Postgres);
    assert_eq!(dialect_of("postgresql://h/d"), Dialect::Postgres);
    // 未知前缀回落 sqlite
    assert_eq!(dialect_of("weird://x"), Dialect::Sqlite);
}

// ── T6：方言能力模型 —— known() 边界 + 两层模型 dialect_info() ──
#[test]
fn known_dialect_rejects_unknown_and_accepts_known() {
    // Given known("oracle") → None（未进入 known 子集即为开放契约下的「未知」）
    assert_eq!(Dialect::known("oracle"), None);
    assert_eq!(Dialect::known("sqlserver"), None);
    // Given known("postgres") → Some(Postgres) 等已知子集
    assert_eq!(Dialect::known("postgres"), Some(Dialect::Postgres));
    assert_eq!(Dialect::known("postgresql"), Some(Dialect::Postgres));
    assert_eq!(Dialect::known("mysql"), Some(Dialect::MySql));
    assert_eq!(Dialect::known("sqlite"), Some(Dialect::Sqlite));
}

#[test]
fn sqlite_dialect_info_is_default_anchor() {
    let info = sqlite_dialect_info();
    assert_eq!(info.name, "sqlite");
    assert_eq!(info.caps.placeholder, PlaceholderStyle::Q);
    assert_eq!(info.caps.id, IdStrategy::TwoStep);
    assert!(info.caps.stream);
    assert!(!info.caps.cancel);
    // 懒初始化幂等：多次取是同一 &'static
    assert!(std::ptr::eq(info, sqlite_dialect_info()));
}

#[test]
fn inmemory_accessor_and_tx_default_dialect_info_is_sqlite() {
    // Given 默认 accessor（InMemoryAccessor）When dialect_info() Then 与 sqlite 默认锚一致
    let da = InMemoryAccessor::new();
    assert_eq!(da.dialect_info().name, "sqlite");
    assert_eq!(da.dialect_info().caps, sqlite_dialect_info().caps);
    // 默认 TxSession（begin 产出的 InMemoryTx）走 default → 同样 sqlite 锚
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let tx = rt.block_on(da.begin()).unwrap();
    assert_eq!(tx.dialect_info().name, "sqlite");
    assert_eq!(tx.dialect_info().caps, sqlite_dialect_info().caps);
}

#[test]
fn active_tx_carries_dialect_info_snapshot() {
    // Given tx begin When 后续 in-tx 改写/exec 路径 Then 使用 ActiveTx.dialect_info
    // （begin 时已快照，而非再次查询 accessor）。此处钉死「快照随 ActiveTx 走」的契约。
    let info = DialectInfo {
        name: "postgres".to_string(),
        caps: DialectCaps {
            placeholder: PlaceholderStyle::Dollar,
            id: IdStrategy::Returning,
            stream: true,
            cancel: true,
        },
    };
    let tx = ActiveTx {
        db: "default".to_string(),
        session: tokio::sync::Mutex::new(Box::new(InMemoryTx {
            inner: Arc::new(RwLock::new(Default::default())),
        })),
        dialect_info: info.clone(),
    };
    // in-tx 路径（db.rs:501/621/918 的 Target::Tx(t) => t.dialect_info）读此快照
    assert_eq!(tx.dialect_info.name, "postgres");
    assert_eq!(tx.dialect_info.caps.placeholder, PlaceholderStyle::Dollar);
    assert_eq!(tx.dialect_info.caps.id, IdStrategy::Returning);
}

// ── T8：六门控点 —— 未知方言（不在 builder-known 子集）一律结构化 unsupported ──

/// 门控核心：oracle 不在 known 子集 → Err(BridgeError::unsupported)，
/// code==OJ_DB_UNSUPPORTED、data.capability==对应能力名；已知方言透传枚举。
#[test]
fn require_known_dialect_gates_unknown_and_passes_known() {
    let oracle = DialectInfo {
        name: "oracle".to_string(),
        caps: DialectCaps {
            placeholder: PlaceholderStyle::Colon,
            id: IdStrategy::None,
            stream: false,
            cancel: false,
        },
    };
    let err = require_known_dialect(&oracle, "query_build").unwrap_err();
    assert_eq!(
        err.category,
        crate::bridge::db_error::DbErrorCategory::Unsupported
    );
    assert_eq!(
        crate::bridge::db_error::code_of(err.category),
        "OJ_DB_UNSUPPORTED"
    );
    assert_eq!(
        err.data.get("capability").and_then(Value::as_str),
        Some("query_build")
    );
    assert_eq!(
        err.data.get("dialect").and_then(Value::as_str),
        Some("oracle")
    );
    assert!(err.message.contains("oracle"), "{}", err.message);
    assert!(err.message.contains("query_build"), "{}", err.message);

    // 已知方言正常透传
    let pg = sqlite_dialect_info().clone();
    assert_eq!(
        require_known_dialect(&pg, "query_build").unwrap(),
        Dialect::Sqlite
    );
    let my = DialectInfo {
        name: "mysql".to_string(),
        caps: DialectCaps {
            placeholder: PlaceholderStyle::Q,
            id: IdStrategy::TwoStep,
            stream: true,
            cancel: false,
        },
    };
    assert_eq!(
        require_known_dialect(&my, "query_build").unwrap(),
        Dialect::MySql
    );
}

/// 测试用「未知方言」accessor：声明 oracle，但 oj 不认识它的 builder（无 sea-query 构造器）。
struct OracleAccessor;
#[async_trait]
impl DataAccessor for OracleAccessor {
    async fn query_with_params(&self, _sql: &str, _params: &[Value]) -> BridgeResult<Vec<Row>> {
        // 裸 SQL 路径对开放方言可用（§5.5）——mock 如实返回空结果集。
        Ok(Vec::new())
    }
    async fn exec_with_params(&self, _sql: &str, _params: &[Value]) -> BridgeResult<i64> {
        // 同上：裸 exec 不打 known 子集门控，调用必须能到达 accessor。
        Ok(7)
    }
    fn dialect_info(&self) -> &DialectInfo {
        static INFO: OnceLock<DialectInfo> = OnceLock::new();
        INFO.get_or_init(|| DialectInfo {
            name: "oracle".to_string(),
            caps: DialectCaps {
                placeholder: PlaceholderStyle::Colon,
                id: IdStrategy::None,
                stream: false,
                cancel: false,
            },
        })
    }
}

/// Given oracle accessor When db.table(...).select().all()（query_build 路径）
/// Then 结构化 unsupported，message 点名 oracle 与能力名（不回落 Sqlite、不静默执行）。
#[tokio::test(flavor = "current_thread")]
async fn oracle_query_build_is_unsupported() {
    let b = Bridge::new(Arc::new(OracleAccessor), Arc::new(InMemoryKV::new()));
    let cap = b
            .run(r#"db.table("t").select(["id"]).all().then(r => json.ok(r)).catch(e => json.fail(500, String(e)));"#)
            .await
            .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 500);
    let msg = v["msg"].as_str().unwrap();
    assert!(msg.contains("oracle"), "{msg}");
    assert!(msg.contains("query_build"), "{msg}");
}

/// Given oracle accessor When db.listTables()（list_tables 门控）
/// Then 结构化 unsupported，message 点名 oracle 与 list_tables。
#[tokio::test(flavor = "current_thread")]
async fn oracle_list_tables_is_unsupported() {
    let b = Bridge::new(Arc::new(OracleAccessor), Arc::new(InMemoryKV::new()));
    let cap = b
        .run(r#"db.listTables().then(r => json.ok(r)).catch(e => json.fail(500, String(e)));"#)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 500);
    let msg = v["msg"].as_str().unwrap();
    assert!(msg.contains("oracle"), "{msg}");
    assert!(msg.contains("list_tables"), "{msg}");
}

/// 未知方言（oracle）下，三个结构内省 op 同样走 known 子集门控 → 结构化
/// `OJ_DB_UNSUPPORTED`、`data.capability` 点名对应能力（绝不回落 sqlite）。
#[tokio::test(flavor = "current_thread")]
async fn oracle_introspect_ops_are_unsupported() {
    let b = Bridge::new(Arc::new(OracleAccessor), Arc::new(InMemoryKV::new()));
    for (js, cap) in [
        (
            r#"db.listColumns('t').then(r => json.ok(r)).catch(e => json.fail(500, String(e)));"#,
            "list_columns",
        ),
        (
            r#"db.listIndexes('t').then(r => json.ok(r)).catch(e => json.fail(500, String(e)));"#,
            "list_indexes",
        ),
        (
            r#"db.listForeignKeys('t').then(r => json.ok(r)).catch(e => json.fail(500, String(e)));"#,
            "list_foreign_keys",
        ),
    ] {
        let resp = b.run(js).await.unwrap();
        let v: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(v["code"], 500, "{v}");
        let msg = v["msg"].as_str().unwrap();
        assert!(msg.contains("oracle"), "{msg}");
        assert!(msg.contains(cap), "{cap} not in {msg}");
    }
}

/// F2（批次 4 / REQ-04，v0.1.105）：清单面 sqlite 腿——`PRAGMA database_list`
/// 经 sqlx Any 落成 main（isDefault=true）；prefix 过滤命中/落空两态。
#[tokio::test(flavor = "current_thread")]
async fn list_schemas_databases_sqlite_leg() {
    let db = crate::bridge::SqlxAccessor::arc("sqlite::memory:")
        .await
        .unwrap();
    let b = Bridge::with_dbs_and_loader(
        std::collections::HashMap::from([("default".to_string(), db as _)]),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
        None,
        Default::default(),
    );
    let cap = b
        .run(
            r#"(async () => {
                     const ss = await db.listSchemas();
                     const ds = await db.listDatabases();
                     const hit = await db.listSchemas("ma");
                     const miss = await db.listSchemas("zz");
                     json.ok({ ss, ds, hit, miss });
                   })().catch(e => json.fail(500, String(e)))"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    let ss = &v["data"]["ss"];
    assert_eq!(ss[0]["name"], json!("main"), "{v}");
    assert_eq!(ss[0]["isDefault"], json!(true), "{v}");
    assert_eq!(v["data"]["ds"][0]["name"], json!("main"), "{v}");
    assert_eq!(v["data"]["hit"].as_array().unwrap().len(), 1, "{v}");
    assert_eq!(v["data"]["miss"].as_array().unwrap().len(), 0, "{v}");
}

/// F3（批次 4 / REQ-04，v0.1.105）：`resolve_scope` 纯函数矩阵——
/// None=原样；sqlite=结构化 unsupported 点名 for_schema；畸形（控制字符/超长）=
/// 空结果不泄漏；schema 名含 `.` 合法（与表名校验不同——库名允许点）；
/// mysql/pg 合法值 → Scoped，SQL 恰增一个参数位、原范围谓词清零。
#[test]
fn resolve_scope_matrix() {
    use crate::bridge::db_introspect::{Scope, resolve_scope};
    let base = "SELECT 1 WHERE table_schema = DATABASE() AND table_name = ?";
    let pgbase = "SELECT 1 WHERE n.nspname = current_schema() AND t.relname = ?";
    assert!(matches!(
        resolve_scope(base, Dialect::MySql, None).unwrap(),
        Scope::Plain
    ));
    let e = resolve_scope(base, Dialect::Sqlite, Some("main")).unwrap_err();
    assert!(e.to_string().contains("for_schema"), "{e}");
    assert!(matches!(
        resolve_scope(base, Dialect::MySql, Some("bad\u{1f}name")).unwrap(),
        Scope::Empty
    ));
    let long = "x".repeat(129);
    assert!(matches!(
        resolve_scope(base, Dialect::MySql, Some(&long)).unwrap(),
        Scope::Empty
    ));
    let ok = resolve_scope(base, Dialect::MySql, Some("my.db")).unwrap();
    let Scope::Scoped(sql, param) = ok else {
        panic!("must be Scoped")
    };
    assert!(!sql.contains("DATABASE()"), "{sql}");
    assert_eq!(sql.matches('?').count(), 2, "scope 参数位须增 1: {sql}");
    assert_eq!(param, "my.db");
    let pg = resolve_scope(pgbase, Dialect::Postgres, Some("public")).unwrap();
    let Scope::Scoped(pgsql, pgparam) = pg else {
        panic!("must be Scoped")
    };
    assert!(!pgsql.contains("current_schema()"), "{pgsql}");
    assert_eq!(pgparam, "public");
}

/// F3/F4（批次 4 / REQ-04，v0.1.105）：sqlite 无 schema 概念——`db.forSchema()`
/// 受限句柄的各内视件一律结构化 unsupported，capability 点名 for_schema，
/// message 点名 sqlite（不回落默认库、不静默给垃圾数据）。
#[tokio::test(flavor = "current_thread")]
async fn sqlite_for_schema_is_unsupported() {
    let db = crate::bridge::SqlxAccessor::arc("sqlite::memory:")
        .await
        .unwrap();
    let b = Bridge::with_dbs_and_loader(
        std::collections::HashMap::from([("default".to_string(), db as _)]),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
        None,
        Default::default(),
    );
    let cap = b
        .run(
            r#"(async () => {
                     const sc = db.forSchema("main");
                     const try_ = (p) => p.then(() => "ok").catch((e) => String(e));
                     const [tables, columns, catalog] = await Promise.all([
                       try_(sc.listTables()),
                       try_(sc.listColumns("t")),
                       try_(sc.catalog("views")),
                     ]);
                     json.ok({ tables, columns, catalog });
                   })().catch(e => json.fail(500, String(e)))"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    for k in ["tables", "columns", "catalog"] {
        let m = v["data"][k].as_str().unwrap();
        assert!(m.contains("for_schema"), "{k}: {m}");
        assert!(m.contains("sqlite"), "{k}: {m}");
    }
}

/// F2：未知方言（oracle）下清单面同样走 known 子集门控 → 结构化 unsupported，
/// message 点名 oracle 与 list_schemas / list_databases（绝不回落 sqlite）。
#[tokio::test(flavor = "current_thread")]
async fn oracle_listing_ops_are_unsupported() {
    let b = Bridge::new(Arc::new(OracleAccessor), Arc::new(InMemoryKV::new()));
    for (js, cap) in [
        (
            r#"db.listSchemas().then(r => json.ok(r)).catch(e => json.fail(500, String(e)));"#,
            "list_schemas",
        ),
        (
            r#"db.listDatabases().then(r => json.ok(r)).catch(e => json.fail(500, String(e)));"#,
            "list_databases",
        ),
    ] {
        let resp = b.run(js).await.unwrap();
        let v: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(v["code"], 500, "{v}");
        let msg = v["msg"].as_str().unwrap();
        assert!(msg.contains("oracle"), "{msg}");
        assert!(msg.contains(cap), "{cap} not in {msg}");
    }
}

/// U-14 批 a R.1-P5（开放方言命名用例）：Given oracle accessor When db.catalog("views")
/// Then 结构化 `OJ_DB_UNSUPPORTED` 信封，`data.capability=="views"`、message 点名 oracle。
#[tokio::test(flavor = "current_thread")]
async fn oracle_db_catalog_is_unsupported() {
    let b = Bridge::new(Arc::new(OracleAccessor), Arc::new(InMemoryKV::new()));
    let cap = b
            .run(
                r#"db.catalog("views").then(r => json.ok(r)).catch(e => json.fail(500, String(e), e.data));"#,
            )
            .await
            .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 500, "{v}");
    let msg = v["msg"].as_str().unwrap();
    assert!(msg.contains("oracle"), "{msg}");
    let data = &v["data"];
    assert_eq!(data["capability"], json!("views"), "{v}");
    assert_eq!(data["dialect"], json!("oracle"), "{v}");
}

/// Given oracle accessor When db.nextSeq("s")（next_seq 门控）
/// Then 结构化 unsupported，message 点名 oracle 与 next_seq。
#[tokio::test(flavor = "current_thread")]
async fn oracle_next_seq_is_unsupported() {
    let b = Bridge::new(Arc::new(OracleAccessor), Arc::new(InMemoryKV::new()));
    let cap = b
        .run(r#"db.nextSeq("s").then(r => json.ok(r)).catch(e => json.fail(500, String(e)));"#)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 500);
    let msg = v["msg"].as_str().unwrap();
    assert!(msg.contains("oracle"), "{msg}");
    assert!(msg.contains("next_seq"), "{msg}");
}

/// Given oracle accessor When db.exec / db.query（裸 SQL 路径）
/// Then 不门控、直达 accessor（设计 §5.5：raw 路径对开放方言全量可用）。
#[tokio::test(flavor = "current_thread")]
async fn oracle_raw_exec_and_query_are_available() {
    let b = Bridge::new(Arc::new(OracleAccessor), Arc::new(InMemoryKV::new()));
    let cap = b
            .run(r#"db.exec("update t set a = 1").then(r => json.ok(r)).catch(e => json.fail(500, String(e)));"#)
            .await
            .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert_eq!(v["data"], 7, "exec 必须直达 accessor: {v}");

    let cap = b
            .run(r#"db.query("select 1 from dual").then(r => json.ok(r)).catch(e => json.fail(500, String(e)));"#)
            .await
            .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert!(v["data"].as_array().is_some_and(|a| a.is_empty()), "{v}");
}

/// Given oracle accessor When db.nextSeq 抛结构化 unsupported Then JS 侧 `e.code` /
/// `e.data` / `e.retryable` / `e.sessionDisposition` 透传（data 经 normDbErr 从 JSON 串
/// 解析为对象、retryable 从 1/0 归一为布尔），handler 可据此结构化判别能力缺失，
/// 而非只能靠 message 文本匹配。
#[tokio::test(flavor = "current_thread")]
async fn oracle_error_exposes_code_and_data_to_js() {
    let b = Bridge::new(Arc::new(OracleAccessor), Arc::new(InMemoryKV::new()));
    let cap = b
            .run(
                r#"db.nextSeq("s").then(r => json.ok(r)).catch(e => json.ok({ code: e.code, data: e.data, retryable: e.retryable, sd: e.sessionDisposition, msg: e.message }));"#,
            )
            .await
            .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    // 信封 code 为 0（handler 成功返回）；结构化信息在 data 载荷内。
    assert_eq!(v["code"], 0, "{v}");
    let p = &v["data"];
    assert_eq!(p["code"], "OJ_DB_UNSUPPORTED");
    // e.data 已被 normDbErr 解析为对象（capability / dialect）。
    assert_eq!(p["data"]["capability"], "next_seq");
    assert_eq!(p["data"]["dialect"], "oracle");
    // retryable 从 1/0 归一为布尔；sessionDisposition 为小写串。
    assert_eq!(p["retryable"], false, "e.retryable 必须是布尔: {v}");
    assert_eq!(p["sd"], "keep", "e.sessionDisposition: {v}");
    assert!(p["msg"].as_str().unwrap().contains("oracle"), "{v}");
}

/// T9 文本冻结快照：既有方言的 db 错误抵达 JS 时 `e.message` **逐字节**等于插件给出的
/// message 原文。宿主不再叠 `ffi <op>: ` 前缀（设计 §5.1）——ABI 11 及以前该前缀与插件
/// 自身的 `db query: ` 叠加成 `ffi db query: db query: …` 双前缀；本条钉死去重后的文本，
/// 任何再改（含重新加前缀）都必须显式更新此断言并写 CHANGELOG。
#[test]
fn db_error_text_snapshot_is_frozen() {
    use deno_error::JsErrorClass;
    let raw = r#"{"category":"constraint","retryable":false,"session_disposition":"keep","message":"db query: Duplicate entry 'neo' for key 'PRIMARY'","data":{"vendor_code":1062}}"#;
    let js = to_js_err(Box::new(crate::bridge::db_error::parse_raw(raw)));
    let msg = JsErrorClass::get_message(&js);
    assert_eq!(
        msg, "db query: Duplicate entry 'neo' for key 'PRIMARY'",
        "既有 db 错误文本必须逐字节冻结（设计 §5.1：宿主不叠 ffi 前缀）"
    );
    // 反向断言：宿主前缀一旦回流，上面那条会红；这条把「为什么红」写清楚。
    assert!(!msg.starts_with("ffi "), "宿主不得再叠 ffi 前缀: {msg}");
    // 非信封（裸文本）路径同样原样透传，不额外加壳。
    let js = to_js_err("dialect not supported".into());
    assert_eq!(JsErrorClass::get_message(&js), "dialect not supported");
}

/// 回归：已知三方言的既有行为零改动（sqlite builder 正常出结果）。
/// 构造器走 with_opts + SchemaRegistry（db.table 查询构造器按白名单解析表/列）。
#[tokio::test(flavor = "current_thread")]
async fn known_sqlite_query_build_still_works() {
    let db = Arc::new(InMemoryAccessor::new());
    db.seed([json!({"id": 1, "name": "neo"})]);
    let registry = SchemaRegistry::new().table("t", &["id"], &["id", "name"]);
    let b = Bridge::with_opts(db, Arc::new(InMemoryKV::new()), registry, false);
    let cap = b
            .run(r#"db.table("t").select(["id","name"]).all().then(r => json.ok(r)).catch(e => json.fail(500, String(e)));"#)
            .await
            .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert_eq!(v["data"][0]["name"], "neo");
}

#[tokio::test(flavor = "current_thread")]
async fn query_propagates_accessor_error() {
    let db = Arc::new(InMemoryAccessor::new());
    db.seed([json!({"id": 1})]);
    db.set_error("boom");
    let b = Bridge::new(db, Arc::new(InMemoryKV::new()));
    let cap = b
        .run(r#"db.query("select 1").then(r => json.ok(r)).catch(e => json.fail(500, String(e)));"#)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 500);
    assert!(v["msg"].as_str().unwrap().contains("boom"), "{v}");

    // exec 同理
    let cap = b
            .run(r#"db.exec("delete from t").then(r => json.ok(r)).catch(e => json.fail(500, String(e)));"#)
            .await
            .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 500);
    assert!(v["msg"].as_str().unwrap().contains("boom"), "{v}");
}

#[tokio::test(flavor = "current_thread")]
async fn commit_without_active_tx_errors() {
    use crate::bridge::ReqState;
    use deno_core::OpState;
    // 无活跃事务时收尾（take_tx 缺失分支）→ 直接报 "no active transaction"。
    // commit/rollback 共用 take_tx，仅需 ReqState，不依赖任何 DataAccessor。
    let mut op_state = OpState::new(None::<deno_core::OpStackTraceCallback>);
    op_state.put(ReqState::default());
    let state = std::rc::Rc::new(std::cell::RefCell::new(op_state));
    let r = crate::bridge::db_tx::take_tx(&state, "default");
    assert!(r.is_err(), "expected error when no tx active");
    assert!(
        r.err()
            .unwrap()
            .to_string()
            .contains("no active transaction"),
        "expected no-active-tx error"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn cross_db_query_during_tx_rejected() {
    let def = Arc::new(InMemoryAccessor::new());
    def.seed([json!({"id": 1})]);
    let other = Arc::new(InMemoryAccessor::new());
    let b = Bridge::with_dbs(
        std::collections::HashMap::from([
            ("default".to_string(), def as Arc<dyn DataAccessor>),
            ("other".to_string(), other as Arc<dyn DataAccessor>),
        ]),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new().table("t", &["id"], &["id"]),
        false,
    );
    // 在 default 上开事务，再于事务内查询 other → 拒绝。
    let cap = b
        .run(
            r#"
                db.tx(async () => {
                  await DB("other").query("select 1");
                }).then(() => json.ok({})).catch(e => json.fail(409, String(e)));
            "#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 409, "{v}");
    assert!(
        v["msg"]
            .as_str()
            .unwrap()
            .contains("transaction active on db 'default'"),
        "{v}"
    );
}

// PR-2 Phase A：未覆盖 stream_query 的 accessor（如插件 FfiDataAccessor）走默认实现，
// 明确报错「backend does not support streaming (ABI 10 required)」。
struct NoStreamAccessor;
#[async_trait]
impl DataAccessor for NoStreamAccessor {
    async fn query_with_params(&self, _sql: &str, _params: &[Value]) -> BridgeResult<Vec<Row>> {
        Ok(vec![])
    }
    async fn exec_with_params(&self, _sql: &str, _params: &[Value]) -> BridgeResult<i64> {
        Ok(0)
    }
}

#[tokio::test(flavor = "current_thread")]
async fn default_stream_query_reports_unsupported() {
    let r = NoStreamAccessor.stream_query("select 1", &[]).await;
    assert!(r.is_err(), "未覆盖的 accessor 必须报错");
    let msg = r.err().map(|e| e.to_string()).unwrap_or_default();
    assert!(
        msg.contains("does not support streaming"),
        "错误文案须指向 ABI 10: {msg}"
    );
}

/// sqlite 流式结果与 `db.query` 全量一致（顺序/内容）。
#[tokio::test(flavor = "current_thread")]
async fn sqlite_stream_matches_query_full_result() {
    let db = crate::bridge::SqlxAccessor::arc("sqlite::memory:")
        .await
        .unwrap();
    let b = Bridge::with_dbs_and_loader(
        std::collections::HashMap::from([("default".to_string(), db as _)]),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
        None,
        Default::default(),
    );
    let cap = b
            .run_with(
                r#"(async () => {
                     await db.exec("create table t(id integer, name text)");
                     for (let i = 0; i < 10; i++) await db.exec("insert into t values (?, ?)", [i, "n" + i]);
                     const all = await db.query("select id, name from t order by id");
                     const streamed = [];
                     for await (const row of db.stream("select id, name from t order by id")) streamed.push(row);
                     json.ok({ n1: all.length, n2: streamed.length, eq: JSON.stringify(all) === JSON.stringify(streamed) });
                   })().catch(e => json.fail(500, String(e)));"#,
                crate::bridge::RequestInfo::default(),
            )
            .await
            .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert_eq!(v["data"]["n1"], json!(10), "{v}");
    assert_eq!(v["data"]["n2"], json!(10), "{v}");
    assert_eq!(v["data"]["eq"], json!(true), "流式结果与全量查询一致：{v}");
}

/// InMemoryAccessor 流式返回已 seed 的行（dev/fake 路径）。
#[tokio::test(flavor = "current_thread")]
async fn inmemory_stream_yields_seeded_rows() {
    let acc = Arc::new(InMemoryAccessor::new());
    acc.seed(vec![
        json!({"id": 1, "name": "a"}),
        json!({"id": 2, "name": "b"}),
    ]);
    let b = Bridge::with_dbs_and_loader(
        std::collections::HashMap::from([("default".to_string(), acc as Arc<dyn DataAccessor>)]),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
        None,
        Default::default(),
    );
    let cap = b
        .run_with(
            r#"(async () => {
                     const rows = [];
                     for await (const r of db.stream("ignored")) rows.push(r);
                     json.ok({ n: rows.length, first: rows[0] });
                   })().catch(e => json.fail(500, String(e)));"#,
            crate::bridge::RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert_eq!(v["data"]["n"], json!(2), "{v}");
    assert_eq!(v["data"]["first"]["id"], json!(1), "{v}");
}

// ===== 防护探测（probe）：守卫/资源回收不能只靠"插旗"声明，用对抗用例钉死 =====

/// 探测辅助：跑一段 JS，返回信封 data（code 必须 0）。
async fn run_probe(b: &Bridge, req: crate::bridge::RequestInfo, js: String) -> Value {
    let cap = b.run_with(&js, req).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    v["data"].clone()
}

fn stream_probe_js(sql: &str, params: &str) -> String {
    format!(
        r#"(async () => {{
                 try {{
                   const rows = [];
                   for await (const r of db.stream({sql}, {params})) rows.push(r);
                   json.ok({{ n: rows.length }});
                 }} catch (e) {{ json.ok({{ err: String(e) }}); }}
               }})().catch(e => json.fail(500, String(e)));"#
    )
}

/// 探测：租户守卫对 db.stream 必须与 db.query **同严**（Deny 模式四态全打）。
/// 若流式路径在守卫上抄近道（open 前漏 check_tenant_raw），下面至少一条红。
#[tokio::test(flavor = "current_thread")]
async fn probe_stream_tenant_guard_deny_four_states() {
    let db = crate::bridge::SqlxAccessor::arc("sqlite::memory:")
        .await
        .unwrap();
    db.exec_with_params(
        "create table t (id integer primary key, name text, tenant_id text)",
        &[],
    )
    .await
    .unwrap();
    db.exec_with_params(
        "insert into t (id, name, tenant_id) values (1,'a','t1'),(2,'b','t2')",
        &[],
    )
    .await
    .unwrap();
    let b = Bridge::with_dbs_and_loader(
        std::collections::HashMap::from([("default".to_string(), db as _)]),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new().table("t", &["id"], &["id", "name", "tenant_id"]),
        false,
        None,
        crate::bridge::Extras {
            sql_guard: crate::bridge::SqlGuard::Deny,
            ..Default::default()
        },
    );
    let run = |req, sql: &str, params: &str| {
        let js = stream_probe_js(sql, params);
        let b = &b;
        async move { run_probe(b, req, js).await }
    };
    let t1 = crate::bridge::RequestInfo {
        tenant_id: Some("t1".into()),
        ..Default::default()
    };
    // ① 租户受约束表、SQL 无 tenant_id 条件 → open 前拒绝（游标不得建立）
    let d = run(t1.clone(), r#""select * from t""#, "null").await;
    assert!(
        d["err"]
            .as_str()
            .unwrap_or_default()
            .contains("lacks tenant_id"),
        "① 流式必须同样吃租户守卫: {d}"
    );
    // ② 无租户头（含受约束表）→ require tenant context
    let d = run(
        crate::bridge::RequestInfo::default(),
        r#""select * from t where tenant_id = 't1'""#,
        "null",
    )
    .await;
    assert!(
        d["err"]
            .as_str()
            .unwrap_or_default()
            .contains("require tenant context"),
        "② 无租户头必须拒绝: {d}"
    );
    // ③ 条件 + 参数租户匹配 → 通过，且只见本租户的行
    let d = run(
        t1.clone(),
        r#""select id from t where tenant_id = ?""#,
        r#"["t1"]"#,
    )
    .await;
    assert_eq!(d["n"], json!(1), "③ 合法流式须放行且只见本租户行: {d}");
    // ④ 条件有、参数租户不符（deny 模式）→ 拒
    let d = run(t1, r#""select id from t where tenant_id = ?""#, r#"["t2"]"#).await;
    assert!(
        d["err"]
            .as_str()
            .unwrap_or_default()
            .contains("must include current tenant id"),
        "④ 参数租户不符必须拒绝: {d}"
    );
}

/// 探测：中途 abort 后连接必须回池——sqlite 池 max_connections(1)，游标泄漏 =
/// 后续同池查询挂死。整条 run_with 套超时哨兵：泄漏即红（不靠人眼看日志）。
#[tokio::test(flavor = "current_thread")]
async fn probe_stream_abort_releases_connection() {
    let db = crate::bridge::SqlxAccessor::arc("sqlite::memory:")
        .await
        .unwrap();
    let b = Bridge::with_dbs_and_loader(
        std::collections::HashMap::from([("default".to_string(), db as _)]),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
        None,
        Default::default(),
    );
    let cap = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        b.run_with(
            r#"(async () => {
                     await db.exec("create table t(id integer)");
                     for (let i = 0; i < 50; i++) await db.exec("insert into t values (?)", [i]);
                     const ac = new AbortController();
                     let n = 0;
                     try {
                       await db.stream("select id from t order by id", null, {
                         signal: ac.signal,
                         onRow: (r) => { n++; if (n === 3) ac.abort(); },
                       });
                     } catch (e) { /* 取消 = 流提前终止（拒绝或正常结束皆可） */ }
                     // abort 后同池再来一发：连接没回池这里挂死（max_connections(1) 哨兵）
                     const after = await db.query("select count(*) as c from t");
                     json.ok({ streamed: n, after: after[0].c });
                   })().catch(e => json.fail(500, String(e)));"#,
            crate::bridge::RequestInfo::default(),
        )
        .await
        .unwrap()
    })
    .await
    .expect("abort 后同池查询挂死 = 连接泄漏（sqlite max_connections(1) 哨兵）");
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    let n = v["data"]["streamed"].as_u64().unwrap();
    assert!((3..=50).contains(&n), "abort 须提前终止（n={n}）: {v}");
    assert_eq!(v["data"]["after"], json!(50), "abort 后连接须可用: {v}");
}

/// 探测：并发游标交叉——每请求流注册表按 stream_id 隔离，两个游标交错推进
/// 互不串行（若注册表被复用/串号，这里序列即错）。
#[tokio::test(flavor = "current_thread")]
async fn probe_stream_concurrent_cursors_interleave() {
    let acc = Arc::new(InMemoryAccessor::new());
    acc.seed(vec![json!({"id": 1}), json!({"id": 2})]);
    let b = Bridge::with_dbs_and_loader(
        std::collections::HashMap::from([("default".to_string(), acc as Arc<dyn DataAccessor>)]),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
        None,
        Default::default(),
    );
    let cap = b
            .run_with(
                r#"(async () => {
                     const a = db.stream("q")[Symbol.asyncIterator]();
                     const b2 = db.stream("q")[Symbol.asyncIterator]();
                     const a1 = await a.next();
                     const b1 = await b2.next();
                     const a2 = await a.next();
                     const b2v = await b2.next();
                     const aDone = (await a.next()).done;
                     const bDone = (await b2.next()).done;
                     json.ok({ a: [a1.value.id, a2.value.id], b: [b1.value.id, b2v.value.id], aDone, bDone });
                   })().catch(e => json.fail(500, String(e)));"#,
                crate::bridge::RequestInfo::default(),
            )
            .await
            .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert_eq!(
        v["data"]["a"],
        json!([1, 2]),
        "游标 a 序列不得被 b 串扰: {v}"
    );
    assert_eq!(
        v["data"]["b"],
        json!([1, 2]),
        "游标 b 序列不得被 a 串扰: {v}"
    );
    assert_eq!(v["data"]["aDone"], json!(true), "{v}");
    assert_eq!(v["data"]["bDone"], json!(true), "{v}");
}

/// 中途 abort：流干净终止（不 panic、连接回池）；abort 经 AbortSignal 的 abort 事件触发。
#[tokio::test(flavor = "current_thread")]
async fn stream_abort_terminates_without_panic() {
    let db = crate::bridge::SqlxAccessor::arc("sqlite::memory:")
        .await
        .unwrap();
    let b = Bridge::with_dbs_and_loader(
        std::collections::HashMap::from([("default".to_string(), db as _)]),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
        None,
        Default::default(),
    );
    let cap = b
        .run_with(
            r#"(async () => {
                     await db.exec("create table t(id integer)");
                     for (let i = 0; i < 1000; i++) await db.exec("insert into t values (?)", [i]);
                     const ac = new AbortController();
                     let count = 0;
                     try {
                       await db.stream("select * from t", [], {
                         signal: ac.signal,
                         onRow: () => { count++; if (count >= 3) ac.abort(); }
                       });
                       json.ok({ aborted: false, count });
                     } catch (e) {
                       json.ok({ aborted: true, count });
                     }
                   })().catch(e => json.fail(500, String(e)));"#,
            crate::bridge::RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    // 有界通道语义：abort = **干净提前结束**（缓冲行交付完即 done，不抛错）；
    // count 必须 < 1000（取消真的停止了后端拉取，而非灌满缓冲后照常跑完）。
    assert_eq!(
        v["data"]["aborted"],
        json!(false),
        "abort = 流提前结束（非错误）: {v}"
    );
    let count = v["data"]["count"].as_u64().unwrap();
    assert!(count >= 3, "至少已消费 3 行：{v}");
    assert!(
        count < 1000,
        "取消必须停止拉取（count={count} 不得跑完全表）: {v}"
    );
}

/// SQL 执行追踪（v0.1.51）：dev 开启后，本请求画像累计 SQL 事件，`db.sqlProfile()` 可读取，
/// 且参数默认脱敏（params 只记个数）。
#[tokio::test(flavor = "current_thread")]
async fn sql_trace_records_profile_and_redacts_params() {
    let db = crate::bridge::SqlxAccessor::arc("sqlite::memory:")
        .await
        .unwrap();
    db.exec_with_params("create table t (id integer primary key, v text)", &[])
        .await
        .unwrap();
    let extras = crate::bridge::Extras {
        sql_trace: crate::bridge::sql_trace::SqlTraceConfig {
            enabled: true,
            redact_params: true,
            slow_ms: 0.0,
            to_log: false,
        },
        ..Default::default()
    };
    let b = Bridge::with_dbs_and_loader(
        std::collections::HashMap::from([("default".to_string(), db as _)]),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
        None,
        extras,
    );
    let cap = b
        .run_with(
            r#"(async () => {
                     await db.exec("insert into t (v) values (?)", ["secret"]);
                     const rows = await db.query("select * from t where v = ?", ["secret"]);
                     json.ok({ n: rows.length, prof: db.sqlProfile() });
                   })().catch(e => json.fail(500, String(e)));"#,
            crate::bridge::RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    // 2 条 SQL：insert + select。
    assert_eq!(
        v["data"]["prof"]["count"],
        json!(2),
        "画像应含 2 条 SQL: {v}"
    );
    // 参数脱敏：只记个数，不记 "secret"。
    assert_eq!(
        v["data"]["prof"]["events"][0]["params"],
        json!("1 params"),
        "参数默认脱敏: {v}"
    );
    assert!(
        v["data"]["prof"]["events"][0]["sql"]
            .as_str()
            .unwrap()
            .contains("insert into t"),
        "事件含 SQL 文本: {v}"
    );
}

/// SQL 执行追踪关闭（release 默认）时不累计画像，`db.sqlProfile()` 返回空对象。
#[tokio::test(flavor = "current_thread")]
async fn sql_trace_off_yields_empty_profile() {
    let db = crate::bridge::SqlxAccessor::arc("sqlite::memory:")
        .await
        .unwrap();
    db.exec_with_params("create table t (id integer primary key)", &[])
        .await
        .unwrap();
    // Extras 默认 sql_trace.enabled = false（release 行为）。
    let b = Bridge::with_dbs_and_loader(
        std::collections::HashMap::from([("default".to_string(), db as _)]),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
        None,
        Default::default(),
    );
    let cap = b
        .run_with(
            r#"(async () => {
                     await db.exec("insert into t (id) values (1)");
                     json.ok({ prof: db.sqlProfile() });
                   })().catch(e => json.fail(500, String(e)));"#,
            crate::bridge::RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert_eq!(v["data"]["prof"], json!({}), "关闭时画像为空对象: {v}");
}

/// 红线探针：即使 `redact_params: false`，画像与响应信封 `_sql` 也永远脱敏（只记个数），
/// 参数原值绝不外泄到客户端（仅服务端 dev 日志可记）。
#[tokio::test(flavor = "current_thread")]
async fn sql_trace_envelope_always_redacts_even_when_redact_false() {
    let db = crate::bridge::SqlxAccessor::arc("sqlite::memory:")
        .await
        .unwrap();
    db.exec_with_params("create table t (id integer primary key, v text)", &[])
        .await
        .unwrap();
    let extras = crate::bridge::Extras {
        sql_trace: crate::bridge::sql_trace::SqlTraceConfig {
            enabled: true,
            redact_params: false, // 故意关脱敏：仅 dev 日志可记原值
            slow_ms: 0.0,
            to_log: false,
        },
        ..Default::default()
    };
    let b = Bridge::with_dbs_and_loader(
        std::collections::HashMap::from([("default".to_string(), db as _)]),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
        None,
        extras,
    );
    let cap = b
        .run_with(
            r#"(async () => {
                     await db.exec("insert into t (v) values (?)", ["SECRET_PLAINTEXT"]);
                     json.ok({ prof: db.sqlProfile() });
                   })().catch(e => json.fail(500, String(e)));"#,
            crate::bridge::RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    let params = v["data"]["prof"]["events"][0]["params"].as_str().unwrap();
    // 画像永远脱敏：只记个数，绝不出现原值。
    assert_eq!(params, "1 params", "画像应永远脱敏: {v}");
    let body = String::from_utf8_lossy(&cap.body);
    assert!(
        !body.contains("SECRET_PLAINTEXT"),
        "响应体/信封永不泄露参数原值: {body}"
    );
}

/// 红线探针：db.table().select().all() 等构造器路径也须进画像（engineer P1-1）。
#[tokio::test(flavor = "current_thread")]
async fn sql_trace_records_query_builder_path() {
    let db = crate::bridge::SqlxAccessor::arc("sqlite::memory:")
        .await
        .unwrap();
    db.exec_with_params("create table t (id integer primary key, v text)", &[])
        .await
        .unwrap();
    let extras = crate::bridge::Extras {
        sql_trace: crate::bridge::sql_trace::SqlTraceConfig {
            enabled: true,
            redact_params: true,
            slow_ms: 0.0,
            to_log: false,
        },
        ..Default::default()
    };
    let b = Bridge::with_dbs_and_loader(
        std::collections::HashMap::from([("default".to_string(), db as _)]),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new().table("t", &["id"], &["id", "v"]),
        false,
        None,
        extras,
    );
    let cap = b
        .run_with(
            r#"(async () => {
                     await db.table("t").select().all();
                     json.ok({ prof: db.sqlProfile() });
                   })().catch(e => json.fail(500, String(e)));"#,
            crate::bridge::RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert!(
        v["data"]["prof"]["count"].as_u64().unwrap() >= 1,
        "构造器路径应进画像: {v}"
    );
    assert!(
        v["data"]["prof"]["events"][0]["sql"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("select"),
        "画像应含 SELECT: {v}"
    );
}

// —— §3③ 真库 e2e（design §3③）：占位符统一改写 × 真实 postgres，经
// oj-db-postgres 插件后端（生产路径）。env-gated：OJ_TEST_PG 未设则静默 skip。
// 注意：改写发生在宿主 op 层（normalize_and_trace），对 DataAccessor 实现透明——
// 故插件后端与 core SqlxAccessor 走同一改写面；插件特有的方言缓存由 ffi mock 钉死。

/// 编译并加载 oj-db-postgres cdylib → DbBackend（进程内一次；DSN 未设返回 None）。
fn pg_backend() -> Option<Arc<dyn crate::bridge::DbBackend>> {
    let Ok(_dsn) = std::env::var("OJ_TEST_PG") else {
        eprintln!("skip: OJ_TEST_PG unset");
        return None;
    };
    static ONCE: std::sync::OnceLock<Option<Arc<dyn crate::bridge::DbBackend>>> =
        std::sync::OnceLock::new();
    ONCE.get_or_init(|| {
        let root = crate::bridge::ffi::workspace_root();
        let status = std::process::Command::new("cargo")
            .args(["build", "-p", "oj-db-postgres"])
            .current_dir(&root)
            .status()
            .expect("invoke cargo build for oj-db-postgres");
        assert!(status.success(), "oj-db-postgres build failed");
        let (prefix, ext) = if cfg!(target_os = "windows") {
            ("", "dll")
        } else if cfg!(target_os = "macos") {
            ("lib", "dylib")
        } else {
            ("lib", "so")
        };
        let built = root
            .join("target/debug")
            .join(format!("{prefix}oj_db_postgres.{ext}"));
        let dir = root
            .join("target/pg-e2e-plugins")
            .join(crate::bridge::ffi::triple());
        std::fs::create_dir_all(&dir).unwrap();
        let dst = dir.join(crate::bridge::ffi::plugin_file_name("db-postgres"));
        if !dst.exists() {
            std::fs::copy(&built, &dst).expect("copy oj-db-postgres artifact");
        }
        let loaded = crate::bridge::plugin_loader::load_scanned(
            &dir,
            crate::bridge::plugin_loader::host_context(),
            &|_, _| "{}".to_string(),
        )
        .expect("load oj-db-postgres plugin");
        loaded
            .iter()
            .find_map(|p| crate::bridge::plugin_loader::db_backend(p).ok().flatten())
    })
    .clone()
}

/// connect 真库（每测试一次连接，供 Bridge 注入）。
async fn pg_accessor() -> Option<Arc<dyn DataAccessor>> {
    let dsn = std::env::var("OJ_TEST_PG").ok()?;
    let be = pg_backend()?;
    let cfg_dir = crate::bridge::ffi::workspace_root().join("target/pg-e2e-cfg");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    Some(be.connect(&dsn, &cfg_dir).await.expect("connect pg"))
}

/// 原始报障场景 + 字面量不误伤 + 方言自报：裸 `?` 写一次，PG 跑通。
#[tokio::test(flavor = "current_thread")]
async fn pg_e2e_raw_question_mark_roundtrip() {
    let Some(da) = pg_accessor().await else {
        return;
    };
    let registry = SchemaRegistry::new().table("ph_e2e_a", &["id"], &["id", "name"]);
    let b = Bridge::with_opts(da, Arc::new(InMemoryKV::new()), registry, false);
    let cap = b
            .run_with(
                r#"(async () => {
                     await db.exec("create table if not exists ph_e2e_a (id integer primary key, name text)", []);
                     await db.exec("delete from ph_e2e_a", []);
                     await db.exec("insert into ph_e2e_a (id, name) values (?, ?)", [1, "a"]);
                     await db.exec("insert into ph_e2e_a (id, name) values (?, 'a?b?')", [2]);
                     const rows = await db.query("select id, name from ph_e2e_a where id = ? and name = ?", [1, "a"]);
                     return json.ok({ dialect: db.dialect(), rows });
                   })().catch((e) => json.fail(500, String(e)));"#,
                crate::bridge::RequestInfo::default(),
            )
            .await
            .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "裸 `?` 在 PG 必须跑通（原始报障场景）：{v}");
    assert_eq!(v["data"]["dialect"], json!("postgres"), "{v}");
    assert_eq!(v["data"]["rows"], json!([{"id": 1, "name": "a"}]), "{v}");
    // 字面量 `a?b?` 逐字相等——SQL 文本里的 `?` 未被改写误伤
    let cap = b
        .run_with(
            r#"db.query("select name from ph_e2e_a where id = ?", [2])
                     .then((rows) => json.ok({ name: rows[0].name }))
                     .catch((e) => json.fail(500, String(e)));"#,
            crate::bridge::RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert_eq!(v["data"]["name"], json!("a?b?"), "字面量 `?` 不得误伤：{v}");
}

/// `$1` + `?` 混用续号；tx 内裸 `?` 同场景（评审 P4）。
#[tokio::test(flavor = "current_thread")]
async fn pg_e2e_mixed_numbering_and_tx() {
    let Some(da) = pg_accessor().await else {
        return;
    };
    let registry = SchemaRegistry::new().table("ph_e2e_b", &["id"], &["id", "name"]);
    let b = Bridge::with_opts(da, Arc::new(InMemoryKV::new()), registry, false);
    let cap = b
            .run_with(
                r#"(async () => {
                     await db.exec("create table if not exists ph_e2e_b (id integer primary key, name text)", []);
                     await db.exec("delete from ph_e2e_b", []);
                     await db.exec("insert into ph_e2e_b (id, name) values (?, ?)", [1, "a"]);
                     await db.exec("insert into ph_e2e_b (id, name) values (?, ?)", [2, "b"]);
                     // `$1` 引 params[0]，`?` 续号为 $2 引 params[1]（vendored P() 兼容面）
                     const mixed = (await db.query("select name from ph_e2e_b where id = $1 or name = ? order by name", [2, "a"])).map((r) => r.name);
                     await db.tx(async (tx) => {
                       await tx.exec("insert into ph_e2e_b (id, name) values (?, ?)", [9, "tx"]);
                       const rows = await tx.query("select name from ph_e2e_b where id = ?", [9]);
                       if (rows[0].name !== "tx") throw new Error("tx query failed");
                     });
                     const after = await db.query("select count(*) c from ph_e2e_b where name = ?", ["tx"]);
                     return json.ok({ mixed, txCount: after[0].c });
                   })().catch((e) => json.fail(500, String(e)));"#,
                crate::bridge::RequestInfo::default(),
            )
            .await
            .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert_eq!(v["data"]["mixed"], json!(["a", "b"]), "{v}");
    assert_eq!(
        v["data"]["txCount"],
        json!(1),
        "tx 内裸 `?` 写入未提交/已提交面: {v}"
    );
}

/// stream 出口带 `?` 带参（评审 P4：改写覆盖三出口之一）。
#[tokio::test(flavor = "current_thread")]
async fn pg_e2e_stream_with_params() {
    let Some(da) = pg_accessor().await else {
        return;
    };
    let registry = SchemaRegistry::new().table("ph_e2e_c", &["id"], &["id", "name"]);
    let b = Bridge::with_opts(da, Arc::new(InMemoryKV::new()), registry, false);
    let cap = b
            .run_with(
                r#"(async () => {
                     await db.exec("create table if not exists ph_e2e_c (id integer primary key, name text)", []);
                     await db.exec("delete from ph_e2e_c", []);
                     await db.exec("insert into ph_e2e_c (id, name) values (?, ?)", [1, "a"]);
                     await db.exec("insert into ph_e2e_c (id, name) values (?, ?)", [2, "b"]);
                     const ids = [];
                     // INT4 列是插件已知不可解码面，cast text 规避（与占位符改写无关）
                     await db.stream("select cast(id as text) as id from ph_e2e_c where id >= ? order by id", [1], {
                       onRow: (r) => ids.push(r.id),
                     });
                     return json.ok({ ids });
                   })().catch((e) => json.fail(500, String(e)));"#,
                crate::bridge::RequestInfo::default(),
            )
            .await
            .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert_eq!(
        v["data"]["ids"],
        json!(["1", "2"]),
        "stream 出口裸 `?` 必须跑通: {v}"
    );
}

/// 计数不匹配 fail-loud：文案含两个数字 + 正解 hint（击穿验证报错本身，评审 P3/P4）。
#[tokio::test(flavor = "current_thread")]
async fn pg_e2e_count_mismatch_message() {
    let Some(da) = pg_accessor().await else {
        return;
    };
    let registry = SchemaRegistry::new().table("ph_e2e_d", &["id"], &["id", "name"]);
    let b = Bridge::with_opts(da, Arc::new(InMemoryKV::new()), registry, false);
    let cap = b
        .run_with(
            r#"db.query("insert into ph_e2e_d (id, name) values (?, ?)", [1])
                     .then(() => json.ok({}))
                     .catch((e) => json.fail(500, String(e)));"#,
            crate::bridge::RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    let msg = v["msg"].as_str().unwrap_or_default();
    assert_ne!(v["code"], 0, "计数不匹配必须报错: {v}");
    assert!(
        msg.contains("placeholder count 2 != params 1"),
        "文案须含两个数字: {msg}"
    );
    assert!(
        msg.contains("hint") && msg.contains("jsonb_exists"),
        "文案须含正解 hint: {msg}"
    );
}

/// sql_trace 记原始 SQL：改写后执行，画像里仍是 `?` 而非 `$n`（评审 P6）。
#[tokio::test(flavor = "current_thread")]
async fn pg_e2e_trace_records_original_sql() {
    let Some(da) = pg_accessor().await else {
        return;
    };
    let extras = crate::bridge::Extras {
        sql_trace: crate::bridge::sql_trace::SqlTraceConfig {
            enabled: true,
            redact_params: true,
            slow_ms: 0.0,
            to_log: false,
        },
        ..Default::default()
    };
    let registry = SchemaRegistry::new().table("ph_e2e", &["id"], &["id", "name"]);
    let b = Bridge::with_dbs_and_loader(
        std::collections::HashMap::from([("default".to_string(), da as _)]),
        Arc::new(InMemoryKV::new()),
        registry,
        false,
        None,
        extras,
    );
    let cap = b
        .run_with(
            r#"(async () => {
                     await db.query("select ? as v", [7]);
                     const p = db.sqlProfile();
                     return json.ok({ sqls: p.events.map((e) => e.sql) });
                   })().catch((e) => json.fail(500, String(e)));"#,
            crate::bridge::RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    let sqls = v["data"]["sqls"].as_array().expect("profile events");
    assert!(
        sqls.iter().any(|s| s.as_str() == Some("select ? as v")),
        "trace 须记原始 `?` SQL: {v}"
    );
    assert!(
        sqls.iter()
            .all(|s| !s.as_str().unwrap_or("").contains("$1")),
        "trace 不得记改写后 SQL: {v}"
    );
}

// ---------- L3 动态数据源登记（client 轴，v0.1.91）行为测试 ----------

/// 构造一个带 `db_client` 配置段的 Bridge（default 库存在 ⇒ `globalThis.db.client` 已挂载）。
fn client_bridge_with(
    enabled: bool,
    allow: AllowList,
    max: usize,
    db_access: Option<Arc<DbAccessSection>>,
) -> Bridge {
    let def = Arc::new(InMemoryAccessor::new());
    Bridge::with_dbs_and_loader(
        HashMap::from([("default".to_string(), def as Arc<dyn DataAccessor>)]),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
        None,
        Extras {
            db_client: Some(Arc::new(DbClientSection {
                enabled,
                max_entries: max,
                max_per_owner: 8,
                max_streams: 32,
                allow,
            })),
            client_dbs: Some(Arc::new(ClientDbRegistry::new(max))),
            db_backend: Some(Arc::new(DbBackendRegistry::builtin())),
            config_dir: Some(Arc::new(PathBuf::from("."))),
            db_access,
            ..Default::default()
        },
    )
}

async fn run_json(b: &Bridge, js: &str) -> serde_json::Value {
    let cap = b.run(js).await.expect("run must not transport-error");
    let v: serde_json::Value =
        serde_json::from_slice(&cap.body).expect("capsule body must be json");
    // 运行响应统一包成 {code,msg,data}；这里取下 data 层，测试断言更直读。
    v.get("data").cloned().unwrap_or(serde_json::Value::Null)
}

fn unrestricted() -> AllowList {
    AllowList::default()
}

// 登记门关闭（默认 off）：register 必须结构化拒绝，不得静默放行。
#[tokio::test(flavor = "current_thread")]
async fn client_disabled_gate_blocks_register() {
    let b = client_bridge_with(false, unrestricted(), 8, None);
    let v = run_json(
        &b,
        r#"(async () => {
                try { await db.client.register("rep", "memory://"); return json.ok({ok:true}); }
                catch (e) { return json.ok({ok:false, err:String(e)}); }
            })()"#,
    )
    .await;
    assert_eq!(v["ok"], false, "{v}");
    assert!(
        v["err"].as_str().unwrap().contains("db_client_disabled"),
        "{v}"
    );
}

// 全生命周期：register → has → list(脱敏) → delete → has=false。
#[tokio::test(flavor = "current_thread")]
async fn client_register_has_list_delete_lifecycle() {
    let b = client_bridge_with(true, unrestricted(), 8, None);
    let v = run_json(
        &b,
        r#"(async () => {
                const r = await db.client.register("rep", "memory://");
                const has = db.client.has("rep");
                const list = await db.client.list();
                const names = list.map(x => x.alias);
                const del = await db.client.delete("rep");
                const has2 = db.client.has("rep");
                return json.ok({ dialect: r.dialect, dsn: r.dsn, has, names, del, has2 });
            })()"#,
    )
    .await;
    assert_eq!(v["dialect"], "sqlite", "{v}");
    assert_eq!(v["has"], true, "{v}");
    assert!(
        v["names"].as_array().unwrap().contains(&json!("rep")),
        "{v}"
    );
    assert_eq!(v["del"], true, "{v}");
    assert_eq!(v["has2"], false, "{v}");
    // DSN 脱敏：memory:// 无凭据，且绝不回显任何 host/库名外的秘密段。
    let dsn = v["dsn"].as_str().unwrap();
    assert!(dsn.starts_with("memory://"), "{dsn}");
}

// 重名登记必须拒绝（不得覆盖）。
#[tokio::test(flavor = "current_thread")]
async fn client_duplicate_register_rejected() {
    let b = client_bridge_with(true, unrestricted(), 8, None);
    let v = run_json(
        &b,
        r#"(async () => {
                await db.client.register("rep", "memory://");
                try { await db.client.register("rep", "memory://"); return json.ok({dup:false}); }
                catch (e) { return json.ok({dup:true, err:String(e)}); }
            })()"#,
    )
    .await;
    assert_eq!(v["dup"], true, "{v}");
    assert!(
        v["err"].as_str().unwrap().contains("already registered"),
        "{v}"
    );
}

// 超配额必须拒绝（连接池数封顶）。
#[tokio::test(flavor = "current_thread")]
async fn client_over_capacity_rejected() {
    let b = client_bridge_with(true, unrestricted(), 1, None);
    let v = run_json(
        &b,
        r#"(async () => {
                await db.client.register("a", "memory://");
                try { await db.client.register("b", "memory://"); return json.ok({full:false}); }
                catch (e) { return json.ok({full:true, err:String(e)}); }
            })()"#,
    )
    .await;
    assert_eq!(v["full"], true, "{v}");
    assert!(v["err"].as_str().unwrap().contains("registry full"), "{v}");
}

// 未知 scheme：结构化错误 + 凭据脱敏（不得回显 secret）。
#[tokio::test(flavor = "current_thread")]
async fn client_unknown_scheme_redacted_error() {
    let b = client_bridge_with(true, unrestricted(), 8, None);
    let v = run_json(
            &b,
            r#"(async () => {
                try { await db.client.register("x", "oracle://user:secret@db/host"); return json.ok({err:false}); }
                catch (e) { return json.ok({err:true, msg:String(e)}); }
            })()"#,
        )
        .await;
    assert_eq!(v["err"], true, "{v}");
    let msg = v["msg"].as_str().unwrap();
    assert!(msg.contains("unknown db scheme"), "{msg}");
    assert!(!msg.contains("secret"), "凭据不得回显: {msg}");
}

// 登记门授权维：不匹配 caller 必须拒绝（asSystem 不豁免）。
#[tokio::test(flavor = "current_thread")]
async fn client_register_requires_allow_and_as_system_no_exempt() {
    // 仅放行 source=task；http caller（默认）应被拒。
    let allow = AllowList {
        sources: Some(vec!["task".to_string()]),
        ..Default::default()
    };
    let b = client_bridge_with(true, allow, 8, None);
    let plain = run_json(
        &b,
        r#"(async () => {
                try { await db.client.register("rep", "memory://"); return json.ok({auth:true}); }
                catch (e) { return json.ok({auth:false, err:String(e)}); }
            })()"#,
    )
    .await;
    assert_eq!(plain["auth"], false, "{plain}");
    assert!(
        plain["err"]
            .as_str()
            .unwrap()
            .contains("db_client_not_authorized"),
        "{plain}"
    );
    // asSystem 显式开后仍被拒（identity 不读 ReqState.system）。
    let via_system = run_json(
            &b,
            r#"(async () => {
                try { db.asSystem(); await db.client.register("rep", "memory://"); return json.ok({auth:true}); }
                catch (e) { return json.ok({auth:false, err:String(e)}); }
            })()"#,
        )
        .await;
    assert_eq!(via_system["auth"], false, "{via_system}");
    assert!(
        via_system["err"]
            .as_str()
            .unwrap()
            .contains("db_client_not_authorized"),
        "{via_system}"
    );
}

// 未声明 db_access.<alias> 的动态库：fail-closed deny（登记成功但查询/内省被拒）。
#[tokio::test(flavor = "current_thread")]
async fn undeclared_client_alias_query_denied() {
    let b = client_bridge_with(true, unrestricted(), 8, None);
    let v = run_json(
        &b,
        r#"(async () => {
                await db.client.register("rep", "memory://");
                try { await DB("rep").query("select 1"); return json.ok({q:true}); }
                catch (e) { return json.ok({q:false, err:String(e)}); }
            })()"#,
    )
    .await;
    assert_eq!(v["q"], false, "{v}");
    let err = v["err"].as_str().unwrap();
    assert!(err.contains("db access"), "{err}");
    assert!(err.contains("db_not_granted"), "{err}");
}

// 显式声明 db_access.<alias> 的动态库：登记后可正常查询（复用 L1 访问控制）。
#[tokio::test(flavor = "current_thread")]
async fn declared_client_alias_query_allowed() {
    let db_access = Arc::new(DbAccessSection {
        defaults: DbAccess::default(),
        by_db: HashMap::from([(
            "rep".to_string(),
            DbAccessOverride {
                allow: Some(AllowList::default()),
                ..Default::default()
            },
        )]),
    });
    let b = client_bridge_with(true, unrestricted(), 8, Some(db_access));
    let v = run_json(
        &b,
        r#"(async () => {
                await db.client.register("rep", "memory://");
                const rows = await DB("rep").query("select 1 as v");
                return json.ok({q:true, rows});
            })()"#,
    )
    .await;
    assert_eq!(v["q"], true, "{v}");
}

// reRegister：已存在则替换；不存在则拒绝。
#[tokio::test(flavor = "current_thread")]
async fn client_re_register_swaps_and_rejects_missing() {
    let b = client_bridge_with(true, unrestricted(), 8, None);
    let v = run_json(
            &b,
            r#"(async () => {
                await db.client.register("rep", "memory://");
                await db.client.reRegister("rep", "memory://");
                try { await db.client.reRegister("nope", "memory://"); return json.ok({bad:false, has:db.client.has("rep")}); }
                catch (e) { return json.ok({bad:true, err:String(e), has:db.client.has("rep")}); }
            })()"#,
        )
        .await;
    assert_eq!(v["bad"], true, "{v}");
    assert!(v["err"].as_str().unwrap().contains("not registered"), "{v}");
    assert_eq!(v["has"], true, "{v}");
}

// 查询/内省经 lookup 自动累加命中计数（声明别名后才可查，故需 declared 配置）。
#[tokio::test(flavor = "current_thread")]
async fn client_queries_counter_increments() {
    let db_access = Arc::new(DbAccessSection {
        defaults: DbAccess::default(),
        by_db: HashMap::from([(
            "rep".to_string(),
            DbAccessOverride {
                allow: Some(AllowList::default()),
                ..Default::default()
            },
        )]),
    });
    let b = client_bridge_with(true, unrestricted(), 8, Some(db_access));
    let v = run_json(
        &b,
        r#"(async () => {
                await db.client.register("rep", "memory://");
                await DB("rep").query("select 1 as v");
                await DB("rep").query("select 1 as v");
                const list = await db.client.list();
                const e = list.find(x => x.alias === "rep");
                return json.ok({queries: e.queries});
            })()"#,
    )
    .await;
    assert!(
        v["queries"].as_u64().unwrap() >= 2,
        "lookup 命中计数应累加: {v}"
    );
}

/// G3 契约裁决实验（v0.1.108，PG16 / MySQL 8.4 真机实测结论）：
/// ① 带参 EXPLAIN 两方言均可用（PG 用 `$n` 原生占位；op 通路由 normalize_and_trace 做 ? → $n）
///    ——预写「params×mysql 结构化回落」**不启用**。
/// ② PG `EXPLAIN (FORMAT JSON)` 输出 json 类型列，sqlx Any 驱动不解（实测报
///    `Any driver does not support the Postgres type PgTypeInfo(Json)`），有无参数都同样炸；
///    故 PG 归一输入定为**文本计划**（默认格式），MySQL 维持 FORMAT=JSON（Any 可解）。
/// 占位符按方言原生形态（accessor 不改写；op 通路才改写）。
/// 未设 OJ_TEST_PG / OJ_TEST_MYSQL 则 skip（诚实登记；CI 无服务容器恒 skip）。
#[tokio::test(flavor = "current_thread")]
async fn explain_accepts_bind_params_real_pg_mysql() {
    let mut ran = false;
    for (env, tag, sql) in [
        ("OJ_TEST_PG", "pg", "EXPLAIN SELECT 1 AS v WHERE 1 = $1"),
        (
            "OJ_TEST_MYSQL",
            "mysql",
            "EXPLAIN FORMAT=JSON SELECT 1 AS v WHERE 1 = ?",
        ),
    ] {
        let Ok(url) = std::env::var(env) else {
            continue;
        };
        if url.is_empty() {
            continue;
        }
        ran = true;
        let db = crate::bridge::SqlxAccessor::arc(&url).await.unwrap();
        let r = db.query_with_params(sql, &[json!(1)]).await;
        assert!(
            r.is_ok(),
            "{tag}: 带参 EXPLAIN 必须可用（实测契约）: {:?}",
            r.err()
        );
    }
    if !ran {
        eprintln!("skip: OJ_TEST_PG / OJ_TEST_MYSQL unset");
    }
}

/// G3（v0.1.108，env-gated 真库腿）：db.explain 在真实 PG / MySQL 上的端到端归一——
/// 带参 explain 断言 operation/category/cost/children 形态；PG 文本计划与 MySQL
/// FORMAT=JSON 两方言同构输出。未设 OJ_TEST_PG / OJ_TEST_MYSQL 则 skip（诚实登记）。
#[tokio::test(flavor = "current_thread")]
async fn explain_plan_real_pg_mysql() {
    let pg = std::env::var("OJ_TEST_PG").ok().filter(|s| !s.is_empty());
    let my = std::env::var("OJ_TEST_MYSQL")
        .ok()
        .filter(|s| !s.is_empty());
    if pg.is_none() && my.is_none() {
        eprintln!("skip: OJ_TEST_PG / OJ_TEST_MYSQL unset");
        return;
    }
    for (url, tag) in [(pg, "pg"), (my, "mysql")] {
        let Some(url) = url else { continue };
        let db = crate::bridge::SqlxAccessor::arc(&url).await.unwrap();
        let b = Bridge::with_dbs_and_loader(
            std::collections::HashMap::from([("default".to_string(), db as _)]),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            None,
            Default::default(),
        );
        let js = r#"(async () => {
                const ddl = await db.dialect();
                if (ddl === "postgres") {
                    await db.exec("drop table if exists oj_explain_t");
                    await db.exec("create table oj_explain_t (id int primary key, val text)");
                } else {
                    await db.exec("drop table if exists oj_explain_t");
                    await db.exec("create table oj_explain_t (id int primary key, val varchar(64))");
                }
                await db.exec("insert into oj_explain_t (id, val) values (1, 'a')");
                const p = (await db.explain("select * from oj_explain_t where id = ?", [1])).plan;
                json.ok({ op: p.operation, cat: p.category, total: p.cost && p.cost.total,
                          rows: p.rowsEstimate, kids: Array.isArray(p.children) });
              })().catch(e => json.fail(500, String(e)))"#;
        let cap = b
            .run_with(js, crate::bridge::RequestInfo::default())
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{tag}: explain 端到端失败: {v}");
        let d = &v["data"];
        assert!(
            d["op"].as_str().unwrap_or("").len() > 1,
            "{tag}: operation 空: {v}"
        );
        assert!(
            ["scan", "join", "aggregate", "sort", "other"]
                .contains(&d["cat"].as_str().unwrap_or("")),
            "{tag}: category 非法: {v}"
        );
        assert!(
            d["total"].is_number() || d["total"].is_null(),
            "{tag}: cost.total 形态: {v}"
        );
        assert_eq!(d["kids"], json!(true), "{tag}: children 必须为数组: {v}");
        let _ = b;
    }
}

/// U-14（v0.1.109，env-gated 真库腿）：MySQL `SHOW CREATE TABLE` 激活——
/// `db.catalog("ddl", t)` 在 mysql 上产出 CREATE TABLE 源码（当前 catalog_sql 查表
/// 先行返回 None，SHOW 拦截是死代码——本腿钉住前置修复）；scoped 先例不破：
/// `forSchema(...).catalog("ddl", ...)` 仍 `OJ_DB_UNSUPPORTED(capability="for_schema")`。
/// 仅 OJ_TEST_MYSQL 有 DSN 才跑；未设则 skip。
#[tokio::test(flavor = "current_thread")]
async fn catalog_ddl_mysql_show_create_real() {
    let Ok(url) = std::env::var("OJ_TEST_MYSQL") else {
        eprintln!("skip: OJ_TEST_MYSQL unset");
        return;
    };
    if url.is_empty() {
        eprintln!("skip: OJ_TEST_MYSQL empty");
        return;
    }
    let db = crate::bridge::SqlxAccessor::arc(&url).await.unwrap();
    let _ = db
        .exec_with_params("drop table if exists oj_ddl_t", &[])
        .await
        .unwrap();
    db.exec_with_params(
        "create table oj_ddl_t (id int auto_increment primary key, name varchar(64))",
        &[],
    )
    .await
    .unwrap();
    let b = Bridge::with_dbs_and_loader(
        std::collections::HashMap::from([("default".to_string(), db as _)]),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
        None,
        Default::default(),
    );
    let cap = b
        .run_with(
            r#"(async () => {
                 try {
                   const r = await db.catalog("ddl", "oj_ddl_t");
                   const scoped = await db.forSchema("ojtest").catalog("ddl", "oj_ddl_t")
                     .then(() => null, (e) => ({ code: e.code, capability: e.data && e.data.capability }));
                   json.ok({ ok: true, items: r.items, scoped });
                 } catch (e) {
                   json.ok({ ok: false, code: e.code, msg: String(e) });
                 }
               })().catch(e => json.fail(500, String(e)))"#,
            crate::bridge::RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    let d = &v["data"];
    assert_eq!(d["ok"], true, "catalog ddl 必须成功（非 unsupported）: {d}");
    let items = d["items"].as_array().expect("items 数组");
    assert_eq!(items.len(), 1, "{d}");
    let ddl = items[0]["ddl"].as_str().unwrap_or("");
    assert!(
        ddl.contains("CREATE TABLE `oj_ddl_t`"),
        "ddl 须为 SHOW CREATE 输出: {ddl}"
    );
    assert!(ddl.contains("AUTO_INCREMENT"), "含自增声明: {ddl}");
    assert_eq!(
        d["scoped"]["code"], "OJ_DB_UNSUPPORTED",
        "scoped 先例须保持: {d}"
    );
    assert_eq!(d["scoped"]["capability"], "for_schema", "{d}");
}

/// U-14（v0.1.109，env-gated 真库腿）：PG `db.catalog("ddl")` 的 relkind 路由——
/// 视图走既有 pg_get_viewdef（行为钉死不动）；不存在的表 → items 空数组（不报错）；
/// 序列（relkind 'S'）→ 空数组（绝不落拼接器，假 DDL 比空结果更糟）。
/// 仅 OJ_TEST_PG 有 DSN 才跑；未设则 skip。Table 路由的拼接器由 Task 3 钉死。
#[tokio::test(flavor = "current_thread")]
async fn catalog_ddl_pg_route_real() {
    let Ok(url) = std::env::var("OJ_TEST_PG") else {
        eprintln!("skip: OJ_TEST_PG unset");
        return;
    };
    if url.is_empty() {
        eprintln!("skip: OJ_TEST_PG empty");
        return;
    }
    let db = crate::bridge::SqlxAccessor::arc(&url).await.unwrap();
    let _ = db
        .exec_with_params("drop view if exists oj_ddl_v", &[])
        .await
        .unwrap();
    db.exec_with_params("create view oj_ddl_v as select 1 as v", &[])
        .await
        .unwrap();
    let b = Bridge::with_dbs_and_loader(
        std::collections::HashMap::from([("default".to_string(), db as _)]),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
        None,
        Default::default(),
    );
    let cap = b
        .run_with(
            r#"(async () => {
                 const view = await db.catalog("ddl", "oj_ddl_v");
                 const missing = await db.catalog("ddl", "oj_ddl_no_such");
                 const seq = await db.catalog("ddl", "oj_ddl_no_such_seq");
                 json.ok({ view, missing, seq });
               })().catch(e => json.fail(500, String(e)))"#,
            crate::bridge::RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    let d = &v["data"];
    // 视图：既有行为（pg_get_viewdef 输出）不变。
    let vitems = d["view"]["items"].as_array().expect("view items");
    assert_eq!(vitems.len(), 1, "视图须出定义: {d}");
    assert!(
        vitems[0]["ddl"].as_str().unwrap_or("").contains("SELECT"),
        "视图 ddl 须为 SELECT 定义: {d}"
    );
    // 不存在 / 序列：空数组不报错。
    assert_eq!(d["missing"]["items"], json!([]), "{d}");
    assert_eq!(d["seq"]["items"], json!([]), "{d}");
}

/// U-14（v0.1.109，env-gated 真库腿）：PG 拼接器 round-trip——含 serial 主键 / 非空 /
/// 默认值 / CHECK 约束 / 唯一索引 / 列注释 / 表注释的表，db.catalog("ddl") 产出
/// 可重放 DDL（序列前置 + ADD CONSTRAINT + CREATE INDEX + 双 COMMENT + OWNED BY），
/// 改名后 db.exec 重建成功且 listColumns 结构对账一致。
/// 仅 OJ_TEST_PG 有 DSN 才跑；未设则 skip。
#[tokio::test(flavor = "current_thread")]
async fn catalog_ddl_pg_stitch_roundtrip_real() {
    let Ok(url) = std::env::var("OJ_TEST_PG") else {
        eprintln!("skip: OJ_TEST_PG unset");
        return;
    };
    if url.is_empty() {
        eprintln!("skip: OJ_TEST_PG empty");
        return;
    }
    let db = crate::bridge::SqlxAccessor::arc(&url).await.unwrap();
    for s in [
        "drop table if exists oj_ddl_full_2",
        "drop table if exists oj_ddl_full",
    ] {
        db.exec_with_params(s, &[]).await.unwrap();
    }
    db.exec_with_params(
        "create table oj_ddl_full (\
            id serial primary key, \
            email varchar(64) not null, \
            val int default 0, \
            constraint ck_val check (val >= 0))",
        &[],
    )
    .await
    .unwrap();
    db.exec_with_params("create unique index ux_email on oj_ddl_full (email)", &[])
        .await
        .unwrap();
    db.exec_with_params("comment on column oj_ddl_full.email is 'login email'", &[])
        .await
        .unwrap();
    db.exec_with_params("comment on table oj_ddl_full is 'full table'", &[])
        .await
        .unwrap();
    let b = Bridge::with_dbs_and_loader(
        std::collections::HashMap::from([("default".to_string(), db as _)]),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
        None,
        Default::default(),
    );
    let cap = b
        .run_with(
            r#"(async () => {
                 const r = await db.catalog("ddl", "oj_ddl_full");
                 const ddl = r.items[0].ddl;
                 json.ok({
                   ddl,
                   hasSeq: ddl.includes("CREATE SEQUENCE IF NOT EXISTS"),
                   hasCons: ddl.includes("ADD CONSTRAINT"),
                   hasIdx: ddl.includes("CREATE UNIQUE INDEX ux_email"),
                   hasColCmt: ddl.includes("COMMENT ON COLUMN"),
                   hasTblCmt: ddl.includes("COMMENT ON TABLE"),
                   hasOwned: ddl.includes("OWNED BY"),
                 });
               })().catch(e => json.fail(500, String(e)))"#,
            crate::bridge::RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    let d = &v["data"];
    for k in [
        "hasSeq",
        "hasCons",
        "hasIdx",
        "hasColCmt",
        "hasTblCmt",
        "hasOwned",
    ] {
        assert_eq!(d[k], json!(true), "{k} 缺: {}", d["ddl"]);
    }
    // 改名重放：序列/表名一并替换；索引名同 schema 唯一，也换名（ux_email → ux_email_2；
    // 文档口径：pg_get_indexdef 带 schema 限定表名 + 裸索引名，同库重放须自行换索引名，
    // 跨库/跨 schema 直迁不可移植）。
    let ddl2 = d["ddl"]
        .as_str()
        .unwrap()
        .replace("oj_ddl_full", "oj_ddl_full_2")
        .replace("ux_email", "ux_email_2");
    // 逐条重放：sqlx exec 走 prepared 协议，一条命令一次。
    let stmts: Vec<&str> = ddl2
        .split(";\n")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let js = format!(
        r#"(async () => {{
             for (const s of {stmts:?}) await db.asSystem().exec(s);
             const a = await db.listColumns("oj_ddl_full");
             const b2 = await db.listColumns("oj_ddl_full_2");
             json.ok({{ a, b2 }});
           }})().catch(e => json.fail(500, String(e)))"#
    );
    let cap2 = b
        .run_with(&js, crate::bridge::RequestInfo::default())
        .await
        .unwrap();
    let v2: Value = serde_json::from_slice(&cap2.body).unwrap();
    assert_eq!(v2["code"], 0, "{v2}");
    let norm = |v: &Value| -> Value {
        // 对账忽略 nextval 文本（序列名随改名变化）。
        let mut arr = v.clone();
        for c in arr.as_array_mut().unwrap() {
            let dft = c["default"].as_str().unwrap_or("");
            if dft.contains("nextval") {
                c["default"] = json!("<seq>");
            }
        }
        arr
    };
    assert_eq!(
        norm(&v2["data"]["a"]),
        norm(&v2["data"]["b2"]),
        "重放后结构须一致"
    );
}

/// U-14（v0.1.109）db.ddlDiff 反向渲染：sqlite 桥 → 结构化 unsupported
/// （capability=ddl_diff，不静默回落）。
#[tokio::test(flavor = "current_thread")]
async fn ddl_diff_sqlite_unsupported() {
    let db = crate::bridge::SqlxAccessor::arc("sqlite::memory:")
        .await
        .unwrap();
    let b = Bridge::with_dbs_and_loader(
        std::collections::HashMap::from([("default".to_string(), db as _)]),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
        None,
        Default::default(),
    );
    let cap = b
        .run_with(
            r#"db.ddlDiff({ objectType: "table", name: "t", from: { columns: [] }, to: { columns: [] } })
                 .then(() => json.ok({ ok: true }),
                       (e) => json.ok({ ok: false, code: e.code, cap: e.data && e.data.capability }))"#,
            crate::bridge::RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    let d = &v["data"];
    assert_eq!(d["ok"], false, "sqlite 必须 unsupported: {d}");
    assert_eq!(d["code"], "OJ_DB_UNSUPPORTED", "{d}");
    assert_eq!(d["cap"], "ddl_diff", "{d}");
}

/// U-14（v0.1.109）db.ddlDiff PG smoke（真实 PG16）：listColumns 喂 from，
/// 改类型 + 加列 → USING 语句 + ADD COLUMN；零 DB I/O 渲染。
#[tokio::test(flavor = "current_thread")]
async fn ddl_diff_pg_real() {
    let Ok(url) = std::env::var("OJ_TEST_PG") else {
        eprintln!("skip: OJ_TEST_PG unset");
        return;
    };
    if url.is_empty() {
        eprintln!("skip: OJ_TEST_PG empty");
        return;
    }
    let db = crate::bridge::SqlxAccessor::arc(&url).await.unwrap();
    db.exec_with_params("drop table if exists oj_ddldiff_t", &[])
        .await
        .unwrap();
    db.exec_with_params(
        "create table oj_ddldiff_t (id int primary key, name text)",
        &[],
    )
    .await
    .unwrap();
    let b = Bridge::with_dbs_and_loader(
        std::collections::HashMap::from([("default".to_string(), db as _)]),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
        None,
        Default::default(),
    );
    let cap = b
        .run_with(
            r#"(async () => {
                 const from = { columns: await db.listColumns("oj_ddldiff_t") };
                 const toCols = from.columns.map(c => ({ ...c }));
                 toCols[1].nativeType = "varchar(64)";
                 toCols.push({ name: "age", nativeType: "integer", nullable: true });
                 const r = await db.ddlDiff({
                   objectType: "table", name: "oj_ddldiff_t", from, to: { columns: toCols },
                 });
                 json.ok({ added: r.diff.columns.added, stmts: r.statements });
               })().catch(e => json.fail(500, String(e)))"#,
            crate::bridge::RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    let d = &v["data"];
    assert_eq!(d["added"], json!(["age"]), "{d}");
    let stmts: String = d["stmts"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        stmts.contains("ALTER COLUMN \"name\" TYPE varchar(64) USING \"name\"::varchar(64)"),
        "{stmts}"
    );
    assert!(stmts.contains("ADD COLUMN \"age\" integer"), "{stmts}");
}

/// U-14（v0.1.109）db.ddlDiff MySQL smoke（真实 MySQL 8.4）：加列带 AFTER 定位。
#[tokio::test(flavor = "current_thread")]
async fn ddl_diff_mysql_real() {
    let Ok(url) = std::env::var("OJ_TEST_MYSQL") else {
        eprintln!("skip: OJ_TEST_MYSQL unset");
        return;
    };
    if url.is_empty() {
        eprintln!("skip: OJ_TEST_MYSQL empty");
        return;
    }
    let db = crate::bridge::SqlxAccessor::arc(&url).await.unwrap();
    db.exec_with_params("drop table if exists oj_ddldiff_t", &[])
        .await
        .unwrap();
    db.exec_with_params(
        "create table oj_ddldiff_t (id int auto_increment primary key, name varchar(64))",
        &[],
    )
    .await
    .unwrap();
    let b = Bridge::with_dbs_and_loader(
        std::collections::HashMap::from([("default".to_string(), db as _)]),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
        None,
        Default::default(),
    );
    let cap = b
        .run_with(
            r#"(async () => {
                 const from = { columns: await db.listColumns("oj_ddldiff_t") };
                 const toCols = from.columns.map(c => ({ ...c }));
                 toCols.push({ name: "age", nativeType: "int", nullable: true });
                 const r = await db.ddlDiff({
                   objectType: "table", name: "oj_ddldiff_t", from, to: { columns: toCols },
                 });
                 json.ok({ added: r.diff.columns.added, stmts: r.statements });
               })().catch(e => json.fail(500, String(e)))"#,
            crate::bridge::RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    let d = &v["data"];
    assert_eq!(d["added"], json!(["age"]), "{d}");
    let stmts: String = d["stmts"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(stmts.contains("ADD COLUMN `age` int"), "{stmts}");
    assert!(stmts.contains("AFTER `name`"), "{stmts}");
}

/// U-14（v0.1.109）PG 分区表 ddl：拼接器附 PARTITION BY（best-effort，子分区不产出）。
#[tokio::test(flavor = "current_thread")]
async fn catalog_ddl_pg_partitioned_real() {
    let Ok(url) = std::env::var("OJ_TEST_PG") else {
        eprintln!("skip: OJ_TEST_PG unset");
        return;
    };
    if url.is_empty() {
        eprintln!("skip: OJ_TEST_PG empty");
        return;
    }
    let db = crate::bridge::SqlxAccessor::arc(&url).await.unwrap();
    let db_cleanup = db.clone();
    db.exec_with_params("drop table if exists oj_ddl_part", &[])
        .await
        .unwrap();
    db.exec_with_params(
        "create table oj_ddl_part (id int not null, created_at date not null) \
         partition by range (created_at)",
        &[],
    )
    .await
    .unwrap();
    let b = Bridge::with_dbs_and_loader(
        std::collections::HashMap::from([("default".to_string(), db as _)]),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
        None,
        Default::default(),
    );
    let cap = b
        .run_with(
            r#"(async () => {
                 const r = await db.catalog("ddl", "oj_ddl_part");
                 json.ok({ ddl: r.items[0].ddl });
               })().catch(e => json.fail(500, String(e)))"#,
            crate::bridge::RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    let ddl = v["data"]["ddl"].as_str().unwrap_or("");
    assert!(
        ddl.contains("PARTITION BY"),
        "分区表 ddl 须含 PARTITION BY: {ddl}"
    );
    assert!(ddl.contains("(created_at)"), "分区键须保留: {ddl}");
    db_cleanup
        .exec_with_params("drop table if exists oj_ddl_part", &[])
        .await
        .unwrap();
}

/// REQ-07a：降级单元格经 `db.query`（裸 SQL 路）后，行值原样交付 + 语句级
/// `X-OJ-Degraded` 头外发去重清单（TEXT 字面量撞前缀时的权威判定面）。
/// fake 行的信封串同时钉住 jsnum 出口护栏不得改动 `$…` 字符串（Review Focus #3）。
#[tokio::test(flavor = "current_thread")]
async fn degraded_header_on_raw_query_path() {
    let db = Arc::new(InMemoryAccessor::new());
    db.seed([
        json!({"ip": "$inet:AQI=", "name": "ever"}),
        json!({"ip": "$inet:AgM=", "name": "root"}),
    ]);
    let b = Bridge::with_dbs(
        std::collections::HashMap::from([("default".to_string(), db as Arc<dyn DataAccessor>)]),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new().table("t", &["id"], &["id"]),
        false,
    );
    let cap = b
        .run(r#"db.query("select ip, name from t").then(r => json.ok(r)).catch(e => json.fail(500, String(e)));"#)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    // 信封串原样（sanitize_rows 不得改动）。
    assert_eq!(v["data"][0]["ip"], json!("$inet:AQI="), "{v}");
    // 语句级权威清单：按 {col,type} 去重。
    let h = cap
        .headers
        .iter()
        .find(|(k, _)| k == "X-OJ-Degraded")
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| panic!("缺 X-OJ-Degraded 头: {:?}", cap.headers));
    assert_eq!(
        serde_json::from_str::<Value>(&h).unwrap(),
        json!([{"col": "ip", "type": "inet"}]),
        "{h}"
    );
}

/// REQ-07a：构造器路（`op_db_query_build`）同一头外发——两路各自独立接线，缺一即漏。
#[tokio::test(flavor = "current_thread")]
async fn degraded_header_on_builder_path() {
    let db = Arc::new(InMemoryAccessor::new());
    db.seed([json!({"ip": "$inet:AQI=", "name": "ever"})]);
    let b = Bridge::with_dbs(
        std::collections::HashMap::from([("default".to_string(), db as Arc<dyn DataAccessor>)]),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new().table("t", &["id"], &["id", "name", "ip"]),
        false,
    );
    let cap = b
        .run(r#"db.table("t").select(["ip"]).all().then(r => json.ok(r)).catch(e => json.fail(500, String(e)));"#)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert!(
        cap.headers.iter().any(|(k, _)| k == "X-OJ-Degraded"),
        "构造器路须打 X-OJ-Degraded 头: {:?}",
        cap.headers
    );
}

/// REQ-07a：无降级单元格时**不打**头（对照面，防无脑常置头污染响应）。
#[tokio::test(flavor = "current_thread")]
async fn no_degraded_header_when_clean() {
    let db = Arc::new(InMemoryAccessor::new());
    db.seed([json!({"id": 1, "name": "ever"})]);
    let b = Bridge::with_dbs(
        std::collections::HashMap::from([("default".to_string(), db as Arc<dyn DataAccessor>)]),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new().table("t", &["id"], &["id", "name"]),
        false,
    );
    let cap = b
        .run(r#"db.query("select id, name from t").then(r => json.ok(r)).catch(e => json.fail(500, String(e)));"#)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert!(
        cap.headers.iter().all(|(k, _)| k != "X-OJ-Degraded"),
        "干净结果不得打 X-OJ-Degraded 头: {:?}",
        cap.headers
    );
}

/// Final review I2：同请求多语句（DB("a") + DB("b") 各一条降级查询）时，X-OJ-Degraded
/// 须**并集合并为一条**——serve 侧同名头 last-wins，分条推会被静默覆盖，前序语句的
/// 降级单元格失去权威标记。
#[tokio::test(flavor = "current_thread")]
async fn degraded_header_merges_across_statements_in_one_request() {
    let a = Arc::new(InMemoryAccessor::new());
    a.seed([json!({"ip": "$inet:AQI="})]);
    let b = Arc::new(InMemoryAccessor::new());
    b.seed([json!({"g": "$point:AAAA"})]);
    let b = Bridge::with_dbs(
        std::collections::HashMap::from([
            ("a".to_string(), a as Arc<dyn DataAccessor>),
            ("b".to_string(), b as Arc<dyn DataAccessor>),
        ]),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
    );
    let cap = b
        .run(
            r#"(async () => {
                const ra = await DB("a").query("select ip from ta");
                const rb = await DB("b").query("select g from tb");
                json.ok({ ra, rb });
              })().catch(e => json.fail(500, String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    let entries: Vec<&(String, String)> = cap
        .headers
        .iter()
        .filter(|(k, _)| k == "X-OJ-Degraded")
        .collect();
    assert_eq!(
        entries.len(),
        1,
        "同名头须并集合并为一条（serve last-wins 会丢前序）: {:?}",
        cap.headers
    );
    assert_eq!(
        serde_json::from_str::<Value>(entries[0].1.as_str()).unwrap(),
        json!([{"col": "ip", "type": "inet"}, {"col": "g", "type": "point"}]),
        "{}",
        entries[0].1
    );
}

/// Final review I1：非 ASCII 列名（PG/MySQL 合法，safe_ident 只剔控制字符）降级时，
/// 头值须纯 ASCII——`\uXXXX` 是合法 JSON 转义；不转义会被 serve 的 HeaderValue
/// 宽容解析**静默丢弃**，权威判定面无声消失。
#[tokio::test(flavor = "current_thread")]
async fn degraded_header_ascii_safe_for_non_ascii_colnames() {
    let db = Arc::new(InMemoryAccessor::new());
    db.seed([json!({"地址": "$point:AAAA"})]);
    let b = Bridge::with_dbs(
        std::collections::HashMap::from([("default".to_string(), db as Arc<dyn DataAccessor>)]),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
    );
    let cap = b
        .run(
            r#"db.query("select 地址 from t").then(r => json.ok(r)).catch(e => json.fail(500, String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    let h = cap
        .headers
        .iter()
        .find(|(k, _)| k == "X-OJ-Degraded")
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| panic!("非 ASCII 列名的降级头被静默丢弃: {:?}", cap.headers));
    assert!(
        h.is_ascii(),
        "头值须纯 ASCII（HeaderValue 只容可见 ASCII）: {h:?}"
    );
    assert_eq!(
        serde_json::from_str::<Value>(&h).unwrap(),
        json!([{"col": "地址", "type": "point"}]),
        "{h}"
    );
}
