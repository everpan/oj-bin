//! T10 vtable 胶水集成用例（spec §5.3 帧序列）。harness=false 自有 main：
//! - env `OJ_FAKE_JAVA=vtable` → **本二进制自重现为假 java**（spawn current_exe()，
//!   跨平台含 Windows CI），按协议形状响应，收发帧逐行记进 `OJ_FAKE_LOG`；
//! - 否则驱动真实 vtable 槽位跑单场景，结束后按日志断言帧序列。
//!
//! 覆盖（BDD story）：connect → query → exec → begin → tx_exec → tx_rollback → close
//! （在途查询时非阻塞返回，disconnect 排队在途 op 之后）→ 崩溃世代失效
//! （stale handle 报 reconnect，绝不落到新 JVM）→ 重连恢复。
//! jsint 通道：$oj$i64 → JSON 数字；$oj$u64 / 超 i64 裸整数 → "$oj$u64:<十进制>" 字符串。
//!
//! 同步纪律：全部用有界等待（timeout/deadline），禁止固定 sleep 当同步。

use oj_db_jdbc::sidecar::SidecarCfg;
use oj_db_jdbc::{__test_init, __test_shutdown, vt};
use oj_plugin_ffi::{FfiFuture, RString};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// 用例总等待上界（CI 慢机余量），是 deadline 不是同步手段。
const BOUND: Duration = Duration::from_secs(20);

