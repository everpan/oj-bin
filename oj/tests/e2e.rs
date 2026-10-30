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
    boot_inner(dev, |_| {}).await.unwrap()
}

/// boot 的可调变体：tweak 在装配前修改 config（MCP/RPC 端点门控等用例用）。
/// 返回 Result 供「装配期 fail-fast」用例直接断言 Err。
async fn boot_inner(
    dev: bool,
    tweak: impl FnOnce(&mut Config),
) -> Result<(std::net::SocketAddr, tokio::task::JoinHandle<()>, PathBuf), String> {
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
    // sample/config.yaml 的 web 挂载指向 dist（gitignore，CI 新克隆无此目录）且
    // api 挂载固定 src——boot() 按本用例的 dev/release 目录重挂：清掉 sample 挂载，
    // 由 start() 注入 (dir, /v1/api) 一条（UC 不覆盖静态兜底，多站点静态有专属 e2e，
    // 见 multi_static_sites_serve_by_longest_prefix_end_to_end）。
    cfg.mounts.clear();
    // 平台特权挂载（v0.1.75 ojm 批次 D4 起 sample/config.yaml 带 platform 段）：
    // platform.root 须经 mounts[].api 接入，否则启动即 fail-fast。boot 清掉 sample
    // 挂载后必须补回 /_platform → platform-root（源码目录，dev/release 双模式都以
    // ts 挂载服务——mixed mounts 逐挂载判模式）。
    cfg.mounts.push(only_js::config::MountConf {
        prefix: "/_platform".into(),
        api: Some(
            root.join("platform-root")
                .canonicalize()
                .unwrap()
                .display()
                .to_string(),
        ),
        web: None,
        spa: None,
        headers: Default::default(),

        ..Default::default()
    });
    tweak(&mut cfg);
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
            config: Some("config.yaml".into()),
            dir: root.join("src").display().to_string(),
            module_root: None,
            out: dist.display().to_string(),
            minify: true,
            check: false,
            bundle: false,
        })
        .await
        .unwrap();
        dist
    };
    // release 语义（README 部署故事）：build 后先 `oj migrate` 再 server——
    // release verify 门禁要求账本与产物齐平，空库首启必须先显式迁移。
    if !dev {
        let acc = only_js::bridge::DbBackendRegistry::builtin()
            .connect(cfg.db.get("default").unwrap(), &root)
            .await
            .unwrap();
        oj::migrate::apply_all(Some(&acc), &dir, false, false, None)
            .await
            .unwrap();
    }
    serve_cmd::start(cfg, &root, dir, "/v1/api".into())
        .await
        .map_err(|e| e.to_string())
        .map(|(addr, h)| (addr, h, tmp))
}

async fn req(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> (u16, serde_json::Value) {
    let c = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap();
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
    let (addr, _h) = serve_cmd::start(cfg, &t, t.join("src"), "/v1/api".into())
        .await
        .unwrap();
    let before = transpile_hits();
    for _ in 0..3 {
        let (_, v) = req(addr, "GET", "/v1/api/u/f/", None).await;
        assert_eq!(v["data"]["v"], 1);
    }
    // 启动内省已预热转译缓存：3 次请求 0 次新转译（缓存全局共享，跨 actor）。
    assert_eq!(transpile_hits(), before);
    // 热重载：改文件 → mtime 变 → 下次请求新结果。mtime 粒度在个别文件系统
    // （HFS+ / 容器卷挂载）可达秒级，固定 sleep 赌不动 → deadline 轮询。
    std::fs::write(
        t.join("src/u/f/api.ts"),
        "export default { get() { json.ok({ v: 2 }); } };\n",
    )
    .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let v = loop {
        let (_, v) = req(addr, "GET", "/v1/api/u/f/", None).await;
        if v["data"]["v"] == 2 || std::time::Instant::now() >= deadline {
            break v;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
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
    let e = serve_cmd::start(base_cfg(&t), &t, t.join("src"), "/v1/api".into())
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
        config: Some("config.yaml".into()),
        dir: t.join("src").display().to_string(),
        module_root: None,
        out: t.join("dist").display().to_string(),
        minify: true,
        check: false,
        bundle: false,
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
    let (addr, _h) = serve_cmd::start(base_cfg(&t), &t, t.join("dist"), "/v1/api".into())
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
    let (addr, _h) = serve_cmd::start(base_cfg(&t), &t, t.join("src"), "/v1/api".into())
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
        config: Some("config.yaml".into()),
        dir: t.join("src").display().to_string(),
        module_root: None,
        out: t.join("dist").display().to_string(),
        minify: true,
        check: false,
        bundle: false,
    };
    oj::build_cmd::run(&a).await.unwrap();
    let routes = std::fs::read_to_string(t.join("dist/u-0.1.0/routes.js")).unwrap();
    assert!(routes.contains("\"u/{id}\""), "{routes}");
    assert!(routes.contains("\"_id_/api.js\""), "{routes}");
    let (addr, _h) = serve_cmd::start(base_cfg(&t), &t, t.join("dist"), "/v1/api".into())
        .await
        .unwrap();
    let (s, v) = req(addr, "GET", "/v1/api/u/42", None).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["data"]["id"], "42", "{v}");
    let _ = std::fs::remove_dir_all(&t);
}

/// v0.1.64 嵌套 handler 组：`{ post, upload: { post } }` + 组 `.route`，
/// dev 与 release 两态同命中（双态不变量，验收 1/11）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nested_handler_group_dev_and_release_serve_end_to_end() {
    let _g = lock();
    let t = tmp_project(&[
        ("src/u/manifest.yaml", MANIFEST),
        (
            "src/u/item/api.ts",
            "async function post() { json.ok({ where: \"post\" }); }\n\
             post.route = \"/x/{anchor}\";\n\
             const upload = { post: async () => { json.ok({ where: \"upload.post\" }); } };\n\
             upload.route = \"/x/{anchor}/upload\";\n\
             export default { post, upload };\n",
        ),
    ]);
    // dev 态
    let (addr, _h) = serve_cmd::start(base_cfg(&t), &t, t.join("src"), "/v1/api".into())
        .await
        .unwrap();
    let (s, v) = req(addr, "POST", "/v1/api/x/a1", None).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["data"]["where"], "post", "{v}");
    let (s, v) = req(addr, "POST", "/v1/api/x/a1/upload", None).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["data"]["where"], "upload.post", "{v}");
    // 组路径只声明了 POST → GET 得 405
    let (s, _) = req(addr, "GET", "/v1/api/x/a1/upload", None).await;
    assert_eq!(s, 405);
    // 目录镜像不复活（组动词 replaced；且 item 目录无 api.ts 之外的声明）
    let (s, _) = req(addr, "GET", "/v1/api/u/item", None).await;
    assert_eq!(s, 404);
    drop(_h);

    // release 态：build → dist 直载，行为逐一相等
    let a = BuildArgs {
        module: Some("u".into()),
        config: Some("config.yaml".into()),
        dir: t.join("src").display().to_string(),
        module_root: None,
        out: t.join("dist").display().to_string(),
        minify: true,
        check: false,
        bundle: false,
    };
    oj::build_cmd::run(&a).await.unwrap();
    let routes = std::fs::read_to_string(t.join("dist/u-0.1.0/routes.js")).unwrap();
    assert!(routes.contains("handler: \"upload.post\""), "{routes}");
    assert!(!routes.contains("handler: \"post\""), "{routes}"); // 顶层动词不落链
    let (addr, _h) = serve_cmd::start(base_cfg(&t), &t, t.join("dist"), "/v1/api".into())
        .await
        .unwrap();
    let (s, v) = req(addr, "POST", "/v1/api/x/a1", None).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["data"]["where"], "post", "{v}");
    let (s, v) = req(addr, "POST", "/v1/api/x/a1/upload", None).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["data"]["where"], "upload.post", "{v}");
    let (s, _) = req(addr, "GET", "/v1/api/x/a1/upload", None).await;
    assert_eq!(s, 405);
    let (s, _) = req(addr, "GET", "/v1/api/u/item", None).await;
    assert_eq!(s, 404);
    let _ = std::fs::remove_dir_all(&t);
}

