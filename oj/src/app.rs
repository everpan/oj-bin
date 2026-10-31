//! 进程内应用装配：`App` 抽取自原 `serve_cmd::start` 的装配段。
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
use only_js::bridge::blob::BlobRegistry;
use only_js::bridge::ldap::{FfiLdapBackend, LdapBackend, LdapConfig};
use only_js::bridge::mail::{FfiMailBackend, MailBackend, MailConfig};
use only_js::bridge::mq::MqInstance;
use only_js::bridge::plugin_loader::kv_backend_connect;
use only_js::bridge::{
    Bridge, EsBackend, Extras, InMemoryKV, JwtCfg, KVStore, LoaderShared, ModuleCtx, ModuleIndex,
    NamedRegistry, SchemaRegistry,
};
use only_js::bridge::{EventBroker, SqlGuard, StableState};
use only_js::config::{self, Config};
use only_js::contract::{InputContract, InputContractRegistry};
use serve::CertificateStatus;
use serve::actor::JsActor;
use serve::certificate::load_certificate_at;
use serve::certificate_watcher::{SharedCertStatus, SharedCertValidUntil};
use serve::routes;
use serve::ws;
use tokio::task::JoinHandle;
use tower::util::ServiceExt;

use crate::manifest;
use crate::serve_cmd::{Registries, assemble_blobs, assemble_plugins, connect_dbs};

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
    /// 后端装配产物（含所有后端连接池 + 插件 vtable）。热更时复用以避免重连（§5 ②）。
    backend: Arc<Backend>,
    base: String,
    /// 长任务停机 flag（spec §6）：信号处理器置位，任务 Bridge 经 Extras 注入。
    tasks_flag: Arc<std::sync::atomic::AtomicBool>,
    /// 任务 Bridge 工厂（tasks_flag = Some(flag)；与 actor 工厂共享全部后端 Arc）。
    make_task_bridge: Arc<dyn Fn() -> Bridge + Send + Sync>,
    /// 鉴权守卫句柄（任务管理 API 复用同一守卫，PRD v2 FR-API-AUTH-001）。
    auth_guard: Option<Arc<dyn only_js::bridge::AuthGuard>>,
    /// 租户头配置（任务管理 API 同管线语义）。
    tenant_header: Option<String>,
    tenant_anon: Vec<String>,
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

/// KV（装配第 6 步）：声明了 `redis.<key>` → 经 kv 插件 vtable connect（单例 fail-fast）；
/// 未声明 → 内置 `InMemoryKV` 兜底。`key` 来自 `--redis`（默认 "default"）。
async fn connect_kv(
    cfg: &Config,
    registries: &Registries,
    key: &str,
) -> Result<Arc<dyn KVStore>, String> {
    match cfg.redis.get(key) {
        Some(url) => match registries.kv {
            Some(vt) => kv_backend_connect(vt, url)
                .await
                .map_err(|e| format!("redis '{key}': {e}")),
            None => Err("config declares redis but no kv plugin loaded \
                 (run `cargo xtask plugin kv-redis`)"
                .to_string()),
        },
        None => {
            if cfg.redis.is_empty() {
                Ok(Arc::new(InMemoryKV::new()) as Arc<dyn KVStore>)
            } else {
                let mut names: Vec<&str> = cfg.redis.keys().map(|s| s.as_str()).collect();
                names.sort_unstable();
                Err(format!(
                    "--redis profile '{key}' not declared (available redis profiles: {names:?})"
                ))
            }
        }
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
    kafka_profile: Option<&str>,
    rabbit_profile: Option<&str>,
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
    for (section, key, kind, reg, profile) in [
        (&cfg.kafkas, "kafkas", "kafka", &mut kafkas, kafka_profile),
        (
            &cfg.rabbits,
            "rabbits",
            "rabbit",
            &mut rabbits,
            rabbit_profile,
        ),
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
        // `--kafka`/`--rabbit`：把选中 profile 别名为 "default"（字面 kafka("default") 即选中源）。
        if let Some(p) = profile {
            reg.set_default_alias(p).map_err(|e| e.to_string())?;
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

/// 多租户 SQL 防护模式（tenant.sql_guard）：enable=false 而 guard 非 Off → warn
/// （防误以为已防护）；非法值已在 config 反序列化期 fail-fast。
fn sql_guard_of(cfg: &Config) -> SqlGuard {
    let g = cfg.tenant.sql_guard;
    if !cfg.tenant.enable && g != SqlGuard::Off {
        eprintln!(
            "warn: tenant.sql_guard {g:?} 生效中但 tenant.enable=false \
             （http.tenantId 恒为 None，防护形同虚设）"
        );
    }
    g
}

/// v0.1.20 通配语义收紧的**迁移**提示——**只针对 `auth.anonymous_paths`**（v0.1.23 起按
/// 影响面判定，不再「凡尾 `/*` 即告警」）。
///
/// **为什么只有 auth**：v0.1.20 的收紧只发生在 oj-auth——插件侧旧实现是
/// `strip_suffix("/*")` 加 `starts_with` 再加 `len > prefix.len()`（即尾 `*` = 任意深度）；
/// **租户侧自引入起就是严格一层**：`git show v0.1.19:server/src/lib.rs` 的 `path_matches`
/// 即 `!rest[1..].contains('/')`，且 v0.1.19 的租户豁免（同文件 `:364`）走的就是它。
/// 对 tenant 条目说「收紧收回了面」是伪前提（v0.1.20 CHANGELOG 兼容性条目把两条列表并列，
/// 措辞不精确，v0.1.23 已订正）。
///
/// 两个条件**同时成立**才告警：
/// 1. 条目是**旧式前缀形态**——只有尾段那一个 `*`，其余段全是字面量（`is_legacy_prefix_shape`）。
///    旧 oj-auth 实现只对「字面前缀 + 尾 `*`」生效；含中段 `*` 的结构条目（`/public/anchor/*/issues/*`）
///    在当时**匹配不上任何真实请求**（前缀里带字面 `*`），只能是 v0.1.20 四形态语义下刻意
///    写出的，对它们提「改 `**`」是错的。
/// 2. 该条目**确实丢了面**——存在一条已注册路由可达「head + ≥2 段」的路径（`tail_entry_loses_coverage`）。
///    这正是「旧任意深度」比「严格一层」多的部分；`**` 额外多出的零层（裸 head）在旧语义下
///    也没覆盖，不算丢面（不为此告警）。
///
/// 只按「已注册路由」判是充分的：静态托管与 `/blob` 都在鉴权前直接返回，不经
/// `anonymous_paths`（`server/src/lib.rs`，两处各留互指注释）——匿名路径只对注册路由有效。
/// 条目带 `one_layer: true`（`config::AnonPath::Detailed`）时永不告警，留给「明知影响面仍
/// 要严格一层」的人工确认。
///
/// `routes` = 路由表 pattern 的去 base 视图（`{param}` 段已归一为 `*`，见 `anon_view_of_route`）。
fn warn_legacy_tail_wildcards(cfg: &Config, routes: &[String]) {
    for (what, legacy) in migration_warn_entries(cfg, routes) {
        eprintln!(
            "warn: {what} 的 {} 条尾 \"/*\" 条目 {legacy:?}：改写为 \"…/**\" 会多命中\
             已注册路由（自 v0.1.20 起尾 \"/*\" 已是严格一层）；需要多层就改 \"…/**\"，\
             确属「有意一层」则写 `{{ path: …, one_layer: true }}` 消音",
            legacy.len()
        );
    }
}

/// 迁移 WARN 的点名清单：(what, entries)。**只有 `auth.anonymous_paths` 参与**——v0.1.20 的
/// 收紧只发生在 oj-auth 侧（见 `warn_legacy_tail_wildcards` 的文档），租户侧自引入起即为
/// 严格一层，对它提「收紧收回了面」是伪前提。抽成纯函数是为了让「哪些列表参与」可被单测钉住
/// （而不是把过滤链抄进测试里）。
fn migration_warn_entries<'a>(
    cfg: &'a Config,
    routes: &[String],
) -> Vec<(&'static str, Vec<&'a str>)> {
    let Some(a) = &cfg.auth else {
        return Vec::new();
    };
    let legacy = legacy_entries(&a.anonymous_paths, routes);
    if legacy.is_empty() {
        Vec::new()
    } else {
        vec![("auth.anonymous_paths", legacy)]
    }
}

/// 迁移 WARN 的点名清单（纯函数，便于单测）：`one_layer` 确认过的、非旧前缀形态的、
/// 以及没丢面的条目都被滤掉。
fn legacy_entries<'a>(list: &'a [config::AnonPath], routes: &[String]) -> Vec<&'a str> {
    list.iter()
        .filter(|p| !p.one_layer())
        .map(config::AnonPath::path)
        .filter(|p| tail_entry_loses_coverage(p, routes))
        .collect()
}

/// 旧前缀条目是否「丢了面」：**旧式前缀形态**（`is_legacy_prefix_shape`）且存在任一条已注册
/// 路由的可达路径是「head + ≥2 段」（head = 去掉尾 `/*` 的部分）。非尾 `/*` 恒为假。
fn tail_entry_loses_coverage(entry: &str, routes: &[String]) -> bool {
    if !is_legacy_prefix_shape(entry) {
        return false;
    }
    let Some(head) = entry.strip_suffix("/*") else {
        return false;
    };
    let head = split_segments(head);
    routes
        .iter()
        .any(|r| view_reaches_beyond_one_layer(&split_segments(r), &head))
}

/// 路由视图能否产出「以 `head` 为前缀、且比 `head` 深 ≥2 段」的路径。
///
/// 视图段语义（`anon_view_of_route` 归一后）：字面段 / `*`（任意一段）/ `**`（任意多段，含零段）。
/// 递归回溯消费 `head`：`**` 既可吃零段（视图前进）也可吃一段（`head` 前进，`**` 保留）。
fn view_reaches_beyond_one_layer(view: &[&str], head: &[&str]) -> bool {
    let Some((&h, rest_head)) = head.split_first() else {
        return view_can_produce_at_least_two(view);
    };
    match view.split_first() {
        None => false,
        Some((&"**", rest)) => {
            view_reaches_beyond_one_layer(rest, head)
                || view_reaches_beyond_one_layer(view, rest_head)
        }
        Some((&"*", rest)) => view_reaches_beyond_one_layer(rest, rest_head),
        Some((v, rest)) => *v == h && view_reaches_beyond_one_layer(rest, rest_head),
    }
}

/// 视图段序列能否产出 **≥2 段** 的路径：`**` 可产出任意段数（含 2），否则段数 = 非 `**` 段数。
fn view_can_produce_at_least_two(view: &[&str]) -> bool {
    let mut literals = 0usize;
    for s in view {
        if *s == "**" {
            return true;
        }
        literals += 1;
    }
    literals >= 2
}

/// 条目是否为「旧式前缀」形态：只有**尾段**那一个 `*`，其余段全是字面量。
///
/// 迁移 WARN 只对这种形态有意义——旧 oj-auth 实现是 `strip_suffix("/*")` + `starts_with`
/// （尾 `*` = 任意深度），而含中段 `*` 的条目在当时**匹配不上任何真实请求**（前缀里带着
/// 字面 `*`），只能诞生于 v0.1.20 的四形态语义之下，即「刻意的结构」而非「待迁移的旧前缀」
/// ——对它们提「改 `**`」是错的（`**` 会把 `…/*/issues/**` 之外的多层路径一并纳入）。
/// 实测依据：下游 config 里 `/public/anchor/*/issues/*` 这类结构条目正是这么写的。
///
/// 注：这里用**裸后缀**口径（与旧实现的 `strip_suffix("/*")` 逐字一致）——`/x/*/` 这类
/// 尾斜杠写法在旧实现下同样不进任意深度分支，故不算旧前缀（匹配层忽略空段，见
/// `validate_anon_paths` 的段级判据，两处口径差异是有意的）。
fn is_legacy_prefix_shape(entry: &str) -> bool {
    match entry.strip_suffix("/*") {
        Some(head) => split_segments(head).iter().all(|seg| !seg.contains('*')),
        None => false,
    }
}

/// 路径 → 段（与 `serve::split_segments` 同口径：去空段，故尾斜杠与重复斜杠不影响判定）。
fn split_segments(path: &str) -> Vec<&str> {
    path.split('/').filter(|s| !s.is_empty()).collect()
}

/// 路由 pattern → 匿名路径口径：剥掉 base 前缀，参数段 `{param}` / `{*rest}` 归一为
/// `*` / `**`（匿名路径按**去 base 后**的请求路径匹配，见 `serve::path_matches`）。
fn anon_view_of_route(pattern: &str, base: &str) -> String {
    let b = base.trim_matches('/');
    let p = pattern.strip_prefix(&format!("/{b}")).unwrap_or(pattern);
    p.split('/')
        .map(
            |seg| match seg.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
                Some(inner) if inner.starts_with('*') => "**",
                Some(_) => "*",
                None => seg,
            },
        )
        .collect::<Vec<_>>()
        .join("/")
}

