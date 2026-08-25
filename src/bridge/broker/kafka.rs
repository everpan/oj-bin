//! Kafka 事件 broker（feature = `kafka`）：基于 rdkafka 的 `FutureProducer` / `StreamConsumer`。
//! `publish` 用 `FutureProducer` 异步投递 JSON 帧 `{"topic","data"}` 到 `<prefix>.<topic>`；
//! `subscribe` 每 topic 建 `StreamConsumer`（消费组），spawn 任务把消息 `tx.send` 转发给本地
//! WS 通道（`tx` 接收端关闭即自清理）。启用：`cargo build --features kafka`
//! （需系统 librdkafka；或 rdkafka 的 `cmake-build` feature 源码编译）。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use rdkafka::config::ClientConfig;
use rdkafka::consumer::{Consumer, StreamConsumer};
use rdkafka::producer::{FutureProducer, FutureRecord};
use rdkafka::Message;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;

use crate::bridge::bus::EventBroker;
use crate::bridge::BridgeResult;
use crate::config::BrokerCfg;

/// Kafka 事件 broker。
pub struct KafkaBroker {
    producer: Arc<FutureProducer>,
    consumer_cfg: ClientConfig,
    topic_prefix: String,
}

impl KafkaBroker {
    /// 装配 producer 与消费组基础配置（fail-fast：bootstrap.servers 缺失即报错）。
    pub async fn new(cfg: &BrokerCfg) -> BridgeResult<Self> {
        let brokers = cfg.brokers.join(",");
        if brokers.is_empty() {
            return Err("kafka requires 'brokers' (comma-separated bootstrap servers)".into());
        }
        let group = cfg.group.clone().unwrap_or_else(|| "oj-bus".into());
        let topic_prefix = cfg.topic_prefix.clone().unwrap_or_default();

        let producer: FutureProducer = ClientConfig::new()
            .set("bootstrap.servers", &brokers)
            .set("message.timeout.ms", "5000")
            .create()
            .map_err(|e| format!("kafka producer: {e}"))?;

        let mut consumer_cfg = ClientConfig::new();
        consumer_cfg
            .set("bootstrap.servers", &brokers)
            .set("group.id", &group)
            .set("enable.auto.commit", "true")
            .set("auto.offset.reset", "earliest");

        Ok(Self {
            producer: Arc::new(producer),
            consumer_cfg,
            topic_prefix,
        })
    }

    /// 物理 topic 名：有前缀则 `<prefix>.<topic>`。
    fn topic_of(&self, topic: &str) -> String {
        if self.topic_prefix.is_empty() {
            topic.to_string()
        } else {
            format!("{}.{}", self.topic_prefix, topic)
        }
    }
}

#[async_trait]
impl EventBroker for KafkaBroker {
    fn kind(&self) -> &'static str {
        "kafka"
    }

    async fn publish(&self, topic: &str, data: &Value) -> BridgeResult<usize> {
        let frame = json!({ "topic": topic, "data": data }).to_string();
        let physical = self.topic_of(topic);
        self.producer
            .send(
                FutureRecord::to(&physical).payload(&frame).key(&physical),
                Duration::from_secs(5),
            )
            .await
            .map_err(|(e, _)| -> Box<dyn std::error::Error + Send + Sync> {
                format!("kafka publish {physical}: {e}").into()
            })?;
        Ok(0)
    }

    async fn subscribe(&self, topic: &str, tx: UnboundedSender<String>) -> BridgeResult<()> {
        let physical = self.topic_of(topic);
        let consumer: StreamConsumer = self
            .consumer_cfg
            .create()
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                format!("kafka consumer {physical}: {e}").into()
            })?;
        consumer
            .subscribe(&[&physical])
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                format!("kafka subscribe {physical}: {e}").into()
            })?;
        // 将 consumer 移入任务：MessageStream 借用 consumer，须同生命周期存活于任务内。
        tokio::spawn(async move {
            let mut stream = consumer.stream();
            while let Some(msg) = stream.next().await {
                match msg {
                    Ok(m) => {
                        let payload = match m.payload() {
                            Some(p) => String::from_utf8_lossy(p).to_string(),
                            None => continue,
                        };
                        // 接收端（WS 连接）关闭 → send 失败 → 退出并自清理消费任务。
                        if tx.send(payload).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(target: "bus.kafka", "consume error on {physical}: {e}");
                    }
                }
            }
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::sync::mpsc;

    /// 真 Kafka roundtrip：env `OJ_TEST_KAFKA_BROKERS` 给逗号分隔 bootstrap servers。
    /// 未设置 → 跳过（`#[ignore]`）。需要 rdkafka 的 `kafka` feature。
    #[tokio::test]
    #[ignore]
    async fn kafka_publish_subscribe_roundtrip() {
        let brokers = match std::env::var("OJ_TEST_KAFKA_BROKERS") {
            Ok(b) if !b.is_empty() => b,
            _ => {
                eprintln!("skip: OJ_TEST_KAFKA_BROKERS unset");
                return;
            }
        };
        let cfg = BrokerCfg {
            kind: "kafka".into(),
            brokers: brokers.split(',').map(|s| s.trim().to_string()).collect(),
            group: Some("oj-test".into()),
            topic_prefix: Some(format!("ojtest-{}", std::process::id())),
            url: None,
        };
        let broker = KafkaBroker::new(&cfg).await.expect("connect kafka");
        let topic = format!("t-{}", std::process::id());
        let (tx, mut rx) = mpsc::unbounded_channel();
        broker.subscribe(&topic, tx).await.expect("subscribe");
        // 等消费者就绪
        tokio::time::sleep(Duration::from_millis(500)).await;
        broker.publish(&topic, &serde_json::json!({"hi": 1})).await.expect("publish");
        let frame = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("recv timeout")
            .expect("recv");
        let v: serde_json::Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(v["topic"], topic);
        assert_eq!(v["data"], serde_json::json!({"hi": 1}));
    }
}
