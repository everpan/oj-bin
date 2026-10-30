# 批次 3 设计：REQ-09 剩余（结构变更保真）

> 上游诉求源：`../oj-dbx/docs/upstream/REQ-09-structure-change-coverage.md` §4.1（表改名缺口）/
> §4.2（建表保真）/ §4.3（分区/表选项变更显式报错）/ §4.4（非表对象）+ §5 验收标准。
> 批次 2 已交付 §4.5 schema 定向（v0.1.112）。本稿覆盖余下全部，目标版本 v0.1.113。
> 纯 host 侧（`src/bridge/db_ddl.rs` + `introspect.rs` PG 拼接器），**零 ABI 变更**。

## 1. 意图与为什么

结构编辑器要给真实用户用，光会 rename 列不够：建表丢分区/MySQL 表选项、分区变更静默
no-op、视图/例程/触发器/序列无法建改、表无法改名——任意一条都会让用户「看起来成功了
其实没改」。本批把这几条全部收口到 fail-loud 或保真。

## 2. 目标 / 非目标

**目标**

1. **G1 建表保真**：`Structure` 顶层新增 `partitionBy`（文本）与 `options`（MySQL
   engine/charset/collate/comment）；`from = null` 建表路径原样拼接；与 `catalog("ddl")`
   输出互为逆运算。
2. **G2 分区/表选项变更**：`from != null` 且 partitionBy/options 有差异 → 结构化
   `OJ_DB_UNSUPPORTED`（绝不静默 no-op）。
3. **G3 非表对象**：`objectType` 扩 `"view" | "index" | "sequence" | "routine" | "trigger"`；
   view 支持 create + definition 文本 diff；其余四型支持 create（from=null）+ drop。
4. **G4 表改名**：表级 `previousName` → `ALTER TABLE ... RENAME TO ...`（schema 段两
   段引用），与列 rename 同一套配对/边界语义。

**非目标**

- 逐字段 diff index/sequence/routine/trigger（只 create/drop，显式收窄）。
- 分区迁移的自动 ALTER 生成（G2 只做显式报错）。
- MySQL options 的逐键 alter（同 G2 一档报错）。
- 触发器 definition diff（v0.1.112 catalog 已有清单面；DDL 拼装只给 create/drop）。

**反目标**：静默吞掉任何 partitionBy/options 差异；为 definition 文本做 SQL 解析。

## 3. 契约（JS 可见面）

```jsonc
// G1/G2：Structure 顶层新字段（ddlDiff spec）
{
  "objectType": "table",
  "schema": "public",            // v0.1.112 已有
  "name": "t",
  "from": null,                  // 建表路径
  "to": {
    "columns": [ /* 既有 */ ],
    "partitionBy": "RANGE (d)",  // 文本，原样拼到 CREATE TABLE 的 ) 之后
    "options": {                 // 仅 MySQL；PG 出现 → 参数错误（fail-loud）
      "engine": "InnoDB", "charset": "utf8mb4", "collate": "utf8mb4_bin",
      "comment": "业务表"
    }
  }
}

// G3：objectType 扩展
{"objectType": "view",     "name": "v", "from": null,
 "to": {"definition": "SELECT ...", "columns": [ /* 可选，视图列声明 */ ]}}
{"objectType": "index",    "name": "ix", "from": null,
 "to": {"definition": "CREATE INDEX ix ON t (a, b)"}}        // 文本原样执行
{"objectType": "sequence", "name": "s", "from": null,
 "to": {"definition": "CREATE SEQUENCE s START 100"}}
{"objectType": "routine",  "name": "f", "from": null,
 "to": {"definition": "CREATE FUNCTION ..."}}
{"objectType": "trigger",  "name": "tr", "from": null,
 "to": {"definition": "CREATE TRIGGER ..."}}

// G4：表改名
{"objectType": "table", "name": "t_new", "previousName": "t_old",
 "from": { /* t_old 结构 */ }, "to": { /* t_new 结构 */ }}
```

**diff 输出**：沿用既有信封 `{diff:{columns:{added,removed,changed,renamed},constraints,
indexes,foreignKeys}, statements, warnings}`（builder-check Q2：无顶层 changed/renamed 键，
以下均挂既有结构）：
- G4：`diff.renamed = [{"from":"t_old","to":"t_new"}]`（表级，与 diff.columns 平级；
  `changed[]` 不加 name 条目——表级 name 变化已在 renamed 表达，前端展示口径同列改名）。
