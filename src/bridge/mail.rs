//! mail 全局对象（宿主侧）：非密钥配置面 + 结果存储 + `deliver` 路由 + vtable 适配。
//!
//! 职责边界（design v3 §2/§6）：**宿主**负责配置装配、入参校验、附件字节解析、
//! 结果存储与 JS 全局挂载；**插件**（`plugins/oj-mail`）负责连接池、有界队列与真实投递。
//! 依赖倒置：本模块只对外暴露 [`MailBackend`] trait，`oj_plugin_ffi` 的 vtable 细节
//! 收敛在 [`FfiMailBackend`]（装配层只构造它，不碰 FFI 类型）。

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use deno_core::{OpState, op2};
use deno_error::JsErrorBox;
use lettre::Address;
use oj_plugin_ffi::{MailAttachment, MailVtable, RBytes, RString};
use serde_json::{Map, Value, json};

use super::blob::BlobRegistry;
use super::bus::BusPayload;
use super::module_loader::ensure_within;
use super::{BridgeResult, EventBroker};

/// 结果上送 topic：插件 `HostContext.deliver(MAIL_RESULT_TOPIC, <信封 JSON>)`。
pub const MAIL_RESULT_TOPIC: &str = "mail.result";

/// 控制报文键（vtable `submit` 的 `req` 顶层）：`{"__ctl":"drain","timeout_ms":N}`。
/// **零 ABI 变更**的停机入口 —— `MailVtable` 形状不变，控制报文与业务 req 共用 `submit`；
/// 与插件侧 `plugins/oj-mail/src/engine.rs` 的 `CTL_KEY`/`CTL_DRAIN` **逐字对齐**（跨进程契约：
/// 宿主拼字符串、插件解析）。
pub const CONTROL_KEY: &str = "__ctl";
/// `__ctl` 取值：停机排空（graceful drain）。
pub const CONTROL_DRAIN: &str = "drain";

/// 结果存储默认限长（条）：超出淘汰最旧，防无界增长。
pub const DEFAULT_RESULT_CAP: usize = 1024;
/// 结果存储默认 TTL：异步投递结果只对近期查询有意义，过期惰性清理。
pub const DEFAULT_RESULT_TTL: Duration = Duration::from_secs(3600);

/// 单附件默认上限（B2）：10 MiB。
pub const DEFAULT_MAX_ATTACHMENT_BYTES: usize = 10 * 1024 * 1024;
/// 单封信全部附件合计默认上限（B2）：25 MiB（有界队列容量 256，故合计上限必须有）。
pub const DEFAULT_MAX_TOTAL_ATTACHMENT_BYTES: usize = 25 * 1024 * 1024;

/// 附件字节上限（B2，来自 `smtp:` 顶层键 `max_attachment_bytes` / `max_total_attachment_bytes`，
/// 与 `workers`/`queue_capacity` 同级；缺省见 [`DEFAULT_MAX_ATTACHMENT_BYTES`] /
/// [`DEFAULT_MAX_TOTAL_ATTACHMENT_BYTES`]）。
///
/// **为什么必须有**：附件字节由**宿主**读盘后经**有界队列**（容量 256）交给插件，
/// 无上限时 project root 内任意大文件（含 `config.yaml` —— 里面有 `jwt_secret` / smtp
/// 口令）都能被一次调用读进内存并被队列放大成内存 DoS。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttachmentLimits {
    /// 单个附件上限（字节）。
    pub max_attachment_bytes: usize,
    /// 单封信全部附件**合计**上限（字节）。
    pub max_total_bytes: usize,
}

impl Default for AttachmentLimits {
    fn default() -> Self {
        Self {
            max_attachment_bytes: DEFAULT_MAX_ATTACHMENT_BYTES,
            max_total_bytes: DEFAULT_MAX_TOTAL_ATTACHMENT_BYTES,
        }
    }
}

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
    /// 停机排空（生产停机路径调用）：让后端停收新投递、等在途 job 跑完再销毁 transport。
    ///
    /// **同步**语义（会阻塞调用线程至多 `timeout`）：调用方放 blocking 池。插件侧的
    /// `drain` 在 FFI 调用内跑完才返回（见 `CONTROL_KEY` 契约），故这里不做异步等待。
    /// 返回统一信封：`{code:0,data:{drained:true}}` = 已排空（或在途已跑完）；
    /// `code:1` = 超时（已停收 + 已销毁，在途 job 可能被丢弃）。
    ///
    /// 默认实现 = 无可排空（第三方/无状态后端无需实现；`data.drained = false`）。
    fn drain(&self, _timeout: Duration) -> BridgeResult<Value> {
        Ok(json!({ "code": 0, "msg": "ok", "data": { "drained": false } }))
    }
}

// ---------- 宿主侧配置（非密钥面） ----------

/// 单个 profile 的**非密钥**校验面（design §4：凭据只进插件）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MailProfileCfg {
    /// 发件人白名单（条目 = 完整地址或 `@domain`，**全等**匹配；空表 = 拒绝，fail-closed）。
    /// 条目格式在装配期校验（见 [`parse_whitelist_entry`]）。
    pub allowed_from: Vec<String>,
    /// 收件人白名单（to/cc/bcc 同上；空表 = 拒绝，fail-closed）。
    pub allowed_recipients: Vec<String>,
}

/// 宿主侧 mail 配置：只承载前置校验与 profile 列举所需字段。
/// `smpt` 段的并发/连接/凭据字段一概不落宿主（design §4/§11）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailConfig {
    profiles: HashMap<String, MailProfileCfg>,
    limits: AttachmentLimits,
}

impl Default for MailConfig {
    /// 手写而非 derive：附件上限的默认值必须是 [`AttachmentLimits::default`]
    /// （derive 会给 0，等于「所有附件一律拒绝」）。
    fn default() -> Self {
        Self {
            profiles: HashMap::new(),
            limits: AttachmentLimits::default(),
        }
    }
}

impl MailConfig {
    /// 空配置（无 profile）：所有 key 都不在清单 → 调用即报未配置的错误。
    pub fn empty() -> Self {
        Self::default()
    }

    /// 显式构造（装配/测试用）；附件上限取默认值（[`Self::with_limits`] 可覆盖）。
    pub fn new(profiles: HashMap<String, MailProfileCfg>) -> Self {
        Self {
            profiles,
            limits: AttachmentLimits::default(),
        }
    }

    /// 显式构造 + 附件上限（测试/装配用）。
    pub fn with_limits(
        profiles: HashMap<String, MailProfileCfg>,
        limits: AttachmentLimits,
    ) -> Self {
        Self { profiles, limits }
    }

    /// 附件字节上限（`resolve_attachments` 的判据）。
    pub fn attachment_limits(&self) -> AttachmentLimits {
        self.limits
    }

