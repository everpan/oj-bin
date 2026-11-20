//! SidecarMgr：JVM sidecar 进程生命周期管理（design spec §5.1，插件最高危面）。
//!
//! - 惰性 spawn（首次 request）：java 解析顺序 cfg `java_bin` → `JAVA_HOME/bin/java` → PATH，
//!   全缺 → 结构化错误含搜索顺序 + 修复路径。
//! - spawn 参数必含 `-Duser.timezone=UTC -Dfile.encoding=UTF-8`（§5.4 时区口径契约）。
//! - spawn 后**同步读 hello 行**（有界等待）再放行请求：proto 严格相等，
//!   不匹配报错含两侧版本号（spec §5.8：「.so 与 jar 必须同版整目录拷贝」）。
//! - req_id 全局单调（AtomicU64），**sidecar 重启绝不归零**（spec §5.1；Java 只回显）。
//! - 三路管道：stdin 写任务（unbounded mpsc 消费，非阻塞投递）；stdout 读任务
//!   （FrameReader 解帧 → pending oneshot 分派，receiver 已 drop 的 send 失败静默）；
//!   stderr **常驻 drain 任务**（逐行进尾部环形缓冲 ~20 行供 connect 失败诊断 + 上送宿主 log）。
//! - 64MiB TooLong / 非 UTF8 / 协议 JSON 坏 / IO 错 / EOF → sidecar 判死（**世代闸**：
//!   只认发起判死的那代 JVM，迟到的旧判死不误杀健康新 JVM、不重复跳世代）：在途 pending
//!   全部投递 Err（"sidecar lost" + session_disposition=reconnect），handle 世代 +1
//!   （旧世代句柄经 [`SidecarMgr::check_generation`] 立即报 reconnect，绝不隐式落到新 JVM），
//!   下一次 request 触发重启。
//! - EOF 退出契约（§5.1）：`shutdown()` 关 stdin 通道 → JVM 有序关闭连接退出 0。

use crate::codec::{self, FrameError, FrameReader, Response};
use crate::errmap;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex as AsyncMutex, mpsc, oneshot};

/// 协议版本：与 `java/jdbc-bridge/src/main/java/app/oj/jdbc/Bridge.java` HELLO 严格相等。
pub const PROTO: u64 = 1;

/// stderr 尾部环形缓冲行数（spec §5.1：connect 失败必须附最后 ~20 行）。
const STDERR_TAIL: usize = 20;

/// shutdown 等待 EOF 有序退出的宽限（超时升级 kill；§5.1 EOF 契约的防御侧）。
const EOF_EXIT_GRACE: Duration = Duration::from_secs(5);

/// stderr 行上送宿主 tracing（level 0=trace..4=error，见 HostContext.log）。
pub type LogSink = Arc<dyn Fn(u8, &str) + Send + Sync>;

/// 装配期配置（`plugins: oj-db-jdbc:` 段透传；spec §5.2）。
pub struct SidecarCfg {
    /// cfg `java_bin` 显式指定（最高优先）。缺省经 JAVA_HOME / PATH 解析。
    pub java_bin: Option<String>,
    /// 附加 JVM 参数（如 `-Xmx512m`）。UTC/file.encoding 由本模块强制前置，不在这里给。
    pub java_opts: Vec<String>,
    /// bridge jar（classpath 第一項）。dladdr 缺省定位是 T10 的职责；T9 只透传显式值。
    pub bridge_jar: PathBuf,
    /// JDBC 驱动目录（classpath `<dir>/*`）。
    pub drivers_dir: PathBuf,
    /// hello 行有界等待上限。
    pub hello_timeout: Duration,
    /// 附加子进程环境变量（测试夹具用：OJ_FAKE_JAVA=<场景>）。
    pub env: Vec<(String, String)>,
    /// stderr 行上送宿主 tracing（level 0=trace..4=error）。None = 只进环形缓冲（测试）。
    pub log: Option<LogSink>,
}

