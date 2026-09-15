//! oj-mail：mail 轴 cdylib 插件（lettre SMTP）。
//!
//! 职责边界（design v3 §2，单一职责拆分）：
//! - **宿主**（`src/bridge/mail.rs`）负责配置装配、入参/白名单校验、附件字节解析
//!   （blob / 本地文件）、bus 发布、结果存储与 JS 全局挂载；
//! - **本插件**负责连接池、有界队列 + worker 池、实际投递，经 `FfiFuture` 回结果、
//!   经 `HostContext.deliver("mail.result", ...)` 上送异步完成。
//!
//! 阶段 3 已落：配置解析（`config.rs`，tls 三模式 + `none` fail-closed）与 profile 的
//! transport 构建（`build_profiles`）。队列与投递在阶段 4-5 补齐，`submit` 仍显式报错
//! （fail-loud，绝不静默成功）。

mod config;

use config::{MailConfig, Mechanism, ProfileCfg, TlsMode};
use lettre::address::Envelope;
use lettre::transport::smtp::authentication::{Credentials, Mechanism as LettreMechanism};
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::{
    AsyncFileTransport, AsyncSmtpTransport, AsyncTransport, FileTransport, SmtpTransport,
    Tokio1Executor, Transport,
};
use oj_plugin_ffi::{
    ABI_VERSION, FfiFuture, HOST_FINGERPRINT, HostContext, MailAttachment, MailVtable,
    PluginDescriptor, RArc, RResult, RString, RVec,
};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// 已解析并校验过的插件配置（`init` 期定型；阶段 4 的 `MailEngine` 从此取
/// `workers`/`queue_capacity` 与 profile 定义）。
static MAIL_CFG: OnceLock<MailConfig> = OnceLock::new();

/// 异步 transport 形态：SMTP（连接池）或本地 `.eml` 落盘（免网络的测试/归档通道）。
pub enum AsyncMailTransport {
    /// lettre SMTP（`pool` feature：连接复用）。
    Smtp(AsyncSmtpTransport<Tokio1Executor>),
    /// 写 `.eml` 到目录（由 `file_transport` 配置项启用）。
    File(AsyncFileTransport<Tokio1Executor>),
}

/// 同步 transport 形态（[`AsyncMailTransport`] 的同步版本）。
pub enum SyncMailTransport {
    /// lettre SMTP（同步阻塞连接）。
    Smtp(SmtpTransport),
    /// 写 `.eml` 到目录。
    File(FileTransport),
}

/// 单个 profile 的 transport 对：async / sync 两路都建好，`submit` 按 req 的 `sync` 选路。
pub struct MailProfile {
    /// 异步路（worker 直接 await）。
    pub async_transport: Arc<AsyncMailTransport>,
    /// 同步路（worker 内 `spawn_blocking`）。
    pub sync_transport: Arc<SyncMailTransport>,
}

impl AsyncMailTransport {
    /// 投递「RFC5322 原文 + 信封」；只做形态派发，不含消息组装（阶段 5）。
    ///
    /// Ok = 投递凭据：SMTP 通道为服务器应答文本（多行合一），file 通道为落盘 id
    /// （即 `<id>.eml` 的文件名主干）。
    pub async fn send_raw(&self, envelope: &Envelope, raw: &[u8]) -> Result<String, String> {
        match self {
            Self::Smtp(t) => t
                .send_raw(envelope, raw)
                .await
                .map(|r| r.message().collect::<Vec<_>>().join(" "))
                .map_err(|e| format!("smtp: {e}")),
            Self::File(t) => t
                .send_raw(envelope, raw)
                .await
                .map_err(|e| format!("file: {e}")),
        }
    }
}

impl SyncMailTransport {
    /// 同 [`AsyncMailTransport::send_raw`] 的同步版本。
    pub fn send_raw(&self, envelope: &Envelope, raw: &[u8]) -> Result<String, String> {
        match self {
            Self::Smtp(t) => t
                .send_raw(envelope, raw)
                .map(|r| r.message().collect::<Vec<_>>().join(" "))
                .map_err(|e| format!("smtp: {e}")),
            Self::File(t) => t.send_raw(envelope, raw).map_err(|e| format!("file: {e}")),
        }
    }
}

/// 安装 rustls 默认 CryptoProvider。**幂等**：已装时 `install_default` 返回
/// `Err(已装实例)`，忽略即可（进程内只允许一个默认 provider）。
///
/// 钉死的是「与框架同源」：`src/bridge/mod.rs` 的 `ws_client_extensions` 同样装
/// `aws_lc_rs::default_provider()`（方案 B 不引 ring），两处落在**同一个默认实例**上。
fn install_crypto_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

