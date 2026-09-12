# db.table 查询构造器对齐 xorm builder — 设计

日期：2026-09-12
状态：已评审（方案 A 经用户确认）

## 背景

现有 JS 安全查询构造器（`src/bridge/query.rs` + `bootstrap.js` 的 `queryBuilder`）只支持
SELECT：单层 AND 条件（eq/ne/gt/gte/lt/lte/in/like/isnull）、orderBy、limit/offset。
本设计将其对齐 xorm `builder` 包的能力面，并新增「只构造不执行」的 SQL 输出通道
（对应 xorm `ToBoundSQL`），便于排障定位。

定位裁决（用户确认）：**宿主侧实现**（`src/bridge/query.rs` 扩展现有 op + bootstrap.js
扩链式层）。`oj-plugin-ffi` **不加模块、ABI 不变**——插件 vtable 只吃 SQL 字符串
（`query(handle, sql, params)`），不构造 SQL；纯 FFI 契约 crate 不放业务 helper。

## 方案（A：单 op 扩展）

`QueryReq` 增字段、`op_db_query_build` 按动词分发 sea-query 四类构造器；白名单校验、
表归属守卫、tx 路由、方言占位符（sqlite/mysql `?`、postgres `$n`）全部复用现有链路。
另增 `op_db_query_sql`（同构造、不执行）供 `toSQL()` 使用。

## JS API（bootstrap.js `queryBuilder` 扩展）

```js
// 查询（现有 + 增强）
db.table("user").select(["id", "name"])
  .where({ or: [ {field:"age",op:"gte",value:18}, {field:"tag",op:"isnull"} ] })
  .distinct()
  .groupBy(["tag"]).having({field:"n",op:"gt",value:1})
  .orderBy([{field:"id",dir:"desc"}]).limit(10).offset(0)
  .all();                                  // 执行 → rows
db.table("user").select([{fn:"count", field:"id", as:"n"}]).all();

// 条件独立构造 / 组合 / 检查（对齐 xorm Cond；纯 JS 实现，零新 op）
const c0 = db.and(
  { field: "tenant_id", op: "eq", value: 7 },
  { field: "age", op: "gte", value: 18 },
);
const c = c0.or({ field: "tag", op: "isnull" });   // 增条件（不可变：返回新树，c0 不变）
c.tree();                                  // 输出嵌套树（普通 JSON，可复制/落日志）
c.fields();                                // → ["tenant_id","age","tag"]（去重，叶子序）
c.has("tenant_id");                        // → true（检查必要字段过滤是否到位）
db.table("user").where(c).all();           // where 接受条件对象或普通 JSON 树
// db.or(...) / db.not(...) / db.leaf(field,op,value) 同形工厂。

// join（on 只支持列对列等值，"表.列" 两段都过白名单）
db.table("a").join("b", [{left:"a.id", right:"b.aid"}], "left")
  .select(["a.id","b.name"]).all();

// DML（新增）
db.table("user").insert({name:"neo", age:1});              // 单行
db.table("user").insert([{name:"a"},{name:"b"}]);          // 多行
db.table("user").update({age:2}).where({field:"id",op:"eq",value:1});
db.table("user").delete().where({field:"id",op:"in",value:[1,2]});
// DML 一律返回受影响行数（exec 语义）。

// SQL 输出（新增，不执行）
db.table("user").select(["id"]).where({field:"age",op:"gt",value:18}).toSQL();
// → { sql: "SELECT ... WHERE \"age\" > $1", params: [18] }   （占位符按目标库方言）
```

链式方法对 DML 的约束（JS 层即报错，不等到 op）：
- `update`/`delete` **必须** `where` 非空（防全表误改/误删，红线级守卫，op 侧同样强制）。
- `insert` 不接受 `where/orderBy/limit`；`update`/`delete` 不接受 `limit/offset/groupBy`。

## Rust 侧（`src/bridge/query.rs`）

### QueryReq 扩展（全部 serde default，向后兼容）

```rust
struct QueryReq {
    db, table, columns, conditions, order_by, limit, offset,   // 现有
    verb: Verb,            // select(默认)/insert/update/delete
    values: Vec<Map>,      // insert：行数组（单行也归一成数组）
    sets: Map,             // update：列 → 值
    joins: Vec<Join>,      // {table, kind: inner(默认)|left|right, on: [{left, op(默认eq), right}]}
    group_by: Vec<String>,
    having: Option<CondTree>,
    distinct: bool,
}
```

`columns` 元素从纯字符串扩展为 `String | {fn, field, as}`（untagged 反序列化）。

### 条件树（对齐 xorm And/Or/Not）
```rust
#[serde(untagged)] enum CondTree {
    Leaf(Cond),                              // 现有 {field,op,value}
    And { and: Vec<CondTree> },
    Or  { or:  Vec<CondTree> },
    Not { not: Box<CondTree> },
}
```