impl Default for SidecarCfg {
    fn default() -> Self {
        Self {
            java_bin: None,
            java_opts: vec![],
            bridge_jar: PathBuf::from("oj-jdbc-bridge.jar"),
            drivers_dir: PathBuf::from("drivers"),
            hello_timeout: Duration::from_secs(10),
            env: vec![],
            log: None,
        }
    }
}

impl SidecarCfg {
    /// 解析装配期 cfg JSON（`plugins: oj-db-jdbc:` 段）。任何形态错误落缺省（fail-loud 是
    /// spawn/first-request 的事；init 期不拒绝装配）。
    pub fn from_json(s: &str) -> Self {
        let mut cfg = Self::default();
        let Ok(v) = serde_json::from_str::<Value>(s) else {
            return cfg;
        };
        let Some(o) = v.as_object() else {
            return cfg;
        };
        if let Some(j) = o.get("java_bin").and_then(Value::as_str) {
            cfg.java_bin = Some(j.to_string());
        }
        if let Some(opts) = o.get("java_opts").and_then(Value::as_array) {
            cfg.java_opts = opts
                .iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect();
        }
        if let Some(d) = o.get("drivers_dir").and_then(Value::as_str) {
            cfg.drivers_dir = PathBuf::from(d);
        }
        if let Some(b) = o.get("bridge_jar").and_then(Value::as_str) {
            cfg.bridge_jar = PathBuf::from(b);
        }
        cfg
    }
}

/// 存活中的 sidecar（writer 通道 + child 所有权 + spawn 世代）。
struct Running {
    /// stdin 写任务入口。drop（判死/关闭）→ 写任务收 None → stdin drop = EOF（§5.1 契约）。
    tx: mpsc::UnboundedSender<String>,
    child: Child,
    /// spawn 世代：判死只认发起它的那代 JVM——重启后迟到的旧判死不得杀健康新 JVM。
    epoch: u64,
}

/// 在途请求条目。
struct PendingEntry {
    /// 帧实际投递的目标世代（`Some(epoch)`）；`None` = 已入表未投递（insert 与 send
    /// 之间的窗口）。判死 drain 只清 `Some(自己的 epoch)` 条目。
    epoch: Option<u64>,
    tx: oneshot::Sender<Result<Value, String>>,
}

struct Shared {
    cfg: SidecarCfg,
    /// None = 未 spawn 或已判死（下一次 request 触发重启）。
    inner: AsyncMutex<Option<Running>>,
    /// req_id → 在途请求。std Mutex：临界区只做 map 操作，不跨 await。
    /// 已知限制（挂账）：JVM 永不响应且调用方被宿主 KillSwitch 取消时，条目留存至
    /// 下次判死 drain（与 mysql 插件在途请求同语义，非泄漏增长——map 随 drain 清空）。
    pending: Mutex<HashMap<u64, PendingEntry>>,
    stderr_tail: Mutex<VecDeque<String>>,
    /// 优雅关闭标志：判死路径跳过（EOF 是预期退出，不是协议死）。
    closing: AtomicBool,
    /// handle 世代（spec §5.1）：判死 +1。旧世代句柄经 check_generation 立即报 reconnect。
    generation: AtomicU64,
    /// spawn 世代分配：每次 spawn 唯一且单调，重启不复用（判死比对依据）。
    next_epoch: AtomicU64,
    /// req_id 全局单调分配（spec §5.1：**sidecar 重启绝不归零**；Java 只回显不分配）。
    next_req: AtomicU64,
}

/// sidecar 死亡错误信封（vtable Err 载荷约定，镜像 oj-db-mysql）：
/// `{category,retryable,session_disposition,message,data}`。session_disposition=reconnect
/// = 会话已随 JVM 消亡，调用方须重连（事务随 JVM 消亡回滚，语义安全，绝不自动重放）。
fn lost_err(why: &str) -> String {
    serde_json::json!({
        "category": "connection",
        "retryable": true,
        "session_disposition": "reconnect",
        "message": format!("sidecar lost: {why}"),
        "data": {},
    })
    .to_string()
}

pub struct SidecarMgr {
    shared: Arc<Shared>,
}

