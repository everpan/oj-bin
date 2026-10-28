# U-14 元数据目录内省扩展（三批次）设计稿

> 状态：**设计稿（待四路评审；评审处置表见 §8，动工前须过评审——批次 a 最低门槛为安全+兼容两路）**。
> 上游：`docs/prds/oj-feature-requirements.md` §5 `U-14`（知识源：dbx 仓
> `crates/dbx-tauri-schema/commands.rs` 68 条内省命令）。
> 相邻：U-11（server_version——golden 快照定位键）；L2（v0.1.90，listColumns/Indexes/FK 三件）；
> L6（v0.1.94，插件 typed 池读侧解码——本设计的行形状依赖它）。

## 0. 范围（三批次）

**宿主侧实现**（与 L2 同构：`introspect.rs` 方言 SQL 常量 + 既有 `DataAccessor` 通路），
**ABI 不动**（12 保持；全部是参数化查询 + 行归一，不碰 vtable）。

| 批次 | 内容 | 新 Kind（`introspect.rs`） |
|---|---|---|
| **a 浏览增强** | 对象 DDL/源码（mysql `show create table/view/procedure…`、pg `pg_get_viewdef`/`pg_get_functiondef`/`pg_get_indexdef`）、列/表注释（`information_schema.columns.column_comment` / `pg_catalog.pg_description`）、constraints（PK/UNIQUE/CHECK/FK 全量，独立于既有 FK 三件）、行数统计（精确 `count` 走业务连接；估算走 `information_schema`/`pg_class.reltuples`，**默认估算**——精确 count 是全表扫描） | `ddl` / `comment` / `constraints` / `stats` |
| **b 管理对象** | sequences（pg `pg_sequences`、mysql 模拟 `auto_increment` 列说明）、views 清单+定义、triggers（`information_schema.triggers` / `pg_trigger`）、partitions（pg `pg_partitioned_table` 家族、mysql `information_schema.partitions`）、routines（functions/procedures 清单+源码）、types（pg `\dT` 等价、mysql `enum/set` 列聚合） | `views` / `triggers` / `partitions` / `sequences` / `routines` / `types` |
| **c 生态（按需，评审后可能砍）** | 库/schema 清单（`show databases` / `pg_database`——**租户隔离敏感**，见 §3）、补全对象清单（completion）、linked servers（mysql federated/mssql 语义）、原生作业（pg cron / mysql events） | `catalogs` / `jobs` / … |

**不做**：

- 写操作（DROP/ALTER 经内省出口）——内省**只读**；管理写走业务 handler（授权面完全不同）。
- 超出三方言的命令矩阵（开放方言：`unsupported`，不静默回落——PRD 验收明文）。
- mssql/oracle/达梦（PRD §7 显式不做）。
- **ABI 扩展内省 vtable 槽**——若评审认为复杂命令（如需要专门解码的 catalog 结构）必须插件侧
  实现，才升级为 ABI 13 提案；本设计默认宿主侧够用（L2/L6 先例）。

## 1. 现状（file:line）

- 宿主内省：`src/bridge/introspect.rs`（`Kind` 枚举 + 方言 SQL 常量 + `MAX_ROWS`）；
  `db.rs:1330`（`valid_introspect_table` 表名白名单）、`db.rs:1351`（`introspect` 走 accessor）。
- op 面：`op_db_list_tables/list_columns/list_indexes/list_foreign_keys`（`db.rs`），
  JS `db.listTables/listColumns/...`（bootstrap.js），三方言 shape 已统一（L2）。
- 授权：内省 op 过 `db_access`（L1/L2 既有链）；`granted_only` 贯穿 = 未授权别名连不上、
  未授权语句照拒。
- 行形状：插件 typed 池读侧解码（L6）；宿主 sqlite 同口径（`::text`，`db.rs:1253`）。

## 2. 归一 shape（评审焦点：三方言一致性）

每命令一个统一 JSON shape，方言差异收敛在 `dialect` 字段 + 可空字段：

```jsonc
// 统一信封：{ kind, dialect, items: [...] }，行数受 MAX_ROWS 封顶
{ "kind": "ddl",      "items": [{ "name": "t", "type": "table", "ddl": "CREATE TABLE …" }] }
{ "kind": "constraints", "items": [{ "table": "t", "name": "…", "type": "pk|uq|ck|fk",
                                     "columns": [...], "refTable": null, "refColumns": null,
                                     "definition": "CHECK (…)" }] }
{ "kind": "stats",    "items": [{ "name": "t", "rowsEstimate": 1234, "sizeBytes": 40960,
                                  "precise": false }] }
```

