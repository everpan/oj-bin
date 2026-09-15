//! oj-mail 配置解析（宿主透传的 `smtp` cfg → 强类型 + fail-closed 校验）。
//!
//! 配置形态（`plugins:` 段的值原样透传到 `oj_plugin_init` 的 `cfg`）：
//!
//! ```json
//! {
//!   "workers": 4,
//!   "queue_capacity": 256,
//!   "default": { "host": "smtp.example.com", "port": 465, "tls": "tls",
//!                "mechanism": "login", "user": "u", "pass": "p", "timeout": 30 },
//!   "local":   { "host": "localhost", "port": 25, "tls": "none",
//!                "allow_none_tls": true, "mechanism": "login",
//!                "file_transport": "/var/log/mail-eml" }
//! }
//! ```
//!
//! 顶层除 `workers`/`queue_capacity`/`max_attachment_bytes`/`max_total_attachment_bytes` 外的
//! **每个键都是一个 profile**（键即 `submit` 的 `key`）——未知/写错的顶层键会因缺少必填字段
//! 而在解析期报错，而非静默忽略。
//!
//! 附件上限键由**宿主**强制（字节由宿主读盘解析后经 FFI 传入，插件不做第二次判定）；
//! 此处声明它们只为不被当作 profile 解析。
//!
//! **fail-closed**：`tls: "none"`（明文，凭据与邮件可被截获）必须显式写
//! `allow_none_tls: true` 才被接受；缺省即拒绝。

use serde::Deserialize;
use std::collections::HashMap;

/// 顶层配置：并发参数 + 附件上限 + 全部 profile。
#[derive(Debug, Clone, Deserialize)]
pub struct MailConfig {
    /// worker 线程数（每个 worker 串行投递队列里的信）。
    #[serde(default = "default_workers")]
    pub workers: usize,
    /// 有界队列容量（满即背压，不无界堆积）。
    #[serde(default = "default_queue_capacity")]
    pub queue_capacity: usize,
    /// 单附件字节上限（**宿主强制**：字节由宿主读盘解析后经 FFI 传入，故判据在宿主
    /// `resolve_attachments`；本字段在此声明只为让它**不被当成 profile 解析**）。
    #[serde(default = "default_max_attachment_bytes")]
    pub max_attachment_bytes: usize,
    /// 单封信全部附件合计上限（字节，同上是宿主强制）。
    #[serde(default = "default_max_total_attachment_bytes")]
    pub max_total_attachment_bytes: usize,
    /// profile 名 → 配置。**键即 `mail.submit(key, ...)` 的 key**（未知 key 显式报错）。
    #[serde(flatten)]
    pub profiles: HashMap<String, ProfileCfg>,
}

/// 单个 profile 的连接与投递配置。
#[derive(Debug, Clone, Deserialize)]
pub struct ProfileCfg {
    /// SMTP 服务器主机名（同时用作 TLS 证书校验的域名）。
    pub host: String,
    /// SMTP 端口。
    pub port: u16,
    /// 加密模式（见 [`TlsMode`]）。
    pub tls: TlsMode,
    /// 显式允许 `tls: "none"`（明文）。缺省 `false` —— fail-closed。
    #[serde(default)]
    pub allow_none_tls: bool,
    /// 认证机制（见 [`Mechanism`]）。
    pub mechanism: Mechanism,
    /// 认证用户名（无认证中继可省略）。
    #[serde(default)]
    pub user: Option<String>,
    /// 认证口令（`mechanism: "login"` 用）。
    #[serde(default)]
    pub pass: Option<String>,
    /// XOAUTH2 凭据（`mechanism: "xoauth2"` 用）。
    #[serde(default)]
    pub xoauth2: Option<XOAuth2Cfg>,
    /// 单次 SMTP 命令超时（秒）。
    #[serde(default = "default_timeout")]
    pub timeout: u64,
    /// 本地落盘目录（给定时**不发网络**，把 `.eml` 写进该目录；测试/归档用）。
    #[serde(default)]
    pub file_transport: Option<String>,
}

