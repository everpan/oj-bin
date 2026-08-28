# 模块自治：表结构 / 种子数据 / 迁移 / 跨模块取数 设计方案

日期：2026-08-28
状态：**待决策**（brainstorming 产出；含 3 个待拍板决策点 D1/D2/D3）

> 本文先做现状调研（第 2 章，全部带 `file:line` 可核），再给决策点与备选（第 3 章），
> 然后按推荐组合展开完整方案（第 4、5 章）。**第 3 章的三个决策未拍板前，第 4 章
> 为推荐组合的展开，不是最终结论。**

## 0. 摘要（TL;DR）

1. 现状是「项目根单个 `seed.sql`，每次启动全量重放，无版本、无账本、无锁、仅 sqlite、
   仅 default 库」，且 `sample/seed.sql` 已经把 5 个不同归属的表混在一起 —— 与
   「模块独立」的目标直接冲突。
2. 有六个既有缺口决定了方案空间，其中最硬的两条：
   **`.sql` 文件根本进不了 `dist/`**（`collect_module` 只收 `*.ts` + `manifest.yaml`），
   **`SchemaRegistry` 在生产运行时是空的**（`db.table()` 100% 报 `unknown table`）。
3. 推荐组合：`schema.yaml` 声明式表结构（D1=C）+ 四层跨模块取数菜单（D2 全选）
   + release 显式 `oj migrate`、server 只校验（D3=显式）。
4. 检查体系建议新增 `oj check` 子命令，规则分三层（结构 / 账本 / 漂移），并复用
   已有的 build 与启动两道闸。
5. 建议按 P0→P3 四步落地，P0 只解决「模块自带 SQL 能进 dist 并被重放」，不引入
   迁移概念，可独立交付。

---

## 1. 背景与目标

`oj` 的模块 = `src/` 首层子目录，每个模块必须有 `manifest.yaml`
（`name/desc/version/config`），`name` 必须等于目录名（`oj/src/manifest.rs:69-99`）。
模块下任意深度的 `api.ts` 目录镜像成路由。

**目标规则**（用户给定，本文要让它可落地、可检查）：

- R1 每个模块是独立的，各自拥有自己的**表定义**与**操作**；
- R2 每个模块的**种子数据与表结构**是独立的；
- R3 模块之间需要相互调用数据 —— 需要有受控的解决方式；
- R4 模块独立版本发展 —— 其表结构的迁移与数据变化需要被管理；
- R5 以上规则需要能被**检查**。

---

## 2. 现状调研

### 2.1 seed.sql 的完整链路

| 环节 | 事实 | 位置 |
|---|---|---|
| 发现 | `config_dir/seed.sql`（config.yaml 所在目录）；不存在则跳过 | `oj/src/app.rs:128` |
| 执行条件 | 仅 `dbs["default"]` 且 `dialect() == Dialect::Sqlite`；否则打印 `warn: seed.sql skipped` | `oj/src/app.rs:130-141` |
| 切分 | `text.split(';')` 朴素切分 → **语句内不得含分号字面量** | `oj/src/app.rs:133` |
| 幂等 | 全靠手写 `CREATE TABLE IF NOT EXISTS` / `INSERT OR IGNORE` | `sample/seed.sql:1-12` |
| 建表 | 走 `db.exec_with_params` → `sqlx::query(AssertSqlSafe(sql))`，**无任何关键字限制** | `src/bridge/accessor_sqlx.rs:206-216` |
| 版本 | **无**。无迁移表、无 checksum、无锁、无 down 路径 | 全仓 grep `migrat`/`user_version`/`schema_migrations` 零命中 |
| 构建 | `oj build` **不执行**（db 用 `sqlite::memory:`，构建零磁盘副作用） | `oj/src/build_cmd.rs:224` |
| 测试 | `oj test` **执行**（走 `App::from_config`） | `oj/src/test_cmd.rs:88` |
| 热重载 | 与 seed 无关；seed 仅启动重放，改了要重启 | `docs/ops-manual.md:92` |

