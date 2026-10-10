//! oj-5/7/8 e2e（L1）：blob 直传 PUT + 路由级 timeout + Range + 自定义响应头。
//!
//! 独立测试二进制的原因同 e2e_cookie.rs：oj-auth 插件单例（GUARD OnceLock）
//! 首次 init 后忽略后续 cfg——本文件只 boot 一次（单用例串行断言全程）。
//!
//! 前置：`cargo xtask plugin auth` 已把 libauth.dylib 归置 bin/plugins/<triple>/。

use std::path::{Path, PathBuf};

use oj::serve_cmd;
use only_js::config::Config;

fn tmp_project(files: &[(&str, &str)]) -> PathBuf {
    let t = std::env::temp_dir().join(format!("oj-upload-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&t);
    std::fs::create_dir_all(&t).unwrap();
    for (rel, c) in files {
        let p = t.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, c).unwrap();
    }
    t
}

fn base_cfg(dir: &Path) -> Config {
    let mut cfg = Config::default();
    cfg.server.port = 0;
    let n = serve::test_support::now_secs();
    serve::test_support::write_cert_into(
        &mut cfg.server,
        dir,
        n.saturating_sub(3600),
        n + 365 * 86_400,
    );
    cfg.db.insert("default".into(), "sqlite::memory:".into());
    cfg
}

fn sign(sub: &str, secret: &[u8]) -> String {
    let now = serve::test_support::now_secs();
    let claims = serde_json::json!({
        "sub": sub, "roles": [], "iat": now, "exp": now + 3600,
    });
    jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(secret),
    )
    .unwrap()
}

