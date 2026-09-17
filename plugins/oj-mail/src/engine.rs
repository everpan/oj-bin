//! `MailEngine`：有界队列 + worker 池 + 背压 + graceful drain（阶段 4）。
//!
//! ## 强引用释放顺序（「顺序即契约」，A3）
//!
//! `submit` 的 ctl drain 与 `Drop` 都要求：**`drain` 收齐全部 worker 退出信号时，worker
//! 手里不再有任何 transport 强引用**，于是最后一份强引用的销毁点确定落在 [`dispose`] 的
//! `rt.enter()` 内（或 worker 自己的 runtime 上下文里）。`async move` 块的捕获变量只在
//! **future 被 drop** 时释放（实测），故 worker 块里必须**显式** `drop(targets)`（再按依赖序
//! drop `deliver`/`rx`）**之后**才发退出信号 —— 见 `with_rt` 的注释。
//!
//! ## 为什么引擎自建 tokio runtime
//!
//! lettre 的 `pool` 在 `AsyncSmtpTransport` 的 **Drop** 里 `E::spawn(...)`
//! （= `tokio::spawn`，见 lettre-0.11.23 `transport/smtp/pool/async_impl.rs:262`），
//! 故 transport 的 **创建 / 使用 / 销毁** 都必须处于引擎自己的 runtime 上下文内，
//! 否则 Drop 中的 `tokio::spawn` 会 panic（且发生在析构中 → 进程 abort）。
//!
//! ## 为什么销毁用 `shutdown_background`（而不是裸 drop runtime）
//!
//! 反过来，`Runtime::drop` 会关闭 blocking pool 并**阻塞等待**；tokio 禁止在 runtime
//! 上下文（async 任务）里做这件事 —— 直接 panic（tokio `runtime/blocking/shutdown.rs:51`
//! "Cannot drop a runtime in a context where blocking is not allowed"）。而调用 `submit` 的
//! 侧（JsRuntime 的 `current_thread` 运行时）与单测都在 runtime 上下文里，故 [`dispose`]
//! 用 tokio 专为「在另一个 runtime 里 drop runtime」提供的非阻塞关闭
//! [`Runtime::shutdown_background`]：先 `rt.enter()` 释放 transport（约束 1），再非阻塞关闭
//! runtime（约束 2）。销毁顺序固定为 **transport → runtime**。
//!
//! ## 停机入口：控制报文（A4）
//!
//! 插件引擎是 `OnceLock` 单例（宿主只给 `&`），且 `MailVtable` 形状不许改（ABI 严格相等），
//! 故 graceful drain 的**生产**入口是 `submit` 的控制报文 `{"__ctl":"drain","timeout_ms":N}`
//! （见 [`CTL_KEY`]）——宿主在 `oj server` 停机路径（HTTP 停收 + 任务收场之后）调用。
//!
//! ## 分期边界
//!
//! 消息组装（text/html/headers/附件 MIME）与附件字节消费归**阶段 5**（`message.rs`）：
//! worker 投递前按 `req.raw` 有无二选一 —— 有 `raw` → 原文（剥离冲突头），无 → 结构化组装。

use crate::message::{SendRequest, build_message, build_raw, envelope_of};
use crate::{build_profiles, config::MailConfig};
use lettre::address::Envelope;
use oj_plugin_ffi::{FfiFuture, MailAttachment, ready_err, ready_ok, spawn_ffi_future};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::runtime::Runtime;
use tokio::sync::{mpsc, oneshot};

/// 结果上送主题（宿主侧据此存结果 + 扇出；design §7）。
pub const TOPIC_MAIL_RESULT: &str = "mail.result";

/// 业务码（design §10）：0 成功 / 1 连接·网络·超时 / 4 队列满 / 5 入参校验。
pub const CODE_OK: i32 = 0;
/// 连接、网络、超时类失败。
pub const CODE_NETWORK: i32 = 1;
/// 背压：队列满（`submit` 立即返回，不阻塞）。
pub const CODE_QUEUE_FULL: i32 = 4;
/// 入参/契约校验失败。
pub const CODE_VALIDATION: i32 = 5;

/// 引擎已停机（`drain` 后）时的统一错误串。
const STOPPED: &str = "mail: engine 已停机（不再接收投递）";

/// 控制报文键（`req` JSON 顶层）：`{"__ctl":"drain","timeout_ms":10000}`。
/// 控制报文走**既有** `submit` 入口（vtable 形状不变 ⇒ **零 ABI 变更**）——宿主停机时
/// 用它触发 graceful drain（见 [`Ctl::Drain`]）。
pub const CTL_KEY: &str = "__ctl";
/// `__ctl` 的取值：停机排空（graceful drain）。回信封
/// `{"code":0,"msg":"ok","data":{"drained":true,"workers":N}}`；drain 超时则 `code:1`
/// （已停收 + 已销毁，在途 job 可能被丢弃）。
pub const CTL_DRAIN: &str = "drain";
/// 控制报文未带 `timeout_ms` 时的默认 drain 总超时（宿主一般显式给）。
const DEFAULT_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// 控制报文（`req` 顶层 `__ctl`）。
enum Ctl {
    /// 停机排空：停收 → 等在途跑完（总超时）→ 销毁 transport 与 runtime。
    Drain { timeout: Duration },
    /// 未识别的控制名（fail-loud，不静默当业务 req 解析）。
    Unknown(String),
}

/// 解析控制报文：`req` 顶层 `__ctl` 为字符串即控制报文（其余字段一概不看）。
/// 非对象/无 `__ctl` → `None`（走业务路）。`timeout_ms` 仅在 `Drain` 下有意义。
fn ctl_request(req: &str) -> Option<Ctl> {
    let v: serde_json::Value = serde_json::from_str(req).ok()?;
    let name = v.get(CTL_KEY)?.as_str()?;
    Some(match name {
        CTL_DRAIN => Ctl::Drain {
            timeout: v
                .get("timeout_ms")
                .and_then(serde_json::Value::as_u64)
                .map_or(DEFAULT_DRAIN_TIMEOUT, Duration::from_millis),
        },
        other => Ctl::Unknown(other.to_string()),
    })
}

/// 异步投递函数的返回 future（须 `Send`：worker 任务跑在引擎多线程 runtime 上）。
pub type SendFuture<'a> = Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>>;
/// 异步投递函数：`(信封, RFC5322 原文) → 投递凭据（messageId）/ 失败原因`。
/// 生产实现包 lettre 的 async transport；测试注入可控桩（免网络、可闩锁）。
pub type SendFn = Arc<dyn Fn(Envelope, Vec<u8>) -> SendFuture<'static> + Send + Sync>;
/// 同步投递函数（`req.sync = true` 时由 worker 放进 blocking 池调用）。
pub type SyncSendFn = Arc<dyn Fn(Envelope, Vec<u8>) -> Result<String, String> + Send + Sync>;

/// 结果上送闭包（`DeliverSink` 的内层形态：共享 + 线程安全）。
type DeliverFn = dyn Fn(&str, &[u8]) + Send + Sync;

/// 结果上送（依赖倒置）：生产转发 `HostContext.deliver`，测试注入收集器。
/// 不持有 `HostContext` 本身，故 enqueue 路可在无宿主（单测）下断言上送内容。
#[derive(Clone)]
pub struct DeliverSink(Arc<DeliverFn>);

impl DeliverSink {
    /// 由闭包构造。**实现必须快速返回**：worker 在投递路径上同步调用它。
    pub fn new<F>(f: F) -> Self
    where
        F: Fn(&str, &[u8]) + Send + Sync + 'static,
    {
        Self(Arc::new(f))
    }

    /// 上送一条消息（非阻塞）。
    pub fn emit(&self, topic: &str, payload: &[u8]) {
        (self.0)(topic, payload);
    }
}

/// 单个 profile 的投递能力（引擎视角）：超时 + 两路投递函数。
pub struct MailTarget {
    /// 单次投递超时（`tokio::time::timeout`；超时 → `code:1`）。
    pub timeout: Duration,
    /// 异步路（`AsyncSmtpTransport` / `AsyncFileTransport`）。
    pub send: SendFn,
    /// 同步路（`SmtpTransport` / `FileTransport`；worker 内 `spawn_blocking`）。
    pub send_sync: SyncSendFn,
}

impl MailTarget {
    /// 完整构造（两路都显式给）：生产与测试同此一处。
    pub fn new(timeout: Duration, send: SendFn, send_sync: SyncSendFn) -> Self {
        Self {
            timeout,
            send,
            send_sync,
        }
    }

    /// 只给异步路。同步路**显式报错**（而非静默走异步）——避免「以为发了 sync 实际没发」。
    pub fn async_only(timeout: Duration, send: SendFn) -> Self {
        Self::new(
            timeout,
            send,
            Arc::new(|_, _| Err("mail: 该 target 未注入同步投递（测试桩）".to_string())),
        )
    }
}

/// 队列中的一个投递任务。
struct Job {
    /// profile 名（`smtp.profiles` 的键）；未知则投递失败（不回落 default）。
    key: String,
    /// 宿主过线的 req JSON 原文（worker 侧重新解析；见 [`Req`]）。
    req: String,
    /// 宿主已解析好的附件原始字节（阶段 5 消费：按下标与 `attachments` 对齐）。
    atts: Vec<MailAttachment>,
    /// `send` 语义下的结果回传口；`enqueue_only` 时为 `None`（完成经 `deliver` 上送）。
    respond: Option<oneshot::Sender<Result<String, String>>>,
    /// 是否「入队即返回」语义。
    enqueue_only: bool,
    /// 作业号（宿主给或引擎生成；回传/上送都带它）。
    job_id: String,
    /// 是否走同步 transport（worker 内 `spawn_blocking`）。
    sync: bool,
}

/// req 视图 = 引擎侧开关（`sync`/`enqueue_only`/`jobId`）+ 组装字段（[`SendRequest`]）。
#[derive(Deserialize)]
struct Req {
    #[serde(default)]
    sync: bool,
    /// 兼容 `enqueueOnly` 写法（宿主/JS 两侧命名习惯不同，两个都收）。
    #[serde(default, alias = "enqueueOnly")]
    enqueue_only: bool,
    /// 作业号（缺省由引擎生成；`jobId` 为契约字段，`job_id` 作兼容别名）。
    #[serde(default, rename = "jobId", alias = "job_id")]
    job_id: Option<String>,
    /// 消息组装字段（`from`/`to`/`cc`/`bcc`/`subject`/`text`/`html`/`headers`/`attachments`/`raw`）。
    #[serde(flatten)]
    message: SendRequest,
}

