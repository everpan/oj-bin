//! oj 命令行（clap derive）。子命令：serve / build。
//! 帮助 `oj --help` / `oj <cmd> --help`；空参打印帮助、非法参数报错均由 clap 退出（code 2）。

use clap::{Parser, Subcommand};

/// server 子命令参数。
#[derive(Debug, Clone, Default)]
pub struct ServeArgs {
    pub config: String,
    /// None → 用 config 的 server.base（默认 /v1/api）。
    pub base: Option<String>,
    /// 后端 API 目录（src 源码树或 oj build 产物 dist），相对 CWD；模式按目录内容
    /// 自动判定。None → server 不开 API 功能（须配置静态站点，否则拒绝启动）。
    pub api_path: Option<String>,
    /// 静态站点目录（相对 CWD）；可重复（v0.1.27）。裸 `dir` = 覆盖 config 的主站点
    /// server.app_path（至多一次）；`prefix=dir` = 覆盖/新增 server.static_sites 中
    /// 该前缀的条目（CLI 优先）。
    pub app_path: Vec<String>,
    /// JWS 证书路径；Some 覆盖 config 的 server.certificate_path。
    pub cert_path: Option<String>,
    /// PEM 公钥路径；Some 覆盖 config 的 server.public_key_path。
    pub key_path: Option<String>,
    /// `--console-log`：true → 打开终端输出（默认关闭，只落盘）。
    /// 打开 config 的 server.console_log 之外的另一条通路（两者为「或」）。
    pub console_log: bool,
    /// `--daemon`：true → 后台运行：re-exec 自身脱离终端
    /// （unix setsid / windows DETACHED_PROCESS，stdio 重定向空设备），
    /// 父进程打印子 pid 后即退出。
    pub daemon: bool,
}

/// test 子命令参数（L1：进程内真实运行时跑 *.test.ts）。
pub struct TestArgs {
    pub config: String,
    /// None → 用 config 的 server.base（默认 /v1/api）。
    pub base: Option<String>,
    /// None → 默认目录（自 config 同级向上逐级搜：每层 src 优先、dist 次之）；模式按目录内容自动判定。
    pub dir: Option<String>,
    /// 测试用例目录：绝对路径原样；相对 → 相对 config_dir（项目根）。默认 "tests"。
    pub tests: Option<String>,
    /// 报告格式：human（默认，可读摘要）/ tap / junit / json，便于 CI 统一收口。
    pub format: Option<String>,
    /// 报告输出文件；省略则打到 stdout。machine 格式（tap/junit/json）配合此旗标落盘。
    pub output: Option<String>,
    /// 测试库：字面 "default" 的库调用改指向该库（默认取 config 的 `db.test`；
    /// 迁移/seed/fixtures 一并跟随，防测试写在开发库上）。未声明的库名 fail-fast。
    pub db: Option<String>,
    /// 把测试请求标记为匿名（等同生产 anonymous_paths 命中），让公开面 handler
    /// （走 `db.asTenant`）在 `oj test` 下可测。
    pub anonymous: bool,
}

/// `oj build [module] [-d src] [-o dist] [--no-minify] [--check]`（src → dist，生成 routes.js）。
pub struct BuildArgs {
    pub module: Option<String>,
    /// 配置文件（读 tasks.dir 决定镜像目录名；缺文件回落默认 "tasks"）。
    pub config: String,
    pub dir: String,
    pub out: String,
    /// 转译产物 minify（swc 全量压缩 + 函数内局部变量名混淆）。默认开；
    /// `--no-minify` 排障逃生门（多行可读产物）。
    pub minify: bool,
    /// 只跑结构检查（S002–S007）不落盘（§5.2 CI 门禁 / 本地快查）。
    pub check: bool,
}

/// `oj migrate [-c config] [-d dir] [--db name] [--baseline] [--module M]`。
pub struct MigrateArgs {
    pub config: String,
    /// None → 默认目录（自 config 同级向上逐级搜：每层 src 优先、dist 次之）；模式按目录内容自动判定。
    pub dir: Option<String>,
    /// 存量库接入门：全部迁移记为已应用而不执行（P0 建过表的库，Q5）。
    pub baseline: bool,
    /// 只迁移指定模块。
    pub module: Option<String>,
    /// 目标库：config `db:` 段的 profile 名（None → "default"）。未声明的库名 fail-fast。
    pub db: Option<String>,
}

/// `oj fixture [-c config] [-d dir] [--db name] [--module M]`。
pub struct FixtureArgs {
    pub config: String,
    pub dir: Option<String>,
    /// 只灌指定模块。
    pub module: Option<String>,
    /// 目标库：config `db:` 段的 profile 名（None → "default"）。未声明的库名 fail-fast。
    pub db: Option<String>,
}

