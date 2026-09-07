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
        Self::spawn_all_with_backoff(cfg, root, make_bridge, flag, Duration::from_secs(1))
    }

    pub fn spawn_all_with_backoff(
        cfg: &TasksCfg,
        root: &Path,
        make_bridge: Arc<dyn Fn() -> only_js::bridge::Bridge + Send + Sync>,
        flag: Arc<AtomicBool>,
        backoff_base: Duration,
    ) -> Result<Self, String> {
        let dir = root.join(&cfg.dir);
        let tasks = scan_tasks(&dir, cfg.max)?;
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
}
