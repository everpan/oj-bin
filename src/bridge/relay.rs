//! ws.rooms_cluster 中继（REF-04，v0.1.140）：跨实例房间扇出的 core 侧传输。
//!
//! 形态（细化稿 REF-04 定稿）：单共享 topic；**实例级单消费者**——每实例以
//! boot-uuid 为显式组订阅一次（kafka 臂 = 独立 group + `auto.offset.reset=latest`，
//! 消 rebalance 抢消费 + 不回放历史；rabbit 臂 = 匿名排他队列，group 文档化 no-op），
//! 收到中继帧按房间扇出本地成员。**房间成员表与 roomState 恒进程本地，跨实例只传帧**。
//!
//! 传输信封（单一 JSON，二进制 base64——KB 级 CRDT/awareness 帧的 33% 开销可忽略，
//! 换一种格式通吃）：`{"room":…,"inst":…,"t":"t"|"b","d":…}`。经 `BusPayload::Bytes`
//! 发布——信封不含 `topic`+`data` 双键，不会命中 bus 消费侧的文本信封启发式，
//! 全程按字节透传。**seq 不进传输层**：缺口检测/join 去重/resync 对账全是
//! oj-collab 库的消费语义，库在 payload 内自带 framing。
//!
//! 回环语义：每实例独立 group/队列 ⇒ 自己发的帧也会回来，`inst == self` 丢弃——
//! 发送者实例的其他成员已由本地直投覆盖（`ws.broadcast` 除己语义不变），
//! 发送者自身协议上无需回发（客户端发送前已本地 apply）。
//!
//! boot 自回声探针（零 ABI）：subscribe → **周期性重发**探针帧 → 等自己收到，
//! 直到自回声或启动预算耗尽。重发覆盖「消费组 join / 队列绑定期间首帧被丢弃」
//! 的窗口：kafka 惰性 producer 骗不过「等接收」（broker 不可达 ⇒ 永远收不到 ⇒
//! 启动拒绝）、rabbit 交换机/权限错误在 publish 即爆、消费循环未起同样收不到——
//! 一个探针覆盖全部装配期悬崖，又不会在健康 broker 上偶发误拒。

use std::sync::{Arc, RwLock};

use serde_json::Value;

use super::WsSend;
use super::bus::{BusPayload, EventBroker};
use super::ws::hub;

/// 探针专用房间：中继循环对该房间不扇出（真实房间名不可能与之冲突——成员表按名隔离）。
pub const PROBE_ROOM: &str = "__oj_ws_relay_probe";
/// 中继逻辑 topic（物理前缀/交换机由臂内既有机制统一拼接；实例间一致性 = 配置一致）。
pub const RELAY_TOPIC: &str = "ws-relay";

/// 进程单例（hub 同款）；`RwLock<Option<_>>` 供测试重置——生产路径 spawn 恰一次。
static RELAY: RwLock<Option<Arc<Relay>>> = RwLock::new(None);

/// 中继面句柄：op 发布用（`relay()`），消费循环自持。
pub struct Relay {
    /// 实例身份：boot 随机 16 字节 hex（兼任 kafka 中继消费组）。
    pub inst: String,
    broker: Arc<dyn EventBroker>,
}

pub fn relay() -> Option<Arc<Relay>> {
    RELAY.read().unwrap_or_else(|e| e.into_inner()).clone()
}

fn set_relay(r: Option<Arc<Relay>>) {
    *RELAY.write().unwrap_or_else(|e| e.into_inner()) = r;
}

// ---- 信封 codec ----

fn encode(room: &str, inst: &str, data: &WsSend) -> Vec<u8> {
    use base64::Engine as _;
    let (t, d) = match data {
        WsSend::Text(s) => ("t", Value::String(s.clone())),
        WsSend::Binary(b) => (
            "b",
            Value::String(base64::engine::general_purpose::STANDARD.encode(b)),
        ),
        // hub/bus 通道不产生 Close（只在 frame_loop 直写 writer）；此处拒之门外。
        WsSend::Close(..) => return Vec::new(),
    };
    serde_json::to_vec(&serde_json::json!({ "room": room, "inst": inst, "t": t, "d": d }))
        .unwrap_or_default()
}

fn decode(bytes: &[u8]) -> Option<(String, String, WsSend)> {
    use base64::Engine as _;
    let v: Value = serde_json::from_slice(bytes).ok()?;
    let room = v.get("room")?.as_str()?.to_string();
    let inst = v.get("inst")?.as_str()?.to_string();
    let d = v.get("d")?;
    let payload = match v.get("t")?.as_str()? {
        "t" => WsSend::Text(d.as_str()?.to_string()),
        "b" => WsSend::Binary(
            base64::engine::general_purpose::STANDARD
                .decode(d.as_str()?)
                .ok()?,
        ),
        _ => return None,
    };
    Some((room, inst, payload))
}

