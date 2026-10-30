# 批次 3（REQ-09 剩余）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** REQ-09 剩余收口——建表保真（partitionBy/MySQL options）、分区/表选项变更显式结构化报错、objectType 扩六值（view/index/sequence/routine/trigger）、表改名 RENAME TO。

**Architecture:** 全部在 host 侧 `src/bridge/db_ddl.rs`（纯渲染零 DB I/O）+ `src/bridge/db_introspect.rs`（op 前置结构化报错通道）。零 ABI。批次内先行 G2 报错骨架（Task 1），再依次挂渲染/对象/改名。

**Tech Stack:** Rust（serde_json/sqlx 类型常量），既有 `quote_ident`/`quote_literal`/`check_fragment`/`ColSpec`/`parse_columns` 机制。

**Spec:** `docs/superpowers/specs/2026-10-30-dbx-gap-batch3-design.md`（已批准，含 builder-check 五问修订）。

## Global Constraints

- 一切构建/测试/clippy 用 `--release`；异步测试 `tokio::test(flavor = "current_thread")`；cargo test 接管道必须 `set -o pipefail`。
- 标识符全走 `quote_ident`（NUL/控制字符拒）；新 DDL 文本字段走 `validate_statement_body`（拒 NUL、限长 64 KiB，**不拒 `;`/换行**——语句体信任类，见 spec §4.6），**不过 `check_fragment`**。
- 本批零 ABI（`oj-plugin-ffi` 不动）；`bootstrap.js` 不动（JSDoc 若补守 7-bit ASCII）。
- commit 尾行 `unix@vip.qq.com ai`；CHANGELOG + devkit 四件随 Task 5 同步（设计红线）。
- env-gated 腿：`OJ_TEST_PG=postgres://postgres:postgres@127.0.0.1:5432/ojtest`、`OJ_TEST_MYSQL=mysql://root:root@127.0.0.1:3306/ojtest`（本机容器，未设即 skip）。

## Review Focus

1. **partitionBy 拼进 CREATE TABLE 的注入面**：值为语句体但绝不能逃逸标识符位——渲染位置钉死在闭合 `)` 之后，值原样、不与任何标识符拼接。
2. **options.comment 引号转义**：`'` 必须翻倍（两方言同），否则建表语句被截断/注入。
3. **G2 判据的 false-positive**：from 缺省 partitionBy + to 显式给同值（或两侧皆空）→ 不得报错；只有「一侧有一侧无或文本不同」才 unsupported。
4. **objectType 扩值后旧行为零回归**：`"table"` 路径（含 schema 段、rename、warnings 聚类）一个测试都不能红。
5. **表改名与列 rename 的叠加**：表级 previousName + 列级 previousName 同 spec 出现时，表 RENAME 必须最先、列 RENAME 用新表名渲染。

---

### Task 1: G2 报错骨架 + 语句体校验 helper

**Files:**
- Modify: `src/bridge/db_ddl.rs`（ddl_diff 顶部解析区 352-405 之间插检查；新增两个 pub helper）
- Modify: `src/bridge/db_introspect.rs`（`op_db_ddl_diff` 1105-1115 之间前置检查）

**Interfaces:**
- Produces: `pub fn validate_statement_body(field: &str, s: &str) -> Result<(), String>`（拒 NUL/超 64 KiB）；`pub fn unsupported_capability_from_spec(d: Dialect, spec: &Value) -> Option<&'static str>`（partition 差异 → `"ddl_diff_partition"`；options 差异 → `"ddl_diff_table_options"`；仅 objectType=table 时判，其余 None）；`ddl_diff` 内部在解析 from/to 后立即调 unsupported 检查（字符串兜底 `format!("ddlDiff: unsupported capability {c}")`）。

- [ ] **Step 1: Write the failing tests**

