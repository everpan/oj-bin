//! db 结构内视 op（从 db.rs 拆分，纯移动）：L2 三件 / catalog / 库-schema 清单面
//! （REQ-04，v0.1.105：listSchemas/listDatabases/forSchema 受限视图——范围谓词
//! 绑定参数化在 introspect::scoped_sql）。
use std::cell::RefCell;
use std::rc::Rc;

use deno_core::{OpState, op2};
use deno_error::JsErrorBox;
use serde_json::Value;

use super::db::{
    Dialect, Target, elapsed_ms, err_meta_of, normalize_and_trace, require_known_dialect,
    resolve_target, to_js_err, trace_sql,
};

/// db.catalog（U-14 批 a，v0.1.100）：目录类内省——views / routines / sequences / ddl /
/// comment / constraints / stats，信封 `{kind, dialect, items}`。与 L2 三件（listColumns/
/// Indexes/ForeignKeys，裸数组）分代：旧件不动（兼容），新 kind 一律走本 op。
/// 授权链与 L2 同（`granted_only`）；kind × 方言 unsupported → 结构化 `OJ_DB_UNSUPPORTED`
/// （能力矩阵见 db-guide §10，报错不静默）。
#[op2]
#[serde]
pub async fn op_db_catalog(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[string] kind: String,
    #[string] arg: Option<String>,
    #[string] scope: Option<String>,
) -> Result<serde_json::Value, JsErrorBox> {
    let kind = super::introspect::Kind::parse_catalog(&kind).ok_or_else(|| {
        JsErrorBox::generic(format!(
            "db catalog: unknown kind {kind:?} (views|routines|sequences|ddl|comment|constraints|stats)"
        ))
    })?;
    let target = resolve_target(&state, &name)?;
    let in_tx = matches!(&target, Target::Tx(_));
    let dialect = match &target {
        Target::Tx(t) => require_known_dialect(&t.dialect_info, kind.capability())?,
        Target::Pool(da) => require_known_dialect(da.dialect_info(), kind.capability())?,
    };
    let alias = super::guard::bound_db(&state, &name);
    super::db_policy::granted_only(&state, &alias)?;

    let mut sql = match super::introspect::catalog_sql(kind, dialect) {
        Some(s) => s.to_string(),
        None => {
            return Err(to_js_err(Box::new(crate::bridge::db_error::unsupported(
                dialect.as_str(),
                kind.capability(),
            ))));
        }
    };
    if kind.catalog_limit() {
        sql.push_str(&format!(" LIMIT {}", super::introspect::MAX_ROWS));
    }
    // F3（REQ-04）：scoped 模式下 mysql SHOW 例外（Ddl）不接受绑定参数——
    // 显式拦截为 unsupported（capability=for_schema），绝不内联 schema 名。
    if scope.is_some() && kind == super::introspect::Kind::Ddl && dialect == Dialect::MySql {
        return Err(to_js_err(Box::new(crate::bridge::db_error::unsupported(
            dialect.as_str(),
            "for_schema",
        ))));
    }
    // mysql SHOW 族 = 零内联铁律的唯一成文例外（introspect.rs 模块注释），白名单校验在 helper。
    let mut params = if kind == super::introspect::Kind::Ddl && dialect == Dialect::MySql {
        match super::introspect::mysql_show_create(arg.as_deref().unwrap_or("")) {
            Ok(s) => {
                sql = s;
                vec![]
            }
            Err(e) => return Err(JsErrorBox::generic(e)),
        }
    } else if kind.catalog_param() {
        let t = arg.as_deref().unwrap_or("");
        if !valid_introspect_table(dialect, t) {
            // 畸形 / 跨库前缀 → 空结果（与 L2 同口径：不报错、不泄漏）。
            return Ok(serde_json::json!({
                "kind": kind.capability(), "dialect": dialect.as_str(), "items": []
            }));
        }
        vec![Value::String(t.to_string())]
    } else {
        vec![]
    };
    // F3（REQ-04）：活动 scope——scope 值前置（表名参数在其后）。sqlite →
    // unsupported；畸形 scope → 空信封。
    let sql = match resolve_scope(&sql, dialect, scope.as_deref()) {
        Ok(Scope::Plain) => sql,
        Ok(Scope::Scoped(s, sc)) => {
            params.insert(0, Value::String(sc));
            s
        }
        Ok(Scope::Empty) => {
            return Ok(serde_json::json!({
                "kind": kind.capability(), "dialect": dialect.as_str(), "items": []
            }));
        }
        Err(e) => return Err(to_js_err(Box::new(e))),
    };

    let placeholder = match &target {
        Target::Tx(t) => t.dialect_info.caps.placeholder,
        Target::Pool(da) => da.dialect_info().caps.placeholder,
    };
    let norm = normalize_and_trace(&state, placeholder, in_tx, &name, &sql, &params)?;
    let start = std::time::Instant::now();
    let trace_params = &[Value::String("<db introspection>".to_string())];
    let res = match &target {
        Target::Pool(da) => da.query_with_params(&norm, &params).await,
        Target::Tx(t) => t.session.lock().await.query(&norm, &params).await,
    };
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    match res {
        Ok(rows) => {
            let items = super::introspect::catalog(kind, rows);
            trace_sql(
                &state,
                &sql,
                trace_params,
                &name,
                in_tx,
                ms,
                Some(items.len() as i64),
                true,
                None,
                None,
                None,
            );
            Ok(serde_json::json!({
                "kind": kind.capability(), "dialect": dialect.as_str(), "items": items
            }))
        }
        Err(e) => {
            let (cat, retry) = err_meta_of(e.as_ref());
            let ms = start.elapsed().as_secs_f64() * 1000.0;
            trace_sql(
                &state,
                &sql,
                trace_params,
                &name,
                in_tx,
                ms,
                None,
                false,
                Some(e.to_string()),
                cat,
                retry,
            );
            Err(to_js_err(e))
        }
    }
}

