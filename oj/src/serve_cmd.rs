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
use only_js::bridge::{
    BusBackendRegistry, DataAccessor, DbBackendRegistry, EsBackend, ModuleIndex, PluginInfo,
};
use only_js::config::{self, Config, LoadedConfig};

use crate::app::{App, ResourceProfiles};
use crate::args::ServeArgs;
use crate::tasks::Tasking;

/// API 前缀缺省常量（v0.1.58）：旧 `server.api_prefix` 键已删，CLI `-b` / 挂载折叠
/// 都回落此值——与删除前的 `ServerCfg::default().api_prefix` 同值（零 URL 变化）。
pub const DEFAULT_API_PREFIX: &str = "/v1/api";

/// CLI 挂载折叠（v0.1.58）：`--api-path d [-b B]` → upsert `{prefix: B, api: d}`
/// （B = `-b` > [`DEFAULT_API_PREFIX`]，**不再有全局 base**——挂载自带完整 prefix）；
/// `--app-path` 裸 `d` → upsert `{prefix: "/", web: d}`（按 CWD 预绝对化）、
/// `prefix=dir` → 同 prefix 条目替换否则追加。`-b` 无 `--api-path` → Err
/// （裸 -b 在挂载模型下无处附着，静默忽略等于部署假配置）。
fn fold_cli_mounts(cfg: &mut Config, a: &ServeArgs) -> Result<(), String> {
    if a.api_path.is_some() && a.module_root.is_some() {
        return Err(
            "--module-root and --api-path are mutually exclusive (both mean the source tree root)"
                .into(),
        );
    }
    // --module-root 与 --api-path 同为「源码树根」：-b 附着任一条都成立。
    if a.base.is_some() && a.api_path.is_none() && a.module_root.is_none() {
        return Err("--base/-b requires --api-path (api mounts carry their own prefix)".into());
    }
    if !a.app_path.is_empty() {
        fold_cli_app_paths(cfg, &a.app_path)?;
    }
    if let Some(d) = a.api_path.as_deref().or(a.module_root.as_deref()) {
        // URL api mount 只能经 config 的 mounts[].api 声明（含 sha256 完整性锚），
        // --api-path 仅接受本地目录（远程 tarball 在 CLI 折叠期无 sha256 上下文）。
        if only_js::config::is_http_url(d) {
            return Err(
                "--api-path does not accept URLs (declare remote dist via mounts[].api with sha256 in config.yaml)"
                    .into(),
            );
        }
        let prefix = resolve_app_prefix(a.base.as_deref().unwrap_or(DEFAULT_API_PREFIX))
            .map_err(|e| format!("--base: {e}"))?;
        let api = absolutize_cwd(
            &std::env::current_dir().map_err(|e| format!("resolve --api-path: {e}"))?,
            d,
        );
        let idx = cfg.mounts.iter().position(|m| {
            m.api.is_some() && resolve_app_prefix(&m.prefix).is_ok_and(|x| x == prefix)
        });
        let entry = config::MountConf {
            prefix,
            api: Some(api),
            web: None,
            spa: None,
            headers: Default::default(),

            ..Default::default()
        };
        match idx {
            Some(i) => cfg.mounts[i] = entry,
            None => cfg.mounts.push(entry),
        }
    }
    Ok(())
}

pub async fn run(a: ServeArgs) -> Result<(), String> {
    // --daemon：re-exec 自身（剥掉 --daemon）脱离终端后父进程即退；
    // 子进程日志照常落 server.logs_dir（console 默认关闭）。
    if a.daemon {
        return daemonize();
    }
    // daemon 子进程回传通道（`OJ_DAEMON_READY_FD` 存在即子进程）：启动成败经该 fd
    // 告知父进程——`ok` = 已监听；失败 = 真因。非 daemon 子进程 env 缺省，全 no-op。
    daemon_ready::init_from_env();
    let r = run_inner(a).await;
    if let Err(e) = &r {
        daemon_ready::fail(e);
    }
    r
}

async fn run_inner(a: ServeArgs) -> Result<(), String> {
    let (mut cfg, top, config_dir) = load_cfg(a.config.as_deref(), true)?;
    // CLI 挂载折叠：--api-path / --module-root / --app-path 全部 upsert 进 cfg.mounts
    // （CLI 优先）。
    fold_cli_mounts(&mut cfg, &a)?;
    // 模块索引（`--module-root`）：装配期一次发现，贯穿 StableState / 路由 / 迁移 / 种子。
    let module_index: Option<Arc<ModuleIndex>> = match a.module_root.as_deref() {
        None => None,
        Some(mr) => {
            let root = PathBuf::from(absolutize_cwd(
                &std::env::current_dir().map_err(|e| format!("resolve --module-root: {e}"))?,
                mr,
            ));
            // 产物树（release）没有「模块根」概念——静默忽略会让下游以为生效了。
            if is_release(&root) {
                return Err(format!(
                    "--module-root is a source-tree option: {} is a release tree (contains manifests.yaml) — point it at the src tree, or drop --module-root",
                    root.display()
                ));
            }
            Some(Arc::new(only_js::bridge::resolve_roots(&root, true, None)?))
        }
    };
    if let Some(p) = a.cert_path.clone() {
        cfg.server.certificate_path = p;
    }
    if let Some(p) = a.key_path.clone() {
        cfg.server.public_key_path = p;
    }
    // 准入门（规则 5）：mounts（config + CLI 折叠后）为空 → 报错提示。
    // 目录存在性统一由装配期 resolve_mounts fail-fast（含具体 prefix/条目序号）。
    admission_gate(&cfg)?;
    // primary api 树（tasks 目录 / 日志行展示用；解析细节由 from_config 统一裁决）。
    let (dir, ts, base) = match cfg
        .mounts
        .iter()
        .find_map(|m| m.api.as_ref().map(|d| (d.clone(), m.prefix.clone())))
    {
        Some((d, b)) => {
            let d = {
                let p = PathBuf::from(&d);
                if p.is_absolute() {
                    p
                } else {
                    config_dir.join(p)
                }
            };
            let ts = !is_release(&d);
            (d, ts, b)
        }
        None => {
            eprintln!("note: no api mount — serving web mount(s) only");
            (
                config_dir.join(".oj-static-only"),
                false,
                DEFAULT_API_PREFIX.into(),
            )
        }
    };
    // 初始化日志：目录默认 config 相对 ./logs，可在 server.logs_dir 配置；不存在自动创建。
    // 大小滚动参数 server.logs_max_bytes / logs_keep_files。
    let logs_dir = serve::logging::resolve_logs_dir(cfg.server.logs_dir.as_deref(), &config_dir);
    // 终端输出默认关闭（只落盘）；config 的 server.console_log 或 CLI `--console-log`
    // 任一打开即打开（「或」而非「覆盖」——两个入口都是「打开」语义，没有「关闭」的一方）。
    let console = cfg.server.console_log || a.console_log;
    serve::logging::init(
        &logs_dir,
        cfg.server.logs_max_m,
        cfg.server.logs_keep_files as usize,
        console,
    );
    let addr = to_socket_addrs_sync(&format!("{}:{}", cfg.server.host, cfg.server.port))?;
    let tasks_cfg = cfg.tasks.clone();
    let static_only = cfg.mounts.iter().all(|m| m.api.is_none());
    // 热更所需：已知 api 挂载 prefix（ReloadState 分流 _system 用）+ 配置路径（摘要回显）。
    let prefixes: Vec<String> = cfg
        .mounts
        .iter()
        .filter_map(|m| m.api.as_ref().map(|_| m.prefix.clone()))
        .collect();
    let config_path = a
        .config
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(|| config_dir.join("config.yaml"));
    // reload_api 开关须在读 cfg 前捕获（cfg 随后移入 App::from_config）。
    let reload_api = cfg.server.reload_api;
    // 热更触发槽（ojm PR-3）：槽指针此刻创建并随 from_config 冻结进 Extras/
    // StableState；内容在 ReloadState 构造 + set_executor 之后由
    // serve_hot_reload_branch 后填（槽位后填，解 App↔ReloadState 构造环）。
    let reload_slot = Arc::new(std::sync::OnceLock::new());
    let mut app = App::from_config(
        cfg,
        &top,
        &config_dir,
        false,
        &ResourceProfiles::default(),
        None,
        Some(reload_slot.clone()),
        module_index.clone(),
    )
    .await?;
    // 任务域事件化（PRD v2 §6/§9 阶段 1）：扫描 → 探测 loop_body 导出分流——
    // 有 loop_body = 池化模式（TaskPool，多任务共享有限 Worker）；无 = 存量 TLA
    // 监督模式（一任务一线程 + 退避重启，tasks.rs 原样）。两模式零迁移共存。
    let task_flag = app.tasks_flag();
    let task_pool = crate::tasks::assemble_tasking(
        &tasks_cfg,
        &dir,
        app.make_task_bridge(),
        task_flag.clone(),
    )?;
    // 任务管理 API（PRD v2 §6.6）：有池（池化长任务/cron）才挂 `{base}/tasks`。
    if let Some(pool) = task_pool.pool.clone() {
        let state = app.tasks_api_state(pool.registry.clone(), Some(pool));
        app.merge_router(serve::tasks::tasks_router(&base, state));
    }
    // 全仓首个信号处理器（评审 F5/S2）：SIGINT/SIGTERM → 置停机 flag → HTTP 侧
    // with_graceful_shutdown 同信号排空在途请求；任务线程在 grace 内自然收场
    // （不退者由 run_task 内置看门狗强杀）。
    if reload_api {
        return serve_hot_reload_branch(
            &app,
            addr,
            &a,
            &top,
            &config_dir,
            &base,
            &dir,
            &ResourceProfiles::default(),
            task_flag.clone(),
            task_pool,
            static_only,
            ts,
            prefixes,
            config_path,
            reload_slot,
            module_index,
        )
        .await;
    }
    let (bound, h) = app
        .serve_graceful(
            addr,
            shutdown_signal(Arc::new(tokio::sync::Mutex::new(task_flag.clone()))),
        )
        .await?;
    // daemon 子进程：监听已绑定 = 启动成功落定，回传父进程（非 daemon 子进程 no-op）。
    daemon_ready::ok();
    println!(
        "oj serve listening on http://{bound}{} (dir={}, {})",
        base,
        dir.display(),
        if static_only {
            "static-only"
        } else if ts {
            "dev/ts"
        } else {
            "release/js"
        }
    );
    h.await.map_err(|e| format!("server task: {e}"))?;
    // 任务线程收场（flag 已置位；join 为阻塞调用，移交 blocking 池）：
    // 先存量 TLA 监督器，再池化 Worker（退出前尽力 teardown 全部会话）。
    let pool2 = task_pool.pool.clone();
    tokio::task::spawn_blocking(move || {
        if let Some(sup) = task_pool.sup {
            sup.shutdown();
        }
        if let Some(p) = pool2 {
            p.shutdown_and_join();
        }
    })
    .await
    .map_err(|e| format!("tasks shutdown: {e}"))?;
    // 停机 graceful drain（spec §6 ⑤）：HTTP 已停收、任务已收场后，排空在途邮件
    // （插件侧停收 → 等在途 job 跑完 → 销毁 transport）。超时只告警，不阻断退出。
    app.drain_mail(MAIL_DRAIN_TIMEOUT).await;
    Ok(())
}

