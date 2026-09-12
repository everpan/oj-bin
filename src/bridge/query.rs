//! 安全查询构造器：以 sea-query 动态构建 SELECT，标识符全部来自 SchemaRegistry 白名单，
//! 值经参数化绑定。JS 侧经 `db.table(name).select(cols).where({field:op:val}).orderBy([...]).limit(n).all()` 调用。
//!
//! 设计取舍（见评审修订）：
//!   - v1 仅 `AND` 组合，无 `$or`/`$not`（后续可加深度/子句数上限）。
//!   - 过滤操作符收敛为类型化枚举：eq/ne/gt/gte/lt/lte/in/like/isNull。
//!   - orderBy 为 `[{field, dir}]`，不解析 SQL 片段。
//!   - limit 默认 100、硬上限 1000。

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use deno_core::{OpState, op2};
use deno_error::JsErrorBox;
use sea_query::{
    Alias, Expr, ExprTrait, IntoColumnRef, LikeExpr, Order, Query, SelectStatement, SimpleExpr,
    SqliteQueryBuilder, Value as Qv,
};
use serde::Deserialize;
use serde_json::Value;

use super::db::Dialect;
use super::registry::{SchemaRegistry, TableDef};
use super::{BridgeResult, DataAccessor, StableState};

/// 过滤操作符（类型化枚举，拒绝未知 `$op`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
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

/// 单条过滤条件（列名 + 操作符 + 值；subquery 见 Phase 8——值/子查询互斥在编译期报）。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cond {
    field: String,
    op: Op,
    #[serde(default)]
    value: Option<Value>,
    #[serde(default)]
    subquery: Option<Box<QueryReq>>,
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
    /// EXISTS (SELECT ...)——嵌套 select，层数由 REQ_NEST_MAX 管。
    Exists(Box<QueryReq>),
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
        if m.contains_key("exists") {
            if m.len() != 1 {
                return Err(Error::custom("exists takes no other keys"));
            }
            return Ok(CondTree::Exists(Box::new(
                QueryReq::deserialize(m["exists"].clone()).map_err(Error::custom)?,
            )));
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

/// join 种类（right 不做：sqlite 旧版本不支持且无用例）。
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum JoinKind {
    #[default]
    Inner,
    Left,
}

/// on 条件：列对列等值（无 op 字段，deny_unknown_fields 防自以为能写 op）。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct OnPair {
    left: String,
    right: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Join {
    table: String,
    #[serde(default)]
    kind: JoinKind,
    on: Vec<OnPair>,
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

/// 聚合函数（类型化枚举，非自由字符串——红线）。
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum AggFn {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

/// 聚合列参数（deny_unknown_fields 拒绝多余键）。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct AggSpec {
    #[serde(rename = "fn")]
    r#fn: AggFn,
    #[serde(default)]
    field: Option<String>,
    #[serde(rename = "as", default)]
    r#as: Option<String>,
}

/// select 列：列名（可限定 "t.col"）或聚合 {fn, field?, as?}。
/// 手写 Deserialize 按值类型分发——untagged 会吞内部错误（{fn:"median"} 只剩
/// "did not match any variant"，unknown variant 文案不可见）。
#[derive(Debug, Clone)]
enum ColSpec {
    Name(String),
    Agg(AggSpec),
}

impl<'de> Deserialize<'de> for ColSpec {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        match Value::deserialize(d)? {
            Value::String(s) => Ok(ColSpec::Name(s)),
            v @ Value::Object(_) => serde_json::from_value::<AggSpec>(v)
                .map(ColSpec::Agg)
                .map_err(Error::custom),
            _ => Err(Error::custom(
                "column must be a string or an aggregate object {fn, field?, as?}",
            )),
        }
    }
}

/// 别名形状纵深校验（发射只经 Alias::new 引号包裹，正则再挡一层）。
fn check_alias(a: &str) -> Result<(), JsErrorBox> {
    let ok = !a.is_empty()
        && a.bytes()
            .enumerate()
            .all(|(i, b)| b.is_ascii_alphabetic() || b == b'_' || (i > 0 && b.is_ascii_digit()));
    if ok {
        Ok(())
    } else {
        Err(JsErrorBox::generic(format!("illegal alias '{a}'")))
    }
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
    joins: Vec<Join>,
    #[serde(default)]
    columns: Vec<ColSpec>,
    #[serde(default)]
    distinct: bool,
    #[serde(default)]
    group_by: Vec<String>,
    #[serde(default)]
    having: Option<CondTree>,
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

/// op 编译为谓词表达式（泛化：左操作数可为任意 ExprTrait——having 别名展开 Phase 6 复用）。
fn apply_op<T: ExprTrait>(t: T, op: Op, val: &Option<Value>) -> Result<SimpleExpr, JsErrorBox> {
    let rhs = |v: &Value| Expr::val(to_qv(v));
    Ok(match op {
        Op::Eq => t.eq(rhs(val.as_ref().unwrap_or(&Value::Null))),
        Op::Ne => t.ne(rhs(val.as_ref().unwrap_or(&Value::Null))),
        Op::Gt => t.gt(rhs(val
            .as_ref()
            .ok_or_else(|| JsErrorBox::generic("gt needs value"))?)),
        Op::Gte => t.gte(rhs(val
            .as_ref()
            .ok_or_else(|| JsErrorBox::generic("gte needs value"))?)),
        Op::Lt => t.lt(rhs(val
            .as_ref()
            .ok_or_else(|| JsErrorBox::generic("lt needs value"))?)),
        Op::Lte => t.lte(rhs(val
            .as_ref()
            .ok_or_else(|| JsErrorBox::generic("lte needs value"))?)),
        Op::In => {
            let arr = val
                .as_ref()
                .and_then(|v| v.as_array())
                .ok_or_else(|| JsErrorBox::generic("in needs array value"))?;
            let vals: Vec<Expr> = arr.iter().map(rhs).collect();
            t.is_in(vals)
        }
        Op::Like => {
            let v = val
                .as_ref()
                .ok_or_else(|| JsErrorBox::generic("like needs value"))?;
            let pat = match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            t.like(LikeExpr::new(pat))
        }
        Op::IsNull => t.is_null(),
    })
}

/// 限定列解析上下文（select/where/orderBy/groupBy/having/join on 六处共用）。
struct ColCtx<'a> {
    base_name: &'a str,
    base: &'a TableDef,
    joins: Vec<(&'a str, &'a TableDef)>,
}

impl ColCtx<'_> {
    /// 列引用 → 所属表定义：`"t.col"` 表段 ∈ {基表} ∪ {join 表}，列段对该表校验；
    /// 非限定仅解析基表（不查 join 表——天然拒绝歧义，强制全限定名）。
    fn table_of(&self, col: &str, site: &str) -> Result<&TableDef, JsErrorBox> {
        match col.split_once('.') {
            Some((t, c)) => {
                let td = if t == self.base_name {
                    self.base
                } else if let Some((_, td)) = self.joins.iter().find(|(n, _)| *n == t) {
                    td
                } else {
                    return Err(JsErrorBox::generic(format!(
                        "unknown table '{t}' in {site}"
                    )));
                };
                if !td.has_column(c) {
                    return Err(JsErrorBox::generic(format!(
                        "unknown column '{col}' in {site}"
                    )));
                }
                Ok(td)
            }
            None if self.base.has_column(col) => Ok(self.base),
            None => Err(JsErrorBox::generic(format!(
                "unknown column '{col}' in {site}"
            ))),
        }
    }

    fn check_col(&self, col: &str, site: &str) -> Result<(), JsErrorBox> {
        self.table_of(col, site).map(|_| ())
    }
}

