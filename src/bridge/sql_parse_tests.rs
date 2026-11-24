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