/// 由配置构建全部 profile 的 transport。错误串含 profile 名与原因。
///
/// **两条调用约束（阶段 0 实测，均为硬约束）**：
/// 1. 首语句必须装 rustls 默认 provider，其后才可能触碰 transport 构建——
///    `TlsParameters::new`（`relay`/`starttls_relay` 内部亦调用它）会**立即**构建 rustls
///    `ClientConfig`，未装默认 provider 时直接 panic；
/// 2. lettre 的 `pool` 会在 transport **Drop** 里 `tokio::spawn` 回收任务，故 transport 的
///    创建/使用/销毁都必须处于 tokio runtime 上下文内——`init`（插件装载期的同步调用）
///    不建 transport，由阶段 4 的 `MailEngine` 在自建 multi_thread runtime 内调用本函数。
pub fn build_profiles(cfg: &MailConfig) -> Result<HashMap<String, MailProfile>, String> {
    install_crypto_provider(); // ← 硬约束 1：必须先于任何 transport 构建
    let mut profiles = HashMap::with_capacity(cfg.profiles.len());
    for (name, profile) in &cfg.profiles {
        // 直接 `Deserialize` 出的 ProfileCfg 绕过了 `MailConfig::parse`，此处补校验；
        // 安全规则的单一事实源仍是 `ProfileCfg::validate`（不写第二份判断）。
        profile
            .validate()
            .map_err(|e| format!("profile '{name}': {e}"))?;
        let built = build_profile(profile).map_err(|e| format!("profile '{name}': {e}"))?;
        profiles.insert(name.clone(), built);
    }
    Ok(profiles)
}

/// 构建单个 profile 的 async/sync transport（错误原因由调用方补上 profile 名）。
fn build_profile(p: &ProfileCfg) -> Result<MailProfile, String> {
    // `file_transport` 给定 → 本地落盘：不建 TLS、不连网（测试/归档通道）。
    if let Some(dir) = &p.file_transport {
        return Ok(MailProfile {
            async_transport: Arc::new(AsyncMailTransport::File(AsyncFileTransport::new(dir))),
            sync_transport: Arc::new(SyncMailTransport::File(FileTransport::new(dir))),
        });
    }

    let tls = build_tls(p)?;
    let (credentials, mechanisms) = credentials(p)?;
    let timeout = Duration::from_secs(p.timeout);

    let mut a = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(p.host.clone())
        .port(p.port)
        .tls(tls.clone())
        .timeout(Some(timeout));
    let mut s = SmtpTransport::builder_dangerous(p.host.clone())
        .port(p.port)
        .tls(tls)
        .timeout(Some(timeout));
    if let Some(c) = credentials {
        a = a.credentials(c.clone());
        s = s.credentials(c);
    }
    if !mechanisms.is_empty() {
        a = a.authentication(mechanisms.clone());
        s = s.authentication(mechanisms);
    }

    Ok(MailProfile {
        async_transport: Arc::new(AsyncMailTransport::Smtp(a.build())),
        sync_transport: Arc::new(SyncMailTransport::Smtp(s.build())),
    })
}

/// `tls` 三模式 → lettre `Tls`（`none` 已由 `ProfileCfg::validate` 要求显式允许）。
fn build_tls(p: &ProfileCfg) -> Result<Tls, String> {
    match p.tls {
        TlsMode::None => Ok(Tls::None),
        TlsMode::Tls => TlsParameters::new(p.host.clone())
            .map(Tls::Wrapper)
            .map_err(|e| format!("隐式 TLS 参数构建失败（host={}）: {e}", p.host)),
        TlsMode::Starttls => TlsParameters::new(p.host.clone())
            .map(Tls::Required)
            .map_err(|e| format!("STARTTLS 参数构建失败（host={}）: {e}", p.host)),
    }
}