/// 引擎：有界队列 + N worker + 结果回传/上送 + 可 drain 的停机。
pub struct MailEngine {
    /// 有界队列发送端。`Mutex<Option<_>>`：`drain` 需 `take` 后 **drop** 才能关闭接收端
    /// （所有 `Sender` 都没了，worker 的 `recv` 才会返回 `None` 而退出）。
    tx: Mutex<Option<mpsc::Sender<Job>>>,
    /// profile 名 → 投递目标。`Option`：`drain`/`Drop` 时 take 出来交给 [`dispose`]
    /// 在 **rt 上下文内**释放（lettre pool transport 的 Drop 需要 runtime 上下文）。
    /// `Mutex` 仅为在 `&self`（生产入口：控制报文 drain）下也能 take —— `submit` 只读。
    targets: Mutex<Option<Arc<HashMap<String, MailTarget>>>>,
    /// 引擎 runtime；同上，take 出去销毁（drop runtime 会阻塞，不能在 async 上下文做）。
    rt: Mutex<Option<Runtime>>,
    /// worker 退出信号（std 通道：drain 的等待是**同步阻塞**，不走 `block_on`，
    /// 故 `drain` 在 async 上下文里调用也安全）。`Mutex` 仅为满足 `Sync`
    /// （`mpsc::Receiver` 不是 `Sync`；本插件引擎要放进 `OnceLock` 静态）。
    exits: Mutex<std::sync::mpsc::Receiver<()>>,
    /// worker 数（drain 需收齐这么多退出信号）。
    workers: usize,
    /// 单附件字节上限（B2 **纵深防御**：宿主已按其 `smtp.max_attachment_bytes` 判过；
    /// 插件再判一次，防第三方宿主漏判 —— 字节是宿主读盘后过线来的）。
    max_attachment_bytes: usize,
    /// 单封附件合计上限（同上）。
    max_total_attachment_bytes: usize,
}

impl MailEngine {
    /// 生产构造：按 `smtp` 配置建 transport 与 worker。
    ///
    /// **硬约束**：transport 的构建必须在引擎 runtime 上下文内（见模块头）——
    /// `rt.enter()` 之后才调 `build_profiles`。
    pub fn new(cfg: &MailConfig, deliver: DeliverSink) -> Result<Self, String> {
        let rt = runtime(cfg.workers)?;
        let built = {
            let _g = rt.enter();
            build_profiles(cfg)
        };
        let targets = match built {
            Ok(profiles) => profiles
                .into_iter()
                .map(|(name, p)| (name, p.into_target()))
                .collect(),
            Err(e) => {
                // 失败路径同样按序销毁：不得在调用者线程上裸 drop runtime（async 上下文会 panic）。
                dispose(rt, None);
                return Err(e);
            }
        };
        Self::with_rt(
            rt,
            targets,
            cfg.workers,
            cfg.queue_capacity,
            (cfg.max_attachment_bytes, cfg.max_total_attachment_bytes),
            deliver,
        )
    }

    /// 用显式投递表构造（**测试注入口**：免网络、可闩锁；生产走 [`MailEngine::new`]）。
    /// `#[cfg(test)]`：cdylib 没有库消费者，注入口不必进发布产物。
    /// 附件上限取 `usize::MAX`（用例不测上限；上限用例走 `MailEngine::new` + 真 cfg）。
    #[cfg(test)]
    pub fn with_targets(
        targets: HashMap<String, MailTarget>,
        workers: usize,
        queue_capacity: usize,
        deliver: DeliverSink,
    ) -> Result<Self, String> {
        let rt = runtime(workers)?;
        Self::with_rt(
            rt,
            targets,
            workers,
            queue_capacity,
            (usize::MAX, usize::MAX),
            deliver,
        )
    }

    /// 装配：起 `workers` 个 worker 消费有界队列。`limits` = (单附件, 单封合计) 字节上限。
    fn with_rt(
        rt: Runtime,
        targets: HashMap<String, MailTarget>,
        workers: usize,
        queue_capacity: usize,
        limits: (usize, usize),
        deliver: DeliverSink,
    ) -> Result<Self, String> {
        // 配置边界 fail-loud：0 会让队列永不被消费 / 每次投递都立即 code 4（都是坏配置）。
        let bad = if workers == 0 {
            Some("mail: workers 必须 ≥ 1（0 会让队列永不被消费）")
        } else if queue_capacity == 0 {
            Some("mail: queue_capacity 必须 ≥ 1（0 会让每次投递都立即 code 4）")
        } else {
            None
        };
        if let Some(msg) = bad {
            dispose(rt, None);
            return Err(msg.to_string());
        }

        let (tx, rx) = mpsc::channel(queue_capacity);
        // `mpsc::Receiver` 是单消费者：多 worker 经 tokio Mutex（FIFO 公平）串行取用，
        // 锁只在 `recv().await` 期间持有 —— 取到 job 立即释放，其余 worker 可并行投递。
        let rx = Arc::new(tokio::sync::Mutex::new(rx));
        let (exit_tx, exits) = std::sync::mpsc::channel();
        let targets = Arc::new(targets);
        for _ in 0..workers {
            let rx = Arc::clone(&rx);
            let targets = Arc::clone(&targets);
            let deliver = deliver.clone();
            let exit_tx = exit_tx.clone();
            rt.spawn(async move {
                // 借用传入：强引用归本块所有。`worker_loop` 按**值**收下的话，强引用会随它的
                // 函数帧释放；而 `async move` 块的捕获变量更晚 —— 只在 **future 被 drop**
                // 时才释放（实测：块体跑完、future 仍存活时捕获变量还在）。两者都晚于下面的
                // 退出信号，于是「信号之后再无 worker 持有的强引用」这条保证不成立。
                worker_loop(&rx, &targets, &deliver).await;
                // **顺序即契约**：退出信号必须在 worker 释放全部强引用**之后**发出。
                // `targets` 持有 transport，而 lettre `pool` 的 Drop 会 `tokio::spawn`
                // （`pool/async_impl.rs:262`）—— 它必须落在 runtime 上下文内，且最后一份
                // 强引用的销毁点要确定（`drain` 收齐信号后由 `dispose` 在 `rt.enter()` 内
                // 释放引擎那一份）。不显式 drop，销毁点就漂到「未来某个线程」上，正确性
                // 依赖 tokio 内部「取消/回收任务时恰好进入了 runtime 上下文」这一实现细节。
                // 依赖序（与 `dispose` 的 transport → runtime 同序）：targets → deliver → rx。
                drop(targets);
                drop(deliver);
                drop(rx);
                let _ = exit_tx.send(());
            });
        }
        drop(exit_tx); // 只留各 worker 手里的副本

        Ok(Self {
            tx: Mutex::new(Some(tx)),
            targets: Mutex::new(Some(targets)),
            rt: Mutex::new(Some(rt)),
            exits: Mutex::new(exits),
            workers,
            max_attachment_bytes: limits.0,
            max_total_attachment_bytes: limits.1,
        })
    }

    /// 取队列发送端（停机后为 `None`）。克隆 `Sender`（廉价）以免跨 `try_send` 持锁。
    fn sender(&self) -> Option<mpsc::Sender<Job>> {
        self.tx.lock().expect("mail: tx 锁中毒").clone()
    }

    /// 非阻塞入队（`MailVtable::submit` 的实现体；vtable 侧另包 `catch_future`）。
    ///
    /// 三种返回：
    /// - 控制报文（`req` 含 `__ctl`）→ 同步执行并回结果信封（见 [`ctl_request`]）；
    /// - `send` 语义 → 等 oneshot 的 `FfiFuture`（由引擎 runtime 驱动，不占调用线程）；
    /// - `enqueue` 语义 → 立即回**统一信封** `{"code":0,"msg":"ok","data":{"jobId":"..."}}`
    ///   （真实完成经 `deliver` 上送；**不是**裸 `{jobId}`，见 [`enqueued_envelope`]）；
    /// - 队列满 → 立即 `{"code":4,...}` 信封（**背压**，绝不阻塞）。
    pub fn submit(&self, key: &str, req: &str, atts: Vec<MailAttachment>) -> FfiFuture {
        // 控制报文先于一切业务校验（不占 profile key、不占队列槽位）。
        if let Some(ctl) = ctl_request(req) {
            return self.run_ctl(ctl);
        }
        // req 形态错误 / 未知 profile = 调用方契约错误 → FFI 层 Err（fail-loud，不占队列槽位）。
        let parsed: Req = match serde_json::from_str(req) {
            Ok(r) => r,
            Err(e) => return ready_err(format!("mail: 请求 JSON 解析失败: {e}")),
        };
        let Some(tx) = self.sender() else {
            return ready_err(STOPPED);
        };
        {
            // 锁只覆盖「查表」：`submit` 不持锁做投递。
            let g = self.targets.lock().expect("mail: targets 锁中毒");
            let Some(targets) = g.as_ref() else {
                return ready_err(STOPPED);
            };
            if !targets.contains_key(key) {
                return ready_err(unknown_profile_msg(key));
            }
        }

        let job_id = parsed.job_id.clone().unwrap_or_else(next_job_id);
        // B2 纵深防御：附件大小上限（宿主侧是主判据；这一层防第三方宿主漏判）。
        // 超限 → `code:5` 信封（**不占队列槽位**，也不进 worker）。
        if let Some(msg) = over_attachment_limit(
            &atts,
            self.max_attachment_bytes,
            self.max_total_attachment_bytes,
        ) {
            return ready_ok(fail_envelope(&job_id, CODE_VALIDATION, &msg));
        }
        let (respond, rx) = if parsed.enqueue_only {
            (None, None)
        } else {
            let (tx, rx) = oneshot::channel();
            (Some(tx), Some(rx))
        };
        let job = Job {
            key: key.to_string(),
            req: req.to_string(),
            atts,
            respond,
            enqueue_only: parsed.enqueue_only,
            job_id: job_id.clone(),
            sync: parsed.sync,
        };

        // **背压**：一律 `try_send`。绝不 `await send` —— 那会阻塞调用线程（JS 事件循环），
        // 队列满时甚至永久等待。
        match tx.try_send(job) {
            Ok(()) => match rx {
                None => ready_ok(enqueued_envelope(&job_id)),
                Some(rx) => {
                    // 已入队 → 取 rt 起 future（drain 竞态：rt 已销毁 ⇒ 回停机信封）。
                    let g = self.rt.lock().expect("mail: rt 锁中毒");
                    let Some(rt) = g.as_ref() else {
                        return ready_err(STOPPED);
                    };
                    spawn_ffi_future(rt, async move {
                        rx.await
                            .map_err(|_| "mail: 投递任务未回传结果（引擎停机？）".to_string())?
                            .map(String::into_bytes)
                    })
                }
            },
            Err(mpsc::error::TrySendError::Full(_)) => ready_ok(queue_full_envelope(&job_id)),
            Err(mpsc::error::TrySendError::Closed(_)) => ready_err(STOPPED),
        }
    }

