//! schema.yaml 声明式表结构（§4.2 / D1=C）：声明为源，reconcile 推导「安全前向」DDL。
//!
//! 安全前向（只进 apply 路径：dev auto 门禁 + `oj migrate`）：缺表 CREATE、缺可空列
//! ALTER ADD、缺索引 CREATE INDEX。无法安全推导的一律 fail-fast 并打印迁移模板：
//! NOT NULL 列新增、疑似改名（缺新列 + 多旧列）。类型漂移不检查（P3 `oj schema diff`）。
//!
//! 同一份声明喂给 SchemaRegistry（归属图 + 列白名单，§4.8），装配层消费 `registry_tables`。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use only_js::bridge::{DataAccessor, Dialect};
use sea_query::{Alias, ColumnDef, Index};

/// 列类型最小集（§4.7 映射层；其余类型走手写 migrations/）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColType {
    Integer,
    BigInt,
    Text,
    Boolean,
    Double,
    Blob,
}

impl ColType {
    fn parse(s: &str) -> Result<Self, String> {
        Ok(match s {
            "integer" => Self::Integer,
            "bigint" => Self::BigInt,
            "text" => Self::Text,
            "boolean" => Self::Boolean,
            "double" => Self::Double,
            "blob" => Self::Blob,
            other => {
                return Err(format!(
                    "未知列类型 {other:?}（支持 integer/bigint/text/boolean/double/blob；\
                     其余类型请写 migrations/NNNN__desc.sql）"
                ));
            }
        })
    }

    fn apply(&self, cd: &mut ColumnDef) {
        match self {
            Self::Integer => {
                cd.integer();
            }
            Self::BigInt => {
                cd.big_integer();
            }
            Self::Text => {
                cd.text();
            }
            Self::Boolean => {
                cd.boolean();
            }
            Self::Double => {
                cd.double();
            }
            Self::Blob => {
                cd.binary();
            }
        }
    }
}

/// 单列声明：`{ type: text, null: false, autoincrement: true }`。null 缺省 = 可空。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ColumnSchema {
    #[serde(rename = "type")]
    pub col_type: String,
    #[serde(default)]
    pub null: Option<bool>,
    #[serde(default)]
    pub autoincrement: bool,
}

impl ColumnSchema {
    /// 类型（parse() 已验证，此处 unwrap 不可达）。
    fn ty(&self) -> ColType {
        ColType::parse(&self.col_type).expect("column type validated at parse")
    }
}

/// 单表声明。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct TableSchema {
    #[serde(default)]
    pub pk: Option<String>,
    pub columns: BTreeMap<String, ColumnSchema>,
    #[serde(default)]
    pub indexes: HashMap<String, Vec<String>>,
}

/// schema.yaml 顶层：`tables: { <name>: {...} }`。BTreeMap 保证 DDL 产出顺序稳定。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct SchemaFile {
    pub tables: BTreeMap<String, TableSchema>,
}

