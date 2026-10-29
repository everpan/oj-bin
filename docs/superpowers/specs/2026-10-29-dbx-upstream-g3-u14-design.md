# dbx 上游补齐：G3（执行计划归一）+ U-14（DDL 源码生成）设计

> 2026-10-29。需求来源：`../oj-dbx/docs/dbx-upstream-requirements.md`（G3 阻塞 PR-1，U-14 阻塞 PR-6/7/8/9）。
> 用户已拍板：G3 独立 op；U-14 正向 + 反向（diff→DDL）**同批**交付；REQ-03 只补 import（本文档不含）。
> 同日经架构师 / 专家工程师 / 产品经理 / 运维工程师四角色评审，采纳意见已并入（见文末评审记录）。

## 意图与为什么

dbx（数据库管理前端）需要三类上游能力，当前 oj-bin 缺口如下：

1. **G3**：PG `EXPLAIN` 出文本行、MySQL 出 12 列表格，结构各异，前端无法维护双解析器。
2. **U-14 正向**：`db.catalog("ddl", t)` 在 PG 只覆盖视图（表返回空）、MySQL 直接 unsupported。
3. **U-14 反向**：结构编辑器需要「期望结构 → 变更 DDL」生成器（PR-7），dbx 不想手写方言 DDL。

## 目标 / 非目标 / 反目标

**目标**
- v0.1.108：`db.explain(sql, params?, opts?)` 返回跨方言归一 plan 节点树（含节点类别）；PG / MySQL 同构 JSON。
- v0.1.109：`db.catalog("ddl")` 对 PG / MySQL 真表返回可重放 DDL；`db.ddlDiff(spec)` 返回**结构化 diff + 变更 DDL 语句列表**。
- 每个版本：静态守卫测试 + env-gated 真库腿（发布前必须真机跑过一次并登记结果）+ CHANGELOG + docs/devkit 四件套同步（含 `global.d.ts`）。

**非目标**
- sqlite 的 explain（`OJ_DB_UNSUPPORTED`，capability=`explain`）；xugu / dameng 方言（本仓库不存在）。
- `EXPLAIN ANALYZE`（真执行，安全面不同）。
- 表**重命名** / 跨对象 diff（仅单表结构）。
- **跨方言结构对比**（PG↔MySQL 迁移核对）：类型等价类映射（int/varchar/text/datetime ~20 个）是独立工作项，dbx PR-9 跨库场景暂以「不可比」处理，api-manual 写死。
- 分区表 DDL 完整还原（子分区等）：relkind='p' 走拼接器 + `pg_get_partkeydef` 尽力而为，文档标注 best-effort。
- PG 不还原清单（不进 DDL、只在文档列明）：自定义类型（enum/domain，前提类型已存在）、collation、attcompression/storage、表级 WITH(fillfactor)/tablespace。

**反目标**
- 为支持新能力破「零标识符内联」红线：MySQL `SHOW CREATE TABLE` 维持现有成文例外（白名单字符集 + 反引号翻倍）。
- **参数绑定失败即内联值兜底**——MySQL 带参 explain 若被方言拒绝，结构化报错点名版本要求，绝不内联（红线）。
- 前端逐方言解析计划或拼接 DDL——归一必须在宿主侧完成。
- 静默降级：空结果冒充 unsupported、宽表静默截断、假 DDL 冒充正确 DDL，一律禁止（见各节结构化报错条款）。

## 约束（设计红线）

- SQL 引擎常量零标识符内联；PG `name` 域列 `::text`；PG 目录查询带 `current_schema()` 限定且**每条常量恰出现一次**（静态守卫）；scoped 变体 scope 占位符恒为第一个 `?`。
- **全部方言知识常量必须进 `introspect.rs` 并挂进 `sql()`/`catalog_sql()` 的守卫遍历**——stitcher 新增常量也不例外（第二方言知识源 = 漂移，v0.1.106/107 两次事故同类）。
- 异步测试 `tokio::test(flavor = "current_thread")`；用例驱动（防回归 + 可定位）。
- 用户可见变更同步 docs/devkit 四件套（api-manual / scenarios / SKILL / README **+ global.d.ts**）+ `cargo xtask build` + xtask 契约测试。

---

## v0.1.108 — G3：`db.explain(sql, params?, opts?)`

### API

