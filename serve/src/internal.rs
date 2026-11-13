//! `bridge.call()`（v0.1.131，上游 R7）的进程内派发接线：隔离内部 actor 池（R7-B）。
//!
//! 死锁背景：公共 actor 池并发上限 = pool_size；handler 经 `fetch` 回环调用本进程
//! 另一 endpoint 时，内层 handler 还要占一个公共槽——并发 ≥ pool_size 全池互相
//! 等待、永久挂死。本模块让 `bridge.call` 的目标 endpoint 在**独立 actor 池**上
//! 执行（`mark_internal` 池），公共池不再被自环耗尽。
//!
//! 结构：根 crate 的 op（`dispatch_bridge::op_dispatch`）读共享槽拿 hook；hook 在
//! `app_with_exec` 装配期 install 进槽（槽位后填，照搬 `reload_bridge` 惯例），持一份
//! `actor` 换绑为内部分发池的 `AppState` 克隆，虚拟请求原样走既有
//! `handle()`/`mcp_http` 管线（准入/租户/超时/契约语义零分叉）。槽为**可换代**
//! 结构（`DispatchSlot::install` 覆盖，评审 #1）：backend 复用热更路径下每代
//! `app_with_exec` 重新 install——bridge.call 永随当前代路由表，不存在跨代串号。

use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Uri};
use axum::response::Response;
use only_js::bridge::dispatch_bridge::{
    DispatchHook, DispatchReq, DispatchResp, DispatchSlot, on_internal_pool, set_internal_pool,
};
use serde_json::Value;

use crate::{AppState, RuntimeMount};

/// 包装 Bridge 工厂：在内部池 actor 线程上置位共享的「内部池」线程标记
///（工厂在 actor 线程内被调用，见 `JsActor::pool`）。二级嵌套护栏
/// `dispatch_virtual` 与 bootstrap.js 的 fetch 回环护栏共用该标记。
pub fn mark_internal(
    make: impl Fn() -> only_js::bridge::Bridge + Send + Sync + 'static,
) -> impl Fn() -> only_js::bridge::Bridge + Send + Sync + 'static {
    move || {
        set_internal_pool();
        make()
    }
}

/// 响应体缓冲上限（64 MiB）与有界等待：流式端点（SSE/下载）的流不透传，
/// 超限/超时按 502 收敛（handler 自身的 route timeout 通常先熔断）。
const DISPATCH_BODY_CAP: usize = 64 * 1024 * 1024;
const DISPATCH_BODY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// 装配 hook 并 install 进共享槽（`app_with_exec` **每代**调用一次：换代覆盖
/// install——评审 #1，OnceLock `let _ = set` 曾丢弃 gen-2 hook、桥永绑 gen-1
/// 路由表；覆盖后 bridge.call 始终命中当前代）。
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
    slot.install(hook);
}

/// 虚拟请求派发：保留路由（与 `app_with_exec` 注册循环同表，改动须两侧同步）
/// → catch-all `handle()` 管线（actor 已换绑内部分发池）→ Response 投影回 DispatchResp。
pub(crate) async fn dispatch_virtual(st: AppState, req: DispatchReq) -> DispatchResp {
    // 二级嵌套护栏：内部分发池上运行的 handler 再 call → 503。放开会让等待环
    // 在池内重现（与公共池自环死锁同构）；深度 1 覆盖代理/编排的全部既定场景。
    // （fetch 回环同构面已由 bootstrap.js 的 loopback 护栏在 JS 层拦下。）
    if on_internal_pool() {
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
    // 业务路由都在 catch-all handle() 内，无需在此点名）。method 不匹配先记
    // Allow（末尾统一 405 + Allow，对齐 axum 路由语义，评审 #12）；HEAD 归一
    // 到 GET（axum get() 自动服务 HEAD——dispatch 与真实 HTTP 同语义），且
    // HEAD 响应体按协议清空。
    let path = uri.path();
    let verb = if method == Method::HEAD {
        "get"
    } else {
        method.as_str()
    };
    let mut allow: Vec<&'static str> = Vec::new();
    let mut hit: Option<Response> = None;
    for m in &st.mounts {
        let RuntimeMount::Api { prefix, .. } = m else {
            continue;
        };
        let p = prefix.trim_end_matches('/');
        let table: [(&str, &str, bool); 4] = [
            ("GET", "health", true),
            ("GET", "plugins", true),
            ("POST", "mcp", st.pipeline.mcp.is_some()),
            ("POST", "rpc", st.pipeline.rpc),
        ];
        for (v, suffix, on) in table {
            if !on || path != format!("{p}/{suffix}") {
                continue;
            }
            allow.push(v);
            if v.eq_ignore_ascii_case(verb) && hit.is_none() {
                hit = Some(match suffix {
                    "health" => crate::health_handler(State(st.clone())).await,
                    "plugins" => crate::plugins_handler(State(st.clone())).await,
                    "mcp" => {
                        crate::mcp_http::handle(
                            State(st.clone()),
                            uri.clone(),
                            headers.clone(),
                            Body::from(req.body.clone()),
                        )
                        .await
                    }
                    _ => {
                        crate::mcp_http::rpc_handle(
                            State(st.clone()),
                            uri.clone(),
                            headers.clone(),
                            Body::from(req.body.clone()),
                        )
                        .await
                    }
                });
            }
        }
    }
    if let Some(mut resp) = hit {
        if method == Method::HEAD {
            *resp.body_mut() = Body::empty();
        }
        return resp_to_dispatch(resp).await;
    }
    if !allow.is_empty() {
        return DispatchResp {
            status: 405,
            headers: vec![
                ("content-type".to_string(), "application/json".to_string()),
                ("allow".to_string(), allow.join(", ")),
            ],
            body: only_js::bridge::fail(405, "method not allowed", &Value::Null).0,
        };
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
    // 头值保真（评审 S3）：非 UTF-8 字节经 lossy 保留（原 to_str().ok()? 整体
    // 丢弃——如 content-disposition 里的 UTF-8 文件名在代理链上静默消失）。
    let headers: Vec<(String, String)> = resp
        .headers()
        .iter()
        .map(|(k, v)| {
            (
                k.to_string(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
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
