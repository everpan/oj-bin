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

// DML（新增，.run() 终执行）
db.table("user").insert({name:"neo", age:1}).run();          // 单行
db.table("user").insert([{name:"a"},{name:"b"}]).run();      // 多行
db.table("user").update({age:2}).where({field:"id",op:"eq",value:1}).run();
db.table("user").delete().where({field:"id",op:"in",value:[1,2]}).run();
// DML 一律返回受影响行数（exec 语义）。

// SQL 输出（新增，不执行）
db.table("user").select(["id"]).where({field:"age",op:"gt",value:18}).toSQL();
// → { sql: "SELECT ... WHERE \"age\" > $1", params: [18] }   （占位符按目标库方言）

// 序列化（新增）：req 本就是纯 JSON，toJSON 深拷贝快照、fromJSON 复原后可继续链/执行
const q = db.table("user").select(["id"]).where(c);          // c = 条件对象（已解包进 req）
const snap = q.toJSON();       // → { db, table, verb, columns, conditions, ... } 纯 JSON
snap.limit = 50;               // 快照是普通对象，可直接改
db.fromJSON(snap).all();       // 复原（db 名从快照内读）→ 继续链或直接执行
```

链式方法对 DML 的约束（JS 层仅 DX 早抛；**所有动词×字段兼容性约束 op 侧为权威校验**，
`fromJSON` 可完全绕过 JS 链层——以下每条 op 侧都强制）：
- `update`/`delete` **必须**带 where 且**叶子数 ≥ 1**（`{and:[]}` 空组不算数，见条件树节）；
  不接受 `limit/offset/groupBy/joins/having/distinct`。
- `insert` 不接受 `where/orderBy/limit/groupBy/joins/having/distinct`；`values` 非空。
- `update` 的 `sets` 非空（空 `UPDATE t SET` 是非法 SQL）。
- `select` 不接受 `values/sets`。

## Rust 侧（`src/bridge/query.rs`）

### QueryReq 扩展（全部 serde default，向后兼容）

```rust
struct QueryReq {
    db, table, columns, conditions, order_by, limit, offset,   // 现有
    verb: Verb,            // select(默认)/insert/update/delete；enum 需 impl Default + #[serde(default)]
    values: Vec<Map>,      // insert：行数组（单行也归一成数组）
    sets: Map,             // update：列 → 值
    joins: Vec<Join>,      // {table, kind: inner(默认)|left, on: [{left, right}]}（列对列等值，
                           // 无 op 字段；right join 砍掉——sqlite 旧版本不支持且无用例，需要再加）
    group_by: Vec<String>,
    having: Option<CondTree>,
    distinct: bool,
}
```

`columns` 元素从纯字符串扩展为 `String | {fn, field, as}`（untagged，String 变体优先；
后果轻——错键对象落 String 变体必然报错）。聚合变体字段是关键字：`r#fn` +
`#[serde(rename = "fn")]`、`r#as` 同理。`fn` 是**类型化枚举**（serde lowercase：
count/sum/avg/min/max，未知值报错 → `Func::{count,sum,avg,min,max}`），非自由字符串；
`as` 别名只允许经 sea-query `Alias::new` 发射（引号包裹、绝不拼接），且 op 侧纵深校验形状
`^[A-Za-z_][A-Za-z0-9_]*$`，非法直接 Err。count 的 `field` 可省略或为 `"*"` →
`Expr::col(Asterisk)` 即 `COUNT(*)`；其余聚合函数 field 必填且过列白名单。

### 条件树（对齐 xorm And/Or/Not）
**CondTree 不用 untagged**（serde 明示 `deny_unknown_fields` 在 untagged 内不生效：
`{field,op,value,or:[...]}` 会误配 Leaf 并**静默丢弃 or**；全不配时报
"did not match any variant" 无字段路径）——**手写 `Deserialize`**：先收
`serde_json::Map`，按 `and`/`or`/`not`/`field` 键存在性唯一分发，多余键显式报错
（约 30 行，换精确报错 + 防静默降级）：

```rust
enum CondTree {
    Leaf(Cond),                 // 现有 {field,op,value}
    And(Vec<CondTree>),         // {and: [...]}
    Or(Vec<CondTree>),          // {or: [...]}
    Not(Box<CondTree>),         // {not: {...}}
}
```

- **向后兼容（写死）**：`conditions` 保持数组，元素逐个按 CondTree 解析，顶层多元素 = AND；
  既有 handler 的 `{field,op,value}[]` 线格式不变。`where()` 多次调用 = 顶层 AND。
- **空组语义**：`{and:[]}` / `{or:[]}` 直接 Err（`empty condition group`），不静默渲染为
  「无 WHERE」——否则 update/delete 的防全表守卫可被空树绕过。同理 update/delete 的
  守卫表述为**叶子数 ≥ 1**，与叶子上限同点检查。
