use mdm_base_rust::config::ServerCfg;
use ring::signature::{UnparsedPublicKey, RSA_PKCS1_2048_8192_SHA256};
use std::{fs, time::{SystemTime, UNIX_EPOCH, Duration}};
use serde_json::Value;
use base64::{engine::general_purpose, Engine};

/// 证书状态
#[derive(Clone, Debug)]
pub enum CertificateStatus {
    /// 证书有效
    Valid,
    /// 证书宽限期内，剩余秒数
    Grace { remaining_secs: u64 },
    /// 证书已过期
    Expired,
}

/// 加载并验证证书
///
/// # Arguments
///
/// * `cfg` - 服务器配置，包含公钥路径、证书路径和宽限期
///
/// # Returns
///
/// * `Ok((CertificateStatus, Option<SystemTime>))` - 证书状态和过期时间
/// * `Err(String)` - 错误信息
pub async fn load_certificate(cfg: &ServerCfg) -> Result<(CertificateStatus, Option<SystemTime>), String> {
    // 1. 读取公钥 PEM
    let key_data = fs::read(&cfg.public_key_path).map_err(|e| format!("read key: {}", e))?;
    let pub_key = load_verification_key(&key_data).map_err(|_| "invalid public key".to_string())?;

    // 2. 读取 JWS 证书字符串
    let cert_str = fs::read_to_string(&cfg.certificate_path).map_err(|e| format!("read cert: {}", e))?;
    let parts: Vec<&str> = cert_str.trim().split('.').collect();
    if parts.len() != 3 { return Err("invalid JWS format".into()); }

    // 3. base64url 解码
    let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(parts[0]).map_err(|e| e.to_string())?;
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(parts[1]).map_err(|e| e.to_string())?;
    let signature = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(parts[2]).map_err(|e| e.to_string())?;

    // 4. 验证签名 (RS256)
    let signing_input = format!("{}.{}", parts[0], parts[1]);
    (&RSA_PKCS1_2048_8192_SHA256).verify(&pub_key, signing_input.as_bytes(), &signature)
        .map_err(|_| "signature verification failed".to_string())?;

    // 5. 解析 payload JSON
    let payload_json: Value = serde_json::from_slice(&payload).map_err(|e| e.to_string())?;
    let nbf = payload_json["nbf"].as_u64().ok_or("missing nbf")?;
    let exp = payload_json["exp"].as_u64().ok_or("missing exp")?;

    // 6. 确定状态
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let grace_secs = cfg.grace_days.unwrap_or(30) as u64 * 86400;
    let status = if now < nbf {
        CertificateStatus::Valid
    } else if now < exp {
        CertificateStatus::Valid
    } else {
        let grace_end = exp + grace_secs;
        if now < grace_end {
            CertificateStatus::Grace { remaining_secs: grace_end - now }
        } else {
            CertificateStatus::Expired
        }
    };
    let expiry_time = UNIX_EPOCH + Duration::from_secs(exp);
    Ok((status, Some(expiry_time)))
}

fn load_verification_key(pem: &[u8]) -> Result<UnparsedPublicKey<Vec<u8>>, ring::error::Unspecified> {
    // 去除 PEM 头尾，base64 解码为 DER 字节
    let pem_str = std::str::from_utf8(pem).map_err(|_| ring::error::Unspecified)?;
    let der_b64: String = pem_str.lines()
        .filter(|l| !l.starts_with("-----"))
        .collect();
    let der = base64::engine::general_purpose::STANDARD.decode(der_b64).map_err(|_| ring::error::Unspecified)?;
    Ok(UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, der))
}