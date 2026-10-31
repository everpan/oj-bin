use super::*;
use serde_json::json;

fn both(spec: Value) -> (Value, Value) {
    (
        ddl_diff(Dialect::Postgres, &spec).unwrap(),
        ddl_diff(Dialect::MySql, &spec).unwrap(),
    )
}

/// REQ-09-2（v0.1.112）：可选 schema 段 → 表引用两段引用渲染（create + alter 两路径）。
#[test]
fn schema_field_two_part_quoting_pg_mysql() {
    let spec = json!({"objectType":"table","schema":"public","name":"t","from":Value::Null,
          "to":{"columns":[{"name":"id","nativeType":"int4"}]}});
    let (pg, my) = both(spec);
    assert!(
        pg["statements"][0]
            .as_str()
            .unwrap()
            .contains("\"public\".\"t\"")
    );
    assert!(
        my["statements"][0]
            .as_str()
            .unwrap()
            .contains("`public`.`t`")
    );
    // alter 路径同样两段引用。
    let spec2 = json!({"objectType":"table","schema":"app","name":"t",
          "from":{"columns":[{"name":"id","nativeType":"int4"}]},
          "to":{"columns":[{"name":"id","nativeType":"int8"}]}});
    let (pg2, my2) = both(spec2);
    let p = pg2["statements"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n");
    let m = my2["statements"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(p.contains("ALTER TABLE \"app\".\"t\""), "{p}");
    assert!(m.contains("ALTER TABLE `app`.`t`"), "{m}");
    // diff 输出键仍用裸名。
    assert_eq!(pg2["diff"]["columns"]["changed"][0]["name"], json!("id"));
}

/// schema 段 NUL/控制字符拒绝（注入防线同 quote_ident，错误须点名 schema）。
#[test]
fn schema_segment_nul_rejected() {
    let spec = json!({"objectType":"table","schema":"a\u{0}b","name":"t","from":Value::Null,
          "to":{"columns":[{"name":"id","nativeType":"int4"}]}});
    let e = ddl_diff(Dialect::Postgres, &spec).unwrap_err();
    assert!(e.contains("schema"), "{e}");
    let e = ddl_diff(Dialect::MySql, &spec).unwrap_err();
    assert!(e.contains("schema"), "{e}");
}

/// 评审 minor 修复：多个同型一删一增时告警按 removed 列聚合——一列一条、
/// 列名给出全部同型候选，不发 N×M 叉积。
#[test]
fn ambiguous_warnings_grouped_per_removed_column() {
    let (pg, _) = both(json!({
            "objectType": "table", "name": "t",
            "from": {"columns": [{"name": "a", "nativeType": "int4"},
                                 {"name": "c", "nativeType": "int4"}]},
            "to": {"columns": [{"name": "b", "nativeType": "int4"},
                               {"name": "d", "nativeType": "int4"}]}}));
    let warns = pg["warnings"].as_array().unwrap();
    assert_eq!(warns.len(), 2, "{warns:?}");
    let joined = warns
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains('a') && joined.contains('c'), "{joined}");
    assert!(joined.contains('b') && joined.contains('d'), "{joined}");
}

/// G2（REQ-09 §4.3）：partitionBy 差异 → unsupported（helper 与 ddl_diff 两路同判据）。
#[test]
fn partition_change_is_unsupported_ddl_diff_partition() {
    let spec = json!({"objectType":"table","name":"t",
            "from":{"columns":[{"name":"d","nativeType":"date"}],"partitionBy":"RANGE (d)"},
            "to":{"columns":[{"name":"d","nativeType":"date"}],"partitionBy":"RANGE (x)"}});
    assert_eq!(
        unsupported_capability_from_spec(Dialect::Postgres, &spec),
        Some("ddl_diff_partition")
    );
    let err = ddl_diff(Dialect::Postgres, &spec).unwrap_err();
    assert!(err.contains("ddl_diff_partition"), "{err}");
    // 一侧有一侧无同样 unsupported。
    let spec2 = json!({"objectType":"table","name":"t",
            "from":{"columns":[{"name":"d","nativeType":"date"}]},
            "to":{"columns":[{"name":"d","nativeType":"date"}],"partitionBy":"RANGE (d)"}});
    assert_eq!(
        unsupported_capability_from_spec(Dialect::MySql, &spec2),
        Some("ddl_diff_partition")
    );
    // 皆空 / 皆同 → None（零语句正常路径不被误伤）。
    let spec3 = json!({"objectType":"table","name":"t",
            "from":{"columns":[{"name":"d","nativeType":"date"}]},
            "to":{"columns":[{"name":"d","nativeType":"date"}]}});
    assert_eq!(
        unsupported_capability_from_spec(Dialect::Postgres, &spec3),
        None
    );
    let spec4 = json!({"objectType":"table","name":"t",
            "from":{"columns":[{"name":"d","nativeType":"date"}],"partitionBy":"RANGE (d)"},
            "to":{"columns":[{"name":"d","nativeType":"date"}],"partitionBy":"RANGE (d)"}});
    assert_eq!(
        unsupported_capability_from_spec(Dialect::Postgres, &spec4),
        None
    );
}

/// G2（REQ-09 §4.3）：MySQL options 差异 → ddl_diff_table_options。
#[test]
fn options_change_is_unsupported_ddl_diff_table_options() {
    let mk = |e: &str| {
        json!({"objectType":"table","name":"t",
                "from":{"columns":[{"name":"id","nativeType":"int"}],
                        "options":{"engine":"InnoDB"}},
                "to":{"columns":[{"name":"id","nativeType":"int"}],
                      "options":{"engine":e}}})
    };
    assert_eq!(
        unsupported_capability_from_spec(Dialect::MySql, &mk("MyISAM")),
        Some("ddl_diff_table_options")
    );
    assert_eq!(
        unsupported_capability_from_spec(Dialect::MySql, &mk("InnoDB")),
        None
    );
}

/// validate_statement_body：拒 NUL / 超 64 KiB；接受多行与分号（语句体信任类）。
#[test]
fn validate_statement_body_accepts_multiline_rejects_nul_and_oversize() {
    validate_statement_body("definition", "BEGIN\n  RETURN 1;\nEND").unwrap();
    assert!(validate_statement_body("definition", "a\0b").is_err());
    let big = "x".repeat(64 * 1024 + 1);
    assert!(validate_statement_body("partitionBy", &big).is_err());
}

/// G1（REQ-09 §4.2）：from=null 建分区表——闭合括号后拼 PARTITION BY（两方言）。
#[test]
fn create_table_renders_partition_by_pg_mysql() {
    let (pg, my) = both(json!({"objectType":"table","name":"t","from":Value::Null,
            "to":{"columns":[{"name":"d","nativeType":"date"}],
                  "partitionBy":"RANGE (d)"}}));
    let pgs = pg["statements"][0].as_str().unwrap();
    assert!(
        pgs.starts_with("CREATE TABLE \"t\"") && pgs.ends_with("PARTITION BY RANGE (d)"),
        "{pgs}"
    );
    let mys = my["statements"][0].as_str().unwrap();
    assert!(
        mys.starts_with("CREATE TABLE `t`") && mys.ends_with("PARTITION BY RANGE (d)"),
        "{mys}"
    );
}

/// G1：MySQL options 四键固定序 + comment 单引号翻倍（PG 直给 → 参数错误，见下测）。
#[test]
fn create_table_renders_mysql_options_in_fixed_order() {
    let my = ddl_diff(
        Dialect::MySql,
        &json!({"objectType":"table","name":"t","from":Value::Null,
                "to":{"columns":[{"name":"id","nativeType":"int"}],
                      "options":{"engine":"InnoDB","charset":"utf8mb4",
                                 "collate":"utf8mb4_bin","comment":"it's 业务"}}}),
    )
    .unwrap();
    let s = my["statements"][0].as_str().unwrap();
    assert!(
        s.contains(
            "ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_bin COMMENT='it''s 业务'"
        ),
        "{s}"
    );
    // 部分键：只给 engine → 其余不渲染。
    let my2 = ddl_diff(
        Dialect::MySql,
        &json!({"objectType":"table","name":"t","from":Value::Null,
                "to":{"columns":[{"name":"id","nativeType":"int"}],"options":{"engine":"MyISAM"}}}),
    )
    .unwrap();
    let s2 = my2["statements"][0].as_str().unwrap();
    assert!(
        s2.contains("ENGINE=MyISAM") && !s2.contains("CHARSET"),
        "{s2}"
    );
}

/// G1：PG 结构出现 options → 参数错误（fail-loud，不静默忽略）。
#[test]
fn pg_options_is_parameter_error() {
    let err = ddl_diff(
        Dialect::Postgres,
        &json!({"objectType":"table","name":"t","from":Value::Null,
                "to":{"columns":[{"name":"id","nativeType":"int4"}],
                      "options":{"engine":"InnoDB"}}}),
    )
    .unwrap_err();
    assert!(err.contains("options"), "{err}");
}

/// G1：partitionBy 带 NUL → 参数错误。
#[test]
fn partition_by_nul_rejected() {
    let err = ddl_diff(
        Dialect::Postgres,
        &json!({"objectType":"table","name":"t","from":Value::Null,
                "to":{"columns":[{"name":"d","nativeType":"date"}],
                      "partitionBy":"a\u{0}b"}}),
    )
    .unwrap_err();
    assert!(err.contains("NUL"), "{err}");
}

/// G3（REQ-09 §4.4）：view from=null → 单条 CREATE VIEW（两方言）。
#[test]
fn view_create_from_null() {
    for (d, q) in [(Dialect::Postgres, "\""), (Dialect::MySql, "`")] {
        let out = ddl_diff(
            d,
            &json!({"objectType":"view","name":"v","from":Value::Null,
                    "to":{"definition":"SELECT 1 AS a"}}),
        )
        .unwrap();
        assert_eq!(
            out["statements"][0],
            json!(format!("CREATE VIEW {q}v{q} AS SELECT 1 AS a"))
        );
        assert_eq!(out["warnings"], json!([]));
    }
}

/// G3：view definition 差异 → PG CREATE OR REPLACE / MySQL ALTER VIEW + diff.definition。
#[test]
fn view_definition_diff_and_drop() {
    let mk = |def: &str| {
        json!({"objectType":"view","name":"v",
                "from":{"definition":"SELECT 1"},"to":{"definition":def}})
    };
    let pg = ddl_diff(Dialect::Postgres, &mk("SELECT 2")).unwrap();
    assert_eq!(
        pg["statements"][0],
        json!("CREATE OR REPLACE VIEW \"v\" AS SELECT 2")
    );
    assert_eq!(
        pg["diff"]["definition"],
        json!({"from":"SELECT 1","to":"SELECT 2"})
    );
    let my = ddl_diff(Dialect::MySql, &mk("SELECT 2")).unwrap();
    assert!(
        my["statements"][0]
            .as_str()
            .unwrap()
            .starts_with("ALTER VIEW `v` AS "),
        "{my}"
    );
    // 相同 → 零语句。
    let same = ddl_diff(Dialect::Postgres, &mk("SELECT 1")).unwrap();
    assert_eq!(same["statements"], json!([]));
    // drop（to=null）→ DROP VIEW。
    let drop = ddl_diff(
        Dialect::Postgres,
        &json!({"objectType":"view","name":"v",
                "from":{"definition":"SELECT 1"},"to":Value::Null}),
    )
    .unwrap();
    assert_eq!(drop["statements"][0], json!("DROP VIEW \"v\""));
}

/// G3：index/sequence/routine/trigger create（definition 原样）+ drop（PG 词法）。
#[test]
fn other_object_types_create_and_drop() {
    let cases = [
        (
            "index",
            "CREATE INDEX ix ON t (a)",
            "DROP INDEX \"x\"",
            json!({}),
        ),
        (
            "sequence",
            "CREATE SEQUENCE s START 100",
            "DROP SEQUENCE \"x\"",
            json!({}),
        ),
        (
            "routine",
            "CREATE FUNCTION f() RETURNS int AS $$ SELECT 1 $$",
            "DROP FUNCTION \"x\"()",
            json!({"type":"function","args":""}),
        ),
        (
            "trigger",
            "CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION f()",
            "DROP TRIGGER \"x\"",
            json!({}),
        ),
    ];
    for (ty, def, drop_sql, extra) in cases {
        let from = if extra.as_object().map(|m| !m.is_empty()).unwrap_or(false) {
            json!({"definition":def, "type":extra["type"], "args":extra["args"]})
        } else {
            json!({"definition":def})
        };
        let out = ddl_diff(
            Dialect::Postgres,
            &json!({"objectType":ty,"name":"x","from":Value::Null,"to":{"definition":def}}),
        )
        .unwrap();
        assert_eq!(out["statements"][0], json!(def), "{ty}");
        let d = ddl_diff(
            Dialect::Postgres,
            &json!({"objectType":ty,"name":"x","from":from,"to":Value::Null}),
        )
        .unwrap();
        assert_eq!(d["statements"][0], json!(drop_sql), "{ty}");
    }
}

/// G3：边界——未知 objectType / 缺 definition / table to:null / MySQL index drop 缺 table。
#[test]
fn object_type_boundaries() {
    assert!(
        ddl_diff(
            Dialect::Postgres,
            &json!({"objectType":"materialized","name":"x",
                "from":Value::Null,"to":{}})
        )
        .is_err()
    );
    assert!(
        ddl_diff(
            Dialect::Postgres,
            &json!({"objectType":"view","name":"v",
                "from":Value::Null,"to":{}})
        )
        .is_err(),
        "缺 definition 参数错误"
    );
    assert!(
        ddl_diff(
            Dialect::Postgres,
            &json!({"objectType":"table","name":"t",
                "from":{"columns":[]},"to":Value::Null})
        )
        .is_err(),
        "table to:null 参数错误"
    );
    assert!(
        ddl_diff(
            Dialect::MySql,
            &json!({"objectType":"index","name":"ix",
                "from":{"definition":"CREATE INDEX ix ON t (a)"},"to":Value::Null})
        )
        .is_err(),
        "mysql index drop 缺 table 参数错误"
    );
}

/// G4（REQ-09 §4.1 表改名缺口）：表×列 rename 叠加——表 RENAME 首条、列 RENAME
/// 次之且用新表名（Review Focus #5）。
#[test]
fn table_rename_plus_column_rename() {
    let (pg, my) = both(
        json!({"objectType":"table","name":"t_new","previousName":"t_old",
            "from":{"columns":[{"name":"a","nativeType":"int4"}]},
            "to":{"columns":[{"name":"b","nativeType":"int4","previousName":"a"}]}}),
    );
    let ss = pg["statements"].as_array().unwrap();
    assert_eq!(ss[0], json!("ALTER TABLE \"t_old\" RENAME TO \"t_new\""));
    assert_eq!(
        ss[1],
        json!("ALTER TABLE \"t_new\" RENAME COLUMN \"a\" TO \"b\"")
    );
    let ss = my["statements"].as_array().unwrap();
    assert_eq!(ss[0], json!("ALTER TABLE `t_old` RENAME TO `t_new`"));
    assert!(
        ss[1]
            .as_str()
            .unwrap()
            .contains("ALTER TABLE `t_new` RENAME COLUMN `a` TO `b`"),
        "{ss:?}"
    );
}

/// G4：表 previousName → ALTER TABLE RENAME TO 首条 + diff.renamed；无 previousName
/// 时 renamed 恒为空数组、无 RENAME 语句（既有行为零变化）。
#[test]
fn table_rename_renders_first_and_records_renamed() {
    let (pg, my) = both(
        json!({"objectType":"table","name":"t_new","previousName":"t_old",
            "from":{"columns":[{"name":"id","nativeType":"int4"}]},
            "to":{"columns":[{"name":"id","nativeType":"int4"}]}}),
    );
    assert_eq!(
        pg["statements"][0],
        json!("ALTER TABLE \"t_old\" RENAME TO \"t_new\"")
    );
    assert_eq!(
        pg["diff"]["renamed"],
        json!([{"from":"t_old","to":"t_new"}])
    );
    assert_eq!(
        my["statements"][0],
        json!("ALTER TABLE `t_old` RENAME TO `t_new`")
    );
    // 其余语句用新名渲染。
    let (pg2, _) = both(
        json!({"objectType":"table","name":"t_new","previousName":"t_old",
            "from":{"columns":[{"name":"id","nativeType":"int4"}]},
            "to":{"columns":[{"name":"id","nativeType":"int8"}]}}),
    );
    assert!(
        pg2["statements"][1].as_str().unwrap().contains("\"t_new\""),
        "{pg2}"
    );
    // 无 previousName → 零回归。
    let (pg3, _) = both(json!({"objectType":"table","name":"t",
            "from":{"columns":[{"name":"id","nativeType":"int4"}]},
            "to":{"columns":[{"name":"id","nativeType":"int4"}]}}));
    assert_eq!(pg3["diff"]["renamed"], json!([]));
    assert!(
        pg3["statements"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| !s.as_str().unwrap_or("").contains("RENAME")),
        "{pg3}"
    );
}

/// G4：schema 段 + 表改名——旧引用两段限定，但 RENAME TO 目标**只许裸标识符**
/// （评审 I-1：PG/MySQL 同律，跨 schema 移动是 SET SCHEMA / RENAME TABLE 的另语义）。
#[test]
fn table_rename_with_schema() {
    let (pg, my) = both(
        json!({"objectType":"table","schema":"public","name":"t_new",
            "previousName":"t_old",
            "from":{"columns":[{"name":"id","nativeType":"int4"}]},
            "to":{"columns":[{"name":"id","nativeType":"int4"}]}}),
    );
    assert_eq!(
        pg["statements"][0],
        json!("ALTER TABLE \"public\".\"t_old\" RENAME TO \"t_new\"")
    );
    assert_eq!(
        my["statements"][0],
        json!("ALTER TABLE `public`.`t_old` RENAME TO `t_new`")
    );
}

/// 评审 I-2：MySQL routine drop 不接受签名（PG 专属语法）——MySQL 分支渲染
/// 裸 `DROP FUNCTION/PROCEDURE name`，args 仅 PG 需要。
#[test]
fn mysql_routine_drop_omits_signature() {
    let d = ddl_diff(
        Dialect::MySql,
        &json!({"objectType":"routine","name":"f",
                "from":{"definition":"CREATE FUNCTION f() RETURNS int RETURN 1",
                        "type":"function","args":""},
                "to":Value::Null}),
    )
    .unwrap();
    assert_eq!(d["statements"][0], json!("DROP FUNCTION `f`"));
    let d2 = ddl_diff(
        Dialect::MySql,
        &json!({"objectType":"routine","name":"p",
                "from":{"definition":"CREATE PROCEDURE p() BEGIN END",
                        "type":"procedure","args":""},
                "to":Value::Null}),
    )
    .unwrap();
    assert_eq!(d2["statements"][0], json!("DROP PROCEDURE `p`"));
}

/// 评审 minors：options 非对象/值校验/Null 规范化/MySQL 序列/object 信封 renamed 键。
#[test]
fn review_minors_options_and_object_envelope() {
    // options 非对象值 → 参数错误（原静默忽略）。
    let err = ddl_diff(
        Dialect::MySql,
        &json!({"objectType":"table","name":"t","from":Value::Null,
                "to":{"columns":[{"name":"id","nativeType":"int"}],"options":"InnoDB"}}),
    )
    .unwrap_err();
    assert!(err.contains("options"), "{err}");
    // options 值带 NUL → 参数错误（语句体校验）。
    let err = ddl_diff(
        Dialect::MySql,
        &json!({"objectType":"table","name":"t","from":Value::Null,
                "to":{"columns":[{"name":"id","nativeType":"int"}],
                      "options":{"engine":"a\u{0}b"}}}),
    )
    .unwrap_err();
    assert!(err.contains("NUL"), "{err}");
    // G2 判等 Null 规范化：{"comment":null} 与缺键不得误报 table_options。
    let spec = json!({"objectType":"table","name":"t",
            "from":{"columns":[{"name":"id","nativeType":"int"}],
                    "options":{"engine":"InnoDB","comment":null}},
            "to":{"columns":[{"name":"id","nativeType":"int"}],
                  "options":{"engine":"InnoDB"}}});
    assert_eq!(
        unsupported_capability_from_spec(Dialect::MySql, &spec),
        None
    );
    // MySQL 无序列 → drop sequence 参数错误。
    let err = ddl_diff(
        Dialect::MySql,
        &json!({"objectType":"sequence","name":"s",
                "from":{"definition":"CREATE SEQUENCE s"},"to":Value::Null}),
    )
    .unwrap_err();
    assert!(err.contains("sequence"), "{err}");
    // object 路径信封 schema 稳定：renamed 键恒在。
    let out = ddl_diff(
        Dialect::Postgres,
        &json!({"objectType":"view","name":"v","from":Value::Null,
                "to":{"definition":"SELECT 1"}}),
    )
    .unwrap();
    assert_eq!(out["diff"]["renamed"], json!([]));
}

/// 缺省 = 不参与 diff（零语句）；空数组 = DROP 全量。
#[test]
fn absent_field_means_skip_empty_array_means_drop_all() {
    let from = json!({
        "columns": [{"name": "id", "nativeType": "integer", "nullable": false, "primaryKey": true}],
        "indexes": [{"name": "ix_a", "unique": false, "primary": false, "origin": "c",
                     "columns": ["id"]}],
        "constraints": [{"name": "uq_a", "type": "uq", "definition": "UNIQUE (id)", "columns": ["id"]}],
        "foreignKeys": [{"name": "fk_a", "columns": ["id"], "refTable": "p", "refColumns": ["id"]}],
    });
    // to 缺省 indexes/constraints/foreignKeys → 不参与 diff，零 DROP。
    let spec_skip = json!({
        "objectType": "table", "name": "t", "from": from.clone(),
        "to": {"columns": from["columns"].clone()},
    });
    let (pg, my) = both(spec_skip);
    assert_eq!(pg["statements"], json!([]));
    assert_eq!(my["statements"], json!([]));
    // to 显式空数组 → DROP 全量。
    let spec_drop = json!({
        "objectType": "table", "name": "t", "from": from.clone(),
        "to": {"columns": from["columns"].clone(), "indexes": [], "constraints": [],
               "foreignKeys": []},
    });
    let (pg, _) = both(spec_drop.clone());
    let ss = pg["statements"].as_array().unwrap();
    let text: String = ss
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("DROP CONSTRAINT \"fk_a\""), "{text}");
    assert!(text.contains("DROP CONSTRAINT \"uq_a\""), "{text}");
    assert!(text.contains("DROP INDEX \"ix_a\""), "{text}");
    // 顺序：FK → 约束 → 索引。
    let i_fk = text.find("fk_a").unwrap();
    let i_uq = text.find("uq_a").unwrap();
    let i_ix = text.find("ix_a").unwrap();
    assert!(i_fk < i_uq && i_uq < i_ix, "{text}");
    let (_, my2) = both(spec_drop);
    let text2: String = my2["statements"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text2.contains("DROP FOREIGN KEY `fk_a`"), "{text2}");
    assert!(text2.contains("DROP INDEX `ix_a`"), "{text2}");
}

