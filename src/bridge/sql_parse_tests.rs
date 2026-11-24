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

/// S2：方言别名归一 + 未知/缺省/非串方言的结构化错误（supported 列表在场）。
#[tokio::test(flavor = "current_thread")]
async fn sql_parse_dialect_alias_normalized_and_unknown_rejected() {
    let (b, _) = new_bridge();
    let cap = b
        .run_with(
            r#"const ok = sql.parse("select 1", { dialect: "pg" });
               const ms = sql.parse("select 1", { dialect: "mssql" });
               let caught = null;
               try { sql.parse("select 1", { dialect: "bogus" }); } catch (e) { caught = e; }
               let missing = null;
               try { sql.parse("select 1"); } catch (e) { missing = e; }
               let nonString = null;
               try { sql.parse("select 1", { dialect: 42 }); } catch (e) { nonString = e; }
               json.ok({
                 dialect: ok.dialect,
                 mssqlDialect: ms.dialect,
                 unkReason: caught && caught.reason,
                 unkCode: caught && caught.code,
                 unkSupportedHasPg: caught && caught.supported && caught.supported.includes("postgresql"),
                 missReason: missing && missing.reason,
                 missSupported: missing && missing.supported && missing.supported.length,
                 nonStringReason: nonString && nonString.reason,
                 nonStringDialectType: nonString && nonString.dialect,
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
    assert_eq!(
        v["data"]["mssqlDialect"], "transactsql",
        "mssql alias must normalize to transactsql: {v}"
    );
    assert_eq!(v["data"]["unkCode"], "OJ_SQL_PARSE");
    assert_eq!(v["data"]["unkReason"], "dialect");
    assert_eq!(v["data"]["unkSupportedHasPg"], true);
    assert_eq!(v["data"]["missReason"], "dialect");
    assert!(v["data"]["missSupported"].as_u64().unwrap() >= 14);
    assert_eq!(
        v["data"]["nonStringReason"], "dialect",
        "non-string dialect must be a structured dialect error: {v}"
    );
    assert_eq!(v["data"]["nonStringDialectType"], "number");
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

/// S1（golden 全量锚）：spec 原例的列级血缘——CTE 穿透一层、别名解析正确。
#[tokio::test(flavor = "current_thread")]
async fn sql_parse_golden_lineage_spec_example() {
    let (b, _) = new_bridge();
    let cap = b
        .run_with(
            r#"const r = sql.parse("WITH recent AS (SELECT order_id, amount FROM orders WHERE created_at > '2026-01-01') SELECT r.order_id, c.name FROM recent r JOIN customers c ON r.order_id = c.order_id", { dialect: "postgresql" });
               json.ok({ lineage: r.lineage });"#,
            RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(
        v["data"]["lineage"],
        json!([
            {"output": {"table": "recent", "column": "order_id"},
             "sources": [{"table": "orders", "column": "order_id"}]},
            {"output": {"table": "customers", "column": "name"},
             "sources": [{"table": "customers", "column": "name"}]},
        ]),
        "spec golden lineage (CTE pass-through + alias resolution): {v}"
    );
}

/// S3：FROM 派生子查询穿透一层（匿名 CTE 同机制）；output.table 无名源为 null；
/// 内层基表出现在 tables。
#[tokio::test(flavor = "current_thread")]
async fn sql_parse_lineage_from_subquery_pass_through() {
    let (b, _) = new_bridge();
    let cap = b
        .run_with(
            r#"const r = sql.parse("SELECT x.id FROM (SELECT id FROM orders) x WHERE x.id > 1", { dialect: "postgresql" });
               json.ok({ lineage: r.lineage, tables: r.tables });"#,
            RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(
        v["data"]["lineage"],
        json!([
            {"output": {"table": null, "column": "id"},
             "sources": [{"table": "orders", "column": "id"}]},
        ]),
        "derived-subquery alias resolves one level to the inner base table: {v}"
    );
    assert_eq!(
        v["data"]["tables"],
        json!([{"name": "orders", "schema": null, "alias": null}]),
        "inner base table must appear in tables: {v}"
    );
}

/// S3b：链式 CTE 单层边界——b→a（CTE 引 CTE）停在 a 名上，不递归第二层。
#[tokio::test(flavor = "current_thread")]
async fn sql_parse_lineage_chained_cte_stops_at_boundary() {
    let (b, _) = new_bridge();
    let cap = b
        .run_with(
            r#"const r = sql.parse("WITH a AS (SELECT id FROM t1), b AS (SELECT id FROM a) SELECT b.id FROM b", { dialect: "postgresql" });
               json.ok({ lineage: r.lineage });"#,
            RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(
        v["data"]["lineage"],
        json!([
            {"output": {"table": "b", "column": "id"},
             "sources": [{"table": "a", "column": "id"}]},
        ]),
        "chained CTE resolves one level and stops at the boundary CTE name: {v}"
    );
}

/// S4：DDL 语义——postgresql 与 snowflake CREATE TABLE 均解析，columns/lineage 空。
#[tokio::test(flavor = "current_thread")]
async fn sql_parse_ddl_multi_dialect_empty_lineage() {
    let (b, _) = new_bridge();
    let cap = b
        .run_with(
            r#"const pg = sql.parse("CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR(50))", { dialect: "postgresql" });
               const sf = sql.parse("CREATE TABLE t (id INT)", { dialect: "snowflake" });
               json.ok({
                 pgTables: pg.tables, pgLineage: pg.lineage, pgColumns: pg.columns,
                 sfType: sf.ast.type, sfTables: sf.tables, sfLineage: sf.lineage,
               });"#,
            RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(
        v["data"]["pgTables"],
        json!([{"name": "t", "schema": null, "alias": null}]),
        "CREATE TABLE target lands in tables: {v}"
    );
    assert_eq!(
        v["data"]["pgLineage"],
        json!([]),
        "DDL lineage is empty: {v}"
    );
    assert_eq!(
        v["data"]["pgColumns"],
        json!([]),
        "DDL columns are empty: {v}"
    );
    assert_eq!(v["data"]["sfType"], "create");
    assert_eq!(
        v["data"]["sfTables"],
        json!([{"name": "t", "schema": null, "alias": null}])
    );
    assert_eq!(v["data"]["sfLineage"], json!([]));
}

/// S9：中文标识符逐字保留（引号标识符，postgresql 方言）。
#[tokio::test(flavor = "current_thread")]
async fn sql_parse_unicode_identifiers_verbatim() {
    let (b, _) = new_bridge();
    let cap = b
        .run_with(
            r#"const r = sql.parse("SELECT \"订单号\" FROM \"订单表\" WHERE \"客户\" = '张三'", { dialect: "postgresql" });
               json.ok({ columns: r.columns, lineage: r.lineage, tables: r.tables });"#,
            RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(
        v["data"]["columns"],
        json!([{"table": "订单表", "column": "订单号"}, {"table": "订单表", "column": "客户"}]),
        "unicode identifiers kept verbatim (quotes stripped): {v}"
    );
    assert_eq!(
        v["data"]["lineage"],
        json!([
            {"output": {"table": "订单表", "column": "订单号"},
             "sources": [{"table": "订单表", "column": "订单号"}]},
        ]),
        "lineage names verbatim: {v}"
    );
    assert_eq!(
        v["data"]["tables"],
        json!([{"name": "订单表", "schema": null, "alias": null}])
    );
}

/// S6：幂等无副作用——同输入连调两次结果 deep-equal。
#[tokio::test(flavor = "current_thread")]
async fn sql_parse_idempotent_same_input_deep_equal() {
    let (b, _) = new_bridge();
    let cap = b
        .run_with(
            r#"const s = "WITH r AS (SELECT id FROM t) SELECT r.id FROM r";
               const a = JSON.stringify(sql.parse(s, { dialect: "postgresql" }));
               const c = JSON.stringify(sql.parse(s, { dialect: "postgresql" }));
               json.ok({ equal: a === c, len: a.length });"#,
            RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["equal"], true);
    assert!(
        v["data"]["len"].as_u64().unwrap() > 100,
        "result must carry real content"
    );
}

/// S7：预算——>1MiB 输入在解析前拒绝（budget），快速返回。
#[tokio::test(flavor = "current_thread")]
async fn sql_parse_budget_rejects_oversized_input() {
    let (b, _) = new_bridge();
    let cap = b
        .run_with(
            r#"let caught = null;
               try { sql.parse("SELECT " + "x".repeat(1048577), { dialect: "postgresql" }); } catch (e) { caught = e; }
               json.ok({
                 reason: caught && caught.reason,
                 code: caught && caught.code,
                 max: caught && caught.max,
               });"#,
            RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["code"], "OJ_SQL_PARSE");
    assert_eq!(v["data"]["reason"], "budget");
    assert_eq!(v["data"]["max"], 1048576);
}

/// S10：深嵌套不崩——千层括号映射为结构化错误（syntax 或 engine），
/// 同 runtime 后续请求仍可用（进程存活）。
#[tokio::test(flavor = "current_thread")]
async fn sql_parse_deep_nesting_structured_error_and_survivor() {
    let (b, _) = new_bridge();
    let cap = b
        .run_with(
            r#"const deep = "SELECT " + "(".repeat(2000) + "1" + ")".repeat(2000);
               let caught = null;
               try { sql.parse(deep, { dialect: "postgresql" }); } catch (e) {
                 caught = { code: e.code, reason: e.reason || "(none)" };
               }
               // survivor probe: same runtime still serves normal parses
               const ok = sql.parse("select 1", { dialect: "postgresql" });
               json.ok({ caught, survivor: ok.ast.type });"#,
            RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(
        v["data"]["survivor"], "select",
        "runtime must survive deep-nest failure: {v}"
    );
    let reason = v["data"]["caught"]["reason"].as_str().unwrap_or("");
    assert!(
        reason == "syntax" || reason == "engine",
        "deep nesting must surface as a STRUCTURED error (syntax|engine), got: {v}"
    );
    assert_eq!(v["data"]["caught"]["code"], "OJ_SQL_PARSE");
}

/// Review Focus 3 + 多语句：空串/纯注释/非字符串 → input；多语句 → syntax + statements。
#[tokio::test(flavor = "current_thread")]
async fn sql_parse_input_edge_cases() {
    let (b, _) = new_bridge();
    let cap = b
        .run_with(
            r#"const grab = (fn) => { try { fn(); return null; } catch (e) { return { reason: e.reason, code: e.code, statements: e.statements }; } };
               json.ok({
                 empty: grab(() => sql.parse("", { dialect: "postgresql" })),
                 blank: grab(() => sql.parse("   ", { dialect: "postgresql" })),
                 commentOnly: grab(() => sql.parse("-- nothing here", { dialect: "postgresql" })),
                 nonString: grab(() => sql.parse(42, { dialect: "postgresql" })),
                 nonStringObj: grab(() => sql.parse({ sql: "select 1" }, { dialect: "postgresql" })),
                 multi: grab(() => sql.parse("select 1; select 2", { dialect: "postgresql" })),
               });"#,
            RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    for k in ["empty", "blank", "nonString", "nonStringObj"] {
        assert_eq!(
            v["data"][k]["code"], "OJ_SQL_PARSE",
            "{k} must be structured: {v}"
        );
        assert_eq!(
            v["data"][k]["reason"], "input",
            "{k} must be reason=input: {v}"
        );
    }
    assert_eq!(
        v["data"]["commentOnly"]["code"], "OJ_SQL_PARSE",
        "comment-only must not be silent: {v}"
    );
    // Comment-only parses to an EMPTY statement list -> the multi-statement
    // rejection path (reason syntax + statements:0), not input (docs pin
    // input to non-string/blank only).
    assert_eq!(
        v["data"]["commentOnly"]["reason"], "syntax",
        "comment-only must be reason=syntax (empty statement list): {v}"
    );
    assert_eq!(
        v["data"]["commentOnly"]["statements"], 0,
        "comment-only reports zero statements: {v}"
    );
    assert_eq!(
        v["data"]["multi"]["reason"], "syntax",
        "multi-statement must be rejected: {v}"
    );
    assert_eq!(
        v["data"]["multi"]["statements"], 2,
        "multi-statement error must carry the statements count: {v}"
    );
}

/// Review Focus 2：SQL 内注释透明 + 引号标识符（mysql 反引号）逐字。
#[tokio::test(flavor = "current_thread")]
async fn sql_parse_comments_transparent_and_backtick_identifiers() {
    let (b, _) = new_bridge();
    let cap = b
        .run_with(
            r#"const c = sql.parse("SELECT id -- trailing comment\n/* block */ FROM users", { dialect: "mysql" });
               const q = sql.parse("SELECT `订单号` FROM `订单表`", { dialect: "mysql" });
               json.ok({ cColumns: c.columns, cTables: c.tables, qColumns: q.columns, qTables: q.tables });"#,
            RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(
        v["data"]["cColumns"],
        json!([{"table": "users", "column": "id"}]),
        "comments must be transparent to extraction: {v}"
    );
    assert_eq!(
        v["data"]["cTables"],
        json!([{"name": "users", "schema": null, "alias": null}])
    );
    assert_eq!(
        v["data"]["qColumns"],
        json!([{"table": "订单表", "column": "订单号"}]),
        "backtick-quoted identifiers verbatim (quotes stripped): {v}"
    );
    assert_eq!(
        v["data"]["qTables"],
        json!([{"name": "订单表", "schema": null, "alias": null}])
    );
}

/// 评审收口（架构师 #6 / 工程师 #1 must）：自连接两个别名都进 tables，别名限定列
/// 各归其主，裸列在双源作用域诚实降级 null（不再塌缩成单源误归属）。
#[tokio::test(flavor = "current_thread")]
async fn sql_parse_self_join_keeps_both_aliases() {
    let (b, _) = new_bridge();
    let cap = b
        .run_with(
            r#"const r = sql.parse("SELECT id, a.mgr, b.name FROM emp a JOIN emp b ON a.mgr = b.id", { dialect: "mysql" });
               json.ok({ tables: r.tables, columns: r.columns });"#,
            RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(
        v["data"]["tables"],
        json!([
            {"name": "emp", "schema": null, "alias": "a"},
            {"name": "emp", "schema": null, "alias": "b"},
        ]),
        "self-join must keep BOTH alias entries: {v}"
    );
    assert_eq!(
        v["data"]["columns"],
        json!([
            {"table": null, "column": "id"},
            {"table": "emp", "column": "mgr"},
            {"table": "emp", "column": "name"},
            {"table": "emp", "column": "id"},
        ]),
        "qualified refs resolve via their own alias (ON-clause b.id included); bare id degrades honestly: {v}"
    );
}

/// 评审收口（工程师 #2）：三段限定列（a.b.c 的 column_ref 自带 db/table 形状）
/// 不得登记成幻影表。
#[tokio::test(flavor = "current_thread")]
async fn sql_parse_three_part_column_ref_no_phantom_table() {
    let (b, _) = new_bridge();
    let cap = b
        .run_with(
            r#"const r = sql.parse("SELECT a.b.c FROM t", { dialect: "postgresql" });
               json.ok({ tables: r.tables, columns: r.columns });"#,
            RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(
        v["data"]["tables"],
        json!([{"name": "t", "schema": null, "alias": null}]),
        "a.b.c column ref must not register phantom table b: {v}"
    );
    assert_eq!(
        v["data"]["columns"],
        json!([{"table": null, "column": "c"}]),
        "qualifier b is unresolved in this scope (honest null): {v}"
    );
}

/// 评审收口（架构师 #7 / 工程师 #3）：WITH 收集绑定语句级——子查询/CTE 体内声明的
/// WITH 不泄漏进顶层 ctes，也不遮蔽同名基表。
#[tokio::test(flavor = "current_thread")]
async fn sql_parse_nested_with_bounded_to_statement() {
    let (b, _) = new_bridge();
    let cap = b
        .run_with(
            r#"const r = sql.parse("WITH z AS (WITH b AS (SELECT 1 AS x) SELECT x FROM b) SELECT id FROM b", { dialect: "postgresql" });
               json.ok({ ctes: r.ctes, tables: r.tables, lineage: r.lineage });"#,
            RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(
        v["data"]["ctes"],
        json!(["z"]),
        "only the statement-level WITH (z) is listed; inner b must not leak: {v}"
    );
    assert_eq!(
        v["data"]["tables"],
        json!([{"name": "b", "schema": null, "alias": null}]),
        "inner CTE name must not mask the same-named base table: {v}"
    );
}

/// 评审收口（工程师 #4 文档对齐 + 测试工程师 should-pin）：SELECT * 的 output.column
/// 恒为 "*"（nsp 以 column_ref 形态呈现 star），含全列语义。
#[tokio::test(flavor = "current_thread")]
async fn sql_parse_star_lineage_shape() {
    let (b, _) = new_bridge();
    let cap = b
        .run_with(
            r#"const r = sql.parse("SELECT * FROM users", { dialect: "mysql" });
               json.ok({ lineage: r.lineage });"#,
            RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(
        v["data"]["lineage"],
        json!([
            {"output": {"table": "users", "column": "*"},
             "sources": [{"table": "users", "column": "*"}]},
        ]),
        "SELECT * renders column:\"*\" (all-columns), table from the single source: {v}"
    );
}

/// 评审收口（测试工程师 should-pin + api-manual:1702 契约）：insert/update 目标表
/// 与 create 一样进 tables。
#[tokio::test(flavor = "current_thread")]
async fn sql_parse_write_targets_in_tables() {
    let (b, _) = new_bridge();
    let cap = b
        .run_with(
            r#"const ins = sql.parse("INSERT INTO audit_log (id, note) VALUES (1, 'x')", { dialect: "mysql" });
               const upd = sql.parse("UPDATE audit_log SET note = 'y' WHERE id = 1", { dialect: "mysql" });
               json.ok({ insTables: ins.tables, updTables: upd.tables });"#,
            RequestInfo::default(),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    let expected = json!([{"name": "audit_log", "schema": null, "alias": null}]);
    assert_eq!(
        v["data"]["insTables"], expected,
        "INSERT target lands in tables: {v}"
    );
    assert_eq!(
        v["data"]["updTables"], expected,
        "UPDATE target lands in tables: {v}"
    );
}
