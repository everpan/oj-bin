# db.table() 构造器补齐 7 项能力设计稿（v0.1.81）

- 日期：2026-10-19
- 状态：**四路评审已完成并通过，用户已确认**（2026-10-19）。评审砍掉 1 项（`expr` 模板），
  修正 6 处设计错误；用户拍板：`expr` 砍除、**op 全小写**、范围全做、delete 不豁免。
  实施计划另册：`2026-10-19-sql-builder-capabilities-implementation-plan.md`。
- 范围：oj-bin 单仓（`src/bridge/query.rs` + `src/bridge/bootstrap.js` 为主）。
  诉求来源：下游模板 `docs/sql-builder-missing-capabilities-pr.md`（上游 PR 提案，7 条）。
- 约束来源：CLAUDE.md 红线（动态标识符只来自 `SchemaRegistry`、值只走绑定参数、
  bootstrap.js 保持 7-bit ASCII、devkit 四件随版本同步）+ §0.3 版本锚复核。

---

## 0. 诉求复核（先核实，再立项）

按「下游指控先对最新版复核」惯例，7 条断言逐条取证。**结果：1 条误报、1 条部分误报、
1 条漏报、1 条因评审被砍。**

| # | 下游断言 | 判定 | 证据（file:line） |
|---|----------|------|-------------------|
| 1 | `like` 无 ESCAPE | **真缺** | `query.rs:488-497` 只 `LikeExpr::new(pat)`，无 escape |
| 2 | join ON 无值条件 | **真缺** | `OnPair{left,right}` 硬编码列=列（`query.rs:132-138`） |
| 3 | select 无字面量/计算列 | **拆分**（见 §0.1） | 字面量列真缺；计算列用现成 `case` 表达 |
| 4 | 无 `orIgnore` / `INSERT…SELECT` | **真缺** | 无 `on_conflict` / `select_from` 调用点 |
| 5 | 全表 delete 无豁免 | **真缺（且是刻意守卫）** | `bootstrap.js:1067` + `query.rs:1089` 双层 |
| 6 | 动态 WHERE 无出口 | **误报** | `where()` 是 `push`（`bootstrap.js:1047`）→ 多次调用 = AND；`in` 的 value 是 JS 数组，长度运行时可变（`query.rs:480-487`） |
| 7 | 无 `select *` | **部分真缺** | 省略 `.select()` 可行（`query.rs:1295-1303`），但**显式 `select(["*"])` 会报 `unknown column '*'`**（`"*"` 只在 count 特判 `query.rs:1319`，无列清单特例） |
| **8** | **（漏报）表别名** | **真缺，且是 notify.ts 的真正阻塞点** | `base_name` 恒等于 `req.table`（`query.rs:1287`）；`query.rs:1273-1275` 报 `self join not supported (no table alias)` |

### 0.1 关于 #3 的拆分

notify.ts 的两类 select 列要分开看：

- **字面量列** `'broadcast' as scope` —— **真缺**，无替代写法，本次做（§2.3）。
- **计算列** `(r.read_time IS NOT NULL AND r.read_time <> '') as is_read` —— **现在就能用
  现成的 `case` 表达**：`case` 的 `cond` 走同一个 `ColCtx`（`query.rs:1360` → `cond_expr`），
  而 `ColCtx` 带 join 表清单（`query.rs:1284-1289`），故 `r.read_time` 能过白名单校验：

  ```ts
  { case: { when: [
        { cond: { field: "r.read_time", op: "isnull" },     then: 0 },
        { cond: { field: "r.read_time", op: "eq", value: "" }, then: 0 },
      ], else: 1 }, as: "is_read" }
  ```

  > **拼写订正**：`Op` 是 `#[serde(rename_all = "lowercase")]`（`query.rs:30`），
  > 线上拼写是 **`isnull`**（`query.rs:41`，`api-manual.md:1143`）。
  > 初稿此处误写 `isNull`，会直接坑下游，已订正。

- **`expr` 任意表达式模板 —— 评审砍掉，用户已确认（2026-10-19）**（见 §12 处置 R-1、
  §13 砍除声明）。notify.ts 用 `case` 即可满足，`expr` 无下游真实代码举证，
  且三方言实现不成立。

### 0.2 关于 #8 的优先级

`FROM notifications_broadcast b LEFT JOIN notifications_broadcast_read r` 需要表别名。
**即使补齐 #2 + #3，notify.ts 仍写不出来**——`check_col("b.label")` 会去找一张名叫 `b` 的表。
故 #8 是 notify.ts 的**前置**，优先级实际高于 PR 提案里的 P0 项。

### 0.3 版本锚

下游提案依据 `api-manual.md`（§1142/§1143/§1343/§1346/§1378）与框架 README v0.1.66。
本仓当前 `oj/Cargo.toml` 为 **0.1.80**。下游引用的能力清单与本仓最新实现存在偏差
（#6 已具备），提示其 vendored 版本落后，**其 devkit 副本亦可能落后**。
故 §9 的文档同步需同时承担「能力落地」与「下游错文档纠正」两件事。

---

## 1. 目标与非目标

### 目标

1. **#1 根治模糊搜索**：新增语义化 op，业务层零转义负担、零方言知识；
2. **#2 join ON 值条件**：`ON r.broadcast_id = b.id AND r.user_id = ?`；
3. **#3-a 字面量列 `lit`**；
4. **#4 幂等写入**：`orIgnore()` 与 `fromSelect()`；
5. **#8 表别名**（仅 select/join，见 §1.1）；
6. **#6 加 `whereIf` 糖**；**#7 实现 `select(["*"])` 展开**。

### 非目标

- **`db.query` / `db.exec` 下架**（PR §4 已排除）；
- **全表 delete 豁免**（用户拍板：不豁免，见 §1.1）；
- **任意表达式模板 `expr`**（评审砍掉，§12 R-1）；
- **放开 `where(rawString, params)`** —— 与红线冲突；
- **update 全表豁免**（比 delete 更危险，一并拒绝）。

### 1.1 拍板 / 评审裁定记录