    /// 从 `smtp:` 段的 JSON 构建（插件的同一段 cfg）。
    /// 只吸收每个 profile 的 `allowed_from`/`allowed_recipients` 与顶层的附件上限；
    /// `workers`/`queue_capacity` 与连接字段（host/port/user/pass…）被忽略；形态错误
    /// （非对象 profile、非字符串数组、附件上限非正整数）与**白名单条目格式非法**
    /// （空串/缺 `@`/首尾空白）都立即报错——配置写错在装配期暴露，而非静默变成
    /// 「无白名单」「白名单悄悄不命中」或「附件上限为 0」（见 [`parse_whitelist_entry`]）。
    pub fn from_value(v: &Value) -> BridgeResult<Self> {
        let obj = v
            .as_object()
            .ok_or_else(|| -> Box<dyn std::error::Error + Send + Sync> {
                "smtp 配置段必须是映射（profile: {...}）".into()
            })?;
        let mut profiles = HashMap::new();
        for (name, pv) in obj {
            if name == "workers"
                || name == "queue_capacity"
                || name == "max_attachment_bytes"
                || name == "max_total_attachment_bytes"
            {
                continue; // 并发参数（插件侧消费）与附件上限（下方单独解析）
            }
            let po = pv
                .as_object()
                .ok_or_else(|| format!("smtp.{name} 必须是映射"))?;
            profiles.insert(
                name.clone(),
                MailProfileCfg {
                    allowed_from: whitelist_list(po.get("allowed_from"), name, "allowed_from")?,
                    allowed_recipients: whitelist_list(
                        po.get("allowed_recipients"),
                        name,
                        "allowed_recipients",
                    )?,
                },
            );
        }
        Ok(Self {
            profiles,
            limits: AttachmentLimits {
                max_attachment_bytes: positive_bytes(
                    obj.get("max_attachment_bytes"),
                    "max_attachment_bytes",
                    DEFAULT_MAX_ATTACHMENT_BYTES,
                )?,
                max_total_bytes: positive_bytes(
                    obj.get("max_total_attachment_bytes"),
                    "max_total_attachment_bytes",
                    DEFAULT_MAX_TOTAL_ATTACHMENT_BYTES,
                )?,
            },
        })
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

/// 顶层字节上限键（`smtp.max_attachment_bytes` / `smtp.max_total_attachment_bytes`）：
/// 缺失 → 默认值；给了非整数或 0 → **装配期报错**（0 等于「附件一律拒绝」，只会是笔误；
/// 非整数是写法错误，二者都不静默取默认）。
fn positive_bytes(v: Option<&Value>, field: &str, default: usize) -> BridgeResult<usize> {
    let Some(v) = v else {
        return Ok(default);
    };
    let n = v
        .as_u64()
        .ok_or_else(|| -> Box<dyn std::error::Error + Send + Sync> {
            format!("smtp.{field} 必须是正整数（字节数）（下一步：写成如 10485760）").into()
        })?;
    if n == 0 {
        return Err(
            format!("smtp.{field} 必须 ≥ 1（0 等于禁止一切附件；下一步：写成字节数）").into(),
        );
    }
    Ok(n as usize)
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

/// `deliver(MAIL_RESULT_TOPIC, payload)` 的宿主落点结果（**细分两种失败**：告警文案要能
/// 分辨「没配 mail」与「载荷非法」——两者都被丢弃，但排障方向完全不同）。
/// 扇出订阅者数只在 [`MailResultRouter::route`] 的返回值里（告警路径不需要它）。
pub(crate) enum DeliverRoute {
    /// 有后端接管：已存结果并扇出（扇出数见 `MailResultRouter::route`）。
    Routed,
    /// 未配置 mail（`StableState.mail` 为空，或弱引用已失效）。
    NotConfigured,
    /// 载荷不是可索引的完成结果（非 JSON，或缺 `jobId`/`code`/`msg`）。
    BadPayload,
}

/// `deliver(MAIL_RESULT_TOPIC, payload)` 的宿主落点：`Routed` = 存结果 + 扇出；
/// 其余两种成因见 [`DeliverRoute`]（调用方分别告警，不混为一句）。
pub(crate) fn route_deliver(payload: &[u8]) -> DeliverRoute {
    let b = MAIL_DELIVER
        .lock()
        .unwrap()
        .as_ref()
        .and_then(Weak::upgrade);
    let Some(b) = b else {
        return DeliverRoute::NotConfigured;
    };
    match b.router().route(payload) {
        Some(_) => DeliverRoute::Routed,
        None => DeliverRoute::BadPayload,
    }
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

/// 纵深防御：把插件返回的 JSON 收敛为**统一信封** `{code,msg,data}`。
///
/// 契约要求插件回信封（`plugins/oj-mail` 的 `engine.rs` 已统一），但宿主不能把这条契约
/// 当成对**第三方/旧版**插件的保证：顶层无 `code` 的形态（如历史上的裸 `{"jobId":…}`
/// 或裸 data）一律包成 `{code:0,msg:"ok",data:<原值>}`，使 JS 侧契约
/// （`res.data.jobId`）不因插件形态差异而 TypeError，也让 `mail.result(jobId)` 拿得到 id。
fn ensure_envelope(v: Value) -> Value {
    match v.as_object() {
        Some(o) if o.contains_key("code") => v,
        _ => json!({ "code": 0, "msg": "ok", "data": v }),
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
        // 宿主侧驱动 FfiFuture：与 es/db/blob/bus 适配器同一 `await_ffi_poll`（poll + 退避
        // sleep；**不**用 `await_ffi` 的 `yield_now` —— SMTP 往返可达 `timeout`（默认 30s），
        // 空转会把该 isolate 的 `current_thread` runtime 烧满一核）。
        let bytes = super::ffi::await_ffi_poll(fut, super::ffi::FFI_POLL_BACKOFF)
            .await
            .map_err(|e| format!("ffi mail submit: {e}"))?;
        let v: Value = serde_json::from_slice(&bytes).map_err(
            |e| -> Box<dyn std::error::Error + Send + Sync> {
                format!("ffi mail submit decode: {e}").into()
            },
        )?;
        Ok(ensure_envelope(v))
    }

    fn config(&self) -> &MailConfig {
        &self.config
    }

    fn router(&self) -> &MailResultRouter {
        &self.router
    }

    /// 控制报文 drain（零 ABI 变更：走 `submit` 的 `{"__ctl":"drain"}`）。
    ///
    /// 契约（见 [`CONTROL_KEY`] 与插件 `engine.rs` 的 ctl 分支）：插件在 FFI 调用内**同步**
    /// 跑完 drain 并回**即可就绪**的 future ⇒ 这里只 poll 一次（不引入异步等待，也就不必
    /// 依赖宿主 reactor）。插件若违反契约（返回 pending）→ fail-loud 报错，绝不挂死。
    fn drain(&self, timeout: Duration) -> BridgeResult<Value> {
        let req = json!({
            CONTROL_KEY: CONTROL_DRAIN,
            "timeout_ms": timeout.as_millis() as u64,
        })
        .to_string();
        let fut = (self.vtable.submit)(
            RString::from(""),
            RString::from(req.as_str()),
            oj_plugin_ffi::RVec::new(),
        );
        let code = (fut.poll)(fut.state);
        if code == 0 {
            // 契约违约：控制报文必须**同步**完成。未就绪 ⇒ 只 free（不 take：FFI 契约要求
            // take 只在 ready 后调用），fail-loud —— 绝不挂死等它。
            (fut.free)(fut.state);
            return Err(
                "ffi mail drain: 插件回了 pending（poll=0）——控制报文契约要求同步完成".into(),
            );
        }
        let taken: Result<oj_plugin_ffi::RBytes, RString> =
            std::result::Result::from((fut.take)(fut.state));
        (fut.free)(fut.state); // FfiFuture 无 Drop：free 与 take 在这里配对
        let bytes = match (code, taken) {
            (1, Ok(b)) => b.iter().copied().collect::<Vec<u8>>(),
            (_, Err(e)) => return Err(format!("ffi mail drain: {}", &e[..]).into()),
            (c, Ok(_)) => {
                return Err(format!(
                    "ffi mail drain: poll={c} 却 take 成功 —— 插件违反 FfiFuture 协议"
                )
                .into());
            }
        };
        let v: Value = serde_json::from_slice(&bytes).map_err(
            |e| -> Box<dyn std::error::Error + Send + Sync> {
                format!("ffi mail drain decode: {e}").into()
            },
        )?;
        Ok(ensure_envelope(v))
    }
}

// ---------- 宿主侧入参校验（权威层；插件侧同款校验是纵深防御） ----------

/// 请求头里由结构化字段决定的名字：自定义头**不得**覆盖（否则信封可由报头派生，
/// 绕过收件人白名单）。与插件 `message.rs::STRUCTURED_HEADERS` 同清单。
const STRUCTURED_HEADERS: [&str; 5] = ["from", "to", "cc", "bcc", "subject"];

/// CRLF **剥离**（design §10「先剥 `\r\n`」）：头字段里的换行没有合法语义，
/// 删除即消除头注入（`subject: "a\r\nBcc: x"` → `"aBcc: x"`）。
/// 地址另走 [`validate_address`]——那里是**拒绝**（地址无「含换行的合法值」）。
/// 正文（`text`/`html`）与 `raw` 原文不在此列：换行在正文里有语义（design §7）。
pub fn strip_crlf(s: &str) -> String {
    s.chars().filter(|c| *c != '\r' && *c != '\n').collect()
}

/// 地址强校验：与插件/lettre 信封**同一解析器**（`lettre::Address`），
/// 保证宿主放行 ≡ 插件放行（口径不分裂）。
pub fn validate_address(s: &str) -> Result<Address, String> {
    if s.contains(['\r', '\n']) {
        return Err("地址含换行（CRLF 注入）".to_string());
    }
    s.parse::<Address>().map_err(|e| e.to_string())
}

/// 白名单条目（B1）的两种**合法**形态（不做子域通配、不做裸后缀匹配）。
///
/// | 写法 | 语义 | 命中 | 不命中 |
/// |---|---|---|---|
/// | `@x.com` | 收件人**域全等** | `a@x.com` | `a@sub.x.com`（子域须显式写 `@sub.x.com`） |
/// | `noreply@x.com` | 与地址**全等**（大小写不敏感） | `noreply@x.com` | `evil-noreply@x.com` |
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WhitelistEntry {
    /// `@domain`：域全等（小写）。
    Domain(String),
    /// 完整地址：地址全等（小写）。
    Address(String),
}

/// 解析并**校验**一条白名单条目。装配期（[`MailConfig::from_value`]）与匹配期
/// （[`whitelist_hit`]）用**同一函数** ⇒ 条目语义只有一份定义，不会解析/匹配口径分裂。
///
/// 非法形态一律拒绝（历史三处绕过：裸后缀让 `noreply@x.com` 放行 `evil-noreply@x.com`、
/// 漏写 `@` 的条目放行跨域 `a@evilx.com`、空条目 `ends_with("")` 恒真把白名单关掉）：
/// - 空串 / 只有空白：空条目会命中一切；
/// - 含首尾空白：`" noreply@x.com"` 与 `"noreply@x.com "` 都是配置笔误，且会导致
///   「配置里看着对、实际不命中」；
/// - 不以 `@` 开头且不含 `@`：没有 `@` 就没有域边界，无从做全等；
/// - `@` 后为空（`"@"`）或域/地址不是合法地址：交给 `lettre::Address`（与投递侧同一
///   解析器，避免「宿主放行 / 插件拒绝」的分裂）。
///
/// 返回值是**原因**（不含位置与下一步）：用户面文案由调用方补上 profile/字段/下标与
/// 下一步（见 [`whitelist_list`]）。
pub fn parse_whitelist_entry(raw: &str) -> Result<WhitelistEntry, String> {
    if raw.trim() != raw {
        return Err("含首尾空白".to_string());
    }
    if raw.is_empty() {
        return Err("空串（空条目会命中一切 = 白名单失效）".to_string());
    }
    if let Some(domain) = raw.strip_prefix('@') {
        if domain.is_empty() {
            return Err("'@' 后缺少域".to_string());
        }
        // `@domain` 的域校验复用同一解析器：造一个探针地址交给 lettre 判定。
        validate_address(&format!("probe@{domain}"))
            .map_err(|e| format!("域 '{domain}' 非法：{e}"))?;
        return Ok(WhitelistEntry::Domain(domain.to_ascii_lowercase()));
    }
    if !raw.contains('@') {
        return Err(
            "缺少 '@'：须为完整地址（user@domain）或 @domain 形式（裸域/裸后缀会放行同域仿冒与跨域收件）"
                .to_string(),
        );
    }
    validate_address(raw).map_err(|e| format!("不是合法地址：{e}"))?;
    Ok(WhitelistEntry::Address(raw.to_lowercase()))
}

/// 单条条目与地址是否命中（大小写不敏感）。`addr` 须是已规范化的地址。
fn entry_matches(entry: &WhitelistEntry, addr: &str) -> bool {
    match entry {
        WhitelistEntry::Domain(d) => addr
            .rsplit_once('@')
            .is_some_and(|(_, dom)| dom.eq_ignore_ascii_case(d)),
        WhitelistEntry::Address(a) => addr.eq_ignore_ascii_case(a),
    }
}

/// 白名单命中判定：逐条解析后做**全等**比较。
///
/// 非法条目视为「不命中」（fail-closed）——装配期已 fail-fast，这里只兜底
/// `MailConfig::new` 直接构造（绕过解析）的情形，绝不因条目写错而放宽匹配。
fn whitelist_hit(list: &[String], addr: &str) -> bool {
    list.iter()
        .any(|raw| matches!(parse_whitelist_entry(raw), Ok(e) if entry_matches(&e, addr)))
}

/// 白名单条目清单（配置期）：类型校验 + **逐条格式校验**（fail-fast）。
///
/// 格式非法在装配期即报错（点名 profile/字段/下标/条目原文 + 下一步），而不是等到发信
/// 时表现为「白名单没生效」——那会把配置错误伪装成权限问题。
fn whitelist_list(v: Option<&Value>, profile: &str, field: &str) -> BridgeResult<Vec<String>> {
    let out = str_list(v, profile, field)?;
    for (i, raw) in out.iter().enumerate() {
        parse_whitelist_entry(raw).map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
            format!(
                "smtp.{profile}.{field}[{i}] 条目 {raw:?} 非法：{e}（下一步：写成完整地址 user@domain，或 @domain 形式）"
            )
            .into()
        })?;
    }
    Ok(out)
}

/// profile 白名单（design §10）：`from` 命中 `allowed_from`，全部收件人
/// （to/cc/bcc）命中 `allowed_recipients`。
///
/// 匹配语义见 [`WhitelistEntry`]：条目要么是 `@domain`（域全等），要么是完整地址
/// （地址全等）——**没有**后缀/子域通配，故 `noreply@x.com` 不会放行
/// `evil-noreply@x.com`，`@x.com` 不会放行 `a@evilx.com` 或 `a@sub.x.com`。
///
/// **空表 = 拒绝**（fail-closed）：白名单是「越权发送」的唯一控制点，
/// 缺省放行等于默认开成开放中继；与本特性 `tls: none` 需显式许可同一取向。
pub fn check_whitelist(from: &str, rcpts: &[String], cfg: &MailProfileCfg) -> Result<(), String> {
    if !whitelist_hit(&cfg.allowed_from, from) {
        return Err(format!(
            "from '{from}' 不在 allowed_from 白名单（{:?}）（下一步：在 smtp 配置里补白名单条目，或改用允许的发件人）",
            cfg.allowed_from
        ));
    }
    for r in rcpts {
        if !whitelist_hit(&cfg.allowed_recipients, r) {
            return Err(format!(
                "收件人 '{r}' 不在 allowed_recipients 白名单（{:?}）（下一步：在 smtp 配置里补白名单条目，或去掉该收件人）",
                cfg.allowed_recipients
            ));
        }
    }
    Ok(())
}

/// 可选字符串字段：缺失/`null` → None；给了别的类型 → Err（明确拒绝，不静默丢弃）。
fn opt_str(obj: &Map<String, Value>, field: &str) -> Result<Option<String>, String> {
    match obj.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("mail: {field} 必须是字符串")),
    }
}

/// 地址数组字段（`to`/`cc`/`bcc`）：缺失 = 空表；非数组/非字符串/地址非法 → Err。
fn address_list(obj: &Map<String, Value>, field: &str) -> Result<Vec<Address>, String> {
    let Some(v) = obj.get(field) else {
        return Ok(Vec::new());
    };
    if v.is_null() {
        return Ok(Vec::new());
    }
    let arr = v
        .as_array()
        .ok_or_else(|| format!("mail: {field} 必须是字符串数组"))?;
    let mut out = Vec::with_capacity(arr.len());
    for (i, a) in arr.iter().enumerate() {
        let s = a
            .as_str()
            .ok_or_else(|| format!("mail: {field}[{i}] 必须是字符串"))?;
        out.push(validate_address(s).map_err(|e| {
            format!("mail: {field}[{i}] 地址非法：{e}（下一步：改为 user@domain 形式）")
        })?);
    }
    Ok(out)
}

/// 自定义报头规范化：名字与值均剥 CRLF；禁止覆盖结构化头（防白名单绕过）。
fn normalize_headers(obj: &mut Map<String, Value>) -> Result<(), String> {
    let Some(h) = obj.get("headers") else {
        return Ok(());
    };
    if h.is_null() {
        obj.remove("headers");
        return Ok(());
    }
    let m = h
        .as_object()
        .ok_or_else(|| "mail: headers 必须是对象（名 → 值）".to_string())?;
    let mut out = Map::new();
    for (k, v) in m {
        let name = strip_crlf(k);
        let val = v
            .as_str()
            .ok_or_else(|| format!("mail: headers['{name}'] 必须是字符串"))?;
        if STRUCTURED_HEADERS
            .iter()
            .any(|s| name.eq_ignore_ascii_case(s))
        {
            return Err(format!(
                "mail: headers 不允许覆盖 {name}（From/To/Cc/Bcc/Subject 由 From/to/cc/bcc/subject 决定；下一步：换一个自定义头名）"
            ));
        }
        out.insert(name, Value::String(strip_crlf(val)));
    }
    obj.insert("headers".to_string(), Value::Object(out));
    Ok(())
}

// ---------- 附件引用（引用式 → 字节） ----------

/// 附件字节来源：`blobKey`（blob 后端）或 `path`（项目根内本地文件）——**二选一**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttSrc {
    /// `blob` 字段给后端名（缺省 "default"），`key` 为 blobKey。
    Blob { name: String, key: String },
    /// 项目根相对路径（绝对路径亦可，但必须落在项目根内）。
    Path(PathBuf),
}

/// 一条附件引用（宿主据此取字节；`filename`/`mime` 供展示）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentRef {
    pub filename: String,
    /// 显式 MIME（`None` = 由扩展名/字节嗅探决定）。
    pub mime: Option<String>,
    pub src: AttSrc,
}

