//! oj-4 e2e（L1）：cookie 会话形态 + CSRF 双提交 + WS 握手鉴权。
//!
//! 独立测试二进制的原因同 e2e.rs L1 子进程注释：oj-auth 插件单例（GUARD OnceLock）
//! 首次 init 后忽略后续 cfg——本文件只 boot 一次（单用例串行断言全程），
//! 不与 e2e.rs 的 boot(cfg.auth=None) 互相污染。
//!
//! 前置：`cargo xtask plugin auth` 已把 libauth.dylib 归置 bin/plugins/<triple>/
//! （server_cmd 默认发现路径）。真实插件装配 = 生产语义。

use std::path::{Path, PathBuf};

use oj::server_cmd;
use only_js::config::Config;

fn tmp_project(files: &[(&str, &str)]) -> PathBuf {
    let t = std::env::temp_dir().join(format!("oj-cookie-{}", std::process::id()));
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
    let n = server::test_support::now_secs();
    server::test_support::write_cert_into(
        &mut cfg.server,
        dir,
        n.saturating_sub(3600),
        n + 365 * 86_400,
    );
    cfg.db.insert("default".into(), "sqlite::memory:".into());
    cfg
}

fn sign(sub: &str, secret: &[u8]) -> String {
    let now = server::test_support::now_secs();
    let claims = serde_json::json!({
        "sub": sub, "roles": ["editor"], "iat": now, "exp": now + 3600,
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
    body: Option<&str>,
    headers: &[(&str, &str)],
) -> (u16, serde_json::Value) {
    let c = reqwest::Client::new();
    let mut r = c.request(
        reqwest::Method::from_bytes(method.as_bytes()).unwrap(),
        format!("http://{addr}{path}"),
    );
    if let Some(b) = body {
        r = r
            .header("content-type", "application/json")
            .body(b.to_string());
    }
    for (k, v) in headers {
        r = r.header(*k, *v);
    }
    let resp = r.send().await.unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap())
}

/// 原始 WS 握手：返回 HTTP 状态码（101 = 升级成功，401 = 守卫拒绝）。
async fn ws_handshake(addr: std::net::SocketAddr, path: &str, headers: &[(&str, &str)]) -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut req = format!(
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n"
    );
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    let mut buf = vec![0u8; 256];
    let n = s.read(&mut buf).await.unwrap();
    String::from_utf8_lossy(&buf[..n])
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0)
}

#[tokio::test(flavor = "current_thread")]
async fn cookie_session_and_ws_handshake_auth_end_to_end() {
    let t = tmp_project(&[
        (
            "src/health/manifest.yaml",
            "name: health\ndesc: d\nversion: 0.1.0\n",
        ),
        (
            "src/health/api.ts",
            "export default { get() { json.ok({ up: true }); } };\n",
        ),
        (
            "src/me/manifest.yaml",
            "name: me\ndesc: d\nversion: 0.1.0\n",
        ),
        (
            "src/me/api.ts",
            "export default {\n\
             \x20 get() { json.ok({ user: http.user }); },\n\
             \x20 post() { json.ok({ user: http.user }); },\n\
             };\n",
        ),
        (
            "src/priv/manifest.yaml",
            "name: priv\ndesc: d\nversion: 0.1.0\n",
        ),
        (
            "src/priv/ws.ts",
            "export default {\n\
             \x20 connection() { json.ok({ hello: true }); },\n\
             };\n",
        ),
    ]);
    let mut cfg = base_cfg(&t);
    // 严格清单只装 oj-auth：避免扫描模式把 bin/plugins 下其它 ABI 滞后的插件拉进来。
    cfg.plugins
        .insert("auth".into(), serde_json::Value::Object(Default::default()));
    cfg.auth = Some(only_js::config::AuthCfg {
        jwt_secret: "cookie-e2e-secret".into(),
        anonymous_paths: vec![
            only_js::config::AnonPath::Plain("/health".into()),
            only_js::config::AnonPath::Plain("/auth/login".into()),
        ],
        cookie: Some(serde_json::json!({"enabled": true})),
        ..Default::default()
    });
    let (addr, _h) = server_cmd::start(cfg, &t, t.join("src"), "/v1/api".into(), true)
        .await
        .unwrap();
    let secret = b"cookie-e2e-secret";
    let sess = sign("u1", secret);

    // 匿名路径无凭证放行。
    let (s, v) = req(addr, "GET", "/v1/api/health/", None, &[]).await;
    assert_eq!(s, 200, "{v}");

    // 受保护路径无凭证 → 401。
    let (s, v) = req(addr, "GET", "/v1/api/me/", None, &[]).await;
    assert_eq!(s, 401, "{v}");

    // cookie 会话：GET 带 oj_sess → 200 + http.user 注入。
    let (s, v) = req(
        addr,
        "GET",
        "/v1/api/me/",
        None,
        &[("Cookie", &format!("oj_sess={sess}"))],
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["data"]["user"]["id"], "u1", "{v}");

    // POST 带 cookie 无 CSRF 头 → 401（透出 csrf 文案）。
    let (s, v) = req(
        addr,
        "POST",
        "/v1/api/me/",
        Some("{}"),
        &[("Cookie", &format!("oj_sess={sess}; oj_csrf=tok1"))],
    )
    .await;
    assert_eq!(s, 401, "{v}");
    assert_eq!(v["msg"], "missing or invalid csrf token", "{v}");

    // 补 CSRF 头（值 == oj_csrf cookie）→ 200。
    let (s, v) = req(
        addr,
        "POST",
        "/v1/api/me/",
        Some("{}"),
        &[
            ("Cookie", &format!("oj_sess={sess}; oj_csrf=tok1")),
            ("x-csrf-token", "tok1"),
        ],
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["data"]["user"]["id"], "u1", "{v}");

    // Bearer 依旧工作且不查 CSRF。
    let (s, v) = req(
        addr,
        "POST",
        "/v1/api/me/",
        Some("{}"),
        &[("Authorization", &format!("Bearer {sess}"))],
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["data"]["user"]["id"], "u1", "{v}");

    // WS 握手鉴权：无 cookie → 401；带 session cookie → 101 升级。
    let s = ws_handshake(addr, "/v1/api/priv/ws", &[]).await;
    assert_eq!(s, 401, "ws without credentials must be rejected");
    let s = ws_handshake(
        addr,
        "/v1/api/priv/ws",
        &[("Cookie", &format!("oj_sess={sess}"))],
    )
    .await;
    assert_eq!(s, 101, "ws with cookie session must upgrade");

    let _ = std::fs::remove_dir_all(&t);
}
