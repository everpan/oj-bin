//! exec 全局（ojm 批次，设计稿 §1.1/§1.3）：外部命令执行原语。
//!
//! 安全形态 = 配置白名单 + argv 模板 + 平台特权限定（无 shell）：
//! - 模板 literal token 逐字相等，JS 只能填类型化占位符（[`template`]）；
//! - `${path:NAME}` 吸收一个 token，值经 [`platform::resolve_allowed`] 校验
//!   （fs.root ∪ trusted_paths，路径校验单点复用，勿复制语义）；
//! - `${raw:NAME}` 吸收一个 token，拒绝 `-` 开头与绝对形态（防 option/路径注入）；
//! - 命令名（模板首 token）宿主侧解析为绝对路径（[`default_resolve_command`]，
//!   `"oj"` 特判 `current_exe`，其余父进程 PATH 解析；解析器做成 `ExecGrant`
//!   字段，测试可注入）；
//! - cwd = fs.root；env 默认空表仅白名单透传；stdout/stderr 流式读取边计数
//!   （合计 10MB 上限）；超时先 `kill(-pgid, SIGKILL)`（spawn 时
//!   `process_group(0)`）再 `child.kill()` 兜底；
//! - 返回 vs 抛错边界：进程正常跑到退出（含非 0 code）→ 返回
//!   `{code, stdout, stderr}`（信号终止 code = 128 + signal）；执行环境失败
//!   （未授权/模板失配/歧义/占位符校验失败/超时/输出超限/spawn 失败/UTF-8
//!   解码失败）→ 抛错点名。

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use deno_core::OpState;
use deno_core::op2;
use deno_error::JsErrorBox;
use serde::{Deserialize, Serialize};

use deno_core::AsyncResult;
use deno_core::BufView;
use deno_core::Resource;
use deno_core::ResourceId;
use std::borrow::Cow;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::sync::watch;

use super::ReqState;
use super::StableState;

// ----- 模板子模块：纯函数（解析 + 匹配），无 IO，独立可测（Task 3.1） -----

pub mod template {
    /// argv 模板 token：literal（逐字相等）/ 路径占位符（值按路径校验）/
    /// raw 占位符（字面收集，拒 `-` 开头与绝对形态）。
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Token {
        Lit(String),
        Path(String),
        Raw(String),
        /// 占位符被字面前缀/后缀包裹，仍属**同一 argv token**。用于 Windows
        /// `cmd.exe` 的 `%VAR%` 形态：模板写 `cmd /c echo %${raw:NAME}%`，
        /// 占位符吸收中间子串 `NAME`（`%…%` 为 cmd 的变量定界符，无法拆成
        /// 独立 token），重建时以模板前缀/后缀原样还原为 `%NAME%`。
        /// 一个 token 至多一个占位符；前缀/后缀为空时退化为整 token 占位符。
        /// 安全不变量不变：占位符仍吸收恰一个 argv token，前后字面量来自
        /// 受信模板（非用户输入），重建不产生新的 token / 不引入注入面。
        Wrapped {
            prefix: String,
            inner: Box<Token>,
            suffix: String,
        },
    }

    /// 解析后的 argv 模板。首 token 恒为 literal（命令名，宿主解析器覆盖）。
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ExecTemplate {
        /// 原文（诊断信息）。
        pub raw: String,
        pub tokens: Vec<Token>,
    }

    /// 多模板匹配失败：0 命中（携最有诊断价值的失配原因）或 ≥2 命中（歧义，
    /// 不静默择一，设计稿 §1.1）。
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum MatchError {
        NoMatch { reason: String },
        Ambiguous { count: usize },
    }

    /// 解析模板：空白拆 token。token 形态：literal（逐字）或恰含一个占位符
    /// `${path:NAME}` / `${raw:NAME}`（整 token，或前后带字面前缀/后缀）；
    /// 未知占位符类型 / 语法错 / 首 token 非 literal / 首 token 含路径分隔符
    /// / 一个 token 含多占位符 → Err 点名。
    pub fn parse_template(s: &str) -> Result<ExecTemplate, String> {
        let mut tokens = Vec::new();
        for (i, tok) in s.split_whitespace().enumerate() {
            if i == 0 {
                if tok.contains("${") {
                    return Err(format!(
                        "template command name must be a literal token, got '{tok}'"
                    ));
                }
                if tok.contains('/') || tok.contains('\\') {
                    return Err(format!(
                        "template command name must be a bare name (path separators rejected): '{tok}'"
                    ));
                }
                tokens.push(Token::Lit(tok.to_string()));
                continue;
            }
            tokens.push(parse_token(tok)?);
        }
        if tokens.is_empty() {
            return Err("template is empty".to_string());
        }
        Ok(ExecTemplate {
            raw: s.to_string(),
            tokens,
        })
    }

    /// 单个非首 token：不含 `${` = literal；含则必须恰有一个连续占位符 `${kind:NAME}`，其前后可带字面前缀/后缀（同一 argv token 内，用于 Windows `cmd /c echo %VAR%` 的 `%…%` 定界）。前缀/后缀逐字还原；占位符吸收 token 中间子串作为绑定值。
    fn parse_token(tok: &str) -> Result<Token, String> {
        let Some(open) = tok.find("${") else {
            return Ok(Token::Lit(tok.to_string()));
        };
        let rest = &tok[open..];
        let close_rel = rest
            .find('}')
            .ok_or_else(|| format!("invalid placeholder token '{tok}' (missing closing '}}')"))?;
        if rest[1..close_rel].contains("${") {
            return Err(format!(
                "invalid placeholder token '{tok}' (nested placeholder not allowed)"
            ));
        }
        if rest[close_rel + 1..].contains("${") {
            return Err(format!(
                "invalid placeholder token '{tok}' (only one placeholder per token allowed)"
            ));
        }
        let close = open + close_rel;
        let inner = &tok[open + 2..close];
        let (kind, name) = inner.split_once(':').ok_or_else(|| {
            format!("invalid placeholder token '{tok}' (expected ${{kind:NAME}})")
        })?;
        if !valid_name(name) {
            return Err(format!(
                "invalid placeholder name in '{tok}' (NAME must match [A-Za-z_][A-Za-z0-9_]*)"
            ));
        }
        let core = match kind {
            "path" => Token::Path(name.to_string()),
            "raw" => Token::Raw(name.to_string()),
            other => {
                return Err(format!(
                    "unknown placeholder type '${{{other}:...}}' in '{tok}' (supported: path, raw)"
                ));
            }
        };
        if open == 0 && close == tok.len() - 1 {
            Ok(core)
        } else {
            Ok(Token::Wrapped {
                prefix: tok[..open].to_string(),
                inner: Box::new(core),
                suffix: tok[close + 1..].to_string(),
            })
        }
    }

    fn valid_name(name: &str) -> bool {
        let mut b = name.bytes();
        match b.next() {
            Some(c) if c.is_ascii_alphabetic() || c == b'_' => {}
            _ => return false,
        }
        b.all(|c| c.is_ascii_alphanumeric() || c == b'_')
    }

    /// raw 占位符取值校验：拒绝 `-` 起手（option 注入）与绝对形态（路径注入）。
    /// 文案不含模板前缀，由调用方拼接 `template '...': `。
    fn check_raw(name: &str, value: &str) -> Result<(), String> {
        if value.starts_with('-') {
            return Err(format!(
                "${{raw:{name}}} rejects option-like token '{value}'"
            ));
        }
        if is_absolute_form(value) {
            return Err(format!(
                "${{raw:{name}}} rejects absolute-form token '{value}'"
            ));
        }
        Ok(())
    }

    /// 单模板匹配：literal 逐字相等；占位符吸收**恰一个** token；返回绑定表
    /// （name, value）。长度不齐 / literal 失配 / raw 非法形态 → Err（携原因）。
    /// `${path:}` 的值校验（jail 集合）不在纯函数层——需 fs 状态，op 层做。
    /// `Wrapped` 占位符：argv token 须以模板前缀起手、后缀收尾，中间子串即绑定值。
    pub fn match_template(
        t: &ExecTemplate,
        argv: &[String],
    ) -> Result<Vec<(String, String)>, String> {
        if argv.len() != t.tokens.len() {
            return Err(format!(
                "template '{}': expects {} tokens, argv has {}",
                t.raw,
                t.tokens.len(),
                argv.len()
            ));
        }
        let mut bindings = Vec::new();
        for (i, (tok, arg)) in t.tokens.iter().zip(argv.iter()).enumerate() {
            match tok {
                Token::Lit(l) => {
                    if l != arg {
                        return Err(format!(
                            "template '{}': token {i} expects literal '{l}', got '{arg}'",
                            t.raw
                        ));
                    }
                }
                Token::Path(name) => bindings.push((name.clone(), arg.clone())),
                Token::Raw(name) => {
                    check_raw(name, arg).map_err(|e| format!("template '{}': {e}", t.raw))?;
                    bindings.push((name.clone(), arg.clone()));
                }
                Token::Wrapped {
                    prefix,
                    inner,
                    suffix,
                } => {
                    let a = arg.as_str();
                    if !a.starts_with(prefix) || !a.ends_with(suffix) {
                        return Err(format!(
                            "template '{}': token expects '{prefix}${{...}}{suffix}', got '{arg}'",
                            t.raw
                        ));
                    }
                    let inner_val = &a[prefix.len()..a.len() - suffix.len()];
                    match inner.as_ref() {
                        Token::Path(name) => bindings.push((name.clone(), inner_val.to_string())),
                        Token::Raw(name) => {
                            check_raw(name, inner_val)
                                .map_err(|e| format!("template '{}': {e}", t.raw))?;
                            bindings.push((name.clone(), inner_val.to_string()));
                        }
                        _ => unreachable!("Wrapped inner is Path or Raw"),
                    }
                }
            }
        }
        Ok(bindings)
    }

    /// 绝对形态：`/` 起手、`\` 起手或 Windows 盘符起手（`C:` / `C:\...`）。
    fn is_absolute_form(s: &str) -> bool {
        if s.starts_with('/') || s.starts_with('\\') {
            return true;
        }
        let b = s.as_bytes();
        b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
    }

    /// argv 与模板 tokens 的共同前缀长度（literal 计入失配即止；占位符恒吸收）。
    /// 失配原因的「走得更深」排序依据——最有诊断价值。
    fn common_prefix_len(tokens: &[Token], argv: &[String]) -> usize {
        tokens
            .iter()
            .zip(argv.iter())
            .take_while(|(t, a)| match t {
                Token::Lit(l) => l == *a,
                Token::Wrapped { prefix, suffix, .. } => {
                    a.starts_with(prefix) && a.ends_with(suffix)
                }
                _ => true,
            })
            .count()
    }