/// 解析 `attachments[]`：每个元素必须且只能给 `blobKey`/`path` 之一（与插件同款判定），
/// 且必须有非空 `filename`（插件 `AttachmentRef.filename` 为必填）。
/// 非数组 / 元素非对象 / 字段类型错 → Err（文案给下一步）。
pub fn parse_attachment_refs(v: &Value) -> Result<Vec<AttachmentRef>, String> {
    if v.is_null() {
        return Ok(Vec::new());
    }
    let arr = v
        .as_array()
        .ok_or_else(|| "mail: attachments 必须是数组".to_string())?;
    let mut out = Vec::with_capacity(arr.len());
    for (i, a) in arr.iter().enumerate() {
        let o = a.as_object().ok_or_else(|| {
            format!("mail: attachments[{i}] 必须是对象（{{filename, blobKey|path}}）")
        })?;
        let filename = match o.get("filename") {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(s)) => s.clone(),
            Some(_) => return Err(format!("mail: attachments[{i}].filename 必须是字符串")),
        };
        if filename.is_empty() {
            return Err(format!(
                "mail: attachments[{i}] 缺少 filename（下一步：给出收件人可见的文件名）"
            ));
        }
        let mime = match o.get("mime") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(s.clone()),
            Some(_) => return Err(format!("mail: attachments[{i}].mime 必须是字符串")),
        };
        let blob_key = o
            .get("blobKey")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let path = o
            .get("path")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let src = match (blob_key, path) {
            (Some(k), None) => AttSrc::Blob {
                name: o
                    .get("blob")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("default")
                    .to_string(),
                key: k.to_string(),
            },
            (None, Some(p)) => AttSrc::Path(PathBuf::from(p)),
            _ => {
                return Err(format!(
                    "mail: attachments[{i}] 必须且只能给 blobKey 或 path 之一（下一步：二选一）"
                ));
            }
        };
        out.push(AttachmentRef {
            filename,
            mime,
            src,
        });
    }
    Ok(out)
}

/// MIME 决议：**显式优先** → 扩展名 → 字节嗅探 → `application/octet-stream`
/// （design §9；与插件 `DEFAULT_MIME` 同兜底）。
pub fn resolve_mime(explicit: Option<&str>, name: &str, bytes: &[u8]) -> String {
    if let Some(m) = explicit.filter(|m| !m.is_empty()) {
        return m.to_string();
    }
    if let Some(ext) = Path::new(name).extension().and_then(|e| e.to_str())
        && let Some(m) = mime_by_ext(&ext.to_ascii_lowercase())
    {
        return m.to_string();
    }
    if let Some(m) = mime_by_magic(bytes) {
        return m.to_string();
    }
    "application/octet-stream".to_string()
}

fn mime_by_ext(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "pdf" => "application/pdf",
        "txt" | "log" => "text/plain",
        "csv" => "text/csv",
        "json" => "application/json",
        "xml" => "application/xml",
        "html" | "htm" => "text/html",
        "md" => "text/markdown",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "eml" => "message/rfc822",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        _ => return None,
    })
}

/// 魔数嗅探（扩展名缺失/未知时的兜底；覆盖最常见的二进制族）。
fn mime_by_magic(bytes: &[u8]) -> Option<&'static str> {
    const SIGS: [(&[u8], &str); 6] = [
        (b"%PDF-", "application/pdf"),
        (b"\x89PNG\r\n\x1a\n", "image/png"),
        (b"\xff\xd8\xff", "image/jpeg"),
        (b"GIF87a", "image/gif"),
        (b"GIF89a", "image/gif"),
        (b"PK\x03\x04", "application/zip"),
    ];
    SIGS.iter()
        .find(|(sig, _)| bytes.starts_with(sig))
        .map(|(_, m)| *m)
        .or_else(|| bytes.starts_with(b"\x1f\x8b").then_some("application/gzip"))
}

/// 附件解析：按引用取字节 → `ParsedAttachment`（**下标与 refs 严格一致**）。
///
/// - `blobKey`：经 `StableState.blobs` 注册表（本地/S3 统一）；后端缺失/键缺失 → Err 给下一步。
/// - `path`：`ensure_within` 钳制到项目根（符号链接经 canonical 化覆盖），
///   并**按返回的 canonical 句柄读盘**（校验路径 ≡ 读盘路径，design §9 TOCTOU）。
/// - **上限（B2）**：单附件与单封合计都按 `limits` 判定，超限 → Err（`code:5`）。
///   读盘走 `tokio::fs`（`spawn_blocking`），不阻塞 isolate 的 `current_thread`；
///   path 路先 `metadata` 拿长度，超限即拒（不把大文件读进内存）。
async fn resolve_attachments(
    blobs: &BlobRegistry,
    root: Option<&Path>,
    refs: &[AttachmentRef],
    limits: AttachmentLimits,
) -> Result<Vec<ParsedAttachment>, String> {
    let mut out = Vec::with_capacity(refs.len());
    for (i, r) in refs.iter().enumerate() {
        let bytes = match &r.src {
            AttSrc::Blob { name, key } => {
                let b = blobs.get(name).ok_or_else(|| {
                    format!(
                        "mail: attachments[{i}] 的 blob 后端 '{name}' not configured（下一步：在 config 里配置 blob.backends.{name}）"
                    )
                })?;
                b.get(key).await.map_err(|e| {
                    format!(
                        "mail: attachments[{i}] 读取 blobKey '{key}' 失败：{e}（下一步：确认键存在且当前后端可读）"
                    )
                })?
            }
            AttSrc::Path(p) => {
                let root = root.ok_or_else(|| {
                    format!(
                        "mail: attachments[{i}] 的 path 附件需要 project root（loader 未配置；dev/release 均由 --api-path 提供）（下一步：改用 blobKey）"
                    )
                })?;
                let full = if p.is_absolute() {
                    p.clone()
                } else {
                    root.join(p)
                };
                let canon = ensure_within(&full, root).map_err(|e| {
                    format!(
                        "mail: attachments[{i}] 附件路径非法：{e}（下一步：把附件放到项目根内，或用 blobKey）"
                    )
                })?;
                // 先看长度再读：超限的文件不进内存（读盘本身也走阻塞池，不占 isolate 线程）。
                let len = tokio::fs::metadata(&canon)
                    .await
                    .map_err(|e| {
                        format!(
                            "mail: attachments[{i}] 读取 {} 失败：{e}（下一步：确认文件存在且可读）",
                            canon.display()
                        )
                    })?
                    .len();
                if len > limits.max_attachment_bytes as u64 {
                    return Err(over_single_limit(i, &r.filename, len, limits));
                }
                tokio::fs::read(&canon).await.map_err(|e| {
                    format!(
                        "mail: attachments[{i}] 读取 {} 失败：{e}（下一步：确认文件存在且可读）",
                        canon.display()
                    )
                })?
            }
        };
        // blob 字节只有拿到才知道长度（后端无 size 接口）→ 在此判；path 路此处是二道防线
        // （metadata 与 read 之间文件可能变大）。
        if bytes.len() > limits.max_attachment_bytes {
            return Err(over_single_limit(
                i,
                &r.filename,
                bytes.len() as u64,
                limits,
            ));
        }
        let total: usize = out
            .iter()
            .map(|a: &ParsedAttachment| a.bytes.len())
            .sum::<usize>()
            .saturating_add(bytes.len());
        if total > limits.max_total_bytes {
            return Err(format!(
                "mail: attachments[{i}]（{}）加入后附件合计 {total} 字节，超过单封上限 {} 字节（smtp.max_total_attachment_bytes）（下一步：减小附件或减少数量，或调大 smtp.max_total_attachment_bytes）",
                r.filename, limits.max_total_bytes
            ));
        }
        let mime = resolve_mime(r.mime.as_deref(), &r.filename, &bytes);
        out.push(ParsedAttachment {
            filename: r.filename.clone(),
            mime,
            bytes,
        });
    }
    Ok(out)
}

/// 单附件超限文案（一处定义：metadata 预判与字节复核共用）。
fn over_single_limit(i: usize, filename: &str, len: u64, limits: AttachmentLimits) -> String {
    format!(
        "mail: attachments[{i}]（{filename}）{len} 字节超过单附件上限 {} 字节（smtp.max_attachment_bytes）（下一步：换更小的附件，或调大 smtp.max_attachment_bytes）",
        limits.max_attachment_bytes
    )
}

// ---------- 编排：校验 → 附件 → submit ----------

/// 投递模式（`Mail` 的四个方法 → 引擎开关 + 校验差异）。
/// **宿主权威**：op 覆写 req 里的 `sync`/`enqueue_only`，JS 侧串用无效。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailMode {
    /// 异步 transport，future resolve = 投递结果。
    Send,
    /// 同步 transport（插件 worker 内 `spawn_blocking`）。
    Sync,
    /// 入队即返回 `{jobId}`，真实完成经 `deliver` 上送。
    Enqueue,
    /// 原始 MIME（`raw`）+ 结构化 `from`/`to` 作信封；与 `attachments` 互斥。
    Raw,
}

fn code5(msg: &str) -> Value {
    json!({ "code": 5, "msg": msg, "data": {} })
}

/// 校验并**就地规范化**请求；成功返回附件引用表（供字节解析）。
///
/// 规范化内容：CRLF 剥离（subject/headers）、地址规范化（lettre 解析后的规范写法）、
/// 开关由 op 覆写、`headers` 重建（剥 CRLF + 禁覆盖结构化头）。
fn validate_request(
    key: &str,
    req: &mut Value,
    cfg: &MailConfig,
    mode: MailMode,
) -> Result<Vec<AttachmentRef>, String> {
    let raw_mode = mode == MailMode::Raw;
    let obj = req
        .as_object_mut()
        .ok_or_else(|| "mail: 请求必须是对象（{from, to, text|html}）".to_string())?;
    let prof = cfg.profile(key).ok_or_else(|| {
        format!(
            "mail: unknown mail profile '{key}'（已知：{:?}）（下一步：改用已配置的 profile key）",
            cfg.profile_keys()
        )
    })?;

    // 地址：强校验（拒绝非法/含换行）→ 规范化写回。
    let from = opt_str(obj, "from")?
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "mail: 缺少 from（发件人）（下一步：给出 user@domain 形式）".to_string())?;
    let from = validate_address(&from)
        .map_err(|e| format!("mail: from 地址非法：{e}（下一步：改为 user@domain 形式）"))?;
    let to = address_list(obj, "to")?;
    let cc = address_list(obj, "cc")?;
    let bcc = address_list(obj, "bcc")?;
    if to.is_empty() {
        return Err(
            "mail: 缺少收件人 to（至少一个；只抄送请同时给 to）（下一步：补 to）".to_string(),
        );
    }
    let rcpts: Vec<String> = to
        .iter()
        .chain(cc.iter())
        .chain(bcc.iter())
        .map(|a| a.to_string())
        .collect();
    let from_norm: &str = from.as_ref();
    check_whitelist(from_norm, &rcpts, prof)?;
    obj.insert("from".into(), Value::String(from_norm.to_string()));
    for (field, list) in [("to", &to), ("cc", &cc), ("bcc", &bcc)] {
        let v: Vec<Value> = list.iter().map(|a| Value::String(a.to_string())).collect();
        obj.insert(field.into(), Value::Array(v));
    }

    // 主题（剥离）与报头（剥离 + 禁覆盖）；正文原样（正文换行有语义）。
    if let Some(s) = opt_str(obj, "subject")? {
        obj.insert("subject".into(), Value::String(strip_crlf(&s)));
    }
    let text = opt_str(obj, "text")?;
    let html = opt_str(obj, "html")?;
    normalize_headers(obj)?;

    // raw 与结构化路互斥：非 raw 模式不得夹带 raw（否则插件静默走原始 MIME 路）。
    let raw = opt_str(obj, "raw")?;
    match (raw_mode, raw) {
        (true, None) => {
            return Err(
                "mail: sendRaw 需要 raw（RFC5322 原文）（下一步：把原文放进 raw 字段）".to_string(),
            );
        }
        (true, Some(r)) if r.is_empty() => {
            return Err("mail: sendRaw 的 raw 不能为空（下一步：给出完整原文）".to_string());
        }
        (false, Some(r)) if !r.is_empty() => {
            return Err(
                "mail: 非 sendRaw 调用不得带 raw（下一步：改用 mail.sendRaw，或去掉 raw）"
                    .to_string(),
            );
        }
        (false, _) => {
            obj.remove("raw"); // 空串/缺省：清掉，避免插件误入原始 MIME 路
        }
        (true, Some(_)) => {}
    }

    // 附件：raw 路不解析（与原文互斥）；结构化路按下标解析。
    if raw_mode {
        let has_atts = obj
            .get("attachments")
            .is_some_and(|a| !a.is_null() && a.as_array().is_none_or(|arr| !arr.is_empty()));
        if has_atts {
            return Err(
                "mail: sendRaw 与 attachments 互斥（raw 原文自带内容）（下一步：去掉 attachments 或改用 send）"
                    .to_string(),
            );
        }
        obj.remove("attachments");
        return Ok(Vec::new());
    }
    let refs = parse_attachment_refs(obj.get("attachments").unwrap_or(&Value::Null))?;
    // 与插件 build_message 同款：text 与 html 都缺且无附件 → 缺正文。
    if text.is_none() && html.is_none() && refs.is_empty() {
        return Err(
            "mail: 缺少正文（text 与 html 至少给一个，或带附件）（下一步：补 text/html）"
                .to_string(),
        );
    }
    Ok(refs)
}

