# dbx Web 对齐 · L6：U-6 方言解码收口（mysql / pg 插件侧）

> 状态：**设计稿（待四路评审，未实现）**。本文件补完 PRD `U-6`（P0 唯一剩余项）。
> 上游：`docs/prds/oj-feature-requirements.md` §1 U-6；L5 设计
> `docs/plans/2026-10-28-dbx-web-l5-p0-main-journey.md` §1.2（当时把插件侧解码划为
> env-gated follow-up）。

## 0. 范围

**做**：

| 方言 | 新增可读类型（PRD U-6 清单） | 落点 |
|---|---|---|
| mysql | `DECIMAL`/`NEWDECIMAL` → 十进制字符串；`DATE`/`TIME`/`DATETIME`/`TIMESTAMP` → ISO 8601；`JSON` → 文本；`BIT` → 位串文本（**并修复 BIT(n) 现状静默出 bool/数字的错值**）；`YEAR` → number | `plugins/oj-db-mysql/src/lib.rs::column_json_mysql` |
| postgres | `NUMERIC` → 十进制字符串；`DATE`/`TIME`/`TIMESTAMP`/`TIMESTAMPTZ` → ISO 8601；`JSON`/`JSONB` → 文本；`UUID` → 连字符文本；数组（`_int2/_int4/_int8/_float4/_float8/_bool/_text/_varchar/_bpchar/_name/_numeric/_uuid/_date/_timestamp/_timestamptz/_json/_jsonb`）→ **JSON 数组** | `plugins/oj-db-postgres/src/lib.rs`：`Pool<Postgres>` 化（见 §2.0）+ `column_json_pg` |
| postgres | `real`/`float4` 标量一并支持（与数组元素口径对齐；Any 时代它报错，属顺手补齐） | 同上 |

**不做（显式）**：

- `GEOMETRY` / `POINT` 等空间类型（PRD 未要求；保持响亮报错，文案补 `ST_AsText` 指引）。
- 插件内 `Any` 回退路径（只服务离线 sqlite 夹具，不承接生产解码；保持原样）。
- 二进制大对象（LOB/blobs > 现有 `Vec<u8>` 通路）、区间/网络/位串数组等长尾类型。

## 1. 现状（file:line）

- mysql typed 路径：`column_json_mysql`（`plugins/oj-db-mysql/src/lib.rs:221-258`）按
  `u64 → i64 → bool → f64 → String → Vec<u8>` 固定顺序探测；`DECIMAL`/时间/`JSON`/`BIT`/
  `GEOMETRY` 全部落空 → `row_to_json_mysql`（`:185`）报错。
- pg typed 路径：`column_json_pg`（`plugins/oj-db-postgres/src/lib.rs:374-...`）按
  `bool → i16 → i32 → i64 → f64 → String → Vec<u8>` 探测；`NUMERIC`/`JSONB`/`UUID`/数组/时间
  全部落空 → `undecodable_column`（`:284`）报错。
- 两条路径都是 `db.query` 与 `db.stream` 共用（同一 `row_to_json_*`），改一处即两处生效。

## 2. 设计

### 2.0 前置（评审阻断项，必须先做）

**pg 插件的 `query` / `exec` / `tx_*` 走的是 `Pool<Any>`（`plugins/oj-db-postgres/src/lib.rs:200`、
`:655`、`:729`），只有 `db.stream` 用 typed `PgConnection`（`:489`）。** 而 sqlx 的 `Any` 只支持
`bool/i16/i32/i64/f32/f64/String/Vec<u8>`（`sqlx-core-0.9.0/src/any/types/`），
`NUMERIC`/`JSONB`/`UUID`/数组/时间**在 Any 上根本没有解码实现**。因此：

- 只改 `column_json_pg` 等于「只有 stream 生效、query 仍报错」——不达验收；
- 必须把 pg 插件改成与 mysql 同构的 `Pooled { Pg(Pool<Postgres>), Any(Pool<Any>) }` +
  `TxInner`，**production DSN（`postgres://` / `postgresql://`）走 typed**，
  `Any` 仅保留给离线 sqlite 夹具（与 mysql 插件同一纪律）。
- 绑定侧复用已存在的 `bind_value_pg`（`:337`）；`shape_tag`、`reject_u64_markers` 不变。

### 2.1 解码顺序（关键：不能破坏既有六类行为）

保留现有探测链**原顺序与优先级**，其后追加「按类型名分派」的新层：