/// db.listTables(prefix?)（v0.1.82）：方言无关的目录自省，返回当前库业务可见表名
/// （含未登记进 SchemaRegistry 的表——api-only 方案 / 历史遗留表），消除「按方言分支
/// information_schema / sqlite_master」的裸 db.query。SQL 文本一律引擎常量、前缀经
/// `escape_like` 转义后走绑定参数（LIKE ? ESCAPE '|'——'|' 不与任何方言的字符串转义
/// 规则冲突，避开 mysql 反斜杠转义 / PG standard_conforming_strings 的分叉）。能力面
/// 严格弱于 db.query，价值纯在跨方言可移植；表名是返回的数据，不进 SQL 拼接。
#[op2]
#[serde]
pub async fn op_db_list_tables(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[string] prefix: Option<String>,
    #[string] scope: Option<String>,
) -> Result<Vec<String>, JsErrorBox> {
    let target = resolve_target(&state, &name)?;
    let in_tx = matches!(&target, Target::Tx(_));
    // 门控（T8）：未知方言（不在 builder-known 子集）一律结构化 unsupported，绝不回落 Sqlite。
    let dialect = match &target {
        Target::Tx(t) => require_known_dialect(&t.dialect_info, "list_tables")?,
        Target::Pool(da) => require_known_dialect(da.dialect_info(), "list_tables")?,
    };
    // 授权维（安全 B1，v0.1.90）：内省也须过授权，否则 db.allowed() 的
    // 「只列已授权库」是假承诺——未授权主体不该靠 listTables 枚举出别的库的表结构。
    let alias = super::guard::bound_db(&state, &name);
    super::db_policy::granted_only(&state, &alias)?;
    // 投影列统一叫 `name`：sqlite_master 本就叫 name；information_schema 的
    // table_name 取别名。系统表排除只在 sqlite 需要（mysql/pg 已被
    // DATABASE()/current_schema() 的范围天然排除）。
    let mut sql = match dialect {
        Dialect::Sqlite => "SELECT name FROM sqlite_master WHERE type = 'table' AND substr(name, 1, 7) <> 'sqlite_'".to_string(),
        Dialect::MySql => {
            "SELECT table_name AS name FROM information_schema.tables WHERE table_schema = DATABASE()"
                .to_string()
        }
        Dialect::Postgres => {
            // `table_name` 是 `sql_identifier`（`name`）域，sqlx Any 驱动解不了该 OID，
            // 且「空库不触发、建表后才崩」——必须 `::text`（与 introspect.rs 同口径）。
            "SELECT table_name::text AS name FROM information_schema.tables WHERE table_schema = current_schema()"
                .to_string()
        }
    };
    let mut params: Vec<Value> = Vec::new();
    if let Some(p) = prefix {
        // 前缀列不能用投影别名（mysql 的 WHERE 不认 SELECT 别名），按方言落真列名。
        let col = match dialect {
            Dialect::Sqlite => "name",
            _ => "table_name",
        };
        sql.push_str(&format!(" AND {col} LIKE ? ESCAPE '|'"));
        params.push(Value::String(format!(
            "{}%",
            super::query::escape_like(&p, '|')
        )));
    }
    sql.push_str(" ORDER BY name");
    // F3（REQ-04）：活动 scope（forSchema 受限视图）——范围谓词改绑定参数，
    // scope 值前置（prefix 参数在其后）。sqlite → unsupported；畸形 → 空结果。
    let sql = match resolve_scope(&sql, dialect, scope.as_deref()) {
        Ok(Scope::Plain) => sql,
        Ok(Scope::Scoped(s, sc)) => {
            params.insert(0, Value::String(sc));
            s
        }
        Ok(Scope::Empty) => return Ok(Vec::new()),
        Err(e) => return Err(to_js_err(Box::new(e))),
    };
    // 占位符统一改写（0.1.67）：`?` 是 oj 唯一占位符，PG 由宿主改写 `$n`——
    // 与三个裸 SQL op 同一插桩点，不许绕过。
    let placeholder = match &target {
        Target::Tx(t) => t.dialect_info.caps.placeholder,
        Target::Pool(da) => da.dialect_info().caps.placeholder,
    };
    let norm = normalize_and_trace(&state, placeholder, in_tx, &name, &sql, &params)?;
    let start = std::time::Instant::now();
    let res = match target {
        Target::Pool(da) => da.query_with_params(&norm, &params).await,
        Target::Tx(t) => t.session.lock().await.query(&norm, &params).await,
    };
    let ms = elapsed_ms(start);
    match res {
        Ok(rows) => {
            trace_sql(
                &state,
                &sql,
                &params,
                &name,
                in_tx,
                ms,
                Some(rows.len() as i64),
                true,
                None,
                None,
                None,
            );
            // 契约是 Promise<string[]>：投影列固定叫 name，抽出来摊平（防驱动把
            // 表名包成对象/带 schema 前缀——只取纯名字）。
            Ok(rows
                .into_iter()
                .filter_map(|r| r.get("name").and_then(|v| v.as_str()).map(String::from))
                .collect())
        }
        Err(e) => {
            let em = e.to_string();
            trace_sql(
                &state,
                &sql,
                &params,
                &name,
                in_tx,
                ms,
                None,
                false,
                Some(em.clone()),
                None,
                None,
            );
            Err(JsErrorBox::generic(em))
        }
    }
}

