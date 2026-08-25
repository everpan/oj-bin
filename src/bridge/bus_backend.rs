//! bus 轴后端工厂（键选式注册表）：按 broker.kind 单选单一后端（spec §2）。
//! Kafka/RabbitMQ 已迁插件（Task 4.3，oj-bus-kafka / oj-bus-rabbitmq cdylib）；
//! 内置只留 local 零依赖 Bus。插件未装而配置声明对应 kind → "unknown broker kind"
//! 明确报错（列出已知 kind；不复用旧的"需要 cargo feature"占位）。

use std::sync::Arc;

use async_trait::async_trait;

use super::BridgeResult;
use super::bus::EventBroker;
use super::named_registry::NamedRegistry;
use crate::config::BrokerCfg;

/// bus 轴后端工厂（键选式，按配置 broker.kind 单选，跨 actor 池/WS 共享语义不变）。
#[async_trait]
pub trait BusBackend: Send + Sync {
    fn kind(&self) -> &str;
    async fn connect(&self, cfg: &BrokerCfg) -> BridgeResult<Arc<dyn EventBroker>>;
}

/// kind → 工厂查表；重名 kind 注册 fail fast（NamedRegistry 语义）。
pub struct BusBackendRegistry {
    inner: NamedRegistry<dyn BusBackend>,
}

impl BusBackendRegistry {
    pub fn new() -> Self {
        Self { inner: NamedRegistry::new() }
    }
    /// 内置：local 零依赖（kafka/rabbitmq 经插件工厂注册进装配期 registry，Task 4.3）。
    pub fn builtin() -> Self {
        let mut r = Self::new();
        r.register(Arc::new(LocalBusBackend)).unwrap();
        r
    }
    /// 重名 kind → fail fast。
    pub fn register(&mut self, b: Arc<dyn BusBackend>) -> BridgeResult<()> {
        let kind = b.kind().to_string();
        self.inner.register(&kind, b)
    }
    /// None / 空 kind / "local" → 进程内 Bus；未知 kind → 报错列已知 kind。
    pub async fn connect(&self, cfg: &Option<BrokerCfg>) -> BridgeResult<Arc<dyn EventBroker>> {
        let empty = BrokerCfg::default();
        let c = cfg.as_ref().unwrap_or(&empty);
        let kind = if c.kind.is_empty() { "local" } else { c.kind.as_str() };
        match self.inner.get(kind) {
            Some(b) => b.connect(c).await,
            None => {
                let known: Vec<_> = self.kinds();
                Err(format!("unknown broker kind '{kind}' (known: {known:?})").into())
            }
        }
    }
    pub fn kinds(&self) -> Vec<String> {
        self.inner.names().map(str::to_string).collect()
    }
}

impl Default for BusBackendRegistry {
    fn default() -> Self {
        Self::new()
    }
}

pub struct LocalBusBackend;
#[async_trait]
impl BusBackend for LocalBusBackend {
    fn kind(&self) -> &str {
        "local"
    }
    async fn connect(&self, _cfg: &BrokerCfg) -> BridgeResult<Arc<dyn EventBroker>> {
        Ok(Arc::new(super::bus::Bus::new()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn registry_connects_local_by_default_and_kind() {
        let r = BusBackendRegistry::builtin();
        let b = r.connect(&None).await.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(b.kind(), "local");
        let cfg = BrokerCfg { kind: "local".into(), ..Default::default() };
        let b2 = r.connect(&Some(cfg)).await.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(b2.kind(), "local");
        assert!(r.kinds().contains(&"local".to_string()));
        // Task 4.3：kafka/rabbitmq 已迁插件，内置不再注册（缺装 → unknown kind）。
        assert!(!r.kinds().contains(&"kafka".to_string()));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unknown_kind_errors_with_known_list() {
        let r = BusBackendRegistry::builtin();
        let cfg = BrokerCfg { kind: "nats".into(), ..Default::default() };
        let msg = match r.connect(&Some(cfg)).await {
            Err(e) => e.to_string(),
            Ok(_) => panic!("unknown kind must fail"),
        };
        assert!(msg.contains("unknown broker kind 'nats'"), "{msg}");
        assert!(msg.contains("local"), "{msg}"); // known 列表透出
    }

    /// kafka kind 但插件未装 → "unknown broker kind"（列出已知 kind；缺装指引在装配层）。
    #[tokio::test(flavor = "current_thread")]
    async fn kafka_without_plugin_reports_unknown_kind() {
        let r = BusBackendRegistry::builtin();
        let cfg = BrokerCfg { kind: "kafka".into(), ..Default::default() };
        let msg = match r.connect(&Some(cfg)).await {
            Err(e) => e.to_string(),
            Ok(_) => panic!("kafka without plugin must fail"),
        };
        assert!(msg.contains("unknown broker kind 'kafka'"), "{msg}");
    }

    #[test]
    fn duplicate_kind_fails() {
        let mut r = BusBackendRegistry::builtin();
        assert!(r.register(Arc::new(LocalBusBackend)).is_err());
    }
}