| 决策点 | 决定 | 来源 |
|--------|------|------|
| 模糊搜索 | **根治**（语义化 op），非最小 `escape` 字段 | 用户拍板 |
| 表达式列 | **砍掉 `expr` 模板**，只做 `lit`；计算列用现成 `case` | 用户拍板「全做」+ 评审 R-1 推翻 |
| 表别名作用域 | **只放行 select / join**；update/delete 带别名直接 Err | 评审 R-3（mysql `DELETE FROM t AS b` 需 8.0.16+） |
| 全表 delete/update | **不豁免** | 用户拍板 |
| 范围 | 全部（B1+B2+B3） | 用户拍板 |

---

## 2. JS API 设计

### 2.1 #1 模糊搜索：语义化 op（根治）

```ts
db.table("contacts")
  .where({ or: [
    { field: "name",  op: "contains",  value: kw },
    { field: "phone", op: "contains",  value: kw },
  ]})
  .all();
```

引擎内部（三方言统一）：

1. 取 `value` 字符串 → **转义**：先转义转义符自身 `!` → `!!`，再转义 `%` → `!%`、`_` → `!_`
   （顺序不可颠倒）；
2. 按 op 拼模式：`contains` → `%kw%`，`startsWith` → `kw%`，`endsWith` → `%kw`；
3. 发射 `Expr::col(..).like(LikeExpr::new(pattern).escape('!'))`
   → 渲染 `LIKE ? ESCAPE '!'`（渲染路径：`expr/enum.rs:869-879` + `query_builder.rs:708`
   `BinOper::Escape → "ESCAPE"`；pattern 走 `Expr::Value` → 绑定参数，**红线不破**）。

**为什么是根治**：业务层既不接触 `%`/`_` 转义，也不接触 `ESCAPE` 方言差异。

**命名（评审订正 + 用户确认 2026-10-19）**：`Op` 是 `#[serde(rename_all = "lowercase")]`
（`query.rs:30`），与 `gte/lte/isnull` 同族。三个新 op 一律**小写**：
`contains` / `startswith` / `endswith`。
（camelCase 会与既有 op 割裂，且需逐个 `rename`，有误改全局 attr 破坏 `isnull`/`eq` 的风险。）
文档在 `api-manual.md:1143` 同一行标注。

**逃生舱**：保留原 `like` op 并加**可选** `escape` 字段：

```ts
{ field: "path", op: "like", value: "a!%b", escape: "!" }
```

`escape` 校验：**`len() == 1`**（`""` 与 `"!!"` 均拒）+ 白名单 `!` `/` `|` `~` `#` `@`。
**显式拒绝 `\`**（mysql 默认 `sql_mode` 下 `ESCAPE '\'` 直接 1064，框架既有纪律）；
拒绝引号类字符（`'` `"` `` ` ``）与通配符自身（`%` `_`）。
不带 `escape` 时行为零变化（向后兼容）。
文档定位为「高级 / 框架内部」，**不在 op 清单主行展示**（避免诱导业务照抄自转义老代码，
与「根治」自相矛盾）。

### 2.2 #2 join ON 值条件

```ts
db.table("notifications_broadcast").as("b")
  .join("notifications_broadcast_read", [
    { left: "r.broadcast_id", right: "b.id" },      // 列 = 列（现有形态，不变）
    { left: "r.user_id", op: "eq", value: uid },    // 列 = 值（新增，值走绑定参数）
  ], "left")
```

> **API 订正**：本仓**没有 `leftJoin`**（只有 `join(table, on, kind)`，
> `bootstrap.js:1060`；`api-manual.md:1185` 亦用 `.join(..., "left")`）。
> 下游提案里的 `.leftJoin(...)` 照抄会 `TypeError`。**本次不新增 `leftJoin` 糖**
> （保持 API 面最小），§11 明确告知下游改用 `.join(..., "left")`。

`op` 复用 `Op` 的比较子集，**排除 `like` / `contains` / `startswith` / `endswith` / `isnull`**
（`ON x LIKE ?` / `ON x IS NULL` 无用例，且 `isnull` 无值、与「值条件」形态冲突）。
**排除四个 like 系 op 是必须的**：否则 `leaf_subquery`（`query.rs:658-667`）等处的
穷尽 match 会编译通过但语义错。

### 2.3 #3-a 字面量列 `lit`

```ts
{ lit: "broadcast", as: "scope" }
```

发射 `CAST(? AS <text-type>)`，值走绑定参数，`<text-type>` 按方言取（§5.1）。
语法上**无法拼出标识符**——这是它区别于被砍掉的 `expr` 的关键。

### 2.4 #4 幂等写入

```ts
db.table("notifications_broadcast_read")
  .insert({ broadcast_id, user_id, read_time })
  .orIgnore()
  .run();

db.table("notifications_broadcast_read")
  .fromSelect(
    db.table("notifications_broadcast").select(["id", { lit: uid }, { lit: now }]),
    ["broadcast_id", "user_id", "read_time"],
  )
  .orIgnore()
  .run();
```

> **API 订正**：本仓 `insert(rows)` 把**数组当多行**（`bootstrap.js:1052`），
> 且 `validate_verb` 对 insert 显式 `reject("insert", "columns")`（`query.rs:1057`）。
> 故初稿的 `insert(["col",...])` 两头不通。改为 **`fromSelect(query, cols)` 单方法**，
> cols 作为第二参数，既消除 `insert` 的重载歧义，也少一个 API 面。

- **列数一致性校验**：`cols.length` 必须与内层 select 的输出列数相等，不等即 Err
  （防「内层忘写 `.select()` → 走全列展开 → 列数恰好相等 → 静默写错列」）。
- **`orIgnore` × `returning` 在 mysql 上直接 Err**（§5.2）。

### 2.5 #8 表别名

```ts
db.table("notifications_broadcast").as("b")
  .join("notifications_broadcast_read", [...], "left")
  .select(["b.id", "r.read_time"])
