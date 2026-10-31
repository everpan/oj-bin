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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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

/// 调用方上下文（B3 归属校验的**最小充分集**）：模块名 + 租户 id。
///
/// 二者都取自 `ReqState`（HTTP / WS / `oj test` 三个入口的 `OpState` 均由 `bridge_ext`
/// 注入 `ReqState`，见 `src/bridge/mod.rs` 的 `state` 闭包）：
/// - `module` = [`crate::bridge::ReqState::module`]（`run_module` 按目录命中注入的模块名）；
/// - `tenant` = `ReqState.req.tenant_id`（tenant 启用时由租户头注入）。
///
/// **能力边界**：宿主 op 拿不到「调用者是哪个 JS handler/用户」——`http.user` 只有已验签
/// 用户的 claims，粒度比模块粗且匿名路径为空。故归属标识取「模块 + 租户」这一对
/// （HTTP 与 WS 两条路都有，且是框架既有的隔离边界）。二者缺失（无模块上下文、未启用
/// 租户）时退化为空串，归属比较随之退化为「profile 相同」。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallCtx {
    /// 本请求所属模块名（`None` = 无模块上下文，如内省/测试驱动）。
    pub module: Option<String>,
    /// 租户 id（`None` = 未启用租户或未带头）。
    pub tenant: Option<String>,
}

impl CallCtx {
    /// 从 op 的 `OpState` 取当前请求上下文（每次调用取最新值，与 `http` 全局同源）。
    pub fn from_state(state: &OpState) -> Self {
        let rs = state.borrow::<super::ReqState>();
        Self {
            module: rs.module.clone(),
            tenant: rs.req.tenant_id.clone(),
        }
    }
}

/// 结果的**归属**（B3）：jobId 与「profile + 模块 + 租户」绑定，`mail.result` 只回本归属的结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MailOwner {
    /// 提交时的 profile key（`new Mail(key)` 的 key）。
    pub profile: String,
    /// 提交时的模块名（缺省空串）。
    pub module: String,
    /// 提交时的租户 id（缺省空串）。
    pub tenant: String,
}

impl MailOwner {
    /// 由 profile key + 调用上下文构造（提交与查询两侧**同一函数** ⇒ 口径不分裂）。
    pub fn new(profile: &str, ctx: &CallCtx) -> Self {
        Self {
            profile: profile.to_string(),
            module: ctx.module.clone().unwrap_or_default(),
            tenant: ctx.tenant.clone().unwrap_or_default(),
        }
    }
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

/// 结果存储：`jobId → (归属, 扁平信封?)`，**限长 + TTL**（惰性清理，无后台任务）。
///
/// 三态语义（B3）：
/// 1. **登记**（[`Self::reserve`]）：宿主在 `submit` **之前**为 enqueue 路开出票 —— 归属在此
///    落定，且同 id 不覆盖；
/// 2. **完成**（[`Self::complete`]）：插件经 `deliver` 上送只能**填充**已登记且尚无结果的槽；
///    未登记（伪造 id）或已有结果（重复/覆写）一律拒绝 ⇒ `code:0` 无法被事后改写；
/// 3. **读取**（[`Self::get`]）：只有归属完全一致的调用方才拿得到（防跨模块/跨租户枚举）。
pub struct MailResultStore {
    cap: usize,
    ttl: Duration,
    entries: Mutex<VecDeque<StoreEntry>>,
}

/// 一条结果槽：归属 + 写入时刻 + 结果（`None` = 已登记、完成上送尚未到达）。
struct StoreEntry {
    job_id: String,
    owner: MailOwner,
    at: Instant,
    result: Option<Value>,
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

    /// 登记一个待完成的票（宿主生成 jobId 后、`submit` 之前调用）。
    /// 同 jobId 已存在 → `false`（**不覆盖**既有归属/结果）。
    pub fn reserve(&self, job_id: &str, owner: MailOwner) -> bool {
        let now = Instant::now();
        let mut g = self.entries.lock().unwrap();
        purge(&mut g, now, self.ttl);
        if g.iter().any(|e| e.job_id == job_id) {
            return false;
        }
        g.push_back(StoreEntry {
            job_id: job_id.to_string(),
            owner,
            at: now,
            result: None,
        });
        while g.len() > self.cap {
            g.pop_front();
        }
        true
    }

    /// 填入结果（`deliver` 上送的唯一落点；见 [`CompleteOutcome`]）。
    pub fn complete(&self, job_id: &str, v: Value) -> CompleteOutcome {
        let now = Instant::now();
        let mut g = self.entries.lock().unwrap();
        purge(&mut g, now, self.ttl);
        let Some(e) = g.iter_mut().rev().find(|e| e.job_id == job_id) else {
            return CompleteOutcome::UnknownJob;
        };
        if e.result.is_some() {
            return CompleteOutcome::Duplicate;
        }
        e.result = Some(v);
        CompleteOutcome::Filled
    }