/// F3（REQ-04 §5.2 方案 B）：活动 scope 的解析结果。
#[derive(Debug)]
pub(crate) enum Scope {
    /// 无 scope：用原 SQL 常量（默认库/schema 语义）。
    Plain,
    /// scope 有效：SQL 已改写（恰增一个 `?` 参数位），元组为 (sql, scope 值)。
    /// 调用方须把 scope 值**前置**进参数数组（范围谓词在 WHERE 中先于表名谓词出现）。
    Scoped(String, String),
    /// scope 畸形：返回空结果（不报错、不泄漏——与表名畸形同口径）。
    Empty,
}

/// scope 名校验：长度 > 128 或含 NUL / 控制字符 → 畸形。与表名不同，**允许 `.`**
/// （mysql 库名含点合法，且 scope 只走绑定参数，`.` 不构成跨库注入面）。
fn valid_scope_name(scope: &str) -> bool {
    scope.len() <= 128 && !scope.bytes().any(|b| b == 0 || b < 0x20)
}

/// F3：解析活动 scope → 内视 SQL 的 scoped 形态。sqlite（无 schema 概念）与
/// scoped 模式下不可参数化的路径（mysql SHOW 例外由调用方先行拦截）→
/// 结构化 `OJ_DB_UNSUPPORTED`（capability=for_schema），绝不回落默认库。
pub(crate) fn resolve_scope(
    base: &str,
    dialect: Dialect,
    scope: Option<&str>,
) -> Result<Scope, crate::bridge::db_error::BridgeError> {
    match scope {
        None => Ok(Scope::Plain),
        Some(s) => {
            if dialect == Dialect::Sqlite {
                return Err(crate::bridge::db_error::unsupported(
                    dialect.as_str(),
                    "for_schema",
                ));
            }
            if !valid_scope_name(s) {
                return Ok(Scope::Empty);
            }
            match super::introspect::scoped_sql(base, dialect) {
                Some(sql) => Ok(Scope::Scoped(sql, s.to_string())),
                None => Err(crate::bridge::db_error::unsupported(
                    dialect.as_str(),
                    "for_schema",
                )),
            }
        }
    }
}

