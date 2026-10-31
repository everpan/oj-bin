//! oj-bus-rabbitmq：rabbitmq cdylib 插件，双轴——bus 轴（既有语义零变化）+ mq 轴（命名客户端）。
//! 共底层（spec 2026-09-07 §2）：一个 RabbitCore driver（lapin 连接/channel），bus / mq
//! 两个薄适配面；消费路径分列——bus 面 push 扇出（deliver 回调，收到即 ack），
//! mq 面 pull（basic_get 手动 ack/nack，at-least-once）。
//!
//! cfg 契约：init cfg = `{}`；bus connect(cfg) 收 BrokerCfg JSON（url 或 brokers +
//! topic_prefix）；mq connect(cfg) 额外要求 `kind == "rabbit"`（装配层按段注入，
//! 不符 → Err fail-fast）。句柄约定：bus / mq 两面 handle **分开编号**。
//!
//! 评审 S1 同款修复：bus close 停掉 detach 的 push 消费任务（watch 信号），
//! 不再只删 map 致 consumer 任务泄漏——这是修复，非行为回归。

use futures::StreamExt;
use lapin::message::Delivery;
use lapin::options::{
    BasicAckOptions, BasicConsumeOptions, BasicGetOptions, BasicNackOptions, BasicPublishOptions,
    ExchangeDeclareOptions, QueueBindOptions, QueueDeclareOptions,
};
use lapin::protocol::BasicProperties;
use lapin::types::{AMQPValue, FieldTable};
use lapin::{Connection, ConnectionProperties, ExchangeKind, acker::Acker};
use oj_plugin_ffi::{
    ABI_VERSION, EventBrokerVtable, FfiFuture, HostContext, MqMessage, MqVtable, PluginDescriptor,
    RArc, RBytes, RResult, RString,
};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// &[u8] → RBytes（stabby 无 From<&[u8]>，逐元素 push；与宿主侧同名助手同款）。
fn to_rbytes(bytes: &[u8]) -> RBytes {
    let mut v = RBytes::new();
    for b in bytes {
        v.push(*b);
    }
    v
}

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

// ---- RabbitCore：共底层 driver（连接；bus 与 mq 两面共享）----

struct RabbitCore {
    url: String,
    conn: Arc<Connection>,
}

impl RabbitCore {
    async fn new(cfg: &BrokerCfgJson) -> Result<Self, String> {
        let url = cfg
            .url
            .clone()
            .or_else(|| cfg.brokers.first().cloned())
            .ok_or_else(|| "rabbitmq requires 'url' or 'brokers'".to_string())?;
        // lapin 拨号是真实连接 → 装配期即可探活 fail-fast（spec §7，评审 S3 裁决）。
        let conn = Connection::connect(&url, ConnectionProperties::default())
            .await
            // 脱敏：amqp URL 带 `user:pass@`，原样进 Err 会经宿主终端镜像落 logs/
            // （凭据本身往往正是 config 里被 ENC[] 密封的那个值）。
            .map_err(|e| format!("rabbitmq connect {}: {e}", mask_url(&url)))?;
        Ok(Self {
            url,
            conn: Arc::new(conn),
        })
    }

    async fn channel(&self) -> Result<lapin::Channel, String> {
        self.conn
            .create_channel()
            .await
            .map_err(|e| format!("rabbitmq channel: {e}"))
    }