/// 归属图 + SchemaRegistry 复活（§4.8，装配第 11 步；v0.1.58 多挂载）：逐 api 树
/// discover 全模块 → schema.yaml + manifest(db/deps) → registry（S002 同表双声明
/// fail-fast，含跨树；table_owned 记 owner）+ ModuleCtx map（键 = 模块目录绝对路径，
/// run_module 祖先命中注入）。**模块名跨树重复 → 报错**（旧单树形态下
/// `module_map.insert` 会静默共存，多挂载下同名模块的路由归属会变得不可判读）。
/// reconcile 不在此执行——由 server 装配层在迁移 gate 后显式调用
/// `schema::reconcile_all`（spec §2.2 #12）。
async fn build_schema_and_modules(
    trees: &[(PathBuf, bool)],
    guard: SqlGuard,
    shared_allow: &[String],
    module_index: Option<&ModuleIndex>,
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
    let mut module_owner: std::collections::HashMap<String, PathBuf> =
        std::collections::HashMap::new();
    for (dir, ts) in trees {
        for (name, mdir) in manifest::discover(dir, *ts, module_index)? {
            if let Some(prev) = module_owner.get(&name) {
                return Err(format!(
                    "module {name:?} declared in multiple places ({} and {}) — module names must be unique across api mounts",
                    prev.display(),
                    mdir.display()
                ));
            }
            module_owner.insert(name.clone(), mdir.clone());
            let mf = manifest::parse_one(&mdir.join("manifest.yaml"))?;
            if let Some(f) = crate::schema::SchemaFile::load(&mdir)? {
                if guard != SqlGuard::Off {
                    f.validate_tenant(&name)?;
                    for st in f.shared_tables() {
                        if !shared_allow.iter().any(|a| a == st) {
                            eprintln!(
                                "warn: [{name}] 共享表声明 {st:?} 未列入 tenant.shared_allow，按受租户约束处理（tenant_id 列校验将生效）"
                            );
                        }
                    }
                }
                // 列类型未识别 → 拒载并点名（不再静默当 text；见 SchemaFile::registry_tables）。
                for (t, pk, cols, tenant_flag) in
                    f.registry_tables().map_err(|e| format!("[{name}] {e}"))?
                {
                    if registry.has_table(t) {
                        return Err(format!(
                            "S002: 表 {t:?} 被多个模块声明（{} 与 {name}）",
                            registry.owner_of(t).unwrap_or("?")
                        ));
                    }
                    // 共享表 = 显式 tenant:false 且列于 shared_allow 白名单（交集，fail-closed）。
                    let shared = !tenant_flag && shared_allow.iter().any(|a| a == t);
                    // v0.1.24：带列类型装配（租户守卫按 tenant_id 列类型绑定数值/字符串）。
                    registry = registry.table_owned_shared_typed(&name, t, &pk, &cols, shared);
                }
            }
            module_map.insert(
                mdir.to_string_lossy().into_owned(),
                ModuleCtx {
                    name: name.clone(),
                    // http.module.version 数据源：dev = src/<m>/manifest.yaml；
                    // release = dist/<m>-<v>/manifest.yaml（build 时原样拷贝）。
                    version: mf.version.clone(),
                    deps: Arc::new(mf.deps.keys().cloned().collect()),
                    db: mf.db.clone(),
                },
            );
        }
    }
    Ok((registry, Arc::new(module_map)))
}

/// jwt / oidc 原语配置（auth 与 OIDC 解耦后注入 bridge Extras 的两项）。
type JwtOidcCfg = (
    Option<Arc<JwtCfg>>,
    Option<Arc<only_js::bridge::oidc::OidcState>>,
);

/// platform 段装配产物（grants 对；`platform:` 缺省 = (None, None) 全 deny）。
type PlatformGrants = (
    Option<Arc<only_js::bridge::platform::PlatformGrant>>,
    Option<Arc<only_js::bridge::platform::ExecGrant>>,
);

/// platform 段装配（ojm 批次，设计稿 §1.2「抢注防护与受控挂载」）：
/// ① **受控挂载根**（特权名单非空时必配）：`platform.root` 须作为 api mount 接入
///    装配（模块归属/路由只能来自挂载树 discover，目录祖先命中机制不变）；其
///    discover 产物名 P 豁免抢注检查，且 `modules` ⊆ P（悬空特权 fail-fast）。
/// ② **抢注防护**（fail-fast）：特权名出现在市场安装树（api 挂载树 discover 产物）
///    且不在 P 中 → Err 点名——「安装名为 X 的 bundle = 继承 X 特权」被装配校验阻断；
///    同名同时出现在受控根与另一挂载树由 build_schema_and_modules 的跨树唯一性
///    检查先行拦截。特权模块只能由部署方放入 `platform.root`。
/// ③ grants 构建：[`PlatformGrant`]（canonical trusted 集合，fs.root 自动并入，
///    trusted_paths 相对 config_dir 解析、不存在 fail-fast）与 [`ExecGrant`]
///    （argv 模板原样保存，阶段 3 解析匹配）。`platform:` 缺省 = (None, None) 全 deny。
pub(crate) fn build_platform_grants(
    cfg: &Config,
    config_dir: &Path,
    fs_grant: &Option<Arc<only_js::bridge::fs::FsGrant>>,
    trees: &[(PathBuf, bool)],
    installed: &std::collections::HashSet<String>,
    module_index: Option<&ModuleIndex>,
) -> Result<PlatformGrants, String> {
    let Some(p) = &cfg.platform else {
        return Ok((None, None));
    };
    // 受控挂载根校验（特权名单非空时）：root 必配 + 必须已挂载为 api mount +
    // discover 产物名集合 P。
    let p_names: Option<std::collections::HashSet<String>> = if p.modules.is_empty() {
        None
    } else {
        let Some(root_cfg) = p.root.as_deref() else {
            return Err("platform.modules is set but platform.root is missing — \
                 privileged modules need a controlled mount root"
                .into());
        };
        let raw = Path::new(root_cfg);
        let proot = if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            config_dir.join(raw)
        };
        let proot = proot.canonicalize().map_err(|e| {
            format!(
                "platform.root canonicalize failed ({}): {e}",
                proot.display()
            )
        })?;
        // 互斥校验：受控根与任何**其它** api 挂载树不得同径或互为父子（前缀关系）——
        // 防部署方误把受控根指到市场安装树（含父目录形态：受控根本身并非挂载树，
        // 其子树里的市场产物会静默穿透抢注绊线）。同径重复树由跨树唯一性检查拦截。
        for (t, _) in trees {
            if *t == proot {
                continue;
            }
            if t.starts_with(&proot) || proot.starts_with(t) {
                return Err(format!(
                    "platform.root ({}) must not overlap another api mount ({}) — \
                     the anti-squatting tripwire would be silently disabled",
                    proot.display(),
                    t.display()
                ));
            }
        }
        let (troot, tts) = trees.iter().find(|(r, _)| *r == proot).ok_or_else(|| {
            format!(
                "platform.root ({}) must be mounted as an api mount \
                 to give platform modules routes and ownership",
                proot.display()
            )
        })?;
        Some(
            crate::manifest::discover(troot, *tts, module_index)?
                .into_iter()
                .map(|(n, _)| n)
                .collect(),
        )
    };
    for name in &p.modules {
        // 抢注（豁免受控根产物 P）：特权名出现在市场安装树且不在受控根下 → fail-fast。
        if installed.contains(name) && !p_names.as_ref().is_some_and(|s| s.contains(name)) {
            return Err(format!(
                "platform module '{name}' must not come from an install tree \
                 (deploy it in a platform-controlled directory under platform.root)"
            ));
        }
    }
    if let Some(p_names) = &p_names {
        for name in &p.modules {
            if !p_names.contains(name) {
                return Err(format!(
                    "platform module '{name}' is not deployed under platform.root \
                     (dangling privilege: no module implementation)"
                ));
            }
        }
    }
    // trusted_paths 相对 config_dir 解析（对齐 fs.root 的 static_dir 同形）；
    // canonicalize + 不存在 fail-fast 归 platform.rs 单点。
    let trusted_raw: Vec<PathBuf> = p
        .trusted_paths
        .iter()
        .map(|tp| {
            let pp = Path::new(tp);
            if pp.is_absolute() {
                pp.to_path_buf()
            } else {
                config_dir.join(pp)
            }
        })
        .collect();
    let fs_root = fs_grant.as_ref().map(|g| g.root.clone());
    let platform = Arc::new(only_js::bridge::platform::PlatformGrant::new(
        p.modules.clone(),
        &trusted_raw,
        fs_root.as_deref(),
    )?);
    let exec = Arc::new(only_js::bridge::platform::ExecGrant::new(
        p.exec
            .iter()
            .map(|(m, c)| (m.clone(), (c.allow.clone(), c.env.clone())))
            .collect(),
    ));
    Ok((Some(platform), Some(exec)))
}

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

/// http_sso 段 → bridge `Extras.sso`（v0.1.61）。段不存在 = None（JS 报 not configured）。
fn build_sso(cfg: &Config) -> Option<Arc<serde_json::Value>> {
    cfg.http_sso
        .as_ref()
        .map(|m| Arc::new(serde_json::to_value(m).expect("http_sso serialize")))
}

/// mail 装配（装配第 N 步，spec 2026-09-15 §4）：顶层 `smtp:` 段 + oj-mail 插件 vtable
/// → `Arc<dyn MailBackend>`（注入 `Extras.mail` 与 `StableState.mail`，两者同源）。
///
/// **一段两用、同源**：给插件的 cfg 与宿主校验面取自 `plugin_cfg(cfg, "mail")`
/// **同一份 JSON**（`plugins.mail` 透传优先，其次顶层 `smtp:` 段）。宿主只吸收
/// `allowed_from`/`allowed_recipients`（`MailConfig::from_value`），凭据不落宿主。
/// 该 JSON 的形态错误（如 `allowed_from` 写了数字）在**装配期** fail-fast，
/// 不静默退化成「无白名单 → 全部拒绝」。
///
/// `bus` 必须与 `Extras.bus` 同一实例：插件经 `deliver("mail.result")` 上送的结果
/// 由宿主路由存下并扇出给同总线的 JS 订阅者。
///
/// 未配 `smtp:`（cfg 为空）或插件未加载 → `None`：`mail.*` 调用报
/// "mail not configured"（与 es/auth 的「未配置」语义一致）。
pub fn build_mail_backend(
    cfg: &Config,
    top: &serde_json::Value,
    vtable: Option<&'static oj_plugin_ffi::MailVtable>,
    bus: Arc<dyn EventBroker>,
) -> Result<Option<Arc<dyn MailBackend>>, String> {
    // 插件未加载（Registrations.mail 为空）→ 无投递能力，不挂后端。
    let Some(vtable) = vtable else {
        return Ok(None);
    };
    // A5：双配置源（非空 `smtp:` ＋ 非空 `plugins.mail`）此处即 fail-fast ——
    // `plugin_cfg` 会让透传静默胜出，不能等到「改了白名单不生效」才发现。
    crate::serve_cmd::check_mail_cfg_sources(cfg)?;
    let json = crate::serve_cmd::plugin_cfg(cfg, top, "mail", None, None).unwrap();
    let value: serde_json::Value =
        serde_json::from_str(&json).map_err(|e| format!("smtp cfg: {e}"))?;
    // 空 cfg（既无 `smtp:` 段也无 `plugins.mail` 透传）→ 视作未配置。
    if value.as_object().is_none_or(|o| o.is_empty()) {
        return Ok(None);
    }
    let mail_cfg = MailConfig::from_value(&value).map_err(|e| format!("smtp: {e}"))?;
    Ok(Some(Arc::new(FfiMailBackend::new(vtable, mail_cfg, bus))))
}

/// ldap 装配：顶层 `ldap:` 段 + oj-ldap 插件 vtable → `Arc<dyn LdapBackend>`。
///
/// 与 mail 同形：给插件的 cfg 与宿主校验面取自 `plugin_cfg(cfg, "ldap")` 同一份 JSON；
/// 实例字段的白名单由 `LdapConfig::from_value` 裁决（未知键装配期 fail-fast）。
/// 未配 `ldap:`（段缺省/空）或插件未加载 → `None`：`ldap.*` 报 "ldap not configured"。
pub fn build_ldap_backend(
    cfg: &Config,
    top: &serde_json::Value,
    vtable: Option<&'static oj_plugin_ffi::LdapVtable>,
) -> Result<Option<Arc<dyn LdapBackend>>, String> {
    let Some(vtable) = vtable else {
        return Ok(None);
    };
    crate::serve_cmd::check_ldap_cfg_sources(cfg)?;
    let json = crate::serve_cmd::plugin_cfg(cfg, top, "ldap", None, None).unwrap();
    let value: serde_json::Value =
        serde_json::from_str(&json).map_err(|e| format!("ldap cfg: {e}"))?;
    if value.as_object().is_none_or(|o| o.is_empty()) {
        return Ok(None);
    }
    let ldap_cfg = LdapConfig::from_value(&value).map_err(|e| format!("ldap: {e}"))?;
    Ok(Some(Arc::new(FfiLdapBackend::new(vtable, ldap_cfg))))
}

/// 停机排空的**逻辑**（[`App::drain_mail`] 的唯一实现；独立成函数便于单测）：
/// 经 `MailBackend::drain`（控制报文）让插件停收新投递、等在途 job 跑完再销毁 transport。
///
/// 返回统一信封：`code:0`（`data.drained` 标记是否真排空）/ `code:1`（超时，在途可能被丢弃）；
/// 后端调用失败也收敛为 `code:1` 信封（停机路径只告警，不阻断进程退出）。
pub fn drain_mail_backend(mail: &Arc<dyn MailBackend>, timeout: Duration) -> serde_json::Value {
    match mail.drain(timeout) {
        Ok(v) => v,
        Err(e) => serde_json::json!({
            "code": 1, "msg": format!("mail: drain 调用失败（{e}）"), "data": {"drained": false},
        }),
    }
}

