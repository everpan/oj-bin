//! 长任务池监督器（spec 2026-09-07 §6）：扫描 task_{name}.* / {name}_task.* 约定文件，
//! 每任务一条专用 OS 线程 + current_thread runtime + 独立 Bridge（tasks_flag 注入），
//! 异常收场指数退避重启（1s→2s→4s…cap 60s，成功运行 ≥60s 归零），停机 flag 置位后
//! run_task 的 grace + 看门狗保证线程在宽限内收场，shutdown 顺序 join。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use only_js::bridge::TaskExit;
use only_js::config::TasksCfg;

/// 扫描任务池目录（评审用户裁决的命名约定，spec §6：递归扫描）：
/// `task_{name}.{ts,js}` / `{name}_task.{ts,js}` → (name, path)；其余文件忽略
/// （任务项目共享库，含子目录）。同名双写（task_x + x_task）→ Err；数量超
/// max → Err。结果按 name 排序（启动顺序确定）。
pub fn scan_tasks(dir: &Path, max: usize) -> Result<Vec<(String, PathBuf)>, String> {
    let mut files = Vec::new();
    match walk_ts_js(dir, &mut files) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("scan {}: {e}", dir.display())),
    }
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    for p in files {
        let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let name = if let Some(n) = stem.strip_prefix("task_") {
            n
        } else if let Some(n) = stem.strip_suffix("_task") {
            n
        } else {
            continue; // 非任务文件（共享库）
        };
        if out.iter().any(|(n, _)| n == name) {
            return Err(format!(
                "duplicate task '{name}' ({}) — task_x.* 与 x_task.* 只能二选一",
                p.display()
            ));
        }
        out.push((name.to_string(), p));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    if out.len() > max {
        return Err(format!(
            "tasks: {} task(s) exceed max={max} — adjust tasks.max or prune the pool",
            out.len()
        ));
    }
    Ok(out)
}

/// 递归收集 dir 下全部 .ts/.js。
fn walk_ts_js(dir: &Path, out: &mut Vec<std::path::PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let p = entry?.path();
        if p.is_dir() {
            walk_ts_js(&p, out)?;
        } else if matches!(
            p.extension().and_then(|s| s.to_str()),
            Some("ts") | Some("js")
        ) {
            out.push(p);
        }
    }
    Ok(())
}

/// 单任务线程句柄。
pub struct TaskHandle {
    pub name: String,
    pub join: std::thread::JoinHandle<()>,
}

/// 任务池监督器：spawn_all 拉起全部任务线程；shutdown 置位后顺序 join
/// （run_task 的 grace + 看门狗保证每个线程在宽限内返回）。
pub struct TaskSupervisor {
    handles: Vec<TaskHandle>,
}

impl TaskSupervisor {
    /// 扫描 + 逐任务拉起（默认退避基 1s）。目录不存在 = 空池（不报错）。
    pub fn spawn_all(
        cfg: &TasksCfg,
        root: &Path,
        make_bridge: Arc<dyn Fn() -> only_js::bridge::Bridge + Send + Sync>,
        flag: Arc<AtomicBool>,
    ) -> Result<Self, String> {
        let tasks = scan_tasks(&root.join(&cfg.dir), cfg.max)?;
        Self::spawn_selected(cfg, tasks, make_bridge, flag, Duration::from_secs(1))
    }

    /// 拉起指定子集（PRD v2 双模式：池化任务剔除后，存量 TLA 任务走此入口）。
    pub fn spawn_selected(
        cfg: &TasksCfg,
        tasks: Vec<(String, PathBuf)>,
        make_bridge: Arc<dyn Fn() -> only_js::bridge::Bridge + Send + Sync>,
        flag: Arc<AtomicBool>,
        backoff_base: Duration,
    ) -> Result<Self, String> {
        let grace = Duration::from_secs(cfg.stop_grace_secs);
        let mut handles = Vec::with_capacity(tasks.len());
        for (name, path) in tasks {
            eprintln!(
                "task: {name} ({}) → started",
                path.file_name().unwrap_or_default().to_string_lossy()
            );
            let j = std::thread::Builder::new()
                .name(format!("task-{name}"))
                .spawn({
                    let name = name.clone();
                    let flag = flag.clone();
                    let make_bridge = make_bridge.clone();
                    move || task_loop(&name, &path, make_bridge, flag, grace, backoff_base)
                })
                .map_err(|e| format!("spawn task {name}: {e}"))?;
            handles.push(TaskHandle { name, join: j });
        }
        eprintln!("task: {} task(s) → started", handles.len());
        Ok(Self { handles })
    }