```rust
/// G2：partitionBy 差异 → unsupported（helper 与 ddl_diff 两路同判据）。
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
    assert_eq!(unsupported_capability_from_spec(Dialect::Postgres, &spec3), None);
    let spec4 = json!({"objectType":"table","name":"t",
        "from":{"columns":[{"name":"d","nativeType":"date"}],"partitionBy":"RANGE (d)"},
        "to":{"columns":[{"name":"d","nativeType":"date"}],"partitionBy":"RANGE (d)"}});
    assert_eq!(unsupported_capability_from_spec(Dialect::Postgres, &spec4), None);
}

/// G2：MySQL options 差异 → ddl_diff_table_options。
#[test]
fn options_change_is_unsupported_ddl_diff_table_options() {
    let mk = |e: &str| json!({"objectType":"table","name":"t",
        "from":{"columns":[{"name":"id","nativeType":"int"}],
                "options":{"engine":"InnoDB"}},
        "to":{"columns":[{"name":"id","nativeType":"int"}],
              "options":{"engine":e}}});
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
```

- [ ] **Step 2: Run to verify RED**

Run: `set -o pipefail && cargo test --release --lib partition_change_is_unsupported 2>&1 | tail -5`
Expected: FAIL（helper 不存在，编译错误即 RED）

- [ ] **Step 3: Implement helpers + ddl_diff 内检**

`validate_statement_body`：NUL 或 `s.len() > 65536` → Err(`"ddlDiff: {field} contains NUL or exceeds 64 KiB"`)。`unsupported_capability_from_spec`：objectType=="table" 时比较 `from.partitionBy`/`to.partitionBy`（as_str，Option 不等 → partition）、`from.options`/`to.options`（值不等 → table_options）。`ddl_diff` 在 394 行（fks 解析）之后插：`if let Some(c) = unsupported_capability_from_spec(d, spec) { return Err(format!("ddlDiff: unsupported capability {c}")); }`。

- [ ] **Step 4: op 前置结构化通道**

`op_db_ddl_diff`：`super::db_policy::granted_only` 之后、`ddl_diff` 调用之前插：`if let Some(cap) = super::db_ddl::unsupported_capability_from_spec(dialect, &spec) { return Err(to_js_err(Box::new(crate::bridge::db_error::unsupported(dialect.as_str(), cap)))); }`。

- [ ] **Step 5: Run tests GREEN**

Run: `set -o pipefail && cargo test --release --lib partition_change_is_unsupported 2>&1 | tail -3 && cargo test --release --lib options_change 2>&1 | tail -3 && cargo test --release --lib validate_statement_body 2>&1 | tail -3`
Expected: 全 PASS

- [ ] **Step 6: Commit**

```bash
git add src/bridge/db_ddl.rs src/bridge/db_introspect.rs
git commit -m "feat(db): REQ-09 §4.3——分区/表选项变更显式结构化报错 + 语句体校验 helper

unix@vip.qq.com ai"
```

---

### Task 2: G1 建表保真渲染

**Files:**
- Modify: `src/bridge/db_ddl.rs`（create 路径 466-530；`both()` 测试 helper 沿用）

**Interfaces:**
- Consumes: Task 1 的 `validate_statement_body`、`unsupported_capability_from_spec`（G2 已挡 from≠null 差异，create 路径 partitionBy 直渲染）。
- Produces: create 语句尾部渲染约定：PG `... ) PARTITION BY {text}`；MySQL `... ) PARTITION BY {text} ENGINE=.. DEFAULT CHARSET=.. COLLATE=.. COMMENT='..'`（键按 engine→charset→collate→comment 固定序、可选跳过空值）。

- [ ] **Step 1: Write the failing tests**

