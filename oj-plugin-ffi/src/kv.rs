//! kv 轴 vtable（spec §3 保守形态；Task 4.4）。
//! 句柄语义同 es/db/blob：connect 产 handle，close 释放；方法全返回 FfiFuture。
//! 方法面 = core `KVStore` trait 全量（get/set/del/expire/incr——以 src/bridge/kv.rs:19
//! 现状定稿，比计划草稿多 expire/incr）。
//! 返回编码：get = JSON `Option<String>`（`null` / `"value"`）；expire = JSON `bool`；
//! incr = JSON `i64`；set/del = 空。跨线时长以秒计（宿主侧 expire_secs 向上取整，
//! 与 Redis EXPIRE 整秒契约对齐——ceil 逻辑留宿主，插件只认秒）。

use crate::{FfiFuture, RString};

#[stabby::stabby]
#[repr(C)]
pub struct KVStoreVtable {
    /// 建立连接（cfg = `{"url": "..."}` JSON，conn 探活 fail-fast）。ok 值 = `{"handle": u64}` JSON。
    pub connect: extern "C" fn(cfg: RString) -> FfiFuture,
    /// 读取键值。ok 值 = JSON 编码的 `Option<String>`。
    pub get: extern "C" fn(handle: u64, key: RString) -> FfiFuture,
    /// 写入键值。ok 值 = 空。
    pub set: extern "C" fn(handle: u64, key: RString, value: RString) -> FfiFuture,
    /// 删除键（幂等：不存在为成功）。ok 值 = 空。
    pub del: extern "C" fn(handle: u64, key: RString) -> FfiFuture,
    /// 设置过期（相对 ttl_secs 后读不到；键不存在 → false）。ok 值 = JSON `bool`。
    pub expire: extern "C" fn(handle: u64, key: RString, ttl_secs: u64) -> FfiFuture,
    /// 原子自增并返回新值（缺失从 0 起；非数字 → Err）。ok 值 = JSON `i64`。
    pub incr: extern "C" fn(handle: u64, key: RString) -> FfiFuture,
    pub close: extern "C" fn(handle: u64),
}
