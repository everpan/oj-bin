//! 安全查询构造器：以 sea-query 动态构建 SELECT，标识符全部来自 SchemaRegistry 白名单，
//! 值经参数化绑定。JS 侧经 `db.table(name).select(cols).where({field:op:val}).orderBy([...]).limit(n).all()` 调用。
//!
//! 设计取舍（见评审修订）：
//!   - v1 仅 `AND` 组合，无 `$or`/`$not`（后续可加深度/子句数上限）。
//!   - 过滤操作符收敛为类型化枚举：eq/ne/gt/gte/lt/lte/in/like/isNull。
//!   - orderBy 为 `[{field, dir}]`，不解析 SQL 片段。
//!   - limit 默认 100、硬上限 1000。

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use deno_core::{OpState, op2};
use deno_error::JsErrorBox;
use sea_query::{
    Alias, Expr, ExprTrait, LikeExpr, Order, Query, SimpleExpr, SqliteQueryBuilder, Value as Qv,
};
use serde::Deserialize;
use serde_json::Value;

use super::db::Dialect;
use super::registry::{SchemaRegistry, TableDef};
use super::{BridgeResult, DataAccessor, StableState};

/// 过滤操作符（类型化枚举，拒绝未知 `$op`）。
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Op {
    Eq,
    Ne,
    Gt,
    Gte,
    Lt,
    Lte,
    #[serde(rename = "in")]
    In,
    Like,
    IsNull,
}

/// 单条过滤条件（列名 + 操作符 + 值）。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cond {
    field: String,
    op: Op,
    #[serde(default)]
    value: Option<Value>,
}

/// 嵌套条件树（对齐 xorm And/Or/Not）。不用 untagged（serde 在 untagged 内
/// deny_unknown_fields 不生效，{field,op,value,or:[...]} 会被静默降级为 Leaf）——
/// 手写 Deserialize 按键唯一分发，多余键显式报错。
#[derive(Debug, Clone)]
enum CondTree {
    Leaf(Cond),
    And(Vec<CondTree>),
    Or(Vec<CondTree>),
    Not(Box<CondTree>),
}

impl<'de> Deserialize<'de> for CondTree {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        let m = serde_json::Map::<String, Value>::deserialize(d)?;
        let groups: Vec<&str> = ["and", "or", "not"]
            .into_iter()
            .filter(|k| m.contains_key(*k))
            .collect();
        if !groups.is_empty() {
            let extra: Vec<&String> = m.keys().filter(|k| !groups.contains(&k.as_str())).collect();
            if groups.len() != 1 || !extra.is_empty() {
                return Err(Error::custom(format!(
                    "condition group takes exactly one of and/or/not, unknown keys {extra:?}"
                )));
            }
            let parse_vec = |v: &Value| -> Result<Vec<CondTree>, D::Error> {
                let xs = Vec::<CondTree>::deserialize(v.clone()).map_err(Error::custom)?;
                if xs.is_empty() {
                    return Err(Error::custom("empty condition group"));
                }
                Ok(xs)
            };
            return match groups[0] {
                "and" => Ok(CondTree::And(parse_vec(&m["and"])?)),
                "or" => Ok(CondTree::Or(parse_vec(&m["or"])?)),
                _ => Ok(CondTree::Not(Box::new(
                    CondTree::deserialize(m["not"].clone()).map_err(Error::custom)?,
                ))),
            };
        }
        let leaf = Cond::deserialize(Value::Object(m)).map_err(Error::custom)?;
        Ok(CondTree::Leaf(leaf))
    }
}

/// 排序项。
#[derive(Debug, Clone, Deserialize)]
struct OrderBy {
    field: String,
    #[serde(default)]
    dir: Option<String>,
}

/// 查询动词（serde default = select，旧线格式零迁移）。
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Verb {
    #[default]
    Select,
    Insert,
    Update,
    Delete,
}

/// 一次查询构建请求（结构化，非 SQL 字符串）。
#[derive(Debug, Clone, Deserialize)]
struct QueryReq {
    /// 目标命名库（bootstrap 的 queryBuilder 填入；缺省 default）。
    #[serde(default = "default_db")]
    db: String,
    table: String,
    #[serde(default)]
    verb: Verb,
    #[serde(default)]
    values: Vec<serde_json::Map<String, Value>>,
    #[serde(default)]
    sets: serde_json::Map<String, Value>,
    #[serde(default)]
    columns: Vec<String>,
    #[serde(default)]
    conditions: Vec<CondTree>,
    #[serde(default)]
    order_by: Vec<OrderBy>,
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    offset: Option<u32>,
}

