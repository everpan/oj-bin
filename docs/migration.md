# 数据初始化与迁移手册（Data Init & Migration Runbook）

面向**运维与开发者**的数据层专题手册：四条数据通道（`schema.yaml` / `migrations/` /
`seed.sql` / `fixtures/`）各自的适用场景、限制与红线、错误码速查、标准部署流程。
规则背景见 `user-manual.md` §5，部署排障见 `ops-manual.md` §7，实现走读见
`docs/modules/07-data-layer.md`。

## 1. 总览：四条数据通道

| 通道 | 管什么 | 何时生效 | 方言 | 幂等保证 | 进 dist 产物 | 静态守卫 |
|---|---|---|---|---|---|---|
| `schema.yaml` | 声明式结构：表 / 列 / 索引 | 启动（auto）/ `oj migrate` 末步 reconcile | 三方言 | reconcile 本身幂等 | 是（`schema.yaml` 原样拷贝） | S005 |
| `migrations/*.sql` | 手写结构 / 数据演进 | 启动（auto）/ `oj migrate` | 三方言（方言覆盖文件） | 账本账（重跑零新增） | 是 | S007 + 账本 M001/M002 |
| `seed.sql` | 引导数据（每次启动重放） | **每次启动**（服务与测试） | 三方言 | S006 门禁 + 引擎按方言改写 `INSERT OR IGNORE` | 是 | S002 / S006 |
| `fixtures/*.sql` | 演示 / 测试数据（按需灌） | `oj fixture` / `oj test`（**server 永不灌**） | 三方言 | SQL 自身（无门禁） | **否（构建整目录排除）** | 无 |

选型决策：

```text
结构变化？
├─ 只是 加表 / 加可空列 / 加索引      → 改 schema.yaml（自动收敛，零 SQL）
└─ NOT NULL 列 / 改名 / 删列 / 类型 / 回填 → 手写 migrations/
数据？
├─ 每次启动都必须在（角色、菜单、字典）→ seed.sql（幂等 INSERT）
└─ 只在测试 / 演示环境要              → fixtures/
```

两条铁律贯穿所有通道：

1. **动态标识符只来自声明，值只走绑定参数**——迁移与 seed 里的表名列名是作者自写的
   静态文本，不得从任何运行时输入拼装。
2. **`;` 朴素切分**：所有 SQL 文件（迁移 / seed / fixtures）按 `;` 拆句，
   **语句内不得含分号字面量**（含在字符串里也不行）。

## 2. 初始化通道

### 2.1 seed.sql —— 引导数据，随启动重放

- **位置**：`src/<module>/seed.sql`（模块自治，无根级 seed）。
- **时机与顺序**：迁移门禁（auto apply / verify）**之后**，按模块目录名排序逐模块
  重放；三方言 default 库都执行（无 default 库 → `warn: seed skipped` 跳过）。
- **适用**：每次启动都必须存在的引导数据（内置角色、菜单树、字典表）。
- **幂等写法以 sqlite 惯用法为源**：S006 只认 `OR IGNORE` / `OR REPLACE` /
  `ON CONFLICT` / `ON DUPLICATE KEY` 四形态；引擎把默认写法
  `INSERT OR IGNORE INTO …` 按目标方言自动改写——mysql → `INSERT IGNORE INTO …`、
  pg → `INSERT INTO … ON CONFLICT DO NOTHING`。其余形态**不翻译**（作者显式选择
  的方言写法原样透传，跨方言部署需自写方言文件）。UPDATE / DELETE 不做任何检查，
  写了就会每次启动重复生效。
- **执行日志（回归与排障）**：seed / migrate / fixture / schema reconcile 的每条
  语句执行结果都记 tracing 日志——`module` / `file` / `seq` / `rows`（受影响行数）/
  `stmt`（截断 200 字符），成功 `ok`、失败 `failed` 附错误。`oj server` 落 `logs/`
  目录（终端镜像落盘）；`oj migrate` / `oj fixture` / `oj test` CLI 直跑落 stderr。
- **限制**：不记账本——重放历史靠执行日志，不在库内。
- **S006（构建门禁）**：seed.sql 禁 DDL（CREATE/ALTER/DROP/TRUNCATE/RENAME/GRANT/
  COMMENT——结构归 schema.yaml 或 migrations）；禁非幂等 INSERT；只准碰本模块表与
  `deps:` 声明模块的表。