```

> **形态裁定**：采用 **`.as("b")` 为主入口**（架构评审反对把 `"x as b"` 字符串协议
> 引入表名，理由是「把不透明字符串这一毒瘤从 expr 又引进了表名」）。
> 兼容写法 `"x as b"` 保留为解析约定，但**文档主推 `.as()`**。

- 支持不带别名（现状不变）、join 表带别名；放开自 join（两侧别名须不同）。
- **仅 select / join 生效**；update / delete 带别名 → Err（§1.1）。
- 别名过 `check_alias`（`query.rs:287-297`），并**排除与 CTE 名冲突**
  （`ColCtx::table_of` 按字符串匹配 CTE 名，`query.rs:552`）。

### 2.6 #6 `whereIf` 糖

```ts
db.table("todos").whereIf(status, { field: "status", op: "eq", value: status }).all();
```

判定：`undefined` / `null` / `false` **与空数组 `[]`** 均跳过。
> 空数组必须排除：动态过滤最常见的是 `ids?: number[]`，`whereIf(ids, {op:"in", value:ids})`
> 在 `ids=[]` 时会落到 `IN ()`。**引擎侧同步加 `Op::In` 空数组守卫**
> （`query.rs:480-487` 当前只校验「是数组」，无空数组检查），双保险。

### 2.7 #7 `select(["*"])`

```ts
db.table("sys_refresh_token").select(["*"]).where({ field: "token_hash", op: "eq", value: hash }).all();
```

- 映射到「清空 columns」走全列展开（`query.rs:1295-1303`）；
- **`["*"]` 必须是唯一元素**，`["*", {...}]` 直接 Err；
- **带 join 时 `select(["*"])` 直接 Err**：全列展开只展开基表列，
  业务若以为拿到了 join 列去做授权判断会拿到 `undefined`（安全评审 M3）。

---

## 3. 引擎侧实现（Rust）

### 3.0 前置：表引用的类型化（`TableRef`）—— 评审 R-2 的核心修复

**不在 20 处零散做字符串解析，而是在入口一次性归一化。**

```rust
/// 表引用：real = registry 真名（守卫/租户判定用），alias = SQL 别名（渲染用）。
struct TableRef { name: String, alias: Option<String> }
impl TableRef { fn sql_name(&self) -> &str { self.alias.as_deref().unwrap_or(&self.name) } }
```

- `QueryReq.table` 与 `Join.table` 类型改为 `TableRef`，自定义 `Deserialize`
  （解析一次：`.as()` 形态直接给两个字段；`"x as b"` 字符串形态按 ` as ` 拆分）。
- **此后所有消费点只读 `.name` 或 `.sql_name()`**，编译期即可区分语义。

必须改的消费点（初稿只列了 10 处，评审补全为 22 处。`query.rs` 除非注明）：

| 用途 | 行 | 用 real 还是 alias |
|------|----|----|
| `guard::check_table` | `788-789` / `792-793` | **real**（否则 `owner_of("t as b")` → None → **守卫静默放行**） |
| `scoped()` / `reg.get` | `855` / `858-859` / `862-863` | **real**（否则别名表恒判为「非租户表」→ **租户注入被跳过**） |
| `tenant_value(reg, ...)` | `874` / `889` / `924` | **real** |
| `reg.get` → unknown table | `1133` / `1186` / `1218` | **real** |
| `resolve()` | `1269` / `1284` | **real** |
| 自 join 判定 | `1273-1275` | 按 `sql_name()` 判重 |
| `ColCtx.base_name` | `1287` / `1189` / `1217` | **alias** |
| 全列展开限定前缀 | `1303` | **alias** |
| `q.from` / `q.join` | `1405` / `1422` | alias（`from_as` / `join_as`） |
| tenant 注入限定列 | **`872`** | **alias** |
| join tenant 注入限定列 | **`1416`** | **alias** |
| insert/update/delete 表标识 | `1144` / `1195` / `1225` | insert 用 real（不支持别名）；update/delete 带别名→Err |

> **为什么是 blocker**：`guard_req` 与 `apply_tenant` 都跑在 build **之前**
> （`query.rs:1595-1597`）。若别名留在原串里，`owner_of("users as u")` 返回 `None`
> → `guard.rs:249` judge 直接 `Ok` → **deny 模式下也能读写他模块表**；
> `scoped("users as u")` → `reg.get` None → **完全不注入 tenant 条件**。
> 这是「改错了才出事」，而是「不改就出事」。

### 3.1 #1 模糊搜索

| 位置 | 改动 |
|------|------|
| `query.rs:31-42` | `Op` 新增 `Contains` / `StartsWith` / `EndsWith`，均 `#[serde(rename = "contains" / "startswith" / "endswith")]`（**不要动全局 `rename_all`**） |
| `query.rs:44-54` | `Cond` 新增 `#[serde(default)] escape: Option<String>`（`deny_unknown_fields` 会自动挡住未声明字段，改动会立刻暴露） |
| 新增 `fn escape_like(kw, escape) -> Result<String>` | 先转义 escape 自身，再转义 `%` `_` |
| `query.rs:488-497` | `Op::Like` 解析 + 白名单 + `len()==1` 校验，有则 `.escape(c)`；新增三个语义 op 分支 |
| `query.rs:644` / `658-667` | `Op::Like` 的既有特判需同步覆盖新 op；**`Op` 加 `fn class()`**（评审 m1），`apply_op` / `leaf_subquery` / join-ON 三处按 class 判定，避免穷尽 match 编译通过但语义错 |

### 3.2 #2 join ON 值条件

| 位置 | 改动 |
|------|------|
| `query.rs:132-138` | `OnPair` 从 struct 改**枚举**：`Cols{left,right}` / `Val{left,op,value}`，手写 `Deserialize` 按键分发（照 `CondTree`，`query.rs:69-113`，避免 untagged 吞错误） |
| `query.rs:1406-1423` | join 循环：`Cols` 走现路径；`Val` 走 `col_simple_expr(left).<op>(Expr::val(to_qv(value)))`；op 按 `class()` 判定，非比较类直接 Err |
| `bootstrap.js:1060` | `on` 映射允许 `{left, op, value}`，且 **`value` 必须过 `encodeParams`**（`bootstrap.js:441`）——否则 BigInt uid 丢精度 / 丢 `$oj$i64` 标记 |