impl SidecarMgr {
    pub fn new(cfg: SidecarCfg) -> Self {
        Self {
            shared: Arc::new(Shared {
                cfg,
                inner: AsyncMutex::new(None),
                pending: Mutex::new(HashMap::new()),
                stderr_tail: Mutex::new(VecDeque::new()),
                closing: AtomicBool::new(false),
                generation: AtomicU64::new(0),
                next_epoch: AtomicU64::new(0),
                next_req: AtomicU64::new(0),
            }),
        }
    }

    /// 发送一个 op 并等待响应（T10/T11 唯一依赖面）。
    ///
    /// payload 必须是 JSON 对象（展开为请求帧参数）。惰性 spawn / 死亡重启对调用方透明，
    /// 除「旧世代句柄」外：T10 的 db handle 持世代快照，请求前过 [`Self::check_generation`]，
    /// 绝不隐式把旧 JVM 的连接号落到新 JVM 上。
    pub async fn request(&self, op: &str, payload: Value) -> Result<Value, String> {
        let params = payload
            .as_object()
            .cloned()
            .ok_or_else(|| format!("sidecar request {op}: payload must be a JSON object"))?;
        let (tx, rx) = oneshot::channel();
        let id = self.shared.next_req.fetch_add(1, Ordering::SeqCst) + 1;
        // 先入表（epoch=None = 未投递）；send 与 epoch 落记都在 inner 锁内完成——
        // 判死的 take/drain 也经同一把锁，二者互斥，杜绝「响应到达时条目不在表」。
        self.shared
            .pending
            .lock()
            .unwrap()
            .insert(id, PendingEntry { epoch: None, tx });
        let line = codec::encode_request(id, op, &params);
        // 惰性 spawn / 重启都在锁内做：并发首请求不会双重 spawn；hello 等待期间
        // 其它请求排队（它们本来也要等这条 sidecar 就绪）。
        let sent = {
            let mut g = self.shared.inner.lock().await;
            if g.is_none()
                && let Err(e) = self.spawn(&mut g).await
            {
                self.shared.pending.lock().unwrap().remove(&id);
                return Err(e);
            }
            let st = g.as_ref().expect("spawn ok ⇒ Some");
            let r = st.tx.send(line);
            if r.is_ok() {
                // 投递成功：记录目标世代（持 inner 锁写 pending，与判死 drain 互斥）。
                if let Some(p) = self.shared.pending.lock().unwrap().get_mut(&id) {
                    p.epoch = Some(st.epoch);
                }
            }
            r
        };
        if sent.is_err() {
            // 与判死竞态（writer 已随 state take 退出）：按 sidecar lost 处理。
            self.shared.pending.lock().unwrap().remove(&id);
            return Err(lost_err("stdin writer gone (sidecar restarting)"));
        }
        rx.await
            .unwrap_or_else(|_| Err(lost_err("response dispatcher gone (sidecar restarting)")))
    }

    /// 当前 handle 世代。T10 connect 时快照；db 请求前 `check_generation` 校验。
    pub fn generation(&self) -> u64 {
        self.shared.generation.load(Ordering::SeqCst)
    }

    /// 旧世代句柄（sidecar 已重启）立即报 reconnect——绝不隐式落到新 JVM（spec §5.1/§5.8）。
    pub fn check_generation(&self, handle_gen: u64) -> Result<(), String> {
        let cur = self.generation();
        if handle_gen == cur {
            Ok(())
        } else {
            Err(lost_err(&format!(
                "sidecar restarted (handle generation {handle_gen} != current {cur}); \
                 connection handles must reconnect"
            )))
        }
    }

    /// stderr 尾部环形缓冲（~20 行；spec §5.1：connect 失败诊断附尾部）。
    pub fn stderr_tail(&self) -> Vec<String> {
        self.shared
            .stderr_tail
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect()
    }

