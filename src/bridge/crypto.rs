//! jwt / bcrypt / crypto 密码学原语 op（auth 解耦：核心只留原语，业务语义在 JS/插件）。
//! jwt 配置经 Extras.jwt 注入（装配层从 config.auth 构建）；未配置 → "jwt not configured"。

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use deno_core::{OpState, op2};
use deno_error::JsErrorBox;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes128Gcm, Aes256Gcm, Nonce};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use getrandom::getrandom;

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
    // 出口护栏（见 jsnum）：claims 是**外部可控**的 JSON——雪花量级的数值声明若原样交给 JS
    // 会变成 v8 BigInt，handler 里 `json.ok(claims)` 直接 500。
    let mut claims =
        serde_json::to_value(d.claims).map_err(|e| JsErrorBox::generic(e.to_string()))?;
    super::jsnum::sanitize_js_numbers(&mut claims);
    Ok(claims)
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

/// crypto.getRandomValues 的随机源（bootstrap.js 填充 TypedArray view；上限对齐 WebCrypto 65536）。
#[op2]
#[serde]
pub fn op_crypto_random(n: u32) -> Vec<u8> {
    let n = n.min(65536) as usize;
    let mut b = vec![0u8; n];
    getrandom::getrandom(&mut b).expect("system rng");
    b
}

/// crypto.aesGcmEncrypt(plaintext, key)：key 为 hex/base64 编码的原始 16/32 字节，
/// 按长度选 Aes128/256（aes-gcm 0.10 不含 AES-192）；返回 `base64(nonce12 ‖ ciphertext ‖ tag16)`。
/// 密钥由调用方从 `vars.get(...)`（可被 `ENC[...]` 密封）传入，op 保持纯原语、不耦合 config。
#[op2]
#[string]
pub fn op_aes_gcm_encrypt(
    #[string] plaintext: String,
    #[string] key: String,
) -> Result<String, JsErrorBox> {
    let key = decode_key(&key)?;
    let mut nonce = [0u8; 12];
    getrandom(&mut nonce).map_err(|e| JsErrorBox::generic(e.to_string()))?;
    let ct = aes_encrypt(&key, &nonce, plaintext.as_bytes())?;
    let mut out = nonce.to_vec();
    out.extend_from_slice(&ct);
    Ok(STANDARD.encode(out))
}

/// crypto.aesGcmDecrypt(ciphertext, key)：ciphertext 为 `base64(nonce12 ‖ ct ‖ tag16)`，
/// 返回明文字符串；篡改/密钥错误抛错（GCM tag 校验失败即报错）。
#[op2]
#[string]
pub fn op_aes_gcm_decrypt(
    #[string] ciphertext: String,
    #[string] key: String,
) -> Result<String, JsErrorBox> {
    let key = decode_key(&key)?;
    let raw = STANDARD
        .decode(ciphertext.trim())
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(ciphertext.trim()))
        .map_err(|e| JsErrorBox::generic(format!("ciphertext not base64: {e}")))?;
    if raw.len() < 12 {
        return Err(JsErrorBox::generic("ciphertext too short"));
    }
    let (nonce, ct) = raw.split_at(12);
    let pt = aes_decrypt(&key, nonce, ct)?;
    String::from_utf8(pt).map_err(|e| JsErrorBox::generic(format!("plaintext not utf8: {e}")))
}

/// key 为 hex（偶数长且全 hex 字符）或 base64（STANDARD / STANDARD_NO_PAD）编码的原始字节。
fn decode_key(s: &str) -> Result<Vec<u8>, JsErrorBox> {
    let t = s.trim();
    if t.len().is_multiple_of(2) && t.bytes().all(|b| b.is_ascii_hexdigit()) {
        let mut out = Vec::with_capacity(t.len() / 2);
        let mut i = t.bytes();
        while let (Some(h), Some(l)) = (i.next(), i.next()) {
            let hb = (h as char).to_digit(16).unwrap() as u8;
            let lb = (l as char).to_digit(16).unwrap() as u8;
            out.push((hb << 4) | lb);
        }
        return Ok(out);
    }
    STANDARD
        .decode(t)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(t))
        .map_err(|e| JsErrorBox::generic(format!("key not hex/base64: {e}")))
}

/// 按原始密钥长度选择 Aes128/256 并加密（nonce 固定 12 字节）。
/// 注意：aes-gcm 0.10 仅重导出 Aes128/Aes256，故 24 字节（AES-192）不受支持。
fn aes_encrypt(key: &[u8], nonce: &[u8; 12], plaintext: &[u8]) -> Result<Vec<u8>, JsErrorBox> {
    match key.len() {
        16 => Aes128Gcm::new_from_slice(key)
            .map_err(box_err)?
            .encrypt(Nonce::from_slice(nonce), plaintext)
            .map_err(enc_err),
        32 => Aes256Gcm::new_from_slice(key)
            .map_err(box_err)?
            .encrypt(Nonce::from_slice(nonce), plaintext)
            .map_err(enc_err),
        n => Err(JsErrorBox::generic(format!(
            "aes key must be 16/32 raw bytes (AES-192/24-byte not supported; got {n})"
        ))),
    }
}

