# db 新人手册 —— 配置 + JS 全量数据访问

> 面向第一次写 oj handler 的新人。目标：读完能独立完成「配库 → 建表 → 增删改查 → 事务」，
> 并理解每一步背后的安全边界。
>
> 本手册讲「怎么上手、为什么这样设计」。逐 API 的穷举式参考见
> [`docs/devkit/api-manual.md`](devkit/api-manual.md) 第 6 章「db / DB(name)」。
> 所有 API 名、字段名、报错文案均与源码（`src/bridge/bootstrap.js`、`src/bridge/query.rs`、
> `src/bridge/db.rs`、`src/bridge/dialect_caps.rs`、`src/bridge/db_policy.rs`）逐字核对，
> **版本 0.1.89 / ABI 12**。
> 方言能力模型（开放方言名 + 能力位）与结构化数据库错误是 0.1.88 的两处结构性变更，
> 见 §10.5 与 §10.7；库级访问控制（`db_access:` 段）是 0.1.89 的新增面，见 §11.5。
>
> 文中「fail-fast」= 发现问题立刻报错退出；「白名单」= 只有预先声明过的表/列才能被
> 访问，其余一律拒绝。

---

## 0. 五分钟上手

从零到跑通一次查询的最短路径。第一次用 oj 就按这节做。

### 1) 配一个库

`config.yaml`（与 `oj serve -c` 指向的目录同级）：

```yaml
db:
  default: "sqlite://db.sqlite"   # 键 = 库名，值 = DSN
```

