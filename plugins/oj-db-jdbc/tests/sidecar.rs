//! SidecarMgr 集成用例（spec §5.1 / §6）。harness=false 自有 main：
//! - env `OJ_FAKE_JAVA=<场景>` 存在 → **本二进制自重现为假 java**（spawn `current_exe()`，
//!   跨平台含 Windows CI，不用 shell 脚本）；
//! - 否则顺序跑场景（单线程顺序执行：java_missing 用例要改进程级 env，顺序执行天然安全）。
//!
//! 场景（夹具侧，见文件底部）：
//! `echo`（hello + 回显，EOF 退 0）/ `flood`（stderr 喷 400KB 后正常服务）/
//! `crash`（收到 `"boom":true` 的请求即 exit(1)）/ `badproto`（hello proto=999）/
//! `hang`（什么都不打印，测 hello 有界等待）。
//!
//! 同步纪律：全部用有界等待（timeout/deadline），禁止固定 sleep。

use oj_db_jdbc::sidecar::{SidecarCfg, SidecarMgr};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;

/// 用例总等待上界（CI 慢机余量），是 deadline 不是同步手段。
const BOUND: Duration = Duration::from_secs(20);

fn main() {
    // 假 java 夹具入口：被 SidecarMgr 以 current_exe() + env OJ_FAKE_JAVA spawn 时进入。
    if std::env::var_os("OJ_FAKE_JAVA").is_some() {
        fake_java_main();
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");
    let mut failed = 0usize;
    // 顺序跑（见文件头）：任一失败打印后继续，退出码反映全部结果。
    for (name, run) in scenarios() {
        print!("sidecar: {name} ... ");
        match rt.block_on(run()) {
            Ok(()) => println!("ok"),
            Err(e) => {
                failed += 1;
                println!("FAIL: {e}");
            }
        }
    }
    if failed > 0 {
        println!("sidecar: {failed} scenario(s) failed");
        std::process::exit(1);
    }
    println!("sidecar: all scenarios passed");
}

type PinFut = std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>>;
type Scenario = (&'static str, fn() -> PinFut);

fn scenarios() -> Vec<Scenario> {
    fn b(f: impl std::future::Future<Output = Result<(), String>> + Send + 'static) -> PinFut {
        Box::pin(f)
    }
    vec![
        ("hello_ok_and_request_roundtrip", || {
            b(hello_ok_and_request_roundtrip())
        }),
        ("badproto_reports_both_versions", || {
            b(badproto_reports_both_versions())
        }),
        ("stderr_flood_request_still_ok", || {
            b(stderr_flood_request_still_ok())
        }),
        (
            "crash_inflight_err_stale_handle_reconnect_reqid_continues",
            || b(crash_inflight_err_stale_handle_reconnect_reqid_continues()),
        ),
        ("hang_hello_bounded_wait", || b(hang_hello_bounded_wait())),
        ("clean_eof_exit", || b(clean_eof_exit())),
        (
            "crash_concurrent_restart_exactly_one_death_per_boom",
            || b(crash_concurrent_restart_exactly_one_death_per_boom()),
        ),
        ("java_missing_error_lists_search_order", || {
            b(java_missing_error_lists_search_order())
        }),
    ]
}

/// 夹具 manager：java_bin 指向本二进制自身（自重现），env 传入场景名。
fn mgr(scenario: &str) -> SidecarMgr {
    cfg_mgr(scenario, Duration::from_secs(10), true)
}

fn cfg_mgr(scenario: &str, hello_timeout: Duration, fake: bool) -> SidecarMgr {
    SidecarMgr::new(SidecarCfg {
        java_bin: Some(
            std::env::current_exe()
                .expect("current_exe")
                .to_string_lossy()
                .into_owned(),
        ),
        java_opts: vec![],
        bridge_jar: PathBuf::from("/nonexistent/bridge.jar"),
        drivers_dir: PathBuf::from("/nonexistent/drivers"),
        hello_timeout,
        env: if fake {
            vec![("OJ_FAKE_JAVA".into(), scenario.into())]
        } else {
            vec![]
        },
        log: None,
    })
}

async fn hello_ok_and_request_roundtrip() -> Result<(), String> {
    let m = mgr("echo");
    let v: Value = tokio::time::timeout(BOUND, m.request("query", json!({"sql":"select 1"})))
        .await
        .map_err(|_| "deadline".to_string())?
        .map_err(|e| format!("request: {e}"))?;
    assert2(v["op"] == json!("query"), "op echo")?;
    assert2(v["req_id"] == json!(1), "首个请求 id 从 1 起")?;
    assert2(v["args"]["sql"] == json!("select 1"), "args echo")?;
    m.shutdown().await.map_err(|e| format!("shutdown: {e}"))
}

async fn badproto_reports_both_versions() -> Result<(), String> {
    let m = mgr("badproto");
    let e = tokio::time::timeout(BOUND, m.request("query", json!({})))
        .await
        .map_err(|_| "deadline".to_string())?
        .expect_err("proto 不匹配必须报错");
    assert2(
        e.contains("proto"),
        &format!("须报 proto 不匹配, 实际: {e}"),
    )?;
    assert2(e.contains("999"), &format!("须含 sidecar proto, 实际: {e}"))?;
    assert2(
        e.contains("9.9.9"),
        &format!("须含 sidecar bridge 版本, 实际: {e}"),
    )?;
    assert2(e.contains('1'), &format!("须含 plugin 期望版本, 实际: {e}"))
}

async fn stderr_flood_request_still_ok() -> Result<(), String> {
    let m = mgr("flood"); // 启动时 stderr 喷 400KB（>>64KB 管道缓冲）
    let v: Value = tokio::time::timeout(BOUND, m.request("query", json!({"sql":"x"})))
        .await
        .map_err(|_| "deadline: stderr 阻塞会挂死请求（楔形死锁）")?
        .map_err(|e| format!("request: {e}"))?;
    assert2(v["req_id"] == json!(1), "req_id")?;
    // drain 逐行保留尾部环形缓冲（~20 行）供 connect 失败诊断。
    let tail = m.stderr_tail();
    assert2(
        tail.len() == 20,
        &format!("环形缓冲上限 20 行, 实际 {}", tail.len()),
    )?;
    assert2(
        tail.iter().all(|l| l.starts_with("fake-java-flood")),
        "tail 内容应为 flood 行",
    )?;
    assert2(
        tail.iter().map(|l| l.len()).sum::<usize>() > 30_000,
        "tail 总量",
    )?;
    m.shutdown().await.map_err(|e| format!("shutdown: {e}"))
}

async fn crash_inflight_err_stale_handle_reconnect_reqid_continues() -> Result<(), String> {
    let m = mgr("crash"); // 收到 {"boom":true} 的请求即 exit(1)
    let g0 = m.generation();
    let r1: Value = m
        .request("query", json!({"sql":"a"}))
        .await
        .map_err(|e| format!("r1: {e}"))?;
    assert2(r1["req_id"] == json!(1), "r1 req_id")?;
    // 在途请求收明确错误（sidecar lost + reconnect 归类）。
    let boom = tokio::time::timeout(BOUND, m.request("query", json!({"boom":true})))
        .await
        .map_err(|_| "deadline")?
        .expect_err("boom 请求必须收 Err");
    assert2(boom.contains("sidecar lost"), &format!("实际: {boom}"))?;
    assert2(boom.contains("reconnect"), &format!("实际: {boom}"))?;
    // 世代 +1：旧世代句柄立即报 reconnect，绝不隐式落到新 JVM。
    assert2(
        m.generation() == g0 + 1,
        &format!("generation {} != {}", m.generation(), g0 + 1),
    )?;
    let stale = m.check_generation(g0).expect_err("旧世代必须拒绝");
    assert2(stale.contains("reconnect"), &format!("实际: {stale}"))?;
    assert2(m.check_generation(m.generation()).is_ok(), "当前世代应通过")?;
    // 下一次请求触发重启且 req_id 全局单调不归零。
    let r3: Value = tokio::time::timeout(BOUND, m.request("query", json!({"sql":"c"})))
        .await
        .map_err(|_| "deadline: 重启后请求必须完成")?
        .map_err(|e| format!("r3: {e}"))?;
    assert2(
        r3["req_id"] == json!(3),
        &format!(
            "req_id 重启后必须继续累加（1→boom 2→3）, 实际 {}",
            r3["req_id"]
        ),
    )?;
    m.shutdown().await.map_err(|e| format!("shutdown: {e}"))
}

/// 评审轮 1 Important 钉红用例：判死与重启的竞态。
///
/// 时序（旧实现必现）：boom 杀 JVM1 → writer 的 EPIPE 判死与 reader 的 EOF 判死是
/// 两次独立 declare_dead；两者之间，在途 echo 请求持锁 spawn JVM2（毫秒级）→ 第二次
/// 判死不认世代，把健康 JVM2 也 take/kill（世代多跳、echo 被假性 drain）。
/// 不变量（修复后确定成立）：每轮恰好 +1 世代；窗口关闭后探针必须成功。
async fn crash_concurrent_restart_exactly_one_death_per_boom() -> Result<(), String> {
    use std::sync::Arc;
    let m = Arc::new(mgr("crash")); // 收到 {"boom":true} 的请求即 exit(1)
    let g0 = m.generation();
    let warm: Value = m
        .request("query", json!({"sql":"warm"}))
        .await
        .map_err(|e| format!("warm: {e}"))?;
    assert2(warm["req_id"] == json!(1), "warm req_id")?;
    const ROUNDS: u64 = 5;
    for round in 1..=ROUNDS {
        // 1 个 boom + 8 个 echo 同时发出。echo 各带 32KB 载荷（合计 ~256KB > 64KB
        // 管道缓冲）：writer 注定阻塞在写中途 → JVM1 死于 boom 时 writer 吃 EPIPE 判死 +
        // reader EOF 判死 = 两次独立 declare_dead 竞争 inner 锁；echo 队列随后 spawn JVM2。
        // 旧实现（判死不认世代、无条件 bump/drain）：两次判死各 +1 → 每轮 +2，且第二个
        // declare_dead 会 take/kill 健康的 JVM2、假性 drain echo。本用例钉住不变量。
        let big = "x".repeat(32 * 1024);
        let mut set = tokio::task::JoinSet::new();
        for i in 0..9u8 {
            let m = m.clone();
            let big = big.clone();
            set.spawn(async move {
                if i == 0 {
                    m.request("query", json!({"boom":true})).await
                } else {
                    m.request("query", json!({"sql":big})).await
                }
            });
        }
        let (mut oks, mut lost) = (0usize, 0usize);
        while let Some(r) = set.join_next().await {
            match r.map_err(|e| format!("join: {e}"))? {
                Ok(_) => oks += 1,
                Err(e) => {
                    // 合法失败只有一种：帧确实发给了死掉的 JVM1，被 drain。
                    assert2(
                        e.contains("sidecar lost"),
                        &format!("round {round}: 只允许 sidecar lost, 实际 {e}"),
                    )?;
                    lost += 1;
                }
            }
        }
        println!("[round {round}: {oks} ok / {lost} lost]");
        // 不变量 1：每轮恰好一次死亡（stale 判死杀健康 JVM2 → 每轮 +2）。
        assert2(
            m.generation() == g0 + round,
            &format!(
                "round {round}: generation {} != {}（stale 判死多跳 / 健康 JVM 被误杀）",
                m.generation(),
                g0 + round
            ),
        )?;
        // 不变量 2：窗口关闭后探针必须成功（健康 JVM 在位，请求不被误丢）。
        let probe: Value = tokio::time::timeout(BOUND, m.request("query", json!({"sql":"probe"})))
            .await
            .map_err(|_| format!("round {round}: probe deadline"))?
            .map_err(|e| format!("round {round} probe: {e}"))?;
        assert2(
            probe["req_id"].as_u64().unwrap_or(0) > 1,
            "probe req_id 应继续累加",
        )?;
    }
    let _ = m.shutdown().await;
    Ok(())
}

async fn hang_hello_bounded_wait() -> Result<(), String> {
    // hang：假进程什么都不打印——hello 有界等待超时后必须报错（且不挂死测试）。
    let m = cfg_mgr("hang", Duration::from_secs(2), true);
    let e = tokio::time::timeout(BOUND, m.request("query", json!({})))
        .await
        .map_err(|_| "deadline: hello 有界等待不得挂死")?
        .expect_err("hello 超时必须报错");
    assert2(e.contains("hello"), &format!("实际: {e}"))?;
    assert2(e.contains("timed out"), &format!("实际: {e}"))
}

async fn clean_eof_exit() -> Result<(), String> {
    // EOF 退出契约（spec §5.1 硬性）：stdin 关闭 → 假进程有序退出 0。
    let m = mgr("echo");
    let _: Value = m
        .request("query", json!({"sql":"x"}))
        .await
        .map_err(|e| format!("request: {e}"))?;
    m.shutdown().await.map_err(|e| format!("clean EOF: {e}"))?;
    m.shutdown().await.map_err(|e| format!("幂等: {e}"))
}

/// java 缺失：错误文案必须含搜索顺序（cfg → JAVA_HOME → PATH）。
/// cfg.java_bin 给不存在的路径（落到 JAVA_HOME/PATH，均已清空）；顺序执行
/// （main 单线程）+ 立即恢复：env 修改窗口内无并发用例。
async fn java_missing_error_lists_search_order() -> Result<(), String> {
    let saved_home = std::env::var_os("JAVA_HOME");
    let saved_path = std::env::var_os("PATH");
    unsafe {
        std::env::remove_var("JAVA_HOME");
        std::env::set_var("PATH", "/nonexistent-oj-jdbc-test-dir");
    }
    // cfg.java_bin 给不存在的路径 → 落到 JAVA_HOME/PATH（均已清空），三者皆无。
    let m = SidecarMgr::new(SidecarCfg {
        java_bin: Some("/nonexistent-oj-jdbc-test/java".into()),
        java_opts: vec![],
        bridge_jar: PathBuf::from("/nonexistent/bridge.jar"),
        drivers_dir: PathBuf::from("/nonexistent/drivers"),
        hello_timeout: Duration::from_secs(2),
        env: vec![],
        log: None,
    });
    let r = tokio::time::timeout(BOUND, m.request("query", json!({}))).await;
    unsafe {
        match saved_home {
            Some(v) => std::env::set_var("JAVA_HOME", v),
            None => std::env::remove_var("JAVA_HOME"),
        }
        match saved_path {
            Some(v) => std::env::set_var("PATH", v),
            None => std::env::remove_var("PATH"),
        }
    }
    let e = r
        .map_err(|_| "deadline")?
        .expect_err("java 缺失必须报结构化错误");
    assert2(e.contains("java_bin"), &format!("须含搜索顺序, 实际: {e}"))?;
    assert2(e.contains("JAVA_HOME"), &format!("须含搜索顺序, 实际: {e}"))?;
    assert2(e.contains("PATH"), &format!("须含搜索顺序, 实际: {e}"))
}

fn assert2(cond: bool, what: &str) -> Result<(), String> {
    if cond {
        Ok(())
    } else {
        Err(format!("assert failed: {what}"))
    }
}

// ---- 假 java 夹具（本二进制自重现；main 按 OJ_FAKE_JAVA 分流到此处）----

fn fake_java_main() -> ! {
    let scenario = std::env::var("OJ_FAKE_JAVA").unwrap_or_default();
    match scenario.as_str() {
        "echo" => fake_echo(false),
        "flood" => {
            use std::io::Write;
            let mut err = std::io::stderr();
            for _ in 0..200 {
                // 400KB >> 64KB 管道缓冲：宿主不 drain 会楔死整个 sidecar（§5.1 楔形死锁）。
                let _ = writeln!(err, "fake-java-flood-{}", "x".repeat(1980));
            }
            let _ = err.flush();
            fake_echo(false)
        }
        "crash" => fake_echo(true),
        "badproto" => {
            use std::io::Write;
            println!("{{\"hello\":{{\"proto\":999,\"bridge\":\"9.9.9\"}}}}");
            let _ = std::io::stdout().flush();
            fake_echo(false)
        }
        "hang" => loop {
            std::thread::park();
        },
        other => {
            eprintln!("fake-java: unknown scenario {other}");
            std::process::exit(2);
        }
    }
}

/// echo 循环：hello → 逐行读请求 → 回显 `{"id":..,"ok":{"op":..,"req_id":..,"args":{..}}}`；
/// boom 模式收到含 `"boom":true` 的行即 exit(1)。stdin EOF → 退出 0（§5.1 EOF 契约）。
fn fake_echo(boom: bool) -> ! {
    use std::io::{BufRead, Write};
    println!("{{\"hello\":{{\"proto\":1,\"bridge\":\"0.1.0\"}}}}");
    let _ = std::io::stdout().flush();
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if boom && line.contains("\"boom\":true") {
            std::process::exit(1);
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = v["id"].as_u64().unwrap_or(0);
        let op = v["op"].as_str().unwrap_or("").to_string();
        let mut args = v.as_object().cloned().unwrap_or_default();
        args.remove("id");
        args.remove("op");
        println!(
            "{}",
            json!({"id": id, "ok": {"op": op, "req_id": id, "args": args}})
        );
        let _ = std::io::stdout().flush();
    }
    std::process::exit(0);
}