```
既有六类探测（u64/i64/bool/f64/String/Vec<u8>）
  → 类型名 switch（§2.2）→ 命中即返回
  → 仍未命中 → 现有 undecodable_column 错误
```

**唯一例外（必须前置）**：mysql `BIT`。`bool::compatible` **无条件**包含 `ColumnType::Bit`
（`sqlx-mysql-0.9.0/src/types/bool.rs:22-33`），`u64::compatible` 在 UNSIGNED 时也含 Bit
（`uint.rs:19-29`）——所以现状 `BIT(n>1)` 会被前两条探测吃掉，**静默返回 `true`/数字（错值）**。
故 mysql 的 `BIT` 判定必须在探测链**最前面**按类型名短路（这是 bug 修复，不是行为回归）。
`YEAR` 同理前置（`int.rs` 无 Year 分支，`u16` 只在 UNSIGNED 下兼容）。

### 2.2 类型映射

| 方言类型名（`type_info().name()`） | sqlx 解码类型 | JSON 输出 |
|---|---|---|
| mysql `BIT` | `u64`（sqlx 按大端拼装） | `string`（二进制，无前导零） |
| mysql `YEAR` | `u16` → 回退 `i64` | `number` |
| mysql `DECIMAL`/`NEWDECIMAL` | `sqlx::types::BigDecimal` | `string`（普通十进制，无指数、保留 scale） |
| mysql `DATE` | `chrono::NaiveDate` | `"YYYY-MM-DD"` |
| mysql `TIME` | `chrono::NaiveTime` | `"HH:MM:SS[.frac]"`（**负值/>24h 解不出 → 响亮报错**，MySQL `TIME` 可达 ±838h） |
| mysql `DATETIME` | `chrono::NaiveDateTime` | `"YYYY-MM-DDTHH:MM:SS[.frac]"` |
| mysql `TIMESTAMP` | `chrono::DateTime<Utc>`（**不能用 `NaiveDateTime`**：其 `compatible` 只认 Datetime） | RFC3339 |
| mysql `JSON` | `sqlx::types::Json<serde_json::Value>`（`String` 不兼容 `ColumnType::Json`） | `string`（`to_string()` 文本；原文键序不保证） |
| pg `NUMERIC` | `sqlx::types::BigDecimal` | `string` |
| pg `DATE` | `NaiveDate` | `"YYYY-MM-DD"` |
| pg `TIME` | `NaiveTime` | `"HH:MM:SS[.frac]"` |
| pg `TIMESTAMP` | `NaiveDateTime` | `"YYYY-MM-DDTHH:MM:SS[.frac]"` |
| pg `TIMESTAMPTZ` | `DateTime<Utc>`（回退 `FixedOffset`） | RFC3339（`+00:00`） |
| pg `JSON`/`JSONB` | `Json<serde_json::Value>` | `string`（`to_string()` 压缩文本；jsonb 本无键序） |
| pg `UUID` | `uuid::Uuid` | `string`（小写连字符） |
| pg `FLOAT4` | `f32` | `number`（与 `_float4` 元素口径一致） |
| pg 数组 `_…` | 见 §2.3 | **JSON 数组** |

**不做**：`TIMETZ`（sqlx 只有 `PgTimeTz`，无 chrono 映射；PRD 未点名，维持响亮报错）。
时间格式一律 `%Y-%m-%dT%H:%M:%S%.f`（有小数秒才带），**不做时区换算**；`TIMESTAMPTZ` 归一 UTC。

### 2.3 pg 数组 → JSON 数组

元素类型白名单（其余报错，绝不猜）：

```
_int2 → i16      _int4 → i32      _int8 → i64
_float4 → f32    _float8 → f64    _bool → bool
_text/_varchar/_bpchar/_name → String
_uuid → Uuid     _numeric → BigDecimal
_date → NaiveDate  _timestamp → NaiveDateTime  _timestamptz → DateTime<Utc>
_json/_jsonb → Json<Value>
```

解码用 `Vec<Option<T>>`（**必须带 `Option`**：sqlx 对含 `NULL` 的数组，`Vec<T>` 会在 null 元素处
报错），输出 `[1, 2, null]`。

> 输出选 JSON 数组而非 PG 字面量 `{1,2}`（评审改判）：行传输本就是 JSON，字面量会让每个消费方
> 再实现一套引号/转义解析，且一旦发布即成兼容负担；JSON 数组可 `Array.isArray` 直判、跨方言同形。

### 2.4 错误文案

未覆盖类型继续 `Err`，文案补方言专属 cast 指引：