/// db.listSchemas / db.listDatabases（批次 4 / REQ-04，v0.1.105）：库-schema 清单面。
/// 跨方言同构 `{name, owner?, isDefault?, comment?}`；系统目录过滤口径统一在
/// `introspect::listing_sql`（调用方零 if-else）。prefix 过滤在 Rust 侧
/// `starts_with`（清单基数天然小，PRAGMA 形态又不接 WHERE，统一Rust侧最简）；
/// 行数经 `introspect::listing` 截断到 MAX_ROWS。授权链与内视面同（`granted_only`）。
async fn listing_impl(
    state: Rc<RefCell<OpState>>,
    name: String,
    prefix: Option<String>,
    listing: super::introspect::Listing,
) -> Result<Vec<Value>, JsErrorBox> {
    let capability = match listing {
        super::introspect::Listing::Schemas => "list_schemas",
        super::introspect::Listing::Databases => "list_databases",
    };
    let target = resolve_target(&state, &name)?;
    let in_tx = matches!(&target, Target::Tx(_));
    let dialect = match &target {
        Target::Tx(t) => require_known_dialect(&t.dialect_info, capability)?,
        Target::Pool(da) => require_known_dialect(da.dialect_info(), capability)?,
    };
    let alias = super::guard::bound_db(&state, &name);
    super::db_policy::granted_only(&state, &alias)?;
    let Some(sql) = super::introspect::listing_sql(listing, dialect) else {
        return Err(to_js_err(Box::new(crate::bridge::db_error::unsupported(
            dialect.as_str(),
            capability,
        ))));
    };
    let placeholder = match &target {
        Target::Tx(t) => t.dialect_info.caps.placeholder,
        Target::Pool(da) => da.dialect_info().caps.placeholder,
    };
    // 清单 SQL 无参数位（prefix 在 Rust 侧过滤）；normalize_and_trace 统一占位符改写。
    let params: Vec<Value> = Vec::new();
    let norm = normalize_and_trace(&state, placeholder, in_tx, &name, sql, &params)?;
    let start = std::time::Instant::now();
    let trace_params = &[Value::String("<db introspection>".to_string())];
    let res = match target {
        Target::Pool(da) => da.query_with_params(&norm, &params).await,
        Target::Tx(t) => t.session.lock().await.query(&norm, &params).await,
    };
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    match res {
        Ok(rows) => {
            let mut out = super::introspect::listing(rows);
            if let Some(p) = prefix.as_deref() {
                out.retain(|v| str_starts_with(v.get("name").and_then(|n| n.as_str()), p));
            }
            trace_sql(
                &state,
                sql,
                trace_params,
                &name,
                in_tx,
                ms,
                Some(out.len() as i64),
                true,
                None,
                None,
                None,
            );
            Ok(out)
        }
        Err(e) => {
            let em = e.to_string();
            trace_sql(
                &state,
                sql,
                trace_params,
                &name,
                in_tx,
                ms,
                None,
                false,
                Some(em.clone()),
                None,
                None,
            );
            Err(JsErrorBox::generic(em))
        }
    }
}

