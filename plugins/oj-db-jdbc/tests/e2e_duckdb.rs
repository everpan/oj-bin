//! REQ-A4（v0.1.144）DuckDB 离线全链腿（env-gated）：真 JVM 桥 + 真 DuckDB 驱动，
//! **零服务端**（内存库 `jdbc:duckdb:`）——七方言中唯一可离线全链实证的腿，用于
//! 证明「连接 → DDL → 参数查询 → 类型分流 → 元数据摄取 → 事务 → stream 批边界」
//! 对第四形态方言端到端可用（上游 REQ-A4 验收第 1 条的平台侧对照）。
//!
//! 与 h2_smoke 的差异（探活纪律：不强迫一厂语义）：DuckDB 内存库**无鉴权**，
//! 口令脱敏子用例不成立，不跑；补 `information_schema` 元数据摄取子用例。
//!
//! 已知局限（调查报告风险 4，文档同步）：DuckDB 结果集全量内存，`setFetchSize`
//! 无流式效果——本腿只验证批边界形状与数据正确性，不承诺流式省内存。
//!
//! 环境门槛（缺一即诚实 skip，指引见 skip() 文案）：
//! - `OJ_JDBC_BRIDGE_JAR`：bridge fat jar（`cd java/jdbc-bridge && ./gradlew shadowJar`）
//! - `OJ_JDBC_DUCKDB_JAR`：DuckDB 驱动 jar（Maven 中央 org.duckdb:duckdb_jdbc），
//!   缺 env 时回落探测发行布局 `<root>/bin/plugins/*/jdbc/drivers/duckdb_jdbc-*.jar`
//! - PATH/JAVA_HOME 可解析 `java`

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

fn assert2(cond: bool, what: &str) -> Result<(), String> {
    if cond {
        Ok(())
    } else {
        Err(format!("assert failed: {what}"))
    }
}

/// FfiFuture → 测试异步桥（与 tests/h2_smoke.rs 同款有界轮询）。
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

/// 发行布局回落探测：`<root>/bin/plugins/<triple>/jdbc/drivers/duckdb_jdbc-*.jar`
/// （cargo xtask jdbc-drivers / 手工下载的落位处；取排序第一个命中）。
fn bundled_duckdb_jar() -> Option<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bin/plugins");
    let mut hits: Vec<PathBuf> = std::fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|e| e.path().join("jdbc").join("drivers"))
        .filter_map(|d| {
            std::fs::read_dir(&d)
                .ok()?
                .flatten()
                .map(|e| e.path())
                .find(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("duckdb_jdbc-") && n.ends_with(".jar"))
                })
        })
        .collect();
    hits.sort();
    hits.into_iter().next()
}

fn skip(msg: &str) {
    eprintln!(
        "e2e_duckdb: SKIP——{msg}\n\
         获取指引：\n\
         1. bridge jar: cd java/jdbc-bridge && ./gradlew shadowJar\n\
            然后 export OJ_JDBC_BRIDGE_JAR=$PWD/java/jdbc-bridge/build/libs/oj-jdbc-bridge.jar\n\
         2. DuckDB jar: cargo xtask jdbc-drivers 不含 DuckDB（81MB 自备），自 Maven 中央\n\
            下载 org.duckdb:duckdb_jdbc（如 https://repo1.maven.org/maven2/org/duckdb/\n\
            duckdb_jdbc/2.0.1.0/duckdb_jdbc-2.0.1.0.jar）；放进 bin/plugins/*/jdbc/\n\
            drivers/ 可享本腿的发行布局回落探测，或 export OJ_JDBC_DUCKDB_JAR=<路径>\n\
         3. 需 PATH 或 JAVA_HOME 可解析 java"
    );
}

