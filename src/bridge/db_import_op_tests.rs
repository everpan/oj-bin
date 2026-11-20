use crate::bridge::{Bridge, Extras, InMemoryAccessor, InMemoryKV, SchemaRegistry, SqlGuard};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;

fn import_bridge(
    registry: SchemaRegistry,
    import_section: super::super::db_policy::DbImportSection,
    sql_guard: SqlGuard,
) -> Bridge {
    let mem = Arc::new(InMemoryAccessor::new());
    Bridge::with_dbs_and_loader(
        HashMap::from([("default".to_string(), mem as _)]),
        Arc::new(InMemoryKV::new()),
        registry,
        false,
        None,
        Extras {
            import_section: Some(Arc::new(import_section)),
            db_access: Some(Arc::new(Default::default())),
            sql_guard,
            ..Default::default()
        },
    )
}

fn scoped_registry() -> SchemaRegistry {
    SchemaRegistry::new().table("orders", &["id"], &["id", "tenant_id", "name"])
}

/// 结构化错误收割：code + capability + data.format。
async fn import_err(b: &Bridge, args: &str) -> Value {
    let js = format!(
        r#"(async () => {{
                 const r = await db.import({args}).then(
                   () => null,
                   (e) => ({{ code: e.code, capability: e.data && e.data.capability, format: e.data && e.data.format, msg: String(e.message || e) }}));
                 json.ok(r);
               }})().catch(e => json.fail(500, String(e)));"#,
    );
    let cap = b.run_with(&js, Default::default()).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    v["data"].clone()
}

/// scoped 表 + guard=Warn/Deny 两档 → 结构化拒 capability=import_under_guard。
#[tokio::test(flavor = "current_thread")]
async fn import_guard_scoped_table_fail_closed() {
    for guard in [SqlGuard::Warn, SqlGuard::Deny] {
        let b = import_bridge(scoped_registry(), Default::default(), guard);
        let d = import_err(
                &b,
                r#"{ table: "orders", format: "jsonl", columns: ["id", "name"], rows: [{ id: 1, name: "a" }] }"#,
            )
            .await;
        assert_eq!(d["code"], json!("OJ_DB_UNSUPPORTED"), "{d}");
        assert_eq!(d["capability"], json!("import_under_guard"), "{d}");
    }
}

/// 未登记 + Deny → 拒（镜像裸 SQL「cannot verify tenant scope」）；
/// 未登记 + Warn → warn 放行（任务建起）。
#[tokio::test(flavor = "current_thread")]
async fn import_guard_unregistered_warn_passes_deny_rejects() {
    let deny = import_bridge(SchemaRegistry::new(), Default::default(), SqlGuard::Deny);
    let d = import_err(
        &deny,
        r#"{ table: "ghost", format: "jsonl", columns: ["id"], rows: [{ id: 1 }] }"#,
    )
    .await;
    assert_eq!(d["code"], json!(null), "{d}"); // 非结构化——守卫 generic 错误
    assert!(
        d["msg"]
            .as_str()
            .unwrap()
            .contains("cannot verify tenant scope"),
        "{d}"
    );
    let warn = import_bridge(SchemaRegistry::new(), Default::default(), SqlGuard::Warn);
    let js = r#"(async () => {
             const { taskId } = await db.import({ table: "ghost", format: "jsonl", columns: ["id"], rows: [{ id: 1 }] });
             json.ok({ taskId });
           })().catch(e => json.fail(500, String(e)));"#;
    let cap = warn.run_with(js, Default::default()).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert!(
        v["data"]["taskId"].as_str().unwrap().starts_with("imp_"),
        "{v}"
    );
}

