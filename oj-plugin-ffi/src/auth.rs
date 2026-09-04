//! auth 轴 vtable（Task auth-1）：请求守卫，同步纯密码学验签——无 async 跨边界，
//! 是全轴里最适合 FFI 的形态。ok 值 JSON：`null` = 匿名路径放行；
//! 对象 = 注入 http.user（`{"id","roles","claims"}`）；Err = 401 消息。
//! authorization 空串 = 无 Authorization 头。

use crate::{RResult, RString};

#[stabby::stabby]
#[repr(C)]
pub struct AuthGuardVtable {
    pub verify:
        extern "C" fn(path_no_base: RString, authorization: RString) -> RResult<RString, RString>,
}