```rust
/// G1：from=null 建分区表——PG/MySQL 闭合括号后拼 PARTITION BY。
#[test]
fn create_table_renders_partition_by_pg_mysql() {
    let (pg, my) = both(json!({"objectType":"table","name":"t","from":Value::Null,
        "to":{"columns":[{"name":"d","nativeType":"date"}],
              "partitionBy":"RANGE (d)"}}));
    let pgs = pg["statements"][0].as_str().unwrap();
    assert!(pgs.starts_with("CREATE TABLE \"t\"") && pgs.ends_with("PARTITION BY RANGE (d)"), "{pgs}");
    let mys = my["statements"][0].as_str().unwrap();
    assert!(mys.starts_with("CREATE TABLE `t`") && mys.ends_with("PARTITION BY RANGE (d)"), "{mys}");
}

/// G1：MySQL options 四键固定序 + comment 单引号翻倍。
#[test]
fn create_table_renders_mysql_options_in_fixed_order() {
    let (_, my) = both(json!({"objectType":"table","name":"t","from":Value::Null,
        "to":{"columns":[{"name":"id","nativeType":"int"}],
              "options":{"engine":"InnoDB","charset":"utf8mb4",
                         "collate":"utf8mb4_bin","comment":"it's 业务"}}}));
    let s = my["statements"][0].as_str().unwrap();
    assert!(s.contains("ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_bin COMMENT='it''s 业务'"), "{s}");
    // 部分键：只给 engine → 其余不渲染。
    let (_, my2) = both(json!({"objectType":"table","name":"t","from":Value::Null,
        "to":{"columns":[{"name":"id","nativeType":"int"}],"options":{"engine":"MyISAM"}}}));
    let s2 = my2["statements"][0].as_str().unwrap();
    assert!(s2.contains("ENGINE=MyISAM") && !s2.contains("CHARSET"), "{s2}");
}

/// G1：PG 结构出现 options → 参数错误（fail-loud，不静默忽略）。
#[test]
fn pg_options_is_parameter_error() {
    let err = ddl_diff(Dialect::Postgres, &json!({"objectType":"table","name":"t",
        "from":Value::Null,
        "to":{"columns":[{"name":"id","nativeType":"int4"}],"options":{"engine":"InnoDB"}}}))
        .unwrap_err();
    assert!(err.contains("options"), "{err}");
}

/// G1：partitionBy 带 NUL → 参数错误。
#[test]
fn partition_by_nul_rejected() {
    let err = ddl_diff(Dialect::Postgres, &json!({"objectType":"table","name":"t",
        "from":Value::Null,
        "to":{"columns":[{"name":"d","nativeType":"date"}],"partitionBy":"a\u{0}b"}}))
        .unwrap_err();
    assert!(err.contains("NUL"), "{err}");
}
```

- [ ] **Step 2: Run to verify RED**

Run: `set -o pipefail && cargo test --release --lib create_table_renders 2>&1 | tail -4`
Expected: FAIL（partitionBy/options 未渲染）

- [ ] **Step 3: Implement**

create 路径（466-502）：`to.get("partitionBy").and_then(Value::as_str)` 存在 → `validate_statement_body("partitionBy", s)?`；MySQL 侧 `to.get("options")` 存在 → 逐键 `as_str` 校验（未知键忽略？——不：未知键参数错误，fail-loud），comment 走既有 `quote_literal`（52 行附近）。PG 侧 options 存在 → Err `"ddlDiff: options are mysql-only"`。渲染：`let tail = ...; format!("CREATE TABLE {t} (\n  {}{}\n){tail}", ...)`。alter 路径不改（G2 已挡差异；from=null 才渲染）。

- [ ] **Step 4: GREEN + 全套回归**

Run: `set -o pipefail && cargo test --release --lib 2>&1 | tail -3`
Expected: 全 PASS（含全部旧 ddlDiff 测试——Review Focus #4）

- [ ] **Step 5: Commit**

```bash
git add src/bridge/db_ddl.rs
git commit -m "feat(db): REQ-09 §4.2——建表保真（partitionBy + MySQL options 四键固定序）

unix@vip.qq.com ai"
```

---

### Task 3: G3 objectType 扩六值

**Files:**
- Modify: `src/bridge/db_ddl.rs`（objectType 校验 353；新增非表对象分发函数 `ddl_diff_object`）

**Interfaces:**
- Consumes: `validate_statement_body`（Task 1）、`t`/`schema` 渲染（既有 360-369）。
- Produces: `ddl_diff` 顶部 objectType 分发：`"table"` → 既有路径；`"view"|"index"|"sequence"|"routine"|"trigger"` → `ddl_diff_object(d, spec)`，返回同信封 `{diff, statements, warnings:[]}`（diff 内 keys 随型）。

