# db 结构内省增强（L2）——把 `listTables` 从「表名级」扩到列 / 索引 / 外键

> 状态：**v2（2026-10-28，四路评审后修订）**。目标版本 v0.1.90（宿主侧，不动 FFI → `ABI_VERSION` 保持 12，无插件 bump）。
> v1 → v2：吸收工程师 / 架构师 / 产品 / 安全四路评审，逐项处置见 §0。
> 上一期：L1 db 访问控制核心（v0.1.89，已提交），见 `docs/plans/2026-10-27-db-access-control-core.md`。

## 0. 四路评审记录与裁决（v1 → v2）

| 评审方 | 结论 | 一句话 |
|---|---|---|
| 工程师 | 调整后通过 | 挂载点齐全，但 PG `indkey` 是 int2vector（`unnest` 直接编译失败）、PG 两条 SQL 漏 schema 范围、`listTables` PG 分支本身漏 `::text`、mysql FK 会把 PRIMARY/UNIQUE 当外键、测试有三处假绿/挂死形态 |
| 架构师 | 调整后通过 | 方向对，但内省与 `SchemaRegistry` 形状重叠易被误读、运行期与 CLI 两份方言知识必须抽公共层、`type` 应取「声明文本」而非裸 `data_type`、方言分支应改成查表以给 L3 留口 |
| 产品 | 调整后通过 | 返回结构有半截契约（`origin` §3.1 与 §6.3 自相矛盾、FK `name` 为 null 无稳定 key、type 不归一化）、「空数组」造假空、三方法调用成本、`OJ_DB_UNSUPPORTED` 需**调用前**可判 |
| 安全 | 调整后通过 | §3.4 授权提案不够（须连 `listTables` 一起收紧、别名须用 `bound_db` 后物理名）、内省范围可被 `USE`/`SET search_path`/`ATTACH` 改写、PG `pg_*` 漏 namespace、表名需限长拒控制字符、内省无行数上限、日志需强制脱敏 |

**裁决（评审方冲突，主持人定）**

1. **聚合 vs 三个 op**（产品「补 `describeTable` 聚合」 vs 架构师「三 op」）→ **取架构师**。
   `OJ_DB_UNSUPPORTED` 是 **per-capability** 粒度，聚合会逼出「部分成功」的半截语义
   （FK 不支持的方言上只想看列也整体失败）；且与 dbx `get_columns`/`list_indexes`/
   `list_foreign_keys` 同粒度。前端展开表节点用 `Promise.all` 并发（文档给配方）。
   产品的调用成本顾虑登记 T8，等真有前端反馈再定。
2. **内省范围冻结**（安全 B2 要求装配期冻结 db/schema）→ **部分采纳，主体改判分类**。
   闭合手段改为：① `USE` / `SET search_path` / `set_config('search_path',…)` / `ATTACH` /
   `DETACH` 由 ReadOnly/Unknown **改判 Write**（池化连接上这些是**会话级**的，`USE` 后会污染
   同连接的后续请求——这是既有敞口，顺带补）；② sqlite 拒绝含 `.` 的表名（挡 `other.secrets`）。
   **不做装配期冻结**，理由：冻结需要从 DSN 解析 schema 并打破多 schema 部署
   （PG `search_path ≠ public` 的站点会内省不到表），且 off/warn 档本就无防护（裸 SQL 可任意写），
   冻结不增加实际强度；deny 档在①之后已无法切换会话。装配期冻结登记 **T9**，届时一并评估。
3. **公共层迁移范围**（架构师 A2 要求 CLI 一并迁移）→ **采纳抽层，CLI 只做口径统一**。
   新建 `src/bridge/introspect.rs`（**只放 SQL 常量 + 方言选择 + 上限常量，不含执行**，
   运行期绑定参数、CLI 内联两种用法都可复用）；本期 CLI 只把 `'public'` 改为 `current_schema()`
   （一行，解决 T5 漂移），完整迁移登记 **T10**——同批迁移会把风险面从「新增能力」扩到
   「改动运维通道 `oj schema`」，不划算。
4. **`type` 取值**（架构师 A3）→ 采纳「声明文本」：sqlite `type`、mysql `column_type`、
   pg `format_type(atttypid, atttypmod)`。PG 列内省因此从 `information_schema.columns`
   改走 `pg_catalog`（与索引/外键同源，`format_type` 与 `::text` 一并解决）。
