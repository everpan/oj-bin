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
                Ok(TableRef { name, alias })
            }
            Value::Object(mut m) => {
                let name = m
                    .remove("name")
                    .and_then(|v| v.as_str().map(str::to_string))
                    .ok_or_else(|| Error::custom("table object needs string 'name'"))?;
                let alias = m.remove("as").and_then(|v| v.as_str().map(str::to_string));
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

fn to_qv(v: &Value) -> Qv {
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
fn escape_like(s: &str, esc: char) -> String {
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
    async fn query(&self, sql: &str, params: &[Value]) -> Result<Vec<Value>, String> {
        match self {
            Exec::Pool(da) => da
                .query_with_params(sql, params)
                .await
                .map_err(|e| e.to_string()),
            Exec::Tx(s) => s.query(sql, params).await.map_err(|e| e.to_string()),
        }
    }

    async fn exec(&self, sql: &str, params: &[Value]) -> Result<i64, String> {
        match self {
            Exec::Pool(da) => da
                .exec_with_params(sql, params)
                .await
                .map_err(|e| e.to_string()),
            Exec::Tx(s) => s.exec(sql, params).await.map_err(|e| e.to_string()),
        }
    }
}

/// 执行路由（三态）：select，或 pg/sqlite 的 insert+returning → 行集；
/// mysql 的 insert+returning → exec + 同目标 SELECT LAST_INSERT_ID() 两步；
/// 其余 DML → 受影响行数。**只有 insert+returning 才能进 LastId**——历史上曾用
/// `rows && dialect == MySql` 判定，把构造器 SELECT（也是行集）误路由进两步
/// 分支，`returning[0]` 越界 panic（v0.1.66 修复）。
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
    let mut req = req;
    apply_tenant_guard(&state, &mut req, &reg)?;
    // LIMIT 归一化（v0.1.20，db_query: 段）：顶层 select 未给 limit → 补 default_limit；
    // 显式 limit → clamp 到 max_limit。返回值用于截断可观测。
    let applied = normalize_limit(&state, &mut req);
    let dialect = lookup(&state, &req.db)?.dialect();
    let (sql, params) = build_statement(&req, &reg, dialect)?;
    // 执行路由：行集 / mysql 两步取自增 id / 受影响行数（判定见 ExecRoute）。
    let route = exec_route(req.verb, req.returning.is_empty(), dialect);
    let err = |e: String| JsErrorBox::generic(e);

    // 活跃事务路由：本库 tx 会话 / 无 tx 池 / 他库 tx 报错（同 db.rs）。
    let target = super::db::resolve_target(&state, &req.db)?;
    let ex = match &target {
        super::db::Target::Pool(da) => Exec::Pool(da),
        super::db::Target::Tx(t) => Exec::Tx(t.session.lock().await),
    };
    let in_tx = matches!(&target, super::db::Target::Tx(_));
    let db_name = req.db.clone();
    let start = std::time::Instant::now();
    let mut out = if route == ExecRoute::Rows {
        match ex.query(&sql, &params).await {
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
                );
                Ok(Value::Array(r))
            }
            Err(e) => {
                let em = e.to_string();
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
                    Some(em.clone()),
                );
                Err(JsErrorBox::generic(em))
            }
        }
    } else if route == ExecRoute::LastId {
        match ex.exec(&sql, &params).await {
            Ok(_) => {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                super::db::trace_sql(&state, &sql, &params, &db_name, in_tx, ms, None, true, None);
                // mysql 无 RETURNING：再发一次 LAST_INSERT_ID 取回自增主键。
                let r = ex
                    .query("SELECT LAST_INSERT_ID() AS id", &[])
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
                    Some(em.clone()),
                );
                Err(JsErrorBox::generic(em))
            }
        }
    } else {
        match ex.exec(&sql, &params).await {
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
                );
                Ok(Value::from(n))
            }
            Err(e) => {
                let em = e.to_string();
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
                    Some(em.clone()),
                );
                Err(JsErrorBox::generic(em))
            }
        }
    };
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

