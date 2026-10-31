use super::*;

/// **PG 常量的 `::text` 静态守卫**（CI 无 PG，这是唯一离线防线）。
///
/// 只查**投影列**（`<ident> … AS …`）：比较条件里的 `relname = column_name` 是 name 与
/// `sql_identifier` 的比较，不需要 cast（真 PG 已验）。
#[test]
fn pg_constants_cast_identifiers() {
    for kind in [Kind::Columns, Kind::Indexes, Kind::ForeignKeys] {
        let s = sql(kind, Dialect::Postgres);
        for id in ["attname", "relname", "conname"] {
            let bare = String::from(id) + " AS";
            let cast = String::from(id) + "::text";
            assert!(
                !s.contains(&bare) || s.contains(&cast),
                "{kind:?}: PG 投影列 {id} 缺 ::text（sqlx Any 驱动解不了 name 域，\
                     且空库不触发、建表后才崩）"
            );
        }
    }
}

/// 内省 SQL 是**等值匹配**，不是 LIKE：表名里的 `%` / `_` 不得被当通配符。
#[test]
fn constants_use_equality_not_like() {
    for kind in [Kind::Columns, Kind::Indexes, Kind::ForeignKeys] {
        for d in [Dialect::Sqlite, Dialect::MySql, Dialect::Postgres] {
            let s = sql(kind, d);
            assert!(!s.contains("LIKE"), "{kind:?}/{d:?} 不该用 LIKE: {s}");
            assert_eq!(s.matches('?').count(), 1, "{kind:?}/{d:?} 参数位须恰为 1");
        }
    }
}

/// `key_column_usage` 的 NULL 过滤是硬要求：漏掉会把 PRIMARY/UNIQUE 当外键。
#[test]
fn mysql_fk_filters_non_fk_rows() {
    assert!(sql(Kind::ForeignKeys, Dialect::MySql).contains("referenced_table_name IS NOT NULL"));
}

/// PG 的索引/外键必须限定 schema（`pg_*` 无 schema 列，否则跨 schema 串数据）。
#[test]
fn pg_scopes_to_current_schema() {
    for kind in [Kind::Indexes, Kind::ForeignKeys] {
        let s = sql(kind, Dialect::Postgres);
        assert!(
            s.contains("current_schema()"),
            "{kind:?}: PG 缺 schema 限定: {s}"
        );
    }
}

/// 保留字别名须按方言加引号（CI 无真实 PG/MySQL 的离线防线）：
/// `column` 在 mysql 8 / pg 均为保留字，`unique`/`primary` 同理——裸投影直接语法错误。
/// pg 用双引号且**不得出现反引号**；mysql 用反引号。sqlite 不保留这些词，豁免。
#[test]
fn reserved_word_aliases_quoted_per_dialect() {
    for kind in [Kind::Indexes, Kind::ForeignKeys] {
        let my = sql(kind, Dialect::MySql);
        assert!(
            !my.contains("AS column"),
            "mysql {kind:?} 裸保留字别名: {my}"
        );
        assert!(
            my.contains("AS `column`"),
            "mysql {kind:?} 缺 `column` 引用: {my}"
        );
        let pg = sql(kind, Dialect::Postgres);
        assert!(!pg.contains('`'), "PG {kind:?} 不得出现反引号: {pg}");
        assert!(
            pg.contains("AS \"column\""),
            "PG {kind:?} 缺 \"column\" 引用: {pg}"
        );
    }
    // Indexes 另投影 unique/primary 两个保留字别名。
    assert!(sql(Kind::Indexes, Dialect::MySql).contains("AS `unique`"));
    assert!(sql(Kind::Indexes, Dialect::MySql).contains("AS `primary`"));
    assert!(sql(Kind::Indexes, Dialect::Postgres).contains("AS \"unique\""));
    assert!(sql(Kind::Indexes, Dialect::Postgres).contains("AS \"primary\""));
    // catalog Constraints 同样投影 `column`（缺陷同类，一并钉死）。
    assert!(
        catalog_sql(Kind::Constraints, Dialect::MySql)
            .unwrap()
            .contains("AS `column`")
    );
    let pg_c = catalog_sql(Kind::Constraints, Dialect::Postgres).unwrap();
    assert!(!pg_c.contains('`'), "PG constraints 不得出现反引号: {pg_c}");
    assert!(
        pg_c.contains("AS \"column\""),
        "PG constraints 缺 \"column\" 引用: {pg_c}"
    );
}

/// 聚合：索引按名合并列、外键按位配对、sqlite 无名 → null。
#[test]
fn aggregation_shapes() {
    let idx = indexes(vec![
        json!({"name": "ix", "unique": 0, "primary": 0, "origin": "c", "column": "a", "seq": 0}),
        json!({"name": "ix", "unique": 0, "primary": 0, "origin": "c", "column": "b", "seq": 1}),
        json!({"name": "PRIMARY", "unique": 1, "primary": 1, "origin": "pk", "column": "id", "seq": 1}),
    ]);
    assert_eq!(idx.len(), 2);
    assert_eq!(idx[0]["columns"], json!(["a", "b"]));
    assert_eq!(idx[1]["origin"], json!("pk"));

    let fk = foreign_keys(vec![
        json!({"group_key": "0", "name": null, "column": "b_id", "ref_table": "p",
                   "ref_column": "id", "col_pos": 0, "ref_pos": 0}),
        json!({"group_key": "0", "name": null, "column": "c_id", "ref_table": "p",
                   "ref_column": "code", "col_pos": 1, "ref_pos": 1}),
    ]);
    assert_eq!(fk.len(), 1, "{fk:?}");
    assert_eq!(fk[0]["name"], json!(null));
    assert_eq!(fk[0]["columns"], json!(["b_id", "c_id"]));
    assert_eq!(fk[0]["refColumns"], json!(["id", "code"]));
    assert_eq!(fk[0]["refTable"], json!("p"));
}