- [ ] **Step 1: Write the failing tests**

```rust
/// G3：view from=null → 单条 CREATE VIEW。
#[test]
fn view_create_from_null() {
    for (d, q) in [(Dialect::Postgres, "\""), (Dialect::MySql, "`")] {
        let out = ddl_diff(d, &json!({"objectType":"view","name":"v","from":Value::Null,
            "to":{"definition":"SELECT 1 AS a"}})).unwrap();
        let s = &out["statements"][0];
        assert_eq!(*s, json!(format!("CREATE VIEW {q}v{q} AS SELECT 1 AS a")));
        assert_eq!(out["warnings"], json!([]));
    }
}

/// G3：view definition 差异 → PG CREATE OR REPLACE / MySQL ALTER VIEW + diff.definition。
#[test]
fn view_definition_diff_and_drop() {
    let mk = |def: &str| json!({"objectType":"view","name":"v",
        "from":{"definition":"SELECT 1"},"to":{"definition":def}});
    let pg = ddl_diff(Dialect::Postgres, &mk("SELECT 2")).unwrap();
    assert_eq!(pg["statements"][0], json!("CREATE OR REPLACE VIEW \"v\" AS SELECT 2"));
    assert_eq!(pg["diff"]["definition"], json!({"from":"SELECT 1","to":"SELECT 2"}));
    let my = ddl_diff(Dialect::MySql, &mk("SELECT 2")).unwrap();
    assert!(my["statements"][0].as_str().unwrap().starts_with("ALTER VIEW `v` AS "), "{my}");
    // 相同 → 零语句。
    let same = ddl_diff(Dialect::Postgres, &mk("SELECT 1")).unwrap();
    assert_eq!(same["statements"], json!([]));
    // drop（to=null）→ DROP VIEW。
    let drop = ddl_diff(Dialect::Postgres, &json!({"objectType":"view","name":"v",
        "from":{"definition":"SELECT 1"},"to":Value::Null})).unwrap();
    assert_eq!(drop["statements"][0], json!("DROP VIEW \"v\""));
}

/// G3：index/sequence/routine/trigger create（definition 原样）+ drop。
#[test]
fn other_object_types_create_and_drop() {
    let cases = [
        ("index", "CREATE INDEX ix ON t (a)", "DROP INDEX \"ix\""),
        ("sequence", "CREATE SEQUENCE s START 100", "DROP SEQUENCE \"s\""),
        ("routine", "CREATE FUNCTION f() RETURNS int AS $$ SELECT 1 $$", "DROP FUNCTION \"f\"()"),
        ("trigger", "CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION f()",
                    "DROP TRIGGER \"tr\""),
    ];
    for (ty, def, drop_sql) in cases {
        let out = ddl_diff(Dialect::Postgres, &json!({"objectType":ty,"name":"x",
            "from":Value::Null,"to":{"definition":def}})).unwrap();
        assert_eq!(out["statements"][0], json!(def), "{ty}");
        let d = ddl_diff(Dialect::Postgres, &json!({"objectType":ty,"name":"x",
            "from":{"definition":def},"to":Value::Null})).unwrap();
        assert_eq!(d["statements"][0], json!(drop_sql), "{ty}");
    }
}