/// 装配期挂载 → 运行时挂载：api 条目配各自的表 + dev 兜底，并**吸收**同 prefix 的
/// web（配对 web 无 spa——规则 2 已在 config 层拒绝 `spa: true` + api 同 prefix）；
/// 未被吸收的 web 条目原样成独立挂载。声明序保留（primary = 第一条 api）。
fn build_runtime_mounts(
    resolved: Vec<ResolvedMount>,
    tables: Vec<(String, routes::RouteTable)>,
) -> Vec<serve::RuntimeMount> {
    let mut out = Vec::new();
    let mut paired: std::collections::HashSet<String> = Default::default();
    for m in &resolved {
        if let Some((dir, ts)) = m.kind.api() {
            let table = tables
                .iter()
                .find(|(p, _)| *p == m.prefix)
                .map(|(_, t)| t.clone())
                .unwrap_or_default();
            let web = resolved.iter().find_map(|w| match &w.kind {
                ResolvedMountKind::Web { root, headers, .. }
                    if w.prefix == m.prefix && !paired.contains(&w.prefix) =>
                {
                    paired.insert(w.prefix.clone());
                    Some(serve::WebMount {
                        prefix: w.prefix.clone(),
                        root: root.clone(),
                        headers: headers.clone(),
                        spa: false,
                    })
                }
                _ => None,
            });
            out.push(serve::RuntimeMount::Api {
                prefix: m.prefix.clone(),
                table,
                fallback: ts.then(|| routes::Routes::new(&m.prefix, dir, ts)),
                web,
            });
        }
    }
    for m in &resolved {
        if let ResolvedMountKind::Web { root, headers, spa } = &m.kind
            && !paired.contains(&m.prefix)
        {
            paired.insert(m.prefix.clone());
            out.push(serve::RuntimeMount::Web(serve::WebMount {
                prefix: m.prefix.clone(),
                root: root.clone(),
                headers: headers.clone(),
                spa: *spa,
            }));
        }
    }
    out
}

/// `server.html_meta_handler`（v0.1.25）装配期校验：必须命中**某个 api 挂载**路由表里的
/// 一个 **GET** 路由（v0.1.58 多挂载：按 handler 前缀定表；不在任何前缀下 → 报错）。
///
/// fail-fast 而非启动后 WARN：配错的后果（页面 meta 没注入）只在爬虫/IM 预览侧可见，
/// 业务页面自己看不出来——留给运行期的告警等于让配置撒谎。
fn validate_html_meta_handler(
    cfg: &Config,
    tables: &[(String, routes::RouteTable)],
) -> Result<(), String> {
    let Some(h) = cfg.server.html_meta_handler.as_deref() else {
        return Ok(());
    };
    let norm = routes::normalize(h)
        .ok_or_else(|| format!("server.html_meta_handler: {h:?} 不是合法路径（须以 / 开头）"))?;
    let under = |p: &str| {
        let p = p.trim_end_matches('/');
        p.is_empty() || norm == p || norm.starts_with(&format!("{p}/"))
    };
    for (prefix, table) in tables {
        if !under(prefix) {
            continue;
        }
        return match table.lookup(&norm, "GET") {
            routes::Lookup::Hit { .. } => Ok(()),
            routes::Lookup::Conflict(m) => {
                Err(format!("server.html_meta_handler: {h:?} 路由冲突：{m}"))
            }
            routes::Lookup::MethodNotAllowed => Err(format!(
                "server.html_meta_handler: {h:?} 未映射 GET 方法（meta handler 只能是 GET）"
            )),
            routes::Lookup::NotFound => Err(format!(
                "server.html_meta_handler: {h:?} 不在路由表（拼错了？）"
            )),
        };
    }
    Err(format!(
        "server.html_meta_handler: {h:?} 不在任何 api 挂载前缀下（挂载前缀：{:?}）",
        tables.iter().map(|(p, _)| p).collect::<Vec<_>>()
    ))
}

/// 静态目录绝对化 + canonicalize（缺失/非目录 → fail-fast）。
fn static_dir(config_dir: &Path, r: &str) -> Result<PathBuf, String> {
    let p = Path::new(r);
    let p = if p.is_absolute() {
        p.to_path_buf()
    } else {
        config_dir.join(p)
    };
    p.canonicalize()
        .map_err(|e| format!("挂载目录 {}: {e}", p.display()))
}

/// 装配期挂载（[`resolve_mounts`] 产物，v0.1.58）：前缀已归一（首斜杠、无尾斜杠、
/// `/` 允许），目录已 canonicalize，api 逐条目 dev/release 判定。**声明序保留**
/// （primary = 第一条 api 挂载）；最长前缀排序归 `serve::app()` 独家负责。
#[derive(Debug)]
pub struct ResolvedMount {
    pub prefix: String,
    pub kind: ResolvedMountKind,
}

#[derive(Debug)]
pub enum ResolvedMountKind {
    /// api 挂载：服务该目录下的模块路由；`ts` = 逐条目 `is_release` 取反（目录即模式）。
    Api { dir: PathBuf, ts: bool },
    /// web 挂载：静态站点；`spa` = 显式 `spa: true`（默认 false）。
    Web {
        root: PathBuf,
        headers: Vec<(String, String)>,
        spa: bool,
    },
}

impl ResolvedMountKind {
    pub(crate) fn api(&self) -> Option<(&Path, bool)> {
        match self {
            ResolvedMountKind::Api { dir, ts } => Some((dir, *ts)),
            _ => None,
        }
    }
}

/// 有效挂载表（装配，v0.1.58 规则 4）：`cfg.mounts` 逐条目 → 前缀经
/// `resolve_app_prefix` 归一（同 prefix **同类**重复 → Err；api+web 配对合法，后续
/// 内联）；目录相对 config_dir 绝对化 + canonicalize（缺失 fail-fast）；api 条目
/// 逐条目 `is_release` 判 dev/release。允许根 `/`（api 路由 `/<module>`，内建
/// `/health`/`/plugins`/`/blob/{key}`）。结构规则 1/2 已在 config 层校验。
pub(crate) fn resolve_mounts(
    cfg: &Config,
    config_dir: &Path,
) -> Result<Vec<ResolvedMount>, String> {
    let mut out = Vec::new();
    let mut seen: Vec<(String, bool)> = Vec::new(); // (归一 prefix, is_api)
    for (i, m) in cfg.mounts.iter().enumerate() {
        let p = crate::serve_cmd::resolve_app_prefix(&m.prefix)
            .map_err(|e| format!("mounts[{i}]: prefix {:?}: {e}", m.prefix))?;
        let is_api = m.api.is_some();
        if seen.iter().any(|(q, a)| *q == p && *a == is_api) {
            let kind = if is_api { "api" } else { "web" };
            return Err(format!("mounts[{i}]: duplicate {kind} mount prefix {p:?}"));
        }
        seen.push((p.clone(), is_api));
        match (&m.api, &m.web) {
            (Some(api), None) => {
                let (dir, ts) = if config::is_http_url(api) {
                    // URL 形态：装配期拉取并物化到内容寻址缓存，release 语义服务。
                    // 校验层（validate_mounts）已保证此处 sha256 必存在且为 64 hex。
                    let sha = m.sha256.as_deref().ok_or_else(|| {
                        format!("mounts[{i}]: URL api requires sha256 integrity anchor")
                    })?;
                    let cache_root = match &m.cache {
                        Some(c) => {
                            let p = Path::new(c);
                            if p.is_absolute() {
                                p.to_path_buf()
                            } else {
                                config_dir.join(p)
                            }
                        }
                        None => config_dir.join(".oj-cache"),
                    };
                    let dir = crate::mount_fetch::materialize(api, sha, &cache_root)
                        .map_err(|e| format!("mounts[{i}]: {e}"))?;
                    let ts = !crate::serve_cmd::is_release(&dir);
                    (dir, ts)
                } else {
                    let dir =
                        static_dir(config_dir, api).map_err(|e| format!("mounts[{i}]: {e}"))?;
                    let ts = !crate::serve_cmd::is_release(&dir);
                    (dir, ts)
                };
                out.push(ResolvedMount {
                    prefix: p,
                    kind: ResolvedMountKind::Api { dir, ts },
                });
            }
            (None, Some(web)) => {
                let root = static_dir(config_dir, web).map_err(|e| format!("mounts[{i}]: {e}"))?;
                out.push(ResolvedMount {
                    prefix: p,
                    kind: ResolvedMountKind::Web {
                        root,
                        headers: m
                            .headers
                            .iter()
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect(),
                        spa: m.spa.unwrap_or(false),
                    },
                });
            }
            _ => {
                return Err(format!(
                    "mounts[{i}]: exactly one of `api` or `web` is required"
                ));
            }
        }
    }
    Ok(out)
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
            // 证书过期且宽限结束属启动期致命配置错误：硬断言中止，而非打一条可被误读为
            // 「运行时可恢复 ERROR」的日志（服务本就不应启动）。消息含 "certificate" 便于
            // 集成测试与运维从 panic 载荷快速定位。
            panic!("certificate has expired and grace period elapsed — service will not start");
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
    // 进程级单例：热更每代 `from_config` 不重复起 watcher 线程（避免线程/inotify fd 累积）。
    serve::certificate_watcher::ensure_watcher(
        cert_status.clone(),
        cert_valid_until.clone(),
        &cfg.server,
        config_dir,
    );
    Ok((cert_status, cert_valid_until))
}

/// 后端装配产物（spec §2.1）：`stable` 是唯一数据源——kv/dbs/registry/loader/blobs/bus/
/// es/modules/plugins/... 全在 StableState，Backend 不逐字段复制；auth_guard 与 Bridge
/// 工厂因不进 StableState 而单独持有。exec/test 的手工 runtime 与 server 的 App 共用
/// 同一份后端。
pub struct Backend {
    /// 唯一数据源：手工 `JsRuntime`（exec/test）经 `stable()` 注入（§2.1）。
    stable: Arc<StableState>,
    /// auth 守卫（oj-auth 插件 vtable 构造；cfg.auth 未配置 → None）。
    auth_guard: Option<Arc<dyn only_js::bridge::AuthGuard>>,
    /// Bridge 工厂。tasks_flag 参数化：None = HTTP 桥，Some = 任务桥。
    make_bridge_of: Arc<dyn Fn(Option<Arc<std::sync::atomic::AtomicBool>>) -> Bridge + Send + Sync>,
    /// 入参契约注册表（v0.1.44）。与 `StableState.input_contracts` **同一个 Arc**：
    /// 路由表建成后由装配写入一次，所有 Bridge（含已构造的）立即共享。
    input_contracts: Arc<std::sync::RwLock<InputContractRegistry>>,
}

impl Backend {
    /// StableState 单源访问器。
    pub fn stable(&self) -> &Arc<StableState> {
        &self.stable
    }
    /// auth 守卫句柄（HTTP 层 Pipeline / App 消费）。
    pub fn auth_guard(&self) -> Option<&Arc<dyn only_js::bridge::AuthGuard>> {
        self.auth_guard.as_ref()
    }
    /// HTTP 桥工厂（= make_bridge_of(None)）。
    pub fn make_bridge(&self) -> impl Fn() -> Bridge + Send + Sync {
        let f = self.make_bridge_of.clone();
        move || f(None)
    }
    /// 入参契约注册表句柄（装配后填充；回填给 ServerInputContract 用）。
    pub fn input_contracts(&self) -> &Arc<std::sync::RwLock<InputContractRegistry>> {
        &self.input_contracts
    }
    /// 任务桥工厂（tasks_flag = Some(flag)；停机 flag 由 HTTP 层创建并传入）。
    pub fn make_task_bridge(
        &self,
        flag: Arc<std::sync::atomic::AtomicBool>,
    ) -> Arc<dyn Fn() -> Bridge + Send + Sync> {
        let f = self.make_bridge_of.clone();
        Arc::new(move || f(Some(flag.clone())))
    }
}

/// 迁移门禁取值（§4.6，装配与 HTTP 层共用）：dev（ts）默认 auto，release 默认 verify；
/// `migrate_on_start: off` 为逃生门；非法值在 HTTP 层 match 处 fail-fast。
fn migrate_gate_of(cfg: &Config, ts: bool) -> &str {
    cfg.server
        .migrate_on_start
        .as_deref()
        .unwrap_or(if ts { "auto" } else { "verify" })
}