/// REQ-10 一期（v0.1.112）：全实例/全库语义的三个新 kind——形状 + truncated 旗标。
/// catalog() 返回 `(items, truncated)`（truncated = 入参行数 > MAX_ROWS）。
#[test]
fn catalog_new_kinds_shape_and_truncated() {
    let (items, truncated) = catalog(
        Kind::Processlist,
        vec![json!({
            "id": 1, "user": "u", "database": "d", "host": "h",
            "state": "active", "elapsed_sec": 3, "query": "select 1"
        })],
    );
    assert!(!truncated);
    let it = &items[0];
    assert_eq!(it["id"], json!(1));
    assert_eq!(it["state"], "active");
    assert!(it.get("elapsedSec").is_some(), "{it:?}");
    assert!(
        it.get("user").is_some() && it.get("query").is_some(),
        "{it:?}"
    );
    let (_, t) = catalog(
        Kind::Users,
        vec![json!({"name": "u", "host": "h"}); MAX_ROWS + 1],
    );
    assert!(t, "超 MAX_ROWS 必须给 truncated 旗标");
    let (roles, f) = catalog(Kind::Roles, vec![json!({"name": "r"})]);
    assert!(!f);
    assert_eq!(roles[0]["name"], "r");
}

/// REQ-08 收口（v0.1.112）：triggers kind——PG 行 timing/events 从
/// pg_get_triggerdef 文本令牌扫（definition 键存在即 PG 路径）。
#[test]
fn triggers_shape_pg_parses_definition() {
    let (items, t) = catalog(
        Kind::Triggers,
        vec![json!({
            "name": "trg", "table": "t", "tgenabled": "O",
            "definition": "CREATE TRIGGER trg BEFORE INSERT OR UPDATE ON t \
                           FOR EACH ROW EXECUTE FUNCTION f()"
        })],
    );
    assert!(!t);
    let it = &items[0];
    for k in [
        "name", "table", "timing", "events", "body", "order", "enabled",
    ] {
        assert!(it.get(k).is_some(), "missing {k}: {it:?}");
    }
    assert_eq!(it["timing"], "BEFORE");
    assert_eq!(it["events"], json!(["INSERT", "UPDATE"]));
    assert_eq!(it["enabled"], json!(true));
    // PG：body/order 无独立来源（definition 即全文）→ 显式 null，schema 稳定。
    assert_eq!(it["body"], Value::Null);
    assert_eq!(it["order"], Value::Null);
    // INSTEAD OF（pg 视图触发器）与 DISABLED 判定。
    let (v, _) = catalog(
        Kind::Triggers,
        vec![json!({
            "name": "iv", "table": "v", "tgenabled": "D",
            "definition": "CREATE TRIGGER iv INSTEAD OF DELETE ON v \
                           FOR EACH ROW EXECUTE FUNCTION g()"
        })],
    );
    assert_eq!(v[0]["timing"], "INSTEAD OF");
    assert_eq!(v[0]["events"], json!(["DELETE"]));
    assert_eq!(v[0]["enabled"], json!(false));
}

/// MySQL 行：timing/event/body/ord 直映（events 为单元素数组），enabled 无来源 → null。
#[test]
fn triggers_shape_mysql_direct_columns() {
    let (items, _) = catalog(
        Kind::Triggers,
        vec![json!({
            "name": "trg", "table": "t", "timing": "AFTER",
            "event": "INSERT", "body": "BEGIN SET @a = 1; END", "ord": 1
        })],
    );
    let it = &items[0];
    assert_eq!(it["timing"], "AFTER");
    assert_eq!(it["events"], json!(["INSERT"]));
    assert_eq!(it["body"], "BEGIN SET @a = 1; END");
    assert_eq!(it["order"], json!(1));
    assert_eq!(it["enabled"], Value::Null);
}

/// REQ-08 §3.5 评审修复（v0.1.112）：catalog 抓取上限必须比 MAX_ROWS 多一行——
/// op 给 catalog SQL 追 `LIMIT` 后，`catalog()` 的 truncated 信令（rows.len() > MAX_ROWS）
/// 只有抓过线才可能为真；追 LIMIT MAX_ROWS 会让旗标在生产路径恒 false（评审 Important）。
#[test]
fn catalog_fetch_cap_enables_truncated_signal() {
    assert_eq!(CATALOG_FETCH_CAP, MAX_ROWS + 1);
}