    /// 非阻塞 disconnect（T10 close 槽）：只投帧不等响应。不注册 pending——调用方
    /// 不等待，响应帧由 dispatch 查表落空静默丢弃；Java per-conn FIFO 队列保证
    /// disconnect 落在在途 op 之后（Session.disconnect 先 drain 再关连接）。
    pub async fn disconnect_nowait(&self, handle: u64) {
        let id = self.shared.next_req.fetch_add(1, Ordering::SeqCst) + 1;
        let params = serde_json::json!({ "handle": handle });
        let line = codec::encode_request(
            id,
            "disconnect",
            params.as_object().expect("json! 字面量恒为对象"),
        );
        let g = self.shared.inner.lock().await;
        // sidecar 已死/未 spawn：连接已随 JVM（或从未建立的进程）消亡，无可投递。
        let Some(st) = g.as_ref() else {
            return;
        };
        let _ = st.tx.send(line);
    }

    /// 优雅关闭：关 stdin 写通道 → 写任务退出、stdin EOF → JVM 有序关闭全部 Connection
    /// 并退出（spec §5.1 EOF 契约）→ 等待 child 退出并报告状态。幂等。
    /// 等待有界：JVM 拒绝退出时升级 kill（进程杀不掉也不挂调用方）。
    /// 刻意不 drain pending、不 bump 世代：生产路径 SidecarMgr 是 OnceLock 静态、永不
    /// shutdown（进程退出即回收）；本方法仅测试/拆卸用，此时在途请求应为零。
    pub async fn shutdown(&self) -> Result<(), String> {
        self.shared.closing.store(true, Ordering::SeqCst);
        // tx 随 Running take 而 drop：writer recv None → stdin drop = EOF。
        let child = self.shared.inner.lock().await.take().map(|r| r.child);
        match child {
            Some(mut c) => match tokio::time::timeout(EOF_EXIT_GRACE, c.wait()).await {
                Ok(Ok(st)) if st.success() => Ok(()),
                Ok(Ok(st)) => Err(format!("sidecar shutdown: exited with {st}")),
                Ok(Err(e)) => Err(format!("sidecar shutdown: wait: {e}")),
                Err(_) => {
                    let _ = c.kill().await;
                    Err(format!(
                        "sidecar shutdown: no EOF exit within {EOF_EXIT_GRACE:?}; killed"
                    ))
                }
            },
            None => Ok(()),
        }
    }

    /// spawn + hello 校验（`g` 持有 inner 锁）。失败保持 `None`（下次 request 重试），
    /// child 已 kill。成功置 `Some(Running)`。
    async fn spawn(
        &self,
        g: &mut tokio::sync::MutexGuard<'_, Option<Running>>,
    ) -> Result<(), String> {
        let cfg = &self.shared.cfg;
        // shutdown 后再 request（测试/非常规路径）：重置 closing，死亡路径恢复生效。
        self.shared.closing.store(false, Ordering::SeqCst);
        // spawn 世代：单调不复用——判死比对依据（重启后迟到的旧判死不得误伤新 JVM）。
        let epoch = self.shared.next_epoch.fetch_add(1, Ordering::SeqCst) + 1;
        let java = resolve_java(cfg)?;
        let sep = if cfg!(windows) { ";" } else { ":" };
        // classpath 升级路径（jar 极多逼近 Windows ~32KB 命令行上限）：Java 9+ @argfile，v1 不做。
        let cp = format!(
            "{}{}{}/*",
            cfg.bridge_jar.display(),
            sep,
            cfg.drivers_dir.display()
        );
        let mut cmd = Command::new(java);
        cmd.arg("-Duser.timezone=UTC") // §5.4 时区口径契约，缺省必带
            .arg("-Dfile.encoding=UTF-8")
            .args(&cfg.java_opts)
            .arg("-cp")
            .arg(cp)
            .arg("app.oj.jdbc.Bridge")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in &cfg.env {
            cmd.env(k, v);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("sidecar spawn failed: {e}"))?;

        // stderr drain 必须先于 hello 启动：JVM 启动期 stderr 写满 64KB 会楔死整进程
        // （§5.1 楔形死锁），hello 等待期间同样要有人消费。
        let stderr = child.stderr.take().expect("piped stderr");
        tokio::spawn(stderr_loop(
            tokio::io::BufReader::new(stderr),
            self.shared.clone(),
        ));

        // hello：有界等待 + 协议严格相等；不匹配报两侧版本号。
        let stdout = child.stdout.take().expect("piped stdout");
        let mut lines = tokio::io::BufReader::new(stdout);
        let hello = match read_hello_line(&mut lines, cfg.hello_timeout).await {
            Ok(Some(h)) => h,
            Ok(None) => {
                let _ = child.kill().await;
                return Err(format!(
                    "sidecar hello: EOF before hello line (process died during startup){}",
                    stderr_hint(&self.shared)
                ));
            }
            Err(why) => {
                let _ = child.kill().await;
                return Err(format!("sidecar hello: {why}{}", stderr_hint(&self.shared)));
            }
        };
        if let Err(e) = check_hello(&hello).await {
            // 任何 hello 失败都杀 child：继续运行只会留下孤儿 JVM（持有管道 fd）。
            let _ = child.kill().await;
            return Err(format!("sidecar hello: {e}{}", stderr_hint(&self.shared)));
        }

        // hello 通过：起 stdin 写任务 + stdout 读任务，放行请求。
        let stdin = child.stdin.take().expect("piped stdin");
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(writer_loop(rx, stdin, self.shared.clone(), epoch));
        tokio::spawn(reader_loop(lines, self.shared.clone(), epoch));
        **g = Some(Running { tx, child, epoch });
        Ok(())
    }
}