/// G3：边界——未知 objectType / 缺 definition / to=null 的 table / MySQL index drop 缺 table。
#[test]
fn object_type_boundaries() {
    assert!(ddl_diff(Dialect::Postgres, &json!({"objectType":"materialized","name":"x",
        "from":Value::Null,"to":{}})).is_err());
    assert!(ddl_diff(Dialect::Postgres, &json!({"objectType":"view","name":"v",
        "from":Value::Null,"to":{}})).is_err(), "缺 definition 参数错误");
    assert!(ddl_diff(Dialect::Postgres, &json!({"objectType":"table","name":"t",
        "from":{"columns":[]},"to":Value::Null})).is_err(), "table to:null 参数错误");
    assert!(ddl_diff(Dialect::MySql, &json!({"objectType":"index","name":"ix",
        "from":{"definition":"CREATE INDEX ix ON t (a)"},"to":Value::Null})).is_err(),
        "mysql index drop 缺 table 参数错误");
}
```

- [ ] **Step 2: Run to verify RED**

Run: `set -o pipefail && cargo test --release --lib view_create_from_null 2>&1 | tail -4`
Expected: FAIL（objectType 校验拒绝）

- [ ] **Step 3: Implement**

`ddl_diff` 353：objectType 分流——`"table"` 走既有；六值走 `ddl_diff_object(d, spec)?`；其余 Err 照旧（文案更新列出六值）。`ddl_diff_object`：name/schema 渲染同既有；`from=null` → to.definition（`validate_statement_body("definition", ..)?`）→ create 语句（view：`CREATE VIEW {t} AS {def}`；其余四型：`to.definition` 原样单条）；from 非空且 to=null → drop（view `DROP VIEW {t}`；index PG `DROP INDEX {t}`，MySQL `DROP INDEX {t} ON {table}`（from.table 缺 → 参数错误）；sequence `DROP SEQUENCE {t}`；routine 按 from.type 选 FUNCTION/PROCEDURE + `({args})`（from.args 缺 → 参数错误）；trigger `DROP TRIGGER {t}`）；view definition 差异 → PG `CREATE OR REPLACE VIEW` / MySQL `ALTER VIEW` + `diff.definition`；from 非空且 to 非空非 view → 参数错误（不做逐字段 diff）。**table 的 to:null 参数错误**：370 行 `missing to` 处改判定——`to` 缺失或为 Null → Err（既有 `missing to` 文案沿用，Null 同样拒绝；先跑既有全套确认无测试依赖 to:null 静默行为，若有则按 ruling 改测试并记 ledger）。

- [ ] **Step 4: GREEN + 全套回归**

Run: `set -o pipefail && cargo test --release --lib 2>&1 | tail -3`
Expected: 全 PASS

- [ ] **Step 5: Commit**

```bash
git add src/bridge/db_ddl.rs
git commit -m "feat(db): REQ-09 §4.4——objectType 扩 view/index/sequence/routine/trigger（create/drop/diff）

unix@vip.qq.com ai"
```

---

### Task 4: G4 表改名 RENAME TO

**Files:**
- Modify: `src/bridge/db_ddl.rs`（table 路径：spec 解析 356-369、首条语句插入点 ~466 前）

**Interfaces:**
- Consumes: 列 rename 语义（`plan_renames` 既有）。
- Produces: spec 顶层可选 `previousName`；`diff.renamed = [{"from","to"}]`（表级，与 diff.columns 平级）。

- [ ] **Step 1: Write the failing tests**

```rust
/// G4：表×列 rename 叠加——表 RENAME 首条、列 RENAME 次之且用新表名（Review Focus #5）。
#[test]
fn table_rename_plus_column_rename() {
    let (pg, my) = both(json!({"objectType":"table","name":"t_new","previousName":"t_old",
        "from":{"columns":[{"name":"a","nativeType":"int4"}]},
        "to":{"columns":[{"name":"b","nativeType":"int4","previousName":"a"}]}}));
    let ss = pg["statements"].as_array().unwrap();
    assert_eq!(ss[0], json!("ALTER TABLE \"t_old\" RENAME TO \"t_new\""));
    assert_eq!(ss[1], json!("ALTER TABLE \"t_new\" RENAME COLUMN \"a\" TO \"b\""));
    let ss = my["statements"].as_array().unwrap();
    assert_eq!(ss[0], json!("ALTER TABLE `t_old` RENAME TO `t_new`"));
    assert!(ss[1].as_str().unwrap().contains("ALTER TABLE `t_new` RENAME COLUMN `a` TO `b`"), "{ss:?}");
}