    /// 控制报文分派（唯一入口 = [`MailEngine::submit`]；`__ctl` 见 [`ctl_request`]）。
    ///
    /// 同步执行并回**统一信封**（`ready_*`：调用方拿到即已就绪，宿主一次 poll 即得结果）。
    fn run_ctl(&self, ctl: Ctl) -> FfiFuture {
        match ctl {
            Ctl::Drain { timeout } => match self.drain(timeout) {
                Ok(()) => ready_ok(
                    json!({
                        "code": CODE_OK, "msg": "ok",
                        "data": {"drained": true, "workers": self.workers},
                    })
                    .to_string(),
                ),
                // 超时不是「没做」而是「没等完」：已停收 + 已销毁，在途 job 可能被丢弃。
                Err(e) => ready_ok(
                    json!({"code": CODE_NETWORK, "msg": e, "data": {"drained": false}}).to_string(),
                ),
            },
            Ctl::Unknown(name) => {
                ready_err(format!("mail: 未知控制报文 '{name}'（支持：{CTL_DRAIN}）"))
            }
        }
    }

    /// graceful drain：停收新 job → 等在途/排队 job 跑完（总超时）→ 销毁 transport 与 runtime。
    ///
    /// 返回 `Err` = 有 worker 未在超时内退出（在途 job 可能被丢弃）。
    ///
    /// **`&self`**：生产停机的唯一入口是控制报文（`{"__ctl":"drain"}`），而插件引擎是
    /// `OnceLock` 单例（只给 `&`）——故 `tx`/`targets`/`rt` 都放在 `Mutex<Option<_>>` 里。
    /// 并发 drain：后到者会等超时并返回 `Err`（生产只有停机一处调用）。
    pub fn drain(&self, timeout: Duration) -> Result<(), String> {
        // 1) 关接收端：drop 唯一的 `Sender` ⇒ 队列排空后各 worker 的 `recv` 返回 `None` 并退出。
        drop(self.tx.lock().expect("mail: tx 锁中毒").take());
        // 2) 等在途/排队 job 跑完（总超时）。退出信号走 **std** 通道：纯同步阻塞等待，
        //    不用 `block_on`（在 async 上下文里起 runtime 会 panic）。
        let deadline = Instant::now() + timeout;
        let mut exited = 0usize;
        while exited < self.workers {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            if self
                .exits
                .lock()
                .expect("mail: exits 锁中毒")
                .recv_timeout(left)
                .is_err()
            {
                break; // 超时或通道断开
            }
            exited += 1;
        }
        // 3) 销毁：transport 先于 runtime（见 `dispose`）。
        self.dispose();
        if exited == self.workers {
            Ok(())
        } else {
            Err(format!(
                "mail: drain 超时，{}/{} 个 worker 未退出（在途 job 可能被丢弃）",
                self.workers - exited,
                self.workers
            ))
        }
    }

    /// 交出 rt/targets 给 [`dispose`] 按序销毁（幂等：已交出则 no-op）。
    fn dispose(&self) {
        let rt = self.rt.lock().expect("mail: rt 锁中毒").take();
        let targets = self.targets.lock().expect("mail: targets 锁中毒").take();
        if let Some(rt) = rt {
            dispose(rt, targets);
        }
    }
}

impl Drop for MailEngine {
    fn drop(&mut self) {
        // 与 `drain` 同序但**不等待**在途 job：
        // 1) 关接收端（`drain` 已关则 no-op）；
        drop(self.tx.get_mut().expect("mail: tx 锁中毒").take());
        // 2) 先 transport 后 runtime。
        self.dispose();
        // 未跑完的在途 job 随 runtime 一起被丢弃——需要「在途必达」请先 `drain`（graceful drain）。
    }
}

/// 引擎 runtime：worker 数即 tokio 工作线程数（worker 基本都在等网络 I/O，不占满线程）。
fn runtime(workers: usize) -> Result<Runtime, String> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers.max(1))
        .thread_name("oj-mail")
        .enable_all()
        .build()
        .map_err(|e| format!("mail: 建 tokio runtime 失败: {e}"))
}

/// 按序销毁引擎资源：**先 transport（必须在 rt 上下文内）后 runtime**。
///
/// 两条相反的约束在这里一次满足：
/// 1. transport 的最后一份强引用必须在**引擎 runtime 上下文内**释放——lettre `pool`
///    的 Drop 会 `tokio::spawn`（`pool/async_impl.rs:262`）；
/// 2. `Runtime::drop` 会阻塞等 blocking pool 收尾，而 tokio 在 runtime 上下文里做这件事
///    会 panic（实测 tokio 1.53 `runtime/blocking/shutdown.rs:51`）——故用
///    [`Runtime::shutdown_background`]：
///    它正是为「在另一个 runtime 里 drop runtime」提供的非阻塞关闭（`runtime.rs`
///    文档与实现 = `shutdown_timeout(Duration::ZERO)`，blocking pool 直接标记已关闭、
///    不做阻塞等待），其后 `Runtime::drop` 亦不再走阻塞分支。
///
/// 于是**不需要**把销毁挪到独立线程，`drain`/`Drop` 在 `#[tokio::test]`、宿主 async
/// 任务、进程退出等任一上下文里都安全。
fn dispose(rt: Runtime, targets: Option<Arc<HashMap<String, MailTarget>>>) {
    {
        let _g = rt.enter();
        drop(targets);
    }
    rt.shutdown_background();
}

/// worker 主循环：`recv` 直到发送端关闭且队列排空（graceful drain 的收敛点）。
///
/// `rx`/`targets`/`deliver` 一律**借用**：强引用归调用方（`with_rt` 的 worker 块）所有，
/// 由它在发出退出信号**之前**显式释放（顺序即契约，见那里的注释）。
async fn worker_loop(
    rx: &Arc<tokio::sync::Mutex<mpsc::Receiver<Job>>>,
    targets: &HashMap<String, MailTarget>,
    deliver: &DeliverSink,
) {
    loop {
        // 锁只在 `recv().await` 期间持有：取到 job 立即释放 ⇒ 其余 worker 可继续取件/并行投递。
        let job = rx.lock().await.recv().await;
        match job {
            Some(job) => run_job(job, targets, deliver).await,
            None => break, // 发送端全部 drop + 队列排空 → 退出
        }
    }
}

/// 处理单个 job：投递 → 回传（`send` 路 oneshot）/ 上送（`enqueue` 路 `deliver`）。
async fn run_job(job: Job, targets: &HashMap<String, MailTarget>, deliver: &DeliverSink) {
    let envelope = deliver_one(&job, targets).await;
    if job.enqueue_only {
        // 异步完成上送宿主（**不经 bus**；宿主侧负责存结果与扇出）。
        deliver.emit(TOPIC_MAIL_RESULT, envelope.as_bytes());
    }
    if let Some(tx) = job.respond {
        // 宿主已放弃该 future（drop 了句柄）时无人接收：结果按契约丢弃，不算错误。
        let _ = tx.send(Ok(envelope));
    }
}

/// 附件上限的**插件侧复核**（B2 纵深防御；宿主侧 `resolve_attachments` 是主判据）。
/// 返回超限文案（`None` = 通过）：单件超限即拒；合计超限点名越界的那个附件。
fn over_attachment_limit(
    atts: &[MailAttachment],
    max_file: usize,
    max_total: usize,
) -> Option<String> {
    let mut total = 0usize;
    for (i, a) in atts.iter().enumerate() {
        let len = a.bytes.len();
        if len > max_file {
            return Some(format!(
                "mail: attachments[{i}]（{}）{len} 字节超过单附件上限 {max_file} 字节（smtp.max_attachment_bytes）（下一步：换更小的附件，或调大该配置）",
                a.filename
            ));
        }
        total = total.saturating_add(len);
        if total > max_total {
            return Some(format!(
                "mail: attachments[{i}]（{}）加入后附件合计 {total} 字节超过单封上限 {max_total} 字节（smtp.max_total_attachment_bytes）（下一步：减小附件或减少数量，或调大该配置）",
                a.filename
            ));
        }
    }
    None
}

/// 投递一个 job，返回结果信封 JSON（**唯一**的信封构造点：sync 回传与 enqueue 上送同形）。
async fn deliver_one(job: &Job, targets: &HashMap<String, MailTarget>) -> String {
    let job_id = job.job_id.as_str();
    let Some(target) = targets.get(&job.key) else {
        // submit 期已 fast-fail 拦一次；此处是权威判定（配置表进程内不可变）——纯兜底。
        return fail_envelope(job_id, CODE_VALIDATION, &unknown_profile_msg(&job.key));
    };
    let req: Req = match serde_json::from_str(&job.req) {
        Ok(r) => r,
        Err(e) => {
            return fail_envelope(job_id, CODE_VALIDATION, &format!("请求 JSON 解析失败: {e}"));
        }
    };
    let (envelope, raw) = match deliver_input(&req, &job.atts) {
        Ok(v) => v,
        Err((code, msg)) => return fail_envelope(job_id, code, &msg),
    };

    let outcome = if job.sync {
        // 同步路：阻塞的 lettre transport 放进 blocking 池，绝不阻塞 worker 的 async 线程。
        // 诚实边界：`timeout` 只停止等待，已在池中执行的阻塞调用会继续跑完（无法取消）。
        let send = Arc::clone(&target.send_sync);
        match tokio::time::timeout(
            target.timeout,
            tokio::task::spawn_blocking(move || send(envelope, raw)),
        )
        .await
        {
            Ok(Ok(r)) => r,
            // JoinError：阻塞任务 panic / 被取消（不是超时）。
            Ok(Err(_)) => return fail_envelope(job_id, CODE_NETWORK, "同步投递任务异常终止"),
            Err(_) => return fail_envelope(job_id, CODE_NETWORK, "投递超时"),
        }
    } else {
        let send = Arc::clone(&target.send);
        match tokio::time::timeout(target.timeout, send(envelope, raw)).await {
            Ok(r) => r,
            Err(_) => return fail_envelope(job_id, CODE_NETWORK, "投递超时"),
        }
    };

    match outcome {
        Ok(message_id) => ok_envelope(job_id, &message_id),
        // 脱敏（design §10/§11）：lettre 原始错误可能含 SMTP 对话/收件人——回传与上送
        // 只出分类文案。原始原因留待阶段 5 经 `HostContext.log` 上送宿主日志（不进信封/总线）。
        Err(_detail) => fail_envelope(job_id, CODE_NETWORK, "投递失败"),
    }
}

