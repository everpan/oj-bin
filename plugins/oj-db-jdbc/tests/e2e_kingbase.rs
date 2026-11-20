//! T14 金仓可选腿（env-gated）。金仓实例就绪前整体诚实 skip；就绪后与达梦主腿
//! 同用例链（tests/e2e）。注意：金仓形态未实测（无本地实例），用例链断言按
//! DM 实测钉死——首个金仓 env 就绪者若见红，先核对差异属库差异还是插件缺陷。
//!
//! 环境门槛（缺一即整体 skip，指引见 skip() 文案）：
//! - `OJ_TEST_JDBC_KB_URL`：如 jdbc:kingbase8://host:54321/db
//! - `OJ_TEST_JDBC_KB_USER` / `OJ_TEST_JDBC_KB_PASSWORD`：实例凭据（**只走 env**）
//! - `OJ_TEST_JDBC_KB_DRIVER_JAR`：kingbase8 JDBC 驱动 jar
//! - `OJ_JDBC_BRIDGE_JAR`：bridge fat jar（`cd java/jdbc-bridge && ./gradlew shadowJar`）
//! - PATH/JAVA_HOME 可解析 `java`

mod e2e;

use oj_db_jdbc::{__test_shutdown, dsn};
use std::path::PathBuf;

fn skip(msg: &str) {
    eprintln!(
        "e2e_kingbase: SKIP——{msg}\n\
         获取指引：\n\
         1. 金仓实例 + 凭据（export OJ_TEST_JDBC_KB_URL='jdbc:kingbase8://host:54321/db'\n\
            OJ_TEST_JDBC_KB_USER=... OJ_TEST_JDBC_KB_PASSWORD=...，凭据只走 env）\n\
         2. kingbase8 驱动 jar（金仓官方渠道获取，许可自负），\n\
            export OJ_TEST_JDBC_KB_DRIVER_JAR=<下载路径>\n\
         3. bridge jar: cd java/jdbc-bridge && ./gradlew shadowJar\n\
            然后 export OJ_JDBC_BRIDGE_JAR=$PWD/java/jdbc-bridge/build/libs/oj-jdbc-bridge.jar\n\
         4. 需 PATH 或 JAVA_HOME 可解析 java"
    );
}

#[test]
fn e2e_kingbase() {
    let (Some(url), Some(user), Some(password)) = (
        std::env::var("OJ_TEST_JDBC_KB_URL").ok(),
        std::env::var("OJ_TEST_JDBC_KB_USER").ok(),
        std::env::var("OJ_TEST_JDBC_KB_PASSWORD").ok(),
    ) else {
        skip("缺 OJ_TEST_JDBC_KB_URL / _USER / _PASSWORD");
        return;
    };
    let (Some(bridge), Some(driver)) = (
        std::env::var_os("OJ_JDBC_BRIDGE_JAR").map(PathBuf::from),
        std::env::var_os("OJ_TEST_JDBC_KB_DRIVER_JAR").map(PathBuf::from),
    ) else {
        skip("缺 OJ_JDBC_BRIDGE_JAR / OJ_TEST_JDBC_KB_DRIVER_JAR");
        return;
    };
    for (p, what) in [
        (&bridge, "OJ_JDBC_BRIDGE_JAR"),
        (&driver, "OJ_TEST_JDBC_KB_DRIVER_JAR"),
    ] {
        if !p.is_file() {
            skip(&format!("{what} 指向的文件不存在"));
            return;
        }
    }
    if !e2e::java_available() {
        skip("PATH/JAVA_HOME 均解析不到 java");
        return;
    }
    let url = url.trim().to_string();
    if !url.starts_with("jdbc:kingbase8:") {
        skip(&format!(
            "OJ_TEST_JDBC_KB_URL 应为 jdbc:kingbase8://...，实际 {url}"
        ));
        return;
    }

    let drivers = std::env::temp_dir().join(format!("oj-jdbc-kb-drivers-{}", std::process::id()));
    std::fs::create_dir_all(&drivers).expect("建临时 drivers 目录");
    std::fs::copy(&driver, drivers.join("kingbase8.jar")).expect("拷贝驱动 jar 进 driversDir");

    let dsn_s = format!(
        "jdbc:{}",
        serde_json::json!({
            "url": url, "user": user, "password": password,
        })
    );

    // 与插件 dsn 模块同解一遍，确保信封在进 vtable 前即合法（错配早失败，不 panic）。
    if !matches!(dsn::parse_dsn(&dsn_s), Ok(dsn::ParsedDsn::Envelope(_))) {
        skip("DSN 信封解析失败");
        return;
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");
    let r = rt.block_on(e2e::run_chain(&e2e::ChainInput {
        bridge,
        drivers: drivers.clone(),
        dsn: dsn_s,
        password,
    }));
    let _ = rt.block_on(__test_shutdown());
    let _ = std::fs::remove_dir_all(&drivers);
    let verdicts = r.unwrap_or_else(|e| panic!("e2e_kingbase: {e}"));
    for line in &verdicts {
        println!("e2e_kingbase verdict: {line}");
    }
    println!("e2e_kingbase: all checks passed");
}
