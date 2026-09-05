//! oidc 全局对象：RS256 JWS 签发/验签原语 + 装配期配置透出（spec 2026-09-05 S3.2）。
//! Keys stay in Rust (DIP): JS only sees sign/verify/jwks interfaces.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rsa::pkcs8::DecodePrivateKey;
use rsa::traits::PublicKeyParts;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::Path;

/// RP 侧：tenant → 外部 IdP 客户端注册（JS 经 oidc.rp 只读透出）。
pub struct RpClient {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    pub scope: String,
}

/// OP 侧：client 白名单（redirect_uri 精确串 + 租户绑定）。
pub struct OidcClient {
    pub secret: String,
    pub redirect_uris: Vec<String>,
    pub tenant: String,
}

/// RS256 密钥与配置态。构造期注入 StableState 后不可变。
/// 手写 Debug（同 JwtCfg 不派生的理由：字段含 client_secret 与私钥 PEM，不进日志）。
impl std::fmt::Debug for OidcState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OidcState")
            .field("issuer", &self.issuer)
            .finish_non_exhaustive()
    }
}

pub struct OidcState {
    pub issuer: String,
    pub rp: HashMap<String, RpClient>,
    pub clients: HashMap<String, OidcClient>,
    signing_pem: String,
    kid: String,
    n: Vec<u8>,
    e: Vec<u8>,
}

fn b64u(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

impl OidcState {
    pub fn from_section(
        cfg: &crate::config::OidcSection,
        config_dir: &Path,
    ) -> Result<Self, String> {
        if cfg.issuer.trim().is_empty() {
            return Err("oidc.issuer must not be empty".into());
        }
        if cfg.private_key_path.trim().is_empty() {
            return Err("oidc.private_key_path must not be empty".into());
        }
        let kp = Path::new(&cfg.private_key_path);
        let kp = if kp.is_absolute() {
            kp.to_path_buf()
        } else {
            config_dir.join(kp)
        };
        let pem = std::fs::read_to_string(&kp)
            .map_err(|err| format!("oidc.private_key_path ({}): {err}", kp.display()))?;
        // PKCS#8 解析校验（与 cert.renew 同一入口）；签名直接用 PEM（jsonwebtoken
        // from_rsa_pem 同时接受 PKCS#1/PKCS#8）。
        rsa::RsaPrivateKey::from_pkcs8_pem(&pem).map_err(|err| {
            format!(
                "oidc private key ({}): parse pkcs8 pem: {err}",
                kp.display()
            )
        })?;
        let key = rsa::RsaPrivateKey::from_pkcs8_pem(&pem).unwrap();
        let pubk = rsa::RsaPublicKey::from(&key);
        let n = pubk.n().to_bytes_be();
        let e = pubk.e().to_bytes_be();
        let n_b64u = b64u(&n);
        // kid = sha256(base64url(n)) 前 16 hex——稳定可复现，够 demo 分辨密钥。
        let kid = {
            let d = Sha256::digest(n_b64u.as_bytes());
            d.iter().map(|b| format!("{b:02x}")).collect::<String>()[..16].to_string()
        };
        Ok(Self {
            issuer: cfg.issuer.clone(),
            rp: cfg
                .rp
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        RpClient {
                            issuer: v.issuer.clone(),
                            client_id: v.client_id.clone(),
                            client_secret: v.client_secret.clone(),
                            scope: v.scope.clone(),
                        },
                    )
                })
                .collect(),
            clients: cfg
                .clients
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        OidcClient {
                            secret: v.secret.clone(),
                            redirect_uris: v.redirect_uris.clone(),
                            tenant: v.tenant.clone(),
                        },
                    )
                })
                .collect(),
            signing_pem: pem,
            kid,
            n,
            e,
        })
    }

    /// RFC 7517 JWKS 文档（discovery/jwks 端点直出）。
    pub fn jwks(&self) -> serde_json::Value {
        serde_json::json!({
            "keys": [{
                "kty": "RSA", "use": "sig", "alg": "RS256",
                "kid": self.kid, "n": b64u(&self.n), "e": b64u(&self.e),
            }]
        })
    }

    pub fn signing_pem(&self) -> &str {
        &self.signing_pem
    }

    /// (modulus, exponent) 大端字节——本地验签用 from_rsa_components。
    pub fn n_e(&self) -> (Vec<u8>, Vec<u8>) {
        (self.n.clone(), self.e.clone())
    }

    pub fn kid(&self) -> &str {
        &self.kid
    }
}

// ----- ops：oidc 全局原语（密钥不出 Rust：JS 只见 sign/verify/jwks/issuer/rp/clients） -----

use std::cell::RefCell;
use std::rc::Rc;

use deno_core::OpState;
use deno_error::JsErrorBox;