/// TLS 模式（lettre `Tls` 的配置面）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TlsMode {
    /// 隐式 TLS（SMTPS，连上即握手；lettre `Tls::Wrapper`）。
    Tls,
    /// STARTTLS，要求升级成功（lettre `Tls::Required`）——**不**回落到明文。
    Starttls,
    /// 明文（lettre `Tls::None`）：仅允许受信本地中继，且须显式 `allow_none_tls`。
    None,
}

/// 认证机制（首版支持 LOGIN 与静态 access_token 的 XOAUTH2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mechanism {
    /// LOGIN（老但部分服务商如 Office 365 仍要求）。
    Login,
    /// XOAUTH2（本版仅静态 `access_token`，不支持刷新）。
    Xoauth2,
}

/// XOAUTH2 凭据。**本版只用 `access_token`**（静态令牌）；只给 `refresh_token` 会显式报错。
#[derive(Debug, Clone, Deserialize)]
pub struct XOAuth2Cfg {
    /// 静态访问令牌（`submit` 时直接作为 Bearer 使用）。
    #[serde(default)]
    pub access_token: Option<String>,
    /// 刷新令牌（本版**不支持**刷新流程，仅用于给出明确报错）。
    #[serde(default)]
    pub refresh_token: Option<String>,
}

impl XOAuth2Cfg {
    /// 取静态访问令牌。**fail-loud**：只给 `refresh_token`（或令牌为空串）时显式报错，
    /// 绝不静默降级成无凭据投递。
    pub(crate) fn static_token(&self) -> Result<&str, String> {
        match self.access_token.as_deref() {
            Some(t) if !t.is_empty() => Ok(t),
            _ if self.refresh_token.is_some() => {
                Err("xoauth2 刷新暂未支持，请提供 access_token".to_string())
            }
            _ => Err("xoauth2 需提供非空 access_token".to_string()),
        }
    }
}

fn default_workers() -> usize {
    4
}

fn default_queue_capacity() -> usize {
    256
}

/// 与宿主 `DEFAULT_MAX_ATTACHMENT_BYTES` 同值（10 MiB）。
fn default_max_attachment_bytes() -> usize {
    10 * 1024 * 1024
}

/// 与宿主 `DEFAULT_MAX_TOTAL_ATTACHMENT_BYTES` 同值（25 MiB）。
fn default_max_total_attachment_bytes() -> usize {
    25 * 1024 * 1024
}

fn default_timeout() -> u64 {
    30
}

impl MailConfig {
    /// 解析 cfg 并逐 profile 校验（**fail-closed**：不安全/自相矛盾的配置一律报错，
    /// 绝不回落默认值悄悄跑偏）。错误串含 profile 名与原因，便于定位。
    pub fn parse(raw: &str) -> Result<Self, String> {
        let cfg: MailConfig =
            serde_json::from_str(raw).map_err(|e| format!("mail 配置解析失败: {e}"))?;
        for (name, profile) in &cfg.profiles {
            profile
                .validate()
                .map_err(|e| format!("profile '{name}': {e}"))?;
        }
        Ok(cfg)
    }
}

