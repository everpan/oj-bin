//! U-10（v0.1.101）：运行期凭据轴——`secret.seal` / `secret.open`。
//!
//! 复用 config `ENC[...]` 的同一套 X25519 + AES-256-GCM 信封（[`crate::secret`]）：公钥
//! 装配期加载（`secrets.public_key_path`，缺省由私钥推导——部署机为解封 config 本就持有
//! 私钥，零新增配置）；私钥**永不出 Rust 边界**，JS 只见 seal 的密文与 open 的明文。
//! 密钥皆缺 → 两个 op 都 fail-closed 报 not configured（不静默降级为明文）。
//! `open` 每次落 `SECRET_OPEN` 审计（零凭据：不记 token / 明文）。

use deno_core::{OpState, op2};
use deno_error::JsErrorBox;

use super::StableState;

const NOT_CONFIGURED: &str = "secret axis not configured: this assembly has no secrets keypair \
 (set secrets.private_key_path / OJ_SECRET_KEY, or secrets.public_key_path for seal-only)";
const NO_PRIV_KEY: &str = "secret.open unavailable: this assembly has no private key \
 (seal-only mode via secrets.public_key_path) — ask the deployment to provide the private key";

/// 取运行期密钥（装配期冻结；None = 该装配没有任何密钥）。
fn keys_of(state: &OpState) -> Result<std::sync::Arc<crate::secret::RuntimeKeys>, JsErrorBox> {
    state
        .borrow::<std::sync::Arc<StableState>>()
        .secret_keys
        .clone()
        .ok_or_else(|| JsErrorBox::generic(NOT_CONFIGURED.to_string()))
}

/// `secret.seal(plaintext)` → `ENC[...]`（部署公钥加密；密文可存自有表/回传前端）。
#[op2]
#[string]
pub fn op_secret_seal(
    state: &mut OpState,
    #[string] plaintext: &str,
) -> Result<String, JsErrorBox> {
    let keys = keys_of(state)?;
    crate::secret::seal_pub(&keys.pub_key, plaintext).map_err(JsErrorBox::generic)
}

/// `secret.open(token)` → 明文（需部署私钥；每次落 SECRET_OPEN 审计）。
#[op2]
#[string]
pub fn op_secret_open(state: &mut OpState, #[string] token: &str) -> Result<String, JsErrorBox> {
    let keys = keys_of(state)?;
    let priv_key = keys
        .priv_key
        .as_ref()
        .ok_or_else(|| JsErrorBox::generic(NO_PRIV_KEY.to_string()))?;
    let plain = crate::secret::open_pem(priv_key, token).map_err(JsErrorBox::generic)?;
    // 统一审计出口（U-9）：动作敏感（把部署凭据解进 JS），内容零凭据。
    let st = state.borrow::<std::sync::Arc<StableState>>();
    super::audit::emit(
        &st.audit,
        "SECRET_OPEN",
        &super::db_session::owner_of(state),
        "secret",
        "runtime open",
    );
    Ok(plain)
}

#[cfg(test)]
mod tests {
    //! 与 db_session::tests 同款骨架：语义在**请求边界**上验证（一次 `run` = 一个请求，
    //! 错误经 JS catch 进 json 信封断言）。

    use crate::bridge::{Bridge, Extras, InMemoryKV, SchemaRegistry};
    use crate::secret::{PubKey, keygen};
    use serde_json::Value;

    async fn bridge_with_keys(keys: Option<crate::secret::RuntimeKeys>) -> Bridge {
        Bridge::with_dbs_and_loader(
            std::collections::HashMap::from([(
                "default".to_string(),
                std::sync::Arc::new(crate::bridge::db::InMemoryAccessor::new()) as _,
            )]),
            std::sync::Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            None,
            Extras {
                secret_keys: keys.map(std::sync::Arc::new),
                ..Default::default()
            },
        )
    }

    async fn run(b: &Bridge, src: &str) -> Value {
        let cap = b.run(src).await.unwrap();
        serde_json::from_slice(&cap.body).unwrap()
    }

    fn full_keys(priv_pem: &str) -> crate::secret::RuntimeKeys {
        let priv_key = crate::secret::parse_private_key(priv_pem).unwrap();
        crate::secret::RuntimeKeys {
            pub_key: crate::secret::public_of(&priv_key),
            priv_key: Some(priv_key),
        }
    }

    /// 全链路：seal → open 往返（JS 面），密文是合法 `ENC[...]` 且不含明文。
    #[tokio::test(flavor = "current_thread")]
    async fn seal_open_roundtrip_via_js() {
        let (priv_pem, _) = keygen().unwrap();
        let b = bridge_with_keys(Some(full_keys(&priv_pem))).await;
        let v = run(
            &b,
            r#"const tok = secret.seal("mysql://root:hunter2@h/app");
               if (!tok.startsWith("ENC[")) throw new Error("not sealed: " + tok);
               if (tok.includes("hunter2")) throw new Error("plaintext leaked");
               json.ok({ plain: secret.open(tok) })"#,
        )
        .await;
        assert_eq!(v["data"]["plain"], "mysql://root:hunter2@h/app");
    }

    /// 无密钥装配：fail-closed 且报错点名怎么配（不静默返回明文/空串）。
    #[tokio::test(flavor = "current_thread")]
    async fn fail_closed_without_keys() {
        let b = bridge_with_keys(None).await;
        for src in ["secret.seal('x')", "secret.open('ENC[x]')"] {
            let v = run(
                &b,
                &format!("try {{ {src}; json.fail(500, 'no-throw') }} catch (e) {{ json.ok({{ msg: String(e) }}) }}"),
            )
            .await;
            let msg = v["data"]["msg"].as_str().unwrap();
            assert!(msg.contains("secret axis not configured"), "{msg}");
        }
    }

    /// seal-only（公钥有、私钥无）：seal 可用，open fail-closed。
    #[tokio::test(flavor = "current_thread")]
    async fn seal_only_rejects_open() {
        let keys = crate::secret::RuntimeKeys {
            pub_key: PubKey([7u8; 32]),
            priv_key: None,
        };
        let b = bridge_with_keys(Some(keys)).await;
        let v = run(&b, r#"json.ok({ tok: secret.seal("s") })"#).await;
        let tok = v["data"]["tok"].as_str().unwrap().to_string();
        let v = run(
            &b,
            &format!(
                "try {{ secret.open({tok:?}); json.fail(500, 'opened') }} \
                 catch (e) {{ json.ok({{ msg: String(e) }}) }}"
            ),
        )
        .await;
        let msg = v["data"]["msg"].as_str().unwrap();
        assert!(msg.contains("no private key"), "{msg}");
    }

    /// open 落 `SECRET_OPEN` 审计（U-9 统一出口可查），seal 不落。
    #[tokio::test(flavor = "current_thread")]
    async fn open_lands_audit_event() {
        let (priv_pem, _) = keygen().unwrap();
        let b = bridge_with_keys(Some(full_keys(&priv_pem))).await;
        run(
            &b,
            r#"const tok = secret.seal("s"); secret.open(tok); json.ok(1)"#,
        )
        .await;
        let v = run(
            &b,
            r#"audit.query({ kind: "SECRET_OPEN" }).then((r) => json.ok(r))"#,
        )
        .await;
        let items = v["data"].as_array().unwrap();
        assert_eq!(items.len(), 1, "seal 不得落审计，open 落一条：{v}");
    }
}
