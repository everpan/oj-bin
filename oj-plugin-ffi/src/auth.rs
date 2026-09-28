//! auth 轴 vtable（Task auth-1）：请求守卫，同步纯密码学验签——无 async 跨边界，
//! 是全轴里最适合 FFI 的形态。ok 值 JSON：`null` = 匿名路径放行；
//! 对象 = 注入 http.user（`{"id","roles","claims"}`）；Err = 401 消息。
//! authorization 空串 = 无 Authorization 头。
//!
//! 实现必须在 `catch_value` 内收敛 panic：宿主侧对 vtable 方法**无 catch_unwind**，
//! 裸 panic = 进程 abort——守卫是每请求热路径，插件作者须自查。

use crate::{RResult, RString};

#[stabby::stabby]
#[repr(C)]
pub struct AuthGuardVtable {
    /// ABI 9 起四参：method = 大写 HTTP 方法（WS 握手 = "GET"）；
    /// headers = 全部请求头 JSON（小写名 → 值，多值取第一个，含 cookie），
    /// 空串 = 无头。cookie 会话/CSRF 双提交的判定材料都在 headers 里（插件按
    /// 各自 cfg 的头名自取），Bearer 形态可全部忽略。
    pub verify: extern "C" fn(
        path_no_base: RString,
        method: RString,
        authorization: RString,
        headers: RString,
    ) -> RResult<RString, RString>,
}