/// 组 `.route` 后目录镜像 404（replaced 经真实 fs 兜底层判定）+ 非标识符段名
/// （`user-list`）计算式访问命中（验收 12 前半）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nested_group_route_suppresses_mirror_and_hyphen_segment_hits() {
    let _g = lock();
    let t = tmp_project(&[
        ("src/u/manifest.yaml", MANIFEST),
        (
            "src/u/item/api.ts",
            "const cccc = { get: () => { json.ok({ where: \"cccc.get\" }); } };\n\
             cccc.route = \"/y\";\n\
             const userList = { get: () => { json.ok({ where: \"user-list.get\" }); } };\n\
             export default { cccc, \"user-list\": userList };\n",
        ),
    ]);
    let (addr, _h) = serve_cmd::start(base_cfg(&t), &t, t.join("src"), "/v1/api".into())
        .await
        .unwrap();
    let (s, v) = req(addr, "GET", "/v1/api/y", None).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["data"]["where"], "cccc.get", "{v}");
    // 镜像 URL 被 replaced 拦（表内无 + 兜底不复活）→ 404
    let (s, _) = req(addr, "GET", "/v1/api/u/item", None).await;
    assert_eq!(s, 404);
    // 连字符组名：无 .route → {父前缀}/user-list
    let (s, v) = req(addr, "GET", "/v1/api/u/item/user-list", None).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["data"]["where"], "user-list.get", "{v}");
    drop(_h);

    let a = BuildArgs {
        module: Some("u".into()),
        config: Some("config.yaml".into()),
        dir: t.join("src").display().to_string(),
        module_root: None,
        out: t.join("dist").display().to_string(),
        minify: true,
        check: false,
        bundle: false,
    };
    oj::build_cmd::run(&a).await.unwrap();
    let (addr, _h) = serve_cmd::start(base_cfg(&t), &t, t.join("dist"), "/v1/api".into())
        .await
        .unwrap();
    let (s, v) = req(addr, "GET", "/v1/api/y", None).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["data"]["where"], "cccc.get", "{v}");
    let (s, _) = req(addr, "GET", "/v1/api/u/item", None).await;
    assert_eq!(s, 404);
    let (s, v) = req(addr, "GET", "/v1/api/u/item/user-list", None).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["data"]["where"], "user-list.get", "{v}");
    let _ = std::fs::remove_dir_all(&t);
}

/// §二.A 示例端到端：组同时含动词与子组（4 路由）；`cccc.get.schema` 元数据隔离
/// （契约只作用于该 handler，且不生成 `schema` 路由）（验收 7/8）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nested_group_mixed_verbs_and_subgroups_with_schema_isolation() {
    let _g = lock();
    let t = tmp_project(&[
        ("src/u/manifest.yaml", MANIFEST),
        (
            "src/u/aaa/bbb/api.ts",
            "function get() { json.ok({ where: \"get\" }); }\n\
             const cccc = {\n\
               get: () => { json.ok({ where: \"cccc.get\", need: http.query.need }); },\n\
               post: () => { json.ok({ where: \"cccc.post\" }); },\n\
               list: { get: () => { json.ok({ where: \"cccc.list.get\" }); } },\n\
             };\n\
             cccc.get.schema = { query: { type: \"object\", required: [\"need\"],\n\
               properties: { need: { type: \"string\" } } } };\n\
             export default { get, cccc };\n",
        ),
    ]);
    for mode in ["src", "dist"] {
        if mode == "dist" {
            let a = BuildArgs {
                module: Some("u".into()),
                config: Some("config.yaml".into()),
                dir: t.join("src").display().to_string(),
                module_root: None,
                out: t.join("dist").display().to_string(),
                minify: true,
                check: false,
                bundle: false,
            };
            oj::build_cmd::run(&a).await.unwrap();
        }
        let (addr, _h) = serve_cmd::start(base_cfg(&t), &t, t.join(mode), "/v1/api".into())
            .await
            .unwrap();
        let (s, v) = req(addr, "GET", "/v1/api/u/aaa/bbb", None).await;
        assert_eq!(s, 200, "{mode} {v}");
        assert_eq!(v["data"]["where"], "get", "{mode}");
        let (s, v) = req(addr, "POST", "/v1/api/u/aaa/bbb/cccc", None).await;
        assert_eq!(s, 200, "{mode} {v}");
        assert_eq!(v["data"]["where"], "cccc.post", "{mode}");
        let (s, v) = req(addr, "GET", "/v1/api/u/aaa/bbb/cccc/list", None).await;
        assert_eq!(s, 200, "{mode} {v}");
        assert_eq!(v["data"]["where"], "cccc.list.get", "{mode}");
        // 契约键按链隔离：cccc.get 缺 need → 400 且不进 JS
        let (s, _) = req(addr, "GET", "/v1/api/u/aaa/bbb/cccc", None).await;
        assert_eq!(s, 400, "{mode}");
        let (s, v) = req(addr, "GET", "/v1/api/u/aaa/bbb/cccc?need=1", None).await;
        assert_eq!(s, 200, "{mode} {v}");
        assert_eq!(v["data"]["where"], "cccc.get", "{mode}");
        // 元数据不生成路由：不存在名为 schema 的子路径
        let (s, _) = req(addr, "GET", "/v1/api/u/aaa/bbb/cccc/schema", None).await;
        assert_eq!(s, 404, "{mode}");
        drop(_h);
    }
    let _ = std::fs::remove_dir_all(&t);
}