    /// 读取：归属（profile + 模块 + 租户）不一致 → 与「未命中」同（`None`），不泄露存在性。
    pub fn get(&self, job_id: &str, owner: &MailOwner) -> Option<Value> {
        let now = Instant::now();
        let mut g = self.entries.lock().unwrap();
        purge(&mut g, now, self.ttl);
        g.iter()
            .rev()
            .find(|e| e.job_id == job_id && &e.owner == owner)
            .and_then(|e| e.result.clone())
    }

    /// 存活条目数（含惰性清理；登记未完成的也算）。
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

fn purge(g: &mut VecDeque<StoreEntry>, now: Instant, ttl: Duration) {
    g.retain(|e| now.saturating_duration_since(e.at) < ttl);
}

/// [`MailResultStore::complete`] 的结果：区分「填上了」与两种**拒绝**（都不改存储）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompleteOutcome {
    /// 已填入结果。
    Filled,
    /// 该 jobId **未经宿主登记**（伪造 id / 非本宿主开出的票）。
    UnknownJob,
    /// 该 jobId **已有结果** → 拒绝覆盖（重复/伪造上送）。
    Duplicate,
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

/// [`MailResultRouter::route`] 的三态结果 + 拒绝态（调用方据此分别告警，不混为一句）。
pub enum RouteOutcome {
    /// 已填充结果并扇出给 n 个本地订阅者。
    Routed(usize),
    /// 载荷不是可索引的完成结果（非 JSON，或缺 `jobId`/`code`/`msg`）。
    BadPayload,
    /// `jobId` **未经宿主登记**（不是本宿主开出的票）→ 丢弃。
    UnknownJob,
    /// `jobId` 已有结果 → **拒绝覆盖**（重复/伪造上送）。
    Duplicate,
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

    /// 登记待完成票（宿主 `handle_send` 在 `submit` 前调用；见 [`MailResultStore::reserve`]）。
    pub fn reserve(&self, job_id: &str, owner: MailOwner) -> bool {
        self.store.reserve(job_id, owner)
    }

    /// 填充结果（未登记/已存在 → 见 [`CompleteOutcome`]）。
    pub fn complete(&self, job_id: &str, v: Value) -> CompleteOutcome {
        self.store.complete(job_id, v)
    }

    /// 存储读取（`op_mail_result`）：归属不符 → `None`（与未命中同）。
    pub fn get(&self, job_id: &str, owner: &MailOwner) -> Option<Value> {
        self.store.get(job_id, owner)
    }

