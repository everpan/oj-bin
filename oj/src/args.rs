//! oj 命令行（clap derive）。子命令：server / build。
//! 帮助 `oj --help` / `oj <cmd> --help`；空参打印帮助、非法参数报错均由 clap 退出（code 2）。

use clap::{Parser, Subcommand};

/// server 子命令参数。
#[derive(Debug, Clone, Default)]
pub struct ServerArgs {
    pub config: String,
    /// None → 用 config 的 server.base（默认 /v1/api）。
    pub base: Option<String>,
    /// 后端 API 目录（src 源码树或 oj build 产物 dist）。None → 默认目录
    /// （src 存在取 src，否则 dist）；模式按目录内容自动判定（server_cmd）。
    pub api_path: Option<String>,
    /// 静态站点目录；Some 覆盖 config 的 server.app_path。
    pub app_path: Option<String>,
    /// JWS 证书路径；Some 覆盖 config 的 server.certificate_path。
    pub cert_path: Option<String>,
    /// PEM 公钥路径；Some 覆盖 config 的 server.public_key_path。
    pub key_path: Option<String>,
    /// `--console-log`：true → 打开终端输出（默认关闭，只落盘）。
    /// 打开 config 的 server.console_log 之外的另一条通路（两者为「或」）。
    pub console_log: bool,
}

/// test 子命令参数（L1：进程内真实运行时跑 *.test.ts）。
pub struct TestArgs {
    pub config: String,
    /// None → 用 config 的 server.base（默认 /v1/api）。
    pub base: Option<String>,
    /// None → 默认目录（src 存在取 src，否则 dist）；模式按目录内容自动判定。
    pub dir: Option<String>,
    /// 测试用例目录：绝对路径原样；相对 → 相对 config_dir（项目根）。默认 "tests"。
    pub tests: Option<String>,
    /// 报告格式：human（默认，可读摘要）/ tap / junit / json，便于 CI 统一收口。
    pub format: Option<String>,
    /// 报告输出文件；省略则打到 stdout。machine 格式（tap/junit/json）配合此旗标落盘。
    pub output: Option<String>,
}

/// `oj build [module] [-d src] [-o dist] [--no-minify] [--check]`（src → dist，生成 routes.js）。
pub struct BuildArgs {
    pub module: Option<String>,
    pub dir: String,
    pub out: String,
    /// 转译产物 minify（单行、剥注释）。默认开；`--no-minify` 排障逃生门。
    pub minify: bool,
    /// 只跑结构检查（S002–S007）不落盘（§5.2 CI 门禁 / 本地快查）。
    pub check: bool,
}

/// `oj migrate [-c config] [-d dir] [--baseline] [--module M]`。
pub struct MigrateArgs {
    pub config: String,
    /// None → 默认目录（src 存在取 src，否则 dist）；模式按目录内容自动判定。
    pub dir: Option<String>,
    /// 存量库接入门：全部迁移记为已应用而不执行（P0 建过表的库，Q5）。
    pub baseline: bool,
    /// 只迁移指定模块。
    pub module: Option<String>,
}

/// `oj fixture [-c config] [-d dir] [--module M]`。
pub struct FixtureArgs {
    pub config: String,
    pub dir: Option<String>,
    /// 只灌指定模块。
    pub module: Option<String>,
}

/// 解析结果（错误/帮助/空参由 clap 处理，不会走到这里）。
pub enum Command {
    Server(ServerArgs),
    Build(BuildArgs),
    Test(TestArgs),
    Migrate(MigrateArgs),
    Fixture(FixtureArgs),
    SchemaDiff(SchemaDiffArgs),
}

/// `oj schema diff [-c config] [-d dir]`：声明 vs 实库只读对账（D001/D002，§5.1）。
pub struct SchemaDiffArgs {
    pub config: String,
    /// None → 默认目录（src 存在取 src，否则 dist）；模式按目录内容自动判定。
    pub dir: Option<String>,
}

