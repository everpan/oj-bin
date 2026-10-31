use super::*;
use crate::bridge::{Bridge, Extras, InMemoryAccessor, InMemoryKV, SchemaRegistry};
use std::sync::Arc;

fn export_bridge(
    db: Arc<dyn super::super::db::DataAccessor>,
    blobs: Arc<super::super::blob::BlobRegistry>,
    export_section: super::super::db_policy::DbExportSection,
    db_access: super::super::db_policy::DbAccessSection,
) -> Bridge {
    Bridge::with_dbs_and_loader(
        std::collections::HashMap::from([("default".to_string(), db)]),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
        None,
        Extras {
            blobs: Some(blobs),
            export_section: Some(Arc::new(export_section)),
            db_access: Some(Arc::new(db_access)),
            ..Default::default()
        },
    )
}

fn tmp_blobs() -> (tempfile::TempDir, Arc<super::super::blob::BlobRegistry>) {
    let d = tempfile::tempdir().unwrap();
    let local = super::super::blob::LocalBlob::new(d.path(), "/v1/api").unwrap();
    (
        d,
        super::super::blob::registry_with_default(Arc::new(local)),
    )
}

/// 轮询直至 status.state 离开 running（deadline 有界——禁固定 sleep 当同步）。
async fn wait_terminal(b: &Bridge, task_id: &str, req: &super::super::http::RequestInfo) -> Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let js = format!(
            r#"(async () => {{
                     const st = await db.exportStatus({task_id:?});
                     json.ok(st);
                   }})().catch(e => json.fail(500, String(e)));"#,
        );
        let cap = b.run_with(&js, req.clone()).await.unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        if v["data"]["state"] != "running" {
            return v["data"].clone();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "export task hung in running"
        );
        tokio::task::yield_now().await;
    }
}

fn req_user(id: &str) -> super::super::http::RequestInfo {
    super::super::http::RequestInfo {
        user: Some(json!({ "id": id })),
        ..Default::default()
    }
}

/// REQ-07a：导出任务状态携带语句级降级清单（`degraded`）——CSV/JSONL 产物里
/// `$<type>:` 编码值的权威判定面（产物本身不改，单元格字符串原样落盘）。
#[tokio::test(flavor = "current_thread")]
async fn export_status_carries_degraded_columns() {
    let mem = Arc::new(InMemoryAccessor::new());
    mem.seed([
        json!({"ip": "$inet:AQI=", "name": "a"}),
        json!({"ip": "$inet:AgM=", "name": "b"}),
    ]);
    let (_d, blobs) = tmp_blobs();
    let b = export_bridge(mem, blobs.clone(), Default::default(), Default::default());
    let js = r#"(async () => {
            const { taskId } = await db.export({ sql: "select * from t", format: "csv" });
            json.ok({ taskId });
          })().catch(e => json.fail(500, String(e)));"#;
    let cap = b.run_with(js, Default::default()).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    let task_id = v["data"]["taskId"].as_str().unwrap().to_string();
    let st = wait_terminal(&b, &task_id, &Default::default()).await;
    assert_eq!(st["state"], json!("done"), "{st}");
    // 按 {col,type} 去重。
    assert_eq!(
        st["degraded"],
        json!([{"col": "ip", "type": "inet"}]),
        "{st}"
    );
}

/// REQ-07a 对照面：无降级单元格的导出显式给 `degraded: []`（schema 稳定，消费方
/// 不必判缺席）。
#[tokio::test(flavor = "current_thread")]
async fn export_status_degraded_empty_when_clean() {
    let mem = Arc::new(InMemoryAccessor::new());
    mem.seed([json!({"id": 1, "name": "a"})]);
    let (_d, blobs) = tmp_blobs();
    let b = export_bridge(mem, blobs.clone(), Default::default(), Default::default());
    let js = r#"(async () => {
            const { taskId } = await db.export({ sql: "select * from t", format: "jsonl" });
            json.ok({ taskId });
          })().catch(e => json.fail(500, String(e)));"#;
    let cap = b.run_with(js, Default::default()).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    let task_id = v["data"]["taskId"].as_str().unwrap().to_string();
    let st = wait_terminal(&b, &task_id, &Default::default()).await;
    assert_eq!(st["state"], json!("done"), "{st}");
    assert_eq!(st["degraded"], json!([]), "{st}");
}

