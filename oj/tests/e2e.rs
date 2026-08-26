//! E2E：sample 作为 oj server 验收载体（spec UC-1~6,8,9,13,14,15）。
//!
//! transpile_hits 是进程级计数，sibling 测试并发转译会污染 uc14 的
//! delta==1 断言（T9 教训），故全体用例串行：E2E_LOCK 全程持有。

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use only_js::bridge::transpile::transpile_hits;
use only_js::config::Config;
use oj::args::BuildArgs;
use oj::server_cmd;

fn lock() -> MutexGuard<'static, ()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(())).lock().unwrap_or_else(|e| e.into_inner())
}

fn sample() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../sample").canonicalize().unwrap()
}

async fn boot(dev: bool) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>, PathBuf) {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // config_dir = sample 项目根（loader 的 project_root 钳制要求 api 在根内）；
    // 仅 db 用独立临时文件隔离，seed 由 start() 对新库重放。
    let tmp = std::env::temp_dir().join(format!("oj-e2e-{}-{}", std::process::id(), n));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let root = sample();
    let mut cfg: Config =
        serde_yaml::from_str(&std::fs::read_to_string(root.join("config.yaml")).unwrap()).unwrap();
    cfg.server.port = 0;
    cfg.db.insert("default".into(), format!("sqlite://{}/db.sqlite", tmp.display()));
    // e2e 是 v0.1 UC 验收（不带租户头/不登录）；sample 的 tenant/auth 留给手工冒烟，
    // 租户注入/400 与鉴权全链路在 mdm-server::tests 覆盖。
    cfg.tenant = Default::default();
    cfg.auth = None;
    let dir = if dev {
        root.join("src")
    } else {
        // release：现场构建 sample → 项目根内临时 dist（loader 的 project_root 钳制
        // 要求 dist ⊆ config_dir；sample/dist 旧格式已废弃，重生成为 T9 交付）。
        let dist = root.join(".e2e-dist");
        let _ = std::fs::remove_dir_all(&dist);
        oj::build_cmd::run(&oj::args::BuildArgs {
            module: None,
            dir: root.join("src").display().to_string(),
            out: dist.display().to_string(),
            minify: true,
        })
        .await
        .unwrap();
        dist
    };
    let (addr, h) = server_cmd::start(cfg, &root, dir, "/v1/api".into(), dev).await.unwrap();
    (addr, h, tmp)
}