fn default_db() -> String {
    "default".into()
}

const LIMIT_DEFAULT: u32 = 100;
const LIMIT_MAX: u32 = 1000;

fn registry(state: &Rc<RefCell<OpState>>) -> Result<Arc<SchemaRegistry>, JsErrorBox> {
    Ok(state.borrow().borrow::<Arc<StableState>>().registry.clone())
}

fn to_qv(v: &Value) -> Qv {
    match v {
        Value::Null => Qv::String(None),
        Value::Bool(b) => Qv::Bool(Some(*b)),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Qv::BigInt(Some(i))
            } else if let Some(f) = n.as_f64() {
                Qv::Double(Some(f))
            } else {
                Qv::String(None)
            }
        }
        Value::String(s) => Qv::from(s.clone()),
        other => Qv::from(other.to_string()),
    }
}

fn build_expr(col: &str, op: Op, val: &Option<Value>) -> Result<SimpleExpr, JsErrorBox> {
    let c = Expr::col(Alias::new(col));
    let rhs = |v: &Value| Expr::val(to_qv(v));
    Ok(match op {
        Op::Eq => c.eq(rhs(val.as_ref().unwrap_or(&Value::Null))),
        Op::Ne => c.ne(rhs(val.as_ref().unwrap_or(&Value::Null))),
        Op::Gt => c.gt(rhs(val
            .as_ref()
            .ok_or_else(|| JsErrorBox::generic("gt needs value"))?)),
        Op::Gte => c.gte(rhs(val
            .as_ref()
            .ok_or_else(|| JsErrorBox::generic("gte needs value"))?)),
        Op::Lt => c.lt(rhs(val
            .as_ref()
            .ok_or_else(|| JsErrorBox::generic("lt needs value"))?)),
        Op::Lte => c.lte(rhs(val
            .as_ref()
            .ok_or_else(|| JsErrorBox::generic("lte needs value"))?)),
        Op::In => {
            let arr = val
                .as_ref()
                .and_then(|v| v.as_array())
                .ok_or_else(|| JsErrorBox::generic("in needs array value"))?;
            let vals: Vec<Expr> = arr.iter().map(rhs).collect();
            c.is_in(vals)
        }
        Op::Like => {
            let v = val
                .as_ref()
                .ok_or_else(|| JsErrorBox::generic("like needs value"))?;
            let pat = match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            c.like(LikeExpr::new(pat))
        }
        Op::IsNull => c.is_null(),
    })
}

const COND_DEPTH_MAX: usize = 8;
const COND_LEAF_MAX: usize = 64;

/// 条件树 → SimpleExpr（Phase 5 把 table 参数换成 ColCtx 以支持限定列）。
fn cond_expr(
    t: &CondTree,
    table: &TableDef,
    depth: usize,
    leaves: &mut usize,
) -> Result<SimpleExpr, JsErrorBox> {
    if depth > COND_DEPTH_MAX {
        return Err(JsErrorBox::generic("condition tree too deep (max 8)"));
    }
    match t {
        CondTree::Leaf(c) => {
            *leaves += 1;
            if *leaves > COND_LEAF_MAX {
                return Err(JsErrorBox::generic(
                    "condition tree too large (max 64 leaves)",
                ));
            }
            if !table.has_column(&c.field) {
                return Err(JsErrorBox::generic(format!(
                    "unknown column '{}' in where",
                    c.field
                )));
            }
            build_expr(&c.field, c.op, &c.value)
        }
        CondTree::And(xs) | CondTree::Or(xs) => {
            let mut cond = if matches!(t, CondTree::And(_)) {
                sea_query::Condition::all()
            } else {
                sea_query::Condition::any()
            };
            for x in xs {
                cond = cond.add(cond_expr(x, table, depth + 1, leaves)?);
            }
            Ok(SimpleExpr::from(cond))
        }
        CondTree::Not(x) => {
            let mut cond = sea_query::Condition::all();
            cond = cond.add(cond_expr(x, table, depth + 1, leaves)?);
            Ok(SimpleExpr::from(cond.not()))
        }
    }
}

