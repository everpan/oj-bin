//! mail 全局对象（宿主侧）：非密钥配置面 + 结果存储 + `deliver` 路由 + vtable 适配。
//!
//! 职责边界（design v3 §2/§6）：**宿主**负责配置装配、入参校验、附件字节解析、
//! 结果存储与 JS 全局挂载；**插件**（`plugins/oj-mail`）负责连接池、有界队列与真实投递。
//! 依赖倒置：本模块只对外暴露 [`MailBackend`] trait，`oj_plugin_ffi` 的 vtable 细节
//! 收敛在 [`FfiMailBackend`]（装配层只构造它，不碰 FFI 类型）。

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use oj_plugin_ffi::{MailAttachment, MailVtable, RBytes, RString};
use serde_json::Value;

use super::bus::BusPayload;
use super::{BridgeResult, EventBroker};

/// 结果上送 topic：插件 `HostContext.deliver(MAIL_RESULT_TOPIC, <信封 JSON>)`。
pub const MAIL_RESULT_TOPIC: &str = "mail.result";

/// 结果存储默认限长（条）：超出淘汰最旧，防无界增长。
pub const DEFAULT_RESULT_CAP: usize = 1024;
/// 结果存储默认 TTL：异步投递结果只对近期查询有意义，过期惰性清理。
pub const DEFAULT_RESULT_TTL: Duration = Duration::from_secs(3600);

/// 宿主解析后的附件（**原始字节**，不进 JS、不走 base64）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedAttachment {
    pub filename: String,
    pub mime: String,
    pub bytes: Vec<u8>,
}

/// mail 轴后端契约（依赖倒置：核心只依赖本 trait；FFI 细节在 [`FfiMailBackend`]）。
#[async_trait]
pub trait MailBackend: Send + Sync {
    /// 投递一封信（vtable `submit` 的宿主封装）：`key` = profile，`req` = 请求 JSON，
    /// `atts` = 已解析附件（**下标与 `req.attachments` 严格一致**）。
    /// 返回统一信封 `{code,msg,data}`。
    async fn submit(
        &self,
        key: &str,
        req: String,
        atts: Vec<ParsedAttachment>,
    ) -> BridgeResult<Value>;
    /// 非密钥配置面（白名单校验 + profile 列举；凭据不在此）。
    fn config(&self) -> &MailConfig;
    /// 结果路由：`deliver(MAIL_RESULT_TOPIC, …)` 钩子与 `op_mail_result` 共用同一份存储。
    fn router(&self) -> &MailResultRouter;
}

// ---------- 宿主侧配置（非密钥面） ----------

/// 单个 profile 的**非密钥**校验面（design §4：凭据只进插件）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MailProfileCfg {
    /// 发件人白名单（后缀匹配；空表 = 拒绝，fail-closed）。
    pub allowed_from: Vec<String>,
    /// 收件人白名单（to/cc/bcc 后缀匹配；空表 = 拒绝，fail-closed）。
    pub allowed_recipients: Vec<String>,
}

/// 宿主侧 mail 配置：只承载前置校验与 profile 列举所需字段。
/// `smpt` 段的并发/连接/凭据字段一概不落宿主（design §4/§11）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MailConfig {
    profiles: HashMap<String, MailProfileCfg>,
}

impl MailConfig {
    /// 空配置（无 profile）：所有 key 都不在清单 → 调用即报未配置的错误。
    pub fn empty() -> Self {
        Self::default()
    }

    /// 显式构造（装配/测试用）。
    pub fn new(profiles: HashMap<String, MailProfileCfg>) -> Self {
        Self { profiles }
    }

    /// 从 `smtp:` 段的 JSON 构建（插件的同一段 cfg）。
    /// 只吸收每个 profile 的 `allowed_from`/`allowed_recipients`；`workers`/`queue_capacity`
    /// 与连接字段（host/port/user/pass…）被忽略；形态错误（非对象 profile、非字符串数组）
    /// 立即报错——配置写错在装配期暴露，而非静默变成「无白名单」。
    pub fn from_value(v: &Value) -> BridgeResult<Self> {
        let obj = v
            .as_object()
            .ok_or_else(|| -> Box<dyn std::error::Error + Send + Sync> {
                "smtp 配置段必须是映射（profile: {...}）".into()
            })?;
        let mut profiles = HashMap::new();
        for (name, pv) in obj {
            if name == "workers" || name == "queue_capacity" {
                continue; // 并发参数（插件侧消费）
            }
            let po = pv
                .as_object()
                .ok_or_else(|| format!("smtp.{name} 必须是映射"))?;
            profiles.insert(
                name.clone(),
                MailProfileCfg {
                    allowed_from: str_list(po.get("allowed_from"), name, "allowed_from")?,
                    allowed_recipients: str_list(
                        po.get("allowed_recipients"),
                        name,
                        "allowed_recipients",
                    )?,
                },
            );
        }
        Ok(Self { profiles })
    }

