//! bus 全局对象：订阅发布总线（WS 会话订阅 + 任意 handler publish 广播）。
//!
//! 总线能力由统一契约 `EventBroker` 抽象：进程内 `Bus` 与分布式 `KafkaBroker` /
//! `RabbitMqBroker`（feature 启用）可透明替换；上层只依赖 `Arc<dyn EventBroker>`。
//! 广播单元是 JSON 帧 `{"topic","data"}`：publish 对该 topic 的所有订阅者
//! `try_send`（满/closed 即清，无背压、不阻塞发布者）。订阅方即 WS 连接的
//! `bus_tx`（server ws.rs 每连接建 channel，帧循环把 bus 帧转写回 socket）。

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use deno_core::{OpState, op2};
use deno_error::JsErrorBox;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;

use super::{BridgeResult, ReqState, StableState};

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

/// 分布式事件总线的统一契约（依赖倒置：上层依赖此接口而非具体 broker）。
///
/// - `publish`：投递事件帧 `{"topic","data"}` 到 topic；返回**本地进程内**订阅者投递成功数
///   （远程 broker 经网络投递，本地 fan-out 数为 0，语义对齐原 `Bus::publish`）。
/// - `subscribe`：注册本地转发通道 `tx`；远程 broker 派生消费任务把消息转发进 `tx`，
///   `tx` 接收端关闭即 `send` 失败 → 任务自清理（无需显式 unsubscribe）。
/// - `kind`：broker 类型标识（local/kafka/rabbitmq），供 JS 侧 `bus.kind()` 感知。
#[async_trait]
pub trait EventBroker: Send + Sync {
    /// 广播事件帧到 topic，返回本地进程内投递成功数。
    async fn publish(&self, topic: &str, data: &Value) -> BridgeResult<usize>;
    /// 注册本地订阅通道（tx）。
    async fn subscribe(&self, topic: &str, tx: UnboundedSender<String>) -> BridgeResult<()>;
    /// broker 类型标识。
    fn kind(&self) -> &'static str {
        "unknown"
    }
}

#[async_trait]
impl EventBroker for Bus {
    fn kind(&self) -> &'static str {
        "local"
    }
    async fn publish(&self, topic: &str, data: &Value) -> BridgeResult<usize> {
        Ok(Bus::publish(self, topic, data))
    }
    async fn subscribe(&self, topic: &str, tx: UnboundedSender<String>) -> BridgeResult<()> {
        Bus::subscribe(self, topic, tx);
        Ok(())
    }
}

/// bus.publish(topic, data)：Promise<number>（本地接收方数；远程 broker 恒 0）。
#[op2]
pub async fn op_bus_publish(
    state: Rc<RefCell<OpState>>,
    #[string] topic: String,
    #[serde] data: serde_json::Value,
) -> Result<u32, JsErrorBox> {
    let bus = state.borrow().borrow::<Arc<StableState>>().bus.clone();
    let n = bus
        .publish(&topic, &data)
        .await
        .map_err(|e| JsErrorBox::generic(e.to_string()))?;
    Ok(n as u32)
}

/// bus.subscribe(topic)：仅 WS 连接（RequestInfo.bus_tx 注入）可注册；
/// HTTP 上下文（bus_tx None）→ JsError "bus.subscribe requires a WebSocket connection"。
#[op2]
pub async fn op_bus_subscribe(
    state: Rc<RefCell<OpState>>,
    #[string] topic: String,
) -> Result<(), JsErrorBox> {
    // 先取出 Arc/tx 并释放 OpState 的 RefCell 借用，避免跨 await 持有 RefCell 引用（clippy）。
    let (bus, tx) = {
        let s = state.borrow();
        let bus = s.borrow::<Arc<StableState>>().bus.clone();
        let tx = s.borrow::<ReqState>().req.bus_tx.clone();
        (bus, tx)
    };
    match tx {
        Some(tx) => bus
            .subscribe(&topic, tx)
            .await
            .map_err(|e| JsErrorBox::generic(e.to_string())),
        None => Err(JsErrorBox::generic("bus.subscribe requires a WebSocket connection")),
    }
}

