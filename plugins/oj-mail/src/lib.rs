//! oj-mail：mail 轴 cdylib 插件（lettre SMTP）。
//!
//! 职责边界（design v3 §2，单一职责拆分）：
//! - **宿主**（`src/bridge/mail.rs`）负责配置装配、入参/白名单校验、附件字节解析
//!   （blob / 本地文件）、bus 发布、结果存储与 JS 全局挂载；
//! - **本插件**负责连接池、有界队列 + worker 池、实际投递，经 `FfiFuture` 回结果、
//!   经 `HostContext.deliver("mail.result", ...)` 上送异步完成。
//!
//! 阶段 3 已落：配置解析（`config.rs`，tls 三模式 + `none` fail-closed）与 profile 的
//! transport 构建（`build_profiles`）。阶段 4 已落：`engine.rs` 的有界队列 + worker 池 +
//! `FfiFuture` 回传 + 背压（`try_send`）+ graceful drain。阶段 5 已落：`message.rs` 的
//! 请求反序列化 + 结构化 multipart 组装 + `raw` 原文（剥离冲突头）两条投递路。

mod config;
mod engine;
mod message;
#[cfg(test)]
mod testutil;

use config::{MailConfig, Mechanism, ProfileCfg, TlsMode};
use engine::{DeliverSink, MailEngine, MailTarget, SendFn, SyncSendFn};
use lettre::address::Envelope;
use lettre::transport::smtp::authentication::{Credentials, Mechanism as LettreMechanism};
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::{
    AsyncFileTransport, AsyncSmtpTransport, AsyncTransport, FileTransport, SmtpTransport,
    Tokio1Executor, Transport,
};
use oj_plugin_ffi::{
    ABI_VERSION, FfiFuture, HOST_FINGERPRINT, HostContext, MailAttachment, MailVtable,
    PluginDescriptor, RArc, RBytes, RResult, RString, RVec,
};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// 进程级引擎（`init` 装配，`submit` 取用）。
///
/// **为什么是 `OnceLock` 而不是每次 init 现建**：`init` 是装载期同步调用，
/// 且同一插件可能被 init 多次（测试 / 重载）；若每次都新建引擎，败者会被就地 drop——
/// 虽然 `MailEngine::dispose` 已能安全销毁（`shutdown_background`），但白建一遍
/// transport 与线程池没有意义。幂等语义参见 `init`。
static MAIL_ENGINE: OnceLock<MailEngine> = OnceLock::new();

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
    /// 单次投递超时（秒配置已转 `Duration`；引擎据它包 `tokio::time::timeout`）。
    pub timeout: Duration,
}

impl MailProfile {
    /// 转引擎投递目标（超时 + 两路投递函数）。**注意**：投递闭包持有 transport 的 `Arc`，
    /// 故 transport 的最终 Drop 发生在目标表销毁处——必须处于引擎 runtime 上下文内
    /// （lettre `pool` 的 Drop 会 `tokio::spawn`，见 `engine.rs` 模块头）。
    pub fn into_target(self) -> MailTarget {
        let Self {
            async_transport,
            sync_transport,
            timeout,
        } = self;
        let send: SendFn = Arc::new(move |env: Envelope, raw: Vec<u8>| {
            let t = Arc::clone(&async_transport);
            Box::pin(async move { t.send_raw(&env, &raw).await })
        });
        let send_sync: SyncSendFn =
            Arc::new(move |env: Envelope, raw: Vec<u8>| sync_transport.send_raw(&env, &raw));
        MailTarget::new(timeout, send, send_sync)
    }
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
/// 2. lettre 的 `pool` 在 `Pool::new`（`transport/smtp/pool/async_impl.rs:56`，清理任务）
///    与 `Pool::drop`（同文件 :262）**两处**都要 `E::spawn`（= `tokio::spawn`），故 transport
///    的创建/使用/销毁都必须处于 tokio runtime 上下文内——`init`（插件装载期的同步调用）
///    不建 transport，由 `MailEngine::new` 先 `rt.enter()` 再调本函数。
///    （阶段 4 实测：无上下文时**构建期**就 panic "there is no reactor running"，不只是销毁期。）
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
            timeout: Duration::from_secs(p.timeout),
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
        timeout,
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

/// 插件自描述。身份必须 = 插件名（crate 名去 `oj-` 前缀），**不是** crate 名 ——
/// `PluginLoader::load_one` 以清单键做严格相等校验（`plugin_loader.rs:404`），
/// 且落盘文件名 `lib<name>.dylib` 亦取该名；全 8 个既有插件同此约定
/// （oj-kv-redis → "kv-redis"、oj-auth → "auth"…）。
fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        name: RString::from("mail"),
        semver: RString::from(env!("CARGO_PKG_VERSION")),
        abi_version: ABI_VERSION,
        fingerprint: RString::from(HOST_FINGERPRINT),
        desc: RString::from("mail 轴：lettre SMTP 发送（多 profile + 连接池/队列线程池）"),
    }
}