/// 热更新服务形态（v1，`server.reload_api=true`）：构造 `ReloadState` + 注入
/// `reload_executor` 闭包，经 `App::serve_hot_reload` 起 swappable 服务。
#[allow(clippy::too_many_arguments)]
async fn serve_hot_reload_branch(
    app: &App,
    addr: SocketAddr,
    args: &ServeArgs,
    top: &serde_json::Value,
    config_dir: &Path,
    base: &str,
    dir: &Path,
    profiles: &ResourceProfiles,
    task_flag: Arc<std::sync::atomic::AtomicBool>,
    initial_tasking: Tasking,
    static_only: bool,
    ts: bool,
    prefixes: Vec<String>,
    config_path: PathBuf,
    // 热更触发槽（ojm PR-3）：run() 创建并已随首代 App::from_config 冻结进
    // Extras/StableState；本函数在 ReloadState + set_executor 之后 set 内容。
    reload_slot: Arc<std::sync::OnceLock<only_js::bridge::reload_bridge::ReloadExecutorLike>>,
    // 模块索引（`--module-root`）：跨代同一 Arc（布局由命令行给定，热更不重新发现）。
    module_index: Option<Arc<ModuleIndex>>,
) -> Result<(), String> {
    // 首代归一 raw 文档作为幂等/diff 基线（reload 路径一致，稳定）。
    // 用 raw 文档而非解析后的 `Config`：避免对核心 `Config` 强加 `Serialize` /
    // `Clone`（涉及大量子段类型），且 CLI/进程级覆盖在进程生命周期内不变，
    // 落盘文件内容即热更真触发源，足以做幂等判定与段 diff。
    let baseline_raw =
        std::fs::read(&config_path).map_err(|e| format!("reload baseline read: {e}"))?;
    let baseline_doc: serde_yaml::Value =
        serde_yaml::from_slice(&baseline_raw).map_err(|e| format!("reload baseline parse: {e}"))?;
    let baseline_sha = doc_sha(&baseline_doc).map_err(|e| format!("reload baseline sha: {e}"))?;
    let ctx = Arc::new(HotReloadCtx {
        args: args.clone(),
        top: top.clone(),
        config_dir: config_dir.to_path_buf(),
        base: base.to_string(),
        dir: dir.to_path_buf(),
        profiles: profiles.clone(),
        backend: app.backend(),
        current_doc: tokio::sync::Mutex::new(baseline_doc),
        current_sha: tokio::sync::Mutex::new(baseline_sha),
        config_path: config_path.clone(),
        module_index: module_index.clone(),
        tasking: tokio::sync::Mutex::new(initial_tasking),
        current_task_flag: Arc::new(tokio::sync::Mutex::new(task_flag)),
        committer: tokio::sync::OnceCell::new(),
        // 热更触发槽（ojm PR-3）：do_reload 每代重建 App 时随 from_config 穿参，
        // 保证槽指针跨代同一（槽内容本代已填，重建桥后 reloadConfig 依旧有效）。
        reload_slot: reload_slot.clone(),
    });

    let rs = serve::reload::ReloadState::new(
        Arc::new(app.router()),
        prefixes,
        app.auth_guard(),
        true,
        config_path,
        None,
    );
    let committer = rs.committer();
    ctx.set_committer(committer);
    rs.set_executor(build_reload_executor(ctx.clone())).await;
    // 槽位后填（ojm PR-3）：闭包走 ReloadState 共享处理体（trigger_sync）——
    // 与 HTTP POST 同一 executor 实例、同一把 reload_lock。OnceLock set 失败 =
    // 二次装配；本进程 ReloadState 只构造一次，理论不可达，保留首填（同一 rs，
    // 语义等价）仅记 warn。
    let rs_for_slot = rs.clone();
    let slot_exec: only_js::bridge::reload_bridge::ReloadExecutorLike =
        Arc::new(move || rs_for_slot.trigger_sync());
    if reload_slot.set(slot_exec).is_err() {
        eprintln!("warn: reload slot already set; keeping the first executor");
    }

    let (bound, h) = app
        .serve_hot_reload(addr, rs, shutdown_signal(ctx.current_task_flag.clone()))
        .await?;
    // daemon 子进程：监听已绑定 = 启动成功落定，回传父进程（非 daemon 子进程 no-op）。
    daemon_ready::ok();
    println!(
        "oj serve listening on http://{bound}{} (dir={}, {}, hot-reload on)",
        base,
        dir.display(),
        if static_only {
            "static-only"
        } else if ts {
            "dev/ts"
        } else {
            "release/js"
        }
    );
    h.await.map_err(|e| format!("server task: {e}"))?;
    // 停机：关闭最新一代任务池（可能在重载中已切换）。
    let mut latest = ctx.tasking.lock().await;
    let pool2 = latest.pool.clone();
    // `TaskSupervisor` 无 `Default` 故不能用 `Option::take()`，这里显式 replace。
    #[allow(clippy::mem_replace_option_with_none)]
    let sup2 = std::mem::replace(&mut latest.sup, None);
    drop(latest);
    tokio::task::spawn_blocking(move || {
        if let Some(sup) = sup2 {
            sup.shutdown();
        }
        if let Some(p) = pool2 {
            p.shutdown_and_join();
        }
    })
    .await
    .map_err(|e| format!("tasks shutdown: {e}"))?;
    app.drain_mail(MAIL_DRAIN_TIMEOUT).await;
    Ok(())
}

/// 热更共享上下文（executor 闭包捕获）。
struct HotReloadCtx {
    args: ServeArgs,
    top: serde_json::Value,
    config_dir: PathBuf,
    base: String,
    dir: PathBuf,
    profiles: ResourceProfiles,
    /// 复用后端（含全部连接池 + 插件 vtable），热更不重建（§5 ②）。
    backend: Arc<crate::app::Backend>,
    /// 当前代归一 raw 文档（段 diff 基线）+ 其 sha256（幂等判定）。
    current_doc: tokio::sync::Mutex<serde_yaml::Value>,
    current_sha: tokio::sync::Mutex<String>,
    /// 配置路径（幂等读取落盘文件用）。
    config_path: PathBuf,
    /// 当前代任务池（重载交接）。
    tasking: tokio::sync::Mutex<Tasking>,
    /// 当前代任务池停机 flag 的「运行时指向」：热更后 `do_reload` 会把它换成
    /// 新一代池的 flag，SIGTERM 经 `shutdown_signal` 精确停机在跑的那一代池。
    current_task_flag: Arc<tokio::sync::Mutex<Arc<std::sync::atomic::AtomicBool>>>,
    /// 提交句柄（构造 ReloadState 后注入，避免循环依赖）。
    committer: tokio::sync::OnceCell<serve::reload::Committer>,
    /// 热更触发槽（ojm PR-3）：与首代 Extras/StableState.reload 同一 Arc；
    /// do_reload 每代重建 App 时随 from_config 穿参，槽指针跨代同一。
    reload_slot: Arc<std::sync::OnceLock<only_js::bridge::reload_bridge::ReloadExecutorLike>>,
    /// 模块索引（`--module-root`）：跨代同一 Arc——模块布局由命令行给定，热更只换
    /// 配置与路由，不重新发现模块（新增模块目录需重启，见文档）。
    module_index: Option<Arc<ModuleIndex>>,
}

impl HotReloadCtx {
    fn set_committer(&self, c: serve::reload::Committer) {
        let _ = self.committer.set(c);
    }

    async fn committer(&self) -> &serve::reload::Committer {
        self.committer.get().expect("committer set before reload")
    }
}

/// 构造 `reload_executor` 闭包：在独立线程 + 多线 runtime 跑 `do_reload`
/// （`from_config` 含 DB verify / 模块转译 / 任务池交接，需在带 blocking 池的多线
/// runtime 上跑，current_thread 会因内部 spawn/join 段死锁；禁止在 axum handler
/// 直接 await，故包一层独立线程）。
fn build_reload_executor(ctx: Arc<HotReloadCtx>) -> serve::reload::ReloadExecutor {
    Arc::new(move || {
        let ctx = ctx.clone();
        let handle = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(4)
                .enable_all()
                .build()
                .map_err(|e| (500u16, format!("reload runtime: {e}")))?;
            rt.block_on(async { do_reload(&ctx).await })
        });
        handle
            .join()
            .unwrap_or_else(|_| Err((500u16, "reload thread panicked".to_string())))
    })
}

