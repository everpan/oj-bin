//! REQ-A4（v0.1.144）驱动装载 smoke 腿（env-gated + 发行布局回落探测）：对随包
//! 三件套（Trino / ClickHouse `-all-dependencies` / Kyuubi shaded）与自备件
//! （HeavyDB 上游已废弃 / Snowflake / Databricks——均仅 env 注入，验 jar 可装载）
//! 单 JVM 单 driversDir spawn 真桥 → ServiceLoader 注册 → 对**必不可达端点**
//! connect。断言口径：报错（预期的网络失败）且错误**不得**含 `No suitable driver`
//! ——出现即 ServiceLoader/Shim 集成失败（jar 选错形态或没装上）；正常应见连接
//! 被拒证据；结构化信封另断言 `data.vendor` = 方言族诊断名。无需真服务端即可
//! 证明驱动集成。
//!
//! Snowflake / Databricks 属 env-gated 实证路径：腿在位、无 jar 环境诚实 skip
//! （评审裁决：登记为「未实跑」，不虚报覆盖）。
//!
//! 环境门槛（缺一即诚实 skip，指引见 skip() 文案）：
//! - `OJ_JDBC_BRIDGE_JAR`：bridge fat jar（`cd java/jdbc-bridge && ./gradlew shadowJar`）
//! - 各方言 jar：`OJ_JDBC_TRINO_JAR` / `OJ_JDBC_CLICKHOUSE_JAR` / `OJ_JDBC_KYUUBI_JAR`
//!   / `OJ_JDBC_HEAVYDB_JAR` / `OJ_JDBC_SNOWFLAKE_JAR` / `OJ_JDBC_DATABRICKS_JAR`；
//!   前三者缺 env 时回落探测发行布局 `<root>/bin/plugins/*/jdbc/drivers/`
//!   （cargo xtask jdbc-drivers 的落位处）
//! - PATH/JAVA_HOME 可解析 `java`

use oj_db_jdbc::sidecar::SidecarCfg;
use oj_db_jdbc::{__test_init, __test_shutdown, vt};
use oj_plugin_ffi::{FfiFuture, RString};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// 单 connect 等待上界（真 JVM 首启 + 驱动装载余量），是 deadline 不是同步手段。
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

/// 发行布局回落探测：`<root>/bin/plugins/*/jdbc/drivers/<prefix>*.jar`（排序首个）。
fn bundled_jar(prefix: &str) -> Option<PathBuf> {
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
                        .is_some_and(|n| n.starts_with(prefix) && n.ends_with(".jar"))
                })
        })
        .collect();
    hits.sort();
    hits.into_iter().next()
}

fn skip(msg: &str) {
    eprintln!(
        "e2e_driver_smoke: SKIP——{msg}\n\
         获取指引：\n\
         1. bridge jar: cd java/jdbc-bridge && ./gradlew shadowJar\n\
            然后 export OJ_JDBC_BRIDGE_JAR=$PWD/java/jdbc-bridge/build/libs/oj-jdbc-bridge.jar\n\
         2. 随包三件套一键落位（含发行布局回落探测）：cargo xtask jdbc-drivers\n\
            或自备 env：OJ_JDBC_TRINO_JAR / OJ_JDBC_CLICKHOUSE_JAR / OJ_JDBC_KYUUBI_JAR\n\
            （自备三件仅 env：HeavyDB 上游已废弃 → OJ_JDBC_HEAVYDB_JAR；\n\
            Snowflake → OJ_JDBC_SNOWFLAKE_JAR；Databricks → OJ_JDBC_DATABRICKS_JAR）\n\
         3. 需 PATH 或 JAVA_HOME 可解析 java"
    );
}

