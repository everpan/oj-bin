//! mq 轴 vtable：命名消息中间件客户端（JSON method dispatch，spec §5）。
//! 方法面（kind/send/poll/commit/ack/nack/metadata/dlq）走 `call` 的 method 字符串
//! 派发——加方法零 ABI 变更；只有 vtable 形状变更才 bump ABI_VERSION（本轴为新增，
//! 既有轴零感知，ABI 保持 7）。

use crate::{FfiFuture, RString};

#[stabby::stabby]
#[repr(C)]
pub struct MqVtable {
    /// 建立命名实例（cfg = 实例 cfg JSON，含 kind）。ok 值 = `{"handle": u64}` JSON；
    /// cfg.kind 与插件不符 / 必填参数缺失 → Err（装配层 fail-fast）。
    pub connect: extern "C" fn(cfg: RString) -> FfiFuture,
    /// method 派发。ok 值 = 结果 JSON（send→`{"sent":1}`、poll→`{"messages":[...]}`、
    /// commit/ack/nack→`{}`、metadata→自述）；未实现 method → Err(`unsupported method: <m>`)。
    pub call: extern "C" fn(handle: u64, method: RString, payload: RString) -> FfiFuture,
    /// 释放实例（退出消费组 / 关连接）。幂等；宿主 Drop 兜底也走此符号。
    pub close: extern "C" fn(handle: u64),
}
