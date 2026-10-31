use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use super::*;
use crate::bridge::db::DataAccessor;
use crate::bridge::{Bridge, Extras, InMemoryKV, RequestInfo, SchemaRegistry};

/// 文件型 sqlite：内存库每连接独立，`db.exec` 建表后 `db.query` 可能换连接看不到。
/// 限额类用例必须落盘（tempfile 随目录析构清理）。
async fn file_sqlite() -> (tempfile::TempDir, Arc<dyn DataAccessor>) {
    let dir = tempfile::tempdir().unwrap();
    let dsn = format!("sqlite://{}", dir.path().join("t.db").display());
    // 走全仓唯一的 DSN 归一入口（它会建文件 + 处理 Windows 盘符坑），
    // 不要手拼 `sqlite://` 直接喂 sqlx（Url 解析会吞绝对路径 → SQLITE_CANTOPEN）。
    let db = crate::bridge::db_backend::DbBackendRegistry::builtin()
        .connect(&dsn, dir.path())
        .await
        .unwrap();
    (dir, db)
}

fn bridge_with(
    db: Arc<dyn DataAccessor>,
    registry: SchemaRegistry,
    policy: DbAccessSection,
    task: bool,
    // `tenant.allow_as_tenant`：`db.asTenant()` 的唯一开关（默认关）。
    as_tenant: bool,
) -> Bridge {
    Bridge::with_dbs_and_loader(
        std::collections::HashMap::from([("default".to_string(), db)]),
        Arc::new(InMemoryKV::new()),
        registry,
        false,
        None,
        Extras {
            db_access: Some(Arc::new(policy)),
            allow_as_tenant: as_tenant,
            tasks_flag: if task {
                Some(Arc::new(AtomicBool::new(false)))
            } else {
                None
            },
            ..Default::default()
        },
    )
}

/// U-1 运行期授权的特权门（`db_client.allow`），供 grant/revoke 用例复用。
fn bridge_with_client(
    db: Arc<dyn DataAccessor>,
    policy: DbAccessSection,
    client: DbClientSection,
) -> Bridge {
    Bridge::with_dbs_and_loader(
        std::collections::HashMap::from([("default".to_string(), db)]),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
        None,
        Extras {
            db_access: Some(Arc::new(policy)),
            db_client: Some(Arc::new(client)),
            ..Default::default()
        },
    )
}

/// 跑一段 JS，返回信封；失败（code != 0）返回 msg 便于断言原因串。
async fn run(b: &Bridge, req: RequestInfo, js: &str) -> (i64, String, serde_json::Value) {
    let cap = b.run_with(js, req).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&cap.body).unwrap();
    let code = v["code"].as_i64().unwrap_or(-1);
    let msg = v["msg"].as_str().unwrap_or_default().to_string();
    (code, msg, v["data"].clone())
}

const JS_WRITE: &str = r#"(async () => {
        try { await db.exec("delete from t where id = 1"); json.ok({ok: true}); }
        catch (e) { json.ok({err: String(e)}); }
      })().catch(e => json.fail(500, String(e)));"#;

const JS_READ: &str = r#"(async () => {
        try { const r = await db.query("select id from t"); json.ok({n: r.length}); }
        catch (e) { json.ok({err: String(e)}); }
      })().catch(e => json.fail(500, String(e)));"#;

fn req_user(roles: &[&str]) -> RequestInfo {
    RequestInfo {
        user: Some(json!({"id": 7, "roles": roles})),
        ..Default::default()
    }
}

/// REQ-05 Q3：语句 deadline 竞速（ yielding 后端证明接线；sqlite 同步步进饿死
/// 定时器的架构限制另行成文）。Given 后端 query 睡 5s + 配置 50ms，When db.query，
/// Then ~50ms 拿 OJ_DB_TIMEOUT（不是 5s 后跑完）。
struct SleepyAccessor;
#[async_trait::async_trait]
impl DataAccessor for SleepyAccessor {
    async fn query_with_params(
        &self,
        _sql: &str,
        _params: &[serde_json::Value],
    ) -> super::super::BridgeResult<Vec<crate::bridge::db::Row>> {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        Ok(vec![])
    }
    async fn exec_with_params(
        &self,
        _sql: &str,
        _params: &[serde_json::Value],
    ) -> super::super::BridgeResult<i64> {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        Ok(0)
    }
}

fn sleepy_bridge(statement_timeout_ms: u64) -> Bridge {
    let pol = DbAccessSection {
        defaults: DbAccess {
            statement_timeout_ms: Some(statement_timeout_ms),
            ..Default::default()
        },
        ..Default::default()
    };
    bridge_with(
        Arc::new(SleepyAccessor),
        SchemaRegistry::new(),
        pol,
        false,
        false,
    )
}

const JS_TIMEOUT_PROBE: &str = r#"(async () => {
        const t0 = Date.now();
        try {
          await db.query("select 1", null, {timeoutMs: 50});
          json.ok({err: "no-timeout", ms: Date.now() - t0});
        } catch (e) {
          json.ok({code: e.code, cancelled: e.data && e.data.cancelled, ms: Date.now() - t0});
        }
      })().catch(e => json.fail(500, String(e)));"#;

#[tokio::test(flavor = "current_thread")]
async fn stmt_timeout_race_fires_and_returns_structured_error() {
    let b = sleepy_bridge(5_000); // 配置 5s；调用侧收紧到 50ms
    let (_c, _m, data) = run(&b, RequestInfo::default(), JS_TIMEOUT_PROBE).await;
    assert_eq!(data["code"], json!("OJ_DB_TIMEOUT"), "{data}");
    let ms = data["ms"].as_i64().unwrap_or(9999);
    assert!(ms < 2_000, "竞速必须在 ~50ms 触发，而非 5s 跑完：{ms}ms");
    // 假后端不宣称服务端取消（data.cancelled 按后端诚实标注）
    assert_eq!(data["cancelled"], json!(false), "{data}");
}

/// 配置侧单独生效（无 per-call opt）：Given 配置 50ms，When db.query，Then OJ_DB_TIMEOUT。
#[tokio::test(flavor = "current_thread")]
async fn stmt_timeout_config_side_alone_applies() {
    let b = sleepy_bridge(50);
    let (_c, _m, data) = run(&b, RequestInfo::default(), JS_TIMEOUT_PROBE).await;
    assert_eq!(data["code"], json!("OJ_DB_TIMEOUT"), "{data}");
}

/// exec 路径同闸。Given 配置 50ms，When 慢 exec，Then OJ_DB_TIMEOUT。
#[tokio::test(flavor = "current_thread")]
async fn stmt_timeout_applies_to_exec() {
    let b = sleepy_bridge(50);
    let js = r#"(async () => {
            try {
              await db.exec("delete from t");
              json.ok({err: "no-timeout"});
            } catch (e) { json.ok({code: e.code}); }
          })().catch(e => json.fail(500, String(e)));"#;
    let (_c, _m, data) = run(&b, RequestInfo::default(), js).await;
    assert_eq!(data["code"], json!("OJ_DB_TIMEOUT"), "{data}");
}

/// 终审 I2（复核定级 Important）：语句超时与字节上限**同时**越界时，raw 路径不得把
/// OJ_DB_TIMEOUT 改写成 OJ_DB_CAPACITY——meter 判定必须有「查询已成功」守卫
/// （构造器路径 query.rs 已有 `meter.exceeded && out.is_ok()`，raw 路径当时漏写）。
#[tokio::test(flavor = "current_thread")]
async fn simultaneous_timeout_and_byte_exceed_reports_timeout() {
    // Given 假 accessor：边喂 meter 爆字节上限、边睡过语句 deadline
    struct GreedySleepy;
    #[async_trait::async_trait]
    impl DataAccessor for GreedySleepy {
        async fn query_with_params_metered(
            &self,
            _sql: &str,
            _params: &[serde_json::Value],
            meter: &mut super::super::db_quota::ByteMeter,
        ) -> super::super::BridgeResult<Vec<crate::bridge::db::Row>> {
            let row = serde_json::json!({ "v": "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx" });
            // 喂到触顶（limit 配 16 字节），再继续喂——meter 在 future 被 drop 前已 exceeded。
            for _ in 0..4 {
                meter.feed(&row);
            }
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            Ok(vec![row])
        }
        async fn query_with_params(
            &self,
            _sql: &str,
            _params: &[serde_json::Value],
        ) -> super::super::BridgeResult<Vec<crate::bridge::db::Row>> {
            unreachable!("raw 池臂走 metered 出口")
        }
        async fn exec_with_params(
            &self,
            _sql: &str,
            _params: &[serde_json::Value],
        ) -> super::super::BridgeResult<i64> {
            Ok(0)
        }
    }
    let pol = DbAccessSection {
        defaults: DbAccess {
            statement_timeout_ms: Some(300),
            max_result_bytes: Some(16),
            on_exceed: Some(OnExceed::Fail),
            ..Default::default()
        },
        ..Default::default()
    };
    let b = bridge_with(
        Arc::new(GreedySleepy),
        SchemaRegistry::new(),
        pol,
        false,
        false,
    );
    let js = r#"(async () => {
            try {
              await db.query("select ?", ["x"]);
              json.ok({err: "no-timeout"});
            } catch (e) { json.ok({code: e.code}); }
          })().catch(e => json.fail(500, String(e)));"#;
    let (_c, _m, data) = run(&b, RequestInfo::default(), js).await;
    assert_eq!(
        data["code"],
        json!("OJ_DB_TIMEOUT"),
        "同时越界必须报超时（先到先生效），不得被字节档改写：{data}"
    );
}