/// REQ-13：previousName 命中 → 单条 RENAME COLUMN，无 DROP/ADD；
/// changed 携带 name 条目，diff.columns.renamed 记录对子。
#[test]
fn rename_produces_single_statement_pg_mysql() {
    let spec = json!({"objectType": "table", "name": "t",
            "from": {"columns": [{"name": "a", "nativeType": "int4"}]},
            "to": {"columns": [{"name": "b", "nativeType": "int4", "previousName": "a"}]}});
    let (pg, my) = both(spec);
    assert_eq!(
        pg["statements"],
        json!(["ALTER TABLE \"t\" RENAME COLUMN \"a\" TO \"b\""])
    );
    assert_eq!(
        my["statements"],
        json!(["ALTER TABLE `t` RENAME COLUMN `a` TO `b`"])
    );
    assert_eq!(
        pg["diff"]["columns"]["renamed"],
        json!([{"from": "a", "to": "b"}])
    );
    assert!(
        pg["diff"]["columns"]["changed"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e == &json!({"name": "b", "field": "name", "from": "a", "to": "b"}))
    );
    assert_eq!(pg["warnings"], json!([]));
}

/// REQ-13 §4.3：六种边界——错必拒（400 文案），previousName===name 视为普通列。
#[test]
fn rename_boundary_errors() {
    let from1 = json!({"columns": [{"name": "a", "nativeType": "int4"}]});
    // previousName 不在 from。
    let e = ddl_diff(
        Dialect::Postgres,
        &json!({
            "objectType": "table", "name": "t", "from": from1.clone(),
            "to": {"columns": [{"name": "b", "nativeType": "int4", "previousName": "x"}]}}),
    )
    .unwrap_err();
    assert!(e.contains("previousName") && e.contains("not found"), "{e}");
    // 两个 to 列指向同一 previousName。
    let e = ddl_diff(Dialect::Postgres, &json!({
            "objectType": "table", "name": "t",
            "from": {"columns": [{"name": "a", "nativeType": "int4"}, {"name": "c", "nativeType": "int4"}]},
            "to": {"columns": [{"name": "b", "nativeType": "int4", "previousName": "a"},
                               {"name": "d", "nativeType": "int4", "previousName": "a"}]}}))
        .unwrap_err();
    assert!(e.contains("previousName"), "{e}");
    // 互换 a↔b。
    let e = ddl_diff(Dialect::Postgres, &json!({
            "objectType": "table", "name": "t",
            "from": {"columns": [{"name": "a", "nativeType": "int4"}, {"name": "b", "nativeType": "int4"}]},
            "to": {"columns": [{"name": "b", "nativeType": "int4", "previousName": "a"},
                               {"name": "a", "nativeType": "int4", "previousName": "b"}]}}))
        .unwrap_err();
    assert!(e.contains("ddl_diff_rename_swap"), "{e}");
    // 目标名在 from 已存在（未被换走）。
    let e = ddl_diff(Dialect::Postgres, &json!({
            "objectType": "table", "name": "t",
            "from": {"columns": [{"name": "a", "nativeType": "int4"}, {"name": "b", "nativeType": "int4"}]},
            "to": {"columns": [{"name": "b", "nativeType": "int4", "previousName": "a"}]}}))
        .unwrap_err();
    assert!(e.contains("conflict"), "{e}");
    // to 既有 previousName:"a" 的列，又有一列 name:"a"。
    let e = ddl_diff(Dialect::Postgres, &json!({
            "objectType": "table", "name": "t",
            "from": {"columns": [{"name": "a", "nativeType": "int4"}, {"name": "c", "nativeType": "int4"}]},
            "to": {"columns": [{"name": "b", "nativeType": "int4", "previousName": "a"},
                               {"name": "a", "nativeType": "int4"}]}}))
        .unwrap_err();
    assert!(e.contains("occupies"), "{e}");
    // previousName === name → 普通列（OK，无 rename 语句）。
    let (pg, _) = both(json!({
            "objectType": "table", "name": "t", "from": from1.clone(),
            "to": {"columns": [{"name": "a", "nativeType": "int8", "previousName": "a"}]}}));
    let text = pg["statements"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!text.contains("RENAME"), "{text}");
}

