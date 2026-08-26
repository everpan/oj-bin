//! 分布式事件 broker 工厂（本地 Bus 保留；Kafka/RabbitMQ 已迁插件，Task 4.3）。
//!
//! 统一契约在 `super::bus::EventBroker`：进程内 `Bus` 与插件 `FfiEventBroker`
//! （oj-bus-kafka / oj-bus-rabbitmq）皆实现之，经 `BusBackendRegistry` 按 kind 装配。
//! `build_broker` 保持签名（薄包装 builtin()），供缺省/测试零插件场景；真实装配在
//! server_cmd 经 Registries.bus（内置 local + 插件工厂）连接。

use std::sync::Arc;

use super::BridgeResult;
use super::bus::EventBroker;
use crate::config::BrokerCfg;

/// 按配置构造事件 broker（薄包装：BusBackendRegistry::builtin().connect，签名不变，
/// 调用点零改动；仅内置 local——插件 broker 场景走装配期 Registries.bus）。
pub async fn build_broker(cfg: &Option<BrokerCfg>) -> BridgeResult<Arc<dyn EventBroker>> {
    super::bus_backend::BusBackendRegistry::builtin()
        .connect(cfg)
        .await
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
