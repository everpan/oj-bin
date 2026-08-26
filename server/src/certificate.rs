//! 证书加载与校验（JWS + RSA 公钥）。
//!
//! 证书为 JWS 三段式 `Base64URL(Header).Base64URL(Payload).Base64URL(Signature)`：
//! - Header：`{"alg":"RS256","typ":"JWT"}`
//! - Payload：至少含 `nbf`（生效）与 `exp`（过期）Unix 秒
//! - Signature：私钥对 `Header.Payload` 的 RS256 签名
//!
//! 未配置证书路径（两者皆空）→ 视为未启用，返回 `Valid`，不做任何限制。

pub use crate::CertificateStatus;
use base64::{Engine, engine::general_purpose};
use only_js::config::ServerCfg;
use ring::signature::{RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::{
    fs,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// 证书功能是否启用：公钥与证书路径任一非空即启用（缺另一半 → 加载时报错，fail fast）。
pub fn is_enabled(cfg: &ServerCfg) -> bool {
    !cfg.public_key_path.trim().is_empty() || !cfg.certificate_path.trim().is_empty()
}

/// 相对路径按 `config_dir` 绝对化（容器/K8s ConfigMap 挂载友好）。
pub fn resolve_path(raw: &str, config_dir: &Path) -> PathBuf {
    let p = Path::new(raw);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        config_dir.join(p)
    }
}

/// 加载并验证证书。
///
/// 返回 `(状态, 证书 exp 时刻)`；未启用证书时返回 `(Valid, None)`。
pub async fn load_certificate(
    cfg: &ServerCfg,
) -> Result<(CertificateStatus, Option<SystemTime>), String> {
    load_certificate_at(cfg, Path::new("."))
}

/// 同 `load_certificate`，但相对路径基于 `config_dir` 解析。
pub fn load_certificate_at(
    cfg: &ServerCfg,
    config_dir: &Path,
) -> Result<(CertificateStatus, Option<SystemTime>), String> {
    if !is_enabled(cfg) {
        return Ok((CertificateStatus::Valid, None));
    }
    if cfg.public_key_path.trim().is_empty() {
        return Err("server.public_key_path is required when certificate_path is set".into());
    }
    if cfg.certificate_path.trim().is_empty() {
        return Err("server.certificate_path is required when public_key_path is set".into());
    }
    let key_path = resolve_path(&cfg.public_key_path, config_dir);
    let cert_path = resolve_path(&cfg.certificate_path, config_dir);

    // 1. 读取公钥 PEM
    let key_data =
        fs::read(&key_path).map_err(|e| format!("read key {}: {e}", key_path.display()))?;
    let pub_key =
        load_verification_key(&key_data).map_err(|e| format!("invalid public key: {e}"))?;

    // 2. 读取 JWS 证书字符串
    let cert_str = fs::read_to_string(&cert_path)
        .map_err(|e| format!("read cert {}: {e}", cert_path.display()))?;
    let parts: Vec<&str> = cert_str.trim().split('.').collect();
    if parts.len() != 3 {
        return Err("invalid JWS format: expected 3 dot-separated parts".into());
    }

    // 3. base64url 解码
    let header = general_purpose::URL_SAFE_NO_PAD
        .decode(parts[0])
        .map_err(|e| format!("decode header: {e}"))?;
    let payload = general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1])
        .map_err(|e| format!("decode payload: {e}"))?;
    let signature = general_purpose::URL_SAFE_NO_PAD
        .decode(parts[2])
        .map_err(|e| format!("decode signature: {e}"))?;

    // 3.1 校验 alg（只接受 RS256，避免 alg=none 降级攻击）
    let header_json: Value =
        serde_json::from_slice(&header).map_err(|e| format!("parse header: {e}"))?;
    if header_json["alg"].as_str() != Some("RS256") {
        return Err("unsupported JWS alg: only RS256 accepted".into());
    }

    // 4. 验证签名（RS256，签名输入为 `header.payload` 的 ASCII 原文）
    let signing_input = format!("{}.{}", parts[0], parts[1]);
    pub_key
        .verify(signing_input.as_bytes(), &signature)
        .map_err(|_| "signature verification failed".to_string())?;

    // 5. 解析 payload
    let payload_json: Value =
        serde_json::from_slice(&payload).map_err(|e| format!("parse payload: {e}"))?;
    let nbf = payload_json["nbf"].as_u64().ok_or("missing nbf")?;
    let exp = payload_json["exp"].as_u64().ok_or("missing exp")?;
    if exp <= nbf {
        return Err("invalid certificate: exp must be greater than nbf".into());
    }

    // 6. 判定状态
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| format!("system clock before unix epoch: {e}"))?
        .as_secs();
    let grace_secs = cfg.grace_days.unwrap_or(30) * 86_400;
    let status = if now < exp {
        // now < nbf（未生效）同样视为可用：待生效后自然进入有效期（spec §4.5）。
        CertificateStatus::Valid
    } else {
        let grace_end = exp + grace_secs;
        if now < grace_end {
            CertificateStatus::Grace {
                remaining_secs: grace_end - now,
            }
        } else {
            CertificateStatus::Expired
        }
    };
    Ok((status, Some(UNIX_EPOCH + Duration::from_secs(exp))))
}