#[tokio::test(flavor = "current_thread")]
async fn export_csv_returns_task_id_immediately_and_artifact_in_blob() {
    let mem = Arc::new(InMemoryAccessor::new());
    mem.seed([json!({"id": 1, "name": "a"}), json!({"id": 2, "name": "b"})]);
    let (_d, blobs) = tmp_blobs();
    let b = export_bridge(mem, blobs.clone(), Default::default(), Default::default());
    // E3 验收：立即回 taskId。
    let js = r#"(async () => {
            const t0 = Date.now();
            const { taskId } = await db.export({ sql: "select * from t", format: "csv" });
            json.ok({ taskId, ms: Date.now() - t0 });
          })().catch(e => json.fail(500, String(e)));"#;
    let cap = b
        .run_with(js, super::super::http::RequestInfo::default())
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert!(v["data"]["taskId"].is_string(), "{v}");
    let task_id = v["data"]["taskId"].as_str().unwrap().to_string();
    let st = wait_terminal(&b, &task_id, &super::super::http::RequestInfo::default()).await;
    assert_eq!(st["state"], json!("done"), "{st}");
    assert_eq!(st["rows"], json!(2));
    assert!(st["blobKey"].is_string());
    let key = st["blobKey"].as_str().unwrap().to_string();
    // 产物可下载且是 csv（BOM + 头 + 两行）。
    let art = blobs.default().unwrap().get(&key).await.unwrap();
    let text = String::from_utf8_lossy(&art);
    assert!(text.starts_with('\u{feff}'), "csv BOM missing: {text:?}");
    assert!(text.contains("id,name"), "{text}");
    assert!(
        text.contains("\r\na,1") || text.contains("1,a") || text.contains("a"),
        "{text}"
    );
    assert_eq!(
        blobs
            .default()
            .unwrap()
            .content_type(&key)
            .await
            .unwrap()
            .as_deref(),
        Some("text/csv")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn export_jsonl_lines_and_unknown_format_is_structured() {
    let mem = Arc::new(InMemoryAccessor::new());
    mem.seed([json!({"x": 1}), json!({"x": 2})]);
    let (_d, blobs) = tmp_blobs();
    let b = export_bridge(mem, blobs.clone(), Default::default(), Default::default());
    let js = r#"(async () => {
            const { taskId } = await db.export({ sql: "select 1", format: "jsonl" });
            const st = await db.exportStatus(taskId);
            json.ok(st);
          })().catch(e => json.fail(500, String(e)));"#;
    // 状态可能还在 running（自驱任务异步）——用轮询。
    let cap = b.run_with(js, Default::default()).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    let task_id = v["data"]["taskId"].as_str().unwrap().to_string();
    let st = wait_terminal(&b, &task_id, &Default::default()).await;
    assert_eq!(st["state"], json!("done"));
    let art = blobs
        .default()
        .unwrap()
        .get(st["blobKey"].as_str().unwrap())
        .await
        .unwrap();
    let lines: Vec<&str> = std::str::from_utf8(&art).unwrap().lines().collect();
    assert_eq!(lines, vec![r#"{"x":1}"#, r#"{"x":2}"#], "{lines:?}");
    // 未知 format → 结构化 OJ_DB_UNSUPPORTED，capability 点名 format。
    let js_bad = r#"(async () => {
            try {
              await db.export({ sql: "select 1", format: "xlsx" });
              json.ok({ err: "no-reject" });
            } catch (e) { json.ok({ code: e.code, cap: e.data && e.data.capability }); }
          })().catch(e => json.fail(500, String(e)));"#;
    let cap = b.run_with(js_bad, Default::default()).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["code"], json!("OJ_DB_UNSUPPORTED"), "{v}");
    assert_eq!(v["data"]["cap"], json!("format"), "{v}");
}