/// op 前置守卫（两个 op 共用）：主表 check_table（Phase 5 扩展 join 表）。
fn guard_req(state: &Rc<RefCell<OpState>>, req: &QueryReq) -> Result<(), JsErrorBox> {
    super::guard::check_table(state, &req.table)
}

/// 动词×字段兼容矩阵（op 侧权威——fromJSON 可完全绕过 JS 链层）。
fn validate_verb(req: &QueryReq) -> Result<(), JsErrorBox> {
    let reject =
        |verb: &str, f: &str| Err(JsErrorBox::generic(format!("{verb} does not accept {f}")));
    match req.verb {
        Verb::Select => {
            if !req.values.is_empty() {
                return reject("select", "values");
            }
            if !req.sets.is_empty() {
                return reject("select", "sets");
            }
        }
        Verb::Insert => {
            if req.values.is_empty() {
                return Err(JsErrorBox::generic("insert needs at least one row"));
            }
            if !req.conditions.is_empty() {
                return reject("insert", "where");
            }
            if !req.order_by.is_empty() {
                return reject("insert", "orderBy");
            }
            if req.limit.is_some() || req.offset.is_some() {
                return reject("insert", "limit/offset");
            }
        }
        Verb::Update | Verb::Delete => {
            if req.verb == Verb::Update && req.sets.is_empty() {
                return Err(JsErrorBox::generic("update needs non-empty sets"));
            }
            // 空 and/or 组在 CondTree Deserialize 已拒 → 非空即叶子 ≥ 1。
            if req.conditions.is_empty() {
                return Err(JsErrorBox::generic(format!(
                    "{:?} requires where (leaf count >= 1)",
                    req.verb
                )));
            }
            if req.limit.is_some() || req.offset.is_some() {
                return reject("update/delete", "limit/offset");
            }
        }
    }
    Ok(())
}

/// 纯构造：白名单校验 → sea-query → 方言 SQL + JSON 参数（不触 OpState/连接/tx）。
/// 动词分发：select/insert/update/delete 四分支平级，列一律白名单校验、值一律参数化。
fn build_statement(
    req: &QueryReq,
    reg: &SchemaRegistry,
    dialect: Dialect,
) -> Result<(String, Vec<Value>), JsErrorBox> {
    validate_verb(req)?;
    let table = reg
        .get(&req.table)
        .ok_or_else(|| JsErrorBox::generic(format!("unknown table '{}'", req.table)))?;
    let params_of =
        |(sql, values): (String, sea_query::Values)| -> Result<(String, Vec<Value>), JsErrorBox> {
            let params = values.iter().map(value_to_json).collect::<Result<_, _>>()?;
            Ok((sql, params))
        };
    match req.verb {
        Verb::Select => build_select_body(req, table, dialect),
        Verb::Insert => {
            let keys: Vec<String> = req.values[0].keys().cloned().collect();
            for k in &keys {
                if !table.has_column(k) {
                    return Err(JsErrorBox::generic(format!(
                        "unknown column '{k}' in insert values"
                    )));
                }
            }
            let mut ins = Query::insert();
            ins.into_table(Alias::new(&req.table))
                .columns(keys.iter().map(Alias::new));
            for row in &req.values {
                if row.len() != keys.len() || !row.keys().all(|k| keys.contains(k)) {
                    return Err(JsErrorBox::generic(
                        "insert rows must share identical key sets",
                    ));
                }
                let vals: Vec<Expr> = keys.iter().map(|k| Expr::val(to_qv(&row[k]))).collect();
                ins.values(vals)
                    .map_err(|e| JsErrorBox::generic(format!("insert values: {e}")))?;
            }
            params_of(build_sql(dialect, &ins))
        }
        Verb::Update => {
            let mut up = Query::update();
            up.table(Alias::new(&req.table));
            for (k, v) in &req.sets {
                if !table.has_column(k) {
                    return Err(JsErrorBox::generic(format!(
                        "unknown column '{k}' in update sets"
                    )));
                }
                up.value(Alias::new(k), Expr::val(to_qv(v)));
            }
            let mut leaves = 0usize;
            for e in req
                .conditions
                .iter()
                .map(|c| cond_expr(c, table, 1, &mut leaves))
                .collect::<Result<Vec<_>, _>>()?
            {
                up.and_where(e);
            }
            params_of(build_sql(dialect, &up))
        }
        Verb::Delete => {
            let mut del = Query::delete();
            del.from_table(Alias::new(&req.table));
            let mut leaves = 0usize;
            for e in req
                .conditions
                .iter()
                .map(|c| cond_expr(c, table, 1, &mut leaves))
                .collect::<Result<Vec<_>, _>>()?
            {
                del.and_where(e);
            }
            params_of(build_sql(dialect, &del))
        }
    }
}