/// 标识符白名单（进内省 SQL 与 DDL 的信任边界）：`[A-Za-z_][A-Za-z0-9_]*`。
fn is_ident(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

impl SchemaFile {
    /// 解析 + 校验（标识符白名单、类型、autoincrement 仅限 pk 列、索引列存在）。
    pub fn parse(text: &str) -> Result<Self, String> {
        let f: SchemaFile =
            serde_yaml::from_str(text).map_err(|e| format!("parse schema.yaml: {e}"))?;
        for (t, ts) in &f.tables {
            if !is_ident(t) {
                return Err(format!("schema: 非法表名 {t:?}"));
            }
            if let Some(pk) = &ts.pk {
                if !is_ident(pk) {
                    return Err(format!("schema: 表 {t:?} 非法主键 {pk:?}"));
                }
                if !ts.columns.contains_key(pk) {
                    return Err(format!("schema: 表 {t:?} 主键 {pk:?} 未在 columns 声明"));
                }
            }
            for (c, cs) in &ts.columns {
                if !is_ident(c) {
                    return Err(format!("schema: 表 {t:?} 非法列名 {c:?}"));
                }
                ColType::parse(&cs.col_type).map_err(|e| format!("schema: 表 {t:?}.{c}: {e}"))?;
                if cs.autoincrement && ts.pk.as_deref() != Some(c.as_str()) {
                    return Err(format!(
                        "schema: 表 {t:?} 列 {c:?} autoincrement 仅允许主键列"
                    ));
                }
            }
            for (ix, cols) in &ts.indexes {
                if !is_ident(ix) {
                    return Err(format!("schema: 表 {t:?} 非法索引名 {ix:?}"));
                }
                if cols.is_empty() {
                    return Err(format!("schema: 表 {t:?} 索引 {ix:?} 列表为空"));
                }
                for c in cols {
                    if !ts.columns.contains_key(c) {
                        return Err(format!("schema: 表 {t:?} 索引 {ix:?} 引用未声明列 {c:?}"));
                    }
                }
            }
        }
        Ok(f)
    }

    /// 读模块目录下 schema.yaml；不存在 = None（模块无声明式表）。
    pub fn load(module_dir: &Path) -> Result<Option<Self>, String> {
        let p = module_dir.join("schema.yaml");
        let text = match std::fs::read_to_string(&p) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("read {}: {e}", p.display())),
        };
        Self::parse(&text).map(Some)
    }

    /// 归属图 + SchemaRegistry 喂料：(表名, 主键, 全部列名)。
    pub fn registry_tables(&self) -> Vec<(&str, Option<&str>, Vec<&str>)> {
        self.tables
            .iter()
            .map(|(name, t)| {
                (
                    name.as_str(),
                    t.pk.as_deref(),
                    t.columns.keys().map(|s| s.as_str()).collect(),
                )
            })
            .collect()
    }
}

// ----- DDL 生成（sea-query 底座；QueryBuilder 非 dyn 兼容，按方言 match 分发，同 query.rs） -----

fn column_def(name: &str, c: &ColumnSchema, is_pk: bool) -> ColumnDef {
    let mut cd = ColumnDef::new(Alias::new(name));
    c.ty().apply(&mut cd);
    if c.autoincrement {
        cd.auto_increment();
    }
    if c.null == Some(false) {
        cd.not_null();
    }
    if is_pk {
        cd.primary_key();
    }
    cd
}

/// 三方言渲染分发（SchemaStatementBuilder 方法泛型非 dyn 兼容，宏展开同款 match，
/// 同 query.rs build_select 模式）。
macro_rules! render {
    ($stmt:expr, $d:expr) => {
        match $d {
            Dialect::Sqlite => $stmt.to_string(sea_query::SqliteQueryBuilder),
            Dialect::MySql => $stmt.to_string(sea_query::MysqlQueryBuilder),
            Dialect::Postgres => $stmt.to_string(sea_query::PostgresQueryBuilder),
        }
    };
}

/// CREATE TABLE（列级主键 / autoincrement / NOT NULL）。
pub fn create_table_ddl(t: &TableSchema, name: &str, d: Dialect) -> String {
    let mut ct = sea_query::Table::create();
    ct.table(Alias::new(name));
    for (c, cs) in &t.columns {
        ct.col(column_def(c, cs, t.pk.as_deref() == Some(c.as_str())));
    }
    render!(ct, d)
}

/// ALTER TABLE ADD COLUMN（仅可空列——调用方已 gate；NOT NULL 新增走手写迁移）。
pub fn add_column_ddl(table: &str, name: &str, c: &ColumnSchema, d: Dialect) -> String {
    let mut at = sea_query::Table::alter();
    at.table(Alias::new(table));
    at.add_column(column_def(name, c, false));
    render!(at, d)
}

/// CREATE INDEX（表/列名已过白名单）。
pub fn create_index_ddl(table: &str, index: &str, cols: &[String], d: Dialect) -> String {
    let mut ix = Index::create();
    ix.name(index).table(Alias::new(table));
    for c in cols {
        ix.col(Alias::new(c));
    }
    render!(ix, d)
}

