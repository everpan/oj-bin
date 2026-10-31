// `async-trait` 宏（0.1.x）为每个 async 方法生成 `#[must_use]`，而返回类型
// `Pin<Box<dyn Future>>` 本身已 `#[must_use]`；clippy 1.99 的 `double_must_use`
// 据此报红。属宏误报——本仓不手动为 future 标注 `#[must_use]`，统一在此屏蔽。
#![allow(clippy::double_must_use)]

//! mdm-server：HTTP 层。目录镜像路由（routes）+ JS actor 线程桥（actor）+ axum 装配（本文件）。

pub mod actor;
pub mod certificate;
pub mod certificate_watcher;
/// API dispatch 层（admit/执行/响应映射，自 lib.rs R2 分层拆出）。
pub mod dispatch;
pub mod logging;
/// OpenAPI spec → MCP tool 列表（纯函数；MCP 双线路共享核心 R3/R4）。
pub mod mcp;
/// `POST {base}/mcp`（MCP streamable HTTP stateless 子集）与 `{base}/rpc`（裸 JSON-RPC）。
pub mod mcp_http;
/// meta 注入层（head meta/缓存/冲突剥离）。
pub mod meta_inject;
/// multipart/blob 上传层（流式解析/直传/守卫）。
pub mod multipart;
/// 路由表 → OpenAPI 3.1 生成（自 oj 迁入，MCP 双线路 R1；字节级不变约束）。
pub mod openapi;
/// 配置热更新传输层（v1）：外层单一 fallback + watch 通道 + reload_api 门禁。
pub mod reload;
pub mod routes;
/// 静态资源层（web 服务/etag/range/blob key）。
pub mod static_files;
pub mod tasks;
/// 测试支撑（仅 dev/test 编译）：生成真实签名 JWS 证书，供装配测试使用。
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
pub mod ws;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::Response;
use axum::routing::any;
use serde_json::Value;
use std::time::Duration;

use tower_http::cors::{AllowHeaders, AllowMethods, AllowOrigin, Any, CorsLayer};

use crate::actor::JsActor;
use crate::routes::{Lookup, RouteTable, Routes};
use only_js::bridge::{
    AuthGuard, BlobBackend, BlobServed, PluginInfo, RequestInfo, UploadedFile, fail,
};
use only_js::config::CorsCfg;

/// 证书状态
#[derive(Clone, Debug)]
pub enum CertificateStatus {
    /// 证书有效
    Valid,
    /// 证书宽限期内，剩余秒数
    Grace { remaining_secs: u64 },
    /// 证书已过期
    Expired,
}

/// 运行时挂载（v0.1.58 mounts 模型）：api（路由表 + dev 兜底 + 可选配对 web）
/// 或 web（纯静态）。`prefix` 已归一（首斜杠、无尾斜杠、`/` 允许）；app() 内按
/// 前缀长度降序（同长字符串升序）排序，请求期取最长命中，**跨挂载不回落**。
// Api 腿携带 RouteTable（与旧 AppState.table 同一暴露面，Clone 成本不变），
// 与 Web 腿的体积差是模型固有形状，非误用。
#[allow(clippy::large_enum_variant)]
#[derive(Clone)]
pub enum RuntimeMount {
    Api {
        prefix: String,
        table: RouteTable,
        /// dev（ts）文件系统兜底：表 miss 时回退目录镜像；release None。
        fallback: Option<Routes>,
        /// 同 prefix 配对的 web 挂载（api 优先；该 web **不开** SPA 回落——
        /// 规则 2：最长前缀命中已使拼错路径直接落 api 挂载，回落吞不掉也不该吞）。
        web: Option<WebMount>,
    },
    Web(WebMount),
}

/// 静态站点（URL 前缀 → 磁盘目录映射；装配期已 canonicalize）。
#[derive(Clone)]
pub struct WebMount {
    pub prefix: String,
    pub root: PathBuf,
    /// 该站点的自定义响应头（v0.1.30；覆盖全局同名头，框架自有头仍优先）。
    pub headers: Vec<(String, String)>,
    /// SPA 深链接回落：显式 `spa: true` 才开（默认 false——静默把 404 变 200 会
    /// 掩盖错配，继承 v0.1.20 立论）。配对 web 恒 false。
    pub spa: bool,
}

