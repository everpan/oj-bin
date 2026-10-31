use super::template::{ExecTemplate, MatchError, Token, match_all, match_template, parse_template};
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
fn wrapped_token_shorter_than_affixes_is_named_error_not_panic() {
    // arg 比前后缀合计还短（"%" vs prefix/suffix 各 "%"）：starts/ends 双过，
    // 切片前不判长度就是 slice-index panic——单字符 argv 可打穿宿主（v0.1.83）。
    let t = p("cmd /c echo %${raw:NAME}%");
    let e = match_template(&t, &sargv(&["cmd", "/c", "echo", "%"])).unwrap_err();
    assert!(e.contains("expects '%${...}%'"), "{e}");
    // 恰好等长仍是合法命中（空内串；此前即此语义，保持不变）。
    let b = match_template(&t, &sargv(&["cmd", "/c", "echo", "%%"])).unwrap();
    assert_eq!(b, vec![("NAME".to_string(), String::new())]);
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
  // 同一条非 UTF-8 输出的另外三种消费形态（钉死「缓冲形态抛、回调/流式形态不抛」
  // 这条不对称契约——文档 §6 据此订正，见「形态 × 非 UTF-8 行为」表）。
  s12_badutf8_out: async () => json.ok(await exec.run(["sh", "-c", "printf '\\200'"])),
  s12_badutf8_cb: async () => {
    const r = await exec.run(["sh", "-c", "printf '\\200'"], { onStdout: () => {} });
    return json.ok({ code: r.code, out: r.stdout });
  },
  s12_badutf8_spawn: async () => {
    const h = await exec.spawn(["sh", "-c", "printf '\\200'"]);
    const bytes = [];
    for await (const c of h.stdout) bytes.push(...c);
    const { code } = await h.exit();
    return json.ok({ code, n: bytes.length, first: bytes[0] });
  },
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
    // S12 续：同一条非 UTF-8 输出的其它三种形态——**只有缓冲形态抛**。
    // 缓冲形态（stdout 侧，补 s12_badutf8 未覆盖的 stdout）：抛点名。
    {
        let e = b
            .run_module(&api("worker"), "s12_badutf8_out", Default::default(), t)
            .await
            .unwrap_err();
        let m = e.to_string();
        assert!(m.contains("stdout") && m.contains("UTF-8"), "{m}");
    }
    // 回调形态：不抛，返回 { code, stdout }，stdout 为**有损**解码（U+FFFD）。
    {
        let cap = b
            .run_module(&api("worker"), "s12_badutf8_cb", Default::default(), t)
            .await
            .expect("callback form must not throw on non-UTF-8");
        let v: serde_json::Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        let out = v["data"]["out"]
            .as_str()
            .expect("callback form 仍返回 stdout");
        assert!(
            out.contains('\u{FFFD}'),
            "回调形态应为有损解码（U+FFFD），实际: {out:?}"
        );
    }
    // spawn 流式形态：不抛，给原始字节（0x80），由 JS 自行解码。
    {
        let cap = b
            .run_module(&api("worker"), "s12_badutf8_spawn", Default::default(), t)
            .await
            .expect("spawn form must not throw on non-UTF-8");
        let v: serde_json::Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(v["data"]["n"], 1, "{v}");
        assert_eq!(v["data"]["first"], 0x80, "流式形态必须给原始字节: {v}");
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
  s10_stdin: async () => {
    const parts = [];
    const r = await exec.run(["sh", "-c", "cat"], {
      stdin: "pipe-through",
      onStdout: (c) => parts.push(new TextDecoder().decode(c)),
    });
    return json.ok({ code: r.code, out: parts.join(""), full: r.stdout });
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

    // SS10（v0.1.83）：回调/流式形态同样吃 opts.stdin（此前静默丢弃，
    // `sh -c cat` 直接吃空输入）。与缓冲形态同语义：写入后关管道。
    let d = data!("s10_stdin");
    assert_eq!(d["code"], 0, "{}", d);
    assert_eq!(d["out"], "pipe-through");
    assert_eq!(d["full"], "pipe-through");

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
