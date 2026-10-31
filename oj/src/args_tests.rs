use super::*;

use clap::error::ErrorKind;

fn cmd(argv: &[&str]) -> Command {
    to_command(Cli::try_parse_from(std::iter::once("oj").chain(argv.iter().copied())).unwrap())
}

#[test]
fn exec_file_last_args_and_log_file_parse() {
    // `--` 之后原样透传（含连字符参数）；--log-file / -d / --db 映射。
    let Command::Exec(a) = cmd(&[
        "exec",
        "scripts/job.ts",
        "-c",
        "c.yaml",
        "-d",
        "src",
        "--db",
        "report",
        "--log-file",
        "out.jsonl",
        "--",
        "-x",
        "foo bar",
    ]) else {
        panic!()
    };
    assert_eq!(a.file.as_deref(), Some("scripts/job.ts"));
    assert_eq!(a.code, None);
    assert_eq!(a.config.as_deref(), Some("c.yaml"));
    assert_eq!(a.dir.as_deref(), Some("src"));
    assert_eq!(a.db.as_deref(), Some("report"));
    assert_eq!(a.log_file.as_deref(), Some("out.jsonl"));
    assert_eq!(a.args, ["-x", "foo bar"]);
    // 无 `--` → args 为空。
    let Command::Exec(a) = cmd(&["exec", "s.ts", "-c", "c.yaml"]) else {
        panic!()
    };
    assert_eq!(a.file.as_deref(), Some("s.ts"));
    assert!(a.args.is_empty());
}

#[test]
fn exec_resource_profile_flags_parse() {
    // v0.1.34：资源根 key 多源选择旗标（`--redis/--blob/--es/--broker/--kafka/--rabbit`）。
    let Command::Exec(a) = cmd(&[
        "exec", "s.ts", "-c", "c.yaml", "--redis", "cache", "--blob", "assets", "--es", "archive",
        "--broker", "prod", "--kafka", "staging", "--rabbit", "jobs",
    ]) else {
        panic!()
    };
    assert_eq!(a.redis.as_deref(), Some("cache"));
    assert_eq!(a.blob.as_deref(), Some("assets"));
    assert_eq!(a.es.as_deref(), Some("archive"));
    assert_eq!(a.broker.as_deref(), Some("prod"));
    assert_eq!(a.kafka.as_deref(), Some("staging"));
    assert_eq!(a.rabbit.as_deref(), Some("jobs"));
    // 缺省 → 全 None（回落各段 default）。
    let Command::Exec(a) = cmd(&["exec", "s.ts", "-c", "c.yaml"]) else {
        panic!()
    };
    assert!(
        a.redis.is_none()
            && a.blob.is_none()
            && a.es.is_none()
            && a.broker.is_none()
            && a.kafka.is_none()
            && a.rabbit.is_none()
    );
}

/// BDD（v0.1.85）：`--module-root` 与 `-d`/`--api-path` 都是「源码树根」，
/// 同时给必须由 clap 挡下（exit 2），不能靠后到者静默覆盖前者。
#[test]
fn given_both_dir_and_module_root_when_parse_then_conflict_error() {
    let cli =
        |argv: &[&str]| Cli::try_parse_from(std::iter::once("oj").chain(argv.iter().copied()));
    // build：`-d` 有 default_value="src"，仅显式给出才算冲突。
    let e = cli(&["build", "-d", "src", "--module-root", "src"]).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::ArgumentConflict, "{e}");
    // serve：与 --api-path 互斥。
    let e = cli(&[
        "serve",
        "-c",
        "c.yaml",
        "--api-path",
        "src",
        "--module-root",
        "src",
    ])
    .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::ArgumentConflict, "{e}");
    // openapi / migrate / test / schema diff 同口径。
    for cmd in [
        vec!["openapi", "-d", "src"],
        vec!["migrate", "-d", "src"],
        vec!["test", "-d", "src"],
        vec!["schema", "diff", "-d", "src"],
    ] {
        let mut argv = cmd.clone();
        argv.extend(["--module-root", "src"]);
        let e = cli(&argv).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::ArgumentConflict, "{argv:?}: {e}");
    }
}

