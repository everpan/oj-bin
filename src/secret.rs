//! 配置凭据密封（sealed secret）：config.yaml 里的敏感值以 `ENC[<base64url>]` 承载，
//! 装配期用部署机私钥就地解密——**配置文件可以进 git，私钥不可以**。
//!
//! 信封采用 **X25519 + AES-256-GCM**：RSA（v1）已移除——`oj secret keygen` 只生成 X25519
//! 密钥，密文固定开销仅 ~62B（版本 1 + 算法 1 + 临时公钥 32 + nonce 12 + tag 16），长度随
//! 明文，短密码不再被 RSA 地板撑大。明文由 AES-256-GCM 加密，长度无上限（与 v1 信封同构）。
//! 存量 v1（RSA）密文会在解密时明确报错，提示用 `oj secret seal` 以 X25519 重新加密。
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

use std::path::Path;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hkdf::Hkdf;
use serde_yaml::Value;
use sha2::Sha256;
use x25519_dalek::{PublicKey as X25519Public, StaticSecret};

/// 密封值标记：YAML 里写作 `db: { default: "ENC[<base64url>]" }`。
pub const PREFIX: &str = "ENC[";
pub const SUFFIX: &str = "]";

/// 私钥来源环境变量（PEM 内联）——优先级最高。
pub const ENV_KEY: &str = "OJ_SECRET_KEY";
/// 私钥来源环境变量（PEM 文件路径）。
pub const ENV_KEY_FILE: &str = "OJ_SECRET_KEY_FILE";

/// 已移除的 v1 信封版本号（RSA-OAEP）。仅保留用于识别存量 v1 密文并报清晰错误，不再产生。
const VERSION_RSA: u8 = 1;
/// 信封版本（当前唯一：X25519 + AES-256-GCM）。布局变更即 bump，旧密文仍能按版本识别。
const VERSION_EC: u8 = 2;
/// 算法标签：1 = X25519 + AES-256-GCM（预留前向兼容别的曲线/算法）。
const ALG_X25519_AESGCM: u8 = 1;
/// X25519 公钥字节数（u 坐标）。
const X25519_PK_LEN: usize = 32;
/// GCM nonce 长度。
const NONCE_LEN: usize = 12;
/// GCM tag 长度（密文至少这么长才算没被截断）。
const TAG_LEN: usize = 16;

/// 私钥：`ENC[...]` 现在是 X25519 信封，只需这一种密钥（32 字节种子）。
#[derive(Debug, Clone)]
pub struct PrivKey(pub [u8; 32]);

/// 公钥：`PrivKey` 的公开侧（32 字节 u 坐标）。
#[derive(Debug, Clone)]
pub struct PubKey(pub [u8; 32]);

/// 本项目 X25519 密钥的 PEM 标签（与 RSA 的 `BEGIN PRIVATE KEY` 等区分）。
const EC_PRIV_LABEL: &str = "OJ X25519 PRIVATE KEY";
const EC_PUB_LABEL: &str = "OJ X25519 PUBLIC KEY";

/// 判断一个字符串是否是密封值（不解密、不需要密钥）。
pub fn is_sealed(s: &str) -> bool {
    s.starts_with(PREFIX) && s.ends_with(SUFFIX)
}

/// 用公钥把明文封成 `ENC[...]`（X25519 信封）。
pub fn seal(public_key_pem: &str, plaintext: &str) -> Result<String, String> {
    seal_pub(&parse_public_key(public_key_pem)?, plaintext)
}

/// 已解析公钥的加密入口（`oj secret seal` 复用同一个 key）。
pub fn seal_pub(key: &PubKey, plaintext: &str) -> Result<String, String> {
    seal_ec_pub(&key.0, plaintext)
}

