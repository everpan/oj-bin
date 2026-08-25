//! es 全局对象：Elasticsearch 薄封装（search / index / del 三 op，Extras 注入）。
//!
//! 只做路径拼装 + 直通 ES 响应体：search → POST {endpoint}/{index}/_search；
//! index → PUT {endpoint}/{index}/_doc/{id}?refresh=true；del → DELETE 同路径。
//! index/id 白名单 `[a-zA-Z0-9_-]+`（防路径注入）；未配置报 "es not configured"；
//! 非 2xx 报错并带 ES 返回体。

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use deno_core::{OpState, op2};
use deno_error::JsErrorBox;

use super::StableState;

/// ES 客户端：endpoint（装配时剪除尾斜杠）+ 独立 reqwest 客户端。
pub struct EsClient {
    pub endpoint: String,
    pub http: reqwest::Client,
}

impl EsClient {
    pub fn new(endpoint: String) -> Self {
        Self {
            endpoint,
            http: reqwest::Client::builder().no_proxy().build().unwrap_or_default(),
        }
    }
}

/// es 轴后端契约（spec §2：先抽 trait，HTTP 实现 EsClient 为首个后端；
/// 阶段 3 起 cdylib 插件经 FFI 适配器实现本 trait）。
#[async_trait::async_trait]
pub trait EsBackend: Send + Sync {
    async fn search(&self, index: &str, dsl: serde_json::Value) -> super::BridgeResult<serde_json::Value>;
    async fn index_doc(&self, index: &str, id: &str, doc: serde_json::Value) -> super::BridgeResult<serde_json::Value>;
    async fn delete_doc(&self, index: &str, id: &str) -> super::BridgeResult<serde_json::Value>;
}

/// 响应直通（BridgeResult 版）：2xx → JSON 体；非 2xx → Err（带 ES 返回体便于排障）。
async fn es_resp_b(resp: reqwest::Response, what: &str) -> super::BridgeResult<serde_json::Value> {
    let status = resp.status();
    if status.is_success() {
        resp.json()
            .await
            .map_err(|e| format!("{what}: parse response: {e}").into())
    } else {
        let body = resp.text().await.unwrap_or_default();
        Err(format!("{what}: HTTP {status}: {body}").into())
    }
}

#[async_trait::async_trait]
impl EsBackend for EsClient {
    async fn search(&self, index: &str, dsl: serde_json::Value) -> super::BridgeResult<serde_json::Value> {
        let url = url_for(&self.endpoint, index, None);
        let resp = self.http.post(&url).json(&dsl).send().await.map_err(|e| format!("es search: {e}"))?;
        es_resp_b(resp, "es search").await
    }
    async fn index_doc(&self, index: &str, id: &str, doc: serde_json::Value) -> super::BridgeResult<serde_json::Value> {
        let url = url_for(&self.endpoint, index, Some(id));
        let resp = self.http.put(&url).json(&doc).send().await.map_err(|e| format!("es index: {e}"))?;
        es_resp_b(resp, "es index").await
    }
    async fn delete_doc(&self, index: &str, id: &str) -> super::BridgeResult<serde_json::Value> {
        let url = url_for(&self.endpoint, index, Some(id));
        let resp = self.http.delete(&url).send().await.map_err(|e| format!("es del: {e}"))?;
        es_resp_b(resp, "es del").await
    }
}