impl ProfileCfg {
    /// 单 profile 的形状校验（凭据的**取值**问题留到 transport 构建期，见 `lib.rs`）。
    /// `build_profiles` 亦会调用（直接 `Deserialize` 出的配置绕过 `parse`）。
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.tls == TlsMode::None && !self.allow_none_tls {
            return Err(
                "tls \"none\" 为明文传输（凭据与邮件可被截获）；确需连接受信本地中继时，\
                 请显式设置 allow_none_tls: true"
                    .to_string(),
            );
        }
        if self.mechanism == Mechanism::Login && self.user.is_some() != self.pass.is_some() {
            return Err(
                "mechanism \"login\" 需同时提供 user 与 pass（或都不给 = 无认证中继）".to_string(),
            );
        }
        if self.mechanism == Mechanism::Xoauth2 && (self.xoauth2.is_none() || self.user.is_none()) {
            return Err("mechanism \"xoauth2\" 需提供 user 与 xoauth2 凭据块".to_string());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_profiles_and_tls_modes() {
        let cfg = r#"{"workers":2,"queue_capacity":8,"default":{"host":"h","port":465,"tls":"tls","mechanism":"login","user":"u","pass":"p","timeout":5}}"#;
        let c = MailConfig::parse(cfg).unwrap();
        assert_eq!(c.workers, 2);
        assert_eq!(c.queue_capacity, 8);
        assert_eq!(c.profiles["default"].port, 465);
        assert_eq!(c.profiles["default"].tls, TlsMode::Tls);
        assert_eq!(c.profiles["default"].mechanism, Mechanism::Login);
        assert_eq!(c.profiles["default"].timeout, 5);
        assert_eq!(c.profiles["default"].user.as_deref(), Some("u"));
    }

    /// 三个 tls 模式都要能解析（`starttls` 不得被当成未知值）。
    #[test]
    fn every_tls_mode_parses() {
        for (raw, want) in [
            ("tls", TlsMode::Tls),
            ("starttls", TlsMode::Starttls),
            ("none", TlsMode::None),
        ] {
            let cfg = format!(
                r#"{{"default":{{"host":"h","port":25,"tls":"{raw}","allow_none_tls":true,"mechanism":"login"}}}}"#
            );
            let c = MailConfig::parse(&cfg).unwrap_or_else(|e| panic!("{raw} 应可解析: {e}"));
            assert_eq!(c.profiles["default"].tls, want);
        }
    }

    /// fail-closed：`tls: "none"` 未显式 `allow_none_tls: true` → 拒绝，且错误信息要指出原因。
    #[test]
    fn none_tls_is_rejected_unless_explicitly_allowed() {
        // 其余字段均合法，确保拒绝的唯一原因是「明文未显式允许」。
        let e = MailConfig::parse(
            r#"{"default":{"host":"h","port":25,"tls":"none","mechanism":"login"}}"#,
        )
        .expect_err("none 未显式允许必须 Err");
        assert!(e.contains("allow_none_tls"), "错误须可读且点明开关: {e}");
        assert!(e.contains("default"), "错误须含 profile 名: {e}");

        // 显式允许 → 通过。
        MailConfig::parse(
            r#"{"default":{"host":"h","port":25,"tls":"none","allow_none_tls":true,"mechanism":"login"}}"#,
        )
        .expect("显式允许后必须通过");
    }

    /// 缺省值：workers=4 / queue_capacity=256 / timeout=30 / allow_none_tls=false。
    #[test]
    fn defaults_are_applied() {
        let c = MailConfig::parse(
            r#"{"default":{"host":"h","port":587,"tls":"starttls","mechanism":"login"}}"#,
        )
        .unwrap();
        assert_eq!(c.workers, 4);
        assert_eq!(c.queue_capacity, 256);
        let p = &c.profiles["default"];
        assert_eq!(p.timeout, 30);
        assert!(!p.allow_none_tls);
        assert!(p.user.is_none() && p.pass.is_none());
        assert!(p.file_transport.is_none());
    }

    /// B2：附件上限键（宿主强制）是**顶层键而非 profile**，解析期必须认得（否则会因
    /// 「缺 host」被当 profile 拒掉，配置一加就启动失败）。
    #[test]
    fn attachment_limit_keys_are_top_level_not_profiles() {
        let c = MailConfig::parse(
            r#"{"max_attachment_bytes":1024,"max_total_attachment_bytes":2048,
                "default":{"host":"h","port":25,"tls":"none","allow_none_tls":true,"mechanism":"login"}}"#,
        )
        .unwrap();
        assert_eq!(c.max_attachment_bytes, 1024);
        assert_eq!(c.max_total_attachment_bytes, 2048);
        assert_eq!(c.profiles.len(), 1, "上限键不得被当成 profile");
        assert!(c.profiles.contains_key("default"));

        // 缺省 = 与宿主同值的 10 MiB / 25 MiB。
        let c = MailConfig::parse(
            r#"{"default":{"host":"h","port":25,"tls":"none","allow_none_tls":true,"mechanism":"login"}}"#,
        )
        .unwrap();
        assert_eq!(c.max_attachment_bytes, 10 * 1024 * 1024);
        assert_eq!(c.max_total_attachment_bytes, 25 * 1024 * 1024);
    }

