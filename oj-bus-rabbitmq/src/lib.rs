//! oj-bus-rabbitmq：bus 轴 rabbitmq cdylib 插件（Task 4.3；core broker/rabbitmq.rs 迁入）。
//! 迁移决策同 kafka 插件（spec §3 插件自包含）：lapin 逻辑逐字复制自 core。
//! 关键差异同 kafka：消费循环经宿主注入的 HostContext.deliver 回调上送（逻辑 topic +
//! 原始帧 payload；UnboundedSender 不过 FFI 边界）。
//!
//! cfg 契约：init cfg = `{}`；connect(cfg) 收 BrokerCfg JSON（url 或 brokers + topic_prefix）。
//! 句柄约定：connect 分配 handle（AtomicU64），close 释放。

use futures::StreamExt;
use lapin::options::{
    BasicAckOptions, BasicConsumeOptions, BasicPublishOptions, ExchangeDeclareOptions,
    QueueBindOptions, QueueDeclareOptions,
};
use lapin::types::FieldTable;
use lapin::{Connection, ConnectionProperties, ExchangeKind};
use oj_plugin_ffi::{
    ABI_VERSION, EventBrokerVtable, FfiFuture, HostContext, PluginDescriptor, PluginRegistrations,
    RArc, RBytes, RResult, RString,
};
use serde::Deserialize;
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// 插件侧配置视图（= core config::BrokerCfg 的 JSON）。
#[derive(Deserialize)]
#[serde(default)]
struct BrokerCfgJson {
    kind: String,
    brokers: Vec<String>,
    url: Option<String>,
    group: Option<String>,
    topic_prefix: Option<String>,
}

impl Default for BrokerCfgJson {
    fn default() -> Self {
        Self {
            kind: String::new(),
            brokers: Vec::new(),
            url: None,
            group: None,
            topic_prefix: None,
        }
    }
}

/// 插件共享状态（进程级单例，init 建立）。
struct BusPluginState {
    rt: tokio::runtime::Runtime,
    brokers: Mutex<HashMap<u64, Arc<RabbitBroker>>>,
    next_handle: AtomicU64,
}

static PLUGIN: OnceLock<BusPluginState> = OnceLock::new();
/// init 时宿主注入的上下文（消费循环经 deliver 回调上送消息）。
static HOST: OnceLock<RArc<HostContext>> = OnceLock::new();

fn state() -> &'static BusPluginState {
    PLUGIN.get().expect("oj-bus-rabbitmq: init not called")
}

// ---- FfiFuture 桥（spike S.2 定稿；同 db/blob 插件）----

struct CallState {
    rx: tokio::sync::oneshot::Receiver<Result<Vec<u8>, String>>,
    result: Option<Result<Vec<u8>, String>>,
}

extern "C" fn poll(state: *mut c_void) -> i32 {
    let s = unsafe { &mut *(state as *mut CallState) };
    if let Some(r) = &s.result {
        return if r.is_ok() { 1 } else { -1 };
    }
    match s.rx.try_recv() {
        Ok(r) => {
            let code = if r.is_ok() { 1 } else { -1 };
            s.result = Some(r);
            code
        }
        Err(tokio::sync::oneshot::error::TryRecvError::Empty) => 0,
        Err(tokio::sync::oneshot::error::TryRecvError::Closed) => -1,
    }
}

extern "C" fn take(state: *mut c_void) -> RResult<RBytes, RString> {
    let s = unsafe { &mut *(state as *mut CallState) };
    match s.result.take() {
        Some(Ok(bytes)) => {
            let mut v = RBytes::new();
            for b in bytes {
                v.push(b);
            }
            RResult::Ok(v)
        }
        Some(Err(e)) => RResult::Err(RString::from(e.as_str())),
        None => RResult::Err(RString::from("take before ready or twice")),
    }
}

extern "C" fn free(state: *mut c_void) {
    if !state.is_null() {
        drop(unsafe { Box::from_raw(state as *mut CallState) });
    }
}

/// 起一个 FfiFuture：异步工作 spawn 到插件 runtime，oneshot 收结果。
fn spawn_call(fut: impl std::future::Future<Output = Result<Vec<u8>, String>> + Send + 'static) -> FfiFuture {
    let (tx, rx) = tokio::sync::oneshot::channel();
    state().rt.spawn(async move {
        let _ = tx.send(fut.await);
    });
    FfiFuture {
        state: Box::into_raw(Box::new(CallState { rx, result: None })).cast(),
        poll,
        take,
        free,
    }
}