impl RuntimeMount {
    /// 挂载前缀（两种形态同口径）。
    fn prefix(&self) -> &str {
        match self {
            RuntimeMount::Api { prefix, .. } => prefix,
            RuntimeMount::Web(w) => &w.prefix,
        }
    }
}

/// 共享状态（JsActor 句柄 Clone = 同一 actor 队列的多份引用）。
#[derive(Clone)]
pub struct AppState {
    /// 挂载表（app() 内已按前缀长度降序排好，取最长命中）。空 = 全 404。
    mounts: Vec<RuntimeMount>,
    actor: JsActor,
    /// 单请求超时（None = 不限时）。
    timeout: Option<std::time::Duration>,
    /// 静态增强（v0.1.25：per-route meta + HTML 缓存头；v0.1.30：全局响应头）。
    static_opts: StaticOpts,
    /// handle() 前置管线（OJ-3..5 单一扩展点；后续阶段只加字段）。
    pipeline: Pipeline,
    /// 当前证书状态（热加载共享，可用 RwLock 原子更新）
    pub certificate_status: Arc<RwLock<CertificateStatus>>,
    /// 证书有效期截止时间（热加载共享）
    pub certificate_valid_until: Arc<RwLock<Option<std::time::SystemTime>>>,
    /// 已装配插件自描述清单（`{base}/plugins` 数据源；装配层注入）。
    pub plugins: Arc<Vec<PluginInfo>>,
    /// REQ-11 G2：进程级执行登记表（与 StableState.exec_sessions 同一 Arc）。
    /// None = 无登记表（挂断联动不挂；池执行仍可由 db.cancel 取消）。
    pub exec_registry: Option<Arc<only_js::bridge::db_exec::ExecRegistry>>,
}

/// 静态增强（v0.1.25 起：动态 meta 与 HTML 缓存头；SPA 回落已下沉 per-挂载 `spa`）。
#[derive(Clone, Default)]
pub struct StaticOpts {
    /// 路由感知 meta 目录名（相对各 web 挂载根）：送 HTML 前按路径查
    /// `<root>/<dir>/<path>.json` 注入 `<title>`/`<meta>`。None = 不注入。
    pub html_meta: Option<String>,
    /// 动态 meta handler 路由（`server.html_meta_handler`，v0.1.25）：静态 JSON 打底后
    /// 按其返回的 JSON 覆盖注入（同一白名单）。None = 只吃静态 JSON。
    pub html_meta_handler: Option<String>,
    /// HTML 响应 Cache-Control（`server.html_cache_control`，v0.1.25）：None = 不加头。
    /// 动态 handler 返回的 `cache_control` 优先。
    pub html_cache_control: Option<String>,
    /// 自定义响应头全局默认（`server.response_headers`，v0.1.30）：施加于动态信封、
    /// 静态站点、blob 下载/直传响应；与框架自有头冲突时框架优先（只补缺），
    /// 静态站点可用 per-site `headers` 覆盖同名的全局值。默认空 = 行为不变。
    /// 挂在 StaticOpts 是历史签名约束（app() 参数面不变），语义上是全站响应策略。
    pub response_headers: Vec<(String, String)>,
}

/// handle() 前置管线配置：请求进入 JS 前的注入/守卫（租户/鉴权/上传）。
#[derive(Clone)]
pub struct Pipeline {
    /// Some(header) = 租户启用：缺失/空 → 400，命中 → http.tenantId。
    pub tenant_header: Option<String>,
    /// PR-9（v0.1.41）：租户只认验签 claim（claims.tenant）；裸头仅作一致性复核。
    pub tenant_require_signed_claim: bool,
    /// 跳转腿豁免（tenant.anonymous_paths；命中则免租户头——OIDC 302 带不了自定义头）。
    pub tenant_anon: Vec<String>,
    /// Some = 鉴权启用：Bearer 守卫 + http.user（实现由 oj-auth 插件提供）。
    pub auth: Option<Arc<dyn AuthGuard>>,
    /// 上传/请求体上限（超限 413 信封）；axum body limit = 2x（超 2x 裸 413，ponytail: 接受）。
    pub max_upload: u64,
    /// blob 直传 PUT 路由（v0.1.30）体积上限（Pipeline::default = 1 GiB，与 config
    /// 默认 `blob_upload_max_bytes` 同值）——直传不经 JsActor，与 handler 面分开计价。
    pub blob_upload_max: u64,
    /// 路由级 timeout 覆盖（v0.1.30）：(pattern, duration)，按声明顺序首个命中生效；
    /// pattern 段语义同 `path_matches`（字面 / `*` 一段 / `**` 跨段），匹配全路径（含 base）。
    pub route_timeouts: Vec<(String, std::time::Duration)>,
    /// Some = blob 启用：`{base}/blob/{key}` 公开下载（local 直出 / s3 302 presign）；
    /// PUT 同一前缀 = 守卫保护下的直传上传（v0.1.30）。
    pub blob: Option<Arc<dyn BlobBackend>>,
    /// Some = MCP 启用：挂 `{base}/mcp`（streamable HTTP，spec 2026-10-30 R7/R8/R9）。
    /// None = 不挂（默认）。modules 白名单 fail-closed；装配期已做无 auth fail-fast。
    pub mcp: Option<McpConfig>,
    /// true = 挂 `{base}/rpc`（裸 JSON-RPC 2.0，method=operationId）。独立门。
    pub rpc: bool,
}