    /// 路由一帧插件上送：结果只进「已登记且尚无结果」的槽（见 [`RouteOutcome`]）。
    pub fn route(&self, payload: &[u8]) -> RouteOutcome {
        let Some(env) = serde_json::from_slice::<Value>(payload).ok() else {
            return RouteOutcome::BadPayload;
        };
        let Some(flat) = flatten_result(&env) else {
            return RouteOutcome::BadPayload;
        };
        let job_id = flat
            .get("jobId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        match self.store.complete(&job_id, flat.clone()) {
            CompleteOutcome::Filled => RouteOutcome::Routed(
                self.bus
                    .publish_local(MAIL_RESULT_TOPIC, &BusPayload::Json(flat)),
            ),
            // 区分「伪造的 id」与「重复上送」：两者都拒绝，但排障方向不同。
            CompleteOutcome::UnknownJob => RouteOutcome::UnknownJob,
            CompleteOutcome::Duplicate => RouteOutcome::Duplicate,
        }
    }
}

/// 进程级 deliver 路由槽：`HostContext.deliver` 是无状态 `extern "C"`，
/// 只能经全局弱引用到达装配期注入的后端（强引用由 `StableState.mail` 持有）。
static MAIL_DELIVER: LazyLock<Mutex<Option<Weak<dyn MailBackend>>>> =
    LazyLock::new(|| Mutex::new(None));

/// 装配期挂载路由（server 经 Bridge 构造、exec/test 手工 runtime 经 assemble_backend
/// 显式调用各装一次；重复安装只是覆写同一弱引用，幂等）。
pub fn install_mail_deliver(b: &Arc<dyn MailBackend>) {
    *MAIL_DELIVER.lock().unwrap() = Some(Arc::downgrade(b));
}

/// 路由是否已挂载且后端仍存活（exec 装配自检 / 测试断言用）。
pub fn mail_deliver_installed() -> bool {
    MAIL_DELIVER
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|w| w.strong_count() > 0)
}

/// `deliver(MAIL_RESULT_TOPIC, payload)` 的宿主落点结果（**细分成因**：告警文案要能分辨
/// 「没配 mail」「载荷非法」「id 不是本宿主开出的票」「重复上送」——都被丢弃，但排障方向
/// 完全不同）。
pub(crate) enum DeliverRoute {
    /// 有后端接管：已存结果并扇出（扇出数见 `MailResultRouter::route`）。
    Routed,
    /// 未配置 mail（`StableState.mail` 为空，或弱引用已失效）。
    NotConfigured,
    /// 载荷不是可索引的完成结果（非 JSON，或缺 `jobId`/`code`/`msg`）。
    BadPayload,
    /// `jobId` 未经宿主登记（不是本宿主开出的票，见 B3）。
    UnknownJob,
    /// `jobId` 已有结果 → 拒绝覆盖（重复/伪造上送，见 B3）。
    Duplicate,
}

/// `deliver(MAIL_RESULT_TOPIC, payload)` 的宿主落点：`Routed` = 存结果 + 扇出；
/// 其余成因见 [`DeliverRoute`]（调用方分别告警，不混为一句）。
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
        RouteOutcome::Routed(_) => DeliverRoute::Routed,
        RouteOutcome::BadPayload => DeliverRoute::BadPayload,
        RouteOutcome::UnknownJob => DeliverRoute::UnknownJob,
        RouteOutcome::Duplicate => DeliverRoute::Duplicate,
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
///
/// B5 补入三个**弱 spoof 面**头：`Sender`（实际提交者，与 From 不一致即冒充）、
/// `Return-Path`（退回地址，按 RFC 5321 只能由收信方添加）、`Reply-To`（把回复引向别处）。
const STRUCTURED_HEADERS: [&str; 8] = [
    "from",
    "to",
    "cc",
    "bcc",
    "subject",
    "sender",
    "return-path",
    "reply-to",
];

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
///
/// 未命中时**只回「未命中 + profile 名」**（B6）：回显整份白名单等于把该 profile 的
/// 内域/客户域清单交给任何调用方（换个 profile key 就能枚举他 profile 的白名单）。
pub fn check_whitelist(
    from: &str,
    rcpts: &[String],
    cfg: &MailProfileCfg,
    profile: &str,
) -> Result<(), String> {
    if !whitelist_hit(&cfg.allowed_from, from) {
        return Err(format!(
            "mail: from '{from}' 未命中 profile '{profile}' 的 allowed_from 白名单（下一步：在 smtp.{profile}.allowed_from 里加上该地址，或改用白名单内的发件人）"
        ));
    }
    for r in rcpts {
        if !whitelist_hit(&cfg.allowed_recipients, r) {
            return Err(format!(
                "mail: 收件人 '{r}' 未命中 profile '{profile}' 的 allowed_recipients 白名单（下一步：在 smtp.{profile}.allowed_recipients 里加上该域/地址，或去掉该收件人）"
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
                "mail: headers 不允许覆盖 {name}（From/To/Cc/Bcc/Subject/Sender/Return-Path/Reply-To 由结构化字段或信封决定；下一步：换一个自定义头名）"
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

/// 宿主生成的 jobId（B3）：`<每进程随机 16 hex>-<单调计数>`。
///
/// 旧形态是插件侧 `{pid}-{seq}`（进程号可枚举 + 计数从 1 起）且宿主不剥调用方传入的
/// `jobId` ⇒ 任意模块可猜出他人 jobId 读结果、并可**覆写**成 `code:0`。现在：随机前缀
/// 不可猜（每进程一次从系统熵取 8 字节），计数只保证进程内唯一。
fn next_job_id() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(1);
    format!("{}-{}", job_prefix(), SEQ.fetch_add(1, Ordering::Relaxed))
}

/// 每进程随机的 jobId 前缀（16 hex = 64 bit）。
///
/// 熵不可得（极罕见）时回落「时间纳秒 + pid」：仍不可枚举，且**不**让发信失败。
fn job_prefix() -> &'static str {
    static PREFIX: LazyLock<String> = LazyLock::new(|| {
        let mut b = [0u8; 8];
        if getrandom::getrandom(&mut b).is_err() {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            return format!("{nanos:x}{:x}", std::process::id());
        }
        b.iter().map(|x| format!("{x:02x}")).collect()
    });
    PREFIX.as_str()
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
    check_whitelist(from_norm, &rcpts, prof, key)?;
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
        // B5：raw 路**无处安放**结构化 headers（报头由原文承载，插件只用 raw + subject）——
        // 原实现静默忽略，等于让调用方以为头加上了。改为 fail-loud。
        let has_headers = obj
            .get("headers")
            .is_some_and(|h| !h.is_null() && h.as_object().is_some_and(|m| !m.is_empty()));
        if has_headers {
            return Err(
                "mail: sendRaw 与 headers 互斥（报头由 raw 原文承载，结构化 headers 不会生效）（下一步：把该头写进 raw 原文，或改用 mail.send）"
                    .to_string(),
            );
        }
        obj.remove("headers");
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
///
/// **jobId 由宿主生成**（B3，剥离调用方自带值）：`enqueue` 路先登记 pending 槽位，插件
/// 随后的完成上送只能填充这张票（见 [`MailResultStore`]）；回执的 `data.jobId` 也钉成
/// 宿主值，保证 JS 拿到的 id 一定可 `mail.result` 回查。
pub async fn handle_send(
    backend: Arc<dyn MailBackend>,
    blobs: Arc<BlobRegistry>,
    project_root: Option<&Path>,
    key: &str,
    req_json: &str,
    mode: MailMode,
    ctx: &CallCtx,
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
    let job_id = next_job_id();
    if let Some(o) = req.as_object_mut() {
        // 宿主权威：调用方自带的 jobId 一律被覆盖（它可被用来指向/覆写他人的结果槽）。
        o.insert("jobId".into(), Value::String(job_id.clone()));
        o.insert("sync".into(), Value::Bool(mode == MailMode::Sync));
        o.insert(
            "enqueue_only".into(),
            Value::Bool(mode == MailMode::Enqueue),
        );
    }
    let router = backend.router();
    if mode == MailMode::Enqueue {
        // 登记必须在 submit **之前**：完成上送可能先于 submit 返回。
        router.reserve(&job_id, MailOwner::new(key, ctx));
    }
    let env = match backend.submit(key, req.to_string(), atts).await {
        Ok(env) => env,
        Err(e) => json!({
            "code": 1,
            "msg": format!("mail: 投递未能送达插件（{e}）（下一步：确认 oj-mail 插件已装配、profile 名正确）"),
            "data": {},
        }),
    };
    if mode != MailMode::Enqueue {
        return env;
    }
    let env = pin_job_id(env, &job_id);
    if env.get("code").and_then(Value::as_i64) != Some(0) {
        // 提交失败（队列满 / 未送达插件）：该票的完成上送不会再发生，直接收敛为终态，
        // 否则 `mail.result(jobId)` 会永久 null（且槽位要等 TTL 才被清理）。
        if let Some(flat) = flatten_result(&env) {
            router.complete(&job_id, flat);
        }
    }
    env
}

/// 把回执的 `data.jobId` 钉成宿主生成的那个（enqueue 路）。
///
/// 不依赖插件回显 `jobId`：第三方/旧版插件可能回自己的 id，那样 JS 拿到的 id 查不到结果
/// （`mail.result` 永远 null）；宿主是 jobId 的权威来源，回执就按宿主的算。
fn pin_job_id(env: Value, job_id: &str) -> Value {
    let mut env = env;
    let Some(o) = env.as_object_mut() else {
        return env;
    };
    if !o.get("data").is_some_and(Value::is_object) {
        o.insert("data".into(), json!({}));
    }
    if let Some(d) = o.get_mut("data").and_then(Value::as_object_mut) {
        d.insert("jobId".into(), Value::String(job_id.to_string()));
    }
    env
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
    // 借用必须先结束（下方有 await）：deps 与 ctx 都是自有值。
    let (backend, blobs, root, ctx) = {
        let g = state.borrow();
        let (backend, blobs, root) = mail_deps(&g)?;
        (backend, blobs, root, CallCtx::from_state(&g))
    };
    Ok(handle_send(backend, blobs, root.as_deref(), &key, &req_json, mode, &ctx).await)
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

/// mail.result(id)：查宿主侧存储的异步结果（未命中/已过期/**非本归属** → `null`）。
///
/// 归属（B3） = 提交时的 `key`（profile）+ 模块名 + 租户 id（取自 `ReqState`）；
/// 三者任一不同都视作「未命中」——不泄露该 jobId 是否存在（见 [`MailOwner`]）。
#[op2]
#[serde]
pub async fn op_mail_result(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
    #[string] job_id: String,
) -> Result<serde_json::Value, JsErrorBox> {
    let g = state.borrow();
    let (backend, ..) = mail_deps(&g)?;
    let owner = MailOwner::new(&key, &CallCtx::from_state(&g));
    // 出口护栏（见 jsnum）：结果体是插件侧 JSON，若含雪花量级整数会以 v8 BigInt 交给 JS
    // （`json.ok` 直接 500）。与其它 op 出口一致降为十进制字符串。
    let mut out = backend.router().get(&job_id, &owner).unwrap_or(Value::Null);
    super::jsnum::sanitize_js_numbers(&mut out);
    Ok(out)
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
#[path = "mail_tests.rs"]
mod tests;
