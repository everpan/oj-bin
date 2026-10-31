use super::super::db::Dialect;
use super::render::*;
use serde_json::json;

fn cols(names: &[&str]) -> Vec<String> {
    names.iter().map(|s| s.to_string()).collect()
}

fn two_rows() -> Vec<Vec<serde_json::Value>> {
    vec![vec![json!(1), json!("a")], vec![json!(2), json!("b")]]
}

#[test]
fn render_pg_append_multivalue() {
    let b = render_batch(
        Dialect::Postgres,
        "t",
        &cols(&["a", "b"]),
        &two_rows(),
        &ImportMode::Append,
        &[],
    )
    .unwrap();
    let flat: String = b.sql.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        flat.contains("INSERTINTO\"t\"(\"a\",\"b\")VALUES($1,$2),($3,$4)"),
        "{sql}",
        sql = b.sql
    );
    assert_eq!(b.params, vec![json!(1), json!("a"), json!(2), json!("b")]);
}

#[test]
fn render_pg_upsert_on_conflict_clause() {
    let rows3: Vec<Vec<serde_json::Value>> = vec![
        vec![json!(1), json!("a"), json!(10)],
        vec![json!(2), json!("b"), json!(20)],
    ];
    let b = render_batch(
        Dialect::Postgres,
        "t",
        &cols(&["a", "b", "c"]),
        &rows3,
        &ImportMode::Upsert {
            conflict_keys: cols(&["a"]),
        },
        &[],
    )
    .unwrap();
    assert!(
        b.sql.contains("ON CONFLICT (\"a\") DO UPDATE SET"),
        "{}",
        b.sql
    );
    // SET 只含非 key 列，excluded 引用正确
    assert!(b.sql.contains("\"b\" = \"excluded\".\"b\""), "{}", b.sql);
    assert!(b.sql.contains("\"c\" = \"excluded\".\"c\""), "{}", b.sql);
    assert!(!b.sql.contains("\"a\" = \"excluded\""), "{}", b.sql);
    // 参数与 append 同序（suffix 不碰绑定）
    assert_eq!(b.params.len(), 6);
}

#[test]
fn render_pg_upsert_all_keys_do_nothing() {
    let b = render_batch(
        Dialect::Postgres,
        "t",
        &cols(&["a"]),
        &[vec![json!(1)]],
        &ImportMode::Upsert {
            conflict_keys: cols(&["a"]),
        },
        &[],
    )
    .unwrap();
    assert!(
        b.sql.contains("ON CONFLICT (\"a\") DO NOTHING"),
        "{}",
        b.sql
    );
}

/// Final-fix 4（review Important）：未登记表的列/键是任意用户串——脏标识符
/// （空/控制字符）在 quote_ident 处必须落 Err，绝不 panic（panic 在 spawned
/// consumer 里 = 任务永卡 running + 配额槽泄漏）。
#[test]
fn render_upsert_dirty_identifier_is_error_not_panic() {
    let e = render_batch(
        Dialect::Postgres,
        "t",
        &cols(&["a", "bad\u{1}key"]),
        &two_rows(),
        &ImportMode::Upsert {
            conflict_keys: cols(&["a"]),
        },
        &[],
    )
    .unwrap_err();
    assert!(e.contains("identifier"), "{e}");
    // MySQL 同路径
    let e = render_batch(
        Dialect::MySql,
        "t",
        &cols(&["a", "bad\u{1}key"]),
        &two_rows(),
        &ImportMode::Upsert {
            conflict_keys: cols(&["a"]),
        },
        &cols(&["a"]),
    )
    .unwrap_err();
    assert!(e.contains("identifier"), "{e}");
}

#[test]
fn render_mysql_upsert_requires_pk() {
    let e = render_batch(
        Dialect::MySql,
        "t",
        &cols(&["a", "b"]),
        &two_rows(),
        &ImportMode::Upsert {
            conflict_keys: cols(&["a"]),
        },
        &[],
    )
    .unwrap_err();
    assert!(e.contains("requires a unique key"), "{e}");
    assert!(e.contains("'t'"), "{e}");
    // 有 pk → ON DUPLICATE KEY UPDATE 只含非 key 列
    let b = render_batch(
        Dialect::MySql,
        "t",
        &cols(&["a", "b"]),
        &two_rows(),
        &ImportMode::Upsert {
            conflict_keys: cols(&["a"]),
        },
        &cols(&["a"]),
    )
    .unwrap();
    assert!(b.sql.contains("ON DUPLICATE KEY UPDATE"), "{}", b.sql);
    assert!(b.sql.contains("`b` = VALUES(`b`)"), "{}", b.sql);
    assert!(!b.sql.contains("`a` = VALUES"), "{}", b.sql);
}

