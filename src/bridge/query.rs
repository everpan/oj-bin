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
    Alias, Expr, ExprTrait, IntoColumnRef, LikeExpr, OnConflict, Order, OverStatement, Query,
    ReturningClause, SelectStatement, SimpleExpr, SqliteQueryBuilder, Value as Qv,
};
use serde::Deserialize;
use serde_json::Value;

use super::db::{Dialect, require_known_dialect};
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
    /// v0.1.81：语义化模糊 op —— pattern 由值派生，值里的 `%` / `_` 是**字面量**。
    Contains,
    StartsWith,
    EndsWith,
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
    /// LIKE 的 ESCAPE 转义符（单个字符，缺省 `\`）。只影响 `like` 与语义化模糊 op。
    escape: Option<String>,
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

/// 表引用：`name` 是 registry 真名（归属守卫 / 租户判定 / 列类型查询一律用它），
/// `alias` 是 SQL 侧别名（渲染 FROM / JOIN / 限定列前缀一律用 `sql_name()`）。
///
/// 二者**必须分开**：别名曾与真名混在同一个 `String` 里，导致
/// `guard::check_table("t as u")` 查不到归属 → `owner_of` 返回 None → 守卫静默放行，
/// 以及 `scoped("t as u")` → `reg.get` 为 None → **租户条件完全不注入**。
/// 类型化后入口只解析一次，此后两个用途在类型上就分开了（v0.1.81）。
#[derive(Debug, Clone)]
struct TableRef {
    name: String,
    alias: Option<String>,
}

impl TableRef {
    /// SQL 侧表标识：有别名则用别名（FROM 已渲染成 `t AS u`），否则用真名。
    fn sql_name(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.name)
    }
}

/// FROM：有别名渲染 `t AS u`，否则逐字节保持现状（v0.1.81）。
fn from_or_as(q: &mut SelectStatement, t: &TableRef) {
    match &t.alias {
        Some(a) => {
            q.from_as(Alias::new(&t.name), Alias::new(a));
        }
        None => {
            q.from(Alias::new(&t.name));
        }
    }
}

/// JOIN：与 `from_or_as` 同形（别名可缺席），ON 子句原样带入。
fn join_or_as(
    q: &mut SelectStatement,
    kind: sea_query::JoinType,
    t: &TableRef,
    on: sea_query::Condition,
) {
    match &t.alias {
        Some(a) => {
            q.join_as(kind, Alias::new(&t.name), Alias::new(a), on);
        }
        None => {
            q.join(kind, Alias::new(&t.name), on);
        }
    }
}

impl<'de> Deserialize<'de> for TableRef {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        match Value::deserialize(d)? {
            Value::String(s) => {
                let (name, alias) = split_table_alias(&s);
                if let Some(a) = &alias {
                    // 与 lit/agg/case/window/CTE 的别名同一道门（空串/怪形直达
                    // SQL 渲染只会换回一句看不懂的 DB 报错，v0.1.83）。
                    check_alias(a).map_err(|e| Error::custom(e.to_string()))?;
                }
                Ok(TableRef { name, alias })
            }
            Value::Object(mut m) => {
                let name = m
                    .remove("name")
                    .and_then(|v| v.as_str().map(str::to_string))
                    .ok_or_else(|| Error::custom("table object needs string 'name'"))?;
                let alias = m.remove("as").and_then(|v| v.as_str().map(str::to_string));
                if let Some(a) = &alias {
                    check_alias(a).map_err(|e| Error::custom(e.to_string()))?;
                }
                if !m.is_empty() {
                    let keys: Vec<&String> = m.keys().collect();
                    return Err(Error::custom(format!(
                        "table object takes only name/as, unknown keys {keys:?}"
                    )));
                }
                Ok(TableRef { name, alias })
            }
            _ => Err(Error::custom(
                "table must be a string or an object {name, as?}",
            )),
        }
    }
}

/// `"t as u"` → `("t", Some("u"))`。
/// **只认「空格 as 空格」单一形态**——表名里含 "as" 子串（如 `cast_as`）不会被误切，
/// 且形态唯一可避免因 `as`/`AS`/多空格产生的解析分歧。
fn split_table_alias(s: &str) -> (String, Option<String>) {
    match s.split_once(" as ") {
        Some((t, a)) if !t.is_empty() && !a.is_empty() => (t.to_string(), Some(a.to_string())),
        _ => (s.to_string(), None),
    }
}

/// join 种类（right 不做：sqlite 旧版本不支持且无用例）。
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum JoinKind {
    #[default]
    Inner,
    Left,
}

/// join 的 on 条件，两种形态互斥（v0.1.81 起支持「列 op 值」）：
/// - `Cols`：列对列等值 `{left, right}`，**不许带 op**。
/// - `Val`：列对值 `{left, op, value}`，op 只收比较类（见 `Op::class`）。
///
/// 手写 Deserialize 而非 untagged：untagged 下 `deny_unknown_fields` 不生效，
/// `{left, right, op}` 这种自以为能写 op 的形态会被**静默降级**成列对列（旧实现
/// 正是靠 derive + deny_unknown_fields 挡住的，改枚举不能把这个保障丢掉）。
#[derive(Debug, Clone)]
enum OnPair {
    Cols { left: String, right: String },
    Val { left: String, op: Op, value: Value },
}

impl<'de> Deserialize<'de> for OnPair {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        let m = serde_json::Map::<String, Value>::deserialize(d)?;
        let left = m
            .get("left")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::custom("join on needs string 'left'"))?
            .to_string();
        let extra = |keep: &[&str]| -> Vec<&String> {
            m.keys().filter(|k| !keep.contains(&k.as_str())).collect()
        };
        if let Some(right) = m.get("right") {
            // `{left, right, op}` 是自以为能写 op 的形态：两种 on 形态互斥，
            // 宁可报错也不猜（否则 op 被静默丢弃，`col = col` 多捞出数据）。
            if m.contains_key("op") {
                return Err(Error::custom(format!(
                    "join on '{left}' takes either right or op/value, not both"
                )));
            }
            let unknown = extra(&["left", "right"]);
            if !unknown.is_empty() {
                return Err(Error::custom(format!(
                    "join on '{left}' takes only right, unknown keys {unknown:?}"
                )));
            }
            let right = right
                .as_str()
                .ok_or_else(|| Error::custom("join on needs string 'right'"))?;
            return Ok(OnPair::Cols {
                left,
                right: right.to_string(),
            });
        }
        let op_v = m
            .get("op")
            .ok_or_else(|| Error::custom(format!("join on '{left}' needs op and value")))?;
        let op = Op::deserialize(op_v.clone()).map_err(Error::custom)?;
        // 只收比较类 op（`col <op> <value>`）：pattern 类（contains 等）在 ON 里
        // 无用例，nullary 类（isnull）无值、与「列 = 值」形态冲突。
        // 挡在**解析期**而不是渲染期——fromJSON 是权威入口，挡晚了就是两套判定。
        if op.class() != OpClass::Compare {
            return Err(Error::custom(format!(
                "join on '{left}' does not support op '{}'",
                op.name()
            )));
        }
        let value = m
            .get("value")
            .ok_or_else(|| Error::custom(format!("join on '{left}' needs value")))?
            .clone();
        let unknown = extra(&["left", "op", "value"]);
        if !unknown.is_empty() {
            return Err(Error::custom(format!(
                "join on '{left}' takes only op/value, unknown keys {unknown:?}"
            )));
        }
        Ok(OnPair::Val { left, op, value })
    }
}

#[derive(Debug, Clone, Deserialize)]
struct Join {
    table: TableRef,
    #[serde(default)]
    kind: JoinKind,
    on: Vec<OnPair>,
    /// 多租户防护注入（apply_tenant 无条件覆盖——fromJSON 喂入的 JS 预设值不可信；
    /// build_select_stmt 消费）。
    /// 进 ON 子句而非 WHERE——LEFT JOIN 注入 WHERE 会静默变 INNER JOIN（评审 P2-9）。
    /// 类型是 `Value`（v0.1.24）：列类型为数值时注入的就是数值/大整数标记。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tenant_id: Option<Value>,
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
    /// count(distinct col)（v0.1.82）：仅 count 消费；其余聚合带此键显式拒绝。
    #[serde(default)]
    distinct: bool,
}

/// 字面量列 `{lit: <value>, as: <alias>}`（v0.1.81）：给结果集补一列常量，
/// 例如 `select([{lit:"svc", as:"scope"}])` 给每行打上来源标记。
/// 值走绑定参数；三方言对裸参数的容忍度不同，故按方言决定是否套 CAST。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct LitSpec {
    lit: Value,
    #[serde(rename = "as")]
    r#as: String,
}

/// case 列 `{case:{when,else?}, as}`（searched case only；then/else 只允许 JSON 值，
/// 走绑定参数）。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct CaseSpec {
    case: CaseBody,
    #[serde(rename = "as")]
    r#as: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct CaseBody {
    when: Vec<CaseWhen>,
    #[serde(default, rename = "else")]
    r#else: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct CaseWhen {
    cond: CondTree,
    then: Value,
}

/// 窗口函数（frame 不做）。
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WinFn {
    RowNumber,
    Rank,
    DenseRank,
}

/// 窗口列 `{window:{fn, partition_by?, order_by?}, as}`。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct WindowSpec {
    window: WindowBody,
    #[serde(rename = "as")]
    r#as: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct WindowBody {
    #[serde(rename = "fn")]
    r#fn: WinFn,
    #[serde(default)]
    partition_by: Vec<String>,
    #[serde(default)]
    order_by: Vec<OrderBy>,
}

/// select 列：列名（可限定 "t.col"）、聚合 {fn, field?, as?}、case 列 {case, as}、
/// 窗口列 {window, as}。
/// 手写 Deserialize 按值类型/键分发——untagged 会吞内部错误（{fn:"median"} 只剩
/// "did not match any variant"，unknown variant 文案不可见）；对象键互不重叠
/// （fn / case / window），按键分发精确且保留各 spec 的原生报错。
#[derive(Debug, Clone)]
enum ColSpec {
    Name(String),
    /// `select(["*"])` 的全列形态（v0.1.81）：展开为 registry 全列，
    /// 与「不写 select」同义，只允许单独出现。
    Star,
    Lit(LitSpec),
    Agg(AggSpec),
    Case(CaseSpec),
    Window(WindowSpec),
}