/// 凭据 + 认证机制：`login` 用 user/pass；`xoauth2` 用静态 `access_token`
/// （仅给 `refresh_token` → 显式报错，不静默降级）；两者皆缺 = 无认证中继，
/// 此时**不**下发认证机制（lettre 默认 PLAIN+LOGIN 需凭据，无凭据即不认证）。
fn credentials(p: &ProfileCfg) -> Result<(Option<Credentials>, Vec<LettreMechanism>), String> {
    match p.mechanism {
        Mechanism::Login => match (&p.user, &p.pass) {
            (Some(u), Some(pw)) => Ok((
                Some(Credentials::new(u.clone(), pw.clone())),
                vec![LettreMechanism::Login],
            )),
            _ => Ok((None, Vec::new())),
        },
        Mechanism::Xoauth2 => {
            let user = p.user.clone().ok_or("xoauth2 缺少 user")?;
            let xo = p.xoauth2.as_ref().ok_or("xoauth2 缺少凭据块")?;
            // lettre 0.11.23 **没有** `Credentials::from_xoauth2`：XOAUTH2 的 secret 就是
            // Bearer token（见 lettre `Mechanism::Xoauth2::response`），故用
            // `Credentials::new(user, token)` 并显式把认证机制钉死为 XOAUTH2。
            let token = xo.static_token()?;
            Ok((
                Some(Credentials::new(user, token.to_string())),
                vec![LettreMechanism::Xoauth2],
            ))
        }
    }
}