#[test]
fn render_sqlite_or_replace_only_rowid() {
    // 仅 conflict_keys == ["rowid"] 合法（spec §4.3）；rowid 须入列清单
    let rows3: Vec<Vec<serde_json::Value>> = vec![
        vec![json!(1), json!(1), json!("a")],
        vec![json!(2), json!(2), json!("b")],
    ];
    let b = render_batch(
        Dialect::Sqlite,
        "t",
        &cols(&["rowid", "a", "b"]),
        &rows3,
        &ImportMode::Upsert {
            conflict_keys: cols(&["rowid"]),
        },
        &[],
    )
    .unwrap();
    let flat: String = b.sql.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        flat.contains("INSERTORREPLACEINTO\"t\"(\"rowid\",\"a\",\"b\")VALUES(?,?,?)"),
        "{}",
        b.sql
    );
    assert_eq!(
        b.params,
        vec![
            json!(1),
            json!(1),
            json!("a"),
            json!(2),
            json!(2),
            json!("b")
        ]
    );
    // 非 rowid 键 → 参数错误
    let e = render_batch(
        Dialect::Sqlite,
        "t",
        &cols(&["rowid", "a", "b"]),
        &rows3,
        &ImportMode::Upsert {
            conflict_keys: cols(&["a"]),
        },
        &[],
    )
    .unwrap_err();
    assert!(e.contains("rowid"), "{e}");
}

#[test]
fn render_conflict_key_must_be_in_columns() {
    let e = render_batch(
        Dialect::Postgres,
        "t",
        &cols(&["a", "b"]),
        &two_rows(),
        &ImportMode::Upsert {
            conflict_keys: cols(&["nope"]),
        },
        &[],
    )
    .unwrap_err();
    assert!(e.contains("nope"), "{e}");
}

#[test]
fn batch_rows_caps_at_65535() {
    use super::super::db::Dialect;
    // ncols=10 → 每语句 ≤6553
    assert_eq!(batch_rows(Dialect::Postgres, usize::MAX, 5000, 10), 5000);
    assert_eq!(batch_rows(Dialect::Postgres, usize::MAX, 100_000, 10), 6553);
    // ncols=100 → ≤655
    assert_eq!(batch_rows(Dialect::Postgres, usize::MAX, 100_000, 100), 655);
    // 用户 batchSize 1_000_000 → 收敛不报错
    assert_eq!(
        batch_rows(Dialect::Postgres, usize::MAX, 1_000_000, 10),
        6553
    );
    // cap（剩余行数）优先
    assert_eq!(batch_rows(Dialect::Postgres, 5, 5000, 10), 5);
    assert!(batch_rows(Dialect::Postgres, 0, 5000, 10) == 0);
    // sqlite 变量上限 32766：7 列 → 4680；默认批 5000 必须被压下来
    assert_eq!(batch_rows(Dialect::Sqlite, usize::MAX, 5000, 7), 4680);
    assert_eq!(batch_rows(Dialect::Sqlite, usize::MAX, 100_000, 10), 3276);
    // mysql 同 65535
    assert_eq!(batch_rows(Dialect::MySql, usize::MAX, 100_000, 10), 6553);
}

#[tokio::test(flavor = "current_thread")]
async fn sqlite_real_multivalue_insert_and_or_replace() {
    use super::super::accessor_sqlx::SqlxAccessor;
    use super::super::db::DataAccessor;
    let db = SqlxAccessor::connect("sqlite::memory:").await.unwrap();
    db.exec_with_params("create table t (a integer, b text)", &[])
        .await
        .unwrap();
    // 多值 append
    let batch = render_batch(
        Dialect::Sqlite,
        "t",
        &cols(&["a", "b"]),
        &two_rows(),
        &ImportMode::Append,
        &[],
    )
    .unwrap();
    let n = db
        .exec_with_params(&batch.sql, &batch.params)
        .await
        .unwrap();
    assert_eq!(n, 2);
    let rows = db
        .query_with_params("select a, b from t order by a", &[])
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    // rowid OR REPLACE 幂等（同 rowid 覆盖）
    let batch = render_batch(
        Dialect::Sqlite,
        "t",
        &cols(&["rowid", "a", "b"]),
        &[vec![json!(1), json!(9), json!("z")]],
        &ImportMode::Upsert {
            conflict_keys: cols(&["rowid"]),
        },
        &[],
    )
    .unwrap();
    db.exec_with_params(&batch.sql, &batch.params)
        .await
        .unwrap();
    let rows = db
        .query_with_params("select a, b from t order by a", &[])
        .await
        .unwrap();
    assert_eq!(rows.len(), 2, "or replace 不增行");
    assert_eq!(rows[1]["b"], json!("z"), "row 1 被覆盖");
}
