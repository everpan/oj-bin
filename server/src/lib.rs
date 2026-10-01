//! mdm-server：HTTP 层。目录镜像路由（routes）+ JS actor 线程桥（actor）+ axum 装配（本文件）。

pub mod actor;
pub mod certificate;
pub mod certificate_watcher;
pub mod logging;
pub mod routes;
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

/// 静态站点（v0.1.27 多站点：URL 前缀 → 磁盘目录映射）。
#[derive(Clone, Debug)]
pub struct StaticSite {
    /// URL 前缀（规范化为首斜杠、无尾斜杠；`/` = 兜底 catch-all）。app() 内按
    /// 前缀长度降序（同长字符串升序）排序，请求期 find_map 取最长命中。
    pub prefix: String,
    /// 磁盘根目录（resolve_static 在此解析；装配期已 canonicalize）。
    pub root: PathBuf,
    /// 该站点的自定义响应头（v0.1.30；覆盖全局同名头，框架自有头仍优先）。
    pub headers: Vec<(String, String)>,
}

/// 共享状态（JsActor 句柄 Clone = 同一 actor 队列的多份引用）。
#[derive(Clone)]
pub struct AppState {
    table: RouteTable,
    /// dev（ts=true）文件系统兜底：表 miss 时回退目录镜像；release None。
    fallback: Option<Routes>,
    actor: JsActor,
    /// 单请求超时（None = 不限时）。
    timeout: Option<std::time::Duration>,
    /// 静态站点表（v0.1.27 多站点：prefix→dir 映射；app() 内已按前缀长度降序
    /// 排好，find_map 取最长命中）。空 = 不开静态服务。
    static_sites: Vec<StaticSite>,
    /// 静态站点增强（v0.1.20）：SPA 深链接回落 + per-route meta 注入。
    static_opts: StaticOpts,
    /// handle() 前置管线（OJ-3..5 单一扩展点；后续阶段只加字段）。
    pipeline: Pipeline,
    /// API 基础前缀（内置 auth 路由 / 匿名路径匹配用）。
    base: String,
    /// 当前证书状态（热加载共享，可用 RwLock 原子更新）
    pub certificate_status: Arc<RwLock<CertificateStatus>>,
    /// 证书有效期截止时间（热加载共享）
    pub certificate_valid_until: Arc<RwLock<Option<std::time::SystemTime>>>,
    /// 已装配插件自描述清单（`{base}/plugins` 数据源；装配层注入）。
    pub plugins: Arc<Vec<PluginInfo>>,
}