fn str_starts_with(name: Option<&str>, prefix: &str) -> bool {
    name.is_some_and(|n| n.starts_with(prefix))
}

/// db.listSchemas(prefix?)：schema 清单（REQ-04 §5.1）。
#[op2]
#[serde]
pub async fn op_db_list_schemas(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[string] prefix: Option<String>,
) -> Result<Vec<Value>, JsErrorBox> {
    listing_impl(state, name, prefix, super::introspect::Listing::Schemas).await
}

/// db.listDatabases(prefix?)：database 清单（REQ-04 §5.1；pg 上由任意库连出）。
#[op2]
#[serde]
pub async fn op_db_list_databases(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[string] prefix: Option<String>,
) -> Result<Vec<Value>, JsErrorBox> {
    listing_impl(state, name, prefix, super::introspect::Listing::Databases).await
}

/// 表名入参校验（安全 B3 / B4）：长度 > 128、含 NUL / 控制字符、或（sqlite）
/// 含 `.` → 视为非法，内省 op 返回空数组而非把畸形输入交给驱动（避免 500，也挡
/// `other.secrets` 形式的跨库内省）。
fn valid_introspect_table(dialect: Dialect, table: &str) -> bool {
    if table.len() > 128 {
        return false;
    }
    if table.bytes().any(|b| b == 0 || b < 0x20) {
        return false;
    }
    if dialect == Dialect::Sqlite && table.contains('.') {
        return false;
    }
    true
}