### 3.3 #3-a 字面量列

| 位置 | 改动 |
|------|------|
| `query.rs:247-253` | `ColSpec` 新增 `Lit(LitSpec)` |
| `query.rs:255-284` | `ColSpec::deserialize` 按键分发新增 `lit` 一支 |
| 新增 `struct LitSpec { lit: Value, as: String }` | 值走 `to_qv` |
| `query.rs:1295-1303` 与 select 构建 | `select(["*"])` 映射为清空 columns；`["*"]` 非唯一元素 → Err；带 join → Err |
| `bootstrap.js:1035-1045` | `select` 的对象形态统一走 `encodeParams`（现只有 case 分支走了），否则 `{lit: BigInt}` 不走既有处理路径 |

### 3.4 #4 幂等写入

| 位置 | 改动 |
|------|------|
| `query.rs:300-339` | `QueryReq` 新增 `or_ignore: bool` / `from_select: Option<Box<QueryReq>>` / `insert_cols: Vec<String>` |
| **`child_reqs()` 抽取** | 见 §3.5 —— 这是安全 blocker 的修复 |
| `query.rs:1060-1098` | `validate_verb`：insert 分支判定 `values` 与 `from_select` **互斥且必居其一**；`insert_cols` 仅与 `from_select` 同现；`from_select` 强制 `verb == Select` + 列数一致 + 调 `validate_nested(.., 1, "fromSelect")`；`or_ignore` 仅 insert 合法；mysql 下 `or_ignore + returning` → Err |
| `query.rs:1144` 附近 | `ins.on_conflict(...)`（方言分派 §5.2）；`from_select` 走 `ins.select_from(...)`（sea-query `query/insert.rs:228`） |
| `registry.rs:57-66` → `query.rs:508-531` | `TableSrc` 新增 `fn pk()` 暴露 `TableDef.primary_key`（`registry.rs:60`） |
| `bootstrap.js` | 新增 `orIgnore()` / `fromSelect(q, cols)`；`run()` 的 insert 判空改为「values 或 from_select 二选一」 |

### 3.5 `child_reqs()` 统一递归（安全 blocker 修复）

`QueryReq` 树现有三个 walker，各自维护递归清单，本次新增的 `from_select` 是第四条边，
**三个 walker 全部会漏**。

```rust
fn child_reqs(r: &mut QueryReq) -> impl Iterator<Item = &mut QueryReq> {
    // unions[].query / with[].query / 条件树里的 subquery+exists / from_select
}
```

三个 walker **改用它**：
- `guard_req`（`query.rs:786-817`）—— 漏则**归属守卫绕过**；
- `apply_tenant`（`query.rs:843-949`）—— 漏则**跨租户读写**；
- `validate_nested`（`query.rs:603-627`）—— 漏则无 `REQ_NEST_MAX`、无「嵌套禁 unions/with」。

**Insert 分支的特殊性（最高风险点）**：`apply_tenant` 的 Insert 分支只遍历 `req.values`
（`query.rs:881-892`）；`from_select` 时 `values` 为空 → **tenant_id 既不校验也不强制写入**
→ 可把受害者的 tenant_id 写进去。

> 规则：**insert 列清单缺 `tenant_id` 时由引擎补齐，值取当前 tid，禁止由内层 select 提供。**

对抗用例：`db.table("my_notes").fromSelect(db.table("sys_user").select(["tenant_id","email"]), ["tenant_id","body"]).run()` → 必须 Err。

### 3.6 #6/#7 辅助

- `whereIf`：仅 `bootstrap.js`；
- `Op::In` 空数组守卫：`query.rs:480-487`；
- `select(["*"])`：§3.3。

### 3.7 CTE / union 列数一致性（评审 m3，顺带补）

`CteReq.columns`（`query.rs:362-366`）与内层输出列**零一致性校验**，错位只在 DB 执行期报错。
既然 §2.7 已禁 `*` 混入、列数可算，在 `build_with_clause`（`query.rs:1506-1528`）加
「内层列数 == 声明 columns 数」；union 侧现有只比数量（`query.rs:1480-1486`），补别名集合比对。

---

## 4. 安全模型

### 4.1 红线对照表

| CLAUDE.md 红线 | 本设计如何守 |
|----------------|--------------|
| 动态标识符只来自 `SchemaRegistry` | 砍掉 `expr` 后，**唯一**新增的字符串→标识符通道是表别名，且经 `check_alias` + 入口归一化（§3.0）；`lit` 语法上无法拼出标识符 |
| 值只走绑定参数 | `lit` / join 值条件 全部 `to_qv` 绑定；like 的 pattern 与 escape 均按常量/参数处理 |
| 别名收口 | 表别名与列别名一律 `check_alias`（`query.rs:287-297`） |

### 4.2 关于被砍掉的 `expr`：残余风险的证伪

初稿曾论断「模板下的函数调用由数据库自身权限边界兜底（连接账号即边界），不构成越权通道」。
**安全评审推翻了这个论断**：连接账号通常就是业务账号，边界不成立。具体：

- `'` 被字符集白名单禁掉 → 拼不出字面量；
- 但 `chr(47)||chr(101)||...` 可拼字符串 → `pg_sleep(60)`、mysql `sleep(60)` /
  `benchmark(...)`、`load_file(concat(char(47), ...))` 在白名单下**全部合法**
  → 连接池占满 DoS / 文件外带。

叠加 PG 占位符不成立（§5.3）与迁移不可逆（§10），**裁定砍掉**。这条同时回答了
「为什么不做减法式的黑名单加固」——黑名单是「按例外扩张」的目录，只会放宽，
而本批次唯一的真实需求（`is_read` 计算列）用 `case` 已可满足。

### 4.3 `select(["*"])` 的约束

- `["*"]` 必须是唯一元素；
- **带 join 时直接 Err**（全列展开只出基表列，业务拿 join 列做授权判断会得到 `undefined`）。

### 4.4 全表 delete 守卫（不豁免）

守卫保持**双层**不动：`bootstrap.js:1067` + `query.rs:1089`。
新增能力**不得**引入绕过路径；`fromSelect` 只作用于 insert。