/// 顶层 select 的 LIMIT 归一化（`db_query:` 段；v0.1.20）。
/// 非 select（insert/update/delete 不接受 limit）返回 None。
fn normalize_limit(state: &Rc<RefCell<OpState>>, req: &mut QueryReq) -> Option<u32> {
    if req.verb != Verb::Select {
        return None;
    }
    let limits = state.borrow().borrow::<Arc<StableState>>().query_limits;
    let applied = limits.applied(req.limit);
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
    let mut req = req;
    apply_tenant_guard(&state, &mut req, &reg)?;
    // 与执行路径同款归一化 —— toSQL 必须反映真实 SQL（含隐式 LIMIT）。
    let _ = normalize_limit(&state, &mut req);
    let (sql, params) = build_statement(&req, &reg, lookup(&state, &req.db)?.dialect())?;
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
fn build_sql<S: sea_query::QueryStatementWriter>(d: Dialect, q: &S) -> (String, sea_query::Values) {
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

    /// 执行路由矩阵（F1 回归钉子）：**Select 在 mysql 上绝不能进 LastId 两步
    /// 分支**——returning 为空会在 `returning[0]` 越界 panic（v0.1.65 实锤，
    /// mysql 全业务面请求挂死）。
    #[test]
    fn exec_route_matrix() {
        // select：任何方言都走行集（对抗用例：mysql + select 曾误路由 LastId）。
        for d in [Dialect::Sqlite, Dialect::MySql, Dialect::Postgres] {
            assert_eq!(exec_route(Verb::Select, true, d), ExecRoute::Rows);
            assert_eq!(exec_route(Verb::Select, false, d), ExecRoute::Rows);
        }
        // insert + returning：pg/sqlite 走 RETURNING 行集；mysql 走两步 LastId。
        assert_eq!(
            exec_route(Verb::Insert, false, Dialect::MySql),
            ExecRoute::LastId
        );
        assert_eq!(
            exec_route(Verb::Insert, false, Dialect::Postgres),
            ExecRoute::Rows
        );
        assert_eq!(
            exec_route(Verb::Insert, false, Dialect::Sqlite),
            ExecRoute::Rows
        );
        // insert 无 returning：受影响行数（mysql 也不例外——LAST_INSERT_ID 无处安放）。
        assert_eq!(
            exec_route(Verb::Insert, true, Dialect::MySql),
            ExecRoute::Affected
        );
        // update / delete：一律受影响行数。
        for v in [Verb::Update, Verb::Delete] {
            assert_eq!(exec_route(v, true, Dialect::MySql), ExecRoute::Affected);
            assert_eq!(exec_route(v, false, Dialect::Postgres), ExecRoute::Affected);
        }
    }

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

    /// `value_to_json` 的 u64 不再回绕（v0.1.22）：`> i64::MAX` 直出 u64，
    /// 避免旧实现 `as i64` 静默变负数；超界部分由 op 出口护栏降为十进制字符串。
    #[test]
    fn value_to_json_keeps_u64_and_bigint_exact() {
        assert_eq!(
            value_to_json(&Qv::BigInt(Some(i64::MIN))).unwrap(),
            json!({ oj_plugin_ffi::jsint::I64_MARKER: i64::MIN.to_string() })
        );
        assert_eq!(
            value_to_json(&Qv::BigUnsigned(Some(u64::MAX))).unwrap(),
            json!({ oj_plugin_ffi::jsint::U64_MARKER: u64::MAX.to_string() })
        );
        // 安全范围内仍是 number（回放与可读性不变）。
        assert_eq!(value_to_json(&Qv::BigUnsigned(Some(0))).unwrap(), json!(0));
        assert_eq!(value_to_json(&Qv::BigInt(Some(42))).unwrap(), json!(42));
        // >2^53 但 ≤i64::MAX：用 i64 marker（u64 的那一段才需要 $oj$u64）。
        assert_eq!(
            value_to_json(&Qv::BigUnsigned(Some(9223372036854775807))).unwrap(),
            json!({ oj_plugin_ffi::jsint::I64_MARKER: "9223372036854775807" })
        );
        // **可重放**（v0.1.24 修的既有漏洞）：出口护栏不再把参数降成字符串，
        // 于是 `db.query(toSQL().sql, ...toSQL().params)` 能原样回跑。
        let mut params = vec![
            value_to_json(&Qv::BigInt(Some(4886674138783273204))).unwrap(),
            value_to_json(&Qv::BigUnsigned(Some(u64::MAX))).unwrap(),
        ];
        super::super::jsnum::sanitize_rows(&mut params);
        assert_eq!(
            params[0],
            json!({ oj_plugin_ffi::jsint::I64_MARKER: "4886674138783273204" }),
            "i64 大整数参数必须可重放（不得被降成字符串）"
        );
        assert_eq!(
            params[1],
            json!({ oj_plugin_ffi::jsint::U64_MARKER: "18446744073709551615" })
        );
        // 而 DB **读出来的**大整数仍按老口径降为十进制字符串（读侧契约不变）。
        let mut row = json!({ "id": 4886674138783273204i64 });
        super::super::jsnum::sanitize_js_numbers(&mut row);
        assert_eq!(row["id"], json!("4886674138783273204"));
    }

    // ----- LIMIT 配置（db_query 段，v0.1.20）-----

    /// 10 行夹具 + `db_query: {default_limit: 3, max_limit: 5}`。
    async fn limited_bridge() -> Bridge {
        let db = SqlxAccessor::arc("sqlite::memory:").await.unwrap();
        db.exec_with_params("create table t (id integer primary key, name text)", &[])
            .await
            .unwrap();
        for i in 0..10 {
            db.exec_with_params("insert into t (name) values (?)", &[json!(format!("n{i}"))])
                .await
                .unwrap();
        }
        let reg = SchemaRegistry::new().table("t", &["id"], &["id", "name"]);
        Bridge::with_dbs_and_loader(
            std::collections::HashMap::from([("default".to_string(), db as _)]),
            Arc::new(InMemoryKV::new()),
            reg,
            false,
            None,
            Extras {
                query_limits: QueryLimits {
                    default_limit: 3,
                    max_limit: 5,
                },
                ..Default::default()
            },
        )
    }

    /// 隐式 default_limit 生效 + 截断写 `X-OJ-Row-Limit` 响应头（不再静默少数据）。
    #[tokio::test(flavor = "current_thread")]
    async fn query_limit_defaults_and_truncation_header() {
        let b = limited_bridge().await;
        // 未给 limit → default_limit=3；返回行数 == 上限 ⇒ 头文件（可能是被截断的）
        let cap = b
            .run(
                r#"db.table("t").select(["name"]).all()
                     .then(r => json.ok({ n: r.length })).catch(e => json.fail(500, String(e)));"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!((&v["code"], &v["data"]["n"]), (&json!(0), &json!(3)), "{v}");
        assert_eq!(cap.header("X-OJ-Row-Limit"), Some("3"));
        // 显式 limit 被 clamp 到 max_limit=5 也写头（旧版这类截断完全静默）
        let cap = b
            .run(
                r#"db.table("t").select(["name"]).limit(1000).all()
                     .then(r => json.ok({ n: r.length })).catch(e => json.fail(500, String(e)));"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["data"]["n"], json!(5), "{v}");
        assert_eq!(cap.header("X-OJ-Row-Limit"), Some("5"));
        // 行数 < 生效上限 ⇒ 不可能被截断，不写头（避免噪音）
        let cap = b
            .run(
                r#"db.table("t").select(["name"]).where({field:"name",op:"eq",value:"n0"}).limit(5).all()
                     .then(r => json.ok({ n: r.length })).catch(e => json.fail(500, String(e)));"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["data"]["n"], json!(1), "{v}");
        assert!(cap.header("X-OJ-Row-Limit").is_none(), "{cap:?}");
        // toSQL 与执行同款归一化（诊断口径一致）
        // LIMIT/OFFSET 是绑定参数（sea-query 渲染为 `LIMIT ?`），故断言参数而非文本。
        let cap = b
            .run(r#"json.ok(db.table("t").select(["name"]).toSQL());"#)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(v["data"]["sql"].as_str().unwrap().contains("LIMIT"), "{v}");
        assert!(
            v["data"]["params"].as_array().unwrap().contains(&json!(3)),
            "{v}"
        );
    }

    /// 拼错的键必须报错（否则 `db_query: {default: 1000}` 会静默取默认值，用户以为生效了）。
    #[test]
    fn query_limits_reject_unknown_keys() {
        let ok: QueryLimits =
            serde_json::from_str(r#"{"default_limit": 5, "max_limit": 9}"#).unwrap();
        assert_eq!((ok.default_limit, ok.max_limit), (5, 9));
        // 缺字段 → 走 Default（`#[serde(default)]`），不是错误。
        let partial: QueryLimits = serde_json::from_str("{}").unwrap();
        assert_eq!(partial, QueryLimits::default());
        // 未知键 → 错误。
        assert!(serde_json::from_str::<QueryLimits>(r#"{"default": 5}"#).is_err());
    }

    /// 装配期校验：0 / 倒置 / 超硬顶 一律 fail-fast（不给静默回落）。
    #[test]
    fn query_limits_validate() {
        assert!(QueryLimits::default().validate().is_ok());
        assert!(
            QueryLimits {
                default_limit: 0,
                max_limit: 100
            }
            .validate()
            .is_err()
        );
        assert!(
            QueryLimits {
                default_limit: 10,
                max_limit: 0
            }
            .validate()
            .is_err()
        );
        assert!(
            QueryLimits {
                default_limit: 10,
                max_limit: LIMIT_HARD_CAP + 1
            }
            .validate()
            .is_err()
        );
        assert!(
            QueryLimits {
                default_limit: 10,
                max_limit: 5
            }
            .validate()
            .is_err()
        );
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
                "self join needs distinct table aliases",
            ),
            (
                // 不同真名但别名撞车：SQL 侧两个都是 "x"
                r#"db.table("a").as("x").join("b",[{left:"x.id",right:"x.aid"}],"inner","x").select(["x.id"]).all()"#,
                "duplicate table alias 'x'",
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
            base: TableSrc::Real(reg.get("a").unwrap()),
            joins: vec![("b", TableSrc::Real(reg.get("b").unwrap()))],
            ctes: Vec::new(),
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
        // insert：values 与 fromSelect 两缺 / 带 where / 带 limit
        assert!(
            bad(r#"{"table":"t","verb":"insert"}"#)
                .to_string()
                .contains("insert needs either values or fromSelect")
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

    #[tokio::test(flavor = "current_thread")]
    async fn union_all_and_distinct() {
        let b = seeded_bridge_2t().await;
        // a.name {x,y} ∪ b.label {L1,L2,L3}：all=5 行，distinct=5 行（无重叠）
        let cap = b
            .run(
                r#"db.table("a").select(["name"])
            .union(db.table("b").select(["label"]), "all").all()
            .then(r=>json.ok({n:r.length})).catch(e=>json.fail(400,String(e)));"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["data"]["n"], 5, "{v}");
        let cap = b
            .run(
                r#"db.table("a").select(["name"])
            .union(db.table("b").select(["label"]).where({field:"aid",op:"eq",value:1})).all()
            .then(r=>json.ok({n:r.length})).catch(e=>json.fail(400,String(e)));"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["data"]["n"], 4, "{v}"); // 2 + 2(L1,L2)
        // toSQL 含 UNION ALL（同步 op，直接进 json.ok）
        let cap = b
            .run(
                r#"json.ok(db.table("a").select(["name"])
            .union(db.table("b").select(["label"]), "all").toSQL());"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(
            v["data"]["sql"].as_str().unwrap().contains("UNION ALL"),
            "{v}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn union_rejections() {
        let b = seeded_bridge_2t().await;
        for (js, want) in [
            // 列数不一致
            (
                r#"db.table("a").select(["id","name"]).union(db.table("b").select(["label"])).all()"#,
                "union column count mismatch",
            ),
            // 成员带 limit
            (
                r#"db.table("a").select(["name"]).union(db.table("b").select(["label"]).limit(1)).all()"#,
                "union member does not accept order_by/limit/offset",
            ),
            // 基查询隐式列（union 必须显式 columns）
            (
                r#"db.table("a").union(db.table("b").select(["label"])).all()"#,
                "union requires explicit columns",
            ),
            // union 套 union（嵌套禁 unions）
            (
                r#"db.table("a").select(["name"]).union(db.table("b").select(["label"])
             .union(db.table("b").select(["label"]))).all()"#,
                "nested select does not accept with/unions",
            ),
            // insert 带 unions（动词矩阵）
            (
                r#"db.table("a").insert({name:"z"}).union(db.table("b").select(["label"])).run()"#,
                "insert does not accept unions",
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
    async fn case_and_window_columns() {
        let b = seeded_bridge_2t().await;
        // case：id=1 → "one"，否则 "other"（a: id=1 x, id=2 y → 按序 one/other）
        let cap = b
            .run(
                r#"db.table("a").select(["name",
        {case:{when:[{cond:{field:"id",op:"eq",value:1},then:"one"}],else:"other"},as:"tag"}
      ]).orderBy([{field:"id",dir:"asc"}]).all()
      .then(r=>json.ok({tags:r.map(x=>x.tag)})).catch(e=>json.fail(400,String(e)));"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["data"]["tags"], json!(["one", "other"]), "{v}");
        // window：row_number over (order by id desc) → y=1, x=2
        let cap = b
            .run(
                r#"db.table("a").select(["name",
        {window:{fn:"row_number",order_by:[{field:"id",dir:"desc"}]},as:"rn"}
      ]).all()
      .then(r=>json.ok({rows:r})).catch(e=>json.fail(400,String(e)));"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        let rows = v["data"]["rows"].as_array().unwrap();
        let rn_of = |n: &str| {
            rows.iter().find(|r| r["name"] == n).unwrap()["rn"]
                .as_i64()
                .unwrap()
        };
        assert_eq!(rn_of("y"), 1, "{v}");
        assert_eq!(rn_of("x"), 2, "{v}");
        // partition_by：b 表按 aid 分区编号
        let cap = b
            .run(
                r#"db.table("b").select(["label",
        {window:{fn:"rank",partition_by:["aid"],order_by:[{field:"id",dir:"asc"}]},as:"rk"}
      ]).all()
      .then(r=>json.ok({rows:r})).catch(e=>json.fail(400,String(e)));"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn case_window_rejections() {
        let b = seeded_bridge_2t().await;
        for (js, want) in [
            // 别名形状非法（brief 负向用例把 as 误写进 case 内部，按权威形状
            // {case:{when},as} 调整——与正向用例一致）
            (
                r#"db.table("a").select([{case:{when:[{cond:{field:"id",op:"eq",value:1},then:1}]},as:"0bad"}]).all()"#,
                "illegal alias",
            ),
            // window fn 非枚举值
            (
                r#"db.table("a").select([{window:{fn:"ntile",order_by:[]},as:"x"}]).all()"#,
                "unknown variant",
            ),
            // having 引用 window 别名（case/window 别名不进 having 台账）
            (
                r#"db.table("a").select([{window:{fn:"row_number"},as:"rn"}]).groupBy(["id"])
             .having({field:"rn",op:"eq",value:1}).all()"#,
                "unknown column",
            ),
            // 空 when（.run 为真值使 && 链求值到 select——不触达 insert 语义；
            // brief 同样把 as 误写进 case 内部，按权威形状调整）
            (
                r#"db.table("a").insert({name:"z"}).run && db.table("a").select([{case:{when:[]},as:"x"}]).all()"#,
                "case needs non-empty when",
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

    /// 统一审查 F-2/F-3：嵌套 offset 必须与 limit 成对；CASE when 数上限。
    #[tokio::test(flavor = "current_thread")]
    async fn nested_offset_and_case_when_caps() {
        let b = seeded_bridge_2t().await;
        for (js, want) in [
            // 嵌套 offset 无 limit：原渲染裸 OFFSET（DB 语法错），现校验期拒绝
            (
                r#"db.table("a").select(["name"]).where({field:"id",op:"in",
             subquery:db.table("b").select(["aid"]).offset(1)}).all()"#,
                "nested select offset requires limit",
            ),
            // 成对出现则合法（正向对照）
            (
                r#"db.table("a").select(["name"]).where({field:"id",op:"in",
             subquery:db.table("b").select(["aid"]).limit(1).offset(1)}).all()"#,
                "CODE0",
            ),
            // CASE when 数 17 > 16
            (
                r#"db.table("a").select([{case:{when:[
               {cond:{field:"id",op:"eq",value:1},then:1},{cond:{field:"id",op:"eq",value:2},then:2},
               {cond:{field:"id",op:"eq",value:3},then:3},{cond:{field:"id",op:"eq",value:4},then:4},
               {cond:{field:"id",op:"eq",value:5},then:5},{cond:{field:"id",op:"eq",value:6},then:6},
               {cond:{field:"id",op:"eq",value:7},then:7},{cond:{field:"id",op:"eq",value:8},then:8},
               {cond:{field:"id",op:"eq",value:9},then:9},{cond:{field:"id",op:"eq",value:10},then:10},
               {cond:{field:"id",op:"eq",value:11},then:11},{cond:{field:"id",op:"eq",value:12},then:12},
               {cond:{field:"id",op:"eq",value:13},then:13},{cond:{field:"id",op:"eq",value:14},then:14},
               {cond:{field:"id",op:"eq",value:15},then:15},{cond:{field:"id",op:"eq",value:16},then:16},
               {cond:{field:"id",op:"eq",value:17},then:17}],else:0},as:"x"}]).all()"#,
                "case when count exceeds 16",
            ),
        ] {
            let cap = b
                .run(&format!(
                    r#"{js}.then(()=>json.ok({{}})).catch(e=>json.fail(400,String(e)));"#
                ))
                .await
                .unwrap();
            let v: Value = serde_json::from_slice(&cap.body).unwrap();
            if want == "CODE0" {
                assert_eq!(v["code"], 0, "{v}");
            } else {
                assert!(v["msg"].as_str().unwrap().contains(want), "{want}: {v}");
            }
        }
    }

    /// 统一审查 F-4：值含引号/反斜杠/换行/制表符经绑定参数往返无损。
    #[tokio::test(flavor = "current_thread")]
    async fn special_char_values_round_trip() {
        let b = seeded_bridge().await;
        let cap = b
            .run(
                r#"const v = "o'brien\\x\n\ty\"z";
             db.table("t").insert({name:v}).run()
               .then(() => db.table("t").where({field:"name",op:"eq",value:v}).all())
               .then((rows) => json.ok({hit: rows.length === 1 && rows[0].name === v}))
               .catch((e) => json.fail(400, String(e)));"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["data"]["hit"], json!(true), "{v}");
    }

    /// 动词矩阵：columns 仅 select（case/window 是 columns 元素级能力，DML 一并拦）。
    #[test]
    fn verb_matrix_rejects_columns_on_dml() {
        let bad = |req: &str| validate_verb(&serde_json::from_str(req).unwrap()).unwrap_err();
        assert!(
            bad(r#"{"table":"t","verb":"insert","values":[{"a":1}],"columns":["a"]}"#)
                .to_string()
                .contains("insert does not accept columns")
        );
        assert!(bad(
            r#"{"table":"t","verb":"update","sets":{"a":1},"conditions":[{"field":"a","op":"eq","value":1}],"columns":["a"]}"#
        )
        .to_string()
        .contains("update/delete does not accept columns"));
        assert!(bad(
            r#"{"table":"t","verb":"delete","conditions":[{"field":"a","op":"eq","value":1}],"columns":["a"]}"#
        )
        .to_string()
        .contains("update/delete does not accept columns"));
    }

    /// returning：仅 insert 接受；pg/sqlite 渲染 RETURNING，mysql 不渲染（两步取 id）。
    #[test]
    fn returning_verb_matrix_and_dialect_rendering() {
        let reg = SchemaRegistry::new().table("t", &["id"], &["id", "name"]);
        let sql_of = |req: &str, d: Dialect| {
            let req: QueryReq = serde_json::from_str(req).unwrap();
            build_statement(&req, &reg, d).map(|(s, _)| s)
        };
        let ins = r#"{"table":"t","verb":"insert","values":[{"name":"a"}],"returning":["id"]}"#;
        assert!(sql_of(ins, Dialect::Sqlite).unwrap().contains("RETURNING"));
        assert!(
            sql_of(ins, Dialect::Postgres)
                .unwrap()
                .contains("RETURNING")
        );
        // mysql 无 RETURNING：由 op 侧两条语句取 LAST_INSERT_ID()。
        assert!(!sql_of(ins, Dialect::MySql).unwrap().contains("RETURNING"));
        // 未知列 / 限定名（白名单按素名查，自然被拒）
        let bad = r#"{"table":"t","verb":"insert","values":[{"name":"a"}],"returning":["nope"]}"#;
        assert!(
            sql_of(bad, Dialect::Sqlite)
                .unwrap_err()
                .to_string()
                .contains("unknown column 'nope' in insert returning")
        );
        // mysql 多列 → 拒
        let bad =
            r#"{"table":"t","verb":"insert","values":[{"name":"a"}],"returning":["id","name"]}"#;
        assert!(
            sql_of(bad, Dialect::MySql)
                .unwrap_err()
                .to_string()
                .contains("exactly one column")
        );
        // 动词矩阵：select / update / delete 一律拒
        assert!(
            sql_of(r#"{"table":"t","returning":["id"]}"#, Dialect::Sqlite)
                .unwrap_err()
                .to_string()
                .contains("select does not accept returning")
        );
        let upd = r#"{"table":"t","verb":"update","sets":{"name":"x"},
            "conditions":[{"field":"id","op":"eq","value":1}],"returning":["id"]}"#;
        assert!(
            sql_of(upd, Dialect::Sqlite)
                .unwrap_err()
                .to_string()
                .contains("update/delete does not accept returning")
        );
    }

    /// insert + returning：池路径与事务路径都能取回自增 id；事务回滚后不可见。
    #[tokio::test(flavor = "current_thread")]
    async fn insert_returning_id_in_pool_and_tx() {
        let b = seeded_bridge().await;
        // 池路径：RETURNING 单语句取回 id（sqlite）
        let cap = b
            .run(
                r#"db.table("t").insert({name:"r1",age:1}).returning(["id"]).run()
                   .then(r => json.ok({rows:r})).catch(e => json.fail(500,String(e)));"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert!(v["data"]["rows"][0]["id"].as_i64().unwrap() > 0, "{v}");
        // 事务路径：同一会话取 id；throw 回滚后该行不可见
        let cap = b
            .run(
                r#"(async () => {
                     const id = await db.tx(async (tx) => {
                       const r = await tx.table("t").insert({name:"r2",age:2}).returning(["id"]).run();
                       return r[0].id;
                     });
                     let gone = null;
                     try {
                       await db.tx(async (tx) => {
                         await tx.table("t").insert({name:"r3",age:3}).returning(["id"]).run();
                         throw new Error("boom");
                       });
                     } catch (e) { gone = String(e); }
                     const left = await db.table("t").select(["name"])
                       .where({field:"name",op:"in",value:["r2","r3"]}).all();
                     json.ok({id, gone, names: left.map(x => x.name)});
                   })().catch(e => json.fail(500,String(e)));"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert!(v["data"]["id"].as_i64().unwrap() > 0, "{v}");
        assert_eq!(v["data"]["gone"], json!("Error: boom"), "{v}");
        assert_eq!(v["data"]["names"], json!(["r2"]), "{v}"); // r3 已回滚
    }

    /// CTE：主表 = CTE 与 join CTE（声明列即白名单；真实执行验证）。
    #[tokio::test(flavor = "current_thread")]
    async fn cte_main_and_join() {
        let b = seeded_bridge_2t().await;
        // 主表 = CTE：r(aid,label) = b 中 aid=1 → 2 行
        let cap = b
            .run(
                r#"db.table("r").with("r", ["aid","label"],
        db.table("b").select(["aid","label"]).where({field:"aid",op:"eq",value:1}))
      .select(["aid","label"]).orderBy([{field:"aid",dir:"asc"}]).all()
      .then(r=>json.ok({n:r.length,first:r[0].label})).catch(e=>json.fail(400,String(e)));"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["data"]["n"], 2, "{v}");
        assert_eq!(v["data"]["first"], "L1", "{v}");
        // join CTE：a ⋈ r on a.id = r.aid → x × {L1,L2} = 2 行
        let cap = b
            .run(
                r#"db.table("a").with("r", ["aid","label"],
        db.table("b").select(["aid","label"]).where({field:"aid",op:"eq",value:1}))
      .join("r", [{left:"a.id",right:"r.aid"}]).select(["a.name","r.label"]).all()
      .then(r=>json.ok({n:r.length})).catch(e=>json.fail(400,String(e)));"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["data"]["n"], 2, "{v}");
    }

    /// CTE 负向：别名形状 / 空 columns / 未声明列 / 嵌套带 with / DML 带 with。
    #[tokio::test(flavor = "current_thread")]
    async fn cte_rejections() {
        let b = seeded_bridge_2t().await;
        for (js, want) in [
            // name 形状非法（既有 check_alias 锁定文案为 illegal alias，brief 写 invalid）
            (
                r#"db.table("r").with("0bad", ["aid"], db.table("b").select(["aid"])).select(["aid"]).all()"#,
                "illegal alias",
            ),
            // columns 空
            (
                r#"db.table("r").with("r", [], db.table("b").select(["aid"])).select(["aid"]).all()"#,
                "cte needs non-empty columns",
            ),
            // 引用未声明的 CTE 列
            (
                r#"db.table("r").with("r", ["aid"], db.table("b").select(["aid"])).select(["r.nope"]).all()"#,
                "unknown column",
            ),
            // 嵌套 req 带 with
            (
                r#"db.table("a").select(["id"]).where({field:"id",op:"in",
                 subquery:{table:"b",columns:["aid"],with:[{name:"x",columns:["aid"],
                   query:{table:"b",columns:["aid"]}}]}}).all()"#,
                "nested select does not accept with/unions",
            ),
            // insert 带 with（动词矩阵）
            (
                r#"db.table("a").insert({name:"z"}).with("r",["aid"],db.table("b").select(["aid"])).run()"#,
                "insert does not accept with",
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

    // ----- 多租户 sql_guard：apply_tenant 注入（Task 3） -----

    use crate::bridge::{Extras, RequestInfo, SqlGuard};

    /// 租户夹具：t（受约束，tenant_id 列）+ s（共享表）+ 两租户种子。
    async fn guarded_bridge() -> Bridge {
        let db = SqlxAccessor::arc("sqlite::memory:").await.unwrap();
        db.exec_with_params(
            "create table t (id integer primary key, name text, tenant_id text)",
            &[],
        )
        .await
        .unwrap();
        db.exec_with_params("create table s (id integer primary key, name text)", &[])
            .await
            .unwrap();
        db.exec_with_params(
            "create table u (id integer primary key, label text, tenant_id text)",
            &[],
        )
        .await
        .unwrap();
        for (n, tid) in [("a1", "t1"), ("a2", "t1"), ("b1", "t2")] {
            db.exec_with_params(
                "insert into t (name, tenant_id) values (?, ?)",
                &[json!(n), json!(tid)],
            )
            .await
            .unwrap();
        }
        // u：id=1 属 t1，id=3 属 t2（join 语义验证用）。
        for (id, l, tid) in [(1, "ua", "t1"), (3, "ub", "t2")] {
            db.exec_with_params(
                "insert into u (id, label, tenant_id) values (?, ?, ?)",
                &[json!(id), json!(l), json!(tid)],
            )
            .await
            .unwrap();
        }
        let reg = SchemaRegistry::new()
            .table("t", &["id"], &["id", "name", "tenant_id"])
            .table("u", &["id"], &["id", "label", "tenant_id"])
            .table_owned_shared("m", "s", &["id"], &["id", "name"], true);
        Bridge::with_dbs_and_loader(
            std::collections::HashMap::from([("default".to_string(), db as _)]),
            Arc::new(InMemoryKV::new()),
            reg,
            false,
            None,
            Extras {
                sql_guard: SqlGuard::Deny,
                ..Default::default()
            },
        )
    }

    fn req_t1() -> RequestInfo {
        RequestInfo {
            tenant_id: Some("t1".into()),
            ..Default::default()
        }
    }

    /// 数值型 `tenant_id` 列的夹具（v0.1.24）：`n`（integer 租户列）+ `n2`（join 用）。
    async fn numeric_tenant_bridge() -> Bridge {
        let db = SqlxAccessor::arc("sqlite::memory:").await.unwrap();
        for ddl in [
            "create table n (id integer primary key, name text, tenant_id integer)",
            "create table n2 (id integer primary key, label text, tenant_id integer)",
        ] {
            db.exec_with_params(ddl, &[]).await.unwrap();
        }
        for (n, tid) in [("n7", 7), ("n8", 8)] {
            db.exec_with_params(
                "insert into n (name, tenant_id) values (?, ?)",
                &[json!(n), json!(tid)],
            )
            .await
            .unwrap();
        }
        db.exec_with_params(
            "insert into n2 (id, label, tenant_id) values (?, ?, ?)",
            &[json!(1), json!("o7"), json!(7)],
        )
        .await
        .unwrap();
        let reg = SchemaRegistry::new()
            .table_owned_shared_typed(
                "m",
                "n",
                &["id"],
                &[
                    ("id", super::super::registry::ColumnType::Integer),
                    ("name", super::super::registry::ColumnType::Text),
                    ("tenant_id", super::super::registry::ColumnType::Integer),
                ],
                false,
            )
            .table_owned_shared_typed(
                "m",
                "n2",
                &["id"],
                &[
                    ("id", super::super::registry::ColumnType::Integer),
                    ("label", super::super::registry::ColumnType::Text),
                    ("tenant_id", super::super::registry::ColumnType::Integer),
                ],
                false,
            );
        Bridge::with_dbs_and_loader(
            std::collections::HashMap::from([("default".to_string(), db as _)]),
            Arc::new(InMemoryKV::new()),
            reg,
            false,
            None,
            Extras {
                sql_guard: SqlGuard::Deny,
                ..Default::default()
            },
        )
    }

    fn req_tenant(id: &str) -> RequestInfo {
        RequestInfo {
            tenant_id: Some(id.into()),
            ..Default::default()
        }
    }

    /// 数值型 `tenant_id`（v0.1.24）：注入条件 / insert 强制写 / update 拒绝 / join ON
    /// 全部按**列类型**走数值，且写成「等值四形态」判定（字符串与数字不再二选一）。
    #[tokio::test(flavor = "current_thread")]
    async fn numeric_tenant_column_binds_numbers_end_to_end() {
        let b = numeric_tenant_bridge().await;
        let cap = b
            .run_with(
                r#"(async () => {
                     const rows = await db.table("n").select(["name"]).all();
                     const s = db.table("n").select(["name"]).toSQL();
                     const j = db.table("n").select(["name"])
                       .join("n2", [{left:"tenant_id",right:"tenant_id"}]).toSQL();
                     await db.table("n").insert({ name: "n9" }).run();
                     const after = await db.table("n").select(["name"]).orderBy([{field:"id",dir:"asc"}]).all();
                     const mism = await db.table("n").insert({ name: "bad", tenant_id: 8 })
                       .run().then(() => "ok", e => String(e));
                     const upd = await db.table("n").update({ tenant_id: 8 })
                       .where({field:"id",op:"eq",value:1}).run().then(() => "ok", e => String(e));
                     json.ok({
                       rows: rows.map(r => r.name),
                       params: s.params, joinParams: j.params,
                       after: after.map(r => r.name), mism, upd,
                     });
                   })().catch(e => json.fail(500, String(e)));"#,
                req_tenant("7"),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(v["data"]["rows"], json!(["n7"]), "{v}");
        // 注入的是**数值** 7（不是字符串 "7"）——这正是 PG 数值列不再报 bigint = text 的原因。
        assert!(
            v["data"]["params"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p == &json!(7)),
            "{v}"
        );
        // join 的 ON 条件同型（数值）。
        assert!(
            v["data"]["joinParams"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p == &json!(7)),
            "{v}"
        );
        // insert 未给 tenant_id → 强制写数值 7；给了别的值 → 四形态等值判定拦下。
        assert_eq!(v["data"]["after"], json!(["n7", "n9"]), "{v}");
        assert!(
            v["data"]["mism"].as_str().unwrap().contains("mismatch"),
            "{v}"
        );
        assert!(
            v["data"]["upd"].as_str().unwrap().contains("not allowed"),
            "{v}"
        );
    }

    /// 数值型租户列 + 非十进制租户头 → 明确报错（而不是把 "acme" 绑给 integer 列，
    /// 让 PG 回一句看不出根因的 `invalid input syntax for type integer`）。
    #[tokio::test(flavor = "current_thread")]
    async fn numeric_tenant_column_rejects_non_numeric_tid() {
        let b = numeric_tenant_bridge().await;
        let cap = b
            .run_with(
                r#"(async () => {
                     const r = await db.table("n").select(["name"]).all()
                       .then(() => "ok", e => String(e));
                     json.ok({ r });
                   })().catch(e => json.fail(500, String(e)));"#,
                req_tenant("acme"),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        let msg = v["data"]["r"].as_str().unwrap();
        assert!(msg.contains("not a valid integer"), "{msg}");
        assert!(msg.contains("n.tenant_id"), "{msg}");
    }

    /// select：构造器查询自动收窄到当前租户；toSQL 产物含注入条件与参数。
    #[tokio::test(flavor = "current_thread")]
    async fn tenant_select_scoped() {
        let b = guarded_bridge().await;
        let cap = b
            .run_with(
                r#"(async () => {
                     const rows = await db.table("t").select(["name"]).orderBy([{field:"id",dir:"asc"}]).all();
                     const s = db.table("t").select(["name"]).toSQL();
                     json.ok({ names: rows.map(r => r.name), sql: s.sql, params: s.params });
                   })().catch(e => json.fail(500, String(e)));"#,
                req_t1(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(v["data"]["names"], json!(["a1", "a2"]), "{v}");
        assert!(
            v["data"]["sql"].as_str().unwrap().contains("tenant_id"),
            "{v}"
        );
        assert!(
            v["data"]["params"]
                .as_array()
                .unwrap()
                .contains(&json!("t1")),
            "{v}"
        );
    }

    /// asTenant 夹具：与 guarded_bridge 同数据，但开启 `tenant.allow_as_tenant`。
    async fn guarded_bridge_allow_as_tenant(allow: bool) -> Bridge {
        let db = SqlxAccessor::arc("sqlite::memory:").await.unwrap();
        db.exec_with_params(
            "create table t (id integer primary key, name text, tenant_id text)",
            &[],
        )
        .await
        .unwrap();
        for (n, tid) in [("a1", "t1"), ("b1", "t2")] {
            db.exec_with_params(
                "insert into t (name, tenant_id) values (?, ?)",
                &[json!(n), json!(tid)],
            )
            .await
            .unwrap();
        }
        let reg = SchemaRegistry::new().table("t", &["id"], &["id", "name", "tenant_id"]);
        Bridge::with_dbs_and_loader(
            std::collections::HashMap::from([("default".to_string(), db as _)]),
            Arc::new(InMemoryKV::new()),
            reg,
            false,
            None,
            Extras {
                sql_guard: SqlGuard::Deny,
                allow_as_tenant: allow,
                ..Default::default()
            },
        )
    }

    fn req_anonymous() -> RequestInfo {
        RequestInfo {
            anonymous: true,
            ..Default::default()
        }
    }

    /// asTenant：匿名请求声明租户后，构造器照常注入该租户条件（仍强制，不是绕过）。
    #[tokio::test(flavor = "current_thread")]
    async fn as_tenant_scopes_query_on_anonymous_request() {
        let b = guarded_bridge_allow_as_tenant(true).await;
        let cap = b
            .run_with(
                r#"(async () => {
                     const d = db.asTenant("t2");
                     const rows = await d.table("t").select(["name"]).all();
                     const s = d.table("t").select(["name"]).toSQL();
                     json.ok({ names: rows.map(r => r.name), sql: s.sql, params: s.params });
                   })().catch(e => json.fail(500, String(e)));"#,
                req_anonymous(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(v["data"]["names"], json!(["b1"]), "{v}");
        assert!(
            v["data"]["sql"].as_str().unwrap().contains("tenant_id"),
            "{v}"
        );
        assert!(
            v["data"]["params"]
                .as_array()
                .unwrap()
                .contains(&json!("t2")),
            "{v}"
        );
    }

    /// asTenant 三道门禁：非匿名请求、已带租户头的请求、开关未开 —— 一律拒绝。
    #[tokio::test(flavor = "current_thread")]
    async fn as_tenant_gates() {
        // 同步抛出（未进 Promise 链）⇒ 一律用 async IIFE 兜住，否则断言看到的是 CoreError。
        let js = r#"(async () => {
                     await db.asTenant("t2").table("t").select(["name"]).all();
                     json.ok({});
                   })().catch(e => json.fail(400, String(e)));"#;
        // ① 非匿名（无租户头但也没命中豁免，如 WS/任务/测试默认路径）
        let b = guarded_bridge_allow_as_tenant(true).await;
        let v: Value =
            serde_json::from_slice(&b.run_with(js, RequestInfo::default()).await.unwrap().body)
                .unwrap();
        assert_eq!(v["code"], 400, "{v}");
        assert!(
            v["msg"]
                .as_str()
                .unwrap()
                .contains("only allowed on anonymous"),
            "{v}"
        );
        // ② 已带租户头（防运行期切换身份）
        let v: Value =
            serde_json::from_slice(&b.run_with(js, req_t1()).await.unwrap().body).unwrap();
        assert_eq!(v["code"], 400, "{v}");
        assert!(
            v["msg"]
                .as_str()
                .unwrap()
                .contains("only allowed on anonymous"),
            "{v}"
        );
        // ③ 开关未开（默认 fail-closed）
        let b = guarded_bridge_allow_as_tenant(false).await;
        let v: Value =
            serde_json::from_slice(&b.run_with(js, req_anonymous()).await.unwrap().body).unwrap();
        assert_eq!(v["code"], 400, "{v}");
        assert!(v["msg"].as_str().unwrap().contains("is disabled"), "{v}");
        // ④ 空 id
        let v: Value = serde_json::from_slice(
            &b.run_with(
                r#"(async () => {
                     await db.asTenant("").table("t").select(["name"]).all();
                     json.ok({});
                   })().catch(e => json.fail(400, String(e)));"#,
                req_anonymous(),
            )
            .await
            .unwrap()
            .body,
        )
        .unwrap();
        assert!(
            v["msg"].as_str().unwrap().contains("must not be empty"),
            "{v}"
        );
    }

    /// asTenant 是请求级：下一次 run（ReqState::reset）后失效，匿名请求重回 deny。
    #[tokio::test(flavor = "current_thread")]
    async fn as_tenant_is_request_scoped() {
        let b = guarded_bridge_allow_as_tenant(true).await;
        let v: Value = serde_json::from_slice(
            &b.run_with(
                r#"(async () => {
                     await db.asTenant("t2").table("t").select(["name"]).all();
                     json.ok({});
                   })().catch(e => json.fail(400, String(e)));"#,
                req_anonymous(),
            )
            .await
            .unwrap()
            .body,
        )
        .unwrap();
        assert_eq!(v["code"], 0, "{v}");
        // reset 后不再有租户身份 → 受约束表被拒
        let v: Value = serde_json::from_slice(
            &b.run_with(
                r#"(async () => {
                     await db.table("t").select(["name"]).all();
                     json.ok({});
                   })().catch(e => json.fail(400, String(e)));"#,
                req_anonymous(),
            )
            .await
            .unwrap()
            .body,
        )
        .unwrap();
        assert_eq!(v["code"], 400, "{v}");
        assert!(
            v["msg"]
                .as_str()
                .unwrap()
                .contains("require tenant context"),
            "{v}"
        );
    }

    /// insert：强制写入当前租户；显式传不符 tenant_id 报错；显式传相符放行。
    #[tokio::test(flavor = "current_thread")]
    async fn tenant_insert_forces_tid_and_rejects_mismatch() {
        let b = guarded_bridge().await;
        let cap = b
            .run_with(
                r#"db.table("t").insert({name:"a3"}).run()
                     .then(() => json.ok({})).catch(e => json.fail(400, String(e)));"#,
                req_t1(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        // 强制写入生效：新行带 t1
        let cap = b
            .run_with(
                r#"db.table("t").select(["name","tenant_id"]).where({field:"name",op:"eq",value:"a3"}).all()
                     .then(r => json.ok({n: r.length, tid: r[0].tenant_id}))
                     .catch(e => json.fail(500, String(e)));"#,
                req_t1(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(
            (&v["data"]["n"], &v["data"]["tid"]),
            (&json!(1), &json!("t1")),
            "{v}"
        );
        // 显式不符 → 报错
        let cap = b
            .run_with(
                r#"db.table("t").insert({name:"x", tenant_id:"t2"}).run()
                     .then(() => json.ok({})).catch(e => json.fail(400, String(e)));"#,
                req_t1(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 400, "{v}");
        assert!(
            v["msg"].as_str().unwrap().contains("tenant_id mismatch"),
            "{v}"
        );
    }

    /// update：sets.tenant_id 写侧逃逸一律报错；where 自动收窄（改不到他租户行）。
    #[tokio::test(flavor = "current_thread")]
    async fn tenant_update_sets_tid_rejected_and_where_narrowed() {
        let b = guarded_bridge().await;
        let cap = b
            .run_with(
                r#"db.table("t").update({name:"zz"}).where({field:"name",op:"eq",value:"b1"}).run()
                     .then(n => json.ok({n})).catch(e => json.fail(500, String(e)));"#,
                req_t1(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(v["data"]["n"], 0, "t1 租户不应改到 t2 的行: {v}");
        // sets.tenant_id → 报错
        let cap = b
            .run_with(
                r#"(async () => db.table("t").update({tenant_id:"t2"}).where({field:"name",op:"eq",value:"a1"}).run())()
                     .then(() => json.ok({})).catch(e => json.fail(400, String(e)));"#,
                req_t1(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 400, "{v}");
        assert!(
            v["msg"]
                .as_str()
                .unwrap()
                .contains("sets.tenant_id not allowed"),
            "{v}"
        );
    }

    /// delete：自动收窄——t1 删不到 t2 的行。
    #[tokio::test(flavor = "current_thread")]
    async fn tenant_delete_narrowed() {
        let b = guarded_bridge().await;
        let cap = b
            .run_with(
                r#"db.table("t").delete().where({field:"name",op:"eq",value:"b1"}).run()
                     .then(n => json.ok({n})).catch(e => json.fail(500, String(e)));"#,
                req_t1(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!((&v["code"], &v["data"]["n"]), (&json!(0), &json!(0)), "{v}");
    }

    /// join：join 表同样受约束，条件进 ON（LEFT JOIN 他租户行出现 NULL，不变 INNER）。
    #[tokio::test(flavor = "current_thread")]
    async fn tenant_join_injects_on_clause() {
        let b = guarded_bridge().await;
        // LEFT JOIN：t1 基表 {a1(id1), a2(id2)}；u 行 id1=t1(ua) / id3=t2(ub)。
        // ON 注入 u.tenant_id='t1' → a2 的 label 为 NULL（LEFT 语义保留）。
        let cap = b
            .run_with(
                r#"db.table("t").join("u", [{left:"t.id",right:"u.id"}], "left")
                     .select(["t.name","u.label"]).orderBy([{field:"t.id",dir:"asc"}]).all()
                     .then(r => json.ok({rows: r})).catch(e => json.fail(500, String(e)));"#,
                req_t1(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(
            v["data"]["rows"],
            json!([{"name": "a1", "label": "ua"}, {"name": "a2", "label": null}]),
            "{v}"
        );
        // toSQL 层面：条件在 ON 区段而非 WHERE。
        let cap = b
            .run_with(
                r#"json.ok(db.table("t").join("u", [{left:"t.id",right:"u.id"}], "left").select(["t.name"]).toSQL());"#,
                req_t1(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        let sql = v["data"]["sql"].as_str().unwrap().to_string();
        let on_part = sql.split("WHERE").next().unwrap_or("");
        assert!(on_part.contains("tenant_id"), "ON 区段应含注入条件: {sql}");
    }

    /// fromJSON 投毒回归（评审 P0）：快照 join 里预设 tenant_id:"t2" 必须被无条件
    /// 覆盖为当前租户——fromJSON 绕过 JS 链层直喂 serde，预设值绝不可信。
    #[tokio::test(flavor = "current_thread")]
    async fn tenant_join_preset_tenant_id_overridden() {
        let b = guarded_bridge().await;
        let cap = b
            .run_with(
                r#"json.ok(db.fromJSON({
                     table: "t", verb: "select", columns: ["t.name"],
                     joins: [{ table: "u", kind: "left",
                               on: [{left: "t.id", right: "u.id"}],
                               tenant_id: "t2" }],
                     order_by: [{field: "t.id", dir: "asc"}] }).toSQL());"#,
                req_t1(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        // 基表 + join 两处绑定都必须是当前租户 t1（尾随 limit 参数不计）；"t2" 不得出现。
        assert_eq!(
            v["data"]["params"].as_array().unwrap().first(),
            Some(&json!("t1")),
            "{v}"
        );
        assert_eq!(
            v["data"]["params"].as_array().unwrap().get(1),
            Some(&json!("t1")),
            "{v}"
        );
        assert!(
            !v["data"]["sql"].as_str().unwrap().contains("t2"),
            "他租户值泄漏进 SQL: {v}"
        );
    }

    /// 子查询递归：in-subquery 同样收窄到当前租户。
    #[tokio::test(flavor = "current_thread")]
    async fn tenant_subquery_recursion() {
        let b = guarded_bridge().await;
        // 子查询查 t2 租户的行名 b1——注入后子查询只回 t1 行 → 外层 in 空集。
        let cap = b
            .run_with(
                r#"db.table("t").select(["name"])
                     .where({field:"name",op:"in",subquery:db.table("t").select(["name"]).where({field:"tenant_id",op:"eq",value:"t2"})})
                     .all()
                     .then(r => json.ok({n: r.length})).catch(e => json.fail(500, String(e)));"#,
                req_t1(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(v["data"]["n"], 0, "子查询应被收窄到 t1，t2 行不可见: {v}");
    }

    /// tid=None：Deny 拒绝；asSystem 逃生口放行；Warn 模式放行。
    #[tokio::test(flavor = "current_thread")]
    async fn tenant_none_denied_and_as_system_escapes() {
        let b = guarded_bridge().await;
        // Deny + None → 拒
        let cap = b
            .run(
                r#"db.table("t").select(["name"]).all()
                     .then(r => json.ok({n: r.length})).catch(e => json.fail(400, String(e)));"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 400, "{v}");
        assert!(
            v["msg"]
                .as_str()
                .unwrap()
                .contains("require tenant context"),
            "{v}"
        );
        // asSystem → 放行（看到全部 3 行）
        let cap = b
            .run(
                r#"db.asSystem().table("t").select(["name"]).all()
                     .then(r => json.ok({n: r.length})).catch(e => json.fail(400, String(e)));"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!((&v["code"], &v["data"]["n"]), (&json!(0), &json!(3)), "{v}");
        // 共享表不受约束：None 也放行
        let cap = b
            .run(
                r#"db.table("s").select(["name"]).all()
                     .then(r => json.ok({n: r.length})).catch(e => json.fail(400, String(e)));"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
    }

    /// 共享表（TableDef.shared=true）：有 tenant_id 列也不注入。
    #[tokio::test(flavor = "current_thread")]
    async fn tenant_shared_table_untouched() {
        let b = guarded_bridge().await;
        let cap = b
            .run_with(
                r#"json.ok(db.table("s").select(["name"]).toSQL());"#,
                req_t1(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(
            !v["data"]["sql"].as_str().unwrap().contains("tenant_id"),
            "{v}"
        );
    }

    /// 租户防护对事务同样生效：注入发生在 resolve_target 之前，与走池/走会话正交。
    /// 事务内 select 收窄到当前租户、insert 强制写当前租户（含 returning）、
    /// update 改不到他租户的行、回滚不留痕。
    #[tokio::test(flavor = "current_thread")]
    async fn tenant_guard_applies_inside_tx() {
        let b = guarded_bridge().await;
        let cap = b
            .run_with(
                r#"(async () => {
                     const out = await db.tx(async (tx) => {
                       const rows = await tx.table("t").select(["name"]).orderBy([{field:"id",dir:"asc"}]).all();
                       const r = await tx.table("t").insert({name:"a3"}).returning(["id"]).run();
                       return { names: rows.map(x => x.name), id: r[0].id };
                     });
                     const after = await db.table("t").select(["name","tenant_id"])
                       .where({field:"name",op:"eq",value:"a3"}).all();
                     json.ok({ out, tid: after.length ? after[0].tenant_id : null });
                   })().catch(e => json.fail(500,String(e)));"#,
                req_t1(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        // t2 的 b1 在事务内同样不可见
        assert_eq!(v["data"]["out"]["names"], json!(["a1", "a2"]), "{v}");
        assert_eq!(v["data"]["tid"], json!("t1"), "insert 未注入租户: {v}");
        assert!(v["data"]["out"]["id"].as_i64().unwrap() > 0, "{v}");
        // 事务内 update 收窄 + 回滚不留痕
        let cap = b
            .run_with(
                r#"(async () => {
                     await db.tx(async (tx) => {
                       const n = await tx.table("t").update({name:"zz"}).where({field:"name",op:"eq",value:"b1"}).run();
                       if (n !== 0) throw new Error("leaked to other tenant");
                       await tx.table("t").insert({name:"gone"}).run();
                       throw new Error("boom");
                     }).catch(() => {});
                     const left = await db.table("t").select(["name"])
                       .where({field:"name",op:"eq",value:"gone"}).all();
                     const b1 = await db.asSystem().table("t").select(["name"])
                       .where({field:"name",op:"eq",value:"b1"}).all();
                     json.ok({ gone: left.length, b1: b1[0].name });
                   })().catch(e => json.fail(500,String(e)));"#,
                req_t1(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(
            (&v["data"]["gone"], &v["data"]["b1"]),
            (&json!(0), &json!("b1")),
            "{v}"
        );
    }

    /// 含 LIKE 元字符的夹具：% 与 _ 是数据，不是通配符。
    async fn seeded_bridge_like() -> Bridge {
        let db = SqlxAccessor::arc("sqlite::memory:").await.unwrap();
        db.exec_with_params("create table t (id integer primary key, name text)", &[])
            .await
            .unwrap();
        for n in ["a%b", "a_b", "abc", "xyz", "100%"] {
            db.exec_with_params("insert into t (name) values (?)", &[json!(n)])
                .await
                .unwrap();
        }
        let reg = SchemaRegistry::new().table("t", &["id"], &["id", "name"]);
        Bridge::with_opts(db, Arc::new(InMemoryKV::new()), reg, false)
    }

    /// `cond` 下按 id 升序取回的 name 列表。
    async fn names_where(b: &Bridge, cond: &str) -> Value {
        let cap = b
            .run(&format!(
                r#"db.table("t").select(["name"]).where({cond}).orderBy([{{field:"id",dir:"asc"}}]).all()
                   .then(r => json.ok({{ names: r.map(x => x.name) }}))
                   .catch(e => json.fail(500, String(e)));"#
            ))
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "query failed: {v}");
        v["data"]["names"].clone()
    }

    /// 语义化模糊 op（v0.1.81）：值里的 % 与 _ 被当**字面量**，
    /// 旧 `like` 直接把 `%` 当用户输入的通配符 → 数据里的 % 会多命中。
    #[tokio::test(flavor = "current_thread")]
    async fn semantic_like_ops_treat_wildcards_as_literals() {
        let b = seeded_bridge_like().await;
        // 关键一条：contains("a%") 只命中字面含 "a%" 的 a%b——
        // 未转义时 "%" 会被当通配符，把 abc / a_b 也捞进来。
        assert_eq!(
            names_where(&b, r#"{field:"name",op:"contains",value:"a%"}"#).await,
            json!(["a%b"]),
        );
        assert_eq!(
            names_where(&b, r#"{field:"name",op:"contains",value:"_b"}"#).await,
            json!(["a_b"]),
        );
        assert_eq!(
            names_where(&b, r#"{field:"name",op:"startswith",value:"a"}"#).await,
            json!(["a%b", "a_b", "abc"]),
        );
        assert_eq!(
            names_where(&b, r#"{field:"name",op:"endswith",value:"b"}"#).await,
            json!(["a%b", "a_b"]),
        );
        // 数据里的 `%`：字面结尾是 % 的只有 100% 一条。
        // 未转义时 `%%` 等于「以任意串结尾」→ 全表 5 条都被捞进来。
        assert_eq!(
            names_where(&b, r#"{field:"name",op:"endswith",value:"%"}"#).await,
            json!(["100%"]),
        );
        // 对照：裸 like 仍按用户给的 pattern 走（% 是通配符）。
        assert_eq!(
            names_where(&b, r#"{field:"name",op:"like",value:"a%"}"#).await,
            json!(["a%b", "a_b", "abc"]),
        );
    }

    /// 模糊 op 的 pattern 与 ESCAPE 符都进 params（绑定参数），
    /// 与裸 like 一样不落进 SQL 文本——否则 `%` / `'` 就是拼接注入面。
    #[tokio::test(flavor = "current_thread")]
    async fn semantic_like_ops_bind_pattern_and_escape() {
        let b = seeded_bridge_like().await;
        let cap = b
            .run(
                r#"json.ok(db.table("t").select(["name"]).where({field:"name",op:"contains",value:"a%b"}).toSQL());"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        let sql = v["data"]["sql"].as_str().unwrap();
        let params = v["data"]["params"].as_array().unwrap();
        // 形状必须对：`col LIKE ? ESCAPE ?`（不是 `(col LIKE ?) ESCAPE ?`）。
        assert!(sql.contains("LIKE ? ESCAPE ?"), "{sql}");
        // 值本身绝不能出现在 SQL 文本里。
        assert!(!sql.contains("a%b"), "{sql}");
        assert_eq!(params[0], json!("%a\\%b%"), "{v}");
        assert_eq!(params[1], json!("\\"), "{v}");
    }

    /// escape 逃生舱：默认 `\` 与输入冲突时可换转义符（v0.1.81）。
    #[tokio::test(flavor = "current_thread")]
    async fn like_escape_char_is_overridable() {
        let b = seeded_bridge_like().await;
        let cap = b
            .run(
                r#"json.ok(db.table("t").select(["name"])
                     .where({field:"name",op:"contains",value:"a_b",escape:"!"}).toSQL());"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        let params = v["data"]["params"].as_array().unwrap();
        assert_eq!(params[0], json!("%a!_b%"), "{v}");
        assert_eq!(params[1], json!("!"), "{v}");
        // 裸 like 也认 escape：用户自带 pattern + 自定义转义符。
        let cap = b
            .run(
                r#"json.ok(db.table("t").select(["name"])
                     .where({field:"name",op:"like",value:"a!%b",escape:"!"}).toSQL());"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        let params = v["data"]["params"].as_array().unwrap();
        assert_eq!(params[0], json!("a!%b"), "{v}");
        assert_eq!(params[1], json!("!"), "{v}");
    }

    /// escape 必须是**单个**字符：多字符会静默截断成第一个，宁可报错。
    #[tokio::test(flavor = "current_thread")]
    async fn like_escape_rejects_non_single_char() {
        let b = seeded_bridge_like().await;
        for esc in [r#""""#, r#""ab""#] {
            let cap = b
                .run(&format!(
                    r#"db.table("t").select(["name"])
                       .where({{field:"name",op:"contains",value:"a",escape:{esc}}})
                       .all().then(() => json.ok({{}}))
                       .catch(e => json.fail(500, String(e)));"#
                ))
                .await
                .unwrap();
            let v: Value = serde_json::from_slice(&cap.body).unwrap();
            assert!(
                v["msg"]
                    .as_str()
                    .unwrap()
                    .contains("escape must be a single char"),
                "{esc}: {v}"
            );
        }
    }

    /// `in []` 必须报错：`is_in` 空集在 sea-query 里渲染成恒假 SQL，
    /// 静默返回 0 行会伪装成「没有匹配数据」（v0.1.81）。
    #[tokio::test(flavor = "current_thread")]
    async fn empty_in_is_rejected() {
        let b = seeded_bridge().await;
        let cap = b
            .run(
                r#"db.table("t").select(["name"]).where({field:"age",op:"in",value:[]})
                   .all().then(() => json.ok({}))
                   .catch(e => json.fail(500, String(e)));"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(
            v["msg"]
                .as_str()
                .unwrap()
                .contains("in needs non-empty array"),
            "{v}"
        );
    }

    /// `js` 是不含 `.then` 的链式片段，返回命中行数。
    async fn rows_of(b: &Bridge, js: &str) -> u64 {
        let cap = b
            .run(&format!(
                r#"{js}.all().then(r => json.ok({{ n: r.length }})).catch(e => json.fail(500, String(e)));"#
            ))
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "query failed: {v}");
        v["data"]["n"].as_u64().unwrap()
    }

    /// `whereIf`：假值跳过、`0`/`""` 保留（`status=0` 是合法过滤值），
    /// 且空数组跳过（避免 `IN ()`）。
    #[tokio::test(flavor = "current_thread")]
    async fn where_if_skips_only_nullish_and_empty_array() {
        let b = seeded_bridge().await;
        let base = r#"db.table("t").select(["name"])"#;
        // 跳过：undefined / null / false / []
        for skip in ["undefined", "null", "false"] {
            let js = format!(r#"{base}.whereIf({skip},{{field:"age",op:"lt",value:0}})"#);
            assert_eq!(rows_of(&b, &js).await, 4, "{js}");
        }
        assert_eq!(
            rows_of(
                &b,
                &format!(r#"{base}.whereIf([],{{field:"id",op:"in",value:[]}})"#)
            )
            .await,
            4
        );
        // 保留：0 / "" 是真过滤值（`status=0` 是合法的业务值）
        assert_eq!(
            rows_of(
                &b,
                &format!(r#"{base}.whereIf(0,{{field:"ok",op:"eq",value:0}})"#)
            )
            .await,
            2
        );
        assert_eq!(
            rows_of(
                &b,
                &format!(r#"{base}.whereIf("",{{field:"tag",op:"isnull"}})"#)
            )
            .await,
            1
        );
        // 非空数组照常拼
        assert_eq!(
            rows_of(
                &b,
                &format!(r#"{base}.whereIf([1],{{field:"id",op:"in",value:[1]}})"#)
            )
            .await,
            1
        );
    }

    /// `select(["*"])`：展开 registry 全列；与显式列混用 / 与 join 混用一律拒绝。
    #[tokio::test(flavor = "current_thread")]
    async fn select_star_expands_columns_and_rejects_mixing() {
        let b = seeded_bridge().await;
        let cap = b
            .run(
                r#"db.table("t").select(["*"]).orderBy([{field:"id",dir:"asc"}]).all()
                   .then(r => json.ok({ first: r[0], keys: Object.keys(r[0]).sort() }))
                   .catch(e => json.fail(500, String(e)));"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(
            v["data"]["keys"],
            json!(["age", "id", "name", "ok", "tag"]),
            "{v}"
        );
        assert_eq!(v["data"]["first"]["name"], json!("a"), "{v}");
        for (js, want) in [
            (
                r#"db.table("t").select(["*","id"]).all()"#,
                "select '*' must be the only column",
            ),
            (
                r#"db.table("t").select(["id","*"]).all()"#,
                "select '*' must be the only column",
            ),
        ] {
            let cap = b
                .run(&format!(
                    r#"{js}.then(() => json.ok({{}})).catch(e => json.fail(500, String(e)));"#
                ))
                .await
                .unwrap();
            let v: Value = serde_json::from_slice(&cap.body).unwrap();
            assert!(v["msg"].as_str().unwrap().contains(want), "{want}: {v}");
        }
        // 与 join 混用：全列在两表同名列（id）上会歧义 → 直接拒绝。
        let b2 = seeded_bridge_2t().await;
        let cap = b2
            .run(
                r#"db.table("a").join("b",[{left:"a.id",right:"b.aid"}]).select(["*"]).all()
                   .then(() => json.ok({})).catch(e => json.fail(500, String(e)));"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(
            v["msg"]
                .as_str()
                .unwrap()
                .contains("select '*' is not allowed with joins"),
            "{v}"
        );
    }

    /// 表别名（v0.1.81）：FROM 渲染 `a AS x`，列限定用别名；
    /// **归属守卫与租户判定仍按真名**——别名混进真名字段会静默绕过这两道防线。
    #[tokio::test(flavor = "current_thread")]
    async fn table_alias_renders_as_and_keeps_guards_on_real_name() {
        let b = seeded_bridge_2t().await;
        let cap = b
            .run(r#"json.ok(db.table("a").as("x").select(["x.name"]).toSQL());"#)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        let sql = v["data"]["sql"].as_str().unwrap();
        assert!(sql.contains("FROM"), "{sql}");
        assert!(sql.contains("AS"), "{sql}");
        assert!(sql.contains("AS \"x\""), "{sql}");
        // 真名仍被认：别名 ≠ 归属守卫的钥匙，列白名单按真名 a 解析。
        let cap = b
            .run(
                r#"db.table("a").as("x").select(["x.name"]).orderBy([{field:"name",dir:"asc"}]).all()
                   .then(r => json.ok({ n: r.length, first: r[0].name }))
                   .catch(e => json.fail(500, String(e)));"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(
            (&v["code"], &v["data"]["n"], &v["data"]["first"]),
            (&json!(0), &json!(2), &json!("x")),
            "{v}"
        );
        // 自 join 靠别名区分：两侧真名都是 a，但 SQL 标识不同 → 放行。
        let cap = b
            .run(
                r#"json.ok(db.table("a").as("l").join("a",[{left:"l.id",right:"r.id"}],"inner","r")
                     .select(["l.name"]).toSQL());"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        let sql = v["data"]["sql"].as_str().unwrap();
        assert!(sql.contains("AS \"l\""), "{sql}");
        assert!(sql.contains("AS \"r\""), "{sql}");
    }

    /// 别名只属于 select：INSERT/UPDATE/DELETE 用别名会让列限定指向不存在的表。
    #[tokio::test(flavor = "current_thread")]
    async fn alias_rejected_on_write_verbs() {
        let b = seeded_bridge().await;
        for (js, want) in [
            (
                r#"db.table("t").as("x").update({name:"z"}).where({field:"id",op:"eq",value:1}).run()"#,
                "table alias is not supported on insert/update/delete",
            ),
            (
                r#"db.table("t").as("x").delete().where({field:"id",op:"eq",value:1}).run()"#,
                "table alias is not supported on insert/update/delete",
            ),
        ] {
            let cap = b
                .run(&format!(
                    r#"{js}.then(() => json.ok({{}})).catch(e => json.fail(500, String(e)));"#
                ))
                .await
                .unwrap();
            let v: Value = serde_json::from_slice(&cap.body).unwrap();
            assert!(v["msg"].as_str().unwrap().contains(want), "{want}: {v}");
        }
    }

    /// join ON 的「列 op 值」（v0.1.81）：值进 params，非比较类 op 直接拒绝。
    #[tokio::test(flavor = "current_thread")]
    async fn join_on_value_condition_binds_and_rejects_bad_ops() {
        let b = seeded_bridge_2t().await;
        let cap = b
            .run(
                r#"json.ok(db.table("a").join("b",[{left:"b.aid",right:"a.id"},{left:"b.aid",op:"eq",value:1}])
                     .select(["a.name"]).toSQL());"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        let sql = v["data"]["sql"].as_str().unwrap();
        assert!(sql.contains("JOIN"), "{sql}");
        assert!(sql.contains("= ?"), "{sql}");
        assert!(
            v["data"]["params"].as_array().unwrap().contains(&json!(1)),
            "{v}"
        );
        for (js, want) in [
            (
                r#"db.table("a").join("b",[{left:"b.aid",op:"contains",value:"x"}]).select(["a.name"]).all()"#,
                "join on 'b.aid' does not support op 'contains'",
            ),
            (
                r#"db.table("a").join("b",[{left:"b.aid",op:"isnull"}]).select(["a.name"]).all()"#,
                "join on 'b.aid' does not support op 'isnull'",
            ),
            (
                r#"db.table("a").join("b",[{left:"b.aid",right:"a.id",op:"eq"}]).select(["a.name"]).all()"#,
                "takes either right or op/value, not both",
            ),
        ] {
            let cap = b
                .run(&format!(
                    r#"{js}.then(() => json.ok({{}})).catch(e => json.fail(500, String(e)));"#
                ))
                .await
                .unwrap();
            let v: Value = serde_json::from_slice(&cap.body).unwrap();
            assert!(v["msg"].as_str().unwrap().contains(want), "{want}: {v}");
        }
    }

    /// `req` JSON 在三个方言下各自渲染（纯构造，不跑库）。
    fn sql_in(reg: &SchemaRegistry, req: &str, d: Dialect) -> Result<String, JsErrorBox> {
        let req: QueryReq = serde_json::from_str(req).unwrap();
        build_statement(&req, reg, d).map(|(s, _)| s)
    }

    /// 空白归一：sea-query 固定拼 " ON CONFLICT " + " DO NOTHING"（空 target），
    /// 断言按语义比对而不是按字节，才不会把上游的排版当接口契约。
    fn norm_ws(s: String) -> String {
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// `lit` 字面列（v0.1.81）：值一律绑定参数，三方言的 CAST 形态各不相同——
    /// PG 裸 `?` 会报「could not determine data type of parameter」，必须 CAST。
    #[test]
    fn lit_column_casts_per_dialect() {
        let reg = SchemaRegistry::new().table("t", &["id"], &["id", "name"]);
        let req = r#"{"table":"t","columns":["name",{"lit":"svc","as":"scope"}]}"#;
        let sq = sql_in(&reg, req, Dialect::Sqlite).unwrap();
        assert!(sq.contains("? AS \"scope\""), "{sq}");
        let my = sql_in(&reg, req, Dialect::MySql).unwrap();
        assert!(my.contains("CAST(? AS CHAR)"), "{my}");
        let pg = sql_in(&reg, req, Dialect::Postgres).unwrap();
        assert!(pg.contains("CAST($1 AS TEXT)"), "{pg}");
        // 值绝不进 SQL 文本。
        for s in [&sq, &my, &pg] {
            assert!(!s.contains("svc"), "{s}");
        }
        // 别名形状仍受既有别名校验约束。
        let bad = r#"{"table":"t","columns":[{"lit":"svc","as":"a-b"}]}"#;
        assert!(sql_in(&reg, bad, Dialect::Sqlite).is_err());
    }

    /// `orIgnore`（v0.1.81）：sqlite/pg 走 ON CONFLICT DO NOTHING；
    /// mysql 无原生支持 → `DO NOTHING` 在 8.0.20+ 才认，旧版只认 DO UPDATE 恒等式。
    #[test]
    fn or_ignore_renders_per_dialect() {
        let reg = SchemaRegistry::new().table("t", &["id"], &["id", "name"]);
        let req =
            r#"{"table":"t","verb":"insert","values":[{"id":1,"name":"a"}],"or_ignore":true}"#;
        // 裸 DO NOTHING（不带 target）才会忽略**任意**唯一键冲突——写成
        // `ON CONFLICT ("id")` 只管 id，别处唯一键撞车照样报错（实测 sqlite 3.54）。
        for d in [Dialect::Sqlite, Dialect::Postgres] {
            let s = norm_ws(sql_in(&reg, req, d).unwrap());
            assert!(s.contains("ON CONFLICT DO NOTHING"), "{s}");
            assert!(!s.contains("ON CONFLICT ("), "{s}");
        }
        let my = sql_in(&reg, req, Dialect::MySql).unwrap();
        assert!(my.contains("ON DUPLICATE KEY UPDATE `id` = `id`"), "{my}");
        // mysql 无主键表：DO UPDATE 的恒等列无处可取 → 拒绝。
        let nopk = SchemaRegistry::new().table("t", &[], &["id", "name"]);
        let e = sql_in(
            &nopk,
            r#"{"table":"t","verb":"insert","values":[{"name":"a"}],"or_ignore":true}"#,
            Dialect::MySql,
        )
        .unwrap_err()
        .to_string();
        assert!(
            e.contains("orIgnore on mysql requires a primary key on table 't'"),
            "{e}"
        );
    }

    /// orIgnore + fromSelect 的方言差异（v0.1.81）：pg/mysql 都能挂上去，
    /// sqlite 的 upsert 子句只能跟 VALUES——实测 `INSERT ... SELECT ... ON CONFLICT`
    /// 报 `near "DO": syntax error`，加 target 也没用（sqlite 3.54 直接不支持）。
    #[test]
    fn or_ignore_with_from_select_renders_per_dialect() {
        let reg = SchemaRegistry::new()
            .table("t", &["id"], &["id", "name"])
            .table("s", &["id"], &["id", "name"]);
        let req = r#"{"table":"t","verb":"insert","from_select":{"table":"s","columns":["id","name"]},"insert_cols":["id","name"],"or_ignore":true}"#;
        let pg = norm_ws(sql_in(&reg, req, Dialect::Postgres).unwrap());
        assert!(pg.contains("SELECT"), "{pg}");
        assert!(pg.contains("ON CONFLICT DO NOTHING"), "{pg}");
        let my = sql_in(&reg, req, Dialect::MySql).unwrap();
        assert!(my.contains("SELECT"), "{my}");
        assert!(my.contains("ON DUPLICATE KEY UPDATE `id` = `id`"), "{my}");
        let e = sql_in(&reg, req, Dialect::Sqlite).unwrap_err().to_string();
        assert!(
            e.contains("orIgnore with fromSelect is not supported on sqlite"),
            "{e}"
        );
    }

    /// `fromSelect`（v0.1.81）：列数必须与源 select 一致，且与 values 互斥。
    #[test]
    fn from_select_checks_arity_and_exclusivity() {
        let reg = SchemaRegistry::new()
            .table("t", &["id"], &["id", "name"])
            .table("s", &["id"], &["id", "name"]);
        let src = r#"{"table":"s","columns":["id","name"]}"#;
        let ok = format!(
            r#"{{"table":"t","verb":"insert","from_select":{src},"insert_cols":["id","name"]}}"#
        );
        let s = sql_in(&reg, &ok, Dialect::Sqlite).unwrap();
        assert!(s.contains("INSERT INTO"), "{s}");
        // 声明 2 列、源产出 3 列（全列展开）→ 拒
        let bad =
            format!(r#"{{"table":"t","verb":"insert","from_select":{src},"insert_cols":["id"]}}"#);
        let e = sql_in(&reg, &bad, Dialect::Sqlite).unwrap_err().to_string();
        assert!(
            e.contains("declares 1 columns but the source select yields 2"),
            "{e}"
        );
        // 与 values 互斥 / 两者都缺
        let both = format!(
            r#"{{"table":"t","verb":"insert","values":[{{"id":1}}],"from_select":{src},"insert_cols":["id","name"]}}"#
        );
        let e = sql_in(&reg, &both, Dialect::Sqlite)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("insert takes either values or fromSelect, not both"),
            "{e}"
        );
        let e = sql_in(&reg, r#"{"table":"t","verb":"insert"}"#, Dialect::Sqlite)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("insert needs either values or fromSelect"),
            "{e}"
        );
    }

    /// orIgnore / fromSelect 是 insert 的尾部子句（v0.1.81）：挂在其他动词上
    /// 无处可渲染 → 一律拒绝，而不是静默丢弃（用户会以为生效了）。
    #[test]
    fn insert_only_clauses_rejected_on_other_verbs() {
        let reg = SchemaRegistry::new().table("t", &["id"], &["id", "name"]);
        let bad = |req: &str| sql_in(&reg, req, Dialect::Sqlite).unwrap_err().to_string();
        for (req, want) in [
            (
                r#"{"table":"t","or_ignore":true}"#,
                "select does not accept orIgnore",
            ),
            (
                r#"{"table":"t","from_select":{"table":"t","columns":["name"]},"insert_cols":["name"]}"#,
                "select does not accept fromSelect",
            ),
            (
                r#"{"table":"t","verb":"update","sets":{"name":"a"},"conditions":[{"field":"id","op":"eq","value":1}],"or_ignore":true}"#,
                "update/delete does not accept orIgnore",
            ),
            (
                r#"{"table":"t","verb":"delete","conditions":[{"field":"id","op":"eq","value":1}],"from_select":{"table":"t","columns":["name"]}}"#,
                "update/delete does not accept fromSelect",
            ),
            (
                r#"{"table":"t","verb":"insert","values":[{"id":1}],"insert_cols":["id"]}"#,
                "insert columns are only allowed with fromSelect",
            ),
        ] {
            let e = bad(req);
            assert!(e.contains(want), "{want}: {e}");
        }
    }

    /// 源列必须点名（v0.1.81）：`*` / 省略都按 registry 的 HashMap 顺序摊列，
    /// 而 INSERT...SELECT 按位置对齐 → 顺序不定等于列名串位。
    #[test]
    fn from_select_source_needs_explicit_columns() {
        let reg = SchemaRegistry::new()
            .table("t", &["id"], &["id", "name"])
            .table("s", &["id"], &["id", "name"]);
        let err = |req: &str| sql_in(&reg, req, Dialect::Sqlite).unwrap_err().to_string();
        let e = err(
            r#"{"table":"t","verb":"insert","from_select":{"table":"s"},"insert_cols":["id","name"]}"#,
        );
        assert!(
            e.contains("fromSelect source needs explicit columns"),
            "{e}"
        );
        let e = err(
            r#"{"table":"t","verb":"insert","from_select":{"table":"s","columns":["*"]},"insert_cols":["id","name"]}"#,
        );
        assert!(
            e.contains("fromSelect source needs explicit columns"),
            "{e}"
        );
    }

    /// fromSelect 端到端：真拷贝行；带上 orIgnore 后主键冲突行静默跳过（v0.1.81）。
    #[tokio::test(flavor = "current_thread")]
    async fn from_select_copies_rows_and_pairs_with_or_ignore() {
        let b = seeded_bridge().await;
        // 不带 id（自增）：4 → 8 行。
        let cap = b
            .run(
                r#"db.table("t").fromSelect(db.table("t").select(["name","age"]), ["name","age"]).run()
                   .then(n => json.ok({ n }))
                   .catch(e => json.fail(500, String(e)));"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!((&v["code"], &v["data"]["n"]), (&json!(0), &json!(4)), "{v}");
        // 带 id：撞主键 → 裸 insert 报错（证明 orIgnore 确实在做事）。
        let cap = b
            .run(
                r#"db.table("t").fromSelect(db.table("t").select(["id","name"]), ["id","name"]).run()
                   .then(n => json.ok({ n }))
                   .catch(e => json.fail(500, String(e)));"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 500, "{v}");
        // sqlite 上 orIgnore 与 fromSelect 不能共存 → 构造期就拒（不是把 sqlite 的
        // `near "DO": syntax error` 抛给用户）。
        let cap = b
            .run(
                r#"db.table("t").fromSelect(db.table("t").select(["id","name"]), ["id","name"]).orIgnore().run()
                   .then(n => json.ok({ n }))
                   .catch(e => json.fail(500, String(e)));"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 500, "{v}");
        assert!(
            v["msg"]
                .as_str()
                .unwrap()
                .contains("orIgnore with fromSelect is not supported on sqlite"),
            "{v}"
        );
        // 失败的语句没写进任何行：仍是 8 行（4 原 + 4 副本）。
        let cap = b
            .run(
                r#"db.table("t").select(["name"]).all()
                   .then(r => json.ok({ n: r.length }))
                   .catch(e => json.fail(500, String(e)));"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!((&v["code"], &v["data"]["n"]), (&json!(0), &json!(8)), "{v}");
    }

    /// 租户守卫对抗（v0.1.81）：fromSelect 是「读+写」两条路径合一——
    /// 源 select 必须同样被租户收窄（否则把 b1 搬进自己表），
    /// 目标行的 tenant_id 必须由守卫补常量列（否则落成无租户行）。
    #[tokio::test(flavor = "current_thread")]
    async fn from_select_is_tenant_narrowed_and_stamped() {
        let b = guarded_bridge().await;
        // SQL 形态：常量列盖章 + 源侧条件，两个租户参数。
        let cap = b
            .run_with(
                r#"json.ok(db.table("t").fromSelect(db.table("t").select(["name"]), ["name"]).toSQL());"#,
                req_tenant("t1"),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        let sql = v["data"]["sql"].as_str().unwrap();
        assert!(sql.contains("__oj_tenant"), "{sql}");
        assert!(sql.contains("tenant_id"), "{sql}");
        assert_eq!(v["data"]["params"], json!(["t1", "t1"]), "{v}");
        // 执行：只搬 t1 的两行，盖 t1 章。
        let cap = b
            .run_with(
                r#"db.table("t").fromSelect(db.table("t").select(["name"]), ["name"]).run()
                   .then(n => json.ok({ n }))
                   .catch(e => json.fail(500, String(e)));"#,
                req_tenant("t1"),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!((&v["code"], &v["data"]["n"]), (&json!(0), &json!(2)), "{v}");
        // 新行确实属于 t1（不是空租户的幽灵行）。
        let cap = b
            .run_with(
                r#"db.table("t").select(["name","tenant_id"]).where({field:"name",op:"eq",value:"a1"}).all()
                   .then(r => json.ok({ n: r.length, tid: r[0].tenant_id }))
                   .catch(e => json.fail(500, String(e)));"#,
                req_tenant("t1"),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(
            (&v["data"]["n"], &v["data"]["tid"]),
            (&json!(2), &json!("t1")),
            "{v}"
        );
        // b1 未被搬走：系统视角下仍是 4 行（a1,a2,b1 + 两行副本）。
        let cap = b
            .run_with(
                r#"db.asSystem().table("t").select(["name"]).all()
                   .then(r => json.ok(r.map(x => x.name).sort()))
                   .catch(e => json.fail(500, String(e)));"#,
                req_tenant("t1"),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["data"], json!(["a1", "a1", "a2", "a2", "b1"]), "{v}");
    }

    /// 攻击者自己塞 tenant_id 列（v0.1.81）：常量列会被盖掉一半，直接拒绝。
    #[tokio::test(flavor = "current_thread")]
    async fn from_select_rejects_declared_tenant_column() {
        let b = guarded_bridge().await;
        let cap = b
            .run_with(
                r#"db.table("t").fromSelect(db.table("t").select(["name"]), ["name","tenant_id"]).run()
                   .then(n => json.ok({ n }))
                   .catch(e => json.fail(500, String(e)));"#,
                req_tenant("t1"),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 500, "{v}");
        assert!(
            v["msg"]
                .as_str()
                .unwrap()
                .contains("insert fromSelect must not supply tenant_id"),
            "{v}"
        );
    }

    /// lit 列的三条边界（v0.1.81）：非标量值拒绝、`*` 混用拒绝、别名缺失拒绝。
    /// 三条都不是「渲染出来能用就行」——它们是把 `{"a":1}` 串成文本那类静默垃圾挡在门外。
    #[test]
    fn lit_column_rejects_nonscalar_missing_alias_and_star_mixing() {
        let reg = SchemaRegistry::new().table("t", &["id"], &["id", "name"]);
        let err = |req: &str| sql_in(&reg, req, Dialect::Sqlite).unwrap_err().to_string();
        for (req, want) in [
            (
                r#"{"table":"t","columns":[{"lit":{"a":1},"as":"x"}]}"#,
                "lit column 'x' needs a scalar value",
            ),
            (
                r#"{"table":"t","columns":[{"lit":["a"],"as":"x"}]}"#,
                "lit column 'x' needs a scalar value",
            ),
        ] {
            let e = err(req);
            assert!(e.contains(want), "{want}: {e}");
        }
        // 缺 as 在反序列化层就被拦（LitSpec 要求别名齐备）。
        let e = serde_json::from_str::<QueryReq>(r#"{"table":"t","columns":[{"lit":1}]}"#)
            .unwrap_err()
            .to_string();
        assert!(e.contains("missing field `as`"), "{e}");
        // `*` 必须独占列清单（混入 lit 常量列同样算混用）。
        let e = err(r#"{"table":"t","columns":["*",{"lit":"s","as":"x"}]}"#);
        assert!(e.contains("select '*' must be the only column"), "{e}");
        // 标量四形态都放行（含 null）。
        for lit in ["1", "true", "null"] {
            let req = format!(r#"{{"table":"t","columns":[{{"lit":{lit},"as":"x"}}]}}"#);
            assert!(sql_in(&reg, &req, Dialect::Sqlite).is_ok(), "{req}");
        }
    }

    /// lit 常量列在嵌套层（子查询/CTE）同样成立：它只是列清单的一档形状。
    #[tokio::test(flavor = "current_thread")]
    async fn lit_column_works_inside_subquery_and_cte() {
        let b = seeded_bridge().await;
        let cap = b
            .run(
                r#"db.table("t").select(["name"]).where({field:"id",op:"in",subquery:
                       db.table("t").select([{lit:1,as:"k"}])}).all()
                   .then(r => json.ok({ n: r.length }))
                   .catch(e => json.fail(500, String(e)));"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!((&v["code"], &v["data"]["n"]), (&json!(0), &json!(1)), "{v}");
        // CTE 内部带常量列。
        let cap = b
            .run(
                r#"db.table("t").select(["name"])
                     .with("c", ["name","k"], db.table("t").select(["name",{lit:1,as:"k"}]))
                     .all()
                   .then(r => json.ok({ n: r.length }))
                   .catch(e => json.fail(500, String(e)));"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert!((v["code"] == 0), "{v}");
    }

    /// mysql orIgnore 的联合主键（v0.1.81）：恒等赋值按主键列逐个展开。
    #[test]
    fn or_ignore_renders_composite_primary_key_on_mysql() {
        let reg = SchemaRegistry::new().table("t", &["a", "b"], &["a", "b", "name"]);
        let req =
            r#"{"table":"t","verb":"insert","values":[{"a":1,"b":2,"name":"x"}],"or_ignore":true}"#;
        let my = sql_in(&reg, req, Dialect::MySql).unwrap();
        assert!(
            my.contains("ON DUPLICATE KEY UPDATE `a` = `a`, `b` = `b`"),
            "{my}"
        );
    }

    /// fromSelect 的目标列同样走白名单（v0.1.81）：写侧列名一律来自 registry。
    #[test]
    fn from_select_validates_target_columns() {
        let reg = SchemaRegistry::new()
            .table("t", &["id"], &["id", "name"])
            .table("s", &["id"], &["id", "name"]);
        let src = r#"{"table":"s","columns":["id","name"]}"#;
        for cols in [
            r#"["id","nope"]"#,
            r#"["s.id","name"]"#,
            r#"["id","name","extra"]"#,
        ] {
            let req = format!(
                r#"{{"table":"t","verb":"insert","from_select":{src},"insert_cols":{cols}}}"#
            );
            let e = sql_in(&reg, &req, Dialect::Sqlite).unwrap_err().to_string();
            assert!(e.contains("unknown column"), "{cols}: {e}");
        }
    }

    /// 快照往返（v0.1.81）：fromSelect / orIgnore 必须跟着 toJSON→fromJSON 走完，
    /// 否则跨模块传递的请求会静默丢掉「忽略冲突」这个意图。
    #[tokio::test(flavor = "current_thread")]
    async fn insert_only_clauses_survive_snapshot_roundtrip() {
        let b = seeded_bridge().await;
        let cap = b
            .run(
                r#"json.ok(db.table("t").fromSelect(db.table("t").select(["id","name"]), ["id","name"]).orIgnore().toJSON());"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(v["data"]["or_ignore"], true, "{v}");
        assert_eq!(v["data"]["insert_cols"], json!(["id", "name"]), "{v}");
        assert!(v["data"]["from_select"]["table"] == "t", "{v}");
        // sqlite 上这对组合本就不可用 → 重建后同样被拒（往返没把意图吞掉）。
        let cap = b
            .run(
                r#"const snap = db.table("t").insert({id:1,name:"a"}).orIgnore().toJSON();
                   json.ok({ sql: db.fromJSON(snap).toSQL().sql, params: db.fromJSON(snap).toSQL().params });"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert!(
            norm_ws(v["data"]["sql"].as_str().unwrap().to_string())
                .contains("ON CONFLICT DO NOTHING"),
            "{v}"
        );
    }

    /// 端到端：lit / orIgnore 在真 sqlite 上跑通。
    #[tokio::test(flavor = "current_thread")]
    async fn lit_and_or_ignore_run_on_sqlite() {
        let b = seeded_bridge().await;
        let cap = b
            .run(
                r#"db.table("t").select(["name",{lit:"svc",as:"scope"}]).orderBy([{field:"id",dir:"asc"}]).all()
                   .then(r => json.ok({ n: r.length, scope: r[0].scope }))
                   .catch(e => json.fail(500, String(e)));"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(v["data"]["n"], 4, "{v}");
        assert_eq!(v["data"]["scope"], "svc", "{v}");
        // 幂等插入：主键冲突时静默跳过，返回 0 行受影响。
        let cap = b
            .run(
                r#"db.table("t").insert({id:1,name:"a"}).orIgnore().run()
                   .then(n => json.ok({ n }))
                   .catch(e => json.fail(500, String(e)));"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(v["data"]["n"], 0, "{v}");
    }

    /// toJSON → fromJSON 往返保留别名：快照是可信入口，别名不能被吞掉。
    #[tokio::test(flavor = "current_thread")]
    async fn alias_survives_snapshot_roundtrip() {
        let b = seeded_bridge_2t().await;
        let cap = b
            .run(
                r#"const snap = db.table("a").as("x").select(["x.name"]).toJSON();
                   json.ok(db.fromJSON(snap).toSQL());"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert!(
            v["data"]["sql"].as_str().unwrap().contains("AS \"x\""),
            "{v}"
        );
    }
}