impl<'de> Deserialize<'de> for ColSpec {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        match Value::deserialize(d)? {
            Value::String(s) if s == "*" => Ok(ColSpec::Star),
            Value::String(s) => Ok(ColSpec::Name(s)),
            v @ Value::Object(_) => {
                let m = v.as_object().expect("object");
                if m.contains_key("case") {
                    return serde_json::from_value::<CaseSpec>(v)
                        .map(ColSpec::Case)
                        .map_err(Error::custom);
                }
                if m.contains_key("window") {
                    return serde_json::from_value::<WindowSpec>(v)
                        .map(ColSpec::Window)
                        .map_err(Error::custom);
                }
                if m.contains_key("lit") {
                    return serde_json::from_value::<LitSpec>(v)
                        .map(ColSpec::Lit)
                        .map_err(Error::custom);
                }
                serde_json::from_value::<AggSpec>(v)
                    .map(ColSpec::Agg)
                    .map_err(Error::custom)
            }
            _ => Err(Error::custom(
                "column must be a string or an object {fn|case|window|lit, ...}",
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
    table: TableRef,
    #[serde(default)]
    verb: Verb,
    #[serde(default)]
    values: Vec<serde_json::Map<String, Value>>,
    #[serde(default)]
    sets: serde_json::Map<String, Value>,
    /// insert 返回列（白名单列；默认空 = 与既有行为一致，只回受影响行数）。
    /// pg/sqlite 渲染 sea-query `RETURNING` 单语句取回；mysql 无 RETURNING，
    /// 由 op 侧在同一目标（事务会话或池）上两步取 `LAST_INSERT_ID()`。
    #[serde(default)]
    returning: Vec<String>,
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
    #[serde(default)]
    unions: Vec<UnionArm>,
    #[serde(default)]
    with: Vec<CteReq>,
    /// 幂等插入（v0.1.81）：主键冲突时跳过而非报错。
    /// sqlite/pg 渲染 `ON CONFLICT DO NOTHING`；mysql 无原生 DO NOTHING，
    /// 退化成 `ON DUPLICATE KEY UPDATE <pk> = <pk>` 恒等式。
    #[serde(default)]
    or_ignore: bool,
    /// 显式整表清空（v0.1.82）：verb=delete + 无 where，渲染 `DELETE FROM t`（三方言通用；
    /// sqlite 无 TRUNCATE 语句，sea-query 的 TruncateStatement 反而不可移植）。仅 delete
    /// 承载（update 带 truncate 报错）；用户自带 where 在 op 入口 `validate_truncate` 拒绝
    /// （必须在租户注入前——注入的 `tenant_id = ?` 叶是引擎写的，不是用户的）。
    #[serde(default)]
    truncate: bool,
    /// insert 的数据源改为一条 select（v0.1.81）：与 `values` 互斥。
    #[serde(default)]
    from_select: Option<Box<QueryReq>>,
    /// `from_select` 路径下的目标列清单（必须与源 select 输出列数一致）。
    #[serde(default)]
    insert_cols: Vec<String>,
}

/// union 种类（Intersect/Except 不做：mysql 旧版本不支持且无用例）。
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum UnionKind {
    #[default]
    Distinct,
    All,
}

/// union 成员臂：kind + 嵌套 select req（显式列 + 禁排序分页，构造期校验）。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct UnionArm {
    #[serde(default)]
    kind: UnionKind,
    query: Box<QueryReq>,
}

/// CTE（非递归；columns 必填——CTE 输出列即后续解析的白名单）。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct CteReq {
    name: String,
    columns: Vec<String>,
    query: Box<QueryReq>,
}

fn default_db() -> String {
    "default".into()
}

/// 隐式 LIMIT（顶层 select 未给 limit 时补上）与显式 limit 的 clamp 硬顶（防 DoS）。
pub const LIMIT_DEFAULT: u32 = 100;
pub const LIMIT_MAX: u32 = 1000;
/// `max_limit` 的绝对上界（配置可上调但不能无界；装配期校验）。
pub const LIMIT_HARD_CAP: u32 = 100_000;

/// 构造器 LIMIT 配置（`db_query:` 段；v0.1.20）。装配期校验
/// `1 ≤ default_limit ≤ max_limit ≤ HARD_CAP`。字段名 = YAML 键名（无 rename/alias）；
/// `deny_unknown_fields` 让拼错的键（如 `default`）在装配期报错而非静默取默认值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QueryLimits {
    pub default_limit: u32,
    pub max_limit: u32,
}

impl Default for QueryLimits {
    fn default() -> Self {
        Self {
            default_limit: LIMIT_DEFAULT,
            max_limit: LIMIT_MAX,
        }
    }
}

impl QueryLimits {
    /// 装配期校验（fail-fast）：0 与倒置区间都非法——`0` 意味着不设限（DoS 面），
    /// `default_limit > max_limit` 会让「隐式比显式还大」，两者都不给静默回落。
    pub fn validate(&self) -> Result<(), String> {
        if self.default_limit == 0 {
            return Err("db_query.default_limit must be >= 1 (0 = unlimited)".into());
        }
        if self.max_limit == 0 {
            return Err("db_query.max_limit must be >= 1 (0 = unlimited)".into());
        }
        if self.max_limit > LIMIT_HARD_CAP {
            return Err(format!(
                "db_query.max_limit {} exceeds hard cap {LIMIT_HARD_CAP}",
                self.max_limit
            ));
        }
        if self.default_limit > self.max_limit {
            return Err(format!(
                "db_query.default_limit {} > max_limit {}",
                self.default_limit, self.max_limit
            ));
        }
        Ok(())
    }

    /// 归一化：显式 limit → clamp 到 max_limit；未给 → default_limit。返回实际生效值。
    fn applied(&self, explicit: Option<u32>) -> u32 {
        match explicit {
            // Ord::min 全路径限定：sea-query 的 ExprTrait 也为 u32 提供了 min。
            Some(l) => Ord::min(l, self.max_limit),
            None => self.default_limit,
        }
    }
}

fn registry(state: &Rc<RefCell<OpState>>) -> Result<Arc<SchemaRegistry>, JsErrorBox> {
    Ok(state.borrow().borrow::<Arc<StableState>>().registry.clone())
}

// REQ-12：导入批渲染复用（serde → sea_query 值）。
pub(crate) fn to_qv(v: &Value) -> Qv {
    // 大整数标记（v0.1.22 `$oj$i64` / v0.1.24 `$oj$u64`，`toBigInt()` / `toUBigInt()` 的返回值）：
    // 必须在 `other => to_string()` 之前——否则标记对象会被串化成文本（PG 拒绝 text → bigint）。
    if let Some(i) = oj_plugin_ffi::jsint::marker_i64(v) {
        return Qv::BigInt(Some(i));
    }
    if let Some(u) = oj_plugin_ffi::jsint::marker_u64(v) {
        return Qv::BigUnsigned(Some(u));
    }
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

/// op 的能力分类（v0.1.81）：决定一个 op 能出现在哪些语境。
/// - `Compare`：`col <op> <value>`，右操作数是调用方给的字面值。
/// - `Pattern`：`col LIKE <pattern>`，pattern 由值派生（`contains` 等），
///   值的 `%` / `_` 已被转义成字面量。
/// - `Nullary`：无右操作数（`isnull`）。
///
/// 单一职责：分类只回答「这个 op 是什么形态」，编译谓词由 `Op::apply` 负责。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpClass {
    Compare,
    Pattern,
    Nullary,
}

/// LIKE 默认转义符。
const LIKE_ESCAPE: char = '\\';

/// 把值里的 LIKE 元字符（`%` / `_` / 转义符自身）转成字面量。
/// 顺序要紧：先处理转义符本身，否则 `a\b` 会漏掉反斜杠的元语义。
pub(crate) fn escape_like(s: &str, esc: char) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if c == esc || c == '%' || c == '_' {
            out.push(esc);
        }
        out.push(c);
    }
    out
}

/// 字面量列的三方言形态（v0.1.81）：值一律绑定参数，差异只在要不要套 CAST。
/// - sqlite：裸 `?` 可用（`SELECT ? AS x` 合法），不套。
/// - mysql：`CAST(? AS CHAR)`——裸参数的列类型由驱动推断，与 UNION / 字符串比较
///   混用时类型不可靠，套一层统一成 CHAR。
/// - postgres：**必须** `CAST($1 AS TEXT)`——裸参数会报
///   `could not determine data type of parameter $1`。
///
/// 字面列能承载的值形态（v0.1.81）：标量，或大整数标记对象（`toBigInt()` 的产物，
/// 它本身就是标量的载体）。对象/数组一律不算 —— 它们会被串成 `{"a":1}` 这种文本。
fn lit_is_scalar(v: &Value) -> bool {
    v.is_null()
        || matches!(v, Value::String(_) | Value::Bool(_) | Value::Number(_))
        || oj_plugin_ffi::jsint::marker_i64(v).is_some()
        || oj_plugin_ffi::jsint::marker_u64(v).is_some()
}

/// 类型名按裸标识符传入（不是 `Alias`）：`Alias` 会被渲染成 `"CHAR"` 而变成列名。
fn lit_expr(v: &Value, dialect: Dialect) -> SimpleExpr {
    let e = Expr::val(to_qv(v));
    match dialect {
        Dialect::Sqlite => e,
        Dialect::MySql => e.cast_as("CHAR"),
        Dialect::Postgres => e.cast_as("TEXT"),
    }
}

/// `col LIKE <pattern> ESCAPE <esc>`：pattern 与转义符都是**绑定参数**，
/// 不落进 SQL 文本（否则 `%` / `'` 就是拼接注入面）。
///
/// 嵌套方向要紧：ESCAPE 必须是 LIKE 的**右**操作数
/// （`Binary(col, Like, Binary(pattern, Escape, esc))`）。反过来的
/// `Binary(Binary(col, Like, pattern), Escape, esc)` 会被 sea-query 的括号化渲染成
/// `(col LIKE ?) ESCAPE ?` —— 语法错误。
fn like_pred<T: ExprTrait>(t: T, pattern: &str, esc: char) -> SimpleExpr {
    let rhs = Expr::val(pattern.to_string())
        .binary(sea_query::BinOper::Escape, Expr::val(esc.to_string()));
    t.binary(sea_query::BinOper::Like, rhs)
}

impl Op {
    /// 该 op 的线上名字（`{op: "<name>"}` 里写的那个），报错文案用它。
    fn name(self) -> &'static str {
        match self {
            Op::Eq => "eq",
            Op::Ne => "ne",
            Op::Gt => "gt",
            Op::Gte => "gte",
            Op::Lt => "lt",
            Op::Lte => "lte",
            Op::In => "in",
            Op::Like => "like",
            Op::IsNull => "isnull",
            Op::Contains => "contains",
            Op::StartsWith => "startswith",
            Op::EndsWith => "endswith",
        }
    }

    fn class(self) -> OpClass {
        match self {
            Op::Eq | Op::Ne | Op::Gt | Op::Gte | Op::Lt | Op::Lte | Op::In | Op::Like => {
                OpClass::Compare
            }
            Op::Contains | Op::StartsWith | Op::EndsWith => OpClass::Pattern,
            Op::IsNull => OpClass::Nullary,
        }
    }

    /// 比较类 op 编译为谓词（`col <op> <value>`），值必填。
    /// 只服务 join ON 语境（那里没有 `escape` 字段，一律用默认转义符）。
    fn apply<T: ExprTrait>(self, t: T, val: &Value) -> Result<SimpleExpr, JsErrorBox> {
        let esc = LIKE_ESCAPE;
        Ok(match (self, val) {
            (Op::Eq, v) => t.eq(Expr::val(to_qv(v))),
            (Op::Ne, v) => t.ne(Expr::val(to_qv(v))),
            (Op::Gt, v) => t.gt(Expr::val(to_qv(v))),
            (Op::Gte, v) => t.gte(Expr::val(to_qv(v))),
            (Op::Lt, v) => t.lt(Expr::val(to_qv(v))),
            (Op::Lte, v) => t.lte(Expr::val(to_qv(v))),
            (Op::In, Value::Array(a)) => {
                if a.is_empty() {
                    return Err(JsErrorBox::generic("in needs non-empty array"));
                }
                t.is_in(a.iter().map(|v| Expr::val(to_qv(v))))
            }
            (Op::In, _) => {
                return Err(JsErrorBox::generic("in needs array value"));
            }
            (Op::Like, v) => like_pred(t, &text_of(v), esc),
            // 其余（pattern / nullary 类）由调用方按 `class()` 提前挡掉。
            other => {
                return Err(JsErrorBox::generic(format!(
                    "op '{}' is not supported here",
                    other.0.name()
                )));
            }
        })
    }
}

