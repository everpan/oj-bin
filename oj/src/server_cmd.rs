//! oj server 装配：config → 逐 db 开库（仅 sqlite）→ seed → manifest 校验 →
//! actor 池 → axum serve。start() 返回 (addr, join_handle)，main 与测试共用。

#![allow(clippy::collapsible_if)]
use std::collections::HashMap;
use std::net::SocketAddr;
use std::net::ToSocketAddrs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use only_js::bridge::plugin_loader::{
    LoadedPlugin, PluginManifestEntry, blob_backend_connect, bus_backend, db_backend, es_backend,
    host_context, load_manifest, load_scanned, resolve_plugins_dir,
};
use only_js::bridge::{BusBackendRegistry, DataAccessor, DbBackendRegistry, EsBackend, PluginInfo};
use only_js::config::{self, Config, StaticSiteConf};

use crate::app::App;
use crate::args::ServerArgs;

pub async fn run(a: ServerArgs) -> Result<(), String> {
    // --daemon：re-exec 自身（剥掉 --daemon）脱离终端后父进程即退；
    // 子进程日志照常落 server.logs_dir（console 默认关闭）。
    if a.daemon {
        return daemonize();
    }
    let (mut cfg, config_dir, dir, ts, base) =
        load_app_config(&a.config, a.api_path.as_deref(), a.base.as_deref())?;
    // CLI 覆盖：静态站点目录 / 证书路径（若有）。强制证书门禁在 App::from_config
    // （统一装配点）判定，CLI 与测试共用同一路径，避免 run()/start() 两处判空漂移。
    // 路径语义：CLI `--app-path` 相对 CWD（此处预绝对化）；config `server.app_path`
    // 相对 config_dir（装配期站点表统一处理）。
    if !a.app_path.is_empty() {
        fold_cli_app_paths(&mut cfg, &a.app_path)?;
    }
    if let Some(p) = a.cert_path {
        cfg.server.certificate_path = p;
    }
    if let Some(p) = a.key_path {
        cfg.server.public_key_path = p;
    }
    // 准入门（admission_gate）：api（--api-path）与静态（server.app_path /
    // server.static_sites / CLI --app-path）至少显式指定其一。静态目录存在性统一由
    // 装配期站点表解析 fail-fast（含具体 prefix 来源）。
    let api_specified = a.api_path.is_some();
    let app_specified = cfg.server.app_path.is_some() || !cfg.server.static_sites.is_empty();
    admission_gate(
        if api_specified {
            Some(dir.as_path())
        } else {
            None
        },
        app_specified,
    )?;
    // 纯静态模式：api 功能未启用。from_config 需要看到「无 API 目录」（模块扫描
    // NotFound = 空 = 无路由），而 load_app_config 的默认搜索可能已命中 src/，
    // 故传必然缺失的占位路径，避免把搜到的 API 目录静默挂上来。
    let dir = if api_specified {
        dir
    } else {
        eprintln!("note: no --api-path — serving static site only");
        config_dir.join(".oj-static-only")
    };
    // 初始化日志：目录默认 config 相对 ./logs，可在 server.logs_dir 配置；不存在自动创建。
    // 大小滚动参数 server.logs_max_bytes / logs_keep_files。
    let logs_dir = server::logging::resolve_logs_dir(cfg.server.logs_dir.as_deref(), &config_dir);
    // 终端输出默认关闭（只落盘）；config 的 server.console_log 或 CLI `--console-log`
    // 任一打开即打开（「或」而非「覆盖」——两个入口都是「打开」语义，没有「关闭」的一方）。
    let console = cfg.server.console_log || a.console_log;
    server::logging::init(
        &logs_dir,
        cfg.server.logs_max_m,
        cfg.server.logs_keep_files as usize,
        console,
    );
    let addr = to_socket_addrs_sync(&format!("{}:{}", cfg.server.host, cfg.server.port))?;
    let tasks_cfg = cfg.tasks.clone();
    let app =
        App::from_config(cfg, &config_dir, dir.clone(), base.clone(), ts, false, None).await?;
    // 长任务池（spec §6）：随服务拉起——扫描 <dir>/<tasks.dir>，空池/缺目录不报错；
    // 重名/超 max fail-fast。停机 flag 由 App 持有，信号处理器置位。
    let task_flag = app.tasks_flag();
    let sup = crate::tasks::TaskSupervisor::spawn_all(
        &tasks_cfg,
        &dir,
        app.make_task_bridge(),
        task_flag.clone(),
    )?;
    // 全仓首个信号处理器（评审 F5/S2）：SIGINT/SIGTERM → 置停机 flag → HTTP 侧
    // with_graceful_shutdown 同信号排空在途请求；任务线程在 grace 内自然收场
    // （不退者由 run_task 内置看门狗强杀）。
    let (bound, h) = app
        .serve_graceful(addr, shutdown_signal(task_flag.clone()))
        .await?;
    println!(
        "oj server listening on http://{bound}{} (dir={}, {})",
        base,
        dir.display(),
        if !api_specified {
            "static-only"
        } else if ts {
            "dev/ts"
        } else {
            "release/js"
        }
    );
    h.await.map_err(|e| format!("server task: {e}"))?;
    // 任务线程收场（flag 已置位；join 为阻塞调用，移交 blocking 池）。
    tokio::task::spawn_blocking(move || sup.shutdown())
        .await
        .map_err(|e| format!("tasks shutdown: {e}"))?;
    // 停机 graceful drain（spec §6 ⑤）：HTTP 已停收、任务已收场后，排空在途邮件
    // （插件侧停收 → 等在途 job 跑完 → 销毁 transport）。超时只告警，不阻断退出。
    app.drain_mail(MAIL_DRAIN_TIMEOUT).await;
    Ok(())
}

/// 停机 mail drain 的总超时：在途投递本身已在插件侧受 `profile.timeout` 约束，
/// 这里给 10s 上限 —— 足够跑完正常在途投递，又不会让 SIGTERM 后的退出过程长期挂住。
const MAIL_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// 后台运行入口（unix 分支）：re-exec 自身，剥掉 --daemon（避免子进程递归 daemon 化），
/// setsid 脱离控制终端（终端关闭的 SIGHUP 不再波及），stdio 重定向 /dev/null。
/// 父进程 spawn 成功即打印子 pid 返回；启动失败的真因见 server.logs_dir 落盘日志。
#[cfg(unix)]
fn daemonize() -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe().map_err(|e| format!("daemon: current_exe: {e}"))?;
    let null =
        std::fs::File::open("/dev/null").map_err(|e| format!("daemon: open /dev/null: {e}"))?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(strip_daemon_flag(std::env::args_os().skip(1)))
        .stdin(null)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Safety: setsid 是 async-signal-safe 的单调用，fork 后的子进程里只做这一件事。
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let child = cmd.spawn().map_err(|e| format!("daemon: spawn: {e}"))?;
    println!("oj server daemonized (pid {})", child.id());
    Ok(())
}

/// re-exec 参数剥离 --daemon（长旗标独占，无短形式，精确匹配即安全）。
fn strip_daemon_flag(
    args: impl IntoIterator<Item = std::ffi::OsString>,
) -> Vec<std::ffi::OsString> {
    args.into_iter().filter(|a| a != "--daemon").collect()
}

/// Windows 后台运行：DETACHED_PROCESS（不分配/不挂控制台，stdio 已重定向 null）
/// + CREATE_NEW_PROCESS_GROUP（父控制台的 Ctrl+C 事件不再波及子进程）。
#[cfg(windows)]
fn daemonize() -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    let exe = std::env::current_exe().map_err(|e| format!("daemon: current_exe: {e}"))?;
    let child = std::process::Command::new(exe)
        .args(strip_daemon_flag(std::env::args_os().skip(1)))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .spawn()
        .map_err(|e| format!("daemon: spawn: {e}"))?;
    println!("oj server daemonized (pid {})", child.id());
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn daemonize() -> Result<(), String> {
    Err("--daemon 仅支持 unix / windows 平台".to_string())
}

/// 停机信号（spec §6 ①）：SIGINT（ctrl_c）与 SIGTERM 二选一（Windows 仅 ctrl_c）；
/// 命中即置停机 flag（任务循环检测退出；HTTP 侧随之排空）。
async fn shutdown_signal(flag: Arc<std::sync::atomic::AtomicBool>) {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
    eprintln!("shutdown: stop flag set — draining tasks and in-flight requests");
    flag.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// 解析配置 + 目录模式（同 server）：读取 config.yaml，确定服务目录（src 优先 / dist 兜底）、
/// dev/release 判定、base 前缀归源。server 与 test 命令共用，避免重复解析逻辑。
pub fn load_app_config(
    config: &str,
    dir_override: Option<&str>,
    base_override: Option<&str>,
) -> Result<(Config, PathBuf, PathBuf, bool, String), String> {
    let config_path = PathBuf::from(config);
    let config_dir = config_dir_of(&config_path);
    let cfg = config::load_from(
        &config_dir,
        config_path.file_name().and_then(|s| s.to_str()),
    )
    .map_err(|e| format!("load config: {e}"))?;
    // 目录即模式：含构建锁 manifests.yaml → release(js)；否则 dev(ts)。
    // 默认目录：自 config 同级起步逐级向上搜索，每层 src 优先、dist 次之；
    // 一路到根都没找到 → 回落 config_dir/src。
    // 目录缺失不在此拦截：server 准入（api 与静态至少其一）由 run() 裁定，
    // migrate/fixture/test 强依赖 api 目录、各自就地报错。from_config 对缺失
    // 目录全程容忍（模块扫描 NotFound = 空 = 无模块，见 manifest::load_modules）。
    let dir = dir_override.map(PathBuf::from).unwrap_or_else(|| {
        let mut cur = Some(config_dir.as_path());
        loop {
            match cur {
                Some(d) => {
                    let src = d.join("src");
                    if src.is_dir() {
                        break src;
                    }
                    let dist = d.join("dist");
                    if dist.is_dir() {
                        break dist;
                    }
                    cur = d.parent();
                }
                None => break config_dir.join("src"),
            }
        }
    });
    let ts = !is_release(&dir);
    let base = resolve_base(base_override, &cfg.server.api_prefix)?;
    Ok((cfg, config_dir, dir, ts, base))
}

/// server 准入门（显式三态，无静默默认）：
/// - api 与 app 都指定 → **两者都必须存在**，任一缺失 Err 退出（显式要求的能力
///   缺失时静默降级是坑）；
/// - 只指定其一 → 该目录必须存在，仅启用对应功能（api 缺席 = 纯静态；app 缺席 = 纯 API）；
/// - 都未指定 → Err，提醒两者必须指定其一（不再自动搜索 src/dist 兜底）。
///
/// `api_dir`：Some = 指定了 `--api-path`（CLI）；`app_specified`：配置了任一静态站点
/// （config `server.app_path` / `server.static_sites` 或 CLI `--app-path`）。
/// 静态目录存在性不在此判——装配期站点表解析统一 fail-fast（含具体 prefix）。
fn admission_gate(api_dir: Option<&Path>, app_specified: bool) -> Result<(), String> {
    match (api_dir, app_specified) {
        (None, false) => Err(
            "neither api path (--api-path) nor static site (server.app_path / \
             server.static_sites / --app-path) specified — one of them is required to start"
                .to_string(),
        ),
        (None, true) => Ok(()), // 纯静态：目录存在性由装配期站点表解析 fail-fast。
        (Some(api), _) => {
            // api 必须存在；静态站点（若有）由装配期 fail-fast。
            if !api.is_dir() {
                return Err(format!(
                    "api path not found: {}（src 源码树或 oj build 产物 dist）",
                    api.display()
                ));
            }
            Ok(())
        }
    }
}

/// 相对路径按 `base`（CWD）绝对化；绝对路径原样。供 CLI `--app-path` 使用。
fn absolutize_cwd(base: &Path, p: &str) -> String {
    let path = Path::new(p);
    if path.is_absolute() {
        p.to_owned()
    } else {
        base.join(path).to_string_lossy().into_owned()
    }
}

/// api 前缀归源：CLI `-b` 显式给出 > config `server.api_prefix`（默认 /v1/api）。
/// 空前缀拒绝（全 404 的静默坑）。
fn resolve_base(cli: Option<&str>, cfg: &str) -> Result<String, String> {
    let b = cli.unwrap_or(cfg);
    if b.trim_matches('/').is_empty() {
        return Err("base prefix must not be empty (-b / server.api_prefix)".into());
    }
    Ok(b.to_string())
}

/// CLI `--app-path` 折叠进 config（v0.1.27，可重复）：
/// - 裸 `dir`（至多一次）→ 覆盖 `server.app_path`，按 CWD 预绝对化（legacy 单值语义）；
/// - `prefix=dir` → `server.static_sites` 中同前缀条目替换、否则追加（CLI 优先于 config）；
///   prefix 过 `resolve_app_prefix` 规范化校验。
fn fold_cli_app_paths(cfg: &mut Config, entries: &[String]) -> Result<(), String> {
    let cwd = std::env::current_dir().map_err(|e| format!("resolve --app-path: {e}"))?;
    let mut bare: Option<&str> = None;
    for e in entries {
        match e.split_once('=') {
            None => {
                if bare.is_some() {
                    return Err("--app-path <dir> (bare) may appear at most once; use prefix=dir for additional sites".into());
                }
                bare = Some(e);
            }
            Some((prefix, dir)) => {
                let p = resolve_app_prefix(prefix)
                    .map_err(|e| format!("--app-path prefix {prefix:?}: {e}"))?;
                let dir = absolutize_cwd(&cwd, dir);
                // 同前缀判定走规范化口径（config 侧可能带尾斜杠等未规范形态）。
                let idx = cfg
                    .server
                    .static_sites
                    .iter()
                    .position(|s| resolve_app_prefix(&s.prefix).is_ok_and(|x| x == p));
                match idx {
                    Some(i) => {
                        let s = &mut cfg.server.static_sites[i];
                        s.prefix = p;
                        s.path = dir;
                    }
                    None => cfg.server.static_sites.push(StaticSiteConf {
                        prefix: p,
                        path: dir,
                    }),
                }
            }
        }
    }
    if let Some(dir) = bare {
        cfg.server.app_path = Some(absolutize_cwd(&cwd, dir));
    }
    Ok(())
}

/// 静态站点前缀归一（`server.app_prefix`，默认 "/"）：必须以 `/` 开头；尾斜杠剪除；
/// 剪完为空 → "/"（根）。非 "/" 前缀时静态兜底仅服务该前缀下的 GET/HEAD。
/// 非法（不以 `/` 开头）→ Err fail-fast。
pub fn resolve_app_prefix(cfg: &str) -> Result<String, String> {
    if !cfg.starts_with('/') {
        return Err(format!("server.app_prefix must start with '/': {cfg:?}"));
    }
    let trimmed = cfg.trim_end_matches('/');
    Ok(if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    })
}

