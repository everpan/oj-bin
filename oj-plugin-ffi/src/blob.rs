//! blob 轴 vtable（spec §3 保守形态；Task 4.2）。
//! 句柄语义同 es/db：connect 产 handle，close 释放；方法全返回 FfiFuture。
//! connect 透传注册名（spec §2：url 裁决——本版 s3 插件 url 对所有名字可用，保留签名）。
//! 无 serve 方法：core 下载路由的 serve = 经 FfiBlobBackend 组合（get+content_type 内联
//! 或 url 重定向），由适配器按后端语义实现（s3 → Redirect(url)）。

use crate::{FfiFuture, RBytes, RString};

#[stabby::stabby]
#[repr(C)]
pub struct BlobBackendVtable {
    /// 建立后端（name = 注册名，cfg = JSON 配置）。ok 值 = `{"handle": u64}` JSON。
    pub connect: extern "C" fn(name: RString, cfg: RString) -> FfiFuture,
    /// ok 值 = 空（成功）；content_type 空串 = 无显式 ct。
    pub put: extern "C" fn(
        handle: u64,
        key: RString,
        bytes: RBytes,
        content_type: RString,
    ) -> FfiFuture,
    /// ok 值 = 原始字节。
    pub get: extern "C" fn(handle: u64, key: RString) -> FfiFuture,
    /// 幂等删除；ok 值 = 空。
    pub del: extern "C" fn(handle: u64, key: RString) -> FfiFuture,
    /// ok 值 = URL 字符串字节（s3 presign / local 路由）。
    pub url: extern "C" fn(handle: u64, key: RString) -> FfiFuture,
    /// ok 值 = content-type 字符串字节（无 → 空串）。
    pub content_type: extern "C" fn(handle: u64, key: RString) -> FfiFuture,
    pub close: extern "C" fn(handle: u64),
}