/// 后端装配（spec §2.2 归属表 #1-#20 + #23）：redis warn → loader → ext_boot → 插件 →
/// kv/es/blob → dbs+db_override → query_limits → ownership/sql_guard → schema+modules →
/// auth/jwt/oidc → bus → mail（显式 install deliver 路由）→ ldap → mq → vars →
/// StableState 组装 + Bridge 工厂。23 步的顺序即语义在后段内部保持原相对顺序；
/// HTTP 步（证书门禁/匿名路径校验/迁移 gate 执行/seed/fixtures/ext_boot 预热/路由表）
/// 留在 `App::from_config`——exec/test 手工 runtime 经此拿到与 server 同一份后端
/// （无证书门禁，故可在无证书 config 上构造）。
///
/// 各资源根 key 的默认 profile 选择（对应 `oj test`/`oj exec` 的 `--db`/`--redis`/`--blob`/
/// `--es`/`--broker`/`--kafka`/`--rabbit`）。None → 字面 "default"；给定 profile 在 config 段中
/// 不存在 → 装配期 fail-fast。`db` 在请求期做字面 "default" 重定向；其余轴在装配期把选中
/// profile 烘焙为 "default" 别名（或选为唯一活跃实例）。
#[derive(Default, Clone)]
pub struct ResourceProfiles {
    pub db: Option<String>,
    pub redis: Option<String>,
    pub blob: Option<String>,
    pub es: Option<String>,
    pub broker: Option<String>,
    pub kafka: Option<String>,
    pub rabbit: Option<String>,
}

/// `base` = blob 下载 URL 前缀（local driver 的 `LocalBlob::named` 烘焙进注册表）：
/// server 传 CLI 解析值（`-b` 覆盖 > `server.api_prefix`），保持路由表与 blob URL
/// 同源不分裂；exec 无 CLI 覆盖，传 config 派生值。
/// U-18 批 T1.2：`server.route_timeouts` 装配期校验（fail-fast）。
/// 规则：pattern 含 `{`/`}` → 拒（运行期比对真实请求路径，`{param}` 永不出现，允许即静默
/// 无保护）；用 [`serve::path_matches`] 对全部已注册路由 pattern 试匹配，命中 0 条 → 拒并
/// 点名 pattern 与路由数。匹配集 = 各 api 挂载路由表全键（保留路径 /health、/plugins 与 WS
/// 挂载不经 run_route、不消费 route_timeouts，天然不在集合内——只命中它们的配置会被拒）。
fn validate_route_timeouts(patterns: &[String], route_keys: &[String]) -> Result<(), String> {
    for p in patterns {
        if p.contains('{') || p.contains('}') {
            return Err(format!(
                "server.route_timeouts: pattern '{p}' contains a '{{param}}' segment — \
                 route_timeouts matches real request paths, so such a pattern can never fire; \
                 use '*' (one segment) or '**' (multi-segment) instead"
            ));
        }
        let hit = route_keys
            .iter()
            .any(|k| serve::path_matches(std::slice::from_ref(p), k));
        if !hit {
            return Err(format!(
                "server.route_timeouts: pattern '{p}' matches none of {} registered routes — \
                 refusing to start with a timeout rule that can never apply (reserved /health, \
                 /plugins and WS mounts are not covered either)",
                route_keys.len()
            ));
        }
    }
    Ok(())
}

/// MCP/RPC 装配防线（spec 2026-10-30 R7/R9 + v0.1.110 终审）：`enabled` 且未装配
/// auth 插件且未显式 `allow_anonymous` → 拒启。否则端点匿名暴露全路由的机器可读
/// 目录 + 调用面，比业务路由自身的暴露面更糟（机器可枚举、广告化）。两端口径
/// 必须一致：/rpc 与 /mcp 共用执行链，暴露面相同。
fn validate_mcp_gate(gate: &impl GateLike, what: &str, auth_present: bool) -> Result<(), String> {
    if gate.enabled() && !auth_present && !gate.allow_anonymous() {
        return Err(format!(
            "server.{what}.enabled requires an auth plugin (oj-auth) — refusing to expose a \
             machine-readable route catalog anonymously; set server.{what}.allow_anonymous: true \
             to override explicitly"
        ));
    }
    Ok(())
}