/// `oj secret keygen [--bits N] [--out-dir D] [--force]`：生成密封密钥对。
pub struct SecretKeygenArgs {
    /// RSA 位数（最小 2048）。
    pub bits: usize,
    /// 输出目录：`<dir>/secrets-private.pem` 与 `<dir>/secrets-public.pem`。
    pub out_dir: String,
    /// 覆盖已存在文件（默认拒绝，防误删在用私钥）。
    pub force: bool,
}

/// `oj secret seal [-k pub.pem] [VALUE]`：把明文封成 `ENC[...]`（无 VALUE 时读 stdin）。
pub struct SecretSealArgs {
    /// 公钥 PEM 路径；None → 取 `-c` 配置的 `secrets.public_key_path`。
    pub key: Option<String>,
    /// 配置文件（只取 `secrets.public_key_path`，不解密整份配置）。
    pub config: Option<String>,
    /// 明文；None → 读 stdin（推荐：命令行参数会进 shell history / `ps`）。
    pub value: Option<String>,
}

/// `oj secret open [-k priv.pem] [VALUE]`：解出 `ENC[...]` 的明文（排障用）。
pub struct SecretOpenArgs {
    /// 私钥 PEM 路径；None → 走 `OJ_SECRET_KEY` / `OJ_SECRET_KEY_FILE` / `-c` 配置的
    /// `secrets.private_key_path`。
    pub key: Option<String>,
    pub config: Option<String>,
    /// 密文；None → 读 stdin。
    pub value: Option<String>,
}

/// 解析结果（错误/帮助/空参由 clap 处理，不会走到这里）。
pub enum Command {
    Serve(ServeArgs),
    Build(BuildArgs),
    Test(TestArgs),
    Migrate(MigrateArgs),
    Fixture(FixtureArgs),
    SchemaDiff(SchemaDiffArgs),
    Exec(ExecArgs),
    SecretKeygen(SecretKeygenArgs),
    SecretSeal(SecretSealArgs),
    SecretOpen(SecretOpenArgs),
}

/// `oj schema diff [-c config] [-d dir] [--db name]`：声明 vs 实库只读对账（D001/D002，§5.1）。
pub struct SchemaDiffArgs {
    pub config: String,
    /// None → 默认目录（自 config 同级向上逐级搜：每层 src 优先、dist 次之）；模式按目录内容自动判定。
    pub dir: Option<String>,
    /// 目标库：config `db:` 段的 profile 名（None → "default"）。未声明的库名 fail-fast。
    pub db: Option<String>,
}

/// `oj exec [-c config] [-d dir] [--db name] [--log-file path] file [args...]`：
/// 直接执行 ts/js 脚本（完整注入后端全局；console/log 终端直出，--log-file 双写落盘）。
pub struct ExecArgs {
    pub file: String,
    pub config: String,
    pub dir: Option<String>,
    pub db: Option<String>,
    pub log_file: Option<String>,
    pub args: Vec<String>,
}

/// oj：目录镜像路由的 JS 服务与构建 CLI。
#[derive(Debug, Parser)]
#[command(
    name = "oj",
    version,
    arg_required_else_help = true,
    help_template = "\
{before-help}{name} {version}
{about-with-newline}
{usage-heading} {usage}

