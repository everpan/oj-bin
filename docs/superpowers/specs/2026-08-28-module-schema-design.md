# 模块自治：表结构 / 种子数据 / 迁移 / 跨模块取数 设计方案

日期：2026-08-28
状态：**待批准**（brainstorming 产出；含 4 个决策点 D1–D4；§11 为推荐结论，
2026-08-29 经架构/产品/研发三方评审修订——记录见 §12，决策请求见 §11.6）

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
6. **迁移执行面的最终结论在 §11**：用 `refinery-core` 适配器（零驱动、跨 dylib
   边界、天然支持模块级独立账本），而非手写引擎；除非项目有「禁止新增依赖」
   硬约束。§9 是被 §10/§11 修正前的调研记录，保留作决策依据。
7. 2026-08-29 经架构/产品/研发三方评审修订（§12 记录，32 条意见采纳 28）：
   修正锁的会话级语义、适配器多语句拆分与装配路径、`_platform` 布局等 6 处高危
   问题；待决事项升格为 §11.6 决策请求（Q1–Q7）。

> **行动指引**：只看结论读 §11（决策请求在 §11.6）；要复核依据读 §2 + §9 + §10；
> 要了解决策空间读 §3；要看评审修订记录读 §12。

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
| 构建 | `oj build` **不执行**（db 用 `sqlite::memory:`，构建零磁盘副作用） | `oj/src/build_cmd.rs:214` |
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

- `schema.yaml` 自动推导**安全前向**步骤：加表、加可空列、加索引。
  **不做「放宽类型」**：sqlite 无 `ALTER COLUMN`（改类型需整表重建），pg `ALTER TYPE`
  可能触发表重写锁——凡涉列类型变更一律走手写迁移。
- 无法安全推导的（删表/删列、收窄/放宽/变更类型、NOT NULL 且无默认值、疑似改名）
  → **fail-fast，要求手写 `migrations/NNNN__desc.sql`**。改名流程闭环：改 yaml 列名 →
  `oj migrate` 报「疑似改名（old↔new）」并打印迁移模板 → 写迁移文件 → 复跑通过
  （此时实库列名已与新 schema 一致，对账不再报错；报错三要素见 §5.1）。
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

1. **契约调用**（默认与首选）：不 import 别人的 `api.ts`（已被 `guard_no_api_imports` 禁），
   改由模块声明 `_public/contract.ts` 导出纯函数供他模块 import，build 时按锁钉版本
   （复用 `build_cmd.rs:329-345` 已有能力，**零改动**）。
   进程内 `oj.call(module, path, req)` 列入 backlog：**不能**复用 `op_client_dispatch`
   （`oj/src/test_ext.rs:66-69` 派发前 reset 外层 `ReqState`——嵌套调用会回滚进行中的
   事务；且该 op 注册在独立 test extension，生产 runtime 未装载），需要新的
   nested-safe op，待真实需求再立项。
2. **读模型**：订阅 `bus`（`src/bridge/bootstrap.js:129-132` 已有 `bus.publish/subscribe`），
   在自己表里维护冗余快照。**这是 `order/list` 那类列表 JOIN 的正解。**
3. **只读视图**：manifest 声明 `deps: {user: "^0.1.0"}`，同库部署时 `user` 发布
   `v_user_account` 视图，`order` 只读 SELECT；**禁写基表、禁直接摸基表**。
4. **共享内核**：`tenant`、`users` 这类框架表显式归 `_platform` 伪模块，
   不再散在根 `seed.sql`（解决 2.3 的归属混乱）。

**禁止**：裸 SQL 跨表（今天 `order/list` 的做法），由归属图在 build / run 时拦截。

**场景 → 机制决策表**（S003 报错文案必须附对应行建议）：

| 你的场景 | 用哪层 |
|---|---|
| 取单条/少量数据、要强一致 | 1 契约调用 |
| 列表聚合、跨模块 JOIN | 2 读模型 |
| 同库部署、确需 SQL JOIN 且接受只读 | 3 声明依赖 + 只读视图 |
| 多模块共有框架表（tenant / users） | 4 上收 `_platform` |

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

**运维约束（有意为之，须知）**：D3=a 使**降版部署被设计禁止**——M002（`abort_missing`）
与 M003（`schema_head` 是版本目录属性）共同保证旧版本目录无法通过启动校验。回滚的
唯一支持路径是「旧代码 + 新的前向迁移」；人工回退账本（`DELETE FROM _oj_migrations_<m>`
行）必须同步手工回退 DDL 且自担数据丢失风险，操作指引随 P1 文档交付。

---

## 4. 推荐方案详述（D1=C ／ D2=全选 ／ D3=a）

### 4.1 目录布局

```
<project>/
├── config.yaml
├── seed.sql                  # 【保留一段时期，标 deprecated】全局遗留种子
└── src/
    ├── _platform/            # 框架/多模块共有表（tenant / users…）——普通模块、零特例：
    │   ├── manifest.yaml     #   name: _platform；无 api.ts 即无路由，
    │   ├── schema.yaml       #   build / 发现 / 迁移与其他模块同一路径
    │   └── migrations/0001__init.sql
    ├── user/                 # 模块 = 首层子目录
    │   ├── manifest.yaml     # 纯手写（生成物一律进 dist/manifests.yaml，见 4.2）
    │   ├── schema.yaml       # 【新】声明式表结构（源）
    │   ├── seed.sql          # 【新】期望状态数据（幂等 upsert，每启动重放）
    │   ├── fixtures/         # 【新】演示数据：oj test 自动灌入、不进 release 产物、不随启动重放
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

> 布局注：`_platform` 曾考虑放项目根（与 src/ 平级），评审否决——`oj build -d` 与
> dev/release 两套发现链路都以 `-d` 目录首层子目录为模块根，根级目录进不了
> build/dist/重放。`validate_module` 允许下划线（`manifest.rs:16-25`），
> 做普通模块毫无障碍。

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
```

- `db:` 让模块能绑定自己的库（当前只能写死在 handler 里的 `DB("analytics")`）；
- `tables:` 与 `schema.yaml` 双向校验，避免声明漂移；
- `deps:` 是 R3 的检查依据，语义与 `build_cmd.rs:334-340` 已有的跨模块 import
  版本解析对齐；
- `schema_head`（本模块 migrations 最大 seq）**不写进 manifest.yaml**——manifest
  保持纯手写；由 `oj build` 写入生成的 `dist/manifests.yaml` 模块条目，M003 从
  那里读取（生成物与手写物分离，避免 build 回写造成的合并冲突与 diff 噪声）。

#### 4.2.1 `schema.yaml` 示例（评估书写成本用）

```yaml
# src/user/schema.yaml
tables:
  account:
    pk: id
    columns:
      id: { type: integer, autoincrement: true }
      name: { type: text, null: false }
      role: { type: text, null: false }
    indexes:
      idx_account_name: [name]
```