    /// 多模板匹配：恰好 1 命中 → (模板下标, 绑定表)；0 命中 → NoMatch（原因取
    /// 共同前缀最长的那次失配）；≥2 → Ambiguous（歧义拒绝，不静默择一）。
    pub fn match_all(
        templates: &[ExecTemplate],
        argv: &[String],
    ) -> Result<(usize, Vec<(String, String)>), MatchError> {
        let mut matched: Vec<(usize, Vec<(String, String)>)> = Vec::new();
        let mut best_fail: Option<(usize, String)> = None;
        for (i, t) in templates.iter().enumerate() {
            match match_template(t, argv) {
                Ok(b) => matched.push((i, b)),
                Err(reason) => {
                    let depth = common_prefix_len(&t.tokens, argv);
                    if best_fail.as_ref().is_none_or(|(d, _)| depth > *d) {
                        best_fail = Some((depth, reason));
                    }
                }
            }
        }
        match matched.len() {
            1 => Ok(matched.pop().expect("len == 1 checked")),
            0 => Err(MatchError::NoMatch {
                reason: best_fail
                    .map(|(_, r)| r)
                    .unwrap_or_else(|| "module has no authorized exec templates".to_string()),
            }),
            n => Err(MatchError::Ambiguous { count: n }),
        }
    }
}

// ----- op_exec_run（Task 3.2） -----

/// 输出合计上限：10MB（stdout + stderr 流式合计，超限 kill + 抛错）。
const OUTPUT_LIMIT: usize = 10 * 1024 * 1024;
/// timeoutMs 缺省 30s；硬上限 600s（超出钳到上限）。
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const MAX_TIMEOUT_MS: u64 = 600_000;

#[derive(Debug, Default, Deserialize)]
struct ExecOpts {
    #[serde(rename = "timeoutMs")]
    timeout_ms: Option<u64>,
    #[serde(rename = "stdin")]
    stdin: Option<String>,
}

#[derive(Debug, Serialize)]
struct ExecResult {
    code: i32,
    stdout: String,
    stderr: String,
}

fn not_capable(msg: impl Into<String>) -> JsErrorBox {
    JsErrorBox::new("NotCapable", msg.into())
}

// ============================================================================
// 流式执行（v0.1.78，设计稿 §2-§4）：spawn -> 双管道并发 drain 进有界 mpsc ->
// 边读边推进；exec.spawn（B 句柄）/ exec.run 回调（A 糖）均建在其上。复用
// prepare_exec 的全部安全闸门（授权 / 模板 / ${path:}/${raw:} / 解析 / env 白名单）。
// 资源回收：ExecHandleResource::close 同步发 kill 信号，配合 runtime.rs::checkin
// 与 mod.rs::checkout_reset 的 resource_table 枚举 close 门禁，杜绝跨请求/租户
// 泄漏子进程组与 next_rid 回绕 panic（评审 CRITICAL）。
// ============================================================================

/// 单流 drain 缓冲队列容量（槽数）。JS 不消费 -> 队列满 -> drain 在 send 上挂起
/// -> 子进程管道满 -> 子进程自然暂停（真实子进程语义，正确背压）。
const STREAM_CHANNEL_BOUND: usize = 16;

/// prepare_exec：授权裁决 + 模板解析/匹配 + 命令解析 + argv 重建（设计稿 §4.2）。
/// 不含 spawn/kill_tree——缓冲路径（op_exec_run）与流式路径（op_exec_spawn）共用。
struct PreparedExec {
    prog: PathBuf,
    final_argv: Vec<String>,
    env_names: Vec<String>,
    cwd: PathBuf,
    timeout_ms: u64,
}

fn prepare_exec(
    state: &Rc<RefCell<OpState>>,
    argv: Vec<String>,
    opts: &ExecOpts,
) -> Result<PreparedExec, JsErrorBox> {
    let (exec, fs, platform, module) = {
        let st = state.borrow();
        let stable = st.borrow::<Arc<StableState>>();
        (
            stable.exec.clone(),
            stable.fs.clone(),
            stable.platform.clone(),
            st.borrow::<ReqState>().module.clone(),
        )
    };

    let exec = exec
        .ok_or_else(|| not_capable("exec not configured (config platform.exec section missing)"))?;
    let module = module.ok_or_else(|| {
        not_capable("exec requires module ownership (request has no module context)")
    })?;
    let grant = exec
        .per_module
        .get(&module)
        .ok_or_else(|| not_capable(format!("module '{module}' is not authorized for exec")))?;

    let fs = fs.ok_or_else(|| not_capable("exec requires fs.root (config fs: section missing)"))?;
    let trusted: &[PathBuf] = platform
        .as_deref()
        .map(|p| p.trusted.as_slice())
        .unwrap_or(&[]);

    let mut templates = Vec::with_capacity(grant.templates.len());
    for raw in &grant.templates {
        templates.push(template::parse_template(raw).map_err(|e| {
            JsErrorBox::generic(format!("exec template parse failed ({raw}): {e}"))
        })?);
    }

    let (idx, bindings) = template::match_all(&templates, &argv).map_err(|e| match e {
        template::MatchError::NoMatch { reason } => {
            not_capable(format!("exec template mismatch: {reason}"))
        }
        template::MatchError::Ambiguous { count } => not_capable(format!(
            "exec template ambiguous: argv matches {count} authorized templates"
        )),
    })?;
    let matched = &templates[idx];

    let first = match &matched.tokens[0] {
        template::Token::Lit(s) => s.clone(),
        _ => unreachable!("parse_template enforces a literal command token"),
    };
    let prog = (exec.resolver)(&first).map_err(|e| {
        JsErrorBox::generic(format!("exec: command resolve failed for '{first}': {e}"))
    })?;

    let mut final_argv: Vec<String> = Vec::with_capacity(matched.tokens.len());
    final_argv.push(first);
    let mut bound = bindings.into_iter();
    for tok in &matched.tokens[1..] {
        match tok {
            template::Token::Lit(s) => final_argv.push(s.clone()),
            template::Token::Path(name) => {
                let val = &bound
                    .next()
                    .map(|(_, v)| v)
                    .expect("bindings align with placeholders 1:1");
                let resolved = super::platform::resolve_allowed(&fs.root, trusted, val)
                    .map_err(|e| not_capable(format!("exec ${{path:{name}}}: {e}")))?;
                final_argv.push(resolved.to_string_lossy().into_owned());
            }
            template::Token::Raw(_) => final_argv.push(
                bound
                    .next()
                    .map(|(_, v)| v)
                    .expect("bindings align with placeholders 1:1"),
            ),
            template::Token::Wrapped {
                prefix,
                inner,
                suffix,
            } => {
                let val = bound
                    .next()
                    .map(|(_, v)| v)
                    .expect("bindings align with placeholders 1:1");
                let resolved = match inner.as_ref() {
                    template::Token::Path(name) => {
                        super::platform::resolve_allowed(&fs.root, trusted, &val)
                            .map_err(|e| not_capable(format!("exec ${{path:{name}}}: {e}")))?
                            .to_string_lossy()
                            .into_owned()
                    }
                    template::Token::Raw(_) => val,
                    _ => unreachable!("Wrapped inner is Path or Raw"),
                };
                final_argv.push(format!("{prefix}{resolved}{suffix}"));
            }
        }
    }

    let timeout_ms = opts
        .timeout_ms
        .unwrap_or(DEFAULT_TIMEOUT_MS)
        .min(MAX_TIMEOUT_MS);
    Ok(PreparedExec {
        prog,
        final_argv,
        env_names: grant.env.clone(),
        cwd: fs.root.clone(),
        timeout_ms,
    })
}

/// 单流 drain：stdout/stderr 管道读块经有界 mpsc 送消费端；合计超 OUTPUT_LIMIT
/// 触发整组击杀（kill_tx）+ 停读；消费端关闭（tx 丢弃）亦停读（背压）。
async fn drain_to_mpsc<R: tokio::io::AsyncRead + Unpin>(
    mut pipe: R,
    total: Arc<AtomicUsize>,
    tx: mpsc::Sender<Vec<u8>>,
    kill_tx: mpsc::UnboundedSender<()>,
) {
    use tokio::io::AsyncReadExt;
    let mut buf = [0u8; 8192];
    loop {
        match pipe.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                let prev = total.fetch_add(n, Ordering::Relaxed);
                if prev + n > OUTPUT_LIMIT {
                    let _ = kill_tx.send(());
                    break;
                }
                if tx.send(buf[..n].to_vec()).await.is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    // tx 在此丢弃 -> 接收端 recv() 返回 None（EOF）
}

/// 退出码换算（与 run_managed 同口径：信号终止 = 128 + signal）。
fn compute_code(status: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        match status.signal() {
            Some(sig) => 128 + sig,
            None => status.code().unwrap_or(-1),
        }
    }
    #[cfg(not(unix))]
    {
        status.code().unwrap_or(-1)
    }
}

/// 流资源：stdout/stderr 任一管道接收端；read 拉一块，EOF 返回空 BufView。
struct StreamResource {
    // Option 包裹：next_chunk 取出 receiver 后释放 RefCell 借用再 await，
    // 避免跨 await 持有 RefMut（clippy::await_holding_refcell_ref）。
    rx: Rc<RefCell<Option<mpsc::Receiver<Vec<u8>>>>>,
}

impl StreamResource {
    async fn next_chunk(&self, limit: usize) -> Result<Vec<u8>, JsErrorBox> {
        let mut rx = self
            .rx
            .borrow_mut()
            .take()
            .expect("exec stream receiver already taken");
        let chunk = rx.recv().await;
        *self.rx.borrow_mut() = Some(rx);
        match chunk {
            Some(mut buf) => {
                if limit > 0 && buf.len() > limit {
                    buf.truncate(limit);
                }
                Ok(buf)
            }
            None => Ok(Vec::new()),
        }
    }
}

impl Resource for StreamResource {
    fn name(&self) -> Cow<'_, str> {
        Cow::Borrowed("exec_stream")
    }
    fn read(self: Rc<Self>, limit: usize) -> AsyncResult<BufView> {
        Box::pin(async move {
            let chunk = self.next_chunk(limit).await?;
            Ok(BufView::from(chunk))
        })
    }
}

/// 句柄资源：持有 kill 信号发送端 + 退出码 watch 接收端 + pid。close() 同步发
/// kill 信号（覆盖 JS 释放 / runtime 回收 / 异常丢弃等全部路径），由 supervisor
/// 任务据此整组击杀子进程，杜绝孤儿进程组（评审 CRITICAL 资源回收）。
struct ExecHandleResource {
    kill_tx: mpsc::UnboundedSender<()>,
    exit: watch::Receiver<Option<i32>>,
}

impl Resource for ExecHandleResource {
    fn name(&self) -> Cow<'_, str> {
        Cow::Borrowed("exec_handle")
    }
    fn close(self: Rc<Self>) {
        let _ = self.kill_tx.send(());
    }
}

#[derive(Debug, Serialize)]
struct ExecSpawnResult {
    rid: u32,
    pid: u32,
    stdout_rid: u32,
    stderr_rid: u32,
}

#[derive(Serialize)]
struct ExecExitResult {
    code: i32,
}

