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
                    // 长度先判：arg 比前后缀合计还短时 starts/ends 仍双过（如
                    // arg "%" vs prefix/suffix 各 "%"），直接切片是 slice-index
                    // panic（单字符参数打穿宿主，v0.1.83）。
                    if a.len() < prefix.len() + suffix.len()
                        || !a.starts_with(prefix)
                        || !a.ends_with(suffix)
                    {
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

/// 句柄资源：持有 kill 信号发送端 + 退出码 watch 接收端 + pid。Drop 同步发
/// kill 信号（覆盖 JS op_close 释放 / checkin 资源回收 / runtime 异常丢弃如
/// heap-limit 不回池等全部路径），由 supervisor 任务据此整组击杀子进程，杜绝
/// 孤儿进程组（评审 CRITICAL 资源回收；v0.1.83 起 Drop 真实存在——此前只有
/// close()，注释承诺的 Drop 兜底缺失，异常丢弃路径会漏杀到超时为止）。
struct ExecHandleResource {
    kill_tx: mpsc::UnboundedSender<()>,
    exit: watch::Receiver<Option<i32>>,
}

impl Resource for ExecHandleResource {
    fn name(&self) -> Cow<'_, str> {
        Cow::Borrowed("exec_handle")
    }
}

impl Drop for ExecHandleResource {
    fn drop(&mut self) {
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
    let pid = child.id().unwrap_or(0);
    let stdout_pipe = child.stdout.take().expect("stdout piped");
    let stderr_pipe = child.stderr.take().expect("stderr piped");
    // stdin：与缓冲路径（op_exec_run）同语义——有则一次性写入后关闭。此前流式
    // 形态静默丢弃 opts.stdin（管道恒 null），`sh -c cat` 直接吃空输入还不报错
    // （v0.1.83）。阻塞写入（子进程不读 + >64KB）由 supervisor 超时组杀收尾。
    if let (Some(mut si), Some(data)) = (child.stdin.take(), opts.stdin) {
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let _ = si.write_all(data.as_bytes()).await;
            let _ = si.shutdown().await;
        });
    }

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
#[path = "exec_tests.rs"]
mod tests;