/// BDD：`--module-root` 单给可解析（default_value 的 `-d` 不触发冲突）。
#[test]
fn given_only_module_root_when_parse_then_reaches_subcommand_args() {
    let Command::Build(a) = cmd(&["build", "--module-root", "src", "-o", "dist"]) else {
        panic!()
    };
    assert_eq!(a.module_root.as_deref(), Some("src"));
    let Command::Serve(a) = cmd(&["serve", "-c", "c.yaml", "--module-root", "src"]) else {
        panic!()
    };
    assert_eq!(a.module_root.as_deref(), Some("src"));
    assert!(a.api_path.is_none());
}

#[test]
fn exec_code_and_repl_flags_parse() {
    // -e/--code 内联代码：与 file 互斥（group），但单独给出时落到 code。
    let Command::Exec(a) = cmd(&["exec", "-e", "console.log(1)", "-c", "c.yaml"]) else {
        panic!()
    };
    assert_eq!(a.code.as_deref(), Some("console.log(1)"));
    assert!(a.file.is_none());
    assert!(!a.repl);
    // --code 长旗标同义。
    let Command::Exec(a) = cmd(&["exec", "--code", "db.query('x')", "-c", "c.yaml"]) else {
        panic!()
    };
    assert_eq!(a.code.as_deref(), Some("db.query('x')"));
    // --repl：落到 repl，file/code 均空。
    let Command::Exec(a) = cmd(&["exec", "--repl", "-c", "c.yaml"]) else {
        panic!()
    };
    assert!(a.repl);
    assert!(a.file.is_none() && a.code.is_none());
    let cli =
        |argv: &[&str]| Cli::try_parse_from(std::iter::once("oj").chain(argv.iter().copied()));
    // 互斥：file + --code 报错。
    assert!(cli(&["exec", "s.ts", "-e", "1"]).is_err());
    // file + --repl 报错。
    assert!(cli(&["exec", "s.ts", "--repl"]).is_err());
    // 三者皆无：args 层照常解析不报错（v0.1.55 起缺省进 REPL——路由在 exec_cmd 层）。
    let Command::Exec(a) = cmd(&["exec", "-c", "c.yaml"]) else {
        panic!()
    };
    assert!(a.file.is_none() && a.code.is_none() && !a.repl);
}

#[test]
fn test_resource_profile_flags_parse() {
    let Command::Test(crate::args::TestCmd::Run(a)) = cmd(&[
        "test", "-c", "c.yaml", "-d", "src", "--redis", "cache", "--blob", "assets", "--es",
        "archive", "--broker", "prod", "--kafka", "staging", "--rabbit", "jobs",
    ]) else {
        panic!()
    };
    assert_eq!(a.redis.as_deref(), Some("cache"));
    assert_eq!(a.blob.as_deref(), Some("assets"));
    assert_eq!(a.es.as_deref(), Some("archive"));
    assert_eq!(a.broker.as_deref(), Some("prod"));
    assert_eq!(a.kafka.as_deref(), Some("staging"));
    assert_eq!(a.rabbit.as_deref(), Some("jobs"));
}

#[test]
fn server_daemon_flag_maps_through() {
    // --daemon 长旗标映射进 ServeArgs；短 -d 仍拒绝（--dir 已删，不回收短旗标）。
    let Command::Serve(a) = cmd(&["serve", "-c", "c.yaml", "--daemon"]) else {
        panic!()
    };
    assert!(a.daemon);
    let Command::Serve(a) = cmd(&["serve", "-c", "c.yaml"]) else {
        panic!()
    };
    assert!(!a.daemon);
    assert!(Cli::try_parse_from(["oj", "serve", "-c", "c.yaml", "-d"]).is_err());
}

#[test]
fn server_defaults_and_overrides() {
    // 默认值：base=None（config server.base 兜底） / api_path=None / app_path=None。
    // 裸 `oj server` 现在打印帮助（arg_required_else_help），给一个参数才进入解析，
    // 故默认值用 -c 触发；config 默认 "config.yaml" 由 clap default_value 保证。
    let Command::Serve(a) = cmd(&["serve", "-c", "config.yaml"]) else {
        panic!()
    };
    assert_eq!(
        (
            a.base.as_deref(),
            a.api_path.as_deref(),
            a.app_path.as_slice()
        ),
        (None, None, &[][..])
    );
    let Command::Serve(a) = cmd(&[
        "serve",
        "-c",
        "c.yaml",
        "-b",
        "/api",
        "--api-path",
        "src",
        "--app-path",
        "web",
    ]) else {
        panic!()
    };
    assert_eq!(
        (
            a.config.as_deref(),
            a.base.as_deref(),
            a.api_path.as_deref(),
            a.app_path.as_slice()
        ),
        (
            Some("c.yaml"),
            Some("/api"),
            Some("src"),
            &["web".to_string()][..]
        )
    );
    // v0.1.27：--app-path 可重复（裸 dir + prefix=dir 混合）
    let Command::Serve(a) = cmd(&[
        "serve",
        "-c",
        "c.yaml",
        "--app-path",
        "web",
        "--app-path",
        "/docs=dist/docs",
    ]) else {
        panic!()
    };
    assert_eq!(
        a.app_path,
        vec!["web".to_string(), "/docs=dist/docs".to_string()]
    );
}