/// 取 OIDC 态（未配置 → 报错，同 jwt/es 的 not configured 语义）。
fn oidc_of(state: &OpState) -> Result<std::sync::Arc<OidcState>, JsErrorBox> {
    state
        .borrow::<std::sync::Arc<crate::bridge::StableState>>()
        .oidc
        .clone()
        .ok_or_else(|| JsErrorBox::generic("oidc not configured (config oidc: section missing)"))
}

fn verify_with(token: &str, key: &jsonwebtoken::DecodingKey) -> Result<serde_json::Value, String> {
    let mut v = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256);
    v.leeway = 0;
    v.validate_exp = true;
    v.validate_aud = false; // aud 由调用方（RP handler）按 client_id 自查
    jsonwebtoken::decode::<serde_json::Value>(token, key, &v)
        .map(|d| d.claims)
        .map_err(|err| format!("oidc.verify: {err}"))
}

/// oidc.sign(claims)：RS256 紧凑 JWS，claims 原样签（OP 自控 iss/aud/exp/nonce）。
#[deno_core::op2]
#[string]
pub fn op_oidc_sign(
    state: Rc<RefCell<OpState>>,
    #[serde] claims: serde_json::Value,
) -> Result<String, JsErrorBox> {
    let st = oidc_of(&state.borrow())?;
    if !claims.is_object() {
        return Err(JsErrorBox::generic("oidc.sign: claims must be an object"));
    }
    jsonwebtoken::encode(
        // header 带 kid：RP 侧凭 jwks 按 kid 选钥（OIDC 惯例）。
        &jsonwebtoken::Header {
            kid: Some(st.kid().to_string()),
            ..jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256)
        },
        &claims,
        &jsonwebtoken::EncodingKey::from_rsa_pem(st.signing_pem().as_bytes())
            .map_err(|e| JsErrorBox::generic(format!("oidc.sign: {e}")))?,
    )
    .map_err(|e| JsErrorBox::generic(format!("oidc.sign: {e}")))
}

/// oidc.verify(token, jwks?)：无 jwks 用本机公钥；有 jwks 按 kid 匹配（from_jwk）。
/// 算法锁定 RS256（Validation::new 即白名单），leeway 0，验 exp。
#[deno_core::op2]
#[serde]
pub fn op_oidc_verify(
    state: Rc<RefCell<OpState>>,
    #[string] token: String,
    #[serde] jwks: Option<serde_json::Value>,
) -> Result<serde_json::Value, JsErrorBox> {
    let st = oidc_of(&state.borrow())?;
    match jwks {
        None => {
            let (n, e) = st.n_e();
            // jsonwebtoken 9：components 为 base64url 无填充串（与 jwks 暴露形态一致）。
            let key = jsonwebtoken::DecodingKey::from_rsa_components(&b64u(&n), &b64u(&e))
                .map_err(|e| JsErrorBox::generic(format!("oidc.verify: {e}")))?;
            verify_with(&token, &key).map_err(JsErrorBox::generic)
        }
        Some(doc) => {
            let header = jsonwebtoken::decode_header(&token)
                .map_err(|e| JsErrorBox::generic(format!("oidc.verify: {e}")))?;
            let kid = header
                .kid
                .ok_or_else(|| JsErrorBox::generic("oidc.verify: token header has no kid"))?;
            let entry = doc["keys"]
                .as_array()
                .and_then(|keys| {
                    keys.iter().find(|k| {
                        k["kid"] == kid.as_str()
                            && k["kty"] == "RSA"
                            && (k["alg"] == "RS256" || k["alg"].is_null())
                    })
                })
                .ok_or_else(|| {
                    JsErrorBox::generic(format!("oidc.verify: no RS256 key for kid {kid}"))
                })?;
            let jwk: jsonwebtoken::jwk::Jwk = serde_json::from_value(entry.clone())
                .map_err(|e| JsErrorBox::generic(format!("oidc.verify: jwk parse: {e}")))?;
            let key = jsonwebtoken::DecodingKey::from_jwk(&jwk)
                .map_err(|e| JsErrorBox::generic(format!("oidc.verify: {e}")))?;
            verify_with(&token, &key).map_err(JsErrorBox::generic)
        }
    }
}