```
db.explain(sql, params?, opts?: { timeoutMs?: number }): { plan: PlanNode }
PlanNode = {
  category: "scan" | "join" | "aggregate" | "sort" | "other",  // 宿主侧枚举，渲染选图标/着色
  operation: string,          // PG "Node Type" 原文；MySQL access_type 原文（ALL/const/ref/...）
  relation: string | null,
  accessPath: string | null,  // PG "Index Name"；MySQL key
  cost: { startup: number | null, total: number | null },     // 字段级 null；MySQL startup 恒 null、total=prefix_cost
  rowsEstimate: number | null,// PG "Plan Rows"；MySQL rows_produced_per_join（对齐输出行语义）
  width: number | null,       // MySQL 恒 null
  extra: string | null,       // 全部谓词字段按序拼接（PG: Filter/Index Cond/Recheck Cond/
                              // Join Filter/Hash Cond/Merge Cond/Rows Removed by Filter；
                              // MySQL: used_key_parts/ref/filtered/Extra）
  children: PlanNode[]
}
```

- 新 op `op_db_explain`，新模块 `src/bridge/db_explain.rs`（仅方言前缀常量 + 归一纯函数 + op 壳）；`bootstrap.js` 挂 `db.explain`。
- sqlite → `OJ_DB_UNSUPPORTED`（capability=`explain`）。
- `opts.timeoutMs` 与 db.query/exec/stream 同口径（REQ-05 收紧旋钮，bootstrap `callOpts` 现成）。

### 实现（同一条通路，不是抄一遍守卫链）

`op_db_explain` = 校验方言 → **对剥掉前缀的内层 SQL 显式过一遍 `check_tenant_raw`**（Deny 模式下 `EXPLAIN UPDATE <未注册表>` 必须拒，否则首词分派被 EXPLAIN 前缀绕过 fail-closed 分支）→ `format!("{EXPLAIN_PREFIX}{sql}")` 直接调 **`query_impl` 原函数**（守卫/配额/trace/session 行为零新代码、零漂移面；Pool 臂天然继承 race_statement + ByteMeter，Tx 臂按 db_query 成文裁决无语句超时——限制表登记 api-manual）→ 取**首行首列**（按名取 `QUERY PLAN`/`EXPLAIN` 有版本风险）→ 归一纯函数。

- PG 前缀：`EXPLAIN `（**文本计划**）。真机实测（v0.1.108 契约实验，oj-test-postgres 16）：`EXPLAIN (FORMAT JSON)` 输出 json 类型列，sqlx Any 驱动不解（`PgTypeInfo(Json)`，有无参数同炸）；文本格式绑定参数可用 → PG 归一输入 = 文本计划的缩进树，由 Rust 解析器 `normalize_pg_text_plan` 转 PlanNode（extra 收 Index Cond/Filter/Hash Cond 等缩进附属行）。
- MySQL 前缀：`EXPLAIN FORMAT=JSON `（真机实测带参可用）。归一器是**递归下降任意容器键**的通用 walker（`nested_loop` / `grouping_operation` / `duplicates_removal` / `ordering_operation` / `union_result.query_specifications[]` / `materialized_from_subquery` / update·delete 标志），叶子是 `table` 节点——**禁止 nested_loop 特判**。
- 授权语义如实表述：「explain 不获得执行能力；副作用维与真跑等价（非 ANALYZE 零执行），风险维按 EXPLAIN 归 ReadOnly 放宽」——**不承诺「能 explain ⊆ 能真跑」**（read_only 下 EXPLAIN UPDATE 可 explain 不可跑）。

### 契约裁决点（已完成：v0.1.108 真机实验）

**结论（oj-test-postgres 16 / oj-test-mysql 8.4 实测）**：带参 EXPLAIN 两方言均可用——PG 需 `$n` 原生占位（op 通路由 normalize_and_trace 做 `?` → `$n`），MySQL `?` 直接可用；预写「params×mysql 结构化回落」**不启用**。另实测发现：PG `EXPLAIN (FORMAT JSON)` 的 json 输出列 sqlx Any 驱动不解（有/无参数同样报错）→ PG 归一输入定为文本计划（见上节）。env-gated 腿发布前真机跑通并登记结果（CI 无服务容器恒 skip——惯例：诚实登记）。

### 测试

- 静态守卫：PG 前缀不含 `ANALYZE`（文本格式）；MySQL 前缀常量含 `FORMAT=JSON` 且不含 `ANALYZE`；两者以空格结尾。
- 单测样本快照：PG 索引扫描（含 Filter/Index Cond → extra）；MySQL **单表扫描 + UNION + GROUP BY 三种**（容器 walker 钉死）+ nested_loop 样本。
- 守卫链：Deny 模式下 `EXPLAIN UPDATE <未注册表>` → 拒（钉死内层租户检查）。
- env-gated 真库腿：PG / MySQL 带参 `EXPLAIN SELECT ... WHERE pk = ?` 断言 operation/relation/cost 非空、children 为数组。

