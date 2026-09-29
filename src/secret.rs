//! 配置凭据密封（sealed secret）：config.yaml 里的敏感值以 `ENC[<base64url>]` 承载，
//! 装配期用部署机私钥就地解密——**配置文件可以进 git，私钥不可以**。
//!
//! ## 为什么是信封（RSA-OAEP + AES-256-GCM）而不是裸 RSA
//! RSA-OAEP 单次的明文上限是 `k - 2*hLen - 2`（2048 位密钥仅 190 字节），DSN 一长就
//! 顶死；信封只让 RSA 加密 32 字节 CEK，明文长度无上限，且 AES-GCM 自带完整性校验
//! （密文被改一个 bit 会解密失败而非解出垃圾）。
//!
//! ## 为什么解密发生在 `serde_yaml::Value` 层
//! `ldap` / `plugins` / `kafkas` 是不透明 `serde_yaml::Value`（类型层拦不住里面的
//! `bind_pw`），在 Value 树上递归替换才能全覆盖，且**配置 schema 一行都不用改**——
//! 将来新增任何段自动支持。
//!
//! ## 威胁模型（说清楚能防什么）
//! 防的是**配置文件泄漏**（误提交 git、镜像层、备份、工单附件）：泄漏者拿不到私钥就
//! 解不出密码。不防：私钥本身泄漏（那就全完了）、内存取证（明文必然在内存里）。
//! 收益是把「N 个密码」收敛成「1 个私钥」，并让加密权（公钥，可进仓库）与解密权分离。

use std::num::NonZeroU32;
use std::path::Path;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rsa::pkcs1::{DecodeRsaPrivateKey, DecodeRsaPublicKey};
use rsa::pkcs8::{
    DecodePrivateKey, DecodePublicKey, EncodePrivateKey, EncodePublicKey, LineEnding,
};
use rsa::{Oaep, RsaPrivateKey, RsaPublicKey};
use serde_yaml::Value;
use sha2::Sha256;

/// 密封值标记：YAML 里写作 `db: { default: "ENC[<base64url>]" }`。
pub const PREFIX: &str = "ENC[";
pub const SUFFIX: &str = "]";

/// 私钥来源环境变量（PEM 内联）——优先级最高。
pub const ENV_KEY: &str = "OJ_SECRET_KEY";
/// 私钥来源环境变量（PEM 文件路径）。
pub const ENV_KEY_FILE: &str = "OJ_SECRET_KEY_FILE";

/// 信封版本（布局变更即 bump，旧密文仍能按版本识别）。
const VERSION: u8 = 1;
/// AES-256-GCM 内容密钥长度。
const CEK_LEN: usize = 32;
/// GCM nonce 长度。
const NONCE_LEN: usize = 12;
/// GCM tag 长度（密文至少这么长才算没被截断）。
const TAG_LEN: usize = 16;
/// 信封头：版本号 + u16 BE 的 RSA 密文长度。
const HEADER_LEN: usize = 3;

/// 判断一个字符串是否是密封值（不解密、不需要密钥）。
pub fn is_sealed(s: &str) -> bool {
    s.starts_with(PREFIX) && s.ends_with(SUFFIX)
}

/// 用公钥把明文封成 `ENC[...]`。
pub fn seal(public_key_pem: &str, plaintext: &str) -> Result<String, String> {
    seal_pub(&parse_public_key(public_key_pem)?, plaintext)
}