/// op_exec_spawn：流式底座（B 句柄）。spawn 子进程，启动 supervisor 任务拥有
/// child 并广播退出码；双管道 drain 进有界 mpsc；三资源入表（两流 + 一句柄）。
#[op2]
#[serde]
pub async fn op_exec_spawn(
    state: Rc<RefCell<OpState>>,
    #[serde] argv: Vec<String>,
    #[serde] opts: serde_json::Value,
) -> Result<ExecSpawnResult, JsErrorBox> {
    let opts: ExecOpts = serde_json::from_value(opts).unwrap_or_default();
    let p = prepare_exec(&state, argv, &opts)?;
    let mut cmd = tokio::process::Command::new(&p.prog);
    cmd.args(&p.final_argv[1..]);
    cmd.current_dir(&p.cwd);
    cmd.env_clear();
    for name in &p.env_names {
        if let Some(v) = std::env::var_os(name) {
            cmd.env(name, v);
        }
    }
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd.spawn().map_err(|e| {
        JsErrorBox::generic(format!(
            "exec: spawn failed for '{}': {e}",
            p.prog.display()
        ))
    })?;
    let pid = child.id().unwrap_or(0);
    let stdout_pipe = child.stdout.take().expect("stdout piped");
    let stderr_pipe = child.stderr.take().expect("stderr piped");

    let (so_tx, so_rx) = mpsc::channel::<Vec<u8>>(STREAM_CHANNEL_BOUND);
    let (se_tx, se_rx) = mpsc::channel::<Vec<u8>>(STREAM_CHANNEL_BOUND);
    let (kill_tx, mut kill_rx) = mpsc::unbounded_channel::<()>();
    let kill_tx_sup = kill_tx.clone();
    let (exit_tx, exit_rx) = watch::channel::<Option<i32>>(None);
    let timeout_ms = p.timeout_ms;

    // supervisor：拥有 child 强引用；监听 kill 信号与超时；退出码经 watch 广播。
    tokio::spawn(async move {
        let total = Arc::new(AtomicUsize::new(0));
        let dh1 = tokio::spawn(drain_to_mpsc(
            stdout_pipe,
            total.clone(),
            so_tx,
            kill_tx_sup.clone(),
        ));
        let dh2 = tokio::spawn(drain_to_mpsc(
            stderr_pipe,
            total.clone(),
            se_tx,
            kill_tx_sup.clone(),
        ));
        // 关键：kill 信号必须优先于「子进程自然退出」被采信，且一旦请求过 kill，
        // 退出码强制为 137（SIGKILL），不受 kill 信号到达前子进程已将缓冲数据写完
        // 并以 0 退出的竞态影响（评审 CRITICAL：OUTPUT_LIMIT/超时/显式 kill 必须
        // 稳定命中 137，否则下游按「已成功」误判）。
        // biased 保证 kill_rx 优先；recv() 返回 None 表示发送端已全部丢弃（自然
        // 完成，句柄未关闭前不会触发），此时仍走自然退出码。
        let mut killed = false;
        let status = tokio::select! {
            biased;
            r = kill_rx.recv() => {
                killed = r.is_some();
                if killed {
                    kill_tree(&mut child);
                }
                child.wait().await
            }
            status = child.wait() => status,
            _ = tokio::time::sleep(Duration::from_millis(timeout_ms)) => {
                killed = true;
                kill_tree(&mut child);
                child.wait().await
            }
        };
        let code: i32 = if killed {
            137
        } else {
            status.map(compute_code).unwrap_or(-1)
        };
        let _ = exit_tx.send(Some(code));
        // 给 drain 一个收尾窗口（管道关闭后自然结束），避免截断已就绪未 flush 的块。
        let _ = tokio::time::timeout(Duration::from_secs(2), async {
            let _ = dh1.await;
            let _ = dh2.await;
        })
        .await;
    });

    let stdout_rid = state.borrow_mut().resource_table.add(StreamResource {
        rx: Rc::new(RefCell::new(Some(so_rx))),
    });
    let stderr_rid = state.borrow_mut().resource_table.add(StreamResource {
        rx: Rc::new(RefCell::new(Some(se_rx))),
    });
    let handle_rid = state.borrow_mut().resource_table.add(ExecHandleResource {
        kill_tx,
        exit: exit_rx,
    });
    let r = ExecSpawnResult {
        rid: handle_rid,
        pid,
        stdout_rid,
        stderr_rid,
    };
    Ok(r)
}

/// op_exec_read：从流资源拉一块（零长 = EOF）。配合 bootstrap.js 的异步迭代。
#[op2]
#[buffer]
pub async fn op_exec_read(
    state: Rc<RefCell<OpState>>,
    #[smi] rid: ResourceId,
    #[smi] limit: i32,
) -> Result<Vec<u8>, JsErrorBox> {
    let res = state
        .borrow()
        .resource_table
        .get::<StreamResource>(rid)
        .map_err(|e| JsErrorBox::generic(e.to_string()))?;
    let chunk = res.next_chunk(limit.max(0) as usize).await?;
    Ok(chunk)
}

/// op_exec_kill：经本会话句柄 rid 发整组击杀信号（仅能杀自己 spawn 的会话）。
#[op2(fast)]
pub fn op_exec_kill(state: Rc<RefCell<OpState>>, #[smi] rid: ResourceId) -> Result<(), JsErrorBox> {
    let res = state
        .borrow()
        .resource_table
        .get::<ExecHandleResource>(rid)
        .map_err(|e| JsErrorBox::generic(e.to_string()))?;
    let _ = res.kill_tx.send(());
    Ok(())
}

/// op_exec_exit：等退出码广播，返回 { code }（设计稿 §3.1）。
#[op2]
#[serde]
pub async fn op_exec_exit(
    state: Rc<RefCell<OpState>>,
    #[serde] rid: ResourceId,
) -> Result<ExecExitResult, JsErrorBox> {
    let mut exit_rx = {
        let st = state.borrow();
        st.resource_table
            .get::<ExecHandleResource>(rid)
            .map_err(|e| JsErrorBox::generic(e.to_string()))?
            .exit
            .clone()
    };
    loop {
        if let Some(code) = *exit_rx.borrow() {
            return Ok(ExecExitResult { code });
        }
        if exit_rx.changed().await.is_err() {
            return Ok(ExecExitResult { code: -1 });
        }
    }
}

/// exec.run(argv, opts?) —— 设计稿 §1.3 全条款。
/// opts: `{ timeoutMs?, stdin? }`。
#[op2]
#[serde]
pub async fn op_exec_run(
    state: Rc<RefCell<OpState>>,
    #[serde] argv: Vec<String>,
    #[serde] opts: serde_json::Value,
) -> Result<ExecResult, JsErrorBox> {
    let opts: ExecOpts = serde_json::from_value(opts).unwrap_or_default();
    let p = prepare_exec(&state, argv, &opts)?;
    let mut cmd = tokio::process::Command::new(&p.prog);
    cmd.args(&p.final_argv[1..]);
    cmd.current_dir(&p.cwd);
    cmd.env_clear();
    for name in &p.env_names {
        if let Some(v) = std::env::var_os(name) {
            cmd.env(name, v);
        }
    }
    cmd.stdin(if opts.stdin.is_some() {
        std::process::Stdio::piped()
    } else {
        std::process::Stdio::null()
    });
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd.spawn().map_err(|e| {
        JsErrorBox::generic(format!(
            "exec: spawn failed for '{}': {e}",
            p.prog.display()
        ))
    })?;

    match run_managed(&mut child, opts.stdin.as_deref(), p.timeout_ms).await {
        Ok(r) => Ok(r),
        Err(e) => {
            // 所有 spawn 之后的失败路径统一组杀 + 兜底 reap（不留残留进程）。
            kill_tree(&mut child);
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await;
            Err(JsErrorBox::generic(e))
        }
    }
}

/// 双流流式计数 + stdin 一次性写入 + 超时 + wait（全部落在 timeout 覆盖窗口内；
/// spawn 之后、错误统一组杀之前的管理段）。Err 携带点名文案（String，由调用方
/// 包 JsErrorBox）。
async fn run_managed(
    child: &mut tokio::process::Child,
    stdin: Option<&str>,
    timeout_ms: u64,
) -> Result<ExecResult, String> {
    // 双流读取任务先起（独立 spawn，不占 timeout 窗口）：即便随后 stdin 写入
    // 阻塞，子进程的 stdout 也持续被排走，减小死锁窗口。
    let total = Arc::new(AtomicUsize::new(0));
    let stdout_pipe = child.stdout.take().expect("stdout piped");
    let stderr_pipe = child.stderr.take().expect("stderr piped");
    let mut h1 = tokio::spawn(drain(stdout_pipe, total.clone()));
    let mut h2 = tokio::spawn(drain(stderr_pipe, total.clone()));

    let inner = async {
        // stdin：一次性写入后关闭（设计稿 §1.3.10）。**并入 timeout 覆盖窗口**
        // （评审 High 1）：子进程不读且输入 >64KB 时 write_all 阻塞 → 超时到期
        // 组杀，不再游离于超时之外（否则 KillSwitch drop op future 时
        // kill_tree 永不执行 → 进程组泄漏）。
        if let Some(input) = stdin {
            use tokio::io::AsyncWriteExt;
            let mut si = child
                .stdin
                .take()
                .expect("stdin piped when opts.stdin is set");
            si.write_all(input.as_bytes())
                .await
                .map_err(|e| format!("exec: stdin write failed: {e}"))?;
            drop(si); // 关闭写端 → 子进程读到 EOF
        }

        let mut r1: Option<Result<Vec<u8>, String>> = None;
        let mut r2: Option<Result<Vec<u8>, String>> = None;
        // 任一读取端失败（超限 / IO 错）立即短路返回，让调用方先组杀——另一端
        // 在进程被杀、管道关闭后自然到 EOF，不会悬挂。
        let early: Result<(), String> = loop {
            tokio::select! {
                r = &mut h1, if r1.is_none() => {
                    r1 = Some(r.unwrap_or_else(|je| Err(format!("exec: stdout reader aborted: {je}"))));
                    if let Some(Err(e)) = &r1 { break Err(e.clone()); }
                }
                r = &mut h2, if r2.is_none() => {
                    r2 = Some(r.unwrap_or_else(|je| Err(format!("exec: stderr reader aborted: {je}"))));
                    if let Some(Err(e)) = &r2 { break Err(e.clone()); }
                }
                else => break Ok(()),
            }
        };
        early?;
        let o1 = r1.expect("loop exits only when both readers completed")?;
        let o2 = r2.expect("loop exits only when both readers completed")?;
        let status = child
            .wait()
            .await
            .map_err(|e| format!("exec: wait failed: {e}"))?;
        Ok((o1, o2, status))
    };

    match tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), inner).await {
        Err(_elapsed) => Err(format!(
            "exec timed out after {timeout_ms} ms (process group killed)"
        )),
        Ok(Err(reason)) => Err(reason),
        Ok(Ok((o1, o2, status))) => {
            // 信号终止 → code = 128 + signal（设计稿 §1.3.6）。
            #[cfg(unix)]
            let code = {
                use std::os::unix::process::ExitStatusExt;
                match status.signal() {
                    Some(sig) => 128 + sig,
                    None => status.code().unwrap_or(-1),
                }
            };
            #[cfg(not(unix))]
            let code = status.code().unwrap_or(-1);
            let stdout = String::from_utf8(o1).map_err(|_| {
                "exec: stdout is not valid UTF-8 (binary output unsupported)".to_string()
            })?;
            let stderr = String::from_utf8(o2).map_err(|_| {
                "exec: stderr is not valid UTF-8 (binary output unsupported)".to_string()
            })?;
            Ok(ExecResult {
                code,
                stdout,
                stderr,
            })
        }
    }
}