- 现有 `{field,op,value}` 叶子写法不变（向后兼容）；`where()` 多次调用 = 顶层 AND。
- 上限：**深度 ≤ 8、叶子总数 ≤ 64**，超出报 `condition tree too deep/too large`。
- `having` 复用 CondTree；聚合别名（`as`）与分组列均允许作为 having 字段。

### 条件对象（JS 侧独立构造/组合/检查，对齐 xorm `Cond`）

条件树本质是 JSON，组合与检查**纯 JS 实现，零新 op**（`bootstrap.js` 内实现，op 侧
收到的还是同一棵树，`CondTree` schema 不变）：

- 工厂（挂在 db / DB(name) 实例上）：`leaf(field, op, value)`、`and(...)`、`or(...)`、
  `not(cond)`——参数接受普通 JSON 树或条件对象，返回条件对象。
- 组合：条件对象自带 `.and(c)` / `.or(c)` / `.not()`（返回新对象，不改原树——不可变，
  同一棵基树可派生多路条件，如「公共租户过滤 + 各业务追加」）。
- 检查/输出：`.tree()` 返回普通 JSON 嵌套树；`.fields()` 收集全部叶子 field（去重，
  深度优先序）；`.has(field)` 判断某字段过滤是否存在（典型用法：守卫层校验多租户
  查询必须带 `tenant_id` 过滤，缺则拒执行）。
- `.where(cond)` / `.having(cond)` 接受条件对象或普通 JSON 树；是对象则先 `.tree()`
  解包进 req。
- 上限（深度 8 / 叶子 64）仍在 op 侧统一强制——JS 层不重复计数，防绕过。

### 构造与执行

- 抽纯函数 `build_statement(req, &TableMeta, dialect) -> Result<(String, Vec<Value>)>`：
  校验（白名单/守卫输入）→ sea-query 构造 → 方言出 SQL + `value_to_json` 参数。
  `op_db_query_build`（执行）与 `op_db_query_sql`（仅构造）共用。
- 动词分发：`Query::select()` / `Query::insert()` / `Query::update()` / `Query::delete()`；
  select 走 `query_with_params`，insert/update/delete 走 `exec_with_params`（返回行数）。
- join 表同样过 `reg.get` + `check_table` 守卫；`table.col` 引用拆两段分别校验。
- DML 的列名（values/sets 的键）全部过白名单；值为任意 JSON → `to_qv` 绑定。
- limit 默认 100 / 硬上限 1000 仅作用于 select（DML 无 limit）。

### toSQL op

```rust
#[op2] #[serde] fn op_db_query_sql(state, req: QueryReq) -> Result<Value /* {sql, params} */>
```

走与执行完全相同的 `build_statement`（含白名单与守卫校验），拿到 `(sql, params)` 直接返回，
不触碰连接池/tx。方言取自目标命名库的 accessor（与执行所见一致）。

## 红线复核

- 标识符只来自 SchemaRegistry 白名单：表名、select 列、where/having 字段、orderBy、
  groupBy、join 表与 on 列、insert/update 的键——全部逐一校验，JS 字符串不拼 SQL。
- 值只经绑定参数（`Expr::val` / insert values → sea-query Values → 参数数组）。
- update/delete 无 where = op 侧 Err（JS 链层也拦）。
- 表归属守卫 `check_table` 对所有动词生效（含 join 表）。

## 错误处理

- 未知表/列/别名、非法 op、非法 join kind、超限条件树 → `JsErrorBox::generic`，
  文案带具体名字（沿用现有风格）。
- JS 链层只做形状约束（如 update 无 where 早抛），权威校验在 op。

## 测试（`src/bridge/query.rs` 既有测试模块内扩展）

- 条件树：or/not/and 嵌套、深度与数量上限触发；现有单层条件回归。
- 条件对象：工厂组合（and/or/not/leaf）、不可变派生、tree()/fields()/has() 输出正确；
  where 接受条件对象与裸 JSON 树等价执行（同结果集）。
- DML：insert 单行/多行、update/delete 行数断言、无 where 被拒。
- join：inner/left 结果集；on 列未过白名单报错。
- 聚合：count/sum + groupBy + having；distinct。
- toSQL：三方言占位符（沿用 `placeholder_per_dialect` 形态）；toSQL 与执行结果一致
  （toSQL 出的 sql+params 直接喂 `db.query` 得相同行）。
- 红线：insert/update 的键不在白名单报错；join 表归属守卫拦截。
- e2e（`oj/tests/e2e.rs`）：一条 join + 一条 insert 走 HTTP 全链路。
