# dbx 上游补齐：G3（执行计划归一）+ U-14（DDL 源码生成）设计

> 2026-10-29。需求来源：`../oj-dbx/docs/dbx-upstream-requirements.md`（G3 阻塞 PR-1，U-14 阻塞 PR-6/7/8/9）。
> 用户已拍板：G3 独立 op；U-14 正向 + 反向（diff→DDL）**同批**交付；REQ-03 只补 import（本文档不含）。

## 意图与为什么

dbx（数据库管理前端）需要三类上游能力，当前 oj-bin 缺口如下：

1. **G3**：PG `EXPLAIN` 出文本行、MySQL 出 12 列表格，结构各异，前端无法维护双解析器。
2. **U-14 正向**：`db.catalog("ddl", t)` 在 PG 只覆盖视图（表返回空）、MySQL 直接 unsupported。
3. **U-14 反向**：结构编辑器需要「期望结构 → 变更 DDL」生成器（PR-7），dbx 不想手写方言 DDL。

## 目标 / 非目标 / 反目标

**目标**
- v0.1.108：`db.explain(sql)` 返回跨方言归一 plan 节点树；PG / MySQL 同构 JSON。
- v0.1.109：`db.catalog("ddl")` 对 PG / MySQL 真表返回可重放 DDL；`db.ddlDiff(from, to)` 返回变更 DDL 语句列表。
- 每个版本：静态守卫测试 + env-gated 真库腿（`OJ_TEST_PG` / `OJ_TEST_MYSQL`）+ CHANGELOG + docs/devkit 四件套同步。

**非目标**
- sqlite 的 explain（返回结构化 `OJ_DB_UNSUPPORTED`）；xugu / dameng 方言（本仓库不存在）。
- `EXPLAIN ANALYZE`（真执行，安全面不同）。
- 表**重命名** / 跨对象 diff（仅单表结构）；分区表 DDL 的 PG 完整还原（分区子句尽力而为，不阻塞主路径）。
- REQ-03 导入、U-9、U-6、CANCEL-NONSTREAM（后续版本，另行立项）。

**反目标**
- 为支持新能力破「零标识符内联」红线：MySQL `SHOW CREATE TABLE` 维持现有成文例外（白名单字符集 + 反引号翻倍）。
- 前端逐方言解析计划或拼接 DDL——归一必须在宿主侧完成。
- 静默空结果冒充 unsupported（既有口径：报错不静默）。

## 约束（设计红线）

- SQL 引擎常量零标识符内联；PG `name` 域列 `::text`；PG 目录查询带 `current_schema()` 限定；scoped 变体 scope 占位符恒为第一个 `?`（v0.1.107 硬契约，有静态守卫）。
- 异步测试 `tokio::test(flavor = "current_thread")`；测试用例驱动（防回归 + 可定位）。
- 用户可见变更同步 docs/devkit 四件套 + `cargo xtask build` + xtask 契约测试。

---

## v0.1.108 — G3：`db.explain(sql)`

### API

```
db.explain(sql: string, params?: unknown[]): { plan: PlanNode }
PlanNode = {
  operation: string,          // "Index Scan" / "Seq Scan" / "Nested Loop" / "query_block" ...
  relation: string | null,    // 表名（PG "Relation Name" / MySQL table_name）
  accessPath: string | null,  // 索引名 / 扫描方式（PG "Index Name" / MySQL key）
  cost: { startup: number, total: number } | null,  // 方言给不出 → null
  rowsEstimate: number | null,
  width: number | null,
  extra: string | null,       // PG "Filter"/"Index Cond"、MySQL "Extra" 归一
  children: PlanNode[]
}
```

- 新 op `op_db_explain`，新模块 `src/bridge/db_explain.rs`；`bootstrap.js` 挂 `db.explain`。
- sqlite → `OJ_DB_UNSUPPORTED`（capability=`explain`），报错不静默。
- 走 `bound_db`；**授权面 = db.query 同款**：包上 explain 前缀的整句过 `check_raw`（表归属）+ `check_tenant_raw`（租户守卫）+ `check_access`（风险分类——`classify` 已把 EXPLAIN 归 ReadOnly）+ REQ-05 配额（语句超时/字节闸）。即「能 explain 的语句 ⊆ 能真跑的语句」，零新增信息泄漏面。bind params 照 db.query 口径经 `normalize_and_trace` 绑定（PG/MySQL 的 EXPLAIN 内均接受参数位）——SQL 编辑器的参数化语句是主场景。