/// 已解析公钥的加密入口（`oj secret seal` 复用同一个 key）。
pub fn seal_pub(pub_key: &RsaPublicKey, plaintext: &str) -> Result<String, String> {
    let mut cek = [0u8; CEK_LEN];
    sys_random(&mut cek).map_err(|e| format!("os random: {e}"))?;
    let mut nonce = [0u8; NONCE_LEN];
    sys_random(&mut nonce).map_err(|e| format!("os random: {e}"))?;

    // RSA-OAEP-SHA256 只封 CEK（32B），与明文长度无关。
    let rsa_ct = pub_key
        .encrypt(&mut SysRng, Oaep::new::<Sha256>(), &cek)
        .map_err(|e| format!("rsa seal failed: {e}"))?;
    let rsa_len = u16::try_from(rsa_ct.len())
        .map_err(|_| format!("rsa ciphertext too long: {} bytes", rsa_ct.len()))?;
    let ct = Aes256Gcm::new_from_slice(&cek)
        .map_err(|e| format!("aes key failed: {e}"))?
        .encrypt(Nonce::from_slice(&nonce), plaintext.as_bytes())
        .map_err(|e| format!("aes-gcm seal failed: {e}"))?;

    let mut env = Vec::with_capacity(HEADER_LEN + rsa_ct.len() + NONCE_LEN + ct.len());
    env.push(VERSION);
    env.extend_from_slice(&rsa_len.to_be_bytes());
    env.extend_from_slice(&rsa_ct);
    env.extend_from_slice(&nonce);
    env.extend_from_slice(&ct);
    Ok(format!("{PREFIX}{}{SUFFIX}", URL_SAFE_NO_PAD.encode(env)))
}

/// 用私钥解出 `ENC[...]` 的明文（也接受裸 base64url，便于排障时粘贴）。
pub fn open(private_key_pem: &str, token: &str) -> Result<String, String> {
    open_pem(&parse_private_key(private_key_pem)?, token)
}

/// 已解析私钥的解密入口（树遍历复用同一个 key，避免每个值都重解析 PEM）。
pub fn open_pem(key: &RsaPrivateKey, token: &str) -> Result<String, String> {
    let body = token
        .strip_prefix(PREFIX)
        .and_then(|s| s.strip_suffix(SUFFIX))
        .unwrap_or(token);
    let env = URL_SAFE_NO_PAD.decode(body).map_err(|e| {
        format!(
            "sealed value is not valid base64url: {e} — 若这其实是**明文**却恰好以 \"ENC[\" \
             开头并以 \"]\" 结尾，请换个写法（如加个前缀/改写大小写），否则会被当密文硬失败"
        )
    })?;
    if env.len() < HEADER_LEN {
        return Err(format!(
            "sealed value truncated: {} bytes (need at least {HEADER_LEN})",
            env.len()
        ));
    }
    if env[0] != VERSION {
        return Err(format!(
            "sealed value version {} unsupported (this oj knows version {VERSION}) — \
             用 `oj secret seal` 重新加密（别手改密文）",
            env[0]
        ));
    }
    let rsa_len = u16::from_be_bytes([env[1], env[2]]) as usize;
    let rest = &env[HEADER_LEN..];
    if rest.len() < rsa_len + NONCE_LEN + TAG_LEN {
        return Err(format!(
            "sealed value truncated: {} bytes body for rsa_len {rsa_len}",
            rest.len()
        ));
    }
    let (rsa_ct, tail) = rest.split_at(rsa_len);
    let (nonce, ct) = tail.split_at(NONCE_LEN);
    let cek = key
        .decrypt(Oaep::new::<Sha256>(), rsa_ct)
        .map_err(|e| format!("rsa open failed (wrong private key?): {e}"))?;
    let pt = Aes256Gcm::new_from_slice(&cek)
        .map_err(|e| format!("aes key failed: {e}"))?
        .decrypt(Nonce::from_slice(nonce), ct)
        .map_err(|e| format!("aes-gcm open failed (sealed value tampered?): {e}"))?;
    String::from_utf8(pt).map_err(|e| format!("sealed value is not utf-8: {e}"))
}

/// 配置树里是否存在密封值——决定要不要去碰私钥
/// （无密封值的旧配置必须**完全不碰密钥路径**，行为逐字节不变）。
pub fn has_sealed(v: &Value) -> bool {
    match v {
        Value::String(s) => is_sealed(s),
        // 键位也扫：键上的密封值由 decrypt_tree 报错（不静默留密文），
        // 这里必须一起判，否则「有密封值却不加载私钥」会跳过那条报错。
        Value::Mapping(m) => m.iter().any(|(k, v)| has_sealed(k) || has_sealed(v)),
        Value::Sequence(s) => s.iter().any(has_sealed),
        Value::Tagged(t) => has_sealed(&t.value),
        _ => false,
    }
}