/// E1（U-18 批次 3）：`db_export:` 段解析 + fail-loud 校验（v0.1.104）。
/// 缺省值即规格值（全局 4 / 每主体 2 / 回落硬顶 128MiB / TTL 24h / 清扫 1h）；
/// 0/超界/回落硬顶 ≥ max_bytes → 点名键拒。
#[test]
fn export_section_parses_defaults_and_validates() {
    // Given 空段，Then 全缺省
    let s: DbExportSection = serde_yaml::from_str("{}").unwrap();
    assert_eq!(s.global_concurrency, 4);
    assert_eq!(s.per_subject_concurrency, 2);
    assert_eq!(s.max_bytes, 512 * 1024 * 1024);
    assert_eq!(s.fallback_buffer_cap, 128 * 1024 * 1024);
    assert_eq!(s.ttl_secs, 24 * 3600);
    assert_eq!(s.sweep_interval_secs, 3600);
    // Given 全键声明，Then 原样取
    let s: DbExportSection = serde_yaml::from_str(
            "global_concurrency: 8\nper_subject_concurrency: 4\nmax_bytes: 4194304\nfallback_buffer_cap: 2097152\nttl_secs: 60\nsweep_interval_secs: 5\n",
        )
        .unwrap();
    assert!(s.validate().is_ok(), "{s:?}");
    assert_eq!(s.max_bytes, 4 * 1024 * 1024);
    // When 非法值，Then Err 点名键
    for (yaml, key) in [
        ("global_concurrency: 0\n", "global_concurrency"),
        ("global_concurrency: 65\n", "global_concurrency"),
        ("per_subject_concurrency: 0\n", "per_subject_concurrency"),
        ("per_subject_concurrency: 17\n", "per_subject_concurrency"),
        ("max_bytes: 0\n", "max_bytes"),
        ("fallback_buffer_cap: 0\n", "fallback_buffer_cap"),
        ("fallback_buffer_cap: 107374182400\n", "fallback_buffer_cap"),
        ("ttl_secs: 0\n", "ttl_secs"),
        ("sweep_interval_secs: 0\n", "sweep_interval_secs"),
        // 回落硬顶不得 ≥ 单任务字节上限（否则回落路径无意义）
        (
            "max_bytes: 1048576\nfallback_buffer_cap: 1048576\n",
            "fallback_buffer_cap",
        ),
    ] {
        let s: DbExportSection = serde_yaml::from_str(yaml).unwrap();
        let e = s.validate().expect_err("must reject");
        assert!(e.contains(key), "{e} must name {key}");
    }
    // 未知键 fail-loud（serde 默认拒绝）
    assert!(serde_yaml::from_str::<DbExportSection>("concurency: 4\n").is_err());
}

/// E1（REQ-12）：`db_import:` 段解析 + fail-loud 校验。缺省值即规格值
/// （全局 4 / 每主体 2 / 行上限 1M / 字节 512MiB / 批 5000 / TTL 24h / 清扫 1h）；
/// 0/超界/字节 < 1MiB → Err 点名 `db_import.<key>`。
#[test]
fn import_section_parses_defaults_and_validates() {
    // Given 空段，Then 全缺省
    let s: DbImportSection = serde_yaml::from_str("{}").unwrap();
    assert_eq!(s.global_concurrency, 4);
    assert_eq!(s.per_subject_concurrency, 2);
    assert_eq!(s.max_rows, 1_000_000);
    assert_eq!(s.max_bytes, 512 * 1024 * 1024);
    assert_eq!(s.default_batch_size, 5000);
    assert_eq!(s.ttl_secs, 24 * 3600);
    assert_eq!(s.sweep_interval_secs, 3600);
    assert!(s.validate().is_ok(), "{s:?}");
    // When 非法值，Then Err 点名 db_import.<key>
    for (yaml, key) in [
        ("global_concurrency: 0\n", "global_concurrency"),
        ("global_concurrency: 65\n", "global_concurrency"),
        ("per_subject_concurrency: 0\n", "per_subject_concurrency"),
        ("per_subject_concurrency: 17\n", "per_subject_concurrency"),
        ("max_rows: 0\n", "max_rows"),
        ("max_bytes: 0\n", "max_bytes"),
        ("max_bytes: 1048575\n", "max_bytes"),
        ("default_batch_size: 99\n", "default_batch_size"),
        ("default_batch_size: 50001\n", "default_batch_size"),
        ("ttl_secs: 0\n", "ttl_secs"),
        ("sweep_interval_secs: 0\n", "sweep_interval_secs"),
    ] {
        let s: DbImportSection = serde_yaml::from_str(yaml).unwrap();
        let e = s.validate().expect_err("must reject");
        assert!(e.contains("db_import."), "{e} must prefix db_import.");
        assert!(e.contains(key), "{e} must name {key}");
    }
    // 未知键 fail-loud（serde 默认拒绝）
    assert!(serde_yaml::from_str::<DbImportSection>("concurrencyy: 4\n").is_err());
}

/// 只能收紧：调用侧 timeoutMs > 配置 → 结构化拒，message 点名 statement_timeout_ms。
#[tokio::test(flavor = "current_thread")]
async fn call_timeout_above_config_rejected_naming_key() {
    let b = sleepy_bridge(5_000);
    let js = r#"(async () => {
            try {
              await db.query("select 1", null, {timeoutMs: 6000});
              json.ok({err: "no-reject"});
            } catch (e) { json.ok({code: e.code, msg: String(e.message || e)}); }
          })().catch(e => json.fail(500, String(e)));"#;
    let (_c, _m, data) = run(&b, RequestInfo::default(), js).await;
    assert_eq!(data["code"], json!("OJ_DB_CAPACITY"), "{data}");
    assert!(
        data["msg"]
            .as_str()
            .unwrap_or_default()
            .contains("statement_timeout_ms"),
        "{data}"
    );
}

/// 快语句不受闸影响（regression sanity）：sqlite 真库 + 5s 配置 + 速查 → 正常返回。
#[tokio::test(flavor = "current_thread")]
async fn fast_query_unaffected_by_stmt_timeout() {
    let (_d, db) = file_sqlite().await;
    db.exec_with_params("create table t (id integer)", &[])
        .await
        .unwrap();
    let b = bridge_with(
        db,
        SchemaRegistry::new(),
        DbAccessSection {
            defaults: DbAccess {
                statement_timeout_ms: Some(5_000),
                ..Default::default()
            },
            ..Default::default()
        },
        false,
        false,
    );
    let (_c, _m, data) = run(&b, RequestInfo::default(), JS_READ).await;
    assert_eq!(data["n"], json!(0), "{data}");
}

/// `enforce: off`（默认档）必须完全透明：`read_only` 也写在配置里也不设防。
#[tokio::test(flavor = "current_thread")]
async fn off_tier_is_fully_transparent() {
    let (_d, db) = file_sqlite().await;
    db.exec_with_params("create table t (id integer)", &[])
        .await
        .unwrap();
    let pol = DbAccessSection {
        defaults: DbAccess {
            read_only: true,     // off 档下不生效
            max_rows: Some(999), // 限额照旧生效（限额不是闸）
            ..Default::default()
        },
        ..Default::default()
    };
    let b = bridge_with(db, SchemaRegistry::new(), pol, false, false);
    let (code, _m, data) = run(&b, RequestInfo::default(), JS_WRITE).await;
    assert_eq!(code, 0, "{data}");
    assert_eq!(data["ok"], json!(true), "off 档不得拦写：{data}");
}