- 上限：**深度 ≤ 8、叶子总数 ≤ 64**，超出报 `condition tree too deep/too large`。
- `in` 空数组：sea-query 渲染为 FALSE（select 返回空、delete 删零行——方向安全），
  明示语义，不额外拒绝。
- `having` 复用 CondTree；字段允许：白名单列、分组列、聚合别名。**别名引用在 op 侧
  展开回聚合表达式再进 HAVING**（PG 不允许 HAVING 引用 SELECT 输出别名，展开消灭三方
  言分叉）；别名与真实列同名时**白名单列优先、别名兜底**（与 PG 的 GROUP BY 歧义规则
  同向）。

### 条件对象（JS 侧独立构造/组合/检查，对齐 xorm `Cond`）

条件树本质是 JSON，组合与检查**纯 JS 实现，零新 op**（`bootstrap.js` 内实现，op 侧
收到的还是同一棵树，`CondTree` schema 不变）：

- 工厂（挂在 db / DB(name) 实例上）：`leaf(field, op, value)`、`and(...)`、`or(...)`、
  `not(cond)`——参数接受普通 JSON 树或条件对象，返回条件对象。
  （条件树本身是库无关的纯 JSON，挂 db 实例只是取用方便；快照/toJSON 语义同上。）
- 组合：条件对象自带 `.and(c)` / `.or(c)` / `.not()`（返回新对象，不改原树——不可变，
  同一棵基树可派生多路条件，如「公共租户过滤 + 各业务追加」）。
- 检查/输出：`.tree()` 返回普通 JSON 嵌套树；`.fields()` 收集全部叶子 field（去重，
  深度优先序）；`.has(field)` 判断某字段过滤是否存在（典型用法：守卫层校验多租户
  查询必须带 `tenant_id` 过滤，缺则拒执行）。
- `.where(cond)` / `.having(cond)` 接受条件对象或普通 JSON 树；是对象则先 `.tree()`
  解包进 req。
- `toJSON()` / `fromJSON(json)`：`queryBuilder` 的 req 本就是纯 JSON 对象——
  `toJSON()` 返回深拷贝快照；`db.fromJSON(json)` 按快照内 `db`/`table` 复原 builder，
  复原后可继续链式调用或直接执行。用途：查询定义落盘/跨模块传递/模板化改参。
  形状校验不做 JS 侧重复——权威校验仍在 op（fromJSON 进非法树与手写非法树同罪同罚）。
  **注意**：快照里的 `db` 是 JS 可见名（绑定模块下字面 `"default"`），复原时经
  `guard::bound_db` 按**复原方**模块重定向——模块内闭环语义一致；快照跨模块传递会
  按接收方重定向，属预期语义（与 tx 记账同约定），文档明示即可。
- 上限（深度 8 / 叶子 64）仍在 op 侧统一强制——JS 层不重复计数，防绕过。

### 构造与执行

- **两段拆分**：op 侧前置守卫函数 `guard_req(state, &req)`（`check_table` 主表 + 每个
  join 表——`check_table` 要 `&Rc<RefCell<OpState>>`，纯函数给不了；两个 op 共用）；
  纯函数 `build_statement(req: &QueryReq, reg: &SchemaRegistry, dialect: Dialect)
  -> Result<(String, Vec<Value>)>`：白名单校验（含 join 表 `reg.get`）→ sea-query 构造 →
  方言出 SQL + `value_to_json` 参数。`op_db_query_build`（执行）与 `op_db_query_sql`
  （仅构造）共用两段。
- **op 返回形态变更**：`op_db_query_build` 由 `Result<Vec<Value>>` 改为 `Result<Value>`
  （select → rows 数组；insert/update/delete → 受影响行数 number）——op2 serde 返回必须
  同一类型，JS 侧 `all()` 回数组、DML 链方法回 number。
- 动词分发：`Query::select()` / `Query::insert()` / `Query::update()` / `Query::delete()`
  （`build_select` 泛化为按动词 match 四类 statement 的 `build`）；
  select 走 `query_with_params`，insert/update/delete 走 `exec_with_params`（返回行数）。
- **insert 用 `InsertStatement::values()`（返回 Result），禁 `values_panic`**——宿主代码
  无 catch_unwind 保护，panic 会直接进 op 调用栈。
- **DML 同走 `resolve_target` tx 路由**（与 select 同一条）：本库活跃 tx → `session.exec`，
  他库 tx → Err——`db.tx(async tx => tx.table("t").insert(...))` 才能进事务。