fn main() {
    if std::env::var_os("OJ_FAKE_JAVA").is_some() {
        fake_java_main();
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");
    let r = rt.block_on(vtable_frame_sequence());
    let _ = rt.block_on(__test_shutdown());
    match r {
        Ok(()) => println!("vtable: all checks passed"),
        Err(e) => {
            println!("vtable: FAIL: {e}");
            std::process::exit(1);
        }
    }
}

fn assert2(cond: bool, what: &str) -> Result<(), String> {
    if cond {
        Ok(())
    } else {
        Err(format!("assert failed: {what}"))
    }
}

fn v(b: &[u8]) -> Value {
    serde_json::from_slice(b).expect("响应应为合法 JSON")
}

async fn vtable_frame_sequence() -> Result<(), String> {
    let log = std::env::temp_dir().join(format!("oj-jdbc-vtable-{}.log", std::process::id()));
    let _ = std::fs::remove_file(&log);
    __test_init(SidecarCfg {
        java_bin: Some(
            std::env::current_exe()
                .expect("current_exe")
                .to_string_lossy()
                .into_owned(),
        ),
        java_opts: vec![],
        bridge_jar: PathBuf::from("/nonexistent/bridge.jar"),
        drivers_dir: PathBuf::from("/nonexistent/drivers"),
        hello_timeout: Duration::from_secs(10),
        env: vec![
            ("OJ_FAKE_JAVA".into(), "vtable".into()),
            ("OJ_FAKE_LOG".into(), log.to_string_lossy().into_owned()),
        ],
        log: None,
    });

    // ---- connect（JSON 信封：url/user/password/props 全量上送）----
    let mut f = vt::connect(RString::from(
        r#"jdbc:{"url":"jdbc:h2:mem:t","user":"u","password":"p","props":{"k":"v"}}"#,
    ));
    let h = v(&drive(&mut f).await?)["handle"]
        .as_u64()
        .ok_or("connect ok 缺 handle")?;
    assert2(h == 1, "首个插件 handle = 1")?;

    // ---- query：jsint 通道（i64 数字透传 / u64 与超 i64 裸整数预字符串化）----
    let mut f = vt::query(
        h,
        RString::from("select 1"),
        RString::from(
            r#"[1,"a",{"$oj$i64":"42"},{"$oj$u64":"18446744073709551615"},9223372036854775808]"#,
        ),
    );
    assert2(
        v(&drive(&mut f).await?) == json!([{"n": 1}]),
        "query 行透传",
    )?;

    // ---- exec：受影响行数 ----
    let mut f = vt::exec(
        h,
        RString::from("insert into t values (?)"),
        RString::from("[2]"),
    );
    assert2(v(&drive(&mut f).await?) == json!(3), "exec 受影响行数")?;

    // ---- begin / tx_exec / tx_rollback：tx 五槽转发 ----
    let mut f = vt::begin(h);
    assert2(
        v(&drive(&mut f).await?) == json!({"tx_id": 5}),
        "begin tx_id",
    )?;
    let mut f = vt::tx_exec(
        h,
        5,
        RString::from("update t set a=?"),
        RString::from("[3]"),
    );
    assert2(v(&drive(&mut f).await?) == json!(3), "tx_exec")?;
    let mut f = vt::tx_rollback(h, 5);
    assert2(v(&drive(&mut f).await?) == json!({}), "tx_rollback")?;

    // ---- close 在途非阻塞：__hold__ 查询挂起，close 立即返回；
    //      假 java 的 per-conn FIFO 语义 = 收到 disconnect 才收尾在途查询 ----
    let mut qf = vt::query(h, RString::from("__hold__"), RString::from("[]"));
    assert2((qf.poll)(qf.state) == 0, "首 poll 调度（pending）")?;
    wait_log(&log, |l| l.contains("__hold__"), "假 java 收到 __hold__ 帧").await?;
    assert2(
        !std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .any(|l| l.starts_with("< ") && l.contains(r#""n":99"#)),
        "close 前 hold 查询不得有响应",
    )?;
    vt::close(h); // 同步槽：非阻塞，立即返回；disconnect 排队在途 op 之后
    wait_log(
        &log,
        |l| l.contains(r#""n":99"#),
        "hold 查询在 disconnect 后收尾",
    )
    .await?;
    assert2(
        v(&drive(&mut qf).await?) == json!([{"n": 99}]),
        "hold 查询最终完成",
    )?;
    // close 后句柄失效。
    let mut f = vt::query(h, RString::from("select 2"), RString::from("[]"));
    let e = drive(&mut f)
        .await
        .expect_err("close 后查询必须报 unknown handle");
    assert2(e.contains("unknown handle"), &format!("实际: {e}"))?;

    // ---- 崩溃世代失效：die → 在途报 sidecar lost；旧 handle 报 reconnect；
    //      下一次 connect 触发重启并可正常服务 ----
    let mut f = vt::connect(RString::from("jdbc:h2:mem:u")); // 裸 URL 形态透传
    let h2 = v(&drive(&mut f).await?)["handle"]
        .as_u64()
        .ok_or("connect2 缺 handle")?;
    let mut f = vt::query(h2, RString::from("__die__"), RString::from("[]"));
    let e = drive(&mut f).await.expect_err("die 必须在途报错");
    assert2(e.contains("sidecar lost"), &format!("实际: {e}"))?;
    let mut f = vt::query(h2, RString::from("select 9"), RString::from("[]"));
    let e = drive(&mut f).await.expect_err("旧世代句柄必须拒绝");
    assert2(e.contains("reconnect"), &format!("实际: {e}"))?;
    let mut f = vt::connect(RString::from("jdbc:h2:mem:u3"));
    let h3 = v(&drive(&mut f).await?)["handle"]
        .as_u64()
        .ok_or("connect3 缺 handle")?;
    assert2(h3 == 3, "插件 handle 单调递增")?;
    let mut f = vt::query(h3, RString::from("select 3"), RString::from("[]"));
    assert2(
        v(&drive(&mut f).await?) == json!([{"n": 1}]),
        "重启后查询恢复",
    )?;

    // ---- 帧序列总断言（spec §5.3）：请求帧逐个比形 + id 单调 + 响应齐套 + FIFO 序 ----
    assert_frames(&log).await
}

/// 日志行形态：`> <请求帧>` / `< <响应帧>`（NDJSON 各一行，跨 JVM 重启连续追加）。
async fn assert_frames(log: &Path) -> Result<(), String> {
    // 有界轮询等待假 java 把最后一帧落盘（shutdown 前已 drive 全部 future，响应已读，
    // 这里等的是假 java 进程侧 flush——EOF 退出契约保证进程退出前全部落盘）。
    wait_log(log, |l| l.contains(r#""sql":"select 3""#), "末帧落盘").await?;
    let s = std::fs::read_to_string(log).map_err(|e| format!("read log: {e}"))?;
    let mut reqs: Vec<Value> = Vec::new();
    let mut resps: Vec<Value> = Vec::new();
    let mut lines: Vec<&str> = Vec::new();
    for l in s.lines() {
        let Some(rest) = l.strip_prefix("> ").or_else(|| l.strip_prefix("< ")) else {
            continue;
        };
        lines.push(l);
        let val: Value =
            serde_json::from_str(rest).map_err(|e| format!("log 帧 JSON: {e}: {l}"))?;
        if l.starts_with("> ") {
            reqs.push(val);
        } else {
            resps.push(val);
        }
    }
    let expect = vec![
        json!({"id":1,"op":"connect","url":"jdbc:h2:mem:t","user":"u","password":"p","props":{"k":"v"}}),
        json!({"id":2,"op":"query","handle":11,"sql":"select 1","params":[1,"a",42,"$oj$u64:18446744073709551615","$oj$u64:9223372036854775808"]}),
        json!({"id":3,"op":"exec","handle":11,"sql":"insert into t values (?)","params":[2]}),
        json!({"id":4,"op":"begin","handle":11}),
        json!({"id":5,"op":"tx_exec","handle":11,"tx_id":5,"sql":"update t set a=?","params":[3]}),
        json!({"id":6,"op":"tx_rollback","handle":11,"tx_id":5}),
        json!({"id":7,"op":"query","handle":11,"sql":"__hold__","params":[]}),
        json!({"id":8,"op":"disconnect","handle":11}),
        json!({"id":9,"op":"connect","url":"jdbc:h2:mem:u"}),
        json!({"id":10,"op":"query","handle":11,"sql":"__die__","params":[]}),
        json!({"id":11,"op":"connect","url":"jdbc:h2:mem:u3"}),
        json!({"id":12,"op":"query","handle":11,"sql":"select 3","params":[]}),
    ];
    assert2(
        reqs == expect,
        &format!(
            "请求帧序列不符（spec §5.3）:\n实际 {}\n期望 {}",
            json!(reqs),
            json!(expect)
        ),
    )?;
    // 除 die（id=10，JVM 随其死亡无响应）外，每个请求都有同 id 响应帧。
    for id in [1u64, 2, 3, 4, 5, 6, 7, 8, 9, 11, 12] {
        assert2(
            resps.iter().any(|r| r["id"] == json!(id)),
            &format!("缺 id={id} 的响应帧"),
        )?;
    }
    assert2(
        !resps.iter().any(|r| r["id"] == json!(10)),
        "die 帧（id=10）不得有响应",
    )?;
    // FIFO 序：disconnect 收到（id=8）→ hold 查询响应（id=7）→ disconnect 响应（id=8）。
    // （帧内键序按 serde_json 排序，位置匹配用键值子串，不假设序。）
    let pos = |prefix: &str, pats: &[&str]| -> Result<usize, String> {
        lines
            .iter()
            .position(|l| l.starts_with(prefix) && pats.iter().all(|p| l.contains(p)))
            .ok_or_else(|| format!("日志缺: {prefix} {pats:?}"))
    };
    let (p_disc, p_hold_resp, p_disc_resp) = (
        pos("> ", &["\"id\":8,", "\"op\":\"disconnect\""])?,
        pos("< ", &["\"id\":7,", "\"n\":99"])?,
        pos("< ", &["\"id\":8,", "\"ok\":{}"])?,
    );
    assert2(
        p_disc < p_hold_resp && p_hold_resp < p_disc_resp,
        "close 在途语义：disconnect 排队在途 op 之后（FIFO 序不符）",
    )
}

async fn wait_log(log: &Path, pred: impl Fn(&str) -> bool, what: &str) -> Result<(), String> {
    let deadline = Instant::now() + BOUND;
    loop {
        let s = std::fs::read_to_string(log).unwrap_or_default();
        if s.lines().any(&pred) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("deadline 等待日志条件: {what}"));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// FfiFuture → 测试异步桥（等价 core await_ffi 的 poll 轮询；与 oj-db-mysql 同款）。
async fn drive(fut: &mut FfiFuture) -> Result<Vec<u8>, String> {
    let deadline = Instant::now() + BOUND;
    loop {
        match (fut.poll)(fut.state) {
            0 => {
                if Instant::now() >= deadline {
                    (fut.free)(fut.state);
                    fut.state = std::ptr::null_mut();
                    return Err("ffi drive timeout".into());
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            code => {
                let r = (fut.take)(fut.state);
                (fut.free)(fut.state);
                fut.state = std::ptr::null_mut();
                return match (code, std::result::Result::from(r)) {
                    (1, Ok(b)) => Ok(b.iter().copied().collect()),
                    (_, Err(e)) => Err(e[..].to_string()),
                    _ => Err("ffi poll reported error but take succeeded".into()),
                };
            }
        }
    }
}

// ---- 假 java 夹具（本二进制自重现；main 按 OJ_FAKE_JAVA 分流到此处）----

fn fake_java_main() -> ! {
    match std::env::var("OJ_FAKE_JAVA").unwrap_or_default().as_str() {
        "vtable" => fake_vtable(),
        other => {
            eprintln!("fake-java: unknown scenario {other}");
            std::process::exit(2);
        }
    }
}

/// 协议形状响应：connect→{"handle":11}；query→{"rows":[{"n":1}]}；exec→3；
/// begin→{"tx_id":5}；tx_* 同形；__hold__ 挂起至 disconnect（per-conn FIFO 语义：
/// disconnect 先等在途 op 收尾）；__die__ 直接 exit(1) 模拟 JVM 猝死。
fn fake_vtable() -> ! {
    use std::io::{BufRead, Write};
    println!("{{\"hello\":{{\"proto\":1,\"bridge\":\"0.1.0\"}}}}");
    let _ = std::io::stdout().flush();
    let log_path = std::env::var("OJ_FAKE_LOG").expect("OJ_FAKE_LOG");
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .expect("open OJ_FAKE_LOG");
    let mut held: Option<u64> = None;
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.is_empty() {
            continue;
        }
        let _ = writeln!(log, "> {line}");
        let _ = log.flush();
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = v["id"].as_u64().unwrap_or(0);
        let op = v["op"].as_str().unwrap_or_default();
        match op {
            "connect" => respond(&mut log, &json!({"id": id, "ok": {"handle": 11}})),
            "query" if v["sql"].as_str() == Some("__die__") => std::process::exit(1),
            "query" if v["sql"].as_str() == Some("__hold__") => held = Some(id),
            "query" | "tx_query" => {
                respond(&mut log, &json!({"id": id, "ok": {"rows": [{"n": 1}]}}))
            }
            "exec" | "tx_exec" => respond(&mut log, &json!({"id": id, "ok": 3})),
            "begin" => respond(&mut log, &json!({"id": id, "ok": {"tx_id": 5}})),
            "tx_commit" | "tx_rollback" => respond(&mut log, &json!({"id": id, "ok": {}})),
            "disconnect" => {
                if let Some(hid) = held.take() {
                    // FIFO：在途 op 先行收尾，disconnect 响应排最后。
                    respond(&mut log, &json!({"id": hid, "ok": {"rows": [{"n": 99}]}}));
                }
                respond(&mut log, &json!({"id": id, "ok": {}}));
            }
            other => respond(
                &mut log,
                &json!({"id": id, "err": format!("unknown op: {other}")}),
            ),
        }
    }
    std::process::exit(0);
}

fn respond(log: &mut std::fs::File, val: &Value) {
    use std::io::Write;
    println!("{val}");
    let _ = std::io::stdout().flush();
    let _ = writeln!(log, "< {val}");
    let _ = log.flush();
}
