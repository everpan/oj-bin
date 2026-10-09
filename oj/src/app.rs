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
    Bridge, EsBackend, Extras, InMemoryKV, JwtCfg, KVStore, LoaderShared, ModuleCtx, NamedRegistry,
    SchemaRegistry,
};
use only_js::bridge::{EventBroker, SqlGuard, StableState};
use only_js::config::{self, Config};
use only_js::contract::{InputContract, InputContractRegistry};
use serve::CertificateStatus;
use serve::actor::JsActor;
use serve::certificate::load_certificate_at;
use serve::certificate_watcher::{SharedCertStatus, SharedCertValidUntil, spawn_watcher};
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

/// 归属图 + SchemaRegistry 复活（§4.8，装配第 11 步）：discover 全模块 → schema.yaml +
/// manifest(db/deps) → registry（S002 同表双声明 fail-fast；table_owned 记 owner）+ ModuleCtx
/// map（键 = 模块目录绝对路径，run_module 祖先命中注入）。reconcile 不在此
/// 执行——由 server 装配层在迁移 gate 后显式调用 `schema::reconcile_all`（spec §2.2 #12）。
async fn build_schema_and_modules(
    dir: &Path,
    ts: bool,
    guard: SqlGuard,
    shared_allow: &[String],
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
            for (t, pk, cols, tenant_flag) in f.registry_tables() {
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

/// `server.html_meta_handler`（v0.1.25）装配期校验：必须命中路由表里的一个 **GET** 路由。
///
/// fail-fast 而非启动后 WARN：配错的后果（页面 meta 没注入）只在爬虫/IM 预览侧可见，
/// 业务页面自己看不出来——留给运行期的告警等于让配置撒谎。
fn validate_html_meta_handler(cfg: &Config, table: &routes::RouteTable) -> Result<(), String> {
    let Some(h) = cfg.server.html_meta_handler.as_deref() else {
        return Ok(());
    };
    let norm = routes::normalize(h)
        .ok_or_else(|| format!("server.html_meta_handler: {h:?} 不是合法路径（须以 / 开头）"))?;
    match table.lookup(&norm, "GET") {
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
    }
}

/// 静态站点目录绝对化 + canonicalize（缺失/非目录 → fail-fast）。
fn static_dir(config_dir: &Path, r: &str) -> Result<PathBuf, String> {
    let p = Path::new(r);
    let p = if p.is_absolute() {
        p.to_path_buf()
    } else {
        config_dir.join(p)
    };
    p.canonicalize()
        .map_err(|e| format!("静态目录 {}: {e}", p.display()))
}

/// 有效静态站点表（装配第 20 步，v0.1.27 多站点）：legacy `(app_prefix, app_path)` 对 +
/// `server.static_sites` 逐条 → `Vec<StaticSite>`。前缀经 `resolve_app_prefix` 归一；
/// 归一后重复 → Err（报两条来源）；目录相对 config_dir 绝对化 + canonicalize。
/// **不排序**——最长前缀优先的排序归 `serve::app()` 独家负责。
fn resolve_static_sites(cfg: &Config, config_dir: &Path) -> Result<Vec<serve::StaticSite>, String> {
    let mut out = Vec::new();
    // 归一前缀 → 来源描述（dup 报错要报两条来源）。
    let mut seen: Vec<(String, String)> = Vec::new();
    let push = |prefix: &str,
                path: &str,
                headers: &std::collections::HashMap<String, String>,
                source: &str,
                out: &mut Vec<serve::StaticSite>,
                seen: &mut Vec<(String, String)>|
     -> Result<(), String> {
        let p = crate::serve_cmd::resolve_app_prefix(prefix)
            .map_err(|e| format!("{source}: prefix {prefix:?}: {e}"))?;
        if let Some((_, prev)) = seen.iter().find(|(q, _)| *q == p) {
            return Err(format!("静态站点前缀 {p} 重复：{source} 与 {prev} 冲突"));
        }
        seen.push((p.clone(), source.to_string()));
        let root = static_dir(config_dir, path).map_err(|e| format!("{source}: {e}"))?;
        out.push(serve::StaticSite {
            prefix: p,
            root,
            headers: headers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        });
        Ok(())
    };
    if let Some(app_path) = &cfg.server.app_path {
        push(
            &cfg.server.app_prefix,
            app_path,
            &std::collections::HashMap::new(),
            "server.app_path（前缀取 server.app_prefix）",
            &mut out,
            &mut seen,
        )?;
    }
    for s in &cfg.server.static_sites {
        push(
            &s.prefix,
            &s.path,
            &s.headers,
            &format!("server.static_sites[prefix={}]", s.prefix),
            &mut out,
            &mut seen,
        )?;
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
    spawn_watcher(
        cert_status.clone(),
        cert_valid_until.clone(),
        cfg.server.clone(),
        config_dir.to_path_buf(),
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
pub async fn assemble_backend(
    cfg: &Config,
    top: &serde_json::Value,
    config_dir: &Path,
    dir: &Path,
    base: &str,
    ts: bool,
    profiles: &ResourceProfiles,
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
    let plugin_infos: std::sync::Arc<Vec<only_js::bridge::PluginInfo>> = std::sync::Arc::new(
        assemble_plugins(
            cfg,
            top,
            config_dir,
            &mut registries,
            profiles.es.as_deref(),
        )
        .await
        .map_err(|e| format!("plugins: {e}"))?,
    );
    // KV：redis.<key> 存在 → 经 kv 插件 vtable connect（单例 fail-fast）；
    // 未声明 → InMemoryKV 内置兜底。
    let kv: Arc<dyn KVStore> = connect_kv(cfg, &registries, redis_key).await?;
    let es: Option<Arc<dyn EsBackend>> = registries.es;
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
    let (registry, modules) =
        build_schema_and_modules(dir, ts, sql_guard, &cfg.tenant.shared_allow).await?;
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
    // db 默认库重定向需进入 'static 工厂闭包，先取出 owned（profiles 是借用，不能捕获）。
    let db_override = profiles.db.clone();
    // 单一工厂（内省 / actor 池 / WS 连接共享同一 Bus 与 Extras）——闭包捕获全 Arc，
    // Clone 即共享。tasks_flag 参数化：None = HTTP 桥，Some = 任务桥。
    // cfg 只以 Copy 字段（allow_as_tenant）进入闭包：引用不得活进 'static 工厂，
    // 故先解出值再捕获（stable 单源之前 from_config 持 cfg 的等价写法）。
    let allow_as_tenant = cfg.tenant.allow_as_tenant;
    // SQL 执行追踪（v0.1.51）：dev 默认开、release 默认关，config 段可强制覆盖。
    let sql_trace = only_js::bridge::sql_trace::SqlTraceConfig::resolve(&cfg.db_trace, ts);
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
        // 影子绑定：同 vars/js_heap_limit —— 闭包带走的是这里的副本。
        let input_contracts = input_contracts.clone();
        let fs_grant = fs_grant.clone();
        move |tasks_flag: Option<Arc<std::sync::atomic::AtomicBool>>| {
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
                    // mq 命名实例（T7 装配注入；此处空表兜底，编译占位）。
                    kafkas: Some(kafkas.clone()),
                    rabbits: Some(rabbits.clone()),
                    tasks_flag,
                    // mail 后端（smtp: 段 + oj-mail 插件；未配置/未加载 = None）。
                    mail: mail.clone(),
                    // ldap 后端（ldap: 段 + oj-ldap 插件；未配置/未加载 = None）。
                    ldap: ldap.clone(),
                    vars: vars.clone(),
                    js_heap_limit,
                    input_contracts: Some(input_contracts.clone()),
                    // fs 授权（v0.1.53；config fs: 段）：与 StableState.fs 同一 Arc。
                    fs: fs_grant.clone(),
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
        kafkas: kafkas.clone(),   // 与 make_bridge 的 Extras.kafkas 同源。
        rabbits: rabbits.clone(), // 与 make_bridge 的 Extras.rabbits 同源。
        tasks_flag: None,
        sql_memo: std::sync::Mutex::new(std::collections::HashMap::new()),
        mail: mail.clone(), // 与 make_bridge 的 Extras.mail 同源（同一 Arc）。
        ldap: ldap.clone(), // 与 make_bridge 的 Extras.ldap 同源（同一 Arc）。
        vars: vars.clone(), // 与 make_bridge 的 Extras.vars 同源（同一 Arc）。
        input_contracts: Some(input_contracts.clone()),
        js_heap_limit,
        fs: fs_grant.clone(), // 与 make_bridge 的 Extras.fs 同一 Arc。
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
    #[allow(clippy::too_many_arguments)] // top 与 cfg 分层传（serde 无关的查找面），不并为 struct
    pub async fn from_config(
        cfg: Config,
        top: &serde_json::Value,
        config_dir: &Path,
        dir: PathBuf,
        base: String,
        ts: bool,
        fixtures: bool,
        // profiles：各资源根 key 的默认 profile 选择（对应 `oj test`/`oj exec` 的
        // `--db`/`--redis`/`--blob`/`--es`/`--broker`/`--kafka`/`--rabbit`）。字面 "default"
        // 的调用改指向所选 profile；迁移 / seed / fixtures / schema 内省一并跟随。
        profiles: &ResourceProfiles,
    ) -> Result<App, String> {
        // 绝对化 dir（Bridge loader 的 project_root 用 config_dir，api 相对 dir）。
        // strip_verbatim 去 Windows `\\?\` 前缀：canonicalize 与 referrer 目录（`to_file_path`
        // 剥前缀）同形，避免 `module_root_of` 词法前缀不一致误判「未找到模块根」。
        let dir = dir.canonicalize().unwrap_or(dir);
        // 后端装配（spec §2.2 归属表）：StableState 唯一构造点，HTTP 步全在下方。
        let backend =
            Arc::new(assemble_backend(&cfg, top, config_dir, &dir, &base, ts, profiles).await?);
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
        let gate = migrate_gate_of(&cfg, ts);
        let db_key: &str = profiles.db.as_deref().unwrap_or("default");
        match gate {
            "auto" => {
                crate::migrate::apply_all(stable.dbs.get(db_key), &dir, ts, false).await?;
                for l in crate::schema::reconcile_all(
                    stable.dbs.get(db_key).map(|a| a.as_ref()),
                    &dir,
                    ts,
                    db_key,
                )
                .await?
                {
                    eprintln!("schema: {l}");
                }
            }
            "verify" => crate::migrate::verify_all(stable.dbs.get(db_key), &dir, ts).await?,
            "off" => {}
            other => {
                return Err(format!(
                    "server.migrate_on_start: illegal value {other:?} (auto|verify|off)"
                ));
            }
        }
        // 种子重放（P0）：各模块 seed.sql（§8-1）。
        crate::seed::replay_all(stable.dbs.get(db_key), &dir).await?;
        // fixtures/ 演示数据（§4.5）：仅 oj test（fixtures=true）灌入；server 不灌。
        if fixtures {
            let modules = crate::manifest::discover(&dir, ts)?;
            crate::migrate_cmd::load_fixtures(stable.dbs.get(db_key), &modules).await?;
        }
        // ext_boot 预热：建 runtime 并跑完 boot，失败即 `Err`（真·启动失败）。
        // 必须前移到建表之前 —— 否则 boot 错误只能借 dev 内省的间接失败暴露，而
        // `bridge_introspector` 会把线程 panic 吞成路由 failure（装配层只 warn 不致命，
        // 结果「路由全空、服务照常监听」）。
        if stable.boot.is_some() {
            prewarm_boot(make_bridge.clone())?;
        }
        // 路由表：dev 启动内省 .route 声明；release 聚合 dist/manifests.yaml。
        let (table, failures) = if ts {
            for m in manifest::load_modules(&dir, Some(&cfg.tasks.dir))? {
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
                        schema: e.schema,
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
        // 路由统计（v0.1.27 起替代逐行三列清单）：一行汇总；错误/冲突的具体路由
        // 信息由上方 failures 循环逐条输出（dev warn + release 硬失败均含 pattern）。
        let method_rows = table.listing().len();
        let patterns = table
            .listing()
            .iter()
            .map(|r| &r.pattern)
            .collect::<std::collections::HashSet<_>>()
            .len();
        let files = table.grouped().len();
        eprintln!(
            "routes: {method_rows} method-row(s), {patterns} pattern(s), {files} api file(s)"
        );
        // 入参契约装配（v0.1.44）：RouteRow.schema 是唯一真源（dev 内省 / release
        // routes.js 都汇到这里）。非法契约必须**硬失败** —— 不得退化为
        // 「记一条 failure 继续跑」，那等于把声明悄悄变成不生效的东西。
        if cfg.server.schema_validation {
            let mut reg = InputContractRegistry::new();
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
                        "server.schema_validation: invalid `.schema` on {} {} ({}): {e}",
                        row.method,
                        row.pattern,
                        file.display()
                    )
                })?;
                reg.insert(file, &row.method, contract).map_err(|e| {
                    format!("server.schema_validation: cannot register contract: {e}")
                })?;
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
        let route_views: Vec<String> = table
            .listing()
            .iter()
            .map(|r| anon_view_of_route(&r.pattern, &base))
            .collect();
        warn_legacy_tail_wildcards(&cfg, &route_views);
        let timeout = config::parse_duration(&cfg.server.timeout).ok();
        // actor 池：bridges 与 WS 连接共享同一 Bus 与 Extras。
        let actor = JsActor::pool(n, make_bridge.clone());
        // 静态站点表（装配第 20 步，v0.1.27 多站点）：legacy (app_prefix, app_path) 对 +
        // server.static_sites 逐条；前缀归一 + dup fail-fast（报两条来源）+ 目录
        // canonicalize（缺失 fail-fast）。最长前缀排序归 serve::app()。
        let static_sites = resolve_static_sites(&cfg, config_dir)?;
        // 证书必配（门禁已确保两路径齐备）→ 加载并校验，证书失效即拒绝启动。
        // 运行中过期由热加载切换到 Grace/Expired → GET 限制（handle 内），服务不中断。
        let (cert_status, cert_valid_until) = load_cert_with_watcher(&cfg, config_dir)?;

        config::validate_tenant_binding(&cfg, auth.is_some())?;
        let pipeline = serve::Pipeline {
            tenant_header: cfg.tenant.enable.then(|| cfg.tenant.header_key.clone()),
            tenant_require_signed_claim: cfg.tenant.require_signed_claim,
            tenant_anon: config::anon_paths(&cfg.tenant.anonymous_paths),
            auth: auth.clone(),
            max_upload: cfg.server.max_upload_bytes,
            blob_upload_max: cfg.server.blob_upload_max_bytes,
            // oj-5c：路由级 timeout 覆盖——装配期解析 fail-fast（非法时长/空 pattern
            // 都是配置错误，静默忽略等于部署了假保险）。
            route_timeouts: cfg
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
                .collect::<Result<Vec<_>, String>>()?,
            // blob default 经 stable.blobs 暴露（stable 单源；拆分前为 blobs.default()）。
            blob: stable.blobs.default(),
        };
        // WS 目录镜像挂载（<dir>/ws.ts → {base}/<dir>/ws）。
        let ws_opts = serve::ws::WsOptions {
            max_connections: cfg.ws.max_connections,
            workers_per_route: cfg.ws.workers_per_route,
            idle_linger_ms: cfg.ws.idle_linger_ms,
        };
        let ws_router = ws::mirror_routes(
            &base,
            &dir,
            timeout.unwrap_or(Duration::from_secs(30)),
            make_bridge,
            ws_opts,
            auth.clone(),
        );
        // server.html_meta_handler（v0.1.25）：必须是路由表里存在的 GET 路由——拼错
        // fail-fast（静默降级会让「已注入 meta」变成一句只在爬虫侧才暴露的谎话）。
        validate_html_meta_handler(&cfg, &table)?;
        // server.cors（v0.1.35）：credentials 需显式 origins——否则 tower-http 运行期
        // panic（Any 源 + credentials 不被允许）。装配期 fail-fast 比请求期崩溃更友好。
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
        let router = serve::app(
            &base,
            dir,
            ts,
            table,
            actor,
            timeout,
            static_sites,
            // 静态站点增强（v0.1.20 / v0.1.25）：SPA 深链接回落 + per-route meta 注入
            // （静态 JSON 打底 + 动态 handler 覆盖）+ HTML Cache-Control。
            serve::StaticOpts {
                spa_fallback: cfg.server.app_spa_fallback,
                html_meta: cfg.server.html_meta.clone(),
                html_meta_handler: cfg.server.html_meta_handler.clone(),
                html_cache_control: cfg.server.html_cache_control.clone(),
                // oj-8：全局自定义响应头（per-site 覆盖在 StaticSite.headers）。
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
            cfg.server.cors.clone(),
        )
        .merge(ws_router);
        Ok(App {
            router,
            bus: stable.bus.clone(),
            stable,
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
mod tests {
    use super::*;

    // ---- v0.1.34：资源根 key 多源选择（connect_kv 的 --redis 解析） ----

    /// 空 redis 段 → 内置 InMemoryKV 兜底；多源 redis 选错 default → fail-fast；
    /// 选中存在的 profile 但无 kv 插件 → 明确提示缺插件（不静默回落）。
    #[tokio::test]
    async fn connect_kv_redis_profile_selection_and_fail_fast() {
        // 空 redis 段：无论 key 是什么都回落 InMemoryKV。
        let cfg = Config::default();
        let reg = Registries::default();
        let _ = connect_kv(&cfg, &reg, "default").await.unwrap();

        // 多源 redis（无 default）+ 未传 --redis（默认 default）→ fail-fast。
        let mut cfg = Config::default();
        cfg.redis
            .insert("cache".into(), "redis://:pw@h:6379/1".into());
        let e = connect_kv(&cfg, &reg, "default")
            .await
            .err()
            .unwrap_or_default();
        assert!(
            e.contains("--redis profile 'default' not declared"),
            "expected fail-fast on missing default profile, got: {e}"
        );

        // 选 cache：profile 存在，但无 kv 插件 → 另一种 fail-fast（提示装插件）。
        let e2 = connect_kv(&cfg, &reg, "cache")
            .await
            .err()
            .unwrap_or_default();
        assert!(
            e2.contains("no kv plugin loaded"),
            "expected missing-plugin error, got: {e2}"
        );
    }

    // ---- v0.1.25：server.html_meta_handler 装配期校验 ----

    /// meta handler 必须命中一个 GET 路由：命中（含尾斜杠归一）/ 未命中 / 非法路径 / 未配。
    #[test]
    fn html_meta_handler_must_resolve_to_a_get_route() {
        let dir = std::env::temp_dir().join(format!("oj-metah-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("meta.ts"), "export default {};\n").unwrap();
        let entries = vec![
            routes::RouteEntry {
                method: "get".into(),
                pattern: "/v1/api/html-meta".into(),
                file: "meta.ts".into(),
                schema: None,
            },
            routes::RouteEntry {
                method: "post".into(),
                pattern: "/v1/api/post-only".into(),
                file: "meta.ts".into(),
                schema: None,
            },
        ];
        let (table, failures) = routes::RouteTable::from_entries(&dir, &entries);
        assert!(failures.is_empty(), "{failures:?}");

        let mut cfg = Config::default();
        // 未配置 → 放行（不开动态 meta 是默认形态）
        assert!(validate_html_meta_handler(&cfg, &table).is_ok());
        // 命中（尾斜杠与 normalize 等价）
        cfg.server.html_meta_handler = Some("/v1/api/html-meta/".into());
        assert!(validate_html_meta_handler(&cfg, &table).is_ok());
        // 未命中 → 拼错必须 fail-fast，不能留到运行期只在爬虫侧暴露
        cfg.server.html_meta_handler = Some("/v1/api/typo".into());
        let e = validate_html_meta_handler(&cfg, &table).unwrap_err();
        assert!(e.contains("不在路由表"), "{e}");
        // 只有 POST → 报「未映射 GET 方法」（而不是含混的未命中）
        cfg.server.html_meta_handler = Some("/v1/api/post-only".into());
        let e = validate_html_meta_handler(&cfg, &table).unwrap_err();
        assert!(e.contains("未映射 GET 方法"), "{e}");
        // 不以 / 开头 → 非法路径
        cfg.server.html_meta_handler = Some("v1/api/html-meta".into());
        let e = validate_html_meta_handler(&cfg, &table).unwrap_err();
        assert!(e.contains("不是合法路径"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- v0.1.27：多静态站点有效站点表 ----

    fn tmp_dirs(names: &[&str]) -> (PathBuf, Vec<PathBuf>) {
        let base = std::env::temp_dir().join(format!(
            "oj-sites-{}-{}",
            std::process::id(),
            names.join("_")
        ));
        let _ = std::fs::remove_dir_all(&base);
        let dirs: Vec<PathBuf> = names
            .iter()
            .map(|n| {
                let d = base.join(n);
                std::fs::create_dir_all(&d).unwrap();
                d
            })
            .collect();
        (base, dirs)
    }

    #[test]
    fn resolve_static_sites_legacy_and_list() {
        // legacy 单站点（app_path + app_prefix）产一条，前缀归一。
        let (base, dirs) = tmp_dirs(&["app", "docs", "root"]);
        let mut cfg = Config::default();
        cfg.server.app_path = Some(dirs[0].to_string_lossy().into());
        cfg.server.app_prefix = "/app/".into(); // 尾斜杠归一 → /app
        let sites = resolve_static_sites(&cfg, &base).unwrap();
        assert_eq!(sites.len(), 1);
        assert_eq!(sites[0].prefix, "/app");
        assert_eq!(sites[0].root, dirs[0].canonicalize().unwrap());

        // static_sites 两条（含 `/` 兜底）。
        let mut cfg = Config::default();
        cfg.server.static_sites = vec![
            only_js::config::StaticSiteConf {
                prefix: "/docs".into(),
                path: dirs[1].to_string_lossy().into(),
                headers: Default::default(),
            },
            only_js::config::StaticSiteConf {
                prefix: "/".into(),
                path: dirs[2].to_string_lossy().into(),
                headers: Default::default(),
            },
        ];
        let sites = resolve_static_sites(&cfg, &base).unwrap();
        assert_eq!(sites.len(), 2);
        assert_eq!(sites[0].prefix, "/docs");
        assert_eq!(sites[1].prefix, "/");

        // 未配置 → 空表（不开静态服务）。
        assert!(
            resolve_static_sites(&Config::default(), &base)
                .unwrap()
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn resolve_static_sites_dup_prefix_and_missing_dir_fail_fast() {
        let (base, dirs) = tmp_dirs(&["app", "docs"]);
        // dup：legacy 对归一后与 static_sites 同前缀 → Err 且报两条来源。
        let mut cfg = Config::default();
        cfg.server.app_path = Some(dirs[0].to_string_lossy().into());
        cfg.server.app_prefix = "/docs/".into();
        cfg.server.static_sites = vec![only_js::config::StaticSiteConf {
            prefix: "/docs".into(),
            path: dirs[1].to_string_lossy().into(),
            headers: Default::default(),
        }];
        let e = resolve_static_sites(&cfg, &base).unwrap_err();
        assert!(e.contains("重复") && e.contains("server.app_path"), "{e}");
        assert!(e.contains("static_sites"), "{e}");

        // 缺失目录 → Err（canonicalize fail-fast）。
        let mut cfg = Config::default();
        cfg.server.static_sites = vec![only_js::config::StaticSiteConf {
            prefix: "/x".into(),
            path: "no-such-dir".into(),
            headers: Default::default(),
        }];
        let e = resolve_static_sites(&cfg, &base).unwrap_err();
        assert!(e.contains("no-such-dir"), "{e}");
        let _ = std::fs::remove_dir_all(&base);
    }

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

    /// exec 拆分契约（spec §2/§3.1）：后端装配不含证书门禁——Backend 可在无证书
    /// 配置的最小 config 上构造，stable 是唯一数据源（访问器直达）。
    #[tokio::test(flavor = "current_thread")]
    async fn backend_constructs_without_certificate() {
        let base = std::env::temp_dir().join(format!("oj-backend-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let cfg = Config::default(); // 无证书、无 db/redis 段
        let backend = super::assemble_backend(
            &cfg,
            &serde_json::Value::Null,
            &base,
            &base,
            "/v1/api",
            true,
            &ResourceProfiles::default(),
        )
        .await
        .unwrap();
        // stable 单源：kv/dbs/loader 都经访问器直达，Backend 不复制字段。
        assert!(backend.stable().loader.is_some());
        assert!(backend.stable().dbs.is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }

    // ---- v0.1.23：匿名路径迁移 WARN 的影响面判定（U39）----

    /// 路由 pattern → 匿名口径：剥 base，`{param}`→`*`、`{*rest}`→`**`。
    #[test]
    fn anon_view_strips_base_and_normalizes_params() {
        assert_eq!(
            anon_view_of_route("/v1/api/oidc/{id}", "/v1/api"),
            "/oidc/*"
        );
        assert_eq!(
            anon_view_of_route("/v1/api/idp/.well-known/openid-configuration", "/v1/api"),
            "/idp/.well-known/openid-configuration"
        );
        assert_eq!(
            anon_view_of_route("/v1/api/assets/{*path}", "/v1/api"),
            "/assets/**"
        );
        // base 不在 pattern 里（非常规）→ 原样归一，不 panic。
        assert_eq!(anon_view_of_route("/health", "/v1/api"), "/health");
    }

    /// 旧式前缀形态：只有尾段一个 `*`、其余段全字面。
    ///
    /// 这是「可能受 v0.1.20 收紧影响」的**唯一**形态——旧 oj-auth 是 `strip_suffix("/*")` +
    /// `starts_with`，含中段 `*` 的条目当时匹配不上任何真实请求（前缀里带字面 `*`），
    /// 只能诞生于 v0.1.20 之后的四形态语义，即刻意写出的结构。
    #[test]
    fn legacy_prefix_shape_is_tail_wildcard_on_literal_segments() {
        assert!(is_legacy_prefix_shape("/idp/*"));
        assert!(is_legacy_prefix_shape("/users/me/accounts/*"));
        assert!(is_legacy_prefix_shape("/public/assets/v2/anchor/*"));
        // 结构条目（中段含 `*`）→ 不是旧前缀形态（下游 config 实测里的真实写法）。
        assert!(!is_legacy_prefix_shape("/public/anchor/*/issues/*"));
        assert!(!is_legacy_prefix_shape("/public/assets/v2/anchor/*/*"));
        assert!(!is_legacy_prefix_shape("/a/*/b")); // 尾段非 `*`
        assert!(!is_legacy_prefix_shape("/health"));
        assert!(!is_legacy_prefix_shape("/a/**"));
    }

    /// 判据核心：旧前缀形态 **且** 自己的面确实比严格一层更深 → 告警；否则静默。
    ///
    /// 语义口径：旧 oj-auth 是「任意深度」，收紧成严格一层后**丢掉的**恰好是「head + ≥2 段」；
    /// `**` 多出来的零层（裸 head）旧语义也没覆盖 → 不算丢面、不告警。
    #[test]
    fn coverage_loss_requires_a_deeper_registered_route() {
        // 深两层的注册路由被收回 → 告警（U39 真阳性样本）。
        assert!(tail_entry_loses_coverage(
            "/idp/*",
            &["/idp/.well-known/openid-configuration".to_string()]
        ));
        // 本就没有更深路由 → 静默。
        assert!(!tail_entry_loses_coverage(
            "/auth/oidc/*",
            &["/auth/oidc/callback".to_string()]
        ));
        // 同层 → 不新增命中 → 静默。
        assert!(!tail_entry_loses_coverage("/a/*", &["/a/b".to_string()]));
        // 无任何路由（纯 404 面）→ 静默。
        assert!(!tail_entry_loses_coverage("/a/*", &[]));
        // 评审反例①：catch-all 路由（`{*path}` → 视图 `**`）。旧行为下 /file/a/b 是免鉴权的，
        // 收紧确实收回了面 → 必须告警（此前用 path_matches 对拍两个模式会漏报）。
        assert!(tail_entry_loses_coverage(
            "/file/*",
            &["/file/**".to_string()]
        ));
        // 评审反例②：路由带参数段（视图 `/*/detail/sub`）→ `/zzz/detail/sub` 真实可达且更深
        // → 必须告警（此前两侧都匹配不上而漏报）。
        assert!(tail_entry_loses_coverage(
            "/zzz/*",
            &["/*/detail/sub".to_string()]
        ));
        // 评审反例③：模块根路由（视图与 head 同层，`**` 只多出「零层」）→ 旧语义也没覆盖裸路径
        // → 不算丢面，静默（否则下游「模块根路由 + /module/* 条目」这种常见形状会被迫标 one_layer）。
        assert!(!tail_entry_loses_coverage(
            "/user/account/*",
            &["/user/account".to_string()]
        ));
        // 「head + 恰好 1 段」= 严格一层已覆盖的深度 → 不算丢面；再加一层才算。
        assert!(!tail_entry_loses_coverage("/a/*", &["/a/b".to_string()]));
        assert!(tail_entry_loses_coverage("/a/*", &["/a/b/c".to_string()]));
        // 结构条目：即使更深路由存在，也不按旧前缀看待 → 静默（改 `**` 不是它想要的形状）。
        let structured = vec!["/public/anchor/*/issues/*/comments/*".to_string()];
        assert!(!tail_entry_loses_coverage(
            "/public/anchor/*/issues/*",
            &structured
        ));
        assert!(!tail_entry_loses_coverage(
            "/public/assets/v2/anchor/*/*",
            &structured
        ));
        // 对照：同一份路由下，旧前缀形态的 /public/anchor/* 仍会告警。
        assert!(tail_entry_loses_coverage("/public/anchor/*", &structured));
    }

    /// 聚合清单（`warn_legacy_tail_wildcards` 的实际输入）：`one_layer` 确认过的、非旧前缀
    /// 形态的、以及没丢面的条目都被滤掉。**直接打这个函数**，不复制它的过滤链。
    #[test]
    fn legacy_entries_filters_by_shape_and_impact() {
        use only_js::config::AnonPath;
        let list = vec![
            AnonPath::Plain("/idp/*".into()),       // 丢面 → 点名
            AnonPath::Plain("/auth/oidc/*".into()), // 无更深路由 → 静默
            AnonPath::Detailed {
                path: "/x/*".into(),
                one_layer: true,
            }, // 已确认 → 静默
            AnonPath::Plain("/health".into()),      // 非尾 /* → 静默
            // 结构条目（中段 `*`）→ 不按旧前缀看待 → 静默（下游 config 实测的同款写法）
            AnonPath::Plain("/public/anchor/*/issues/*".into()),
            AnonPath::Plain("/public/assets/v2/anchor/*/*".into()),
        ];
        let routes = vec![
            "/idp/.well-known/openid-configuration".to_string(),
            "/auth/oidc/callback".to_string(),
            "/x/a/b/c".to_string(), // 即使会丢面，one_layer 也压掉
            "/public/anchor/*/issues/*/comments/*".to_string(),
        ];
        assert_eq!(legacy_entries(&list, &routes), vec!["/idp/*"]);
        // 空列表 / 空路由表不 panic。
        assert!(legacy_entries(&[], &routes).is_empty());
        assert!(legacy_entries(&list, &[]).is_empty());
    }

    /// 迁移 WARN **只针对 auth 列表**（v0.1.23 订正）：租户侧自引入起即为严格一层
    /// （`git show v0.1.19:server/src/lib.rs` 的 `path_matches`，v0.1.19 的租户豁免 `:364`
    /// 走的就是它），v0.1.20 的收紧没碰它，对 tenant 条目说「收回了面」是伪前提。
    #[test]
    fn migration_warn_is_auth_only() {
        use only_js::config::{AnonPath, AuthCfg};
        let routes = vec!["/idp/.well-known/openid-configuration".to_string()];

        // 只配 tenant → 一条都不点（尽管该条目在 auth 侧同形是会丢面的）。
        let mut cfg = Config::default();
        cfg.tenant.anonymous_paths = vec![AnonPath::Plain("/idp/*".into())];
        assert!(migration_warn_entries(&cfg, &routes).is_empty());

        // 同形条目出现在 auth 侧 → 点名，且 what 是 auth 段。
        cfg.auth = Some(AuthCfg {
            jwt_secret: "k".into(),
            anonymous_paths: vec![AnonPath::Plain("/idp/*".into())],
            ..Default::default()
        });
        let entries = migration_warn_entries(&cfg, &routes);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "auth.anonymous_paths");
        assert_eq!(entries[0].1, vec!["/idp/*"]);
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
        let (kafkas, rabbits) = build_mq_registries(&cfg, &mq_table(&["bus-kafka"]), None, None)
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
        let Err(e) = build_mq_registries(&cfg, &mq_table(&["bus-rabbitmq"]), None, None).await
        else {
            panic!("expected fail-fast");
        };
        assert!(e.contains("no mq plugin for kind 'kafka'"), "{e}");
    }

    /// Given: rabbits 声明只装 kafka 插件（负例，评审 N2）；Then: fail-fast。
    #[tokio::test(flavor = "current_thread")]
    async fn given_rabbits_with_only_kafka_plugin_when_build_then_err() {
        let cfg = cfg_with(serde_json::json!({ "default": {} }), "rabbits");
        let Err(e) = build_mq_registries(&cfg, &mq_table(&["bus-kafka"]), None, None).await else {
            panic!("expected fail-fast");
        };
        assert!(e.contains("no mq plugin for kind 'rabbit'"), "{e}");
    }

    /// Given: 两插件同名 kind 冲突；Then: fail-fast（评审 S5）。
    #[tokio::test(flavor = "current_thread")]
    async fn given_duplicate_kind_plugins_when_build_then_err_conflict() {
        let cfg = cfg_with(serde_json::json!({ "default": {} }), "kafkas");
        let Err(e) =
            build_mq_registries(&cfg, &mq_table(&["bus-kafka", "bus-kafka"]), None, None).await
        else {
            panic!("expected fail-fast");
        };
        assert!(e.contains("multiple mq plugins serve kind 'kafka'"), "{e}");
    }

    /// Given: 两段都空；Then: 空 registry（不报错，JS undefined 语义）。
    #[tokio::test(flavor = "current_thread")]
    async fn given_empty_sections_when_build_then_empty_registries() {
        let cfg = Config::default();
        let (kafkas, rabbits) = build_mq_registries(&cfg, &mq_table(&["bus-kafka"]), None, None)
            .await
            .unwrap();
        assert!(kafkas.is_empty() && rabbits.is_empty());
    }
}

#[cfg(test)]
mod mail_assembly_tests {
    use super::*;
    use oj_plugin_ffi::{FfiFuture, MailAttachment, MailVtable, RString, RVec};
    use only_js::bridge::RequestInfo;
    use only_js::bridge::mail::{CallCtx, MailMode, handle_send};

    /// 假 mail vtable：submit 回固定信封（装配测试零网络、零插件）。jobId 回显 key，
    /// 证明 profile 名确实过线（不是被适配器吞掉）。控制报文（`__ctl`）回排空信封，
    /// 并把收到的 req 记进 [`SEEN_REQS`]（A4 停机排空用）。
    static SEEN_REQS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

    extern "C" fn fake_submit(
        key: RString,
        req: RString,
        _atts: RVec<MailAttachment>,
    ) -> FfiFuture {
        let req = req[..].to_string();
        let is_ctl = req.contains("__ctl");
        SEEN_REQS.lock().unwrap().push(req);
        if is_ctl {
            return oj_plugin_ffi::ready_ok(
                br#"{"code":0,"msg":"ok","data":{"drained":true,"workers":2}}"#.to_vec(),
            );
        }
        oj_plugin_ffi::ready_ok(
            format!(
                r#"{{"code":0,"msg":"ok","data":{{"jobId":"j-{}"}}}}"#,
                &key[..]
            )
            .into_bytes(),
        )
    }
    static FAKE_MAIL: MailVtable = MailVtable {
        submit: fake_submit,
    };

    /// 「控制报文却回 pending」的坏插件（A4：宿主必须 fail-loud，不挂死）。
    extern "C" fn pending_submit(
        _key: RString,
        _req: RString,
        _atts: RVec<MailAttachment>,
    ) -> FfiFuture {
        extern "C" fn poll(_s: *mut std::ffi::c_void) -> i32 {
            0
        }
        extern "C" fn take(
            _s: *mut std::ffi::c_void,
        ) -> oj_plugin_ffi::RResult<oj_plugin_ffi::RBytes, RString> {
            oj_plugin_ffi::RResult::Err(RString::from("not ready"))
        }
        extern "C" fn free(_s: *mut std::ffi::c_void) {}
        FfiFuture {
            state: std::ptr::null_mut(),
            poll,
            take,
            free,
        }
    }
    static PENDING_MAIL: MailVtable = MailVtable {
        submit: pending_submit,
    };

    /// 顶层 `smtp:` 段：多 profile（默认/本地落盘）+ 白名单 + 并发参数。
    fn smtp_cfg() -> Config {
        Config {
            smtp: Some(
                serde_yaml::from_str(
                    "workers: 2\n\
                     queue_capacity: 8\n\
                     default:\n  host: smtp.example.com\n  port: 465\n  tls: tls\n  \
                     mechanism: login\n  user: u\n  pass: p\n  \
                     allowed_from: [noreply@x.com]\n  allowed_recipients: [\"@x.com\"]\n\
                     mock:\n  host: localhost\n  port: 25\n  tls: none\n  allow_none_tls: true\n  \
                     mechanism: login\n  file_transport: /tmp/oj-mail-assembly-eml\n",
                )
                .unwrap(),
            ),
            ..Default::default()
        }
    }

    /// Given: 顶层 `smtp:` 段（多 profile + 白名单）+ oj-mail 插件在册；
    /// When: 装配期构造 mail 后端；
    /// Then: `Extras.mail` 注入可用后端 —— 宿主校验面与插件 cfg **同源**，
    /// JS 全局 `mail.send` 走通「宿主校验 → vtable」全链（越白名单仍 code:5）。
    #[tokio::test(flavor = "current_thread")]
    async fn given_smtp_configured_when_assemble_then_mail_backend_injected() {
        let cfg = smtp_cfg();
        let bus: Arc<dyn EventBroker> = Arc::new(only_js::bridge::Bus::new());
        let mb = build_mail_backend(&cfg, &serde_json::Value::Null, Some(&FAKE_MAIL), bus)
            .unwrap()
            .expect("smtp 段 + 插件在册 → 必须注入 mail 后端");
        // 非密钥面：profile 名单 / 白名单（宿主校验依据）。
        assert_eq!(mb.config().profile_keys(), vec!["default", "mock"]);
        let p = mb.config().profile("default").unwrap();
        assert_eq!(p.allowed_from, vec!["noreply@x.com".to_string()]);
        assert_eq!(p.allowed_recipients, vec!["@x.com".to_string()]);
        // mock profile 未声明白名单 → 空表（fail-closed 的判定依据）。
        assert!(mb.config().profile("mock").unwrap().allowed_from.is_empty());

        // 经 Extras → StableState → JS 全局：装配产物真的能服务 `mail.*`。
        let b = Bridge::with_dbs_and_loader(
            std::collections::HashMap::new(),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            None,
            Extras {
                mail: Some(mb.clone()),
                ..Default::default()
            },
        );
        let cap = b
            .run_with(
                r#"(async () => {
                    const ok = await mail.send({ from: "noreply@x.com", to: ["a@x.com"], text: "hi" });
                    const bad = await new Mail("default").send({ from: "evil@y.com", to: ["a@x.com"], text: "hi" });
                    json.ok({ ok, bad });
                  })().catch((e) => json.ok({ err: String(e) }));
                  "#,
                RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(v["data"].get("err").is_none(), "{v}");
        assert_eq!(v["data"]["ok"]["code"], 0, "{v}");
        assert_eq!(v["data"]["ok"]["data"]["jobId"], "j-default", "{v}");
        assert_eq!(v["data"]["bad"]["code"], 5, "{v}");
        assert!(
            v["data"]["bad"]["msg"]
                .as_str()
                .unwrap()
                .contains("allowed_from"),
            "{v}"
        );

        // 直接编排（同一后端）：白名单放行的那封同样到插件（key 过线）。
        let env = handle_send(
            mb.clone(),
            Arc::new(BlobRegistry::new()),
            None,
            "mock",
            &serde_json::json!({
                "from": "anyone@localhost", "to": ["a@localhost"], "text": "x",
            })
            .to_string(),
            MailMode::Send,
            &CallCtx::default(),
        )
        .await;
        // mock profile 白名单为空 → fail-closed（即便 profile 存在也拒绝）。
        assert_eq!(env["code"], 5, "{env}");
        assert!(
            env["msg"].as_str().unwrap().contains("allowed_from"),
            "{env}"
        );
    }

    /// Given: 未配 `smtp:`（或空段）／配了但插件未加载；
    /// Then: 不挂后端（None）——`mail.*` 调用报 "mail not configured"
    /// （与 es/auth 的「未配置」语义一致；不静默降级成空 profile 的谜之 code:5）。
    #[tokio::test(flavor = "current_thread")]
    async fn given_no_smtp_or_no_plugin_when_assemble_then_no_backend() {
        let bus = || -> Arc<dyn EventBroker> { Arc::new(only_js::bridge::Bus::new()) };
        // 段缺省。
        assert!(
            build_mail_backend(
                &Config::default(),
                &serde_json::Value::Null,
                Some(&FAKE_MAIL),
                bus()
            )
            .unwrap()
            .is_none()
        );
        // 空段（`smtp: {}`，零 profile）= 未配置。
        let empty = Config {
            smtp: Some(Default::default()),
            ..Default::default()
        };
        assert!(
            build_mail_backend(&empty, &serde_json::Value::Null, Some(&FAKE_MAIL), bus())
                .unwrap()
                .is_none()
        );
        // 配了段但 oj-mail 未加载（`Registrations.mail = None`）。
        assert!(
            build_mail_backend(&smtp_cfg(), &serde_json::Value::Null, None, bus())
                .unwrap()
                .is_none()
        );
    }

    /// Given: 配置写错（`allowed_from` 是数字数组，经 `plugins.mail` 透传进来）；
    /// When: 装配；Then: **装配期** Err —— 不静默变成「无白名单 → 全部拒绝」。
    /// （透传与 `smtp:` 适配器走同一份 JSON，故两条路的校验面同源。）
    #[tokio::test(flavor = "current_thread")]
    async fn given_malformed_whitelist_when_assemble_then_err() {
        let mut cfg = Config::default();
        cfg.plugins.insert(
            "mail".into(),
            serde_json::json!({
                "default": {
                    "host": "h", "port": 25, "tls": "none", "allow_none_tls": true,
                    "mechanism": "login", "allowed_from": [1, 2],
                }
            }),
        );
        let e = match build_mail_backend(
            &cfg,
            &serde_json::Value::Null,
            Some(&FAKE_MAIL),
            Arc::new(only_js::bridge::Bus::new()),
        ) {
            Ok(_) => panic!("allowed_from 写错必须在装配期报错"),
            Err(e) => e,
        };
        assert!(e.contains("allowed_from"), "{e}");
    }

    // ---- ldap 装配测试 ----

    /// 假 ldap vtable：call 回固定结果（装配测试零网络、零插件）。
    extern "C" fn fake_ldap_call(req: RString) -> FfiFuture {
        let r = req[..].to_string();
        let out = if r.contains("\"op\":\"bind\"") {
            "true"
        } else {
            "\"dn:fake\""
        };
        oj_plugin_ffi::ready_ok(out.as_bytes().to_vec())
    }
    static FAKE_LDAP: oj_plugin_ffi::LdapVtable = oj_plugin_ffi::LdapVtable {
        call: fake_ldap_call,
    };

    /// 顶层 `ldap:` 段（不透明 yaml map）：双实例（default 服务账号 / ad ldaps）。
    fn ldap_cfg() -> Config {
        Config {
            ldap: Some(serde_yaml::from_str(
                "default:\n  url: ldap://dc.example.com:389\n  bind_dn: cn=svc,dc=example,dc=com\n  bind_pw: s\n  timeout_ms: 3000\nad:\n  url: ldaps://ad.internal:636\n",
            )
            .unwrap()),
            ..Config::default()
        }
    }

    /// Given: 未配 `ldap:`（或空段）／配了但插件未加载；
    /// Then: 不挂后端（None）——`ldap.*` 报 "ldap not configured"（与 mail 同语义）。
    #[tokio::test(flavor = "current_thread")]
    async fn given_no_ldap_or_no_plugin_when_assemble_then_no_backend() {
        assert!(
            build_ldap_backend(
                &Config::default(),
                &serde_json::Value::Null,
                Some(&FAKE_LDAP)
            )
            .unwrap()
            .is_none()
        );
        let empty = Config {
            ldap: Some(serde_yaml::Value::Mapping(Default::default())),
            ..Default::default()
        };
        assert!(
            build_ldap_backend(&empty, &serde_json::Value::Null, Some(&FAKE_LDAP))
                .unwrap()
                .is_none()
        );
        assert!(
            build_ldap_backend(&ldap_cfg(), &serde_json::Value::Null, None)
                .unwrap()
                .is_none()
        );
    }

    /// Given: 合法 `ldap:` 段 + 插件在册；Then: 后端就位，实例表与段一致，
    /// 且调用确实经 vtable 过线（bind → true）。
    #[tokio::test(flavor = "current_thread")]
    async fn given_ldap_section_when_assemble_then_backend_serves_calls() {
        let b = build_ldap_backend(&ldap_cfg(), &serde_json::Value::Null, Some(&FAKE_LDAP))
            .unwrap()
            .expect("ldap 段 + 插件在册 → 后端就位");
        assert_eq!(
            b.config().instance_keys(),
            vec!["ad".to_string(), "default".to_string()]
        );
        let v = b
            .call(serde_json::json!({"op":"bind","key":"default","dn":"u","pw":"p"}))
            .await
            .unwrap();
        assert_eq!(v, serde_json::json!(true));
    }

    /// Given: 配置写错（未知键/坏 url，经 `plugins.ldap` 透传进来）或双配置源皆非空；
    /// Then: 装配期 Err —— 不静默忽略拼错的配置，不在透传/适配器间静默遮蔽。
    #[tokio::test(flavor = "current_thread")]
    async fn given_bad_ldap_cfg_when_assemble_then_err() {
        let mut cfg = Config::default();
        cfg.plugins.insert(
            "ldap".into(),
            serde_json::json!({"default": {"url": "http://x"}}),
        );
        assert!(build_ldap_backend(&cfg, &serde_json::Value::Null, Some(&FAKE_LDAP)).is_err());
        // 双配置源：`ldap:` 段与 `plugins.ldap` 皆非空 → Err（二选一）。
        let mut both = ldap_cfg();
        both.plugins.insert(
            "ldap".into(),
            serde_json::json!({"other": {"url": "ldap://x:389"}}),
        );
        let e = match build_ldap_backend(&both, &serde_json::Value::Null, Some(&FAKE_LDAP)) {
            Ok(_) => panic!("双配置源必须在装配期报错"),
            Err(e) => e,
        };
        assert!(e.contains("pick one"), "{e}");
    }

    /// A4：停机排空 —— `drain_mail_backend` 必须把**控制报文**发给插件
    /// （`{"__ctl":"drain","timeout_ms":N}`），并把插件的排空信封解回。
    #[tokio::test(flavor = "current_thread")]
    async fn given_mail_backend_when_drain_then_control_message_sent_and_envelope_returned() {
        let bus: Arc<dyn EventBroker> = Arc::new(only_js::bridge::Bus::new());
        let mb = build_mail_backend(&smtp_cfg(), &serde_json::Value::Null, Some(&FAKE_MAIL), bus)
            .unwrap()
            .expect("smtp 段 + 插件在册 → 后端就位");
        SEEN_REQS.lock().unwrap().clear();

        let v = drain_mail_backend(&mb, Duration::from_millis(1234));
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(v["data"]["drained"], true, "{v}");
        assert_eq!(v["data"]["workers"], 2, "{v}");

        let reqs = SEEN_REQS.lock().unwrap().clone();
        assert_eq!(reqs.len(), 1, "停机排空只发一次控制报文");
        let req: serde_json::Value = serde_json::from_str(&reqs[0]).unwrap();
        assert_eq!(req["__ctl"], "drain", "{req}");
        assert_eq!(req["timeout_ms"], 1234, "超时必须过线（插件据此等）：{req}");
    }

    /// A4：坏插件（控制报文回 pending）→ 排空失败收敛为 `code:1` 信封（停机路径只告警，
    /// **不 panic、不挂死**、不阻断进程退出）。
    #[tokio::test(flavor = "current_thread")]
    async fn given_broken_plugin_when_drain_then_failure_becomes_envelope() {
        let bus: Arc<dyn EventBroker> = Arc::new(only_js::bridge::Bus::new());
        let mb = build_mail_backend(
            &smtp_cfg(),
            &serde_json::Value::Null,
            Some(&PENDING_MAIL),
            bus,
        )
        .unwrap()
        .expect("后端就位");
        let v = drain_mail_backend(&mb, Duration::from_millis(10));
        assert_eq!(v["code"], 1, "{v}");
        assert_eq!(v["data"]["drained"], false, "{v}");
        assert!(
            v["msg"].as_str().unwrap().contains("drain 调用失败"),
            "文案须点明排空失败：{v}"
        );
    }
}

#[cfg(test)]
mod prewarm_boot_tests {
    use super::*;

    /// Given: ext_boot.js 合法（仅赋值全局）；When: prewarm_boot；Then: Ok —— boot 模块
    /// 借真实 runtime 加载执行成功（与生产同路径：独立线程 + current_thread runtime）。
    #[test]
    fn given_valid_ext_boot_when_prewarm_then_ok() {
        let dir = std::env::temp_dir().join(format!("oj-prewarm-ok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ext_boot.js"), "globalThis.__oj_booted = true;\n").unwrap();
        let spec = ext_boot_spec(&dir).unwrap().unwrap();
        let loader = Arc::new(LoaderShared {
            project_root: dir.canonicalize().unwrap(),
            ts: true,
        });
        let r = prewarm_boot(move || {
            Bridge::with_dbs_and_loader(
                std::collections::HashMap::new(),
                Arc::new(InMemoryKV::new()),
                SchemaRegistry::new(),
                false,
                Some(loader.clone()),
                Extras {
                    boot: Some(spec.clone()),
                    ..Default::default()
                },
            )
        });
        assert!(r.is_ok(), "{r:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Given: boot 指向不存在的模块；When: prewarm_boot；Then: Err 点名 ext_boot 并附
    /// 重启提示（boot 错误前移到装配期 = 真·启动失败，而非「路由全空、服务照常监听」）。
    #[test]
    fn given_missing_boot_module_when_prewarm_then_err_named_ext_boot() {
        let r = prewarm_boot(|| {
            Bridge::with_dbs_and_loader(
                std::collections::HashMap::new(),
                Arc::new(InMemoryKV::new()),
                SchemaRegistry::new(),
                false,
                None,
                Extras {
                    boot: Some("file:///oj/ext_boot_missing_deadbeef.js".into()),
                    ..Default::default()
                },
            )
        });
        let Err(e) = r else {
            panic!("expected prewarm failure for missing boot module");
        };
        assert!(e.contains("ext_boot"), "{e}");
        assert!(e.contains("restart the process"), "{e}");
    }

    /// Given: make_bridge 闭包 panic；When: prewarm_boot；Then: Err 收敛（线程 join
    /// 接住 panic，宿主不 abort，文案点名 panicked）。
    #[test]
    fn given_bridge_factory_panics_when_prewarm_then_err() {
        let r = prewarm_boot(|| panic!("boom"));
        let Err(e) = r else {
            panic!("expected panic-to-err convergence");
        };
        assert!(e.contains("panicked"), "{e}");
    }
}