---

## v0.1.109 — U-14

### 正向 DDL

**MySQL（半成品激活）**：`op_db_catalog` 在 `catalog_sql` 查表**之前**拦截 Ddl × MySql 路由到 `mysql_show_create`（当前是死代码——查表先返回 None 抛 unsupported）。不动 scoped×SHOW→unsupported 成文先例。文档备注：SHOW 输出含 `AUTO_INCREMENT=<当前值>`（重放拷走计数器，前端可 strip）。

**PG（本批主体）**：`op_db_catalog` 的 Ddl × PG 分支改组合查询，**relkind 探测先行**：

| relkind | 对象 | 路由 |
|---|---|---|
| 'v' / 'm' | 视图 / 物化视图 | 既有 `pg_get_viewdef` 路径，**输出逐字节保持已发布行为** |
| 'r' / 'p' | 表 / 分区表 | 拼接器（'p' 附 `pg_get_partkeydef`，best-effort） |（✅ v0.1.109 终审已落地：PARTITION BY 随 ddl 产出，子分区不产出）
| 其余（'f'/'i'/'S'/'c'/'t'） | 外部表等 | `items: []`，**绝不落拼接器**（假 DDL 比空结果更糟） |

静态守卫 + 单测矩阵钉死上表；不存在的表名 → `items: []`。

拼接器 = 探测 + 4 条查询（Pool 臂各取连接，**整体包一个 deadline**——REQ-05 接线是逐调用点的，catalog 通路现状为零接线，绝不允许 4 条慢查询拖死池槽位；实施为 6 常量：列/约束/索引/列注释/表注释拆分/partkey，见 feedback B-表#4）：

1. **列**（新常量，**进 introspect.rs + 守卫遍历**）：`pg_attribute` 必须 `attnum > 0 AND NOT attisdropped`（dropped 列是 `........pg.dropped.N........` 占位行）；取 `format_type` / not_null / default / is_pk / **`attidentity`** / **`attgenerated`**（`pg_get_expr(adbin, adrelid)` 渲染 `GENERATED ALWAYS AS (expr) STORED`）。
2. **约束**（新常量）：`pg_constraint` `contype IN ('p','u','c','f','x')` + `pg_get_constraintdef(oid)`（**含排他约束 'x'**，重放丢约束违反不静默口径）；拼接 `ADD CONSTRAINT "name" <definition>`——**定义文本不含约束名，名字要方言引号**。注：PG18 把 NOT NULL 收编为 contype='n'，届时两路径口径差一行注释。
3. **索引定义**（新常量）：`pg_get_indexdef(i.indexrelid)`，**必须排除约束支撑索引**——`AND NOT EXISTS (SELECT 1 FROM pg_constraint c WHERE c.conindid = i.indexrelid)`（否则与约束查询的 ADD CONSTRAINT 重复产出，重放索引名冲突必炸；`NOT indisprimary AND NOT indisunique` 粗过滤会误杀非约束 CREATE UNIQUE INDEX，禁止）。
4. **注释**（新常量；既有 Kind::Comment 只产列注释，**不能复用**）：列注释 `col_description` + 表注释 `obj_description(oid, 'pg_class')`（**双参数形态**，单参数已废弃）。

- Rust 侧 `pg_create_table(rows...) -> String` 拼 `CREATE TABLE` + `CREATE INDEX` + `COMMENT ON`。**serial 走 `pg_depend`（'a' auto 依赖）取序列名，绝不解析 nextval 文本**：前置 `CREATE SEQUENCE IF NOT EXISTS`，表创建后补 `ALTER SEQUENCE ... OWNED BY t.col`（DROP TABLE 后序列不成孤儿）。
- **容量防线**：任一路查询行数达 `MAX_ROWS` → 结构化报错（`OJ_DB_CAPACITY` 口径），禁止 `.take(MAX_ROWS)` 静默截断产出不完整 DDL。
- trace：逐查询照常 `trace_sql`（每条都是真实 SQL 执行，与既有内省同口径）。
- 已知限制登记 db-guide/api-manual：4 条查询无事务包裹 = best-effort 快照（DDL 管理工具可接受，Tx 臂任一条失败会毒化事务——错误消息点名）；Pool 臂各查询可能落不同连接（search_path 会话漂移低概率，要一致就走 db.tx / forSchema）；跨 schema FK 的 definition 带 schema 限定 → **DDL 不可跨库移植**。

### 反向 DDL：`db.ddlDiff(spec)`