#[tokio::test(flavor = "current_thread")]
async fn export_cancel_is_idempotent_and_status_survives_across_requests() {
    // 慢流：每行 30ms，100 行 → 3s 跑不完；跨请求取消。
    struct Slow;
    #[async_trait::async_trait]
    impl super::super::db::DataAccessor for Slow {
        async fn query_with_params(
            &self,
            _sql: &str,
            _params: &[Value],
        ) -> super::super::BridgeResult<Vec<Value>> {
            Ok(vec![])
        }
        async fn exec_with_params(
            &self,
            _sql: &str,
            _params: &[Value],
        ) -> super::super::BridgeResult<i64> {
            Ok(0)
        }
        async fn stream_query(
            &self,
            _sql: &str,
            _params: &[Value],
        ) -> super::super::BridgeResult<
            std::pin::Pin<Box<dyn futures::Stream<Item = Result<Value, String>> + Send>>,
        > {
            Ok(Box::pin(futures::stream::unfold(0u64, |i| async move {
                if i >= 100 {
                    return None;
                }
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                Some((Ok(json!({"i": i})), i + 1))
            })))
        }
    }
    let (_d, blobs) = tmp_blobs();
    let b = export_bridge(
        Arc::new(Slow),
        blobs.clone(),
        Default::default(),
        Default::default(),
    );
    // 请求 A（user u1）发起——等价「发起后断连接」：run_with 结束后任务仍跑。
    let task_id = {
        let cap = b
            .run_with(
                r#"(async () => {
                        const { taskId } = await db.export({ sql: "select 1", format: "jsonl" });
                        json.ok({ taskId });
                      })().catch(e => json.fail(500, String(e)));"#,
                req_user("u1"),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        v["data"]["taskId"].as_str().unwrap().to_string()
    };
    // 请求 B（同主体）：进度可见且在涨（断线重连任务仍在——验收对照行）。
    let saw_running = {
        let cap = b
            .run_with(
                &format!(
                    r#"(async () => {{
                             const st = await db.exportStatus({task_id:?});
                             json.ok(st);
                           }})().catch(e => json.fail(500, String(e)));"#,
                ),
                req_user("u1"),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], json!(0), "{v}");
        v["data"]["state"] == "running"
    };
    assert!(saw_running, "慢导出必须仍在 running（跨请求存活）");
    // 请求 C：取消（幂等双调）。
    for _ in 0..2 {
        let cap = b
            .run_with(
                &format!(
                    r#"(async () => {{
                             const r = await db.exportCancel({task_id:?});
                             json.ok(r);
                           }})().catch(e => json.fail(500, String(e)));"#,
                ),
                req_user("u1"),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["data"]["cancelled"], json!(true), "{v}");
    }
    let st = wait_terminal(&b, &task_id, &req_user("u1")).await;
    assert_eq!(st["state"], json!("cancelled"), "{st}");
    // 他人视角：status/cancel 一律 not found（不泄露存在性）。
    let cap = b
        .run_with(
            &format!(
                r#"(async () => {{
                         try {{
                           await db.exportStatus({task_id:?});
                           json.ok({{ err: "no-reject" }});
                         }} catch (e) {{ json.ok({{ msg: String(e) }}); }}
                       }})().catch(e => json.fail(500, String(e)));"#,
            ),
            req_user("u2"),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert!(
        v["data"]["msg"]
            .as_str()
            .unwrap_or("")
            .contains("not found"),
        "{v}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn export_max_bytes_fails_and_max_rows_truncates() {
    let mem = Arc::new(InMemoryAccessor::new());
    mem.seed(
        (0..10)
            .map(|i| json!({"n": i, "pad": "xxxxxxxxxx"}))
            .collect::<Vec<_>>(),
    );
    let (_d, blobs) = tmp_blobs();
    // maxBytes 超限 → failed（错误 ≠ 策略）。
    let b = export_bridge(
        mem.clone(),
        blobs.clone(),
        Default::default(),
        Default::default(),
    );
    let js = r#"(async () => {
            const { taskId } = await db.export({ sql: "select 1", format: "jsonl", maxBytes: 16 });
            json.ok({ taskId });
          })().catch(e => json.fail(500, String(e)));"#;
    let cap = b.run_with(js, Default::default()).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    let task_id = v["data"]["taskId"].as_str().unwrap().to_string();
    let st = wait_terminal(&b, &task_id, &Default::default()).await;
    assert_eq!(st["state"], json!("failed"), "{st}");
    assert!(
        st["error"].as_str().unwrap_or("").contains("maxBytes"),
        "{st}"
    );
    // max_rows 策略截断 → done + truncated + 原因可见。
    let pol = super::super::db_policy::DbAccessSection {
        defaults: super::super::db_policy::DbAccess {
            max_rows: Some(3),
            ..Default::default()
        },
        ..Default::default()
    };
    let b2 = export_bridge(mem, blobs, Default::default(), pol);
    let js = r#"(async () => {
            const { taskId } = await db.export({ sql: "select 1", format: "jsonl" });
            json.ok({ taskId });
          })().catch(e => json.fail(500, String(e)));"#;
    let cap = b2.run_with(js, Default::default()).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    let task_id = v["data"]["taskId"].as_str().unwrap().to_string();
    let st = wait_terminal(&b2, &task_id, &Default::default()).await;
    assert_eq!(st["state"], json!("done"), "{st}");
    assert_eq!(st["truncated"], json!(true), "{st}");
    assert_eq!(st["truncatedReason"], json!("max_rows"), "{st}");
    assert_eq!(st["rows"], json!(3), "{st}");
}