生成（sqlite）：`CREATE TABLE account (id INTEGER PRIMARY KEY AUTOINCREMENT,
name TEXT NOT NULL, role TEXT NOT NULL); CREATE INDEX idx_account_name ON account(name);`
（order 模块的 `orders` 同型；`db.table()` 复活后列白名单即取自此处的 `columns`。）

### 4.3 版本模型：module version 与 schema seq 分离

- **module version**（`manifest.version`）：管路由与产物目录 `dist/<m>-<v>/`。
- **schema seq**（`migrations/NNNN__x.sql` 的数字前缀）：**模块内单调递增，跨版本不回退**。

为什么分离：改一次表不必强发一个新的产物目录；但在装了 `user 0.2.0` 时能表达
「它的 schema 到 seq 7」。若把两者绑死（版本一变就换目录），dist 会迅速堆积版本目录
（现有逻辑：同版本重建先 `remove_dir_all`，不同版本并存，`build_cmd.rs:91-93`）。

### 4.4 迁移账本

> **已被 §11.3 修订**：最终账本形态为每模块一张 `_oj_migrations_<module>`，
> checksum 改由 refinery 计算（SipHash13）、无 `success` 列。本节保留原始推演。

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
| `fixtures/*.sql` | **演示数据**（仅 dev/test） | **`oj test` 自动灌入**；`oj fixture load` 供 dev 手动补数；不随启动重放 | `neo` / `trinity` 这类演示账号（现网 cert 测试的 `trinity` 账号即靠 test 灌入，见 §5.2） |

这条划分直接修掉 2.3 里「删掉的数据重启后复活」的问题：演示数据从 `seed.sql`
迁到 `fixtures/` 后不再随启动重放。

### 4.6 迁移执行与并发锁

- **dev**：启动时自动 apply 全部待应用迁移（沿用今天 seed 的手感）。
- **release**：默认 `oj migrate`（显式）；`oj server` 启动**只校验**，
  账本落后 → fail-fast 并打印应执行的命令。
- **并发**（锁必须是**会话级正确**的——pg `pg_advisory_lock` 与 mysql `GET_LOCK`
  均为**连接作用域**：经池逐条 exec 取锁，锁会留在连接 A 而 DDL 跑在连接 B，
  池回收 A 即静默丢锁、B 上 unlock 是 no-op，互斥形同虚设）：
  - postgres：`SELECT pg_advisory_xact_lock(<id>)` 作为每条迁移事务的**首条语句**
    （事务结束自动释放、无泄漏路径；每迁移事务重取为免费重入；并发双跑另由
    账本 `version` PRIMARY KEY 冲突兜底报错）
  - mysql：连接级 `GET_LOCK` 在本契约下无法保证同连接 unlock，**不做连接锁**；
    互斥靠账本 `version` PRIMARY KEY 冲突（第二实例写同一 version 行即失败中止）
  - sqlite：不取锁（`max_connections(1)` 天然串行，账本写入在事务内）
- 逃生门：配置项 `migrate_on_start: auto | verify | off`（dev 默认 `auto`，
  release 默认 `verify`）。
- **首次部署**：空库 + 空账本在 release `verify` 模式同样判 M004 拒启（设计行为）；
  报错文案必须打印完整命令 `oj migrate -c <config> -d <dir>`。README/quick start
  随 P1 更新为 `oj build && oj migrate && oj server`。

### 4.7 方言策略

- `schema.yaml` → 生成器产出三方言 DDL（sqlite / mysql / postgres），**这是根治
  「seed 仅 sqlite」的关键**。生成器基于**已在依赖中的 `sea-query` 1.0**
  （根 `Cargo.toml:29`，`query.rs` 构造器在用）：`Table::create` + 三方言
  QueryBuilder 即底座，`schema.yaml` 仅做映射层，无需手写 DDL 拼接。
- 手写迁移支持方言覆盖文件：`0007.pg.sql` / `0007.mysql.sql`；缺省回落
  `0007.sql`（保持现状的 sqlite 语义）。载入侧按当前 `Dialect` **过滤后只取其一**
  ——同 seq 两个文件并存会被 refinery 判 `RepeatedVersion`；两种文件的
  (seq, desc) 映射必须一致（决定账本 `name` 列与 M001 对比）。
- 裸 SQL 占位符差异归业务（`src/bridge/db.rs:24` 注释已明确：`?` vs `$1`）。

### 4.8 表归属图与 SchemaRegistry 复活

启动时汇总所有模块（含 `_platform`）的 `schema.yaml` → 得到
`table → module` 的**归属图**（必须单射，同名表被两模块声明 = S002 错误）。

一举两得：

1. 归属图 → 运行时守卫 + 静态检查的依据（R5）；
2. 同一份数据填入 `SchemaRegistry` → **`db.table()` 终于可用**，且天然只能访问
   已声明的表；列白名单（`src/bridge/query.rs:156-200`）也一并生效。装配上
   **两处**空注册表一并替换：`oj/src/app.rs:179`（make_bridge 闭包，server 池）
   **与 `:353`（App.stable，`oj test` 派发）**——在 `from_config` 构造一次
   （`SchemaRegistry: Clone`）两处共享；只改一处会造成 server 可用而
   `oj test` 仍 100% `unknown table` 的分裂。另注：schema.yaml 变更**不参与
   热重载**（notify 只盯证书，dev 热重载按 `?v=<mtime>` 只覆盖 .ts），dev 下
   同样需重启生效。

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

> **已被 §11.4 修订**：M001/M002 改由 refinery 的 `abort_divergent` / `abort_missing`
> 承担，无需自写。本节规则 ID 仍然有效。

新增检查体系（CI 门禁，有违规即非零退出），规则分三层：

**报错文案硬性要求**（S*/M*/D* 通用）：每条违规输出必须含三要素——
① 违规文件路径（与 seq/规则 ID）；② 违规原因（引用具体声明/账本行）；
③ 下一步动作（确切命令或 YAML 片段；S003 必须附 §3-D2 决策表对应行建议）。

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
| D001 | 声明的 schema 与实库（`sqlite_master` / `information_schema`）存在差异（排除 `_oj_migrations%` 账本表） |
| D002 | 实库存在未被任何模块声明的表（排除 `_oj_migrations%` 账本表） |

### 5.2 各命令承担的检查

```
oj build --check → 全部 S*（+ 可选 D*；只校验不落盘）   # CI 门禁 / 本地快查
oj build     → S002/S003/S004/S005/S006/S007 → fail build（构建即检查，无独立 oj check 子命令）
oj migrate   → M001/M002/M003               → apply + 写账本
oj server    → release 模式 M004；dev 模式自动 apply
oj test      → 走 App::from_config，迁移 + 种子 + fixtures 全跑一遍
oj fixture   → load：按模块灌 fixtures/（dev 手动补数）
oj schema diff → D001/D002
```

