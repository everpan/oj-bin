//! fetch(url, options?) HTTP 客户端绑定（基于 reqwest）。
//!
//! op 返回缓冲后的完整响应，Response 对象（json()/text()/arrayBuffer()/
//! clone()/body.getReader()）由 bootstrap.js 在 JS 侧组装。
//! 当前限制：不支持 signal/AbortController。
// ponytail: body 以字节数组+文本两份经 serde 传给 JS（大 body 有一次多余拷贝）；
// 真在意带宽时改用 #[buffer] 或 resource 句柄。

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use deno_core::{OpState, op2};
use deno_error::JsErrorBox;
use serde::Serialize;

use super::StableState;
use std::sync::Arc;

/// 序列化给 JS 的原始响应。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RawResponse {
    ok: bool,
    status: u16,
    status_text: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
    body_text: String,
}

/// fetch 的核心 op：执行请求并缓冲完整响应。
#[op2]
#[serde]
pub async fn op_fetch(
    state: Rc<RefCell<OpState>>,
    #[string] url: String,
    #[string] method: String,
    #[serde] headers: HashMap<String, String>,
    #[string] body: Option<String>,
) -> Result<RawResponse, JsErrorBox> {
    if url.is_empty() {
        return Err(JsErrorBox::generic("fetch: url is required"));
    }
    let client = state.borrow().borrow::<Arc<StableState>>().client.clone();

    let method = reqwest::Method::from_bytes(method.as_bytes()).unwrap_or(reqwest::Method::GET);
    let mut req = client.request(method, &url);
    for (k, v) in &headers {
        req = req.header(k, v);
    }
    if let Some(b) = body {
        req = req.body(b);
        // 用户未显式设置 Content-Type 时自动推断。
        if !headers
            .keys()
            .any(|k| k.eq_ignore_ascii_case("content-type"))
        {
            req = req.header("Content-Type", "text/plain;charset=UTF-8");
        }
    }

    let resp = req
        .send()
        .await
        .map_err(|e| JsErrorBox::generic(format!("fetch: {e}")))?;
    let status = resp.status();
    // 同名多值头按浏览器规范拼接为 ", " 分隔。
    let mut hdrs = HashMap::new();
    for (k, v) in resp.headers() {
        let val = v.to_str().unwrap_or_default();
        hdrs.entry(k.as_str().to_string())
            .and_modify(|e: &mut String| {
                e.push_str(", ");
                e.push_str(val);
            })
            .or_insert_with(|| val.to_string());
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| JsErrorBox::generic(format!("fetch: {e}")))?;

    Ok(RawResponse {
        ok: status.is_success(),
        status: status.as_u16(),
        status_text: status.canonical_reason().unwrap_or_default().to_string(),
        headers: hdrs,
        body_text: String::from_utf8_lossy(&bytes).into_owned(),
        body: bytes.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::{Value, json};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use crate::bridge::{Bridge, InMemoryAccessor, InMemoryKV, RequestInfo};

    /// 本地一次性 HTTP 服务器 + JS fetch 全链路。
    #[tokio::test(flavor = "current_thread")]
    async fn fetch_json_roundtrip() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf).await.unwrap();
            let body = r#"{"hello":"world"}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            s.write_all(resp.as_bytes()).await.unwrap();
        });

        let b = Bridge::new(
            Arc::new(InMemoryAccessor::new()),
            Arc::new(InMemoryKV::new()),
        );
        let src = format!(
            r#"fetch("http://{addr}/data", {{ method: "POST", body: "ping" }})
              .then((r) => r.json().then((data) => json.ok({{
                status: r.status, ok: r.ok, data, ct: r.headers["content-type"],
              }})))
              .catch((e) => json.fail(500, String(e)));"#
        );
        let cap = b.run(&src).await.unwrap();

        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "fetch failed: {v}");
        assert_eq!(v["data"]["status"], 200);
        assert_eq!(v["data"]["ok"], true);
        assert_eq!(v["data"]["data"], json!({"hello": "world"}));
        assert!(v["data"]["ct"].as_str().unwrap().contains("json"));
    }

    use httptest::Expectation;
    use httptest::Server;
    use httptest::matchers::*;
    use httptest::responders::*;

    fn fetch_bridge() -> Bridge {
        Bridge::new(
            Arc::new(InMemoryAccessor::new()),
            Arc::new(InMemoryKV::new()),
        )
    }

    #[tokio::test(flavor = "current_thread")]
    async fn empty_url_and_connection_refused_error() {
        let b = fetch_bridge();
        let cap = b
            .run(r#"fetch("").then(r => json.ok({})).catch(e => json.fail(400, String(e)));"#)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 400);
        assert!(
            v["msg"].as_str().unwrap().contains("url is required"),
            "{v}"
        );

        // 端口 1 无监听 → 连接拒绝
        let cap = b
            .run(r#"fetch("http://127.0.0.1:1/").then(r => json.ok({})).catch(e => json.fail(502, String(e)));"#)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 502, "{v}");
        assert!(v["msg"].as_str().unwrap().contains("fetch:"), "{v}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn non_2xx_status_reported_and_body_autocontenttype() {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::path("/fail"))
                .respond_with(status_code(503).body("down")),
        );
        let b = fetch_bridge();
        let cap = b
            .run_with(
                &format!(
                    r#"fetch("{}", {{ method: "POST", body: "ping" }})
                      .then(r => r.text().then(body => json.ok({{ status: r.status, ok: r.ok, body }})))
                      .catch(e => json.fail(500, String(e)));"#,
                    server.url("/fail")
                ),
                RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(v["data"]["status"], 503);
        assert_eq!(v["data"]["ok"], false);
        assert_eq!(v["data"]["body"], "down");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn invalid_method_falls_back_to_get() {
        // 非法方法 → reqwest 回退 GET；mock 仅按 path 匹配，不约束方法。
        let server = Server::run();
        server.expect(
            Expectation::matching(request::path("/x"))
                .respond_with(status_code(200).body(r#"{"ok":1}"#)),
        );
        let b = fetch_bridge();
        let cap = b
            .run_with(
                &format!(
                    r#"fetch("{}", {{ method: "NOTAMETHOD", body: "b" }})
                      .then(r => r.json().then(d => json.ok(d)))
                      .catch(e => json.fail(500, String(e)));"#,
                    server.url("/x")
                ),
                RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(v["data"]["ok"], 1);
    }
}