- join 表同样过 `reg.get` + `check_table` 守卫。**无表别名 ⇒ 自 join v1 不支持**。
- **限定列引用通用规则**（select/where/orderBy/groupBy/having/join on 六处共用一个
  `split_once('.')` helper）：`"表.列"` 形态 → 表段必须 ∈ {基表} ∪ {join 表}，列段对该表
  的 TableDef 校验；非限定列只对基表解析——**join 存在时拒绝非限定列命中 join 表**
  （消歧，强制写全限定名）。
- DML 的列名（values/sets 的键）全部过白名单；值为任意 JSON → `to_qv` 绑定。
- 多行 insert 行间键集不一致 → **Err 拒绝**（评审两案之一，另一案「并集补 Null」被否：
  隐藏 NULL 插入有惊喜；且须拍平成 sea-query 的等长列模型，拒绝最简单）。
  `insert([])` 空数组 → Err（`insert needs at least one row`）。
- limit 默认 100 / 硬上限 1000 仅作用于 select（DML 无 limit）。
- **已知上限（本 spec 不治）**：`to_qv(Null) = Qv::String(None)` 的 NULL 绑定在 Postgres
  有类型推断风险（既有 eq-null 路径同缺陷，DML 把它扩散到 insert/update 值；根治需按列
  元数据选 `None::<i64>` 等，超范围）。DML-null 测试只断 sqlite。

### toSQL op

```rust
// 同步 op（构造全程是 OpState 同步借用，无 await，别加 async）
#[op2] #[serde] fn op_db_query_sql(state, #[serde] req: QueryReq) -> Result<Value, JsErrorBox>
// 返回 { sql, params }
```

走与执行完全相同的两段（`guard_req` + `build_statement`），拿到 `(sql, params)` 直接返回；
**不做 tx 路由、不受活跃 tx 影响**。方言取 `lookup(&state, &req.db)?.dialect()`
（含 bound_db 重定向，与执行所见一致）。

## 红线复核

- 标识符只来自 SchemaRegistry 白名单：表名、select 列、where/having 字段、orderBy、
  groupBy、join 表与 on 列、insert/update 的键——全部逐一校验，JS 字符串不拼 SQL。
- **新增标识符面收口**：聚合 `fn` 为类型化枚举（非字符串）；别名 `as` 只经 sea-query
  `Alias::new` 引号发射 + op 侧形状正则 `^[A-Za-z_][A-Za-z0-9_]*$` 纵深校验。
- 值只经绑定参数（`Expr::val` / insert values → sea-query Values → 参数数组）。
- update/delete 必须带 where 且**叶子数 ≥ 1**（空 and/or 组直接 Err）= op 侧强制，
  JS 链层仅 DX 早抛。
- 表归属守卫 `check_table` 对所有动词生效（含 join 表）。
- `bootstrap.js` 保持 **7-bit ASCII**（新增条件对象/链式方法的注释全英文）。

## 错误处理

- 未知表/列/别名、非法 op、非法 join kind、超限条件树、空条件组、动词×字段不兼容 →
  `JsErrorBox::generic`，文案带具体名字（沿用现有风格）。
- **反序列化错误消息质量**：`CondTree` 已改手写 `Deserialize`（按键唯一分发 + 多余键报错），
  天然精确；`columns` 保留 untagged（String 变体优先——错键对象落 String 变体必然报错，
  后果轻）。JS 链层对 where/having 入参做形状预检（便宜且只跑一次），与 DML 链层守卫同思路。
- JS 链层只做形状约束（如 update 无 where 早抛），权威校验在 op。

## 测试（`src/bridge/query.rs` 既有测试模块内扩展）

- 条件树：or/not/and 嵌套、深度与数量上限触发；**空 and/or 组 Err**；现有单层条件回归。
- 条件对象：工厂组合（and/or/not/leaf）、不可变派生、tree()/fields()/has() 输出正确；
  where 接受条件对象与裸 JSON 树等价执行（同结果集）。
- 序列化：toJSON 快照 → fromJSON 复原后执行，结果与原 builder 一致；快照改字段
  （如 limit）后生效；fromJSON 非法树被 op 拒绝（同手写非法树）。
- DML：insert 单行/多行（**行间键集不一致 Err**）、update/delete 行数断言、
  无 where / 空 where 组被拒、空 sets/空 values 被拒、动词×字段不兼容矩阵 op 侧全拦；
  **DML 走 tx 路由**（tx 内 insert 提交可见/回滚消失）。
- join：inner/left 结果集；on 列未过白名单报错；join 存在时非限定列命中 join 表被拒。
- 聚合：count(*)/count(field)/sum + groupBy + having（**别名引用展开**在 PG 方言 SQL 里
  不含别名字样）；distinct；别名形状非法 Err。
