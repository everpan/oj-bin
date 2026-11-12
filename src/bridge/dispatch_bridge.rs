//! `bridge.call()` 进程内 dispatch 原语（上游 R7 需求，v0.1.131）。
//!
//! 背景：handler 在进程内转发到本进程另一 endpoint（典型：代理 handler → `/api/mcp`
//! tools/call）时，`fetch` 回环会经公共 actor 池二次占槽——并发 ≥ pool_size 即全池
//! 等待、永久挂死。本原语把目标 endpoint 派发到**隔离的内部 actor 池**（R7-B），
//! 公共池不再因自环耗尽；代理 handler 原地 await 即得结果。
//!
//! 注入走槽位后填（照搬 `reload_bridge` 惯例）：[`StableState::dispatch`] 是共享
//! 可填充槽，槽指针构造期冻结（Extras→StableState 注入同一 Arc，桥池多实例共享），
//! 内容后填（`app_with_exec` 装配期 `slot.set`）。可用形态 = 经 `App::from_config`
//! 的 `oj serve` 与 `oj test`（后者同样装配 hook，经真实管线派发）；裸嵌入式
//! （不经 App 自建 Bridge）不填槽 → op 报 `dispatch not available`。热更每代重建
//! 自己的槽与 hook（各代 `from_config` 独立接线），不存在跨代串号。

use std::cell::RefCell;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;

use deno_core::OpState;
use deno_core::op2;
use deno_error::JsErrorBox;

/// 进程内派发请求（op 入参投影；serve 侧经它组虚拟 HTTP 请求走既有管线）。
/// 不参与 serde 反序列化（op 逐字段构造），derive 仅为调用方便利。
#[derive(Clone, Debug)]
pub struct DispatchReq {
    pub method: String,
    /// 以 `/` 开头的路径（可带 `?query`）。
    pub path: String,
    /// 有序头列表（可重名）；serve 侧非法头名/值直接丢弃（同 HTTP 解析容错）。
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// 进程内派发响应（目标 endpoint 的完整 HTTP 响应投影；体整体缓冲，非流式）。
#[derive(Clone, Debug)]
pub struct DispatchResp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// 派发闭包类型：`oj serve` 装配期填进共享槽。异步执行（虚拟请求走既有
/// handle/mcp_http 管线，目标 handler 在隔离内部 actor 池上运行）。
pub type DispatchHook = Arc<
    dyn Fn(DispatchReq) -> Pin<Box<dyn std::future::Future<Output = DispatchResp> + Send>>
        + Send
        + Sync,
>;

/// 共享可填充槽（[`StableState::dispatch`] 的元素类型）：槽指针构造期冻结、
/// 内容后填（`slot.set` 一次，照搬 `reload_bridge` 解环惯例）。
pub type DispatchSlot = std::sync::OnceLock<DispatchHook>;

/// `bridge.call(req)`：进程内派发一个 HTTP 请求到本进程 endpoint。
///
/// ① 槽空（裸嵌入式，不经 App）→ 点名报错（fail-closed，同 reloadConfig）；
/// ② hook 执行在**调用方 actor 线程上 await**——目标 handler 占的是隔离内部池
///    的槽位，不与公共池竞争，自环代理在并发 ≥ pool_size 下不再死锁。
#[op2]
#[serde]
pub async fn op_dispatch(
    state: Rc<RefCell<OpState>>,
    #[string] method: String,
    #[string] path: String,
    #[serde] headers: Vec<(String, String)>,
    /// body 为 base64（与 #[serde] 不冲突的稳妥载体；JS 侧 ojBytesToB64 编码）。
    #[string]
    body_b64: String,
) -> Result<serde_json::Value, JsErrorBox> {
    use base64::Engine;
    let hook = {
        let st = state.borrow();
        st.borrow::<Arc<super::StableState>>()
            .dispatch
            .get()
            .cloned()
    }
    .ok_or_else(|| {
        JsErrorBox::generic(
            "dispatch not available (not running under an App — oj serve / oj test)",
        )
    })?;
    let body = base64::engine::general_purpose::STANDARD
        .decode(&body_b64)
        .map_err(|e| JsErrorBox::generic(format!("bridge.call: invalid body base64: {e}")))?;
    let req = DispatchReq {
        method,
        path,
        headers,
        body,
    };
    let resp = hook(req).await;
    Ok(serde_json::json!({
        "status": resp.status,
        "headers": resp.headers,
        "body_b64": base64::engine::general_purpose::STANDARD.encode(&resp.body),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 槽空（裸嵌入式 Bridge，不经 App）→ bridge.call 点名报错。
    #[tokio::test(flavor = "current_thread")]
    async fn empty_slot_errors() {
        let b = crate::bridge::Bridge::new(
            std::sync::Arc::new(crate::bridge::InMemoryAccessor::new()),
            std::sync::Arc::new(crate::bridge::InMemoryKV::new()),
        );
        let cap = b
            .run_with(
                r#"(async () => {
                   try {
                     await bridge.call({ method: "GET", path: "/x" });
                     json.ok({ threw: false });
                   } catch (e) {
                     json.ok({ threw: true, msg: String(e) });
                   }
                 })();"#,
                Default::default(),
            )
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{}", v["msg"]);
        assert_eq!(v["data"]["threw"], serde_json::json!(true));
        let msg = v["data"]["msg"].as_str().unwrap();
        assert!(msg.contains("dispatch not available"), "{msg}");
    }

    /// 假 hook：请求投影（method/path/headers/body）完整到达、响应映射为
    /// WHATWG Response（status/headers/text()）。
    #[tokio::test(flavor = "current_thread")]
    async fn fake_hook_roundtrip_response_shape() {
        let slot: Arc<DispatchSlot> = Arc::new(std::sync::OnceLock::new());
        let b = crate::bridge::Bridge::with_dbs_and_loader(
            std::collections::HashMap::new(),
            std::sync::Arc::new(crate::bridge::InMemoryKV::new()),
            crate::bridge::SchemaRegistry::new(),
            false,
            None,
            crate::bridge::Extras {
                dispatch: Some(slot.clone()),
                ..Default::default()
            },
        );
        slot.set(Arc::new(|req: DispatchReq| {
            assert_eq!(req.method, "POST");
            assert_eq!(req.path, "/v1/api/echo?a=1");
            assert!(req.headers.iter().any(|(k, v)| k == "x-test" && v == "yes"));
            Box::pin(async move {
                DispatchResp {
                    status: 201,
                    headers: vec![
                        ("content-type".into(), "application/json".into()),
                        (
                            "x-echo".into(),
                            String::from_utf8_lossy(&req.body).into_owned(),
                        ),
                    ],
                    body: br#"{"pong":true}"#.to_vec(),
                }
            })
        }))
        .is_ok()
        .then_some(())
        .expect("slot set");
        let cap = b
            .run_with(
                r#"(async () => {
                   const r = await bridge.call({
                     method: "POST",
                     path: "/v1/api/echo?a=1",
                     headers: { "x-test": "yes" },
                     body: "ping-bytes",
                   });
                   json.ok({
                     status: r.status,
                     echo: r.headers.get("x-echo"),
                     text: await r.text(),
                   });
                 })();"#,
                Default::default(),
            )
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{}", v["msg"]);
        let d = &v["data"];
        assert_eq!(d["status"], 201);
        assert_eq!(d["echo"], "ping-bytes");
        assert_eq!(d["text"], r#"{"pong":true}"#);
    }
}