5. **`SchemaRegistry` 边界**（架构师 A1）→ 采纳字段名 `nativeType`（不用 `type`，
   避免与 registry 的 `ColumnType` 混淆）+ 文档红线「内省结果**不是**标识符白名单，
   禁止把列名回填构造器/裸 SQL」（构造器的标识符只认 `SchemaRegistry`）。

**待做登记（不在 L2 夹带）**

- T8：是否补 `db.describeTable()` 聚合 op（等产品有前端反馈）。
- T9：内省范围装配期冻结（db/schema 从 DSN 解析，需评估多 schema 部署）。
- T10：CLI `oj/src/schema.rs` 迁移到 `introspect.rs`（顺带消掉 `is_ident` 内联的两套形态）。
- T11：视图 / 数据类型字典 / 触发器 / CHECK（`db.listViews` 等），下游真需要时立项。

## 1. 目标与非目标

**目标**

1. **列**：`db.listColumns(table)` → `{name, nativeType, nullable, default, primaryKey}[]`。
2. **索引**：`db.listIndexes(table)` → `{name, unique, primary, origin, columns}[]`。
3. **外键**：`db.listForeignKeys(table)` → `{name, columns, refTable, refColumns}[]`。
4. 三方言语（sqlite / mysql / postgres）**同口径**：字段名与排序一致，业务不分方言。
5. 表名**一律绑定参数**，零标识符内联。
6. 授权：**只过授权维**（`granted_only`），`listTables` 一并收紧（安全 B1）。
7. 非已知方言 → 结构化 `OJ_DB_UNSUPPORTED`（`data.capability` 点名 `list_columns` / `list_indexes` / `list_foreign_keys`）。

**非目标（显式不做，不留假入口）**

- 视图 / 序列 / 触发器 / CHECK / 注释 / 数据类型字典 → T11。
- DDL 还原与结构对比：CLI `oj schema diff` 已有，不平移进运行期。
- 跨库（PG catalog / SQL Server linked server）：oj 无跨库执行面。
- 动态数据源登记（client 轴）：L3。读侧类型解码（D10-a）：独立立项。
- **类型归一化**：`nativeType` 是方言原始声明文本，不做跨方言统一（归一化 = 半套 DDL 还原，越界）。

**适用边界**：三个 op 的 SQL 由**宿主生成**（引擎常量 + 绑定参数），非 JS 提交的 SQL；
内省结果与 `SchemaRegistry`（声明式白名单）**互补而非等价**（§3.1 红线）。

## 2. 现状事实（v0.1.89 实码核对 + 评审补充）

| 事实 | 位置 |
|---|---|
| `op_db_list_tables(prefix?)`：三方言分支、引擎常量 SQL、前缀 `escape_like` 后绑定 | `db.rs:1089-1160` |
| **PG 分支漏 `::text`**：`information_schema.tables.table_name` 是 `name` 域，sqlx Any 驱动必炸，且「空库不触发、建表后才崩」 | `db.rs:1111`；反例 `oj/src/schema.rs:361-372` |
| 方言门控 `require_known_dialect` → 未知方言 unsupported，不回落 sqlite | `db.rs:65-73`、门控点 `db.rs:1097-1100` |
| `op_db_*` 白名单 19 项全等断言 | `db.rs:1334-1375` |
| op 注册表 / JS 装配 / tx 对象 | `mod.rs:429-447`；`bootstrap.js:39`、`969`、`1044` |
| CLI 列/索引内省 SQL，表名 `is_ident` 后**内联**；PG 用 `'public'` | `oj/src/schema.rs:387-417`、`381` |
| 运行期用 `current_schema()` / `DATABASE()` | `db.rs:1107-1113`（T5 漂移，本期统一 CLI 侧） |
| 事务内路径必须走 `resolve_target` → `Target::Tx`（不能照 `op_db_tx_begin` 直连池） | `db.rs:313`、`1094` |
| 未知方言测试载具：`InMemoryAccessor::dialect_info()` 是 trait 默认且无注入字段 → 需自建包装 accessor | `db.rs:138`；范式 `mod.rs:3657` |
| 真库 env-gated 用例本 crate 有先例 | `db.rs:1494`、`1542` |
| sqlite 池 `max_connections(1)`：事务内误走池路径会**挂死**而非报错 → 用例套 timeout | `db.rs:2326`、`2341-2364` |
| L1 铁律：`db_access` 只约束 JS 提交的 SQL | L1 §1（**本期按 §3.4 收紧授权维**） |