/// 取 LIKE 转义符：缺省 `\`；给了就必须是**单个**字符（多字符会被静默截断成第一个，
/// 宁可报错）。
fn escape_char_of(c: &Cond) -> Result<char, JsErrorBox> {
    match &c.escape {
        None => Ok(LIKE_ESCAPE),
        Some(s) => {
            let mut it = s.chars();
            match (it.next(), it.next()) {
                (Some(ch), None) => Ok(ch),
                _ => Err(JsErrorBox::generic(format!(
                    "escape must be a single char, got '{}'",
                    s
                ))),
            }
        }
    }
}

/// op 编译为谓词表达式（泛化：左操作数可为任意 ExprTrait——having 别名展开复用）。
/// `value` 为 None 时按 op 分派：eq/ne 取 NULL 右值、isnull 无值，其余报「needs value」。
fn apply_op<T: ExprTrait>(t: T, c: &Cond) -> Result<SimpleExpr, JsErrorBox> {
    let val = &c.value;
    let rhs = |v: &Value| Expr::val(to_qv(v));
    let esc = escape_char_of(c)?;
    Ok(match c.op {
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
            // 空集会被 sea-query 渲染成恒假（`IN ()` / `IN (NULL)`），
            // 结果是**静默 0 行**——与「查到 0 条」无法区分，故显式拒绝（v0.1.81）。
            if arr.is_empty() {
                return Err(JsErrorBox::generic("in needs non-empty array"));
            }
            let vals: Vec<Expr> = arr.iter().map(rhs).collect();
            t.is_in(vals)
        }
        Op::Like => {
            let v = val
                .as_ref()
                .ok_or_else(|| JsErrorBox::generic("like needs value"))?;
            // 裸 like：pattern 由用户给，`escape` 给了才挂 ESCAPE 子句。
            let pat = text_of(v);
            match &c.escape {
                Some(_) => like_pred(t, &pat, esc),
                None => t.like(LikeExpr::new(pat)),
            }
        }
        // 语义化模糊 op：pattern 由值派生，值里的 % / _ 一律当字面量。
        Op::Contains | Op::StartsWith | Op::EndsWith => {
            let v = val
                .as_ref()
                .ok_or_else(|| JsErrorBox::generic(format!("{} needs value", c.op.name())))?;
            let core = escape_like(&text_of(v), esc);
            let pat = match c.op {
                Op::Contains => format!("%{core}%"),
                Op::StartsWith => format!("{core}%"),
                _ => format!("%{core}"),
            };
            like_pred(t, &pat, esc)
        }
        Op::IsNull => t.is_null(),
    })
}

/// Cond 值的文本形态（数值/布尔等非字符串按 JSON 文本，与既有行为一致）。
fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// 表来源：真实表（registry）或 CTE 虚拟表（声明列）。
enum TableSrc<'a> {
    Real(&'a TableDef),
    Cte(&'a [String]),
}

impl TableSrc<'_> {
    fn has_column(&self, col: &str) -> bool {
        match self {
            TableSrc::Real(t) => t.has_column(col),
            TableSrc::Cte(cols) => cols.iter().any(|c| c == col),
        }
    }

    /// 真实表按注册表类型判定；CTE 列无类型信息，声明即可排序。
    fn is_sortable(&self, col: &str) -> bool {
        match self {
            TableSrc::Real(t) => t.is_sortable(col),
            TableSrc::Cte(cols) => cols.iter().any(|c| c == col),
        }
    }

    /// 全列名（select 省略 columns 时的展开清单）。
    fn col_names(&self) -> Vec<&str> {
        match self {
            TableSrc::Real(t) => t.columns.keys().map(|s| s.as_str()).collect(),
            TableSrc::Cte(cols) => cols.iter().map(|s| s.as_str()).collect(),
        }
    }
}

/// 限定列解析上下文（select/where/orderBy/groupBy/having/join on 六处共用）。
struct ColCtx<'a> {
    base_name: &'a str,
    base: TableSrc<'a>,
    joins: Vec<(&'a str, TableSrc<'a>)>,
    /// 顶层 with 声明（未参与 FROM 的直接引用也可解析列——由 SQL 侧报错兜底）。
    ctes: Vec<(&'a str, TableSrc<'a>)>,
}

/// 构造期共享上下文（registry + 方言）：替代把两个值逐层透传的长参数表。
/// 方言是必需的——`lit` 的 CAST 形态与 `orIgnore` 的方言分派都要它，
/// 而它们可能出现在任意嵌套层（子查询 / CTE / union 成员），不能只在顶层可见。
#[derive(Clone, Copy)]
struct Build<'a> {
    reg: &'a SchemaRegistry,
    dialect: Dialect,
}

impl ColCtx<'_> {
    /// 列引用 → 所属表来源：`"t.col"` 表段 ∈ {基表} ∪ {join 表} ∪ {CTE 名}，
    /// 列段对该来源校验；非限定仅解析基表（不查 join 表——天然拒绝歧义，强制全限定名）。
    fn table_of(&self, col: &str, site: &str) -> Result<&TableSrc<'_>, JsErrorBox> {
        match col.split_once('.') {
            Some((t, c)) => {
                let ts = if t == self.base_name {
                    &self.base
                } else if let Some((_, ts)) = self.joins.iter().find(|(n, _)| *n == t) {
                    ts
                } else if let Some((_, ts)) = self.ctes.iter().find(|(n, _)| *n == t) {
                    ts
                } else {
                    return Err(JsErrorBox::generic(format!(
                        "unknown table '{t}' in {site}"
                    )));
                };
                if !ts.has_column(c) {
                    return Err(JsErrorBox::generic(format!(
                        "unknown column '{col}' in {site}"
                    )));
                }
                Ok(ts)
            }
            None if self.base.has_column(col) => Ok(&self.base),
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

/// 单个 CASE 列的 when 子句上限。
const CASE_WHEN_MAX: usize = 16;

/// insert-from-select 的租户常量列别名（守卫内部注入，用户不可指定同名列）。
const TENANT_LIT_ALIAS: &str = "__oj_tenant";

/// 嵌套 select 通用约束（site 用于报错定位：subquery/exists/union/cte）。
/// with 的禁带判断由 Task 18 落地字段后补上（unions 已落地）。
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
    if !req.unions.is_empty() || !req.with.is_empty() {
        return Err(JsErrorBox::generic(format!(
            "{site}: nested select does not accept with/unions (v1)"
        )));
    }
    // 嵌套无隐式 LIMIT 兜底（Task 16 起），offset 无 limit 会渲染裸 OFFSET——
    // 嵌套分页本无意义，要求成对出现，把 DB 语法错提前为校验错（F-2）。
    if req.limit.is_none() && req.offset.is_some() {
        return Err(JsErrorBox::generic(format!(
            "{site}: nested select offset requires limit"
        )));
    }
    Ok(())
}