CI 建议串法：`oj build && oj test`（build 内含全部 S*，无需前置 check）。

**命令面总览**（防命令面无序膨胀）：

| 命令 | 日常 dev | 发布 | CI | 排障 |
|---|---|---|---|---|
| `oj server` | ✔ | ✔（release 只校验） | | |
| `oj build` | | ✔ | ✔（含全部 S*） | |
| `oj migrate` | 自动（dev） | ✔ 显式 | | 账本修复 |
| `oj test` | | | ✔ | |
| `oj fixture` | ✔（补数） | | | |
| `oj schema diff` | | | 可选 | 漂移排查 |

### 5.3 SQL 表名提取的实现选型

S003 / S006 / 运行时守卫都依赖「从 SQL 字符串里取出表名」。

| 选项 | 做法 | 评价 |
|---|---|---|
| 加 `sqlparser` 依赖 | 真正的 SQL 解析，按方言 parse 后遍历 `TableFactor` | 准，但引入重量级依赖（当前 `Cargo.toml` 无此依赖） |
| 轻量正则 | 抓 `FROM` / `JOIN` / `INTO` / `UPDATE` 后的标识符 | 无新依赖；有误报，需 `/* oj:allow-table=x */` 豁免注释兜底 |

建议：**先用正则 + 豁免注释**落地（`db.query` 里大量模板字符串拼 SQL，
静态解析本就是 best-effort），若误报不可接受再引入 `sqlparser`。

**模块执行上下文（P2 前置，守卫的先决条件）**：当前 `ReqState` 无模块身份字段，
op 层无从知道 handler 属于哪个模块；且 `db.table()` 只查全局白名单不查归属——
order 模块 `db.table("account")` 会直接通过，守卫被查询构造器一行绕过。需新增
`ReqState.module: Option<ModuleCtx>`（module 名、deps 集合、bound_db 名），由
server actor 从 RouteEntry 的 `<module>-<version>/` 前缀在 `run_module` 前注入；
归属检查收敛为单一函数，`op_db_query` / `op_db_exec` / `op_db_query_build`
**三处统一调用**（漏 build 则守卫失效）；`manifest.db` 绑定在派发层按 `bound_db`
重定向——不改 bootstrap.js（`globalThis.db` 每 runtime 绑死、池化 runtime 跨模块
复用，模块级绑定只能每请求解析）。

运行时守卫**默认 `warn`（日志告警不拦截）**，配置项 `ownership_guard: warn | deny`
——现网全是裸 SQL（`sample/src/user/account/api.ts:6` 起），默认 deny 会让升级当天
第一条请求即报错；deny 切换时点绑到 P3 / 下个大版本，§7-3 改造须在 deny 前完成。
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
| 1 | **向后兼容** | 根 `seed.sql` 与模块 `seed.sql` 并存期如何取舍？建议：并存一期、打印 deprecation，下一期移除。并存期执行语义：**根 seed 先于模块 seed**；同名表以模块声明为准，冲突即 S002 fail-fast（不静默合并）；deprecation 文案固定为「root seed.sql 已废弃：请将 <表名> 迁至 <模块>/seed.sql」 |
| 2 | **模块应用顺序** | 有 `deps` 的模块必须后于被依赖模块建表/迁移 → 需拓扑排序，环则 fail-fast |
| 3 | **跨库** | `manifest.db` 若允许模块绑不同库，则「跨模块 JOIN / 视图」在同库才成立，跨库只能走契约调用或读模型。归属图需带库名维度 |
| 4 | **多租户** | `tenant` 注入是请求级的（`config.yaml` 的 `tenant` 段），种子数据是否按租户灌？当前 seed 无租户维度，需在方案里明确（建议：seed 只灌租户无关数据） |
| 5 | **`db.table()` 复活的影响面** | 现在所有业务都是裸 SQL，启用归属守卫后**存量代码可能大面积报错**。需要一个「先 warn 后 deny」的灰度开关 |
| 6 | **破坏性变更检测** | 删列 vs 改名的歧义无法自动判定，必须人工写迁移 —— 需要清晰的报错文案与操作指引 |
| 7 | **`oj test` 成本** | 测试走 `App::from_config`，迁移一多会拖慢测试启动。建议测试场景支持「从 schema.yaml 一次性建库」的快路径 |
| 8 | **`_platform` 与框架内置表的冲突** | `auth.user_table: users` 目前从 config 读；若 `users` 归 `_platform`，需明确 config 与 `_platform` 谁为准 |

---

## 9. 附录（2026-08-28 追加）：sqlx 迁移机制调研与 P1 执行面建议

> 针对「能否借助 sqlx 生态做迁移」的专项调研结论。不影响第 3 章决策点，只修订
> 4.4 账本细节与 P1 的实现路径。引用的 sqlx 均为本仓已依赖的 0.9.0
> （根 `Cargo.toml:30-36`，**未开 `migrate` feature**）。

### 9.1 sqlx 0.9 迁移实现拆解

- **入口**：`Migrator::new(Path)`（运行时目录）/ `with_migrations(Vec<Migration>)`
  （程序化）/ `migrate!()`（编译期宏）。文件格式 `<VERSION>_<DESC>.sql`
  （version 为 `i64 > 0`）；文件头 `-- no-transaction` 标注 → `no_tx = true`；
  可选 `.down.sql` 配 `undo()`（官方标注 experimental）。文件名按
  `splitn(2, '_')` 切分并要求后半段以 `.sql` 结尾，描述里 `_` → 空格；
  `no_tx` 判定是 `sql.starts_with("-- no-transaction")`（必须位于文件首字节）。
- **账本 `_sqlx_migrations`**：`version BIGINT PRIMARY KEY, description TEXT
  NOT NULL, installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
  (pg: TIMESTAMPTZ DEFAULT now()), success BOOLEAN NOT NULL, checksum
  BYTEA/BLOB NOT NULL, execution_time BIGINT NOT NULL` —— **单流模型**
  （version 单列主键，无归属维度）。表名可由 `dangerous_set_table_name()`
  改写（即 issue #1698 已实现，非未决）。
- **执行算法（`run_direct`）**：lock → create_schemas →
  ensure_migrations_table → 查 dirty（`success=false` 即 `MigrateError::Dirty`
  拒跑）→ list_applied → validate checksum（篡改报 `VersionMismatch`）→
  逐条「事务内执行 SQL + 写账本」→ unlock。逃生门 `conn.skip()`：账本插一行
  `success=TRUE, execution_time=-1` 标记已跳过而不执行。