impl Relay {
    /// 发布一帧到中继 topic（fire-and-forget 语义由调用方承担——`ws.broadcast`
    /// 的 JS 面不 await；broker 层错误在此记日志后透传给 op 侧吞掉）。
    pub async fn publish(&self, room: &str, data: &WsSend) -> Result<(), String> {
        let bytes = encode(room, &self.inst, data);
        self.broker
            .publish(RELAY_TOPIC, &BusPayload::Bytes(bytes))
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// boot 自回声探针句柄：spawn_relay 后周期性重发探针帧，`wait` 等自己收到。
pub struct Probe {
    nonce: String,
    rx: tokio::sync::oneshot::Receiver<()>,
}

impl Probe {
    /// publish 一帧探针（room = PROBE_ROOM，载荷 = nonce）。一次性；生产路径的
    /// `wait` 自带重发循环，本方法供单测手动打帧用。
    pub async fn ping(&self, relay: &Relay) -> Result<(), String> {
        relay
            .publish(PROBE_ROOM, &WsSend::Text(self.nonce.clone()))
            .await
    }

    /// 等自回声落地；**周期内周期性重发探针**直到收到自己的 nonce 或超时。
    ///
    /// 重发是必要的：kafka 显式组订阅后 group join / rabbit 队列绑定都要时间，
    /// 订阅刚返回就发的首帧会被 broker 丢弃（消费组尚未就绪）——单发一次会让
    /// `ws.rooms_cluster` 在健康 broker 上偶发启动被拒（真实踩坑：oj-bus-kafka
    /// 测试曾须 `sleep(700ms)` 才能收到首帧）。每轮重发间隔 250ms，总预算 = timeout。
    pub async fn wait(self, relay: &Relay, timeout: std::time::Duration) -> Result<(), String> {
        const RETRY: std::time::Duration = std::time::Duration::from_millis(250);
        let Probe { nonce, rx } = self;
        let mut rx = rx;
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            relay
                .publish(PROBE_ROOM, &WsSend::Text(nonce.clone()))
                .await
                .map_err(|e| format!("rooms_cluster boot probe publish: {e}"))?;
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining.min(RETRY), &mut rx).await {
                Ok(Ok(())) => return Ok(()),
                Ok(Err(_)) => {
                    return Err(
                        "rooms_cluster boot probe: relay consumer dropped before echo".to_string(),
                    );
                }
                Err(_) => continue, // 本轮未等到，重发再试
            }
        }
        Err(format!(
            "rooms_cluster boot probe: relay self-echo not received within {timeout:?} — \
             broker unreachable, topic permission denied, or relay consumer not running"
        ))
    }
}