/// deny 档 + read_only：写类语句被拦，原因串可 grep。
#[tokio::test(flavor = "current_thread")]
async fn deny_read_only_blocks_write() {
    let (_d, db) = file_sqlite().await;
    db.exec_with_params("create table t (id integer)", &[])
        .await
        .unwrap();
    let pol = DbAccessSection {
        defaults: DbAccess {
            enforce: Enforce::Deny,
            read_only: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let b = bridge_with(db, SchemaRegistry::new(), pol, false, false);
    let (code, _m, data) = run(&b, RequestInfo::default(), JS_WRITE).await;
    assert_eq!(code, 0, "{data}"); // handler 自己兜住了
    let err = data["err"].as_str().unwrap_or_default();
    assert!(err.contains("db_read_only"), "{err}");
    // 读不受影响
    let (_, _, data) = run(&b, RequestInfo::default(), JS_READ).await;
    assert_eq!(data["n"], json!(0), "{data}");
}

/// 授权未命中 → `db_not_granted`；命中 roles → 放行（并集语义的行为面版本）。
#[tokio::test(flavor = "current_thread")]
async fn role_grant_hit_and_miss() {
    let (_d, db) = file_sqlite().await;
    db.exec_with_params("create table t (id integer)", &[])
        .await
        .unwrap();
    let policy = || DbAccessSection {
        defaults: DbAccess {
            enforce: Enforce::Deny,
            allow: AllowList {
                roles: Some(vec!["admin".into()]),
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    // 无身份（user=null）→ fail-closed
    let b = bridge_with(db.clone(), SchemaRegistry::new(), policy(), false, false);
    let (_, _, data) = run(&b, RequestInfo::default(), JS_READ).await;
    let err = data["err"].as_str().unwrap_or_default();
    assert!(err.contains("db_not_granted"), "无身份不得放行：{err}");
    // 有身份但角色不命中
    let (_, _, data) = run(&b, req_user(&["guest"]), JS_READ).await;
    assert!(
        data["err"]
            .as_str()
            .unwrap_or_default()
            .contains("db_not_granted"),
        "{data}"
    );
    // 角色命中
    let (_, _, data) = run(&b, req_user(&["admin"]), JS_READ).await;
    assert_eq!(data["n"], json!(0), "命中即放行：{data}");
}

/// `asSystem()` **不豁免** db 访问控制（它只绕过租户注入）。
#[tokio::test(flavor = "current_thread")]
async fn as_system_does_not_exempt_access_control() {
    let (_d, db) = file_sqlite().await;
    db.exec_with_params("create table t (id integer)", &[])
        .await
        .unwrap();
    let pol = DbAccessSection {
        defaults: DbAccess {
            enforce: Enforce::Deny,
            read_only: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let b = bridge_with(db, SchemaRegistry::new(), pol, false, false);
    let js = r#"(async () => {
            try { await db.asSystem().query("delete from t"); json.ok({ok: true}); }
            catch (e) { json.ok({err: String(e)}); }
          })().catch(e => json.fail(500, String(e)));"#;
    let (_, _, data) = run(&b, RequestInfo::default(), js).await;
    assert!(
        data["err"]
            .as_str()
            .unwrap_or_default()
            .contains("db_read_only"),
        "asSystem 不得豁免访问控制：{data}"
    );
}

/// `asTenant()` 声明的租户**不作数**（tenants 维只认 server 注入），故匿名请求
/// 走该维一律 fail-closed —— 否则等于把授权交给请求方自己填。
#[tokio::test(flavor = "current_thread")]
async fn as_tenant_voids_the_tenant_dimension() {
    let (_d, db) = file_sqlite().await;
    db.exec_with_params("create table t (id integer)", &[])
        .await
        .unwrap();
    let pol = DbAccessSection {
        defaults: DbAccess {
            enforce: Enforce::Deny,
            allow: AllowList {
                tenants: Some(vec!["t1".into()]),
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let b = bridge_with(db, SchemaRegistry::new(), pol, false, true);
    let js = r#"(async () => {
            try { await db.asTenant("t1").query("select id from t"); json.ok({ok: true}); }
            catch (e) { json.ok({err: String(e)}); }
          })().catch(e => json.fail(500, String(e)));"#;
    // server 注入的 tenant 命中 → 放行
    let hit = RequestInfo {
        tenant_id: Some("t1".into()),
        ..Default::default()
    };
    let (_, _, data) = run(&b, hit, JS_READ).await;
    assert_eq!(data["n"], json!(0), "server 注入的租户应命中：{data}");
    // 未命中
    let miss = RequestInfo {
        tenant_id: Some("t2".into()),
        ..Default::default()
    };
    let (_, _, data) = run(&b, miss, JS_READ).await;
    assert!(
        data["err"]
            .as_str()
            .unwrap_or_default()
            .contains("db_not_granted"),
        "{data}"
    );
    // 匿名 + asTenant("t1")：租户由请求方自填 → 该维作废 → fail-closed
    let anon = RequestInfo {
        anonymous: true,
        ..Default::default()
    };
    let (_, _, data) = run(&b, anon, js).await;
    assert!(
        data["err"]
            .as_str()
            .unwrap_or_default()
            .contains("db_not_granted"),
        "asTenant 不得自授租户权：{data}"
    );
}

/// 任务桥走 `sources: ["task"]`，HTTP 桥走 `http`（同一策略、身份相同，只换 source）。
#[tokio::test(flavor = "current_thread")]
async fn task_bridge_reports_source_task() {
    let (_d, db) = file_sqlite().await;
    db.exec_with_params("create table t (id integer)", &[])
        .await
        .unwrap();
    let policy = |want: &str| DbAccessSection {
        defaults: DbAccess {
            enforce: Enforce::Deny,
            allow: AllowList {
                sources: Some(vec![want.into()]),
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    // HTTP 桥 + 只允许 task → 拒
    let b = bridge_with(
        db.clone(),
        SchemaRegistry::new(),
        policy("task"),
        false,
        false,
    );
    let (_, _, data) = run(&b, RequestInfo::default(), JS_READ).await;
    assert!(
        data["err"]
            .as_str()
            .unwrap_or_default()
            .contains("db_not_granted"),
        "HTTP 桥不得自称 task：{data}"
    );
    // 任务桥 + 只允许 task → 放行
    let b = bridge_with(
        db.clone(),
        SchemaRegistry::new(),
        policy("task"),
        true,
        false,
    );
    let (_, _, data) = run(&b, RequestInfo::default(), JS_READ).await;
    assert_eq!(data["n"], json!(0), "任务桥应走 source=task：{data}");
    // 任务桥 + 只允许 http → 拒
    let b = bridge_with(db, SchemaRegistry::new(), policy("http"), true, false);
    let (_, _, data) = run(&b, RequestInfo::default(), JS_READ).await;
    assert!(
        data["err"]
            .as_str()
            .unwrap_or_default()
            .contains("db_not_granted"),
        "{data}"
    );
}

/// raw `query` 超 `max_rows` → 截断（并打 `X-OJ-Row-Limit`）。
#[tokio::test(flavor = "current_thread")]
async fn raw_query_truncates_at_max_rows() {
    let (_d, db) = file_sqlite().await;
    db.exec_with_params("create table t (id integer)", &[])
        .await
        .unwrap();
    db.exec_with_params(
            "insert into t select 1 union all select 2 union all select 3 union all select 4 union all select 5",
            &[],
        )
        .await
        .unwrap();
    let pol = DbAccessSection {
        defaults: DbAccess {
            max_rows: Some(3),
            ..Default::default()
        },
        ..Default::default()
    };
    let b = bridge_with(db, SchemaRegistry::new(), pol, false, false);
    let (_, _, data) = run(&b, RequestInfo::default(), JS_READ).await;
    assert_eq!(data["n"], json!(3), "raw 路径须截断到 max_rows：{data}");
}

/// REQ-05 Q4：max_result_bytes fail 档（缺省）——触顶回结构化 OJ_DB_CAPACITY。
/// Given 两行各 ~15 JSON 字节、上限 20，When db.query，Then 抛错点名 max_result_bytes。
#[tokio::test(flavor = "current_thread")]
async fn raw_query_fails_at_max_result_bytes() {
    let (_d, db) = file_sqlite().await;
    db.exec_with_params("create table t (id text)", &[])
        .await
        .unwrap();
    db.exec_with_params(
        "insert into t select 'aaaaaaaaaa' union all select 'bbbbbbbbbb'",
        &[],
    )
    .await
    .unwrap();
    let pol = DbAccessSection {
        defaults: DbAccess {
            max_result_bytes: Some(20),
            ..Default::default()
        },
        ..Default::default()
    };
    let b = bridge_with(db, SchemaRegistry::new(), pol, false, false);
    let js = r#"(async () => {
            try {
              const r = await db.query("select id from t");
              json.ok({n: r.length});
            } catch (e) { json.ok({code: e.code, msg: String(e.message || e)}); }
          })().catch(e => json.fail(500, String(e)));"#;
    let (_c, _m, data) = run(&b, RequestInfo::default(), js).await;
    assert_eq!(data["code"], json!("OJ_DB_CAPACITY"), "{data}");
    assert!(
        data["msg"]
            .as_str()
            .unwrap_or_default()
            .contains("max_result_bytes"),
        "{data}"
    );
}

/// REQ-05 Q4：truncate 档——交付已取行 + X-OJ-Result-Truncated 显式信令（验收原文）。
#[tokio::test(flavor = "current_thread")]
async fn raw_query_truncates_at_max_result_bytes_with_header() {
    let (_d, db) = file_sqlite().await;
    db.exec_with_params("create table t (id text)", &[])
        .await
        .unwrap();
    db.exec_with_params(
        "insert into t select 'aaaaaaaaaa' union all select 'bbbbbbbbbb'",
        &[],
    )
    .await
    .unwrap();
    let pol = DbAccessSection {
        defaults: DbAccess {
            max_result_bytes: Some(20),
            on_exceed: Some(OnExceed::Truncate),
            ..Default::default()
        },
        ..Default::default()
    };
    let b = bridge_with(db, SchemaRegistry::new(), pol, false, false);
    let js = r#"(async () => {
            const r = await db.query("select id from t");
            json.ok({n: r.length});
          })().catch(e => json.fail(500, String(e)));"#;
    let cap = b.run_with(js, RequestInfo::default()).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["n"], json!(1), "{v}");
    // 显式信令：响应头（对齐 X-OJ-Row-Limit 先例）
    assert!(
        cap.headers
            .iter()
            .any(|(k, _)| k == "X-OJ-Result-Truncated"),
        "truncate 必须打 X-OJ-Result-Truncated 头：{:?}",
        cap.headers
    );
}

/// 回落后端（未覆盖 metered 的 accessor）口径钉死：行已物化，fail 仍拒、
/// truncate 交付全量 + 信令照打（成文限制，ABI 13 车对齐 sqlite 真早停）。
#[tokio::test(flavor = "current_thread")]
async fn metered_fallback_on_uncapped_accessor() {
    let db = Arc::new(crate::bridge::InMemoryAccessor::new());
    db.seed([json!({"id": "aaaaaaaaaa"}), json!({"id": "bbbbbbbbbb"})]);
    let pol = DbAccessSection {
        defaults: DbAccess {
            max_result_bytes: Some(20),
            ..Default::default()
        },
        ..Default::default()
    };
    let b = bridge_with(db, SchemaRegistry::new(), pol, false, false);
    let js = r#"(async () => {
            try {
              const r = await db.query("select id from t");
              json.ok({n: r.length});
            } catch (e) { json.ok({code: e.code}); }
          })().catch(e => json.fail(500, String(e)));"#;
    let (_c, _m, data) = run(&b, RequestInfo::default(), js).await;
    assert_eq!(data["code"], json!("OJ_DB_CAPACITY"), "{data}");
}

/// REQ-05 Q5：构造器路径同闸。Given max_result_bytes=20 + 两行 ~15 字节，When
/// db.table().select().all()，Then fail 档 OJ_DB_CAPACITY / truncate 档部分行 + 信令。
#[tokio::test(flavor = "current_thread")]
async fn builder_path_applies_max_result_bytes() {
    let (_d, db) = file_sqlite().await;
    db.exec_with_params("create table t (id text)", &[])
        .await
        .unwrap();
    db.exec_with_params(
        "insert into t select 'aaaaaaaaaa' union all select 'bbbbbbbbbb'",
        &[],
    )
    .await
    .unwrap();
    let reg = SchemaRegistry::new().table("t", &["id"], &["id"]);
    // fail 档（缺省）
    let b = bridge_with(
        db.clone(),
        reg.clone(),
        DbAccessSection {
            defaults: DbAccess {
                max_result_bytes: Some(20),
                ..Default::default()
            },
            ..Default::default()
        },
        false,
        false,
    );
    let js = r#"(async () => {
            try {
              const r = await db.table("t").select(["id"]).orderBy([{field:"id",dir:"asc"}]).all();
              json.ok({n: r.length});
            } catch (e) { json.ok({code: e.code}); }
          })().catch(e => json.fail(500, String(e)));"#;
    let (_c, _m, data) = run(&b, RequestInfo::default(), js).await;
    assert_eq!(data["code"], json!("OJ_DB_CAPACITY"), "{data}");
    // truncate 档
    let b = bridge_with(
        db,
        reg,
        DbAccessSection {
            defaults: DbAccess {
                max_result_bytes: Some(20),
                on_exceed: Some(OnExceed::Truncate),
                ..Default::default()
            },
            ..Default::default()
        },
        false,
        false,
    );
    let cap = b.run_with(js, RequestInfo::default()).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["n"], json!(1), "{v}");
    assert!(
        cap.headers
            .iter()
            .any(|(k, _)| k == "X-OJ-Result-Truncated"),
        "构造器 truncate 必须打信令头：{:?}",
        cap.headers
    );
}

