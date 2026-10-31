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