#[test]
fn e2e_duckdb() {
    let bridge = std::env::var_os("OJ_JDBC_BRIDGE_JAR").map(PathBuf::from);
    let duck = std::env::var_os("OJ_JDBC_DUCKDB_JAR")
        .map(PathBuf::from)
        .or_else(bundled_duckdb_jar);
    let (Some(bridge), Some(duck)) = (bridge, duck) else {
        skip("缺 OJ_JDBC_BRIDGE_JAR / OJ_JDBC_DUCKDB_JAR（且发行布局无回落命中）");
        return;
    };
    if !bridge.is_file() {
        skip("OJ_JDBC_BRIDGE_JAR 指向的文件不存在");
        return;
    }
    if !duck.is_file() {
        skip("DuckDB jar 路径不存在");
        return;
    }
    if !java_available() {
        skip("PATH/JAVA_HOME 均解析不到 java");
        return;
    }

    // 夹具纪律（T2 教训）：驱动 jar 只进临时 driversDir，绝不进 -cp。
    let drivers = std::env::temp_dir().join(format!("oj-jdbc-duck-drivers-{}", std::process::id()));
    std::fs::create_dir_all(&drivers).expect("建临时 drivers 目录");
    std::fs::copy(&duck, drivers.join("duckdb.jar")).expect("拷贝 DuckDB jar 进 driversDir");

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");
    let r = rt.block_on(chain(&bridge, &drivers));
    let _ = rt.block_on(__test_shutdown());
    let _ = std::fs::remove_dir_all(&drivers);
    r.unwrap_or_else(|e| panic!("e2e_duckdb: {e}"));
}

/// select count(*) 首行 n 列。
async fn count(f: &mut FfiFuture) -> Result<i64, String> {
    let rows = v(&drive(f).await?);
    rows[0]["n"]
        .as_i64()
        .ok_or_else(|| "count 缺 n".to_string())
}