/// REQ-08 §3.1（v0.1.112）：routines items 带元数据键集（缺省 null，schema 稳定）。
#[test]
fn routines_shape_carries_metadata() {
    let (items, _) = catalog(
        Kind::Routines,
        vec![json!({
            "name": "f_add", "type": "FUNCTION", "args": "a integer, b integer",
            "returns": "integer", "source": "select a+b", "language": "sql",
            "volatility": "s", "secdef": true, "comment": "c"
        })],
    );
    let it = &items[0];
    for k in [
        "args",
        "returns",
        "source",
        "language",
        "volatility",
        "securityDefiner",
        "comment",
    ] {
        assert!(it.get(k).is_some(), "missing {k}: {it:?}");
    }
    assert_eq!(it["volatility"], "stable");
    assert_eq!(it["securityDefiner"], json!(true));
    // mysql 行（deterministic/security 词汇）同口径。
    let (my, _) = catalog(
        Kind::Routines,
        vec![json!({
            "name": "p", "type": "PROCEDURE", "deterministic": "YES",
            "security": "DEFINER"
        })],
    );
    assert_eq!(my[0]["volatility"], "deterministic");
    assert_eq!(my[0]["securityDefiner"], json!(true));
    // sequences 维度零变化（仍只有 name/type）。
    let (seq, _) = catalog(
        Kind::Sequences,
        vec![json!({"name": "s", "type": "sequence"})],
    );
    assert_eq!(seq[0], json!({"name": "s", "type": "sequence"}));
}

/// REQ-08 §3.3：pg_get_viewdef 片段补壳（剥尾分号）为可执行 DDL。
#[test]
fn pg_view_wrap_adds_create_shell() {
    assert_eq!(
        pg_view_wrap("v", " SELECT 1 AS v;", false),
        "CREATE OR REPLACE VIEW \"v\" AS  SELECT 1 AS v"
    );
    assert!(pg_view_wrap("mv", "SELECT 1", true).starts_with("CREATE MATERIALIZED VIEW"));
    assert!(pg_view_wrap("v", "SELECT 1;", false).ends_with("SELECT 1"));
}

/// REQ-08 §3.2 新常量守卫：PG 例程 DDL（scope 谓词 / 单参数位 / name 域 cast / 无 LIKE）；
/// MySQL 对象类型探测（DATABASE() 谓词 ×3、3 参数位、无 LIKE）。
#[test]
fn req08_routing_constant_guards() {
    assert!(
        PG_ROUTINE_DDL.contains("current_schema()"),
        "{PG_ROUTINE_DDL}"
    );
    assert_eq!(PG_ROUTINE_DDL.matches('?').count(), 1, "{PG_ROUTINE_DDL}");
    assert!(PG_ROUTINE_DDL.contains("proname::text"), "{PG_ROUTINE_DDL}");
    assert!(!PG_ROUTINE_DDL.contains("LIKE"), "{PG_ROUTINE_DDL}");
    assert!(
        MYSQL_OBJECT_TYPE.contains("DATABASE()"),
        "{MYSQL_OBJECT_TYPE}"
    );
    assert_eq!(
        MYSQL_OBJECT_TYPE.matches('?').count(),
        3,
        "{MYSQL_OBJECT_TYPE}"
    );
    assert!(!MYSQL_OBJECT_TYPE.contains("LIKE"), "{MYSQL_OBJECT_TYPE}");
    // SHOW 动词路由。
    assert_eq!(mysql_show_verb("table"), "SHOW CREATE TABLE");
    assert_eq!(mysql_show_verb("view"), "SHOW CREATE VIEW");
    assert_eq!(mysql_show_verb("function"), "SHOW CREATE FUNCTION");
    assert_eq!(mysql_show_verb("procedure"), "SHOW CREATE PROCEDURE");
}

// ---- U-14 批 a：catalog 类的静态守卫（同一组离线防线扩展）----

const CATALOG: [Kind; 11] = [
    Kind::Views,
    Kind::Routines,
    Kind::Sequences,
    Kind::Ddl,
    Kind::Comment,
    Kind::Constraints,
    Kind::Stats,
    Kind::Processlist,
    Kind::Users,
    Kind::Roles,
    Kind::Triggers,
];

/// BUG-02 守卫：`key_column_usage` JOIN 必须含 table_name 关联。MySQL 全库 PK
/// 同名 `PRIMARY`，缺该条件会把同库（乃至被 scope 异库）的列全部聚合串味。
#[test]
fn mysql_constraints_join_guard() {
    let sql = catalog_sql(Kind::Constraints, Dialect::MySql).unwrap();
    assert!(
        sql.contains("kcu.table_name = tc.table_name"),
        "BUG-02: JOIN 必须含 table_name 关联，否则全库同名约束串味: {sql}"
    );
}

/// BUG-02 env-gated 真库腿（OJ_TEST_MYSQL）：同库两表各建 PK/UNIQUE，真跑
/// catalog SQL 再喂 `catalog()` 聚合，断言列归属不互串。未设 DSN 诚实 skip。
#[tokio::test(flavor = "current_thread")]
async fn real_mysql_constraints_no_cross_table_leak() {
    let url = match std::env::var("OJ_TEST_MYSQL") {
        Ok(u) if !u.is_empty() => u,
        _ => {
            eprintln!("skip: OJ_TEST_MYSQL unset");
            return;
        }
    };
    let db = crate::bridge::SqlxAccessor::arc(&url).await.unwrap();
    for ddl in [
        "drop table if exists t_b02_a",
        "drop table if exists t_b02_b",
        "create table t_b02_a (id int primary key, code int unique)",
        "create table t_b02_b (id int primary key, name varchar(8))",
    ] {
        db.exec_with_params(ddl, &[]).await.unwrap();
    }
    let sql = catalog_sql(Kind::Constraints, Dialect::MySql).unwrap();
    // t_b02_a：PK(id) + UQ(code)；t_b02_b：仅 PK(id)。串味时两表列集会相同。
    for (table, want) in [("t_b02_a", vec!["code", "id"]), ("t_b02_b", vec!["id"])] {
        let rows = db
            .query_with_params(sql, &[serde_json::json!(table)])
            .await
            .unwrap();
        let (items, _) = catalog(Kind::Constraints, rows);
        let mut cols: Vec<&str> = items
            .iter()
            .flat_map(|it| it["columns"].as_array().into_iter().flatten())
            .filter_map(|c| c.as_str())
            .collect();
        cols.sort();
        assert_eq!(cols, want, "{table} 约束串味: {items:?}");
    }
    for ddl in [
        "drop table if exists t_b02_a",
        "drop table if exists t_b02_b",
    ] {
        db.exec_with_params(ddl, &[]).await.unwrap();
    }
}