/// routes.js 手改 `handler` 非法段（原型注入）→ release 启动失败（§四 双保险，验收 12 后半）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn release_routes_js_illegal_handler_chain_fails_startup() {
    let _g = lock();
    let t = tmp_project(&[
        ("src/u/manifest.yaml", MANIFEST),
        (
            "src/u/item/api.ts",
            "const cccc = { get: () => { json.ok({}); } };\n\
             export default { cccc };\n",
        ),
    ]);
    let a = BuildArgs {
        module: Some("u".into()),
        config: Some("config.yaml".into()),
        dir: t.join("src").display().to_string(),
        module_root: None,
        out: t.join("dist").display().to_string(),
        minify: true,
        check: false,
        bundle: false,
    };
    oj::build_cmd::run(&a).await.unwrap();
    let rjs = t.join("dist/u-0.1.0/routes.js");
    let mut s = std::fs::read_to_string(&rjs).unwrap();
    // 手改产物：把合法链替换成原型注入段（替换整个 handler 值，避免重复键后者生效）
    s = s.replace("handler: \"cccc.get\"", "handler: \"cccc.__proto__\"");
    assert!(s.contains("cccc.__proto__"), "{s}");
    std::fs::write(&rjs, s).unwrap();
    let e = serve_cmd::start(base_cfg(&t), &t, t.join("dist"), "/v1/api".into())
        .await
        .err()
        .unwrap_or_default();
    assert!(e.contains("handler"), "{e}");
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
        config: Some("config.yaml".into()),
        dir: t.join("src").display().to_string(),
        module_root: None,
        out: t.join("dist").display().to_string(),
        minify: true,
        check: false,
        bundle: false,
    })
    .await
    .unwrap();
    // manifests.yaml 两键
    let lock = oj::manifest::load_lock(&t.join("dist/manifests.yaml")).unwrap();
    assert_eq!(lock.len(), 2, "{lock:?}");
    assert_eq!(lock["other"], "0.9.0");
    let (addr, _h) = serve_cmd::start(base_cfg(&t), &t, t.join("dist"), "/v1/api".into())
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
    let (addr, _h) = serve_cmd::start(base_cfg(&t), &t, t.join("dist"), "/v1/api".into())
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
    // 有锁才进 release 腿（目录即模式）；锁指向的产物目录缺失 routes.js → 提示 oj build。
    let t = tmp_project(&[
        ("dist/manifests.yaml", "u: 0.1.0\n"),
        ("dist/u-0.1.0/manifest.yaml", MANIFEST),
    ]);
    let e = serve_cmd::start(base_cfg(&t), &t, t.join("dist"), "/v1/api".into())
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
    let (addr, _h) = serve_cmd::start(base_cfg(&t), &t, t.join("src"), "/v1/api".into())
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
    let (addr, _h) = serve_cmd::start(base_cfg(&t), &t, t.join("src"), "/v1/api".into())
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
    let (addr, _h) = serve_cmd::start(cfg, &t, t.join("src"), "/v1/api".into())
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
    let (addr, _h) = serve_cmd::start(cfg, &t, t.join("src"), "/v1/api".into())
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
    // fixtures=true + db_override="test"：`oj test` 的装配形态（v0.1.58：api 目录进挂载）。
    cfg.mounts.push(only_js::config::MountConf {
        prefix: "/v1/api".into(),
        api: Some(t.join("src").to_string_lossy().into_owned()),
        web: None,
        spa: None,
        headers: Default::default(),

        ..Default::default()
    });
    let app = oj::app::App::from_config(
        cfg,
        &serde_json::Value::Null,
        &t,
        true,
        &oj::app::ResourceProfiles {
            db: Some("test".into()),
            ..Default::default()
        },
        None,
        None,
        None,
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
    cfg.mounts.push(only_js::config::MountConf {
        prefix: "/".into(),
        api: None,
        web: Some(site.canonicalize().unwrap().display().to_string()),
        spa: Some(true),
        headers: Default::default(),

        ..Default::default()
    });
    cfg.server.html_meta_handler = Some("/v1/api/meta/html".into());
    cfg.server.html_cache_control = Some("no-cache".into());
    let (addr, _h) = serve_cmd::start(cfg, &t, t.join("src"), "/v1/api".into())
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
    cfg.mounts = vec![
        only_js::config::MountConf {
            prefix: "/docs".into(),
            api: None,
            web: Some("docs".into()),
            spa: None,
            headers: Default::default(),

            ..Default::default()
        },
        only_js::config::MountConf {
            prefix: "/".into(),
            api: None,
            web: Some("web".into()),
            spa: None,
            headers: Default::default(),

            ..Default::default()
        },
    ];
    let (addr, _h) = serve_cmd::start(cfg, &t, t.join("src"), "/v1/api".into())
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
    let (addr, _h) = serve_cmd::start(cfg, &t, t.join("src"), "/v1/api".into())
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
    let c: Config = serde_yaml::from_str(&cfg).unwrap();
    let dir = tmp.join("src");
    serve_cmd::start(c, tmp, dir, "/v1/api".into())
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
    // v0.1.70：声明了响应契约但**故意不按它返回**——用作「response 不是守卫」的反向击穿。
    std::fs::create_dir_all(tmp.join("src/g/r")).unwrap();
    std::fs::write(
        tmp.join("src/g/r/api.ts"),
        "function get() { json.ok({ reached: true }); }\n\
         get.schema = { response: { type: \"object\", required: [\"nope\"], \
           properties: { nope: { type: \"string\" } } } };\n\
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn given_response_contract_when_response_violates_then_still_200() {
    // 击穿（**反向**）：`response` 是声明、不是守卫——违反它不得变成 400/500。
    // 响应由 JS 产生，校验失败时请求已执行完、无处可去（拿它当硬约束会把
    // 「少返回一个字段」升级成 500 级故障）。
    let _g = lock();
    let tmp = std::env::temp_dir().join(format!("oj-e2e-resp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    contract_project(&tmp);
    let (addr, h) = contract_boot(&tmp, true).await;

    // 声明了必填 `nope`，实际只返回 `reached`——必须照常 200 且不被裁剪。
    let (st, v) = req(addr, "GET", "/v1/api/g/r", None).await;
    assert_eq!(st, 200, "response 声明不得拦请求: {v}");
    assert_eq!(v["data"]["reached"], true, "{v}");
    assert!(
        v["data"].get("nope").is_none(),
        "声明不得反过来裁剪/补写响应: {v}"
    );

    h.abort();
    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn given_invalid_schema_when_boot_then_fails_even_with_validation_off() {
    // 击穿：声明**合法性**审计不受 `server.schema_validation` 开关影响。
    // 开关管「是否执行运行期校验」，不管「声明是否合法」——逃生门是给「规则误杀
    // 合法流量」准备的，不是给代码写错准备的。拼错通道以前静默不生效（接口裸奔）。
    let _g = lock();
    let tmp = std::env::temp_dir().join(format!("oj-e2e-badschema-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    contract_project(&tmp);
    // 改成拼错的通道名（`bodys`）。
    std::fs::write(
        tmp.join("src/g/api.ts"),
        "function post() { json.ok({ reached: true }); }\n\
         post.schema = { bodys: { type: \"object\" } };\n\
         export default { post };\n",
    )
    .unwrap();

    let root = sample();
    let cfg = format!(
        "server:\n  host: \"127.0.0.1\"\n  port: 0\n  public_key_path: \"{}\"\n  certificate_path: \"{}\"\n  schema_validation: false\ndb:\n  default: \"{}\"\n",
        fwd(&root.join("config/public.pem")),
        fwd(&root.join("config/cert.jws")),
        oj_plugin_ffi::path_util::sqlite_file_dsn(&tmp.join("db.sqlite")),
    );
    std::fs::write(tmp.join("config.yaml"), &cfg).unwrap();
    let c: Config = serde_yaml::from_str(&cfg).unwrap();
    let err = serve_cmd::start(c, &tmp, tmp.join("src"), "/v1/api".into())
        .await
        .expect_err("非法 .schema 必须让启动失败，即使 schema_validation: false");
    assert!(
        err.contains("bodys") && err.contains("unsupported channel"),
        "报错必须指向拼错的通道: {err}"
    );

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
    let (addr, h) = serve_cmd::start(cfg, &tmp, tmp.join("src"), "/v1/api".into())
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
    let (addr, h) = serve_cmd::start(cfg, &t, t.join("src"), "/v1/api".into())
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
    let (addr, h) = serve_cmd::start(cfg, &t, t.join("src"), "/v1/api".into())
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
    let (addr, h) = serve_cmd::start(cfg, &t, t.join("src"), "/v1/api".into())
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

/// 配置热更新 v1 端到端（spec 2026-10-12）：`server.reload_api: true` 起服务，
/// 经真实 TCP 验证 health / config GET / POST 幂等 / 改段后 applied 清单 / 再 POST 幂等。
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn given_reload_api_when_post_config_then_swaps_and_reports_diff() {
    let _g = lock();
    let tmp = std::env::temp_dir().join(format!("oj-e2e-reload-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let root = sample();
    // config 必须落在 sample 项目根内（loader 的 project_root 钳制要求 api 在根内，
    // 否则别名解析 `#_shared/...` 报 escapes project root）——故写入 sample 内临时文件，
    // 相对路径即可（config_dir = sample）。db 用独立临时文件隔离。
    // 固定端口（e2e 全用例串行持锁，无并发冲突）：避免解析 stdout（pipe 下 stdout 行缓冲
    // 不保证及时送达），改用 health 端点轮询探活。
    let cfg_path = root.join(".oj-reload-e2e.yaml");
    let _ = std::fs::remove_file(&cfg_path);
    // reload_api: true；证书复用 sample 示例证书（相对 config_dir）；api 挂载 src（相对）。
    let cfg = format!(
        "server:\n  host: \"127.0.0.1\"\n  port: 19978\n  reload_api: true\n  public_key_path: \"config/public.pem\"\n  certificate_path: \"config/cert.jws\"\ndb:\n  default: \"{}\"\nmounts:\n  - prefix: \"/v1/api\"\n    api: \"src\"\nvars:\n  FOO: \"bar\"\n",
        oj_plugin_ffi::path_util::sqlite_file_dsn(&tmp.join("db.sqlite")),
    );
    std::fs::write(&cfg_path, &cfg).unwrap();

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_oj"))
        .args(["serve", "-c", &cfg_path.display().to_string()])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn oj serve child");

    // stdout/stderr 收集进同一缓冲（仅用于失败诊断，不用于就绪探测）。
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
            for l in std::io::BufReader::new(stream)
                .lines()
                .map_while(Result::ok)
            {
                buf.lock().unwrap().push_str(&format!("{l}\n"));
            }
        }));
    }

    // 等就绪：轮询 health 端点（≤120s；首启含全 sample 转译 + 迁移）。
    // 直接用 reqwest 容忍连接拒绝（server 未起时），`req` 辅助会因 connect 失败 unwrap 而 panic。
    let addr: std::net::SocketAddr = "127.0.0.1:19978".parse().unwrap();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    let health_url = format!("http://{addr}/v1/api/_system/health");
    loop {
        let ready = match client.get(&health_url).send().await {
            Ok(r) => r.status().as_u16() == 200,
            Err(_) => false,
        };
        if ready {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "server not ready in time:\n{}",
            out_buf.lock().unwrap()
        );
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }

    // health（绕过内层证书门禁，运维窗口可探活）。
    let (st, _body) = req(addr, "GET", "/v1/api/_system/health", None).await;
    assert_eq!(st, 200, "health");

    // config GET：初始代际 0。
    let (st, body) = req(addr, "GET", "/v1/api/_system/config", None).await;
    assert_eq!(st, 200, "config get");
    assert_eq!(body["generation"].as_u64(), Some(0), "{body}");

    // POST 空 body：首次与落盘一致 → unchanged（代际不变）。
    let (st, body) = req(addr, "POST", "/v1/api/_system/config", None).await;
    assert_eq!(st, 200, "config post unchanged");
    assert_eq!(body["msg"].as_str(), Some("unchanged"), "{body}");
    assert_eq!(body["generation"].as_u64(), Some(0), "{body}");

    // 改 vars 段 → 重写落盘 → POST：applied 含 "vars"，代际推进到 1。
    let cfg2 = cfg.replace("FOO: \"bar\"", "FOO: \"baz\"");
    std::fs::write(&cfg_path, &cfg2).unwrap();
    let (st, body) = req(addr, "POST", "/v1/api/_system/config", None).await;
    assert_eq!(st, 200, "config post applied");
    assert_eq!(body["code"].as_i64(), Some(0), "{body}");
    let applied = body["data"]["applied"].as_array().expect("applied array");
    assert!(
        applied.iter().any(|v| v.as_str() == Some("vars")),
        "vars should be in applied: {body}"
    );
    assert_eq!(body["data"]["generation"].as_u64(), Some(1), "{body}");

    // 再 POST（无改动）→ 仍 unchanged，代际保持 1。
    let (st, body) = req(addr, "POST", "/v1/api/_system/config", None).await;
    assert_eq!(st, 200, "config post idempotent");
    assert_eq!(body["msg"].as_str(), Some("unchanged"), "{body}");
    assert_eq!(body["generation"].as_u64(), Some(1), "{body}");

    // 停机。
    let _ = std::process::Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status();
    let _ = child.wait();
    // 清理 sample 内临时 config。
    let _ = std::fs::remove_file(&cfg_path);
}

/// 经真实 TCP 验证 §6 段 diff 三清单：改 `vars`（applied）/ `db`（restart_pending）/
/// `server`（kept_listener）三类段，POST 后代际推进且三清单分类正确；v1 仅上报不实际重启，
/// 监听器仍绑定原端口（kept_listener 语义）。
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn given_reload_api_when_backend_and_listener_segments_change_then_classified() {
    let _g = lock();
    let tmp = std::env::temp_dir().join(format!("oj-e2e-reload2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let root = sample();
    let cfg_path = root.join(".oj-reload-e2e2.yaml");
    let _ = std::fs::remove_file(&cfg_path);
    let db1 = oj_plugin_ffi::path_util::sqlite_file_dsn(&tmp.join("db1.sqlite"));
    let db2 = oj_plugin_ffi::path_util::sqlite_file_dsn(&tmp.join("db2.sqlite"));
    let cfg = format!(
        "server:\n  host: \"127.0.0.1\"\n  port: 19978\n  reload_api: true\n  public_key_path: \"config/public.pem\"\n  certificate_path: \"config/cert.jws\"\ndb:\n  default: \"{}\"\nmounts:\n  - prefix: \"/v1/api\"\n    api: \"src\"\nvars:\n  FOO: \"bar\"\n",
        db1,
    );
    std::fs::write(&cfg_path, &cfg).unwrap();

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_oj"))
        .args(["serve", "-c", &cfg_path.display().to_string()])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn oj serve child");

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
            for l in std::io::BufReader::new(stream)
                .lines()
                .map_while(Result::ok)
            {
                buf.lock().unwrap().push_str(&format!("{l}\n"));
            }
        }));
    }

    let addr: std::net::SocketAddr = "127.0.0.1:19978".parse().unwrap();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    let health_url = format!("http://{addr}/v1/api/_system/health");
    loop {
        let ready = match client.get(&health_url).send().await {
            Ok(r) => r.status().as_u16() == 200,
            Err(_) => false,
        };
        if ready {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "server not ready in time:\n{}",
            out_buf.lock().unwrap()
        );
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }

    // 改三类段：`vars`（applied）/ `db` DSN 变更（restart_pending）/ `server.port` 变更
    // （kept_listener；监听器不动，仍走 19978）。
    let cfg2 = cfg
        .replace("FOO: \"bar\"", "FOO: \"baz\"")
        .replace(&db1, &db2)
        .replace("port: 19978", "port: 19111");
    std::fs::write(&cfg_path, &cfg2).unwrap();
    let (st, body) = req(addr, "POST", "/v1/api/_system/config", None).await;
    assert_eq!(st, 200, "config post classified");
    assert_eq!(body["code"].as_i64(), Some(0), "{body}");
    let d = &body["data"];
    let applied = d["applied"].as_array().expect("applied array");
    let restart_pending = d["restart_pending"]
        .as_array()
        .expect("restart_pending array");
    let kept_listener = d["kept_listener"].as_array().expect("kept_listener array");
    assert!(
        applied.iter().any(|v| v.as_str() == Some("vars")),
        "vars in applied: {body}"
    );
    assert!(
        restart_pending.iter().any(|v| v.as_str() == Some("db")),
        "db in restart_pending: {body}"
    );
    assert!(
        kept_listener.iter().any(|v| v.as_str() == Some("server")),
        "server in kept_listener: {body}"
    );
    assert_eq!(d["generation"].as_u64(), Some(1), "{body}");

    // kept_listener：监听器仍绑定原端口，health 仍可探活。
    let (st, _body) = req(addr, "GET", "/v1/api/_system/health", None).await;
    assert_eq!(st, 200, "listener kept on old port after server-seg change");

    // 停机。
    let _ = std::process::Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status();
    let _ = child.wait();
    let _ = std::fs::remove_file(&cfg_path);
}

// ----- ojm PR-3：server.reloadConfig() 进程内热更（设计稿 §3；BDD S1–S6） -----

/// reloadConfig e2e 夹具：独立临时项目——`src/reloadplain`（无特权探针）+
/// `platform-modules/reloadinst`（受控根特权模块），双 api 挂载 + platform 段 +
/// fs 段 + vars 段 + reload_api。证书复用 sample 示例证书（拷入临时根）。
/// 返回 (临时根, config 路径, config 原文)。
/// 使用者均为下方 `#[cfg(unix)]` reload 用例（Windows 编译下避免 dead_code）。
#[cfg(unix)]
fn reload_platform_fixture(name: &str, port: u16) -> (PathBuf, PathBuf, String) {
    let raw = std::env::temp_dir().join(format!("oj-e2e-reloadplat-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&raw);
    std::fs::create_dir_all(&raw).unwrap();
    let root = raw.canonicalize().unwrap();
    // 证书（sample 示例证书拷入；路径相对 config_dir）。
    std::fs::create_dir_all(root.join("config")).unwrap();
    std::fs::copy(
        sample().join("config/public.pem"),
        root.join("config/public.pem"),
    )
    .unwrap();
    std::fs::copy(
        sample().join("config/cert.jws"),
        root.join("config/cert.jws"),
    )
    .unwrap();
    // fs root（目录须存在）。
    std::fs::create_dir_all(root.join("data")).unwrap();
    // 无特权探针模块（市场安装树 src/）。
    std::fs::create_dir_all(root.join("src/reloadplain/probe")).unwrap();
    std::fs::write(
        root.join("src/reloadplain/manifest.yaml"),
        "name: reloadplain\ndesc: reload probe\nversion: 0.1.0\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/reloadplain/probe/api.ts"),
        r#"export default {
  async get() {
    try {
      const r = await server.reloadConfig();
      json.ok({ reloaded: r });
    } catch (e) {
      json.ok({ err: String(e) });
    }
  },
};
"#,
    )
    .unwrap();
    // 特权模块（受控根 platform-modules/）。dev 内省要求根级只放动词键：
    // 并发 pair 场景放独立子目录（pair/api.ts）。
    std::fs::create_dir_all(root.join("platform-modules/reloadinst/reload")).unwrap();
    std::fs::create_dir_all(root.join("platform-modules/reloadinst/pair")).unwrap();
    std::fs::write(
        root.join("platform-modules/reloadinst/manifest.yaml"),
        "name: reloadinst\ndesc: installer\nversion: 0.1.0\n",
    )
    .unwrap();
    std::fs::write(
        root.join("platform-modules/reloadinst/reload/api.ts"),
        r#"export default {
  async get() {
    try {
      const r = await server.reloadConfig();
      json.ok({ reloaded: r });
    } catch (e) {
      json.ok({ err: String(e) });
    }
  },
};
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("platform-modules/reloadinst/pair/api.ts"),
        r#"export default {
  async get() {
    const p1 = server.reloadConfig();
    const p2 = server.reloadConfig();
    const fmt = (r) =>
      r.status === "fulfilled" ? r : { status: "rejected", reason: String(r.reason) };
    const [r1, r2] = await Promise.allSettled([p1, p2]);
    json.ok({ pair: [fmt(r1), fmt(r2)] });
  },
};
"#,
    )
    .unwrap();
    let dsn = oj_plugin_ffi::path_util::sqlite_file_dsn(&root.join("data/db.sqlite"));
    let cfg = format!(
        "server:\n  host: \"127.0.0.1\"\n  port: {port}\n  reload_api: true\n  public_key_path: \"config/public.pem\"\n  certificate_path: \"config/cert.jws\"\ndb:\n  default: \"{dsn}\"\nmounts:\n  - prefix: \"/v1/api\"\n    api: \"src\"\n  - prefix: \"/pf/api\"\n    api: \"platform-modules\"\nplatform:\n  modules: [\"reloadinst\"]\n  root: \"./platform-modules\"\n  trusted_paths: [\"./config.yaml\"]\nfs:\n  root: \"./data\"\n  readonly: false\nvars:\n  FOO: \"bar\"\n",
    );
    let cfg_path = root.join("config.yaml");
    std::fs::write(&cfg_path, &cfg).unwrap();
    (root, cfg_path, cfg)
}

/// spawn `oj serve` 子进程并轮询 health 至就绪（≤120s）。返回 (child, 诊断缓冲)。
#[cfg(unix)]
async fn boot_reload_serve(
    cfg_path: &Path,
    port: u16,
) -> (
    std::process::Child,
    std::sync::Arc<std::sync::Mutex<String>>,
) {
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_oj"))
        .args(["serve", "-c", &cfg_path.display().to_string()])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn oj serve child");
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
            for l in std::io::BufReader::new(stream)
                .lines()
                .map_while(Result::ok)
            {
                buf.lock().unwrap().push_str(&format!("{l}\n"));
            }
        }));
    }
    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    let health_url = format!("http://{addr}/pf/api/_system/health");
    loop {
        let ready =
            matches!(client.get(&health_url).send().await, Ok(r) if r.status().as_u16() == 200);
        if ready {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "server not ready in time:\n{}",
            out_buf.lock().unwrap()
        );
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
    (child, out_buf)
}