    /// 在给定 channel 上 publish（mq 面 send；JS 层 RabbitMQ.publish——payload：
    /// exchange/routingKey/value|value_b64/headers。审查 #3：走复用 channel，不逐次新建即弃）。
    async fn send_on(
        channel: &lapin::Channel,
        exchange: &str,
        routing_key: &str,
        headers: &HashMap<String, String>,
        payload: &[u8],
    ) -> Result<Vec<u8>, String> {
        let mut props = BasicProperties::default();
        if !headers.is_empty() {
            let mut ft = FieldTable::default();
            for (k, v) in headers {
                ft.insert(
                    k.clone().into(),
                    AMQPValue::LongString(v.as_bytes().to_vec().into()),
                );
            }
            props = props.with_headers(ft);
        }
        channel
            .basic_publish(
                exchange,
                routing_key,
                BasicPublishOptions::default(),
                payload,
                props,
            )
            .await
            .map_err(|e| format!("rabbitmq publish {exchange}/{routing_key}: {e}"))?;
        Ok(br#"{"sent":1}"#.to_vec())
    }
}

// ---- mq 面 poll 请求（rabbit 专属：queues；kafka 为 topics，两插件 payload 各自自解释）----

#[derive(serde::Deserialize)]
struct PollReq {
    queues: Vec<String>,
    #[serde(default = "d_max")]
    max: usize,
    // JS 面是 camelCase（timeoutMs）——rename 对齐；alias 兼容 snake_case 直调。
    #[serde(rename = "timeoutMs", alias = "timeout_ms", default = "d_timeout")]
    timeout_ms: u64,
}
fn d_max() -> usize {
    10
}
fn d_timeout() -> u64 {
    1000
}

// ---- bus 面：push 扇出（既有语义零变化）----

struct RabbitBroker {
    core: Arc<RabbitCore>,
    /// topic 交换名（默认 "oj-bus"；可由 `topic_prefix` 配置覆盖）。
    exchange: String,
    /// 复用 channel（v0.1.84：lapin 2.5 的 Channel 无 Drop→close，逐次新建即弃会泄漏
    /// 连接上的 channel，打满 channel_max 被 broker 杀连接——与 mq 面 reuse_channel 同款）。
    channel: tokio::sync::Mutex<Option<lapin::Channel>>,
    /// close = drop 发送端 → push 消费任务收场（评审 S1 泄漏修复，同 kafka 插件）。
    stop_tx: tokio::sync::watch::Sender<bool>,
}

/// 取复用 channel：连接存活直接 clone（廉价句柄），断线/失效置 None 重建。
/// bus / mq 两面共用（slot + core 由调用方持有）。
async fn reuse_channel(
    slot: &tokio::sync::Mutex<Option<lapin::Channel>>,
    core: &RabbitCore,
) -> Result<lapin::Channel, String> {
    let mut g = slot.lock().await;
    if let Some(ch) = g.as_ref() {
        if ch.status().connected() {
            return Ok(ch.clone());
        }
        *g = None;
    }
    let ch = core.channel().await?;
    *g = Some(ch.clone());
    Ok(ch)
}

/// 单次消费面搭建（bus 面）：channel → 排他自动删除队列（每订阅者独立，断连自动回收）
/// → 绑定 topic → 消费者。首次订阅与断连后的重建共用（v0.1.84 订阅自愈）。
async fn setup_consumer(
    core: &RabbitCore,
    exchange: &str,
    topic: &str,
) -> Result<lapin::Consumer, String> {
    let channel = core.channel().await?;
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
        .queue_bind(
            &queue_name,
            exchange,
            topic,
            QueueBindOptions::default(),
            FieldTable::default(),
        )
        .await
        .map_err(|e| format!("rabbitmq queue bind {topic}: {e}"))?;
    channel
        .basic_consume(
            &queue_name,
            "",
            BasicConsumeOptions::default(),
            FieldTable::default(),
        )
        .await
        .map_err(|e| format!("rabbitmq consume {topic}: {e}"))
}

impl RabbitBroker {
    async fn new(cfg: &BrokerCfgJson) -> Result<Self, String> {
        let core = Arc::new(RabbitCore::new(cfg).await?);
        let exchange = cfg.topic_prefix.clone().unwrap_or_else(|| "oj-bus".into());
        // 交换声明用的 channel 进复用槽（不再用完即弃——同款 channel 泄漏，量小但同为泄漏）。
        let channel = tokio::sync::Mutex::new(None);
        let ch = reuse_channel(&channel, &core).await?;
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
        let (stop_tx, _) = tokio::sync::watch::channel(false);
        Ok(Self {
            core,
            exchange,
            channel,
            stop_tx,
        })
    }
}

// ---- mq 面：pull（basic_get + 手动 ack/nack）----

struct MqInstance {
    core: Arc<RabbitCore>,
    /// 单 poller 互斥（插件侧保底；宿主任务上下文门禁是第一道，评审 M2）。
    poller: tokio::sync::Mutex<()>,
    /// 未确认投递：delivery_tag → Acker（ack/nack 载荷只回传 tag）。
    ackers: Mutex<HashMap<u64, Acker>>,
    /// 复用 channel（统一审查 must-fix #3：lapin 2.5 的 Channel 无 Drop→close，
    /// 每次 poll/send 新建即弃会泄漏连接上的 channel，最终打满 channel_max 被
    /// broker 杀连接）。断线/失效时置 None 下次重建。
    channel: tokio::sync::Mutex<Option<lapin::Channel>>,
}

impl MqInstance {
    async fn new(cfg: &BrokerCfgJson) -> Result<Self, String> {
        // kind 校验前置（同 kafka 插件；评审 F7）。url 校验在 Core::new（拨号即探活）。
        if cfg.kind != "rabbit" {
            return Err(format!(
                "oj-bus-rabbitmq: cfg kind '{}' mismatch (plugin serves 'rabbit')",
                cfg.kind
            ));
        }
        let core = Arc::new(RabbitCore::new(cfg).await?);
        Ok(Self {
            core,
            poller: tokio::sync::Mutex::new(()),
            ackers: Mutex::new(HashMap::new()),
            channel: tokio::sync::Mutex::new(None),
        })
    }