/// REQ-13 §4.4：同型一删一增（无 previousName）→ warnings 告警，语句仍 DROP+ADD。
#[test]
fn ambiguous_same_type_drop_add_warns() {
    let (pg, _) = both(json!({
            "objectType": "table", "name": "t",
            "from": {"columns": [{"name": "a", "nativeType": "int4"}]},
            "to": {"columns": [{"name": "b", "nativeType": "int4"}]}}));
    let warns = pg["warnings"].as_array().unwrap();
    assert_eq!(warns.len(), 1, "{warns:?}");
    let w = warns[0].as_str().unwrap();
    assert!(
        w.contains('a') && w.contains('b') && w.contains("数据"),
        "{w}"
    );
    let text = pg["statements"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("DROP COLUMN") && text.contains("ADD COLUMN"),
        "{text}"
    );
    // 干净结果：warnings 恒为空数组（schema 稳定）。
    let (clean, _) = both(json!({
            "objectType": "table", "name": "t",
            "from": {"columns": [{"name": "a", "nativeType": "int4"}]},
            "to": {"columns": [{"name": "a", "nativeType": "int4"}]}}));
    assert_eq!(clean["warnings"], json!([]));
}

/// REQ-13 §4.2：改名 + 改类型——RENAME 在先，变更语句用新名。
#[test]
fn rename_plus_type_change_orders_and_uses_new_name() {
    let (pg, my) = both(json!({
            "objectType": "table", "name": "t",
            "from": {"columns": [{"name": "a", "nativeType": "int4"}]},
            "to": {"columns": [{"name": "b", "nativeType": "int8", "previousName": "a"}]}}));
    let ss = pg["statements"].as_array().unwrap();
    assert_eq!(
        ss[0],
        json!("ALTER TABLE \"t\" RENAME COLUMN \"a\" TO \"b\"")
    );
    assert_eq!(
        ss[1],
        json!("ALTER TABLE \"t\" ALTER COLUMN \"b\" TYPE int8 USING \"b\"::int8")
    );
    let ss = my["statements"].as_array().unwrap();
    assert_eq!(ss[0], json!("ALTER TABLE `t` RENAME COLUMN `a` TO `b`"));
    assert!(
        ss[1].as_str().unwrap().contains("MODIFY COLUMN `b`"),
        "{ss:?}"
    );
}

