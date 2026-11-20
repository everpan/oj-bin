//! MySQL / PostgreSQL 本机容器常驻 env-gated 腿：经 oj-db-jdbc 全栈（vtable →
//! SidecarMgr → 真 JVM → mysql-connector-j / postgresql JDBC 驱动）连本机容器
//! 验证行为面。与 h2_smoke（类型往返）/ e2e_dm（达梦主腿）互补：
//! 本机容器是 CI 之外唯一能跑这两家真驱动的环境（memory: local-test-db-containers）。
//!
//! 断言只锚**行为**（事务回滚、参数绑定、约束分类、流式往返），不锚容器内业务
//! 数据内容（会随使用变化）；表清单与行数仅 --nocapture 信息输出。
//!
//! 环境门槛（缺一即诚实 skip，指引见 skip 文案；两家各自独立可缺）：
//! - `OJ_JDBC_BRIDGE_JAR`：bridge fat jar（`cd java/jdbc-bridge && ./gradlew shadowJar`）
//! - `OJ_TEST_JDBC_MYSQL` / `OJ_TEST_JDBC_PG`：connect 直吃的信封 DSN（含凭据，
//!   只走 env 不进仓库），如
//!   `jdbc:{"url":"jdbc:mysql://127.0.0.1:3306/ojtest","user":"root","password":"root"}`
//! - `OJ_TEST_JDBC_MYSQL_DRIVER_JAR` / `OJ_TEST_JDBC_PG_DRIVER_JAR`：驱动 jar
//!   （Maven 中央 com.mysql:mysql-connector-j / org.postgresql:postgresql，
//!   许可自负），两 jar 同置一个临时 driversDir——顺带验证 DriverLoader 多驱动共存。
//!
//! 同步纪律：有界 deadline 等待，无固定 sleep。

use oj_db_jdbc::sidecar::SidecarCfg;
use oj_db_jdbc::{__test_init, __test_shutdown, vt};
use oj_plugin_ffi::{FfiFuture, RString};
use serde_json::Value;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const BOUND: Duration = Duration::from_secs(60);

fn v(b: &[u8]) -> Value {
    serde_json::from_slice(b).expect("响应应为合法 JSON")
}

/// 列标签大小写因驱动而异（MySQL information_schema 回大写、PG 回小写、达梦
/// CASE_SENSITIVE）——按键取值一律大小写不敏感（T14 探针教训，勿直接 `t["key"]`）。
fn ci<'a>(row: &'a Value, key: &str) -> Option<&'a Value> {
    row.as_object()
        .and_then(|o| o.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)))
        .map(|(_, v)| v)
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

async fn q(handle: u64, sql: &str) -> Result<Value, String> {
    let mut f = vt::query(handle, RString::from(sql), RString::from("[]"));
    Ok(v(&drive(&mut f).await?))
}

async fn x(handle: u64, sql: &str) -> Result<Value, String> {
    let mut f = vt::exec(handle, RString::from(sql), RString::from("[]"));
    Ok(v(&drive(&mut f).await?))
}

async fn probe(label: &str, handle: u64, catalog_sql: &str) -> Result<(), String> {
    println!("==== {label} ====");
    // 1) 库内表清单 + 行数（信息输出，不锚断言——内容随使用变化）
    let tables = q(handle, catalog_sql).await?;
    let mut total = 0i64;
    for t in tables.as_array().ok_or("表清单非数组")? {
        let name = ci(t, "table_name")
            .and_then(Value::as_str)
            .ok_or("缺 table_name")?;
        let n = q(handle, &format!("select count(*) n from {name}"))
            .await
            .and_then(|r| Ok(ci(&r[0], "n").and_then(Value::as_i64).ok_or("count 缺 n")?))?;
        total += n;
        println!("  表 {name}: {n} 行");
    }
    println!("  合计 {total} 行");

    // 2) 事务：begin → 写探针表 → rollback 不留痕
    let probe_table = format!("oj_probe_{}", std::process::id());
    x(
        handle,
        &format!("create table {probe_table}(id int, v varchar(32) unique)"),
    )
    .await?;
    let mut f = vt::begin(handle);
    let tx = ci(&v(&drive(&mut f).await?), "tx_id")
        .and_then(Value::as_u64)
        .ok_or("begin 缺 tx_id")?;
    let mut f = vt::tx_exec(
        handle,
        tx,
        RString::from(&*format!("insert into {probe_table} values(1,'a')")),
        RString::from("[]"),
    );
    drive(&mut f).await?;
    let mut f = vt::tx_rollback(handle, tx);
    drive(&mut f).await?;
    let n = q(handle, &format!("select count(*) n from {probe_table}"))
        .await
        .and_then(|r| Ok(ci(&r[0], "n").and_then(Value::as_i64).ok_or("count 缺 n")?))?;
    assert!(n == 0, "rollback 未回滚（残留 {n} 行）");

    // 3) 参数化 `?` 占位符 + 唯一键冲突 → category=constraint（errmap 生产路径
    //    端到端钉：F1 修复后结构化帧在此两家驱动上必须真接通）
    let mut f = vt::exec(
        handle,
        RString::from(&*format!("insert into {probe_table} values(?,?)")),
        RString::from("[7,\"seven\"]"),
    );
    assert!(drive(&mut f).await.is_ok(), "参数化 insert 失败");
    let mut f = vt::exec(
        handle,
        RString::from(&*format!("insert into {probe_table} values(?,?)")),
        RString::from("[8,\"seven\"]"),
    );
    let err = drive(&mut f).await.expect_err("唯一键冲突应报错");
    let cat = v(err.as_bytes())["category"]
        .as_str()
        .unwrap_or("?")
        .to_string();
    assert!(cat == "constraint", "约束冲突未归类 constraint: {err}");

    // 4) 流式：全表流 + done + close 幂等
    let mut f = vt::stream_open(
        handle,
        RString::from(&*format!("select * from {probe_table} order by id")),
        RString::from("[]"),
    );
    let sid = ci(&v(&drive(&mut f).await?), "stream_id")
        .and_then(Value::as_u64)
        .ok_or("open 缺 stream_id")?;
    let mut rows = 0usize;
    loop {
        let mut f = vt::stream_next(handle, sid);
        let batch = v(&drive(&mut f).await?);
        rows += batch["rows"].as_array().map(|r| r.len()).unwrap_or(0);
        if batch["done"].as_bool().unwrap_or(false) {
            break;
        }
    }
    assert!(rows == 1, "流式应读到 1 行（探针表单行），实读 {rows}");
    let mut f = vt::stream_close(handle, sid);
    drive(&mut f).await?;

    x(handle, &format!("drop table {probe_table}")).await?;
    println!("  行为断言全过（rollback/参数/约束分类/流式），清理完毕");
    Ok(())
}

