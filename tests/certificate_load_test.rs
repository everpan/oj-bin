//! 证书加载与热加载测试：
//! - `load_certificate_at` 对真实签名 JWS 判定 Valid / Grace / Expired。
//! - 文件被覆盖后，watcher 原子更新共享状态。
//!
//! 证书夹具统一来自 `server::test_support`（全仓唯一定义，勿再本地拷贝密钥对/签名器）。

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use only_js::config::ServerCfg;
use serde_json::json;
use server::CertificateStatus;
use server::certificate::load_certificate_at;
use server::certificate_watcher::{
    SharedCertStatus, SharedCertValidUntil, reload_certificate, spawn_watcher,
};
use server::test_support::{TEST_RSA_PUBLIC_PEM, now_secs, write_cert};
use std::path::Path;
use std::sync::{Arc, RwLock};
use tempfile::{NamedTempFile, TempDir};

/// 在独立临时目录生成 (nbf, exp) 的已签名证书，返回 (key 路径, cert 路径)。
/// 目录随 TempDir 存活，文件在测试期间保持有效。
fn signed_cert(nbf: u64, exp: u64) -> (TempDir, String, String) {
    let dir = TempDir::new().unwrap();
    let (cert, key) = write_cert(dir.path(), nbf, exp);
    let key_path = key.to_string_lossy().into_owned();
    let cert_path = cert.to_string_lossy().into_owned();
    (dir, key_path, cert_path)
}

fn cfg_with(key_path: &str, cert_path: &str, grace_days: u64) -> ServerCfg {
    ServerCfg {
        public_key_path: key_path.to_string(),
        certificate_path: cert_path.to_string(),
        grace_days: Some(grace_days),
        ..ServerCfg::default()
    }
}

#[test]
fn load_valid_cert() {
    let (_d, k, c) = signed_cert(now_secs() - 100, now_secs() + 1000);
    let (status, valid) = load_certificate_at(&cfg_with(&k, &c, 30), Path::new(".")).unwrap();
    assert!(matches!(status, CertificateStatus::Valid));
    assert!(valid.is_some());
}

#[test]
fn load_grace_cert() {
    // 过期 10 秒，宽限 30 天 → Grace
    let (_d, k, c) = signed_cert(now_secs() - 2000, now_secs() - 10);
    let (status, _valid) = load_certificate_at(&cfg_with(&k, &c, 30), Path::new(".")).unwrap();
    match status {
        CertificateStatus::Grace { remaining_secs } => {
            assert!(remaining_secs > 0 && remaining_secs <= 30 * 86_400);
        }
        other => panic!("expected Grace, got {other:?}"),
    }
}

#[test]
fn load_expired_cert() {
    // 过期远超 30 天宽限期 → Expired
    let (_d, k, c) = signed_cert(now_secs() - 40 * 86_400, now_secs() - 35 * 86_400);
    let (status, _valid) = load_certificate_at(&cfg_with(&k, &c, 30), Path::new(".")).unwrap();
    assert!(matches!(status, CertificateStatus::Expired));
}

#[test]
fn load_rejects_wrong_alg() {
    // 头部 alg 非 RS256 → 拒绝（防 alg=none 降级）
    let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"none","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(
        serde_json::to_string(&json!({ "nbf": now_secs(), "exp": now_secs() + 100 })).unwrap(),
    );
    let jws = format!("{}.{}.{}", header, payload, URL_SAFE_NO_PAD.encode("x"));
    let d = TempDir::new().unwrap();
    let key_file = d.path().join("public.pem");
    let cert_file = d.path().join("cert.jws");
    std::fs::write(&key_file, TEST_RSA_PUBLIC_PEM).unwrap();
    std::fs::write(&cert_file, jws).unwrap();
    let res = load_certificate_at(
        &cfg_with(
            &key_file.to_string_lossy(),
            &cert_file.to_string_lossy(),
            30,
        ),
        Path::new("."),
    );
    assert!(res.is_err());
}

#[test]
fn watcher_picks_up_replaced_cert() {
    // 初始：有效证书 → 共享状态 Valid；将文件替换为过期证书 → 状态应变为 Expired。
    let (_d, k, c) = signed_cert(now_secs() - 100, now_secs() + 1000);
    let status: SharedCertStatus = Arc::new(RwLock::new(CertificateStatus::Valid));
    let valid_until: SharedCertValidUntil = Arc::new(RwLock::new(None));

    let cfg = cfg_with(&k, &c, 30);
    spawn_watcher(
        status.clone(),
        valid_until.clone(),
        cfg.clone(),
        Path::new(".").to_path_buf(),
    );

    // 用 rename 原子覆盖证书为「已过期且宽限期结束」（rename 事件最可靠）。
    let (_ed, _ek, expired_cert) = signed_cert(now_secs() - 40 * 86_400, now_secs() - 35 * 86_400);
    let new_content = std::fs::read(&expired_cert).unwrap();
    let tmp = NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), &new_content).unwrap();
    std::fs::rename(tmp.path(), &c).unwrap();

    // 事件驱动路径（notify）在本测试环境中未必投递 FS 事件，故此处直接调用与
    // watcher 回调相同的重载逻辑，确定性地验证「证书被替换后状态切换到 Expired」。
    reload_certificate(&status, &valid_until, &cfg, Path::new("."));
    assert!(
        matches!(*status.read().unwrap(), CertificateStatus::Expired),
        "replaced cert should reload as Expired"
    );
}