/// MCP 端点配置（serve 侧载体；config.rs 的 `McpCfg` 在装配层转来）。
#[derive(Clone, Debug, Default)]
pub struct McpConfig {
    /// 模块/tag 白名单（fail-closed）：Some 时仅保留 tags 有交集的 tool。
    pub modules: Option<Vec<String>>,
    /// 装配期已消费（无 auth fail-fast 在 oj/src/app.rs）；运行期仅作记录。
    pub allow_anonymous: bool,
    /// oj 版本号（MCP `initialize` 的 serverInfo.version，与 `oj --version` 同源；
    /// 装配层自 oj crate 的 CARGO_PKG_VERSION 注入）。
    pub server_version: String,
}

impl Default for Pipeline {
    fn default() -> Self {
        Self {
            tenant_header: None,
            tenant_require_signed_claim: false,
            tenant_anon: Vec::new(),
            auth: None,
            max_upload: 10 * 1024 * 1024,        // 10MiB
            blob_upload_max: 1024 * 1024 * 1024, // 1GiB
            route_timeouts: Vec::new(),
            blob: None,
            mcp: None,
            rpc: false,
        }
    }
}

/// 路径通配匹配（v0.1.20 升级：字面 / `*` 单段 / `**` 跨段；尾 `/*` 仍是严格一层）。
///
/// 三种模式（逐段比对，段由 `/` 切分、忽略空段与尾斜杠）：
/// - 字面对等：`/health` 只命中 `/health`；
/// - `*`：匹配**恰好一个**非空段 —— `/oidc/*` 命中 `/oidc/callback`，**不**命中裸
///   前缀 `/oidc`、也**不**命中两层 `/oidc/a/b`（维持 v0.1.19 的严格一层语义：放宽等于
///   静默扩大免租户/免鉴权面；要深路径请显式写 `**`）；
/// - `**`：匹配**零个或多个**段 —— `/public/**` 命中 `/public`、`/public/a`、
///   `/public/a/b`；中段 `*` 可写 `/public/anchor/*/states`。
///
/// 与 `plugins/oj-auth` 的 `is_anonymous` 是**同一语义的两份实现**（插件不能依赖 server
/// crate，两处各自持有，注释互指）——改任一侧都必须同步另一侧与两侧的单测矩阵。
pub fn path_matches(list: &[String], path: &str) -> bool {
    let seg = split_segments(path);
    list.iter()
        .any(|p| segments_match(&split_segments(p), &seg))
}

/// 路径 → 段（去空段与尾斜杠；`"/a/b/"` → `["a","b"]`）。
fn split_segments(path: &str) -> Vec<&str> {
    path.split('/').filter(|s| !s.is_empty()).collect()
}

/// 段序列匹配（`*` 单段 / `**` 跨段；`**` 用回溯试 0..=n 段）。
fn segments_match(pat: &[&str], seg: &[&str]) -> bool {
    match (pat.first(), seg.first()) {
        (None, None) => true,
        (None, Some(_)) => false,
        (Some(p), _) if *p == "**" => (0..=seg.len()).any(|k| segments_match(&pat[1..], &seg[k..])),
        (Some(_), None) => false,
        (Some(p), Some(s)) => (*p == *s || *p == "*") && segments_match(&pat[1..], &seg[1..]),
    }
}