### 4.5 租户与别名的精确断言（评审 m5）

tenant 注入改用别名后，若 join 的 ON 条件误用基表别名（`a.tenant_id`），
条件退化为恒真 → join 表跨租户全读。断言须精确到**「每条 ON 用的是该 join 自己的别名」**，
并单列自 join（`t as a` / `t as b`）用例。

---

## 5. 方言矩阵

### 5.1 `lit` 字面量列的 CAST

| 方言 | 发射 | 理由 |
|------|------|------|
| Sqlite | `?` | 动态类型，无需 cast（读码确认） |
| Postgres | `CAST(? AS TEXT)` | **spike 项**：裸 `$1` 可能被 sqlx 报 `could not determine data type of parameter`。注：`case` 的 `Expr::val(to_qv)`（`query.rs:1365`）走的是 CASE 内部上下文，PG 对 unknown 按 text 解析，不构成先例 |
| MySql | `CAST(? AS CHAR)` | mysql 无 `TEXT` 目标类型（读码确认） |

### 5.2 `orIgnore` 方言分派

| 方言 | 发射 | 依据 |
|------|------|------|
| Sqlite | `ON CONFLICT DO NOTHING` | sqlite 3.24+ upsert |
| Postgres | `ON CONFLICT DO NOTHING` | 标准 |
| **MySql** | **`ON DUPLICATE KEY UPDATE <pk> = <pk>[, ...]`** | 见下 |

**关键 spike 结论（已取证，实现时不要走回头路）**：

- sea-query mysql 后端在 `DoNothing` 且 **pk 为空**时发射 `" IGNORE"`
  （`backend/mysql/query.rs:261`），但通用发射器先写 `prepare_on_conflict_keywords`
  （mysql = `" ON DUPLICATE KEY"`，`backend/mysql/query.rs:268`）再写 action
  （`backend/query_builder.rs:1438-1441`），组合结果疑似 `ON DUPLICATE KEY IGNORE`
  —— **不是合法 mysql**；
- 上游 sea-query 1.0.2 全仓**零条**测试断言 `INSERT IGNORE`
  （`grep -rn "INSERT IGNORE" src/ tests/` 无命中）；
- 且 `INSERT IGNORE` 语义本身就坏：把**所有**错误（NOT NULL、截断、类型错误）
  降级为警告，属静默数据损坏面。

**故 mysql 一律走 `do_nothing_on(pk)`**，既绕开未验证路径，语义上也只吞重复键。
**mysql 侧表无主键（`TableDef.primary_key` 为空）→ 直接 Err，不静默回落。**
pg/sqlite 无主键时正常回落 `do_nothing()`（无需 target）。

### 5.3 `expr` 已被砍，其方言证据保留备查

- PG 的 placeholder 是 `$N`（`backend/postgres/query.rs:35-37`），
  `CustomWithExpr` 只认 `$N`（`backend/query_builder.rs:473-500`）→ **`?` 在 PG 下不是占位符**；
- `cust_with_values` 数量不匹配时 `&values[count]` **越界 panic**
  （`backend/query_builder.rs:492`），PG 下 `$0` 还会 usize underflow；
- 模板里手工拼的列名不按方言加引号 → mysql 反引号下 `"r"."read_time"` 被当字符串常量
  → **比较恒真**（静默错值）。

### 5.4 `orIgnore` 的 affected-rows 三方言不一致（已知，写进契约测试）

本仓 mysql 协商 `CLIENT_FOUND_ROWS`（`plugins/oj-db-mysql/src/lib.rs:1524`、`1566-1570`）：

| 方言 | 命中重复键时的 affected rows |
|------|------|
| MySql（`ON DUPLICATE KEY UPDATE id=id`） | **1**（匹配行） |
| Pg / Sqlite（`DO NOTHING`） | **0** |

→ 下游 `if (n === 1)` 的幂等判定在 mysql 上**恒真**。必须在契约测试里钉死三方值，
并在 api-manual 限制表与 SKILL 陷阱速查里写明。

### 5.5 ESCAPE

三方言统一 `LIKE ? ESCAPE '!'`。转义符默认 `!`，白名单见 §2.1。
**`!` 是框架既有纪律值**（README v0.1.66 ESCAPE 纪律）。

---

## 6. 错误面（点名 + 原因 + 下一步）

| 场景 | 文案 |
|------|------|
| `escape` 非法长度 / 字符 | `escape must be a single char from: ! / \| ~ # @` |
| join 值条件用了非法 op | `join on value condition does not support op 'contains'` |
| 自 join 别名重复 | `duplicate table alias 'b'` |
| update/delete 带别名 | `table alias is not supported on update/delete` |
| 别名非法形状 / 与 CTE 冲突 | `illegal alias 'x as y'` |
| `*` 与列混用 | `select '*' must be the only column` |
| 带 join 的 `*` | `select '*' is not allowed with joins (only base-table columns would expand)` |
| `Op::In` 空数组 | `in needs non-empty array` |
| `orIgnore` 在 mysql 无主键 | `orIgnore on mysql requires a primary key on table 'x'` |
| `orIgnore` × `returning` 在 mysql | `orIgnore with returning is not supported on mysql` |
| insert 同时给 values 与 fromSelect | `insert takes either values or fromSelect, not both` |
| insert 缺 values 与 fromSelect | `insert needs either values or fromSelect` |
| fromSelect 列数不符 | `fromSelect declares 3 columns but the source select yields 2` |
| fromSelect 非 select | `fromSelect source must be a select query` |
| CTE 列数不符 | `cte 'x' declares 2 columns but the inner select yields 3` |
| update/delete 无 where（既有） | `update requires where (leaf count >= 1)` —— **保持不动** |

---

## 7. 兼容性