/// REQ-13 §4.2：被索引/约束引用的列改名——依赖对象随 rename 走，不重建。
#[test]
fn rename_keeps_index_and_constraint() {
    let from = json!({
        "columns": [{"name": "a", "nativeType": "int4"}],
        "indexes": [{"name": "ix_a", "unique": false, "primary": false, "columns": ["a"]}],
        "constraints": [{"name": "uq_a", "type": "uq", "definition": "UNIQUE (a)", "columns": ["a"]}],
    });
    let to = json!({
        "columns": [{"name": "b", "nativeType": "int4", "previousName": "a"}],
        "indexes": [{"name": "ix_a", "unique": false, "primary": false, "columns": ["b"]}],
        "constraints": [{"name": "uq_a", "type": "uq", "definition": "UNIQUE (b)", "columns": ["b"]}],
    });
    let (pg, my) = both(json!({"objectType": "table", "name": "t", "from": from, "to": to}));
    for v in [pg, my] {
        let text = v["statements"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !text.contains("DROP INDEX") && !text.contains("DROP CONSTRAINT"),
            "{text}"
        );
        assert!(
            !text.contains("CREATE INDEX") && !text.contains("ADD CONSTRAINT"),
            "{text}"
        );
        assert!(text.contains("RENAME COLUMN"), "{text}");
    }
}