/// 构造 axum 应用：catch-all fallback（`All("/*")` 语义）。
/// `mounts` 按前缀长度降序（同长字符串升序）重排后入 state；每个 api 挂载注册
/// `{prefix}/health` 与 `{prefix}/plugins` 两条真实路由（公共基础设施端点：不走
/// Bearer 守卫 / 证书 GET 门禁，匿名可访问，保留路径遮蔽同名业务路由）。
/// 与 v0.1.57 零 URL 变化：今日即注册在 `{base}/health` 等，base = api 挂载 prefix。
#[allow(clippy::too_many_arguments)]
pub fn app(
    mounts: Vec<RuntimeMount>,
    actor: JsActor,
    timeout: Option<std::time::Duration>,
    // static_opts：per-route meta + HTML 缓存头 + 全局响应头（SPA 回落已下沉挂载 `spa`）。
    static_opts: StaticOpts,
    pipeline: Pipeline,
    certificate_status: Arc<RwLock<CertificateStatus>>,
    certificate_valid_until: Arc<RwLock<Option<std::time::SystemTime>>>,
    plugins: Arc<Vec<PluginInfo>>,
    // CORS（v0.1.35）：段存在即启用；None = 不挂层（行为完全不变）。
    cors: Option<CorsCfg>,
) -> Router {
    app_with_exec(
        mounts,
        actor,
        timeout,
        static_opts,
        pipeline,
        certificate_status,
        certificate_valid_until,
        plugins,
        None,
        cors,
    )
}

/// [`app`] 的 REQ-11 G2 变体：注入进程级执行登记表（与 StableState.exec_sessions
/// 同一 Arc），挂断联动（DisconnectGuard）据此触达在飞池执行。None = 不挂 guard。
#[allow(clippy::too_many_arguments)]
pub fn app_with_exec(
    mounts: Vec<RuntimeMount>,
    actor: JsActor,
    timeout: Option<std::time::Duration>,
    static_opts: StaticOpts,
    pipeline: Pipeline,
    certificate_status: Arc<RwLock<CertificateStatus>>,
    certificate_valid_until: Arc<RwLock<Option<std::time::SystemTime>>>,
    plugins: Arc<Vec<PluginInfo>>,
    exec_registry: Option<Arc<only_js::bridge::db_exec::ExecRegistry>>,
    cors: Option<CorsCfg>,
) -> Router {
    let mut router = Router::new();
    for m in mounts
        .iter()
        .filter(|m| matches!(m, RuntimeMount::Api { .. }))
    {
        let p = m.prefix().trim_end_matches('/');
        router = router
            .route(&format!("{p}/health"), axum::routing::get(health_handler))
            .route(&format!("{p}/plugins"), axum::routing::get(plugins_handler));
        // MCP / 裸 JSON-RPC 端点（spec 2026-10-30，配置门控，默认不挂）。
        if pipeline.mcp.is_some() {
            router = router.route(
                &format!("{p}/mcp"),
                axum::routing::post(crate::mcp_http::handle),
            );
            // 保留路径遮蔽告警（与 /health、/plugins 同款语义）。
            if let RuntimeMount::Api { table, .. } = m {
                let reserved = format!("{p}/mcp");
                if table
                    .patterns()
                    .iter()
                    .any(|k| k.trim_end_matches('/') == reserved.as_str())
                {
                    tracing::warn!(
                        "reserved path {reserved} shadows a business route of the same path"
                    );
                }
            }
        }
        if pipeline.rpc {
            router = router.route(
                &format!("{p}/rpc"),
                axum::routing::post(crate::mcp_http::rpc_handle),
            );
            if let RuntimeMount::Api { table, .. } = m {
                let reserved = format!("{p}/rpc");
                if table
                    .patterns()
                    .iter()
                    .any(|k| k.trim_end_matches('/') == reserved.as_str())
                {
                    tracing::warn!(
                        "reserved path {reserved} shadows a business route of the same path"
                    );
                }
            }
        }
    }
    router = router
        .fallback(any(handle))
        // 请求日志中间件（method/path/status/耗时 → 文件日志 + stderr）。
        .layer(axum::middleware::from_fn(crate::logging::log_requests));
    // CORS 层放在最外层（先于 handle 与日志中间件）：预检由本层短路（204 + CORS 头），
    // 不进入路由；简单请求补 Access-Control-* 响应头。
    if let Some(cfg) = cors {
        router = router.layer(build_cors_layer(&cfg));
    }
    router.with_state(AppState {
        mounts: sorted_mounts(mounts),
        actor,
        timeout,
        static_opts,
        pipeline,
        certificate_status,
        certificate_valid_until,
        plugins,
        exec_registry,
    })
}

