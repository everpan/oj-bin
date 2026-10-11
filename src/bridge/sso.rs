//! HTTP 表单 / CAS SSO 提供者注册表（v0.1.61，spec 2026-10-11 §一）：装配期注入
//! config `http_sso:` 段，JS 登录端点（sample/src/sso/，普通业务路由）经
//! `sso.providers` 读取。段不存在 = not configured（同 oidc/jwt/es 语义）。
//! 暴露面声明：providers 对 handler 全量可读，与 oidc.info（含 client_secret）同级；
//! ENC[...] 密封值已在 config 装载期整树解封（secret.rs decrypt_tree）。

use std::cell::RefCell;
use std::rc::Rc;

use deno_core::OpState;
use deno_core::op2;
use deno_error::JsErrorBox;

/// 取 SSO 配置态（未配置 → 报错，同 oidc 的 not configured 语义）。
fn sso_of(state: &deno_core::OpState) -> Result<std::sync::Arc<serde_json::Value>, JsErrorBox> {
    state
        .borrow::<std::sync::Arc<crate::bridge::StableState>>()
        .sso
        .clone()
        .ok_or_else(|| {
            JsErrorBox::generic("http sso not configured (config http_sso: section missing)")
        })
}

/// `sso.providers`：`http_sso:` 段原样（map：提供者名 → 配置）。
#[op2]
#[serde]
pub fn op_sso_providers(state: Rc<RefCell<OpState>>) -> Result<serde_json::Value, JsErrorBox> {
    Ok((*sso_of(&state.borrow())?).clone())
}

#[cfg(test)]
mod tests {
    /// 段缺省 → `http sso not configured`（与 oidc not configured 同语义）。
    #[tokio::test(flavor = "current_thread")]
    async fn sso_providers_errors_when_unconfigured() {
        let b = crate::bridge::Bridge::new(
            std::sync::Arc::new(crate::bridge::InMemoryAccessor::new()),
            std::sync::Arc::new(crate::bridge::InMemoryKV::new()),
        );
        let cap = b
            .run_with(
                r#"(async () => { try { sso.providers; json.ok("no"); } catch (e) { json.ok(String(e)); } })()"#,
                crate::bridge::RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(
            v["data"]
                .as_str()
                .unwrap()
                .contains("http sso not configured"),
            "{v}"
        );
    }
}
