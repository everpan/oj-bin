//! bus 全局对象：订阅发布总线（WS 会话订阅 + 任意 handler publish 广播）。
//!
//! 总线能力由统一契约 `EventBroker` 抽象：进程内 `Bus` 与分布式 `KafkaBroker` /
//! `RabbitMqBroker`（feature 启用）可透明替换；上层只依赖 `Arc<dyn EventBroker>`。
//! 广播单元（v0.1.16 二进制支持）：JSON 数据 → 文本帧 `{"topic","data"}` 信封；
//! 字节数据 → 二进制帧原样透传（无信封，topic 由订阅关系隐含）。publish 对该
//! topic 的所有订阅者 `try_send`（满/closed 即清，无背压、不阻塞发布者）。订阅方
//! 即 WS 连接的 `bus_tx`（server ws.rs 每连接建 channel，帧循环把 bus 帧转写回 socket）。

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use deno_core::{JsBuffer, OpState, op2};
use deno_error::JsErrorBox;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;

use super::{BridgeResult, ReqState, StableState, WsSend};

/// publish 载荷：JSON（包 `{"topic","data"}` 信封走文本帧）或原始字节（二进制帧透传）。
#[derive(Clone, Debug)]
pub enum BusPayload {
    Json(Value),
    Bytes(Vec<u8>),
}

/// 订阅发布总线：topic → 订阅者发送器列表（去重注册，按发送失败惰性清理）。
#[derive(Default)]
pub struct Bus {
    topics: Mutex<std::collections::HashMap<String, Vec<UnboundedSender<WsSend>>>>,
}

impl Bus {
    pub fn new() -> Self {
        Self::default()
    }