补充事实：dev 模式**没有 notify 监听 `src/`**（notify 只用于证书 watcher），
真实热重载是「每请求重新解析 specifier，`?v=<mtime>` 变了就当新模块」
（`src/bridge/module_loader.rs:223-236`）。这意味着**新增 `.sql` 文件不会触发任何重载**。

### 2.2 六个关键缺口

**G1. `.sql` 进不了 `dist/` —— 模块自带 SQL 当前无处安放。**

```rust
// oj/src/build_cmd.rs:268
if is_api || name.ends_with(".ts") || name == "manifest.yaml" {
```

`walk` 递归收集时只保留 `*.ts` 与任意层级的 `manifest.yaml`（`build_cmd.rs:249-274`）；
落盘时 `.yaml` 原样 copy（`:119-122`），`.ts` 转译后写 `.js`。
**`schema.sql` / `seed.sql` / `migrations/*.sql` 一律不会进产物目录。**
对比：`sample/dist/user-0.1.0/` 里也没有 README.md，印证了同一规则。

**G2. `manifest.config` 是死字段 —— 天然的扩展位。**

```rust
// oj/src/manifest.rs:11-12
#[serde(default)]
pub struct config: serde_yaml::Value,
```

已声明、可解析、但全仓**无任何读取点**。要加 `tables` / `deps` / `db` 声明，改这里成本最低。

**G3. `SchemaRegistry` 在生产运行时是空的 —— `db.table()` 不可用。**

```rust
// oj/src/app.rs:179 —— Bridge 工厂
SchemaRegistry::new(),
```

注册表唯一的填充方式是构造期 builder `SchemaRegistry::new().table(name, pk, cols)`
（`src/bridge/registry.rs:48`），**没有任何 op 能注册，也没有启动内省填充**。
于是 `op_db_query_build` 必然走到：

```rust
// src/bridge/query.rs:156-158
let table = reg.get(&req.table)
    .ok_or_else(|| JsErrorBox::generic(format!("unknown table '{}'", req.table)))?;
```

结果：生产环境 `db.table(...)` 100% 报错，所有业务只能裸 SQL
（见 `sample/src/user/account/api.ts:6`）。
> 文档漂移提醒：`docs/devkit/api-manual.md:423` 声称表名来自「启动内省所得」，
> 代码中不存在该能力。

**G4. `db.exec` 零限制，DDL 随便跑。**
`sqlx::query(AssertSqlSafe(sql))` 直接执行（`src/bridge/accessor_sqlx.rs:207`），
无关键字黑名单、无 DDL 拦截、无只读判定。所以「模块能否自己建表」技术上畅通无阻，
缺的只是**管理机制**与**约束**。

**G5. 跨模块今天就是裸 JOIN + 跨模块 import。**

```ts
// sample/src/order/list/api.ts:1-11
import { requireRole } from "../../user/_shared/validate";   // 跨模块代码 import
db.query(`select ... from orders o join account a on a.id = o.account_id ...`)
//                                       ^^^^^^^ 直接摸 user 模块的表
```

- 跨模块 **import** 是受支持的：`fix_relative_imports` 会按 `dist/manifests.yaml` 锁把
  `../user/_shared/x` 重写成 `../user-0.1.0/x.js`，版本缺失则 fail-fast
  （`oj/src/build_cmd.rs:329-345`）。**注意：这已经是一条「模块版本耦合」的边。**
- 跨模块 import `api.ts` **被禁**：`guard_no_api_imports` 按 basename 判 `api` 即拒
  （`oj/src/build_cmd.rs:454-475`），理由是 `api.ts` 是路由入口而非可复用模块。
- 跨模块 **取数** 完全无约束 —— 裸 SQL 想 join 谁就 join 谁。

**G6. 迁移基建为零。** 无版本表、无 up/down、无 DDL diff、无 checksum、无并发锁。
sqlite 侧 `max_connections(1)`（`accessor_sqlx.rs:32-34`）间接起了点串行作用，mysql/pg 无。

### 2.3 现状已经违规的地方（sample 实证）

`sample/seed.sql` 一个文件里混了 5 张表、4 个归属：