    /// 取复用 channel：连接存活直接 clone（廉价句柄），断线/失效重建。
    async fn reuse_channel(&self) -> Result<lapin::Channel, String> {
        reuse_channel(&self.channel, &self.core).await
    }

    /// poll：逐队列轮转 basic_get（每条一个 round-trip，评审 nit：非批量；prefetch 留升级），
    /// max 条或 timeout_ms 到期先到为准。no_ack=false → 手动 ack/nack。
    async fn poll(&self, req: PollReq) -> Result<Vec<u8>, String> {
        let _guard = self.poller.lock().await;
        let channel = self.reuse_channel().await?;
        let deadline = std::time::Instant::now() + Duration::from_millis(req.timeout_ms);
        let mut msgs: Vec<MqMessage> = Vec::new();
        'outer: while msgs.len() < req.max {
            for queue in &req.queues {
                if std::time::Instant::now() >= deadline || msgs.len() >= req.max {
                    break 'outer;
                }
                match channel
                    .basic_get(queue, BasicGetOptions { no_ack: false })
                    .await
                {
                    Ok(Some(get)) => {
                        let d: Delivery = get.delivery;
                        let mut headers = HashMap::new();
                        if let Some(ft) = d.properties.headers() {
                            for (k, v) in ft.inner().iter() {
                                headers.insert(
                                    k.to_string(),
                                    match v {
                                        AMQPValue::LongString(s) => s.to_string(),
                                        other => format!("{other:?}"),
                                    },
                                );
                            }
                        }
                        let value = serde_json::from_slice(&d.data).unwrap_or_else(|_| {
                            Value::String(String::from_utf8_lossy(&d.data).into_owned())
                        });
                        // 非 UTF-8 载荷（二进制透传，v0.1.16）→ base64，value 置 null。
                        let value_b64 = if std::str::from_utf8(&d.data).is_err()
                            && serde_json::from_slice::<Value>(&d.data).is_err()
                        {
                            use base64::Engine as _;
                            Some(base64::engine::general_purpose::STANDARD.encode(&d.data))
                        } else {
                            None
                        };
                        let value = if value_b64.is_some() {
                            Value::Null
                        } else {
                            value
                        };
                        let ts = d.properties.timestamp().unwrap_or(0) as i64;
                        self.ackers
                            .lock()
                            .unwrap()
                            .insert(d.delivery_tag, d.acker.clone());
                        msgs.push(MqMessage {
                            topic: queue.clone(),
                            partition: None,
                            offset: None,
                            key: None,
                            value,
                            value_b64,
                            headers,
                            ts,
                            delivery_tag: Some(d.delivery_tag),
                        });
                    }
                    Ok(None) => { /* 该队列空，继续下一队列 */ }
                    Err(e) => return Err(format!("rabbitmq get {queue}: {e}")),
                }
            }
            // 整轮全空 → 小睡再战（审查 #5：空轮间热旋转打 AMQP RPC 烧 broker/网络）。
            if msgs.is_empty() && std::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
        Ok(serde_json::json!({ "messages": msgs })
            .to_string()
            .into_bytes())
    }