    pub fn spawn_all_with_backoff(
        cfg: &TasksCfg,
        root: &Path,
        make_bridge: Arc<dyn Fn() -> only_js::bridge::Bridge + Send + Sync>,
        flag: Arc<AtomicBool>,
        backoff_base: Duration,
    ) -> Result<Self, String> {
        let tasks = scan_tasks(&root.join(&cfg.dir), cfg.max)?;
        Self::spawn_selected(cfg, tasks, make_bridge, flag, backoff_base)
    }

    /// 停机收场：等每个任务线程退出（调用方须先置位停机 flag）。
    pub fn shutdown(self) {
        for h in self.handles {
            let _ = h.join.join();
        }
    }
}

/// 单任务监督循环：catch_unwind 兜 panic；异常收场（Crashed / 无停机 flag 的
/// Stopped/Killed）→ 指数退避重启（成功运行 ≥60s 归零）；停机 flag 置位 → 收场退出。
fn task_loop(
    name: &str,
    path: &Path,
    make_bridge: Arc<dyn Fn() -> only_js::bridge::Bridge + Send + Sync>,
    flag: Arc<AtomicBool>,
    grace: Duration,
    backoff_base: Duration,
) {
    const STABLE: Duration = Duration::from_secs(60);
    let mut backoff = backoff_base;
    let mut attempt: u32 = 0;
    loop {
        let started = Instant::now();
        let exit = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| format!("task runtime: {e}"))?;
            let bridge = (make_bridge)();
            let out = rt.block_on(bridge.run_task(path, flag.clone(), grace));
            drop(bridge);
            Ok::<TaskExit, String>(out)
        }));
        let exit = match exit {
            Ok(Ok(e)) => e,
            Ok(Err(m)) => TaskExit::Crashed(m),
            Err(_) => TaskExit::Crashed("task panicked".into()),
        };
        if flag.load(Ordering::Relaxed) {
            eprintln!("task: {name} → stopped");
            return;
        }
        if started.elapsed() >= STABLE {
            attempt = 0;
            backoff = backoff_base;
        }
        attempt += 1;
        eprintln!(
            "task: {name} crashed, restart in {}s (attempt {attempt}) [{exit:?}]",
            backoff.as_secs().max(1)
        );
        // 切片睡眠：退避期间停机 flag 置位即提前醒来（shutdown join 不被 60s cap 拖住，
        // 审查 #6）；醒来后 run_task 对已置位 flag 立即 Stopped 收场。
        let mut left = backoff;
        while left > Duration::ZERO && !flag.load(Ordering::Relaxed) {
            let step = left.min(Duration::from_millis(100));
            std::thread::sleep(step);
            left = left.saturating_sub(step);
        }
        backoff = backoff.saturating_mul(2).min(Duration::from_secs(60));
    }
}

/// 任务域装配产物（PRD v2 双模式）：两池各自可选（无对应任务即为 None）。
pub struct Tasking {
    /// 存量 TLA 任务监督器（无 TLA 任务 = None）。
    pub sup: Option<TaskSupervisor>,
    /// 池化任务执行体（含 cron 一次性作业派发；无池化/cron 任务 = None）。
    pub pool: Option<Arc<only_js::bridge::task_pool::TaskPool>>,
}

