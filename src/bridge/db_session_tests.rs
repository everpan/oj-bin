//! 会话的语义全部在**请求边界**上，故这里一律经 `Bridge::run` 跑 JS：一次 `run` = 一个
//! 请求（独立 isolate 复用 + `ReqState::reset`）。这样「会话跨请求存活」不是靠内部
//! 断言推出来的，而是被真正跨过三次请求验证的。
use super::*;
use crate::bridge::db::InMemoryAccessor;
use crate::bridge::db_policy::AllowList;
use crate::bridge::{Bridge, DataAccessor, Extras, InMemoryKV, RequestInfo, SchemaRegistry};
use serde_json::{Value, json};

/// 临时 sqlite 文件（**不用 `:memory:`**：会话要跨请求，连接必须落在同一个真实库上；
/// 文件也让「提交后重开连接可见」这件事可验证）。
fn tmp_dsn(tag: &str) -> String {
    let p = std::env::temp_dir().join(format!("oj-sess-{}-{tag}.db", std::process::id()));
    let _ = std::fs::remove_file(&p);
    format!("sqlite://{}", p.display())
}

async fn sqlite(tag: &str) -> Arc<dyn DataAccessor> {
    // 走全仓唯一的 DSN 归一入口（建文件 + Windows 盘符坑）；手拼 `sqlite://` 直喂
    // sqlx 会被 Url 解析吞掉绝对路径（盘符成 host）→ SQLITE_CANTOPEN。
    crate::bridge::db_backend::DbBackendRegistry::builtin()
        .connect(&tmp_dsn(tag), &std::env::temp_dir())
        .await
        .unwrap()
}

fn cfg(edit: impl FnOnce(&mut DbSessionSection)) -> DbSessionSection {
    let mut c = DbSessionSection {
        enabled: true,
        // 单测没有 auth 插件：身份由 RequestInfo.user 直接注入，不看 require_identity。
        require_identity: false,
        ..DbSessionSection::default()
    };
    edit(&mut c);
    c
}

fn bridge(dbs: Vec<(&str, Arc<dyn DataAccessor>)>, c: DbSessionSection) -> Bridge {
    Bridge::with_dbs_and_loader(
        dbs.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
        None,
        Extras {
            db_session: Some(Arc::new(c)),
            ..Default::default()
        },
    )
}

/// 单库桥（temp sqlite 已建 t 表）。
async fn bridge_t(tag: &str, c: DbSessionSection) -> Bridge {
    let db = sqlite(tag).await;
    db.exec_with_params("create table t (id integer primary key, v text)", &[])
        .await
        .unwrap();
    bridge(vec![("default", db)], c)
}

async fn run(b: &Bridge, src: &str) -> Value {
    let cap = b.run(src).await.unwrap();
    serde_json::from_slice(&cap.body).unwrap()
}

async fn run_as(b: &Bridge, user: Option<Value>, src: &str) -> Value {
    let req = RequestInfo {
        user,
        ..RequestInfo::default()
    };
    let cap = b.run_with(src, req).await.unwrap();
    serde_json::from_slice(&cap.body).unwrap()
}

