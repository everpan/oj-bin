//! bus 轴后端工厂（键选式注册表）：按 broker.kind 单选单一后端（spec §2）。
//! Kafka/RabbitMQ 注册化：feature 未启用时注册占位后端，connect 报"需要 cargo
//! feature"指引（不退化为 unknown kind，保持现状错误文案质量）。

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
    /// 内置：local 零依赖；kafka/rabbitmq 按 feature 注册（未启用 = 占位报错后端）。
    pub fn builtin() -> Self {
        let mut r = Self::new();
        r.register(Arc::new(LocalBusBackend)).unwrap();
        #[cfg(feature = "kafka")]
        r.register(Arc::new(KafkaBusBackend)).unwrap();
        #[cfg(not(feature = "kafka"))]
        r.register(Arc::new(FeatureGatedBackend("kafka"))).unwrap();
        #[cfg(feature = "rabbitmq")]
        r.register(Arc::new(RabbitMqBusBackend)).unwrap();
        #[cfg(not(feature = "rabbitmq"))]
        r.register(Arc::new(FeatureGatedBackend("rabbitmq"))).unwrap();
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

/// feature 未启用的占位后端：connect 时报"需要 cargo feature"指引。
pub struct FeatureGatedBackend(&'static str);
#[async_trait]
impl BusBackend for FeatureGatedBackend {
    fn kind(&self) -> &str {
        self.0
    }
    async fn connect(&self, _cfg: &BrokerCfg) -> BridgeResult<Arc<dyn EventBroker>> {
        Err(format!(
            "broker kind '{}' requires the '{}' cargo feature (build with --features {})",
            self.0, self.0, self.0
        )
        .into())
    }
}

#[cfg(feature = "kafka")]
pub struct KafkaBusBackend;
#[cfg(feature = "kafka")]
#[async_trait]
impl BusBackend for KafkaBusBackend {
    fn kind(&self) -> &str {
        "kafka"
    }
    async fn connect(&self, cfg: &BrokerCfg) -> BridgeResult<Arc<dyn EventBroker>> {
        Ok(Arc::new(super::broker::kafka::KafkaBroker::new(cfg).await?))
    }
}

#[cfg(feature = "rabbitmq")]
pub struct RabbitMqBusBackend;
#[cfg(feature = "rabbitmq")]
#[async_trait]
impl BusBackend for RabbitMqBusBackend {
    fn kind(&self) -> &str {
        "rabbitmq"
    }
    async fn connect(&self, cfg: &BrokerCfg) -> BridgeResult<Arc<dyn EventBroker>> {
        Ok(Arc::new(super::broker::rabbitmq::RabbitMqBroker::new(cfg).await?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BrokerCfg;

    #[tokio::test(flavor = "current_thread")]
    async fn registry_connects_local_by_default_and_kind() {
        let r = BusBackendRegistry::builtin();
        let b = r.connect(&None).await.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(b.kind(), "local");
        let cfg = BrokerCfg { kind: "local".into(), ..Default::default() };
        let b2 = r.connect(&Some(cfg)).await.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(b2.kind(), "local");
        assert!(r.kinds().contains(&"local".to_string()));
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

    #[cfg(not(feature = "kafka"))]
    #[tokio::test(flavor = "current_thread")]
    async fn kafka_without_feature_reports_feature_hint() {
        // 未启用 feature 时错误文案保留"需要 cargo feature"指引（不退化为 unknown kind）。
        let r = BusBackendRegistry::builtin();
        let cfg = BrokerCfg { kind: "kafka".into(), ..Default::default() };
        let msg = match r.connect(&Some(cfg)).await {
            Err(e) => e.to_string(),
            Ok(_) => panic!("kafka without feature must fail"),
        };
        assert!(msg.contains("kafka") && msg.contains("feature"), "{msg}");
    }

    #[test]
    fn duplicate_kind_fails() {
        let mut r = BusBackendRegistry::builtin();
        assert!(r.register(Arc::new(LocalBusBackend)).is_err());
    }
}
