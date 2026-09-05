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