/// REQ-05 Q6：db.stream 字节触顶 fail 档——pump 发结构化错行经通道，next() 还原
/// OJ_DB_CAPACITY（中途错误必须抛错，不静默 done——取消先例同口径）。
#[tokio::test(flavor = "current_thread")]
async fn stream_fails_at_max_result_bytes() {
    let (_d, db) = file_sqlite().await;
    db.exec_with_params("create table t (id text)", &[])
        .await
        .unwrap();
    db.exec_with_params(
        "insert into t select 'aaaaaaaaaa' union all select 'bbbbbbbbbb'",
        &[],
    )
    .await
    .unwrap();
    let pol = DbAccessSection {
        defaults: DbAccess {
            max_result_bytes: Some(20),
            ..Default::default()
        },
        ..Default::default()
    };
    let b = bridge_with(db, SchemaRegistry::new(), pol, false, false);
    let js = r#"(async () => {
            try {
              const rows = [];
              for await (const r of db.stream("select id from t")) rows.push(r);
              json.ok({n: rows.length});
            } catch (e) { json.ok({code: e.code}); }
          })().catch(e => json.fail(500, String(e)));"#;
    let (_c, _m, data) = run(&b, RequestInfo::default(), js).await;
    assert_eq!(data["code"], json!("OJ_DB_CAPACITY"), "{data}");
}

/// 终审 I1（复核定级 Important）：fail 档触顶后经迭代 catch 抛错——`it.info()` 仍须
/// 拿到 {rows,bytes,truncated} 快照（done 分支有快照、catch 分支当时漏了，info 退化为 null）。
#[tokio::test(flavor = "current_thread")]
async fn stream_info_snapshot_survives_fail_row_error() {
    let (_d, db) = file_sqlite().await;
    db.exec_with_params("create table t (id text)", &[])
        .await
        .unwrap();
    db.exec_with_params(
        "insert into t select 'aaaaaaaaaa' union all select 'bbbbbbbbbb'",
        &[],
    )
    .await
    .unwrap();
    let pol = DbAccessSection {
        defaults: DbAccess {
            max_result_bytes: Some(20),
            ..Default::default()
        },
        ..Default::default()
    };
    let b = bridge_with(db, SchemaRegistry::new(), pol, false, false);
    // fail 档触顶 → 迭代 next() 抛 OJ_DB_CAPACITY → 之后 info() 必须仍给快照。
    let js = r#"(async () => {
            const st = db.stream("select id from t");
            const it = st[Symbol.asyncIterator]();
            let code = null;
            try {
              for (;;) { const r = await it.next(); if (r.done) break; }
            } catch (e) { code = e.code; }
            const inf = await st.info();
            json.ok({code, info: inf});
          })().catch(e => json.fail(500, String(e)));"#;
    let (_c, _m, data) = run(&b, RequestInfo::default(), js).await;
    assert_eq!(data["code"], json!("OJ_DB_CAPACITY"), "{data}");
    let info = &data["info"];
    assert!(info.is_object(), "错误路径 info() 不得退化为 null：{data}");
    assert_eq!(info["rows"], json!(1), "{data}");
    assert!(
        info["bytes"].as_u64().unwrap_or(0) >= 20,
        "bytes 应含触顶行计数：{data}"
    );
    assert_eq!(info["truncated"], json!(false), "{data}");
}