- **锁**：`Migrator.locking` 默认 `true`，`set_locking(false)` 可关（官方用于
  CockroachDB 这类不支持 advisory lock 的）。lock id **由数据库名派生**（Rails
  启发：`0x3d32ad9e * CRC32(db_name)`，pg 传 i64、mysql 传 hex 字符串），
  使同实例上的不同库可并发迁移。
  - postgres：`SELECT pg_advisory_lock($1)` / `pg_advisory_unlock($1)`
  - mysql：`SELECT GET_LOCK(?, -1)` / `RELEASE_LOCK(?)`（**-1 = 无限等待**）
  - sqlite：`lock()`/`unlock()` 是 **no-op**（`sqlx-sqlite/src/migrate.rs:149`），
    原子性完全靠 `apply()` 里的 `self.begin()` 普通事务（非 `BEGIN IMMEDIATE`）
- **值得抄的五件协议**：① checksum 内容校验（sha384，且支持 `ignore_chars`
  剔除 BOM/CRLF 等噪声字符）；② `success` 脏标记 + dirty 拒绝自动续跑；
  ③ 方言级 advisory lock + **按库名派生 lock id**；④ per-migration
  `-- no-transaction` 标注（mysql DDL 隐式提交专用）；⑤ `skip` 逃生门。

### 9.2 为什么不能直接用 `Migrator`（本项目硬约束）

根因是 **dylib 边界两侧的 sqlx 是同名不同型**：

- oj 二进制只链 `any + sqlite` → 宿主 `AnyPool` 只能装本进程编译进去的驱动，
  **实际只能跑 sqlite**；
- mysql/pg 连接由 `oj-db-mysql` / `oj-db-postgres` 在插件内建立（各带完整
  sqlx），`AnyPool`/`AnyConnection` 跨 dylib 类型身份不共享，插件连接**喂不进
  宿主 `Migrator`**；
- FFI 契约（`ABI_VERSION = 5`，`oj-plugin-ffi/src/lib.rs:34`）只暴露
  `DataAccessor` vtable，没有「跑 Migrator」槽位。

用原生 `Migrator` 跑三方言 ⇒ 必须 ABI 5→6 + 全部 7 插件重建 + 迁移元数据跨
FFI 通道。为省 ~200 行代码动 ABI，不值。

### 9.3 生态备选排除表

| 方案 | 排除理由 |
|---|---|
| refinery | ⚠️ **排除理由部分不成立，见附录 10**。账本名是 `migrate()` 的逐次调用参数（天然支持模块级独立账本）；且 `AsyncMigrate` 全是 provided methods，只需实现 2 个只吃 SQL 字符串的方法即可跨 dylib 边界 |
| sea-orm-migration | 绑定 sea-orm 连接与实体模型，更重；同样的 dylib 问题 |
| barrel | 已停止维护 |
| sqlx-cli / dbmate / goose / atlas / golang-migrate | 外部二进制破坏单二进制分发；单流模型不懂模块归属 |
| sea-schema | typed probe 需要具体驱动连接类型，与 `Any` / `DataAccessor` 边界不兼容 |

共性结论（**已被 §10 修正，以此为准**）：sqlx / sea-orm / diesel 确实都要拿原生
连接，跨不过 `DataAccessor` vtable 边界；**但 refinery 是例外** —— 它的契约只要求
「能执行 SQL 字符串」，可由适配器满足，无需原生连接。详见 §10。

### 9.4 推荐执行面：手写引擎 + 抄 sqlx 协议（零 ABI 变更）

**关键事实（本次调研确认）**：`DataAccessor::begin() -> TxSession` 事务面已
**全线贯通**——trait 定义 `src/bridge/db.rs:64`；宿主 sqlite 实现
`src/bridge/accessor_sqlx.rs:183`（`Pool::begin` → `Transaction<'static, Any>`）；
**跨 FFI 也通**：`src/bridge/ffi.rs:307-315` `FfiDataAccessor::begin` 经插件
vtable 取 `tx_id` 包装 `FfiTxSession`（query/exec/commit/rollback，Drop 兜底
rollback）。即宿主侧可拿真实事务，`BEGIN → DDL → 写账本 → COMMIT` 在同一连接
成立，不存在「池化 exec 换连接导致无法原子」的问题。

引擎形态（建议新文件 `oj/src/migrate.rs`，流程对齐 `run_direct`）：

```
输入：模块列表（dev: src/<m>/migrations/*.sql；release: dist/<m>-<v>/migrations/*.sql）
1. 取锁（方言 SQL，见下）
2. CREATE TABLE IF NOT EXISTS _oj_migrations（4.4 结构 + success 列）
3. 查 dirty 行 → 拒跑，提示人工修复或 --skip <module:seq>
4. list_applied → validate checksum（篡改即 M001）
5. 逐条：accessor.begin() → TxSession 内执行 SQL + 写账本 → commit
   （文件头 -- no-transaction 则 exec 后单独写账本，mysql DDL 场景）
6. 解锁
```

锁（手写三行，不依赖 sqlx 内部，按 `Dialect` 分派，`src/bridge/db.rs:26`）：

- postgres：`SELECT pg_advisory_lock(<id>)` / `SELECT pg_advisory_unlock(<id>)`
- mysql：`SELECT GET_LOCK(<name>, -1)` / `SELECT RELEASE_LOCK(<name>)`
  （`-1` = 无限等待，对齐 sqlx：超时返回 0 会被误判成拿到锁，不如等）
- sqlite：不取锁（对齐 sqlx no-op），靠 `max_connections(1)`
  （`accessor_sqlx.rs:32-34`）+ 账本写入在 `begin()` 事务内，天然串行

`<id>` / `<name>`：固定常量（如 `oj_migrate`）即可。**偏离 sqlx 之处**：sqlx
按库名 CRC32 派生，使同实例多库可并发迁移；我们用常量则同实例多库串行。
单库单部署是本项目常态，串行只是迁移稍慢，不引入 crc 依赖更划算——若将来出现
同实例多库场景，再改成库名派生。

明确放弃：**选项 A**（ABI 6 + 插件内跑 Migrator，等真实需求再议）；sqlx
`migrate` feature 不开启；`undo`/down 迁移不做（只向前）。

> **已被 §11.2/§4.6 修订**：本节的执行循环方案让位于 refinery 适配器（D4，§10 论证）；
> 「关键事实」（`begin()` 事务面贯通）仍成立；**但三方言锁设计已废弃**——
> pg/mysql advisory lock 是**连接作用域**，按本节「手写三行、经池 exec 取锁」
> 会跨连接失效（池 idle_timeout 回收持锁连接即静默丢锁），修正案见 §4.6/§11.2。

### 9.5 对 4.4 账本设计的修订