/// 任务域装配（PRD v2 §9 阶段 1）：扫描 → 探测 loop_body 导出分流（池化/TLA）→
/// crontab.yaml 注册 → TaskPool 起 Worker → TLA 监督器拉起 → cron 调度器上树。
/// 探测在独立线程跑（Bridge !Send，own current_thread runtime）；探测失败的文件
/// 归入 TLA 桶（存量监督器会照常报错 + 退避重启，行为与旧版一致）。
pub fn assemble_tasking(
    cfg: &TasksCfg,
    root: &Path,
    make_bridge: Arc<dyn Fn() -> only_js::bridge::Bridge + Send + Sync>,
    flag: Arc<AtomicBool>,
) -> Result<Tasking, String> {
    use only_js::bridge::task_pool::{
        TaskEntry, TaskEventLog, TaskPool, TaskRegistry, parse_crontab_line,
    };

    let scanned = scan_tasks(&root.join(&cfg.dir), cfg.max)?;
    // 1) 探测分流。
    let mut pooled: Vec<(String, PathBuf)> = Vec::new();
    let mut tla: Vec<(String, PathBuf)> = Vec::new();
    if !scanned.is_empty() {
        let make = make_bridge.clone();
        let probe_set = scanned.clone();
        let probed = std::thread::Builder::new()
            .name("task-probe".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| format!("task probe runtime: {e}"))?;
                let bridge = make();
                rt.block_on(async {
                    let mut out = Vec::with_capacity(probe_set.len());
                    for (name, path) in &probe_set {
                        let is_loop = bridge.probe_task_loop(path).await.unwrap_or(false);
                        out.push((name.clone(), path.clone(), is_loop));
                    }
                    Ok::<_, String>(out)
                })
            })
            .map_err(|e| format!("spawn task probe: {e}"))?
            .join()
            .map_err(|_| "task probe panicked".to_string())??;
        for (name, path, is_loop) in probed {
            if is_loop {
                pooled.push((name, path));
            } else {
                tla.push((name, path));
            }
        }
    }

    // 2) 注册表 + 事件日志。
    let log = TaskEventLog::new(
        cfg.event_log.enabled,
        &cfg.event_log.path,
        cfg.event_log.max_mb,
    )
    .map_err(|e| format!("task event log: {e}"))?;
    let registry = TaskRegistry::new(log);

    // 3) 池化任务先入注册表，再解析 crontab.yaml（tasks.dir 相对；行级错误 fail-fast
    //    带行号；与池化任务同名的 cron 行拒启）。
    for (name, path) in pooled {
        registry.upsert(TaskEntry::long(name, path));
    }
    let mut cron_count = 0usize;
    let tasks_dir = root.join(&cfg.dir);
    let crontab_path = tasks_dir.join(&cfg.crontab);
    if let Ok(content) = std::fs::read_to_string(&crontab_path) {
        for (i, line) in content.lines().enumerate() {
            if line.trim().is_empty() || line.trim_start().starts_with('#') {
                continue;
            }
            let (expr, p) = parse_crontab_line(line)
                .map_err(|e| format!("{}:{}: {e}", crontab_path.display(), i + 1))?;
            let path = tasks_dir.join(p.trim_start_matches("./"));
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("cron")
                .to_string();
            if registry.get(&name).is_some() {
                return Err(format!(
                    "crontab:{}: task '{name}' duplicates a pool task",
                    i + 1
                ));
            }
            registry.upsert(TaskEntry::cron(name, path, expr));
            cron_count += 1;
        }
    }

    // 4) TaskPool（有池化任务或 cron 才起）+ cron 调度器。
    let pool = if !registry.list().is_empty() {
        let pool = TaskPool::new(
            cfg.pool.workers,
            Duration::from_millis(cfg.pool.loop_body_timeout_ms),
            Duration::from_millis(cfg.pool.interval_ms),
            make_bridge.clone(),
            registry,
        );
        pool.spawn();
        if cron_count > 0 {
            spawn_cron_driver(pool.clone(), flag.clone());
        }
        Some(pool)
    } else {
        None
    };

    // 5) 存量 TLA 监督器（有 TLA 任务才起）。
    let sup = if tla.is_empty() {
        None
    } else {
        Some(TaskSupervisor::spawn_selected(
            cfg,
            tla,
            make_bridge,
            flag,
            Duration::from_secs(1),
        )?)
    };
    Ok(Tasking { sup, pool })
}