DSN 支持 `sqlite://`、`mysql://`、`postgres://`，可混用（见 [§1](#1-db-配置)）。
test-only 还有 `sqlite::memory:`（真 sqlite，重启即丢）与 `memory://`（**假后端**，不执行 SQL）。

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
./bin/oj serve -c sample/config.yaml --api-path sample/src
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

- 键 = **库名**，值 = DSN。DSN scheme 只负责**选后端**（`sqlite://` 内建、
  `mysql://` → `oj-db-mysql`、`postgres://` → `oj-db-postgres`）；**方言名由后端自己声明**
  （插件 `describe()` 的 `dialect`，v0.1.88 起是开放集），不是宿主从前缀猜的。
  未知 scheme 启动即拒：`unknown db scheme in dsn '<脱敏>' (known: ["sqlite://", …])`——
  `known` 列表随**实际装上**的后端变，看不出 mysql 就是插件没装上（不是配置写错）。
- 仅测试可用的两个 DSN：`sqlite::memory:`（真 sqlite，进程退出即丢）与 `memory://`
  （`InMemoryAccessor`，**假后端**：不执行 SQL，`query`/`exec` 一律回夹具行）。因此
  `memory://` 上 `db.nextSeq` 拿不到返回值（`no value returned`）——它不是「内存版
  sqlite」，别拿它验业务 SQL。
- JS 侧 `db` 就是 `DB("default")`；其他库用 `DB("analytics")` 取。名字不存在时
  `DB(name)` 返回 `undefined`（`op_db_has` 探测）。
- **模块默认库重定向**：模块 `manifest.yaml` 可写 `db: warehouse`，此后该模块里的
  字面 `db.*`（即 "default"）自动落到 warehouse；显式 `DB("...")` 不受影响。
- **库级迁移 / 对账（v0.1.21）**：`oj migrate` / `oj test fixture` / `oj schema diff` 默认作用
  `default`，用 `--db <name>` 改指上面任一命名库（未声明即 fail-fast）。它是**整轮**的
  目标库，不认模块级 `manifest.yaml` 的 `db:` 绑定——多库部署对每个 profile 各跑一遍，
  账本 `_oj_migrations` 与 schema 收敛各库独立。详见 `docs/migration.md` §3.8。

### 1.2 数据库插件

方言能力由 cdylib 插件提供：`oj-db-mysql`、`oj-db-postgres`（sqlite 由内置 sqlx 访问器承载）。
插件在 `describe()` 里声明方言名与能力位（`placeholder_*` / `id_*` / `stream` / `cancel`），
宿主装配期严格校验，写错直接拒启——能力位全表见 §10.5，运维面（`plugins.lock.json`、
整体拷贝 `bin/`）见 `docs/ops-manual.md` §1.2。装配走 `config.yaml` 的 `plugins:` 段，一段三用：

```yaml
plugins:
  oj-db-mysql: {}          # 值 = 空对象：装配插件，配置回落轴适配器
  # oj-es: { endpoint: ... }  # 值 = 非空对象：原样透传给插件 cfg
  # 缺省/空 map = 扫描模式：加载插件目录下全部插件
```

插件发现路径（先到先得）：`OJ_PLUGINS_DIR` 环境变量 > config 的 `plugins_dir` >
打包布局（`OJ_BUNDLED_LD` 上溯两级，仅打包形态）> `<exe>/plugins` >
`<workspace_root>/bin/plugins/<host-triple>/`。
用 `cargo xtask build` 会把插件归置到 `bin/plugins/`。

### 1.3 `schema.yaml` —— 白名单与建表

- **列白名单**：表/列只有声明进 schema.yaml，构造器与归属守卫才认。
  列类型最小集：`integer | bigint | text | boolean | double | blob`。
- **安全前向自动收敛**（`migrate_on_start: auto`，dev 默认）：加表、加可空列、加索引
  会在启动/reconcile 时自动应用；**类型变更、删列、改名必须走 `migrations/` 手写迁移**
  （release 默认 `verify`：账本落后拒启，先 `oj migrate`；非 `default` 库用
  `oj migrate --db <name>`）。
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
  header_key: "X-TENANT-ID"        # 缺失/空 → 400 `missing tenant header: X-TENANT-ID`
  sql_guard: deny                  # off（默认）| warn | deny；也认 bool（true=deny / false=off）
  require_signed_claim: false      # true：租户只认验签后的 user.claims.tenant
  shared_allow: [dict, geo]        # 共享表白名单（schema.yaml `tenant: false` 才生效）
  allow_as_tenant: false           # 放行 db.asTenant(id)
  anonymous_paths: ["/v1/api/oidc/*"]   # 免租户头（OIDC 回跳带不了自定义头）
```

`enable: true` 后请求头值注入 `http.tenantId`；`require_signed_claim: true` 时改用验签
claim（`user.claims.tenant`），裸头只做一致性复核——不符 `tenant header does not match
signed claim`，缺 claim `tenant requires a signed claim (user.claims.tenant missing)`。
装配期校验：`require_signed_claim` 必须同时有 `tenant.enable=true` 与 `auth:` 段。

**SQL 层是否自动拼租户条件，由 `sql_guard` 决定**（默认 `off` = 完全不注入，
「过滤靠你的 where」只在这个默认档成立）：

| 档位 | 构造器（`db.table()`） | 裸 SQL（`db.query/exec/stream`） |
|---|---|---|
| `off`（默认） | 不注入、不校验 | 不校验 |
| `warn` | 缺失租户头 → `eprintln!` 告警后**不注入**放行；有头则照常注入 | 缺 `tenant_id` / 无租户头 → 告警放行（warn 只查「文本里有没有 `tenant_id`」，不核对绑定值） |
| `deny` | 缺失租户头 → 拒；其余同 `warn` 的注入路径但违规即拒 | 缺 `tenant_id` / 参数不含当前租户 / 碰 DDL → 拒 |

`warn` / `deny` 下构造器的注入是全自动的（`build_statement` 之前做，`toSQL()` 产物
同样已注入，可安全回跑 `db.query`）：

- `select` / `update` / `delete`：追加 `{表别名}.tenant_id = ?`（用别名限定，避免 join 歧义）。
- `insert`：强制写入当前租户值；行里已带 `tenant_id` 且不等 → `tenant guard: insert
  tenant_id mismatch (got …, want …)`；`update` 的 `sets.tenant_id` 一律拒
  （`tenant guard: update sets.tenant_id not allowed (got …)`）。
- `insert…fromSelect`：源 select 当作常量列补 `tenant_id`，`insert_cols` 显式带
  `tenant_id` → `tenant guard: insert fromSelect must not supply tenant_id`。
- join 表：租户条件进 `ON`（不是 `WHERE`）；条件树 / `having` / `case when` / 子查询 /
  `exists` / union 臂 / CTE / `fromSelect` 全部递归覆盖。
- 缺失租户头时：`tenant guard: table(s) ["t"] require tenant context (missing tenant
  header; use db.asSystem() for system tasks)`。

**哪些表算租户表**：`!shared && has_column("tenant_id")`。共享表要在 schema.yaml 标
`tenant: false`，**并且**列进 `tenant.shared_allow`——白名单为空时该标记被忽略（fail-closed，
仍按受租户约束校验 `tenant_id`）。

`sql_guard` 非 `off` 时的声明期硬约束（serve 启动 / `oj build` / `oj migrate` 三处校验）：
schema.yaml 里声明的业务表**必须有 `tenant_id` 列**（`schema: [m] 表 "t" 缺 tenant_id 列…`，
共享表双声明豁免）；`tenant_id` 列类型只能是 `text` / `integer` / `bigint`
（`double` / `boolean` / `blob` 没有等值租户 id 语义，直接拒）。注入值按**声明类型**绑：
`text` → 字符串，`integer` / `bigint` → 数值形态（小值 number、更宽 i64/u64 标记）。
数值列上租户头必须是**十进制字面量**，否则 `tenant guard: tenant id "…" is not a valid
integer for numeric column t.tenant_id`——**`warn` 档也硬失败**（warn 只放行「无法判定」，
不放行「判定为不匹配」）。

裸 SQL 是**文本级 best-effort**（清洗注释/引号后找 `tenant_id` 字样 + 参数等值），
不等于语义分析：`tenant guard: raw sql on ["t"] lacks tenant_id condition (use db.table()
builder)`、`tenant guard: raw sql params must include current tenant id ("t1") under deny
mode`；另有两条 fail-closed——DML/DDL 命中却**提取不到已登记表**
（`tenant guard: {KW} touches no registered table (cannot verify tenant scope)`）、
`TRUNCATE/ALTER/DROP/CREATE` 触碰已登记表（`tenant guard: {KW} on registered table(s) […]
not allowed under sql_guard`）——都无法注入条件，deny 拒、warn 只告警。

**两个逃生口**：`db.asSystem()`（请求级绕过、`ReqState` 重置即失效，日志留
`warn: db.asSystem() invoked (tenant guard bypass for this request)`，只给审计/运维类
handler）；`db.asTenant(id)`（匿名请求声明租户，**不绕过**防护——租户条件照常注入，只是
id 由 handler 给。三道门禁：`allow_as_tenant: true`、请求匿名且尚无租户头、`id` 非空）。
报错文案：`db.asTenant is disabled: set tenant.allow_as_tenant=true to let anonymous handlers
declare a tenant` / `db.asTenant is only allowed on anonymous requests (anonymous_paths hit,
no tenant header)` / `db.asTenant: id must not be empty`。

---

## 2. `db` 全局：方法面总览

`db === DB("default")`，每个命名库同构：

| 成员 | 签名 | 返回 | 用途 |
|---|---|---|---|
| `query` | `query(sql, params?)` | `Promise<行数组>` | 原生参数化查询 |
| `exec` | `exec(sql, params?)` | `Promise<受影响行数>` | 原生参数化执行 |
| `stream` | `stream(sql, params?, opts?)` | `Promise<void>` \| `AsyncIterable<行>` | 流式查询（逐行拉取，v0.1.37，见 §3.1） |
| `table` | `table(name)` | 查询构造器 | 安全构造器（本手册主角） |
| `fromJSON` | `fromJSON(snapshot)` | 查询构造器 | 从 toJSON 快照恢复 |
| `tx` | `tx(async (tx) => {...})` | `Promise<fn 返回值>` | 事务 |
| `leaf` / `and` / `or` / `not` | 工厂 | 条件对象 | 条件组合（§5.2） |
| `nextSeq` | `nextSeq(name)` | `Promise<number \| string>` | 平台序列取号（§11 第 7 条） |
| `listTables` | `listTables(prefix?)` | `Promise<string[]>` | 目录自省（v0.1.82；三内置方言同口径，其他方言 `OJ_DB_UNSUPPORTED`） |
| `dialect` | `dialect()` | `string` | 后端声明的方言名——**别拿它当能力探测**（§10.7） |
| `sqlProfile` | `sqlProfile()` | `{count,totalMs,slow,byDb,events}` | 本请求 SQL 画像（dev 默认开；追踪关时 `{}`） |
| `asSystem` | `asSystem()` | 本实例 | 本请求以系统身份绕过租户防护（审计日志；业务禁用） |
| `asTenant` | `asTenant(id)` | 本实例 | 匿名请求**声明**租户身份（仍需 `tenant.allow_as_tenant`） |

除 `stream` 外，所有方法在**命名库实例**（`DB(name)`）与 **`tx` 回调对象**上同面；`tx` 对象另有
`isTx: true` 标记（DDL 必须拒绝在此句柄上执行），且**不提供 `stream`**（流式只走直连池，见 §3.1）。

---

## 3. 原生参数化查询（query / exec）

适合构造器不覆盖的语句（聚合报表、方言特有语法）；简单增删改查优先用构造器（见 [§5](#5-查询构造器从最小查询到组合)）：

```ts
const rows = await db.table("account").select(["id", "name"]).where({ field: "role", op: "eq", value: "admin" }).all();
// 构造器不覆盖的语句才退回原生参数化执行：
const n = await db.exec("update account set role = ? where id = ?", ["user", 7]);
```

- **值只经 `?` 占位符绑定**，绝不允许字符串拼接——这是红线（SQL 注入），不是风格建议。
  **`?` 是三引擎统一占位符（0.1.67 起）**：postgres 由宿主自动改写 `$n`，无需 `db.dialect()`
  分支；存量 `$n` 写法继续有效（`$n`+`?` 混用按序号续接）。唯一例外：JSONB 存在性
  运算符在 PG 下请写 `jsonb_exists(doc, 'key')`（裸 `?` 与占位符无法区分，见 §错误表）。
- `params` 可省略（无参便捷形式）。
- 裸 SQL 也过归属守卫：SQL 里出现的表必须是你模块拥有的或已声明 deps 的。
- 表名/列名无法参数化——动态标识符请改用 `db.table()` 构造器（白名单）。

### 3.1 流式查询（`db.stream`，v0.1.37）

`db.query` 会把整张结果集拉进内存；导出、ETL、大表遍历这类场景应该改用 `db.stream`——
**逐行**从后端拉取，一边收一边处理（写 `json.stream` 推流、落库、累积聚合），常驻内存只有一行：

```ts
// 回调形态（推荐）：Promise 在流走完后 resolve，异常 reject
const s = json.stream({ contentType: "text/csv" });
s.write("id,name\n");
await db.stream("select id, name from account order by id", null, {
  onRow: (row) => s.write(`${row.id},${row.name}\n`),
});
s.end();

// 异步迭代器形态（逃生舱）：for await 消费
for await (const row of db.stream("select id, name from account order by id")) {
  console.log(row);
}
```

- 结果与 `db.query` **内容一致**（同一查询逐行 ≡ 全量数组），差异只在内存形态。
- 取消：传 `opts.signal`（`AbortController.signal`），`abort` 事件触发即中止后端拉取——
  流**干净提前结束**（消费端收到 done，不报错）；服务端行缓冲有界，消费端不拉取时后端
  拉取同步暂停；已回调的行不会回滚。
- **事务内不可用**：`db.stream` 只走 `DB(name)` 直连池，在 `db.tx(...)` 内调用直接报错。
- 后端支持（v0.1.38 起 ABI 10）：核心 `SqlxAccessor`（内置只承载 sqlite）、测试用
  InMemory，以及第一方插件 `oj-db-mysql`/`oj-db-postgres`（typed 直连，vtable 批量 pull，
  ≤100 行/次）真流式；**未声明** `stream` 的插件（或旧插件返回 `{"unsupported":true}`）→ 回落
  `db.query` 全量（不报错，仅失去内存优势）。
  **声明了 `stream` 却返回 unsupported** 是能力契约违规 → 结构化 `OJ_DB_UNSUPPORTED`
  （`e.data.capability = "stream"`，v0.1.88），不静默回落；反过来「未声明却真提供了流」
  宿主采信但打 warn（插件该补 `"stream"`）。
- **专用连接并发上限 8**（v0.1.84，mysql/postgres 的 typed 直连路径）：每条游标独占一条
  池外连接（真取消语义的前提），超限报
  `db stream_open: too many concurrent cursors (limit 8) — close or cancel running streams first`。
  并行导出请改成串行或复用单流——打满会连累同库的池查询。
- 裸 SQL 同样过归属守卫与多租户防护（与 `db.query` 相同）。

---

## 4. 事务

```ts
const out = await db.tx(async (tx) => {
  await tx.table("account").insert({ name: "neo", role: "user" }).run();
  const rows = await tx.table("account").select(["id", "name"])
    .where({ field: "name", op: "eq", value: "neo" }).all();
  if (rows.length === 0) throw new Error("gone");   // throw/reject → 自动回滚
  return rows;
});                                                  // 正常 resolve → 自动 commit
```

- `tx` 回调拿到的对象与 `db` 同面（`query`/`exec`/`table`/`fromJSON`/`nextSeq`/`listTables`/
  `dialect`/`sqlProfile`/`asSystem`/`asTenant` + 条件工厂，另有 `isTx: true`；**没有 `stream`**），
  全部路由到**同一连接**上。
- **例外：`tx.asSystem()` / `tx.asTenant(id)` 返回的是外层池句柄**（不带 `isTx` 标记）。别指望
  它「离开事务」——执行路由按**库名**判定，语句照样进事务会话；反倒是靠 `isTx` 认事务域的
  共享库（如 `search.ensureIndex` 的 DDL 拒绝）会因此失明。要绕租户就在进 `db.tx` 之前调
  `db.asSystem()`（逃生位是请求级的）。
- 事务期间（包括构造器 `.run()/.all()`）同库操作自动骑到事务会话；
  **碰别的库**报错：`transaction active on db 'x' (finish it before touching db 'y')`。
- 不支持嵌套：`transaction already active (nested tx not supported)`。
- **构造器在事务里同样受多租户防护**：租户条件在「走池 / 走会话」判定之前注入，
  所以 `tx.table(...)` 与 `db.table(...)` 的改写完全一致（select 收窄、insert 强制写
  当前租户、update/delete 收窄）。裸 SQL 只有「文本里有没有 tenant_id」的 best-effort
  检查——事务里也请优先用构造器。

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

- **隐式 LIMIT 只加在顶层查询**，值取 `db_query.default_limit`（**默认 100**）；显式
  `limit(n)` 一律生效并被 clamp 到 `db_query.max_limit`（**默认 1000**，硬顶 100000）。
  子查询/union 成员不会被隐式截断。两者都在装配期校验（`0` / 倒置 / 超硬顶 → fail-fast）。
- **截断不再静默**：返回行数达到实际生效上限时，响应带 `X-OJ-Row-Limit: <n>` 头
  （含「显式 limit 被 clamp」这一类）。显式给了 limit 且行数未达上限 → 不写头。
- 多次 `.where()` 条件之间 **AND**（`.having()` 是整体替换，单树）。
- **可选筛选写 `whereIf(test, cond)`**（v0.1.81）：`test` 为 `undefined` / `null` / `false` /
  **空数组** 时整条跳过；`0` 与 `""` **保留**（`status = 0` 是合法过滤值）。用来避免
  `in []` 这种「永久假谓词静默返回 0 行」：
  ```ts
  db.table("order")
    .whereIf(id, { field: "id", op: "eq", value: id })
    .whereIf(ids?.length, { field: "id", op: "in", value: ids });
  ```
- **`select(["*"])`**（v0.1.81）必须唯一（`select '*' must be the only column`），且不能带
  join（`select '*' is not allowed with joins`）；不写 `select` 与写 `["*"]` 同义。
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
| `eq` `ne` | 任意 JSON 值；**省略 value 渲染成 `= NULL` / `<> NULL`（三值逻辑，恒不成立——判空用 `isnull`）** |
| `gt` `gte` `lt` `lte` | 必填（`gt needs value`） |
| `in` | 必须**非空**数组（非数组 `in needs array value`；空数组 `in needs non-empty array`——`IN ()` 是永久假谓词，会静默 0 行） |
| `like` | 必填（**已成形**的 LIKE 模式，不转义；需要转义就用 `escape:"!"` 逃生舱） |
| `isnull` | 不需要 value |
| `contains` `startswith` `endswith` | 必填（v0.1.81）。把值当**普通搜索词**：引擎自动转义 `%` `_` `!` 并渲染 `LIKE ? ESCAPE '!'`，业务零转义、零方言知识 |

> 模糊搜索优先用语义化三兄弟（`contains` / `startswith` / `endswith`）；`like` 只留给框架内部
> 与确有「模式已成形」需求的场景——详见 `docs/devkit/sql-builder-guide.md` §3.5。

组合树：`{and: [...]}` / `{or: [...]}` / `{not: 叶子}`。手工写嵌套树容易错，推荐工厂：

```ts
const { leaf, and, or, not } = db;          // 每个命名库（含 tx 回调）都带这组工厂

const cond = and(
  leaf("age", "gte", 18),
  or(leaf("role", "eq", "admin"), not(leaf("tag", "eq", "hidden"))),
);

await db.table("account").where(cond).all();   // .all() 是 async，必须 await
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
- `on` 是**列对列等值**数组 `{left, right}`；v0.1.81 起也收**列对常量** `{left, op, value}`
  且可与列对列混排（op 限比较类，`contains`/`isnull` 等模式类直接拒）：
  ```ts
  db.table("receipt").as("r")
    .join("broadcast", [{ left: "r.broadcast_id", right: "b.id" },
                        { left: "r.user_id", op: "eq", value: uid }], "left", "b");
  // → ... JOIN broadcast b ON r.broadcast_id = b.id AND r.user_id = ?
  ```
- 限定名 `"表.列"`：表段 ∈ {基表, join 表, CTE 名}；**非限定列只解析基表**
  （两表同名列天然无歧义）。列不存在：`unknown column 'x' in join on`。
- **表别名**（v0.1.81）：基表用 `db.table("x").as("b")`，被 join 表用第 4 参数
  `join(t, on, kind, "b")`。别名只作用于列限定与 SQL 渲染——**归属/租户守卫始终按真名**
  生效（守卫失效的那次缺陷就是别名混进真名导致的）。
- **自 join 需起别名**（v0.1.81）：`db.table("a").as("x").join("a", [...], "inner", "y")`
  可用；两侧同真名且 `sql_name()` 撞车才拒——`self join needs distinct table aliases
  (got 'a')`（同真名）或 `duplicate table alias 'a'`（异真名、别名撞车）。
- `update`/`delete` **不接受别名**（mysql 的 `DELETE FROM t AS b` 需 8.0.16+，跨引擎不保证）：
  `table alias is not supported on insert/update/delete`。
- on 不能为空：`join 'x' needs non-empty on`；join 表同样过归属守卫。
- select 省略 columns 时展开**基表**全列（带 join 时自动限定）；显式 `select(["*"])`
  （v0.1.81）与 join **互斥**（`select '*' is not allowed with joins`）——全列只出基表，
  join 列会丢。

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

动词×字段**兼容矩阵在 op 侧权威校验**（`fromJSON` 也绕不过）。注意 `update`/`delete` 共用一个
拒绝分支，报错里的动词字样就是字面 `update/delete`：

| 动词 | 允许 | 拒绝（`<verb> does not accept <field>`） |
|---|---|---|
| select | where/joins/columns/groupBy/having/orderBy/limit/offset/distinct/unions/with | values, sets, returning, orIgnore, fromSelect |
| insert | values 或 fromSelect（**二选一**）、returning、orIgnore | where, orderBy, limit/offset, joins, distinct, groupBy, having, unions, with, columns |
| update | sets（非空）+ where（≥1 叶子） | joins, columns, distinct, groupBy, having, unions, with, limit/offset, returning, orIgnore, fromSelect, truncate |
| delete | where（≥1 叶子）或 `truncate()`（此时不得带 where） | joins, columns, distinct, groupBy, having, unions, with, limit/offset, returning, orIgnore, fromSelect |

`truncate` 不是独立动词：`.truncate()` 置 `verb=delete` + 标志位（矩阵里只在 update 行拒绝；
`delete` 行不列它）。`sets` 只有 `update` 渲染 `SET`——`fromJSON` 让 delete 带上 `sets` 不会被
拒，会被静默忽略。

其它 op 侧兜底（矩阵外）：`table alias is not supported on insert/update/delete`；
`insert takes either values or fromSelect, not both` / `insert needs either values or fromSelect`；
`insert fromSelect needs insert columns` / `insert columns are only allowed with fromSelect`；
`fromSelect source needs explicit columns (no '*' / omission)`。

insert 行数组必须**键集完全一致**（`insert rows must share identical key sets`）；
键/sets 逐一过白名单（`unknown column 'k' in insert values` / `in update sets`）。
update/delete 无 where 在 JS 侧提前抛（`update requires where`），绕过 JS 层时 op 侧
兜底（`Update requires where (leaf count >= 1)`）。

**返回形态**：select → 行数组；insert/update/delete → 受影响行数（number）；
insert 带 `.returning([...])` → 行数组。

```ts
// 取回自增主键：替代原先的「insert ... returning id」裸 SQL
const rows = await db.table("account").insert({ name: "neo", role: "user" })
  .returning(["id"]).run();
const id = rows[0].id;
```

- `.returning(["id"])` 只有 insert 接受（其它动词报 `<verb> does not accept returning`），
  列过白名单。
- pg / sqlite：渲染 `INSERT ... RETURNING "id"`，**单语句一次往返**（不再需要
  「exec 之后查 `last_insert_rowid()`」这种会串号的写法）。
- mysql：方言无 RETURNING，只接受**单列**（`mysql returning accepts exactly one column
  (no RETURNING support)`），在同一连接上两步取 `LAST_INSERT_ID()`
  ——并发请放 `db.tx` 内。

同一 `resolve_target` 路由：有活跃事务走会话，否则走池——DML 与查询天然同事务。

### 7.1 幂等写入 / 查询插入 / 整表清空（v0.1.81–0.1.82）

```ts
// 撞主键/任意唯一键就跳过（不报错、不写入）——幂等导入
await db.table("notify").insert({ id: 1, user_id: uid }).orIgnore().run();

// INSERT ... SELECT：目标列与源 select 列按位置对齐
await db.table("notify_daily").fromSelect(
  db.table("notify").select(["user_id", "cnt"]).where(leaf("day", "eq", d)),
  ["user_id", "cnt"],
).run();

// 命名式整表清空（渲染 DELETE FROM t；租户表上只清当前租户）
await db.table("sys_login_throttle").truncate().run();
```

- **`orIgnore()`**：sqlite/pg 渲染 `ON CONFLICT DO NOTHING`，mysql 渲染
  `ON DUPLICATE KEY UPDATE <pk> = <pk>`（恒等式）——所以 mysql **要求表有主键**
  （`orIgnore on mysql requires a primary key on table '…'`）。**别把 `orIgnore()` 与
  `returning(["id"])` 一起用**（mysql 上恒等赋值不刷新 `LAST_INSERT_ID()`，跳过重复行时会拿到
  连接上的旧值）——要幂等+id 就先按唯一键 `select`。**affected-rows 三方言不一致**（mysql 命中
  重复键给 1，pg/sqlite 给 0）：别用 `if (n === 1)` 判幂等，靠唯一约束 + 事务保证「不会重复写」。
- **sqlite 上 `orIgnore()` + `fromSelect()` 不可组合**（upsert 子句只能跟在 VALUES 后）：
  `orIgnore with fromSelect is not supported on sqlite (its upsert clause only follows VALUES)`；
  pg/mysql 组合可用。
- **`fromSelect(q, cols)`**：列数不符 `insert declares N columns but the source select yields M`；
  `cols` 含 `tenant_id` 直接拒（租户由服务端注入）；源 select 递归过租户与归属守卫
  （`ownership_guard: deny` 下跨模块搬运同样被挡）。
- **`truncate()`** 是唯一无 where 的写动词（明示整表清空，取代裸 `delete from`）：带 where 报
  `truncate does not accept where`；**租户表上只清当前租户**（要整库清空走 `db.asSystem()`）。

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
- **`count(distinct col)`**（v0.1.82）：`{ fn: "count", distinct: true, field: "user_id", as: "c" }`
  → `COUNT(DISTINCT user_id)`，聚合成一行、不受 LIMIT 钳制。仅 count 支持
  （`distinct is only supported on count`），且必须带具名 field（`count distinct needs a field`）。
  **别再用 `distinct().all()` 拉行后取 `.length` 计数**——顶层隐式 LIMIT 会静默截断，
  受影响用户数 > 1000 就漏算。
- **字面量列**（v0.1.81）：`{ lit: "svc", as: "scope" }` 把常量作为一列输出，值走**绑定参数**
  并按方言 `CAST`（sqlite 裸 `?`、mysql `CAST(? AS CHAR)`、postgres `CAST(? AS TEXT)`）。
  `lit` 必须是标量（`lit column 'x' needs a scalar value …`）；要与 `*` 同时用会报
  `select '*' must be the only column`。计算列请用 `case`。
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
- 嵌套深度上限 `REQ_NEST_MAX = 4`：顶层算 `depth 0`，最深只能用到 `depth 3`（即最多 3 层
  嵌套），再深即 `subquery: nested select too deep`（site 换成 `exists` / `union` / `cte` /
  `fromSelect`）。
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

**概念**：CTE（Common Table Expression，公用表表达式）就是**给一段子查询起个名字**，
让它在**这一条语句内**可以被当成表来用。SQL 形态是
`WITH 名字(列, ...) AS (子查询) 主查询`。心智模型：**语句级的临时视图**——不是建表、
不落盘、语句结束就消失；相当于"先把这部分结果算出来存个名，主查询再拿这个名字当表查"。

什么时候需要它：

- **可读性**：嵌套子查询套到第三层就读不动了，把每层拆成具名步骤，像读流水账。
- **自 join 的替代**：v0.1.81 起同表自 join 只要两侧起不同别名就能写（见 §6）；但同一张表
  被引用三处以上时，CTE 复制一份读起来更像流水账。
- **复用**：一个定义，主查询里可以当基表、也可以进 join 条件引用它。

```ts
const adults = db.table("account")
  .select(["id", "name"]).where(leaf("age", "gte", 18));

db.table("adults")
  .with("adults", ["id", "name"], adults)     // name + 声明列 + 成员查询
  .select(["id", "name"])
  .where(leaf("name", "like", "a%"))
  .all();
```

**原理（渲染与安全模型）**：上面这条链渲染为（`.toSQL()` 随时可验证）：

```sql
WITH "adults" ("id", "name") AS
  (SELECT "id", "name" FROM "account" WHERE "age" >= ?)
SELECT "id", "name" FROM "adults" WHERE "name" LIKE ?
```

- 执行时成员 SELECT 先算出一个结果集，主查询把它当普通表引用；至于引擎内联还是
  物化这个中间结果是优化细节，语义不变、不落盘。
- **为什么 `columns` 必填**：引擎不推断 CTE 的输出列形状，**声明的列就是它对外发布的
  契约**（`cte needs non-empty columns`）。后续一切对 `adults` 的列引用都按这份清单
  过白名单——成员查询里摸不到的列，引用侧同样摸不到，白名单防线在这里不打折。
- **非递归**：不支持 `WITH RECURSIVE`（树形/图遍历请用应用层循环或裸 SQL）。
- CTE 是**虚拟表**，不查 schema 注册表、无归属模块（归属守卫递归查它的成员查询）；
  但名字与列名仍过别名形状校验（`illegal alias 'x'`）。
- **WITH 名遮蔽同名真实表**（标准 SQL 语义）：本语句内 `adults` 指 CTE 而非同名实体表。
- **校验顺序**：WITH 先于主语句表解析——所以基表直接写 CTE 名是合法写法。
- 成员查询受嵌套约束：仅 select、禁再嵌 with/unions（`nested select does not accept
  with/unions (v1)`），嵌套深度计入 `REQ_NEST_MAX`（§9.1）。

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

## 10.5 方言能力矩阵（v0.1.88，ABI 12）

宿主只对**它认识的三种方言**替你生成 SQL；其他方言（由插件 `describe()` 声明）上一期只开放
**裸 SQL 路径**。能力缺失一律 `OJ_DB_UNSUPPORTED`（`e.data.capability` 点名是哪个 API），
**绝不**静默按 sqlite 语义生成一段错误 SQL。

| API | sqlite | mysql | postgres | 其他方言 |
|---|---|---|---|---|
| `db.query` / `db.exec` / `db.tx` | ✅ | ✅ | ✅ | ✅（裸 SQL 全量可用） |
| `db.stream` | ✅ | ✅（typed 直连，真流式） | ✅（typed 直连，真流式） | 取决于插件是否声明 `stream`；未声明回落 `db.query` 全量 |
| `db.stream` 的 `signal` 取消 | 协作式（批间） | ✅ `KILL QUERY` | ✅ `pg_cancel_backend` | 取决于插件是否声明 `cancel` |
| `db.table(...)` 构造器（含 DML / join / CTE / 聚合） | ✅ | ✅ | ✅ | ❌ `OJ_DB_UNSUPPORTED` |
| `db.listTables` | ✅ | ✅ | ✅ | ❌ |
| `db.nextSeq` | ✅ | ✅ | ✅ | ❌ |
| `_shared/search.ts` / `_shared/versions.ts` | ✅ | ✅ | ✅ | ❌ |
| `oj migrate` / `oj seed` | ✅ | ✅ | ✅ | ❌（迁移须用该方言自己的工具） |

> **`memory://` 不在上表三列里**：它是测试**假后端**（见 §1.1），`query`/`exec`/`stream`
> 都直接回夹具行、不执行 SQL，`db.nextSeq` 因此拿不到返回值（`db.nextSeq: no value
> returned`）。要验真实 SQL 语义请用 `sqlite::memory:`。

### 能力位对照表（8 个串 × 三内置方言）

插件在 `describe()` 里声明**它真做得到**的能力位；宿主装配期解析成
`DialectCaps { placeholder, id, stream, cancel }`，运行期纯查表。下表是**声明面**的逐位对照
（`plugins()` / `GET {base}/plugins` 的 `caps` 列、启动横幅 `| caps: …` 同源，
串由 `DialectInfo::summary()` 拼装）：

| 能力串 | 语义（声明 = 承诺） | sqlite | mysql | postgres | 宿主消费点 / 执行期影响 |
|---|---|---|---|---|---|
| `placeholder_q` | 原生占位符 `?` | ✅ | ✅ | — | `placeholder.rs` **不改写**，JS 写的 `?` 原样下发。**参与执行** |
| `placeholder_dollar` | 原生占位符 `$n` | — | — | ✅ | `placeholder.rs` 把 `?` 依次改写成 `$1..$n`（`?` 个数与参数个数不符提前报 `placeholder count <n> != params <m>`）。**参与执行** |
| `placeholder_colon` | 原生占位符 `:n` | — | — | — | 宿主**不改写**（透传），`:n` 改写归**插件内部**。**参与执行（插件侧）** |
| `id_returning` | insert 单语句 `RETURNING` 取自增 | — | — | ✅ | **仅登记**：构造器取 id 按具体方言选路，不读这一位（见下） |
| `id_two_step` | 取自增需二次查询 | ✅ | ✅ | — | **仅登记**（同上）：mysql `LAST_INSERT_ID()`、sqlite `last_insert_rowid()` |
| `id_none` | 不提供自增 id（主键由客户端 / `db.nextSeq` 生成） | — | — | — | **仅登记**：上层要 id 时给「无 id」，不报错 |
| `stream` | 实现 `stream_open/next/cancel/close` | ✅ | ✅ | ✅ | `db.stream` 走 vtable 批量 pull（≤100 行/次）。**参与执行** |
| `cancel` | 语句级真取消（服务端中断） | — | ✅ | ✅ | `signal` 中止活跃游标时调 `stream_cancel`（mysql `KILL QUERY` / pg `pg_cancel_backend`）。**参与执行** |

**三内置方言的完整能力串**（启动横幅与 `oj info` 里逐字可见）：

```
sqlite    placeholder=? id=two_step stream
mysql     placeholder=? id=two_step stream cancel
postgres  placeholder=$ id=returning stream cancel
```

> **组的约束**（装配期校验，错即拒载并点名）：`placeholder_*` 三选一**必选**、`id_*` 三选一
> **必选**；`stream` / `cancel` 独立可选（缺省 false）。同组出现两个（如 `id_returning` +
> `id_two_step`）算错。四种失败各有固定文案：`unknown capability "…"`（typo）、
> `describe json invalid: …`（JSON 本身坏了）、`missing capability group "placeholder"|"id"|"dialect"`、
> `duplicate capability in group "placeholder"|"id"|"stream"|"cancel"`。

> `id` 组的现实口径：**登记与对账用，不参与执行路由**。构造器取 id 的路由按具体方言
> 判定（只有 mysql 进「两步 `LAST_INSERT_ID()`」，其余走 `RETURNING`）——所以 sqlite 虽然
> 声明 `id_two_step`，`.returning(["id"])` 生成的却是**单语句 `RETURNING`**。因为回取语句是
> 方言私有的，宿主无法从能力位推出来。`query.rs::exec_route` 的注释与
> `lastid_route_is_mysql_only_not_equivalent_to_id_two_step` 用例钉死了这条边界。

> **能力位齐全 ≠ 业务 API 可用**：新方言的每格都由它自己的 `describe()` 决定，规则与上表
> 完全一致；但构造器 / `listTables` / `nextSeq` / `migrate` / `seed` 还要求方言名落在宿主
> known 子集（`sqlite` / `mysql` / `postgres`），否则一律 `OJ_DB_UNSUPPORTED`（见本节上方
> API 矩阵的「其他方言」列）。

**接新方言（插件作者）**：在 `describe()` 返回 `{"dialect": "<名>", "capabilities": [...]}`，
能力串只能取上表 8 个之一（两个互斥组各必选一，`stream` / `cancel` 可选）。宿主装配期严格
校验，写错/漏项直接拒绝该插件并点名——宁可拒绝启动，也不让错配活到运行期。接入全流程与
验收口径见 `docs/plugins/plugin-development.md` §15（conformance 清单），起手骨架见
[`tools/plugin-template-db`](../tools/plugin-template-db/README.md)；类型/值契约见本节 §10.6。

**错误诊断**：db 的 reject 自带 `e.code` / `e.retryable` / `e.sessionDisposition` / `e.data`
（含引擎原始码 `data.vendor_code`）——不要解析 message 文本。完整口径见
`docs/devkit/api-manual.md`「结构化数据库错误」。

`OJ_DB_UNSUPPORTED` 的 `e.data.capability` 是**闭集**（业务代码可以安全 switch）：

| capability | 触发点 |
|---|---|
| `query_build` / `query_sql` | `db.table(...).all()/run()` 与 `.toSQL()`（宿主生成 SQL） |
| `list_tables` | `db.listTables()` |
| `next_seq` | `db.nextSeq()` |
| `migrate` / `seed` | `oj migrate` / `oj seed`（CLI 侧 fail-loud，不进 JS） |
| `stream` / `cancel` | 插件声明了该能力却回 unsupported（契约违规） |

**业务代码要不要按方言分支？** 多半不需要，且 `db.dialect()` 不是判断能力的正确工具——
迁移分档与模板见 §10.7。

---

## 10.6 类型映射表与值契约（跨方言）

`schema.yaml` 的列类型词汇是**固定的六类**（唯一来源 `src/bridge/registry.rs` 的
`ColumnType::SCHEMA_TYPES`：`integer` / `bigint` / `text` / `boolean` / `double` / `blob`）。
任何方言插件都必须能把六类建成表、并读回 JS 可用的值；对不上要**报错**，不许静默兜底。

### 1) 建表映射（DDL）

| 逻辑类型 | sqlite | mysql | postgres | oracle（M2 · 方案级） | 达梦 DM8（M2 · 待真库复核） |
|---|---|---|---|---|---|
| `integer` | `INTEGER` | `INT` | `int4` | `NUMBER(38)` | `INT` |
| `bigint` | `INTEGER` | `BIGINT` | `int8` | `NUMBER(20)` | `BIGINT` |
| `text` | `TEXT` | `TEXT` / `VARCHAR(n)` | `text` | `VARCHAR2(4000)`；超长 `CLOB` | `VARCHAR(n)` / `CLOB` |
| `boolean` | `INTEGER`（0/1） | `TINYINT(1)` | `boolean` | `NUMBER(1)` + `CHECK (c IN (0,1))` | `BIT` / `TINYINT`（M2 定） |
| `double` | `REAL` | `DOUBLE` | `float8` | `BINARY_DOUBLE` | `DOUBLE` |
| `blob` | `BLOB` | `BLOB` | `bytea` | `BLOB` | `BLOB` |

> 后两列来自设计 §5.6 的方案结论，**M2 落地时以真库 DDL 复核后回填**——不要据此直接生产建表。

### 2) 读侧值契约（JSON / JS）

| 逻辑类型 | JS 侧拿到 | 备注 |
|---|---|---|
| `integer` / `bigint` | `number`；**超出 ±(2^53-1) 的 i64 → 十进制字符串** | 见 `docs/numeric-limits.md`；回写超界整数用 `toBigInt()` |
| `double` | `number` | 二进制浮点，别用来放钱 |
| `boolean` | `true` / `false`；**MySQL 读出 `1` / `0`** | MySQL 无独立布尔类型（`TINYINT(1)`） |
| `text` | `string` | |
| `blob` | `string`（**UTF-8 lossy 转换**） | 真二进制请自己 base64 后存 `text`；二进制不会原样穿过 JSON |
| 日期时间（**不在六类内**） | 用 `text` 存 ISO 8601 | 引擎原生 DATE/TIMESTAMP 属「受限列」，需显式 `cast`（见下表） |

### 3) 三内置方言的读侧边界

| 方言 | 可读 | 报错（点名列名 + 类型，给 `cast` 指路） | 未识别时静默 `null`？ |
|---|---|---|---|
| sqlite | 整数 / 浮点 / 文本 / BLOB（动态类型） | —— | **是**（`row_to_json` 兜 `Value::Null`，既有行为，未收紧） |
| mysql | 整数家族（含 `BIGINT UNSIGNED`）/ 文本家族 / 二进制 / `FLOAT` / `DOUBLE` | `DECIMAL`、日期时间、`JSON`、`BIT`、`GEOMETRY` | 否 |
| postgres | `boolean`、`int2` / `int4` / `int8`、`float8`、`text` / `varchar` / `char`、`bytea` | `real`（float4）、`numeric`、`json` / `jsonb`、`uuid`、日期时间、数组、`void` | 否 |

`db.query` 与 `db.stream` 用**同一套判据**（v0.1.88 起流式补齐 `smallint`/`integer`；此前流式
读这两类会误报「未解码」，而同样的查询走 `db.query` 却正常）。

### 4) 新方言接入的硬要求

1. **列类型必须闭集**：禁止 `Unknown` 静默当 text。`SchemaFile::parse` 在声明期就拒未知
   类型（点名原串 + 列出支持清单 + 指引「其余类型请写 `migrations/NNNN__desc.sql`」）；v0.1.88
   起核心侧也收紧——`ColumnType::from_schema_type` 返回 `Option`（不再兜底 `Unknown`），
   `registry_tables()` 对未识别类型同样拒载并点名，**即使有人绕过 `parse` 直接反序列化**也走不到
   「当 text」。`Unknown` 只剩一个含义：**列类型未声明**（旧装配路径 `table()`，按 v0.1.23 行为）。
   为什么必须这样：把 `varchar` 当 text，租户守卫会给 `tenant_id` 绑字符串，在 PG 上退化成一句
   难懂的 `operator does not exist: bigint = text`——看起来像方言 bug，实际是声明写错。
2. **读侧不许静默 `null`**：解不出的列类型必须报错（mysql/pg 插件的既有做法）。sqlite 的静默
   `null` 是历史包袱，新插件**不要照抄**。
3. 上表第 2 节的**值契约是跨方言契约**，逐项写进插件 conformance 清单验收（见
   `docs/plugins/plugin-development.md` §15「新增数据库方言」）；起手用
   [`tools/plugin-template-db`](../tools/plugin-template-db/README.md) 骨架，它已把这四条
   摆成代码面（含未声明/已声明 `stream`·`cancel` 的一致性用例）。

---

## 10.7 `db.dialect()` 的红字与迁移范式（v0.1.88）

> **红字：不要用 `db.dialect()` 判断「这个库能干什么」。**
> 方言名是**开放集**（插件 `describe()` 声明什么就是什么，`"oracle"` / `"dameng"` / 私有的
> `"demo"` 都合法）。v0.1.88 起宿主**不再把它回落成 `"sqlite"`**（旧行为是封闭三值枚举：
> 未识别的 DSN 前缀一律当 sqlite 处理，`dialect()` 于是报 `"sqlite"`，让一切都「看起来支持」），
> 也不再据此生成 SQL。能力位
> `{placeholder, id, stream, cancel}` 才是宿主认识的契约，但它是**宿主 ↔ 插件**之间的契约，
> 不是给业务 handler 的分派开关。
>
> 危害是**静默**的：`if (d === "mysql") A else B` 在新方言接入后会把 B 直接发给它——
> 占位符风格对不上、取 id 走错路、`ON DUPLICATE KEY` 发给 Oracle，报错落在引擎侧
> （`ORA-00933`、`near "?"`…），看起来像方言 bug，实际是分支兜底。

`db.dialect()` 只剩三种正当用途：**诊断/日志**、**确属方言特有的 SQL**（第 ③ 档，必须白名单
+ 响亮失败）、**共享模块的集中适配层**（`_shared/dialect.ts` 这一类，业务代码不直接看它）。
`db.tx` 回调对象是同面（`tx.dialect()`），语义一致。

### 迁移分档：先问「这段分支能不能删」

| 档 | 原来在分支什么 | 现在怎么写 | 为什么不用问方言 |
|---|---|---|---|
| ① 删掉 | 拼占位符（`?` / `$n` / `:n`） | 一律写 `?` | 按 `placeholder` 能力位改写：`placeholder_dollar` 宿主改 `$n`；`placeholder_colon` 宿主**不改写**，由插件内部把 `?` 改 `:n` |
| ① 删掉 | 取自增 id（`RETURNING` vs `LAST_INSERT_ID`） | `db.table(...).insert({...}).returning(["id"]).run()`（mysql 只收单列） | 取 id 路线由构造器按**具体方言**决定（只有 mysql 走两步 `LAST_INSERT_ID()`，其余 `RETURNING`）——不是按 `caps.id` 推的，回取语句是方言私有 |
| ① 删掉 | 列目录（`sqlite_master` / `information_schema`） | `db.listTables(prefix?)` | 三内置方言同口径（SQL 是引擎常量），业务侧不用写分支 |
| ① 删掉 | `limit n` 语法 / `select max(id)+1` 取号 | 构造器 `.limit(n)` / `db.nextSeq(name)` | 构造器与序列原语自己处理方言差异 |
| ② 换问法 | 「这个 API 支持吗」 | **直接调用，接住 `OJ_DB_UNSUPPORTED`** | 宿主抛结构化错误，`e.data.capability` 点名是哪个 API；不用猜 |
| ③ 保留 | 真方言特有 SQL（upsert 语法、窗口函数差异、受限列型 `cast`、DDL） | 白名单 + 未知方言**响亮失败**（模板见下） | 这类差异确实存在，但必须「不认识就报错」而不是兜底 |

**第 ② 档的写法**——不要「先问再调」，直接调：

```ts
async function listReportIds(): Promise<unknown[]> {
  try {
    return await db.table("report").select(["id"]).all();
  } catch (e: any) {
    if (e.code === "OJ_DB_UNSUPPORTED") {          // 该方言不支持构造器
      return db.query("select id from report");    // 裸 SQL 路径始终可用
    }
    throw e;
  }
}
```

这比 `if (["sqlite","mysql","postgres"].includes(db.dialect()))` 更准：白名单写错会骗自己，
结构化错误不会。

### 第 ③ 档的模板（白名单 + 禁止静默兜底）

```ts
// _shared/dialect.ts —— 全项目唯一的方言分派点，集中受审
const UPSERT_SQL = {
  sqlite:   "insert into account (id,name) values (?,?) on conflict(id) do update set name=excluded.name",
  postgres: "insert into account (id,name) values (?,?) on conflict(id) do update set name=excluded.name",
  mysql:    "insert into account (id,name) values (?,?) on duplicate key update name=values(name)",
} as const;
type UpsertDialect = keyof typeof UPSERT_SQL;   // 新增方言时编译器会在这里报错

export async function upsertAccount(id: number, name: string): Promise<void> {
  const d = db.dialect();                       // 可能是任何声明名，包括 "oracle"
  if (!(d in UPSERT_SQL)) {
    throw new Error(`upsertAccount: dialect ${d} 未适配（已支持 ${Object.keys(UPSERT_SQL).join("/")}）`);
  }
  await db.exec(UPSERT_SQL[d as UpsertDialect], [id, name]);
}
```

要点：

- **白名单用对象/数组并 `in`/`includes` 判定**，键集合随类型走——新增方言忘改会编译期红，
  不会运行期静默。
- **失败信息点名函数 + 方言 + 已支持集**：排障时一眼定位，比引擎原始码有用。
- **别把 `db.dialect()` 存进模块级常量**：命名库之间可以不同方言，`db` 是 `DB(name)`，
  `db.tx` 句柄同面——缓存会导致跨库读错。
- **别在 handler 里按 `plugins()` 的 `caps` 自己分派**：`caps` 是留给运维核对装配的
  外显（`GET {base}/plugins`、`oj info`），不是业务能力探测入口；能力探测请走第 ② 档。

完整可照抄场景见 `docs/devkit/scenarios.md` 场景 45；能力位全表见 §10.5，插件侧接入见
`docs/plugins/plugin-development.md` §15。

---

## 11. 边界与红线（新人必读）

1. **动态标识符只来自白名单**。表名/列名绝不允许来自请求拼 SQL——注入防线的根。
   构造器里一切标识符都过 SchemaRegistry（或 CTE 声明列）；裸 SQL 的标识符只能写死。
2. **值只走绑定参数**。构造器值、`?` 占位符（三引擎统一，0.1.67 起）；永不字符串拼接。
3. **update/delete 必须带 where 且叶子 ≥ 1**——防全表误伤，JS 与 op 双层强制。唯一显式例外是
   `.truncate()`（命名式整表清空，见 §7.1）。
4. 数量级：条件树深度 ≤ 8、叶子 ≤ 64；嵌套 select 最多 3 层（`REQ_NEST_MAX=4`，顶层算 0 层）；
   limit 显式受 `db_query.max_limit` 约束（**默认 1000**，可配至硬顶 **100000**）。
5. **多写 .toSQL()**：与执行同管线，报错前先看它渲染出了什么。
6. **64 位整数（雪花 id）读出来是字符串**（v0.1.22）：`|v| > 2^53-1` 的 i64 一律以十进制
   字符串交付（超界给 bigint 会让 `json.ok` 直接 500）；回写用 `toBigInt()`——
   **字符串是文本意图、BigInt 是整数意图**，平台不做启发式转换（PG 上字符串写 bigint 列必报
   `column "x" is of type bigint but expression is of type text`）。**绝不要 `Number(大整数字符串)`**。
   无符号 64 位（`> i64::MAX`，仅 MySQL `BIGINT UNSIGNED`）用 `toUBigInt()`；
   **取号优先用 `db.nextSeq(name)`**（见 §11 第 7 条），不要再手写 `max(id)+1`。
   完整契约与范式见 `docs/numeric-limits.md`。
7. **取号用 `db.nextSeq(name)`（v0.1.24）**：平台序列分配原语，单语句原子、并发安全——
   `const id = await db.nextSeq("project_identifier");`（在 `db.tx` 内调用则搭车同一连接）。
   平台表 `_oj_sequences(name, v)` 首次使用**自动建**（`_oj_` 是平台命名空间，**业务不要手工
   读写/迁移它**）。返回值 ≤2^53-1 是 number，超出是十进制字符串（与 DB 读值同契约）。
   它取代了历史上 `select max(id)+1` 的写法（后者并发下会分配出相同序号，见
   `docs/numeric-limits.md` §3）。
   **四条必须知道的语义**（v0.1.24）：
   - **库级、跨租户共享**：`_oj_sequences` 没有 `tenant_id`，同一库内所有租户共用一个计数器；
     需要按租户隔离就把租户拼进 `name`（如 `` `order_no:${http.tenantId}` ``）。
   - **`name` 用服务端常量**，别直接取用户输入——否则序列表行数可被外部无限放大。
   - **需要 DDL 权限**（首次使用要 `create table if not exists`）；生产账号若只有 DML 权限，
     请用迁移**预建** `_oj_sequences(name, v)`。
   - **`memory://` 假后端不支持**（`no value returned`）；`sqlite://` / `sqlite::memory:`
     正常。平台序列表是普通表，`db.nextSeq` 只在**宿主认识的三种方言**上可用（§10.5）。
   - 序列值**不随调用方事务回滚而回退**（一次独立的原子自增）——按「只增不复用」理解。
8. **MySQL 读侧的列类型边界（v0.1.24）**：类型化路径下**可读** = 整数家族（含
   `BIGINT UNSIGNED`）、文本家族（`TEXT`/`VARCHAR`/`CHAR`/`ENUM`）、二进制、`FLOAT`/`DOUBLE`；
   **其余报错**：`DECIMAL`、`DATE`/`TIME`/`DATETIME`/`TIMESTAMP`/`YEAR`、`JSON`、`BIT`、
   `GEOMETRY`——报错点名列名与类型，**不会静默给 `null`**。读这些列请在 SQL 里显式转换
   （`select cast(amount as char) as amount from t`），别用 `select *` 兜。`BOOLEAN`/`TINYINT(1)`
   按整数读出 `1`/`0`（MySQL 无独立 boolean 类型）。

### 常见报错速查

| 报错（逐字） | 原因 | 修法 |
|---|---|---|
| `unknown table 'x'` | 表未进 schema.yaml | 声明表并检查模块装配 |
| `ownership: 表 "x" 属于模块 "y"…`（deny 模式） | 跨模块访问未声明依赖 | manifest `deps: [y]` 或改契约调用 |
| `unknown column 'x' in <site>` | 列不在白名单 / 未限定 | 补 schema；join 场景用 `表.列` |
| `unknown table 'x' in <site>` | 限定名表段不是基表/join/CTE | 检查限定名拼写 |
| `insert needs either values or fromSelect` | `.insert()` 空参（或 `fromJSON` 里两条都缺） | 传对象/非空数组，或改用 `.fromSelect(q, cols)` |
| `insert takes either values or fromSelect, not both` | 同一请求里两条路都给了 | 二选一（upsert 语义请用 `orIgnore`） |
| `insert rows must share identical key sets` | 行数组键不一致 | 对齐每行键 |
| `orIgnore on mysql requires a primary key on table 't'` | mysql 的 `orIgnore` 靠 `ON DUPLICATE KEY UPDATE pk=pk`，无主键无从恒等 | 给表加主键，或改用唯一约束 + 先查后写 |
| `orIgnore with fromSelect is not supported on sqlite (its upsert clause only follows VALUES)` | sqlite 的 upsert 只能跟 VALUES | 裸 `insert or ignore … select`（注意它连 NOT NULL/CHECK 也静默跳过），或 pg/mysql |
| `insert declares N columns but the source select yields M` | `fromSelect` 目标列与源列数不符 | 按位置对齐列清单 |
| `truncate does not accept where` | `.truncate()` 自带条件 | 走 `.delete().where(...)` |
| `count distinct needs a field` / `distinct is only supported on count` | `{fn:"count",distinct:true}` 缺 field / 给非 count 聚合加了 distinct | 写 `{fn:"count", distinct:true, field:"col"}` |
| `lit column 'x' needs a scalar value (string/number/bool/null)` | `{lit: …}` 给了数组/对象 | 标量；表达式请用 `case` |
| `db.nextSeq: no value returned` | `memory://` 假后端不执行 SQL | 换 `sqlite::memory:` 或真库 |
| `unknown db scheme in dsn '…' (known: […])` | DSN scheme 不认识，或对应插件没装上（`known` 列表按实际后端） | 装插件 / 改 DSN |
| `tenant guard: table(s) ["t"] require tenant context (missing tenant header; …)` | `sql_guard` 开着但请求没带租户头（deny 档） | 带 `X-TENANT-ID`，或系统任务改 `db.asSystem()` |
| `schema: [m] 表 "t" 缺 tenant_id 列（tenant.sql_guard 启用中…）` | 开了 `sql_guard` 但表没声明 `tenant_id` 列 | 补列；共享表标 `tenant: false` 并列进 `shared_allow` |
| `update needs non-empty sets` | `.update({})` | 至少一列 |
| `Update requires where (leaf count >= 1)`（delete 同款，动词首字母大写） | update/delete 无 where（绕过 JS `run()` 预检时由 op 兜底） | 补 `.where(...)`，或明示 `.truncate()` |
| `<verb> does not accept <field>` | 动词×字段矩阵拒绝 | 见 §7 表 |
| `condition tree too deep (max 8)` / `too large (max 64 leaves)` | 条件树超限 | 拍平/拆分条件 |
| `in needs array value` / `in needs non-empty array` | `op:"in"` 值非数组 / 空数组 | 传非空数组（空集请在 JS 侧直接短路，别发查询） |
| `value and subquery are mutually exclusive` | 叶子同时给了 value 和 subquery | 二选一 |
| `column "x" is of type bigint but expression is of type text`（PG） | 回写给 bigint 列传了**字符串** | 用 `toBigInt(...)`（见 §11 第 6 条） |
| `operator does not exist: bigint = text`（PG） | `where bigint 列 = $1` 传了字符串 | 同上 |
| 主键 `duplicate key`，被撞值末几位是 0 | `Number(max(id)) + 1` 坍缩到 f64 网格值 | 改 `toBigInt(max) + 1n`（`docs/numeric-limits.md` §3） |
| `toBigInt: … is not a safe integer` | 传入的是已坍缩的 number | 传 DB 原样给出的十进制字符串 |
| `invalid byte sequence for encoding "UTF8": 0x00` / `insufficient data left in message`（PG） | v0.1.24 前：同一 SQL 文本混用字符串/数字参数（sqlx 语句缓存按文本 key） | **v0.1.24 起平台自动按参数形态分缓存键**，无需再规避；若仍出现，检查 PG 插件是否随宿主重建 |
| `db param: u64 value … is not supported on this path` | `toUBigInt()` 的值走了一条不能承载 u64 的路径（PG/SQLite 全部；MySQL 的 sqlx::Any 回落路径） | 该值改存 text，或确保走 `BIGINT UNSIGNED` 的 typed 直连（`mysql://` DSN + 插件） |
| ``db(mysql): column 'x' has MySQL type 'DECIMAL' which this plugin does not decode yet — select explicit columns and cast it in SQL (e.g. `cast(x as char) as x`); see docs/numeric-limits.md §4.3``（v0.1.24） | MySQL 读侧不支持该列类型（`DECIMAL`/`JSON`/时间/`BIT`/`GEOMETRY`）——**报错而不再静默给 `null`** | 在 SQL 里显式转换：`select cast(x as char) as x from t`；避免 `select *` |
| MySQL `BOOLEAN` 读出是 `1`/`0` | `BOOLEAN` 即 `TINYINT(1)`，协议层无长度元数据可区分 | 按整数读；需要布尔语义就在 SQL 里转：`select flag = 1 as flag from t` |
| `<site>: nested select too deep`（site ∈ `subquery`/`exists`/`union`/`cte`/`fromSelect`） | 嵌套 > 3 层（depth ≥ 4） | 减层或拆 CTE |
| `nested select does not accept with/unions (v1)` | 子查询/union 成员带了 with/unions | 移到顶层 |
| `union requires explicit columns` | union 侧省略 select 列 | 双方显式列且列数一致 |
| `union member does not accept order_by/limit/offset` | 成员带排序分页 | 排序分页放最外层 |
| `self join needs distinct table aliases (got 'a')` / `duplicate table alias 'a'` | 同表自联没起别名 / 别名撞车（v0.1.81 起带别名可用） | 基表 `.as("x")`、被 join 表第 4 参起别名 |
| `table alias is not supported on insert/update/delete` | 别名只用于 select/join | 去掉 `.as()`，或先查后写 |
| `select '*' must be the only column` / `select '*' is not allowed with joins` | `*` 与显式列/join 混用 | 要么只写 `["*"]`，要么列清单写全 |
| `illegal alias 'x'` | 别名形状不合法 | 字母/下划线开头 |
| `cte needs non-empty columns` | `.with(name, [], q)` | 声明列清单 |
| `transaction active on db 'x' …` | 事务中碰了别的库 | 先 commit/rollback |
| `transaction already active (nested tx not supported)` | 嵌套 `db.tx` | 复用当前 tx 回调对象 |
| `db: instance 'x' not configured` | 执行期按名取不到库：多半是 `manifest.yaml` 的 `db: x` 指向了 config 未声明的库（`DB("x")` 本身对未声明名返回 `undefined`，不会抛这条） | 在 `db:` 段补 DSN，或改 manifest 的绑定 |
| `db.stream within an active transaction is not supported (streaming queries run on the connection pool only)` | 事务内调 `db.stream` | 移到事务外（流式只走直连池，见 §3.1） |
| `db access: alias "x" not granted to this caller (…)` | `db_access` 的 `allow` 未命中（四维全不中/取不到身份） | 找管理员在 `allow` 里给对应维度授权（reason: `db_not_granted`） |
| `db access: alias "x" is read_only; statement classified write` | 该库 `read_only: true` 而语句被判为写类 | 改只读语句，或放开 `read_only`（reason: `db_read_only`） |
| `db access: alias "x" does not allow DDL` | DDL 未放开 | 放开 `allow_ddl`，或走迁移通道（reason: `db_ddl_denied`） |
| `db access: alias "x": dangerous write without an effective WHERE (…)` | UPDATE/DELETE 无 WHERE（或多表 / 恒真谓词） | 补 WHERE，或放开 `allow_dangerous`（reason: `db_dangerous_write`） |
| `db access: statement classification failed; denied by fail-closed` | 词法分不出类别（串未闭合 / 断词 / 未知语句） | 改用构造器 `db.table()`——它走精确意图判定，不依赖词法（reason: `db_unknown_risk`） |

---

## 11.5 db 访问控制（`db_access:` 段，v0.1.89）

一句话：`db:` 只定义「有哪些库、怎么连」，`db_access:` 定义「谁能对这些库做什么」。
两者**分开配置**——`db:` 仍是纯粹的 `别名 -> DSN` map，不塞任何策略。

### 11.5.1 配置形态

```yaml
db:                        # 不动：name -> DSN
  default: "sqlite://db.sqlite"
  analytics: "mysql://root@10.0.0.5:3306/olap"

db_access:
  defaults:                # 全局默认档（缺省 = off，与升级前完全一致）
    enforce: off           # off | warn | deny
  analytics:
    read_only: true
    max_rows: 1000         # 缺省 = 不限；范围 [1, 100000]
    allow:                 # 四维并集；缺省 = 不限制
      roles: [admin, dba]
      sources: [http]      # http | task
```

**继承语义**：`db_access.<alias>` 是**覆盖条目**，未声明的键**逐键继承** `defaults`
（`allow` 是**逐维**覆盖）。这不只是一个口味问题——若整条覆盖，运维设好
`defaults.enforce: deny` 后再给某库加一条 `read_only: true`，该库的 `enforce`
会悄悄回落 off：**全局设防被一次局部微调静默解除**。

装配期校验（fail-loud，不静默取其一）：`max_rows` 越界、`read_only: true` +
`allow_ddl: true` 矛盾、未知键（点名 `db_access.<alias>.<key>`）、非法档位。

### 11.5.2 谁可以访问：四维并集

主体四维：`modules`（请求所属模块）/ `roles`（`user.roles`）/ `tenants` /
`sources`（`http` | `task`）。

- 语义是**并集**：任一**已声明**的维度命中即放行；未声明的维度不参与限制。
- ⚠️ **声明多维不会提高强度，只会降低**（任一命中即过）。要「同时满足」本阶段做不到。
- `allow: {}`（或整段不写）= 不限制；`allow: { roles: [] }` = 声明且为空 → **无人可过**。
- fail-closed：声明了某维而请求取不到该维身份 → 该维不命中（**不因「取不到」而放行**）。
- `tenants` 维**只认 server 注入的租户**：`db.asTenant()` 自填的租户不作数
  （否则匿名 handler 可自造租户命中白名单 → 借道跨租户）。
- `db.asSystem()` **不豁免**访问控制（它只绕过租户注入）；管理员通道请走 `allow.roles`。
- `sources` 维是为了解决**任务桥恒无身份**（定时任务没有 user/module/tenant）：
  给它们声明 `sources: [task]`。WS 帧跑在 HTTP 桥上，source 记 `http`
  （**没有 `ws` / `anon` 两个取值**，写了永不命中）。

### 11.5.3 做什么：风险分类与策略位

语句按词法分成五类：`readonly` / `write` / `ddl` / `tx` / `unknown`。
`unknown` 一律按最严档处置（**fail-closed**）——分不出来就当最坏的看。

- `read_only: true` → 拦 `write` 与 `ddl`。
- `allow_ddl: false`（默认）→ 拦 `ddl`。
- `allow_dangerous: false`（默认）→ 拦**危险写**：`UPDATE`/`DELETE` 无 WHERE、
  多表（`update a, b` / JOIN / USING）、恒真谓词（`where 1=1`、`where a=a`）。
- `max_rows` → raw `query` 取回后截断（响应打 `X-OJ-Row-Limit`）；`db.stream`
  达上限**提前终止**；构造器路径取 `min(声明 limit, max_rows)`。

### 11.5.4 档位与自省

`enforce`：`off`（默认，零开销早退）/ `warn`（告警放行，灰度用）/ `deny`（拒）。
`warn` 与 `deny` 都落结构化日志，前缀 **`DB_ACCESS_DENY`**，按 `(alias, reason)`
聚合计数（warn 档逐条落会被刷爆、且可被当成刷盘放大器）。

`db.allowed()` 返回**当前主体可用**的库列表（含各库能力位）：

```js
const list = db.allowed();
// [{ alias: "default", dialect: "sqlite", read_only: false, allow_ddl: false,
//    allow_dangerous: false, max_rows: null, enforce: "off" }]
```

没有这个出口，前端只能列出全部连接、点了才报错——那是**假入口**。

### 11.5.5 承诺 / 不承诺

| 承诺 | 不承诺 |
|---|---|
| 库级授权（谁可以访问哪个 `db` 别名） | 行级 / 列级授权（已有 `SchemaRegistry` 白名单 + 租户守卫，不重叠建设） |
| 语句风险分类 + 危险写门 | 分类是 **best-effort 词法**，不是 SQL 解析器；复杂语句会落 `unknown` 并被拒 |
| `read_only` 位（**宿主侧文本判定**） | **不是**数据库账号权限的替代品——真正的写防线仍是 DB 侧 GRANT |
| `max_rows` 截断（raw）/ 提前终止（stream） | **不是**限流 / DoS 防线：raw 是先取回全量再截断，扫描与传输照旧 |
| 决策可观测（`DB_ACCESS_DENY` + `SqlEvent.risk/operator`） | **不是**审计系统：无防篡改、无持久化、无回溯 |
| `db.allowed()` 只列已授权的库 | 不回显 `allow` 名单（否则 403 响应可被用来枚举库与授权名单） |
| 约束 **JS 提交的 SQL** | **不约束**平台自生成 SQL（`db.nextSeq` 序列表 DDL、`db.listTables` 内省）与 CLI 通道（`oj migrate` / `oj schema` / `oj build` 不经 bridge） |
| 模块 / 角色 / 租户 / 来源 四维白名单 | 无黑名单形态；多维是**并集**（声明更多维只会放宽） |

### 11.5.6 升级须知

- 默认 `enforce: off` → **不配 `db_access:` 与升级前行为完全一致**（包括 `db.asSystem()`
  跑写语句的既有 handler 不受影响）。
- 一旦把某库设成 `deny` 档，用 `db.asSystem()` 跑写语句的 handler **会被拒**
  （asSystem 不豁免访问控制）——这是有意的行为变更。
- `sql_guard=deny` 站点上「raw SELECT 未登记表」的敞口**原样保留**：本策略只管
  alias + risk，只读语句一律放行。不要把本节读成「该敞口已修」。

---

## 12. 延伸阅读

- [`docs/devkit/api-manual.md`](devkit/api-manual.md) —— 权威 API 参考（§6「db / DB(name)」
  含全量字段表与更多示例；§3「模块数据层」讲 schema/migrations 全流程）。
- [`docs/dev-guide.md`](dev-guide.md) —— 内部实现走读（bridge、装配、插件）。
- [`sample/`](../sample/README.md) —— 可运行示例工程（`sample/config.yaml` +
  `sample/src/`，含 raw SQL 与构造器混用的真实 handler）。