| 项 | 原案 | 修订 | 理由 |
|---|---|---|---|
| 账本结构 | `(module, seq, name, checksum, dialect, applied_at)` | **+ `success BOOLEAN NOT NULL DEFAULT 1`** | 抄 sqlx dirty 协议：失败留痕、拒绝自动续跑，比「失败后状态未知」可诊断 |
| checksum | sha256 | **维持 sha256，但先规范化** | 算法不对齐（理由同上）；但要抄 sqlx 的 `ignore_chars` 思路：计算前剔除 BOM、`\r\n`→`\n`，否则同一份 SQL 在 Windows 检出后被误判篡改 |
| 文件命名 | `NNNN__desc.sql` | **维持原案** | 同上，单/双下划线不产生真实收益，少一个变更点 |
| 原子性 | 「pg/sqlite 同事务；mysql DDL 隐式提交需文档标注」 | **维持，补实现细节** | `-- no-transaction` 文件头标注（抄 sqlx 语法）；mysql 下失败即 dirty 行 + 人工提示 |
| 执行面 | 未指明 | **`DataAccessor::begin()`** | 见 9.4，事务钉死单连接 |

### 9.6 对 P1 的落地更新

| 项 | 内容 |
|---|---|
| 新增 | `oj/src/migrate.rs`（引擎 ~200 行）+ `sha2 = "0.10"` + `oj migrate` 子命令 |
| 执行面 | `DataAccessor`（三方言同路径，事务经 `begin()`，零 ABI 变更、零插件重建） |
| 账本 | `_oj_migrations` + `success` 列；dirty 拒跑 + `--skip` 逃生门 |
| 锁 | 三方言三行 SQL（见 9.4） |
| 交互 | 维持 D3=a：dev 启动自动 apply；release 显式 `oj migrate` + server 只校验 |
| 不做 | sqlx `migrate` feature；down 迁移；方言覆盖文件 `0007.pg.sql` 维持 4.7 原案 |

P0 不受影响（`.sql` 进 dist 的白名单扩展与迁移引擎无关），可先行。

### 9.7 调研来源

- sqlx::migrate 模块与 Migrator 文档：docs.rs/sqlx（0.9.0）
- v0.9.0 源码（raw.githubusercontent.com/launchbadge/sqlx）：
  `sqlx-core/src/migrate/{migrator,source,migration}.rs`、
  `sqlx-{postgres,mysql,sqlite}/src/migrate.rs`（账本 DDL / lock / apply 三方
  言实现，本节 9.1 结论均逐条对照过源码）
- sqlx issue #1966（「执行脚本 + 写账本必须同一事务，否则重复执行」，apply 内
  的注释即引自此），#1698（账本表名可配置 → 已由 `dangerous_set_table_name()`
  实现）

---

## 10. 附录（2026-08-28）：对 §9 的核验与修正 —— refinery 可行性

> §9.3 以「单全局流账本 + 需要原生连接」为由排除了 refinery。逐条核验其 trait
> 定义后，**这两条理由均不成立**。本节给出证据、修正后的取舍，以及随之而来的
> 新风险。§9 其余结论（不能用 sqlx `Migrator`、`DataAccessor::begin()` 事务面贯通、
> 三方言锁）核验无误，继续成立。

### 10.1 核验证据

**（1）`AsyncMigrate` 没有自己的 required method —— 不需要原生连接。**

docs.rs/refinery-core/0.9.2 的 `AsyncMigrate` 定义中，方法区标题是
**Provided methods**（无 Required methods 段）—— `migrate()` /
`get_applied_migrations()` / `get_last_applied_migration()` 全部有默认实现。
它唯一的约束是 `AsyncQuery<Vec<Migration>>`：

```rust
pub trait AsyncQuery<T>: AsyncTransaction {
    fn query(&mut self, query: &str) -> /* Future<Output = Result<T, Self::Error>> */;
}
pub trait AsyncTransaction {
    type Error: std::error::Error + Send + Sync + 'static;
    fn execute<'a, T>(&mut self, queries: T) -> /* Future<Output = Result<usize, Self::Error>> */
    where T: Iterator<Item = &'a str> + Send;
}
```

两个方法的入参**只有 SQL 字符串**，不含任何 sqlx / 驱动类型。所以只要给
`Arc<dyn DataAccessor>` 包一层、实现这 2 个方法，整个 refinery 迁移引擎即可用。
`DataAccessor: Send + Sync`（`src/bridge/db.rs:57`）与 `TxSession: Send`（`:47`）
也已满足 `#[async_trait]` 要求的 `Self: Send`。

**（2）账本名是 `migrate()` 的逐次调用参数 —— 天然支持模块级独立账本。**

```rust
fn migrate(&mut self, migrations: &[Migration], abort_divergent: bool, abort_missing: bool,
           grouped: bool, target: Target, migration_table_name: &str) -> Result<Report, Error>
```

对 N 个模块循环调用 N 次、`migration_table_name` 传 `_oj_migrations_<module>`，
即得**每模块一张账本、version 各自从 1 起** —— 正是 §4.3 要的模型，无需改造。

**（3）依赖不重，且异步 trait 不受 feature 门控。**

crates.io 元数据（refinery-core 0.9.2）：`default = []`，**零数据库驱动**；
非可选依赖仅 `async-trait / cfg-if / log / regex / siphasher / thiserror / time /
url / walkdir`；features 列表全是驱动与 tls/config 相关，**没有任何 feature 门控
`traits::async`**。

### 10.2 修正后的取舍

| 维度 | §9.4 手写引擎 | refinery + ~60 行适配器 |
|---|---|---|
| 新增依赖 | 0（+ `sha2` 一行） | `refinery-core = "0.9", default-features = false`（9 个轻量 crate，无驱动） |
| 自写代码量 | ~200 行（执行循环 + 校验 + 排序） | ~60 行（2 个 trait impl + 错误 newtype） |
| checksum 篡改校验（M001） | 自己写 | `abort_divergent` 白送 |
| 账本有而源码无（M002） | 自己写 | `abort_missing` 白送 |
| 排序 / target / grouped 事务 | 自己写 | 白送 |
| 模块级独立账本 | 自己设计 | 白送（10.1.2） |
| 并发锁 | 手写三行 | **同样要手写**（refinery 无锁） |
| `success` 脏标记列 | 有 | **没有**（见 10.3） |
| SQL 拼接风险 | 无（全程绑定参数） | **有**（见 10.3） |

### 10.3 refinery 方案的三个新风险（须并入 §8 风险表）

1. **失去 `success` 脏标记**。refinery 账本只有
   `version / name / applied_on / checksum` 四列；`insert_migration_query` 不是
   trait 的 provided method、**无法覆写**，故加不进 `success` 列。
   - sqlite / postgres：DDL 事务性，单条失败整体回滚 → **不产生脏状态，该列本就无意义**；
   - mysql：DDL 隐式提交，可能留下部分变更而账本无记录 → 重跑该迁移时，若其不可重入
     （如 `ALTER TABLE ADD COLUMN` 已部分生效）会得到 `duplicate column` 这类费解报错。
   - 兜底：`Migration` 在 0.9.2 无公开的 `set_no_transaction`，所有迁移一律走
     `AsyncTransaction::execute` —— **正好把方言决策集中进我们自己的适配器**：
     mysql 分支不 `BEGIN`、直接顺序 exec；并约定「迁移须可重入」+ 报错文案指引。
     这也替代了 §9.5 的 `-- no-transaction` 文件头方案。
