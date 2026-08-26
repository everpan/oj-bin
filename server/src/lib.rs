//! mdm-server：HTTP 层。目录镜像路由（routes）+ JS actor 线程桥（actor）+ axum 装配（本文件）。

pub mod actor;
pub mod auth;
pub mod logging;
pub mod routes;
pub mod ws;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use serde_json::Value;

use crate::actor::JsActor;
use crate::auth::Auth;
use crate::routes::{Lookup, RouteTable, Routes};
use mdm_base_rust::bridge::{fail, BlobBackend, BlobServed, RequestInfo, UploadedFile};
use serde_json::json;

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

/// 共享状态（JsActor 句柄 Clone = 同一 actor 队列的多份引用）。
#[derive(Clone)]
pub struct AppState {
    table: RouteTable,
    /// dev（ts=true）文件系统兜底：表 miss 时回退目录镜像；release None。
    fallback: Option<Routes>,
    actor: JsActor,
    /// 单请求超时（None = 不限时）。
    timeout: Option<std::time::Duration>,
    /// 静态站点根（config server.root）；None → 不开静态服务。
    static_root: Option<PathBuf>,
    /// handle() 前置管线（OJ-3..5 单一扩展点；后续阶段只加字段）。
    pipeline: Pipeline,
    /// API 基础前缀（内置 auth 路由 / 匿名路径匹配用）。
    base: String,
    /// 当前证书状态
    pub certificate_status: CertificateStatus,
    /// 证书有效期截止时间
    pub certificate_valid_until: Option<std::time::SystemTime>,
}

/// handle() 前置管线配置：请求进入 JS 前的注入/守卫（租户/鉴权/上传）。
#[derive(Clone)]
pub struct Pipeline {
    /// Some(header) = 租户启用：缺失/空 → 400，命中 → http.tenantId。
    pub tenant_header: Option<String>,
    /// Some = 鉴权启用：内置 {base}/auth/* 路由 + Bearer 守卫 + http.user。
    pub auth: Option<Arc<Auth>>,
    /// 上传/请求体上限（超限 413 信封）；axum body limit = 2x（超 2x 裸 413，ponytail: 接受）。
    pub max_upload: u64,
    /// Some = blob 启用：`{base}/blob/{key}` 公开下载（local 直出 / s3 302 presign）。
    pub blob: Option<Arc<dyn BlobBackend>>,
}

impl Default for Pipeline {
    fn default() -> Self {
        Self {
            tenant_header: None,
            auth: None,
            max_upload: 10 * 1024 * 1024, // 10MiB
            blob: None,
        }
    }
}

/// 构造 axum 应用：catch-all fallback（对齐 Go fiber 的 `All("/*")`）。
#[allow(clippy::too_many_arguments)]
pub fn app(
    base: &str,
    dir: impl Into<PathBuf>,
    ts: bool,
    table: RouteTable,
    actor: JsActor,
    timeout: Option<std::time::Duration>,
    static_root: Option<PathBuf>,
    pipeline: Pipeline,
) -> Router {
    let dir = dir.into();
    Router::new()
        .fallback(any(handle))
        // 请求日志中间件（method/path/status/耗时 → 文件日志 + stderr）。
        .layer(axum::middleware::from_fn(crate::logging::log_requests))
        // 超 2x max_upload 的请求在 axum 层直接被拒（裸 413）；handle() 内再做信封 413。
        .layer(axum::extract::DefaultBodyLimit::max((pipeline.max_upload * 2) as usize))
        .with_state(AppState {
            table,
            fallback: ts.then(|| Routes::new(base, dir, ts)),
            actor,
            timeout,
            static_root,
            pipeline,
            base: base.to_string(),
            certificate_status: CertificateStatus::Valid,
            certificate_valid_until: None,
        })
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
    static_root: Option<PathBuf>,
    pipeline: Pipeline,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    serve_with_listener(listener, base, dir, ts, table, actor, timeout, static_root, pipeline).await
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
    static_root: Option<PathBuf>,
    pipeline: Pipeline,
) -> std::io::Result<()> {
    serve_router(listener, app(base, dir, ts, table, actor, timeout, static_root, pipeline)).await
}