```
db.ddlDiff({
  objectType: "table",
  name: string,
  from: Structure | null,     // null = 新建表
  to: Structure
}): {
  diff: {                     // 结构化 diff——PR-9 比较视图直接渲染它，statements 供执行
    // ⚠️ 实施校正（v0.1.109 发布裁决，详见同目录 feedback 文档 B1）：实际发布为**嵌套**形状——
    // diff.<dim>.{added, removed, changed[]}（dim ∈ columns/constraints/indexes/foreignKeys；
    // changed 条目 { name, field, from, to }，field: nativeType/nullable/default/identity/
    // comment/position + 具名维度的 type/definition/unique/columns/refTable）。
    // 本节扁平键草图（addedColumns/…）为早期示意，勿据其实现。
    addedColumns: [...], removedColumns: [...],
    changedColumns: [{ name, field, from, to }],   // field: nativeType/nullable/default/identity/comment/position
    addedConstraints/removedConstraints/changedConstraints: [...],
    addedIndexes/removedIndexes/changedIndexes: [...],
    addedForeignKeys/removedForeignKeys/changedForeignKeys: [...]
  },
  statements: string[]        // 有序、可逐条重放
}
```

- 新 op `op_db_ddl_diff`，新模块 `src/bridge/db_ddl.rs`（**仅纯渲染器**）；`granted_only` 授权（catalog 先例）；**零 DB I/O**。执行仍走 `db.exec` + `allow_ddl` + G9——红线不破，执行侧 `extract_tables` 归属守卫接得住生成物。
- **Structure 契约**（与已发布内省输出同词汇，并显式列出差异项）：
  ```
  columns:    [{ name, nativeType, nullable, default, primaryKey, identity?, comment? }]
              // identity: "always" | "byDefault" | 缺省——listColumns 同步加性输出该字段（不破契约）
  constraints:[{ name, type: "pk"|"uq"|"ck", definition, columns }]
  indexes:    [{ name, unique, primary, origin, columns: [..] }]
  foreignKeys:[{ name, columns, refTable, refColumns, ... }]
  ```
  - **缺省语义（数据安全，钉死）**：某维度字段**缺省 = 不参与 diff**；**空数组 = 期望为空（出 DROP）**。省略 `indexes` 绝不生成 DROP INDEX 全家桶。
  - 畸形 Structure（未知 constraint type、缺 name/nativeType）→ 参数错误结构化报错，写进 api-manual 错误表。
  - identity 列**跳过 default 文本比对**（nextval 是 identity 的派生物，否则恒报假 changed）。
- **引用与注入防线**：标识符引用只做**方言引号 + 翻倍**（PG `"`→`""`、MySQL 反引号→双反引号）——**不复用 `mysql_show_create` 的白名单字符集**（内省可见的合法表名可含非 ASCII/空格，白名单只增误拒）；静态守卫喂恶意标识符（含引号/换行；**NUL 直接拒**）。verbatim 片段字段（nativeType/default/definition）**拒断 `;`**（合法片段不含语句分隔符）；api-manual/SKILL 标注四字段 = SQL 片段，结构编辑器的自由文本框须自行校验。
- **diff 语义**：同方言精确字符串比较（跨方言不可比，见非目标）。
- **语句排序（钉死）**：DROP（FK → CHECK/UQ → 索引 → 列）→ ADD/ALTER 列 → 约束/索引 → FK 最后。
- **原子性文档**：PG DDL 事务安全可整批回滚；MySQL DDL 隐式提交，部分应用是常态——api-manual/SKILL 必写。PG `ALTER COLUMN TYPE` 引擎自管表重写与索引重建，生成器默认产 `USING "col"::newtype`；依赖该列的视图/函数执行期拒绝，生成器管不了，文档写明。
- **列位置**：MySQL 按 ordinal 差产 `AFTER x`/`FIRST`；PG 无列位置 DDL → 忽略位置差（api-manual 写明）。
- sqlite → `OJ_DB_UNSUPPORTED`（capability=`ddl_diff`）。
- 陷阱登记 SKILL.md：ddlDiff **不生成序列对象**（加列带 nextval 默认值的全新环境重放会失败，与正向 stitcher 不对称）。

### 测试与文档（两版共同）