| 表 | 实际归属 | 备注 |
|---|---|---|
| `account`、`orders` | `user` 模块 / `order` 模块 | 且 `order` 还 join 了 `account` |
| `tenant` | **框架**（多租户，`config.yaml` 的 `tenant` 段） | 不属于任何业务模块 |
| `users` | **框架**（`auth.user_table: users`） | 不属于任何业务模块 |
| `certs` | `cert` 模块 | |

另有两处体验级问题：

- **演示数据每次启动复活**：删掉 `account id=1 neo` 后重启，`INSERT OR IGNORE`
  会把它插回来。演示数据与「期望状态数据」语义混在一起。
- **仅 sqlite**：换 mysql/pg 后 seed 整个失效，建表归运维 —— 与「模块自带表结构」目标冲突。

---

## 3. 决策点（待拍板）

### D1. 模块表结构的声明方式

| | 方案 | 做法 | 优点 | 代价 |
|---|---|---|---|---|
| A | 最小改动 | 每模块 `schema.sql` + `seed.sql`，启动时按模块顺序幂等重放 | 改动最小、心智与今天一致 | **无迁移能力**；**仍仅 sqlite**；拿不到机器可读的归属图 |
| B | 迁移式 | 每模块 `migrations/0001__init.sql…` + 账本，只向前 | 工业标准、语义精确、有账本 | 三方言要手写；**拿不到声明式归属图**，检查只能靠正则 |
| **C** | **声明式（推荐）** | `schema.yaml` 声明表/列/索引 → 引擎按方言生成 DDL；破坏性变更强制手写迁移 | ① 方言问题一并解决 ② **填饱 SchemaRegistry，`db.table()` 复活** ③ 产出**表→模块归属图**，是 R5 检查的基石 | 需写 YAML schema → DDL 生成器；破坏性变更需人工兜底 |

**推荐 C，并以 B 作为执行机制**（C 为源、B 为执行）：

- `schema.yaml` 自动推导**安全前向**步骤：加表、加可空列、加索引、放宽类型；
- 无法安全推导的（删表/删列、收窄类型、NOT NULL 且无默认值、疑似改名）
  → **fail-fast，要求手写 `migrations/NNNN__desc.sql`**。
- 理由：只有 C 能同时给出「表归属图」与「列白名单」，而这两者正是 R1/R5 的落点；
  B 单独使用会让检查退化成正则猜测。

### D2. 跨模块取数允许哪些机制（可多选）

四层是**互补**关系，不是四选一。建议全部写入规范，按场景选用：

| 层 | 机制 | 适用 | 一致性 | 代价 |
|---|---|---|---|---|
| 1 | **契约调用（同步）** | 取单条/少量、需要强一致 | 强 | 列表场景 N+1 |
| 2 | **事件驱动读模型（异步）** | 列表聚合、跨模块 JOIN | 最终一致 | 要写订阅与冗余表 |
| 3 | **声明依赖 + 只读视图** | 同库部署、确需 SQL JOIN | 强（读） | 引入跨模块耦合，需显式声明 |
| 4 | **共享内核上收** | 多模块共有表（如 tenant） | — | 需要一个 `_platform` 伪模块 |

1. **契约调用**：不 import 别人的 `api.ts`（已被 `guard_no_api_imports` 禁），
   改由模块声明 `_public/contract.ts` 导出纯函数供他模块 import，build 时按锁钉版本
   （复用 `build_cmd.rs:329-345` 已有能力，**零改动**）。
   或新增进程内 `oj.call(module, path, req)` 走 RouteTable 派发，思路可复用
   `oj/src/test_ext.rs:51-103` 的免 TCP dispatch，并自动透传 tenant / auth 上下文
   （比 `fetch` 回环少一次 TCP + 少一次鉴权）。
2. **读模型**：订阅 `bus`（`src/bridge/bootstrap.js:129-132` 已有 `bus.publish/subscribe`），
   在自己表里维护冗余快照。**这是 `order/list` 那类列表 JOIN 的正解。**
