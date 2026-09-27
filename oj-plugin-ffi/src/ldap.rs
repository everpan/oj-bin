//! ldap 轴 vtable（新增轴，ABI 不变——spec「加轴零破坏」）。
//! 契约形态对齐 mail 轴：仅 `call` 一个 extern "C" 入口，操作与参数由 req JSON 承载
//! （加操作零 ABI 变更）。req：`{"op":"bind|search|search_paged|whoami|compare",
//! "key":"<实例名>", ...op 参数}`；ok 值 = JSON（bind/compare → `bool`；search/
//! search_paged → `[{"dn","attrs","bin"}]`；whoami → `String`）。
//! 实例表（`ldap:` 段）在插件 `init` 时装配；`call` 每请求幂等取表，不做连接句柄
//! （连接 = 每调用建立 + 服务账号绑定 + unbind 释放，bind(dn,pw) 鉴权独立成连，
//! 用户凭据绝不进共享池）。

use crate::{FfiFuture, RString};

/// ldap 轴：`call` 统一入口，行为由 req JSON 的 `op` 决定。
#[stabby::stabby]
#[repr(C)]
pub struct LdapVtable {
    /// 执行一个 LDAP 操作。`req` = 请求 JSON（字段语义见本模块文档）；
    /// ok 值 = 结果 JSON（op 相关形态）；Err = 操作失败（连接/协议/解析），
    /// 宿主侧收敛为 JS 异常。`key` 未知实例 → Err（fail-loud，不回落 default）。
    pub call: extern "C" fn(req: RString) -> FfiFuture,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 编译期形状断言：vtable 只含一个 call 入口（同 mail 轴的保守形态）。
    #[test]
    fn vtable_is_single_entry() {
        extern "C" fn stub(_: RString) -> FfiFuture {
            unreachable!()
        }
        let vt = LdapVtable { call: stub };
        let _: extern "C" fn(RString) -> FfiFuture = vt.call;
    }
}
