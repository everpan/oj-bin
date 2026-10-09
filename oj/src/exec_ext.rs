//! exec 扩展（oj_exec_ext）：exec_bootstrap.js 注入 `args`/`console`/`log`（覆盖
//! bridge 默认 log 的 tracing 通道；终端输出按通道分离——console.log/info 原样
//! stdout，debug/warn/error 与 log.* 走 stderr——另可选 JSONL 落盘）。
//!
//! 注入时序（评审 H1）：extension 的 esm entry 在 `JsRuntime::new` 内部求值，
//! bootstrap 期读不到 new 之后才 put 的 OpState 值——故 args/log_file 走 options
//! 模式（同 bridge_ext 的 stable），经 state 闭包 put。

use deno_core::OpState;
use deno_core::op2;
use std::io::Write as _;

/// exec 运行时配置（options 模式注入；`oj exec --` 之后 argv + --log-file 句柄）。
pub struct ExecOptions {
    pub args: Vec<String>,
    pub log_file: Option<std::fs::File>,
}

/// JS `globalThis.args` 的数据源（OpState）。
pub struct ExecArgs(Vec<String>);

/// 日志落点：终端（按级别/来源分流 stdout|stderr）+ 可选 JSONL 双写。
pub struct ExecSink {
    log_file: Option<std::fs::File>,
    /// 写文件失败只告警一次，不刷 stderr。
    warn_failed: bool,
}

const LEVELS: [&str; 4] = ["DEBUG", "INFO", "WARN", "ERROR"];

pub(crate) fn level_label(level: u8) -> &'static str {
    LEVELS.get(level as usize).copied().unwrap_or("INFO")
}

/// 终端输出目标通道（v0.1.55 通道分离：结果走 stdout，诊断走 stderr）。
pub(crate) enum Target {
    Stdout,
    Stderr,
}

/// console.* 的终端行路由（纯函数，可测）：level 1（log/info）→ stdout **原样**
/// （无前缀，管道友好）；debug/warn/error → stderr 带级别标签（诊断不污染管道）。
pub(crate) fn console_line(level: u8, msg: &str) -> (Target, String) {
    match level {
        1 => (Target::Stdout, msg.to_string()),
        _ => (
            Target::Stderr,
            format!("{:<5}  {}", level_label(level), msg),
        ),
    }
}

/// log.*（zap 结构化日志）的终端行路由：**一律** stderr 带级别标签——它是日志
/// 不是结果，level 1 也进 stderr（否则字段 JSON 会混进管道输出）。
pub(crate) fn log_line(level: u8, msg: &str) -> (Target, String) {
    (
        Target::Stderr,
        format!("{:<5}  {}", level_label(level), msg),
    )
}

/// JSONL 行（与 server 落盘字段同形：ts/level/msg）。
pub(crate) fn jsonl_line(level: &str, msg: &str) -> String {
    let ts = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();
    format!(
        "{{\"ts\":\"{ts}\",\"level\":\"{level}\",\"msg\":{}}}",
        serde_json::to_string(msg).unwrap_or_default()
    )
}

impl ExecSink {
    pub fn new(log_file: Option<std::fs::File>) -> Self {
        Self {
            log_file,
            warn_failed: false,
        }
    }

    /// console.* 通道：按级别分流终端行（`console_line`），JSONL 双写不变。
    pub fn emit(&mut self, level: u8, msg: &str) {
        let (target, line) = console_line(level, msg);
        self.terminal(target, &line);
        self.jsonl(level, msg);
    }

    /// log.* 通道：终端行一律 stderr 带标签（`log_line`），JSONL 双写不变。
    pub fn emit_err(&mut self, level: u8, msg: &str) {
        let (target, line) = log_line(level, msg);
        self.terminal(target, &line);
        self.jsonl(level, msg);
    }

    #[allow(clippy::print_stdout, clippy::print_stderr)]
    fn terminal(&self, target: Target, line: &str) {
        match target {
            Target::Stdout => println!("{line}"),
            Target::Stderr => eprintln!("{line}"),
        }
    }

    /// JSONL 双写（含 console.log —— 文件里始终带级别标签，v0.1.55 语义未变）；
    /// 写失败 warn-once 后放弃落盘，不影响终端。
    fn jsonl(&mut self, level: u8, msg: &str) {
        if let Some(f) = &mut self.log_file
            && writeln!(f, "{}", jsonl_line(level_label(level), msg)).is_err()
            && !self.warn_failed
        {
            self.warn_failed = true;
            eprintln!("warn: exec: write --log-file failed (terminal output continues)");
        }
    }
}

#[op2(fast)]
fn op_exec_log(state: &mut OpState, level: u8, #[string] msg: String) {
    state.borrow_mut::<ExecSink>().emit(level, &msg);
}

