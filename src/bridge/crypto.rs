//! jwt / bcrypt / crypto 密码学原语 op（auth 解耦：核心只留原语，业务语义在 JS/插件）。
//! jwt 配置经 Extras.jwt 注入（装配层从 config.auth 构建）；未配置 → "jwt not configured"。

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use deno_core::{JsBuffer, OpState, op2};
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

/// crypto.md5Hex(s)：MD5 已密码学破损，仅限非安全场景（缓存键/etag/旧系统兼容）。
#[op2]
#[string]
pub fn op_md5_hex(#[string] s: String) -> String {
    use md5::Digest;
    let mut h = md5::Md5::new();
    h.update(s.as_bytes());
    format!("{:x}", h.finalize())
}

/// crypto.md5Hex 二进制扩面：对任意字节序列（Uint8Array view）求 MD5 hex
/// （字符串走 op_md5_hex，bootstrap.js 按类型分流）。
#[op2]
#[string]
pub fn op_md5_bytes(#[buffer] data: JsBuffer) -> String {
    use md5::Digest;
    let mut h = md5::Md5::new();
    h.update(&data);
    format!("{:x}", h.finalize())
}

/// crypto.verifyEd25519(publicKey, msg, sigBase64)：Ed25519 验签（纯原语，bundle 签名校验用）。
/// publicKey 为 hex/base64 编码的 32 字节原始公钥（不支持 PEM/SPKI）；msg 为 UTF-8 字符串；
/// sig 仅 base64（STANDARD / NO_PAD，base64url 不支持）。格式错（base64 非法、公钥 ≠ 32 字节、
/// sig 解码后 ≠ 64 字节）抛错；签名/密钥不匹配返回 false。
#[op2(fast)]
pub fn op_ed25519_verify(
    #[string] public_key: String,
    #[string] msg: String,
    #[string] sig_base64: String,
) -> Result<bool, JsErrorBox> {
    let pk =
        decode_key(&public_key).map_err(|e| JsErrorBox::generic(format!("verifyEd25519: {e}")))?;
    if pk.len() != 32 {
        return Err(JsErrorBox::generic(format!(
            "verifyEd25519: public key must be 32 raw bytes (got {})",
            pk.len()
        )));
    }
    let sig_b64 = sig_base64.trim();
    let sig = STANDARD
        .decode(sig_b64)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(sig_b64))
        .map_err(|e| JsErrorBox::generic(format!("verifyEd25519: signature not base64: {e}")))?;
    if sig.len() != 64 {
        return Err(JsErrorBox::generic(format!(
            "verifyEd25519: signature must be 64 bytes (got {})",
            sig.len()
        )));
    }
    let v = ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &pk);
    Ok(v.verify(msg.as_bytes(), &sig).is_ok())
}

