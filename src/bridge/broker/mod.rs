//! 分布式事件 broker 工厂与具体实现。
//!
//! 统一契约在 `super::bus::EventBroker`：进程内 `Bus` 与分布式 `KafkaBroker` /
//! `RabbitMqBroker` 皆实现之，经 `build_broker` 按配置构造为 `Arc<dyn EventBroker>`
//! 注入 `StableState.bus`。新增 broker 只需加一个 feature-gated 模块并在 `build_broker`
//! 补分支（OCP）。
//!
//! Kafka / RabbitMQ 实现经 Cargo feature 可选编译（默认构建不含其客户端依赖）：
//! `cargo build --features kafka` / `--features rabbitmq`。未启用 feature 却声明对应
//! `kind` → 装配期明确报错（而非静默退化）。

#[cfg(feature = "kafka")]
pub mod kafka;
#[cfg(feature = "rabbitmq")]
pub mod rabbitmq;

use std::sync::Arc;

use super::bus::EventBroker;
use super::BridgeResult;
use crate::config::BrokerCfg;

/// 按配置构造事件 broker。
///
/// - `None` 或 `kind` 为空 / "local" → 进程内 `Bus`（零配置、默认行为，保持现状）。
/// - "kafka" / "rabbitmq" → 对应实现；若未启用对应 feature → Err（装配 fail-fast）。
pub async fn build_broker(cfg: &Option<BrokerCfg>) -> BridgeResult<Arc<dyn EventBroker>> {
    match cfg {
        None => Ok(Arc::new(super::bus::Bus::new())),
        Some(c) => match c.kind.as_str() {
            "" | "local" => Ok(Arc::new(super::bus::Bus::new())),
            #[cfg(feature = "kafka")]
            "kafka" => Ok(Arc::new(kafka::KafkaBroker::new(c).await?)),
            #[cfg(feature = "rabbitmq")]
            "rabbitmq" => Ok(Arc::new(rabbitmq::RabbitMqBroker::new(c).await?)),
            #[cfg(not(feature = "kafka"))]
            "kafka" => Err(
                "broker kind 'kafka' requires the 'kafka' cargo feature (build with --features kafka)"
                    .into(),
            ),
            #[cfg(not(feature = "rabbitmq"))]
            "rabbitmq" => Err(
                "broker kind 'rabbitmq' requires the 'rabbitmq' cargo feature (build with --features rabbitmq)"
                    .into(),
            ),
            other => Err(format!("unknown broker kind: {other}").into()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::bus::Bus;
    use serde_json::json;

    /// 缺省与 "local" 均返回进程内 Bus（kind == "local"），保持零配置现状。
    #[tokio::test(flavor = "current_thread")]
    async fn build_broker_default_and_local_are_local_bus() {
        let b = build_broker(&None).await.unwrap();
        assert_eq!(b.kind(), "local");
        // 经统一契约可发布（无订阅者返回 0）。
        assert_eq!(b.publish("t", &json!(1)).await.unwrap(), 0);

        let cfg = BrokerCfg {
            kind: "local".into(),
            ..Default::default()
        };
        let b2 = build_broker(&Some(cfg)).await.unwrap();
        assert_eq!(b2.kind(), "local");

        // 未知 kind → 报错（非静默退化）。
        let bad = BrokerCfg {
            kind: "nope".into(),
            ..Default::default()
        };
        assert!(build_broker(&Some(bad)).await.is_err());

        // 兜底：Bus 仍可用。
        let _ = Bus::new();
    }
}