// ----- reconcile（声明 vs 实库收敛；只进 apply 路径） -----

/// 内省行首列取字符串（各方言列名不同，只认值）。
fn first_strings(rows: Vec<only_js::bridge::Row>) -> Vec<String> {
    rows.into_iter()
        .filter_map(|r| {
            r.as_object()
                .and_then(|o| o.values().next())
                .and_then(|v| v.as_str().map(|s| s.to_string()))
        })
        .collect()
}

async fn q(acc: &dyn DataAccessor, sql: &str) -> Result<Vec<String>, String> {
    let rows = acc
        .query_with_params(sql, &[])
        .await
        .map_err(|e| format!("schema 内省失败: {e}"))?;
    Ok(first_strings(rows))
}

/// 实库全部表名。
async fn db_tables(acc: &dyn DataAccessor, d: Dialect) -> Result<HashSet<String>, String> {
    let sql: String = match d {
        Dialect::Sqlite => "SELECT name FROM sqlite_master WHERE type = 'table'".into(),
        Dialect::MySql => {
            "SELECT table_name FROM information_schema.tables WHERE table_schema = DATABASE()"
                .into()
        }
        Dialect::Postgres => {
            "SELECT table_name FROM information_schema.tables WHERE table_schema = 'public'".into()
        }
    };
    Ok(q(acc, &sql).await?.into_iter().collect())
}

/// 表的全部列名（表名已过 is_ident 白名单——pragma/info 查询无法绑定参数处内联）。
async fn db_columns(
    acc: &dyn DataAccessor,
    d: Dialect,
    table: &str,
) -> Result<HashSet<String>, String> {
    let sql = match d {
        Dialect::Sqlite => format!("SELECT name FROM pragma_table_info('{table}')"),
        Dialect::MySql => format!(
            "SELECT column_name FROM information_schema.columns \
             WHERE table_schema = DATABASE() AND table_name = '{table}'"
        ),
        Dialect::Postgres => format!(
            "SELECT column_name FROM information_schema.columns \
             WHERE table_schema = 'public' AND table_name = '{table}'"
        ),
    };
    Ok(q(acc, &sql).await?.into_iter().collect())
}

/// 表的全部索引名。
async fn db_indexes(
    acc: &dyn DataAccessor,
    d: Dialect,
    table: &str,
) -> Result<HashSet<String>, String> {
    let sql = match d {
        Dialect::Sqlite => {
            format!("SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = '{table}'")
        }
        Dialect::MySql => format!(
            "SELECT DISTINCT index_name FROM information_schema.statistics \
             WHERE table_schema = DATABASE() AND table_name = '{table}'"
        ),
        Dialect::Postgres => format!(
            "SELECT indexname FROM pg_indexes \
             WHERE schemaname = 'public' AND tablename = '{table}'"
        ),
    };
    Ok(q(acc, &sql).await?.into_iter().collect())
}

async fn exec(acc: &dyn DataAccessor, sql: &str) -> Result<(), String> {
    acc.exec_with_params(sql, &[])
        .await
        .map(|_| ())
        .map_err(|e| format!("exec `{sql}`: {e}"))
}