// 仅 reload 相关 unix 用例使用（`kill -TERM` 不可移植；Windows 编译下避免 dead_code）。
#[cfg(unix)]
async fn stop_reload_serve(child: &mut std::process::Child) {
    let _ = std::process::Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status();
    let _ = child.wait();
}

/// reloadConfig e2e 专用请求：返回 (status, 原文)——空体不炸（诊断可见）。
#[cfg(unix)]
async fn req_raw(addr: std::net::SocketAddr, method: &str, path: &str) -> (u16, String) {
    let c = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap();
    let r = c
        .request(
            reqwest::Method::from_bytes(method.as_bytes()).unwrap(),
            format!("http://{addr}{path}"),
        )
        .send()
        .await
        .expect("request send");
    let st = r.status().as_u16();
    (st, r.text().await.unwrap_or_default())
}

/// req_raw + JSON 解码（空体归 Null，失败信息由上层断言携带）。
#[cfg(unix)]
async fn req_rel(addr: std::net::SocketAddr, method: &str, path: &str) -> (u16, serde_json::Value) {
    let (st, txt) = req_raw(addr, method, path).await;
    (
        st,
        serde_json::from_str(&txt).unwrap_or(serde_json::Value::Null),
    )
}

/// BDD S1/S2/S5/S6（特权 JS 回调路径）：非特权模块 NotCapable；特权模块改盘上
/// vars 段触发 reloadConfig → applied 含段名 + generation+1；再触发 unchanged；
/// 并发双触发一方 busy（文案点名）。
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn given_reload_api_when_privileged_module_calls_reload_config_then_applies() {
    let _g = lock();
    let (root, cfg_path, cfg) = reload_platform_fixture("js", 19988);
    let (mut child, out_buf) = boot_reload_serve(&cfg_path, 19988).await;
    let addr: std::net::SocketAddr = "127.0.0.1:19988".parse().unwrap();

    // S5：非特权模块 → NotCapable: not platform-privileged。
    let (st, body) = req_rel(addr, "GET", "/v1/api/reloadplain/probe/").await;
    assert_eq!(st, 200, "probe: {body}");
    let err = body["data"]["err"].as_str().expect("err string");
    assert!(err.contains("NotCapable"), "{err}");
    assert!(err.contains("not platform-privileged"), "{err}");

    // S1：改盘上 vars 段 → 特权模块 reloadConfig → applied 含段名、generation+1。
    let cfg2 = cfg.replace("FOO: \"bar\"", "FOO: \"baz\"");
    std::fs::write(&cfg_path, &cfg2).unwrap();
    let (st, body) = req_rel(addr, "GET", "/pf/api/reloadinst/reload/").await;
    assert_eq!(st, 200, "reload1: {body}\n{}", out_buf.lock().unwrap());
    let r = &body["data"]["reloaded"];
    assert_eq!(r["unchanged"], serde_json::json!(false), "{body}");
    assert_eq!(r["generation"], serde_json::json!(1), "{body}");
    let applied = r["applied"].as_array().expect("applied array");
    assert!(
        applied.iter().any(|v| v.as_str() == Some("vars")),
        "vars in applied: {body}"
    );

    // S2：盘上无变化再触发 → unchanged=true、generation 不动、三清单空数组。
    let (st, body) = req_rel(addr, "GET", "/pf/api/reloadinst/reload/").await;
    assert_eq!(st, 200, "reload2: {body}");
    let r = &body["data"]["reloaded"];
    assert_eq!(r["unchanged"], serde_json::json!(true), "{body}");
    assert_eq!(r["generation"], serde_json::json!(1), "{body}");
    assert_eq!(r["applied"], serde_json::json!([]), "{body}");
    assert_eq!(r["restart_pending"], serde_json::json!([]), "{body}");
    assert_eq!(r["kept_listener"], serde_json::json!([]), "{body}");

    // S6：并发双触发 → 一方 busy 报错（文案点名，非静默排队）。先改盘上配置，
    // 让先抢到锁的一方走完整影子装配（秒级），busy 窗口确定相交。
    let cfg3 = cfg2.replace("FOO: \"baz\"", "FOO: \"qux\"");
    std::fs::write(&cfg_path, &cfg3).unwrap();
    let (st, body) = req_rel(addr, "GET", "/pf/api/reloadinst/pair/").await;
    assert_eq!(st, 200, "pair: {body}");
    let pair = body["data"]["pair"].as_array().expect("pair array");
    assert_eq!(pair.len(), 2);
    let ok = pair
        .iter()
        .find(|r| r["status"] == serde_json::json!("fulfilled"))
        .expect("one fulfilled");
    assert_eq!(ok["value"]["generation"], serde_json::json!(2), "{body}");
    let busy = pair
        .iter()
        .find(|r| r["status"] == serde_json::json!("rejected"))
        .expect("one rejected");
    let reason = busy["reason"].as_str().expect("reason string");
    assert!(reason.contains("busy"), "{reason}");
    assert!(reason.contains("reload in progress"), "{reason}");

    stop_reload_serve(&mut child).await;
    let _ = std::fs::remove_dir_all(&root);
}

