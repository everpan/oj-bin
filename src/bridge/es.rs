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

fn client(state: &OpState) -> Result<Arc<EsClient>, JsErrorBox> {
    state
        .borrow::<Arc<StableState>>()
        .es
        .clone()
        .ok_or_else(|| JsErrorBox::generic("es not configured (config es: section missing)"))
}

/// 响应直通：2xx → JSON 体；非 2xx → Err（带 ES 返回体便于排障）。
async fn es_resp(resp: reqwest::Response, what: &str) -> Result<serde_json::Value, JsErrorBox> {
    let status = resp.status();
    if status.is_success() {
        resp.json()
            .await
            .map_err(|e| JsErrorBox::generic(format!("{what}: parse response: {e}")))
    } else {
        let body = resp.text().await.unwrap_or_default();
        Err(JsErrorBox::generic(format!("{what}: HTTP {status}: {body}")))
    }
}

/// es.search(index, dsl)：POST `/{index}/_search`，返回 ES 响应体。
#[op2]
#[serde]
pub async fn op_es_search(
    state: Rc<RefCell<OpState>>,
    #[string] index: String,
    #[serde] dsl: serde_json::Value,
) -> Result<serde_json::Value, JsErrorBox> {
    let es = client(&state.borrow())?;
    if !valid_ident(&index) {
        return Err(JsErrorBox::generic(format!("es search: invalid index {index:?}")));
    }
    let url = url_for(&es.endpoint, &index, None);
    let resp = es
        .http
        .post(&url)
        .json(&dsl)
        .send()
        .await
        .map_err(|e| JsErrorBox::generic(format!("es search: {e}")))?;
    es_resp(resp, "es search").await
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
    let es = client(&state.borrow())?;
    if !valid_ident(&index) {
        return Err(JsErrorBox::generic(format!("es index: invalid index {index:?}")));
    }
    if !valid_ident(&id) {
        return Err(JsErrorBox::generic(format!("es index: invalid id {id:?}")));
    }
    let url = url_for(&es.endpoint, &index, Some(&id));
    let resp = es
        .http
        .put(&url)
        .json(&doc)
        .send()
        .await
        .map_err(|e| JsErrorBox::generic(format!("es index: {e}")))?;
    es_resp(resp, "es index").await
}

/// es.del(index, id)：DELETE `/{index}/_doc/{id}?refresh=true`（幂等：缺失返回 404 体）。
#[op2]
#[serde]
pub async fn op_es_del(
    state: Rc<RefCell<OpState>>,
    #[string] index: String,
    #[string] id: String,
) -> Result<serde_json::Value, JsErrorBox> {
    let es = client(&state.borrow())?;
    if !valid_ident(&index) {
        return Err(JsErrorBox::generic(format!("es del: invalid index {index:?}")));
    }
    if !valid_ident(&id) {
        return Err(JsErrorBox::generic(format!("es del: invalid id {id:?}")));
    }
    let url = url_for(&es.endpoint, &index, Some(&id));
    let resp = es
        .http
        .delete(&url)
        .send()
        .await
        .map_err(|e| JsErrorBox::generic(format!("es del: {e}")))?;
    es_resp(resp, "es del").await
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
}