{all-args}{after-help}"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// 启动 HTTP 服务（目录镜像路由 + app_path 静态兜底）
    #[command(arg_required_else_help = true)]
    Serve {
        /// 配置文件路径（相对 CWD；server.host/port/app_path + db/redis）
        #[arg(short, long, default_value = "config.yaml")]
        config: String,
        /// API 基础路由前缀；缺省用 config 的 server.base（默认 /v1/api）
        #[arg(short, long)]
        base: Option<String>,
        /// 后端 API 目录（src 源码树或 oj build 产物 dist），相对 CWD；
        /// 模式自动判定（含 manifests.yaml → release/js，否则 dev/ts）。
        /// 缺省（server）：不开 API 功能 —— 未指定 --api-path 且未配置静态站点
        /// （server.app_path / --app-path）则拒绝启动
        #[arg(long = "api-path")]
        api_path: Option<String>,
        /// 静态站点目录（相对 CWD；可重复，v0.1.27）：裸 `dir` 覆盖 config 的
        /// server.app_path（至多一次）；`prefix=dir`（如 `--app-path /docs=dist/docs`）
        /// 覆盖/新增 server.static_sites 中该前缀的条目（CLI 优先于 config）
        #[arg(long = "app-path")]
        app_path: Vec<String>,
        /// JWS 证书路径（覆盖 config 的 server.certificate_path）
        #[arg(long)]
        cert_path: Option<String>,
        /// PEM 公钥路径（覆盖 config 的 server.public_key_path）
        #[arg(long)]
        key_path: Option<String>,
        /// 打开终端输出；**默认关闭**（只落盘至 server.logs_dir）。
        /// 非 unix 平台无落盘，终端输出强制保留。
        #[arg(long = "console-log")]
        console_log: bool,
        /// 后台运行：脱离终端（unix setsid / windows DETACHED_PROCESS），
        /// stdio 重定向空设备；日志照常落 server.logs_dir，父进程打印子 pid 后退出
        #[arg(long = "daemon")]
        daemon: bool,
    },
    /// 构建模块产物（src → dist：版本目录 / routes.js / tgz）
    Build {
        /// 目标模块名（src 首层子目录）；省略 = 全部模块
        module: Option<String>,
        /// 配置文件（读 tasks.dir 决定镜像目录名；缺文件回落默认 "tasks"）
        #[arg(short, long, default_value = "config.yaml")]
        config: String,
        /// 源码目录
        #[arg(short, long, default_value = "src")]
        dir: String,
        /// 产物目录
        #[arg(short, long, default_value = "dist")]
        out: String,
        /// 产物不 minify（默认 minify；排障逃生门，得到多行可读产物）
        #[arg(long)]
        no_minify: bool,
        /// 只跑结构检查（S002–S007），不写任何产物（CI 门禁）
        #[arg(long)]
        check: bool,
    },
    /// 跑 sample API 测试（无需启动 oj serve；进程内真实运行时派发）
    Test {
        /// 配置文件路径（相对 CWD；server.host/port/root + db/redis）
        #[arg(short, long, default_value = "config.yaml")]
        config: String,
        /// API 基础路由前缀；缺省用 config 的 server.base（默认 /v1/api）
        #[arg(short, long)]
        base: Option<String>,
        /// 服务目录；模式自动判定（含 manifests.yaml → release/js，否则 dev/ts）。
        /// 默认：自 config 同级向上逐级搜，每层 src 优先、dist 次之
        #[arg(short, long)]
        dir: Option<String>,
        /// 测试用例目录（默认 tests）；相对 config_dir（项目根）
        #[arg(short, long)]
        tests: Option<String>,
        /// 报告格式：human（默认）/ tap / junit / json（CI 兼容）
        #[arg(long)]
        format: Option<String>,
        /// 报告落盘文件；省略则打印到 stdout
        #[arg(long)]
        output: Option<String>,
        /// 测试库名（默认取 config 的 db.test）：字面 "default" 的库调用改指向该库，
        /// 迁移/seed/fixtures 一并跟随
        #[arg(long)]
        db: Option<String>,
        /// 把测试请求标记为匿名（等同生产 anonymous_paths 命中），供公开面
        /// handler（db.asTenant）在测试中授信
        #[arg(long)]
        anonymous: bool,
    },
    /// 应用模块迁移到最新（migrations/*.sql → 目标库；部署 = build && migrate && serve）
    Migrate {
        /// 配置文件路径（相对 CWD；db 段提供目标库）
        #[arg(short, long, default_value = "config.yaml")]
        config: String,
        /// 服务目录；模式自动判定（含 manifests.yaml → release/js，否则 dev/ts）。
        /// 默认：自 config 同级向上逐级搜，每层 src 优先、dist 次之
        #[arg(short, long)]
        dir: Option<String>,
        /// 目标库（整轮）：config `db:` 段的 profile 名，缺省 default；未声明即 fail-fast。
        /// 与 `oj test --db` 不同——那里是「字面 default 调用重定向」，这里是整轮迁移的目标库
        #[arg(long)]
        db: Option<String>,
        /// 存量库接入门：≤head 的迁移全部记为已应用而不执行（P0 建过表的库）
        #[arg(long)]
        baseline: bool,
        /// 只迁移指定模块（src 首层子目录 / dist 模块名）
        module: Option<String>,
    },
    /// 灌入模块 fixtures/ 演示数据（dev/test 用；不进 release 产物、不随启动重放）
    Fixture {
        /// 配置文件路径（相对 CWD；db 段提供目标库）
        #[arg(short, long, default_value = "config.yaml")]
        config: String,
        /// 服务目录；模式自动判定。默认：src 目录存在取 src，否则 dist
        #[arg(short, long)]
        dir: Option<String>,
        /// 目标库（整轮）：config `db:` 段的 profile 名，缺省 default；未声明即 fail-fast
        #[arg(long)]
        db: Option<String>,
        /// 只灌指定模块
        module: Option<String>,
    },
    /// 声明式 schema 运维
    Schema {
        #[command(subcommand)]
        command: SchemaCmd,
    },
    /// 配置凭据密封（config 里的 `ENC[...]`）：keygen / seal / open
    Secret {
        #[command(subcommand)]
        command: SecretCmd,
    },
    /// 直接执行 ts/js 脚本（完整注入后端全局；console/log 终端直出，--log-file 双写落盘）
    Exec {
        /// 脚本文件路径（.ts/.js）
        file: String,
        /// 配置文件路径（db/redis/插件段）
        #[arg(short, long, default_value = "config.yaml")]
        config: String,
        /// 服务目录（schema 白名单来源）；默认自动探测（src 优先 dist 次之）
        #[arg(short, long)]
        dir: Option<String>,
        /// 字面 "default" 的库调用重定向到该库（未声明的库名 fail-fast）
        #[arg(long)]
        db: Option<String>,
        /// 日志同时落盘 JSONL（终端照出；打开失败仅告警不中断）
        #[arg(long = "log-file")]
        log_file: Option<String>,
        /// 传给脚本的参数（`--` 之后原样注入 globalThis.args）
        #[arg(last = true)]
        args: Vec<String>,
    },
}