async fn req(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    body: Option<Vec<u8>>,
    headers: &[(&str, &str)],
) -> reqwest::Response {
    let c = reqwest::Client::new();
    let mut r = c.request(
        reqwest::Method::from_bytes(method.as_bytes()).unwrap(),
        format!("http://{addr}{path}"),
    );
    if let Some(b) = body {
        r = r.body(b);
    }
    for (k, v) in headers {
        r = r.header(*k, *v);
    }
    r.send().await.unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn upload_route_timeout_range_and_custom_headers_end_to_end() {
    let t = tmp_project(&[
        (
            "src/ping/manifest.yaml",
            "name: ping\ndesc: d\nversion: 0.1.0\n",
        ),
        (
            "src/ping/api.ts",
            "export default { get() { json.ok({ pong: true }); } };\n",
        ),
        (
            "src/slow/manifest.yaml",
            "name: slow\ndesc: d\nversion: 0.1.0\n",
        ),
        (
            "src/slow/api.ts",
            "export default { get() { for (;;) {} } };\n",
        ),
        (
            "src/upurl/manifest.yaml",
            "name: upurl\ndesc: d\nversion: 0.1.0\n",
        ),
        (
            "src/upurl/api.ts",
            "export default {\n\
             \x20 get() {\n\
             \x20   blob.uploadUrl('a/f.bin')\n\
             \x20     .then((u) => json.ok({ url: u }))\n\
             \x20     .catch((e) => json.ok({ err: String(e) }));\n\
             \x20 },\n\
             };\n",
        ),
        ("web/index.html", "<html><body>app</body></html>\n"),
        ("docs/index.html", "<html><body>docs</body></html>\n"),
        ("blob-root/.keep", ""),
    ]);
    let mut cfg = base_cfg(&t);
    // 直传上限压到 256B：200B 上传成功、300B 413（默认 1 GiB 的档位行为同路径）。
    cfg.server.blob_upload_max_bytes = 256;
    // oj-8：全局自定义响应头 + per-site 覆盖。
    cfg.server
        .response_headers
        .insert("x-frame-options".into(), "DENY".into());
    cfg.server
        .response_headers
        .insert("x-oj-e2e".into(), "global".into());
    cfg.mounts.push(only_js::config::MountConf {
        prefix: "/docs".into(),
        api: None,
        web: Some(t.join("docs").to_string_lossy().into()),
        spa: None,
        headers: std::collections::BTreeMap::from([("x-oj-e2e".into(), "docs".into())]),
    });
    // oj-5c：slow 路由 1s 超时（全局 30s 不可能等到——408 必是覆盖生效）。
    cfg.server
        .route_timeouts
        .push(only_js::config::RouteTimeoutConf {
            pattern: "/v1/api/slow/**".into(),
            timeout: "1s".into(),
        });
    cfg.mounts.push(only_js::config::MountConf {
        prefix: "/".into(),
        api: None,
        web: Some(t.join("web").to_string_lossy().into()),
        spa: None,
        headers: Default::default(),
    });
    cfg.blob = Some(only_js::config::BlobSection {
        driver: "local".into(),
        root: t.join("blob-root").to_string_lossy().into(),
        ..Default::default()
    });
    // 严格清单只装 oj-auth：上传 401 臂需要真实守卫。
    cfg.plugins
        .insert("auth".into(), serde_json::Value::Object(Default::default()));
    cfg.auth = Some(only_js::config::AuthCfg {
        jwt_secret: "upload-e2e-secret".into(),
        anonymous_paths: vec![
            only_js::config::AnonPath::Plain("/ping".into()),
            only_js::config::AnonPath::Plain("/slow".into()),
            only_js::config::AnonPath::Plain("/upurl".into()),
        ],
        ..Default::default()
    });
    let (addr, _h) = serve_cmd::start(cfg, &t, t.join("src"), "/v1/api".into())
        .await
        .unwrap();
    let secret = b"upload-e2e-secret";
    let bearer = format!("Bearer {}", sign("u1", secret));
    let file: Vec<u8> = (0..200u8).map(|i| i.wrapping_mul(7)).collect();

    // ---- oj-5b：直传 PUT ----
    // 未授权 → 401（写面过守卫；GET 公开读语义不受影响）。
    let r = req(addr, "PUT", "/v1/api/blob/a/f.bin", Some(file.clone()), &[]).await;
    assert_eq!(
        r.status(),
        401,
        "upload without credentials must be rejected"
    );

    // 超限 → 413 信封（300B > 256B 档）。
    let big: Vec<u8> = vec![9u8; 300];
    let r = req(
        addr,
        "PUT",
        "/v1/api/blob/a/big.bin",
        Some(big),
        &[("Authorization", &bearer)],
    )
    .await;
    assert_eq!(r.status(), 413, "over-limit upload must be 413");
    let v: serde_json::Value = r.json().await.unwrap();
    assert_eq!(v["msg"], "upload too large", "{v}");

    // 正常上传 → 200；GET 回读字节一致（含 Accept-Ranges）。
    let r = req(
        addr,
        "PUT",
        "/v1/api/blob/a/f.bin",
        Some(file.clone()),
        &[
            ("Authorization", &bearer),
            ("Content-Type", "application/octet-stream"),
        ],
    )
    .await;
    assert_eq!(r.status(), 200, "direct PUT must succeed");
    let v: serde_json::Value = r.json().await.unwrap();
    assert_eq!(v["code"], 0, "{v}");

    let r = req(addr, "GET", "/v1/api/blob/a/f.bin", None, &[]).await;
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.headers()["accept-ranges"],
        "bytes",
        "blob GET must advertise Accept-Ranges"
    );
    assert_eq!(
        r.bytes().await.unwrap().as_ref(),
        file.as_slice(),
        "readback bytes must match"
    );

    // ---- oj-7：Range ----
    let r = req(
        addr,
        "GET",
        "/v1/api/blob/a/f.bin",
        None,
        &[("Range", "bytes=0-99")],
    )
    .await;
    assert_eq!(r.status(), 206, "single range must be 206");
    assert_eq!(
        r.headers()["content-range"],
        "bytes 0-99/200",
        "{:?}",
        r.headers()
    );
    assert_eq!(r.bytes().await.unwrap().len(), 100);

    let r = req(
        addr,
        "GET",
        "/v1/api/blob/a/f.bin",
        None,
        &[("Range", "bytes=150-")],
    )
    .await;
    assert_eq!(r.status(), 206);
    assert_eq!(r.headers()["content-range"], "bytes 150-199/200");
    assert_eq!(r.bytes().await.unwrap().as_ref(), &file[150..]);

    let r = req(
        addr,
        "GET",
        "/v1/api/blob/a/f.bin",
        None,
        &[("Range", "bytes=-50")],
    )
    .await;
    assert_eq!(r.status(), 206);
    assert_eq!(r.headers()["content-range"], "bytes 150-199/200");
    assert_eq!(r.bytes().await.unwrap().as_ref(), &file[150..]);

    let r = req(
        addr,
        "GET",
        "/v1/api/blob/a/f.bin",
        None,
        &[("Range", "bytes=500-")],
    )
    .await;
    assert_eq!(r.status(), 416, "out-of-range must be 416");
    assert_eq!(r.headers()["content-range"], "bytes */200");

    // ---- oj-5a：blob.uploadUrl 在 local 后端抛错（指路直传路由）----
    let r = req(addr, "GET", "/v1/api/upurl/", None, &[]).await;
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = r.json().await.unwrap();
    let err = v["data"]["err"].as_str().unwrap_or_default();
    assert!(
        err.contains("local blob backend has no upload presign")
            && err.contains("direct PUT upload route"),
        "{err}"
    );

    // ---- oj-5c：路由级 timeout ----
    let r = req(addr, "GET", "/v1/api/slow/", None, &[]).await;
    assert_eq!(r.status(), 408, "route timeout override must yield 408");
    // 未命中路由不受覆盖影响（ping 正常）。
    let r = req(addr, "GET", "/v1/api/ping/", None, &[]).await;
    assert_eq!(r.status(), 200);

    // ---- oj-8：自定义响应头 ----
    // 动态信封响应：全局头补缺。
    let r = req(addr, "GET", "/v1/api/ping/", None, &[]).await;
    assert_eq!(r.headers()["x-frame-options"], "DENY");
    assert_eq!(r.headers()["x-oj-e2e"], "global");
    // blob 响应：同全局头。
    let r = req(addr, "GET", "/v1/api/blob/a/f.bin", None, &[]).await;
    assert_eq!(r.headers()["x-frame-options"], "DENY");
    // 主静态站点：全局头。
    let r = req(addr, "GET", "/index.html", None, &[]).await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-frame-options"], "DENY");
    assert_eq!(r.headers()["x-oj-e2e"], "global");
    // per-site 覆盖：/docs 站点的 x-oj-e2e 换成 docs，全局 x-frame-options 保留。
    let r = req(addr, "GET", "/docs/index.html", None, &[]).await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-oj-e2e"], "docs");
    assert_eq!(r.headers()["x-frame-options"], "DENY");
    // 框架自有头优先：动态响应的 content-type 是 application/json，不被覆盖（无配置它）。
    let r = req(addr, "GET", "/v1/api/ping/", None, &[]).await;
    assert!(
        r.headers()["content-type"]
            .to_str()
            .unwrap()
            .contains("application/json")
    );

    let _ = std::fs::remove_dir_all(&t);
}
