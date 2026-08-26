use oj::args::{self, Command};

/// 执行解析后的命令，返回进程退出码（0 成功 / 1 业务错误）。
pub async fn run_command(cmd: Command) -> i32 {
    match cmd {
        Command::Build(a) => match oj::build_cmd::run(&a).await {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("oj build: {e}");
                1
            }
        },
        Command::Server(a) => match oj::server_cmd::run(a).await {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("oj server: {e}");
                1
            }
        },
        Command::Test(a) => match oj::test_cmd::run(a) {
            Ok(code) => code,
            Err(e) => {
                eprintln!("oj test: {e}");
                1
            }
        },
    }
}

#[tokio::main]
async fn main() {
    let code = run_command(args::parse_from(std::env::args_os())).await;
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;
    use oj::args::BuildArgs;
    use std::path::PathBuf;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("oj-main-{}-{}-{}", name, std::process::id(), std::sync::atomic::AtomicUsize::new(0).fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn build_ok_returns_zero() {
        let d = tmp("build-ok");
        std::fs::create_dir_all(d.join("src/u")).unwrap();
        std::fs::write(d.join("src/u/manifest.yaml"), "name: u\ndesc: d\nversion: 0.1.0\n").unwrap();
        std::fs::write(d.join("src/u/api.ts"), "export default { get() { json.ok({}); } };\n").unwrap();
        let a = BuildArgs {
            module: None,
            dir: d.join("src").display().to_string(),
            out: d.join("dist").display().to_string(),
            minify: true,
        };
        let code = run_command(Command::Build(a)).await;
        assert_eq!(code, 0);
        assert!(d.join("dist/u-0.1.0/api.js").is_file());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn build_missing_manifest_returns_one() {
        let d = tmp("build-err");
        std::fs::create_dir_all(d.join("src/foo")).unwrap();
        // foo 目录无 manifest.yaml → 构建失败。
        let a = BuildArgs {
            module: Some("foo".into()),
            dir: d.join("src").display().to_string(),
            out: d.join("dist").display().to_string(),
            minify: true,
        };
        let code = run_command(Command::Build(a)).await;
        assert_eq!(code, 1);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn server_missing_config_returns_one() {
        let code = run_command(Command::Server(oj::args::ServerArgs {
            config: "no-such-config.yaml".into(),
            base: None,
            dir: None,
            cert_path: None,
            key_path: None,
            grace_days: None,
        }))
        .await;
        assert_eq!(code, 1);
    }
}