/// 就地解密配置树里的全部密封值，返回解密个数。
///
/// 递归走 Value 而非 `Config` 结构体：`ldap` / `plugins` / `kafkas` 是不透明 Value，
/// 类型层拦不住里面的 `bind_pw`；且这样将来新增任何段自动支持，schema 零改动。
pub fn decrypt_tree(v: &mut Value, key: &RsaPrivateKey) -> Result<usize, String> {
    let mut n = 0;
    match v {
        Value::String(s) => {
            if is_sealed(s) {
                *s = open_pem(key, s)?;
                n += 1;
            }
        }
        Value::Mapping(m) => {
            // 整体重建以保序（`remove`+`insert` 会把键挪到末尾，改了声明顺序）。
            let entries: Vec<(Value, Value)> = std::mem::take(m).into_iter().collect();
            let mut out = Vec::with_capacity(entries.len());
            for (k, mut val) in entries {
                // 键位上的密封值一律报错：解密结果只能是字符串，而键的语义是「名字」，
                // 静默留着密文会让后续装配拿着 `ENC[…]` 当名字用——fail-closed 优先。
                if let Value::String(s) = &k
                    && is_sealed(s)
                {
                    return Err(format!(
                        "sealed value used as a mapping key ({s}): ENC[...] may only appear as a value"
                    ));
                }
                n += decrypt_tree(&mut val, key)?;
                out.push((k, val));
            }
            *m = out.into_iter().collect();
        }
        Value::Sequence(s) => {
            for item in s.iter_mut() {
                n += decrypt_tree(item, key)?;
            }
        }
        Value::Tagged(t) => n += decrypt_tree(&mut t.value, key)?,
        _ => {}
    }
    Ok(n)
}

/// 生成密钥对（PEM 字符串）：`(private_pkcs8, public_spki)`。
pub fn keygen(bits: usize) -> Result<(String, String), String> {
    if bits < 2048 {
        return Err(format!(
            "rsa bits must be >= 2048 (got {bits}); 1024-bit RSA is factorable in practice"
        ));
    }
    let mut rng = SysRng;
    let private = RsaPrivateKey::new(&mut rng, bits).map_err(|e| format!("rsa keygen: {e}"))?;
    let public = RsaPublicKey::from(&private);
    let priv_pem = private
        .to_pkcs8_pem(LineEnding::LF)
        .map_err(|e| format!("encode private key: {e}"))?
        .to_string();
    let pub_pem = public
        .to_public_key_pem(LineEnding::LF)
        .map_err(|e| format!("encode public key: {e}"))?;
    Ok((priv_pem, pub_pem))
}

/// 私钥加载：env 内联 PEM > env 文件 > config 的 `secrets.private_key_path`
/// （相对 config 目录）。三处都没有 → 明确报错（绝不静默把密文当明文用）。
pub fn load_private_key(
    cfg_path: Option<&str>,
    config_dir: &Path,
) -> Result<RsaPrivateKey, String> {
    if let Ok(inline) = std::env::var(ENV_KEY)
        && !inline.trim().is_empty()
    {
        return parse_private_key(&inline).map_err(|e| format!("{ENV_KEY}: {e}"));
    }
    if let Ok(p) = std::env::var(ENV_KEY_FILE)
        && !p.trim().is_empty()
    {
        let pem = std::fs::read_to_string(p.trim())
            .map_err(|e| format!("{ENV_KEY_FILE}: read {}: {e}", p.trim()))?;
        return parse_private_key(&pem).map_err(|e| format!("{ENV_KEY_FILE}: {e}"));
    }
    let Some(p) = cfg_path.map(str::trim).filter(|s| !s.is_empty()) else {
        return Err(format!(
            "no decryption key: set {ENV_KEY} (PEM), {ENV_KEY_FILE} (path), \
             or secrets.private_key_path in config"
        ));
    };
    let full = if Path::new(p).is_absolute() {
        Path::new(p).to_path_buf()
    } else {
        config_dir.join(p)
    };
    let pem = std::fs::read_to_string(&full)
        .map_err(|e| format!("secrets.private_key_path: read {}: {e}", full.display()))?;
    parse_private_key(&pem).map_err(|e| format!("secrets.private_key_path {}: {e}", full.display()))
}

