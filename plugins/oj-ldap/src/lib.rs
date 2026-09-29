//! oj-ldap：ldap 轴 cdylib 插件（ldap3 客户端）。
//!
//! 职责边界（同 mail 轴的宿主/插件分工）：
//! - **宿主**（`src/bridge/ldap.rs`）负责实例表白名单校验 + 调用入参校验 + JS 全局；
//! - **本插件**负责连接生命周期（每调用 connect → 服务账号绑定 → 操作 → unbind）、
//!   LDAP 协议交互与结果编码（经 `FfiFuture` 回 JSON）。
//!
//! 连接模型（ponytail）：不做连接池——每次调用独立成连，bind(dn,pw) 用户鉴权
//! 本就要求凭据不共享连接；search 的服务账号绑定在 AD/LAN 上是毫秒级开销。
//! 真有热路径再加 ldap3 pool（`ldap3::pool`，升级路径已预留）。

mod config;
mod engine;

use config::PluginCfg;
use engine::Engine;
use oj_plugin_ffi::{
    ABI_VERSION, FfiFuture, HOST_FINGERPRINT, HostContext, LdapVtable, PluginDescriptor, RArc,
    RResult, RString,
};
use std::sync::OnceLock;

/// 进程级引擎（`init` 装配，`call` 取用；重复 init 保留首个，幂等同 oj-mail）。
static ENGINE: OnceLock<Engine> = OnceLock::new();

fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        name: RString::from("ldap"),
        semver: RString::from(env!("CARGO_PKG_VERSION")),
        abi_version: ABI_VERSION,
        fingerprint: RString::from(HOST_FINGERPRINT),
        desc: RString::from("LDAP directory search + bind-as-auth (ldap3)"),
    }
}

fn init(host: RArc<HostContext>, cfg: RString) -> RResult<PluginDescriptor, RString> {
    let _ = host; // ldap 轴暂无 deliver 上送需求（无异步完成语义）。
    // rustls 0.23 要求显式安装默认 CryptoProvider（`tls-rustls-aws-lc-rs` 只启用
    // 实现，不自动 install）。本插件是独立 cdylib、自带一份 rustls，宿主侧装的
    // provider 不覆盖此 copy——不装则在 ldaps:// / start_tls 路径 panic：
    // "Could not automatically determine the process-level CryptoProvider"。
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let cfg_str = String::from_utf8_lossy(cfg.as_bytes()).into_owned();
    let parsed: PluginCfg = match serde_json::from_str(&cfg_str) {
        Ok(c) => c,
        Err(e) => return RResult::Err(RString::from(format!("ldap: cfg parse: {e}"))),
    };
    let engine = match Engine::new(parsed) {
        Ok(e) => e,
        Err(e) => return RResult::Err(RString::from(e)),
    };
    let _ = ENGINE.set(engine); // 重复 init：保留首个（幂等）
    RResult::Ok(descriptor())
}

/// 统一调用入口（契约见 `oj-plugin-ffi/src/ldap.rs` 的 `LdapVtable::call` 文档）。
/// 解析/分派/回传全在 [`Engine::call`]；此处只做「引擎未装配」兜底与跨边界 panic 收敛。
extern "C" fn call(req: RString) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let Some(engine) = ENGINE.get() else {
            return oj_plugin_ffi::ready_err("oj-ldap: init not called");
        };
        engine.call(&req[..])
    })
}

static LDAP_VTABLE: LdapVtable = LdapVtable { call };

oj_plugin_ffi::oj_plugin_entry!(init, ldap => oj_plugin_ffi::axis::ldap(&LDAP_VTABLE));

#[cfg(test)]
mod tests {
    use super::*;

    /// descriptor 身份必须是**插件名**（`ldap`）而非 crate 名（`oj-ldap`）：
    /// 宿主按文件 stem/清单键（`libldap` → `ldap`）索引，错名会让插件永远装不上。
    #[test]
    fn descriptor_name_is_plugin_name_not_crate_name() {
        assert_eq!(&descriptor().name[..], "ldap");
    }

    /// init：合法 cfg → 引擎装配成功且幂等（第二次保留首个）。
    #[test]
    fn init_parses_cfg_and_is_idempotent() {
        let cfg = r#"{"default":{"url":"ldap://dc.example.com:389","bind_dn":"cn=svc,dc=example,dc=com","bind_pw":"s","timeout_ms":3000}}"#;
        let d = init(RArc::new(dummy_host()), RString::from(cfg));
        assert!(std::result::Result::from(d).is_ok());
        let d2 = init(RArc::new(dummy_host()), RString::from(cfg));
        assert!(std::result::Result::from(d2).is_ok());
    }

    /// init：非法 cfg（未知键/坏 url）→ Err。
    #[test]
    fn init_rejects_invalid_config() {
        let bad = init(
            RArc::new(dummy_host()),
            RString::from(r#"{"default":{"url":"http://x"}}"#),
        );
        assert!(std::result::Result::from(bad).is_err());
    }

    fn dummy_host() -> HostContext {
        extern "C" fn log(_: u8, _: RString) {}
        extern "C" fn deliver(_: RString, _: oj_plugin_ffi::RBytes) {}
        HostContext { log, deliver }
    }
}