### 实现

- PG：跑 `EXPLAIN (FORMAT JSON) ` + sql（语句级文本拼接合法——`db.query` 本就以整句 SQL 为参数；explain 前缀是引擎常量）。结果单行列，JSON 解析后取 `[0].Plan` 递归归一。
- MySQL：`EXPLAIN FORMAT=JSON ` + sql。结果单列（列名 `EXPLAIN`），JSON 根是 `query_block`，`nested_loop` 数组 → children，`table` 节点取 `access_type`/`table_name`/`key`/`rows_examined_per_scan`/`cost_info.prefix_cost`。
- 归一器是纯函数（JSON → PlanNode），单测用两份真实计划样本（PG 索引扫描、MySQL nested_loop）钉死映射。
- 无 ANALYZE、无内层语句风险门：`EXPLAIN`（非 ANALYZE）不执行语句，副作用为零。这是**我的裁决**，理由：内层任何语句都不执行，门只增加误拒（dbx 要 explain UPDATE 的计划）；若后续加 ANALYZE 选项必须补门。

### 测试

- 静态守卫：PG 常量含 `FORMAT JSON` 且不含 `ANALYZE`；MySQL 常量含 `FORMAT=JSON`。
- 单测：两份样本 JSON → 归一树快照断言。
- env-gated 真库腿：`EXPLAIN SELECT ... WHERE pk = ?`（params 绑定）在 PG / MySQL 下断言 operation / relation / cost 非空、children 为数组。

---

## v0.1.109 — U-14

### 正向 DDL

**MySQL（近免费）**：`op_db_catalog` 在 `catalog_sql` 查表**之前**拦截 Ddl × MySql 路由到 `mysql_show_create`（helper/路由/行映射已全在，属半成品激活）——不造假常量去喂既有静态守卫（`scope_predicate_precedes_first_param` / `catalog_constants_param_count_and_no_like`）。scoped（for_schema）× MySQL × Ddl 维持显式 unsupported（SHOW 族不吃绑定参数，成文裁决）。静态守卫：`mysql_show_create` 白名单/反引号翻倍测试（补畸形标识符用例）。

**PG（本批主体）**：目录拼接器。
- `op_db_catalog` 的 Ddl × PG 分支改为**组合查询**：先做 `relkind` 探测（`pg_class` 按表名 + `current_schema()`），视图 → 走既有 `pg_get_viewdef` 路径（不回归已发布行为）；普通表 → 顺序执行 4 条引擎常量（均表名走绑定参数、scoped 变体逐条过 `resolve_scope`、scope 占位符恒在首）：
  1. 列：**新增 stitcher 自有常量** `pg_catalog_ddl_columns`（不动 L2 已发布输出）——在 `format_type` / not_null / default / is_pk 之外加 `a.attidentity`（identity 列）；
  2. 约束：`pg_constraint` `contype IN ('p','u','c','f')` + `pg_get_constraintdef(oid)`（FK 定义由此拿到，与 ForeignKeys kind 的列表口径互补）；
  3. 索引定义：`pg_get_indexdef(i.indexrelid)`（含 `WHERE` 部分索引）；
  4. 注释：复用 `Kind::Comment` 常量 + `obj_description` 取表注释。
- Rust 侧 `pg_create_table(rows...) -> String` 按既有目录方言拼接 `CREATE TABLE` + `CREATE INDEX` 语句 + `COMMENT ON` 语句。拼接器是纯函数：单测喂固定行数据断言输出文本，CI 无 PG 可跑。
- **serial/identity 可重放**（验收硬要求，最常见表形）：列默认值为 `nextval('x_seq'::regclass)` 时，前置 `CREATE SEQUENCE IF NOT EXISTS x_seq` 并保持原默认文本；`attidentity` 非空时渲染 `GENERATED {ALWAYS|BY DEFAULT} AS IDENTITY`（不产序列语句）。
- 不存在的表名 → `items: []`（与畸形表名同口径，不报错不泄漏）。
- 输出形态沿用 catalog 口径：`{name, ddl}`（`catalog()` 的 `Kind::Ddl` 分支已产出）。
- scoped 变体照常可用（scope 占位符恒为首 `?`，静态守卫已有；拼接器不感知 scope）。

