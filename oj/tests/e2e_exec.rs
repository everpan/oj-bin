//! `oj exec` 全功能 / 全参数 E2E 验收。
//!
//! 夹具脚本置于 `tests/e2e/exec/`（静态 .ts 文件），本文件作为集成测试驱动 `bin/oj` 子进程
//! 逐参数 / 逐功能断言。每个用例独立 spawn 子进程（互不干扰，无需全局串行锁）。
//!
//! 覆盖（spec §3 / §4）：
//! - 三种入口：`<file>` / `-e,--code` / `--repl`（含裸 exec 缺省进 REPL）
//! - 输出通道分离：console.log→stdout、console.error/log.*→stderr（带级别）
//! - 退出码：成功 0、顶层 throw→1 且 stderr 透传 V8 消息
//! - v0.1.60 单表达式回显（devtools 风格）：`-e '1'`→`1`；多语句 / 显式 console.log 不叠打
//! - 顶层 await（TLA）保真
//! - `--` 之后参数注入 globalThis.args
//! - 相对 import（file 模式）
//! - `--db` 默认库查询 + profile 选树（坏 default → fail，alt 成功证明选树生效）
//! - `--log-file` JSONL 抓取
//! - 缺省无 config：静默回落内置默认（不打 `note:`），`-e '1'` 仍回显
//! - fail-fast：缺省 profile（`--redis default`）、strict 清单列不存在的插件
//! - `migrate_on_start` 门禁：auto 建表、缺省跳过（表不存在即报错）
//! - `--site` 与 `-d` 互斥、不存在的 prefix→not found

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// 静态夹具脚本（位于本 crate 的 `tests/e2e/exec/`）。
fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/e2e/exec")
        .join(name)
}

