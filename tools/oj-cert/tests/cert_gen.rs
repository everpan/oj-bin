//! 集成测试：gen/renew 产物用 rsa 独立解码 + 验签（与生成路径不对称，可捕格式错误）。

use base64::Engine;
use oj_cert::{GenOpts, MIN_BITS, RenewOpts, r#gen, renew};
use rsa::RsaPublicKey;
use rsa::pkcs1v15::{Signature, VerifyingKey};
use rsa::pkcs8::DecodePublicKey;
use rsa::sha2::Sha256;
use rsa::signature::Verifier;
use std::path::PathBuf;

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("oj-cert-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// 独立验签：解析 SPKI PEM + JWS 三段，重放 RS256 verify。
fn verify_jws(public_pem: &str, jws: &str, nbf: u64, exp: u64) -> Result<(), String> {
    let parts: Vec<&str> = jws.trim().split('.').collect();
    assert_eq!(parts.len(), 3, "jws must have 3 parts");
    let dec = |s: &str| {
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(s)
            .unwrap()
    };
    let header: serde_json::Value =
        serde_json::from_slice(&dec(parts[0])).map_err(|e| e.to_string())?;
    assert_eq!(header["alg"], "RS256");
    assert_eq!(header["typ"], "JWT");
    let payload: serde_json::Value =
        serde_json::from_slice(&dec(parts[1])).map_err(|e| e.to_string())?;
    assert_eq!(payload["nbf"], nbf);
    assert_eq!(payload["exp"], exp);
    let pub_key = RsaPublicKey::from_public_key_pem(public_pem).map_err(|e| e.to_string())?;
    let sig = Signature::try_from(dec(parts[2]).as_slice()).map_err(|e| e.to_string())?;
    VerifyingKey::<Sha256>::new(pub_key)
        .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig)
        .map_err(|_| "signature verify failed".to_string())
}

#[test]
fn gen_writes_and_verifies() {
    let dir = tmpdir("gen");
    let (nbf, exp) = (1_000_000_u64, 2_000_000_u64);
    r#gen(&GenOpts {
        out_dir: dir.clone(),
        bits: MIN_BITS,
        nbf,
        exp,
    })
    .unwrap();
    let jws = std::fs::read_to_string(dir.join("cert.jws")).unwrap();
    let pub_pem = std::fs::read_to_string(dir.join("public.pem")).unwrap();
    let priv_pem = std::fs::read_to_string(dir.join("private.pem")).unwrap();
    assert!(
        priv_pem.starts_with("-----BEGIN PRIVATE KEY-----"),
        "{priv_pem}"
    );
    assert!(
        pub_pem.starts_with("-----BEGIN PUBLIC KEY-----"),
        "{pub_pem}"
    );
    verify_jws(&pub_pem, &jws, nbf, exp).unwrap();
    // unix 下私钥落盘即 600
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.join("private.pem"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rejects_bad_params() {
    let dir = tmpdir("bad");
    let e = r#gen(&GenOpts {
        out_dir: dir.clone(),
        bits: MIN_BITS,
        nbf: 100,
        exp: 100,
    })
    .unwrap_err();
    assert!(e.contains("exp must be greater"), "{e}");
    let e = r#gen(&GenOpts {
        out_dir: dir.clone(),
        bits: 1024,
        nbf: 1,
        exp: 2,
    })
    .unwrap_err();
    assert!(e.contains("bits must be"), "{e}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn gen_refuses_to_overwrite_existing_private_key() {
    let dir = tmpdir("gen-guard");
    let opts = GenOpts {
        out_dir: dir.clone(),
        bits: MIN_BITS,
        nbf: 1,
        exp: 2,
    };
    r#gen(&opts).unwrap();
    let original = std::fs::read_to_string(dir.join("private.pem")).unwrap();
    let e = r#gen(&opts).unwrap_err();
    assert!(e.contains("refusing to overwrite"), "{e}");
    assert_eq!(
        std::fs::read_to_string(dir.join("private.pem")).unwrap(),
        original
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn renews_with_moved_exp_and_same_key() {
    let dir = tmpdir("renew");
    r#gen(&GenOpts {
        out_dir: dir.clone(),
        bits: MIN_BITS,
        nbf: 1_000_000,
        exp: 1_000_100,
    })
    .unwrap();
    let new_exp = 3_000_000_u64;
    let out = renew(&RenewOpts {
        key_path: dir.join("private.pem"),
        out_dir: Some(dir.clone()),
        nbf: 1_000_000,
        exp: new_exp,
    })
    .unwrap();
    assert_eq!(out, dir.join("cert.jws"));
    let pub_pem = std::fs::read_to_string(dir.join("public.pem")).unwrap();
    verify_jws(
        &pub_pem,
        &std::fs::read_to_string(&out).unwrap(),
        1_000_000,
        new_exp,
    )
    .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn renew_rejects_bad_exp_and_missing_key() {
    let dir = tmpdir("renew-bad");
    let e = renew(&RenewOpts {
        key_path: dir.join("no.pem"),
        out_dir: None,
        nbf: 100,
        exp: 100,
    })
    .unwrap_err();
    assert!(e.contains("exp must be greater"), "{e}");
    let e = renew(&RenewOpts {
        key_path: dir.join("no.pem"),
        out_dir: None,
        nbf: 100,
        exp: 200,
    })
    .unwrap_err();
    assert!(e.contains("read key"), "{e}");
    let _ = std::fs::remove_dir_all(&dir);
}