#[test]
fn build_module_positional_and_flags() {
    let Command::Build(a) = cmd(&["build"]) else {
        panic!()
    };
    assert_eq!(
        (
            a.module.as_deref(),
            a.dir.as_str(),
            a.out.as_str(),
            a.minify
        ),
        (None, "src", "dist", true)
    );
    let Command::Build(a) = cmd(&["build", "--no-minify"]) else {
        panic!()
    };
    assert!(!a.minify); // 排障逃生门
    let Command::Build(a) = cmd(&["build", "user", "-d", "s", "-o", "d"]) else {
        panic!()
    };
    assert_eq!(
        (a.module.as_deref(), a.dir.as_str(), a.out.as_str()),
        (Some("user"), "s", "d")
    );
}

#[test]
fn secret_subcommands_map_through() {
    // keygen：--out-dir / --force；--alg 缺省 = x25519（密封，行为零变化）。
    let Command::SecretKeygen(a) = cmd(&["secret", "keygen", "--out-dir", "keys", "--force"])
    else {
        panic!()
    };
    assert_eq!(
        (a.out_dir.as_str(), a.force, a.alg),
        ("keys", true, SecretKeygenAlg::X25519)
    );
    let Command::SecretKeygen(a) = cmd(&["secret", "keygen"]) else {
        panic!()
    };
    assert_eq!(
        (a.out_dir.as_str(), a.force, a.alg),
        (".", false, SecretKeygenAlg::X25519)
    );
    // --alg ed25519（签名密钥对，v0.1.80）：显式传入可解析。
    let Command::SecretKeygen(a) =
        cmd(&["secret", "keygen", "--out-dir", "keys", "--alg", "ed25519"])
    else {
        panic!()
    };
    assert_eq!(a.alg, SecretKeygenAlg::Ed25519);
    // seal/open：positional VALUE 可省（省则读 stdin），-k 与 -c 均可选。
    let Command::SecretSeal(a) = cmd(&["secret", "seal", "-k", "pub.pem", "hunter2"]) else {
        panic!()
    };
    assert_eq!(
        (a.key.as_deref(), a.value.as_deref()),
        (Some("pub.pem"), Some("hunter2"))
    );
    let Command::SecretSeal(a) = cmd(&["secret", "seal", "-c", "config.yaml"]) else {
        panic!()
    };
    assert!(a.key.is_none() && a.value.is_none());
    assert_eq!(a.config.as_deref(), Some("config.yaml"));
    let Command::SecretOpen(a) = cmd(&["secret", "open", "-k", "priv.pem"]) else {
        panic!()
    };
    assert_eq!(
        (a.key.as_deref(), a.config.as_deref()),
        (Some("priv.pem"), None)
    );
    let cli =
        |argv: &[&str]| Cli::try_parse_from(std::iter::once("oj").chain(argv.iter().copied()));
    // 未知子命令 / 未知旗标仍由 clap 拒（--bits 已随 v1 一并移除）。
    assert!(cli(&["secret", "nope"]).is_err());
    assert!(cli(&["secret", "keygen", "--bits", "1"]).is_err());
}

