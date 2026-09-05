//! oj-bus-kafka：bus 轴 kafka cdylib 插件（Task 4.3；core broker/kafka.rs 迁入）。
//! 迁移决策同 db/blob 插件（spec §3 插件自包含）：rdkafka 逻辑逐字复制自 core。
//! 关键差异：core 的 subscribe 直接把消息 tx.send 给本地 WS 通道；FFI 版经宿主注入的
//! HostContext.deliver 回调上送（UnboundedSender 不过 FFI 边界，spec §3 回调注入条）。
//! deliver 回调按**逻辑 topic** 上送（宿主按 topic 扇出到本地订阅通道）。
//!
//! cfg 契约：init cfg = `{}`；connect(cfg) 收 BrokerCfg JSON（brokers/group/topic_prefix）。
//! 句柄约定：connect 分配 handle（AtomicU64），close 释放。

use futures::StreamExt;
use oj_plugin_ffi::{
    ABI_VERSION, EventBrokerVtable, FfiFuture, HostContext, PluginDescriptor, RArc, RResult,
    RString,
};
use rdkafka::Message;
use rdkafka::config::ClientConfig;
use rdkafka::consumer::{Consumer, StreamConsumer};
use rdkafka::producer::{FutureProducer, FutureRecord};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// 插件侧配置视图（= core config::BrokerCfg 的 JSON）。
#[derive(Deserialize, Default)]
#[serde(default)]
struct BrokerCfgJson {
    kind: String,
    brokers: Vec<String>,
    url: Option<String>,
    group: Option<String>,
    topic_prefix: Option<String>,
}

/// 插件共享状态（进程级单例，init 建立）。
struct BusPluginState {
    rt: tokio::runtime::Runtime,
    brokers: Mutex<HashMap<u64, Arc<KafkaBroker>>>,
    next_handle: AtomicU64,
}

static PLUGIN: OnceLock<BusPluginState> = OnceLock::new();
/// init 时宿主注入的上下文（消费循环经 deliver 回调上送消息）。
static HOST: OnceLock<RArc<HostContext>> = OnceLock::new();

fn state() -> &'static BusPluginState {
    PLUGIN.get().expect("oj-bus-kafka: init not called")
}

// ---- FfiFuture 桥（统一走 oj-plugin-ffi 的 catch_unwind 安全工厂：spawn_ffi_future / catch_future）----

// ---- kafka 逻辑（迁自 core broker/kafka.rs，语义对齐）----

/// Kafka 事件 broker：FutureProducer 发布 + StreamConsumer 消费（经 deliver 上送）。
struct KafkaBroker {
    producer: Arc<FutureProducer>,
    consumer_cfg: ClientConfig,
    topic_prefix: String,
}

impl KafkaBroker {
    fn new(cfg: &BrokerCfgJson) -> Result<Self, String> {
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

impl BusPluginState {
    fn broker(&self, handle: u64) -> Result<Arc<KafkaBroker>, String> {
        self.brokers
            .lock()
            .unwrap()
            .get(&handle)
            .cloned()
            .ok_or_else(|| format!("bus: unknown handle {handle}"))
    }

    async fn do_publish(&self, handle: u64, topic: &str, data: &str) -> Result<Vec<u8>, String> {
        let b = self.broker(handle)?;
        let physical = b.topic_of(topic);
        b.producer
            .send(
                FutureRecord::to(&physical).payload(data).key(&physical),
                std::time::Duration::from_secs(5),
            )
            .await
            .map_err(|(e, _)| format!("kafka publish {physical}: {e}"))?;
        Ok(b"".to_vec())
    }

    /// 起消费循环：收到消息经宿主 deliver 上送（逻辑 topic + 原始帧 payload）。
    async fn do_subscribe(&self, handle: u64, topic: &str) -> Result<Vec<u8>, String> {
        let b = self.broker(handle)?;
        let physical = b.topic_of(topic);
        let consumer: StreamConsumer = b
            .consumer_cfg
            .create()
            .map_err(|e| format!("kafka consumer {physical}: {e}"))?;
        consumer
            .subscribe(&[&physical])
            .map_err(|e| format!("kafka subscribe {physical}: {e}"))?;
        let host = HOST
            .get()
            .cloned()
            .expect("oj-bus-kafka: init before subscribe");
        let logical = topic.to_string();
        // 将 consumer 移入任务：MessageStream 借用 consumer，须同生命周期存活于任务内。
        tokio::spawn(async move {
            let mut stream = consumer.stream();
            while let Some(msg) = stream.next().await {
                match msg {
                    Ok(m) => {
                        let Some(p) = m.payload() else { continue };
                        let payload = String::from_utf8_lossy(p).to_string();
                        // 宿主按逻辑 topic 扇出；非阻塞投递（宿主 tx.send）。
                        (host.deliver)(
                            RString::from(logical.as_str()),
                            RString::from(payload.as_str()),
                        );
                    }
                    Err(e) => {
                        eprintln!("[oj-bus-kafka] consume error on {physical}: {e}");
                    }
                }
            }
        });
        Ok(b"".to_vec())
    }
}

// ---- vtable（同步签名返回 FfiFuture；connect 产 handle，close 释放）----

extern "C" fn connect(cfg: RString) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(&st.rt, async move {
            let cfg: BrokerCfgJson =
                serde_json::from_str(&cfg[..]).map_err(|e| format!("kafka: bad cfg: {e}"))?;
            let broker = Arc::new(KafkaBroker::new(&cfg)?);
            let handle = st.next_handle.fetch_add(1, Ordering::SeqCst) + 1;
            st.brokers.lock().unwrap().insert(handle, broker);
            Ok(format!(r#"{{"handle":{handle}}}"#).into_bytes())
        })
    })
}

extern "C" fn publish(handle: u64, topic: RString, data: RString) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(&st.rt, async move {
            st.do_publish(handle, &topic[..], &data[..]).await
        })
    })
}