- **全部新增字段 `#[serde(default)]`**，既有 JS 调用零影响；
- `Op` 新增变体、`ColSpec` / `OnPair` 新增分支 —— 均为**加法**；
- **`TableRef` 是破坏性类型改动**，但 `Deserialize` 兼容字符串 → 字符串输入零迁移；
- `bootstrap.js` 保持 **7-bit ASCII**（红线）：新增文案全部英文；
- 快照：`QueryReq` 无 `deny_unknown_fields`（`query.rs:300`），
  `bootstrap.js:1029-1032` 的 `Object.assign` 会补齐默认值，
  `encodeSnapshot`（`bootstrap.js:463`）是通用 JSON 往返 → 新字段自动带上（读码确认）。

---

## 8. 测试计划

### 8.1 方言矩阵（主力，无需真库）

`toSQL()`（`op_db_query_sql`）可纯字符串断言。矩阵：

| 能力 | Sqlite | MySql | Postgres |
|------|:------:|:-----:|:--------:|
| contains/startswith/endswith + ESCAPE | ✓ | ✓ | ✓ |
| `like` + `escape` 字段 | ✓ | ✓ | ✓ |
| join ON 值条件 | ✓ | ✓ | ✓ |
| `lit` 列（**含三方 CAST 差异**） | ✓ | ✓ | ✓ |
| `orIgnore`（**含 mysql 走 PK 分支**） | ✓ | ✓ | ✓ |
| `fromSelect` | ✓ | ✓ | ✓ |
| 表别名（**含 tenant 注入用别名**） | ✓ | ✓ | ✓ |

### 8.2 对抗性用例（按「新功能要配对抗性探测用例」惯例）

**守卫类（blocker 级，缺一不可）**：

1. `db.table("users as u")` 中 users 属别模块 → **仍报 ownership 错**（守卫未被别名绕过）；
2. `db.table("tenant_tbl as t")` → **生成的 SQL 仍含 tenant 条件**（租户未被跳过）；
3. `db.table("t as a").join("t", [...], "left").as("b")`（自 join）→ 放行；同名别名 → 拒；
4. `fromSelect` 内层 → **内嵌 SELECT 含 `<alias>.tenant_id = ?`**；
5. insert 列清单缺 tenant_id → 引擎补齐，**且拒绝由内层 select 提供 tenant_id**；
6. `fromSelect` 深度 5 层 → Err（`REQ_NEST_MAX`）；
7. `fromSelect` 源为 `verb:"delete"` → Err（不得静默渲染成 SELECT）；
8. 别名 + join → 断言每条 ON 用的是**该 join 自己的别名**（非基表别名）；

**能力类**：

9. `escape: "\\"` → 拒；`escape: "'"` → 拒；`escape: ""` / `"!!"` → 拒（长度）；
10. 三个语义 op 的 `%` / `_` 注入企图：`kw = "a%b_c!d"` → 断言模式串被转义；
11. join 值条件用 `op:"contains"` → 拒；
12. `Op::In` 空数组 → 拒；`whereIf([], ...)` → 跳过；
13. `select(["*", {lit:"x", as:"y"}])` → 拒；`select(["*"])` + join → 拒；
14. `orIgnore` + `returning` on mysql → 拒；mysql 无主键表 + `orIgnore` → 拒；
15. `fromSelect` 列数不符 → 拒；CTE 列数不符 → 拒；
16. update/delete 带别名 → 拒；
17. **别名 + 全列展开**：断言渲染出的是 `<alias>.col` 而非 `<真名>.col`（`query.rs:1303`）；
18. **BigInt**：join on 的 value 传 `toBigInt()` 结果 → 断言 `$oj$i64` 标记未丢
    （`encodeParams` 覆盖）；`{lit: BigInt}` 同上；
19. `orIgnore` 三方 affected-rows 契约值（§5.4）。

### 8.3 回归

`cargo test --release --workspace` 全量。既有构造器用例集中在 `query.rs:2200-2900`
的 `#[cfg(test)]` 段，含 `unknown column` / `requires where` 文案断言
—— **新增能力不得改动既有文案**。

---

## 9. 文档与发布

按 CLAUDE.md 红线，devkit **四件**逐条对齐。**体例订正**（产品评审 m2）：

> 构造器内容全在 `api-manual.md` 的 `### db / DB(name)` 下，体例是
> 「散文 + 代码块 + 破折号要点」，**没有局部错误表**；
> 全书错误表在「13. 安全红线与已知限制」与「v0.2 已知限制全表」（`api-manual.md:3973` / `4010`）。

落点：

- **`api-manual.md`**
  - op 清单：改 `api-manual.md:1143` **单行**，加入 `contains` / `startswith` / `endswith`；
    `escape` 字段标注为「高级 / 框架内部」，不在主行展示；
  - **订正**：`api-manual.md:1143` 的 `isnull` 拼写（下游据此写错过代码）；
  - **订正**：`api-manual.md:1356`「union requires explicit columns」——下游把全列读取
    搬进 union 会撞墙，补一句；
  - join / select / insert 各自要点块追加（值条件、`lit`、`orIgnore`/`fromSelect`、表别名）；
  - `select *` 说明写在 `api-manual.md:1146` 红线要点旁（**不新建小节**）；
  - §6 全部文案进 `api-manual.md:4010` 全表；
  - **引导**：`case` 小节（1360）讲完再提其他，避免 `expr` 式捷径心理（已砍，无需此条，
    但保留「优先用结构化形态」的引导语）。
- **`SKILL.md`**：
  - **改写**第 117 行现有条目 —— 它当前正在教人手写 `ESCAPE '!'` 转义，
    与新方案直接打架，**必须改写而非新增**；
  - mysql `orIgnore` 走 PK 不用 `INSERT IGNORE`；`orIgnore` 的 mysql affected-rows = 匹配行；
  - 没有 `leftJoin`，用 `.join(..., "left")`。
- **`scenarios.md`**：模糊搜索、通知合并读（别名 + join 值条件 + `case` + `lit`）、
  幂等回执、`whereIf` 动态过滤。
  > **注意**：`scenarios.md` 已存在两个「场景 8」（644 / 693），新增前先定编号。
- **`README.md`**：版本行；
- `CHANGELOG.md` v0.1.81；`oj/Cargo.toml` 0.1.80 → 0.1.81（`Cargo.lock` 同步）；
- `cargo xtask build` 归置 `bin/devkit/`，由 `cargo test --release -p xtask` 的
  devkit 契约用例校验产物与源一致。