/// 一次投递的完整宿主流程（op 与测试共用编排）：
/// 校验（权威层）→ 附件字节解析 → `submit`。
///
/// 返回**结果信封**（与插件同款 `{code,msg,data}`）：校验/解析失败 → `{code:5}`，
/// FFI 层失败 → `{code:1}`（连接/网络类）。即 JS 侧 `mail.*` 一律 resolve 信封，
/// 不因邮件内容问题抛异常；仅「未配置 mail」在 op 层抛（见 `op_mail_send`）。
pub async fn handle_send(
    backend: Arc<dyn MailBackend>,
    blobs: Arc<BlobRegistry>,
    project_root: Option<&Path>,
    key: &str,
    req_json: &str,
    mode: MailMode,
) -> Value {
    let mut req: Value = match serde_json::from_str(req_json) {
        Ok(v) => v,
        Err(e) => return code5(&format!("mail: 请求不是合法 JSON：{e}")),
    };
    let refs = {
        let cfg = backend.config();
        match validate_request(key, &mut req, cfg, mode) {
            Ok(r) => r,
            Err(e) => return code5(&e),
        }
    };
    let limits = backend.config().attachment_limits();
    let atts = match resolve_attachments(&blobs, project_root, &refs, limits).await {
        Ok(a) => a,
        Err(e) => return code5(&e),
    };
    if let Some(o) = req.as_object_mut() {
        o.insert("sync".into(), Value::Bool(mode == MailMode::Sync));
        o.insert(
            "enqueue_only".into(),
            Value::Bool(mode == MailMode::Enqueue),
        );
    }
    match backend.submit(key, req.to_string(), atts).await {
        Ok(env) => env,
        Err(e) => json!({
            "code": 1,
            "msg": format!("mail: 投递未能送达插件（{e}）（下一步：确认 oj-mail 插件已装配、profile 名正确）"),
            "data": {},
        }),
    }
}

// ---------- JS 侧入口（ops） ----------

/// op 层依赖三元组：mail 后端 / blob 注册表 / 项目根（path 附件钳制基准）。
type MailDeps = (Arc<dyn MailBackend>, Arc<BlobRegistry>, Option<PathBuf>);

fn mail_deps(state: &OpState) -> Result<MailDeps, JsErrorBox> {
    let st = state.borrow::<Arc<super::StableState>>();
    let backend = st.mail.clone().ok_or_else(|| {
        JsErrorBox::generic(
            "mail not configured (config smtp: section missing, or oj-mail plugin not loaded)",
        )
    })?;
    Ok((
        backend,
        st.blobs.clone(),
        st.loader.as_ref().map(|l| l.project_root.clone()),
    ))
}

async fn op_send(
    state: Rc<RefCell<OpState>>,
    key: String,
    req_json: String,
    mode: MailMode,
) -> Result<serde_json::Value, JsErrorBox> {
    let (backend, blobs, root) = mail_deps(&state.borrow())?;
    Ok(handle_send(backend, blobs, root.as_deref(), &key, &req_json, mode).await)
}

/// mail.send(m)：异步 transport，resolve 投递结果信封。
#[op2]
#[serde]
pub async fn op_mail_send(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
    #[string] req_json: String,
) -> Result<serde_json::Value, JsErrorBox> {
    op_send(state, key, req_json, MailMode::Send).await
}

/// mail.sendSync(m)：同步 transport（插件 worker 内 spawn_blocking）。
#[op2]
#[serde]
pub async fn op_mail_send_sync(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
    #[string] req_json: String,
) -> Result<serde_json::Value, JsErrorBox> {
    op_send(state, key, req_json, MailMode::Sync).await
}

/// mail.enqueue(m)：入队即返回 `{jobId}`；真实完成经 `mail.result` 上送。
#[op2]
#[serde]
pub async fn op_mail_enqueue(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
    #[string] req_json: String,
) -> Result<serde_json::Value, JsErrorBox> {
    op_send(state, key, req_json, MailMode::Enqueue).await
}

/// mail.sendRaw(o)：原始 MIME 投递（`raw` + 结构化 `from`/`to` 作信封）。
#[op2]
#[serde]
pub async fn op_mail_send_raw(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
    #[string] req_json: String,
) -> Result<serde_json::Value, JsErrorBox> {
    op_send(state, key, req_json, MailMode::Raw).await
}

/// mail.result(id)：查宿主侧存储的异步结果（未命中/已过期 → `null`）。
/// `key` 保留形参仅为对齐 `Mail` 实例方法签名：结果按 `jobId` 全局索引（与 profile 无关）。
#[op2]
#[serde]
pub async fn op_mail_result(
    state: Rc<RefCell<OpState>>,
    #[string] _key: String,
    #[string] job_id: String,
) -> Result<serde_json::Value, JsErrorBox> {
    let (backend, ..) = mail_deps(&state.borrow())?;
    Ok(backend.router().get(&job_id).unwrap_or(Value::Null))
}

/// mail.profiles()：已配置的 profile 名清单（**非密钥面**：凭据/连接字段不进 JS）。
#[op2]
#[serde]
pub async fn op_mail_profiles(
    state: Rc<RefCell<OpState>>,
) -> Result<serde_json::Value, JsErrorBox> {
    let (backend, ..) = mail_deps(&state.borrow())?;
    Ok(json!(backend.config().profile_keys()))
}