/// REQ-05 Q6：db.stream truncate 档——干净 done + info() 显式信令（rows/bytes/truncated）。
#[tokio::test(flavor = "current_thread")]
async fn stream_truncates_at_max_result_bytes_with_info_signal() {
    let (_d, db) = file_sqlite().await;
    db.exec_with_params("create table t (id text)", &[])
        .await
        .unwrap();
    db.exec_with_params(
        "insert into t select 'aaaaaaaaaa' union all select 'bbbbbbbbbb'",
        &[],
    )
    .await
    .unwrap();
    let pol = DbAccessSection {
        defaults: DbAccess {
            max_result_bytes: Some(20),
            on_exceed: Some(OnExceed::Truncate),
            ..Default::default()
        },
        ..Default::default()
    };
    let b = bridge_with(db, SchemaRegistry::new(), pol, false, false);
    let js = r#"(async () => {
            const it = db.stream("select id from t");
            const rows = [];
            for await (const r of it) rows.push(r);
            const inf = await it.info();
            json.ok({n: rows.length, truncated: inf.truncated, bytes: inf.bytes});
          })().catch(e => json.fail(500, String(e)));"#;
    let (_c, m, data) = run(&b, RequestInfo::default(), js).await;
    assert_eq!(data["n"], json!(1), "msg={m} data={data}");
    assert_eq!(data["truncated"], json!(true), "msg={m} data={data}");
    assert!(
        data["bytes"].as_i64().unwrap_or(0) > 20,
        "msg={m} data={data}"
    );
}

/// REQ-05 Q6：db.stream 语句超时（pump 全程竞速）：慢流（200ms/行）+ timeoutMs 350
/// → ~350ms 抛 OJ_DB_TIMEOUT，不是取完 10 行（2s）。
struct SlowStreamAccessor;
#[async_trait::async_trait]
impl DataAccessor for SlowStreamAccessor {
    async fn query_with_params(
        &self,
        _sql: &str,
        _params: &[serde_json::Value],
    ) -> super::super::BridgeResult<Vec<crate::bridge::db::Row>> {
        Ok(vec![])
    }
    async fn exec_with_params(
        &self,
        _sql: &str,
        _params: &[serde_json::Value],
    ) -> super::super::BridgeResult<i64> {
        Ok(0)
    }
    async fn stream_query(
        &self,
        _sql: &str,
        _params: &[serde_json::Value],
    ) -> super::super::BridgeResult<
        std::pin::Pin<Box<dyn futures::Stream<Item = Result<serde_json::Value, String>> + Send>>,
    > {
        let s = async_stream::stream! {
            for i in 0..10 {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                yield Ok(serde_json::json!({"n": i}));
            }
        };
        Ok(Box::pin(s))
    }
}

#[tokio::test(flavor = "current_thread")]
async fn stream_statement_timeout_via_pump_race() {
    let pol = DbAccessSection {
        defaults: DbAccess {
            statement_timeout_ms: Some(60_000), // 配置宽裕；调用侧收紧到 350ms
            ..Default::default()
        },
        ..Default::default()
    };
    let b = bridge_with(
        Arc::new(SlowStreamAccessor),
        SchemaRegistry::new(),
        pol,
        false,
        false,
    );
    let js = r#"(async () => {
            const t0 = Date.now();
            try {
              const rows = [];
              for await (const r of db.stream("select 1", null, {timeoutMs: 350})) rows.push(r);
              json.ok({err: "no-timeout", n: rows.length, ms: Date.now() - t0});
            } catch (e) { json.ok({code: e.code, ms: Date.now() - t0}); }
          })().catch(e => json.fail(500, String(e)));"#;
    let (_c, _m, data) = run(&b, RequestInfo::default(), js).await;
    assert_eq!(data["code"], json!("OJ_DB_TIMEOUT"), "{data}");
    let ms = data["ms"].as_i64().unwrap_or(9999);
    assert!(ms < 2_000, "流超时必须在 ~350ms 触发：{ms}ms");
}

/// `stream` 达 `max_rows` → 提前 done（不是截断：真省后端拉取）。
#[tokio::test(flavor = "current_thread")]
async fn stream_stops_early_at_max_rows() {
    let (_d, db) = file_sqlite().await;
    db.exec_with_params("create table t (id integer)", &[])
        .await
        .unwrap();
    db.exec_with_params(
            "insert into t select 1 union all select 2 union all select 3 union all select 4 union all select 5",
            &[],
        )
        .await
        .unwrap();
    let pol = DbAccessSection {
        defaults: DbAccess {
            max_rows: Some(2),
            ..Default::default()
        },
        ..Default::default()
    };
    let b = bridge_with(db, SchemaRegistry::new(), pol, false, false);
    let js = r#"(async () => {
            try {
              const rows = [];
              for await (const r of db.stream("select id from t")) rows.push(r);
              json.ok({n: rows.length});
            } catch (e) { json.ok({err: String(e)}); }
          })().catch(e => json.fail(500, String(e)));"#;
    let (_, _, data) = run(&b, RequestInfo::default(), js).await;
    assert_eq!(data["n"], json!(2), "stream 须在 max_rows 提前终止：{data}");
}

/// 构造器路径（不走词法）也吃 `max_rows`：生效值 = min(声明 limit, max_rows)。
#[tokio::test(flavor = "current_thread")]
async fn builder_path_applies_max_rows() {
    let (_d, db) = file_sqlite().await;
    db.exec_with_params("create table t (id integer, name text)", &[])
        .await
        .unwrap();
    db.exec_with_params(
            "insert into t select 1,'a' union all select 2,'b' union all select 3,'c' union all select 4,'d'",
            &[],
        )
        .await
        .unwrap();
    let pol = DbAccessSection {
        defaults: DbAccess {
            max_rows: Some(2),
            ..Default::default()
        },
        ..Default::default()
    };
    let reg = SchemaRegistry::new().table("t", &["id"], &["id", "name"]);
    let b = bridge_with(db, reg, pol, false, false);
    let js = r#"(async () => {
            try {
              const r = await db.table("t").select(["id"]).orderBy([{field:"id",dir:"asc"}]).limit(10).all();
              json.ok({n: r.length});
            } catch (e) { json.ok({err: String(e)}); }
          })().catch(e => json.fail(500, String(e)));"#;
    let (_, _, data) = run(&b, RequestInfo::default(), js).await;
    assert_eq!(
        data["n"],
        json!(2),
        "构造器 limit 与 max_rows 取小（此处 max_rows 更小）：{data}"
    );
}

