//! oj-cert 核心：JWS/RS256 证书生成与重签。
//!
//! 证书格式与 server/src/certificate.rs 契约一致：header `{"alg":"RS256","typ":"JWT"}`
//! + payload `{nbf, exp}`（Unix 秒）+ RS256 签名；公钥 PEM 为 SPKI（loader 兼容）。

use rsa::RsaPrivateKey;
use rsa::pkcs1v15::SigningKey;
use rsa::pkcs8::{DecodePrivateKey, EncodePrivateKey, EncodePublicKey, LineEnding};
use rsa::rand_core::OsRng;
use rsa::sha2::Sha256;
use rsa::signature::{SignatureEncoding, Signer};
use std::path::{Path, PathBuf};

/// ring `RSA_PKCS1_2048_8192_SHA256` 验签下限。
pub const MIN_BITS: u32 = 2048;

/// 生成参数（gen）。
#[derive(Debug, Clone)]
pub struct GenOpts {
    /// 输出目录（private.pem / public.pem / cert.jws）。
    pub out_dir: PathBuf,
    /// RSA 位数（下限 MIN_BITS）。
    pub bits: u32,
    /// 生效时间（Unix 秒）。
    pub nbf: u64,
    /// 过期时间（Unix 秒，须 > nbf）。
    pub exp: u64,
}

/// 重签参数（renew）。
#[derive(Debug, Clone)]
pub struct RenewOpts {
    /// 现有私钥（PKCS#8 PEM）。
    pub key_path: PathBuf,
    /// cert.jws 输出目录；None = 私钥所在目录。
    pub out_dir: Option<PathBuf>,
    /// 生效时间（Unix 秒）。
    pub nbf: u64,
    /// 过期时间（Unix 秒，须 > nbf）。
    pub exp: u64,
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_secs()
}

/// JWS 三段拼接（b64url no-pad，与 server 加载端 split('.') 对齐）。
fn jws(signing_key: &SigningKey<Sha256>, nbf: u64, exp: u64) -> String {
    use base64::Engine;
    let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
    let header = serde_json::json!({ "alg": "RS256", "typ": "JWT" }).to_string();
    let payload = serde_json::json!({ "nbf": nbf, "exp": exp }).to_string();
    let h = b64(header.as_bytes());
    let p = b64(payload.as_bytes());
    let sig = signing_key.sign(format!("{h}.{p}").as_bytes()).to_vec();
    format!("{h}.{p}.{}", b64(&sig))
}

/// 私钥落盘：PKCS#8 PEM；unix 下 chmod 600。
fn write_private(path: &Path, key: &RsaPrivateKey) -> Result<(), String> {
    let pem = key
        .to_pkcs8_pem(LineEnding::LF)
        .map_err(|e| format!("encode pkcs8: {e}"))?;
    std::fs::write(path, &pem).map_err(|e| format!("write {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("chmod {}: {e}", path.display()))?;
    }
    Ok(())
}

/// gen：生成密钥对 + JWS，写 private.pem / public.pem / cert.jws，返回 cert.jws 路径。
///
/// 注：`gen` 是 edition 2024 保留字，定义须写作 `r#gen`（公开名仍为 `gen`，
/// 调用方以 `oj_cert::r#gen` 引用）。
pub fn r#gen(opts: &GenOpts) -> Result<PathBuf, String> {
    if opts.bits < MIN_BITS {
        return Err(format!("bits must be >= {MIN_BITS}"));
    }
    if opts.exp <= opts.nbf {
        return Err("exp must be greater than nbf".into());
    }
    let key_out = opts.out_dir.join("private.pem");
    if key_out.exists() {
        return Err(format!(
            "refusing to overwrite existing {}: use renew to extend expiry, or move/delete the old keypair first",
            key_out.display()
        ));
    }
    std::fs::create_dir_all(&opts.out_dir)
        .map_err(|e| format!("mkdir {}: {e}", opts.out_dir.display()))?;
    let key =
        RsaPrivateKey::new(&mut OsRng, opts.bits as usize).map_err(|e| format!("keygen: {e}"))?;
    let signing = SigningKey::<Sha256>::new(key.clone());
    let out = |name: &str| opts.out_dir.join(name);
    write_private(&key_out, &key)?;
    let pub_pem = key
        .to_public_key()
        .to_public_key_pem(LineEnding::LF)
        .map_err(|e| format!("encode spki: {e}"))?;
    std::fs::write(out("public.pem"), pub_pem).map_err(|e| format!("write public.pem: {e}"))?;
    std::fs::write(out("cert.jws"), jws(&signing, opts.nbf, opts.exp))
        .map_err(|e| format!("write cert.jws: {e}"))?;
    Ok(out("cert.jws"))
}

/// renew：读现有私钥重签，只写新 cert.jws，返回其路径。
///
/// 公钥不变 → config 不改，配合 server 证书热重载免重启续期。
pub fn renew(opts: &RenewOpts) -> Result<PathBuf, String> {
    if opts.exp <= opts.nbf {
        return Err("exp must be greater than nbf".into());
    }
    let pem = std::fs::read_to_string(&opts.key_path)
        .map_err(|e| format!("read key {}: {e}", opts.key_path.display()))?;
    let key = RsaPrivateKey::from_pkcs8_pem(&pem)
        .map_err(|e| format!("parse private key (PKCS#8 PEM): {e}"))?;
    let dir = match &opts.out_dir {
        Some(d) => d.clone(),
        None => opts
            .key_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default(),
    };
    let path = dir.join("cert.jws");
    std::fs::write(
        &path,
        jws(&SigningKey::<Sha256>::new(key), opts.nbf, opts.exp),
    )
    .map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(path)
}