/// PG 投影列 `::text` 守卫扩到 catalog 常量（铁律 2：空库不触发、建表后才崩）。
#[test]
fn catalog_pg_constants_cast_identifiers() {
    for kind in CATALOG {
        let Some(s) = catalog_sql(kind, Dialect::Postgres) else {
            continue;
        };
        // REQ-10 一期：全实例/全库 kind 无 schema 范围语义，豁免范围限定与投影钉集。
        if matches!(kind, Kind::Processlist | Kind::Users | Kind::Roles) {
            continue;
        }
        for id in ["attname", "relname", "conname", "proname", "sequencename"] {
            let bare = String::from(id) + " AS";
            let cast = String::from(id) + "::text";
            assert!(
                !s.contains(&bare) || s.contains(&cast),
                "{kind:?}: PG 投影列 {id} 缺 ::text: {s}"
            );
        }
        assert!(
            s.contains("current_schema()"),
            "{kind:?}: PG 缺 schema 限定: {s}"
        );
    }
}

/// catalog 常量：无 LIKE；参数位与 `catalog_param` 一致（0 或 1）；
/// 不可 LIMIT 的 kind（Ddl）不得自带 LIMIT 之外的可追加语义冲突。
#[test]
fn catalog_constants_param_count_and_no_like() {
    for kind in CATALOG {
        for d in [Dialect::Sqlite, Dialect::MySql, Dialect::Postgres] {
            let Some(s) = catalog_sql(kind, d) else {
                continue;
            };
            assert!(!s.contains("LIKE"), "{kind:?}/{d:?} 不该用 LIKE: {s}");
            let want = usize::from(kind.catalog_param());
            assert_eq!(
                s.matches('?').count(),
                want,
                "{kind:?}/{d:?} 参数位应为 {want}: {s}"
            );
        }
    }
}

/// 能力矩阵锚：批 a 的 sqlite 只覆盖 views/ddl，其余 unsupported（报错不静默）。
#[test]
fn catalog_sqlite_matrix_matches_plan() {
    assert!(catalog_sql(Kind::Views, Dialect::Sqlite).is_some());
    assert!(catalog_sql(Kind::Ddl, Dialect::Sqlite).is_some());
    for kind in [
        Kind::Routines,
        Kind::Sequences,
        Kind::Comment,
        Kind::Constraints,
        Kind::Stats,
        Kind::Processlist,
        Kind::Users,
        Kind::Roles,
        Kind::Triggers,
    ] {
        assert!(
            catalog_sql(kind, Dialect::Sqlite).is_none(),
            "{kind:?} × sqlite 应为 unsupported"
        );
    }
}

/// mysql SHOW 例外：白名单字符集 + 反引号引用；非法标识符拒绝；REQ-08 动词路由。
#[test]
fn mysql_show_create_whitelist() {
    assert_eq!(
        mysql_show_create_typed("t", "table").unwrap(),
        "SHOW CREATE TABLE `t`"
    );
    assert_eq!(
        mysql_show_create_typed("v", "view").unwrap(),
        "SHOW CREATE VIEW `v`"
    );
    for bad in ["t; DROP TABLE x", "t`", "", "粗", "t a"] {
        assert!(mysql_show_create_typed(bad, "table").is_err(), "{bad:?}");
    }
    // REQ-09-2：scoped 两段形态——各段独立白名单 + 反引号引用。
    assert_eq!(
        mysql_show_create_scoped("app.t", "table").unwrap(),
        "SHOW CREATE TABLE `app`.`t`"
    );
    assert_eq!(
        mysql_show_create_scoped("db1.v1", "view").unwrap(),
        "SHOW CREATE VIEW `db1`.`v1`"
    );
    for bad in [
        "app.t.x",
        "app.t; DROP",
        "app`.t",
        ".t",
        "app.",
        "粗.t",
        "app.粗",
    ] {
        assert!(mysql_show_create_scoped(bad, "table").is_err(), "{bad:?}");
    }
}

// ---- U-14（v0.1.109）PG 拼接器：常量守卫 + 行归一 + 渲染器 ----

const STITCH_CONSTS: [&str; 5] = [
    PG_STITCH_COLUMNS,
    PG_STITCH_CONSTRAINTS,
    PG_STITCH_INDEXES,
    PG_STITCH_COLUMN_COMMENTS,
    PG_STITCH_TABLE_COMMENT,
];