/// 模式判定：服务目录含 `manifests.yaml`（oj build 锁文件）→ release 产物树。
/// src 源码树无此文件 → dev。两类目录形态互斥，判据确定。
fn is_release(dir: &Path) -> bool {
    dir.join("manifests.yaml").is_file()
}

/// 装配并监听（port=0 → 随机端口，测试用）。维持旧签名（返回 (SocketAddr, JoinHandle)），
/// 现有 `cargo test` 端口 0 + reqwest 用例零改动——内部委托 `App::from_config` + `App::serve`。
pub async fn start(
    cfg: Config,
    config_dir: &Path,
    dir: PathBuf,
    base: String,
    ts: bool,
) -> Result<(SocketAddr, tokio::task::JoinHandle<()>), String> {
    let addr = to_socket_addrs_sync(&format!("{}:{}", cfg.server.host, cfg.server.port))?;
    let app = App::from_config(cfg, config_dir, dir, base, ts, false, None).await?;
    app.serve(addr).await
}

/// config 文件的父目录（即 project_root）。bare 文件名（`parent()==""`）回落当前目录，
/// 避免 config_dir 为空 → `canonicalize("")` 失败 → project_root 钳制静默失效。
fn config_dir_of(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}

/// `host:port` → 首个解析地址（阻塞式，仅启动调用一次）。
fn to_socket_addrs_sync(s: &str) -> Result<SocketAddr, String> {
    s.to_socket_addrs()
        .map_err(|e| format!("resolve {s}: {e}"))?
        .next()
        .ok_or_else(|| format!("resolve {s}: no addresses"))
}

/// blob 命名多后端装配：逐条目构造（local root 相对 config_dir 绝对化；
/// s3 经 blob 插件 vtable connect，cfg 按值 JSON 透传）。配置声明即必须成功注册，
/// 缺一启动期报错（fail fast，spec §2）。driver != local 且无 blob 插件 → fail fast。
pub async fn assemble_blobs(
    section: &config::BlobSection,
    config_dir: &Path,
    base: &str,
    blob_vt: Option<&'static oj_plugin_ffi::BlobBackendVtable>,
) -> Result<Arc<only_js::bridge::blob::BlobRegistry>, String> {
    let mut r = only_js::bridge::blob::BlobRegistry::new();
    for (name, c) in section.entries()? {
        let backend: Arc<dyn only_js::bridge::BlobBackend> = match c.driver.as_str() {
            "local" => {
                let root = Path::new(&c.root);
                let root = if root.is_absolute() {
                    root.to_path_buf()
                } else {
                    config_dir.join(root)
                };
                Arc::new(
                    only_js::bridge::LocalBlob::named(&name, &root, base)
                        .map_err(|e| format!("blob '{name}': {e}"))?,
                )
            }
            "s3" => {
                let vt = blob_vt.ok_or_else(|| {
                    format!("blob '{name}': driver 's3' requires the oj-blob-s3 plugin (run `cargo xtask plugin blob-s3`)")
                })?;
                let cfg_json = serde_json::to_string(&c)
                    .map_err(|e| format!("blob '{name}': serialize cfg: {e}"))?;
                blob_backend_connect(vt, &name, &cfg_json)
                    .await
                    .map_err(|e| format!("blob '{name}': {e}"))?
            }
            other => {
                return Err(format!(
                    "blob '{name}': driver must be local|s3, got {other:?}"
                ));
            }
        };
        r.register(&name, backend)
            .map_err(|e| format!("blob '{name}': {e}"))?;
    }
    Ok(Arc::new(r))
}

/// 逐 db 开库（server/build 共用）：经注册表按 scheme 认领，错误文案带库名。
pub async fn connect_dbs(
    cfg_db: &HashMap<String, String>,
    registry: &only_js::bridge::DbBackendRegistry,
    config_dir: &Path,
) -> Result<HashMap<String, Arc<dyn DataAccessor>>, String> {
    let mut dbs = HashMap::new();
    for (name, dsn) in cfg_db {
        let acc = registry
            .connect(dsn, config_dir)
            .await
            .map_err(|e| format!("open db '{name}': {e}"))?;
        dbs.insert(name.clone(), acc);
    }
    Ok(dbs)
}