/// crypto.sha256Hex 二进制扩面：对任意字节序列（Uint8Array view）求 SHA-256 hex
/// （字符串走 op_sha256_hex，bootstrap.js 按类型分流）。
#[op2]
#[string]
pub fn op_sha256_bytes(#[buffer] data: JsBuffer) -> String {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    h.update(&data);
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

    #[tokio::test(flavor = "current_thread")]
    async fn ed25519_verify_rfc8032_vectors_and_format_gates() {
        // RFC 8032 §7.1 TEST 1 / TEST 2 官方向量；格式错抛错（文案以 verifyEd25519: 起头），
        // 验签不过返回 false（双层语义）。
        let b = bridge(None);
        let cap = b
            .run_with(
                r#"(async () => {
                    const PK = "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a";
                    // RFC 8032 TEST 2 自带公钥（msg 为单字节 0x72 = "r"）
                    const PK2 = "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c";
                    const PK_B64 = "11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=";
                    const PK_URL = "11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo=";
                    const SIG1 = "5VZDAMNgrHKQhuLMgG6CioSHfx645dl02HPgZSJJAVVfuIIVkKM7rMYeOXAc+bRr0lv18FlbviRlUUFDjnoQCw==";
                    const SIG2 = "kqAJqfDUyrhyDoILX2QlQKKye1QWUD+Ps3YiI+vbadoIWsHkPhWZbkWPNhPQ8R2MOHsurrQwKu6wDSkWErsMAA==";
                    const SIG63 = "5VZDAMNgrHKQhuLMgG6CioSHfx645dl02HPgZSJJAVVfuIIVkKM7rMYeOXAc+bRr0lv18FlbviRlUUFDjnoQ";
                    // 场景1：正确签名（Test 1，msg=""）通过
                    const s1 = crypto.verifyEd25519(PK, "", SIG1);
                    // 场景2：篡改拒绝 —— msg 配错签名 false；Test 2 正向 true；再篡改 false
                    const s2a = crypto.verifyEd25519(PK, "r", SIG1);
                    const s2b = crypto.verifyEd25519(PK2, "r", SIG2);
                    const s2c = crypto.verifyEd25519(PK, "s", SIG2);
                    // 场景3：错公钥拒绝（翻转首字节）
                    const s3 = crypto.verifyEd25519("c7" + PK.slice(2), "r", SIG2);
                    // 场景4：非法 base64 → 抛错
                    let e4 = null;
                    try { crypto.verifyEd25519(PK, "", "!!!"); } catch (err) { e4 = String(err); }
                    // 场景5：公钥 31/33 字节 → 抛错
                    let e5a = null, e5b = null;
                    try { crypto.verifyEd25519(PK.slice(0, 62), "", SIG1); } catch (err) { e5a = String(err); }
                    try { crypto.verifyEd25519(PK + "aa", "", SIG1); } catch (err) { e5b = String(err); }
                    // 场景6：sig 解码后 63 字节 → 抛错
                    let e6 = null;
                    try { crypto.verifyEd25519(PK, "", SIG63); } catch (err) { e6 = String(err); }
                    // 场景7：hex/base64 双编码均可用（同一把公钥换 base64 形式）
                    const s7 = crypto.verifyEd25519(PK_B64, "", SIG1);
                    // 场景8：base64url 公钥（含 '_'）→ 抛错点名
                    let e8 = null;
                    try { crypto.verifyEd25519(PK_URL, "", SIG1); } catch (err) { e8 = String(err); }
                    // 场景9：hex 优先于 base64 —— 32 字符全 hexdigit 串同时是合法 NO_PAD
                    // base64（会解出 24 字节），hex 分支优先故解出 16 字节并点名长度
                    let e9 = null;
                    try { crypto.verifyEd25519("0123456789abcdef0123456789abcdef", "", SIG1); } catch (err) { e9 = String(err); }
                    // 场景10：格式合法但密码学无效 —— 32 字节全零公钥 + 合法 64 字节 sig → false（不抛错）
                    const s10 = crypto.verifyEd25519("0".repeat(64), "r", SIG2);
                    json.ok({ s1, s2a, s2b, s2c, s3, e4, e5a, e5b, e6, s7, e8, e9, s10 });
                })().catch((e) => json.fail(500, String(e)));"#,
                Default::default(),
            )
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&cap.body).unwrap();
        let d = &v["data"];
        // 通过/拒绝双层
        assert_eq!(d["s1"], true, "{v}");
        assert_eq!(d["s2a"], false, "{v}");
        assert_eq!(d["s2b"], true, "{v}");
        assert_eq!(d["s2c"], false, "{v}");
        assert_eq!(d["s3"], false, "{v}");
        assert_eq!(d["s7"], true, "{v}");
        // 格式错抛错并点名原因（String(err) 带 "Error: " 前缀，故查 contains）
        let e4 = d["e4"].as_str().unwrap();
        assert!(e4.contains("verifyEd25519: signature not base64"), "{e4}");
        for e in [d["e5a"].as_str().unwrap(), d["e5b"].as_str().unwrap()] {
            assert!(e.contains("must be 32 raw bytes"), "{e}");
            assert!(e.contains("got "), "{e}");
        }
        let e6 = d["e6"].as_str().unwrap();
        assert!(e6.contains("must be 64 bytes"), "{e6}");
        let e8 = d["e8"].as_str().unwrap();
        assert!(e8.contains("verifyEd25519: key not hex/base64"), "{e8}");
        // hex 分支优先于 base64：16 字节长度点名同时钉死分支选择
        let e9 = d["e9"].as_str().unwrap();
        assert!(e9.contains("must be 32 raw bytes (got 16)"), "{e9}");
        // 无效曲线点 / 全零公钥：格式合法 → 返回 false 而非抛错
        assert_eq!(d["s10"], false, "{v}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn keygen_ed25519_sign_and_verify_roundtrip() {
        // v0.1.80 `oj secret keygen --alg ed25519` 的往返 smoke：上游 keygen 产物
        // （PKCS#8 私钥 PEM + raw 32B 公钥 hex）→ ring 以 PKCS#8 重读并签名（模拟
        // 作者侧 Node `crypto.sign("ed25519", …)`，sig 走标准 base64 无 pad）→
        // 真 bridge 里 `crypto.verifyEd25519` 验签 = true；换 msg = false（不抛）。
        use base64::Engine;
        use ring::signature::KeyPair;

        let (priv_pem, pk) = crate::secret::keygen_ed25519().unwrap();
        assert_eq!(pk.len(), 32);
        let pk_hex = {
            let mut s = String::with_capacity(64);
            for b in pk {
                s.push_str(&format!("{b:02x}"));
            }
            s
        };
        // PEM 体：标准 base64（64 字符换行），剥标签后解码回 DER。
        let b64: String = priv_pem
            .lines()
            .filter(|l| !l.starts_with("-----"))
            .collect();
        let der = base64::engine::general_purpose::STANDARD
            .decode(&b64)
            .expect("pkcs8 pem body");
        let pair = ring::signature::Ed25519KeyPair::from_pkcs8(&der).expect("reparse pkcs8");
        assert_eq!(pair.public_key().as_ref(), &pk[..], "公钥须与私钥配对");
        // 规范串（bundle 校验同形）：<name>@<version>:<sha256>。
        let msg = "app@0.1.80:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        let sig = pair.sign(msg.as_bytes());
        let sig_b64 = base64::engine::general_purpose::STANDARD_NO_PAD.encode(sig.as_ref());

        let b = bridge(None);
        let js = format!(
            r#"(async () => {{
                const pk = "{pk_hex}";
                const ok = crypto.verifyEd25519(pk, "{msg}", "{sig_b64}");
                const bad = crypto.verifyEd25519(pk, "other-msg", "{sig_b64}");
                json.ok({{ ok, bad }});
            }})().catch((e) => json.fail(500, String(e)));"#
        );
        let cap = b.run_with(&js, Default::default()).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["data"]["ok"], true, "{v}");
        assert_eq!(v["data"]["bad"], false, "{v}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sha256_hex_accepts_binary_input() {
        // 场景9：sha256Hex 扩二进制 —— Uint8Array / ArrayBuffer 等价；非 UTF-8 字节不炸。
        let b = bridge(None);
        let cap = b
            .run_with(
                r#"(async () => {
                    const u8 = new Uint8Array([1, 2, 3]);
                    json.ok({
                        bytes: crypto.sha256Hex(u8),
                        ab: crypto.sha256Hex(u8.buffer),
                        nonUtf8: crypto.sha256Hex(new Uint8Array([0xff, 0xfe])),
                    });
                })().catch((e) => json.fail(500, String(e)));"#,
                Default::default(),
            )
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&cap.body).unwrap();
        let d = &v["data"];
        assert_eq!(
            d["bytes"], "039058c6f2c0cb492c533b0a4d14ef77cc0f78abccced5287d84a1a2011cfb81",
            "{v}"
        );
        assert_eq!(
            d["ab"], "039058c6f2c0cb492c533b0a4d14ef77cc0f78abccced5287d84a1a2011cfb81",
            "{v}"
        );
        assert_eq!(
            d["nonUtf8"], "b3d510ef04275ca8e698e5b3cbb0ece3949ef9252f0cdc839e9ee347409a2209",
            "{v}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn md5_hex_string_binary_and_type_gates() {
        // RFC 1321 向量 + 空串 + 二进制/非 UTF-8（期望值经 python3 hashlib 独立预计算，
        // 不用实现自身当期望）+ 非 string/二进制入参 TypeError（与 sha256Hex 同门禁）。
        let b = bridge(None);
        let cap = b
            .run_with(
                r#"(async () => {
                    const u8 = new Uint8Array([1, 2, 3]);
                    let tNum = null, tNull = null;
                    try { crypto.md5Hex(123); } catch (e) { tNum = String(e); }
                    try { crypto.md5Hex(null); } catch (e) { tNull = String(e); }
                    json.ok({
                        abc: crypto.md5Hex("abc"),
                        empty: crypto.md5Hex(""),
                        digest: crypto.md5Hex("message digest"),
                        bytes: crypto.md5Hex(u8),
                        ab: crypto.md5Hex(u8.buffer),
                        nonUtf8: crypto.md5Hex(new Uint8Array([0xff, 0xfe])),
                        tNum, tNull,
                    });
                })().catch((e) => json.fail(500, String(e)));"#,
                Default::default(),
            )
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&cap.body).unwrap();
        let d = &v["data"];
        // RFC 1321 §A.5 测试套件抽查
        assert_eq!(d["abc"], "900150983cd24fb0d6963f7d28e17f72", "{v}");
        assert_eq!(d["empty"], "d41d8cd98f00b204e9800998ecf8427e", "{v}");
        assert_eq!(d["digest"], "f96b697d7cb7938d525a2f31aaf161d0", "{v}");
        // 二进制：Uint8Array 与等价 ArrayBuffer 一致
        assert_eq!(d["bytes"], "5289df737df57326fcdd22597afb1fac", "{v}");
        assert_eq!(d["ab"], "5289df737df57326fcdd22597afb1fac", "{v}");
        // 非 UTF-8 字节不炸且结果正确
        assert_eq!(d["nonUtf8"], "f3b25701fe362ec84616a93a45ce9998", "{v}");
        // 非 string/二进制 → TypeError
        assert!(d["tNum"].as_str().unwrap().contains("TypeError"), "{v}");
        assert!(d["tNull"].as_str().unwrap().contains("TypeError"), "{v}");
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