/// X25519 信封（X25519 ECDH + HKDF-SHA256 派生 CEK + AES-256-GCM）。
///
/// 临时密钥每次随机生成，故固定开销仅 `1(版本)+1(alg)+32(临时公钥)+12(nonce)+16(tag)`
/// ≈ 62 字节——密文长度≈明文+62B，短密码不再被 RSA 的 256/512B 地板撑大。
fn seal_ec_pub(pk: &[u8; 32], plaintext: &str) -> Result<String, String> {
    let mut eph_seed = [0u8; 32];
    sys_random(&mut eph_seed)?;
    let eph = StaticSecret::from(eph_seed);
    let eph_pk = X25519Public::from(&eph);
    // ECIES：封装方用**临时私钥 × 收件方公钥**做 ECDH（pk 是公钥，不能当 StaticSecret 种子用）。
    let shared = eph.diffie_hellman(&X25519Public::from(*pk));
    let cek = derive_cek(shared.as_bytes())?;

    let mut nonce = [0u8; NONCE_LEN];
    sys_random(&mut nonce)?;
    let ct = Aes256Gcm::new_from_slice(&cek)
        .map_err(|e| format!("aes key failed: {e}"))?
        .encrypt(Nonce::from_slice(&nonce), plaintext.as_bytes())
        .map_err(|e| format!("aes-gcm seal failed: {e}"))?;

    let mut env = Vec::with_capacity(2 + X25519_PK_LEN + NONCE_LEN + ct.len());
    env.push(VERSION_EC);
    env.push(ALG_X25519_AESGCM);
    env.extend_from_slice(eph_pk.as_bytes());
    env.extend_from_slice(&nonce);
    env.extend_from_slice(&ct);
    Ok(format!("{PREFIX}{}{SUFFIX}", URL_SAFE_NO_PAD.encode(env)))
}

/// X25519 共享秘密 → 32 字节 AES-256-GCM 内容密钥（HKDF-SHA256，固定 info 串）。
fn derive_cek(shared: &[u8]) -> Result<[u8; 32], String> {
    let hk = Hkdf::<Sha256>::new(None, shared);
    let mut cek = [0u8; 32];
    hk.expand(b"oj-sealed-v2", &mut cek)
        .map_err(|_| "hkdf expand failed".to_string())?;
    Ok(cek)
}

/// 用私钥解出 `ENC[...]` 的明文（也接受裸 base64url，便于排障时粘贴）。
pub fn open(private_key_pem: &str, token: &str) -> Result<String, String> {
    open_pem(&parse_private_key(private_key_pem)?, token)
}

/// 已解析私钥的解密入口（树遍历复用同一个 key，避免每个值都重解析 PEM）。
/// 按密文版本字节派发：当前仅 X25519 信封；版本 1(RSA, 已移除) 或未知版本明确报错。
pub fn open_pem(key: &PrivKey, token: &str) -> Result<String, String> {
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
    if env.is_empty() {
        return Err("sealed value empty".into());
    }
    match env[0] {
        VERSION_EC => open_ec_pem(key, &env),
        // 已移除的 v1(RSA) 密文：明确报错，提示用 X25519 重新加密（不静默降级）。
        VERSION_RSA => Err(
            "sealed value is v1 (RSA) — v1 信封已移除；请用 `oj secret seal` 以 X25519 重新加密 \
             （RSA(v1) 密钥不再被本版本支持）"
                .into(),
        ),
        other => Err(format!(
            "sealed value version {other} unsupported (this oj only knows X25519 envelope) — \
             用 `oj secret seal` 重新加密（别手改密文）"
        )),
    }
}

