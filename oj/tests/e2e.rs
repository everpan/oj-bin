//! E2E：sample 作为 oj server 验收载体（spec UC-1~6,8,9,13,14,15）。
//!
//! transpile_hits 是进程级计数，sibling 测试并发转译会污染 uc14 的
//! delta==1 断言（T9 教训），故全体用例串行：E2E_LOCK 全程持有。

// lock() 的 std MutexGuard 有意全程持有（见下：E2E 全用例串行），横跨 await 是设计而非疏漏。
#![allow(clippy::await_holding_lock)]

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use oj::args::BuildArgs;
use oj::serve_cmd;
use only_js::bridge::transpile::transpile_hits;
use only_js::config::Config;

fn lock() -> MutexGuard<'static, ()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// YAML 双引号标量里的**纯路径**字段：Windows 反斜杠在 YAML 里是转义序列
/// （`C:\Users` 的 `\U` → unknown escape），故转正斜杠；Windows 文件 API 接受正斜杠。
///
/// **不要**用它处理 `sqlite://` DSN：`canonicalize()` 在 Windows 上产出 verbatim 前缀
/// `\\?\D:\...`，裸 replace 会把 `\\?\` 变成 `//?/`，从而命中 `normalize_sqlite_dsn`
/// 的「`//` 直通」分支（src/bridge/db_backend.rs），跳过盘符修正 → SQLITE_CANTOPEN。
/// DSN 一律走 `oj_plugin_ffi::path_util::sqlite_file_dsn`（先 `dunce` 剥 verbatim，
/// 再转正斜杠，并用单冒号 `sqlite:`）。
fn fwd(p: &Path) -> String {
    p.display().to_string().replace('\\', "/")
}

fn sample() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../sample")
        .canonicalize()
        .unwrap()
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
    cfg.db.insert(
        "default".into(),
        oj_plugin_ffi::path_util::sqlite_file_dsn(&tmp.join("db.sqlite")),
    );
    // e2e 是 v0.1 UC 验收（不带租户头/不登录）；sample 的 tenant/auth 留给手工冒烟，
    // 租户注入/400 与鉴权全链路在 mdm-server::tests 覆盖。
    cfg.tenant = Default::default();
    cfg.auth = None;
    // sample/config.yaml 的 app_path: "dist" 指向仓库内产物目录（已停止跟踪，CI
    // 新克隆无此目录）——boot() 的 UC 不覆盖静态兜底（多站点静态有专属 e2e，见
    // multi_static_sites_serve_by_longest_prefix_end_to_end），显式关闭，避免
    // resolve_static_sites 因目录缺失 fail-fast。
    cfg.server.app_path = None;
    // 证书必配（无逃生口）：启动需真实签名证书，随测试临时目录生成（有效期 1 年）。
    let n = serve::test_support::now_secs();
    serve::test_support::write_cert_into(
        &mut cfg.server,
        &tmp,
        n.saturating_sub(3600),
        n + 365 * 86_400,
    );
    let dir = if dev {
        root.join("src")
    } else {
        // release：现场构建 sample → 项目根内临时 dist（loader 的 project_root 钳制
        // 要求 dist ⊆ config_dir；sample/dist 旧格式已废弃，重生成为 T9 交付）。
        let dist = root.join(".e2e-dist");
        let _ = std::fs::remove_dir_all(&dist);
        oj::build_cmd::run(&oj::args::BuildArgs {
            module: None,
            config: "config.yaml".into(),
            dir: root.join("src").display().to_string(),
            out: dist.display().to_string(),
            minify: true,
            check: false,
        })
        .await
        .unwrap();
        dist
    };
    let (addr, h) = {
        // release 语义（README 部署故事）：build 后先 `oj migrate` 再 server——
        // release verify 门禁要求账本与产物齐平，空库首启必须先显式迁移。
        if !dev {
            let acc = only_js::bridge::DbBackendRegistry::builtin()
                .connect(cfg.db.get("default").unwrap(), &root)
                .await
                .unwrap();
            oj::migrate::apply_all(Some(&acc), &dir, false, false)
                .await
                .unwrap();
        }
        serve_cmd::start(cfg, &root, dir, "/v1/api".into(), dev)
            .await
            .unwrap()
    };
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
        r = r
            .header("content-type", "application/json")
            .body(b.to_string());
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
    let _ = req(
        addr,
        "PUT",
        "/v1/api/user/account/",
        Some(r#"{"id":1,"name":"neo2"}"#),
    )
    .await;
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
    std::fs::write(
        t.join("src/u/manifest.yaml"),
        "name: u\ndesc: d\nversion: 0.1.0\n",
    )
    .unwrap();
    std::fs::write(
        t.join("src/u/f/api.ts"),
        "export default { get() { json.ok({ v: 1 }); } };\n",
    )
    .unwrap();
    let mut cfg = Config::default();
    cfg.server.port = 0;
    // 证书必配（无逃生口）：生成真实签名证书并配好两路径。
    let n = serve::test_support::now_secs();
    serve::test_support::write_cert_into(
        &mut cfg.server,
        &t,
        n.saturating_sub(3600),
        n + 365 * 86_400,
    );
    cfg.db.insert("default".into(), "sqlite::memory:".into());
    std::fs::write(t.join("seed.sql"), "").unwrap();
    let (addr, _h) = serve_cmd::start(cfg, &t, t.join("src"), "/v1/api".into(), true)
        .await
        .unwrap();
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

/// 最小可用配置（port 0 随机端口；default 内存库；证书必配 → 在项目目录生成真实证书）。
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

const MANIFEST: &str = "name: u\ndesc: d\nversion: 0.1.0\n";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uc7_manifest_mismatch_blocks_startup() {
    let _g = lock();
    let t = tmp_project(&[(
        "src/order/manifest.yaml",
        "name: orderr\ndesc: d\nversion: 0.1.0\n",
    )]);
    let e = serve_cmd::start(base_cfg(&t), &t, t.join("src"), "/v1/api".into(), true)
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
        (
            "src/u/list/api.ts",
            "export default { get() { json.ok({ all: true }); } };\n",
        ),
    ]);
    let a = BuildArgs {
        module: Some("u".into()),
        config: "config.yaml".into(),
        dir: t.join("src").display().to_string(),
        out: t.join("dist").display().to_string(),
        minify: true,
        check: false,
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
    let (addr, _h) = serve_cmd::start(base_cfg(&t), &t, t.join("dist"), "/v1/api".into(), false)
        .await
        .unwrap();
    let (s, v) = req(addr, "GET", "/v1/api/u/item/3", None).await;
    assert_eq!(s, 200, "{v}"); // .route 行：{id} 参数 + _shared import 生效
    assert_eq!(v["data"]["id"], 3, "{v}");
    assert_eq!(v["data"]["ok"], true, "{v}");
    let (s, _) = req(addr, "GET", "/v1/api/u/list/", None).await;
    assert_eq!(s, 200); // 镜像行
    let _ = std::fs::remove_dir_all(&t);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn underscore_dir_param_dev_serves_and_release_round_trips() {
    let _g = lock();
    // v0.1.27：`_name_` 目录段即路径参数，dev 建表与 release routes.js 同口径。
    let t = tmp_project(&[
        ("src/u/manifest.yaml", MANIFEST),
        (
            "src/u/_id_/api.ts",
            "export default { get() { json.ok({ id: http.param(\"id\") }); } };\n",
        ),
    ]);
    // dev：路由表（matchit）命中 `_id_` 目录 → `{id}` 参数
    let (addr, _h) = serve_cmd::start(base_cfg(&t), &t, t.join("src"), "/v1/api".into(), true)
        .await
        .unwrap();
    let (s, v) = req(addr, "GET", "/v1/api/u/42", None).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["data"]["id"], "42", "{v}");
    // 字面 `_id_` URL 被参数吃掉（实参值 "_id_"），不 404
    let (s, v) = req(addr, "GET", "/v1/api/u/_id_", None).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["data"]["id"], "_id_", "{v}");
    drop(_h);
    // build → release 直载：同口径
    let a = BuildArgs {
        module: Some("u".into()),
        config: "config.yaml".into(),
        dir: t.join("src").display().to_string(),
        out: t.join("dist").display().to_string(),
        minify: true,
        check: false,
    };
    oj::build_cmd::run(&a).await.unwrap();
    let routes = std::fs::read_to_string(t.join("dist/u-0.1.0/routes.js")).unwrap();
    assert!(routes.contains("\"u/{id}\""), "{routes}");
    assert!(routes.contains("\"_id_/api.js\""), "{routes}");
    let (addr, _h) = serve_cmd::start(base_cfg(&t), &t, t.join("dist"), "/v1/api".into(), false)
        .await
        .unwrap();
    let (s, v) = req(addr, "GET", "/v1/api/u/42", None).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["data"]["id"], "42", "{v}");
    let _ = std::fs::remove_dir_all(&t);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn build_then_release_serves_end_to_end() {
    let _g = lock();
    // 夹具：两个模块（user 0.1.0 带 .route、other 0.9.0 纯镜像）
    let t = tmp_project(&[
        (
            "src/user/manifest.yaml",
            "name: user\ndesc: d\nversion: 0.1.0\n",
        ),
        (
            "src/user/item/api.ts",
            "function get() { json.ok({ id: Number(http.param(\"id\", 0)) }); }\n\
             get.route = \"{id}\";\n\
             export default { get };\n",
        ),
        (
            "src/other/manifest.yaml",
            "name: other\ndesc: d\nversion: 0.9.0\n",
        ),
        (
            "src/other/l/api.ts",
            "export default { get() { json.ok({ m: 1 }); } };\n",
        ),
    ]);
    oj::build_cmd::run(&BuildArgs {
        module: None,
        config: "config.yaml".into(),
        dir: t.join("src").display().to_string(),
        out: t.join("dist").display().to_string(),
        minify: true,
        check: false,
    })
    .await
    .unwrap();
    // manifests.yaml 两键
    let lock = oj::manifest::load_lock(&t.join("dist/manifests.yaml")).unwrap();
    assert_eq!(lock.len(), 2, "{lock:?}");
    assert_eq!(lock["other"], "0.9.0");
    let (addr, _h) = serve_cmd::start(base_cfg(&t), &t, t.join("dist"), "/v1/api".into(), false)
        .await
        .unwrap();
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
        (
            "dist/u-0.1.0/f/api.js",
            "export default { get() { json.ok({ v: 1 }); } };\n",
        ),
        (
            "dist/u-0.1.0/routes.js",
            "export default [ { method: \"get\", pattern: \"u/f/{id}\", file: \"f/api.js\" } ];\n",
        ),
    ]);
    let (addr, _h) = serve_cmd::start(base_cfg(&t), &t, t.join("dist"), "/v1/api".into(), false)
        .await
        .unwrap();
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
    let e = serve_cmd::start(base_cfg(&t), &t, t.join("dist"), "/v1/api".into(), false)
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
        (
            "src/u/f/api.ts",
            "export default { get() { json.ok({}); } };\n",
        ),
    ]);
    let (addr, _h) = serve_cmd::start(base_cfg(&t), &t, t.join("src"), "/v1/api".into(), true)
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
    let (addr, _h) = serve_cmd::start(base_cfg(&t), &t, t.join("src"), "/v1/api".into(), true)
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
        (
            "src/u/loop/api.ts",
            "export default { get() { while (true) {} } };\n",
        ),
        (
            "src/u/ok/api.ts",
            "export default { get() { json.ok({ alive: true }); } };\n",
        ),
    ]);
    let mut cfg = base_cfg(&t);
    cfg.server.timeout = "300ms".into();
    let (addr, _h) = serve_cmd::start(cfg, &t, t.join("src"), "/v1/api".into(), true)
        .await
        .unwrap();
    let (s, _) = req(addr, "GET", "/v1/api/u/loop/", None).await;
    assert_eq!(s, 408);
    let (s, v) = req(addr, "GET", "/v1/api/u/ok/", None).await;
    assert_eq!(s, 200);
    assert_eq!(v["data"]["alive"], true, "{v}");
    let _ = std::fs::remove_dir_all(&t);
}