/// McpCfg/RpcCfg 的门控公共形状（避免为两条配置各写一份校验）。
trait GateLike {
    fn enabled(&self) -> bool;
    fn allow_anonymous(&self) -> bool;
}
impl GateLike for config::McpCfg {
    fn enabled(&self) -> bool {
        self.enabled
    }
    fn allow_anonymous(&self) -> bool {
        self.allow_anonymous
    }
}
impl GateLike for config::RpcCfg {
    fn enabled(&self) -> bool {
        self.enabled
    }
    fn allow_anonymous(&self) -> bool {
        self.allow_anonymous
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn assemble_backend(
    cfg: &Config,
    top: &serde_json::Value,
    config_dir: &Path,
    // api 树集合（v0.1.58 多挂载）：(目录, ts) 逐树 discover；loader 全局 ts 由
    // 调用方取 primary 树的判定传入。
    trees: &[(PathBuf, bool)],
    base: &str,
    ts: bool,
    profiles: &ResourceProfiles,
    // 热更触发槽（ojm PR-3）：`oj serve` 传入共享槽 Arc（ Extras/StableState 注入
    // 同一 Arc，槽指针构造期冻结、内容后填）；None = 空槽（oj test / exec / 嵌入式
    // 零接线，op 报 reload not available，fail-closed）。
    reload_slot: Option<Arc<only_js::bridge::reload_bridge::ReloadSlot>>,
    // 模块索引（v0.1.85 `--module-root`）：None = 今日布局（源树首层子目录 = 模块根）。
    // 装配期构造一次，注入 StableState 与 Extras——运行期别名、schema 归属、迁移、
    // 路由共用同一份判定。
    module_index: Option<Arc<ModuleIndex>>,
) -> Result<Backend, String> {
    // PR-4：单 isolate 堆限额（u64 copy 提前取出，避免闭包借用 &Config）。
    let js_heap_limit = Some(cfg.server.js_heap_limit_bytes as usize);
    // 入参契约注册表（v0.1.44）：此刻为空，**路由表建成后**填充。
    // 共享 Arc 是刻意的 —— Bridge（make_bridge 闭包）先于路由表定义，写入后
    // 所有 Bridge（含已构造的）一同生效， Injection 顺序不再是约束。
    let input_contracts = Arc::new(std::sync::RwLock::new(InputContractRegistry::new()));
    // 其余非选中 redis key warn 忽略（仅 selected profile 参与装配）。
    let redis_key = profiles.redis.as_deref().unwrap_or("default");
    for (name, url) in cfg.redis.iter().filter(|(n, _)| n.as_str() != redis_key) {
        // 脱敏：redis URL 格式是 `redis://:password@host`，原样打出来（且会镜像进
        // logs/）等于明文泄漏凭据；排障认得 host 就够。
        eprintln!(
            "warn: redis '{name}' ({}) ignored (only redis.{redis_key} is used)",
            only_js::secret::redact(url)
        );
    }
    // loader：project_root 用 config_dir（api 相对 dir；dir 由调用方绝对化）。
    // strip_verbatim 去 Windows `\\?\` 前缀：canonicalize 与 referrer 目录（`to_file_path`
    // 剥前缀）同形，避免 `module_root_of` 词法前缀不一致误判「未找到模块根」。
    let loader = Arc::new(LoaderShared {
        project_root: only_js::bridge::strip_verbatim(
            &config_dir
                .canonicalize()
                .unwrap_or_else(|_| config_dir.to_path_buf()),
        ),
        ts,
    });
    // ext_boot：运行时创建期加载一次（bootstrap 的动态补充）。
    // specifier 在此冻结 `?v=<mtime>`：改文件须重启进程（池常驻，不做热重载）。
    let boot = ext_boot_spec(config_dir)?;
    // 插件装配（spec §5）：解析 plugins_dir → 清单严格/缺省扫描 → 校验 → 注册。
    // 自描述清单（PluginInfo）同时喂 Extras/StableState.plugins（JS 内省）与
    // `GET {base}/plugins`（AppState）。
    let mut registries = Registries::default();
    let (plugin_infos, loaded) = {
        let (infos, loaded) = assemble_plugins(
            cfg,
            top,
            config_dir,
            &mut registries,
            profiles.es.as_deref(),
        )
        .await
        .map_err(|e| format!("plugins: {e}"))?;
        (std::sync::Arc::new(infos), loaded)
    };
    // ojInfo() 数据源（与 `oj info` CLI 同源，`serve_cmd::assemble_ojinfo` 单一事实源）：
    // to_value 固化后注入 StableState/Extras——config 只含段名/键名（值一律不出），
    // 注入即公开。registries 尚未被消费（es/kv/generic 等槽位在下方逐次取出）。
    let oj_info: Arc<serde_json::Value> = Arc::new(
        serde_json::to_value(crate::serve_cmd::assemble_ojinfo(
            cfg,
            top,
            &loaded,
            &registries,
            None,
        ))
        .map_err(|e| format!("ojinfo: {e}"))?,
    );
    // KV：redis.<key> 存在 → 经 kv 插件 vtable connect（单例 fail-fast）；
    // 未声明 → InMemoryKV 内置兜底。
    let kv: Arc<dyn KVStore> = connect_kv(cfg, &registries, redis_key).await?;
    let es: Option<Arc<dyn EsBackend>> = registries.es;
    // 泛型轴注册表（插件自报的非类型化轴 → bootstrap `axis(name)`）：装配期冻结，
    // 与 make_bridge 的 Extras.generic_axes / StableState.generic_axes 同源。
    let generic_axes =
        only_js::bridge::generic_axis::registry_from(std::mem::take(&mut registries.generic));
    // U-15：管理面收编轴（`mgmt:` 段 `axes` 名单）——**不进** `axis()` 公开面，仅供 mgmt 面。
    let mgmt_axes =
        only_js::bridge::generic_axis::registry_from(std::mem::take(&mut registries.mgmt_generic));
    let mgmt_section = Arc::new(cfg.mgmt.clone());
    // 管理面登记表（进程级）：装配层构造一次，热更前后同一 Arc（与 tx_sessions 同款）。
    let mgmt = Arc::new(only_js::bridge::mgmt::MgmtRegistry::new(
        mgmt_section.max_entries,
        mgmt_section.max_per_owner,
    ));
    if mgmt_section.enabled {
        if mgmt_section.max_entries == 0 || mgmt_section.max_per_owner == 0 {
            return Err(
                "mgmt.enabled=true requires mgmt.max_entries >= 1 and mgmt.max_per_owner >= 1"
                    .to_string(),
            );
        }
        if mgmt_section.allow.is_unrestricted() {
            return Err(
                "mgmt.enabled=true requires mgmt.allow to declare at least one of \
                 modules/roles/tenants/sources (refusing an unrestricted management surface)"
                    .to_string(),
            );
        }
        if mgmt_section.axes.is_empty() {
            eprintln!(
                "warn: mgmt is enabled but mgmt.axes is empty -- no connector can register \
                 (add the generic axis names to claim them off the public axis() surface)"
            );
        }
    } else if !mgmt_section.axes.is_empty() {
        // 评审 R.1-P8：disabled 却收编轴 = 公开面静默少一个轴（违背 fail-loud）。
        eprintln!(
            "warn: mgmt is disabled but mgmt.axes is non-empty ({:?}) -- these axes are \
             claimed off the public axis() surface and unreachable; remove mgmt.axes or \
             set mgmt.enabled: true",
            mgmt_section.axes
        );
    }
    // blob：blob 段存在即启用；未声明 → None。`--blob` 选中的 profile 别名为 "default"。
    let blobs: Option<Arc<BlobRegistry>> = match &cfg.blob {
        None => None,
        Some(section) => Some(
            assemble_blobs(
                section,
                config_dir,
                base,
                registries.blob,
                profiles.blob.as_deref(),
            )
            .await?,
        ),
    };
    // fs：fs 段存在即启用（jail 到 root；root 相对 config_dir 解析）；root 不存在/
    // 非目录在 FsGrant::new fail-fast。未声明 → fs.* 报 NotCapable（fail-closed）。
    let fs_grant: Option<Arc<only_js::bridge::fs::FsGrant>> = match &cfg.fs {
        None => None,
        Some(section) => {
            let root = static_dir(config_dir, &section.root)?;
            Some(Arc::new(only_js::bridge::fs::FsGrant::new(
                &root,
                section.readonly,
            )?))
        }
    };
    // 逐 db 开库（未知 scheme 注册表 fail-fast）。
    let dbs = connect_dbs(&cfg.db, &registries.dbs, config_dir).await?; // 默认库重定向（v0.1.20）：`oj test` 走 db.test。未声明的库名 fail-fast——
    // 静默回落 default 等于把测试写在开发库上（正是本项要修的事故面）。
    if let Some(o) = &profiles.db
        && !dbs.contains_key(o)
    {
        let mut names: Vec<&str> = dbs.keys().map(|s| s.as_str()).collect();
        names.sort_unstable();
        return Err(format!(
            "--db {o:?} not declared in config (db keys: {names:?})"
        ));
    }
    if let Some(o) = &profiles.db {
        eprintln!("oj: default db redirected to {o:?} (migrate/seed/fixtures follow)");
    }
    // LIMIT 配置（db_query 段）：装配期校验（倒置区间 / 0 / 超硬顶 均 fail-fast）。
    let query_limits = cfg.db_query;
    query_limits.validate()?;
    // 迁移 gate 的执行留在 HTTP 层（from_config gate match）；此处仅做 schema 归属图。
    // 表归属守卫模式（§5.3）：warn（默认）| deny（违规拒绝）；非法值 fail-fast。
    let ownership_deny = ownership_deny_of(cfg)?;
    let sql_guard = sql_guard_of(cfg);
    // §4.8 归属图 + SchemaRegistry 复活。
    let (registry, modules) = build_schema_and_modules(
        trees,
        sql_guard,
        &cfg.tenant.shared_allow,
        module_index.as_deref(),
    )
    .await?;
    // platform 段装配（ojm 批次）：受控挂载根校验 + 抢注防护（特权名 ∩ 安装树
    // 模块名，豁免受控根产物 → fail-fast）+ grants 冻结。必须在
    // build_schema_and_modules 之后——安装树模块名集合取自其产物。
    let installed: std::collections::HashSet<String> =
        modules.values().map(|m| m.name.clone()).collect();
    let (platform_grant, exec_grant) = build_platform_grants(
        cfg,
        config_dir,
        &fs_grant,
        trees,
        &installed,
        module_index.as_deref(),
    )?;
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
    let (jwt, oidc) = build_jwt_and_oidc(cfg, config_dir)?;
    let sso = build_sso(cfg); // v0.1.61：http_sso 段（'static 闭包外先算好）。
    let sso_for_closure = sso.clone(); // move 闭包（:1104）捕获自有副本。
    // 共享事件总线：按 `--broker` 选中的 profile（`config.broker` 命名 map）连接；
    // 段为空 → 进程内 Bus；给定 profile 不存在 → fail-fast。
    let bus = {
        let key = profiles.broker.as_deref().unwrap_or("default");
        let cfg_broker = if cfg.broker.is_empty() {
            None
        } else {
            Some(
                cfg.broker
                    .get(key)
                    .ok_or_else(|| {
                        let mut names: Vec<&str> = cfg.broker.keys().map(|s| s.as_str()).collect();
                        names.sort_unstable();
                        format!(
                            "--broker profile '{key}' not declared (available broker profiles: {names:?})"
                        )
                    })?
                    .to_owned(),
            )
        };
        registries
            .bus
            .connect(&cfg_broker)
            .await
            .map_err(|e| format!("broker: {e}"))?
    };
    // mail 后端（spec 2026-09-15）：顶层 smtp: 段 + oj-mail 插件 vtable。
    // 必须在 bus 之后——结果上送（`mail.result`）的扇出目标是同一总线实例。
    let mail = build_mail_backend(cfg, top, registries.mail, bus.clone())?;
    // exec/test 手工 runtime 不经 Bridge 构造（install 原只在 Bridge 构造期触发）——
    // 装配层显式装一次；幂等（覆写同一进程级弱引用，见 mail::install_mail_deliver）。
    if let Some(m) = &mail {
        only_js::bridge::mail::install_mail_deliver(m);
    }
    // ldap 后端：ldap: 段 + oj-ldap 插件 vtable（独立能力，无跨后端依赖）。
    let ldap = build_ldap_backend(cfg, top, registries.ldap)?;
    // mq 命名实例（spec §3/§7）：kafkas:/rabbits: 段 → 插件 connect → 命名 registry。
    let (kafkas, rabbits) = build_mq_registries(
        cfg,
        &registries.mq,
        profiles.kafka.as_deref(),
        profiles.rabbit.as_deref(),
    )
    .await?;
    // 部署期常量（config `vars:` 段，v0.1.25）：装配期冻结成只读 Arc（`vars.get` 唯一
    // 数据源），与 make_bridge 的 Extras.vars 同源。
    let vars = Arc::new(cfg.vars.clone());
    // db 访问控制策略表（`db_access:` 段，v0.1.89）：装配期校验（越界/矛盾 fail-loud）
    // 后冻结成只读 Arc，与 make_bridge 的 Extras.db_access 同源。
    let db_access = Arc::new(cfg.db_access.clone());
    db_access.defaults.validate("defaults")?;
    // 逐库条目是**覆盖**而非整条策略：校验的是合并后的生效策略（否则
    // `read_only` + `allow_ddl` 的矛盾可能只在合并后才浮现，装配期漏检）。
    for (alias, ov) in &db_access.by_db {
        ov.merge(&db_access.defaults).validate(alias)?;
    }
    // 动态数据源登记配置段（`db_client:`，L3 v0.1.91）：装配期 fail-loud 校验。
    // 安全红线：enabled=true 但 allow 四维全缺（is_unrestricted）→ 拒启动，防「开了忘配」全开（SSRF 面）。
    let db_client = Arc::new(cfg.db_client.clone());
    if db_client.enabled {
        if db_client.max_entries == 0 {
            return Err("db_client.enabled=true requires db_client.max_entries >= 1".to_string());
        }
        // U-7（v0.1.97）：0 = 不设限（DoS 面）；per-owner 上限大于全局上限是配置矛盾。
        if db_client.max_per_owner == 0 {
            return Err("db_client.enabled=true requires db_client.max_per_owner >= 1".to_string());
        }
        if db_client.max_streams == 0 {
            return Err("db_client.enabled=true requires db_client.max_streams >= 1".to_string());
        }
        if db_client.max_per_owner > db_client.max_entries {
            return Err(format!(
                "db_client.max_per_owner ({}) > db_client.max_entries ({}) is contradictory",
                db_client.max_per_owner, db_client.max_entries
            ));
        }
        if db_client.allow.is_unrestricted() {
            return Err(
                "db_client.enabled=true requires db_client.allow to declare at least one of \
                 modules/roles/tenants/sources (refusing to start with an open-but-unauthorized \
                 client registration surface)"
                    .to_string(),
            );
        }
    }
    // 跨请求事务会话（`db_session:`，L7 v0.1.95）：装配期 fail-loud 校验。
    // 与 db_client 同哲学——能力默认关，开了就必须配清楚（长事务占连接 + 放大脏读窗口）。
    let db_session = Arc::new(cfg.db_session.clone());
    if db_session.enabled {
        if db_session.allow.is_unrestricted() {
            return Err(
                "db_session.enabled=true requires db_session.allow to declare at least one of \
                 modules/roles/tenants/sources (refusing to open an unauthorized long-transaction \
                 surface)"
                    .to_string(),
            );
        }
        if db_session.max_sessions == 0 {
            return Err(
                "db_session.enabled=true requires db_session.max_sessions >= 1".to_string(),
            );
        }
        if db_session.max_per_owner == 0 {
            return Err(
                "db_session.enabled=true requires db_session.max_per_owner >= 1".to_string(),
            );
        }
        if db_session.max_per_owner > db_session.max_sessions {
            return Err(format!(
                "db_session.max_per_owner ({}) must not exceed db_session.max_sessions ({})",
                db_session.max_per_owner, db_session.max_sessions
            ));
        }
        if db_session.idle_timeout_secs == 0 {
            return Err(
                "db_session.enabled=true requires db_session.idle_timeout_secs >= 1".to_string(),
            );
        }
        if db_session.max_lifetime_secs < db_session.idle_timeout_secs {
            return Err(format!(
                "db_session.max_lifetime_secs ({}) must be >= db_session.idle_timeout_secs ({})",
                db_session.max_lifetime_secs, db_session.idle_timeout_secs
            ));
        }
        // sqlite 的进程内池刻意限 1 条连接（`:memory:` 与写锁语义使然）：一个存活会话会
        // 独占唯一连接，其余请求只能等 acquire 超时。生产用 mysql/postgres，这里只告警。
        for (alias, dsn) in &cfg.db {
            if dsn.starts_with("sqlite") {
                eprintln!(
                    "warn: db_session is enabled but db alias '{alias}' is sqlite \
                     (single pooled connection) — an open session will block other queries \
                     on this alias until it is committed/rolled back"
                );
            }
        }
    }
    // 安全审计（`audit:`，U-9 v0.1.98）：缺省开（有界环形、零凭据）；0 值拒启；
    // 读面不限制只 warn（新段不 brick 既有部署，但生产应收紧）。
    let audit = Arc::new(cfg.audit.clone());
    if audit.enabled {
        if audit.max_entries == 0 {
            return Err("audit.enabled=true requires audit.max_entries >= 1".to_string());
        }
        if audit.retention_secs == 0 {
            return Err("audit.enabled=true requires audit.retention_secs >= 1".to_string());
        }
        if audit.allow.is_unrestricted() {
            eprintln!(
                "warn: audit is enabled but audit.allow is unrestricted — any caller may query \
                 audit events via audit.query (events are in-process and credential-free); \
                 tighten audit.allow in production"
            );
        }
    }
    let audit_store = Arc::new(only_js::bridge::audit::AuditStore::new(&audit));
    // 会话注册表（进程级）：装配层构造一次，Extras 与 StableState 共享同一 Arc——
    // 热更只 clone StableState，故已开会话跨热更存活（跨请求语义的应有结果）。
    let tx_sessions = Arc::new(only_js::bridge::db_session::TxSessionRegistry::new(
        (*db_session).clone(),
        audit_store.clone(),
    ));
    // 执行登记表（U-5 v0.1.96）：同 tx_sessions——装配层构造一次，热更前后同一 Arc
    // （存活流的可取消性跨热更有效）。
    let exec_sessions = Arc::new(only_js::bridge::db_exec::ExecRegistry::default());
    // 任务式导出（U-18 v0.1.104）：`db_export:` 段装配期校验（fail-loud 点名键），
    // 注册表同 tx_sessions——热更前后同一 Arc（在跑导出任务跨热更存活）。
    cfg.db_export.validate()?;
    let export_section = Arc::new(cfg.db_export.clone());
    let export_registry = Arc::new(only_js::bridge::db_export::ExportRegistry::new(
        &cfg.db_export,
    ));
    // 任务式批量导入（REQ-12 v0.1.115）：`db_import:` 段装配期校验（fail-loud 点名键），
    // 注册表同 tx_sessions——热更前后同一 Arc（在跑导入任务跨热更存活）。
    cfg.db_import.validate()?;
    let import_section = Arc::new(cfg.db_import.clone());
    let import_registry = Arc::new(only_js::bridge::db_import::ImportRegistry::new(
        &cfg.db_import,
    ));
    // 运行期连接工厂（与装配期同集：含插件 mysql/postgres）；config 目录作为 sqlite 相对路径基准。
    let db_backend = Arc::new(registries.dbs.clone());
    let config_dir_arc = Arc::new(config_dir.to_path_buf());
    // 运行期凭据密钥（U-10 v0.1.101，`secret.seal/open`）：公钥 = secrets.public_key_path
    // > 私钥推导（部署机为解封 ENC[...] 本就持有私钥）；皆缺 = None（op 层 fail-closed）。
    // 显式 public_key_path 配错 → 装配期 fail-loud（配了就要对）。
    let secret_keys = only_js::secret::load_runtime_keys(
        cfg.secrets.public_key_path.as_deref(),
        cfg.secrets.private_key_path.as_deref(),
        config_dir,
    )
    .map_err(|e| format!("secrets: {e}"))?
    .map(Arc::new);
    // 运行期动态登记注册表（per-process 硬上限 = max_entries；U-7：per-owner 与流预算同源配置）。
    let client_dbs = Arc::new(only_js::bridge::ClientDbRegistry::with_caps(
        db_client.max_entries,
        db_client.max_per_owner,
        db_client.max_streams,
    ));
    // 运行期动态授权表（U-1 v0.1.93）：壳 Arc 不可变、内部 Mutex 变动，起始空。
    // 仅作用于动态登记别名（见 db_policy::policy_for），静态 config 兜底不受影响。
    let db_access_rt: Arc<
        std::sync::Mutex<std::collections::HashMap<String, only_js::bridge::db_policy::DbAccess>>,
    > = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    // db 默认库重定向需进入 'static 工厂闭包，先取出 owned（profiles 是借用，不能捕获）。
    let db_override = profiles.db.clone();
    // 单一工厂（内省 / actor 池 / WS 连接共享同一 Bus 与 Extras）——闭包捕获全 Arc，
    // Clone 即共享。tasks_flag 参数化：None = HTTP 桥，Some = 任务桥。
    // cfg 只以 Copy 字段（allow_as_tenant）进入闭包：引用不得活进 'static 工厂，
    // 故先解出值再捕获（stable 单源之前 from_config 持 cfg 的等价写法）。
    let allow_as_tenant = cfg.tenant.allow_as_tenant;
    // SQL 执行追踪（v0.1.51）：dev 默认开、release 默认关，config 段可强制覆盖。
    let sql_trace = only_js::bridge::sql_trace::SqlTraceConfig::resolve(&cfg.db_trace, ts);
    // 热更触发槽（ojm PR-3）：None → 空槽（oj test / exec / 嵌入式形态，op 报
    // reload not available，fail-closed）。解析成 owned Arc——工厂闭包 Extras 与
    // StableState 注入同一 Arc，槽指针构造期冻结、内容由装配层后填。
    let reload_slot = reload_slot.unwrap_or_else(|| Arc::new(std::sync::OnceLock::new()));
    let make_bridge_of = {
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
        let mail = mail.clone();
        let ldap = ldap.clone();
        // db_override 已是 owned（上方从 profiles.db 克隆），闭包内直接复用。
        // 影子绑定：`move` 捕获的是这里的副本（外层 vars 仍供后续 StableState 使用）。
        let vars = vars.clone();
        // 影子绑定：同 vars —— 闭包带走的是这里的副本（外层 db_access 仍供 StableState 使用）。
        let db_access = db_access.clone();
        // 影子绑定：同 db_access —— 闭包带走的是这里的副本（外层 db_client/client_dbs/
        // db_backend/config_dir 仍供下方直接 StableState 使用）。
        let db_client = db_client.clone();
        // 影子绑定：同 db_client —— 会话配置与注册表也须与 StableState 同一 Arc。
        let db_session = db_session.clone();
        let tx_sessions = tx_sessions.clone();
        let export_section = export_section.clone();
        let export_registry = export_registry.clone();
        let import_section = import_section.clone();
        let import_registry = import_registry.clone();
        let audit = audit.clone();
        let audit_store = audit_store.clone();
        let exec_sessions = exec_sessions.clone();
        let mgmt_section = mgmt_section.clone();
        let mgmt_axes = mgmt_axes.clone();
        let secret_keys = secret_keys.clone();
        let mgmt = mgmt.clone();
        let client_dbs = client_dbs.clone();
        let db_access_rt = db_access_rt.clone();
        let db_backend = db_backend.clone();
        let config_dir_arc = config_dir_arc.clone();
        // 影子绑定：同 vars/js_heap_limit —— 闭包带走的是这里的副本。
        let input_contracts = input_contracts.clone();
        let fs_grant = fs_grant.clone();
        let generic_axes = generic_axes.clone();
        // platform/exec grants（ojm 批次；下方 build_platform_grants 产物，同一 Arc）。
        let platform_grant = platform_grant.clone();
        let exec_grant = exec_grant.clone();
        // 模块索引（v0.1.85）：闭包带走副本，与 StableState.module_index 同一 Arc。
        let module_index = module_index.clone();
        // 热更触发槽（ojm PR-3）：闭包带走副本，与 StableState.reload 同一 Arc。
        let reload_slot = reload_slot.clone();
        // 影子绑定：同 vars —— 闭包带走的是这里的副本（外层 oj_info 仍供 StableState 使用）。
        let oj_info = oj_info.clone();
        move |tasks_flag: Option<Arc<std::sync::atomic::AtomicBool>>| {
            Bridge::with_dbs_and_loader(
                dbs.clone(),
                kv.clone(),
                registry.clone(),
                false,
                Some(loader.clone()),
                Extras {
                    module_index: module_index.clone(),
                    blobs: blobs.clone(),
                    es: es.clone(),
                    bus: Some(bus.clone()),
                    plugins: plugins.clone(),
                    modules: modules.clone(),
                    ownership_deny,
                    sql_guard,
                    allow_as_tenant,
                    db_override: db_override.clone(),
                    query_limits,
                    sql_trace,
                    boot: boot.clone(),
                    // jwt 原语配置（auth 解耦：JS 端点 jwt.sign/verify 数据源）。
                    jwt: jwt.clone(),
                    // oidc 原语配置（OIDC 解耦：JS 端点 oidc.sign/verify/jwks 数据源）。
                    oidc: oidc.clone(),
                    // http_sso 段（v0.1.61：JS 登录端点 sso.providers 数据源）。
                    sso: sso_for_closure.clone(),
                    // mq 命名实例（T7 装配注入；此处空表兜底，编译占位）。
                    kafkas: Some(kafkas.clone()),
                    rabbits: Some(rabbits.clone()),
                    tasks_flag,
                    // mail 后端（smtp: 段 + oj-mail 插件；未配置/未加载 = None）。
                    mail: mail.clone(),
                    // ldap 后端（ldap: 段 + oj-ldap 插件；未配置/未加载 = None）。
                    ldap: ldap.clone(),
                    db_access: Some(db_access.clone()),
                    db_client: Some(db_client.clone()),
                    db_session: Some(db_session.clone()),
                    tx_sessions: Some(tx_sessions.clone()),
                    audit_section: Some(audit.clone()),
                    audit: Some(audit_store.clone()),
                    exec_sessions: Some(exec_sessions.clone()),
                    export_section: Some(export_section.clone()),
                    export_registry: Some(export_registry.clone()),
                    import_section: Some(import_section.clone()),
                    import_registry: Some(import_registry.clone()),
                    mgmt_section: Some(mgmt_section.clone()),
                    mgmt_axes: Some(mgmt_axes.clone()),
                    mgmt: Some(mgmt.clone()),
                    // 运行期凭据密钥（U-10）：与 StableState.secret_keys 同源。
                    secret_keys: secret_keys.clone(),
                    client_dbs: Some(client_dbs.clone()),
                    db_access_rt: Some(db_access_rt.clone()),
                    db_backend: Some(db_backend.clone()),
                    config_dir: Some(config_dir_arc.clone()),
                    vars: vars.clone(),
                    js_heap_limit,
                    input_contracts: Some(input_contracts.clone()),
                    // fs 授权（v0.1.53；config fs: 段）：与 StableState.fs 同一 Arc。
                    fs: fs_grant.clone(),
                    // platform/exec 授权（ojm 批次；config platform: 段）：与 StableState 同一 Arc。
                    platform: platform_grant.clone(),
                    exec: exec_grant.clone(),
                    // 热更触发槽（ojm PR-3）：与 StableState.reload 同一 Arc（槽位后填）。
                    reload: reload_slot.clone(),
                    // 泛型轴注册表：与 StableState.generic_axes 同一 Arc。
                    generic_axes: Some(generic_axes.clone()),
                    // ojInfo() 数据源：与 StableState.oj_info 同一 Arc（`oj info` CLI 同源）。
                    oj_info: Some(oj_info.clone()),
                },
            )
        }
    };
    // 共享 StableState：与 actor 工厂同一组后端 Arc（StableState 唯一构造点——
    // 原 from_config 尾部的组装段迁入，「单一 StableState」不变量由此得到唯一载体）。
    let stable = Arc::new(StableState {
        kv: kv.clone(),
        seq_ensured: std::sync::Mutex::new(std::collections::HashSet::new()),
        dbs: dbs.clone(),
        registry: Arc::new(registry),
        loader: Some(loader.clone()),
        module_index: module_index.clone(),
        blobs: blobs
            .clone()
            .unwrap_or_else(|| Arc::new(BlobRegistry::new())),
        bus: bus.clone(),
        es: es.clone(),
        plugins: (*plugin_infos).clone(),
        modules,
        ownership_deny,
        sql_guard,
        allow_as_tenant: cfg.tenant.allow_as_tenant,
        db_override: profiles.db.clone(),
        query_limits,
        sql_trace,
        boot: boot.clone(),
        jwt: jwt.clone(),         // 与 make_bridge 的 Extras.jwt 同源。
        oidc: oidc.clone(),       // 与 make_bridge 的 Extras.oidc 同源。
        sso: sso.clone(),         // 与 make_bridge 的 Extras.sso 同源。
        kafkas: kafkas.clone(),   // 与 make_bridge 的 Extras.kafkas 同源。
        rabbits: rabbits.clone(), // 与 make_bridge 的 Extras.rabbits 同源。
        tasks_flag: None,
        sql_memo: std::sync::Mutex::new(std::collections::HashMap::new()),
        mail: mail.clone(), // 与 make_bridge 的 Extras.mail 同源（同一 Arc）。
        ldap: ldap.clone(), // 与 make_bridge 的 Extras.ldap 同源（同一 Arc）。
        vars: vars.clone(), // 与 make_bridge 的 Extras.vars 同源（同一 Arc）。
        db_access: db_access.clone(), // 与 make_bridge 的 Extras.db_access 同源（同一 Arc）。
        db_client: db_client.clone(), // 与 make_bridge 的 Extras.db_client 同源（同一 Arc）。
        db_session: db_session.clone(), // 与 make_bridge 的 Extras.db_session 同源（同一 Arc）。
        client_dbs: client_dbs.clone(), // 与 make_bridge 的 Extras.client_dbs 同源（同一 Arc）。
        tx_sessions: tx_sessions.clone(), // 与 make_bridge 的 Extras.tx_sessions 同一注册表。
        audit_section: audit.clone(), // 与 make_bridge 的 Extras.audit_section 同一 Arc。
        audit: audit_store.clone(), // 与 make_bridge 的 Extras.audit 同一存储。
        exec_sessions: exec_sessions.clone(), // 与 make_bridge 的 Extras.exec_sessions 同一注册表。
        export_section: export_section.clone(), // 与 make_bridge 的 Extras.export_section 同一 Arc。
        export_registry: export_registry.clone(), // 与 make_bridge 的 Extras.export_registry 同一注册表。
        import_section: import_section.clone(), // 与 make_bridge 的 Extras.import_section 同一 Arc。
        import_registry: import_registry.clone(), // 与 make_bridge 的 Extras.import_registry 同一注册表。
        mgmt_section: mgmt_section.clone(), // 与 make_bridge 的 Extras.mgmt_section 同一 Arc。
        mgmt_axes: mgmt_axes.clone(),       // 与 make_bridge 的 Extras.mgmt_axes 同一注册表。
        mgmt: mgmt.clone(),                 // 与 make_bridge 的 Extras.mgmt 同一登记表。
        secret_keys: secret_keys.clone(),   // 与 make_bridge 的 Extras.secret_keys 同源。
        db_access_rt: db_access_rt.clone(), // 与 make_bridge 的 Extras.db_access_rt 同源（同一 Arc）。
        db_backend: db_backend.clone(), // 与 make_bridge 的 Extras.db_backend 同源（同一 Arc）。
        config_dir: config_dir_arc.clone(), // 与 make_bridge 的 Extras.config_dir 同源（同一 Arc）。
        input_contracts: Some(input_contracts.clone()),
        js_heap_limit,
        fs: fs_grant.clone(), // 与 make_bridge 的 Extras.fs 同一 Arc。
        platform: platform_grant.clone(), // 与 make_bridge 的 Extras.platform 同一 Arc。
        exec: exec_grant.clone(), // 与 make_bridge 的 Extras.exec 同一 Arc。
        // 热更触发槽（ojm PR-3）：与 make_bridge 的 Extras.reload 同一 Arc
        // （槽指针构造期冻结；oj serve 装配层在 ReloadState 就绪后 set）。
        reload: reload_slot,
        generic_axes, // 与 make_bridge 的 Extras.generic_axes 同一 Arc。
        oj_info,      // 与 make_bridge 的 Extras.oj_info 同一 Arc（ojInfo() 数据源）。
    });
    Ok(Backend {
        stable,
        auth_guard: auth,
        make_bridge_of: Arc::new(make_bridge_of),
        input_contracts,
    })
}

impl App {
    /// 装配并构造（原 `start` 逻辑搬入）。唯一构造一处 `StableState`，同时被 actor 工厂与
    /// 测试运行时引用，保证 db/bus/kv 跨家族为同一组 Arc（修正 #2）。
    ///
    /// 23 步的顺序即语义（见 [docs/modules/04-oj-cli.md]）；其中自成一体的步骤已抽为
    /// 私有函数（connect_kv / ownership_deny_of / build_schema_and_modules /
    /// build_jwt_and_oidc / resolve_static_sites / load_cert_with_watcher）。
    #[allow(clippy::too_many_lines)]
    #[allow(clippy::too_many_arguments)]
    pub async fn from_config(
        cfg: Config,
        top: &serde_json::Value,
        config_dir: &Path,
        fixtures: bool,
        // profiles：各资源根 key 的默认 profile 选择（对应 `oj test`/`oj exec` 的
        // `--db`/`--redis`/`--blob`/`--es`/`--broker`/`--kafka`/`--rabbit`）。字面 "default"
        // 的调用改指向所选 profile；迁移 / seed / fixtures / schema 内省一并跟随。
        profiles: &ResourceProfiles,
        // 热更复用：Some(旧 Backend) → 跳过 `assemble_backend`（不重建任何后端连接池，
        // 不二次 init 插件），仅重建路由层 + 任务池 + JS 模块树；None = 现状零变化。
        reuse_backend: Option<Arc<Backend>>,
        // 热更触发槽（ojm PR-3）：`oj serve` 传入共享槽 Arc，穿参至 `assemble_backend`
        // → Extras/StableState（同一 Arc，槽位后填）。热更复用路径不重建 Extras——
        // 复用的 Backend 已持有同一槽 Arc，槽内容跨代持续有效；形参仅为签名统一。
        reload_slot: Option<Arc<only_js::bridge::reload_bridge::ReloadSlot>>,
        // 模块索引（v0.1.85 `--module-root`）：None = 今日布局（源树首层子目录 = 模块根）。
        // 装配期构造一次并注入 StableState / Extras —— 运行期别名、schema 归属、迁移、
        // seed、路由共用同一份「哪个目录是模块」的判定。
        module_index: Option<Arc<ModuleIndex>>,
    ) -> Result<App, String> {
        // 挂载解析（v0.1.58）：结构校验（程序化构造的 cfg 不经 load 层，这里兜底）→
        // 归一 / canonicalize / 逐条目 dev-release（规则 4）。primary = 第一条 api 挂载
        // （base/loader 全局 ts 取自它；迁移/seed/fixtures 逐 api 树循环）；纯静态时
        // api 功能未启用——占位路径（模块扫描 NotFound = 空 = 无路由）。
        config::validate_mounts(&cfg.mounts)?;
        let resolved = resolve_mounts(&cfg, config_dir)?;
        let primary = resolved
            .iter()
            .find_map(|m| m.kind.api().map(|(d, t)| (d.to_path_buf(), t)));
        let (_dir, ts, base) = match primary {
            Some((d, t)) => (
                d,
                t,
                resolved
                    .iter()
                    .find(|m| m.kind.api().is_some())
                    .map(|m| m.prefix.clone())
                    .unwrap_or_default(),
            ),
            None => (
                config_dir.join(".oj-static-only"),
                false,
                crate::serve_cmd::DEFAULT_API_PREFIX.to_string(),
            ),
        };
        let trees: Vec<(PathBuf, bool)> = resolved
            .iter()
            .filter_map(|m| m.kind.api().map(|(d, t)| (d.to_path_buf(), t)))
            .collect();
        // 后端装配（spec §2.2 归属表）：StableState 唯一构造点，HTTP 步全在下方。
        // 热更复用：Some(旧 Backend) → 跳过 assemble_backend（不重建连接池、不二次 init 插件）。
        let reused = reuse_backend.is_some();
        let backend = match reuse_backend {
            Some(b) => b,
            None => Arc::new(
                assemble_backend(
                    &cfg,
                    top,
                    config_dir,
                    &trees,
                    &base,
                    ts,
                    profiles,
                    reload_slot,
                    module_index.clone(),
                )
                .await?,
            ),
        };
        let stable = backend.stable().clone();
        // 停机 flag（spec §6）：App 持有，信号处理器置位；任务 Bridge 的 Extras 注
        // Some(flag)（HTTP actor 桥注 None——消费会话归属评审 M2）。HTTP 层创建。
        let tasks_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let make_bridge = {
            let backend = backend.clone();
            move || backend.make_bridge()()
        };
        let make_task_bridge = backend.make_task_bridge(tasks_flag.clone());
        let auth = backend.auth_guard().cloned();
        // 证书必配门禁（无逃生口）：两个证书路径必须都配齐才启动，否则 fail-fast。
        // 证书校验不可被 config 或 CLI 关闭——任何绕过都会违背证书强制校验的初衷。
        if !cfg.server.cert_paths_configured() {
            return Err("certificate is mandatory but not configured \
                 (set server.public_key_path + server.certificate_path; \
                 no config or flag can skip certificate validation)"
                .to_string());
        }
        // 匿名路径条目校验（v0.1.23）：`one_layer` 只对尾 "/*" 条目有意义（fail-fast）。
        config::validate_anon_paths(&cfg)?;
        config::validate_server_limits(&cfg)?;
        // 迁移门禁（§4.6，先于 seed）：dev 默认 auto（apply），release 默认 verify
        // （M003/M004 校验，账本落后拒启）；`migrate_on_start: off` 为逃生门。
        // 热更路径强制 verify-only（dev 默认 auto-apply 会打生产库，§5 ②）。
        let gate = if reused {
            "verify"
        } else {
            migrate_gate_of(&cfg, ts)
        };
        let db_key: &str = profiles.db.as_deref().unwrap_or("default");
        match gate {
            "auto" => {
                // v0.1.58 多挂载：逐 api 树 apply + reconcile（同一目标库；表名冲突由
                // build_schema_and_modules 的 S002 先行拦截，走不到这里）。
                for (tdir, tts) in &trees {
                    crate::migrate::apply_all(
                        stable.dbs.get(db_key),
                        tdir,
                        *tts,
                        false,
                        module_index.as_deref(),
                    )
                    .await?;
                    for l in crate::schema::reconcile_all(
                        stable.dbs.get(db_key).map(|a| a.as_ref()),
                        tdir,
                        *tts,
                        db_key,
                        module_index.as_deref(),
                    )
                    .await?
                    {
                        eprintln!("schema: {l}");
                    }
                }
            }
            "verify" => {
                for (tdir, tts) in &trees {
                    crate::migrate::verify_all(
                        stable.dbs.get(db_key),
                        tdir,
                        *tts,
                        module_index.as_deref(),
                    )
                    .await?;
                }
            }
            "off" => {}
            other => {
                return Err(format!(
                    "server.migrate_on_start: illegal value {other:?} (auto|verify|off)"
                ));
            }
        }
        // 种子重放（P0）：各 api 树的模块 seed.sql（§8-1）。热更复用时不重放
        // （种子是一次性初始化，重载不应再灌库，§5 ②）。
        if !reused {
            for (tdir, _) in &trees {
                crate::seed::replay_all(stable.dbs.get(db_key), tdir, module_index.as_deref())
                    .await?;
            }
        }
        // fixtures/ 演示数据（§4.5）：仅 oj test（fixtures=true）灌入；server 不灌。
        if fixtures {
            for (tdir, tts) in &trees {
                let modules = crate::manifest::discover(tdir, *tts, module_index.as_deref())?;
                crate::migrate_cmd::load_fixtures(stable.dbs.get(db_key), &modules).await?;
            }
        }
        // ext_boot 预热：建 runtime 并跑完 boot，失败即 `Err`（真·启动失败）。
        // 必须前移到建表之前 —— 否则 boot 错误只能借 dev 内省的间接失败暴露，而
        // `bridge_introspector` 会把线程 panic 吞成路由 failure（装配层只 warn 不致命，
        // 结果「路由全空、服务照常监听」）。
        if stable.boot.is_some() {
            prewarm_boot(make_bridge.clone())?;
        }
        // 路由表：逐 api 挂载构建（v0.1.58 多挂载）。dev 启动内省 .route 声明；
        // release 聚合该树 manifests.yaml（pattern = /{挂载prefix}/{e.pattern}）。
        let mut tables: Vec<(String, routes::RouteTable)> = Vec::new();
        let mut failures: Vec<String> = Vec::new();
        for m in resolved.iter().filter(|m| m.kind.api().is_some()) {
            let (prefix, mdir, mts) = match m.kind.api() {
                Some((d, t)) => (m.prefix.as_str(), d, t),
                None => unreachable!("filtered above"),
            };
            let (table, mount_failures) = if mts {
                for md in
                    manifest::load_modules(mdir, Some(&cfg.tasks.dir), module_index.as_deref())?
                {
                    eprintln!("module {} v{} — {}", md.name, md.version, md.desc);
                }
                routes::RouteTable::build(
                    prefix,
                    mdir,
                    mts,
                    module_index.as_deref(),
                    routes::bridge_introspector(make_bridge.clone()),
                )
            } else {
                let lock = manifest::load_lock(&mdir.join("manifests.yaml")).map_err(|e| {
                    format!(
                        "release mode: {}: {e}",
                        mdir.join("manifests.yaml").display()
                    )
                })?;
                if lock.is_empty() {
                    return Err(format!(
                        "release mode: {} missing or empty — run `oj build` first",
                        mdir.join("manifests.yaml").display()
                    ));
                }
                let reader = routes::bridge_default_reader(make_bridge.clone());
                let mut entries = Vec::new();
                let b = prefix.trim_matches('/');
                for (module, version) in &lock {
                    manifest::validate_module(module)
                        .map_err(|e| format!("manifests.yaml: {e}"))?;
                    manifest::validate_version(version)
                        .map_err(|e| format!("manifests.yaml: {e}"))?;
                    let mdir = mdir.join(format!("{module}-{version}"));
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
                    // version 一致性（http.module.version 的正确性前提）：manifest 的
                    // version 必须等于目录后缀 <module>-<version>。错位/手改的 dist
                    // manifest 会让 http.module.version 与真实加载版本静默不符——
                    // 正是该注入要防的误判源，与 name 校验同款 fail-fast。
                    if m.version != *version {
                        return Err(format!(
                            "manifest version {:?} != directory suffix {version:?} (in {}) — stale or hand-edited dist? run `oj build`",
                            m.version,
                            mf.display()
                        ));
                    }
                    eprintln!("module {} v{} — {}", m.name, m.version, m.desc);
                    let rjs = mdir.join("routes.js");
                    let v = reader(&rjs).map_err(|e| {
                        format!("load {}: {e} — run `oj build` first", rjs.display())
                    })?;
                    for e in routes::entries_from_value(&v) {
                        entries.push(routes::RouteEntry {
                            method: e.method,
                            pattern: format!("/{b}/{}", e.pattern.trim_matches('/')),
                            file: format!("{module}-{version}/{}", e.file),
                            handler: e.handler,
                            schema: e.schema,
                        });
                    }
                }
                let (table2, failures2) = routes::RouteTable::from_entries(mdir, &entries);
                if !failures2.is_empty() {
                    return Err(format!("release routes: {}", failures2.join("; ")));
                }
                (table2, Vec::new())
            };
            failures.extend(mount_failures);
            tables.push((prefix.to_string(), table));
        }
        for f in &failures {
            if let Some(w) = f.strip_prefix("warning: ") {
                eprintln!("warn: route: {w}");
            } else {
                eprintln!("error: route: {f}");
            }
        }
        let n_err = failures
            .iter()
            .filter(|f| !f.starts_with("warning: "))
            .count();
        if n_err > 0 {
            eprintln!("warn: {n_err} route declaration(s) skipped (see errors above)");
        }
        // 路由统计（v0.1.27 起替代逐行三列清单；v0.1.58 跨挂载汇总）：一行汇总；
        // 错误/冲突的具体路由信息由上方 failures 循环逐条输出。
        let method_rows = tables.iter().map(|(_, t)| t.listing().len()).sum::<usize>();
        let patterns = tables
            .iter()
            .flat_map(|(_, t)| t.listing())
            .map(|r| &r.pattern)
            .collect::<std::collections::HashSet<_>>()
            .len();
        let files = tables.iter().map(|(_, t)| t.grouped().len()).sum::<usize>();
        eprintln!(
            "routes: {method_rows} method-row(s), {patterns} pattern(s), {files} api file(s)"
        );
        // `.schema` 声明合法性审计（v0.1.70）：RouteRow.schema 是唯一真源（dev 内省 /
        // release routes.js 都汇到这里）。非法声明必须**硬失败** —— 不得退化为
        // 「记一条 failure 继续跑」，那等于把声明悄悄变成不生效的东西。
        //
        // **不受 `server.schema_validation` 开关影响**：开关管的是「是否执行运行期校验」，
        // 不是「声明是否合法」。逃生门是给「规则误杀合法流量」准备的，不是给代码写错
        // 准备的——写错了就别让它起来，否则拼错的通道还在裸奔、下游还拿 openapi.json
        // 去生成 client。
        for (_, table) in &tables {
            table.audit_schemas()?;
        }
        // 入参契约装配（v0.1.44）：审计已过，此处只登记运行期要执行的那部分。
        if cfg.server.schema_validation {
            let mut reg = InputContractRegistry::new();
            for (_, table) in &tables {
                for row in table.listing() {
                    let Some(sv) = row.schema.clone() else {
                        continue;
                    };
                    let file = table.file_path(row.file).to_path_buf();
                    let contract = InputContract::try_new(
                        sv.get("params").cloned(),
                        sv.get("query").cloned(),
                        sv.get("body").cloned(),
                        &mut Vec::new(),
                    )
                    .map_err(|e| {
                        format!(
                            "invalid `.schema` on {} {} (handler `{}`, {}): {e}",
                            row.method,
                            row.pattern,
                            row.handler,
                            file.display()
                        )
                    })?;
                    // 契约键 = (api 文件, handler 链)（v0.1.64）：嵌套组各 verb
                    // 契约互不串门；与 run_module_validated 的 check_request 同键。
                    reg.insert(file, &row.handler, contract).map_err(|e| {
                        format!("server.schema_validation: cannot register contract: {e}")
                    })?;
                }
            }
            let n = reg.len();
            *backend
                .input_contracts()
                .write()
                .unwrap_or_else(|p| p.into_inner()) = reg;
            if n > 0 {
                eprintln!("input contracts: {n} route-method(s) validated before reaching JS");
            }
        }
        let n = cfg.server.pool_size.max(1) as usize;
        // 通配语义 v0.1.20 收紧的迁移提示（v0.1.23 起按影响面判定）：必须等路由表就绪，
        // 才能判「改 `**` 是否会真多命中」（见 warn_legacy_tail_wildcards）。
        // 多挂载：每张表按**自己的挂载前缀**剥 base。
        let route_views: Vec<String> = tables
            .iter()
            .flat_map(|(prefix, table)| {
                table
                    .listing()
                    .iter()
                    .map(|r| anon_view_of_route(&r.pattern, prefix))
                    .collect::<Vec<_>>()
            })
            .collect();
        warn_legacy_tail_wildcards(&cfg, &route_views);
        let timeout = config::parse_duration(&cfg.server.timeout).ok();
        // actor 池：bridges 与 WS 连接共享同一 Bus 与 Extras。
        let actor = JsActor::pool(n, make_bridge.clone());
        // 证书必配（门禁已确保两路径齐备）→ 加载并校验，证书失效即拒绝启动。
        // 运行中过期由热加载切换到 Grace/Expired → GET 限制（handle 内），服务不中断。
        let (cert_status, cert_valid_until) = load_cert_with_watcher(&cfg, config_dir)?;

        config::validate_tenant_binding(&cfg, auth.is_some())?;
        // MCP/RPC 装配防线（spec 2026-10-30 R7/R9 + v0.1.110 终审）：enabled 但未
        // 装配 auth 插件且未显式 allow_anonymous → 拒启——否则端点匿名暴露全路由
        // 机器可读目录。两端共用执行链，门控口径一致。
        let mcp_cfg = cfg.server.mcp.clone().unwrap_or_default();
        validate_mcp_gate(&mcp_cfg, "mcp", auth.is_some())?;
        validate_mcp_gate(&cfg.server.rpc, "rpc", auth.is_some())?;
        if !mcp_cfg.enabled {
            let n_routes: usize = tables.iter().map(|(_, t)| t.listing().len()).sum();
            if n_routes > 0 {
                tracing::info!(
                    "{n_routes} routes could be exposed via MCP: set server.mcp.enabled=true"
                );
            }
        }
        // U-18 批 T1.3：装配期 fail-fast——pattern 悬空（零命中/含 {}）在监听前拒绝，
        // 热更路径复用同一装配，悬空 reload 同样被点名拒绝。
        let route_timeouts: Vec<(String, std::time::Duration)> = cfg
            .server
            .route_timeouts
            .iter()
            .map(|r| {
                if r.pattern.trim().is_empty() {
                    return Err("server.route_timeouts: empty pattern".to_string());
                }
                config::parse_duration(&r.timeout)
                    .map(|d| (r.pattern.clone(), d))
                    .map_err(|e| format!("server.route_timeouts[{}]: {e}", r.pattern))
            })
            .collect::<Result<Vec<_>, String>>()?;
        {
            let route_keys: Vec<String> = tables
                .iter()
                .flat_map(|(_, t)| t.patterns().into_iter().map(str::to_string))
                .collect();
            let pats: Vec<String> = route_timeouts.iter().map(|(p, _)| p.clone()).collect();
            validate_route_timeouts(&pats, &route_keys)?;
        }
        let pipeline = serve::Pipeline {
            tenant_header: cfg.tenant.enable.then(|| cfg.tenant.header_key.clone()),
            tenant_require_signed_claim: cfg.tenant.require_signed_claim,
            tenant_anon: config::anon_paths(&cfg.tenant.anonymous_paths),
            auth: auth.clone(),
            max_upload: cfg.server.max_upload_bytes,
            blob_upload_max: cfg.server.blob_upload_max_bytes,
            // oj-5c：路由级 timeout 覆盖（解析/校验见上方 T1.3：fail-fast 含悬空 pattern 拒启）。
            route_timeouts,
            // blob default 经 stable.blobs 暴露（stable 单源；拆分前为 blobs.default()）。
            blob: stable.blobs.default(),
            // MCP / 裸 JSON-RPC 端点（R7/R8/R9）：enabled 才挂，白名单 fail-closed。
            mcp: mcp_cfg.enabled.then(|| serve::McpConfig {
                modules: mcp_cfg.modules.clone(),
                allow_anonymous: mcp_cfg.allow_anonymous,
                // serverInfo.version 与 `oj --version` 同一来源。
                server_version: env!("CARGO_PKG_VERSION").to_string(),
            }),
            rpc: cfg.server.rpc.enabled,
        };
        // WS 目录镜像挂载：逐 api 挂载。路径口径与 api.ts **同源**（`ws::ws_mounts`
        // 走 `routes::mirror_rel`）——`--module-root` 下由模块索引给 `{prefix}/{模块根内路径}`，
        // 不再按挂载根平铺（v0.1.87：平铺会让 `src/wiki/api/ws.ts` 挂成 `/api/wiki/api/ws`）。
        let ws_opts = serve::ws::WsOptions {
            max_connections: cfg.ws.max_connections,
            workers_per_route: cfg.ws.workers_per_route,
            idle_linger_ms: cfg.ws.idle_linger_ms,
        };
        let mut ws_router = Router::new();
        // (挂载目录, 挂载)——清单按各自挂载根取相对路径显示。
        let mut ws_mounts: Vec<(PathBuf, ws::WsMount)> = Vec::new();
        for m in resolved.iter().filter(|m| m.kind.api().is_some()) {
            let (prefix, mdir, ts) = match m.kind.api() {
                Some((d, ts)) => (m.prefix.as_str(), d, ts),
                None => unreachable!("filtered above"),
            };
            // 碰撞（两个不同目录的 ws 文件映射到同一 path）→ 拒启：胜者决定执行哪份 JS。
            let mounts = ws::ws_mounts(prefix, mdir, ts, module_index.as_deref())?;
            ws_router = ws_router.merge(ws::mirror_routes(
                &mounts,
                timeout.unwrap_or(Duration::from_secs(30)),
                make_bridge.clone(),
                ws_opts,
                auth.clone(),
            ));
            ws_mounts.extend(mounts.into_iter().map(|mo| (mdir.to_path_buf(), mo)));
        }
        // 挂载清单（外显）：WS 不进路由表也不进 openapi.json，启动打印是唯一权威面——
        // 下游工具据此核对，不得再从目录结构推 WS 路径。`[anonymous]` = 命中
        // `auth.anonymous_paths`：浏览器 WS 发不了 Authorization 头，匿名是其唯一入口，
        // 命中即「该端点全程匿名可达」（一次验、持续用），故显式点名。
        if !ws_mounts.is_empty() {
            let anon = cfg
                .auth
                .as_ref()
                .map(|a| config::anon_paths(&a.anonymous_paths))
                .unwrap_or_default();
            eprintln!("ws: {} mount(s)", ws_mounts.len());
            for (mdir, mo) in &ws_mounts {
                let rel = mo.file.strip_prefix(mdir).unwrap_or(&mo.file).display();
                let tag = if auth.is_some() {
                    if serve::path_matches(&anon, &mo.path_no_base) {
                        " [anonymous]"
                    } else {
                        " [guarded]"
                    }
                } else {
                    ""
                };
                eprintln!("  GET {} → {}{}", mo.path, rel, tag);
            }
        }
        // server.html_meta_handler（v0.1.25）：必须在（某个挂载的）路由表里命中 GET
        // 路由——拼错 fail-fast（静默降级会让「已注入 meta」变成一句只在爬虫侧才暴露
        // 的谎话）。多 api 挂载：按 handler 前缀定表。
        validate_html_meta_handler(&cfg, &tables)?;
        // server.cors（v0.1.35）：credentials 需显式 origins——否则 tower-http 运行期
        // panic（Any 源 + credentials 不被允许）。装配期 fail-fast 比请求期崩溃更友好。
        if let Some(cors) = &cfg.server.cors
            && cors.credentials
            && cors.origins.is_empty()
        {
            return Err(
                "server.cors: credentials=true 需要显式 origins（origins 为空 = 允许任意源，不可与 credentials 同用）"
                    .into(),
            );
        }
        // 运行时挂载：api 条目吸收同 prefix 的 web（配对 → 无 spa）；独立 web 原样。
        // 最长前缀排序归 serve::app()。
        let runtime_mounts = build_runtime_mounts(resolved, tables);
        let router = serve::app_with_exec(
            runtime_mounts,
            actor,
            timeout,
            // 静态增强（v0.1.25）：per-route meta 注入（静态 JSON 打底 + 动态 handler
            // 覆盖）+ HTML Cache-Control；SPA 回落已下沉挂载 `spa`。
            serve::StaticOpts {
                html_meta: cfg.server.html_meta.clone(),
                html_meta_handler: cfg.server.html_meta_handler.clone(),
                html_cache_control: cfg.server.html_cache_control.clone(),
                // oj-8：全局自定义响应头（per-site 覆盖在 WebMount.headers）。
                response_headers: cfg
                    .server
                    .response_headers
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            },
            pipeline,
            cert_status,
            cert_valid_until,
            // 插件自省清单经 stable 单源读取（与 StableState.plugins 同源）。
            Arc::new(stable.plugins.clone()),
            // REQ-11 G2：挂断联动登记表（与 StableState.exec_sessions 同一 Arc）。
            Some(stable.exec_sessions.clone()),
            cfg.server.cors.clone(),
        )
        .merge(ws_router);
        Ok(App {
            router,
            bus: stable.bus.clone(),
            stable,
            backend,
            base,
            tasks_flag,
            make_task_bridge,
            auth_guard: auth,
            tenant_header: cfg.tenant.enable.then(|| cfg.tenant.header_key.clone()),
            tenant_anon: config::anon_paths(&cfg.tenant.anonymous_paths),
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
    /// `&self`（不消费 App）：停机路径还要用它做 `drain_mail`（在途邮件排空）。
    pub async fn serve(&self, addr: SocketAddr) -> Result<(SocketAddr, JoinHandle<()>), String> {
        self.serve_graceful(addr, std::future::pending()).await
    }

    /// 绑定并服务 + 优雅停机（spec §6 ⑤）：`shutdown` resolve 后停止接受新连接、
    /// 排空在途请求（与任务池停机同一信号触发）。`&self`：调用方在服务停止后仍需
    /// 用它做停机排空（`drain_mail`）。
    pub async fn serve_graceful(
        &self,
        addr: SocketAddr,
        shutdown: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> Result<(SocketAddr, JoinHandle<()>), String> {
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| format!("bind: {e}"))?;
        let bound = listener
            .local_addr()
            .map_err(|e| format!("local_addr: {e}"))?;
        let router = self.router.clone(); // Router 克隆廉价（内部 Arc）
        let h = tokio::spawn(async move {
            let _ = serve::serve_router(listener, router, shutdown).await;
        });
        Ok((bound, h))
    }

    /// 热更新服务形态（v1，仅 `server.reload_api=true` 时由 `serve_cmd::run` 调用）：
    /// listener 永生 + 外层单一 fallback（`ReloadState` 持有当前内层 Router 的 watch 通道）。
    /// `rs` 已由 `serve_cmd` 构造（含 `reload_executor` 闭包与首代内层 Router）。
    pub async fn serve_hot_reload(
        &self,
        addr: SocketAddr,
        rs: std::sync::Arc<serve::reload::ReloadState>,
        shutdown: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> Result<(SocketAddr, JoinHandle<()>), String> {
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| format!("bind: {e}"))?;
        let bound = listener
            .local_addr()
            .map_err(|e| format!("local_addr: {e}"))?;
        let h = tokio::spawn(async move {
            let _ = serve::reload::serve_swappable(listener, rs, shutdown).await;
        });
        Ok((bound, h))
    }

    /// 长任务停机 flag（serve_cmd 信号处理器置位）。
    pub fn tasks_flag(&self) -> Arc<std::sync::atomic::AtomicBool> {
        self.tasks_flag.clone()
    }

    /// 合并附加路由（PRD v2 任务管理 API：装配在 from_config 之后，经此挂进主 router）。
    pub fn merge_router(&mut self, r: Router) {
        self.router = self.router.clone().merge(r);
    }

    /// 任务管理 API 装配状态（serve_cmd 组装 `{base}/tasks` 路由用）。
    pub fn tasks_api_state(
        &self,
        registry: Arc<only_js::bridge::task_pool::TaskRegistry>,
        pool: Option<Arc<only_js::bridge::task_pool::TaskPool>>,
    ) -> serve::tasks::TasksApiState {
        serve::tasks::TasksApiState {
            registry,
            pool,
            auth: self.auth_guard.clone(),
            tenant_header: self.tenant_header.clone(),
            tenant_anon: self.tenant_anon.clone(),
        }
    }

    /// 停机排空在途邮件（spec §6 ⑤「停机 graceful drain」）：SIGTERM/正常退出的停机路径调用
    /// （HTTP 停收 + 任务收场**之后**，进程退出之前）。
    ///
    /// 控制报文 drain 是**同步**语义（会等至多 `timeout`）⇒ 放 blocking 池，不占 reactor 线程。
    /// 失败只告警不阻断：停机路径的目标是「尽力送达」，不是「保证送达」。
    pub async fn drain_mail(&self, timeout: Duration) {
        let Some(mail) = self.stable.mail.clone() else {
            return; // 未配 smtp / 未装 oj-mail：无可排空
        };
        match tokio::task::spawn_blocking(move || drain_mail_backend(&mail, timeout)).await {
            Ok(v) if v["code"] == 0 && v["data"]["drained"] == true => {
                eprintln!("mail: drain ok（在途投递已排空）");
            }
            Ok(v) => {
                // 超时或后端不支持：如实告警（超时 ⇒ 在途邮件可能被丢弃）。
                eprintln!(
                    "warn: mail drain 未完成：{}（在途邮件可能被丢弃）",
                    v["msg"].as_str().unwrap_or("未知原因")
                );
            }
            Err(e) => eprintln!("warn: mail drain 任务异常：{e}"),
        }
    }

    /// 任务 Bridge 工厂（tasks_flag 已注入；监督器每任务一条线程独立建桥）。
    pub fn make_task_bridge(&self) -> Arc<dyn Fn() -> Bridge + Send + Sync> {
        self.make_task_bridge.clone()
    }

    /// 唯一 StableState（与 actor 共享后端），供测试运行时构造 bridge_ext。
    pub fn stable(&self) -> Arc<StableState> {
        self.stable.clone()
    }

    /// 后端装配产物（含全部连接池 + 插件 vtable）；热更时复用以避免重连（§5 ②）。
    pub fn backend(&self) -> Arc<Backend> {
        self.backend.clone()
    }

    /// 当前鉴权守卫（reload 注入 `ReloadState` 用；返回克隆的 `Arc` 句柄）。
    pub fn auth_guard(&self) -> Option<Arc<dyn only_js::bridge::AuthGuard>> {
        self.auth_guard.clone()
    }

    /// API 基础前缀。
    pub fn base(&self) -> &str {
        &self.base
    }

    /// 路由表副本（client.ws 惰性本地 `axum::serve` 用；oneshot 派发覆盖不到 WS 帧循环）。
    pub fn router(&self) -> Router {
        self.router.clone()
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
#[path = "app_tests.rs"]
mod tests;
