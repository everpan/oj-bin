//! T12 H2 真实 JVM 冒烟腿（env-gated）：真 SidecarMgr spawn → 真 NDJSON 管道 → 真
//! H2 驱动（仅放临时 driversDir，不进 -cp——DriverShim/URLClassLoader 路径必须真测，
//! T2 复盘裁决）。覆盖：connect（信封 DSN）→ golden 类型往返（fixtures/types.json，
//! 19 列含整数边界/ARRAY，$oj$ 标记按 RowCodec 线上形态断言）→ exec 受影响行数 →
//! begin/tx_query/tx_exec/tx_rollback/tx_commit → stream 250 行批边界(100/100/50)
//! + done + close 幂等 → cancel 可达语义 → close → 有序停机。
//!
//! 环境门槛（缺一即诚实 skip，指引见 skip() 文案）：
//! - `OJ_JDBC_BRIDGE_JAR`：bridge fat jar（`cd java/jdbc-bridge && ./gradlew shadowJar`）
//! - `OJ_JDBC_H2_JAR`：H2 驱动 jar（Maven 中央 com.h2database:h2:2.3.232）
//! - PATH/JAVA_HOME 可解析 `java`
//!
//! H2 已知局限（阶段 1 小结登记）：embedded 在 executeQuery 全量求值——
//! next 在途 cancel 分支不可达；cancel 帧被接受但 Statement.cancel 对已完成
//! 求值是 no-op（next 照常吐行）。cancelled Err 映射由 fake-java 腿（T11）覆盖，
//! 真 JVM 中断语义 defer 至 T14（达梦）。
//! 同步纪律：有界 deadline 等待，无固定 sleep。

use oj_db_jdbc::sidecar::SidecarCfg;
use oj_db_jdbc::{__test_init, __test_shutdown, vt};
use oj_plugin_ffi::{FfiFuture, RString};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// 用例总等待上界（真 JVM 启动余量），是 deadline 不是同步手段。
const BOUND: Duration = Duration::from_secs(60);

fn v(b: &[u8]) -> Value {
    serde_json::from_slice(b).expect("响应应为合法 JSON")
}

/// PATH/JAVA_HOME 粗解析 java（与 SidecarMgr 的解析顺序同序，仅为诚实 skip 判定）。
fn java_available() -> bool {
    if let Some(jh) = std::env::var_os("JAVA_HOME")
        && Path::new(&jh).join("bin/java").exists()
    {
        return true;
    }
    let exe = if cfg!(windows) { "java.exe" } else { "java" };
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(exe).is_file()))
        .unwrap_or(false)
}

fn skip(msg: &str) {
    eprintln!(
        "h2_smoke: SKIP——{msg}\n\
         获取指引：\n\
         1. bridge jar: cd java/jdbc-bridge && ./gradlew shadowJar\n\
            然后 export OJ_JDBC_BRIDGE_JAR=$PWD/java/jdbc-bridge/build/libs/oj-jdbc-bridge.jar\n\
         2. H2 jar: 自 Maven 中央下载 com.h2database:h2:2.3.232（如\n\
            https://repo1.maven.org/maven2/com/h2database/h2/2.3.232/h2-2.3.232.jar）\n\
            然后 export OJ_JDBC_H2_JAR=<下载路径>\n\
         3. 需 PATH 或 JAVA_HOME 可解析 java"
    );
}

#[test]
fn h2_smoke() {
    let bridge = std::env::var_os("OJ_JDBC_BRIDGE_JAR").map(PathBuf::from);
    let h2 = std::env::var_os("OJ_JDBC_H2_JAR").map(PathBuf::from);
    let (Some(bridge), Some(h2)) = (bridge, h2) else {
        skip("缺 OJ_JDBC_BRIDGE_JAR / OJ_JDBC_H2_JAR");
        return;
    };
    if !bridge.is_file() {
        skip("OJ_JDBC_BRIDGE_JAR 指向的文件不存在");
        return;
    }
    if !h2.is_file() {
        skip("OJ_JDBC_H2_JAR 指向的文件不存在");
        return;
    }
    if !java_available() {
        skip("PATH/JAVA_HOME 均解析不到 java");
        return;
    }

    // 夹具纪律（T2 教训）：H2 jar 只进临时 driversDir（cfg 透传），绝不进 -cp。
    let drivers = std::env::temp_dir().join(format!("oj-jdbc-h2-drivers-{}", std::process::id()));
    std::fs::create_dir_all(&drivers).expect("建临时 drivers 目录");
    std::fs::copy(&h2, drivers.join("h2.jar")).expect("拷贝 H2 jar 进 driversDir");

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");
    let r = rt.block_on(smoke(&bridge, &drivers));
    let _ = rt.block_on(__test_shutdown());
    let _ = std::fs::remove_dir_all(&drivers);
    r.unwrap_or_else(|e| panic!("h2_smoke: {e}"));
}