/// `oj secret <sub>`。
#[derive(Debug, Subcommand)]
pub enum SecretCmd {
    /// 生成密封密钥对（私钥留在部署机，公钥可随仓库走）
    Keygen {
        /// RSA 位数（最小 2048；长期密钥建议 4096）
        #[arg(long, default_value_t = 2048)]
        bits: usize,
        /// 输出目录（落 secrets-private.pem / secrets-public.pem）
        #[arg(long, default_value = ".")]
        out_dir: String,
        /// 覆盖已存在文件（默认拒绝）
        #[arg(long)]
        force: bool,
    },
    /// 把明文封成 `ENC[...]`（密文可直接写进 config.yaml）
    Seal {
        /// 公钥 PEM 路径；缺省取 --config 的 secrets.public_key_path
        #[arg(short, long)]
        key: Option<String>,
        /// 配置文件（只读 secrets.public_key_path）
        #[arg(short, long)]
        config: Option<String>,
        /// 明文；**省略则读 stdin**（命令行参数会进 shell history 与 `ps`）
        value: Option<String>,
    },
    /// 解出 `ENC[...]` 的明文（排障：确认密文与私钥对得上）
    Open {
        /// 私钥 PEM 路径；缺省走 OJ_SECRET_KEY / OJ_SECRET_KEY_FILE / --config
        /// 的 secrets.private_key_path
        #[arg(short, long)]
        key: Option<String>,
        #[arg(short, long)]
        config: Option<String>,
        /// 密文 `ENC[...]`；省略则读 stdin
        value: Option<String>,
    },
}

/// `oj schema <sub>`：现有仅 diff（漂移对账）。
#[derive(Debug, Subcommand)]
pub enum SchemaCmd {
    /// 声明式 schema 与实库只读对账（D001 漂移 / D002 未声明表；有差异退 1）
    Diff {
        /// 配置文件路径（相对 CWD；db 段提供目标库）
        #[arg(short, long, default_value = "config.yaml")]
        config: String,
        /// 服务目录；模式自动判定（含 manifests.yaml → release/js，否则 dev/ts）。
        /// 默认：自 config 同级向上逐级搜，每层 src 优先、dist 次之
        #[arg(short, long)]
        dir: Option<String>,
        /// 目标库（整轮）：config `db:` 段的 profile 名，缺省 default；未声明即 fail-fast
        #[arg(long)]
        db: Option<String>,
    },
}

/// 解析 argv；非法参数/帮助/空参由 clap 打印并退出（exit 2）。
pub fn parse_from(
    argv: impl IntoIterator<Item = impl Into<std::ffi::OsString> + Clone>,
) -> Command {
    to_command(Cli::parse_from(argv))
}