/// 已登记但非 scoped + Deny → 放行（同裸 SQL）。
#[tokio::test(flavor = "current_thread")]
async fn import_guard_registered_non_scoped_passes() {
    let reg = SchemaRegistry::new().table("plain", &["id"], &["id", "name"]);
    let b = import_bridge(reg, Default::default(), SqlGuard::Deny);
    let js = r#"(async () => {
             const { taskId } = await db.import({ table: "plain", format: "jsonl", columns: ["id", "name"], rows: [{ id: 1, name: "a" }] });
             json.ok({ taskId });
           })().catch(e => json.fail(500, String(e)));"#;
    let cap = b.run_with(js, Default::default()).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert!(
        v["data"]["taskId"].as_str().unwrap().starts_with("imp_"),
        "{v}"
    );
}

/// rs.system=true + scoped 表 + Deny → 预检与 check_tenant_raw 一致跳过 → 放行。
#[tokio::test(flavor = "current_thread")]
async fn import_as_system_skips_guard() {
    let b = import_bridge(scoped_registry(), Default::default(), SqlGuard::Deny);
    let js = r#"(async () => {
             const { taskId } = await db.asSystem().import({ table: "orders", format: "jsonl", columns: ["id", "name"], rows: [{ id: 1, name: "a" }] });
             json.ok({ taskId });
           })().catch(e => json.fail(500, String(e)));"#;
    let cap = b.run_with(js, Default::default()).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert!(
        v["data"]["taskId"].as_str().unwrap().starts_with("imp_"),
        "{v}"
    );
}

/// 内联 rows 超 max_rows → 登记前拒（点名键）；拒后配额位不泄漏
/// （per_subject=1 下被拒后同主体立刻可再登记 = 无残留 entry）。
#[tokio::test(flavor = "current_thread")]
async fn import_rows_over_quota_rejected_before_register() {
    let section = super::super::db_policy::DbImportSection {
        max_rows: 2,
        per_subject_concurrency: 1,
        ..Default::default()
    };
    let b = import_bridge(SchemaRegistry::new(), section, SqlGuard::Off);
    let d = import_err(
            &b,
            r#"{ table: "t", format: "jsonl", columns: ["id"], rows: [{ id: 1 }, { id: 2 }, { id: 3 }] }"#,
        )
        .await;
    assert!(d["msg"].as_str().unwrap().contains("max_rows"), "{d}");
    // 登记无残留：同主体（per_subject=1）立刻可登记成功
    let js = r#"(async () => {
             const { taskId } = await db.import({ table: "t", format: "jsonl", columns: ["id"], rows: [{ id: 1 }] });
             json.ok({ taskId });
           })().catch(e => json.fail(500, String(e)));"#;
    let cap = b.run_with(js, Default::default()).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
}

/// 前缀隔离：importStatus(exp_id) / exportStatus(imp_id) 互 not found。
#[tokio::test(flavor = "current_thread")]
async fn import_prefix_isolation() {
    let b = import_bridge(SchemaRegistry::new(), Default::default(), SqlGuard::Off);
    let js = r#"(async () => {
             const expId = "exp_ab12_00000000000000000000000000000000";
             const impId = "imp_ab12_00000000000000000000000000000000";
             const r1 = await db.importStatus(expId).then(() => "found", (e) => String(e.message || e));
             const r2 = await db.exportStatus(impId).then(() => "found", (e) => String(e.message || e));
             json.ok({ r1, r2 });
           })().catch(e => json.fail(500, String(e)));"#;
    let cap = b.run_with(js, Default::default()).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert!(
        v["data"]["r1"].as_str().unwrap().contains("not found"),
        "{v}"
    );
    assert!(
        v["data"]["r2"].as_str().unwrap().contains("not found"),
        "{v}"
    );
}