/// 单次热更全流程（§5）：归一 → 影子装配（复用后端）→ 任务池交接 → swap + 三清单。
async fn do_reload(ctx: &HotReloadCtx) -> Result<serve::reload::ReloadOutcome, (u16, String)> {
    // ① 归一：load_cfg（解密→类型→mounts→legacy 键）→ fold_cli_mounts 重放 →
    //    cert/key 覆盖重放 → admission_gate。
    let (mut cfg, _, _) = load_cfg(ctx.args.config.as_deref(), true).map_err(|e| (400u16, e))?;
    fold_cli_mounts(&mut cfg, &ctx.args).map_err(|e| (400u16, e))?;
    if let Some(p) = ctx.args.cert_path.clone() {
        cfg.server.certificate_path = p;
    }
    if let Some(p) = ctx.args.key_path.clone() {
        cfg.server.public_key_path = p;
    }
    admission_gate(&cfg).map_err(|e| (400u16, e))?;
    // 幂等 / diff 基线：归一 raw 文档（落盘文件解析，CLI 覆盖在进程生命周期不变）。
    let raw = std::fs::read(&ctx.config_path).map_err(|e| (400u16, format!("read config: {e}")))?;
    let doc: serde_yaml::Value =
        serde_yaml::from_slice(&raw).map_err(|e| (400u16, format!("parse config: {e}")))?;
    let sha = doc_sha(&doc).map_err(|e| (400u16, e))?;
    // 幂等：归一 raw 文档的 sha256 与当前代相同 → unchanged（不装配、不 swap）。
    let cur_gen = ctx.committer().await.generation().await;
    {
        let cur = ctx.current_sha.lock().await;
        if *cur == sha {
            return Ok(serve::reload::ReloadOutcome {
                generation: cur_gen,
                applied: vec![],
                restart_pending: vec![],
                kept_listener: vec![],
                unchanged: true,
            });
        }
    }
    // 段 diff 三清单（旧代 vs 新代）。
    let old_doc = ctx.current_doc.lock().await.clone();
    let (applied, restart_pending, kept_listener) = diff_configs(&old_doc, &doc);
    // ② 影子装配（复用旧 Backend：不重建连接池、不二次 init 插件；迁移强制 verify-only）。
    // `cfg` 整体移入 `from_config`；任务配置先取出克隆供下方任务池交接。
    let tasks_cfg = cfg.tasks.clone();
    let mut new_app = App::from_config(
        cfg,
        &ctx.top,
        &ctx.config_dir,
        false,
        &ctx.profiles,
        Some(ctx.backend.clone()),
        // 槽指针随代传递（ojm PR-3）：复用 Backend 时 Extras 不重建，但同一 Arc
        // 的传入保证「每代热更新建 App 同样带槽」，热更一次后 reloadConfig 不失效。
        Some(ctx.reload_slot.clone()),
        ctx.module_index.clone(),
    )
    .await
    .map_err(|e| (400u16, e))?;
    // ③ 任务池交接（顺序：先建新池 fail-fast，再停旧池——回滚完整：新池失败则旧池仍跑）。
    let new_flag = new_app.tasks_flag();
    let new_tasking = {
        // 旧池用「上一代」flag 停机：取出 current_task_flag 当前指向的旧 flag，
        // 置位使其退出；随后把 current_task_flag 指向新 flag，使 SIGTERM 精确停机
        // 当前在跑的新池（spec §6 ⑤）。
        let old_flag = ctx.current_task_flag.lock().await.clone();
        let mut old = ctx.tasking.lock().await;
        let t = crate::tasks::assemble_tasking(
            &tasks_cfg,
            &ctx.dir,
            new_app.make_task_bridge(),
            new_flag.clone(),
        )
        .map_err(|e| (500u16, e))?;
        // 旧池停机（flag 置位 → 驱动退出；spawn_blocking 同步 join）。
        old_flag.store(true, std::sync::atomic::Ordering::SeqCst);
        let old_pool = old.pool.clone();
        // `TaskSupervisor` 无 `Default` 故不能用 `Option::take()`，这里显式 replace。
        #[allow(clippy::mem_replace_option_with_none)]
        let old_sup = std::mem::replace(&mut old.sup, None);
        drop(old);
        if let Some(p) = old_pool {
            let p2 = p.clone();
            tokio::task::spawn_blocking(move || p2.shutdown_and_join())
                .await
                .map_err(|e| (500u16, format!("join old task pool: {e}")))?;
        }
        if let Some(s) = old_sup {
            s.shutdown();
        }
        t
    };
    // 任务管理 API 随新代重建（有池才挂）。
    if let Some(p) = new_tasking.pool.clone() {
        let state = new_app.tasks_api_state(p.registry.clone(), Some(p));
        new_app.merge_router(serve::tasks::tasks_router(&ctx.base, state));
    }
    // ④ watch swap：新 Router 送入通道，旧 App 随在途请求完成自然 drain。
    let new_inner = std::sync::Arc::new(new_app.router());
    let generation = ctx.committer().await.commit(new_inner, sha.clone()).await;
    *ctx.current_doc.lock().await = doc;
    *ctx.current_sha.lock().await = sha;
    *ctx.tasking.lock().await = new_tasking;
    // 当前代 flag 指向新池：后续 SIGTERM 精确停机这一代（spec §6 ⑤）。
    *ctx.current_task_flag.lock().await = new_flag.clone();
    Ok(serve::reload::ReloadOutcome {
        generation,
        applied,
        restart_pending,
        kept_listener,
        unchanged: false,
    })
}

/// 归一 raw 文档的 sha256（幂等 / 代际追踪用）。raw 文档 → 规范 JSON → sha256。
fn doc_sha(doc: &serde_yaml::Value) -> Result<String, String> {
    let v = serde_json::to_value(doc).map_err(|e| format!("cfg doc: {e}"))?;
    let s = serde_json::to_string(&v).map_err(|e| format!("cfg enc: {e}"))?;
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    h.update(s.as_bytes());
    Ok(format!("{:x}", h.finalize()))
}

/// 新旧归一 raw 文档顶层段逐段 diff → 三清单（§6 矩阵）。
/// 比对 `known_top_level_keys` 范围内各段（serde_yaml::Value 的 PartialEq）。
fn diff_configs(
    old: &serde_yaml::Value,
    new: &serde_yaml::Value,
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut applied = Vec::new();
    let mut restart_pending = Vec::new();
    let mut kept_listener = Vec::new();
    for key in only_js::config::known_top_level_keys() {
        let o = old.get(key);
        let n = new.get(key);
        if o == n {
            continue;
        }
        match *key {
            "db" | "redis" | "blob" | "es" | "broker" | "smtp" | "ldap" | "auth" | "kafkas"
            | "rabbits" | "plugins" | "plugins_dir" | "secrets" => {
                restart_pending.push((*key).to_string())
            }
            // 特权面段（ojm PR-3 安全阻塞项 3 修复，显式验收项）：platform.modules
            // （reloadConfig 等平台 API 资格）与 fs（jail 授权面）若随热更生效 =
            // 特权模块可热改自己的白名单（自提权链）。归 restart_pending：验证合法
            // 但必须重启生效，HTTP 与 JS 回调两侧一致。
            "platform" | "fs" => restart_pending.push((*key).to_string()),
            // U-1 v0.1.93：访问控制策略段（`db_access:` / `db_client:`）改动**不强行热生效**
            // —— StableState 不可变，强行热改会破坏红线。纳入已知键并标 restart_pending，
            // 让运维明确知道「改了这两段必须重启」，消除旧授权静默盲区的误标 `applied`。
            "db_access" | "db_client" | "db_session" | "audit" | "mgmt" => {
                restart_pending.push((*key).to_string())
            }
            "server" => kept_listener.push((*key).to_string()),
            _ => applied.push((*key).to_string()),
        }
    }
    (applied, restart_pending, kept_listener)
}

