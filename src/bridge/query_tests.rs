//! `query` 的测试模块：经 `#[path]` 挂在 `query::tests`，断言全走 Bridge JS 面。

use super::*;

/// 执行路由矩阵（F1 回归钉子）：**Select 在 mysql 上绝不能进 LastId 两步
/// 分支**——returning 为空会在 `returning[0]` 越界 panic（v0.1.65 实锤，
/// mysql 全业务面请求挂死）。
#[test]
fn exec_route_matrix() {
    // select：任何方言都走行集（对抗用例：mysql + select 曾误路由 LastId）。
    for d in [Dialect::Sqlite, Dialect::MySql, Dialect::Postgres] {
        assert_eq!(exec_route(Verb::Select, true, d), ExecRoute::Rows);
        assert_eq!(exec_route(Verb::Select, false, d), ExecRoute::Rows);
    }
    // insert + returning：pg/sqlite 走 RETURNING 行集；mysql 走两步 LastId。
    assert_eq!(
        exec_route(Verb::Insert, false, Dialect::MySql),
        ExecRoute::LastId
    );
    assert_eq!(
        exec_route(Verb::Insert, false, Dialect::Postgres),
        ExecRoute::Rows
    );
    assert_eq!(
        exec_route(Verb::Insert, false, Dialect::Sqlite),
        ExecRoute::Rows
    );
    // insert 无 returning：受影响行数（mysql 也不例外——LAST_INSERT_ID 无处安放）。
    assert_eq!(
        exec_route(Verb::Insert, true, Dialect::MySql),
        ExecRoute::Affected
    );
    // update / delete：一律受影响行数。
    for v in [Verb::Update, Verb::Delete] {
        assert_eq!(exec_route(v, true, Dialect::MySql), ExecRoute::Affected);
        assert_eq!(exec_route(v, false, Dialect::Postgres), ExecRoute::Affected);
    }
}

/// 相同条件三种方言的占位符风格：sqlite/mysql 用 `?`，postgres 用 `$1`（builder 职责）。
#[test]
fn placeholder_per_dialect() {
    let sql_of = |d: Dialect| {
        let mut q = sea_query::Query::select();
        q.column(Alias::new("name")).from(Alias::new("user"));
        q.and_where(Expr::col(Alias::new("id")).eq(1));
        build_sql(d, &q).0
    };
    assert!(sql_of(Dialect::Sqlite).contains('?'));
    assert!(sql_of(Dialect::MySql).contains('?'));
    assert!(sql_of(Dialect::Postgres).contains("$1"));
}