/// 判死（spec §5.1）——**世代闸**：只有「发起本判死的那代 JVM」仍占据 inner 时才动手；
/// state 已换代（重启完成）或已 None（同代另一管道先判死）→ 直接返回，不 bump、不
/// drain（一次物理死亡只 +1 世代，且绝不误杀健康新 JVM）。竞态背景：writer 的 EPIPE
/// 判死与 reader 的 EOF 判死是两次独立调用，其间在途请求可能已完成 JVM2 的 spawn+hello。
async fn declare_dead(shared: &Arc<Shared>, why: &str, epoch: u64) {
    if shared.closing.load(Ordering::SeqCst) {
        return; // 优雅关闭中的 EOF 是预期退出
    }
    let child = {
        let mut g = shared.inner.lock().await;
        if !matches!(g.as_ref(), Some(r) if r.epoch == epoch) {
            return; // 世代不匹配：迟到的旧判死，或同代重复判死（幂等）
        }
        shared.generation.fetch_add(1, Ordering::SeqCst); // bump 纳入世代闸保护
        g.take().map(|r| r.child)
    };
    if let Some(mut c) = child {
        // reaper：回收退出码防僵尸（句柄泄漏可接受，进程生命周期 = 插件生命周期）。
        tokio::spawn(async move {
            let _ = c.wait().await;
        });
    }
    // tx 随 Running take 而 drop → writer 收 None 退出 → stdin EOF（契约顺序的一部分）。
    // 只清发给本代 JVM 的条目（epoch=None 未投递 / 其它 epoch 发给了新 JVM，均不动）。
    let mut pending = shared.pending.lock().unwrap();
    let dead_ids: Vec<u64> = pending
        .iter()
        .filter(|(_, p)| p.epoch == Some(epoch))
        .map(|(id, _)| *id)
        .collect();
    for id in dead_ids {
        if let Some(p) = pending.remove(&id) {
            let _ = p.tx.send(Err(lost_err(why))); // receiver 已 drop：静默丢弃
        }
    }
}

fn frame_reason(fe: &FrameError) -> String {
    match fe {
        // 64MiB 超限 = 协议损坏（防 native 库直写 fd 1 的无换行垃圾永远楔死 reader）。
        FrameError::TooLong => "protocol violation: line exceeds 64 MiB limit".to_string(),
        FrameError::NotUtf8 => "protocol violation: non-UTF8 line on stdout".to_string(),
        FrameError::Json(e) => format!("protocol violation: bad response frame: {e}"),
    }
}