---

## 10. 分批与登记待做

实现分批（仍是一次发布）：

- **B0（前置，安全）**：§3.0 `TableRef` 归一化 + §3.5 `child_reqs()` 统一递归；
- **B1（低风险）**：#1 语义化 op + `escape` 逃生舱、#6 `whereIf` + `In` 空数组守卫、#7 `select(["*"])`；
- **B2（中风险）**：#8 表别名（**仅 select/join**）、#2 join ON 值条件；
- **B3（高风险）**：#3-a `lit`、#4 `orIgnore` / `fromSelect`。

登记待做（本次不做，单独立项）：

- **结构化表达式入口 `{calc:"<fn>", args:[{col}|{lit}]}`** —— 若将来出现 `expr` 的真实需求，
  **以结构化形态做，不做模板串**（架构评审 M2）。本次砍 `expr` 的前提就是这条退路存在；
- `db.query` / `db.exec` 的下架或 lint 规则（PR §4 已排除）；
- `where(rawFragment, params)` 逃逸舱（§1 非目标）；
- update/delete 的别名支持（受 mysql 版本约束，§1.1）；
- 窗口函数 frame、right join、intersect/except（既有取舍）；
- having 引用 `lit` / `case` / `window` 派生列（`agg_aliases` 只登记 Agg，`query.rs:1294`）；
- CTE/union 的别名集合一致性（本次只做列数，§3.7）。

---

## 11. 给下游（模板侧）的迁移建议

1. **没有 `leftJoin`，用 `.join(table, on, "left")`**。提案里的 `.leftJoin(...)` 会 `TypeError`。
2. **表别名用 `.as("b")`**（如 `db.table("notifications_broadcast").as("b")`），
   `"x as b"` 字符串形态虽可解析，但文档主推 `.as()`。
3. **`rate.ts:78` 全表清空不豁免**。请确认语义后二选一：
   - 若是「重置**该主体**的失败计数」→ `delete().where({field:"<主体键>", op:"eq", value:key})`
     **今天就能表达，PR 第 5 条可直接关闭，无需豁免**；
   - 若确为「清空整张表」→ 保留 `db.exec`，且**建议改走 `tasks` cron 的周期清理**，
     不要塞进登录请求（每次登录全表扫描 + 不可预测写放大）。
   > 初稿曾直接建议按 `expire_at` 清理，产品评审指出该列是否存在未核实，且会在登录
   > 路径引入写放大。改为上述两条分支请下游确认。
4. **#6 是误报**：动态 WHERE 可多次 `.where()` 或用新增的 `whereIf`；请删除对应 `保留 db.query` 注释。
5. **#7 只需 `.select(["*"])`**（本次实现）；省略 `.select()` 也等价。
6. **`escape` 不用手写**：优先用 `contains` / `startswith` / `endswith`，引擎内部统一 `ESCAPE '!'`。
7. **notify.ts 的计算列用 `case` 写**（§0.1），注意 op 拼写是 **`isnull`** 不是 `isNull`。
8. **`orIgnore` 的返回值三方言不一致**（§5.4），不要写 `if (n === 1)` 判定幂等。
9. 请核对模板侧 vendored 的 oj 版本与 devkit 副本（§0.3）。

---

## 12. 评审意见与处置

四路评审（工程师 / 架构师 / 产品 / 安全）共报 7 blocker、10 major。以下为逐条处置。

### Blocker

| ID | 意见 | 处置 | 理由 |
|----|------|------|------|
| R-1（安全 M2 + 架构 M2 + 产品 m1 + 工程师 B1） | **砍掉 `expr` 模板** | **采纳** | ① PG 下 `?` 不是占位符（`backend/postgres/query.rs:35-37`），机制不成立；② 字符白名单挡得住注入、挡不住 `pg_sleep(60)` / `load_file(concat(char(47)...))` → 我原本「连接账号即边界」的论断**被证伪**；③ 唯一真实需求（`is_read` 计算列）用现成 `case` 即可满足（§0.1）；④ 模板串进下游代码与 `toJSON()` 快照后成永久兼容面，迁移不可逆。保留 §10 的结构化 `{calc}` 退路 |
| R-2（安全 B1 + 架构 B2 + 工程师 B3） | 表别名会让 `guard_req` + `apply_tenant` **同时静默失效** | **采纳，且提升为前置 B0** | 三路独立收敛。`owner_of("t as b")` → None → judge `Ok` → 归属守卫消失；`scoped("t as b")` → None → 租户注入跳过。**照搬架构方案：引入 `TableRef` 类型 + 入口一次归一化**，不用散落 `parse`（§3.0）。采纳 `.as("b")` 为主入口 |
| R-3（工程师 M3） | update/delete 别名的方言风险（mysql `DELETE FROM t AS b` 需 8.0.16+） | **采纳：别名只放行 select/join**，其余 → Err | fail-fast 优于静默产出错误 SQL。登记为待做 |
| R-4（安全 B2 + 架构 B1） | `from_select` 是第四条递归边，三个 walker 全漏 → 跨租户读写、归属绕过、无深度上限 | **采纳，提升为前置 B0** | 最高风险点。抽 `child_reqs()`，三 walker 统一（§3.5）。insert 的 tenant_id **禁止由内层 select 提供** |
| R-5（产品 B1 + 工程师 M1） | `insert(["col",...])` 列清单形态不存在（数组=多行 + `reject("insert","columns")`） | **采纳：改 `fromSelect(q, cols)`** | 消除 `insert` 重载歧义，且少一个 API 面 |
| R-6（产品 B2） | `leftJoin` 不存在 | **采纳（订正文档 + API）** | 本次**不新增** `leftJoin` 糖（保持 API 面最小），改用 `.join(...,"left")`，§11 告知下游 |
| R-7（产品 M5 + 安全 M3 + 工程师 minor） | `select(["*"])` 当前根本不支持，§4.3 是悬空约束 | **采纳：本次实现展开 + 两条约束** | 显式 API 才是下游要的；带 join 时拒绝（防业务拿 `undefined` 做授权判断） |