/// PEM（SPKI 或 PKCS#1）→ ring 验证密钥。SPKI 头部会被剥离为裸 RSA PKCS#1 DER。
fn load_verification_key(pem: &[u8]) -> Result<UnparsedPublicKey<Vec<u8>>, String> {
    let pem_str = std::str::from_utf8(pem).map_err(|e| format!("key not utf-8: {e}"))?;
    let der_b64: String = pem_str
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("-----"))
        .collect();
    if der_b64.is_empty() {
        return Err("empty PEM body".into());
    }
    let der = general_purpose::STANDARD
        .decode(der_b64)
        .map_err(|e| format!("base64: {e}"))?;
    // `BEGIN PUBLIC KEY` = SPKI 包装；ring 需要裸 RSAPublicKey，故剥去 AlgorithmIdentifier。
    let der = if pem_str.contains("BEGIN PUBLIC KEY") {
        spki_to_pkcs1(&der)?
    } else {
        der
    };
    Ok(UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, der))
}

/// 最小 DER 解析：SubjectPublicKeyInfo → 内层 RSAPublicKey BIT STRING 内容。
fn spki_to_pkcs1(der: &[u8]) -> Result<Vec<u8>, String> {
    let mut r = DerReader { buf: der, pos: 0 };
    let mut outer = r.expect_seq()?; // SubjectPublicKeyInfo
    let _alg = outer.expect_seq()?; // AlgorithmIdentifier（跳过，alg 已由 JWS header 约束）
    let bits = outer.expect_tag(0x03)?; // BIT STRING
    // BIT STRING 首字节 = 未用 bit 数（RSA 公钥恒为 0）
    match bits.split_first() {
        Some((0, rest)) => Ok(rest.to_vec()),
        _ => Err("unexpected BIT STRING padding in SPKI".into()),
    }
}

/// 极简 DER 游标：只支持本文件所需的 SEQUENCE / BIT STRING 读取。
struct DerReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> DerReader<'a> {
    fn expect_seq(&mut self) -> Result<DerReader<'a>, String> {
        let body = self.expect_tag(0x30)?;
        Ok(DerReader { buf: body, pos: 0 })
    }

    fn expect_tag(&mut self, tag: u8) -> Result<&'a [u8], String> {
        if self.buf.get(self.pos) != Some(&tag) {
            return Err(format!("DER: expected tag 0x{tag:02x}"));
        }
        self.pos += 1;
        let len = self.read_len()?;
        let end = self.pos.checked_add(len).ok_or("DER: length overflow")?;
        let body = self.buf.get(self.pos..end).ok_or("DER: truncated body")?;
        self.pos = end;
        Ok(body)
    }

    fn read_len(&mut self) -> Result<usize, String> {
        let first = *self.buf.get(self.pos).ok_or("DER: missing length")?;
        self.pos += 1;
        if first < 0x80 {
            return Ok(first as usize);
        }
        let n = (first & 0x7f) as usize;
        if n == 0 || n > 4 {
            return Err("DER: unsupported length encoding".into());
        }
        let bytes = self
            .buf
            .get(self.pos..self.pos + n)
            .ok_or("DER: truncated length")?;
        self.pos += n;
        Ok(bytes.iter().fold(0usize, |acc, b| (acc << 8) | *b as usize))
    }
}