/// 装配产物：插件注册的后端槽位（es 键选单后端 + db 认领式注册表 + blob 键选
/// 单后端 vtable 槽 + bus 键选注册表 + kv 键选单 vtable 槽）。
#[derive(Default)]
pub struct Registries {
    pub es: Option<Arc<dyn EsBackend>>,
    pub dbs: DbBackendRegistry,
    /// 插件 blob vtable（Task 4.2：单槽位，多 blob 插件注册冲突 fail fast；s3 驱动
    /// 经它 connect，装配期逐后端调用）。
    pub blob: Option<&'static oj_plugin_ffi::BlobBackendVtable>,
    /// bus 键选注册表（Task 4.3：内置 local + 插件 kafka/rabbitmq 工厂；kind 冲突 fail fast）。
    pub bus: BusBackendRegistry,
    /// kv 键选单 vtable 槽（Task 4.4：redis.default 声明 → 经插件 connect；
    /// 未声明仍 InMemoryKV 内置兜底；多 kv 插件注册冲突 fail fast）。
    pub kv: Option<&'static oj_plugin_ffi::KVStoreVtable>,
    /// auth 键选单 vtable 槽（auth 解耦：cfg [auth] 声明 → 必须恰有一个 auth 插件，
    /// 缺失/多插件冲突都 fail fast；未声明时插件槽位不进 Pipeline）。
    pub auth: Option<&'static oj_plugin_ffi::AuthGuardVtable>,
    /// mq 命名客户端 vtable 表（spec 2026-09-07：插件名 bus-<kind> 路由；
    /// kafkas:/rabbits: 段声明 → 按名找 vtable → connect 出命名实例）。
    pub mq: Vec<(String, &'static oj_plugin_ffi::MqVtable)>,
    /// mail 键选单 vtable 槽（mail 轴，spec 2026-09-15）：smtp: 段 + 恰一个 mail 插件
    /// → 装配层构造 `FfiMailBackend`（`app::build_mail_backend`）；多 mail 插件注册冲突
    /// fail fast。未加载插件时保持 None（`mail.*` 报未配置，不降级）。
    pub mail: Option<&'static oj_plugin_ffi::MailVtable>,
}

/// 装配层把宿主侧解析出的跨后端参数经 cfg JSON 注入插件（spec §3 有意的边界；
/// cfg 按值传入，插件须持久化时自行持有副本）。
/// 第一方轴适配器清单（实际适配分支在 plugin_cfg 的 match；此表供测试与
/// plugin_loader::AXES 对账，防止两表失步——适配器打空）。加新第一方轴时在此登记。
#[cfg(test)]
const ADAPTER_AXES: &[&str] = &["es", "auth", "mail"];

/// A5：mail 的**双配置源**闸门（装配期 fail-fast）。
///
/// `plugin_cfg` 对 mail 有两条来源：非空 `plugins.mail`（原样透传）与顶层 `smtp:` 段
/// （适配器臂）；**前者静默胜出**。运维若在 `smtp:` 里改了白名单/凭据（或反之），改动会
/// 悄无声息地不生效——典型的「静默遮蔽」。故两者**皆非空**时装配期直接报错，明确「二选一」，
/// 与 es/kv「声明即校验」同一取向（配置写错在装配期暴露，不静默改变语义）。
///
/// 「非空」判定与 `plugin_cfg` 的选用条件逐条对齐：`plugins.mail` 是**非空对象**；
/// `smtp:` 段则要求**有实质内容**（profile 或 `workers`/`queue_capacity`）——
/// `smtp: {}`（空段）本来就被视作未配置，不算冲突（`plugins: {mail: {}}` 空对象同理，
/// 它是「回落适配器」而非透传）。
pub(crate) fn check_mail_cfg_sources(cfg: &Config) -> Result<(), String> {
    let passthrough = cfg
        .plugins
        .get("mail")
        .is_some_and(|v| v.as_object().is_some_and(|o| !o.is_empty()));
    let smtp_nonempty = cfg.smtp.as_ref().is_some_and(|s| {
        s.workers.is_some() || s.queue_capacity.is_some() || !s.profiles.is_empty()
    });
    if passthrough && smtp_nonempty {
        return Err(
            "config declares both a non-empty `smtp:` section and a non-empty `plugins.mail` \
             entry: pick one (`smtp:` is the standard form; `plugins.mail` is a raw passthrough \
             that silently wins over `smtp:` and bypasses its typed parsing)"
                .to_string(),
        );
    }
    Ok(())
}

/// cfg 回落：plugins.<name> 非空对象原样透传 → 轴适配器 → "{}"。
/// schema 归插件所有：插件在 init 校验，非法即 Err fail-fast（宿主不解释字段）。
/// **唯一例外**：`"mail"` 的适配器产物同时是**宿主**的白名单校验面（`MailConfig::from_value`
/// 吃同一份 JSON）——故 white-list/并发/凭据字段必须一并过线，两条路（透传与适配器）
/// 都得让宿主看得见 `allowed_*`，否则宿主与插件配置面分叉。
/// `pub(crate)`：`app::build_mail_backend` 复用同一份产物（单一真相源）。
///
/// 两条 mail 来源同时非空是**配置错误**：见 [`check_mail_cfg_sources`]（装配期 fail-fast）。
pub(crate) fn plugin_cfg(cfg: &Config, name: &str) -> String {
    if let Some(v) = cfg.plugins.get(name)
        && v.as_object().is_some_and(|o| !o.is_empty())
    {
        return v.to_string();
    }
    match name {
        "es" => match &cfg.es {
            Some(es) => serde_json::json!({ "endpoint": es.endpoint }).to_string(),
            None => "{}".to_string(),
        },
        "auth" => match &cfg.auth {
            Some(a) => serde_json::json!({
                "jwt_secret": a.jwt_secret,
                "signing_method": a.signing_method,
                // 插件只吃路径字符串（v0.1.23 起宿主支持条目对象形态，插件侧零改动）。
                "anonymous_paths": only_js::config::anon_paths(&a.anonymous_paths),
            })
            .to_string(),
            None => "{}".to_string(),
        },
        // mail（spec 2026-09-15）：顶层 `smtp:` 段 → oj-mail 插件 cfg。
        // 段缺省/空 → "{}"（插件零 profile；装配层视作未配置，不挂 mail 后端）。
        "mail" => match &cfg.smtp {
            Some(s) => serde_json::to_string(s).unwrap_or_else(|_| "{}".to_string()),
            None => "{}".to_string(),
        },
        _ => "{}".to_string(),
    }
}

/// 注册：插件后端（插件先于内置）→ 内置后端。
/// es 为键选式单后端：cfg es: 声明 + 恰好一个 es 插件 → FfiEsBackend(handle 0)；
/// 「配置声明了能力但插件未装」→ fail fast（§2 闸门）；多个 es 注册冲突 → fail fast。
/// db 为认领式：内置 sqlite/memory 打底 + 每个插件 db 工厂注册（scheme 交集冲突 → fail fast）。
/// blob 为键选式单后端 vtable 槽：多个 blob 插件冲突 fail fast；「s3 驱动但无 blob 插件」
/// 在 assemble_blobs 逐后端判定（driver != local 且无插件 → fail fast）。
/// bus 为键选式注册表：内置 local 打底 + 每个插件 bus 工厂注册（kind 冲突 fail fast；
/// kafka/rabbitmq kind 未装插件 → "unknown broker kind" 明确报错）。
fn build_registries(cfg: &Config, loaded: &[LoadedPlugin]) -> Result<Registries, String> {
    let es_plugins: Vec<&LoadedPlugin> = loaded
        .iter()
        .filter(|p| p.registrations.es.is_some())
        .collect();
    if cfg.es.is_some() && es_plugins.is_empty() {
        return Err(
            "config declares [es] but no es plugin loaded (run `cargo xtask plugin es`)"
                .to_string(),
        );
    }
    if es_plugins.len() > 1 {
        return Err("plugins conflict: multiple plugins register es backend".to_string());
    }
    let es = es_plugins.first().and_then(|p| es_backend(p));
    let mut dbs = DbBackendRegistry::builtin();
    for p in loaded {
        if let Some(be) = db_backend(p) {
            dbs.register(be)
                .map_err(|e| format!("plugins db register: {e}"))?;
        }
    }
    let blob_plugins: Vec<&LoadedPlugin> = loaded
        .iter()
        .filter(|p| p.registrations.blob.is_some())
        .collect();
    if blob_plugins.len() > 1 {
        return Err("plugins conflict: multiple plugins register blob backend".to_string());
    }
    let blob = blob_plugins.first().and_then(|p| p.registrations.blob);
    let mut bus = BusBackendRegistry::builtin();
    for p in loaded {
        if let Some(be) = bus_backend(p) {
            bus.register(be)
                .map_err(|e| format!("plugins bus register: {e}"))?;
        }
    }
    // kv 键选式单 vtable 槽（Task 4.4）：多 kv 插件冲突 fail fast；redis.default 声明
    // 但无 kv 插件 → 在 start() 装配 kv 时 fail fast（未声明走 InMemoryKV，不进插件）。
    let kv_plugins: Vec<&LoadedPlugin> = loaded
        .iter()
        .filter(|p| p.registrations.kv.is_some())
        .collect();
    if kv_plugins.len() > 1 {
        return Err("plugins conflict: multiple plugins register kv backend".to_string());
    }
    let kv = kv_plugins.first().and_then(|p| p.registrations.kv);
    // auth 键选式单 vtable 槽（auth 解耦）：cfg [auth] 声明但无 auth 插件 → fail fast
    // （§2 闸门，鉴权不许静默失守）；多 auth 插件注册冲突 → fail fast。
    let auth_plugins: Vec<&LoadedPlugin> = loaded
        .iter()
        .filter(|p| p.registrations.auth.is_some())
        .collect();
    if cfg.auth.is_some() && auth_plugins.is_empty() {
        return Err(
            "config declares [auth] but no auth plugin loaded (run `cargo xtask plugin auth`)"
                .to_string(),
        );
    }
    if auth_plugins.len() > 1 {
        return Err("plugins conflict: multiple plugins register auth guard".to_string());
    }
    let auth = auth_plugins.first().and_then(|p| p.registrations.auth);
    // mq 命名客户端（spec 2026-09-07）：收集所有提供 mq 轴的插件（名 + vtable）。
    // 多插件同 kind 冲突在 build_mq_registries 按段路由时报错（错误文案带 kind）。
    let mq: Vec<(String, &'static oj_plugin_ffi::MqVtable)> = loaded
        .iter()
        .filter_map(|p| {
            p.registrations
                .mq
                .map(|vt| (p.descriptor.name[..].to_string(), vt))
        })
        .collect();
    // mail 键选式单 vtable 槽（spec 2026-09-15）：多 mail 插件注册冲突 fail fast；
    // `smtp:` 段声明但插件未装 → 不在装配期硬失败（`app::build_mail_backend` 返回 None，
    // `mail.*` 调用报 "mail not configured" 并点名两种成因）。
    let mail_plugins: Vec<&LoadedPlugin> = loaded
        .iter()
        .filter(|p| p.registrations.mail.is_some())
        .collect();
    if mail_plugins.len() > 1 {
        return Err("plugins conflict: multiple plugins register mail backend".to_string());
    }
    let mail = mail_plugins.first().and_then(|p| p.registrations.mail);
    Ok(Registries {
        es,
        dbs,
        blob,
        bus,
        kv,
        auth,
        mq,
        mail,
    })
}

/// spec §5 全流程：解析 plugins_dir → 严格（plugins map 键）/缺省扫描 → 逐个加载校验
/// → 身份核对 → semver 对照 → 注册（插件先于内置）→ 内置后端注册。
/// 空 map/目录不存在/为空 → 扫描模式零插件（仅内置后端，不报错，除非 §2 闸门触发）。
pub async fn assemble_plugins(
    cfg: &Config,
    config_dir: &Path,
    registries: &mut Registries,
) -> Result<Vec<PluginInfo>, String> {
    // A5：mail 双配置源（`smtp:` 与 `plugins.mail` 皆非空）是配置错误 → 装配期 fail-fast
    // （`plugin_cfg` 会让非空 `plugins.mail` 静默胜出，运维改 `smtp:` 会「改了不生效」）。
    check_mail_cfg_sources(cfg)?;
    let dir = resolve_plugins_dir(config_dir, cfg.plugins_dir.as_deref())
        .map_err(|e| format!("plugins dir: {e}"))?;
    let host = host_context();
    let cfg_for = |name: &str| -> String { plugin_cfg(cfg, name) };
    let loaded = match dir {
        Some(dir) if !cfg.plugins.is_empty() => {
            // 严格模式（spec「plugins: 统一语义」）：map 键即清单——只装配列出的插件；
            // 键由 HashMap 保证唯一（旧 list 去重闸门随类型收编）。缺文件/校验失败 → fail fast。
            // 键序先按名字排序：HashMap 遍历序逐进程漂移，装配结果/GET {base}/plugins
            // 输出会随之不稳。
            let mut entries: Vec<PluginManifestEntry> = cfg
                .plugins
                .keys()
                .map(|name| PluginManifestEntry {
                    name: name.clone(),
                    semver_pin: None,
                })
                .collect();
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            load_manifest(&dir, &entries, host, &cfg_for)
                .map_err(|e| format!("plugins manifest: {e}"))?
        }
        Some(dir) => {
            // 缺省扫描：目录为空 → 零插件；扫描到损坏插件 → fail fast（不静默跳过）。
            // cfg_for 同样传入：cfg 键 = 文件 stem（auth/es 等按名注入真实 cfg，
            // 其余插件得 "{}"），与清单模式同一取值路径。
            load_scanned(&dir, host, &cfg_for).map_err(|e| format!("plugins scan: {e}"))?
        }
        None => Vec::new(),
    };
    *registries = build_registries(cfg, &loaded)?;
    Ok(loaded.iter().map(PluginInfo::from).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use only_js::bridge::plugin_loader::kv_backend_connect;

    struct Tmp(PathBuf);
    fn tmpdir(tag: &str) -> Tmp {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        use std::sync::atomic::Ordering;
        let d = std::env::temp_dir().join(format!(
            "oj-sc-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        Tmp(d)
    }

    /// 隔离插件扫描到空 `<triple>` 目录（同 tests/start_cert_expired_test.rs）：避免
    /// workspace / CI 检出的 `bin/plugins` 含 ABI 不符的陈旧产物（如遗留 bus-kafka.dll），
    /// 在目标校验前抢先报「plugins scan」错，偏离本测目标。Windows 主机三元组会命中
    /// `bin/plugins/<triple>`，故测试须显式把扫描钉到空目录。
    fn isolate_plugins(dir: &Path) -> PathBuf {
        let base = dir.join("plugins-isolated");
        std::fs::create_dir_all(base.join(host_triple())).unwrap();
        base
    }

    /// 证书必配门禁下，测试须自带有效证书：在 `dir`（随测试存活的临时目录）生成
    /// 真实签名 JWS，返回配好两路径的 Config。有效期 [now-1h, now+1y] → 启动时 Valid。
    fn cert_cfg(dir: &Path) -> Config {
        let mut cfg = Config::default();
        let n = server::test_support::now_secs();
        server::test_support::write_cert_into(
            &mut cfg.server,
            dir,
            n.saturating_sub(3600),
            n + 365 * 86_400,
        );
        // 隔离插件扫描（见 isolate_plugins）。
        cfg.plugins_dir = Some(isolate_plugins(dir));
        cfg
    }

    /// 同 `cert_cfg`，但证书已过期（宽限期 0）→ 启动应被证书门禁拒绝。
    fn expired_cert_cfg(dir: &Path) -> Config {
        let mut cfg = Config::default();
        cfg.server.grace_days = Some(0);
        let n = server::test_support::now_secs();
        server::test_support::write_cert_into(&mut cfg.server, dir, n - 2000, n - 1000);
        // 隔离插件扫描（见 isolate_plugins）。
        cfg.plugins_dir = Some(isolate_plugins(dir));
        cfg
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// re-exec 剥旗标：--daemon 精确移除，其余参数（含值）原样保留。
    #[test]
    fn strip_daemon_flag_drops_only_daemon() {
        use std::ffi::OsString;
        let out = strip_daemon_flag(
            ["server", "-c", "c.yaml", "--daemon", "--api-path", "src"]
                .into_iter()
                .map(OsString::from),
        );
        assert_eq!(
            out,
            ["server", "-c", "c.yaml", "--api-path", "src"]
                .into_iter()
                .map(OsString::from)
                .collect::<Vec<_>>()
        );
        // 无 --daemon 时原样透传。
        let out = strip_daemon_flag(["server", "-c", "c.yaml"].into_iter().map(OsString::from));
        assert_eq!(out.len(), 3);
    }

    /// 回归钉：缺省服务目录自 config 同级起步逐级向上搜索（src 优先、dist 次之），
    /// 而非 CWD——否则在仓库根（自带核心 crate 的 src/）跑 `-c sample/config.yaml`
    /// 会把 src/bridge/ 当业务模块，报 "module 'bridge' missing manifest.yaml"。
    #[test]
    fn default_dir_searches_upward_from_config_dir() {
        // 同级命中：src 优先于 dist。
        let t = tmpdir("defdir");
        std::fs::create_dir_all(t.0.join("src")).unwrap();
        std::fs::create_dir_all(t.0.join("dist")).unwrap();
        std::fs::write(t.0.join("config.yaml"), "{}\n").unwrap();
        let cfg = t.0.join("config.yaml");
        let (_, _, dir, ts, _) = load_app_config(cfg.to_str().unwrap(), None, None).unwrap();
        assert_eq!(dir, t.0.join("src"));
        assert!(ts);

        // 同级只有 dist → dist。
        let t2 = tmpdir("defdir-dist");
        std::fs::create_dir_all(t2.0.join("dist")).unwrap();
        std::fs::write(t2.0.join("config.yaml"), "{}\n").unwrap();
        let cfg2 = t2.0.join("config.yaml");
        let (_, _, dir2, _, _) = load_app_config(cfg2.to_str().unwrap(), None, None).unwrap();
        assert_eq!(dir2, t2.0.join("dist"));

        // config 下钻一层（sub/config.yaml），src 在父级 → 向上搜索命中。
        let t3 = tmpdir("defdir-up");
        std::fs::create_dir_all(t3.0.join("src")).unwrap();
        std::fs::create_dir_all(t3.0.join("sub")).unwrap();
        std::fs::write(t3.0.join("sub/config.yaml"), "{}\n").unwrap();
        let cfg3 = t3.0.join("sub/config.yaml");
        let (_, _, dir3, _, _) = load_app_config(cfg3.to_str().unwrap(), None, None).unwrap();
        assert_eq!(dir3, t3.0.join("src"));
    }

    /// cfg 回落链（spec「plugins: 统一语义」）：非空对象透传 → 轴适配器 → {}；
    /// 空对象不抢占透传优先级（否则 auth: {} 会切断顶层段 cfg 流）。
    #[test]
    fn plugin_cfg_fallback_chain() {
        let auth_yaml = "jwt_secret: \"s\"\n";
        let mut cfg = Config {
            auth: Some(serde_yaml::from_str(auth_yaml).unwrap()),
            ..Default::default()
        };
        // 1) 非空对象：原样透传（宿主不改写、不增删字段；键序不判——serde_json 保序）
        cfg.plugins.insert(
            "auth".into(),
            serde_json::json!({ "jwt_secret": "override", "extra_field": 42 }),
        );
        let v: serde_json::Value = serde_json::from_str(&plugin_cfg(&cfg, "auth")).unwrap();
        assert_eq!(v["jwt_secret"], "override");
        assert_eq!(v["extra_field"], 42);
        assert_eq!(v.as_object().map(|o| o.len()), Some(2));
        // 2) 无条目 → 轴适配器（auth 分支既有行为不变）；未知插件 → {}
        let mut cfg2 = Config {
            auth: Some(serde_yaml::from_str(auth_yaml).unwrap()),
            ..Default::default()
        };
        let v: serde_json::Value = serde_json::from_str(&plugin_cfg(&cfg2, "auth")).unwrap();
        assert_eq!(v["jwt_secret"], "s");
        assert_eq!(plugin_cfg(&cfg2, "vendor-thing"), "{}");
        // 3) 空对象：跳过透传 → 仍轴适配器
        cfg2.plugins.insert("auth".into(), serde_json::json!({}));
        let v: serde_json::Value = serde_json::from_str(&plugin_cfg(&cfg2, "auth")).unwrap();
        assert_eq!(v["jwt_secret"], "s");
    }

    /// mail 轴 cfg 走**适配器臂**（顶层 `smtp:` 段序列化），**不**要求用户用
    /// `plugins:` 透传——`plugins:` 非空即切严格清单模式，用户会被迫列全所有插件。
    /// 过线 JSON 与宿主的校验面同源（`MailConfig::from_value` 吃同一份）。
    #[test]
    fn plugin_cfg_mail_serializes_top_level_smtp_section() {
        // 段缺省 → 空 cfg（插件 init 得到零 profile；宿主不挂后端）。
        let mut cfg = Config::default();
        assert_eq!(plugin_cfg(&cfg, "mail"), "{}");
        // 顶层 smtp: 段 → 插件 cfg（并发参数 + 每 profile 一个键 + 凭据 + 白名单）。
        cfg.smtp = Some(
            serde_yaml::from_str(
                "workers: 2\nqueue_capacity: 8\nmock:\n  host: localhost\n  port: 25\n  \
                 tls: none\n  allow_none_tls: true\n  mechanism: login\n  \
                 file_transport: /tmp/eml\n  allowed_from: [noreply@x.com]\n  \
                 allowed_recipients: [\"@x.com\"]\n",
            )
            .unwrap(),
        );
        let raw = plugin_cfg(&cfg, "mail");
        assert_ne!(raw, "{}");
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["workers"], 2);
        assert_eq!(v["queue_capacity"], 8);
        assert_eq!(v["mock"]["file_transport"], "/tmp/eml");
        assert_eq!(v["mock"]["tls"], "none");
        assert_eq!(v["mock"]["allow_none_tls"], true);
        assert_eq!(v["mock"]["allowed_from"][0], "noreply@x.com");
        assert_eq!(v["mock"]["allowed_recipients"][0], "@x.com");
        // 空字段省略（`null` 会让插件强类型反序列化报错）。
        assert!(v["mock"].get("user").is_none(), "{v}");
        // 非空对象透传优先（统一语义；宿主与插件仍吃同一份 → 不产生两边分叉）。
        cfg.plugins.insert(
            "mail".into(),
            serde_json::json!({ "mock": { "host": "override" } }),
        );
        let v: serde_json::Value = serde_json::from_str(&plugin_cfg(&cfg, "mail")).unwrap();
        assert_eq!(v["mock"]["host"], "override");
        assert!(v.get("workers").is_none(), "{v}");
    }

    /// 适配器轴必须是宿主探测轴的子集（两表失步 = 适配器永远打空）。
    #[test]
    fn cfg_adapters_subset_of_probed_axes() {
        for name in ADAPTER_AXES {
            assert!(
                only_js::bridge::plugin_loader::AXES.contains(name),
                "{name}"
            );
        }
    }

    /// A5：mail 双配置源闸门 —— `smtp:` 与非空 `plugins.mail` **皆非空** ⇒ 装配期 Err
    /// （`plugin_cfg` 会让透传静默胜出，运维改 `smtp:` 会「改了不生效」）。
    /// 其余四种组合都合法（含 `plugins: {mail: {}}` 空对象 = 回落适配器；`smtp: {}` 空段 = 未配置）。
    #[test]
    fn given_both_mail_cfg_sources_nonempty_when_check_then_errors() {
        use only_js::config::SmtpSection;

        /// 一个 profile（实质内容）的 `smtp:` 段。
        fn profiles() -> SmtpSection {
            serde_yaml::from_str("default:\n  host: h\n  allowed_from: [a@x.com]\n").unwrap()
        }
        /// 非空 `plugins.mail`（透传形态）。
        fn passthrough() -> serde_json::Value {
            serde_json::json!({ "default": { "host": "override" } })
        }
        /// 组装一个 Config；`None` = 该来源缺省。
        fn case(plugins_mail: Option<serde_json::Value>, smtp: Option<SmtpSection>) -> Config {
            let mut cfg = Config {
                smtp,
                ..Default::default()
            };
            if let Some(v) = plugins_mail {
                cfg.plugins.insert("mail".into(), v);
            }
            cfg
        }

        // 皆非空 → Err，文案须点名两条来源与「二选一」。
        let e = check_mail_cfg_sources(&case(Some(passthrough()), Some(profiles())))
            .expect_err("双源皆非空必须 Err");
        assert!(
            e.contains("smtp:") && e.contains("plugins.mail") && e.contains("pick one"),
            "文案须点明两条来源与二选一：{e}"
        );
        // 仅 `workers`（无 profile）也算「非空段」——它就是被静默忽略的那部分配置。
        let workers_only: SmtpSection = serde_yaml::from_str("workers: 4\n").unwrap();
        assert!(check_mail_cfg_sources(&case(Some(passthrough()), Some(workers_only))).is_err());

        // 合法的四种组合：
        // 1) 只有 `smtp:`（标准形态）。
        assert!(check_mail_cfg_sources(&case(None, Some(profiles()))).is_ok());
        // 2) 只有 `plugins.mail`（透传）。
        assert!(check_mail_cfg_sources(&case(Some(passthrough()), None)).is_ok());
        // 3) `smtp:` 非空 + `plugins: {mail: {}}`（空对象 = 回落适配器，非透传；e2e 夹具即此形态）。
        assert!(
            check_mail_cfg_sources(&case(Some(serde_json::json!({})), Some(profiles()))).is_ok()
        );
        // 4) `plugins.mail` 非空 + `smtp: {}`（空段 = 未配置，不算冲突）。
        assert!(
            check_mail_cfg_sources(&case(Some(passthrough()), Some(SmtpSection::default())))
                .is_ok()
        );
    }

    #[test]
    fn config_dir_never_empty() {
        // bare 文件名回落当前目录（project_root 钳制不因空路径失效）。
        assert_eq!(config_dir_of(Path::new("config.yaml")), PathBuf::from("."));
        assert_eq!(
            config_dir_of(Path::new("./config.yaml")),
            PathBuf::from(".")
        );
        assert_eq!(
            config_dir_of(Path::new("sub/config.yaml")),
            PathBuf::from("sub")
        );
        assert_eq!(
            config_dir_of(Path::new("/abs/config.yaml")),
            PathBuf::from("/abs")
        );
    }

    #[test]
    fn cli_app_path_resolves_against_cwd() {
        // CLI --app-path：相对路径按 CWD（base）拼接；绝对路径原样透传。
        // 以 Path 语义断言——Windows 下 join 产出 `\` 分隔符，字符串比较会误报。
        let base = Path::new("/repo/root");
        assert_eq!(
            PathBuf::from(absolutize_cwd(base, "dist")),
            base.join("dist")
        );
        // join 不做归一化，`..` 原样保留（由 resolve_static_root 的 canonicalize 收敛）。
        assert_eq!(
            PathBuf::from(absolutize_cwd(base, "../site")),
            base.join("../site")
        );
        assert_eq!(
            PathBuf::from(absolutize_cwd(base, "/abs/dist")),
            PathBuf::from("/abs/dist")
        );
    }

    #[test]
    fn admission_gate_three_states() {
        // 三态准入：皆指定 → 两者都必须存在；指定其一 → 该目录必须存在；
        // 皆未指定 → Err 提醒必须指定其一。静态站点存在性移交装配期（fail-fast）。
        let t = tmpdir("admit");
        std::fs::create_dir(t.0.join("src")).unwrap();
        // 皆指定且都存在 → Ok。
        assert!(admission_gate(Some(Path::new("src")), true).is_ok());
        // 皆指定但 api 缺失 → Err。
        assert!(admission_gate(Some(Path::new("no-api")), true).is_err());
        // 仅 api：存在 → Ok，缺失 → Err。
        assert!(admission_gate(Some(Path::new("src")), false).is_ok());
        assert!(admission_gate(Some(Path::new("no-api")), false).is_err());
        // 仅静态站点声明：装配期再 fail-fast 目录存在性（gate 只判「指定与否」）。
        assert!(admission_gate(None, true).is_ok());
        // 皆未指定 → Err。
        let e = admission_gate(None, false).unwrap_err();
        assert!(e.contains("--api-path"), "{e}");
    }

    #[test]
    fn fold_cli_app_paths_rules() {
        // 裸 dir：覆盖主站点 app_path；至多一次。
        let mut c = Config::default();
        fold_cli_app_paths(&mut c, &["web".into()]).unwrap();
        assert!(c.server.app_path.as_deref().unwrap().ends_with("web"));
        assert!(c.server.static_sites.is_empty());
        assert!(fold_cli_app_paths(&mut c, &["a".into(), "b".into()]).is_err());
        // prefix=dir：追加 + 同前缀替换（规范化口径：config 带尾斜杠也命中）。
        let mut c = Config::default();
        c.server.static_sites.push(StaticSiteConf {
            prefix: "/docs/".into(),
            path: "old".into(),
        });
        fold_cli_app_paths(&mut c, &["/docs=new".into(), "/app=dist/app".into()]).unwrap();
        assert_eq!(c.server.static_sites.len(), 2);
        let docs = c
            .server
            .static_sites
            .iter()
            .find(|s| s.prefix == "/docs")
            .unwrap();
        assert!(docs.path.ends_with("new"), "{docs:?}");
        let app = c
            .server
            .static_sites
            .iter()
            .find(|s| s.prefix == "/app")
            .unwrap();
        assert!(app.path.ends_with("dist/app"), "{app:?}");
        // 裸 + 带前缀混合。
        let mut c = Config::default();
        fold_cli_app_paths(&mut c, &["web".into(), "/x=dx".into()]).unwrap();
        assert!(c.server.app_path.is_some());
        assert_eq!(c.server.static_sites.len(), 1);
        // 非法 prefix 报错。
        assert!(fold_cli_app_paths(&mut Config::default(), &["docs=dx".into()]).is_err());
    }

    #[test]
    fn base_precedence_and_empty_guard() {
        // CLI -b > config server.api_prefix（config 默认 /v1/api 由 ServerCfg::default 兜底）
        assert_eq!(resolve_base(None, "/xapi").unwrap(), "/xapi");
        assert_eq!(resolve_base(Some("/cli"), "/xapi").unwrap(), "/cli");
        assert_eq!(resolve_base(None, "/v1/api").unwrap(), "/v1/api");
        // 空前缀（仅斜杠）拒绝
        assert!(resolve_base(Some(""), "/xapi").is_err());
        assert!(resolve_base(None, "///").is_err());
    }

    #[test]
    fn app_prefix_normalized() {
        // 归一：尾斜杠剪除；剪完为空 → "/"；不以 / 开头 → Err。
        assert_eq!(resolve_app_prefix("/").unwrap(), "/");
        assert_eq!(resolve_app_prefix("/site").unwrap(), "/site");
        assert_eq!(resolve_app_prefix("/site/").unwrap(), "/site");
        assert_eq!(resolve_app_prefix("/a/b/").unwrap(), "/a/b");
        let e = resolve_app_prefix("site").unwrap_err();
        assert!(e.contains("must start with"), "{e}");
    }

    /// 证书必配：未配置证书路径 → 启动 fail-fast（任何方式都无法绕过）；配齐有效证书 →
    /// 正常启动；证书过期 → 启动拒绝。
    #[tokio::test]
    async fn certificate_mandatory_gate() {
        let t = tmpdir("sc-cert");
        std::fs::create_dir_all(t.0.join("src")).unwrap();
        // a) 未配置任何证书路径 → 拒绝启动。
        let e = start(
            Config::default(),
            &t.0,
            t.0.join("src"),
            "/v1/api".into(),
            true,
        )
        .await
        .err()
        .unwrap_or_default();
        assert!(
            e.contains("certificate is mandatory") && e.contains("public_key_path"),
            "{e}"
        );
        // b) 只配一个路径 → 仍拒绝（缺任一不算就绪）。
        let mut cfg = Config::default();
        cfg.server.public_key_path = "/nonexistent/key.pem".into();
        let e = start(cfg, &t.0, t.0.join("src"), "/v1/api".into(), true)
            .await
            .err()
            .unwrap_or_default();
        assert!(e.contains("certificate is mandatory"), "{e}");
        // c) 配齐有效证书 → 启动成功（随机端口）。
        let mut cfg = cert_cfg(&t.0);
        cfg.server.host = "127.0.0.1".into(); // 沙箱环境 localhost 解析受限，绑定回环更稳
        cfg.server.port = 0;
        let r = start(cfg, &t.0, t.0.join("src"), "/v1/api".into(), true)
            .await
            .unwrap();
        assert!(r.0.port() != 0);
    }

    /// 证书过期且宽限期 0 → 启动期硬断言中止（oj/src/app.rs::load_cert_with_watcher 的
    /// assert!），不再返回可被静默的 Err。panic 载荷含 "certificate"，故用 should_panic。
    /// 原 certificate_mandatory_gate 的 (d) 步骤拆出独立：该用例 a/b/c 须正常返回，不能与
    /// 本测的 panic 混在同一测试函数里。
    #[tokio::test]
    #[should_panic(expected = "certificate")]
    async fn expired_cert_aborts_startup() {
        let t = tmpdir("sc-cert-exp");
        std::fs::create_dir_all(t.0.join("src")).unwrap();
        // 触发 load_cert_with_watcher 的 assert!（过期且宽限结束）→ panic 经 .await 上抛，
        // 由 #[should_panic] 捕获。返回值不会到达，故忽略。
        let _ = start(
            expired_cert_cfg(&t.0),
            &t.0,
            t.0.join("src"),
            "/v1/api".into(),
            true,
        )
        .await;
    }

    /// auth 配了但 jwt_secret 空 → 装配 fail-fast（不静默跳过鉴权）。
    /// 插件目录隔离到临时夹具（就地编译 oj-auth）——不得依赖 workspace 的 bin/ 产物，
    /// 否则 CI 检出（无 bin/plugins）会先撞「no auth plugin loaded」而非本测的目标错误。
    #[tokio::test]
    async fn empty_jwt_secret_fails_fast() {
        let t = tmpdir("sc-jwt");
        let pdir = t.0.join(host_triple());
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::copy(auth_plugin_artifact(), pdir.join(plugin_file("auth"))).unwrap();
        let mut cfg = cert_cfg(&t.0);
        cfg.plugins_dir = Some(t.0.clone());
        cfg.db.insert("default".into(), "sqlite::memory:".into());
        cfg.auth = Some(serde_yaml::from_str("jwt_secret: \"\"\n").unwrap());
        let e = start(
            cfg,
            Path::new("/tmp"),
            t.0.join("src"),
            "/v1/api".into(),
            true,
        )
        .await
        .err()
        .unwrap_or_default();
        assert!(e.contains("jwt_secret"), "{e}");
    }

    #[test]
    fn mode_detected_by_lock_file() {
        let t = tmpdir("sc-mode");
        // 空目录 / 模块树（只有 manifest.yaml）→ dev
        assert!(!is_release(&t.0));
        std::fs::create_dir_all(t.0.join("user")).unwrap();
        std::fs::write(t.0.join("user/manifest.yaml"), "name: user\n").unwrap();
        assert!(!is_release(&t.0));
        // 构建锁存在 → release
        std::fs::write(t.0.join("manifests.yaml"), "user: 0.1.0\n").unwrap();
        assert!(is_release(&t.0));
    }

    #[tokio::test]
    async fn rejects_unknown_dsn_scheme() {
        let t = tmpdir("sc-dsn2");
        let mut cfg = cert_cfg(&t.0);
        cfg.db
            .insert("default".into(), "oracle://u:p@localhost/test".into());
        let e = start(
            cfg,
            Path::new("/tmp"),
            t.0.join("src"),
            "/v1/api".into(),
            true,
        )
        .await
        .err()
        .unwrap_or_default();
        assert!(e.contains("scheme"), "{e}");
    }

    #[tokio::test]
    async fn registry_connect_dispatches_by_scheme() {
        let t = tmpdir("sc-dsn");
        let reg = only_js::bridge::DbBackendRegistry::builtin();
        // sqlite：相对路径归一为 config_dir 下绝对路径并建空库
        reg.connect("sqlite://db.sqlite", &t.0)
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(t.0.join("db.sqlite").is_file());
        reg.connect("sqlite::memory:", &t.0)
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        reg.connect("memory://m", &t.0)
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        // Task 4.1：mysql/postgres 已迁插件，内置不再认领 → 缺装时明确 unknown db scheme
        // （快速失败不触网——原测试真连 127.0.0.1:1 每个 ~30s 超时，本版消除）。
        for unclaimed in ["mysql://u:p@127.0.0.1:1/app", "postgres://127.0.0.1:1/app"] {
            let e = reg
                .connect(unclaimed, &t.0)
                .await
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default();
            assert!(e.contains("unknown db scheme"), "{e}");
        }
        // 未知 scheme 拒绝
        assert!(reg.connect("oracle://x", &t.0).await.is_err());
    }

    /// 下载路由数据源 = registry.default()：同 key 不同内容时字节与 default 一致（spec §2 裁决回归）。
    #[tokio::test]
    async fn download_route_source_is_default_backend_only() {
        let t = tmpdir("sc-blob-def");
        let section: config::BlobSection =
            serde_yaml::from_str("backends:\n  default:\n    driver: local\n    root: a\n  img:\n    driver: local\n    root: b\n")
                .unwrap();
        let r = assemble_blobs(&section, &t.0, "/v1/api", None)
            .await
            .unwrap();
        r.default().unwrap().put("k", b"DEF", None).await.unwrap();
        r.get("img").unwrap().put("k", b"IMG", None).await.unwrap();
        match r.default().unwrap().serve("k").await.unwrap() {
            only_js::bridge::BlobServed::Bytes(bytes, _) => assert_eq!(bytes, b"DEF"),
            _ => panic!("local must inline-serve"),
        }
    }

    #[tokio::test]
    async fn assemble_blobs_multi_local_and_unknown_driver() {
        let t = tmpdir("sc-blob");
        let section: config::BlobSection =
            serde_yaml::from_str("backends:\n  default:\n    driver: local\n    root: a\n  img:\n    driver: local\n    root: b\n")
                .unwrap();
        let r = assemble_blobs(&section, &t.0, "/v1/api", None)
            .await
            .unwrap();
        assert!(r.default().is_some() && r.get("img").is_some());
        // local root 相对 config_dir 绝对化并创建
        assert!(t.0.join("a").is_dir() && t.0.join("b").is_dir());
        // 未知 driver：错误带后端名
        let bad: config::BlobSection =
            serde_yaml::from_str("backends:\n  x:\n    driver: ghost\n").unwrap();
        let e = assemble_blobs(&bad, &t.0, "/v1/api", None)
            .await
            .err()
            .unwrap_or_default();
        assert!(e.contains("'x'") && e.contains("ghost"), "{e}");
    }

    /// s3 驱动但无 blob 插件 → fail fast（driver != local 且无插件闸门，Task 4.2）。
    #[tokio::test]
    async fn s3_without_blob_plugin_fails_fast() {
        let t = tmpdir("sc-blob-s3");
        let section: config::BlobSection = serde_yaml::from_str(
            "backends:\n  default:\n    driver: s3\n    bucket: b\n    region: r\n",
        )
        .unwrap();
        let e = assemble_blobs(&section, &t.0, "/v1/api", None)
            .await
            .err()
            .unwrap_or_default();
        assert!(e.contains("requires the oj-blob-s3 plugin"), "{e}");
    }

    #[tokio::test]
    async fn connect_dbs_two_engines_and_unknown_named() {
        let t = tmpdir("sc-cdb");
        let reg = only_js::bridge::DbBackendRegistry::builtin();
        let mut m = HashMap::new();
        m.insert("default".to_string(), "sqlite::memory:".to_string());
        m.insert("aux".to_string(), "memory://aux".to_string());
        let dbs = connect_dbs(&m, &reg, &t.0).await.unwrap();
        assert!(dbs.contains_key("default") && dbs.contains_key("aux"));
        // 未知 scheme：库名出现在错误里
        let mut bad = HashMap::new();
        bad.insert("mydb".to_string(), "oracle://x".to_string());
        let e = connect_dbs(&bad, &reg, &t.0)
            .await
            .err()
            .unwrap_or_default();
        assert!(e.contains("mydb"), "{e}");
        assert!(e.contains("unknown db scheme"), "{e}");
    }

    #[tokio::test]
    async fn manifest_mismatch_blocks_startup() {
        let t = tmpdir("sc-md");
        std::fs::create_dir_all(t.0.join("src/user")).unwrap();
        std::fs::write(
            t.0.join("src/user/manifest.yaml"),
            "name: x\ndesc: d\nversion: 0.1.0\n",
        )
        .unwrap();
        let e = start(
            cert_cfg(&t.0),
            &t.0,
            t.0.join("src"),
            "/v1/api".into(),
            true,
        )
        .await
        .err()
        .unwrap_or_default();
        assert!(e.contains("name"), "{e}");
    }

    /// 夹具：手摆 release dist（file 名任意合法即可）。
    fn rel_fixture(files: &[(&str, &str)]) -> PathBuf {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        use std::sync::atomic::Ordering;
        let t = std::env::temp_dir().join(format!(
            "oj-sc-rel-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        for (rel, c) in files {
            let p = t.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, c).unwrap();
        }
        t
    }

    const MANI: &str = "name: user\ndesc: d\nversion: 0.1.0\n";

    #[tokio::test]
    async fn release_aggregates_modules_via_lock() {
        let t = rel_fixture(&[
            ("dist/manifests.yaml", "user: 0.1.0\n"),
            ("dist/user-0.1.0/manifest.yaml", MANI),
            (
                "dist/user-0.1.0/routes.js",
                "export default [ { method: \"get\", pattern: \"user/item/{id}\", file: \"item/api-x.js\" } ];\n",
            ),
            (
                "dist/user-0.1.0/item/api-x.js",
                "export default { get() { json.ok({ v: 1 }); } };\n",
            ),
        ]);
        let mut cfg = cert_cfg(&t);
        cfg.server.port = 0; // 随机端口（默认 778 并行测试会撞）
        let (addr, _h) = start(cfg, &t, t.join("dist"), "/v1/api".into(), false)
            .await
            .unwrap();
        let r = reqwest::get(format!("http://{addr}/v1/api/user/item/7"))
            .await
            .unwrap();
        assert_eq!(r.status(), 200); // pattern 无 base → 聚合拼 /v1/api/user/item/{id}
    }

    /// P1 迁移门禁（§4.6）：dev 默认 auto（启动即 apply）；release 默认 verify
    /// （账本落后 M004 拒启 + 指引命令；apply 后放行）；off 逃生门；非法值 fail-fast。
    #[tokio::test]
    async fn migrate_gate_auto_verify_off() {
        let t = tmpdir("sc-gate");
        std::fs::create_dir_all(t.0.join("src/u/migrations")).unwrap();
        std::fs::write(
            t.0.join("src/u/manifest.yaml"),
            "name: u\ndesc: d\nversion: 0.1.0\n",
        )
        .unwrap();
        std::fs::write(
            t.0.join("src/u/migrations/0001__init.sql"),
            "CREATE TABLE g (x);",
        )
        .unwrap();
        let db = format!("sqlite://{}/db.sqlite", t.0.display());
        // a) dev auto：启动即迁移（表已建）。
        let mut cfg = cert_cfg(&t.0);
        cfg.server.port = 0;
        cfg.db.insert("default".into(), db.clone());
        let _ = start(cfg, &t.0, t.0.join("src"), "/v1/api".into(), true)
            .await
            .unwrap();
        let acc = only_js::bridge::DbBackendRegistry::builtin()
            .connect(&db, &t.0)
            .await
            .unwrap();
        let rows = acc
            .query("select name from sqlite_master where type='table' and name='g'")
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "dev auto gate must apply migrations");
        // b) release verify：空库 + 产物带迁移 → M004 拒启（文案含命令）。
        const UMANI: &str = "name: u\ndesc: d\nversion: 0.1.0\n";
        let dist = rel_fixture(&[
            ("dist/manifests.yaml", "u: 0.1.0\n"),
            ("dist/u-0.1.0/manifest.yaml", UMANI),
            ("dist/u-0.1.0/routes.js", "export default [];\n"),
            (
                "dist/u-0.1.0/migrations/0001__init.sql",
                "CREATE TABLE g (x);",
            ),
        ]);
        let db2 = format!("sqlite://{}/rel.sqlite", t.0.display());
        let dist = dist.join("dist"); // rel_fixture 返回父目录
        let mut cfg = cert_cfg(&t.0);
        cfg.server.port = 0;
        cfg.db.insert("default".into(), db2.clone());
        let e = start(cfg, &t.0, dist.clone(), "/v1/api".into(), false)
            .await
            .err()
            .unwrap_or_default();
        assert!(e.contains("M004") && e.contains("oj migrate"), "{e}");
        // c) 显式 apply（oj migrate 等价）→ verify 放行。
        let acc = only_js::bridge::DbBackendRegistry::builtin()
            .connect(&db2, &t.0)
            .await
            .unwrap();
        oj_migrate_apply(&dist, &acc).await;
        let mut cfg = cert_cfg(&t.0);
        cfg.server.port = 0;
        cfg.db.insert("default".into(), db2);
        let _ = start(cfg, &t.0, dist, "/v1/api".into(), false)
            .await
            .unwrap();
        // d) off 逃生门：空库 + 迁移不执行也不拒启。
        let dist2 = rel_fixture(&[
            ("dist/manifests.yaml", "u: 0.1.0\n"),
            ("dist/u-0.1.0/manifest.yaml", UMANI),
            ("dist/u-0.1.0/routes.js", "export default [];\n"),
            (
                "dist/u-0.1.0/migrations/0001__init.sql",
                "CREATE TABLE g (x);",
            ),
        ])
        .join("dist");
        let mut cfg = cert_cfg(&t.0);
        cfg.server.port = 0;
        cfg.db.insert(
            "default".into(),
            format!("sqlite://{}/off.sqlite", t.0.display()),
        );
        cfg.server.migrate_on_start = Some("off".into());
        let _ = start(cfg, &t.0, dist2, "/v1/api".into(), false)
            .await
            .unwrap();
        // e) 非法值 fail-fast。
        let mut cfg = cert_cfg(&t.0);
        cfg.server.migrate_on_start = Some("nope".into());
        let e = start(cfg, &t.0, t.0.join("src"), "/v1/api".into(), true)
            .await
            .err()
            .unwrap_or_default();
        assert!(e.contains("illegal value"), "{e}");
    }

    /// 测试辅助：release 布局 apply_all（等价 `oj migrate -d dist`）。
    async fn oj_migrate_apply(dist: &Path, acc: &Arc<dyn DataAccessor>) {
        crate::migrate::apply_all(Some(acc), dist, false, false)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn release_fail_fast_paths() {
        // a) 无 manifests.yaml
        let t = rel_fixture(&[("dist/user-0.1.0/manifest.yaml", MANI)]);
        let e = start(cert_cfg(&t), &t, t.join("dist"), "/v1/api".into(), false)
            .await
            .err()
            .unwrap_or_default();
        assert!(
            e.contains("manifests.yaml") || e.contains("oj build"),
            "{e}"
        );
        // b) 锁指向不存在版本
        let t = rel_fixture(&[
            ("dist/manifests.yaml", "user: 9.9.9\n"),
            ("dist/user-0.1.0/manifest.yaml", MANI),
        ]);
        let e = start(cert_cfg(&t), &t, t.join("dist"), "/v1/api".into(), false)
            .await
            .err()
            .unwrap_or_default();
        assert!(e.contains("9.9.9"), "{e}");
        // c) version 注入
        let t = rel_fixture(&[("dist/manifests.yaml", "user: ../../etc\n")]);
        let e = start(cert_cfg(&t), &t, t.join("dist"), "/v1/api".into(), false)
            .await
            .err()
            .unwrap_or_default();
        assert!(e.contains("version") || e.contains("illegal"), "{e}");
        // d) manifest name 不符
        let t = rel_fixture(&[
            ("dist/manifests.yaml", "user: 0.1.0\n"),
            (
                "dist/user-0.1.0/manifest.yaml",
                "name: other\ndesc: d\nversion: 0.1.0\n",
            ),
        ]);
        let e = start(cert_cfg(&t), &t, t.join("dist"), "/v1/api".into(), false)
            .await
            .err()
            .unwrap_or_default();
        assert!(e.contains("name"), "{e}");
    }

    #[tokio::test]
    async fn release_routes_js_syntax_error_fails_fast() {
        // routes.js 语法错 → reader 解析失败 → 启动 Err（spec §4）
        let t = rel_fixture(&[
            ("dist/manifests.yaml", "user: 0.1.0\n"),
            ("dist/user-0.1.0/manifest.yaml", MANI),
            ("dist/user-0.1.0/routes.js", "export default [ "),
        ]);
        let e = start(cert_cfg(&t), &t, t.join("dist"), "/v1/api".into(), false)
            .await
            .err()
            .unwrap_or_default();
        assert!(e.contains("routes.js"), "{e}");
    }

    #[tokio::test]
    async fn release_conflicting_patterns_fail_fast() {
        // 两模块同 pattern 同 method → from_entries failures → 启动 Err（spec §4）
        let t = rel_fixture(&[
            ("dist/manifests.yaml", "user: 0.1.0\nother: 0.9.0\n"),
            ("dist/user-0.1.0/manifest.yaml", MANI),
            (
                "dist/user-0.1.0/routes.js",
                "export default [ { method: \"get\", pattern: \"user/item/{id}\", file: \"item/api-x.js\" } ];\n",
            ),
            (
                "dist/user-0.1.0/item/api-x.js",
                "export default { get() { json.ok({}); } };\n",
            ),
            (
                "dist/other-0.9.0/manifest.yaml",
                "name: other\ndesc: d\nversion: 0.9.0\n",
            ),
            (
                "dist/other-0.9.0/routes.js",
                "export default [ { method: \"get\", pattern: \"user/item/{id}\", file: \"item/api-y.js\" } ];\n",
            ),
            (
                "dist/other-0.9.0/item/api-y.js",
                "export default { get() { json.ok({}); } };\n",
            ),
        ]);
        let e = start(cert_cfg(&t), &t, t.join("dist"), "/v1/api".into(), false)
            .await
            .err()
            .unwrap_or_default();
        assert!(e.contains("conflict"), "{e}");
    }

    #[tokio::test]
    async fn release_keeps_real_error_from_corrupt_lock() {
        // M-2：锁解析错保留真实错误（不与缺失混为一句 not found or invalid）
        let t = rel_fixture(&[
            ("dist/manifests.yaml", "user: [unclosed\n"),
            ("dist/user-0.1.0/manifest.yaml", MANI),
        ]);
        let e = start(cert_cfg(&t), &t, t.join("dist"), "/v1/api".into(), false)
            .await
            .err()
            .unwrap_or_default();
        assert!(e.contains("unclosed") || e.contains("parse"), "{e}");
    }

    #[tokio::test]
    async fn server_app_path_serves_static_relative_to_config_dir() {
        let t = tmpdir("sc-root");
        std::fs::write(t.0.join("index.html"), "<h1>site</h1>").unwrap();
        let mut cfg = cert_cfg(&t.0);
        cfg.server.port = 0; // 随机端口
        cfg.server.app_path = Some(".".into()); // 相对 config_dir
        let (addr, _h) = start(cfg, &t.0, t.0.join("src"), "/v1/api".into(), true)
            .await
            .unwrap();
        let r = reqwest::get(format!("http://{addr}/")).await.unwrap();
        assert_eq!(r.status(), 200);
        assert!(r.text().await.unwrap().contains("site"));
    }

    #[tokio::test]
    async fn server_app_path_missing_dir_fails_fast() {
        // dir 指向不存在的子目录（相对 tmpdir），使模块扫描为空、顺利走到 server.app_path
        // 校验；此前用绝对 "src" 会 canonicalize 到 oj/src（含无 manifest 的 test_ext），
        // 在抵达 server.app_path 检查前就于模块扫描阶段报错，掩盖了本测试真正要验证的逻辑。
        let t = tmpdir("sc-root-missing");
        let mut cfg = cert_cfg(&t.0);
        cfg.server.app_path = Some("no-such-dir".into());
        let e = start(cfg, &t.0, t.0.join("src"), "/v1/api".into(), true)
            .await
            .err()
            .unwrap_or_default();
        assert!(e.contains("server.app_path"), "{e}");
    }

    /// P0：模块种子重放——src/u/seed.sql 建表插数，handler 查得到。
    #[tokio::test]
    async fn module_seeds_replayed_and_served() {
        let t = tmpdir("sc-mseed");
        std::fs::create_dir_all(t.0.join("src/u")).unwrap();
        std::fs::write(
            t.0.join("src/u/manifest.yaml"),
            "name: u\ndesc: d\nversion: 0.1.0\n",
        )
        .unwrap();
        std::fs::write(
            t.0.join("src/u/seed.sql"),
            "CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY, v TEXT);\n\
             INSERT OR IGNORE INTO t (id, v) VALUES (1, 'mod');\n",
        )
        .unwrap();
        let mut cfg = cert_cfg(&t.0);
        cfg.server.port = 0;
        cfg.db.insert(
            "default".into(),
            format!("sqlite://{}/db.sqlite", t.0.display()),
        );
        let (addr, _h) = start(cfg, &t.0, t.0.join("src"), "/v1/api".into(), true)
            .await
            .unwrap();
        std::fs::create_dir_all(t.0.join("src/u/f")).unwrap();
        std::fs::write(
            t.0.join("src/u/f/api.ts"),
            "export default { get() { db.query(\"select v from t where id = ?\", [1]).then(r => json.ok(r)); } };\n",
        )
        .unwrap();
        let resp = reqwest::get(format!("http://{addr}/v1/api/u/f/"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let v: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(v["data"][0]["v"], "mod", "{v}");
    }

    // ----- 插件装配（spec §5）：全部经 cfg.plugins_dir 注入（每测试独立 Config，
    // 无全局 env 竞争；OAT_PLUGINS_DIR 只留给真实部署路径）。 -----

    use std::sync::OnceLock;

    fn host_triple() -> String {
        let out = std::process::Command::new("rustc")
            .arg("-vV")
            .output()
            .unwrap();
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .find_map(|l| l.strip_prefix("host: "))
            .unwrap()
            .to_string()
    }

    /// 插件存放文件名（= loader plugin_file_name）。
    fn plugin_file(name: &str) -> String {
        if cfg!(target_os = "windows") {
            format!("{name}.dll")
        } else if cfg!(target_os = "macos") {
            format!("lib{name}.dylib")
        } else {
            format!("lib{name}.so")
        }
    }

    /// 编译 oj-es 产物路径（全进程一次，oj-es 已有 debug 构建，命中缓存）。
    fn es_plugin_artifact() -> PathBuf {
        static ONCE: OnceLock<PathBuf> = OnceLock::new();
        ONCE.get_or_init(|| {
            let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
            let status = std::process::Command::new("cargo")
                .args(["build", "-p", "oj-es"])
                .current_dir(&root)
                .status()
                .expect("invoke cargo build for oj-es");
            assert!(status.success(), "oj-es build failed");
            let (prefix, ext) = if cfg!(target_os = "windows") {
                ("", "dll")
            } else if cfg!(target_os = "macos") {
                ("lib", "dylib")
            } else {
                ("lib", "so")
            };
            root.join("target/debug")
                .join(format!("{prefix}oj_es.{ext}"))
        })
        .clone()
    }

    fn es_cfg(endpoint: &str) -> Config {
        Config {
            es: Some(config::EsCfg {
                endpoint: endpoint.to_string(),
            }),
            ..Default::default()
        }
    }

    /// plugins 键声明但文件缺失 → fail fast。
    #[tokio::test(flavor = "current_thread")]
    async fn manifest_missing_file_fails_fast() {
        let t = tmpdir("sc-man");
        std::fs::create_dir_all(t.0.join(host_triple())).unwrap();
        let mut cfg = Config {
            plugins_dir: Some(t.0.clone()),
            ..Default::default()
        };
        cfg.plugins.insert("ghost".into(), serde_json::json!({}));
        let mut r = Registries::default();
        let e = assemble_plugins(&cfg, &t.0, &mut r)
            .await
            .err()
            .unwrap_or_default();
        assert!(e.contains("plugin file missing"), "{e}");
    }

    /// 缺省扫描空目录 → 零插件、仅内置后端（es 未配置，不触发 §2 闸门）。
    #[tokio::test(flavor = "current_thread")]
    async fn scan_empty_dir_yields_only_builtin() {
        let t = tmpdir("sc-empty");
        std::fs::create_dir_all(t.0.join(host_triple())).unwrap();
        let cfg = Config {
            plugins_dir: Some(t.0.clone()),
            ..Default::default()
        };
        let mut r = Registries::default();
        let plugins = assemble_plugins(&cfg, &t.0, &mut r).await.unwrap();
        assert!(plugins.is_empty());
        assert!(r.es.is_none());
    }

    /// 扫描到损坏插件 → fail fast（不静默跳过）。
    #[tokio::test(flavor = "current_thread")]
    async fn scan_bad_plugin_fails_fast() {
        let t = tmpdir("sc-bad");
        let pdir = t.0.join(host_triple());
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::write(pdir.join(plugin_file("broken")), b"not a real dylib").unwrap();
        let cfg = Config {
            plugins_dir: Some(t.0.clone()),
            ..Default::default()
        };
        let mut r = Registries::default();
        let e = assemble_plugins(&cfg, &t.0, &mut r)
            .await
            .err()
            .unwrap_or_default();
        assert!(e.contains("plugins scan"), "{e}");
    }

    /// 配置声明 [es] 但插件未装 → 启动期报错（§2 闸门）。
    #[tokio::test(flavor = "current_thread")]
    async fn es_declared_without_plugin_fails_startup() {
        let t = tmpdir("sc-esgate");
        std::fs::create_dir_all(t.0.join(host_triple())).unwrap();
        let mut cfg = es_cfg("http://127.0.0.1:1");
        cfg.plugins_dir = Some(t.0.clone());
        let mut r = Registries::default();
        let e = assemble_plugins(&cfg, &t.0, &mut r)
            .await
            .err()
            .unwrap_or_default();
        assert!(e.contains("no es plugin loaded"), "{e}");
    }

    /// 全链路：真实 oj-es 插件装配 → es 后端经 FfiEsBackend 接线（handle 0）+ 自省信息。
    #[tokio::test(flavor = "current_thread")]
    async fn es_plugin_wires_backend() {
        let t = tmpdir("sc-esplug");
        let pdir = t.0.join(host_triple());
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::copy(es_plugin_artifact(), pdir.join(plugin_file("es"))).unwrap();
        let mut cfg = es_cfg("http://127.0.0.1:1");
        cfg.plugins_dir = Some(t.0.clone());
        let mut r = Registries::default();
        let plugins = assemble_plugins(&cfg, &t.0, &mut r).await.unwrap();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name, "es");
        assert!(r.es.is_some(), "es backend must be wired from the plugin");
    }

    /// plugins: 一段三用（spec「plugins: 统一语义」）：map 键即严格清单——键 ∈ 装配
    /// 结果（空对象值只影响 cfg 透传、不影响加载）；map 外插件不装配；空 map = 扫描模式。
    #[tokio::test(flavor = "current_thread")]
    async fn plugins_map_keys_drive_strict_load() {
        let t = tmpdir("sc-mapkeys");
        let pdir = t.0.join(host_triple());
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::copy(es_plugin_artifact(), pdir.join(plugin_file("es"))).unwrap();
        // 1) 键 = 清单：{"es": {}} → 只装配 es，es 后端照常接线。
        let mut cfg = es_cfg("http://127.0.0.1:1");
        cfg.plugins_dir = Some(t.0.clone());
        cfg.plugins.insert("es".into(), serde_json::json!({}));
        let mut r = Registries::default();
        let plugins = assemble_plugins(&cfg, &t.0, &mut r).await.unwrap();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name, "es");
        assert!(r.es.is_some(), "es backend must be wired from the plugin");
        // 2) map 外插件不装配：es 在盘上但键只有 ghost → 严格模式缺 ghost fail fast
        //    （扫描模式此时会装配 es， fail 即证明走的是清单路径）。
        let mut cfg = Config {
            plugins_dir: Some(t.0.clone()),
            ..Default::default()
        };
        cfg.plugins.insert("ghost".into(), serde_json::json!({}));
        let mut r = Registries::default();
        let e = assemble_plugins(&cfg, &t.0, &mut r)
            .await
            .err()
            .unwrap_or_default();
        assert!(e.contains("plugin file missing"), "{e}");
        // 3) 空 map = 扫描模式：同一目录照旧扫描装配。
        let cfg = Config {
            plugins_dir: Some(t.0.clone()),
            ..Default::default()
        };
        let mut r = Registries::default();
        let plugins = assemble_plugins(&cfg, &t.0, &mut r).await.unwrap();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name, "es");
    }

    /// 严格清单装配顺序确定：HashMap 键序逐进程漂移，entries 排序后按名字序装配，
    /// Vec<PluginInfo> 输出跨进程稳定（GET {base}/plugins 可比对）。插入序故意逆字母序。
    #[tokio::test(flavor = "current_thread")]
    async fn strict_manifest_loads_in_name_order() {
        let t = tmpdir("sc-manorder");
        let pdir = t.0.join(host_triple());
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::copy(es_plugin_artifact(), pdir.join(plugin_file("es"))).unwrap();
        std::fs::copy(auth_plugin_artifact(), pdir.join(plugin_file("auth"))).unwrap();
        let mut cfg = es_cfg("http://127.0.0.1:1");
        cfg.plugins_dir = Some(t.0.clone());
        cfg.plugins.insert("es".into(), serde_json::json!({}));
        cfg.plugins
            .insert("auth".into(), serde_json::json!({"jwt_secret": "x"}));
        let mut r = Registries::default();
        let plugins = assemble_plugins(&cfg, &t.0, &mut r).await.unwrap();
        let names: Vec<&str> = plugins.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["auth", "es"]);
    }

    /// 编译 oj-db-mysql 产物路径（全进程一次；sqlx 首次编译慢，OnceLock 缓存）。
    fn db_plugin_artifact() -> PathBuf {
        static ONCE: OnceLock<PathBuf> = OnceLock::new();
        ONCE.get_or_init(|| {
            let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
            let status = std::process::Command::new("cargo")
                .args(["build", "-p", "oj-db-mysql"])
                .current_dir(&root)
                .status()
                .expect("invoke cargo build for oj-db-mysql");
            assert!(status.success(), "oj-db-mysql build failed");
            let (prefix, ext) = if cfg!(target_os = "windows") {
                ("", "dll")
            } else if cfg!(target_os = "macos") {
                ("lib", "dylib")
            } else {
                ("lib", "so")
            };
            root.join("target/debug")
                .join(format!("{prefix}oj_db_mysql.{ext}"))
        })
        .clone()
    }

    /// 真实 oj-db-mysql 插件装配 → db 工厂经 FfiDbBackend 注册进 DbBackendRegistry
    /// （scheme 认领；连接转发由 ffi.rs 适配器测试覆盖，此处不断网连接）。
    #[tokio::test(flavor = "current_thread")]
    async fn db_plugin_wires_backend() {
        let t = tmpdir("sc-dbplug");
        let pdir = t.0.join(host_triple());
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::copy(db_plugin_artifact(), pdir.join(plugin_file("db-mysql"))).unwrap();
        let cfg = Config {
            plugins_dir: Some(t.0.clone()),
            plugins: HashMap::from([("db-mysql".into(), serde_json::json!({}))]),
            ..Default::default()
        };
        let mut r = Registries::default();
        let plugins = assemble_plugins(&cfg, &t.0, &mut r).await.unwrap();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name, "db-mysql");
        let names = r.dbs.backend_names();
        assert!(
            names.contains(&"db-mysql"),
            "factory not registered: {names:?}"
        );
        // 未认领 scheme 仍 unknown（插件没声明 oracle）→ 快速失败
        let e = r
            .dbs
            .connect("oracle://x", &t.0)
            .await
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(e.contains("unknown db scheme"), "{e}");
    }

    /// mysql DSN 但插件未装 → 明确 "unknown db scheme"（不触网，快速失败）。
    #[tokio::test(flavor = "current_thread")]
    async fn db_declared_without_plugin_unknown_scheme() {
        let t = tmpdir("sc-dbplug-none");
        std::fs::create_dir_all(t.0.join(host_triple())).unwrap(); // 空插件目录 → 零插件
        let cfg = Config {
            plugins_dir: Some(t.0.clone()),
            ..Default::default()
        };
        let mut r = Registries::default();
        assemble_plugins(&cfg, &t.0, &mut r).await.unwrap();
        let mut m = std::collections::HashMap::new();
        m.insert(
            "mydb".to_string(),
            "mysql://u:p@127.0.0.1:1/app".to_string(),
        );
        let e = connect_dbs(&m, &r.dbs, &t.0)
            .await
            .err()
            .unwrap_or_default();
        assert!(e.contains("mydb"), "{e}");
        assert!(e.contains("unknown db scheme"), "{e}");
    }

    /// 编译 oj-blob-s3 产物路径（全进程一次；object_store aws 首次编译慢，OnceLock 缓存）。
    fn blob_plugin_artifact() -> PathBuf {
        static ONCE: OnceLock<PathBuf> = OnceLock::new();
        ONCE.get_or_init(|| {
            let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
            let status = std::process::Command::new("cargo")
                .args(["build", "-p", "oj-blob-s3"])
                .current_dir(&root)
                .status()
                .expect("invoke cargo build for oj-blob-s3");
            assert!(status.success(), "oj-blob-s3 build failed");
            let (prefix, ext) = if cfg!(target_os = "windows") {
                ("", "dll")
            } else if cfg!(target_os = "macos") {
                ("lib", "dylib")
            } else {
                ("lib", "so")
            };
            root.join("target/debug")
                .join(format!("{prefix}oj_blob_s3.{ext}"))
        })
        .clone()
    }

    /// 真实 oj-blob-s3 插件装配 → Registries.blob 槽就位；经它 connect 建后端
    /// （cfg 校验离线路径：bucket/region 必填在插件侧 fail-fast，不触网）。
    #[tokio::test(flavor = "current_thread")]
    async fn blob_plugin_wires_vtable_and_connect_gate() {
        let t = tmpdir("sc-blobplug");
        let pdir = t.0.join(host_triple());
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::copy(blob_plugin_artifact(), pdir.join(plugin_file("blob-s3"))).unwrap();
        let cfg = Config {
            plugins_dir: Some(t.0.clone()),
            plugins: HashMap::from([("blob-s3".into(), serde_json::json!({}))]),
            ..Default::default()
        };
        let mut r = Registries::default();
        let plugins = assemble_plugins(&cfg, &t.0, &mut r).await.unwrap();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name, "blob-s3");
        assert!(r.blob.is_some(), "blob vtable slot not registered");
        // 经 vtable connect：配置缺 bucket → 插件侧 fail-fast（快速失败不触网）
        let cfg_json = serde_json::json!({ "driver": "s3", "region": "us-east-1" }).to_string();
        let e = blob_backend_connect(r.blob.unwrap(), "img", &cfg_json)
            .await
            .err()
            .unwrap_or_default();
        assert!(e.contains("bucket required"), "{e}");
    }

    /// 编译 oj-bus-kafka 产物路径（全进程一次；rdkafka 首次编译慢，OnceLock 缓存）。
    fn bus_kafka_plugin_artifact() -> PathBuf {
        static ONCE: OnceLock<PathBuf> = OnceLock::new();
        ONCE.get_or_init(|| {
            let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
            let status = std::process::Command::new("cargo")
                .args(["build", "-p", "oj-bus-kafka"])
                .current_dir(&root)
                .status()
                .expect("invoke cargo build for oj-bus-kafka");
            assert!(status.success(), "oj-bus-kafka build failed");
            let (prefix, ext) = if cfg!(target_os = "windows") {
                ("", "dll")
            } else if cfg!(target_os = "macos") {
                ("lib", "dylib")
            } else {
                ("lib", "so")
            };
            root.join("target/debug")
                .join(format!("{prefix}oj_bus_kafka.{ext}"))
        })
        .clone()
    }

    /// 编译 oj-bus-rabbitmq 产物路径（全进程一次；lapin 首次编译慢，OnceLock 缓存）。
    fn bus_rabbitmq_plugin_artifact() -> PathBuf {
        static ONCE: OnceLock<PathBuf> = OnceLock::new();
        ONCE.get_or_init(|| {
            let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
            let status = std::process::Command::new("cargo")
                .args(["build", "-p", "oj-bus-rabbitmq"])
                .current_dir(&root)
                .status()
                .expect("invoke cargo build for oj-bus-rabbitmq");
            assert!(status.success(), "oj-bus-rabbitmq build failed");
            let (prefix, ext) = if cfg!(target_os = "windows") {
                ("", "dll")
            } else if cfg!(target_os = "macos") {
                ("lib", "dylib")
            } else {
                ("lib", "so")
            };
            root.join("target/debug")
                .join(format!("{prefix}oj_bus_rabbitmq.{ext}"))
        })
        .clone()
    }

    /// 真实 oj-bus-kafka 插件装配 → Registries.bus 注册 kind "kafka"（kind 由插件名
    /// 去 "bus-" 前缀推断）。connect 需真 broker，此处只验证注册与 kind 键选。
    #[tokio::test(flavor = "current_thread")]
    async fn bus_plugin_wires_kind() {
        let t = tmpdir("sc-busplug");
        let pdir = t.0.join(host_triple());
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::copy(
            bus_kafka_plugin_artifact(),
            pdir.join(plugin_file("bus-kafka")),
        )
        .unwrap();
        let cfg = Config {
            plugins_dir: Some(t.0.clone()),
            plugins: HashMap::from([("bus-kafka".into(), serde_json::json!({}))]),
            ..Default::default()
        };
        let mut r = Registries::default();
        let plugins = assemble_plugins(&cfg, &t.0, &mut r).await.unwrap();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name, "bus-kafka");
        let kinds = r.bus.kinds();
        assert!(
            kinds.iter().any(|k| k == "kafka"),
            "kind not registered: {kinds:?}"
        );
        // 本地 kind 仍内置
        assert!(kinds.iter().any(|k| k == "local"), "{kinds:?}");
    }

