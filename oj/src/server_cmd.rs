//! oj server 装配：config → 逐 db 开库（仅 sqlite）→ seed → manifest 校验 →
//! actor 池 → axum serve。start() 返回 (addr, join_handle)，main 与测试共用。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::net::ToSocketAddrs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use mdm_base_rust::bridge::{
    Bridge, DataAccessor, InMemoryKV, LoaderShared, SchemaRegistry, SqlxAccessor,
};
use mdm_base_rust::config::{self, Config};
use mdm_server::actor::JsActor;
use mdm_server::routes;

use crate::args::ServerArgs;
use crate::manifest;

pub async fn run(a: ServerArgs) -> Result<(), String> {
    let config_path = PathBuf::from(&a.config);
    let config_dir = config_dir_of(&config_path);
    let cfg = config::load_from(&config_dir, config_path.file_name().and_then(|s| s.to_str()))
        .map_err(|e| format!("load config: {e}"))?;
    let (addr, h) = start(cfg, &config_dir, PathBuf::from(&a.dir), a.base.clone(), a.dev).await?;
    println!(
        "oj server listening on http://{addr}{} (dir={}, {})",
        a.base, a.dir, if a.dev { "dev/ts" } else { "release/js" }
    );
    h.await.map_err(|e| format!("server task: {e}"))
}

/// 装配并监听（port=0 → 随机端口，测试用）。
pub async fn start(
    cfg: Config,
    config_dir: &Path,
    dir: PathBuf,
    base: String,
    ts: bool,
) -> Result<(SocketAddr, tokio::task::JoinHandle<()>), String> {
    for (name, url) in &cfg.redis {
        eprintln!("warn: redis '{name}' ({url}) configured but served by in-memory KV (v0.1)");
    }
    // 逐 db 开库：v0.1 仅 sqlite，其余 fail-fast。
    let mut dbs: HashMap<String, Arc<dyn DataAccessor>> = HashMap::new();
    for (name, dsn) in &cfg.db {
        let acc = SqlxAccessor::arc(&resolve_sqlite(dsn, config_dir)?)
            .await
            .map_err(|e| format!("open db '{name}': {e}"))?;
        dbs.insert(name.clone(), acc);
    }
    // 项目根 seed.sql（存在则对 default 库执行，语句按 ';' 切分——ponytail: seed 内不得有分号字面量）。
    let seed = config_dir.join("seed.sql");
    if seed.is_file() {
        let text = std::fs::read_to_string(&seed).map_err(|e| format!("read seed: {e}"))?;
        if let Some(db) = dbs.get("default") {
            for stmt in text.split(';').map(str::trim).filter(|s| !s.is_empty()) {
                db.exec_with_params(stmt, &[]).await.map_err(|e| format!("seed: {e}"))?;
            }
        }
    }
    // 绝对化 dir（Bridge loader 的 project_root 用 config_dir，api 相对 dir）。
    let dir = dir.canonicalize().unwrap_or(dir);
    let loader = Arc::new(LoaderShared {
        project_root: config_dir.canonicalize().unwrap_or_else(|_| config_dir.to_path_buf()),
        ts,
    });
    let kv = Arc::new(InMemoryKV::new());
    // 路由表：dev 启动内省 .route 声明（设计 §2）；release 聚合 dist/manifests.yaml（spec §3）。
    let make_bridge = {
        let (dbs, kv, loader) = (dbs.clone(), kv.clone(), loader.clone());
        move || {
            Bridge::with_dbs_and_loader(
                dbs.clone(),
                kv.clone(),
                SchemaRegistry::new(),
                false,
                Some(loader.clone()),
            )
        }
    };
    let (table, failures) = if ts {
        // manifest 校验 + 路由表打印（UC-8）。release 的版本目录命名 `m-v` 过不了
        // name==dirname 校验，模块清单改在下方锁循环里打印。
        for m in manifest::load_modules(&dir)? {
            println!("module {} v{} — {}", m.name, m.version, m.desc);
        }
        routes::RouteTable::build(&base, &dir, ts, routes::bridge_introspector(make_bridge))
    } else {
        // release：manifests.yaml 锁版本 → 逐模块加载 routes.js → 扁平化单次 from_entries（spec §3）。
        let lock = manifest::load_lock(&dir.join("manifests.yaml"))
            .map_err(|e| format!("release mode: {}: {e}", dir.join("manifests.yaml").display()))?;
        if lock.is_empty() {
            return Err(format!(
                "release mode: {} missing or empty — run `oj build` first",
                dir.join("manifests.yaml").display()
            ));
        }
        let reader = routes::bridge_default_reader(make_bridge);
        let mut entries = Vec::new();
        let b = base.trim_matches('/');
        for (module, version) in &lock {
            manifest::validate_module(module).map_err(|e| format!("manifests.yaml: {e}"))?;
            manifest::validate_version(version).map_err(|e| format!("manifests.yaml: {e}"))?;
            let mdir = dir.join(format!("{module}-{version}"));
            let mf = mdir.join("manifest.yaml");
            if !mf.is_file() {
                return Err(format!("release mode: {} missing — run `oj build {module}`", mf.display()));
            }
            let m = manifest::parse_one(&mf)?;
            if m.name != *module {
                return Err(format!("manifest name {:?} != module {module:?} (in {})", m.name, mf.display()));
            }
            println!("module {} v{} — {}", m.name, m.version, m.desc);
            let rjs = mdir.join("routes.js");
            let v = reader(&rjs).map_err(|e| format!("load {}: {e}", rjs.display()))?;
            for e in routes::entries_from_value(&v) {
                entries.push(routes::RouteEntry {
                    method: e.method,
                    pattern: format!("/{b}/{}", e.pattern.trim_matches('/')),
                    file: format!("{module}-{version}/{}", e.file),
                });
            }
        }
        let (table2, failures2) = routes::RouteTable::from_entries(&dir, &entries);
        if !failures2.is_empty() {
            return Err(format!("release routes: {}", failures2.join("; "))); // release fail-fast（spec §4）
        }
        (table2, Vec::new())
    };
    for f in &failures {
        eprintln!("error: route: {f}");
    }
    if !failures.is_empty() {
        eprintln!("warn: {} route declaration(s) skipped (see errors above)", failures.len());
    }
    for r in table.listing() {
        println!("  {:8} {}  <- {}", r.method.to_uppercase(), r.pattern, r.file.display());
    }
    let n = cfg.server.pool_size.max(1) as usize;
    let timeout = config::parse_duration(&cfg.server.timeout).ok();
    let actor = JsActor::pool(n, {
        let (dbs, kv, loader) = (dbs.clone(), kv.clone(), loader.clone());
        move || {
            Bridge::with_dbs_and_loader(
                dbs.clone(),
                kv.clone(),
                SchemaRegistry::new(),
                false,
                Some(loader.clone()),
            )
        }
    });
    let addr = format!("{}:{}", cfg.server.host, cfg.server.port);
    // localhost → 127.0.0.1 解析；阻塞 resolve 仅启动一次，可接受——ponytail。
    let addr = to_socket_addrs_sync(&addr)?;
    // 静态站点根：相对 config_dir 绝对化（缺失目录 fail-fast）。
    let static_root = match &cfg.server.root {
        Some(r) => {
            let p = Path::new(r);
            let p = if p.is_absolute() { p.to_path_buf() } else { config_dir.join(p) };
            Some(p.canonicalize().map_err(|e| format!("server.root {}: {e}", p.display()))?)
        }
        None => None,
    };
    let listener = tokio::net::TcpListener::bind(addr).await.map_err(|e| format!("bind: {e}"))?;
    let bound = listener.local_addr().map_err(|e| format!("local_addr: {e}"))?;
    let h = tokio::spawn(async move {
        let _ = mdm_server::serve_with_listener(listener, &base, dir, ts, table, actor, timeout, static_root).await;
    });
    Ok((bound, h))
}