**实测前置结论（本机 SQLite 3.54，评审复测通过）**

- `SELECT name FROM pragma_table_info(?)` 可绑定参数 ✅
- `SELECT … FROM pragma_index_list(?) il JOIN pragma_index_info(il.name) ii` 关联可用 ✅（写 `CROSS JOIN` 防计划器换序）
- ⚠️ 项目内 `libsqlite3-sys` 版本需在真驱动上复测（见 §4 测试 2）。

## 3. 设计

### 3.1 JS API 形态

```ts
db.listColumns(table): Promise<Column[]>
db.listIndexes(table): Promise<Index[]>
db.listForeignKeys(table): Promise<ForeignKey[]>
// db.tx 回调对象同签名（搭车事务连接）

interface Column     { name: string; nativeType: string; nullable: boolean;
                       default: string | null; primaryKey: boolean }
interface Index      { name: string; unique: boolean; primary: boolean;
                       origin: "c" | "u" | "pk"; columns: string[] }
interface ForeignKey { name: string | null; columns: string[];
                       refTable: string; refColumns: string[] }
```

- **`nativeType`**（不叫 `type`）：方言**声明文本**——sqlite `type`、mysql `column_type`、
  pg `format_type(atttypid, atttypmod)`。仍是原始文本，零归一化；文档给三方言对照表
  （含 pg `ARRAY` / `character varying` 等差异）。
- **`origin` 三方言统一**（消除产品 P1 的方言分支）：sqlite 直取 `pragma_index_list.origin`；
  mysql：`index_name='PRIMARY'`→`pk`，否则 `non_unique=0`→`u`，其余 `c`；
  pg：`indisprimary`→`pk`，`indisunique`→`u`，其余 `c`。
  sqlite 的 `sqlite_autoindex_*`（`origin='u'`）**照实返回**，由 UI 自决是否过滤。
- **`primaryKey` 以 `listColumns` 为准**：sqlite 的 rowid 表在 `listIndexes` 里可能没有索引项
  （文档写死这条，避免 UI 拿索引推 PK）。
- **`ForeignKey.name` 可为 `null`**（sqlite 外键无约束名）→ 文档给稳定 key 配方
  （`columns.join(",") + "->" + refTable`），不新增字段避免半截契约。
- 复合外键：`columns[]` 与 `refColumns[]` **按位配对**（mysql 用 `position_in_unique_constraint`
  定序，pg 用 `conkey`/`confkey` 的 ordinality 配对），文档写死。
- **排序**：列按声明序（`cid` / `attnum` / `ordinal_position`）；索引按名再按列序；外键按约束名再按列序。
  `ORDER BY` 必须写进 SQL，不依赖 JOIN 顺序。
- **表不存在 / 看不到 → 空数组**。文档写死：「空数组 = 当前账号在当前 schema 下看不到该表的列」，
  **不区分**「不存在 / 不可见 / 在别的 schema」；要校验表名先用 `db.listTables(prefix)`（对齐 PIP-0003 总则 4）。
- **表名入参校验**：长度 > 128 或含 NUL / 控制字符 → 空数组（不让驱动把输入错误变成 500）；
  sqlite 侧表名含 `.` → 空数组（挡 `other.secrets` 形式的跨库内省）。

**红线（文档加粗）**：内省结果是**目录读**，不是标识符白名单；
`SchemaRegistry` 才是构造器的唯一标识符来源（`db.table()` 对未登记表报 `unknown table`）。
禁止把内省列名回填构造器或拼进裸 SQL。

### 3.2 新模块 `src/bridge/introspect.rs`

只放：**三方言 SQL 常量 + 方言选择 + 上限常量**，不含执行。形态按「CLI 可复用」设计（T10）。

```rust
pub const MAX_ROWS: u32 = 1000;                 // 每 op 结果硬上限（安全 B5）
pub enum Introspect { Columns, Indexes, ForeignKeys }
pub fn sql(kind: Introspect, d: Dialect) -> &'static str;   // 引擎常量，参数位固定 `?`
```

SQL 要点（评审修正后）：