/// 池上查询（会话必须在提交/回滚之后才允许走池：sqlite 池刻意只有 1 条连接）。
async fn count_via_pool(b: &Bridge) -> i64 {
    let v = run(b, r#"db.query("select count(*) c from t").then((r) => json.ok(r[0])).catch((e) => json.fail(500, String(e)))"#).await;
    v["data"]["c"]
        .as_i64()
        .unwrap_or_else(|| panic!("pool query failed: {v}"))
}

fn id_of(v: &Value) -> String {
    v["data"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("no session id: {v}"))
        .to_string()
}

// ---------------------------------------------------------------------
// 主旅程：会话跨请求存活，未提交的写在下一个请求里仍可见，最后才提交
// ---------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn session_outlives_requests_and_only_commit_makes_it_visible() {
    let b = bridge_t("xreq", cfg(|_| {})).await;
    // 请求 1：开会话 + 写一行（不提交）
    let v = run(
            &b,
            r#"db.session.begin()
                 .then((s) => db.session.exec(s.id, "insert into t (v) values (?)", ["a"]).then(() => json.ok(s)))
                 .catch((e) => json.fail(500, String(e)))"#,
        )
        .await;
    let id = id_of(&v);
    // id 形状即契约（前缀 + 4 位 boot tag + 32 位随机）
    assert!(
        handle_core::id_is_wellformed(PREFIX, &id),
        "bad id shape: {id}"
    );

    // 请求 2：attach 后 db.query 走**同一条连接** → 看得见未提交的写
    let v = run(
        &b,
        &format!(
            r#"db.session.attach("{id}")
                     .then(() => db.query("select count(*) c from t"))
                     .then((r) => json.ok(r[0]))
                     .catch((e) => json.fail(500, String(e)))"#
        ),
    )
    .await;
    assert_eq!(
        v["data"]["c"], 1,
        "session must see its own uncommitted write: {v}"
    );

    // 请求 3：提交
    let v = run(
            &b,
            &format!(r#"db.session.commit("{id}").then(() => json.ok({{}})).catch((e) => json.fail(500, String(e)))"#),
        )
        .await;
    assert_eq!(v["code"], 0, "commit failed: {v}");

    // 提交后连接归还池 → 池查询看得见
    assert_eq!(count_via_pool(&b).await, 1);
}

#[tokio::test(flavor = "current_thread")]
async fn rollback_discards_writes_and_reports_a_tombstone() {
    let b = bridge_t("rb", cfg(|_| {})).await;
    let v = run(
            &b,
            r#"db.session.begin()
                 .then((s) => db.session.exec(s.id, "insert into t (v) values (?)", ["a"]).then(() => db.session.rollback(s.id)).then(() => json.ok(s)))
                 .catch((e) => json.fail(500, String(e)))"#,
        )
        .await;
    let id = id_of(&v);
    assert_eq!(
        count_via_pool(&b).await,
        0,
        "rollback must discard the insert"
    );
    // 已结束的会话：本主体能看到具体死法，而不是含糊的 not found；
    // 且只有**非正常收尾**才许说 "rolled back"（commit 成功却这么写就是撒谎）。
    let v = run(
            &b,
            &format!(r#"db.session.attach("{id}").then(() => json.ok({{}})).catch((e) => json.ok({{ msg: String(e) }}))"#),
        )
        .await;
    let msg = v["data"]["msg"].as_str().unwrap();
    assert!(
        msg.contains("closed") && msg.contains("rollback") && msg.contains("has ended"),
        "{msg}"
    );
    assert!(!msg.contains("rolled back"), "{msg}");
}

#[tokio::test(flavor = "current_thread")]
async fn request_boundary_does_not_roll_back_an_open_session() {
    let b = bridge_t("boundary", cfg(|_| {})).await;
    let v = run(
            &b,
            r#"db.session.begin()
                 .then((s) => db.session.exec(s.id, "insert into t (v) values (?)", ["a"]).then(() => json.ok(s)))
                 .catch((e) => json.fail(500, String(e)))"#,
        )
        .await;
    let id = id_of(&v);
    // 第三个请求仍能续用（若请求结束回滚了，这里会 not found）
    let v = run(
            &b,
            &format!(
                r#"db.session.info("{id}").then((i) => json.ok(i)).catch((e) => json.fail(500, String(e)))"#
            ),
        )
        .await;
    assert_eq!(v["data"]["state"], "open", "{v}");
    assert_eq!(
        v["data"]["ops"], 1,
        "ops must accumulate across requests: {v}"
    );
    let _ = run(
        &b,
        &format!(
            r#"db.session.rollback("{id}").then(() => json.ok({{}})).catch(() => json.ok({{}}))"#
        ),
    )
    .await;
}

// ---------------------------------------------------------------------
// 门禁
// ---------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn disabled_by_default() {
    // 缺省配置（enabled=false）：与升级前完全一致——能力不存在，且报错点名怎么开。
    let b = bridge_t("off", DbSessionSection::default()).await;
    let v = run(
        &b,
        r#"db.session.begin().then(() => json.ok({})).catch((e) => json.ok({ msg: String(e) }))"#,
    )
    .await;
    let msg = v["data"]["msg"].as_str().unwrap();
    assert!(msg.contains("disabled"), "{msg}");
    assert!(msg.contains("db_session.enabled"), "{msg}");
    // 读面保持静默（list/stats 不抛）：未启用 = 空表
    let v = run(
        &b,
        r#"db.session.list().then((l) => json.ok({ n: l.length }))"#,
    )
    .await;
    assert_eq!(v["data"]["n"], 0, "{v}");
}

#[tokio::test(flavor = "current_thread")]
async fn allow_list_is_rechecked_per_call() {
    // 允许 db-admin 角色；普通 user 被拒
    let b = bridge_t(
        "gate",
        cfg(|c| {
            c.allow = AllowList {
                roles: Some(vec!["db-admin".into()]),
                ..Default::default()
            }
        }),
    )
    .await;
    let v = run_as(
        &b,
        Some(json!({"id": "u1", "roles": ["user"]})),
        r#"db.session.begin().then(() => json.ok({})).catch((e) => json.ok({ msg: String(e) }))"#,
    )
    .await;
    assert!(
        v["data"]["msg"].as_str().unwrap().contains("not granted"),
        "{v}"
    );
    let v = run_as(
            &b,
            Some(json!({"id": "u1", "roles": ["db-admin"]})),
            r#"db.session.begin().then((s) => db.session.rollback(s.id)).then(() => json.ok({})).catch((e) => json.fail(500, String(e)))"#,
        )
        .await;
    assert_eq!(v["code"], 0, "{v}");
}

#[tokio::test(flavor = "current_thread")]
async fn require_identity_rejects_anonymous_callers() {
    let b = bridge_t("ident", cfg(|c| c.require_identity = true)).await;
    let v = run(
        &b,
        r#"db.session.begin().then(() => json.ok({})).catch((e) => json.ok({ msg: String(e) }))"#,
    )
    .await;
    assert!(
        v["data"]["msg"]
            .as_str()
            .unwrap()
            .contains("authenticated caller"),
        "{v}"
    );
}

// ---------------------------------------------------------------------
// 防伪造 / 归属隔离 / 容量
// ---------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn unknown_and_foreign_ids_are_indistinguishable() {
    let b = bridge_t("iso", cfg(|_| {})).await;
    let v = run_as(
        &b,
        Some(json!({"id": "alice"})),
        r#"db.session.begin().then((s) => json.ok(s)).catch((e) => json.fail(500, String(e)))"#,
    )
    .await;
    let id = id_of(&v);
    // 他人（bob）拿真 id 也只会得到 not found
    let v = run_as(
            &b,
            Some(json!({"id": "bob"})),
            &format!(r#"db.session.attach("{id}").then(() => json.ok({{}})).catch((e) => json.ok({{ msg: String(e) }}))"#),
        )
        .await;
    let msg = v["data"]["msg"].as_str().unwrap().to_string();
    assert!(msg.contains("not found"), "{msg}");
    assert!(
        !msg.contains(&id),
        "must not echo the full session id: {msg}"
    );

    // 伪造的合法形状 id：同一文案（不泄露存在性）
    let fake = format!("tx_0000_{}", "0".repeat(32));
    let v = run_as(
            &b,
            Some(json!({"id": "alice"})),
            &format!(r#"db.session.attach("{fake}").then(() => json.ok({{}})).catch((e) => json.ok({{ msg: String(e) }}))"#),
        )
        .await;
    assert!(
        v["data"]["msg"].as_str().unwrap().contains("not found"),
        "{v}"
    );
    let _ = run_as(
        &b,
        Some(json!({"id": "alice"})),
        &format!(
            r#"db.session.rollback("{id}").then(() => json.ok({{}})).catch(() => json.ok({{}}))"#
        ),
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn capacity_limits_refuse_new_sessions() {
    let b = bridge_t("cap", cfg(|c| c.max_sessions = 1)).await;
    let v = run(
            &b,
            r#"db.session.begin()
                 .then((s) => db.session.begin().then(() => json.ok({ leaked: s.id })).catch((e) => json.ok({ msg: String(e), id: s.id })))
                 .catch((e) => json.fail(500, String(e)))"#,
        )
        .await;
    assert!(
        v["data"]["msg"].as_str().unwrap().contains("limit reached"),
        "{v}"
    );
    let id = v["data"]["id"].as_str().unwrap().to_string();
    let _ = run(
        &b,
        &format!(
            r#"db.session.rollback("{id}").then(() => json.ok({{}})).catch(() => json.ok({{}}))"#
        ),
    )
    .await;
}

// ---------------------------------------------------------------------
// 会话内语句禁令（MySQL 隐式提交 → 假回滚）
// ---------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn transaction_control_and_session_state_sql_are_rejected() {
    let b = bridge_t("forbid", cfg(|_| {})).await;
    let v = run(
            &b,
            r#"db.session.begin().then(async (s) => {
                 const errs = [];
                 for (const sql of ["BEGIN", "COMMIT", "ROLLBACK", "SAVEPOINT sp1", "SET autocommit=1", "USE other", "PRAGMA foreign_keys=1"]) {
                   try { await db.session.exec(s.id, sql); errs.push("LEAK:" + sql); }
                   catch (e) { errs.push(String(e)); }
                 }
                 return db.session.rollback(s.id).then(() => json.ok({ errs }));
               }).catch((e) => json.fail(500, String(e)))"#,
        )
        .await;
    let errs = v["data"]["errs"].as_array().unwrap();
    assert_eq!(errs.len(), 7, "{v}");
    for e in errs {
        let s = e.as_str().unwrap();
        assert!(
            s.contains("not allowed inside a cross-request transaction"),
            "{s}"
        );
    }
    // 会话内正常语句不受影响
    let v = run(
            &b,
            r#"db.session.begin()
                 .then((s) => db.session.exec(s.id, "insert into t (v) values (?)", ["ok"]).then(() => db.session.rollback(s.id)))
                 .then(() => json.ok({}))
                 .catch((e) => json.fail(500, String(e)))"#,
        )
        .await;
    assert_eq!(v["code"], 0, "{v}");
}

// ---------------------------------------------------------------------
// 与 db.tx / 其他别名 / 动态别名的交互
// ---------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn db_tx_and_sessions_are_mutually_exclusive() {
    let b = bridge_t("excl", cfg(|_| {})).await;
    let v = run(
            &b,
            r#"db.tx(async () => {
                 try { await db.session.begin(); return json.fail(500, "session.begin should fail inside db.tx"); }
                 catch (e) { return json.ok({ msg: String(e) }); }
               })"#,
        )
        .await;
    assert!(v["data"]["msg"].as_str().unwrap().contains("db.tx"), "{v}");

    // 会话**已绑定**时 db.tx 必须被拒（未绑定时不冲突：那是另一条连接上的另一件事；
    // 注意 sqlite 池只有 1 条连接，未绑定会话 + db.tx 会互相等连接，不在本用例范围）。
    let v = run(
            &b,
            r#"db.session.begin().then(async (s) => {
                 await db.session.attach(s.id);
                 try { await db.tx(async () => {}); return json.fail(500, "db.tx should fail inside a session"); }
                 catch (e) { const m = String(e); await db.session.rollback(s.id); return json.ok({ msg: m }); }
               })"#,
        )
        .await;
    assert!(
        v["data"]["msg"]
            .as_str()
            .unwrap()
            .contains("session is attached"),
        "{v}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn attached_session_refuses_queries_on_another_alias() {
    let a = sqlite("alias-a").await;
    let o = sqlite("alias-o").await;
    o.exec_with_params("create table t (id integer primary key, v text)", &[])
        .await
        .unwrap();
    let b = bridge(vec![("default", a), ("other", o)], cfg(|_| {}));
    let v = run(
        &b,
        r#"DB("other").session.begin()
                 .then((s) => db.session.attach(s.id).then(() => db.query("select 1")))
                 .then(() => json.fail(500, "cross-alias query should fail"))
                 .catch((e) => json.ok({ msg: String(e) }))"#,
    )
    .await;
    let msg = v["data"]["msg"].as_str().unwrap();
    assert!(msg.contains("attached to db 'other'"), "{msg}");
    assert!(msg.contains("not 'default'"), "{msg}");
}