- **S002（构建 + 启动双侧）**：同一张表不许两处 `CREATE TABLE`——表 → 模块单射，
  冲突 fail-fast，不静默合并。

### 2.2 fixtures/ —— 演示与测试数据，按需灌

- **入口**：`oj fixture -c config.yaml [-d dir] [--module M]`；`oj test` 装配时自动灌
  （`fixtures=true`）。**server 启动永不灌**。
- **行为**：模块目录下 `fixtures/*.sql`，按文件名排序逐文件、文件内按 `;` 切分顺序
  exec 到 default 库；**不记账本**；重复灌靠 SQL 自身幂等
  （推荐 `INSERT OR IGNORE` / `ON CONFLICT DO NOTHING`）。
- **适用**：本地开发造数、演示环境灌数、`oj test` 前置数据。
- **限制**：
  - 构建时整目录排除，**不随产物发布**——生产演示数据须走 migrations 或人工执行。
  - 无 S006 类静态检查：非幂等 SQL 重复灌会翻倍，作者自审。
  - 三方言都执行，但 SQL 方言兼容性（如 `OR IGNORE` 仅 sqlite/mysql）作者自担；
    跨方言用 `MERGE` 语义的写法不存在，写方言分支文件或取 portable 写法。

## 3. 迁移通道（migrations/）

### 3.1 文件命名与方言覆盖

- `{seq:04}__{desc}[.{sqlite|mysql|pg}].sql`，如 `0002__add_avatar.pg.sql`。
  `desc` 限 `[A-Za-z0-9_]`；seq 从 1 起**连续**（空洞 / 乱序 / 同 seq 双文件 → S007）。
- 方言覆盖文件**必胜**，无后缀通用文件兜底，其他方言文件跳过；同 seq 的通用与覆盖
  文件 `desc` 必须一致（账本 name 的唯一来源，M001 对比依据）。
- BOM / CRLF 载入侧自动规范化（Windows 检出不误判篡改）。
- **适用**：一切无法安全推导的变更——NOT NULL 列新增、改名、删列、类型变更、
  数据回填、方言特化 DDL（分区、表空间等）。

### 3.2 账本

- **单表 `_oj_migrations`**：`(module, version)` 复合主键 + `name` / `applied_on` /
  `checksum`。一行 = 该模块该迁移已应用；重跑零新增（幂等由账本保证）。
- **M001 篡改**：已应用迁移的文件内容被改（checksum 对不上）→ 拒绝。修历史错误的
  正确姿势是**追加新迁移**（新 seq 前向修正），不是改旧文件。
- **M002 缺文件**：账本里有、产物里没有 → 拒绝（产物不完整 / 回滚了 migrations 目录）。
- 旧版每模块一张 `_oj_migrations_<module>` 的库需一次性收敛（否则重迁移撞
  "table exists"）：

  ```sql
  CREATE TABLE _oj_migrations (module VARCHAR(255) NOT NULL, version int8 NOT NULL,
    name VARCHAR(255), applied_on VARCHAR(255), checksum VARCHAR(255),
    PRIMARY KEY (module, version));
  -- 每个旧账本一条（module 换成实际模块名），全部完成后 DROP 旧账本表：
  INSERT INTO _oj_migrations SELECT 'user', version, name, applied_on, checksum FROM _oj_migrations_user;
  ```

### 3.3 事务边界与并发（按方言）

| | sqlite / postgres | mysql |
|---|---|---|
| 事务性 DDL | 是：每个迁移 = 一个事务（迁移 SQL + 账本写入同事务，**原子**） | 否：DDL 隐式提交，**逐条执行** |
| 中途崩溃 | 当前迁移整体回滚，账本无残留 | **半套 DDL 落库且无账本行**：重跑撞 "table exists"，需人工清理残留对象后重试 |
| 并发互斥 | `pg_advisory_xact_lock`（模块级咨询锁）；sqlite 单连接池天然串行 | 账本 `(module, version)` 主键冲突兜底（后到者 INSERT 报错） |
| 多语句迁移文件 | 整文件同事务 | 逐条提交——**mysql 上尽量单语句一文件**，把"原子性"留给文件粒度的人工保证 |

### 3.4 存量库接入：--baseline

