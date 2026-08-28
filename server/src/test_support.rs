//! 测试支撑：证书必配（无逃生口）后，测试需自带真实签名的 JWS 证书。
//!
//! 用固定测试密钥对（`tests/cert_fixture.rs` 同源）对 `{nbf,exp}` 生成 RS256 JWS
//! 并落盘，返回临时文件句柄（保持存活）与路径。供 `oj`（server_cmd/app/e2e）与
//! `server` 自身测试装配证书使用。生成路径与 `server/src/certificate.rs` 契约一致。

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use serde_json::json;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// 测试私钥（PKCS#8 DER，base64）——与根 crate `tests/cert_fixture.rs` 同源。
pub const TEST_RSA_PKCS8_B64: &str = "MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQDT8ZDGpkXI6dx55tXPtlfTOqKHCu2w3vs9j6XvhGIKacp+qX0uQp1FqBgU8OqRXmYJTKAE7weRykCZToBoJhlO5Dlang1/JtGc7pAu/xQHfNpb+U16hbIbu1JUkC6J9b3jSb/gVS1DNjviPQXkiVGR1sUunBgMTRva/7h2RC8fZ01YzcEukE70flv+7tFzaSY1zwH6zynOdzoRrJDbQItqqbyw9tDxcjIE2lZUjJi9haFNEL+rAqBqomdlWajpZ8kWudhuHoWFA0S0aq2MdgYa30uZsqzUE9yWdGTBzlD0XLlBBJ4u+Dxm9d5v2VexhRpWsst1sCZ6zKrNix3uwb6DAgMBAAECggEAVtxatD8yvHuzwzXqjL0zUztlnqjI70MDfqBfpkEAGTpwJeb6ibn9UK3qaLKvv7ILaWZA8qSv2n0kanA0yfpLRvzb0JqT93eGUqWm68vYfpUZvLX4ne0rKJhlzohkul+/WeZAwATIjxIsCrVts9LfXkDCAS8x3+C+OMuy4q1hDqH9r/q5Yaqjd9xV/BGIkdz4GNNXt3qeOwufXb64ukdbsCdt7f11wTMim9rvQqsz1IbPHhDx6Jy+o/FKr//z3JvQswH2Xv/WsntvIGKRoo0yx4brkL6dku2lx+37wCK8iYal1qQe4WEhvrCN7qbFmeG5tNz6neIE/t0wBVIKumCsYQKBgQDp+r3gwDftIjiXKSDblyBrYzplNAerwJnwiYhiVkujSifMqU8rr2NaH88nQPlnX3Ari4UhmfI6bhLbgyZ6OyQDGeKZa4aEQiM8dtSPui05GsTlW9poPmHdBNZNUkkI2wW3qaaw8gmHU9YDKHUSyP89vJdLXrg7ICQkhBc/ltLsEQKBgQDn4+pA3VO15E660GBV2JJBF2HpFd3OzmDLqwZfez5V+rcLg5h5ZgBbjz9nj/3XxEp56bDAX7WyUNsVzE9givutEKtGW+2giPUK9W2bxCuQtVLhdwCD7w7paaVM23A67w61SnM27c3JAZpnTPODEw7KqwdFPEvaTceiu54Nq/TlUwKBgQCZABy/5hHsH8+PkRZqYYWSk11xJjfJ6PUA5H5ph3KIgYpK+3/I2jSGj3xfd85e+XqZDu/sjAVojegI4Nb9YMTovjl+B2D8BV+TP0U6Aw1lZQrRzGGifwBxjaMxBpi5kLdJZUeaN3thocG1aPQ9Z2/4h+ULJRIln5viwPmO3GpqcQKBgDEBb44JuBkmiKTeSJ2byTzMTjrODjQYVUh1ekFPcFsHQwvB4cU2EzlGSqX+Pi0NJJgjFOFy2Jk4kTRIGzZR6OIoNaoG328fwnlwaJuUl4hbaYqQdaFsMgCN/QsDDPLHdppFg5fGJckm95SBJK08p9GY106AcZ9O9LOlZr+I6ZZVAoGAGFdJ6JvCaJU6bL+moTaksuC5Fsxxt8brMK7mMQM5eUFVJUgDJJ2ETd+cUBj3e4u4e65pDwuqtjoBfWweCjWkdtn3gSzbLcYHbTRWyYUwfNTMzKDMtf9QpfaDSQxHJyS2Kx2wFgVjyMdcgM6xq9rQNOKHfy7Lj/inrwqAoVABBjs=";

/// 上述私钥对应的 SPKI PEM 公钥（loader 验签用）。
pub const TEST_RSA_PUBLIC_PEM: &str = "-----BEGIN PUBLIC KEY-----\nMIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEA0/GQxqZFyOnceebVz7ZX\n0zqihwrtsN77PY+l74RiCmnKfql9LkKdRagYFPDqkV5mCUygBO8HkcpAmU6AaCYZ\nTuQ5Wp4NfybRnO6QLv8UB3zaW/lNeoWyG7tSVJAuifW940m/4FUtQzY74j0F5IlR\nkdbFLpwYDE0b2v+4dkQvH2dNWM3BLpBO9H5b/u7Rc2kmNc8B+s8pznc6EayQ20CL\naqm8sPbQ8XIyBNpWVIyYvYWhTRC/qwKgaqJnZVmo6WfJFrnYbh6FhQNEtGqtjHYG\nGt9LmbKs1BPclnRkwc5Q9Fy5QQSeLvg8ZvXeb9lXsYUaVrLLdbAmesyqzYsd7sG+\ngwIDAQAB\n-----END PUBLIC KEY-----\n";

/// 当前 Unix 秒。
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_secs()
}

/// 在 `dir` 下写出 `{nbf,exp}` 的真实签名 JWS `cert.jws` 与 SPKI PEM `public.pem`。
/// 返回 (cert 路径, key 路径)；证书有效期为 [nbf, exp)。
pub fn write_cert(dir: &Path, nbf: u64, exp: u64) -> (std::path::PathBuf, std::path::PathBuf) {
    let pkcs8 = base64::engine::general_purpose::STANDARD
        .decode(TEST_RSA_PKCS8_B64)
        .expect("decode test key");
    let key_pair = RsaKeyPair::from_pkcs8(&pkcs8).expect("load test key");
    let rng = SystemRandom::new();

    let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"RS256","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(json!({ "nbf": nbf, "exp": exp }).to_string());
    let signing_input = format!("{header}.{payload}");
    let mut sig = vec![0u8; key_pair.public().modulus_len()];
    key_pair
        .sign(&RSA_PKCS1_SHA256, &rng, signing_input.as_bytes(), &mut sig)
        .expect("sign cert");
    let jws = format!("{header}.{payload}.{}", URL_SAFE_NO_PAD.encode(&sig));

    let cert = dir.join("cert.jws");
    let key = dir.join("public.pem");
    std::fs::write(&cert, jws).expect("write cert.jws");
    std::fs::write(&key, TEST_RSA_PUBLIC_PEM).expect("write public.pem");
    (cert, key)
}