/// QueryReq 缺省 db=default（bootstrap 旧调用兼容）。
#[test]
fn query_req_defaults_to_default_db() {
    let req: QueryReq = serde_json::from_str(r#"{"table":"user"}"#).unwrap();
    assert_eq!(req.db, "default");
    let req: QueryReq = serde_json::from_str(r#"{"db":"other","table":"user"}"#).unwrap();
    assert_eq!(req.db, "other");
}

use crate::bridge::{Bridge, InMemoryKV, SchemaRegistry, SqlxAccessor};
use serde_json::{Value, json};
use std::sync::Arc;

/// 真实 sqlite 库（内存）+ 4 行种子，便于校验构造器生成的 WHERE/ORDER/LIMIT/OFFSET
/// 真正参与执行（InMemoryAccessor 忽略 SQL 不做过滤，无法验证语义）。
async fn seeded_bridge() -> Bridge {
    let db = SqlxAccessor::arc("sqlite::memory:").await.unwrap();
    db.exec_with_params(
        "create table t (id integer primary key, name text, age integer, tag text, ok integer)",
        &[],
    )
    .await
    .unwrap();
    for (n, a, tg, ok) in [
        ("a", 10, Some("x"), 1),
        ("b", 20, Some("y"), 0),
        ("c", 30, Some("x"), 1),
        ("d", 40, None::<&str>, 0),
    ] {
        db.exec_with_params(
            "insert into t (name, age, tag, ok) values (?, ?, ?, ?)",
            &[json!(n), json!(a), json!(tg), json!(ok)],
        )
        .await
        .unwrap();
    }
    let reg = SchemaRegistry::new().table("t", &["id"], &["id", "name", "age", "tag", "ok"]);
    Bridge::with_opts(db, Arc::new(InMemoryKV::new()), reg, false)
}

/// 跑一段返回 rows 长度的查询构造器脚本。
async fn count_where(b: &Bridge, cond: &str) -> usize {
    let cap = b
        .run(&format!(
            r#"db.table("t").select(["name"]).where({cond}).all().then(r => json.ok({{ n: r.length }})).catch(e => json.fail(500, String(e)));"#
        ))
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "query failed: {v}");
    v["data"]["n"].as_u64().unwrap() as usize
}

/// `value_to_json` 的 u64 不再回绕（v0.1.22）：`> i64::MAX` 直出 u64，
/// 避免旧实现 `as i64` 静默变负数；超界部分由 op 出口护栏降为十进制字符串。
#[test]
fn value_to_json_keeps_u64_and_bigint_exact() {
    assert_eq!(
        value_to_json(&Qv::BigInt(Some(i64::MIN))).unwrap(),
        json!({ oj_plugin_ffi::jsint::I64_MARKER: i64::MIN.to_string() })
    );
    assert_eq!(
        value_to_json(&Qv::BigUnsigned(Some(u64::MAX))).unwrap(),
        json!({ oj_plugin_ffi::jsint::U64_MARKER: u64::MAX.to_string() })
    );
    // 安全范围内仍是 number（回放与可读性不变）。
    assert_eq!(value_to_json(&Qv::BigUnsigned(Some(0))).unwrap(), json!(0));
    assert_eq!(value_to_json(&Qv::BigInt(Some(42))).unwrap(), json!(42));
    // >2^53 但 ≤i64::MAX：用 i64 marker（u64 的那一段才需要 $oj$u64）。
    assert_eq!(
        value_to_json(&Qv::BigUnsigned(Some(9223372036854775807))).unwrap(),
        json!({ oj_plugin_ffi::jsint::I64_MARKER: "9223372036854775807" })
    );
    // **可重放**（v0.1.24 修的既有漏洞）：出口护栏不再把参数降成字符串，
    // 于是 `db.query(toSQL().sql, ...toSQL().params)` 能原样回跑。
    let mut params = vec![
        value_to_json(&Qv::BigInt(Some(4886674138783273204))).unwrap(),
        value_to_json(&Qv::BigUnsigned(Some(u64::MAX))).unwrap(),
    ];
    super::super::jsnum::sanitize_rows(&mut params);
    assert_eq!(
        params[0],
        json!({ oj_plugin_ffi::jsint::I64_MARKER: "4886674138783273204" }),
        "i64 大整数参数必须可重放（不得被降成字符串）"
    );
    assert_eq!(
        params[1],
        json!({ oj_plugin_ffi::jsint::U64_MARKER: "18446744073709551615" })
    );
    // 而 DB **读出来的**大整数仍按老口径降为十进制字符串（读侧契约不变）。
    let mut row = json!({ "id": 4886674138783273204i64 });
    super::super::jsnum::sanitize_js_numbers(&mut row);
    assert_eq!(row["id"], json!("4886674138783273204"));
}

// ----- LIMIT 配置（db_query 段，v0.1.20）-----

/// 10 行夹具 + `db_query: {default_limit: 3, max_limit: 5}`。
async fn limited_bridge() -> Bridge {
    let db = SqlxAccessor::arc("sqlite::memory:").await.unwrap();
    db.exec_with_params("create table t (id integer primary key, name text)", &[])
        .await
        .unwrap();
    for i in 0..10 {
        db.exec_with_params("insert into t (name) values (?)", &[json!(format!("n{i}"))])
            .await
            .unwrap();
    }
    let reg = SchemaRegistry::new().table("t", &["id"], &["id", "name"]);
    Bridge::with_dbs_and_loader(
        std::collections::HashMap::from([("default".to_string(), db as _)]),
        Arc::new(InMemoryKV::new()),
        reg,
        false,
        None,
        Extras {
            query_limits: QueryLimits {
                default_limit: 3,
                max_limit: 5,
            },
            ..Default::default()
        },
    )
}

/// 隐式 default_limit 生效 + 截断写 `X-OJ-Row-Limit` 响应头（不再静默少数据）。
#[tokio::test(flavor = "current_thread")]
async fn query_limit_defaults_and_truncation_header() {
    let b = limited_bridge().await;
    // 未给 limit → default_limit=3；返回行数 == 上限 ⇒ 头文件（可能是被截断的）
    let cap = b
        .run(
            r#"db.table("t").select(["name"]).all()
                 .then(r => json.ok({ n: r.length })).catch(e => json.fail(500, String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!((&v["code"], &v["data"]["n"]), (&json!(0), &json!(3)), "{v}");
    assert_eq!(cap.header("X-OJ-Row-Limit"), Some("3"));
    // 显式 limit 被 clamp 到 max_limit=5 也写头（旧版这类截断完全静默）
    let cap = b
        .run(
            r#"db.table("t").select(["name"]).limit(1000).all()
                 .then(r => json.ok({ n: r.length })).catch(e => json.fail(500, String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["n"], json!(5), "{v}");
    assert_eq!(cap.header("X-OJ-Row-Limit"), Some("5"));
    // 行数 < 生效上限 ⇒ 不可能被截断，不写头（避免噪音）
    let cap = b
        .run(
            r#"db.table("t").select(["name"]).where({field:"name",op:"eq",value:"n0"}).limit(5).all()
                 .then(r => json.ok({ n: r.length })).catch(e => json.fail(500, String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["n"], json!(1), "{v}");
    assert!(cap.header("X-OJ-Row-Limit").is_none(), "{cap:?}");
    // toSQL 与执行同款归一化（诊断口径一致）
    // LIMIT/OFFSET 是绑定参数（sea-query 渲染为 `LIMIT ?`），故断言参数而非文本。
    let cap = b
        .run(r#"json.ok(db.table("t").select(["name"]).toSQL());"#)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert!(v["data"]["sql"].as_str().unwrap().contains("LIMIT"), "{v}");
    assert!(
        v["data"]["params"].as_array().unwrap().contains(&json!(3)),
        "{v}"
    );
}

/// 拼错的键必须报错（否则 `db_query: {default: 1000}` 会静默取默认值，用户以为生效了）。
#[test]
fn query_limits_reject_unknown_keys() {
    let ok: QueryLimits = serde_json::from_str(r#"{"default_limit": 5, "max_limit": 9}"#).unwrap();
    assert_eq!((ok.default_limit, ok.max_limit), (5, 9));
    // 缺字段 → 走 Default（`#[serde(default)]`），不是错误。
    let partial: QueryLimits = serde_json::from_str("{}").unwrap();
    assert_eq!(partial, QueryLimits::default());
    // 未知键 → 错误。
    assert!(serde_json::from_str::<QueryLimits>(r#"{"default": 5}"#).is_err());
}

/// 装配期校验：0 / 倒置 / 超硬顶 一律 fail-fast（不给静默回落）。
#[test]
fn query_limits_validate() {
    assert!(QueryLimits::default().validate().is_ok());
    assert!(
        QueryLimits {
            default_limit: 0,
            max_limit: 100
        }
        .validate()
        .is_err()
    );
    assert!(
        QueryLimits {
            default_limit: 10,
            max_limit: 0
        }
        .validate()
        .is_err()
    );
    assert!(
        QueryLimits {
            default_limit: 10,
            max_limit: LIMIT_HARD_CAP + 1
        }
        .validate()
        .is_err()
    );
    assert!(
        QueryLimits {
            default_limit: 10,
            max_limit: 5
        }
        .validate()
        .is_err()
    );
}

/// 两表夹具：a(2 行) × b(3 行，aid 指向 a.id)，验证 join 装配真实参与执行。
async fn seeded_bridge_2t() -> Bridge {
    let db = SqlxAccessor::arc("sqlite::memory:").await.unwrap();
    db.exec_with_params("create table a (id integer primary key, name text)", &[])
        .await
        .unwrap();
    db.exec_with_params(
        "create table b (id integer primary key, aid integer, label text)",
        &[],
    )
    .await
    .unwrap();
    for (n,) in [("x",), ("y",)] {
        db.exec_with_params("insert into a (name) values (?)", &[json!(n)])
            .await
            .unwrap();
    }
    for (aid, l) in [(1, "L1"), (1, "L2"), (3, "L3")] {
        db.exec_with_params(
            "insert into b (aid, label) values (?, ?)",
            &[json!(aid), json!(l)],
        )
        .await
        .unwrap();
    }
    let reg = SchemaRegistry::new()
        .table("a", &["id"], &["id", "name"])
        .table("b", &["id"], &["id", "aid", "label"]);
    Bridge::with_opts(db, Arc::new(InMemoryKV::new()), reg, false)
}

#[tokio::test(flavor = "current_thread")]
async fn join_inner_left_and_rejections() {
    let b = seeded_bridge_2t().await;
    // inner join：a(id=1 x, 2 y) × b(aid=1 ×2) → 2 行
    let cap = b
        .run(
            r#"db.table("a").join("b", [{left:"a.id",right:"b.aid"}])
    .select(["a.name","b.label"]).all()
    .then(r=>json.ok({n:r.length, first:r[0].label})).catch(e=>json.fail(400,String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert_eq!(v["data"]["n"], 2, "{v}");
    // left join：3 行（x×2, y×1 NULL label）
    let cap = b
        .run(
            r#"db.table("a").join("b", [{left:"a.id",right:"b.aid"}], "left")
    .select(["a.name","b.label"]).orderBy([{field:"a.id",dir:"asc"}]).all()
    .then(r=>json.ok({n:r.length})).catch(e=>json.fail(400,String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["n"], 3, "{v}");
    // join 表未知 / 自 join / on 列未知 / 非限定列命中 join 表
    for (js, want) in [
        (
            r#"db.table("a").join("nope",[{left:"a.id",right:"nope.aid"}]).select(["a.id"]).all()"#,
            "unknown table 'nope'",
        ),
        (
            r#"db.table("a").join("a",[{left:"a.id",right:"a.id"}]).select(["a.id"]).all()"#,
            "self join needs distinct table aliases",
        ),
        (
            // 不同真名但别名撞车：SQL 侧两个都是 "x"
            r#"db.table("a").as("x").join("b",[{left:"x.id",right:"x.aid"}],"inner","x").select(["x.id"]).all()"#,
            "duplicate table alias 'x'",
        ),
        (
            r#"db.table("a").join("b",[{left:"a.id",right:"b.nope"}]).select(["a.id"]).all()"#,
            "unknown column 'b.nope'",
        ),
        (
            r#"db.table("a").join("b",[{left:"a.id",right:"b.aid"}]).select(["label"]).all()"#,
            "unknown column 'label'",
        ),
    ] {
        let cap = b
            .run(&format!(
                r#"{js}.then(()=>json.ok({{}})).catch(e=>json.fail(400,String(e)));"#
            ))
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(v["msg"].as_str().unwrap().contains(want), "{want}: {v}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn comparison_ops_filter_rows() {
    let b = seeded_bridge().await;
    assert_eq!(
        count_where(&b, r#"{field:"age",op:"gt",value:15}"#).await,
        3
    );
    assert_eq!(
        count_where(&b, r#"{field:"age",op:"gte",value:20}"#).await,
        3
    );
    assert_eq!(
        count_where(&b, r#"{field:"age",op:"lt",value:20}"#).await,
        1
    );
    assert_eq!(
        count_where(&b, r#"{field:"age",op:"lte",value:10}"#).await,
        1
    );
    assert_eq!(
        count_where(&b, r#"{field:"age",op:"ne",value:20}"#).await,
        3
    );
    assert_eq!(
        count_where(&b, r#"{field:"age",op:"eq",value:10}"#).await,
        1
    );
    assert_eq!(
        count_where(&b, r#"{field:"age",op:"in",value:[10,30]}"#).await,
        2
    );
    assert_eq!(
        count_where(&b, r#"{field:"name",op:"like",value:"a%"}"#).await,
        1
    );
    assert_eq!(count_where(&b, r#"{field:"tag",op:"isnull"}"#).await, 1);
    // float 值走 to_qv 的 f64 分支
    assert_eq!(
        count_where(&b, r#"{field:"age",op:"gte",value:15.5}"#).await,
        3
    );
    // 布尔值走 to_qv 的 bool 分支
    assert_eq!(
        count_where(&b, r#"{field:"ok",op:"eq",value:true}"#).await,
        2
    );
    // 对象值走 to_qv 的 other 分支（sqlite 接受其字符串化）
    assert!(count_where(&b, r#"{field:"name",op:"eq",value:{a:1}}"#).await <= 4);
}

#[tokio::test(flavor = "current_thread")]
async fn order_by_desc_with_offset() {
    let b = seeded_bridge().await;
    let cap = b
        .run(
            r#"db.table("t").select(["age"]).orderBy([{field:"age",dir:"desc"}]).limit(2).offset(1).all()
              .then(r => json.ok({ ages: r.map(x => x.age) }))
              .catch(e => json.fail(500, String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    // 降序 40,30,20,10 → offset 1 limit 2 → 30,20
    assert_eq!(v["data"]["ages"], json!([30, 20]));
}

#[tokio::test(flavor = "current_thread")]
async fn to_sql_returns_dialect_sql_and_params_without_executing() {
    let b = seeded_bridge().await;
    let cap = b
        .run(
            r#"const s = db.table("t").select(["name"]).where({field:"age",op:"gte",value:18}).toSQL();
               db.query(s.sql, s.params).then(rows => json.ok({ sql: s.sql, n: rows.length }))
                 .catch(e => json.fail(500, String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert!(v["data"]["sql"].as_str().unwrap().contains('?'), "{v}"); // sqlite placeholder
    assert_eq!(v["data"]["n"], 3); // age>=18 -> 3 rows (20,30,40)
}

#[tokio::test(flavor = "current_thread")]
async fn nested_condition_tree_filters_and_limits() {
    let b = seeded_bridge().await;
    // or：age>=20 (b,c,d) or name like 'a%' (a) → 4
    let n = count_where(
        &b,
        r#"{or:[{field:"age",op:"gte",value:20},{field:"name",op:"like",value:"a%"}]}"#,
    )
    .await;
    assert_eq!(n, 4);
    let n = count_where(&b, r#"{not:{field:"tag",op:"eq",value:"x"}}"#).await;
    assert_eq!(n, 1); // b (d has null tag, SQL NOT excludes unknown)
    // 深度 9 → too deep
    let cap = b
        .run(
            r#"let c={field:"age",op:"eq",value:1}; for(let i=0;i<9;i++) c={and:[c]};
        db.table("t").select(["name"]).where(c).all()
          .then(r=>json.ok({})).catch(e=>json.fail(400,String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert!(v["msg"].as_str().unwrap().contains("too deep"), "{v}");
    // 65 叶 → too large
    let cap = b
        .run(
            r#"const xs=[]; for(let i=0;i<65;i++) xs.push({field:"age",op:"gt",value:i});
        db.table("t").select(["name"]).where({and:xs}).all()
          .then(r=>json.ok({})).catch(e=>json.fail(400,String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert!(v["msg"].as_str().unwrap().contains("too large"), "{v}");
    // 空组（JS 直塞）→ empty condition group
    let cap = b
        .run(
            r#"db.table("t").select(["name"]).where({and:[]}).all()
        .then(r=>json.ok({})).catch(e=>json.fail(400,String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert!(
        v["msg"].as_str().unwrap().contains("empty condition group"),
        "{v}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn unknown_column_in_select_and_where_errors() {
    let b = seeded_bridge().await;
    let cap = b
        .run(r#"db.table("t").select(["nope"]).all().then(r => json.ok({})).catch(e => json.fail(400, String(e)));"#)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 400);
    assert!(
        v["msg"].as_str().unwrap().contains("unknown column 'nope'"),
        "{v}"
    );

    let cap = b
        .run(r#"db.table("t").select(["name"]).where({field:"nope",op:"eq",value:1}).all().then(r => json.ok({})).catch(e => json.fail(400, String(e)));"#)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 400);
    assert!(
        v["msg"]
            .as_str()
            .unwrap()
            .contains("unknown column 'nope' in where"),
        "{v}"
    );
}

#[test]
fn cond_tree_deserialize_dispatch_and_errors() {
    // 叶子向后兼容
    let t: CondTree = serde_json::from_str(r#"{"field":"a","op":"eq","value":1}"#).unwrap();
    assert!(matches!(t, CondTree::Leaf(_)));
    // and / or / not
    let t: CondTree = serde_json::from_str(
        r#"{"or":[{"field":"a","op":"eq","value":1},{"not":{"field":"b","op":"isnull"}}]}"#,
    )
    .unwrap();
    assert!(matches!(t, CondTree::Or(ref xs) if xs.len() == 2));
    // 空组 → Err
    let e = serde_json::from_str::<CondTree>(r#"{"and":[]}"#).unwrap_err();
    assert!(e.to_string().contains("empty condition group"), "{e}");
    // 多余键（leaf 形状 + or）→ Err，不静默吞
    let e = serde_json::from_str::<CondTree>(r#"{"field":"a","op":"eq","value":1,"or":[]}"#)
        .unwrap_err();
    assert!(e.to_string().contains("unknown keys"), "{e}");
    // 组键多于一个 → Err
    assert!(serde_json::from_str::<CondTree>(r#"{"and":[],"or":[]}"#).is_err());
    // 空对象 → Err
    assert!(serde_json::from_str::<CondTree>(r#"{}"#).is_err());
}

#[tokio::test(flavor = "current_thread")]
async fn cond_object_compose_inspect_and_equivalent_exec() {
    let b = seeded_bridge().await;
    let cap = b
        .run(
            r#"const base = db.and({field:"age",op:"gte",value:18}, {field:"ok",op:"eq",value:1});
               const c = base.or({field:"tag",op:"isnull"});
               const bare = {or:[{and:[{field:"age",op:"gte",value:18},{field:"ok",op:"eq",value:1}]},{field:"tag",op:"isnull"}]};
               Promise.all([
                 db.table("t").select(["name"]).where(c).all(),
                 db.table("t").select(["name"]).where(bare).all(),
               ]).then(([a, b2]) => json.ok({
                 eq: a.length === b2.length && a.length === 2,
                 fields: c.fields(), has: c.has("age") && !c.has("zz"),
                 immutable: base.fields().length === 2,
               })).catch(e => json.fail(500, String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert_eq!(v["data"]["eq"], true, "{v}");
    assert_eq!(v["data"]["fields"], json!(["age", "ok", "tag"]), "{v}");
    assert_eq!(v["data"]["has"], true, "{v}");
    assert_eq!(v["data"]["immutable"], true, "{v}");
}

#[test]
fn col_ctx_qualified_and_unqualified_rules() {
    let reg = SchemaRegistry::new()
        .table("a", &["id"], &["id", "name"])
        .table("b", &["id"], &["id", "aid", "label"]);
    let ctx = ColCtx {
        base_name: "a",
        base: TableSrc::Real(reg.get("a").unwrap()),
        joins: vec![("b", TableSrc::Real(reg.get("b").unwrap()))],
        ctes: Vec::new(),
    };
    assert!(ctx.check_col("name", "select").is_ok());
    assert!(ctx.check_col("b.label", "select").is_ok());
    assert!(ctx.check_col("a.id", "where").is_ok());
    assert!(ctx.check_col("label", "select").is_err()); // 非限定不解析 join 表
    assert!(ctx.check_col("b.nope", "select").is_err());
    assert!(ctx.check_col("c.id", "select").is_err()); // 表段不在 {a, b}
}

#[test]
fn verb_matrix_enforced_op_side() {
    let bad = |req: &str| validate_verb(&serde_json::from_str(req).unwrap()).unwrap_err();
    // select 拒 values/sets
    assert!(
        bad(r#"{"table":"t","values":[{"a":1}]}"#)
            .to_string()
            .contains("select does not accept values")
    );
    // insert：values 与 fromSelect 两缺 / 带 where / 带 limit
    assert!(
        bad(r#"{"table":"t","verb":"insert"}"#)
            .to_string()
            .contains("insert needs either values or fromSelect")
    );
    assert!(bad(
        r#"{"table":"t","verb":"insert","values":[{"a":1}],"conditions":[{"field":"a","op":"eq","value":1}]}"#
    )
    .to_string()
    .contains("insert does not accept where"));
    assert!(
        bad(r#"{"table":"t","verb":"insert","values":[{"a":1}],"limit":5}"#)
            .to_string()
            .contains("insert does not accept limit")
    );
    assert!(bad(
        r#"{"table":"t","verb":"insert","values":[{"a":1}],"joins":[{"table":"b","on":[{"left":"a.id","right":"b.aid"}]}]}"#
    )
    .to_string()
    .contains("insert does not accept joins"));
    // update：空 sets / 无 where / limit
    assert!(
        bad(r#"{"table":"t","verb":"update","conditions":[{"field":"a","op":"eq","value":1}]}"#)
            .to_string()
            .contains("non-empty sets")
    );
    assert!(
        bad(r#"{"table":"t","verb":"update","sets":{"a":1}}"#)
            .to_string()
            .contains("requires where")
    );
    assert!(bad(
        r#"{"table":"t","verb":"update","sets":{"a":1},"conditions":[{"field":"a","op":"eq","value":1}],"limit":5}"#
    )
    .to_string()
    .contains("limit/offset"));
    // delete：无 where
    assert!(
        bad(r#"{"table":"t","verb":"delete"}"#)
            .to_string()
            .contains("requires where")
    );
    // 合法形态 Ok
    assert!(
        validate_verb(
            &serde_json::from_str(
                r#"{"table":"t","verb":"delete","conditions":[{"field":"a","op":"eq","value":1}]}"#
            )
            .unwrap()
        )
        .is_ok()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn dml_insert_update_delete_and_tx_routing() {
    let b = seeded_bridge().await;
    let cap = b
        .run(
            r#"(async () => {
               const ins = await db.table("t").insert([{name:"e",age:50},{name:"f",age:60}]).run();
               const upd = await db.table("t").update({age:55}).where({field:"name",op:"eq",value:"e"}).run();
               const del = await db.table("t").delete().where({field:"name",op:"eq",value:"f"}).run();
               const n = await db.table("t").select(["name"]).all().then(r => r.length);
               // tx 路由：tx 内 insert 回滚后不可见
               let txErr = false;
               try { await db.tx(async (tx) => { await tx.table("t").insert({name:"g",age:1}).run(); throw new Error("boom"); }); }
               catch (e) { txErr = true; }
               const g = await db.table("t").select(["name"]).where({field:"name",op:"eq",value:"g"}).all();
               json.ok({ ins, upd, del, n, txErr, g: g.length });
             })().catch(e => json.fail(500, String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert_eq!(v["data"]["ins"], 2, "{v}");
    assert_eq!(v["data"]["upd"], 1, "{v}");
    assert_eq!(v["data"]["del"], 1, "{v}");
    assert_eq!(v["data"]["n"], 5, "{v}"); // 4 种子 + e - f + e 留下 = 5
    assert_eq!(v["data"]["txErr"], true, "{v}");
    assert_eq!(v["data"]["g"], 0, "{v}"); // 回滚
}

#[tokio::test(flavor = "current_thread")]
async fn dml_rejects_bad_shapes() {
    let b = seeded_bridge().await;
    // 行间键集不一致
    let cap = b
        .run(
            r#"db.table("t").insert([{name:"x"},{name:"y",age:1}]).run()
    .then(()=>json.ok({})).catch(e=>json.fail(400,String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert!(
        v["msg"].as_str().unwrap().contains("identical key sets"),
        "{v}"
    );
    // 键不在白名单
    let cap = b
        .run(
            r#"db.table("t").insert({nope:1}).run()
    .then(()=>json.ok({})).catch(e=>json.fail(400,String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert!(
        v["msg"].as_str().unwrap().contains("unknown column 'nope'"),
        "{v}"
    );
    // JS 链层早抛：update 无 where（run() 同步 throw，须在 async 上下文才能被 catch）
    let cap = b
        .run(
            r#"(async () => db.table("t").update({age:1}).run())()
    .then(()=>json.ok({})).catch(e=>json.fail(400,String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert!(v["msg"].as_str().unwrap().contains("requires where"), "{v}");
}

#[tokio::test(flavor = "current_thread")]
async fn aggregate_columns_alias_and_distinct() {
    let b = seeded_bridge().await;
    let cap = b
        .run(
            r#"Promise.all([
                 db.table("t").select([{fn:"count",as:"n"}]).all(),
                 db.table("t").select([{fn:"sum",field:"age",as:"total"}]).all(),
                 db.table("t").select(["tag"]).distinct().all(),
               ]).then(([c, s, d]) => json.ok({ n: c[0].n, total: s[0].total, d: d.length }))
                 .catch(e => json.fail(500, String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert_eq!(v["data"]["n"], 4, "{v}");
    assert_eq!(v["data"]["total"], 100, "{v}"); // 10+20+30+40
    assert_eq!(v["data"]["d"], 3, "{v}"); // x, y, NULL
    // 非法别名 / 未知 fn / sum 缺 field
    for (sel, want) in [
        (r#"[{fn:"count",as:"1bad"}]"#, "illegal alias"),
        (r#"[{fn:"median",field:"age"}]"#, "unknown variant"),
        (r#"[{fn:"sum"}]"#, "aggregate needs field"),
    ] {
        let cap = b
            .run(&format!(
                r#"db.table("t").select({sel}).all().then(()=>json.ok({{}})).catch(e=>json.fail(400,String(e)));"#
            ))
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(v["msg"].as_str().unwrap().contains(want), "{want}: {v}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn group_by_having_with_alias_expansion() {
    let b = seeded_bridge().await;
    // 按 tag 分组 count>1 → tag=x (2 行)
    let cap = b
        .run(
            r#"db.table("t").select(["tag", {fn:"count",as:"n"}])
               .groupBy(["tag"]).having({field:"n",op:"gt",value:1})
               .orderBy([{field:"tag",dir:"asc"}]).all()
               .then(r => json.ok({ rows: r.length, tag: r[0].tag, n: r[0].n }))
               .catch(e => json.fail(500, String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert_eq!(v["data"]["rows"], 1, "{v}");
    assert_eq!(v["data"]["tag"], "x", "{v}");
    assert_eq!(v["data"]["n"], 2, "{v}");
    // having 用白名单列（列优先于别名）
    let cap = b
        .run(
            r#"db.table("t").select(["tag", {fn:"sum",field:"age",as:"n"}])
               .groupBy(["tag"]).having({field:"age",op:"gt",value:0})
               .all().then(r => json.ok({ n: r.length })).catch(e => json.fail(500, String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    // having 引用未知别名/列 → 报错
    let cap = b.run(r#"db.table("t").select(["tag"]).groupBy(["tag"]).having({field:"zz",op:"eq",value:1}).all()
        .then(()=>json.ok({})).catch(e=>json.fail(400,String(e)));"#).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert!(
        v["msg"]
            .as_str()
            .unwrap()
            .contains("unknown column 'zz' in having"),
        "{v}"
    );
    // 别名展开的 SQL 不含别名字样（方言验证：展开后 HAVING 引用原聚合表达式）
    let cap = b.run(r#"json.ok(db.table("t").select([{fn:"count",as:"n"}]).groupBy(["tag"]).having({field:"n",op:"gt",value:1}).toSQL());"#).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    let sql = v["data"]["sql"].as_str().unwrap().to_string();
    assert!(
        sql.contains("COUNT(") && !sql.contains("HAVING \"n\""),
        "{sql}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn to_json_from_json_roundtrip() {
    let b = seeded_bridge().await;
    let cap = b
        .run(
            r#"(async () => {
               const c = db.and({field:"age",op:"gte",value:18});
               const q = db.table("t").select(["name"]).where(c).limit(10);
               const snap = q.toJSON();
               snap.limit = 1;
               const via = await db.fromJSON(snap).all();
               const direct = await db.table("t").select(["name"]).where(c).limit(1).all();
               json.ok({ same: via.length === direct.length && via[0].name === direct[0].name,
                         plain: typeof snap.conditions[0].and !== "undefined" });
             })().catch(e => json.fail(500, String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert_eq!(v["data"]["same"], true, "{v}");
    assert_eq!(v["data"]["plain"], true, "{v}"); // 条件对象已解包为纯 JSON
    // fromJSON 非法树被拒（同手写非法树：delete 无 where → JS 链层同步早抛，须 async 包裹）
    let cap = b
        .run(
            r#"(async () => db.fromJSON({table:"t",verb:"delete"}).run())()
               .then(()=>json.ok({})).catch(e=>json.fail(400,String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert!(v["msg"].as_str().unwrap().contains("requires where"), "{v}");
}

#[tokio::test(flavor = "current_thread")]
async fn subquery_where_and_exists() {
    let b = seeded_bridge_2t().await;
    // in 子查询：b 中 label=L1 的 aid={1} → a.id ∈ {1} → 1 行 x
    let cap = b
        .run(
            r#"db.table("a").select(["name"])
    .where({field:"id",op:"in",subquery:db.table("b").select(["aid"])
        .where({field:"label",op:"eq",value:"L1"})}).all()
    .then(r=>json.ok({n:r.length,name:r[0].name})).catch(e=>json.fail(400,String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["n"], 1, "{v}");
    assert_eq!(v["data"]["name"], "x", "{v}");
    // 标量 eq 子查询：aid of L1 = 1 → a.id = 1 → 1 行
    let cap = b
        .run(
            r#"db.table("a").select(["name"])
    .where({field:"id",op:"eq",subquery:db.table("b").select(["aid"])
        .where({field:"label",op:"eq",value:"L1"}).limit(1)}).all()
    .then(r=>json.ok({n:r.length})).catch(e=>json.fail(400,String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["n"], 1, "{v}");
    // exists（非关联）：b 有 L2 → a 全量 2 行
    let cap = b
        .run(
            r#"db.table("a").select(["name"])
    .where({exists:db.table("b").select(["aid"]).where({field:"label",op:"eq",value:"L2"})}).all()
    .then(r=>json.ok({n:r.length})).catch(e=>json.fail(400,String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["n"], 2, "{v}");
    // 裸 JSON 树等价（不走 builder 包装）
    let cap = b
        .run(
            r#"db.table("a").select(["name"])
    .where({field:"id",op:"in",subquery:{table:"b",columns:["aid"],
        conditions:[{field:"label",op:"eq",value:"L1"}]}}).all()
    .then(r=>json.ok({n:r.length})).catch(e=>json.fail(400,String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["n"], 1, "{v}");
}

#[tokio::test(flavor = "current_thread")]
async fn subquery_rejections() {
    let b = seeded_bridge_2t().await;
    for (js, want) in [
        // value 与 subquery 同现
        (
            r#"db.table("a").where({field:"id",op:"in",value:[1],subquery:{table:"b",columns:["aid"]}}).all()"#,
            "value and subquery are mutually exclusive",
        ),
        // isnull 不接受 subquery
        (
            r#"db.table("a").where({field:"id",op:"isnull",subquery:{table:"b",columns:["aid"]}}).all()"#,
            "isnull does not accept subquery",
        ),
        // 嵌套 req 动词非 select
        (
            r#"db.table("a").where({field:"id",op:"in",subquery:{table:"b",verb:"delete",columns:["aid"],
             conditions:[{field:"aid",op:"eq",value:1}]}}).all()"#,
            "nested select",
        ),
        // 嵌套 req 未知列（递归过白名单）
        (
            r#"db.table("a").where({field:"id",op:"in",subquery:{table:"b",columns:["nope"]}}).all()"#,
            "unknown column",
        ),
    ] {
        let cap = b
            .run(&format!(
                r#"{js}.then(()=>json.ok({{}})).catch(e=>json.fail(400,String(e)));"#
            ))
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(v["msg"].as_str().unwrap().contains(want), "{want}: {v}");
    }
    // 深度超限：5 层 exists 嵌套（REQ_NEST_MAX=4）
    let mut js = String::from(r#"db.table("a").select(["id"])"#);
    let mut inner = String::from(r#"{table:"b",columns:["aid"]}"#);
    for _ in 0..5 {
        inner = format!(r#"{{table:"b",columns:["aid"],conditions:[{{exists:{inner}}}]}}"#);
    }
    js.push_str(&format!(r#".where({{exists:{inner}}}).all()"#));
    let cap = b
        .run(&format!(
            r#"{js}.then(()=>json.ok({{}})).catch(e=>json.fail(400,String(e)));"#
        ))
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert!(
        v["msg"]
            .as_str()
            .unwrap()
            .contains("nested select too deep"),
        "{v}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn union_all_and_distinct() {
    let b = seeded_bridge_2t().await;
    // a.name {x,y} ∪ b.label {L1,L2,L3}：all=5 行，distinct=5 行（无重叠）
    let cap = b
        .run(
            r#"db.table("a").select(["name"])
        .union(db.table("b").select(["label"]), "all").all()
        .then(r=>json.ok({n:r.length})).catch(e=>json.fail(400,String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["n"], 5, "{v}");
    let cap = b
        .run(
            r#"db.table("a").select(["name"])
        .union(db.table("b").select(["label"]).where({field:"aid",op:"eq",value:1})).all()
        .then(r=>json.ok({n:r.length})).catch(e=>json.fail(400,String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["n"], 4, "{v}"); // 2 + 2(L1,L2)
    // toSQL 含 UNION ALL（同步 op，直接进 json.ok）
    let cap = b
        .run(
            r#"json.ok(db.table("a").select(["name"])
        .union(db.table("b").select(["label"]), "all").toSQL());"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert!(
        v["data"]["sql"].as_str().unwrap().contains("UNION ALL"),
        "{v}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn union_rejections() {
    let b = seeded_bridge_2t().await;
    for (js, want) in [
        // 列数不一致
        (
            r#"db.table("a").select(["id","name"]).union(db.table("b").select(["label"])).all()"#,
            "union column count mismatch",
        ),
        // 成员带 limit
        (
            r#"db.table("a").select(["name"]).union(db.table("b").select(["label"]).limit(1)).all()"#,
            "union member does not accept order_by/limit/offset",
        ),
        // 基查询隐式列（union 必须显式 columns）
        (
            r#"db.table("a").union(db.table("b").select(["label"])).all()"#,
            "union requires explicit columns",
        ),
        // union 套 union（嵌套禁 unions）
        (
            r#"db.table("a").select(["name"]).union(db.table("b").select(["label"])
         .union(db.table("b").select(["label"]))).all()"#,
            "nested select does not accept with/unions",
        ),
        // insert 带 unions（动词矩阵）
        (
            r#"db.table("a").insert({name:"z"}).union(db.table("b").select(["label"])).run()"#,
            "insert does not accept unions",
        ),
    ] {
        let cap = b
            .run(&format!(
                r#"{js}.then(()=>json.ok({{}})).catch(e=>json.fail(400,String(e)));"#
            ))
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(v["msg"].as_str().unwrap().contains(want), "{want}: {v}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn case_and_window_columns() {
    let b = seeded_bridge_2t().await;
    // case：id=1 → "one"，否则 "other"（a: id=1 x, id=2 y → 按序 one/other）
    let cap = b
        .run(
            r#"db.table("a").select(["name",
    {case:{when:[{cond:{field:"id",op:"eq",value:1},then:"one"}],else:"other"},as:"tag"}
  ]).orderBy([{field:"id",dir:"asc"}]).all()
  .then(r=>json.ok({tags:r.map(x=>x.tag)})).catch(e=>json.fail(400,String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["tags"], json!(["one", "other"]), "{v}");
    // window：row_number over (order by id desc) → y=1, x=2
    let cap = b
        .run(
            r#"db.table("a").select(["name",
    {window:{fn:"row_number",order_by:[{field:"id",dir:"desc"}]},as:"rn"}
  ]).all()
  .then(r=>json.ok({rows:r})).catch(e=>json.fail(400,String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    let rows = v["data"]["rows"].as_array().unwrap();
    let rn_of = |n: &str| {
        rows.iter().find(|r| r["name"] == n).unwrap()["rn"]
            .as_i64()
            .unwrap()
    };
    assert_eq!(rn_of("y"), 1, "{v}");
    assert_eq!(rn_of("x"), 2, "{v}");
    // partition_by：b 表按 aid 分区编号
    let cap = b
        .run(
            r#"db.table("b").select(["label",
    {window:{fn:"rank",partition_by:["aid"],order_by:[{field:"id",dir:"asc"}]},as:"rk"}
  ]).all()
  .then(r=>json.ok({rows:r})).catch(e=>json.fail(400,String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
}

#[tokio::test(flavor = "current_thread")]
async fn case_window_rejections() {
    let b = seeded_bridge_2t().await;
    for (js, want) in [
        // 别名形状非法（brief 负向用例把 as 误写进 case 内部，按权威形状
        // {case:{when},as} 调整——与正向用例一致）
        (
            r#"db.table("a").select([{case:{when:[{cond:{field:"id",op:"eq",value:1},then:1}]},as:"0bad"}]).all()"#,
            "illegal alias",
        ),
        // window fn 非枚举值
        (
            r#"db.table("a").select([{window:{fn:"ntile",order_by:[]},as:"x"}]).all()"#,
            "unknown variant",
        ),
        // having 引用 window 别名（case/window 别名不进 having 台账）
        (
            r#"db.table("a").select([{window:{fn:"row_number"},as:"rn"}]).groupBy(["id"])
         .having({field:"rn",op:"eq",value:1}).all()"#,
            "unknown column",
        ),
        // 空 when（.run 为真值使 && 链求值到 select——不触达 insert 语义；
        // brief 同样把 as 误写进 case 内部，按权威形状调整）
        (
            r#"db.table("a").insert({name:"z"}).run && db.table("a").select([{case:{when:[]},as:"x"}]).all()"#,
            "case needs non-empty when",
        ),
    ] {
        let cap = b
            .run(&format!(
                r#"{js}.then(()=>json.ok({{}})).catch(e=>json.fail(400,String(e)));"#
            ))
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(v["msg"].as_str().unwrap().contains(want), "{want}: {v}");
    }
}

/// 统一审查 F-2/F-3：嵌套 offset 必须与 limit 成对；CASE when 数上限。
#[tokio::test(flavor = "current_thread")]
async fn nested_offset_and_case_when_caps() {
    let b = seeded_bridge_2t().await;
    for (js, want) in [
        // 嵌套 offset 无 limit：原渲染裸 OFFSET（DB 语法错），现校验期拒绝
        (
            r#"db.table("a").select(["name"]).where({field:"id",op:"in",
         subquery:db.table("b").select(["aid"]).offset(1)}).all()"#,
            "nested select offset requires limit",
        ),
        // 成对出现则合法（正向对照）
        (
            r#"db.table("a").select(["name"]).where({field:"id",op:"in",
         subquery:db.table("b").select(["aid"]).limit(1).offset(1)}).all()"#,
            "CODE0",
        ),
        // CASE when 数 17 > 16
        (
            r#"db.table("a").select([{case:{when:[
           {cond:{field:"id",op:"eq",value:1},then:1},{cond:{field:"id",op:"eq",value:2},then:2},
           {cond:{field:"id",op:"eq",value:3},then:3},{cond:{field:"id",op:"eq",value:4},then:4},
           {cond:{field:"id",op:"eq",value:5},then:5},{cond:{field:"id",op:"eq",value:6},then:6},
           {cond:{field:"id",op:"eq",value:7},then:7},{cond:{field:"id",op:"eq",value:8},then:8},
           {cond:{field:"id",op:"eq",value:9},then:9},{cond:{field:"id",op:"eq",value:10},then:10},
           {cond:{field:"id",op:"eq",value:11},then:11},{cond:{field:"id",op:"eq",value:12},then:12},
           {cond:{field:"id",op:"eq",value:13},then:13},{cond:{field:"id",op:"eq",value:14},then:14},
           {cond:{field:"id",op:"eq",value:15},then:15},{cond:{field:"id",op:"eq",value:16},then:16},
           {cond:{field:"id",op:"eq",value:17},then:17}],else:0},as:"x"}]).all()"#,
            "case when count exceeds 16",
        ),
    ] {
        let cap = b
            .run(&format!(
                r#"{js}.then(()=>json.ok({{}})).catch(e=>json.fail(400,String(e)));"#
            ))
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        if want == "CODE0" {
            assert_eq!(v["code"], 0, "{v}");
        } else {
            assert!(v["msg"].as_str().unwrap().contains(want), "{want}: {v}");
        }
    }
}

/// 统一审查 F-4：值含引号/反斜杠/换行/制表符经绑定参数往返无损。
#[tokio::test(flavor = "current_thread")]
async fn special_char_values_round_trip() {
    let b = seeded_bridge().await;
    let cap = b
        .run(
            r#"const v = "o'brien\\x\n\ty\"z";
         db.table("t").insert({name:v}).run()
           .then(() => db.table("t").where({field:"name",op:"eq",value:v}).all())
           .then((rows) => json.ok({hit: rows.length === 1 && rows[0].name === v}))
           .catch((e) => json.fail(400, String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["hit"], json!(true), "{v}");
}

/// 动词矩阵：columns 仅 select（case/window 是 columns 元素级能力，DML 一并拦）。
#[test]
fn verb_matrix_rejects_columns_on_dml() {
    let bad = |req: &str| validate_verb(&serde_json::from_str(req).unwrap()).unwrap_err();
    assert!(
        bad(r#"{"table":"t","verb":"insert","values":[{"a":1}],"columns":["a"]}"#)
            .to_string()
            .contains("insert does not accept columns")
    );
    assert!(bad(
        r#"{"table":"t","verb":"update","sets":{"a":1},"conditions":[{"field":"a","op":"eq","value":1}],"columns":["a"]}"#
    )
    .to_string()
    .contains("update/delete does not accept columns"));
    assert!(bad(
        r#"{"table":"t","verb":"delete","conditions":[{"field":"a","op":"eq","value":1}],"columns":["a"]}"#
    )
    .to_string()
    .contains("update/delete does not accept columns"));
}

/// returning：仅 insert 接受；pg/sqlite 渲染 RETURNING，mysql 不渲染（两步取 id）。
#[test]
fn returning_verb_matrix_and_dialect_rendering() {
    let reg = SchemaRegistry::new().table("t", &["id"], &["id", "name"]);
    let sql_of = |req: &str, d: Dialect| {
        let req: QueryReq = serde_json::from_str(req).unwrap();
        build_statement(&req, &reg, d).map(|(s, _)| s)
    };
    let ins = r#"{"table":"t","verb":"insert","values":[{"name":"a"}],"returning":["id"]}"#;
    assert!(sql_of(ins, Dialect::Sqlite).unwrap().contains("RETURNING"));
    assert!(
        sql_of(ins, Dialect::Postgres)
            .unwrap()
            .contains("RETURNING")
    );
    // mysql 无 RETURNING：由 op 侧两条语句取 LAST_INSERT_ID()。
    assert!(!sql_of(ins, Dialect::MySql).unwrap().contains("RETURNING"));
    // 未知列 / 限定名（白名单按素名查，自然被拒）
    let bad = r#"{"table":"t","verb":"insert","values":[{"name":"a"}],"returning":["nope"]}"#;
    assert!(
        sql_of(bad, Dialect::Sqlite)
            .unwrap_err()
            .to_string()
            .contains("unknown column 'nope' in insert returning")
    );
    // mysql 多列 → 拒
    let bad = r#"{"table":"t","verb":"insert","values":[{"name":"a"}],"returning":["id","name"]}"#;
    assert!(
        sql_of(bad, Dialect::MySql)
            .unwrap_err()
            .to_string()
            .contains("exactly one column")
    );
    // 动词矩阵：select / update / delete 一律拒
    assert!(
        sql_of(r#"{"table":"t","returning":["id"]}"#, Dialect::Sqlite)
            .unwrap_err()
            .to_string()
            .contains("select does not accept returning")
    );
    let upd = r#"{"table":"t","verb":"update","sets":{"name":"x"},
        "conditions":[{"field":"id","op":"eq","value":1}],"returning":["id"]}"#;
    assert!(
        sql_of(upd, Dialect::Sqlite)
            .unwrap_err()
            .to_string()
            .contains("update/delete does not accept returning")
    );
}

/// insert + returning：池路径与事务路径都能取回自增 id；事务回滚后不可见。
#[tokio::test(flavor = "current_thread")]
async fn insert_returning_id_in_pool_and_tx() {
    let b = seeded_bridge().await;
    // 池路径：RETURNING 单语句取回 id（sqlite）
    let cap = b
        .run(
            r#"db.table("t").insert({name:"r1",age:1}).returning(["id"]).run()
               .then(r => json.ok({rows:r})).catch(e => json.fail(500,String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert!(v["data"]["rows"][0]["id"].as_i64().unwrap() > 0, "{v}");
    // 事务路径：同一会话取 id；throw 回滚后该行不可见
    let cap = b
        .run(
            r#"(async () => {
                 const id = await db.tx(async (tx) => {
                   const r = await tx.table("t").insert({name:"r2",age:2}).returning(["id"]).run();
                   return r[0].id;
                 });
                 let gone = null;
                 try {
                   await db.tx(async (tx) => {
                     await tx.table("t").insert({name:"r3",age:3}).returning(["id"]).run();
                     throw new Error("boom");
                   });
                 } catch (e) { gone = String(e); }
                 const left = await db.table("t").select(["name"])
                   .where({field:"name",op:"in",value:["r2","r3"]}).all();
                 json.ok({id, gone, names: left.map(x => x.name)});
               })().catch(e => json.fail(500,String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert!(v["data"]["id"].as_i64().unwrap() > 0, "{v}");
    assert_eq!(v["data"]["gone"], json!("Error: boom"), "{v}");
    assert_eq!(v["data"]["names"], json!(["r2"]), "{v}"); // r3 已回滚
}

/// CTE：主表 = CTE 与 join CTE（声明列即白名单；真实执行验证）。
#[tokio::test(flavor = "current_thread")]
async fn cte_main_and_join() {
    let b = seeded_bridge_2t().await;
    // 主表 = CTE：r(aid,label) = b 中 aid=1 → 2 行
    let cap = b
        .run(
            r#"db.table("r").with("r", ["aid","label"],
    db.table("b").select(["aid","label"]).where({field:"aid",op:"eq",value:1}))
  .select(["aid","label"]).orderBy([{field:"aid",dir:"asc"}]).all()
  .then(r=>json.ok({n:r.length,first:r[0].label})).catch(e=>json.fail(400,String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["n"], 2, "{v}");
    assert_eq!(v["data"]["first"], "L1", "{v}");
    // join CTE：a ⋈ r on a.id = r.aid → x × {L1,L2} = 2 行
    let cap = b
        .run(
            r#"db.table("a").with("r", ["aid","label"],
    db.table("b").select(["aid","label"]).where({field:"aid",op:"eq",value:1}))
  .join("r", [{left:"a.id",right:"r.aid"}]).select(["a.name","r.label"]).all()
  .then(r=>json.ok({n:r.length})).catch(e=>json.fail(400,String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["n"], 2, "{v}");
}

/// CTE 负向：别名形状 / 空 columns / 未声明列 / 嵌套带 with / DML 带 with。
#[tokio::test(flavor = "current_thread")]
async fn cte_rejections() {
    let b = seeded_bridge_2t().await;
    for (js, want) in [
        // name 形状非法（既有 check_alias 锁定文案为 illegal alias，brief 写 invalid）
        (
            r#"db.table("r").with("0bad", ["aid"], db.table("b").select(["aid"])).select(["aid"]).all()"#,
            "illegal alias",
        ),
        // columns 空
        (
            r#"db.table("r").with("r", [], db.table("b").select(["aid"])).select(["aid"]).all()"#,
            "cte needs non-empty columns",
        ),
        // 引用未声明的 CTE 列
        (
            r#"db.table("r").with("r", ["aid"], db.table("b").select(["aid"])).select(["r.nope"]).all()"#,
            "unknown column",
        ),
        // 嵌套 req 带 with
        (
            r#"db.table("a").select(["id"]).where({field:"id",op:"in",
             subquery:{table:"b",columns:["aid"],with:[{name:"x",columns:["aid"],
               query:{table:"b",columns:["aid"]}}]}}).all()"#,
            "nested select does not accept with/unions",
        ),
        // insert 带 with（动词矩阵）
        (
            r#"db.table("a").insert({name:"z"}).with("r",["aid"],db.table("b").select(["aid"])).run()"#,
            "insert does not accept with",
        ),
    ] {
        let cap = b
            .run(&format!(
                r#"{js}.then(()=>json.ok({{}})).catch(e=>json.fail(400,String(e)));"#
            ))
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(v["msg"].as_str().unwrap().contains(want), "{want}: {v}");
    }
}

// ----- 多租户 sql_guard：apply_tenant 注入（Task 3） -----

use crate::bridge::{Extras, RequestInfo, SqlGuard};

/// 租户夹具：t（受约束，tenant_id 列）+ s（共享表）+ 两租户种子。
async fn guarded_bridge() -> Bridge {
    let db = SqlxAccessor::arc("sqlite::memory:").await.unwrap();
    db.exec_with_params(
        "create table t (id integer primary key, name text, tenant_id text)",
        &[],
    )
    .await
    .unwrap();
    db.exec_with_params("create table s (id integer primary key, name text)", &[])
        .await
        .unwrap();
    db.exec_with_params(
        "create table u (id integer primary key, label text, tenant_id text)",
        &[],
    )
    .await
    .unwrap();
    for (n, tid) in [("a1", "t1"), ("a2", "t1"), ("b1", "t2")] {
        db.exec_with_params(
            "insert into t (name, tenant_id) values (?, ?)",
            &[json!(n), json!(tid)],
        )
        .await
        .unwrap();
    }
    // u：id=1 属 t1，id=3 属 t2（join 语义验证用）。
    for (id, l, tid) in [(1, "ua", "t1"), (3, "ub", "t2")] {
        db.exec_with_params(
            "insert into u (id, label, tenant_id) values (?, ?, ?)",
            &[json!(id), json!(l), json!(tid)],
        )
        .await
        .unwrap();
    }
    let reg = SchemaRegistry::new()
        .table("t", &["id"], &["id", "name", "tenant_id"])
        .table("u", &["id"], &["id", "label", "tenant_id"])
        .table_owned_shared("m", "s", &["id"], &["id", "name"], true);
    Bridge::with_dbs_and_loader(
        std::collections::HashMap::from([("default".to_string(), db as _)]),
        Arc::new(InMemoryKV::new()),
        reg,
        false,
        None,
        Extras {
            sql_guard: SqlGuard::Deny,
            ..Default::default()
        },
    )
}

fn req_t1() -> RequestInfo {
    RequestInfo {
        tenant_id: Some("t1".into()),
        ..Default::default()
    }
}

/// 数值型 `tenant_id` 列的夹具（v0.1.24）：`n`（integer 租户列）+ `n2`（join 用）。
async fn numeric_tenant_bridge() -> Bridge {
    let db = SqlxAccessor::arc("sqlite::memory:").await.unwrap();
    for ddl in [
        "create table n (id integer primary key, name text, tenant_id integer)",
        "create table n2 (id integer primary key, label text, tenant_id integer)",
    ] {
        db.exec_with_params(ddl, &[]).await.unwrap();
    }
    for (n, tid) in [("n7", 7), ("n8", 8)] {
        db.exec_with_params(
            "insert into n (name, tenant_id) values (?, ?)",
            &[json!(n), json!(tid)],
        )
        .await
        .unwrap();
    }
    db.exec_with_params(
        "insert into n2 (id, label, tenant_id) values (?, ?, ?)",
        &[json!(1), json!("o7"), json!(7)],
    )
    .await
    .unwrap();
    let reg = SchemaRegistry::new()
        .table_owned_shared_typed(
            "m",
            "n",
            &["id"],
            &[
                ("id", super::super::registry::ColumnType::Integer),
                ("name", super::super::registry::ColumnType::Text),
                ("tenant_id", super::super::registry::ColumnType::Integer),
            ],
            false,
        )
        .table_owned_shared_typed(
            "m",
            "n2",
            &["id"],
            &[
                ("id", super::super::registry::ColumnType::Integer),
                ("label", super::super::registry::ColumnType::Text),
                ("tenant_id", super::super::registry::ColumnType::Integer),
            ],
            false,
        );
    Bridge::with_dbs_and_loader(
        std::collections::HashMap::from([("default".to_string(), db as _)]),
        Arc::new(InMemoryKV::new()),
        reg,
        false,
        None,
        Extras {
            sql_guard: SqlGuard::Deny,
            ..Default::default()
        },
    )
}

fn req_tenant(id: &str) -> RequestInfo {
    RequestInfo {
        tenant_id: Some(id.into()),
        ..Default::default()
    }
}

/// 数值型 `tenant_id`（v0.1.24）：注入条件 / insert 强制写 / update 拒绝 / join ON
/// 全部按**列类型**走数值，且写成「等值四形态」判定（字符串与数字不再二选一）。
#[tokio::test(flavor = "current_thread")]
async fn numeric_tenant_column_binds_numbers_end_to_end() {
    let b = numeric_tenant_bridge().await;
    let cap = b
        .run_with(
            r#"(async () => {
                 const rows = await db.table("n").select(["name"]).all();
                 const s = db.table("n").select(["name"]).toSQL();
                 const j = db.table("n").select(["name"])
                   .join("n2", [{left:"tenant_id",right:"tenant_id"}]).toSQL();
                 await db.table("n").insert({ name: "n9" }).run();
                 const after = await db.table("n").select(["name"]).orderBy([{field:"id",dir:"asc"}]).all();
                 const mism = await db.table("n").insert({ name: "bad", tenant_id: 8 })
                   .run().then(() => "ok", e => String(e));
                 const upd = await db.table("n").update({ tenant_id: 8 })
                   .where({field:"id",op:"eq",value:1}).run().then(() => "ok", e => String(e));
                 json.ok({
                   rows: rows.map(r => r.name),
                   params: s.params, joinParams: j.params,
                   after: after.map(r => r.name), mism, upd,
                 });
               })().catch(e => json.fail(500, String(e)));"#,
            req_tenant("7"),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert_eq!(v["data"]["rows"], json!(["n7"]), "{v}");
    // 注入的是**数值** 7（不是字符串 "7"）——这正是 PG 数值列不再报 bigint = text 的原因。
    assert!(
        v["data"]["params"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p == &json!(7)),
        "{v}"
    );
    // join 的 ON 条件同型（数值）。
    assert!(
        v["data"]["joinParams"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p == &json!(7)),
        "{v}"
    );
    // insert 未给 tenant_id → 强制写数值 7；给了别的值 → 四形态等值判定拦下。
    assert_eq!(v["data"]["after"], json!(["n7", "n9"]), "{v}");
    assert!(
        v["data"]["mism"].as_str().unwrap().contains("mismatch"),
        "{v}"
    );
    assert!(
        v["data"]["upd"].as_str().unwrap().contains("not allowed"),
        "{v}"
    );
}

/// 数值型租户列 + 非十进制租户头 → 明确报错（而不是把 "acme" 绑给 integer 列，
/// 让 PG 回一句看不出根因的 `invalid input syntax for type integer`）。
#[tokio::test(flavor = "current_thread")]
async fn numeric_tenant_column_rejects_non_numeric_tid() {
    let b = numeric_tenant_bridge().await;
    let cap = b
        .run_with(
            r#"(async () => {
                 const r = await db.table("n").select(["name"]).all()
                   .then(() => "ok", e => String(e));
                 json.ok({ r });
               })().catch(e => json.fail(500, String(e)));"#,
            req_tenant("acme"),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    let msg = v["data"]["r"].as_str().unwrap();
    assert!(msg.contains("not a valid integer"), "{msg}");
    assert!(msg.contains("n.tenant_id"), "{msg}");
}

/// select：构造器查询自动收窄到当前租户；toSQL 产物含注入条件与参数。
#[tokio::test(flavor = "current_thread")]
async fn tenant_select_scoped() {
    let b = guarded_bridge().await;
    let cap = b
        .run_with(
            r#"(async () => {
                 const rows = await db.table("t").select(["name"]).orderBy([{field:"id",dir:"asc"}]).all();
                 const s = db.table("t").select(["name"]).toSQL();
                 json.ok({ names: rows.map(r => r.name), sql: s.sql, params: s.params });
               })().catch(e => json.fail(500, String(e)));"#,
            req_t1(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert_eq!(v["data"]["names"], json!(["a1", "a2"]), "{v}");
    assert!(
        v["data"]["sql"].as_str().unwrap().contains("tenant_id"),
        "{v}"
    );
    assert!(
        v["data"]["params"]
            .as_array()
            .unwrap()
            .contains(&json!("t1")),
        "{v}"
    );
}

/// asTenant 夹具：与 guarded_bridge 同数据，但开启 `tenant.allow_as_tenant`。
async fn guarded_bridge_allow_as_tenant(allow: bool) -> Bridge {
    let db = SqlxAccessor::arc("sqlite::memory:").await.unwrap();
    db.exec_with_params(
        "create table t (id integer primary key, name text, tenant_id text)",
        &[],
    )
    .await
    .unwrap();
    for (n, tid) in [("a1", "t1"), ("b1", "t2")] {
        db.exec_with_params(
            "insert into t (name, tenant_id) values (?, ?)",
            &[json!(n), json!(tid)],
        )
        .await
        .unwrap();
    }
    let reg = SchemaRegistry::new().table("t", &["id"], &["id", "name", "tenant_id"]);
    Bridge::with_dbs_and_loader(
        std::collections::HashMap::from([("default".to_string(), db as _)]),
        Arc::new(InMemoryKV::new()),
        reg,
        false,
        None,
        Extras {
            sql_guard: SqlGuard::Deny,
            allow_as_tenant: allow,
            ..Default::default()
        },
    )
}

fn req_anonymous() -> RequestInfo {
    RequestInfo {
        anonymous: true,
        ..Default::default()
    }
}

/// asTenant：匿名请求声明租户后，构造器照常注入该租户条件（仍强制，不是绕过）。
#[tokio::test(flavor = "current_thread")]
async fn as_tenant_scopes_query_on_anonymous_request() {
    let b = guarded_bridge_allow_as_tenant(true).await;
    let cap = b
        .run_with(
            r#"(async () => {
                 const d = db.asTenant("t2");
                 const rows = await d.table("t").select(["name"]).all();
                 const s = d.table("t").select(["name"]).toSQL();
                 json.ok({ names: rows.map(r => r.name), sql: s.sql, params: s.params });
               })().catch(e => json.fail(500, String(e)));"#,
            req_anonymous(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert_eq!(v["data"]["names"], json!(["b1"]), "{v}");
    assert!(
        v["data"]["sql"].as_str().unwrap().contains("tenant_id"),
        "{v}"
    );
    assert!(
        v["data"]["params"]
            .as_array()
            .unwrap()
            .contains(&json!("t2")),
        "{v}"
    );
}

/// asTenant 三道门禁：非匿名请求、已带租户头的请求、开关未开 —— 一律拒绝。
#[tokio::test(flavor = "current_thread")]
async fn as_tenant_gates() {
    // 同步抛出（未进 Promise 链）⇒ 一律用 async IIFE 兜住，否则断言看到的是 CoreError。
    let js = r#"(async () => {
                 await db.asTenant("t2").table("t").select(["name"]).all();
                 json.ok({});
               })().catch(e => json.fail(400, String(e)));"#;
    // ① 非匿名（无租户头但也没命中豁免，如 WS/任务/测试默认路径）
    let b = guarded_bridge_allow_as_tenant(true).await;
    let v: Value =
        serde_json::from_slice(&b.run_with(js, RequestInfo::default()).await.unwrap().body)
            .unwrap();
    assert_eq!(v["code"], 400, "{v}");
    assert!(
        v["msg"]
            .as_str()
            .unwrap()
            .contains("only allowed on anonymous"),
        "{v}"
    );
    // ② 已带租户头（防运行期切换身份）
    let v: Value = serde_json::from_slice(&b.run_with(js, req_t1()).await.unwrap().body).unwrap();
    assert_eq!(v["code"], 400, "{v}");
    assert!(
        v["msg"]
            .as_str()
            .unwrap()
            .contains("only allowed on anonymous"),
        "{v}"
    );
    // ③ 开关未开（默认 fail-closed）
    let b = guarded_bridge_allow_as_tenant(false).await;
    let v: Value =
        serde_json::from_slice(&b.run_with(js, req_anonymous()).await.unwrap().body).unwrap();
    assert_eq!(v["code"], 400, "{v}");
    assert!(v["msg"].as_str().unwrap().contains("is disabled"), "{v}");
    // ④ 空 id
    let v: Value = serde_json::from_slice(
        &b.run_with(
            r#"(async () => {
                 await db.asTenant("").table("t").select(["name"]).all();
                 json.ok({});
               })().catch(e => json.fail(400, String(e)));"#,
            req_anonymous(),
        )
        .await
        .unwrap()
        .body,
    )
    .unwrap();
    assert!(
        v["msg"].as_str().unwrap().contains("must not be empty"),
        "{v}"
    );
}

/// asTenant 是请求级：下一次 run（ReqState::reset）后失效，匿名请求重回 deny。
#[tokio::test(flavor = "current_thread")]
async fn as_tenant_is_request_scoped() {
    let b = guarded_bridge_allow_as_tenant(true).await;
    let v: Value = serde_json::from_slice(
        &b.run_with(
            r#"(async () => {
                 await db.asTenant("t2").table("t").select(["name"]).all();
                 json.ok({});
               })().catch(e => json.fail(400, String(e)));"#,
            req_anonymous(),
        )
        .await
        .unwrap()
        .body,
    )
    .unwrap();
    assert_eq!(v["code"], 0, "{v}");
    // reset 后不再有租户身份 → 受约束表被拒
    let v: Value = serde_json::from_slice(
        &b.run_with(
            r#"(async () => {
                 await db.table("t").select(["name"]).all();
                 json.ok({});
               })().catch(e => json.fail(400, String(e)));"#,
            req_anonymous(),
        )
        .await
        .unwrap()
        .body,
    )
    .unwrap();
    assert_eq!(v["code"], 400, "{v}");
    assert!(
        v["msg"]
            .as_str()
            .unwrap()
            .contains("require tenant context"),
        "{v}"
    );
}

/// insert：强制写入当前租户；显式传不符 tenant_id 报错；显式传相符放行。
#[tokio::test(flavor = "current_thread")]
async fn tenant_insert_forces_tid_and_rejects_mismatch() {
    let b = guarded_bridge().await;
    let cap = b
        .run_with(
            r#"db.table("t").insert({name:"a3"}).run()
                 .then(() => json.ok({})).catch(e => json.fail(400, String(e)));"#,
            req_t1(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    // 强制写入生效：新行带 t1
    let cap = b
        .run_with(
            r#"db.table("t").select(["name","tenant_id"]).where({field:"name",op:"eq",value:"a3"}).all()
                 .then(r => json.ok({n: r.length, tid: r[0].tenant_id}))
                 .catch(e => json.fail(500, String(e)));"#,
            req_t1(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(
        (&v["data"]["n"], &v["data"]["tid"]),
        (&json!(1), &json!("t1")),
        "{v}"
    );
    // 显式不符 → 报错
    let cap = b
        .run_with(
            r#"db.table("t").insert({name:"x", tenant_id:"t2"}).run()
                 .then(() => json.ok({})).catch(e => json.fail(400, String(e)));"#,
            req_t1(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 400, "{v}");
    assert!(
        v["msg"].as_str().unwrap().contains("tenant_id mismatch"),
        "{v}"
    );
}

/// update：sets.tenant_id 写侧逃逸一律报错；where 自动收窄（改不到他租户行）。
#[tokio::test(flavor = "current_thread")]
async fn tenant_update_sets_tid_rejected_and_where_narrowed() {
    let b = guarded_bridge().await;
    let cap = b
        .run_with(
            r#"db.table("t").update({name:"zz"}).where({field:"name",op:"eq",value:"b1"}).run()
                 .then(n => json.ok({n})).catch(e => json.fail(500, String(e)));"#,
            req_t1(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert_eq!(v["data"]["n"], 0, "t1 租户不应改到 t2 的行: {v}");
    // sets.tenant_id → 报错
    let cap = b
        .run_with(
            r#"(async () => db.table("t").update({tenant_id:"t2"}).where({field:"name",op:"eq",value:"a1"}).run())()
                 .then(() => json.ok({})).catch(e => json.fail(400, String(e)));"#,
            req_t1(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 400, "{v}");
    assert!(
        v["msg"]
            .as_str()
            .unwrap()
            .contains("sets.tenant_id not allowed"),
        "{v}"
    );
}

/// delete：自动收窄——t1 删不到 t2 的行。
#[tokio::test(flavor = "current_thread")]
async fn tenant_delete_narrowed() {
    let b = guarded_bridge().await;
    let cap = b
        .run_with(
            r#"db.table("t").delete().where({field:"name",op:"eq",value:"b1"}).run()
                 .then(n => json.ok({n})).catch(e => json.fail(500, String(e)));"#,
            req_t1(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!((&v["code"], &v["data"]["n"]), (&json!(0), &json!(0)), "{v}");
}

/// join：join 表同样受约束，条件进 ON（LEFT JOIN 他租户行出现 NULL，不变 INNER）。
#[tokio::test(flavor = "current_thread")]
async fn tenant_join_injects_on_clause() {
    let b = guarded_bridge().await;
    // LEFT JOIN：t1 基表 {a1(id1), a2(id2)}；u 行 id1=t1(ua) / id3=t2(ub)。
    // ON 注入 u.tenant_id='t1' → a2 的 label 为 NULL（LEFT 语义保留）。
    let cap = b
        .run_with(
            r#"db.table("t").join("u", [{left:"t.id",right:"u.id"}], "left")
                 .select(["t.name","u.label"]).orderBy([{field:"t.id",dir:"asc"}]).all()
                 .then(r => json.ok({rows: r})).catch(e => json.fail(500, String(e)));"#,
            req_t1(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert_eq!(
        v["data"]["rows"],
        json!([{"name": "a1", "label": "ua"}, {"name": "a2", "label": null}]),
        "{v}"
    );
    // toSQL 层面：条件在 ON 区段而非 WHERE。
    let cap = b
        .run_with(
            r#"json.ok(db.table("t").join("u", [{left:"t.id",right:"u.id"}], "left").select(["t.name"]).toSQL());"#,
            req_t1(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    let sql = v["data"]["sql"].as_str().unwrap().to_string();
    let on_part = sql.split("WHERE").next().unwrap_or("");
    assert!(on_part.contains("tenant_id"), "ON 区段应含注入条件: {sql}");
}

/// fromJSON 投毒回归（评审 P0）：快照 join 里预设 tenant_id:"t2" 必须被无条件
/// 覆盖为当前租户——fromJSON 绕过 JS 链层直喂 serde，预设值绝不可信。
#[tokio::test(flavor = "current_thread")]
async fn tenant_join_preset_tenant_id_overridden() {
    let b = guarded_bridge().await;
    let cap = b
        .run_with(
            r#"json.ok(db.fromJSON({
                 table: "t", verb: "select", columns: ["t.name"],
                 joins: [{ table: "u", kind: "left",
                           on: [{left: "t.id", right: "u.id"}],
                           tenant_id: "t2" }],
                 order_by: [{field: "t.id", dir: "asc"}] }).toSQL());"#,
            req_t1(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    // 基表 + join 两处绑定都必须是当前租户 t1（尾随 limit 参数不计）；"t2" 不得出现。
    assert_eq!(
        v["data"]["params"].as_array().unwrap().first(),
        Some(&json!("t1")),
        "{v}"
    );
    assert_eq!(
        v["data"]["params"].as_array().unwrap().get(1),
        Some(&json!("t1")),
        "{v}"
    );
    assert!(
        !v["data"]["sql"].as_str().unwrap().contains("t2"),
        "他租户值泄漏进 SQL: {v}"
    );
}

/// 子查询递归：in-subquery 同样收窄到当前租户。
#[tokio::test(flavor = "current_thread")]
async fn tenant_subquery_recursion() {
    let b = guarded_bridge().await;
    // 子查询查 t2 租户的行名 b1——注入后子查询只回 t1 行 → 外层 in 空集。
    let cap = b
        .run_with(
            r#"db.table("t").select(["name"])
                 .where({field:"name",op:"in",subquery:db.table("t").select(["name"]).where({field:"tenant_id",op:"eq",value:"t2"})})
                 .all()
                 .then(r => json.ok({n: r.length})).catch(e => json.fail(500, String(e)));"#,
            req_t1(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert_eq!(v["data"]["n"], 0, "子查询应被收窄到 t1，t2 行不可见: {v}");
}

/// tid=None：Deny 拒绝；asSystem 逃生口放行；Warn 模式放行。
#[tokio::test(flavor = "current_thread")]
async fn tenant_none_denied_and_as_system_escapes() {
    let b = guarded_bridge().await;
    // Deny + None → 拒
    let cap = b
        .run(
            r#"db.table("t").select(["name"]).all()
                 .then(r => json.ok({n: r.length})).catch(e => json.fail(400, String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 400, "{v}");
    assert!(
        v["msg"]
            .as_str()
            .unwrap()
            .contains("require tenant context"),
        "{v}"
    );
    // asSystem → 放行（看到全部 3 行）
    let cap = b
        .run(
            r#"db.asSystem().table("t").select(["name"]).all()
                 .then(r => json.ok({n: r.length})).catch(e => json.fail(400, String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!((&v["code"], &v["data"]["n"]), (&json!(0), &json!(3)), "{v}");
    // 共享表不受约束：None 也放行
    let cap = b
        .run(
            r#"db.table("s").select(["name"]).all()
                 .then(r => json.ok({n: r.length})).catch(e => json.fail(400, String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
}

/// 共享表（TableDef.shared=true）：有 tenant_id 列也不注入。
#[tokio::test(flavor = "current_thread")]
async fn tenant_shared_table_untouched() {
    let b = guarded_bridge().await;
    let cap = b
        .run_with(
            r#"json.ok(db.table("s").select(["name"]).toSQL());"#,
            req_t1(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert!(
        !v["data"]["sql"].as_str().unwrap().contains("tenant_id"),
        "{v}"
    );
}

/// 租户防护对事务同样生效：注入发生在 resolve_target 之前，与走池/走会话正交。
/// 事务内 select 收窄到当前租户、insert 强制写当前租户（含 returning）、
/// update 改不到他租户的行、回滚不留痕。
#[tokio::test(flavor = "current_thread")]
async fn tenant_guard_applies_inside_tx() {
    let b = guarded_bridge().await;
    let cap = b
        .run_with(
            r#"(async () => {
                 const out = await db.tx(async (tx) => {
                   const rows = await tx.table("t").select(["name"]).orderBy([{field:"id",dir:"asc"}]).all();
                   const r = await tx.table("t").insert({name:"a3"}).returning(["id"]).run();
                   return { names: rows.map(x => x.name), id: r[0].id };
                 });
                 const after = await db.table("t").select(["name","tenant_id"])
                   .where({field:"name",op:"eq",value:"a3"}).all();
                 json.ok({ out, tid: after.length ? after[0].tenant_id : null });
               })().catch(e => json.fail(500,String(e)));"#,
            req_t1(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    // t2 的 b1 在事务内同样不可见
    assert_eq!(v["data"]["out"]["names"], json!(["a1", "a2"]), "{v}");
    assert_eq!(v["data"]["tid"], json!("t1"), "insert 未注入租户: {v}");
    assert!(v["data"]["out"]["id"].as_i64().unwrap() > 0, "{v}");
    // 事务内 update 收窄 + 回滚不留痕
    let cap = b
        .run_with(
            r#"(async () => {
                 await db.tx(async (tx) => {
                   const n = await tx.table("t").update({name:"zz"}).where({field:"name",op:"eq",value:"b1"}).run();
                   if (n !== 0) throw new Error("leaked to other tenant");
                   await tx.table("t").insert({name:"gone"}).run();
                   throw new Error("boom");
                 }).catch(() => {});
                 const left = await db.table("t").select(["name"])
                   .where({field:"name",op:"eq",value:"gone"}).all();
                 const b1 = await db.asSystem().table("t").select(["name"])
                   .where({field:"name",op:"eq",value:"b1"}).all();
                 json.ok({ gone: left.length, b1: b1[0].name });
               })().catch(e => json.fail(500,String(e)));"#,
            req_t1(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(
        (&v["data"]["gone"], &v["data"]["b1"]),
        (&json!(0), &json!("b1")),
        "{v}"
    );
}

/// 含 LIKE 元字符的夹具：% 与 _ 是数据，不是通配符。
async fn seeded_bridge_like() -> Bridge {
    let db = SqlxAccessor::arc("sqlite::memory:").await.unwrap();
    db.exec_with_params("create table t (id integer primary key, name text)", &[])
        .await
        .unwrap();
    for n in ["a%b", "a_b", "abc", "xyz", "100%"] {
        db.exec_with_params("insert into t (name) values (?)", &[json!(n)])
            .await
            .unwrap();
    }
    let reg = SchemaRegistry::new().table("t", &["id"], &["id", "name"]);
    Bridge::with_opts(db, Arc::new(InMemoryKV::new()), reg, false)
}

/// `cond` 下按 id 升序取回的 name 列表。
async fn names_where(b: &Bridge, cond: &str) -> Value {
    let cap = b
        .run(&format!(
            r#"db.table("t").select(["name"]).where({cond}).orderBy([{{field:"id",dir:"asc"}}]).all()
               .then(r => json.ok({{ names: r.map(x => x.name) }}))
               .catch(e => json.fail(500, String(e)));"#
        ))
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "query failed: {v}");
    v["data"]["names"].clone()
}

/// 语义化模糊 op（v0.1.81）：值里的 % 与 _ 被当**字面量**，
/// 旧 `like` 直接把 `%` 当用户输入的通配符 → 数据里的 % 会多命中。
#[tokio::test(flavor = "current_thread")]
async fn semantic_like_ops_treat_wildcards_as_literals() {
    let b = seeded_bridge_like().await;
    // 关键一条：contains("a%") 只命中字面含 "a%" 的 a%b——
    // 未转义时 "%" 会被当通配符，把 abc / a_b 也捞进来。
    assert_eq!(
        names_where(&b, r#"{field:"name",op:"contains",value:"a%"}"#).await,
        json!(["a%b"]),
    );
    assert_eq!(
        names_where(&b, r#"{field:"name",op:"contains",value:"_b"}"#).await,
        json!(["a_b"]),
    );
    assert_eq!(
        names_where(&b, r#"{field:"name",op:"startswith",value:"a"}"#).await,
        json!(["a%b", "a_b", "abc"]),
    );
    assert_eq!(
        names_where(&b, r#"{field:"name",op:"endswith",value:"b"}"#).await,
        json!(["a%b", "a_b"]),
    );
    // 数据里的 `%`：字面结尾是 % 的只有 100% 一条。
    // 未转义时 `%%` 等于「以任意串结尾」→ 全表 5 条都被捞进来。
    assert_eq!(
        names_where(&b, r#"{field:"name",op:"endswith",value:"%"}"#).await,
        json!(["100%"]),
    );
    // 对照：裸 like 仍按用户给的 pattern 走（% 是通配符）。
    assert_eq!(
        names_where(&b, r#"{field:"name",op:"like",value:"a%"}"#).await,
        json!(["a%b", "a_b", "abc"]),
    );
}

/// 模糊 op 的 pattern 与 ESCAPE 符都进 params（绑定参数），
/// 与裸 like 一样不落进 SQL 文本——否则 `%` / `'` 就是拼接注入面。
#[tokio::test(flavor = "current_thread")]
async fn semantic_like_ops_bind_pattern_and_escape() {
    let b = seeded_bridge_like().await;
    let cap = b
        .run(
            r#"json.ok(db.table("t").select(["name"]).where({field:"name",op:"contains",value:"a%b"}).toSQL());"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    let sql = v["data"]["sql"].as_str().unwrap();
    let params = v["data"]["params"].as_array().unwrap();
    // 形状必须对：`col LIKE ? ESCAPE ?`（不是 `(col LIKE ?) ESCAPE ?`）。
    assert!(sql.contains("LIKE ? ESCAPE ?"), "{sql}");
    // 值本身绝不能出现在 SQL 文本里。
    assert!(!sql.contains("a%b"), "{sql}");
    assert_eq!(params[0], json!("%a\\%b%"), "{v}");
    assert_eq!(params[1], json!("\\"), "{v}");
}

/// escape 逃生舱：默认 `\` 与输入冲突时可换转义符（v0.1.81）。
#[tokio::test(flavor = "current_thread")]
async fn like_escape_char_is_overridable() {
    let b = seeded_bridge_like().await;
    let cap = b
        .run(
            r#"json.ok(db.table("t").select(["name"])
                 .where({field:"name",op:"contains",value:"a_b",escape:"!"}).toSQL());"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    let params = v["data"]["params"].as_array().unwrap();
    assert_eq!(params[0], json!("%a!_b%"), "{v}");
    assert_eq!(params[1], json!("!"), "{v}");
    // 裸 like 也认 escape：用户自带 pattern + 自定义转义符。
    let cap = b
        .run(
            r#"json.ok(db.table("t").select(["name"])
                 .where({field:"name",op:"like",value:"a!%b",escape:"!"}).toSQL());"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    let params = v["data"]["params"].as_array().unwrap();
    assert_eq!(params[0], json!("a!%b"), "{v}");
    assert_eq!(params[1], json!("!"), "{v}");
}

/// escape 必须是**单个**字符：多字符会静默截断成第一个，宁可报错。
#[tokio::test(flavor = "current_thread")]
async fn like_escape_rejects_non_single_char() {
    let b = seeded_bridge_like().await;
    for esc in [r#""""#, r#""ab""#] {
        let cap = b
            .run(&format!(
                r#"db.table("t").select(["name"])
                   .where({{field:"name",op:"contains",value:"a",escape:{esc}}})
                   .all().then(() => json.ok({{}}))
                   .catch(e => json.fail(500, String(e)));"#
            ))
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(
            v["msg"]
                .as_str()
                .unwrap()
                .contains("escape must be a single char"),
            "{esc}: {v}"
        );
    }
}

/// `in []` 必须报错：`is_in` 空集在 sea-query 里渲染成恒假 SQL，
/// 静默返回 0 行会伪装成「没有匹配数据」（v0.1.81）。
#[tokio::test(flavor = "current_thread")]
async fn empty_in_is_rejected() {
    let b = seeded_bridge().await;
    let cap = b
        .run(
            r#"db.table("t").select(["name"]).where({field:"age",op:"in",value:[]})
               .all().then(() => json.ok({}))
               .catch(e => json.fail(500, String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert!(
        v["msg"]
            .as_str()
            .unwrap()
            .contains("in needs non-empty array"),
        "{v}"
    );
}

/// `js` 是不含 `.then` 的链式片段，返回命中行数。
async fn rows_of(b: &Bridge, js: &str) -> u64 {
    let cap = b
        .run(&format!(
            r#"{js}.all().then(r => json.ok({{ n: r.length }})).catch(e => json.fail(500, String(e)));"#
        ))
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "query failed: {v}");
    v["data"]["n"].as_u64().unwrap()
}

/// `whereIf`：假值跳过、`0`/`""` 保留（`status=0` 是合法过滤值），
/// 且空数组跳过（避免 `IN ()`）。
#[tokio::test(flavor = "current_thread")]
async fn where_if_skips_only_nullish_and_empty_array() {
    let b = seeded_bridge().await;
    let base = r#"db.table("t").select(["name"])"#;
    // 跳过：undefined / null / false / []
    for skip in ["undefined", "null", "false"] {
        let js = format!(r#"{base}.whereIf({skip},{{field:"age",op:"lt",value:0}})"#);
        assert_eq!(rows_of(&b, &js).await, 4, "{js}");
    }
    assert_eq!(
        rows_of(
            &b,
            &format!(r#"{base}.whereIf([],{{field:"id",op:"in",value:[]}})"#)
        )
        .await,
        4
    );
    // 保留：0 / "" 是真过滤值（`status=0` 是合法的业务值）
    assert_eq!(
        rows_of(
            &b,
            &format!(r#"{base}.whereIf(0,{{field:"ok",op:"eq",value:0}})"#)
        )
        .await,
        2
    );
    assert_eq!(
        rows_of(
            &b,
            &format!(r#"{base}.whereIf("",{{field:"tag",op:"isnull"}})"#)
        )
        .await,
        1
    );
    // 非空数组照常拼
    assert_eq!(
        rows_of(
            &b,
            &format!(r#"{base}.whereIf([1],{{field:"id",op:"in",value:[1]}})"#)
        )
        .await,
        1
    );
}

/// `select(["*"])`：展开 registry 全列；与显式列混用 / 与 join 混用一律拒绝。
#[tokio::test(flavor = "current_thread")]
async fn select_star_expands_columns_and_rejects_mixing() {
    let b = seeded_bridge().await;
    let cap = b
        .run(
            r#"db.table("t").select(["*"]).orderBy([{field:"id",dir:"asc"}]).all()
               .then(r => json.ok({ first: r[0], keys: Object.keys(r[0]).sort() }))
               .catch(e => json.fail(500, String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert_eq!(
        v["data"]["keys"],
        json!(["age", "id", "name", "ok", "tag"]),
        "{v}"
    );
    assert_eq!(v["data"]["first"]["name"], json!("a"), "{v}");
    for (js, want) in [
        (
            r#"db.table("t").select(["*","id"]).all()"#,
            "select '*' must be the only column",
        ),
        (
            r#"db.table("t").select(["id","*"]).all()"#,
            "select '*' must be the only column",
        ),
    ] {
        let cap = b
            .run(&format!(
                r#"{js}.then(() => json.ok({{}})).catch(e => json.fail(500, String(e)));"#
            ))
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(v["msg"].as_str().unwrap().contains(want), "{want}: {v}");
    }
    // 与 join 混用：全列在两表同名列（id）上会歧义 → 直接拒绝。
    let b2 = seeded_bridge_2t().await;
    let cap = b2
        .run(
            r#"db.table("a").join("b",[{left:"a.id",right:"b.aid"}]).select(["*"]).all()
               .then(() => json.ok({})).catch(e => json.fail(500, String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert!(
        v["msg"]
            .as_str()
            .unwrap()
            .contains("select '*' is not allowed with joins"),
        "{v}"
    );
}

/// 表别名（v0.1.81）：FROM 渲染 `a AS x`，列限定用别名；
/// **归属守卫与租户判定仍按真名**——别名混进真名字段会静默绕过这两道防线。
#[tokio::test(flavor = "current_thread")]
async fn table_alias_renders_as_and_keeps_guards_on_real_name() {
    let b = seeded_bridge_2t().await;
    let cap = b
        .run(r#"json.ok(db.table("a").as("x").select(["x.name"]).toSQL());"#)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    let sql = v["data"]["sql"].as_str().unwrap();
    assert!(sql.contains("FROM"), "{sql}");
    assert!(sql.contains("AS"), "{sql}");
    assert!(sql.contains("AS \"x\""), "{sql}");
    // 真名仍被认：别名 ≠ 归属守卫的钥匙，列白名单按真名 a 解析。
    let cap = b
        .run(
            r#"db.table("a").as("x").select(["x.name"]).orderBy([{field:"name",dir:"asc"}]).all()
               .then(r => json.ok({ n: r.length, first: r[0].name }))
               .catch(e => json.fail(500, String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(
        (&v["code"], &v["data"]["n"], &v["data"]["first"]),
        (&json!(0), &json!(2), &json!("x")),
        "{v}"
    );
    // 自 join 靠别名区分：两侧真名都是 a，但 SQL 标识不同 → 放行。
    let cap = b
        .run(
            r#"json.ok(db.table("a").as("l").join("a",[{left:"l.id",right:"r.id"}],"inner","r")
                 .select(["l.name"]).toSQL());"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    let sql = v["data"]["sql"].as_str().unwrap();
    assert!(sql.contains("AS \"l\""), "{sql}");
    assert!(sql.contains("AS \"r\""), "{sql}");
}

/// 别名只属于 select：INSERT/UPDATE/DELETE 用别名会让列限定指向不存在的表。
#[tokio::test(flavor = "current_thread")]
async fn alias_rejected_on_write_verbs() {
    let b = seeded_bridge().await;
    for (js, want) in [
        (
            r#"db.table("t").as("x").update({name:"z"}).where({field:"id",op:"eq",value:1}).run()"#,
            "table alias is not supported on insert/update/delete",
        ),
        (
            r#"db.table("t").as("x").delete().where({field:"id",op:"eq",value:1}).run()"#,
            "table alias is not supported on insert/update/delete",
        ),
    ] {
        let cap = b
            .run(&format!(
                r#"{js}.then(() => json.ok({{}})).catch(e => json.fail(500, String(e)));"#
            ))
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(v["msg"].as_str().unwrap().contains(want), "{want}: {v}");
    }
}

/// join ON 的「列 op 值」（v0.1.81）：值进 params，非比较类 op 直接拒绝。
#[tokio::test(flavor = "current_thread")]
async fn join_on_value_condition_binds_and_rejects_bad_ops() {
    let b = seeded_bridge_2t().await;
    let cap = b
        .run(
            r#"json.ok(db.table("a").join("b",[{left:"b.aid",right:"a.id"},{left:"b.aid",op:"eq",value:1}])
                 .select(["a.name"]).toSQL());"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    let sql = v["data"]["sql"].as_str().unwrap();
    assert!(sql.contains("JOIN"), "{sql}");
    assert!(sql.contains("= ?"), "{sql}");
    assert!(
        v["data"]["params"].as_array().unwrap().contains(&json!(1)),
        "{v}"
    );
    for (js, want) in [
        (
            r#"db.table("a").join("b",[{left:"b.aid",op:"contains",value:"x"}]).select(["a.name"]).all()"#,
            "join on 'b.aid' does not support op 'contains'",
        ),
        (
            r#"db.table("a").join("b",[{left:"b.aid",op:"isnull"}]).select(["a.name"]).all()"#,
            "join on 'b.aid' does not support op 'isnull'",
        ),
        (
            r#"db.table("a").join("b",[{left:"b.aid",right:"a.id",op:"eq"}]).select(["a.name"]).all()"#,
            "takes either right or op/value, not both",
        ),
    ] {
        let cap = b
            .run(&format!(
                r#"{js}.then(() => json.ok({{}})).catch(e => json.fail(500, String(e)));"#
            ))
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(v["msg"].as_str().unwrap().contains(want), "{want}: {v}");
    }
}

/// `req` JSON 在三个方言下各自渲染（纯构造，不跑库）。
fn sql_in(reg: &SchemaRegistry, req: &str, d: Dialect) -> Result<String, JsErrorBox> {
    let req: QueryReq = serde_json::from_str(req).unwrap();
    build_statement(&req, reg, d).map(|(s, _)| s)
}

/// 空白归一：sea-query 固定拼 " ON CONFLICT " + " DO NOTHING"（空 target），
/// 断言按语义比对而不是按字节，才不会把上游的排版当接口契约。
fn norm_ws(s: String) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `lit` 字面列（v0.1.81）：值一律绑定参数，三方言的 CAST 形态各不相同——
/// PG 裸 `?` 会报「could not determine data type of parameter」，必须 CAST。
#[test]
fn lit_column_casts_per_dialect() {
    let reg = SchemaRegistry::new().table("t", &["id"], &["id", "name"]);
    let req = r#"{"table":"t","columns":["name",{"lit":"svc","as":"scope"}]}"#;
    let sq = sql_in(&reg, req, Dialect::Sqlite).unwrap();
    assert!(sq.contains("? AS \"scope\""), "{sq}");
    let my = sql_in(&reg, req, Dialect::MySql).unwrap();
    assert!(my.contains("CAST(? AS CHAR)"), "{my}");
    let pg = sql_in(&reg, req, Dialect::Postgres).unwrap();
    assert!(pg.contains("CAST($1 AS TEXT)"), "{pg}");
    // 值绝不进 SQL 文本。
    for s in [&sq, &my, &pg] {
        assert!(!s.contains("svc"), "{s}");
    }
    // 别名形状仍受既有别名校验约束。
    let bad = r#"{"table":"t","columns":[{"lit":"svc","as":"a-b"}]}"#;
    assert!(sql_in(&reg, bad, Dialect::Sqlite).is_err());
}

/// `orIgnore`（v0.1.81）：sqlite/pg 走 ON CONFLICT DO NOTHING；
/// mysql 无原生支持 → `DO NOTHING` 在 8.0.20+ 才认，旧版只认 DO UPDATE 恒等式。
#[test]
fn or_ignore_renders_per_dialect() {
    let reg = SchemaRegistry::new().table("t", &["id"], &["id", "name"]);
    let req = r#"{"table":"t","verb":"insert","values":[{"id":1,"name":"a"}],"or_ignore":true}"#;
    // 裸 DO NOTHING（不带 target）才会忽略**任意**唯一键冲突——写成
    // `ON CONFLICT ("id")` 只管 id，别处唯一键撞车照样报错（实测 sqlite 3.54）。
    for d in [Dialect::Sqlite, Dialect::Postgres] {
        let s = norm_ws(sql_in(&reg, req, d).unwrap());
        assert!(s.contains("ON CONFLICT DO NOTHING"), "{s}");
        assert!(!s.contains("ON CONFLICT ("), "{s}");
    }
    let my = sql_in(&reg, req, Dialect::MySql).unwrap();
    assert!(my.contains("ON DUPLICATE KEY UPDATE `id` = `id`"), "{my}");
    // mysql 无主键表：DO UPDATE 的恒等列无处可取 → 拒绝。
    let nopk = SchemaRegistry::new().table("t", &[], &["id", "name"]);
    let e = sql_in(
        &nopk,
        r#"{"table":"t","verb":"insert","values":[{"name":"a"}],"or_ignore":true}"#,
        Dialect::MySql,
    )
    .unwrap_err()
    .to_string();
    assert!(
        e.contains("orIgnore on mysql requires a primary key on table 't'"),
        "{e}"
    );
}

/// orIgnore + fromSelect 的方言差异（v0.1.81）：pg/mysql 都能挂上去，
/// sqlite 的 upsert 子句只能跟 VALUES——实测 `INSERT ... SELECT ... ON CONFLICT`
/// 报 `near "DO": syntax error`，加 target 也没用（sqlite 3.54 直接不支持）。
#[test]
fn or_ignore_with_from_select_renders_per_dialect() {
    let reg = SchemaRegistry::new()
        .table("t", &["id"], &["id", "name"])
        .table("s", &["id"], &["id", "name"]);
    let req = r#"{"table":"t","verb":"insert","from_select":{"table":"s","columns":["id","name"]},"insert_cols":["id","name"],"or_ignore":true}"#;
    let pg = norm_ws(sql_in(&reg, req, Dialect::Postgres).unwrap());
    assert!(pg.contains("SELECT"), "{pg}");
    assert!(pg.contains("ON CONFLICT DO NOTHING"), "{pg}");
    let my = sql_in(&reg, req, Dialect::MySql).unwrap();
    assert!(my.contains("SELECT"), "{my}");
    assert!(my.contains("ON DUPLICATE KEY UPDATE `id` = `id`"), "{my}");
    let e = sql_in(&reg, req, Dialect::Sqlite).unwrap_err().to_string();
    assert!(
        e.contains("orIgnore with fromSelect is not supported on sqlite"),
        "{e}"
    );
}

/// `fromSelect`（v0.1.81）：列数必须与源 select 一致，且与 values 互斥。
#[test]
fn from_select_checks_arity_and_exclusivity() {
    let reg = SchemaRegistry::new()
        .table("t", &["id"], &["id", "name"])
        .table("s", &["id"], &["id", "name"]);
    let src = r#"{"table":"s","columns":["id","name"]}"#;
    let ok = format!(
        r#"{{"table":"t","verb":"insert","from_select":{src},"insert_cols":["id","name"]}}"#
    );
    let s = sql_in(&reg, &ok, Dialect::Sqlite).unwrap();
    assert!(s.contains("INSERT INTO"), "{s}");
    // 声明 2 列、源产出 3 列（全列展开）→ 拒
    let bad =
        format!(r#"{{"table":"t","verb":"insert","from_select":{src},"insert_cols":["id"]}}"#);
    let e = sql_in(&reg, &bad, Dialect::Sqlite).unwrap_err().to_string();
    assert!(
        e.contains("declares 1 columns but the source select yields 2"),
        "{e}"
    );
    // 与 values 互斥 / 两者都缺
    let both = format!(
        r#"{{"table":"t","verb":"insert","values":[{{"id":1}}],"from_select":{src},"insert_cols":["id","name"]}}"#
    );
    let e = sql_in(&reg, &both, Dialect::Sqlite)
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("insert takes either values or fromSelect, not both"),
        "{e}"
    );
    let e = sql_in(&reg, r#"{"table":"t","verb":"insert"}"#, Dialect::Sqlite)
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("insert needs either values or fromSelect"),
        "{e}"
    );
}

/// orIgnore / fromSelect 是 insert 的尾部子句（v0.1.81）：挂在其他动词上
/// 无处可渲染 → 一律拒绝，而不是静默丢弃（用户会以为生效了）。
#[test]
fn insert_only_clauses_rejected_on_other_verbs() {
    let reg = SchemaRegistry::new().table("t", &["id"], &["id", "name"]);
    let bad = |req: &str| sql_in(&reg, req, Dialect::Sqlite).unwrap_err().to_string();
    for (req, want) in [
        (
            r#"{"table":"t","or_ignore":true}"#,
            "select does not accept orIgnore",
        ),
        (
            r#"{"table":"t","from_select":{"table":"t","columns":["name"]},"insert_cols":["name"]}"#,
            "select does not accept fromSelect",
        ),
        (
            r#"{"table":"t","verb":"update","sets":{"name":"a"},"conditions":[{"field":"id","op":"eq","value":1}],"or_ignore":true}"#,
            "update/delete does not accept orIgnore",
        ),
        (
            r#"{"table":"t","verb":"delete","conditions":[{"field":"id","op":"eq","value":1}],"from_select":{"table":"t","columns":["name"]}}"#,
            "update/delete does not accept fromSelect",
        ),
        (
            r#"{"table":"t","verb":"insert","values":[{"id":1}],"insert_cols":["id"]}"#,
            "insert columns are only allowed with fromSelect",
        ),
    ] {
        let e = bad(req);
        assert!(e.contains(want), "{want}: {e}");
    }
}

/// 源列必须点名（v0.1.81）：`*` / 省略都按 registry 的 HashMap 顺序摊列，
/// 而 INSERT...SELECT 按位置对齐 → 顺序不定等于列名串位。
#[test]
fn from_select_source_needs_explicit_columns() {
    let reg = SchemaRegistry::new()
        .table("t", &["id"], &["id", "name"])
        .table("s", &["id"], &["id", "name"]);
    let err = |req: &str| sql_in(&reg, req, Dialect::Sqlite).unwrap_err().to_string();
    let e = err(
        r#"{"table":"t","verb":"insert","from_select":{"table":"s"},"insert_cols":["id","name"]}"#,
    );
    assert!(
        e.contains("fromSelect source needs explicit columns"),
        "{e}"
    );
    let e = err(
        r#"{"table":"t","verb":"insert","from_select":{"table":"s","columns":["*"]},"insert_cols":["id","name"]}"#,
    );
    assert!(
        e.contains("fromSelect source needs explicit columns"),
        "{e}"
    );
}

/// fromSelect 端到端：真拷贝行；带上 orIgnore 后主键冲突行静默跳过（v0.1.81）。
#[tokio::test(flavor = "current_thread")]
async fn from_select_copies_rows_and_pairs_with_or_ignore() {
    let b = seeded_bridge().await;
    // 不带 id（自增）：4 → 8 行。
    let cap = b
        .run(
            r#"db.table("t").fromSelect(db.table("t").select(["name","age"]), ["name","age"]).run()
               .then(n => json.ok({ n }))
               .catch(e => json.fail(500, String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!((&v["code"], &v["data"]["n"]), (&json!(0), &json!(4)), "{v}");
    // 带 id：撞主键 → 裸 insert 报错（证明 orIgnore 确实在做事）。
    let cap = b
        .run(
            r#"db.table("t").fromSelect(db.table("t").select(["id","name"]), ["id","name"]).run()
               .then(n => json.ok({ n }))
               .catch(e => json.fail(500, String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 500, "{v}");
    // sqlite 上 orIgnore 与 fromSelect 不能共存 → 构造期就拒（不是把 sqlite 的
    // `near "DO": syntax error` 抛给用户）。
    let cap = b
        .run(
            r#"db.table("t").fromSelect(db.table("t").select(["id","name"]), ["id","name"]).orIgnore().run()
               .then(n => json.ok({ n }))
               .catch(e => json.fail(500, String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 500, "{v}");
    assert!(
        v["msg"]
            .as_str()
            .unwrap()
            .contains("orIgnore with fromSelect is not supported on sqlite"),
        "{v}"
    );
    // 失败的语句没写进任何行：仍是 8 行（4 原 + 4 副本）。
    let cap = b
        .run(
            r#"db.table("t").select(["name"]).all()
               .then(r => json.ok({ n: r.length }))
               .catch(e => json.fail(500, String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!((&v["code"], &v["data"]["n"]), (&json!(0), &json!(8)), "{v}");
}

/// 租户守卫对抗（v0.1.81）：fromSelect 是「读+写」两条路径合一——
/// 源 select 必须同样被租户收窄（否则把 b1 搬进自己表），
/// 目标行的 tenant_id 必须由守卫补常量列（否则落成无租户行）。
#[tokio::test(flavor = "current_thread")]
async fn from_select_is_tenant_narrowed_and_stamped() {
    let b = guarded_bridge().await;
    // SQL 形态：常量列盖章 + 源侧条件，两个租户参数。
    let cap = b
        .run_with(
            r#"json.ok(db.table("t").fromSelect(db.table("t").select(["name"]), ["name"]).toSQL());"#,
            req_tenant("t1"),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    let sql = v["data"]["sql"].as_str().unwrap();
    assert!(sql.contains("__oj_tenant"), "{sql}");
    assert!(sql.contains("tenant_id"), "{sql}");
    assert_eq!(v["data"]["params"], json!(["t1", "t1"]), "{v}");
    // 执行：只搬 t1 的两行，盖 t1 章。
    let cap = b
        .run_with(
            r#"db.table("t").fromSelect(db.table("t").select(["name"]), ["name"]).run()
               .then(n => json.ok({ n }))
               .catch(e => json.fail(500, String(e)));"#,
            req_tenant("t1"),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!((&v["code"], &v["data"]["n"]), (&json!(0), &json!(2)), "{v}");
    // 新行确实属于 t1（不是空租户的幽灵行）。
    let cap = b
        .run_with(
            r#"db.table("t").select(["name","tenant_id"]).where({field:"name",op:"eq",value:"a1"}).all()
               .then(r => json.ok({ n: r.length, tid: r[0].tenant_id }))
               .catch(e => json.fail(500, String(e)));"#,
            req_tenant("t1"),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(
        (&v["data"]["n"], &v["data"]["tid"]),
        (&json!(2), &json!("t1")),
        "{v}"
    );
    // b1 未被搬走：系统视角下仍是 4 行（a1,a2,b1 + 两行副本）。
    let cap = b
        .run_with(
            r#"db.asSystem().table("t").select(["name"]).all()
               .then(r => json.ok(r.map(x => x.name).sort()))
               .catch(e => json.fail(500, String(e)));"#,
            req_tenant("t1"),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"], json!(["a1", "a1", "a2", "a2", "b1"]), "{v}");
}

/// 攻击者自己塞 tenant_id 列（v0.1.81）：常量列会被盖掉一半，直接拒绝。
#[tokio::test(flavor = "current_thread")]
async fn from_select_rejects_declared_tenant_column() {
    let b = guarded_bridge().await;
    let cap = b
        .run_with(
            r#"db.table("t").fromSelect(db.table("t").select(["name"]), ["name","tenant_id"]).run()
               .then(n => json.ok({ n }))
               .catch(e => json.fail(500, String(e)));"#,
            req_tenant("t1"),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 500, "{v}");
    assert!(
        v["msg"]
            .as_str()
            .unwrap()
            .contains("insert fromSelect must not supply tenant_id"),
        "{v}"
    );
}

/// lit 列的三条边界（v0.1.81）：非标量值拒绝、`*` 混用拒绝、别名缺失拒绝。
/// 三条都不是「渲染出来能用就行」——它们是把 `{"a":1}` 串成文本那类静默垃圾挡在门外。
#[test]
fn lit_column_rejects_nonscalar_missing_alias_and_star_mixing() {
    let reg = SchemaRegistry::new().table("t", &["id"], &["id", "name"]);
    let err = |req: &str| sql_in(&reg, req, Dialect::Sqlite).unwrap_err().to_string();
    for (req, want) in [
        (
            r#"{"table":"t","columns":[{"lit":{"a":1},"as":"x"}]}"#,
            "lit column 'x' needs a scalar value",
        ),
        (
            r#"{"table":"t","columns":[{"lit":["a"],"as":"x"}]}"#,
            "lit column 'x' needs a scalar value",
        ),
    ] {
        let e = err(req);
        assert!(e.contains(want), "{want}: {e}");
    }
    // 缺 as 在反序列化层就被拦（LitSpec 要求别名齐备）。
    let e = serde_json::from_str::<QueryReq>(r#"{"table":"t","columns":[{"lit":1}]}"#)
        .unwrap_err()
        .to_string();
    assert!(e.contains("missing field `as`"), "{e}");
    // `*` 必须独占列清单（混入 lit 常量列同样算混用）。
    let e = err(r#"{"table":"t","columns":["*",{"lit":"s","as":"x"}]}"#);
    assert!(e.contains("select '*' must be the only column"), "{e}");
    // 标量四形态都放行（含 null）。
    for lit in ["1", "true", "null"] {
        let req = format!(r#"{{"table":"t","columns":[{{"lit":{lit},"as":"x"}}]}}"#);
        assert!(sql_in(&reg, &req, Dialect::Sqlite).is_ok(), "{req}");
    }
}

/// lit 常量列在嵌套层（子查询/CTE）同样成立：它只是列清单的一档形状。
#[tokio::test(flavor = "current_thread")]
async fn lit_column_works_inside_subquery_and_cte() {
    let b = seeded_bridge().await;
    let cap = b
        .run(
            r#"db.table("t").select(["name"]).where({field:"id",op:"in",subquery:
                   db.table("t").select([{lit:1,as:"k"}])}).all()
               .then(r => json.ok({ n: r.length }))
               .catch(e => json.fail(500, String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!((&v["code"], &v["data"]["n"]), (&json!(0), &json!(1)), "{v}");
    // CTE 内部带常量列。
    let cap = b
        .run(
            r#"db.table("t").select(["name"])
                 .with("c", ["name","k"], db.table("t").select(["name",{lit:1,as:"k"}]))
                 .all()
               .then(r => json.ok({ n: r.length }))
               .catch(e => json.fail(500, String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert!((v["code"] == 0), "{v}");
}

/// mysql orIgnore 的联合主键（v0.1.81）：恒等赋值按主键列逐个展开。
#[test]
fn or_ignore_renders_composite_primary_key_on_mysql() {
    let reg = SchemaRegistry::new().table("t", &["a", "b"], &["a", "b", "name"]);
    let req =
        r#"{"table":"t","verb":"insert","values":[{"a":1,"b":2,"name":"x"}],"or_ignore":true}"#;
    let my = sql_in(&reg, req, Dialect::MySql).unwrap();
    assert!(
        my.contains("ON DUPLICATE KEY UPDATE `a` = `a`, `b` = `b`"),
        "{my}"
    );
}

/// fromSelect 的目标列同样走白名单（v0.1.81）：写侧列名一律来自 registry。
#[test]
fn from_select_validates_target_columns() {
    let reg = SchemaRegistry::new()
        .table("t", &["id"], &["id", "name"])
        .table("s", &["id"], &["id", "name"]);
    let src = r#"{"table":"s","columns":["id","name"]}"#;
    for cols in [
        r#"["id","nope"]"#,
        r#"["s.id","name"]"#,
        r#"["id","name","extra"]"#,
    ] {
        let req =
            format!(r#"{{"table":"t","verb":"insert","from_select":{src},"insert_cols":{cols}}}"#);
        let e = sql_in(&reg, &req, Dialect::Sqlite).unwrap_err().to_string();
        assert!(e.contains("unknown column"), "{cols}: {e}");
    }
}

/// 快照往返（v0.1.81）：fromSelect / orIgnore 必须跟着 toJSON→fromJSON 走完，
/// 否则跨模块传递的请求会静默丢掉「忽略冲突」这个意图。
#[tokio::test(flavor = "current_thread")]
async fn insert_only_clauses_survive_snapshot_roundtrip() {
    let b = seeded_bridge().await;
    let cap = b
        .run(
            r#"json.ok(db.table("t").fromSelect(db.table("t").select(["id","name"]), ["id","name"]).orIgnore().toJSON());"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert_eq!(v["data"]["or_ignore"], true, "{v}");
    assert_eq!(v["data"]["insert_cols"], json!(["id", "name"]), "{v}");
    assert!(v["data"]["from_select"]["table"] == "t", "{v}");
    // sqlite 上这对组合本就不可用 → 重建后同样被拒（往返没把意图吞掉）。
    let cap = b
        .run(
            r#"const snap = db.table("t").insert({id:1,name:"a"}).orIgnore().toJSON();
               json.ok({ sql: db.fromJSON(snap).toSQL().sql, params: db.fromJSON(snap).toSQL().params });"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert!(
        norm_ws(v["data"]["sql"].as_str().unwrap().to_string()).contains("ON CONFLICT DO NOTHING"),
        "{v}"
    );
}

/// 端到端：lit / orIgnore 在真 sqlite 上跑通。
#[tokio::test(flavor = "current_thread")]
async fn lit_and_or_ignore_run_on_sqlite() {
    let b = seeded_bridge().await;
    let cap = b
        .run(
            r#"db.table("t").select(["name",{lit:"svc",as:"scope"}]).orderBy([{field:"id",dir:"asc"}]).all()
               .then(r => json.ok({ n: r.length, scope: r[0].scope }))
               .catch(e => json.fail(500, String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert_eq!(v["data"]["n"], 4, "{v}");
    assert_eq!(v["data"]["scope"], "svc", "{v}");
    // 幂等插入：主键冲突时静默跳过，返回 0 行受影响。
    let cap = b
        .run(
            r#"db.table("t").insert({id:1,name:"a"}).orIgnore().run()
               .then(n => json.ok({ n }))
               .catch(e => json.fail(500, String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert_eq!(v["data"]["n"], 0, "{v}");
}

/// toJSON → fromJSON 往返保留别名：快照是可信入口，别名不能被吞掉。
#[tokio::test(flavor = "current_thread")]
async fn alias_survives_snapshot_roundtrip() {
    let b = seeded_bridge_2t().await;
    let cap = b
        .run(
            r#"const snap = db.table("a").as("x").select(["x.name"]).toJSON();
               json.ok(db.fromJSON(snap).toSQL());"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert!(
        v["data"]["sql"].as_str().unwrap().contains("AS \"x\""),
        "{v}"
    );
}

// ----- v0.1.82：count(distinct) / truncate / listTables -----

/// 目录自省夹具：t 是唯一登记进 registry 的表，legacy_* / auto_t 模拟未登记表；
/// auto_t 用 AUTOINCREMENT 迫使 sqlite 建 sqlite_sequence 内部表——listTables 必须排除它。
async fn seeded_bridge_catalog() -> Bridge {
    let db = SqlxAccessor::arc("sqlite::memory:").await.unwrap();
    for ddl in [
        "create table t (id integer primary key, name text)",
        "create table legacy_a (id integer primary key)",
        "create table legacy_b (id integer primary key)",
        "create table auto_t (id integer primary key autoincrement, n text)",
    ] {
        db.exec_with_params(ddl, &[]).await.unwrap();
    }
    db.exec_with_params("insert into auto_t (n) values ('x')", &[])
        .await
        .unwrap();
    let reg = SchemaRegistry::new().table("t", &["id"], &["id", "name"]);
    Bridge::with_opts(db, Arc::new(InMemoryKV::new()), reg, false)
}

/// count(distinct col)：聚合一行返回、NULL 不计入（SQL 语义），不受 LIMIT 钳制——
/// 修掉「distinct().all() 拉行再 .length + limit 硬顶」的静默漏算形态（v0.1.82）。
#[tokio::test(flavor = "current_thread")]
async fn count_distinct_aggregate_counts_unique_non_null() {
    let b = seeded_bridge().await;
    // tag 列：x, y, x, NULL → 去重非空 = 2
    let cap = b
        .run(
            r#"db.table("t").select([{fn:"count",distinct:true,field:"tag",as:"c"}]).all()
               .then(r => json.ok(r));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert_eq!(v["data"], json!([{ "c": 2 }]), "{v}");
}

/// toSQL 形状：COUNT(DISTINCT 列)——列仍走白名单校验，值形态与普通聚合一致。
#[tokio::test(flavor = "current_thread")]
async fn count_distinct_tosql_renders_distinct() {
    let b = seeded_bridge().await;
    let cap = b
        .run(
            r#"json.ok(db.table("t").select([{fn:"count",distinct:true,field:"name",as:"c"}]).toSQL());"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    let sql = v["data"]["sql"].as_str().unwrap();
    assert!(sql.to_lowercase().contains("count(distinct"), "{sql}");
}

/// distinct 门禁：count 缺 field（含 `"*"`）/ 非 count 聚合带 distinct → 显式报错，
/// 不静默忽略（给了不生效的键等于用户以为生效了）。
#[tokio::test(flavor = "current_thread")]
async fn count_distinct_guards_reject_bad_shapes() {
    let b = seeded_bridge().await;
    for (js, msg) in [
        (
            r#"db.table("t").select([{fn:"count",distinct:true}]).all()"#,
            "count distinct needs a field",
        ),
        (
            r#"db.table("t").select([{fn:"count",distinct:true,field:"*"}]).all()"#,
            "count distinct needs a field",
        ),
        (
            r#"db.table("t").select([{fn:"sum",distinct:true,field:"age",as:"s"}]).all()"#,
            "distinct is only supported on count",
        ),
    ] {
        let cap = b
            .run(&format!("{js}.catch(e => json.fail(500, String(e)));"))
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 500, "{js} -> {v}");
        assert!(v["msg"].as_str().unwrap().contains(msg), "{js} -> {v}");
    }
}

/// truncate：无 where 的显式清空——JS 层不再抛 requires where，引擎渲染
/// `DELETE FROM t`（sqlite 无 TRUNCATE 语句，三方言以 DELETE 承载），返回受影响行数。
#[tokio::test(flavor = "current_thread")]
async fn truncate_clears_table_without_where() {
    let b = seeded_bridge().await;
    let cap = b
        .run(
            r#"db.table("t").truncate().run().then(n => json.ok({ n })).catch(e => json.fail(500, String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert_eq!(v["data"]["n"], 4, "{v}");
    // 清后计数归零（复用带 where 的 count 路径验证确实清了）
    assert_eq!(count_where(&b, r#"{field:"id",op:"gte",value:0}"#).await, 0);
}

/// truncate 门禁：自带 where 显式拒绝（带条件的清空用 delete）；verb=update 携带
/// truncate 旗标是矛盾语义，同样拒绝（经 fromJSON 直造 req——链式 truncate() 会覆写
/// verb，正常链路到不了这个状态）。
#[tokio::test(flavor = "current_thread")]
async fn truncate_rejects_where_and_update_verb() {
    let b = seeded_bridge().await;
    let cap = b
        .run(
            r#"db.table("t").truncate().where({field:"id",op:"gte",value:1}).run()
               .catch(e => json.fail(500, String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 500, "{v}");
    assert!(
        v["msg"]
            .as_str()
            .unwrap()
            .contains("truncate does not accept where"),
        "{v}"
    );

    let cap = b
        .run(
            r#"db.fromJSON({db:"default", table:"t", verb:"update", truncate:true, sets:{ok:1}}).run()
               .catch(e => json.fail(500, String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 500, "{v}");
    assert!(
        v["msg"]
            .as_str()
            .unwrap()
            .contains("update does not accept truncate"),
        "{v}"
    );
}

/// listTables：方言无关目录自省——未登记 registry 的表也列出（api-only 方案 /
/// 历史遗留表），sqlite 内部表（sqlite_sequence）排除，按名称排序。
#[tokio::test(flavor = "current_thread")]
async fn list_tables_lists_registry_unknown_tables_sorted() {
    let b = seeded_bridge_catalog().await;
    let cap = b
        .run(
            r#"db.listTables().then(ts => json.ok({ ts })).catch(e => json.fail(500, String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert_eq!(
        v["data"]["ts"],
        json!(["auto_t", "legacy_a", "legacy_b", "t"]),
        "{v}"
    );
}

/// listTables(prefix) 助手：跑 JS 并断言信封 code=0，返回表名数组。
async fn list_tables_of(b: &Bridge, p: &str) -> Vec<Value> {
    let js = format!(
        r#"db.listTables({p:?}).then(ts => json.ok({{ ts }})).catch(e => json.fail(500, String(e)));"#
    );
    let cap = b.run(&js).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{p} -> {v}");
    v["data"]["ts"].as_array().unwrap().clone()
}

/// 前缀过滤：绑定参数 + `|` 转义——前缀里的 % / _ 按字面量匹配，绝不泄漏成通配符
/// （若转义被移除，"leg%" 会 LIKE 匹配出全部 legacy_* 表——变异体必死）。
#[tokio::test(flavor = "current_thread")]
async fn list_tables_prefix_escapes_wildcards() {
    let b = seeded_bridge_catalog().await;
    assert_eq!(
        json!(list_tables_of(&b, "leg").await),
        json!(["legacy_a", "legacy_b"])
    );
    assert_eq!(json!(list_tables_of(&b, "t").await), json!(["t"]));
    // % / _ 必须是字面量：不匹配任何表
    assert!(
        list_tables_of(&b, "leg%").await.is_empty(),
        "wildcard % leaked"
    );
    assert!(
        list_tables_of(&b, "l_g").await.is_empty(),
        "wildcard _ leaked"
    );
    assert!(
        list_tables_of(&b, "sqlite_").await.is_empty(),
        "internal tables leaked"
    );
}
