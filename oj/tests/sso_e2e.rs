//! HTTP 表单 / CAS SSO 全链路 E2E：真 server + 真 TCP + fetch 回环（mock 上游 = 项目内
//! 业务模块 mocksso）。
//!
//! 单独测试目标的原因：oj-auth 插件的进程级 `static GUARD: OnceLock` 只认首次 init 的
//! 配置（anonymous_paths/jwt_secret/cookie），与 e2e.rs 共进程会互相污染（oidc_e2e.rs
//! 同款隔离）。本目标的**两个用例共用完全相同的 auth cfg**（含 cookie.enabled）——
//! 守卫钉死的是首次 init 的那份，两用例一致才安全。

// lock() 的 std MutexGuard 有意全程持有（横跨 await 是设计而非疏漏）。
#![allow(clippy::await_holding_lock)]

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use oj::serve_cmd;
use only_js::config::{AnonPath, AuthCfg, Config, HttpSsoProvider};

fn lock() -> MutexGuard<'static, ()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn sample() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../sample")
        .canonicalize()
        .unwrap()
}

/// 临时项目：config_dir 用绝对路径（钳制要求 project_root ⊇ 模块目录）。
fn tmp_project(files: &[(&str, &str)]) -> PathBuf {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let t = std::env::temp_dir().join(format!(
        "oj-sso-{}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
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

fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for e in std::fs::read_dir(src).unwrap().flatten() {
        let d = dst.join(e.file_name());
        if e.path().is_dir() {
            copy_dir(&e.path(), &d);
        } else {
            std::fs::copy(e.path(), &d).unwrap();
        }
    }
}

fn plugins_dir() -> Option<PathBuf> {
    ["../bin/plugins", "../../../../bin/plugins"]
        .iter()
        .map(|p| Path::new(env!("CARGO_MANIFEST_DIR")).join(p))
        .find(|p| p.is_dir())
}

/// mock 上游（企业 SSO 伪装端点，匿名业务模块）：
/// POST /mocksso/ → form 判定（alice 成功，message 为空串；其余失败）；
/// GET  /mocksso/?ticket=… → cas2 XML（ST-ok 成功，其余 failure）。
const MOCKSSO_API: &str = r#"export default {
  post() {
    const ok = String(http.body?.username ?? "") === "alice";
    json.raw(JSON.stringify(ok
      ? { message: "", data: { name: "Alice", email: "a@x" } }
      : { message: "bad credentials" }));
  },
  get() {
    json.raw(String(http.query.ticket ?? "") === "ST-ok"
      ? "<cas:serviceResponse>\n  <cas:authenticationSuccess>\n    <cas:user>alice</cas:user>\n  </cas:authenticationSuccess>\n</cas:serviceResponse>"
      : "<cas:serviceResponse><cas:authenticationFailure code=\"INVALID_TICKET\">no</cas:authenticationFailure></cas:serviceResponse>");
  },
};
"#;

/// 组起服务：auth/sso/_platform/mocksso/me 五模块 + 两用例**完全相同**的 auth cfg。
/// port 由调用方预探并复用（mock 上游 URL 是配置期字符串，必须与实际绑定端口一致）。
async fn start_with(port: u16, prov: HttpSsoProvider) -> (String, PathBuf) {
    let t = tmp_project(&[
        (
            "src/mocksso/manifest.yaml",
            "name: mocksso\ndesc: mock upstream\nversion: 0.1.0\n",
        ),
        // 文件路由 /mocksso/up（无尾斜杠）：`/mocksso/*` 严格一层稳定命中——目录镜像根路由
        // `/mocksso/` 会因尾斜杠归一后只剩一段而落守卫。
        ("src/mocksso/up/api.ts", MOCKSSO_API),
        (
            "src/me/manifest.yaml",
            "name: me\ndesc: probe\nversion: 0.1.0\n",
        ),
        (
            "src/me/api.ts",
            "export default { get() { json.ok({ u: http.user }); } };\n",
        ),
    ]);
    let src = t.join("src");
    for m in ["auth", "sso"] {
        copy_dir(&sample().join("src").join(m), &src.join(m));
    }
    // _platform（users 表；JIT 建号落这里；schema.yaml 供 db.table 白名单）。
    std::fs::create_dir_all(src.join("_platform")).unwrap();
    std::fs::write(
        src.join("_platform/manifest.yaml"),
        "name: _platform\ndesc: users\nversion: 0.1.0\ntables:\n  - users\n",
    )
    .unwrap();
    std::fs::write(
        src.join("_platform/schema.yaml"),
        "tables:\n  users:\n    pk: id\n    columns:\n      id: { type: integer, autoincrement: true }\n      username: { type: text, null: false }\n      password_hash: { type: text, null: false }\n      name: { type: text }\n      roles: { type: text, null: false }\n",
    )
    .unwrap();
    std::fs::write(
        src.join("_platform/seed.sql"),
        "CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY AUTOINCREMENT, \
         username TEXT NOT NULL UNIQUE, password_hash TEXT NOT NULL, name TEXT, roles TEXT NOT NULL DEFAULT '[]');\n",
    )
    .unwrap();
    let mut cfg = base_cfg(&t);
    cfg.server.host = "127.0.0.1".into();
    cfg.server.port = port;
    cfg.auth = Some(AuthCfg {
        jwt_secret: "e2e".into(),
        // 两用例一致（守卫 OnceLock 钉死首份）：登录端点 + CAS 302 腿 + mock 上游全匿名。
        anonymous_paths: vec![
            AnonPath::Plain("/sso/login".into()),
            AnonPath::Detailed {
                path: "/sso/cas/*".into(),
                one_layer: true,
            },
            AnonPath::Plain("/mocksso/*".into()),
        ],
        // cookie 会话开（CAS 302 腿靠 httpOnly 双 cookie 交接，token 不进 URL）。
        cookie: Some(serde_json::json!({ "enabled": true })),
        ..Default::default()
    });
    cfg.http_sso = Some(std::collections::HashMap::from([("corp".into(), prov)]));
    cfg.plugins_dir = plugins_dir();
    cfg.plugins.insert("auth".into(), serde_json::json!({}));
    let (addr, _h) = serve_cmd::start(cfg, &t, src, "/v1/api".into())
        .await
        .unwrap();
    (format!("http://{addr}/v1/api"), t)
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .unwrap()
}

/// form：凭据转发 → success_when 判定 → JIT 建号 → 签发（Bearer + cookie 双面可用）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sso_form_login_jits_and_issues_session() {
    let _g = lock();
    let port_probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = port_probe.local_addr().unwrap().port();
    drop(port_probe);
    let prov = HttpSsoProvider {
        kind: "form".into(),
        login_url: format!("http://127.0.0.1:{port}/v1/api/mocksso/up/"),
        // mock 端读 http.body JSON（urlencoded 只解析 query 不解析 body——见 serve 解析层）
        body_encoding: "json".into(),
        success_when: std::collections::HashMap::from([(
            "message".to_string(),
            serde_json::json!(""),
        )]),
        profile: std::collections::HashMap::from([("name".to_string(), "data.name".to_string())]),
        allow_http: true, // 回环 mock
        ..Default::default()
    };
    let (base, t) = start_with(port, prov).await;
    let http = http_client();
    // ⓪ mock 直打先绿（隔离「上游被守卫挡了」与「判定逻辑错」两类失败）。
    let r0 = http
        .post(format!("{base}/mocksso/up"))
        .json(&serde_json::json!({"username": "alice"}))
        .send()
        .await
        .unwrap();
    let (s0, b0) = (r0.status(), r0.text().await.unwrap());
    assert_eq!(s0, 200, "mock direct: {b0}");
    // ① 登录成功 → 200 + access_token + oj_sess cookie。
    let r1 = http
        .post(format!("{base}/sso/login/"))
        .json(&serde_json::json!({"provider": "corp", "username": "alice", "password": "x"}))
        .send()
        .await
        .unwrap();
    let s1 = r1.status();
    let b1: serde_json::Value = r1.json().await.unwrap();
    assert_eq!(s1, 200, "{b1}");
    assert_eq!(b1["code"], 0, "{b1}");
    let access = b1["data"]["access_token"].as_str().unwrap().to_string();
    // ② Bearer 打受保护路由：http.user 是 JIT 建的号（seed 无行 → id 1）。
    let r2 = http
        .get(format!("{base}/me/"))
        .bearer_auth(&access)
        .send()
        .await
        .unwrap();
    let v2: serde_json::Value = r2.json().await.unwrap();
    assert_eq!(v2["data"]["u"]["id"], "1", "{v2}");
    // ③ 错误凭据 → 401（不泄露上游响应体）。
    let r3 = http
        .post(format!("{base}/sso/login/"))
        .json(&serde_json::json!({"provider": "corp", "username": "bob", "password": "x"}))
        .send()
        .await
        .unwrap();
    let (s3, b3) = (r3.status(), r3.text().await.unwrap());
    assert_eq!(s3, 401, "{b3}");
    assert!(b3.contains("invalid credentials"), "{b3}");
    // ④ provider 未命中已配置名单 → 400（防自报命名空间）。
    let r4 = http
        .post(format!("{base}/sso/login/"))
        .json(&serde_json::json!({"provider": "nope", "username": "alice", "password": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r4.status(), 400);
    let _ = std::fs::remove_dir_all(&t);
}

/// cas2：ticket 回校 XML 判定 → JIT → 双 cookie 交接（token 不进 URL）→ 302。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sso_cas2_callback_validates_ticket() {
    let _g = lock();
    let port_probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = port_probe.local_addr().unwrap().port();
    drop(port_probe);
    let prov = HttpSsoProvider {
        kind: "cas2".into(),
        login_url: format!("http://127.0.0.1:{port}/v1/api/mocksso/up?leg=caslogin"),
        cas_validate_url: format!("http://127.0.0.1:{port}/v1/api/mocksso/up/"),
        callback_url: format!("http://127.0.0.1:{port}/v1/api/sso/cas/callback/?provider=corp"),
        allow_http: true,
        ..Default::default()
    };
    let (base, t) = start_with(port, prov).await;
    let http = http_client();
    // ① 好票 → 302（回 safeNext 缺省 "/"）+ oj_sess cookie。
    let r1 = http
        .get(format!(
            "{base}/sso/cas/callback/?provider=corp&ticket=ST-ok"
        ))
        .send()
        .await
        .unwrap();
    let s1 = r1.status();
    let loc1 = r1
        .headers()
        .get("location")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    let cookie_hdr = r1
        .headers()
        .get("set-cookie")
        .map(|v| v.to_str().unwrap().to_string());
    let b1 = r1.text().await.unwrap_or_default();
    assert_eq!(s1, 302, "{b1}");
    assert_eq!(loc1, "/", "缺省回 /");
    let cookie = cookie_hdr.unwrap_or_else(|| panic!("no set-cookie, body={b1}"));
    assert!(cookie.starts_with("oj_sess="), "{cookie}");
    let sess = cookie.split(';').next().unwrap().to_string();
    // ② cookie 会话打受保护路由：JIT 号（GET 不查 CSRF）。
    let r2 = http
        .get(format!("{base}/me/"))
        .header("cookie", &sess)
        .send()
        .await
        .unwrap();
    let v2: serde_json::Value = r2.json().await.unwrap();
    assert_eq!(v2["data"]["u"]["id"], "1", "{v2}");
    // ③ 坏票 → 401。
    let r3 = http
        .get(format!(
            "{base}/sso/cas/callback/?provider=corp&ticket=ST-bad"
        ))
        .send()
        .await
        .unwrap();
    let (s3, b3) = (r3.status(), r3.text().await.unwrap());
    assert_eq!(s3, 401, "{b3}");
    assert!(b3.contains("invalid credentials"), "{b3}");
    let _ = std::fs::remove_dir_all(&t);
}
