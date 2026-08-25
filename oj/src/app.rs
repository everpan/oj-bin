//! 进程内应用装配：`App` 抽取自原 `server_cmd::start` 的装配段。
//!
//! 设计要点（评审修正清单）：
//! - 单一 `StableState` 在 `from_config` 内构造一次，被 `App` 的 actor 工厂与测试运行时
//!   共享（同一组 `bus`/`dbs`/`kv`/`loader`/`es` Arc），保证 in-memory 后端跨家族互通（修正 #2）。
//! - `dispatch` 以 `axum::Router::oneshot` 在进程内跑完整路由 + 真实运行时 + 真实后端，
//!   零 TCP（对标 Go Fiber `app.Test`）。WS upgrade 经 oneshot 返回 101，由 `op_client_dispatch`
//!   占位处理，不跑帧循环（修正 #3）。
//! - 外层 `timeout` 包裹防止 handler 死循环挂死测试 task（修正 #10）。

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use tower::util::ServiceExt;
use mdm_base_rust::bridge::plugin_loader::kv_backend_connect;
use mdm_base_rust::bridge::{
    Bridge, Dialect, EsBackend, Extras, InMemoryKV, KVStore, LoaderShared, SchemaRegistry,
};
use mdm_base_rust::bridge::blob::{BlobBackend, BlobRegistry};
use mdm_base_rust::bridge::{EventBroker, StableState};
use mdm_base_rust::config::{self, Config};
use mdm_server::actor::JsActor;
use mdm_server::routes;
use mdm_server::ws;
use tokio::task::JoinHandle;

use crate::manifest;
use crate::server_cmd::{assemble_blobs, assemble_plugins, connect_dbs, Registries};

/// 进程内 HTTP 派发契约：测试运行时把 `Arc<App>` 注入 OpState，JS `client` 全局经
/// `op_client_dispatch` 调它。trait 必须 `Send+Sync+'static` 且方法用 `async_trait`
/// （不能用 `-> impl Future`，否则非对象安全，`Arc<dyn>` 编不过——修正 #6）。
#[async_trait]
pub trait ClientTransport: Send + Sync + 'static {
    /// 派发一个已构造好的请求，返回完整响应（含 101 upgrade）。
    async fn dispatch(&self, req: Request<Body>) -> axum::http::Response<Body>;
    /// API 基础前缀（如 `/v1/api`）；op 拼进 path，测试写 `client.get("/x")` 即可（修正 #7）。
    fn base(&self) -> &str;
}

/// 共享运行时：装配产物 + 进程内 dispatch 句柄。
pub struct App {
    router: Router,
    #[allow(dead_code)]
    bus: Arc<dyn EventBroker>,
    /// 与 actor 工厂共享同一组后端的 StableState，供测试运行时（bridge_ext）复用（修正 #2）。
    stable: Arc<StableState>,
    base: String,
}

/// dispatch 外层超时（handler 死循环 KillSwitch 兜底 server.timeout，这里再兜底测试 task）。
const DISPATCH_TIMEOUT: Duration = Duration::from_secs(60);

