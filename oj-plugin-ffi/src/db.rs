//! db 轴 vtable（spec §3 保守形态；Task 4.1）。
//! 句柄语义同 es：connect 产 handle，close 释放；方法全返回 FfiFuture。
//! `schemes` 是工厂级属性（无 handle）：插件自我声明认领的 DSN scheme 前缀，
//! 宿主装配 DbBackendRegistry 时据此路由（spec §2 认领式；不硬编码 scheme 白名单）。

use crate::{FfiFuture, RVec, RString};

#[stabby::stabby]
#[repr(C)]
pub struct DataAccessorVtable {
    /// 建立连接（cfg = DSN 字符串）。ok 值 = `{"handle": u64}` JSON。
    pub connect: extern "C" fn(cfg: RString) -> FfiFuture,
    /// 参数化查询。params = JSON 数组；ok 值 = JSON 行数组（每行 JSON 对象）。
    pub query: extern "C" fn(handle: u64, sql: RString, params: RString) -> FfiFuture,
    /// 参数化执行，ok 值 = 受影响行数（JSON 数字）。
    pub exec: extern "C" fn(handle: u64, sql: RString, params: RString) -> FfiFuture,
    /// 开启事务。ok 值 = `{"tx_id": u64}` JSON。
    pub begin: extern "C" fn(handle: u64) -> FfiFuture,
    pub tx_query: extern "C" fn(handle: u64, tx_id: u64, sql: RString, params: RString) -> FfiFuture,
    pub tx_exec: extern "C" fn(handle: u64, tx_id: u64, sql: RString, params: RString) -> FfiFuture,
    pub tx_commit: extern "C" fn(handle: u64, tx_id: u64) -> FfiFuture,
    pub tx_rollback: extern "C" fn(handle: u64, tx_id: u64) -> FfiFuture,
    /// 已连接句柄的方言（"mysql"/"postgres"/"sqlite"，host 选 sea-query builder 用）。
    pub dialect: extern "C" fn(handle: u64) -> RString,
    pub close: extern "C" fn(handle: u64),
    /// 工厂认领的 DSN scheme 前缀列表（如 `["mysql://"]`）；host 装配期读一次。
    pub schemes: extern "C" fn() -> RVec<RString>,
}