表已由历史手段（手工脚本 / 旧版本）建好的库：`oj migrate --baseline` 把 ≤ 最新 seq
的迁移**全部记为已应用而不执行**（账本齐平、DDL 不动）。之后新增迁移正常增量执行。
声明与实库的差异随后用 `oj schema diff` 核对、以 schema.yaml 收敛对齐。

### 3.5 启动门禁：server.migrate_on_start

| 值 | 默认 | 行为 |
|---|---|---|
| `auto` | **dev（-d src）** | 启动即 apply 到最新 + reconcile |
| `verify` | **release（-d dist）** | 只校验不执行：**M003** 账本 seq 超过产物最大 seq（降版部署）拒启；**M004** 有待应用迁移拒启（报错附 `oj migrate` 命令） |
| `off` | — | 逃生门：迁移完全归运维（先 migrate 后启动，不推荐常态） |

### 3.6 schema.yaml reconcile 的能力边界（声明式收敛）

- **会做（安全前向，幂等）**：缺表 `CREATE TABLE`、缺**可空**列 `ALTER ADD`、
  缺索引 `CREATE INDEX`。
- **拒绝（fail-fast 并打印迁移模板）**：NOT NULL 列新增（存量行无值）、疑似改名
  （缺新列 + 多旧列同时出现）——此时按报错里的模板手写 migrations/。
- **不检查**：类型漂移（方言类型反查长尾，人工核）。
- reconcile 只进 apply 路径（auto / `oj migrate`），verify 启动不收敛。

### 3.7 oj schema diff —— 漂移门禁（CI 可用）

- 只读对账，**有漂移 exit 1**：
  - **D001**：声明有实库无（缺表 / 缺列 / 缺索引）、实库有声明无（多列）；
  - **D002**：实库有而无任何模块声明的表（排除账本 `_oj_migrations%` 与 `sqlite_%`）。
- 误报逃生门：SQL 注释 `/* oj:allow-table=x,y */`。

## 4. 场景速查

| 场景 | 操作 |
|---|---|
| 新环境首次部署 | `oj build` → `oj migrate -c config.yaml -d dist` → `oj server`（release verify 门禁要求先迁移） |
| 开发冷启动 | `oj server -c config.yaml --api-path src`（auto 门禁自动建表 + seed 重放） |
| 加新表 / 可空列 / 索引 | 只改 `schema.yaml`，下次启动或 `oj migrate` 自动收敛 |
| 加 NOT NULL 列 | 手写迁移（`ALTER TABLE … ADD COLUMN … NOT NULL DEFAULT <值>`）+ schema.yaml 声明——reconcile 对此 fail-fast 并打印模板 |
| 改列名 / 删列 | 手写迁移（`RENAME COLUMN` / 先备份后 `DROP`）；删列同时从 schema.yaml 移除，残留会被 `oj schema diff` 报 D001 多列 |
| 存量库接入（表已存在） | `oj migrate --baseline`（§3.4） |
| 方言差异 | `0002__add_x.mysql.sql` 方言覆盖文件，与通用文件并存；无后缀 = 全方言执行 |
| 演示 / 测试数据 | `fixtures/`：`oj fixture` / `oj test`（§2.2）；引导数据走 seed.sql（§2.1） |
| 只迁一个模块 | `oj migrate --module user` / `oj fixture --module user` |
| 发布前巡检 | `oj schema diff`：D001/D002 有漂移 exit 1 |
| CI 无证书环境跑迁移 | `oj migrate` / `oj fixture` / `oj schema diff` 走**瘦身装配**（config → 插件 → 开库，不走 App 装配、无证书门禁、不起路由） |

## 5. 检查规则与错误码速查（运维排障用）