- mysql：`— select explicit columns and cast it in SQL (e.g. cast(x as char) as x)`
  （空间类型另加 `ST_AsText(x)`）。
- pg：`— cast it in SQL (e.g. cast(x as text) as x)`（数组补「元素类型未支持」）。

### 2.5 依赖

- `sqlx` features：mysql 增 `chrono` / `bigdecimal`；pg 增 `chrono` / `bigdecimal` / `uuid`。
- 新 crate：`bigdecimal`（经 sqlx feature 引入，含 `num-bigint`/`num-traits`）。
- `chrono` 0.4 / `uuid` 1.x 已在 `Cargo.lock`（其它依赖带入），版本无需钉死。

## 3. 兼容与红线

- **既有六类行为逐字节不变**：新增分支只在原探测链**全部落空**时进入。
- **绝不静默回退**：白名单外类型一律 `Err`，不允许 `Null` 或 `to_string()` 兜底。
- **不重写 SQL**：解码只在宿主/插件侧做值转换，不改用户语句。
- **`db.stream` 同步**：共用 `row_to_json_*`，无需额外分支。

## 4. 测试

**离线单测**（不依赖真库）：

- 数组字面量格式化（含 `NULL`、引号/反斜杠转义、空数组）。
- 时间格式化（无小数秒 / 有小数秒 / 带时区）。
- 类型名分派表：未覆盖类型返回 `None`（由调用方报错）。

**env-gated 真库**（`OJ_TEST_MYSQL` / `OJ_TEST_PG` 未设 = skip-as-pass，须显式带 env 跑）：

- mysql：建宽表（`DECIMAL(18,4)`、`DATE`、`TIME(6)`、`DATETIME(6)`、`TIMESTAMP`、`YEAR`、
  `JSON`、`BIT(5)`）→ `select *` 全列可读且形状符合 §2.2；同表 `db.stream` 一致。
- pg：建宽表（`NUMERIC(18,4)`、`DATE`、`TIME`、`TIMESTAMP`、`TIMESTAMPTZ`、`JSONB`、`UUID`、
  `INT4[]`、`TEXT[]`）→ 同上。
- 负例：mysql `GEOMETRY` 仍报错且文案含 `ST_AsText`。

## 5. 文档与版本

**权威落点（评审补全，避免同页两套口径）**：

- `docs/devkit/db-guide.md` §10.6「值契约 / 方言边界表」+ §10.5 能力矩阵：删「DECIMAL/时间/JSON
  一律报错」表述，改成分类型映射表；补「BIT(n) 位串无前导零」「JSON 出文本」「数组出 JSON 数组」
  「DECIMAL 恒为字符串（`row.amount + 1` 会拼接，用 `toDouble()`）」。
- `docs/devkit/api-manual.md`：§6 值契约段落（约 1195-1203 行）、§13 限制表（约 3784 行）。
- `docs/devkit/SKILL.md`：陷阱表把「DECIMAL/时间/JSON 一撞就报错、别用 `select *`」改为
  「现在能读了，但 DECIMAL 是字符串、JSON 是文本、数组是数组」。
- `docs/devkit/scenarios.md`：场景里的 `JSON.parse(row.x)` 旧范式改为直接使用文本/对象。
- `docs/plugins/plugin-development.md` §15 conformance 清单同步。
- `docs/numeric-limits.md` §4.3 的 cast 指引改为「仅 LOB/空间类型仍需 cast」。
- **升级提示**：同一列由「报错」变「可读」属能力新增，不破坏既有可工作代码；但**写侧未跟进**
  （`bind_value` 对 `DECIMAL`/JSON/数组无对称编码，PG 会 42804）→ 必须在 db-guide 明示
  「本版新增类型为**只读**，回写仍需显式 cast」。
- `CHANGELOG.md`：`feat(db)` 条目 + 插件版本段；`oj-db-mysql` / `oj-db-postgres`
  0.1.2 → **0.1.3**（`Cargo.lock` 同步）。
- 真库用例 env-gated：CHANGELOG 明示「三方言未在 CI 齐绿」（sqlite 离线跑，mysql/pg 需
  `OJ_TEST_MYSQL` / `OJ_TEST_PG`）。


## 6. 风险与缓解

