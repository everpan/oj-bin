//! jwt / bcrypt / crypto 密码学原语 op（auth 解耦：核心只留原语，业务语义在 JS/插件）。
//! jwt 配置经 Extras.jwt 注入（装配层从 config.auth 构建）；未配置 → "jwt not configured"。

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use deno_core::{OpState, op2};
use deno_error::JsErrorBox;

use super::StableState;

/// jwt 运行时配置（装配期从 AuthCfg 构建，fail-fast 同旧 Auth::new 语义）。
pub struct JwtCfg {
    pub secret: String,
    /// HS256 | HS384 | HS512。
    pub alg: String,
    pub access_secs: u64,
    pub refresh_secs: u64,
}

impl JwtCfg {
    pub fn from_auth_cfg(cfg: &crate::config::AuthCfg) -> Result<Self, String> {
        // alg 合法性在此 fail-fast（与旧 Auth::new 一致）
        match cfg.signing_method.as_str() {
            "HS256" | "HS384" | "HS512" => {}
            other => {
                return Err(format!(
                    "auth.signing_method '{other}' not supported (HS256|HS384|HS512)"
                ));
            }
        }
        let access = crate::config::parse_duration(&cfg.access_token_duration)
            .map_err(|e| format!("auth.access_token_duration: {e}"))?;
        let refresh = crate::config::parse_duration(&cfg.refresh_token_duration)
            .map_err(|e| format!("auth.refresh_token_duration: {e}"))?;
        Ok(Self {
            secret: cfg.jwt_secret.clone(),
            alg: cfg.signing_method.clone(),
            access_secs: access.as_secs(),
            refresh_secs: refresh.as_secs(),
        })
    }

    fn algorithm(&self) -> jsonwebtoken::Algorithm {
        match self.alg.as_str() {
            "HS384" => jsonwebtoken::Algorithm::HS384,
            "HS512" => jsonwebtoken::Algorithm::HS512,
            _ => jsonwebtoken::Algorithm::HS256,
        }
    }
}