- mysql/mysql 型：`information_schema` + `show create`；pg：`pg_catalog`；sqlite：多数 b 类
  命令**无 catalog 来源**（triggers 走 `sqlite_master`、sequences/views 走 `sqlite_master`
  模拟、partitions/routines/types → `unsupported`）——**按 kind × 方言给能力矩阵**，
  落 db-guide 表格（三方言 × 全 kind 的 ✅/模拟/❌）。
- 排序与可空口径与 L2 同规则（按名再按列序；无则 null）。

## 3. 安全（评审重点）

1. **`catalogs`（库/schema 清单）是租户边界敏感面**：一库一租户部署里列全库清单 = 泄租户存在性。
   建议：默认 `unsupported`（不做），产品需要时按 `db_access.<alias>.allow_catalogs: true`
   显式开——**评审定夺是否纳入批次 c**。
2. **DDL/源码回显**：存储过程/视图源码可能内嵌凭据（开发者的锅，但会经内截面出）。处置：
   过 `secret::redact`（L7 同判据：仅当文本真含 `://` 凭据 URL 形态）+ 文档声明。
3. **`granted_only` 贯穿**：所有新 op 复用 `check_access` 同链（未授权别名 `db_not_granted`）；
   不新增特权面。
4. 只读保证：批次内全部是 `select`/`show`/`explain` 形态，过既有 tenant/raw 守卫链。

## 4. 分期与验收

| 批 | 估 | 验收 |
|---|---|---|
| a | 2 天 | 三方言 golden 快照（env-gated 真库 + sqlite fixture）断言 shape 一致；sqlite/mysql/pg 各至少 3 命令实测 |
| b | 3 天 | 同上 + 能力矩阵表（db-guide）+ sqlite 模拟/unsupported 行为实测 |
| c | 评审后另立 | ——（`catalogs` 若纳入，须先过 §3.1 的产品裁决） |

- **golden 快照**：每命令 × 每方言一份 JSON fixture（真库录制、含 `serverVersion` 定位键
  ——U-11 产出）；CI env-gated skip-as-pass（与 L6 同口径，交接文档坑②）。
- **JS 面**：`db.listXxx(...)` 家族扩展（bootstrap.js），handler `.schema` 出参声明由
  业务侧按归一 shape 写（devkit 给样例）。
- 每批次独立发版（a → v0.1.100，b → v0.1.101，c 视评审）。

## 5. 文档面

db-guide §10（内省章扩成命令矩阵）、api-manual §6/§13、scenarios 场景 51、SKILL、README；
`global.d.ts` 全部新方法类型。

## 6. 依赖

- U-11（server_version 进 golden 定位键）——建议同车或先行。
- L6 typed 池解码（已具备）；若批次命令撞上未解码类型（如 pg `regclass`），按 L6 口径
  **响亮报错**不静默。

## 7. 风险

| 风险 | 处置 |
|---|---|
| 68 条命令映射进来范围爆炸 | 批次化 + 每批只做 PRD 点名的命令族；c 批默认砍到按需 |
| mysql `show create` 是**语句**不是 catalog 查询——过 tenant 守卫可能误伤 | `show`/`explain` 白名单进 `valid_introspect` 同层判定，不进业务守卫 |
| sqlite 模拟语义与真 catalog 有出入（如 auto_increment ≠ sequence） | 文档明示「模拟」字样进字段（`simulated: true`） |

## 8. 待评审问题（四路）

| 路 | 问题 |
|---|---|
| 安全 | §3.1 `catalogs` 是否纳入？DDL 回显的 redact 判据够不够？ |
| 兼容 | 宿主侧 SQL 常量 vs 插件侧实现：MySQL/PG 小版本间 catalog 列差异（8.0 vs 5.7 / pg12+ 分区语法）——声明最低支持版本并 golden 锚定，可否？ |
| 性能 | `stats` 估算口径（`reltuples` 可能严重失真）是否加 `precise` 显式开关（全表扫描自担）？ |
| 产品 | b 批 six kinds 一次交付还是拆 a/b 间再插一期（dbx 前端联调节奏）？ |