/// stdout 读任务：解帧 → 分派 pending；任何异常（IO/EOF/帧错）= sidecar 判死
/// （携带本代 epoch：世代闸只认发起判死的那代 JVM）。
async fn reader_loop(
    mut lines: tokio::io::BufReader<ChildStdout>,
    shared: Arc<Shared>,
    epoch: u64,
) {
    let mut fr = FrameReader::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match lines.read(&mut buf).await {
            Err(e) => {
                declare_dead(&shared, &format!("stdout read error: {e}"), epoch).await;
                return;
            }
            Ok(0) => {
                // EOF = 进程退出（崩溃或被杀），与协议死同路径判死。
                declare_dead(&shared, "stdout EOF (sidecar process exited)", epoch).await;
                return;
            }
            Ok(n) => match fr.push(&buf[..n]) {
                Err(fe) => {
                    let why = frame_reason(&fe);
                    declare_dead(&shared, &why, epoch).await;
                    return;
                }
                Ok(chunk_lines) => {
                    for line in chunk_lines {
                        if let Err(why) = dispatch(&line, &shared) {
                            declare_dead(&shared, &why, epoch).await;
                            return;
                        }
                    }
                }
            },
        }
    }
}

/// 分派一帧到 pending。Err = 协议级判死原因。
fn dispatch(line: &str, shared: &Shared) -> Result<(), String> {
    let resp = codec::parse_response(line).map_err(|fe| frame_reason(&fe))?;
    let (id, result) = match resp {
        Response::Ok { id, value } => (id, Ok(value)),
        Response::Err {
            id,
            message,
            sql_state,
            error_code,
        } => {
            let (category, retryable, disposition) =
                errmap::classify(sql_state.as_deref(), error_code);
            let mut data = serde_json::Map::new();
            if let Some(c) = error_code {
                data.insert("vendor_code".to_string(), Value::from(c));
            }
            (
                id,
                Err(serde_json::json!({
                    "category": category,
                    "retryable": retryable,
                    "session_disposition": disposition,
                    // 凭据脱敏（spec §5.8 硬性）：驱动错误消息回显完整 JDBC URL。
                    "message": errmap::redact(&message),
                    "data": data,
                })
                .to_string()),
            )
        }
    };
    if let Some(p) = shared.pending.lock().unwrap().remove(&id) {
        let _ = p.tx.send(result); // receiver 已 drop（宿主 KillSwitch）：静默丢弃
    }
    Ok(())
}

/// stdin 写任务：消费 unbounded 通道（非阻塞投递方是 request）；写失败 = sidecar 已死
/// （携带本代 epoch：世代闸只认发起判死的那代 JVM）。
async fn writer_loop(
    mut rx: mpsc::UnboundedReceiver<String>,
    mut stdin: ChildStdin,
    shared: Arc<Shared>,
    epoch: u64,
) {
    while let Some(line) = rx.recv().await {
        if stdin.write_all(line.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
            declare_dead(&shared, "stdin write failed (sidecar process gone)", epoch).await;
            return;
        }
    }
    // rx 关闭（判死 take / shutdown）→ 本任务退出，stdin drop = EOF（§5.1 EOF 契约）。
}

/// stderr 常驻 drain（§5.1 硬性）：逐行进尾部环形缓冲（~20 行）+ 上送宿主 log。
/// JVM/驱动 stderr 写满管道缓冲（~64KB）即阻塞整个 sidecar——drain 缺席 = 楔形死锁。
async fn stderr_loop(mut r: tokio::io::BufReader<ChildStderr>, shared: Arc<Shared>) {
    let mut line = String::new();
    loop {
        line.clear();
        match r.read_line(&mut line).await {
            Ok(0) => break, // EOF：sidecar 退出
            Ok(_) => {
                let l = line.trim_end().to_string();
                if l.is_empty() {
                    continue;
                }
                {
                    let mut tail = shared.stderr_tail.lock().unwrap();
                    if tail.len() >= STDERR_TAIL {
                        tail.pop_front();
                    }
                    tail.push_back(l.clone());
                }
                if let Some(log) = &shared.cfg.log {
                    log(3, &l); // level 3 = warn（JVM/驱动 stderr 默认噪音级）
                }
            }
            Err(_) => break,
        }
    }
}

enum HelloStep {
    Eof,
    Line,
    More,
}