/// REQ-13 验收 #2/#8（env-gated 真库重放，PG）：rename 语句重放后数据原样、旧名消失。
#[tokio::test(flavor = "current_thread")]
async fn real_pg_rename_column_preserves_data() {
    let url = match std::env::var("OJ_TEST_PG") {
        Ok(u) if !u.is_empty() => u,
        _ => {
            eprintln!("skip: OJ_TEST_PG unset");
            return;
        }
    };
    let db = crate::bridge::SqlxAccessor::arc(&url).await.unwrap();
    for ddl in [
        "drop table if exists t_req13",
        "create table t_req13 (a int4, keep int)",
        "insert into t_req13 (a, keep) values (42, 7)",
    ] {
        db.exec_with_params(ddl, &[]).await.unwrap();
    }
    let spec = json!({"objectType": "table", "name": "t_req13",
            "from": {"columns": [{"name": "a", "nativeType": "int4"},
                                 {"name": "keep", "nativeType": "int"}]},
            "to": {"columns": [{"name": "b", "nativeType": "int4", "previousName": "a"},
                               {"name": "keep", "nativeType": "int"}]}});
    let out = ddl_diff(Dialect::Postgres, &spec).unwrap();
    let stmts: Vec<&str> = out["statements"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(stmts.len(), 1, "{out:?}");
    assert!(stmts[0].contains("RENAME COLUMN"), "{stmts:?}");
    for s in &stmts {
        db.exec_with_params(s, &[]).await.unwrap();
    }
    let rows = db
        .query_with_params("select b, keep from t_req13", &[])
        .await
        .unwrap();
    assert_eq!(rows[0]["b"], json!(42), "{rows:?}");
    assert_eq!(rows[0]["keep"], json!(7), "{rows:?}");
    assert!(
        db.query_with_params("select a from t_req13", &[])
            .await
            .is_err()
    );
    // 评审 minor 补齐：rename + 改类型真库重放（RENAME 在先、USING 新名）。
    let spec = json!({"objectType": "table", "name": "t_req13",
            "from": {"columns": [{"name": "b", "nativeType": "int4"},
                                 {"name": "keep", "nativeType": "int"}]},
            "to": {"columns": [{"name": "c", "nativeType": "int8", "previousName": "b"},
                               {"name": "keep", "nativeType": "int"}]}});
    let out = ddl_diff(Dialect::Postgres, &spec).unwrap();
    let stmts: Vec<&str> = out["statements"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(stmts.len(), 2, "{out:?}");
    assert!(stmts[0].contains("RENAME COLUMN"), "{stmts:?}");
    assert!(stmts[1].contains("\"c\"::int8"), "{stmts:?}");
    for s in &stmts {
        db.exec_with_params(s, &[]).await.unwrap();
    }
    let rows = db
        .query_with_params("select c, keep from t_req13", &[])
        .await
        .unwrap();
    assert_eq!(rows[0]["c"], json!(42), "{rows:?}");
    assert!(
        db.query_with_params("select b from t_req13", &[])
            .await
            .is_err()
    );
    db.exec_with_params("drop table if exists t_req13", &[])
        .await
        .unwrap();
}

/// REQ-13 验收 #2/#8（env-gated 真库重放，MySQL 8.0+）：同上。
#[tokio::test(flavor = "current_thread")]
async fn real_mysql_rename_column_preserves_data() {
    let url = match std::env::var("OJ_TEST_MYSQL") {
        Ok(u) if !u.is_empty() => u,
        _ => {
            eprintln!("skip: OJ_TEST_MYSQL unset");
            return;
        }
    };
    let db = crate::bridge::SqlxAccessor::arc(&url).await.unwrap();
    for ddl in [
        "drop table if exists t_req13",
        "create table t_req13 (a int, keep int)",
        "insert into t_req13 (a, keep) values (42, 7)",
    ] {
        db.exec_with_params(ddl, &[]).await.unwrap();
    }
    let spec = json!({"objectType": "table", "name": "t_req13",
            "from": {"columns": [{"name": "a", "nativeType": "int"},
                                 {"name": "keep", "nativeType": "int"}]},
            "to": {"columns": [{"name": "b", "nativeType": "int", "previousName": "a"},
                               {"name": "keep", "nativeType": "int"}]}});
    let out = ddl_diff(Dialect::MySql, &spec).unwrap();
    let stmts: Vec<&str> = out["statements"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(stmts.len(), 1, "{out:?}");
    assert!(stmts[0].contains("RENAME COLUMN"), "{stmts:?}");
    for s in &stmts {
        db.exec_with_params(s, &[]).await.unwrap();
    }
    let rows = db
        .query_with_params("select b, keep from t_req13", &[])
        .await
        .unwrap();
    assert_eq!(rows[0]["b"], json!(42), "{rows:?}");
    assert_eq!(rows[0]["keep"], json!(7), "{rows:?}");
    assert!(
        db.query_with_params("select a from t_req13", &[])
            .await
            .is_err()
    );
    // 评审 minor 补齐：rename + 改类型真库重放（RENAME 在先、MODIFY 用新名）。
    let spec = json!({"objectType": "table", "name": "t_req13",
            "from": {"columns": [{"name": "b", "nativeType": "int"},
                                 {"name": "keep", "nativeType": "int"}]},
            "to": {"columns": [{"name": "c", "nativeType": "bigint", "previousName": "b"},
                               {"name": "keep", "nativeType": "int"}]}});
    let out = ddl_diff(Dialect::MySql, &spec).unwrap();
    let stmts: Vec<&str> = out["statements"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(stmts.len(), 2, "{out:?}");
    assert!(stmts[0].contains("RENAME COLUMN"), "{stmts:?}");
    assert!(stmts[1].contains("MODIFY COLUMN `c`"), "{stmts:?}");
    for s in &stmts {
        db.exec_with_params(s, &[]).await.unwrap();
    }
    let rows = db
        .query_with_params("select c, keep from t_req13", &[])
        .await
        .unwrap();
    assert_eq!(rows[0]["c"], json!(42), "{rows:?}");
    assert!(
        db.query_with_params("select b from t_req13", &[])
            .await
            .is_err()
    );
    db.exec_with_params("drop table if exists t_req13", &[])
        .await
        .unwrap();
}

/// G4（REQ-09 §4.1，env-gated 真库重放）：表 previousName → RENAME TO 重放后
/// 新表可读、旧表消失、数据保留。
#[tokio::test(flavor = "current_thread")]
async fn real_pg_table_rename_replays() {
    let url = match std::env::var("OJ_TEST_PG") {
        Ok(u) if !u.is_empty() => u,
        _ => {
            eprintln!("skip: OJ_TEST_PG unset");
            return;
        }
    };
    let db = crate::bridge::SqlxAccessor::arc(&url).await.unwrap();
    for ddl in [
        "drop table if exists t_g4_old",
        "drop table if exists t_g4_new",
        "create table t_g4_old (id int4)",
        "insert into t_g4_old (id) values (7)",
    ] {
        db.exec_with_params(ddl, &[]).await.unwrap();
    }
    let spec = json!({"objectType":"table","schema":"public","name":"t_g4_new",
            "previousName":"t_g4_old",
            "from":{"columns":[{"name":"id","nativeType":"int4"}]},
            "to":{"columns":[{"name":"id","nativeType":"int4"}]}});
    let out = ddl_diff(Dialect::Postgres, &spec).unwrap();
    let stmts: Vec<&str> = out["statements"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(stmts.len(), 1, "{out:?}");
    assert!(stmts[0].contains("RENAME TO"), "{stmts:?}");
    for s in &stmts {
        db.exec_with_params(s, &[]).await.unwrap();
    }
    let rows = db
        .query_with_params("select id from t_g4_new", &[])
        .await
        .unwrap();
    assert_eq!(rows[0]["id"], json!(7), "{rows:?}");
    assert!(
        db.query_with_params("select id from t_g4_old", &[])
            .await
            .is_err()
    );
    db.exec_with_params("drop table if exists t_g4_new", &[])
        .await
        .unwrap();
}

/// G4（env-gated 真库重放，MySQL 8.0+）：同上。
#[tokio::test(flavor = "current_thread")]
async fn real_mysql_table_rename_replays() {
    let url = match std::env::var("OJ_TEST_MYSQL") {
        Ok(u) if !u.is_empty() => u,
        _ => {
            eprintln!("skip: OJ_TEST_MYSQL unset");
            return;
        }
    };
    let db = crate::bridge::SqlxAccessor::arc(&url).await.unwrap();
    for ddl in [
        "drop table if exists t_g4_old",
        "drop table if exists t_g4_new",
        "create table t_g4_old (id int)",
        "insert into t_g4_old (id) values (7)",
    ] {
        db.exec_with_params(ddl, &[]).await.unwrap();
    }
    let spec = json!({"objectType":"table","name":"t_g4_new","previousName":"t_g4_old",
            "from":{"columns":[{"name":"id","nativeType":"int"}]},
            "to":{"columns":[{"name":"id","nativeType":"int"}]}});
    let out = ddl_diff(Dialect::MySql, &spec).unwrap();
    let stmts: Vec<&str> = out["statements"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(stmts.len(), 1, "{out:?}");
    assert!(stmts[0].contains("RENAME TO"), "{stmts:?}");
    for s in &stmts {
        db.exec_with_params(s, &[]).await.unwrap();
    }
    let rows = db
        .query_with_params("select id from t_g4_new", &[])
        .await
        .unwrap();
    assert_eq!(rows[0]["id"], json!(7), "{rows:?}");
    assert!(
        db.query_with_params("select id from t_g4_old", &[])
            .await
            .is_err()
    );
    db.exec_with_params("drop table if exists t_g4_new", &[])
        .await
        .unwrap();
}

/// 畸形 Structure → 参数错误（Err），不 panic。
#[test]
fn malformed_structure_is_parameter_error() {
    for (label, spec) in [
        (
            "bad objectType",
            json!({"objectType": "view", "name": "t", "to": {"columns": []}}),
        ),
        (
            "missing name",
            json!({"objectType": "table", "to": {"columns": []}}),
        ),
        (
            "col missing name",
            json!({"objectType": "table", "name": "t",
                "from": null,
                "to": {"columns": [{"nativeType": "int"}]}}),
        ),
        (
            "col missing nativeType",
            json!({"objectType": "table", "name": "t",
                "from": null,
                "to": {"columns": [{"name": "c"}]}}),
        ),
        (
            "unknown constraint type",
            json!({"objectType": "table", "name": "t",
                "from": {"columns": []},
                "to": {"columns": [], "constraints": [{"name": "c", "type": "zz", "definition": "x"}]}}),
        ),
        (
            "NUL in identifier",
            json!({"objectType": "table", "name": "a\u{0}b",
                "from": {"columns": []}, "to": {"columns": []}}),
        ),
    ] {
        let r = ddl_diff(Dialect::Postgres, &spec);
        assert!(r.is_err(), "{label}: 须报错: {r:?}");
        assert!(r.unwrap_err().contains("ddlDiff"), "{label}");
    }
}

/// 引号翻倍 + NUL 拒绝（不做字符集白名单：合法名可含非 ASCII/空格）。
#[test]
fn quoting_doubles_and_rejects_nul() {
    assert_eq!(
        quote_ident(Dialect::Postgres, "a\"b").unwrap(),
        "\"a\"\"b\""
    );
    assert_eq!(quote_ident(Dialect::MySql, "a`b").unwrap(), "`a``b`");
    assert_eq!(
        quote_ident(Dialect::Postgres, "我的 table").unwrap(),
        "\"我的 table\""
    );
    assert!(quote_ident(Dialect::Postgres, "a\nb").is_err());
    assert!(quote_ident(Dialect::Postgres, "").is_err());
}

/// verbatim 片段拒 `;`（语句拆分防线）。
#[test]
fn fragments_reject_semicolon() {
    assert!(check_fragment("integer").is_ok());
    assert!(check_fragment("nextval('s'::regclass)").is_ok());
    assert!(check_fragment("integer; drop table t").is_err());
    assert!(check_fragment("a\nb").is_err());
}

/// identity 恒等时跳 default 文本比对（nextval 派生，否则恒假 changed）。
#[test]
fn identity_column_skips_default_compare() {
    let spec = json!({
        "objectType": "table", "name": "t",
        "from": {"columns": [{"name": "id", "nativeType": "integer", "nullable": false,
                              "identity": "always",
                              "default": "nextval('t_id_seq'::regclass)", "primaryKey": true}]},
        "to": {"columns": [{"name": "id", "nativeType": "integer", "nullable": false,
                            "primaryKey": true, "identity": "always"}]},
    });
    let pg = ddl_diff(Dialect::Postgres, &spec).unwrap();
    assert_eq!(pg["diff"]["columns"]["changed"], json!([]), "{pg:?}");
}

/// identity 翻转（加/去）→ changed 出 identity 项，PG 产 ADD/DROP IDENTITY 语句
///（spec 列 identity 为 changedColumns 字段之一；静默缺失 = 假 diff，禁止）。
#[test]
fn identity_flip_emits_changed_and_pg_statement() {
    let plain = json!({"columns": [{"name": "id", "nativeType": "integer",
                                        "nullable": false, "primaryKey": true}]});
    let ident = json!({"columns": [{"name": "id", "nativeType": "integer",
                                        "nullable": false, "primaryKey": true,
                                        "identity": "always"}]});
    let up = ddl_diff(
        Dialect::Postgres,
        &json!({"objectType": "table", "name": "t", "from": plain.clone(), "to": ident.clone()}),
    )
    .unwrap();
    let ch = up["diff"]["columns"]["changed"].as_array().unwrap();
    assert!(
        ch.iter()
            .any(|e| e["field"] == json!("identity") && e["to"] == json!("always")),
        "{ch:?}"
    );
    let text: String = up["statements"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        text.contains("ALTER COLUMN \"id\" ADD GENERATED ALWAYS AS IDENTITY"),
        "{text}"
    );
    let down = ddl_diff(
        Dialect::Postgres,
        &json!({"objectType": "table", "name": "t", "from": ident, "to": plain}),
    )
    .unwrap();
    let text2: String = down["statements"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        text2.contains("ALTER COLUMN \"id\" DROP IDENTITY"),
        "{text2}"
    );
}

/// identity 是 PG 专属能力：MySQL 结构里出现 identity → 参数错误（不产非法 SQL）。
#[test]
fn mysql_rejects_identity_columns() {
    let spec = json!({
        "objectType": "table", "name": "t", "from": null,
        "to": {"columns": [{"name": "id", "nativeType": "int", "nullable": false,
                            "primaryKey": true, "identity": "always"}]},
    });
    let r = ddl_diff(Dialect::MySql, &spec);
    assert!(r.is_err(), "mysql 须拒 identity: {r:?}");
    assert!(r.unwrap_err().contains("identity"), "错误须点名 identity");
}

/// 列维度一端缺省 = 不参与 diff：绝不许全表 ADD/DROP COLUMN（数据安全）。
#[test]
fn absent_columns_dimension_means_no_column_add_drop() {
    let from = json!({
        "columns": [{"name": "a", "nativeType": "int", "nullable": true}],
        "indexes": [{"name": "ix_a", "unique": false, "primary": false,
                     "origin": "c", "columns": ["a"]}],
    });
    // to 缺省 columns → 列维度不参与：只有 DROP INDEX，零列语句。
    let spec = json!({"objectType": "table", "name": "t", "from": from.clone(),
                          "to": {"indexes": []}});
    let pg = ddl_diff(Dialect::Postgres, &spec).unwrap();
    let ss: Vec<&str> = pg["statements"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(ss.len(), 1, "{ss:?}");
    assert!(ss[0].contains("DROP INDEX \"ix_a\""), "{ss:?}");
    assert_eq!(pg["diff"]["columns"]["added"], json!([]), "{pg:?}");
    assert_eq!(pg["diff"]["columns"]["removed"], json!([]), "{pg:?}");
    // from 缺省 columns（to 有列）→ 同样不参与：零列语句。
    let spec2 = json!({"objectType": "table", "name": "t",
                           "from": {"indexes": []},
                           "to": {"columns": [{"name": "b", "nativeType": "int", "nullable": true}],
                                  "indexes": []}});
    let pg2 = ddl_diff(Dialect::Postgres, &spec2).unwrap();
    let text2: String = pg2["statements"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(!text2.contains("COLUMN"), "{text2}");
}

/// 同名 changed 约束/索引/外键：先 DROP 再 ADD（否则重放撞已存在对象必炸）。
#[test]
fn changed_named_dims_drop_before_add() {
    let from = json!({
        "columns": [{"name": "id", "nativeType": "int", "nullable": false}],
        "constraints": [{"name": "ck_v", "type": "ck", "definition": "CHECK (id > 0)",
                         "columns": []}],
        "indexes": [{"name": "ix_id", "unique": false, "primary": false,
                     "origin": "c", "columns": ["id"]}],
        "foreignKeys": [{"name": "fk_p", "columns": ["id"], "refTable": "p",
                         "refColumns": ["id"]}],
    });
    let to = json!({
        "columns": [{"name": "id", "nativeType": "int", "nullable": false}],
        "constraints": [{"name": "ck_v", "type": "ck", "definition": "CHECK (id > 10)",
                         "columns": []}],
        "indexes": [{"name": "ix_id", "unique": true, "primary": false,
                     "origin": "c", "columns": ["id"]}],
        "foreignKeys": [{"name": "fk_p", "columns": ["id"], "refTable": "q",
                         "refColumns": ["id"]}],
    });
    let pg = ddl_diff(
        Dialect::Postgres,
        &json!({"objectType": "table", "name": "t", "from": from, "to": to}),
    )
    .unwrap();
    let ss: Vec<&str> = pg["statements"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let pos = |needle: &str| {
        ss.iter()
            .position(|s| s.contains(needle))
            .unwrap_or(usize::MAX)
    };
    assert!(
        pos("DROP CONSTRAINT \"ck_v\"") < pos("ADD CONSTRAINT \"ck_v\" CHECK (id > 10)"),
        "{ss:?}"
    );
    assert!(
        pos("DROP INDEX \"ix_id\"") < pos("CREATE UNIQUE INDEX \"ix_id\""),
        "{ss:?}"
    );
    assert!(
        pos("DROP CONSTRAINT \"fk_p\"") < pos("ADD CONSTRAINT \"fk_p\" FOREIGN KEY"),
        "{ss:?}"
    );
}

/// MySQL 约束 DROP 按类型分派：pk → DROP PRIMARY KEY；uq → DROP INDEX；ck → DROP CHECK。
#[test]
fn mysql_constraint_drop_uses_type_specific_syntax() {
    let from = json!({
        "columns": [{"name": "id", "nativeType": "int", "nullable": false}],
        "constraints": [
            {"name": "pk_t", "type": "pk", "definition": "PRIMARY KEY (id)",
             "columns": ["id"]},
            {"name": "uq_a", "type": "uq", "definition": "UNIQUE (id)", "columns": ["id"]},
            {"name": "ck_a", "type": "ck", "definition": "CHECK (id > 0)", "columns": []},
        ],
    });
    let spec = json!({"objectType": "table", "name": "t", "from": from.clone(),
                          "to": {"columns": from["columns"].clone(), "constraints": []}});
    let my = ddl_diff(Dialect::MySql, &spec).unwrap();
    let text: String = my["statements"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(text.contains("ALTER TABLE `t` DROP PRIMARY KEY"), "{text}");
    assert!(text.contains("DROP INDEX `uq_a`"), "{text}");
    assert!(text.contains("DROP CHECK `ck_a`"), "{text}");
    assert!(!text.contains("DROP CHECK `pk_t`"), "{text}");
    assert!(!text.contains("DROP CHECK `uq_a`"), "{text}");
    // PG 仍统一 DROP CONSTRAINT。
    let pg = ddl_diff(Dialect::Postgres, &spec).unwrap();
    let ptext: String = pg["statements"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(ptext.contains("DROP CONSTRAINT \"pk_t\""), "{ptext}");
    assert!(ptext.contains("DROP CONSTRAINT \"uq_a\""), "{ptext}");
}

/// 列名 / 索引组成列名含 NUL → 参数错误（不许静默落成裸标识符）。
#[test]
fn nul_in_column_and_member_names_rejected() {
    let bad_col_alter = json!({
        "objectType": "table", "name": "t",
        "from": {"columns": [{"name": "a", "nativeType": "int", "nullable": true}]},
        "to": {"columns": [{"name": "a\u{0}b", "nativeType": "int", "nullable": true}]},
    });
    let r = ddl_diff(Dialect::Postgres, &bad_col_alter);
    let msg = r.unwrap_err();
    assert!(msg.contains("ddlDiff"), "{msg}");
    let bad_col_create = json!({
        "objectType": "table", "name": "t", "from": null,
        "to": {"columns": [{"name": "a\u{0}b", "nativeType": "int", "nullable": true}]},
    });
    let r2 = ddl_diff(Dialect::Postgres, &bad_col_create);
    assert!(r2.is_err(), "{r2:?}");
    let bad_member = json!({
        "objectType": "table", "name": "t",
        "from": {"columns": [{"name": "a", "nativeType": "int", "nullable": true}]},
        "to": {"columns": [{"name": "a", "nativeType": "int", "nullable": true}],
               "indexes": [{"name": "ix_a", "columns": ["a\u{0}b"]}]},
    });
    let r3 = ddl_diff(Dialect::Postgres, &bad_member);
    assert!(r3.is_err(), "{r3:?}");
}

/// 建表路径（from = null）diff 形状与 alter 路径一致：四维 added/removed/changed。
#[test]
fn create_path_diff_shape_matches_alter_path() {
    let spec = json!({
        "objectType": "table", "name": "t", "from": null,
        "to": {"columns": [{"name": "id", "nativeType": "int", "nullable": false,
                            "primaryKey": true}]},
    });
    let pg = ddl_diff(Dialect::Postgres, &spec).unwrap();
    for dim in ["columns", "constraints", "indexes", "foreignKeys"] {
        for side in ["added", "removed", "changed"] {
            assert!(pg["diff"][dim][side].is_array(), "{dim}.{side}: {pg:?}");
        }
    }
    assert_eq!(pg["diff"]["columns"]["added"], json!(["id"]), "{pg:?}");
}

/// 语句排序：DROP（FK → 约束 → 索引 → 列）→ 列变更 → ADD → 约束/索引 → FK 最后。
#[test]
fn statement_ordering_is_drop_then_alter_then_add_fk_last() {
    let from = json!({
        "columns": [
            {"name": "id", "nativeType": "integer", "nullable": false, "primaryKey": true},
            {"name": "old_c", "nativeType": "text", "nullable": true},
            {"name": "v", "nativeType": "integer", "nullable": true, "default": "0"},
        ],
        "constraints": [{"name": "ck_v", "type": "ck", "definition": "CHECK (v > 0)", "columns": []}],
        "indexes": [{"name": "ix_old", "unique": false, "primary": false, "origin": "c", "columns": ["old_c"]}],
        "foreignKeys": [{"name": "fk_old", "columns": ["id"], "refTable": "p", "refColumns": ["id"]}],
    });
    let to = json!({
        "columns": [
            {"name": "id", "nativeType": "integer", "nullable": false, "primaryKey": true},
            {"name": "v", "nativeType": "bigint", "nullable": false, "default": "1"},
            {"name": "new_c", "nativeType": "text", "nullable": true},
        ],
        "constraints": [{"name": "ck_v2", "type": "ck", "definition": "CHECK (v > 1)", "columns": []}],
        "indexes": [{"name": "ix_new", "unique": true, "primary": false, "origin": "c", "columns": ["new_c"]}],
        "foreignKeys": [{"name": "fk_new", "columns": ["v"], "refTable": "q", "refColumns": ["id"]}],
    });
    let (pg, _) = both(json!({"objectType": "table", "name": "t", "from": from, "to": to}));
    let ss: Vec<&str> = pg["statements"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let pos = |needle: &str| {
        ss.iter()
            .position(|s| s.contains(needle))
            .unwrap_or(usize::MAX)
    };
    assert!(pos("DROP CONSTRAINT \"fk_old\"") < pos("DROP CONSTRAINT \"ck_v\""));
    assert!(pos("DROP CONSTRAINT \"ck_v\"") < pos("DROP INDEX \"ix_old\""));
    assert!(pos("DROP INDEX \"ix_old\"") < pos("DROP COLUMN \"old_c\""));
    assert!(pos("DROP COLUMN \"old_c\"") < pos("ALTER COLUMN \"v\" TYPE bigint"));
    assert!(pos("ALTER COLUMN \"v\" SET NOT NULL") < pos("ADD COLUMN \"new_c\""));
    assert!(pos("ADD COLUMN \"new_c\"") < pos("ADD CONSTRAINT \"ck_v2\""));
    assert!(pos("ADD CONSTRAINT \"ck_v2\"") < pos("CREATE UNIQUE INDEX \"ix_new\""));
    assert!(pos("CREATE UNIQUE INDEX \"ix_new\"") < pos("ADD CONSTRAINT \"fk_new\""));
}

/// from = null → 建表：PG 双引号 / MySQL 反引号，主键内联，FK 最后。
#[test]
fn create_table_from_null_from() {
    let spec = json!({
        "objectType": "table", "name": "t", "from": null,
        "to": {
            "columns": [
                {"name": "id", "nativeType": "integer", "nullable": false, "primaryKey": true},
                {"name": "name", "nativeType": "text", "nullable": true},
            ],
            "constraints": [{"name": "ck_id", "type": "ck", "definition": "CHECK (id > 0)", "columns": []}],
            "foreignKeys": [{"name": "fk_p", "columns": ["id"], "refTable": "p", "refColumns": ["id"]}],
        },
    });
    let (pg, my) = both(spec);
    let pgs: Vec<&str> = pg["statements"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(pgs[0].contains("CREATE TABLE \"t\""), "{pgs:?}");
    assert!(pgs[0].contains("PRIMARY KEY (\"id\")"), "{pgs:?}");
    assert!(
        pgs.iter()
            .any(|s| s.contains("ADD CONSTRAINT \"ck_id\" CHECK (id > 0)"))
    );
    assert!(
        pgs.last()
            .unwrap()
            .contains("ADD CONSTRAINT \"fk_p\" FOREIGN KEY (\"id\") REFERENCES \"p\" (\"id\")")
    );
    let mys: Vec<&str> = my["statements"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(mys[0].contains("CREATE TABLE `t`"), "{mys:?}");
    assert!(mys[0].contains("PRIMARY KEY (`id`)"), "{mys:?}");
}

/// PG 改类型产 USING；MySQL 位置差产 AFTER/FIRST；PG 忽略位置。
#[test]
fn pg_alter_type_emits_using_and_mysql_position() {
    let from = json!({"columns": [
        {"name": "a", "nativeType": "integer", "nullable": true},
        {"name": "b", "nativeType": "text", "nullable": true},
    ]});
    let to = json!({"columns": [
        {"name": "b", "nativeType": "text", "nullable": true},
        {"name": "a", "nativeType": "bigint", "nullable": true},
    ]});
    let spec = json!({"objectType": "table", "name": "t", "from": from, "to": to});
    let (pg, my) = both(spec.clone());
    let pgs: String = pg["statements"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        pgs.contains("ALTER COLUMN \"a\" TYPE bigint USING \"a\"::bigint"),
        "{pgs}"
    );
    // PG 无列位置 DDL：changedColumns 无 position 项、无位置语句。
    assert!(!pgs.contains("FIRST") && !pgs.contains("AFTER"), "{pgs}");
    let pg_changed = pg["diff"]["columns"]["changed"].as_array().unwrap();
    assert!(
        !pg_changed.iter().any(|e| e["field"] == json!("position")),
        "{pg_changed:?}"
    );
    let mys: String = my["statements"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(mys.contains("AFTER `b`"), "{mys}");
    let my_changed = my["diff"]["columns"]["changed"].as_array().unwrap();
    assert!(
        my_changed.iter().any(|e| e["field"] == json!("position")),
        "{my_changed:?}"
    );
}

/// diff 结构形状：columns/constraints/indexes/foreignKeys 四维 added/removed/changed。
#[test]
fn diff_shape_has_all_dimensions() {
    let spec = json!({
        "objectType": "table", "name": "t",
        "from": {"columns": [{"name": "a", "nativeType": "int", "nullable": true}]},
        "to": {"columns": [{"name": "a", "nativeType": "int", "nullable": false},
                           {"name": "b", "nativeType": "text", "nullable": true}]},
    });
    let (pg, _) = both(spec);
    let d = &pg["diff"];
    for dim in ["columns", "constraints", "indexes", "foreignKeys"] {
        for side in ["added", "removed", "changed"] {
            assert!(d[dim][side].is_array(), "{dim}.{side} 缺: {d:?}");
        }
    }
    assert_eq!(d["columns"]["added"], json!(["b"]));
    assert_eq!(d["columns"]["removed"], json!([]));
    assert_eq!(d["columns"]["changed"][0]["field"], json!("nullable"));
}