| 码 | 含义 | 触发点 | 下一步 |
|---|---|---|---|
| S001 | manifest 不合法 | `oj build` / 启动 | 按报错修 manifest.yaml |
| S002 | 同表多处建表（归属单射破坏） | `oj build` + 启动 seed 检查 | 保留唯一归属处的建表，删另一处 |
| S003 | 跨模块表访问未声明 deps | `oj build`；运行时 ownership_guard（warn/deny） | manifest 补 `deps:` |
| S004 | deps 版本范围不满足 | `oj build` | 对齐依赖模块版本 |
| S005 | manifest.tables 与 schema.yaml 双向不一致 | `oj build` | 两处声明对齐 |
| S006 | seed.sql 纪律：禁 DDL / 非幂等 INSERT / 越模块写 | `oj build` | DDL 移去 schema.yaml/migrations；INSERT 加幂等写法 |
| S007 | 迁移文件名不合法 / seq 空洞乱序 / 同 seq desc 不一致 | `oj build` / `oj migrate` 载入 | 重排 migrations/ 使 seq 从 1 连续、命名合规 |
| D001 | 声明 vs 实库漂移（缺表/缺列/多列/缺索引） | `oj schema diff`（exit 1） | `oj migrate` 收敛安全前向，其余手写迁移 |
| D002 | 实库有而未声明表 | `oj schema diff` | 补 schema.yaml 声明或人工清理 |
| M001 | 已应用迁移被篡改（checksum 不符） | apply | 追加新迁移前向修正，勿改旧文件 |
| M002 | 账本有、产物缺迁移文件 | apply | 补齐产物（先 `oj build`） |
| M003 | 账本 seq 超过产物最大 seq（降版部署） | release 启动 verify | 部署含最新迁移的产物；人工回退账本自担风险 |
| M004 | 有待应用迁移（含首启空账本） | release 启动 verify | 先 `oj migrate -c <config> -d <dir>` 再启动；`off` 是逃生门 |

## 6. 限制与红线汇总

1. **只前向，无 down 迁移**（refinery 语义）。降版部署被 M003 硬禁；schema 回滚 =
   换回旧 dist + 手写前向回填迁移（§7）。
2. **`;` 朴素切分**：所有 SQL 文件语句内不得含分号字面量。
3. **seed 幂等写法默认 sqlite 惯用法 `INSERT OR IGNORE`**，引擎按方言自动改写；
   `OR REPLACE` / `ON CONFLICT` / `ON DUPLICATE KEY` 不翻译，跨方言部署须自管（§2.1）。
4. **mysql 迁移无原子性**：DDL 隐式提交，崩溃可留半套 DDL（§3.3）——多语句文件谨慎。
5. **fixture / seed 的幂等是作者责任**：引擎不记账、不去重；S006 只在构建期挡 seed 的
   INSERT，fixtures 无任何门禁。
6. **reconcile 不做类型漂移检查**、不做删列、不做改名（§3.6）。
7. **`oj migrate` / `oj fixture` / `oj schema diff` 只作用 default 库**（config 缺
   `db.default` 直接报错）；多库场景其余库不参与。
8. **迁移 SQL 由引擎按文件执行与记账**：文件内容即契约——改一个字节 = M001。

## 7. 回滚

- **应用回滚**：换回上一版 `dist/`（`dist/manifests.yaml` 指回旧版本目录 + 重启）。
  schema 已前向的部分不自动回退——这就是 M003 禁降版部署的原因：账本领先于产物会被
  verify 拒启，必须部署**含最新迁移**的产物。
- **schema 回滚没有自动机制**：破坏性变更前备份 `*.sqlite` / mysqldump / pg_dump；
  回退以**新 seq 前向迁移**实现（如 `RENAME` 回去）。
- 账本与文件一致性由 S007 / M001 / M002 把守，人工改账本前先备份。

## 8. 遗留与已知取舍

- **旧版每模块账本**：一次性收敛 SQL 见 §3.2。
- **mysql 账本 version 列为 TINYINT**：每模块 > 127 个迁移时 INSERT 越界，须人工改型
  （`migrate.rs::mysql_ledger_ddl`）。

## 9. 变更记录

- **2026-09**：模块 `schema.sql` 通道**移除**（结构只剩 schema.yaml + migrations
  双轨；目录里残留的 `schema.sql` 不再被读取，可直接删除）；根 `config_dir/seed.sql`
  通道**移除**（启动只重放模块 seed）；seed 幂等写法默认 sqlite 惯用法
  `INSERT OR IGNORE`，引擎按方言**自动改写关键字**（mysql `INSERT IGNORE`、pg 句尾
  `ON CONFLICT DO NOTHING`）；seed / migrate / fixture / reconcile 语句级执行与
  结果记 **tracing 日志**（server 落 logs/，CLI 落 stderr）；mysql 账本 DDL
  `int8` → `TINYINT` 方言化（上限 127 个迁移/模块）。
