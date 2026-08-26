//! 集成测试：证书进入「过期且宽限期结束」时，服务必须在启动期中止。
//!
//! 用固定测试密钥对（cert_fixture）对 JWS 正确签名，`exp` 设为过去、宽限期 0 →
//! 启动应返回含 "certificate" 的错误。

mod cert_fixture;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use cert_fixture::{TEST_RSA_PKCS8_B64, TEST_RSA_PUBLIC_PEM};
use oj::args::ServerArgs;
use oj::server_cmd;
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use serde_json::json;
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::NamedTempFile;

/// 写出「已过期、宽限期 0」的真实签名 JWS 证书 + 对应公钥。
/// 返回临时文件句柄（须保持存活）与对应路径。
fn expired_cert_files() -> (NamedTempFile, NamedTempFile, String, String) {
    let pkcs8 = base64::engine::general_purpose::STANDARD
        .decode(TEST_RSA_PKCS8_B64)
        .unwrap();
    let key_pair = RsaKeyPair::from_pkcs8(&pkcs8).expect("load test key");
    let rng = SystemRandom::new();

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let nbf = now - 2000;
    let exp = now - 1000; // 已过期 1000 秒，grace_days=0 → 宽限期结束

    let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"RS256","typ":"JWT"}"#);
    let payload =
        URL_SAFE_NO_PAD.encode(serde_json::to_string(&json!({ "nbf": nbf, "exp": exp })).unwrap());
    let signing_input = format!("{}.{}", header, payload);
    let mut sig = vec![0u8; key_pair.public().modulus_len()];
    key_pair
        .sign(&RSA_PKCS1_SHA256, &rng, signing_input.as_bytes(), &mut sig)
        .expect("sign cert");
    let jws = format!("{}.{}.{}", header, payload, URL_SAFE_NO_PAD.encode(&sig));

    let key_file = NamedTempFile::new().unwrap();
    let cert_file = NamedTempFile::new().unwrap();
    std::fs::write(key_file.path(), TEST_RSA_PUBLIC_PEM).unwrap();
    std::fs::write(cert_file.path(), jws).unwrap();

    let key_path = key_file.path().to_string_lossy().into_owned();
    let cert_path = cert_file.path().to_string_lossy().into_owned();
    (key_file, cert_file, key_path, cert_path)
}

#[tokio::test]
async fn test_start_fails_when_cert_expired_and_grace_over() {
    let (_key_file, _cert_file, key_path, cert_path) = expired_cert_files();

    let temp_dir = tempfile::TempDir::new().unwrap();
    let service_dir = temp_dir.path();

    let config_file = NamedTempFile::new().unwrap();
    let content = format!(
        "server:\n  host: \"127.0.0.1\"\n  port: 0\n  public_key_path: \"{}\"\n  certificate_path: \"{}\"\n  grace_days: 0\n",
        key_path, cert_path
    );
    std::fs::write(config_file.path(), content).unwrap();

    let result = server_cmd::run(ServerArgs {
        config: config_file.path().to_string_lossy().into_owned(),
        dir: Some(service_dir.to_string_lossy().into_owned()),
        ..Default::default()
    })
    .await;

    assert!(
        result.is_err(),
        "server must refuse to start when cert expired"
    );
    let msg = result.unwrap_err();
    assert!(
        msg.contains("certificate"),
        "expected certificate error, got: {msg}"
    );
}
