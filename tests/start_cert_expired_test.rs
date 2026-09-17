//! 集成测试：证书进入「过期且宽限期结束」时，服务必须在启动期中止。
//!
//! 用 `server::test_support` 的固定测试密钥对签出 `exp` 在过去、宽限期 0 的真实证书 →
//! 启动应返回含 "certificate" 的错误。证书夹具全仓唯一定义（勿再本地拷贝）。

use oj::args::ServerArgs;
use oj::server_cmd;
use server::test_support::{now_secs, write_cert};
use tempfile::{NamedTempFile, TempDir};

/// 与 `src/bridge/ffi.rs::triple()` 一致——`<plugins_dir>/<triple>/` 才是扫描目录。
/// 测试须把插件扫描隔离到空目录，否则 workspace 自带（或 CI 检出）的 bin/plugins 里
/// 若含 ABI 不符的陈旧产物，会在证书校验前先撞「plugin ABI mismatch」而偏离本测目标。
fn host_plugin_triple() -> String {
    let arch = std::env::consts::ARCH;
    match std::env::consts::OS {
        "macos" => format!("{arch}-apple-darwin"),
        "windows" => format!("{arch}-pc-windows-msvc"),
        "linux" => format!("{arch}-unknown-linux-gnu"),
        other => format!("{arch}-unknown-{other}-gnu"),
    }
}

// 证书过期且宽限结束 → 启动期硬断言中止（见 oj/src/app.rs::load_cert_with_watcher），
// 故此处用 should_panic 而非 is_err：panic 载荷含 "certificate" 即命中目标错误。
#[tokio::test]
#[should_panic(expected = "certificate")]
async fn test_start_fails_when_cert_expired_and_grace_over() {
    let now = now_secs();
    let dir = TempDir::new().unwrap();
    let (cert, key) = write_cert(dir.path(), now - 2000, now - 1000); // 已过期 1000 秒

    // Windows 临时路径含反斜杠（如 C:\Users\...）；YAML 双引号标量里 \U、\A 等会被
    // 当作转义序列解析失败。统一转正斜杠——Windows 同样认 / 路径，且 YAML 不再报错。
    let key_path = key.to_string_lossy().replace('\\', "/");
    let cert_path = cert.to_string_lossy().replace('\\', "/");

    // 插件目录隔离到空夹具：显式指向存在的空 <base>/<triple> 目录，扫描得 0 插件，
    // 避免 workspace 的 bin/plugins（可能含 ABI 不符的陈旧产物）抢在证书校验前报错。
    let plugins_base = TempDir::new().unwrap();
    std::fs::create_dir_all(plugins_base.path().join(host_plugin_triple())).unwrap();
    let plugins_path = plugins_base.path().to_string_lossy().replace('\\', "/");

    let temp_dir = TempDir::new().unwrap();
    let service_dir = temp_dir.path();

    let config_file = NamedTempFile::new().unwrap();
    let content = format!(
        "server:\n  host: \"127.0.0.1\"\n  port: 0\n  public_key_path: \"{}\"\n  certificate_path: \"{}\"\n  grace_days: 0\nplugins_dir: \"{}\"\n",
        key_path, cert_path, plugins_path
    );
    std::fs::write(config_file.path(), content).unwrap();

    // 断言必有 panic：证书过期且宽限结束应中止启动。panic 载荷含 "certificate"，
    // 由 #[should_panic(expected = "certificate")] 校验；其余断言（如插件 ABI）失败会
    // 以不同 panic 信息暴露，从而偏离本测目标。
    let _ = server_cmd::run(ServerArgs {
        config: config_file.path().to_string_lossy().into_owned(),
        api_path: Some(service_dir.to_string_lossy().into_owned()),
        // 测试必须留终端输出：server_cmd::run 会装 fd 级 tee，console 关闭时连
        // libtest 自身的汇总行与 panic 信息都会被吞进日志文件，CI 里看不到失败原因。
        console_log: true,
        ..Default::default()
    })
    .await;
}