3. **只读视图**：manifest 声明 `deps: {user: "^0.1.0"}`，同库部署时 `user` 发布
   `v_user_account` 视图，`order` 只读 SELECT；**禁写基表、禁直接摸基表**。
4. **共享内核**：`tenant`、`users` 这类框架表显式归 `_platform` 伪模块，
   不再散在根 `seed.sql`（解决 2.3 的归属混乱）。

**禁止**：裸 SQL 跨表（今天 `order/list` 的做法），由归属图在 build / run 时拦截。

### D3. release 环境迁移的执行时机

| | 方案 | dev | release | 评价 |
|---|---|---|---|---|
| **a** | **显式 `oj migrate` + 启动校验（推荐）** | 启动自动 apply | `oj migrate`（带锁、单飞），server 启动**只校验**，账本落后即 fail-fast 并提示命令 | 多实例部署安全，发布节奏可控 |
| b | 全部启动自动（带锁） | 自动 | 自动 + advisory lock | 沿用今天的手感，改动小；滚动发布期存在版本错配窗口 |
| c | 保持现状全自动（不加锁） | 自动 | 自动 | 多实例与非 sqlite 场景不可靠，不建议 |

推荐 a。理由：server 启动即写 DDL 在多实例下是事故源；把迁移提成显式步骤后，
「部署」= `oj migrate && oj server`，失败点前移且可重试。
注意：这会改变 `release` 模式的行为（今天根目录 `seed.sql` 是静默重放的），
需要一个 `--no-migrate` / `migrate_on_start` 配置项做逃生门。

---

## 4. 推荐方案详述（D1=C ／ D2=全选 ／ D3=a）

### 4.1 目录布局

```
<project>/
├── config.yaml
├── seed.sql                  # 【保留一段时期，标 deprecated】全局遗留种子
├── _platform/                # 伪模块：框架/多模块共有表（tenant / users…）
│   ├── manifest.yaml         # name: _platform
│   ├── schema.yaml
│   └── migrations/0001__init.sql
└── src/
    ├── user/                 # 模块 = 首层子目录（_platform 除外）
    │   ├── manifest.yaml     # 扩展：tables / deps / db
    │   ├── schema.yaml       # 【新】声明式表结构（源）
    │   ├── seed.sql          # 【新】期望状态数据（幂等 upsert，每启动重放）
    │   ├── fixtures/         # 【新】仅 dev/test 演示数据，不随启动重放
    │   │   └── demo.sql
    │   ├── migrations/       # 【新】有序、只向前
    │   │   ├── 0001__init.sql
    │   │   └── 0007__add_email.sql
    │   ├── _shared/validate.ts
    │   ├── _public/contract.ts   # 【新】对外契约（可被他模块 import）
    │   └── account/api.ts
    └── order/
        └── list/api.ts
```

### 4.2 manifest 扩展

复用已存在但未被读取的 `config` 字段，或提为顶层字段（建议顶层，语义更清晰）：

```yaml
name: "order"
desc: "订单：建单、列表联查、详情缓存"
version: "0.2.0"

# ↓ 新增
db: "default"                 # 本模块绑定的库名（默认 "default"；对应 DB(name)）
tables:                       # 本模块拥有的表（与 schema.yaml 必须一致，由 oj check 校验）
  - orders
  - order_item
deps:                         # 声明式跨模块依赖（版本范围，由 dist/manifests.yaml 满足）
  user: "^0.1.0"
schema_head: 7                # 本模块 migrations 的最大 seq（build 时写入，release 校验用）
```

- `db:` 让模块能绑定自己的库（当前只能写死在 handler 里的 `DB("analytics")`）；
- `tables:` 与 `schema.yaml` 双向校验，避免声明漂移；
- `deps:` 是 R3 的检查依据，语义与 `build_cmd.rs:334-340` 已有的跨模块 import
  版本解析对齐。

### 4.3 版本模型：module version 与 schema seq 分离

- **module version**（`manifest.version`）：管路由与产物目录 `dist/<m>-<v>/`。
- **schema seq**（`migrations/NNNN__x.sql` 的数字前缀）：**模块内单调递增，跨版本不回退**。

