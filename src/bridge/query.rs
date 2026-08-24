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
use sea_query::{Alias, Expr, ExprTrait, LikeExpr, Order, Query, SimpleExpr, SqliteQueryBuilder, Value as Qv};
use serde::Deserialize;
use serde_json::Value;

use super::db::Dialect;
use super::registry::SchemaRegistry;
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
struct Cond {
    field: String,
    op: Op,
    #[serde(default)]
    value: Option<Value>,
}

/// 排序项。
#[derive(Debug, Clone, Deserialize)]
struct OrderBy {
    field: String,
    #[serde(default)]
    dir: Option<String>,
}

/// 一次查询构建请求（结构化，非 SQL 字符串）。
#[derive(Debug, Clone, Deserialize)]
struct QueryReq {
    /// 目标命名库（bootstrap 的 queryBuilder 填入；缺省 default）。
    #[serde(default = "default_db")]
    db: String,
    table: String,
    #[serde(default)]
    columns: Vec<String>,
    #[serde(default)]
    conditions: Vec<Cond>,
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
    Ok(state
        .borrow()
        .borrow::<Arc<StableState>>()
        .registry
        .clone())
}

fn to_qv(v: &Value) -> Qv {
    match v {
        Value::Null => Qv::String(None),
        Value::Bool(b) => Qv::Bool(Some(*b)),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Qv::BigInt(Some(i.into()))
            } else if let Some(f) = n.as_f64() {
                Qv::Float(Some((f as f32).into()))
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
        Op::Gt => c.gt(rhs(val.as_ref().ok_or_else(|| JsErrorBox::generic("gt needs value"))?)),
        Op::Gte => c.gte(rhs(val.as_ref().ok_or_else(|| JsErrorBox::generic("gte needs value"))?)),
        Op::Lt => c.lt(rhs(val.as_ref().ok_or_else(|| JsErrorBox::generic("lt needs value"))?)),
        Op::Lte => c.lte(rhs(val.as_ref().ok_or_else(|| JsErrorBox::generic("lte needs value"))?)),
        Op::In => {
            let arr = val
                .as_ref()
                .and_then(|v| v.as_array())
                .ok_or_else(|| JsErrorBox::generic("in needs array value"))?;
            let vals: Vec<Expr> = arr.iter().map(|v| rhs(v)).collect();
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

/// op_db_query_build：结构化查询 -> 参数化 SQL -> DataAccessor.query_with_params。
/// 标识符（表/列）全部经 SchemaRegistry 白名单校验；值参数化。
#[op2]
#[serde]
pub async fn op_db_query_build(
    state: Rc<RefCell<OpState>>,
    #[serde] req: QueryReq,
) -> Result<Vec<Value>, JsErrorBox> {
    let reg = registry(&state)?;
    let table = reg
        .get(&req.table)
        .ok_or_else(|| JsErrorBox::generic(format!("unknown table '{}'", req.table)))?;

    // 列白名单校验（空 = SELECT *）。
    let cols: Vec<Alias> = if req.columns.is_empty() {
        table.columns.keys().map(|c| Alias::new(c.clone())).collect()
    } else {
        req.columns
            .iter()
            .map(|c| {
                if !table.has_column(c) {
                    Err(JsErrorBox::generic(format!("unknown column '{c}' on '{}'", req.table)))
                } else {
                    Ok(Alias::new(c.clone()))
                }
            })
            .collect::<Result<_, _>>()?
    };

    let mut q = Query::select();
    q.columns(cols).from(Alias::new(&req.table));

    for c in &req.conditions {
        if !table.has_column(&c.field) {
            return Err(JsErrorBox::generic(format!(
                "unknown column '{}' in where",
                c.field
            )));
        }
        q.and_where(build_expr(&c.field, c.op, &c.value)?);
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

    let da = lookup(&state, &req.db)?;
    let (sql, values) = build_select(da.dialect(), &q);
    let params: Vec<Value> = values
        .into_iter()
        .map(|v| value_to_json(&v))
        .collect::<Result<_, _>>()?;

    da.query_with_params(&sql, &params)
        .await
        .map_err(|e| JsErrorBox::generic(e.to_string()))
}

/// 按方言出 SQL（sea-query QueryBuilder 非 dyn 兼容，match 分发三实现）。
fn build_select(
    d: Dialect,
    q: &sea_query::SelectStatement,
) -> (String, sea_query::Values) {
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

/// 按名取 DataAccessor（默认 default；后续可扩展命名实例）。
pub(crate) fn lookup(
    state: &Rc<RefCell<OpState>>,
    name: &str,
) -> Result<Arc<dyn DataAccessor>, JsErrorBox> {
    state
        .borrow()
        .borrow::<Arc<StableState>>()
        .dbs
        .get(name)
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
            build_select(d, &q).0
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
        let req: QueryReq =
            serde_json::from_str(r#"{"db":"other","table":"user"}"#).unwrap();
        assert_eq!(req.db, "other");
    }
}
