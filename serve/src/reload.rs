//! 配置热更新传输层（v1，spec 2026-10-12）。
//!
//! 外层单一 fallback：在 ReloadState 已知的 mount prefix 集合上分流 `{base}/_system/health`
//! 与 `{base}/_system/config`，其余请求原样 `oneshot` 转发给当前内层 Router（watch 通道）。
//! 内层 Router 由 `oj` 侧 `reload_executor` 闭包在热更时 swap。本模块不依赖 `oj` crate，
//! 只持有 `Arc<Router>` 与 reload_api 门禁/守卫。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::{Json, Router};
use only_js::bridge::AuthGuard;
use serde_json::Value;
use tokio::sync::{Mutex, watch};
use tower::util::ServiceExt;

/// 热更执行结果（POST 成功返回体）。
#[derive(Clone, Debug)]
pub struct ReloadOutcome {
    /// 代际 id（每次成功 swap +1；unchanged 时不变）。
    pub generation: u64,
    /// 本代已生效段。
    pub applied: Vec<String>,
    /// 已验证合法但本代沿用旧值、重启后生效。
    pub restart_pending: Vec<String>,
    /// listener 级沿旧（port/host/certificate_path/public_key_path）。
    pub kept_listener: Vec<String>,
    /// 归一后 cfg 与当前代相同（幂等）。
    pub unchanged: bool,
}

/// 热更执行闭包类型：`oj` 侧注入。同步执行（内部用 blocking 线程做影子装配），
/// 返回 `Ok(outcome)` 或 `Err((status, msg))`（`status`=HTTP 状态，400/500）。
pub type ReloadExecutor = Arc<dyn Fn() -> Result<ReloadOutcome, (u16, String)> + Send + Sync>;

/// 代际元数据（GET 摘要来源）。
#[derive(Clone, Debug)]
struct ReloadMeta {
    generation: u64,
    cfg_sha256: String,
    loaded_at: String,
    config_path: PathBuf,
}

/// 热更新共享状态（外层 fallback 持有，启动期构造一次）。
pub struct ReloadState {
    inner_tx: watch::Sender<Arc<Router>>,
    inner_rx: watch::Receiver<Arc<Router>>,
    /// 已知 api 挂载 prefix（ReloadState.prefixes，重载改 mounts 时更新）。
    prefixes: Mutex<Vec<String>>,
    auth_guard: Option<Arc<dyn AuthGuard>>,
    /// `server.reload_api`：false → `_system/config` 永远 404。
    enabled: bool,
    /// 串行化执行（try_lock 失败 → 409 busy）。
    reload_lock: Mutex<()>,
    meta: Arc<Mutex<ReloadMeta>>,
    /// 热更执行闭包（`oj` 侧注入；构造后由 `set_executor` 填入，避免与
    /// `ReloadState` 的构造循环依赖）。
    executor: Mutex<Option<ReloadExecutor>>,
}

/// 提交句柄：`reload_executor` 闭包捕获它来 swap 内层 Router + 推进代际。
/// 与 `ReloadState` 解耦，避免闭包与 `ReloadState` 的构造循环依赖。
pub struct Committer {
    inner_tx: watch::Sender<Arc<Router>>,
    meta: Arc<Mutex<ReloadMeta>>,
}

impl Committer {
    /// 热更成功后提交：swap 内层 Router + 推进代际元数据。返回新代际 id。
    pub async fn commit(&self, new_inner: Arc<Router>, sha256: String) -> u64 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let generation = {
            let mut meta = self.meta.lock().await;
            meta.generation += 1;
            let g = meta.generation;
            meta.cfg_sha256 = sha256;
            meta.loaded_at = now.to_string();
            g
        };
        // 旧内层 Router 随 watch 接收方丢弃（在途请求仍持有旧 Arc）自然 drain。
        let _ = self.inner_tx.send(new_inner);
        generation
    }

    /// 当前代际 id（幂等判定用）。
    pub async fn generation(&self) -> u64 {
        self.meta.lock().await.generation
    }
}

impl ReloadState {
    /// 构造（启动期调用一次）。`initial_inner` = 首代内层 Router。
    pub fn new(
        initial_inner: Arc<Router>,
        prefixes: Vec<String>,
        auth_guard: Option<Arc<dyn AuthGuard>>,
        enabled: bool,
        config_path: PathBuf,
        executor: Option<ReloadExecutor>,
    ) -> Arc<Self> {
        let (inner_tx, inner_rx) = watch::channel(initial_inner);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Arc::new(Self {
            inner_tx,
            inner_rx,
            prefixes: Mutex::new(prefixes),
            auth_guard,
            enabled,
            reload_lock: Mutex::new(()),
            meta: Arc::new(Mutex::new(ReloadMeta {
                generation: 0,
                cfg_sha256: String::new(),
                loaded_at: now.to_string(),
                config_path,
            })),
            executor: Mutex::new(executor),
        })
    }

    /// 注入热更执行闭包（构造 `ReloadState` 后再填，避免循环依赖）。
    /// 异步取锁：本函数在 `serve_hot_reload_branch` 的 axum 运行时内调用，
    /// 不可用 `blocking_lock`（会 panic "Cannot block the current thread"）。
    pub async fn set_executor(&self, ex: ReloadExecutor) {
        *self.executor.lock().await = Some(ex);
    }

    /// 取出提交句柄（供 `reload_executor` 闭包捕获）。
    pub fn committer(&self) -> Committer {
        Committer {
            inner_tx: self.inner_tx.clone(),
            meta: self.meta.clone(),
        }
    }