/// 静态站点增强（v0.1.20 起；v0.1.25 增动态 meta 与 HTML 缓存头）。
#[derive(Clone, Default)]
pub struct StaticOpts {
    /// SPA 深链接回落：静态未命中 + 无扩展名 + Accept html + 不在 api_prefix 下
    /// → 送 root/index.html。默认 false（静默把 404 变 200 会掩盖错配，故显式开启）。
    pub spa_fallback: bool,
    /// 路由感知 meta 目录名（相对静态根）：送 HTML 前按路径查
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
#[allow(clippy::too_many_arguments)]
pub fn app(
    base: &str,
    dir: impl Into<PathBuf>,
    ts: bool,
    table: RouteTable,
    actor: JsActor,
    timeout: Option<std::time::Duration>,
    static_sites: Vec<StaticSite>,
    // static_opts：静态站点两条 v0.1.20 增强（SPA 回落 / per-route meta）。
    static_opts: StaticOpts,
    pipeline: Pipeline,
    certificate_status: Arc<RwLock<CertificateStatus>>,
    certificate_valid_until: Arc<RwLock<Option<std::time::SystemTime>>>,
    plugins: Arc<Vec<PluginInfo>>,
    // CORS（v0.1.35）：段存在即启用；None = 不挂层（行为完全不变）。
    cors: Option<CorsCfg>,
) -> Router {
    let dir = dir.into();
    let base = base.trim_end_matches('/');
    let health_path = format!("{base}/health");
    // 公共基础设施端点（先于 fallback 的真实 route）：不走 Bearer 守卫 / 证书 GET 门禁，
    // 与 /health 同位（匿名可访问），保留路径遮蔽同名业务路由。
    let plugins_path = format!("{base}/plugins");
    let mut router = Router::new()
        .route(&health_path, axum::routing::get(health_handler))
        .route(&plugins_path, axum::routing::get(plugins_handler))
        .fallback(any(handle))
        // 请求日志中间件（method/path/status/耗时 → 文件日志 + stderr）。
        .layer(axum::middleware::from_fn(crate::logging::log_requests));
    // CORS 层放在最外层（先于 handle 与日志中间件）：预检由本层短路（204 + CORS 头），
    // 不进入路由；简单请求补 Access-Control-* 响应头。
    if let Some(cfg) = cors {
        router = router.layer(build_cors_layer(&cfg));
    }
    router.with_state(AppState {
        table,
        fallback: ts.then(|| Routes::new(base, dir, ts)),
        actor,
        timeout,
        static_sites: sorted_sites(static_sites),
        static_opts,
        pipeline,
        base: base.to_string(),
        certificate_status,
        certificate_valid_until,
        plugins,
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

/// 站点表排序（v0.1.27 多站点）：前缀长度降序（最长命中优先），同长按字符串升序保确定性。
/// 归 app() 独家负责——装配层（oj/src/app.rs）只归一/去重，不排序。
fn sorted_sites(mut sites: Vec<StaticSite>) -> Vec<StaticSite> {
    sites.sort_by(|a, b| {
        b.prefix
            .len()
            .cmp(&a.prefix.len())
            .then_with(|| a.prefix.cmp(&b.prefix))
    });
    sites
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

/// 绑定监听并服务。
#[allow(clippy::too_many_arguments)]
pub async fn serve(
    addr: std::net::SocketAddr,
    base: &str,
    dir: impl Into<PathBuf>,
    ts: bool,
    table: RouteTable,
    actor: JsActor,
    timeout: Option<std::time::Duration>,
    static_sites: Vec<StaticSite>,
    static_opts: StaticOpts,
    pipeline: Pipeline,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    serve_with_listener(
        listener,
        base,
        dir,
        ts,
        table,
        actor,
        timeout,
        static_sites,
        static_opts,
        pipeline,
        None,
    )
    .await
}

/// 已绑定监听上服务（测试/T11：先 bind 端口 0 再读 local_addr）。
#[allow(clippy::too_many_arguments)]
pub async fn serve_with_listener(
    listener: tokio::net::TcpListener,
    base: &str,
    dir: impl Into<PathBuf>,
    ts: bool,
    table: RouteTable,
    actor: JsActor,
    timeout: Option<std::time::Duration>,
    static_sites: Vec<StaticSite>,
    static_opts: StaticOpts,
    pipeline: Pipeline,
    cors: Option<CorsCfg>,
) -> std::io::Result<()> {
    serve_router(
        listener,
        app(
            base,
            dir,
            ts,
            table,
            actor,
            timeout,
            static_sites,
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

    // 请求体按路径分档限长（v0.1.30 取代全局 DefaultBodyLimit 层）：blob 直传 PUT
    // 用 blob_upload_max（默认 1 GiB），其余维持 2x max_upload 硬顶——**不能**把全局
    // 层抬到 1 GiB（handler 路由会被动接受巨体缓冲，内存 DoS 面扩大）。超限裸 413，
    // 与旧 DefaultBodyLimit 行为逐字节一致。
    // v0.1.38（ABI 10）：顶部不再无条件 `to_bytes` 整段缓冲——blob 直传腿与
    // multipart 腿改为**流式**消费（put_stream_* / multer 流式解析，内存恒定），
    // 其余腿（非 multipart 的小体）在 run_route 内缓冲，上限语义不变。
    let is_blob_put = verb == "PUT"
        && st.pipeline.blob.is_some()
        && uri.path().starts_with(&format!("{}/blob/", st.base));

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

    // 内置 blob 下载路由（{base}/blob/{key}，公开 GET，先于路由表，也先于前置管线）。
    //
    // **与匿名路径的耦合（互指注释）**：本分支（以及下方静态站点兜底）在 auth/tenant
    // 前置管线**之前**直接 return，因此 `anonymous_paths`（`Pipeline.tenant_anon` /
    // `AuthGuard::verify`）对它们**不产生任何效力**。启动期的匿名路径迁移 WARN
    // （`oj/src/app.rs` 的 `warn_legacy_tail_wildcards`）正是靠这一点把「影响面」收窄到
    // 「已注册路由」——**若把这两处提前返回改为走前置管线，或让它们咨询匿名表，必须同步
    // 该判定的前提**，否则 WARN 会静默失准。
    // v0.1.25 补第三处**同一前提**的分支：静态兜底内部派发 `server.html_meta_handler`
    // （见 `dispatch_meta_handler`）同样不经守卫（页面请求带不了 Bearer），故该 handler
    // **不必**进 `anonymous_paths`；它被外部直接访问时仍受守卫约束（两侧口径不冲突）。
    // v0.1.30 补第四处**例外**：下方 PUT（直传上传）走守卫 + 租户准入（写面不能公开），
    // 只豁免**读取**语义不变。
    if verb == "GET"
        && let Some(blob) = st.pipeline.blob.as_ref()
        && let Some(key) = uri.path().strip_prefix(&format!("{}/blob/", st.base))
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
    // blob 直传上传（v0.1.30）：`PUT {base}/blob/{key}`。写面必须过守卫（GET 公开是
    // 读语义；上传公开 = 任意人写你的存储）。经 admit()（鉴权 + 租户，同 run_route 语义），
    // 体积上限走 blob_upload_max 档。不经 JsActor：无 handler 30s timeout。
    // anonymous_paths 可用 `/blob/**` 显式豁免（自甘风险）。
    // v0.1.38（ABI 10）：body 流式 `put_stream_open/chunk/finish` 直落后端（local 落盘 /
    // s3 multipart），内存恒定；后端不支持流式（put_stream_open Err）回落整段缓冲 put
    // （上限 blob_upload_max，语义与旧版一致）。
    if is_blob_put
        && let Some(blob) = st.pipeline.blob.as_ref()
        && let Some(key) = uri.path().strip_prefix(&format!("{}/blob/", st.base))
        && let Some(key) = decode_blob_key(key)
    {
        let path_no_base = format!("/blob/{key}");
        if let Err(resp) = admit(&st, &headers, verb, Some(&path_no_base)) {
            return *resp;
        }
        return blob_put_direct(&st, blob.clone(), &key, &headers, body).await;
    }
    // 去 base 路径（鉴权匿名匹配用；不在 base 下 → None = 不设防）。
    let path_no_base = crate::routes::normalize(uri.path())
        .and_then(|p| p.strip_prefix(st.base.as_str()).map(|s| s.to_string()));
    if let Some(path) = crate::routes::normalize(uri.path()) {
        match st.table.lookup(&path, verb) {
            Lookup::Hit { file, params } => {
                return run_route(
                    &st,
                    &headers,
                    body,
                    verb,
                    parse_query(uri.query()),
                    path_no_base.as_deref(),
                    &path,
                    file,
                    params,
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
    if let Some((file, params)) = st.fallback.as_ref().and_then(|fb| fb.resolve(uri.path())) {
        // 表内路径经过 canonicalize（macOS /var ↔ /private/var），对齐后再比对
        let file = file.canonicalize().unwrap_or(file);
        match crate::routes::method_name(verb) {
            Some(m) if !st.table.is_replaced(&file, m) => {
                return run_route(
                    &st,
                    &headers,
                    body,
                    verb,
                    parse_query(uri.query()),
                    path_no_base.as_deref(),
                    uri.path(),
                    file,
                    params,
                )
                .await;
            }
            Some(_) => {} // replaced → 404
            None => return fail_response(405, &format!("method {verb} not mapped")),
        }
    }
    // 静态站点兜底（v0.1.27 多站点：prefix→dir，最长前缀命中——表已在 app() 内
    // 按前缀长度降序）：API 优先，GET/HEAD only。命中站点内未命中 → **仅该站**
    // SPA 回落，不跨站；前缀外路径不走静态。
    if matches!(verb, "GET" | "HEAD")
        && let Some((site, rel_path)) = st
            .static_sites
            .iter()
            .find_map(|s| strip_app_prefix(&s.prefix, uri.path()).map(|r| (s, r)))
    {
        let root = &site.root;
        let meta = st.static_opts.html_meta.as_deref();
        if let Some(file) = resolve_static(root, rel_path, meta)
            && let Ok(body) = tokio::fs::read(&file).await
        {
            let mut r = static_page(&st, root, rel_path, &file, body).await;
            apply_custom_headers(&mut r, &site_headers(&st, site));
            return r;
        }
        // SPA 深链接回落（server.app_spa_fallback，v0.1.20）：未命中 + 无扩展名 +
        // Accept html + **不在 api_prefix 下**（否则拼错的 API 路径会被 index.html
        // 吞成 200，掩盖真实 404）→ 送 **本站点** root/index.html。
        if st.static_opts.spa_fallback
            && accept_html
            && !has_extension(rel_path)
            && !path_under_base(rel_path, &st.base)
        {
            let idx = root.join("index.html");
            if let Ok(body) = tokio::fs::read(&idx).await {
                let mut r = static_page(&st, root, rel_path, &idx, body).await;
                apply_custom_headers(&mut r, &site_headers(&st, site));
                return r;
            }
        }
    }
    fail_response(404, "no route matched")
}

/// 路由命中 → 前置管线（鉴权/租户/上传上限/multipart）→ 执行 handler → Capture 回写。
///
/// 自由函数而非 `handle()` 内的闭包：`handle()` 的静态兜底（v0.1.25 起还会内部派发
/// meta handler）与 `strip_app_prefix` 同款理由——闭包会**部分移动** `st`/`headers`，
/// 之后就没法再借用它们。
#[allow(clippy::too_many_arguments)]
async fn run_route(
    st: &AppState,
    headers: &HeaderMap,
    body: axum::body::Body,
    verb: &str,
    query: HashMap<String, String>,
    path_no_base: Option<&str>,
    full_path: &str,
    file: PathBuf,
    params: HashMap<String, String>,
) -> Response {
    let m = crate::routes::method_name(verb)
        .expect("checked by caller")
        .to_string();
    // 前置管线（鉴权 + 租户）统一进口：run_route 与 blob 直传 PUT 共用（v0.1.30 抽出，
    // 防两处准入语义漂移）。401/400 以 Response 返回。
    let (user, tenant_id, anonymous) = match admit(st, headers, verb, path_no_base) {
        Ok(v) => v,
        Err(resp) => return *resp,
    };
    // v0.1.38（ABI 10）：multipart + blob 已配置 → **流式**解析（文件字段直落 blob，
    // 内存恒定；大文件不再受 max_upload 硬顶约束，单文件上限 blob_upload_max）；
    // 其余（非 multipart，或未配 blob）维持整体缓冲（上限 2x max_upload；超 max_upload
    // 信封 413、超 2x 裸 413——与旧 DefaultBodyLimit 行为逐字节一致）。
    let (body_bytes, files) = if is_multipart(headers) && st.pipeline.blob.is_some() {
        match stream_multipart(st, headers, body).await {
            Ok(v) => v,
            Err(resp) => return *resp,
        }
    } else {
        let buffered =
            match axum::body::to_bytes(body, (st.pipeline.max_upload.saturating_mul(2)) as usize)
                .await
            {
                Ok(b) => b,
                Err(_) => {
                    return Response::builder()
                        .status(StatusCode::PAYLOAD_TOO_LARGE)
                        .body(axum::body::Body::empty())
                        .unwrap_or_else(|_| Response::new(axum::body::Body::empty()));
                }
            };
        if buffered.len() > st.pipeline.max_upload as usize {
            return fail_response(413, "upload too large");
        }
        if is_multipart(headers) {
            parse_multipart(headers, &buffered).await
        } else {
            (buffered.to_vec(), Vec::new())
        }
    };
    let req = RequestInfo {
        method: verb.to_string(),
        params,
        query,
        headers: headers
            .iter()
            .filter_map(|(k, v)| Some((k.to_string(), v.to_str().ok()?.to_string())))
            .collect(),
        body: body_bytes,
        body_binary: false,
        tenant_id,
        anonymous,
        user,
        files,
        bus_tx: None,
    };
    // oj-5c：路由级 timeout 覆盖（按声明顺序首个命中；含 base 的全路径匹配）。
    let timeout = st
        .pipeline
        .route_timeouts
        .iter()
        .find(|(p, _)| path_matches(std::slice::from_ref(p), full_path))
        .map(|(_, d)| *d)
        .or(st.timeout);
    let mut resp = match st.actor.run_module(file, m, req, timeout, true).await {
        Ok(cap) => capture_response(cap),
        // 超时熔断 → 408。
        Err(e) if e.timeout => fail_response(408, &e.msg),
        Err(e) => fail_response(500, &e.msg),
    };
    apply_custom_headers(&mut resp, &st.static_opts.response_headers);
    resp
}

/// 前置管线准入（v0.1.30 抽出）：鉴权（ABI 9 四参守卫）+ 租户提取。
/// Ok((user, tenant_id, anonymous))；Err = 已构造的 401/400 响应。
fn admit(
    st: &AppState,
    headers: &HeaderMap,
    verb: &str,
    path_no_base: Option<&str>,
) -> Result<(Option<serde_json::Value>, Option<String>, bool), Box<Response>> {
    // 鉴权（base 内非匿名路径必须过守卫 → 401；Ok(None) = 匿名放行）。
    // ABI 9：方法 + 全部请求头 JSON（小写名 → 值，含 cookie）透传守卫——
    // cookie 会话/CSRF 双提交的判定材料都在 headers 里（oj-auth 消费）。
    let user = match (st.pipeline.auth.as_ref(), path_no_base) {
        (Some(guard), Some(p)) => {
            let header = headers
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok());
            let headers_json = serde_json::to_string(
                &headers
                    .iter()
                    .filter_map(|(k, v)| v.to_str().ok().map(|s| (k.as_str(), s)))
                    .collect::<std::collections::HashMap<_, _>>(),
            )
            .unwrap_or_default();
            match guard.verify(p, verb, header, Some(&headers_json)) {
                Ok(Some(u)) => Some(u),
                Ok(None) => None,
                Err(msg) => return Err(Box::new(fail_response(401, &msg))),
            }
        }
        _ => None,
    };
    // 租户提取（启用后缺失/空 → 400；anonymous_paths 命中的跳转腿
    // 豁免"缺失 400"——OIDC 302 带不了自定义头——但已带的头仍注入）。
    // anonymous：db.asTenant 的授信判据（v0.1.20）。只有「豁免命中 + 确实没带
    // 租户头」才是匿名；带了头或没豁免都不是（后者走 400 / tid 注入）。
    let mut anonymous = false;
    let tenant_id = match st.pipeline.tenant_header.as_deref() {
        Some(key) => {
            let exempt = path_matches(&st.pipeline.tenant_anon, path_no_base.unwrap_or(""));
            let header_tid = headers
                .get(key)
                .and_then(|v| v.to_str().ok())
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            if st.pipeline.tenant_require_signed_claim {
                // PR-9（v0.1.41）：租户只认**验签后的 claim**（user.claims.tenant）。
                // 裸头不再自证：带头仅作与 claim 的一致性复核（不符 403）；
                // anonymous_paths 豁免保持（OIDC 回跳逃生口不变），其余路径 fail-closed。
                let claim = user
                    .as_ref()
                    .and_then(|u| u.get("claims"))
                    .and_then(|c| c.get("tenant"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                match (&header_tid, &claim) {
                    (Some(h), Some(c)) if h != c => {
                        return Err(Box::new(fail_response(
                            403,
                            "tenant header does not match signed claim",
                        )));
                    }
                    // claim 是唯一来源（头一致或缺失都取 claim）；claim 缺失时头不得自证。
                    (_, Some(c)) => Some(c.clone()),
                    (Some(_), None) => {
                        return Err(Box::new(fail_response(
                            403,
                            "tenant requires a signed claim (user.claims.tenant missing)",
                        )));
                    }
                    (None, None) if exempt => {
                        anonymous = true;
                        None
                    }
                    (None, None) => {
                        return Err(Box::new(fail_response(
                            403,
                            "tenant requires a signed claim (user.claims.tenant missing)",
                        )));
                    }
                }
            } else {
                match header_tid {
                    Some(tid) => Some(tid),
                    None if exempt => {
                        anonymous = true;
                        None
                    }
                    None => {
                        return Err(Box::new(fail_response(
                            400,
                            &format!("missing tenant header: {key}"),
                        )));
                    }
                }
            }
        }
        None => None,
    };
    Ok((user, tenant_id, anonymous))
}

/// SPA 回落的 Accept 判据（v0.1.20）：缺失 / `*/*` / 含 `text/html`（q>0）均视为 html。
/// curl 默认发 `*/*` —— 只认字面 `text/html` 会让回落对最常用客户端失效。
fn wants_html(headers: &HeaderMap) -> bool {
    match headers
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
    {
        None => true,
        Some(v) => v
            .split(',')
            .map(|p| p.split(';').next().unwrap_or("").trim())
            .any(|t| t == "text/html" || t == "*/*"),
    }
}

/// 路径是否带扩展名（带扩展名 = 资源请求，不该回落成 HTML）。
fn has_extension(rel_path: &str) -> bool {
    rel_path
        .rsplit('/')
        .next()
        .and_then(|last| (last.contains('.')).then_some(true))
        .unwrap_or(false)
}

/// 是否落在 API 前缀下（回落须排除，防吞掉 404）。
fn path_under_base(rel_path: &str, base: &str) -> bool {
    let base = base.trim_end_matches('/');
    !base.is_empty() && (rel_path == base || rel_path.starts_with(&format!("{base}/")))
}

/// 静态响应（v0.1.20 起；v0.1.25 增动态 meta + HTML 缓存头）：HTML 走 per-route meta
/// 注入（静态 JSON 打底 → 动态 handler 覆盖），其余原样；什么都不配时逐字节同旧行为。
///
/// `rel_path` 是**已剥站点前缀**的请求路径（SPA 回落时即深链接本身，不是 index.html
/// 的落盘路径）——按它查 meta 才能做到「按路由」；`site_root` 是命中站点的磁盘根
/// （meta JSON 目录按站点各自解析，v0.1.27 多站点）。
async fn static_page(
    st: &AppState,
    site_root: &Path,
    rel_path: &str,
    file: &Path,
    body: Vec<u8>,
) -> Response {
    let is_html = file
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("html"));
    if !is_html {
        // 缓存头只管 HTML：js/css/图片仍交前置反代（见已知限制）。
        return file_response(file, body);
    }
    let opts = &st.static_opts;
    let (has_static, has_dynamic) = (opts.html_meta.is_some(), opts.html_meta_handler.is_some());
    if !has_static && !has_dynamic {
        // 注入面全关：**只**加缓存头（`html_cache_control` 是独立键——只想给壳挂
        // `no-cache` 而不做注入，是最常见的用法之一）；该键也没配才逐字节同旧行为。
        let mut r = file_response(file, body);
        if let Some(cc) = opts.html_cache_control.as_deref() {
            set_cache_control(&mut r, cc);
        }
        return r;
    }
    // 1) 静态 JSON 打底（`<site_root>/<dir>/<path>.json`，v0.1.20），2) 动态 handler 按 key 覆盖。
    let mut map = load_static_meta(site_root, opts.html_meta.as_deref(), rel_path);
    let Ok(text) = String::from_utf8(body) else {
        // 非 UTF-8 的「HTML」不是我们能注入的文档（保持 v0.1.20 的旧行为：空体）。
        return file_response(file, Vec::new());
    };
    let mut dynamic_cc: Option<String> = None;
    if has_dynamic {
        match dispatch_meta_handler(st, rel_path).await {
            Ok((m, cc)) => {
                dynamic_cc = cc;
                map.extend(m);
            }
            // fail-open：注入面坏了不能让页面跟着坏（爬虫/用户看到的仍是一份完整 HTML）。
            Err(e) => eprintln!("warn: html_meta_handler: {e}"),
        }
    }
    let out = inject_head(&text, &map);
    let mut r = file_response(file, out.into_bytes());
    // 缓存头：动态 handler 的 cache_control > server.html_cache_control；都没有 = 不加。
    if let Some(cc) = dynamic_cc.as_deref().or(opts.html_cache_control.as_deref()) {
        set_cache_control(&mut r, cc);
    }
    r
}

/// 内部派发 `server.html_meta_handler`（GET）→ (键值表, 可选 cache_control)。
///
/// **不经前置守卫**：页面请求（爬虫 / IM 预览）带不了 `Authorization`，走守卫等于恒 401；
/// 故该 handler 不需要进 `anonymous_paths`（外部直接访问它仍受守卫约束，这正是想要的）。
///
/// **恒以匿名身份运行**（`tenant_id = None` + `anonymous = true`，租户头/请求头/请求体一律
/// 不传递）——这不是省事，是安全边界：页面请求带什么头由客户端决定，若把 `X-Tenant` 透传进来，
/// ①`sql_guard: deny` 下构造器会把**攻击者指定的租户**当过滤条件（匿名访客即可读某租户的行并
/// 写进公开 meta），②`db.asTenant` 的三道门禁要求 `tenant_id.is_none()`，透传会让它必抛。
/// 于是「按租户取数」只有一条正路：handler 从 URL 派生 id 后调 `db.asTenant(id)`
/// （需 `tenant.allow_as_tenant: true`）。`http.tenantId` 在本 handler 里恒 `null`。
///
/// 失败（非法路径 / 不在路由表 / 非 2xx / 超时 / 非 JSON / 信封 `code != 0`）→ Err，
/// 调用点 WARN 后按静态结果送出。
async fn dispatch_meta_handler(
    st: &AppState,
    rel_path: &str,
) -> Result<(serde_json::Map<String, Value>, Option<String>), String> {
    let handler = st
        .static_opts
        .html_meta_handler
        .as_deref()
        .ok_or("not configured")?;
    let norm = crate::routes::normalize(handler)
        .ok_or_else(|| format!("{handler:?} 不是合法路径（须以 / 开头）"))?;
    let (file, params) = match st.table.lookup(&norm, "GET") {
        Lookup::Hit { file, params } => (file, params),
        Lookup::Conflict(m) => return Err(format!("{handler:?} 路由冲突：{m}")),
        Lookup::MethodNotAllowed => return Err(format!("{handler:?} 未映射 GET 方法")),
        Lookup::NotFound => return Err(format!("{handler:?} 不在路由表（拼错了？）")),
    };
    let mut query = HashMap::new();
    query.insert("path".to_string(), rel_path.to_string());
    let req = RequestInfo {
        method: "GET".to_string(),
        params,
        query,
        headers: HashMap::new(),
        body: Vec::new(),
        body_binary: false,
        tenant_id: None,
        anonymous: true,
        user: None,
        files: Vec::new(),
        bus_tx: None,
    };
    let cap = st
        .actor
        .run_module(
            file,
            // JS 侧方法名（"get"，不是 HTTP 动词）——`run_module` 用它索引 `default[method]`，
            // 传 "GET" 会命中「方法未导出」→ json.fail(405)。
            crate::routes::method_name("GET")
                .expect("GET is mapped")
                .to_string(),
            req,
            st.timeout,
            // 内部合成派发：不是外部输入，契约校验失败只会静默降级页面 meta。
            false,
        )
        .await
        .map_err(|e| {
            if e.timeout {
                format!("{handler:?} 超时")
            } else {
                format!("{handler:?} 执行失败：{}", e.msg)
            }
        })?;
    if !(200..300).contains(&cap.status) {
        return Err(format!("{handler:?} 返回 HTTP {}", cap.status));
    }
    let mut v: Value = serde_json::from_slice(&cap.body)
        .map_err(|e| format!("{handler:?} 返回非 JSON 体：{e}"))?;
    // 标准信封 {code,msg,data}：code != 0 视为失败；data 是对象则用它，否则用顶层对象。
    if let Some(code) = v.get("code").and_then(|c| c.as_i64()) {
        if code != 0 {
            let msg = v.get("msg").and_then(|m| m.as_str()).unwrap_or("failed");
            return Err(format!("{handler:?} 返回 code {code}: {msg}"));
        }
        if let Some(data) = v.get_mut("data") {
            v = data.take();
        }
    }
    let Value::Object(mut obj) = v else {
        return Err(format!("{handler:?} 的返回不是 JSON 对象"));
    };
    // `cache_control` 是保留键（不进 `<head>`）：仅作本响应的 Cache-Control 覆盖。
    // 类型错**只丢这个键**（其余 title/og 照旧注入）——一个笔误不该让整份 meta 消失。
    let cc = match obj.remove("cache_control") {
        Some(Value::String(s)) => Some(s),
        Some(other) => {
            eprintln!("warn: {handler:?} 的 cache_control 不是字符串（已忽略该键）：{other}");
            None
        }
        None => None,
    };
    Ok((obj, cc))
}

/// `server.html_cache_control` / handler 的 `cache_control` → 响应头。
/// 非法头值只 WARN 并忽略（不能让一个配置笔误把页面打成 500）。
fn set_cache_control(r: &mut Response, value: &str) {
    match axum::http::HeaderValue::from_str(value) {
        Ok(hv) => {
            r.headers_mut()
                .insert(axum::http::header::CACHE_CONTROL, hv);
        }
        Err(_) => eprintln!("warn: illegal Cache-Control value {value:?} (ignored)"),
    }
}

/// 静态 meta JSON（`<root>/<dir>/<path>.json`）→ 键值表；未配置/未命中/非对象 → 空表。
fn load_static_meta(
    root: &Path,
    html_meta: Option<&str>,
    rel_path: &str,
) -> serde_json::Map<String, Value> {
    let Some(dir) = html_meta else {
        return serde_json::Map::new();
    };
    let Some(json_path) = meta_json_path(root, dir, rel_path) else {
        return serde_json::Map::new();
    };
    let Ok(raw) = std::fs::read_to_string(&json_path) else {
        return serde_json::Map::new();
    };
    match serde_json::from_str::<Value>(&raw) {
        Ok(Value::Object(o)) => o,
        Ok(_) => serde_json::Map::new(),
        Err(_) => {
            eprintln!("warn: html_meta {}: invalid JSON", json_path.display());
            serde_json::Map::new()
        }
    }
}

/// 键值表 → `<head>` 注入片段（白名单：`title` / `description` / `canonical` /
/// `og:*`（`property`）/ `twitter:*`（`name`））。只注入 `<title>`/`<meta>`/`<link>`，
/// **绝不注入脚本**（静态响应拿不到 CSP nonce）；值一律 HTML 转义；其余键忽略。
fn render_meta_tags(obj: &serde_json::Map<String, Value>) -> String {
    let mut tags = String::new();
    if let Some(t) = obj.get("title").and_then(|t| t.as_str()) {
        tags.push_str(&format!("<title>{}</title>\n", escape_html(t)));
    }
    if let Some(d) = obj.get("description").and_then(|d| d.as_str()) {
        tags.push_str(&format!(
            "<meta name=\"description\" content=\"{}\">\n",
            escape_html(d)
        ));
    }
    if let Some(c) = obj.get("canonical").and_then(|c| c.as_str()) {
        tags.push_str(&format!(
            "<link rel=\"canonical\" href=\"{}\">\n",
            escape_html(c)
        ));
    }
    // og:* / twitter:*：`og:*` 用 property，其余用 name。
    let mut rest: Vec<(&String, &Value)> = obj
        .iter()
        .filter(|(k, _)| k.starts_with("og:") || k.starts_with("twitter:"))
        .collect();
    rest.sort_by(|a, b| a.0.cmp(b.0)); // 稳定输出（便于测试与 diff）
    for (k, val) in rest {
        if let Some(s) = val.as_str() {
            let attr = if k.starts_with("og:") {
                "property"
            } else {
                "name"
            };
            tags.push_str(&format!(
                "<meta {attr}=\"{}\" content=\"{}\">\n",
                escape_html(k),
                escape_html(s)
            ));
        }
    }
    tags
}

/// 注入键 → 需要**摘掉**的既有标签形态（决定「谁跟谁同名」）。
#[derive(Debug, PartialEq, Eq)]
enum Conflict {
    /// `<title>…</title>`
    Title,
    /// `<link rel="canonical">`
    CanonicalLink,
    /// `<meta name|property="KEY">`
    MetaTag(String),
}

impl Conflict {
    fn matches(&self, name: &str, attrs: &str) -> bool {
        match self {
            Conflict::Title => name.eq_ignore_ascii_case("title"),
            Conflict::CanonicalLink => {
                name.eq_ignore_ascii_case("link")
                    && attr_value(attrs, "rel").is_some_and(|v| v.eq_ignore_ascii_case("canonical"))
            }
            Conflict::MetaTag(key) => {
                name.eq_ignore_ascii_case("meta")
                    && ["name", "property"]
                        .iter()
                        .any(|a| attr_value(attrs, a).is_some_and(|v| v.eq_ignore_ascii_case(key)))
            }
        }
    }
}

/// 键值表 → 「要摘的既有标签」清单（与 `render_meta_tags` 的渲染判据逐条对齐：
/// 只对**字符串值**的键摘旧的，否则会摘掉却不补上）。
fn conflict_keys(obj: &serde_json::Map<String, Value>) -> Vec<Conflict> {
    let text = |k: &str| obj.get(k).and_then(|v| v.as_str()).is_some();
    let mut out = Vec::new();
    if text("title") {
        out.push(Conflict::Title);
    }
    if text("description") {
        out.push(Conflict::MetaTag("description".to_string()));
    }
    if text("canonical") {
        out.push(Conflict::CanonicalLink);
    }
    for k in obj.keys() {
        if (k.starts_with("og:") || k.starts_with("twitter:")) && text(k) {
            out.push(Conflict::MetaTag(k.clone()));
        }
    }
    out
}

/// 把键值表注入 `<head>`：**先摘掉 head 里同名的既有标签**，再在 `</head>` 前插入新标签。
///
/// 为什么必须摘：浏览器与爬虫只认**第一个**匹配——壳里写死的 `<title>App</title>` 不摘掉，
/// 注入的第二个 title 根本不会生效（`og:*` 同理）。v0.1.20 只插入不摘除，等于注入了个寂寞。
/// 键白名单与 HTML 转义由 `render_meta_tags` 负责，这里只管「摘同名旧标签 + 放新标签」。
/// 无 `</head>` / 无可渲染键 → 原样返回（不猜结构）。
fn inject_head(html: &str, obj: &serde_json::Map<String, Value>) -> String {
    let tags = render_meta_tags(obj);
    if tags.is_empty() {
        return html.to_string();
    }
    let Some(head_end) = find_ci(html, b"</head>") else {
        return html.to_string();
    };
    // head 内容起点：`<head …>` 开标签之后；没有开标签就从文档头起算（仍只动 head 段内）。
    // 用 ASCII 不敏感扫描（**不** `to_lowercase()`——大小写变换会改字节长度，索引会错位）。
    let head_start = match find_ci(html, b"<head") {
        Some(i) => match html[i..].find('>') {
            Some(gt) => i + gt + 1,
            None => return html.to_string(),
        },
        None => 0,
    };
    if head_start > head_end {
        return html.to_string();
    }
    let stripped = strip_conflicts(&html[head_start..head_end], &conflict_keys(obj));
    let mut out = String::with_capacity(html.len() + tags.len());
    out.push_str(&html[..head_start]);
    out.push_str(&stripped);
    out.push_str(&tags);
    out.push_str(&html[head_end..]);
    out
}

/// ASCII 不敏感查找（返回**字节**偏移；针头是 ASCII 标签文本，不会落在多字节字符内部）。
fn find_ci(hay: &str, needle: &[u8]) -> Option<usize> {
    let h = hay.as_bytes();
    if needle.is_empty() || h.len() < needle.len() {
        return None;
    }
    h.windows(needle.len())
        .position(|w| w.eq_ignore_ascii_case(needle))
}

/// 一个 HTML 元素（供摘除扫描）：`name` = 标签名（特殊情形为 `"!"` = 永不匹配），
/// `attrs` = 开标签内的属性原文，`len` = 该元素在原文里的字节长度（含闭合标签）。
struct Element<'a> {
    name: &'a str,
    attrs: &'a str,
    len: usize,
}

/// 从 `<` 处解析一个元素；解析不出（未闭合等）→ `None`（调用点放弃摘除，原样保留）。
///
/// `<!…>` / `<?…>` / 闭合标签 / 无名标签一律以 `"!"` 返回（跳过、不参与匹配）；
/// `script` / `style` 的内容当**不透明文本**整段跳过（内容里出现 `<title>` 之类的字符串
/// 不得被当成标签摘掉）。
fn element_at(s: &str) -> Option<Element<'_>> {
    if !s.starts_with('<') {
        return None;
    }
    let never = |len: usize| {
        Some(Element {
            name: "!",
            attrs: "",
            len,
        })
    };
    if s.starts_with("<!--") {
        return never(s.find("-->").map(|i| i + 3)?);
    }
    if s.starts_with("<!") || s.starts_with("<?") || s.starts_with("</") {
        return never(s.find('>').map(|i| i + 1)?);
    }
    let name_end = s.as_bytes()[1..]
        .iter()
        .position(|c| !(c.is_ascii_alphanumeric() || *c == b'-'))
        .map(|i| i + 1)
        .unwrap_or(s.len());
    if name_end == 1 {
        return never(s.find('>').map(|i| i + 1)?);
    }
    let name = &s[1..name_end];
    let gt = s.find('>')?;
    let attrs = &s[name_end..gt];
    // 不透明内容段 / 需要连闭合标签一起摘的元素
    let opaque = name.eq_ignore_ascii_case("script") || name.eq_ignore_ascii_case("style");
    let has_close = opaque || name.eq_ignore_ascii_case("title");
    if has_close {
        let close = format!("</{name}");
        let len = match find_ci(&s[gt + 1..], close.as_bytes()) {
            Some(k) => {
                let after = gt + 1 + k;
                match s[after..].find('>') {
                    Some(g) => after + g + 1,
                    None => gt + 1,
                }
            }
            None => gt + 1,
        };
        return Some(Element {
            name: if opaque { "!" } else { name },
            attrs,
            len,
        });
    }
    Some(Element {
        name,
        attrs,
        len: gt + 1,
    })
}

/// 摘掉 `head` 段里与 `wants` 同名的既有标签（其余原样保留）。
fn strip_conflicts(head: &str, wants: &[Conflict]) -> String {
    if wants.is_empty() {
        return head.to_string();
    }
    let mut out = String::with_capacity(head.len());
    let mut i = 0usize;
    while i < head.len() {
        let Some(rel) = head[i..].find('<') else {
            out.push_str(&head[i..]);
            break;
        };
        let lt = i + rel;
        out.push_str(&head[i..lt]);
        let Some(el) = element_at(&head[lt..]) else {
            // 解析不了就不冒险：剩余原文照抄
            out.push_str(&head[lt..]);
            break;
        };
        if !wants.iter().any(|w| w.matches(el.name, el.attrs)) {
            out.push_str(&head[lt..lt + el.len]);
        }
        i = lt + el.len;
    }
    out
}

/// 取开标签属性值（ASCII 不敏感属性名；值支持 `"`/`'`/裸值）。属性名前必须是词边界，
/// 免得把 `data-name=` 当成 `name=`。
fn attr_value(attrs: &str, key: &str) -> Option<String> {
    let b = attrs.as_bytes();
    let kb = key.as_bytes();
    if kb.is_empty() || b.len() < kb.len() {
        return None;
    }
    for i in 0..=(b.len() - kb.len()) {
        let boundary = i == 0 || (!b[i - 1].is_ascii_alphanumeric() && b[i - 1] != b'-');
        if !boundary || !b[i..i + kb.len()].eq_ignore_ascii_case(kb) {
            continue;
        }
        let mut j = i + kb.len();
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        if j >= b.len() || b[j] != b'=' {
            continue;
        }
        j += 1;
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        if j >= b.len() {
            return None;
        }
        if b[j] == b'"' || b[j] == b'\'' {
            let start = j + 1;
            let end = start + b[start..].iter().position(|c| *c == b[j])?;
            return Some(String::from_utf8_lossy(&b[start..end]).into_owned());
        }
        let end = b[j..]
            .iter()
            .position(|c| c.is_ascii_whitespace())
            .map(|k| j + k)
            .unwrap_or(b.len());
        return Some(String::from_utf8_lossy(&b[j..end]).into_owned());
    }
    None
}

/// meta JSON 路径：`<root>/<dir>/<sanitized path>.json`；空路径 → `index.json`。
/// 段守卫与 resolve_static 同款（防 `%2e%2e` / `..` 走私）。
fn meta_json_path(root: &Path, dir: &str, rel_path: &str) -> Option<PathBuf> {
    let rel = rel_path
        .strip_prefix('/')
        .unwrap_or(rel_path)
        .trim_end_matches('/');
    let mut p = root.to_path_buf();
    p.push(dir);
    if rel.is_empty() {
        p.push("index.json");
        return Some(p);
    }
    for seg in rel.split('/') {
        let s = percent_encoding::percent_decode_str(seg)
            .decode_utf8()
            .ok()?;
        if s.is_empty() || s == "." || s == ".." || s.contains(['/', '\\', '\0']) {
            return None;
        }
        p.push(s.as_ref());
    }
    p.set_extension("json");
    Some(p)
}

/// HTML 文本/属性值转义（防 meta JSON 里的 `"`/`</title>` 破坏文档结构）。
fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// blob 下载 key：percent-decode 每段后过 valid_key（防 `%2e%2e` 穿越，与 resolve_static 同款守卫）。
fn decode_blob_key(s: &str) -> Option<String> {
    let decoded = s
        .split('/')
        .map(|seg| percent_encoding::percent_decode_str(seg).decode_utf8().ok())
        .collect::<Option<Vec<_>>>()?
        .join("/");
    only_js::bridge::valid_key(&decoded).then_some(decoded)
}

/// 静态文件解析：uri.path()（仍 percent-encoded）逐段解码后拼 root；
/// 根/目录 → index.html；越界段（`.`/`..`/`\`/`/`/`\0`/空段，含解码后——
/// `%2F` 走私等价穿越）→ None（404）。
/// 请求路径 → 命中站点的相对路径（剥站点 `prefix`，多站点最长命中由调用方保证）。
/// 前缀 "/" → 原样（全路径兜底，行为与旧版一致）；非 "/" 前缀：精确命中前缀根
/// → "/"（resolve_static 落 index.html），`前缀/...` → "/..."，前缀外（含仅前缀
/// 更长串如 `/sitex`）→ None（404）。
/// 自由函数（handle() 处 `st` 已被闭包部分移动，方法接收者会整借 `st`）。
fn strip_app_prefix<'a>(prefix: &str, path: &'a str) -> Option<&'a str> {
    if prefix == "/" {
        return Some(path);
    }
    if path == prefix {
        return Some("/");
    }
    path.strip_prefix(prefix)
        .filter(|rest| rest.starts_with('/'))
}

/// `meta_dir`：`server.html_meta` 的目录名——该目录是**数据**不是站点资产，
/// 命中即 404（v0.1.20）。
fn resolve_static(root: &Path, uri_path: &str, meta_dir: Option<&str>) -> Option<PathBuf> {
    let rel = uri_path.strip_prefix('/')?.trim_end_matches('/');
    let mut p = root.to_path_buf();
    if !rel.is_empty() {
        for seg in rel.split('/') {
            let s = percent_encoding::percent_decode_str(seg)
                .decode_utf8()
                .ok()?;
            if s.is_empty() || s == "." || s == ".." || s.contains(['/', '\\', '\0']) {
                return None;
            }
            if meta_dir.is_some_and(|d| d == s.as_ref()) {
                return None;
            }
            p.push(s.as_ref());
        }
    }
    if p.is_dir() {
        p.push("index.html");
    }
    p.is_file().then_some(p)
}

/// 扩展名 → Content-Type（常见集，未知 = octet-stream——ponytail: 嫌少再加）。
fn mime_of(p: &Path) -> &'static str {
    match p
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css",
        "js" | "mjs" => "text/javascript",
        "json" | "map" => "application/json",
        "txt" | "md" => "text/plain; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "wasm" => "application/wasm",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "xml" => "application/xml",
        "yaml" | "yml" => "application/yaml",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

fn file_response(file: &Path, body: Vec<u8>) -> Response {
    let mut r = Response::new(axum::body::Body::from(body));
    r.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_str(mime_of(file)).expect("valid mime"),
    );
    r
}

/// Capture → axum Response（status/headers/body 原样回写）。
/// 流式响应（v0.1.35）：`cap.stream` 为 Some 时以 `Body::from_stream` 逐块写出，绕过信封。
fn capture_response(cap: only_js::bridge::Capture) -> Response {
    let mut r = if let Some(rx) = cap.stream {
        // 接收端逐块收，映射到 `Result<Bytes, io::Error>` 交给 axum（chunked 传输）。
        // 不用 `StreamExt::map`：tokio 1.x 的 mpsc 接收端未实现 `Stream` trait（无 stream
        // feature），改用 unfold + recv() 自建流。
        use futures_util::stream::unfold;
        let s = unfold(rx, |mut rx| async move {
            rx.recv().await.map(|b| (Ok::<_, std::io::Error>(b), rx))
        });
        Response::new(axum::body::Body::from_stream(s))
    } else {
        Response::new(axum::body::Body::from(cap.body))
    };
    *r.status_mut() = StatusCode::from_u16(cap.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    for (k, v) in cap.headers {
        if let (Ok(name), Ok(hv)) = (
            k.parse::<axum::http::HeaderName>(),
            v.parse::<axum::http::HeaderValue>(),
        ) {
            r.headers_mut().insert(name, hv);
        }
    }
    r
}

/// 统一失败信封。
fn fail_response(code: i32, msg: &str) -> Response {
    let (body, status) = fail(code, msg, &Value::Null);
    let mut r = Response::new(axum::body::Body::from(body));
    *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    r
}

/// Range 头解析结果（oj-7）：Full = 非区间/非法/多区间（ponytail: 多区间不做
/// multipart/byteranges 响应，回落 200 全量——单区间覆盖 pdf.js/媒体 seek 的全部场景）；
/// Partial(start, end)（含端点，已按 len 钳制）；Unsatisfiable = 越界/空文件 → 416。
#[derive(Debug, PartialEq, Eq)]
enum RangeSpec {
    Full,
    Partial(usize, usize),
    Unsatisfiable,
}

fn parse_range(header: Option<&axum::http::HeaderValue>, len: usize) -> RangeSpec {
    let Some(h) = header else {
        return RangeSpec::Full;
    };
    let Ok(s) = h.to_str() else {
        return RangeSpec::Full;
    };
    let Some(rest) = s.trim().strip_prefix("bytes=") else {
        return RangeSpec::Full;
    };
    // ponytail: 多区间（`bytes=a-b,c-d`）→ 200 全量。
    if rest.contains(',') {
        return RangeSpec::Full;
    }
    let (a, b) = match rest.split_once('-') {
        Some(v) => v,
        None => return RangeSpec::Full,
    };
    let parse = |s: &str| s.trim().parse::<usize>().ok();
    match (parse(a), parse(b)) {
        // "a-b"
        (Some(start), Some(end)) if start <= end => {
            if len == 0 || start >= len {
                RangeSpec::Unsatisfiable
            } else {
                RangeSpec::Partial(start, end.min(len - 1))
            }
        }
        // "a-"（到末尾）
        (Some(start), None) => {
            if len == 0 || start >= len {
                RangeSpec::Unsatisfiable
            } else {
                RangeSpec::Partial(start, len - 1)
            }
        }
        // "-N"（末尾 N 字节）
        (None, Some(n)) => {
            if len == 0 || n == 0 {
                RangeSpec::Unsatisfiable
            } else {
                RangeSpec::Partial(len.saturating_sub(n), len - 1)
            }
        }
        _ => RangeSpec::Full,
    }
}

/// oj-8：自定义响应头补缺——**只插入响应里还没有的头**（框架自有头 Content-Type/
/// Content-Length/Location/Content-Range 等永远优先）；非法头名/值静默跳过
/// （与 capture_response 对 cap.headers 的宽容解析同款语义）。
fn apply_custom_headers(r: &mut Response, extra: &[(String, String)]) {
    for (k, v) in extra {
        if let (Ok(name), Ok(hv)) = (
            k.parse::<axum::http::HeaderName>(),
            axum::http::HeaderValue::from_str(v),
        ) && !r.headers().contains_key(&name)
        {
            r.headers_mut().insert(name, hv);
        }
    }
}

/// 站点级响应头 = 全局默认 + 该站点覆盖（同名替换值，不删全局其它头）。
fn site_headers<'a>(st: &'a AppState, site: &'a StaticSite) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = st.static_opts.response_headers.clone();
    for (k, v) in &site.headers {
        out.retain(|(ek, _)| ek != k);
        out.push((k.clone(), v.clone()));
    }
    out
}

/// `a=1&b=2` → map（application/x-www-form-urlencoded 解码，+ → 空格、%xx → 字节）。
fn parse_query(q: Option<&str>) -> HashMap<String, String> {
    q.map(|s| {
        form_urlencoded::parse(s.as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect()
    })
    .unwrap_or_default()
}

/// content-type 是否 multipart/form-data。
fn is_multipart(headers: &HeaderMap) -> bool {
    headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| s.starts_with("multipart/form-data"))
}

/// blob 直传 PUT 的执行体（v0.1.38）：流式 `put_stream_*` 优先（内存恒定，总量闸
/// blob_upload_max，超限 abort + 信封 413）；后端不支持流式（put_stream_open Err，
/// 如旧形态后端/网关类实现）→ 回落整段缓冲 put（上限 blob_upload_max，语义同旧版）。
async fn blob_put_direct(
    st: &AppState,
    blob: std::sync::Arc<dyn only_js::bridge::BlobBackend>,
    key: &str,
    headers: &HeaderMap,
    body: axum::body::Body,
) -> Response {
    let ct = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let ok_resp = || {
        let mut r = Response::new(axum::body::Body::from(only_js::bridge::ok(
            &serde_json::Value::Null,
        )));
        *r.status_mut() = StatusCode::OK;
        r.headers_mut().insert(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("application/json"),
        );
        apply_custom_headers(&mut r, &st.static_opts.response_headers);
        r
    };
    match blob.put_stream_open(key, ct.as_deref()).await {
        Ok(upload_id) => {
            let mut total: u64 = 0;
            let mut too_large = false;
            let mut err: Option<String> = None;
            let mut ds = body.into_data_stream();
            use futures_util::StreamExt;
            while let Some(chunk) = ds.next().await {
                match chunk {
                    Ok(b) => {
                        total += b.len() as u64;
                        if total > st.pipeline.blob_upload_max {
                            too_large = true;
                            break;
                        }
                        if let Err(e) = blob.put_stream_chunk(upload_id, &b).await {
                            err = Some(e.to_string());
                            break;
                        }
                    }
                    Err(e) => {
                        err = Some(format!("read body: {e}"));
                        break;
                    }
                }
            }
            if too_large || err.is_some() {
                let _ = blob.put_stream_abort(upload_id).await;
                if too_large {
                    return fail_response(413, "upload too large");
                }
                tracing::warn!(
                    key,
                    err = err.as_deref().unwrap_or("?"),
                    "blob streaming upload failed"
                );
                return fail_response(500, "blob upload failed");
            }
            match blob.put_stream_finish(upload_id).await {
                Ok(()) => ok_resp(),
                Err(_) => fail_response(500, "blob upload failed"),
            }
        }
        // 回落：整段缓冲（上限 blob_upload_max）。
        Err(_) => match axum::body::to_bytes(body, st.pipeline.blob_upload_max as usize).await {
            Ok(b) => match blob.put(key, &b, ct.as_deref()).await {
                Ok(()) => ok_resp(),
                Err(_) => fail_response(500, "blob upload failed"),
            },
            Err(_) => fail_response(413, "upload too large"),
        },
    }
}

/// 流式上传的对象 key（服务端代分配；handler 经 `http.files[i].key`/`.url` 取）。
/// `uploads/<ns 时间戳>-<进程内序号>-<安全化文件名>`——段非空、无穿越字符（valid_key 直通）。
fn gen_upload_key(filename: &str) -> String {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let safe: String = filename
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let safe = safe.trim_matches(|c| c == '.' || c == '_').to_string();
    let safe = if safe.is_empty() {
        "file.bin".to_string()
    } else {
        safe
    };
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "uploads/{nanos:x}-{}-{safe}",
        N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    )
}

/// multer 错误 → 响应：超限 413（FieldSizeExceeded / StreamSizeExceeded），其余 400。
fn multipart_err_response(e: multer::Error) -> Response {
    match &e {
        multer::Error::FieldSizeExceeded { .. } | multer::Error::StreamSizeExceeded { .. } => {
            fail_response(413, "upload too large")
        }
        _ => fail_response(400, &format!("multipart parse: {e}")),
    }
}

/// ABI 10 流式 multipart 解析（v0.1.38）：
/// - 文本字段：缓冲并入 body（{name: value}），累计 ≤ max_upload（防内存 DoS）；
/// - 文件字段 ≤ max_upload：缓冲进 bytes（`http.file(i)` 旧行为不变）；
/// - 文件字段 > max_upload：**转流式** `put_stream_*` 直落 blob（单文件上限
///   blob_upload_max），bytes 置空、`key`/`url` 回填（大文件走 `blob.get(key)` /
///   `files[i].url`）；任意失败 abort 会话（防 local 临时文件泄漏 / s3 orphan parts）。
///   multer 总闸 = max_upload + blob_upload_max；multer 报超限 → 413。
async fn stream_multipart(
    st: &AppState,
    headers: &HeaderMap,
    body: axum::body::Body,
) -> Result<(Vec<u8>, Vec<UploadedFile>), Box<Response>> {
    let boundary = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split("boundary=").nth(1))
        .map(|b| b.trim().trim_matches('"'))
        .unwrap_or_default();
    let blob = st
        .pipeline
        .blob
        .as_ref()
        .expect("caller checked blob configured");
    let max_up = st.pipeline.max_upload;
    let blob_cap = st.pipeline.blob_upload_max;
    let constraints = multer::Constraints::new().size_limit(
        multer::SizeLimit::new()
            .whole_stream(max_up + blob_cap)
            .per_field(blob_cap),
    );
    let mut mp =
        multer::Multipart::with_constraints(body.into_data_stream(), boundary, constraints);
    let mut fields = serde_json::Map::new();
    let mut files = Vec::new();
    while let Some(mut field) = mp
        .next_field()
        .await
        .map_err(multipart_err_response)
        .map_err(Box::new)?
    {
        let name = field.name().unwrap_or_default().to_string();
        let filename = field.file_name().unwrap_or_default().to_string();
        let content_type = field.content_type().map(|c| c.to_string());
        if filename.is_empty() {
            // 文本字段：缓冲（累计 ≤ max_upload；multer per_field 闸是 blob_cap，
            // 文本内存闸以本计数为准，先到先拦）。
            let mut buf: Vec<u8> = Vec::new();
            loop {
                match field.chunk().await {
                    Ok(Some(chunk)) => {
                        if buf.len() as u64 + chunk.len() as u64 > max_up {
                            return Err(Box::new(fail_response(413, "upload too large")));
                        }
                        buf.extend_from_slice(&chunk);
                    }
                    Ok(None) => break,
                    Err(e) => return Err(Box::new(multipart_err_response(e))),
                }
            }
            fields.insert(
                name,
                serde_json::Value::String(String::from_utf8_lossy(&buf).into_owned()),
            );
        } else {
            // 文件字段：缓冲到 max_upload 为止；超过转流式（blob_cap 闸）。
            let mut buf: Vec<u8> = Vec::new();
            let mut session: Option<(u64, String)> = None;
            let mut total: u64 = 0;
            let mut too_large = false;
            loop {
                match field.chunk().await {
                    Ok(Some(chunk)) => {
                        total += chunk.len() as u64;
                        if total > blob_cap {
                            too_large = true;
                            break;
                        }
                        if session.is_none() && buf.len() as u64 + chunk.len() as u64 > max_up {
                            // 缓冲阈值越过 → 转流式：开 session + flush 已缓冲字节
                            let key = gen_upload_key(&filename);
                            match blob.put_stream_open(&key, content_type.as_deref()).await {
                                Ok(id) => {
                                    if let Err(e) = blob.put_stream_chunk(id, &buf).await {
                                        let _ = blob.put_stream_abort(id).await;
                                        return Err(Box::new(fail_response(
                                            500,
                                            &format!("blob upload failed: {e}"),
                                        )));
                                    }
                                    buf = Vec::new();
                                    session = Some((id, key));
                                }
                                Err(_) => {
                                    // 后端不支持流式且文件 > max_upload：无可回落面（
                                    // 缓冲会突破内存闸）→ 413 拒绝
                                    return Err(Box::new(fail_response(413, "upload too large")));
                                }
                            }
                        }
                        if let Some((id, _)) = session.as_ref() {
                            if let Err(e) = blob.put_stream_chunk(*id, &chunk).await {
                                let _ = blob.put_stream_abort(*id).await;
                                return Err(Box::new(fail_response(
                                    500,
                                    &format!("blob upload failed: {e}"),
                                )));
                            }
                        } else {
                            buf.extend_from_slice(&chunk);
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        if let Some((id, _)) = session.take() {
                            let _ = blob.put_stream_abort(id).await;
                        }
                        return Err(Box::new(multipart_err_response(e)));
                    }
                }
            }
            if too_large {
                if let Some((id, _)) = session.take() {
                    let _ = blob.put_stream_abort(id).await;
                }
                return Err(Box::new(fail_response(413, "upload too large")));
            }
            if let Some((id, key)) = session {
                // 流式完成：bytes 不回填，key/url 交给 handler
                match blob.put_stream_finish(id).await {
                    Ok(()) => {
                        let url = blob.url(&key).await.ok();
                        files.push(UploadedFile {
                            field: name,
                            filename,
                            content_type,
                            bytes: Vec::new(),
                            size: total,
                            key: Some(key),
                            url,
                        });
                    }
                    Err(e) => {
                        return Err(Box::new(fail_response(
                            500,
                            &format!("blob upload failed: {e}"),
                        )));
                    }
                }
            } else {
                // 小文件（≤ max_upload）：旧行为，bytes 回填
                files.push(UploadedFile {
                    field: name,
                    filename,
                    content_type,
                    bytes: buf,
                    size: total,
                    key: None,
                    url: None,
                });
            }
        }
    }
    Ok((serde_json::to_vec(&fields).unwrap_or_default(), files))
}

/// multer 解析：文本字段 → {name: value}，文件 → Vec<UploadedFile>。
/// body 已整体在内存（DefaultBodyLimit 上限内），用 once stream 喂 multer。
/// （v0.1.38 起仅 blob 未配置/小文件路径经此；流式路径见 `stream_multipart`。）
async fn parse_multipart(headers: &HeaderMap, body: &[u8]) -> (Vec<u8>, Vec<UploadedFile>) {
    let boundary = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split("boundary=").nth(1))
        .map(|b| b.trim().trim_matches('"'))
        .unwrap_or_default();
    let stream = futures_util::stream::once(async move {
        Ok::<_, multer::Error>(axum::body::Bytes::from(body.to_vec()))
    });
    let mut mp = multer::Multipart::new(stream, boundary);
    let mut fields = serde_json::Map::new();
    let mut files = Vec::new();
    while let Some(f) = mp.next_field().await.ok().flatten() {
        let name = f.name().unwrap_or_default().to_string();
        let filename = f.file_name().unwrap_or_default().to_string();
        let content_type = f.content_type().map(|s| s.to_string());
        let bytes = f.bytes().await.unwrap_or_default().to_vec();
        if filename.is_empty() {
            fields.insert(
                name,
                Value::String(String::from_utf8_lossy(&bytes).into_owned()),
            );
        } else {
            files.push(UploadedFile {
                field: name,
                filename,
                content_type,
                size: bytes.len() as u64,
                bytes,
                key: None,
                url: None,
            });
        }
    }
    (serde_json::to_vec(&fields).unwrap_or_default(), files)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use only_js::bridge::{Bridge, Extras, InMemoryKV, LoaderShared, LocalBlob, SchemaRegistry};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Helper for tests: create a minimal AppState
    pub(crate) fn dummy_app_state() -> AppState {
        AppState {
            table: RouteTable::default(),
            fallback: None,
            actor: make_actor(PathBuf::from("."), false),
            timeout: None,
            static_sites: Vec::new(),
            static_opts: StaticOpts::default(),
            pipeline: Pipeline::default(),
            base: "/v1/api".to_string(),
            certificate_status: Arc::new(RwLock::new(CertificateStatus::Valid)),
            certificate_valid_until: Arc::new(RwLock::new(None)),
            plugins: Arc::default(),
        }
    }

    pub(crate) struct TempRoutes(pub(crate) PathBuf);
    pub(crate) fn routes(files: &[(&str, &str)]) -> TempRoutes {
        static N: AtomicUsize = AtomicUsize::new(0);
        let base = std::env::temp_dir().join(format!(
            "oj-server-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&base).unwrap();
        for (rel, content) in files {
            let p = base.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        }
        TempRoutes(base)
    }
    impl Drop for TempRoutes {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// 带 oj 模块加载器的 actor（project_root = 路由根：api 文件全在其下，clamp 可达）。
    pub(crate) fn make_actor(root: PathBuf, ts: bool) -> JsActor {
        JsActor::new(move || {
            Bridge::with_dbs_and_loader(
                HashMap::new(),
                Arc::new(InMemoryKV::new()),
                SchemaRegistry::new(),
                false,
                Some(Arc::new(LoaderShared {
                    project_root: root.clone(),
                    ts,
                })),
                Extras::default(),
            )
        })
    }

    pub(crate) async fn spawn_server(
        base: &str,
        dir: PathBuf,
        ts: bool,
        timeout: Option<std::time::Duration>,
    ) -> std::net::SocketAddr {
        spawn_pipeline(base, dir, ts, timeout, Pipeline::default()).await
    }

    pub(crate) async fn spawn_pipeline(
        base: &str,
        dir: PathBuf,
        ts: bool,
        timeout: Option<std::time::Duration>,
        pipeline: Pipeline,
    ) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let table = build_table(&dir, ts, base);
        let base = base.to_string();
        tokio::spawn(async move {
            serve_with_listener(
                listener,
                &base,
                dir.clone(),
                ts,
                table,
                make_actor(dir, ts),
                timeout,
                Vec::new(),
                StaticOpts::default(),
                pipeline,
                None,
            )
            .await
            .unwrap();
        });
        addr
    }

    /// blob 启用的 serve fixture：actor 与 Pipeline 共享同一 backend。
    async fn spawn_blob(
        base: &str,
        dir: PathBuf,
        blob: Arc<dyn BlobBackend>,
    ) -> std::net::SocketAddr {
        spawn_blob_with(base, dir, blob, Pipeline::default()).await
    }

    /// spawn_blob + 自定义 Pipeline（流式上传测试用：max_upload / blob_upload_max 分档）。
    async fn spawn_blob_with(
        base: &str,
        dir: PathBuf,
        blob: Arc<dyn BlobBackend>,
        mut pipeline: Pipeline,
    ) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let table = build_table(&dir, true, base);
        let root = dir.clone();
        let blob2 = blob.clone();
        let actor = JsActor::new(move || {
            Bridge::with_dbs_and_loader(
                HashMap::new(),
                Arc::new(InMemoryKV::new()),
                SchemaRegistry::new(),
                false,
                Some(Arc::new(LoaderShared {
                    project_root: root.clone(),
                    ts: true,
                })),
                Extras {
                    blobs: Some(only_js::bridge::blob::registry_with_default(blob2.clone())),
                    ..Default::default()
                },
            )
        });
        let base = base.to_string();
        pipeline.blob = Some(blob);
        tokio::spawn(async move {
            serve_with_listener(
                listener,
                &base,
                dir.clone(),
                true,
                table,
                actor,
                None,
                Vec::new(),
                StaticOpts::default(),
                pipeline,
                None,
            )
            .await
            .unwrap();
        });
        addr
    }

    /// 建表：真实内省（bridge_introspector）。失败清单按设计只跳过+记日志，
    /// 不在此断言（conflict / broken 夹具本身就要产生 failures）。
    pub(crate) fn build_table(dir: &Path, ts: bool, base: &str) -> crate::routes::RouteTable {
        let root = dir.canonicalize().unwrap();
        let make = {
            let root = root.clone();
            move || {
                Bridge::with_dbs_and_loader(
                    HashMap::new(),
                    Arc::new(InMemoryKV::new()),
                    SchemaRegistry::new(),
                    false,
                    Some(Arc::new(LoaderShared {
                        project_root: root.clone(),
                        ts,
                    })),
                    Extras::default(),
                )
            }
        };
        let (t, failures) = crate::routes::RouteTable::build(
            base,
            &root,
            ts,
            crate::routes::bridge_introspector(make),
        );
        if !failures.is_empty() {
            eprintln!("build_table failures: {failures:?}");
        }
        t
    }

    /// ABI 10 流式 multipart：大文件（> max_upload）转 put_stream 落 blob——bytes 置空、
    /// key/url 回填、handler 经 blob.get(key) 取回全文、http.file(i) 明确报错指路；
    /// 小文件（≤ max_upload）仍缓冲（http.file(i) 兼容）；文本字段语义不变。
    #[tokio::test]
    async fn multipart_streaming_big_file_to_blob_small_file_buffered() {
        let t = routes(&[(
            "u/api.ts",
            "export default { async post() {\n\
               const f = http.files[0];\n\
               let fileErr = null, content = null, blobLen = null;\n\
               try { const b = await http.file(0); content = b.length; } catch (e) { fileErr = String(e); }\n\
               if (f && f.key) blobLen = (await blob.get(f.key)).length;\n\
               json.ok({ name: f ? f.filename : null, size: f ? f.size : null, key: f && f.key ? f.key : null, url: f && f.url ? true : false, fileErr, content, blobLen, note: http.body.note });\n\
             } };",
        )]);
        let root = std::env::temp_dir().join(format!("oj-blob-srm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let blob: Arc<dyn BlobBackend> = Arc::new(LocalBlob::new(&root, "/v1/api").unwrap());
        let addr = spawn_blob_with(
            "/v1/api",
            t.0.clone(),
            blob,
            Pipeline {
                max_upload: 8,
                blob_upload_max: 64000,
                ..Default::default()
            },
        )
        .await;
        let send = |body: String| {
            format!(
                "POST /v1/api/u/ HTTP/1.1\r\nHost: t\r\nContent-Type: multipart/form-data; boundary=X-BND\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
        };
        // 大文件 10000B（> max_upload=8）→ 流式落 blob
        let big = "A".repeat(10000);
        let body = format!(
            "--X-BND\r\nContent-Disposition: form-data; name=\"note\"\r\n\r\nhi\r\n--X-BND\r\nContent-Disposition: form-data; name=\"file\"; filename=\"big.txt\"\r\n\r\n{big}\r\n--X-BND--\r\n"
        );
        let r = raw_http(addr, &send(body)).await;
        let v: Value =
            serde_json::from_slice(r.split("\r\n\r\n").nth(1).unwrap_or("null").as_bytes())
                .unwrap();
        assert!(r.starts_with("HTTP/1.1 200"), "1: {r}");
        assert_eq!(v["data"]["name"], "big.txt", "{v}");
        assert_eq!(v["data"]["size"], 10000, "{v}");
        let key = v["data"]["key"]
            .as_str()
            .expect("streamed file must have key");
        assert!(key.starts_with("uploads/"), "{v}");
        assert_eq!(v["data"]["url"], true, "{v}");
        assert_eq!(
            v["data"]["blobLen"], 10000,
            "handler 经 blob.get(key) 取回全文: {v}"
        );
        assert_eq!(v["data"]["content"], Value::Null, "{v}");
        assert!(
            v["data"]["fileErr"]
                .as_str()
                .unwrap_or_default()
                .contains("streamed to blob"),
            "{v}"
        );
        assert_eq!(v["data"]["note"], "hi", "{v}");
        // 小文件 5B（≤ max_upload=8）→ 旧行为：bytes 回填，key 为空
        let body = "--X-BND\r\nContent-Disposition: form-data; name=\"note\"\r\n\r\nhi\r\n--X-BND\r\nContent-Disposition: form-data; name=\"file\"; filename=\"s.txt\"\r\n\r\nsmall\r\n--X-BND--\r\n".to_string();
        let r = raw_http(addr, &send(body)).await;
        let v: Value =
            serde_json::from_slice(r.split("\r\n\r\n").nth(1).unwrap_or("null").as_bytes())
                .unwrap();
        assert!(r.starts_with("HTTP/1.1 200"), "2: {r}");
        assert_eq!(v["data"]["content"], 5, "{v}");
        assert_eq!(v["data"]["key"], Value::Null, "{v}");
        assert_eq!(v["data"]["blobLen"], Value::Null, "{v}");
        assert_eq!(v["data"]["fileErr"], Value::Null, "{v}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// ABI 10 流式 multipart：单文件超 blob_upload_max → abort 会话 + 413。
    #[tokio::test]
    async fn multipart_streaming_oversize_field_413() {
        let t = routes(&[(
            "u/api.ts",
            "export default { async post() { json.ok({}); } };",
        )]);
        let root = std::env::temp_dir().join(format!("oj-blob-sr4-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let blob: Arc<dyn BlobBackend> = Arc::new(LocalBlob::new(&root, "/v1/api").unwrap());
        let addr = spawn_blob_with(
            "/v1/api",
            t.0.clone(),
            blob,
            Pipeline {
                max_upload: 8,
                blob_upload_max: 100,
                ..Default::default()
            },
        )
        .await;
        let big = "B".repeat(200);
        let body = format!(
            "--X-BND\r\nContent-Disposition: form-data; name=\"file\"; filename=\"x.bin\"\r\n\r\n{big}\r\n--X-BND--\r\n"
        );
        let req = format!(
            "POST /v1/api/u/ HTTP/1.1\r\nHost: t\r\nContent-Type: multipart/form-data; boundary=X-BND\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let r = raw_http(addr, &req).await;
        assert!(
            r.starts_with("HTTP/1.1 413") && r.contains("upload too large"),
            "{r}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// ABI 10 blob 直传 PUT 腿：流式落盘（local put_stream），内容一致；超
    /// blob_upload_max → 413 信封。
    #[tokio::test]
    async fn blob_put_route_streams_to_backend() {
        let t = routes(&[("n/api.ts", "export default { get() { json.ok({}); } };")]);
        let root = std::env::temp_dir().join(format!("oj-blob-srp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let blob: Arc<dyn BlobBackend> = Arc::new(LocalBlob::new(&root, "/v1/api").unwrap());
        let addr = spawn_blob_with(
            "/v1/api",
            t.0.clone(),
            blob,
            Pipeline {
                blob_upload_max: 100,
                ..Default::default()
            },
        )
        .await;
        // PUT 60B（≤100）→ 200；GET 回读一致（经流式腿落盘，非缓冲腿）
        let body = "C".repeat(60);
        let req = format!(
            "PUT /v1/api/blob/stream.bin HTTP/1.1\r\nHost: t\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let r = raw_http(addr, &req).await;
        assert!(r.starts_with("HTTP/1.1 200"), "put: {r}");
        let r = raw_http(
            addr,
            "GET /v1/api/blob/stream.bin HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(
            r.starts_with("HTTP/1.1 200") && r.ends_with(&body),
            "get: {r}"
        );
        // PUT 120B（>100）→ 413 信封（abort 路径无残留）
        let big = "D".repeat(120);
        let req = format!(
            "PUT /v1/api/blob/big.bin HTTP/1.1\r\nHost: t\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{big}",
            big.len()
        );
        let r = raw_http(addr, &req).await;
        assert!(
            r.starts_with("HTTP/1.1 413") && r.contains("upload too large"),
            "big: {r}"
        );
        assert!(!root.join("big.bin").exists(), "超限对象不得落盘");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 探测：blob 直传**写面**必须过守卫——无 token PUT → 401（"写面不能公开"不能只
    /// 靠注释声明）；带合法 token → 200。
    #[tokio::test]
    async fn probe_blob_put_route_requires_auth() {
        let t = routes(&[("n/api.ts", "export default { get() { json.ok({}); } };")]);
        let root = std::env::temp_dir().join(format!("oj-blob-ppa-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let blob: Arc<dyn BlobBackend> = Arc::new(LocalBlob::new(&root, "/v1/api").unwrap());
        let addr = spawn_blob_with(
            "/v1/api",
            t.0.clone(),
            blob,
            Pipeline {
                auth: Some(Arc::new(StubGuard)),
                ..Default::default()
            },
        )
        .await;
        // 无 token → 401（对象不得落盘）
        let r = raw_http(
            addr,
            "PUT /v1/api/blob/evil.bin HTTP/1.1\r\nHost: t\r\nContent-Length: 4\r\nConnection: close\r\n\r\nDATA",
        )
        .await;
        assert!(r.starts_with("HTTP/1.1 401"), "写面必须过守卫: {r}");
        assert!(!root.join("evil.bin").exists(), "未授权上传不得落盘");
        // 合法 token → 200
        let r = raw_http(
            addr,
            "PUT /v1/api/blob/ok.bin HTTP/1.1\r\nHost: t\r\nAuthorization: Bearer good\r\nContent-Length: 4\r\nConnection: close\r\n\r\nDATA",
        )
        .await;
        assert!(r.starts_with("HTTP/1.1 200"), "authz 上传须成功: {r}");
        assert!(root.join("ok.bin").is_file());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 探测：流式上传的**服务端代分配 key** 必须净化穿越文件名——filename 带
    /// `../../` 时对象必须落在 uploads/ 段内、root 不得出现逃逸文件、key 可安全回读。
    #[tokio::test]
    async fn probe_streamed_key_sanitizes_traversal_filename() {
        let t = routes(&[(
            "u/api.ts",
            "export default { async post() {\n\
               const f = http.files[0];\n\
               const content = f.key ? (await blob.get(f.key)).length : null;\n\
               json.ok({ key: f ? f.key : null, size: f ? f.size : null, content });\n\
             } };",
        )]);
        let root = std::env::temp_dir().join(format!("oj-blob-pks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let blob: Arc<dyn BlobBackend> = Arc::new(LocalBlob::new(&root, "/v1/api").unwrap());
        let addr = spawn_blob_with(
            "/v1/api",
            t.0.clone(),
            blob,
            Pipeline {
                max_upload: 8,
                blob_upload_max: 64000,
                ..Default::default()
            },
        )
        .await;
        let big = "E".repeat(500); // > max_upload=8 → 走流式
        let body = format!(
            "--X-BND\r\nContent-Disposition: form-data; name=\"file\"; filename=\"../../evil.sh\"\r\n\r\n{big}\r\n--X-BND--\r\n"
        );
        let req = format!(
            "POST /v1/api/u/ HTTP/1.1\r\nHost: t\r\nContent-Type: multipart/form-data; boundary=X-BND\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let r = raw_http(addr, &req).await;
        let v: Value =
            serde_json::from_slice(r.split("\r\n\r\n").nth(1).unwrap_or("null").as_bytes())
                .unwrap();
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        let key = v["data"]["key"].as_str().expect("{v}");
        assert!(key.starts_with("uploads/"), "key 必须在 uploads/ 段内: {v}");
        assert!(
            !key.contains("..") && !key.contains('\\'),
            "key 不得含穿越字符: {v}"
        );
        assert_eq!(v["data"]["content"], 500, "key 须可安全回读: {v}");
        // root 下不得出现逃逸文件（净化后的 safe 名只会出现在 uploads/ 段内）
        assert!(!root.join("evil.sh").exists(), "穿越文件名不得逃逸: {v}");
        assert!(root.join(key).is_file(), "对象实际落盘位置 = key: {v}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 探测：两个内存闸——文本字段 > max_upload → 413（防文本字段撑内存）；
    /// 多个小文件各自合规但**总和**超 whole_stream 闸 → 413（multer StreamSizeExceeded）。
    #[tokio::test]
    async fn probe_multipart_text_oversize_and_total_gate_413() {
        let t = routes(&[(
            "u/api.ts",
            "export default { async post() { json.ok({}); } };",
        )]);
        let root = std::env::temp_dir().join(format!("oj-blob-pgt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let blob: Arc<dyn BlobBackend> = Arc::new(LocalBlob::new(&root, "/v1/api").unwrap());
        let addr = spawn_blob_with(
            "/v1/api",
            t.0.clone(),
            blob,
            Pipeline {
                max_upload: 8,
                blob_upload_max: 100,
                ..Default::default()
            },
        )
        .await;
        let send = |body: String| {
            format!(
                "POST /v1/api/u/ HTTP/1.1\r\nHost: t\r\nContent-Type: multipart/form-data; boundary=X-BND\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
        };
        // ① 文本字段 20B > max_upload=8 → 413
        let body = format!(
            "--X-BND\r\nContent-Disposition: form-data; name=\"note\"\r\n\r\n{}\r\n--X-BND--\r\n",
            "N".repeat(20)
        );
        let r = raw_http(addr, &send(body)).await;
        assert!(r.starts_with("HTTP/1.1 413"), "文本字段超限必须 413: {r}");
        // ② 三个 40B 文件（各自 ≤ blob_cap=100）总和 120 > whole_stream(8+100=108) → 413
        let f = |n: &str| {
            format!(
                "--X-BND\r\nContent-Disposition: form-data; name=\"{n}\"; filename=\"{n}.bin\"\r\n\r\n{}\r\n",
                "F".repeat(40)
            )
        };
        let body = format!("{}{}{}--X-BND--\r\n", f("a"), f("b"), f("c"));
        let r = raw_http(addr, &send(body)).await;
        assert!(r.starts_with("HTTP/1.1 413"), "总和超总闸必须 413: {r}");
        let _ = std::fs::remove_dir_all(&root);
    }

    async fn raw_http(addr: std::net::SocketAddr, req: &str) -> String {
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(req.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).await.unwrap();
        String::from_utf8_lossy(&buf).into_owned()
    }

    // tenant.enable：header 存在 → 注入 http.tenantId；缺失 → 400；未启用 → null。
    // ===== PR-9 租户声明绑定（v0.1.41）：击穿测试 =====

    /// 带 claims.tenant 的假守卫（Bearer good → tenant=t1；Bearer plain → 无 tenant claim）。
    struct ClaimStubGuard;
    impl only_js::bridge::AuthGuard for ClaimStubGuard {
        fn verify(
            &self,
            path: &str,
            _method: &str,
            auth: Option<&str>,
            _headers: Option<&str>,
        ) -> Result<Option<Value>, String> {
            if path == "/health" {
                return Ok(None);
            }
            match auth {
                Some("Bearer good") => Ok(Some(serde_json::json!({
                    "id": "1", "roles": ["admin"],
                    "claims": {"sub": "1", "tenant": "t1"},
                }))),
                Some("Bearer plain") => Ok(Some(serde_json::json!({
                    "id": "2", "roles": ["user"],
                    "claims": {"sub": "2"},
                }))),
                _ => Err("missing or invalid bearer token".into()),
            }
        }
    }

    async fn claim_guard_fixture() -> (std::net::SocketAddr, TempRoutes) {
        let t = routes(&[(
            "u/api.ts",
            "export default { get() { json.ok({ t: http.tenantId, uid: http.user ? http.user.id : null }); } };",
        )]);
        let addr = spawn_pipeline(
            "/v1/api",
            t.0.clone(),
            true,
            None,
            Pipeline {
                tenant_header: Some("X-TENANT-ID".into()),
                tenant_require_signed_claim: true,
                auth: Some(Arc::new(ClaimStubGuard)),
                ..Default::default()
            },
        )
        .await;
        (addr, t)
    }

    /// 探测矩阵：验签 claim 是唯一租户来源；裸头仅作一致性复核（fail-closed）。
    #[tokio::test]
    async fn probe_tenant_signed_claim_binding() {
        let (addr, _root) = claim_guard_fixture().await;
        let get = |hdr: Option<&str>, tok: Option<&str>| {
            format!(
                "GET /v1/api/u/ HTTP/1.1\r\nHost: t\r\n{}{}Connection: close\r\n\r\n",
                hdr.map(|h| format!("X-TENANT-ID: {h}\r\n"))
                    .unwrap_or_default(),
                tok.map(|t| format!("Authorization: {t}\r\n"))
                    .unwrap_or_default(),
            )
        };
        // ① claim=t1 + 头 t1 → 200，tenantId=t1（头与 claim 一致）
        let r = raw_http(addr, &get(Some("t1"), Some("Bearer good"))).await;
        let v: Value =
            serde_json::from_slice(r.split("\r\n\r\n").nth(1).unwrap_or("null").as_bytes())
                .unwrap();
        assert!(
            r.starts_with("HTTP/1.1 200") && v["data"]["t"] == "t1",
            "1: {r}"
        );
        // ② claim=t1 + 头 t2（不符）→ 403
        let r = raw_http(addr, &get(Some("t2"), Some("Bearer good"))).await;
        assert!(
            r.starts_with("HTTP/1.1 403") && r.contains("does not match signed claim"),
            "2: {r}"
        );
        // ③ 无头 + claim=t1 → 200（claim 即来源，头可选）
        let r = raw_http(addr, &get(None, Some("Bearer good"))).await;
        let v: Value =
            serde_json::from_slice(r.split("\r\n\r\n").nth(1).unwrap_or("null").as_bytes())
                .unwrap();
        assert!(
            r.starts_with("HTTP/1.1 200") && v["data"]["t"] == "t1",
            "3: {r}"
        );
        // ④ 验签 token 无 tenant claim + 裸头 → 403（头不再自证，fail-closed）
        let r = raw_http(addr, &get(Some("t1"), Some("Bearer plain"))).await;
        assert!(
            r.starts_with("HTTP/1.1 403") && r.contains("requires a signed claim"),
            "4: {r}"
        );
        // ⑤ 无 token 无头（受保护路径）→ 401（鉴权先于租户）
        let r = raw_http(addr, &get(None, None)).await;
        assert!(r.starts_with("HTTP/1.1 401"), "5: {r}");
        // ⑥ anonymous 豁免路径保持（无头无 token → 放行且匿名）
        let r = raw_http(
            addr,
            &get(None, None).replacen("/v1/api/u/", "/v1/api/health", 1),
        )
        .await;
        assert!(r.starts_with("HTTP/1.1 200"), "6: {r}");
    }

    /// 探测：开关关闭 → 行为与旧版逐字节一致（裸头自证照常，缺头 400）。
    #[tokio::test]
    async fn probe_tenant_binding_off_keeps_legacy_behavior() {
        let t = routes(&[(
            "u/api.ts",
            "export default { get() { json.ok({ t: http.tenantId }); } };",
        )]);
        let addr = spawn_pipeline(
            "/v1/api",
            t.0.clone(),
            true,
            None,
            Pipeline {
                tenant_header: Some("X-TENANT-ID".into()),
                ..Default::default()
            },
        )
        .await;
        let r = raw_http(
            addr,
            "GET /v1/api/u/ HTTP/1.1\r\nHost: t\r\nX-TENANT-ID: acme\r\nConnection: close\r\n\r\n",
        )
        .await;
        let v: Value =
            serde_json::from_slice(r.split("\r\n\r\n").nth(1).unwrap_or("null").as_bytes())
                .unwrap();
        assert!(
            r.starts_with("HTTP/1.1 200") && v["data"]["t"] == "acme",
            "off: {r}"
        );
        let r = raw_http(
            addr,
            "GET /v1/api/u/ HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(r.starts_with("HTTP/1.1 400"), "off 缺头仍 400: {r}");
    }

    #[tokio::test]
    async fn tenant_header_injected_or_400() {
        let t = routes(&[(
            "u/f/api.ts",
            "export default { get() { json.ok({ t: http.tenantId === undefined ? null : http.tenantId }); } };",
        )]);
        let addr = spawn_pipeline(
            "/v1/api",
            t.0.clone(),
            true,
            None,
            Pipeline {
                tenant_header: Some("X-TENANT-ID".into()),
                ..Default::default()
            },
        )
        .await;
        let ok = raw_http(
            addr,
            "GET /v1/api/u/f/ HTTP/1.1\r\nHost: t\r\nX-TENANT-ID: acme\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(
            ok.starts_with("HTTP/1.1 200") && ok.contains("\"t\":\"acme\""),
            "{ok}"
        );
        let miss = raw_http(
            addr,
            "GET /v1/api/u/f/ HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(
            miss.starts_with("HTTP/1.1 400") && miss.contains("X-TENANT-ID"),
            "{miss}"
        );
        // 未启用 → 无注入也无 400
        let addr2 = spawn_server("/v1/api", t.0.clone(), true, None).await;
        let plain = raw_http(
            addr2,
            "GET /v1/api/u/f/ HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(
            plain.starts_with("HTTP/1.1 200") && plain.contains("\"t\":null"),
            "{plain}"
        );
    }

    /// tenant.anonymous_paths：命中豁免路径的跳转腿免租户头；未命中仍 400。
    #[tokio::test]
    async fn tenant_anonymous_paths_skip_header_requirement() {
        let t = routes(&[(
            "oidc/callback/api.ts",
            "export default { get() { json.ok({ t: http.tenantId === undefined ? null : http.tenantId }); } };",
        )]);
        let addr = spawn_pipeline(
            "/v1/api",
            t.0.clone(),
            true,
            None,
            Pipeline {
                tenant_header: Some("X-TENANT-ID".into()),
                tenant_anon: vec!["/oidc/*".into()],
                ..Default::default()
            },
        )
        .await;
        // 命中 "/oidc/*"（一层）：/oidc/callback 免头，tenantId 为 null。
        let exempt = raw_http(
            addr,
            "GET /v1/api/oidc/callback/ HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(
            exempt.starts_with("HTTP/1.1 200") && exempt.contains("\"t\":null"),
            "{exempt}"
        );
        // 未命中路径仍强制租户头。
        let miss = raw_http(
            addr,
            "GET /v1/api/oidc/callback/?x=1 HTTP/1.1\r\nHost: t\r\nX-TENANT-ID: acme\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(
            miss.starts_with("HTTP/1.1 200") && miss.contains("\"t\":\"acme\""),
            "{miss}"
        );
    }

    /// path_matches：字面 / 尾 `/*` 一层 / 中段 `*` / `**` 跨段（下游 U2 实测五形态矩阵）。
    #[test]
    fn path_matches_semantics() {
        let l = vec!["/oidc/*".to_string(), "/idp/.well-known/*".to_string()];
        // 旧语义不变：尾 `/*` 严格一层（不命中裸前缀、不命中两层）。
        assert!(crate::path_matches(&l, "/oidc/callback"));
        assert!(!crate::path_matches(&l, "/oidc"));
        assert!(!crate::path_matches(&l, "/oidc/a/b"));
        assert!(crate::path_matches(
            &l,
            "/idp/.well-known/openid-configuration"
        ));
        assert!(crate::path_matches(&["/health".to_string()], "/health"));
        // 尾斜杠与空段容忍。
        assert!(crate::path_matches(&l, "/oidc/callback/"));
        // 中段 `*`：匹配任意单段（U2 第一层）。
        let mid = vec!["/public/anchor/*/states".to_string()];
        assert!(crate::path_matches(&mid, "/public/anchor/v1c/states"));
        assert!(!crate::path_matches(&mid, "/public/anchor/v1c/x/states"));
        assert!(!crate::path_matches(&mid, "/public/anchor/v1c/states/x"));
        // `**` 跨段：≥0 段。
        let deep = vec!["/public/**".to_string()];
        assert!(crate::path_matches(&deep, "/public"));
        assert!(crate::path_matches(&deep, "/public/a"));
        assert!(crate::path_matches(&deep, "/public/a/b/c"));
        assert!(!crate::path_matches(&deep, "/publik/a"));
        // 单模式多 `*`。
        let multi = vec!["/a/*/b/*".to_string()];
        assert!(crate::path_matches(&multi, "/a/1/b/2"));
        assert!(!crate::path_matches(&multi, "/a/1/b/2/3"));
        // `**` 不得放行穿越形态（段内含 `..` 只是字面段，但不得因 `**` 命中根路径）。
        assert!(!crate::path_matches(&deep, "/"));
        assert!(!crate::path_matches(&deep, ""));
    }

    /// oj-7：Range 单区间解析矩阵（多区间/非法 → 200 全量；越界/空 → 416）。
    #[test]
    fn parse_range_semantics() {
        use super::{RangeSpec, parse_range};
        let hv = |s: &str| axum::http::HeaderValue::from_str(s).unwrap();
        let len = 100usize;
        // 无头 / 非 bytes 单位 / 多区间 / 段序非法 → 全量。
        assert_eq!(parse_range(None, len), RangeSpec::Full);
        assert_eq!(parse_range(Some(&hv("items=0-9")), len), RangeSpec::Full);
        assert_eq!(
            parse_range(Some(&hv("bytes=0-9,20-29")), len),
            RangeSpec::Full
        );
        assert_eq!(parse_range(Some(&hv("bytes=9-0")), len), RangeSpec::Full);
        assert_eq!(parse_range(Some(&hv("bytes=x-y")), len), RangeSpec::Full);
        // 闭区间 + 末端钳制。
        assert_eq!(
            parse_range(Some(&hv("bytes=0-9")), len),
            RangeSpec::Partial(0, 9)
        );
        assert_eq!(
            parse_range(Some(&hv("bytes=90-200")), len),
            RangeSpec::Partial(90, 99)
        );
        // 开区间到末尾。
        assert_eq!(
            parse_range(Some(&hv("bytes=50-")), len),
            RangeSpec::Partial(50, 99)
        );
        // 末尾 N 字节。
        assert_eq!(
            parse_range(Some(&hv("bytes=-10")), len),
            RangeSpec::Partial(90, 99)
        );
        assert_eq!(
            parse_range(Some(&hv("bytes=-0")), len),
            RangeSpec::Unsatisfiable
        );
        // 越界 / 空文件 → 416。
        assert_eq!(
            parse_range(Some(&hv("bytes=100-")), len),
            RangeSpec::Unsatisfiable
        );
        assert_eq!(
            parse_range(Some(&hv("bytes=0-9")), 0),
            RangeSpec::Unsatisfiable
        );
    }

    /// oj-8：apply_custom_headers 只补缺、不覆盖框架自有头、跳过非法头名。
    #[test]
    fn apply_custom_headers_fills_only_missing() {
        use super::apply_custom_headers;
        let mut r = Response::new(axum::body::Body::empty());
        r.headers_mut().insert(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("application/json"),
        );
        let extra = vec![
            ("x-frame-options".to_string(), "DENY".to_string()),
            ("content-type".to_string(), "text/html".to_string()), // 框架已有 → 不覆盖
            ("bad header name".to_string(), "x".to_string()),      // 非法名 → 跳过
        ];
        apply_custom_headers(&mut r, &extra);
        assert_eq!(
            r.headers()["x-frame-options"],
            axum::http::HeaderValue::from_static("DENY")
        );
        assert_eq!(
            r.headers()[axum::http::header::CONTENT_TYPE],
            axum::http::HeaderValue::from_static("application/json")
        );
        assert!(!r.headers().contains_key("bad header name"));
    }

    /// multipart：文本字段并入 body、文件进 http.files + http.file(i) 取字节；
    /// 非 multipart JSON 语义不变；超 max_upload 413 信封。
    #[tokio::test]
    async fn multipart_upload_and_413() {
        let t = routes(&[(
            "u/api.ts",
            "export default { async post() {\n\
               const f = http.files[0];\n\
               const b = f ? (await http.file(0)) : null;\n\
               json.ok({ name: f ? f.filename : null, n: f ? b.length : 0, note: http.body.note });\n\
             } };",
        )]);
        // 默认上限（10MiB）：multipart + JSON 双语义。
        let addr = spawn_server("/v1/api", t.0.clone(), true, None).await;
        let mp = |bytes: &[u8], note: &str| {
            let body = format!(
                "--X-BND\r\nContent-Disposition: form-data; name=\"note\"\r\n\r\n{note}\r\n\
                 --X-BND\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.png\"\r\nContent-Type: image/png\r\n\r\n{}\r\n\
                 --X-BND--\r\n",
                String::from_utf8_lossy(bytes)
            );
            format!(
                "POST /v1/api/u/ HTTP/1.1\r\nHost: t\r\nContent-Type: multipart/form-data; boundary=X-BND\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
        };
        let r = raw_http(addr, &mp(&[1, 2, 3], "hi")).await;
        let v: Value =
            serde_json::from_slice(r.split("\r\n\r\n").nth(1).unwrap_or("null").as_bytes())
                .unwrap();
        assert!(r.starts_with("HTTP/1.1 200"), "1: {r}");
        assert_eq!(v["data"]["name"], "a.png", "{v}");
        assert_eq!(v["data"]["n"], 3, "{v}");
        assert_eq!(v["data"]["note"], "hi", "{v}");
        // 非 multipart JSON → body 原语义不变（files 空 → name null）
        let j = format!(
            "POST /v1/api/u/ HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{{\"note\":\"hi\"}}",
            13
        );
        let r = raw_http(addr, &j).await;
        let v: Value =
            serde_json::from_slice(r.split("\r\n\r\n").nth(1).unwrap_or("null").as_bytes())
                .unwrap();
        assert!(r.starts_with("HTTP/1.1 200"), "2: {r}");
        assert_eq!(v["data"]["name"], serde_json::Value::Null, "{v}");
        assert_eq!(v["data"]["note"], "hi", "{v}");
        // 超 max_upload（8B，默认 body limit=16B，12B JSON 能进 handle）→ 413 信封。
        let addr2 = spawn_pipeline(
            "/v1/api",
            t.0.clone(),
            true,
            None,
            Pipeline {
                max_upload: 8,
                ..Default::default()
            },
        )
        .await;
        let r = raw_http(
            addr2,
            "POST /v1/api/u/ HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\nContent-Length: 12\r\nConnection: close\r\n\r\n{\"a\":123456}",
        )
        .await;
        assert!(
            r.starts_with("HTTP/1.1 413") && r.contains("upload too large"),
            "3: {r}"
        );
    }

    /// blob 下载路由（local）：api 上传 → {base}/blob/k 200 bytes 一致 → del 404 →
    /// 非 GET 404 → %2e%2e 穿越 404。
    #[tokio::test]
    async fn blob_download_route_local() {
        let t = routes(&[(
            "u/api.ts",
            "export default { async post() {\n\
               const f = http.files[0];\n\
               const b = await http.file(0);\n\
               await blob.put(f.filename, b, f.content_type);\n\
               json.ok({ url: await blob.url(f.filename), n: b.length });\n\
             },\n\
             async del() { await blob.del(http.param(\"k\", \"\")); json.ok({ ok: 1 }); } };",
        )]);
        let root = std::env::temp_dir().join(format!("oj-blob-srv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let blob: Arc<dyn BlobBackend> = Arc::new(LocalBlob::new(&root, "/v1/api").unwrap());
        let addr = spawn_blob("/v1/api", t.0.clone(), blob).await;
        // 上传 a.png（PNGDATA 7B）
        let mp = "--X-BND\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.png\"\r\nContent-Type: image/png\r\n\r\nPNGDATA\r\n--X-BND--\r\n"
            .to_string();
        let req = format!(
            "POST /v1/api/u/ HTTP/1.1\r\nHost: t\r\nContent-Type: multipart/form-data; boundary=X-BND\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{mp}",
            mp.len()
        );
        let r = raw_http(addr, &req).await;
        assert!(
            r.starts_with("HTTP/1.1 200") && r.contains("\"n\":7"),
            "upload: {r}"
        );
        // 下载：bytes 一致 + Content-Type 按扩展名推断
        let r = raw_http(
            addr,
            "GET /v1/api/blob/a.png HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(
            r.starts_with("HTTP/1.1 200") && r.contains("image/png") && r.ends_with("PNGDATA"),
            "get: {r}"
        );
        // 非 GET 不走 blob 路由
        let r = raw_http(
            addr,
            "POST /v1/api/blob/a.png HTTP/1.1\r\nHost: t\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(r.starts_with("HTTP/1.1 404"), "post: {r}");
        // del（query k=…）→ 之后 GET 404
        let r = raw_http(
            addr,
            "DELETE /v1/api/u/?k=a.png HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(r.starts_with("HTTP/1.1 200"), "del: {r}");
        let r = raw_http(
            addr,
            "GET /v1/api/blob/a.png HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(r.starts_with("HTTP/1.1 404"), "after del: {r}");
        // %2e%2e 穿越 → decode 后 valid_key 拒绝 → 404
        let r = raw_http(
            addr,
            "GET /v1/api/blob/%2e%2e/x HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(r.starts_with("HTTP/1.1 404"), "traversal: {r}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Stub 守卫：/health 匿名；Bearer good → user；其余 Err。
    struct StubGuard;
    impl only_js::bridge::AuthGuard for StubGuard {
        fn verify(
            &self,
            path: &str,
            _method: &str,
            auth: Option<&str>,
            _headers: Option<&str>,
        ) -> Result<Option<Value>, String> {
            if path == "/health" {
                return Ok(None);
            }
            match auth {
                Some("Bearer good") => Ok(Some(serde_json::json!({
                    "id": "1", "roles": ["admin"],
                    "claims": {"sub": "1", "roles": ["admin"], "iat": 0, "exp": 0},
                }))),
                _ => Err("missing or invalid bearer token".into()),
            }
        }
    }

    /// 守卫管线：401 / 匿名放行 / http.user 注入；内置 /auth/* 路由已删除（404）。
    #[tokio::test]
    async fn auth_guard_pipeline() {
        let t = routes(&[
            (
                "me/api.ts",
                "export default { get() { json.ok({ u: http.user }); } };",
            ),
            (
                "health/api.ts",
                "export default { get() { json.ok({ ok: 1 }); } };",
            ),
        ]);
        let addr = spawn_pipeline(
            "/v1/api",
            t.0.clone(),
            true,
            None,
            Pipeline {
                auth: Some(Arc::new(StubGuard)),
                ..Default::default()
            },
        )
        .await;
        let get = |p: &str, token: Option<&str>| {
            format!(
                "GET {p} HTTP/1.1\r\nHost: t\r\n{}Connection: close\r\n\r\n",
                token
                    .map(|t| format!("Authorization: {t}\r\n"))
                    .unwrap_or_default()
            )
        };
        // 无 token → 401；坏 token → 401
        let r = raw_http(addr, &get("/v1/api/me/", None)).await;
        assert!(r.starts_with("HTTP/1.1 401"), "{r}");
        let r = raw_http(addr, &get("/v1/api/me/", Some("Bearer bad"))).await;
        assert!(r.starts_with("HTTP/1.1 401"), "{r}");
        // 匿名路径放行
        let r = raw_http(addr, &get("/v1/api/health/", None)).await;
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        // 注入 http.user
        let r = raw_http(addr, &get("/v1/api/me/", Some("Bearer good"))).await;
        assert!(
            r.starts_with("HTTP/1.1 200") && r.contains("\"id\":\"1\""),
            "{r}"
        );
        // 内置 auth 路由已删除：无对应业务模块 → 404（不再是 200/405）
        let r = raw_http(
            addr,
            "POST /v1/api/auth/login HTTP/1.1\r\nHost: t\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
        )
        .await;
        assert!(r.starts_with("HTTP/1.1 404"), "{r}");
    }

    #[tokio::test]
    async fn serves_mirror_route_with_envelope() {
        let t = routes(&[(
            "user/account/api.ts",
            r#"export default { get() { json.ok({ m: http.method, q: http.param("id", 0) }); } };"#,
        )]);
        let addr = spawn_server("/v1/api", t.0.clone(), true, None).await;
        let resp = raw_http(
            addr,
            "GET /v1/api/user/account/?id=7 HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        assert!(resp.contains("\"q\":\"7\""), "{resp}");
        assert!(resp.contains("\"m\":\"GET\""), "{resp}");
    }

    #[tokio::test]
    async fn missing_api_is_404_and_unmapped_verb_405() {
        let t = routes(&[]);
        let addr = spawn_server("/v1/api", t.0.clone(), true, None).await;
        let resp = raw_http(
            addr,
            "GET /v1/api/none/here/ HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(resp.starts_with("HTTP/1.1 404"), "{resp}");
        // api 文件在但未导出 del → 405（driver 侧 json.fail(405) 信封）。
        let t2 = routes(&[("u/f/api.ts", "export default { get() { json.ok({}); } };")]);
        let addr2 = spawn_server("/v1/api", t2.0.clone(), true, None).await;
        let resp2 = raw_http(
            addr2,
            "DELETE /v1/api/u/f/ HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(resp2.starts_with("HTTP/1.1 405"), "{resp2}");
    }

    #[tokio::test]
    async fn handler_timeout_returns_408_envelope() {
        let t = routes(&[(
            "u/f/api.ts",
            "export default { get() { while (true) {} } };",
        )]);
        let addr = spawn_server(
            "/v1/api",
            t.0.clone(),
            true,
            Some(std::time::Duration::from_millis(200)),
        )
        .await;
        let resp = raw_http(
            addr,
            "GET /v1/api/u/f/ HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(resp.starts_with("HTTP/1.1 408"), "{resp}");
    }

    #[tokio::test]
    async fn handler_error_returns_500_envelope() {
        let t = routes(&[("u/f/api.ts", "function {{{{\nexport default {};")]);
        let addr = spawn_server("/v1/api", t.0.clone(), true, None).await;
        let resp = raw_http(
            addr,
            "GET /v1/api/u/f/ HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(resp.starts_with("HTTP/1.1 500"), "{resp}");
    }

    #[tokio::test]
    async fn health_endpoint_reports_cert_status() {
        use crate::CertificateStatus;
        // 默认未配置证书 → Valid
        let resp = crate::health_handler(axum::extract::State(dummy_app_state())).await;
        let body = body_text(resp).await;
        assert!(body.contains("\"certificate_status\":\"valid\""), "{body}");

        // 注入 Grace 状态后，health 应反映 grace 与剩余秒数
        let s = dummy_app_state();
        *s.certificate_status.write().unwrap() = CertificateStatus::Grace {
            remaining_secs: 86_400,
        };
        let resp2 = crate::health_handler(axum::extract::State(s)).await;
        let body2 = body_text(resp2).await;
        assert!(
            body2.contains("\"certificate_status\":\"grace\""),
            "{body2}"
        );
        assert!(body2.contains("\"grace_remaining_secs\":86400"), "{body2}");
    }

    /// GET {base}/plugins：公共查询端点——返回装配插件的自描述清单（ok 信封）。
    #[tokio::test]
    async fn plugins_endpoint_lists_self_descriptions() {
        let mut st = dummy_app_state();
        // 注入自描述（AppState.plugins 为 Arc<Vec<PluginInfo>>）。
        st.plugins = Arc::new(vec![only_js::bridge::PluginInfo {
            name: "auth".into(),
            semver: "0.1.0".into(),
            abi_version: 7,
            fingerprint: "fp".into(),
            description: "auth guard".into(),
            host_abi_version: 0,
        }]);
        // handler 以 route 形态挂在 base 下，直接调（base 前缀拼接的路径校验在集成层）。
        let resp = crate::plugins_handler(axum::extract::State(st)).await;
        let body = body_text(resp).await;
        assert!(body.contains("\"code\":0"), "{body}");
        assert!(body.contains("\"name\":\"auth\""), "{body}");
        assert!(body.contains("\"description\":\"auth guard\""), "{body}");
        assert!(body.contains("\"abi_version\":7"), "{body}");
    }

    async fn body_text(resp: axum::response::Response) -> String {
        let b = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8_lossy(&b).into_owned()
    }

    /// 构造指定证书状态的 AppState，用于 GET 限制测试。
    fn state_with_cert(status: CertificateStatus) -> AppState {
        let s = dummy_app_state();
        *s.certificate_status.write().unwrap() = status;
        s
    }

    /// handle() 直接调用的请求构造（v0.1.30 签名改为 Request 后测试统一走这里）。
    fn test_req(method: &str, uri: &str) -> axum::extract::Request {
        axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .body(axum::body::Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn get_blocked_when_cert_expired() {
        let st = state_with_cert(CertificateStatus::Expired);
        let resp = crate::handle(axum::extract::State(st), test_req("GET", "/v1/api/foo")).await;
        assert_eq!(
            resp.status(),
            axum::http::StatusCode::FORBIDDEN,
            "GET must be 403 when cert expired"
        );
        assert!(
            body_text(resp).await.contains("certificate expired"),
            "error body should explain cert expiry"
        );
    }

    #[tokio::test]
    async fn get_blocked_when_cert_grace() {
        let st = state_with_cert(CertificateStatus::Grace {
            remaining_secs: 86_400,
        });
        let resp = crate::handle(axum::extract::State(st), test_req("GET", "/v1/api/foo")).await;
        assert_eq!(
            resp.status(),
            axum::http::StatusCode::FORBIDDEN,
            "GET must be 403 in grace period"
        );
        assert!(
            body_text(resp).await.contains("grace period"),
            "error body should mention grace period"
        );
    }

    #[tokio::test]
    async fn non_get_allowed_when_cert_expired() {
        // 仅 GET 受限；POST 等即便证书过期也应继续走到路由层（此处无路由 → 404，但非 403）。
        let st = state_with_cert(CertificateStatus::Expired);
        let resp = crate::handle(axum::extract::State(st), test_req("POST", "/v1/api/foo")).await;
        assert_ne!(
            resp.status(),
            axum::http::StatusCode::FORBIDDEN,
            "non-GET must not be blocked by cert"
        );
    }

    #[tokio::test]
    async fn get_allowed_when_cert_valid() {
        let st = state_with_cert(CertificateStatus::Valid);
        let resp = crate::handle(axum::extract::State(st), test_req("GET", "/v1/api/foo")).await;
        assert_ne!(
            resp.status(),
            axum::http::StatusCode::FORBIDDEN,
            "valid cert must not block GET"
        );
    }

    // ----- 路径参数路由 e2e -----

    fn get(_addr: std::net::SocketAddr, path: &str) -> String {
        format!("GET {path} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n")
    }

    #[tokio::test]
    async fn serves_path_param_route() {
        let t = routes(&[(
            "user/account/api.ts",
            "function detail() { json.ok({ id: Number(http.param(\"id\", 0)) }); }\n\
             detail.route = \"{id}\";\n\
             export default { get: detail };",
        )]);
        let addr = spawn_server("/v1/api", t.0.clone(), true, None).await;
        let resp = raw_http(addr, &get(addr, "/v1/api/user/account/42")).await;
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        assert!(resp.contains("\"id\":42"), "{resp}");
        // 尾斜杠等价
        let resp2 = raw_http(addr, &get(addr, "/v1/api/user/account/42/")).await;
        assert!(resp2.starts_with("HTTP/1.1 200"), "{resp2}");
        // 挂 .route 后目录镜像 404（替换语义）
        let resp3 = raw_http(addr, &get(addr, "/v1/api/user/account")).await;
        assert!(resp3.starts_with("HTTP/1.1 404"), "{resp3}");
    }

    #[tokio::test]
    async fn path_param_overrides_query_and_decodes() {
        let t = routes(&[(
            "u/api.ts",
            "function get() { json.ok({ id: http.param(\"id\", 0) }); }\n\
             get.route = \"{id}\";\n\
             export default { get };",
        )]);
        let addr = spawn_server("/v1/api", t.0.clone(), true, None).await;
        let resp = raw_http(addr, &get(addr, "/v1/api/u/42%41?id=99")).await;
        assert!(resp.contains("\"id\":\"42A\""), "{resp}"); // 解码 + 路径优先
    }

    #[tokio::test]
    async fn catch_all_and_guards() {
        let t = routes(&[(
            "file/api.ts",
            "function get() { json.ok({ p: http.param(\"path\", \"\") }); }\n\
             get.route = \"{*path}\";\n\
             export default { get };",
        )]);
        let addr = spawn_server("/v1/api", t.0.clone(), true, None).await;
        let ok = raw_http(addr, &get(addr, "/v1/api/file/a/b/c")).await;
        assert!(
            ok.starts_with("HTTP/1.1 200") && ok.contains("a/b/c"),
            "{ok}"
        );
        for path in [
            "/v1/api/file",
            "/v1/api/file/",
            "/v1/api//file/a",
            "/v1/api/file/%2e%2e",
        ] {
            let r = raw_http(addr, &get(addr, path)).await;
            assert!(r.starts_with("HTTP/1.1 404"), "{path}: {r}");
        }
    }

    #[tokio::test]
    async fn verb_missing_is_405_and_trace_405() {
        let t = routes(&[(
            "u/f/api.ts",
            "function get() { json.ok({}); }\nexport default { get };",
        )]);
        let addr = spawn_server("/v1/api", t.0.clone(), true, None).await;
        for verb in ["DELETE", "TRACE"] {
            let r = raw_http(
                addr,
                &format!("{verb} /v1/api/u/f HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n"),
            )
            .await;
            assert!(r.starts_with("HTTP/1.1 405"), "{verb}: {r}");
        }
    }

    #[tokio::test]
    async fn conflict_route_returns_500() {
        let t = routes(&[
            (
                "a/api.ts",
                "function get() { json.ok({ a: 1 }); }\nget.route = \"/user/{id}\";\nexport default { get };",
            ),
            (
                "b/api.ts",
                "function get() { json.ok({ b: 1 }); }\nget.route = \"/user/{id}\";\nexport default { get };",
            ),
        ]);
        let addr = spawn_server("/v1/api", t.0.clone(), true, None).await;
        let r = raw_http(addr, &get(addr, "/v1/api/user/9")).await;
        assert!(
            r.starts_with("HTTP/1.1 500") && r.contains("route conflict"),
            "{r}"
        );
    }

    #[tokio::test]
    async fn query_decodes_form_urlencoded() {
        let t = routes(&[(
            "q/api.ts",
            "export default { get() { json.ok({ q: http.param(\"q\", \"\") }); } };",
        )]);
        let addr = spawn_server("/v1/api", t.0.clone(), true, None).await;
        let r = raw_http(addr, &get(addr, "/v1/api/q?q=a+b%21")).await;
        assert!(r.contains("\"q\":\"a b!\""), "{r}");
    }

    #[tokio::test]
    async fn dev_fallback_serves_new_file_without_rebuild() {
        let t = routes(&[]); // 建表时无文件
        let addr = spawn_server("/v1/api", t.0.clone(), true, None).await;
        let p = t.0.join("late/api.ts");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "export default { get() { json.ok({ late: true }); } };").unwrap();
        let r = raw_http(addr, &get(addr, "/v1/api/late")).await;
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
    }

    #[tokio::test]
    async fn dev_fallback_does_not_resurrect_replaced_route() {
        // 建表时文件在、get 挂了 .route → 目录镜像被替换，兜底不得复活
        let t = routes(&[(
            "r/api.ts",
            "function get() { json.ok({}); }\nget.route = \"{id}\";\nexport default { get };",
        )]);
        let addr = spawn_server("/v1/api", t.0.clone(), true, None).await;
        let r = raw_http(addr, &get(addr, "/v1/api/r")).await;
        assert!(r.starts_with("HTTP/1.1 404"), "{r}");
    }

    // ----- 静态站点（server.app_path）-----

    /// 返回 (addr, 夹具)：夹具须在测试内持有（TempRoutes Drop 会删目录）。
    async fn spawn_static(
        api: &[(&str, &str)],
        site: &[(&str, &str)],
        opts: StaticOpts,
    ) -> (std::net::SocketAddr, (TempRoutes, TempRoutes)) {
        let (addr, (t, keeps)) = spawn_static_sites(api, &[("/", site.to_vec())], opts).await;
        (addr, (t, keeps.into_iter().next().unwrap()))
    }

    /// 多站点版（v0.1.27）：sites = [(prefix, files)]，按给定顺序装配（app() 内重排）。
    async fn spawn_static_sites(
        api: &[(&str, &str)],
        sites: &[(&str, Vec<(&str, &str)>)],
        opts: StaticOpts,
    ) -> (std::net::SocketAddr, (TempRoutes, Vec<TempRoutes>)) {
        let t = routes(api);
        let keeps: Vec<TempRoutes> = sites.iter().map(|(_, f)| routes(f)).collect();
        let static_sites: Vec<StaticSite> = sites
            .iter()
            .zip(&keeps)
            .map(|((prefix, _), k)| StaticSite {
                prefix: prefix.to_string(),
                root: k.0.clone(),
                headers: Vec::new(),
            })
            .collect();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (dir, table) = (t.0.clone(), build_table(&t.0, true, "/v1/api"));
        tokio::spawn(async move {
            serve_with_listener(
                listener,
                "/v1/api",
                dir.clone(),
                true,
                table,
                make_actor(dir, true),
                None,
                static_sites,
                opts,
                Pipeline::default(),
                None,
            )
            .await
            .unwrap();
        });
        (addr, (t, keeps))
    }

    #[tokio::test]
    async fn serves_static_index_files_and_content_types() {
        let (addr, _keep) = spawn_static(
            &[(
                "u/f/api.ts",
                "export default { get() { json.ok({ api: true }); } };",
            )],
            &[
                ("index.html", "<h1>hi</h1>"),
                ("css/app.css", "body{}"),
                ("v1/api/u", "STATIC"),
            ],
            StaticOpts::default(),
        )
        .await;
        // / → index.html + text/html
        let r = raw_http(
            addr,
            "GET / HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(
            r.starts_with("HTTP/1.1 200") && r.contains("text/html") && r.contains("<h1>hi</h1>"),
            "{r}"
        );
        // 普通文件 + Content-Type；目录 → index.html
        let r = raw_http(addr, &get(addr, "/css/app.css")).await;
        assert!(
            r.starts_with("HTTP/1.1 200") && r.contains("text/css"),
            "{r}"
        );
        // API 优先于静态：同名路径走路由表
        let r = raw_http(addr, &get(addr, "/v1/api/u/f/")).await;
        assert!(
            r.starts_with("HTTP/1.1 200") && r.contains("\"api\":true") && !r.contains("STATIC"),
            "{r}"
        );
    }

    // ----- 静态站点增强（v0.1.20）：SPA 深链接回落 + per-route meta 注入 -----

    /// SPA 回落：深链接（无扩展名）→ index.html；带扩展名的资源与 API 前缀不回落。
    #[tokio::test]
    async fn spa_fallback_serves_index_for_deep_link_only() {
        let (addr, _keep) = spawn_static(
            &[],
            &[("index.html", "<html><head></head><body>app</body></html>")],
            StaticOpts {
                spa_fallback: true,
                ..Default::default()
            },
        )
        .await;
        // 深链接 → index.html（curl 默认 Accept: */* 也算 html）
        let r = raw_http(addr, &get(addr, "/space/issues/abc")).await;
        assert!(
            r.starts_with("HTTP/1.1 200") && r.contains("body>app") && r.contains("text/html"),
            "{r}"
        );
        // 带扩展名 = 资源请求，不回落（404 而非被吞成 200）
        let r = raw_http(addr, &get(addr, "/assets/app.js")).await;
        assert!(r.starts_with("HTTP/1.1 404"), "{r}");
        // API 前缀下的未命中路径不回落（否则拼错的 API 路径会静默变 200）
        let r = raw_http(addr, &get(addr, "/v1/api/nope/deep")).await;
        assert!(r.starts_with("HTTP/1.1 404"), "{r}");
    }

    /// 回落关时（默认）深链接仍是 404 —— 静默把 404 变 200 会掩盖错配。
    #[tokio::test]
    async fn spa_fallback_off_keeps_404() {
        let (addr, _keep) =
            spawn_static(&[], &[("index.html", "app")], StaticOpts::default()).await;
        let r = raw_http(addr, &get(addr, "/space/issues/abc")).await;
        assert!(r.starts_with("HTTP/1.1 404"), "{r}");
    }

    /// meta 注入：按路径查 `__meta/<path>.json` 注入 title/og；值一律转义；
    /// 该目录本身不对静态服务公开。
    #[tokio::test]
    async fn html_meta_injects_escaped_tags_and_hides_meta_dir() {
        let (addr, _keep) = spawn_static(
            &[],
            &[
                ("index.html", "<html><head></head><body>hi</body></html>"),
                (
                    "__meta/space.json",
                    r#"{"title":"A & B","description":"d","og:title":"OG","twitter:card":"summary","canonical":"https://x/space"}"#,
                ),
                ("__meta/index.json", r#"{"title":"Home"}"#),
                (
                    "__meta/dirty.json",
                    r#"{"title":"</title><script>alert(1)</script>"}"#,
                ),
            ],
            StaticOpts {
                spa_fallback: true,
                html_meta: Some("__meta".into()),
                ..Default::default()
            },
        )
        .await;
        // 深链接回落 + 注入
        let r = raw_http(addr, &get(addr, "/space")).await;
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        assert!(r.contains("<title>A &amp; B</title>"), "{r}");
        assert!(
            r.contains("<meta property=\"og:title\" content=\"OG\">"),
            "{r}"
        );
        assert!(
            r.contains("<meta name=\"twitter:card\" content=\"summary\">"),
            "{r}"
        );
        assert!(
            r.contains("<link rel=\"canonical\" href=\"https://x/space\">"),
            "{r}"
        );
        // 首页（/ → index.html）同样注入（否则首页无 title，与需求矛盾）
        let r = raw_http(addr, &get(addr, "/")).await;
        assert!(r.contains("<title>Home</title>"), "{r}");
        // 值里的标签被转义，不得注入可执行脚本
        let r = raw_http(addr, &get(addr, "/dirty")).await;
        assert!(r.contains("&lt;/title&gt;"), "{r}");
        assert!(!r.contains("<script>alert(1)</script>"), "{r}");
        // meta 目录是数据不是站点资产 → 不可被直接拉取
        let r = raw_http(addr, &get(addr, "/__meta/space.json")).await;
        assert!(r.starts_with("HTTP/1.1 404"), "{r}");
        // 未命中 meta → 原样返回
        let r = raw_http(addr, &get(addr, "/none")).await;
        assert!(
            r.starts_with("HTTP/1.1 200") && !r.contains("<title>"),
            "{r}"
        );
        // v0.1.25 向后兼容：没配 html_cache_control、也没动态 handler → 不加任何缓存头
        assert!(!r.to_lowercase().contains("cache-control"), "{r}");
    }

    // ----- 动态 meta（v0.1.25）：handler 按数据注入 + per-route Cache-Control -----

    /// 静态 JSON 打底、动态按 key 覆盖（同名覆盖、异名保留）；`cache_control` 进响应头
    /// 且优先于 `server.html_cache_control`；handler 失败（非 2xx）→ fail-open（原样送出）。
    #[tokio::test]
    async fn html_meta_handler_overrides_static_and_sets_cache_control() {
        let (addr, _keep) = spawn_static(
            &[(
                "html-meta/api.ts",
                r#"export default {
                     get() {
                       const p = http.query.path;
                       if (p === "/space") {
                         json.ok({ title: "Dyn", "og:image": "https://x/i.png" });
                         return;
                       }
                       if (p === "/cached") {
                         json.ok({ title: "C", cache_control: "public, max-age=60" });
                         return;
                       }
                       if (p === "/boom") {
                         json.fail(500, "boom");
                         return;
                       }
                       json.ok({ "og:desc": "fallback" });
                     },
                   };"#,
            )],
            &[
                (
                    "index.html",
                    "<html><head><title>shell</title></head><body>app</body></html>",
                ),
                (
                    "__meta/space.json",
                    r#"{"title":"Static","og:title":"OGStatic","description":"d"}"#,
                ),
            ],
            StaticOpts {
                spa_fallback: true,
                html_meta: Some("__meta".into()),
                html_meta_handler: Some("/v1/api/html-meta".into()),
                html_cache_control: Some("no-cache".into()),
                response_headers: Vec::new(),
            },
        )
        .await;
        // 深链接 → 请求路径原样进 handler（?path=），静态打底 + 动态按 key 覆盖
        let r = raw_http(addr, &get(addr, "/space")).await;
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        assert!(r.contains("<title>Dyn</title>"), "{r}");
        // 壳里写死的 title 必须被**替换**（只认第一个 title，追加等于没注入）
        assert!(!r.contains("<title>shell</title>"), "{r}");
        assert_eq!(r.matches("<title>").count(), 1, "{r}");
        // 动态没给的键保留静态值（合并而非替换）
        assert!(r.contains("og:title\" content=\"OGStatic\""), "{r}");
        assert!(r.contains("name=\"description\" content=\"d\""), "{r}");
        assert!(r.contains("og:image\" content=\"https://x/i.png\""), "{r}");
        // 默认 HTML 缓存头 = server.html_cache_control
        assert!(r.to_lowercase().contains("cache-control: no-cache"), "{r}");
        // handler 的 cache_control 覆盖配置值（per-route）
        let r = raw_http(addr, &get(addr, "/cached")).await;
        assert!(
            r.to_lowercase()
                .contains("cache-control: public, max-age=60"),
            "{r}"
        );
        // 信封 code != 0 → fail-open：页面照常 200 送出静态壳（无 meta），不 500
        let r = raw_http(addr, &get(addr, "/boom")).await;
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        assert!(!r.contains("<title>Dyn</title>"), "{r}");
        // 静态 JSON 未命中的路由：动态单独生效
        let r = raw_http(addr, &get(addr, "/plain")).await;
        assert!(r.contains("og:desc\" content=\"fallback\""), "{r}");
    }

    /// 动态 handler 关（只配静态 JSON）：不加缓存头、不派发（无 handler 也不报错）。
    #[tokio::test]
    async fn html_meta_handler_off_is_byte_identical() {
        let (addr, _keep) = spawn_static(
            &[],
            &[("index.html", "<html><head></head><body>hi</body></html>")],
            StaticOpts {
                spa_fallback: true,
                html_meta: None,
                html_meta_handler: None,
                html_cache_control: None,
                response_headers: Vec::new(),
            },
        )
        .await;
        let r = raw_http(addr, &get(addr, "/space/abc")).await;
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        assert!(
            r.contains("<html><head></head><body>hi</body></html>"),
            "{r}"
        );
        assert!(!r.to_lowercase().contains("cache-control"), "{r}");
        assert!(!r.to_lowercase().contains("title"), "{r}");
    }

    /// 只配 `html_cache_control`（不做任何注入）也必须生效：这是「只想给 SPA 壳挂
    /// no-cache」的典型用法，不能被「注入面全关」的早退一起吞掉。
    #[tokio::test]
    async fn html_cache_control_alone_still_applies() {
        let (addr, _keep) = spawn_static(
            &[],
            &[("index.html", "<html><head></head><body>hi</body></html>")],
            StaticOpts {
                spa_fallback: true,
                html_cache_control: Some("no-cache".into()),
                ..Default::default()
            },
        )
        .await;
        let r = raw_http(addr, &get(addr, "/deep/link")).await;
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        assert!(r.to_lowercase().contains("cache-control: no-cache"), "{r}");
        // 正文仍逐字节是壳（没注入任何东西）
        assert!(
            r.contains("<html><head></head><body>hi</body></html>"),
            "{r}"
        );
        // 非 HTML 资产不受影响（缓存头只管 HTML）
        let (addr2, _keep2) = spawn_static(
            &[],
            &[("index.html", "x"), ("assets/app.js", "console.log(1)")],
            StaticOpts {
                spa_fallback: true,
                html_cache_control: Some("no-cache".into()),
                ..Default::default()
            },
        )
        .await;
        let r = raw_http(addr2, &get(addr2, "/assets/app.js")).await;
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        assert!(!r.to_lowercase().contains("cache-control"), "{r}");
    }

    // ----- 注入器单测（`inject_head`）：比 HTTP 级用例更能钉住边界 -----

    fn meta(pairs: &[(&str, &str)]) -> serde_json::Map<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), Value::String((*v).to_string())))
            .collect()
    }

    /// 壳里写死的 `<title>` 必须被**替换**（不是追加第二个）：浏览器/爬虫只认第一个，
    /// 不摘掉旧的话注入等于没注入——这正是 v0.1.20 只插不摘的坑。
    #[test]
    fn inject_head_replaces_shell_title() {
        let html = "<html><head><title>shell</title></head><body>x</body></html>";
        let out = inject_head(html, &meta(&[("title", "Dyn & Co")]));
        assert_eq!(out.matches("<title>").count(), 1, "{out}");
        assert!(out.contains("<title>Dyn &amp; Co</title>"), "{out}");
        assert!(!out.contains("shell"), "{out}");
        // 其余结构原样
        assert!(out.contains("<body>x</body>"), "{out}");
    }

    /// description / og:* / canonical 的旧标签按「同名」摘掉（含属性顺序颠倒、
    /// `data-name` 之类伪属性不得误伤）。
    #[test]
    fn inject_head_dedupes_meta_and_canonical() {
        let html = concat!(
            "<html><head>",
            "<meta content=\"old\" name=\"description\">",
            "<meta property=\"og:title\" content=\"Old OG\">",
            "<link rel=\"canonical\" href=\"https://old\">",
            "<meta data-name=\"description\" content=\"keep me\">",
            "</head><body>x</body></html>"
        );
        let out = inject_head(
            html,
            &meta(&[
                ("description", "new desc"),
                ("og:title", "New OG"),
                ("canonical", "https://new"),
            ]),
        );
        assert!(
            !out.contains("content=\"old\""),
            "旧 description 未摘：{out}"
        );
        assert!(!out.contains("https://old"), "旧 canonical 未摘：{out}");
        assert!(!out.contains("Old OG"), "旧 og:title 未摘：{out}");
        assert!(
            out.contains("<meta name=\"description\" content=\"new desc\">"),
            "{out}"
        );
        assert!(
            out.contains("<meta property=\"og:title\" content=\"New OG\">"),
            "{out}"
        );
        assert!(
            out.contains("<link rel=\"canonical\" href=\"https://new\">"),
            "{out}"
        );
        assert!(out.contains("content=\"keep me\""), "伪属性被误摘：{out}");
    }

    /// head 里的 `<script>` 内容是不透明文本：里面出现 `<title>` 字符串不得被当标签摘掉。
    #[test]
    fn inject_head_skips_opaque_script_content() {
        let html = concat!(
            "<html><head>",
            "<script>window.__tpl = \"<title>inner</title>\";</script>",
            "<title>shell</title>",
            "</head><body>x</body></html>"
        );
        let out = inject_head(html, &meta(&[("title", "Real")]));
        assert!(
            out.contains("window.__tpl = \"<title>inner</title>\""),
            "{out}"
        );
        assert!(out.contains("<title>Real</title>"), "{out}");
        assert!(!out.contains("<title>shell</title>"), "{out}");
    }

    /// 非 ASCII 出现在 head 里（大小写变换会改字节长度）不得错位 / panic。
    #[test]
    fn inject_head_is_index_safe_on_non_ascii() {
        let html =
            "<html><head><meta charset=\"utf-8\"><!-- İstanbul --></head><body>ş</body></html>";
        let out = inject_head(html, &meta(&[("title", "Türkçe")]));
        assert!(out.contains("<title>Türkçe</title>"), "{out}");
        assert!(out.contains("<!-- İstanbul -->"), "注释被破坏：{out}");
        // 注入点仍在 `</head>` 之前，head 之外原样
        let head_close = out.find("</head>").expect("head 收口还在");
        assert!(
            out.find("<title>Türkçe</title>").unwrap() < head_close,
            "{out}"
        );
        assert!(out.ends_with("<body>ş</body></html>"), "{out}");
    }

    /// 无 `</head>` / 空表 → 原样返回（不猜结构、零副作用）。
    #[test]
    fn inject_head_no_head_or_empty_is_untouched() {
        let no_head = "<html><body>x</body></html>";
        assert_eq!(inject_head(no_head, &meta(&[("title", "T")])), no_head);
        let html = "<html><head><title>s</title></head></html>";
        assert_eq!(inject_head(html, &serde_json::Map::new()), html);
        // 只给非白名单键（渲染不出东西）→ 不得摘任何标签
        assert_eq!(inject_head(html, &meta(&[("foo", "bar")])), html);
    }

    #[tokio::test]
    async fn static_guards_traversal_missing_and_verbs() {
        let (addr, _keep) = spawn_static(&[], &[("index.html", "x")], StaticOpts::default()).await;
        for path in [
            "/../etc/passwd",
            "/a%2e%2e/b",
            "/..%2fetc",
            "/nope.txt",
            "/css//x",
        ] {
            let r = raw_http(addr, &get(addr, path)).await;
            assert!(r.starts_with("HTTP/1.1 404"), "{path}: {r}");
        }
        // 非 GET/HEAD 不走静态
        let r = raw_http(
            addr,
            "POST / HTTP/1.1\r\nHost: t\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(r.starts_with("HTTP/1.1 404"), "{r}");
    }

    // ----- 多静态站点（v0.1.27：prefix→dir，最长前缀命中）-----

    /// 双站点夹具：`/docs` → docs 根（含嵌套前缀用例文件），`/` → app 根。
    async fn spawn_two_sites(
        opts: StaticOpts,
    ) -> (std::net::SocketAddr, (TempRoutes, Vec<TempRoutes>)) {
        spawn_static_sites(
            &[],
            &[
                (
                    "/",
                    vec![
                        ("index.html", "<h1>app</h1>"),
                        ("docs/deep.txt", "APP-ROOT-SHOULD-NOT-SERVE"),
                    ],
                ),
                (
                    "/docs",
                    vec![("index.html", "<h1>docs</h1>"), ("api/x.txt", "DOCS-API-X")],
                ),
            ],
            opts,
        )
        .await
    }

    #[tokio::test]
    async fn multi_static_longest_prefix_wins_and_root_catchall() {
        let (addr, _keep) = spawn_two_sites(StaticOpts::default()).await;
        // 最长前缀：`/docs/api/x.txt` 命中 /docs 站，胜过 `/` 站。
        let r = raw_http(addr, &get(addr, "/docs/api/x.txt")).await;
        assert!(
            r.starts_with("HTTP/1.1 200") && r.contains("DOCS-API-X"),
            "{r}"
        );
        // 前缀根 → 该站 index.html。
        let r = raw_http(addr, &get(addr, "/docs")).await;
        assert!(
            r.starts_with("HTTP/1.1 200") && r.contains("<h1>docs</h1>"),
            "{r}"
        );
        // `/` 站兜底：前缀外路径落 `/` 站。
        let r = raw_http(addr, &get(addr, "/other.txt")).await;
        assert!(r.starts_with("HTTP/1.1 404"), "{r}");
        let r = raw_http(addr, &get(addr, "/")).await;
        assert!(
            r.starts_with("HTTP/1.1 200") && r.contains("<h1>app</h1>"),
            "{r}"
        );
    }

    #[tokio::test]
    async fn multi_static_site_miss_does_not_fall_across_sites() {
        // 不跨站：`/docs/deep.txt` 仅存在于 `/` 站的 docs/ 子目录——命中 /docs 站
        // （最长前缀）后未命中必须 404，**不得**回落到 `/` 站的同名文件。
        let (addr, _keep) = spawn_two_sites(StaticOpts::default()).await;
        let r = raw_http(addr, &get(addr, "/docs/deep.txt")).await;
        assert!(r.starts_with("HTTP/1.1 404"), "{r}");
    }

    #[tokio::test]
    async fn multi_static_spa_fallback_is_per_site() {
        // spa_fallback 全局开关，但回落目标各站独立：/docs 站有 index.html →
        // 深链接 200；`/` 站无 index.html → 404（不跨站借）。
        let (addr, _keep) = spawn_static_sites(
            &[],
            &[
                ("/", vec![("docs/deep.txt", "SHOULD-NOT-REACH")]),
                ("/docs", vec![("index.html", "<h1>docs</h1>")]),
            ],
            StaticOpts {
                spa_fallback: true,
                ..StaticOpts::default()
            },
        )
        .await;
        let accept = "GET /docs/deep/link HTTP/1.1\r\nHost: t\r\nAccept: text/html\r\nConnection: close\r\n\r\n";
        let r = raw_http(addr, accept).await;
        assert!(
            r.starts_with("HTTP/1.1 200") && r.contains("<h1>docs</h1>"),
            "{r}"
        );
        let accept =
            "GET /deep/link HTTP/1.1\r\nHost: t\r\nAccept: text/html\r\nConnection: close\r\n\r\n";
        let r = raw_http(addr, accept).await;
        assert!(r.starts_with("HTTP/1.1 404"), "{r}");
    }

    #[tokio::test]
    async fn multi_static_meta_is_per_site() {
        // html_meta 全局配置，meta JSON 按**命中站点**解析：仅 /docs 站有 __meta，
        // `/` 站 HTML 无注入（且无错）。
        let (addr, _keep) = spawn_static_sites(
            &[],
            &[
                (
                    "/",
                    vec![("index.html", "<html><head></head><body>app</body></html>")],
                ),
                (
                    "/docs",
                    vec![
                        ("index.html", "<html><head></head><body>docs</body></html>"),
                        ("__meta/index.json", r#"{"title":"Docs Home"}"#),
                    ],
                ),
            ],
            StaticOpts {
                html_meta: Some("__meta".to_string()),
                ..StaticOpts::default()
            },
        )
        .await;
        let r = raw_http(addr, &get(addr, "/docs/")).await;
        assert!(
            r.starts_with("HTTP/1.1 200") && r.contains("<title>Docs Home</title>"),
            "{r}"
        );
        // `/` 站无 __meta 目录：页面原样，仍 200。
        let r = raw_http(addr, &get(addr, "/")).await;
        assert!(
            r.starts_with("HTTP/1.1 200") && r.contains("app</body>") && !r.contains("<title>"),
            "{r}"
        );
    }

    #[test]
    fn resolve_static_blocks_decoded_traversal() {
        let root = Path::new("/srv");
        // %2F 走私（解码后含 /）、点段、反斜杠、NUL、空段 → None
        for p in [
            "/..%2fetc%2fpasswd",
            "/%2e%2e/x",
            "/a/b%2Fc",
            "/a%5Cb",
            "/a%00b",
            "/a//b",
        ] {
            assert_eq!(resolve_static(root, p, None), None, "{p}");
        }
    }

    #[test]
    fn strip_app_prefix_modes() {
        // "/" = 全路径兜底（与旧版一致）。
        assert_eq!(strip_app_prefix("/", "/a/b"), Some("/a/b"));
        // 非 "/" 前缀：前缀根 → "/"；前缀下 → 剥除；前缀外 → None。
        assert_eq!(strip_app_prefix("/site", "/site"), Some("/"));
        assert_eq!(strip_app_prefix("/site", "/site/"), Some("/"));
        assert_eq!(strip_app_prefix("/site", "/site/x/y"), Some("/x/y"));
        assert_eq!(strip_app_prefix("/site", "/sitex"), None);
        assert_eq!(strip_app_prefix("/site", "/other/x"), None);
    }

    // 静态断言：axum state 可跨线程（Send 边界）。
    fn _assert_send() {
        fn takes_send<T: Send>() {}
        takes_send::<JsActor>();
        takes_send::<AppState>();
    }

    #[tokio::test]
    async fn test_appstate_has_certificate_fields() {
        let state = dummy_app_state();
        let _ = &state.certificate_status;
        let _ = &state.certificate_valid_until;
    }

    #[tokio::test]
    async fn test_load_certificate_returns_err_for_invalid_key() {
        use base64::{Engine, engine::general_purpose};
        use tempfile::NamedTempFile;

        // Create temporary files for key and certificate
        let key_file = NamedTempFile::new().expect("Failed to create temp file for key");
        let cert_file = NamedTempFile::new().expect("Failed to create temp file for cert");

        // Write an invalid PEM key (not a real key)
        std::fs::write(
            key_file.path(),
            "-----BEGIN PUBLIC KEY-----\ninvalidkey\n-----END PUBLIC KEY-----\n",
        )
        .expect("Failed to write key");

        // Create a simple JWS with dummy payload
        let header = general_purpose::URL_SAFE_NO_PAD.encode(r#"{"alg":"RS256"}"#);
        let payload =
            general_purpose::URL_SAFE_NO_PAD.encode(r#"{"nbf":1000000000,"exp":2000000000}"#);
        let signature = general_purpose::URL_SAFE_NO_PAD.encode("signature");
        let jws = format!("{}.{}.{}", header, payload, signature);
        std::fs::write(cert_file.path(), jws).expect("Failed to write cert");

        // Create config pointing to our temporary files
        let cfg = only_js::config::ServerCfg {
            public_key_path: key_file.path().to_string_lossy().into_owned(),
            certificate_path: cert_file.path().to_string_lossy().into_owned(),
            grace_days: Some(30),
            ..only_js::config::ServerCfg::default()
        };

        // Call load_certificate - should return Err because the key is invalid
        let res = certificate::load_certificate(&cfg).await;
        assert!(
            res.is_err(),
            "Expected error due to invalid key, got {:?}",
            res
        );
    }

    // ---------- 补覆盖：serve()（bind 版） / blob 302 重定向 / mime_of ----------

    /// Given: 自由端口 + 合法路由；When: serve() 自行 bind 后收 GET；Then: 200 信封回包；
    /// 且端口被占时 serve() 以 bind 错误快速失败（Err 腿，覆盖 `bind().await?` 传播）。
    #[tokio::test]
    async fn given_free_addr_when_serve_binds_then_requests_answered() {
        let t = routes(&[(
            "u/s/api.ts",
            "export default { get() { json.ok({ ok: 1 }); } };",
        )]);
        let dir = t.0.clone();
        let table = build_table(&dir, true, "/v1/api");
        let actor = make_actor(dir.clone(), true);
        // 用临时 listener 探一个自由端口，drop 后交给 serve() 自行 bind。
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = probe.local_addr().unwrap();
        drop(probe);
        let server = tokio::spawn(serve(
            addr,
            "/v1/api",
            dir.clone(),
            true,
            table,
            actor,
            None,
            Vec::new(),
            StaticOpts::default(),
            Pipeline::default(),
        ));
        // 轮询等 bind 完成（spawn 与本测试同一 current_thread 运行时，await 期间被驱动）。
        let mut bound = false;
        for _ in 0..200 {
            if tokio::net::TcpStream::connect(addr).await.is_ok() {
                bound = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(bound, "serve() did not bind {addr}");
        let r = raw_http(
            addr,
            "GET /v1/api/u/s/ HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(
            r.starts_with("HTTP/1.1 200") && r.contains("\"ok\":1"),
            "{r}"
        );
        server.abort();

        // Err 腿：端口已被占 → serve() 返回 Err（bind 冲突 fail-fast）。
        let held = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let taken = held.local_addr().unwrap();
        let err = serve(
            taken,
            "/v1/api",
            t.0.clone(),
            true,
            RouteTable::default(),
            make_actor(t.0.clone(), true),
            None,
            Vec::new(),
            StaticOpts::default(),
            Pipeline::default(),
        )
        .await;
        assert!(err.is_err(), "expected bind-conflict error");
    }

    /// Given: serve 恒 302 的 blob 后端（模拟 s3 presign 直链）；When: GET {base}/blob/k；
    /// Then: SEE_OTHER(303) + Location 直链（重定向分支，local 后端永远走不到）。
    struct RedirectBlob;
    #[async_trait::async_trait]
    impl BlobBackend for RedirectBlob {
        async fn put(
            &self,
            _: &str,
            _: &[u8],
            _: Option<&str>,
        ) -> only_js::bridge::BridgeResult<()> {
            Err("not used".into())
        }
        async fn get(&self, _: &str) -> only_js::bridge::BridgeResult<Vec<u8>> {
            Err("not used".into())
        }
        async fn del(&self, _: &str) -> only_js::bridge::BridgeResult<()> {
            Err("not used".into())
        }
        async fn url(&self, _: &str) -> only_js::bridge::BridgeResult<String> {
            Ok(String::new())
        }
        async fn upload_url(
            &self,
            _: &str,
            _: &str,
        ) -> only_js::bridge::BridgeResult<serde_json::Value> {
            Err("not used".into())
        }
        async fn content_type(&self, _: &str) -> only_js::bridge::BridgeResult<Option<String>> {
            Ok(None)
        }
        async fn serve(&self, _: &str) -> only_js::bridge::BridgeResult<BlobServed> {
            Ok(BlobServed::Redirect(
                "https://s3.example.com/presigned".into(),
            ))
        }
    }

    #[tokio::test]
    async fn given_redirecting_blob_when_get_blob_route_then_see_other_with_location() {
        let t = routes(&[("u/api.ts", "export default { get() { json.ok({}); } };")]);
        let addr = spawn_blob("/v1/api", t.0.clone(), Arc::new(RedirectBlob)).await;
        let r = raw_http(
            addr,
            "GET /v1/api/blob/a.png HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(r.starts_with("HTTP/1.1 303"), "{r}");
        // hyper 落盘的 header 名恒小写。
        assert!(
            r.to_ascii_lowercase()
                .contains("location: https://s3.example.com/presigned"),
            "{r}"
        );
    }

    /// mime_of：常见扩展名映射 + 大小写归一 + 无扩展名/未知回落 octet-stream。
    #[test]
    fn given_common_extensions_when_mime_of_then_expected_content_types() {
        let m = |n: &str| mime_of(Path::new(n));
        assert_eq!(m("a.html"), "text/html; charset=utf-8");
        assert_eq!(m("a.htm"), "text/html; charset=utf-8");
        assert_eq!(m("a.css"), "text/css");
        assert_eq!(m("a.js"), "text/javascript");
        assert_eq!(m("a.MJS"), "text/javascript"); // 大小写归一
        assert_eq!(m("a.json"), "application/json");
        assert_eq!(m("a.map"), "application/json");
        assert_eq!(m("a.txt"), "text/plain; charset=utf-8");
        assert_eq!(m("a.md"), "text/plain; charset=utf-8");
        assert_eq!(m("a.svg"), "image/svg+xml");
        assert_eq!(m("a.PNG"), "image/png");
        assert_eq!(m("a.jpeg"), "image/jpeg");
        assert_eq!(m("a.gif"), "image/gif");
        assert_eq!(m("a.webp"), "image/webp");
        assert_eq!(m("a.avif"), "image/avif");
        assert_eq!(m("a.ico"), "image/x-icon");
        assert_eq!(m("a.wasm"), "application/wasm");
        assert_eq!(m("a.woff"), "font/woff");
        assert_eq!(m("a.woff2"), "font/woff2");
        assert_eq!(m("a.ttf"), "font/ttf");
        assert_eq!(m("a.xml"), "application/xml");
        assert_eq!(m("a.yaml"), "application/yaml");
        assert_eq!(m("a.yml"), "application/yaml");
        assert_eq!(m("a.pdf"), "application/pdf");
        assert_eq!(m("noext"), "application/octet-stream");
        assert_eq!(m("a.xyz"), "application/octet-stream");
    }

    /// Send 静态断言：编译期已验证，此处调用覆盖函数本体（零运行时成本）。
    #[test]
    fn given_send_assertion_when_called_then_holds() {
        _assert_send();
    }

    // ----- 流式响应 / SSE（v0.1.35）-----

    /// 流式路径：handler 用 `json.stream` 逐块写出 → 响应为 chunked 且绕过信封。
    #[tokio::test]
    async fn stream_responds_chunked_bypassing_envelope() {
        let t = routes(&[(
            "s/export/api.ts",
            r#"export default { get() {
                const w = json.stream({ contentType: "text/csv" });
                w.write("a,b\n");
                w.write("1,2\n");
                w.end();
            } };"#,
        )]);
        let addr = spawn_server("/v1/api", t.0.clone(), true, None).await;
        let r = raw_http(
            addr,
            "GET /v1/api/s/export/ HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        assert!(
            r.to_lowercase().contains("transfer-encoding: chunked"),
            "expected chunked transfer: {r}"
        );
        assert!(r.contains("content-type: text/csv"), "{r}");
        // 绕过信封：body 即裸 CSV（chunk 帧可能把两次 write 拆开 → 逐段断言），无 {code,msg,data}。
        assert!(r.contains("a,b"), "{r}");
        assert!(r.contains("1,2"), "{r}");
        assert!(!r.contains("\"code\""), "{r}");
    }

    /// SSE 路径：content-type=text/event-stream 且数据按 `data: ..\n\n` 帧化。
    #[tokio::test]
    async fn sse_sets_event_stream_content_type() {
        let t = routes(&[(
            "s/feed/api.ts",
            r#"export default { get() {
                const w = json.sse();
                w.write("hello");
                w.write("world");
                w.end();
            } };"#,
        )]);
        let addr = spawn_server("/v1/api", t.0.clone(), true, None).await;
        let r = raw_http(
            addr,
            "GET /v1/api/s/feed/ HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(
            r.to_lowercase().contains("content-type: text/event-stream"),
            "{r}"
        );
        // chunk 帧可能拆分两次 write → 逐段断言。
        assert!(r.contains("data: hello"), "{r}");
        assert!(r.contains("data: world"), "{r}");
    }

    /// 非流式行为不变：普通 json.ok 仍走缓冲信封（回归）。
    #[tokio::test]
    async fn non_stream_still_buffered_envelope() {
        let t = routes(&[(
            "s/echo/api.ts",
            "export default { get() { json.ok({ hi: 1 }); } };",
        )]);
        let addr = spawn_server("/v1/api", t.0.clone(), true, None).await;
        let r = raw_http(
            addr,
            "GET /v1/api/s/echo/ HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(r.contains("\"code\":0"), "{r}");
        assert!(r.contains("\"hi\":1"), "{r}");
    }

    // ----- CORS（v0.1.35）-----

    /// 带 CORS 配置起服务，返回监听地址（server.cors 段存在即启用）。
    async fn spawn_cors(
        base: &str,
        dir: PathBuf,
        ts: bool,
        cors: only_js::config::CorsCfg,
    ) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let table = build_table(&dir, ts, base);
        let base = base.to_string();
        tokio::spawn(async move {
            serve_with_listener(
                listener,
                &base,
                dir.clone(),
                ts,
                table,
                make_actor(dir, ts),
                None,
                Vec::new(),
                StaticOpts::default(),
                Pipeline::default(),
                Some(cors),
            )
            .await
            .unwrap();
        });
        addr
    }

    /// 预检（OPTIONS）由 CORS 层短路，返回 Allow-Origin/Allow-Methods；简单请求补 Allow-Origin。
    #[tokio::test]
    async fn cors_preflight_and_simple_request_get_headers() {
        let t = routes(&[(
            "u/f/api.ts",
            "export default { get() { json.ok({ ok: 1 }); } };",
        )]);
        let cors = only_js::config::CorsCfg {
            origins: vec!["http://example.com".into()],
            methods: vec!["GET".into()],
            headers: vec!["x-foo".into()],
            credentials: false,
            max_age: Some(600),
            expose: vec![],
        };
        let addr = spawn_cors("/v1/api", t.0.clone(), true, cors).await;

        let pre = raw_http(
            addr,
            "OPTIONS /v1/api/u/f/ HTTP/1.1\r\nHost: t\r\nOrigin: http://example.com\r\nAccess-Control-Request-Method: GET\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(
            pre.to_lowercase()
                .contains("access-control-allow-origin: http://example.com"),
            "{pre}"
        );
        assert!(
            pre.to_lowercase()
                .contains("access-control-allow-methods: GET")
                || pre.to_lowercase().contains("access-control-allow-methods"),
            "{pre}"
        );

        let ok = raw_http(
            addr,
            "GET /v1/api/u/f/ HTTP/1.1\r\nHost: t\r\nOrigin: http://example.com\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(
            ok.to_lowercase()
                .contains("access-control-allow-origin: http://example.com"),
            "{ok}"
        );
    }

    /// 无 server.cors 段时行为与现在一致（不挂 CORS 层，响应无 Access-Control-* 头）。
    #[tokio::test]
    async fn cors_absent_means_no_headers() {
        let t = routes(&[(
            "u/f/api.ts",
            "export default { get() { json.ok({ ok: 1 }); } };",
        )]);
        let addr = spawn_server("/v1/api", t.0.clone(), true, None).await;
        let ok = raw_http(
            addr,
            "GET /v1/api/u/f/ HTTP/1.1\r\nHost: t\r\nOrigin: http://example.com\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(
            !ok.to_lowercase().contains("access-control-allow-origin"),
            "{ok}"
        );
    }
}