/// 有界读一行 hello（fill_buf 手动切片：无换行垃圾不会撑爆内存——上限即判死）。
async fn read_hello_line(
    reader: &mut tokio::io::BufReader<ChildStdout>,
    timeout: Duration,
) -> Result<Option<String>, String> {
    let mut hello: Vec<u8> = Vec::new();
    loop {
        let (step, used) = {
            let fb = tokio::time::timeout(timeout, reader.fill_buf())
                .await
                .map_err(|_| format!("timed out after {timeout:?}"))?
                .map_err(|e| format!("read error: {e}"))?;
            if fb.is_empty() {
                (HelloStep::Eof, 0)
            } else if let Some(i) = fb.iter().position(|&b| b == b'\n') {
                hello.extend_from_slice(&fb[..=i]);
                (HelloStep::Line, i + 1)
            } else {
                hello.extend_from_slice(fb);
                (HelloStep::More, fb.len())
            }
        };
        reader.consume(used);
        match step {
            HelloStep::Eof => return Ok(None),
            HelloStep::Line => {
                let s = String::from_utf8(hello).map_err(|_| "line not UTF-8".to_string())?;
                return Ok(Some(s));
            }
            HelloStep::More => {
                if hello.len() > codec::MAX_LINE {
                    return Err("line exceeds 64 MiB without newline".to_string());
                }
            }
        }
    }
}

/// hello 协议校验：严格相等；不匹配报两侧版本号（spec §5.8 错误文案规范）。
/// 不持有 child：杀进程是调用方（spawn）的统一收尾。
async fn check_hello(hello: &str) -> Result<(), String> {
    let bad = |why: &str| -> String {
        let snippet: String = hello.chars().take(64).collect();
        format!("not a valid hello line ({why}; plugin proto={PROTO}): {snippet}")
    };
    let v: Value = serde_json::from_str(hello.trim()).map_err(|e| bad(&e.to_string()))?;
    let Some(h) = v.get("hello").and_then(Value::as_object) else {
        return Err(bad("missing \"hello\" object"));
    };
    let proto = h.get("proto").and_then(Value::as_u64).unwrap_or(u64::MAX);
    if proto != PROTO {
        let bridge = h.get("bridge").and_then(Value::as_str).unwrap_or("unknown");
        return Err(format!(
            "proto mismatch: sidecar proto={proto} bridge={bridge}, \
             plugin expects proto={PROTO} — .so 与 jar 必须同版整目录拷贝"
        ));
    }
    Ok(())
}

/// spawn/hello 失败时的 stderr 尾部提示（java 缺失/驱动加载失败时 child 秒退，无尾部不可诊断）。
fn stderr_hint(shared: &Shared) -> String {
    let tail = shared.stderr_tail.lock().unwrap();
    if tail.is_empty() {
        String::new()
    } else {
        format!(
            "; stderr tail:\n{}",
            tail.iter().cloned().collect::<Vec<_>>().join("\n")
        )
    }
}

/// java 解析顺序（spec §5.1）：cfg `java_bin` → `JAVA_HOME/bin/java` → PATH。
/// 三者皆无 → 结构化错误含搜索顺序 + 三条修复路径（spec §5.8 错误文案规范）。
fn resolve_java(cfg: &SidecarCfg) -> Result<PathBuf, String> {
    fn java_name() -> &'static str {
        if cfg!(windows) { "java.exe" } else { "java" }
    }
    if let Some(j) = &cfg.java_bin {
        let p = PathBuf::from(j);
        if p.is_file() {
            return Ok(p);
        }
        // cfg 给了但不存在：落到下一优先源（给出错机会而非直接拒绝）。
    }
    if let Some(home) = std::env::var_os("JAVA_HOME") {
        let p = Path::new(&home).join("bin").join(java_name());
        if p.is_file() {
            return Ok(p);
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let p = dir.join(java_name());
            if p.is_file() {
                return Ok(p);
            }
        }
    }
    Err(
        "java not found (searched in order: plugins.oj-db-jdbc.java_bin → \
         JAVA_HOME/bin/java → PATH). Fix: set java_bin to a JDK's java, or install a JDK \
         and set JAVA_HOME, or put java on PATH"
            .to_string(),
    )
}
