//! Task 3 单测（v0.1.122 D3）：复用路径影子装配的准入校验。
//!
//! - 抢注拦截：热装特权名模块必须 fail-fast（点名），已部署的平台模块不误伤；
//! - 空锁门：冷启动空锁 = 忘跑 `oj build`（Err）；复用路径空锁 = 合法卸载全部（Ok）。

use super::*;
use only_js::bridge::Bus;
use only_js::bridge::platform::PlatformGrant;
use std::collections::BTreeMap;

/// 最小 StableState（仅 platform 授权 + 旧代装配；其余字段全空）——
/// 抢注检查只读这两个面。
fn stable_min(platform: Option<Arc<PlatformGrant>>, old: PerGenAssembly) -> StableState {
    let audit_section = Arc::new(only_js::bridge::db_policy::AuditSection::default());
    let audit = Arc::new(only_js::bridge::audit::AuditStore::new(&audit_section));
    StableState {
        kv: Arc::new(InMemoryKV::new()),
        dbs: std::collections::HashMap::new(),
        assembly: Arc::new(std::sync::RwLock::new(Arc::new(old))),
        loader: None,
        module_index: None,
        blobs: Arc::new(BlobRegistry::new()),
        bus: Arc::new(Bus::new()),
        es: None,
        plugins: Vec::new(),
        db_override: None,
        query_limits: only_js::bridge::QueryLimits::default(),
        sql_trace: only_js::bridge::sql_trace::SqlTraceConfig::default(),
        seq_ensured: std::sync::Mutex::new(std::collections::HashSet::new()),
        kafkas: Arc::new(NamedRegistry::new()),
        rabbits: Arc::new(NamedRegistry::new()),
        tasks_flag: None,
        sql_memo: std::sync::Mutex::new(std::collections::HashMap::new()),
        js_heap_limit: None,
        boot: None,
        jwt: None,
        oidc: None,
        sso: None,
        mail: None,
        ldap: None,
        db_access: Arc::new(only_js::bridge::db_policy::DbAccessSection::default()),
        db_client: Arc::new(only_js::bridge::db_policy::DbClientSection::default()),
        db_session: Arc::new(only_js::bridge::db_policy::DbSessionSection::default()),
        client_dbs: Arc::new(only_js::bridge::ClientDbRegistry::with_caps(16, 4, 4)),
        tx_sessions: Arc::new(only_js::bridge::db_session::TxSessionRegistry::new(
            only_js::bridge::db_policy::DbSessionSection::default(),
            audit.clone(),
        )),
        audit_section,
        audit,
        export_section: Arc::new(only_js::bridge::db_policy::DbExportSection::default()),
        export_registry: Arc::new(only_js::bridge::db_export::ExportRegistry::new(
            &only_js::bridge::db_policy::DbExportSection::default(),
        )),
        import_section: Arc::new(only_js::bridge::db_policy::DbImportSection::default()),
        import_registry: Arc::new(only_js::bridge::db_import::ImportRegistry::new(
            &only_js::bridge::db_policy::DbImportSection::default(),
        )),
        exec_sessions: Arc::new(only_js::bridge::db_exec::ExecRegistry::default()),
        mgmt_section: Arc::new(only_js::bridge::db_policy::MgmtSection::default()),
        mgmt_axes: only_js::bridge::generic_axis::registry_from(vec![]),
        mgmt: Arc::new(only_js::bridge::mgmt::MgmtRegistry::new(16, 4)),
        secret_keys: None,
        db_access_rt: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        db_backend: Arc::new(only_js::bridge::DbBackendRegistry::default()),
        config_dir: Arc::new(std::path::PathBuf::from("/tmp")),
        vars: Arc::new(std::collections::HashMap::new()),
        input_contracts: None,
        fs: None,
        platform,
        exec: None,
        reload: Arc::new(std::sync::OnceLock::new()),
        dispatch: Arc::new(only_js::bridge::dispatch_bridge::DispatchSlot::new()),
        generic_axes: only_js::bridge::generic_axis::registry_from(vec![]),
        oj_info: Arc::new(serde_json::json!({})),
    }
}

/// 构造装配快照：modules 按目录键 `/x/<name>`，registry 空。
fn asm_of(names: &[&str]) -> PerGenAssembly {
    let mut modules = std::collections::HashMap::new();
    for n in names {
        modules.insert(
            format!("/x/{n}"),
            ModuleCtx {
                name: (*n).to_string(),
                version: "1.0.0".into(),
                deps: Arc::new(std::collections::HashSet::new()),
                db: None,
            },
        );
    }
    PerGenAssembly {
        registry: Arc::new(SchemaRegistry::new()),
        modules: Arc::new(modules),
        sql_guard: SqlGuard::Off,
        ownership_deny: false,
        allow_as_tenant: false,
    }
}

/// Given: 平台特权名单含 admin、旧代无 admin；When: 新代出现 admin；
/// Then: Err 且点名 admin（热装特权名 = 继承特权，装配期阻断）。
#[test]
fn squatting_check_rejects_privileged_name() {
    let pg = Arc::new(PlatformGrant::new(vec!["admin".to_string()], &[], None).unwrap());
    let stable = stable_min(Some(pg), asm_of(&["shop"]));
    let new = asm_of(&["shop", "admin"]);
    let r = check_platform_squatting(&stable, &new);
    assert!(r.is_err(), "hot-installing a privileged name must fail");
    assert!(r.unwrap_err().contains("admin"));

    // 平台未配置（无特权名单）→ 不拦。
    let stable = stable_min(None, asm_of(&["shop"]));
    assert!(check_platform_squatting(&stable, &new).is_ok());
}

/// Given: admin 已部署（旧代含）；When: 新代仍含 admin（正常 reload）；
/// Then: Ok —— 抢注检查只拦「新增」的特权名，不误伤受控根产物。
#[test]
fn squatting_check_allows_predeployed_platform_module() {
    let pg = Arc::new(PlatformGrant::new(vec!["admin".to_string()], &[], None).unwrap());
    let stable = stable_min(Some(pg), asm_of(&["admin", "shop"]));
    let new = asm_of(&["admin", "shop", "cart"]);
    assert!(check_platform_squatting(&stable, &new).is_ok());
}

/// Given: 空 manifests.yaml；When: 冷启动 → Err（忘跑 oj build）；复用路径 → Ok
/// （合法「卸载全部模块」，D7 决策）。
#[test]
fn empty_lock_allowed_on_reuse_rejected_on_cold() {
    let lock: BTreeMap<String, String> = BTreeMap::new();
    assert!(release_route_lock_check(&lock, false).is_err());
    assert!(release_route_lock_check(&lock, true).is_ok());
}