/// blobKey 与 rows 恰一；format 只注册 csv/jsonl。
#[tokio::test(flavor = "current_thread")]
async fn import_blob_and_rows_mutex_and_bad_format() {
    let b = import_bridge(SchemaRegistry::new(), Default::default(), SqlGuard::Off);
    let d = import_err(
        &b,
        r#"{ table: "t", format: "csv", blobKey: "k", rows: [{ id: 1 }] }"#,
    )
    .await;
    assert!(d["msg"].as_str().unwrap().contains("blobKey"), "{d}");
    let d = import_err(&b, r#"{ table: "t", format: "csv" }"#).await;
    assert!(d["msg"].as_str().unwrap().contains("rows"), "{d}");
    let d = import_err(&b, r#"{ table: "t", format: "xlsx", rows: [{ id: 1 }] }"#).await;
    assert_eq!(d["code"], json!("OJ_DB_UNSUPPORTED"), "{d}");
    assert_eq!(d["format"], json!("xlsx"), "{d}");
}

/// 方言为 "jdbc" 的 fake accessor（即将到来的泛型 JDBC 插件形态）：
/// dialect_info 报未知名，dialect() 计数——未知方言下它是 panic 路径（FfiDataAccessor
/// 对 unknown 方言 panic），绝不允许被宿主代码触到。
struct JdbcLikeAccessor {
    inner: Arc<InMemoryAccessor>,
    info: crate::bridge::dialect_caps::DialectInfo,
    dialect_calls: std::sync::atomic::AtomicUsize,
}

impl JdbcLikeAccessor {
    fn new() -> Self {
        use crate::bridge::dialect_caps::{DialectCaps, IdStrategy, PlaceholderStyle};
        Self {
            inner: Arc::new(InMemoryAccessor::new()),
            info: crate::bridge::dialect_caps::DialectInfo {
                name: "jdbc".to_string(),
                caps: DialectCaps {
                    placeholder: PlaceholderStyle::Q,
                    id: IdStrategy::None,
                    stream: true,
                    cancel: false,
                },
            },
            dialect_calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl crate::bridge::db::DataAccessor for JdbcLikeAccessor {
    fn dialect(&self) -> crate::bridge::db::Dialect {
        self.dialect_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // 模拟 FfiDataAccessor::dialect() 对未知方言的 fail-loud panic。
        panic!("dialect() called on unknown dialect 'jdbc'");
    }
    fn dialect_info(&self) -> &crate::bridge::dialect_caps::DialectInfo {
        &self.info
    }
    async fn query_with_params(
        &self,
        sql: &str,
        params: &[serde_json::Value],
    ) -> crate::bridge::BridgeResult<Vec<crate::bridge::db::Row>> {
        self.inner.query_with_params(sql, params).await
    }
    async fn exec_with_params(
        &self,
        sql: &str,
        params: &[serde_json::Value],
    ) -> crate::bridge::BridgeResult<i64> {
        self.inner.exec_with_params(sql, params).await
    }
}

/// 未知方言（如 "jdbc"）调 db.import → 结构化 OJ_DB_UNSUPPORTED（capability=import），
/// 且 da.dialect() 零调用（防回归：无门控时它在 detached 任务里 panic 卡死任务）。
#[tokio::test(flavor = "current_thread")]
async fn import_unknown_dialect_gated_without_dialect_call() {
    let jdbc = Arc::new(JdbcLikeAccessor::new());
    let acc: Arc<dyn crate::bridge::db::DataAccessor> = jdbc.clone();
    let b = Bridge::with_dbs_and_loader(
        HashMap::from([("default".to_string(), acc)]),
        Arc::new(InMemoryKV::new()),
        scoped_registry(),
        false,
        None,
        Extras {
            import_section: Some(Arc::new(Default::default())),
            db_access: Some(Arc::new(Default::default())),
            sql_guard: SqlGuard::Off,
            ..Default::default()
        },
    );
    let d = import_err(
        &b,
        r#"{ table: "orders", format: "jsonl", columns: ["id", "tenant_id", "name"], rows: [{ id: 1, tenant_id: "t", name: "a" }] }"#,
    )
    .await;
    assert_eq!(d["code"], json!("OJ_DB_UNSUPPORTED"), "{d}");
    assert_eq!(d["capability"], json!("import"), "{d}");
    assert!(d["msg"].as_str().unwrap().contains("not supported"), "{d}");
    assert_eq!(
        jdbc.dialect_calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "dialect() must not be called"
    );
}