/// 拼接器常量守卫（PG 铁律 2/3 + 参数位 + 等值匹配）。
#[test]
fn pg_stitch_constants_guard() {
    for s in STITCH_CONSTS {
        assert!(s.contains("current_schema()"), "schema 限定: {s}");
        assert_eq!(
            s.matches("current_schema()").count(),
            1,
            "范围谓词恰一次: {s}"
        );
        assert_eq!(s.matches('?').count(), 1, "参数位恰 1: {s}");
        assert!(!s.contains("LIKE"), "等值匹配，非 LIKE: {s}");
    }
    // name 域投影必须 ::text（attname/relname/conname）。
    assert!(PG_STITCH_COLUMNS.contains("attname::text"));
    assert!(PG_STITCH_COLUMNS.contains("relname::text AS seq_name"));
    assert!(PG_STITCH_CONSTRAINTS.contains("conname::text"));
    assert!(PG_STITCH_INDEXES.contains("relname::text"));
    // dropped 列过滤是硬要求（占位行进 DDL = 语法错误产物）。
    assert!(PG_STITCH_COLUMNS.contains("NOT a.attisdropped"));
    assert!(PG_STITCH_COLUMNS.contains("a.attnum > 0"));
    // 约束支撑索引必须排除（否则与 ADD CONSTRAINT 重复产出，重放名冲突必炸）；
    // 粗过滤 indisprimary AND NOT indisunique 会误杀非约束 CREATE UNIQUE INDEX，禁止。
    assert!(PG_STITCH_INDEXES.contains("NOT EXISTS (SELECT 1 FROM pg_constraint"));
    assert!(!PG_STITCH_INDEXES.contains("indisprimary"));
    // serial 走 pg_depend 'a' auto 依赖取序列名，绝不解析 nextval 文本。
    assert!(PG_STITCH_COLUMNS.contains("deptype = 'a'"));
    assert!(!PG_STITCH_COLUMNS.contains("nextval"));
    // 表注释必须双参数形态 obj_description(oid, 'pg_class')（单参数已废弃）。
    assert!(PG_STITCH_TABLE_COMMENT.contains("obj_description(c.oid, 'pg_class')"));
}

/// 行归一：identity 映射（a→always/d→byDefault）、seqName、generated、
/// dropped 占位行纵深跳过（SQL 已过滤，归一再挡一道）。
#[test]
fn pg_stitch_columns_normalizes() {
    let out = pg_stitch_columns(vec![
        json!({"name": "id", "native_type": "integer", "not_null": true,
                   "default": "nextval('t_id_seq'::regclass)", "is_pk": true,
                   "identity": "", "generated": "", "seq_name": "t_id_seq", "pos": 1}),
        json!({"name": "code", "native_type": "character varying(64)", "not_null": true,
                   "default": null, "is_pk": false,
                   "identity": "a", "generated": "", "seq_name": null, "pos": 2}),
        json!({"name": "........pg.dropped.3........", "native_type": "text",
                   "not_null": false, "default": null, "is_pk": false,
                   "identity": "", "generated": "", "seq_name": null, "pos": 3}),
    ]);
    assert_eq!(out.len(), 2, "dropped 占位行不得入列: {out:?}");
    assert_eq!(out[0]["name"], json!("id"));
    assert_eq!(out[0]["nullable"], json!(false));
    assert_eq!(out[0]["primaryKey"], json!(true));
    assert_eq!(out[0]["seqName"], json!("t_id_seq"));
    assert!(!out[0].as_object().unwrap().contains_key("identity"));
    assert_eq!(out[1]["identity"], json!("always"));
    assert!(!out[1].as_object().unwrap().contains_key("seqName"));
}

/// 渲染器：serial（CREATE SEQUENCE 前置 + OWNED BY 后置）+ 约束 + 索引 + 双注释。
#[test]
fn pg_stitch_renders_serial_and_constraints() {
    let ddl = pg_create_table(
        "oj_ddl_t",
        &[
            json!({"name": "id", "nativeType": "integer", "nullable": false,
                       "default": null, "primaryKey": true, "seqName": "oj_ddl_t_id_seq"}),
            json!({"name": "email", "nativeType": "character varying(64)",
                       "nullable": false, "default": null, "primaryKey": false}),
        ],
        &[json!({"name": "oj_ddl_t_pkey", "type": "pk",
                     "definition": "PRIMARY KEY (id)"})],
        &[json!({"name": "ix_email",
                     "definition": "CREATE INDEX ix_email ON public.oj_ddl_t USING btree (email)"})],
        &[json!({"column": "email", "comment": "登录邮箱"})],
        Some("业务表"),
        None,
    );
    assert!(
        ddl.contains("CREATE SEQUENCE IF NOT EXISTS \"oj_ddl_t_id_seq\""),
        "{ddl}"
    );
    assert!(ddl.contains("CREATE TABLE \"oj_ddl_t\""), "{ddl}");
    assert!(ddl.contains("\"id\" integer NOT NULL"), "{ddl}");
    assert!(
        ddl.contains("ADD CONSTRAINT \"oj_ddl_t_pkey\" PRIMARY KEY (id)"),
        "{ddl}"
    );
    assert!(
        ddl.contains("CREATE INDEX ix_email ON public.oj_ddl_t USING btree (email)"),
        "{ddl}"
    );
    assert!(
        ddl.contains("COMMENT ON COLUMN \"oj_ddl_t\".\"email\" IS '登录邮箱'"),
        "{ddl}"
    );
    assert!(
        ddl.contains("COMMENT ON TABLE \"oj_ddl_t\" IS '业务表'"),
        "{ddl}"
    );
    assert!(
        ddl.contains("ALTER SEQUENCE \"oj_ddl_t_id_seq\" OWNED BY \"oj_ddl_t\".\"id\""),
        "{ddl}"
    );
    // 顺序：序列在 CREATE TABLE 前，OWNED BY 在最后。
    let i_seq = ddl.find("CREATE SEQUENCE").unwrap();
    let i_tbl = ddl.find("CREATE TABLE").unwrap();
    let i_own = ddl.find("OWNED BY").unwrap();
    assert!(i_seq < i_tbl && i_tbl < i_own, "顺序钉死: {ddl}");
}