#[test]
fn bad_usage_is_clap_error() {
    let cli =
        |argv: &[&str]| Cli::try_parse_from(std::iter::once("oj").chain(argv.iter().copied()));
    // -b 已不是 build 参数：clap 拒绝（不再吞值静默全量构建）
    assert!(cli(&["build", "-b", "/x"]).is_err());
    // 第二个位置参数不再静默丢弃
    assert!(cli(&["build", "user", "other"]).is_err());
    // 未知子命令 / 未知长旗标
    assert!(cli(&["foo"]).is_err());
    assert!(cli(&["serve", "--nope"]).is_err());
    // --dev 已删：模式由 --api-path 目录自动判定（serve_cmd::is_release）
    assert!(cli(&["serve", "--dev"]).is_err());
    // server 的 -d/--dir 已删（改为 --api-path）：clap 拒绝
    assert!(cli(&["serve", "-d", "src"]).is_err());
    assert!(cli(&["serve", "--dir", "src"]).is_err());
    // --grace-days 已删：宽限天数仅由 config 的 server.grace_days 提供
    assert!(cli(&["serve", "--grace-days", "30"]).is_err());
    // 空参 → 帮助（arg_required_else_help）
    assert_eq!(
        cli(&[]).unwrap_err().kind(),
        ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
    );
    // `oj server` 裸调（无任何参数）→ 帮助；给了参数（如 -c）才真正启动
    assert_eq!(
        cli(&["serve"]).unwrap_err().kind(),
        ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
    );
    assert!(cli(&["serve", "-c", "c.yaml"]).is_ok());
}

#[test]
fn server_cert_key_console_overrides_map_through() {
    // 证书三旗标是 config 的覆盖通道（Some → 覆盖 server.certificate_path 等）。
    let Command::Serve(a) = cmd(&[
        "serve",
        "-c",
        "c.yaml",
        "--cert-path",
        "cert.jws",
        "--key-path",
        "pub.pem",
        "--console-log",
    ]) else {
        panic!()
    };
    assert_eq!(
        (a.cert_path.as_deref(), a.key_path.as_deref(), a.console_log),
        (Some("cert.jws"), Some("pub.pem"), true)
    );
}

#[test]
fn test_subcommand_maps_all_report_flags() {
    let Command::Test(crate::args::TestCmd::Run(a)) = cmd(&[
        "test", "-c", "c.yaml", "-d", "src", "-t", "tests", "--format", "tap", "--output", "r.tap",
    ]) else {
        panic!()
    };
    assert_eq!(
        (
            a.config.as_deref(),
            a.dir.as_deref(),
            a.tests.as_deref(),
            a.format.as_deref(),
            a.output.as_deref()
        ),
        (
            Some("c.yaml"),
            Some("src"),
            Some("tests"),
            Some("tap"),
            Some("r.tap")
        )
    );
}

#[test]
fn migrate_baseline_and_module_positional() {
    let Command::Migrate(a) = cmd(&["migrate", "--baseline", "user"]) else {
        panic!()
    };
    assert_eq!(
        (a.baseline, a.module.as_deref(), a.dir.as_deref()),
        (true, Some("user"), None)
    );
}

#[test]
fn migrate_fixture_schema_diff_map_db_profile() {
    // --db：三处瘦身装配统一的目标库 profile；缺省 None（→ default）。
    let Command::Migrate(a) = cmd(&["migrate", "-c", "c.yaml", "--db", "analytics"]) else {
        panic!()
    };
    assert_eq!(
        (a.db.as_deref(), a.config.as_deref()),
        (Some("analytics"), Some("c.yaml"))
    );
    let Command::Migrate(a) = cmd(&["migrate"]) else {
        panic!()
    };
    assert!(a.db.is_none());
    let Command::Test(crate::args::TestCmd::Fixture(a)) = cmd(&["test", "fixture", "--db", "test"])
    else {
        panic!()
    };
    assert_eq!(a.db.as_deref(), Some("test"));
    let Command::SchemaDiff(a) = cmd(&["schema", "diff", "--db", "test"]) else {
        panic!()
    };
    assert_eq!(a.db.as_deref(), Some("test"));
    // --db 需要值；非法用法仍由 clap 报错。
    assert!(
        Cli::try_parse_from(["oj", "migrate", "--db"]).is_err(),
        "--db 缺值须 clap 报错"
    );
}

#[test]
fn fixture_and_schema_diff_map_config_dir() {
    let Command::Test(crate::args::TestCmd::Fixture(a)) =
        cmd(&["test", "fixture", "-c", "c.yaml", "-d", "src"])
    else {
        panic!()
    };
    assert_eq!(
        (a.config.as_deref(), a.dir.as_deref()),
        (Some("c.yaml"), Some("src"))
    );
    let Command::SchemaDiff(a) = cmd(&["schema", "diff", "-c", "c.yaml"]) else {
        panic!()
    };
    assert_eq!(
        (a.config.as_deref(), a.dir.as_deref()),
        (Some("c.yaml"), None)
    );
}