/// 列引用 → ColumnRef（qualified → (table, col) 元组；join on 的列对列等值也用它）。
fn col_ref(col: &str) -> sea_query::ColumnRef {
    match col.split_once('.') {
        Some((t, c)) => (Alias::new(t), Alias::new(c)).into(),
        None => Alias::new(col).into(),
    }
}

/// 列引用 → 列表达式。
/// 注：sea-query 1.0 起 `SimpleExpr` 即 `Expr` 的类型别名，无需转换。
fn col_simple_expr(col: &str) -> SimpleExpr {
    Expr::col(col_ref(col))
}

const COND_DEPTH_MAX: usize = 8;
const COND_LEAF_MAX: usize = 64;

/// 嵌套 select 最大层数（子查询/exists 共用；顶层为 0）。
const REQ_NEST_MAX: u8 = 4;

/// 嵌套 select 通用约束（site 用于报错定位：subquery/exists/union/cte）。
/// with/unions 的禁带判断由 Task 16/18 落地字段后各自补上。
fn validate_nested(req: &QueryReq, depth: u8, site: &str) -> Result<(), JsErrorBox> {
    if depth >= REQ_NEST_MAX {
        return Err(JsErrorBox::generic(format!(
            "{site}: nested select too deep"
        )));
    }
    if req.verb != Verb::Select {
        return Err(JsErrorBox::generic(format!(
            "{site}: nested select must be select"
        )));
    }
    Ok(())
}