/// config 文件的父目录（即 project_root）。bare 文件名（`parent()==""`）回落当前目录，
/// 避免 config_dir 为空 → `canonicalize("")` 失败 → project_root 钳制静默失效。
fn config_dir_of(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}

/// `host:port` → 首个解析地址（阻塞式，仅启动调用一次）。
fn to_socket_addrs_sync(s: &str) -> Result<SocketAddr, String> {
    s.to_socket_addrs()
        .map_err(|e| format!("resolve {s}: {e}"))?
        .next()
        .ok_or_else(|| format!("resolve {s}: no addresses"))
}

/// DSN 归一：非 sqlite 报错；相对路径相对 config_dir；内存库原样。
fn resolve_sqlite(dsn: &str, config_dir: &Path) -> Result<String, String> {
    let rest = dsn.strip_prefix("sqlite://").or_else(|| {
        if dsn == "sqlite::memory:" { Some("") } else { None }
    });
    let Some(rest) = rest else {
        return Err(format!("v0.1 supports only sqlite:// DSN (got '{dsn}')"));
    };
    if rest.is_empty() {
        return Ok("sqlite::memory:".into()); // sqlite://（空）视作内存
    }
    if rest.starts_with(':') || rest.starts_with("//") {
        return Ok(dsn.to_string());
    }
    let p = Path::new(rest);
    let p = if p.is_absolute() { p.to_path_buf() } else { config_dir.join(p) };
    // sqlx 默认 create_if_missing=false：文件库不存在则建零长空库（sqlite 视作有效空 DB）。
    if !p.is_file() {
        std::fs::write(&p, b"").map_err(|e| format!("create db file {}: {e}", p.display()))?;
    }
    Ok(format!("sqlite://{}", p.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tmp(PathBuf);
    fn tmpdir(tag: &str) -> Tmp {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        use std::sync::atomic::Ordering;
        let d = std::env::temp_dir().join(format!(
            "oj-sc-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        Tmp(d)
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn config_dir_never_empty() {
        // bare 文件名回落当前目录（project_root 钳制不因空路径失效）。
        assert_eq!(config_dir_of(Path::new("config.yaml")), PathBuf::from("."));
        assert_eq!(config_dir_of(Path::new("./config.yaml")), PathBuf::from("."));
        assert_eq!(config_dir_of(Path::new("sub/config.yaml")), PathBuf::from("sub"));
        assert_eq!(config_dir_of(Path::new("/abs/config.yaml")), PathBuf::from("/abs"));
    }

    #[tokio::test]
    async fn rejects_non_sqlite_dsn_at_startup() {
        let mut cfg = Config::default();
        cfg.db.insert("default".into(), "mysql://u:p@localhost/test".into());
        let e = start(cfg, Path::new("/tmp"), PathBuf::from("src"), "/v1/api".into(), true)
            .await
            .err()
            .unwrap_or_default();
        assert!(e.contains("sqlite"), "{e}");
    }

    #[tokio::test]
    async fn manifest_mismatch_blocks_startup() {
        let t = tmpdir("sc-md");
        std::fs::create_dir_all(t.0.join("src/user")).unwrap();
        std::fs::write(t.0.join("src/user/manifest.yaml"), "name: x\ndesc: d\nversion: 0.1.0\n").unwrap();
        let e = start(Config::default(), &t.0, t.0.join("src"), "/v1/api".into(), true)
            .await
            .err()
            .unwrap_or_default();
        assert!(e.contains("name"), "{e}");
    }

    /// 夹具：手摆 release dist（file 名任意合法即可）。
    fn rel_fixture(files: &[(&str, &str)]) -> PathBuf {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        use std::sync::atomic::Ordering;
        let t = std::env::temp_dir().join(format!(
            "oj-sc-rel-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        for (rel, c) in files {
            let p = t.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, c).unwrap();
        }
        t
    }

    const MANI: &str = "name: user\ndesc: d\nversion: 0.1.0\n";

    #[tokio::test]
    async fn release_aggregates_modules_via_lock() {
        let t = rel_fixture(&[
            ("dist/manifests.yaml", "user: 0.1.0\n"),
            ("dist/user-0.1.0/manifest.yaml", MANI),
            ("dist/user-0.1.0/routes.js",
             "export default [ { method: \"get\", pattern: \"user/item/{id}\", file: \"item/api-x.js\" } ];\n"),
            ("dist/user-0.1.0/item/api-x.js", "export default { get() { json.ok({ v: 1 }); } };\n"),
        ]);
        let mut cfg = Config::default();
        cfg.server.port = 0; // 随机端口（默认 778 并行测试会撞）
        let (addr, _h) = start(cfg, &t, t.join("dist"), "/v1/api".into(), false).await.unwrap();
        let r = reqwest::get(format!("http://{addr}/v1/api/user/item/7")).await.unwrap();
        assert_eq!(r.status(), 200); // pattern 无 base → 聚合拼 /v1/api/user/item/{id}
    }

    #[tokio::test]
    async fn release_fail_fast_paths() {
        // a) 无 manifests.yaml
        let t = rel_fixture(&[("dist/user-0.1.0/manifest.yaml", MANI)]);
        let e = start(Config::default(), &t, t.join("dist"), "/v1/api".into(), false).await.err().unwrap_or_default();
        assert!(e.contains("manifests.yaml") || e.contains("oj build"), "{e}");
        // b) 锁指向不存在版本
        let t = rel_fixture(&[("dist/manifests.yaml", "user: 9.9.9\n"), ("dist/user-0.1.0/manifest.yaml", MANI)]);
        let e = start(Config::default(), &t, t.join("dist"), "/v1/api".into(), false).await.err().unwrap_or_default();
        assert!(e.contains("9.9.9"), "{e}");
        // c) version 注入
        let t = rel_fixture(&[("dist/manifests.yaml", "user: ../../etc\n")]);
        let e = start(Config::default(), &t, t.join("dist"), "/v1/api".into(), false).await.err().unwrap_or_default();
        assert!(e.contains("version") || e.contains("illegal"), "{e}");
        // d) manifest name 不符
        let t = rel_fixture(&[
            ("dist/manifests.yaml", "user: 0.1.0\n"),
            ("dist/user-0.1.0/manifest.yaml", "name: other\ndesc: d\nversion: 0.1.0\n"),
        ]);
        let e = start(Config::default(), &t, t.join("dist"), "/v1/api".into(), false).await.err().unwrap_or_default();
        assert!(e.contains("name"), "{e}");
    }

    #[tokio::test]
    async fn release_routes_js_syntax_error_fails_fast() {
        // routes.js 语法错 → reader 解析失败 → 启动 Err（spec §4）
        let t = rel_fixture(&[
            ("dist/manifests.yaml", "user: 0.1.0\n"),
            ("dist/user-0.1.0/manifest.yaml", MANI),
            ("dist/user-0.1.0/routes.js", "export default [ "),
        ]);
        let e = start(Config::default(), &t, t.join("dist"), "/v1/api".into(), false).await.err().unwrap_or_default();
        assert!(e.contains("routes.js"), "{e}");
    }

    #[tokio::test]
    async fn release_conflicting_patterns_fail_fast() {
        // 两模块同 pattern 同 method → from_entries failures → 启动 Err（spec §4）
        let t = rel_fixture(&[
            ("dist/manifests.yaml", "user: 0.1.0\nother: 0.9.0\n"),
            ("dist/user-0.1.0/manifest.yaml", MANI),
            ("dist/user-0.1.0/routes.js",
             "export default [ { method: \"get\", pattern: \"user/item/{id}\", file: \"item/api-x.js\" } ];\n"),
            ("dist/user-0.1.0/item/api-x.js", "export default { get() { json.ok({}); } };\n"),
            ("dist/other-0.9.0/manifest.yaml", "name: other\ndesc: d\nversion: 0.9.0\n"),
            ("dist/other-0.9.0/routes.js",
             "export default [ { method: \"get\", pattern: \"user/item/{id}\", file: \"item/api-y.js\" } ];\n"),
            ("dist/other-0.9.0/item/api-y.js", "export default { get() { json.ok({}); } };\n"),
        ]);
        let e = start(Config::default(), &t, t.join("dist"), "/v1/api".into(), false).await.err().unwrap_or_default();
        assert!(e.contains("conflict"), "{e}");
    }

    #[tokio::test]
    async fn release_keeps_real_error_from_corrupt_lock() {
        // M-2：锁解析错保留真实错误（不与缺失混为一句 not found or invalid）
        let t = rel_fixture(&[
            ("dist/manifests.yaml", "user: [unclosed\n"),
            ("dist/user-0.1.0/manifest.yaml", MANI),
        ]);
        let e = start(Config::default(), &t, t.join("dist"), "/v1/api".into(), false).await.err().unwrap_or_default();
        assert!(e.contains("unclosed") || e.contains("parse"), "{e}");
    }

    #[tokio::test]
    async fn server_root_serves_static_relative_to_config_dir() {
        let t = tmpdir("sc-root");
        std::fs::write(t.0.join("index.html"), "<h1>site</h1>").unwrap();
        let mut cfg = Config::default();
        cfg.server.port = 0; // 随机端口
        cfg.server.root = Some(".".into()); // 相对 config_dir
        let (addr, _h) = start(cfg, &t.0, t.0.join("src"), "/v1/api".into(), true).await.unwrap();
        let r = reqwest::get(format!("http://{addr}/")).await.unwrap();
        assert_eq!(r.status(), 200);
        assert!(r.text().await.unwrap().contains("site"));
    }

    #[tokio::test]
    async fn server_root_missing_dir_fails_fast() {
        let mut cfg = Config::default();
        cfg.server.root = Some("no-such-dir".into());
        let e = start(cfg, Path::new("/tmp"), PathBuf::from("src"), "/v1/api".into(), true)
            .await
            .err()
            .unwrap_or_default();
        assert!(e.contains("server.root"), "{e}");
    }

    #[tokio::test]
    async fn seeds_and_serves_sqlite() {
        let t = tmpdir("sc-seed");
        std::fs::write(
            t.0.join("seed.sql"),
            "CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY, v TEXT);\n\
             INSERT OR IGNORE INTO t (id, v) VALUES (1, 'a');\n",
        )
        .unwrap();
        let mut cfg = Config::default();
        cfg.server.port = 0; // 随机端口
        cfg.db.insert("default".into(), format!("sqlite://{}/db.sqlite", t.0.display()));
        let (addr, _h) = start(cfg, &t.0, t.0.join("src"), "/v1/api".into(), true).await.unwrap();
        // 直接打一个临时 api.ts 验证全链路。
        std::fs::create_dir_all(t.0.join("src/u/f")).unwrap();
        std::fs::write(
            t.0.join("src/u/f/api.ts"),
            "export default { get() { db.query(\"select v from t where id = ?\", [1]).then(r => json.ok(r)); } };\n",
        )
        .unwrap();
        let resp = reqwest::get(format!("http://{addr}/v1/api/u/f/")).await.unwrap();
        assert_eq!(resp.status(), 200);
        let v: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(v["data"][0]["v"], "a", "{v}");
    }
}
