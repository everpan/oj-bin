//! DSN 解析表驱动测试（design spec §5.2 全部形态 + §5.5 vendor 识别表）。

use crate::dsn::{ParsedDsn, parse_dsn, vendor_of};

/// 裸 JDBC URL 与整串 `ENC[...]` 一律原样透出（Bare）：
/// 凭据在 URL 内的裸形态不做解析；ENC 解密发生在宿主配置装载期（decrypt_tree），
/// 插件按契约零支持——整串原样回送即可（spec §5.2 ENC 密封范式）。
#[test]
fn bare_passthrough() {
    let cases = [
        "jdbc:oracle:thin:@//127.0.0.1:1521/FREEPDB1",
        "jdbc:dm://192.168.67.64:5237",
        "ENC[c2VhbGVkLXNlY3JldA:rsa1]",
        "ENC[]",
    ];
    for dsn in cases {
        match parse_dsn(dsn) {
            Ok(ParsedDsn::Bare(s)) => assert_eq!(s, dsn, "must pass through verbatim"),
            other => panic!("{dsn}: expected Bare passthrough, got {other:?}"),
        }
    }
}

/// `jdbc:{...}` JSON 信封：url/user/password/props 全字段解析。
#[test]
fn envelope_full_parse() {
    let dsn = r#"jdbc:{"url":"jdbc:dm://h:5237","user":"u","password":"p","props":{"compatibleMode":"mysql"}}"#;
    let ParsedDsn::Envelope(e) = parse_dsn(dsn).expect("envelope") else {
        panic!("expected Envelope, got Bare");
    };
    assert_eq!(e.url, "jdbc:dm://h:5237");
    assert_eq!(e.user.as_deref(), Some("u"));
    assert_eq!(e.password.as_deref(), Some("p"));
    assert_eq!(e.props["compatibleMode"], serde_json::json!("mysql"));
    assert_eq!(e.driver_class, None);
}

/// 信封只给 url 也是合法形态（user/password/props 全可选）。
#[test]
fn envelope_url_only() {
    let dsn = r#"jdbc:{"url":"jdbc:oracle:thin:@//h:1521/X"}"#;
    let ParsedDsn::Envelope(e) = parse_dsn(dsn).expect("envelope") else {
        panic!("expected Envelope");
    };
    assert_eq!(e.url, "jdbc:oracle:thin:@//h:1521/X");
    assert_eq!(e.user, None);
    assert_eq!(e.password, None);
    assert!(e.props.is_empty());
}

/// 信封缺 url → Err（url 是唯二必填——另一必填由宿主装配保证：整条 DSN 非空）。
#[test]
fn envelope_missing_url_errs() {
    let dsn = r#"jdbc:{"user":"u","password":"p"}"#;
    let err = parse_dsn(dsn).expect_err("missing url must err");
    assert!(err.contains("url"), "err should name the field: {err}");
}

/// 信封 JSON 语法坏 → Err（含解析器细节）。
#[test]
fn envelope_bad_json_errs() {
    for dsn in ["jdbc:{not json", r#"jdbc:{"url":42}"#, r#"jdbc:["url"]"#] {
        assert!(parse_dsn(dsn).is_err(), "{dsn} must err");
    }
}

/// `password: {"$env":"VAR"}` 展开：已设置 → 取值。
#[test]
fn envelope_password_env_expansion() {
    const VAR: &str = "OJ_TEST_JDBC_T7_PASS";
    unsafe { std::env::set_var(VAR, "s3cret") };
    let dsn = format!(r#"jdbc:{{"url":"jdbc:oracle:thin:@//h/x","password":{{"$env":"{VAR}"}}}}"#);
    let ParsedDsn::Envelope(e) = parse_dsn(&dsn).expect("envelope") else {
        panic!("expected Envelope");
    };
    assert_eq!(e.password.as_deref(), Some("s3cret"));
}

/// `password: {"$env":"VAR"}` 展开：未设置 → Err 且指明变量名（部署机可诊断）。
#[test]
fn envelope_password_env_unset_errs() {
    const VAR: &str = "OJ_TEST_JDBC_T7_UNSET";
    unsafe { std::env::remove_var(VAR) };
    let dsn = format!(r#"jdbc:{{"url":"jdbc:dm://h","password":{{"$env":"{VAR}"}}}}"#);
    let err = parse_dsn(&dsn).expect_err("unset env var must err");
    assert!(err.contains(VAR), "err should name the var: {err}");
    assert!(err.contains("not set"), "err should say not set: {err}");
}

/// `props.driver_class`：缺 ServiceLoader 条目的老商用 jar 的逃生舱（spec §5.2），
/// 从 props 提取为独立字段（不随 props 上送 Java 连接属性）。
#[test]
fn envelope_driver_class_from_props() {
    let dsn = r#"jdbc:{"url":"jdbc:dm://h","props":{"driver_class":"dm.jdbc.driver.DmDriver","compatibleMode":"mysql"}}"#;
    let ParsedDsn::Envelope(e) = parse_dsn(dsn).expect("envelope") else {
        panic!("expected Envelope");
    };
    assert_eq!(e.driver_class.as_deref(), Some("dm.jdbc.driver.DmDriver"));
    assert!(
        !e.props.contains_key("driver_class"),
        "driver_class must be extracted out of props"
    );
    assert_eq!(e.props["compatibleMode"], serde_json::json!("mysql"));
}

/// vendor 识别表（spec §5.5）：子协议段 → 方言族；仅识别，不改变运行时行为。
/// 未知 vendor 原样透出（用于诊断日志/错误文案）。
#[test]
fn vendor_table() {
    let cases = [
        ("jdbc:postgresql://h/db", "postgres"),
        ("jdbc:postgres://h/db", "postgres"),
        ("jdbc:mysql://h/db", "mysql"),
        ("jdbc:oracle:thin:@//h:1521/FREEPDB1", "oracle"),
        ("jdbc:dm://h:5237", "dameng"),
        ("jdbc:kingbase8://h:54321/db", "kingbase"),
        ("jdbc:sqlserver://h;databaseName=d", "mssql"),
        ("jdbc:microsoft://h", "mssql"),
        // 七方言扩展（REQ-A4，v0.1.144）：仅诊断臂，dialect() 恒 "jdbc" 不变。
        (
            "jdbc:snowflake://acct.snowflakecomputing.com/?warehouse=w",
            "snowflake",
        ),
        (
            "jdbc:databricks://h:443/default;httpPath=/sql/1.0/warehouses/x",
            "databricks",
        ),
        ("jdbc:ch:http://h:8123/db", "clickhouse"),
        ("jdbc:clickhouse:http://h:8123/db", "clickhouse"),
        ("jdbc:trino://h:8080/catalog/schema", "trino"),
        ("jdbc:kyuubi://h:10009/default", "kyuubi"),
        ("jdbc:heavyai://h:6274/db", "heavydb"),
        ("jdbc:mapd://h:6274/db", "heavydb"),    // 旧前缀同臂
        ("jdbc:omnisci://h:6274/db", "heavydb"), // 旧前缀同臂
        ("jdbc:duckdb:", "duckdb"),              // 内存库无 host 段
        ("jdbc:duckdb:/path/to/db", "duckdb"),
        ("jdbc:teradata://h", "teradata"),    // 未知原样
        ("not-a-jdbc-url", "not-a-jdbc-url"), // 无 jdbc: 前缀：整串当段（调用方保证先判形态）
    ];
    for (url, want) in cases {
        assert_eq!(vendor_of(url), want, "{url}");
    }
}
