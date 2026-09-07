//! 进程内应用装配：`App` 抽取自原 `server_cmd::start` 的装配段。
//!
//! 设计要点（评审修正清单）：
//! - 单一 `StableState` 在 `from_config` 内构造一次，被 `App` 的 actor 工厂与测试运行时
//!   共享（同一组 `bus`/`dbs`/`kv`/`loader`/`es` Arc），保证 in-memory 后端跨家族互通（修正 #2）。
//! - `dispatch` 以 `axum::Router::oneshot` 在进程内跑完整路由 + 真实运行时 + 真实后端，
//!   零 TCP（对标 Go Fiber `app.Test`）。WS upgrade 经 oneshot 返回 101，由 `op_client_dispatch`
//!   占位处理，不跑帧循环（修正 #3）。
//! - 外层 `timeout` 包裹防止 handler 死循环挂死测试 task（修正 #10）。

#![allow(clippy::needless_borrow)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use oj_plugin_ffi;
use only_js::bridge::blob::{BlobBackend, BlobRegistry};
use only_js::bridge::mq::MqInstance;
use only_js::bridge::plugin_loader::kv_backend_connect;
use only_js::bridge::{
    Bridge, DataAccessor, EsBackend, Extras, InMemoryKV, JwtCfg, KVStore, LoaderShared, ModuleCtx,
    NamedRegistry, SchemaRegistry,
};
use only_js::bridge::{EventBroker, StableState};
use only_js::config::{self, Config};
use server::CertificateStatus;
use server::actor::JsActor;
use server::certificate::load_certificate_at;
use server::certificate_watcher::{SharedCertStatus, SharedCertValidUntil, spawn_watcher};
use server::routes;
use server::ws;
use tokio::task::JoinHandle;
use tower::util::ServiceExt;

use crate::manifest;
use crate::server_cmd::{Registries, assemble_blobs, assemble_plugins, connect_dbs};

/// 进程内 HTTP 派发契约：测试运行时把 `Arc<App>` 注入 OpState，JS `client` 全局经
/// `op_client_dispatch` 调它。trait 必须 `Send+Sync+'static` 且方法用 `async_trait`
/// （不能用 `-> impl Future`，否则非对象安全，`Arc<dyn>` 编不过——修正 #6）。
#[async_trait]
pub trait ClientTransport: Send + Sync + 'static {
    /// 派发一个已构造好的请求，返回完整响应（含 101 upgrade）。
    async fn dispatch(&self, req: Request<Body>) -> axum::http::Response<Body>;
    /// API 基础前缀（如 `/v1/api`）；op 拼进 path，测试写 `client.get("/x")` 即可（修正 #7）。
    fn base(&self) -> &str;
}

/// 共享运行时：装配产物 + 进程内 dispatch 句柄。
pub struct App {
    router: Router,
    #[allow(dead_code)]
    bus: Arc<dyn EventBroker>,
    /// 与 actor 工厂共享同一组后端的 StableState，供测试运行时（bridge_ext）复用（修正 #2）。
    stable: Arc<StableState>,
    base: String,
}

/// dispatch 外层超时（handler 死循环 KillSwitch 兜底 server.timeout，这里再兜底测试 task）。
const DISPATCH_TIMEOUT: Duration = Duration::from_secs(60);

/// 探测 `<dir>/ext_boot.js`：不存在 → None（静默）；存在但 stat/canonicalize 失败 → Err（fail-fast）。
/// 命中时冻结 `?v=<mtime>` 并打印绝对路径 —— 改动后须重启进程，这行日志是唯一的核对依据。
pub fn ext_boot_spec(dir: &Path) -> Result<Option<String>, String> {
    let p = dir.join("ext_boot.js");
    if !p.is_file() {
        return Ok(None);
    }
    let spec = only_js::bridge::versioned_specifier(&p).map_err(|e| format!("ext_boot.js: {e}"))?;
    eprintln!("ext_boot: loaded {} ({spec})", p.display());
    Ok(Some(spec.to_string()))
}