/// 由 `CorsCfg` 构造 `CorsLayer`（v0.1.35）。
/// - `origins` 空 → 允许任意源（Any）；非空 → 显式源列表。
/// - `methods`/`headers` 空 → tower-http 默认（反射/标准方法）。
/// - `credentials: true` 时调用方（oj/src/app.rs）必须已确保 `origins` 非空，否则
///   tower-http 运行期 panic；此处仅按值装配。
/// - `max_age`/`expose` 可选。
fn build_cors_layer(cfg: &CorsCfg) -> CorsLayer {
    let mut layer = CorsLayer::new();
    layer = if cfg.origins.is_empty() {
        layer.allow_origin(Any)
    } else {
        let origins = cfg
            .origins
            .iter()
            .filter_map(|o| o.parse::<HeaderValue>().ok())
            .collect::<Vec<_>>();
        layer.allow_origin(AllowOrigin::list(origins))
    };
    if !cfg.methods.is_empty() {
        let methods = cfg
            .methods
            .iter()
            .filter_map(|m| m.parse::<Method>().ok())
            .collect::<Vec<_>>();
        layer = layer.allow_methods(AllowMethods::list(methods));
    }
    if !cfg.headers.is_empty() {
        let headers = cfg
            .headers
            .iter()
            .filter_map(|h| h.parse::<HeaderName>().ok())
            .collect::<Vec<_>>();
        layer = layer.allow_headers(AllowHeaders::list(headers));
    }
    if cfg.credentials {
        layer = layer.allow_credentials(true);
    }
    if let Some(age) = cfg.max_age {
        layer = layer.max_age(Duration::from_secs(age));
    }
    if !cfg.expose.is_empty() {
        let expose = cfg
            .expose
            .iter()
            .filter_map(|h| h.parse::<HeaderName>().ok())
            .collect::<Vec<HeaderName>>();
        layer = layer.expose_headers(expose);
    }
    layer
}

/// 挂载表排序（v0.1.58 mounts 模型，语义沿用 v0.1.27 多站点）：前缀长度降序
/// （最长命中优先），同长按字符串升序保确定性。归 app() 独家负责——装配层
/// （oj/src/app.rs）只归一/去重/配对，不排序。
fn sorted_mounts(mut mounts: Vec<RuntimeMount>) -> Vec<RuntimeMount> {
    mounts.sort_by(|a, b| {
        b.prefix()
            .len()
            .cmp(&a.prefix().len())
            .then_with(|| a.prefix().cmp(b.prefix()))
    });
    mounts
}

/// 健康检查：返回服务状态与证书状态（供监控轮询）。
/// 此路由在证书 GET 限制之前注册，故即使证书进入宽限期/失效仍可访问，
/// 以便 Prometheus/Grafana 及时发现。
async fn health_handler(State(st): State<AppState>) -> Response {
    let status_guard = st
        .certificate_status
        .read()
        .expect("certificate_status lock poisoned");
    let (status_str, grace_remaining_secs) = match &*status_guard {
        CertificateStatus::Valid => ("valid", None),
        CertificateStatus::Grace { remaining_secs } => ("grace", Some(*remaining_secs)),
        CertificateStatus::Expired => ("expired", None),
    };
    let expiry = st
        .certificate_valid_until
        .read()
        .expect("certificate_valid_until lock poisoned")
        .map(|t| chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339())
        .unwrap_or_default();
    drop(status_guard);
    let body = serde_json::json!({
        "status": "OK",
        "certificate_status": status_str,
        "certificate_expiry": expiry,
        "grace_remaining_secs": grace_remaining_secs,
    });
    let mut r = Response::new(axum::body::Body::from(serde_json::to_vec(&body).unwrap()));
    r.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    r
}

/// 插件清单查询（公共基础设施端点，与 /health 同位：监控/运维可匿名访问）。
/// 返回装配插件的自描述（name/version/description/abi/fingerprint）。
async fn plugins_handler(State(st): State<AppState>) -> Response {
    let data = serde_json::to_value(&*st.plugins).unwrap_or_else(|_| Value::Null);
    let mut r = Response::new(axum::body::Body::from(only_js::bridge::ok(&data)));
    r.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    r
}

