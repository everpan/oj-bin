//! oj-bus-kafka：kafka cdylib 插件，双轴——bus 轴（既有语义零变化）+ mq 轴（命名客户端）。
//! 共底层（spec 2026-09-07 §2）：一个 KafkaCore driver（rdkafka 连接/生产/cfg 解析），
//! bus / mq 两个薄适配面；消费路径分列——bus 面 push 扇出（deliver 回调，auto-commit），
//! mq 面 pull 消费会话（显式 commit，at-least-once）。
//!
//! cfg 契约：init cfg = `{}`；bus connect(cfg) 收 BrokerCfg JSON（brokers/group/topic_prefix）；
//! mq connect(cfg) 额外要求 `kind == "kafka"`（装配层按段注入，不符 → Err fail-fast）。
//! 句柄约定：bus / mq 两面 handle **分开编号**（各自 AtomicU64 计数 + 各自 map）。
//!
//! 评审 S1 修复：bus close 现在停掉 detach 的 push 消费任务（watch 信号 + 发送端随
//! 实例 drop），不再只删 map 致 consumer 任务泄漏——这是修复，非行为回归。

use futures::StreamExt;
use oj_plugin_ffi::{
    ABI_VERSION, EventBrokerVtable, FfiFuture, HostContext, MqMessage, MqVtable, PluginDescriptor,
    RArc, RResult, RString,
};
use rdkafka::Message;
use rdkafka::config::ClientConfig;
use rdkafka::consumer::{CommitMode, Consumer, StreamConsumer};
use rdkafka::message::{Header, Headers, OwnedHeaders, Timestamp};
use rdkafka::producer::{FutureProducer, FutureRecord, Producer};
use rdkafka::topic_partition_list::{Offset, TopicPartitionList};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

/// 插件侧配置视图（= core config::BrokerCfg 的 JSON + mq 的 kind 注入）。
#[derive(Deserialize, Default)]
#[serde(default)]
struct BrokerCfgJson {
    kind: String,
    brokers: Vec<String>,
    url: Option<String>,
    group: Option<String>,
    topic_prefix: Option<String>,
}

// ---- KafkaCore：共底层 driver（连接/生产/cfg 解析；bus 与 mq 两面共享）----

struct KafkaCore {
    brokers: String,
    group: String,
    producer: FutureProducer,
}

impl KafkaCore {
    fn new(cfg: &BrokerCfgJson) -> Result<Self, String> {
        let brokers = cfg.brokers.join(",");
        if brokers.is_empty() {
            return Err("kafka requires 'brokers' (comma-separated bootstrap servers)".into());
        }
        let group = cfg.group.clone().unwrap_or_else(|| "oj-bus".into());
        // cfg 校验 fail-fast（spec §7）；rdkafka create 离线构造、可达性惰性由任务监督兜底。
        let producer: FutureProducer = ClientConfig::new()
            .set("bootstrap.servers", &brokers)
            .set("message.timeout.ms", "5000")
            .create()
            .map_err(|e| format!("kafka producer: {e}"))?;
        Ok(Self {
            brokers,
            group,
            producer,
        })
    }

    fn bus_consumer_cfg(&self) -> ClientConfig {
        // bus 面维持现状：auto-commit（push 扇出语义）。
        let mut c = ClientConfig::new();
        c.set("bootstrap.servers", &self.brokers)
            .set("group.id", &self.group)
            .set("enable.auto.commit", "true")
            .set("auto.offset.reset", "earliest");
        c
    }

    fn mq_consumer_cfg(&self) -> ClientConfig {
        // mq 面：显式 commit（at-least-once）——auto.commit 关，
        // 处理完由 JS 显式 commit（评审 S1：消费路径分列）。
        let mut c = ClientConfig::new();
        c.set("bootstrap.servers", &self.brokers)
            .set("group.id", &self.group)
            .set("enable.auto.commit", "false")
            .set("auto.offset.reset", "earliest");
        c
    }