impl App {
    /// 装配并构造（原 `start` 逻辑搬入）。唯一构造一处 `StableState`，同时被 actor 工厂与
    /// 测试运行时引用，保证 db/bus/kv 跨家族为同一组 Arc（修正 #2）。
    pub async fn from_config(
        cfg: Config,
        config_dir: &Path,
        dir: PathBuf,
        base: String,
        ts: bool,
    ) -> Result<App, String> {
        // 其余 redis key warn 忽略（仅 redis.default 参与装配）。
        for (name, url) in cfg.redis.iter().filter(|(n, _)| n.as_str() != "default") {
            eprintln!("warn: redis '{name}' ({url}) ignored (only redis.default is used)");
        }
        // 绝对化 dir（Bridge loader 的 project_root 用 config_dir，api 相对 dir）。
        let dir = dir.canonicalize().unwrap_or(dir);
        let loader = Arc::new(LoaderShared {
            project_root: config_dir.canonicalize().unwrap_or_else(|_| config_dir.to_path_buf()),
            ts,
        });
        // 插件装配（spec §5）：解析 plugins_dir → 清单严格/缺省扫描 → 校验 → 注册。
        let mut registries = Registries::default();
        let _plugins = assemble_plugins(&cfg, config_dir, &mut registries)
            .await
            .map_err(|e| format!("plugins: {e}"))?;
        // KV：redis.default 存在 → 经 kv 插件 vtable connect（单例 fail-fast）；
        // 未声明 → InMemoryKV 内置兜底。
        let kv: Arc<dyn KVStore> = match cfg.redis.get("default") {
            Some(url) => match registries.kv {
                Some(vt) => kv_backend_connect(vt, url)
                    .await
                    .map_err(|e| format!("redis 'default': {e}"))?,
                None => {
                    return Err("config declares redis.default but no kv plugin loaded \
                                (run `cargo xtask plugin kv-redis`)"
                        .to_string())
                }
            },
            None => Arc::new(InMemoryKV::new()),
        };
        let es: Option<Arc<dyn EsBackend>> = registries.es;
        // blob：blob 段存在即启用；未声明 → None。
        let blobs: Option<Arc<BlobRegistry>> = match &cfg.blob {
            None => None,
            Some(section) => {
                Some(assemble_blobs(section, config_dir, &base, registries.blob).await?)
            }
        };
        // 下载路由仅服务 default 后端。
        let blob: Option<Arc<dyn BlobBackend>> = blobs.as_ref().and_then(|r| r.default());
        // 逐 db 开库（未知 scheme 注册表 fail-fast）。
        let dbs = connect_dbs(&cfg.db, &registries.dbs, config_dir).await?;
        // 项目根 seed.sql（仅 sqlite 库重放）。
        let seed = config_dir.join("seed.sql");
        if seed.is_file() {
            if dbs.get("default").map(|d| d.dialect()) == Some(Dialect::Sqlite) {
                let text = std::fs::read_to_string(&seed).map_err(|e| format!("read seed: {e}"))?;
                if let Some(db) = dbs.get("default") {
                    for stmt in text.split(';').map(str::trim).filter(|s| !s.is_empty()) {
                        db.exec_with_params(stmt, &[]).await.map_err(|e| format!("seed: {e}"))?;
                    }
                }
            } else {
                eprintln!("warn: seed.sql skipped (default db is not sqlite)");
            }
        }
        // 鉴权。
        let auth = match &cfg.auth {
            Some(a) if a.jwt_secret.trim().is_empty() => {
                return Err("auth.jwt_secret must not be empty".into())
            }
            Some(a) => {
                let db = dbs.get("default").ok_or("auth requires db 'default'")?.clone();
                Some(Arc::new(
                    mdm_server::auth::Auth::new(a, db, kv.clone()).map_err(|e| format!("auth: {e}"))?,
                ))
            }
            None => None,
        };
        // 共享事件总线。
        let bus = registries
            .bus
            .connect(&cfg.broker)
            .await
            .map_err(|e| format!("broker: {e}"))?;
        // 单一工厂（内省 / actor 池 / WS 连接共享同一 Bus 与 Extras）——闭包捕获全 Arc，Clone 即共享。
        let make_bridge = {
            let (dbs, kv, loader, es, bus) = (
                dbs.clone(),
                kv.clone(),
                loader.clone(),
                es.clone(),
                bus.clone(),
            );
            let blobs = blobs.clone();
            move || {
                Bridge::with_dbs_and_loader(
                    dbs.clone(),
                    kv.clone(),
                    SchemaRegistry::new(),
                    false,
                    Some(loader.clone()),
                    Extras {
                        blobs: blobs.clone(),
                        es: es.clone(),
                        bus: Some(bus.clone()),
                        plugins: Vec::new(),
                    },
                )
            }
        };
        // 路由表：dev 启动内省 .route 声明；release 聚合 dist/manifests.yaml。
        let (table, failures) = if ts {
            for m in manifest::load_modules(&dir)? {
                eprintln!("module {} v{} — {}", m.name, m.version, m.desc);
            }
            routes::RouteTable::build(&base, &dir, ts, routes::bridge_introspector(make_bridge.clone()))
        } else {
            let lock = manifest::load_lock(&dir.join("manifests.yaml"))
                .map_err(|e| format!("release mode: {}: {e}", dir.join("manifests.yaml").display()))?;
            if lock.is_empty() {
                return Err(format!(
                    "release mode: {} missing or empty — run `oj build` first",
                    dir.join("manifests.yaml").display()
                ));
            }
            let reader = routes::bridge_default_reader(make_bridge.clone());
            let mut entries = Vec::new();
            let b = base.trim_matches('/');
            for (module, version) in &lock {
                manifest::validate_module(module).map_err(|e| format!("manifests.yaml: {e}"))?;
                manifest::validate_version(version).map_err(|e| format!("manifests.yaml: {e}"))?;
                let mdir = dir.join(format!("{module}-{version}"));
                let mf = mdir.join("manifest.yaml");
                if !mf.is_file() {
                    return Err(format!(
                        "release mode: {} missing — run `oj build {module}`",
                        mf.display()
                    ));
                }
                let m = manifest::parse_one(&mf)?;
                if m.name != *module {
                    return Err(format!(
                        "manifest name {:?} != module {module:?} (in {})",
                        m.name,
                        mf.display()
                    ));
                }
                eprintln!("module {} v{} — {}", m.name, m.version, m.desc);
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
                return Err(format!("release routes: {}", failures2.join("; ")));
            }
            (table2, Vec::new())
        };
        for f in &failures {
            eprintln!("error: route: {f}");
        }
        if !failures.is_empty() {
            eprintln!("warn: {} route declaration(s) skipped (see errors above)", failures.len());
        }
        for (_, file, methods) in table.grouped() {
            eprintln!("  {}:", file.display());
            for (method, pattern) in methods {
                eprintln!("    {:8} {}", method, pattern);
            }
        }
        let n = cfg.server.pool_size.max(1) as usize;
        let timeout = config::parse_duration(&cfg.server.timeout).ok();
        // actor 池：bridges 与 WS 连接共享同一 Bus 与 Extras。
        let actor = JsActor::pool(n, make_bridge.clone());
        // 静态站点根：相对 config_dir 绝对化（缺失目录 fail-fast）。
        let static_root = match &cfg.server.root {
            Some(r) => {
                let p = Path::new(r);
                let p = if p.is_absolute() { p.to_path_buf() } else { config_dir.join(p) };
                Some(p.canonicalize().map_err(|e| format!("server.root {}: {e}", p.display()))?)
            }
            None => None,
        };
        let pipeline = mdm_server::Pipeline {
            tenant_header: cfg.tenant.enable.then(|| cfg.tenant.header_key.clone()),
            auth,
            max_upload: cfg.server.max_upload_bytes,
            blob: blob.clone(),
        };
        // WS 目录镜像挂载（<dir>/WS.ts → {base}/<dir>/ws）。
        let ws_router = ws::mirror_routes(
            &base,
            &dir,
            timeout.unwrap_or(Duration::from_secs(30)),
            make_bridge,
        );
        let router = mdm_server::app(&base, dir, ts, table, actor, timeout, static_root, pipeline)
            .merge(ws_router);
        // 共享 StableState：与 actor 工厂用同一组后端 Arc，供测试运行时注入（修正 #2）。
        let stable = Arc::new(StableState {
            kv: kv.clone(),
            dbs: dbs.clone(),
            client: reqwest::Client::builder()
                .no_proxy()
                .build()
                .unwrap_or_default(),
            registry: Arc::new(SchemaRegistry::new()),
            loader: Some(loader.clone()),
            blobs: blobs.clone().unwrap_or_else(|| Arc::new(BlobRegistry::new())),
            bus: bus.clone(),
            es: es.clone(),
            plugins: Vec::new(),
        });
        Ok(App { router, bus, stable, base })
    }