#[tokio::test(flavor = "current_thread")]
async fn export_audit_events_cover_start_and_final_state() {
    let mem = Arc::new(InMemoryAccessor::new());
    mem.seed([json!({"a": 1})]);
    let (_d, blobs) = tmp_blobs();
    let b = export_bridge(mem, blobs, Default::default(), Default::default());
    let js = r#"(async () => {
            const { taskId } = await db.export({ sql: "select 1", format: "csv" });
            json.ok({ taskId });
          })().catch(e => json.fail(500, String(e)));"#;
    let cap = b.run_with(js, Default::default()).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    let task_id = v["data"]["taskId"].as_str().unwrap().to_string();
    let _ = wait_terminal(&b, &task_id, &Default::default()).await;
    // 审计：发起 + 终态两件；SQL 全文不入审计。
    let cap = b
        .run_with(
            r#"(async () => {
                    const ev = await audit.query({ kinds: ["db.export.start", "db.export.done"] });
                    json.ok(ev);
                  })().catch(e => json.fail(500, String(e)));"#,
            Default::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    let evs = v["data"].as_array().cloned().unwrap_or_default();
    assert!(
        evs.iter().any(|e| e["kind"] == "db.export.start"),
        "缺发起事件：{evs:?}"
    );
    assert!(
        evs.iter().any(|e| e["kind"] == "db.export.done"),
        "缺终态事件：{evs:?}"
    );
    assert!(
        evs.iter()
            .all(|e| !e["detail"].as_str().unwrap_or("").contains("select 1")),
        "SQL 全文不得入审计：{evs:?}"
    );
}

fn section(global: usize, per_subject: usize) -> super::super::db_policy::DbExportSection {
    super::super::db_policy::DbExportSection {
        global_concurrency: global,
        per_subject_concurrency: per_subject,
        ..Default::default()
    }
}

fn owner(user: &str) -> Owner {
    Owner {
        tenant: None,
        user: Some(user.into()),
        module: None,
        source: "test",
    }
}

#[test]
fn register_progress_finish_done_status_roundtrip() {
    let reg = ExportRegistry::new(&section(4, 2));
    let (id, notify) = reg.register(owner("u1"), "rep".into()).unwrap();
    assert!(handle_core::id_is_wellformed(PREFIX, &id));
    reg.progress(&id, 10, 4096);
    reg.finish(
        &id,
        Finish::Done {
            rows: 20,
            bytes: 8192,
            truncated: true,
            truncated_reason: Some("max_rows"),
            blob_key: "export/abc".into(),
            degraded: Vec::new(),
        },
    );
    let st = reg.status(&id, &owner("u1")).expect("own id");
    assert_eq!(st["state"], json!("done"));
    assert_eq!(st["rows"], json!(20));
    assert_eq!(st["bytes"], json!(8192));
    assert_eq!(st["truncated"], json!(true));
    assert_eq!(st["truncatedReason"], json!("max_rows"));
    assert_eq!(st["blobKey"], json!("export/abc"));
    assert_eq!(st["alias"], json!("rep"));
    assert!(st["finishedAtMs"].is_u64());
    // 终态幂等：二次 finish 不翻盘
    reg.finish(&id, Finish::Cancelled { rows: 99, bytes: 1 });
    assert_eq!(
        reg.status(&id, &owner("u1")).unwrap()["state"],
        json!("done")
    );
    drop(notify);
}

#[test]
fn status_hides_foreign_and_malformed_ids() {
    let reg = ExportRegistry::new(&section(4, 2));
    let (id, _n) = reg.register(owner("u1"), "rep".into()).unwrap();
    assert!(reg.status(&id, &owner("u2")).is_none(), "他人 id 不泄露");
    assert!(reg.status("export_nope", &owner("u1")).is_none());
    assert!(reg.status("../../../etc/passwd", &owner("u1")).is_none());
}