/// 按原始密钥长度选择 Aes128/256 并解密。
fn aes_decrypt(key: &[u8], nonce: &[u8], ct: &[u8]) -> Result<Vec<u8>, JsErrorBox> {
    match key.len() {
        16 => Aes128Gcm::new_from_slice(key)
            .map_err(box_err)?
            .decrypt(Nonce::from_slice(nonce), ct)
            .map_err(dec_err),
        32 => Aes256Gcm::new_from_slice(key)
            .map_err(box_err)?
            .decrypt(Nonce::from_slice(nonce), ct)
            .map_err(dec_err),
        n => Err(JsErrorBox::generic(format!(
            "aes key must be 16/32 raw bytes (AES-192/24-byte not supported; got {n})"
        ))),
    }
}

fn enc_err<E: std::fmt::Display>(e: E) -> JsErrorBox {
    JsErrorBox::generic(format!("aes-gcm encrypt failed: {e}"))
}

fn dec_err<E: std::fmt::Display>(e: E) -> JsErrorBox {
    JsErrorBox::generic(format!("aes-gcm decrypt failed: {e}"))
}

fn box_err<E: std::fmt::Display>(e: E) -> JsErrorBox {
    JsErrorBox::generic(format!("aes key init failed: {e}"))
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
    async fn aes_gcm_roundtrip_and_bad_key() {
        let b = bridge(None);
        let cap = b
            .run_with(
                r#"(async () => {
                    // 32 字节原始密钥（hex），等价于 fixtures 用的 sealed vars 取值。
                    const k = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
                    const ct = crypto.aesGcmEncrypt("hello metabase", k);
                    const pt = crypto.aesGcmDecrypt(ct, k);
                    let bad = null;
                    try { crypto.aesGcmDecrypt(ct, "deadbeef"); } catch (e) { bad = String(e); }
                    let badLen = null;
                    try { crypto.aesGcmEncrypt("x", "short"); } catch (e) { badLen = String(e); }
                    json.ok({ ct, pt, bad, badLen });
                })().catch((e) => json.fail(500, String(e)));"#,
                Default::default(),
            )
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["data"]["pt"], "hello metabase", "{v}");
        assert!(v["data"]["ct"].as_str().unwrap().len() > 16, "{v}");
        assert!(v["data"]["bad"].is_string(), "{v}");
        assert!(v["data"]["badLen"].is_string(), "{v}");
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

    #[test]
    fn from_auth_cfg_gates_alg_and_duration_failures() {
        // 装配期门禁：不支持的方法 / 非法 duration 各自点名（fail-fast 在装配，不进运行时）。
        let Err(m) = JwtCfg::from_auth_cfg(&crate::config::AuthCfg {
            jwt_secret: "k".into(),
            signing_method: "RS256".into(),
            access_token_duration: "60s".into(),
            refresh_token_duration: "720h".into(),
            anonymous_paths: vec![],
            cookie: None,
        }) else {
            panic!("RS256 must be rejected")
        };
        assert!(m[..].contains("not supported"), "{}", &m[..]);

        let Err(m) = JwtCfg::from_auth_cfg(&crate::config::AuthCfg {
            jwt_secret: "k".into(),
            signing_method: "HS256".into(),
            access_token_duration: "soon".into(),
            refresh_token_duration: "720h".into(),
            anonymous_paths: vec![],
            cookie: None,
        }) else {
            panic!("bad duration must be rejected")
        };
        assert!(m[..].contains("access_token_duration"), "{}", &m[..]);

        let ok = JwtCfg::from_auth_cfg(&crate::config::AuthCfg {
            jwt_secret: "k".into(),
            signing_method: "HS512".into(),
            access_token_duration: "60s".into(),
            refresh_token_duration: "720h".into(),
            anonymous_paths: vec![],
            cookie: None,
        })
        .unwrap();
        assert_eq!(ok.alg, "HS512");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn jwt_sign_rejects_non_string_sub_and_hs512_roundtrips() {
        // payload.sub 非字符串 → 点名报错；HS512 算法臂全链路（sign→verify）。
        let base = jwt_cfg();
        let cfg = JwtCfg {
            secret: base.secret.clone(),
            alg: "HS512".into(),
            access_secs: base.access_secs,
            refresh_secs: base.refresh_secs,
        };
        let b = bridge(Some(Arc::new(cfg)));
        let cap = b
            .run_with(
                r#"(async () => {
                    let noSub = null;
                    try { await jwt.sign({ roles: [] }); } catch (e) { noSub = String(e); }
                    const t = await jwt.sign({ sub: "9", roles: ["r1", 2, "r2"] });
                    const c = await jwt.verify(t);
                    json.ok({ noSub, sub: c.sub, roles: c.roles });
                })().catch((e) => json.fail(500, String(e)));"#,
                Default::default(),
            )
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(
            v["data"]["noSub"]
                .as_str()
                .unwrap()
                .contains("must be a string"),
            "{v}"
        );
        assert_eq!(v["data"]["sub"], "9", "{v}");
        // 非 string roles 元素被过滤（对齐守卫插件解码契约）。
        assert_eq!(v["data"]["roles"], serde_json::json!(["r1", "r2"]), "{v}");
    }
}