/// 声明 vs 实库收敛（幂等）：缺表 CREATE、缺可空列 ALTER ADD、缺索引 CREATE INDEX。
/// NOT NULL 新增 / 疑似改名 → Err（含迁移模板）。返回执行日志（空 = 已收敛）。
pub async fn reconcile(
    acc: &dyn DataAccessor,
    module: &str,
    schema: &SchemaFile,
) -> Result<Vec<String>, String> {
    let d = acc.dialect();
    let mut log = Vec::new();
    let tables = db_tables(acc, d).await?;
    for (name, t) in &schema.tables {
        if !tables.contains(name) {
            exec(acc, &create_table_ddl(t, name, d)).await?;
            log.push(format!("[{module}] create table {name}"));
            for (ix, cols) in &t.indexes {
                exec(acc, &create_index_ddl(name, ix, cols, d)).await?;
                log.push(format!("[{module}] create index {ix}"));
            }
            continue;
        }
        let db_cols = db_columns(acc, d, name).await?;
        let missing: Vec<&String> = t.columns.keys().filter(|c| !db_cols.contains(*c)).collect();
        let extra: Vec<&String> = db_cols
            .iter()
            .filter(|c| !t.columns.contains_key(c.as_str()))
            .collect();
        if !missing.is_empty() && !extra.is_empty() {
            return Err(format!(
                "schema: 表 {name:?} 疑似改名（声明缺列 {missing:?}，实库多列 {extra:?}）。\n  \
                 下一步：写迁移手写改名（模板 migrations/NNNN__rename.sql）：\n  \
                 ALTER TABLE {name} RENAME COLUMN {} TO {};（对应关系按列名人工核对）",
                extra[0], missing[0],
            ));
        }
        for m in &missing {
            let c = &t.columns[*m];
            if c.null == Some(false) {
                return Err(format!(
                    "schema: 表 {name:?} 列 {m:?} 声明 NOT NULL 且实库缺失，无法安全推导\
                     （存量行无值）。\n  下一步：手写 migrations/NNNN__add_{m}.sql，例如：\n  \
                     ALTER TABLE {name} ADD COLUMN {m} {} NOT NULL DEFAULT <值>;",
                    c.col_type
                ));
            }
            exec(acc, &add_column_ddl(name, m, c, d)).await?;
            log.push(format!("[{module}] add column {name}.{m}"));
        }
        let db_ix = db_indexes(acc, d, name).await?;
        for (ix, cols) in &t.indexes {
            if !db_ix.contains(ix) {
                exec(acc, &create_index_ddl(name, ix, cols, d)).await?;
                log.push(format!("[{module}] create index {ix}"));
            }
        }
    }
    Ok(log)
}