    fn take_acker(&self, msg: &MqMessage) -> Result<Acker, String> {
        let tag = msg.delivery_tag.ok_or_else(|| {
            "rabbitmq ack/nack: payload requires deliveryTag (poll 返回的消息原样回传)".to_string()
        })?;
        self.ackers
            .lock()
            .unwrap()
            .remove(&tag)
            .ok_or_else(|| format!("rabbitmq ack/nack: unknown deliveryTag {tag}"))
    }

    async fn ack(&self, msg: MqMessage) -> Result<Vec<u8>, String> {
        let acker = self.take_acker(&msg)?;
        acker
            .ack(BasicAckOptions::default())
            .await
            .map_err(|e| format!("rabbitmq ack: {e}"))?;
        Ok(b"{}".to_vec())
    }

    async fn nack(&self, msg: MqMessage, requeue: bool) -> Result<Vec<u8>, String> {
        let acker = self.take_acker(&msg)?;
        acker
            .nack(BasicNackOptions {
                requeue,
                ..Default::default()
            })
            .await
            .map_err(|e| format!("rabbitmq nack: {e}"))?;
        Ok(b"{}".to_vec())
    }
}

// ---- 插件共享状态（bus / mq 两面 handle 分开编号）----

struct BusPluginState {
    rt: tokio::runtime::Runtime,
    bus: Mutex<HashMap<u64, Arc<RabbitBroker>>>,
    mq: Mutex<HashMap<u64, Arc<MqInstance>>>,
    next_bus: AtomicU64,
    next_mq: AtomicU64,
}

static PLUGIN: OnceLock<BusPluginState> = OnceLock::new();
/// init 时宿主注入的上下文（消费循环经 deliver 回调上送消息）。
static HOST: OnceLock<RArc<HostContext>> = OnceLock::new();

fn state() -> &'static BusPluginState {
    PLUGIN.get().expect("oj-bus-rabbitmq: init not called")
}

