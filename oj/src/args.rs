//! oj 参数解析（纯函数）。v0.1 子命令：server（build 占位）。

/// server 子命令参数。
pub struct ServerArgs {
    pub config: String,
    pub base: String,
    pub dir: String,
    pub dev: bool,
}

/// `oj build [module] [-d src] [-o dist]`（src → dist，生成 routes.js）。
pub struct BuildArgs {
    pub module: Option<String>,
    pub dir: String,
    pub out: String,
}

/// 解析结果。None = 无子命令（main 打用法）；Err = 参数错误（main exit 1）。
pub enum Command {
    Server(ServerArgs),
    Build(BuildArgs),
    Err(String),
    None,
}

/// `oj server [-c config.yaml] [-b /v1/api] [-d src|dist] [--dev]` / `oj build [module] [-d src] [-o dist]`。
pub fn parse(args: &[String]) -> Command {
    let mut it = args.iter();
    match it.next().map(|s| s.as_str()) {
        Some("build") => {
            let (mut module, mut dir, mut out) = (None, String::new(), String::new());
            let mut cur = it.clone().peekable();
            while let Some(a) = cur.next() {
                match a.as_str() {
                    // 已删参数：吞值会让 `-b user` 静默变成全量构建，值得专属报错
                    "-b" => return Command::Err("oj build no longer takes -b".into()),
                    "-d" | "-o" => {
                        if let Some(v) = cur.next() {
                            match a.as_str() {
                                "-d" => dir = v.clone(),
                                _ => out = v.clone(),
                            }
                        }
                    }
                    s if s.starts_with('-') => {}
                    _ => {
                        if module.is_none() {
                            module = Some(a.clone());
                        } else {
                            eprintln!("oj build: ignoring extra module {a:?}");
                        }
                    }
                }
            }
            Command::Build(BuildArgs {
                module,
                dir: if dir.is_empty() { "src".into() } else { dir },
                out: if out.is_empty() { "dist".into() } else { out },
            })
        }
        Some("server") => {
            let (mut config, mut base, mut dir, mut dev) =
                (String::new(), String::new(), String::new(), false);
            let mut cur = it.clone().peekable();
            while let Some(a) = cur.next() {
                match a.as_str() {
                    "-c" | "-b" | "-d" => {
                        if let Some(v) = cur.next() {
                            match a.as_str() {
                                "-c" => config = v.clone(),
                                "-b" => base = v.clone(),
                                _ => dir = v.clone(),
                            }
                        }
                    }
                    "--dev" => dev = true,
                    _ => {}
                }
            }
            let dir = if dir.is_empty() { if dev { "src".into() } else { "dist".into() } } else { dir };
            Command::Server(ServerArgs {
                config: if config.is_empty() { "config.yaml".into() } else { config },
                base: if base.is_empty() { "/v1/api".into() } else { base },
                dir,
                dev,
            })
        }
        _ => Command::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_branches() {
        // 无参 → None；server 默认值（dev=false, dir=dist）；显式覆盖。
        assert!(matches!(parse(&args(&[])), Command::None));
        let Command::Server(a) = parse(&args(&["server"])) else { panic!() };
        assert_eq!((a.config.as_str(), a.base.as_str(), a.dir.as_str(), a.dev),
                   ("config.yaml", "/v1/api", "dist", false));
        let Command::Server(a) = parse(&args(&["server", "-c", "c.yaml", "-b", "/api",
                                               "-d", "dist"])) else { panic!() };
        assert_eq!((a.config.as_str(), a.base.as_str(), a.dir.as_str(), a.dev),
                   ("c.yaml", "/api", "dist", false));
        let Command::Server(a) = parse(&args(&["server", "-b", "/api"])) else { panic!() };
        assert_eq!((a.config.as_str(), a.base.as_str(), a.dir.as_str(), a.dev),
                   ("config.yaml", "/api", "dist", false));
        let Command::Server(a) = parse(&args(&["server", "--dev", "-d", "x"])) else { panic!() };
        assert!(a.dev && a.dir == "x");
    }

    #[test]
    fn build_parses_module_positional() {
        let Command::Build(a) = parse(&args(&["build"])) else { panic!() };
        assert_eq!((a.module.as_deref(), a.dir.as_str(), a.out.as_str()), (None, "src", "dist"));
        let Command::Build(a) = parse(&args(&["build", "user"])) else { panic!() };
        assert_eq!(a.module.as_deref(), Some("user"));
        let Command::Build(a) = parse(&args(&["build", "user", "-d", "s", "-o", "d"])) else { panic!() };
        assert_eq!((a.module.as_deref(), a.dir.as_str(), a.out.as_str()), (Some("user"), "s", "d"));
        // -b 已删：显式报错（不再吞值静默全量构建）
        assert!(matches!(parse(&args(&["build", "-b", "/x"])), Command::Err(e) if e.contains("-b")));
        // 第二个位置参数被丢弃 → eprintln 提示，首个生效
        let Command::Build(a) = parse(&args(&["build", "user", "other"])) else { panic!() };
        assert_eq!(a.module.as_deref(), Some("user"));
    }
}