/// select count(*) 首行 n 列（事务可见性断言的共用件）。
async fn count(f: &mut FfiFuture) -> Result<i64, String> {
    let rows = v(&drive(f).await?);
    rows[0]["n"]
        .as_i64()
        .ok_or_else(|| "count 缺 n".to_string())
}

async fn smoke(bridge: &Path, drivers: &Path) -> Result<(), String> {
    __test_init(SidecarCfg {
        java_bin: None,
        java_opts: vec![],
        bridge_jar: bridge.to_path_buf(),
        drivers_dir: drivers.to_path_buf(),
        hello_timeout: Duration::from_secs(30),
        env: vec![],
        log: None,
    });

    // ---- connect：信封 DSN（url + props 全量上送）----
    let mut f = vt::connect(RString::from(
        r#"jdbc:{"url":"jdbc:h2:mem:smoke","props":{"DB_CLOSE_DELAY":"-1"}}"#,
    ));
    let h = v(&drive(&mut f).await?)["handle"]
        .as_u64()
        .ok_or("connect ok 缺 handle")?;
    assert2(h == 1, "首个插件 handle = 1")?;

    // ---- golden 类型往返：fixtures/types.json 19 行驱动 DDL + 填充 + 回读断言 ----
    // 消费契约：每行 = 一列（jdbc_type/ddl/input_sql 为行内元数据，非独立行）。
    let rows: Vec<Value> =
        serde_json::from_slice(include_bytes!("../fixtures/types.json")).expect("golden JSON");
    assert2(
        rows.len() == 19,
        &format!("golden 行数 19，实际 {}", rows.len()),
    )?;
    // 列名引号包裹：H2 对大写未引标识符（getColumnLabel 回 "C0"），引号保大小写。
    let col = |i: usize| format!("\"c{i}\"");
    let ddl: Vec<String> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let ty = r["ddl"]
                .as_str()
                .or_else(|| r["jdbc_type"].as_str())
                .expect("golden 行缺 ddl/jdbc_type");
            format!("{} {ty}", col(i))
        })
        .collect();
    let mut f = vt::exec(
        h,
        RString::from(&*format!("create table golden ({})", ddl.join(", "))),
        RString::from("[]"),
    );
    assert2(
        v(&drive(&mut f).await?) == json!(0),
        "DDL exec 受影响行数 0",
    )?;

    // 填充：input_sql 行内联 SQL 表达式（无绑参），其余按序绑 input。
    let placeholders: Vec<String> = rows
        .iter()
        .map(|r| match r.get("input_sql") {
            Some(sql) => sql.as_str().expect("input_sql 为字符串").to_string(),
            None => "?".to_string(),
        })
        .collect();
    let params: Vec<Value> = rows
        .iter()
        .filter(|r| r.get("input_sql").is_none())
        .map(|r| r["input"].clone())
        .collect();
    let mut f = vt::exec(
        h,
        RString::from(&*format!(
            "insert into golden values ({})",
            placeholders.join(", ")
        )),
        RString::from(
            serde_json::to_string(&params)
                .expect("params JSON")
                .as_str(),
        ),
    );
    assert2(
        v(&drive(&mut f).await?) == json!(1),
        "insert exec 受影响行数 1",
    )?;

    let select_list: Vec<String> = (0..rows.len()).map(col).collect();
    let mut f = vt::query(
        h,
        RString::from(&*format!("select {} from golden", select_list.join(", "))),
        RString::from("[]"),
    );
    let got = v(&drive(&mut f).await?);
    let got_row = got
        .as_array()
        .and_then(|a| a.first())
        .ok_or("golden 回读缺行")?;
    for (i, r) in rows.iter().enumerate() {
        let name = format!("c{i}"); // 回读键：引号只存在于 SQL 文本，label 本身无引号
        assert2(
            got_row.get(&name) == Some(&r["expect"]),
            &format!(
                "golden {name} 往返不符: 实际 {} 期望 {}",
                got_row.get(&name).unwrap(),
                r["expect"]
            ),
        )?;
    }

    // ---- 事务：begin → tx_query（见未提交基线）→ tx_exec → rollback 不留痕；
    //      再 begin → tx_exec → commit 留痕 ----
    let mut f = vt::query(
        h,
        RString::from("select count(*) as \"n\" from golden"),
        RString::from("[]"),
    );
    let base = count(&mut f).await?;
    assert2(base == 1, "golden 表基线 1 行")?;

    let mut f = vt::begin(h);
    let tx1 = v(&drive(&mut f).await?)["tx_id"]
        .as_u64()
        .ok_or("begin 缺 tx_id")?;
    let mut f = vt::tx_query(
        h,
        tx1,
        RString::from("select count(*) as \"n\" from golden"),
        RString::from("[]"),
    );
    assert2(count(&mut f).await? == base, "tx_query 见未提交基线")?;
    let mut f = vt::tx_exec(
        h,
        tx1,
        RString::from("insert into golden(\"c0\") values (7)"),
        RString::from("[]"),
    );
    assert2(v(&drive(&mut f).await?) == json!(1), "tx_exec insert 1 行")?;
    let mut f = vt::tx_rollback(h, tx1);
    assert2(v(&drive(&mut f).await?) == json!({}), "tx_rollback ok")?;
    let mut f = vt::query(
        h,
        RString::from("select count(*) as \"n\" from golden"),
        RString::from("[]"),
    );
    assert2(count(&mut f).await? == base, "rollback 后行数复原")?;

    let mut f = vt::begin(h);
    let tx2 = v(&drive(&mut f).await?)["tx_id"]
        .as_u64()
        .ok_or("begin2 缺 tx_id")?;
    assert2(tx2 > tx1, "tx_id 单调递增")?;
    let mut f = vt::tx_exec(
        h,
        tx2,
        RString::from("insert into golden(\"c0\") values (8)"),
        RString::from("[]"),
    );
    drive(&mut f).await?;
    let mut f = vt::tx_commit(h, tx2);
    assert2(v(&drive(&mut f).await?) == json!({}), "tx_commit ok")?;
    let mut f = vt::query(
        h,
        RString::from("select count(*) as \"n\" from golden"),
        RString::from("[]"),
    );
    assert2(count(&mut f).await? == base + 1, "commit 后行数 +1")?;

    // ---- stream：250 行批边界 100/100/50 + done；close 幂等 ----
    let mut f = vt::exec(
        h,
        RString::from("create table s250 (n int primary key)"),
        RString::from("[]"),
    );
    drive(&mut f).await?;
    let tuples: String = (0..250)
        .map(|n| format!("({n})"))
        .collect::<Vec<_>>()
        .join(",");
    let mut f = vt::exec(
        h,
        RString::from(&*format!("insert into s250 values {tuples}")),
        RString::from("[]"),
    );
    assert2(v(&drive(&mut f).await?) == json!(250), "stream 夹具 250 行")?;

    let mut f = vt::stream_open(
        h,
        RString::from("select n from s250 order by n"),
        RString::from("[]"),
    );
    let sid = v(&drive(&mut f).await?)["stream_id"]
        .as_u64()
        .ok_or("stream_open 缺 stream_id")?;
    let mut got_rows = 0usize;
    let mut batches: Vec<usize> = Vec::new();
    loop {
        let mut f = vt::stream_next(h, sid);
        let env = v(&drive(&mut f).await?);
        if env.get("done").and_then(Value::as_bool) == Some(true) {
            break;
        }
        let n = env["rows"].as_array().ok_or("next 信封缺 rows")?.len();
        batches.push(n);
        got_rows += n;
        assert2(n <= 100, "单批 ≤100 行")?;
        if batches.len() > 10 {
            return Err("批循环未收敛".into());
        }
    }
    assert2(
        batches == [100, 100, 50] && got_rows == 250,
        &format!("批边界 100/100/50，实际 {batches:?} 共 {got_rows} 行"),
    )?;
    // done 不自动关：显式 close 是唯一回收口；重入幂等（本地未知 = 已关，不发帧）。
    let mut f = vt::stream_close(h, sid);
    assert2(v(&drive(&mut f).await?) == json!({}), "stream_close ok")?;
    let mut f = vt::stream_close(h, sid);
    assert2(v(&drive(&mut f).await?) == json!({}), "stream_close 幂等")?;

    // ---- cancel（H2 可达语义，见文件头局限声明）：cancel 帧被接受、真 Statement.cancel
    //      调用；H2 已全量求值 → no-op，next 照常吐行、close 正常收口 ----
    let mut f = vt::stream_open(
        h,
        RString::from("select n from s250 order by n"),
        RString::from("[]"),
    );
    let sid2 = v(&drive(&mut f).await?)["stream_id"]
        .as_u64()
        .ok_or("cancel 腿 open 缺 stream_id")?;
    let mut f = vt::stream_cancel(h, sid2);
    assert2(v(&drive(&mut f).await?) == json!({}), "stream_cancel ok")?;
    let mut f = vt::stream_next(h, sid2);
    let env = v(&drive(&mut f).await?);
    assert2(
        env["rows"].as_array().is_some_and(|r| !r.is_empty()),
        "post-cancel next 照常吐行（H2 no-op 语义）",
    )?;
    let mut f = vt::stream_close(h, sid2);
    assert2(
        v(&drive(&mut f).await?) == json!({}),
        "post-cancel close ok",
    )?;

    // ---- close 连接：同步槽非阻塞；后续查询报 unknown handle ----
    vt::close(h);
    let mut f = vt::query(h, RString::from("select 1"), RString::from("[]"));
    let e = drive(&mut f)
        .await
        .expect_err("close 后查询必须报 unknown handle");
    assert2(e.contains("unknown handle"), &format!("实际: {e}"))?;

    println!("h2_smoke: all checks passed");
    Ok(())
}

fn assert2(cond: bool, what: &str) -> Result<(), String> {
    if cond {
        Ok(())
    } else {
        Err(format!("assert failed: {what}"))
    }
}

/// FfiFuture → 测试异步桥（与 tests/vtable.rs 同款有界轮询）。
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