/// BDD S3/S4（安全验收项，自提权封堵）：改 platform / fs 段 → HTTP POST 返回
/// restart_pending 含段名且**不热生效**；撤销特权后特权模块仍可调用（授予未变，
/// 须重启生效）。
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn given_reload_api_when_privilege_segments_change_then_restart_pending_and_grants_unchanged()
{
    let _g = lock();
    let (root, cfg_path, cfg) = reload_platform_fixture("seg", 19989);
    let (mut child, out_buf) = boot_reload_serve(&cfg_path, 19989).await;
    let addr: std::net::SocketAddr = "127.0.0.1:19989".parse().unwrap();

    // S4：改 fs 段（readonly）→ restart_pending 含 fs，不进 applied。
    let cfg2 = cfg.replace("readonly: false", "readonly: true");
    std::fs::write(&cfg_path, &cfg2).unwrap();
    let (st, body) = req_rel(addr, "POST", "/v1/api/_system/config").await;
    assert_eq!(st, 200, "fs seg: {body}\n{}", out_buf.lock().unwrap());
    assert_eq!(body["code"], 0, "{body}");
    assert_eq!(body["data"]["generation"], 1, "{body}");
    let rp = body["data"]["restart_pending"].as_array().expect("rp");
    assert!(rp.iter().any(|v| v.as_str() == Some("fs")), "{body}");
    assert!(
        !body["data"]["applied"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v.as_str() == Some("fs")),
        "{body}"
    );

    // S3：改 platform 段（撤销 reloadinst 特权）→ restart_pending 含 platform，
    // 不热生效——击穿断言：reload 后特权授予未变（特权模块仍可 reloadConfig，
    // 且因盘上无再变化走 unchanged 幂等）。
    let cfg3 = cfg2.replace("modules: [\"reloadinst\"]", "modules: []");
    std::fs::write(&cfg_path, &cfg3).unwrap();
    let (st, body) = req_rel(addr, "POST", "/v1/api/_system/config").await;
    assert_eq!(st, 200, "platform seg: {body}");
    assert_eq!(body["code"], 0, "{body}");
    assert_eq!(body["data"]["generation"], 2, "{body}");
    let rp = body["data"]["restart_pending"].as_array().expect("rp");
    assert!(rp.iter().any(|v| v.as_str() == Some("platform")), "{body}");
    assert!(
        !body["data"]["applied"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v.as_str() == Some("platform")),
        "{body}"
    );
    // 击穿断言：撤销特权的配置已落盘且 reload 已跑，但特权授予未热生效。
    let (st, body) = req_rel(addr, "GET", "/pf/api/reloadinst/reload/").await;
    assert_eq!(st, 200, "privileged after revoke: {body}");
    let r = &body["data"]["reloaded"];
    assert_eq!(
        r["unchanged"],
        serde_json::json!(true),
        "grants must not hot apply: {body}"
    );
    assert_eq!(r["generation"], serde_json::json!(2), "{body}");

    stop_reload_serve(&mut child).await;
    let _ = std::fs::remove_dir_all(&root);
}