/// G4：表 previousName → ALTER TABLE RENAME TO 首条 + diff.renamed。
#[test]
fn table_rename_renders_first_and_records_renamed() {
    let (pg, my) = both(json!({"objectType":"table","name":"t_new","previousName":"t_old",
        "from":{"columns":[{"name":"id","nativeType":"int4"}]},
        "to":{"columns":[{"name":"id","nativeType":"int4"}]}}));
    assert_eq!(pg["statements"][0], json!("ALTER TABLE \"t_old\" RENAME TO \"t_new\""));
    assert_eq!(pg["diff"]["renamed"], json!([{"from":"t_old","to":"t_new"}]));
    assert_eq!(my["statements"][0], json!("ALTER TABLE `t_old` RENAME TO `t_new`"));
    // 其余语句用新名渲染。
    let (pg2, _) = both(json!({"objectType":"table","name":"t_new","previousName":"t_old",
        "from":{"columns":[{"name":"id","nativeType":"int4"}]},
        "to":{"columns":[{"name":"id","nativeType":"int8"}]}}));
    assert!(pg2["statements"][1].as_str().unwrap().contains("\"t_new\""), "{pg2}");
}

/// G4：schema 段 + 表改名两段引用；无 previousName 时零回归（renamed 缺省或空数组——
/// 实现取「恒在且空数组」与列 renamed 一致）。
#[test]
fn table_rename_with_schema_and_noop() {
    let (pg, _) = both(json!({"objectType":"table","schema":"public","name":"t_new",
        "previousName":"t_old",
        "from":{"columns":[{"name":"id","nativeType":"int4"}]},
        "to":{"columns":[{"name":"id","nativeType":"int4"}]}}));
    assert_eq!(pg["statements"][0],
        json!("ALTER TABLE \"public\".\"t_old\" RENAME TO \"public\".\"t_new\""));
    // 无 previousName → renamed 空数组、无 RENAME 语句（既有行为零变化）。
    let (pg2, _) = both(json!({"objectType":"table","name":"t",
        "from":{"columns":[{"name":"id","nativeType":"int4"}]},
        "to":{"columns":[{"name":"id","nativeType":"int4"}]}}));
    assert_eq!(pg2["diff"]["renamed"], json!([]));
    assert!(!pg2["statements"][0].as_str().unwrap().contains("RENAME"), "{pg2}");
}
```

- [ ] **Step 2: Run to verify RED**

Run: `set -o pipefail && cargo test --release --lib table_rename 2>&1 | tail -4`
Expected: FAIL

- [ ] **Step 3: Implement**

spec 解析：`previousName`（as_str，缺省 None）。`t` 渲染照旧（新名）；旧名渲染 `t_old` 同 schema 段拼接。`previousName` 存在且 != name 且 from 非空 → `diff.renamed = [{"from","to"}]`，statements 首位插 `ALTER TABLE {t_old} RENAME TO {t_new}`（PG/MySQL 同词法 `RENAME TO`）。**Ruling 候选**：MySQL 表级用 `ALTER TABLE ... RENAME TO`（与列级 RENAME 同族，非 `RENAME TABLE` 语句）。previousName == name → 忽略（同列 rename 同名跳过语义）。from 为空（create 路径）previousName 无意义 → 忽略。diff envelope 插入点：`"renamed".into()` 旁（679 行附近，diff 顶层与 columns 平级）。

- [ ] **Step 4: GREEN + 全套回归 + env-gated 真库腿**

新增 env-gated（放 db_tests.rs 或 ddl 测试内，风格同 `real_pg_rename_column_preserves_data`）：建 t_old → ddlDiff 表改名 → 重放 → `select * from t_new` 通、t_old 不存在。PG+MySQL 各一。

Run: `set -o pipefail && OJ_TEST_PG=postgres://postgres:postgres@127.0.0.1:5432/ojtest OJ_TEST_MYSQL=mysql://root:root@127.0.0.1:3306/ojtest cargo test --release --lib 2>&1 | tail -3`
Expected: 全 PASS

- [ ] **Step 5: Commit**

```bash
git add src/bridge/db_ddl.rs
git commit -m "feat(db): REQ-09 §4.1 缺口——表改名 RENAME TO（previousName 表级配对）

unix@vip.qq.com ai"
```

---

### Task 5: 逆运算 env-gated 腿 + devkit 四件 + CHANGELOG + v0.1.113