| 能力 | sqlite | mysql | postgres |
|---|---|---|---|
| 列 | `SELECT name, type, "notnull", dflt_value, pk FROM pragma_table_info(?) ORDER BY cid LIMIT ?` | `information_schema.columns`：`column_name, column_type, is_nullable, column_default, column_key='PRI'`；`table_schema = DATABASE() AND table_name = ?` | **`pg_catalog`**：`a.attname::text, format_type(a.atttypid, a.atttypmod), NOT a.attnotnull, pg_get_expr(d.adbin,d.adrelid)`，PK 由 `pg_index.indisprimary` 判；`c.relname = ? AND n.nspname = current_schema()` |
| 索引 | `pragma_index_list(?) il CROSS JOIN pragma_index_info(il.name) ii`（`origin`：`c`/`u`/`pk`） | `information_schema.statistics`：`index_name, non_unique, column_name, seq_in_index`；`PRIMARY` 派生 `pk` | `pg_class`+`pg_index`+`pg_attribute`+`unnest(i.indkey::int2[]) WITH ORDINALITY`（**`indkey` 是 int2vector，必须显式转 `int2[]`**）；`n.nspname = current_schema()`；`indisunique/indisprimary` |
| 外键 | `pragma_foreign_key_list(?)`：`id, "table", "from", "to"`，按 `id` 分组、`seq` 定序；`name = null` | `information_schema.key_column_usage`：**必须 `referenced_table_name IS NOT NULL`**（否则 PRIMARY/UNIQUE 会被当外键）；复合外键按 `position_in_unique_constraint` 配对 | `pg_constraint` + 双向 `unnest(conkey/confkey) WITH ORDINALITY` **按位配对**；`n.nspname = current_schema()` |

统一约束：

1. **零标识符内联**：表名只经绑定参数。禁止「绑定失败即内联」兜底——若某方言/驱动不支持绑定，
   报 `OJ_DB_UNSUPPORTED`（`data.capability` 点名），不降级。
2. **PG 一律 `::text`**：`pg_*` 与 `information_schema.*` 的标识符列是 `name`（sql_identifier 域），
   sqlx Any 驱动解不了该 OID，且「空库不触发、建表后才崩」（`oj/src/schema.rs:361`）。
   **新增静态守卫单测**：扫 `introspect.rs` 的 PG 常量，断言标识符类列引用必带 `::text`
   （CI 无 PG，这是唯一离线防线）。
3. **PG/索引与外键必须带 namespace 过滤**（`n.nspname = current_schema()`）：
   `pg_index` / `pg_constraint` 无 schema 列，只按 relname 关联会吐出其他 schema 的同名对象。
4. **schema 口径**：运行期一律 `current_schema()` / `DATABASE()`；CLI `oj/src/schema.rs:381`
   的 `'public'` 本期改为 `current_schema()`（T5 收口）。
5. **每 op 带 `LIMIT ?`**（上限 `MAX_ROWS=1000`）闭合安全 B5；SQL 侧天然截断，无需额外承载位。
6. 新 op **不用 LIKE**（表名是等值匹配），与 `listTables` 的前缀匹配区分；
   断言「SQL 常量不含 LIKE 且 params.len() == 2（表名 + limit）」。

### 3.3 方言门控与错误

复用 `require_known_dialect(info, capability)`，`capability` 分别为
`list_columns` / `list_indexes` / `list_foreign_keys`。未知方言 → `OJ_DB_UNSUPPORTED`，
`e.data.capability` 点名能力、`e.data.dialect` 点名方言名，绝不回落 sqlite。
**调用前可判**（产品 P4）：文档给配方 `db.dialect()` 白名单 + `plugins()` 能力位；
三个新 `capability` 值必须进 api-manual 闭集表与 db-guide 方言矩阵。

### 3.4 授权（安全 B1，全采纳）

新增 `db_policy::granted_only(state, alias) -> Result<(), BridgeError>`：

- **只判授权维**（四维并集），**不分类风险、不套 `read_only` / `allow_ddl`**（内省恒只读，
  read_only 库也要能看结构，否则 UI 连展开都做不到）。
- **别名用 `guard::bound_db` 后的物理名**（`db.rs:696` 同款），否则 `manifest.db` 把字面
  `"default"` 重定向到 `analytics` 的站点会被整条绕过。
- **三个新 op 与 `op_db_list_tables` 一起过这道**（安全要求；`db.allowed()` 已按 granted 过滤，
  否则它的可信度是假的）。
