use super::*;

#[test]
fn given_star_fields_when_next_then_next_minute_boundary() {
    let e = CronExpr::parse("* * * * *").unwrap();
    let from = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    // 对齐到下一分钟起点（cron 分钟粒度）。
    let expect = UNIX_EPOCH + Duration::from_secs((1_700_000_000 / 60 + 1) * 60);
    assert_eq!(e.next_after(from).unwrap(), expect);
}

#[test]
fn given_every_5_min_when_next_then_aligns_to_step() {
    let e = CronExpr::parse("*/5 * * * *").unwrap();
    let from = UNIX_EPOCH + Duration::from_secs(1_700_000_001);
    let next = e.next_after(from);
    let mins = next.unwrap().duration_since(UNIX_EPOCH).unwrap().as_secs() / 60;
    assert_eq!(mins % 5, 0);
    assert!(next.unwrap() > from);
}

#[test]
fn given_specific_time_when_next_then_that_time() {
    // 2023-11-14T22:13:20Z（已知周二）→ 下一 30 2 * * * = 次日 02:30。
    let e = CronExpr::parse("30 2 * * *").unwrap();
    let from = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let next = e.next_after(from).unwrap();
    let (mi, h, _, _, _) = CronExpr::civil(next.duration_since(UNIX_EPOCH).unwrap().as_secs());
    assert_eq!((mi, h), (30, 2));
}

#[test]
fn given_dow_7_when_parse_then_treated_as_sunday_0() {
    let e = CronExpr::parse("0 0 * * 7").unwrap();
    assert_eq!(e.dow, CronExpr::parse("0 0 * * 0").unwrap().dow);
}

#[test]
fn given_macro_or_six_fields_when_parse_then_err() {
    assert!(CronExpr::parse("@daily").is_err());
    assert!(CronExpr::parse("0 */5 * * * *").is_err());
    assert!(CronExpr::parse("bad * * * *").is_err());
}

#[test]
fn given_crontab_line_when_parse_then_expr_and_path() {
    let (e, p) = parse_crontab_line("*/5 * * * * ./aa/bb/cc.ts").unwrap();
    assert_eq!(e, CronExpr::parse("*/5 * * * *").unwrap());
    assert_eq!(p, "./aa/bb/cc.ts");
    assert!(parse_crontab_line("*/5 * * * *").is_err());
    assert!(parse_crontab_line("# comment").is_err());
    // 行尾 `#` 注释：截断后正常解析（crontab 惯例）。
    let (e, p) = parse_crontab_line("*/5 * * * * ./aa/bb/cc.ts   # 每 5 分钟").unwrap();
    assert_eq!(e, CronExpr::parse("*/5 * * * *").unwrap());
    assert_eq!(p, "./aa/bb/cc.ts");
}

#[test]
fn given_registry_when_toggle_then_status_follows() {
    let reg = TaskRegistry::new(TaskEventLog::memory_only());
    reg.upsert(TaskEntry::long(
        "orders",
        PathBuf::from("/t/task_orders.ts"),
    ));
    // set_enabled 只翻转开关（FR-LT-005：Stopped 由 worker 在 teardown 完成后置位；
    // 未连接的 Pending 任务由 worker 直接置 Stopped）。
    assert!(reg.set_enabled("orders", false));
    assert!(!reg.get("orders").unwrap().enabled);
    assert_eq!(reg.get("orders").unwrap().status, TaskStatus::Pending);
    assert!(reg.request_reload("orders"));
    assert!(reg.take_reload("orders"));
    assert!(!reg.take_reload("orders"));
    assert!(!reg.set_enabled("nope", true));
}

#[test]
fn given_event_log_when_record_then_recent_ring_keeps_latest() {
    let log = TaskEventLog::memory_only();
    for i in 0..(RECENT_CAP as u64 + 10) {
        log.record("tasks.execution", "t", serde_json::json!({"i": i}));
    }
    let recent = log.recent(0);
    assert_eq!(recent.len(), RECENT_CAP);
    assert_eq!(recent.last().unwrap()["event"]["payload"]["i"], 1009);
}