/// index/id 白名单：字母数字下划线连字符（防路径注入）。
fn valid_ident(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// 路径拼装纯函数（endpoint 尾斜杠幂等剪除；单守卫覆盖装配与手写调用）。
/// id None → `/{index}/_search`；Some → `/{index}/_doc/{id}?refresh=true`。
fn url_for(endpoint: &str, index: &str, id: Option<&str>) -> String {
    let base = endpoint.trim_end_matches('/');
    match id {
        Some(id) => format!("{base}/{index}/_doc/{id}?refresh=true"),
        None => format!("{base}/{index}/_search"),
    }
}

fn backend(state: &OpState) -> Result<Arc<dyn EsBackend>, JsErrorBox> {
    state
        .borrow::<Arc<StableState>>()
        .es
        .clone()
        .ok_or_else(|| JsErrorBox::generic("es not configured (config es: section missing)"))
}

/// es.search(index, dsl)：POST `/{index}/_search`，返回 ES 响应体。
#[op2]
#[serde]
pub async fn op_es_search(
    state: Rc<RefCell<OpState>>,
    #[string] index: String,
    #[serde] dsl: serde_json::Value,
) -> Result<serde_json::Value, JsErrorBox> {
    let es = backend(&state.borrow())?;
    if !valid_ident(&index) {
        return Err(JsErrorBox::generic(format!("es search: invalid index {index:?}")));
    }
    es.search(&index, dsl).await.map_err(|e| JsErrorBox::generic(e.to_string()))
}

/// es.index(index, id, doc)：PUT `/{index}/_doc/{id}?refresh=true`（实时可查）。
#[op2]
#[serde]
pub async fn op_es_index(
    state: Rc<RefCell<OpState>>,
    #[string] index: String,
    #[string] id: String,
    #[serde] doc: serde_json::Value,
) -> Result<serde_json::Value, JsErrorBox> {
    let es = backend(&state.borrow())?;
    if !valid_ident(&index) {
        return Err(JsErrorBox::generic(format!("es index: invalid index {index:?}")));
    }
    if !valid_ident(&id) {
        return Err(JsErrorBox::generic(format!("es index: invalid id {id:?}")));
    }
    es.index_doc(&index, &id, doc).await.map_err(|e| JsErrorBox::generic(e.to_string()))
}

/// es.del(index, id)：DELETE `/{index}/_doc/{id}?refresh=true`（幂等：缺失返回 404 体）。
#[op2]
#[serde]
pub async fn op_es_del(
    state: Rc<RefCell<OpState>>,
    #[string] index: String,
    #[string] id: String,
) -> Result<serde_json::Value, JsErrorBox> {
    let es = backend(&state.borrow())?;
    if !valid_ident(&index) {
        return Err(JsErrorBox::generic(format!("es del: invalid index {index:?}")));
    }
    if !valid_ident(&id) {
        return Err(JsErrorBox::generic(format!("es del: invalid id {id:?}")));
    }
    es.delete_doc(&index, &id).await.map_err(|e| JsErrorBox::generic(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::{Bridge, Extras, InMemoryAccessor, InMemoryKV, RequestInfo, SchemaRegistry};
    use serde_json::Value;

    #[test]
    fn url_for_builds_search_and_doc_paths() {
        assert_eq!(
            url_for("http://localhost:9200", "user", None),
            "http://localhost:9200/user/_search"
        );
        // 尾斜杠幂等剪除
        assert_eq!(
            url_for("http://localhost:9200/", "user", None),
            "http://localhost:9200/user/_search"
        );
        assert_eq!(
            url_for("http://localhost:9200", "user", Some("d1")),
            "http://localhost:9200/user/_doc/d1?refresh=true"
        );
    }

    #[test]
    fn valid_ident_rejects_unsafe_chars() {
        assert!(valid_ident("user"));
        assert!(valid_ident("u_2-a"));
        assert!(!valid_ident(""));
        assert!(!valid_ident("../etc"));
        assert!(!valid_ident("a b"));
        assert!(!valid_ident("中文"));
        assert!(!valid_ident("a?b"));
    }

    /// 未配置 → "es not configured"；配置但非法 index/id → 校验拒绝（不发网络请求）。
    #[tokio::test(flavor = "current_thread")]
    async fn es_ops_error_when_unconfigured_or_invalid() {
        let b = Bridge::new(Arc::new(InMemoryAccessor::new()), Arc::new(InMemoryKV::new()));
        let cap = b
            .run_with(
                r#"(async () => { await es.search("idx", { q: 1 }); json.ok({}); })().catch((e) => json.ok({ err: String(e) }));"#,
                RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(v["data"]["err"].as_str().unwrap().contains("es not configured"), "{v}");

        // 配置（dead endpoint：port 1）——非法 index 在发请求前被拒。
        let es = Arc::new(EsClient::new("http://127.0.0.1:1".into()));
        let b2 = Bridge::with_dbs_and_loader(
            std::collections::HashMap::new(),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            None,
            Extras { es: Some(es), ..Default::default() },
        );
        let cap = b2
            .run_with(
                r#"(async () => { await es.search("bad/index", { q: 1 }); json.ok({}); })().catch((e) => json.ok({ err: String(e) }));"#,
                RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(v["data"]["err"].as_str().unwrap().contains("invalid index"), "{v}");

        // 非法 id 同被拒（index 路径）。
        let cap = b2
            .run_with(
                r#"(async () => { await es.index("idx", "../x", { a: 1 }); json.ok({}); })().catch((e) => json.ok({ err: String(e) }));"#,
                RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(v["data"]["err"].as_str().unwrap().contains("invalid id"), "{v}");
    }

    /// op 层经 EsBackend trait 分发：Stub 实现即生效（不触网）。
    #[tokio::test(flavor = "current_thread")]
    async fn ops_dispatch_via_es_backend_trait() {
        struct Stub;
        #[async_trait::async_trait]
        impl EsBackend for Stub {
            async fn search(&self, index: &str, _dsl: Value) -> crate::bridge::BridgeResult<Value> {
                Ok(serde_json::json!({"stub": index}))
            }
            async fn index_doc(&self, _: &str, _: &str, _: Value) -> crate::bridge::BridgeResult<Value> {
                unreachable!()
            }
            async fn delete_doc(&self, _: &str, _: &str) -> crate::bridge::BridgeResult<Value> {
                unreachable!()
            }
        }
        let b = Bridge::with_dbs_and_loader(
            std::collections::HashMap::new(),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            None,
            Extras { es: Some(Arc::new(Stub)), ..Default::default() },
        );
        let cap = b
            .run_with(
                r#"(async () => { const r = await es.search("idx", { q: 1 }); json.ok(r); })().catch((e) => json.ok({ err: String(e) }));"#,
                RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["data"]["stub"], serde_json::json!("idx"), "{v}");
    }

    /// 真 ES roundtrip：env `OJ_TEST_ES` 给 endpoint（如 http://127.0.0.1:9200）。
    /// 未设置 → 跳过。
    #[tokio::test(flavor = "current_thread")]
    #[ignore]
    async fn es_roundtrip() {
        let Ok(endpoint) = std::env::var("OJ_TEST_ES") else {
            eprintln!("skip: OJ_TEST_ES unset");
            return;
        };
        let es = Arc::new(EsClient::new(endpoint));
        let b = Bridge::with_dbs_and_loader(
            std::collections::HashMap::new(),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            None,
            Extras { es: Some(es), ..Default::default() },
        );
        let id = format!("t{}", std::process::id());
        let cap = b
            .run_with(
                &format!(
                    r#"(async () => {{
                        await es.index("oj-test", "{id}", {{ a: 42 }});
                        const r = await es.search("oj-test", {{ query: {{ match: {{ _id: "{id}" }} }} }});
                        await es.del("oj-test", "{id}");
                        json.ok({{ hits: r.hits && r.hits.total ? r.hits.total.value : 0 }});
                    }})().catch((e) => json.fail(500, String(e)));"#
                ),
                RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(v["data"]["hits"], 1, "{v}");
    }

    use httptest::Expectation;
    use httptest::Server;
    use httptest::matchers::*;
    use httptest::responders::*;

    /// 用 httptest 起真实本地 ES 桩，覆盖 search/index/del 的请求拼装与响应直通。
    fn es_bridge(endpoint: String) -> Bridge {
        let es = Arc::new(EsClient::new(endpoint));
        Bridge::with_dbs_and_loader(
            std::collections::HashMap::new(),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            None,
            Extras { es: Some(es), ..Default::default() },
        )
    }

    #[tokio::test(flavor = "current_thread")]
    async fn search_index_del_hit_mock_server() {
        let server = Server::run();
        server.expect(
            Expectation::matching(all_of![
                request::method("POST"),
                request::path("/user/_search")
            ])
            .respond_with(status_code(200).body(r#"{"hits":{"total":{"value":3}}}"#)),
        );
        server.expect(
            Expectation::matching(all_of![
                request::method("PUT"),
                request::path("/user/_doc/d1")
            ])
            .respond_with(status_code(200).body(r#"{"result":"created"}"#)),
        );
        server.expect(
            Expectation::matching(all_of![
                request::method("DELETE"),
                request::path("/user/_doc/d1")
            ])
            .respond_with(status_code(200).body(r#"{"result":"deleted"}"#)),
        );

        let b = es_bridge(server.url("/").to_string());
        let cap = b
            .run_with(
                r#"(async () => {
                    const s = await es.search("user", { query: {} });
                    const i = await es.index("user", "d1", { a: 1 });
                    const d = await es.del("user", "d1");
                    json.ok({ s, i, d });
                })().catch((e) => json.fail(500, String(e)));"#,
                RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(v["data"]["s"]["hits"]["total"]["value"], 3);
        assert_eq!(v["data"]["i"]["result"], "created");
        assert_eq!(v["data"]["d"]["result"], "deleted");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn non_2xx_and_bad_json_propagate_errors() {
        let server = Server::run();
        // 非 2xx → 报错带状态码与响应体
        server.expect(
            Expectation::matching(all_of![
                request::method("POST"),
                request::path("/user/_search")
            ])
            .respond_with(status_code(500).body("es exploded")),
        );
        let b = es_bridge(server.url("/").to_string());
        let cap = b
            .run_with(
                r#"(async () => { await es.search("user", {}); json.ok({}); })().catch((e) => json.fail(500, String(e)));"#,
                RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 500, "{v}");
        let msg = v["msg"].as_str().unwrap();
        assert!(msg.contains("HTTP 500") && msg.contains("es exploded"), "{v}");

        // 2xx 但响应体非 JSON → parse 错误
        let server2 = Server::run();
        server2.expect(
            Expectation::matching(all_of![
                request::method("POST"),
                request::path("/user/_search")
            ])
            .respond_with(status_code(200).body("not-json")),
        );
        let b2 = es_bridge(server2.url("/").to_string());
        let cap = b2
            .run_with(
                r#"(async () => { await es.search("user", {}); json.ok({}); })().catch((e) => json.fail(500, String(e)));"#,
                RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 500, "{v}");
        assert!(v["msg"].as_str().unwrap().contains("parse response"), "{v}");
    }
}