/// oj：目录镜像路由的 JS 服务与构建 CLI。
#[derive(Debug, Parser)]
#[command(name = "oj", version, arg_required_else_help = true)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// 启动 HTTP 服务（目录镜像路由 + app_path 静态兜底）
    #[command(arg_required_else_help = true)]
    Server {
        /// 配置文件路径（相对 CWD；server.host/port/app_path + db/redis）
        #[arg(short, long, default_value = "config.yaml")]
        config: String,
        /// API 基础路由前缀；缺省用 config 的 server.base（默认 /v1/api）
        #[arg(short, long)]
        base: Option<String>,
        /// 后端 API 目录（src 源码树或 oj build 产物 dist）；
        /// 模式自动判定（含 manifests.yaml → release/js，否则 dev/ts）。
        /// 缺省：src 目录存在取 src，否则 dist
        #[arg(long = "api-path")]
        api_path: Option<String>,
        /// 静态站点目录（覆盖 config 的 server.app_path）
        #[arg(long = "app-path")]
        app_path: Option<String>,
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
    },
    /// 构建模块产物（src → dist：版本目录 / routes.js / tgz）
    Build {
        /// 目标模块名（src 首层子目录）；省略 = 全部模块
        module: Option<String>,
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
    /// 跑 sample API 测试（无需启动 oj server；进程内真实运行时派发）
    Test {
        /// 配置文件路径（相对 CWD；server.host/port/root + db/redis）
        #[arg(short, long, default_value = "config.yaml")]
        config: String,
        /// API 基础路由前缀；缺省用 config 的 server.base（默认 /v1/api）
        #[arg(short, long)]
        base: Option<String>,
        /// 服务目录；模式自动判定（含 manifests.yaml → release/js，否则 dev/ts）。
        /// 默认：src 目录存在取 src，否则 dist
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
    },
    /// 应用模块迁移到最新（migrations/*.sql → default 库；部署 = build && migrate && server）
    Migrate {
        /// 配置文件路径（相对 CWD；db 段提供目标库）
        #[arg(short, long, default_value = "config.yaml")]
        config: String,
        /// 服务目录；模式自动判定（含 manifests.yaml → release/js，否则 dev/ts）。
        /// 默认：src 目录存在取 src，否则 dist
        #[arg(short, long)]
        dir: Option<String>,
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
        /// 只灌指定模块
        module: Option<String>,
    },
    /// 声明式 schema 运维
    Schema {
        #[command(subcommand)]
        command: SchemaCmd,
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
        /// 默认：src 目录存在取 src，否则 dist
        #[arg(short, long)]
        dir: Option<String>,
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
        Commands::Server {
            config,
            base,
            api_path,
            app_path,
            cert_path,
            key_path,
            console_log,
        } => Command::Server(ServerArgs {
            config,
            base,
            api_path,
            app_path,
            cert_path,
            key_path,
            console_log,
        }),
        Commands::Build {
            module,
            dir,
            out,
            no_minify,
            check,
        } => Command::Build(BuildArgs {
            module,
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
        } => Command::Test(TestArgs {
            config,
            base,
            dir,
            tests,
            format,
            output,
        }),
        Commands::Migrate {
            config,
            dir,
            baseline,
            module,
        } => Command::Migrate(MigrateArgs {
            config,
            dir,
            baseline,
            module,
        }),
        Commands::Fixture {
            config,
            dir,
            module,
        } => Command::Fixture(FixtureArgs {
            config,
            dir,
            module,
        }),
        Commands::Schema {
            command: SchemaCmd::Diff { config, dir },
        } => Command::SchemaDiff(SchemaDiffArgs { config, dir }),
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
    fn server_defaults_and_overrides() {
        // 默认值：base=None（config server.base 兜底） / api_path=None / app_path=None。
        // 裸 `oj server` 现在打印帮助（arg_required_else_help），给一个参数才进入解析，
        // 故默认值用 -c 触发；config 默认 "config.yaml" 由 clap default_value 保证。
        let Command::Server(a) = cmd(&["server", "-c", "config.yaml"]) else {
            panic!()
        };
        assert_eq!(
            (
                a.base.as_deref(),
                a.api_path.as_deref(),
                a.app_path.as_deref()
            ),
            (None, None, None)
        );
        let Command::Server(a) = cmd(&[
            "server",
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
                a.app_path.as_deref()
            ),
            ("c.yaml", Some("/api"), Some("src"), Some("web"))
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
    fn bad_usage_is_clap_error() {
        let cli =
            |argv: &[&str]| Cli::try_parse_from(std::iter::once("oj").chain(argv.iter().copied()));
        // -b 已不是 build 参数：clap 拒绝（不再吞值静默全量构建）
        assert!(cli(&["build", "-b", "/x"]).is_err());
        // 第二个位置参数不再静默丢弃
        assert!(cli(&["build", "user", "other"]).is_err());
        // 未知子命令 / 未知长旗标
        assert!(cli(&["foo"]).is_err());
        assert!(cli(&["server", "--nope"]).is_err());
        // --dev 已删：模式由 --api-path 目录自动判定（server_cmd::is_release）
        assert!(cli(&["server", "--dev"]).is_err());
        // server 的 -d/--dir 已删（改为 --api-path）：clap 拒绝
        assert!(cli(&["server", "-d", "src"]).is_err());
        assert!(cli(&["server", "--dir", "src"]).is_err());
        // --grace-days 已删：宽限天数仅由 config 的 server.grace_days 提供
        assert!(cli(&["server", "--grace-days", "30"]).is_err());
        // 空参 → 帮助（arg_required_else_help）
        assert_eq!(
            cli(&[]).unwrap_err().kind(),
            ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );
        // `oj server` 裸调（无任何参数）→ 帮助；给了参数（如 -c）才真正启动
        assert_eq!(
            cli(&["server"]).unwrap_err().kind(),
            ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );
        assert!(cli(&["server", "-c", "c.yaml"]).is_ok());
    }
}
