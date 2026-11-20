//! T14 达梦远程 e2e 主腿（env-gated）。对真·远程 DM 实例（维护者 env 注入凭据）
//! 跑 tests/e2e 全用例链：connect 脱敏 → DDL（事务外）→ null 绑参（setNull VARCHAR）→
//! 整数列回 number（NUMERIC scale==0 分流）→ 空串实际形态钉死 → 时间 UTC 口径 →
//! 事务回滚/提交 → stream 批边界（= db.export 泵流式面）→ 真·在途 cancel（旗舰 a）→
//! cancel 后 stream_close 拆解语义裁决（旗舰 b）。
//!
//! 环境门槛（缺一即诚实 skip，指引见 skip() 文案）：
//! - `OJ_TEST_JDBC_DM_URL`：如 jdbc:dm://192.168.67.64:5237
//! - `OJ_TEST_JDBC_DM_USER` / `OJ_TEST_JDBC_DM_PASSWORD`：实例凭据（**只走 env，不进仓库**）
//! - `OJ_TEST_JDBC_DM_DRIVER_JAR`：Dm8JdbcDriver18 jar 路径（官方 Maven 中央
//!   com.dameng:Dm8JdbcDriver18，许可自负）
//! - `OJ_JDBC_BRIDGE_JAR`：bridge fat jar（`cd java/jdbc-bridge && ./gradlew shadowJar`）
//! - PATH/JAVA_HOME 可解析 `java`

mod e2e;

use oj_db_jdbc::{__test_shutdown, dsn};
use std::path::PathBuf;

fn skip(msg: &str) {
    eprintln!(
        "e2e_dm: SKIP——{msg}\n\
         获取指引：\n\
         1. 远程 DM 实例 + 凭据（export OJ_TEST_JDBC_DM_URL='jdbc:dm://host:5237'\n\
            OJ_TEST_JDBC_DM_USER=... OJ_TEST_JDBC_DM_PASSWORD=...，凭据只走 env）\n\
         2. DM 驱动 jar：自官方 Maven 中央下载 com.dameng:Dm8JdbcDriver18\n\
            （https://repo1.maven.org/maven2/com/dameng/Dm8JdbcDriver18/），\n\
            export OJ_TEST_JDBC_DM_DRIVER_JAR=<下载路径>（许可自负）\n\
         3. bridge jar: cd java/jdbc-bridge && ./gradlew shadowJar\n\
            然后 export OJ_JDBC_BRIDGE_JAR=$PWD/java/jdbc-bridge/build/libs/oj-jdbc-bridge.jar\n\
         4. 需 PATH 或 JAVA_HOME 可解析 java"
    );
}

#[test]
fn e2e_dm() {
    let (Some(url), Some(user), Some(password)) = (
        std::env::var("OJ_TEST_JDBC_DM_URL").ok(),
        std::env::var("OJ_TEST_JDBC_DM_USER").ok(),
        std::env::var("OJ_TEST_JDBC_DM_PASSWORD").ok(),
    ) else {
        skip("缺 OJ_TEST_JDBC_DM_URL / _USER / _PASSWORD");
        return;
    };
    let (Some(bridge), Some(driver)) = (
        std::env::var_os("OJ_JDBC_BRIDGE_JAR").map(PathBuf::from),
        std::env::var_os("OJ_TEST_JDBC_DM_DRIVER_JAR").map(PathBuf::from),
    ) else {
        skip("缺 OJ_JDBC_BRIDGE_JAR / OJ_TEST_JDBC_DM_DRIVER_JAR");
        return;
    };
    for (p, what) in [
        (&bridge, "OJ_JDBC_BRIDGE_JAR"),
        (&driver, "OJ_TEST_JDBC_DM_DRIVER_JAR"),
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
    if !url.starts_with("jdbc:dm:") {
        skip(&format!(
            "OJ_TEST_JDBC_DM_URL 应为 jdbc:dm://...，实际 {url}"
        ));
        return;
    }

    // 夹具纪律（T2 教训）：DM jar 只进临时 driversDir（cfg 透传），绝不进 -cp。
    let drivers = std::env::temp_dir().join(format!("oj-jdbc-dm-drivers-{}", std::process::id()));
    std::fs::create_dir_all(&drivers).expect("建临时 drivers 目录");
    std::fs::copy(&driver, drivers.join("dm.jar")).expect("拷贝 DM jar 进 driversDir");

    // 信封 DSN（dsn::parse_dsn 同路径，证明 envelope 形态可连真实例）。
    let dsn_s = format!(
        "jdbc:{}",
        serde_json::json!({
            "url": url, "user": user, "password": password,
        })
    );
    // 与插件 dsn 模块同解一遍，确保信封在进 vtable 前即合法（错配早失败）。
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
    let verdicts = r.unwrap_or_else(|e| panic!("e2e_dm: {e}"));
    for line in &verdicts {
        println!("e2e_dm verdict: {line}");
    }
    println!("e2e_dm: all checks passed");
}
