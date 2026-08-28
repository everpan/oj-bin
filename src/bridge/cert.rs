//! cert 全局对象：JWS 证书生成与重签（tools/oj-cert 的桥接；纯内存，不落盘）。
//!
//! 格式契约（header `{"alg":"RS256","typ":"JWT"}` + payload `{nbf,exp}` + b64url
//! no-pad）的单一事实来源在 tools/oj-cert；CLI 与本模块同源。验签与状态判定在
//! server/src/certificate.rs。

use deno_error::JsErrorBox;
use rsa::pkcs8::DecodePrivateKey;
use serde_json::json;

fn to_unix(v: i64) -> Result<u64, JsErrorBox> {
    u64::try_from(v).map_err(|_| JsErrorBox::generic("timestamp must be >= 0"))
}

/// cert.generate(bits, nbf, exp)：生成 RSA 密钥对 + JWS，返回三件套（内存字符串）。
/// nbf/exp 为 Unix 秒；`#[number]` 使 JS 侧普通 number 直入 i64（同 kv.expire）。
#[deno_core::op2]
#[serde]
pub fn op_cert_gen(
    bits: u32,
    #[number] nbf: i64,
    #[number] exp: i64,
) -> Result<serde_json::Value, JsErrorBox> {
    let nbf = to_unix(nbf)?;
    let exp = to_unix(exp)?;
    if exp <= nbf {
        return Err(JsErrorBox::generic("exp must be greater than nbf"));
    }
    let key = oj_cert::keygen(bits).map_err(JsErrorBox::generic)?;
    Ok(json!({
        "private_pem": oj_cert::private_pem(&key).map_err(JsErrorBox::generic)?,
        "public_pem": oj_cert::public_pem(&key).map_err(JsErrorBox::generic)?,
        "cert_jws": oj_cert::sign_jws(&key, nbf, exp),
    }))
}

/// cert.renew(private_pem, nbf, exp)：读 PKCS#8 私钥重签，返回新 cert.jws（公钥不变）。
#[deno_core::op2]
#[string]
pub fn op_cert_renew(
    #[string] private_pem: String,
    #[number] nbf: i64,
    #[number] exp: i64,
) -> Result<String, JsErrorBox> {
    let nbf = to_unix(nbf)?;
    let exp = to_unix(exp)?;
    if exp <= nbf {
        return Err(JsErrorBox::generic("exp must be greater than nbf"));
    }
    let key = rsa::RsaPrivateKey::from_pkcs8_pem(&private_pem)
        .map_err(|e| JsErrorBox::generic(format!("parse private key (PKCS#8 PEM): {e}")))?;
    Ok(oj_cert::sign_jws(&key, nbf, exp))
}

#[cfg(test)]
mod tests {
    use crate::bridge::{Bridge, InMemoryAccessor, InMemoryKV, RequestInfo};
    use serde_json::Value;
    use std::sync::Arc;

    async fn run(js: &str) -> Value {
        let b = Bridge::new(
            Arc::new(InMemoryAccessor::new()),
            Arc::new(InMemoryKV::new()),
        );
        let cap = b.run_with(js, RequestInfo::default()).await.unwrap();
        serde_json::from_slice(&cap.body).unwrap()
    }

    const GEN: &str = r#"(async () => { json.ok(await cert.generate(2048, 1000, 2000)); })()
        .catch((e) => json.fail(500, String(e)));"#;

    #[tokio::test(flavor = "current_thread")]
    async fn generate_returns_three_part_jws_and_pems() {
        let v = run(GEN).await;
        assert_eq!(v["code"], 0, "{v}");
        let m = &v["data"];
        assert_eq!(m["cert_jws"].as_str().unwrap().split('.').count(), 3, "{v}");
        assert!(
            m["public_pem"]
                .as_str()
                .unwrap()
                .starts_with("-----BEGIN PUBLIC KEY"),
            "{v}"
        );
        assert!(
            m["private_pem"].as_str().unwrap().contains("PRIVATE KEY"),
            "{v}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn renew_reuses_key_and_changes_jws() {
        let v = run(r#"(async () => {
                const m = await cert.generate(2048, 1000, 2000);
                const jws2 = await cert.renew(m.private_pem, 3000, 4000);
                json.ok({ jws1: m.cert_jws, jws2, pub: m.public_pem });
            })().catch((e) => json.fail(500, String(e)));"#)
        .await;
        assert_eq!(v["code"], 0, "{v}");
        let d = &v["data"];
        assert_ne!(d["jws1"], d["jws2"], "{v}");
        assert_eq!(d["jws2"].as_str().unwrap().split('.').count(), 3, "{v}");
        assert!(
            d["pub"]
                .as_str()
                .unwrap()
                .starts_with("-----BEGIN PUBLIC KEY"),
            "{v}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rejects_bad_bits_and_bad_window() {
        let v = run(
            r#"(async () => { await cert.generate(1024, 1000, 2000); json.ok({}); })()
                .catch((e) => json.ok({ err: String(e) }));"#,
        )
        .await;
        assert!(
            v["data"]["err"]
                .as_str()
                .unwrap()
                .contains("bits must be >="),
            "{v}"
        );
        let v = run(
            r#"(async () => { await cert.generate(2048, 2000, 2000); json.ok({}); })()
                .catch((e) => json.ok({ err: String(e) }));"#,
        )
        .await;
        assert!(
            v["data"]["err"]
                .as_str()
                .unwrap()
                .contains("exp must be greater than nbf"),
            "{v}"
        );
    }
}