/// 分区表：partkeydef 非空 → CREATE TABLE 附 PARTITION BY（best-effort，子分区不产出）。
#[test]
fn pg_stitch_partitioned_appends_partition_by() {
    let ddl = pg_create_table(
        "m",
        &[
            json!({"name": "id", "nativeType": "integer", "nullable": false,
                     "default": null, "primaryKey": true}),
        ],
        &[],
        &[],
        &[],
        None,
        Some("RANGE (created_at)"),
    );
    assert!(ddl.contains(") PARTITION BY RANGE (created_at)"), "{ddl}");
    let plain = pg_create_table(
        "m",
        &[
            json!({"name": "id", "nativeType": "integer", "nullable": false,
                     "default": null, "primaryKey": true}),
        ],
        &[],
        &[],
        &[],
        None,
        None,
    );
    assert!(!plain.contains("PARTITION BY"), "{plain}");
}

/// generated 列渲染 GENERATED ALWAYS AS (expr) STORED 且无 DEFAULT；
/// identity 列渲染 GENERATED ALWAYS AS IDENTITY 且不产序列语句。
#[test]
fn pg_stitch_generated_and_identity_columns() {
    let ddl = pg_create_table(
        "t",
        &[
            json!({"name": "id", "nativeType": "integer", "nullable": false,
                       "default": null, "primaryKey": true, "identity": "always"}),
            json!({"name": "total", "nativeType": "integer", "nullable": false,
                       "default": null, "primaryKey": false,
                       "generated": "stored", "generatedExpr": "a + b"}),
        ],
        &[],
        &[],
        &[],
        None,
        None,
    );
    assert!(
        ddl.contains("\"id\" integer GENERATED ALWAYS AS IDENTITY"),
        "{ddl}"
    );
    assert!(!ddl.contains("CREATE SEQUENCE"), "identity 自管序列: {ddl}");
    assert!(ddl.contains("GENERATED ALWAYS AS (a + b) STORED"), "{ddl}");
    assert!(!ddl.contains("DEFAULT"), "generated 列无 DEFAULT: {ddl}");
}

/// 容量防线：db_error::capacity 结构化（category=Capacity → OJ_DB_CAPACITY，
/// data.limit 带出限额），拼接器任一路行数触顶即此错误，绝不静默截断。
#[test]
fn pg_stitch_capacity_error_shape() {
    let e = crate::bridge::db_error::capacity("pg ddl stitch", MAX_ROWS);
    assert_eq!(
        e.category,
        crate::bridge::db_error::DbErrorCategory::Capacity
    );
    assert_eq!(
        e.data.get("limit").and_then(|v| v.as_u64()),
        Some(MAX_ROWS as u64)
    );
}

/// listColumns identity 加性输出（spec：Structure 契约与已发布内视同词汇）：
/// PG 行 attidentity='a'→always、'd'→byDefault；mysql/sqlite 无投影列 → 不落键。
#[test]
fn list_columns_identity_is_additive() {
    let out = columns(vec![
        json!({"name": "id", "native_type": "integer", "not_null": true,
                   "default": null, "is_pk": true, "identity": "a"}),
        json!({"name": "v", "native_type": "integer", "not_null": false,
                   "default": null, "is_pk": false, "identity": "d"}),
        json!({"name": "n", "native_type": "text", "not_null": false,
                   "default": null, "is_pk": false, "identity": ""}),
        // mysql 形态（无 identity 投影列）。
        json!({"name": "m", "native_type": "int", "not_null": false,
                   "default": null, "is_pk": false}),
    ]);
    assert_eq!(out[0]["identity"], json!("always"));
    assert_eq!(out[1]["identity"], json!("byDefault"));
    assert!(
        !out[2].as_object().unwrap().contains_key("identity"),
        "{:?}",
        out[2]
    );
    assert!(
        !out[3].as_object().unwrap().contains_key("identity"),
        "{:?}",
        out[3]
    );
    // 其余字段不破契约。
    assert_eq!(out[0]["name"], json!("id"));
    assert_eq!(out[0]["nullable"], json!(false));
    assert_eq!(out[0]["primaryKey"], json!(true));
}