2. **`AsyncQuery::query(&str)` 无绑定参数，refinery 内部以字符串拼接生成账本 SQL**
   （表名插值、`insert_migration_query` 值拼接）。这与 `CLAUDE.md` 的注入红线
   （动态标识符只来自白名单）存在张力。
   - 缓解：插值值只有两处 —— 账本表名来自模块名、迁移名来自模块目录下的文件名，
     **均非 JS 可控输入**；但仍须在适配器入口显式过 `validate_module`
     （`oj/src/manifest.rs:16-25`，`[A-Za-z0-9_-]`）与迁移名白名单，并在文档中
     把这条标注为受控边界。这是选此方案**必须接受并在评审中明示**的代价。
3. **错误类型需 newtype 包装**。`type Error` 要求
   `std::error::Error + Send + Sync + 'static`，而项目错误类型是
   `Box<dyn Error + Send + Sync>`（`src/bridge/mod.rs:77`），它本身不实现 `Error`
   （` impl Error for Box<dyn Error>` 不成立）。需 ~10 行 newtype。

### 10.4 对文件命名无影响

`Migration::unapplied(input_name, sql)` 要求 `input_name` 形如 `V1__desc`
（U/V 前缀 + 双下划线）。磁盘上仍用 §4.7 的 `0001__init.sql`，构造时映射为
`format!("V{seq}__{desc}")` 传入即可，**文件命名约定不必改**。

### 10.5 结论与建议

- **§9.3 中 refinery 一行的排除理由应撤回**，改为「可行，但需承担 10.3 三项风险」。
- 若项目存在**「禁止新增依赖」的硬约束**（`CLAUDE.md` 未载，需向维护者确认），
  则 §9.4 手写引擎的结论原样成立。
- 若无该约束，**建议改走 refinery 方案**：净收益是省掉 ~140 行自写代码，并把
  M001/M002 两类关键校验交给久经使用的实现；代价主要是 mysql 脏标记缺失，
  而它对占 2/3 的事务性 DDL 方言本就无意义。
- 无论选哪条，**锁、seed/fixtures 语义、D3 执行时机**（§4.5 / §4.6 / §9.6）
  均不受影响；P0 同样不受影响。

### 10.6 核验来源

- `AsyncMigrate` / `AsyncQuery` / `AsyncTransaction` / `Migration`：
  docs.rs/refinery-core/0.9.2（逐页读取确认，非二手转述）
- 依赖与 features：crates.io/api/v1/crates/refinery-core/0.9.2（含 /dependencies）

---

## 11. 结论：最终方案汇总

> 综合第 2 章现状、第 3 章决策点、第 9/10 章生态调研。**本章是唯一的行动依据**；
> 与前文冲突之处以本章为准。

### 11.1 决策点的结论

| 决策 | 结论 | 依据 |
|---|---|---|
| D1 表结构声明 | **C 声明式 `schema.yaml`**；P0/P1 可先用 B 的 `migrations/` 先行 | §3-D1 / G3 |
| D2 跨模块取数 | **四层全收**：默认契约调用，列表聚合走读模型，同库确需 JOIN 走「声明依赖 + 只读视图」 | §3-D2 |
| D3 迁移执行时机 | **a 显式 `oj migrate` + server 只校验** | §3-D3 |
| **D4 迁移执行面**（新增） | **`refinery-core` 适配器**；若「禁止新增依赖」成立则回落 §9.4 手写引擎 | §10 |

D4 是本轮调研新增的决策点，也是原问题「借助生态增强迁移能力」的落点：
**不是抄 sqlx 的协议自己造，而是直接用一个跨得过 FFI 边界的生态 crate。**

### 11.2 迁移执行面（最终形态）

**装配契约**：`oj migrate` **不走** `App::from_config`——其携带不可关闭的证书强制
门禁（`oj/src/app.rs:81-86`，注释明言「无逃生口」）、seed 重放与路由构建，
CI/运维机无证书文件即被挡死。走瘦身装配：`assemble_plugins` + `connect_dbs` +
迁移引擎（无证书、无 seed、无路由）；server 启动校验复用 from_config 已有
dbs 句柄，无此问题。

新文件 `oj/src/migrate.rs`：

```rust
/// 模块白名单已过 validate_module（oj/src/manifest.rs:16-25），
/// 账本表名由此派生 → 进入拼接前必须是可信输入（§10.3-2）。
struct OjConn { acc: Arc<dyn DataAccessor>, module: String }

impl AsyncTransaction for OjConn {
    type Error = MigrateErr;                       // ~10 行 newtype（§10.3-3）
    // #[async_trait] 展开要求泛型 + 生命周期（`impl Iterator` 形式与 trait 定义不匹配）：
    async fn execute<'a, T>(&mut self, queries: T) -> Result<usize, MigrateErr>
    where T: Iterator<Item = &'a str> + Send {
        match self.acc.dialect() {
            // 事务性 DDL：BEGIN → 全部语句（含账本写入）→ COMMIT
            Dialect::Sqlite | Dialect::Postgres => {
                // ① 先按 ';' 拆分语句：refinery 把整个迁移文件作为一条字符串传入，
                //    而 TxSession::exec 是单条预编译语句，多语句串不可执行
                //    （继承 §2.1 已文档化的「语句内不得含分号字面量」约束）；
                // ② pg 首条语句插 SELECT pg_advisory_xact_lock(<id>)（§4.6 锁）；
                // ③ accessor.begin() → TxSession 顺序 exec → commit。
                //    execute 内聚事务、外不持连接（见下方注）
            }
            // mysql DDL 隐式提交：不 BEGIN，逐条 exec（§10.3-1）；
            // 互斥靠账本 version 主键冲突兜底（§4.6）
            Dialect::MySql => { /* 拆分后顺序 exec */ }
        }
    }
}

impl AsyncQuery<Vec<Migration>> for OjConn {
    async fn query(&mut self, sql: &str) -> Result<Vec<Migration>, MigrateErr> { /* 行 → Migration */ }
}
// AsyncMigrate 全是 provided methods → migrate() 直接可用，无需实现
```