| 风险 | 缓解 |
|---|---|
| `bigdecimal` 新增依赖体积/编译时间 | 仅经 sqlx feature 引入；spike 记录 `cargo tree` 增量 |
| 时区语义争议（`TIMESTAMP` vs `TIMESTAMPTZ`） | 无时区类型不做换算，`TIMESTAMPTZ` 归一 UTC，文档写死 |
| `BIT` 宽度不可得（sqlx `max_size` 私有） | 输出无前导零的位串，文档明示；`BIT(1)` 走 bool |
| pg 数组类型爆炸 | 白名单 15 种，其余响亮报错 |
| 真库用例 skip-as-pass 假绿 | 门禁命令显式带 `OJ_TEST_MYSQL` / `OJ_TEST_PG`，报告附实际执行行数 |

## 7. 四路评审处置（2026-10-28）

| # | 评审意见 | 处置 |
|---|---|---|
| E1 / A1 | mysql `BIT` 分支不可达：`bool::compatible` 无条件含 Bit、`u64` 在 UNSIGNED 时也含 Bit → 现状 BIT(n) 已静默返回 bool/数字（错值） | **采纳**：BIT 类型名判定前置到探测链最前（§2.1），并把「现状错值」写进 CHANGELOG 的 `fix(db)` |
| E2 | mysql `JSON` 不能用 `String` 解（`str::compatible` 不含 Json） | **采纳**：`sqlx::types::Json<Value>` |
| E3 / A1 | pg `serde_json::Value` 无 `Type<Postgres>`；`Vec<Value>` 亦不可 | **采纳**：统一 `Json<Value>` / `Vec<Option<Json<Value>>>` |
| E4 | mysql `TIMESTAMP` 不能用 `NaiveDateTime`（compatible 只认 Datetime） | **采纳**：TIMESTAMP → `DateTime<Utc>`，DATETIME → `NaiveDateTime` |
| E5 | pg `TIMETZ` 无 chrono 映射 | **采纳**：移出范围，维持响亮报错 |
| E6 | mysql `YEAR` 的 `i64` 回退是死代码；`u16` 依赖 UNSIGNED | **采纳**：前置类型名分派，`u16` 失败即报错（不猜 i64） |
| E7 | pg 数组含 NULL 需 `Vec<Option<T>>` | **采纳**（§2.3） |
| E8 | 用 `sqlx::types::*` re-export，避免新增直接依赖 | **采纳**：`bigdecimal` 只在 `Cargo.toml` 的 sqlx feature 里出现 |
| E9 | mysql `TIME` 负值/>24h 解不出 | **采纳**：保留响亮报错，文案点明 |
| E10 / A4 | `_float4` 白名单与标量 `real` 报错不一致 | **采纳**：标量 `FLOAT4` 一并支持（§2.2） |
| A2 | pg 数组应输出 JSON 数组而非字面量 | **采纳**（§2.3 改判） |
| A3 | DECIMAL/JSON/数组「只读」，写侧 `bind_value` 不对称（PG 42804） | **采纳**：db-guide 明示「只读」；不在本版做写侧编码（需独立设计） |
| A5 | 文档权威落点漏 §10.6 / api-manual 具体段 | **采纳**（§5 列表） |
| A6 | JSON 建议直出对象 | **不采纳（本版）**：PRD U-6 明确「json/jsonb/uuid/数组 → text」；改形状属契约变更，留待下游确认后单独立项。已在 §2.2 记录 |
| P1 | 缺 sqlite 覆盖 | **部分采纳**：sqlite 无 DECIMAL/JSONB 原生类型（NUMERIC 亲和性 → 值即 number/text），L5 已交付 sqlite 路径；本版补一条「sqlite 既有形状不回退」断言，并在文档写明「decimal→字符串只适用于 mysql/pg」 |
| P2 / P3 | conformance 清单与升级提示遗漏 | **采纳**（§5） |
| S5 | **pg `query`/`tx` 走 Any，改 `column_json_pg` 对 query 无效** | **采纳（最高优先）**：新增 §2.0，pg 插件改 `Pooled` typed 池 |
| S1 | 解码后出流量扩面（原报错列现可读） | **采纳**：db-guide 风险表登记「仅按整条语句授权，无列级控制」 |
| S2 | 数组/文本回灌 SQL 的注入面 | **采纳**：输出为 JSON 数组（非 SQL 字面量），文档写死「只能作为绑定参数」 |
| S3 | 单行字节上限不在本项范围 | **采纳**：风险表登记，引导 `db.stream` |
| S4 | BIT 去前导零可能被当权限位 | **采纳**：文档红线「不得用 BIT 承载授权位」 |
| S6 | 错误文案中的列名可含控制字符（日志注入） | **采纳**：错误辅助函数过滤控制字符并截断 64 字符 |
