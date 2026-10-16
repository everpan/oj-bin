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
///
/// **单一真源**（ojm PR-3 类型下沉，设计稿 §3）：定义在根 crate
/// `only_js::bridge::reload_bridge`，此处 re-export——HTTP 端点序列化与
/// JS `server.reloadConfig()` 共用同一结构，根 crate 不可反向依赖 serve。
pub use only_js::bridge::reload_bridge::ReloadOutcome;

/// 热更执行闭包类型：`oj` 侧注入。同步执行（内部用 blocking 线程做影子装配），
/// 返回 `Ok(outcome)` 或 `Err((status, msg))`（`status`=HTTP 状态，400/500）。
/// 与根 crate 的 [`only_js::bridge::reload_bridge::ReloadExecutorLike`] 同一类型：
/// 槽位（Extras/StableState.reload）与 HTTP 侧注入的是同一 executor 实例。
pub type ReloadExecutor = only_js::bridge::reload_bridge::ReloadExecutorLike;

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

    /// busy 错误（409）：共享 busy 文案——HTTP 线上行为逐字节不变（v1 既有），
    /// JS 侧 op 映射时叠加 busy 语义点名。
    fn busy() -> (u16, String) {
        (409u16, "reload in progress".to_string())
    }

    /// executor 未就绪（503）：`ReloadState` 已构造但 executor 尚未注入。
    fn not_ready() -> (u16, String) {
        (503u16, "reload executor not ready".to_string())
    }

    /// 共享触发入口（异步形态，HTTP POST 专用）：try_lock(`reload_lock`) 串行化
    /// → 取 executor → 同步执行。JS `server.reloadConfig` 经
    /// [`Self::trigger_sync`]（spawn_blocking 线程）调**同一 executor 实例与
    /// 同一把 `reload_lock`**（共享处理体，设计稿 §3；撞锁 → busy 明确报错，
    /// 不静默并发排队）。
    pub async fn trigger(&self) -> Result<ReloadOutcome, (u16, String)> {
        let _guard = self.reload_lock.try_lock().map_err(|_| Self::busy())?;
        let exec = self.executor.lock().await;
        let exec = exec.as_ref().ok_or_else(Self::not_ready)?;
        exec()
    }

    /// 共享触发入口（同步形态，JS `server.reloadConfig` 经 spawn_blocking 调用）。
    /// tokio Mutex 的 `try_lock` 是同步操作，可在阻塞池线程内完成；`reload_lock`
    /// 已串行化全部触发（HTTP 侧经 [`Self::trigger`] 先抢同一把锁），executor
    /// 锁此处实际无竞争，`try_lock` 失败按 busy 归类（防御性，不可达路径）。
    pub fn trigger_sync(&self) -> Result<ReloadOutcome, (u16, String)> {
        let _guard = self.reload_lock.try_lock().map_err(|_| Self::busy())?;
        let exec = self.executor.try_lock().map_err(|_| Self::busy())?;
        let exec = exec.as_ref().ok_or_else(Self::not_ready)?;
        exec()
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
            // 共享处理体（设计稿 §3）：try_lock → executor → outcome 映射收敛在
            // [`ReloadState::trigger`] / [`outcome_response`]，HTTP POST 与 JS
            // `server.reloadConfig`（槽位闭包 → `trigger_sync`）同源——同一把
            // reload_lock、同一 executor 实例，形状/状态码由结构保证逐字对齐。
            // JS 侧经 spawn_blocking 调用（executor 同步闭包，Send+Sync 已成立）；
            // **既定语义**：触发 reload 的 handler 所在 actor 在热更期间被占用
            // （秒级，DB verify + 转译），完成前不归还池。
            match rs.trigger().await {
                Ok(outcome) => outcome_response(outcome),
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

/// outcome → HTTP 响应（唯一映射点；v1 线上行为逐字节不变）：
/// unchanged → `{code:0,msg:"unchanged",generation}`（无三清单）；
/// 其余 → `{code:0,data:{generation,applied,restart_pending,kept_listener}}`。
fn outcome_response(outcome: ReloadOutcome) -> Response<Body> {
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

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    fn state() -> Arc<ReloadState> {
        ReloadState::new(
            Arc::new(Router::new()),
            vec![],
            None,
            true,
            PathBuf::from("config.yaml"),
            None,
        )
    }

    fn outcome(generation: u64, unchanged: bool) -> ReloadOutcome {
        ReloadOutcome {
            generation,
            unchanged,
            applied: Vec::new(),
            restart_pending: Vec::new(),
            kept_listener: Vec::new(),
        }
    }

    /// 撞 reload_lock → busy（共享串行化语义，HTTP 与 JS 同一把锁）。
    #[tokio::test(flavor = "current_thread")]
    async fn trigger_busy_when_reload_lock_held() {
        let rs = state();
        let _g = rs.reload_lock.try_lock().unwrap();
        let e = rs.trigger().await.unwrap_err();
        assert_eq!(e.0, 409);
        assert_eq!(e.1, "reload in progress");
        // 同步形态（JS spawn_blocking 路径）同一把锁：同样 busy。
        let e = rs.trigger_sync().unwrap_err();
        assert_eq!(e.0, 409);
        assert_eq!(e.1, "reload in progress");
    }

    /// executor 缺席 → 503；注入后 async/sync 两形态同源。
    #[tokio::test(flavor = "current_thread")]
    async fn trigger_not_ready_then_executor_shared() {
        let rs = state();
        let e = rs.trigger().await.unwrap_err();
        assert_eq!(e.0, 503);
        let e = rs.trigger_sync().unwrap_err();
        assert_eq!(e.0, 503);

        let called = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c2 = called.clone();
        rs.set_executor(Arc::new(move || {
            c2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(outcome(7, true))
        }))
        .await;
        let o = rs.trigger().await.unwrap();
        assert!(o.unchanged && o.generation == 7);
        let o = rs.trigger_sync().unwrap();
        assert!(o.unchanged && o.generation == 7);
        assert_eq!(called.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    /// Err((status, msg)) 透传（oj 侧装配失败 400/500 形态）。
    #[tokio::test(flavor = "current_thread")]
    async fn trigger_propagates_executor_error() {
        let rs = state();
        rs.set_executor(Arc::new(|| Err((400u16, "bad config".to_string()))))
            .await;
        let e = rs.trigger().await.unwrap_err();
        assert_eq!(e.0, 400);
        assert_eq!(e.1, "bad config");
    }

    /// outcome → HTTP 响应形状（v1 线上行为逐字节不变）：unchanged 三键无清单，
    /// 其余 code/data 四键。
    #[test]
    fn outcome_response_shapes() {
        let resp = outcome_response(outcome(3, true));
        assert_eq!(resp.status(), StatusCode::OK);
        let body = serde_json::json!({
            "code": 0,
            "msg": "unchanged",
            "generation": 3,
        });
        assert_eq!(
            serde_json::to_string(&body).unwrap(),
            r#"{"code":0,"msg":"unchanged","generation":3}"#
        );

        let resp = outcome_response(ReloadOutcome {
            generation: 4,
            unchanged: false,
            applied: vec!["vars".into()],
            restart_pending: vec!["db".into()],
            kept_listener: vec!["server".into()],
        });
        assert_eq!(resp.status(), StatusCode::OK);
        let body = serde_json::json!({
            "code": 0,
            "data": {
                "generation": 4,
                "applied": ["vars"],
                "restart_pending": ["db"],
                "kept_listener": ["server"],
            }
        });
        assert_eq!(
            serde_json::to_string(&body).unwrap(),
            r#"{"code":0,"data":{"generation":4,"applied":["vars"],"restart_pending":["db"],"kept_listener":["server"]}}"#
        );
    }

    /// busy 期间 executor 不被触碰：串行化先于取 executor（共享处理体的次序契约）。
    #[tokio::test(flavor = "current_thread")]
    async fn busy_precedes_executor_fetch() {
        let rs = state();
        let c = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c2 = c.clone();
        rs.set_executor(Arc::new(move || {
            std::thread::sleep(Duration::from_millis(50));
            c2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(outcome(1, false))
        }))
        .await;
        // 先抢锁模拟在跑的 reload，再触发：busy，executor 未被调用。
        let _g = rs.reload_lock.try_lock().unwrap();
        let e = rs.trigger().await.unwrap_err();
        assert_eq!(e.0, 409);
        assert_eq!(c.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
}