/// access token 载荷（与旧 server/auth.rs Claims 同形，守卫插件侧解码契约）。
#[derive(serde::Serialize, serde::Deserialize)]
struct Claims {
    sub: String,
    roles: Vec<String>,
    iat: u64,
    exp: u64,
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn jwt(state: &OpState) -> Result<Arc<JwtCfg>, JsErrorBox> {
    state
        .borrow::<Arc<StableState>>()
        .jwt
        .clone()
        .ok_or_else(|| JsErrorBox::generic("jwt not configured (config auth: section missing)"))
}

/// jwt.sign({sub, roles})：iat/exp 由 Rust 补（JS 不可控有效期）。
#[op2]
#[string]
pub fn op_jwt_sign(
    state: Rc<RefCell<OpState>>,
    #[serde] payload: serde_json::Value,
) -> Result<String, JsErrorBox> {
    let cfg = jwt(&state.borrow())?;
    let sub = payload["sub"]
        .as_str()
        .ok_or_else(|| JsErrorBox::generic("jwt.sign: payload.sub must be a string"))?;
    let roles: Vec<String> = payload["roles"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|r| r.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let now = now_unix();
    let claims = Claims {
        sub: sub.to_string(),
        roles,
        iat: now,
        exp: now + cfg.access_secs,
    };
    jsonwebtoken::encode(
        &jsonwebtoken::Header::new(cfg.algorithm()),
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(cfg.secret.as_bytes()),
    )
    .map_err(|e| JsErrorBox::generic(e.to_string()))
}

/// jwt.verify(token) → claims；篡改/过期/算法不符均抛错（leeway 0）。
#[op2]
#[serde]
pub fn op_jwt_verify(
    state: Rc<RefCell<OpState>>,
    #[string] token: String,
) -> Result<serde_json::Value, JsErrorBox> {
    let cfg = jwt(&state.borrow())?;
    let mut v = jsonwebtoken::Validation::new(cfg.algorithm());
    v.leeway = 0;
    v.validate_exp = true;
    v.validate_aud = false;
    let d = jsonwebtoken::decode::<Claims>(
        &token,
        &jsonwebtoken::DecodingKey::from_secret(cfg.secret.as_bytes()),
        &v,
    )
    .map_err(|e| JsErrorBox::generic(e.to_string()))?;
    serde_json::to_value(d.claims).map_err(|e| JsErrorBox::generic(e.to_string()))
}

/// jwt.accessDuration / jwt.refreshDuration（秒；getter 每 runtime 惰性取）。
#[op2]
#[serde]
pub fn op_jwt_durations(state: Rc<RefCell<OpState>>) -> Result<serde_json::Value, JsErrorBox> {
    let cfg = jwt(&state.borrow())?;
    Ok(serde_json::json!({
        "access": cfg.access_secs,
        "refresh": cfg.refresh_secs,
    }))
}

/// bcrypt.hash(pw, cost?)：CPU 密集，spawn_blocking 避免卡住 isolate 所在线程。
#[op2]
#[string]
pub async fn op_bcrypt_hash(
    #[string] password: String,
    cost: Option<u32>,
) -> Result<String, JsErrorBox> {
    tokio::task::spawn_blocking(move || {
        bcrypt::hash(password, cost.unwrap_or(bcrypt::DEFAULT_COST))
    })
    .await
    .map_err(|e| JsErrorBox::generic(e.to_string()))?
    .map_err(|e| JsErrorBox::generic(e.to_string()))
}

/// bcrypt.verify(pw, hash)：非法 hash → false（不抛错，对齐旧 unwrap_or(false)）。
#[op2]
pub async fn op_bcrypt_verify(
    #[string] password: String,
    #[string] hash: String,
) -> Result<bool, JsErrorBox> {
    tokio::task::spawn_blocking(move || bcrypt::verify(password, &hash).unwrap_or(false))
        .await
        .map_err(|e| JsErrorBox::generic(e.to_string()))
}

/// crypto.sha256Hex(s)。
#[op2]
#[string]
pub fn op_sha256_hex(#[string] s: String) -> String {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    h.update(s.as_bytes());
    format!("{:x}", h.finalize())
}

/// crypto.randomHex(nBytes)：默认 32 字节 → 64 hex 字符（refresh token 用）。
#[op2]
#[string]
pub fn op_random_hex(n: Option<u32>) -> String {
    let n = n.unwrap_or(32).min(1024) as usize;
    let mut b = vec![0u8; n];
    getrandom::getrandom(&mut b).expect("system rng");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::{Bridge, Extras, InMemoryAccessor, InMemoryKV, SchemaRegistry};
    use std::collections::HashMap;
    use std::sync::Arc;

    fn jwt_cfg() -> Arc<JwtCfg> {
        Arc::new(JwtCfg {
            secret: "test-secret".into(),
            alg: "HS256".into(),
            access_secs: 60,
            refresh_secs: 720 * 3600,
        })
    }

    fn bridge(jwt: Option<Arc<JwtCfg>>) -> Bridge {
        Bridge::with_dbs_and_loader(
            HashMap::from([(
                "default".to_string(),
                Arc::new(InMemoryAccessor::new()) as Arc<dyn crate::bridge::DataAccessor>,
            )]),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            None,
            Extras {
                jwt,
                ..Default::default()
            },
        )
    }

    #[tokio::test(flavor = "current_thread")]
    async fn jwt_sign_verify_roundtrip_and_tamper() {
        let b = bridge(Some(jwt_cfg()));
        let cap = b
            .run_with(
                r#"(async () => {
                    const t = await jwt.sign({ sub: "7", roles: ["admin"] });
                    const c = await jwt.verify(t);
                    let tampered = null;
                    try { await jwt.verify(t + "x"); } catch (e) { tampered = String(e); }
                    json.ok({ sub: c.sub, roles: c.roles, tampered, dur: [jwt.accessDuration, jwt.refreshDuration] });
                })().catch((e) => json.fail(500, String(e)));"#,
                Default::default(),
            )
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["data"]["sub"], "7", "{v}");
        assert_eq!(v["data"]["roles"][0], "admin", "{v}");
        assert!(v["data"]["tampered"].is_string(), "{v}");
        assert_eq!(v["data"]["dur"], serde_json::json!([60, 720 * 3600]), "{v}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn jwt_not_configured_errors() {
        let b = bridge(None);
        let cap = b
            .run_with(
                r#"(async () => { await jwt.sign({ sub: "1", roles: [] }); })()
                    .catch((e) => json.ok({ err: String(e) }));"#,
                Default::default(),
            )
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(
            v["data"]["err"]
                .as_str()
                .unwrap()
                .contains("jwt not configured"),
            "{v}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn bcrypt_and_crypto_ops() {
        let b = bridge(None);
        let cap = b
            .run_with(
                r#"(async () => {
                    const h = await bcrypt.hash("pw123", 4);
                    const okV = await bcrypt.verify("pw123", h);
                    const bad = await bcrypt.verify("nope", h);
                    json.ok({
                        okV, bad,
                        sha: crypto.sha256Hex("abc"),
                        hexLen: crypto.randomHex(32).length,
                        randType: typeof crypto.getRandomValues,
                    });
                })().catch((e) => json.fail(500, String(e)));"#,
                Default::default(),
            )
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["data"]["okV"], true, "{v}");
        assert_eq!(v["data"]["bad"], false, "{v}");
        assert_eq!(
            v["data"]["sha"],
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(v["data"]["hexLen"], 64);
    }
}