#[test]
fn given_civil_when_known_anchor_then_components() {
    // 1970-01-01 00:00:00Z = 周四。
    let (mi, h, d, mo, dow) = CronExpr::civil(0);
    assert_eq!((mi, h, d, mo, dow), (0, 0, 1, 1, 4));
}

// ---------- panic 补员 / 停机拒绝（对照 frame_pool 夹具形态） ----------

use super::super::{Extras, InMemoryKV, LoaderShared, SchemaRegistry};
use std::collections::HashMap;
use std::time::Instant;

/// Bridge 测试工厂（带模块加载器；panic 注入/拒绝路径均不真正执行任务文件）。
fn test_bridge(root: &std::path::Path) -> Arc<dyn Fn() -> Bridge + Send + Sync> {
    let root = root.to_path_buf();
    Arc::new(move || {
        Bridge::with_dbs_and_loader(
            HashMap::new(),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            Some(Arc::new(LoaderShared {
                project_root: root.clone(),
                ts: true,
            })),
            Extras::default(),
        )
    })
}

fn bridge_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("oj-taskpool-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// panic 补员钉（对照 frame_pool 的 worker panic 存活钉）：Worker 线程内 panic
/// （make 首调注入）后池必须补员——旧代码只标记 failed 不 respawn，坑位永久丢失。
#[test]
fn given_worker_panic_when_respawn_then_live_recovered() {
    let d = bridge_dir("panic");
    let inner = test_bridge(&d);
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let make: Arc<dyn Fn() -> Bridge + Send + Sync> = Arc::new(move || {
        if calls.fetch_add(1, Ordering::SeqCst) == 0 {
            panic!("test-injected task worker panic");
        }
        inner()
    });
    let pool = TaskPool::new(
        1,
        Duration::from_secs(1),
        Duration::from_millis(5),
        make,
        TaskRegistry::new(TaskEventLog::memory_only()),
    );
    pool.spawn();
    // 第 2 次 make = 补员 Worker 已就位（首调 panic → fetch_sub → 原参重起）。
    let deadline = Instant::now() + Duration::from_secs(10);
    while seen.load(Ordering::SeqCst) < 2 {
        assert!(
            Instant::now() < deadline,
            "panic 后必须补员，calls={}",
            seen.load(Ordering::SeqCst)
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(pool.live_workers(), 1, "存活 Worker 数必须恢复");
    pool.shutdown_and_join();
    assert_eq!(pool.live_workers(), 0, "join 后 Worker 全部退出");
}

/// 停机拒绝钉：stopping 置位后 run_once 返回明确错误，且不触碰任务状态
/// （不得误标 Running/Failed）。
#[tokio::test(flavor = "current_thread")]
async fn given_stopping_when_run_once_then_rejected_without_status_change() {
    let d = bridge_dir("stop");
    let pool = TaskPool::new(
        1,
        Duration::from_secs(1),
        Duration::from_millis(5),
        test_bridge(&d),
        TaskRegistry::new(TaskEventLog::memory_only()),
    );
    pool.registry.upsert(TaskEntry::cron(
        "nightly",
        PathBuf::from("/t/nightly.ts"),
        CronExpr::parse("* * * * *").unwrap(),
    ));
    pool.shutdown();
    let bridge = (test_bridge(&d))();
    let err = pool
        .run_once(
            &bridge,
            &OnceJob {
                name: "nightly".into(),
                path: PathBuf::from("/t/nightly.ts"),
            },
        )
        .await
        .unwrap_err();
    assert!(err.contains("stopping"), "{err}");
    assert_eq!(
        pool.registry.get("nightly").unwrap().status,
        TaskStatus::Pending
    );
}