/// cron 调度器（PRD v2 §6.4）：tokio 任务，睡到最近 next_run → 先推后 next_run
/// （防重复触发，单机权威）→ 投一次性作业。停机 flag 置位即退。
fn spawn_cron_driver(pool: Arc<only_js::bridge::task_pool::TaskPool>, flag: Arc<AtomicBool>) {
    use only_js::bridge::task_pool::{OnceJob, TaskKind};
    tokio::spawn(async move {
        loop {
            if flag.load(Ordering::Relaxed) {
                return;
            }
            let now = std::time::SystemTime::now();
            let next = pool
                .registry
                .list()
                .into_iter()
                .filter_map(|e| match (&e.kind, e.enabled) {
                    (
                        TaskKind::Cron {
                            next_run: Some(t), ..
                        },
                        true,
                    ) => Some((*t, e)),
                    _ => None,
                })
                .min_by_key(|(t, _)| *t);
            let Some((fire_at, entry)) = next else {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(1)) => continue,
                    _ = wait_flag(&flag) => return,
                }
            };
            let wait = fire_at.duration_since(now).unwrap_or(Duration::ZERO);
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = wait_flag(&flag) => return,
            }
            // 到点：先计算并写回下一次触发时间，再投作业（顺序 = 防重契约）。
            let new_next = match &entry.kind {
                TaskKind::Cron { expr, .. } => expr.next_after(std::time::SystemTime::now()),
                _ => None,
            };
            pool.registry.set_next_run(&entry.name, new_next);
            pool.submit_once(OnceJob {
                name: entry.name.clone(),
                path: entry.path.clone(),
            });
            pool.registry.log.record(
                "tasks.commands",
                "cron.triggered",
                serde_json::json!({ "task": entry.name }),
            );
        }
    });
}