- `db.asSystem()` **不豁免**（沿用 `evaluate` 不读 `rs.system` 的口径，补内省版行为测试）；
  `db.asTenant()` 不影响（`identity()` 匿名即 `tenant=None`）。
- `enforce: warn` 档 → `deny_log("warn")` 后**放行**，不 fail-closed 报错；`deny` → 拒。
- **行为变更**：默认 `off` 档零影响；`deny` + 声明了 `allow` 的站点，未授权主体不再能枚举表结构。
  CHANGELOG 与升级说明点名。

### 3.5 分类补丁（安全 B2 处置①）

`db_policy` 的词法中，以下会话级语句由 ReadOnly / Unknown **改判 Write**：

- `USE <db>`（mysql）
- `SET search_path = …`、`SELECT set_config('search_path', …)`（pg）
- `ATTACH DATABASE …` / `DETACH`（sqlite）

理由：池化连接上这些改动是**会话级**的，会污染同连接的后续请求（既有敞口）；
判 Write 后 `deny` + `read_only` 站点即被挡住。属**行为变更**（原 ReadOnly 放行），CHANGELOG 点名。

### 3.6 观测

三个 op 走 `normalize_and_trace` + `trace_sql`，进 `SqlEvent` 与每请求画像（与 `listTables` 同路径）。
**强制脱敏**（安全 B6）：调用 trace 时把 params 记为脱敏形态（`<N params>`），
**不因 `redact=false` 而把表名写进日志**；文档「不承诺」表补一行。

### 3.7 挂载点与同步清单

1. `src/bridge/introspect.rs`（新）：SQL 常量 + 方言选择 + `MAX_ROWS`。
2. `src/bridge/db.rs`：三个 op（紧邻 `op_db_list_tables`，共用 `resolve_target` /
   `require_known_dialect` / `normalize_and_trace` 范式）；顺带修 `op_db_list_tables` 的 PG `::text`。
3. `src/bridge/mod.rs:429-447`：注册 3 项。
4. `src/bridge/bootstrap.js`：import 表 + `DB(name)` 实例 + `tx` 回调对象（须 7-bit ASCII）。
5. 白名单断言 `db.rs:1334` **19 → 22**，分别写理由：三 op 是「平台自生成 SQL + 全绑定参数」，
   与 `op_db_allowed`（不执行 SQL）**不同类**，理由分开写。
6. `sample/global.d.ts`（三个方法 + 三个 interface）→ `cargo xtask build` 同步 `bin/devkit/global.d.ts`。
7. `oj/src/schema.rs:381`：`'public'` → `current_schema()`（T5）。
8. 文档：`docs/db-guide.md`（内省章 + 三方言差异对照表 + 报错速查 + 红线）、
   devkit 四件（`api-manual.md` 三方法行 + 类型表 + capability 闭集 + 已知限制 + 陷阱；
   `scenarios.md` 一条端到端；`SKILL.md` 陷阱；`README.md`）、`docs/api/06-db.md`（若含内省条目）。
9. `CHANGELOG.md` + `oj/Cargo.toml` → v0.1.90 + `Cargo.lock`。

## 4. 测试清单

1. **sqlite 行为测试**（离线唯一可跑方言）：`t(id INTEGER PRIMARY KEY, name TEXT)` +
   `u(name)` 索引 + 指向父表外键 + `UNIQUE` 约束（验证 `sqlite_autoindex_*` 的 `origin='u'`），
   断言三 op 的字段、顺序、布尔位、`default` / `name` 为空的形态。
2. **正对照（必写，防假绿）**：每条对抗性用例**先建真表断言非空**，再跑异常输入——
   否则「空数组」与「SQL 没跑 / 方言分支走错」不可区分。`%` / `_` 表名要**正向断言能查到**
   （只钉「不泄漏」的话 LIKE 泄漏的变异体不死）。
3. **对抗性探测（安全方样本全进单测）**：
   - 表名：`t'; DROP TABLE x --`、`t"x`、`` t`x ``、`t/*c*/`、`t%`、`t_`、`%`、`_`、`a'b`、`a\b`、
     `main.t`、`other.secrets`、`public.users`、`[t]`、`t\u0000`、128 / 129 字符、`""`、`" t "`、未登记表；
   - sqlite：`ATTACH DATABASE ':memory:' AS other` → `listColumns("other.t")` → 空数组；
   - mysql：`db.exec("USE otherdb")` → `listColumns` 仍在原库范围（真库 env 用例）；
   - pg：`select set_config('search_path','payroll',false)` 后内省（真库 env 用例）。