> 三个「照抄即错」的细节（研发评审补充，已核实）：
> ① **execute 内聚事务、外不持连接**——`migrate()` 开头的账本建表 assert 同样走
>    `execute`（`traits/async.rs:177-181`），若适配器跨调用持有 `TxSession`，
>    sqlite `max_connections(1)` 下随后的 `get_applied_migrations`（走池）会自阻塞
>    至 acquire 超时；
> ② refinery 默认账本建表 DDL 用 `int4` 版本列（MySQL 8 无此类型）——
>    `assert_migrations_table_query` 是 provided method **可覆写**
>    （`traits/async.rs:132-134`），适配器按 Dialect 产出 `BIGINT` 版本列 DDL
>    （默认 DDL 兼容性纳入 Q7 spike 验证）；
> ③ `grouped=false` 选择正确：grouped 会把全部迁移并成单事务，mysql DDL
>    隐式提交下必裂。
>
> 估算修正：含多语句拆分、锁注入与行转换，适配器 **~100–120 行**（非 §10.2 的 ~60 行）。

调用侧（逐模块一张账本、version 各自从 1 起）：

```rust
for m in &modules {                                   // 拓扑序：被依赖模块先行（§8-2）
    let migs = load_migrations(&format!("{dir}/{m}/migrations"))?   // 0001__init.sql
        .map(|(seq, desc, sql)| {
            let sql = normalize(&sql);                // 去 BOM、CRLF→LF（§9.5 关注点）
            Migration::unapplied(&format!("V{seq}__{desc}"), &sql)  // §10.4 命名映射
        })
        .collect::<Result<Vec<_>, _>>()?;
    conn.migrate(&migs, /*abort_divergent=*/true, /*abort_missing=*/true,
                 /*grouped=*/false, Target::Latest,
                 &format!("_oj_migrations_{m}")).await?;
}
```

外层仍按 §9.4 自己加三方言锁 —— **refinery 不带锁，这点没有变化**。

### 11.3 账本最终形态

每模块一张：`_oj_migrations_<module>(version, name, applied_on, checksum)`

- `version` 模块内从 1 起单调递增 → 满足 §4.3「module version 与 schema seq 分离」
- `checksum` 由 refinery 计算（SipHash 族 u64；**精确参数序未对源码核实**——但不影响
  正确性：对比双方「账本存量 vs 当前文件重算」都出自同一 refinery 版本，Q7 spike
  顺手确认）。账本存储形态：checksum 为 **u64 十进制字符串（VARCHAR）**、applied_on
  为 **RFC3339 字符串**——可无损穿过行 JSON 边界（`column_json`），行 → `Migration`
  转换仅需 `i64` / 三个 `String` + `time` 解析
  （`Migration::applied(version, name, applied_on, checksum)` 为 public 构造器）；
  宿主需显式加 `time = "0.3"` 直接依赖（已在依赖树 `Cargo.lock` time 0.3.55，零新增 crate）
- **BOM / CRLF 规范化在读入侧由我们做**（§9.5 的关注点），规范化后的文本才交给
  `unapplied()` → Windows 检出不会被误判篡改
- **无 `success` 列**（§10.3-1）：sqlite/pg 事务性 DDL 自动回滚，不受影响；
  mysql 用「迁移须可重入 + 报错文案指引人工核对」兜底，替代 §9.5 的
  `-- no-transaction` 文件头方案（方言决策已集中进适配器的 `execute`）

### 11.4 检查规则最终映射

| 规则 | 承担者 |
|---|---|
| M001 checksum 篡改 | refinery `abort_divergent` |
| M002 账本超前于模块 | refinery `abort_missing`（第二触发条件：文件系统 seq ≤ 账本 max 但未应用） |
| M003 `schema_head` < 账本 max(version) | 自写（读 `dist/manifests.yaml`，见 4.2） |
| M004 verify 模式存在待应用 | 自写（dry-run 计数；首次部署语义见 §4.6） |
| S001–S007（结构层） | 自写，与迁移引擎无关；S007 的「同 seq 重复」由 refinery `RepeatedVersion` 覆盖，自写仅剩**空洞/乱序**检测 |
| D001–D002（漂移层） | 自写（排除 `_oj_migrations%` 账本表，见 §5.1） |
| 并发锁 | pg 事务级 `pg_advisory_xact_lock` / mysql 账本主键冲突兜底（§4.6；refinery 无锁） |

### 11.5 落地顺序（最终）

| 阶段 | 内容 | 新增依赖 |
|---|---|---|
| **P0** | 扩 `oj/src/build_cmd.rs:268` 收集白名单（`.sql` / `schema.yaml`；**显式排除 `fixtures/`**）；启动时按模块顺序重放 `schema.sql` + `seed.sql`。语义等同今日根 `seed.sql`，仅拆到模块，**不引入迁移概念**，可独立交付。并存期顺序：根 seed → 模块 seed；同名表冲突报 S002（§8-1） | 无 |
| **P1** | `oj/src/migrate.rs` 适配器（~100–120 行，§11.2）+ `oj migrate` 子命令（**瘦身装配**，不走 from_config）+ `--baseline`（P0 建过表的存量库，语义见 Q5）+ `oj fixture` 子命令 + `fixtures/` 分离（`from_config` 增 `fixtures: bool` 形参，e2e 调用点适配）+ build 侧三项（方言文件按 Dialect 过滤去重；`collect_module` 排除 fixtures；`schema_head` 写入 `dist/manifests.yaml`）+ sample/README quick start 更新为 `oj build && oj migrate && oj server` | `refinery-core = { version = "0.9", default-features = false }` + `time = "0.3"`（已在树） |
| **P2** | `schema.yaml` 声明式（sea-query 底座，§4.7）+ 三方言 DDL 生成 + 表归属图 + 填 `SchemaRegistry`（**两处装配点**，§4.8）+ `ReqState.module` 执行上下文 + 运行时归属守卫（**默认 warn**，§5.3） | 视 D1 定 |
| **P3** | 检查体系收口（build 内含全部 S*，D* 随 `build --check`）+ `oj schema diff` + `ownership_guard: deny` 切换评估 + 三册文档更新 | 无 |

P0 与 D1/D4 决策无关，可立即开工。启动顺序（P1 后）：迁移 → 模块 seed → 根 seed（deprecated）。

### 11.6 决策请求（每条含推荐/备选/默认条款；Q1/Q2 阻塞 P1/P2）

| # | 问题 | 推荐 | 备选 | 未决时默认 |
|---|---|---|---|---|
| Q1 | 是否允许新增 `refinery-core` 依赖（`CLAUDE.md` 未载依赖政策） | **允许**（D4 = refinery） | §9.4 手写引擎 | 允许 |
| Q2 | D1 是否采纳 C（声明式 `schema.yaml`） | **采纳** | B 迁移式 | 采纳 |
| Q3 | 根 `seed.sql` 废弃节奏 | 并存一个大版本（根 seed 先执行，同名表冲突报 S002，§8-1） | 直接切 | 并存一期 |
| Q4 | 多租户 seed 是否带租户维度 | 不带（seed 只灌租户无关数据） | 按租户灌 | 不带 |
| Q5 | P0 存量库的 baseline 语义 | `oj migrate --baseline`：将 ≤head 的 seq 记为已应用而不执行 | 首迁移强制空库 | `--baseline` |
| Q6 | `manifest.db` 多库时账本落点 | 账本随各模块 bound_db 同库（表名含模块名，库间天然隔离） | 集中 default 库 | 随 bound_db |
| Q7 | D4 拍板前 spike | 30 分钟 scratch crate：`cargo add refinery-core` + 实现 2 trait 编译通过；核实 `Migration::unapplied` 签名、默认账本建表 DDL 的 MySQL 兼容性、checksum 参数序（§11.2-②、§11.3） | — | 必做后拍板 |