/// flag 置位唤醒（select 分支用）：25ms 轮询（ponytail：简单可靠，量级 irrelevant）。
async fn wait_flag(flag: &AtomicBool) {
    while !flag.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use only_js::bridge::LoaderShared;
    use only_js::bridge::{Bridge, Extras, InMemoryKV, MqInstance, NamedRegistry, SchemaRegistry};
    use std::collections::HashMap;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ojtasks-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn cfg(dir: &str, max: usize) -> TasksCfg {
        TasksCfg {
            dir: dir.into(),
            max,
            stop_grace_secs: 30,
            ..Default::default()
        }
    }

    /// BDD：scan 只认 task_*/ *_task 约定文件，其余（含子目录共享库）忽略；递归入池。
    #[test]
    fn given_dir_with_task_and_lib_files_when_scan_then_only_task_files_listed() {
        let d = tmpdir("scan");
        let pool = d.join("tasks");
        std::fs::create_dir_all(&pool).unwrap();
        std::fs::write(pool.join("task_orders.ts"), "export {};\n").unwrap();
        std::fs::write(pool.join("audit_task.js"), "export {};\n").unwrap();
        std::fs::write(pool.join("helpers.ts"), "export {};\n").unwrap();
        std::fs::create_dir_all(pool.join("_shared")).unwrap();
        std::fs::write(pool.join("_shared/x.ts"), "export {};\n").unwrap();
        std::fs::create_dir_all(pool.join("nested/deep")).unwrap();
        std::fs::write(pool.join("nested/deep/task_deep.ts"), "export {};\n").unwrap();
        let out = scan_tasks(&pool, 64).unwrap();
        assert_eq!(
            out.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            vec!["audit", "deep", "orders"],
            "{out:?}"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// BDD：同名双写（task_x + x_task）→ Err。
    #[test]
    fn given_both_prefix_and_suffix_same_name_when_scan_then_err_conflict() {
        let d = tmpdir("dup");
        let pool = d.join("tasks");
        std::fs::create_dir_all(&pool).unwrap();
        std::fs::write(pool.join("task_orders.ts"), "export {};\n").unwrap();
        std::fs::write(pool.join("orders_task.ts"), "export {};\n").unwrap();
        let err = scan_tasks(&pool, 64).unwrap_err();
        assert!(err.contains("duplicate task 'orders'"), "{err}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// BDD：超 max → Err（防误配打满机器）。
    #[test]
    fn given_more_tasks_than_max_when_scan_then_err_limit() {
        let d = tmpdir("max");
        let pool = d.join("tasks");
        std::fs::create_dir_all(&pool).unwrap();
        for n in ["a", "b", "c"] {
            std::fs::write(pool.join(format!("task_{n}.ts")), "export {};\n").unwrap();
        }
        let err = scan_tasks(&pool, 2).unwrap_err();
        assert!(err.contains("max"), "{err}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 测试用 Bridge 工厂：内存 kafka（poll 尊重 timeoutMs，10ms 步进）+ 任务 flag。
    fn test_bridge_factory(
        root: &Path,
        flag: Arc<AtomicBool>,
    ) -> Arc<dyn Fn() -> Bridge + Send + Sync> {
        let root = root.to_path_buf();
        Arc::new(move || {
            let mut reg = NamedRegistry::new();
            let inst = MqInstance::new(
                "kafka",
                Arc::new(|_m: String, _p: serde_json::Value| {
                    Box::pin(async {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        Ok(serde_json::json!({ "messages": [] }))
                    })
                }),
            );
            reg.register("default", Arc::new(inst)).unwrap();
            Bridge::with_dbs_and_loader(
                HashMap::new(),
                Arc::new(InMemoryKV::new()),
                SchemaRegistry::new(),
                false,
                Some(Arc::new(LoaderShared {
                    project_root: root.clone(),
                    ts: true,
                })),
                Extras {
                    kafkas: Some(Arc::new(reg)),
                    rabbits: Some(Arc::new(NamedRegistry::new())),
                    tasks_flag: Some(flag.clone()),
                    ..Default::default()
                },
            )
        })
    }

    fn crash_task(pool: &Path) -> PathBuf {
        let p = pool.join("task_boom.ts");
        std::fs::write(&p, "export {};\nthrow new Error(\"boom\");\n").unwrap();
        p
    }

    /// BDD：崩溃任务按退避重启（工厂调用计数 ≥2 = 重启发生）；置位后收场 join。
    #[tokio::test(flavor = "current_thread")]
    async fn given_crashing_task_when_supervised_then_restarts_with_backoff() {
        let d = tmpdir("backoff");
        let pool = d.join("tasks");
        std::fs::create_dir_all(&pool).unwrap();
        crash_task(&pool);
        let flag = Arc::new(AtomicBool::new(false));
        // 计数器：工厂每被调用一次 = 任务（重新）启动一次。
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let make_bridge = {
            let counter = counter.clone();
            let f = test_bridge_factory(&d, flag.clone());
            Arc::new(move || {
                counter.fetch_add(1, Ordering::Relaxed);
                f()
            }) as Arc<dyn Fn() -> Bridge + Send + Sync>
        };
        let sup = TaskSupervisor::spawn_all_with_backoff(
            &cfg("tasks", 64),
            &d,
            make_bridge,
            flag.clone(),
            Duration::from_millis(80),
        )
        .unwrap();
        // 等 ≥2 次启动（首次 + 至少一次退避重启）。
        let deadline = Instant::now() + Duration::from_secs(10);
        while counter.load(Ordering::Relaxed) < 2 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            counter.load(Ordering::Relaxed) >= 2,
            "task was not restarted"
        );
        flag.store(true, Ordering::Relaxed);
        sup.shutdown();
        let _ = std::fs::remove_dir_all(&d);
    }

    /// BDD：停机 flag 置位 → 全部任务线程在宽限内 join。
    #[tokio::test(flavor = "current_thread")]
    async fn given_running_tasks_when_shutdown_flag_then_all_join_within_grace() {
        let d = tmpdir("stop");
        let pool = d.join("tasks");
        std::fs::create_dir_all(&pool).unwrap();
        std::fs::write(
            pool.join("task_loop.ts"),
            "export {};\nwhile (!tasks.stopping()) { await Kafka(\"default\").poll([\"t\"], { timeoutMs: 30 }); }\n",
        )
        .unwrap();
        let flag = Arc::new(AtomicBool::new(false));
        let make_bridge = test_bridge_factory(&d, flag.clone());
        let sup = TaskSupervisor::spawn_all_with_backoff(
            &cfg("tasks", 64),
            &d,
            make_bridge,
            flag.clone(),
            Duration::from_millis(80),
        )
        .unwrap();
        assert_eq!(sup.handles.len(), 1);
        tokio::time::sleep(Duration::from_millis(150)).await;
        flag.store(true, Ordering::Relaxed);
        // shutdown 在宽限（此处 cfg 给 30s，但任务 30ms 内自退）内完成——用线程 + 超时兜底。
        let jh = std::thread::spawn(move || sup.shutdown());
        let done =
            tokio::time::timeout(Duration::from_secs(5), tokio::task::spawn_blocking(|| ())).await;
        assert!(done.is_ok());
        jh.join().unwrap();
        let _ = std::fs::remove_dir_all(&d);
    }

    /// BDD（PRD v2 FR-LT-002~005）：loop_body 导出 → 池化模式——setup 执行、
    /// loop_body 轮转计数、stop 后 teardown 执行（kv 侧可观察）；TLA 任务分流不受影响。
    #[tokio::test(flavor = "current_thread")]
    async fn given_loop_task_when_pool_runs_then_lifecycle_and_teardown_on_stop() {
        use only_js::bridge::KVStore;
        use only_js::bridge::task_pool::{TaskKind, TaskStatus};
        let d = tmpdir("pool");
        let pool_dir = d.join("tasks");
        std::fs::create_dir_all(&pool_dir).unwrap();
        std::fs::write(
            pool_dir.join("task_counter.ts"),
            "export async function setup() { await kv.set(\"up\", \"1\"); }\n\
             export async function loop_body() {\n\
               const n = Number((await kv.get(\"n\")) ?? \"0\");\n\
               await kv.set(\"n\", String(n + 1));\n\
             }\n\
             export async function teardown() { await kv.set(\"down\", \"1\"); }\n",
        )
        .unwrap();
        // 存量 TLA 任务同目录共存：无 loop_body 导出 → TLA 桶（监督器线程）。
        std::fs::write(
            pool_dir.join("audit_task.ts"),
            "export {};\nwhile (!tasks.stopping()) { await Kafka(\"default\").poll([\"t\"], { timeoutMs: 30 }); }\n",
        )
        .unwrap();
        let flag = Arc::new(AtomicBool::new(false));
        let kv = Arc::new(only_js::bridge::InMemoryKV::new());
        let make_bridge = {
            let kv = kv.clone();
            let flag = flag.clone();
            let root = d.clone();
            Arc::new(move || {
                let mut reg = only_js::bridge::NamedRegistry::new();
                let inst = only_js::bridge::MqInstance::new(
                    "kafka",
                    Arc::new(|_m: String, _p: serde_json::Value| {
                        Box::pin(async {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                            Ok(serde_json::json!({ "messages": [] }))
                        })
                    }),
                );
                reg.register("default", Arc::new(inst)).unwrap();
                only_js::bridge::Bridge::with_dbs_and_loader(
                    std::collections::HashMap::new(),
                    kv.clone(),
                    only_js::bridge::SchemaRegistry::new(),
                    false,
                    Some(Arc::new(only_js::bridge::LoaderShared {
                        project_root: root.clone(),
                        ts: true,
                    })),
                    only_js::bridge::Extras {
                        tasks_flag: Some(flag.clone()),
                        kafkas: Some(Arc::new(reg)),
                        ..Default::default()
                    },
                )
            }) as Arc<dyn Fn() -> Bridge + Send + Sync>
        };
        let tasking = assemble_tasking(&cfg("tasks", 64), &d, make_bridge, flag.clone()).unwrap();
        let pool = tasking.pool.expect("loop task must assemble a pool");
        assert!(
            tasking.sup.is_some(),
            "TLA task must go to legacy supervisor"
        );
        // setup + loop_body 轮转：run_count 涨、kv.n 涨、kv.up = 1。
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let e = pool.registry.get("counter").unwrap();
            if e.run_count >= 3 {
                break;
            }
            assert!(Instant::now() < deadline, "loop_body did not run: {e:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(kv.get("up").await.unwrap().as_deref(), Some("1"));
        // stop：desired=false → Worker teardown → 状态 Stopped + kv.down = 1。
        pool.registry.set_enabled("counter", false);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let e = pool.registry.get("counter").unwrap();
            if e.status == TaskStatus::Stopped {
                break;
            }
            assert!(Instant::now() < deadline, "task did not stop: {e:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(kv.get("down").await.unwrap().as_deref(), Some("1"));
        assert!(kv.get("n").await.unwrap().unwrap().parse::<u64>().unwrap() >= 3);
        // 双模式条目同时在册：counter=long（池化）、audit=long（TLA，监督器管状态，
        // 注册表不跟踪 TLA 运行态——它只是不在池注册表里出现……断言池表只含 counter）。
        let names: Vec<String> = pool.registry.list().into_iter().map(|e| e.name).collect();
        assert_eq!(names, vec!["counter".to_string()]);
        // start 复位：enable → Worker 下轮重连（状态离开 Stopped，异步——轮询等）。
        pool.registry.start("counter");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let e = pool.registry.get("counter").unwrap();
            if e.status != TaskStatus::Stopped {
                break;
            }
            assert!(Instant::now() < deadline, "task did not restart: {e:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let _ = pool
            .registry
            .get("counter")
            .map(|e| assert!(matches!(e.kind, TaskKind::Long)));
        flag.store(true, Ordering::Relaxed);
        pool.shutdown_and_join();
        let _ = std::fs::remove_dir_all(&d);
    }

    /// BDD：多个池化任务同 Worker 轮转——各自 setup 一次、loop_body 都在推进
    /// （轮转公平，无饥饿；interval_ms 默认 100ms）。
    #[tokio::test(flavor = "current_thread")]
    async fn given_two_loop_tasks_when_pool_runs_then_both_advance() {
        use only_js::bridge::KVStore;
        let d = tmpdir("pool2");
        let pool_dir = d.join("tasks");
        std::fs::create_dir_all(&pool_dir).unwrap();
        for name in ["alpha", "beta"] {
            std::fs::write(
                pool_dir.join(format!("task_{name}.ts")),
                format!(
                    "export async function setup() {{ await kv.set(\"{name}:up\", \"1\"); }}\n\
                     export async function loop_body() {{\n\
                       const n = Number((await kv.get(\"{name}:n\")) ?? \"0\");\n\
                       await kv.set(\"{name}:n\", String(n + 1));\n\
                     }}\n"
                ),
            )
            .unwrap();
        }
        let flag = Arc::new(AtomicBool::new(false));
        let kv = Arc::new(only_js::bridge::InMemoryKV::new());
        let make_bridge = {
            let kv = kv.clone();
            let flag = flag.clone();
            let root = d.clone();
            Arc::new(move || {
                only_js::bridge::Bridge::with_dbs_and_loader(
                    std::collections::HashMap::new(),
                    kv.clone(),
                    only_js::bridge::SchemaRegistry::new(),
                    false,
                    Some(Arc::new(only_js::bridge::LoaderShared {
                        project_root: root.clone(),
                        ts: true,
                    })),
                    only_js::bridge::Extras {
                        tasks_flag: Some(flag.clone()),
                        ..Default::default()
                    },
                )
            }) as Arc<dyn Fn() -> Bridge + Send + Sync>
        };
        let tasking = assemble_tasking(&cfg("tasks", 64), &d, make_bridge, flag.clone()).unwrap();
        let pool = tasking.pool.expect("pooled tasks must assemble a pool");
        // 轮询：两个任务的 kv 计数都 ≥3（10s 宽限，远超 100ms 轮间节奏）。
        for name in ["alpha", "beta"] {
            let key = format!("{name}:n");
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let n = kv
                    .get(&key)
                    .await
                    .unwrap()
                    .map(|s| s.parse::<u64>().unwrap_or(0))
                    .unwrap_or(0);
                if n >= 3 {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "task {name} starved: kv.{key} = {n}"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            assert_eq!(
                kv.get(&format!("{name}:up")).await.unwrap().as_deref(),
                Some("1")
            );
        }
        // 注册表两条 long 条目；两任务很可能同 Worker（workers=4，2 任务各自独占）。
        let longs: Vec<String> = pool
            .registry
            .list()
            .into_iter()
            .filter(|e| matches!(e.kind, only_js::bridge::task_pool::TaskKind::Long))
            .map(|e| e.name)
            .collect();
        assert_eq!(longs, vec!["alpha".to_string(), "beta".to_string()]);
        flag.store(true, Ordering::Relaxed);
        pool.shutdown_and_join();
        let _ = std::fs::remove_dir_all(&d);
    }

    /// BDD（PRD v2 §6.4）：crontab.yaml（tasks.dir 相对）行级错误 fail-fast 带行号；
    /// 合法行注册 cron 条目；与池化任务同名拒启。
    #[tokio::test(flavor = "current_thread")]
    async fn given_crontab_when_assemble_then_cron_registered_and_bad_line_fails() {
        let d = tmpdir("cron");
        std::fs::create_dir_all(d.join("tasks/task")).unwrap();
        std::fs::create_dir_all(d.join("tasks/jobs")).unwrap();
        std::fs::write(
            d.join("tasks/task/crontab.yaml"),
            "*/5 * * * * ./jobs/xx.ts\n",
        )
        .unwrap();
        std::fs::write(d.join("tasks/jobs/xx.ts"), "export {};\n").unwrap();
        let flag = Arc::new(AtomicBool::new(false));
        let make_bridge = test_bridge_factory(&d, flag.clone());
        let tasking = assemble_tasking(&cfg("tasks", 64), &d, make_bridge, flag).unwrap();
        let pool = tasking.pool.expect("cron entry must assemble a pool");
        let e = pool.registry.get("xx").expect("cron entry registered");
        assert!(matches!(
            e.kind,
            only_js::bridge::task_pool::TaskKind::Cron { .. }
        ));
        pool.shutdown_and_join();
        let _ = std::fs::remove_dir_all(&d);

        // 坏行：行号 + 错误文案 fail-fast。
        let d2 = tmpdir("cron-bad");
        std::fs::create_dir_all(d2.join("tasks/task")).unwrap();
        std::fs::write(d2.join("tasks/task/crontab.yaml"), "# ok\n*/5 * * * *\n").unwrap();
        let flag2 = Arc::new(AtomicBool::new(false));
        let make_bridge2 = test_bridge_factory(&d2, flag2.clone());
        let e = assemble_tasking(&cfg("tasks", 64), &d2, make_bridge2, flag2)
            .err()
            .unwrap();
        assert!(e.contains(":2:"), "{e}");
        let _ = std::fs::remove_dir_all(&d2);

        // 与池化任务同名：拒启（cron 行不许吞掉池化任务）。
        let d3 = tmpdir("cron-dup");
        std::fs::create_dir_all(d3.join("tasks/task")).unwrap();
        std::fs::create_dir_all(d3.join("tasks/jobs")).unwrap();
        std::fs::write(
            d3.join("tasks/task/crontab.yaml"),
            "* * * * * ./jobs/watch.ts\n",
        )
        .unwrap();
        std::fs::write(d3.join("tasks/jobs/watch.ts"), "export {};\n").unwrap();
        std::fs::write(
            d3.join("tasks/task_watch.ts"),
            "export async function loop_body() {}\n",
        )
        .unwrap();
        let flag3 = Arc::new(AtomicBool::new(false));
        let make_bridge3 = test_bridge_factory(&d3, flag3.clone());
        let e = assemble_tasking(&cfg("tasks", 64), &d3, make_bridge3, flag3)
            .err()
            .unwrap();
        assert!(e.contains("duplicates a pool task"), "{e}");
        let _ = std::fs::remove_dir_all(&d3);
    }
}