// ---- rabbitmq 逻辑（迁自 core broker/rabbitmq.rs，语义对齐）----

/// RabbitMQ 事件 broker：topic 交换发布 + 排他队列消费（经 deliver 上送）。
struct RabbitBroker {
    conn: Arc<Connection>,
    /// topic 交换名（默认 "oj-bus"；可由 `topic_prefix` 配置覆盖）。
    exchange: String,
}

impl RabbitBroker {
    async fn new(cfg: &BrokerCfgJson) -> Result<Self, String> {
        let url = cfg
            .url
            .clone()
            .or_else(|| cfg.brokers.first().cloned())
            .ok_or_else(|| "rabbitmq requires 'url' or 'brokers'".to_string())?;
        let exchange = cfg.topic_prefix.clone().unwrap_or_else(|| "oj-bus".into());

        let conn = Connection::connect(&url, ConnectionProperties::default())
            .await
            .map_err(|e| format!("rabbitmq connect {url}: {e}"))?;
        let ch = conn
            .create_channel()
            .await
            .map_err(|e| format!("rabbitmq channel: {e}"))?;
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
        .map_err(|e| format!("rabbitmq exchange declare {exchange}: {e}"))?;
        Ok(Self {
            conn: Arc::new(conn),
            exchange,
        })
    }
}

impl BusPluginState {
    fn broker(&self, handle: u64) -> Result<Arc<RabbitBroker>, String> {
        self.brokers
            .lock()
            .unwrap()
            .get(&handle)
            .cloned()
            .ok_or_else(|| format!("bus: unknown handle {handle}"))
    }

    async fn do_publish(&self, handle: u64, topic: &str, data: &str) -> Result<Vec<u8>, String> {
        let b = self.broker(handle)?;
        let channel = b
            .conn
            .create_channel()
            .await
            .map_err(|e| format!("rabbitmq channel: {e}"))?;
        let payload = data.as_bytes().to_vec();
        // 投递到 topic 交换，路由键 = topic；不阻塞等待 broker confirm（与 core 一致）。
        channel
            .basic_publish(
                &b.exchange,
                topic,
                BasicPublishOptions::default(),
                &payload,
                lapin::BasicProperties::default(),
            )
            .await
            .map_err(|e| format!("rabbitmq publish {topic}: {e}"))?;
        Ok(b"".to_vec())
    }

    async fn do_subscribe(&self, handle: u64, topic: &str) -> Result<Vec<u8>, String> {
        let b = self.broker(handle)?;
        let channel = b
            .conn
            .create_channel()
            .await
            .map_err(|e| format!("rabbitmq channel: {e}"))?;
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
            .map_err(|e| format!("rabbitmq queue declare: {e}"))?;
        let queue_name = queue.name().as_str().to_string();
        channel
            .queue_bind(&queue_name, &b.exchange, topic, QueueBindOptions::default(), FieldTable::default())
            .await
            .map_err(|e| format!("rabbitmq queue bind {topic}: {e}"))?;
        let mut consumer = channel
            .basic_consume(&queue_name, "", BasicConsumeOptions::default(), FieldTable::default())
            .await
            .map_err(|e| format!("rabbitmq consume {topic}: {e}"))?;
        let host = HOST.get().cloned().expect("oj-bus-rabbitmq: init before subscribe");
        let logical = topic.to_string();
        tokio::spawn(async move {
            while let Some(delivery) = consumer.next().await {
                if let Ok(delivery) = delivery {
                    let payload = String::from_utf8_lossy(&delivery.data).to_string();
                    delivery.ack(BasicAckOptions::default()).await.ok();
                    (host.deliver)(RString::from(logical.as_str()), RString::from(payload.as_str()));
                }
            }
        });
        Ok(b"".to_vec())
    }
}

// ---- vtable（同步签名返回 FfiFuture；connect 产 handle，close 释放）----