为什么分离：改一次表不必强发一个新的产物目录；但在装了 `user 0.2.0` 时能表达
「它的 schema 到 seq 7」。若把两者绑死（版本一变就换目录），dist 会迅速堆积版本目录
（现有逻辑：同版本重建先 `remove_dir_all`，不同版本并存，`build_cmd.rs:89-94`）。

### 4.4 迁移账本

```sql
CREATE TABLE IF NOT EXISTS _oj_migrations (
  module     TEXT NOT NULL,
  seq        INTEGER NOT NULL,
  name       TEXT NOT NULL,
  checksum   TEXT NOT NULL,          -- 文件内容 sha256，防篡改
  dialect    TEXT NOT NULL,
  applied_at INTEGER NOT NULL,
  PRIMARY KEY (module, seq)
);
```

- `seq` **模块内全局单调**，不按 module version 归零 → 回退到旧版本时能检出
  「账本超前于模块」（见 S007）。
- 应用前查 `(module, seq)` 存在即跳过；成功后写账本。
- 事务性：**postgres / sqlite 支持 DDL 事务**，可与账本行同事务提交；
  **mysql DDL 隐式提交**，只能「先跑 DDL，再写账本」，失败时留下脏状态需人工介入
  （在文档中明确标注该方言差异）。

### 4.5 三类数据的语义边界（重要，当前最大的设计债）

| 类别 | 语义 | 执行次数 | 内容 |
|---|---|---|---|
| `migrations/*.sql` | **一次性**变更（DDL + 数据回填） | 账本保证只跑一次 | `alter table`、`update ... set ...` 回填 |
| `seed.sql` | **期望状态**（幂等 upsert） | 每启动重放 | 枚举表、参考数据、默认配置行 |
| `fixtures/*.sql` | **演示数据**（仅 dev/test） | 仅 `oj fixture load` 或建库时 | `neo` / `trinity` 这类演示账号 |

这条划分直接修掉 2.3 里「删掉的数据重启后复活」的问题：演示数据从 `seed.sql`
迁到 `fixtures/` 后不再随启动重放。

### 4.6 迁移执行与并发锁

- **dev**：启动时自动 apply 全部待应用迁移（沿用今天 seed 的手感）。
- **release**：默认 `oj migrate`（显式）；`oj server` 启动**只校验**，
  账本落后 → fail-fast 并打印应执行的命令。
- **并发**：任何 apply 前取 advisory lock，避免多实例同时迁移：
  - sqlite：独占事务（现有 `max_connections(1)` 已近似串行，但仍需显式保证）
  - mysql：`SELECT GET_LOCK('oj_migrate', timeout)`
  - postgres：`pg_advisory_lock(hashtext('oj_migrate'))`
- 逃生门：配置项 `migrate_on_start: auto | verify | off`（dev 默认 `auto`，
  release 默认 `verify`）。

### 4.7 方言策略

- `schema.yaml` → 生成器产出三方言 DDL（sqlite / mysql / postgres），**这是根治
  「seed 仅 sqlite」的关键**。
- 手写迁移支持方言覆盖文件：`0007.pg.sql` / `0007.mysql.sql`；缺省回落
  `0007.sql`（保持现状的 sqlite 语义）。
- 裸 SQL 占位符差异归业务（`src/bridge/db.rs:24` 注释已明确：`?` vs `$1`）。

### 4.8 表归属图与 SchemaRegistry 复活

启动时汇总所有模块（含 `_platform`）的 `schema.yaml` → 得到
`table → module` 的**归属图**（必须单射，同名表被两模块声明 = S002 错误）。

一举两得：

1. 归属图 → 运行时守卫 + 静态检查的依据（R5）；
2. 同一份数据填入 `SchemaRegistry`（替换 `oj/src/app.rs:179` 的 `SchemaRegistry::new()`）
   → **`db.table()` 终于可用**，且天然只能访问已声明的表；列白名单
   （`src/bridge/query.rs:156-200`）也一并生效。

> 这一步把「SQL 注入红线」从「只靠人写绑定参数」升级为「标识符也来自声明」，
> 与 `CLAUDE.md` 的设计红线方向一致。