/// 单流流式读取边计数：合计（stdout+stderr 共享计数器）超 OUTPUT_LIMIT 即
/// 停读返回 Err（已收部分不随错返回——调用方整段丢弃）。
async fn drain<R: tokio::io::AsyncRead + Unpin>(
    mut r: R,
    total: Arc<AtomicUsize>,
) -> Result<Vec<u8>, String> {
    use tokio::io::AsyncReadExt;
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = r
            .read(&mut buf)
            .await
            .map_err(|e| format!("exec: output read failed: {e}"))?;
        if n == 0 {
            return Ok(out);
        }
        let prev = total.fetch_add(n, Ordering::Relaxed);
        if prev + n > OUTPUT_LIMIT {
            return Err(format!(
                "exec output limit exceeded ({} bytes cap, stdout+stderr combined)",
                OUTPUT_LIMIT
            ));
        }
        out.extend_from_slice(&buf[..n]);
    }
}

/// 组杀：先 `kill(-pgid, SIGKILL)`（spawn 时 `process_group(0)`，pgid = 子进程
/// pid，负号 = 杀整组，孙进程同尽），再 `start_kill()` 兜底（直接子进程）。
/// Windows 无进程组对应物，仅兜底杀直接子进程（设计稿 §1.3.5，部署文档声明）。
fn kill_tree(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // SAFETY: kill(2) 仅向目标进程组发送信号，无内存语义。
        unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) };
    }
    let _ = child.start_kill();
}

// ----- 默认命令解析器（ExecGrant 的生产实现；测试经 with_resolver 注入） -----

/// 模板首 token → 绝对路径（设计稿 §1.1「宿主侧解析」）：
/// - `"oj"` 特判 → `std::env::current_exe()`（serve 进程即 bin/oj，同版本自保证）；
/// - 其余裸名 → 父进程 PATH 逐目录探测可执行文件（env 空表下子进程 execvp
///   会回落 confstr 默认路径、行为不可控——宿主侧解析消除该不确定性）；
/// - 含路径分隔符 → Err（parse 期已拒绝，此处防御性兜底）；
/// - 解析失败 → Err 点名（op 层转抛错）。
pub fn default_resolve_command(name: &str) -> Result<PathBuf, String> {
    if name == "oj" {
        return std::env::current_exe().map_err(|e| format!("current_exe failed: {e}"));
    }
    if name.contains('/') || name.contains('\\') {
        return Err(format!(
            "command name must be a bare name (path separators rejected): '{name}'"
        ));
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let cand = dir.join(name);
        if is_executable_file(&cand) {
            return Ok(cand);
        }
    }
    Err(format!("command '{name}' not found in PATH"))
}