/// U-14（v0.1.109）relkind 路由矩阵（spec 钉死）：v/m→View、r/p→Table、
/// 其余（f/i/S/c/t）与不存在（None）→Empty——假 DDL 比空结果更糟。
#[test]
fn pg_ddl_route_matrix() {
    assert_eq!(pg_ddl_route(Some("v")), PgDdlRoute::View);
    assert_eq!(pg_ddl_route(Some("m")), PgDdlRoute::View);
    assert_eq!(pg_ddl_route(Some("r")), PgDdlRoute::Table);
    assert_eq!(pg_ddl_route(Some("p")), PgDdlRoute::Table);
    for k in ["f", "i", "S", "c", "t"] {
        assert_eq!(pg_ddl_route(Some(k)), PgDdlRoute::Empty, "{k}");
    }
    assert_eq!(pg_ddl_route(None), PgDdlRoute::Empty);
}

/// relkind 探测常量守卫（PG 铁律 2/3 + 参数位）：`::text`、current_schema()、
/// `?` 恰 1、无 LIKE。
#[test]
fn pg_ddl_relkind_guard() {
    let s = PG_DDL_RELKIND;
    assert!(s.contains("relkind::text"), "char 域投影必须 ::text: {s}");
    assert!(s.contains("current_schema()"), "schema 限定: {s}");
    assert_eq!(s.matches('?').count(), 1, "参数位恰 1: {s}");
    assert!(!s.contains("LIKE"), "等值匹配，非 LIKE: {s}");
}

/// catalog 归一：constraints 按名聚合列；stats 解析字符串数值（pg ::text 口径）。
#[test]
fn catalog_normalization() {
    let (cs, _) = catalog(
        Kind::Constraints,
        vec![
            json!({"name": "pk_t", "type": "PRIMARY KEY", "definition": null, "column": "id", "pos": 1}),
            json!({"name": "pk_t", "type": "PRIMARY KEY", "definition": null, "column": "code", "pos": 2}),
            json!({"name": "uq_t", "type": "UNIQUE", "definition": null, "column": "email", "pos": 1}),
        ],
    );
    assert_eq!(cs.len(), 2, "{cs:?}");
    assert_eq!(cs[0]["type"], json!("pk"));
    assert_eq!(cs[0]["columns"], json!(["id", "code"]));

    let (st, _) = catalog(
        Kind::Stats,
        vec![json!({"name": "t", "rows_estimate": "42", "size_bytes": "8192"})],
    );
    assert_eq!(st[0]["rowsEstimate"], json!(42.0));
    assert_eq!(st[0]["sizeBytes"], json!(8192.0));
}

/// 列归一（aggregation_shapes 的余段，评审修订时被拆分——语义不变）：
/// sqlite rowid PK 仍可空、default null。
#[test]
fn columns_rowid_pk_shape() {
    let col = columns(vec![json!({"name": "id", "native_type": "integer",
                                      "not_null": 0, "default": null, "is_pk": 1})]);
    assert_eq!(col[0]["nullable"], json!(true), "sqlite rowid PK 仍可空");
    assert_eq!(col[0]["primaryKey"], json!(true));
    assert_eq!(col[0]["default"], json!(null));
}

// -----------------------------------------------------------------
// F1（批次 4 / REQ-04，v0.1.105）：库-schema 清单面 + forSchema 范围变换
// -----------------------------------------------------------------

/// 清单 SQL 矩阵：mysql schema≡database 同源（schemata）；pg schemas 走
/// information_schema.schemata、databases 走集群共享目录 pg_database；
/// sqlite 两条腿都给 database_list（main + attached，是唯一诚实的答案）。
#[test]
fn listing_sql_matrix() {
    assert_eq!(
        listing_sql(Listing::Schemas, Dialect::MySql),
        listing_sql(Listing::Databases, Dialect::MySql),
        "mysql schema≡database，须同源"
    );
    let my = listing_sql(Listing::Schemas, Dialect::MySql).unwrap();
    for sys in [
        "'information_schema'",
        "'mysql'",
        "'performance_schema'",
        "'sys'",
    ] {
        assert!(my.contains(sys), "mysql 系统库过滤缺 {sys}: {my}");
    }
    let pg = listing_sql(Listing::Schemas, Dialect::Postgres).unwrap();
    assert!(pg.contains("information_schema.schemata"), "{pg}");
    assert!(pg.contains("'pg_catalog'"), "{pg}");
    assert!(pg.contains("pg_toast%"), "pg_toast 系列过滤: {pg}");
    assert!(pg.contains("::text"), "pg 标识符域铁律: {pg}");
    let pgd = listing_sql(Listing::Databases, Dialect::Postgres).unwrap();
    assert!(pgd.contains("pg_database"), "{pgd}");
    assert!(pgd.contains("datistemplate"), "模板库过滤: {pgd}");
    assert!(pgd.contains("current_database()"), "isDefault 判定: {pgd}");
    let lite = listing_sql(Listing::Schemas, Dialect::Sqlite).unwrap();
    assert!(lite.contains("database_list"), "{lite}");
}

/// 未知方言（插件轴开放契约态）：清单面与内视面一样结构化 unsupported，不回落。
#[test]
fn listing_sql_unknown_dialect_is_none() {
    // Dialect 只有三 known 变体；矩阵用全遍历钉死——新增方言必须显式给清单 SQL。
    for l in [Listing::Schemas, Listing::Databases] {
        for d in [Dialect::Sqlite, Dialect::MySql, Dialect::Postgres] {
            assert!(listing_sql(l, d).is_some(), "{l:?}/{d:?} 缺清单 SQL");
        }
    }
}

