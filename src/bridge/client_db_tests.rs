use super::*;
use crate::bridge::db::InMemoryAccessor;
use serde_json::json;

#[test]
fn registry_register_and_overwrite() {
    let r = ClientDbRegistry::new(2);
    let mk = || ClientDbEntry {
        accessor: Arc::new(crate::bridge::db::InMemoryAccessor::new()),
        dsn_redacted: "sqlite::memory:".into(),
        dialect: "sqlite".into(),
        registered_by: None,
        registered_at_ms: 0,
        server_version: None,
        probed_at_ms: 0,
        queries: AtomicU64::new(0),
        last_error: Mutex::new(None),
    };
    r.register("a", mk()).unwrap();
    assert!(r.contains("a"));
    // 重名 → 拒
    assert!(r.register("a", mk()).is_err());
    // 超配额 → 拒
    r.register("b", mk()).unwrap();
    assert!(r.register("c", mk()).is_err(), "cap must block 3rd");
    // 删除释放名额
    assert!(r.remove("a"));
    r.register("c", mk()).unwrap();
    assert_eq!(r.names(), vec!["b".to_string(), "c".to_string()]);
    // accessor_and_count 累加
    let acc = r.accessor_and_count("b").unwrap();
    assert!(acc.dialect_info().name == "sqlite");
    assert_eq!(r.get("b").unwrap().queries.load(Ordering::Relaxed), 1);
}

#[test]
fn alias_validation() {
    assert!(valid_client_alias("sales_replica"));
    assert!(!valid_client_alias(""));
    assert!(!valid_client_alias("default"));
    assert!(!valid_client_alias(&format!("bad{u}name", u = '\u{0007}'))); // 控制字符
    assert!(!valid_client_alias(&format!("bad{u}name", u = '\u{0000}'))); // NUL
    assert!(!valid_client_alias(&"x".repeat(129)));
}

/// U-7：per-owner 登记上限——同 owner 第 N+1 个被拒且错误可辨识；异 owner 不受影响；
/// delete 释放名额。
#[test]
fn per_owner_cap_blocks_n_plus_one() {
    let r = ClientDbRegistry::with_caps(8, 1, 8);
    let mk = |by: Option<&str>| ClientDbEntry {
        accessor: Arc::new(crate::bridge::db::InMemoryAccessor::new()),
        dsn_redacted: "sqlite::memory:".into(),
        dialect: "sqlite".into(),
        registered_by: by.map(|s| s.to_string()),
        registered_at_ms: 0,
        server_version: None,
        probed_at_ms: 0,
        queries: AtomicU64::new(0),
        last_error: Mutex::new(None),
    };
    r.register("a", mk(Some("u"))).unwrap();
    let err = r.register("b", mk(Some("u"))).unwrap_err();
    assert!(err.contains("per-owner cap 1"), "{err}");
    // 异 owner / 匿名桶不受影响
    r.register("c", mk(Some("v"))).unwrap();
    r.register("d", mk(None)).unwrap();
    // delete 释放
    assert!(r.remove("a"));
    r.register("e", mk(Some("u"))).unwrap();
}

/// U-7：拨号锁退——连续失败达阈值即拒拨（异 owner 不受影响）；成功记账即清零。
#[test]
fn scan_lockout_after_consecutive_dial_failures() {
    let r = ClientDbRegistry::with_caps(8, 8, 8);
    assert!(r.dial_allowed("u").is_ok());
    for i in 0..SCAN_FAIL_THRESHOLD - 1 {
        assert!(!r.note_dial("u", false), "第 {} 次失败还不该锁", i + 1);
    }
    assert!(r.note_dial("u", false), "第 8 次失败应触发锁退");
    let left = r.dial_allowed("u").unwrap_err();
    assert!(left <= SCAN_LOCKOUT && left > std::time::Duration::ZERO);
    // 异 owner 照常
    assert!(r.dial_allowed("v").is_ok());
    // 成功记账清零（锁退态被显式成功解除——记账单一真源）
    r.note_dial("u", true);
    assert!(r.dial_allowed("u").is_ok());
}

/// U-7：全局流预算——占满后 try_acquire 拒绝且错误点名配置键。
#[test]
fn stream_budget_exhausts_distinctly() {
    let r = ClientDbRegistry::with_caps(8, 8, 2);
    let a = r.try_acquire_stream().unwrap();
    let b = r.try_acquire_stream().unwrap();
    let err = r.try_acquire_stream().unwrap_err();
    assert!(err.contains("db_client.max_streams"), "{err}");
    drop(a);
    let _ = r.try_acquire_stream().unwrap(); // 释放后可再占
    drop(b);
}

// ---- U-11 连接级能力探测 ----

/// 挂起 accessor（超时路径夹具；查询永不返回）。
struct HangingAccessor;

#[async_trait::async_trait]
impl DataAccessor for HangingAccessor {
    async fn query_with_params(
        &self,
        _sql: &str,
        _params: &[Value],
    ) -> crate::bridge::BridgeResult<Vec<Value>> {
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        Ok(vec![])
    }
    async fn exec_with_params(
        &self,
        _sql: &str,
        _params: &[Value],
    ) -> crate::bridge::BridgeResult<i64> {
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        Ok(0)
    }
}