#[test]
fn quota_global_full_and_subject_full_have_distinct_messages() {
    let reg = ExportRegistry::new(&section(1, 1)); // 全局 1、每主体 1
    let _ = reg.register(owner("u1"), "rep".into()).unwrap();
    // 全局满（u2 也被全局挡）
    match reg.register(owner("u2"), "rep".into()) {
        Err(RegisterError::GlobalFull) => {}
        other => panic!("expect GlobalFull, got {other:?}"),
    }
    assert!(
        RegisterError::GlobalFull
            .message()
            .contains("global export concurrency")
    );
    // 每主体满（全局有坑位时）：换个全局上限验证
    let reg = ExportRegistry::new(&section(4, 1));
    let _ = reg.register(owner("u1"), "rep".into()).unwrap();
    match reg.register(owner("u1"), "rep".into()) {
        Err(RegisterError::SubjectFull) => {}
        other => panic!("expect SubjectFull, got {other:?}"),
    }
    assert!(
        RegisterError::SubjectFull
            .message()
            .contains("per-subject export concurrency")
    );
    // 不同主体不受他人占额影响
    assert!(reg.register(owner("u2"), "rep".into()).is_ok());
}

#[test]
fn cancel_is_idempotent_and_terminal_is_noop() {
    let reg = ExportRegistry::new(&section(4, 2));
    let (id, _n) = reg.register(owner("u1"), "rep".into()).unwrap();
    assert_eq!(reg.cancel(&id, &owner("u1")), Some(true));
    assert_eq!(reg.cancel(&id, &owner("u1")), Some(false), "幂等");
    assert_eq!(reg.cancel(&id, &owner("u2")), None, "他人 not found");
    // 终态后 cancel = no-op（已知且属主，不 not found）
    reg.finish(
        &id,
        Finish::Cancelled {
            rows: 5,
            bytes: 100,
        },
    );
    assert_eq!(reg.cancel(&id, &owner("u1")), Some(false));
    let st = reg.status(&id, &owner("u1")).unwrap();
    assert_eq!(st["state"], json!("cancelled"));
    assert_eq!(st["rows"], json!(5));
}

#[test]
fn cross_registry_ids_are_not_found() {
    // 模拟重启：新注册表（新 boot tag）不认旧 id
    let a = ExportRegistry::new(&section(4, 2));
    let (id, _n) = a.register(owner("u1"), "rep".into()).unwrap();
    let b = ExportRegistry::new(&section(4, 2));
    assert!(
        b.status(&id, &owner("u1")).is_none(),
        "跨重启一律 not found"
    );
    assert_eq!(b.cancel(&id, &owner("u1")), None);
}

#[test]
fn failed_finish_records_error_and_frees_quota() {
    let reg = ExportRegistry::new(&section(1, 1));
    let (id, _n) = reg.register(owner("u1"), "rep".into()).unwrap();
    reg.finish(
        &id,
        Finish::Failed {
            rows: 3,
            bytes: 12,
            error: "maxBytes exceeded".into(),
        },
    );
    let st = reg.status(&id, &owner("u1")).unwrap();
    assert_eq!(st["state"], json!("failed"));
    assert_eq!(st["error"], json!("maxBytes exceeded"));
    // 终态不占并发额：可立即再登记
    assert!(reg.register(owner("u1"), "rep".into()).is_ok());
}

#[test]
fn sweep_expires_old_keys_and_exempts_live() {
    let reg = ExportRegistry::new(&section(4, 2));
    let now = ms_now();
    // 台账三种 key：老 done 产物、新 done 产物、live 任务产物（登记簿挂着的）
    lock(&reg.ledger).insert("export/old".into(), now - 25 * 3_600_000);
    lock(&reg.ledger).insert("export/fresh".into(), now - 60_000);
    let (id, _n) = reg.register(owner("u1"), "rep".into()).unwrap();
    reg.finish(
        &id,
        Finish::Done {
            rows: 1,
            bytes: 2,
            truncated: false,
            truncated_reason: None,
            blob_key: "export/live".into(),
            degraded: Vec::new(),
        },
    );
    lock(&reg.ledger).insert("export/live".into(), now - 25 * 3_600_000);
    let expired = reg.sweep(now, 24 * 3_600_000);
    assert_eq!(expired, vec!["export/old".to_string()], "{expired:?}");
    // 已扫的不再重复扫
    assert!(reg.sweep(now, 24 * 3_600_000).is_empty());
}