/// 由 req 组装投递入参（**阶段 5 的两条路**，错误码一律 `code:5`）：
///
/// - `req.raw` 有 → 原文路：信封由结构化 `from`/`to` 生成（与原文报头**解耦**，防双收件人），
///   正文取 [`build_raw`]：剥离原文 `From`/`To`/`Cc`/`Bcc`；`Subject` 保留，但结构化 `subject`
///   非空时覆盖（设计 §7/§11，2026-09-15 决策）；
/// - 否则 → 结构化组装：`build_message` 出 [`lettre::Message`]，信封由 lettre 按其报头派生
///   （To ∪ Cc ∪ Bcc，且 `Bcc` 报头已丢弃）。
fn deliver_input(req: &Req, atts: &[MailAttachment]) -> Result<(Envelope, Vec<u8>), (i32, String)> {
    let m = &req.message;
    if let Some(raw) = m.raw.as_deref() {
        // raw 路：报头由原文承载，故结构化 cc/bcc **无处安放** —— 静默丢件或塞进 To 头
        // （泄露 Bcc）都不可接受，直接 fail-loud。
        if !m.cc.is_empty() || !m.bcc.is_empty() {
            return Err((
                CODE_VALIDATION,
                "raw 路不支持结构化 cc/bcc（原文的 Cc/Bcc 头会被剥离）：请把它们并入 to，或改走结构化组装（去掉 raw）".to_string(),
            ));
        }
        if !m.attachments.is_empty() {
            return Err((
                CODE_VALIDATION,
                "raw 与附件互斥：raw 给定时附件必须为空（vtable 契约：宿主不为 raw 解析附件）"
                    .to_string(),
            ));
        }
        let envelope = envelope_of(&m.from, &m.to).map_err(|e| (CODE_VALIDATION, e))?;
        let bytes =
            build_raw(&envelope, raw, &m.subject, atts).map_err(|e| (CODE_VALIDATION, e))?;
        return Ok((envelope, bytes));
    }
    let msg = build_message(m, atts).map_err(|e| (CODE_VALIDATION, e))?;
    Ok((msg.envelope().clone(), msg.formatted()))
}

/// 成功信封：`{code:0,msg:"ok",data:{jobId,messageId}}`（`messageId` = 投递凭据）。
fn ok_envelope(job_id: &str, message_id: &str) -> String {
    json!({"code": CODE_OK, "msg": "ok", "data": {"jobId": job_id, "messageId": sanitize_message_id(message_id)}})
        .to_string()
}

/// `messageId` 的长度上限（B6；超长即截断）。
const MESSAGE_ID_MAX: usize = 64;

/// 投递凭据脱敏（B6）：MTA 原始应答可能带**服务器指纹 / 队列信息 / 主机名**
/// （如 `250 OK id=1a2b (Exim 4.95 mail.example.com)`），而 `messageId` 会进信封、上送
/// 总线，故只留**队列号**或整段截断：
/// - 先剥 CR/LF（应答可能是多行 `join(" ")` 出来的）；
/// - 含 `queued as <id>`（Postfix 系）→ 只取那个 token（去尾部标点）；
/// - 否则整段截断到 [`MESSAGE_ID_MAX`]（FileTransport 的落盘 id 无空格，原样通过）。
///
/// 诚实边界：非 Postfix 应答在截断后仍可能带上少量服务端文本（≤ 64 字符）。
fn sanitize_message_id(raw: &str) -> String {
    let one_line: String = raw.chars().filter(|c| *c != '\r' && *c != '\n').collect();
    let s = one_line.trim();
    let lower = s.to_ascii_lowercase();
    if let Some(i) = lower.find("queued as")
        && let Some(tok) = s[i + "queued as".len()..].split_whitespace().next()
    {
        let tok =
            tok.trim_matches(|c: char| matches!(c, '.' | ',' | ';' | ':' | '>' | '<' | ')' | '('));
        if !tok.is_empty() {
            return tok.chars().take(MESSAGE_ID_MAX).collect();
        }
    }
    s.chars().take(MESSAGE_ID_MAX).collect()
}

/// `enqueue` 语义的**立即**回执：与 `send` 路**同形**的统一信封
/// `{code:0,msg:"ok",data:{jobId}}`（无 `messageId` —— 投递尚未发生，故不写该键）。
///
/// 刻意**不**回裸 `{jobId}`：三层公开契约一致承诺 `res.data.jobId`
/// （`sample/global.d.ts`、`docs/devkit/api-manual.md`、`docs/mail-smtp.md` §6）；
/// 裸形态会让用户按手册写 `res.data.jobId` 直接 TypeError，且拿不到 jobId 就无法
/// `mail.result()` 回查。
fn enqueued_envelope(job_id: &str) -> String {
    json!({"code": CODE_OK, "msg": "ok", "data": {"jobId": job_id}}).to_string()
}

/// 失败信封：**只回分类文案 + jobId**（脱敏：无收件人/主题/SMTP 对话）。
fn fail_envelope(job_id: &str, code: i32, msg: &str) -> String {
    json!({"code": code, "msg": msg, "data": {"jobId": job_id}}).to_string()
}

/// 背压信封（design §10：队列满 → `code:4`，`submit` 立即返回，不冻结调用方）。
/// 文案与全链其余信封统一为中文（A6），并给下一步（`code:4` 语义不变）。
fn queue_full_envelope(job_id: &str) -> String {
    fail_envelope(
        job_id,
        CODE_QUEUE_FULL,
        "mail: 队列已满（背压：不阻塞调用方）（下一步：稍后重试，或调大 smtp.queue_capacity）",
    )
}

/// 未知 profile 的文案（submit 期 fast-fail 与 worker 兜底**共用一处**，不写第二份判断）。
fn unknown_profile_msg(key: &str) -> String {
    format!("mail: 未知 smtp profile '{key}'（不回落 default）")
}