/// Cli → 领域参数（server.dir 的 dev 条件默认在此落地）。
fn to_command(cli: Cli) -> Command {
    match cli.command {
        Commands::Serve {
            config,
            base,
            api_path,
            app_path,
            cert_path,
            key_path,
            console_log,
            daemon,
        } => Command::Serve(ServeArgs {
            config,
            base,
            api_path,
            app_path,
            cert_path,
            key_path,
            console_log,
            daemon,
        }),
        Commands::Build {
            module,
            config,
            dir,
            out,
            no_minify,
            check,
        } => Command::Build(BuildArgs {
            module,
            config,
            dir,
            out,
            minify: !no_minify,
            check,
        }),
        Commands::Test {
            config,
            base,
            dir,
            tests,
            format,
            output,
            db,
            anonymous,
        } => Command::Test(TestArgs {
            config,
            base,
            dir,
            tests,
            format,
            output,
            db,
            anonymous,
        }),
        Commands::Migrate {
            config,
            dir,
            db,
            baseline,
            module,
        } => Command::Migrate(MigrateArgs {
            config,
            dir,
            baseline,
            module,
            db,
        }),
        Commands::Fixture {
            config,
            dir,
            db,
            module,
        } => Command::Fixture(FixtureArgs {
            config,
            dir,
            module,
            db,
        }),
        Commands::Schema {
            command: SchemaCmd::Diff { config, dir, db },
        } => Command::SchemaDiff(SchemaDiffArgs { config, dir, db }),
        Commands::Secret {
            command:
                SecretCmd::Keygen {
                    bits,
                    out_dir,
                    force,
                },
        } => Command::SecretKeygen(SecretKeygenArgs {
            bits,
            out_dir,
            force,
        }),
        Commands::Secret {
            command: SecretCmd::Seal { key, config, value },
        } => Command::SecretSeal(SecretSealArgs { key, config, value }),
        Commands::Secret {
            command: SecretCmd::Open { key, config, value },
        } => Command::SecretOpen(SecretOpenArgs { key, config, value }),
        Commands::Exec {
            file,
            config,
            dir,
            db,
            log_file,
            args,
        } => Command::Exec(ExecArgs {
            file,
            config,
            dir,
            db,
            log_file,
            args,
        }),
    }
}

#[cfg(test)]
mod tests {
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
        assert_eq!(a.file, "scripts/job.ts");
        assert_eq!(a.config, "c.yaml");
        assert_eq!(a.dir.as_deref(), Some("src"));
        assert_eq!(a.db.as_deref(), Some("report"));
        assert_eq!(a.log_file.as_deref(), Some("out.jsonl"));
        assert_eq!(a.args, ["-x", "foo bar"]);
        // 无 `--` → args 为空。
        let Command::Exec(a) = cmd(&["exec", "s.ts", "-c", "c.yaml"]) else {
            panic!()
        };
        assert!(a.args.is_empty());
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
                a.config.as_str(),
                a.base.as_deref(),
                a.api_path.as_deref(),
                a.app_path.as_slice()
            ),
            (
                "c.yaml",
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
        // keygen：--bits / --out-dir / --force。
        let Command::SecretKeygen(a) = cmd(&[
            "secret",
            "keygen",
            "--bits",
            "4096",
            "--out-dir",
            "keys",
            "--force",
        ]) else {
            panic!()
        };
        assert_eq!((a.bits, a.out_dir.as_str(), a.force), (4096, "keys", true));
        let Command::SecretKeygen(a) = cmd(&["secret", "keygen"]) else {
            panic!()
        };
        assert_eq!((a.bits, a.out_dir.as_str(), a.force), (2048, ".", false));
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
        // 未知子命令 / 未知旗标仍由 clap 拒（--bits 只属于 keygen）。
        assert!(cli(&["secret", "nope"]).is_err());
        assert!(cli(&["secret", "seal", "--bits", "1"]).is_err());
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
        // --dev 已删：模式由 --api-path 目录自动判定（server_cmd::is_release）
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
        let Command::Test(a) = cmd(&[
            "test", "-c", "c.yaml", "-d", "src", "-t", "tests", "--format", "tap", "--output",
            "r.tap",
        ]) else {
            panic!()
        };
        assert_eq!(
            (
                a.config.as_str(),
                a.dir.as_deref(),
                a.tests.as_deref(),
                a.format.as_deref(),
                a.output.as_deref()
            ),
            (
                "c.yaml",
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
            (a.db.as_deref(), a.config.as_str()),
            (Some("analytics"), "c.yaml")
        );
        let Command::Migrate(a) = cmd(&["migrate"]) else {
            panic!()
        };
        assert!(a.db.is_none());
        let Command::Fixture(a) = cmd(&["fixture", "--db", "test"]) else {
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
        let Command::Fixture(a) = cmd(&["fixture", "-c", "c.yaml", "-d", "src"]) else {
            panic!()
        };
        assert_eq!(
            (a.config.as_str(), a.dir.as_deref()),
            ("c.yaml", Some("src"))
        );
        let Command::SchemaDiff(a) = cmd(&["schema", "diff", "-c", "c.yaml"]) else {
            panic!()
        };
        assert_eq!((a.config.as_str(), a.dir.as_deref()), ("c.yaml", None));
    }
}
