//! `bridge.call()`（v0.1.131，上游 R7）的进程内派发接线：隔离内部 actor 池（R7-B）。
//!
//! 死锁背景：公共 actor 池并发上限 = pool_size；handler 经 `fetch` 回环调用本进程
//! 另一 endpoint 时，内层 handler 还要占一个公共槽——并发 ≥ pool_size 全池互相
//! 等待、永久挂死。本模块让 `bridge.call` 的目标 endpoint 在**独立 actor 池**上
//! 执行（`mark_internal` 池），公共池不再被自环耗尽。
//!
//! 结构：根 crate 的 op（`dispatch_bridge::op_dispatch`）读共享槽拿 hook；hook 在
//! `app_with_exec` 装配期 set 进槽（槽位后填，照搬 `reload_bridge` 惯例），持一份
//! `actor` 换绑为内部分发池的 `AppState` 克隆，虚拟请求原样走既有
//! `handle()`/`mcp_http` 管线（准入/租户/超时/契约语义零分叉）。热更每代重建自己的
//! 槽与 hook（各代 `from_config` 独立接线），不存在跨代串号。

use std::cell::Cell;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Uri};
use axum::response::Response;
use only_js::bridge::dispatch_bridge::{DispatchHook, DispatchReq, DispatchResp, DispatchSlot};
use serde_json::Value;

use crate::{AppState, RuntimeMount};

// 内部池 actor 线程标记：为 true 时，本线程上运行的 handler 已是 dispatch 目标。
thread_local! {
    static ON_INTERNAL_POOL: Cell<bool> = const { Cell::new(false) };
}

/// 包装 Bridge 工厂：在内部池 actor 线程上置位 `ON_INTERNAL_POOL`（工厂在 actor
/// 线程内被调用，见 `JsActor::pool`）。二级嵌套护栏 `dispatch_virtual` 读它。
pub fn mark_internal(
    make: impl Fn() -> only_js::bridge::Bridge + Send + Sync + 'static,
) -> impl Fn() -> only_js::bridge::Bridge + Send + Sync + 'static {
    move || {
        ON_INTERNAL_POOL.with(|f| f.set(true));
        make()
    }
}

/// 响应体缓冲上限（64 MiB）与有界等待：流式端点（SSE/下载）的流不透传，
/// 超限/超时按 502 收敛（handler 自身的 route timeout 通常先熔断）。
const DISPATCH_BODY_CAP: usize = 64 * 1024 * 1024;
const DISPATCH_BODY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// 装配 hook 并 set 进共享槽（`app_with_exec` 每代调用一次；OnceLock 重复 set
/// 忽略——每代持有自己的槽，重复 set 仅防御性容错）。
pub(crate) fn install_dispatch_hook(
    st: &AppState,
    d_actor: crate::actor::JsActor,
    slot: &Arc<DispatchSlot>,
) {
    let inner = AppState {
        actor: d_actor,
        ..st.clone()
    };
    let hook: DispatchHook = Arc::new(move |req: DispatchReq| {
        let st = inner.clone();
        Box::pin(async move { dispatch_virtual(st, req).await })
    });
    let _ = slot.set(hook);
}