/// 启动期预热 boot：建一个 runtime 跑完 ext_boot，失败即 `Err`。
/// 与 `routes::bridge_introspector` 同构（独立线程 + current_thread runtime）——`Bridge`
/// 是 `!Send`，而 `App::from_config` 跑在 multi_thread 主 runtime 上（future 须 Send），
/// 不可在其中直接 await bridge。
fn prewarm_boot(make_bridge: impl Fn() -> Bridge + Send + Sync + 'static) -> Result<(), String> {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("prewarm runtime");
        let b = make_bridge();
        rt.block_on(async { b.prewarm().await })
            .map_err(|e| format!("ext_boot: {e}"))
    })
    .join()
    .unwrap_or_else(|_| Err("ext_boot: prewarm thread panicked".to_string()))
    .map_err(|e| format!("{e} (edit ext_boot.js, then restart the process)"))
}

/// KV（装配第 6 步）：声明了 `redis.default` → 经 kv 插件 vtable connect（单例 fail-fast）；
/// 未声明 → 内置 `InMemoryKV` 兜底。
async fn connect_kv(cfg: &Config, registries: &Registries) -> Result<Arc<dyn KVStore>, String> {
    match cfg.redis.get("default") {
        Some(url) => match registries.kv {
            Some(vt) => kv_backend_connect(vt, url)
                .await
                .map_err(|e| format!("redis 'default': {e}")),
            None => Err("config declares redis.default but no kv plugin loaded \
                 (run `cargo xtask plugin kv-redis`)"
                .to_string()),
        },
        None => Ok(Arc::new(InMemoryKV::new()) as Arc<dyn KVStore>),
    }
}

/// mq 插件名 → 服务的 kind（strip "bus-"；历史插件名 rabbitmq 归一为 rabbit，评审 F7）。
fn mq_kind_of(desc_name: &str) -> Option<&'static str> {
    match desc_name.strip_prefix("bus-")? {
        "kafka" => Some("kafka"),
        "rabbitmq" | "rabbit" => Some("rabbit"),
        _ => None,
    }
}

