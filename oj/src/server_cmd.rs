//! oj server 装配：config → 逐 db 开库（仅 sqlite）→ seed → manifest 校验 →
//! actor 池 → axum serve。start() 返回 (addr, join_handle)，main 与测试共用。

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::net::ToSocketAddrs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use only_js::bridge::plugin_loader::{
    LoadedPlugin, PluginManifestEntry, blob_backend_connect, bus_backend, db_backend, es_backend,
    host_context, load_manifest, load_scanned, resolve_plugins_dir,
};
use only_js::bridge::{
    BusBackendRegistry, DataAccessor, DbBackendRegistry, EsBackend, PluginInfo,
};
use only_js::config::{self, Config};

use crate::app::App;
use crate::args::ServerArgs;

pub async fn run(a: ServerArgs) -> Result<(), String> {
    let (mut cfg, config_dir, dir, ts, base) =
        load_app_config(&a.config, a.dir.as_deref(), a.base.as_deref())?;
    // CLI 覆盖：证书路径与宽限天数（若有）。
    if let Some(p) = a.cert_path {
        cfg.server.certificate_path = p;
    }
    if let Some(p) = a.key_path {
        cfg.server.public_key_path = p;
    }
    if let Some(d) = a.grace_days {
        cfg.server.grace_days = Some(d);
    }
    // 初始化日志：目录默认 config 相对 ./logs，可在 server.logs_dir 配置；不存在自动创建。
    let logs_dir = server::logging::resolve_logs_dir(cfg.server.logs_dir.as_deref(), &config_dir);
    server::logging::init(&logs_dir);
    let addr = to_socket_addrs_sync(&format!("{}:{}", cfg.server.host, cfg.server.port))?;
    let app = App::from_config(cfg, &config_dir, dir.clone(), base.clone(), ts).await?;
    let (bound, h) = app.serve(addr).await?;
    println!(
        "oj server listening on http://{bound}{} (dir={}, {})",
        base,
        dir.display(),
        if ts { "dev/ts" } else { "release/js" }
    );
    h.await.map_err(|e| format!("server task: {e}"))
}

/// 解析配置 + 目录模式（同 server）：读取 config.yaml，确定服务目录（src 优先 / dist 兜底）、
/// dev/release 判定、base 前缀归源。server 与 test 命令共用，避免重复解析逻辑。
pub fn load_app_config(
    config: &str,
    dir_override: Option<&str>,
    base_override: Option<&str>,
) -> Result<(Config, PathBuf, PathBuf, bool, String), String> {
    let config_path = PathBuf::from(config);
    let config_dir = config_dir_of(&config_path);
    let cfg = config::load_from(&config_dir, config_path.file_name().and_then(|s| s.to_str()))
        .map_err(|e| format!("load config: {e}"))?;
    // 目录即模式：含构建锁 manifests.yaml → release(js)；否则 dev(ts)。
    // 默认目录：src 存在取 src，否则 dist。
    let dir = dir_override
        .map(|s| s.to_string())
        .unwrap_or_else(|| if Path::new("src").is_dir() { "src".into() } else { "dist".into() });
    let dir_path = Path::new(&dir);
    if !dir_path.is_dir() {
        return Err(format!(
            "service dir not found: {dir}（src 源码树或 oj build 产物 dist）"
        ));
    }
    let ts = !is_release(dir_path);
    let base = resolve_base(base_override, &cfg.server.base)?;
    Ok((cfg, config_dir, PathBuf::from(dir), ts, base))
}

/// base 归源：CLI `-b` 显式给出 > config `server.base`（默认 /v1/api）。
/// 空前缀拒绝（全 404 的静默坑）。
fn resolve_base(cli: Option<&str>, cfg: &str) -> Result<String, String> {
    let b = cli.unwrap_or(cfg);
    if b.trim_matches('/').is_empty() {
        return Err("base prefix must not be empty (-b / server.base)".into());
    }
    Ok(b.to_string())
}

/// 模式判定：服务目录含 `manifests.yaml`（oj build 锁文件）→ release 产物树。
/// src 源码树无此文件 → dev。两类目录形态互斥，判据确定。
fn is_release(dir: &Path) -> bool {
    dir.join("manifests.yaml").is_file()
}

