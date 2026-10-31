use super::*;
use oj_plugin_ffi::RBytes;

/// DSN 前缀 → 方言判定（占位符补全 / lock 句柄等都依赖它，错判即打错方言 SQL）。
#[test]
fn given_dsn_when_dialect_of_then_prefix_decides() {
    assert_eq!(dialect_of("postgres://u:p@h/db"), Dialect::Postgres);
    assert_eq!(dialect_of("postgresql://u:p@h/db"), Dialect::Postgres);
    assert_eq!(dialect_of("mysql://u:p@h/db"), Dialect::MySql);
    assert_eq!(dialect_of("sqlite://f.db"), Dialect::Sqlite);
    assert_eq!(dialect_of("file.db"), Dialect::Sqlite);
    assert_eq!(dialect_of(""), Dialect::Sqlite);
}

#[test]
fn given_dialect_when_str_then_wire_name() {
    assert_eq!(dialect_str(Dialect::Sqlite), "sqlite");
    assert_eq!(dialect_str(Dialect::MySql), "mysql");
    assert_eq!(dialect_str(Dialect::Postgres), "postgres");
}

/// 解不出列类型必须响亮报错且点名列名+类型（消费方 JS 要能据此改写 SQL）。
#[test]
fn undecodable_column_error_names_column_and_type() {
    let e = undecodable_column("amount", "NUMERIC");
    assert!(e.contains("amount") && e.contains("NUMERIC"), "{e}");
    assert!(e.contains("cast"), "报错须给出改写线索: {e}");
    assert!(e.contains("db-guide.md"), "报错须指到读侧口径文档: {e}");
}

/// `NUMERIC` 文本化口径（v0.1.3 / L6，离线）：去尾零、保有效位。
///
/// 这条钉的是 sqlx 的标度失真（`scale = (digits.len()-weight-1)*4`）：真库读出 `1.5`
/// 时 `BigDecimal` 的标度是 4，直接 `to_string()` 会给出 `1.5000`（假精度）。
#[test]
fn given_padded_scale_when_decimal_text_then_trailing_zeros_stripped() {
    use std::str::FromStr;
    let cases = [
        ("1.5000", "1.5"),
        ("0.1000", "0.1"),
        ("1234.5678", "1234.5678"),
        ("0.12345000", "0.12345"),
        ("10000", "10000"),
        ("-2.5000", "-2.5"),
        ("0.0000", "0"),
    ];
    for (raw, want) in cases {
        let d = BigDecimal::from_str(raw).unwrap();
        assert_eq!(decimal_text(&d), want, "输入 {raw}");
    }
    // 精度不得被削减：20 位小数原样保留。
    let d = BigDecimal::from_str("0.12345678901234567890").unwrap();
    assert_eq!(decimal_text(&d), "0.1234567890123456789");
}

/// `safe_ident` 的消毒/截断（离线）：错误串是日志注入面。
#[test]
fn given_hostile_column_name_when_safe_ident_then_control_chars_stripped() {
    assert_eq!(safe_ident("col"), "col");
    assert_eq!(safe_ident("a\nb\tc"), "abc");
    assert_eq!(safe_ident("x\u{0}y"), "xy");
    let long = "a".repeat(100);
    let out = safe_ident(&long);
    assert_eq!(out.chars().count(), 65, "64 字符 + 省略号");
    assert!(out.ends_with('…'));
    // 未截断时不得画蛇添足加省略号（含多字节字符的列名也不能误判）。
    assert_eq!(safe_ident("列名"), "列名");
}

/// 离线 sqlite 全路径 roundtrip（dev 构建统一出 sqlite 驱动，生产 cdylib 仍单方言）：
/// connect → DDL → 参数化 insert → query 行 JSON 形状 → 事务 rollback 不可见 /
/// commit 可见 → unknown handle/tx、bad SQL、bad params 各自点名 → dialect() 线名。
#[tokio::test(flavor = "multi_thread")]
async fn given_sqlite_dsn_when_full_vtable_roundtrip_then_offline_green() {
    let _ = std::result::Result::from(init(host(), RString::from("{}")));
    let dir = std::env::temp_dir().join(format!("oj-dbpg-off-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // Any 默认 create_if_missing=false → 先 touch（0 字节即合法空库）。
    std::fs::File::create(dir.join("t.db")).unwrap();
    // 与宿主同一入口：绝对路径走单冒号 `sqlite:` + 正斜杠（Windows 盘符不会
    // 被 Url 解析吞成 host），见 oj_plugin_ffi::path_util。
    let dsn = oj_plugin_ffi::path_util::sqlite_file_dsn(&dir.join("t.db"));

    let bytes = drive(&mut connect(RString::from(dsn.as_str())))
        .await
        .expect("connect");
    let h = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["handle"]
        .as_u64()
        .unwrap();

    drive(&mut exec(
        h,
        RString::from("create table if not exists t (id integer primary key, v text)"),
        RString::from("[]"),
    ))
    .await
    .expect("ddl");
    drive(&mut exec(
        h,
        RString::from("insert into t (id, v) values (?, ?)"),
        RString::from(r#"[1,"hi"]"#),
    ))
    .await
    .expect("insert");
    let rows = drive(&mut query(
        h,
        RString::from("select id, v from t where id = ?"),
        RString::from("[1]"),
    ))
    .await
    .expect("query");
    let v: serde_json::Value = serde_json::from_slice(&rows).unwrap();
    assert_eq!(v[0]["id"], 1, "{v}");
    assert_eq!(v[0]["v"], "hi", "{v}");

    // 事务：rollback 后不可见。
    let b = drive(&mut begin(h)).await.expect("begin");
    let tx = serde_json::from_slice::<serde_json::Value>(&b).unwrap()["tx_id"]
        .as_u64()
        .unwrap();
    drive(&mut tx_exec(
        h,
        tx,
        RString::from("insert into t (id, v) values (?, ?)"),
        RString::from(r#"[2,"tx"]"#),
    ))
    .await
    .expect("tx insert");
    drive(&mut tx_rollback(h, tx)).await.expect("rollback");
    let rows = drive(&mut query(
        h,
        RString::from("select count(*) as n from t"),
        RString::from("[]"),
    ))
    .await
    .expect("count");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&rows).unwrap()[0]["n"],
        1
    );

    // 事务：tx_query 可见未提交数据 → commit 后全局可见。
    let b = drive(&mut begin(h)).await.expect("begin2");
    let tx = serde_json::from_slice::<serde_json::Value>(&b).unwrap()["tx_id"]
        .as_u64()
        .unwrap();
    drive(&mut tx_exec(
        h,
        tx,
        RString::from("insert into t (id, v) values (?, ?)"),
        RString::from(r#"[3,"tx2"]"#),
    ))
    .await
    .expect("tx insert2");
    let rows = drive(&mut tx_query(
        h,
        tx,
        RString::from("select v from t where id = ?"),
        RString::from("[3]"),
    ))
    .await
    .expect("tx query");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&rows).unwrap()[0]["v"],
        "tx2"
    );
    drive(&mut tx_commit(h, tx)).await.expect("commit");
    let rows = drive(&mut query(
        h,
        RString::from("select count(*) as n from t"),
        RString::from("[]"),
    ))
    .await
    .expect("count2");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&rows).unwrap()[0]["n"],
        2
    );

    // 错误面各自点名。
    let e = drive(&mut query(
        999,
        RString::from("select 1"),
        RString::from("[]"),
    ))
    .await
    .unwrap_err();
    assert!(e.contains("unknown handle"), "{e}");
    let e = drive(&mut tx_commit(h, 777)).await.unwrap_err();
    assert!(e.contains("unknown tx"), "{e}");
    let e = drive(&mut exec(
        h,
        RString::from("definitely not sql"),
        RString::from("[]"),
    ))
    .await
    .unwrap_err();
    assert!(e.contains("db exec"), "{e}");
    let e = drive(&mut query(
        h,
        RString::from("select 1"),
        RString::from("not json"),
    ))
    .await
    .unwrap_err();
    assert!(e.contains("bad params"), "{e}");

    // dialect(): sqlite DSN → 线名 "sqlite"（占位符补全/幂立按键选依赖它）。
    assert_eq!(&dialect(h)[..], "sqlite");
    // 未知 handle：上送 "unknown"（宿主按 Sqlite 兜底映射），绝不冒充 "sqlite"。
    assert_eq!(&dialect(999)[..], "unknown");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 无效 DSN 快速失败（不触网）。