/// 停机 mail drain 的总超时：在途投递本身已在插件侧受 `profile.timeout` 约束，
/// 这里给 10s 上限 —— 足够跑完正常在途投递，又不会让 SIGTERM 后的退出过程长期挂住。
const MAIL_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// 后台运行入口（unix 分支）：re-exec 自身（剥掉 --daemon）脱离终端后父进程即退，
/// setsid 脱离控制终端（终端关闭的 SIGHUP 不再波及），stdio 重定向 /dev/null。
/// 父进程随后**等子进程就绪信号**（[`wait_daemon_ready`]）：`ok` = 已监听；启动期
/// 死亡 = 带真因报错并非零退出——不再有「spawn 成功即报 daemonized」的假成功
/// （stdio 全指 /dev/null，子进程启动失败时父进程秒退会让真因彻底不可见）。
///
/// 自带 glibc 发行包（deploy.sh 启动器注入 OJ_BUNDLED_LD / OJ_BUNDLED_LIB）：re-exec
/// **直接 exec 随包 ld-linux + <bin>/oj.bin**（见 [`daemon_reexec_cmd`]）——kernel 按
/// oj.bin 的 PT_INTERP（系统 /lib64 路径）加载系统 ld-linux 的路子不走，低版本宿主
/// glibc 即崩；非打包形态 env 缺省，原样 re-exec。
#[cfg(unix)]
fn daemonize() -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    let mut cmd = daemon_reexec_cmd()?;
    let null =
        std::fs::File::open("/dev/null").map_err(|e| format!("daemon: open /dev/null: {e}"))?;
    // 就绪管道（父读子写）：子进程启动落定后经此回传 ok / 真因。写端 fd 号经 env 告知
    // 子进程（fd 非 CLOEXEC、exec 后号子不变；读端设 CLOEXEC，子进程不继承）。
    let (rx, tx) = daemon_ready_pipe().map_err(|e| format!("daemon: ready pipe: {e}"))?;
    cmd.args(strip_daemon_flag(std::env::args_os().skip(1)))
        .stdin(null)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .env(OJ_DAEMON_READY_FD, tx.to_string());
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
    let pid = child.id();
    // 父进程只留读端：本副本的写端关掉，子进程死亡时读端才见 EOF。
    // Safety: tx 为管道写端、父进程内无其他用户。
    unsafe { libc::close(tx) };
    match wait_daemon_ready(rx, DAEMON_READY_TIMEOUT) {
        DaemonReady::Ok => {
            println!("oj serve daemonized (pid {pid})");
            Ok(())
        }
        DaemonReady::Starting => {
            // 超时但子进程还活着：不杀不判死——大应用装配慢于超时属正常，提示看日志。
            println!(
                "oj serve daemonized (pid {pid}); not confirmed listening within {}s — check server logs",
                DAEMON_READY_TIMEOUT.as_secs()
            );
            Ok(())
        }
        DaemonReady::Died(msg) => Err(format!(
            "daemon child (pid {pid}) exited during startup: {}",
            if msg.is_empty() {
                "no error captured (crash?) — see server logs".to_string()
            } else {
                msg
            }
        )),
    }
}

/// daemon re-exec 命令（unix）：打包形态（`OJ_BUNDLED_LD` 指向 `<bin>/lib/ld-linux-*`）
/// → **直接 exec 随包 ld-linux + `<bin>/oj.bin`**。刻意不走 `<bin>/oj 启动器脚本`：
/// 脚本经 kernel shebang 拉起**系统 /bin/sh**，DIR 计算还要 fork 子 shell exec
/// `readlink`/`dirname`——整段中间层都是系统 glibc，宿主这段任何异常（sh 变体 /
/// coreutils 缺失 / exec 受限）都让 daemon 子进程死得悄无声息（stdio 已指 /dev/null）。
/// 直接 exec 后子进程从 fork 后第一个 exec 起全程跑随包 glibc，链条零系统组件。
/// `<bin>/oj.bin` 缺失（异常布局）→ 回退启动器脚本（它自会重放 ld 链）；非打包形态
/// （env 缺省）→ 原样 re-exec 自身（bundled_reexec_cmd 直通分支）。
#[cfg(unix)]
fn daemon_reexec_cmd() -> Result<std::process::Command, String> {
    if let Some(ld) = std::env::var_os("OJ_BUNDLED_LD") {
        let ld = std::path::PathBuf::from(ld);
        // <bin>/lib/ld-linux-* → <bin>；oj.bin 与 lib/ 都与 ld 同级布局（deploy.sh）。
        if let Some(bin_dir) = ld.parent().and_then(|p| p.parent()) {
            let oj_bin = bin_dir.join("oj.bin");
            if oj_bin.is_file() {
                let lib = ld.parent().expect("ld has a parent dir").to_path_buf();
                let mut c = std::process::Command::new(ld);
                c.arg("--library-path").arg(lib).arg(oj_bin);
                return Ok(c);
            }
            let launcher = bin_dir.join("oj");
            if launcher.is_file() {
                return Ok(std::process::Command::new(launcher));
            }
        }
    }
    let exe = std::env::current_exe().map_err(|e| format!("daemon: current_exe: {e}"))?;
    Ok(bundled_reexec_cmd(&exe))
}

/// 构造 daemon re-exec 命令：打包形态（env 齐）→ 打包 ld-linux + --library-path + 程序路径；
/// 否则 → 直接 re-exec 自身。
#[cfg(unix)]
fn bundled_reexec_cmd(exe: &std::path::Path) -> std::process::Command {
    match (
        std::env::var_os("OJ_BUNDLED_LD"),
        std::env::var_os("OJ_BUNDLED_LIB"),
    ) {
        (Some(ld), Some(lib)) => {
            let mut c = std::process::Command::new(ld);
            c.arg("--library-path").arg(lib).arg(exe);
            c
        }
        _ => std::process::Command::new(exe),
    }
}

/// re-exec 参数剥离 --daemon（长旗标独占，无短形式，精确匹配即安全）。
fn strip_daemon_flag(
    args: impl IntoIterator<Item = std::ffi::OsString>,
) -> Vec<std::ffi::OsString> {
    args.into_iter().filter(|a| a != "--daemon").collect()
}

/// daemon 子进程就绪回传的 env 键：值 = 就绪管道写端 fd 号（exec 后号子不变）。
/// 子进程（serve 非 --daemon 形态）见该 env 即知自己是 daemon 子进程。
#[cfg(unix)]
const OJ_DAEMON_READY_FD: &str = "OJ_DAEMON_READY_FD";

/// 父进程等就绪信号的总超时：config 装配 + 插件加载 + V8 初始化在大应用上要数秒，
/// 30s 够慢机器落定；超时只是「未确认」，不杀不判死。
#[cfg(unix)]
const DAEMON_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// 就绪管道（父读子写）：读端设 CLOEXEC（子进程不继承），写端原样继承、号子经 env 告知。
#[cfg(unix)]
fn daemon_ready_pipe() -> std::io::Result<(libc::c_int, libc::c_int)> {
    let mut fds = [0 as libc::c_int; 2];
    // Safety: 纯 fd 创建，buf 为栈上双元素数组。
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // Safety: 纯 fcntl。
    if unsafe { libc::fcntl(fds[0], libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
        let e = std::io::Error::last_os_error();
        // Safety: 两支皆为刚建出、本函数独享的 fd。
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
        return Err(e);
    }
    Ok((fds[0], fds[1]))
}

/// 父进程等的就绪信号：Ok = 子进程已监听；Starting = 超时但仍存活；Died = 启动期退出。
#[cfg(unix)]
#[derive(Debug)]
enum DaemonReady {
    Ok,
    Starting,
    Died(String),
}

/// 等就绪信号（`rx` 读端所有权在函数内收回并关闭）：读到 `ok` → Ok；EOF 无字节
/// （子进程未及回传即死）→ Died("")；EOF 前有字节 → Died(真因)；超时 → Starting。
#[cfg(unix)]
fn wait_daemon_ready(rx: libc::c_int, timeout: std::time::Duration) -> DaemonReady {
    let r = wait_daemon_ready_inner(rx, timeout);
    // Safety: rx 为本函数独享（父进程另一副本已关），关闭一次。
    unsafe { libc::close(rx) };
    r
}

#[cfg(unix)]
fn wait_daemon_ready_inner(rx: libc::c_int, timeout: std::time::Duration) -> DaemonReady {
    use std::io::Read;
    use std::os::unix::io::FromRawFd;
    // 不取得 fd 所有权（防 drop 误关）：ManuallyDrop 包一层。
    let mut f = std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(rx) });
    let mut buf: Vec<u8> = Vec::with_capacity(512);
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let remain = deadline.saturating_duration_since(std::time::Instant::now());
        if remain.is_zero() {
            return DaemonReady::Starting;
        }
        let mut pfd = libc::pollfd {
            fd: rx,
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = remain.as_millis().min(i32::MAX as u128) as libc::c_int;
        // Safety: pfd 栈上有效，n = 1。
        let n = unsafe { libc::poll(&mut pfd, 1, ms) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            // poll 自身失败（不该发生）：按未确认处理，不误判死亡。
            return DaemonReady::Starting;
        }
        if n == 0 {
            return DaemonReady::Starting;
        }
        let mut chunk = [0u8; 512];
        match f.read(&mut chunk) {
            // EOF：子进程写毕即关 / 已死。攒到的内容一次性裁决。
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let msg = String::from_utf8_lossy(&buf).trim().to_string();
    if msg == "ok" {
        DaemonReady::Ok
    } else {
        DaemonReady::Died(msg)
    }
}

/// daemon 子进程 → 父进程的就绪/失败回传（unix）。父进程（[`daemonize`]）建管道并把
/// 写端 fd 号经 env [`OJ_DAEMON_READY_FD`] 传给孩子；孩子启动落定后**写一次**即关：
/// [`ok`] 在监听绑定后调，[`fail`] 在 run 返回 Err 时调。进程级一次性（静态位防重复
/// 写——服务器运行期错误不得再回传，那时父进程早退出了）；env 缺省（非 daemon 子进程）
/// 时全部 no-op。
#[cfg(unix)]
mod daemon_ready {
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicBool, Ordering};

    static FD: OnceLock<libc::c_int> = OnceLock::new();
    static SIGNALED: AtomicBool = AtomicBool::new(false);

    /// 从 env 取就绪 fd（run 入口调一次；无 env = 非 daemon 子进程）。
    pub fn init_from_env() {
        if let Some(fd) = std::env::var_os(super::OJ_DAEMON_READY_FD)
            .and_then(|s| s.to_str().and_then(|s| s.parse::<libc::c_int>().ok()))
            .filter(|fd| *fd > 2)
        {
            let _ = FD.set(fd);
        }
    }

    /// 启动成功落定（监听已绑定）：回传 `ok`。仅首次生效。
    pub fn ok() {
        write_once(b"ok");
    }

    /// 启动失败：回传真因。仅首次生效（ok 之后的运行期错误不再回传）。
    pub fn fail(msg: &str) {
        write_once(msg.as_bytes());
    }

    fn write_once(bytes: &[u8]) {
        let Some(fd) = FD.get() else { return };
        if SIGNALED.swap(true, Ordering::SeqCst) {
            return;
        }
        // Safety: fd 来自 env、本进程独享；单次 write + close。
        unsafe {
            libc::write(*fd, bytes.as_ptr().cast(), bytes.len());
            libc::close(*fd);
        }
    }
}