/// **兜底** jobId（正常路恒由宿主注入 `req.jobId`，见 B3）：`<每进程随机 64 bit>-<单调计数>`。
///
/// 旧形态 `{pid}-{seq}` 可枚举（进程号 + 从 1 起的计数）——若让调用方带上它去查结果，
/// 就等于把结果通道开放给猜测。宿主侧已改为随机前缀（`src/bridge/mail.rs::next_job_id`），
/// 这里同款兜底：随机源取 std 的 `RandomState`（每进程随机种子，哈希 time+pid+seq），
/// **不引新依赖**。
fn next_job_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    static SEQ: AtomicU64 = AtomicU64::new(1);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(seq);
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    h.write_u32(std::process::id());
    format!("{:016x}-{seq}", h.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{drive, temp_dir};
    use oj_plugin_ffi::{RBytes, RString};
    use serde_json::Value;
    use tokio::sync::Semaphore;

    const PROFILE: &str = "default";

    /// B6：`messageId` 只留队列号或截断 —— MTA 原始应答可能带服务器指纹/队列信息，
    /// 而它会进信封并上送总线（脱敏面）。
    #[test]
    fn message_id_keeps_queue_id_only_or_truncates() {
        // Postfix 系：只留 `queued as` 之后的队列号（去尾部标点）。
        assert_eq!(
            sanitize_message_id("250 2.0.0 Ok: queued as 4Wx1AbC\r\n"),
            "4Wx1AbC"
        );
        assert_eq!(sanitize_message_id("250 OK queued as ABC123."), "ABC123");
        // FileTransport：落盘 id 原样（无空格、短）。
        assert_eq!(sanitize_message_id("mail-9f8a1b"), "mail-9f8a1b");
        // 其它应答：剥 CR/LF 并截断到 64。
        let long = format!("250 {}", "x".repeat(200));
        let got = sanitize_message_id(&long);
        assert_eq!(got.len(), MESSAGE_ID_MAX, "{got}");
        assert!(!got.contains('\n') && !got.contains('\r'), "{got}");
        assert_eq!(sanitize_message_id("250\r\nOK\r\n"), "250OK");
    }

    /// B2 纵深防御：附件超限在**插件侧**也回 `code:5`（宿主是主判据；这一层防第三方宿主
    /// 漏判），且**不占队列槽位**（超限信不落盘 = 没进 worker）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn submit_rejects_oversized_attachments_with_code5() {
        let dir = temp_dir("engine-attlimit");
        let cfg = MailConfig::parse(&format!(
            r#"{{"max_attachment_bytes":8,"max_total_attachment_bytes":12,
                 "default":{{"host":"localhost","port":25,"tls":"none","allow_none_tls":true,
                             "mechanism":"login","file_transport":"{}"}}}}"#,
            dir.display()
        ))
        .expect("cfg");
        let eng = MailEngine::new(&cfg, DeliverSink::new(|_, _| {})).expect("引擎");

        // 单附件超限（9 > 8）。
        let mut fut = eng.submit(
            PROFILE,
            &req_assemble("j-big", true),
            vec![att_bytes("a.bin", "application/octet-stream", b"123456789")],
        );
        let v: Value = serde_json::from_slice(&drive(&mut fut).await.expect("应回信封")).unwrap();
        assert_eq!(v["code"], CODE_VALIDATION, "信封: {v}");
        assert!(
            v["msg"]
                .as_str()
                .unwrap_or_default()
                .contains("max_attachment_bytes"),
            "文案须给配置键与下一步: {v}"
        );

        // 合计超限（两件各 8 字节 = 16 > 12）。
        let req2 = json!({
            "from": "from@example.com",
            "to": ["to@example.com"],
            "text": "hi",
            "jobId": "j-big-2",
            "attachments": [
                {"filename": "a.bin", "blobKey": "k1"},
                {"filename": "b.bin", "blobKey": "k2"},
            ],
        })
        .to_string();
        let mut fut = eng.submit(
            PROFILE,
            &req2,
            vec![
                att_bytes("a.bin", "application/octet-stream", b"12345678"),
                att_bytes("b.bin", "application/octet-stream", b"12345678"),
            ],
        );
        let v: Value = serde_json::from_slice(&drive(&mut fut).await.expect("应回信封")).unwrap();
        assert_eq!(v["code"], CODE_VALIDATION, "信封: {v}");
        assert!(
            v["msg"]
                .as_str()
                .unwrap_or_default()
                .contains("max_total_attachment_bytes"),
            "{v}"
        );

        // 未投递（file transport 目录里没有 .eml）。
        assert!(
            std::fs::read_dir(&dir).expect("目录").next().is_none(),
            "超限附件不得进 worker（不落盘）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// B3 兜底：未给 `jobId` 时引擎自造的 id 也不得是可枚举的 `{pid}-{seq}`
    /// （正常路宿主恒注入 id，故这只是纵深防御；这里钉死形态与唯一性）。
    #[test]
    fn fallback_job_id_is_not_pid_enumerable() {
        let a = next_job_id();
        let b = next_job_id();
        assert_ne!(a, b, "进程内必须唯一");
        let (prefix, seq) = a.rsplit_once('-').expect("形态 <hex>-<seq>");
        assert_eq!(prefix.len(), 16, "{a}");
        assert!(prefix.chars().all(|c| c.is_ascii_hexdigit()), "{a}");
        assert!(seq.parse::<u64>().is_ok(), "{a}");
        assert!(
            !a.starts_with(&format!("{}-", std::process::id())),
            "不得是可枚举的 pid 前缀：{a}"
        );
    }

    /// `raw` 路的最小合法 req（信封 + 原文）。
    fn req_raw(job_id: Option<&str>, enqueue_only: bool) -> String {
        let mut v = json!({
            "from": "from@example.com",
            "to": ["to@example.com"],
            "raw": "Subject: t\r\n\r\nbody",
            "enqueue_only": enqueue_only,
        });
        if let Some(id) = job_id {
            v["jobId"] = json!(id);
        }
        v.to_string()
    }

    /// 结构化组装路的最小合法 req（**无** `raw`）；`attachment` 决定是否声明附件引用。
    fn req_assemble(job_id: &str, attachment: bool) -> String {
        let mut v = json!({
            "from": "from@example.com",
            "to": ["to@example.com"],
            "subject": "组装主题",
            "text": "hi",
            "html": "<b>hi</b>",
            "jobId": job_id,
        });
        if attachment {
            v["attachments"] = json!([{ "filename": "a.pdf", "blobKey": "k1" }]);
        }
        v.to_string()
    }

    /// 宿主解析结果形态的附件（原始字节，非 base64）。
    fn att_bytes(filename: &str, mime: &str, bytes: &[u8]) -> MailAttachment {
        MailAttachment {
            filename: RString::from(filename),
            mime: RString::from(mime),
            bytes: RBytes::from(bytes),
        }
    }

    /// file transport 引擎（免网络；`.eml` 落到 `dir`）。
    fn file_engine(dir: &std::path::Path) -> MailEngine {
        let cfg = MailConfig::parse(&format!(
            r#"{{"workers":1,"queue_capacity":4,"default":{{"host":"localhost","port":25,"tls":"none","allow_none_tls":true,"mechanism":"login","file_transport":"{}"}}}}"#,
            dir.display()
        ))
        .expect("cfg");
        MailEngine::new(&cfg, DeliverSink::new(|_, _| {})).expect("引擎")
    }

    /// 经队列真投递一封，读回 file transport 落盘的 `.eml` 原文（`messageId` = 文件名主干）。
    async fn submit_and_read_eml(
        eng: &MailEngine,
        req: &str,
        atts: Vec<MailAttachment>,
        dir: &std::path::Path,
    ) -> String {
        let mut fut = eng.submit(PROFILE, req, atts);
        let out = drive(&mut fut).await.expect("submit 必须回结果信封");
        let v: Value = serde_json::from_slice(&out).expect("信封是 JSON");
        assert_eq!(v["code"], CODE_OK, "信封: {v}");
        let id = v["data"]["messageId"].as_str().expect("messageId");
        let eml = dir.join(format!("{id}.eml"));
        std::fs::read_to_string(&eml).unwrap_or_else(|e| panic!("读 {eml:?} 失败: {e}"))
    }

    /// 投递一封并回其失败信封（`code != 0` 的用例）。
    async fn submit_expect_fail(eng: &MailEngine, req: &str, atts: Vec<MailAttachment>) -> Value {
        let mut fut = eng.submit(PROFILE, req, atts);
        let out = drive(&mut fut)
            .await
            .expect("校验失败也须回信封（不是 FFI Err）");
        let v: Value = serde_json::from_slice(&out).expect("信封是 JSON");
        assert_ne!(v["code"], CODE_OK, "必须失败，实际: {v}");
        v
    }

    fn targets_with(t: MailTarget) -> HashMap<String, MailTarget> {
        HashMap::from([(PROFILE.to_string(), t)])
    }

    /// 立即成功的注入桩（不连网）。
    fn ok_target(timeout: Duration) -> MailTarget {
        MailTarget::async_only(
            timeout,
            Arc::new(|_e, _r| Box::pin(async { Ok("stub-message-id".to_string()) })),
        )
    }

    /// 收集器 sink：enqueue 路的结果上送可在此断言（无需真 HostContext）。
    fn collector() -> (DeliverSink, mpsc::UnboundedReceiver<(String, Vec<u8>)>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let sink = DeliverSink::new(move |topic: &str, payload: &[u8]| {
            let _ = tx.send((topic.to_string(), payload.to_vec()));
        });
        (sink, rx)
    }

    /// 可控「已开始投递」信号 + 闸门（0 许可）：投递函数在闸门放行前不返回 —— 完全确定，
    /// 不依赖 sleep / 网络 / 真实 SMTP。
    fn gated_target(
        timeout: Duration,
    ) -> (MailTarget, Arc<Semaphore>, mpsc::UnboundedReceiver<()>) {
        let gate = Arc::new(Semaphore::new(0));
        let (started_tx, started_rx) = mpsc::unbounded_channel();
        let g = Arc::clone(&gate);
        let target = MailTarget::async_only(
            timeout,
            Arc::new(move |_e, _r| {
                let g = Arc::clone(&g);
                let started = started_tx.clone();
                Box::pin(async move {
                    let _ = started.send(()); // 通知测试：worker 已取走 job 并进入投递
                    let permit = g.acquire().await.map_err(|e| format!("gate closed: {e}"))?;
                    permit.forget();
                    Ok("gated-message-id".to_string())
                })
            }),
        );
        (target, gate, started_rx)
    }

    /// TDD-1：普通 `submit` 经**有界队列 + worker** 真投递，future resolve 出 `code:0` 信封。
    /// 用 file transport profile：断言信封 `messageId` 与落盘 `.eml` 对得上——证明「经队列
    /// 真投递」而非短路成功。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn submit_through_queue_resolves_with_envelope() {
        let dir = temp_dir("engine-queue");
        let cfg = MailConfig::parse(&format!(
            r#"{{"workers":2,"queue_capacity":8,"default":{{"host":"localhost","port":25,"tls":"none","allow_none_tls":true,"mechanism":"login","file_transport":"{}"}}}}"#,
            dir.display()
        ))
        .expect("cfg");
        let eng = MailEngine::new(&cfg, DeliverSink::new(|_, _| {})).expect("引擎");

        let mut fut = eng.submit(PROFILE, &req_raw(Some("j-queue-1"), false), vec![]);
        let out = drive(&mut fut).await.expect("submit 必须回结果信封");
        let v: Value = serde_json::from_slice(&out).expect("信封是 JSON");
        assert_eq!(v["code"], CODE_OK, "信封: {v}");
        assert_eq!(v["data"]["jobId"], "j-queue-1");

        let message_id = v["data"]["messageId"].as_str().expect("messageId");
        assert!(
            dir.join(format!("{message_id}.eml")).exists(),
            "信封里的 messageId 必须对应真落盘的 .eml（证明真投递）"
        );
    }

    /// TDD-2：背压。worker 被闸门挂住 + 队列容量 1 → 第三个 `submit` **立即** `code:4`
    /// （`try_send` 而非 `await send`，不冻结调用方）。
    #[tokio::test(flavor = "multi_thread")]
    async fn full_queue_returns_code4_without_blocking() {
        let (target, gate, mut started) = gated_target(Duration::from_secs(60));
        let (sink, mut delivered) = collector();
        let eng = MailEngine::with_targets(targets_with(target), 1, 1, sink).expect("引擎");

        // A：占住唯一的 worker（等 `started` 才继续 ⇒ 此刻 worker 已把 A 取走，队列为空）。
        let _ = eng.submit(PROFILE, &req_raw(Some("j-a"), true), vec![]);
        started.recv().await.expect("worker 应已开始投递 A");
        // B：占住队列唯一槽位。
        let _ = eng.submit(PROFILE, &req_raw(Some("j-b"), true), vec![]);
        assert_eq!(gate.available_permits(), 0);

        // C：队列已满 → 立即回 code:4（此处只测 `submit` 调用本身是否阻塞）。
        let t0 = Instant::now();
        let mut fut = eng.submit(PROFILE, &req_raw(Some("j-c"), false), vec![]);
        let call_elapsed = t0.elapsed();
        assert!(
            call_elapsed < Duration::from_secs(2),
            "submit 必须非阻塞返回（闸门未放行也不得等待）：{call_elapsed:?}"
        );
        let v: Value = serde_json::from_slice(&drive(&mut fut).await.expect("应回信封")).unwrap();
        assert_eq!(v["code"], CODE_QUEUE_FULL, "信封: {v}");
        assert_eq!(v["data"]["jobId"], "j-c");
        // A6：文案与全链统一为中文（不再回英文 "queue full"），且指出下一步。
        let msg = v["msg"].as_str().unwrap_or_default();
        assert!(
            msg.contains("队列已满") && msg.contains("下一步"),
            "背压文案须中文且给下一步：{v}"
        );

        // 放行 A/B：队列被消费完 → drain 成功（也证明 A/B 确实在队列/在途，而非被丢弃）。
        gate.add_permits(2);
        assert!(eng.drain(Duration::from_secs(5)).is_ok(), "drain 应成功");
        assert_eq!(
            delivered.try_recv().expect("A 的结果上送").0,
            TOPIC_MAIL_RESULT
        );
        assert_eq!(
            delivered.try_recv().expect("B 的结果上送").0,
            TOPIC_MAIL_RESULT
        );
    }

    /// TDD-3：`enqueue_only` 立即回**统一信封** `{code:0,msg:"ok",data:{jobId}}`（**非**裸
    /// `{jobId}` —— 三层公开契约都承诺 `res.data.jobId`）；真实完成经 sink 上送 `mail.result`
    /// （信封含**同** jobId、code 0，且**不含**收件人/主题）。
    #[tokio::test(flavor = "multi_thread")]
    async fn enqueue_delivers_completion_via_sink() {
        let (sink, mut delivered) = collector();
        let eng =
            MailEngine::with_targets(targets_with(ok_target(Duration::from_secs(5))), 1, 8, sink)
                .expect("引擎");

        let mut fut = eng.submit(PROFILE, &req_raw(Some("j-enq-1"), true), vec![]);
        let v: Value =
            serde_json::from_slice(&drive(&mut fut).await.expect("enqueue 回统一信封")).unwrap();
        // 严格形态：顶层只有 code/msg/data，data 只有 jobId（**没有**裸 `jobId` 顶层键）。
        let mut top: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        top.sort_unstable();
        assert_eq!(top, ["code", "data", "msg"], "enqueue 必须是统一信封: {v}");
        assert_eq!(v["code"], CODE_OK, "enqueue 立即回执: {v}");
        assert_eq!(v["msg"], "ok", "{v}");
        assert_eq!(v["data"]["jobId"], "j-enq-1", "enqueue 立即回 jobId: {v}");
        assert!(
            v["data"].get("messageId").is_none(),
            "投递尚未发生，不得写 messageId: {v}"
        );

        let (topic, payload) = tokio::time::timeout(Duration::from_secs(5), delivered.recv())
            .await
            .expect("worker 完成后应上送结果")
            .expect("sink 未关闭");
        assert_eq!(topic, TOPIC_MAIL_RESULT);
        let v: Value = serde_json::from_slice(&payload).expect("上送是 JSON 信封");
        assert_eq!(v["code"], CODE_OK, "上送信封: {v}");
        assert_eq!(v["data"]["jobId"], "j-enq-1", "须带同一个 jobId: {v}");

        let text = String::from_utf8(payload).expect("UTF-8");
        for banned in ["to@example.com", "Subject", "subject"] {
            assert!(
                !text.contains(banned),
                "结果上送不得含收件人/主题（{banned}）: {text}"
            );
        }
        assert!(eng.drain(Duration::from_secs(5)).is_ok());
    }

    /// TDD-4：graceful drain —— 入队后立刻 `drain`，**在途 job 必须跑完**（结果上送 +
    /// 全部 worker 退出），且全程无 panic / 无「no reactor running」类 abort
    /// （transport 与 runtime 的销毁顺序见 `dispose`）。
    #[tokio::test(flavor = "multi_thread")]
    async fn drain_waits_inflight_then_stops() {
        let (sink, mut delivered) = collector();
        // 在途 job 需 50ms 才完成：drain 必须等它，而不是直接扔下 runtime。
        let slow = MailTarget::async_only(
            Duration::from_secs(5),
            Arc::new(|_e, _r| {
                Box::pin(async {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    Ok("drained-message-id".to_string())
                })
            }),
        );
        let eng = MailEngine::with_targets(targets_with(slow), 2, 8, sink).expect("引擎");

        let _ = eng.submit(PROFILE, &req_raw(Some("j-drain"), true), vec![]);
        eng.drain(Duration::from_secs(10))
            .expect("drain 应在超时内完成且 worker 全部退出");

        // drain 成功 ⇒ 在途 job 已完成并上送（上送发生在 worker 退出之前）。
        let (topic, payload) = delivered.try_recv().expect("在途 job 的结果应在停机前上送");
        assert_eq!(topic, TOPIC_MAIL_RESULT);
        let v: Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(v["code"], CODE_OK, "信封: {v}");
        assert_eq!(v["data"]["jobId"], "j-drain");

        // 停机后 submit 必须 fail-loud（不再入队）。
        let mut fut = eng.submit(PROFILE, &req_raw(Some("j-after"), false), vec![]);
        assert!(
            drive(&mut fut).await.is_err(),
            "停机后 submit 必须报错（不静默丢弃）"
        );
        // 此处 drop 引擎（rt/transport 已在 drain 中销毁）——不得 panic。
    }

    /// 控制报文 `{"__ctl":"drain","timeout_ms":N}` 的 req 形态。
    fn ctl_drain_req(timeout_ms: u64) -> String {
        json!({ CTL_KEY: CTL_DRAIN, "timeout_ms": timeout_ms }).to_string()
    }

    /// A4：**控制报文** drain（生产停机的唯一入口）—— 插件引擎是 `OnceLock` 单例（只给
    /// `&`，`drain` 取不到 `&mut`），故 `submit({"__ctl":"drain"})` 必须能触发 graceful drain：
    /// 等在途 job 跑完（结果照常上送）→ 回 `{code:0,data:{drained:true}}` → 此后 submit
    /// fail-loud。ctl 路**同步**完成（drain 在 `submit` 内跑完才返回 ready future）。
    #[tokio::test(flavor = "multi_thread")]
    async fn ctl_drain_waits_inflight_then_refuses_new_jobs() {
        let (target, gate, mut started) = gated_target(Duration::from_secs(5));
        let (sink, mut delivered) = collector();
        let eng =
            Arc::new(MailEngine::with_targets(targets_with(target), 2, 4, sink).expect("引擎"));

        // 在途 job：worker 已取件并卡在闸门上（enqueue 语义 ⇒ 不等结果）。
        let _ = eng.submit(PROFILE, &req_raw(Some("j-ctl"), true), vec![]);
        started.recv().await.expect("worker 应已进入投递");

        // 控制报文在 blocking 池里发（drain 是同步等待，不占 async 线程）。
        let e = Arc::clone(&eng);
        let handle = tokio::task::spawn_blocking(move || {
            let fut = e.submit("", &ctl_drain_req(5_000), vec![]);
            // ctl 路同步完成：首次 poll 即就绪（drain 已在 submit 内跑完）。
            let code = (fut.poll)(fut.state);
            assert_eq!(code, 1, "ctl drain 的 future 应即刻就绪（drain 同步完成）");
            let taken: Result<RBytes, RString> = std::result::Result::from((fut.take)(fut.state));
            (fut.free)(fut.state); // FfiFuture 无 Drop：free 由本侧显式配对
            match taken {
                Ok(b) => b.iter().copied().collect::<Vec<u8>>(),
                Err(e) => panic!("ctl drain 回了 Err: {}", &e[..]),
            }
        });

        // 放行在途 job：drain 必须等它跑完（而不是超时丢件）。
        tokio::time::sleep(Duration::from_millis(150)).await;
        gate.add_permits(1);
        let out = handle.await.expect("drain 线程不得 panic");
        let v: Value = serde_json::from_slice(&out).expect("ctl drain 回信封");
        assert_eq!(v["code"], CODE_OK, "ctl drain 信封: {v}");
        assert_eq!(v["data"]["drained"], true, "{v}");
        assert_eq!(v["data"]["workers"], 2, "{v}");

        // 在途 job 真跑完并上送了（drain 等到了，不是丢件）。
        let (topic, payload) = delivered
            .try_recv()
            .expect("在途 job 的结果应在 drain 前上送");
        assert_eq!(topic, TOPIC_MAIL_RESULT, "{payload:?}");
        // 停机后一律 fail-loud：普通投递与再次 drain 都不再入队。
        let mut fut = eng.submit(PROFILE, &req_raw(Some("j-after-ctl"), false), vec![]);
        let e = drive(&mut fut).await.expect_err("停机后 submit 必须报错");
        assert!(e.contains("停机"), "{e}");
    }

    /// 控制报文解析：未知 `__ctl` fail-loud（**不**静默当业务 req）；无 `__ctl` 的 req 走业务路
    /// （故「非法 JSON」与「未知 profile」的错误文案仍来自业务路）。
    #[tokio::test(flavor = "multi_thread")]
    async fn ctl_unknown_name_fails_loud_and_plain_req_takes_business_path() {
        let eng = MailEngine::with_targets(
            targets_with(ok_target(Duration::from_secs(1))),
            1,
            4,
            DeliverSink::new(|_, _| {}),
        )
        .expect("引擎");

        let mut fut = eng.submit(PROFILE, &json!({ CTL_KEY: "nope" }).to_string(), vec![]);
        let e = drive(&mut fut).await.expect_err("未知控制报文必须 Err");
        assert!(e.contains("nope") && e.contains(CTL_DRAIN), "{e}");

        // 无 `__ctl` ⇒ 业务路（未知 profile 的既有文案，不被控制报文分支吞掉）。
        let mut fut = eng.submit("no-such", &req_raw(Some("j-plain"), false), vec![]);
        let e = drive(&mut fut).await.expect_err("未知 profile 仍报业务错");
        assert!(e.contains("no-such"), "{e}");
        assert!(eng.drain(Duration::from_secs(2)).is_ok());
    }

    /// TDD-5：未知 profile 必须 fail-loud（不回落 default）。
    #[tokio::test(flavor = "multi_thread")]
    async fn unknown_key_returns_error() {
        let eng = MailEngine::with_targets(
            targets_with(ok_target(Duration::from_secs(1))),
            1,
            4,
            DeliverSink::new(|_, _| {}),
        )
        .expect("引擎");
        let mut fut = eng.submit("typo", &req_raw(Some("j-x"), false), vec![]);
        let e = drive(&mut fut)
            .await
            .expect_err("未知 profile 必须 Err（不回落 default）");
        assert!(e.contains("typo"), "错误须含 key，便于定位: {e}");
        // 未入队（fast-fail）⇒ 队列空，drain 立即可完成。
        assert!(eng.drain(Duration::from_secs(2)).is_ok());
    }

    /// TDD-5b：worker 侧的未知 key 兜底分支（submit 期已 fast-fail；直调内部函数覆盖）。
    #[tokio::test(flavor = "multi_thread")]
    async fn worker_reports_unknown_key_as_validation_envelope() {
        let job = Job {
            key: "nope".to_string(),
            req: req_raw(Some("j-兜底"), false),
            atts: Vec::new(),
            respond: None,
            enqueue_only: false,
            job_id: "j-兜底".to_string(),
            sync: false,
        };
        let env = deliver_one(&job, &HashMap::new()).await;
        let v: Value = serde_json::from_slice(env.as_bytes()).unwrap();
        assert_eq!(v["code"], CODE_VALIDATION, "信封: {v}");
        assert_eq!(v["data"]["jobId"], "j-兜底");
    }

    // ---- 阶段 5：两条投递路（结构化组装 / raw 原文）----

    /// 结构化组装路：text+html+附件 → `multipart/alternative`（备选体）套在 `mixed` 里，
    /// 附件名/类型来自请求与宿主，**字节来自 vtable `atts` 且不被 base64 化**（ASCII 内容
    /// 走 7bit）。断言读的是**真落盘的 `.eml`**，证明组装结果确实过了队列与 transport。
    #[tokio::test(flavor = "multi_thread")]
    async fn assemble_path_sends_multipart_with_host_resolved_attachment_bytes() {
        let dir = temp_dir("engine-assemble");
        let eng = file_engine(&dir);
        let atts = vec![att_bytes("a.pdf", "application/pdf", b"%PDF-1.4")];
        let eml = submit_and_read_eml(&eng, &req_assemble("j-asm-1", true), atts, &dir).await;

        assert!(eml.contains("multipart/alternative"), "备选体缺失: {eml}");
        assert!(
            eml.contains("multipart/mixed"),
            "有附件应有 mixed 外层: {eml}"
        );
        assert!(eml.contains("a.pdf"), "附件名缺失: {eml}");
        assert!(eml.contains("application/pdf"), "附件类型缺失: {eml}");
        assert!(
            !eml.contains("JVBERi0xLjQ"),
            "附件字节不得被 base64 化成文本流: {eml}"
        );
        assert!(
            eml.contains("hi") && eml.contains("<b>hi</b>"),
            "两版正文都在: {eml}"
        );
        assert!(
            eml.contains("To: to@example.com"),
            "信封/报头应对齐 to: {eml}"
        );
    }

    /// raw 路（**结构化为空**）：原文 `From`/`To` 被剥离、报头由结构化信封重建（防双收件人 /
    /// 发件人 spoof），而原文 `Subject` **保留**（决策：Subject 非信封字段，剥它只会丢主题）。
    /// 以**落盘 `.eml`** 为证。
    #[tokio::test(flavor = "multi_thread")]
    async fn raw_path_drops_envelope_headers_but_keeps_raw_subject() {
        let dir = temp_dir("engine-raw");
        let eng = file_engine(&dir);
        let req = json!({
            "from": "from@example.com",
            "to": ["to@example.com"],
            "jobId": "j-raw-1",
            "raw": "From: evil@x\nTo: victim@x\nSubject: raw-sub\nX-Keep: 1\n\nbody",
        })
        .to_string();
        let eml = submit_and_read_eml(&eng, &req, vec![], &dir).await;

        assert!(
            eml.contains("X-Keep: 1") && eml.contains("body"),
            "其余头与正文保留: {eml}"
        );
        assert!(
            !eml.contains("evil@x") && !eml.contains("victim@x"),
            "原文信封头已剥离: {eml}"
        );
        assert!(
            eml.contains("Subject: raw-sub"),
            "结构化为空时原文 Subject 必须保留: {eml}"
        );
        assert_eq!(
            eml.to_lowercase().matches("subject:").count(),
            1,
            "只能有一个 Subject 头: {eml}"
        );
        assert!(
            eml.contains("From: from@example.com") && eml.contains("To: to@example.com"),
            "报头由结构化信封重建: {eml}"
        );
    }

    /// raw 路（**结构化 `subject` 非空**）：以结构化值为准覆盖原文 Subject —— 落盘 `.eml` 里
    /// 只有一个 Subject 头，且原文那一个（连同其值）不残留。非 ASCII 主题走 RFC2047 编码。
    #[tokio::test(flavor = "multi_thread")]
    async fn raw_path_structured_subject_overrides_raw_subject() {
        let dir = temp_dir("engine-raw-subject");
        let eng = file_engine(&dir);

        // 1) ASCII 结构化主题：可逐字断言（不必解 RFC2047）
        let req = json!({
            "from": "from@example.com",
            "to": ["to@example.com"],
            "jobId": "j-raw-sub-1",
            "subject": "struct-sub",
            "raw": "Subject: raw-sub\n  folded-leak\nX-Keep: 1\n\nbody",
        })
        .to_string();
        let eml = submit_and_read_eml(&eng, &req, vec![], &dir).await;
        assert!(eml.contains("Subject: struct-sub"), "结构化主题生效: {eml}");
        assert!(
            !eml.contains("raw-sub") && !eml.contains("folded-leak"),
            "原文 Subject 及其折行续行不得残留: {eml}"
        );
        assert_eq!(
            eml.to_lowercase().matches("subject:").count(),
            1,
            "最终只能有一个 Subject 头: {eml}"
        );
        assert!(eml.contains("X-Keep: 1") && eml.contains("body"), "{eml}");

        // 2) 非 ASCII 结构化主题：lettre 做 RFC2047 编码（原文 Subject 同样不残留）
        let req = json!({
            "from": "from@example.com",
            "to": ["to@example.com"],
            "jobId": "j-raw-sub-2",
            "subject": "结构主题",
            "raw": "Subject: raw-sub\n\nbody",
        })
        .to_string();
        let eml = submit_and_read_eml(&eng, &req, vec![], &dir).await;
        assert!(
            eml.contains("Subject: =?utf-8?") && !eml.contains("raw-sub"),
            "非 ASCII 主题应 RFC2047 编码且覆盖原文: {eml}"
        );
    }

    /// 附件下标对齐失败（请求声明 N 个、宿主解析出 M 个）必须 `code:5` fail-loud，
    /// 绝不静默把没字节的附件发出去（`message.rs` 的对齐契约）。
    #[tokio::test(flavor = "multi_thread")]
    async fn attachment_count_mismatch_returns_code5() {
        let (sink, _delivered) = collector();
        let eng =
            MailEngine::with_targets(targets_with(ok_target(Duration::from_secs(5))), 1, 4, sink)
                .expect("引擎");
        let v = submit_expect_fail(&eng, &req_assemble("j-mismatch", true), vec![]).await;
        assert_eq!(v["code"], CODE_VALIDATION, "信封: {v}");
        assert!(
            v["msg"].as_str().unwrap().contains("附件"),
            "msg 须点明附件对齐失败: {v}"
        );
        assert_eq!(v["data"]["jobId"], "j-mismatch");
        assert!(eng.drain(Duration::from_secs(5)).is_ok());
    }

    /// 无 `raw` 且无正文 → `code:5`（**替换**阶段 4 的「缺 raw 未实现组装」）：不许发空信。
    #[tokio::test(flavor = "multi_thread")]
    async fn assemble_path_without_body_returns_code5() {
        let (sink, _delivered) = collector();
        let eng =
            MailEngine::with_targets(targets_with(ok_target(Duration::from_secs(5))), 1, 4, sink)
                .expect("引擎");
        let req = json!({"from": "f@example.com", "to": ["t@example.com"], "jobId": "j-nobody"})
            .to_string();
        let v = submit_expect_fail(&eng, &req, vec![]).await;
        assert_eq!(v["code"], CODE_VALIDATION, "信封: {v}");
        let msg = v["msg"].as_str().unwrap();
        assert!(
            msg.contains("text") && msg.contains("html"),
            "msg 须给下一步: {v}"
        );
        assert!(eng.drain(Duration::from_secs(5)).is_ok());
    }

    /// raw 路的结构化 `cc`/`bcc` 无对应报头（原文里的 Cc/Bcc 头会被剥离）→ 必须 fail-loud
    /// 而不是静默丢件 / 把 Bcc 塞进 To 头；raw 与附件同样互斥（vtable 契约）。
    #[tokio::test(flavor = "multi_thread")]
    async fn raw_path_rejects_cc_bcc_and_attachments() {
        let (sink, _delivered) = collector();
        let eng =
            MailEngine::with_targets(targets_with(ok_target(Duration::from_secs(5))), 1, 8, sink)
                .expect("引擎");

        for extra in [
            json!({"cc": ["c@example.com"]}),
            json!({"bcc": ["b@example.com"]}),
        ] {
            let mut v = json!({
                "from": "f@example.com",
                "to": ["t@example.com"],
                "raw": "X-Keep: 1\n\nbody",
                "jobId": "j-raw-cc",
            });
            for (k, val) in extra.as_object().unwrap() {
                v[k] = val.clone();
            }
            let env = submit_expect_fail(&eng, &v.to_string(), vec![]).await;
            assert_eq!(env["code"], CODE_VALIDATION, "信封: {env}");
            assert!(
                env["msg"].as_str().unwrap().contains("cc/bcc"),
                "信封: {env}"
            );
        }

        // raw + 声明附件（宿主按契约不解析 → 走 refs 校验）与 raw + 已解析字节，两者都拒。
        let mut v = json!({
            "from": "f@example.com",
            "to": ["t@example.com"],
            "raw": "X-Keep: 1\n\nbody",
            "jobId": "j-raw-att",
            "attachments": [{"filename": "a.pdf", "blobKey": "k"}],
        });
        let env = submit_expect_fail(&eng, &v.to_string(), vec![]).await;
        assert!(env["msg"].as_str().unwrap().contains("附件"), "信封: {env}");

        v["attachments"] = json!([]);
        let env = submit_expect_fail(
            &eng,
            &v.to_string(),
            vec![att_bytes("a.pdf", "application/pdf", b"x")],
        )
        .await;
        assert!(env["msg"].as_str().unwrap().contains("附件"), "信封: {env}");

        assert!(eng.drain(Duration::from_secs(5)).is_ok());
    }

    /// 真 SMTP 路（不连网即失败）：端口 1 无监听 → `code:1`（网络/连接类），
    /// 且错误文案**不含** SMTP 对话/凭据。
    #[tokio::test(flavor = "multi_thread")]
    async fn smtp_connection_refused_returns_network_code() {
        let cfg = MailConfig::parse(
            r#"{"workers":2,"queue_capacity":4,"default":{"host":"127.0.0.1","port":1,"tls":"none","allow_none_tls":true,"mechanism":"login","timeout":5}}"#,
        )
        .expect("cfg");
        let eng = MailEngine::new(&cfg, DeliverSink::new(|_, _| {}))
            .expect("引擎（建 transport 不连网）");
        let mut fut = eng.submit(PROFILE, &req_raw(Some("j-refused"), false), vec![]);
        let v: Value = serde_json::from_slice(&drive(&mut fut).await.expect("应回信封")).unwrap();
        assert_eq!(v["code"], CODE_NETWORK, "信封: {v}");
        assert_eq!(v["data"]["jobId"], "j-refused");
        assert_eq!(v["msg"], "投递失败", "msg 须为脱敏分类文案: {v}");
    }

    /// design §10「每 job `tokio::time::timeout(profile.timeout)`；超时 `code:1`」——**异步路**：
    /// 投递挂住（闸门永不放行）→ future 在超时点 resolve `code:1`（既不无限等待，也不 panic
    /// 整个 worker），msg 为脱敏分类文案。
    #[tokio::test(flavor = "multi_thread")]
    async fn async_delivery_timeout_returns_network_code() {
        let (target, _gate, mut started) = gated_target(Duration::from_millis(50));
        let eng = MailEngine::with_targets(targets_with(target), 1, 4, DeliverSink::new(|_, _| {}))
            .expect("引擎");
        let mut fut = eng.submit(PROFILE, &req_raw(Some("j-timeout"), false), vec![]);
        // 先确认 worker **已进入投递**（否则下面测到的可能只是「还没开始」而非超时）。
        started.recv().await.expect("worker 应已进入投递");

        let v: Value =
            serde_json::from_slice(&drive(&mut fut).await.expect("超时必须回信封")).unwrap();
        assert_eq!(v["code"], CODE_NETWORK, "超时归连接/网络类: {v}");
        assert_eq!(v["data"]["jobId"], "j-timeout");
        assert_eq!(
            v["msg"], "投递超时",
            "msg 须为脱敏分类文案（无 SMTP 对话）: {v}"
        );
        assert!(
            eng.drain(Duration::from_secs(5)).is_ok(),
            "超时后 worker 应能正常 drain"
        );
    }

    /// 同步路同款（`deliver_one` 的 `spawn_blocking` 分支）：阻塞投递超过 `profile.timeout`
    /// → 同样 `code:1`。`timeout` 只停止**等待**，池中已开始的阻塞调用会跑完（诚实边界，
    /// 见 `deliver_one` 注释）——本用例末尾放行闸门，不留挂死的 blocking 线程。
    #[tokio::test(flavor = "multi_thread")]
    async fn sync_delivery_timeout_returns_network_code() {
        // 阻塞闸门：`Mutex<Receiver>` 同时满足 Send + Sync（`Receiver` 只 Send）。
        let (release, blocked) = std::sync::mpsc::channel::<()>();
        let blocked = Mutex::new(blocked);
        let target = MailTarget::new(
            Duration::from_millis(50),
            // 本用例只走同步路（req.sync = true），异步路给立即成功的桩。
            Arc::new(|_, _| Box::pin(async { Ok("unused".to_string()) })),
            Arc::new(move |_, _| {
                let _ = blocked.lock().expect("闸门锁").recv();
                Err("blocked send".to_string())
            }),
        );
        let eng = MailEngine::with_targets(targets_with(target), 1, 4, DeliverSink::new(|_, _| {}))
            .expect("引擎");
        let req = json!({
            "from": "from@example.com", "to": ["to@example.com"],
            "raw": "Subject: t\r\n\r\nbody", "sync": true, "jobId": "j-sync-timeout",
        })
        .to_string();

        let mut fut = eng.submit(PROFILE, &req, vec![]);
        let v: Value =
            serde_json::from_slice(&drive(&mut fut).await.expect("超时必须回信封")).unwrap();
        assert_eq!(v["code"], CODE_NETWORK, "同步路超时归连接/网络类: {v}");
        assert_eq!(v["data"]["jobId"], "j-sync-timeout");
        assert_eq!(v["msg"], "投递超时", "{v}");

        let _ = release.send(()); // 放行阻塞线程
        assert!(eng.drain(Duration::from_secs(5)).is_ok());
    }

    /// design §10/§11「`msg` 脱敏（无账号/密码/令牌/SMTP 对话）」+「令牌不进信封」：
    /// 带凭据的 profile 投递失败 → 信封里**不得**出现口令、令牌或用户名，且信封保持
    /// **严格白名单形态**（`{code,msg,data:{jobId}}`，msg 为分类文案、无原始诊断细节）。
    /// 凭据只在插件 cfg 里（不进 req），故信封一旦带凭据/对话即是回归。
    #[tokio::test(flavor = "multi_thread")]
    async fn failure_envelope_never_carries_credentials() {
        const PASS: &str = "S3CRET-PASS-42";
        const TOKEN: &str = "ya29.S3CRET-TOKEN-42";
        const USER: &str = "mailer@example.com";
        // 端口 1 无监听 → 真连接失败（凭据确实参与过认证路径，只是尚未发出即断）。
        let cfg = MailConfig::parse(&format!(
            r#"{{"workers":2,"queue_capacity":4,
                 "login":{{"host":"127.0.0.1","port":1,"tls":"none","allow_none_tls":true,"mechanism":"login","user":"{USER}","pass":"{PASS}","timeout":5}},
                 "oauth":{{"host":"127.0.0.1","port":1,"tls":"none","allow_none_tls":true,"mechanism":"xoauth2","user":"{USER}","xoauth2":{{"access_token":"{TOKEN}"}},"timeout":5}}}}"#
        ))
        .expect("cfg");
        let eng = MailEngine::new(&cfg, DeliverSink::new(|_, _| {})).expect("引擎");

        for (profile, job) in [("login", "j-login"), ("oauth", "j-oauth")] {
            let mut fut = eng.submit(profile, &req_raw(Some(job), false), vec![]);
            let out = drive(&mut fut).await.expect("应回信封");
            let v: Value = serde_json::from_slice(&out).expect("信封是 JSON");
            assert_eq!(v["code"], CODE_NETWORK, "{profile}: {v}");
            // 严格形态：只有 code/msg/data 三个键，data 只有 jobId —— 不得夹带诊断细节。
            // （键序无关：serde_json 的 Map 后端可能是 BTreeMap，故排序后比对。）
            let mut top: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
            top.sort_unstable();
            assert_eq!(top, ["code", "data", "msg"], "{profile}: {v}");
            let data: Vec<&str> = v["data"]
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            assert_eq!(data, ["jobId"], "{profile}: {v}");
            assert_eq!(v["msg"], "投递失败", "{profile} 的 msg 须为分类文案: {v}");

            let text = String::from_utf8(out).expect("UTF-8");
            for leaked in [PASS, TOKEN, USER, "AUTH", "LOGIN"] {
                assert!(
                    !text.contains(leaked),
                    "{profile} 的信封泄露凭据/认证对话（{leaked}）: {text}"
                );
            }
        }
    }

    /// 生产路径构造「SMTP（pool）transport」引擎：建 transport 本身**不连网**（首次 send
    /// 才连），但该 transport 的 Drop 会 `tokio::spawn`（lettre `pool/async_impl.rs:262`）。
    ///
    /// 注意：`build_profiles` **必须**在 runtime 上下文内调用（`MailEngine::new` 已 `rt.enter()`）
    /// ——阶段 4 实测：在无 runtime 上下文的同步线程上构建，构建期的临时 Pool 析构就会
    /// panic（"there is no reactor running"）。
    fn pool_engine(workers: usize) -> MailEngine {
        MailEngine::new(&pool_cfg(workers), DeliverSink::new(|_, _| {})).expect("引擎")
    }

    /// 单 profile、明文、指向本地无监听端口的配置（`tls:none` 需显式允许）。
    fn pool_cfg(workers: usize) -> MailConfig {
        MailConfig::parse(&format!(
            r#"{{"workers":{workers},"queue_capacity":4,"default":{{"host":"127.0.0.1","port":1,"tls":"none","allow_none_tls":true,"mechanism":"login","timeout":1}}}}"#
        ))
        .expect("cfg")
    }

    /// 硬约束（模块头 §2）证据 · Drop 路：在 async 上下文（`#[tokio::test]`、宿主 async 任务）
    /// 里 drop 引擎必须安全（`dispose` 用非阻塞的 `shutdown_background`）。
    ///
    /// 变异验证：把 `dispose` 的 `rt.shutdown_background()` 换成裸 `drop(rt)` → 本用例 panic
    /// （tokio `blocking/shutdown.rs:51` "Cannot drop a runtime in a context where blocking is
    /// not allowed"）。
    #[tokio::test(flavor = "multi_thread")]
    async fn dropping_engine_in_async_context_does_not_abort() {
        // 同时覆盖注入路（`build_profiles` + `into_target` + `with_targets`）：它在 async
        // 上下文里调用，故顺带证明 `build_profiles` 的「必须在 runtime 上下文内」约束。
        let mut targets = HashMap::new();
        for (k, p) in build_profiles(&pool_cfg(2)).expect("build").into_iter() {
            targets.insert(k, p.into_target());
        }
        let eng =
            MailEngine::with_targets(targets, 2, 4, DeliverSink::new(|_, _| {})).expect("引擎");
        // 不投递（无网络）；只验证销毁路径在 async 上下文里不 panic。
        drop(eng);
    }

    /// 硬约束（模块头 §1）证据 · drain 路：`drain` 后各 worker 已退出并释放自己的强引用，
    /// 于是 transport 的**最后一份强引用**在 `dispose` 里释放 —— 必须在 rt 上下文内
    /// （lettre `pool` 的 Drop 会 `tokio::spawn`）。
    ///
    /// 变异验证：去掉 `dispose` 的 `rt.enter()` → 本用例 panic（"there is no reactor running"）。
    #[tokio::test(flavor = "multi_thread")]
    async fn drain_releases_pool_transports_inside_runtime_context() {
        let eng = pool_engine(2);
        eng.drain(Duration::from_secs(5))
            .expect("空队列的 drain 应立即成功（worker 全部退出）");
        drop(eng); // rt/targets 已在 drain 内销毁；此处不得 panic
    }

    /// 同一约束的**决定性**版本：同步上下文（进程退出 / 宿主非 async 路径）里线程上
    /// **没有任何** runtime，`rt.enter()` 是唯一让 lettre pool 的 Drop 能 `tokio::spawn`
    /// 的东西。（上一条异步用例里测试自身 runtime 会「兜住」漏掉的 enter，故单靠它无法
    /// 分辨 —— 变异验证：去掉 `dispose` 里的 `rt.enter()`，本用例 panic
    /// "there is no reactor running"，异步那条仍绿。）
    #[test]
    fn drain_from_sync_context_releases_pool_transports_in_rt_context() {
        let eng = pool_engine(2);
        eng.drain(Duration::from_secs(5))
            .expect("空队列的 drain 应立即成功（worker 全部退出）");
    }

    /// A3：**在途 job** + **真 pool transport**（非注入桩）+ 两条「丢弃在途」的销毁路径
    /// （非 graceful `Drop` 与 drain 超时→`dispose`）都必须无 panic。
    ///
    /// 真 pool transport 指向「只 accept、永不应答」的本地监听：SMTP 握手挂在投递中，
    /// job 跑不完 ⇒ worker 帧（连同它捕获的 `targets` 强引用与在途投递 future）只能在
    /// runtime 停机时被丢弃。lettre `pool` 的 Drop 会 `tokio::spawn`（`pool/async_impl.rs:262`），
    /// 故「最后一份强引用的销毁点是否有 runtime 上下文」是这条路径的成败关键。
    ///
    /// **实测结论（本用例在当前依赖下是绿的，非「先红后绿」）**：tokio 1.53 在
    /// `shutdown_background` 取消未完成任务时会先进入 runtime 上下文
    /// （`OwnedTasks::shutdown` 走 `try_enter_blocking_region`，实测 `Handle::try_current()`
    /// 为 `Ok`、`tokio::spawn` 成功），故**没有**出现「无上下文析构 → panic」。
    /// 但这属于 tokio 的实现细节：worker 帧的释放点若晚于退出信号，正确性就不再由本模块
    /// 的顺序保证所决定（见 `with_rt` 的「顺序即契约」注释）——本用例因此是**回归护栏**
    /// （钉住「两条丢弃路径都不得 panic」），而非该 bug 的判定证据。
    #[test]
    fn discarding_inflight_pool_job_does_not_panic() {
        // 真监听：accept 后只持有连接、不读不写 ⇒ SMTP 问候永不到来 ⇒ 投递挂住。
        fn hanging_listener() -> u16 {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            let port = listener.local_addr().expect("addr").port();
            std::thread::spawn(move || {
                let mut held = Vec::new();
                for c in listener.incoming().flatten() {
                    held.push(c);
                }
            });
            port
        }
        fn engine_to(port: u16) -> MailEngine {
            let cfg = MailConfig::parse(&format!(
                r#"{{"workers":1,"queue_capacity":4,"default":{{"host":"127.0.0.1","port":{port},"tls":"none","allow_none_tls":true,"mechanism":"login","timeout":30}}}}"#
            ))
            .expect("cfg");
            MailEngine::new(&cfg, DeliverSink::new(|_, _| {})).expect("引擎")
        }

        // (1) 非 graceful Drop：不 drain，直接丢弃（在途 job 随 runtime 一起丢）。
        let eng = engine_to(hanging_listener());
        let _ = eng.submit(PROFILE, &req_raw(Some("j-drop"), true), vec![]);
        std::thread::sleep(Duration::from_millis(500));
        drop(eng);

        // (2) drain 超时：在途 job 挂住 ⇒ drain 到点返回 Err，随后 dispose（同样丢在途）。
        let eng = engine_to(hanging_listener());
        let _ = eng.submit(PROFILE, &req_raw(Some("j-drain-timeout"), true), vec![]);
        std::thread::sleep(Duration::from_millis(500));
        let e = eng
            .drain(Duration::from_millis(200))
            .expect_err("在途 job 挂住 ⇒ drain 必须超时报错");
        assert!(e.contains("超时"), "{e}");
        drop(eng); // rt/targets 已在 drain 内销毁；此处不得 panic
    }
}