    /// broker.kind=kafka 但插件未装 → "unknown broker kind"（列出已知 kind，快速失败）。
    #[tokio::test(flavor = "current_thread")]
    async fn kafka_declared_without_plugin_unknown_kind() {
        let t = tmpdir("sc-busplug-none");
        std::fs::create_dir_all(t.0.join(host_triple())).unwrap(); // 空插件目录 → 零插件
        let cfg = Config {
            plugins_dir: Some(t.0.clone()),
            ..Default::default()
        };
        let mut r = Registries::default();
        assemble_plugins(&cfg, &t.0, &mut r).await.unwrap();
        let broker_cfg = only_js::config::BrokerCfg {
            kind: "kafka".into(),
            ..Default::default()
        };
        let e = r
            .bus
            .connect(&Some(broker_cfg))
            .await
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(e.contains("unknown broker kind 'kafka'"), "{e}");
    }

    /// 编译 oj-kv-redis 产物路径（全进程一次）。
    fn kv_redis_plugin_artifact() -> PathBuf {
        static ONCE: OnceLock<PathBuf> = OnceLock::new();
        ONCE.get_or_init(|| {
            let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
            let status = std::process::Command::new("cargo")
                .args(["build", "-p", "oj-kv-redis"])
                .current_dir(&root)
                .status()
                .expect("invoke cargo build for oj-kv-redis");
            assert!(status.success(), "oj-kv-redis build failed");
            let (prefix, ext) = if cfg!(target_os = "windows") {
                ("", "dll")
            } else if cfg!(target_os = "macos") {
                ("lib", "dylib")
            } else {
                ("lib", "so")
            };
            root.join("target/debug")
                .join(format!("{prefix}oj_kv_redis.{ext}"))
        })
        .clone()
    }