#[tokio::test(flavor = "current_thread")]
async fn discarding_an_alias_kills_its_sessions() {
    let b = bridge_t("discard", cfg(|_| {})).await;
    let v = run(
        &b,
        r#"db.session.begin().then((s) => json.ok(s)).catch((e) => json.fail(500, String(e)))"#,
    )
    .await;
    let id = id_of(&v);
    // 模拟 client_db delete / reRegister 的钩子
    let n = b.stable_for_test().tx_sessions.discard_alias("default");
    assert_eq!(n, 1);
    let v = run(
            &b,
            &format!(r#"db.session.attach("{id}").then(() => json.ok({{}})).catch((e) => json.ok({{ msg: String(e) }}))"#),
        )
        .await;
    let msg = v["data"]["msg"].as_str().unwrap();
    assert!(msg.contains("discarded"), "{msg}");
    assert_eq!(count_via_pool(&b).await, 0);
}

// ---------------------------------------------------------------------
// 过期回收 / 幂等收尾 / 观察面
// ---------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn idle_sessions_expire_and_are_rolled_back() {
    let b = bridge_t(
        "idle",
        cfg(|c| {
            c.idle_timeout_secs = 1;
            c.max_lifetime_secs = 30;
        }),
    )
    .await;
    let v = run(
            &b,
            r#"db.session.begin()
                 .then((s) => db.session.exec(s.id, "insert into t (v) values (?)", ["a"]).then(() => json.ok(s)))
                 .catch((e) => json.fail(500, String(e)))"#,
        )
        .await;
    let id = id_of(&v);
    // 空闲超时（有界等待：回收器 tick 上限 1s，这里只等「过期事实」，不等具体文案）
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let v = run(
                &b,
                &format!(r#"db.session.info("{id}").then(() => json.ok({{ alive: true }})).catch((e) => json.ok({{ alive: false, msg: String(e) }}))"#),
            )
            .await;
        if v["data"]["alive"] == false {
            let msg = v["data"]["msg"].as_str().unwrap();
            assert!(
                msg.contains("expired") || msg.contains("not found"),
                "{msg}"
            );
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "session never expired (deadline hit)"
        );
    }
    // 超时即回滚：池上查不到那行
    assert_eq!(count_via_pool(&b).await, 0);
}