extern "C" fn subscribe(handle: u64, topic: RString) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(
            &st.rt,
            async move { st.do_subscribe(handle, &topic[..]).await },
        )
    })
}

extern "C" fn close(handle: u64) {
    oj_plugin_ffi::catch_void(|| {
        state().brokers.lock().unwrap().remove(&handle);
    })
}

static VTABLE: EventBrokerVtable = EventBrokerVtable {
    connect,
    publish,
    subscribe,
    close,
};

// ---- 入口 ----

fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        name: RString::from("bus-kafka"),
        semver: RString::from(env!("CARGO_PKG_VERSION")),
        abi_version: ABI_VERSION,
        fingerprint: RString::from(oj_plugin_ffi::HOST_FINGERPRINT),
        desc: RString::from(
            "bus 轴 kafka cdylib 插件：rdkafka 迁自 core broker/kafka.rs（Task 4.3）",
        ),
    }
}

fn init(host: RArc<HostContext>, cfg: RString) -> RResult<PluginDescriptor, RString> {
    if PLUGIN.get().is_some() {
        return RResult::Ok(descriptor());
    }
    let _ = cfg; // init 无装配期配置（每 broker cfg 在 connect 传入）
    // get_or_init：并发 init 时闭包只跑一次（竞争方阻塞复用），不重复建 runtime，
    // 避免 `let _ = set(st)` 在竞争下把败者的 tokio Runtime 从 async 上下文 drop 崩溃。
    // HOST 随闭包同设一次；并发下 set 失败丢弃的 RArc 无 runtime，无害。
    PLUGIN.get_or_init(|| {
        let _ = HOST.set(host);
        BusPluginState {
            rt: runtime(),
            brokers: Mutex::new(HashMap::new()),
            next_handle: AtomicU64::new(0),
        }
    });
    RResult::Ok(descriptor())
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("oj-bus-kafka tokio runtime")
}

oj_plugin_ffi::oj_plugin_entry!(init, bus => &VTABLE);

#[cfg(test)]
mod tests {
    use super::*;

    /// cfg 校验离线路径：brokers 缺失 fail-fast。
    #[test]
    fn kafka_requires_brokers() {
        let cfg = BrokerCfgJson::default();
        assert!(KafkaBroker::new(&cfg).is_err());
        let cfg = BrokerCfgJson {
            brokers: vec!["127.0.0.1:9092".into()],
            ..Default::default()
        };
        // 仅 brokers 可构造（rdkafka create 离线构造；连接按需）。
        assert!(KafkaBroker::new(&cfg).is_ok());
    }

    /// 真 kafka roundtrip（env-gated）：`OJ_TEST_KAFKA_BROKERS` 给逗号分隔 bootstrap servers。
    /// 未设置 → 跳过（不进网络）。
    #[tokio::test(flavor = "multi_thread")]
    async fn real_kafka_publish_subscribe_roundtrip() {
        let brokers = match std::env::var("OJ_TEST_KAFKA_BROKERS") {
            Ok(b) if !b.is_empty() => b,
            _ => {
                eprintln!("skip: OJ_TEST_KAFKA_BROKERS unset");
                return;
            }
        };
        let cfg = serde_json::json!({
            "kind": "kafka",
            "brokers": brokers.split(',').map(|s| s.trim()).collect::<Vec<_>>(),
            "group": "oj-test",
            "topic_prefix": format!("ojtest-{}", std::process::id()),
            "url": null,
        })
        .to_string();
        let desc = match std::result::Result::from(init(host(), RString::from("{}"))) {
            Ok(d) => d,
            Err(e) => panic!("init failed: {}", &e[..]),
        };
        assert_eq!(&desc.name[..], "bus-kafka");

        let bytes = drive(&mut connect(RString::from(cfg.as_str())))
            .await
            .expect("connect");
        let handle = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["handle"]
            .as_u64()
            .unwrap();

        let topic = format!("t.{}", std::process::id());
        drive(&mut subscribe(handle, RString::from(topic.as_str())))
            .await
            .expect("subscribe");
        // 等消费者就绪
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        drive(&mut publish(
            handle,
            RString::from(topic.as_str()),
            RString::from(r#"{"topic":"t","data":{"hi":1}}"#),
        ))
        .await
        .expect("publish");

        // 消费循环经 deliver 回调把消息上送（此处 host 的 deliver 为测试桩，记录即可——
        // 真实扇出语义由宿主侧 ffi.rs 适配器测试覆盖；本测试验证 vtable 通路不 panic）。
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        close(handle);
    }

    extern "C" fn test_log(_level: u8, _msg: RString) {}
    extern "C" fn test_deliver(_topic: RString, _payload: RString) {}

    fn host() -> RArc<HostContext> {
        RArc::new(HostContext {
            log: test_log,
            deliver: test_deliver,
        })
    }

    /// FfiFuture → 测试异步桥（等价 core await_ffi 的 poll 轮询）。
    async fn drive(fut: &mut FfiFuture) -> Result<Vec<u8>, String> {
        for _ in 0..100_000 {
            match (fut.poll)(fut.state) {
                0 => tokio::task::yield_now().await,
                code => {
                    let r = (fut.take)(fut.state);
                    (fut.free)(fut.state);
                    fut.state = std::ptr::null_mut();
                    return match (code, std::result::Result::from(r)) {
                        (1, Ok(b)) => Ok(b.iter().copied().collect()),
                        (_, Err(e)) => Err(e[..].to_string()),
                        _ => Err("ffi drive timeout".into()),
                    };
                }
            }
        }
        Err("ffi drive timeout".into())
    }
}