- fmt / clippy / workspace 测试全绿后 bump；CHANGELOG 两节。
- docs/devkit 四件套 + `global.d.ts`（`db.explain` / `db.ddlDiff` 的 TS 类型进 global.d.ts，否则发布物类型面缺新 API）：
  - api-manual：新 API 节 + `db.catalog` ddl 能力矩阵改「PG（表/视图/物化视图）/MySQL 支持」+ **错误/限制表**（capability 串 `explain`/`ddl_diff`/scoped-ddl 复用既有 `for_schema`；MySQL 带参 explain 版本要求；字段级 null 口径；MySQL DDL 隐式提交；PG 无列位置；Tx 臂无超时；不还原清单；不可跨库移植）。
  - SKILL.md 陷阱：explain 无 ANALYZE、MySQL for_schema × ddl unsupported、ddlDiff 缺省 vs 空数组、verbatim 字段拒 `;`、不生成序列。
  - scenarios：可照抄用例 + **版本错位能力探测**（`typeof db.explain === 'function'` 探测降级，禁版本号嗅探——回滚 0.1.107 时新 API 是 `TypeError` 而非 OJ_DB_UNSUPPORTED，dbx 须走能力探测，同 PG scoped stopgap 惯例）。
- `cargo xtask build` 归置 bin/devkit + xtask 契约测试。

## 交付顺序

| 版本 | 内容 | 解锁 dbx |
|---|---|---|
| v0.1.108 | G3 `db.explain` | PR-1 执行计划面板 |
| v0.1.109 | U-14 正向（MySQL 激活 + PG 拼接器）+ 反向 `db.ddlDiff` | PR-6 DDL 查看 ✓；PR-7 结构编辑（经 G9 写回）✓；PR-9 同方言结构对比 ✓（跨库对比 ✗ 另立） |
| — | REQ-03 任务式导入 | **PR-8 导入向导仍阻塞**，dbx 排期以 REQ-03 为准，勿误读 v0.1.109 为 PR-8 解锁 |

应急口（不改变已拍板）：若 ddlDiff 滑期，允许正向单独切 v0.1.109、ddlDiff 顺延 v0.1.110——PR-6 不被反向生成器拖累。

---

## 已定裁决（builder-check + 四角色评审，不再讨论）

- `db.explain` 支持 Tx 目标（经 `query_impl` 的 `resolve_target` 自动同语义）；无 scoped 变体。
- `db.ddlDiff` 授权走 `granted_only`；纯渲染零 DB I/O。
- stitcher 常量 + 行归一全部进 `introspect.rs`（含守卫遍历扩展：每条新常量 `current_schema()` 恰一次、scope 占位符在首、`::text`）；`db_ddl.rs` 只留 ddlDiff 渲染器。
- `db.explain` 实现 = 复用 `query_impl`（同一条通路），不复制守卫链；内层 SQL 单独过租户检查。
- MySQL 带参 explain 可行性以真机实验为裁决点；失败即结构化报错，绝不内联。

## 评审记录（2026-10-29）

四角色评审（架构 / 专家工程 / 产品 / 运维）共 30+ 条发现，全部采纳或转为文档条款，重点：

- **架构**：explain 复用 query_impl 而非抄守卫链（F1）；Structure 接不住 identity 的正反向不对称（F2）；stitcher 常量进 introspect.rs 守卫遍历（F3）；引用 helper 去白名单（F5）；explain 补 `opts.timeoutMs` + global.d.ts（F7）。
- **专家**：MySQL 参数化 explain 预裁决（S1）；索引查询排除约束支撑索引 `conindid`（S2）；serial 走 pg_depend + OWNED BY（S3）；MySQL 归一器通用容器 walker + 三样本（H4）；operation=access_type/accessPath=key（H5）；rows_produced_per_join 对齐 PG 语义（H6）；extra 收全部谓词字段（H7）；contype 'x' 排他约束（H8）；dropped/generated 列过滤（H9）；diff 排序与原子性（M11）；`ADD CONSTRAINT "name"` 引号（M14）。
- **产品**：ddlDiff 返回结构化 diff（PR-9 解锁，H1）；逐 PR 解锁表（H2）；PlanNode.category（H3）；缺省 vs 空数组语义（M1）；字段级 null 口径（M2）；capability 串登记（M3）；分区子句 pg_get_partkeydef（M4）。
- **运维**：REQ-05 配额逐调用点接线，explain/stitcher 显式接线（H1）；relkind 路由矩阵静态钉死（H2）；Deny 模式租户检查剥前缀（M3）；verbatim 字段拒 `;`（M4）；发布前真机跑 env-gated 腿（M5）；快照撕裂 Tx 毒化文档化（M6）；MAX_ROWS 截断改结构化报错（L7）；dbx 能力探测条款（L8）。
- **驳回/缓议**：跨方言类型等价类（独立工作项，PR-9 跨库场景暂划掉）；UNLOGGED/存储参数等 PG 细节进「不还原清单」不做；ddlDiff debug 审计日志（执行即审计，零成本优先级）。