- G2：不发 statements，整体结构化 `OJ_DB_UNSUPPORTED`（通道见 §4.3）。
- G3 view diff（from 非 null）：definition 文本不同 → `diff.definition = {"from":...,
  "to":...}` + 单条语句（PG `CREATE OR REPLACE VIEW` / MySQL `ALTER VIEW`）；相同 →
  零语句。其余四型 from 非 null：仅支持整体 drop（to 为 null）或零差异，逐字段 diff
  请求 → 参数错误。

## 4. 设计决策

### 4.1 建表保真（G1）

- `partitionBy` 渲染点：`db_ddl.rs:498` 的 `CREATE TABLE {t} (\n...\n)` 闭合括号后
  追加 ` PARTITION BY {text}`（PG 与 MySQL 同词法；sqlite 出现 → 参数错误）。
- MySQL `options` 渲染（闭合括号后、分号前）：
  `ENGINE={engine} DEFAULT CHARSET={charset} COLLATE={collate} COMMENT='{comment}'`，
  各键可选、按固定序、空串缺省。comment 值单引号内 `'` → `''` 转义（DDL 文本面，与
  既有 constraint definition 同一信任模型：这是 DDL 编排入参，不是查询值；标识符仍全走
  quote_ident）。
- `from != null` 时 to 侧 partitionBy/options 不参与 diff——差异走 G2 报错通道。
- **逆运算判据**：MySQL 天然成立（`catalog("ddl")` 走 SHOW CREATE 原文）。PG 拼接器的
  `PARTITION BY` 段 **v0.1.109 已交付**（`PG_STITCH_PARTKEY`/`pg_stitch`，
  builder-check Q3 核实）——本批对该路径只做逆运算真库验证腿，无新实现。sqlite 无
  分区，不补。

### 4.2 逆运算验收（G1 判据落法）

env-gated 真库腿：建带分区表（PG RANGE + MySQL RANGE）→ `catalog("ddl")` 抓回 →
从抓回的 DDL 构造 structure（partitionBy 段切出）→ `ddlDiff(create)` 重放 → 两版
语句分区段一致。单测层：create 语句串断言（PG/MySQL 各一离线腿）。

### 4.3 分区/表选项变更（G2）

- `from.partitionBy` 与 `to.partitionBy` 任一侧非空且文本不同（或一侧有一侧无）→
  `db_error::unsupported(dialect, "ddl_diff_partition")`。
- `options` 深比较不同 → `db_error::unsupported(dialect, "ddl_diff_table_options")`。
- 检查在列 diff 之前、解析 structure 之后立即做（fail-fast，不出半截 statements）。
- 两侧皆空/皆同 → 零语句正常继续。
- **结构化报错通道**（builder-check：ddl_diff 返回 `Result<Value, String>`，op 统一
  `JsErrorBox::generic` 会丢 capability）——新增 host 侧 helper
  `unsupported_capability_from_spec(d, spec) -> Option<&'static str>`，`ddl_diff` 内部
  检查保留（字符串兜底），op（`op_db_ddl_diff`）在调用前先跑该 helper，命中则直接走
  `db_error::unsupported` → `to_js_err` 结构化通道。两层同判据，无分叉。

### 4.4 非表对象（G3）

- `objectType` 校验从 `"table"` 白名单扩六值；其余字符串照旧参数错误。
- `view` create：`CREATE VIEW {t} AS {definition}`（PG/MySQL 同词法）。
  definition diff：文本严格相等 → 零语句；不等 → 单条（PG `CREATE OR REPLACE VIEW {t}
  AS {definition}`；MySQL `ALTER VIEW {t} AS {definition}`）。
  view drop（to 为 null）→ `DROP VIEW {t}`（builder-check Q4：补对称，不做成参数错误）。
- `index|sequence|routine|trigger` create：`to.definition` 文本原样单条语句。
  drop（`to == null` 且 from 非 null）：`DROP INDEX {t}` / `DROP SEQUENCE {t}` /
  `DROP FUNCTION|PROCEDURE {t}`（routine 需 from 侧 type 判别）/ `DROP TRIGGER {t}`。
  MySQL DROP INDEX 需 `ON 表`——index 形 **drop 路径**需带 `table` 字段（缺省 → 参数
  错误；create 路径不强制，definition 自含 `ON t`，builder-check Q5）。