/// 叶子内的子查询编译（where/having 叶子共用）：无 subquery → None。
/// op ∈ in/eq/ne/gt/gte/lt/lte；value 与 subquery 互斥；isnull/like 不接受。
fn leaf_subquery(
    c: &Cond,
    ctx: &ColCtx<'_>,
    reg: &SchemaRegistry,
    sel_depth: u8,
    site: &str,
) -> Result<Option<SimpleExpr>, JsErrorBox> {
    let Some(sub) = c.subquery.as_deref() else {
        return Ok(None);
    };
    if c.op == Op::IsNull {
        return Err(JsErrorBox::generic("isnull does not accept subquery"));
    }
    if c.op == Op::Like {
        return Err(JsErrorBox::generic("like does not accept subquery"));
    }
    if c.value.is_some() {
        return Err(JsErrorBox::generic(
            "value and subquery are mutually exclusive",
        ));
    }
    validate_nested(sub, sel_depth + 1, "subquery")?;
    ctx.check_col(&c.field, site)?;
    let col = col_simple_expr(&c.field);
    let sel = build_select_stmt(sub, reg, sel_depth + 1)?;
    // ExprTrait 的 eq/ne/gt/gte/lt/lte 接受 R: Into<Expr>（SelectStatement 可转）；
    // in 走 in_subquery——比较 op 渲染为 `col = (SELECT ...)` 标量子查询。
    Ok(Some(match c.op {
        Op::In => Expr::expr(col).in_subquery(sel),
        Op::Eq => Expr::expr(col).eq(sel),
        Op::Ne => Expr::expr(col).ne(sel),
        Op::Gt => Expr::expr(col).gt(sel),
        Op::Gte => Expr::expr(col).gte(sel),
        Op::Lt => Expr::expr(col).lt(sel),
        Op::Lte => Expr::expr(col).lte(sel),
        Op::Like | Op::IsNull => unreachable!("rejected above"),
    }))
}

/// 条件树 → SimpleExpr（树形递归一份，叶子解析由 site 注入；exists 臂由调用方注入——
/// 子查询编译需要 reg/嵌套层数，闭包捕获而非泛化参数）。
fn cond_expr_generic(
    t: &CondTree,
    leaf: &mut dyn FnMut(&Cond) -> Result<SimpleExpr, JsErrorBox>,
    exists: &mut dyn FnMut(&QueryReq) -> Result<SimpleExpr, JsErrorBox>,
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
            leaf(c)
        }
        CondTree::Exists(sub) => {
            *leaves += 1;
            if *leaves > COND_LEAF_MAX {
                return Err(JsErrorBox::generic(
                    "condition tree too large (max 64 leaves)",
                ));
            }
            exists(sub)
        }
        CondTree::And(xs) | CondTree::Or(xs) => {
            let mut cond = if matches!(t, CondTree::And(_)) {
                sea_query::Condition::all()
            } else {
                sea_query::Condition::any()
            };
            for x in xs {
                cond = cond.add(cond_expr_generic(x, leaf, exists, depth + 1, leaves)?);
            }
            Ok(SimpleExpr::from(cond))
        }
        CondTree::Not(x) => {
            let mut cond = sea_query::Condition::all();
            cond = cond.add(cond_expr_generic(x, leaf, exists, depth + 1, leaves)?);
            Ok(SimpleExpr::from(cond.not()))
        }
    }
}

/// 条件树 → SimpleExpr（where 叶子：白名单列，限定列经 ColCtx 支持联表；
/// sel_depth 为嵌套 select 层数，随子查询/exists 递归 +1）。
fn cond_expr(
    t: &CondTree,
    ctx: &ColCtx<'_>,
    reg: &SchemaRegistry,
    sel_depth: u8,
    depth: usize,
    leaves: &mut usize,
) -> Result<SimpleExpr, JsErrorBox> {
    cond_expr_generic(
        t,
        &mut |c| {
            if let Some(e) = leaf_subquery(c, ctx, reg, sel_depth, "where")? {
                return Ok(e);
            }
            ctx.check_col(&c.field, "where")?;
            apply_op(col_simple_expr(&c.field), c.op, &c.value)
        },
        &mut |sub| {
            validate_nested(sub, sel_depth + 1, "exists")?;
            Ok(Expr::exists(build_select_stmt(sub, reg, sel_depth + 1)?))
        },
        depth,
        leaves,
    )
}