---

## 12. 附录（2026-08-29）：三方评审记录与修订

> 架构 / 产品 / 研发三视角独立评审（评审者对照代码逐条核实后裁决）。共 32 条意见：
> 采纳 28、驳回 2、部分采纳 2。本节只记条目与处置，论述见正文各节修订处。

### 12.1 已采纳并修订正文

| 来源 | 条目 | 落点 |
|---|---|---|
| 架构+研发（双确认） | pg/mysql advisory lock 为连接作用域，池化取锁跨连接失效、idle_timeout 回收即静默丢锁 | §4.6 / §9.4 / §11.2（pg 改事务级 xact_lock；mysql 不做连接锁，账本 PK 兜底） |
| 研发 | refinery 整文件单串传入 execute，须按 `;` 拆分（exec 是单条预编译语句） | §11.2（继承 §2.1 分号约束） |
| 架构 | `oj migrate` 不可走 from_config（证书门禁无逃生口，`app.rs:81-86`） | §11.2 装配契约（瘦身装配） |
| 架构+研发（双确认） | 第二处空 `SchemaRegistry::new()`（`app.rs:353`，oj test）——只改一处即状态分裂 | §4.8（两处一并替换，from_config 构造一次共享） |
| 架构+研发+产品（三确认） | 根级 `_platform/` 进不了 build/发现链路 | §4.1（移入 `src/_platform/`，普通模块零特例） |
| 架构 | 归属守卫缺模块执行上下文；`db.table()` 不查归属、可一行绕过 | §5.3（`ReqState.module` + 三 op 统一守卫） |
| 产品 | 跨表守卫无灰度节奏，升级当天现网即坏 | §5.3（默认 warn，`ownership_guard: warn\|deny`） |
| 产品 | 空库首启 M004 拒启无引导，README 两行故事被打破 | §4.6 首次部署段 + P1 README 更新 |
| 架构 | P0 存量库在 P1 首迁移必炸 `table already exists` | §11.5-P1 `--baseline` + Q5 |
| 架构+产品（双确认） | `oj.call` 复用 dispatch 前提不成立（reset 回滚外层事务、生产未装载） | §3-D2-1（backlog，须 nested-safe op） |
| 产品 | 四层菜单给了选项没给「怎么选」 | §3-D2 场景决策表（S003 报错须附对应行） |
| 产品+研发 | `oj test` 是否灌 fixtures 两处矛盾（cert.test.ts `trinity` 实证） | §4.5（test 自动灌）+ `from_config` 形参（P1） |
| 研发 | 方言覆盖文件同 seq 并存会报 `RepeatedVersion` | §4.7（按 Dialect 过滤取一，desc 映射一致） |
| 产品+研发 | `schema_head` 回写 manifest.yaml 造成生成物混手写、合并冲突 | §4.2（移入 `dist/manifests.yaml`） |
| 架构 | 「放宽类型」自动推导与 sqlite 无 `ALTER COLUMN` 冲突 | §3-D1-C（归入手写迁移）+ 改名闭环 |
| 架构 | DDL 生成器未提已有依赖 sea-query | §4.7（Table DSL + 三方言 QueryBuilder 即底座） |
| 架构 | D3=a 使降版部署被禁但全文未言明 | §3-D3 运维约束段 |
| 架构+研发（双确认） | D001/D002 会把账本表误报漂移 | §5.1（排除 `_oj_migrations%`） |
| 研发 | `oj fixture load` 反复被引用却无落地阶段 | §11.5-P1 `oj fixture` 子命令 |
| 产品 | `oj check` 与 build 检查集几乎重合、命令面 3→7 无总览 | §5.2（收敛为 `oj build --check` + 命令面总览表） |
| 研发 | execute 跨调用持会话在 sqlite `max_connections(1)` 下自阻塞；`impl Iterator` 签名与 `#[async_trait]` 不符 | §11.2 细节注 ① + 伪码签名修正 |
| 研发 | refinery 默认账本 DDL `int4` 与 MySQL 8 不兼容（覆写口存在） | §11.2 细节注 ②（入 Q7 spike） |
| 研发 | checksum 为 VARCHAR 十进制串（行转换可行）；`SipHash13(name,version,sql)` 公式未核实 | §11.3（形态更正 + 公式标注 + 闭环论证） |
| 研发 | 行号漂移 ×2（`build_cmd.rs:214`、`:91-93`） | §2.1 / §4.3 已改 |
| 产品 | 破坏性变更只 fail-fast、缺「写完迁移后」闭环 | §3-D1-C 三步闭环 + §5.1 报错三要素硬性要求 |
| 产品 | 全文无 schema.yaml 示例，D1-C 成本不可评估 | §4.2.1 |
| 产品 | 页眉「3 个决策点」与 §11 新增 D4 矛盾；§11.6 自相矛盾（D1 已答又问） | 页眉改 D1–D4；§11.6 改决策请求格式（Q1–Q7） |
| 产品+研发 | 根 seed 并存期执行语义未定义（双重建表风险） | §8-1（根先模块后、冲突 S002） |

### 12.2 驳回（附理由）

| 来源 | 条目 | 理由 |
|---|---|---|
| 架构 | 「§10 refinery 断言离线不可复核、拍板依据建在流沙上」 | `traits/async.rs:26-64` / `error.rs` / `runner.rs` 已于前轮从 GitHub raw 源码逐行核实（§10.6 有载），「流沙」指控不成立；但其 spike 建议（编译级验证）有独立价值，采纳为 Q7 |
| 研发 | 「适配器需确认 dialect() 是否为 trait 方法，若非需补一行」 | `DataAccessor::dialect()` 是 trait 方法（`src/bridge/db.rs:59`，默认 Sqlite），§11.2 伪码成立，无需改动 |

### 12.3 部分采纳

| 来源 | 条目 | 处置 |
|---|---|---|
| 架构 | 锁必须钉死单一 TxSession 贯穿迁移全程 | 与研发方案合并：pg 用事务级锁（每迁移事务首条语句重取为免费重入，无需跨调用持会话，与「execute 外不持连接」自洽）；「贯穿单会话」方案弃用——它要求 grouped 单事务，与 grouped=false 的正确选择冲突 |
| 研发 | 适配器 ~60 行估算 | 修为 ~100–120 行（含多语句拆分、锁注入、行转换），§11.2 |