    /// mq 面 send（唯一发送 method；rabbit 的 publish 同型不同 payload，见 oj-bus-rabbitmq）。
    async fn send(&self, req: SendReq) -> Result<Vec<u8>, String> {
        let payload = req.value.to_string();
        let mut record = FutureRecord::to(&req.topic).payload(payload.as_str());
        record = match &req.key {
            Some(k) => record.key(k.as_str()),
            None => record,
        };
        record = match req.partition {
            Some(p) => record.partition(p),
            None => record,
        };
        if !req.headers.is_empty() {
            let mut h = OwnedHeaders::new();
            for (k, v) in &req.headers {
                h = h.insert(Header {
                    key: k.as_str(),
                    value: Some(v.as_str()),
                });
            }
            record = record.headers(h);
        }
        self.producer
            .send(record, std::time::Duration::from_secs(5))
            .await
            .map_err(|(e, _)| format!("kafka send {}: {e}", req.topic))?;
        Ok(br#"{"sent":1}"#.to_vec())
    }

    /// mq 面 metadata（可选 method）：topic 概览。同步 API → spawn_blocking。
    async fn metadata(&self) -> Result<Vec<u8>, String> {
        let producer = self.producer.clone(); // 内部 Arc，clone 低廉
        let md = tokio::task::spawn_blocking(move || {
            let client = producer.client();
            client
                .fetch_metadata(None, std::time::Duration::from_secs(5))
                .map_err(|e| format!("kafka metadata: {e}"))
                .map(|md| {
                    let topics: Vec<Value> = md
                        .topics()
                        .iter()
                        .map(|t| Value::String(t.name().to_string()))
                        .collect();
                    serde_json::json!({ "kind": "kafka", "topics": topics })
                })
        })
        .await
        .map_err(|e| format!("kafka metadata join: {e}"))??;
        Ok(md.to_string().into_bytes())
    }
}

// ---- mq 消息契约（spec §4 统一形态）----

#[derive(serde::Deserialize)]
struct SendReq {
    topic: String,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    partition: Option<i32>,
    #[serde(default)]
    headers: HashMap<String, String>,
    value: Value,
}

#[derive(serde::Deserialize)]
struct PollReq {
    topics: Vec<String>,
    #[serde(default = "d_max")]
    max: usize,
    // JS 面是 camelCase（timeoutMs）——rename 对齐；alias 兼容 snake_case 直调。
    #[serde(rename = "timeoutMs", alias = "timeout_ms", default = "d_timeout")]
    timeout_ms: u64,
}
fn d_max() -> usize {
    100
}
fn d_timeout() -> u64 {
    1000
}

// MqMessage 使用契约 crate 的共享词汇表（oj_plugin_ffi::mq::MqMessage）。

// ---- bus 面：push 扇出（既有语义零变化）----

struct BusInstance {
    core: Arc<KafkaCore>,
    topic_prefix: String,
    /// close = drop 发送端 → 所有 clone 的 Receiver 收到 closed → 消费任务退出
    /// （评审 S1 泄漏修复；Receiver 一份留在实例内供 close 前检查，一份随任务）。
    stop_tx: tokio::sync::watch::Sender<bool>,
    consumer_cfg: ClientConfig,
}

impl BusInstance {
    fn new(cfg: &BrokerCfgJson) -> Result<Self, String> {
        let core = Arc::new(KafkaCore::new(cfg)?);
        let (stop_tx, _) = tokio::sync::watch::channel(false);
        let consumer_cfg = core.bus_consumer_cfg();
        Ok(Self {
            core,
            topic_prefix: cfg.topic_prefix.clone().unwrap_or_default(),
            stop_tx,
            consumer_cfg,
        })
    }

    fn topic_of(&self, topic: &str) -> String {
        if self.topic_prefix.is_empty() {
            topic.to_string()
        } else {
            format!("{}.{}", self.topic_prefix, topic)
        }
    }
}

// ---- mq 面：pull 消费会话（显式 commit）----

struct MqInstance {
    core: Arc<KafkaCore>,
    /// lazy 建立的 pull 消费会话（首次 poll 建立；close 即 drop = 离开消费组）。
    session: tokio::sync::Mutex<Option<Arc<StreamConsumer>>>,
    /// 单 poller 互斥（插件侧保底；宿主任务上下文门禁是第一道，评审 M2）。
    poller: tokio::sync::Mutex<()>,
}

impl MqInstance {
    fn new(cfg: &BrokerCfgJson) -> Result<Self, String> {
        let core = Arc::new(KafkaCore::new(cfg)?);
        Ok(Self {
            core,
            session: tokio::sync::Mutex::new(None),
            poller: tokio::sync::Mutex::new(()),
        })
    }