fn init(host: RArc<HostContext>, cfg: RString) -> RResult<PluginDescriptor, RString> {
    // 配置在装载期就解析并校验（fail-loud：坏配置让启动失败，而不是等第一封信才炸）。
    //
    // **先校验、后查幂等**：坏配置的拒绝与「是否已装配过」无关 —— 若先查幂等再解析，
    // 重复 init（测试并行 / 重载）会直接把坏配置当成功放行。
    let parsed = match MailConfig::parse(&cfg[..]) {
        Ok(c) => c,
        Err(e) => return RResult::Err(RString::from(e.as_str())),
    };
    if MAIL_ENGINE.get().is_some() {
        return RResult::Ok(descriptor()); // 重复 init 保留首个引擎，保持幂等
    }

    // 结果上送：生产转发 `HostContext.deliver`（测试注入收集器；见 `engine::DeliverSink`）。
    let deliver = DeliverSink::new(move |topic: &str, payload: &[u8]| {
        (host.deliver)(RString::from(topic), RBytes::from(payload));
    });
    // transport 与 worker 都在引擎自建 runtime 内装配（lettre `pool` 的 Drop 要 runtime 上下文）。
    let engine = match MailEngine::new(&parsed, deliver) {
        Ok(e) => e,
        Err(e) => return RResult::Err(RString::from(e.as_str())),
    };
    // 竞争失败（已被别的 init 抢先）时 `set` 退回引擎 → 就地 drop：`dispose` 保证按序、
    // 且在 async 上下文里也安全（见 `engine.rs` 模块头）。
    let _ = MAIL_ENGINE.set(engine);

    RResult::Ok(descriptor())
}

/// 统一投递入口（契约见 `oj-plugin-ffi/src/mail.rs` 的 `MailVtable::submit` 文档）。
/// 入队/背压/回传全部由 [`MailEngine::submit`] 承担；此处只做「引擎未装配」的兜底与
/// 跨边界 panic 收敛。
extern "C" fn submit(key: RString, req: RString, atts: RVec<MailAttachment>) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let Some(engine) = MAIL_ENGINE.get() else {
            return oj_plugin_ffi::ready_err("oj-mail: init 未调用");
        };
        engine.submit(&key[..], &req[..], atts.into_iter().collect())
    })
}

static MAIL_VTABLE: MailVtable = MailVtable { submit };

// 轴标识小写 `mail` → 生成导出符号 `oj_plugin_axis_mail`（宿主 AXES 探测表同名）。
// 经 axis::mail helper 传 vtable：类型错配在编译期即失败（宏裸传无此检查）。
oj_plugin_ffi::oj_plugin_entry!(init, mail => oj_plugin_ffi::axis::mail(&MAIL_VTABLE));

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{drive, host, temp_dir};

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

    /// vtable `submit` 的入口守卫：**未装配引擎**（或已装配的配置里没有该 profile）时一律
    /// fail-loud，绝不静默成功。对「其它用例先行 init」免疫：无论 `MAIL_ENGINE` 是否已装，
    /// 该 key 都不存在 → 必 Err（`init 未调用` / `未知 smtp profile`）。
    ///
    /// Note（阶段 5）：req 必须是**合法形态**（`from`/`to` 是契约必填，见 `message::SendRequest`）
    /// ——req 形态错误在 `submit` 期就以 FFI `Err` 返回（先于 profile 判定），那测的是另一条分支。
    ///
    /// 说明：vtable → 引擎 → 真 transport 的端到端覆盖由 `engine::tests`（真 file transport
    /// 落盘）与阶段 7 的 `oj test` e2e 承担；进程级单例不适合按用例换配置，故此处只钉入口守卫。
    #[tokio::test(flavor = "multi_thread")]
    async fn vtable_submit_fails_loud_without_matching_profile() {
        let mut fut = submit(
            RString::from("不存在的-profile"),
            RString::from(
                r#"{"from":"f@example.com","to":["t@example.com"],"raw":"X-Keep: 1\n\nbody"}"#,
            ),
            RVec::new(),
        );
        let e = drive(&mut fut).await.expect_err("无匹配 profile 必须 Err");
        assert!(
            e.contains("init 未调用") || e.contains("未知 smtp profile"),
            "错误须可读: {e}"
        );
    }

    // ---- 阶段 3：transport 构建 ----
    //
    // 全部 `#[tokio::test(flavor = "multi_thread")]`：lettre `pool` 在 AsyncSmtpTransport
    // 的 `Drop` 里 `tokio::spawn`，无 runtime 上下文 drop 即 abort（硬约束 2）。

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