/// 统一审查 #8（spec §8）：进程级停机 e2e——拉起 oj server 子进程，观察长任务
/// 启动日志 → SIGTERM → 任务在 grace 内自然收场 → HTTP 排空 → 进程退出。
/// 子进程用测试编译产物（CARGO_BIN_EXE_oj，cargo test 默认 profile——仅测试
/// 脚手架，发布物门禁仍走 `cargo xtask build` 的 release）。无插件依赖：极简
/// config 不声明 auth/oidc 等，纯任务池生命周期验收。
// kill -TERM 是 POSIX 语义（Windows 无对应：Git Bash kill 打不进原生进程，
// ctrl_c 语义另测）——按设计只在 unix 跑，Windows runner 直接编译排除。
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn given_running_server_when_sigterm_then_tasks_stop_and_process_exits() {
    let _g = lock();
    let tmp = std::env::temp_dir().join(format!("oj-e2e-sig-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let root = sample();
    // 极简 config：console_log 开（任务/停机日志镜像到子进程 stderr 可断言）；
    // 证书复用 sample 的示例证书（绝对路径）；db 隔离到临时目录；tasks 默认目录。
    // 纯路径字段走 `fwd`（YAML 双引号标量里反斜杠是转义）；db DSN 走
    // `sqlite_file_dsn`（裸 replace 会把 verbatim `\\?\` 变成 `//?/`，见其文档）。
    let cfg = format!(
        "server:\n  host: \"127.0.0.1\"\n  port: 0\n  console_log: true\n  public_key_path: \"{}\"\n  certificate_path: \"{}\"\ndb:\n  default: \"{}\"\ntasks:\n  dir: \"tasks\"\n",
        fwd(&root.join("config/public.pem")),
        fwd(&root.join("config/cert.jws")),
        oj_plugin_ffi::path_util::sqlite_file_dsn(&tmp.join("db.sqlite")),
    );
    std::fs::write(tmp.join("config.yaml"), &cfg).unwrap();

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_oj"))
        .args([
            "serve",
            "-c",
            &tmp.join("config.yaml").display().to_string(),
            "--api-path",
            &root.join("src").display().to_string(),
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn oj serve child");

    // stdout/stderr 各一线程收集进同一缓冲（task 日志走 eprintln = stderr）。
    let out_buf = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let mut readers = Vec::new();
    for stream in [
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
    ] {
        let Some(stream) = stream else { continue };
        let buf = out_buf.clone();
        readers.push(std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(stream).lines() {
                match line {
                    Ok(l) => buf.lock().unwrap().push_str(&format!("{l}\n")),
                    Err(_) => break,
                }
            }
        }));
    }

    // 等 started（≤60s；首启含全 sample 转译）。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        if out_buf
            .lock()
            .unwrap()
            .contains("task: 2 task(s) → started")
        // demo + wsclient（v0.1.7 WS 案例）
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "server/tasks not started in time:\n{}",
            out_buf.lock().unwrap()
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    assert!(
        out_buf
            .lock()
            .unwrap()
            .contains("task: demo (task_demo.ts) → started"),
        "{}",
        out_buf.lock().unwrap()
    );

    // SIGTERM（unix 信号；windows 仅 ctrl_c 语义，本用例 cfg(unix) 语义下跳过不适平台）。
    let status = std::process::Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status()
        .expect("send SIGTERM");

    // 等退出（≤ grace 30s + 收尾余量），断言停机序列日志。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(40);
    let code = loop {
        match child.try_wait().unwrap() {
            Some(st) => break st,
            None => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "server did not exit after SIGTERM:\n{}",
                    out_buf.lock().unwrap()
                );
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
    };
    assert!(status.success(), "kill -TERM exit: {status}");
    assert!(code.success(), "server exit: {code}");
    let log = out_buf.lock().unwrap();
    assert!(log.contains("shutdown: stop flag set"), "{log}");
    assert!(log.contains("task: demo → stopped"), "{log}");
    assert!(log.contains("task: wsclient → stopped"), "{log}");
    let _ = std::fs::remove_dir_all(&tmp);
}

/// 覆盖率 spec 波1：`oj test` 全链路——真实 V8 + `client` oneshot 派发（零 TCP）
/// + describe/it 框架 + json 报告落盘 + 退出码约定。sample L1 套件（7 文件 ~40
/// 用例，auth/tenant 全开）全绿为门槛。
/// 子进程形态：插件单例（oj-auth GUARD 等 OnceLock）首次 init 后忽略后续 cfg，
/// 同进程内先跑的 uc 测试会用空 secret 占住守卫 → L1 的 sample cfg 失效。子进程
/// 隔离即生产语义（一次装配一个进程）；子进程同为插桩二进制，覆盖率照常入账。
#[tokio::test(flavor = "current_thread")]
async fn given_sample_l1_suite_when_oj_test_then_all_pass_and_report_written() {
    let _g = lock();
    let tmp = std::env::temp_dir().join(format!("oj-e2e-testrun-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let report = tmp.join("report.json");
    // sample/dist 已停止跟踪（CI 新克隆无此目录）；子进程 `oj test` 读 sample
    // config 且无 --app-path 覆盖手段，resolve_static_root 只要求目录存在，
    // 现场补建即可（gitignore 内，本地常态本就存在；oj test 不做静态断言）。
    std::fs::create_dir_all(sample().join("dist")).unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_oj"))
        .args([
            "test",
            "-c",
            &sample().join("config.yaml").display().to_string(),
            "-d",
            &sample().join("src").display().to_string(),
            "--format",
            "json",
            "--output",
            &report.display().to_string(),
        ])
        .output()
        .expect("spawn oj test child");
    assert!(
        out.status.success(),
        "L1 套件必须全绿（stderr: {}）",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&report).unwrap()).unwrap();
    assert_eq!(v["failed"], 0, "{v}");
    assert!(v["total"].as_u64().unwrap() >= 30, "套件规模骤降? {v}");
    assert_eq!(v["passed"], v["total"]);
    let _ = std::fs::remove_dir_all(&tmp);
}

/// 查询构造器全链路（v0.1.14）：schema.yaml 声明 a/b 两表（gate=auto 自动建表）
/// → HTTP POST 走 db.table().insert().run()（值经绑定参数）→ HTTP GET 走
/// join + select 全链路；白名单外的列名在 op 侧拒绝（500 信封）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e2e_query_builder_join_and_insert() {
    let _g = lock();
    let t = tmp_project(&[
        ("src/u/manifest.yaml", MANIFEST),
        (
            "src/u/schema.yaml",
            "tables:\n\
             \x20 a:\n\
             \x20   pk: id\n\
             \x20   columns:\n\
             \x20     id: { type: integer, autoincrement: true }\n\
             \x20     name: { type: text }\n\
             \x20 b:\n\
             \x20   pk: id\n\
             \x20   columns:\n\
             \x20     id: { type: integer, autoincrement: true }\n\
             \x20     aid: { type: integer, null: false }\n\
             \x20     label: { type: text }\n",
        ),
        (
            "src/u/api/api.ts",
            "function get(): void {\n\
             \x20 db.table(\"a\")\n\
             \x20   .join(\"b\", [{ left: \"a.id\", right: \"b.aid\" }])\n\
             \x20   .select([\"a.name\", \"b.label\"])\n\
             \x20   .all()\n\
             \x20   .then((rows) => json.ok(rows));\n\
             }\n\
             \n\
             function post(): void {\n\
             \x20 const b = http.body as { name?: string };\n\
             \x20 db.table(\"a\").insert(b).run().then((n) => json.ok({ n }));\n\
             }\n\
             \n\
             function put(): void {\n\
             \x20 const b = http.body as { aid?: number; label?: string };\n\
             \x20 db.table(\"b\").insert({ aid: b.aid ?? 0, label: b.label ?? \"\" }).run().then((n) => json.ok({ n }));\n\
             }\n\
             \n\
             export default { get, post, put };\n",
        ),
    ]);
    // 文件库（内存库跨连接不可见，种子须落盘可查）。
    let mut cfg = base_cfg(&t);
    cfg.db.insert(
        "default".into(),
        oj_plugin_ffi::path_util::sqlite_file_dsn(&t.join("db.sqlite")),
    );
    let (addr, _h) = serve_cmd::start(cfg, &t, t.join("src"), "/v1/api".into(), true)
        .await
        .unwrap();
    // insert a×2（builder DML 返回受影响行数）。
    let (s, v) = req(addr, "POST", "/v1/api/u/api/", Some(r#"{"name":"n1"}"#)).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["data"]["n"], 1, "{v}");
    let (s, v) = req(addr, "POST", "/v1/api/u/api/", Some(r#"{"name":"n2"}"#)).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["data"]["n"], 1, "{v}");
    // insert b（aid=1 → 只有 n1 有 b 行）。
    let (s, v) = req(
        addr,
        "PUT",
        "/v1/api/u/api/",
        Some(r#"{"aid":1,"label":"L1"}"#),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["data"]["n"], 1, "{v}");
    // join 全链路：a ⋈ b on a.id=b.aid，n2 被 inner join 过滤。
    let (s, v) = req(addr, "GET", "/v1/api/u/api/", None).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["data"].as_array().map(|a| a.len()), Some(1), "{v}");
    assert_eq!(v["data"][0]["name"], "n1", "{v}");
    assert_eq!(v["data"][0]["label"], "L1", "{v}");
    // 白名单外列名（evil）在 op 侧拒绝：500 信封 + 未落行。
    let (s, v) = req(
        addr,
        "POST",
        "/v1/api/u/api/",
        Some(r#"{"name":"n3","evil":1}"#),
    )
    .await;
    assert_eq!(s, 500, "{v}");
    assert!(v["msg"].as_str().unwrap_or("").contains("evil"), "{v}");
    let (s, v) = req(addr, "GET", "/v1/api/u/api/", None).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["data"].as_array().map(|a| a.len()), Some(1), "{v}");
    let _ = std::fs::remove_dir_all(&t);
}

/// `oj test` 默认库重定向（v0.1.20，U33）：`db_override` 后**建表/种子/fixtures
/// 一并落在 test 库**，开发库（db.default）不被触碰——测试写在开发库上（读到非种子
/// 数据、写坏开发库）是本次要堵的事故面。
#[tokio::test(flavor = "current_thread")]
async fn test_db_override_builds_schema_on_test_db_not_dev() {
    let _g = lock();
    let t = tmp_project(&[
        ("src/u/manifest.yaml", MANIFEST),
        (
            "src/u/schema.yaml",
            "tables:\n\
             \x20 a:\n\
             \x20   pk: id\n\
             \x20   columns:\n\
             \x20     id: { type: integer, autoincrement: true }\n\
             \x20     name: { type: text }\n",
        ),
    ]);
    let mut cfg = base_cfg(&t);
    cfg.db.insert(
        "default".into(),
        oj_plugin_ffi::path_util::sqlite_file_dsn(&t.join("dev.sqlite")),
    );
    cfg.db.insert(
        "test".into(),
        oj_plugin_ffi::path_util::sqlite_file_dsn(&t.join("tst.sqlite")),
    );
    // fixtures=true + db_override="test"：`oj test` 的装配形态。
    let app = oj::app::App::from_config(
        cfg,
        &serde_json::Value::Null,
        &t,
        t.join("src"),
        "/v1/api".into(),
        true,
        true,
        &oj::app::ResourceProfiles {
            db: Some("test".into()),
            ..Default::default()
        },
    )
    .await
    .expect("from_config with db_override");
    drop(app);
    assert!(
        has_table(&t.join("tst.sqlite"), "a").await,
        "test 库必须有表"
    );
    assert!(
        !has_table(&t.join("dev.sqlite"), "a").await,
        "开发库不得被测试装配建表"
    );
    let _ = std::fs::remove_dir_all(&t);
}

/// sqlite 文件里是否存在某表（迁移是否落在预期库的判据）。
async fn has_table(file: &Path, table: &str) -> bool {
    if !file.is_file() {
        return false;
    }
    let dsn = oj_plugin_ffi::path_util::sqlite_file_dsn(file);
    let db = only_js::bridge::SqlxAccessor::arc(&dsn).await.unwrap();
    let rows = db
        .query_with_params(
            "select name from sqlite_master where type='table' and name=?",
            &[serde_json::json!(table)],
        )
        .await
        .unwrap();
    !rows.is_empty()
}

// —— 动态 HTML meta（v0.1.25）：壳 + handler 按路由注入 + per-route 缓存头 —— //

/// 静态站 `app_path` + `server.html_meta_handler`：深链回落送出壳，handler 按**请求路径**
/// 注入 `<title>`/`og:*`（数据驱动的 SEO/IM 预览正解），`server.html_cache_control` 落到
/// 响应头；API 前缀下的 404 依旧不被壳吞掉。
///
/// 静态夹具建在临时项目里（**不用** `sample/dist`——那是构建产物，禁止手改，见 CLAUDE.md）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn html_meta_handler_injects_per_route_tags_end_to_end() {
    let _g = lock();
    let t = tmp_project(&[
        (
            "src/meta/manifest.yaml",
            "name: meta\ndesc: d\nversion: 0.1.0\n",
        ),
        (
            "src/meta/html/api.ts",
            r#"export default {
                 get() {
                   const p = String(http.query.path);
                   json.ok({ title: "issue " + p, "og:title": "OG " + p });
                 },
               };"#,
        ),
    ]);
    let site = t.join("site");
    std::fs::create_dir_all(&site).unwrap();
    std::fs::write(
        site.join("index.html"),
        "<html><head><title>shell</title></head><body>app</body></html>",
    )
    .unwrap();

    let mut cfg = base_cfg(&t);
    cfg.server.app_path = Some(site.canonicalize().unwrap().display().to_string());
    cfg.server.app_spa_fallback = true;
    cfg.server.html_meta_handler = Some("/v1/api/meta/html".into());
    cfg.server.html_cache_control = Some("no-cache".into());
    let (addr, _h) = serve_cmd::start(cfg, &t, t.join("src"), "/v1/api".into(), true)
        .await
        .unwrap();

    let c = reqwest::Client::new();
    let resp = c
        .get(format!("http://{addr}/spaces/demo-issue"))
        .header("accept", "text/html")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some("no-cache")
    );
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("<title>issue /spaces/demo-issue</title>"),
        "按路由注入失败：{body}"
    );
    // 壳里写死的 title 被替换（浏览器/爬虫只认第一个 title）
    assert!(!body.contains("<title>shell</title>"), "{body}");
    assert_eq!(body.matches("<title>").count(), 1, "{body}");
    assert!(
        body.contains(r#"<meta property="og:title" content="OG /spaces/demo-issue">"#),
        "og 注入失败：{body}"
    );
    // API 前缀下的未命中路径不回落（不被壳吞成 200）
    let resp = c
        .get(format!("http://{addr}/v1/api/nope/deep"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let _ = std::fs::remove_dir_all(&t);
}

// —— 多静态站点（v0.1.27）：server.static_sites prefix→dir，最长前缀命中，站内 miss 不跨站 —— //

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_static_sites_serve_by_longest_prefix_end_to_end() {
    let _g = lock();
    let t = tmp_project(&[
        ("src/u/manifest.yaml", MANIFEST),
        (
            "src/u/api.ts",
            "export default { get() { json.ok({ u: 1 }); } };",
        ),
    ]);
    let d1 = t.join("docs");
    let d2 = t.join("web");
    std::fs::create_dir_all(&d1).unwrap();
    std::fs::create_dir_all(&d2).unwrap();
    std::fs::write(d1.join("a.txt"), "DOCS-A").unwrap();
    std::fs::write(d1.join("index.html"), "<h1>docs</h1>").unwrap();
    std::fs::write(d2.join("b.txt"), "WEB-B").unwrap();
    std::fs::write(d2.join("index.html"), "<h1>web</h1>").unwrap();
    // 不跨站探针：仅 web 根有 docs/deep.txt，docs 站没有 → /docs/deep.txt 必须 404。
    std::fs::create_dir_all(d2.join("docs")).unwrap();
    std::fs::write(d2.join("docs/deep.txt"), "MUST-NOT-REACH").unwrap();

    // path 相对 config_dir（t）解析。
    let mut cfg = base_cfg(&t);
    cfg.server.static_sites = vec![
        only_js::config::StaticSiteConf {
            prefix: "/docs".into(),
            path: "docs".into(),
            headers: Default::default(),
        },
        only_js::config::StaticSiteConf {
            prefix: "/".into(),
            path: "web".into(),
            headers: Default::default(),
        },
    ];
    let (addr, _h) = serve_cmd::start(cfg, &t, t.join("src"), "/v1/api".into(), true)
        .await
        .unwrap();

    let c = reqwest::Client::new();
    let get = |p: &str| c.get(format!("http://{addr}{p}")).send();
    // 前缀根 → 该站 index.html；前缀下文件 → 该站内容。
    assert_eq!(
        get("/docs").await.unwrap().text().await.unwrap(),
        "<h1>docs</h1>"
    );
    assert_eq!(
        get("/docs/a.txt").await.unwrap().text().await.unwrap(),
        "DOCS-A"
    );
    // `/` 兜底站。
    assert_eq!(get("/b.txt").await.unwrap().text().await.unwrap(), "WEB-B");
    assert_eq!(
        get("/").await.unwrap().text().await.unwrap(),
        "<h1>web</h1>"
    );
    // 最长命中 /docs 后 miss → 404，不跨站回落 web 根的 docs/deep.txt。
    assert_eq!(get("/docs/deep.txt").await.unwrap().status(), 404);
    let _ = std::fs::remove_dir_all(&t);
}

/// json.redirect 原语端到端：3xx + Location + RFC 9110 §15.4 超文本注记（HEAD 豁免为空 body）。
/// 非 3xx code 由 op 层回落 302。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn json_redirect_302_location_hypertext_note_end_to_end() {
    let _g = lock();
    let t = tmp_project(&[
        ("src/r/manifest.yaml", "name: r\ndesc: d\nversion: 0.1.0\n"),
        (
            "src/r/api.ts",
            r#"function dispatch() {
                 const to = String(http.param("to", "https://example.com/"));
                 const via = http.param("via", "");
                 if (via === "seeOther") json.redirect.seeOther(to);
                 else json.redirect(to, Number(http.param("code", "0")));
               }
               export default { get: dispatch, head: dispatch };"#,
        ),
    ]);
    let cfg = base_cfg(&t);
    let (addr, _h) = serve_cmd::start(cfg, &t, t.join("src"), "/v1/api".into(), true)
        .await
        .unwrap();

    // 不重定向跟随：断言的是本服务吐出的第一跳（302 + Location + 超文本注记）。
    let c = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    // 默认 302：Location 原样回写，body 为含链接的短超文本（text/html）。
    let resp = c
        .get(format!(
            "http://{addr}/v1/api/r/?to=https%3A%2F%2Fcdn.example.com%2Fa.png"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::FOUND);
    assert_eq!(resp.headers()["location"], "https://cdn.example.com/a.png");
    assert!(
        resp.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    let body = resp.text().await.unwrap();
    assert!(
        body.contains(r#"<a href="https://cdn.example.com/a.png">Found</a>."#),
        "超文本注记缺失：{body}"
    );

    // HEAD 请求：§15.4 豁免，body 为空，Location 仍在。
    let resp = c
        .head(format!(
            "http://{addr}/v1/api/r/?to=https%3A%2F%2Fcdn.example.com%2Fa.png"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::FOUND);
    assert_eq!(resp.headers()["location"], "https://cdn.example.com/a.png");
    assert!(resp.bytes().await.unwrap().is_empty());

    // 显式 307 生效；非法 code（0/404 → op 层回落 302）。
    for (q, want) in [("code=307", 307), ("code=0", 302), ("code=404", 302)] {
        let resp = c
            .get(format!(
                "http://{addr}/v1/api/r/?to=https%3A%2F%2Fcdn.example.com%2Fb&{q}"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), want, "{q}");
        let body = resp.text().await.unwrap();
        assert!(body.contains("<a href="), "{q}: {body}");
    }

    // 具名封装（RFC 9110）：seeOther → 303 + See Other 注记。
    let resp = c
        .get(format!(
            "http://{addr}/v1/api/r/?to=https%3A%2F%2Fcdn.example.com%2Fc&via=seeOther"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::SEE_OTHER);
    assert_eq!(resp.headers()["location"], "https://cdn.example.com/c");
    let body = resp.text().await.unwrap();
    assert!(body.contains(">See Other</a>"), "{body}");
    let _ = std::fs::remove_dir_all(&t);
}

/// v0.1.55 exec e2e（输出通道分离）：console.log/info 原样 stdout（无级别前缀，
/// 管道友好）；console.warn/error 与 log.* 走 stderr（带级别标签）；退出码 0；
/// 顶层 throw → 退出码 1 且 stderr 携带 V8 异常消息（spec §3.3/§4）。
/// 最小 config 即可：exec 跳过证书门禁、不声明 db/kv → 内存兜底（spec §3.1 有意分叉）。
#[test]
fn given_exec_script_when_console_then_stdout_direct_and_exit_codes() {
    let _g = lock();
    let tmp = std::env::temp_dir().join(format!("oj-e2e-exec-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(tmp.join("config.yaml"), "{}\n").unwrap();
    let script = tmp.join("hello.ts");
    std::fs::write(
        &script,
        "console.log(\"hello-stdout\"); console.error(\"err-stderr\"); log.info(\"via-log\", \"k\", 1);",
    )
    .unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_oj"))
        .args([
            "exec",
            &script.display().to_string(),
            "-c",
            &tmp.join("config.yaml").display().to_string(),
        ])
        .output()
        .expect("spawn oj exec child");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    // stdout 原样：整行 == 消息本体，无级别前缀（双保险）。
    assert_eq!(stdout.trim(), "hello-stdout", "{stdout}");
    assert!(!stdout.contains("INFO"), "{stdout}");
    // console.error / log.* → stderr（带级别标签，诊断不污染管道）。
    assert!(stderr.contains("err-stderr"), "{stderr}");
    assert!(stderr.contains("ERROR"), "{stderr}");
    assert!(stderr.contains("via-log"), "{stderr}");
    assert!(stderr.contains("INFO"), "{stderr}");
    // 管道友好：tracing 装配日志不进 stdout（只走 stderr）。
    assert!(!stdout.contains("oj::"), "{stdout}");

    // 失败路径：顶层 throw → 退出码 1，stderr 原样透传 V8 异常（不吞不改写）。
    let bad = tmp.join("bad.ts");
    std::fs::write(&bad, "throw new Error(\"exec-e2e-boom\");").unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_oj"))
        .args([
            "exec",
            &bad.display().to_string(),
            "-c",
            &tmp.join("config.yaml").display().to_string(),
        ])
        .output()
        .expect("spawn oj exec child");
    assert_eq!(
        out.status.code(),
        Some(1),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("exec-e2e-boom"), "{stderr}");
    let _ = std::fs::remove_dir_all(&tmp);
}

/// v0.1.55 裸 exec e2e：无 file/-e/--repl 时缺省进 REPL——stdin 管道喂一行后
/// 关闭（EOF 退出），退出码 0，stdout 原样携带脚本打印（同时钉死通道分离）。
#[test]
fn given_bare_exec_when_no_src_then_defaults_to_repl() {
    let _g = lock();
    let tmp = std::env::temp_dir().join(format!("oj-e2e-exec-repl-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(tmp.join("config.yaml"), "{}\n").unwrap();
    use std::io::Write as _;
    use std::process::Stdio;
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_oj"))
        .args(["exec", "-c", &tmp.join("config.yaml").display().to_string()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn oj exec repl child");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"console.log('repl-e2e');\n")
        .unwrap();
    let out = child.wait_with_output().expect("wait oj exec repl child");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("REPL"), "{stdout}");
    assert!(stdout.lines().any(|l| l.trim() == "repl-e2e"), "{stdout}");
    assert!(!stdout.contains("INFO"), "{stdout}");
    let _ = std::fs::remove_dir_all(&tmp);
}

/// v0.1.44 入参契约端到端（**真 HTTP**）：违反 handler 声明的 `.schema` 必须
/// 400 且不进 JS；合规放行；`server.schema_validation: false` 时不校验。
/// 自造最小项目（不依赖 sample），db 隔离到临时目录，证书复用 sample 示例证书。
async fn contract_boot(
    tmp: &Path,
    schema_validation: bool,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let root = sample();
    let cfg = format!(
        "server:\n  host: \"127.0.0.1\"\n  port: 0\n  public_key_path: \"{}\"\n  certificate_path: \"{}\"\n  schema_validation: {}\ndb:\n  default: \"{}\"\n",
        fwd(&root.join("config/public.pem")),
        fwd(&root.join("config/cert.jws")),
        schema_validation,
        oj_plugin_ffi::path_util::sqlite_file_dsn(&tmp.join("db.sqlite")),
    );
    std::fs::write(tmp.join("config.yaml"), &cfg).unwrap();
    let mut c: Config = serde_yaml::from_str(&cfg).unwrap();
    c.server.app_path = None;
    let dir = tmp.join("src");
    serve_cmd::start(c, tmp, dir, "/v1/api".into(), true)
        .await
        .unwrap()
}

fn contract_project(tmp: &Path) {
    // dev 模式要求模块级 manifest.yaml（S005）；本用例无表，tables 留空。
    std::fs::create_dir_all(tmp.join("src/g/_id_")).unwrap();
    std::fs::write(
        tmp.join("src/g/manifest.yaml"),
        "name: \"g\"\ndesc: \"入参契约 e2e 夹具\"\nversion: \"0.1.0\"\ntables: []\n",
    )
    .unwrap();
    std::fs::write(
        tmp.join("src/g/api.ts"),
        "function post() { json.ok({ reached: true }); }\n\
         post.schema = { body: { type: \"object\", required: [\"name\"], \
           properties: { name: { type: \"string\", maxLength: 4 } } } };\n\
         export default { post };\n",
    )
    .unwrap();
    std::fs::write(
        tmp.join("src/g/_id_/api.ts"),
        "function get() { json.ok({ reached: true }); }\n\
         get.schema = { params: { type: \"object\", required: [\"id\"], \
           properties: { id: { type: \"integer\", minimum: 1 } } } };\n\
         export default { get };\n",
    )
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn given_input_contract_when_request_violates_then_400() {
    let _g = lock();
    let tmp = std::env::temp_dir().join(format!("oj-e2e-ct-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    contract_project(&tmp);
    let (addr, h) = contract_boot(&tmp, true).await;

    // 1) body 缺 required → 400，且 handler 未执行（不带 reached）。
    let (st, v) = req(addr, "POST", "/v1/api/g", Some("{}")).await;
    assert_eq!(st, 400, "{v}");
    assert_eq!(v["code"], 400, "{v}");
    assert!(
        v["data"].is_null() && !format!("{v}").contains("reached"),
        "契约失败必须在进 JS 之前拦截: {v}"
    );

    // 2) body 超长 → 400
    let (st, v) = req(addr, "POST", "/v1/api/g", Some("{\"name\":\"toolong\"}")).await;
    assert_eq!(st, 400, "{v}");

    // 3) 路径参数非整数（强转失败）→ 400
    let (st, _) = req(addr, "GET", "/v1/api/g/abc", None).await;
    assert_eq!(st, 400, "非整数路径参数必须被拒");
    // 4) 整数但越界（minimum: 1）→ 400
    let (st, _) = req(addr, "GET", "/v1/api/g/0", None).await;
    assert_eq!(st, 400, "强转后仍受 minimum 约束");

    // 5) 合规：放行并真正执行 handler
    let (st, v) = req(addr, "POST", "/v1/api/g", Some("{\"name\":\"ab\"}")).await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["data"]["reached"], true, "{v}");
    let (st, v) = req(addr, "GET", "/v1/api/g/12", None).await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["data"]["reached"], true, "{v}");

    h.abort();
    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn given_schema_validation_off_when_request_violates_then_passes() {
    // 击穿：开关关闭时即便违反契约也**不得**返回 400（这正是开关的意义）。
    let _g = lock();
    let tmp = std::env::temp_dir().join(format!("oj-e2e-ctoff-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    contract_project(&tmp);
    let (addr, h) = contract_boot(&tmp, false).await;

    let (st, v) = req(addr, "POST", "/v1/api/g", Some("{}")).await;
    assert_eq!(st, 200, "开关关闭时不得校验: {v}");
    assert_eq!(v["data"]["reached"], true, "{v}");

    h.abort();
    let _ = std::fs::remove_dir_all(&tmp);
}

// ---------- fs 轴（v0.1.53）：config fs: 段端到端 ----------

/// 独立临时项目起服务：config 带 fs: 段（root = <tmp>/data），两个 handler 经
/// fs 写/读 note.txt。返回 (addr, handle, tmp)。
async fn boot_fs(readonly: bool) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>, PathBuf) {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = std::env::temp_dir().join(format!("oj-e2e-fs-{}-{}", std::process::id(), n));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("src/fsdemo")).unwrap();
    std::fs::create_dir_all(tmp.join("data")).unwrap();
    std::fs::write(
        tmp.join("src/fsdemo/manifest.yaml"),
        "name: fsdemo\ndesc: fs e2e\nversion: 0.1.0\n",
    )
    .unwrap();
    std::fs::write(
        tmp.join("src/fsdemo/api.ts"),
        "async function post() {\n  await fs.writeTextFile(\"note.txt\", \"hello-e2e\");\n  json.ok({ done: true });\n}\nasync function get() {\n  json.ok({ t: await fs.readTextFile(\"note.txt\") });\n}\nexport default { get, post };\n",
    )
    .unwrap();
    std::fs::write(tmp.join("seed.sql"), "").unwrap();
    let mut cfg = Config::default();
    cfg.server.port = 0;
    let now = serve::test_support::now_secs();
    serve::test_support::write_cert_into(
        &mut cfg.server,
        &tmp,
        now.saturating_sub(3600),
        now + 365 * 86_400,
    );
    cfg.db.insert("default".into(), "sqlite::memory:".into());
    cfg.fs = Some(only_js::config::FsSection {
        root: "data".into(),
        readonly,
    });
    let (addr, h) = serve_cmd::start(cfg, &tmp, tmp.join("src"), "/v1/api".into(), true)
        .await
        .unwrap();
    (addr, h, tmp)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fs_read_write_end_to_end() {
    let _g = lock();
    let (addr, h, tmp) = boot_fs(false).await;
    let (st, v) = req(addr, "POST", "/v1/api/fsdemo/", None).await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["code"], 0, "{v}");
    assert_eq!(v["data"]["done"], true, "{v}");
    // 落盘证据：jail 根（config_dir/data）下真实存在该文件。
    assert_eq!(
        std::fs::read_to_string(tmp.join("data/note.txt")).unwrap(),
        "hello-e2e"
    );
    let (_, v) = req(addr, "GET", "/v1/api/fsdemo/", None).await;
    assert_eq!(v["data"]["t"], "hello-e2e", "{v}");
    h.abort();
    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fs_readonly_denies_write_end_to_end() {
    let _g = lock();
    let (addr, h, tmp) = boot_fs(true).await;
    let (st, v) = req(addr, "POST", "/v1/api/fsdemo/", None).await;
    assert_eq!(st, 500, "未捕获异常走 500 信封: {v}");
    assert_ne!(v["code"], 0, "{v}");
    assert!(v["msg"].to_string().contains("NotCapable"), "{v}");
    // 写确实没落盘。
    assert!(!tmp.join("data/note.txt").exists());
    h.abort();
    let _ = std::fs::remove_dir_all(&tmp);
}

// ---------- 泛型轴通道（T5）：mini-generic 夹具 → bootstrap axis() ----------

/// rustc host triple（serve_cmd 插件装配测试同款形态）。
fn host_triple() -> String {
    let out = std::process::Command::new("rustc")
        .arg("-vV")
        .output()
        .unwrap();
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("host: "))
        .unwrap()
        .to_string()
}

/// 插件存放文件名（= loader plugin_file_name）。
fn plugin_file(name: &str) -> String {
    if cfg!(target_os = "windows") {
        format!("{name}.dll")
    } else if cfg!(target_os = "macos") {
        format!("lib{name}.dylib")
    } else {
        format!("lib{name}.so")
    }
}

/// 编译 mini-generic 泛型轴夹具产物路径（全进程一次；夹具体量小，debug 命中快）。
fn generic_plugin_artifact() -> PathBuf {
    static ONCE: OnceLock<PathBuf> = OnceLock::new();
    ONCE.get_or_init(|| {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        let status = std::process::Command::new("cargo")
            .args(["build", "-p", "oj-plugin-test-mini-generic"])
            .current_dir(&root)
            .status()
            .expect("invoke cargo build for mini-generic");
        assert!(status.success(), "mini-generic build failed");
        let (prefix, ext) = if cfg!(target_os = "windows") {
            ("", "dll")
        } else if cfg!(target_os = "macos") {
            ("lib", "dylib")
        } else {
            ("lib", "so")
        };
        root.join("target/debug")
            .join(format!("{prefix}oj_plugin_test_mini_generic.{ext}"))
    })
    .clone()
}

/// 编译 oj-ldap（release；泛型轴迁移后的真实插件产物）路径（全进程一次）。
fn ldap_plugin_artifact() -> PathBuf {
    static ONCE: OnceLock<PathBuf> = OnceLock::new();
    ONCE.get_or_init(|| {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        let status = std::process::Command::new("cargo")
            .args(["build", "--release", "-p", "oj-ldap"])
            .current_dir(&root)
            .status()
            .expect("invoke cargo build for oj-ldap");
        assert!(status.success(), "oj-ldap build failed");
        let (prefix, ext) = if cfg!(target_os = "windows") {
            ("", "dll")
        } else if cfg!(target_os = "macos") {
            ("lib", "dylib")
        } else {
            ("lib", "so")
        };
        root.join("target/release")
            .join(format!("{prefix}oj_ldap.{ext}"))
    })
    .clone()
}

/// ojInfo() e2e（HTTP 池入口）：serve 装配注入 → handler 内 `ojInfo()` 返回与
/// `oj info` CLI 同源的结构（abi/build/config 声明面）。安全红线：config 只出
/// 段名/键名——vars 的部署值不得出现在响应的任何序列化字段里。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ojinfo_global_available_in_handler() {
    let _g = lock();
    let t = tmp_project(&[
        ("src/i/manifest.yaml", "name: i\ndesc: d\nversion: 0.1.0\n"),
        (
            "src/i/api.ts",
            "export default { get() { json.ok(ojInfo()); } };",
        ),
    ]);
    let mut cfg = base_cfg(&t);
    // 敏感样式部署值：只许以「段名/键名」形态出现，值一律不得泄漏。
    cfg.vars.insert("TOKEN".into(), "secret-value-123".into());
    let (addr, h) = serve_cmd::start(cfg, &t, t.join("src"), "/v1/api".into(), true)
        .await
        .unwrap();

    let (s, v) = req(addr, "GET", "/v1/api/i/", None).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        v["data"]["abi"]["abi_version"],
        oj_plugin_ffi::ABI_VERSION,
        "{v}"
    );
    assert_eq!(
        v["data"]["abi"]["host_fingerprint"],
        oj_plugin_ffi::HOST_FINGERPRINT,
        "{v}"
    );
    assert_eq!(v["data"]["build"]["oj"], env!("CARGO_PKG_VERSION"), "{v}");
    assert!(v["data"]["config"]["sections"].is_array(), "{v}");
    assert!(v["data"]["plugins"].is_array(), "{v}");
    let leaked = serde_json::to_string(&v).unwrap();
    assert!(!leaked.contains("secret-value-123"), "{leaked}");

    h.abort();
    let _ = std::fs::remove_dir_all(&t);
}

/// 泛型轴 e2e：mini-generic 插件（自报轴名 "greet"）扫描装配 → JS 侧
/// `axis("greet").greet("oj")` → `{"hello":"oj"}`；未知轴 `axis("nope").x()`
/// → 500 信封且消息列可用轴名（greet）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generic_axis_greet_end_to_end() {
    let _g = lock();
    let t = tmp_project(&[
        (
            "src/g/manifest.yaml",
            "name: g\ndesc: generic axis e2e\nversion: 0.1.0\n",
        ),
        (
            "src/g/api.ts",
            r#"async function get() {
                 const who = http.query.who;
                 // 无 query 时不传参：插件侧 name 缺省 "world"（空数组序列化路径）。
                 json.ok(await axis("greet").greet(...(who ? [String(who)] : [])));
               }
               export default { get };"#,
        ),
        (
            "src/g/nope/api.ts",
            r#"async function get() {
                 await axis("nope").x();
               }
               export default { get };"#,
        ),
    ]);
    // 夹具拷入隔离插件目录（plugins_dir 指向 <t>/plugins，扫描模式只见到 mini-generic）。
    let pdir = t.join("plugins").join(host_triple());
    std::fs::create_dir_all(&pdir).unwrap();
    std::fs::copy(
        generic_plugin_artifact(),
        pdir.join(plugin_file("mini-generic")),
    )
    .unwrap();
    let mut cfg = base_cfg(&t);
    cfg.plugins_dir = Some(t.join("plugins"));
    let (addr, h) = serve_cmd::start(cfg, &t, t.join("src"), "/v1/api".into(), true)
        .await
        .unwrap();

    // 泛型轴调用全链路：args 经 ojStringify 进、插件 JSON 出信封 data。
    let (s, v) = req(addr, "GET", "/v1/api/g/?who=oj", None).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["data"], serde_json::json!({ "hello": "oj" }), "{v}");
    // 缺省参数：插件侧 name 缺省 "world"。
    let (s, v) = req(addr, "GET", "/v1/api/g/", None).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["data"], serde_json::json!({ "hello": "world" }), "{v}");

    // 未知轴：500 信封 + 消息含轴名与可用轴列表。
    let (s, v) = req(addr, "GET", "/v1/api/g/nope", None).await;
    assert_eq!(s, 500, "{v}");
    let msg = v["msg"].as_str().unwrap_or("");
    assert!(msg.contains("unknown generic axis 'nope'"), "{v}");
    assert!(msg.contains("greet"), "{v}");

    h.abort();
    let _ = std::fs::remove_dir_all(&t);
}

/// oj-ldap 泛型轴迁移 e2e：真实插件产物（generic(ldap) 声明）扫描装配 →
/// JS 侧 `axis("ldap").<op>(...)` 全链路。仓库无 LDAP 测试基建，取舍：
/// 装配 + 协议校验/未知 op 错误路径 + connect 失败链路（127.0.0.1:1 即拒，
/// 证明 axis() → op_axis_call → GenericVtable → 引擎 → ldap3 的端到端通路）在此
/// 验收；成功路径（真实目录返回）由 oj-ldap crate 内单测覆盖协议层，与迁移前同级。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ldap_generic_axis_end_to_end() {
    let _g = lock();
    let t = tmp_project(&[
        (
            "src/l/manifest.yaml",
            "name: l\ndesc: ldap generic axis e2e\nversion: 0.1.0\n",
        ),
        (
            "src/l/api.ts",
            r#"async function get() {
             const op = http.query.op;
             try {
               if (op === "unknown") await axis("ldap").delete();
               else if (op === "badargs") await axis("ldap").bind("uid=eve");
               else if (op === "whoami") await axis("ldap").whoami({ key: "default" });
               else if (op === "noaxis") await axis("nope").x();
               else if (op === "search") {
                 await axis("ldap").search("ou=users,dc=example,dc=com", { scope: "one", attrs: ["uid"] });
               } else json.ok({ done: true });
             } catch (e) {
               json.ok({ err: String(e) });
             }
           }
           export default { get };"#,
        ),
    ]);
    let pdir = t.join("plugins").join(host_triple());
    std::fs::create_dir_all(&pdir).unwrap();
    std::fs::copy(ldap_plugin_artifact(), pdir.join(plugin_file("ldap"))).unwrap();
    let mut cfg = base_cfg(&t);
    cfg.plugins_dir = Some(t.join("plugins"));
    // start() 测试入口 top=Null（config key 段查找不可用）→ 走 plugins.<name> 透传臂
    // （plugin_cfg 第 1 级优先）。实例指向本机端口 1：connect 即 ECONNREFUSED（毫秒级）。
    cfg.plugins.insert(
        "ldap".into(),
        serde_json::json!({ "default": { "url": "ldap://127.0.0.1:1" } }),
    );
    let (addr, h) = serve_cmd::start(cfg, &t, t.join("src"), "/v1/api".into(), true)
        .await
        .unwrap();

    // 泛型轴注册成功 + 插件侧 op 分派：未知 op 错误文案点名已知 op 全集。
    let (s, v) = req(addr, "GET", "/v1/api/l/?op=unknown", None).await;
    assert_eq!(s, 200, "{v}");
    let err = v["data"]["err"].as_str().unwrap_or("");
    assert!(err.contains("ldap: unknown op 'delete'"), "{v}");
    assert!(
        err.contains("bind|search|search_paged|whoami|compare"),
        "{v}"
    );
    // 协议入参校验（泛型通道无宿主校验层，插件侧文案 = 旧宿主层文案）。
    let (s, v) = req(addr, "GET", "/v1/api/l/?op=badargs", None).await;
    assert_eq!(s, 200, "{v}");
    assert!(
        v["data"]["err"]
            .as_str()
            .unwrap_or("")
            .contains("ldap.bind: 'pw' must be a string"),
        "{v}"
    );
    // 端到端通路：whoami → 引擎 connect（127.0.0.1:1 拒绝）→ 错误经 FFI future 回程。
    let (s, v) = req(addr, "GET", "/v1/api/l/?op=whoami", None).await;
    assert_eq!(s, 200, "{v}");
    assert!(
        v["data"]["err"]
            .as_str()
            .unwrap_or("")
            .contains("ldap: connect ldap://127.0.0.1:1"),
        "{v}"
    );
    // search 同样抵达网络层（opts 校验通过后才 connect）。
    let (s, v) = req(addr, "GET", "/v1/api/l/?op=search", None).await;
    assert_eq!(s, 200, "{v}");
    assert!(
        v["data"]["err"]
            .as_str()
            .unwrap_or("")
            .contains("ldap: connect ldap://127.0.0.1:1"),
        "{v}"
    );
    // 未知轴报错列出可用轴（含 ldap）→ 装配注册证据。
    let (s, v) = req(addr, "GET", "/v1/api/l/?op=noaxis", None).await;
    assert_eq!(s, 200, "{v}");
    let err = v["data"]["err"].as_str().unwrap_or("");
    assert!(err.contains("unknown generic axis 'nope'"), "{v}");
    assert!(err.contains("ldap"), "{v}");

    h.abort();
    let _ = std::fs::remove_dir_all(&t);
}