#[test]
fn e2e_driver_smoke() {
    let bridge = std::env::var_os("OJ_JDBC_BRIDGE_JAR").map(PathBuf::from);
    let Some(bridge) = bridge else {
        skip("缺 OJ_JDBC_BRIDGE_JAR");
        return;
    };
    if !bridge.is_file() {
        skip("OJ_JDBC_BRIDGE_JAR 指向的文件不存在");
        return;
    }
    if !java_available() {
        skip("PATH/JAVA_HOME 均解析不到 java");
        return;
    }

    // vendor → (env 名, 发行布局前缀, URL 前缀自检, 不可达端点 URL)。
    // 随包三件套 jar 形态在文件头/指引注明：ClickHouse 必须 -all-dependencies，
    // Kyuubi 必须 shaded。自备三件仅 env 注入（发行布局前缀留空即不回落探测）：
    // HeavyDB 上游已废弃；Snowflake/Databricks 评审补臂——env-gated 未实跑如实登记。
    let specs: [(&str, &str, &str, &str, &str); 6] = [
        (
            "trino",
            "OJ_JDBC_TRINO_JAR",
            "trino-jdbc-",
            "jdbc:trino:",
            "jdbc:trino://127.0.0.1:1/ctl/default?user=oj",
        ),
        (
            "clickhouse",
            "OJ_JDBC_CLICKHOUSE_JAR",
            "clickhouse-jdbc-",
            "jdbc:ch:",
            "jdbc:ch:http://127.0.0.1:1/db",
        ),
        (
            "kyuubi",
            "OJ_JDBC_KYUUBI_JAR",
            "kyuubi-hive-jdbc-shaded-",
            "jdbc:kyuubi:",
            "jdbc:kyuubi://127.0.0.1:1/default",
        ),
        (
            "heavydb",
            "OJ_JDBC_HEAVYDB_JAR",
            "",
            "jdbc:heavyai:",
            "jdbc:heavyai://127.0.0.1:1/db",
        ),
        (
            "snowflake",
            "OJ_JDBC_SNOWFLAKE_JAR",
            "",
            "jdbc:snowflake:",
            "jdbc:snowflake://127.0.0.1:1/?account=oj",
        ),
        (
            "databricks",
            "OJ_JDBC_DATABRICKS_JAR",
            "",
            "jdbc:databricks:",
            "jdbc:databricks://127.0.0.1:1/default;transportMode=http;ssl=0",
        ),
    ];
    let cases: Vec<(String, Option<PathBuf>, String, String, String)> = specs
        .iter()
        .map(|(vendor, env, prefix, urlpfx, url)| {
            let jar = std::env::var_os(*env)
                .map(PathBuf::from)
                .or_else(|| (!prefix.is_empty()).then(|| bundled_jar(prefix)).flatten());
            (
                vendor.to_string(),
                jar,
                urlpfx.to_string(),
                url.to_string(),
                env.to_string(),
            )
        })
        .collect();

    let runnable: Vec<_> = cases
        .into_iter()
        .filter(|(vendor, jar, _urlpfx, _url, _env)| match jar {
            Some(p) if p.is_file() => true,
            Some(_) => {
                eprintln!("e2e_driver_smoke: SKIP {vendor}——jar 路径不存在");
                false
            }
            None => {
                eprintln!("e2e_driver_smoke: SKIP {vendor}——jar 未提供（自备件属预期，随包件先跑 cargo xtask jdbc-drivers）");
                false
            }
        })
        .collect();
    if runnable.is_empty() {
        skip("无任何方言 jar 可跑（env 与发行布局均未命中）");
        return;
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");
    let r = rt.block_on(smoke_all(&bridge, &runnable));
    let _ = rt.block_on(__test_shutdown());
    r.unwrap_or_else(|e| panic!("e2e_driver_smoke: {e}"));
}

async fn smoke_all(
    bridge: &Path,
    cases: &[(String, Option<PathBuf>, String, String, String)],
) -> Result<(), String> {
    // 单 JVM 单 driversDir（`__test_init` 是 OnceLock——进程内换不了 sidecar；且随包
    // 三件套同目录共处正是生产形态，「一目录一版本」针对的是同 vendor 同名异版）。
    let drivers = std::env::temp_dir().join(format!("oj-jdbc-smoke-{}", std::process::id()));
    std::fs::create_dir_all(&drivers).expect("建临时 drivers 目录");
    for (_vendor, jar, _urlpfx, _url, _env) in cases {
        let jar = jar.as_ref().expect("runnable 过滤后 jar 必为 Some");
        std::fs::copy(jar, drivers.join(jar.file_name().expect("jar 有文件名")))
            .expect("拷贝驱动 jar");
    }

    __test_init(SidecarCfg {
        java_bin: None,
        java_opts: vec![],
        bridge_jar: bridge.to_path_buf(),
        drivers_dir: drivers.clone(),
        hello_timeout: Duration::from_secs(30),
        env: vec![],
        log: None,
    });

    let mut failures: Vec<String> = Vec::new();
    for (vendor, _jar, urlpfx, url, _env) in cases {
        // 逐 vendor 累积失败（评审修复：不再首败截断——全部 vendor 跑完、清理
        // 临时目录后再收敛断言，一次报全）。
        if let Err(e) = probe(vendor, urlpfx, url).await {
            failures.push(format!("{vendor}: {e}"));
        }
    }
    let _ = std::fs::remove_dir_all(&drivers);
    if !failures.is_empty() {
        return Err(format!(
            "{} 个方言腿失败:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        ));
    }
    println!("e2e_driver_smoke: all checks passed");
    Ok(())
}

/// 单方言探针：connect（懒连接驱动补 select 1 一刀）→ 断言驱动注册证据。
/// 失败以 Err 返回（不 panic），供外层逐 vendor 累积。
async fn probe(vendor: &str, urlpfx: &str, url: &str) -> Result<(), String> {
    // URL 前缀自检（防用例自身写错形态，探活纪律）。
    assert2(
        url.starts_with(urlpfx),
        &format!("用例 URL 前缀自检 {url} 应以 {urlpfx} 开头"),
    )?;

    // 探针目的：证明驱动经 ServiceLoader/Shim 完成注册（无 "No suitable driver"）。
    // 懒连接驱动（如 Trino：connect 只建对象不开 TCP）→ connect 成功是真驱动
    // 正确行为，网络证据落到首个碰线阶段（此处用 select 1 补一刀）。
    let mut f = vt::connect(RString::from(
        format!("jdbc:{{\"url\":\"{url}\"}}").as_str(),
    ));
    let (e, lazy_handle) = match drive(&mut f).await {
        Err(e) => (e, None),
        Ok(ok) => {
            let h = v(&ok)["handle"]
                .as_u64()
                .ok_or("懒连接 connect 缺 handle")?;
            let mut f = vt::query(h, RString::from("select 1"), RString::from("[]"));
            let e = match drive(&mut f).await {
                Err(e) => e,
                Ok(_) => return Err("懒连接驱动的查询必须碰网络且失败".into()),
            };
            (e, Some(h))
        }
    };
    if let Some(h) = lazy_handle {
        vt::close(h);
    }

    // 核心断言：错误绝非 No suitable driver（出现即 ServiceLoader/Shim 集成失败
    // ——jar 形态错选（thin/非 -all-dependencies）或没装上的典型症状）；且含
    // 连接失败证据。
    assert2(
        !e.contains("No suitable driver"),
        &format!("驱动未注册（No suitable driver）——jar 形态或装载问题: {e}"),
    )?;
    assert2(
        e.contains("127.0.0.1")
            || e.contains("Connection")
            || e.contains("connect")
            || e.contains("refused")
            || e.contains("Failed")
            || e.contains("Suitable")
            || e.contains("250")
            || e.contains("08S"),
        &format!("错误应含连接失败证据: {e}"),
    )?;
    // 结构化信封（v0.1.144 接线）：data.vendor 必为 vendor_of(url) 方言族诊断名，
    // 与 spec vendor 名同词表（trino/clickhouse/kyuubi/heavydb/snowflake/databricks）。
    if let Ok(env_j) = serde_json::from_str::<Value>(&e) {
        assert2(
            env_j
                .get("data")
                .and_then(|d| d.get("vendor"))
                .and_then(Value::as_str)
                == Some(vendor),
            &format!(
                "结构化信封 data.vendor 应为 {vendor}，实际 {:?}",
                env_j.get("data")
            ),
        )?;
        println!(
            "e2e_driver_smoke verdict: {vendor}: structured category={:?} retryable={:?} vendor={:?}",
            env_j.get("category"),
            env_j.get("retryable"),
            env_j.get("data").and_then(|d| d.get("vendor")),
        );
    } else {
        println!(
            "e2e_driver_smoke verdict: {vendor}: connect error (plain): {}",
            e.chars().take(120).collect::<String>()
        );
    }
    Ok(())
}