    pub fn profile(&self, key: &str) -> Option<&MailProfileCfg> {
        self.profiles.get(key)
    }

    /// profile 名清单（排序后稳定输出；`op_mail_profiles` 数据源）。
    pub fn profile_keys(&self) -> Vec<String> {
        let mut ks: Vec<String> = self.profiles.keys().cloned().collect();
        ks.sort();
        ks
    }
}

fn str_list(v: Option<&Value>, profile: &str, field: &str) -> BridgeResult<Vec<String>> {
    let Some(v) = v else {
        return Ok(Vec::new());
    };
    let arr = v
        .as_array()
        .ok_or_else(|| format!("smtp.{profile}.{field} 必须是字符串数组"))?;
    let mut out = Vec::with_capacity(arr.len());
    for s in arr {
        out.push(
            s.as_str()
                .ok_or_else(|| format!("smtp.{profile}.{field} 必须是字符串数组"))?
                .to_string(),
        );
    }
    Ok(out)
}

// ---------- 结果存储 + deliver 路由 ----------

/// 结果存储：`jobId → 扁平信封`，**限长 + TTL**（惰性清理，无后台任务）。
pub struct MailResultStore {
    cap: usize,
    ttl: Duration,
    entries: Mutex<VecDeque<(String, Instant, Value)>>,
}

impl Default for MailResultStore {
    fn default() -> Self {
        Self::new(DEFAULT_RESULT_CAP, DEFAULT_RESULT_TTL)
    }
}

impl MailResultStore {
    pub fn new(cap: usize, ttl: Duration) -> Self {
        Self {
            cap,
            ttl,
            entries: Mutex::new(VecDeque::new()),
        }
    }

    /// 写入（同 jobId 覆盖）；超出限长淘汰最旧。
    pub fn put(&self, job_id: &str, v: Value) {
        let now = Instant::now();
        let mut g = self.entries.lock().unwrap();
        purge(&mut g, now, self.ttl);
        g.retain(|(k, _, _)| k != job_id);
        g.push_back((job_id.to_string(), now, v));
        while g.len() > self.cap {
            g.pop_front();
        }
    }

    /// 读取（过期项不可见）。
    pub fn get(&self, job_id: &str) -> Option<Value> {
        let now = Instant::now();
        let mut g = self.entries.lock().unwrap();
        purge(&mut g, now, self.ttl);
        g.iter()
            .rev()
            .find(|(k, _, _)| k == job_id)
            .map(|(_, _, v)| v.clone())
    }

