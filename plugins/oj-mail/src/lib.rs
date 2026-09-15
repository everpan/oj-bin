//! oj-mail：mail 轴 cdylib 插件（lettre SMTP）。
//!
//! 职责边界（design v3 §2，单一职责拆分）：
//! - **宿主**（`src/bridge/mail.rs`）负责配置装配、入参/白名单校验、附件字节解析
//!   （blob / 本地文件）、bus 发布、结果存储与 JS 全局挂载；
//! - **本插件**负责连接池、有界队列 + worker 池、实际投递，经 `FfiFuture` 回结果、
//!   经 `HostContext.deliver("mail.result", ...)` 上送异步完成。
//!
//! 阶段 2 仅落骨架：`oj_plugin_entry!` 导出 ABI/init/`oj_plugin_axis_mail` 三符号，
//! `submit` 一律返回**显式错误**（fail-loud，绝不静默成功）；配置解析、transport 构建、
//! 队列与投递在阶段 3-5 补齐。

use oj_plugin_ffi::{
    ABI_VERSION, FfiFuture, HOST_FINGERPRINT, HostContext, MailAttachment, MailVtable,
    PluginDescriptor, RArc, RResult, RString, RVec,
};

fn init(_host: RArc<HostContext>, cfg: RString) -> RResult<PluginDescriptor, RString> {
    // 阶段 3 起在此解析 cfg 构建 MailEngine（含 rustls provider 安装——阶段 0 实测硬约束：
    // `AsyncSmtpTransport::relay()` 会立即构建 ClientConfig，install_default 必须早于任何
    // transport 构建；transport 的创建/使用/销毁亦须在本插件自己的 tokio runtime 内，
    // 因 lettre `pool` 会在 Drop 里 tokio::spawn）。
    let _ = cfg;
    RResult::Ok(PluginDescriptor {
        // 身份必须 = 插件名（crate 名去 `oj-` 前缀），**不是** crate 名 ——
        // `PluginLoader::load_one` 以清单键做严格相等校验（`plugin_loader.rs:404`），
        // 且落盘文件名 `lib<name>.dylib` 亦取该名；全 8 个既有插件同此约定
        // （oj-kv-redis → "kv-redis"、oj-auth → "auth"…）。
        name: RString::from("mail"),
        semver: RString::from(env!("CARGO_PKG_VERSION")),
        abi_version: ABI_VERSION,
        fingerprint: RString::from(HOST_FINGERPRINT),
        desc: RString::from("mail 轴：lettre SMTP 发送（多 profile + 连接池/队列线程池）"),
    })
}

/// 统一投递入口（契约见 `oj-plugin-ffi/src/mail.rs` 的 `MailVtable::submit` 文档）。
extern "C" fn submit(_key: RString, _req: RString, _atts: RVec<MailAttachment>) -> FfiFuture {
    // 阶段 3-5 实现队列/worker 投递；未实现期显式报错（fail-loud，勿静默或假装成功）。
    oj_plugin_ffi::ready_err("oj-mail: submit not implemented (阶段 2 骨架)")
}

static MAIL_VTABLE: MailVtable = MailVtable { submit };

// 轴标识小写 `mail` → 生成导出符号 `oj_plugin_axis_mail`（宿主 AXES 探测表同名）。
// 经 axis::mail helper 传 vtable：类型错配在编译期即失败（宏裸传无此检查）。
oj_plugin_ffi::oj_plugin_entry!(init, mail => oj_plugin_ffi::axis::mail(&MAIL_VTABLE));

#[cfg(test)]
mod tests {
    use super::*;
    use oj_plugin_ffi::RBytes;

    extern "C" fn test_log(_level: u8, _msg: RString) {}
    extern "C" fn test_deliver(_topic: RString, _payload: RBytes) {}

    fn host() -> RArc<HostContext> {
        RArc::new(HostContext {
            log: test_log,
            deliver: test_deliver,
        })
    }

    /// FfiFuture → 测试异步桥（等价 core 侧 await_ffi 的 poll 轮询）。
    /// 以真实墙钟为界，避免固定轮询次数在 CI 负载下误报超时。
    async fn drive(fut: &mut FfiFuture) -> Result<Vec<u8>, String> {
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

    /// descriptor 身份必须是**插件名**（`mail`）而非 crate 名（`oj-mail`）：
    /// `PluginLoader::load_one` 以清单键做严格相等校验，落盘文件名亦取该名。
    /// 本用例钉死这条约定——写错成 "oj-mail" 时预检报
    /// `plugin identity mismatch: expected 'mail', got 'oj-mail'`。
    #[test]
    fn descriptor_name_is_plugin_name_not_crate_name() {
        let desc = match std::result::Result::from(init(host(), RString::from("{}"))) {
            Ok(d) => d,
            Err(e) => panic!("init failed: {}", &e[..]),
        };
        assert_eq!(&desc.name[..], "mail");
        assert_eq!(&desc.semver[..], env!("CARGO_PKG_VERSION"));
        assert_eq!(desc.abi_version, ABI_VERSION);
        assert_eq!(&desc.fingerprint[..], HOST_FINGERPRINT);
    }

    /// 骨架期 `submit` 必须**显式失败**（fail-loud）：绝不静默返回 Ok 让宿主以为投递成功。
    /// 阶段 3-5 落地后本用例随实现更新为真投递断言。
    #[tokio::test(flavor = "current_thread")]
    async fn submit_fails_loud_until_implemented() {
        let _ = std::result::Result::from(init(host(), RString::from("{}")));
        let mut fut = submit(RString::from("default"), RString::from("{}"), RVec::new());
        let e = drive(&mut fut).await.expect_err("骨架期 submit 必须 Err");
        assert!(e.contains("not implemented"), "错误须可读: {e}");
    }
}