#[test]
fn local_db_pg_mysql() {
    let bridge = match std::env::var_os("OJ_JDBC_BRIDGE_JAR").map(PathBuf::from) {
        Some(p) if p.is_file() => p,
        _ => {
            eprintln!(
                "local_db: SKIP——缺 OJ_JDBC_BRIDGE_JAR\n\
                 获取指引：cd java/jdbc-bridge && ./gradlew shadowJar，然后\n\
                 export OJ_JDBC_BRIDGE_JAR=$PWD/java/jdbc-bridge/build/libs/oj-jdbc-bridge.jar"
            );
            return;
        }
    };
    let vendors = [
        (
            "MySQL",
            "OJ_TEST_JDBC_MYSQL",
            "OJ_TEST_JDBC_MYSQL_DRIVER_JAR",
            "select table_name from information_schema.tables where table_schema=database() order by table_name",
            "com.mysql:mysql-connector-j（Maven 中央，许可自负）",
        ),
        (
            "PostgreSQL",
            "OJ_TEST_JDBC_PG",
            "OJ_TEST_JDBC_PG_DRIVER_JAR",
            "select table_name from information_schema.tables where table_schema='public' order by table_name",
            "org.postgresql:postgresql（Maven 中央，许可自负）",
        ),
    ];
    // 先收集实跑清单（缺 env 的诚实 skip 打印获取指引，不挡另一家）
    let mut legs = vec![];
    for (label, dsn_env, jar_env, catalog, coord) in vendors {
        let (Some(dsn), Some(jar)) = (
            std::env::var_os(dsn_env).map(PathBuf::from),
            std::env::var_os(jar_env).map(PathBuf::from),
        ) else {
            eprintln!(
                "local_db: SKIP——{label} 缺 {dsn_env} / {jar_env}\n\
                 获取指引：\n\
                 1. 驱动 jar: Maven 中央 {coord}，export {jar_env}=<下载路径>\n\
                 2. DSN: export {dsn_env}='jdbc:{{\"url\":\"jdbc:…\",\"user\":\"…\",\"password\":\"…\"}}'\n\
                    （信封形态 connect 直吃，凭据只走 env 不进仓库）"
            );
            continue;
        };
        if !jar.is_file() {
            eprintln!("local_db: SKIP——{label} {jar_env} 指向的文件不存在");
            continue;
        }
        legs.push((label, dsn, jar, catalog));
    }
    if legs.is_empty() {
        return;
    }

    // 两 jar 同置一个 driversDir（DriverLoader 多驱动共存验证），绝不进 -cp（T2 教训）
    let drivers = std::env::temp_dir().join(format!("oj-jdbc-localdb-{}", std::process::id()));
    std::fs::create_dir_all(&drivers).expect("建临时 drivers 目录");
    for (i, (_, _, jar, _)) in legs.iter().enumerate() {
        std::fs::copy(jar, drivers.join(format!("driver{i}.jar")))
            .expect("拷贝驱动 jar 进 driversDir");
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");
    let r = rt.block_on(async {
        __test_init(SidecarCfg {
            java_bin: None,
            java_opts: vec![],
            bridge_jar: bridge.clone(),
            drivers_dir: drivers.clone(),
            hello_timeout: Duration::from_secs(30),
            env: vec![],
            log: None,
        });
        let mut r = Ok::<(), String>(());
        for (label, dsn, _, catalog) in &legs {
            let mut f = vt::connect(RString::from(dsn.to_str().ok_or("DSN 非 UTF-8")?));
            match drive(&mut f).await.and_then(|b| {
                ci(&v(&b), "handle")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| "connect ok 缺 handle".to_string())
            }) {
                Ok(h) => {
                    if let Err(e) = probe(label, h, catalog).await {
                        r = Err(format!("{label}: {e}"));
                        break;
                    }
                    vt::close(h);
                }
                Err(e) => {
                    r = Err(format!("{label} connect: {e}"));
                    break;
                }
            }
        }
        r
    });
    let _ = rt.block_on(__test_shutdown());
    let _ = std::fs::remove_dir_all(&drivers);
    r.unwrap_or_else(|e| panic!("local_db: {e}"));
    println!("local_db: all checks passed");
}