fn init(_host: RArc<HostContext>, cfg: RString) -> RResult<PluginDescriptor, RString> {
    // 配置在装载期就解析并校验（fail-loud：坏配置让启动失败，而不是等第一封信才炸）。
    // **不在此建 transport**：`init` 是同步调用（未必处于 tokio runtime 上下文），而 lettre
    // `pool` 的 transport 一旦 Drop 就会 `tokio::spawn`（无 runtime → abort）。transport 的
    // 创建/使用/销毁统一归阶段 4 的 `MailEngine`（自建 runtime，在其内调 `build_profiles`）。
    let parsed = match MailConfig::parse(&cfg[..]) {
        Ok(c) => c,
        Err(e) => return RResult::Err(RString::from(e.as_str())),
    };
    let _ = MAIL_CFG.set(parsed); // 重复 init（测试/重载）保留首份，保持幂等

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

    // ---- 阶段 3：transport 构建 ----
    //
    // 全部 `#[tokio::test(flavor = "multi_thread")]`：lettre `pool` 在 AsyncSmtpTransport
    // 的 `Drop` 里 `tokio::spawn`，无 runtime 上下文 drop 即 abort（硬约束 2）。

    /// 每个测试用**独立**临时目录（进程号 + 标签），跑完自行清理，不污染 `sample/`。
    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("oj-mail-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    fn envelope() -> Envelope {
        Envelope::new(
            Some("from@example.com".parse().expect("from")),
            vec!["to@example.com".parse().expect("to")],
        )
        .expect("envelope")
    }

    fn init_outcome(cfg: &str) -> Result<PluginDescriptor, String> {
        match std::result::Result::from(init(host(), RString::from(cfg))) {
            Ok(d) => Ok(d),
            Err(e) => Err(e[..].to_string()),
        }
    }

    /// `MailProfile` 未实现 `Debug`（内含 lettre transport），故不走 `expect_err`。
    fn expect_build_err(cfg: &MailConfig) -> String {
        match build_profiles(cfg) {
            Err(e) => e,
            Ok(_) => panic!("应构建失败"),
        }
    }

    /// `build_profiles` 必须能建出 file transport profile（免网络通道），且该 profile
    /// **真能投递**（端到端写出 `.eml`）——证明 profile 不是空壳。
    #[tokio::test(flavor = "multi_thread")]
    async fn builds_file_transport_profile_after_installing_provider() {
        let dir = temp_dir("eml");
        let cfg = MailConfig::parse(&format!(
            r#"{{"default":{{"host":"localhost","port":25,"tls":"none","allow_none_tls":true,"mechanism":"login","file_transport":"{}"}}}}"#,
            dir.display()
        ))
        .expect("cfg");
        let profiles = build_profiles(&cfg).expect("build");
        assert!(profiles.contains_key("default"));
        assert!(
            matches!(
                profiles["default"].async_transport.as_ref(),
                AsyncMailTransport::File(_)
            ),
            "file_transport 给定必须走本地落盘通道"
        );

        let id = profiles["default"]
            .async_transport
            .send_raw(&envelope(), b"Subject: t\r\n\r\nbody")
            .await
            .expect("file transport 投递");
        let eml = dir.join(format!("{id}.eml"));
        assert!(eml.exists(), "应写出 {eml:?}");
        assert!(
            std::fs::read(&eml)
                .expect("读 eml")
                .starts_with(b"Subject: t"),
            "落盘内容应为投递原文"
        );

        // 同步路同款（形态派发不得只在 async 路可用）。
        let id = profiles["default"]
            .sync_transport
            .send_raw(&envelope(), b"Subject: s\r\n\r\nbody")
            .expect("sync file transport 投递");
        assert!(dir.join(format!("{id}.eml")).exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 硬约束 1：`build_profiles` 的首语句安装 rustls 默认 provider（幂等）。
    /// 安装实例必须与框架 `ws_client_extensions` 同源（aws-lc-rs）——否则出现第二个
    /// provider，运行期 `ClientConfig` 自动判定即 panic 面。
    #[tokio::test(flavor = "multi_thread")]
    async fn provider_install_is_idempotent_and_aws_lc_rs() {
        install_crypto_provider();
        let installed = rustls::crypto::CryptoProvider::get_default()
            .expect("build_profiles 后应有默认 provider");
        let fresh = rustls::crypto::aws_lc_rs::default_provider();
        assert_eq!(
            installed.cipher_suites.len(),
            fresh.cipher_suites.len(),
            "默认 provider 须为 aws-lc-rs 的完整套件集"
        );
        assert!(
            std::ptr::eq(installed.kx_groups[0], fresh.kx_groups[0]),
            "默认 provider 与框架 aws_lc_rs::default_provider() 同源实例"
        );

        // 再装一次：返回 Err（已装），且不换实例、不 panic。
        assert!(
            rustls::crypto::aws_lc_rs::default_provider()
                .install_default()
                .is_err()
        );
        assert!(std::ptr::eq(
            rustls::crypto::CryptoProvider::get_default().expect("仍在"),
            installed
        ));
    }

    /// 硬约束 1 的行为面：隐式 TLS profile 要经 `TlsParameters::new` **立即**构建 rustls
    /// `ClientConfig`——provider 未先装则直接 panic。本用例证明该路径构建成功且不 panic，
    /// 且构建后默认 provider 已装（`build_profiles` 首语句所为）。
    #[tokio::test(flavor = "multi_thread")]
    async fn tls_relay_profile_builds_after_provider_install() {
        let cfg = MailConfig::parse(
            r#"{"default":{"host":"smtp.example.com","port":465,"tls":"tls","mechanism":"login","user":"u","pass":"p"}}"#,
        )
        .expect("cfg");
        let profiles = build_profiles(&cfg).expect("隐式 TLS profile 构建不得失败/panic");
        assert!(matches!(
            profiles["default"].async_transport.as_ref(),
            AsyncMailTransport::Smtp(_)
        ));
        assert!(matches!(
            profiles["default"].sync_transport.as_ref(),
            SyncMailTransport::Smtp(_)
        ));
        assert!(
            rustls::crypto::CryptoProvider::get_default().is_some(),
            "build_profiles 首语句必须已安装默认 provider"
        );
    }

    /// starttls / none 两模式同样可建（none 需显式允许，见 config.rs）。
    #[tokio::test(flavor = "multi_thread")]
    async fn starttls_and_allowed_none_profiles_build() {
        let cfg = MailConfig::parse(
            r#"{"a":{"host":"h","port":587,"tls":"starttls","mechanism":"login","user":"u","pass":"p"},
                "b":{"host":"localhost","port":25,"tls":"none","allow_none_tls":true,"mechanism":"login"}}"#,
        )
        .expect("cfg");
        let profiles = build_profiles(&cfg).expect("build");
        assert_eq!(profiles.len(), 2);
    }

    /// unknown / 坏 profile 的错误必须可读，且**含 profile 名**（便于定位是哪一段配置）。
    #[tokio::test(flavor = "multi_thread")]
    async fn build_errors_carry_profile_name_and_reason() {
        let cfg = MailConfig::parse(
            r#"{"good":{"host":"h","port":465,"tls":"tls","mechanism":"login","user":"u","pass":"p"},
                "bad":{"host":"h","port":465,"tls":"tls","mechanism":"xoauth2","user":"u","xoauth2":{"refresh_token":"rt"}}}"#,
        )
        .expect("cfg（refresh 缺失在 build 期才报）");
        let e = expect_build_err(&cfg);
        assert!(e.contains("bad"), "错误须含 profile 名: {e}");
        assert!(e.contains("access_token"), "错误须含原因: {e}");
    }

    /// 直接 `Deserialize` 出的配置绕过 `parse`：`build_profiles` 仍须拒绝未显式允许的
    /// 明文（安全规则不可只落在解析期）。
    #[tokio::test(flavor = "multi_thread")]
    async fn build_profiles_revalidates_plaintext_bypassing_parse() {
        let cfg: MailConfig = serde_json::from_str(
            r#"{"local":{"host":"h","port":25,"tls":"none","mechanism":"login"}}"#,
        )
        .expect("反序列化");
        let e = expect_build_err(&cfg);
        assert!(e.contains("local") && e.contains("allow_none_tls"), "{e}");
    }

    /// XOAUTH2 静态 `access_token` → 可建（凭据走 lettre `Credentials::new(user, token)`）。
    #[tokio::test(flavor = "multi_thread")]
    async fn xoauth2_static_access_token_builds() {
        let cfg = MailConfig::parse(
            r#"{"default":{"host":"h","port":465,"tls":"tls","mechanism":"xoauth2","user":"u","xoauth2":{"access_token":"at"}}}"#,
        )
        .expect("cfg");
        let profiles = build_profiles(&cfg).expect("build");
        assert!(profiles.contains_key("default"));
    }

    /// 硬约束 1 的**过程**证据：子进程独享一份进程级 provider 状态（rustls 的默认 provider
    /// 是全局单例、**无卸载 API**），故只有在新进程里才能断言「未装 → 调 `build_profiles`
    /// → 已装」这条链由 `build_profiles` 自身闭合（而非来自环境里的其他 crate）。
    ///
    /// 诚实边界：单靠运行时观测**无法**反证「install 晚于 transport 构建」——方案 B 下
    /// lettre 自带 aws-lc-rs 回落，漏装也不会 panic。顺序的保证是结构性的
    /// （`build_profiles` 的第一条语句即 `install_crypto_provider()`，见其函数体），
    /// 本用例钉的是可观测事实：install 确在构建路径上，且落的是 aws-lc-rs 实例。
    #[test]
    fn provider_install_is_on_build_path_in_fresh_process() {
        if std::env::var("OJ_MAIL_PROVIDER_ORDER_CHILD").is_ok() {
            return; // 子进程只跑下面那条 exact 用例
        }
        let out = std::process::Command::new(std::env::current_exe().expect("current_exe"))
            .args([
                "--exact",
                "tests::fresh_process_starts_without_provider_and_build_installs_it",
                "--nocapture",
            ])
            .env("OJ_MAIL_PROVIDER_ORDER_CHILD", "1")
            .output()
            .expect("spawn 子进程");
        assert!(
            out.status.success(),
            "子进程必须成功（无 panic）：\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// 见上：仅在子进程（`OJ_MAIL_PROVIDER_ORDER_CHILD=1`）里做真断言。
    #[test]
    fn fresh_process_starts_without_provider_and_build_installs_it() {
        if std::env::var("OJ_MAIL_PROVIDER_ORDER_CHILD").is_err() {
            return; // 主进程里已可能被其他用例装过，此断言无意义
        }
        assert!(
            rustls::crypto::CryptoProvider::get_default().is_none(),
            "子进程起点应无默认 provider（本用例的前提）"
        );

        let cfg = MailConfig::parse(
            r#"{"default":{"host":"smtp.example.com","port":465,"tls":"tls","mechanism":"login","user":"u","pass":"p"}}"#,
        )
        .expect("cfg");
        // 自建 multi_thread runtime：隐式 TLS 走 `TlsParameters::new`（立即建 rustls
        // ClientConfig），且 async SMTP transport 带 pool（Drop 时 tokio::spawn）。
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            let profiles = build_profiles(&cfg).expect("构建不得 panic/失败");
            let installed = rustls::crypto::CryptoProvider::get_default()
                .expect("build_profiles 必须已安装默认 provider");
            let fresh = rustls::crypto::aws_lc_rs::default_provider();
            assert!(
                std::ptr::eq(installed.kx_groups[0], fresh.kx_groups[0]),
                "装的必须是 aws-lc-rs（与框架同源）实例"
            );
            drop(profiles); // 在 runtime 内销毁（pool 约束）
        });
    }

    /// `init` 必须 fail-loud：坏配置让装载失败，而不是等第一封信才炸。
    #[test]
    fn init_rejects_invalid_config() {
        let e = match init_outcome(
            r#"{"default":{"host":"h","port":25,"tls":"none","mechanism":"login"}}"#,
        ) {
            Err(e) => e,
            Ok(_) => panic!("明文未显式允许 → init 必须 Err"),
        };
        assert!(e.contains("allow_none_tls"), "{e}");
        assert!(init_outcome("not json").is_err());
        assert!(init_outcome("{}").is_ok(), "空配置合法（无 profile）");
    }
}