/// 叶子内的子查询编译（where/having 叶子共用）：无 subquery → None。
/// op ∈ in/eq/ne/gt/gte/lt/lte；value 与 subquery 互斥；isnull/like 不接受。
fn leaf_subquery(
    c: &Cond,
    ctx: &ColCtx<'_>,
    bx: Build<'_>,
    sel_depth: u8,
    site: &str,
) -> Result<Option<SimpleExpr>, JsErrorBox> {
    let Some(sub) = c.subquery.as_deref() else {
        return Ok(None);
    };
    // 按 class 判定而不是逐个点名（v0.1.81）：pattern 类（contains 等）与
    // nullary 类（isnull）形态上就构不成 `col <op> (SELECT ...)`，
    // 新增 op 只要分好类就自动落进这条守卫，不必再改这里。
    match c.op.class() {
        OpClass::Pattern | OpClass::Nullary => {
            return Err(JsErrorBox::generic(format!(
                "{} does not accept subquery",
                c.op.name()
            )));
        }
        OpClass::Compare => {}
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
    let sel = build_select_stmt(sub, bx, sel_depth + 1)?;
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
        Op::Like | Op::IsNull | Op::Contains | Op::StartsWith | Op::EndsWith => {
            unreachable!("rejected above")
        }
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
    bx: Build<'_>,
    sel_depth: u8,
    depth: usize,
    leaves: &mut usize,
) -> Result<SimpleExpr, JsErrorBox> {
    cond_expr_generic(
        t,
        &mut |c| {
            if let Some(e) = leaf_subquery(c, ctx, bx, sel_depth, "where")? {
                return Ok(e);
            }
            ctx.check_col(&c.field, "where")?;
            apply_op(col_simple_expr(&c.field), c)
        },
        &mut |sub| {
            validate_nested(sub, sel_depth + 1, "exists")?;
            Ok(Expr::exists(build_select_stmt(sub, bx, sel_depth + 1)?))
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
    bx: Build<'_>,
    sel_depth: u8,
    depth: usize,
    leaves: &mut usize,
) -> Result<SimpleExpr, JsErrorBox> {
    cond_expr_generic(
        t,
        &mut |c| {
            if let Some(e) = leaf_subquery(c, ctx, bx, sel_depth, "having")? {
                return Ok(e);
            }
            if ctx.check_col(&c.field, "having").is_ok() {
                apply_op(col_simple_expr(&c.field), c)
            } else {
                let e = aliases.get(&c.field).ok_or_else(|| {
                    JsErrorBox::generic(format!("unknown column '{}' in having", c.field))
                })?;
                apply_op(Expr::expr(e.clone()), c)
            }
        },
        &mut |sub| {
            validate_nested(sub, sel_depth + 1, "exists")?;
            Ok(Expr::exists(build_select_stmt(sub, bx, sel_depth + 1)?))
        },
        depth,
        leaves,
    )
}

/// op 前置守卫（两个 op 共用）：主表与 join 表 check_table（命中本层 CTE 名则跳过——
/// 虚拟表无归属）；条件树（含 having）内所有子查询/exists 的嵌套 req 递归过守卫；
/// 每个 cte.query 递归过守卫。
fn guard_req(state: &Rc<RefCell<OpState>>, req: &QueryReq) -> Result<(), JsErrorBox> {
    // 归属守卫只认**真名**：别名一旦混进来，`owner_of("t as u")` 查不到条目 →
    // 返回 None → judge 直接放行，守卫在 deny 模式下也会失效（v0.1.81 修）。
    let cte_name = |n: &str| req.with.iter().any(|c| c.name == n);
    if !cte_name(&req.table.name) {
        super::guard::check_table(state, &req.table.name)?;
    }
    for j in &req.joins {
        if !cte_name(&j.table.name) {
            super::guard::check_table(state, &j.table.name)?;
        }
    }
    for c in &req.conditions {
        guard_nested(state, c)?;
    }
    if let Some(h) = &req.having {
        guard_nested(state, h)?;
    }
    // CASE 列的 when 条件可携带 subquery/exists 嵌套 req——同样递归过守卫（F-1）。
    for col in &req.columns {
        if let ColSpec::Case(c) = col {
            for w in &c.case.when {
                guard_nested(state, &w.cond)?;
            }
        }
    }
    for u in &req.unions {
        guard_req(state, &u.query)?;
    }
    for c in &req.with {
        guard_req(state, &c.query)?;
    }
    // fromSelect 的源 select 同样是一次跨表读取：不递归等于允许
    // `insert into 我的表 select * from 别人的表`（apply_tenant 早已递归，
    // 此处是同一漏洞的归属守卫面，v0.1.83）。
    if let Some(s) = &req.from_select {
        guard_req(state, s)?;
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

/// 多租户防护预变换（guard_req 之后、build_statement 之前；op_db_query_build 与
/// op_db_query_sql 两 op 同步注入——toSQL 产物离开 AST 后经 db.query 直跑时防护才成立）。
/// 形状递归照抄 guard_req：joins/条件树子查询/exists/union 臂/cte/case-when 全覆盖。
/// tid=None：Deny 对受约束表 Err；Warn 告警放行（软过渡，不注入）。system=请求级逃生口。
fn apply_tenant(
    req: &mut QueryReq,
    reg: &SchemaRegistry,
    tid: Option<&str>,
    guard: super::SqlGuard,
    system: bool,
) -> Result<(), JsErrorBox> {
    if system {
        return Ok(());
    }
    let cte_name = |n: &str| req.with.iter().any(|c| c.name == n);
    // `scoped` 只认真名：别名混进来时 `reg.get("t as u")` 为 None → 恒判为「非租户表」
    // → 租户条件被静默跳过（v0.1.81 修）。
    let scoped =
        |name: &str| !cte_name(name) && reg.get(name).is_some_and(|t| t.is_tenant_scoped());
    // 本层受约束表（基表 + join 表；用于 tid=None 判定与写侧校验）。
    // 存真名：报错文案 `table(s) {touched:?}` 形态由既有测试锁定，不能带别名信息。
    let mut touched = vec![];
    if scoped(&req.table.name) {
        touched.push(req.table.name.clone());
    }
    for j in &req.joins {
        if scoped(&j.table.name) {
            touched.push(j.table.name.clone());
        }
    }
    if !touched.is_empty() {
        match tid {
            Some(tid) => match req.verb {
                Verb::Select | Verb::Update | Verb::Delete => {
                    // 限定列注入：join 场景下裸 tenant_id 会撞歧义（两表同名列）。
                    // 前缀用 **sql_name()**（别名）——FROM 已渲染成 `t AS u`，
                    // 用真名会得到不存在的限定表 → SQL 报错（v0.1.81）。
                    req.conditions.push(CondTree::Leaf(Cond {
                        field: format!("{}.tenant_id", req.table.sql_name()),
                        op: Op::Eq,
                        value: Some(tenant_value(reg, &req.table.name, tid)?),
                        escape: None,
                        subquery: None,
                    }));
                }
                Verb::Insert => {
                    // v0.1.81：fromSelect 路径没有 `values` 可注入 → 租户列必须由
                    // **常量列**补上（`INSERT ... SELECT c1, c2, ? AS __oj_tenant`）。
                    // 少了这一步，租户表上的 insert-from-select 会写出 tenant_id 为空的
                    // 行——守卫不是「拦错租户」，而是「根本没写租户」。
                    if let Some(src) = &mut req.from_select {
                        if req.insert_cols.iter().any(|c| c == "tenant_id") {
                            return Err(JsErrorBox::generic(
                                "tenant guard: insert fromSelect must not supply tenant_id",
                            ));
                        }
                        src.columns.push(ColSpec::Lit(LitSpec {
                            lit: tenant_value(reg, &req.table.name, tid)?,
                            r#as: TENANT_LIT_ALIAS.into(),
                        }));
                        req.insert_cols.push("tenant_id".into());
                    }
                    // v0.1.24：等值判定复用 `param_is_tenant`（字符串/数字/i64 标记/u64 标记
                    // 四形态等价）——此前只认字符串，数值型 tenant_id 列因此无法插入。
                    for row in &mut req.values {
                        match row.get("tenant_id") {
                            Some(v) if !super::guard::param_is_tenant(v, tid) => {
                                return Err(JsErrorBox::generic(format!(
                                    "tenant guard: insert tenant_id mismatch (got {v}, want {tid:?})"
                                )));
                            }
                            _ => {
                                row.insert(
                                    "tenant_id".into(),
                                    tenant_value(reg, &req.table.name, tid)?,
                                );
                            }
                        }
                    }
                }
            },
            None => {
                let msg = format!(
                    "tenant guard: table(s) {touched:?} require tenant context \
                     (missing tenant header; use db.asSystem() for system tasks)"
                );
                if guard == super::SqlGuard::Deny {
                    return Err(JsErrorBox::generic(msg));
                }
                // Warn：整层不注入、告警放行（软过渡），继续递归让嵌套层各自告警。
                eprintln!("warn: {msg}");
            }
        }
        // update 写侧逃逸：sets 显式 tenant_id 必须等于当前租户（比 insert 更隐蔽，
        // update({tenant_id:"victim"}) 会把本租户行迁移到他租户）。
        if req.verb == Verb::Update
            && scoped(&req.table.name)
            && let Some(v) = req.sets.get("tenant_id")
            && (tid.is_none() || !tid.is_some_and(|t| super::guard::param_is_tenant(v, t)))
        {
            return Err(JsErrorBox::generic(format!(
                "tenant guard: update sets.tenant_id not allowed (got {v})"
            )));
        }
        // join 表：tenant 条件进 ON 子句（Join.tenant_id，build_select_stmt 消费）。
        // 无条件覆盖：fromJSON 可绕过 JS 链层直接喂 serde，JS 预设值绝不可信
        // （评审 P0——is_none() 放行会让攻击者选定他租户 ON 条件）。
        if let Some(tid) = tid {
            for j in &mut req.joins {
                if scoped(&j.table.name) {
                    j.tenant_id = Some(tenant_value(reg, &j.table.name, tid)?);
                }
            }
        }
    }
    // 递归：条件树（含 having/case-when）内子查询 + union 臂 + cte。
    for c in &mut req.conditions {
        apply_tenant_tree(c, reg, tid, guard)?;
    }
    if let Some(h) = &mut req.having {
        apply_tenant_tree(h, reg, tid, guard)?;
    }
    for col in &mut req.columns {
        if let ColSpec::Case(c) = col {
            for w in &mut c.case.when {
                apply_tenant_tree(&mut w.cond, reg, tid, guard)?;
            }
        }
    }
    for u in &mut req.unions {
        apply_tenant(&mut u.query, reg, tid, guard, false)?;
    }
    for c in &mut req.with {
        apply_tenant(&mut c.query, reg, tid, guard, false)?;
    }
    // fromSelect 的源 select 也是一次完整读取：不递归就等于允许
    // `insert into 我的表 select * from 租户表`（跨租户搬运，v0.1.81）。
    if let Some(s) = &mut req.from_select {
        apply_tenant(s, reg, tid, guard, false)?;
    }
    Ok(())
}

/// 租户 id 按**列类型**生成绑定值（v0.1.24）。
///
/// - `text` / `Unknown`（旧装配路径）→ 字符串，即 v0.1.23 及以前的行为；
/// - `integer` / `bigint` → 十进制字面量转数值：≤2^53-1 用 `Number`，更宽用 `$oj$i64`；
///   超出 i64 的（雪花量级无符号）用 `$oj$u64`——只有 MySQL `BIGINT UNSIGNED` 能承载，
///   PG/SQLite 会在绑定前**明确报错**（`.oj-plugin-ffi::jsint::reject_u64_markers`）；
/// - 数值列上给了非十进制租户头 → 直接报错，不要让 PG 回一句
///   `invalid input syntax for type bigint` 那种看不出「是租户头写错了」的消息。
///
/// 该值同时用于：select/update/delete 的注入条件、insert 的强制写、join 的 ON 条件
/// ——三处必须同型，否则 PG 会在数值列上撞 `bigint = text`。
fn tenant_value(reg: &SchemaRegistry, table: &str, tid: &str) -> Result<Value, JsErrorBox> {
    let ty = reg
        .get(table)
        .map(|t| t.column_type("tenant_id"))
        .unwrap_or_default();
    if !ty.is_numeric() {
        return Ok(Value::String(tid.into()));
    }
    if let Ok(i) = tid.parse::<i64>() {
        return Ok(int_param(i));
    }
    if ty == super::registry::ColumnType::BigInt
        && let Ok(u) = tid.parse::<u64>()
    {
        return Ok(uint_param(u));
    }
    Err(JsErrorBox::generic(format!(
        "tenant guard: tenant id {tid:?} is not a valid integer for numeric column {table}.tenant_id"
    )))
}

/// 条件树遍历，嵌套 req 递归 apply_tenant（与 guard_nested 同构镜像演进）。
fn apply_tenant_tree(
    t: &mut CondTree,
    reg: &SchemaRegistry,
    tid: Option<&str>,
    guard: super::SqlGuard,
) -> Result<(), JsErrorBox> {
    match t {
        CondTree::Leaf(c) => {
            if let Some(sub) = &mut c.subquery {
                apply_tenant(sub, reg, tid, guard, false)?;
            }
            Ok(())
        }
        CondTree::And(xs) | CondTree::Or(xs) => {
            for x in xs {
                apply_tenant_tree(x, reg, tid, guard)?;
            }
            Ok(())
        }
        CondTree::Not(x) => apply_tenant_tree(x, reg, tid, guard),
        CondTree::Exists(sub) => apply_tenant(sub, reg, tid, guard, false),
    }
}

/// 动词×字段兼容矩阵（op 侧权威——fromJSON 可完全绕过 JS 链层）。
fn validate_verb(req: &QueryReq) -> Result<(), JsErrorBox> {
    let reject =
        |verb: &str, f: &str| Err(JsErrorBox::generic(format!("{verb} does not accept {f}")));
    // 别名只在 select/join 成立（方言的 INSERT/UPDATE/DELETE 不支持 `AS` 别名），
    // 带别名的 DML 会让列限定指向一张 SQL 里不存在的表 → 显式拒绝（v0.1.81）。
    if req.table.alias.is_some() && !matches!(req.verb, Verb::Select) {
        return Err(JsErrorBox::generic(
            "table alias is not supported on insert/update/delete",
        ));
    }
    match req.verb {
        Verb::Select => {
            if !req.values.is_empty() {
                return reject("select", "values");
            }
            if !req.sets.is_empty() {
                return reject("select", "sets");
            }
            if !req.returning.is_empty() {
                return reject("select", "returning");
            }
            // orIgnore / fromSelect 是 INSERT 的尾部子句，挂在其他动词上无处可渲染
            // → 显式拒绝，而不是静默丢弃（v0.1.81）。
            if req.or_ignore {
                return reject("select", "orIgnore");
            }
            if req.from_select.is_some() || !req.insert_cols.is_empty() {
                return reject("select", "fromSelect");
            }
        }
        Verb::Insert => {
            // values 与 fromSelect 互斥且必居其一（v0.1.81）：两条都走的语义无法定义
            // （是插入字面量行还是插入查询结果？），宁可报错也不猜。
            if !req.values.is_empty() && req.from_select.is_some() {
                return Err(JsErrorBox::generic(
                    "insert takes either values or fromSelect, not both",
                ));
            }
            if req.values.is_empty() && req.from_select.is_none() {
                return Err(JsErrorBox::generic(
                    "insert needs either values or fromSelect",
                ));
            }
            if req.from_select.is_some() && req.insert_cols.is_empty() {
                return Err(JsErrorBox::generic(
                    "insert fromSelect needs insert columns",
                ));
            }
            // 列清单只在 fromSelect 路径上有意义（values 路径的列名取自行对象）——
            // 给了却不用 = 用户以为生效了，必须报错。
            if req.from_select.is_none() && !req.insert_cols.is_empty() {
                return Err(JsErrorBox::generic(
                    "insert columns are only allowed with fromSelect",
                ));
            }
            // 源 select 与 union 臂同一把尺子（动词/嵌套深度/with-unions/裸 offset）。
            if let Some(src) = req.from_select.as_deref() {
                validate_nested(src, 0, "fromSelect")?;
                // 源列必须逐个点名：`select(["*"])` 与「不写 select」都按 registry 的
                // HashMap 顺序摊成全列，而 INSERT ... SELECT 是按**位置**对齐的——
                // 顺序不定等于列名串位（name 写进 age）。宁可要用户多敲几个字。
                if src.columns.is_empty() || src.columns.iter().any(|c| matches!(c, ColSpec::Star))
                {
                    return Err(JsErrorBox::generic(
                        "fromSelect source needs explicit columns (no '*' / omission)",
                    ));
                }
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
            if !req.unions.is_empty() {
                return reject("insert", "unions");
            }
            if !req.with.is_empty() {
                return reject("insert", "with");
            }
            if !req.columns.is_empty() {
                return reject("insert", "columns");
            }
        }
        Verb::Update | Verb::Delete => {
            // truncate 只在 delete 成立（v0.1.82）：update().truncate() 是矛盾语义，显式拒绝。
            if req.truncate && req.verb == Verb::Update {
                return reject("update", "truncate");
            }
            if !req.joins.is_empty() {
                return reject("update/delete", "joins");
            }
            if !req.columns.is_empty() {
                return reject("update/delete", "columns");
            }
            if !req.returning.is_empty() {
                return reject("update/delete", "returning");
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
            if !req.unions.is_empty() {
                return reject("update/delete", "unions");
            }
            if !req.with.is_empty() {
                return reject("update/delete", "with");
            }
            if req.verb == Verb::Update && req.sets.is_empty() {
                return Err(JsErrorBox::generic("update needs non-empty sets"));
            }
            if req.or_ignore {
                return reject("update/delete", "orIgnore");
            }
            if req.from_select.is_some() || !req.insert_cols.is_empty() {
                return reject("update/delete", "fromSelect");
            }
            // 空 and/or 组在 CondTree Deserialize 已拒 → 非空即叶子 ≥ 1。
            // truncate（v0.1.82）就是要无 where，跳过该门禁；用户自带 where 已在
            // op 入口的 validate_truncate 拒掉（租户注入叶在此之前发生，照常渲染）。
            if req.conditions.is_empty() && !req.truncate {
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
    let bx = Build { reg, dialect };
    let params_of =
        |(sql, values): (String, sea_query::Values)| -> Result<(String, Vec<Value>), JsErrorBox> {
            let params = values.iter().map(value_to_json).collect::<Result<_, _>>()?;
            Ok((sql, params))
        };
    match req.verb {
        Verb::Select => {
            // WITH 先校验装配（名称/列形状/嵌套约束须先于主语句解析——CTE 基表
            // 依赖声明合法），再方言 build；参数由 sea-query 跨整棵语句树统一收集。
            let clause = if req.with.is_empty() {
                None
            } else {
                Some(build_with_clause(req, bx, 0)?)
            };
            let sel = build_select_stmt(req, bx, 0)?;
            match clause {
                None => params_of(build_sql(dialect, &sel)),
                Some(c) => params_of(build_sql(dialect, &sel.with(c))),
            }
        }
        Verb::Insert => {
            let table = reg.get(&req.table.name).ok_or_else(|| {
                JsErrorBox::generic(format!("unknown table '{}'", req.table.name))
            })?;
            let mut ins = Query::insert();
            // INSERT 无 FROM 语义需求 → 恒用真名（别名在动词矩阵里已被拒）。
            ins.into_table(Alias::new(&req.table.name));
            if let Some(src) = req.from_select.as_deref() {
                // 目标列同样走白名单：写侧列名一律来自 registry，拼接面只在
                // `Alias::new`（sea-query 侧）和这里（注入面）。
                for c in &req.insert_cols {
                    if !table.has_column(c) {
                        return Err(JsErrorBox::generic(format!(
                            "unknown column '{c}' in insert columns"
                        )));
                    }
                }
                ins.columns(req.insert_cols.iter().map(Alias::new));
                let sel = build_select_stmt(src, bx, 0)?;
                // 列数与源产出列数由 sea-query 在 `select_from` 里对齐：
                // 少了漏列、多了串行，两种错都是运行时才炸的 INSERT 语法错。
                // sea-query 的 Error 当前只有列数/值数不匹配一种形态。
                let mismatch = |sea_query::error::Error::ColValNumMismatch { col_len, val_len }| {
                    JsErrorBox::generic(format!(
                        "insert declares {col_len} columns but the source select yields {val_len}"
                    ))
                };
                ins.select_from(sel).map_err(mismatch)?;
            } else {
                let keys: Vec<String> = req.values[0].keys().cloned().collect();
                for k in &keys {
                    if !table.has_column(k) {
                        return Err(JsErrorBox::generic(format!(
                            "unknown column '{k}' in insert values"
                        )));
                    }
                }
                ins.columns(keys.iter().map(Alias::new));
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
            }
            // orIgnore（v0.1.81）：冲突行静默跳过，不抛唯一键冲突。
            // - sqlite / pg：`ON CONFLICT DO NOTHING`（无参）；
            // - mysql：无 DO NOTHING；8.0.20+ 才认，旧版只有
            //   `ON DUPLICATE KEY UPDATE <pk> = <pk>` 这个恒等赋值可用，
            //   故必须知道主键列名——没有主键就无从构造，直接拒绝而不是猜列名。
            if req.or_ignore {
                // sqlite 的 upsert 子句只能跟在 VALUES 后面：
                // `INSERT ... SELECT ... ON CONFLICT DO NOTHING` 是语法错
                // （sqlite 直接报 `near "DO": syntax error`），加 target 也不行。
                // 让 sqlite 吐这句谁也看不懂的错，不如在构造期就说清楚。
                if req.from_select.is_some() && dialect == Dialect::Sqlite {
                    return Err(JsErrorBox::generic(
                        "orIgnore with fromSelect is not supported on sqlite \
                         (its upsert clause only follows VALUES)",
                    ));
                }
                match dialect {
                    Dialect::MySql => {
                        if table.primary_key.is_empty() {
                            return Err(JsErrorBox::generic(format!(
                                "orIgnore on mysql requires a primary key on table '{}'",
                                req.table.name
                            )));
                        }
                        // mysql 渲染时丢弃 target 列表，只有 action 用得上（恒等赋值）。
                        let mut oc = OnConflict::new();
                        ins.on_conflict(
                            oc.do_nothing_on(
                                table.primary_key.iter().map(|c| Alias::new(c.as_str())),
                            )
                            .to_owned(),
                        );
                    }
                    _ => {
                        ins.on_conflict(OnConflict::new().do_nothing().to_owned());
                    }
                }
            }
            // returning 列：白名单校验（限定名自然被 has_column 拒）→ 渲染 RETURNING。
            // mysql 无 RETURNING：只接受单列，由 op 侧在同一目标上两步取 LAST_INSERT_ID()。
            if !req.returning.is_empty() {
                for c in &req.returning {
                    if !table.has_column(c) {
                        return Err(JsErrorBox::generic(format!(
                            "unknown column '{c}' in insert returning"
                        )));
                    }
                }
                if dialect == Dialect::MySql {
                    if req.returning.len() != 1 {
                        return Err(JsErrorBox::generic(
                            "mysql returning accepts exactly one column (no RETURNING support)",
                        ));
                    }
                } else {
                    ins.returning(ReturningClause::Columns(
                        req.returning
                            .iter()
                            .map(|c| Alias::new(c).into_column_ref())
                            .collect(),
                    ));
                }
            }
            params_of(build_sql(dialect, &ins))
        }
        Verb::Update => {
            // DML 带 joins/with 已被动词矩阵拒绝 → 条件上下文恒为真实基表、无联表。
            let table = reg.get(&req.table.name).ok_or_else(|| {
                JsErrorBox::generic(format!("unknown table '{}'", req.table.name))
            })?;
            let ctx = ColCtx {
                base_name: &req.table.name,
                base: TableSrc::Real(table),
                joins: Vec::new(),
                ctes: Vec::new(),
            };
            let mut up = Query::update();
            up.table(Alias::new(&req.table.name));
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
                .map(|c| cond_expr(c, &ctx, bx, 0, 1, &mut leaves))
                .collect::<Result<Vec<_>, _>>()?
            {
                up.and_where(e);
            }
            params_of(build_sql(dialect, &up))
        }
        Verb::Delete => {
            let ctx = ColCtx {
                base_name: &req.table.name,
                base: TableSrc::Real(reg.get(&req.table.name).ok_or_else(|| {
                    JsErrorBox::generic(format!("unknown table '{}'", req.table.name))
                })?),
                joins: Vec::new(),
                ctes: Vec::new(),
            };
            let mut del = Query::delete();
            del.from_table(Alias::new(&req.table.name));
            let mut leaves = 0usize;
            for e in req
                .conditions
                .iter()
                .map(|c| cond_expr(c, &ctx, bx, 0, 1, &mut leaves))
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
    bx: Build<'_>,
    depth: u8,
) -> Result<SelectStatement, JsErrorBox> {
    // CTE 虚拟表集合（本层 with 声明；嵌套层 validate_nested 已保证为空）。
    // WITH 名遮蔽同名真实表（SQL 语义）；CTE 名跳过 registry 解析。
    let cte_cols = |name: &str| {
        req.with
            .iter()
            .find(|c| c.name == name)
            .map(|c| c.columns.as_slice())
    };
    let resolve = |name: &str| -> Result<TableSrc<'_>, JsErrorBox> {
        if let Some(cols) = cte_cols(name) {
            return Ok(TableSrc::Cte(cols));
        }
        Ok(TableSrc::Real(bx.reg.get(name).ok_or_else(|| {
            JsErrorBox::generic(format!("unknown table '{name}'"))
        })?))
    };
    let ctes: Vec<(&str, TableSrc<'_>)> = req
        .with
        .iter()
        .map(|c| (c.name.as_str(), TableSrc::Cte(c.columns.as_slice())))
        .collect();
    let base = resolve(&req.table.name)?;
    // join 表先行解析（自 join / 空 on / 未知表在此拒绝；join 表可为 CTE 名）。
    let mut join_defs: Vec<(&str, TableSrc<'_>)> = Vec::new();
    for j in &req.joins {
        // 判定改为按 **sql_name()**（v0.1.81）：两侧都指向同一张表但别名不同
        // → 放行（`a AS x` JOIN `a AS y`）；SQL 侧标识撞车（含都无别名）→ 仍拒。
        // 文案按成因分开：同真名是自 join 没起别名，异真名是别名撞车。
        if j.table.sql_name() == req.table.sql_name() {
            let msg = if j.table.name == req.table.name {
                format!(
                    "self join needs distinct table aliases (got '{}')",
                    j.table.sql_name()
                )
            } else {
                format!("duplicate table alias '{}'", j.table.sql_name())
            };
            return Err(JsErrorBox::generic(msg));
        }
        if j.on.is_empty() {
            return Err(JsErrorBox::generic(format!(
                "join '{}' needs non-empty on",
                j.table.name
            )));
        }
        join_defs.push((j.table.sql_name(), resolve(&j.table.name)?));
    }
    let ctx = ColCtx {
        base_name: req.table.sql_name(),
        base,
        joins: join_defs,
        ctes,
    };
    let mut q = Query::select();
    // 聚合别名台账（alias → 原表达式），having 展开用（Task 11）。
    let mut agg_aliases: HashMap<String, SimpleExpr> = HashMap::new();
    // `select(["*"])`：与「不写 select」同义（展开 registry 全列）。
    // 与显式列混用 / 与 join 混用一律拒绝——SQL 里的 `*` 在 join 下会把两表列
    // 全捞出来（同名 id 撞名），与「基表全列」不是一回事，宁可报错也不猜（v0.1.81）。
    let star = req.columns.iter().any(|c| matches!(c, ColSpec::Star));
    if star {
        if req.columns.len() != 1 {
            return Err(JsErrorBox::generic("select '*' must be the only column"));
        }
        if !req.joins.is_empty() {
            return Err(JsErrorBox::generic("select '*' is not allowed with joins"));
        }
    }
    let cols: &[ColSpec] = if star { &[] } else { req.columns.as_slice() };
    if cols.is_empty() {
        // 全列；带 join 时全部限定为基表列（两表同名列如 id 歧义）。
        let names = ctx.base.col_names();
        let cols: Vec<SimpleExpr> = if req.joins.is_empty() {
            names.iter().map(|c| col_simple_expr(c)).collect()
        } else {
            names
                .iter()
                .map(|c| col_simple_expr(&format!("{}.{c}", req.table.sql_name())))
                .collect()
        };
        q.exprs(cols);
    } else {
        // site 复刻既有报错文案形态 `unknown column '<c>' on '<table>'`（既有测试锁定）。
        let site = format!("on '{}'", req.table.sql_name());
        for spec in cols {
            match spec {
                ColSpec::Star => unreachable!("star is filtered out above"),
                ColSpec::Lit(l) => {
                    // 别名形状与其他列规格同一把尺子（注入面在 Alias::new 之外再挡一层）。
                    check_alias(&l.r#as)?;
                    // 值必须是标量：对象/数组会被 to_qv 串成 `{"a":1}` / `["x"]`
                    // 这种谁也没想写的文本 —— 大整数标记对象除外（它是标量的载体）。
                    if !lit_is_scalar(&l.lit) {
                        return Err(JsErrorBox::generic(format!(
                            "lit column '{}' needs a scalar value (string/number/bool/null)",
                            l.r#as
                        )));
                    }
                    q.expr_as(lit_expr(&l.lit, bx.dialect), Alias::new(&l.r#as));
                }
                ColSpec::Name(c) => {
                    ctx.check_col(c, &site)?;
                    q.expr(col_simple_expr(c));
                }
                ColSpec::Agg(a) => {
                    let AggSpec {
                        r#fn,
                        field,
                        r#as,
                        distinct,
                    } = a;
                    // count(distinct col)（v0.1.82）：仅 count 支持 distinct，且必须有具名列
                    // （`COUNT(DISTINCT *)` 不是合法 SQL）。显式拒绝而非静默忽略——给了
                    // 不生效的键等于用户以为生效了。
                    if *distinct {
                        match (r#fn, field.as_deref()) {
                            (AggFn::Count, Some(f)) if f != "*" => {}
                            (AggFn::Count, _) => {
                                return Err(JsErrorBox::generic("count distinct needs a field"));
                            }
                            _ => {
                                return Err(JsErrorBox::generic(
                                    "distinct is only supported on count",
                                ));
                            }
                        }
                    }
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
                        AggFn::Count if *distinct => sea_query::Func::count_distinct(arg),
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
                ColSpec::Case(c) => {
                    if c.case.when.is_empty() {
                        return Err(JsErrorBox::generic("case needs non-empty when"));
                    }
                    if c.case.when.len() > CASE_WHEN_MAX {
                        return Err(JsErrorBox::generic(format!(
                            "case when count exceeds {CASE_WHEN_MAX}"
                        )));
                    }
                    check_alias(&c.r#as)?;
                    let mut case = sea_query::CaseStatement::new();
                    // 所有 when 共享每棵条件树 64 叶预算（F-3：原每 when 独立预算）。
                    let mut leaves = 0usize;
                    for w in &c.case.when {
                        let e = cond_expr(&w.cond, &ctx, bx, depth, 1, &mut leaves)?;
                        case = case.case(
                            sea_query::Condition::all().add(e),
                            Expr::val(to_qv(&w.then)),
                        );
                    }
                    if let Some(e) = &c.case.r#else {
                        case = case.finally(Expr::val(to_qv(e)));
                    }
                    q.expr_as(case, Alias::new(&c.r#as));
                }
                ColSpec::Window(w) => {
                    check_alias(&w.r#as)?;
                    let name = match w.window.r#fn {
                        WinFn::RowNumber => "ROW_NUMBER",
                        WinFn::Rank => "RANK",
                        WinFn::DenseRank => "DENSE_RANK",
                    };
                    let mut win = sea_query::WindowStatement::new();
                    for c in &w.window.partition_by {
                        ctx.check_col(c, "window partition_by")?;
                        win.add_partition_by(col_simple_expr(c));
                    }
                    for o in &w.window.order_by {
                        ctx.check_col(&o.field, "window order_by")?;
                        let dir = match o.dir.as_deref() {
                            Some("desc") => Order::Desc,
                            _ => Order::Asc,
                        };
                        win.order_by_columns([(Alias::new(&o.field), dir)]);
                    }
                    q.expr_window_as(
                        sea_query::Func::cust(Alias::new(name)),
                        win.take(),
                        Alias::new(&w.r#as),
                    );
                }
            }
        }
    }
    if req.distinct {
        q.distinct();
    }
    // FROM：有别名渲染 `t AS u`，否则逐字节保持现状（v0.1.81）。
    from_or_as(&mut q, &req.table);
    for j in &req.joins {
        let mut on = sea_query::Condition::all();
        for p in &j.on {
            match p {
                OnPair::Cols { left, right } => {
                    ctx.check_col(left, "join on")?;
                    ctx.check_col(right, "join on")?;
                    on = on.add(col_simple_expr(left).equals(col_ref(right)));
                }
                // 值条件只收比较类 op（解析期已挡，见 `OnPair::deserialize`）。
                OnPair::Val { left, op, value } => {
                    ctx.check_col(left, "join on")?;
                    on = on.add(op.apply(col_simple_expr(left), value)?);
                }
            }
        }
        // 多租户防护注入（apply_tenant 填充）：join 表 tenant_id 进 ON 子句。
        // 限定前缀用 **sql_name()**（别名）——JOIN 已渲染成 `t AS u`，
        // 用真名会得到不存在的限定表 → SQL 报错（v0.1.81）。
        if let Some(tid) = &j.tenant_id {
            on = on.add(
                col_simple_expr(&format!("{}.tenant_id", j.table.sql_name()))
                    .eq(Expr::val(to_qv(tid))),
            );
        }
        let jt = match j.kind {
            JoinKind::Inner => sea_query::JoinType::InnerJoin,
            JoinKind::Left => sea_query::JoinType::LeftJoin,
        };
        join_or_as(&mut q, jt, &j.table, on);
    }
    let mut leaves = 0usize;
    for c in &req.conditions {
        q.and_where(cond_expr(c, &ctx, bx, depth, 1, &mut leaves)?);
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
        let e = cond_expr_having(h, &ctx, &agg_aliases, bx, depth, 1, &mut leaves)?;
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
    // v0.1.20：limit 已在 op 层归一化（显式 clamp 到 max、顶层未给则补 default），
    // 构造器只按给定值渲染 —— 嵌套子查询/union 成员的 limit 仍为 None ⇒ 不隐式截断
    // （union 成员括号内禁 LIMIT：SQLite 复合项语法）。
    if let Some(l) = req.limit {
        q.limit(l as u64);
    }
    if let Some(off) = req.offset {
        q.offset(off as u64);
    }
    // union 臂：基查询与成员都须显式列、列数一致；成员禁排序/分页；
    // 嵌套约束（深度/动词/禁 unions）过 validate_nested，成员复用本函数构造。
    if !req.unions.is_empty() {
        if req.columns.is_empty() {
            return Err(JsErrorBox::generic("union requires explicit columns"));
        }
        for arm in &req.unions {
            validate_nested(&arm.query, depth + 1, "union")?;
            let m = &arm.query;
            if m.columns.is_empty() {
                return Err(JsErrorBox::generic("union requires explicit columns"));
            }
            if m.columns.len() != req.columns.len() {
                return Err(JsErrorBox::generic(format!(
                    "union column count mismatch: {} vs {}",
                    req.columns.len(),
                    m.columns.len()
                )));
            }
            if !m.order_by.is_empty() || m.limit.is_some() || m.offset.is_some() {
                return Err(JsErrorBox::generic(
                    "union member does not accept order_by/limit/offset",
                ));
            }
            let member = build_select_stmt(m, bx, depth + 1)?;
            let ty = match arm.kind {
                UnionKind::All => sea_query::UnionType::All,
                UnionKind::Distinct => sea_query::UnionType::Distinct,
            };
            q.union(ty, member);
        }
    }
    Ok(q)
}

/// 顶层 WITH 装配（build_select_stmt 不做 with 判断——嵌套嵌入只需要 SelectStatement；
/// SelectStatement::with 消费 self 返回 WithQuery，由 build_statement 的 Select 臂包装）。
/// name/columns 过别名形状校验；成员过 validate_nested（禁 with/unions/非 select）。
fn build_with_clause(
    req: &QueryReq,
    bx: Build<'_>,
    depth: u8,
) -> Result<sea_query::WithClause, JsErrorBox> {
    let mut clause = sea_query::WithClause::new();
    for c in &req.with {
        check_alias(&c.name)?;
        if c.columns.is_empty() {
            return Err(JsErrorBox::generic("cte needs non-empty columns"));
        }
        for col in &c.columns {
            check_alias(col)?;
        }
        validate_nested(&c.query, depth + 1, "cte")?;
        let mut cte = sea_query::CommonTableExpression::new();
        cte.table_name(Alias::new(&c.name));
        cte.columns(c.columns.iter().map(Alias::new));
        cte.query(build_select_stmt(&c.query, bx, depth + 1)?);
        clause.cte(cte);
    }
    Ok(clause)
}

/// 执行目标（池 / 事务会话）的统一出口。
/// mysql 无 RETURNING：取自增 id 必须在**同一目标**上连发 insert + SELECT
/// LAST_INSERT_ID()（两条语句非原子，池路径下建议放进 db.tx 以免并发串号）。
enum Exec<'a> {
    Pool(&'a Arc<dyn DataAccessor>),
    Tx(tokio::sync::MutexGuard<'a, Box<dyn super::db::TxSession>>),
}

impl Exec<'_> {
    /// 错误保持 `BridgeResult`（`Box<dyn Error>`）形态，**不得**在此 `to_string()` 压平：
    /// 压平会丢掉 ffi 侧的结构化信封（code/data/category/retryable），builder 路径
    /// 就退化成 generic 文案（T12 审查 blocker）。
    async fn query(
        &self,
        sql: &str,
        params: &[Value],
        meter: &mut super::db_quota::ByteMeter,
    ) -> BridgeResult<Vec<Value>> {
        match self {
            // REQ-05：pool 臂走字节计量拉取（触顶早停由 accessor 实现决定）；
            // tx 臂不计量（REQ 只列 query/stream/构造器三路径的 pool 形态）。
            Exec::Pool(da) => da.query_with_params_metered(sql, params, meter).await,
            Exec::Tx(s) => s.query(sql, params).await,
        }
    }

    async fn exec(&self, sql: &str, params: &[Value]) -> BridgeResult<i64> {
        match self {
            Exec::Pool(da) => da.exec_with_params(sql, params).await,
            Exec::Tx(s) => s.exec(sql, params).await,
        }
    }
}

/// 执行路由（三态）：select，或 pg/sqlite 的 insert+returning → 行集；
/// mysql 的 insert+returning → exec + 同目标 SELECT LAST_INSERT_ID() 两步；
/// 其余 DML → 受影响行数。**只有 insert+returning 才能进 LastId**——历史上曾用
/// `rows && dialect == MySql` 判定，把构造器 SELECT（也是行集）误路由进两步
/// 分支，`returning[0]` 越界 panic（v0.1.66 修复）。
///
/// 为什么不能改成按 `caps.id` 路由：`id_two_step` 只说明「取自增需要两条语句」，
/// **回取语句是后端方言私有的**（mysql `SELECT LAST_INSERT_ID()`、sqlite
/// `last_insert_rowid()`），不能由宿主从能力位推出。故 LastId 分支必须绑定到
/// 已知方言；`exec_route_matches_known_dialect_caps` 是对应守护（能力位改口径时它会红）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExecRoute {
    Rows,
    LastId,
    Affected,
}

fn exec_route(verb: Verb, returning_empty: bool, dialect: Dialect) -> ExecRoute {
    match verb {
        Verb::Select => ExecRoute::Rows,
        Verb::Insert if !returning_empty => match dialect {
            Dialect::MySql => ExecRoute::LastId,
            _ => ExecRoute::Rows,
        },
        _ => ExecRoute::Affected,
    }
}

/// op_db_query_build：结构化查询 -> 参数化 SQL -> 执行。
/// select → rows 数组；insert 带 returning → rows 数组（pg/sqlite 走 sea-query
/// RETURNING 单语句；mysql 同目标两步）；其余 DML → 受影响行数 number。
/// 路由同 db.rs：本库活跃 tx → 会话，否则池，他库 tx → 报错。
/// 标识符（表/列）全部经 SchemaRegistry 白名单校验；值参数化。
#[op2]
#[serde]
pub async fn op_db_query_build(
    state: Rc<RefCell<OpState>>,
    #[serde] req: QueryReq,
) -> Result<serde_json::Value, JsErrorBox> {
    let reg = registry(&state)?;
    guard_req(&state, &req)?;
    validate_truncate(&req)?;
    // db 访问控制（v0.1.89）：构造器路径走**精确意图**判定——verb 已知，不经词法，
    // 因此不存在「分类失败」的误报面（与裸 SQL 路径的 best-effort 分类相对）。
    let alias = super::guard::bound_db(&state, &req.db);
    let intent = super::db_policy::Classified {
        risk: match req.verb {
            Verb::Select => super::db_policy::Risk::ReadOnly,
            Verb::Insert | Verb::Update | Verb::Delete => super::db_policy::Risk::Write,
        },
        // 无条件的 update/delete = 全表写 → 危险写（受 allow_dangerous 门）。
        dangerous: matches!(req.verb, Verb::Update | Verb::Delete) && req.conditions.is_empty(),
        statements: 1,
    };
    // G9：危险语句确认门在 off 早退前过（intent 内部有早退，见 db_policy 调用点约定）。
    super::db_policy::dangerous_confirm_gate(&state, &alias, &intent)?;
    let row_cap = super::db_policy::check_access_intent(&state, &alias, &intent)?;
    let mut req = req;
    apply_tenant_guard(&state, &mut req, &reg)?;
    // LIMIT 归一化（v0.1.20，db_query: 段）：顶层 select 未给 limit → 补 default_limit；
    // 显式 limit → clamp 到 max_limit。返回值用于截断可观测。
    let applied = normalize_limit(&state, &mut req, row_cap);
    // 门控（T8）：未知方言（不在 builder-known 子集）一律结构化 unsupported，绝不回落 Sqlite。
    let da = lookup(&state, &req.db)?;
    let dialect = require_known_dialect(da.dialect_info(), "query_build")?;
    let (sql, params) = build_statement(&req, &reg, dialect)?;
    // 执行路由：行集 / mysql 两步取自增 id / 受影响行数（判定见 ExecRoute）。
    let route = exec_route(req.verb, req.returning.is_empty(), dialect);
    let err = |e: Box<dyn std::error::Error + Send + Sync>| super::db::to_js_err(e);

    // 活跃事务路由：会话（跨请求事务）/ 本库 tx / 无 tx 池 / 他库 tx 报错（同 db.rs）。
    let target = super::db::resolve_target(&state, &req.db)?;
    let ex = match &target {
        super::db::Target::Pool(da) => Exec::Pool(da),
        super::db::Target::Tx(t) => {
            // 会话有寿命：等锁也要封顶（长查询会把锁占到过期之后）。查询本身不另设超时
            // ——与 `db.tx` 同口径（sqlx 无语句级取消，强杀会把连接留在未知状态）。
            let guard = match super::db_session::attached_deadline(&state) {
                Some(left) => match tokio::time::timeout(left, t.session.lock()).await {
                    Ok(g) => g,
                    Err(_) => {
                        super::db_session::kill_attached(&state, "lifetime_exceeded");
                        return Err(JsErrorBox::generic(
                            "db session lifetime exhausted while waiting for the connection; \
                             the session was discarded (transaction rolled back)",
                        ));
                    }
                },
                None => t.session.lock().await,
            };
            Exec::Tx(guard)
        }
    };
    let in_tx = matches!(&target, super::db::Target::Tx(_));
    let db_name = req.db.clone();
    // REQ-05：构造器路径的语句超时（只吃配置侧——裁决：per-call opts 只给裸 SQL 面）
    // 与行级字节计量。tx 臂不叠语句超时（与 db.tx 同口径，连接状态不可知）。
    let quota = super::db_policy::quota_for(&state, &alias);
    let timeout_ms = super::db_quota::effective_timeout_ms(quota.statement_timeout_ms, None)
        .map_err(JsErrorBox::from)?;
    let mut meter = super::db_quota::ByteMeter::new(quota.max_result_bytes, quota.on_exceed);
    let (arm_timeout, cancelled) = match &ex {
        Exec::Pool(da) => (timeout_ms, da.server_cancel_on_abandon()),
        Exec::Tx(_) => (None, false),
    };
    let start = std::time::Instant::now();
    let mut out = if route == ExecRoute::Rows {
        match super::db_quota::race_statement(
            ex.query(&sql, &params, &mut meter),
            &alias,
            arm_timeout,
            cancelled,
        )
        .await
        {
            Ok(r) => {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                super::db::trace_sql(
                    &state,
                    &sql,
                    &params,
                    &db_name,
                    in_tx,
                    ms,
                    Some(r.len() as i64),
                    true,
                    None,
                    None,
                    None,
                );
                // REQ-07a：语句级降级清单外发（与 db_query.rs 裸 SQL 路同一口径/同一
                // 出口——权威判定面是头，前缀只是单元格编码）。
                super::db_query::push_degraded(&state, oj_plugin_ffi::cellenv::degraded_of(&r));
                Ok(Value::Array(r))
            }
            Err(e) => {
                let em = e.to_string();
                // 结构化错误（来自 FFI 信封）必须原样带到 JS：code/data 经 to_js_err 还原，
                // category/retryable 同时落 SQL 画像（T12 在 builder 路径的对应半边）。
                let (cat, retry) = super::db::err_meta_of(e.as_ref());
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                super::db::trace_sql(
                    &state,
                    &sql,
                    &params,
                    &db_name,
                    in_tx,
                    ms,
                    None,
                    false,
                    Some(em),
                    cat,
                    retry,
                );
                Err(super::db::to_js_err(e))
            }
        }
    } else if route == ExecRoute::LastId {
        match super::db_quota::race_statement(
            ex.exec(&sql, &params),
            &alias,
            arm_timeout,
            cancelled,
        )
        .await
        {
            Ok(_) => {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                super::db::trace_sql(
                    &state, &sql, &params, &db_name, in_tx, ms, None, true, None, None, None,
                );
                // mysql 无 RETURNING：再发一次 LAST_INSERT_ID 取回自增主键。
                let r = super::db_quota::race_statement(
                    ex.query("SELECT LAST_INSERT_ID() AS id", &[], &mut meter),
                    &alias,
                    arm_timeout,
                    cancelled,
                )
                .await
                .map_err(err)?;
                let ms2 = start.elapsed().as_secs_f64() * 1000.0;
                super::db::trace_sql(
                    &state,
                    "SELECT LAST_INSERT_ID() AS id",
                    &[],
                    &db_name,
                    in_tx,
                    ms2 - ms,
                    Some(r.len() as i64),
                    true,
                    None,
                    None,
                    None,
                );
                let v = r
                    .first()
                    .and_then(|row| row.get("id"))
                    .cloned()
                    .unwrap_or(Value::Null);
                let mut obj = serde_json::Map::new();
                obj.insert(req.returning[0].clone(), v);
                Ok(Value::Array(vec![Value::Object(obj)]))
            }
            Err(e) => {
                let em = e.to_string();
                // 结构化错误（来自 FFI 信封）必须原样带到 JS：code/data 经 to_js_err 还原，
                // category/retryable 同时落 SQL 画像（T12 在 builder 路径的对应半边）。
                let (cat, retry) = super::db::err_meta_of(e.as_ref());
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                super::db::trace_sql(
                    &state,
                    &sql,
                    &params,
                    &db_name,
                    in_tx,
                    ms,
                    None,
                    false,
                    Some(em),
                    cat,
                    retry,
                );
                Err(super::db::to_js_err(e))
            }
        }
    } else {
        match super::db_quota::race_statement(
            ex.exec(&sql, &params),
            &alias,
            arm_timeout,
            cancelled,
        )
        .await
        {
            Ok(n) => {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                super::db::trace_sql(
                    &state,
                    &sql,
                    &params,
                    &db_name,
                    in_tx,
                    ms,
                    Some(n),
                    true,
                    None,
                    None,
                    None,
                );
                Ok(Value::from(n))
            }
            Err(e) => {
                let em = e.to_string();
                // 结构化错误（来自 FFI 信封）必须原样带到 JS：code/data 经 to_js_err 还原，
                // category/retryable 同时落 SQL 画像（T12 在 builder 路径的对应半边）。
                let (cat, retry) = super::db::err_meta_of(e.as_ref());
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                super::db::trace_sql(
                    &state,
                    &sql,
                    &params,
                    &db_name,
                    in_tx,
                    ms,
                    None,
                    false,
                    Some(em),
                    cat,
                    retry,
                );
                Err(super::db::to_js_err(e))
            }
        }
    };
    // REQ-05：字节触顶分档（同 raw 路径口径）——fail → 结构化 OJ_DB_CAPACITY；
    // truncate → 交付已取行 + X-OJ-Result-Truncated 信令。查询本身已失败时原错误优先。
    if meter.exceeded && out.is_ok() {
        match quota.on_exceed {
            super::db_policy::OnExceed::Fail => {
                return Err(JsErrorBox::from(super::db_quota::result_too_large_error(
                    &alias,
                    quota.max_result_bytes.unwrap_or(0),
                    meter.counted,
                )));
            }
            super::db_policy::OnExceed::Truncate => {
                state
                    .borrow_mut()
                    .borrow_mut::<super::ReqState>()
                    .headers
                    .push(("X-OJ-Result-Truncated".into(), meter.counted.to_string()));
            }
        }
    }
    // 出口护栏（见 jsnum）：行/合成行/受影响行数里的超界整数降十进制字符串，防 JS 侧 BigInt。
    if let Ok(v) = out.as_mut() {
        super::jsnum::sanitize_js_numbers(v);
    }
    // 截断可观测（v0.1.20）：返回行数达到生效上限 ⇒ 可能是被 LIMIT 截断的（含「显式
    // limit 被 clamp」这类旧版完全静默的情形）。写响应头而不动信封形状（契约不变）。
    if let (Some(a), Ok(Value::Array(v))) = (applied, &out)
        && v.len() >= a as usize
    {
        state
            .borrow_mut()
            .borrow_mut::<super::ReqState>()
            .headers
            .push(("X-OJ-Row-Limit".into(), a.to_string()));
    }
    out
}

/// truncate 的用户输入门禁（v0.1.82）：truncate 拒绝自带 where（带条件的清空用 delete）。
/// 两个构造器 op（执行 / toSQL）入口都要调，且必须在 apply_tenant_guard **之前**——
/// 租户注入的 `tenant_id = ?` 叶是引擎写的，不是用户的，不能挡（租户表上 truncate
/// 语义 = 只清当前租户，照常注入，见 sql-builder-guide）。
fn validate_truncate(req: &QueryReq) -> Result<(), JsErrorBox> {
    if req.truncate && !req.conditions.is_empty() {
        return Err(JsErrorBox::generic("truncate does not accept where"));
    }
    Ok(())
}

/// 顶层 select 的 LIMIT 归一化（`db_query:` 段；v0.1.20）。
/// 非 select（insert/update/delete 不接受 limit）返回 None。
fn normalize_limit(
    state: &Rc<RefCell<OpState>>,
    req: &mut QueryReq,
    row_cap: Option<u32>,
) -> Option<u32> {
    if req.verb != Verb::Select {
        return None;
    }
    let limits = state.borrow().borrow::<Arc<StableState>>().query_limits;
    // db 访问控制的 `max_rows` 与 `db_query` 生效值取小（两者都声明时以更严者为准）。
    let applied = match row_cap {
        Some(cap) => std::cmp::Ord::min(limits.applied(req.limit), cap),
        None => limits.applied(req.limit),
    };
    req.limit = Some(applied);
    Some(applied)
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
    validate_truncate(&req)?;
    let mut req = req;
    apply_tenant_guard(&state, &mut req, &reg)?;
    // 与执行路径同款归一化 —— toSQL 必须反映真实 SQL（含隐式 LIMIT）。
    let _ = normalize_limit(&state, &mut req, None);
    // 门控（T8）：toSQL 同样属 oj 生成 SQL 路径，未知方言不回落 Sqlite。
    let da = lookup(&state, &req.db)?;
    let dialect = require_known_dialect(da.dialect_info(), "query_sql")?;
    let (sql, params) = build_statement(&req, &reg, dialect)?;
    // 出口护栏（见 jsnum）：`toSQL().params` 里的大整数同样会以 BigInt 交给 JS（json.ok 会 500）。
    let mut out = serde_json::json!({ "sql": sql, "params": params });
    super::jsnum::sanitize_js_numbers(&mut out);
    Ok(out)
}

/// 两构造器 op 共用的租户预变换：读 StableState.sql_guard + ReqState（tenant_id/system），
/// Off 或 system 逃生口直接放行。
fn apply_tenant_guard(
    state: &Rc<RefCell<OpState>>,
    req: &mut QueryReq,
    reg: &SchemaRegistry,
) -> Result<(), JsErrorBox> {
    let g = state.borrow();
    let guard = g.borrow::<Arc<StableState>>().sql_guard;
    if guard == super::SqlGuard::Off {
        return Ok(());
    }
    let (tid, system) = {
        let rs = g.borrow::<super::ReqState>();
        (rs.req.tenant_id.clone(), rs.system)
    };
    apply_tenant(req, reg, tid.as_deref(), guard, system)
}

/// 按方言出 SQL（QueryStatementWriter::build 泛型，四类 statement 通吃）。
pub(crate) fn build_sql<S: sea_query::QueryStatementWriter>(
    d: Dialect,
    q: &S,
) -> (String, sea_query::Values) {
    match d {
        Dialect::Sqlite => q.build(SqliteQueryBuilder),
        Dialect::MySql => q.build(sea_query::MysqlQueryBuilder),
        Dialect::Postgres => q.build(sea_query::PostgresQueryBuilder),
    }
}

/// sea-query 的 `Value` 转 serde_json::Value（简化：整数/浮点/字符串/布尔/ null）。
///
/// **超出安全范围的整数回吐为 marker 而不是 JSON number**（v0.1.24 修既有漏洞）：这里的
/// 产物就是 `toSQL().params`，而文档教用户「`db.query(toSQL().sql, ...toSQL().params)` 回跑」；
/// 若吐 number，出口护栏 `jsnum::sanitize_js_numbers` 会把 `>2^53` 的整数降成**字符串**，
/// 回跑时被绑成 text（PG 报 `bigint but expression is of type text`）——即参数不可重放。
/// 吐 marker 后既可重放又保持精确；插件侧 `bind_value` 认得这两个形状（见 `jsint`）。
/// 安全范围内的整数仍吐 number（回放与可读性都不变）。
// REQ-12：导入批渲染复用（db_import::render）。
pub(crate) fn value_to_json(v: &Qv) -> Result<Value, JsErrorBox> {
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
        Qv::BigInt(Some(i)) => int_param(*i),
        // sea-query 将 LIMIT/OFFSET 渲染为 unsigned 绑定参数，缺失会退化为 NULL 绑定。
        Qv::TinyUnsigned(Some(i)) => Value::from(*i as i64),
        Qv::SmallUnsigned(Some(i)) => Value::from(*i as i64),
        Qv::Unsigned(Some(i)) => Value::from(*i as i64),
        // u64 直出（勿 `as i64`：> i64::MAX 会回绕成负数，静默错值）。超界部分由 op 出口的
        // jsnum 护栏降为十进制字符串；此处先按可重放口径给 marker。
        Qv::BigUnsigned(Some(i)) => uint_param(*i),
        Qv::Float(Some(f)) => num(*f as f64),
        Qv::Double(Some(f)) => num(*f),
        Qv::String(Some(s)) => Value::String(s.to_string()),
        _ => Value::Null,
    })
}

/// i64 参数回吐：安全范围内给 number，超出给 `$oj$i64` marker（可重放，见 `value_to_json`）。
fn int_param(i: i64) -> Value {
    if (-super::jsnum::MAX_SAFE_INT..=super::jsnum::MAX_SAFE_INT).contains(&i) {
        Value::from(i)
    } else {
        serde_json::json!({ oj_plugin_ffi::jsint::I64_MARKER: i.to_string() })
    }
}

/// u64 参数回吐：≤2^53-1 给 number；≤i64::MAX 用 `$oj$i64`；再往上只能用 `$oj$u64`
/// （只有 MySQL `BIGINT UNSIGNED` 能承载，PG/SQLite 会明确报错）。
fn uint_param(u: u64) -> Value {
    if u <= super::jsnum::MAX_SAFE_INT as u64 {
        Value::from(u)
    } else if u <= i64::MAX as u64 {
        serde_json::json!({ oj_plugin_ffi::jsint::I64_MARKER: u.to_string() })
    } else {
        serde_json::json!({ oj_plugin_ffi::jsint::U64_MARKER: u.to_string() })
    }
}

/// 按名取 DataAccessor（默认 default）。模块 db 绑定在此收敛重定向：
/// manifest 声明 `db: <name>` 的模块，其字面 "default" 调用落到命名库（§5.3）；
/// 显式 DB("name") 不受影响。tx 以 JS 可见名记账，仅 accessor 解析被重定向。
pub(crate) fn lookup(
    state: &Rc<RefCell<OpState>>,
    name: &str,
) -> Result<Arc<dyn DataAccessor>, JsErrorBox> {
    let name = super::guard::bound_db(state, name);
    let g = state.borrow();
    let st = g.borrow::<Arc<StableState>>();
    // 静态库优先；miss 再回落运行期动态登记库（L3 v0.1.91）。
    if let Some(d) = st.dbs.get(&name) {
        return Ok(d.clone());
    }
    if let Some(a) = st.client_dbs.accessor_and_count(&name) {
        return Ok(a);
    }
    Err(JsErrorBox::generic(format!(
        "db: instance '{name}' not configured"
    )))
}

/// 仅用于 trait 约束引用，避免 unused import 警告。
#[allow(dead_code)]
fn _assert(_: &BridgeResult<()>) {}

#[cfg(test)]
#[path = "query_tests.rs"]
mod tests;