/// 自省出口只列**已授权**的库：前端不该列出点了才报错的连接（假入口）。
#[tokio::test(flavor = "current_thread")]
async fn allowed_lists_only_granted_aliases() {
    let (_d, db) = file_sqlite().await;
    let pol = DbAccessSection {
        defaults: DbAccess {
            enforce: Enforce::Deny,
            read_only: true,
            max_rows: Some(5),
            allow: AllowList {
                roles: Some(vec!["admin".into()]),
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let b = bridge_with(db, SchemaRegistry::new(), pol, false, false);
    let js = r#"json.ok({list: db.allowed()});"#;
    // 未授权 → 空表
    let (_, _, data) = run(&b, RequestInfo::default(), js).await;
    assert_eq!(data["list"], json!([]), "未授权不得出现在列表里：{data}");
    // 授权 → 单条，能力位如实回传
    let (_, _, data) = run(&b, req_user(&["admin"]), js).await;
    let list = data["list"].as_array().unwrap();
    assert_eq!(list.len(), 1, "{data}");
    assert_eq!(list[0]["alias"], json!("default"));
    assert_eq!(list[0]["read_only"], json!(true));
    assert_eq!(list[0]["max_rows"], json!(5));
    assert_eq!(list[0]["enforce"], json!("deny"));
}

// ---- U-1 运行期动态授权（v0.1.93） ----
//
// 对抗性口径：grant 必须真的「拒」未授权主体（`warn` 档只记日志、不拒——那不是授权）；
// 必须不能放宽 config 的安全位；必须不能被无特权主体调用。

/// `off` 档（默认）上一旦 grant，该别名立刻变成**白名单**：命中放行、未命中拒。
#[tokio::test(flavor = "current_thread")]
async fn runtime_grant_turns_off_tier_into_a_whitelist() {
    let (_d, db) = file_sqlite().await;
    db.exec_with_params("create table t (id integer)", &[])
        .await
        .unwrap();
    let b = bridge_with_client(
        db,
        DbAccessSection::default(),
        DbClientSection {
            enabled: true,
            ..Default::default()
        },
    );
    let grant = r#"(async () => {
            const r = await db.access.grant("default", { roles: ["admin"] });
            json.ok({ r, epoch: db.epoch("no-such-alias") });
          })().catch(e => json.fail(500, String(e)));"#;
    let (code, msg, data) = run(&b, req_user(&["admin"]), grant).await;
    assert_eq!(code, 0, "grant 本身不得失败：{msg}");
    assert_eq!(data["r"]["ok"], json!(true), "{data}");
    // 未知别名的 epoch 恒为 0（不是错误、不是随机值——JS 侧据此判断缓存新鲜度）。
    assert_eq!(data["epoch"], json!(0), "{data}");
    // 命中 → 放行
    let (_, _, data) = run(&b, req_user(&["admin"]), JS_READ).await;
    assert_eq!(data["n"], json!(0), "grant 命中应放行：{data}");
    // 未命中 → 拒（grant 之前是 off = 全员放通；grant 之后必须拒，否则「授权」是假的）
    let (_, _, data) = run(&b, req_user(&["guest"]), JS_READ).await;
    assert!(
        data["err"]
            .as_str()
            .unwrap_or_default()
            .contains("db_not_granted"),
        "未授权角色不得放行：{data}"
    );
    let (_, _, data) = run(&b, RequestInfo::default(), JS_READ).await;
    assert!(
        data["err"]
            .as_str()
            .unwrap_or_default()
            .contains("db_not_granted"),
        "无身份不得放行：{data}"
    );
    // `db.allowed()` 与授权同源：授权角色看得到，未授权角色看到的是空表（不留假入口）。
    let (_, _, data) = run(&b, req_user(&["admin"]), "json.ok({list: db.allowed()});").await;
    assert_eq!(data["list"].as_array().unwrap().len(), 1, "{data}");
    let (_, _, data) = run(&b, req_user(&["guest"]), "json.ok({list: db.allowed()});").await;
    assert_eq!(data["list"], json!([]), "未授权不得进连接树：{data}");
}

/// grant **只注入放行维**：`opts.readOnly=false` 也不得放开 config 的 `read_only`。
#[tokio::test(flavor = "current_thread")]
async fn runtime_grant_never_relaxes_config_policy_bits() {
    let (_d, db) = file_sqlite().await;
    db.exec_with_params("create table t (id integer)", &[])
        .await
        .unwrap();
    let pol = DbAccessSection {
        defaults: DbAccess {
            enforce: Enforce::Deny,
            read_only: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let b = bridge_with_client(
        db,
        pol,
        DbClientSection {
            enabled: true,
            ..Default::default()
        },
    );
    let grant = r#"(async () => {
            const r = await db.access.grant("default", { roles: ["admin"] }, { readOnly: false });
            json.ok({ r });
          })().catch(e => json.fail(500, String(e)));"#;
    let (code, msg, _) = run(&b, req_user(&["admin"]), grant).await;
    assert_eq!(code, 0, "{msg}");
    // config 的 read_only 是安全位，运行期 grant 不得削它 → 写仍被拒（原因仍是 db_read_only，
    // 不是 db_not_granted：说明授权已过、拦在策略位上）。
    let (_, _, data) = run(&b, req_user(&["admin"]), JS_WRITE).await;
    let err = data["err"].as_str().unwrap_or_default();
    assert!(
        err.contains("db_read_only"),
        "grant 不得放开 read_only：{err}"
    );
    assert!(!err.contains("db_not_granted"), "授权应已通过：{err}");
}

/// config 已有白名单时，grant 是**逐维并集**：不挤掉 config 已授权的角色。
#[tokio::test(flavor = "current_thread")]
async fn runtime_grant_unions_with_config_allow() {
    let (_d, db) = file_sqlite().await;
    db.exec_with_params("create table t (id integer)", &[])
        .await
        .unwrap();
    let pol = DbAccessSection {
        defaults: DbAccess {
            enforce: Enforce::Deny,
            allow: AllowList {
                roles: Some(vec!["admin".into()]),
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let b = bridge_with_client(
        db,
        pol,
        DbClientSection {
            enabled: true,
            ..Default::default()
        },
    );
    let grant = r#"(async () => {
            const r = await db.access.grant("default", { roles: ["guest"] });
            json.ok({ r });
          })().catch(e => json.fail(500, String(e)));"#;
    let (code, msg, _) = run(&b, req_user(&["admin"]), grant).await;
    assert_eq!(code, 0, "{msg}");
    // config 里的 admin 仍在（并集，不是替换）
    let (_, _, data) = run(&b, req_user(&["admin"]), JS_READ).await;
    assert_eq!(
        data["n"],
        json!(0),
        "config 已授权的主语不得被 grant 挤掉：{data}"
    );
    // 新授予的 guest 也进得来
    let (_, _, data) = run(&b, req_user(&["guest"]), JS_READ).await;
    assert_eq!(data["n"], json!(0), "grant 追加的主语应放行：{data}");
    // 都没声明的角色仍被拒（并集不等于「全员放通」）
    let (_, _, data) = run(&b, req_user(&["ops"]), JS_READ).await;
    assert!(
        data["err"]
            .as_str()
            .unwrap_or_default()
            .contains("db_not_granted"),
        "{data}"
    );
}

/// revoke 只撤运行期项、回落 config；没有运行期项时**显式报错**（不静默、不代表删了 config）。
#[tokio::test(flavor = "current_thread")]
async fn revoke_without_runtime_grant_is_explicit_error() {
    let (_d, db) = file_sqlite().await;
    db.exec_with_params("create table t (id integer)", &[])
        .await
        .unwrap();
    let b = bridge_with_client(
        db,
        DbAccessSection::default(),
        DbClientSection {
            enabled: true,
            ..Default::default()
        },
    );
    let js = r#"(async () => {
            try { await db.access.revoke("default"); json.ok({ok: true}); }
            catch (e) { json.ok({err: String(e)}); }
          })().catch(e => json.fail(500, String(e)));"#;
    let (_, _, data) = run(&b, req_user(&["admin"]), js).await;
    let err = data["err"].as_str().unwrap_or_default();
    assert!(
        err.contains("no runtime access grant"),
        "应显式说明无运行期项：{err}"
    );
    // grant → revoke → 回落 config（off = 全员放通）
    let round = r#"(async () => {
            await db.access.grant("default", { roles: ["admin"] });
            await db.access.revoke("default");
            json.ok({ok: true});
          })().catch(e => json.fail(500, String(e)));"#;
    let (code, msg, _) = run(&b, req_user(&["admin"]), round).await;
    assert_eq!(code, 0, "{msg}");
    let (_, _, data) = run(&b, RequestInfo::default(), JS_READ).await;
    assert_eq!(
        data["n"],
        json!(0),
        "revoke 后应回落 config（off 档全员放通）：{data}"
    );
}

/// grant / revoke 走 `db_client.allow` 特权门：未授权主体不得改他人的运行期授权。
#[tokio::test(flavor = "current_thread")]
async fn runtime_grant_requires_client_gate() {
    let (_d, db) = file_sqlite().await;
    db.exec_with_params("create table t (id integer)", &[])
        .await
        .unwrap();
    let b = bridge_with_client(
        db,
        DbAccessSection::default(),
        DbClientSection {
            enabled: true,
            allow: AllowList {
                roles: Some(vec!["admin".into()]),
                ..Default::default()
            },
            ..Default::default()
        },
    );
    let js = r#"(async () => {
            try { await db.access.grant("default", { roles: ["guest"] }); json.ok({ok: true}); }
            catch (e) { json.ok({err: String(e)}); }
          })().catch(e => json.fail(500, String(e)));"#;
    let (_, _, data) = run(&b, req_user(&["guest"]), js).await;
    let err = data["err"].as_str().unwrap_or_default();
    assert!(
        err.contains("db_client_not_authorized"),
        "越权 grant 必须被门挡：{err}"
    );
    // 特权主体可以
    let (_, _, data) = run(&b, req_user(&["admin"]), js).await;
    assert_eq!(data["ok"], json!(true), "{data}");
}

