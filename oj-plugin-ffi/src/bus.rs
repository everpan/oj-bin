//! bus 轴 vtable（spec §3 保守形态；Task 4.3）。
//! 句柄语义同 es/db/blob：connect 产 handle，close 释放；方法全返回 FfiFuture。
//! subscribe 在插件侧起消费循环，收到消息经宿主注入的 HostContext.deliver 回调
//! （非阻塞）上送宿主——UnboundedSender 不过 FFI 边界（spec §3 回调注入条）。
//! 宿主侧 deliver 按 topic 扇出到本地订阅通道（同一 broker 实例跨 actor/WS 共享语义）。

use crate::{FfiFuture, RBytes, RString};

#[stabby::stabby]
#[repr(C)]
pub struct EventBrokerVtable {
    /// 建立 broker（cfg = BrokerCfg JSON）。ok 值 = `{"handle": u64}` JSON。
    pub connect: extern "C" fn(cfg: RString) -> FfiFuture,
    /// 投递事件载荷到 topic。ok 值 = 空。
    /// ABI 8 起 data 为字节（RBytes）：JSON 载荷 = `{"topic","data"}` 信封 UTF-8；
    /// 二进制载荷 = 原始字节（宿主消费侧按「UTF-8 且含 topic/data 键的 JSON 对象」
    /// 启发式还原帧型，见宿主 envelope_or_binary）。
    pub publish: extern "C" fn(handle: u64, topic: RString, data: RBytes) -> FfiFuture,
    /// 起消费循环（每 topic 至多一次；宿主按 topic 扇出）。ok 值 = 空。
    pub subscribe: extern "C" fn(handle: u64, topic: RString) -> FfiFuture,
    pub close: extern "C" fn(handle: u64),
}