    /// 进程内 dispatch：克隆 router + oneshot（零 TCP）。外层 timeout 防 hang（修正 #10）。
    pub async fn dispatch(&self, req: Request<Body>) -> axum::http::Response<Body> {
        let router = self.router.clone();
        match tokio::time::timeout(DISPATCH_TIMEOUT, router.oneshot(req)).await {
            Ok(resp) => resp.expect("oneshot dispatch is infallible"),
            Err(_) => {
                // 超时：返回 408 信封（对齐 Go dev server）。
                let mut r = axum::http::Response::new(Body::from(
                    mdm_base_rust::bridge::fail(408, "dispatch timed out", &serde_json::Value::Null).0,
                ));
                *r.status_mut() = StatusCode::REQUEST_TIMEOUT;
                r
            }
        }
    }

    /// 绑定并服务（行为同原 `start`：`.merge(ws)` 已在 from_config 完成；port-0 随机端口）。
    pub async fn serve(self, addr: SocketAddr) -> Result<(SocketAddr, JoinHandle<()>), String> {
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| format!("bind: {e}"))?;
        let bound = listener
            .local_addr()
            .map_err(|e| format!("local_addr: {e}"))?;
        let h = tokio::spawn(async move {
            let _ = mdm_server::serve_router(listener, self.router).await;
        });
        Ok((bound, h))
    }

    /// 唯一 StableState（与 actor 共享后端），供测试运行时构造 bridge_ext。
    pub fn stable(&self) -> Arc<StableState> {
        self.stable.clone()
    }

    /// API 基础前缀。
    pub fn base(&self) -> &str {
        &self.base
    }
}

#[async_trait]
impl ClientTransport for App {
    async fn dispatch(&self, req: Request<Body>) -> axum::http::Response<Body> {
        App::dispatch(self, req).await
    }
    fn base(&self) -> &str {
        App::base(self)
    }
}
