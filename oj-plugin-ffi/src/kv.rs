//! kv 轴 vtable（spec §3 保守形态；Task 4.4；ABI 13 增原子原语三槽）。
//! 句柄语义同 es/db/blob：connect 产 handle，close 释放；方法全返回 FfiFuture。
//! 方法面 = core `KVStore` trait 全量（get/set/del/expire/incr + cas/setnx/get_set）。
//! 返回编码：get/get_set = JSON `Option<String>`（`null` / `"value"`）；
//! expire/cas/setnx = JSON `bool`；incr = JSON `i64`；set/del = 空。
//! 跨线时长以秒计（宿主侧 expire_secs 向上取整，与 Redis EXPIRE 整秒契约对齐——
//! ceil 逻辑留宿主，插件只认秒）；ttl_secs = 0 表无 TTL（ABI 13 三新槽约定）。

use crate::{FfiFuture, ROption, RString};

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
    /// 原子比较写（ABI 13）：expected=None 表「键不存在才写」（此时 value=None 为幂等
    /// noop——键不存在 → true 且不写）；value=None 表「匹配则删除」（解锁原语）；
    /// 值不匹配 → false 且不写。ttl_secs=0 表无 TTL。ok 值 = JSON `bool`。
    pub cas: extern "C" fn(
        handle: u64,
        key: RString,
        expected: ROption<RString>,
        value: ROption<RString>,
        ttl_secs: u64,
    ) -> FfiFuture,
    /// 原子占位（ABI 13）：键（存活视图中）不存在才写。ok 值 = JSON `bool`。
    pub setnx: extern "C" fn(handle: u64, key: RString, value: RString, ttl_secs: u64) -> FfiFuture,
    /// 原子换写（ABI 13）：无条件覆盖并返回旧值。ok 值 = JSON `Option<String>`。
    pub get_set:
        extern "C" fn(handle: u64, key: RString, value: RString, ttl_secs: u64) -> FfiFuture,
    pub close: extern "C" fn(handle: u64),
}