/// X25519 解密（X25519 ECDH → HKDF 派生 CEK → AES-256-GCM 解正文）。
fn open_ec_pem(key: &PrivKey, env: &[u8]) -> Result<String, String> {
    if env.len() < 2 + X25519_PK_LEN + NONCE_LEN + TAG_LEN {
        return Err(format!(
            "sealed value truncated: {} bytes (needs at least {} bytes)",
            env.len(),
            2 + X25519_PK_LEN + NONCE_LEN + TAG_LEN
        ));
    }
    if env[1] != ALG_X25519_AESGCM {
        return Err(format!(
            "sealed value alg {} unsupported (this oj knows X25519+AES-GCM = {ALG_X25519_AESGCM})",
            env[1]
        ));
    }
    let (pk_bytes, rest) = env[2..].split_at(X25519_PK_LEN);
    let (nonce, ct) = rest.split_at(NONCE_LEN);
    let eph_pk = X25519Public::from(<[u8; 32]>::try_from(pk_bytes).unwrap());
    let shared = StaticSecret::from(key.0).diffie_hellman(&eph_pk);
    let cek = derive_cek(shared.as_bytes())?;
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
pub fn decrypt_tree(v: &mut Value, key: &PrivKey) -> Result<usize, String> {
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

/// 生成 X25519 密钥对（本项目 `BEGIN OJ X25519 …` 标签的 PEM）。
/// 固定 32 字节，无需 `--bits`；公钥 32 字节 u 坐标、私钥 32 字节种子。
/// `ENC[...]` 的密文长度≈明文+62B（短密码不再被 RSA 地板撑大）。
pub fn keygen() -> Result<(String, String), String> {
    let mut seed = [0u8; 32];
    sys_random(&mut seed)?;
    let priv_pem = format!(
        "-----BEGIN {EC_PRIV_LABEL}-----\n{}\n-----END {EC_PRIV_LABEL}-----\n",
        URL_SAFE_NO_PAD.encode(seed)
    );
    let pub_bytes: [u8; 32] = *X25519Public::from(&StaticSecret::from(seed)).as_bytes();
    let pub_pem = format!(
        "-----BEGIN {EC_PUB_LABEL}-----\n{}\n-----END {EC_PUB_LABEL}-----\n",
        URL_SAFE_NO_PAD.encode(pub_bytes)
    );
    Ok((priv_pem, pub_pem))
}

/// 私钥加载：env 内联 PEM > env 文件 > config 的 `secrets.private_key_path`
/// （相对 config 目录）。三处都没有 → 明确报错（绝不静默把密文当明文用）。
pub fn load_private_key(
    cfg_path: Option<&str>,
    config_dir: &Path,
) -> Result<PrivKey, String> {
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
pub fn load_public_key(path: &Path) -> Result<PubKey, String> {
    let pem = std::fs::read_to_string(path)
        .map_err(|e| format!("read public key {}: {e}", path.display()))?;
    parse_public_key(&pem).map_err(|e| format!("{}: {e}", path.display()))
}

/// 从 PEM 取出 base64 体（剥离 BEGIN/END 标签与空白），优先 URL_SAFE_NO_PAD，回落标准 base64。
fn pem_body(pem: &str, label: &str) -> Result<Vec<u8>, String> {
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let lines: Vec<&str> = pem.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    if lines.first() != Some(&begin.as_str()) || lines.last() != Some(&end.as_str()) {
        return Err(format!("not a {label} PEM"));
    }
    let b64: String = lines[1..lines.len() - 1].concat();
    URL_SAFE_NO_PAD
        .decode(&b64)
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(&b64))
        .map_err(|e| format!("{label}: invalid base64: {e}"))
}

/// 解析私钥 PEM：只接受本项目 `BEGIN OJ X25519 …` 标签（RSA(v1) 已移除）。
pub fn parse_private_key(pem: &str) -> Result<PrivKey, String> {
    if pem.contains(EC_PRIV_LABEL) {
        let body = pem_body(pem, EC_PRIV_LABEL)?;
        if body.len() != 32 {
            return Err(format!("{EC_PRIV_LABEL}: expected 32 bytes, got {}", body.len()));
        }
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&body);
        return Ok(PrivKey(seed));
    }
    Err(
        "secrets 现在只支持 X25519 私钥（BEGIN OJ X25519 PRIVATE KEY）；RSA(v1) 已移除，\
         请用 `oj secret keygen` 重新生成密钥对"
            .into(),
    )
}

/// 解析公钥 PEM：只接受本项目 `BEGIN OJ X25519 …` 标签（RSA(v1) 已移除）。
pub fn parse_public_key(pem: &str) -> Result<PubKey, String> {
    if pem.contains(EC_PUB_LABEL) {
        let body = pem_body(pem, EC_PUB_LABEL)?;
        if body.len() != 32 {
            return Err(format!("{EC_PUB_LABEL}: expected 32 bytes, got {}", body.len()));
        }
        let mut pk = [0u8; 32];
        pk.copy_from_slice(&body);
        return Ok(PubKey(pk));
    }
    Err(
        "secrets 现在只支持 X25519 公钥（BEGIN OJ X25519 PUBLIC KEY）；RSA(v1) 已移除，\
         请用 `oj secret keygen` 重新生成密钥对"
            .into(),
    )
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

/// 系统随机数：`getrandom`（已在依赖里）填充缓冲区。用于 X25519 密钥种子与 GCM nonce。
fn sys_random(dest: &mut [u8]) -> Result<(), String> {
    getrandom::getrandom(dest).map_err(|e| format!("os random unavailable: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试专用密钥对（X25519；每用例现生成，不落盘）。
    fn keypair() -> (String, String) {
        keygen().unwrap()
    }

    #[test]
    fn seal_open_roundtrip() {
        let (priv_pem, pub_pem) = keypair();
        // 短明文（8 字）验证「密文随明文、固定开销 ~62B」：应远短于 RSA(v1) 的 ~735 字符。
        let short = seal(&pub_pem, "hunter2").unwrap();
        assert!(is_sealed(&short), "{short}");
        assert!(short.len() < 130, "short token unexpectedly long: {short}");
        assert!(!short.contains("hunter2"));
        assert_eq!(open(&priv_pem, &short).unwrap(), "hunter2");
        // 长 DSN 也能正常往返（明文长度无上限）。
        let dsn = "mysql://root:hunter2@127.0.0.1:3306/app";
        let token = seal(&pub_pem, dsn).unwrap();
        assert_eq!(open(&priv_pem, &token).unwrap(), dsn);
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

    /// 信封而非裸 RSA：明文可以远超 190 字节——X25519 只派生会话密钥，正文由
    /// AES-256-GCM 加密，长度无上限（与 v1 信封同构）。
    #[test]
    fn envelope_handles_long_plaintext() {
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
        assert!(e.contains("aes-gcm open failed"), "{e}");
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

    /// 截断：长度校验分支（不是 base64 或版本分支）必须覆盖。
    #[test]
    fn truncated_envelope_fails_length_check() {
        let (priv_pem, pub_pem) = keypair();
        let env = decode_env(&seal(&pub_pem, "secret").unwrap());
        let min = 2 + X25519_PK_LEN + NONCE_LEN + TAG_LEN;
        // ① 截到「长度门槛 - 1」→ 长度校验拦下。
        let e = open(&priv_pem, &encode_env(&env[..min - 1])).unwrap_err();
        assert!(e.contains("truncated"), "{e}");
        // ② 只留头（版本 + 算法 2 字节）→ 长度校验拦下。
        let e = open(&priv_pem, &encode_env(&env[..2])).unwrap_err();
        assert!(e.contains("truncated"), "{e}");
        // ③ 尾部少一字节但仍在长度门槛之上 → GCM 校验失败（同样不接受，只是拦在不同层）。
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

    // ===== X25519 信封：密文长度随明文（v1/RSA 已移除）=====

    #[test]
    fn keygen_produces_parseable_pem() {
        let (priv_pem, pub_pem) = keypair();
        assert!(priv_pem.contains("OJ X25519 PRIVATE KEY"));
        assert!(pub_pem.contains("OJ X25519 PUBLIC KEY"));
        let pt = "x".repeat(4096);
        let token = seal(&pub_pem, &pt).unwrap();
        assert_eq!(open(&priv_pem, &token).unwrap(), pt);
    }

    /// 存量 v1（RSA）密文在移除后必须明确报错，提示用 X25519 重新加密——不静默降级。
    #[test]
    fn legacy_v1_token_rejected_with_clear_error() {
        let (priv_pem, _) = keypair();
        // 手工拼一个 version=1 的密文（v1 已移除）。
        let v1_env = vec![VERSION_RSA, ALG_X25519_AESGCM, 0u8, 0u8, 0u8];
        let token = encode_env(&v1_env);
        let e = open(&priv_pem, &token).unwrap_err();
        assert!(e.contains("v1 (RSA)"), "{e}");
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
