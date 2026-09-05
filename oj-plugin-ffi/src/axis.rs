//! 按轴类型安全擦除：`oj_plugin_entry!` 宏对「轴标识 ↔ vtable 类型」配对不做编译期
//! 检查（`$vtable as *const _` 对任意静态都可编译），复制粘贴写错（如 `auth => &KV_VTABLE`）
//! 会在宿主侧按轴转型时直接 UB——auth verify 是每请求热路径。本模块每轴一个 helper，
//! 参数 = 该轴确切的 vtable 类型，类型错配编译失败。推荐一律经 helper 传 vtable：
//! `oj_plugin_entry!(init, kv => oj_plugin_ffi::axis::kv(&KV_VTABLE))`。

use std::ffi::c_void;

use crate::{
    AuthGuardVtable, BlobBackendVtable, DataAccessorVtable, EsBackendVtable, EventBrokerVtable,
    KVStoreVtable,
};

pub fn es(vt: &'static EsBackendVtable) -> *const c_void {
    vt as *const _ as *const c_void
}

pub fn db(vt: &'static DataAccessorVtable) -> *const c_void {
    vt as *const _ as *const c_void
}

pub fn blob(vt: &'static BlobBackendVtable) -> *const c_void {
    vt as *const _ as *const c_void
}

pub fn bus(vt: &'static EventBrokerVtable) -> *const c_void {
    vt as *const _ as *const c_void
}

pub fn kv(vt: &'static KVStoreVtable) -> *const c_void {
    vt as *const _ as *const c_void
}

pub fn auth(vt: &'static AuthGuardVtable) -> *const c_void {
    vt as *const _ as *const c_void
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RResult, RString};

    use crate::axis;

    /// 编译期配对断言：fn 项到具名 fn 指针的强制转换要求签名完全一致——把「轴 ↔
    /// vtable 类型」配对钉死。传错类型（如 `axis::kv(&AUTH_VTABLE)`，宏裸传下可编译）
    /// 在此形态下一律编译失败（编译失败无法写成运行期用例，此处即证明）。
    #[test]
    fn helpers_bind_exact_vtable_types() {
        let _: fn(&'static EsBackendVtable) -> *const c_void = axis::es;
        let _: fn(&'static DataAccessorVtable) -> *const c_void = axis::db;
        let _: fn(&'static BlobBackendVtable) -> *const c_void = axis::blob;
        let _: fn(&'static EventBrokerVtable) -> *const c_void = axis::bus;
        let _: fn(&'static KVStoreVtable) -> *const c_void = axis::kv;
        let _: fn(&'static AuthGuardVtable) -> *const c_void = axis::auth;
    }

    /// 运行期抽查：helper 返回的裸指针与引用同址且非空。auth 是唯一单字段 vtable，
    /// 可低成本真实构造（其余轴同理，不重复摆）。
    #[test]
    fn auth_helper_returns_same_non_null_pointer() {
        extern "C" fn verify(_: RString, _: RString) -> RResult<RString, RString> {
            RResult::Err(RString::from("stub"))
        }
        static VT: AuthGuardVtable = AuthGuardVtable { verify };
        let p = axis::auth(&VT);
        assert!(!p.is_null());
        assert_eq!(p, &VT as *const AuthGuardVtable as *const c_void);
    }
}