/// 非 unix：无就绪管道（windows daemon 化未接回传），全部 no-op。
#[cfg(not(unix))]
mod daemon_ready {
    pub fn init_from_env() {}
    pub fn ok() {}
    pub fn fail(_msg: &str) {}
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
    println!("oj serve daemonized (pid {})", child.id());
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn daemonize() -> Result<(), String> {
    Err("--daemon 仅支持 unix / windows 平台".to_string())
}

/// 停机信号（spec §6 ①）：SIGINT（ctrl_c）与 SIGTERM 二选一（Windows 仅 ctrl_c）；
/// 命中即置停机 flag（任务循环检测退出；HTTP 侧随之排空）。
/// `flag` 是当前任务池停机 flag 的「运行时指向」：热更后 `do_reload` 会把它换成
/// 新一代池的 flag，故 SIGTERM 总是精确停机「在跑的那一代」池（spec §6 ⑤）。
async fn shutdown_signal(flag: Arc<tokio::sync::Mutex<Arc<std::sync::atomic::AtomicBool>>>) {
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
    flag.lock()
        .await
        .store(true, std::sync::atomic::Ordering::Relaxed);
}

/// 解析配置文件（v0.1.58 自 load_app_config 抽出）：`config` None 走统一搜索
/// （CWD 逐级向上 config.yaml，兜底 $HOME/.oj/config.yaml），未找到不报错——回落
/// 内置默认 Config（config_dir = CWD），纯静态 serve / 无后端 exec/test 等场景无需配置文件。
/// 返回附带 `top`（顶层全量段 JSON 形，已知 + 未知）：插件自报 config key 的 cfg 查找面
/// （撞宿主已知段名合法，spec Part 3；load_with_extra）。
pub fn load_cfg(
    config: Option<&str>,
    note_on_default: bool,
) -> Result<(Config, serde_json::Value, PathBuf), String> {
    match config::find_path(config)? {
        Some(path) => {
            let config_dir = config_dir_of(&path);
            let LoadedConfig { config, top } = config::load_with_extra(&path, &config_dir)
                .map_err(|e| format!("load config: {e}"))?;
            Ok((config, top, config_dir))
        }
        None => {
            if note_on_default {
                eprintln!(
                    "note: no config.yaml found (searched cwd upward, $HOME/.oj/config.yaml) — \
                     using built-in defaults"
                );
            }
            Ok((
                Config::default(),
                serde_json::Value::Object(Default::default()),
                std::env::current_dir().map_err(|e| format!("resolve cwd: {e}"))?,
            ))
        }
    }
}

/// 解析配置 + 服务目录 / api 前缀（v0.1.58 挂载模型）。server 折叠 CLI 挂载后自解析
/// （见 `fold_cli_mounts`）；test/migrate/exec/openapi 共用本函数，取值优先级：
/// - `--site <prefix>`：命中 api 挂载 → **只留这一条**（选树语义：迁移/路由都只对它；
///   折叠写回 `cfg.mounts`——下游 `App::from_config` 只看挂载）；命中 web 挂载 → Err
///   并列出全部 api prefix 供选；与 `-d`/`-b` 同给 → Err（互斥，不留含糊面）。
/// - 无 --site：显式 `-d`（按 CWD 绝对化，覆盖第一条 api 挂载的目录）> 第一条 api
///   挂载（目录已按 config_dir 解析）> 缺省搜索（自 config 同级逐级向上，每层 src
///   优先、dist 次之）。`-b` 改写第一条 api 挂载的前缀（无挂载 → 以该前缀新增）。
/// - ts = 目录即模式（`is_release`）。目录缺失不在此拦截：test/migrate 强依赖 api
///   目录、各自就地报错；from_config 对缺失目录全程容忍（模块扫描 NotFound = 空）。
/// - `--module-root`（v0.1.85）与 `-d` 同为「源码树根」，二者互斥；给了即在子树内
///   **递归**发现模块（模块名取 `manifest.yaml` 的 name），索引随结果一并返回。
pub fn load_app_config(
    config: Option<&str>,
    site: Option<&str>,
    dir_override: Option<&str>,
    base_override: Option<&str>,
    note_on_default: bool,
    module_root: Option<&str>,
) -> Result<AppConfig, String> {
    let (mut cfg, top, config_dir) = load_cfg(config, note_on_default)?;
    if dir_override.is_some() && module_root.is_some() {
        return Err(
            "--module-root and -d/--dir are mutually exclusive (both mean the source tree root)"
                .into(),
        );
    }
    if site.is_some() {
        if dir_override.is_some() || module_root.is_some() {
            return Err("--site and -d/--dir/--module-root are mutually exclusive".into());
        }
        if base_override.is_some() {
            return Err("--site and -b/--base are mutually exclusive".into());
        }
    }
    let resolved = crate::app::resolve_mounts(&cfg, &config_dir)?;
    let primary = resolved.iter().find_map(|m| {
        m.kind
            .api()
            .map(|(d, t)| (d.to_path_buf(), t, m.prefix.clone()))
    });
    // --site：选树。只保留选中的 api 挂载（web 挂载对 test/migrate/exec/openapi 无意义）。
    if let Some(s) = site {
        let want = resolve_app_prefix(s).map_err(|e| format!("--site: {e}"))?;
        let Some((dir, ts, prefix)) = resolved.iter().find_map(|m| {
            (m.prefix == want)
                .then_some(m.kind.api())
                .flatten()
                .map(|(d, t)| (d.to_path_buf(), t, m.prefix.clone()))
        }) else {
            let api_prefixes: Vec<&str> = resolved
                .iter()
                .filter(|m| m.kind.api().is_some())
                .map(|m| m.prefix.as_str())
                .collect();
            return Err(if resolved.iter().any(|m| m.prefix == want) {
                format!(
                    "--site {s:?} is a web mount — pick an api mount (available api prefixes: {api_prefixes:?})"
                )
            } else {
                format!("--site {s:?} not found (available api prefixes: {api_prefixes:?})")
            });
        };
        let picked = cfg
            .mounts
            .iter()
            .find(|m| m.api.is_some() && resolve_app_prefix(&m.prefix).is_ok_and(|x| x == prefix))
            .cloned();
        if let Some(m) = picked {
            cfg.mounts = vec![m];
        }
        return Ok(AppConfig {
            cfg,
            top,
            config_dir,
            dir,
            ts,
            base: prefix,
            module_index: None,
        });
    }
    // -d / --module-root：覆盖第一条 api 挂载的目录（无挂载 → 以默认前缀新增一条）。
    // 二者 clap 层已互斥，此处 `.or()` 只为「同一个语义、一个入口」——下游只认 dir。
    let dir = match dir_override.or(module_root) {
        Some(d) => {
            let p = absolutize_cwd(
                &std::env::current_dir().map_err(|e| format!("resolve cwd: {e}"))?,
                d,
            );
            match cfg.mounts.iter().position(|m| m.api.is_some()) {
                Some(i) => cfg.mounts[i].api = Some(p),
                None => cfg.mounts.push(config::MountConf {
                    prefix: base_override.unwrap_or(DEFAULT_API_PREFIX).to_string(),
                    api: Some(p),
                    web: None,
                    spa: None,
                    headers: Default::default(),

                    ..Default::default()
                }),
            }
            PathBuf::from(d)
        }
        None => match &primary {
            Some((d, _, _)) => d.clone(),
            // 无任何 api 挂载：兜底搜索（自 config 同级/CWD 向上，只认「像业务树」的
            // 候选——首层 */manifest.yaml、manifests.yaml 或空树；源码仓自己的 src/
            // 由此跳过）。无合格候选 → 占位路径（必然不存在，discover = 空 =
            // 零模块零路由），沿用纯静态 serve 的 `.oj-static-only` 同款模式：
            // exec 照常跑（纯脚本不需要表白名单，stderr 有 note），test/migrate/
            // schema diff 由各自的 dir 存在性检查给出明确报错。
            None => match search_api_dir(&config_dir) {
                Some(d) => d,
                None => {
                    let fallback = config_dir.join(".oj-no-api");
                    eprintln!(
                        "note: no plausible api dir found (searched upward from {} for a \
                         src/dist containing first-level */manifest.yaml, manifests.yaml, or \
                         an empty dir) — using {}; pass -d <dir> or -c a config with api mounts",
                        config_dir.display(),
                        fallback.display()
                    );
                    fallback
                }
            },
        },
    };
    // -b：改写第一条 api 挂载的前缀（无挂载 → 以该前缀新增一条指向解析出的目录）。
    let base = match base_override {
        Some(b) => {
            let p = resolve_app_prefix(b).map_err(|e| format!("--base: {e}"))?;
            match cfg.mounts.iter().position(|m| m.api.is_some()) {
                Some(i) => cfg.mounts[i].prefix = p.clone(),
                None => cfg.mounts.push(config::MountConf {
                    prefix: p.clone(),
                    api: Some(dir.to_string_lossy().into_owned()),
                    web: None,
                    spa: None,
                    headers: Default::default(),

                    ..Default::default()
                }),
            }
            p
        }
        None => primary
            .as_ref()
            .map(|(_, _, p)| p.clone())
            .unwrap_or_else(|| DEFAULT_API_PREFIX.to_string()),
    };
    let ts = !is_release(&dir);
    // --module-root：递归发现模块索引（模块名取 manifest.yaml 的 name，目录名不参与身份）。
    // 产物树（release）没有「模块根」概念——静默忽略会让下游以为生效了，故 fail-fast。
    let module_index = match module_root {
        None => None,
        Some(_) if !ts => {
            return Err(format!(
                "--module-root is a source-tree option: {} is a release tree (contains dist manifests.yaml) — point it at the src tree, or drop --module-root",
                dir.display()
            ));
        }
        Some(mr) => {
            let cwd = std::env::current_dir().map_err(|e| format!("resolve cwd: {e}"))?;
            let abs = PathBuf::from(absolutize_cwd(&cwd, mr));
            Some(Arc::new(only_js::bridge::resolve_roots(&abs, true, None)?))
        }
    };
    Ok(AppConfig {
        cfg,
        top,
        config_dir,
        dir,
        ts,
        base,
        module_index,
    })
}

/// `load_app_config` 的返回（v0.1.85：6 元组改为具名结构——加了模块索引后元组已
/// 长到读不出字段含义）。
pub struct AppConfig {
    pub cfg: Config,
    /// 顶层全量段 JSON（插件自报 config key 的查找面）。
    pub top: serde_json::Value,
    pub config_dir: PathBuf,
    /// api 树根（源码树 src / 产物树 dist）。
    pub dir: PathBuf,
    /// dev（ts）模式。
    pub ts: bool,
    /// URL 前缀（base）。
    pub base: String,
    /// 模块索引（`--module-root` 的递归发现结果）；None = 今日首层语义。
    pub module_index: Option<Arc<ModuleIndex>>,
}

/// 缺省 api 目录搜索（无 `-d`、无 api 挂载时的兜底）：自 config 同级（无 config 时
/// = CWD）起步逐级向上，每层 src 优先、dist 次之。候选须「像业务树」才被接受——
/// ≥1 个首层 `*/manifest.yaml`（dev 模块树）、含 `manifests.yaml`（release 产物锁）、
/// 或**无任何首层子目录**（空树：fresh 项目合法，discover = 零模块）；否则继续向上
/// ——本仓这类「祖先有非业务 `src/`」（如 Rust 源码树）由此跳过，不再让裸 `oj exec`
/// 在源码仓根爆出 `module 'bridge' missing manifest.yaml`（2026-10-10 用户实测）。
/// 全程无合格候选 → None（调用方报 service dir not found，不再回落编造的
/// `config_dir/src`——静默猜进错误的树然后把 discover 错误甩给用户，才是坑）。
fn search_api_dir(config_dir: &Path) -> Option<PathBuf> {
    let plausible = |d: &Path| -> bool {
        let Ok(entries) = std::fs::read_dir(d) else {
            return false;
        };
        let mut has_subdir = false;
        for e in entries.flatten() {
            if e.path().is_dir() {
                has_subdir = true;
                if e.path().join("manifest.yaml").is_file() {
                    return true; // dev 模块树
                }
            }
        }
        if !has_subdir {
            return true; // 空树（无首层子目录）：fresh 项目
        }
        d.join("manifests.yaml").is_file() // release 锁
    };
    let mut cur = Some(config_dir);
    while let Some(d) = cur {
        let src = d.join("src");
        if src.is_dir() && plausible(&src) {
            return Some(src);
        }
        let dist = d.join("dist");
        if dist.is_dir() && plausible(&dist) {
            return Some(dist);
        }
        cur = d.parent();
    }
    None
}

/// server 准入门（v0.1.58 规则 5）：mounts（config + CLI 折叠后）为空 → Err 提示。
/// 目录存在性不在此判——装配期 resolve_mounts 统一 fail-fast（含 prefix/条目序号）。
fn admission_gate(cfg: &Config) -> Result<(), String> {
    if cfg.mounts.is_empty() {
        return Err(
            "no mounts configured — add a top-level `mounts:` entry (one line = one URL \
             prefix + one api/web directory) or pass --api-path/--app-path"
                .to_string(),
        );
    }
    Ok(())
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

/// CLI `--app-path` 折叠进 mounts（v0.1.58，可重复；语义沿用 v0.1.27 的两形态）：
/// - 裸 `dir`（至多一次）→ upsert `{prefix: "/", web: dir}`，按 CWD 预绝对化；
/// - `prefix=dir` → 同前缀 **web** 条目替换（保留 headers/spa）、否则追加（CLI 优先）。
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
                upsert_web(cfg, &p, absolutize_cwd(&cwd, dir));
            }
        }
    }
    if let Some(dir) = bare {
        upsert_web(cfg, "/", absolutize_cwd(&cwd, dir));
    }
    Ok(())
}