    /// 更新已知 mount prefix 集合（重载改 mounts 时调用；设计 §9.6）。
    pub async fn set_prefixes(&self, prefixes: Vec<String>) {
        *self.prefixes.lock().await = prefixes;
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn auth_guard(&self) -> Option<&Arc<dyn AuthGuard>> {
        self.auth_guard.as_ref()
    }
}

/// 回环判定（auth 未配置时仅回环可调）。
fn is_loopback(addr: SocketAddr) -> bool {
    addr.ip().is_loopback()
}

/// 外层单一 fallback。
async fn system_fallback(
    State(rs): State<Arc<ReloadState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    req: Request<Body>,
) -> Response<Body> {
    let path = req.uri().path().to_string();
    // 在已知 prefix 集合上分流 _system / health。
    let prefixes = rs.prefixes.lock().await.clone();
    for p in &prefixes {
        let base = p.trim_end_matches('/');
        let health_new = format!("{base}/_system/health");
        let health_old = format!("{base}/health");
        let config = format!("{base}/_system/config");
        if path == health_new {
            return health_ok(&rs).await;
        }
        if path == health_old {
            tracing::warn!("deprecated health path {health_old} (use {health_new})");
            return health_ok(&rs).await;
        }
        if path == config {
            return handle_config(rs, addr, req, &path).await;
        }
    }
    // 非系统路径 → 原样转发当前内层 Router（不 nest、不剥前缀）。
    let inner = rs.inner_rx.borrow().clone();
    match (*inner).clone().oneshot(req).await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::error!("inner router oneshot failed: {e}");
            (StatusCode::BAD_GATEWAY, Json(Value::Null)).into_response()
        }
    }
}

/// health 摘要（不经内层证书门禁，运维到期可用）。
async fn health_ok(rs: &Arc<ReloadState>) -> Response<Body> {
    let meta = rs.meta.lock().await;
    Json(serde_json::json!({
        "status": "OK",
        "generation": meta.generation,
        "config_path": meta.config_path.display().to_string(),
    }))
    .into_response()
}

/// `_system/config` GET（摘要）/ POST（触发热更）。
async fn handle_config(
    rs: Arc<ReloadState>,
    addr: SocketAddr,
    req: Request<Body>,
    _path: &str,
) -> Response<Body> {
    if !rs.enabled() {
        return StatusCode::NOT_FOUND.into_response();
    }
    // 守卫：auth 配置 → Bearer 验签；未配 → 仅回环。
    match rs.auth_guard() {
        Some(g) => {
            let headers: &HeaderMap = req.headers();
            let authorization = headers
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok());
            match g.verify(
                "/_system/config",
                req.method().as_str(),
                authorization,
                None,
            ) {
                Ok(_) => {}
                Err(msg) => {
                    return (StatusCode::UNAUTHORIZED, Json(Value::String(msg))).into_response();
                }
            }
        }
        None => {
            if !is_loopback(addr) {
                return (
                    StatusCode::FORBIDDEN,
                    Json(Value::String(
                        "reload_api requires loopback or auth".to_string(),
                    )),
                )
                    .into_response();
            }
        }
    }

    match *req.method() {
        Method::GET => {
            let meta = rs.meta.lock().await;
            Json(serde_json::json!({
                "config_path": meta.config_path.display().to_string(),
                "sha256": meta.cfg_sha256,
                "loaded_at": meta.loaded_at,
                "generation": meta.generation,
            }))
            .into_response()
        }
        Method::POST => {
            // 无 body：非空 body 拒收 400。
            let body = match axum::body::to_bytes(req.into_body(), 1024).await {
                Ok(b) => b,
                Err(e) => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(Value::String(format!("read body: {e}"))),
                    )
                        .into_response();
                }
            };
            if !body.is_empty() {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(Value::String(
                        "reload_api POST must have empty body".to_string(),
                    )),
                )
                    .into_response();
            }
            // 串行化：并发 POST → 后来者 409 busy。
            let _guard = match rs.reload_lock.try_lock() {
                Ok(g) => g,
                Err(_) => {
                    return (
                        StatusCode::CONFLICT,
                        Json(Value::String("reload in progress".to_string())),
                    )
                        .into_response();
                }
            };
            let exec = rs.executor.lock().await;
            let exec = match exec.as_ref() {
                Some(e) => e,
                None => {
                    return (
                        StatusCode::SERVICE_UNAVAILABLE,
                        Json(Value::String("reload executor not ready".to_string())),
                    )
                        .into_response();
                }
            };
            match exec() {
                Ok(outcome) => {
                    if outcome.unchanged {
                        Json(serde_json::json!({
                            "code": 0,
                            "msg": "unchanged",
                            "generation": outcome.generation,
                        }))
                        .into_response()
                    } else {
                        Json(serde_json::json!({
                            "code": 0,
                            "data": {
                                "generation": outcome.generation,
                                "applied": outcome.applied,
                                "restart_pending": outcome.restart_pending,
                                "kept_listener": outcome.kept_listener,
                            }
                        }))
                        .into_response()
                    }
                }
                Err((status, msg)) => (
                    StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                    Json(Value::String(msg)),
                )
                    .into_response(),
            }
        }
        _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
    }
}

/// 启动 swappable 服务：listener 永生 + 外层单一 fallback（注入 ConnectInfo）。
/// `reload_api=false` 时请勿调用（改走 `serve_router`）。
pub async fn serve_swappable(
    listener: tokio::net::TcpListener,
    rs: Arc<ReloadState>,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let router = Router::new().fallback(any(system_fallback)).with_state(rs);
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await
}
