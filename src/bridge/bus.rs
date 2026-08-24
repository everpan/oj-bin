//! bus 全局对象：进程内订阅发布总线（WS 会话订阅 + 任意 handler publish 广播）。
//!
//! 广播单元是 JSON 帧 `{"topic","data"}`：publish 对该 topic 的所有订阅者
//! `try_send`（满/closed 即清，无背压、不阻塞发布者）。订阅方即 WS 连接的
//! `bus_tx`（server ws.rs 每连接建 channel，帧循环把 bus 帧转写回 socket）。

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use deno_core::{OpState, op2};
use deno_error::JsErrorBox;
use serde_json::{Value, json};

use super::{ReqState, StableState};

/// 订阅发布总线：topic → 订阅者发送器列表（去重注册，按发送失败惰性清理）。
#[derive(Default)]
pub struct Bus {
    topics: Mutex<std::collections::HashMap<String, Vec<tokio::sync::mpsc::UnboundedSender<String>>>>,
}

impl Bus {
    pub fn new() -> Self {
        Self::default()
    }

    /// 广播 `{"topic","data"}` JSON 帧，返回成功投递数；closed 接收方即清。
    pub fn publish(&self, topic: &str, data: &Value) -> usize {
        let frame = json!({ "topic": topic, "data": data }).to_string();
        let mut g = self.topics.lock().unwrap();
        let mut n = 0;
        if let Some(list) = g.get_mut(topic) {
            list.retain(|tx| {
                if tx.send(frame.clone()).is_ok() {
                    n += 1;
                    true
                } else {
                    false
                }
            });
            if list.is_empty() {
                g.remove(topic);
            }
        }
        n
    }

    /// 注册订阅（同 channel 去重；同一 topic 幂等）。
    pub fn subscribe(&self, topic: &str, tx: tokio::sync::mpsc::UnboundedSender<String>) {
        let mut g = self.topics.lock().unwrap();
        let list = g.entry(topic.to_string()).or_default();
        if !list.iter().any(|t| t.same_channel(&tx)) {
            list.push(tx);
        }
    }
}

/// bus.publish(topic, data)：Promise<number>（接收方数）。
#[op2]
pub async fn op_bus_publish(
    state: Rc<RefCell<OpState>>,
    #[string] topic: String,
    #[serde] data: serde_json::Value,
) -> Result<u32, JsErrorBox> {
    let bus = state.borrow().borrow::<Arc<StableState>>().bus.clone();
    Ok(bus.publish(&topic, &data) as u32)
}

/// bus.subscribe(topic)：仅 WS 连接（RequestInfo.bus_tx 注入）可注册；
/// HTTP 上下文（bus_tx None）→ JsError "bus.subscribe requires a WebSocket connection"。
#[op2]
pub async fn op_bus_subscribe(
    state: Rc<RefCell<OpState>>,
    #[string] topic: String,
) -> Result<(), JsErrorBox> {
    let s = state.borrow();
    let bus = s.borrow::<Arc<StableState>>().bus.clone();
    let tx = s.borrow::<ReqState>().req.bus_tx.clone();
    match tx {
        Some(tx) => {
            bus.subscribe(&topic, tx);
            Ok(())
        }
        None => Err(JsErrorBox::generic("bus.subscribe requires a WebSocket connection")),
    }
}