/// 同前缀 web 条目替换（保留 headers/spa——CLI 只覆盖目录）、否则追加（规范化口径：
/// config 侧可能带尾斜杠等未规范形态）。
fn upsert_web(cfg: &mut Config, prefix: &str, dir: String) {
    let idx = cfg
        .mounts
        .iter()
        .position(|m| m.web.is_some() && resolve_app_prefix(&m.prefix).is_ok_and(|x| x == prefix));
    match idx {
        Some(i) => {
            cfg.mounts[i].web = Some(dir);
            cfg.mounts[i].prefix = prefix.to_string();
        }
        None => cfg.mounts.push(config::MountConf {
            prefix: prefix.to_string(),
            api: None,
            web: Some(dir),
            spa: None,
            headers: Default::default(),

            ..Default::default()
        }),
    }
}

/// 静态挂载前缀归一（挂载 `prefix`，`/` 允许）：必须以 `/` 开头；尾斜杠剪除；
/// 剪完为空 → "/"（根）。非法（不以 `/` 开头）→ Err fail-fast。
pub fn resolve_app_prefix(cfg: &str) -> Result<String, String> {
    if !cfg.starts_with('/') {
        return Err(format!("mount prefix must start with '/': {cfg:?}"));
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
pub fn is_release(dir: &Path) -> bool {
    dir.join("manifests.yaml").is_file()
}

/// 装配并监听（port=0 → 随机端口，测试用）。维持旧签名（返回 (SocketAddr, JoinHandle)），
/// 现有 `cargo test` 端口 0 + reqwest 用例零改动——内部把 `(base, dir)` 折成一条 api
/// 挂载后委托 `App::from_config` + `App::serve`（ts 由目录内容自动判定）。
pub async fn start(
    mut cfg: Config,
    config_dir: &Path,
    dir: PathBuf,
    base: String,
) -> Result<(SocketAddr, tokio::task::JoinHandle<()>), String> {
    let addr = to_socket_addrs_sync(&format!("{}:{}", cfg.server.host, cfg.server.port))?;
    cfg.mounts.push(config::MountConf {
        prefix: base,
        api: Some(dir.to_string_lossy().into_owned()),
        web: None,
        spa: None,
        headers: Default::default(),

        ..Default::default()
    });
    // top 传 Null：测试入口不依赖顶层未知段（config key 解析），保持旧签名零改动。
    // 槽位 None：cargo test 环境性入口无热更执行器（fail-closed）。
    let app = App::from_config(
        cfg,
        &serde_json::Value::Null,
        config_dir,
        false,
        &ResourceProfiles::default(),
        None,
        None,
        None,
    )
    .await?;
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
    blob_profile: Option<&str>,
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

    // CLI `--blob <profile>`：将指定命名 profile 设为字面 "default" 的解析目标。
    // 仅当注册表非空（已声明 blob 后端）且 profile 存在时生效；选错直接 fail-fast。
    if let Some(p) = blob_profile {
        if !r.names().is_empty() {
            if r.get(p).is_none() {
                return Err(format!(
                    "blob profile '{p}' not declared (available: {:?})",
                    r.names()
                ));
            }
            r.set_default_alias(p).map_err(|e| format!("blob: {e}"))?;
        }
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
    /// ldap 键选单 vtable 槽（ldap 轴）：ldap: 段 + 恰一个 ldap 插件
    /// → 装配层构造 `FfiLdapBackend`（`app::build_ldap_backend`）；多 ldap 插件注册冲突
    /// fail fast。未加载插件时保持 None（`ldap.*` 报未配置，不降级）。
    pub ldap: Option<&'static oj_plugin_ffi::LdapVtable>,
    /// 泛型轴（插件自报的非类型化轴，不在 plugin_loader::TYPED_AXES 内）：
    /// (轴名, 插件名, vtable)；跨插件同名冲突在 build_registries fail fast。
    pub generic: Vec<(String, String, &'static oj_plugin_ffi::GenericVtable)>,
    /// U-15：`mgmt.axes` 收编的管理面泛型轴（**不进**公开 `generic`——特权旁路封堵）。
    pub mgmt_generic: Vec<(String, String, &'static oj_plugin_ffi::GenericVtable)>,
}

/// 装配层把宿主侧解析出的跨后端参数经 cfg JSON 注入插件（spec §3 有意的边界；
/// cfg 按值传入，插件须持久化时自行持有副本）。
/// 第一方轴适配器清单（实际适配分支在 plugin_cfg 的 match；此表供测试与
/// plugin_loader::AXES 对账，防止两表失步——适配器打空）。加新第一方轴时在此登记。
#[cfg(test)]
const ADAPTER_AXES: &[&str] = &["es", "auth", "mail", "ldap"];

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
/// A5-ldap：`plugins.ldap` 透传与顶层 `ldap:` 段皆非空 → 配置错误（透传静默胜出，
/// 运维改 `ldap:` 会「改了不生效」）。判定与 `plugin_cfg` 选用条件对齐：
/// `plugins.ldap` = 非空对象；`ldap:` 段 = Some 且非空 map。
pub(crate) fn check_ldap_cfg_sources(cfg: &Config) -> Result<(), String> {
    let passthrough = cfg
        .plugins
        .get("ldap")
        .is_some_and(|v| v.as_object().is_some_and(|o| !o.is_empty()));
    let section_nonempty = cfg
        .ldap
        .as_ref()
        .is_some_and(|v| v.as_mapping().is_some_and(|m| !m.is_empty()));
    if passthrough && section_nonempty {
        return Err(
            "config declares both a non-empty `ldap:` section and a non-empty `plugins.ldap` \
             entry: pick one (`ldap:` is the standard form; `plugins.ldap` is a raw passthrough \
             that silently wins over `ldap:` and bypasses its typed parsing)"
                .to_string(),
        );
    }
    Ok(())
}

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
/// `es_profile`：CLI `--es <profile>` 选定的 profile（None → 默认 `default`）。仅 "es" 分支使用，
/// 用于从命名 map `cfg.es` 挑 endpoint 发给插件 init；给定 profile 不存在 → 返回错误串（调用方 fail-fast）。
pub(crate) fn plugin_cfg(
    cfg: &Config,
    top: &serde_json::Value,
    name: &str,
    config_key: Option<&str>,
    es_profile: Option<&str>,
) -> Result<String, String> {
    if let Some(v) = cfg.plugins.get(name)
        && v.as_object().is_some_and(|o| !o.is_empty())
    {
        return Ok(v.to_string());
    }
    // 第 2 级：插件自报 config key（oj_plugin_config_key，T3 探测）→ 全量顶层 Value
    // 查找（含宿主已知段——段是读不是占；撞已知名是合法场景，评审 H2 的静默 {} 洞
    // 由此消解；未声明该段的插件仍走下面按名遗留臂 es/auth/mail/ldap）。
    if let Some(k) = config_key {
        if let Some(v) = top.get(k) {
            return serde_json::to_string(v).map_err(|e| format!("plugin cfg section '{k}': {e}"));
        }
        return Ok("{}".to_string()); // 段可选是既有语义
    }
    match name {
        "es" => {
            let key = es_profile.unwrap_or("default");
            match cfg.es.get(key) {
                Some(es) => Ok(serde_json::json!({ "endpoint": es.endpoint }).to_string()),
                None => {
                    if es_profile.is_some() {
                        let mut keys: Vec<&str> = cfg.es.keys().map(|s| s.as_str()).collect();
                        keys.sort_unstable();
                        return Err(format!(
                            "--es profile '{key}' not declared (available es profiles: {keys:?})"
                        ));
                    }
                    Ok("{}".to_string())
                }
            }
        }
        "auth" => match &cfg.auth {
            Some(a) => {
                let mut v = serde_json::json!({
                    "jwt_secret": a.jwt_secret,
                    "signing_method": a.signing_method,
                    // 插件只吃路径字符串（v0.1.23 起宿主支持条目对象形态，插件侧零改动）。
                    "anonymous_paths": only_js::config::anon_paths(&a.anonymous_paths),
                });
                // oj-4：cookie 会话形态原样透传（schema 归插件；缺省不带该键 = 关闭）。
                if let Some(c) = &a.cookie {
                    v["cookie"] = c.clone();
                }
                Ok(v.to_string())
            }
            None => Ok("{}".to_string()),
        },
        // mail（spec 2026-09-15）：顶层 `smtp:` 段 → oj-mail 插件 cfg。
        // 段缺省/空 → "{}"（插件零 profile；装配层视作未配置，不挂 mail 后端）。
        "mail" => match &cfg.smtp {
            Some(s) => Ok(serde_json::to_string(s).unwrap_or_else(|_| "{}".to_string())),
            None => Ok("{}".to_string()),
        },
        // ldap：顶层 `ldap:` 段（不透明 map）→ oj-ldap 插件 cfg。
        // 段缺省/空 → "{}"（插件零实例；装配层视作未配置，不挂 ldap 后端）。
        "ldap" => match &cfg.ldap {
            Some(l) => Ok(serde_json::to_string(l).unwrap_or_else(|_| "{}".to_string())),
            None => Ok("{}".to_string()),
        },
        _ => Ok("{}".to_string()),
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
    if !cfg.es.is_empty() && es_plugins.is_empty() {
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
        if let Some(be) = db_backend(p).map_err(|e| format!("plugins db assembly: {e}"))? {
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
    // ldap 键选式单 vtable 槽：多 ldap 插件注册冲突 fail fast；
    // `ldap:` 段声明但插件未装 → 不在装配期硬失败（`app::build_ldap_backend` 返回 None，
    // `ldap.*` 调用报 "ldap not configured"，与 mail 同语义）。
    let ldap_plugins: Vec<&LoadedPlugin> = loaded
        .iter()
        .filter(|p| p.registrations.ldap.is_some())
        .collect();
    if ldap_plugins.len() > 1 {
        return Err("plugins conflict: multiple plugins register ldap backend".to_string());
    }
    let ldap = ldap_plugins.first().and_then(|p| p.registrations.ldap);
    // 泛型轴：跨插件轴名冲突 fail-fast（与「不静默跳过」哲学一致）。
    // U-15：`mgmt.axes` 收编的轴**不进** `axis()` 公开面（特权旁路封堵，评审阻断①），
    // 单独归集给管理面注册表（`StableState.mgmt_axes`）。
    let mgmt_claimed = &cfg.mgmt.axes;
    let mut seen: HashMap<&str, &str> = HashMap::new();
    let mut generic = Vec::new();
    let mut mgmt_generic = Vec::new();
    for p in loaded {
        for (axis, vt) in &p.generic_axes {
            if let Some(prev) = seen.insert(axis, &p.descriptor.name[..]) {
                return Err(format!(
                    "plugins conflict: generic axis '{axis}' provided by both '{prev}' and '{}' \
                     (generic axes are single-provider per axis)",
                    &p.descriptor.name[..]
                ));
            }
            if mgmt_claimed.contains(axis) {
                mgmt_generic.push((axis.clone(), p.descriptor.name[..].to_string(), *vt));
            } else {
                generic.push((axis.clone(), p.descriptor.name[..].to_string(), *vt));
            }
        }
    }
    Ok(Registries {
        es,
        dbs,
        blob,
        bus,
        kv,
        auth,
        mq,
        mail,
        ldap,
        generic,
        mgmt_generic,
    })
}

/// 装配后顶层 Value 中未被任何已加载插件消费的段（config key 或 plugins:<name> 非空）。
/// top 是全量 mapping，但宿主已知段恒不算 unconsumed（host 自己消费）——只面向未知段。
pub(crate) fn unconsumed_sections(
    top: &serde_json::Value,
    cfg: &Config,
    loaded: &[LoadedPlugin],
) -> Vec<String> {
    let consumed: std::collections::HashSet<&str> = loaded
        .iter()
        .filter_map(|p| p.config_key.as_deref())
        .chain(
            cfg.plugins
                .iter()
                .filter(|(_, v)| v.as_object().is_some_and(|o| !o.is_empty()))
                .map(|(k, _)| k.as_str()),
        )
        .collect();
    top.as_object()
        .map(|m| {
            m.keys()
                .filter(|k| {
                    !config::known_top_level_keys().contains(&k.as_str()) // 宿主已知段不算
                        && !consumed.contains(k.as_str())
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// spec §5 全流程：解析 plugins_dir → 严格（plugins map 键）/缺省扫描 → 逐个加载校验
/// → 身份核对 → semver 对照 → 注册（插件先于内置）→ 内置后端注册。
/// 空 map/目录不存在/为空 → 扫描模式零插件（仅内置后端，不报错，除非 §2 闸门触发）。
///
/// 返回 (自省清单, loaded 插件体)：loaded 供 `assemble_ojinfo` 取 config_key
/// （unconsumed 段诊断）——JS ojInfo() 与 CLI 共用同一装配面。
pub async fn assemble_plugins(
    cfg: &Config,
    top: &serde_json::Value,
    config_dir: &Path,
    registries: &mut Registries,
    es_profile: Option<&str>,
) -> Result<(Vec<PluginInfo>, Vec<LoadedPlugin>), String> {
    // A5：mail 双配置源（`smtp:` 与 `plugins.mail` 皆非空）是配置错误 → 装配期 fail-fast
    // （`plugin_cfg` 会让非空 `plugins.mail` 静默胜出，运维改 `smtp:` 会「改了不生效」）。
    check_mail_cfg_sources(cfg)?;
    check_ldap_cfg_sources(cfg)?;
    // es profile 预校验：给定了 `--es` 但 `cfg.es` 无此 profile → fail-fast（列出可用 profile）。
    if let Some(p) = es_profile {
        if !cfg.es.contains_key(p) {
            let mut keys: Vec<&str> = cfg.es.keys().map(|s| s.as_str()).collect();
            keys.sort_unstable();
            return Err(format!(
                "--es profile '{p}' not declared (available es profiles: {keys:?})"
            ));
        }
    }
    let dir = resolve_plugins_dir(config_dir, cfg.plugins_dir.as_deref())
        .map_err(|e| format!("plugins dir: {e}"))?;
    let host = host_context();
    // 三级解析：passthrough（plugins:<name> 非空）→ 插件自报 config key 查全量顶层段
    // → 按名遗留臂（es/auth/mail/ldap）。config_key 由 load_one init 前探测（T3）传入。
    let cfg_for = |name: &str, config_key: Option<&str>| -> String {
        plugin_cfg(cfg, top, name, config_key, es_profile)
            .unwrap_or_else(|e| panic!("plugin_cfg: {e}"))
    };
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
    let unused = unconsumed_sections(top, cfg, &loaded);
    if !unused.is_empty() {
        eprintln!("[oj-serve] unconsumed config sections: {unused:?} (typo? or plugin not loaded)");
    }
    // T13 启动能力位一览：宿主 ABI + 每插件 abi/fingerprint/caps（caps 仅 db 轴插件有）。
    eprintln!(
        "[oj-serve] host ABI_VERSION = {}",
        oj_plugin_ffi::ABI_VERSION
    );
    for p in &loaded {
        eprintln!("{}", plugin_banner_line(p));
    }
    Ok((loaded.iter().map(PluginInfo::from).collect(), loaded))
}

/// T13：单个插件的启动横幅行。抽成独立函数以便单测断言 caps 串（无需真装配）。
fn plugin_banner_line(p: &only_js::bridge::plugin_loader::LoadedPlugin) -> String {
    let caps = p
        .caps
        .as_ref()
        .map(|c| format!(" | caps: {}", c.summary()))
        .unwrap_or_default();
    format!(
        "[oj-serve] plugin {} abi={} fp={}{}",
        &p.descriptor.name[..],
        p.descriptor.abi_version,
        &p.descriptor.fingerprint[..],
        caps
    )
}

/// oj_info（phpinfo 对应物）单一事实源。两出口（CLI / JS ojInfo()）共用。
/// 只报告声明面：不 connect 库/broker；config 只出段名/键名，值一律不出。
#[derive(serde::Serialize)]
pub struct OjInfo {
    pub build: serde_json::Value,
    pub abi: serde_json::Value,
    pub plugins: Vec<PluginInfo>,
    pub backends: serde_json::Value,
    pub config: serde_json::Value,
    pub generic_axes: Vec<String>,
    pub unconsumed_sections: Vec<String>,
}

impl OjInfo {
    /// php -i 风格分段纯文本。
    pub fn to_text(&self) -> String {
        let mut o =
            String::from("oj info — declaration surface only (no connections, no config values)\n");
        let sec = |o: &mut String, t: &str| {
            o.push_str("\n## ");
            o.push_str(t);
            o.push('\n');
        };
        sec(&mut o, "build");
        if let Some(m) = self.build.as_object() {
            for (k, v) in m {
                line(&mut o, k, v);
            }
        }
        sec(&mut o, "abi");
        if let Some(m) = self.abi.as_object() {
            for (k, v) in m {
                line(&mut o, k, v);
            }
        }
        sec(&mut o, "plugins");
        if self.plugins.is_empty() {
            o.push_str("(none loaded)\n");
        }
        for p in &self.plugins {
            o.push_str(&format!(
                "- {} {} (abi {})\n  desc: {}\n",
                p.name, p.semver, p.abi_version, p.description
            ));
            // db 轴插件的方言能力位与启动横幅同口径（T13）：两个出口都看得见，
            // 否则「横幅有、info 没有」会让你在排障时用错工具。
            if let Some(c) = &p.caps {
                o.push_str(&format!("  caps: {}\n", c.summary()));
            }
        }
        line(
            &mut o,
            "generic_axes",
            &serde_json::json!(self.generic_axes),
        );
        sec(&mut o, "backends");
        if let Some(m) = self.backends.as_object() {
            for (k, v) in m {
                line(&mut o, k, v);
            }
        }
        sec(&mut o, "config");
        line(&mut o, "sections", &self.config["sections"]);
        line(&mut o, "unconsumed", &self.config["unconsumed"]);
        o
    }
}

/// `key: value` 一行；字符串值原样出，其余按 JSON 出。
fn line(o: &mut String, k: &str, v: &serde_json::Value) {
    o.push_str(k);
    o.push_str(": ");
    if let Some(s) = v.as_str() {
        o.push_str(s);
    } else {
        o.push_str(&serde_json::to_string(v).unwrap_or_default());
    }
    o.push('\n');
}

/// 顶层 config 段键排序清单（只出键名不出值）。
fn known_section_keys(top: &serde_json::Value) -> Vec<String> {
    let mut keys: Vec<String> = top
        .as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    keys.sort();
    keys
}

/// V8 版本（经 deno_core re-export；独立小函数便于测试替换）。
fn v8_version() -> String {
    deno_core::v8::V8::get_version().to_string()
}

/// 宿主 triple（与 core `ffi::triple()` 同口径——该 fn 为 pub(crate)，此处按
/// `std::env::consts` 重建，与 serve_cmd tests / migrate_cmd 的既有 helper 一致）。
fn host_triple_str() -> String {
    let arch = std::env::consts::ARCH;
    match std::env::consts::OS {
        "macos" => format!("{arch}-apple-darwin"),
        "windows" => format!("{arch}-pc-windows-msvc"),
        "linux" => format!("{arch}-unknown-linux-gnu"),
        other => format!("{arch}-unknown-{other}-gnu"),
    }
}

/// `oj info` 与 JS ojInfo() 的共同装配体（单一事实源，两出口共用）：
/// backends 声明面 + config 段名 + build/abi + 插件清单 + 泛型轴名。
/// `config_path` None（serve 测试入口）时 build 不含 config_path 字段。
/// **只报告声明面**：不 connect 库/broker；config 只出段名/键名，值一律不出。
pub(crate) fn assemble_ojinfo(
    cfg: &Config,
    top: &serde_json::Value,
    loaded: &[LoadedPlugin],
    reg: &Registries,
    config_path: Option<&str>,
) -> OjInfo {
    // backends 声明面：db schemes、blob 有/无、broker kinds、kv/auth/es/mail/ldap/mq 槽位。
    let backends = serde_json::json!({
        "db_schemes": { "declared": cfg.db.keys().collect::<Vec<_>>() },
        "blob_configured": cfg.blob.is_some(),
        "kv_plugin": reg.kv.is_some(),
        "auth_plugin": reg.auth.is_some(),
        "mail_plugin": reg.mail.is_some(),
        "ldap_plugin": reg.ldap.is_some(),
        "es_plugin": reg.es.is_some(),
        "mq_plugins": reg.mq.iter().map(|(n, _)| n).collect::<Vec<_>>(),
        "bus_kinds": reg.bus.kinds(),
        "dbs_registered": reg.dbs.backend_names(),
    });
    let unconsumed = unconsumed_sections(top, cfg, loaded);
    let config = serde_json::json!({
        "sections": known_section_keys(top),
        "unconsumed": unconsumed,
    });
    let mut build = serde_json::json!({
        "oj": env!("CARGO_PKG_VERSION"),
        "profile": if cfg!(debug_assertions) { "debug" } else { "release" },
        "host_triple": host_triple_str(),
        "v8": v8_version(),
        "exe": std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default(),
    });
    if let Some(p) = config_path
        && let Some(m) = build.as_object_mut()
    {
        m.insert("config_path".into(), p.into());
    }
    OjInfo {
        build,
        abi: serde_json::json!({
            "abi_version": oj_plugin_ffi::ABI_VERSION,
            "host_fingerprint": oj_plugin_ffi::HOST_FINGERPRINT,
        }),
        plugins: loaded.iter().map(PluginInfo::from).collect(),
        backends,
        config,
        generic_axes: reg.generic.iter().map(|(a, _, _)| a.clone()).collect(),
        unconsumed_sections: unconsumed,
    }
}

/// `oj info` 与 JS ojInfo() 的共同装配面：load_with_extra → resolve_plugins_dir →
/// assemble_plugins 的加载与 config 解析路径 → build_registries；不 connect、不监听。
/// 副作用 = 执行插件 init 代码，信任边界同 serve。
pub async fn assemble_for_info(cfg_path: Option<&str>) -> Result<OjInfo, String> {
    // 与 load_app_config 同一搜索口径；未找到 → 默认 Config + 空 top（config 段显示为空）。
    let (loaded_cfg, config_dir) = match config::find_path(cfg_path)? {
        Some(path) => {
            let config_dir = config_dir_of(&path);
            let loaded =
                config::load_with_extra(&path, &config_dir).map_err(|e| format!("config: {e}"))?;
            (loaded, config_dir)
        }
        None => (
            LoadedConfig {
                config: Config::default(),
                top: serde_json::Value::Object(Default::default()),
            },
            std::env::current_dir().map_err(|e| format!("resolve cwd: {e}"))?,
        ),
    };
    let cfg = &loaded_cfg.config;
    let dir = resolve_plugins_dir(&config_dir, cfg.plugins_dir.as_deref())
        .map_err(|e| format!("plugins dir: {e}"))?;
    let host = host_context();
    // 与 assemble_plugins 同一取值路径（三级解析：passthrough → config key → 按名臂）。
    let cfg_for = |name: &str, config_key: Option<&str>| -> String {
        plugin_cfg(cfg, &loaded_cfg.top, name, config_key, None)
            .unwrap_or_else(|e| panic!("plugin_cfg: {e}"))
    };
    let loaded = match &dir {
        Some(d) if !cfg.plugins.is_empty() => {
            let mut entries: Vec<PluginManifestEntry> = cfg
                .plugins
                .keys()
                .map(|name| PluginManifestEntry {
                    name: name.clone(),
                    semver_pin: None,
                })
                .collect();
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            load_manifest(d, &entries, host, &cfg_for)
                .map_err(|e| format!("plugins manifest: {e}"))?
        }
        Some(d) => load_scanned(d, host, &cfg_for).map_err(|e| format!("plugins scan: {e}"))?,
        None => vec![],
    };
    let reg = build_registries(cfg, &loaded)?;
    Ok(assemble_ojinfo(
        cfg,
        &loaded_cfg.top,
        &loaded,
        &reg,
        cfg_path,
    ))
}

#[cfg(test)]
#[path = "serve_cmd_tests.rs"]
mod tests;