    async fn session(&self, topics: &[String]) -> Result<Arc<StreamConsumer>, String> {
        let mut g = self.session.lock().await;
        match g.as_ref() {
            Some(c) => Ok(c.clone()),
            None => {
                let consumer: StreamConsumer = self
                    .core
                    .mq_consumer_cfg()
                    .create()
                    .map_err(|e| format!("kafka consumer: {e}"))?;
                let refs: Vec<&str> = topics.iter().map(|s| s.as_str()).collect();
                consumer
                    .subscribe(&refs)
                    .map_err(|e| format!("kafka subscribe {topics:?}: {e}"))?;
                let c = Arc::new(consumer);
                *g = Some(c.clone());
                Ok(c)
            }
        }
    }

    /// poll：max 条或 timeout_ms 到期先到为准；消息转统一 MqMessage 形态。
    async fn poll(&self, req: PollReq) -> Result<Vec<u8>, String> {
        let _guard = self.poller.lock().await; // 单 poller（插件侧保底）
        let consumer = self.session(&req.topics).await?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(req.timeout_ms);
        let mut msgs: Vec<MqMessage> = Vec::new();
        while msgs.len() < req.max {
            let remain = deadline.saturating_duration_since(std::time::Instant::now());
            if remain.is_zero() {
                break;
            }
            match tokio::time::timeout(remain, consumer.recv()).await {
                Ok(Ok(m)) => {
                    let mut headers = HashMap::new();
                    if let Some(hs) = m.headers() {
                        for i in 0..hs.count() {
                            let h = hs.get(i);
                            if let Some(v) = h.value {
                                headers.insert(
                                    h.key.to_string(),
                                    String::from_utf8_lossy(v).to_string(),
                                );
                            }
                        }
                    }
                    let value = match m.payload() {
                        Some(p) => serde_json::from_slice(p).unwrap_or_else(|_| {
                            Value::String(String::from_utf8_lossy(p).into_owned())
                        }),
                        None => Value::Null,
                    };
                    let ts = match m.timestamp() {
                        Timestamp::CreateTime(ms) | Timestamp::LogAppendTime(ms) => ms,
                        Timestamp::NotAvailable => 0,
                    };
                    msgs.push(MqMessage {
                        topic: m.topic().to_string(),
                        partition: Some(m.partition()),
                        offset: Some(m.offset()),
                        key: m.key().map(|k| String::from_utf8_lossy(k).to_string()),
                        value,
                        headers,
                        ts,
                        delivery_tag: None,
                    });
                }
                Ok(Err(e)) => return Err(format!("kafka poll: {e}")),
                Err(_) => break, // 到期
            }
        }
        Ok(serde_json::json!({ "messages": msgs })
            .to_string()
            .into_bytes())
    }

    /// commit：显式 TPL 提交 offset+1（spawn_blocking，rdkafka 同步 API）。
    async fn commit(&self, msg: MqMessage) -> Result<Vec<u8>, String> {
        let consumer = {
            let g = self.session.lock().await;
            g.as_ref().cloned().ok_or_else(|| {
                "kafka commit: no active consumer session (poll first)".to_string()
            })?
        };
        let topic = msg.topic.clone();
        let (partition, offset) = (msg.partition.unwrap_or(0), msg.offset.unwrap_or(0) + 1);
        tokio::task::spawn_blocking(move || {
            let mut tpl = TopicPartitionList::new();
            tpl.add_partition_offset(&topic, partition, Offset::Offset(offset))
                .map_err(|e| format!("kafka tpl: {e}"))?;
            consumer
                .commit(&tpl, CommitMode::Sync)
                .map_err(|e| format!("kafka commit: {e}"))
        })
        .await
        .map_err(|e| format!("kafka commit join: {e}"))??;
        Ok(b"{}".to_vec())
    }
}

// ---- 插件共享状态（bus / mq 两面 handle 分开编号）----

struct BusPluginState {
    rt: tokio::runtime::Runtime,
    bus: Mutex<HashMap<u64, Arc<BusInstance>>>,
    mq: Mutex<HashMap<u64, Arc<MqInstance>>>,
    next_bus: AtomicU64,
    next_mq: AtomicU64,
}

static PLUGIN: OnceLock<BusPluginState> = OnceLock::new();
/// init 时宿主注入的上下文（bus 消费循环经 deliver 回调上送消息）。
static HOST: OnceLock<RArc<HostContext>> = OnceLock::new();

fn state() -> &'static BusPluginState {
    PLUGIN.get().expect("oj-bus-kafka: init not called")
}