/// `db.listColumns(table)` / `listIndexes(table)` / `listForeignKeys(table)`（v0.1.90，L2）。
///
/// 三者共用 [`introspect`] 的方言 SQL 常量（表名**只经绑定参数**，零标识符内联），
/// 由宿主按方言固定 `LIMIT` 截断到 [`introspect::MAX_ROWS`]。返回结构与排序三方言一致
/// （见 `introspect.rs` 的 `columns` / `indexes` / `foreign_keys`）。
///
/// 授权：只过 [`db_policy::granted_only`]（内省恒只读，但授权维必须过——见该 fn 文档）。
/// 未知方言 → 结构化 `OJ_DB_UNSUPPORTED`（`data.capability` 点名），绝不回落 sqlite。
async fn introspect(
    state: Rc<RefCell<OpState>>,
    name: String,
    table: String,
    kind: super::introspect::Kind,
    scope: Option<String>,
) -> Result<Vec<Value>, JsErrorBox> {
    let target = resolve_target(&state, &name)?;
    let in_tx = matches!(&target, Target::Tx(_));
    let dialect = match &target {
        Target::Tx(t) => require_known_dialect(&t.dialect_info, kind.capability())?,
        Target::Pool(da) => require_known_dialect(da.dialect_info(), kind.capability())?,
    };
    // 授权维（安全 B1）：未授权主体不该枚举出别的库的表结构。
    let alias = super::guard::bound_db(&state, &name);
    super::db_policy::granted_only(&state, &alias)?;

    // 表名入参校验：畸形 / 跨库前缀 → 空数组（不报错、不泄漏）。
    if !valid_introspect_table(dialect, &table) {
        return Ok(Vec::new());
    }

    let mut sql = super::introspect::sql(kind, dialect).to_string();
    sql.push_str(&format!(" LIMIT {}", super::introspect::MAX_ROWS));
    let mut params = vec![Value::String(table)];
    // F3（REQ-04）：活动 scope——scope 值前置（表名参数在其后）。sqlite →
    // unsupported；畸形 scope → 空结果。
    let sql = match resolve_scope(&sql, dialect, scope.as_deref()) {
        Ok(Scope::Plain) => sql,
        Ok(Scope::Scoped(s, sc)) => {
            params.insert(0, Value::String(sc));
            s
        }
        Ok(Scope::Empty) => return Ok(Vec::new()),
        Err(e) => return Err(to_js_err(Box::new(e))),
    };

    let placeholder = match &target {
        Target::Tx(t) => t.dialect_info.caps.placeholder,
        Target::Pool(da) => da.dialect_info().caps.placeholder,
    };
    let norm = normalize_and_trace(&state, placeholder, in_tx, &name, &sql, &params)?;
    let start = std::time::Instant::now();
    // 强制脱敏（安全 B6）：trace 的 params 记为占位，表名不因 `redact=false` 进日志。
    let trace_params = &[Value::String("<db introspection>".to_string())];
    let res = match &target {
        Target::Pool(da) => da.query_with_params(&norm, &params).await,
        Target::Tx(t) => t.session.lock().await.query(&norm, &params).await,
    };
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    match res {
        Ok(rows) => {
            let out = match kind {
                super::introspect::Kind::Columns => super::introspect::columns(rows),
                super::introspect::Kind::Indexes => super::introspect::indexes(rows),
                super::introspect::Kind::ForeignKeys => super::introspect::foreign_keys(rows),
                // catalog 类只从 op_db_catalog 进（parse_catalog 白名单），L2 通路不可达。
                _ => unreachable!("catalog kinds do not flow through the L2 introspect path"),
            };
            trace_sql(
                &state,
                &sql,
                trace_params,
                &name,
                in_tx,
                ms,
                Some(out.len() as i64),
                true,
                None,
                None,
                None,
            );
            Ok(out)
        }
        Err(e) => {
            let em = e.to_string();
            trace_sql(
                &state,
                &sql,
                trace_params,
                &name,
                in_tx,
                ms,
                None,
                false,
                Some(em.clone()),
                None,
                None,
            );
            Err(JsErrorBox::generic(em))
        }
    }
}

/// db.listColumns(table)（v0.1.90）：列结构内省。
/// 返回 `{name, nativeType, nullable, default, primaryKey}[]`。
#[op2]
#[serde]
pub async fn op_db_list_columns(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[string] table: String,
    #[string] scope: Option<String>,
) -> Result<Vec<Value>, JsErrorBox> {
    introspect(state, name, table, super::introspect::Kind::Columns, scope).await
}

/// db.listIndexes(table)（v0.1.90）：索引内省。
/// 返回 `{name, unique, primary, origin, columns}[]`（origin ∈ `c`/`u`/`pk`）。
#[op2]
#[serde]
pub async fn op_db_list_indexes(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[string] table: String,
    #[string] scope: Option<String>,
) -> Result<Vec<Value>, JsErrorBox> {
    introspect(state, name, table, super::introspect::Kind::Indexes, scope).await
}

/// db.listForeignKeys(table)（v0.1.90）：外键内省。
/// 返回 `{name, columns, refTable, refColumns}[]`（sqlite 无名 → `name` 为 null）。
#[op2]
#[serde]
pub async fn op_db_list_foreign_keys(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[string] table: String,
    #[string] scope: Option<String>,
) -> Result<Vec<Value>, JsErrorBox> {
    introspect(
        state,
        name,
        table,
        super::introspect::Kind::ForeignKeys,
        scope,
    )
    .await
}