async fn chain(bridge: &Path, drivers: &Path) -> Result<(), String> {
    __test_init(SidecarCfg {
        java_bin: None,
        java_opts: vec![],
        bridge_jar: bridge.to_path_buf(),
        drivers_dir: drivers.to_path_buf(),
        hello_timeout: Duration::from_secs(30),
        env: vec![],
        log: None,
    });

    // ---- connect：内存库信封 DSN（无鉴权——口令脱敏子用例不成立，见文件头）----
    let mut f = vt::connect(RString::from(r#"jdbc:{"url":"jdbc:duckdb:"}"#));
    let h = v(&drive(&mut f).await?)["handle"]
        .as_u64()
        .ok_or("connect ok 缺 handle")?;

    // ---- DDL + 参数写读 + 类型分流（DECIMAL scale 语义跨厂一致契约的第四形态实证）----
    let mut f = vt::exec(
        h,
        RString::from(
            r#"create table oj_duck_e2e (id int primary key, name varchar(64), amount decimal(10,2), flag boolean)"#,
        ),
        RString::from("[]"),
    );
    let ddl_env = v(&drive(&mut f).await?);
    // DuckDB 驱动对 DDL 回 -1（Statement.SUCCESS_NO_INFO），DM/H2 回 0——跨厂
    // 差异按实际钉死：DDL 成功 = 无错误且计数 ∈ {0, -1}。
    assert2(
        ddl_env == json!(0) || ddl_env == json!(-1),
        &format!("DDL 无错误且计数 ∈ {{0,-1}}（实际 {ddl_env}）"),
    )?;
    let mut f = vt::exec(
        h,
        RString::from("insert into oj_duck_e2e values (?, ?, ?, ?)"),
        RString::from(r#"[42,"hello","123.45",true]"#),
    );
    assert2(v(&drive(&mut f).await?) == json!(1), "参数化 insert 1 行")?;
    let mut f = vt::query(
        h,
        RString::from("select id, name, amount, flag from oj_duck_e2e where id = ?"),
        RString::from("[42]"),
    );
    let row = v(&drive(&mut f).await?)[0].clone();
    assert2(
        row["id"] == json!(42),
        &format!("整数列回 number: {}", row["id"]),
    )?;
    assert2(row["name"] == json!("hello"), "varchar 往返")?;
    // scale>0 → string 保精度（DECIMAL 分流契约对第四形态方言成立）。
    assert2(
        row["amount"] == json!("123.45"),
        &format!("DECIMAL scale>0 回 string: {}", row["amount"]),
    )?;
    assert2(row["flag"] == json!(true), "boolean 往返")?;

    // ---- REQ-A4 验收第 1 条对照：information_schema 元数据摄取（catalog 摄取
    //      的平台侧最小形态——db.query 跑元数据查询即摄取可用）----
    let mut f = vt::query(
        h,
        RString::from(
            "select table_name from information_schema.tables where table_name = 'oj_duck_e2e'",
        ),
        RString::from("[]"),
    );
    let meta = v(&drive(&mut f).await?);
    assert2(
        meta.as_array().is_some_and(|a| {
            a.iter().any(|r| {
                r.as_object()
                    .is_some_and(|o| o.values().any(|val| val.as_str() == Some("oj_duck_e2e")))
            })
        }),
        &format!("information_schema 摄取应见夹具表: {meta}"),
    )?;

    // ---- 事务：rollback 不留痕 / commit 留痕 ----
    let mut f = vt::query(
        h,
        RString::from("select count(*) as \"n\" from oj_duck_e2e"),
        RString::from("[]"),
    );
    let base = count(&mut f).await?;
    let mut f = vt::begin(h);
    let tx1 = v(&drive(&mut f).await?)["tx_id"]
        .as_u64()
        .ok_or("begin 缺 tx_id")?;
    let mut f = vt::tx_exec(
        h,
        tx1,
        RString::from("insert into oj_duck_e2e values (900, 't', '1.00', false)"),
        RString::from("[]"),
    );
    assert2(v(&drive(&mut f).await?) == json!(1), "tx_exec insert 1")?;
    let mut f = vt::tx_rollback(h, tx1);
    assert2(v(&drive(&mut f).await?) == json!({}), "tx_rollback ok")?;
    let mut f = vt::query(
        h,
        RString::from("select count(*) as \"n\" from oj_duck_e2e"),
        RString::from("[]"),
    );
    assert2(count(&mut f).await? == base, "rollback 后行数复原")?;
    let mut f = vt::begin(h);
    let tx2 = v(&drive(&mut f).await?)["tx_id"]
        .as_u64()
        .ok_or("begin2 缺 tx_id")?;
    let mut f = vt::tx_exec(
        h,
        tx2,
        RString::from("insert into oj_duck_e2e values (901, 't', '1.00', false)"),
        RString::from("[]"),
    );
    drive(&mut f).await?;
    let mut f = vt::tx_commit(h, tx2);
    assert2(v(&drive(&mut f).await?) == json!({}), "tx_commit ok")?;
    let mut f = vt::query(
        h,
        RString::from("select count(*) as \"n\" from oj_duck_e2e"),
        RString::from("[]"),
    );
    assert2(count(&mut f).await? == base + 1, "commit 后行数 +1")?;

    // ---- stream：250 行批边界 100/100/50（桥侧信封尺寸；不承诺流式省内存，
    //      见文件头局限声明）+ close 幂等 ----
    let mut f = vt::exec(
        h,
        RString::from("create table s250 (n int)"),
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
    let mut batches: Vec<usize> = Vec::new();
    let mut got_rows = 0usize;
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
    let mut f = vt::stream_close(h, sid);
    assert2(v(&drive(&mut f).await?) == json!({}), "stream_close ok")?;
    let mut f = vt::stream_close(h, sid);
    assert2(v(&drive(&mut f).await?) == json!({}), "stream_close 幂等")?;

    // ---- 唯一键冲突：结构化错误面（第四形态差异按实际钉死）——DuckDB 驱动对
    //      约束冲突不回 SQLState（vendor_code 0），errmap 无 23 类可归 → internal
    //      兜底；retryable:false + 结构化信封契约仍成立。差异登记进 jdbc.md 限制表。----
    let mut f = vt::exec(
        h,
        RString::from("insert into oj_duck_e2e values (42, 'dup', '1.00', false)"),
        RString::from("[]"),
    );
    let e = drive(&mut f).await.expect_err("重复主键 insert 必须失败");
    let env: Value = serde_json::from_str(&e).map_err(|_| format!("错误信封非 JSON: {e}"))?;
    // data.vendor = vendor_of(url) 方言族诊断名（v0.1.144 接线：所有 handle 级结构化
    // 错误信封携带）——多 vendor 共处部署的排障定位锚点。
    assert2(
        env["category"] == json!("internal")
            && env["retryable"] == json!(false)
            && env["data"]["vendor"] == json!("duckdb")
            && env["message"]
                .as_str()
                .is_some_and(|m| m.contains("Constraint Error") || m.contains("primary key")),
        &format!("无 SQLState 约束冲突应落 internal 兜底且结构化，实际 {e}"),
    )?;
    println!(
        "e2e_duckdb verdict: constraint_no_sqlstate: DuckDB 不回 SQLState → internal 兜底（跨厂差异登记 jdbc.md）"
    );

    // ---- close → unknown handle ----
    vt::close(h);
    let mut f = vt::query(h, RString::from("select 1"), RString::from("[]"));
    let e = drive(&mut f)
        .await
        .expect_err("close 后查询必须报 unknown handle");
    assert2(e.contains("unknown handle"), &format!("实际: {e}"))?;

    println!("e2e_duckdb: all checks passed");
    Ok(())
}