// —— http.module：模块路由分发注入只读模块身份（v0.1.76）—— //

/// dev + release 双模式：GET /v1/api/user/module-info/ 回显 http.module
/// {name, version}。dev 版本取 src/<m>/manifest.yaml，release 取
/// dist/<m>-<v>/manifest.yaml——双 boot 同值（版本一致性门禁的运行期验证）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_module_injected_in_dev_and_release() {
    let _g = lock();
    for dev in [true, false] {
        let (addr, _h, _t) = boot(dev).await;
        let (s, v) = req(addr, "GET", "/v1/api/user/module-info/", None).await;
        assert_eq!(s, 200, "dev={dev}: {v}");
        assert_eq!(v["data"]["module"]["name"], "user", "dev={dev}: {v}");
        assert_eq!(v["data"]["module"]["version"], "0.1.0", "dev={dev}: {v}");
        if !dev {
            let _ = std::fs::remove_dir_all(sample().join(".e2e-dist"));
        }
    }
}

/// 挂载根级直置 api（不在任何模块目录内）→ http.module === null
/// （serve 栈透传 bridge 派生结果，非请求来源判定）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_module_null_for_root_level_api() {
    let _g = lock();
    let t = tmp_project(&[(
        "src/api.ts",
        "export default { get() { json.ok({ mod: http.module }); } };\n",
    )]);
    let (addr, _h) = serve_cmd::start(base_cfg(&t), &t, t.join("src"), "/v1/api".into())
        .await
        .unwrap();
    let (s, v) = req(addr, "GET", "/v1/api/", None).await;
    assert_eq!(s, 200, "{v}");
    assert!(v["data"]["mod"].is_null(), "{v}");
    let _ = std::fs::remove_dir_all(&t);
}

/// 页面 meta 合成派发（serve 内部 run_module，validate=false）：handler 在模块
/// 目录内 → 合成派发同样携带 http.module 身份（合成派发继承所属模块，非 null）。
/// title 由 http.module 拼成，经壳注入断言。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn meta_dispatch_injects_module_identity() {
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
                   const m = http.module;
                   json.ok({ title: "mod:" + (m === null ? "null" : m.name + ":" + m.version) });
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
    cfg.mounts.push(only_js::config::MountConf {
        prefix: "/".into(),
        api: None,
        web: Some(site.canonicalize().unwrap().display().to_string()),
        spa: Some(true),
        headers: Default::default(),

        ..Default::default()
    });
    cfg.server.html_meta_handler = Some("/v1/api/meta/html".into());
    let (addr, _h) = serve_cmd::start(cfg, &t, t.join("src"), "/v1/api".into())
        .await
        .unwrap();
    let c = reqwest::Client::new();
    let resp = c
        .get(format!("http://{addr}/spaces/demo"))
        .header("accept", "text/html")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("<title>mod:meta:0.1.0</title>"),
        "合成派发未携带模块身份：{body}"
    );
    let _ = std::fs::remove_dir_all(&t);
}

/// 对抗性（v0.1.76）：strict ESM handler 写 http.module → 500 信封携 TypeError
/// ——冻结只读经完整 serve 栈生效（不止 bridge 单测）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_module_frozen_enforced_through_serve_stack() {
    let _g = lock();
    let t = tmp_project(&[
        ("src/u/manifest.yaml", MANIFEST),
        (
            "src/u/probe/api.ts",
            r#"export default {
                 get() { http.module.name = "evil"; json.ok({ mod: http.module }); },
               };"#,
        ),
    ]);
    let (addr, _h) = serve_cmd::start(base_cfg(&t), &t, t.join("src"), "/v1/api".into())
        .await
        .unwrap();
    let (s, v) = req(addr, "GET", "/v1/api/u/probe/", None).await;
    assert_eq!(s, 500, "{v}");
    assert!(
        v["msg"].as_str().unwrap_or("").contains("read only"),
        "strict 写冻结对象应经 500 信封回传 TypeError：{v}"
    );
    let _ = std::fs::remove_dir_all(&t);
}

// ── MCP / 裸 JSON-RPC 端点（spec 2026-10-30，R5/R7/R8/R9）──────────────────

/// MCP/JSON-RPC 端点请求辅助：406 门拒绝 reqwest 默认的 `Accept: */*`，必须显式声明。
async fn rpc_req(addr: std::net::SocketAddr, path: &str, body: &str) -> (u16, serde_json::Value) {
    let c = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap();
    let resp = c
        .post(format!("http://{addr}{path}"))
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap())
}

/// Given server.mcp 未配置 When POST {base}/mcp Then 404（tool 面默认不外露，R8）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_e2e_disabled_gate_404() {
    let _g = lock();
    let (addr, _h, _t) = boot(true).await;
    let (s, _v) = rpc_req(
        addr,
        "/v1/api/mcp",
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"e2e","version":"0"}}}"#,
    )
    .await;
    assert_eq!(s, 404);
}