impl BusPluginState {
    fn bus_broker(&self, handle: u64) -> Result<Arc<BusInstance>, String> {
        self.bus
            .lock()
            .unwrap()
            .get(&handle)
            .cloned()
            .ok_or_else(|| format!("bus: unknown handle {handle}"))
    }

    fn mq_instance(&self, handle: u64) -> Result<Arc<MqInstance>, String> {
        self.mq
            .lock()
            .unwrap()
            .get(&handle)
            .cloned()
            .ok_or_else(|| format!("mq: unknown handle {handle}"))
    }

    async fn do_publish(&self, handle: u64, topic: &str, data: &str) -> Result<Vec<u8>, String> {
        let b = self.bus_broker(handle)?;
        let physical = b.topic_of(topic);
        b.core
            .producer
            .send(
                FutureRecord::to(&physical).payload(data).key(&physical),
                std::time::Duration::from_secs(5),
            )
            .await
            .map_err(|(e, _)| format!("kafka publish {physical}: {e}"))?;
        Ok(b"".to_vec())
    }

    /// 起消费循环：收到消息经宿主 deliver 上送（逻辑 topic + 原始帧 payload）。
    /// stop_tx drop（close）→ changed()/closed 任一即退出循环（评审 S1 泄漏修复）。
    async fn do_subscribe(&self, handle: u64, topic: &str) -> Result<Vec<u8>, String> {
        let b = self.bus_broker(handle)?;
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
        let mut stop = b.stop_tx.subscribe();
        // 将 consumer 移入任务：MessageStream 借用 consumer，须同生命周期存活于任务内。
        tokio::spawn(async move {
            let mut stream = consumer.stream();
            loop {
                tokio::select! {
                    _ = stop.changed() => break,   // close(handle) 显式停
                    msg = stream.next() => match msg {
                        Some(Ok(m)) => {
                            let Some(p) = m.payload() else { continue };
                            let payload = String::from_utf8_lossy(p).to_string();
                            // 宿主按逻辑 topic 扇出；非阻塞投递（宿主 tx.send）。
                            (host.deliver)(
                                RString::from(logical.as_str()),
                                RString::from(payload.as_str()),
                            );
                        }
                        Some(Err(e)) => {
                            eprintln!("[oj-bus-kafka] consume error on {physical}: {e}");
                        }
                        None => break, // 发送端 drop（close）→ stream 结束
                    }
                }
            }
        });
        Ok(b"".to_vec())
    }
}

// ---- bus vtable（同步签名返回 FfiFuture；connect 产 handle，close 释放+停任务）----

extern "C" fn connect(cfg: RString) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(&st.rt, async move {
            let cfg: BrokerCfgJson =
                serde_json::from_str(&cfg[..]).map_err(|e| format!("kafka: bad cfg: {e}"))?;
            let broker = Arc::new(BusInstance::new(&cfg)?);
            let handle = st.next_bus.fetch_add(1, Ordering::SeqCst) + 1;
            st.bus.lock().unwrap().insert(handle, broker);
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
        // remove 即 drop BusInstance → stop_tx drop → push 消费任务收场（评审 S1）。
        state().bus.lock().unwrap().remove(&handle);
    })
}

static VTABLE: EventBrokerVtable = EventBrokerVtable {
    connect,
    publish,
    subscribe,
    close,
};

// ---- mq vtable（JSON method dispatch：kind/send/poll/commit/metadata）----

extern "C" fn mq_connect(cfg: RString) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(&st.rt, async move {
            let cfg: BrokerCfgJson =
                serde_json::from_str(&cfg[..]).map_err(|e| format!("kafka mq: bad cfg: {e}"))?;
            // kind 自检前置（评审 F7：装错插件 fail-fast，文案点名 kind）。
            if cfg.kind != "kafka" {
                return Err(format!(
                    "oj-bus-kafka: cfg kind '{}' mismatch (plugin serves 'kafka')",
                    cfg.kind
                ));
            }
            let inst = Arc::new(MqInstance::new(&cfg)?);
            let handle = st.next_mq.fetch_add(1, Ordering::SeqCst) + 1;
            st.mq.lock().unwrap().insert(handle, inst);
            Ok(format!(r#"{{"handle":{handle}}}"#).into_bytes())
        })
    })
}