/// 条件树 → SimpleExpr（having 叶子：白名单列优先，未命中查聚合别名台账展开——
/// PG 不允许 HAVING 引用 select 输出别名，展开消灭方言分叉；subquery/exists 同 where）。
fn cond_expr_having(
    t: &CondTree,
    ctx: &ColCtx<'_>,
    aliases: &HashMap<String, SimpleExpr>,
    reg: &SchemaRegistry,
    sel_depth: u8,
    depth: usize,
    leaves: &mut usize,
) -> Result<SimpleExpr, JsErrorBox> {
    cond_expr_generic(
        t,
        &mut |c| {
            if let Some(e) = leaf_subquery(c, ctx, reg, sel_depth, "having")? {
                return Ok(e);
            }
            if ctx.check_col(&c.field, "having").is_ok() {
                apply_op(col_simple_expr(&c.field), c.op, &c.value)
            } else {
                let e = aliases.get(&c.field).ok_or_else(|| {
                    JsErrorBox::generic(format!("unknown column '{}' in having", c.field))
                })?;
                apply_op(Expr::expr(e.clone()), c.op, &c.value)
            }
        },
        &mut |sub| {
            validate_nested(sub, sel_depth + 1, "exists")?;
            Ok(Expr::exists(build_select_stmt(sub, reg, sel_depth + 1)?))
        },
        depth,
        leaves,
    )
}

/// op 前置守卫（两个 op 共用）：主表与 join 表 check_table；条件树（含 having）
/// 内所有子查询/exists 的嵌套 req 递归过守卫（与 cond_expr 同构遍历）。
fn guard_req(state: &Rc<RefCell<OpState>>, req: &QueryReq) -> Result<(), JsErrorBox> {
    super::guard::check_table(state, &req.table)?;
    for j in &req.joins {
        super::guard::check_table(state, &j.table)?;
    }
    for c in &req.conditions {
        guard_nested(state, c)?;
    }
    if let Some(h) = &req.having {
        guard_nested(state, h)?;
    }
    Ok(())
}

/// 条件树遍历，嵌套 req 递归 guard_req。
fn guard_nested(state: &Rc<RefCell<OpState>>, t: &CondTree) -> Result<(), JsErrorBox> {
    match t {
        CondTree::Leaf(c) => {
            if let Some(sub) = &c.subquery {
                guard_req(state, sub)?;
            }
            Ok(())
        }
        CondTree::And(xs) | CondTree::Or(xs) => {
            for x in xs {
                guard_nested(state, x)?;
            }
            Ok(())
        }
        CondTree::Not(x) => guard_nested(state, x),
        CondTree::Exists(sub) => guard_req(state, sub),
    }
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
            if !req.joins.is_empty() {
                return reject("insert", "joins");
            }
            if req.distinct {
                return reject("insert", "distinct");
            }
            if !req.group_by.is_empty() {
                return reject("insert", "groupBy");
            }
            if req.having.is_some() {
                return reject("insert", "having");
            }
        }
        Verb::Update | Verb::Delete => {
            if !req.joins.is_empty() {
                return reject("update/delete", "joins");
            }
            if req.distinct {
                return reject("update/delete", "distinct");
            }
            if !req.group_by.is_empty() {
                return reject("update/delete", "groupBy");
            }
            if req.having.is_some() {
                return reject("update/delete", "having");
            }
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
    // join 表先行解析（自 join / 空 on / 未知表在此拒绝；DML 带 joins 已被矩阵拒绝）。
    let mut join_defs: Vec<(&str, &TableDef)> = Vec::new();
    for j in &req.joins {
        if j.table == req.table {
            return Err(JsErrorBox::generic(
                "self join not supported (no table alias)",
            ));
        }
        if j.on.is_empty() {
            return Err(JsErrorBox::generic(format!(
                "join '{}' needs non-empty on",
                j.table
            )));
        }
        let td = reg
            .get(&j.table)
            .ok_or_else(|| JsErrorBox::generic(format!("unknown table '{}'", j.table)))?;
        join_defs.push((j.table.as_str(), td));
    }
    let ctx = ColCtx {
        base_name: &req.table,
        base: table,
        joins: join_defs,
    };
    let params_of =
        |(sql, values): (String, sea_query::Values)| -> Result<(String, Vec<Value>), JsErrorBox> {
            let params = values.iter().map(value_to_json).collect::<Result<_, _>>()?;
            Ok((sql, params))
        };
    match req.verb {
        Verb::Select => {
            // 方言 build 只在最顶层做一次，参数由 sea-query 跨整棵语句树统一收集。
            let sel = build_select_stmt(req, reg, 0)?;
            params_of(build_sql(dialect, &sel))
        }
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
                .map(|c| cond_expr(c, &ctx, reg, 0, 1, &mut leaves))
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
                .map(|c| cond_expr(c, &ctx, reg, 0, 1, &mut leaves))
                .collect::<Result<Vec<_>, _>>()?
            {
                del.and_where(e);
            }
            params_of(build_sql(dialect, &del))
        }
    }
}

