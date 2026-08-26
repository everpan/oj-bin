//! L1 测试框架扩展（收口于 oj crate；bridge 零改动、零 axum 依赖）。
//!
//! - `op_client_dispatch`：JS `client` 全局底层。op 经 OpState 取注入的
//!   `Arc<dyn ClientTransport>`（即 `App`），进程内 `oneshot` 派发（零 TCP，对标
//!   Go Fiber `app.Test`）。遇 101 upgrade 不 `to_bytes`（修正 #3）。
//! - `oj_test_ext`：`deno_core::extension!`，随 `test_bootstrap.js`（esm 入口）注入
//!   `client` 全局 + 轻量 `describe/it/expect/beforeEach` + `client.login` 助手。

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use deno_core::{OpState, op2};
use deno_error::JsErrorBox;

use crate::app::ClientTransport;

/// 进程内派发响应（op 返回给 JS `client.{method}` 的结果）。
/// 同名多值头以 ", " 拼接（修正 #8）。
#[derive(serde::Serialize, serde::Deserialize)]
pub struct ClientResp {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: String,
    /// 101 upgrade：不 to_bytes（修正 #3）。
    pub upgrade: bool,
}

/// HeaderMap → HashMap，多值同名头按浏览器规范以 ", " 拼接。
fn header_map_to_map(h: &axum::http::HeaderMap) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for name in h.keys() {
        let vals: Vec<&str> = h
            .get_all(name)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect();
        out.insert(name.as_str().to_string(), vals.join(", "));
    }
    out
}

/// JS→Rust 派发入口（op_client_dispatch）。
///
/// OpState 借位（修正 #4）：先 clone 出 `Arc<dyn ClientTransport>` 再 drop 借位，
/// 之后才 `.await`，禁止持 `Ref` 跨 await。每次请求重置 `ReqState`（json/http 捕获），
/// 避免跨 `client` 调用串号（与 server `checkout_reset` 一致）。
#[op2]
#[serde]
pub async fn op_client_dispatch(
    state: Rc<RefCell<OpState>>,
    #[string] method: String,
    #[string] path: String,
    #[serde] headers: HashMap<String, String>,
    #[string] body: String,
) -> Result<ClientResp, JsErrorBox> {
    // 1) 取 transport（clone 即复制 Arc，借位随块结束）。OpState 借位模式（修正 #4）。
    let t: Arc<dyn ClientTransport> = {
        let g = state.borrow();
        g.borrow::<Arc<dyn ClientTransport>>().clone()
    };
    // 2) 重置每请求状态（ReqState 在 OpState），防跨请求串号。
    {
        let mut g = state.borrow_mut();
        let rs = g.borrow_mut::<only_js::bridge::ReqState>();
        rs.reset(only_js::bridge::RequestInfo::default());
    }
    // 3) base 拼接（与 app() 路由单一事实来源一致，修正 #7）。
    let uri = format!("{}{}", t.base(), path);
    let mut builder = Request::builder().method(method.as_str()).uri(uri);
    for (k, v) in &headers {
        builder = builder.header(k, v);
    }
    let req = builder
        .body(Body::from(body))
        .map_err(|e| JsErrorBox::generic(e.to_string()))?;
    // 4) 进程内 oneshot 派发（零 TCP）。dispatch 内部已 timeout 包裹。
    let resp = t.dispatch(req).await;
    // 5) 101 upgrade：不 to_bytes（WS 帧循环不经 oneshot，修正 #3）。
    if resp.status() == StatusCode::SWITCHING_PROTOCOLS {
        return Ok(ClientResp {
            status: 101,
            headers: header_map_to_map(resp.headers()),
            body: String::new(),
            upgrade: true,
        });
    }
    // 先取 status/headers（resp 即将被 into_body 移动）。
    let status = resp.status().as_u16();
    let headers = header_map_to_map(resp.headers());
    let bytes = to_bytes(resp.into_body(), usize::MAX)
        .await
        .map_err(|e| JsErrorBox::generic(e.to_string()))?;
    Ok(ClientResp {
        status,
        headers,
        body: String::from_utf8_lossy(&bytes).into_owned(),
        upgrade: false,
    })
}

deno_core::extension!(
    oj_test_ext,
    ops = [op_client_dispatch],
    esm_entry_point = "ext:oj_test_ext/test_bootstrap.js",
    esm = [dir "src/test_ext", "test_bootstrap.js"],
);