/// warn 档：只告警放行（灰度用），语句照常执行。
#[tokio::test(flavor = "current_thread")]
async fn warn_tier_passes_through() {
    let (_d, db) = file_sqlite().await;
    db.exec_with_params("create table t (id integer)", &[])
        .await
        .unwrap();
    let pol = DbAccessSection {
        defaults: DbAccess {
            enforce: Enforce::Warn,
            read_only: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let b = bridge_with(db, SchemaRegistry::new(), pol, false, false);
    let (_, _, data) = run(&b, RequestInfo::default(), JS_WRITE).await;
    assert_eq!(data["ok"], json!(true), "warn 档不得拦：{data}");
}

// ───────── db 结构内省（L2，v0.1.90）行为测试 ─────────

/// off 档下，三 op 返回的形状 / 排序 / 布尔位 / 默认值与 sqlite 实测一致。
#[tokio::test(flavor = "current_thread")]
async fn introspect_columns_indexes_fk_shape() {
    let (_d, db) = file_sqlite().await;
    let b = bridge_with(
        db,
        SchemaRegistry::new(),
        DbAccessSection::default(),
        false,
        false,
    );
    let (code, _m, data) = run(
            &b,
            RequestInfo::default(),
            r#"(async () => {
              await db.exec("create table if not exists parent (pid integer primary key, pname text)");
              await db.exec("create table if not exists child (cid integer primary key, pid integer references parent(pid), name text default 'x', extra text)");
              await db.exec("create index if not exists ix_child_extra on child(extra)");
              await db.exec("create unique index if not exists ux_child_cid_extra on child(cid, extra)");
              const cols = await db.listColumns('child');
              const idx = await db.listIndexes('child');
              const fk = await db.listForeignKeys('child');
              json.ok({cols, idx, fk});
            })().catch(e => json.fail(500, String(e)));"#,
        )
        .await;
    assert_eq!(code, 0, "{data}");

    // 列：声明序 = cid / pid / name / extra；PK 只认 listColumns。
    let cols = data["cols"].as_array().unwrap();
    assert_eq!(cols.len(), 4, "{cols:?}");
    assert_eq!(cols[0]["name"], json!("cid"));
    assert_eq!(cols[0]["primaryKey"], json!(true));
    assert_eq!(cols[0]["nativeType"], json!("INTEGER"));
    assert_eq!(cols[1]["name"], json!("pid"));
    assert_eq!(cols[1]["primaryKey"], json!(false));
    assert_eq!(cols[1]["nullable"], json!(true));
    assert_eq!(cols[2]["name"], json!("name"));
    // sqlite 的 pragma_table_info.dflt_value 是原始 DDL 文本（含引号）。
    assert_eq!(cols[2]["default"], json!("'x'"));
    assert_eq!(cols[3]["name"], json!("extra"));

    // 索引：按名聚合列；ux_ 是 unique（origin=u）、ix_ 是普通（origin=c）。
    let idx = data["idx"].as_array().unwrap();
    assert!(idx.len() >= 2, "{idx:?}");
    let ux = idx
        .iter()
        .find(|i| i["name"] == json!("ux_child_cid_extra"))
        .unwrap();
    assert_eq!(ux["unique"], json!(true));
    assert_eq!(ux["origin"], json!("u"));
    assert_eq!(ux["columns"], json!(["cid", "extra"]));
    let ix = idx
        .iter()
        .find(|i| i["name"] == json!("ix_child_extra"))
        .unwrap();
    assert_eq!(ix["unique"], json!(false));
    assert_eq!(ix["origin"], json!("c"));
    assert_eq!(ix["columns"], json!(["extra"]));

    // 外键：sqlite 无名 → name 为 null；按位配对 pid -> pid。
    let fk = data["fk"].as_array().unwrap();
    assert_eq!(fk.len(), 1, "{fk:?}");
    assert_eq!(fk[0]["name"], json!(null));
    assert_eq!(fk[0]["columns"], json!(["pid"]));
    assert_eq!(fk[0]["refTable"], json!("parent"));
    assert_eq!(fk[0]["refColumns"], json!(["pid"]));
}

/// 含 `.` 的表名（sqlite）被挡为跨库内省 → 空数组，而非 500。
#[tokio::test(flavor = "current_thread")]
async fn introspect_dotted_table_returns_empty() {
    let (_d, db) = file_sqlite().await;
    let b = bridge_with(
        db,
        SchemaRegistry::new(),
        DbAccessSection::default(),
        false,
        false,
    );
    let (code, _m, data) = run(
        &b,
        RequestInfo::default(),
        r#"(async () => {
              const c = await db.listColumns('other.secrets');
              json.ok({c});
            })().catch(e => json.fail(500, String(e)));"#,
    )
    .await;
    assert_eq!(code, 0, "{data}");
    assert_eq!(data["c"], json!([]), "含 . 的表名必须空数组而非报错");
}

/// 含 NUL / 控制字符的表名 → 空数组（不让驱动把畸形输入变 500）。
#[tokio::test(flavor = "current_thread")]
async fn introspect_control_char_table_returns_empty() {
    let (_d, db) = file_sqlite().await;
    let b = bridge_with(
        db,
        SchemaRegistry::new(),
        DbAccessSection::default(),
        false,
        false,
    );
    let (code, _m, data) = run(
        &b,
        RequestInfo::default(),
        r#"(async () => {
              const c = await db.listColumns("ta\u0000ble");
              json.ok({c});
            })().catch(e => json.fail(500, String(e)));"#,
    )
    .await;
    assert_eq!(code, 0, "{data}");
    assert_eq!(data["c"], json!([]));
}

/// 正对照：`_` 在表名里是字面量（等值匹配），不是 LIKE 通配符——能查到。
#[tokio::test(flavor = "current_thread")]
async fn underscore_table_name_is_literal_not_like() {
    let (_d, db) = file_sqlite().await;
    let b = bridge_with(
        db,
        SchemaRegistry::new(),
        DbAccessSection::default(),
        false,
        false,
    );
    let (code, _m, data) = run(
        &b,
        RequestInfo::default(),
        r#"(async () => {
              await db.exec("create table if not exists foo_bar (a integer)");
              const c = await db.listColumns('foo_bar');
              json.ok({c});
            })().catch(e => json.fail(500, String(e)));"#,
    )
    .await;
    assert_eq!(code, 0, "{data}");
    let c = data["c"].as_array().unwrap();
    assert_eq!(c.len(), 1, "{c:?}");
    assert_eq!(c[0]["name"], json!("a"));
}

