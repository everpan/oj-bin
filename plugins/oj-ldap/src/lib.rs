//! oj-ldap：ldap **泛型轴** cdylib 插件（ldap3 客户端；经 GenericVtable + `axis("ldap")`）。
//!
//! 职责边界：
//! - **宿主**（`src/bridge/ldap.rs` 全局 `ldap`/`LDAP`）是遗留 typed 通道面，保持不动；
//!   本插件不再注册 typed 槽（`generic(ldap)` 声明 kind=GENERIC，loader 按 kind 路由）。
//! - **本插件**负责实例 cfg 校验（纵深防御）、连接生命周期（每调用 connect →
//!   服务账号绑定 → 操作 → unbind）、LDAP 协议交互与泛型轴 JSON 协议的全部入参校验
//!   （旧 typed 路径的宿主 `validate_call` 职责收编到 [`engine`]）。
//!
//! # 泛型轴 JSON 协议（vtable: `call(op, args)`）
//!
//! `args` 恒为位置参数 JSON 数组；末位可选 opts 对象。实例选单经 `opts.key`
//! （缺省 `"default"`）。未知 opts 键忽略（旧宿主白名单重建语义）。连接/协议/
//! 校验错误经 future Err 透传（JS 侧 reject）；`bind` 凭据被 LDAP 拒绝 = `false`，非错误。
//!
//! | op | args | opts | 结果 JSON |
//! |----|------|------|-----------|
//! | `bind` | `[dn, pw]` | `{key?}` | `true \| false` |
//! | `search` | `[base]` | `{key?, scope?, filter?, attrs?, bindDn?, bindPw?}` | `[{dn, attrs:{k:[v]}, bin:{k:[base64]}}]` |
//! | `search_paged` | `[base]` | 同上 + `{pageSize?}`（缺省 500，范围 1..=10000） | 同 `search` |
//! | `whoami` | `[]` | `{key?}` | authzid 字符串 |
//! | `compare` | `[dn, attr, val]` | `{key?}` | `true \| false` |
//!
//! - `scope`：`"base" \| "one" \| "sub"`（缺省 `"sub"`）；`filter` 缺省 `"(objectClass=*)"`);
//!   `attrs` 缺省 `[]`（全属性）。
//! - `bindDn`/`bindPw`：覆盖本次 search 系查询的绑定凭据，与 config 服务账号按位合并
//!   （各取一，另一回落 config；皆无 = 匿名绑定）。
//! - 校验失败文案沿用旧 typed 端到端（宿主 `validate_call` 层）文本，如
//!   `ldap.search: 'base' must be a non-empty string`、`ldap: unknown instance 'x'（known: …）`。
//!
//! JS 调用面（T5 泛型通道）：
//! ```js
//! await axis("ldap").bind("uid=eve,dc=example,dc=com", "pw");            // → true|false
//! await axis("ldap").search("ou=users,dc=example,dc=com", { scope: "one", attrs: ["uid"] });
//! await axis("ldap").search_paged("dc=example,dc=com", { pageSize: 1000, key: "ad" });
//! await axis("ldap").whoami({ key: "ad" });                              // → "dn:cn=svc,…"
//! await axis("ldap").compare(dn, "uid", "eve");                          // → true|false
//! ```
//!
//! 连接模型（ponytail）：不做连接池——每次调用独立成连，bind(dn,pw) 用户鉴权
//! 本就要求凭据不共享连接；search 的服务账号绑定在 AD/LAN 上是毫秒级开销。
//! 真有热路径再加 ldap3 pool（`ldap3::pool`，升级路径已预留）。

mod config;
mod engine;

use config::PluginCfg;
use engine::Engine;
use oj_plugin_ffi::{
    ABI_VERSION, FfiFuture, HOST_FINGERPRINT, HostContext, PluginDescriptor, RArc, RResult, RString,
};

/// 进程级引擎（`init` 装配，`call` 取用；重复 init 保留首个，幂等同 oj-mail）。
static ENGINE: OnceLock<Engine> = OnceLock::new();
use std::sync::OnceLock;

fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        name: RString::from("ldap"),
        semver: RString::from(env!("CARGO_PKG_VERSION")),
        abi_version: ABI_VERSION,
        fingerprint: RString::from(HOST_FINGERPRINT),
        // desc 会出现在 GET {base}/plugins 与 JS plugins()——泛型轴 + config key 自报
        // 均为 v0.1.54 起的新能力，旧宿主不可加载本迁移版（在此注明宿主最低版本）。
        desc: RString::from(
            "LDAP directory search + bind-as-auth (ldap3); generic axis, requires host >= v0.1.54",
        ),
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

/// 统一调用入口（泛型轴协议见 crate 根文档）。
/// 解析/分派/回传全在 [`Engine::call`]；此处只做「引擎未装配」兜底与跨边界 panic 收敛
/// （panic=unwind 红线：宿主对 vtable 方法无 catch_unwind）。
extern "C" fn call(op: RString, args: RString) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let Some(engine) = ENGINE.get() else {
            return oj_plugin_ffi::ready_err("oj-ldap: init not called");
        };
        engine.call(&op[..], &args[..])
    })
}

static LDAP_VT: oj_plugin_ffi::GenericVtable = oj_plugin_ffi::GenericVtable { call };

// config: "ldap" 沿用既有 config 段名；generic(ldap) 声明泛型轴（kind=GENERIC，
// loader 按 kind 路由——typed 槽不再注册，宿主 ldap.* 全局留给遗留 typed 插件）。
oj_plugin_ffi::oj_plugin_entry!(init, config: "ldap", generic(ldap) => &LDAP_VT);

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

    /// vtable 协议入口：经 GenericVtable.call(op, args) 分派——未知 op 即 ready Err
    /// （泛型通道协议在插件边界的工作证据，无网络）。
    #[test]
    fn vtable_call_dispatches_by_op() {
        let cfg = r#"{"default":{"url":"ldap://dc.example.com:389"}}"#;
        let _ = init(RArc::new(dummy_host()), RString::from(cfg));
        let f = (LDAP_VT.call)(RString::from("delete"), RString::from("[]"));
        assert_eq!((f.poll)(f.state), -1);
        let r = std::result::Result::from((f.take)(f.state));
        (f.free)(f.state);
        let Err(e) = r else { panic!("expected Err") };
        let msg = String::from_utf8_lossy(e.as_bytes());
        assert!(msg.contains("unknown op 'delete'"), "{msg}");
        assert!(
            msg.contains("bind|search|search_paged|whoami|compare"),
            "{msg}"
        );
    }

    fn dummy_host() -> HostContext {
        extern "C" fn log(_: u8, _: RString) {}
        extern "C" fn deliver(_: RString, _: oj_plugin_ffi::RBytes) {}
        HostContext { log, deliver }
    }
}