/// Given mcp.enabled=true、无 auth 插件、allow_anonymous=false When start
/// Then 装配期 fail-fast（R7：绝不静默暴露匿名 catalog）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_e2e_enabled_without_auth_fails_startup() {
    let _g = lock();
    let err = boot_inner(true, |c| {
        c.server.mcp = Some(only_js::config::McpCfg {
            enabled: true,
            ..Default::default()
        });
    })
    .await
    .unwrap_err();
    assert!(err.contains("server.mcp"), "{err}");
}

/// Given allow_anonymous=true When initialize → tools/list → tools/call
/// Then 协商回显 + capabilities/serverInfo 齐备 + 已知 tool 在列 +
/// call 成功为 content+structuredContent（无 isError）；未知 tool 为 -32601。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_e2e_full_flow() {
    let _g = lock();
    let (addr, _h, _t) = boot_inner(true, |c| {
        c.server.mcp = Some(only_js::config::McpCfg {
            enabled: true,
            allow_anonymous: true,
            modules: None,
        });
    })
    .await
    .unwrap();
    // initialize：协议协商 + 能力声明（严格客户端凭 capabilities 发 tools/list）。
    let (s, v) = rpc_req(
        addr,
        "/v1/api/mcp",
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"e2e","version":"0"}}}"#,
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["result"]["protocolVersion"], "2025-06-18");
    assert!(v["result"]["capabilities"]["tools"].is_object(), "{v}");
    assert!(v["result"]["serverInfo"]["name"].is_string(), "{v}");
    // tools/list：sample 的 user/account 路由必须出现在 catalog。
    let (s, v) = rpc_req(
        addr,
        "/v1/api/mcp",
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let tools = v["result"]["tools"].as_array().expect("tools 数组：{v}");
    let get_tool = tools
        .iter()
        .find(|t| t["name"].as_str().unwrap_or("").contains("user_account"))
        .unwrap_or_else(|| panic!("catalog 缺 user_account 系 tool：{v}"));
    // tools/call：query 参数（字符串值）直透 envelope data。
    let name = get_tool["name"].as_str().unwrap();
    let call = format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"{name}","arguments":{{"query":{{"id":"1"}}}}}}}}"#
    );
    let (s, v) = rpc_req(addr, "/v1/api/mcp", &call).await;
    assert_eq!(s, 200, "{v}");
    let r = &v["result"];
    assert!(r.get("isError").is_none(), "成功不应带 isError：{v}");
    assert_eq!(r["content"][0]["type"], "text");
    assert!(
        r["structuredContent"].is_array() || r["structuredContent"].is_object(),
        "{v}"
    );
    // 未知 tool：JSON-RPC 方法级错误（-32601），不是 isError。
    let (s, v) = rpc_req(
        addr,
        "/v1/api/mcp",
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"no_such_tool","arguments":{}}}"#,
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(v["error"]["code"], -32601, "{v}");
}

/// Given 业务 handler 返回 code!=0（POST 缺 name → 400 信封）
/// When tools/call Then HTTP 200 + isError:true + 业务 msg（R3：非 JSON-RPC error）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_e2e_business_error_is_iserror_result() {
    let _g = lock();
    let (addr, _h, _t) = boot_inner(true, |c| {
        c.server.mcp = Some(only_js::config::McpCfg {
            enabled: true,
            allow_anonymous: true,
            modules: None,
        });
    })
    .await
    .unwrap();
    // 先取 user_account 系 POST tool 的真实净化名。
    let (_s, v) = rpc_req(
        addr,
        "/v1/api/mcp",
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#,
    )
    .await;
    let tools = v["result"]["tools"].as_array().unwrap();
    // POST tool：非只读（GET 带 readOnlyHint）。
    let post_tool = tools
        .iter()
        .find(|t| {
            t["name"].as_str().unwrap_or("").contains("user_account")
                && t["annotations"].get("readOnlyHint").is_none()
        })
        .unwrap_or_else(|| panic!("无 user_account 写 tool：{v}"));
    let name = post_tool["name"].as_str().unwrap();
    let call = format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"{name}","arguments":{{"body":{{}}}}}}}}"#
    );
    let (s, v) = rpc_req(addr, "/v1/api/mcp", &call).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["result"]["isError"], true, "{v}");
    let text = v["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("name"), "业务 msg 应透传：{v}");
}

/// Given modules 白名单与任何模块都不匹配 When tools/list Then 空列表
///（R9 fail-closed：绝不回落全集）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_e2e_modules_whitelist_fail_closed() {
    let _g = lock();
    let (addr, _h, _t) = boot_inner(true, |c| {
        c.server.mcp = Some(only_js::config::McpCfg {
            enabled: true,
            allow_anonymous: true,
            modules: Some(vec!["no_such_module".into()]),
        });
    })
    .await
    .unwrap();
    let (s, v) = rpc_req(
        addr,
        "/v1/api/mcp",
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#,
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["result"]["tools"].as_array().unwrap().len(), 0, "{v}");
}