/// 授权维：deny + allow.roles:[admin] 下，无身份 / 非 admin → db_not_granted；
/// admin → 放行。listTables 一并收紧（安全 B1）。
#[tokio::test(flavor = "current_thread")]
async fn introspect_granted_only_blocks_unauthorized() {
    let (_d, db) = file_sqlite().await;
    db.exec_with_params("create table t (id integer)", &[])
        .await
        .unwrap();
    let pol = DbAccessSection {
        defaults: DbAccess {
            enforce: Enforce::Deny,
            allow: AllowList {
                roles: Some(vec!["admin".to_string()]),
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let b = bridge_with(db, SchemaRegistry::new(), pol, false, false);

    // 无身份 → 拒
    let (code, msg, _) = run(
            &b,
            RequestInfo::default(),
            r#"(async () => { const c = await db.listColumns('t'); json.ok({c}); })().catch(e => json.fail(500, String(e)));"#,
        )
        .await;
    assert_ne!(code, 0, "无身份应被拒");
    assert!(msg.contains("db_not_granted"), "{msg}");

    // 非 admin 角色 → 拒
    let (code2, msg2, _) = run(
            &b,
            req_user(&["viewer"]),
            r#"(async () => { const c = await db.listTables(); json.ok({c}); })().catch(e => json.fail(500, String(e)));"#,
        )
        .await;
    assert_ne!(code2, 0, "非 admin 应被拒");
    assert!(msg2.contains("db_not_granted"), "{msg2}");

    // admin → 放行
    let (code3, _m, data3) = run(
            &b,
            req_user(&["admin"]),
            r#"(async () => { const c = await db.listColumns('t'); json.ok({c}); })().catch(e => json.fail(500, String(e)));"#,
        )
        .await;
    assert_eq!(code3, 0, "{data3}");
    assert_eq!(data3["c"].as_array().unwrap().len(), 1, "{data3:?}");
}

/// `db.asSystem()` **不豁免**内省授权（与 evaluate 不读 ReqState.system 同口径）。
#[tokio::test(flavor = "current_thread")]
async fn introspect_as_system_does_not_exempt() {
    let (_d, db) = file_sqlite().await;
    db.exec_with_params("create table t (id integer)", &[])
        .await
        .unwrap();
    let pol = DbAccessSection {
        defaults: DbAccess {
            enforce: Enforce::Deny,
            allow: AllowList {
                roles: Some(vec!["admin".to_string()]),
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let b = bridge_with(db, SchemaRegistry::new(), pol, false, false);
    let (code, msg, _) = run(
            &b,
            RequestInfo::default(),
            r#"(async () => { const c = await db.asSystem().listColumns('t'); json.ok({c}); })().catch(e => json.fail(500, String(e)));"#,
        )
        .await;
    assert_ne!(code, 0, "asSystem 不得豁免内省授权");
    assert!(msg.contains("db_not_granted"), "{msg}");
}

/// warn 档：未授权主体不拦，放行并通过。
#[tokio::test(flavor = "current_thread")]
async fn introspect_warn_tier_passes_through() {
    let (_d, db) = file_sqlite().await;
    db.exec_with_params("create table t (id integer)", &[])
        .await
        .unwrap();
    let pol = DbAccessSection {
        defaults: DbAccess {
            enforce: Enforce::Warn,
            allow: AllowList {
                roles: Some(vec!["admin".to_string()]),
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let b = bridge_with(db, SchemaRegistry::new(), pol, false, false);
    let (code, _m, data) = run(
            &b,
            RequestInfo::default(),
            r#"(async () => { const c = await db.listColumns('t'); json.ok({c}); })().catch(e => json.fail(500, String(e)));"#,
        )
        .await;
    assert_eq!(code, 0, "warn 档须放行：{data}");
    assert_eq!(data["c"].as_array().unwrap().len(), 1, "{data:?}");
}

/// 事务内路径：db.tx 回调对象同样支持三 op（走活跃事务连接）。
#[tokio::test(flavor = "current_thread")]
async fn introspect_in_tx_uses_tx_connection() {
    let (_d, db) = file_sqlite().await;
    db.exec_with_params("create table t (id integer primary key, name text)", &[])
        .await
        .unwrap();
    let b = bridge_with(
        db,
        SchemaRegistry::new(),
        DbAccessSection::default(),
        false,
        false,
    );
    let (code, _m, data) = run(
        &b,
        RequestInfo::default(),
        r#"(async () => {
              const r = await db.tx(async (t) => {
                const cols = await t.listColumns('t');
                const idx = await t.listIndexes('t');
                const fk = await t.listForeignKeys('t');
                return { cols, idx, fk };
              });
              json.ok(r);
            })().catch(e => json.fail(500, String(e)));"#,
    )
    .await;
    assert_eq!(code, 0, "{data}");
    assert_eq!(data["cols"].as_array().unwrap().len(), 2, "{data:?}");
    assert_eq!(data["fk"].as_array().unwrap().len(), 0, "{data:?}");
}

/// 会话级改写（USE / SET search_path / ATTACH）被分类为 Write：deny + read_only 站点
/// 下 `db.exec("USE other")` 被拦（安全 B2 处置①）。
#[tokio::test(flavor = "current_thread")]
async fn session_switch_classified_as_write_blocks_under_deny() {
    let (_d, db) = file_sqlite().await;
    db.exec_with_params("create table t (id integer)", &[])
        .await
        .unwrap();
    let pol = DbAccessSection {
        defaults: DbAccess {
            enforce: Enforce::Deny,
            read_only: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let b = bridge_with(db, SchemaRegistry::new(), pol, false, false);
    // sqlite 无 USE；用 ATTACH 验证（同属会话级改写 → Write）。
    let (code, msg, _) = run(
            &b,
            RequestInfo::default(),
            r#"(async () => { await db.exec("ATTACH DATABASE ':memory:' AS other"); json.ok({ok: true}); })().catch(e => json.fail(500, String(e)));"#,
        )
        .await;
    assert_ne!(code, 0, "ATTACH 在 deny 站点应被拦");
    assert!(
        msg.contains("db_read_only") || msg.contains("db_not_granted"),
        "{msg}"
    );
}

/// G9：危险语句确认门——配 token 后，无 WHERE 的写无头/错头拒、对头放行且落审计。
#[tokio::test(flavor = "current_thread")]
async fn dangerous_statements_require_confirm_token() {
    let (_d, db) = file_sqlite().await;
    db.exec_with_params("create table t (id integer)", &[])
        .await
        .unwrap();
    let policy = DbAccessSection {
        defaults: DbAccess {
            allow_dangerous: true,
            dangerous_confirm_token: Some("s3cret".into()),
            ..Default::default()
        },
        by_db: HashMap::new(),
    };
    let b = bridge_with_client(
        db,
        policy,
        DbClientSection {
            enabled: true,
            ..Default::default()
        },
    );
    const JS: &str = r#"(async () => {
            await db.exec("delete from t");
            json.ok({ ok: true });
          })().catch(e => json.fail(500, String(e)));"#;
    // 无头 → 拒（enforce=off 也要生效：显式配 token = 要它生效）
    let (code, msg, _) = run(&b, RequestInfo::default(), JS).await;
    assert_ne!(code, 0, "无确认头应被拒");
    assert!(msg.contains("confirmation"), "{msg}");
    // 错头 → 拒
    let bad = RequestInfo {
        headers: HashMap::from([("X-OJ-Confirm".to_string(), "wrong".to_string())]),
        ..RequestInfo::default()
    };
    let (code, msg, _) = run(&b, bad, JS).await;
    assert_ne!(code, 0, "错头应被拒");
    assert!(msg.contains("confirmation"), "{msg}");
    // 对头（键大小写不同）→ 放行 + 审计
    let ok = RequestInfo {
        headers: HashMap::from([("x-oj-confirm".to_string(), "s3cret".to_string())]),
        ..RequestInfo::default()
    };
    let (code, _m, data) = run(&b, ok, JS).await;
    assert_eq!(code, 0, "对头应放行");
    assert_eq!(data["ok"], serde_json::json!(true), "{data}");
    // 放行落 DB_DANGEROUS_CONFIRM，audit.query 可检索
    let (_, _, data) = run(
        &b,
        RequestInfo::default(),
        r#"audit.query({ kind: "DB_DANGEROUS_CONFIRM" }).then((r) => json.ok({ n: r.length }))"#,
    )
    .await;
    assert!(data["n"].as_u64().unwrap() >= 1, "{data}");
}

/// G9：确认 token 是 config 安全位——运行期 grant 后仍生效（防运行期绕过）。
#[tokio::test(flavor = "current_thread")]
async fn confirm_token_survives_runtime_grant() {
    let (_d, db) = file_sqlite().await;
    db.exec_with_params("create table t (id integer)", &[])
        .await
        .unwrap();
    let policy = DbAccessSection {
        defaults: DbAccess {
            allow_dangerous: true,
            dangerous_confirm_token: Some("s3cret".into()),
            ..Default::default()
        },
        by_db: HashMap::new(),
    };
    let b = bridge_with_client(
        db,
        policy,
        DbClientSection {
            enabled: true,
            ..Default::default()
        },
    );
    // 先 grant（带确认头），再不带头试危险语句 → 仍拒
    let (code, msg, _) = run(
        &b,
        RequestInfo {
            headers: HashMap::from([("x-oj-confirm".to_string(), "s3cret".to_string())]),
            ..RequestInfo::default()
        },
        r#"(async () => {
            const r = await db.access.grant("default", { roles: ["admin"] });
            json.ok({ r });
          })().catch(e => json.fail(500, String(e)))"#,
    )
    .await;
    assert_eq!(code, 0, "grant 应成功：{msg}");
    // grant 后主体命中 allow，但确认门仍要求头
    let (code2, msg2, _) = run(
        &b,
        RequestInfo::default(),
        r#"(async () => { await db.exec("delete from t"); json.ok({ ok: true }); })()
               .catch(e => json.fail(500, String(e)))"#,
    )
    .await;
    assert_ne!(code2, 0, "grant 后确认门仍应生效");
    assert!(msg2.contains("confirmation"), "{msg2}");
}
