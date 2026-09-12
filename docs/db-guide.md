# db 新人手册 —— 配置 + JS 全量数据访问

> 面向第一次写 oj handler 的新人。目标：读完能独立完成「配库 → 建表 → 增删改查 → 事务」，
> 并理解每一步背后的安全边界。
>
> 本手册讲「怎么上手、为什么这样设计」。逐 API 的穷举式参考见
> [`docs/devkit/api-manual.md`](devkit/api-manual.md) 第 6 章「db / DB(name)」。
> 所有 API 名、字段名、报错文案均与源码（`src/bridge/bootstrap.js`、`src/bridge/query.rs`、
> `src/bridge/db.rs`）逐字核对，版本 0.1.14。

---

## 0. 五分钟上手

### 1) 配一个库

`config.yaml`（与 `oj server -c` 指向的目录同级）：

```yaml
db:
  default: "sqlite://db.sqlite"   # 键 = 库名，值 = DSN
```

DSN 支持 `sqlite://`、`mysql://`、`postgres://`，可混用（见 [§1](#1-db-配置)）。

### 2) 声明表（白名单的源头）

模块目录下 `schema.yaml`（例：`src/user/schema.yaml`）：

```yaml
tables:
  account:
    pk: id
    columns:
      id: { type: integer, autoincrement: true }
      name: { type: text, null: false }
      role: { type: text, null: false }
```

同时在该模块的 `manifest.yaml` 里声明拥有它（`tables: [account]`）。
**没有进 schema.yaml 的表/列，JS 侧一律摸不到**——这是整座安全模型的根。

### 3) 写第一个 handler

`src/user/account/api.ts`：

```ts
function get(): void {
  db.table("account")
    .select(["id", "name"])
    .where({ field: "role", op: "eq", value: "admin" })
    .orderBy([{ field: "id", dir: "desc" }])
    .limit(10)
    .all()
    .then((rows) => json.ok(rows))
    .catch((e) => json.fail(500, String(e)));
}

export const get_route = "{id}";   // 可选：路径参数路由
export { get };
```

handler 也支持 **async 写法**——`db.*` 全部返回 Promise，运行时会等 Promise 落定再捕获
信封（sample 的 admin 模块即此风格，多数人觉得更好读）：

```ts
async function get(): Promise<void> {
  try {
    const rows = await db.table("account")
      .select(["id", "name"])
      .where({ field: "role", op: "eq", value: "admin" })
      .orderBy([{ field: "id", dir: "desc" }])
      .limit(10)
      .all();
    json.ok(rows);
  } catch (e) {
    json.fail(500, String(e));
  }
}

export const get_route = "{id}";
export { get };
```

两种写法等价，选一种贯穿整个模块即可。async 的额外好处：多个 db 调用按顺序
`await`（读起来像同步代码），整个函数共享一个 `try/catch`，不用每步 `.catch`。
事务（§4）的 `db.tx(async (tx) => {...})` 只能用 async 写。

### 4) 跑起来

```bash
./bin/oj server -c sample/config.yaml --api-path sample/src
curl "http://localhost:9778/v1/api/user/account/?role=admin"
```

返回 `{code:0, msg:"ok", data:[...行数组]}`。就这么多——下面逐层展开。

---

## 1. db 配置

### 1.1 `db:` 段 —— 命名库

```yaml
db:
  default:   "sqlite://db.sqlite"
  analytics: "mysql://user:pass@127.0.0.1:3306/app"
  warehouse: "postgres://127.0.0.1:5432/app"
```

- 键 = **库名**，值 = DSN。方言由 DSN scheme 决定：`sqlite://` / `mysql://` / `postgres://`。
- JS 侧 `db` 就是 `DB("default")`；其他库用 `DB("analytics")` 取。名字不存在时
  `DB(name)` 返回 `undefined`（`op_db_has` 探测）。
- **模块默认库重定向**：模块 `manifest.yaml` 可写 `db: warehouse`，此后该模块里的
  字面 `db.*`（即 "default"）自动落到 warehouse；显式 `DB("...")` 不受影响。

### 1.2 数据库插件

方言能力由 cdylib 插件提供：`oj-db-mysql`、`oj-db-postgres`（sqlite 内建）。
装配走 `config.yaml` 的 `plugins:` 段，一段三用：

```yaml
plugins:
  oj-db-mysql: {}          # 值 = 空对象：装配插件，配置回落轴适配器
  # oj-es: { endpoint: ... }  # 值 = 非空对象：原样透传给插件 cfg
  # 缺省/空 map = 扫描模式：加载插件目录下全部插件
```

插件发现路径（先到先得）：`OJ_PLUGINS_DIR` 环境变量 > config 的 `plugins_dir` >
`<exe>/plugins` > `<workspace_root>/bin/plugins/<host-triple>/`。
用 `cargo xtask build` 会把插件归置到 `bin/plugins/`。

### 1.3 `schema.yaml` —— 白名单与建表

- **列白名单**：表/列只有声明进 schema.yaml，构造器与归属守卫才认。
  列类型最小集：`integer | bigint | text | boolean | double | blob`。
- **安全前向自动收敛**（`migrate_on_start: auto`，dev 默认）：加表、加可空列、加索引
  会在启动/reconcile 时自动应用；**类型变更、删列、改名必须走 `migrations/` 手写迁移**
  （release 默认 `verify`：账本落后拒启，先 `oj migrate`）。
- 同一张表被两个模块声明 → 装配 fail-fast（S002）。
  `manifest.yaml` 的 `tables:` 清单与 schema.yaml 双向一致（S005，`oj build` 内嵌检查）。

### 1.4 表归属守卫（模块间）

schema.yaml 声明使表「归属」其模块。跨模块访问需要显式依赖：

```yaml
# src/order/manifest.yaml
deps: { user: "^0.1.0" }   # 声明后 order 模块可读 user 模块的表
```

`server.ownership_guard` 控制处置：`warn`（默认，仅告警）/ `deny`
（拒绝执行，报错文案附修复指引）。未声明归属的表不设防。
守卫覆盖三条路：构造器（`db.table()` 按表精确判）、裸 SQL（提取表名逐表判）、
以及构造器条件里嵌套的子查询/exists/union 成员/CTE 内部（递归全查）。

### 1.5 多租户（与 db 的关系）

```yaml
tenant:
  enable: true
  header_key: "X-TENANT-ID"   # 缺失 → 400
```

启用后请求头值注入 `http.tenantId`。oj **不在 SQL 层自动拼租户条件**——过滤靠你的
where（构造器条件对象自带 `.has("tenant_id")` 检查，见 [§5.3](#53-检查与内省)）。

---

## 2. `db` 全局：方法面总览

`db === DB("default")`，每个命名库同构：

| 成员 | 签名 | 返回 | 用途 |
|---|---|---|---|
| `query` | `query(sql, params?)` | `Promise<行数组>` | 原生参数化查询 |
| `exec` | `exec(sql, params?)` | `Promise<受影响行数>` | 原生参数化执行 |
| `table` | `table(name)` | 查询构造器 | 安全构造器（本手册主角） |
| `fromJSON` | `fromJSON(snapshot)` | 查询构造器 | 从 toJSON 快照恢复 |
| `tx` | `tx(async (tx) => {...})` | `Promise<fn 返回值>` | 事务 |
| `leaf` / `and` / `or` / `not` | 工厂 | 条件对象 | 条件组合（§5.2） |

---

## 3. 原生参数化查询（query / exec）

适合已有 SQL、聚合报表、构造器不覆盖的语句：

```ts
const rows = await db.query("select id, name from account where role = ?", ["admin"]);
const n = await db.exec("update account set role = ? where id = ?", ["user", 7]);
```

- **值只经 `?` 占位符绑定**，绝不允许字符串拼接——这是红线（SQL 注入），不是风格建议。
  方言占位符：sqlite/mysql 用 `?`，postgres 用 `$1, $2`。
- `params` 可省略（无参便捷形式）。
- 裸 SQL 也过归属守卫：SQL 里出现的表必须是你模块拥有的或已声明 deps 的。
- 表名/列名无法参数化——动态标识符请改用 `db.table()` 构造器（白名单）。

---

## 4. 事务

```ts
const out = await db.tx(async (tx) => {
  await tx.exec("insert into account (name, role) values (?, ?)", ["neo", "user"]);
  const rows = await tx.table("account").where({ field: "name", op: "eq", value: "neo" }).all();
  if (rows.length === 0) throw new Error("gone");   // throw/reject → 自动回滚
  return rows;
});                                                  // 正常 resolve → 自动 commit
```

- `tx` 回调拿到的对象与 `db` 同构（query/exec/table/fromJSON + 条件工厂），
  全部路由到**同一连接**上。
- 事务期间（包括构造器 `.run()/.all()`）同库操作自动骑到事务会话；
  **碰别的库**报错：`transaction active on db 'x' (finish it before touching db 'y')`。
- 不支持嵌套：`transaction already active (nested tx not supported)`。

---

## 5. 查询构造器：从最小查询到组合

一行心智模型：**`db.table(表)` 起链 → 链上每个方法只修改请求快照 → `.all()/.run()` 终执行**。
构造器完全不可变副作用——方法返回同一 builder 便于链式，但内部 req 是逐步填充的普通对象。

### 5.1 基础查询

```ts
// 全列
const rows = await db.table("account").all();

// 选列 + 条件 + 排序 + 分页
const rows = await db.table("account")
  .select(["id", "name"])
  .where({ field: "age", op: "gte", value: 18 })
  .orderBy([{ field: "id", dir: "desc" }])   // dir 省略 = asc
  .limit(10).offset(20)
  .all();
```

要点：

- **隐式 LIMIT 100 只加在顶层查询**；显式 `limit(n)` 一律生效并被 clamp 到 **1000**
  （`LIMIT_MAX`）。子查询/union 成员不会被隐式截断。
- 多次 `.where()` 条件之间 **AND**（`.having()` 是整体替换，单树）。
- `.orderBy` 的列必须是表/join/CTE 白名单里的**真实列**（聚合别名不行，见 §8）。

#### 调试：`.toSQL()`

```ts
const { sql, params } = db.table("account")
  .where({ field: "id", op: "in", value: [1, 2, 3] }).toSQL();
// sql: SELECT ... FROM "account" WHERE "id" IN (?, ?, ?)   params: [1,2,3]
```

同步返回 `{sql, params}`（按配置库的方言渲染），**只构造不执行**，和 `.all()` 走完全相同的
校验管线。写不出来或报错看不懂时，先 toSQL 看。

### 5.2 条件：对象形式与工厂

叶子：`{field, op, value}`。`op` 全集（类型化枚举，拼错直接报）：

| op | value 要求 |
|---|---|
| `eq` `ne` | 任意 JSON 值（省略 = NULL 比较） |
| `gt` `gte` `lt` `lte` | 必填（`gt needs value`） |
| `in` | 必须数组（`in needs array value`） |
| `like` | 必填（模式串） |
| `isnull` | 不需要 value |

组合树：`{and: [...]}` / `{or: [...]}` / `{not: 叶子}`。手工写嵌套树容易错，推荐工厂：

```ts
const { leaf, and, or, not } = db;          // 每个命名库（含 tx 回调）都带这组工厂

const cond = and(
  leaf("age", "gte", 18),
  or(leaf("role", "eq", "admin"), not(leaf("tag", "eq", "hidden"))),
);

db.table("account").where(cond).all();
```

- 工厂产物是**不可变**条件对象：`.and(c)` / `.or(c)` / `.not()` 返回新对象，不改原树。
- `.where()` 入参三种通吃：普通对象树、条件对象（自动 `.tree()` 解包）、
  **另一个构造器**（自动取快照作为子查询，见 §9.1）。

### 5.3 检查与内省

条件对象自带内省（适合「租户字段必须存在」这类守卫）：

```ts
const c = and(leaf("tenant_id", "eq", http.tenantId), leaf("ok", "eq", 1));
c.fields();        // ["tenant_id", "ok"] —— 树内全部字段（去重）
c.has("tenant_id"); // true
if (!c.has("tenant_id")) return json.fail(403, "tenant required");
```

---

## 6. join 联表

```ts
const rows = await db.table("order")
  .join("user", [{ left: "order.user_id", right: "user.id" }], "left")
  .select(["order.id", "user.name"])
  .all();
```

- `kind`：`"inner"`（默认）/ `"left"`。**right join 不做**。
- `on` 是**列对列等值**数组 `{left, right}`——没有 op 字段（多余键直接报错）。
- 限定名 `"表.列"`：表段 ∈ {基表, join 表, CTE 名}；**非限定列只解析基表**
  （两表同名列天然无歧义）。列不存在：`unknown column 'x' in join on`。
- **自 join 拒绝**：`self join not supported (no table alias)`；on 不能为空：
  `join 'x' needs non-empty on`。
- join 表同样过归属守卫。select 省略 columns 时展开**基表**全列（带 join 时自动限定）。

---

## 7. DML：insert / update / delete

动词由 `.insert()/.update()/.delete()` 声明，**`.run()` 终执行**（带 JS 侧预检）：

```ts
// insert：单对象或行数组；返回受影响行数
await db.table("account").insert({ name: "neo", role: "user" }).run();
await db.table("account").insert([{ name: "a" }, { name: "b" }]).run();

// update / delete：必须带 where（叶子数 ≥ 1），返回受影响行数
await db.table("account").update({ role: "admin" })
  .where({ field: "id", op: "eq", value: 7 }).run();
await db.table("account").delete()
  .where({ field: "id", op: "in", value: [8, 9] }).run();
```

动词×字段**兼容矩阵在 op 侧权威校验**（`fromJSON` 也绕不过）：

| 动词 | 允许 | 拒绝（报 `X does not accept Y`） |
|---|---|---|
| select | where/joins/columns/groupBy/having/orderBy/limit/offset/distinct/unions/with | values, sets |
| insert | values（≥1 行） | where, orderBy, limit/offset, joins, distinct, groupBy, having, unions, with, columns |
| update | sets（非空）+ where（≥1 叶子） | joins, columns, distinct, groupBy, having, unions, with, limit/offset |
| delete | where（≥1 叶子） | 同 update + sets |

insert 行数组必须**键集完全一致**（`insert rows must share identical key sets`）；
键/sets 逐一过白名单（`unknown column 'k' in insert values` / `in update sets`）。
update/delete 无 where 在 JS 侧提前抛（`update requires where`），绕过 JS 层时 op 侧
兜底（`Update requires where (leaf count >= 1)`）。

**返回形态**：select → 行数组；insert/update/delete → 受影响行数（number）。
同一 `resolve_target` 路由：有活跃事务走会话，否则走池——DML 与查询天然同事务。

---

## 8. 聚合 / 分组 / having / distinct

```ts
const rows = await db.table("account")
  .select([
    "role",                                        // 普通列（字符串）
    { fn: "count" },                               // count(*)，别名可省
    { fn: "avg", field: "age", as: "avg_age" },    // 聚合 + 别名
  ])
  .groupBy(["role"])
  .having(leaf("avg_age", "gt", 20))               // ← 引用聚合别名，自动展开
  .all();
```

- `fn` 全集：`count | sum | avg | min | max`。**只有 count 允许省略 field**
  （其余报 `aggregate needs field (only count allows omission)`）。
- 别名形状：ASCII 字母/下划线开头（数字不能开头），违者 `illegal alias 'x'`。
- `.having()` 引用列优先按真实列解析，未命中查**聚合别名台账**展开——所以 having 里
  可以直接写 `avg_age`（PG 不允许 HAVING 引用输出别名，展开消灭了方言分叉）。
  两者都不是：`unknown column 'x' in having`。
- `.distinct()` 去重。`orderBy` 不认聚合别名（只解析真实列），排聚合结果请子查询或
  在 SQL 侧处理。
- case/window 列也走 `.select([...])` 对象形式，见 §9.3。

---

## 9. 进阶构造（Phase 8）

### 9.1 where / having 子查询 + exists

叶子把 `value` 换成 `subquery`（或顶层用 `exists`）——值与子查询**互斥**：

```ts
// IN (SELECT ...)
db.table("order").where({
  field: "user_id", op: "in",
  subquery: db.table("user").select(["id"]).where(leaf("role", "eq", "admin")),
}).all();

// EXISTS：条件树槽位直接放 {exists: <req>}（对象或构造器均可）
db.table("user").where({
  exists: db.table("order").select(["id"]).where(leaf("amount", "gt", 100)),
}).all();
```

注意：子查询是**自包含**的——不能引用外层查询的列（没有相关子查询/外列引用机制）。
「存在订单金额大于 100 的用户」这类语义如需按行关联，用 **join** 表达。

规则：

- 子查询 op ∈ `in/eq/ne/gt/gte/lt/lte`；`like does not accept subquery`、
  `isnull does not accept subquery`、`value and subquery are mutually exclusive`。
- 嵌套层数 ≤ **4**（`REQ_NEST_MAX`，超限 `subquery: nested select too deep`）。
- 嵌套 req 只能是 select（`nested select must be select`）且**禁带 with/unions**
  （`nested select does not accept with/unions (v1)`）。
- 子查询/exists 同样支持写在 **having** 叶子里，且递归过归属守卫。

### 9.2 UNION / UNION ALL

```ts
db.table("user")
  .select(["id", "name"])
  .union(db.table("account").select(["id", "name"]), "all")   // "distinct"(默认) | "all"
  .all();
```

- 基查询与成员都必须**显式 select 列**（`union requires explicit columns`），
  列数一致（`union column count mismatch: 2 vs 3`）。
- 成员禁排序/分页：`union member does not accept order_by/limit/offset`。
- Intersect / Except 不做（旧版 mysql 不支持）。

### 9.3 CASE 列与窗口函数列

都挂在 `.select([...])` 的对象形态上：

```ts
db.table("account").select([
  "name",
  {
    case: {                                     // searched case；then/else 只能是值
      when: [
        { cond: leaf("age", "lt", 18), then: "minor" },
        { cond: leaf("age", "gte", 60), then: "senior" },
      ],
      else: "adult",
    },
    as: "age_group",
  },
  {
    window: { fn: "row_number", partition_by: ["role"], order_by: [{ field: "id", dir: "desc" }] },
    as: "rn",
  },
]).all();
```

- 窗口 `fn`：`row_number | rank | dense_rank`（frame 不做）。
- `when` 非空（`case needs non-empty when`）；then/else 走绑定参数，**不能是标识符**。
- partition_by / order_by 列过白名单（site：`window partition_by` / `window order_by`）。

### 9.4 CTE（非递归 WITH）

```ts
const adults = db.table("account")
  .select(["id", "name"]).where(leaf("age", "gte", 18));

db.table("adults")
  .with("adults", ["id", "name"], adults)     // name + 声明列 + 成员查询
  .select(["id", "name"])
  .where(leaf("name", "like", "a%"))
  .all();
```

- `columns` **必填**：CTE 输出列就是后续引用它的白名单（`cte needs non-empty columns`）。
- CTE 名不查 schema 注册表（虚拟表无归属），但名字与列名过别名形状校验；
  **WITH 名遮蔽同名真实表**（SQL 语义）。
- 成员查询过嵌套约束（仅 select、禁嵌套 with/unions）。
- WITH 校验先于主语句表解析——基表直接用 CTE 名的写法合法。

---

## 10. 序列化：toJSON / fromJSON

```ts
const snap = db.table("account")
  .select(["id", "name"])
  .where(leaf("role", "eq", "admin"))
  .toJSON();                                  // 纯 JSON 快照（可入库/入队列/跨网络）

const rows = await DB("analytics").fromJSON(snap).all();   // 在另一个库上恢复继续链
```

- 快照是**不透明 req 对象**：`db` 字段只是创建时的 JS 可见名，恢复时统一重指到
  调用 `fromJSON` 的那个库。
- 快照恢复**完全绕过 JS 链层**——所有安全校验在 op 侧重复生效（动词矩阵、白名单、
  归属守卫），不要试图从快照夹带私货。
- 典型用途：把查询存表、由别的模块/服务恢复执行、审计。

---

## 11. 边界与红线（新人必读）

1. **动态标识符只来自白名单**。表名/列名绝不允许来自请求拼 SQL——注入防线的根。
   构造器里一切标识符都过 SchemaRegistry（或 CTE 声明列）；裸 SQL 的标识符只能写死。
2. **值只走绑定参数**。构造器值、`?`/`$n` 占位符；永不字符串拼接。
3. **update/delete 必须带 where 且叶子 ≥ 1**——防全表误伤，JS 与 op 双层强制。
4. 数量级：条件树深度 ≤ 8、叶子 ≤ 64；嵌套 select ≤ 4 层；limit 显式 ≤ 1000。
5. **多写 .toSQL()**：与执行同管线，报错前先看它渲染出了什么。

### 常见报错速查

| 报错（逐字） | 原因 | 修法 |
|---|---|---|
| `unknown table 'x'` | 表未进 schema.yaml | 声明表并检查模块装配 |
| `ownership: 表 "x" 属于模块 "y"…`（deny 模式） | 跨模块访问未声明依赖 | manifest `deps: [y]` 或改契约调用 |
| `unknown column 'x' in <site>` | 列不在白名单 / 未限定 | 补 schema；join 场景用 `表.列` |
| `unknown table 'x' in <site>` | 限定名表段不是基表/join/CTE | 检查限定名拼写 |
| `insert needs at least one row` | `.insert()` 空参 | 传对象或非空数组 |
| `insert rows must share identical key sets` | 行数组键不一致 | 对齐每行键 |
| `update needs non-empty sets` | `.update({})` | 至少一列 |
| `Update requires where (leaf count >= 1)` | update/delete 无 where | 补 `.where(...)` |
| `<verb> does not accept <field>` | 动词×字段矩阵拒绝 | 见 §7 表 |
| `condition tree too deep (max 8)` / `too large (max 64 leaves)` | 条件树超限 | 拍平/拆分条件 |
| `in needs array value` | `op:"in"` 值非数组 | 传数组 |
| `value and subquery are mutually exclusive` | 叶子同时给了 value 和 subquery | 二选一 |
| `subquery: nested select too deep` | 嵌套 > 4 层 | 减层或拆 CTE |
| `nested select does not accept with/unions (v1)` | 子查询/union 成员带了 with/unions | 移到顶层 |
| `union requires explicit columns` | union 侧省略 select 列 | 双方显式列且列数一致 |
| `union member does not accept order_by/limit/offset` | 成员带排序分页 | 排序分页放最外层 |
| `self join not supported (no table alias)` | 同表自联 | CTE 复制一份再 join |
| `illegal alias 'x'` | 别名形状不合法 | 字母/下划线开头 |
| `cte needs non-empty columns` | `.with(name, [], q)` | 声明列清单 |
| `transaction active on db 'x' …` | 事务中碰了别的库 | 先 commit/rollback |
| `transaction already active (nested tx not supported)` | 嵌套 `db.tx` | 复用当前 tx 回调对象 |
| `db: instance 'x' not configured` | `DB("x")` 未在 config.db 声明 | 补 DSN 或改库名 |

---

## 12. 延伸阅读

- [`docs/devkit/api-manual.md`](devkit/api-manual.md) —— 权威 API 参考（§6「db / DB(name)」
  含全量字段表与更多示例；§3「模块数据层」讲 schema/migrations 全流程）。
- [`docs/dev-guide.md`](dev-guide.md) —— 内部实现走读（bridge、装配、插件）。
- [`sample/`](../sample/README.md) —— 可运行示例工程（`sample/config.yaml` +
  `sample/src/`，含 raw SQL 与构造器混用的真实 handler）。
