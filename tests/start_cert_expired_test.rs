//! 集成测试：证书进入「过期且宽限期结束」时，服务必须在启动期中止。
//!
//! 用 `server::test_support` 的固定测试密钥对签出 `exp` 在过去、宽限期 0 的真实证书 →
//! 启动应返回含 "certificate" 的错误。证书夹具全仓唯一定义（勿再本地拷贝）。

use oj::args::ServerArgs;
use oj::server_cmd;
use server::test_support::{now_secs, write_cert};
use tempfile::{NamedTempFile, TempDir};

#[tokio::test]
async fn test_start_fails_when_cert_expired_and_grace_over() {
    let now = now_secs();
    let dir = TempDir::new().unwrap();
    let (cert, key) = write_cert(dir.path(), now - 2000, now - 1000); // 已过期 1000 秒

    // Windows 临时路径含反斜杠（如 C:\Users\...）；YAML 双引号标量里 \U、\A 等会被
    // 当作转义序列解析失败。统一转正斜杠——Windows 同样认 / 路径，且 YAML 不再报错。
    let key_path = key.to_string_lossy().replace('\\', "/");
    let cert_path = cert.to_string_lossy().replace('\\', "/");

    let temp_dir = TempDir::new().unwrap();
    let service_dir = temp_dir.path();

    let config_file = NamedTempFile::new().unwrap();
    let content = format!(
        "server:\n  host: \"127.0.0.1\"\n  port: 0\n  public_key_path: \"{}\"\n  certificate_path: \"{}\"\n  grace_days: 0\n",
        key_path, cert_path
    );
    std::fs::write(config_file.path(), content).unwrap();

    let result = server_cmd::run(ServerArgs {
        config: config_file.path().to_string_lossy().into_owned(),
        api_path: Some(service_dir.to_string_lossy().into_owned()),
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