/// 装配并监听（port=0 → 随机端口，测试用）。维持旧签名（返回 (SocketAddr, JoinHandle)），
/// 现有 `cargo test` 端口 0 + reqwest 用例零改动——内部委托 `App::from_config` + `App::serve`。
pub async fn start(
    cfg: Config,
    config_dir: &Path,
    dir: PathBuf,
    base: String,
    ts: bool,
) -> Result<(SocketAddr, tokio::task::JoinHandle<()>), String> {
    let addr = to_socket_addrs_sync(&format!("{}:{}", cfg.server.host, cfg.server.port))?;
    let app = App::from_config(cfg, config_dir, dir, base, ts).await?;
    app.serve(addr).await
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

/// blob 命名多后端装配：逐条目构造（local root 相对 config_dir 绝对化；
/// s3 经 blob 插件 vtable connect，cfg 按值 JSON 透传）。配置声明即必须成功注册，
/// 缺一启动期报错（fail fast，spec §2）。driver != local 且无 blob 插件 → fail fast。
pub async fn assemble_blobs(
    section: &config::BlobSection,
    config_dir: &Path,
    base: &str,
    blob_vt: Option<&'static oj_plugin_ffi::BlobBackendVtable>,
) -> Result<Arc<only_js::bridge::blob::BlobRegistry>, String> {
    let mut r = only_js::bridge::blob::BlobRegistry::new();
    for (name, c) in section.entries()? {
        let backend: Arc<dyn only_js::bridge::BlobBackend> = match c.driver.as_str() {
            "local" => {
                let root = Path::new(&c.root);
                let root = if root.is_absolute() { root.to_path_buf() } else { config_dir.join(root) };
                Arc::new(
                    only_js::bridge::LocalBlob::named(&name, &root, base)
                        .map_err(|e| format!("blob '{name}': {e}"))?,
                )
            }
            "s3" => {
                let vt = blob_vt.ok_or_else(|| {
                    format!("blob '{name}': driver 's3' requires the oj-blob-s3 plugin (run `cargo xtask plugin blob-s3`)")
                })?;
                let cfg_json = serde_json::to_string(&c)
                    .map_err(|e| format!("blob '{name}': serialize cfg: {e}"))?;
                blob_backend_connect(vt, &name, &cfg_json)
                    .await
                    .map_err(|e| format!("blob '{name}': {e}"))?
            }
            other => return Err(format!("blob '{name}': driver must be local|s3, got {other:?}")),
        };
        r.register(&name, backend).map_err(|e| format!("blob '{name}': {e}"))?;
    }
    Ok(Arc::new(r))
}

/// 逐 db 开库（server/build 共用）：经注册表按 scheme 认领，错误文案带库名。
pub async fn connect_dbs(
    cfg_db: &HashMap<String, String>,
    registry: &only_js::bridge::DbBackendRegistry,
    config_dir: &Path,
) -> Result<HashMap<String, Arc<dyn DataAccessor>>, String> {
    let mut dbs = HashMap::new();
    for (name, dsn) in cfg_db {
        let acc = registry
            .connect(dsn, config_dir)
            .await
            .map_err(|e| format!("open db '{name}': {e}"))?;
        dbs.insert(name.clone(), acc);
    }
    Ok(dbs)
}

/// 装配产物：插件注册的后端槽位（es 键选单后端 + db 认领式注册表 + blob 键选
/// 单后端 vtable 槽 + bus 键选注册表 + kv 键选单 vtable 槽）。
#[derive(Default)]
pub struct Registries {
    pub es: Option<Arc<dyn EsBackend>>,
    pub dbs: DbBackendRegistry,
    /// 插件 blob vtable（Task 4.2：单槽位，多 blob 插件注册冲突 fail fast；s3 驱动
    /// 经它 connect，装配期逐后端调用）。
    pub blob: Option<&'static oj_plugin_ffi::BlobBackendVtable>,
    /// bus 键选注册表（Task 4.3：内置 local + 插件 kafka/rabbitmq 工厂；kind 冲突 fail fast）。
    pub bus: BusBackendRegistry,
    /// kv 键选单 vtable 槽（Task 4.4：redis.default 声明 → 经插件 connect；
    /// 未声明仍 InMemoryKV 内置兜底；多 kv 插件注册冲突 fail fast）。
    pub kv: Option<&'static oj_plugin_ffi::KVStoreVtable>,
}

/// 装配层把宿主侧解析出的跨后端参数经 cfg JSON 注入插件（spec §3 有意的边界；
/// cfg 按值传入，插件须持久化时自行持有副本）。阶段 3：es 插件 cfg = {"endpoint"}。
fn plugin_cfg_json(cfg: &Config, name: &str) -> String {
    if name == "es" {
        if let Some(es) = &cfg.es {
            return serde_json::json!({ "endpoint": es.endpoint }).to_string();
        }
    }
    "{}".to_string()
}

/// 注册：插件后端（插件先于内置）→ 内置后端。
/// es 为键选式单后端：cfg es: 声明 + 恰好一个 es 插件 → FfiEsBackend(handle 0)；
/// 「配置声明了能力但插件未装」→ fail fast（§2 闸门）；多个 es 注册冲突 → fail fast。
/// db 为认领式：内置 sqlite/memory 打底 + 每个插件 db 工厂注册（scheme 交集冲突 → fail fast）。
/// blob 为键选式单后端 vtable 槽：多个 blob 插件冲突 fail fast；「s3 驱动但无 blob 插件」
/// 在 assemble_blobs 逐后端判定（driver != local 且无插件 → fail fast）。
/// bus 为键选式注册表：内置 local 打底 + 每个插件 bus 工厂注册（kind 冲突 fail fast；
/// kafka/rabbitmq kind 未装插件 → "unknown broker kind" 明确报错）。
fn build_registries(cfg: &Config, loaded: &[LoadedPlugin]) -> Result<Registries, String> {
    let es_plugins: Vec<&LoadedPlugin> =
        loaded.iter().filter(|p| p.registrations.es.is_some()).collect();
    if cfg.es.is_some() && es_plugins.is_empty() {
        return Err("config declares [es] but no es plugin loaded (run `cargo xtask plugin es`)"
            .to_string());
    }
    if es_plugins.len() > 1 {
        return Err("plugins conflict: multiple plugins register es backend".to_string());
    }
    let es = es_plugins.first().and_then(|p| es_backend(p));
    let mut dbs = DbBackendRegistry::builtin();
    for p in loaded {
        if let Some(be) = db_backend(p) {
            dbs.register(be).map_err(|e| format!("plugins db register: {e}"))?;
        }
    }
    let blob_plugins: Vec<&LoadedPlugin> =
        loaded.iter().filter(|p| p.registrations.blob.is_some()).collect();
    if blob_plugins.len() > 1 {
        return Err("plugins conflict: multiple plugins register blob backend".to_string());
    }
    let blob = blob_plugins.first().and_then(|p| p.registrations.blob);
    let mut bus = BusBackendRegistry::builtin();
    for p in loaded {
        if let Some(be) = bus_backend(p) {
            bus.register(be).map_err(|e| format!("plugins bus register: {e}"))?;
        }
    }
    // kv 键选式单 vtable 槽（Task 4.4）：多 kv 插件冲突 fail fast；redis.default 声明
    // 但无 kv 插件 → 在 start() 装配 kv 时 fail fast（未声明走 InMemoryKV，不进插件）。
    let kv_plugins: Vec<&LoadedPlugin> =
        loaded.iter().filter(|p| p.registrations.kv.is_some()).collect();
    if kv_plugins.len() > 1 {
        return Err("plugins conflict: multiple plugins register kv backend".to_string());
    }
    let kv = kv_plugins.first().and_then(|p| p.registrations.kv);
    Ok(Registries { es, dbs, blob, bus, kv })
}

/// spec §5 全流程：解析 plugins_dir → 清单严格/缺省扫描 → 去重 → 逐个加载校验
/// → 身份核对 → semver 对照 → 注册（插件先于内置）→ 内置后端注册。
/// 缺省扫描且目录不存在/为空 → 零插件（仅内置后端，不报错，除非 §2 闸门触发）。
pub async fn assemble_plugins(
    cfg: &Config,
    config_dir: &Path,
    registries: &mut Registries,
) -> Result<Vec<PluginInfo>, String> {
    let dir = resolve_plugins_dir(config_dir, cfg.plugins_dir.as_deref())
        .map_err(|e| format!("plugins dir: {e}"))?;
    let host = host_context();
    let cfg_for = |name: &str| -> String { plugin_cfg_json(cfg, name) };
    let loaded = match dir {
        Some(dir) if cfg.plugins.is_some() => {
            // 清单严格模式：按名去重 → fail fast；缺文件/校验失败 → fail fast。
            let names = cfg.plugins.as_ref().unwrap();
            let mut seen = HashSet::new();
            for name in names {
                if !seen.insert(name) {
                    return Err(format!("plugins manifest: duplicate plugin '{name}'"));
                }
            }
            let entries: Vec<PluginManifestEntry> = names
                .iter()
                .map(|name| PluginManifestEntry { name: name.clone(), semver_pin: None })
                .collect();
            load_manifest(&dir, &entries, host, &cfg_for)
                .map_err(|e| format!("plugins manifest: {e}"))?
        }
        Some(dir) => {
            // 缺省扫描：目录为空 → 零插件；扫描到损坏插件 → fail fast（不静默跳过）。
            load_scanned(&dir, host).map_err(|e| format!("plugins scan: {e}"))?
        }
        None => Vec::new(),
    };
    *registries = build_registries(cfg, &loaded)?;
    Ok(loaded.iter().map(PluginInfo::from).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use only_js::bridge::plugin_loader::kv_backend_connect;

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

    #[test]
    fn base_precedence_and_empty_guard() {
        // CLI -b > config server.base（config 默认 /v1/api 由 ServerCfg::default 兜底）
        assert_eq!(resolve_base(None, "/xapi").unwrap(), "/xapi");
        assert_eq!(resolve_base(Some("/cli"), "/xapi").unwrap(), "/cli");
        assert_eq!(resolve_base(None, "/v1/api").unwrap(), "/v1/api");
        // 空前缀（仅斜杠）拒绝
        assert!(resolve_base(Some(""), "/xapi").is_err());
        assert!(resolve_base(None, "///").is_err());
    }

    /// auth 配了但 jwt_secret 空 → 装配 fail-fast（不静默跳过鉴权）。
    #[tokio::test]
    async fn empty_jwt_secret_fails_fast() {
        let mut cfg = Config::default();
        cfg.db.insert("default".into(), "sqlite::memory:".into());
        cfg.auth = Some(serde_yaml::from_str("jwt_secret: \"\"\n").unwrap());
        let e = start(cfg, Path::new("/tmp"), PathBuf::from("src"), "/v1/api".into(), true)
            .await
            .err()
            .unwrap_or_default();
        assert!(e.contains("jwt_secret"), "{e}");
    }

    #[test]
    fn mode_detected_by_lock_file() {
        let t = tmpdir("sc-mode");
        // 空目录 / 模块树（只有 manifest.yaml）→ dev
        assert!(!is_release(&t.0));
        std::fs::create_dir_all(t.0.join("user")).unwrap();
        std::fs::write(t.0.join("user/manifest.yaml"), "name: user\n").unwrap();
        assert!(!is_release(&t.0));
        // 构建锁存在 → release
        std::fs::write(t.0.join("manifests.yaml"), "user: 0.1.0\n").unwrap();
        assert!(is_release(&t.0));
    }

    #[tokio::test]
    async fn rejects_unknown_dsn_scheme() {
        let mut cfg = Config::default();
        cfg.db.insert("default".into(), "oracle://u:p@localhost/test".into());
        let e = start(cfg, Path::new("/tmp"), PathBuf::from("src"), "/v1/api".into(), true)
            .await
            .err()
            .unwrap_or_default();
        assert!(e.contains("scheme"), "{e}");
    }

    #[tokio::test]
    async fn registry_connect_dispatches_by_scheme() {
        let t = tmpdir("sc-dsn");
        let reg = only_js::bridge::DbBackendRegistry::builtin();
        // sqlite：相对路径归一为 config_dir 下绝对路径并建空库
        reg.connect("sqlite://db.sqlite", &t.0).await.unwrap_or_else(|e| panic!("{e}"));
        assert!(t.0.join("db.sqlite").is_file());
        reg.connect("sqlite::memory:", &t.0).await.unwrap_or_else(|e| panic!("{e}"));
        reg.connect("memory://m", &t.0).await.unwrap_or_else(|e| panic!("{e}"));
        // Task 4.1：mysql/postgres 已迁插件，内置不再认领 → 缺装时明确 unknown db scheme
        // （快速失败不触网——原测试真连 127.0.0.1:1 每个 ~30s 超时，本版消除）。
        for unclaimed in ["mysql://u:p@127.0.0.1:1/app", "postgres://127.0.0.1:1/app"] {
            let e = reg.connect(unclaimed, &t.0).await.err().map(|e| e.to_string()).unwrap_or_default();
            assert!(e.contains("unknown db scheme"), "{e}");
        }
        // 未知 scheme 拒绝
        assert!(reg.connect("oracle://x", &t.0).await.is_err());
    }

    /// 下载路由数据源 = registry.default()：同 key 不同内容时字节与 default 一致（spec §2 裁决回归）。
    #[tokio::test]
    async fn download_route_source_is_default_backend_only() {
        let t = tmpdir("sc-blob-def");
        let section: config::BlobSection =
            serde_yaml::from_str("backends:\n  default:\n    driver: local\n    root: a\n  img:\n    driver: local\n    root: b\n")
                .unwrap();
        let r = assemble_blobs(&section, &t.0, "/v1/api", None).await.unwrap();
        r.default().unwrap().put("k", b"DEF", None).await.unwrap();
        r.get("img").unwrap().put("k", b"IMG", None).await.unwrap();
        match r.default().unwrap().serve("k").await.unwrap() {
            only_js::bridge::BlobServed::Bytes(bytes, _) => assert_eq!(bytes, b"DEF"),
            _ => panic!("local must inline-serve"),
        }
    }

    #[tokio::test]
    async fn assemble_blobs_multi_local_and_unknown_driver() {
        let t = tmpdir("sc-blob");
        let section: config::BlobSection =
            serde_yaml::from_str("backends:\n  default:\n    driver: local\n    root: a\n  img:\n    driver: local\n    root: b\n")
                .unwrap();
        let r = assemble_blobs(&section, &t.0, "/v1/api", None).await.unwrap();
        assert!(r.default().is_some() && r.get("img").is_some());
        // local root 相对 config_dir 绝对化并创建
        assert!(t.0.join("a").is_dir() && t.0.join("b").is_dir());
        // 未知 driver：错误带后端名
        let bad: config::BlobSection =
            serde_yaml::from_str("backends:\n  x:\n    driver: ghost\n").unwrap();
        let e = assemble_blobs(&bad, &t.0, "/v1/api", None).await.err().unwrap_or_default();
        assert!(e.contains("'x'") && e.contains("ghost"), "{e}");
    }

    /// s3 驱动但无 blob 插件 → fail fast（driver != local 且无插件闸门，Task 4.2）。
    #[tokio::test]
    async fn s3_without_blob_plugin_fails_fast() {
        let t = tmpdir("sc-blob-s3");
        let section: config::BlobSection = serde_yaml::from_str(
            "backends:\n  default:\n    driver: s3\n    bucket: b\n    region: r\n",
        )
        .unwrap();
        let e = assemble_blobs(&section, &t.0, "/v1/api", None).await.err().unwrap_or_default();
        assert!(e.contains("requires the oj-blob-s3 plugin"), "{e}");
    }

    #[tokio::test]
    async fn connect_dbs_two_engines_and_unknown_named() {
        let t = tmpdir("sc-cdb");
        let reg = only_js::bridge::DbBackendRegistry::builtin();
        let mut m = HashMap::new();
        m.insert("default".to_string(), "sqlite::memory:".to_string());
        m.insert("aux".to_string(), "memory://aux".to_string());
        let dbs = connect_dbs(&m, &reg, &t.0).await.unwrap();
        assert!(dbs.contains_key("default") && dbs.contains_key("aux"));
        // 未知 scheme：库名出现在错误里
        let mut bad = HashMap::new();
        bad.insert("mydb".to_string(), "oracle://x".to_string());
        let e = connect_dbs(&bad, &reg, &t.0).await.err().unwrap_or_default();
        assert!(e.contains("mydb"), "{e}");
        assert!(e.contains("unknown db scheme"), "{e}");
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
        // dir 指向不存在的子目录（相对 tmpdir），使模块扫描为空、顺利走到 server.root
        // 校验；此前用绝对 "src" 会 canonicalize 到 oj/src（含无 manifest 的 test_ext），
        // 在抵达 server.root 检查前就于模块扫描阶段报错，掩盖了本测试真正要验证的逻辑。
        let t = tmpdir("sc-root-missing");
        let mut cfg = Config::default();
        cfg.server.root = Some("no-such-dir".into());
        let e = start(cfg, &t.0, t.0.join("src"), "/v1/api".into(), true)
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

    // ----- 插件装配（spec §5）：全部经 cfg.plugins_dir 注入（每测试独立 Config，
    // 无全局 env 竞争；OAT_PLUGINS_DIR 只留给真实部署路径）。 -----

    use std::sync::OnceLock;

    fn host_triple() -> String {
        let out = std::process::Command::new("rustc").arg("-vV").output().unwrap();
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

    /// 编译 oj-es 产物路径（全进程一次，oj-es 已有 debug 构建，命中缓存）。
    fn es_plugin_artifact() -> PathBuf {
        static ONCE: OnceLock<PathBuf> = OnceLock::new();
        ONCE.get_or_init(|| {
            let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
            let status = std::process::Command::new("cargo")
                .args(["build", "-p", "oj-es"])
                .current_dir(&root)
                .status()
                .expect("invoke cargo build for oj-es");
            assert!(status.success(), "oj-es build failed");
            let (prefix, ext) = if cfg!(target_os = "windows") {
                ("", "dll")
            } else if cfg!(target_os = "macos") {
                ("lib", "dylib")
            } else {
                ("lib", "so")
            };
            root.join("target/debug").join(format!("{prefix}oj_es.{ext}"))
        })
        .clone()
    }

    fn es_cfg(endpoint: &str) -> Config {
        let mut cfg = Config::default();
        cfg.es = Some(config::EsCfg { endpoint: endpoint.to_string() });
        cfg
    }

    /// 清单显式给出但文件缺失 → fail fast。
    #[tokio::test(flavor = "current_thread")]
    async fn manifest_missing_file_fails_fast() {
        let t = tmpdir("sc-man");
        std::fs::create_dir_all(t.0.join(host_triple())).unwrap();
        let mut cfg = Config::default();
        cfg.plugins = Some(vec!["ghost".into()]);
        cfg.plugins_dir = Some(t.0.clone());
        let mut r = Registries::default();
        let e = assemble_plugins(&cfg, &t.0, &mut r).await.err().unwrap_or_default();
        assert!(e.contains("plugin file missing"), "{e}");
    }

    /// 清单同名两次 → fail fast（去重闸门先于加载）。
    #[tokio::test(flavor = "current_thread")]
    async fn manifest_duplicate_fails_fast() {
        let t = tmpdir("sc-dup");
        std::fs::create_dir_all(t.0.join(host_triple())).unwrap();
        let mut cfg = Config::default();
        cfg.plugins = Some(vec!["es".into(), "es".into()]);
        cfg.plugins_dir = Some(t.0.clone());
        let mut r = Registries::default();
        let e = assemble_plugins(&cfg, &t.0, &mut r).await.err().unwrap_or_default();
        assert!(e.contains("duplicate plugin 'es'"), "{e}");
    }

    /// 缺省扫描空目录 → 零插件、仅内置后端（es 未配置，不触发 §2 闸门）。
    #[tokio::test(flavor = "current_thread")]
    async fn scan_empty_dir_yields_only_builtin() {
        let t = tmpdir("sc-empty");
        std::fs::create_dir_all(t.0.join(host_triple())).unwrap();
        let mut cfg = Config::default();
        cfg.plugins_dir = Some(t.0.clone());
        let mut r = Registries::default();
        let plugins = assemble_plugins(&cfg, &t.0, &mut r).await.unwrap();
        assert!(plugins.is_empty());
        assert!(r.es.is_none());
    }

    /// 扫描到损坏插件 → fail fast（不静默跳过）。
    #[tokio::test(flavor = "current_thread")]
    async fn scan_bad_plugin_fails_fast() {
        let t = tmpdir("sc-bad");
        let pdir = t.0.join(host_triple());
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::write(pdir.join(plugin_file("broken")), b"not a real dylib").unwrap();
        let mut cfg = Config::default();
        cfg.plugins_dir = Some(t.0.clone());
        let mut r = Registries::default();
        let e = assemble_plugins(&cfg, &t.0, &mut r).await.err().unwrap_or_default();
        assert!(e.contains("plugins scan"), "{e}");
    }

    /// 配置声明 [es] 但插件未装 → 启动期报错（§2 闸门）。
    #[tokio::test(flavor = "current_thread")]
    async fn es_declared_without_plugin_fails_startup() {
        let t = tmpdir("sc-esgate");
        std::fs::create_dir_all(t.0.join(host_triple())).unwrap();
        let mut cfg = es_cfg("http://127.0.0.1:1");
        cfg.plugins_dir = Some(t.0.clone());
        let mut r = Registries::default();
        let e = assemble_plugins(&cfg, &t.0, &mut r).await.err().unwrap_or_default();
        assert!(e.contains("no es plugin loaded"), "{e}");
    }

    /// 全链路：真实 oj-es 插件装配 → es 后端经 FfiEsBackend 接线（handle 0）+ 自省信息。
    #[tokio::test(flavor = "current_thread")]
    async fn es_plugin_wires_backend() {
        let t = tmpdir("sc-esplug");
        let pdir = t.0.join(host_triple());
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::copy(es_plugin_artifact(), pdir.join(plugin_file("es"))).unwrap();
        let mut cfg = es_cfg("http://127.0.0.1:1");
        cfg.plugins_dir = Some(t.0.clone());
        let mut r = Registries::default();
        let plugins = assemble_plugins(&cfg, &t.0, &mut r).await.unwrap();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name, "es");
        assert!(r.es.is_some(), "es backend must be wired from the plugin");
    }

    /// 编译 oj-db-mysql 产物路径（全进程一次；sqlx 首次编译慢，OnceLock 缓存）。
    fn db_plugin_artifact() -> PathBuf {
        static ONCE: OnceLock<PathBuf> = OnceLock::new();
        ONCE.get_or_init(|| {
            let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
            let status = std::process::Command::new("cargo")
                .args(["build", "-p", "oj-db-mysql"])
                .current_dir(&root)
                .status()
                .expect("invoke cargo build for oj-db-mysql");
            assert!(status.success(), "oj-db-mysql build failed");
            let (prefix, ext) = if cfg!(target_os = "windows") {
                ("", "dll")
            } else if cfg!(target_os = "macos") {
                ("lib", "dylib")
            } else {
                ("lib", "so")
            };
            root.join("target/debug").join(format!("{prefix}oj_db_mysql.{ext}"))
        })
        .clone()
    }

    /// 真实 oj-db-mysql 插件装配 → db 工厂经 FfiDbBackend 注册进 DbBackendRegistry
    /// （scheme 认领；连接转发由 ffi.rs 适配器测试覆盖，此处不断网连接）。
    #[tokio::test(flavor = "current_thread")]
    async fn db_plugin_wires_backend() {
        let t = tmpdir("sc-dbplug");
        let pdir = t.0.join(host_triple());
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::copy(db_plugin_artifact(), pdir.join(plugin_file("db-mysql"))).unwrap();
        let mut cfg = Config::default();
        cfg.plugins_dir = Some(t.0.clone());
        cfg.plugins = Some(vec!["db-mysql".into()]);
        let mut r = Registries::default();
        let plugins = assemble_plugins(&cfg, &t.0, &mut r).await.unwrap();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name, "db-mysql");
        let names = r.dbs.backend_names();
        assert!(names.iter().any(|n| *n == "db-mysql"), "factory not registered: {names:?}");
        // 未认领 scheme 仍 unknown（插件没声明 oracle）→ 快速失败
        let e = r
            .dbs
            .connect("oracle://x", &t.0)
            .await
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(e.contains("unknown db scheme"), "{e}");
    }

    /// mysql DSN 但插件未装 → 明确 "unknown db scheme"（不触网，快速失败）。
    #[tokio::test(flavor = "current_thread")]
    async fn db_declared_without_plugin_unknown_scheme() {
        let t = tmpdir("sc-dbplug-none");
        std::fs::create_dir_all(t.0.join(host_triple())).unwrap(); // 空插件目录 → 零插件
        let mut cfg = Config::default();
        cfg.plugins_dir = Some(t.0.clone());
        let mut r = Registries::default();
        assemble_plugins(&cfg, &t.0, &mut r).await.unwrap();
        let mut m = std::collections::HashMap::new();
        m.insert("mydb".to_string(), "mysql://u:p@127.0.0.1:1/app".to_string());
        let e = connect_dbs(&m, &r.dbs, &t.0).await.err().unwrap_or_default();
        assert!(e.contains("mydb"), "{e}");
        assert!(e.contains("unknown db scheme"), "{e}");
    }

    /// 编译 oj-blob-s3 产物路径（全进程一次；object_store aws 首次编译慢，OnceLock 缓存）。
    fn blob_plugin_artifact() -> PathBuf {
        static ONCE: OnceLock<PathBuf> = OnceLock::new();
        ONCE.get_or_init(|| {
            let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
            let status = std::process::Command::new("cargo")
                .args(["build", "-p", "oj-blob-s3"])
                .current_dir(&root)
                .status()
                .expect("invoke cargo build for oj-blob-s3");
            assert!(status.success(), "oj-blob-s3 build failed");
            let (prefix, ext) = if cfg!(target_os = "windows") {
                ("", "dll")
            } else if cfg!(target_os = "macos") {
                ("lib", "dylib")
            } else {
                ("lib", "so")
            };
            root.join("target/debug").join(format!("{prefix}oj_blob_s3.{ext}"))
        })
        .clone()
    }

    /// 真实 oj-blob-s3 插件装配 → Registries.blob 槽就位；经它 connect 建后端
    /// （cfg 校验离线路径：bucket/region 必填在插件侧 fail-fast，不触网）。
    #[tokio::test(flavor = "current_thread")]
    async fn blob_plugin_wires_vtable_and_connect_gate() {
        let t = tmpdir("sc-blobplug");
        let pdir = t.0.join(host_triple());
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::copy(blob_plugin_artifact(), pdir.join(plugin_file("blob-s3"))).unwrap();
        let mut cfg = Config::default();
        cfg.plugins_dir = Some(t.0.clone());
        cfg.plugins = Some(vec!["blob-s3".into()]);
        let mut r = Registries::default();
        let plugins = assemble_plugins(&cfg, &t.0, &mut r).await.unwrap();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name, "blob-s3");
        assert!(r.blob.is_some(), "blob vtable slot not registered");
        // 经 vtable connect：配置缺 bucket → 插件侧 fail-fast（快速失败不触网）
        let cfg_json = serde_json::json!({ "driver": "s3", "region": "us-east-1" }).to_string();
        let e = blob_backend_connect(r.blob.unwrap(), "img", &cfg_json)
            .await
            .err()
            .unwrap_or_default();
        assert!(e.contains("bucket required"), "{e}");
    }

    /// 编译 oj-bus-kafka 产物路径（全进程一次；rdkafka 首次编译慢，OnceLock 缓存）。
    fn bus_kafka_plugin_artifact() -> PathBuf {
        static ONCE: OnceLock<PathBuf> = OnceLock::new();
        ONCE.get_or_init(|| {
            let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
            let status = std::process::Command::new("cargo")
                .args(["build", "-p", "oj-bus-kafka"])
                .current_dir(&root)
                .status()
                .expect("invoke cargo build for oj-bus-kafka");
            assert!(status.success(), "oj-bus-kafka build failed");
            let (prefix, ext) = if cfg!(target_os = "windows") {
                ("", "dll")
            } else if cfg!(target_os = "macos") {
                ("lib", "dylib")
            } else {
                ("lib", "so")
            };
            root.join("target/debug").join(format!("{prefix}oj_bus_kafka.{ext}"))
        })
        .clone()
    }

    /// 编译 oj-bus-rabbitmq 产物路径（全进程一次；lapin 首次编译慢，OnceLock 缓存）。
    fn bus_rabbitmq_plugin_artifact() -> PathBuf {
        static ONCE: OnceLock<PathBuf> = OnceLock::new();
        ONCE.get_or_init(|| {
            let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
            let status = std::process::Command::new("cargo")
                .args(["build", "-p", "oj-bus-rabbitmq"])
                .current_dir(&root)
                .status()
                .expect("invoke cargo build for oj-bus-rabbitmq");
            assert!(status.success(), "oj-bus-rabbitmq build failed");
            let (prefix, ext) = if cfg!(target_os = "windows") {
                ("", "dll")
            } else if cfg!(target_os = "macos") {
                ("lib", "dylib")
            } else {
                ("lib", "so")
            };
            root.join("target/debug").join(format!("{prefix}oj_bus_rabbitmq.{ext}"))
        })
        .clone()
    }

    /// 真实 oj-bus-kafka 插件装配 → Registries.bus 注册 kind "kafka"（kind 由插件名
    /// 去 "bus-" 前缀推断）。connect 需真 broker，此处只验证注册与 kind 键选。
    #[tokio::test(flavor = "current_thread")]
    async fn bus_plugin_wires_kind() {
        let t = tmpdir("sc-busplug");
        let pdir = t.0.join(host_triple());
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::copy(bus_kafka_plugin_artifact(), pdir.join(plugin_file("bus-kafka"))).unwrap();
        let mut cfg = Config::default();
        cfg.plugins_dir = Some(t.0.clone());
        cfg.plugins = Some(vec!["bus-kafka".into()]);
        let mut r = Registries::default();
        let plugins = assemble_plugins(&cfg, &t.0, &mut r).await.unwrap();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name, "bus-kafka");
        let kinds = r.bus.kinds();
        assert!(kinds.iter().any(|k| k == "kafka"), "kind not registered: {kinds:?}");
        // 本地 kind 仍内置
        assert!(kinds.iter().any(|k| k == "local"), "{kinds:?}");
    }

    /// broker.kind=kafka 但插件未装 → "unknown broker kind"（列出已知 kind，快速失败）。
    #[tokio::test(flavor = "current_thread")]
    async fn kafka_declared_without_plugin_unknown_kind() {
        let t = tmpdir("sc-busplug-none");
        std::fs::create_dir_all(t.0.join(host_triple())).unwrap(); // 空插件目录 → 零插件
        let mut cfg = Config::default();
        cfg.plugins_dir = Some(t.0.clone());
        let mut r = Registries::default();
        assemble_plugins(&cfg, &t.0, &mut r).await.unwrap();
        let broker_cfg = only_js::config::BrokerCfg {
            kind: "kafka".into(),
            ..Default::default()
        };
        let e = r
            .bus
            .connect(&Some(broker_cfg))
            .await
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(e.contains("unknown broker kind 'kafka'"), "{e}");
    }

    /// 编译 oj-kv-redis 产物路径（全进程一次）。
    fn kv_redis_plugin_artifact() -> PathBuf {
        static ONCE: OnceLock<PathBuf> = OnceLock::new();
        ONCE.get_or_init(|| {
            let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
            let status = std::process::Command::new("cargo")
                .args(["build", "-p", "oj-kv-redis"])
                .current_dir(&root)
                .status()
                .expect("invoke cargo build for oj-kv-redis");
            assert!(status.success(), "oj-kv-redis build failed");
            let (prefix, ext) = if cfg!(target_os = "windows") {
                ("", "dll")
            } else if cfg!(target_os = "macos") {
                ("lib", "dylib")
            } else {
                ("lib", "so")
            };
            root.join("target/debug").join(format!("{prefix}oj_kv_redis.{ext}"))
        })
        .clone()
    }

    /// 真实 oj-kv-redis 插件装配 → Registries.kv 槽就位；经 vtable connect 建 KV
    /// （连接探活需真 redis——此处只验证槽位 + connect 到无监听端口 fail-fast，不触网）。
    #[tokio::test(flavor = "current_thread")]
    async fn kv_plugin_wires_vtable_and_connect_gate() {
        let t = tmpdir("sc-kvplug");
        let pdir = t.0.join(host_triple());
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::copy(kv_redis_plugin_artifact(), pdir.join(plugin_file("kv-redis"))).unwrap();
        let mut cfg = Config::default();
        cfg.plugins_dir = Some(t.0.clone());
        cfg.plugins = Some(vec!["kv-redis".into()]);
        let mut r = Registries::default();
        let plugins = assemble_plugins(&cfg, &t.0, &mut r).await.unwrap();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name, "kv-redis");
        assert!(r.kv.is_some(), "kv vtable slot not registered");
        // 经 vtable connect：无监听端口 → 插件侧探活 fail-fast（不挂启动）。
        let e = kv_backend_connect(r.kv.unwrap(), "redis://127.0.0.1:1/")
            .await
            .err()
            .unwrap_or_default();
        assert!(e.contains("redis connect"), "{e}");
    }

    /// redis.default 声明但无 kv 插件 → 启动 fail-fast（§2 闸门，不退化静默）。
    #[tokio::test(flavor = "current_thread")]
    async fn redis_declared_without_kv_plugin_fails_fast() {
        let t = tmpdir("sc-kvplug-none");
        std::fs::create_dir_all(t.0.join(host_triple())).unwrap(); // 空插件目录 → 零插件
        let mut cfg = Config::default();
        cfg.plugins_dir = Some(t.0.clone());
        cfg.redis.insert("default".into(), "redis://127.0.0.1:1/".into());
        let mut r = Registries::default();
        assemble_plugins(&cfg, &t.0, &mut r).await.unwrap();
        assert!(r.kv.is_none());
        let e = start(cfg, &t.0, t.0.join("src"), "/v1/api".into(), true)
            .await
            .err()
            .unwrap_or_default();
        assert!(e.contains("no kv plugin loaded"), "{e}");
        assert!(e.contains("cargo xtask plugin kv-redis"), "{e}");
    }

    /// 硬验收（Task 6.1 Step 5）：真 kafka 插件 broker 下 Task 0.5 共享语义回归
    /// （env-gated，`OJ_TEST_KAFKA_BROKERS` 给逗号分隔 bootstrap servers；未设置 → 跳过）。
    /// 同一 broker 实例（一个 FfiEventBroker，单消费循环/每 topic）上两个订阅通道
    /// （模拟跨 actor 池与全部 WS 连接）共享同一 topic：一次 publish → 插件消费循环
    /// 经 host.deliver → 全局 DELIVER_TARGETS 扇出 → 两通道都收到。
    #[tokio::test(flavor = "multi_thread")]
    async fn kafka_plugin_broker_shared_across_channels() {
        let brokers = match std::env::var("OJ_TEST_KAFKA_BROKERS") {
            Ok(b) if !b.is_empty() => b,
            _ => {
                eprintln!("skip: OJ_TEST_KAFKA_BROKERS unset");
                return;
            }
        };
        let t = tmpdir("sc-busshare-k");
        let pdir = t.0.join(host_triple());
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::copy(bus_kafka_plugin_artifact(), pdir.join(plugin_file("bus-kafka"))).unwrap();
        let mut cfg = Config::default();
        cfg.plugins_dir = Some(t.0.clone());
        cfg.plugins = Some(vec!["bus-kafka".into()]);
        let mut r = Registries::default();
        let plugins = assemble_plugins(&cfg, &t.0, &mut r).await.unwrap();
        assert_eq!(plugins.len(), 1);
        let broker_cfg = only_js::config::BrokerCfg {
            kind: "kafka".into(),
            brokers: brokers.split(',').map(|s| s.trim().to_string()).collect(),
            group: Some("oj-shared".into()),
            topic_prefix: Some(format!("ojshare-k-{}", std::process::id())),
            ..Default::default()
        };
        let broker = r.bus.connect(&Some(broker_cfg)).await.unwrap();
        let topic = format!("shared.{}", std::process::id());
        let (tx1, mut rx1) = tokio::sync::mpsc::unbounded_channel();
        let (tx2, mut rx2) = tokio::sync::mpsc::unbounded_channel();
        broker.subscribe(&topic, tx1).await.unwrap();
        broker.subscribe(&topic, tx2).await.unwrap(); // 同 topic 第二通道（不新起消费）
        tokio::time::sleep(std::time::Duration::from_millis(500)).await; // 等消费就绪
        broker.publish(&topic, &serde_json::json!({ "v": 9 })).await.unwrap();
        let f1 = tokio::time::timeout(std::time::Duration::from_secs(10), rx1.recv())
            .await
            .expect("shared receive 1 timeout")
            .expect("channel 1 closed");
        let f2 = tokio::time::timeout(std::time::Duration::from_secs(10), rx2.recv())
            .await
            .expect("shared receive 2 timeout")
            .expect("channel 2 closed");
        let v1: serde_json::Value = serde_json::from_str(&f1).unwrap();
        assert_eq!(v1["data"]["v"], 9, "{f1}");
        let v2: serde_json::Value = serde_json::from_str(&f2).unwrap();
        assert_eq!(v2["data"]["v"], 9, "{f2}");
    }

    /// 硬验收（Task 6.1 Step 5）：真 rabbitmq 插件 broker 下 Task 0.5 共享语义回归
    /// （env-gated，`OJ_TEST_RABBITMQ_URL` 给 amqp URL；未设置 → 跳过）。语义同 kafka 测试。
    #[tokio::test(flavor = "multi_thread")]
    async fn rabbitmq_plugin_broker_shared_across_channels() {
        let url = match std::env::var("OJ_TEST_RABBITMQ_URL") {
            Ok(u) if !u.is_empty() => u,
            _ => {
                eprintln!("skip: OJ_TEST_RABBITMQ_URL unset");
                return;
            }
        };
        let t = tmpdir("sc-busshare-r");
        let pdir = t.0.join(host_triple());
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::copy(bus_rabbitmq_plugin_artifact(), pdir.join(plugin_file("bus-rabbitmq"))).unwrap();
        let mut cfg = Config::default();
        cfg.plugins_dir = Some(t.0.clone());
        cfg.plugins = Some(vec!["bus-rabbitmq".into()]);
        let mut r = Registries::default();
        let plugins = assemble_plugins(&cfg, &t.0, &mut r).await.unwrap();
        assert_eq!(plugins.len(), 1);
        let broker_cfg = only_js::config::BrokerCfg {
            kind: "rabbitmq".into(),
            url: Some(url),
            topic_prefix: Some(format!("ojshare-r-{}", std::process::id())),
            ..Default::default()
        };
        let broker = r.bus.connect(&Some(broker_cfg)).await.unwrap();
        let topic = format!("shared.{}", std::process::id());
        let (tx1, mut rx1) = tokio::sync::mpsc::unbounded_channel();
        let (tx2, mut rx2) = tokio::sync::mpsc::unbounded_channel();
        broker.subscribe(&topic, tx1).await.unwrap();
        broker.subscribe(&topic, tx2).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        broker.publish(&topic, &serde_json::json!({ "v": 11 })).await.unwrap();
        let f1 = tokio::time::timeout(std::time::Duration::from_secs(10), rx1.recv())
            .await
            .expect("shared receive 1 timeout")
            .expect("channel 1 closed");
        let f2 = tokio::time::timeout(std::time::Duration::from_secs(10), rx2.recv())
            .await
            .expect("shared receive 2 timeout")
            .expect("channel 2 closed");
        let v1: serde_json::Value = serde_json::from_str(&f1).unwrap();
        assert_eq!(v1["data"]["v"], 11, "{f1}");
        let v2: serde_json::Value = serde_json::from_str(&f2).unwrap();
        assert_eq!(v2["data"]["v"], 11, "{f2}");
    }
}
