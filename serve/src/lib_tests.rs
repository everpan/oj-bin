use super::*;

use only_js::bridge::{Bridge, Extras, InMemoryKV, LoaderShared, LocalBlob, SchemaRegistry};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// Helper for tests: create a minimal AppState
pub(crate) fn dummy_app_state() -> AppState {
    AppState {
        mounts: Vec::new(),
        actor: make_actor(PathBuf::from("."), false),
        timeout: None,
        static_opts: StaticOpts::default(),
        pipeline: Pipeline::default(),
        certificate_status: Arc::new(RwLock::new(CertificateStatus::Valid)),
        certificate_valid_until: Arc::new(RwLock::new(None)),
        plugins: Arc::default(),
        exec_registry: None,
    }
}

/// capture_response 写回（同名头多值，v0.1.46）：Set-Cookie 追加（双发 oj_sess +
/// oj_csrf 的唯一通道，防回归）；非 cookie 同名头保持覆盖写（最后一个生效）。
#[test]
fn capture_response_set_cookie_appends_others_last_wins() {
    let cap = only_js::bridge::Capture {
        status: 200,
        headers: vec![
            ("Set-Cookie".to_string(), "oj_sess=a; HttpOnly".to_string()),
            (
                "Set-Cookie".to_string(),
                "oj_csrf=t; SameSite=Lax".to_string(),
            ),
            ("X-Dup".to_string(), "one".to_string()),
            ("X-Dup".to_string(), "two".to_string()),
        ],
        body: b"{\"code\":0}".to_vec(),
        stream: None,
        raw: false,
    };
    let r = capture_response(cap);
    let cookies: Vec<_> = r
        .headers()
        .get_all(axum::http::header::SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap())
        .collect();
    assert_eq!(cookies, ["oj_sess=a; HttpOnly", "oj_csrf=t; SameSite=Lax"]);
    assert_eq!(r.headers()["X-Dup"], "two");
    assert_eq!(r.status(), axum::http::StatusCode::OK);
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
    let mounts = vec![RuntimeMount::Api {
        prefix: base.clone(),
        table,
        fallback: ts.then(|| Routes::new(&base, dir.clone(), ts)),
        web: None,
    }];
    tokio::spawn(async move {
        serve_with_listener(
            listener,
            mounts,
            make_actor(dir, ts),
            timeout,
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
async fn spawn_blob(base: &str, dir: PathBuf, blob: Arc<dyn BlobBackend>) -> std::net::SocketAddr {
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
    let mounts = vec![RuntimeMount::Api {
        prefix: base.clone(),
        table,
        fallback: Some(Routes::new(&base, dir.clone(), true)),
        web: None,
    }];
    tokio::spawn(async move {
        serve_with_listener(
            listener,
            mounts,
            actor,
            None,
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
        None,
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
        serde_json::from_slice(r.split("\r\n\r\n").nth(1).unwrap_or("null").as_bytes()).unwrap();
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
        serde_json::from_slice(r.split("\r\n\r\n").nth(1).unwrap_or("null").as_bytes()).unwrap();
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
        serde_json::from_slice(r.split("\r\n\r\n").nth(1).unwrap_or("null").as_bytes()).unwrap();
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
        serde_json::from_slice(r.split("\r\n\r\n").nth(1).unwrap_or("null").as_bytes()).unwrap();
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
        serde_json::from_slice(r.split("\r\n\r\n").nth(1).unwrap_or("null").as_bytes()).unwrap();
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
        serde_json::from_slice(r.split("\r\n\r\n").nth(1).unwrap_or("null").as_bytes()).unwrap();
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
        serde_json::from_slice(r.split("\r\n\r\n").nth(1).unwrap_or("null").as_bytes()).unwrap();
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
        serde_json::from_slice(r.split("\r\n\r\n").nth(1).unwrap_or("null").as_bytes()).unwrap();
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

// U-18 批 T1.5（REQ-01）：route_timeouts 三形态命中 busy handler → 408。
// 全局 timeout 传 None：证明生效的是路由覆盖而非全局（粒度 ≥25ms 看门狗 tick）。
async fn route_timeout_shape_hits(shape: &str) -> bool {
    let t = routes(&[(
        "u/f/api.ts",
        "export default { get() { while (true) {} } };",
    )]);
    let pl = Pipeline {
        route_timeouts: vec![(shape.to_string(), std::time::Duration::from_millis(200))],
        ..Pipeline::default()
    };
    let addr = spawn_pipeline("/v1/api", t.0.clone(), true, None, pl).await;
    let resp = raw_http(
        addr,
        "GET /v1/api/u/f HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n",
    )
    .await;
    resp.starts_with("HTTP/1.1 408")
}

#[tokio::test]
async fn route_timeout_literal_with_base_hits_408() {
    assert!(route_timeout_shape_hits("/v1/api/u/f").await);
}

#[tokio::test]
async fn route_timeout_tail_one_segment_hits_408() {
    assert!(route_timeout_shape_hits("/v1/api/u/*").await);
}

#[tokio::test]
async fn route_timeout_multi_segment_hits_408() {
    assert!(route_timeout_shape_hits("/**").await);
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
        caps: None,
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
/// 根 `/` 站；`spa` 即挂载显式 `spa: true`（默认 false）。
async fn spawn_static(
    api: &[(&str, &str)],
    spa: bool,
    site: &[(&str, &str)],
    opts: StaticOpts,
) -> (std::net::SocketAddr, (TempRoutes, TempRoutes)) {
    let (addr, (t, keeps)) = spawn_static_sites(api, &[("/", spa, site.to_vec())], opts).await;
    (addr, (t, keeps.into_iter().next().unwrap()))
}

/// 静态站夹具条目：(prefix, spa, files)。
type SiteFixture<'a> = (&'a str, bool, Vec<(&'a str, &'a str)>);

/// 多站点版（v0.1.27 语义沿用 v0.1.58 挂载模型）：sites = [(prefix, files, spa)]。
/// 另挂一条空 api 挂载 /v1/api（表空，仅为最长前缀路由提供 api 面）。
async fn spawn_static_sites(
    api: &[(&str, &str)],
    sites: &[SiteFixture<'_>],
    opts: StaticOpts,
) -> (std::net::SocketAddr, (TempRoutes, Vec<TempRoutes>)) {
    let t = routes(api);
    let keeps: Vec<TempRoutes> = sites.iter().map(|(_, _, f)| routes(f)).collect();
    let mut mounts: Vec<RuntimeMount> = sites
        .iter()
        .zip(&keeps)
        .map(|((prefix, spa, _), k)| {
            RuntimeMount::Web(WebMount {
                prefix: prefix.to_string(),
                root: k.0.clone(),
                headers: Vec::new(),
                spa: *spa,
            })
        })
        .collect();
    let table = build_table(&t.0, true, "/v1/api");
    mounts.push(RuntimeMount::Api {
        prefix: "/v1/api".to_string(),
        table,
        fallback: Some(Routes::new("/v1/api", t.0.clone(), true)),
        web: None,
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let dir = t.0.clone();
    tokio::spawn(async move {
        serve_with_listener(
            listener,
            mounts,
            make_actor(dir, true),
            None,
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
        false,
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
        true,
        &[("index.html", "<html><head></head><body>app</body></html>")],
        StaticOpts::default(),
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
        spawn_static(&[], false, &[("index.html", "app")], StaticOpts::default()).await;
    let r = raw_http(addr, &get(addr, "/space/issues/abc")).await;
    assert!(r.starts_with("HTTP/1.1 404"), "{r}");
}

/// meta 注入：按路径查 `__meta/<path>.json` 注入 title/og；值一律转义；
/// 该目录本身不对静态服务公开。
#[tokio::test]
async fn html_meta_injects_escaped_tags_and_hides_meta_dir() {
    let (addr, _keep) = spawn_static(
            &[],
            true,
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
        true,
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
        true,
        &[("index.html", "<html><head></head><body>hi</body></html>")],
        StaticOpts::default(),
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
        true,
        &[("index.html", "<html><head></head><body>hi</body></html>")],
        StaticOpts {
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
        true,
        &[("index.html", "x"), ("assets/app.js", "console.log(1)")],
        StaticOpts {
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
    let out = inject_head(html.to_string(), &meta(&[("title", "Dyn & Co")]));
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
        html.to_string(),
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
    let out = inject_head(html.to_string(), &meta(&[("title", "Real")]));
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
    let html = "<html><head><meta charset=\"utf-8\"><!-- İstanbul --></head><body>ş</body></html>";
    let out = inject_head(html.to_string(), &meta(&[("title", "Türkçe")]));
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
    assert_eq!(
        inject_head(no_head.to_string(), &meta(&[("title", "T")])),
        no_head
    );
    let html = "<html><head><title>s</title></head></html>";
    assert_eq!(inject_head(html.to_string(), &serde_json::Map::new()), html);
    // 只给非白名单键（渲染不出东西）→ 不得摘任何标签
    assert_eq!(
        inject_head(html.to_string(), &meta(&[("foo", "bar")])),
        html
    );
}

#[tokio::test]
async fn static_guards_traversal_missing_and_verbs() {
    let (addr, _keep) =
        spawn_static(&[], false, &[("index.html", "x")], StaticOpts::default()).await;
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
    // 非 GET/HEAD 不走静态（规则 4：显式 405，v0.1.58 前落入 404）
    let r = raw_http(
        addr,
        "POST / HTTP/1.1\r\nHost: t\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(r.starts_with("HTTP/1.1 405"), "{r}");
}

// ----- 多静态站点（v0.1.27：prefix→dir，最长前缀命中）-----

/// 双站点夹具：`/docs` → docs 根（含嵌套前缀用例文件），`/` → app 根（spa 取 `root_spa`）。
async fn spawn_two_sites(
    root_spa: bool,
    opts: StaticOpts,
) -> (std::net::SocketAddr, (TempRoutes, Vec<TempRoutes>)) {
    spawn_static_sites(
        &[],
        &[
            (
                "/",
                root_spa,
                vec![
                    ("index.html", "<h1>app</h1>"),
                    ("docs/deep.txt", "APP-ROOT-SHOULD-NOT-SERVE"),
                ],
            ),
            (
                "/docs",
                false,
                vec![("index.html", "<h1>docs</h1>"), ("api/x.txt", "DOCS-API-X")],
            ),
        ],
        opts,
    )
    .await
}

#[tokio::test]
async fn multi_static_longest_prefix_wins_and_root_catchall() {
    let (addr, _keep) = spawn_two_sites(false, StaticOpts::default()).await;
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
    let (addr, _keep) = spawn_two_sites(false, StaticOpts::default()).await;
    let r = raw_http(addr, &get(addr, "/docs/deep.txt")).await;
    assert!(r.starts_with("HTTP/1.1 404"), "{r}");
}

#[tokio::test]
async fn multi_static_spa_fallback_is_per_site() {
    // v0.1.58：spa 是**挂载级**显式开关（`spa: true`，默认 false）——/docs 开、
    // `/` 关：/docs 深链接 200；`/` 深链接 404（不跨站借）。
    let (addr, _keep) = spawn_static_sites(
        &[],
        &[
            ("/", false, vec![("docs/deep.txt", "SHOULD-NOT-REACH")]),
            ("/docs", true, vec![("index.html", "<h1>docs</h1>")]),
        ],
        StaticOpts::default(),
    )
    .await;
    let accept =
        "GET /docs/deep/link HTTP/1.1\r\nHost: t\r\nAccept: text/html\r\nConnection: close\r\n\r\n";
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
                false,
                vec![("index.html", "<html><head></head><body>app</body></html>")],
            ),
            (
                "/docs",
                false,
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
    let payload = general_purpose::URL_SAFE_NO_PAD.encode(r#"{"nbf":1000000000,"exp":2000000000}"#);
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

// ---------- 补覆盖：serve_with_listener（直接喂挂载） / blob 302 重定向 / mime_of ----------

/// Given: 已绑定 listener + api 挂载；When: serve_with_listener 收 GET；Then: 200 信封回包
/// （v0.1.58 起 bind 归调用方——`oj serve` 走 `App::serve_graceful`，本 crate 无 bind 版入口）。
#[tokio::test]
async fn given_listener_with_mounts_when_serving_then_requests_answered() {
    let t = routes(&[(
        "u/s/api.ts",
        "export default { get() { json.ok({ ok: 1 }); } };",
    )]);
    let dir = t.0.clone();
    let table = build_table(&dir, true, "/v1/api");
    let actor = make_actor(dir.clone(), true);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mounts = vec![RuntimeMount::Api {
        prefix: "/v1/api".to_string(),
        table,
        fallback: Some(Routes::new("/v1/api", dir, true)),
        web: None,
    }];
    let server = tokio::spawn(serve_with_listener(
        listener,
        mounts,
        actor,
        None,
        StaticOpts::default(),
        Pipeline::default(),
        None,
    ));
    // 有界轮询等 bind 完成（spawn 与本测试同一 current_thread 运行时，await 期间被驱动）。
    let mut bound = false;
    for _ in 0..200 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            bound = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(bound, "server did not bind {addr}");
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
}

/// Given: serve 恒 302 的 blob 后端（模拟 s3 presign 直链）；When: GET {base}/blob/k；
/// Then: SEE_OTHER(303) + Location 直链（重定向分支，local 后端永远走不到）。
struct RedirectBlob;
#[async_trait::async_trait]
impl BlobBackend for RedirectBlob {
    async fn put(&self, _: &str, _: &[u8], _: Option<&str>) -> only_js::bridge::BridgeResult<()> {
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
    let mounts = vec![RuntimeMount::Api {
        prefix: base.clone(),
        table,
        fallback: ts.then(|| Routes::new(&base, dir.clone(), ts)),
        web: None,
    }];
    tokio::spawn(async move {
        serve_with_listener(
            listener,
            mounts,
            make_actor(dir, ts),
            None,
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