- PG 重载例程 drop 需签名——routines 信封 v0.1.112 已带 args 文本，drop 语句拼
  `name(args)`；拿不到 args → 参数错误（不猜）。
- **table 的 `to: null`**（builder-check Q4）：参数错误（fail-loud）。DROP TABLE 是
  高危独立语义，不进本批；现状「零语句静默」正是本批要消灭的形态。

### 4.5 表改名（G4）

- spec 顶层 `previousName`（表级）：命中后表名渲染用新名，首条语句
  `ALTER TABLE {schema}.{old} RENAME TO {new}`（PG 词法）；MySQL
  `RENAME TABLE {schema}.{old} TO {schema}.{new}`（8.0 `ALTER TABLE ... RENAME COLUMN`
  是列级；表级 MySQL 用 RENAME TABLE 或 ALTER TABLE RENAME——取后者
  `ALTER TABLE {old} RENAME TO {new}`，与列级同族）。
- 与列 rename 边界一致：from 侧 previousName 严格忽略；columns 内列级 previousName
  与表级互不干扰。
- 逆序固定：表 RENAME 先于一切列/约束/索引语句。

### 4.6 安全红线自查（含 builder-check Q1 的澄清修订）

- 标识符（表/列/约束/索引名）全走 `quote_ident`/白名单，本批不引入新拼接面。
- **check_fragment 信任类澄清**（builder-check Q1）：`check_fragment` 拒 `;`/换行的
  法理是「该片段会被**嵌入**进一条更大的生成语句——分隔符 = 语句分裂注入面」
  （列 default / 约束 definition 属此类，维持原判不动）。G3 的 `to.definition` 与
  G1 的 `partitionBy` 是**整条语句体**（不再嵌入任何生成语句）：语句数由构造保证
  为 1，且重放走 prepared 协议（多语句直接被服务端拒）——无分裂面。故新字段走
  独立校验：拒 NUL、限长（64 KiB），**不拒 `;`/换行**。**这是对 check_fragment
  作用域的澄清修订（非废除），随本稿一并提请批准。**
- definition/partitionBy/options 文本 = DDL 编排入参（raw 透传），信任模型同
  `db.exec` + `allow_ddl` 授权面；值不混入标识符位。
- `bootstrap.js` 本批不动（JSDoc 若补须守 7-bit ASCII）。

## 5. 验收映射（REQ-09 §5 → 本批）

| 上游验收 | 落点 |
|---|---|
| from=null 建分区表 → CREATE 带 PARTITION BY；MySQL 带 ENGINE/CHARSET | §4.1 + 离线单测双腿 |
| ddl(create(structure(t))) 与 catalog("ddl", t) 语义等价 | §4.2 env-gated 逆运算腿 |
| objectType:"view" + from=null 产出完整 CREATE VIEW | §4.4 离线单测 |
| schema 两段引用 | v0.1.112 已交付，本批 G4 复用同渲染 |
| 隐含 rename warnings | v0.1.112 已交付（且本批前已收敛 N×M） |
| 全部有单测 + 真库用例 | 每 G 至少 1 离线 + 相关 G 各 1 env-gated 腿 |

## 6. 交付物清单

- `src/bridge/db_ddl.rs`：G1/G2/G3/G4 全部逻辑 + `unsupported_capability_from_spec`
  helper + 离线单测（每 G ≥2 用例：正向 + 边界/报错）。
- `src/bridge/db_introspect.rs`：op 前置 G2 结构化报错通道（消费上述 helper）。
- PG partkeydef 拼接 **v0.1.109 已交付**——本批只做逆运算 env-gated 验证腿，无新实现。
- `src/bridge/db_tests.rs`：env-gated 逆运算腿 + view create 真库重放腿（PG+MySQL）。
- devkit 四件 + CHANGELOG（v0.1.113）+ `cargo xtask build` 归置。
- 下游 `oj-dbx/docs/upstream/2026-10-30-upstream-gap-inventory.md` §八追加对账行。

## 7. 留给计划阶段的口子

- col_def_sql 的 after/位置 diff 与 G4 表 rename 的交互（改名后列位置参考系）——按
  「表 rename 与列维度无关」处理，不特判。
- routine 的 `DROP FUNCTION` vs `DROP PROCEDURE` 判别键（from.type）缺省行为。
- view columns 声明是否渲染进 CREATE VIEW 列清单（倾向不渲染，definition 全权）。
