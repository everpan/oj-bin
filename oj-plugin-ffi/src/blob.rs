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
    pub put:
        extern "C" fn(handle: u64, key: RString, bytes: RBytes, content_type: RString) -> FfiFuture,
    /// ok 值 = 原始字节。
    pub get: extern "C" fn(handle: u64, key: RString) -> FfiFuture,
    /// 幂等删除；ok 值 = 空。
    pub del: extern "C" fn(handle: u64, key: RString) -> FfiFuture,
    /// ok 值 = URL 字符串字节（s3 presign / local 路由）。
    pub url: extern "C" fn(handle: u64, key: RString) -> FfiFuture,
    /// ABI 9 起：上传直传预签名。op = JSON：
    /// `{"kind":"put"}` / `{"kind":"multipart_initiate"}` /
    /// `{"kind":"multipart_part","upload_id":s,"part_number":n}` /
    /// `{"kind":"multipart_complete","upload_id":s,"parts":[{"part_number":n,"etag":s}]}`。
    /// ok 值 = JSON `{"url":s, ...}`（url 为客户端直传目标；multipart_complete 的 parts
    /// 已在 op 内，返回仅需确认字段）。不支持的上传形态返回 Err（宿主回落 handler 收字节）。
    pub upload_url: extern "C" fn(handle: u64, key: RString, op: RString) -> FfiFuture,
    /// ok 值 = content-type 字符串字节（无 → 空串）。
    pub content_type: extern "C" fn(handle: u64, key: RString) -> FfiFuture,
    pub close: extern "C" fn(handle: u64),
    /// ABI 10 起：流式上传——开会话。content_type 空串 = 无显式 ct。
    /// ok 值 = `{"upload_id":u64}`；不支持流式的后端返回 Err（宿主回落整体缓冲 put）。
    pub put_stream_open:
        extern "C" fn(handle: u64, key: RString, content_type: RString) -> FfiFuture,
    /// 追加一块字节（大小不限；s3 插件内部按 ≥5MiB 攒 part）。
    pub put_stream_chunk: extern "C" fn(handle: u64, upload_id: u64, bytes: RBytes) -> FfiFuture,
    /// 提交会话（local = 临时文件转正 + ct sidecar；s3 = complete multipart）。
    pub put_stream_finish: extern "C" fn(handle: u64, upload_id: u64) -> FfiFuture,
    /// 失败清理（**必加**：s3 abort multipart 防 orphan parts 烧钱；local 删临时文件）。
    pub put_stream_abort: extern "C" fn(handle: u64, upload_id: u64) -> FfiFuture,
    /// ABI 11 起：服务端复制（**src 保留**）。ok 值 = 空；src 不存在报错。
    /// 不支持服务端复制的后端返回 Err——宿主回落 `get` + `put`（字节进 V8，仅作能力保险）。
    pub copy: extern "C" fn(handle: u64, src: RString, dst: RString) -> FfiFuture,
    /// ABI 11 起：服务端搬移（**src 不再存在**；等价于 copy + del 的原子形态）。
    /// ok 值 = 空；src 不存在报错。不支持的后端返回 Err（宿主回落 copy + del）。
    pub move_to: extern "C" fn(handle: u64, src: RString, dst: RString) -> FfiFuture,
    /// ABI 11 起：区间读。**短读截断语义**——`offset + len` 越过对象尾部时返回实际可读到的
    /// 字节（可能短于 len），`offset` 已过尾部返回空字节。ok 值 = 读到的字节。
    /// 不支持区间读的后端返回 Err（宿主回落 `get` 全量再切片）。
    pub read_range: extern "C" fn(handle: u64, key: RString, offset: u64, len: u64) -> FfiFuture,
}