/// mq 命名实例装配（spec §3/§7）：kafkas:/rabbits: 段逐实例 connect（kind 注入 cfg）→
/// MqInstance。段缺省 = 空 registry（JS 侧 undefined）。失败 fail-fast：
/// 段声明但无对应 kind 插件 / 插件拒绝 cfg（kind 不符、参数缺失）/ 注册重名。
async fn build_mq_registries(
    cfg: &Config,
    mq: &[(String, &'static oj_plugin_ffi::MqVtable)],
) -> Result<
    (
        Arc<NamedRegistry<MqInstance>>,
        Arc<NamedRegistry<MqInstance>>,
    ),
    String,
> {
    const BACKOFF: Duration = Duration::from_millis(10);
    let mut kinds: Vec<&'static str> = Vec::new();
    for (name, _) in mq {
        if let Some(k) = mq_kind_of(name) {
            if kinds.contains(&k) {
                return Err(format!(
                    "plugins conflict: multiple mq plugins serve kind '{k}' ({name})"
                ));
            }
            kinds.push(k);
        }
    }
    let mut kafkas = NamedRegistry::new();
    let mut rabbits = NamedRegistry::new();
    for (section, key, kind, reg) in [
        (&cfg.kafkas, "kafkas", "kafka", &mut kafkas),
        (&cfg.rabbits, "rabbits", "rabbit", &mut rabbits),
    ] {
        if section.is_empty() {
            continue;
        }
        let vt = mq
            .iter()
            .find_map(|(n, vt)| (mq_kind_of(n) == Some(kind)).then_some(*vt))
            .ok_or_else(|| format!("config declares {key} but no mq plugin for kind '{kind}'"))?;
        for (name, v) in section {
            let mut obj = v.clone();
            if let Some(o) = obj.as_object_mut() {
                o.insert("kind".into(), serde_json::Value::String(kind.into()));
            }
            let inst = MqInstance::ffi_connect(kind, vt, obj.to_string(), BACKOFF)
                .await
                .map_err(|e| format!("{key}.{name}: {e}"))?;
            reg.register(name, Arc::new(inst))
                .map_err(|e| format!("mq register: {e}"))?;
        }
    }
    Ok((Arc::new(kafkas), Arc::new(rabbits)))
}

/// 表归属守卫模式（§5.3，装配第 10 步）：`warn`（默认，违规仅告警）| `deny`（违规拒绝）；
/// 非法值 fail-fast。
fn ownership_deny_of(cfg: &Config) -> Result<bool, String> {
    match cfg.server.ownership_guard.as_deref() {
        None | Some("warn") => Ok(false),
        Some("deny") => Ok(true),
        Some(other) => Err(format!(
            "server.ownership_guard: illegal value {other:?} (warn|deny)"
        )),
    }
}

/// 归属图 + SchemaRegistry 复活（§4.8，装配第 11 步）：discover 全模块 → schema.yaml +
/// manifest(db/deps) → registry（S002 同表双声明 fail-fast；table_owned 记 owner）+ ModuleCtx
/// map（键 = 模块目录绝对路径，run_module 祖先命中注入）。`gate == "auto"` 时逐模块
/// reconcile（§D1：安全前向只进 apply 路径，迁移后补声明漂移）。
async fn build_schema_and_modules(
    dir: &Path,
    ts: bool,
    dbs: &std::collections::HashMap<String, Arc<dyn DataAccessor>>,
    gate: &str,
) -> Result<
    (
        SchemaRegistry,
        Arc<std::collections::HashMap<String, ModuleCtx>>,
    ),
    String,
> {
    let mut registry = SchemaRegistry::new();
    let mut module_map: std::collections::HashMap<String, ModuleCtx> =
        std::collections::HashMap::new();
    for (name, mdir) in manifest::discover(dir, ts)? {
        let mf = manifest::parse_one(&mdir.join("manifest.yaml"))?;
        if let Some(f) = crate::schema::SchemaFile::load(&mdir)? {
            for (t, pk, cols) in f.registry_tables() {
                if registry.has_table(t) {
                    return Err(format!(
                        "S002: 表 {t:?} 被多个模块声明（{} 与 {name}）",
                        registry.owner_of(t).unwrap_or("?")
                    ));
                }
                registry = registry.table_owned(&name, t, &pk, &cols);
            }
            if gate == "auto" {
                let acc = dbs
                    .get("default")
                    .ok_or("schema.yaml requires db 'default'")?;
                for l in crate::schema::reconcile(acc.as_ref(), &name, &f).await? {
                    eprintln!("schema: {l}");
                }
            }
        }
        module_map.insert(
            mdir.to_string_lossy().into_owned(),
            ModuleCtx {
                name: name.clone(),
                deps: Arc::new(mf.deps.keys().cloned().collect()),
                db: mf.db.clone(),
            },
        );
    }
    Ok((registry, Arc::new(module_map)))
}

/// jwt / oidc 原语配置（auth 与 OIDC 解耦后注入 bridge Extras 的两项）。
type JwtOidcCfg = (
    Option<Arc<JwtCfg>>,
    Option<Arc<only_js::bridge::oidc::OidcState>>,
);

/// jwt / oidc 原语配置（装配第 14 步）。auth 与 OIDC 解耦：核心只留原语，端点在 JS；
/// 装配期构造失败即 fail-fast。
fn build_jwt_and_oidc(cfg: &Config, config_dir: &Path) -> Result<JwtOidcCfg, String> {
    let jwt = cfg
        .auth
        .as_ref()
        .map(only_js::bridge::JwtCfg::from_auth_cfg)
        .transpose()
        .map_err(|e| format!("auth: {e}"))?
        .map(Arc::new);
    let oidc = match &cfg.oidc {
        Some(s) => Some(Arc::new(
            only_js::bridge::oidc::OidcState::from_section(s, config_dir)
                .map_err(|e| format!("oidc: {e}"))?,
        )),
        None => None,
    };
    Ok((jwt, oidc))
}

/// 静态站点根（装配第 20 步）：`server.app_path`（CLI `--app-path` 可覆盖）相对 config_dir
/// 绝对化；目录缺失 → fail-fast。
fn resolve_static_root(cfg: &Config, config_dir: &Path) -> Result<Option<PathBuf>, String> {
    let Some(r) = &cfg.server.app_path else {
        return Ok(None);
    };
    let p = Path::new(r);
    let p = if p.is_absolute() {
        p.to_path_buf()
    } else {
        config_dir.join(p)
    };
    let p = p
        .canonicalize()
        .map_err(|e| format!("server.app_path {}: {e}", p.display()))?;
    Ok(Some(p))
}

/// 证书加载 + 校验 + 热加载 watcher（装配第 21 步）。启动期 `Expired`（宽限已过）→ 拒启；
/// `Grace` 只告警；运行中过期由 watcher 切状态（handle 内仅限制 GET），服务不中断。
fn load_cert_with_watcher(
    cfg: &Config,
    config_dir: &Path,
) -> Result<(SharedCertStatus, SharedCertValidUntil), String> {
    let (status, valid_until) = load_certificate_at(&cfg.server, config_dir)?;
    match &status {
        CertificateStatus::Expired => {
            tracing::error!(
                "certificate has expired and grace period elapsed — service will not start"
            );
            return Err("certificate expired".into());
        }
        CertificateStatus::Grace { remaining_secs } => {
            tracing::warn!(
                "certificate expired, {} days grace period remaining — service starting",
                remaining_secs / 86_400
            );
        }
        CertificateStatus::Valid => {
            tracing::info!("certificate loaded: valid");
        }
    }
    let cert_status: SharedCertStatus = Arc::new(RwLock::new(status));
    let cert_valid_until: SharedCertValidUntil = Arc::new(RwLock::new(valid_until));
    // 热加载：证书/公钥文件被覆盖即原子更新状态（事件驱动，不轮询）。
    spawn_watcher(
        cert_status.clone(),
        cert_valid_until.clone(),
        cfg.server.clone(),
        config_dir.to_path_buf(),
    );
    Ok((cert_status, cert_valid_until))
}

impl App {
    /// 装配并构造（原 `start` 逻辑搬入）。唯一构造一处 `StableState`，同时被 actor 工厂与
    /// 测试运行时引用，保证 db/bus/kv 跨家族为同一组 Arc（修正 #2）。
    ///
    /// 23 步的顺序即语义（见 [docs/modules/04-oj-cli.md]）；其中自成一体的步骤已抽为
    /// 私有函数（connect_kv / ownership_deny_of / build_schema_and_modules /
    /// build_jwt_and_oidc / resolve_static_root / load_cert_with_watcher）。
    #[allow(clippy::too_many_lines)]
    pub async fn from_config(
        cfg: Config,
        config_dir: &Path,
        dir: PathBuf,
        base: String,
        ts: bool,
        fixtures: bool,
    ) -> Result<App, String> {
        // 其余 redis key warn 忽略（仅 redis.default 参与装配）。
        for (name, url) in cfg.redis.iter().filter(|(n, _)| n.as_str() != "default") {
            eprintln!("warn: redis '{name}' ({url}) ignored (only redis.default is used)");
        }
        // 证书必配门禁（无逃生口）：两个证书路径必须都配齐才启动，否则 fail-fast。
        // 证书校验不可被 config 或 CLI 关闭——任何绕过都会违背证书强制校验的初衷。
        if !cfg.server.cert_paths_configured() {
            return Err("certificate is mandatory but not configured \
                 (set server.public_key_path + server.certificate_path; \
                 no config or flag can skip certificate validation)"
                .to_string());
        }
        // 绝对化 dir（Bridge loader 的 project_root 用 config_dir，api 相对 dir）。
        let dir = dir.canonicalize().unwrap_or(dir);
        let loader = Arc::new(LoaderShared {
            project_root: config_dir
                .canonicalize()
                .unwrap_or_else(|_| config_dir.to_path_buf()),
            ts,
        });
        // ext_boot：运行时创建期加载一次（bootstrap 的动态补充）。
        // specifier 在此冻结 `?v=<mtime>`：改文件须重启进程（池常驻，不做热重载）。
        let boot = ext_boot_spec(config_dir)?;
        // 插件装配（spec §5）：解析 plugins_dir → 清单严格/缺省扫描 → 校验 → 注册。
        // 自描述清单（PluginInfo）同时喂 Extras/StableState.plugins（JS 内省）与
        // `GET {base}/plugins`（AppState）。
        let mut registries = Registries::default();
        let plugin_infos: std::sync::Arc<Vec<only_js::bridge::PluginInfo>> = std::sync::Arc::new(
            assemble_plugins(&cfg, config_dir, &mut registries)
                .await
                .map_err(|e| format!("plugins: {e}"))?,
        );
        // KV：redis.default 存在 → 经 kv 插件 vtable connect（单例 fail-fast）；
        // 未声明 → InMemoryKV 内置兜底。
        let kv: Arc<dyn KVStore> = connect_kv(&cfg, &registries).await?;
        let es: Option<Arc<dyn EsBackend>> = registries.es;
        // blob：blob 段存在即启用；未声明 → None。
        let blobs: Option<Arc<BlobRegistry>> = match &cfg.blob {
            None => None,
            Some(section) => {
                Some(assemble_blobs(section, config_dir, &base, registries.blob).await?)
            }
        };
        // 下载路由仅服务 default 后端。
        let blob: Option<Arc<dyn BlobBackend>> = blobs.as_ref().and_then(|r| r.default());
        // 逐 db 开库（未知 scheme 注册表 fail-fast）。
        let dbs = connect_dbs(&cfg.db, &registries.dbs, config_dir).await?;
        // 迁移门禁（§4.6，先于 seed）：dev 默认 auto（apply），release 默认 verify
        // （M003/M004 校验，账本落后拒启）；`migrate_on_start: off` 为逃生门。
        let gate =
            cfg.server
                .migrate_on_start
                .as_deref()
                .unwrap_or(if ts { "auto" } else { "verify" });
        match gate {
            "auto" => crate::migrate::apply_all(dbs.get("default"), &dir, ts, false).await?,
            "verify" => crate::migrate::verify_all(dbs.get("default"), &dir, ts).await?,
            "off" => {}
            other => {
                return Err(format!(
                    "server.migrate_on_start: illegal value {other:?} (auto|verify|off)"
                ));
            }
        }
        // 表归属守卫模式（§5.3）：warn（默认）| deny（违规拒绝）；非法值 fail-fast。
        let ownership_deny = ownership_deny_of(&cfg)?;
        // §4.8 归属图 + SchemaRegistry 复活（含 gate=auto 时的逐模块 reconcile）。
        let (registry, modules) = build_schema_and_modules(&dir, ts, &dbs, gate).await?;
        // 种子重放（P0）：各模块 seed.sql（§8-1）。
        crate::seed::replay_all(dbs.get("default"), &dir).await?;
        // fixtures/ 演示数据（§4.5）：仅 oj test（fixtures=true）灌入；server 不灌。
        if fixtures {
            let modules = crate::manifest::discover(&dir, ts)?;
            crate::migrate_cmd::load_fixtures(dbs.get("default"), &modules).await?;
        }
        // 鉴权：守卫由 oj-auth 插件提供（缺插件 fail-fast 已在 build_registries 完成）；
        // jwt 原语配置注入 bridge Extras（JS 端点 jwt.sign/verify 用）。
        let auth: Option<Arc<dyn only_js::bridge::AuthGuard>> = match &cfg.auth {
            Some(a) if a.jwt_secret.trim().is_empty() => {
                return Err("auth.jwt_secret must not be empty".into());
            }
            Some(_) => registries
                .auth
                .map(only_js::bridge::plugin_loader::auth_guard_from_vtable),
            None => None,
        };
        let (jwt, oidc) = build_jwt_and_oidc(&cfg, config_dir)?;
        // 共享事件总线。
        let bus = registries
            .bus
            .connect(&cfg.broker)
            .await
            .map_err(|e| format!("broker: {e}"))?;
        // mq 命名实例（spec §3/§7）：kafkas:/rabbits: 段 → 插件 connect → 命名 registry。
        let (kafkas, rabbits) = build_mq_registries(&cfg, &registries.mq).await?;
        // 单一工厂（内省 / actor 池 / WS 连接共享同一 Bus 与 Extras）——闭包捕获全 Arc，Clone 即共享。
        let make_bridge = {
            let (dbs, kv, loader, es, bus) = (
                dbs.clone(),
                kv.clone(),
                loader.clone(),
                es.clone(),
                bus.clone(),
            );
            let blobs = blobs.clone();
            let kafkas = kafkas.clone();
            let rabbits = rabbits.clone();
            let (registry, modules, ownership_deny) =
                (registry.clone(), modules.clone(), ownership_deny);
            // 影子绑定：`move` 捕获的是这里的副本，外层 `boot` 仍可供后续 StableState 使用。
            let plugins = (*plugin_infos).clone();
            let boot = boot.clone();
            let jwt = jwt.clone();
            let oidc = oidc.clone();
            move || {
                Bridge::with_dbs_and_loader(
                    dbs.clone(),
                    kv.clone(),
                    registry.clone(),
                    false,
                    Some(loader.clone()),
                    Extras {
                        blobs: blobs.clone(),
                        es: es.clone(),
                        bus: Some(bus.clone()),
                        plugins: plugins.clone(),
                        modules: modules.clone(),
                        ownership_deny,
                        boot: boot.clone(),
                        // jwt 原语配置（auth 解耦：JS 端点 jwt.sign/verify 数据源）。
                        jwt: jwt.clone(),
                        // oidc 原语配置（OIDC 解耦：JS 端点 oidc.sign/verify/jwks 数据源）。
                        oidc: oidc.clone(),
                        // mq 命名实例（T7 装配注入；此处空表兜底，编译占位）。
                        kafkas: Some(kafkas.clone()),
                        rabbits: Some(rabbits.clone()),
                        tasks_flag: None,
                    },
                )
            }
        };
        // ext_boot 预热：建 runtime 并跑完 boot，失败即 `Err`（真·启动失败）。
        // 必须前移到建表之前 —— 否则 boot 错误只能借 dev 内省的间接失败暴露，而
        // `bridge_introspector` 会把线程 panic 吞成路由 failure（装配层只 warn 不致命，
        // 结果「路由全空、服务照常监听」）。
        if boot.is_some() {
            prewarm_boot(make_bridge.clone())?;
        }
        // 路由表：dev 启动内省 .route 声明；release 聚合 dist/manifests.yaml。
        let (table, failures) = if ts {
            for m in manifest::load_modules(&dir)? {
                eprintln!("module {} v{} — {}", m.name, m.version, m.desc);
            }
            routes::RouteTable::build(
                &base,
                &dir,
                ts,
                routes::bridge_introspector(make_bridge.clone()),
            )
        } else {
            let lock = manifest::load_lock(&dir.join("manifests.yaml")).map_err(|e| {
                format!(
                    "release mode: {}: {e}",
                    dir.join("manifests.yaml").display()
                )
            })?;
            if lock.is_empty() {
                return Err(format!(
                    "release mode: {} missing or empty — run `oj build` first",
                    dir.join("manifests.yaml").display()
                ));
            }
            let reader = routes::bridge_default_reader(make_bridge.clone());
            let mut entries = Vec::new();
            let b = base.trim_matches('/');
            for (module, version) in &lock {
                manifest::validate_module(module).map_err(|e| format!("manifests.yaml: {e}"))?;
                manifest::validate_version(version).map_err(|e| format!("manifests.yaml: {e}"))?;
                let mdir = dir.join(format!("{module}-{version}"));
                let mf = mdir.join("manifest.yaml");
                if !mf.is_file() {
                    return Err(format!(
                        "release mode: {} missing — run `oj build {module}`",
                        mf.display()
                    ));
                }
                let m = manifest::parse_one(&mf)?;
                if m.name != *module {
                    return Err(format!(
                        "manifest name {:?} != module {module:?} (in {})",
                        m.name,
                        mf.display()
                    ));
                }
                eprintln!("module {} v{} — {}", m.name, m.version, m.desc);
                let rjs = mdir.join("routes.js");
                let v = reader(&rjs).map_err(|e| format!("load {}: {e}", rjs.display()))?;
                for e in routes::entries_from_value(&v) {
                    entries.push(routes::RouteEntry {
                        method: e.method,
                        pattern: format!("/{b}/{}", e.pattern.trim_matches('/')),
                        file: format!("{module}-{version}/{}", e.file),
                    });
                }
            }
            let (table2, failures2) = routes::RouteTable::from_entries(&dir, &entries);
            if !failures2.is_empty() {
                return Err(format!("release routes: {}", failures2.join("; ")));
            }
            (table2, Vec::new())
        };
        for f in &failures {
            eprintln!("error: route: {f}");
        }
        if !failures.is_empty() {
            eprintln!(
                "warn: {} route declaration(s) skipped (see errors above)",
                failures.len()
            );
        }
        for (_, file, methods) in table.grouped() {
            eprintln!("  {}:", file.display());
            for (method, pattern) in methods {
                eprintln!("    {:8} {}", method, pattern);
            }
        }
        let n = cfg.server.pool_size.max(1) as usize;
        let timeout = config::parse_duration(&cfg.server.timeout).ok();
        // actor 池：bridges 与 WS 连接共享同一 Bus 与 Extras。
        let actor = JsActor::pool(n, make_bridge.clone());
        // 静态站点根（server.app_path，CLI --app-path 可覆盖）：相对 config_dir 绝对化（缺失目录 fail-fast）。
        let static_root = resolve_static_root(&cfg, config_dir)?;
        // 证书必配（门禁已确保两路径齐备）→ 加载并校验，证书失效即拒绝启动。
        // 运行中过期由热加载切换到 Grace/Expired → GET 限制（handle 内），服务不中断。
        let (cert_status, cert_valid_until) = load_cert_with_watcher(&cfg, config_dir)?;

        let pipeline = server::Pipeline {
            tenant_header: cfg.tenant.enable.then(|| cfg.tenant.header_key.clone()),
            tenant_anon: cfg.tenant.anonymous_paths.clone(),
            auth,
            max_upload: cfg.server.max_upload_bytes,
            blob: blob.clone(),
        };
        // WS 目录镜像挂载（<dir>/ws.ts → {base}/<dir>/ws）。
        let ws_router = ws::mirror_routes(
            &base,
            &dir,
            timeout.unwrap_or(Duration::from_secs(30)),
            make_bridge,
        );
        let router = server::app(
            &base,
            dir,
            ts,
            table,
            actor,
            timeout,
            static_root,
            pipeline,
            cert_status,
            cert_valid_until,
            plugin_infos.clone(),
        )
        .merge(ws_router);
        // 共享 StableState：与 actor 工厂用同一组后端 Arc，供测试运行时注入（修正 #2）。
        let stable = Arc::new(StableState {
            kv: kv.clone(),
            dbs: dbs.clone(),
            client: reqwest::Client::builder()
                .no_proxy()
                .build()
                .unwrap_or_default(),
            registry: Arc::new(registry),
            loader: Some(loader.clone()),
            blobs: blobs
                .clone()
                .unwrap_or_else(|| Arc::new(BlobRegistry::new())),
            bus: bus.clone(),
            es: es.clone(),
            plugins: (*plugin_infos).clone(),
            modules,
            ownership_deny,
            boot: boot.clone(),
            jwt: jwt.clone(),         // 与 make_bridge 的 Extras.jwt 同源。
            oidc: oidc.clone(),       // 与 make_bridge 的 Extras.oidc 同源。
            kafkas: kafkas.clone(),   // 与 make_bridge 的 Extras.kafkas 同源。
            rabbits: rabbits.clone(), // 与 make_bridge 的 Extras.rabbits 同源。
            tasks_flag: None,
            sql_memo: std::sync::Mutex::new(std::collections::HashMap::new()),
        });
        Ok(App {
            router,
            bus,
            stable,
            base,
        })
    }

    /// 进程内 dispatch：克隆 router + oneshot（零 TCP）。外层 timeout 防 hang（修正 #10）。
    pub async fn dispatch(&self, req: Request<Body>) -> axum::http::Response<Body> {
        let router = self.router.clone();
        match tokio::time::timeout(DISPATCH_TIMEOUT, router.oneshot(req)).await {
            Ok(resp) => resp.expect("oneshot dispatch is infallible"),
            Err(_) => {
                // 超时：返回 408 信封。
                let mut r = axum::http::Response::new(Body::from(
                    only_js::bridge::fail(408, "dispatch timed out", &serde_json::Value::Null).0,
                ));
                *r.status_mut() = StatusCode::REQUEST_TIMEOUT;
                r
            }
        }
    }

    /// 绑定并服务（行为同原 `start`：`.merge(ws)` 已在 from_config 完成；port-0 随机端口）。
    pub async fn serve(self, addr: SocketAddr) -> Result<(SocketAddr, JoinHandle<()>), String> {
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| format!("bind: {e}"))?;
        let bound = listener
            .local_addr()
            .map_err(|e| format!("local_addr: {e}"))?;
        let h = tokio::spawn(async move {
            let _ = server::serve_router(listener, self.router).await;
        });
        Ok((bound, h))
    }

    /// 唯一 StableState（与 actor 共享后端），供测试运行时构造 bridge_ext。
    pub fn stable(&self) -> Arc<StableState> {
        self.stable.clone()
    }

    /// API 基础前缀。
    pub fn base(&self) -> &str {
        &self.base
    }
}

#[async_trait]
impl ClientTransport for App {
    async fn dispatch(&self, req: Request<Body>) -> axum::http::Response<Body> {
        App::dispatch(self, req).await
    }
    fn base(&self) -> &str {
        App::base(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 无 ext_boot.js（现网默认）→ None（静默）；存在 → 冻结 `?v=<mtime>`。
    #[test]
    fn ext_boot_spec_absent_vs_present() {
        let dir = std::env::temp_dir().join(format!("oj-bootspec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(ext_boot_spec(&dir).unwrap().is_none());

        let p = dir.join("ext_boot.js");
        std::fs::write(&p, "globalThis.foo = 1;\n").unwrap();
        let spec = ext_boot_spec(&dir).unwrap().unwrap();
        assert!(spec.starts_with("file://"), "{spec}");
        assert!(spec.contains("?v="), "{spec}");
        assert!(spec.contains("ext_boot.js"), "{spec}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod mq_assembly_tests {
    use super::*;
    use oj_plugin_ffi::{FfiFuture, MqVtable, RString};

    // 假 mq vtable：connect 恒 {"handle":7}，call 回显 payload（装配测试零网络）。
    extern "C" fn fake_connect(cfg: RString) -> FfiFuture {
        let _ = cfg;
        oj_plugin_ffi::ready_ok(br#"{"handle":7}"#)
    }
    extern "C" fn fake_call(_h: u64, m: RString, p: RString) -> FfiFuture {
        let out = format!(r#"{{"method":"{}","payload":{}}}"#, &m[..], &p[..]);
        oj_plugin_ffi::ready_ok(out.into_bytes())
    }
    extern "C" fn fake_close(_h: u64) {}
    static FAKE_MQ: MqVtable = MqVtable {
        connect: fake_connect,
        call: fake_call,
        close: fake_close,
    };

    fn mq_table(names: &[&str]) -> Vec<(String, &'static MqVtable)> {
        names.iter().map(|n| (n.to_string(), &FAKE_MQ)).collect()
    }

    fn cfg_with(section: serde_json::Value, which: &str) -> Config {
        let mut cfg = Config::default();
        let map: std::collections::HashMap<String, serde_json::Value> =
            serde_json::from_value(section).unwrap();
        if which == "kafkas" {
            cfg.kafkas = map;
        } else {
            cfg.rabbits = map;
        }
        cfg
    }

    /// Given: kafkas.default 声明 + bus-kafka 插件；Then: registry 命名实例就位。
    #[tokio::test(flavor = "current_thread")]
    async fn given_kafkas_section_with_kafka_plugin_when_build_then_registered() {
        let cfg = cfg_with(
            serde_json::json!({ "default": { "brokers": ["b:9092"] } }),
            "kafkas",
        );
        let (kafkas, rabbits) = build_mq_registries(&cfg, &mq_table(&["bus-kafka"]))
            .await
            .unwrap();
        assert!(kafkas.get("default").is_some());
        assert!(rabbits.get("default").is_none());
        // kind 注入核验：cfg 透传含 kind=kafka（fake connect 忽略，仅验通路不 panic）
    }

    /// Given: kafkas 声明但无 kafka 插件；Then: fail-fast 文案带 kind（评审 F1/N2）。
    #[tokio::test(flavor = "current_thread")]
    async fn given_kafkas_without_matching_plugin_when_build_then_err() {
        let cfg = cfg_with(serde_json::json!({ "default": {} }), "kafkas");
        let Err(e) = build_mq_registries(&cfg, &mq_table(&["bus-rabbitmq"])).await else {
            panic!("expected fail-fast");
        };
        assert!(e.contains("no mq plugin for kind 'kafka'"), "{e}");
    }

    /// Given: rabbits 声明只装 kafka 插件（负例，评审 N2）；Then: fail-fast。
    #[tokio::test(flavor = "current_thread")]
    async fn given_rabbits_with_only_kafka_plugin_when_build_then_err() {
        let cfg = cfg_with(serde_json::json!({ "default": {} }), "rabbits");
        let Err(e) = build_mq_registries(&cfg, &mq_table(&["bus-kafka"])).await else {
            panic!("expected fail-fast");
        };
        assert!(e.contains("no mq plugin for kind 'rabbit'"), "{e}");
    }

    /// Given: 两插件同名 kind 冲突；Then: fail-fast（评审 S5）。
    #[tokio::test(flavor = "current_thread")]
    async fn given_duplicate_kind_plugins_when_build_then_err_conflict() {
        let cfg = cfg_with(serde_json::json!({ "default": {} }), "kafkas");
        let Err(e) = build_mq_registries(&cfg, &mq_table(&["bus-kafka", "bus-kafka"])).await else {
            panic!("expected fail-fast");
        };
        assert!(e.contains("multiple mq plugins serve kind 'kafka'"), "{e}");
    }

    /// Given: 两段都空；Then: 空 registry（不报错，JS undefined 语义）。
    #[tokio::test(flavor = "current_thread")]
    async fn given_empty_sections_when_build_then_empty_registries() {
        let cfg = Config::default();
        let (kafkas, rabbits) = build_mq_registries(&cfg, &mq_table(&["bus-kafka"]))
            .await
            .unwrap();
        assert!(kafkas.is_empty() && rabbits.is_empty());
    }
}