impl BusPluginState {
    fn broker(&self, handle: u64) -> Result<Arc<RabbitBroker>, String> {
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

    async fn do_publish(&self, handle: u64, topic: &str, data: &[u8]) -> Result<Vec<u8>, String> {
        let b = self.broker(handle)?;
        // 复用 channel（v0.1.84：原逐次新建即弃——Channel 无 Drop→close，泄漏打满
        // channel_max 被 broker 杀连接；同 mq 面 reuse_channel）。
        let channel = reuse_channel(&b.channel, &b.core).await?;
        // 投递到 topic 交换，路由键 = topic；不阻塞等待 broker confirm（与 core 一致）。
        channel
            .basic_publish(
                &b.exchange,
                topic,
                BasicPublishOptions::default(),
                data,
                BasicProperties::default(),
            )
            .await
            .map_err(|e| format!("rabbitmq publish {topic}: {e}"))?;
        Ok(b"".to_vec())
    }

    async fn do_subscribe(&self, handle: u64, topic: &str) -> Result<Vec<u8>, String> {
        let b = self.broker(handle)?;
        // 首次搭建同步执行：broker 不可达仍让 vtable.subscribe 以 Err 透出（宿主回滚
        // 刚注册的通道），语义与既有装配一致。
        let mut consumer = setup_consumer(&b.core, &b.exchange, topic).await?;
        let host = HOST
            .get()
            .cloned()
            .expect("oj-bus-rabbitmq: init before subscribe");
        let logical = topic.to_string();
        let core = Arc::clone(&b.core);
        let exchange = b.exchange.clone();
        let mut stop = b.stop_tx.subscribe();
        tokio::spawn(async move {
            // v0.1.84：连接/通道级错误不再永久退出（原 `Some(Err(_)) => break`——一次
            // 抖动即静默杀死订阅，排他自动删除队列随断连消失，topic 停摆到进程重启）。
            // 错误 → 退避后重建 队列+绑定+消费者；成功收到消息即重置退避。
            let mut backoff = Duration::from_millis(500);
            const BACKOFF_MAX: Duration = Duration::from_secs(10);
            'outer: loop {
                loop {
                    tokio::select! {
                        _ = stop.changed() => break 'outer,   // close(handle) 显式停（评审 S1）
                        msg = consumer.next() => match msg {
                            Some(Ok(delivery)) => {
                                // 原始字节上送（ABI 8；JSON 信封/二进制帧型由宿主侧判定）。
                                delivery.ack(BasicAckOptions::default()).await.ok();
                                (host.deliver)(
                                    RString::from(logical.as_str()),
                                    to_rbytes(&delivery.data),
                                );
                                backoff = Duration::from_millis(500); // 连接健康，重置退避
                            }
                            // 瞬时错误：退出内层 → 退避后重建队列/消费者（订阅存活）。
                            Some(Err(e)) => {
                                eprintln!(
                                    "[oj-bus-rabbitmq] subscribe {logical}: consume error, \
                                     rebuilding: {e}"
                                );
                                break;
                            }
                            None => break,
                        }
                    }
                }
                tokio::select! {
                    _ = stop.changed() => break 'outer,
                    _ = tokio::time::sleep(backoff) => {}
                }
                backoff = (backoff * 2).min(BACKOFF_MAX);
                match setup_consumer(&core, &exchange, &logical).await {
                    Ok(c) => consumer = c,
                    Err(e) => {
                        eprintln!("[oj-bus-rabbitmq] subscribe {logical}: rebuild: {e}");
                    }
                }
            }
        });
        Ok(b"".to_vec())
    }
}

// ---- bus vtable ----

extern "C" fn connect(cfg: RString) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(&st.rt, async move {
            let cfg: BrokerCfgJson =
                serde_json::from_str(&cfg[..]).map_err(|e| format!("rabbitmq: bad cfg: {e}"))?;
            let broker = Arc::new(RabbitBroker::new(&cfg).await?);
            let handle = st.next_bus.fetch_add(1, Ordering::SeqCst) + 1;
            st.bus.lock().unwrap().insert(handle, broker);
            Ok(format!(r#"{{"handle":{handle}}}"#).into_bytes())
        })
    })
}

extern "C" fn publish(handle: u64, topic: RString, data: RBytes) -> FfiFuture {
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
        // remove 即 drop RabbitBroker → stop_tx drop → push 消费任务收场（评审 S1）。
        state().bus.lock().unwrap().remove(&handle);
    })
}

static VTABLE: EventBrokerVtable = EventBrokerVtable {
    connect,
    publish,
    subscribe,
    close,
};

// ---- mq vtable（JSON method dispatch：kind/send/poll/ack/nack）----