/// Given server.rpc.enabled=true When POST {base}/rpc（method=operationId）
/// Then 裸 JSON-RPC result 直透 envelope data；业务错误为 error 对象（取业务 code）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_e2e_rpc_parity() {
    let _g = lock();
    let (addr, _h, _t) = boot_inner(true, |c| {
        c.server.rpc.enabled = true;
        c.server.rpc.allow_anonymous = true; // e2e 不装配 auth（装配门 v0.1.110 终审）
    })
    .await
    .unwrap();
    // GET 成功：result = envelope.data（list 数组）。
    let (s, v) = rpc_req(
        addr,
        "/v1/api/rpc",
        r#"{"jsonrpc":"2.0","id":1,"method":"user_account_get","params":{"query":{"id":"1"}}}"#,
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert!(v["result"].is_array() || v["result"].is_object(), "{v}");
    // 业务错误（POST 缺 name → 400 信封）：error 对象，code = 业务 code。
    let (s, v) = rpc_req(
        addr,
        "/v1/api/rpc",
        r#"{"jsonrpc":"2.0","id":2,"method":"user_account_post","params":{"body":{}}}"#,
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_ne!(v["error"]["code"], 0, "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("name"),
        "{v}"
    );
    // 未开门时 404（默认 off）。
    let (addr2, _h2, _t2) = boot(true).await;
    let (s, _v) = rpc_req(
        addr2,
        "/v1/api/rpc",
        r#"{"jsonrpc":"2.0","id":1,"method":"user_account_get","params":{}}"#,
    )
    .await;
    assert_eq!(s, 404);
}

/// A 线路冒烟（env-gated：`OJ_E2E_NODE` 未设则诚实 skip 并打印提示）。
/// Given node ≥18 可用且 e2e server（auth 关）已起
/// When `oj mcp` 渲染产物以 stdio 驱动 initialize → tools/list → tools/call
/// Then 协商回显 + catalog 含已知 tool + call 成功为 structuredContent（真 HTTP 回环）
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generated_stdio_server_handshakes() {
    if std::env::var("OJ_E2E_NODE").is_err() {
        eprintln!("SKIP generated_stdio_server_handshakes: set OJ_E2E_NODE=1 (needs node >= 18)");
        return;
    }
    let _g = lock();
    let (addr, _h, tmp) = boot(true).await;
    // 渲染：对 sample src 离线生成（不经 serve 端点，纯 codegen 路径）。
    let out = tmp.join("mcp-server.js");
    let code = oj::mcp_cmd::run(&oj::args::McpArgs {
        config: Some(sample().join("config.yaml").display().to_string()),
        site: None,
        dir: Some(sample().join("src").display().to_string()),
        module_root: None,
        base: None,
        out: Some(out.display().to_string()),
    })
    .await
    .unwrap();
    assert_eq!(code, 0);
    assert!(out.is_file());
    // spawn stdio server。
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let mut child = tokio::process::Command::new("node")
        .arg(&out)
        .env("OJ_BASE", format!("http://{addr}"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn node（OJ_E2E_NODE 已设：须可用）");
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let send = async |stdin: &mut tokio::process::ChildStdin, msg: &str| {
        stdin.write_all(msg.as_bytes()).await.unwrap();
        stdin.write_all(b"\n").await.unwrap();
        stdin.flush().await.unwrap();
    };
    // initialize → 协商回显 + capabilities。
    send(&mut stdin, r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}"#).await;
    let v: serde_json::Value =
        serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(v["result"]["protocolVersion"], "2025-06-18", "{v}");
    assert!(v["result"]["capabilities"]["tools"].is_object(), "{v}");
    // tools/list → catalog 含 user_account_get。
    send(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
    )
    .await;
    let v: serde_json::Value =
        serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    let names: Vec<&str> = v["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"user_account_get"), "{names:?}");
    // tools/call（query id=1）→ 成功无 isError，structuredContent = envelope data。
    send(&mut stdin, r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"user_account_get","arguments":{"query":{"id":"1"}}}}"#).await;
    let v: serde_json::Value =
        serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    let r = &v["result"];
    assert!(r.get("isError").is_none(), "{v}");
    assert_eq!(r["content"][0]["type"], "text", "{v}");
    assert!(r["structuredContent"].is_array(), "{v}");
    drop(stdin);
    let _ = child.wait().await;
}

/// 评审盲区补钉：inputSchema 公布的嵌套形状（arguments.path）必须能真调通
/// 参数化路由（终审 blocker 修复的回归钉——曾 schema 平铺公布、执行嵌套读取）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_e2e_path_param_tool_roundtrip() {
    let _g = lock();
    let (addr, _h, _t) = boot_inner(true, |c| {
        c.server.mcp = Some(only_js::config::McpCfg {
            enabled: true,
            allow_anonymous: true,
            modules: None,
        });
    })
    .await
    .unwrap();
    // tools/list：参数化路由的 tool 公布嵌套 schema（path 键、id 在键内 required）。
    let (s, v) = rpc_req(
        addr,
        "/v1/api/mcp",
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#,
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let tools = v["result"]["tools"].as_array().unwrap();
    let t = tools
        .iter()
        .find(|t| t["name"] == "user__id__get")
        .unwrap_or_else(|| panic!("缺 user__id__get：{v}"));
    assert_eq!(t["inputSchema"]["required"], serde_json::json!(["path"]));
    assert_eq!(
        t["inputSchema"]["properties"]["path"]["required"],
        serde_json::json!(["id"])
    );
    // tools/call：按公布的嵌套形状发 path 参数 → 命中参数化路由成功。
    let (s, v) = rpc_req(
        addr,
        "/v1/api/mcp",
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"user__id__get","arguments":{"path":{"id":"1"}}}}"#,
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert!(
        v["result"].get("isError").is_none(),
        "嵌套 path 参数必须调通：{v}"
    );
}

/// REQ-11 设计 §7 Task 0 验证（env-gated PG）：客户端挂断后，在飞查询被
/// cancel token 联动取消——pg_stat_activity 里的 pg_sleep 须在断开 ≤5s 内消失
/// （不依赖语句自带超时；无 guard 时它会跑满 30s，本用例轮询 10s 后红）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disconnect_cancels_running_query_within_5s() {
    let _g = lock();
    let url = match std::env::var("OJ_TEST_PG") {
        Ok(u) if !u.is_empty() => u,
        _ => {
            eprintln!("skip: OJ_TEST_PG unset");
            return;
        }
    };
    // 独立 fixture：tmp/src/sleep/api.ts（dev 模式直接服务该目录）。
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = std::env::temp_dir().join(format!("oj-e2e-disc-{}-{}", std::process::id(), n));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("src/sleep")).unwrap();
    std::fs::write(
        tmp.join("src/sleep/manifest.yaml"),
        "name: \"sleep\"\ndesc: \"REQ-11 disconnect e2e fixture\"\nversion: \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(
        tmp.join("src/sleep/api.ts"),
        r#"async function get() {
  try {
    const rows = await db.query("select pg_sleep(30) as s");
    return json.ok({ n: rows.length });
  } catch (e) {
    return json.fail(500, "sleep failed: " + (e && e.message ? e.message : e));
  }
}
export default { get };
"#,
    )
    .unwrap();
    // 监控经服务自身查 pg_stat_activity（根 crate 无 PG 驱动；插件轴已载）。
    std::fs::create_dir_all(tmp.join("src/probe")).unwrap();
    std::fs::write(
        tmp.join("src/probe/manifest.yaml"),
        "name: \"probe\"\ndesc: \"REQ-11 disconnect e2e probe\"\nversion: \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(
        tmp.join("src/probe/api.ts"),
        r#"async function get() {
  const rows = await db.query(
    "select count(*)::int4 as n from pg_stat_activity where state = 'active' and query like '%pg_sleep(30)%' and pid <> pg_backend_pid()");
  return json.ok({ n: rows[0].n });
}
export default { get };
"#,
    )
    .unwrap();
    let mut cfg: Config = serde_yaml::from_str(
        &std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../sample/config.yaml"),
        )
        .unwrap(),
    )
    .unwrap();
    cfg.server.port = 0;
    cfg.db.insert("default".into(), url.clone());
    cfg.tenant = Default::default();
    cfg.auth = None;
    cfg.mounts.clear();
    cfg.platform = None;
    cfg.blob = None;
    cfg.oidc = None;
    cfg.http_sso = None;
    cfg.smtp = None;
    cfg.ldap = None;
    cfg.es.clear();
    cfg.secrets.private_key_path = None;
    cfg.secrets.public_key_path = None;
    cfg.fs = None;
    let secs = serve::test_support::now_secs();
    serve::test_support::write_cert_into(
        &mut cfg.server,
        &tmp,
        secs.saturating_sub(3600),
        secs + 365 * 86_400,
    );
    let (addr, _h) = serve_cmd::start(cfg, &tmp, tmp.join("src"), "/v1/api".into())
        .await
        .unwrap();
    // probe 长超时客户端：actor 池轮询可能把 probe 派到正被 sleep job 占用的 worker
    // 上排队（队前 job 最久 30s），短超时会误判——probe 语义只信实际响应。
    let probe_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(35))
        .build()
        .unwrap();
    let probe_active = |addr: std::net::SocketAddr| {
        let c = probe_client.clone();
        async move {
            let v: serde_json::Value = c
                .get(format!("http://{addr}/v1/api/probe/"))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            v["data"]["n"].as_i64().unwrap_or(-1)
        }
    };
    // 预检：probe 自身可服务（断开前 pg_sleep 未在跑 → 0）。
    assert_eq!(probe_active(addr).await, 0, "probe pre-check");
    // 裸 TCP 发请求（connection: close）——500ms 后 drop socket = 确定的 FIN。
    // 不能用 reqwest 丢 future：连接可能回池复用，服务端看不到断开。
    use tokio::io::AsyncWriteExt as _;
    let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
    sock.write_all(b"GET /v1/api/sleep/ HTTP/1.1\r\nhost: oj-e2e\r\nconnection: close\r\n\r\n")
        .await
        .unwrap();
    // 500ms 内若服务端已回包 = handler 提前结束（错误路径），抓出来看。
    let mut early = Vec::new();
    let read = async {
        use tokio::io::AsyncReadExt as _;
        let mut buf = [0u8; 4096];
        loop {
            match sock.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => early.extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }
    };
    tokio::select! {
        _ = read => {}
        _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {}
    }
    assert!(
        early.is_empty(),
        "sleep handler returned early: {}",
        String::from_utf8_lossy(&early)
    );
    // 断开前确认 sleep 已在飞（probe 看到 1；排队 probe 会等到 sleep 结束，仍返回真值）。
    let t1 = std::time::Instant::now();
    loop {
        let n = probe_active(addr).await;
        if n == 1 {
            break;
        }
        assert!(
            t1.elapsed() <= std::time::Duration::from_secs(5),
            "pg_sleep(30) never observed in flight"
        );
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    drop(sock); // FIN：hyper 侧应感知连接死亡 → guard 触发 cancel_by_token
    let t0 = std::time::Instant::now();
    let deadline = std::time::Duration::from_secs(10);
    loop {
        let active = probe_active(addr).await;
        if active == 0 {
            let el = t0.elapsed();
            assert!(
                el <= std::time::Duration::from_secs(5),
                "cancel took {:?} (>5s) after disconnect",
                el
            );
            return;
        }
        if t0.elapsed() > deadline {
            panic!("pg_sleep(30) still active 10s after disconnect — guard not wired");
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}
