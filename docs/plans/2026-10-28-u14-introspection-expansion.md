# U-14 元数据目录内省扩展（三批次）设计稿

> 状态：**设计稿（三路专家评审完成、处置落稿——处置表见 §8；批次已按「先树后详情」重切）**。
> 上游：`docs/prds/oj-feature-requirements.md` §5 `U-14`（知识源：dbx 仓
> `crates/dbx-tauri-schema/commands.rs` 68 条内省命令）。
> 相邻：U-11（server_version——golden 快照定位键，**同车 v0.1.100**）；L2（v0.1.90）；
> L6（v0.1.94，插件 typed 池读侧解码——行形状依赖它）。
> 评审：架构路 3 条 / 工程路 4 条 / 产品路 6 条，共 13 条；采纳 12、部分采纳 1、未采纳 0。

## 0. 范围（三批次，评审重切：**先树后详情**）

**宿主侧实现**（与 L2 同构：`introspect.rs` 方言 SQL 常量 + 既有 `DataAccessor` 通路），
**ABI 不动**（12 保持；全部是参数化查询 + 行归一，不碰 vtable）。
信任边界（架构路成文）：宿主内省 SQL **直连 accessor**（`db.rs:1374`），不经业务语句守卫
（`guard::check_raw`/`check_tenant_raw`）——新 SQL 常量**只进 `introspect.rs`**，那是唯一
该被审计的出口。

| 批次 | 内容 | 新 Kind（`introspect.rs`） |
|---|---|---|
| **a 连接树 + 对象详情（v0.1.100，与 U-11 同车）** | 连接树二级节点刚需：**views 清单+定义**（`pg_get_viewdef` / `sqlite_master`）、**routines 清单**（functions/procedures）、**sequences**（pg `pg_sequences`、mysql auto_increment 说明、sqlite 模拟）、对象 DDL/源码（`show create table/view/procedure…` / `pg_get_indexdef`）、列/表注释、constraints（PK/UNIQUE/CHECK/FK 全量，独立于既有 FK）、行数统计（**v1 只做估算**：`information_schema`/`pg_class.reltuples`；`precise` 显式开关不做——产品路裁决：点一下行数不应触发全表扫描，精确数业务 handler 自己 `count`） | `views` / `routines` / `sequences` / `ddl` / `comment` / `constraints` / `stats` |
| **b 管理对象（v0.1.101）** | triggers、partitions、types；sqlite 的「模拟」项（产品路确认：`sqlite_master.sql` 文本解析是宿主侧启发式，**非 catalog 语义**，矩阵标注 `simulated: true`，工时单列） | `triggers` / `partitions` / `types` |
| **c 生态（评审裁决：默认不排期）** | 仅**原生作业**（pg_cron / mysql events）保留按需；catalogs（租户存在性泄漏面，web 版由方案包登记别名、本无全库清单语义）、补全对象清单（listTables/listColumns 已覆盖）、linked servers（mssql 显式不做）——**砍** | `jobs`（按需） |

**不做**：

- 写操作——内省**只读**；管理写走业务 handler。
- 超出三方言的命令矩阵（开放方言：`unsupported`，不静默回落——有命名测试锚）。
- mssql/oracle/达梦（PRD §7 显式不做）。
- ABI 扩展内省 vtable 槽——宿主侧够用（L2/L6 先例）；若评审后认为必须插件侧实现才立 ABI 13。

## 1. 现状（file:line）

- 宿主内省：`src/bridge/introspect.rs`（`Kind` 枚举 + 方言 SQL 常量 + `MAX_ROWS`）；
  `db.rs:1330`（`valid_introspect_table`）、`db.rs:1351`（`introspect` 走 accessor，
  `db.rs:1374` 直接 `query_with_params`，**不经业务守卫**）。
- op 面：`op_db_list_tables/list_columns/list_indexes/list_foreign_keys`，JS
  `db.listTables/listColumns/...`；三方言 shape 已统一（L2，**裸数组返回**——见 §2 决策）。
- 授权：内省 op 过 `db_access` 同链；`granted_only` 贯穿。
- 行形状：插件 typed 池读侧解码（L6）；宿主 sqlite 同口径（`::text`，`db.rs:1253`）。

## 2. 归一 shape（评审焦点：三方言一致性 + 两代约定）

```jsonc
// 新 kind 统一信封：{ kind, dialect, items: [...] }，行数受 MAX_ROWS 封顶
{ "kind": "ddl",      "items": [{ "name": "t", "type": "table", "ddl": "CREATE TABLE …" }] }
{ "kind": "constraints", "items": [{ "table": "t", "name": "…", "type": "pk|uq|ck|fk",
                                     "columns": [...], "refTable": null, "refColumns": null,
                                     "definition": "CHECK (…)" }] }
{ "kind": "stats",    "items": [{ "name": "t", "rowsEstimate": 1234, "sizeBytes": 40960 }] }
```

- **两代响应约定裁决（架构路）**：L2 既有三件（listColumns/Indexes/FK）**保持裸数组不动**
  （兼容，handler 已在消费）；**新 kind 一律用 `{kind, dialect, items}` 信封**——不回改旧件，
  约定写入 db-guide。
- mysql：`information_schema` + `show create`；pg：`pg_catalog`；sqlite：catalog 缺项走
  `sqlite_master` 模拟（`simulated: true`）或 `unsupported`。
- **能力矩阵表（三方言 × 全 kind 的 ✅/模拟/❌）随 a 批首发**（产品路：sqlite 在 a 批就有
  comment/stats 缺口，矩阵不能等 b 批）。
- 排序与可空口径与 L2 同规则。