/// 虚拟请求派发：保留路由（与 `app_with_exec` 注册循环同表，改动须两侧同步）
/// → catch-all `handle()` 管线（actor 已换绑内部分发池）→ Response 投影回 DispatchResp。
pub(crate) async fn dispatch_virtual(st: AppState, req: DispatchReq) -> DispatchResp {
    // 二级嵌套护栏：内部分发池上运行的 handler 再 call → 503。放开会让等待环
    // 在池内重现（与公共池自环死锁同构）；深度 1 覆盖代理/编排的全部既定场景。
    if ON_INTERNAL_POOL.with(Cell::get) {
        return DispatchResp {
            status: 503,
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: only_js::bridge::fail(503, "nested bridge.call is not supported", &Value::Null).0,
        };
    }
    let method = match req.method.parse::<Method>() {
        Ok(m) => m,
        Err(_) => return fail_dispatch(400, &format!("invalid method: {}", req.method)),
    };
    if !req.path.starts_with('/') {
        return fail_dispatch(400, "dispatch path must start with '/'");
    }
    let uri: Uri = match req.path.parse() {
        Ok(u) => u,
        Err(_) => return fail_dispatch(400, "invalid dispatch path"),
    };
    let mut headers = HeaderMap::new();
    for (k, v) in &req.headers {
        if let (Ok(n), Ok(val)) = (k.parse::<HeaderName>(), HeaderValue::from_str(v)) {
            headers.append(n, val);
        }
    }
    // 保留基础设施端点（与 app_with_exec 的注册循环同表；/blob 下载/直传与
    // 业务路由都在 catch-all handle() 内，无需在此点名）。
    let path = uri.path();
    let verb = method.as_str();
    for m in &st.mounts {
        let RuntimeMount::Api { prefix, .. } = m else {
            continue;
        };
        let p = prefix.trim_end_matches('/');
        let hit = match (verb, path) {
            ("GET", x) if x == format!("{p}/health") => {
                Some(crate::health_handler(State(st.clone())).await)
            }
            ("GET", x) if x == format!("{p}/plugins") => {
                Some(crate::plugins_handler(State(st.clone())).await)
            }
            ("POST", x) if x == format!("{p}/mcp") && st.pipeline.mcp.is_some() => Some(
                crate::mcp_http::handle(
                    State(st.clone()),
                    uri.clone(),
                    headers.clone(),
                    Body::from(req.body.clone()),
                )
                .await,
            ),
            ("POST", x) if x == format!("{p}/rpc") && st.pipeline.rpc => Some(
                crate::mcp_http::rpc_handle(
                    State(st.clone()),
                    uri.clone(),
                    headers.clone(),
                    Body::from(req.body.clone()),
                )
                .await,
            ),
            _ => None,
        };
        if let Some(resp) = hit {
            return resp_to_dispatch(resp).await;
        }
    }
    // 业务路由 / blob / dev 兜底 / 配对 web：catch-all 管线（AppState 的 actor
    // 已换绑内部分发池，目标 handler 不占公共槽）。
    let mut request = match axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::from(req.body))
    {
        Ok(r) => r,
        Err(_) => return fail_dispatch(500, "dispatch request build failed"),
    };
    *request.headers_mut() = headers;
    resp_to_dispatch(crate::handle(State(st), request).await).await
}

/// Response → DispatchResp（体整体缓冲；流式/超限/超时按 502 收敛）。
async fn resp_to_dispatch(resp: Response) -> DispatchResp {
    let status = resp.status().as_u16();
    let headers: Vec<(String, String)> = resp
        .headers()
        .iter()
        .filter_map(|(k, v)| Some((k.to_string(), v.to_str().ok()?.to_string())))
        .collect();
    let body = match tokio::time::timeout(
        DISPATCH_BODY_TIMEOUT,
        axum::body::to_bytes(resp.into_body(), DISPATCH_BODY_CAP),
    )
    .await
    {
        Ok(Ok(b)) => b.to_vec(),
        _ => {
            return DispatchResp {
                status: 502,
                headers: vec![("content-type".to_string(), "application/json".to_string())],
                body: only_js::bridge::fail(
                    502,
                    "dispatch response is streamed or exceeds the body cap",
                    &Value::Null,
                )
                .0,
            };
        }
    };
    DispatchResp {
        status,
        headers,
        body,
    }
}

fn fail_dispatch(status: u16, msg: &str) -> DispatchResp {
    DispatchResp {
        status,
        headers: vec![("content-type".to_string(), "application/json".to_string())],
        body: only_js::bridge::fail(i32::from(status), msg, &Value::Null).0,
    }
}
