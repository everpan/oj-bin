//! RabbitMQ 事件 broker（feature = `rabbitmq`）：基于 lapin 的 AMQP 0-9-1 客户端（纯 Rust）。
//! 使用 topic 交换（`<exchange>`）；`publish` 以 topic 为路由键投递 JSON 帧 `{"topic","data"}`。
//! `subscribe` 每订阅者建排他、自动删除队列并绑定到 topic 路由键，spawn 任务把投递转发给
//! 本地 WS 通道（`tx` 关闭即自清理）。启用：`cargo build --features rabbitmq`。

use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use lapin::options::{
    BasicAckOptions, BasicConsumeOptions, BasicPublishOptions, ExchangeDeclareOptions,
    QueueBindOptions, QueueDeclareOptions,
};
use lapin::types::FieldTable;
use lapin::{Connection, ConnectionProperties, ExchangeKind};
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;

use crate::bridge::bus::EventBroker;
use crate::bridge::BridgeResult;
use crate::config::BrokerCfg;

/// RabbitMQ 事件 broker。
pub struct RabbitMqBroker {
    conn: Arc<Connection>,
    /// topic 交换名（默认 "oj-bus"；可由 `topic_prefix` 配置覆盖）。
    exchange: String,
}

impl RabbitMqBroker {
    /// 建立连接并声明 topic 交换（fail-fast：URL 缺失或握手失败即报错；交换幂等）。
    pub async fn new(cfg: &BrokerCfg) -> BridgeResult<Self> {
        let url = cfg
            .url
            .clone()
            .or_else(|| cfg.brokers.first().cloned())
            .ok_or_else(|| -> Box<dyn std::error::Error + Send + Sync> {
                "rabbitmq requires 'url' or 'brokers'".into()
            })?;
        let exchange = cfg.topic_prefix.clone().unwrap_or_else(|| "oj-bus".into());

        let conn = Connection::connect(&url, ConnectionProperties::default())
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                format!("rabbitmq connect {url}: {e}").into()
            })?;
        let ch = conn
            .create_channel()
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                format!("rabbitmq channel: {e}").into()
            })?;
        ch.exchange_declare(
            &exchange,
            ExchangeKind::Topic,
            ExchangeDeclareOptions {
                durable: true,
                ..Default::default()
            },
            FieldTable::default(),
        )
        .await
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
            format!("rabbitmq exchange declare {exchange}: {e}").into()
        })?;
        Ok(Self {
            conn: Arc::new(conn),
            exchange,
        })
    }
}

#[async_trait]
impl EventBroker for RabbitMqBroker {
    fn kind(&self) -> &'static str {
        "rabbitmq"
    }

    async fn publish(&self, topic: &str, data: &Value) -> BridgeResult<usize> {
        let frame = json!({ "topic": topic, "data": data }).to_string();
        let channel = self
            .conn
            .create_channel()
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                format!("rabbitmq channel: {e}").into()
            })?;
        let payload = frame.into_bytes();
        // 投递到 topic 交换，路由键 = topic；丢弃返回的 PublisherConfirm（不阻塞等待 broker confirm，
        // 避免 confirm 模式未启用时挂起）。
        channel
            .basic_publish(
                &self.exchange,
                topic,
                BasicPublishOptions::default(),
                &payload,
                lapin::BasicProperties::default(),
            )
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                format!("rabbitmq publish {topic}: {e}").into()
            })?;
        Ok(0)
    }

    async fn subscribe(&self, topic: &str, tx: UnboundedSender<String>) -> BridgeResult<()> {
        let channel = self
            .conn
            .create_channel()
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                format!("rabbitmq channel: {e}").into()
            })?;
        // 排他、自动删除队列（每订阅者独立，断连自动回收）。
        let queue = channel
            .queue_declare(
                "",
                QueueDeclareOptions {
                    exclusive: true,
                    auto_delete: true,
                    ..Default::default()
                },
                FieldTable::default(),
            )
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                format!("rabbitmq queue declare: {e}").into()
            })?;
        let queue_name = queue.name().as_str().to_string();
        channel
            .queue_bind(
                &queue_name,
                &self.exchange,
                topic,
                QueueBindOptions::default(),
                FieldTable::default(),
            )
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                format!("rabbitmq queue bind {topic}: {e}").into()
            })?;
        let mut consumer = channel
            .basic_consume(
                &queue_name,
                "",
                BasicConsumeOptions::default(),
                FieldTable::default(),
            )
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                format!("rabbitmq consume {topic}: {e}").into()
            })?;
        tokio::spawn(async move {
            while let Some(delivery) = consumer.next().await {
                if let Ok(delivery) = delivery {
                    let frame = String::from_utf8_lossy(&delivery.data).to_string();
                    delivery.ack(BasicAckOptions::default()).await.ok();
                    // 接收端（WS 连接）关闭 → send 失败 → 退出并自清理。
                    if tx.send(frame).is_err() {
                        break;
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

    /// 真 RabbitMQ roundtrip：env `OJ_TEST_RABBITMQ_URL` 给 amqp URL（如 amqp://127.0.0.1:5672/）。
    /// 未设置 → 跳过（`#[ignore]`）。需要 lapin 的 `rabbitmq` feature。
    #[tokio::test]
    #[ignore]
    async fn rabbitmq_publish_subscribe_roundtrip() {
        let url = match std::env::var("OJ_TEST_RABBITMQ_URL") {
            Ok(u) if !u.is_empty() => u,
            _ => {
                eprintln!("skip: OJ_TEST_RABBITMQ_URL unset");
                return;
            }
        };
        let cfg = BrokerCfg {
            kind: "rabbitmq".into(),
            url: Some(url),
            topic_prefix: Some(format!("ojtest-{}", std::process::id())),
            ..Default::default()
        };
        let broker = RabbitMqBroker::new(&cfg).await.expect("connect rabbitmq");
        let topic = format!("t.{}", std::process::id());
        let (tx, mut rx) = mpsc::unbounded_channel();
        broker.subscribe(&topic, tx).await.expect("subscribe");
        // 等队列绑定与消费就绪
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