/// bus.kind()：当前 broker 类型（"local" | "kafka" | "rabbitmq"），Promise<string>。
#[op2]
#[string]
pub async fn op_bus_kind(state: Rc<RefCell<OpState>>) -> Result<String, JsErrorBox> {
    let bus = state.borrow().borrow::<Arc<StableState>>().bus.clone();
    Ok(bus.kind().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 同一 broker 实例跨两个 Bridge 注入：经 B 的 JS 发布，A 侧订阅通道收到
    /// （"同一已连接 broker 实例跨 actor 池与全部 WS 连接共享"语义回归，spec §2）。
    #[tokio::test(flavor = "current_thread")]
    async fn shared_broker_broadcasts_across_bridges() {
        use crate::bridge::{Bridge, Extras, InMemoryAccessor, InMemoryKV, RequestInfo, SchemaRegistry};
        let bus = crate::bridge::broker::build_broker(&None).await.unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        bus.subscribe("t", tx).await.unwrap();

        let mk = |bus: &Arc<dyn EventBroker>| {
            Bridge::with_dbs_and_loader(
                std::collections::HashMap::from([(
                    "default".to_string(),
                    Arc::new(InMemoryAccessor::new()) as Arc<dyn crate::bridge::DataAccessor>,
                )]),
                Arc::new(InMemoryKV::new()),
                SchemaRegistry::new(),
                false,
                None,
                Extras { bus: Some(bus.clone()), ..Default::default() },
            )
        };
        let _a = mk(&bus); // A 持同一实例（池内另一 actor）
        let b = mk(&bus); // B 经 JS 发布
        let cap = b
            .run_with(
                r#"(async () => { const n = await bus.publish("t", { v: 42 }); json.ok({ n }); })().catch((e) => json.ok({ err: String(e) }));"#,
                RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["data"]["n"], 1, "{v}");
        let got = rx.recv().await.unwrap();
        assert!(got.contains("42"), "{got}");
    }

    #[test]
    fn publish_to_subscriber_and_unknown_topic() {
        let bus = Bus::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        bus.subscribe("news", tx);
        assert_eq!(bus.publish("news", &json!({"a": 1})), 1);
        let frame = rx.try_recv().unwrap();
        let v: serde_json::Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(v["topic"], "news");
        assert_eq!(v["data"], json!({"a": 1}));
        // 无订阅者 → 0
        assert_eq!(bus.publish("other", &json!(2)), 0);
    }

    #[test]
    fn subscribe_dedupes_same_channel_and_cleans_closed() {
        let bus = Bus::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        bus.subscribe("t", tx.clone());
        bus.subscribe("t", tx); // 同 channel 去重，不重复注册
        assert_eq!(bus.publish("t", &json!(1)), 1);
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_err()); // 只投递一次

        // 订阅者已关闭 → publish 清理并返回 0
        let bus2 = Bus::new();
        let (tx2, rx2) = tokio::sync::mpsc::unbounded_channel();
        bus2.subscribe("t", tx2);
        drop(rx2);
        assert_eq!(bus2.publish("t", &json!(1)), 0);
    }

    /// 经统一契约 `Arc<dyn EventBroker>` 验证本地 Bus 可替换、行为一致（LSP）。
    #[tokio::test(flavor = "current_thread")]
    async fn event_broker_trait_local_fanout() {
        let broker: Arc<dyn EventBroker> = Arc::new(Bus::new());
        assert_eq!(broker.kind(), "local");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        broker.subscribe("e", tx).await.unwrap();
        assert_eq!(broker.publish("e", &json!({"x": 1})).await.unwrap(), 1);
        let frame = rx.try_recv().unwrap();
        let v: serde_json::Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(v["topic"], "e");
        assert_eq!(v["data"], json!({"x": 1}));
        // 无订阅者 → 0
        assert_eq!(broker.publish("other", &json!(2)).await.unwrap(), 0);
    }
}