4. **分类补丁**：`USE x` / `SET search_path = x` / `set_config('search_path',…)` / `ATTACH …`
   → `Risk::Write`（单测）；`deny` + `read_only` 站点下 `db.exec("USE x")` 被拒（行为测试）。
5. **真库对账**（env-gated `OJ_TEST_PG` / `OJ_TEST_MYSQL`，照 `db.rs:1494` 先例）：
   同一 fixture 三方言字段名/排序一致；**必须先建表再跑**（空库不触发解码，`::text` 漏了也不炸）；
   验证 `indkey::int2[]` 与 namespace 过滤在真 PG 上成立。
6. **方言门控**：自建包装 accessor（照 `mod.rs:3657` 的 `DdlCounting` 范式）覆盖
   `dialect_info()` 造未知方言 → `OJ_DB_UNSUPPORTED` + `data.capability` 点名。
7. **事务内**：`db.tx` 内三个 op 走事务连接（结果与池路径一致）；整条 `run_with` 套
   `tokio::time::timeout(15s, …).expect(…)`（sqlite 池 `max_connections(1)`，走错路径会挂死）。
8. **授权**：`deny` + `allow.roles:[admin]` → 无身份 / 非 admin → `db_not_granted`（三个 op +
   `listTables`）；admin 放行；`asSystem()` 不豁免；`off` 档完全透明；`warn` 档放行且落
   `DB_ACCESS_DENY`；物理别名（manifest 绑 `analytics`）场景不被绕过。
9. **静态守卫**：PG 常量的 `::text` 扫描；SQL 常量不含 LIKE、params 个数固定。
10. **白名单守护**：`raw_sql_op_exits_are_pinned` 同步到 22 项。

## 5. 交付件

- 代码：`src/bridge/introspect.rs`（新）、`src/bridge/db.rs`（三 op + PG `::text` 修复）、
  `src/bridge/mod.rs`、`src/bridge/bootstrap.js`、`src/bridge/db_policy.rs`（`granted_only` + 分类补丁）、
  `oj/src/schema.rs`（T5）、`sample/global.d.ts`。
- 文档：`docs/db-guide.md` 内省章 + 三方言对照表 + 报错速查 + 红线；devkit 四件同步；
  `CHANGELOG.md` + v0.1.90 + `Cargo.lock`。
- 门禁：`cargo fmt --check`、`cargo clippy --release --workspace --all-targets -- -D warnings`、
  `cargo test --release --workspace`（真库验收须显式带 `OJ_TEST_PG` / `OJ_TEST_MYSQL`）。
- 插件与 ABI 不动 → 无插件版本 bump。

## 6. 风险与开放问题

1. **`information_schema` 只返回当前账号有权限的对象** → 结果是「该连接账号可见的结构」，
   不是「库的完整结构」。文档加粗声明。
2. **PG 无运行期 schema 切换口**：多 schema 库只能内省 `current_schema()` 那一个（dbx 的
   P0.5-b 分层树仍需包侧壳）——本设计不解决，登记 T9/T10。
3. **sqlite `ATTACH`**：本期以「拒绝含 `.` 的表名 + `ATTACH` 判 Write」两道收口；
   `ATTACH` 后同名表（不带点前缀）仍可被内省——属池化连接的既有敞口，文档声明。
4. **`nativeType` 不做归一化**：pg 对数组/域返回 `ARRAY` / `character varying(32)`，
   mysql `column_type` 带长度，sqlite 是声明原文。文档给对照表，前端自行分类。
5. **`DATABASE()` 无默认库时返回 NULL** → 结果是空数组而非「无表」，写进文档。
6. 开放问题：`ForeignKey` 是否补 `onDelete` / `onUpdate`？本阶段不补（下游未提，
   且 mysql/pg/sqlite 三方言取值口径差异大）——登记待做。
7. 开放问题：sqlite 侧 `to` 为 NULL（`REFERENCES p` 不指定列）时 `refColumns` 取什么？
   本阶段取 `null`（数组内元素为 null）→ 改为：该外键**整条跳过并落 warn 日志**更干净，实施时定。