## 3. 安全（评审重点）

1. **DDL/源码回显**：过 `secret::redact`（仅当文本真含凭据 URL 形态）+ 文档声明。
2. **`granted_only` 贯穿**：所有新 op 复用 `check_access` 同链（未授权别名 `db_not_granted`）；
   不新增特权面。
3. 只读保证：批次内全部是 `select`/`show` 形态；信任边界 = 宿主（§0），常量只进
   `introspect.rs`。
4. `catalogs` 砍单后，租户存在性泄漏面消除。

## 4. 实现要点（评审改写：两个真坑）

原稿「show create 过 tenant 守卫误伤」的机制定位**是错的**（三路核实：内省通路不经业务
守卫），真正的坑与处置：

1. **mysql `SHOW CREATE TABLE/VIEW/PROCEDURE` 不支持绑定参数**（`SHOW CREATE TABLE ?` 非法）
   → 标识符必然内联，违反 `introspect.rs` 铁律 1「零标识符内联」。处置：**成文例外**写进
   模块注释——内省标识符经**严格白名单字符集校验**（同 `valid_introspect_table` 口径）+
   反引号引用（内嵌反引号翻倍转义）；例外只限 SHOW 族。
2. **共用通路统一追加 ` LIMIT 1000`**（`db.rs:1377` 附近），SHOW 语法不能接 LIMIT →
   `Kind` 增「可否追加 LIMIT」标志位，`introspect()` 据此分支。
3. **静态守卫（工程路）**：introspect.rs 既有测试四件（`::text` 钉死、无 LIKE、参数位计数、
   `current_schema()` 限定）必须覆盖**全部新 SQL 常量**——新 pg catalog 投影（`tgname`/
   `viewname`/`conname`/`seqname`…）同为 `name` 域，「空库不触发、建表后才崩」的雷同一颗。
   golden（env-gated）只是第二道。

## 5. 分期与验收

| 批 | 估 | 验收 |
|---|---|---|
| a（v0.1.100，与 U-11 同车） | **3 天**（评审上调） | **每 kind × 每方言各 1 条 golden 断言（含 unsupported 显式断言——unsupported 也算一条用例）**；开放方言调任一新 kind → 结构化 unsupported 信封（命名用例）；静态守卫四件扩覆盖；devkit handler `.schema` 出参样例 + scenarios 场景 51 **随 a 批首发** |
| b（v0.1.101） | 3 天（sqlite 模拟工时单列） | 同上口径 + 能力矩阵补全 b 批行 |
| c | 默认不排期 | 仅原生作业按需另立 |

- golden 快照：真库录制、含 `serverVersion` 定位键（U-11 产出）；CI env-gated skip-as-pass。
- 每批次独立发版；「每批可独立发版」声明以 a 批 3 天口径保真（工程路：不拆 a1/a2，一次交付
  7 kind 但估时如实）。

## 6. 文档面

db-guide §10（命令矩阵 + 两代响应约定）、api-manual §6/§13、scenarios 场景 51（a 批首发）、
SKILL、README；`global.d.ts` 全部新方法类型。

## 7. 依赖与风险

- U-11 同车（golden 定位键）。
- L6 typed 池解码已具备；新命令撞未解码类型（如 pg `regclass`）按 L6 口径响亮报错。
- mysql/pg catalog 列的小版本差异（8.0 vs 5.7 / pg12+ 分区语法）：声明最低支持版本 +
  golden 锚定（评审问答定案）。

## 8. 评审处置表（三路 13 条：采纳 12、部分采纳 1、未采纳 0）

| # | 来源 | 原意见要点 | 处置 |
|---|---|---|---|
| 1 | 产品 | 批次与浏览器交付顺序相反；views/routines/sequences 提前并入 a | **采纳**（§0 重切：先树后详情） |
| 2 | 产品 | a 批验收「3 命令」不可度量；能力矩阵应随 a 首发 | **采纳**（§5：每 kind×方言 1 条 golden 含 unsupported；矩阵随 a） |
| 3 | 产品 | PRD「未知方言 unsupported」缺命名用例 | **采纳**（§5 命名用例） |
| 4 | 产品 | c 批基本应砍，仅作业保留 | **采纳**（§0：c 默认不排期） |
| 5 | 产品 | `precise` 开关过度设计 | **采纳**（§0 stats 只估算） |
| 6 | 产品 | devkit 样例与场景 51 随 a 首发 | **采纳**（§5） |
| 7 | 工程 | show create 不能绑定参数 + LIMIT 对 SHOW 非法——两个真坑，原风险表前提错 | **采纳**（§4 重写：成文例外 + LIMIT 标志位） |
| 8 | 工程 | 静态守卫四件必须覆盖新常量 | **采纳**（§4.3 + §5） |
| 9 | 工程 | sqlite 模拟=文本解析启发式，工时单列 | **采纳**（§0 b 批 + 模拟标注） |
| 10 | 工程 | a 批 2 天偏紧：改 3 天或拆 a1/a2 | **部分采纳**：估时上调至 3 天；不拆 a1/a2（7 kind 一次交付，保「独立发版」声明真实） |
| 11 | 架构 | 「show create 过守卫误伤」机制错置；信任边界=宿主；删 show/explain 白名单机制 | **采纳**（§0 信任边界成文；与 #7 合并改写 §4） |
| 12 | 架构 | 新信封 vs L2 裸数组两代约定需裁决 | **采纳**（§2：旧件不动、新 kind 用信封） |
| 13 | 架构 | 能力矩阵表排期与 sqlite 缺口矛盾（与 #2 同源） | **采纳**（同 #2） |
