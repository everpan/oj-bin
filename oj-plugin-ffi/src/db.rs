//! db 轴 vtable（spec §3 保守形态；Task 4.1）。
//! 句柄语义同 es：connect 产 handle，close 释放；方法全返回 FfiFuture。
//! `schemes` 是工厂级属性（无 handle）：插件自我声明认领的 DSN scheme 前缀，
//! 宿主装配 DbBackendRegistry 时据此路由（spec §2 认领式；不硬编码 scheme 白名单）。

use crate::{FfiFuture, RString, RVec};

#[stabby::stabby]
#[repr(C)]
pub struct DataAccessorVtable {
    /// 建立连接（cfg = DSN 字符串）。ok 值 = `{"handle": u64}` JSON。
    ///
    /// 注意（M-3）：connect **只**收到 DSN 字符串，**不**接收宿主 config_dir；
    /// 因此依赖「相对路径解析」的 DSN 语义（如相对 data 目录）仅内置后端支持，
    /// 插件须始终使用绝对/完整 DSN 或自带路径解析。
    pub connect: extern "C" fn(cfg: RString) -> FfiFuture,
    /// 参数化查询。params = JSON 数组；ok 值 = JSON 行数组（每行 JSON 对象）。
    pub query: extern "C" fn(handle: u64, sql: RString, params: RString) -> FfiFuture,
    /// 参数化执行，ok 值 = 受影响行数（JSON 数字）。
    pub exec: extern "C" fn(handle: u64, sql: RString, params: RString) -> FfiFuture,
    /// 开启事务。ok 值 = `{"tx_id": u64}` JSON。
    pub begin: extern "C" fn(handle: u64) -> FfiFuture,
    pub tx_query:
        extern "C" fn(handle: u64, tx_id: u64, sql: RString, params: RString) -> FfiFuture,
    pub tx_exec: extern "C" fn(handle: u64, tx_id: u64, sql: RString, params: RString) -> FfiFuture,
    pub tx_commit: extern "C" fn(handle: u64, tx_id: u64) -> FfiFuture,
    pub tx_rollback: extern "C" fn(handle: u64, tx_id: u64) -> FfiFuture,
    /// 已连接句柄的方言（"mysql"/"postgres"/"sqlite"，host 选 sea-query builder 用）。
    pub dialect: extern "C" fn(handle: u64) -> RString,
    pub close: extern "C" fn(handle: u64),
    /// 工厂认领的 DSN scheme 前缀列表（如 `["mysql://"]`）；host 装配期读一次。
    pub schemes: extern "C" fn() -> RVec<RString>,
    /// ABI 10 起：流式查询——打开游标。params = JSON 数组。
    /// ok 值 = `{"stream_id":u64}`；**不支持流式**的后端返回 Ok(`{"unsupported":true}`)
    /// （宿主据此回落有界 fetch_all，保 dev/test 与旧插件一致），错误走 Err。
    pub stream_open: extern "C" fn(handle: u64, sql: RString, params: RString) -> FfiFuture,
    /// 批量拉取（≤100 行/次，砍逐行 FFI 往返）。ok 值 = 信封 JSON：
    /// `{"rows":[...]}`（行数组，可为空但须有键）/ `{"done":true}` / `{"error":"..."}`。
    /// **绝不裸 null**——单列 null 行会与「结束」冲突（spec §1.4 契约）。
    pub stream_next: extern "C" fn(handle: u64, stream_id: u64) -> FfiFuture,
    /// 方言级取消（协作式：批间生效——置取消标志，下一次 stream_next 返回
    /// `{"error":"cancelled"}`；是否同步杀服务端查询由插件自行决定）。
    pub stream_cancel: extern "C" fn(handle: u64, stream_id: u64) -> FfiFuture,
    /// 显式 reclaim 游标（完成/出错/取消后都必须调用；**勿用 cancel 兼任清理**——
    /// 否则每 aborted 查询泄漏一个 HashMap 条目）。
    pub stream_close: extern "C" fn(handle: u64, stream_id: u64) -> FfiFuture,
}
