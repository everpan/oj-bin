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
use deno_core::{JsBuffer, OpState, op2};
use deno_error::JsErrorBox;

use crate::app::ClientTransport;

/// 进程内派发响应（op 返回给 JS `client.{method}` 的结果）。
/// 同名多值头以 ", " 拼接（修正 #8）。
/// `oj test --anonymous` 标志（v0.1.20）：置于 OpState，供 op_client_dispatch 把测试
/// 请求标记为匿名（公开面 handler 的 db.asTenant 需要）。
pub struct TestAnonymous(pub bool);

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
    //    `--anonymous`（v0.1.20）：测试态显式授信匿名请求，让公开面 handler（走
    //    db.asTenant）在 `oj test` 下可测——生产只有 anonymous_paths 豁免才置位。
    {
        let anonymous = state
            .borrow()
            .try_borrow::<TestAnonymous>()
            .is_some_and(|a| a.0);
        let mut g = state.borrow_mut();
        let rs = g.borrow_mut::<only_js::bridge::ReqState>();
        rs.reset(only_js::bridge::RequestInfo {
            anonymous,
            ..Default::default()
        });
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

// ----- client.ws：L1 WS 帧测试面（v0.1.16）-----
//
// oneshot 派发止步于 101 upgrade，收发不了帧；这里起真服务：首次 open 时
// 127.0.0.1:0 bind + `axum::serve(app.router())`（tokio::spawn，与测试共用
// current_thread 运行时——op await 点位让路，顺序 JS 调用无死锁），客户端走
// tokio-tungstenite 连 `ws://127.0.0.1:{port}{base}{path}`。连接与「最后一帧」
// 槽位挂在 OpState；发送/读帧 op 先把流从表里取出、await 完再放回（修正 #4
// 借位纪律：不持 Ref 跨 await）。

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type WsStream = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

#[derive(Default)]
pub struct ClientWs {
    /// 惰性服务端口（None = 尚未起 serve）。
    port: Option<u16>,
    conns: HashMap<u64, WsStream>,
    /// op_client_ws_next 读到的最后一帧字节（binary, bytes），op_client_ws_last_bytes 取走。
    last: HashMap<u64, (bool, Vec<u8>)>,
    next_id: u64,
}

/// op_client_ws_next 结果：None（超时无帧）由 Option 表达；frame=false = 对端关闭。
#[derive(serde::Serialize, serde::Deserialize)]
pub struct ClientWsNext {
    pub frame: bool,
    pub binary: bool,
}

/// 取出连接（借位纪律：await 前从 OpState 表里 remove，完事再 insert 回）。
fn take_conn(state: &Rc<RefCell<OpState>>, id: u64) -> Result<WsStream, JsErrorBox> {
    let mut g = state.borrow_mut();
    g.borrow_mut::<ClientWs>()
        .conns
        .remove(&id)
        .ok_or_else(|| JsErrorBox::generic(format!("client.ws: no connection #{id}")))
}

fn put_conn(state: &Rc<RefCell<OpState>>, id: u64, c: WsStream) {
    state
        .borrow_mut()
        .borrow_mut::<ClientWs>()
        .conns
        .insert(id, c);
}

#[op2]
#[serde]
pub async fn op_client_ws_open(
    state: Rc<RefCell<OpState>>,
    #[string] path: String,
) -> Result<u64, JsErrorBox> {
    let app: Arc<crate::app::App> = state.borrow().borrow::<Arc<crate::app::App>>().clone();
    let base = app.base().to_string();
    // 惰性端口：首次 open 才 bind + serve（None → Some）。
    let port = {
        let g = state.borrow();
        g.borrow::<ClientWs>().port
    };
    let port = match port {
        Some(p) => p,
        None => {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .map_err(|e| JsErrorBox::generic(format!("client.ws bind: {e}")))?;
            let p = listener
                .local_addr()
                .map_err(|e| JsErrorBox::generic(e.to_string()))?
                .port();
            let router = app.router();
            tokio::spawn(async move {
                let _ = axum::serve(listener, router).await;
            });
            state.borrow_mut().borrow_mut::<ClientWs>().port = Some(p);
            eprintln!("[oj test] client.ws serving on 127.0.0.1:{p}");
            p
        }
    };
    let url = format!("ws://127.0.0.1:{port}{base}{path}");
    let (stream, _) = tokio_tungstenite::connect_async(url.clone())
        .await
        .map_err(|e| JsErrorBox::generic(format!("client.ws connect {url}: {e}")))?;
    let mut g = state.borrow_mut();
    let ws = g.borrow_mut::<ClientWs>();
    ws.next_id += 1;
    let id = ws.next_id;
    ws.conns.insert(id, stream);
    Ok(id)
}

#[op2]
pub async fn op_client_ws_send(
    state: Rc<RefCell<OpState>>,
    #[bigint] id: u64,
    #[string] text: String,
) -> Result<(), JsErrorBox> {
    let mut c = take_conn(&state, id)?;
    let r = c.send(Message::text(text.clone())).await;
    put_conn(&state, id, c);
    r.map_err(|e| JsErrorBox::generic(format!("client.ws send: {e}")))
}

#[op2]
pub async fn op_client_ws_send_bin(
    state: Rc<RefCell<OpState>>,
    #[bigint] id: u64,
    #[buffer] data: JsBuffer,
) -> Result<(), JsErrorBox> {
    let mut c = take_conn(&state, id)?;
    let r = c.send(Message::Binary(data.to_vec().into())).await;
    put_conn(&state, id, c);
    r.map_err(|e| JsErrorBox::generic(format!("client.ws send: {e}")))
}

#[op2]
#[serde]
pub async fn op_client_ws_next(
    state: Rc<RefCell<OpState>>,
    #[bigint] id: u64,
    #[bigint] timeout_ms: u64,
) -> Result<Option<ClientWsNext>, JsErrorBox> {
    let mut c = take_conn(&state, id)?;
    let wait = tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), c.next()).await;
    let res = match wait {
        // 超时：流放回，返回 None（无帧，不视为关闭）。
        Err(_elapsed) => {
            put_conn(&state, id, c);
            return Ok(None);
        }
        // 流结束（对端关闭）：不再放回。
        Ok(None) => {
            state.borrow_mut().borrow_mut::<ClientWs>().last.remove(&id);
            return Ok(Some(ClientWsNext {
                frame: false,
                binary: false,
            }));
        }
        Ok(Some(Err(e))) => {
            state.borrow_mut().borrow_mut::<ClientWs>().last.remove(&id);
            return Err(JsErrorBox::generic(format!("client.ws recv: {e}")));
        }
        Ok(Some(Ok(msg))) => match msg {
            Message::Text(t) => (false, t.as_bytes().to_vec()),
            Message::Binary(b) => (true, b.to_vec()),
            // Ping/Pong 控制帧直接继续等（timeout 已耗掉一部分，简化：靠上层重试）。
            Message::Close(_) => {
                state.borrow_mut().borrow_mut::<ClientWs>().last.remove(&id);
                return Ok(Some(ClientWsNext {
                    frame: false,
                    binary: false,
                }));
            }
            other => {
                put_conn(&state, id, c);
                return Err(JsErrorBox::generic(format!(
                    "client.ws: unexpected frame {other:?}"
                )));
            }
        },
    };
    let (binary, bytes) = res;
    {
        let mut g = state.borrow_mut();
        g.borrow_mut::<ClientWs>().last.insert(id, (binary, bytes));
    }
    put_conn(&state, id, c);
    Ok(Some(ClientWsNext {
        frame: true,
        binary,
    }))
}