#[tokio::test(flavor = "current_thread")]
async fn close_is_idempotent_and_stats_track_lifecycle() {
    let b = bridge_t("close", cfg(|_| {})).await;
    let v = run(
            &b,
            r#"db.session.begin().then((s) => db.session.stats().then((st1) => db.session.close(s.id).then((a) => db.session.close(s.id).then((b2) => json.ok({ id: s.id, st1, a, b: b2 }))))).catch((e) => json.fail(500, String(e)))"#,
        )
        .await;
    assert_eq!(v["data"]["st1"]["active"], 1, "{v}");
    assert_eq!(v["data"]["st1"]["enabled"], true, "{v}");
    assert_eq!(v["data"]["a"], true);
    assert_eq!(v["data"]["b"], true, "close must be idempotent");
    let v = run(&b, r#"db.session.list().then((l) => db.session.stats().then((s) => json.ok({ n: l.length, active: s.active, discarded: s.discarded })))"#).await;
    assert_eq!(v["data"]["n"], 0, "{v}");
    assert_eq!(v["data"]["active"], 0, "{v}");
    assert!(v["data"]["discarded"].as_u64().unwrap() >= 1, "{v}");
}

#[tokio::test(flavor = "current_thread")]
async fn touch_reports_remaining_lifetime() {
    let b = bridge_t("touch", cfg(|_| {})).await;
    let v = run(
            &b,
            r#"db.session.begin().then((s) => db.session.touch(s.id).then((t) => db.session.rollback(s.id).then(() => json.ok(t)))).catch((e) => json.fail(500, String(e)))"#,
        )
        .await;
    let ms = v["data"]["expiresInMs"].as_u64().unwrap();
    // 默认 idle 300s / lifetime 1800s → 剩余寿命恒 > 0 且不超过最长寿命
    assert!(ms > 0 && ms <= 1_800_000, "{v}");
}

// InMemoryAccessor 存在性：保证夹具与生产同契约（不参与断言，仅编译期钉住可替换性）
#[allow(dead_code)]
fn _in_memory_is_a_data_accessor() {
    let _: Arc<dyn DataAccessor> = Arc::new(InMemoryAccessor::new());
}

/// G9：只读会话位——attach 路径与显式 op 双入口都拦 Write；SELECT 照常；
/// 错误信封 `sessionDisposition=keep`（会话不被判死）；summary 透出 readOnly 位。
#[tokio::test(flavor = "current_thread")]
async fn readonly_session_blocks_writes_at_op_layer() {
    let b = bridge_t("g9ro", cfg(|_| {})).await;
    let v = run(
        &b,
        r#"(async () => {
              const s = await db.session.begin({ readOnly: true });
              await db.session.attach(s.id);
              let err = null, disp = null;
              try { await db.exec("insert into t (v) values ('x')"); }
              catch (e) { err = String(e); disp = e.sessionDisposition; }
              let err2 = null;
              try { await db.session.exec(s.id, "insert into t (v) values ('y')"); }
              catch (e) { err2 = String(e); }
              const c = await db.query("select count(*) c from t");
              const info = await db.session.info(s.id);
              return json.ok({ err, disp, err2, count: c[0].c, ro: info.readOnly });
            })().catch((e) => json.fail(500, String(e)))"#,
    )
    .await;
    let err = v["data"]["err"]
        .as_str()
        .unwrap_or_else(|| panic!("no err: {v}"));
    assert!(err.contains("is read-only"), "{err}");
    assert_eq!(v["data"]["disp"], json!("keep"), "{v}");
    let err2 = v["data"]["err2"].as_str().unwrap_or_default();
    assert!(err2.contains("is read-only"), "{err2}");
    assert_eq!(v["data"]["count"], json!(0), "两条写都必须被拒：{v}");
    assert_eq!(v["data"]["ro"], json!(true), "{v}");
}

/// G9 回归锚：缺省（不给 readOnly）会话写照常——位缺省 false 不改既有行为。
#[tokio::test(flavor = "current_thread")]
async fn writable_session_default_still_writes() {
    let b = bridge_t("g9rw", cfg(|_| {})).await;
    let v = run(
        &b,
        r#"(async () => {
              const s = await db.session.begin();
              if (s.readOnly !== false) return json.fail(500, "readOnly must default false");
              await db.session.attach(s.id);
              await db.exec("insert into t (v) values ('ok')");
              const c = await db.query("select count(*) c from t");
              return json.ok({ count: c[0].c });
            })().catch((e) => json.fail(500, String(e)))"#,
    )
    .await;
    assert_eq!(v["data"]["count"], json!(1), "{v}");
}
