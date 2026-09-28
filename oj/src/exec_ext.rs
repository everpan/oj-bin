//! exec 扩展（oj_exec_ext）：exec_bootstrap.js 注入 `args`/`console`/`log`（覆盖
//! bridge 默认 log 的 tracing 通道，直出终端 + 可选 JSONL 落盘）。
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

/// 日志落点：终端 stdout 直出 + 可选 JSONL 双写。
pub struct ExecSink {
    log_file: Option<std::fs::File>,
    /// 写文件失败只告警一次，不刷 stderr。
    warn_failed: bool,
}

const LEVELS: [&str; 4] = ["DEBUG", "INFO", "WARN", "ERROR"];

pub(crate) fn level_label(level: u8) -> &'static str {
    LEVELS.get(level as usize).copied().unwrap_or("INFO")
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

    #[allow(clippy::print_stdout, clippy::print_stderr)]
    pub fn emit(&mut self, level: u8, msg: &str) {
        println!("{:<5}  {}", level_label(level), msg);
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

#[op2]
#[serde]
fn op_exec_args(state: &mut OpState) -> Vec<String> {
    state.borrow::<ExecArgs>().0.clone()
}

deno_core::extension!(
    oj_exec_ext,
    ops = [op_exec_log, op_exec_args],
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