/// 声明 vs 实库只读对账（`oj schema diff`，§5.1 漂移层）：
/// D001 缺表 / 缺列 / 多列 / 缺索引；D002 实库有而无任何模块声明（排除
/// `_oj_migrations%` 账本与 `sqlite_%` 内部表）。类型漂移不比对（方言类型
/// 反查长尾，手写迁移场景人工核）。返回报告行（空 = 一致）。
pub async fn diff(
    acc: &dyn DataAccessor,
    modules: &[(String, SchemaFile)],
) -> Result<Vec<String>, String> {
    let d = acc.dialect();
    let mut report = Vec::new();
    let all = db_tables(acc, d).await?;
    let mut declared: HashSet<String> = HashSet::new();
    for (module, f) in modules {
        for (name, t) in &f.tables {
            declared.insert(name.clone());
            if !all.contains(name) {
                report.push(format!(
                    "D001: [{module}] 表 {name} 实库缺失（oj migrate 收敛）"
                ));
                continue;
            }
            let cols = db_columns(acc, d, name).await?;
            for c in t.columns.keys().filter(|c| !cols.contains(*c)) {
                report.push(format!("D001: [{module}] 表 {name} 缺列 {c}"));
            }
            for c in cols.iter().filter(|c| !t.columns.contains_key(c.as_str())) {
                report.push(format!(
                    "D001: [{module}] 表 {name} 多列 {c}（改名/删除须手写迁移）"
                ));
            }
            let ixs = db_indexes(acc, d, name).await?;
            for ix in t.indexes.keys().filter(|ix| !ixs.contains(*ix)) {
                report.push(format!("D001: [{module}] 表 {name} 缺索引 {ix}"));
            }
        }
    }
    for t in all {
        if !declared.contains(&t) && !t.starts_with("_oj_migrations") && !t.starts_with("sqlite_") {
            report.push(format!("D002: 表 {t} 未被任何模块 schema.yaml 声明"));
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER_YAML: &str = "\
tables:
  account:
    pk: id
    columns:
      id: { type: integer, autoincrement: true }
      name: { type: text, null: false }
      role: { type: text, null: false }
    indexes:
      idx_account_name: [name]
  audit:
    columns:
      id: { type: bigint }
      note: { type: text }
";

    fn user_schema() -> SchemaFile {
        SchemaFile::parse(USER_YAML).unwrap()
    }

    #[test]
    fn parse_valid_and_rejects_bad_decls() {
        let f = user_schema();
        assert_eq!(f.tables.len(), 2);
        let acct = &f.tables["account"];
        assert_eq!(acct.pk.as_deref(), Some("id"));
        assert_eq!(acct.indexes["idx_account_name"], vec!["name".to_string()]);

        // 未知类型
        let e = SchemaFile::parse("tables:\n  t:\n    columns:\n      a: { type: jsonb }\n")
            .unwrap_err();
        assert!(e.contains("jsonb"), "{e}");
        // autoincrement 非主键
        let e = SchemaFile::parse(
            "tables:\n  t:\n    columns:\n      a: { type: integer, autoincrement: true }\n",
        )
        .unwrap_err();
        assert!(e.contains("autoincrement"), "{e}");
        // 主键未在 columns 声明
        let e = SchemaFile::parse(
            "tables:\n  t:\n    pk: nope\n    columns:\n      a: { type: text }\n",
        )
        .unwrap_err();
        assert!(e.contains("nope"), "{e}");
        // 非法表名 / 列名 / 索引名（信任边界：内省 SQL 内联）
        for yaml in [
            "tables:\n  bad-name:\n    columns:\n      a: { type: text }\n",
            "tables:\n  t:\n    columns:\n      \"a;b\": { type: text }\n",
            "tables:\n  t:\n    columns:\n      a: { type: text }\n    indexes:\n      x y: [a]\n",
        ] {
            assert!(SchemaFile::parse(yaml).is_err(), "{yaml}");
        }
        // 索引引用未声明列 / 空列
        assert!(SchemaFile::parse(
            "tables:\n  t:\n    columns:\n      a: { type: text }\n    indexes:\n      ix: [ghost]\n",
        )
        .is_err());
        assert!(SchemaFile::parse(
            "tables:\n  t:\n    columns:\n      a: { type: text }\n    indexes:\n      ix: []\n",
        )
        .is_err());
    }

    #[test]
    fn ddl_three_dialects() {
        let f = user_schema();
        let acct = &f.tables["account"];
        let sq = create_table_ddl(acct, "account", Dialect::Sqlite);
        assert!(sq.contains("CREATE TABLE"), "{sq}");
        assert!(sq.contains("\"account\""), "{sq}");
        assert!(sq.contains("\"id\" integer PRIMARY KEY"), "{sq}");
        assert!(sq.contains("\"name\" text NOT NULL"), "{sq}");
        let my = create_table_ddl(acct, "account", Dialect::MySql);
        assert!(my.contains("`account`"), "{my}");
        assert!(my.contains("AUTO_INCREMENT"), "{my}");
        let pg = create_table_ddl(acct, "account", Dialect::Postgres);
        // sea-query 1.0 pg 将 auto_increment 渲染为 identity 列（非旧版 serial）。
        assert!(pg.contains("IDENTITY"), "{pg}");

        let c = ColumnSchema {
            col_type: "text".into(),
            null: None,
            autoincrement: false,
        };
        let alter = add_column_ddl("account", "bio", &c, Dialect::Sqlite);
        assert!(alter.contains("ALTER TABLE"), "{alter}");
        assert!(alter.contains("ADD COLUMN"), "{alter}");
        assert!(alter.contains("\"bio\""), "{alter}");

        let ix = create_index_ddl(
            "account",
            "idx_account_name",
            &["name".to_string()],
            Dialect::Sqlite,
        );
        assert!(ix.contains("CREATE INDEX"), "{ix}");
        assert!(ix.contains("idx_account_name"), "{ix}");
    }

    async fn sqlite_acc() -> std::sync::Arc<dyn DataAccessor> {
        only_js::bridge::SqlxAccessor::arc("sqlite::memory:")
            .await
            .unwrap()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reconcile_creates_adds_and_is_idempotent() {
        let acc = sqlite_acc().await;
        let f = user_schema();
        // 首轮：建 account + audit + 索引 = 3 条（audit 无索引）
        let log = reconcile(acc.as_ref(), "user", &f).await.unwrap();
        assert_eq!(log.len(), 3, "{log:?}");
        // 幂等：复跑零动作
        let log2 = reconcile(acc.as_ref(), "user", &f).await.unwrap();
        assert!(log2.is_empty(), "{log2:?}");
        // 加可空列 → ALTER；缺索引 → CREATE
        let yaml2 = USER_YAML.replace(
            "      note: { type: text }",
            "      note: { type: text }\n      tag: { type: text }",
        );
        let f2 = SchemaFile::parse(&yaml2).unwrap();
        let log3 = reconcile(acc.as_ref(), "user", &f2).await.unwrap();
        assert_eq!(
            log3,
            vec!["[user] add column audit.tag".to_string()],
            "{log3:?}"
        );
        // 再复跑干净
        assert!(
            reconcile(acc.as_ref(), "user", &f2)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reconcile_rejects_rename_and_not_null_add() {
        let acc = sqlite_acc().await;
        let f = user_schema();
        reconcile(acc.as_ref(), "user", &f).await.unwrap();
        // 改名：role → user_role（缺新列 + 多旧列 → 疑似改名 fail-fast）
        let renamed = USER_YAML.replace(
            "role: { type: text, null: false }",
            "user_role: { type: text, null: false }",
        );
        let e = reconcile(acc.as_ref(), "user", &SchemaFile::parse(&renamed).unwrap())
            .await
            .unwrap_err();
        assert!(e.contains("疑似改名"), "{e}");
        assert!(e.contains("RENAME COLUMN"), "{e}");
        // NOT NULL 新列（新增而非替换，否则触发改名路径）：不可安全推导
        let nn = USER_YAML.replace(
            "      note: { type: text }",
            "      note: { type: text }\n      extra: { type: text, null: false }",
        );
        let e = reconcile(acc.as_ref(), "user", &SchemaFile::parse(&nn).unwrap())
            .await
            .unwrap_err();
        assert!(e.contains("NOT NULL"), "{e}");
        assert!(e.contains("migrations/"), "{e}");
    }

    /// `oj schema diff`（D001/D002）：缺表/缺列/多列与未声明表逐一报告。
    #[tokio::test(flavor = "current_thread")]
    async fn diff_reports_missing_extra_and_undeclared() {
        let acc = sqlite_acc().await;
        acc.exec_with_params("CREATE TABLE present (id integer, gone text)", &[])
            .await
            .unwrap();
        acc.exec_with_params("CREATE TABLE rogue (x)", &[])
            .await
            .unwrap();
        let f = SchemaFile::parse(
            "tables:\n  present:\n    pk: id\n    columns:\n      id: { type: integer }\n      ghost: { type: text }\n  absent:\n    columns:\n      a: { type: text }\n",
        )
        .unwrap();
        let r = diff(acc.as_ref(), &[("m".to_string(), f)]).await.unwrap();
        let has = |s: &str| r.iter().any(|l| l.contains(s));
        assert!(has("D001") && has("absent") && has("缺失"), "{r:?}");
        assert!(has("ghost"), "{r:?}"); // 声明有实库无
        assert!(has("gone"), "{r:?}"); // 实库有声明无
        assert!(has("D002") && has("rogue"), "{r:?}");
    }

    #[test]
    fn load_missing_returns_none() {
        let d = std::env::temp_dir().join(format!("oj-schema-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        assert!(SchemaFile::load(&d).unwrap().is_none());
        std::fs::write(d.join("schema.yaml"), "tables: {}\n").unwrap();
        let f = SchemaFile::load(&d).unwrap().unwrap();
        assert!(f.tables.is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }
}