/// 实例身份：ring SystemRandom 16 字节 hex（rand 只在 dev-deps，不为此加依赖）。
fn boot_inst() -> String {
    use ring::rand::SecureRandom as _;
    let mut b = [0u8; 16];
    ring::rand::SystemRandom::new()
        .fill(&mut b)
        .expect("system rng fill");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// 起中继：显式组（boot-uuid）订阅一次 → 消费循环扇出。进程生命周期，不回收
/// （同插件 dlopen 句柄惯例）。测试可经 `set_relay(None)` 复位。
pub async fn spawn_relay(broker: Arc<dyn EventBroker>) -> Result<(Arc<Relay>, Probe), String> {
    let inst = boot_inst();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<WsSend>();
    broker
        .subscribe(RELAY_TOPIC, Some(&inst), tx)
        .await
        .map_err(|e| format!("rooms_cluster relay subscribe: {e}"))?;
    let relay = Arc::new(Relay {
        inst: inst.clone(),
        broker,
    });
    let (probe_tx, probe_rx) = tokio::sync::oneshot::channel::<()>();
    let probe = Probe {
        nonce: boot_inst(),
        rx: probe_rx,
    };
    let probe_nonce = probe.nonce.clone();
    let mut probe_tx = Some(probe_tx);
    tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            let bytes = match &frame {
                // 中继帧只经 Bytes 通道发布；Text/Close 形状（他人误发/不可能经 hub）
                // 忽略。
                WsSend::Binary(b) => b.clone(),
                WsSend::Text(_) | WsSend::Close(..) => continue,
            };
            let Some((room, from_inst, payload)) = decode(&bytes) else {
                continue; // 非信封字节：静默忽略（版本漂移/异己帧）
            };
            if room == PROBE_ROOM {
                // 探针帧：载荷 = nonce，仅本人探针命中才点亮（他实例的探针不认）。
                if let WsSend::Text(s) = &payload
                    && *s == probe_nonce
                    && let Some(tx) = probe_tx.take()
                {
                    let _ = tx.send(());
                }
                continue;
            }
            if from_inst == inst {
                continue; // 回环：自己发的帧（本地直投已覆盖，绝不重复投递）
            }
            // 他实例的帧：本实例房间成员全收（from=0 不排除任何本地成员）。
            hub().broadcast(&room, 0, payload);
        }
    });
    set_relay(Some(relay.clone()));
    Ok((relay, probe))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::bus::Bus;

    fn bus_broker() -> Arc<dyn EventBroker> {
        Arc::new(Bus::new())
    }

    /// 双实例模拟：同一 broker、两个独立中继（各自组/循环）。他实例帧 → 本实例
    /// 房间成员恰收一次；发送者实例自身不重复投递（inst==self 丢弃 + 本地直投覆盖）。
    #[tokio::test(flavor = "current_thread")]
    async fn given_two_relay_instances_when_frame_published_then_other_side_fans_out_once() {
        set_relay(None);
        let bus = bus_broker();
        let (_r1, _p1) = spawn_relay(bus.clone()).await.unwrap();
        let (r2, _p2) = spawn_relay(bus.clone()).await.unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        hub().join("cluster-r", 7, tx);
        r2.publish("cluster-r", &WsSend::Text("hi-remote".to_string()))
            .await
            .unwrap();
        let got = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await;
        assert_eq!(got.unwrap().unwrap(), WsSend::Text("hi-remote".to_string()));
        // 不重复：发送者实例的回环帧被丢弃，本地仅此一份。
        let dup = tokio::time::timeout(std::time::Duration::from_millis(150), rx.recv()).await;
        assert!(dup.is_err(), "self frame must not be double-delivered");
        hub().leave("cluster-r", 7);
        set_relay(None);
    }

    /// 非 UTF-8 字节经 base64 信封无损往返（二进制帧型保持）。
    #[tokio::test(flavor = "current_thread")]
    async fn given_binary_frame_when_relayed_then_bytes_lossless() {
        set_relay(None);
        let bus = bus_broker();
        let (_r1, _p1) = spawn_relay(bus.clone()).await.unwrap();
        let (r2, _p2) = spawn_relay(bus.clone()).await.unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        hub().join("cluster-bin", 7, tx);
        r2.publish("cluster-bin", &WsSend::Binary(vec![0x00, 0xA9, 0xFF, 0xFE]))
            .await
            .unwrap();
        let got = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got, WsSend::Binary(vec![0x00, 0xA9, 0xFF, 0xFE]));
        hub().leave("cluster-bin", 7);
        set_relay(None);
    }

    /// boot 自回声探针：broker 活着 ⇒ 重发的探针帧经消费循环回到自己 ⇒ wait 过。
    #[tokio::test(flavor = "current_thread")]
    async fn given_alive_broker_when_self_echo_probe_then_probe_passes() {
        set_relay(None);
        let bus = bus_broker();
        let (relay, probe) = spawn_relay(bus.clone()).await.unwrap();
        probe
            .wait(&relay, std::time::Duration::from_secs(2))
            .await
            .expect("self-echo should arrive via local bus");
        set_relay(None);
    }

    /// 模拟「订阅已注册但消费尚未就绪」：首帧被丢弃，后续透传给真实 Bus。
    struct DropsFirstPublish {
        inner: Bus,
        drop_next: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl EventBroker for DropsFirstPublish {
        async fn publish(
            &self,
            topic: &str,
            data: &BusPayload,
        ) -> crate::bridge::BridgeResult<usize> {
            if self
                .drop_next
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                return Ok(0); // 消费组尚未就绪 → 首帧静默丢弃
            }
            Ok(Bus::publish(&self.inner, topic, data))
        }
        async fn subscribe(
            &self,
            topic: &str,
            group: Option<&str>,
            tx: tokio::sync::mpsc::UnboundedSender<WsSend>,
        ) -> crate::bridge::BridgeResult<()> {
            EventBroker::subscribe(&self.inner, topic, group, tx).await
        }
        fn kind(&self) -> &'static str {
            self.inner.kind()
        }
    }

    /// 回归（review finding）：首帧被丢弃（kafka group-join / rabbit 队列绑定）时，
    /// 单点 ping+wait 会误判启动失败；重发循环应在下一轮补发并收到自回声。
    #[tokio::test(flavor = "current_thread")]
    async fn given_first_publish_dropped_when_probe_waits_then_retry_succeeds() {
        set_relay(None);
        let broker = Arc::new(DropsFirstPublish {
            inner: Bus::new(),
            drop_next: std::sync::atomic::AtomicBool::new(true),
        });
        let (relay, probe) = spawn_relay(broker).await.unwrap();
        probe
            .wait(&relay, std::time::Duration::from_secs(2))
            .await
            .expect("retry must recover from a single dropped first publish");
        set_relay(None);
    }
}