/// 清单归一：字段可选、跨方言同构；布尔宽容（0/1/'YES'/true）。
#[test]
fn listing_normalization() {
    let rows = vec![
        json!({"name": "app", "owner": "alice", "is_default": 1, "comment": null}),
        json!({"name": "public", "owner": "postgres", "is_default": 0}),
    ];
    let out = listing(rows);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0]["name"], json!("app"));
    assert_eq!(out[0]["owner"], json!("alice"));
    assert_eq!(out[0]["isDefault"], json!(true));
    assert!(
        !out[0].as_object().unwrap().contains_key("comment"),
        "NULL 不落键"
    );
    assert!(
        !out[1].as_object().unwrap().contains_key("ownerMap"),
        "无垃圾键"
    );
    assert_eq!(out[1]["isDefault"], json!(false));
}

/// forSchema 范围变换：mysql/pg 的全部内视常量（L2 三件 + catalog 全类 +
/// list_tables 骨架同规则）把范围谓词改为恰好一个绑定参数；sqlite → None。
#[test]
fn scoped_sql_replaces_exactly_one_scope_predicate() {
    for kind in [Kind::Columns, Kind::Indexes, Kind::ForeignKeys] {
        for d in [Dialect::MySql, Dialect::Postgres] {
            let base = sql(kind, d);
            let scoped = scoped_sql(base, d).unwrap();
            assert!(
                !scoped.contains("DATABASE()") && !scoped.contains("current_schema()"),
                "{kind:?}/{d:?}: 残留范围谓词: {scoped}"
            );
            assert_eq!(
                scoped.matches('?').count(),
                base.matches('?').count() + 1,
                "{kind:?}/{d:?}: scope 参数位须恰增 1"
            );
        }
    }
    for kind in [
        Kind::Views,
        Kind::Routines,
        Kind::Sequences,
        Kind::Comment,
        Kind::Constraints,
        Kind::Stats,
    ] {
        for d in [Dialect::MySql, Dialect::Postgres] {
            let base = catalog_sql(kind, d).unwrap();
            let scoped = scoped_sql(base, d).unwrap();
            assert!(
                !scoped.contains("DATABASE()") && !scoped.contains("current_schema()"),
                "{kind:?}/{d:?}: 残留范围谓词: {scoped}"
            );
        }
    }
    assert!(scoped_sql(sql(Kind::Columns, Dialect::Sqlite), Dialect::Sqlite).is_none());
    // mysql SHOW 例外（Ddl）走标识符内联白名单、不接受绑定参数——scoped 模式由
    // 调用方直接报 unsupported，不经过 scoped_sql。
}

/// F3 硬契约钉死：凡带参数位的非 sqlite 内视常量，范围谓词必须先于**第一个 `?`**
/// 出现——调用方 `params.insert(0, scope)` 依赖此序。PG 的 Indexes/ForeignKeys 与
/// catalog 的 Ddl/Comment/Constraints 曾把 `n.nspname = current_schema()` 写在表名
/// `?` 之后 → scoped 改写后 scope 占位符不再是首位，错绑表名位、静默空结果
/// （BUG-pg-scoped-introspect-param-binding，v0.1.106 实测暴露）。
#[test]
fn scope_predicate_precedes_first_param() {
    let pred = |d: Dialect| match d {
        Dialect::MySql => "DATABASE()",
        _ => "current_schema()",
    };
    for kind in [Kind::Columns, Kind::Indexes, Kind::ForeignKeys] {
        for d in [Dialect::MySql, Dialect::Postgres] {
            let s = sql(kind, d);
            let Some(q) = s.find('?') else { continue };
            assert!(
                s[..q].contains(pred(d)),
                "{kind:?}/{d:?}: 范围谓词须在第一个 ? 之前（insert(0, scope) 契约）: {s}"
            );
        }
    }
    for kind in CATALOG {
        for d in [Dialect::MySql, Dialect::Postgres] {
            let Some(s) = catalog_sql(kind, d) else {
                continue;
            };
            let Some(q) = s.find('?') else { continue };
            assert!(
                s[..q].contains(pred(d)),
                "{kind:?}/{d:?}: 范围谓词须在第一个 ? 之前（insert(0, scope) 契约）: {s}"
            );
        }
    }
}

/// 范围谓词恰出现一次（变换正确性的前置不变量）：    /// 若某常量漏写范围限定（如 pg 索引忘加 nspname），上面的替换会静默 no-op——
/// 用「非 sqlite 常量必含范围谓词」钉死。
#[test]
fn network_constants_all_carry_scope_predicate() {
    for kind in [Kind::Columns, Kind::Indexes, Kind::ForeignKeys] {
        for d in [Dialect::MySql, Dialect::Postgres] {
            let base = sql(kind, d);
            let has = base.contains("DATABASE()") || base.contains("current_schema()");
            assert!(has, "{kind:?}/{d:?}: 内视 SQL 缺范围谓词: {base}");
        }
    }
    for kind in [
        Kind::Views,
        Kind::Routines,
        Kind::Sequences,
        Kind::Comment,
        Kind::Constraints,
        Kind::Stats,
    ] {
        for d in [Dialect::MySql, Dialect::Postgres] {
            let base = catalog_sql(kind, d).unwrap();
            let has = base.contains("DATABASE()") || base.contains("current_schema()");
            assert!(has, "{kind:?}/{d:?}: catalog SQL 缺范围谓词: {base}");
        }
    }
}
