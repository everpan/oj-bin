use super::*;
use oj_plugin_ffi::RBytes;

/// DSN 前缀 → 方言判定（占位符补全 / lock 句柄等都依赖它，错判即打错方言 SQL）。
#[test]
fn given_dsn_when_dialect_of_then_prefix_decides() {
    assert_eq!(dialect_of("mysql://u:p@h/db"), Dialect::MySql);
    assert_eq!(dialect_of("postgres://u:p@h/db"), Dialect::Postgres);
    assert_eq!(dialect_of("postgresql://u:p@h/db"), Dialect::Postgres);
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

/// 离线 sqlite 全路径 roundtrip（dev 构建统一出 sqlite 驱动，生产 cdylib 仍单方言）：
/// connect → DDL → 参数化 insert → query 行 JSON 形状 → 事务 rollback 不可见 /
/// commit 可见 → unknown handle/tx、bad SQL、bad params 各自点名 → dialect() 线名。
#[tokio::test(flavor = "multi_thread")]
async fn given_sqlite_dsn_when_full_vtable_roundtrip_then_offline_green() {
    let _ = std::result::Result::from(init(host(), RString::from("{}")));
    let dir = std::env::temp_dir().join(format!("oj-dbmys-off-{}", std::process::id()));
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

/// 无效 DSN 快速失败（不触网）：scheme 未知/畸形 URL 在 sqlx 解析期即报错。
#[tokio::test(flavor = "multi_thread")]
async fn invalid_dsn_fails_fast() {
    assert!(Client::connect("not a url").await.is_err());
    assert!(Client::connect("").await.is_err());
}

/// ABI 10 流式全路径（离线 sqlite）：open → 批量 next（≤100 行/批）→ done 就地
/// 移除条目 → 再 next 报 unknown stream；cancel/close 幂等。
#[tokio::test(flavor = "multi_thread")]
async fn given_sqlite_dsn_when_stream_roundtrip_then_batched_rows_done_and_cancel() {
    let _ = std::result::Result::from(init(host(), RString::from("{}")));
    let dir = std::env::temp_dir().join(format!("oj-dbmy-stream-{}", std::process::id()));
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
    let rows = v
        .get("rows")
        .and_then(|r| r.as_array())
        .unwrap_or_else(|| panic!("rows envelope: {v}"));
    assert_eq!(rows.len(), 5, "{v}");
    assert_eq!(rows[0]["id"], 0, "{v}");
    assert_eq!(rows[4]["v"], "v4", "{v}");

    let bytes = drive(&mut stream_next(h, sid)).await.expect("next2");
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["done"], serde_json::json!(true), "{v}");

    let e = drive(&mut stream_next(h, sid)).await.unwrap_err();
    assert!(e.contains("unknown stream"), "{e}");
    drive(&mut stream_cancel(h, sid)).await.expect("cancel");
    drive(&mut stream_close(h, sid)).await.expect("close");

    let _ = std::fs::remove_dir_all(&dir);
}

/// ABI 10 取消路径（离线）：cancel 置标志 → 下一次 next 返回 {"error":"cancelled"}；
/// close 收口后条目消失。
#[tokio::test(flavor = "multi_thread")]
async fn given_sqlite_dsn_when_stream_cancelled_then_next_returns_cancelled_envelope() {
    let _ = std::result::Result::from(init(host(), RString::from("{}")));
    let dir = std::env::temp_dir().join(format!("oj-dbmy-cancel-{}", std::process::id()));
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

/// 真实 mysql 集成（env-gated）：`OJ_TEST_MYSQL=mysql://… cargo test -p oj-db-mysql`。
/// 未设 env → 打印 skip 直接通过（不进网络）。
#[tokio::test(flavor = "multi_thread")]
async fn real_mysql_roundtrip_via_vtable() {
    let Ok(url) = std::env::var("OJ_TEST_MYSQL") else {
        eprintln!("skip: OJ_TEST_MYSQL unset");
        return;
    };
    let cfg = serde_json::json!({}).to_string();
    let desc = match std::result::Result::from(init(host(), RString::from(cfg.as_str()))) {
        Ok(d) => d,
        Err(e) => panic!("init failed: {}", &e[..]),
    };
    assert_eq!(&desc.name[..], "db-mysql");

    let mut c = connect(RString::from(url.as_str()));
    let bytes = drive(&mut c).await.expect("connect");
    let handle: u64 = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["handle"]
        .as_u64()
        .unwrap();

    // 先清场：本用例只 `create if not exists` + 固定 id 插入，残留行会让重跑撞主键。
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
        RString::from("insert into oj_plugin_t (id, v) values (?, ?)"),
        RString::from(r#"[1,"hi"]"#),
    ))
    .await
    .expect("insert");

    let rows = drive(&mut query(
        handle,
        RString::from("select v from oj_plugin_t where id = ?"),
        RString::from(r#"[1]"#),
    ))
    .await
    .expect("query");
    let v: serde_json::Value = serde_json::from_slice(&rows).unwrap();
    assert_eq!(v[0]["v"], serde_json::json!("hi"), "{v}");

    // 事务 commit
    let bytes = drive(&mut begin(handle)).await.expect("begin");
    let tx_id = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["tx_id"]
        .as_u64()
        .unwrap();
    drive(&mut tx_exec(
        handle,
        tx_id,
        RString::from("insert into oj_plugin_t (id, v) values (?, ?)"),
        RString::from(r#"[2,"tx"]"#),
    ))
    .await
    .expect("tx insert");
    drive(&mut tx_commit(handle, tx_id))
        .await
        .expect("tx commit");

    // 事务 rollback
    let bytes = drive(&mut begin(handle)).await.expect("begin2");
    let tx_id = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["tx_id"]
        .as_u64()
        .unwrap();
    drive(&mut tx_exec(
        handle,
        tx_id,
        RString::from("insert into oj_plugin_t (id, v) values (?, ?)"),
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

/// T11 结构化错误信封（env-gated，`OJ_TEST_MYSQL=mysql://…`）：
/// - 重复主键插入 → `category=="constraint"` + `data.vendor_code==1062`（mysql 错误码原样透传）。
/// - 连向关闭端口 → `category=="connection"` + `session_disposition=="close"`（Io 类）。
/// 未设 env 直接跳过（不进网络）。
#[tokio::test(flavor = "multi_thread")]
async fn real_mysql_envelope_classifies_constraint_and_connection() {
    let Ok(url) = std::env::var("OJ_TEST_MYSQL") else {
        eprintln!("skip: OJ_TEST_MYSQL unset");
        return;
    };
    let cfg = serde_json::json!({}).to_string();
    let _ = init(host(), RString::from(cfg.as_str()));
    let mut c = connect(RString::from(url.as_str()));
    let bytes = drive(&mut c).await.expect("connect");
    let handle: u64 = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["handle"]
        .as_u64()
        .unwrap();

    drive(&mut exec(
        handle,
        RString::from("drop table if exists oj_env_t"),
        RString::from("[]"),
    ))
    .await
    .expect("drop");
    drive(&mut exec(
        handle,
        RString::from("create table oj_env_t (id int primary key, u int unique, v text)"),
        RString::from("[]"),
    ))
    .await
    .expect("create");
    drive(&mut exec(
        handle,
        RString::from("insert into oj_env_t (id, u, v) values (?, ?, ?)"),
        RString::from(r#"[1,10,"a"]"#),
    ))
    .await
    .expect("insert1");

    // 重复唯一键 → constraint + 厂商码 1062。
    let e = drive(&mut exec(
        handle,
        RString::from("insert into oj_env_t (id, u, v) values (?, ?, ?)"),
        RString::from(r#"[2,10,"b"]"#),
    ))
    .await
    .unwrap_err();
    let env: serde_json::Value = serde_json::from_str(&e).expect("envelope must be json");
    assert_eq!(env["category"], "constraint", "{e}");
    assert_eq!(env["data"]["vendor_code"], 1062, "{e}");
    assert_eq!(env["session_disposition"], "keep", "{e}");
}

/// T11 错误分类纯单测（不触网，确定性强）：Io 类 → connection/close。约束类落在
/// `Database`（由上面的真库用例覆盖 1062 透传）。本例只钉死分类映射本身。
#[test]
fn sqlx_category_maps_io_to_connection_and_close() {
    let io = sqlx::Error::Io(std::io::Error::new(
        std::io::ErrorKind::ConnectionRefused,
        "x",
    ));
    let (cat, retry, disp) = sqlx_category(&io);
    assert_eq!((cat, retry, disp), ("connection", true, "close"));
}

/// 大整数参数（v0.1.22）：`toBigInt()` 的标记形态 → i64 → bigint 列，精确往返。
/// env-gated：`OJ_TEST_MYSQL=mysql://…`；未设 env 直接跳过（不进网络）。
#[tokio::test(flavor = "multi_thread")]
async fn real_mysql_bigint_marker_binds_i64() {
    let Ok(url) = std::env::var("OJ_TEST_MYSQL") else {
        eprintln!("skip: OJ_TEST_MYSQL unset");
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

    // 标记参数 → i64 → bigint 主键（雪花量级 + i64::MIN）
    ex(
        "insert into oj_bigint_t (id, note) values (?, ?)",
        r#"[{"$oj$i64":"4886674138783273204"},"marker"]"#,
    )
    .await
    .expect("marker param must bind as i64");
    ex(
        "insert into oj_bigint_t (id, note) values (?, ?)",
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

    // where 用标记参数比较 bigint 列
    let rows = qy(
        "select note from oj_bigint_t where id = ?",
        r#"[{"$oj$i64":"4886674138783273204"}]"#,
    )
    .await
    .expect("marker param must compare against bigint column");
    let v: serde_json::Value = serde_json::from_slice(&rows).unwrap();
    assert_eq!(v[0]["note"], serde_json::json!("marker"), "{v}");

    ex("drop table if exists oj_bigint_t", "[]").await.unwrap();
    close(handle);
}

/// MySQL affected-rows 语义契约（env-gated）：插件 typed 池经 sqlx 连接时协商
/// `CLIENT_FOUND_ROWS`（sqlx-mysql `MySqlStream::with_socket` 默认请求，再与服务端
/// 广告取交集），UPDATE 的 `rows_affected()` = **匹配行数**（found rows）——与
/// sqlite `changes()`、pg 命令标签同语义，而非裸 mysql 客户端默认的「实际变更行」。
/// 下游「按受影响行数判存在/归属」的写法因此在 oj 栈上三方言一致（同值重复 UPDATE
/// 仍得 1，不会把幂等重放误判成 0 行）；一旦池构造被改（自定义 ConnectOptions 丢
/// flag / sqlx 升级改默认），同值重复 UPDATE 会变 0，本用例即红。
#[tokio::test(flavor = "multi_thread")]
async fn real_mysql_update_affected_rows_counts_matched_not_changed() {
    let Ok(url) = std::env::var("OJ_TEST_MYSQL") else {
        eprintln!("skip: OJ_TEST_MYSQL unset");
        return;
    };
    let cfg = serde_json::json!({}).to_string();
    let _ = init(host(), RString::from(cfg.as_str()));
    let mut c = connect(RString::from(url.as_str()));
    let bytes = drive(&mut c).await.expect("connect");
    let handle: u64 = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["handle"]
        .as_u64()
        .unwrap();
    let affected = |sql: &str| {
        let mut f = exec(handle, RString::from(sql), RString::from("[]"));
        async move {
            let b = drive(&mut f).await.expect("exec");
            serde_json::from_slice::<serde_json::Value>(&b)
                .unwrap()
                .as_i64()
                .expect("exec returns affected-rows number")
        }
    };

    affected("drop table if exists oj_affrows_t").await;
    affected(
        "create table oj_affrows_t (id bigint primary key, is_read bigint not null default 0)",
    )
    .await;
    affected("insert into oj_affrows_t (id, is_read) values (1, 0)").await;

    // 首次 0→1：1 行。
    assert_eq!(
        affected("update oj_affrows_t set is_read = 1 where id = 1").await,
        1
    );
    // 同值重复 1→1：**匹配行 1**（FOUND_ROWS 语义；若落回「实际变更行」则 0）。
    assert_eq!(
        affected("update oj_affrows_t set is_read = 1 where id = 1").await,
        1,
        "CLIENT_FOUND_ROWS 未协商：同值 UPDATE 返回了变更行数而非匹配行数"
    );
    // 真实变更 1→0：1 行。
    assert_eq!(
        affected("update oj_affrows_t set is_read = 0 where id = 1").await,
        1
    );
    // WHERE 不匹配：0 行（两种语义下皆 0，防恒真式实现）。
    assert_eq!(
        affected("update oj_affrows_t set is_read = 1 where id = 999").await,
        0
    );

    affected("drop table if exists oj_affrows_t").await;
    close(handle);
}

/// u64 / `BIGINT UNSIGNED`（v0.1.24，env-gated）：typed 路径下**读侧不再回绕成负数**、
/// 写侧能精确绑 `u64::MAX`。这是债务②的验收基线——Any 路径（老实现）会把
/// `18446744073709551615` 读成 `-1`。
#[tokio::test(flavor = "multi_thread")]
async fn real_mysql_unsigned_bigint_roundtrips_as_u64() {
    let Ok(url) = std::env::var("OJ_TEST_MYSQL") else {
        eprintln!("skip: OJ_TEST_MYSQL unset");
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

    ex("drop table if exists oj_unsigned_t", "[]")
        .await
        .unwrap();
    ex(
        "create table oj_unsigned_t (id bigint unsigned primary key, note text)",
        "[]",
    )
    .await
    .expect("create");

    // ① u64::MAX 与 i64::MAX+1 都能精确写入（`$oj$u64` 标记 → 绑 u64）。
    for (id, note) in [
        ("18446744073709551615", "umax"),
        ("9223372036854775808", "i64max_plus1"),
    ] {
        ex(
            "insert into oj_unsigned_t (id, note) values (?, ?)",
            &format!(r#"[{{"$oj$u64":"{id}"}},"{note}"]"#),
        )
        .await
        .unwrap_or_else(|e| panic!("insert {id}: {e}"));
    }
    // ② 读回来是**无符号**值：不是负数、也不是字符串（宿主 op 出口才会把 >2^53 降成字符串）。
    let rows = qy("select id, note from oj_unsigned_t order by note", "[]")
        .await
        .expect("select");
    let v: serde_json::Value = serde_json::from_slice(&rows).unwrap();
    assert_eq!(v[1]["id"], serde_json::json!(u64::MAX), "{v}");
    assert_eq!(v[0]["id"], serde_json::json!(9223372036854775808u64), "{v}");
    assert!(
        v[1]["id"].as_i64().is_none(),
        "u64::MAX 绝不能以 i64 形态出现（那意味着回绕成 -1）"
    );

    // ③ i64 范围内的无符号值也精确（走 u64 探测分支）。
    ex(
        "insert into oj_unsigned_t (id, note) values (?, ?)",
        r#"[{"$oj$u64":"42"},"small"]"#,
    )
    .await
    .expect("insert small unsigned");
    let rows = qy(
        "select id from oj_unsigned_t where note = ?",
        r#"["small"]"#,
    )
    .await
    .expect("select small");
    let v: serde_json::Value = serde_json::from_slice(&rows).unwrap();
    assert_eq!(v[0]["id"], serde_json::json!(42), "{v}");

    // ④ 越 u64 的标记必须失败（前端 i64Marker 之后端防线）。
    let e = ex(
        "insert into oj_unsigned_t (id) values (?)",
        r#"[{"$oj$u64":"18446744073709551616"}]"#,
    )
    .await
    .expect_err("越 u64 必须失败");
    assert!(e.contains("db") || e.contains("value"), "{e}");

    ex("drop table if exists oj_unsigned_t", "[]")
        .await
        .unwrap();
    close(handle);
}

/// 列类型能力边界（v0.1.24，env-gated；修开发侧评审 P0）：**不会解码的列类型必须响亮报错，
/// 不能静默 null**。
///
/// 背景：`sqlx::Any` 时代这类列在列转换就抛 `AnyDriverError`（`sqlx-mysql/src/any.rs` 的
/// `TryFrom<&MySqlTypeInfo>` 只认 Null/Short/Long/LongLong/Float/Double + str/bytes
/// 可兼容者）。typed 化后若沿用 `unwrap_or(Null)`，就把「报错」退化成「错值」——踩本仓
/// 「静默错值不可接受」的红线。
///
/// v0.1.3（L6 / U-6）分家：`DECIMAL`/`DATETIME`/`JSON` 等已能解码（见
/// `real_mysql_dialect_types_decode_readable`），本用例只钉**仍未覆盖**的
/// `GEOMETRY` 家族必须响亮报错并给出 `ST_AsText` 提示。
///
/// 同时钉住**可读**的边界：`TEXT` 家族（协议里是 `Blob` + 非 BINARY collation）与
/// `BOOLEAN`（= `TINYINT(1)`，按整数读成 1/0，不是 true/false）。
#[tokio::test(flavor = "multi_thread")]
async fn real_mysql_unsupported_column_types_error_loudly() {
    let Ok(url) = std::env::var("OJ_TEST_MYSQL") else {
        eprintln!("skip: OJ_TEST_MYSQL unset");
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

    ex("drop table if exists oj_types_t", "[]").await.unwrap();
    ex(
        "create table oj_types_t (id int primary key, amount decimal(20,4), \
             made_at datetime, payload json, note text, flag boolean, ub bigint unsigned, \
             shape geometry)",
        "[]",
    )
    .await
    .expect("create");
    ex(
        "insert into oj_types_t (id, amount, made_at, payload, note, flag, ub, shape) values \
             (1, 12.3456, '2026-09-19 10:00:00', '{\"a\":1}', 'hi', true, \
             18446744073709551615, ST_GeomFromText('POINT(1 2)'))",
        "[]",
    )
    .await
    .expect("insert");

    // ① 支持的列可读：整数 / TEXT / BIGINT UNSIGNED（u64）。
    let rows = qy(
        "select id, note, ub, cast(amount as char) as amount_txt from oj_types_t",
        "[]",
    )
    .await
    .expect("supported columns must read");
    let v: serde_json::Value = serde_json::from_slice(&rows).unwrap();
    assert_eq!(v[0]["id"], serde_json::json!(1), "{v}");
    assert_eq!(v[0]["note"], serde_json::json!("hi"), "{v}");
    assert_eq!(v[0]["ub"], serde_json::json!(u64::MAX), "{v}");
    assert_eq!(v[0]["amount_txt"], serde_json::json!("12.3456"), "{v}");

    // ② BOOLEAN 走整数读（1/0）——记录能力边界，不是 true/false。
    let rows = qy("select flag from oj_types_t", "[]")
        .await
        .expect("boolean reads as integer");
    let v: serde_json::Value = serde_json::from_slice(&rows).unwrap();
    assert_eq!(v[0]["flag"], serde_json::json!(1), "BOOLEAN 边界：{v}");

    // ③ 仍未覆盖的类型必须**报错**（点名列 + 类型 + 可执行提示），绝不静默 null。
    let e = qy("select shape from oj_types_t", "[]")
        .await
        .err()
        .unwrap_or_else(|| panic!("select shape 必须报错而不是返回静默 null"));
    assert!(
        e.contains("GEOMETRY"),
        "错误信息应点名 MySQL 类型 GEOMETRY，实得 {e}"
    );
    assert!(
        e.contains("ST_AsText(shape)"),
        "GEOMETRY 应给出可执行的 SQL 提示，实得 {e}"
    );
    assert!(!e.contains('\n'), "错误串不得含裸换行（日志注入面）：{e}");

    ex("drop table if exists oj_types_t", "[]").await.unwrap();
    close(handle);
}

/// 方言类型解码（v0.1.3 / L6 / PRD U-6，env-gated 真库）：`DECIMAL` / 日期时间 /
/// `YEAR` / `JSON` / `BIT` 的可读性，且**`query` 与 `stream` 两条路径同口径**。
///
/// 输出口径（`docs/devkit/db-guide.md` §10.6）：十进制走**字符串**（不经 f64，保精度）、
/// 日期时间走 ISO 8601（`DATETIME` 无时区，`TIMESTAMP` 带 `Z`）、`JSON` 走 **JSON 文本**、
/// `BIT(n)` 走**位串文本**、`YEAR` 走数字。
///
/// 反回归重点（评审 S1）：`BIT(5)` 在 v0.1.92 及以前会被 `bool::compatible` /
/// `u64::compatible` 吃掉，**静默返回 true 或数字**——此处断言位串，钉死该错值路径。
#[tokio::test(flavor = "multi_thread")]
async fn real_mysql_dialect_types_decode_readable() {
    let Ok(url) = std::env::var("OJ_TEST_MYSQL") else {
        eprintln!("skip: OJ_TEST_MYSQL unset");
        return;
    };
    let cfg = serde_json::json!({}).to_string();
    let _ = init(host(), RString::from(cfg.as_str()));
    let bytes = drive(&mut connect(RString::from(url.as_str())))
        .await
        .expect("connect");
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

    ex("drop table if exists oj_dialect_t", "[]").await.unwrap();
    ex(
        "create table oj_dialect_t (\
               id int primary key, \
               amount decimal(18,4), \
               made_on date, \
               made_at time(6), \
               made_dt datetime(6), \
               stamped timestamp null, \
               yr year, \
               payload json, \
               flags bit(5), \
               nullable_dec decimal(10,2) null)",
        "[]",
    )
    .await
    .expect("create");
    ex(
        "insert into oj_dialect_t values \
             (1, 1234.5678, '2026-09-19', '10:11:12.345678', '2026-09-19 10:11:12.345678', \
              '2026-09-19 10:11:12', 2026, '{\"a\":1,\"b\":[2,3]}', b'10101', null)",
        "[]",
    )
    .await
    .expect("insert");

    let rows = qy(
        "select id, amount, made_on, made_at, made_dt, stamped, yr, payload, flags, nullable_dec \
             from oj_dialect_t",
        "[]",
    )
    .await
    .expect("dialect columns must read (query path)");
    let v: serde_json::Value = serde_json::from_slice(&rows).unwrap();
    let r = &v[0];
    // DECIMAL 保精度：`1234.5678` 原样，不经 f64 往返。
    assert_eq!(r["amount"], serde_json::json!("1234.5678"), "{v}");
    assert_eq!(r["made_on"], serde_json::json!("2026-09-19"), "{v}");
    assert_eq!(r["made_at"], serde_json::json!("10:11:12.345678"), "{v}");
    assert_eq!(
        r["made_dt"],
        serde_json::json!("2026-09-19T10:11:12.345678"),
        "{v}"
    );
    // TIMESTAMP 是 UTC 语义 → 带 `Z`。
    assert_eq!(
        r["stamped"],
        serde_json::json!("2026-09-19T10:11:12Z"),
        "{v}"
    );
    assert_eq!(r["yr"], serde_json::json!(2026), "{v}");
    // JSON 走 JSON 文本（PRD U-6 口径：不做自动 JSON.parse，由业务侧决定）。
    let payload = r["payload"].as_str().expect("JSON 应为字符串");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(payload).unwrap(),
        serde_json::json!({"a": 1, "b": [2, 3]}),
        "{v}"
    );
    // BIT(5) = 21 = 0b10101 —— 必须是位串，不能是 true 或 21。
    assert_eq!(r["flags"], serde_json::json!("10101"), "{v}");
    // NULL 保持 null。
    assert!(r["nullable_dec"].is_null(), "{v}");

    // stream 路径同口径（同一 `row_to_json_mysql`，此处钉住未分叉）。
    let bytes = drive(&mut stream_open(
        handle,
        RString::from("select amount, flags, yr from oj_dialect_t"),
        RString::from("[]"),
    ))
    .await
    .expect("stream_open");
    let sid = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["stream_id"]
        .as_u64()
        .unwrap();
    let bytes = drive(&mut stream_next(handle, sid)).await.expect("next");
    let sv: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let srow = &sv["rows"][0];
    assert_eq!(srow["amount"], serde_json::json!("1234.5678"), "{sv}");
    assert_eq!(srow["flags"], serde_json::json!("10101"), "{sv}");
    assert_eq!(srow["yr"], serde_json::json!(2026), "{sv}");
    drive(&mut stream_close(handle, sid)).await.expect("close");

    ex("drop table if exists oj_dialect_t", "[]").await.unwrap();
    close(handle);
}

/// REQ-07b（env-gated，`OJ_TEST_MYSQL`，breaking）：BLOB/BINARY/VARBINARY 族
/// （Vec<u8> 探针臂）恒定 cellenv 信封——**utf8 合法内容也信封**（判别依据：sqlx 对
/// binary charset 拒 String 解码，TEXT 永不落入该臂）；TEXT 列保持可读裸字符串。
#[tokio::test(flavor = "multi_thread")]
async fn real_mysql_binary_columns_cellenv() {
    let Ok(url) = std::env::var("OJ_TEST_MYSQL") else {
        eprintln!("skip: OJ_TEST_MYSQL unset");
        return;
    };
    let cfg = serde_json::json!({}).to_string();
    let _ = init(host(), RString::from(cfg.as_str()));
    let bytes = drive(&mut connect(RString::from(url.as_str())))
        .await
        .expect("connect");
    let handle: u64 = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["handle"]
        .as_u64()
        .unwrap();
    let ex = |sql: &str| {
        let mut f = exec(handle, RString::from(sql), RString::from("[]"));
        async move { drive(&mut f).await }
    };
    ex("drop table if exists oj_req07b_t").await.unwrap();
    ex("create table oj_req07b_t (b blob, vb varbinary(8), bn binary(4), t text)")
        .await
        .expect("create");
    ex("insert into oj_req07b_t values ('hello', 'wörld', 'ab', 'plain')")
        .await
        .expect("insert");
    let mut f = query(
        handle,
        RString::from("select b, vb, bn, t from oj_req07b_t"),
        RString::from("[]"),
    );
    let rows = drive(&mut f).await.expect("binary columns must read");
    let v: serde_json::Value = serde_json::from_slice(&rows).unwrap();
    let r = &v[0];
    for (col, ty) in [("b", "blob"), ("vb", "varbinary"), ("bn", "binary")] {
        let s = r[col]
            .as_str()
            .unwrap_or_else(|| panic!("{col} 应为字符串标量: {v}"));
        let (t, payload) = oj_plugin_ffi::cellenv::decode(s)
            .unwrap_or_else(|| panic!("{col} 应为 cellenv 信封: {s:?}"));
        assert_eq!(t, ty, "{col} 类型段不符");
        assert!(!payload.is_empty(), "{col} 空信封=红线");
    }
    assert_ne!(
        r["b"],
        serde_json::json!("hello"),
        "合法 UTF-8 二进制不再裸串（REQ-07b breaking）"
    );
    assert_ne!(r["vb"], serde_json::json!("wörld"), "{v}");
    // TEXT 行为零变化。
    assert_eq!(r["t"], serde_json::json!("plain"), "{v}");
    ex("drop table if exists oj_req07b_t").await.unwrap();
    close(handle);
}

/// `safe_ident` 的消毒/截断（离线，无需真库）：错误串是日志注入面。
#[test]
fn given_hostile_column_name_when_safe_ident_then_control_chars_stripped() {
    assert_eq!(super::safe_ident("col"), "col");
    assert_eq!(super::safe_ident("a\nb\tc"), "abc");
    assert_eq!(super::safe_ident("x\u{0}y"), "xy");
    let long: String = "a".repeat(100);
    let out = super::safe_ident(&long);
    assert_eq!(out.chars().count(), 65, "64 字符 + 省略号");
    assert!(out.ends_with('…'));
    // 未截断时不得画蛇添足加省略号（含多字节字符的列名也不能误判）。
    assert_eq!(super::safe_ident("列名"), "列名");
}

/// 并发驱动多个 FfiFuture（FfiFuture 非 Send）：在单个任务里轮询全部——调度已在插件
/// runtime 上并行开始，故这就是真并发。（与 pg 插件同款；两插件按 spec 允许复制。）
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

/// 债④回归（env-gated，MySQL 侧）：`db.nextSeq` 的两条语句在真库上正确且原子。
///
/// MySQL 没有 `RETURNING`，平台用 `insert … values (?, last_insert_id(1)) on duplicate key
/// update v = last_insert_id(v + 1)` + `select last_insert_id()`（**必须同一连接**，宿主在
/// 短事务里跑）。这里分两段验证：
/// ① 顺序 3 次（同一连接）→ 1、2、3（验证 `last_insert_id` 惯用法的**首次插入/已存在**两态）；
/// ② 12 个并发语句 → 最终 v 恰为 12（**原子性**：无丢失更新；`select max(id)+1` 会丢）。
#[tokio::test(flavor = "multi_thread")]
async fn real_mysql_next_seq_is_atomic_under_concurrency() {
    let Ok(url) = std::env::var("OJ_TEST_MYSQL") else {
        eprintln!("skip: OJ_TEST_MYSQL unset");
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

    ex("drop table if exists _oj_sequences", "[]")
        .await
        .unwrap();
    ex(
        "create table _oj_sequences (name varchar(128) primary key, v bigint not null)",
        "[]",
    )
    .await
    .expect("create seq table");

    let bump = "insert into _oj_sequences (name, v) values (?, last_insert_id(1)) \
                    on duplicate key update v = last_insert_id(v + 1)";
    let read = "select last_insert_id() as v";

    // ① 顺序 3 次（同一连接 = 同一个 tx）：1 → 2 → 3。
    for expect in 1..=3 {
        let b = drive(&mut begin(handle)).await.expect("begin");
        let tx = serde_json::from_slice::<serde_json::Value>(&b).unwrap()["tx_id"]
            .as_u64()
            .unwrap();
        drive(&mut tx_exec(
            handle,
            tx,
            RString::from(bump),
            RString::from(r#"["seq_a"]"#),
        ))
        .await
        .expect("bump in tx");
        let rows = drive(&mut tx_query(
            handle,
            tx,
            RString::from(read),
            RString::from("[]"),
        ))
        .await
        .expect("read last_insert_id");
        drive(&mut tx_commit(handle, tx)).await.expect("commit");
        let v: serde_json::Value = serde_json::from_slice(&rows).unwrap();
        assert_eq!(
            v[0]["v"],
            serde_json::json!(expect),
            "顺序取号应为 {expect}：{v}"
        );
    }

    // ② 12 个并发语句（各自独立连接/语句）→ 最终值恰为 12。
    ex(
        "insert into _oj_sequences (name, v) values (?, 0)",
        r#"["seq_b"]"#,
    )
    .await
    .expect("seed seq_b");
    let mut futs: Vec<FfiFuture> = (0..12)
        .map(|_| exec(handle, RString::from(bump), RString::from(r#"["seq_b"]"#)))
        .collect();
    for o in drive_all(&mut futs).await {
        o.expect("并发 bump 应成功");
    }
    let rows = drive(&mut query(
        handle,
        RString::from("select v from _oj_sequences where name = ?"),
        RString::from(r#"["seq_b"]"#),
    ))
    .await
    .expect("read final");
    let v: serde_json::Value = serde_json::from_slice(&rows).unwrap();
    assert_eq!(
        v[0]["v"],
        serde_json::json!(12),
        "12 次并发 self-increment 不得丢更新：{v}"
    );

    ex("drop table if exists _oj_sequences", "[]")
        .await
        .unwrap();
    close(handle);
}

/// 真取消（env-gated，MySQL 侧，债②验收）：`mysql://` DSN 走 typed 专用连接，
/// `stream_cancel` = `KILL QUERY` + 断连，服务端 `information_schema.processlist` 中的
/// 慢查询秒级消失（≤5s），随后池 query 正常。
#[tokio::test(flavor = "multi_thread")]
async fn real_mysql_stream_cancel_kills_server_query() {
    let Ok(dsn) = std::env::var("OJ_TEST_MYSQL") else {
        eprintln!("skip: OJ_TEST_MYSQL unset");
        return;
    };
    let _ = std::result::Result::from(init(host(), RString::from("{}")));
    let bytes = drive(&mut connect(RString::from(dsn.as_str())))
        .await
        .expect("connect");
    let h = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["handle"]
        .as_u64()
        .unwrap();
    // 第二个 client 轮询 processlist（独立连接，不被慢查询阻塞）。
    let pbytes = drive(&mut connect(RString::from(dsn.as_str())))
        .await
        .expect("poll connect");
    let ph = serde_json::from_slice::<serde_json::Value>(&pbytes).unwrap()["handle"]
        .as_u64()
        .unwrap();

    let slow = "with nums as (select 0 n union all select 1 union all select 2 union all select 3 \
                     union all select 4 union all select 5 union all select 6 union all select 7 \
                     union all select 8 union all select 9) \
                    /*oj-cancel-test*/ select a.n + b.n*10 + c.n*100 as v, sleep(0.02) \
                     from nums a, nums b, nums c";
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

    let poll = |ph: u64| {
        async move {
            // 排除轮询自身连接（其 info 含同一 marker，会自匹配导致计数恒 ≥1）。
            let sql = "select count(*) as n from information_schema.processlist \
                           where info like '%oj-cancel-test%' and id <> connection_id()";
            let mut fut = query(ph, RString::from(sql), RString::from("[]"));
            drive(&mut fut).await
        }
    };
    // cancel 前：服务端有该查询
    let pre = poll(ph).await.expect("poll pre");
    let pre_n = serde_json::from_slice::<serde_json::Value>(&pre).unwrap()[0]["n"]
        .as_i64()
        .unwrap();
    assert!(pre_n >= 1, "cancel 前服务端应有慢查询在跑: {pre_n}");

    // cancel → KILL QUERY → 服务端查询终止
    drive(&mut stream_cancel(h, sid)).await.expect("cancel");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut cleared = false;
    while std::time::Instant::now() < deadline {
        let b = poll(ph).await.expect("poll");
        if serde_json::from_slice::<serde_json::Value>(&b).unwrap()[0]["n"].as_i64() == Some(0) {
            cleared = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    assert!(cleared, "5s 内 processlist 必须无孤儿查询（真取消）");
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

extern "C" fn test_log(_level: u8, _msg: RString) {}
extern "C" fn test_deliver(_topic: RString, _payload: RBytes) {}

fn host() -> RArc<HostContext> {
    RArc::new(HostContext {
        log: test_log,
        deliver: test_deliver,
    })
}

/// FfiFuture → 测试异步桥（等价 core await_ffi 的 poll 轮询）。
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