### 4.9 `.sql` 如何进 dist（G1 的解法）

扩 `oj/src/build_cmd.rs:268` 的收集白名单：`*.ts` / `manifest.yaml`
**+ `schema.yaml` + `seed.sql` + `migrations/*.sql`**（`fixtures/` 可选，建议**不进** release 产物）。

落盘分支扩一条：`.sql` / `schema.yaml` 原样 copy（同 `:119-122` 的 `.yaml` 处理）。
release 模式已按 `dist/<m>-<v>/manifest.yaml` 发现模块（`oj/src/app.rs:203-247`），
因此迁移文件随版本目录一起发布，天然版本化。

---

## 5. 规则检查体系（R5）

### 5.1 规则清单

新增 `oj check` 子命令（CI 门禁，有违规即非零退出），规则分三层：

**结构层（`oj build` 内跑 → fail build）**

| ID | 规则 |
|---|---|
| S001 | 模块缺 `manifest.yaml`（已有，`oj/src/manifest.rs:84-86`） |
| S002 | 同名表被两个模块声明（归属图必须单射） |
| S003 | 模块 SQL 引用了非本模块表，且未在 `deps:` 声明 |
| S004 | `deps:` 的版本范围不被 `dist/manifests.yaml` 满足（扩展 `oj/src/build_cmd.rs:334-340`） |
| S005 | `manifest.tables` 与 `schema.yaml` 不一致 |
| S006 | `seed.sql` 含 DDL（应走 `migrations/`）/ 含非幂等 INSERT / 触碰非本模块表 |
| S007 | `migrations/` 序号空洞、重复或乱序 |

**账本层（`oj migrate` 与 server 启动 → fail-fast）**

| ID | 规则 |
|---|---|
| M001 | 已应用迁移的 `checksum` 被篡改 |
| M002 | 账本超前于模块（模块被降级，本地 `migrations/` 缺少已应用的 seq） |
| M003 | `manifest.schema_head` 小于账本中该模块的最大 seq（release 校验） |
| M004 | 存在待应用迁移但当前为 `verify` 模式 |

**漂移层（`oj schema diff`，只读报告）**

| ID | 规则 |
|---|---|
| D001 | 声明的 schema 与实库（`sqlite_master` / `information_schema`）存在差异 |
| D002 | 实库存在未被任何模块声明的表 |

### 5.2 各命令承担的检查

```
oj check     → 全部 S*（+ 可选 D*）        # CI 门禁
oj build     → S002/S003/S004/S005/S006/S007 → fail build
oj migrate   → M001/M002/M003               → apply + 写账本
oj server    → release 模式 M004；dev 模式自动 apply
oj test      → 走 App::from_config，迁移+种子+fixtures 全跑一遍
oj schema diff → D001/D002
```

CI 建议串法：`oj check && oj build && oj test`。

### 5.3 SQL 表名提取的实现选型

S003 / S006 / 运行时守卫都依赖「从 SQL 字符串里取出表名」。

| 选项 | 做法 | 评价 |
|---|---|---|
| 加 `sqlparser` 依赖 | 真正的 SQL 解析，按方言 parse 后遍历 `TableFactor` | 准，但引入重量级依赖（当前 `Cargo.toml` 无此依赖） |
| 轻量正则 | 抓 `FROM` / `JOIN` / `INTO` / `UPDATE` 后的标识符 | 无新依赖；有误报，需 `/* oj:allow-table=x */` 豁免注释兜底 |

建议：**先用正则 + 豁免注释**落地（`db.query` 里大量模板字符串拼 SQL，
静态解析本就是 best-effort），若误报不可接受再引入 `sqlparser`。

运行时守卫（可选开关，dev/test 默认开）：
`op_db_query` / `op_db_exec` 提取表名 → 非本模块且非 `deps` 声明 → 拒绝。
按 SQL 字符串做 memo 缓存，避免每条查询重复解析。

---

## 6. 落地顺序