/// select 构造段（列白名单、条件、order_by、limit/offset）——Task 1 产出原样抽出。
fn build_select_body(
    req: &QueryReq,
    table: &TableDef,
    dialect: Dialect,
) -> Result<(String, Vec<Value>), JsErrorBox> {
    let cols: Vec<Alias> = if req.columns.is_empty() {
        table
            .columns
            .keys()
            .map(|c| Alias::new(c.clone()))
            .collect()
    } else {
        req.columns
            .iter()
            .map(|c| {
                if !table.has_column(c) {
                    Err(JsErrorBox::generic(format!(
                        "unknown column '{c}' on '{}'",
                        req.table
                    )))
                } else {
                    Ok(Alias::new(c.clone()))
                }
            })
            .collect::<Result<_, _>>()?
    };
    let mut q = Query::select();
    q.columns(cols).from(Alias::new(&req.table));
    let mut leaves = 0usize;
    for c in &req.conditions {
        q.and_where(cond_expr(c, table, 1, &mut leaves)?);
    }
    for o in &req.order_by {
        if !table.is_sortable(&o.field) {
            return Err(JsErrorBox::generic(format!(
                "column '{}' not sortable",
                o.field
            )));
        }
        let dir = match o.dir.as_deref() {
            Some("desc") => Order::Desc,
            _ => Order::Asc,
        };
        q.order_by(Alias::new(&o.field), dir);
    }
    let limit = Ord::min(req.limit.unwrap_or(LIMIT_DEFAULT), LIMIT_MAX);
    q.limit(limit as u64);
    if let Some(off) = req.offset {
        q.offset(off as u64);
    }
    let (sql, values) = build_sql(dialect, &q);
    let params: Vec<Value> = values.iter().map(value_to_json).collect::<Result<_, _>>()?;
    Ok((sql, params))
}

/// op_db_query_build：结构化查询 -> 参数化 SQL -> 执行。
/// select → rows 数组；DML → 受影响行数 number。DML 同走 resolve_target：
/// 本库活跃 tx → 会话 exec，无 tx → 池 exec_with_params，他库 tx → 报错。
/// 标识符（表/列）全部经 SchemaRegistry 白名单校验；值参数化。
#[op2]
#[serde]
pub async fn op_db_query_build(
    state: Rc<RefCell<OpState>>,
    #[serde] req: QueryReq,
) -> Result<serde_json::Value, JsErrorBox> {
    let reg = registry(&state)?;
    guard_req(&state, &req)?;
    let (sql, params) = build_statement(&req, &reg, lookup(&state, &req.db)?.dialect())?;
    let is_select = req.verb == Verb::Select;

    // 活跃事务路由：本库 tx 会话 / 无 tx 池 / 他库 tx 报错（同 db.rs）。
    match super::db::resolve_target(&state, &req.db)? {
        super::db::Target::Pool(da) => {
            if is_select {
                da.query_with_params(&sql, &params)
                    .await
                    .map(Value::Array)
                    .map_err(|e| JsErrorBox::generic(e.to_string()))
            } else {
                da.exec_with_params(&sql, &params)
                    .await
                    .map(Value::from)
                    .map_err(|e| JsErrorBox::generic(e.to_string()))
            }
        }
        super::db::Target::Tx(t) => {
            let s = t.session.lock().await;
            if is_select {
                s.query(&sql, &params)
                    .await
                    .map(Value::Array)
                    .map_err(|e| JsErrorBox::generic(e.to_string()))
            } else {
                s.exec(&sql, &params)
                    .await
                    .map(Value::from)
                    .map_err(|e| JsErrorBox::generic(e.to_string()))
            }
        }
    }
}