async fn req(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> (u16, serde_json::Value) {
    let c = reqwest::Client::new();
    let mut r = c.request(
        reqwest::Method::from_bytes(method.as_bytes()).unwrap(),
        format!("http://{addr}{path}"),
    );
    if let Some(b) = body {
        r = r.header("content-type", "application/json").body(b.to_string());
    }
    let resp = r.send().await.unwrap();
    let status = resp.status().as_u16();
    // HEAD 无 body，reqwest 解 JSON 会挂：只回 status。
    if method == "HEAD" {
        return (status, serde_json::Value::Null);
    }
    (status, resp.json().await.unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uc1_method_table() {
    let _g = lock();
    let (addr, _h, _t) = boot(true).await;
    // 各动词给最小合法输入：断言的是路由表本身（到 handler 即 200，空 body 会被
    // handler 的 body 解析拒绝，与路由无关）。
    let cases: &[(&str, Option<&str>)] = &[
        ("GET", None),
        ("POST", Some(r#"{"name":"m","role":"user"}"#)),
        ("PUT", Some(r#"{"id":1,"name":"n"}"#)),
        ("DELETE", None),
        ("PATCH", Some(r#"{"id":1,"role":"user"}"#)),
        ("OPTIONS", None),
        ("HEAD", None),
    ];
    for (m, b) in cases {
        let path = match *m {
            "DELETE" => "/v1/api/user/account/?id=999",
            _ => "/v1/api/user/account/",
        };
        let (s, _) = req(addr, m, path, *b).await;
        assert_eq!(s, 200, "{m}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uc2_uc3_crud_params_body() {
    let _g = lock();
    let (addr, _h, _t) = boot(true).await;
    let name = format!("u-{}", std::process::id());
    // POST body 建号。
    let (s, v) = req(
        addr,
        "POST",
        "/v1/api/user/account/",
        Some(&format!(r#"{{"name":"{name}","role":"admin"}}"#)),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["code"], 0, "{v}");
    // query 参数查回。
    let (s, v) = req(addr, "GET", "/v1/api/user/account/?id=1", None).await;
    assert_eq!(s, 200);
    assert_eq!(v["data"][0]["name"], "neo", "{v}");
    // PUT 改名后 GET 验证。
    let _ = req(addr, "PUT", "/v1/api/user/account/", Some(r#"{"id":1,"name":"neo2"}"#)).await;
    let (_, v) = req(addr, "GET", "/v1/api/user/account/?id=1", None).await;
    assert_eq!(v["data"][0]["name"], "neo2", "{v}");
    let (s, _) = req(addr, "DELETE", "/v1/api/user/account/?id=2", None).await;
    assert_eq!(s, 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uc4_nested_route_uc5_join() {
    let _g = lock();
    let (addr, _h, _t) = boot(true).await;
    let (s, v) = req(addr, "GET", "/v1/api/user/profile/detail/", None).await;
    assert_eq!(s, 200);
    assert_eq!(v["data"]["depth"], 3, "{v}");
    let (s, v) = req(addr, "GET", "/v1/api/order/list/", None).await;
    assert_eq!(s, 200);
    assert_eq!(v["data"][0]["account_name"], "neo", "{v}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uc6_release_mode_dist() {
    let _g = lock();
    let (addr, _h, _t) = boot(false).await;
    let (s, v) = req(addr, "GET", "/v1/api/user/account/?id=1", None).await;
    assert_eq!(s, 200);
    assert_eq!(v["data"][0]["name"], "neo", "{v}");
    // order/list（跨模块相对导入 ../../user/_shared/validate）：build 已改写 specifier
    // 指向 dist/user-0.1.0/，release 全链路命中（spec §2.4）。
    let (s, v) = req(addr, "GET", "/v1/api/order/list/", None).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["data"][0]["account_name"], "neo", "{v}");
    let _ = std::fs::remove_dir_all(sample().join(".e2e-dist"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uc9_kv_cache_read_through() {
    let _g = lock();
    let (addr, _h, _t) = boot(true).await;
    let (_, v1) = req(addr, "GET", "/v1/api/order/detail/?id=1", None).await;
    assert_eq!(v1["data"]["cached"], false, "{v1}");
    let (_, v2) = req(addr, "GET", "/v1/api/order/detail/?id=1", None).await;
    assert_eq!(v2["data"]["cached"], true, "{v2}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uc13_uc15_imports_and_bare() {
    let _g = lock();
    let (addr, _h, _t) = boot(true).await;
    // 裸 specifier：建单时 escapeHtml 生效（<script> 被转义）。
    let (s, v) = req(
        addr,
        "POST",
        "/v1/api/order/account/",
        Some(r#"{"account_id":1,"amount":9.9,"no":"<script>x</script>"}"#),
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(v["data"]["no"], "&lt;script&gt;x&lt;/script&gt;", "{v}");
    // 跨模块相对导入（requireRole）过滤 role=user（只回 trinity 的单）。
    let (_, v) = req(addr, "GET", "/v1/api/order/list/?role=user", None).await;
    assert_eq!(v["data"][0]["account_name"], "trinity", "{v}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uc14_transpile_cache_and_hot_reload() {
    let _g = lock();
    // 独立临时项目（不动 sample 文件）。
    let t = std::env::temp_dir().join(format!("oj-hot-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&t);
    std::fs::create_dir_all(t.join("src/u/f")).unwrap();
    std::fs::write(t.join("src/u/manifest.yaml"), "name: u\ndesc: d\nversion: 0.1.0\n").unwrap();
    std::fs::write(
        t.join("src/u/f/api.ts"),
        "export default { get() { json.ok({ v: 1 }); } };\n",
    )
    .unwrap();
    let mut cfg = Config::default();
    cfg.server.port = 0;
    cfg.db.insert("default".into(), "sqlite::memory:".into());
    std::fs::write(t.join("seed.sql"), "").unwrap();
    let (addr, _h) =
        server_cmd::start(cfg, &t, t.join("src"), "/v1/api".into(), true).await.unwrap();
    let before = transpile_hits();
    for _ in 0..3 {
        let (_, v) = req(addr, "GET", "/v1/api/u/f/", None).await;
        assert_eq!(v["data"]["v"], 1);
    }
    // 启动内省已预热转译缓存：3 次请求 0 次新转译（缓存全局共享，跨 actor）。
    assert_eq!(transpile_hits(), before);
    // 热重载：改文件 → mtime 变 → 下次请求新结果。
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(
        t.join("src/u/f/api.ts"),
        "export default { get() { json.ok({ v: 2 }); } };\n",
    )
    .unwrap();
    let (_, v) = req(addr, "GET", "/v1/api/u/f/", None).await;
    assert_eq!(v["data"]["v"], 2, "{v}");
    let _ = std::fs::remove_dir_all(&t);
}

// —— 负向用例（spec §5.7 错误表 404/405/500/408）—— //

/// 临时项目：config_dir 用绝对路径（钳制要求 project_root ⊇ 模块目录）。
fn tmp_project(files: &[(&str, &str)]) -> PathBuf {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let t = std::env::temp_dir().join(format!(
        "oj-neg-{}-{}",
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

/// 最小可用配置（port 0 随机端口；default 内存库）。
fn base_cfg() -> Config {
    let mut cfg = Config::default();
    cfg.server.port = 0;
    cfg.db.insert("default".into(), "sqlite::memory:".into());
    cfg
}

const MANIFEST: &str = "name: u\ndesc: d\nversion: 0.1.0\n";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uc7_manifest_mismatch_blocks_startup() {
    let _g = lock();
    let t = tmp_project(&[("src/order/manifest.yaml", "name: orderr\ndesc: d\nversion: 0.1.0\n")]);
    let e = server_cmd::start(base_cfg(), &t, t.join("src"), "/v1/api".into(), true)
        .await
        .err()
        .unwrap_or_default();
    assert!(e.contains("orderr") && e.contains("order"), "{e}");
    let _ = std::fs::remove_dir_all(&t);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn build_emits_routes_js_strips_route_then_release_serves() {
    let _g = lock();
    let t = tmp_project(&[
        ("src/u/manifest.yaml", MANIFEST),
        ("src/u/_shared/v.ts", "export const ok = (x) => x > 0;\n"),
        (
            "src/u/item/api.ts",
            "import { ok } from \"../_shared/v\";\n\
             function detail() { json.ok({ id: Number(http.param(\"id\", 0)), ok: ok(1) }); }\n\
             detail.route = \"{id}\";\n\
             export default { get: detail };\n",
        ),
        ("src/u/list/api.ts", "export default { get() { json.ok({ all: true }); } };\n"),
    ]);
    let a = BuildArgs {
        module: Some("u".into()),
        dir: t.join("src").display().to_string(),
        out: t.join("dist").display().to_string(),
        minify: true,
    };
    oj::build_cmd::run(&a).await.unwrap();
    // routes.js：.route 行 + 镜像行（pattern 无 base 含模块段，file 为同名产物）
    let vd = t.join("dist/u-0.1.0");
    let routes = std::fs::read_to_string(vd.join("routes.js")).unwrap();
    assert!(routes.contains("\"u/item/{id}\""), "{routes}");
    assert!(routes.contains("\"u/list\""), "{routes}");
    assert!(routes.contains("\"item/api.js\""), "{routes}");
    assert!(routes.contains("\"list/api.js\""), "{routes}");
    assert!(!routes.contains("/v1/api"), "{routes}");
    // 产物：原名原目录；.route 剥离；相对 import 补 .js；默认 minify 单行；_shared/manifest 落盘
    let item = std::fs::read_to_string(vd.join("item/api.js")).unwrap();
    assert!(!item.contains(".route"), "{item}");
    assert!(!item.trim_end().contains('\n'), "{item}");
    assert!(item.contains("\"../_shared/v.js\""), "{item}");
    assert!(vd.join("_shared/v.js").is_file());
    assert!(vd.join("manifest.yaml").is_file());
    assert!(t.join("dist/u-0.1.0.tgz").is_file());
    assert_eq!(
        oj::manifest::load_lock(&t.join("dist/manifests.yaml")).unwrap()["u"],
        "0.1.0"
    );
    // release 全链路：聚合 dist/manifests.yaml 锁定版本服务（spec §3）
    let (addr, _h) =
        server_cmd::start(base_cfg(), &t, t.join("dist"), "/v1/api".into(), false).await.unwrap();
    let (s, v) = req(addr, "GET", "/v1/api/u/item/3", None).await;
    assert_eq!(s, 200, "{v}"); // .route 行：{id} 参数 + _shared import 生效
    assert_eq!(v["data"]["id"], 3, "{v}");
    assert_eq!(v["data"]["ok"], true, "{v}");
    let (s, _) = req(addr, "GET", "/v1/api/u/list/", None).await;
    assert_eq!(s, 200); // 镜像行
    let _ = std::fs::remove_dir_all(&t);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn build_then_release_serves_end_to_end() {
    let _g = lock();
    // 夹具：两个模块（user 0.1.0 带 .route、other 0.9.0 纯镜像）
    let t = tmp_project(&[
        ("src/user/manifest.yaml", "name: user\ndesc: d\nversion: 0.1.0\n"),
        (
            "src/user/item/api.ts",
            "function get() { json.ok({ id: Number(http.param(\"id\", 0)) }); }\n\
             get.route = \"{id}\";\n\
             export default { get };\n",
        ),
        ("src/other/manifest.yaml", "name: other\ndesc: d\nversion: 0.9.0\n"),
        ("src/other/l/api.ts", "export default { get() { json.ok({ m: 1 }); } };\n"),
    ]);
    oj::build_cmd::run(&BuildArgs {
        module: None,
        dir: t.join("src").display().to_string(),
        out: t.join("dist").display().to_string(),
        minify: true,
    })
    .await
    .unwrap();
    // manifests.yaml 两键
    let lock = oj::manifest::load_lock(&t.join("dist/manifests.yaml")).unwrap();
    assert_eq!(lock.len(), 2, "{lock:?}");
    assert_eq!(lock["other"], "0.9.0");
    let (addr, _h) =
        server_cmd::start(base_cfg(), &t, t.join("dist"), "/v1/api".into(), false).await.unwrap();
    // /v1/api/user/item/3 命中 .route 行；/v1/api/other/l 命中镜像行
    let (s, v) = req(addr, "GET", "/v1/api/user/item/3", None).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["data"]["id"], 3, "{v}");
    let (s, v) = req(addr, "GET", "/v1/api/other/l/", None).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["data"]["m"], 1, "{v}");
    // 表外模块 404
    let (s, _) = req(addr, "GET", "/v1/api/none", None).await;
    assert_eq!(s, 404);
    let _ = std::fs::remove_dir_all(&t);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn release_mode_loads_routes_js_without_introspection() {
    let _g = lock();
    let t = tmp_project(&[
        ("dist/manifests.yaml", "u: 0.1.0\n"),
        ("dist/u-0.1.0/manifest.yaml", MANIFEST),
        ("dist/u-0.1.0/f/api.js", "export default { get() { json.ok({ v: 1 }); } };\n"),
        (
            "dist/u-0.1.0/routes.js",
            "export default [ { method: \"get\", pattern: \"u/f/{id}\", file: \"f/api.js\" } ];\n",
        ),
    ]);
    let (addr, _h) =
        server_cmd::start(base_cfg(), &t, t.join("dist"), "/v1/api".into(), false).await.unwrap();
    let (s, v) = req(addr, "GET", "/v1/api/u/f/7", None).await;
    assert_eq!(s, 200);
    assert_eq!(v["data"]["v"], 1, "{v}");
    // release 无 fs 兜底：routes.js 之外的镜像路径 404
    let (s, _) = req(addr, "GET", "/v1/api/u/f/", None).await;
    assert_eq!(s, 404);
    let _ = std::fs::remove_dir_all(&t);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn release_mode_without_routes_js_fails_fast() {
    let _g = lock();
    let t = tmp_project(&[("dist/u/manifest.yaml", MANIFEST)]);
    let e = server_cmd::start(base_cfg(), &t, t.join("dist"), "/v1/api".into(), false)
        .await
        .err()
        .unwrap_or_default();
    assert!(e.contains("oj build"), "{e}");
    let _ = std::fs::remove_dir_all(&t);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uc10_404_and_405_and_traversal() {
    let _g = lock();
    let t = tmp_project(&[
        ("src/u/manifest.yaml", MANIFEST),
        ("src/u/f/api.ts", "export default { get() { json.ok({}); } };\n"),
    ]);
    let (addr, _h) = server_cmd::start(base_cfg(), &t, t.join("src"), "/v1/api".into(), true)
        .await
        .unwrap();
    let (s, _) = req(addr, "GET", "/v1/api/none/here/", None).await;
    assert_eq!(s, 404);
    let (s, v) = req(addr, "DELETE", "/v1/api/u/f/", None).await;
    assert_eq!(s, 405);
    // 405 判定上移到路由表层（pattern 命中、方法缺席），消息随之变化。
    assert!(v["msg"].as_str().unwrap().contains("DELETE"), "{v}");
    // 目录穿越按 404（url crate 将 ../ 归一化为 /v1/etc/，同样落 404 信封）。
    let (s, _) = req(addr, "GET", "/v1/api/../etc/", None).await;
    assert_eq!(s, 404);
    let _ = std::fs::remove_dir_all(&t);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uc11_compile_error_envelope() {
    let _g = lock();
    let t = tmp_project(&[
        ("src/u/manifest.yaml", MANIFEST),
        ("src/u/f/api.ts", "function {{{{\nexport default {};\n"),
    ]);
    let (addr, _h) = server_cmd::start(base_cfg(), &t, t.join("src"), "/v1/api".into(), true)
        .await
        .unwrap();
    let (s, v) = req(addr, "GET", "/v1/api/u/f/", None).await;
    assert_eq!(s, 500);
    assert!(v["msg"].as_str().unwrap_or("").contains("api.ts"), "{v}");
    let _ = std::fs::remove_dir_all(&t);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uc12_timeout_408_server_survives() {
    let _g = lock();
    let t = tmp_project(&[
        ("src/u/manifest.yaml", MANIFEST),
        ("src/u/loop/api.ts", "export default { get() { while (true) {} } };\n"),
        ("src/u/ok/api.ts", "export default { get() { json.ok({ alive: true }); } };\n"),
    ]);
    let mut cfg = base_cfg();
    cfg.server.timeout = "300ms".into();
    let (addr, _h) = server_cmd::start(cfg, &t, t.join("src"), "/v1/api".into(), true)
        .await
        .unwrap();
    let (s, _) = req(addr, "GET", "/v1/api/u/loop/", None).await;
    assert_eq!(s, 408);
    let (s, v) = req(addr, "GET", "/v1/api/u/ok/", None).await;
    assert_eq!(s, 200);
    assert_eq!(v["data"]["alive"], true, "{v}");
    let _ = std::fs::remove_dir_all(&t);
}
