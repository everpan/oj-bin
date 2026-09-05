//! OIDC 全链路 E2E：真 server + 真 TCP + fetch 回环。
//!
//! 单独测试目标的原因：oj-auth 插件的进程级 `static GUARD: OnceLock` 只认首次
//! init 的配置（anonymous_paths/jwt_secret），与 e2e.rs 共进程会互相污染（先启动
//! 的用例把 `/health` 钉进守卫，后续用例的 `/oidc/*` 豁免被丢弃 → 401）。
//! 独立目标 = 独立进程 = 独立 dlopen = 独立 GUARD。辅助函数为隔离刻意与
//! e2e.rs 重复（集成测试不共享代码，无 common module）。

// lock() 的 std MutexGuard 有意全程持有（横跨 await 是设计而非疏漏，本目标仅 1 用例）。
#![allow(clippy::await_holding_lock)]

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use oj::server_cmd;
use only_js::config::{AuthCfg, Config, OidcClientCfg, OidcRpCfg, OidcSection};

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
        "oj-oidc-{}-{}",
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

/// 最小可用配置（port 0 随机端口；default 内存库；证书必配 → 在项目目录生成真实证书）。
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

/// 递归拷贝目录（模块夹具直拷 sample 真实 handler——测试即真实产物）。
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

/// 插件产物目录（resolve_plugins_dir 会再拼 `<host-triple>/`）。worktree 无 bin/
/// （xtask 产物在主仓）；正常检出/CI 命中第一候选后落回 loader 的
/// `<workspace_root>/bin/plugins` 兜底——两候选谁存在用谁，都不存在留 None（启动
/// 报缺 auth 插件，指向明确）。
fn plugins_dir() -> Option<PathBuf> {
    ["../bin/plugins", "../../../../bin/plugins"]
        .iter()
        .map(|p| Path::new(env!("CARGO_MANIFEST_DIR")).join(p))
        .find(|p| p.is_dir())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oidc_full_chain_login_bridge_and_tenant() {
    let _g = lock();
    let t = tmp_project(&[]);
    // 空闲端口预探（issuer/redirect_uri 是配置期字符串，不能用 port=0 随机端口）。
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
    let self_base = format!("http://127.0.0.1:{port}/v1/api");
    let issuer = format!("{self_base}/idp");
    // RS256 测试密钥（PKCS#8 PEM；与根 crate 同 0.9，依赖树单版本）。
    use rsa::pkcs8::EncodePrivateKey;
    let key = rsa::RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).unwrap();
    std::fs::write(
        t.join("oidc_rs256.pem"),
        key.to_pkcs8_pem(rsa::pkcs8::LineEnding::LF)
            .unwrap()
            .as_bytes(),
    )
    .unwrap();
    // 模块夹具：idp/oidc/auth 直拷 sample；_platform（users 表 seed）与 me（受保护
    // 探针）现场写。manifest.yaml 缺失即启动失败，五个模块都得有。
    let src = t.join("src");
    for m in ["idp", "oidc", "auth"] {
        copy_dir(&sample().join("src").join(m), &src.join(m));
    }
    std::fs::create_dir_all(src.join("_platform")).unwrap();
    std::fs::write(
        src.join("_platform/manifest.yaml"),
        "name: _platform\ndesc: users\nversion: 0.1.0\n",
    )
    .unwrap();
    std::fs::write(
        src.join("_platform/seed.sql"),
        "CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY AUTOINCREMENT, \
         username TEXT NOT NULL UNIQUE, password_hash TEXT NOT NULL, roles TEXT NOT NULL DEFAULT '[]');\n\
         INSERT OR IGNORE INTO users (id, username, password_hash, roles) VALUES \
         (1, 'demo', '$2b$10$aKN7gpFP.dhK7Il8sc19neUPaziSONYdsfks1xm0H2COzkp2vlqV2', '[\"admin\"]');\n",
    )
    .unwrap();
    std::fs::create_dir_all(src.join("me")).unwrap();
    std::fs::write(
        src.join("me/manifest.yaml"),
        "name: me\ndesc: d\nversion: 0.1.0\n",
    )
    .unwrap();
    std::fs::write(
        src.join("me/api.ts"),
        "export default { get() { json.ok({ u: http.user }); } };\n",
    )
    .unwrap();
    // config：证书（base_cfg）+ 鉴权/租户豁免 + OIDC 段 + 仅装配 oj-auth 插件。
    let mut cfg = base_cfg(&t);
    cfg.server.host = "127.0.0.1".into(); // issuer/redirect_uri 须与实际绑定地址一致
    cfg.server.port = port;
    let anon = vec![
        "/oidc/*".into(),
        "/idp/*".into(),
        "/idp/.well-known/*".into(),
    ];
    cfg.auth = Some(AuthCfg {
        jwt_secret: "e2e".into(),
        anonymous_paths: anon.clone(),
        ..Default::default()
    });
    cfg.tenant.enable = true;
    cfg.tenant.anonymous_paths = anon;
    cfg.oidc = Some(OidcSection {
        issuer: issuer.clone(),
        private_key_path: "oidc_rs256.pem".into(), // 相对 config 目录
        rp: std::collections::HashMap::from([(
            "default".into(),
            OidcRpCfg {
                issuer: issuer.clone(),
                client_id: "sample-rp".into(),
                client_secret: "rp-secret".into(),
                scope: "openid profile".into(),
            },
        )]),
        clients: std::collections::HashMap::from([(
            "sample-rp".into(),
            OidcClientCfg {
                secret: "rp-secret".into(),
                redirect_uris: vec![format!("{self_base}/oidc/callback")],
                tenant: "default".into(),
            },
        )]),
    });
    cfg.plugins_dir = plugins_dir();
    cfg.plugins.insert("auth".into(), serde_json::json!({})); // 严格清单：只装 oj-auth
    let (addr, _h) = server_cmd::start(cfg, &t, src, "/v1/api".into(), true)
        .await
        .unwrap();
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .unwrap();
    let base = format!("http://{addr}/v1/api");
    // 1) RP login → 302 IdP authorize（state/nonce/PKCE 快照进 KV）。
    let r1 = http
        .get(format!("{base}/oidc/login?tenant=default"))
        .send()
        .await
        .unwrap();
    let s1 = r1.status();
    let b1 = r1.text().await.unwrap_or_default();
    assert_eq!(s1, 302, "r1 body: {b1}");
    let r1h = http
        .get(format!("{base}/oidc/login?tenant=default"))
        .send()
        .await
        .unwrap();
    let loc1 = r1h
        .headers()
        .get("location")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_else(|| panic!("no location, status={} body={b1}", r1h.status()));
    // 2) authorize 无 OP 会话 → 401 login required。
    let r2 = http.get(&loc1).send().await.unwrap();
    let (s2, b2) = (r2.status(), r2.text().await.unwrap());
    assert_eq!(s2, 401, "{b2}");
    // 3) OP login（bcrypt 校验 _platform.users）→ IDP_SESSION cookie。
    let r3 = http
        .post(format!("{base}/idp/login"))
        .json(&serde_json::json!({"username": "demo", "password": "demo1234"}))
        .send()
        .await
        .unwrap();
    let (s3, cookie) = (
        r3.status(),
        r3.headers()["set-cookie"].to_str().unwrap().to_string(),
    );
    assert_eq!(s3, 200, "{cookie}");
    let sid = cookie.split(';').next().unwrap().to_string();
    // 4) authorize 带 OP 会话 → 302 回 RP callback（code 一次一用）。
    let r4 = http.get(&loc1).header("cookie", &sid).send().await.unwrap();
    let (s4, loc4) = (
        r4.status(),
        r4.headers()["location"].to_str().unwrap().to_string(),
    );
    assert_eq!(s4, 302, "{loc4}");
    assert!(
        loc4.starts_with(&format!("{self_base}/oidc/callback?code=")),
        "{loc4}"
    );
    // 5) callback（豁免路径，无租户头）→ fetch 回环换 token + 验签 + JIT 桥接。
    let r5 = http.get(&loc4).send().await.unwrap();
    let s5 = r5.status();
    let body: serde_json::Value = r5.json().await.unwrap();
    assert_eq!(s5, 200, "{body}");
    assert_eq!(body["code"], 0, "{body}");
    // JIT 按 `oidc:default:1`（tenant+sub 命名空间，sub = OP 会话 uid "1"）查
    // users.username 未命中 → 现建行（demo 行 username 是 'demo'），故桥接会话是
    // id 2 / roles [] 的 JIT 行。
    assert_eq!(body["data"]["user"]["id"], "2", "{body}");
    let access = body["data"]["access_token"].as_str().unwrap().to_string();
    // 6) 桥接会话打受保护路由（租户头恢复强制；http.user = access token claims）。
    let r6 = http
        .get(format!("{base}/me/"))
        .bearer_auth(&access)
        .header("X-TENANT-ID", "acme-ish")
        .send()
        .await
        .unwrap();
    let s6 = r6.status();
    let v6: serde_json::Value = r6.json().await.unwrap();
    assert_eq!(s6, 200, "{v6}");
    assert_eq!(v6["data"]["u"]["id"], "2", "{v6}");
    assert_eq!(v6["data"]["u"]["claims"]["sub"], "2", "{v6}");
    // 豁免只限跳转腿：同一受保护路由缺租户头仍 400（强制未被豁免面误伤）。
    let r6b = http
        .get(format!("{base}/me/"))
        .bearer_auth(&access)
        .send()
        .await
        .unwrap();
    let (s6b, b6b) = (r6b.status(), r6b.text().await.unwrap());
    assert_eq!(s6b, 400, "{b6b}");
    // 7) state 一次一用：重放被拒。
    let r7 = http.get(&loc4).send().await.unwrap();
    let (s7, b7) = (r7.status(), r7.text().await.unwrap());
    assert_eq!(s7, 401, "{b7}");
    let _ = std::fs::remove_dir_all(&t);
}