/// 已绑定监听上服务（测试/T11：先 bind 端口 0 再读 local_addr）。
#[allow(clippy::too_many_arguments)]
pub async fn serve_with_listener(
    listener: tokio::net::TcpListener,
    mounts: Vec<RuntimeMount>,
    actor: JsActor,
    timeout: Option<std::time::Duration>,
    static_opts: StaticOpts,
    pipeline: Pipeline,
    cors: Option<CorsCfg>,
) -> std::io::Result<()> {
    serve_router(
        listener,
        app(
            mounts,
            actor,
            timeout,
            static_opts,
            pipeline,
            Arc::new(RwLock::new(CertificateStatus::Valid)),
            Arc::new(RwLock::new(None)),
            Arc::default(),
            cors,
        ),
        // 测试路径：永不触发的停机信号（保持原行为）。
        std::future::pending(),
    )
    .await
}

/// 已绑定监听 + 完整 Router 服务（oj server 生产路径：app().merge(ws) 后经此起服务）。
/// `shutdown` resolve 后停止接受新连接并排空在途请求（axum with_graceful_shutdown）。
pub async fn serve_router(
    listener: tokio::net::TcpListener,
    router: axum::Router,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await
}

async fn handle(State(st): State<AppState>, req: axum::extract::Request) -> Response {
    let (parts, body) = req.into_parts();
    let method = parts.method;
    let uri = parts.uri;
    let headers = parts.headers;
    let verb = method.as_str();
    // Accept 判据先算：`headers` 稍后被 run 闭包整体捕获（SPA 回落要用）。
    let accept_html = wants_html(&headers);

    // Certificate validation: restrict GET requests when certificate is expired or in grace period
    if verb == "GET" {
        match &*st
            .certificate_status
            .read()
            .expect("certificate_status lock poisoned")
        {
            CertificateStatus::Expired => {
                return fail_response(403, "certificate expired: service unavailable");
            }
            CertificateStatus::Grace { remaining_secs: _ } => {
                return fail_response(
                    403,
                    "certificate expired: service available in grace period, but GET requests are restricted",
                );
            }
            CertificateStatus::Valid => {}
        }
    }

    // 挂载分发（v0.1.58 mounts 模型）：最长前缀命中（表已按前缀长度降序排序），
    // **跨挂载不回落**；未命中任何挂载 → 404。请求体按路径分档限长的说明
    // （v0.1.30/v0.1.38）随执行腿下移：blob 直传腿流式消费（内存恒定），其余腿在
    // run_route 内缓冲（上限 2x max_upload，超 max_upload 信封 413、超 2x 裸 413）。
    let Some((mount, rel)) = st
        .mounts
        .iter()
        .find_map(|m| strip_app_prefix(m.prefix(), uri.path()).map(|r| (m, r)))
    else {
        return fail_response(404, "no route matched");
    };
    match mount {
        RuntimeMount::Api {
            prefix,
            table,
            fallback,
            web,
        } => {
            handle_api(
                &st,
                prefix,
                table,
                fallback.as_ref(),
                web.as_ref(),
                rel,
                verb,
                &uri,
                &headers,
                body,
                accept_html,
            )
            .await
        }
        RuntimeMount::Web(w) => serve_web(&st, w, rel, verb, accept_html, &headers).await,
    }
}