/// 已绑定监听 + 完整 Router 服务（oj server 生产路径：app().merge(ws) 后经此起服务）。
pub async fn serve_router(
    listener: tokio::net::TcpListener,
    router: axum::Router,
) -> std::io::Result<()> {
    axum::serve(listener, router).await
}

async fn handle(
    State(st): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let verb = method.as_str();
    // 内置 auth 路由（{base}/auth/*）：先于路由表（login/refresh/logout，POST only）。
    if let Some(auth) = st.pipeline.auth.clone()
        && let Some(rest) = crate::routes::normalize(uri.path())
            .and_then(|p| p.strip_prefix(&format!("{}/auth/", st.base)).map(|s| s.to_string()))
    {
        return match (verb, rest.as_str()) {
            ("POST", "login") => {
                let v: Value = serde_json::from_slice(&body).unwrap_or_default();
                let (u, p) = (
                    v["username"].as_str().unwrap_or_default().to_string(),
                    v["password"].as_str().unwrap_or_default().to_string(),
                );
                auth_json(auth.login(&u, &p).await)
            }
            ("POST", "refresh") => {
                let v: Value = serde_json::from_slice(&body).unwrap_or_default();
                let t = v["refresh_token"].as_str().unwrap_or_default().to_string();
                auth_json(auth.refresh(&t).await)
            }
            ("POST", "logout") => {
                let v: Value = serde_json::from_slice(&body).unwrap_or_default();
                let t = v["refresh_token"].as_str().unwrap_or_default().to_string();
                auth_json(auth.logout(&t).await.map(|_| Value::Null))
            }
            _ => fail_response(405, "auth route not found"),
        };
    }
    // 内置 blob 下载路由（{base}/blob/{key}，公开 GET，先于路由表）。
    if verb == "GET"
        && let Some(blob) = st.pipeline.blob.as_ref()
        && let Some(key) = uri.path().strip_prefix(&format!("{}/blob/", st.base))
        && let Some(key) = decode_blob_key(key)
    {
        return match blob.serve(&key).await {
            Ok(BlobServed::Bytes(bytes, ct)) => {
                let mut r = Response::new(axum::body::Body::from(bytes));
                if let Some(ct) = ct {
                    r.headers_mut().insert(
                        axum::http::header::CONTENT_TYPE,
                        axum::http::HeaderValue::from_str(&ct)
                            .unwrap_or(axum::http::HeaderValue::from_static("application/octet-stream")),
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
            Err(_) => fail_response(404, "blob not found"),
        };
    }
    // 去 base 路径（鉴权匿名匹配用；不在 base 下 → None = 不设防）。
    let path_no_base = crate::routes::normalize(uri.path())
        .and_then(|p| p.strip_prefix(st.base.as_str()).map(|s| s.to_string()));
    let run = |file: PathBuf, params: HashMap<String, String>| {
        let m = crate::routes::method_name(verb).expect("checked by caller").to_string();
        let query = parse_query(uri.query());
        let path_no_base = path_no_base.clone();
        async move {
            // 前置管线：鉴权（base 内非匿名路径必须带有效 Bearer → 401；通过 → http.user）。
            let user = match (st.pipeline.auth.as_ref(), path_no_base.as_deref()) {
                (Some(auth), Some(p)) if !auth.is_anonymous(p) => {
                    let Some(claims) = headers
                        .get(axum::http::header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.strip_prefix("Bearer "))
                        .and_then(|t| auth.verify_access(t).ok())
                    else {
                        return fail_response(401, "missing or invalid bearer token");
                    };
                    Some(json!({ "id": claims.sub, "roles": claims.roles, "claims": claims }))
                }
                _ => None,
            };
            // 前置管线：租户提取（启用后缺失/空 → 400）。
            let tenant_id = match st.pipeline.tenant_header.as_deref() {
                Some(key) => {
                    let Some(tid) = headers
                        .get(key)
                        .and_then(|v| v.to_str().ok())
                        .filter(|s| !s.is_empty())
                    else {
                        return fail_response(400, &format!("missing tenant header: {key}"));
                    };
                    Some(tid.to_string())
                }
                None => None,
            };
            // 上传/请求体上限（信封 413）；超 2x 的已在 axum 层被拒。
            if body.len() > st.pipeline.max_upload as usize {
                return fail_response(413, "upload too large");
            }
            // multipart：文本字段并入 body（{name: value}），文件入 files。
            let (body_bytes, files) = if is_multipart(&headers) {
                parse_multipart(&headers, &body).await
            } else {
                (body.to_vec(), Vec::new())
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
                tenant_id,
                user,
                files,
                bus_tx: None,
            };
            match st.actor.run_module(file, m, req, st.timeout).await {
                Ok(cap) => capture_response(cap),
                // 超时熔断 → 408（对齐 Go dev server）。
                Err(e) if e.timeout => fail_response(408, &e.msg),
                Err(e) => fail_response(500, &e.msg),
            }
        }
    };
    if let Some(path) = crate::routes::normalize(uri.path()) {
        match st.table.lookup(&path, verb) {
            Lookup::Hit { file, params } => return run(file, params).await,
            Lookup::Conflict(msg) => return fail_response(500, &msg),
            Lookup::MethodNotAllowed => {
                return fail_response(405, &format!("method {verb} not allowed"))
            }
            Lookup::NotFound => {}
        }
    }
    // dev 兜底：目录镜像（挂 .route 的方法已被替换，不得复活）。
    if let Some(file) = st.fallback.as_ref().and_then(|fb| fb.resolve(uri.path())) {
        // 表内路径经过 canonicalize（macOS /var ↔ /private/var），对齐后再比对
        let file = file.canonicalize().unwrap_or(file);
        match crate::routes::method_name(verb) {
            Some(m) if !st.table.is_replaced(&file, m) => return run(file, HashMap::new()).await,
            Some(_) => {} // replaced → 404
            None => return fail_response(405, &format!("method {verb} not mapped")),
        }
    }
    // 静态站点兜底（server.root）：API 优先，GET/HEAD only。
    if let Some(root) = st.static_root.as_deref()
        && matches!(verb, "GET" | "HEAD")
        && let Some(file) = resolve_static(root, uri.path())
        && let Ok(body) = tokio::fs::read(&file).await
    {
        return file_response(&file, body);
    }
    fail_response(404, "no route matched")
}

/// blob 下载 key：percent-decode 每段后过 valid_key（防 `%2e%2e` 穿越，与 resolve_static 同款守卫）。
fn decode_blob_key(s: &str) -> Option<String> {
    let decoded = s
        .split('/')
        .map(|seg| percent_encoding::percent_decode_str(seg).decode_utf8().ok())
        .collect::<Option<Vec<_>>>()?
        .join("/");
    mdm_base_rust::bridge::valid_key(&decoded).then_some(decoded)
}

/// 静态文件解析：uri.path()（仍 percent-encoded）逐段解码后拼 root；
/// 根/目录 → index.html；越界段（`.`/`..`/`\`/`/`/`\0`/空段，含解码后——
/// `%2F` 走私等价穿越）→ None（404）。
fn resolve_static(root: &Path, uri_path: &str) -> Option<PathBuf> {
    let rel = uri_path.strip_prefix('/')?.trim_end_matches('/');
    let mut p = root.to_path_buf();
    if !rel.is_empty() {
        for seg in rel.split('/') {
            let s = percent_encoding::percent_decode_str(seg).decode_utf8().ok()?;
            if s.is_empty() || s == "." || s == ".." || s.contains(['/', '\\', '\0']) {
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
    match p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase().as_str() {
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
fn capture_response(cap: mdm_base_rust::bridge::Capture) -> Response {
    let mut r = Response::new(axum::body::Body::from(cap.body));
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

/// 内置 auth 路由响应：Ok(data) → ok 信封 200；Err(msg) → 401 信封。
fn auth_json(r: Result<Value, String>) -> Response {
    match r {
        Ok(data) => {
            let mut resp = Response::new(axum::body::Body::from(mdm_base_rust::bridge::ok(&data)));
            resp.headers_mut().insert(
                axum::http::header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static("application/json"),
            );
            resp
        }
        Err(msg) => fail_response(401, &msg),
    }
}

/// 统一失败信封。
fn fail_response(code: i32, msg: &str) -> Response {
    let (body, status) = fail(code, msg, &Value::Null);
    let mut r = Response::new(axum::body::Body::from(body));
    *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    r
}

/// `a=1&b=2` → map（application/x-www-form-urlencoded 解码，+ → 空格、%xx → 字节）。
fn parse_query(q: Option<&str>) -> HashMap<String, String> {
    q.map(|s| form_urlencoded::parse(s.as_bytes()).map(|(k, v)| (k.into_owned(), v.into_owned())).collect())
        .unwrap_or_default()
}

/// content-type 是否 multipart/form-data。
fn is_multipart(headers: &HeaderMap) -> bool {
    headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| s.starts_with("multipart/form-data"))
}

/// multer 解析：文本字段 → {name: value}，文件 → Vec<UploadedFile>。
/// body 已整体在内存（DefaultBodyLimit 上限内），用 once stream 喂 multer。
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
            fields.insert(name, Value::String(String::from_utf8_lossy(&bytes).into_owned()));
        } else {
            files.push(UploadedFile { field: name, filename, content_type, bytes });
        }
    }
    (serde_json::to_vec(&fields).unwrap_or_default(), files)
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use mdm_base_rust::bridge::{Bridge, Extras, InMemoryKV, LoaderShared, LocalBlob, SchemaRegistry};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
    pub(crate) fn actor(root: PathBuf, ts: bool) -> JsActor {
        JsActor::new(move || {
            Bridge::with_dbs_and_loader(
                HashMap::new(),
                Arc::new(InMemoryKV::new()),
                SchemaRegistry::new(),
                false,
                Some(Arc::new(LoaderShared { project_root: root.clone(), ts })),
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
                listener, &base, dir.clone(), ts, table, actor(dir, ts), timeout, None, pipeline,
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
                Some(Arc::new(LoaderShared { project_root: root.clone(), ts: true })),
                Extras { blobs: Some(mdm_base_rust::bridge::blob::registry_with_default(blob2.clone())), ..Default::default() },
            )
        });
        let base = base.to_string();
        let pipeline = Pipeline { blob: Some(blob), ..Default::default() };
        tokio::spawn(async move {
            serve_with_listener(
                listener, &base, dir.clone(), true, table, actor, None, None, pipeline,
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
                    Some(Arc::new(LoaderShared { project_root: root.clone(), ts })),
                    Extras::default(),
                )
            }
        };
        let (t, failures) =
            crate::routes::RouteTable::build(base, &root, ts, crate::routes::bridge_introspector(make));
        if !failures.is_empty() {
            eprintln!("build_table failures: {failures:?}");
        }
        t
    }

    async fn raw_http(addr: std::net::SocketAddr, req: &str) -> String {
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(req.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).await.unwrap();
        String::from_utf8_lossy(&buf).into_owned()
    }

    /// tenant.enable：header 存在 → 注入 http.tenantId；缺失 → 400；未启用 → null。
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
            Pipeline { tenant_header: Some("X-TENANT-ID".into()), ..Default::default() },
        )
        .await;
        let ok = raw_http(
            addr,
            "GET /v1/api/u/f/ HTTP/1.1\r\nHost: t\r\nX-TENANT-ID: acme\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(ok.starts_with("HTTP/1.1 200") && ok.contains("\"t\":\"acme\""), "{ok}");
        let miss = raw_http(
            addr,
            "GET /v1/api/u/f/ HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(miss.starts_with("HTTP/1.1 400") && miss.contains("X-TENANT-ID"), "{miss}");
        // 未启用 → 无注入也无 400
        let addr2 = spawn_server("/v1/api", t.0.clone(), true, None).await;
        let plain = raw_http(
            addr2,
            "GET /v1/api/u/f/ HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(plain.starts_with("HTTP/1.1 200") && plain.contains("\"t\":null"), "{plain}");
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
        let v: Value = serde_json::from_slice(r.split("\r\n\r\n").nth(1).unwrap_or("null").as_bytes()).unwrap();
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
        let v: Value = serde_json::from_slice(r.split("\r\n\r\n").nth(1).unwrap_or("null").as_bytes()).unwrap();
        assert!(r.starts_with("HTTP/1.1 200"), "2: {r}");
        assert_eq!(v["data"]["name"], serde_json::Value::Null, "{v}");
        assert_eq!(v["data"]["note"], "hi", "{v}");
        // 超 max_upload（8B，默认 body limit=16B，12B JSON 能进 handle）→ 413 信封。
        let addr2 = spawn_pipeline(
            "/v1/api",
            t.0.clone(),
            true,
            None,
            Pipeline { max_upload: 8, ..Default::default() },
        )
        .await;
        let r = raw_http(
            addr2,
            "POST /v1/api/u/ HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\nContent-Length: 12\r\nConnection: close\r\n\r\n{\"a\":123456}",
        )
        .await;
        assert!(r.starts_with("HTTP/1.1 413") && r.contains("upload too large"), "3: {r}");
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
        let mp = format!(
            "--X-BND\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.png\"\r\nContent-Type: image/png\r\n\r\nPNGDATA\r\n--X-BND--\r\n"
        );
        let req = format!(
            "POST /v1/api/u/ HTTP/1.1\r\nHost: t\r\nContent-Type: multipart/form-data; boundary=X-BND\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{mp}",
            mp.len()
        );
        let r = raw_http(addr, &req).await;
        assert!(r.starts_with("HTTP/1.1 200") && r.contains("\"n\":7"), "upload: {r}");
        // 下载：bytes 一致 + Content-Type 按扩展名推断
        let r = raw_http(
            addr,
            "GET /v1/api/blob/a.png HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(r.starts_with("HTTP/1.1 200") && r.contains("image/png") && r.ends_with("PNGDATA"), "get: {r}");
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

    /// auth 全链路：401/匿名/login/Bearer 注入/篡改/refresh 轮换/logout。
    #[tokio::test]
    async fn auth_full_pipeline() {
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
        // auth 自带库：users 表 + demo 用户（bcrypt cost 4 提速）。
        let db = mdm_base_rust::bridge::SqlxAccessor::arc("sqlite::memory:").await.unwrap();
        let hash = bcrypt::hash("pw", 4).unwrap();
        db.exec_with_params(
            "create table users (id integer primary key, username text, password_hash text, roles text)",
            &[],
        )
        .await
        .unwrap();
        db.exec_with_params(
            "insert into users (username, password_hash, roles) values ('demo', ?, ?)",
            &[serde_json::json!(hash), serde_json::json!(r#"["admin"]"#)],
        )
        .await
        .unwrap();
        let auth = Arc::new(
            crate::auth::Auth::new(
                &mdm_base_rust::config::AuthCfg {
                    jwt_secret: "test-secret".into(),
                    anonymous_paths: vec!["/health".into()],
                    ..Default::default()
                },
                db,
                Arc::new(mdm_base_rust::bridge::InMemoryKV::new()),
            )
            .unwrap(),
        );
        let addr = spawn_pipeline(
            "/v1/api",
            t.0.clone(),
            true,
            None,
            Pipeline { auth: Some(auth), ..Default::default() },
        )
        .await;
        let get = |p: &str, token: Option<&str>| {
            format!(
                "GET {p} HTTP/1.1\r\nHost: t\r\n{}Connection: close\r\n\r\n",
                token.map(|t| format!("Authorization: Bearer {t}\r\n")).unwrap_or_default()
            )
        };
        // 1) 无 token 访问 /me → 401
        let r = raw_http(addr, &get("/v1/api/me/", None)).await;
        assert!(r.starts_with("HTTP/1.1 401"), "1: {r}");
        // 2) /health 匿名 → 200
        let r = raw_http(addr, &get("/v1/api/health/", None)).await;
        assert!(r.starts_with("HTTP/1.1 200"), "2: {r}");
        // 3) login 错密码 → 401
        let login_req = |body: &str| {
            format!(
                "POST /v1/api/auth/login HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
        };
        let r = raw_http(addr, &login_req(r#"{"username":"demo","password":"x"}"#)).await;
        assert!(r.starts_with("HTTP/1.1 401"), "3: {r}");
        // 4) login 成功 → Bearer 访问 /me → 200 且 user.id=="1"、roles==["admin"]
        let r = raw_http(addr, &login_req(r#"{"username":"demo","password":"pw"}"#)).await;
        assert!(r.starts_with("HTTP/1.1 200"), "4a: {r}");
        let v: Value = serde_json::from_slice(r.split("\r\n\r\n").nth(1).unwrap_or("null").as_bytes()).unwrap();
        let at = v["data"]["access_token"].as_str().unwrap().to_string();
        let rt1 = v["data"]["refresh_token"].as_str().unwrap().to_string();
        let r = raw_http(addr, &get("/v1/api/me/", Some(&at))).await;
        assert!(r.starts_with("HTTP/1.1 200") && r.contains("\"id\":\"1\"") && r.contains("\"roles\":[\"admin\"]"), "4b: {r}");
        // 5) 篡改 token → 401
        let r = raw_http(addr, &get("/v1/api/me/", Some(&format!("{at}x")))).await;
        assert!(r.starts_with("HTTP/1.1 401"), "5: {r}");
        // 6) refresh → 新 access 可用
        let r = raw_http(
            addr,
            &format!(
                "POST /v1/api/auth/refresh HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{{\"refresh_token\":\"{rt1}\"}}",
                format!("{{\"refresh_token\":\"{rt1}\"}}").len()
            ),
        )
        .await;
        assert!(r.starts_with("HTTP/1.1 200"), "6a: {r}");
        let v: Value = serde_json::from_slice(r.split("\r\n\r\n").nth(1).unwrap_or("null").as_bytes()).unwrap();
        let at2 = v["data"]["access_token"].as_str().unwrap().to_string();
        let rt2 = v["data"]["refresh_token"].as_str().unwrap().to_string();
        let r = raw_http(addr, &get("/v1/api/me/", Some(&at2))).await;
        assert!(r.starts_with("HTTP/1.1 200"), "6b: {r}");
        // 7) logout 后 refresh 旧 token → 401
        let r = raw_http(
            addr,
            &format!(
                "POST /v1/api/auth/logout HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{{\"refresh_token\":\"{rt2}\"}}",
                format!("{{\"refresh_token\":\"{rt2}\"}}").len()
            ),
        )
        .await;
        assert!(r.starts_with("HTTP/1.1 200"), "7a: {r}");
        let r = raw_http(
            addr,
            &format!(
                "POST /v1/api/auth/refresh HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{{\"refresh_token\":\"{rt2}\"}}",
                format!("{{\"refresh_token\":\"{rt2}\"}}").len()
            ),
        )
        .await;
        assert!(r.starts_with("HTTP/1.1 401"), "7b: {r}");
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
        let t = routes(&[("u/f/api.ts", "export default { get() { while (true) {} } };")]);
        let addr =
            spawn_server("/v1/api", t.0.clone(), true, Some(std::time::Duration::from_millis(200))).await;
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
        assert!(ok.starts_with("HTTP/1.1 200") && ok.contains("a/b/c"), "{ok}");
        for path in ["/v1/api/file", "/v1/api/file/", "/v1/api//file/a", "/v1/api/file/%2e%2e"] {
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
        assert!(r.starts_with("HTTP/1.1 500") && r.contains("route conflict"), "{r}");
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

    // ----- 静态站点（server.root）-----

    /// 返回 (addr, 夹具)：夹具须在测试内持有（TempRoutes Drop 会删目录）。
    async fn spawn_static(
        api: &[(&str, &str)],
        site: &[(&str, &str)],
    ) -> (std::net::SocketAddr, (TempRoutes, TempRoutes)) {
        let t = routes(api);
        let s = routes(site);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (dir, table, site) = (t.0.clone(), build_table(&t.0, true, "/v1/api"), s.0.clone());
        tokio::spawn(async move {
            serve_with_listener(
                listener, "/v1/api", dir.clone(), true, table, actor(dir, true), None, Some(site),
                Pipeline::default(),
            )
            .await
            .unwrap();
        });
        (addr, (t, s))
    }

    #[tokio::test]
    async fn serves_static_index_files_and_content_types() {
        let (addr, _keep) = spawn_static(
            &[("u/f/api.ts", "export default { get() { json.ok({ api: true }); } };")],
            &[("index.html", "<h1>hi</h1>"), ("css/app.css", "body{}"), ("v1/api/u", "STATIC")],
        )
        .await;
        // / → index.html + text/html
        let r = raw_http(addr, "GET / HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n").await;
        assert!(r.starts_with("HTTP/1.1 200") && r.contains("text/html") && r.contains("<h1>hi</h1>"), "{r}");
        // 普通文件 + Content-Type；目录 → index.html
        let r = raw_http(addr, &get(addr, "/css/app.css")).await;
        assert!(r.starts_with("HTTP/1.1 200") && r.contains("text/css"), "{r}");
        // API 优先于静态：同名路径走路由表
        let r = raw_http(addr, &get(addr, "/v1/api/u/f/")).await;
        assert!(r.starts_with("HTTP/1.1 200") && r.contains("\"api\":true") && !r.contains("STATIC"), "{r}");
    }

    #[tokio::test]
    async fn static_guards_traversal_missing_and_verbs() {
        let (addr, _keep) = spawn_static(&[], &[("index.html", "x")]).await;
        for path in ["/../etc/passwd", "/a%2e%2e/b", "/..%2fetc", "/nope.txt", "/css//x"] {
            let r = raw_http(addr, &get(addr, path)).await;
            assert!(r.starts_with("HTTP/1.1 404"), "{path}: {r}");
        }
        // 非 GET/HEAD 不走静态
        let r = raw_http(addr, "POST / HTTP/1.1\r\nHost: t\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
        assert!(r.starts_with("HTTP/1.1 404"), "{r}");
    }

    #[test]
    fn resolve_static_blocks_decoded_traversal() {
        let root = Path::new("/srv");
        // %2F 走私（解码后含 /）、点段、反斜杠、NUL、空段 → None
        for p in [
            "/..%2fetc%2fpasswd", "/%2e%2e/x", "/a/b%2Fc", "/a%5Cb", "/a%00b", "/a//b",
        ] {
            assert_eq!(resolve_static(root, p), None, "{p}");
        }
    }

    // 静态断言：axum state 可跨线程（Send 边界）。
    fn _assert_send() {
        fn takes_send<T: Send>() {}
        takes_send::<JsActor>();
        takes_send::<AppState>();
    }

    }

    /// 创建用于测试的最小 AppState
    pub fn dummy_app_state() -> AppState {
        AppState {
            table: RouteTable::default(),
            fallback: None,
            actor: JsActor::new(|| panic!("dummy actor")),
            timeout: None,
            static_root: None,
            pipeline: Pipeline::default(),
            base: "/v1/api".to_string(),
            certificate_status: CertificateStatus::Valid,
            certificate_valid_until: None,
        }
    }

    #[tokio::test]
    async fn test_appstate_has_certificate_fields() {
        let state = dummy_app_state();
        let _ = &state.certificate_status;
        let _ = &state.certificate_valid_until;
    }