/// 每个用例独立临时目录（按名区分，避免并行用例互相污染）。
fn tmp(name: &str) -> PathBuf {
    let t = std::env::temp_dir().join(format!("oj-e2e-exec-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&t);
    std::fs::create_dir_all(&t).unwrap();
    t
}

fn write_config(dir: &Path, body: &str) -> PathBuf {
    let p = dir.join("config.yaml");
    std::fs::write(&p, body).unwrap();
    p
}

/// sqlite 文件 DSN（跨平台安全，避开 Windows verbatim 前缀陷阱）。
fn sqlite(path: &Path) -> String {
    oj_plugin_ffi::path_util::sqlite_file_dsn(path)
}

fn oj() -> Command {
    Command::new(env!("CARGO_BIN_EXE_oj"))
}

/// 文件入口：输出通道分离 + 退出码（spec §3.3/§4）。
#[test]
fn given_file_when_console_then_stdout_direct_stderr_labeled() {
    let t = tmp("file");
    let cfg = write_config(&t, "{}\n");
    let out = oj()
        .args([
            "exec",
            fixture("hello.ts").to_str().unwrap(),
            "-c",
            cfg.to_str().unwrap(),
        ])
        .output()
        .expect("spawn oj exec");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(stdout.trim(), "stdout-line", "{stdout}"); // 原样、无级别前缀
    assert!(stderr.contains("stderr-line"), "{stderr}"); // console.error→stderr
    assert!(stderr.contains("INFO"), "{stderr}"); // log.* 带级别
    assert!(stderr.contains("log-info-msg"), "{stderr}");
}

/// 顶层 throw → 退出码 1，stderr 原样携带 V8 异常消息。
#[test]
fn given_file_when_throw_then_exit_1_with_v8_msg() {
    let t = tmp("boom");
    let cfg = write_config(&t, "{}\n");
    let out = oj()
        .args([
            "exec",
            fixture("boom.ts").to_str().unwrap(),
            "-c",
            cfg.to_str().unwrap(),
        ])
        .output()
        .expect("spawn oj exec");
    assert_eq!(
        out.status.code(),
        Some(1),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("EXEC_BOOM_MARKER"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// 相对 import（file 模式）解析链跑通。夹具脚本须位于 exec 的 project root 内
/// （模块加载器禁止 import 逃出 project root），故拷入临时项目根再跑。
#[test]
fn given_file_when_relative_import_then_chain_resolves() {
    let t = tmp("import");
    for f in ["import_main.ts", "import_util.ts"] {
        std::fs::copy(fixture(f), t.join(f)).unwrap();
    }
    let cfg = write_config(&t, "{}\n");
    let out = oj()
        .current_dir(&t)
        .args(["exec", "import_main.ts", "-c", cfg.to_str().unwrap()])
        .output()
        .expect("spawn oj exec");
    // 解析成功才抛得出 TAG=util-ok（导入链未断）。
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("TAG=util-ok"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `-e` 单表达式默认回显其值（v0.1.60，devtools 风格）。
#[test]
fn given_inline_code_when_single_expr_then_value_echoed() {
    let t = tmp("echo");
    let cfg = write_config(&t, "{}\n");
    let cases = [("1", "1"), ("({a:1})", r#"{"a":1}"#)];
    for (code, want) in cases {
        let out = oj()
            .args(["exec", "-e", code, "-c", cfg.to_str().unwrap()])
            .output()
            .expect("spawn oj exec");
        assert!(
            out.status.success(),
            "code={code} stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            want,
            "code={code}"
        );
    }
}

/// 显式 console.log 不叠打 undefined；多语句 / 声明不回显。
#[test]
fn given_inline_code_when_not_single_expr_then_no_echo() {
    let t = tmp("noecho");
    let cfg = write_config(&t, "{}\n");
    // 已是 console.log → 仅原样输出，不叠打 undefined。
    let out = oj()
        .args([
            "exec",
            "-e",
            r#"console.log("x")"#,
            "-c",
            cfg.to_str().unwrap(),
        ])
        .output()
        .expect("spawn oj exec");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "x");
    assert!(!String::from_utf8_lossy(&out.stdout).contains("undefined"));
    // 多语句 → 不回显。
    let out = oj()
        .args(["exec", "-e", "const x = 1; x", "-c", cfg.to_str().unwrap()])
        .output()
        .expect("spawn oj exec");
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).trim().is_empty());
}

/// 顶层 await 跑完 event loop 后 settle（spec §3.4）。
#[test]
fn given_inline_code_when_top_level_await_then_settles() {
    let t = tmp("tla");
    let cfg = write_config(&t, "{}\n");
    let out = oj()
        .args([
            "exec",
            "-e",
            r#"await Promise.resolve(); console.log("TLA_OK")"#,
            "-c",
            cfg.to_str().unwrap(),
        ])
        .output()
        .expect("spawn oj exec");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("TLA_OK"));
}

/// `--` 之后参数原样注入 globalThis.args。
#[test]
fn given_dashdash_args_then_injected_into_global_args() {
    let t = tmp("args");
    let cfg = write_config(&t, "{}\n");
    let out = oj()
        .args([
            "exec",
            "-e",
            r#"console.log("ARGS=" + args.join("|"))"#,
            "-c",
            cfg.to_str().unwrap(),
            "--",
            "--x",
            "foo bar",
        ])
        .output()
        .expect("spawn oj exec");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("ARGS=--x|foo bar"),
        "stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

/// REPL：裸 `oj exec`（无 file/-e/--repl）缺省进 REPL，stdin 喂入逐行求值。
fn run_repl(args: &[&str], input: &[u8]) -> String {
    let t = tmp("repl");
    let cfg = write_config(&t, "{}\n");
    let mut full = vec!["exec"];
    for a in args {
        full.push(a);
    }
    full.push("-c");
    full.push(cfg.to_str().unwrap());
    let mut child = oj()
        .args(&full)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn oj exec repl");
    child.stdin.take().unwrap().write_all(input).unwrap();
    let out = child.wait_with_output().expect("wait oj exec repl");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

#[test]
fn given_bare_exec_when_no_src_then_defaults_to_repl() {
    let s = run_repl(&[], b"console.log('repl-x');\n");
    assert!(s.contains("REPL"), "{s}");
    assert!(s.lines().any(|l| l.trim() == "repl-x"), "{s}");
}

#[test]
fn given_repl_flag_then_stdin_evaluated() {
    let s = run_repl(&["--repl"], b"console.log('repl-y');\n");
    assert!(s.contains("REPL"), "{s}");
    assert!(s.lines().any(|l| l.trim() == "repl-y"), "{s}");
}

/// 默认库查询（sqlite）。
#[test]
fn given_db_default_when_query_then_rows_returned() {
    let t = tmp("db");
    let cfg = write_config(
        &t,
        &format!("db:\n  default: \"{}\"\n", sqlite(&t.join("d.sqlite"))),
    );
    let out = oj()
        .args([
            "exec",
            fixture("db_probe.ts").to_str().unwrap(),
            "-c",
            cfg.to_str().unwrap(),
        ])
        .output()
        .expect("spawn oj exec");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "DBV=1");
}

/// `--db <profile>` 选树生效：先在 alt 库建表（跨进程持久化），default 库不含该表 →
/// 查询 alt 成功、查询 default 报「no such table」，证明 `--db` 真的选了不同 profile。
#[test]
fn given_db_profile_flag_then_selects_named_profile() {
    let t = tmp("dbpick");
    let default_dsn = sqlite(&t.join("d.sqlite"));
    let alt_dsn = sqlite(&t.join("alt.sqlite"));
    let cfg = write_config(
        &t,
        &format!("db:\n  default: \"{default_dsn}\"\n  alt: \"{alt_dsn}\"\n"),
    );
    // 在 alt 库建表（sqlite 文件跨进程持久化）。
    let out = oj()
        .args([
            "exec",
            "-e",
            "await db.query('create table if not exists exec_alt (id integer)', [])",
            "-c",
            cfg.to_str().unwrap(),
            "--db",
            "alt",
        ])
        .output()
        .expect("spawn oj exec");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // alt 有表 → 查询成功。
    let out = oj()
        .args([
            "exec",
            "-e",
            "console.log('A=' + (await db.query('select count(*) as c from exec_alt', [])).map(r => r.c)[0])",
            "-c",
            cfg.to_str().unwrap(),
            "--db",
            "alt",
        ])
        .output()
        .expect("spawn oj exec");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("A=0"),
        "stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    // default 无该表 → 报错（证明未误用 alt）。
    let out = oj()
        .args([
            "exec",
            "-e",
            "await db.query('select count(*) from exec_alt', [])",
            "-c",
            cfg.to_str().unwrap(),
            "--db",
            "default",
        ])
        .output()
        .expect("spawn oj exec");
    assert!(!out.status.success(), "default 不应含 exec_alt 表");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("exec_alt"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `--log-file` 抓取 JSONL（终端 + JSONL 双写）。
#[test]
fn given_log_file_then_jsonl_captures_console() {
    let t = tmp("logfile");
    let cfg = write_config(&t, "{}\n");
    let lf = t.join("out.jsonl");
    let out = oj()
        .args([
            "exec",
            "-e",
            "console.log('LF')",
            "-c",
            cfg.to_str().unwrap(),
            "--log-file",
            lf.to_str().unwrap(),
        ])
        .output()
        .expect("spawn oj exec");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let j = std::fs::read_to_string(&lf).expect("read log file");
    assert!(j.contains("LF"), "log: {j}");
}

/// 缺省无 config：`oj exec` 静默回落内置默认（不打 `note:`），`-e '1'` 仍回显。
#[test]
fn given_no_config_then_silent_default_and_echo_works() {
    let t = tmp("noconf");
    // 隔离 HOME 与 cwd，确保不会向上搜到任何 config.yaml。
    let out = oj()
        .current_dir(&t)
        .env("HOME", &t)
        .args(["exec", "-e", "1"])
        .output()
        .expect("spawn oj exec");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "1");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("no config.yaml found"),
        "exec 不应打 note: {stderr}"
    );
}

/// fail-fast：声明了不存在的插件 → 退出码非 0 且 stderr 含 plugin。
#[test]
fn given_strict_manifest_missing_plugin_then_err() {
    let t = tmp("plugin");
    let cfg = write_config(&t, "plugins:\n  nope-plugin: {}\n");
    let out = oj()
        .args(["exec", "-e", "1", "-c", cfg.to_str().unwrap()])
        .output()
        .expect("spawn oj exec");
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("plugin"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// fail-fast：未声明的 `--db <profile>` → 退出码非 0（明确报错，不静默兜底）。
#[test]
fn given_undeclared_profile_then_fail_fast() {
    let t = tmp("redis");
    let cfg = write_config(
        &t,
        &format!("db:\n  default: \"{}\"\n", sqlite(&t.join("d.sqlite"))),
    );
    let out = oj()
        .args([
            "exec",
            "-e",
            "1",
            "-c",
            cfg.to_str().unwrap(),
            "--db",
            "ghost",
        ])
        .output()
        .expect("spawn oj exec");
    assert!(!out.status.success(), "未声明 profile 应 fail-fast");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("ghost") || stderr.contains("profile"),
        "stderr: {stderr}"
    );
}

/// `migrate_on_start` 门禁：auto 建表 → 查询成功；缺省跳过 → 表不存在报错。
/// 迁移按模块扫描：`dir` 须为模块树的父目录（含 `*/manifest.yaml` + `*/migrations/`）。
#[test]
fn given_migrate_gate_then_auto_applies_and_default_skips() {
    let t = tmp("mig");
    // 把迁移夹具构造成模块：dir/src/mig/{manifest.yaml, migrations/0001__init.sql}
    let mod_dir = t.join("src/mig");
    std::fs::create_dir_all(mod_dir.join("migrations")).unwrap();
    std::fs::write(
        mod_dir.join("manifest.yaml"),
        "name: mig\ndesc: d\nversion: 0.1.0\n",
    )
    .unwrap();
    std::fs::write(
        mod_dir.join("migrations/0001__init.sql"),
        "create table if not exists exec_t (id integer);\n",
    )
    .unwrap();
    let dir = t.join("src");
    // auto：迁移应用到 default 库 → exec_t 建出。
    let cfg_auto = write_config(
        &t,
        &format!(
            "db:\n  default: \"{}\"\nserver:\n  migrate_on_start: auto\n",
            sqlite(&t.join("auto.sqlite"))
        ),
    );
    let out = oj()
        .args([
            "exec",
            "-e",
            "console.log('MIG=' + (await db.query('select count(*) as c from exec_t', [])).map(r => r.c)[0])",
            "-c",
            cfg_auto.to_str().unwrap(),
            "-d",
            dir.to_str().unwrap(),
        ])
        .output()
        .expect("spawn oj exec");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("MIG=0"),
        "stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    // 缺省（不写 migrate_on_start）→ 全跳过 → 表不存在 → 脚本报错。
    let cfg_off = write_config(
        &t,
        &format!("db:\n  default: \"{}\"\n", sqlite(&t.join("off.sqlite"))),
    );
    let out = oj()
        .args([
            "exec",
            "-e",
            "await db.query('select count(*) from exec_t', [])",
            "-c",
            cfg_off.to_str().unwrap(),
            "-d",
            dir.to_str().unwrap(),
        ])
        .output()
        .expect("spawn oj exec");
    assert!(!out.status.success(), "缺省应跳过迁移 → 表不存在报错");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("exec_t"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `--site` 与 `-d` 互斥。
#[test]
fn given_site_and_dir_then_mutually_exclusive() {
    let t = tmp("site");
    let cfg = write_config(&t, "{}\n");
    let out = oj()
        .args([
            "exec",
            "-e",
            "1",
            "-c",
            cfg.to_str().unwrap(),
            "-d",
            "src",
            "--site",
            "/x",
        ])
        .output()
        .expect("spawn oj exec");
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("mutually exclusive"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `--site <prefix>` 命中不存在的 prefix → not found。
#[test]
fn given_site_nonexistent_prefix_then_not_found() {
    let t = tmp("site_nf");
    std::fs::create_dir_all(t.join("src/a")).unwrap();
    std::fs::write(t.join("src/a/manifest.yaml"), "name: a\n").unwrap();
    let cfg = write_config(&t, "mounts:\n  - prefix: /v1/api/a\n    api: src/a\n");
    let out = oj()
        .args([
            "exec",
            "-e",
            "1",
            "-c",
            cfg.to_str().unwrap(),
            "--site",
            "/v1/api/zzz",
        ])
        .output()
        .expect("spawn oj exec");
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("not found"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
