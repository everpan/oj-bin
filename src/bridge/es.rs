//! es 全局对象：Elasticsearch 薄封装（search / index / del 三 op，Extras 注入）。
//!
//! HTTP 实现已迁入 oj-es cdylib 插件（plan Task 3.4/3.7）：本模块只留后端契约
//! `EsBackend` trait + op 层（校验 + 分发）。装配层按「cfg es: 声明 + es 插件已装」
//! 注入 `FfiEsBackend`（ffi.rs）或测试 Stub。
//!
//! index/id 白名单 `[a-zA-Z0-9_-]+`（防路径注入，op 层校验）；未配置报 "es not configured"。

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use deno_core::{OpState, op2};
use deno_error::JsErrorBox;

use super::StableState;

/// es 轴后端契约（spec §2：HTTP 实现迁入插件后，插件经 FFI 适配器 `FfiEsBackend`
/// 实现本 trait；core 不再有内置 HTTP 后端）。
#[async_trait::async_trait]
pub trait EsBackend: Send + Sync {
    async fn search(
        &self,
        index: &str,
        dsl: serde_json::Value,
    ) -> super::BridgeResult<serde_json::Value>;
    async fn index_doc(
        &self,
        index: &str,
        id: &str,
        doc: serde_json::Value,
    ) -> super::BridgeResult<serde_json::Value>;
    async fn delete_doc(&self, index: &str, id: &str) -> super::BridgeResult<serde_json::Value>;
}

/// index/id 白名单：字母数字下划线连字符（防路径注入）。
fn valid_ident(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
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
        return Err(JsErrorBox::generic(format!(
            "es search: invalid index {index:?}"
        )));
    }
    es.search(&index, dsl)
        .await
        .map_err(|e| JsErrorBox::generic(e.to_string()))
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
        return Err(JsErrorBox::generic(format!(
            "es index: invalid index {index:?}"
        )));
    }
    if !valid_ident(&id) {
        return Err(JsErrorBox::generic(format!("es index: invalid id {id:?}")));
    }
    es.index_doc(&index, &id, doc)
        .await
        .map_err(|e| JsErrorBox::generic(e.to_string()))
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
        return Err(JsErrorBox::generic(format!(
            "es del: invalid index {index:?}"
        )));
    }
    if !valid_ident(&id) {
        return Err(JsErrorBox::generic(format!("es del: invalid id {id:?}")));
    }
    es.delete_doc(&index, &id)
        .await
        .map_err(|e| JsErrorBox::generic(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::{
        Bridge, Extras, InMemoryAccessor, InMemoryKV, RequestInfo, SchemaRegistry,
    };
    use serde_json::Value;

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

    /// 永不触发的后端（校验必须在前）：被调到即报错——若校验被绕过则断言失败。
    struct MustNotCall;
    #[async_trait::async_trait]
    impl EsBackend for MustNotCall {
        async fn search(&self, _: &str, _: Value) -> crate::bridge::BridgeResult<Value> {
            Err("must not call search".into())
        }
        async fn index_doc(
            &self,
            _: &str,
            _: &str,
            _: Value,
        ) -> crate::bridge::BridgeResult<Value> {
            Err("must not call index_doc".into())
        }
        async fn delete_doc(&self, _: &str, _: &str) -> crate::bridge::BridgeResult<Value> {
            Err("must not call delete_doc".into())
        }
    }

    /// 未配置 → "es not configured"；配置但非法 index/id → 校验拒绝（不发后端请求）。
    #[tokio::test(flavor = "current_thread")]
    async fn es_ops_error_when_unconfigured_or_invalid() {
        let b = Bridge::new(
            Arc::new(InMemoryAccessor::new()),
            Arc::new(InMemoryKV::new()),
        );
        let cap = b
            .run_with(
                r#"(async () => { await es.search("idx", { q: 1 }); json.ok({}); })().catch((e) => json.ok({ err: String(e) }));"#,
                RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(
            v["data"]["err"]
                .as_str()
                .unwrap()
                .contains("es not configured"),
            "{v}"
        );

        // 配置（MustNotCall 后端）——非法 index/id 在触达后端前被拒。
        let b2 = Bridge::with_dbs_and_loader(
            std::collections::HashMap::new(),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            None,
            Extras {
                es: Some(Arc::new(MustNotCall)),
                ..Default::default()
            },
        );
        let cap = b2
            .run_with(
                r#"(async () => { await es.search("bad/index", { q: 1 }); json.ok({}); })().catch((e) => json.ok({ err: String(e) }));"#,
                RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(
            v["data"]["err"].as_str().unwrap().contains("invalid index"),
            "{v}"
        );

        // 非法 id 同被拒（index 路径）。
        let cap = b2
            .run_with(
                r#"(async () => { await es.index("idx", "../x", { a: 1 }); json.ok({}); })().catch((e) => json.ok({ err: String(e) }));"#,
                RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(
            v["data"]["err"].as_str().unwrap().contains("invalid id"),
            "{v}"
        );
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
            async fn index_doc(
                &self,
                _: &str,
                _: &str,
                _: Value,
            ) -> crate::bridge::BridgeResult<Value> {
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
            Extras {
                es: Some(Arc::new(Stub)),
                ..Default::default()
            },
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
}
