//! init cfg（`ldap:` 段）解析：插件 schema 的权威校验层。
//! 宿主白名单（`bridge::LdapConfig::from_value`）是同形镜像——两侧吃同一份 JSON，
//! 未知键/坏形态在宿主装配期已 fail-fast；这里是纵深防御（第三方宿主路径）。

use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;

#[derive(Debug, Clone, Deserialize)]
pub struct InstanceCfg {
    pub url: String,
    #[serde(default)]
    pub bind_dn: Option<String>,
    #[serde(default)]
    pub bind_pw: Option<String>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub start_tls: Option<bool>,
    #[serde(default)]
    pub tls_skip_verify: Option<bool>,
}

impl InstanceCfg {
    /// 校验（init 期 fail-fast）：url scheme、超时范围、bind_dn/bind_pw 成对出现。
    pub fn validate(&self) -> Result<(), String> {
        if !self.url.starts_with("ldap://") && !self.url.starts_with("ldaps://") {
            return Err(format!(
                "url must start with ldap:// or ldaps:// (got '{}')",
                self.url
            ));
        }
        if let Some(t) = self.timeout_ms
            && !(100..=3_600_000).contains(&t)
        {
            return Err(format!("timeout_ms must be in 100..=3600000 (got {t})"));
        }
        // bind_pw 可经 search opts 的 bindPw 运行时传入（与 config bind_dn 合并），
        // 故允许 config 只配 bind_dn；仅「有密码无 DN」无意义，须报错。
        if self.bind_dn.is_none() && self.bind_pw.is_some() {
            return Err(
                "bind_pw requires bind_dn (set both in config, or supply bindPw per search call)"
                    .to_string(),
            );
        }
        if self.start_tls == Some(true) && self.url.starts_with("ldaps://") {
            return Err(
                "start_tls is meaningless on an ldaps:// URL (TLS already implicit)".to_string(),
            );
        }
        Ok(())
    }

    /// 操作/连接超时（默认 5s）。
    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms.unwrap_or(5_000))
    }
}

#[derive(Debug, Deserialize)]
pub struct PluginCfg {
    #[serde(flatten)]
    pub instances: HashMap<String, InstanceCfg>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_url_and_timeout() {
        let ok: InstanceCfg =
            serde_json::from_str(r#"{"url":"ldaps://ad.internal:636","timeout_ms":3000}"#).unwrap();
        assert!(ok.validate().is_ok());
        let bad: InstanceCfg = serde_json::from_str(r#"{"url":"http://x"}"#).unwrap();
        assert!(bad.validate().is_err());
        let bad: InstanceCfg =
            serde_json::from_str(r#"{"url":"ldap://x","timeout_ms":1}"#).unwrap();
        assert!(bad.validate().is_err());
    }

    #[test]
    fn validate_bind_pair_and_starttls_ldaps_conflict() {
        // bind_dn 可单独存在（bind_pw 经 search opts 运行时传入）——不再报错。
        let half: InstanceCfg =
            serde_json::from_str(r#"{"url":"ldap://x","bind_dn":"cn=a"}"#).unwrap();
        assert!(half.validate().is_ok());
        // 仅 bind_pw 无 bind_dn 仍报错。
        let pw_only: InstanceCfg =
            serde_json::from_str(r#"{"url":"ldap://x","bind_pw":"s"}"#).unwrap();
        assert!(pw_only.validate().is_err());
        let conflict: InstanceCfg =
            serde_json::from_str(r#"{"url":"ldaps://x","start_tls":true}"#).unwrap();
        assert!(conflict.validate().is_err());
    }

    #[test]
    fn parse_section_flattened_to_instances() {
        let cfg: PluginCfg = serde_json::from_str(
            r#"{"default":{"url":"ldap://a:389"},"ad":{"url":"ldaps://b:636"}}"#,
        )
        .unwrap();
        assert_eq!(cfg.instances.len(), 2);
        assert!(cfg.instances.contains_key("default"));
    }
}