    /// 多 profile：键即 profile 名，互不干扰。
    #[test]
    fn parses_multiple_profiles() {
        let c = MailConfig::parse(
            r#"{"a":{"host":"h1","port":465,"tls":"tls","mechanism":"login","user":"u","pass":"p"},
                "b":{"host":"h2","port":25,"tls":"none","allow_none_tls":true,"mechanism":"login","file_transport":"/tmp/x"}}"#,
        )
        .unwrap();
        assert_eq!(c.profiles.len(), 2);
        assert_eq!(c.profiles["a"].host, "h1");
        assert_eq!(c.profiles["b"].file_transport.as_deref(), Some("/tmp/x"));
    }

    /// 顶层未知键会被当作 profile 解析 → 缺必填字段即报错（不静默忽略写错的键）。
    #[test]
    fn misspelled_top_level_key_fails_loud() {
        let e = MailConfig::parse(r#"{"workers":2,"profiles":{}}"#)
            .expect_err("`profiles` 不是本配置的字段，应作为 profile 解析并因缺 host 报错");
        assert!(
            e.contains("profiles") || e.contains("host"),
            "错误须可读: {e}"
        );
    }

    /// 非法 JSON / 未知枚举值必须报错（不回落默认值悄悄跑偏）。
    #[test]
    fn invalid_values_fail_loud() {
        assert!(MailConfig::parse("not json").is_err());
        assert!(MailConfig::parse(r#"{"default":{"host":"h","port":25,"tls":"ssl"}}"#).is_err());
        assert!(
            MailConfig::parse(
                r#"{"default":{"host":"h","port":25,"tls":"tls","mechanism":"plain"}}"#
            )
            .is_err()
        );
    }

    /// login 只给一半凭据是配置错误（fail-loud）；两半都不给 = 无认证中继，允许。
    #[test]
    fn partial_login_credentials_fail_loud() {
        assert!(
            MailConfig::parse(
                r#"{"default":{"host":"h","port":465,"tls":"tls","mechanism":"login","user":"u"}}"#
            )
            .is_err()
        );
        MailConfig::parse(
            r#"{"default":{"host":"h","port":25,"tls":"none","allow_none_tls":true,"mechanism":"login"}}"#,
        )
        .expect("无认证中继（user/pass 均缺）必须允许");
    }

    /// 空配置合法（无 profile 时 `submit` 一律因未知 key 报错，由 lib.rs 承担）。
    #[test]
    fn empty_config_is_valid() {
        let c = MailConfig::parse("{}").unwrap();
        assert!(c.profiles.is_empty());
    }

    /// XOAUTH2：静态 `access_token` 可用；只给 `refresh_token` → 显式报错（本版不刷新）；
    /// 缺 user / 缺凭据块 → 解析期即拒。
    #[test]
    fn xoauth2_uses_static_token_only() {
        let c = MailConfig::parse(
            r#"{"default":{"host":"h","port":465,"tls":"tls","mechanism":"xoauth2","user":"u","xoauth2":{"access_token":"at"}}}"#,
        )
        .unwrap();
        let xo = c.profiles["default"].xoauth2.as_ref().unwrap();
        assert_eq!(xo.static_token().unwrap(), "at");

        let c = MailConfig::parse(
            r#"{"default":{"host":"h","port":465,"tls":"tls","mechanism":"xoauth2","user":"u","xoauth2":{"refresh_token":"rt"}}}"#,
        )
        .unwrap();
        let e = c.profiles["default"]
            .xoauth2
            .as_ref()
            .unwrap()
            .static_token()
            .expect_err("仅 refresh_token 必须 Err");
        assert!(e.contains("access_token"), "错误须点明要 access_token: {e}");

        // 空串等同于没给。
        let c = MailConfig::parse(
            r#"{"default":{"host":"h","port":465,"tls":"tls","mechanism":"xoauth2","user":"u","xoauth2":{"access_token":""}}}"#,
        )
        .unwrap();
        assert!(
            c.profiles["default"]
                .xoauth2
                .as_ref()
                .unwrap()
                .static_token()
                .is_err()
        );

        assert!(
            MailConfig::parse(
                r#"{"default":{"host":"h","port":465,"tls":"tls","mechanism":"xoauth2","user":"u"}}"#
            )
            .is_err(),
            "xoauth2 缺凭据块须解析期拒"
        );
    }
}