    /// 真实 oj-kv-redis 插件装配 → Registries.kv 槽就位；经 vtable connect 建 KV
    /// （连接探活需真 redis——此处只验证槽位 + connect 到无监听端口 fail-fast，不触网）。
    #[tokio::test(flavor = "current_thread")]
    async fn kv_plugin_wires_vtable_and_connect_gate() {
        let t = tmpdir("sc-kvplug");
        let pdir = t.0.join(host_triple());
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::copy(
            kv_redis_plugin_artifact(),
            pdir.join(plugin_file("kv-redis")),
        )
        .unwrap();
        let cfg = Config {
            plugins_dir: Some(t.0.clone()),
            plugins: HashMap::from([("kv-redis".into(), serde_json::json!({}))]),
            ..Default::default()
        };
        let mut r = Registries::default();
        let plugins = assemble_plugins(&cfg, &t.0, &mut r).await.unwrap();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name, "kv-redis");
        assert!(r.kv.is_some(), "kv vtable slot not registered");
        // 经 vtable connect：无监听端口 → 插件侧探活 fail-fast（不挂启动）。
        let e = kv_backend_connect(r.kv.unwrap(), "redis://127.0.0.1:1/")
            .await
            .err()
            .unwrap_or_default();
        assert!(e.contains("redis connect"), "{e}");
    }

    /// redis.default 声明但无 kv 插件 → 启动 fail-fast（§2 闸门，不退化静默）。
    #[tokio::test(flavor = "current_thread")]
    async fn redis_declared_without_kv_plugin_fails_fast() {
        let t = tmpdir("sc-kvplug-none");
        std::fs::create_dir_all(t.0.join(host_triple())).unwrap(); // 空插件目录 → 零插件
        let mut cfg = cert_cfg(&t.0);
        cfg.plugins_dir = Some(t.0.clone());
        cfg.redis
            .insert("default".into(), "redis://127.0.0.1:1/".into());
        let mut r = Registries::default();
        assemble_plugins(&cfg, &t.0, &mut r).await.unwrap();
        assert!(r.kv.is_none());
        let e = start(cfg, &t.0, t.0.join("src"), "/v1/api".into(), true)
            .await
            .err()
            .unwrap_or_default();
        assert!(e.contains("no kv plugin loaded"), "{e}");
        assert!(e.contains("cargo xtask plugin kv-redis"), "{e}");
    }

    /// 编译 oj-auth 产物路径（全进程一次）。
    fn auth_plugin_artifact() -> PathBuf {
        static ONCE: OnceLock<PathBuf> = OnceLock::new();
        ONCE.get_or_init(|| {
            let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
            let status = std::process::Command::new("cargo")
                .args(["build", "-p", "oj-auth"])
                .current_dir(&root)
                .status()
                .expect("invoke cargo build for oj-auth");
            assert!(status.success(), "oj-auth build failed");
            let (prefix, ext) = if cfg!(target_os = "windows") {
                ("", "dll")
            } else if cfg!(target_os = "macos") {
                ("lib", "dylib")
            } else {
                ("lib", "so")
            };
            root.join("target/debug")
                .join(format!("{prefix}oj_auth.{ext}"))
        })
        .clone()
    }

    /// auth 声明但无 auth 插件 → fail-fast（§2 闸门，不静默放行）。
    #[tokio::test(flavor = "current_thread")]
    async fn auth_plugin_required_when_configured() {
        let t = tmpdir("sc-authgate");
        std::fs::create_dir_all(t.0.join(host_triple())).unwrap(); // 空插件目录 → 零插件
        let mut cfg = Config {
            plugins_dir: Some(t.0.clone()),
            ..Default::default()
        };
        cfg.auth = Some(serde_yaml::from_str("jwt_secret: \"x\"\n").unwrap());
        let mut r = Registries::default();
        let e = assemble_plugins(&cfg, &t.0, &mut r)
            .await
            .err()
            .unwrap_or_default();
        assert!(e.contains("no auth plugin loaded"), "{e}");
    }

    /// 全链路：真实 oj-auth 插件装配 → Registries.auth 槽就位；经 FfiAuthGuard
    /// 验签测试签发的 JWT（sign 用 jsonwebtoken）：匿名路径放行、有效 token 出 user、
    /// 坏 token / 缺 token 拒绝。
    #[tokio::test(flavor = "current_thread")]
    async fn auth_plugin_wires_guard_and_verifies_jwt() {
        let t = tmpdir("sc-authplug");
        let pdir = t.0.join(host_triple());
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::copy(auth_plugin_artifact(), pdir.join(plugin_file("auth"))).unwrap();
        let mut cfg = Config {
            plugins_dir: Some(t.0.clone()),
            ..Default::default()
        };
        cfg.auth = Some(
            serde_yaml::from_str("jwt_secret: \"oj-test-secret\"\nanonymous_paths:\n  - /health\n")
                .unwrap(),
        );
        let mut r = Registries::default();
        assemble_plugins(&cfg, &t.0, &mut r).await.unwrap();
        let vt = r
            .auth
            .expect("auth vtable slot must be wired from the plugin");
        let guard = only_js::bridge::plugin_loader::auth_guard_from_vtable(vt);
        // 匿名路径 → None（放行，无 token 也行）
        assert!(guard.verify("/health", None).unwrap().is_none());
        // 有效 token → user（sub → id）
        let now = server::test_support::now_secs();
        let claims = serde_json::json!({
            "sub": "u1", "roles": ["admin"], "iat": now, "exp": now + 3600
        });
        let token = jsonwebtoken::encode(
            &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(b"oj-test-secret"),
        )
        .unwrap();
        let user = guard
            .verify("/items", Some(&format!("Bearer {token}")))
            .unwrap()
            .expect("valid token must yield user");
        assert_eq!(user["id"], "u1");
        // 坏 token / 缺 token → Err（401 语义）
        assert!(guard.verify("/items", Some("Bearer nope")).is_err());
        assert!(guard.verify("/items", None).is_err());
    }

    /// JSON publish → 订阅通道必收 text 信封帧（bus Bytes 载荷才会走 Binary）。
    fn env_text(f: only_js::bridge::WsSend) -> String {
        match f {
            only_js::bridge::WsSend::Text(s) => s,
            other => panic!("json publish must deliver text frame, got {other:?}"),
        }
    }

    /// 硬验收（Task 6.1 Step 5）：真 kafka 插件 broker 下 Task 0.5 共享语义回归
    /// （env-gated，`OJ_TEST_KAFKA_BROKERS` 给逗号分隔 bootstrap servers；未设置 → 跳过）。
    /// 同一 broker 实例（一个 FfiEventBroker，单消费循环/每 topic）上两个订阅通道
    /// （模拟跨 actor 池与全部 WS 连接）共享同一 topic：一次 publish → 插件消费循环
    /// 经 host.deliver → 全局 DELIVER_TARGETS 扇出 → 两通道都收到。
    #[tokio::test(flavor = "multi_thread")]
    async fn kafka_plugin_broker_shared_across_channels() {
        let brokers = match std::env::var("OJ_TEST_KAFKA_BROKERS") {
            Ok(b) if !b.is_empty() => b,
            _ => {
                eprintln!("skip: OJ_TEST_KAFKA_BROKERS unset");
                return;
            }
        };
        let t = tmpdir("sc-busshare-k");
        let pdir = t.0.join(host_triple());
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::copy(
            bus_kafka_plugin_artifact(),
            pdir.join(plugin_file("bus-kafka")),
        )
        .unwrap();
        let cfg = Config {
            plugins_dir: Some(t.0.clone()),
            plugins: HashMap::from([("bus-kafka".into(), serde_json::json!({}))]),
            ..Default::default()
        };
        let mut r = Registries::default();
        let plugins = assemble_plugins(&cfg, &t.0, &mut r).await.unwrap();
        assert_eq!(plugins.len(), 1);
        let broker_cfg = only_js::config::BrokerCfg {
            kind: "kafka".into(),
            brokers: brokers.split(',').map(|s| s.trim().to_string()).collect(),
            group: Some("oj-shared".into()),
            topic_prefix: Some(format!("ojshare-k-{}", std::process::id())),
            ..Default::default()
        };
        let broker = r.bus.connect(&Some(broker_cfg)).await.unwrap();
        let topic = format!("shared.{}", std::process::id());
        let (tx1, mut rx1) = tokio::sync::mpsc::unbounded_channel();
        let (tx2, mut rx2) = tokio::sync::mpsc::unbounded_channel();
        broker.subscribe(&topic, tx1).await.unwrap();
        broker.subscribe(&topic, tx2).await.unwrap(); // 同 topic 第二通道（不新起消费）
        tokio::time::sleep(std::time::Duration::from_millis(500)).await; // 等消费就绪
        broker
            .publish(
                &topic,
                &only_js::bridge::BusPayload::Json(serde_json::json!({ "v": 9 })),
            )
            .await
            .unwrap();
        let f1 = tokio::time::timeout(std::time::Duration::from_secs(10), rx1.recv())
            .await
            .expect("shared receive 1 timeout")
            .expect("channel 1 closed");
        let f2 = tokio::time::timeout(std::time::Duration::from_secs(10), rx2.recv())
            .await
            .expect("shared receive 2 timeout")
            .expect("channel 2 closed");
        let t1 = env_text(f1);
        let v1: serde_json::Value = serde_json::from_str(&t1).unwrap();
        assert_eq!(v1["data"]["v"], 9, "{t1}");
        let t2 = env_text(f2);
        let v2: serde_json::Value = serde_json::from_str(&t2).unwrap();
        assert_eq!(v2["data"]["v"], 9, "{t2}");
    }

    /// 硬验收（Task 6.1 Step 5）：真 rabbitmq 插件 broker 下 Task 0.5 共享语义回归
    /// （env-gated，`OJ_TEST_RABBITMQ_URL` 给 amqp URL；未设置 → 跳过）。语义同 kafka 测试。
    #[tokio::test(flavor = "multi_thread")]
    async fn rabbitmq_plugin_broker_shared_across_channels() {
        let url = match std::env::var("OJ_TEST_RABBITMQ_URL") {
            Ok(u) if !u.is_empty() => u,
            _ => {
                eprintln!("skip: OJ_TEST_RABBITMQ_URL unset");
                return;
            }
        };
        let t = tmpdir("sc-busshare-r");
        let pdir = t.0.join(host_triple());
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::copy(
            bus_rabbitmq_plugin_artifact(),
            pdir.join(plugin_file("bus-rabbitmq")),
        )
        .unwrap();
        let cfg = Config {
            plugins_dir: Some(t.0.clone()),
            plugins: HashMap::from([("bus-rabbitmq".into(), serde_json::json!({}))]),
            ..Default::default()
        };
        let mut r = Registries::default();
        let plugins = assemble_plugins(&cfg, &t.0, &mut r).await.unwrap();
        assert_eq!(plugins.len(), 1);
        let broker_cfg = only_js::config::BrokerCfg {
            kind: "rabbitmq".into(),
            url: Some(url),
            topic_prefix: Some(format!("ojshare-r-{}", std::process::id())),
            ..Default::default()
        };
        let broker = r.bus.connect(&Some(broker_cfg)).await.unwrap();
        let topic = format!("shared.{}", std::process::id());
        let (tx1, mut rx1) = tokio::sync::mpsc::unbounded_channel();
        let (tx2, mut rx2) = tokio::sync::mpsc::unbounded_channel();
        broker.subscribe(&topic, tx1).await.unwrap();
        broker.subscribe(&topic, tx2).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        broker
            .publish(
                &topic,
                &only_js::bridge::BusPayload::Json(serde_json::json!({ "v": 11 })),
            )
            .await
            .unwrap();
        let f1 = tokio::time::timeout(std::time::Duration::from_secs(10), rx1.recv())
            .await
            .expect("shared receive 1 timeout")
            .expect("channel 1 closed");
        let f2 = tokio::time::timeout(std::time::Duration::from_secs(10), rx2.recv())
            .await
            .expect("shared receive 2 timeout")
            .expect("channel 2 closed");
        let t1 = env_text(f1);
        let v1: serde_json::Value = serde_json::from_str(&t1).unwrap();
        assert_eq!(v1["data"]["v"], 11, "{t1}");
        let t2 = env_text(f2);
        let v2: serde_json::Value = serde_json::from_str(&t2).unwrap();
        assert_eq!(v2["data"]["v"], 11, "{t2}");
    }
}