#[op2]
#[buffer]
pub async fn op_client_ws_last_bytes(
    state: Rc<RefCell<OpState>>,
    #[bigint] id: u64,
) -> Result<Vec<u8>, JsErrorBox> {
    let g = state.borrow();
    let ws = g.borrow::<ClientWs>();
    ws.last
        .get(&id)
        .map(|(_, b)| b.clone())
        .ok_or_else(|| JsErrorBox::generic("client.ws: no frame buffered (call next() first)"))
}

#[op2]
pub async fn op_client_ws_close(
    state: Rc<RefCell<OpState>>,
    #[bigint] id: u64,
) -> Result<(), JsErrorBox> {
    if let Ok(mut c) = take_conn(&state, id) {
        let _ = c.send(Message::Close(None)).await;
    }
    Ok(())
}

deno_core::extension!(
    oj_test_ext,
    ops = [
        op_client_dispatch,
        op_client_ws_open,
        op_client_ws_send,
        op_client_ws_send_bin,
        op_client_ws_next,
        op_client_ws_last_bytes,
        op_client_ws_close,
    ],
    esm_entry_point = "ext:oj_test_ext/test_bootstrap.js",
);

/// oj_test_ext 的 ESM 源（编译期内嵌；理由见 `only_js::bridge::bridge_ext_init` ——
/// `esm = [dir ...]` 会把构建机绝对路径烧进二进制，非构建机上无法初始化 JsRuntime）。
const OJ_TEST_ESM: &[deno_core::ExtensionFileSource] = &[deno_core::ExtensionFileSource::new(
    "ext:oj_test_ext/test_bootstrap.js",
    deno_core::ascii_str_include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/test_ext/test_bootstrap.js"
    )),
)];

/// 构造 `oj_test_ext`（test_bootstrap.js 编进二进制）。
pub fn oj_test_ext_init() -> deno_core::Extension {
    let mut ext = oj_test_ext::init();
    ext.esm_files = std::borrow::Cow::Borrowed(OJ_TEST_ESM);
    ext
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_multi_value_header_when_to_map_then_joined_comma_space() {
        // 浏览器规范：同名多值头以 ", " 拼接（修正 #8 的业务约定）。
        let mut h = axum::http::HeaderMap::new();
        h.insert("set-cookie", "a=1".parse().unwrap());
        h.append("set-cookie", "b=2".parse().unwrap());
        h.insert("x-single", "s".parse().unwrap());
        let m = header_map_to_map(&h);
        assert_eq!(m["set-cookie"], "a=1, b=2");
        assert_eq!(m["x-single"], "s");
    }

    /// 回归护栏：test_ext 的 ESM 源必须内嵌（不得依赖构建机路径）。
    #[test]
    fn oj_test_ext_esm_source_is_embedded() {
        assert_eq!(OJ_TEST_ESM.len(), 1);
        assert!(OJ_TEST_ESM.iter().all(|f| f.is_runtime_loadable()));
        assert_eq!(
            OJ_TEST_ESM[0].specifier,
            "ext:oj_test_ext/test_bootstrap.js"
        );
    }
}