/// toSQL：与执行完全相同的两段（guard_req + build_statement），只构造不执行；
/// 不做 tx 路由、不受活跃 tx 影响。同步 op（全程 OpState 同步借用，无 await）。
#[op2]
#[serde]
pub fn op_db_query_sql(
    state: Rc<RefCell<OpState>>,
    #[serde] req: QueryReq,
) -> Result<serde_json::Value, JsErrorBox> {
    let reg = registry(&state)?;
    guard_req(&state, &req)?;
    let (sql, params) = build_statement(&req, &reg, lookup(&state, &req.db)?.dialect())?;
    Ok(serde_json::json!({ "sql": sql, "params": params }))
}

/// 按方言出 SQL（QueryStatementWriter::build 泛型，四类 statement 通吃）。
fn build_sql<S: sea_query::QueryStatementWriter>(d: Dialect, q: &S) -> (String, sea_query::Values) {
    match d {
        Dialect::Sqlite => q.build(SqliteQueryBuilder),
        Dialect::MySql => q.build(sea_query::MysqlQueryBuilder),
        Dialect::Postgres => q.build(sea_query::PostgresQueryBuilder),
    }
}

/// sea-query 的 `Value` 转 serde_json::Value（简化：整数/浮点/字符串/布尔/ null）。
fn value_to_json(v: &Qv) -> Result<Value, JsErrorBox> {
    let num = |f: f64| {
        serde_json::Number::from_f64(f)
            .map(Value::Number)
            .unwrap_or(Value::Null)
    };
    Ok(match v {
        Qv::Bool(Some(b)) => Value::Bool(*b),
        Qv::TinyInt(Some(i)) => Value::from(*i),
        Qv::SmallInt(Some(i)) => Value::from(*i),
        Qv::Int(Some(i)) => Value::from(*i),
        Qv::BigInt(Some(i)) => Value::from(*i),
        // sea-query 将 LIMIT/OFFSET 渲染为 unsigned 绑定参数，缺失会退化为 NULL 绑定。
        Qv::TinyUnsigned(Some(i)) => Value::from(*i as i64),
        Qv::SmallUnsigned(Some(i)) => Value::from(*i as i64),
        Qv::Unsigned(Some(i)) => Value::from(*i as i64),
        Qv::BigUnsigned(Some(i)) => Value::from(*i as i64),
        Qv::Float(Some(f)) => num(*f as f64),
        Qv::Double(Some(f)) => num(*f),
        Qv::String(Some(s)) => Value::String(s.to_string()),
        _ => Value::Null,
    })
}

/// 按名取 DataAccessor（默认 default）。模块 db 绑定在此收敛重定向：
/// manifest 声明 `db: <name>` 的模块，其字面 "default" 调用落到命名库（§5.3）；
/// 显式 DB("name") 不受影响。tx 以 JS 可见名记账，仅 accessor 解析被重定向。
pub(crate) fn lookup(
    state: &Rc<RefCell<OpState>>,
    name: &str,
) -> Result<Arc<dyn DataAccessor>, JsErrorBox> {
    let name = super::guard::bound_db(state, name);
    state
        .borrow()
        .borrow::<Arc<StableState>>()
        .dbs
        .get(&name)
        .cloned()
        .ok_or_else(|| JsErrorBox::generic(format!("db: instance '{name}' not configured")))
}

/// 仅用于 trait 约束引用，避免 unused import 警告。
#[allow(dead_code)]
fn _assert(_: &BridgeResult<()>) {}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn verb_matrix_enforced_op_side() {
        let bad = |req: &str| validate_verb(&serde_json::from_str(req).unwrap()).unwrap_err();
        // select 拒 values/sets
        assert!(
            bad(r#"{"table":"t","values":[{"a":1}]}"#)
                .to_string()
                .contains("select does not accept values")
        );
        // insert：空 values / 带 where / 带 limit
        assert!(
            bad(r#"{"table":"t","verb":"insert"}"#)
                .to_string()
                .contains("at least one row")
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
        // update：空 sets / 无 where / limit
        assert!(
            bad(
                r#"{"table":"t","verb":"update","conditions":[{"field":"a","op":"eq","value":1}]}"#
            )
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
        assert!(validate_verb(
            &serde_json::from_str(
                r#"{"table":"t","verb":"delete","conditions":[{"field":"a","op":"eq","value":1}]}"#
            )
            .unwrap()
        )
        .is_ok());
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
}
