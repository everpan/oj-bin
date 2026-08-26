//! plugins 自省 op（spec §4 升级核对、§2 注册表自省并入）。
//! JS 侧 `plugins()` → 已加载插件 [{name, semver, abi_version, fingerprint, host_abi_version}]。

use crate::bridge::StableState;
use crate::bridge::plugin_loader::PluginInfo;
use deno_core::{OpState, op2};
use std::sync::Arc;

/// 自省：已加载插件清单 + 宿主当前 ABI_VERSION（spec §4 升级核对）。
#[op2]
#[serde]
pub fn op_plugins(state: &mut OpState) -> Vec<PluginInfo> {
    state.borrow::<Arc<StableState>>().plugins.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::{
        Bridge, Extras, InMemoryAccessor, InMemoryKV, RequestInfo, SchemaRegistry,
    };
    use serde_json::Value;

    fn info(name: &str) -> PluginInfo {
        PluginInfo {
            name: name.into(),
            semver: "0.1.0".into(),
            abi_version: 1,
            fingerprint: "test-fingerprint".into(),
            host_abi_version: oj_plugin_ffi::ABI_VERSION,
        }
    }

    /// 装配后 JS `plugins()` 字段齐全（name/semver/abi/fingerprint + host ABI）。
    #[tokio::test(flavor = "current_thread")]
    async fn plugins_reports_loaded_fields_and_host_abi() {
        let b = Bridge::with_dbs_and_loader(
            std::collections::HashMap::new(),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            None,
            Extras {
                plugins: vec![info("es"), info("kv")],
                ..Default::default()
            },
        );
        let cap = b
            .run_with(
                r#"(async () => { json.ok(plugins()); })().catch((e) => json.ok({ err: String(e) }));"#,
                RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        let arr = v["data"].as_array().unwrap();
        assert_eq!(arr.len(), 2, "{v}");
        assert_eq!(arr[0]["name"], "es");
        assert_eq!(arr[0]["semver"], "0.1.0");
        assert_eq!(arr[0]["abi_version"], 1);
        assert_eq!(arr[0]["fingerprint"], "test-fingerprint");
        assert_eq!(
            arr[0]["host_abi_version"],
            oj_plugin_ffi::ABI_VERSION,
            "{v}"
        );
        assert_eq!(arr[1]["name"], "kv");
    }

    /// 零插件 → 空数组（host ABI 由 op 类型与装配层携行，见上例）。
    #[tokio::test(flavor = "current_thread")]
    async fn plugins_empty_when_none_loaded() {
        let b = Bridge::new(
            Arc::new(InMemoryAccessor::new()),
            Arc::new(InMemoryKV::new()),
        );
        let cap = b
            .run_with(
                r#"(async () => { json.ok(plugins()); })().catch((e) => json.ok({ err: String(e) }));"#,
                RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["data"], serde_json::json!([]), "{v}");
    }
}