/// oidc 信息（issuer/jwks/rp/clients 一次取齐；bootstrap 侧拆成 getter）。
#[deno_core::op2]
#[serde]
pub fn op_oidc_info(state: Rc<RefCell<OpState>>) -> Result<serde_json::Value, JsErrorBox> {
    let st = oidc_of(&state.borrow())?;
    Ok(serde_json::json!({
        "issuer": st.issuer,
        "jwks": st.jwks(),
        "rp": st.rp.iter().map(|(k, v)| (k.clone(), serde_json::json!({
            "issuer": v.issuer, "client_id": v.client_id,
            "client_secret": v.client_secret, "scope": v.scope,
        }))).collect::<serde_json::Map<String, serde_json::Value>>(),
        "clients": st.clients.iter().map(|(k, v)| (k.clone(), serde_json::json!({
            "secret": v.secret, "redirect_uris": v.redirect_uris, "tenant": v.tenant,
        }))).collect::<serde_json::Map<String, serde_json::Value>>(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{OidcClientCfg, OidcRpCfg, OidcSection};
    use rsa::pkcs8::EncodePrivateKey;

    fn write_key(dir: &std::path::Path) -> String {
        let key = rsa::RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).unwrap();
        let pem = key.to_pkcs8_pem(rsa::pkcs8::LineEnding::LF).unwrap();
        let path = dir.join("oidc_rs256.pem");
        std::fs::write(&path, pem.as_bytes()).unwrap();
        path.to_str().unwrap().to_string()
    }

    fn section(_dir: &std::path::Path, key_path: &str, issuer: &str) -> OidcSection {
        OidcSection {
            issuer: issuer.into(),
            private_key_path: key_path.into(),
            rp: [(
                "default".into(),
                OidcRpCfg {
                    issuer: issuer.into(),
                    client_id: "sample-rp".into(),
                    client_secret: "rp-secret".into(),
                    scope: "openid profile".into(),
                },
            )]
            .into_iter()
            .collect(),
            clients: [(
                "sample-rp".into(),
                OidcClientCfg {
                    secret: "rp-secret".into(),
                    redirect_uris: vec!["http://h/v1/api/oidc/callback".into()],
                    tenant: "default".into(),
                },
            )]
            .into_iter()
            .collect(),
        }
    }

    #[test]
    fn from_section_loads_key_and_exposes_config() {
        let dir = std::env::temp_dir().join(format!("oj-oidc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let key_path = write_key(&dir);
        let st = OidcState::from_section(&section(&dir, &key_path, "http://h/v1/api/idp"), &dir)
            .unwrap();
        assert_eq!(st.issuer, "http://h/v1/api/idp");
        assert_eq!(st.rp["default"].client_id, "sample-rp");
        assert_eq!(st.clients["sample-rp"].tenant, "default");
        let jwks = st.jwks();
        assert_eq!(jwks["keys"][0]["kty"], "RSA");
        assert_eq!(jwks["keys"][0]["alg"], "RS256");
        assert_eq!(jwks["keys"][0]["kid"].as_str().unwrap().len(), 16);
        assert!(st.signing_pem().starts_with("-----BEGIN"));
        let (n, e) = st.n_e();
        assert!(!n.is_empty() && !e.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 独立实例夹具：OidcState 不 Clone（含私钥）；每用例重读同一形态的 key 文件
    /// 构造独立实例。原子计数防并行测试互踩同名临时目录。
    fn make_state(dir_tag: &str) -> OidcState {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "oj-oidc-{dir_tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let key_path = write_key(&dir);
        OidcState::from_section(&section(&dir, &key_path, "http://h/v1/api/idp"), &dir).unwrap()
    }

    /// 经完整 Bridge 跑一段 JS（Extras.oidc 注入），回传 json.ok 信封的 data。
    async fn run_with_oidc(js: &str) -> serde_json::Value {
        let b = crate::bridge::Bridge::with_dbs_and_loader(
            std::collections::HashMap::new(),
            std::sync::Arc::new(crate::bridge::InMemoryKV::new()),
            crate::bridge::SchemaRegistry::new(),
            false,
            None,
            crate::bridge::Extras {
                oidc: Some(std::sync::Arc::new(make_state("op"))),
                ..Default::default()
            },
        );
        let cap = b
            .run_with(js, crate::bridge::RequestInfo::default())
            .await
            .unwrap();
        serde_json::from_slice(&cap.body).unwrap()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sign_then_verify_roundtrip_local_and_jwks() {
        let v = run_with_oidc(
            r#"(async () => {
              const now = Math.floor(Date.now() / 1000);
              const claims = { iss: oidc.issuer, sub: "u1", aud: "sample-rp", iat: now, exp: now + 3600, nonce: "n1", tenant: "default" };
              const tok = oidc.sign(claims);
              const local = oidc.verify(tok);
              const remote = oidc.verify(tok, oidc.jwks());
              json.ok({ local, remote, kid: oidc.jwks().keys[0].kid });
            })().catch((e) => json.fail(500, String(e)));"#,
        )
        .await;
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(v["data"]["local"]["sub"], "u1");
        assert_eq!(v["data"]["remote"]["tenant"], "default");
        assert_eq!(v["data"]["kid"].as_str().unwrap().len(), 16);
    }

    /// JWKS 里 kid 对但 kty 非 RSA（如 EC）：不得静默回落本地钥，必须抛错（负路径）。
    #[tokio::test(flavor = "current_thread")]
    async fn verify_with_wrong_kty_jwks_throws() {
        let v = run_with_oidc(
            r#"(async () => {
              const now = Math.floor(Date.now() / 1000);
              const tok = oidc.sign({ iss: oidc.issuer, sub: "u1", aud: "a", iat: now, exp: now + 3600 });
              const kid = oidc.jwks().keys[0].kid;
              let msg = "no-throw";
              try { oidc.verify(tok, { keys: [{ kty: "EC", kid, alg: "RS256" }] }); } catch (e) { msg = String(e); }
              json.ok(msg);
            })().catch((e) => json.fail(500, String(e)));"#,
        )
        .await;
        assert_eq!(v["code"], 0, "{v}");
        assert!(
            v["data"].as_str().unwrap().contains("no RS256 key for kid"),
            "{v}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn verify_rejects_tampered_expired_and_wrong_alg() {
        let v = run_with_oidc(
            r#"(async () => {
              const now = Math.floor(Date.now() / 1000);
              const good = oidc.sign({ iss: oidc.issuer, sub: "u1", aud: "a", iat: now, exp: now + 3600 });
              // tamper the payload (re-encode the middle segment)
              const parts = good.split(".");
              const payload = JSON.parse(atobUrl(parts[1]));
              payload.sub = "evil";
              const b64u = (s) => { const B = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"; let bin = ""; for (const c of s) bin += String.fromCharCode(c.charCodeAt(0)); const bytes = new Uint8Array(bin.length); for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i); let out = ""; for (let i = 0; i < bytes.length; i += 3) { const n = (bytes[i] << 16) | ((bytes[i+1] ?? 0) << 8) | (bytes[i+2] ?? 0); out += B[(n >> 18) & 63] + B[(n >> 12) & 63] + ((bytes[i+1] !== undefined) ? B[(n >> 6) & 63] : "") + ((bytes[i+2] !== undefined) ? B[n & 63] : ""); } return out; };
              function atobUrl(s) { const B = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"; let out = ""; let bits = 0, acc = 0; for (const c of s) { const v = B.indexOf(c); if (v < 0) continue; acc = (acc << 6) | v; bits += 6; if (bits >= 8) { bits -= 8; out += String.fromCharCode((acc >> bits) & 0xff); } } return out; }
              const tampered = parts[0] + "." + b64u(JSON.stringify(payload)) + "." + parts[2];
              const expired = oidc.sign({ iss: oidc.issuer, sub: "u1", aud: "a", iat: now - 7200, exp: now - 3600 });
              const results = {};
              try { oidc.verify(tampered); results.tampered = "ok"; } catch (e) { results.tampered = "err"; }
              try { oidc.verify(expired); results.expired = "ok"; } catch (e) { results.expired = "err"; }
              try { oidc.verify("eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJ4In0.aaaa"); results.wrong_alg = "ok"; } catch (e) { results.wrong_alg = "err"; }
              json.ok(results);
            })().catch((e) => json.fail(500, String(e)));"#,
        )
        .await;
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(v["data"]["tampered"], "err", "{v}");
        assert_eq!(v["data"]["expired"], "err", "{v}");
        assert_eq!(v["data"]["wrong_alg"], "err", "{v}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn verify_without_config_errors() {
        let b = crate::bridge::Bridge::new(
            std::sync::Arc::new(crate::bridge::InMemoryAccessor::new()),
            std::sync::Arc::new(crate::bridge::InMemoryKV::new()),
        );
        let cap = b
            .run_with(
                r#"(async () => { try { oidc.sign({}); json.ok("no"); } catch (e) { json.ok(String(e)); } })()"#,
                crate::bridge::RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(
            v["data"].as_str().unwrap().contains("oidc not configured"),
            "{v}"
        );
    }

    #[test]
    fn from_section_fail_fast_on_bad_inputs() {
        let dir = std::env::temp_dir().join(format!("oj-oidc-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let key_path = write_key(&dir);
        // issuer 空
        let e = OidcState::from_section(&section(&dir, &key_path, ""), &dir).unwrap_err();
        assert!(e.contains("issuer"), "{e}");
        // key 文件不存在
        let e = OidcState::from_section(&section(&dir, "nope.pem", "http://h"), &dir).unwrap_err();
        assert!(
            e.contains("oidc_rs256.pem") || e.contains("nope.pem"),
            "{e}"
        );
        // 非法 PEM
        let bad = dir.join("bad.pem");
        std::fs::write(&bad, "not a pem").unwrap();
        let e = OidcState::from_section(&section(&dir, "bad.pem", "http://h"), &dir).unwrap_err();
        assert!(e.contains("pkcs8") || e.contains("private key"), "{e}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
