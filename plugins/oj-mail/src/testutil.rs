//! `#[cfg(test)]` 共享测试工具（`config` / `engine` / `lib` 三处用例共用，避免复制）。
//!
//! 只放**与断言无关**的脚手架：FFI future 的驱动桥、隔离临时目录、空宿主。
//! 断言一律留在各自 `mod tests` 内。

use oj_plugin_ffi::{FfiFuture, HostContext, RArc, RBytes, RString};

extern "C" fn test_log(_level: u8, _msg: RString) {}
extern "C" fn test_deliver(_topic: RString, _payload: RBytes) {}

/// 空宿主（日志/上送都不做事）——用于只关心 descriptor / transport 的用例。
pub fn host() -> RArc<HostContext> {
    RArc::new(HostContext {
        log: test_log,
        deliver: test_deliver,
    })
}

/// 带自定义日志口的宿主（B6 告警用例：断言告警确实经 `HostContext.log` 上送）。
pub fn host_with_log(log: extern "C" fn(u8, RString)) -> RArc<HostContext> {
    RArc::new(HostContext {
        log,
        deliver: test_deliver,
    })
}

/// FfiFuture → 测试异步桥（等价 core 侧 `await_ffi` 的 poll 轮询）。
/// 以真实墙钟为界，避免固定轮询次数在 CI 负载下误报超时。
pub async fn drive(fut: &mut FfiFuture) -> Result<Vec<u8>, String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        match (fut.poll)(fut.state) {
            0 => {
                if std::time::Instant::now() >= deadline {
                    (fut.free)(fut.state); // 超时也要释放 state（防 FfiTask 泄漏）
                    fut.state = std::ptr::null_mut();
                    return Err("ffi drive timeout".into());
                }
                tokio::time::sleep(std::time::Duration::from_micros(100)).await;
            }
            code => {
                let r = (fut.take)(fut.state);
                (fut.free)(fut.state);
                fut.state = std::ptr::null_mut();
                return match (code, std::result::Result::from(r)) {
                    (1, Ok(b)) => Ok(b.iter().copied().collect()),
                    (_, Err(e)) => Err(e[..].to_string()),
                    _ => Err("ffi drive timeout".into()),
                };
            }
        }
    }
}

/// 每个测试用**独立**临时目录（进程号 + 标签），跑完自行清理，不污染 `sample/`。
pub fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("oj-mail-test-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建临时目录");
    dir
}

/// 路径进 JSON/YAML 双引号标量前先转正斜杠：Windows 反斜杠是转义序列
/// （`C:\Users` 的 `\U` → invalid escape），会让 `MailConfig::parse` 直接失败。
/// Windows 文件 API 均接受正斜杠。
pub fn fwd(p: &std::path::Path) -> String {
    p.display().to_string().replace('\\', "/")
}
