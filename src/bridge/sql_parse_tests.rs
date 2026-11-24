//! `sql.parse` 平台 SQL 解析服务测试（REQ-PARSE Phase 2，v0.1.143）。
//! harness 样板同 mod_tests.rs：Bridge::new + run_with（纯 JS 面，零 db 依赖）。
//! 契约与解析口径：docs/superpowers/plans/2026-11-24-upstream-reqs-phase2-sql-parse.md。

use super::*;
use serde_json::{Value, json};

fn new_bridge() -> (Bridge, Arc<InMemoryAccessor>) {
    let db = Arc::new(InMemoryAccessor::new());
    let b = Bridge::new(db.clone(), Arc::new(InMemoryKV::new()));
    (b, db)
}

/// S2：方言别名归一 + 未知/缺省方言的结构化错误（supported 列表在场）。
#[tokio::test(flavor = "current_thread")]
async fn sql_parse_dialect_alias_normalized_and_unknown_rejected() {
    let (b, _) = new_bridge();
    let cap = b
        .run_with(
            r#"const ok = sql.parse("select 1", { dialect: "pg" });
               let caught = null;
               try { sql.parse("select 1", { dialect: "bogus" }); } catch (e) { caught = e; }
               let missing = null;
               try { sql.parse("select 1"); } catch (e) { missing = e; }
               json.ok({
                 dialect: ok.dialect,
                 unkReason: caught && caught.reason,
                 unkCode: caught && caught.code,
                 unkSupportedHasPg: caught && caught.supported && caught.supported.includes("postgresql"),
                 missReason: missing && missing.reason,
                 missSupported: missing && missing.supported && missing.supported.length,
               });"#,
            RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(
        v["data"]["dialect"], "postgresql",
        "pg alias must normalize: {v}"
    );
    assert_eq!(v["data"]["unkCode"], "OJ_SQL_PARSE");
    assert_eq!(v["data"]["unkReason"], "dialect");
    assert_eq!(v["data"]["unkSupportedHasPg"], true);
    assert_eq!(v["data"]["missReason"], "dialect");
    assert!(v["data"]["missSupported"].as_u64().unwrap() >= 14);
}

/// S5：语法垃圾 → 结构化错误（code/reason/location 在场），非静默。
#[tokio::test(flavor = "current_thread")]
async fn sql_parse_garbage_returns_structured_syntax_error() {
    let (b, _) = new_bridge();
    let cap = b
        .run_with(
            r#"let caught = null;
               try { sql.parse("SELECT FROM WHERE", { dialect: "postgresql" }); } catch (e) { caught = e; }
               json.ok({
                 code: caught && caught.code,
                 reason: caught && caught.reason,
                 hasLocation: !!(caught && caught.location && caught.location.start && caught.location.start.line),
                 line: caught && caught.location && caught.location.start.line,
               });"#,
            RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["code"], "OJ_SQL_PARSE");
    assert_eq!(v["data"]["reason"], "syntax");
    assert_eq!(v["data"]["hasLocation"], true);
    assert_eq!(v["data"]["line"], 1);
}

/// S8：裸列解析——单表归属；join 作用域下裸列诚实降级 table:null。
#[tokio::test(flavor = "current_thread")]
async fn sql_parse_bare_column_resolution() {
    let (b, _) = new_bridge();
    let cap = b
        .run_with(
            r#"const single = sql.parse("SELECT id, name FROM users", { dialect: "postgresql" });
               const join = sql.parse("SELECT id, c.name FROM users u JOIN customers c ON u.cid = c.id", { dialect: "postgresql" });
               json.ok({ single: single.columns, join: join.columns });"#,
            RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(
        v["data"]["single"],
        json!([{"table": "users", "column": "id"}, {"table": "users", "column": "name"}]),
        "single-table bare columns must resolve: {v}"
    );
    // ON-clause column refs are referenced columns too (u.cid / c.id); the
    // bare `id` in a join scope degrades to table:null (ambiguous).
    assert_eq!(
        v["data"]["join"],
        json!([
            {"table": null, "column": "id"},
            {"table": "customers", "column": "name"},
            {"table": "users", "column": "cid"},
            {"table": "customers", "column": "id"},
        ]),
        "ambiguous bare column degrades to table:null; ON refs are included: {v}"
    );
}

/// S1（partial，lineage 留 Task 3）：tables 恰为基表、ctes 在列、dialect/ast 归位。
#[tokio::test(flavor = "current_thread")]
async fn sql_parse_golden_tables_columns_ctes() {
    let (b, _) = new_bridge();
    let cap = b
        .run_with(
            r#"const r = sql.parse("WITH recent AS (SELECT order_id, amount FROM orders WHERE created_at > '2026-01-01') SELECT r.order_id, c.name FROM recent r JOIN customers c ON r.order_id = c.order_id", { dialect: "postgresql" });
               json.ok({ tables: r.tables, ctes: r.ctes, columns: r.columns, dialect: r.dialect, astType: r.ast.type, hasLineageField: Array.isArray(r.lineage) });"#,
            RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(
        v["data"]["tables"],
        json!([{"name": "orders", "schema": null, "alias": null}, {"name": "customers", "schema": null, "alias": "c"}]),
        "CTE names must NOT appear in tables; base-table aliases are preserved: {v}"
    );
    assert_eq!(v["data"]["ctes"], json!(["recent"]));
    assert_eq!(v["data"]["dialect"], "postgresql");
    assert_eq!(v["data"]["astType"], "select");
    assert_eq!(v["data"]["hasLineageField"], true);
}

/// Task 1 spike：懒加载 vendored 包 + 裸 AST 透传。postgresql 方言解析 CTE 语句。
#[tokio::test(flavor = "current_thread")]
async fn sql_parse_returns_raw_ast_type() {
    let (b, _) = new_bridge();
    let cap = b
        .run_with(
            r#"json.ok(sql.parse("WITH r AS (SELECT id FROM t) SELECT r.id FROM r", { dialect: "postgresql" }).ast.type);"#,
            RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"], "select", "ast.type must be \"select\", got: {v}");
}

/// Task 1 spike（续）：同一 runtime 二次调用命中 JS 模块缓存（loadExtScript 只拉一次）。
#[tokio::test(flavor = "current_thread")]
async fn sql_parse_reuses_lazy_module_on_second_call() {
    let (b, _) = new_bridge();
    let cap = b
        .run_with(
            r#"const a = sql.parse("select 1", { dialect: "mysql" }).ast;
               const b = sql.parse("select 2", { dialect: "mysql" }).ast;
               json.ok({ t1: a.type, t2: b.type });"#,
            RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["t1"], "select");
    assert_eq!(v["data"]["t2"], "select");
}