#[cfg(test)]
// 全局 deliver 槽是进程级的，用例须串行；同批用例排队，不与其它的锁形成环（同
// `ffi.rs::adapter_tests` 的豁免理由）。改成 drop 再 await 反而会失去串行化。
#[allow(clippy::await_holding_lock)]
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
    static T_LOCK: Mutex<()> = Mutex::new(());

    /// 取共享锁：用例 panic 后锁被毒化，后续用例取 `into_inner()` 继续（不连锁失败）。
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        T_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

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
        let _g = lock();
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

    /// 未配置（StableState.mail = None）→ 路由明确报「未配置」（与「载荷非法」分开，
    /// 告警文案据此区分）；不 panic（结果丢弃）。
    #[tokio::test(flavor = "current_thread")]
    async fn route_deliver_without_backend_is_not_configured() {
        let _g = lock();
        let _b = Bridge::new(
            Arc::new(InMemoryAccessor::new()),
            Arc::new(InMemoryKV::new()),
        );
        assert!(matches!(
            route_deliver(ENVELOPE),
            DeliverRoute::NotConfigured
        ));
    }

    /// A6：**载荷非法** ≠ 「未配置」—— 有后端但 payload 不是结果信封（非 JSON / 缺
    /// `jobId`/`code`/`msg`）时细分报 `BadPayload`，告警文案才能指向插件侧。
    #[tokio::test(flavor = "current_thread")]
    async fn route_deliver_with_backend_but_bad_payload_is_bad_payload() {
        let _g = lock();
        let bus = Arc::new(Bus::new());
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
        for bad in [
            &b"not json"[..],
            &br#"{"code":0,"msg":"ok","data":{}}"#[..], // 缺 jobId
            &br#"{"data":{"jobId":"j"}}"#[..],          // 缺 code/msg
        ] {
            assert!(
                matches!(route_deliver(bad), DeliverRoute::BadPayload),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
        // 合法信封 → Routed（对照：不是所有载荷都被判非法）。
        assert!(matches!(route_deliver(ENVELOPE), DeliverRoute::Routed));
        assert!(fake.router().get("j1").is_some());
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

    /// 假 vtable 用的 **ready** FfiFuture 状态（poll/take/free 三指针，与 `ffi.rs`
    /// 适配器测试同款）；`FREED` 记录 free 次数（句柄必须释放）。
    /// `FREED` 是进程级静态：用它做断言的用例须持 [`lock`]（串行化，取增量）。
    struct ReadyState(Option<Result<Vec<u8>, String>>);
    static FREED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    extern "C" fn ready_poll(state: *mut std::ffi::c_void) -> i32 {
        let s = unsafe { &mut *(state as *mut ReadyState) };
        match &s.0 {
            Some(Ok(_)) => 1,
            Some(Err(_)) => -1,
            None => 0,
        }
    }

    extern "C" fn ready_take(
        state: *mut std::ffi::c_void,
    ) -> oj_plugin_ffi::RResult<oj_plugin_ffi::RBytes, oj_plugin_ffi::RString> {
        let s = unsafe { &mut *(state as *mut ReadyState) };
        match s.0.take() {
            Some(Ok(b)) => oj_plugin_ffi::RResult::Ok(oj_plugin_ffi::RBytes::from(&b[..])),
            Some(Err(e)) => oj_plugin_ffi::RResult::Err(oj_plugin_ffi::RString::from(e.as_str())),
            None => oj_plugin_ffi::RResult::Err(oj_plugin_ffi::RString::from("not ready")),
        }
    }

    extern "C" fn ready_free(state: *mut std::ffi::c_void) {
        if !state.is_null() {
            FREED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            drop(unsafe { Box::from_raw(state as *mut ReadyState) });
        }
    }

    fn ready_future(r: Result<Vec<u8>, String>) -> oj_plugin_ffi::FfiFuture {
        oj_plugin_ffi::FfiFuture {
            state: Box::into_raw(Box::new(ReadyState(Some(r)))).cast(),
            poll: ready_poll,
            take: ready_take,
            free: ready_free,
        }
    }

    /// 单方法假 vtable 的返回体（用例串行「设置 → 立即调用」，无并发窗口）。
    static SUBMIT_BODY: Mutex<Vec<u8>> = Mutex::new(Vec::new());

    extern "C" fn body_submit(
        _key: oj_plugin_ffi::RString,
        _req: oj_plugin_ffi::RString,
        _atts: oj_plugin_ffi::RVec<oj_plugin_ffi::MailAttachment>,
    ) -> oj_plugin_ffi::FfiFuture {
        ready_future(Ok(SUBMIT_BODY.lock().unwrap().clone()))
    }

    /// 构造「`submit` 恒返回 `body`」的假 vtable。
    fn vtable_returning(body: &[u8]) -> &'static MailVtable {
        *SUBMIT_BODY.lock().unwrap() = body.to_vec();
        Box::leak(Box::new(MailVtable {
            submit: body_submit,
        }))
    }

    /// 记录 fake `submit` 收到的 `(key, req)`（A4 控制报文用）。
    static SEEN_SUBMIT: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

    extern "C" fn record_submit(
        key: oj_plugin_ffi::RString,
        req: oj_plugin_ffi::RString,
        _atts: oj_plugin_ffi::RVec<oj_plugin_ffi::MailAttachment>,
    ) -> oj_plugin_ffi::FfiFuture {
        SEEN_SUBMIT
            .lock()
            .unwrap()
            .push((key[..].to_string(), req[..].to_string()));
        ready_future(Ok(SUBMIT_BODY.lock().unwrap().clone()))
    }

    /// A4：`FfiMailBackend::drain` 必须发**控制报文** `{"__ctl":"drain","timeout_ms":N}`
    /// （key 为空 —— 控制报文不占 profile），并把插件的排空信封解回。
    #[tokio::test(flavor = "current_thread")]
    async fn ffi_mail_backend_drain_sends_control_message_and_decodes_envelope() {
        let _g = lock(); // 与用 SUBMIT_BODY/SEEN_SUBMIT 的用例串行
        let vt: &'static MailVtable = Box::leak(Box::new(MailVtable {
            submit: record_submit,
        }));
        *SUBMIT_BODY.lock().unwrap() =
            br#"{"code":0,"msg":"ok","data":{"drained":true,"workers":2}}"#.to_vec();
        SEEN_SUBMIT.lock().unwrap().clear();

        let b = FfiMailBackend::new(vt, MailConfig::empty(), Arc::new(Bus::new()));
        let env = b.drain(Duration::from_millis(2500)).expect("drain 信封");
        assert_eq!(env["code"], 0, "{env}");
        assert_eq!(env["data"]["drained"], true, "{env}");
        assert_eq!(env["data"]["workers"], 2, "{env}");

        let seen = SEEN_SUBMIT.lock().unwrap().clone();
        assert_eq!(seen.len(), 1, "控制报文只发一次");
        assert_eq!(seen[0].0, "", "控制报文不占 profile key");
        let req: Value = serde_json::from_str(&seen[0].1).unwrap();
        assert_eq!(req[CONTROL_KEY], CONTROL_DRAIN, "{req}");
        assert_eq!(req["timeout_ms"], 2500, "超时必须过线（插件据此等）：{req}");
    }

    /// A4 契约边界：插件回 **pending**（违反「控制报文同步完成」契约）或坏 JSON → 一律
    /// fail-loud 报错，**绝不挂死**（drain 只有一次 poll，不做异步等待）。
    #[tokio::test(flavor = "current_thread")]
    async fn ffi_mail_backend_drain_fails_loud_on_pending_or_garbage() {
        let _g = lock();
        extern "C" fn pending_poll(_s: *mut std::ffi::c_void) -> i32 {
            0
        }
        extern "C" fn pending_take(
            _s: *mut std::ffi::c_void,
        ) -> oj_plugin_ffi::RResult<oj_plugin_ffi::RBytes, oj_plugin_ffi::RString> {
            oj_plugin_ffi::RResult::Err(oj_plugin_ffi::RString::from("not ready"))
        }
        extern "C" fn pending_free(_s: *mut std::ffi::c_void) {}
        extern "C" fn pending_submit(
            _k: oj_plugin_ffi::RString,
            _r: oj_plugin_ffi::RString,
            _a: oj_plugin_ffi::RVec<oj_plugin_ffi::MailAttachment>,
        ) -> oj_plugin_ffi::FfiFuture {
            oj_plugin_ffi::FfiFuture {
                state: std::ptr::null_mut(),
                poll: pending_poll,
                take: pending_take,
                free: pending_free,
            }
        }
        let vt: &'static MailVtable = Box::leak(Box::new(MailVtable {
            submit: pending_submit,
        }));
        let b = FfiMailBackend::new(vt, MailConfig::empty(), Arc::new(Bus::new()));
        let e = b.drain(Duration::from_millis(10)).unwrap_err().to_string();
        assert!(e.contains("pending"), "pending 必须点名契约违约：{e}");

        // 坏 JSON（非 UTF-8/非 JSON）→ decode 错误臂。
        let vt = vtable_returning(b"{not json");
        let b = FfiMailBackend::new(vt, MailConfig::empty(), Arc::new(Bus::new()));
        let e = b.drain(Duration::from_millis(10)).unwrap_err().to_string();
        assert!(e.contains("ffi mail drain decode"), "{e}");
    }

    /// 适配器把宿主办的 `Vec<ParsedAttachment>` 按**下标原序**填进 `RVec<MailAttachment>`
    /// （插件按 index 对齐，数量/顺序错位即 code:5），并把 future 结果（信封 JSON）解回。
    #[tokio::test(flavor = "current_thread")]
    async fn ffi_mail_backend_forwards_key_and_ordered_attachments() {
        use std::sync::atomic::Ordering;
        let _g = lock(); // FREED 是进程级静态：本用例取增量，须与用它的用例串行
        /// 假 vtable 记录的一次调用：key / req / 附件（filename, mime, bytes）。
        type Seen = (String, String, Vec<(String, String, Vec<u8>)>);
        static SEEN: Mutex<Vec<Seen>> = Mutex::new(Vec::new());
        let freed_before = FREED.load(Ordering::SeqCst);

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
            ready_future(Ok(
                br#"{"code":0,"msg":"sent","data":{"jobId":"j9","messageId":"m9"}}"#.to_vec(),
            ))
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
        assert_eq!(
            FREED.load(Ordering::SeqCst),
            freed_before + 1,
            "future 句柄须 free（本用例增量 1）"
        );
    }

    /// A1 **纵深防御**：插件回**裸**载荷（旧版/第三方形态，如 `{"jobId":"j-bare"}`）时，
    /// 宿主必须包成统一信封 —— JS 侧 `res.data.jobId` 契约不因插件形态差异而 TypeError，
    /// 且 `mail.result(jobId)` 拿得到 id。
    #[tokio::test(flavor = "current_thread")]
    async fn ffi_mail_backend_wraps_bare_plugin_payload_into_envelope() {
        let _g = lock(); // 与用 FREED 的用例串行（同一 ready future 脚手架）
        let cases: [(&'static [u8], &str); 3] = [
            // 历史上的裸 `{jobId}`（A1 修复前的插件形态）。
            (br#"{"jobId":"j-bare"}"#, "j-bare"),
            // 裸标量/数组同样不崩（data 原样收纳）。
            (br#""naked""#, "naked"),
            (br#"[1,2]"#, "arr"),
        ];
        for (body, tag) in cases {
            let vt = vtable_returning(body);
            let b = FfiMailBackend::new(vt, MailConfig::empty(), Arc::new(Bus::new()));
            let env = b
                .submit("default", "{}".to_string(), vec![])
                .await
                .unwrap_or_else(|e| panic!("{e}"));
            assert_eq!(env["code"], 0, "{tag}: 裸载荷须包成成功信封: {env}");
            assert_eq!(env["msg"], "ok", "{tag}: {env}");
            match tag {
                "j-bare" => assert_eq!(env["data"]["jobId"], "j-bare", "{env}"),
                "naked" => assert_eq!(env["data"], "naked", "{env}"),
                _ => assert_eq!(env["data"], json!([1, 2]), "{env}"),
            }
        }
        // 已是信封的形态**原样透传**（不重复包裹）。
        let vt = vtable_returning(br#"{"code":5,"msg":"boom","data":{}}"#);
        let b = FfiMailBackend::new(vt, MailConfig::empty(), Arc::new(Bus::new()));
        let env = b
            .submit("default", "{}".to_string(), vec![])
            .await
            .expect("信封");
        assert_eq!(
            env,
            json!({"code": 5, "msg": "boom", "data": {}}),
            "已有 code 的信封不得被二次包裹"
        );
    }

    /// A2：插件 future 长期 pending 时，宿主**退避睡眠**而非 `yield_now` 空转。
    /// 20 次 pending × `FFI_POLL_BACKOFF`(2ms) ⇒ 墙钟 ≥ 30ms；空转实现只需 µs 级
    /// （原实现用 `await_ffi`：SMTP 往返可达 30s ⇒ 每秒烧满该 isolate 的 current_thread）。
    #[tokio::test(flavor = "current_thread")]
    async fn ffi_mail_backend_backs_off_instead_of_spinning_while_pending() {
        use std::sync::atomic::{AtomicU32, Ordering};

        /// 前 `left` 次 poll 返回 pending，之后 ready。
        struct Countdown {
            left: AtomicU32,
            result: Option<Result<Vec<u8>, String>>,
        }
        extern "C" fn poll(state: *mut std::ffi::c_void) -> i32 {
            let s = unsafe { &mut *(state as *mut Countdown) };
            if s.left.load(Ordering::SeqCst) == 0 {
                1
            } else {
                s.left.fetch_sub(1, Ordering::SeqCst);
                0
            }
        }
        extern "C" fn take(
            state: *mut std::ffi::c_void,
        ) -> oj_plugin_ffi::RResult<oj_plugin_ffi::RBytes, oj_plugin_ffi::RString> {
            let s = unsafe { &mut *(state as *mut Countdown) };
            match s.result.take() {
                Some(Ok(b)) => oj_plugin_ffi::RResult::Ok(oj_plugin_ffi::RBytes::from(&b[..])),
                _ => oj_plugin_ffi::RResult::Err(oj_plugin_ffi::RString::from("not ready")),
            }
        }
        extern "C" fn free(state: *mut std::ffi::c_void) {
            if !state.is_null() {
                drop(unsafe { Box::from_raw(state as *mut Countdown) });
            }
        }
        extern "C" fn submit(
            _key: oj_plugin_ffi::RString,
            _req: oj_plugin_ffi::RString,
            _atts: oj_plugin_ffi::RVec<oj_plugin_ffi::MailAttachment>,
        ) -> oj_plugin_ffi::FfiFuture {
            const PENDING: u32 = 20;
            oj_plugin_ffi::FfiFuture {
                state: Box::into_raw(Box::new(Countdown {
                    left: AtomicU32::new(PENDING),
                    result: Some(Ok(
                        br#"{"code":0,"msg":"ok","data":{"jobId":"j-slow"}}"#.to_vec()
                    )),
                }))
                .cast(),
                poll,
                take,
                free,
            }
        }
        let vt: &'static MailVtable = Box::leak(Box::new(MailVtable { submit }));
        let b = FfiMailBackend::new(vt, MailConfig::empty(), Arc::new(Bus::new()));

        let t0 = Instant::now();
        let env = b
            .submit("default", "{}".to_string(), vec![])
            .await
            .expect("信封");
        let elapsed = t0.elapsed();
        assert_eq!(env["data"]["jobId"], "j-slow", "{env}");
        assert!(
            elapsed >= Duration::from_millis(30),
            "pending 期间必须退避睡眠（20 × {:?}），实测 {elapsed:?} —— 空转（yield_now）会瞬回",
            crate::bridge::ffi::FFI_POLL_BACKOFF
        );
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

    // ---------- 6.4：入参校验（纯函数，宿主权威层） ----------

    #[test]
    fn strip_crlf_removes_header_injection() {
        assert_eq!(strip_crlf("hi\r\nBcc: evil@x.com"), "hiBcc: evil@x.com");
        assert_eq!(strip_crlf("a\nb\rc"), "abc");
        assert_eq!(strip_crlf("clean"), "clean");
    }

    #[test]
    fn address_validation_uses_lettre_parser() {
        assert!(validate_address("a@x.com").is_ok());
        assert!(validate_address("not-an-addr").is_err());
        assert!(validate_address("").is_err());
        assert!(validate_address("a@x.com>b@y.com").is_err());
        // CRLF 一律拒（地址无「合法换行」语义，不同于 subject 的剥离）。
        assert!(validate_address("a@x.com\r\nBcc: b@y.com").is_err());
        assert!(validate_address("a@x.com\n").is_err());
    }

    #[test]
    fn whitelist_is_exact_and_fail_closed() {
        let p = MailProfileCfg {
            allowed_from: vec!["noreply@x.com".into()],
            allowed_recipients: vec!["@x.com".into(), "@partner.com".into()],
        };
        assert!(check_whitelist("noreply@x.com", &["a@x.com".into()], &p).is_ok());
        assert!(check_whitelist("noreply@x.com", &["a@partner.com".into()], &p).is_ok());
        // 大小写不敏感（域名大小写无语义）。
        assert!(check_whitelist("NoReply@X.com", &["A@X.com".into()], &p).is_ok());
        let e = check_whitelist("evil@y.com", &["a@x.com".into()], &p).unwrap_err();
        assert!(
            e.contains("allowed_from") && e.contains("evil@y.com"),
            "{e}"
        );
        let e = check_whitelist(
            "noreply@x.com",
            &["a@x.com".into(), "b@evil.com".into()],
            &p,
        )
        .unwrap_err();
        assert!(
            e.contains("allowed_recipients") && e.contains("b@evil.com"),
            "{e}"
        );
        // 空表 = 拒绝（fail-closed，同 `tls: none` 的显式许可思路）。
        let empty = MailProfileCfg::default();
        let e = check_whitelist("a@x.com", &["b@x.com".into()], &empty).unwrap_err();
        assert!(e.contains("allowed_from"), "{e}");
        let no_rcpt = MailProfileCfg {
            allowed_from: vec!["@x.com".into()],
            ..Default::default()
        };
        let e = check_whitelist("a@x.com", &["b@x.com".into()], &no_rcpt).unwrap_err();
        assert!(e.contains("allowed_recipients"), "{e}");
    }

    /// B1：白名单是**唯一**越权控制点，故匹配必须是「域全等 / 地址全等」——
    /// 裸后缀（`ends_with`）三处绕过逐一钉死（先红）：
    /// ① `noreply@x.com` 放行同域仿冒 `evil-noreply@x.com`；
    /// ② 漏写 `@` 的条目（`x.com`）放行跨域 `a@evilx.com`；
    /// ③ 空条目 `""`（`ends_with("")` 恒真）= 白名单等于关闭。
    /// 另钉「不做子域通配」：`@x.com` **不**命中 `a@sub.x.com`（子域须显式 `@sub.x.com`）。
    #[test]
    fn whitelist_blocks_the_three_suffix_bypasses() {
        let p = MailProfileCfg {
            allowed_from: vec!["noreply@x.com".into()],
            allowed_recipients: vec!["@x.com".into(), "@partner.com".into()],
        };
        // 正向对照（防「一刀切全拒」也被判绿）。
        assert!(check_whitelist("noreply@x.com", &["a@x.com".into()], &p).is_ok());
        assert!(
            check_whitelist("NoReply@X.COM", &["a@PARTNER.com".into()], &p).is_ok(),
            "大小写不敏感（域名/本地部大小写无语义）"
        );

        // ① 同域仿冒发件人：条目是完整地址 ⇒ 必须全等，不得后缀命中。
        let e = check_whitelist("evil-noreply@x.com", &["a@x.com".into()], &p).unwrap_err();
        assert!(e.contains("allowed_from"), "{e}");

        // ② 漏写 `@` 的条目：既不命中 `a@evilx.com`，也不命中任何地址（fail-closed）。
        let bare = MailProfileCfg {
            allowed_from: vec!["noreply@x.com".into()],
            allowed_recipients: vec!["x.com".into()],
        };
        let e = check_whitelist("noreply@x.com", &["a@evilx.com".into()], &bare).unwrap_err();
        assert!(e.contains("allowed_recipients"), "{e}");
        assert!(
            check_whitelist("noreply@x.com", &["a@x.com".into()], &bare).is_err(),
            "裸域条目不得退化成后缀匹配（连本域也不放行）"
        );

        // ③ 空条目 = 白名单关闭：任何地址都不命中。
        let blank = MailProfileCfg {
            allowed_from: vec!["".into()],
            allowed_recipients: vec!["@x.com".into()],
        };
        let e = check_whitelist("anyone@anywhere.com", &["a@x.com".into()], &blank).unwrap_err();
        assert!(e.contains("allowed_from"), "{e}");

        // 子域**不**通配：`@x.com` 只覆盖本域；子域要显式列出。
        assert!(
            check_whitelist("noreply@x.com", &["a@sub.x.com".into()], &p).is_err(),
            "@x.com 不得命中子域 a@sub.x.com"
        );
        let sub = MailProfileCfg {
            allowed_from: vec!["noreply@x.com".into()],
            allowed_recipients: vec!["@sub.x.com".into()],
        };
        assert!(
            check_whitelist("noreply@x.com", &["a@sub.x.com".into()], &sub).is_ok(),
            "显式 @sub.x.com 必须命中"
        );
        assert!(
            check_whitelist("noreply@x.com", &["a@x.com".into()], &sub).is_err(),
            "@sub.x.com 是域全等，不覆盖父域"
        );
    }

    /// B1：条目格式在**装配期**校验（fail-fast）——非法条目让配置解析失败，而不是等到发信
    /// 时才退化成「不命中」（那样运维只看到 code:5，不知是自己写错了白名单）。
    /// 错误须点名：profile、字段、第几条、条目原文、原因、下一步。
    #[test]
    fn mail_config_rejects_malformed_whitelist_entries() {
        let cases: [(&str, &str); 5] = [
            ("", "空"),
            ("x.com", "'@'"),
            (" noreply@x.com", "空白"),
            ("@", "域"),
            ("not an address", "'@'"),
        ];
        for (bad, needle) in cases {
            let v = json!({"default": {"allowed_from": ["noreply@x.com", bad]}});
            let e = format!(
                "{}",
                MailConfig::from_value(&v).expect_err("非法白名单条目必须让装配期失败")
            );
            assert!(
                e.contains("default") && e.contains("allowed_from[1]"),
                "错误须点名 profile 与下标：{e}"
            );
            assert!(e.contains(needle), "错误须给出原因（{needle}）：{e}");
            assert!(e.contains("下一步"), "错误须给下一步：{e}");
        }
        // 合法两形态：完整地址 与 `@domain`。
        let ok = json!({"default": {
            "allowed_from": ["noreply@x.com"],
            "allowed_recipients": ["@x.com", "@partner.com"]
        }});
        let cfg = MailConfig::from_value(&ok).expect("合法白名单条目必须通过");
        assert_eq!(
            cfg.profile("default").unwrap().allowed_recipients,
            vec!["@x.com", "@partner.com"]
        );
    }

    // ---------- 6.4：附件引用解析 ----------

    #[test]
    fn attachment_refs_require_exactly_one_source() {
        let refs = parse_attachment_refs(&json!([
            {"filename": "a.pdf", "blobKey": "r2d2"},
            {"filename": "b.pdf", "path": "reports/b.pdf", "mime": "application/pdf", "blob": "img"},
        ]))
        .unwrap();
        assert_eq!(refs.len(), 2);
        assert_eq!(
            refs[0].src,
            AttSrc::Blob {
                name: "default".into(),
                key: "r2d2".into()
            }
        );
        assert_eq!(refs[0].mime, None);
        assert_eq!(refs[1].mime.as_deref(), Some("application/pdf"));
        assert_eq!(refs[1].src, AttSrc::Path("reports/b.pdf".into()));
        // 缺省（无 attachments / null）= 空表。
        assert!(parse_attachment_refs(&Value::Null).unwrap().is_empty());
        // 两个来源都给 / 都不给 / 缺 filename / 非对象 / 空 key → 全部拒绝。
        for bad in [
            json!([{"filename": "a", "blobKey": "k", "path": "p"}]),
            json!([{"filename": "a"}]),
            json!([{"blobKey": "k"}]),
            json!([{"filename": "", "blobKey": "k"}]),
            json!([{"filename": "a", "blobKey": ""}]),
            json!(["nope"]),
            json!({"filename": "a"}),
        ] {
            assert!(parse_attachment_refs(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn mime_resolution_prefers_explicit_then_ext_then_magic() {
        assert_eq!(
            resolve_mime(Some("application/x-custom"), "a.bin", b"%PDF-1.7"),
            "application/x-custom"
        );
        assert_eq!(resolve_mime(None, "r.pdf", b"whatever"), "application/pdf");
        assert_eq!(resolve_mime(None, "r.PDF", b""), "application/pdf");
        assert_eq!(resolve_mime(None, "n.csv", b""), "text/csv");
        // 扩展名未知 → 字节嗅探。
        assert_eq!(resolve_mime(None, "x.dat", b"%PDF-1.7"), "application/pdf");
        assert_eq!(
            resolve_mime(None, "x.dat", b"\x89PNG\r\n\x1a\n"),
            "image/png"
        );
        // 都不认识 → 兜底（插件侧同款默认）。
        assert_eq!(
            resolve_mime(None, "x.dat", b"hello"),
            "application/octet-stream"
        );
    }

    // ---------- 6.4：附件字节解析（blob / path） ----------

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let d = std::env::temp_dir().join(format!(
            "oj-mail-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test(flavor = "current_thread")]
    async fn resolve_attachments_reads_blob_and_path_in_declared_order() {
        use crate::bridge::blob::{self, BlobBackend, LocalBlob};
        let dir = tmpdir("att");
        let blob_root = dir.join("blobs");
        std::fs::create_dir_all(&blob_root).unwrap();
        let lb = LocalBlob::new(&blob_root, "/v1/api").unwrap();
        lb.put("r2d2", b"BLOBBYTES", Some("application/pdf"))
            .await
            .unwrap();
        let reg = blob::registry_with_default(Arc::new(lb));
        let proj = dir.join("proj");
        std::fs::create_dir_all(proj.join("reports")).unwrap();
        std::fs::write(proj.join("reports/x.pdf"), b"%PDF-1.7 file").unwrap();

        let refs = parse_attachment_refs(&json!([
            {"filename": "b.pdf", "path": "reports/x.pdf"},
            {"filename": "a.pdf", "blobKey": "r2d2", "mime": "application/custom"},
        ]))
        .unwrap();
        let atts = resolve_attachments(&reg, Some(&proj), &refs, AttachmentLimits::default())
            .await
            .unwrap();
        assert_eq!(atts.len(), 2);
        // 下标严格对齐（插件按下标取字节）。
        assert_eq!(atts[0].filename, "b.pdf");
        assert_eq!(atts[0].bytes, b"%PDF-1.7 file");
        assert_eq!(atts[0].mime, "application/pdf"); // 扩展名嗅探
        assert_eq!(atts[1].filename, "a.pdf");
        assert_eq!(atts[1].bytes, b"BLOBBYTES");
        assert_eq!(atts[1].mime, "application/custom"); // 显式优先

        // `../` 越界 → 拒绝（文案给下一步）。
        let esc =
            parse_attachment_refs(&json!([{"filename": "e", "path": "../outside.txt"}])).unwrap();
        let e = resolve_attachments(&reg, Some(&proj), &esc, AttachmentLimits::default())
            .await
            .unwrap_err();
        assert!(e.contains("附件路径") && e.contains("下一步"), "{e}");
        // 无 project root（loader 未配置）→ path 附件拒绝。
        let e = resolve_attachments(&reg, None, &refs, AttachmentLimits::default())
            .await
            .unwrap_err();
        assert!(e.contains("project root"), "{e}");
        // blob 后端未配置 / 键缺失 → 明确错误。
        let missing =
            parse_attachment_refs(&json!([{"filename": "m", "blobKey": "nope"}])).unwrap();
        let e = resolve_attachments(&reg, None, &missing, AttachmentLimits::default())
            .await
            .unwrap_err();
        assert!(e.contains("nope"), "{e}");
        let empty = blob::BlobRegistry::new();
        let e = resolve_attachments(&empty, None, &missing, AttachmentLimits::default())
            .await
            .unwrap_err();
        assert!(e.contains("not configured"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// B2：附件上限（单附件 + 单封合计）必须**先于**入队放大生效：超限即 `code:5`，
    /// 文案给下一步；path 路先看长度不读盘（`tokio::fs::metadata`），blob 路拿到字节即判。
    /// 正向对照：正常大小的附件仍通过（防「一刀切拒绝」也被判绿）。
    #[tokio::test(flavor = "current_thread")]
    async fn resolve_attachments_enforces_per_file_and_total_limits() {
        use crate::bridge::blob::{self, BlobBackend, LocalBlob};
        let dir = tmpdir("limits");
        let proj = dir.join("proj");
        std::fs::create_dir_all(proj.join("reports")).unwrap();
        // 上限设为 8 字节，便于用小文件覆盖所有分支。
        let limits = AttachmentLimits {
            max_attachment_bytes: 8,
            max_total_bytes: 12,
        };
        std::fs::write(proj.join("reports/ok.txt"), b"12345678").unwrap(); // == 上限：通过
        std::fs::write(proj.join("reports/big.txt"), b"123456789").unwrap(); // 超 1 字节
        std::fs::write(proj.join("reports/a.txt"), b"12345678").unwrap();
        std::fs::write(proj.join("reports/b.txt"), b"12345678").unwrap();
        let blob_root = dir.join("blobs");
        std::fs::create_dir_all(&blob_root).unwrap();
        let lb = LocalBlob::new(&blob_root, "/v1/api").unwrap();
        BlobBackend::put(&lb, "big-blob", b"123456789", None)
            .await
            .unwrap();
        let reg = blob::registry_with_default(Arc::new(lb));

        // 单附件上限：path 路。
        let one = parse_attachment_refs(&json!([{"filename": "b.txt", "path": "reports/big.txt"}]))
            .unwrap();
        let e = resolve_attachments(&reg, Some(&proj), &one, limits)
            .await
            .unwrap_err();
        assert!(
            e.contains("attachments[0]") && e.contains("9") && e.contains("8"),
            "错误须点名附件与两侧字节数：{e}"
        );
        assert!(
            e.contains("max_attachment_bytes") && e.contains("下一步"),
            "错误须给配置键与下一步：{e}"
        );
        // 单附件上限：blob 路（长度只有拿到字节才知道）。
        let one =
            parse_attachment_refs(&json!([{"filename": "b.bin", "blobKey": "big-blob"}])).unwrap();
        let e = resolve_attachments(&reg, Some(&proj), &one, limits)
            .await
            .unwrap_err();
        assert!(e.contains("max_attachment_bytes"), "{e}");

        // 单封合计上限：两个都在单件上限内（8+8），但合计 16 > 12 → 第二个即拒。
        let two = parse_attachment_refs(&json!([
            {"filename": "a.txt", "path": "reports/a.txt"},
            {"filename": "b.txt", "path": "reports/b.txt"},
        ]))
        .unwrap();
        let e = resolve_attachments(&reg, Some(&proj), &two, limits)
            .await
            .unwrap_err();
        assert!(
            e.contains("attachments[1]") && e.contains("max_total_attachment_bytes"),
            "错误须点名越界的那一个附件与合计上限键：{e}"
        );
        assert!(e.contains("下一步"), "{e}");

        // 正向对照：边界值（== 上限）与合计不超限时必须通过。
        let ok = parse_attachment_refs(&json!([
            {"filename": "a.txt", "path": "reports/a.txt"},
            {"filename": "ok.bin", "blobKey": "ok"},
        ]))
        .unwrap();
        let lb = LocalBlob::new(&blob_root, "/v1/api").unwrap();
        BlobBackend::put(&lb, "ok", b"1234", None).await.unwrap();
        let reg = blob::registry_with_default(Arc::new(lb));
        let atts = resolve_attachments(&reg, Some(&proj), &ok, limits)
            .await
            .unwrap();
        assert_eq!(atts.len(), 2);
        assert_eq!(atts[0].bytes, b"12345678");
        assert_eq!(atts[1].bytes, b"1234");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// B2：附件上限是**装配期**配置（`smtp.max_attachment_bytes` /
    /// `max_total_attachment_bytes`，与 `workers` 同级）：缺省取默认，非正整数即报错
    /// （0 等于禁止一切附件；写成字符串/负数都是写法错误，不静默取默认）。
    #[test]
    fn mail_config_parses_and_validates_attachment_limits() {
        let base = json!({"default": {"allowed_from": ["@x.com"]}});
        let cfg = MailConfig::from_value(&base).unwrap();
        assert_eq!(
            cfg.attachment_limits(),
            AttachmentLimits {
                max_attachment_bytes: DEFAULT_MAX_ATTACHMENT_BYTES,
                max_total_bytes: DEFAULT_MAX_TOTAL_ATTACHMENT_BYTES,
            }
        );
        assert_eq!(DEFAULT_MAX_ATTACHMENT_BYTES, 10 * 1024 * 1024);

        let mut v = base.clone();
        v["max_attachment_bytes"] = json!(1024);
        v["max_total_attachment_bytes"] = json!(2048);
        let cfg = MailConfig::from_value(&v).unwrap();
        assert_eq!(
            cfg.attachment_limits(),
            AttachmentLimits {
                max_attachment_bytes: 1024,
                max_total_bytes: 2048,
            }
        );
        // 上限键**不是** profile（不会因「缺 host」而被当 profile 解析失败）。
        assert_eq!(cfg.profile_keys(), vec!["default"]);

        for bad in [json!(0), json!(-1), json!("1024"), json!(1.5)] {
            let mut v = base.clone();
            v["max_attachment_bytes"] = bad.clone();
            let e = format!(
                "{}",
                MailConfig::from_value(&v).expect_err("非法附件上限必须让装配期失败")
            );
            assert!(
                e.contains("max_attachment_bytes") && e.contains("下一步"),
                "{bad} → {e}"
            );
        }
    }

    // ---------- 6.4：编排（校验 → 附件 → submit） ----------

    fn whitelisted_config() -> MailConfig {
        MailConfig::new(HashMap::from([(
            "default".to_string(),
            MailProfileCfg {
                allowed_from: vec!["noreply@x.com".into()],
                allowed_recipients: vec!["@x.com".into()],
            },
        )]))
    }

    fn empty_blobs() -> Arc<crate::bridge::blob::BlobRegistry> {
        Arc::new(crate::bridge::blob::BlobRegistry::new())
    }

    /// 正常路：校验通过 → CRLF 剥离 → 地址规范化 → 开关按 op 覆写 → 原样转发给插件。
    #[tokio::test(flavor = "current_thread")]
    async fn handle_send_normalizes_and_dispatches() {
        let fake = FakeMail::new(whitelisted_config(), Arc::new(Bus::new()));
        let req = json!({
            "from": "noreply@x.com",
            "to": ["a@x.com"],
            "subject": "hi\r\nBcc: evil@y.com",
            "headers": {"X-Custom": "v\r\nX-Injected: 1"},
            "text": "body\r\nline2",
            "sync": true,          // JS 侧串用：宿主按 op 覆写
            "enqueue_only": true,
        })
        .to_string();
        let env = handle_send(
            fake.clone(),
            empty_blobs(),
            None,
            "default",
            &req,
            MailMode::Send,
        )
        .await;
        assert_eq!(env["code"], 0);
        let sent = fake.sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, "default");
        let fwd: Value = serde_json::from_str(&sent[0].1).unwrap();
        assert_eq!(fwd["subject"], "hiBcc: evil@y.com"); // 剥离而非拒绝（design §10）
        assert_eq!(fwd["headers"]["X-Custom"], "vX-Injected: 1");
        assert_eq!(fwd["text"], "body\r\nline2"); // 正文不剥（正文换行有语义）
        assert_eq!(fwd["sync"], false, "宿主按 op 覆写开关");
        assert_eq!(fwd["enqueue_only"], false);
        assert_eq!(fwd["to"][0], "a@x.com");
    }

    /// B2（编排面）：超限附件经 `mail.*` 拿到 `{code:5}`（resolve，不 throw），且**不触达
    /// 插件**（上限在宿主读盘处生效，故队列里不会出现这封信）；正常附件仍原样过线。
    #[tokio::test(flavor = "current_thread")]
    async fn handle_send_rejects_oversized_attachment_with_code5() {
        let dir = tmpdir("limit-send");
        let proj = dir.join("proj");
        std::fs::create_dir_all(proj.join("reports")).unwrap();
        std::fs::write(proj.join("reports/ok.txt"), b"12345678").unwrap();
        std::fs::write(proj.join("reports/big.txt"), b"123456789").unwrap();
        let cfg = MailConfig::with_limits(
            HashMap::from([(
                "default".to_string(),
                MailProfileCfg {
                    allowed_from: vec!["noreply@x.com".into()],
                    allowed_recipients: vec!["@x.com".into()],
                },
            )]),
            AttachmentLimits {
                max_attachment_bytes: 8,
                max_total_bytes: 16,
            },
        );
        let fake = FakeMail::new(cfg, Arc::new(Bus::new()));

        let env = handle_send(
            fake.clone(),
            empty_blobs(),
            Some(&proj),
            "default",
            &json!({"from": "noreply@x.com", "to": ["a@x.com"], "text": "x",
            "attachments": [
                {"filename": "a.txt", "path": "reports/ok.txt"},
                {"filename": "b.txt", "path": "reports/big.txt"}
            ]})
            .to_string(),
            MailMode::Send,
        )
        .await;
        assert_eq!(env["code"], 5, "{env}");
        let msg = env["msg"].as_str().unwrap();
        assert!(
            msg.contains("max_attachment_bytes") && msg.contains("下一步"),
            "{env}"
        );
        assert!(
            fake.sent.lock().unwrap().is_empty(),
            "超限信不得触达插件（上限在宿主读盘处生效）"
        );

        // 正向对照：同配置下正常附件照常过线（字节与下标不变）。
        let env = handle_send(
            fake.clone(),
            empty_blobs(),
            Some(&proj),
            "default",
            &json!({"from": "noreply@x.com", "to": ["a@x.com"], "text": "x",
                    "attachments": [{"filename": "a.txt", "path": "reports/ok.txt"}]})
            .to_string(),
            MailMode::Send,
        )
        .await;
        assert_eq!(env["code"], 0, "{env}");
        let sent = fake.sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].2[0].bytes, b"12345678");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 各 mode 的引擎开关：sendSync → sync、enqueue → enqueue_only（两者互斥不叠加）。
    #[tokio::test(flavor = "current_thread")]
    async fn handle_send_sets_engine_flags_per_mode() {
        let fake = FakeMail::new(whitelisted_config(), Arc::new(Bus::new()));
        let req = json!({"from": "noreply@x.com", "to": ["a@x.com"], "text": "x"}).to_string();
        let expect = [
            (MailMode::Send, false, false),
            (MailMode::Sync, true, false),
            (MailMode::Enqueue, false, true),
        ];
        for (mode, _, _) in expect {
            handle_send(fake.clone(), empty_blobs(), None, "default", &req, mode).await;
        }
        let sent = fake.sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 3);
        let flags: Vec<(bool, bool)> = sent
            .iter()
            .map(|(_, r, _)| {
                let v: Value = serde_json::from_str(r).unwrap();
                (
                    v["sync"].as_bool().unwrap(),
                    v["enqueue_only"].as_bool().unwrap(),
                )
            })
            .collect();
        let want: Vec<(bool, bool)> = expect.iter().map(|(_, s, e)| (*s, *e)).collect();
        assert_eq!(flags, want);
    }

    /// 校验失败一律 `{code:5}` 信封（不 throw）：地址非法 / 白名单未命中 / 未知 profile /
    /// 缺收件人 / 头名越权 / 附件形态错 / 路径越界。校验优先于后端调用（后端不被触达）。
    #[tokio::test(flavor = "current_thread")]
    async fn handle_send_rejects_with_code5_and_never_touches_backend() {
        let fake = FakeMail::new(whitelisted_config(), Arc::new(Bus::new()));
        let proj = tmpdir("escape");
        let cases: Vec<(Value, &str)> = vec![
            (
                json!({"from": "not-an-addr", "to": ["a@x.com"], "text": "x"}),
                "from",
            ),
            (
                json!({"from": "evil@y.com", "to": ["a@x.com"], "text": "x"}),
                "allowed_from",
            ),
            (
                json!({"from": "noreply@x.com", "to": ["b@evil.com"], "text": "x"}),
                "allowed_recipients",
            ),
            (
                json!({"from": "noreply@x.com", "to": [], "text": "x"}),
                "to",
            ),
            (json!({"from": "noreply@x.com", "to": ["a@x.com"]}), "正文"),
            (
                json!({"from": "noreply@x.com", "to": ["a@x.com"], "text": "x",
                       "headers": {"Subject": "hijack"}}),
                "headers",
            ),
            (
                json!({"from": "noreply@x.com", "to": ["a@x.com"], "text": "x",
                       "attachments": [{"filename": "a", "blobKey": "k", "path": "p"}]}),
                "blobKey",
            ),
            (
                json!({"from": "noreply@x.com", "to": ["a@x.com"], "text": "x",
                       "attachments": [{"filename": "a", "path": "../escape.pdf"}]}),
                "附件路径",
            ),
        ];
        for (req, needle) in cases {
            let env = handle_send(
                fake.clone(),
                empty_blobs(),
                Some(&proj),
                "default",
                &req.to_string(),
                MailMode::Send,
            )
            .await;
            assert_eq!(env["code"], 5, "{req} → {env}");
            assert!(
                env["msg"].as_str().unwrap().contains(needle),
                "{req} → {env}"
            );
        }
        // 未知 profile（不回落 default：错配的 profile 名必须显式失败）。
        let env = handle_send(
            fake.clone(),
            empty_blobs(),
            None,
            "nope",
            &json!({"from": "noreply@x.com", "to": ["a@x.com"], "text": "x"}).to_string(),
            MailMode::Send,
        )
        .await;
        assert_eq!(env["code"], 5, "{env}");
        assert!(env["msg"].as_str().unwrap().contains("nope"), "{env}");
        // 非对象请求 / 非法 JSON。
        for bad in [json!([1, 2]).to_string(), "{not json".to_string()] {
            let env = handle_send(
                fake.clone(),
                empty_blobs(),
                None,
                "default",
                &bad,
                MailMode::Send,
            )
            .await;
            assert_eq!(env["code"], 5, "{bad} → {env}");
        }
        // 一次后端都没触达（校验在前）。
        assert!(fake.sent.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&proj);
    }

    /// 收件人地址非法一律 `{code:5}`：**to/cc/bcc 与 from 同一解析器**（`lettre::Address`，
    /// 见 `address_list`），且错误**逐个字段点名**（`to[0]`/`cc[0]`/`bcc[0]`）便于定位。
    /// 注意口径差异（design §10）：地址含 CRLF 是**拒绝**，不像 subject/headers 那样剥离
    /// ——「含换行的合法地址」不存在，剥离只会把注入内容粘进地址。
    #[tokio::test(flavor = "current_thread")]
    async fn handle_send_rejects_illegal_addresses_in_every_recipient_field() {
        let fake = FakeMail::new(whitelisted_config(), Arc::new(Bus::new()));
        let base = json!({"from": "noreply@x.com", "to": ["a@x.com"], "text": "x"});
        let cases: Vec<(Value, &str, &str)> = vec![
            (json!({"to": ["not-an-addr"]}), "to[0]", "地址非法"),
            (json!({"cc": ["not-an-addr"]}), "cc[0]", "地址非法"),
            (json!({"bcc": ["not-an-addr"]}), "bcc[0]", "地址非法"),
            // CRLF 注入地址 → 拒绝（而非剥离成 a@x.comBcc: evil@y.com）。
            (
                json!({"to": ["a@x.com\r\nBcc: evil@y.com"]}),
                "to[0]",
                "地址非法",
            ),
            (json!({"cc": ["a@x.com\n"]}), "cc[0]", "地址非法"),
            (json!({"bcc": ["a@x.com\r\n"]}), "bcc[0]", "地址非法"),
            // 元素/字段类型错也走同一 code:5（不静默丢弃该收件人）。
            (json!({"to": [1]}), "to[0]", "必须是字符串"),
            (json!({"cc": "a@x.com"}), "cc", "必须是字符串数组"),
            (json!({"bcc": [null]}), "bcc[0]", "必须是字符串"),
        ];
        for (patch, field, reason) in cases {
            let mut req = base.clone();
            for (k, v) in patch.as_object().unwrap() {
                req[k] = v.clone();
            }
            let env = handle_send(
                fake.clone(),
                empty_blobs(),
                None,
                "default",
                &req.to_string(),
                MailMode::Send,
            )
            .await;
            assert_eq!(env["code"], 5, "{req} → {env}");
            let msg = env["msg"].as_str().unwrap();
            assert!(msg.contains(field), "错误须点名 {field}：{env}");
            assert!(msg.contains(reason), "错误须给出原因 {reason}：{env}");
        }
        assert!(
            fake.sent.lock().unwrap().is_empty(),
            "地址校验失败不得触达后端"
        );
    }

    /// `headers` **不得覆盖结构化字段**（design §11：`From`/`To`/`Cc`/`Bcc`/`Subject` 决定
    /// 信封与主题）。这条是白名单绕过面：若放行 `To`，收件人白名单只查结构化 `to`，而信封
    /// 由报头派生即可把信发给任意人。宿主侧对五个名字（大小写不敏感）一律 `{code:5}`。
    #[tokio::test(flavor = "current_thread")]
    async fn handle_send_rejects_headers_covering_structured_fields() {
        let fake = FakeMail::new(whitelisted_config(), Arc::new(Bus::new()));
        for name in [
            "From", "from", "FROM", "To", "to", "Cc", "cc", "Bcc", "bcc", "Subject", "subject",
        ] {
            let mut headers = serde_json::Map::new();
            headers.insert(name.to_string(), Value::String("evil@y.com".into()));
            let mut req = json!({"from": "noreply@x.com", "to": ["a@x.com"], "text": "x"});
            req["headers"] = Value::Object(headers);

            let env = handle_send(
                fake.clone(),
                empty_blobs(),
                None,
                "default",
                &req.to_string(),
                MailMode::Send,
            )
            .await;
            assert_eq!(env["code"], 5, "headers 覆盖 {name} 必须拒：{env}");
            let msg = env["msg"].as_str().unwrap();
            assert!(
                msg.contains("headers") && msg.contains(name),
                "错误须点明头名 {name}：{env}"
            );
        }
        assert!(fake.sent.lock().unwrap().is_empty());
    }

    /// 收件人白名单覆盖 **to ∪ cc ∪ bcc**（design §10）：任一字段越界即 `{code:5}`，且错误
    /// 点名越界地址——防「只查 to、漏了 cc/bcc」的静默放行（Bcc 是典型绕过路径）。
    #[tokio::test(flavor = "current_thread")]
    async fn handle_send_gates_cc_and_bcc_against_allowed_recipients() {
        let cfg = MailConfig::new(HashMap::from([(
            "default".to_string(),
            MailProfileCfg {
                allowed_from: vec!["noreply@x.com".into()],
                allowed_recipients: vec!["@x.com".into(), "@partner.com".into()],
            },
        )]));
        let fake = FakeMail::new(cfg, Arc::new(Bus::new()));

        for (patch, bad) in [
            (json!({"cc": ["c@evil.com"]}), "c@evil.com"),
            (json!({"bcc": ["b@evil.com"]}), "b@evil.com"),
        ] {
            let mut req = json!({"from": "noreply@x.com", "to": ["a@x.com"], "text": "x"});
            for (k, v) in patch.as_object().unwrap() {
                req[k] = v.clone();
            }
            let env = handle_send(
                fake.clone(),
                empty_blobs(),
                None,
                "default",
                &req.to_string(),
                MailMode::Send,
            )
            .await;
            assert_eq!(env["code"], 5, "{req} → {env}");
            let msg = env["msg"].as_str().unwrap();
            assert!(
                msg.contains("allowed_recipients") && msg.contains(bad),
                "错误须点名越界收件人 {bad}：{env}"
            );
        }
        assert!(fake.sent.lock().unwrap().is_empty(), "越权信不得触达后端");

        // 正向对照：cc/bcc 都在白名单内 → 放行，且三字段原样过线
        // （防「一刀切拒绝 cc/bcc」的变异也被判绿）。
        let req = json!({"from": "noreply@x.com", "to": ["a@x.com"],
                         "cc": ["c@partner.com"], "bcc": ["b@x.com"], "text": "x"});
        let env = handle_send(
            fake.clone(),
            empty_blobs(),
            None,
            "default",
            &req.to_string(),
            MailMode::Send,
        )
        .await;
        assert_eq!(env["code"], 0, "{env}");
        let sent = fake.sent.lock().unwrap().clone();
        let fwd: Value = serde_json::from_str(&sent[0].1).unwrap();
        assert_eq!(fwd["cc"][0], "c@partner.com");
        assert_eq!(fwd["bcc"][0], "b@x.com");
    }

    /// sendRaw：`raw` 必填且与 `attachments` 互斥；正文/主题不参与校验；from/to 仍校验。
    #[tokio::test(flavor = "current_thread")]
    async fn handle_send_raw_requires_raw_and_forbids_attachments() {
        let fake = FakeMail::new(whitelisted_config(), Arc::new(Bus::new()));
        let req = json!({
            "from": "noreply@x.com",
            "to": ["a@x.com"],
            "raw": "Subject: s\r\nFrom: spoof@evil.com\r\n\r\nbody\r\n",
        })
        .to_string();
        let env = handle_send(
            fake.clone(),
            empty_blobs(),
            None,
            "default",
            &req,
            MailMode::Raw,
        )
        .await;
        assert_eq!(env["code"], 0);
        let sent = fake.sent.lock().unwrap().clone();
        let fwd: Value = serde_json::from_str(&sent[0].1).unwrap();
        // raw 原文（含换行）原样转发：冲突头剥离是插件的职责。
        assert!(fwd["raw"].as_str().unwrap().contains("spoof@evil.com"));
        assert_eq!(fwd["sync"], false);
        assert_eq!(fwd["enqueue_only"], false);

        // 缺 raw。
        let env = handle_send(
            fake.clone(),
            empty_blobs(),
            None,
            "default",
            &json!({"from": "noreply@x.com", "to": ["a@x.com"]}).to_string(),
            MailMode::Raw,
        )
        .await;
        assert_eq!(env["code"], 5);
        assert!(env["msg"].as_str().unwrap().contains("raw"), "{env}");
        // raw 与附件互斥。
        let env = handle_send(
            fake.clone(),
            empty_blobs(),
            None,
            "default",
            &json!({"from": "noreply@x.com", "to": ["a@x.com"], "raw": "X: 1\r\n\r\nb",
                    "attachments": [{"filename": "a", "blobKey": "k"}]})
            .to_string(),
            MailMode::Raw,
        )
        .await;
        assert_eq!(env["code"], 5);
        assert!(
            env["msg"].as_str().unwrap().contains("attachments"),
            "{env}"
        );
        // 结构化 subject 非空 → 覆盖原文 Subject（design §7：结构化为准）。
        let env = handle_send(
            fake.clone(),
            empty_blobs(),
            None,
            "default",
            &json!({"from": "noreply@x.com", "to": ["a@x.com"], "subject": "override",
                    "raw": "Subject: orig\r\n\r\nb"})
            .to_string(),
            MailMode::Raw,
        )
        .await;
        assert_eq!(env["code"], 0);
        let sent = fake.sent.lock().unwrap().clone();
        let fwd: Value = serde_json::from_str(&sent[1].1).unwrap();
        assert_eq!(fwd["subject"], "override");
        assert_eq!(fake.sent.lock().unwrap().len(), 2, "失败路不触达后端");
    }

    // ---------- 6.5：JS 全局（Mail / mail）端到端 ----------

    /// 带 mail（+ 可选 blob/loader）的 Bridge：JS 侧经全局 `mail.*` 打全套 op。
    fn mail_bridge(
        fake: Arc<dyn MailBackend>,
        blobs: Option<Arc<BlobRegistry>>,
        root: Option<PathBuf>,
    ) -> Bridge {
        use crate::bridge::LoaderShared;
        Bridge::with_dbs_and_loader(
            HashMap::new(),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            root.map(|project_root| {
                Arc::new(LoaderShared {
                    project_root,
                    ts: true,
                })
            }),
            Extras {
                mail: Some(fake),
                blobs,
                ..Default::default()
            },
        )
    }

    async fn run_js(b: &Bridge, src: &str) -> Value {
        let cap = b
            .run_with(src, crate::bridge::RequestInfo::default())
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        serde_json::from_slice(&cap.body)
            .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&cap.body)))
    }

    /// `mail.send`（全局）：JS 对象 → JSON → 宿主校验/附件解析 → 插件；信封原样回 JS。
    /// 同批覆盖：`sendSync`/`enqueue` 的引擎开关、`Mail(key)` 实例 profile、CRLF 剥离。
    #[tokio::test(flavor = "current_thread")]
    async fn js_mail_send_and_instance_methods_hit_ops() {
        use crate::bridge::blob::{BlobBackend, LocalBlob};
        let _g = lock();
        let dir = tmpdir("js");
        let proj = dir.join("proj");
        std::fs::create_dir_all(proj.join("reports")).unwrap();
        std::fs::write(proj.join("reports/x.pdf"), b"%PDF-1.7 body").unwrap();
        let blob_root = dir.join("blobs");
        std::fs::create_dir_all(&blob_root).unwrap();
        let lb = LocalBlob::new(&blob_root, "/v1/api").unwrap();
        BlobBackend::put(&lb, "r2d2", b"BLOBBYTES", None)
            .await
            .unwrap();

        let cfg = MailConfig::new(HashMap::from([
            ("default".to_string(), {
                let mut p = whitelisted_config().profile("default").unwrap().clone();
                p.allowed_recipients.push("@partner.com".into());
                p
            }),
            (
                "alerts".to_string(),
                MailProfileCfg {
                    allowed_from: vec!["alert@x.com".into()],
                    allowed_recipients: vec!["@x.com".into()],
                },
            ),
        ]));
        let fake = FakeMail::new(cfg, Arc::new(Bus::new()));
        let b = mail_bridge(
            fake.clone(),
            Some(crate::bridge::blob::registry_with_default(Arc::new(lb))),
            Some(proj),
        );
        let v = run_js(
            &b,
            r#"(async () => {
                const r = await mail.send({
                  from: "noreply@x.com", to: ["a@x.com"],
                  subject: "hi\r\nBcc: evil@y.com",
                  headers: { "X-Custom": "v\r\nX-Injected: 1" },
                  text: "body",
                  attachments: [
                    { filename: "b.pdf", path: "reports/x.pdf" },
                    { filename: "a.bin", blobKey: "r2d2", mime: "application/x-custom" },
                  ],
                });
                const s = await mail.sendSync({ from: "noreply@x.com", to: ["c@partner.com"], text: "x" });
                const j = await mail.enqueue({ from: "noreply@x.com", to: ["a@x.com"], text: "x" });
                const a = await new Mail("alerts").send({ from: "alert@x.com", to: ["a@x.com"], text: "x" });
                json.ok({ r, s, j, a });
              })().catch((e) => json.ok({ err: String(e) }));
              "#,
        )
        .await;
        assert!(v["data"].get("err").is_none(), "{v}");
        for k in ["r", "s", "j", "a"] {
            assert_eq!(v["data"][k]["code"], 0, "{k}: {v}");
        }
        assert_eq!(v["data"]["j"]["data"]["jobId"], "j-stub");
        let sent = fake.sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 4);
        // 附件：下标序 + 字节 + MIME（显式优先 / 扩展名嗅探）。
        assert_eq!(sent[0].2.len(), 2);
        assert_eq!(sent[0].2[0].filename, "b.pdf");
        assert_eq!(sent[0].2[0].bytes, b"%PDF-1.7 body");
        assert_eq!(sent[0].2[0].mime, "application/pdf");
        assert_eq!(sent[0].2[1].filename, "a.bin");
        assert_eq!(sent[0].2[1].bytes, b"BLOBBYTES");
        assert_eq!(sent[0].2[1].mime, "application/x-custom");
        // CRLF 剥离在宿主侧完成（插件拿到的是消毒后的请求）。
        let fwd: Value = serde_json::from_str(&sent[0].1).unwrap();
        assert_eq!(fwd["subject"], "hiBcc: evil@y.com");
        assert_eq!(fwd["headers"]["X-Custom"], "vX-Injected: 1");
        // 引擎开关按方法覆写；profile key 按实例走。
        let f1: Value = serde_json::from_str(&sent[1].1).unwrap();
        assert_eq!(
            (f1["sync"].as_bool(), f1["enqueue_only"].as_bool()),
            (Some(true), Some(false))
        );
        let f2: Value = serde_json::from_str(&sent[2].1).unwrap();
        assert_eq!(
            (f2["sync"].as_bool(), f2["enqueue_only"].as_bool()),
            (Some(false), Some(true))
        );
        assert_eq!(sent[3].0, "alerts");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 校验失败经 JS 拿到 `{code:5}`（resolve，不抛）；`mail not configured` 才抛异常。
    #[tokio::test(flavor = "current_thread")]
    async fn js_mail_validation_envelope_and_not_configured_throw() {
        let _g = lock();
        let fake = FakeMail::new(whitelisted_config(), Arc::new(Bus::new()));
        let b = mail_bridge(fake.clone(), None, None);
        let v = run_js(
            &b,
            r#"(async () => {
                const bad = await mail.send({ from: "noreply@x.com", to: ["b@evil.com"], text: "x" });
                const raw = await mail.sendRaw({ from: "noreply@x.com", to: ["a@x.com"] });
                json.ok({ bad, raw });
              })().catch((e) => json.ok({ err: String(e) }));
              "#,
        )
        .await;
        assert_eq!(v["data"]["bad"]["code"], 5, "{v}");
        assert!(
            v["data"]["bad"]["msg"]
                .as_str()
                .unwrap()
                .contains("allowed_recipients"),
            "{v}"
        );
        assert_eq!(v["data"]["raw"]["code"], 5, "{v}");
        assert!(fake.sent.lock().unwrap().is_empty());

        // 未配置（无 smtp/插件）→ 抛明确错误（不 panic、不静默）。
        let plain = Bridge::new(
            Arc::new(InMemoryAccessor::new()),
            Arc::new(InMemoryKV::new()),
        );
        let v = run_js(
            &plain,
            r#"(async () => { await mail.send({ from: "a@x.com", to: ["b@x.com"], text: "x" }); json.ok({}); })()
                 .catch((e) => json.ok({ err: String(e) }));
              "#,
        )
        .await;
        assert!(
            v["data"]["err"]
                .as_str()
                .unwrap()
                .contains("mail not configured"),
            "{v}"
        );
    }

    /// `mail.result(id)`（异步上送结果）+ `Mail.profiles()`（非密钥面）。
    #[tokio::test(flavor = "current_thread")]
    async fn js_mail_result_and_profiles() {
        let _g = lock();
        let fake = FakeMail::new(whitelisted_config(), Arc::new(Bus::new()));
        let b = mail_bridge(fake.clone(), None, None);
        // 插件 worker 上送完成结果 → 宿主存下（供 enqueue 的调用方回查）。
        deliver_to_host(MAIL_RESULT_TOPIC, ENVELOPE);
        let v = run_js(
            &b,
            r#"(async () => {
                const r = await mail.result("j1");
                const miss = await mail.result("nope");
                const p = await Mail.profiles();
                json.ok({ r, miss, p });
              })().catch((e) => json.ok({ err: String(e) }));
              "#,
        )
        .await;
        assert_eq!(v["data"]["r"]["code"], 0, "{v}");
        assert_eq!(v["data"]["r"]["messageId"], "m1");
        assert!(v["data"]["r"].get("subject").is_none(), "{v}");
        assert_eq!(v["data"]["miss"], Value::Null);
        assert_eq!(v["data"]["p"], json!(["default"]));
    }
}