    /// 广播一帧，返回成功投递数；closed 接收方即清。
    pub fn publish(&self, topic: &str, data: &BusPayload) -> usize {
        let frame = match data {
            BusPayload::Json(v) => WsSend::Text(json!({ "topic": topic, "data": v }).to_string()),
            BusPayload::Bytes(b) => WsSend::Binary(b.clone()),
        };
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
    pub fn subscribe(&self, topic: &str, tx: UnboundedSender<WsSend>) {
        let mut g = self.topics.lock().unwrap();
        let list = g.entry(topic.to_string()).or_default();
        if !list.iter().any(|t| t.same_channel(&tx)) {
            list.push(tx);
        }
    }
}

/// 分布式事件总线的统一契约（依赖倒置：上层依赖此接口而非具体 broker）。
///
/// - `publish`：投递事件载荷到 topic（JSON → 信封文本帧；字节 → 二进制帧）；
///   返回**本地进程内**订阅者投递成功数（远程 broker 经网络投递，本地 fan-out 数
///   为 0，语义对齐原 `Bus::publish`）。
/// - `subscribe`：注册本地转发通道 `tx`；远程 broker 派生消费任务把消息转发进 `tx`，
///   `tx` 接收端关闭即 `send` 失败 → 任务自清理（无需显式 unsubscribe）。
/// - `kind`：broker 类型标识（local/kafka/rabbitmq），供 JS 侧 `bus.kind()` 感知。
#[async_trait]
pub trait EventBroker: Send + Sync {
    /// 广播事件载荷到 topic，返回本地进程内投递成功数。
    async fn publish(&self, topic: &str, data: &BusPayload) -> BridgeResult<usize>;
    /// 注册本地订阅通道（tx）。`group`：分布式 broker 的显式消费组（v0.1.139；
    /// kafka 臂 = 覆盖 group.id + `auto.offset.reset=latest`，同组跨实例分摊消费）；
    /// `None` = 插件缺省（cfg `broker.group` / 匿名排他队列）。进程内 `Bus` 忽略
    /// group（订阅天然进程本地广播）。
    async fn subscribe(
        &self,
        topic: &str,
        group: Option<&str>,
        tx: UnboundedSender<WsSend>,
    ) -> BridgeResult<()>;
    /// 同步本地扇出一帧（不经网络）。用于**非 async 上下文**——插件线程上的
    /// `HostContext.deliver`（`extern "C"`）把上送结果转给本地订阅者时调用。
    /// 返回本地投递成功数；默认 0 = 本 broker 无本地扇出面。
    fn publish_local(&self, _topic: &str, _data: &BusPayload) -> usize {
        0
    }
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
    async fn publish(&self, topic: &str, data: &BusPayload) -> BridgeResult<usize> {
        Ok(Bus::publish(self, topic, data))
    }
    async fn subscribe(
        &self,
        topic: &str,
        _group: Option<&str>,
        tx: UnboundedSender<WsSend>,
    ) -> BridgeResult<()> {
        Bus::subscribe(self, topic, tx);
        Ok(())
    }
    fn publish_local(&self, topic: &str, data: &BusPayload) -> usize {
        Bus::publish(self, topic, data)
    }
}

/// bus.publish(topic, data)：Promise<number>（本地接收方数；远程 broker 恒 0）。
/// JSON 载荷路径（对象/数组/字符串/数字都包进 `{"topic","data"}` 信封）。
#[op2]
pub async fn op_bus_publish(
    state: Rc<RefCell<OpState>>,
    #[string] topic: String,
    #[serde] data: serde_json::Value,
) -> Result<u32, JsErrorBox> {
    let bus = state.borrow().borrow::<Arc<StableState>>().bus.clone();
    let n = bus
        .publish(&topic, &BusPayload::Json(data))
        .await
        .map_err(|e| JsErrorBox::generic(e.to_string()))?;
    Ok(n as u32)
}

/// bus.publish(topic, Uint8Array)：二进制载荷路径（broker 侧按 bytes 透传，
/// 订阅会话收 binary 帧原字节；v0.1.16）。
#[op2]
pub async fn op_bus_publish_bin(
    state: Rc<RefCell<OpState>>,
    #[string] topic: String,
    #[buffer] data: JsBuffer,
) -> Result<u32, JsErrorBox> {
    let bus = state.borrow().borrow::<Arc<StableState>>().bus.clone();
    let n = bus
        .publish(&topic, &BusPayload::Bytes(data.to_vec()))
        .await
        .map_err(|e| JsErrorBox::generic(e.to_string()))?;
    Ok(n as u32)
}

/// bus.subscribe(topic, opts?)：仅 WS 连接（RequestInfo.bus_tx 注入）可注册；
/// HTTP 上下文（bus_tx None）→ JsError "bus.subscribe requires a WebSocket connection"。
/// `opts.group`：分布式 broker 显式消费组（v0.1.139）；空串/缺省 = 插件缺省语义。
#[derive(Default, serde::Deserialize)]
struct BusSubscribeOpts {
    group: Option<String>,
}

#[op2]
pub async fn op_bus_subscribe(
    state: Rc<RefCell<OpState>>,
    #[string] topic: String,
    #[serde] opts: Option<BusSubscribeOpts>,
) -> Result<(), JsErrorBox> {
    // 先取出 Arc/tx 并释放 OpState 的 RefCell 借用，避免跨 await 持有 RefCell 引用（clippy）。
    let (bus, tx, group) = {
        let s = state.borrow();
        let bus = s.borrow::<Arc<StableState>>().bus.clone();
        let tx = s.borrow::<ReqState>().req.bus_tx.clone();
        let group = opts.and_then(|o| o.group);
        (bus, tx, group)
    };
    match tx {
        Some(tx) => bus
            .subscribe(&topic, group.as_deref(), tx)
            .await
            .map_err(|e| JsErrorBox::generic(e.to_string())),
        None => Err(JsErrorBox::generic(
            "bus.subscribe requires a WebSocket connection",
        )),
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
        use crate::bridge::{
            Bridge, Extras, InMemoryAccessor, InMemoryKV, RequestInfo, SchemaRegistry,
        };
        let bus = crate::bridge::broker::build_broker(&None).await.unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        bus.subscribe("t", None, tx).await.unwrap();

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
                Extras {
                    bus: Some(bus.clone()),
                    ..Default::default()
                },
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
        let WsSend::Text(frame) = got else {
            panic!("json publish must deliver text frame: {got:?}");
        };
        assert!(frame.contains("42"), "{frame}");
    }

    #[test]
    fn publish_to_subscriber_and_unknown_topic() {
        let bus = Bus::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        bus.subscribe("news", tx);
        assert_eq!(bus.publish("news", &BusPayload::Json(json!({"a": 1}))), 1);
        let WsSend::Text(frame) = rx.try_recv().unwrap() else {
            panic!("json publish must deliver text frame");
        };
        let v: serde_json::Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(v["topic"], "news");
        assert_eq!(v["data"], json!({"a": 1}));
        // 无订阅者 → 0
        assert_eq!(bus.publish("other", &BusPayload::Json(json!(2))), 0);
    }

    /// 字节载荷 → 二进制帧原字节透传（无信封；v0.1.16）。
    #[test]
    fn bytes_payload_delivers_binary_frame_raw() {
        let bus = Bus::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        bus.subscribe("bin", tx);
        assert_eq!(bus.publish("bin", &BusPayload::Bytes(vec![0, 159, 255])), 1);
        let WsSend::Binary(b) = rx.try_recv().unwrap() else {
            panic!("bytes publish must deliver binary frame");
        };
        assert_eq!(b, vec![0, 159, 255]); // 非 UTF-8 字节无损
    }

    /// BDD：进程内 broker 下显式 group 订阅 —— group 是分布式 broker 语义，
    /// 进程本地广播不区分组：两组订阅者同 topic 全收（「内存总线无分摊」语义锚，
    /// 跨实例分摊必须配 kafka + 显式 group，见 api-manual bus 章节）。
    #[tokio::test(flavor = "current_thread")]
    async fn given_local_broker_when_subscribe_with_group_then_group_is_noop_fanout_intact() {
        let broker: Arc<dyn EventBroker> = Arc::new(Bus::new());
        let (tx1, mut rx1) = tokio::sync::mpsc::unbounded_channel();
        let (tx2, mut rx2) = tokio::sync::mpsc::unbounded_channel();
        broker.subscribe("t", Some("g1"), tx1).await.unwrap();
        broker.subscribe("t", Some("g2"), tx2).await.unwrap();
        assert_eq!(
            broker
                .publish("t", &BusPayload::Json(json!(1)))
                .await
                .unwrap(),
            2
        );
        assert!(rx1.try_recv().is_ok());
        assert!(rx2.try_recv().is_ok());
    }

    #[test]
    fn subscribe_dedupes_same_channel_and_cleans_closed() {
        let bus = Bus::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        bus.subscribe("t", tx.clone());
        bus.subscribe("t", tx); // 同 channel 去重，不重复注册
        assert_eq!(bus.publish("t", &BusPayload::Json(json!(1))), 1);
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_err()); // 只投递一次

        // 订阅者已关闭 → publish 清理并返回 0
        let bus2 = Bus::new();
        let (tx2, rx2) = tokio::sync::mpsc::unbounded_channel();
        bus2.subscribe("t", tx2);
        drop(rx2);
        assert_eq!(bus2.publish("t", &BusPayload::Json(json!(1))), 0);
    }

    /// 经统一契约 `Arc<dyn EventBroker>` 验证本地 Bus 可替换、行为一致（LSP）。
    #[tokio::test(flavor = "current_thread")]
    async fn event_broker_trait_local_fanout() {
        let broker: Arc<dyn EventBroker> = Arc::new(Bus::new());
        assert_eq!(broker.kind(), "local");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        broker.subscribe("e", None, tx).await.unwrap();
        assert_eq!(
            broker
                .publish("e", &BusPayload::Json(json!({"x": 1})))
                .await
                .unwrap(),
            1
        );
        let WsSend::Text(frame) = rx.try_recv().unwrap() else {
            panic!("json publish must deliver text frame");
        };
        let v: serde_json::Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(v["topic"], "e");
        assert_eq!(v["data"], json!({"x": 1}));
        // 无订阅者 → 0
        assert_eq!(
            broker
                .publish("other", &BusPayload::Json(json!(2)))
                .await
                .unwrap(),
            0
        );
    }
}