/// 构造 select 语句（含 join/where/group/having/order/limit）；depth 为嵌套层数。
/// 顶层（depth=0）由 build_statement 调用后统一 build；嵌套层被子查询/exists 复用，
/// 不单独 build（参数由 sea-query 跨整棵语句树统一收集）。
fn build_select_stmt(
    req: &QueryReq,
    reg: &SchemaRegistry,
    depth: u8,
) -> Result<SelectStatement, JsErrorBox> {
    let table = reg
        .get(&req.table)
        .ok_or_else(|| JsErrorBox::generic(format!("unknown table '{}'", req.table)))?;
    // join 表先行解析（自 join / 空 on / 未知表在此拒绝）。
    let mut join_defs: Vec<(&str, &TableDef)> = Vec::new();
    for j in &req.joins {
        if j.table == req.table {
            return Err(JsErrorBox::generic(
                "self join not supported (no table alias)",
            ));
        }
        if j.on.is_empty() {
            return Err(JsErrorBox::generic(format!(
                "join '{}' needs non-empty on",
                j.table
            )));
        }
        let td = reg
            .get(&j.table)
            .ok_or_else(|| JsErrorBox::generic(format!("unknown table '{}'", j.table)))?;
        join_defs.push((j.table.as_str(), td));
    }
    let ctx = ColCtx {
        base_name: &req.table,
        base: table,
        joins: join_defs,
    };
    let mut q = Query::select();
    // 聚合别名台账（alias → 原表达式），having 展开用（Task 11）。
    let mut agg_aliases: HashMap<String, SimpleExpr> = HashMap::new();
    if req.columns.is_empty() {
        // 全列；带 join 时全部限定为基表列（两表同名列如 id 歧义）。
        let cols: Vec<SimpleExpr> = if req.joins.is_empty() {
            ctx.base
                .columns
                .keys()
                .map(|c| col_simple_expr(c))
                .collect()
        } else {
            ctx.base
                .columns
                .keys()
                .map(|c| col_simple_expr(&format!("{}.{c}", req.table)))
                .collect()
        };
        q.exprs(cols);
    } else {
        // site 复刻既有报错文案形态 `unknown column '<c>' on '<table>'`（既有测试锁定）。
        let site = format!("on '{}'", req.table);
        for spec in &req.columns {
            match spec {
                ColSpec::Name(c) => {
                    ctx.check_col(c, &site)?;
                    q.expr(col_simple_expr(c));
                }
                ColSpec::Agg(a) => {
                    let AggSpec { r#fn, field, r#as } = a;
                    let arg: SimpleExpr = match (r#fn, field.as_deref()) {
                        (AggFn::Count, None) | (AggFn::Count, Some("*")) => {
                            Expr::col(sea_query::Asterisk)
                        }
                        (_, None) => {
                            return Err(JsErrorBox::generic(
                                "aggregate needs field (only count allows omission)",
                            ));
                        }
                        (_, Some(f)) => {
                            ctx.check_col(f, &site)?;
                            col_simple_expr(f)
                        }
                    };
                    let e: SimpleExpr = match r#fn {
                        AggFn::Count => sea_query::Func::count(arg),
                        AggFn::Sum => sea_query::Func::sum(arg),
                        AggFn::Avg => sea_query::Func::avg(arg),
                        AggFn::Min => sea_query::Func::min(arg),
                        AggFn::Max => sea_query::Func::max(arg),
                    }
                    .into();
                    if let Some(a) = r#as {
                        check_alias(a)?;
                        agg_aliases.insert(a.clone(), e.clone());
                        q.expr_as(e, Alias::new(a));
                    } else {
                        q.expr(e);
                    }
                }
            }
        }
    }
    if req.distinct {
        q.distinct();
    }
    q.from(Alias::new(&req.table));
    for j in &req.joins {
        let mut on = sea_query::Condition::all();
        for p in &j.on {
            ctx.check_col(&p.left, "join on")?;
            ctx.check_col(&p.right, "join on")?;
            on = on.add(col_simple_expr(&p.left).equals(col_ref(&p.right)));
        }
        let jt = match j.kind {
            JoinKind::Inner => sea_query::JoinType::InnerJoin,
            JoinKind::Left => sea_query::JoinType::LeftJoin,
        };
        q.join(jt, Alias::new(&j.table), on);
    }
    let mut leaves = 0usize;
    for c in &req.conditions {
        q.and_where(cond_expr(c, &ctx, reg, depth, 1, &mut leaves)?);
    }
    for g in &req.group_by {
        ctx.check_col(g, "groupBy")?;
    }
    if !req.group_by.is_empty() {
        q.group_by_columns(req.group_by.iter().map(|g| match g.split_once('.') {
            Some((t, c)) => (Alias::new(t), Alias::new(c)).into_column_ref(),
            None => Alias::new(g).into_column_ref(),
        }));
    }
    if let Some(h) = &req.having {
        let mut leaves = 0;
        let e = cond_expr_having(h, &ctx, &agg_aliases, reg, depth, 1, &mut leaves)?;
        let mut cond = sea_query::Condition::all();
        cond = cond.add(e);
        q.cond_having(cond);
    }
    for o in &req.order_by {
        let td = ctx.table_of(&o.field, "orderBy")?;
        let bare = o.field.rsplit('.').next().unwrap_or(&o.field);
        if !td.is_sortable(bare) {
            return Err(JsErrorBox::generic(format!(
                "column '{}' not sortable",
                o.field
            )));
        }
        let dir = match o.dir.as_deref() {
            Some("desc") => Order::Desc,
            _ => Order::Asc,
        };
        q.order_by_expr(col_simple_expr(&o.field), dir);
    }
    let limit = Ord::min(req.limit.unwrap_or(LIMIT_DEFAULT), LIMIT_MAX);
    q.limit(limit as u64);
    if let Some(off) = req.offset {
        q.offset(off as u64);
    }
    Ok(q)
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

    /// 两表夹具：a(2 行) × b(3 行，aid 指向 a.id)，验证 join 装配真实参与执行。
    async fn seeded_bridge_2t() -> Bridge {
        let db = SqlxAccessor::arc("sqlite::memory:").await.unwrap();
        db.exec_with_params("create table a (id integer primary key, name text)", &[])
            .await
            .unwrap();
        db.exec_with_params(
            "create table b (id integer primary key, aid integer, label text)",
            &[],
        )
        .await
        .unwrap();
        for (n,) in [("x",), ("y",)] {
            db.exec_with_params("insert into a (name) values (?)", &[json!(n)])
                .await
                .unwrap();
        }
        for (aid, l) in [(1, "L1"), (1, "L2"), (3, "L3")] {
            db.exec_with_params(
                "insert into b (aid, label) values (?, ?)",
                &[json!(aid), json!(l)],
            )
            .await
            .unwrap();
        }
        let reg = SchemaRegistry::new()
            .table("a", &["id"], &["id", "name"])
            .table("b", &["id"], &["id", "aid", "label"]);
        Bridge::with_opts(db, Arc::new(InMemoryKV::new()), reg, false)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn join_inner_left_and_rejections() {
        let b = seeded_bridge_2t().await;
        // inner join：a(id=1 x, 2 y) × b(aid=1 ×2) → 2 行
        let cap = b
            .run(
                r#"db.table("a").join("b", [{left:"a.id",right:"b.aid"}])
        .select(["a.name","b.label"]).all()
        .then(r=>json.ok({n:r.length, first:r[0].label})).catch(e=>json.fail(400,String(e)));"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(v["data"]["n"], 2, "{v}");
        // left join：3 行（x×2, y×1 NULL label）
        let cap = b
            .run(
                r#"db.table("a").join("b", [{left:"a.id",right:"b.aid"}], "left")
        .select(["a.name","b.label"]).orderBy([{field:"a.id",dir:"asc"}]).all()
        .then(r=>json.ok({n:r.length})).catch(e=>json.fail(400,String(e)));"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["data"]["n"], 3, "{v}");
        // join 表未知 / 自 join / on 列未知 / 非限定列命中 join 表
        for (js, want) in [
            (
                r#"db.table("a").join("nope",[{left:"a.id",right:"nope.aid"}]).select(["a.id"]).all()"#,
                "unknown table 'nope'",
            ),
            (
                r#"db.table("a").join("a",[{left:"a.id",right:"a.id"}]).select(["a.id"]).all()"#,
                "self join not supported",
            ),
            (
                r#"db.table("a").join("b",[{left:"a.id",right:"b.nope"}]).select(["a.id"]).all()"#,
                "unknown column 'b.nope'",
            ),
            (
                r#"db.table("a").join("b",[{left:"a.id",right:"b.aid"}]).select(["label"]).all()"#,
                "unknown column 'label'",
            ),
        ] {
            let cap = b
                .run(&format!(
                    r#"{js}.then(()=>json.ok({{}})).catch(e=>json.fail(400,String(e)));"#
                ))
                .await
                .unwrap();
            let v: Value = serde_json::from_slice(&cap.body).unwrap();
            assert!(v["msg"].as_str().unwrap().contains(want), "{want}: {v}");
        }
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
    fn col_ctx_qualified_and_unqualified_rules() {
        let reg = SchemaRegistry::new()
            .table("a", &["id"], &["id", "name"])
            .table("b", &["id"], &["id", "aid", "label"]);
        let ctx = ColCtx {
            base_name: "a",
            base: reg.get("a").unwrap(),
            joins: vec![("b", reg.get("b").unwrap())],
        };
        assert!(ctx.check_col("name", "select").is_ok());
        assert!(ctx.check_col("b.label", "select").is_ok());
        assert!(ctx.check_col("a.id", "where").is_ok());
        assert!(ctx.check_col("label", "select").is_err()); // 非限定不解析 join 表
        assert!(ctx.check_col("b.nope", "select").is_err());
        assert!(ctx.check_col("c.id", "select").is_err()); // 表段不在 {a, b}
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
        assert!(bad(
            r#"{"table":"t","verb":"insert","values":[{"a":1}],"joins":[{"table":"b","on":[{"left":"a.id","right":"b.aid"}]}]}"#
        )
        .to_string()
        .contains("insert does not accept joins"));
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

    #[tokio::test(flavor = "current_thread")]
    async fn aggregate_columns_alias_and_distinct() {
        let b = seeded_bridge().await;
        let cap = b
            .run(
                r#"Promise.all([
                     db.table("t").select([{fn:"count",as:"n"}]).all(),
                     db.table("t").select([{fn:"sum",field:"age",as:"total"}]).all(),
                     db.table("t").select(["tag"]).distinct().all(),
                   ]).then(([c, s, d]) => json.ok({ n: c[0].n, total: s[0].total, d: d.length }))
                     .catch(e => json.fail(500, String(e)));"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(v["data"]["n"], 4, "{v}");
        assert_eq!(v["data"]["total"], 100, "{v}"); // 10+20+30+40
        assert_eq!(v["data"]["d"], 3, "{v}"); // x, y, NULL
        // 非法别名 / 未知 fn / sum 缺 field
        for (sel, want) in [
            (r#"[{fn:"count",as:"1bad"}]"#, "illegal alias"),
            (r#"[{fn:"median",field:"age"}]"#, "unknown variant"),
            (r#"[{fn:"sum"}]"#, "aggregate needs field"),
        ] {
            let cap = b
                .run(&format!(
                    r#"db.table("t").select({sel}).all().then(()=>json.ok({{}})).catch(e=>json.fail(400,String(e)));"#
                ))
                .await
                .unwrap();
            let v: Value = serde_json::from_slice(&cap.body).unwrap();
            assert!(v["msg"].as_str().unwrap().contains(want), "{want}: {v}");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn group_by_having_with_alias_expansion() {
        let b = seeded_bridge().await;
        // 按 tag 分组 count>1 → tag=x (2 行)
        let cap = b
            .run(
                r#"db.table("t").select(["tag", {fn:"count",as:"n"}])
                   .groupBy(["tag"]).having({field:"n",op:"gt",value:1})
                   .orderBy([{field:"tag",dir:"asc"}]).all()
                   .then(r => json.ok({ rows: r.length, tag: r[0].tag, n: r[0].n }))
                   .catch(e => json.fail(500, String(e)));"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(v["data"]["rows"], 1, "{v}");
        assert_eq!(v["data"]["tag"], "x", "{v}");
        assert_eq!(v["data"]["n"], 2, "{v}");
        // having 用白名单列（列优先于别名）
        let cap = b
            .run(
                r#"db.table("t").select(["tag", {fn:"sum",field:"age",as:"n"}])
                   .groupBy(["tag"]).having({field:"age",op:"gt",value:0})
                   .all().then(r => json.ok({ n: r.length })).catch(e => json.fail(500, String(e)));"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        // having 引用未知别名/列 → 报错
        let cap = b.run(r#"db.table("t").select(["tag"]).groupBy(["tag"]).having({field:"zz",op:"eq",value:1}).all()
            .then(()=>json.ok({})).catch(e=>json.fail(400,String(e)));"#).await.unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(
            v["msg"]
                .as_str()
                .unwrap()
                .contains("unknown column 'zz' in having"),
            "{v}"
        );
        // 别名展开的 SQL 不含别名字样（方言验证：展开后 HAVING 引用原聚合表达式）
        let cap = b.run(r#"json.ok(db.table("t").select([{fn:"count",as:"n"}]).groupBy(["tag"]).having({field:"n",op:"gt",value:1}).toSQL());"#).await.unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        let sql = v["data"]["sql"].as_str().unwrap().to_string();
        assert!(
            sql.contains("COUNT(") && !sql.contains("HAVING \"n\""),
            "{sql}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn to_json_from_json_roundtrip() {
        let b = seeded_bridge().await;
        let cap = b
            .run(
                r#"(async () => {
                   const c = db.and({field:"age",op:"gte",value:18});
                   const q = db.table("t").select(["name"]).where(c).limit(10);
                   const snap = q.toJSON();
                   snap.limit = 1;
                   const via = await db.fromJSON(snap).all();
                   const direct = await db.table("t").select(["name"]).where(c).limit(1).all();
                   json.ok({ same: via.length === direct.length && via[0].name === direct[0].name,
                             plain: typeof snap.conditions[0].and !== "undefined" });
                 })().catch(e => json.fail(500, String(e)));"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(v["data"]["same"], true, "{v}");
        assert_eq!(v["data"]["plain"], true, "{v}"); // 条件对象已解包为纯 JSON
        // fromJSON 非法树被拒（同手写非法树：delete 无 where → JS 链层同步早抛，须 async 包裹）
        let cap = b
            .run(
                r#"(async () => db.fromJSON({table:"t",verb:"delete"}).run())()
                   .then(()=>json.ok({})).catch(e=>json.fail(400,String(e)));"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(v["msg"].as_str().unwrap().contains("requires where"), "{v}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn subquery_where_and_exists() {
        let b = seeded_bridge_2t().await;
        // in 子查询：b 中 label=L1 的 aid={1} → a.id ∈ {1} → 1 行 x
        let cap = b
            .run(
                r#"db.table("a").select(["name"])
        .where({field:"id",op:"in",subquery:db.table("b").select(["aid"])
            .where({field:"label",op:"eq",value:"L1"})}).all()
        .then(r=>json.ok({n:r.length,name:r[0].name})).catch(e=>json.fail(400,String(e)));"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["data"]["n"], 1, "{v}");
        assert_eq!(v["data"]["name"], "x", "{v}");
        // 标量 eq 子查询：aid of L1 = 1 → a.id = 1 → 1 行
        let cap = b
            .run(
                r#"db.table("a").select(["name"])
        .where({field:"id",op:"eq",subquery:db.table("b").select(["aid"])
            .where({field:"label",op:"eq",value:"L1"}).limit(1)}).all()
        .then(r=>json.ok({n:r.length})).catch(e=>json.fail(400,String(e)));"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["data"]["n"], 1, "{v}");
        // exists（非关联）：b 有 L2 → a 全量 2 行
        let cap = b.run(r#"db.table("a").select(["name"])
        .where({exists:db.table("b").select(["aid"]).where({field:"label",op:"eq",value:"L2"})}).all()
        .then(r=>json.ok({n:r.length})).catch(e=>json.fail(400,String(e)));"#).await.unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["data"]["n"], 2, "{v}");
        // 裸 JSON 树等价（不走 builder 包装）
        let cap = b
            .run(
                r#"db.table("a").select(["name"])
        .where({field:"id",op:"in",subquery:{table:"b",columns:["aid"],
            conditions:[{field:"label",op:"eq",value:"L1"}]}}).all()
        .then(r=>json.ok({n:r.length})).catch(e=>json.fail(400,String(e)));"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["data"]["n"], 1, "{v}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn subquery_rejections() {
        let b = seeded_bridge_2t().await;
        for (js, want) in [
            // value 与 subquery 同现
            (
                r#"db.table("a").where({field:"id",op:"in",value:[1],subquery:{table:"b",columns:["aid"]}}).all()"#,
                "value and subquery are mutually exclusive",
            ),
            // isnull 不接受 subquery
            (
                r#"db.table("a").where({field:"id",op:"isnull",subquery:{table:"b",columns:["aid"]}}).all()"#,
                "isnull does not accept subquery",
            ),
            // 嵌套 req 动词非 select
            (
                r#"db.table("a").where({field:"id",op:"in",subquery:{table:"b",verb:"delete",columns:["aid"],
                 conditions:[{field:"aid",op:"eq",value:1}]}}).all()"#,
                "nested select",
            ),
            // 嵌套 req 未知列（递归过白名单）
            (
                r#"db.table("a").where({field:"id",op:"in",subquery:{table:"b",columns:["nope"]}}).all()"#,
                "unknown column",
            ),
        ] {
            let cap = b
                .run(&format!(
                    r#"{js}.then(()=>json.ok({{}})).catch(e=>json.fail(400,String(e)));"#
                ))
                .await
                .unwrap();
            let v: Value = serde_json::from_slice(&cap.body).unwrap();
            assert!(v["msg"].as_str().unwrap().contains(want), "{want}: {v}");
        }
        // 深度超限：5 层 exists 嵌套（REQ_NEST_MAX=4）
        let mut js = String::from(r#"db.table("a").select(["id"])"#);
        let mut inner = String::from(r#"{table:"b",columns:["aid"]}"#);
        for _ in 0..5 {
            inner = format!(r#"{{table:"b",columns:["aid"],conditions:[{{exists:{inner}}}]}}"#);
        }
        js.push_str(&format!(r#".where({{exists:{inner}}}).all()"#));
        let cap = b
            .run(&format!(
                r#"{js}.then(()=>json.ok({{}})).catch(e=>json.fail(400,String(e)));"#
            ))
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(
            v["msg"]
                .as_str()
                .unwrap()
                .contains("nested select too deep"),
            "{v}"
        );
    }
}