/// 探测三态：成功取串 / 失败与超时降级 None / 开放方言跳过。
#[tokio::test(flavor = "current_thread")]
async fn probe_version_degrades_and_times_out() {
    let ok = Arc::new(InMemoryAccessor::new());
    ok.seed(vec![serde_json::json!("3.45.1")]);
    let ok: Arc<dyn DataAccessor> = ok;
    assert_eq!(
        probe_version(&ok, "sqlite").await.unwrap().0,
        "3.45.1",
        "成功路径取首行首列"
    );
    // 开放方言：跳过（不发 SQL 也拿不到）
    assert!(probe_version(&ok, "oracle").await.is_none());
    // 失败 → None（InMemoryAccessor::set_error）
    let failing = Arc::new(InMemoryAccessor::new());
    failing.set_error("boom");
    let failing: Arc<dyn DataAccessor> = failing;
    assert!(probe_version(&failing, "sqlite").await.is_none());
    // 超时 → None（短超时 + 挂起 accessor）
    let hang: Arc<dyn DataAccessor> = Arc::new(HangingAccessor);
    assert!(
        probe_version_with(&hang, "sqlite", std::time::Duration::from_millis(50))
            .await
            .is_none()
    );
}

/// Bridge 级：register 真库（temp sqlite）后 list 透出版本；test 同样返回。
#[tokio::test(flavor = "current_thread")]
async fn register_probes_server_version() {
    use crate::bridge::db_backend::DbBackendRegistry;
    use crate::bridge::{Bridge, Extras, InMemoryKV, SchemaRegistry};
    use std::path::PathBuf;
    let p = std::env::temp_dir().join(format!("oj-probe-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&p);
    std::fs::File::create(&p).unwrap();
    let b = Bridge::with_dbs_and_loader(
        std::collections::HashMap::from([(
            "default".to_string(),
            Arc::new(InMemoryAccessor::new()) as _,
        )]),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
        None,
        Extras {
            db_client: Some(Arc::new(DbClientSection {
                enabled: true,
                max_entries: 8,
                max_per_owner: 8,
                max_streams: 8,
                allow: Default::default(),
            })),
            db_backend: Some(Arc::new(DbBackendRegistry::builtin())),
            config_dir: Some(Arc::new(PathBuf::from("."))),
            ..Default::default()
        },
    );
    let src = format!(
        r#"(async () => {{
              await db.client.register("rep", "sqlite://{}");
              const e = (await db.client.list())[0];
              const t = await db.client.test("sqlite://{}");
              return json.ok({{ v: e.serverVersion, probed: e.probedAtMs > 0,
                                tv: t.serverVersion }});
            }})().catch(e => json.fail(500, String(e)))"#,
        p.display(),
        p.display()
    );
    let cap = b.run(&src).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    let s = v["data"]["v"].as_str().unwrap_or_default();
    assert!(s.starts_with('3'), "sqlite 版本串形如 3.x.y：{v}");
    assert_eq!(v["data"]["probed"], json!(true), "{v}");
    assert_eq!(v["data"]["tv"], v["data"]["v"], "test 与 list 同源：{v}");
}

/// U-18 批 T2.5（REQ-06）：`db.client.*` / `db.access.*` 异步方法返回**真 Promise**
/// （`instanceof Promise`、`.catch` 可链）——类型面声明 `Promise<...>`，运行期曾回裸值。
#[tokio::test(flavor = "current_thread")]
async fn client_and_access_surfaces_return_real_promises() {
    use crate::bridge::{Bridge, Extras, InMemoryKV, SchemaRegistry};
    let b = Bridge::with_dbs_and_loader(
        std::collections::HashMap::from([(
            "default".to_string(),
            Arc::new(InMemoryAccessor::new()) as _,
        )]),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
        None,
        Extras::default(),
    );
    // client 未启用：方法应返回「被拒绝的真 Promise」，而非同步抛或裸值。
    let cap = b
        .run(
            r#"(async () => {
                     const p = db.client.list();
                     const g = db.access.grant("x", { roles: ["r"] });
                     const caught = await p.catch(() => "rejected");
                     const gCaught = await g.catch(() => "rejected");
                     const entries = await db.client.list().catch(() => []);
                     return json.ok({ isP: p instanceof Promise,
                                      hasCatch: typeof p.catch === "function",
                                      caught, gIsP: g instanceof Promise, gCaught,
                                      n: entries.length });
                   })()"#,
        )
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["isP"], true, "{v}");
    assert_eq!(v["data"]["hasCatch"], true, "{v}");
    assert_eq!(v["data"]["caught"], "rejected", "{v}");
    assert_eq!(v["data"]["gIsP"], true, "{v}");
    assert_eq!(v["data"]["gCaught"], "rejected", "{v}");
    assert_eq!(v["data"]["n"], 0, "{v}");
}

// 无关 `AllowList` 默认形态（确保引用不被误删）。
#[test]
fn allow_list_default_unrestricted() {
    assert!(db_policy::AllowList::default().is_unrestricted());
}