### 反向 DDL：`db.ddlDiff(spec)`

```
db.ddlDiff({
  objectType: "table",
  name: string,               // 目标表名（生成 DDL 用）
  from: Structure | null,     // null = 新建表
  to: Structure               // 期望结构
}): { statements: string[] }  // 有序、可逐条重放

Structure = {
  columns:    [{ name, nativeType, nullable, default, primaryKey, comment? }], // 与 listColumns 行同词汇（已发布口径），comment 可选
  constraints:[{ name, type: "pk"|"uq"|"ck", definition, columns }], // 与 catalog constraints 同词汇
  indexes:    [{ name, unique, primary, origin, columns: [..] }],    // 与 listIndexes 行同词汇
  foreignKeys:[{ name, columns, refTable, refColumns, ... }]        // 与 listForeignKeys 同词汇
}
```

- 新 op `op_db_ddl_diff`，新模块 `src/bridge/db_ddl.rs`；**纯渲染零 DB I/O**（仅取绑定 db 的方言），输出是数据不是执行——执行仍走既有 `db.exec` + `allow_ddl` + G9 确认门，红线不破。
- diff 按名字匹配：列（added/removed/changed：type/default/notNull/comment）、约束、索引、外键各出语句。方言渲染：
  - PG：`ALTER TABLE ... ADD/DROP/ALTER COLUMN`（`USING` 子句处理类型变更）、`ADD CONSTRAINT`（definition 来自 `pg_get_constraintdef` 口径）；
  - MySQL：同构 ALTER 族（`MODIFY COLUMN` 承载类型/空值/默认值变更）；
  - sqlite → `OJ_DB_UNSUPPORTED`。
- 标识符引用：方言引号 + 翻倍转义（与 `mysql_show_create` 同纵深）；**这是生成器的必然职责**——输出为文本数据，注入面不变（执行侧授权不变）。
- `from` 直接喂活体内省结果（词汇表刻意与 listColumns/catalog constraints/listIndexes/listForeignKeys 对齐），dbx 零映射成本。
- 单测：固定 from/to 对的语句快照（PG + MySQL 各建表 / 加列 / 改类型 / 加索引 / 加 FK 用例），CI 可跑；env-gated 真库腿：PG 建表 → 内省回喂 `from` → 构造 `to` → 生成 ALTER → 执行 → 断言列形态变更生效。

### 测试与文档（两版共同）

- fmt / clippy / workspace 测试全绿后 bump；CHANGELOG 两节。
- docs/devkit 四件套：api-manual（`db.explain` / `db.ddlDiff` 新节 + `db.catalog` ddl 能力矩阵改「PG/MySQL 支持」+ 错误表）、SKILL.md 陷阱（explain 不支持 ANALYZE、mysql for_schema × ddl unsupported、ddlDiff 输出需经 G9 执行）、scenarios（可照抄用例）、README 能力表。
- `cargo xtask build` 归置 bin/devkit + xtask 契约测试。

## 交付顺序

1. v0.1.108 = G3（独立小版本，dbx PR-1 可先行）。
2. v0.1.109 = U-14 正向（MySQL 路由 + PG 拼接器）+ 反向 ddlDiff。
3. 验收以 dbx 文档 §验收 为准：pgtest / mysqltest 真库返回非空可重放 DDL；同查询两方言同构 plan JSON。

## builder-check 已定项（本轮裁决，不再讨论）

- `db.explain` 支持 Tx 目标（镜像 catalog 通路）；无 scoped 变体。
- `db.ddlDiff` 授权走 `granted_only`（catalog 先例；纯渲染无 DB I/O）。
- `db.explain` 的 trace 走既有 `trace_sql` 通路（含 SQL 文本审计，不进错误消息）。