extern "C" fn mq_call(handle: u64, method: RString, payload: RString) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(&st.rt, async move {
            // method 检查前置（未知 method 无需真实连接即可报错，评审可诊断性）。
            match &method[..] {
                "kind" => return Ok(br#""kafka""#.to_vec()),
                "send" | "poll" | "commit" | "metadata" => {}
                _ => return Err(format!("unsupported method: {}", &method[..])),
            }
            let inst = st.mq_instance(handle)?;
            match &method[..] {
                "send" => {
                    let req: SendReq = serde_json::from_str(&payload[..])
                        .map_err(|e| format!("kafka send: bad payload: {e}"))?;
                    inst.core.send(req).await
                }
                "poll" => {
                    let req: PollReq = serde_json::from_str(&payload[..])
                        .map_err(|e| format!("kafka poll: bad payload: {e}"))?;
                    inst.poll(req).await
                }
                "commit" => {
                    let msg: MqMessage = serde_json::from_str(&payload[..])
                        .map_err(|e| format!("kafka commit: bad payload: {e}"))?;
                    inst.commit(msg).await
                }
                "metadata" => inst.core.metadata().await,
                _ => unreachable!("method gated above"),
            }
        })
    })
}

extern "C" fn mq_close(handle: u64) {
    oj_plugin_ffi::catch_void(|| {
        // remove 即 drop MqInstance → session consumer drop → 离开消费组。
        state().mq.lock().unwrap().remove(&handle);
    })
}

static MQ_VTABLE: MqVtable = MqVtable {
    connect: mq_connect,
    call: mq_call,
    close: mq_close,
};

// ---- 入口 ----

fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        name: RString::from("bus-kafka"),
        semver: RString::from(env!("CARGO_PKG_VERSION")),
        abi_version: ABI_VERSION,
        fingerprint: RString::from(oj_plugin_ffi::HOST_FINGERPRINT),
        desc: RString::from(
            "bus + mq 双轴 kafka 插件：KafkaCore 共底层（bus push 扇出 / mq pull 显式 commit）",
        ),
    }
}