### Major

| ID | 意见 | 处置 | 理由 |
|----|------|------|------|
| M-1（产品 M1 + 工程师 minor） | `Op` 是 `rename_all="lowercase"` 不是 camelCase；初稿 `isNull` 拼错 | **采纳订正** | 误改全局 attr 会破坏 `isnull`/`eq`。新 op 一律小写；§0.1 示例改为 `isnull` |
| M-2（产品 M2） | `whereIf` 未排除空数组 → `IN ()` | **采纳** | 引擎侧加 `Op::In` 空数组守卫 + `whereIf` 排除 `[]`，双保险 |
| M-3（产品 M2） | `fromSelect` 无列数校验 → 静默写错列 | **采纳** | 加列数一致性 + 文案（§6） |
| M-4（安全 M4 + 工程师 M2） | `orIgnore` 三方 affected-rows 不一致；mysql 上 `returning` 值不可定 | **采纳** | 三方值进契约测试 + mysql 下 `orIgnore+returning` 直接 Err + 文档写明 |
| M-5（产品 M4） | 初稿对 `rate.ts` 的 `expire_at` 建议在产品上不成立 | **采纳：改为两条分支** | 该列是否存在未核实；且会在登录路径引入写放大。见 §11.3 |
| M-6（安全 M1 + 工程师 B2） | `cust_with_values` 数量不匹配 = **panic**（非静默）；PG `$0` underflow | **采纳（随 R-1 一并消失）** | `expr` 已砍，该路径不再引入。证据保留在 §5.3 备查 |
| M-7（架构 m3） | CTE 内层输出列与声明 columns 零一致性校验 | **采纳** | §3.7，顺带补 union 别名集合比对 |
| M-8（安全 m5） | tenant 注入改别名后可能误用基表别名 → 条件恒真 | **采纳** | §4.5，断言精确到「每条 ON 用自己别名」+ 自 join 用例 |
| M-9（工程师 m4） | 模板里手工拼列名不按方言加引号 → mysql 下被当字符串常量 | **随 R-1 消失** | 同 R-1 |
| M-10（产品 m2） | §9 文档体例对不上（无局部错误表，全书表在 3973/4010） | **采纳** | §9 落点全部改到具体小节与行号 |

### Minor / nit（采纳）

- `escape` 加 `len() == 1` 校验（安全 m6）→ §2.1；
- `bootstrap.js` 的 `join` on 值条件与 `select` 对象形态须过 `encodeParams`
  （BigInt 标记丢失）→ §3.2 / §3.3；
- `Op` 加 `fn class()`，三处消费者按 class 判定（架构 m1）→ §3.1；
- 别名过 `check_alias` + 排除与 CTE 名冲突（架构 nit）→ §2.5；
- `Op::In` 空数组守卫 → §2.6；
- `scenarios.md` 已存在两个「场景 8」，新增前先定编号 → §9；
- SKILL.md:117 现有条目教人手写 ESCAPE，**必须改写而非新增** → §9。

### 不采纳

| 意见 | 不采纳理由 |
|------|-----------|
| 架构 M2 建议本次同时落地结构化 `{calc}` 入口 | 无下游真实代码举证（PR 提案里一条算术/日期函数需求都没有）。登记为 §10 待做，作为砍 `expr` 的退路 |
| 产品建议 `insertCols([...])` 与 `fromSelect` 拆两个方法 | 合为 `fromSelect(q, cols)` 少一个 API 面，且能消除 `insert` 的数组重载歧义 |
| 产品 m3 建议彻底移除 `escape` 逃生舱 | 保留但降级为「高级 / 框架内部」，不在 op 清单主行展示。给框架内部与迁移期留出口，同时避免诱导业务照抄自转义 |

---

## 13. `expr` 砍除声明（用户确认 2026-10-19）

**本版不做 `expr` 任意表达式模板。**这是一项被评审推翻、并由用户确认的**减法**，
而非遗漏。记录在此，供后续任何人提出「为什么构造器不支持表达式列」时直接引用，
不必重新论证。

### 13.1 它被砍的四条理由（各自独立成立）

1. **三方言实现不成立**：PG 占位符是 `$N` 而非 `?`（`backend/postgres/query.rs:35-37`），
   模板里写 `?` 在 PG 下原样落进 SQL、值完全不绑定；模板里手工拼的列名不按方言加引号，
   mysql 反引号下 `"r"."read_time"` 被当字符串常量 → 比较恒真（静默错值）。
2. **黑名单守不住**：字符集白名单禁掉 `'` 后拼不出字面量，但 `chr(47)||chr(101)||…`
   仍可拼串 → `pg_sleep(60)` / mysql `sleep(60)` / `load_file(concat(char(47),…))`
   全部合法。设计稿原论断「连接账号即边界」被证伪——连接账号通常就是业务账号。
3. **无真实需求举证**：下游 PR 提案 7 条里，没有一条算术/日期函数类需求；
   唯一的 `is_read` 计算列用现成 `case` 即可表达（§0.1）。
4. **迁移不可逆**：模板串一旦写进下游代码与 `toJSON()` 快照，即成永久兼容面，
   而我们从不记录其结构，未来结构化改造无自动迁移路径。

### 13.2 砍掉之后，这些需求怎么满足

| 需求 | 本版做法 |
|------|---------|
| `'broadcast' as scope`（字面量列） | **新增 `lit`**（§2.3），语法上无法拼出标识符 |
| `(x IS NOT NULL AND x <> '') as is_read`（布尔计算列） | **用现成 `case`**（§0.1），零新增 API |
| 算术 / 日期函数列 | **暂时不支持**；登记为 §10 待做，未来以结构化 `{calc}` 形态落地 |

### 13.3 结论

`expr` 的退路是**结构化的**，不是**更严的黑名单**。若将来出现真实需求，
以 `{calc:"<fn>", args:[{col}|{lit}]}` 形态实现（§10），
不重开模板字符串。在此之前，`lit` + `case` + `agg`/`window` 是构造器列表达式的全部形态。