    /// 存活条目数（含惰性清理）。
    pub fn len(&self) -> usize {
        let now = Instant::now();
        let mut g = self.entries.lock().unwrap();
        purge(&mut g, now, self.ttl);
        g.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn purge(g: &mut VecDeque<(String, Instant, Value)>, now: Instant, ttl: Duration) {
    g.retain(|(_, t, _)| now.saturating_duration_since(*t) < ttl);
}

/// 插件上送的统一信封 → 扁平结果（design §5：`{jobId,code,msg,messageId?}`）。
///
/// **白名单构造**：只取这四个字段，`to`/`subject` 等一律不进（design §11 脱敏，
/// 即使插件侧多塞字段也不会外泄到 bus 订阅者）。
/// `None` = 载荷不是可索引的完成结果（缺 `jobId`/`code`/`msg`）。
pub fn flatten_result(envelope: &Value) -> Option<Value> {
    let job_id = envelope.get("data")?.get("jobId")?.as_str()?;
    let code = envelope.get("code")?.as_i64()?;
    let msg = envelope.get("msg")?.as_str()?;
    let mut out = serde_json::Map::new();
    out.insert("jobId".into(), Value::String(job_id.to_string()));
    out.insert("code".into(), Value::from(code));
    out.insert("msg".into(), Value::from(msg));
    if let Some(mid) = envelope
        .get("data")
        .and_then(|d| d.get("messageId"))
        .and_then(Value::as_str)
    {
        out.insert("messageId".into(), Value::from(mid));
    }
    Some(Value::Object(out))
}

/// `deliver("mail.result", payload)` 的宿主落点：存结果 + 本地扇出。
///
/// 扇出走**同步** `publish_local`——回调是插件 worker 线程上的 `extern "C"`，
/// 不能 await（`EventBroker::publish` 是 async）。
pub struct MailResultRouter {
    store: Arc<MailResultStore>,
    bus: Arc<dyn EventBroker>,
}

impl MailResultRouter {
    pub fn new(bus: Arc<dyn EventBroker>) -> Self {
        Self {
            store: Arc::new(MailResultStore::default()),
            bus,
        }
    }

    /// 显式共享存储（限长/TTL 可注入）。
    pub fn with_store(store: Arc<MailResultStore>, bus: Arc<dyn EventBroker>) -> Self {
        Self { store, bus }
    }

    pub fn store(&self) -> &Arc<MailResultStore> {
        &self.store
    }

    /// 存储读取（`op_mail_result`）。
    pub fn get(&self, job_id: &str) -> Option<Value> {
        self.store.get(job_id)
    }

    /// 路由一帧插件上送：`None` = 载荷不是可索引的结果（丢弃，不 panic）。
    /// `Some(n)` = 已存结果并扇出给 n 个本地订阅者。
    pub fn route(&self, payload: &[u8]) -> Option<usize> {
        let env: Value = serde_json::from_slice(payload).ok()?;
        let flat = flatten_result(&env)?;
        let job_id = flat.get("jobId").and_then(Value::as_str)?.to_string();
        self.store.put(&job_id, flat.clone());
        Some(
            self.bus
                .publish_local(MAIL_RESULT_TOPIC, &BusPayload::Json(flat)),
        )
    }
}

/// 进程级 deliver 路由槽：`HostContext.deliver` 是无状态 `extern "C"`，
/// 只能经全局弱引用到达装配期注入的后端（强引用由 `StableState.mail` 持有）。
static MAIL_DELIVER: LazyLock<Mutex<Option<Weak<dyn MailBackend>>>> =
    LazyLock::new(|| Mutex::new(None));

/// 装配期挂载路由（每次构造带 mail 的 `StableState` 时调用；旧的弱引用随之失效）。
pub(crate) fn install_mail_deliver(b: &Arc<dyn MailBackend>) {
    *MAIL_DELIVER.lock().unwrap() = Some(Arc::downgrade(b));
}

/// `deliver(MAIL_RESULT_TOPIC, payload)` 的宿主落点。
/// `true` = 有 mail 后端接管（存结果 + 扇出）；`false` = 未配置 mail（调用方决定是否告警）。
pub(crate) fn route_deliver(payload: &[u8]) -> bool {
    let b = MAIL_DELIVER
        .lock()
        .unwrap()
        .as_ref()
        .and_then(Weak::upgrade);
    let Some(b) = b else {
        return false;
    };
    b.router().route(payload).is_some()
}

// ---------- vtable 适配器 ----------

/// `MailVtable` → 核心 [`MailBackend`]（FFI 细节只在本结构内：RVec/RString/RBytes 与 await）。
pub struct FfiMailBackend {
    vtable: &'static MailVtable,
    config: MailConfig,
    router: MailResultRouter,
}

impl FfiMailBackend {
    /// `bus` = 结果扇出目标（装配层传与 `Extras.bus` 同一实例）。
    pub fn new(vtable: &'static MailVtable, config: MailConfig, bus: Arc<dyn EventBroker>) -> Self {
        Self {
            vtable,
            config,
            router: MailResultRouter::new(bus),
        }
    }
}

#[async_trait]
impl MailBackend for FfiMailBackend {
    async fn submit(
        &self,
        key: &str,
        req: String,
        atts: Vec<ParsedAttachment>,
    ) -> BridgeResult<Value> {
        // 下标原序填 RVec：插件按 index 对齐 req.attachments（数量/顺序错位即 code:5）。
        let mut rv = oj_plugin_ffi::RVec::new();
        for a in atts {
            rv.push(MailAttachment {
                filename: RString::from(a.filename.as_str()),
                mime: RString::from(a.mime.as_str()),
                bytes: RBytes::from(&a.bytes[..]),
            });
        }
        let fut = (self.vtable.submit)(RString::from(key), RString::from(req.as_str()), rv);
        // 宿主侧驱动 FfiFuture：与 es/db/blob/bus 适配器同一 `await_ffi`（poll+yield_now）。
        let bytes = super::ffi::await_ffi(fut)
            .await
            .map_err(|e| format!("ffi mail submit: {e}"))?;
        serde_json::from_slice(&bytes).map_err(|e| format!("ffi mail submit decode: {e}").into())
    }

    fn config(&self) -> &MailConfig {
        &self.config
    }

    fn router(&self) -> &MailResultRouter {
        &self.router
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::{
        Bridge, Bus, EventBroker, Extras, InMemoryAccessor, InMemoryKV, SchemaRegistry, WsSend,
    };
    use oj_plugin_ffi::MailVtable;
    use serde_json::{Value, json};
    use std::sync::Mutex;
    use std::time::Duration;

    /// 全局 deliver 槽是进程级的：本模块用例串行化（同 `ffi.rs::adapter_tests` 手法）。
    /// 同批用例排队，不与其它的锁形成环。
    #[allow(clippy::await_holding_lock)]
    static T_LOCK: Mutex<()> = Mutex::new(());

    // ---------- 假后端（无插件依赖） ----------

    struct FakeMail {
        config: MailConfig,
        router: MailResultRouter,
        sent: Mutex<Vec<(String, String, Vec<ParsedAttachment>)>>,
    }

    impl FakeMail {
        fn new(config: MailConfig, bus: Arc<dyn EventBroker>) -> Arc<Self> {
            Arc::new(Self {
                config,
                router: MailResultRouter::new(bus),
                sent: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait::async_trait]
    impl MailBackend for FakeMail {
        async fn submit(
            &self,
            key: &str,
            req: String,
            atts: Vec<ParsedAttachment>,
        ) -> crate::bridge::BridgeResult<Value> {
            self.sent.lock().unwrap().push((key.to_string(), req, atts));
            Ok(json!({"code": 0, "msg": "ok", "data": {"jobId": "j-stub"}}))
        }
        fn config(&self) -> &MailConfig {
            &self.config
        }
        fn router(&self) -> &MailResultRouter {
            &self.router
        }
    }

    /// 假插件上送的统一信封（`HostContext.deliver(MAIL_RESULT_TOPIC, …)` 的载荷）。
    /// 故意夹带 `to`/`subject`：扁平化必须按白名单剔除（design §5/§11 脱敏）。
    const ENVELOPE: &[u8] = br#"{"code":0,"msg":"sent","data":{"jobId":"j1","messageId":"m1","to":["a@x.com"],"subject":"s"}}"#;

    fn deliver_to_host(topic: &str, payload: &[u8]) {
        crate::bridge::ffi::host_deliver(
            oj_plugin_ffi::RString::from(topic),
            oj_plugin_ffi::RBytes::from(payload),
        );
    }

    // ---------- 6.2：StableState/Extras 注入 ----------

    /// Extras.mail 注入 → StableState.mail 取用 → 插件上送的信封落进结果存储
    /// （deliver 路由已由构造期挂上）。
    #[tokio::test(flavor = "current_thread")]
    async fn bridge_injects_mail_backend_and_routes_deliver() {
        let _g = T_LOCK.lock().unwrap();
        let bus = Arc::new(Bus::new());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        bus.subscribe(MAIL_RESULT_TOPIC, tx);
        let fake = FakeMail::new(MailConfig::empty(), bus.clone());
        let _b = Bridge::with_dbs_and_loader(
            std::collections::HashMap::new(),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            None,
            Extras {
                mail: Some(fake.clone()),
                ..Default::default()
            },
        );
        // 插件 worker 上送 → 宿主路由：存结果 + 本地扇出。
        deliver_to_host(MAIL_RESULT_TOPIC, ENVELOPE);
        let stored = fake.router().get("j1").expect("结果必须已存");
        assert_eq!(stored["code"], 0);
        assert_eq!(stored["messageId"], "m1");
        // 扇出帧：扁平形态，且**不含** to/subject。
        let WsSend::Text(frame) = rx.try_recv().expect("订阅者必须收到扇出") else {
            panic!("mail.result 扇出应为文本帧");
        };
        let v: Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(v["topic"], MAIL_RESULT_TOPIC);
        assert_eq!(v["data"]["jobId"], "j1");
        assert_eq!(v["data"]["code"], 0);
        assert_eq!(v["data"]["messageId"], "m1");
        assert!(v["data"].get("to").is_none(), "{v}");
        assert!(v["data"].get("subject").is_none(), "{v}");
        assert_eq!(v["data"].as_object().unwrap().len(), 4, "{v}");
    }

    /// 未配置（StableState.mail = None）→ 路由明确「未接管」，不 panic（结果丢弃）。
    #[tokio::test(flavor = "current_thread")]
    async fn route_deliver_without_backend_is_false() {
        let _g = T_LOCK.lock().unwrap();
        let _b = Bridge::new(
            Arc::new(InMemoryAccessor::new()),
            Arc::new(InMemoryKV::new()),
        );
        assert!(!route_deliver(ENVELOPE));
    }

    // ---------- 6.3：结果存储 + 扁平化 ----------

    #[test]
    fn flatten_result_whitelists_fields_and_needs_job_id() {
        let e: Value = serde_json::from_slice(ENVELOPE).unwrap();
        assert_eq!(
            flatten_result(&e).unwrap(),
            json!({"jobId": "j1", "code": 0, "msg": "sent", "messageId": "m1"})
        );
        // 无 jobId → 无法索引，丢弃（不 panic）。
        assert!(flatten_result(&json!({"code": 0, "msg": "ok", "data": {}})).is_none());
        // 无 code/msg（非信封形态）→ 丢弃。
        assert!(flatten_result(&json!({"data": {"jobId": "j"}})).is_none());
    }

    #[test]
    fn result_store_caps_oldest_and_expires_by_ttl() {
        let s = MailResultStore::new(2, Duration::from_millis(60));
        s.put("a", json!({"jobId": "a"}));
        s.put("b", json!({"jobId": "b"}));
        s.put("c", json!({"jobId": "c"}));
        assert!(s.get("a").is_none(), "超限须淘汰最旧");
        assert!(s.get("b").is_some());
        assert_eq!(s.get("c").unwrap()["jobId"], "c");
        assert_eq!(s.len(), 2, "限长：容量恒定");
        std::thread::sleep(Duration::from_millis(80));
        assert!(s.get("c").is_none(), "TTL 过期后不可读");
        assert!(s.is_empty(), "过期项被惰性清理");
    }

    /// 同 jobId 重投覆盖（不重复占容量）。
    #[test]
    fn result_store_overwrites_same_job_id() {
        let s = MailResultStore::new(4, Duration::from_secs(60));
        s.put("j", json!({"jobId": "j", "code": 0}));
        s.put("j", json!({"jobId": "j", "code": 2}));
        assert_eq!(s.len(), 1);
        assert_eq!(s.get("j").unwrap()["code"], 2);
    }

    // ---------- 6.2：vtable 适配器 ----------

    /// 适配器把宿主办的 `Vec<ParsedAttachment>` 按**下标原序**填进 `RVec<MailAttachment>`
    /// （插件按 index 对齐，数量/顺序错位即 code:5），并把 future 结果（信封 JSON）解回。
    #[tokio::test(flavor = "current_thread")]
    async fn ffi_mail_backend_forwards_key_and_ordered_attachments() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        /// 假 vtable 记录的一次调用：key / req / 附件（filename, mime, bytes）。
        type Seen = (String, String, Vec<(String, String, Vec<u8>)>);
        static SEEN: Mutex<Vec<Seen>> = Mutex::new(Vec::new());
        static FREED: AtomicUsize = AtomicUsize::new(0);

        extern "C" fn fake_submit(
            key: oj_plugin_ffi::RString,
            req: oj_plugin_ffi::RString,
            atts: oj_plugin_ffi::RVec<oj_plugin_ffi::MailAttachment>,
        ) -> oj_plugin_ffi::FfiFuture {
            let mut got = Vec::new();
            for a in atts {
                got.push((
                    a.filename[..].to_string(),
                    a.mime[..].to_string(),
                    a.bytes[..].to_vec(),
                ));
            }
            SEEN.lock()
                .unwrap()
                .push((key[..].to_string(), req[..].to_string(), got));
            ready(Ok(
                br#"{"code":0,"msg":"sent","data":{"jobId":"j9","messageId":"m9"}}"#.to_vec(),
            ))
        }

        // 与 ffi.rs 适配器测试同款的 ready FfiFuture（poll/take/free 三指针）。
        struct Ready(Option<Result<Vec<u8>, String>>);
        extern "C" fn poll(state: *mut std::ffi::c_void) -> i32 {
            let s = unsafe { &mut *(state as *mut Ready) };
            match &s.0 {
                Some(Ok(_)) => 1,
                Some(Err(_)) => -1,
                None => 0,
            }
        }
        extern "C" fn take(
            state: *mut std::ffi::c_void,
        ) -> oj_plugin_ffi::RResult<oj_plugin_ffi::RBytes, oj_plugin_ffi::RString> {
            let s = unsafe { &mut *(state as *mut Ready) };
            match s.0.take() {
                Some(Ok(b)) => oj_plugin_ffi::RResult::Ok(oj_plugin_ffi::RBytes::from(&b[..])),
                Some(Err(e)) => {
                    oj_plugin_ffi::RResult::Err(oj_plugin_ffi::RString::from(e.as_str()))
                }
                None => oj_plugin_ffi::RResult::Err(oj_plugin_ffi::RString::from("not ready")),
            }
        }
        extern "C" fn free(state: *mut std::ffi::c_void) {
            if !state.is_null() {
                FREED.fetch_add(1, Ordering::SeqCst);
                drop(unsafe { Box::from_raw(state as *mut Ready) });
            }
        }
        fn ready(r: Result<Vec<u8>, String>) -> oj_plugin_ffi::FfiFuture {
            oj_plugin_ffi::FfiFuture {
                state: Box::into_raw(Box::new(Ready(Some(r)))).cast(),
                poll,
                take,
                free,
            }
        }

        let vt: &'static MailVtable = Box::leak(Box::new(MailVtable {
            submit: fake_submit,
        }));
        let bus: Arc<dyn EventBroker> = Arc::new(Bus::new());
        let b = FfiMailBackend::new(vt, MailConfig::empty(), bus);
        let atts = vec![
            ParsedAttachment {
                filename: "a.pdf".into(),
                mime: "application/pdf".into(),
                bytes: vec![1, 2, 3],
            },
            ParsedAttachment {
                filename: "b.bin".into(),
                mime: "application/octet-stream".into(),
                bytes: vec![0, 159, 255],
            },
        ];
        let env = b
            .submit("default", r#"{"subject":"s"}"#.to_string(), atts)
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(env["code"], 0);
        assert_eq!(env["data"]["jobId"], "j9");
        let seen = SEEN.lock().unwrap().clone();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, "default");
        assert_eq!(seen[0].1, r#"{"subject":"s"}"#);
        // 顺序与下标严格一致（含非 UTF-8 字节原样过线）。
        assert_eq!(
            seen[0].2,
            vec![
                (
                    "a.pdf".to_string(),
                    "application/pdf".to_string(),
                    vec![1, 2, 3]
                ),
                (
                    "b.bin".to_string(),
                    "application/octet-stream".to_string(),
                    vec![0, 159, 255]
                ),
            ]
        );
        assert_eq!(FREED.load(Ordering::SeqCst), 1, "future 句柄须 free");
    }

    // ---------- 6.2：宿主侧配置（非密钥面） ----------

    /// `smtp:` 段原样交给宿主：只吸收 profile 的 `allowed_*`（校验用），
    /// `workers`/`queue_capacity`/连接字段（含凭据）一律不进宿主结构。
    #[test]
    fn mail_config_takes_only_non_secret_whitelist_face() {
        let v = json!({
            "workers": 4,
            "queue_capacity": 256,
            "default": {
                "host": "smtp.example.com", "port": 465, "tls": "tls",
                "mechanism": "login", "user": "u", "pass": "SECRET",
                "allowed_from": ["noreply@x.com"],
                "allowed_recipients": ["@x.com", "@partner.com"]
            },
            "mock": { "file_transport": "/tmp/eml" }
        });
        let cfg = MailConfig::from_value(&v).unwrap();
        assert_eq!(cfg.profile_keys(), vec!["default", "mock"]);
        let p = cfg.profile("default").unwrap();
        assert_eq!(p.allowed_from, vec!["noreply@x.com"]);
        assert_eq!(p.allowed_recipients, vec!["@x.com", "@partner.com"]);
        // 未声明白名单的 profile：空表（校验层 fail-closed 的依据）。
        assert!(cfg.profile("mock").unwrap().allowed_from.is_empty());
        assert!(cfg.profile("nope").is_none());
        // 非对象 profile / 非字符串数组 → 配置期明确报错（不静默吞）。
        assert!(MailConfig::from_value(&json!("x")).is_err());
        assert!(MailConfig::from_value(&json!({"a": 1})).is_err());
        assert!(
            MailConfig::from_value(&json!({"a": {"allowed_from": [1, 2]}})).is_err(),
            "allowed_from 必须是字符串数组"
        );
    }
}