fn init(host: RArc<HostContext>, cfg: RString) -> RResult<PluginDescriptor, RString> {
    if PLUGIN.get().is_some() {
        return RResult::Ok(descriptor());
    }
    let _ = cfg; // init 无装配期配置（每实例 cfg 在 connect 传入）
    // get_or_init：并发 init 时闭包只跑一次（竞争方阻塞复用），不重复建 runtime，
    // 避免 `let _ = set(st)` 在竞争下把败者的 tokio Runtime 从 async 上下文 drop 崩溃。
    // HOST 随闭包同设一次；并发下 set 失败丢弃的 RArc 无 runtime，无害。
    PLUGIN.get_or_init(|| {
        let _ = HOST.set(host);
        BusPluginState {
            rt: runtime(),
            bus: Mutex::new(HashMap::new()),
            mq: Mutex::new(HashMap::new()),
            next_bus: AtomicU64::new(0),
            next_mq: AtomicU64::new(0),
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

oj_plugin_ffi::oj_plugin_entry!(init, bus => &VTABLE, mq => oj_plugin_ffi::axis::mq(&MQ_VTABLE));

#[cfg(test)]
mod tests {
    /// timeoutMs（camelCase JS 面）必须真正生效——评审 must-fix：此前 serde 静默丢弃。
    #[test]
    fn poll_req_accepts_js_camel_case_timeout() {
        let req: PollReq =
            serde_json::from_value(serde_json::json!({ "topics": ["t"], "timeoutMs": 1234 }))
                .unwrap();
        assert_eq!(req.timeout_ms, 1234);
        // 缺省回落 d_timeout；snake alias 兼容。
        let d: PollReq = serde_json::from_value(serde_json::json!({ "topics": ["t"] })).unwrap();
        assert_eq!(d.timeout_ms, d_timeout());
        let a: PollReq =
            serde_json::from_value(serde_json::json!({ "topics": ["t"], "timeout_ms": 77 }))
                .unwrap();
        assert_eq!(a.timeout_ms, 77);
    }

    use super::*;

    // ---- mq 面（TDD 先行用例；实现见 KafkaCore / MQ_VTABLE）----

    #[test]
    fn given_wrong_kind_when_mq_connect_then_err_names_kind() {
        // Given: cfg.kind = "rabbit"（装错插件场景）；When: mq connect
        // Then: Err 且文案点名 kind（装配层 fail-fast 依据，评审 F7）
        let _ = std::result::Result::from(init(host(), RString::from("{}")));
        let cfg = serde_json::json!({ "kind": "rabbit", "brokers": ["b:9092"] }).to_string();
        let rt = runtime();
        let out = rt.block_on(drive(&mut mq_connect(RString::from(cfg.as_str()))));
        assert!(matches!(out, Err(ref e) if e.contains("kind")), "{out:?}");
    }

    #[test]
    fn given_unknown_method_when_mq_call_then_err_unsupported() {
        // Given: 任意 handle；When: call method="nope"；Then: Err 列出 unsupported
        //（method 检查前置，无需真实连接）
        let _ = std::result::Result::from(init(host(), RString::from("{}")));
        let rt = runtime();
        let out = rt.block_on(drive(&mut mq_call(
            1,
            RString::from("nope"),
            RString::from("{}"),
        )));
        assert!(
            matches!(out, Err(ref e) if e.contains("unsupported method: nope")),
            "{out:?}"
        );
    }

    #[test]
    fn given_mq_cfg_without_brokers_when_connect_then_err_fail_fast() {
        // Given: kind 正确但缺 brokers；Then: cfg 校验 fail-fast（spec §7；可达性惰性另计）
        let _ = std::result::Result::from(init(host(), RString::from("{}")));
        let cfg = serde_json::json!({ "kind": "kafka" }).to_string();
        let rt = runtime();
        let out = rt.block_on(drive(&mut mq_connect(RString::from(cfg.as_str()))));
        assert!(
            matches!(out, Err(ref e) if e.contains("brokers")),
            "{out:?}"
        );
    }

    /// 真 kafka mq 面 roundtrip（env-gated）：send → poll → commit。
    #[tokio::test(flavor = "multi_thread")]
    async fn given_real_kafka_when_mq_send_poll_commit_then_roundtrips() {
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
            "group": format!("oj-mq-test-{}", std::process::id()),
        })
        .to_string();
        let _ = std::result::Result::from(init(host(), RString::from("{}")));
        let bytes = drive(&mut mq_connect(RString::from(cfg.as_str())))
            .await
            .expect("mq connect");
        let handle = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["handle"]
            .as_u64()
            .unwrap();
        let topic = format!("mq.{}", std::process::id());
        let payload = serde_json::json!({
            "topic": topic, "value": {"n": 1}
        })
        .to_string();
        drive(&mut mq_call(
            handle,
            RString::from("send"),
            RString::from(payload.as_str()),
        ))
        .await
        .expect("mq send");
        let polled = drive(&mut mq_call(
            handle,
            RString::from("poll"),
            RString::from(
                serde_json::json!({ "topics": [topic], "max": 10, "timeoutMs": 5000 })
                    .to_string()
                    .as_str(),
            ),
        ))
        .await
        .expect("mq poll");
        let v: serde_json::Value = serde_json::from_slice(&polled).unwrap();
        assert_eq!(v["messages"].as_array().unwrap().len(), 1, "{v}");
        let msg = v["messages"][0].clone();
        drive(&mut mq_call(handle, RString::from("commit"), {
            RString::from(serde_json::to_string(&msg).unwrap().as_str())
        }))
        .await
        .expect("mq commit");
        mq_close(handle);
    }

    // ---- bus 面（既有用例零改动 = 回归护栏）----

    /// cfg 校验离线路径：brokers 缺失 fail-fast。
    #[test]
    fn kafka_requires_brokers() {
        let cfg = BrokerCfgJson::default();
        assert!(BusInstance::new(&cfg).is_err());
        let cfg = BrokerCfgJson {
            brokers: vec!["127.0.0.1:9092".into()],
            ..Default::default()
        };
        // 仅 brokers 可构造（rdkafka create 离线构造；连接按需）。
        assert!(BusInstance::new(&cfg).is_ok());
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
        // 以真实墙钟时间为界轮询（同 oj-es 的 drive）：固定 10w 次 yield_now 在 CI
        // 负载/优化下会在插件 rt 的任务完成前耗尽预算，误报 "ffi drive timeout"。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            match (fut.poll)(fut.state) {
                0 => {
                    if std::time::Instant::now() >= deadline {
                        (fut.free)(fut.state); // 超时也要释放 state（防 FfiTask 泄漏）
                        fut.state = std::ptr::null_mut();
                        return Err("ffi drive timeout".into());
                    }
                    tokio::time::sleep(std::time::Duration::from_micros(100)).await;
                }
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
    }
}