extern "C" fn mq_connect(cfg: RString) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(&st.rt, async move {
            let cfg: BrokerCfgJson =
                serde_json::from_str(&cfg[..]).map_err(|e| format!("rabbitmq mq: bad cfg: {e}"))?;
            let inst = Arc::new(MqInstance::new(&cfg).await?);
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
            // method 检查前置（未知 method 无需真实连接即可报错）。
            match &method[..] {
                "kind" => return Ok(br#""rabbit""#.to_vec()),
                "send" | "poll" | "ack" | "nack" | "metadata" => {}
                _ => return Err(format!("unsupported method: {}", &method[..])),
            }
            let inst = st.mq_instance(handle)?;
            match &method[..] {
                "send" => {
                    let req: SendPayload = serde_json::from_str(&payload[..])
                        .map_err(|e| format!("rabbitmq send: bad payload: {e}"))?;
                    // value_b64（v0.1.16）→ 解码字节为 AMQP payload；否则 value JSON 序列化。
                    let payload_bytes: Vec<u8> = if let Some(b64) = &req.value_b64 {
                        use base64::Engine as _;
                        base64::engine::general_purpose::STANDARD
                            .decode(b64)
                            .map_err(|e| format!("rabbitmq send: bad value_b64: {e}"))?
                    } else {
                        req.value.unwrap_or(Value::Null).to_string().into_bytes()
                    };
                    let ch = inst.reuse_channel().await?;
                    RabbitCore::send_on(
                        &ch,
                        &req.exchange,
                        &req.routing_key,
                        &req.headers,
                        &payload_bytes,
                    )
                    .await
                }
                "poll" => {
                    let req: PollReq = serde_json::from_str(&payload[..])
                        .map_err(|e| format!("rabbitmq poll: bad payload: {e}"))?;
                    inst.poll(req).await
                }
                "ack" => {
                    let msg: MqMessage = serde_json::from_str(&payload[..])
                        .map_err(|e| format!("rabbitmq ack: bad payload: {e}"))?;
                    inst.ack(msg).await
                }
                "nack" => {
                    let req: NackPayload = serde_json::from_str(&payload[..])
                        .map_err(|e| format!("rabbitmq nack: bad payload: {e}"))?;
                    inst.nack(req.rest, req.requeue).await
                }
                // rabbit 无集群 metadata 概览 API 的轻量面：kind + 连接 URL 掩码。
                // 返回固定形状即可（可选 method，spec §5）。
                "metadata" => Ok(
                    serde_json::json!({ "kind": "rabbit", "url": mask_url(&inst.core.url) })
                        .to_string()
                        .into_bytes(),
                ),
                _ => unreachable!("method gated above"),
            }
        })
    })
}

/// 掩去 URL userinfo（amqp://user:pass@host → amqp://user:***@host；无 userinfo 原样）。
/// metadata() 对外可见，密码不得回显（审查 #12）。
fn mask_url(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => match rest.split_once('@') {
            Some((userinfo, host)) => {
                let user = userinfo.split(':').next().unwrap_or("");
                format!("{scheme}://{user}:***@{host}")
            }
            None => url.to_string(),
        },
        None => url.to_string(),
    }
}

extern "C" fn mq_close(handle: u64) {
    oj_plugin_ffi::catch_void(|| {
        // remove 即 drop MqInstance → ackers drop（未确认投递由 broker 重投，at-least-once）。
        state().mq.lock().unwrap().remove(&handle);
    })
}

static MQ_VTABLE: MqVtable = MqVtable {
    connect: mq_connect,
    call: mq_call,
    close: mq_close,
};

#[derive(serde::Deserialize)]
struct SendPayload {
    exchange: String,
    #[serde(rename = "routingKey")]
    routing_key: String,
    #[serde(default)]
    headers: HashMap<String, String>,
    /// 文本/JSON 载荷（value_b64 存在时可省）。
    #[serde(default)]
    value: Option<Value>,
    /// 二进制载荷（base64，v0.1.16）：设置时 AMQP payload = 解码字节。
    #[serde(default)]
    value_b64: Option<String>,
}

#[derive(serde::Deserialize)]
struct NackPayload {
    #[serde(flatten)]
    rest: MqMessage,
    #[serde(default)]
    requeue: bool,
}

// ---- 入口 ----

fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        name: RString::from("bus-rabbitmq"),
        semver: RString::from(env!("CARGO_PKG_VERSION")),
        abi_version: ABI_VERSION,
        fingerprint: RString::from(oj_plugin_ffi::HOST_FINGERPRINT),
        desc: RString::from(
            "bus + mq 双轴 rabbitmq 插件：RabbitCore 共底层（bus push 扇出 / mq pull 手动 ack）",
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
    // worker 只跑 IO 转发（amqp 连接自管），2 足够；缺省 = num_cpus 全核白占线程。
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("oj-bus-rabbitmq tokio runtime")
}

oj_plugin_ffi::oj_plugin_entry!(init, bus => &VTABLE, mq => oj_plugin_ffi::axis::mq(&MQ_VTABLE));

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