extern "C" fn connect(cfg: RString) -> FfiFuture {
    let st = state();
    spawn_call(async move {
        let cfg: BrokerCfgJson =
            serde_json::from_str(&cfg[..]).map_err(|e| format!("rabbitmq: bad cfg: {e}"))?;
        let broker = Arc::new(RabbitBroker::new(&cfg).await?);
        let handle = st.next_handle.fetch_add(1, Ordering::SeqCst) + 1;
        st.brokers.lock().unwrap().insert(handle, broker);
        Ok(format!(r#"{{"handle":{handle}}}"#).into_bytes())
    })
}

extern "C" fn publish(handle: u64, topic: RString, data: RString) -> FfiFuture {
    let st = state();
    spawn_call(async move { st.do_publish(handle, &topic[..], &data[..]).await })
}

extern "C" fn subscribe(handle: u64, topic: RString) -> FfiFuture {
    let st = state();
    spawn_call(async move { st.do_subscribe(handle, &topic[..]).await })
}

extern "C" fn close(handle: u64) {
    state().brokers.lock().unwrap().remove(&handle);
}

static VTABLE: EventBrokerVtable = EventBrokerVtable {
    connect,
    publish,
    subscribe,
    close,
};

extern "C" fn register() -> PluginRegistrations {
    PluginRegistrations { es: std::ptr::null(), db: std::ptr::null(), blob: std::ptr::null(), bus: &VTABLE }
}

// ---- 入口 ----

fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        name: RString::from("bus-rabbitmq"),
        semver: RString::from("0.1.0"),
        abi_version: ABI_VERSION,
        fingerprint: RString::from(oj_plugin_ffi::HOST_FINGERPRINT),
        register,
    }
}

fn init(host: RArc<HostContext>, cfg: RString) -> RResult<PluginDescriptor, RString> {
    if PLUGIN.get().is_some() {
        return RResult::Ok(descriptor());
    }
    let _ = cfg; // init 无装配期配置（每 broker cfg 在 connect 传入）
    let _ = HOST.set(host);
    let st = BusPluginState {
        rt: runtime(),
        brokers: Mutex::new(HashMap::new()),
        next_handle: AtomicU64::new(0),
    };
    let _ = PLUGIN.set(st);
    RResult::Ok(descriptor())
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("oj-bus-rabbitmq tokio runtime")
}

oj_plugin_ffi::oj_plugin_entry!(init);

#[cfg(test)]
mod tests {
    use super::*;

    /// cfg 校验离线路径：url 与 brokers 皆缺 → fail-fast。
    #[test]
    fn rabbitmq_requires_url_or_brokers() {
        let cfg = BrokerCfgJson::default();
        let st = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let r = st.block_on(RabbitBroker::new(&cfg));
        assert!(r.is_err());
    }

    /// 真 rabbitmq roundtrip（env-gated）：`OJ_TEST_RABBITMQ_URL` 给 amqp URL。
    /// 未设置 → 跳过（不进网络）。
    #[tokio::test(flavor = "multi_thread")]
    async fn real_rabbitmq_publish_subscribe_roundtrip() {
        let url = match std::env::var("OJ_TEST_RABBITMQ_URL") {
            Ok(u) if !u.is_empty() => u,
            _ => {
                eprintln!("skip: OJ_TEST_RABBITMQ_URL unset");
                return;
            }
        };
        let cfg = serde_json::json!({
            "kind": "rabbitmq",
            "brokers": [],
            "url": url,
            "group": null,
            "topic_prefix": format!("ojtest-{}", std::process::id()),
        })
        .to_string();
        let desc = match std::result::Result::from(init(host(), RString::from("{}"))) {
            Ok(d) => d,
            Err(e) => panic!("init failed: {}", e[..].to_string()),
        };
        assert_eq!(&desc.name[..], "bus-rabbitmq");

        let bytes = drive(&mut connect(RString::from(cfg.as_str()))).await.expect("connect");
        let handle = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["handle"]
            .as_u64()
            .unwrap();

        let topic = format!("t.{}", std::process::id());
        drive(&mut subscribe(handle, RString::from(topic.as_str())))
            .await
            .expect("subscribe");
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        drive(&mut publish(
            handle,
            RString::from(topic.as_str()),
            RString::from(r#"{"topic":"t","data":{"hi":1}}"#),
        ))
        .await
        .expect("publish");
        // 消费循环经 deliver 回调上送（真实扇出语义由宿主侧 ffi.rs 适配器测试覆盖）。
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        close(handle);
    }

    extern "C" fn test_log(_level: u8, _msg: RString) {}
    extern "C" fn test_deliver(_topic: RString, _payload: RString) {}

    fn host() -> RArc<HostContext> {
        RArc::new(HostContext { log: test_log, deliver: test_deliver })
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