#[cfg(unix)]
fn is_executable_file(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.is_file()
        && p.metadata()
            .is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable_file(p: &std::path::Path) -> bool {
    p.is_file()
}

#[cfg(test)]
mod tests {
    use super::template::{
        ExecTemplate, MatchError, Token, match_all, match_template, parse_template,
    };
    use super::*;
    use crate::bridge::Bridge;
    use crate::bridge::Extras;
    use crate::bridge::InMemoryKV;
    use crate::bridge::ModuleCtx;
    use crate::bridge::SchemaRegistry;
    use crate::bridge::fs::FsGrant;
    use crate::bridge::module_loader::LoaderShared;
    use crate::bridge::platform::{CommandResolver, ExecGrant, PlatformGrant};

    use std::collections::{HashMap, HashSet};
    use std::path::Path;
    use std::sync::Mutex;

    fn sargv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// 组杀残留断言（unix 门控，评审 Minor 3）：有界轮询 `kill(-pgid, 0)`，
    /// ESRCH（组内无存活）即通过；deadline 兜底 fail。sh 经 `process_group(0)`
    /// 自成组长（pgid = 自身 pid），脚本先 `echo $$ > pidfile` 交出 pgid。
    #[cfg(unix)]
    async fn wait_group_gone(pgid: i32, label: &str) {
        use std::time::{Duration, Instant};
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            // SAFETY: kill(2) 探测（sig=0 不投递信号），无副作用。
            let alive = unsafe { libc::kill(-pgid, 0) } == 0;
            if !alive {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{label}: process group {pgid} still alive 5s after kill"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// 读取脚本写出的组长 pid（pgid）。
    #[cfg(unix)]
    fn read_pid(jail: &Path, name: &str) -> i32 {
        let s = std::fs::read_to_string(jail.join(name)).expect("pidfile written by sh");
        s.trim().parse().expect("pid is an integer")
    }

    fn p(s: &str) -> ExecTemplate {
        parse_template(s).unwrap()
    }

    // ----- Task 3.1：模板解析（成功/失败形态） -----

    #[test]
    fn parse_shapes() {
        let t = p("tar -xzf ${path:archive} -C ${path:dest}");
        assert_eq!(
            t.tokens,
            vec![
                Token::Lit("tar".into()),
                Token::Lit("-xzf".into()),
                Token::Path("archive".into()),
                Token::Lit("-C".into()),
                Token::Path("dest".into()),
            ]
        );
        let t = p("oj migrate -c ${path:config} --format json -d ${path:root} ${raw:module}");
        assert_eq!(t.tokens[3], Token::Path("config".into()));
        assert_eq!(t.tokens[8], Token::Raw("module".into()));
        // 连续空白容忍。
        assert_eq!(p("echo   ${raw:x}").tokens.len(), 2);
        assert!(parse_template("   ").is_err());
        assert!(parse_template("").is_err());
    }

    #[test]
    fn parse_rejects_named() {
        // 首 token 含路径分隔符 → 解析期 Err（设计稿 §1.3.3）。
        let e = parse_template("/bin/tar -xzf x").unwrap_err();
        assert!(e.contains("path separators"), "{e}");
        let e = parse_template("..\\tar x").unwrap_err();
        assert!(e.contains("path separators"), "{e}");
        // 首 token 占位符 → 命令名必须 literal。
        let e = parse_template("${path:c} x").unwrap_err();
        assert!(e.contains("must be a literal"), "{e}");
        // 未知占位符类型点名。
        let e = parse_template("x ${exec:name}").unwrap_err();
        assert!(e.contains("unknown placeholder type"), "{e}");
        // 占位符可被字面前缀/后缀包裹（同一 token 内；Windows cmd `%VAR%` 形态）。
        let w = parse_template("cmd /c echo %${raw:NAME}%").unwrap();
        assert!(matches!(
            w.tokens.last(),
            Some(Token::Wrapped { prefix, inner, suffix })
                if prefix == "%"
                    && suffix == "%"
                    && matches!(inner.as_ref(), Token::Raw(n) if n == "NAME")
        ));
        // 前缀附着（无后缀）同样合法。
        assert!(parse_template("x a${path:n}").is_ok());
        // 一个 token 多占位符 → 拒绝。
        assert!(parse_template("x a${path:n}${raw:m}").is_err());
        // 语法错：未闭合 / 空名 / 非法名 / 多冒号 / 空占位。
        assert!(parse_template("x ${path:n").is_err());
        assert!(parse_template("x ${path:}").is_err());
        assert!(parse_template("x ${path:9bad}").is_err());
        assert!(parse_template("x ${path:a:b}").is_err());
        assert!(parse_template("x ${}").is_err());
    }

    // ----- Task 3.1：匹配（literal 逐字 / 占位符恰吸一 token / raw 拒绝） -----

    #[test]
    fn match_bindings_and_failures() {
        let t = p("tar -xzf ${path:archive} -C ${path:dest}");
        let b = match_template(&t, &sargv(&["tar", "-xzf", "/tmp/a.tgz", "-C", "/tmp/d"])).unwrap();
        assert_eq!(
            b,
            vec![
                ("archive".to_string(), "/tmp/a.tgz".to_string()),
                ("dest".to_string(), "/tmp/d".to_string()),
            ]
        );
        // literal 被替换（--use-compress-program 注入）→ 失配携原因点名。
        let e = match_template(
            &t,
            &sargv(&["tar", "--use-compress-program=x", "-xzf", "a", "d"]),
        )
        .unwrap_err();
        assert!(
            e.contains("literal '-xzf'") && e.contains("--use-compress-program=x"),
            "{e}"
        );
        // 占位符恰吸收一个 token：多出 / 缺失的 token → 长度失配（吞两段即失配）。
        let e = match_template(&t, &sargv(&["tar", "-xzf", "a", "-C", "d", "extra"])).unwrap_err();
        assert!(e.contains("expects 5 tokens, argv has 6"), "{e}");
        let e = match_template(&t, &sargv(&["tar", "-xzf", "a"])).unwrap_err();
        assert!(e.contains("argv has 3"), "{e}");
    }

    #[test]
    fn raw_placeholder_rejects_option_and_absolute() {
        let t = p("sh -c ${raw:script}");
        for bad in ["-x", "--long", "/abs", "\\abs", "C:\\x", "C:"] {
            let e = match_template(&t, &sargv(&["sh", "-c", bad])).unwrap_err();
            assert!(e.contains("${raw:script} rejects"), "{bad} -> {e}");
        }
        // 合法形态：中段 '-' / '/' 允许（起手形态才拒绝）。
        assert!(match_template(&t, &sargv(&["sh", "-c", "sleep 30 & wait"])).is_ok());
        assert!(match_template(&t, &sargv(&["sh", "-c", "a-b"])).is_ok());
        assert!(match_template(&t, &sargv(&["sh", "-c", "a/b"])).is_ok());
    }

    #[test]
    fn ambiguity_rejected_not_silently_picked() {
        let ts = vec![p("echo ${raw:a}"), p("echo ${raw:b}")];
        let e = match_all(&ts, &sargv(&["echo", "hi"])).unwrap_err();
        assert_eq!(e, MatchError::Ambiguous { count: 2 });
        // 0 命中 → 失配原因取「走得更深」的那次（literal 失配点）。
        let ts = vec![p("tar -xzf ${path:a}")];
        let e = match_all(&ts, &sargv(&["tar", "--bad", "x"])).unwrap_err();
        match e {
            MatchError::NoMatch { reason } => {
                assert!(reason.contains("literal '-xzf'"), "{reason}")
            }
            _ => panic!("expected NoMatch"),
        }
        // 恰一命中 → 模板下标 + 绑定表。
        let ts = vec![p("tar -xzf ${path:a}"), p("echo ${raw:w}")];
        let (i, b) = match_all(&ts, &sargv(&["echo", "hi"])).unwrap();
        assert_eq!(i, 1);
        assert_eq!(b, vec![("w".to_string(), "hi".to_string())]);
    }

    /// Windows 夹具模板表的「恰一命中」契约——**平台无关**（unix 上也跑）。
    ///
    /// 动机：Windows 夹具在本机既编不到也跑不了，v0.1.79 上连续两次由 CI
    /// 打脸（先是 `%${raw:NAME}%` 在解析期失败，修好后又撞上「裸 raw 与
    /// `%…%` 包裹 raw 同位置 → 恒歧义」）。故直接对真夹具表
    /// [`WIN_TEMPLATES`] 断言，把这类缺陷挡在 unix 面。
    #[test]
    fn windows_fixture_templates_are_unambiguous() {
        let ts: Vec<ExecTemplate> = WIN_TEMPLATES
            .iter()
            .map(|s| parse_template(s).unwrap_or_else(|e| panic!("{s}: {e}")))
            .collect();
        // 各场景的 argv 形态（S1/SS1、SS7、SS8、S4、s1_cat、S7、S6/SS2、
        // S9、S3）在夹具里都恰命中 1 条。
        for argv in [
            vec!["cmd", "/c", "echo", "hello"],
            vec!["cmd", "/c", "echo", "x"],
            vec!["cmd", "/c", "echo", "compat"],
            vec!["cmd", "/c", "echo", "%OJ_EXEC_TEST_ON%"],
            vec!["cmd", "/c", "type", "C:\\jail\\in.txt"],
            vec!["cmd", "/c", "exit", "3"],
            vec!["ping", "-n", "31", "127.0.0.1"],
            vec!["oj", "/c", "echo", "ping"],
            vec!["sh", "-c", "script"],
        ] {
            let a = sargv(&argv);
            let got = match_all(&ts, &a);
            assert!(
                got.is_ok(),
                "argv {argv:?} 在夹具里未恰命中 1 条模板: {got:?}"
            );
        }
        // 反例钉死本次踩到的语义：同一位置的裸 raw 与 `%…%` 包裹 raw 天然
        // 重叠——`%X%` 形态恒 2 命中（ambiguous），非 `%…%` 形态只命中裸 raw。
        let overlapping = vec![p("cmd /c echo ${raw:word}"), p("cmd /c echo %${raw:NAME}%")];
        assert_eq!(
            match_all(&overlapping, &sargv(&["cmd", "/c", "echo", "%VAR%"])),
            Err(MatchError::Ambiguous { count: 2 })
        );
        let (i, b) = match_all(&overlapping, &sargv(&["cmd", "/c", "echo", "hi"])).unwrap();
        assert_eq!(i, 0);
        assert_eq!(b, vec![("word".to_string(), "hi".to_string())]);
        // 包裹 raw 的校验作用在**中间子串**上：`%…%` 内的 option-like /
        // 绝对形态仍被拒（Windows 面删掉裸 echo 模板后，该覆盖只在此处）。
        let t = p("cmd /c echo %${raw:NAME}%");
        for bad in ["-x", "/abs", "C:\\x"] {
            let a = sargv(&["cmd", "/c", "echo", &format!("%{bad}%")]);
            let e = match_template(&t, &a).unwrap_err();
            assert!(e.contains("${raw:NAME} rejects"), "{bad} -> {e}");
        }
    }

    // ----- 默认命令解析器（"oj" 特判 / PATH 解析 / 未知名点名） -----

    #[test]
    fn default_resolver_oj_and_path() {
        // "oj" → current_exe（serve 进程即 bin/oj；测试里即测试二进制自身）。
        let oj = default_resolve_command("oj").unwrap();
        assert_eq!(oj, std::env::current_exe().unwrap());
        // 裸名 → 父进程 PATH 解析为绝对路径。探测名取「该平台恒在」者：
        // unix 用 sh；Windows 无 sh，用 System32 常在 PATH 的 cmd.exe——带扩展名
        // 是因为 default_resolve_command 的 Windows 分支只 is_file()、不探测
        // PATHEXT（裸名 cmd 匹配不到 cmd.exe；该缺口另立 OJ-8，不混进本测试）。
        #[cfg(unix)]
        let probe = "sh";
        #[cfg(windows)]
        let probe = "cmd.exe";
        let resolved = default_resolve_command(probe).unwrap();
        assert!(resolved.is_absolute(), "{probe} -> {resolved:?}");
        // 未知命令 → Err 点名。
        let e = default_resolve_command("oj-no-such-command-xyz").unwrap_err();
        assert!(e.contains("not found in PATH"), "{e}");
        // 路径分隔符 → Err（parse 期已拒，此处防御性兜底）。
        assert!(default_resolve_command("./sh").is_err());
    }

    // ----- BDD S1–S12（真实子进程；注入解析器 + 直构 grants，经 run_module 归属模块） -----

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("oj-exectest-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 注入解析器：名字 → 映射绝对路径；记录被查询的名字（S9 断言用）。
    fn test_resolver(map: &[(&str, &str)]) -> (CommandResolver, Arc<Mutex<Vec<String>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let m: HashMap<String, String> = map
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let seen2 = seen.clone();
        (
            Arc::new(move |name: &str| {
                seen2.lock().unwrap().push(name.to_string());
                m.get(name)
                    .cloned()
                    .map(PathBuf::from)
                    .ok_or_else(|| format!("command '{name}' not found"))
            }),
            seen,
        )
    }

    /// 路径嵌进 JS 双引号串字面量前的转义：canonicalize 在 Windows 得
    /// verbatim `\\?\C:\...`，反斜杠会被 JS 字符串当转义吃掉（`\r`→CR、
    /// `\U`→U……）毁掉路径（同 oj/src/app.rs 测试 2026-10-17 的坑）。
    /// unix 路径无反斜杠，no-op。
    fn js_path(p: &Path) -> String {
        p.display().to_string().replace('\\', "\\\\")
    }

    /// 全场景 handler 表（unix 命令形态；Windows 对应面见 `api_src_win`）。
    /// 占位符经 replace 注入实际路径/变量名。
    fn api_src(jail: &Path, outside: &Path, var_on: &str, var_off: &str) -> String {
        r#"
export default {
  s1_echo: async () => json.ok(await exec.run(["echo", "hello"])),
  s1_cat: async () => json.ok(await exec.run(["cat", "__JAIL__/in.txt"])),
  s2_inject: async () => json.ok(await exec.run(["tar", "--use-compress-program=x", "-xzf", "__JAIL__/a.tgz", "-C", "__JAIL__/dest"])),
  s3_escape: async () => json.ok(await exec.run(["cat", "__OUT__/secret.txt"])),
  s3_raw_opt: async () => json.ok(await exec.run(["sh", "-c", "-x"])),
  s3_raw_abs: async () => json.ok(await exec.run(["sh", "-c", "/abs"])),
  s4_env_on: async () => json.ok(await exec.run(["printenv", "__VON__"])),
  s4_env_off: async () => json.ok(await exec.run(["printenv", "__VOFF__"])),
  s5_big: async () => json.ok(await exec.run(["sh", "-c", "echo $$ > __JAIL__/t5.pid; cat __JAIL__/big.bin"])),
  s6_timeout: async () => json.ok(await exec.run(["sh", "-c", "echo $$ > __JAIL__/t6.pid; sleep 30 & wait"], { timeoutMs: 400 })),
  s7_exit3: async () => json.ok(await exec.run(["sh", "-c", "exit 3"])),
  s7_signal: async () => json.ok(await exec.run(["sh", "-c", "kill -TERM $$"])),
  s9_oj: async () => json.ok(await exec.run(["oj", "ping"])),
  s10_amb: async () => json.ok(await exec.run(["echo", "hi"])),
  s12_badutf8: async () => json.ok(await exec.run(["sh", "-c", "printf '\\200' >&2"])),
};
"#
        .replace("__JAIL__", &js_path(jail))
        .replace("__OUT__", &js_path(outside))
        .replace("__VON__", var_on)
        .replace("__VOFF__", var_off)
    }

    fn make_bridge(
        base: &Path,
        modules: &[(&str, &str)],
        api_js: &str,
        fs_root: &Path,
        exec: Option<ExecGrant>,
        platform: Option<PlatformGrant>,
    ) -> Bridge {
        let mut map = HashMap::new();
        for (d, n) in modules {
            let dir = base.join(d);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("api.js"), api_js).unwrap();
            map.insert(
                dir.to_string_lossy().into_owned(),
                ModuleCtx {
                    name: n.to_string(),
                    version: "0.1.0".into(),
                    deps: Arc::new(HashSet::new()),
                    db: None,
                },
            );
        }
        Bridge::with_dbs_and_loader(
            HashMap::new(),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            Some(Arc::new(LoaderShared {
                project_root: base.to_path_buf(),
                ts: false,
            })),
            Extras {
                fs: Some(Arc::new(FsGrant::new(fs_root, false).unwrap())),
                platform: platform.map(Arc::new),
                exec: exec.map(Arc::new),
                modules: Arc::new(map),
                ..Default::default()
            },
        )
    }

    /// worker 模块的模板表（unix 形态：echo/cat/tar/sh/printenv/oj）。
    /// Windows 对应面见 `worker_grant_win`。
    #[cfg(unix)]
    fn worker_grant(env: Vec<String>, resolver: CommandResolver) -> ExecGrant {
        ExecGrant::with_resolver(
            [(
                "worker".to_string(),
                (
                    vec![
                        "echo ${raw:word}".to_string(),
                        "cat ${path:file}".to_string(),
                        "tar -xzf ${path:archive} -C ${path:dest}".to_string(),
                        "sh -c ${raw:script}".to_string(),
                        "printenv ${raw:NAME}".to_string(),
                        "oj ${raw:msg}".to_string(),
                    ],
                    env,
                ),
            )]
            .into(),
            resolver,
        )
    }

    // S1–S12 主场景（unix 命令形态；Windows 对应面见
    // `exec_bdd_main_scenarios_windows`——契约断言同源，差异仅限平台
    // 命令形态：CRLF、echo %VAR% 字面量回落、ping 代 sleep、无信号/组杀）。
    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn exec_bdd_main_scenarios() {
        // macOS temp_dir 经 symlink：统一 canonical（前缀比对防形态打穿）。
        let jail = tmp("jail").canonicalize().unwrap();
        let outside = tmp("outside").canonicalize().unwrap();
        std::fs::write(jail.join("in.txt"), b"hello-jail").unwrap();
        std::fs::write(outside.join("secret.txt"), b"secret").unwrap();
        std::fs::write(jail.join("big.bin"), vec![b'a'; OUTPUT_LIMIT + 1]).unwrap();

        // env 白名单夹具：白名单内 / 外各一个宿主变量（printenv 断言可见性）。
        let var_on = format!("OJ_EXEC_TEST_ON_{}", std::process::id());
        let var_off = format!("OJ_EXEC_TEST_OFF_{}", std::process::id());
        // SAFETY: 测试夹具（对齐 plugin_loader/tests.rs 惯例）；变量名带 pid 唯一。
        unsafe { std::env::set_var(&var_on, "oj-exec-visible") };
        unsafe { std::env::set_var(&var_off, "oj-exec-hidden") };

        let base = tmp("mods").canonicalize().unwrap();
        let api_js = api_src(&jail, &outside, &var_on, &var_off);
        let (resolver, seen) = test_resolver(&[
            ("echo", "/bin/echo"),
            ("cat", "/bin/cat"),
            ("sh", "/bin/sh"),
            ("printenv", "/usr/bin/printenv"),
            ("oj", "/bin/echo"),
        ]);
        let b = make_bridge(
            &base,
            &[("worker", "worker"), ("plain", "plain")],
            &api_js,
            &jail,
            Some(worker_grant(vec![var_on.clone()], resolver)),
            Some(PlatformGrant::new(vec!["worker".into()], &[], Some(&jail)).unwrap()),
        );
        let api = |m: &str| base.join(m).join("api.js");
        let t = std::time::Duration::from_secs(60);

        macro_rules! data {
            ($h:expr) => {{
                let cap = b
                    .run_module(&api("worker"), $h, Default::default(), t)
                    .await
                    .unwrap_or_else(|e| panic!("{} failed: {e}", $h));
                let v: serde_json::Value = serde_json::from_slice(&cap.body)
                    .unwrap_or_else(|e| panic!("{} envelope: {e}", $h));
                assert_eq!(v["code"], 0, "{}: {}", $h, v["msg"]);
                v["data"].clone()
            }};
        }
        macro_rules! denied {
            // NotCapable 类失败（未授权 / 模板失配 / 歧义 / 占位符校验）。
            ($h:expr, $part:expr) => {{
                let e = b
                    .run_module(&api("worker"), $h, Default::default(), t)
                    .await
                    .unwrap_err();
                let m = e.to_string();
                assert!(
                    m.contains("NotCapable") && m.contains($part),
                    "{} -> {m}",
                    $h
                );
            }};
        }
        macro_rules! fails {
            // 环境/运行期失败（超时 / 输出超限 / 解码），非 NotCapable 类。
            ($h:expr, $part:expr) => {{
                let e = b
                    .run_module(&api("worker"), $h, Default::default(), t)
                    .await
                    .unwrap_err();
                let m = e.to_string();
                assert!(m.contains($part), "{} -> {m}", $h);
            }};
        }

        // S1：合法模板 + jail 内路径 → code=0，stdout 正确。
        let d = data!("s1_echo");
        assert_eq!(d["code"], 0);
        assert_eq!(d["stdout"], "hello\n");
        let d = data!("s1_cat");
        assert_eq!(d["code"], 0);
        assert_eq!(d["stdout"], "hello-jail");

        // S2：literal 被替换（argv 夹 --use-compress-program）→ 模板失配 NotCapable。
        denied!("s2_inject", "template mismatch");

        // S3：${path:} 越界（绝对路径逃出集合）→ NotCapable；${raw:} 收 `-x` / `/abs` → 拒绝。
        denied!("s3_escape", "path outside fs.root");
        denied!("s3_raw_opt", "option-like");
        denied!("s3_raw_abs", "absolute-form");

        // S4：env 白名单外不可见（printenv 退出码 1 + 空 stdout）/ 白名单内值透传。
        let d = data!("s4_env_on");
        assert_eq!(d["code"], 0);
        assert_eq!(d["stdout"], "oj-exec-visible\n");
        let d = data!("s4_env_off");
        assert_eq!(d["code"], 1);
        assert_eq!(d["stdout"], "");

        // S5：输出 >10MB → 抛错点名（已收部分丢弃）+ 组杀无残留。
        fails!("s5_big", "output limit exceeded");
        #[cfg(unix)]
        wait_group_gone(read_pid(&jail, "t5.pid"), "s5").await;

        // S6：timeoutMs 到期 → 抛错点名 + 组杀无残留。
        fails!("s6_timeout", "timed out");
        #[cfg(unix)]
        wait_group_gone(read_pid(&jail, "t6.pid"), "s6").await;

        // S7：非 0 退出 → 返回 {code:非0} 不抛；信号终止 → code=128+n（TERM=15）。
        let d = data!("s7_exit3");
        assert_eq!(d["code"], 3);
        let d = data!("s7_signal");
        assert_eq!(d["code"], 143);

        // S9："oj" 模板 → 解析器以 "oj" 调用（注入哨兵映射 → /bin/echo spawn 成功）。
        let d = data!("s9_oj");
        assert_eq!(d["stdout"], "ping\n");
        assert!(
            seen.lock().unwrap().iter().any(|n| n == "oj"),
            "resolver calls: {:?}",
            seen.lock().unwrap()
        );

        // S12：stderr 非 UTF-8 → 抛错点名（from_utf8_lossy 不接受）。
        {
            let e = b
                .run_module(&api("worker"), "s12_badutf8", Default::default(), t)
                .await
                .unwrap_err();
            let m = e.to_string();
            assert!(m.contains("stderr") && m.contains("UTF-8"), "{m}");
        }

        // S8：非特权模块（plain 不在 exec 授权表）→ NotCapable。
        {
            let e = b
                .run_module(&api("plain"), "s1_echo", Default::default(), t)
                .await
                .unwrap_err();
            let m = e.to_string();
            assert!(
                m.contains("NotCapable") && m.contains("not authorized for exec"),
                "{m}"
            );
        }

        let _ = std::fs::remove_dir_all(&jail);
        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&base);
    }

    // ----- BDD Windows 对应面（cmd.exe 形态；契约断言与 unix 面同源） -----
    //
    // 平台差异均为「命令语义」而非契约差异：
    // - echo/type/exit 经 cmd /c（Windows 无独立 echo/cat 可执行）；
    // - cmd 的 echo 输出 CRLF、type 不补行尾 → stdout 断言一律经下方
    //   「行尾真相源」（IN_TXT / echo_out）拼装；
    // - 白名单外变量经 echo %VAR% 原样打出字面量（printenv 的 exit 1
    //   无对应物）→ 断言「值不可见」而非「空输出」；
    // - 超时用 ping -n 31（≈30s；timeout.exe 不支持输入重定向会立即退出）；
    // - 信号终止 / 非 UTF-8 stderr / 组杀残留无 Windows 对应物（kill_tree
    //   仅杀直接子进程，api-manual §6 已声明）——留 unix 面覆盖，编解码与
    //   模板账本逻辑本身平台无关。

    // ----- Windows 夹具的「行尾真相源」 -----
    // cmd 家族有两条输出约定，缺一不可（违反任一条都会让断言变红）：
    // - `echo` 的输出一律以 CRLF 收尾；
    // - `type` 原样吐文件字节，**不**补行尾（文件无行尾 → 输出亦无行尾）。
    // 因此所有 stdout / out 断言一律经下面这两个构造器拼装，**禁止在断言里
    // 手写 "\r\n" / "\n"**：行尾字面散落在 8 处时，只能逐个被 CI 打出来，
    // 修完一处下一处接着红（v0.1.79 前已反复发生）。

    /// in.txt 夹具内容：`type` 原样吐字节，故 `s1_cat` 的期望值必须与写入
    /// 内容**同源**（此前断言写死 "hello-jail\r\n"、文件写的却是无行尾的
    /// "hello-jail"，两处一旦漂移就红）。
    #[cfg(windows)]
    const IN_TXT: &str = "hello-jail";

    /// `cmd /c echo <s>` 的输出（echo 恒以 CRLF 收尾）。
    #[cfg(windows)]
    fn echo_out(s: &str) -> String {
        format!("{s}\r\n")
    }

    /// Windows 场景 handler 表（占位符经 replace 注入实际路径/变量名）。
    #[cfg(windows)]
    fn api_src_win(jail: &Path, outside: &Path, var_on: &str, var_off: &str) -> String {
        r#"
export default {
  s1_echo: async () => json.ok(await exec.run(["cmd", "/c", "echo", "hello"])),
  s1_cat: async () => json.ok(await exec.run(["cmd", "/c", "type", "__JAIL__/in.txt"])),
  s2_inject: async () => json.ok(await exec.run(["cmd", "--bad", "/c", "type", "__JAIL__/in.txt"])),
  s3_escape: async () => json.ok(await exec.run(["cmd", "/c", "type", "__OUT__/secret.txt"])),
  s3_raw_opt: async () => json.ok(await exec.run(["sh", "-c", "-x"])),
  s3_raw_abs: async () => json.ok(await exec.run(["sh", "-c", "/abs"])),
  s4_env_on: async () => json.ok(await exec.run(["cmd", "/c", "echo", "%__VON__%"])),
  s4_env_off: async () => json.ok(await exec.run(["cmd", "/c", "echo", "%__VOFF__%"])),
  // 10MiB 经 cmd /c type 吐出的速率受机器负载与实时防护扫描影响，显式放宽到
  // 40s（仍在下方用例 60s 窗口内）：否则 30s 缺省可能先到，报 timed out 而
  // 拿不到本场景要点的 output limit exceeded 文案。
  s5_big: async () => json.ok(await exec.run(["cmd", "/c", "type", "__JAIL__/big.bin"], { timeoutMs: 40000 })),
  s6_timeout: async () => json.ok(await exec.run(["ping", "-n", "31", "127.0.0.1"], { timeoutMs: 400 })),
  s7_exit3: async () => json.ok(await exec.run(["cmd", "/c", "exit", "3"])),
  s9_oj: async () => json.ok(await exec.run(["oj", "/c", "echo", "ping"])),
  s10_amb: async () => json.ok(await exec.run(["echo", "hi"])),
};
"#
        .replace("__JAIL__", &js_path(jail))
        .replace("__OUT__", &js_path(outside))
        .replace("__VON__", var_on)
        .replace("__VOFF__", var_off)
    }

    /// worker 模块的模板表（Windows 形态：cmd /c 家族 + ping）。sh -c 条目
    /// 仅供 S3 的 ${raw:} pre-spawn 拒绝——匹配期即拒，"sh" 无需在 Windows 存在。
    ///
    /// **平台无关常量**（unix 上也编译）：供
    /// [`windows_fixture_templates_are_unambiguous`] 直接对**真夹具**做匹配
    /// 断言。Windows 夹具在本机既编不到也跑不了，只能等 CI 打脸——这类缺陷
    /// 已反复漏网（v0.1.79 两次），故把「夹具自身不歧义」钉在 unix 也能跑的
    /// 地方。
    const WIN_TEMPLATES: &[&str] = &[
        // `cmd /c echo` 家族刻意每条一个**全字面**模板，而不是一条
        // `cmd /c echo ${raw:word}`：后者会与 S4 用的 `cmd /c echo
        // %${raw:NAME}%` 天然重叠（%…% 形态是裸 raw 的子集），任何 `%X%`
        // argv 都同时命中两条 → 恒报 `exec template ambiguous`（生产语义是
        // 歧义拒绝、不静默择一）。裸 raw 的覆盖不靠它——`oj /c echo
        // ${raw:msg}`（resolver 同指 cmd.exe）、`cmd /c exit ${raw:code}`、
        // `ping -n ${raw:count}`、`sh -c ${raw:script}` 已覆盖正向与负向。
        "cmd /c echo hello",
        "cmd /c echo x",
        "cmd /c echo compat",
        "cmd /c echo %${raw:NAME}%",
        "cmd /c type ${path:file}",
        "cmd /c exit ${raw:code}",
        "ping -n ${raw:count} 127.0.0.1",
        "oj /c echo ${raw:msg}",
        "sh -c ${raw:script}",
    ];

    #[cfg(windows)]
    fn worker_grant_win(env: Vec<String>, resolver: CommandResolver) -> ExecGrant {
        ExecGrant::with_resolver(
            [(
                "worker".to_string(),
                (WIN_TEMPLATES.iter().map(|s| s.to_string()).collect(), env),
            )]
            .into(),
            resolver,
        )
    }

    /// 跑一个 handler 取 data（envelope code 必须为 0）。
    #[cfg(windows)]
    async fn data_of(b: &Bridge, api: &Path, h: &str) -> serde_json::Value {
        let cap = b
            .run_module(
                api,
                h,
                Default::default(),
                std::time::Duration::from_secs(60),
            )
            .await
            .unwrap_or_else(|e| panic!("{h} failed: {e}"));
        let v: serde_json::Value =
            serde_json::from_slice(&cap.body).unwrap_or_else(|e| panic!("{h} envelope: {e}"));
        assert_eq!(v["code"], 0, "{}: {}", h, v["msg"]);
        v["data"].clone()
    }

    /// NotCapable 类失败（未授权 / 模板失配 / 占位符校验）。
    #[cfg(windows)]
    async fn denied_of(b: &Bridge, api: &Path, h: &str, part: &str) {
        let e = b
            .run_module(
                api,
                h,
                Default::default(),
                std::time::Duration::from_secs(60),
            )
            .await
            .unwrap_err();
        let m = e.to_string();
        assert!(m.contains("NotCapable") && m.contains(part), "{h} -> {m}");
    }

    /// 环境/运行期失败（超时 / 输出超限），非 NotCapable 类。
    #[cfg(windows)]
    async fn fails_of(b: &Bridge, api: &Path, h: &str, part: &str) {
        let e = b
            .run_module(
                api,
                h,
                Default::default(),
                std::time::Duration::from_secs(60),
            )
            .await
            .unwrap_err();
        let m = e.to_string();
        assert!(m.contains(part), "{h} -> {m}");
    }

    #[cfg(windows)]
    #[tokio::test(flavor = "current_thread")]
    async fn exec_bdd_main_scenarios_windows() {
        let jail = tmp("jail").canonicalize().unwrap();
        let outside = tmp("outside").canonicalize().unwrap();
        std::fs::write(jail.join("in.txt"), IN_TXT.as_bytes()).unwrap();
        std::fs::write(outside.join("secret.txt"), b"secret").unwrap();
        std::fs::write(jail.join("big.bin"), vec![b'a'; OUTPUT_LIMIT + 1]).unwrap();

        // env 白名单夹具：白名单内 / 外各一个宿主变量（echo %VAR% 断言可见性）。
        let var_on = format!("OJ_EXEC_TEST_ON_{}", std::process::id());
        let var_off = format!("OJ_EXEC_TEST_OFF_{}", std::process::id());
        // SAFETY: 测试夹具（对齐 plugin_loader/tests.rs 惯例）；变量名带 pid 唯一。
        unsafe { std::env::set_var(&var_on, "oj-exec-visible") };
        unsafe { std::env::set_var(&var_off, "oj-exec-hidden") };

        let base = tmp("mods").canonicalize().unwrap();
        let api_js = api_src_win(&jail, &outside, &var_on, &var_off);
        // SystemRoot 直取（不依赖 PATH 形态）；"oj" 映射到 cmd.exe 得确定输出。
        let sysroot = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
        let cmd_exe = std::path::Path::new(&sysroot)
            .join("System32")
            .join("cmd.exe")
            .to_string_lossy()
            .into_owned();
        let ping_exe = std::path::Path::new(&sysroot)
            .join("System32")
            .join("ping.exe")
            .to_string_lossy()
            .into_owned();
        let (resolver, seen) = test_resolver(&[
            ("cmd", cmd_exe.as_str()),
            ("ping", ping_exe.as_str()),
            ("oj", cmd_exe.as_str()),
        ]);
        let b = make_bridge(
            &base,
            &[("worker", "worker"), ("plain", "plain")],
            &api_js,
            &jail,
            Some(worker_grant_win(vec![var_on.clone()], resolver)),
            Some(PlatformGrant::new(vec!["worker".into()], &[], Some(&jail)).unwrap()),
        );
        let api = base.join("worker").join("api.js");
        let plain_api = base.join("plain").join("api.js");

        // S1：合法模板 + jail 内路径 → code=0，stdout 正确（cmd echo 为 CRLF）。
        let d = data_of(&b, &api, "s1_echo").await;
        assert_eq!(d["code"], 0);
        assert_eq!(d["stdout"], echo_out("hello"));
        let d = data_of(&b, &api, "s1_cat").await;
        assert_eq!(d["code"], 0);
        assert_eq!(d["stdout"], IN_TXT);

        // S2：literal 被替换（argv 夹 --bad）→ 模板失配 NotCapable。
        denied_of(&b, &api, "s2_inject", "template mismatch").await;

        // S3：${path:} 越界 → NotCapable；${raw:} 收 -x / /abs → 拒绝。
        denied_of(&b, &api, "s3_escape", "path outside fs.root").await;
        denied_of(&b, &api, "s3_raw_opt", "option-like").await;
        denied_of(&b, &api, "s3_raw_abs", "absolute-form").await;

        // S4：白名单内值可见；白名单外不可见（cmd 对未定义变量原样打出
        // %NAME% 字面量 → 断言「值不出现」而非「空输出」）。
        let d = data_of(&b, &api, "s4_env_on").await;
        assert_eq!(d["code"], 0);
        assert_eq!(d["stdout"], echo_out("oj-exec-visible"));
        let d = data_of(&b, &api, "s4_env_off").await;
        assert!(
            !d["stdout"].as_str().unwrap().contains("oj-exec-hidden"),
            "{d}"
        );

        // S5：输出 >10MB → 抛错点名。
        fails_of(&b, &api, "s5_big", "output limit exceeded").await;

        // S6：timeoutMs 到期 → 抛错点名（ping -n 31 ≈ 30s；Windows 无进程组，
        // kill_tree 仅杀直接子进程）。
        fails_of(&b, &api, "s6_timeout", "timed out").await;

        // S7：非 0 退出 → 返回 {code:非0} 不抛（信号终止无 Windows 对应物）。
        let d = data_of(&b, &api, "s7_exit3").await;
        assert_eq!(d["code"], 3);

        // S9："oj" 模板 → 解析器以 "oj" 调用（注入 cmd.exe，stdout 确定）。
        let d = data_of(&b, &api, "s9_oj").await;
        assert_eq!(d["stdout"], echo_out("ping"));
        assert!(
            seen.lock().unwrap().iter().any(|n| n == "oj"),
            "resolver calls: {:?}",
            seen.lock().unwrap()
        );

        // S8：非特权模块（plain 不在 exec 授权表）→ NotCapable。
        denied_of(&b, &plain_api, "s1_echo", "not authorized for exec").await;

        let _ = std::fs::remove_dir_all(&jail);
        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&base);
    }

    // S10：歧义双模板 → 报错点名（不静默择一）。
    // S11：未配 platform.exec → NotCapable。
    #[tokio::test(flavor = "current_thread")]
    async fn exec_bdd_ambiguity_and_unconfigured() {
        let jail = tmp("jail2").canonicalize().unwrap();
        let outside = tmp("outside2").canonicalize().unwrap();
        std::fs::write(outside.join("secret.txt"), b"s").unwrap();
        let base = tmp("mods2").canonicalize().unwrap();
        let api_js = api_src(&jail, &outside, "NO_VAR_ON", "NO_VAR_OFF");
        let t = std::time::Duration::from_secs(60);

        // S10：同一 argv 命中两条模板 → 歧义拒绝。
        let (resolver, _seen) = test_resolver(&[("echo", "/bin/echo")]);
        let amb = ExecGrant::with_resolver(
            [(
                "worker".to_string(),
                (
                    vec!["echo ${raw:a}".to_string(), "echo ${raw:b}".to_string()],
                    vec![],
                ),
            )]
            .into(),
            resolver,
        );
        let b = make_bridge(
            &base,
            &[("worker", "worker")],
            &api_js,
            &jail,
            Some(amb),
            Some(PlatformGrant::new(vec![], &[], Some(&jail)).unwrap()),
        );
        let e = b
            .run_module(
                &base.join("worker").join("api.js"),
                "s10_amb",
                Default::default(),
                t,
            )
            .await
            .unwrap_err();
        let m = e.to_string();
        assert!(m.contains("NotCapable") && m.contains("ambiguous"), "{m}");

        // S11：未配 exec（StableState.exec = None）→ NotCapable（fail-closed）。
        let b2 = make_bridge(&base, &[("worker", "worker")], &api_js, &jail, None, None);
        let e = b2
            .run_module(
                &base.join("worker").join("api.js"),
                "s1_echo",
                Default::default(),
                t,
            )
            .await
            .unwrap_err();
        let m = e.to_string();
        assert!(
            m.contains("NotCapable") && m.contains("exec not configured"),
            "{m}"
        );

        let _ = std::fs::remove_dir_all(&jail);
        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&base);
    }

    // ----- BDD SS1-SS9 流式（spawn 句柄 / 回调 / 向后兼容；真实子进程） -----
    // 复用本模块既有夹具：make_bridge / test_resolver / worker_grant /
    // worker_grant_win / tmp / js_path。平台差异仅限命令形态（同 S 主场景）。

    /// 流式场景 handler 表（unix 命令形态）。占位符 __BIG__ 注入 jail/big.bin。
    #[cfg(unix)]
    fn api_src_stream(jail: &Path) -> String {
        let big = js_path(&jail.join("big.bin"));
        r#"
export default {
  s1_spawn: async () => {
    const h = await exec.spawn(["echo", "hello"]);
    let out = "";
    for await (const c of h.stdout) out += new TextDecoder().decode(c);
    const { code } = await h.exit();
    return json.ok({
      rid: h.rid,
      pid: h.pid,
      hasStdout: typeof h.stdout[Symbol.asyncIterator] === "function",
      hasStderr: typeof h.stderr[Symbol.asyncIterator] === "function",
      code,
      out,
    });
  },
  s2_realtime: async () => {
    const parts = [];
    await exec.run(["sh", "-c", "echo a; sleep 0.15; echo b"], {
      onStdout: (c) => parts.push(new TextDecoder().decode(c)),
    });
    return json.ok({ parts: parts.join("") });
  },
  s3_exit: async () => {
    const h = await exec.spawn(["sh", "-c", "exit 7"]);
    for await (const _ of h.stdout) {}
    const { code } = await h.exit();
    return json.ok({ code });
  },
  s4_kill: async () => {
    const h = await exec.spawn(["sh", "-c", "sleep 30"]);
    h.kill();
    const { code } = await h.exit();
    return json.ok({ code });
  },
  s5_big: async () => {
    const h = await exec.spawn(["sh", "-c", "cat __BIG__"]);
    let n = 0;
    for await (const c of h.stdout) n += c.byteLength;
    const { code } = await h.exit();
    return json.ok({ code, bytes: n });
  },
  s6_timeout: async () => {
    const h = await exec.spawn(["sh", "-c", "sleep 30"], { timeoutMs: 400 });
    for await (const _ of h.stdout) {}
    const { code } = await h.exit();
    return json.ok({ code });
  },
  s7_callback: async () => {
    const parts = [];
    const r = await exec.run(["sh", "-c", "echo x; echo y"], {
      onStdout: (c) => parts.push(new TextDecoder().decode(c)),
    });
    return json.ok({ code: r.code, out: parts.join(""), full: r.stdout });
  },
  s8_compat: async () => {
    const r = await exec.run(["echo", "compat"]);
    return json.ok({ code: r.code, out: r.stdout });
  },
  s9_unauth: async () => {
    await exec.spawn(["echo", "hi"]);
    return json.ok({});
  },
};
"#
        .replace("__BIG__", &big)
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn exec_bdd_streaming_scenarios() {
        let jail = tmp("stream-jail").canonicalize().unwrap();
        std::fs::write(jail.join("big.bin"), vec![b'a'; OUTPUT_LIMIT + 1]).unwrap();
        let base = tmp("stream-mods").canonicalize().unwrap();
        let api_js = api_src_stream(&jail);
        let (resolver, _seen) = test_resolver(&[
            ("echo", "/bin/echo"),
            ("cat", "/bin/cat"),
            ("sh", "/bin/sh"),
            ("oj", "/bin/echo"),
        ]);
        let b = make_bridge(
            &base,
            &[("worker", "worker"), ("plain", "plain")],
            &api_js,
            &jail,
            Some(worker_grant(vec![], resolver)),
            Some(PlatformGrant::new(vec!["worker".into()], &[], Some(&jail)).unwrap()),
        );
        let api = |m: &str| base.join(m).join("api.js");
        let t = std::time::Duration::from_secs(60);

        macro_rules! data {
            ($h:expr) => {{
                let cap = b
                    .run_module(&api("worker"), $h, Default::default(), t)
                    .await
                    .unwrap_or_else(|e| panic!("{} failed: {e}", $h));
                let v: serde_json::Value = serde_json::from_slice(&cap.body)
                    .unwrap_or_else(|e| panic!("{} envelope: {e}", $h));
                assert_eq!(v["code"], 0, "{}: {}", $h, v["msg"]);
                v["data"].clone()
            }};
        }
        macro_rules! denied {
            ($m:expr, $h:expr, $part:expr) => {{
                let e = b
                    .run_module(&api($m), $h, Default::default(), t)
                    .await
                    .unwrap_err();
                let m = e.to_string();
                assert!(
                    m.contains("NotCapable") && m.contains($part),
                    "{} -> {m}",
                    $h
                );
            }};
        }

        // SS1：spawn 返回句柄，含 rid/pid 与两个 AsyncIterable 流，stdout 增量正确。
        let d = data!("s1_spawn");
        assert!(d["rid"].is_u64(), "rid: {d}");

        // SS2：回调形态实时收到分块（echo a / sleep / echo b 串联）。
        let d = data!("s2_realtime");
        let p = d["parts"].as_str().unwrap();
        assert!(
            p.contains(
                "a
"
            ),
            "{p}"
        );
        assert!(
            p.contains(
                "b
"
            ),
            "{p}"
        );

        // SS3：h.exit() 返回退出码（exit 7）。
        let d = data!("s3_exit");
        assert_eq!(d["code"], 7);

        // SS4：h.kill() 中途击杀长睡任务 -> SIGKILL(137)。
        let d = data!("s4_kill");
        assert_eq!(d["code"], 137);

        // SS5：输出 >10MiB 触发 OUTPUT_LIMIT 组杀 -> 137，且至少流出部分数据。
        let d = data!("s5_big");
        assert_eq!(d["code"], 137);
        assert!(d["bytes"].as_u64().unwrap() > 1_000_000, "{}", d["bytes"]);

        // SS6：timeoutMs 短于命令 -> 超时组杀 -> 137。
        let d = data!("s6_timeout");
        assert_eq!(d["code"], 137);

        // SS7：回调形态 onStdout 累计 + 返回完整 stdout。
        let d = data!("s7_callback");
        assert_eq!(d["code"], 0);
        assert_eq!(
            d["out"],
            "x
y
"
        );
        assert_eq!(
            d["full"],
            "x
y
"
        );

        // SS8：向后兼容 exec.run(argv) 无 opts -> 缓冲 {code,stdout}。
        let d = data!("s8_compat");
        assert_eq!(d["code"], 0);
        assert_eq!(
            d["out"],
            "compat
"
        );

        // SS9：未授权模块 spawn -> NotCapable（fail-closed）。
        denied!("plain", "s9_unauth", "not authorized for exec");

        let _ = std::fs::remove_dir_all(&jail);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 流式场景 handler 表（Windows 命令形态；契约断言与 unix 面同源，
    /// 仅命令语义差异：cmd /c / ping，CRLF，击杀码为「非 0」而非固定 137）。
    #[cfg(windows)]
    fn api_src_stream_win(jail: &Path) -> String {
        let big = js_path(&jail.join("big.bin"));
        r#"
export default {
  s1_spawn: async () => {
    const h = await exec.spawn(["cmd", "/c", "echo", "hello"]);
    let out = "";
    for await (const c of h.stdout) out += new TextDecoder().decode(c);
    const { code } = await h.exit();
    return json.ok({
      rid: h.rid,
      pid: h.pid,
      hasStdout: typeof h.stdout[Symbol.asyncIterator] === "function",
      hasStderr: typeof h.stderr[Symbol.asyncIterator] === "function",
      code,
      out,
    });
  },
  s2_realtime: async () => {
    const parts = [];
    await exec.run(["ping", "-n", "3", "127.0.0.1"], {
      onStdout: (c) => parts.push(new TextDecoder().decode(c)),
    });
    return json.ok({ parts: parts.join("") });
  },
  s3_exit: async () => {
    const h = await exec.spawn(["cmd", "/c", "exit", "7"]);
    for await (const _ of h.stdout) {}
    const { code } = await h.exit();
    return json.ok({ code });
  },
  s4_kill: async () => {
    const h = await exec.spawn(["ping", "-n", "31", "127.0.0.1"]);
    h.kill();
    const { code } = await h.exit();
    return json.ok({ code });
  },
  s5_big: async () => {
    // 同主场景 s5_big：cmd /c type 吐 10MiB 的速率不可控，放宽超时窗（缺省
    // 30s 可能先到，把「超限击杀」误呈为「超时击杀」）。
    const h = await exec.spawn(["cmd", "/c", "type", "__BIG__"], { timeoutMs: 40000 });
    let n = 0;
    for await (const c of h.stdout) n += c.byteLength;
    const { code } = await h.exit();
    return json.ok({ code, bytes: n });
  },
  s6_timeout: async () => {
    const h = await exec.spawn(["ping", "-n", "31", "127.0.0.1"], { timeoutMs: 400 });
    for await (const _ of h.stdout) {}
    const { code } = await h.exit();
    return json.ok({ code });
  },
  s7_callback: async () => {
    const parts = [];
    const r = await exec.run(["cmd", "/c", "echo", "x"], {
      onStdout: (c) => parts.push(new TextDecoder().decode(c)),
    });
    return json.ok({ code: r.code, out: parts.join(""), full: r.stdout });
  },
  s8_compat: async () => {
    const r = await exec.run(["cmd", "/c", "echo", "compat"]);
    return json.ok({ code: r.code, out: r.stdout });
  },
  s9_unauth: async () => {
    await exec.spawn(["cmd", "/c", "echo", "hi"]);
    return json.ok({});
  },
};
"#
        .replace("__BIG__", &big)
    }

    #[cfg(windows)]
    #[tokio::test(flavor = "current_thread")]
    async fn exec_bdd_streaming_scenarios_windows() {
        let jail = tmp("stream-jail").canonicalize().unwrap();
        std::fs::write(jail.join("big.bin"), vec![b'a'; OUTPUT_LIMIT + 1]).unwrap();
        let base = tmp("stream-mods").canonicalize().unwrap();
        let api_js = api_src_stream_win(&jail);
        let sysroot = std::env::var("SystemRoot").unwrap_or_else(|_| "C:/Windows".to_string());
        let cmd_exe = std::path::Path::new(&sysroot)
            .join("System32")
            .join("cmd.exe")
            .to_string_lossy()
            .into_owned();
        let ping_exe = std::path::Path::new(&sysroot)
            .join("System32")
            .join("ping.exe")
            .to_string_lossy()
            .into_owned();
        let (resolver, _seen) = test_resolver(&[
            ("cmd", cmd_exe.as_str()),
            ("ping", ping_exe.as_str()),
            ("oj", cmd_exe.as_str()),
        ]);
        let b = make_bridge(
            &base,
            &[("worker", "worker"), ("plain", "plain")],
            &api_js,
            &jail,
            Some(worker_grant_win(vec![], resolver)),
            Some(PlatformGrant::new(vec!["worker".into()], &[], Some(&jail)).unwrap()),
        );
        let api = |m: &str| base.join(m).join("api.js");
        let t = std::time::Duration::from_secs(60);

        macro_rules! data {
            ($h:expr) => {{
                let cap = b
                    .run_module(&api("worker"), $h, Default::default(), t)
                    .await
                    .unwrap_or_else(|e| panic!("{} failed: {e}", $h));
                let v: serde_json::Value = serde_json::from_slice(&cap.body)
                    .unwrap_or_else(|e| panic!("{} envelope: {e}", $h));
                assert_eq!(v["code"], 0, "{}: {}", $h, v["msg"]);
                v["data"].clone()
            }};
        }
        macro_rules! denied {
            ($m:expr, $h:expr, $part:expr) => {{
                let e = b
                    .run_module(&api($m), $h, Default::default(), t)
                    .await
                    .unwrap_err();
                let m = e.to_string();
                assert!(
                    m.contains("NotCapable") && m.contains($part),
                    "{} -> {m}",
                    $h
                );
            }};
        }

        // SS1：spawn 返回句柄，含 rid/pid 与两个 AsyncIterable 流（CRLF 输出）。
        let d = data!("s1_spawn");
        assert!(d["rid"].is_u64(), "rid: {d}");
        assert!(d["pid"].is_u64(), "pid: {d}");
        assert_eq!(d["hasStdout"], true);
        assert_eq!(d["hasStderr"], true);
        assert_eq!(d["code"], 0);
        assert_eq!(d["out"], echo_out("hello"));

        // SS2：回调形态实时收到分块（ping 多行输出）。
        let d = data!("s2_realtime");
        assert!(!d["parts"].as_str().unwrap().is_empty(), "{}", d["parts"]);

        // SS3：h.exit() 返回退出码（exit 7）。
        let d = data!("s3_exit");
        assert_eq!(d["code"], 7);

        // SS4：h.kill() 中途击杀 -> 非 0（Windows 无信号码，仅判非 0）。
        let d = data!("s4_kill");
        assert_ne!(d["code"], 0);

        // SS5：输出 >10MiB 触发 OUTPUT_LIMIT 组杀 -> 非 0，且至少流出部分数据。
        let d = data!("s5_big");
        assert_ne!(d["code"], 0);
        assert!(d["bytes"].as_u64().unwrap() > 1_000_000, "{}", d["bytes"]);

        // SS6：timeoutMs 短于命令 -> 超时组杀 -> 非 0。
        let d = data!("s6_timeout");
        assert_ne!(d["code"], 0);

        // SS7：回调形态 onStdout 累计 + 返回完整 stdout。
        let d = data!("s7_callback");
        assert_eq!(d["code"], 0);
        assert_eq!(d["out"], echo_out("x"));
        assert_eq!(d["full"], echo_out("x"));

        // SS8：向后兼容 exec.run(argv) 无 opts -> 缓冲 {code,stdout}。
        let d = data!("s8_compat");
        assert_eq!(d["code"], 0);
        assert_eq!(d["out"], echo_out("compat"));

        // SS9：未授权模块 spawn -> NotCapable（fail-closed）。
        denied!("plain", "s9_unauth", "not authorized for exec");

        let _ = std::fs::remove_dir_all(&jail);
        let _ = std::fs::remove_dir_all(&base);
    }
}