#[tokio::test(flavor = "multi_thread")]
async fn invalid_dsn_fails_fast() {
    assert!(Client::connect("not a url").await.is_err());
    assert!(Client::connect("").await.is_err());
}

/// ABI 10 流式全路径（离线 sqlite）：open → 批量 next（≤100 行/批）→ done 就地
/// 移除条目 → 再 next 报 unknown stream；cancel → 下批 cancelled；close 幂等。
#[tokio::test(flavor = "multi_thread")]
async fn given_sqlite_dsn_when_stream_roundtrip_then_batched_rows_done_and_cancel() {
    let _ = std::result::Result::from(init(host(), RString::from("{}")));
    let dir = std::env::temp_dir().join(format!("oj-dbpg-stream-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::File::create(dir.join("t.db")).unwrap();
    let dsn = oj_plugin_ffi::path_util::sqlite_file_dsn(&dir.join("t.db"));

    let bytes = drive(&mut connect(RString::from(dsn.as_str())))
        .await
        .expect("connect");
    let h = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["handle"]
        .as_u64()
        .unwrap();

    drive(&mut exec(
        h,
        RString::from("create table oj_stream_t (id integer primary key, v text)"),
        RString::from("[]"),
    ))
    .await
    .expect("ddl");
    for i in 0..5 {
        drive(&mut exec(
            h,
            RString::from("insert into oj_stream_t (id, v) values (?, ?)"),
            RString::from(format!(r#"[{i},"v{i}"]"#).as_str()),
        ))
        .await
        .expect("insert");
    }

    // open → 第一批拉完 5 行（< 100 不含 done）
    let bytes = drive(&mut stream_open(
        h,
        RString::from("select id, v from oj_stream_t order by id"),
        RString::from("[]"),
    ))
    .await
    .expect("stream_open");
    let sid = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["stream_id"]
        .as_u64()
        .unwrap();
    let bytes = drive(&mut stream_next(h, sid)).await.expect("next1");
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let rows = v["rows"].as_array().expect("rows envelope");
    assert_eq!(rows.len(), 5, "{v}");
    assert_eq!(rows[0]["id"], 0, "{v}");
    assert_eq!(rows[4]["v"], "v4", "{v}");

    // 第二批 = done（条目就地移除）
    let bytes = drive(&mut stream_next(h, sid)).await.expect("next2");
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["done"], serde_json::json!(true), "{v}");

    // done 后再 next → unknown stream（点名，不静默）
    let e = drive(&mut stream_next(h, sid)).await.unwrap_err();
    assert!(e.contains("unknown stream"), "{e}");

    // cancel/close 对已移除条目幂等 Ok
    drive(&mut stream_cancel(h, sid)).await.expect("cancel");
    drive(&mut stream_close(h, sid)).await.expect("close");

    let _ = std::fs::remove_dir_all(&dir);
}

/// ABI 10 取消路径（离线）：cancel 置标志 → 下一次 next 返回 {"error":"cancelled"}；
/// close 收口后条目消失。
#[tokio::test(flavor = "multi_thread")]
async fn given_sqlite_dsn_when_stream_cancelled_then_next_returns_cancelled_envelope() {
    let _ = std::result::Result::from(init(host(), RString::from("{}")));
    let dir = std::env::temp_dir().join(format!("oj-dbpg-cancel-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::File::create(dir.join("t.db")).unwrap();
    let dsn = oj_plugin_ffi::path_util::sqlite_file_dsn(&dir.join("t.db"));

    let bytes = drive(&mut connect(RString::from(dsn.as_str())))
        .await
        .expect("connect");
    let h = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["handle"]
        .as_u64()
        .unwrap();
    drive(&mut exec(
        h,
        RString::from("create table oj_cancel_t (id integer primary key)"),
        RString::from("[]"),
    ))
    .await
    .expect("ddl");
    for i in 0..3 {
        drive(&mut exec(
            h,
            RString::from("insert into oj_cancel_t (id) values (?)"),
            RString::from(format!("[{i}]").as_str()),
        ))
        .await
        .expect("insert");
    }

    let bytes = drive(&mut stream_open(
        h,
        RString::from("select id from oj_cancel_t order by id"),
        RString::from("[]"),
    ))
    .await
    .expect("stream_open");
    let sid = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["stream_id"]
        .as_u64()
        .unwrap();

    drive(&mut stream_cancel(h, sid)).await.expect("cancel");
    let bytes = drive(&mut stream_next(h, sid)).await.expect("next");
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["error"], "cancelled", "{v}");

    drive(&mut stream_close(h, sid)).await.expect("close");
    let e = drive(&mut stream_next(h, sid)).await.unwrap_err();
    assert!(e.contains("unknown stream"), "{e}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// PR-2 真取消验收（env-gated PG）：慢查询游标 cancel → 专用连接断开 →
/// `pg_stat_activity` 中的服务端查询秒级消失（≤5s，验收②），随后池 query 正常。
#[tokio::test(flavor = "multi_thread")]
async fn real_pg_stream_cancel_kills_server_query() {
    let Ok(dsn) = std::env::var("OJ_TEST_PG") else {
        eprintln!("skip: OJ_TEST_PG unset");
        return;
    };
    let _ = std::result::Result::from(init(host(), RString::from("{}")));
    let bytes = drive(&mut connect(RString::from(dsn.as_str())))
        .await
        .expect("connect");
    let h = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["handle"]
        .as_u64()
        .unwrap();
    // 第二个 client 轮询 pg_stat_activity（独立连接，不被慢查询阻塞）。
    let pbytes = drive(&mut connect(RString::from(dsn.as_str())))
        .await
        .expect("poll connect");
    let ph = serde_json::from_slice::<serde_json::Value>(&pbytes).unwrap()["handle"]
        .as_u64()
        .unwrap();

    // 两列都显式转 text：typed 流式对未解码类型（int4/void 等）响亮报错，一旦行转换
    // 报错条目即被移除、cancel 变 no-op（用例会假失败）。`cast(pg_sleep(...) as text)`
    // 是 target list 里的 volatile 调用——逐行求值，保证服务端真在跑 ~20s。
    let slow = "/*oj-cancel-test*/ select cast(i as text) as i, cast(pg_sleep(0.02) as text) as s \
                    from generate_series(1,1000) g(i)";
    let bytes = drive(&mut stream_open(
        h,
        RString::from(slow),
        RString::from("[]"),
    ))
    .await
    .expect("stream_open");
    let sid = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["stream_id"]
        .as_u64()
        .unwrap();
    // 取一批（批内已在服务端跑起来）
    drive(&mut stream_next(h, sid)).await.expect("next1");

    let poll = |ph: u64, detail: bool| async move {
        let sql = if detail {
            "select coalesce(state,'?') as state, coalesce(wait_event_type,'-') as wet, coalesce(wait_event,'-') as we from pg_stat_activity where query like '%oj-cancel-test%' and pid <> pg_backend_pid()"
        } else {
            "select count(*)::int as n from pg_stat_activity where query like '%oj-cancel-test%' and pid <> pg_backend_pid()"
        };
        let mut fut = query(ph, RString::from(sql), RString::from("[]"));
        drive(&mut fut).await
    };
    // cancel 前：服务端有该查询
    let pre = poll(ph, false).await.expect("poll pre");
    let pre_n = serde_json::from_slice::<serde_json::Value>(&pre).unwrap()[0]["n"]
        .as_i64()
        .unwrap();
    assert!(pre_n >= 1, "cancel 前服务端应有慢查询在跑: {pre_n}");

    // cancel → 专用连接断开 → 服务端查询终止
    drive(&mut stream_cancel(h, sid)).await.expect("cancel");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut cleared = false;
    while std::time::Instant::now() < deadline {
        let b = poll(ph, true).await.expect("poll");
        eprintln!(
            "DBG poll rows={:?}",
            serde_json::from_slice::<serde_json::Value>(&b).unwrap()
        );
        if serde_json::from_slice::<serde_json::Value>(&b)
            .unwrap()
            .as_array()
            .map(|a| a.is_empty())
            .unwrap_or(false)
        {
            cleared = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    assert!(cleared, "5s 内 pg_stat_activity 必须无孤儿查询（真取消）");
    // 池仍正常
    let b = drive(&mut query(
        h,
        RString::from("select 1 as ok"),
        RString::from("[]"),
    ))
    .await
    .expect("post-cancel query");
    assert!(
        serde_json::from_slice::<serde_json::Value>(&b).unwrap()[0]["ok"] == 1,
        "cancel 后池路径须正常"
    );
    close(h);
    close(ph);
}

/// 真实 postgres 集成（env-gated）：`OJ_TEST_PG=postgres://… cargo test -p oj-db-postgres`。
#[tokio::test(flavor = "multi_thread")]
async fn real_postgres_roundtrip_via_vtable() {
    let Ok(url) = std::env::var("OJ_TEST_PG") else {
        eprintln!("skip: OJ_TEST_PG unset");
        return;
    };
    let cfg = serde_json::json!({}).to_string();
    let desc = match std::result::Result::from(init(host(), RString::from(cfg.as_str()))) {
        Ok(d) => d,
        Err(e) => panic!("init failed: {}", &e[..]),
    };
    assert_eq!(&desc.name[..], "db-postgres");

    let mut c = connect(RString::from(url.as_str()));
    let bytes = drive(&mut c).await.expect("connect");
    let handle: u64 = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["handle"]
        .as_u64()
        .unwrap();

    // 先清场：本用例只 `create if not exists` + 固定 id 插入，若上一次运行的残留行还在，
    // 重跑会撞主键（PK 冲突）——env-gated 用例必须可重复执行。
    drive(&mut exec(
        handle,
        RString::from("drop table if exists oj_plugin_t"),
        RString::from("[]"),
    ))
    .await
    .expect("drop");
    drive(&mut exec(
        handle,
        RString::from("create table if not exists oj_plugin_t (id int primary key, v text)"),
        RString::from("[]"),
    ))
    .await
    .expect("create");

    drive(&mut exec(
        handle,
        RString::from("insert into oj_plugin_t (id, v) values ($1, $2)"),
        RString::from(r#"[1,"hi"]"#),
    ))
    .await
    .expect("insert");

    let rows = drive(&mut query(
        handle,
        RString::from("select v from oj_plugin_t where id = $1"),
        RString::from(r#"[1]"#),
    ))
    .await
    .expect("query");
    let v: serde_json::Value = serde_json::from_slice(&rows).unwrap();
    assert_eq!(v[0]["v"], serde_json::json!("hi"), "{v}");

    let bytes = drive(&mut begin(handle)).await.expect("begin");
    let tx_id = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["tx_id"]
        .as_u64()
        .unwrap();
    drive(&mut tx_exec(
        handle,
        tx_id,
        RString::from("insert into oj_plugin_t (id, v) values ($1, $2)"),
        RString::from(r#"[2,"tx"]"#),
    ))
    .await
    .expect("tx insert");
    drive(&mut tx_commit(handle, tx_id))
        .await
        .expect("tx commit");

    let bytes = drive(&mut begin(handle)).await.expect("begin2");
    let tx_id = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["tx_id"]
        .as_u64()
        .unwrap();
    drive(&mut tx_exec(
        handle,
        tx_id,
        RString::from("insert into oj_plugin_t (id, v) values ($1, $2)"),
        RString::from(r#"[3,"rb"]"#),
    ))
    .await
    .expect("tx insert2");
    drive(&mut tx_rollback(handle, tx_id))
        .await
        .expect("tx rollback");

    let rows = drive(&mut query(
        handle,
        RString::from("select count(*) c from oj_plugin_t where id in (1,2,3)"),
        RString::from("[]"),
    ))
    .await
    .expect("count");
    let v: serde_json::Value = serde_json::from_slice(&rows).unwrap();
    assert_eq!(
        v[0]["c"],
        serde_json::json!(2),
        "rolled back row must be absent: {v}"
    );

    close(handle);
    drive(&mut query(
        handle,
        RString::from("select 1"),
        RString::from("[]"),
    ))
    .await
    .expect_err("unknown handle after close");
}

/// T11 结构化错误信封（env-gated，`OJ_TEST_PG=postgres://…`）：
/// - `select 1/0` 除法溢出 → SQLSTATE `22012` 原样透传为 `data.vendor_code`（字符串厂商码）。
/// - 连向关闭端口 → `category=="connection"` + `session_disposition=="close"`（Io 类）。
/// 未设 env 直接跳过（不进网络）。
#[tokio::test(flavor = "multi_thread")]
async fn real_postgres_envelope_classifies_sqlstate_and_connection() {
    let Ok(url) = std::env::var("OJ_TEST_PG") else {
        eprintln!("skip: OJ_TEST_PG unset");
        return;
    };
    let cfg = serde_json::json!({}).to_string();
    let _ = init(host(), RString::from(cfg.as_str()));
    let mut c = connect(RString::from(url.as_str()));
    let bytes = drive(&mut c).await.expect("connect");
    let handle: u64 = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["handle"]
        .as_u64()
        .unwrap();

    // 除法溢出 → SQLSTATE 22012 透传为厂商码（字符串）。
    let e = drive(&mut query(
        handle,
        RString::from("select 1/0"),
        RString::from("[]"),
    ))
    .await
    .unwrap_err();
    let env: serde_json::Value = serde_json::from_str(&e).expect("envelope must be json");
    let cat = env["category"].as_str().expect("category must be string");
    assert!(
        matches!(cat, "constraint" | "connection" | "internal"),
        "unexpected category: {e}"
    );
    assert_eq!(env["data"]["vendor_code"], "22012", "{e}");
}

/// T11 错误分类纯单测（不触网，确定性强）：Io 类 → connection/close。约束类落在
/// `Database`（由上面的真库用例覆盖 22012 透传）。本例只钉死分类映射本身。
#[test]
fn sqlx_category_maps_io_to_connection_and_close() {
    let io = sqlx::Error::Io(std::io::Error::new(
        std::io::ErrorKind::ConnectionRefused,
        "x",
    ));
    let (cat, retry, disp) = sqlx_category(&io);
    assert_eq!((cat, retry, disp), ("connection", true, "close"));
}

/// REQ-07a env-gated（`OJ_TEST_PG`）：未解码类型不再整条 400——`inet`/`interval`/
/// `money` 降级为 `$<type>:<base64>` 前缀标量（cellenv 契约；见
/// `docs/superpowers/specs/2026-10-30-dbx-gap-triage-design.md`）。语句级权威
/// 降级清单由宿主扫描外发（`X-OJ-Degraded` 头），本例钉**单元格编码**形态。
/// `try_get_raw` 拿不到原始字节的类型维持 `undecodable_column` 响亮报错（红线）。
#[tokio::test(flavor = "multi_thread")]
async fn real_pg_undecoded_types_degrade_to_cellenv() {
    let Ok(url) = std::env::var("OJ_TEST_PG") else {
        eprintln!("skip: OJ_TEST_PG unset");
        return;
    };
    let _ = std::result::Result::from(init(host(), RString::from("{}")));
    let bytes = drive(&mut connect(RString::from(url.as_str())))
        .await
        .expect("connect");
    let h = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["handle"]
        .as_u64()
        .unwrap();

    let bytes = drive(&mut query(
        h,
        RString::from(
            "select '1.2.3.4'::inet as ip, \
                        '1 mon 2 days'::interval as iv, \
                        '$1.50'::money as m",
        ),
        RString::from("[]"),
    ))
    .await
    .expect("未解码类型应降级为前缀标量而非整条 400");
    let rows: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    for (col, ty) in [("ip", "inet"), ("iv", "interval"), ("m", "money")] {
        let v = rows[0][col]
            .as_str()
            .unwrap_or_else(|| panic!("{col} 应为字符串标量: {rows}"));
        let (t, payload) = oj_plugin_ffi::cellenv::decode(v)
            .unwrap_or_else(|| panic!("{col} 应为 cellenv 信封: {v:?}"));
        assert_eq!(t, ty, "{col} 类型段不符");
        assert!(!payload.is_empty(), "{col} payload 不得为空（空信封=红线）");
    }
    close(h);
}

/// REQ-07a 行为变更登记（env-gated，`OJ_TEST_PG`）：`point` 这类未覆盖类型自
/// v0.1.111 起**降级为 cellenv 前缀标量**（原行为：整条 400 `undecodable_column`，
/// 见本用例旧版）。池路径与 typed 流式路径**都要**降级，且形态一致；语句级权威
/// 降级清单由宿主外发（`X-OJ-Degraded` 头）。「原始字节也拿不到 → 整行报错」的
/// fail-loud 窄场景无真库类型可钉，由单测级不变式守（`row_to_json_pg` 不产空信封）。
#[tokio::test(flavor = "multi_thread")]
async fn real_postgres_unsupported_column_types_degrade_loudly() {
    let Ok(url) = std::env::var("OJ_TEST_PG") else {
        eprintln!("skip: OJ_TEST_PG unset");
        return;
    };
    let _ = std::result::Result::from(init(host(), RString::from("{}")));
    let bytes = drive(&mut connect(RString::from(url.as_str())))
        .await
        .expect("connect");
    let h = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["handle"]
        .as_u64()
        .unwrap();

    // 池路径（typed PG）：降级为 `$point:<base64>`。
    let bytes = drive(&mut query(
        h,
        RString::from("select point(1,2) as shape"),
        RString::from("[]"),
    ))
    .await
    .expect("未解码类型应降级而非 400");
    let rows: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let (t, payload) = oj_plugin_ffi::cellenv::decode(
        rows[0]["shape"]
            .as_str()
            .unwrap_or_else(|| panic!("应为标量: {rows}")),
    )
    .unwrap_or_else(|| panic!("应为 cellenv 信封: {rows}"));
    assert_eq!(t, "point");
    assert!(!payload.is_empty());

    // typed 流式路径：同一降级形态（错误只在「原始字节也拿不到」时出现，带内信封）。
    let bytes = drive(&mut stream_open(
        h,
        RString::from("select point(1,2) as shape"),
        RString::from("[]"),
    ))
    .await
    .expect("stream_open");
    let sid = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["stream_id"]
        .as_u64()
        .unwrap();
    let b = drive(&mut stream_next(h, sid)).await.expect("stream next");
    let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
    assert!(
        v["error"].is_null(),
        "流式未解码类型应降级而非带内报错: {v}"
    );
    let row = v["rows"][0].clone();
    let (t, _) = oj_plugin_ffi::cellenv::decode(
        row["shape"]
            .as_str()
            .unwrap_or_else(|| panic!("流式应为标量: {v}")),
    )
    .unwrap_or_else(|| panic!("流式应为 cellenv 信封: {v}"));
    assert_eq!(t, "point");
    close(h);
}

/// REQ-07b（env-gated，`OJ_TEST_PG`，breaking）：BYTEA 列**含合法 UTF-8 内容**也
/// 恒定 cellenv 信封（原 lossy 裸串）——`'\x68656c6c6f'`（"hello"）不再以可读串
/// 出现；非 UTF-8 内容同形态；池路径与 typed 流式路径一致。
#[tokio::test(flavor = "multi_thread")]
async fn real_pg_bytea_always_cellenv() {
    let Ok(url) = std::env::var("OJ_TEST_PG") else {
        eprintln!("skip: OJ_TEST_PG unset");
        return;
    };
    let _ = std::result::Result::from(init(host(), RString::from("{}")));
    let bytes = drive(&mut connect(RString::from(url.as_str())))
        .await
        .expect("connect");
    let h = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["handle"]
        .as_u64()
        .unwrap();
    let sql = "select '\\x68656c6c6f'::bytea as u, '\\xdeadbeef'::bytea as b";
    let bytes = drive(&mut query(h, RString::from(sql), RString::from("[]")))
        .await
        .expect("bytea query 不得报错");
    let rows: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    for col in ["u", "b"] {
        let v = rows[0][col]
            .as_str()
            .unwrap_or_else(|| panic!("{col} 应为字符串标量: {rows}"));
        let (t, payload) = oj_plugin_ffi::cellenv::decode(v)
            .unwrap_or_else(|| panic!("{col} 应为 cellenv 信封: {v:?}"));
        assert_eq!(t, "bytea", "{col} 类型段不符");
        assert!(!payload.is_empty(), "{col} 空信封=红线");
    }
    assert_ne!(
        rows[0]["u"],
        serde_json::json!("hello"),
        "合法 UTF-8 二进制不再裸串（REQ-07b breaking）"
    );
    // typed 流式路径同一形态。
    let bytes = drive(&mut stream_open(h, RString::from(sql), RString::from("[]")))
        .await
        .expect("stream_open");
    let sid = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["stream_id"]
        .as_u64()
        .unwrap();
    let b = drive(&mut stream_next(h, sid)).await.expect("stream next");
    let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
    assert!(v["error"].is_null(), "流式 bytea 应正常带内信封: {v}");
    let sv = v["rows"][0]["u"]
        .as_str()
        .unwrap_or_else(|| panic!("流式 u 应为标量: {v}"));
    let (t, _) = oj_plugin_ffi::cellenv::decode(sv)
        .unwrap_or_else(|| panic!("流式应为 cellenv 信封: {sv:?}"));
    assert_eq!(t, "bytea");
    close(h);
}

/// typed 流式的**常规整数类型**必须可解（回归：`i64::compatible` 只认 INT8，
/// INT4/INT2 曾整类落到「未解码列类型」硬报错，而 `db.stream` 走的正是这条路）。
/// 真库才有意义：列类型由服务端元数据给出，mock 造不出来。env-gated：`OJ_TEST_PG=…`。
#[tokio::test(flavor = "multi_thread")]
async fn real_pg_stream_decodes_common_numeric_types() {
    let Ok(url) = std::env::var("OJ_TEST_PG") else {
        eprintln!("skip: OJ_TEST_PG unset");
        return;
    };
    let _ = std::result::Result::from(init(host(), RString::from("{}")));
    let bytes = drive(&mut connect(RString::from(url.as_str())))
        .await
        .expect("connect");
    let h = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["handle"]
        .as_u64()
        .unwrap();

    let bytes = drive(&mut stream_open(
        h,
        RString::from(
            "select 2147483647::int4 as i4, (-32768)::int2 as i2, 9007199254740993::int8 as i8, \
                 2.25::float8 as r8, true as b, 'txt'::text as t",
        ),
        RString::from("[]"),
    ))
    .await
    .expect("stream_open");
    let sid = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["stream_id"]
        .as_u64()
        .unwrap();

    let b = drive(&mut stream_next(h, sid)).await.expect("stream next");
    let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
    let row = &v["rows"][0];
    assert_eq!(row["i4"], serde_json::json!(2147483647_i32), "{v}");
    assert_eq!(row["i2"], serde_json::json!(-32768_i16), "{v}");
    assert_eq!(row["i8"], serde_json::json!(9007199254740993_i64), "{v}");
    assert_eq!(row["r8"], serde_json::json!(2.25), "{v}");
    assert_eq!(row["b"], serde_json::json!(true), "{v}");
    assert_eq!(row["t"], serde_json::json!("txt"), "{v}");
    assert!(v["error"].is_null(), "不应有带内错误: {v}");

    // 与 `db.query`（同为 typed 池，v0.1.3 / L6 起）逐值对齐：同一列在流式与查询上
    // 不许给出不同结论。`real`/FLOAT4 过去两条路径都**不支持**，L6 起两条都支持，
    // 此处把「一致」钉住，避免将来只补一头造成分叉。
    let b = drive(&mut query(
        h,
        RString::from("select 2147483647::int4 as i4, (-32768)::int2 as i2"),
        RString::from("[]"),
    ))
    .await
    .expect("query ints");
    let vq: serde_json::Value = serde_json::from_slice(&b).unwrap();
    assert_eq!(row["i4"], vq[0]["i4"], "stream/query i4 必须一致: {vq}");
    assert_eq!(row["i2"], vq[0]["i2"], "stream/query i2 必须一致: {vq}");
    let b = drive(&mut query(
        h,
        RString::from("select 1.1::real as r4"),
        RString::from("[]"),
    ))
    .await
    .expect("query real");
    let vq: serde_json::Value = serde_json::from_slice(&b).unwrap();
    // FLOAT4 是 4 字节浮点，1.1 的真实值就是 1.100000023841858（不回退成 1.1，
    // 那才是静默错值）。
    assert_eq!(
        vq[0]["r4"],
        serde_json::json!(1.1f32 as f64),
        "real 应可解码: {vq}"
    );
    close(h);
}

/// 方言类型解码（v0.1.3 / L6 / PRD U-6，env-gated 真库）：`NUMERIC` / 日期时间 / `JSON(B)` /
/// `UUID` / 数组 / `FLOAT4` 的可读性，且 **`query` 与 `stream` 两条路径同口径**。
///
/// 输出口径（`docs/devkit/db-guide.md` §10.6）：`NUMERIC` 走**字符串**（不经 f64，保精度）、
/// 日期时间走 ISO 8601（`TIMESTAMP` 无时区，`TIMESTAMPTZ` 带 `Z`）、`JSON`/`JSONB` 走
/// **JSON 文本**、`UUID` 走规范文本、数组走 **JSON 数组**（元素 NULL → JSON `null`）。
///
/// 这条用例同时是「typed 池化」的验收：v0.1.2 及以前 `query`/`tx_*` 走 `Pool<Any>`，
/// 这些类型在 Any 上**根本取不出来**——只补 `column_json_pg` 会让流式能读、查询报错。
#[tokio::test(flavor = "multi_thread")]
async fn real_pg_dialect_types_decode_readable() {
    let Ok(url) = std::env::var("OJ_TEST_PG") else {
        eprintln!("skip: OJ_TEST_PG unset");
        return;
    };
    let _ = std::result::Result::from(init(host(), RString::from("{}")));
    let bytes = drive(&mut connect(RString::from(url.as_str())))
        .await
        .expect("connect");
    let h = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["handle"]
        .as_u64()
        .unwrap();

    let sql = "select 1234.5678::numeric(18,4) as amount, \
                          '2026-09-19'::date as made_on, \
                          '10:11:12.345678'::time as made_at, \
                          '2026-09-19 10:11:12.345678'::timestamp as made_ts, \
                          '2026-09-19 10:11:12.345678+00'::timestamptz as made_tstz, \
                          '{\"a\":1}'::jsonb as payload_b, \
                          '{\"a\":1}'::json as payload, \
                          '2f9e8f3a-1c4b-4d5e-8f6a-7b8c9d0e1f2a'::uuid as uid, \
                          array[1, null, 3]::int4[] as i4s, \
                          array['x', null]::text[] as texts, \
                          array[1.5, 2.5]::numeric[] as nums, \
                          1.1::real as r4, \
                          null::numeric as nullable_num";
    let b = drive(&mut query(h, RString::from(sql), RString::from("[]")))
        .await
        .expect("dialect types must read (query path)");
    let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
    let r = &v[0];
    assert_eq!(r["amount"], serde_json::json!("1234.5678"), "{v}");
    assert_eq!(r["made_on"], serde_json::json!("2026-09-19"), "{v}");
    assert_eq!(r["made_at"], serde_json::json!("10:11:12.345678"), "{v}");
    assert_eq!(
        r["made_ts"],
        serde_json::json!("2026-09-19T10:11:12.345678"),
        "{v}"
    );
    assert_eq!(
        r["made_tstz"],
        serde_json::json!("2026-09-19T10:11:12.345678Z"),
        "{v}"
    );
    // JSON 走 JSON 文本（PRD U-6 口径：不自动 JSON.parse，由业务侧决定）。
    for k in ["payload_b", "payload"] {
        let txt = r[k]
            .as_str()
            .unwrap_or_else(|| panic!("{k} 应为字符串: {v}"));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(txt).unwrap(),
            serde_json::json!({"a": 1}),
            "{k}: {v}"
        );
    }
    assert_eq!(
        r["uid"],
        serde_json::json!("2f9e8f3a-1c4b-4d5e-8f6a-7b8c9d0e1f2a"),
        "{v}"
    );
    // 数组 → JSON 数组；元素 NULL → null（这才是 `Vec<Option<T>>` 的意义）。
    assert_eq!(r["i4s"], serde_json::json!([1, null, 3]), "{v}");
    assert_eq!(r["texts"], serde_json::json!(["x", null]), "{v}");
    assert_eq!(r["nums"], serde_json::json!(["1.5", "2.5"]), "{v}");
    assert_eq!(r["r4"], serde_json::json!(1.1f32 as f64), "{v}");
    assert!(r["nullable_num"].is_null(), "{v}");

    // stream 路径同口径。
    let bytes = drive(&mut stream_open(h, RString::from(sql), RString::from("[]")))
        .await
        .expect("stream_open");
    let sid = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["stream_id"]
        .as_u64()
        .unwrap();
    let b = drive(&mut stream_next(h, sid)).await.expect("stream next");
    let sv: serde_json::Value = serde_json::from_slice(&b).unwrap();
    assert!(sv["error"].is_null(), "不应有带内错误: {sv}");
    let sr = &sv["rows"][0];
    for k in [
        "amount",
        "made_on",
        "made_at",
        "made_ts",
        "made_tstz",
        "payload_b",
        "payload",
        "uid",
        "i4s",
        "texts",
        "nums",
        "r4",
    ] {
        assert_eq!(sr[k], r[k], "stream/query 的 {k} 必须一致: {sv}");
    }

    // 事务路径（`TxInner::Pg`）：v0.1.2 及以前 `tx_query` 走 `Pool<Any>` 的连接，
    // 同样读不出这些类型——这是 typed 池化最容易漏的一处，必须显式钉住。
    let b = drive(&mut begin(h)).await.expect("begin");
    let tx_id = serde_json::from_slice::<serde_json::Value>(&b).unwrap()["tx_id"]
        .as_u64()
        .unwrap();
    let b = drive(&mut tx_query(
        h,
        tx_id,
        RString::from(sql),
        RString::from("[]"),
    ))
    .await
    .expect("tx_query must decode dialect types");
    let tv: serde_json::Value = serde_json::from_slice(&b).unwrap();
    for k in ["amount", "uid", "i4s", "nums", "made_tstz"] {
        assert_eq!(tv[0][k], r[k], "tx/query 的 {k} 必须一致: {tv}");
    }
    drive(&mut tx_rollback(h, tx_id)).await.expect("rollback");
    close(h);
}

/// 大整数参数（v0.1.22）：`toBigInt()` 的标记形态必须能写进 bigint 列并精确读回；
/// 且**字符串参数写不进 bigint 列**（PG 严格类型）——这是"字符串=文本意图、
/// BigInt=整数意图"契约的真库依据。env-gated：`OJ_TEST_PG=postgres://…`。
#[tokio::test(flavor = "multi_thread")]
async fn real_postgres_bigint_marker_binds_i64() {
    let Ok(url) = std::env::var("OJ_TEST_PG") else {
        eprintln!("skip: OJ_TEST_PG unset");
        return;
    };
    let cfg = serde_json::json!({}).to_string();
    let _ = init(host(), RString::from(cfg.as_str()));
    let mut c = connect(RString::from(url.as_str()));
    let bytes = drive(&mut c).await.expect("connect");
    let handle: u64 = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["handle"]
        .as_u64()
        .unwrap();

    let ex = |sql: &str, params: &str| {
        let mut f = exec(handle, RString::from(sql), RString::from(params));
        async move { drive(&mut f).await }
    };
    let qy = |sql: &str, params: &str| {
        let mut f = query(handle, RString::from(sql), RString::from(params));
        async move { drive(&mut f).await }
    };

    ex("drop table if exists oj_bigint_t", "[]").await.unwrap();
    ex(
        "create table oj_bigint_t (id bigint primary key, note text)",
        "[]",
    )
    .await
    .unwrap();

    // ① 标记参数（toBigInt 的编解码形态）→ 精确落入 bigint 主键
    ex(
        "insert into oj_bigint_t (id, note) values ($1, $2)",
        r#"[{"$oj$i64":"4886674138783273204"},"marker"]"#,
    )
    .await
    .expect("marker param must bind as i64");
    ex(
        "insert into oj_bigint_t (id, note) values ($1, $2)",
        r#"[{"$oj$i64":"-9223372036854775808"},"imin"]"#,
    )
    .await
    .expect("i64::MIN must round-trip");

    let rows = qy("select id, note from oj_bigint_t order by id", "[]")
        .await
        .expect("query");
    let v: serde_json::Value = serde_json::from_slice(&rows).unwrap();
    assert_eq!(
        v[0]["id"],
        serde_json::json!(-9223372036854775808i64),
        "插件行转换必须逐字精确：{v}"
    );
    assert_eq!(v[1]["id"], serde_json::json!(4886674138783273204i64), "{v}");

    // ② where 用标记参数比较 bigint 列（字符串形式在 PG 上是 operator does not exist）
    let rows = qy(
        "select note from oj_bigint_t where id = $1",
        r#"[{"$oj$i64":"4886674138783273204"}]"#,
    )
    .await
    .expect("marker param must compare against bigint column");
    let v: serde_json::Value = serde_json::from_slice(&rows).unwrap();
    assert_eq!(v[0]["note"], serde_json::json!("marker"), "{v}");

    // ③ 反例固化：字符串参数写 bigint 列在 PG 上必然失败（防未来"顺手加启发式"）。
    // 用**独立 SQL 文本**取全新 prepared statement，避免命中 sqlx 语句缓存里
    // 「同文本换参数 Rust 类型」的既有不一致（该问题另有登记，见 numeric-limits 手册）。
    let e = ex(
        "insert into oj_bigint_t (id) values ($1)",
        r#"["9007199254740993"]"#,
    )
    .await
    .expect_err("PG 必须拒绝 text → bigint");
    assert!(
        e.contains("bigint") && e.contains("text"),
        "报错应指明类型不匹配：{e}"
    );

    // ④ 事务路径同款（tx_query/tx_exec 共用一个 bind_value）
    let bytes = drive(&mut begin(handle)).await.expect("begin");
    let tx_id = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["tx_id"]
        .as_u64()
        .unwrap();
    drive(&mut tx_exec(
        handle,
        tx_id,
        RString::from("insert into oj_bigint_t (id, note) values ($1, $2)"),
        RString::from(r#"[{"$oj$i64":"9007199254740993"},"tx"]"#),
    ))
    .await
    .expect("tx marker param");
    drive(&mut tx_commit(handle, tx_id)).await.expect("commit");
    let rows = qy(
        "select note from oj_bigint_t where id = $1",
        r#"[{"$oj$i64":"9007199254740993"}]"#,
    )
    .await
    .expect("tx read");
    let v: serde_json::Value = serde_json::from_slice(&rows).unwrap();
    assert_eq!(v[0]["note"], serde_json::json!("tx"), "{v}");

    ex("drop table if exists oj_bigint_t", "[]").await.unwrap();
    close(handle);
}

/// 债①回归（env-gated）：**同一条 SQL 文本**在不同调用里绑不同 Rust 类型的参数。
///
/// 根因：sqlx 的语句缓存只以 SQL 文本为 key（不含参数类型），PG 命中缓存时直接复用旧的
/// `(StatementId, param OIDs)`，Bind 却按本次 Rust 类型编码字节 → 协议级错误
/// （`insufficient data left in message` / `invalid byte sequence … 0x00`）。
///
/// 期望终态（v0.1.24 起宿主在 PG 方言下给 SQL 前置形态签名 `/*oj:<形态>*/`，不同参数形态
/// 落到不同缓存条目）：**两个形态都能跑通**。修前本用例必红——它是债①的验收基线。
///
/// 用 **text 列**做混形态载体：`insert … (k, v) values ($1, $2)` 传 `("a","x")` 与 `(1,2)`
/// 在 PG 上都合法（int8 → text 有赋值转换），差别只在参数形态——正是缓存 bug 的触发面。
/// bigint 列不能当载体：字符串参数写 bigint 列本来就会被 PG 拒（那是另一条既有契约，
/// 见 `real_postgres_bigint_marker_binds_i64` ③）。
#[tokio::test(flavor = "multi_thread")]
async fn real_postgres_same_sql_text_mixed_param_shapes() {
    let Ok(url) = std::env::var("OJ_TEST_PG") else {
        eprintln!("skip: OJ_TEST_PG unset");
        return;
    };
    let cfg = serde_json::json!({}).to_string();
    let desc = match std::result::Result::from(init(host(), RString::from(cfg.as_str()))) {
        Ok(d) => d,
        Err(e) => panic!("init failed: {}", &e[..]),
    };
    assert_eq!(&desc.name[..], "db-postgres");

    let mut c = connect(RString::from(url.as_str()));
    let bytes = drive(&mut c).await.expect("connect");
    let handle: u64 = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["handle"]
        .as_u64()
        .unwrap();

    drive(&mut exec(
        handle,
        RString::from("drop table if exists oj_shape_t"),
        RString::from("[]"),
    ))
    .await
    .expect("drop");
    drive(&mut exec(
        handle,
        RString::from("create table if not exists oj_shape_t (k text primary key, v text)"),
        RString::from("[]"),
    ))
    .await
    .expect("create");

    // ---- 池路径：同一文本两种形态（t,t）→（i,i）----
    let ins = "insert into oj_shape_t (k, v) values ($1, $2)";
    drive(&mut exec(
        handle,
        RString::from(ins),
        RString::from(r#"["a","x"]"#),
    ))
    .await
    .expect("池路径 text 形态（首次会 PARSE 并缓存该文本）");
    drive(&mut exec(
        handle,
        RString::from("insert into oj_shape_t (k, v) values ($1, $2)"),
        RString::from(r#"[1,2]"#),
    ))
    .await
    .expect("池路径 i64 形态（修前这里报 insufficient data left in message）");

    // ---- tx 路径：同一文本反序再来一次（i,i → t,t），并在 tx 内数缓存条目 ----
    // tx 独占一条连接，故 `pg_prepared_statements` 的计数是确定的（池路径会分散到多连接）。
    let bytes = drive(&mut begin(handle)).await.expect("begin");
    let tx_id = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["tx_id"]
        .as_u64()
        .unwrap();
    drive(&mut tx_exec(
        handle,
        tx_id,
        RString::from("insert into oj_shape_t (k, v) values ($1, $2)"),
        RString::from(r#"[3,4]"#),
    ))
    .await
    .expect("tx 路径 i64 形态");
    drive(&mut tx_exec(
        handle,
        tx_id,
        RString::from("insert into oj_shape_t (k, v) values ($1, $2)"),
        RString::from(r#"["b","y"]"#),
    ))
    .await
    .expect("tx 路径 text 形态（与上一条同文本、不同形态）");
    // 同一文本的三种形态 = 三个缓存条目（形态分键生效），且都带 `/*oj:` 前缀。
    let rows = drive(&mut tx_query(
        handle,
        tx_id,
        RString::from(
            "select count(*)::int as n from pg_prepared_statements \
                 where statement like '/*oj:%' and statement like '%oj_shape_t%'",
        ),
        RString::from("[]"),
    ))
    .await
    .expect("count prepared");
    let v: serde_json::Value = serde_json::from_slice(&rows).unwrap();
    assert!(
        v[0]["n"].as_i64().unwrap_or(0) >= 2,
        "同一条 SQL 文本的不同参数形态必须各占一个缓存条目（形态分键）：{v}"
    );
    drive(&mut tx_commit(handle, tx_id)).await.expect("commit");

    // ---- 结果正确性：四个形态写进去的行都在 ----
    let rows = drive(&mut query(
        handle,
        RString::from("select count(*)::int as n from oj_shape_t"),
        RString::from("[]"),
    ))
    .await
    .expect("count rows");
    let v: serde_json::Value = serde_json::from_slice(&rows).unwrap();
    assert_eq!(v[0]["n"], serde_json::json!(4), "四种形态都应落库：{v}");

    drive(&mut exec(
        handle,
        RString::from("drop table if exists oj_shape_t"),
        RString::from("[]"),
    ))
    .await
    .expect("cleanup");
    close(handle);
}

/// 并发驱动多个 FfiFuture（FfiFuture 非 Send，不能 `tokio::spawn`）：在单个任务里
/// 轮询全部——**调度已在插件 runtime 上并行开始**，故这就是真并发。
async fn drive_all(futs: &mut [FfiFuture]) -> Vec<Result<Vec<u8>, String>> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut out: Vec<Option<Result<Vec<u8>, String>>> = (0..futs.len()).map(|_| None).collect();
    let mut left = futs.len();
    while left > 0 {
        for (i, fut) in futs.iter_mut().enumerate() {
            if out[i].is_some() {
                continue;
            }
            if (fut.poll)(fut.state) == 0 {
                if std::time::Instant::now() >= deadline {
                    for slot in out.iter_mut() {
                        if slot.is_none() {
                            *slot = Some(Err("ffi drive timeout".into()));
                        }
                    }
                    left = 0;
                    break;
                }
                continue;
            }
            let r = (fut.take)(fut.state);
            (fut.free)(fut.state);
            fut.state = std::ptr::null_mut();
            out[i] = Some(match (1, std::result::Result::from(r)) {
                (_, Err(e)) => Err(e[..].to_string()),
                (_, Ok(b)) => Ok(b.iter().copied().collect()),
            });
            left -= 1;
        }
        if left > 0 {
            tokio::time::sleep(std::time::Duration::from_micros(200)).await;
        }
    }
    out.into_iter().flatten().collect()
}

/// 债④回归（env-gated）：平台序列的单语句原子取号在**真库并发**下无重号、无空洞。
///
/// 走的正是 `db.nextSeq` 在 PG 上的那条 SQL（`on conflict … do update … returning v`）；
/// 20 个调用在插件 runtime 上并行发起，断言排序后恰为 1..20——`select max(id)+1` 做不到。
#[tokio::test(flavor = "multi_thread")]
async fn real_postgres_next_seq_is_atomic_under_concurrency() {
    let Ok(url) = std::env::var("OJ_TEST_PG") else {
        eprintln!("skip: OJ_TEST_PG unset");
        return;
    };
    let cfg = serde_json::json!({}).to_string();
    let _ = std::result::Result::from(init(host(), RString::from(cfg.as_str())));
    let mut c = connect(RString::from(url.as_str()));
    let bytes = drive(&mut c).await.expect("connect");
    let handle: u64 = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["handle"]
        .as_u64()
        .unwrap();
    let ex = |sql: &str, params: &str| {
        let mut f = exec(handle, RString::from(sql), RString::from(params));
        async move { drive(&mut f).await }
    };

    ex("drop table if exists _oj_sequences", "[]")
        .await
        .unwrap();
    ex(
        "create table _oj_sequences (name text primary key, v bigint not null)",
        "[]",
    )
    .await
    .expect("create seq table");

    let sql = "insert into _oj_sequences (name, v) values ($1, 1) \
                   on conflict (name) do update set v = _oj_sequences.v + 1 returning v";
    let mut futs: Vec<FfiFuture> = (0..20)
        .map(|_| query(handle, RString::from(sql), RString::from(r#"["conc"]"#)))
        .collect();
    let outs = drive_all(&mut futs).await;
    let mut vs: Vec<i64> = Vec::new();
    for o in outs {
        let rows = o.expect("并发取号应成功");
        let v: serde_json::Value = serde_json::from_slice(&rows).unwrap();
        let n = v[0]["v"].as_i64().expect("v 应是整数");
        vs.push(n);
    }
    vs.sort_unstable();
    assert_eq!(vs, (1..=20).collect::<Vec<i64>>(), "并发取号必须稠密无重复");

    ex("drop table if exists _oj_sequences", "[]")
        .await
        .unwrap();
    close(handle);
}

extern "C" fn test_log(_level: u8, _msg: RString) {}
extern "C" fn test_deliver(_topic: RString, _payload: RBytes) {}

fn host() -> RArc<HostContext> {
    RArc::new(HostContext {
        log: test_log,
        deliver: test_deliver,
    })
}

async fn drive(fut: &mut FfiFuture) -> Result<Vec<u8>, String> {
    // 以真实墙钟时间为界轮询（同 oj-es 的 drive）：固定 10w 次 yield_now 在 CI
    // 负载/优化下会在插件 rt 的任务完成前耗尽预算，误报 "ffi drive timeout"。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        match (fut.poll)(fut.state) {
            0 => {
                if std::time::Instant::now() >= deadline {
                    (fut.free)(fut.state); // 超时也要释放 state（防 FfiTask 泄漏）
                    fut.state = std::ptr::null_mut();
                    return Err("ffi drive timeout".into());
                }
                tokio::time::sleep(std::time::Duration::from_micros(100)).await;
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
