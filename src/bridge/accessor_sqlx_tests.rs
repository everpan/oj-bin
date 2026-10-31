use super::*;
use crate::bridge::db::{Dialect, PoolExecShared, dialect_of};
use crate::bridge::handle_core::Owner;
use crate::bridge::{Bridge, InMemoryKV, SchemaRegistry};
use serde_json::json;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

// 仅类型/构造检查（不连真实库）；保证 SqlxAccessor 满足 DataAccessor trait。
fn _assert_impl() {
    fn takes<T: DataAccessor>() {}
    takes::<SqlxAccessor>();
}

fn test_shared() -> Arc<PoolExecShared> {
    Arc::new(PoolExecShared {
        owner: Owner {
            tenant: None,
            user: None,
            module: None,
            source: "test",
        },
        alias: "d1".to_string(),
        start: std::time::Instant::now(),
        cancel_requested: std::sync::atomic::AtomicBool::new(false),
        notify: Arc::new(tokio::sync::Notify::new()),
        cancel_token: None,
        done: std::sync::atomic::AtomicBool::new(false),
        events: Mutex::new(Vec::new()),
        outcome: Mutex::new(None),
        meter_final: Mutex::new(None),
        query_ctx: Mutex::new(None),
    })
}

// REQ-11 Task 2：可取消执行的相位事件与取消语义（sqlite 离线腿）。
#[tokio::test(flavor = "current_thread")]
async fn cancelable_query_records_sent_fetching_done_events() {
    let db = SqlxAccessor::connect("sqlite::memory:").await.unwrap();
    let shared = test_shared();
    let mut meter =
        super::super::db_quota::ByteMeter::new(None, super::super::db_policy::OnExceed::Truncate);
    let rows = db
        .query_cancelable(
            "select 1 as x union all select 2 as x",
            &[],
            &mut meter,
            &shared,
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    let ev: Vec<&str> = shared
        .events
        .lock()
        .unwrap()
        .iter()
        .map(|e| e.phase)
        .collect();
    assert_eq!(ev, vec!["sent", "fetching", "done"]);
    // done 事件带最终行数
    assert_eq!(shared.events.lock().unwrap().last().unwrap().rows, Some(2));
}

#[tokio::test(flavor = "current_thread")]
async fn cancel_before_submit_returns_cancelled_without_running() {
    let db = SqlxAccessor::connect("sqlite::memory:").await.unwrap();
    let shared = test_shared();
    shared.cancel_requested.store(true, Ordering::SeqCst);
    let mut meter =
        super::super::db_quota::ByteMeter::new(None, super::super::db_policy::OnExceed::Truncate);
    let e = db
        .query_cancelable("select 1", &[], &mut meter, &shared)
        .await
        .unwrap_err();
    let be = e.downcast_ref::<BridgeError>().unwrap();
    assert_eq!(be.category, DbErrorCategory::Cancelled);
    // 语句未执行：sent 事件未发
    assert!(shared.events.lock().unwrap().is_empty());
    // exec 路径同口径
    let e2 = db
        .exec_cancelable("create table nope (id int)", &[], &shared)
        .await
        .unwrap_err();
    assert_eq!(
        e2.downcast_ref::<BridgeError>().unwrap().category,
        DbErrorCategory::Cancelled
    );
}

#[tokio::test(flavor = "current_thread")]
async fn cancel_mid_execution_returns_cancelled_category() {
    let db = SqlxAccessor::connect("sqlite::memory:").await.unwrap();
    let shared = test_shared();
    let s2 = shared.clone();
    let killer = tokio::spawn(async move {
        // 有界轮询等 sent 事件（提交已发生），随后发 cancel——非固定 sleep
        for _ in 0..10_000 {
            if s2.events.lock().unwrap().iter().any(|e| e.phase == "sent") {
                break;
            }
            tokio::task::yield_now().await;
        }
        s2.cancel_requested.store(true, Ordering::SeqCst);
        s2.notify.notify_waiters();
    });
    let mut meter =
        super::super::db_quota::ByteMeter::new(None, super::super::db_policy::OnExceed::Truncate);
    // LIMIT 放外层（Windows 上 sqlx 捆绑的旧版 sqlite 不接受 CTE 内的 LIMIT）
    let big = "WITH RECURSIVE cnt(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM cnt) SELECT x FROM cnt LIMIT 5000000";
    let e = db
        .query_cancelable(big, &[], &mut meter, &shared)
        .await
        .unwrap_err();
    killer.await.unwrap();
    assert_eq!(
        e.downcast_ref::<BridgeError>().unwrap().category,
        DbErrorCategory::Cancelled
    );
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
    // ② toSQL().params 的超界整数 v0.1.24 起是**标记**而不是十进制字符串——
    //    这样 `db.query(toSQL().sql, ...toSQL().params)` 才能原样回跑（字符串会被绑成 text）。
    assert_eq!(
        v["data"]["tsParam0"],
        json!({ "$oj$i64": "9007199254740993" })
    );
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