/// 从文件读 PEM 公钥（`oj secret seal -k` 用）。
pub fn load_public_key(path: &Path) -> Result<RsaPublicKey, String> {
    let pem = std::fs::read_to_string(path)
        .map_err(|e| format!("read public key {}: {e}", path.display()))?;
    parse_public_key(&pem).map_err(|e| format!("{}: {e}", path.display()))
}

/// PKCS#8 优先，回落 PKCS#1（`openssl genrsa` 的老格式）。
fn parse_private_key(pem: &str) -> Result<RsaPrivateKey, String> {
    RsaPrivateKey::from_pkcs8_pem(pem)
        .or_else(|_| RsaPrivateKey::from_pkcs1_pem(pem))
        .map_err(|e| format!("not a usable RSA private key PEM (want PKCS#8 or PKCS#1): {e}"))
}

/// SPKI（`BEGIN PUBLIC KEY`）优先，回落 PKCS#1（`BEGIN RSA PUBLIC KEY`）。
fn parse_public_key(pem: &str) -> Result<RsaPublicKey, String> {
    RsaPublicKey::from_public_key_pem(pem)
        .or_else(|_| RsaPublicKey::from_pkcs1_pem(pem))
        .map_err(|e| format!("not a usable RSA public key PEM (want SPKI or PKCS#1): {e}"))
}

/// URL/DSN 脱敏：凭据段（`scheme://` 与 `@` 之间）换成 `***`，host/库名保留
/// ——排障要知道连的是哪个库，但不该在日志里看到密码。
///
/// 非 URL 形态（裸密码、token）整串替换；不带 `@` 的 URL 说明里面没有凭据，原样返回
/// （`sqlite://path` 这类路径不是秘密）。
pub fn redact(s: &str) -> String {
    let Some(after_scheme) = s.split_once("://").map(|(_, r)| r) else {
        return "***".into();
    };
    match after_scheme.find('@') {
        Some(at) => format!(
            "{}://***@{}",
            &s[..s.len() - after_scheme.len() - 3],
            &after_scheme[at + 1..]
        ),
        None => s.to_string(),
    }
}

/// `getrandom`（已在依赖里）→ rsa 需要的 `rand_core 0.6` RNG。
///
/// 不引 `rand`：仓库里 `rand 0.10` 是 dev-only 且走 rand_core 0.9，与 rsa 0.9 的
/// rand_core 0.6 不是同一个 trait，接不上。
fn sys_random(dest: &mut [u8]) -> Result<(), rsa::rand_core::Error> {
    getrandom::getrandom(dest).map_err(|_| {
        NonZeroU32::new(rsa::rand_core::Error::CUSTOM_START)
            .unwrap()
            .into()
    })
}

struct SysRng;

impl rsa::rand_core::RngCore for SysRng {
    fn next_u32(&mut self) -> u32 {
        let mut b = [0u8; 4];
        self.fill_bytes(&mut b);
        u32::from_be_bytes(b)
    }
    fn next_u64(&mut self) -> u64 {
        let mut b = [0u8; 8];
        self.fill_bytes(&mut b);
        u64::from_be_bytes(b)
    }
    fn fill_bytes(&mut self, dest: &mut [u8]) {
        sys_random(dest).expect("os random unavailable")
    }
    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rsa::rand_core::Error> {
        sys_random(dest)
    }
}

