//! mdm-server：HTTP 层。目录镜像路由（routes）+ JS actor 线程桥（actor）+ axum 装配（本文件）。

pub mod actor;
pub mod routes;
pub mod ws;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use serde_json::Value;

use crate::actor::JsActor;
use crate::routes::{Lookup, RouteTable, Routes};
use mdm_base_rust::bridge::{fail, RequestInfo};

/// 共享状态（JsActor 句柄 Clone = 同一 actor 队列的多份引用）。
#[derive(Clone)]
pub struct AppState {
    table: RouteTable,
    /// dev（ts=true）文件系统兜底：表 miss 时回退目录镜像；release None。
    fallback: Option<Routes>,
    actor: JsActor,
    /// 单请求超时（None = 不限时）。
    timeout: Option<std::time::Duration>,
}

/// 构造 axum 应用：catch-all fallback（对齐 Go fiber 的 `All("/*")`）。
pub fn app(
    base: &str,
    dir: impl Into<PathBuf>,
    ts: bool,
    table: RouteTable,
    actor: JsActor,
    timeout: Option<std::time::Duration>,
) -> Router {
    let dir = dir.into();
    Router::new().fallback(any(handle)).with_state(AppState {
        table,
        fallback: ts.then(|| Routes::new(base, dir, ts)),
        actor,
        timeout,
    })
}

/// 绑定监听并服务。
pub async fn serve(
    addr: std::net::SocketAddr,
    base: &str,
    dir: impl Into<PathBuf>,
    ts: bool,
    table: RouteTable,
    actor: JsActor,
    timeout: Option<std::time::Duration>,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    serve_with_listener(listener, base, dir, ts, table, actor, timeout).await
}

/// 已绑定监听上服务（测试/T11：先 bind 端口 0 再读 local_addr）。
pub async fn serve_with_listener(
    listener: tokio::net::TcpListener,
    base: &str,
    dir: impl Into<PathBuf>,
    ts: bool,
    table: RouteTable,
    actor: JsActor,
    timeout: Option<std::time::Duration>,
) -> std::io::Result<()> {
    axum::serve(listener, app(base, dir, ts, table, actor, timeout)).await
}

async fn handle(
    State(st): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let verb = method.as_str();
    let run = |file: PathBuf, params: HashMap<String, String>| {
        let req = RequestInfo {
            method: verb.to_string(),
            params,
            query: parse_query(uri.query()),
            headers: headers
                .iter()
                .filter_map(|(k, v)| Some((k.to_string(), v.to_str().ok()?.to_string())))
                .collect(),
            body: body.to_vec(),
        };
        let m = crate::routes::method_name(verb).expect("checked by caller").to_string();
        async move {
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
    fail_response(404, "no route matched")
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use mdm_base_rust::bridge::{Bridge, InMemoryKV, LoaderShared, SchemaRegistry};
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
            )
        })
    }

    pub(crate) async fn spawn_server(
        base: &str,
        dir: PathBuf,
        ts: bool,
        timeout: Option<std::time::Duration>,
    ) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let table = build_table(&dir, ts, base);
        let base = base.to_string();
        tokio::spawn(async move {
            serve_with_listener(listener, &base, dir.clone(), ts, table, actor(dir, ts), timeout)
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

    // 静态断言：axum state 可跨线程（Send 边界）。
    fn _assert_send() {
        fn takes_send<T: Send>() {}
        takes_send::<JsActor>();
        takes_send::<AppState>();
    }
}