**Files:**
- Modify: `src/bridge/db_tests.rs`（逆运算腿）
- Modify: `CHANGELOG.md`、`oj/Cargo.toml`、`docs/devkit/{api-manual,SKILL,scenarios,README}.md`
- Modify: `../oj-dbx/docs/upstream/2026-10-30-upstream-gap-inventory.md`（§八对账追加）

**Interfaces:**
- Consumes: Task 1-4 全部行为。

- [ ] **Step 1: Write the failing env-gated 逆运算腿**

`real_pg_partitioned_roundtrip`（PG）：建 `t_part (d date, v int) PARTITION BY RANGE (d)` → `catalog("ddl", "t_part")` 抓回 → 断言抓回 DDL 含 `PARTITION BY RANGE`（钉 v0.1.109 拼接器不被本批回归）→ 从抓回 DDL 手工构造 structure（partitionBy 段切出）→ `ddlDiff(create)` 语句含 `PARTITION BY RANGE (d)` → 重放建 t_part2 → 抓回比对分区段一致。`real_mysql_partitioned_and_options_roundtrip`（MySQL）：`create table t_part (...) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 PARTITION BY RANGE (...) (...)` → catalog("ddl") 抓回 → 构造 structure（options + partitionBy）→ ddlDiff create 语句四键 + 分区段 → 重放 → 抓回一致。

Run: `set -o pipefail && OJ_TEST_PG=... OJ_TEST_MYSQL=... cargo test --release --lib roundtrip 2>&1 | tail -4`
Expected: FAIL 或暴露缺口 → 按缺口最小修复（若 PG 抓回不含分区段 → 回归排查，属 Critical）。

- [ ] **Step 2: devkit 四件 + CHANGELOG + bump**

- CHANGELOG 新开 `## v0.1.113（未打标签）` 段：G1/G2/G3/G4 各一条 + breaking 无（本批无 breaking——options/partitionBy 均为新增可选字段）。
- `oj/Cargo.toml` version → 0.1.113。
- api-manual ddlDiff 行：partitionBy/options 契约 + 渲染序 + `ddl_diff_partition`/`ddl_diff_table_options` capability + objectType 六值 + 表 previousName + `to:null` 参数错误；错误/限制表补新参数错误文案。
- SKILL 陷阱速查：「definition/partitionBy 是整条语句体——含 `;`/换行合法，与列 default 的 fragment 校验不同档」；「MySQL options 只走建表路径，改它 = unsupported」。
- scenarios：场景 63（建分区表保真 + catalog 逆运算）、64（objectType 扩值建视图）、65（表改名）。
- README 索引补场景号。
- `cargo xtask build` 归置 `bin/devkit/`；`cargo test --release -p xtask` 契约 10/10。

- [ ] **Step 3: 全套最终验证**

Run: `set -o pipefail && cargo test --release --workspace 2>&1 | tail -3 && cargo clippy --release --workspace --all-targets 2>&1 | tail -2 && cargo fmt --check`
Expected: 全绿 / 0 警 / fmt 净。

- [ ] **Step 4: Commit + 下游对账**

```bash
git add -A
git commit -m "release: v0.1.113——REQ-09 剩余（建表保真/分区报错/非表对象/表改名）

unix@vip.qq.com ai"
```

`../oj-dbx/docs/upstream/2026-10-30-upstream-gap-inventory.md` §八追加 v0.1.113 对账行（REQ-09 全单 ✅）。

---

## Self-Review 备注

- Spec 覆盖：§4.1 表改名→T4；§4.2→T2；§4.3→T1；§4.4→T3；§5 验收→T2-T5 测试逐条映射（partition 建表 T2、逆运算 T5、view create T3、schema 两段 v0.1.112 已有 + T4 复用、单测+真库各任务内）。
- Review Focus #5（表×列 rename 叠加）：T4 Step 1 未含此用例——补进 T4 测试：`table_rename_plus_column_rename`（表 RENAME 首条、列 RENAME 第二条且用新表名）。
- Task 3 的 table to:null 改动有现存行为风险（Step 3 内注明先跑全套确认）。
