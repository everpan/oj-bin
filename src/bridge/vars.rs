//! vars 全局对象（v0.1.25）：部署期常量读口（config `vars:` 段 → JS `vars.get(name)`）。
//!
//! 存在理由：`WEB_URL` 这类「部署期决定、换域名即变」的常量过去只能被编译进产物
//! （业务侧硬编码发版常量），换环境就得重新构建。本读口把它们还给 config。
//!
//! **fail-closed**：只有 config `vars:` 段里显式声明的键可读，其余一律 `null`；平台不提供
//! 「读任意 OS env / 读任意 config 键」的通道（`db:` 的 DSN、`server.public_key_path` 等
//! 敏感面因此不可能经此泄漏到 JS）。值是装配期冻结的字符串（标量按其 YAML 字面量成串），
//! 读操作无 IO —— op 是**同步**的（同 `plugins()`），故 handler 里可直写
//! `const base = vars.get("WEB_URL") ?? "http://localhost:3000"`。

use std::sync::Arc;

use deno_core::{OpState, op2};

use super::StableState;

/// vars.get(name)：已声明键 → 该字符串；未声明 → `null`。
///
/// **未声明不抛错**（返回 `null`）是有意的：调用点写 `?? 兜底` 就能表达「可选部署项」，
/// 抛错则会把「没配这个 var」变成 500。要区分「没配」与「配了空串」，注意两者都返回
/// 声明值（空串原样返回，不是 `null`）。
///
/// 返回类型必须写成**全限定** `serde_json::Value`：`op2` 的 `#[serde]` 只认这个字面类型
/// 路径（用 `use serde_json::Value` 的短名会被判「Invalid return type」，同 http.rs）。
#[op2]
#[serde]
pub fn op_vars_get(state: &mut OpState, #[string] name: String) -> serde_json::Value {
    match state.borrow::<Arc<StableState>>().vars.get(name.as_str()) {
        Some(v) => serde_json::Value::String(v.clone()),
        None => serde_json::Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::{Bridge, Extras, InMemoryKV, RequestInfo, SchemaRegistry};
    use serde_json::Value;
    use std::collections::HashMap;

    fn bridge_with(vars: HashMap<String, String>) -> Bridge {
        Bridge::with_dbs_and_loader(
            HashMap::new(),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            None,
            Extras {
                vars: Arc::new(vars),
                ..Default::default()
            },
        )
    }

    /// 声明键可读 / 未声明键为 null（fail-closed）——JS 侧直测，钉住跨语言形状。
    #[tokio::test(flavor = "current_thread")]
    async fn vars_get_reads_declared_and_nulls_undeclared() {
        let mut vars = HashMap::new();
        vars.insert("WEB_URL".to_string(), "https://plane.example".to_string());
        vars.insert("EMPTY".to_string(), String::new());
        let b = bridge_with(vars);

        let cap = b
            .run_with(
                r#"json.ok({
                    hit: vars.get("WEB_URL"),
                    miss: vars.get("DB_PASSWORD"),
                    empty: vars.get("EMPTY"),
                    what: typeof vars.get("WEB_URL"),
                });"#,
                RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0);
        assert_eq!(v["data"]["hit"], "https://plane.example");
        // 未声明 → null（不是 undefined、不抛错）
        assert!(v["data"]["miss"].is_null(), "undeclared key must be null");
        // 声明的空串原样返回（与「未声明」可区分）
        assert_eq!(v["data"]["empty"], "");
        // 同步值（不是 Promise）——handler 里可直接 `?? 兜底`
        assert_eq!(v["data"]["what"], "string");
    }

    /// 空 vars 段（未配置）：一切键恒 null，且不抛错。
    #[tokio::test(flavor = "current_thread")]
    async fn vars_get_on_empty_section_is_null() {
        let b = bridge_with(HashMap::new());
        let cap = b
            .run_with(
                r#"json.ok({ v: vars.get("ANY") });"#,
                RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(v["data"]["v"].is_null());
    }
}