| 阶段 | 内容 | 产出 | 风险 |
|---|---|---|---|
| **P0** | 扩 `collect_module` 白名单让 `.sql`/`schema.yaml` 进 dist；启动时按模块顺序重放 `schema.sql` + `seed.sql`（语义等同今日全局 seed，仅拆到模块） | 模块自带 SQL 可用，**不引入迁移概念** | 低；可独立交付，向后兼容根 `seed.sql` |
| **P1** | `migrations/` + 账本表 + `oj migrate` 子命令 + advisory lock + `fixtures/` 分离 | 迁移能力落地，D3 落地 | 中；需处理 mysql DDL 隐式提交 |
| **P2** | `schema.yaml` 声明式 + 三方言 DDL 生成 + 归属图 + 填充 SchemaRegistry（`db.table()` 复活）+ 运行时归属守卫 | 根治方言问题，R1/R5 有落点 | 高；改动触及 `app.rs`/`registry.rs`/`query.rs` |
| **P3** | `oj check` 全套规则 + `oj schema diff` + `_platform` 伪模块迁移 + 三册文档更新 | 完整检查体系 | 低 |

P0 与 P1 可先于 D1 决策启动（无论最终选 A/B/C 都需要）；**P2 依赖 D1=C**。

---

## 7. 对 sample 的具体改造处方

1. **拆 `sample/seed.sql`**：
   - `tenant` / `users` → `_platform/`（框架归属，`config.yaml` 的 `tenant` 与
     `auth.user_table` 引用它）
   - `account` → `src/user/`；`orders` → `src/order/`；`certs` → `src/cert/`
2. **演示数据搬进 `fixtures/`**：`neo` / `trinity` / `A-0001` / `demo` 用户
   → 不再随启动重放，改由 `oj fixture load` 灌入（测试流程里保留）。
3. **修掉 `order/list` 的裸 JOIN**（`sample/src/order/list/api.ts:6-11`）：
   - 首选**读模型**：`orders` 表冗余 `account_name` / `account_role` 两列，
     由 `user` 模块的变更事件（`bus`）驱动更新；列表查询不再 JOIN。
   - 次选**只读视图**：`user` 发布 `v_user_account`，`order` 的 manifest 声明
     `deps: {user: "^0.1.0"}` 后只读 SELECT。
4. **`order` 的 `requireRole` 跨模块 import**：保持现状（机制已支持、build 按锁钉版本），
   但建议在 manifest 里补 `deps: {user: "^0.1.0"}` 使其**显式化、可检查**。

---

## 8. 风险与未决问题

| # | 问题 | 说明 |
|---|---|---|
| 1 | **向后兼容** | 根 `seed.sql` 与模块 `seed.sql` 并存期如何取舍？建议：并存一期、打印 deprecation，下一期移除 |
| 2 | **模块应用顺序** | 有 `deps` 的模块必须后于被依赖模块建表/迁移 → 需拓扑排序，环则 fail-fast |
| 3 | **跨库** | `manifest.db` 若允许模块绑不同库，则「跨模块 JOIN / 视图」在同库才成立，跨库只能走契约调用或读模型。归属图需带库名维度 |
| 4 | **多租户** | `tenant` 注入是请求级的（`config.yaml` 的 `tenant` 段），种子数据是否按租户灌？当前 seed 无租户维度，需在方案里明确（建议：seed 只灌租户无关数据） |
| 5 | **`db.table()` 复活的影响面** | 现在所有业务都是裸 SQL，启用归属守卫后**存量代码可能大面积报错**。需要一个「先 warn 后 deny」的灰度开关 |
| 6 | **破坏性变更检测** | 删列 vs 改名的歧义无法自动判定，必须人工写迁移 —— 需要清晰的报错文案与操作指引 |
| 7 | **`oj test` 成本** | 测试走 `App::from_config`，迁移一多会拖慢测试启动。建议测试场景支持「从 schema.yaml 一次性建库」的快路径 |
| 8 | **`_platform` 与框架内置表的冲突** | `auth.user_table: users` 目前从 config 读；若 `users` 归 `_platform`，需明确 config 与 `_platform` 谁为准 |