impl rsa::rand_core::CryptoRng for SysRng {}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试专用密钥对（2048 位够用例跑得快；每用例现生成，不落盘）。
    fn keypair() -> (String, String) {
        keygen(2048).unwrap()
    }

    #[test]
    fn seal_open_roundtrip() {
        let (priv_pem, pub_pem) = keypair();
        let token = seal(&pub_pem, "mysql://root:hunter2@127.0.0.1:3306/app").unwrap();
        assert!(is_sealed(&token), "{token}");
        assert!(!token.contains("hunter2"));
        assert_eq!(
            open(&priv_pem, &token).unwrap(),
            "mysql://root:hunter2@127.0.0.1:3306/app"
        );
    }

    /// 同一明文两次加密结果不同（OAEP 随机化 + 随机 nonce），但都能解回来。
    #[test]
    fn seal_is_randomized() {
        let (priv_pem, pub_pem) = keypair();
        let a = seal(&pub_pem, "same").unwrap();
        let b = seal(&pub_pem, "same").unwrap();
        assert_ne!(a, b);
        assert_eq!(open(&priv_pem, &a).unwrap(), "same");
        assert_eq!(open(&priv_pem, &b).unwrap(), "same");
    }

    /// 信封而非裸 RSA：明文可以远超 190 字节（2048 位 OAEP 的上限）。
    #[test]
    fn envelope_handles_plaintext_longer_than_rsa_limit() {
        let (priv_pem, pub_pem) = keypair();
        let long = "x".repeat(4096);
        assert_eq!(
            open(&priv_pem, &seal(&pub_pem, &long).unwrap()).unwrap(),
            long
        );
    }

    #[test]
    fn wrong_private_key_fails_loud() {
        let (_, pub_pem) = keypair();
        let (other_priv, _) = keypair();
        let token = seal(&pub_pem, "secret").unwrap();
        let e = open(&other_priv, &token).unwrap_err();
        assert!(e.contains("rsa open failed"), "{e}");
    }

    /// 密文改一个字节 → GCM tag 校验失败（不是解出垃圾）。
    #[test]
    fn tampered_ciphertext_fails_tag_check() {
        let (priv_pem, pub_pem) = keypair();
        let token = seal(&pub_pem, "secret").unwrap();
        let body = token
            .strip_prefix(PREFIX)
            .unwrap()
            .strip_suffix(SUFFIX)
            .unwrap();
        let mut env = URL_SAFE_NO_PAD.decode(body).unwrap();
        let last = env.len() - 1;
        env[last] ^= 0x01;
        let bad = format!("{PREFIX}{}{SUFFIX}", URL_SAFE_NO_PAD.encode(env));
        let e = open(&priv_pem, &bad).unwrap_err();
        assert!(e.contains("aes-gcm open failed"), "{e}");
    }

    /// 截断：头部合法但声明的 rsa_len 超过实际 body / 密文少一个字节。
    /// 这两条走的是「长度校验」分支（不是 base64 或版本分支），必须各有覆盖。
    #[test]
    fn truncated_envelope_fails_length_check() {
        let (priv_pem, pub_pem) = keypair();
        let env = decode_env(&seal(&pub_pem, "secret").unwrap());
        // ① 头部说 rsa_len 很大，body 却很短。
        let mut lying = env.clone();
        lying[1..3].copy_from_slice(&u16::MAX.to_be_bytes());
        let e = open(&priv_pem, &encode_env(&lying)).unwrap_err();
        assert!(e.contains("truncated"), "{e}");
        // ② 截到「长度门槛 - 1」→ 长度校验拦下。
        let rsa_len = u16::from_be_bytes([env[1], env[2]]) as usize;
        let cut = HEADER_LEN + rsa_len + NONCE_LEN + TAG_LEN - 1;
        let e = open(&priv_pem, &encode_env(&env[..cut])).unwrap_err();
        assert!(e.contains("truncated"), "{e}");
        // ③ 只留头（3 字节）。
        let e = open(&priv_pem, &encode_env(&env[..HEADER_LEN])).unwrap_err();
        assert!(e.contains("truncated"), "{e}");
        // ④ 尾部少一字节但仍在长度门槛之上 → GCM 校验失败（同样不接受，只是拦在不同层）。
        let e = open(&priv_pem, &encode_env(&env[..env.len() - 1])).unwrap_err();
        assert!(e.contains("aes-gcm open failed"), "{e}");
    }

    /// 键位上的密封值必须报错，不能静默留着密文当名字用（fail-closed）。
    #[test]
    fn sealed_mapping_key_fails_loud() {
        let (priv_pem, pub_pem) = keypair();
        let token = seal(&pub_pem, "x").unwrap();
        let mut v: Value = serde_yaml::from_str(&format!("\"{token}\": 1\n")).unwrap();
        assert!(has_sealed(&v), "键位也要被 has_sealed 判出来");
        let key = parse_private_key(&priv_pem).unwrap();
        let e = decrypt_tree(&mut v, &key).unwrap_err();
        assert!(e.contains("mapping key"), "{e}");
    }

    /// 恰好以 `ENC[` 开头、以 `]` 结尾的**明文**会被当密文硬失败——报错要说清怎么办。
    #[test]
    fn literal_plaintext_that_looks_sealed_gets_actionable_error() {
        let (priv_pem, _) = keypair();
        let e = open(&priv_pem, "ENC[this-is-not-base64!!]").unwrap_err();
        assert!(e.contains("明文"), "{e}");
    }

    fn decode_env(token: &str) -> Vec<u8> {
        let body = token
            .strip_prefix(PREFIX)
            .and_then(|s| s.strip_suffix(SUFFIX))
            .unwrap();
        URL_SAFE_NO_PAD.decode(body).unwrap()
    }

    fn encode_env(env: &[u8]) -> String {
        format!("{PREFIX}{}{SUFFIX}", URL_SAFE_NO_PAD.encode(env))
    }

    #[test]
    fn malformed_tokens_fail_loud() {
        let (priv_pem, _) = keypair();
        for (bad, what) in [
            ("ENC[!!!not-base64!!!]", "bad base64"),
            ("ENC[]", "empty"),
            (
                format!("ENC[{}]", URL_SAFE_NO_PAD.encode([9u8, 0, 0])).as_str(),
                "future version",
            ),
        ] {
            let e = open(&priv_pem, bad).unwrap_err();
            assert!(!e.is_empty(), "{what}");
        }
    }

    #[test]
    fn keygen_rejects_short_keys() {
        assert!(keygen(1024).unwrap_err().contains(">= 2048"));
    }

    /// 递归解密：含不透明段（`ldap` 那种 host 对象里的 `bind_pw`）与嵌套 list。
    #[test]
    fn decrypt_tree_walks_nested_and_opaque_sections() {
        let (priv_pem, pub_pem) = keypair();
        let pw = seal(&pub_pem, "bind-secret").unwrap();
        let dsn = seal(&pub_pem, "postgres://u:p@h:5432/db").unwrap();
        let yaml = format!(
            "db:\n  default: \"{dsn}\"\nldap:\n  default:\n    url: ldap://h:389\n    bind_dn: cn=admin\n    bind_pw: \"{pw}\"\nplugins:\n  oj-auth: {{}}\nanon: [\"{dsn}\"]\nplain: keep-me\n"
        );
        let mut v: Value = serde_yaml::from_str(&yaml).unwrap();
        assert!(has_sealed(&v));
        let key = parse_private_key(&priv_pem).unwrap();
        assert_eq!(decrypt_tree(&mut v, &key).unwrap(), 3);
        assert_eq!(
            v["db"]["default"],
            Value::String("postgres://u:p@h:5432/db".into())
        );
        assert_eq!(
            v["ldap"]["default"]["bind_pw"],
            Value::String("bind-secret".into())
        );
        assert_eq!(
            v["anon"][0],
            Value::String("postgres://u:p@h:5432/db".into())
        );
        assert_eq!(v["plain"], Value::String("keep-me".into()));
        assert!(!has_sealed(&v), "解密后树上不该还剩密封值");
    }

    /// 保序：`remove`+`insert` 会把键挪到末尾，改用整体重建。
    #[test]
    fn decrypt_tree_preserves_mapping_order() {
        let (priv_pem, pub_pem) = keypair();
        let s = seal(&pub_pem, "v").unwrap();
        let mut v: Value = serde_yaml::from_str(&format!("a: \"{s}\"\nb: 1\nc: 2\n")).unwrap();
        let key = parse_private_key(&priv_pem).unwrap();
        decrypt_tree(&mut v, &key).unwrap();
        let keys: Vec<&str> = v
            .as_mapping()
            .unwrap()
            .keys()
            .map(|k| k.as_str().unwrap())
            .collect();
        assert_eq!(keys, vec!["a", "b", "c"]);
    }

    #[test]
    fn no_sealed_values_means_no_key_needed() {
        let v: Value = serde_yaml::from_str("db:\n  default: sqlite://a.db\n").unwrap();
        assert!(!has_sealed(&v));
    }

    #[test]
    fn redact_masks_credentials_but_keeps_host() {
        assert_eq!(
            redact("mysql://root:hunter2@127.0.0.1:3306/app"),
            "mysql://***@127.0.0.1:3306/app"
        );
        assert_eq!(redact("redis://:pwd@h:6379/1"), "redis://***@h:6379/1");
        // 无凭据的 URL 原样（sqlite 路径不是秘密）。
        assert_eq!(redact("sqlite://db.sqlite"), "sqlite://db.sqlite");
        // 非 URL 形态（裸密码/token）整串打码。
        assert_eq!(redact("hunter2"), "***");
    }

    /// 私钥三通道：`OJ_SECRET_KEY`（内联 PEM）> `OJ_SECRET_KEY_FILE`（文件）>
    /// config 段；都没有 → 明确报错（不静默降级）。
    ///
    /// env 是**进程级**状态，而 cargo test 默认多线程跑用例——这里用静态锁把本用例
    /// 串行化，否则会与其它的 env 读写互相污染。
    #[test]
    fn private_key_source_precedence() {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let (priv_pem, _) = keypair();
        let dir = std::env::temp_dir().join(format!("oj-secret-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("k.pem");
        std::fs::write(&file, &priv_pem).unwrap();
        // 无 env、无 config 段 → 报错（先清 env：本用例自己会设，别留脏状态）。
        clear_env(ENV_KEY);
        clear_env(ENV_KEY_FILE);
        let e = load_private_key(None, &dir).unwrap_err();
        assert!(e.contains("OJ_SECRET_KEY"), "{e}");
        // config 段（相对 config_dir）。
        assert!(load_private_key(Some("k.pem"), &dir).is_ok());
        assert!(
            load_private_key(Some("missing.pem"), &dir)
                .unwrap_err()
                .contains("read")
        );

        // ② OJ_SECRET_KEY_FILE 优先于 config 段（指向错文件时报它的名字，便于定位）。
        set_env(ENV_KEY_FILE, &file.display().to_string());
        assert!(load_private_key(Some("missing.pem"), &dir).is_ok());
        set_env(ENV_KEY_FILE, &dir.join("nope.pem").display().to_string());
        let e = load_private_key(Some("k.pem"), &dir).unwrap_err();
        assert!(e.contains(ENV_KEY_FILE), "{e}");

        // ① OJ_SECRET_KEY 内联 PEM 优先级最高（连文件通道的错误都盖掉）。
        set_env(ENV_KEY, &priv_pem);
        assert!(load_private_key(Some("missing.pem"), &dir).is_ok());
        // 内联内容不是 PEM → 报它的名字（不静默回落到文件通道）。
        set_env(ENV_KEY, "not-a-pem");
        let e = load_private_key(Some("k.pem"), &dir).unwrap_err();
        assert!(e.contains(ENV_KEY), "{e}");

        clear_env(ENV_KEY);
        clear_env(ENV_KEY_FILE);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// edition 2024 起 `set_var`/`remove_var` 是 `unsafe`（多线程下与 env 的其它读者
    /// 数据竞争）。本用例已用 `LOCK` 串行化，且是全仓唯一写 env 的用例，故此处封装。
    fn set_env(k: &str, v: &str) {
        unsafe { std::env::set_var(k, v) };
    }

    fn clear_env(k: &str) {
        unsafe { std::env::remove_var(k) };
    }
}