- toSQL：三方言占位符（沿用 `placeholder_per_dialect` 形态）；toSQL 与执行结果一致
  （toSQL 出的 sql+params 直接喂 `db.query` 得相同行）。
- 红线：insert/update 的键不在白名单报错；join 表归属守卫拦截。
- **夹具扩展**：`seeded_bridge` 扩成两表（第二表入 SchemaRegistry）供 join 用例；
  e2e（`oj/tests/e2e.rs`）的 schema.yaml 声明两张表，一条 join + 一条 insert 走 HTTP 全链路。

## 追加：Phase 8 —— sea-query 开放能力的安全 DSL 暴露（2026-09-12 用户确认）

范围裁决（用户确认）：**扩展现有安全 DSL**——sea-query 的更多构造能力以 JSON 请求形态
暴露，标识符仍全部过 SchemaRegistry 白名单，值仍只经绑定参数；不引入细粒度 statement
句柄绑定。v1 纳入五块：**子查询、UNION、CASE 列、窗口函数列、CTE**。全部仅 select 动词
可用，叠加进既有动词×字段矩阵（op 侧权威）。

### 嵌套请求通用约束（五块共用）

- 嵌套 select 以 `Box<QueryReq>` 递归承载（serde 递归经 Box 天然支持）；嵌套层：
  verb 必须 select、**禁止再带 `with`/`unions`**（防组合爆炸，v1），嵌套深度
  `REQ_NEST_MAX = 4`（子查询/union 成员/cte 查询共用同一计数）。
- 嵌套 req 递归过同一套白名单校验与归属守卫（`guard_req` 同步递归）。
- 参数收集：嵌套 SelectStatement 直接嵌入父语句，整棵树**一次性 build**，参数顺序由
  sea-query 统一收集——不做逐子查询参数合并。

### 子查询（where 值位置 + exists）

- `Cond` 增 `subquery: Option<Box<QueryReq>>`：`{field, op, subquery}`，op ∈
  in/eq/ne/gt/gte/lt/lte；`in` → `in_subquery`，比较 op → `col.op(select)`（标量子查询）。
  `value` 与 `subquery` 互斥（同现 Err）；isnull 不接受 subquery。
- `CondTree` 手写 Deserialize 增 `exists` 键：`{exists: <select-req>}` → `Expr::exists`，
  计入叶子数（叶子上限同约束）。
- **不做 from 子查询**：派生表列无白名单可依，破坏标识符收口。

### UNION

- `unions: [{kind: "all"|"distinct"(默认), query: <select-req>}]`；Intersect/Except 砍掉
  （mysql 8.0.30 之前不支持，无用例）。
- 仅顶层 select；union 时**基查询与成员都必须显式 columns** 且列数一致（op 侧校验，
  否则 DB 报错信息差）；成员禁 order_by/limit/offset（方言分叉，排序分页只对整体生效）。
- JS：`.union(otherBuilder, kind?)`。

### CASE 列 / 窗口函数列（select columns 新变体）

- case：`{case:{when:[{cond:<CondTree>, then:<JSON 标量>}], else:<JSON 标量,可省>},
  as:"x"}`——searched case only；cond 过 cond_expr 白名单；then/else **只允许值**
  （绑定参数），不允许列引用（v1）；`as` 必填过 `check_alias`。
- window：`{window:{fn:"row_number"|"rank"|"dense_rank", partition_by:[..] 可省,
  order_by:[{field,dir}] 可省}, as:"x"}`——fn 为类型化枚举 → 固定 `Func::custom`
  名（非自由字符串，红线不破）；partition/order 列过 ColCtx 白名单；frame 不做（YAGNI）。
- 两者别名**不进** having 别名账本（having 引用 case/window 别名直接 Err）。

### CTE（非递归 WITH）

- `with: [{name, columns:[..], query:<select-req>}]`（deny_unknown_fields）；name/columns
  过标识符形状校验；columns **必填**（CTE 输出列即白名单来源）；recursive 不做。
- 表解析：with 存在时，可引用表集合 = registry 表 ∪ CTE 名；CTE 表的列集合 = 声明的
  columns。主表与 join 表都允许是 CTE 名；CTE 名**不过** `check_table` 归属守卫
  （非真实表，无 owner 可言），registry 表照常。
- JS：`.with(name, columns, builder)`。

### 红线复核（Phase 8）

- 新增标识符面：CTE name/columns、case/window 别名、window fn 枚举——全部白名单/形状
  校验或类型化枚举，无自由字符串拼 SQL。
- 嵌套 req 的值仍只经绑定参数；子查询/union/cte 的表与列递归过同一白名单与归属守卫。
- 深度上限 REQ_NEST_MAX=4 防嵌套爆炸；叶子/深度上限在嵌套 req 内各自独立计数。