/// log.* 专用通道（v0.1.55）：与 `op_exec_log` 分离——结构化日志一律 stderr，
/// 不用 level 魔数位编码（显式 op = 显式协议）。
#[op2(fast)]
fn op_exec_log_err(state: &mut OpState, level: u8, #[string] msg: String) {
    state.borrow_mut::<ExecSink>().emit_err(level, &msg);
}

#[op2]
#[serde]
fn op_exec_args(state: &mut OpState) -> Vec<String> {
    state.borrow::<ExecArgs>().0.clone()
}

deno_core::extension!(
    oj_exec_ext,
    ops = [op_exec_log, op_exec_log_err, op_exec_args],
    esm_entry_point = "ext:oj_exec_ext/exec_bootstrap.js",
    options = { exec_options: ExecOptions },
    state = |state, options| {
        state.put(ExecSink::new(options.exec_options.log_file));
        state.put(ExecArgs(options.exec_options.args));
    },
);

/// oj_exec_ext 的 ESM 源（编译期内嵌；理由同 test_ext——dir 形式会烧构建机路径）。
const OJ_EXEC_ESM: &[deno_core::ExtensionFileSource] = &[deno_core::ExtensionFileSource::new(
    "ext:oj_exec_ext/exec_bootstrap.js",
    deno_core::ascii_str_include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/exec_ext/exec_bootstrap.js"
    )),
)];

/// 构造 `oj_exec_ext`（exec_bootstrap.js 编进二进制）。
pub fn oj_exec_ext_init(exec_options: ExecOptions) -> deno_core::Extension {
    let mut ext = oj_exec_ext::init(exec_options);
    ext.esm_files = std::borrow::Cow::Borrowed(OJ_EXEC_ESM);
    ext
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_level_when_jsonl_line_then_rfc3339_ts_and_escaped_msg() {
        let line = jsonl_line("INFO", "hi \"x\"\nline2");
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["level"], "INFO");
        assert_eq!(v["msg"], "hi \"x\"\nline2");
        assert!(v["ts"].as_str().unwrap().contains('T')); // RFC3339
    }

    #[test]
    fn given_unknown_level_when_emit_label_then_falls_back_info() {
        assert_eq!(level_label(9), "INFO");
        assert_eq!(level_label(3), "ERROR");
    }

    /// v0.1.55 通道分离：console.log/info（level 1）→ stdout 原样（无前缀，管道友好）；
    /// debug/warn/error → stderr 带级别标签（`{:<5}  ` 格式不变）。
    #[test]
    fn given_console_levels_when_route_then_level1_stdout_raw_others_stderr_labeled() {
        let (t, line) = console_line(1, "hello");
        assert!(matches!(t, Target::Stdout));
        assert_eq!(line, "hello");
        let (t, line) = console_line(0, "dbg");
        assert!(matches!(t, Target::Stderr));
        assert_eq!(line, "DEBUG  dbg");
        let (t, line) = console_line(2, "care");
        assert!(matches!(t, Target::Stderr));
        assert_eq!(line, "WARN   care");
        let (t, line) = console_line(3, "boom");
        assert!(matches!(t, Target::Stderr));
        assert_eq!(line, "ERROR  boom");
    }

    /// v0.1.55：log.*（zap 结构化日志）一律 stderr 带标签——含 level 1，不混进管道结果；
    /// 未知 level 标签回退 INFO（与 level_label 现有回归一致）。
    #[test]
    fn given_log_line_when_route_then_always_stderr_labeled() {
        for level in [0u8, 1, 2, 3] {
            let (t, line) = log_line(level, "m");
            assert!(matches!(t, Target::Stderr));
            assert!(line.starts_with(level_label(level)), "{line}");
        }
        let (t, line) = log_line(9, "m");
        assert!(matches!(t, Target::Stderr));
        assert!(line.starts_with("INFO"), "{line}");
    }

    /// 回归护栏：exec_ext 的 ESM 源必须内嵌（不得依赖构建机路径，同 test_ext 模式）。
    #[test]
    fn oj_exec_ext_esm_source_is_embedded() {
        assert_eq!(OJ_EXEC_ESM.len(), 1);
        assert!(OJ_EXEC_ESM.iter().all(|f| f.is_runtime_loadable()));
        assert_eq!(
            OJ_EXEC_ESM[0].specifier,
            "ext:oj_exec_ext/exec_bootstrap.js"
        );
    }

    #[test]
    fn given_log_file_when_sink_emit_then_appends_jsonl() {
        let dir = std::env::temp_dir().join(format!("oj-sink-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("e.jsonl");
        {
            let f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&p)
                .unwrap();
            let mut sink = ExecSink::new(Some(f));
            sink.emit(1, "hello");
        }
        let v: serde_json::Value =
            serde_json::from_str(std::fs::read_to_string(&p).unwrap().trim_end()).unwrap();
        assert_eq!(v["msg"], "hello");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