/// api 挂载执行腿：blob 下载/直传 → 路由表 → dev 兜底 → 同 prefix 配对 web（无 SPA）
/// → 404 JSON。前置管线的 rel path = 剥命中挂载 prefix（多 api 挂载各自对齐匿名面）。
#[allow(clippy::too_many_arguments)]
async fn handle_api(
    st: &AppState,
    prefix: &str,
    table: &RouteTable,
    fallback: Option<&Routes>,
    web: Option<&WebMount>,
    rel: &str,
    verb: &str,
    uri: &axum::http::Uri,
    headers: &HeaderMap,
    body: axum::body::Body,
    accept_html: bool,
) -> Response {
    // 内置 blob 下载路由（{prefix}/blob/{key}，公开 GET，先于路由表，也先于前置管线）。
    //
    // **与匿名路径的耦合（互指注释）**：本分支（以及配对 web 静态腿）在 auth/tenant
    // 前置管线**之前**直接 return，因此 `anonymous_paths`（`Pipeline.tenant_anon` /
    // `AuthGuard::verify`）对它们**不产生任何效力**。启动期的匿名路径迁移 WARN
    // （`oj/src/app.rs` 的 `warn_legacy_tail_wildcards`）正是靠这一点把「影响面」收窄到
    // 「已注册路由」——**若把这两处提前返回改为走前置管线，或让它们咨询匿名表，必须同步
    // 该判定的前提**，否则 WARN 会静默失准。
    // v0.1.25 补第三处**同一前提**的分支：配对 web 的 meta 注入内部派发
    // `server.html_meta_handler`（见 `dispatch_meta_handler`）同样不经守卫（页面请求
    // 带不了 Bearer），故该 handler **不必**进 `anonymous_paths`；它被外部直接访问时
    // 仍受守卫约束（两侧口径不冲突）。
    // v0.1.30 补第四处**例外**：下方 PUT（直传上传）走守卫 + 租户准入（写面不能公开），
    // 只豁免**读取**语义不变。
    if verb == "GET"
        && let Some(blob) = st.pipeline.blob.as_ref()
        && let Some(key) = blob_key_of(prefix, uri.path())
        && let Some(key) = decode_blob_key(key)
    {
        let mut r = match blob.serve(&key).await {
            Ok(BlobServed::Bytes(bytes, ct)) => {
                // oj-7：local 内联直出支持单区间 Range（s3 302 腿不在此，presign URL
                // 由对象存储自己处理 seek）。
                let mut r = match parse_range(headers.get(axum::http::header::RANGE), bytes.len()) {
                    RangeSpec::Partial(start, end) => {
                        let mut r =
                            Response::new(axum::body::Body::from(bytes[start..=end].to_vec()));
                        *r.status_mut() = StatusCode::PARTIAL_CONTENT;
                        r.headers_mut().insert(
                            axum::http::header::CONTENT_RANGE,
                            axum::http::HeaderValue::from_str(&format!(
                                "bytes {start}-{end}/{}",
                                bytes.len()
                            ))
                            .unwrap_or(axum::http::HeaderValue::from_static("bytes 0-0/0")),
                        );
                        r
                    }
                    RangeSpec::Unsatisfiable => {
                        let mut r = Response::new(axum::body::Body::empty());
                        *r.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
                        r.headers_mut().insert(
                            axum::http::header::CONTENT_RANGE,
                            axum::http::HeaderValue::from_str(&format!("bytes */{}", bytes.len()))
                                .unwrap_or(axum::http::HeaderValue::from_static("bytes */0")),
                        );
                        r
                    }
                    RangeSpec::Full => Response::new(axum::body::Body::from(bytes)),
                };
                r.headers_mut().insert(
                    axum::http::header::ACCEPT_RANGES,
                    axum::http::HeaderValue::from_static("bytes"),
                );
                if let Some(ct) = ct {
                    r.headers_mut().insert(
                        axum::http::header::CONTENT_TYPE,
                        axum::http::HeaderValue::from_str(&ct).unwrap_or(
                            axum::http::HeaderValue::from_static("application/octet-stream"),
                        ),
                    );
                }
                // U-18 导出产物：有原始文件名 sidecar（local 首方）→ 另存为原名。
                // s3 腿无此元数据 = 不下发（尽力口径成文于 db-guide）。
                if let Ok(Some(name)) = blob.filename(&key).await
                    && !name.is_empty()
                {
                    let safe: String = name
                        .chars()
                        .filter(|c| !c.is_control() && *c != '"' && *c != '\\')
                        .collect();
                    r.headers_mut().insert(
                        axum::http::header::CONTENT_DISPOSITION,
                        axum::http::HeaderValue::from_str(&format!(
                            "attachment; filename=\"{safe}\""
                        ))
                        .unwrap_or(axum::http::HeaderValue::from_static("attachment")),
                    );
                }
                r
            }
            Ok(BlobServed::Redirect(url)) => {
                let mut r = Response::new(axum::body::Body::empty());
                *r.status_mut() = StatusCode::SEE_OTHER;
                r.headers_mut().insert(
                    axum::http::header::LOCATION,
                    axum::http::HeaderValue::from_str(&url)
                        .unwrap_or(axum::http::HeaderValue::from_static("/")),
                );
                r
            }
            Err(_) => return fail_response(404, "blob not found"),
        };
        apply_custom_headers(&mut r, &st.static_opts.response_headers);
        return r;
    }
    // blob 直传上传（v0.1.30）：`PUT {prefix}/blob/{key}`。写面必须过守卫（GET 公开是
    // 读语义；上传公开 = 任意人写你的存储）。经 admit()（鉴权 + 租户，同 run_route 语义），
    // 体积上限走 blob_upload_max 档。不经 JsActor：无 handler 30s timeout。
    // anonymous_paths 可用 `/blob/**` 显式豁免（自甘风险）。
    // v0.1.38（ABI 10）：body 流式 `put_stream_open/chunk/finish` 直落后端（local 落盘 /
    // s3 multipart），内存恒定；后端不支持流式（put_stream_open Err）回落整段缓冲 put
    // （上限 blob_upload_max，语义与旧版一致）。
    if verb == "PUT"
        && let Some(blob) = st.pipeline.blob.as_ref()
        && let Some(key) = blob_key_of(prefix, uri.path())
        && let Some(key) = decode_blob_key(key)
    {
        let path_no_base = format!("/blob/{key}");
        if let Err(resp) = admit(st, headers, verb, Some(&path_no_base)) {
            return *resp;
        }
        return blob_put_direct(st, blob.clone(), &key, headers, body).await;
    }
    // 去 prefix 路径（鉴权匿名匹配用；不在 prefix 下 → None = 不设防）。
    let path_no_base = crate::routes::normalize(uri.path())
        .and_then(|p| p.strip_prefix(prefix).map(|s| s.to_string()));
    if let Some(path) = crate::routes::normalize(uri.path()) {
        match table.lookup(&path, verb) {
            Lookup::Hit {
                file,
                params,
                handler,
                ..
            } => {
                return run_route(
                    st,
                    headers,
                    body,
                    verb,
                    parse_query(uri.query()),
                    path_no_base.as_deref(),
                    &path,
                    file,
                    params,
                    handler,
                )
                .await;
            }
            Lookup::Conflict(msg) => return fail_response(500, &msg),
            Lookup::MethodNotAllowed => {
                return fail_response(405, &format!("method {verb} not allowed"));
            }
            Lookup::NotFound => {}
        }
    }
    // dev 兜底：目录镜像（挂 .route 的方法已被替换，不得复活）。
    if let Some((file, params)) = fallback.and_then(|fb| fb.resolve(uri.path())) {
        // 表内路径经过 canonicalize（macOS /var ↔ /private/var），对齐后再比对
        let file = file.canonicalize().unwrap_or(file);
        match crate::routes::method_name(verb) {
            Some(m) if !table.is_replaced(&file, m) => {
                return run_route(
                    st,
                    headers,
                    body,
                    verb,
                    parse_query(uri.query()),
                    path_no_base.as_deref(),
                    uri.path(),
                    file,
                    params,
                    m.to_string(),
                )
                .await;
            }
            Some(_) => {} // replaced → 404
            None => return fail_response(405, &format!("method {verb} not mapped")),
        }
    }
    // 同 prefix 配对 web（规则 2）：api 优先已由顺序保证；该 web **不开** SPA 回落
    // （最长前缀命中已使拼错路径直接落 api 挂载，回落吞不掉也不该吞）。
    if let Some(w) = web {
        return serve_web(st, w, rel, verb, accept_html, headers).await;
    }
    fail_response(404, "no route matched")
}

/// web 挂载静态服务（v0.1.58 自 handle() 静态兜底抽出）：仅 GET/HEAD（其余 405，
/// 规则 4）；`spa: true` 且未命中文件 + 无扩展名 + Accept html → 回落该挂载根
/// index.html（默认 false：静默把 404 变 200 会掩盖错配，继承 v0.1.20 立论）。
/// 跨挂载不回落：未命中即 404——旧版的 `path_under_base` 排除检查结构性消亡
/// （api 前缀路径恒最长命中 api 挂载，走不到 web 腿；配对 web 又被规则 2 禁 spa）。
use crate::dispatch::*;
/// 根路径再导出：mcp_http 等以 crate::admit/run_capture 引用的入口。
pub(crate) use crate::dispatch::{admit, fail_response, run_capture};
use crate::meta_inject::*;
use crate::multipart::*;
use crate::static_files::*;

#[cfg(test)]
#[path = "lib_tests.rs"]
pub(crate) mod tests;
