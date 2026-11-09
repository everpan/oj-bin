# 设计：可移植幂等 `INSERT … SELECT`（fromSelect + orIgnore 三方言闭环）

日期：2026-11-09
状态：已获用户批准（一次性闭环；加强 SQL builder 统一性、多方言支持）
关联：`../oj-module/docs/devkit/sql-builder-missing-capabilities-pr.md` §7（上游闭环需求）、oj issue 来源：模板通知铃「清除 / 全部已读」在 MySQL 1064

## 意图与为什么

下游模板（oj-module）通知模块的批量回执写入（`markAllRead` / `clearNotices`）需要一条
**三方言通用、且真幂等**的 `INSERT … SELECT`（一行广播 + 按人回执模型，避免按人 fan-out）。
当前构造器在三条路上各堵一处：

1. **sqlite**：`fromSelect().orIgnore()` 构造期拒绝（v0.1.82 引入）。
2. **mysql**：允许组合但渲染 `ON DUPLICATE KEY UPDATE id = id`，源 SELECT 含目标表同名列时报 1052 歧义。
3. **pg**：源 SELECT 的 `lit` 列一律渲染 `CAST($n AS TEXT)`，写入 BIGINT 目标列报类型错。

三方言**都具备**该语句的合法形态（真机已验证，见下），这是构造器缺口，不是引擎限制。

## 真机证据（2026-11-09，本仓容器 oj-test-mysql 8.4.11 / oj-test-postgres 16.15）

| 方言 | 验证形态 | 结果 |
|---|---|---|
| sqlite | `INSERT OR IGNORE INTO t SELECT …` | ✅ 合法（PR 文档 §7.3②；注意需唯一约束否则去重是空操作） |
| mysql | `INSERT IGNORE INTO t (…) SELECT …` 连跑两次（有唯一键） | ✅ 幂等，2 行 |
| mysql | `… SELECT … ON DUPLICATE KEY UPDATE c = VALUES(c)` | ✅ 但 `VALUES()` 自 8.0.20 弃用，不采 |
| pg | `PREPARE ins(BIGINT,TEXT) AS INSERT … SELECT … $1, $2 … ON CONFLICT DO NOTHING` 连跑两次 | ✅ 裸参数由 INSERT 目标列推断类型（bigint/text 混合），幂等 |

关键事实：

- sqlite 语法里 **upsert 子句**（`ON CONFLICT …`）只能跟在 VALUES 后；但**冲突解决子句**
  （`OR IGNORE`）是另一个语法槽位，可以跟 SELECT。现有拒绝（`src/bridge/query.rs:1715`）
  只考虑了前者。
- pg 裸参数在**独立 SELECT** 里无法推断类型（故 lit 当初套了 CAST），但在
  **INSERT…SELECT 目标列上下文**里由赋值目标推断。lit 只会出现在 select 列清单
  （`ColSpec::Lit` 仅此一处），作 INSERT 源时必有目标列上下文。

## 目标

1. 同一段 `db.table(t).fromSelect(src, cols).orIgnore().run()` 在 sqlite / mysql / pg
   三方言都**构造成功并执行成功**（src 含 lit 注入的常量、目标列类型混合）。
2. 三方言的插入冲突处理渲染收拢到**单一分派点**（统一性），新增方言或新形态只改一处。
3. 文档（api-manual / sql-builder-guide 等 devkit 四件 + CHANGELOG）与实现同步，
   删除「sqlite 不支持 orIgnore + fromSelect」条目，更正「sanctioned 裸 SQL 三方言通用」
   的误标，新增「orIgnore 依赖唯一约束」说明。
4. 测试：三方言渲染快照单测 + env-gated 真机执行腿（含连跑两次行数不变的幂等断言）。

## 非目标

- `db.exec` 方言改写（继续原样透传）。
- 模板侧唯一约束迁移、模板 `notify.ts` 迁移（oj-module 仓动作，待本仓发布后处理）。
- 下架 `db.query`。
- 计算列 `expr` 入口（历史已裁砍，不复活）。

## 反目标（做了就算失败）

- 向业务层暴露方言分支或字符串拼接 SQL。
- 改变 mysql **VALUES 路径**的既有渲染（`ON DUPLICATE KEY UPDATE pk = pk`）——已有用户
  可能依赖其"只吞唯一键冲突"的窄语义，零回归优先。
- 让 sea-query 不支持的语法以裸字符串拼接进 SQL 文本（前缀补丁只作用于**生成后**的
  完整语句串，且校验 `starts_with`，不是拼接注入面）。

## 方案

### 1. 统一冲突处理分派（src/bridge/query.rs）

新增单一分派，按 `(dialect, 是否 fromSelect)` 决定策略：

```rust
/// orIgnore 的方言 × 形态分派（v0.1.127 统一入口）：
/// 新方言 / 新形态只改这张表。
enum IgnoreStrategy {
    /// sea-query on_conflict：sqlite/pg VALUES、pg fromSelect
    OnConflictDoNothing,
    /// 生成串前缀补丁：sqlite fromSelect（upsert 子句只能跟 VALUES，
    /// 但冲突解决子句 OR IGNORE 可跟 SELECT）。语义较宽：吞所有约束冲突。
    SqliteOrIgnorePrefix,
    /// 生成串前缀补丁：mysql fromSelect（INSERT IGNORE；无 1052 歧义、
    /// 不要求主键。VALUES() 形态已弃用不采）。语义较宽：降级部分错误为告警。
    MySqlInsertIgnorePrefix,
    /// sea-query on_conflict 恒等赋值：mysql VALUES（既有行为不动）。
    MySqlDuplicateKeyIdentity,
}

fn ignore_strategy(d: Dialect, from_select: bool) -> IgnoreStrategy
```

- 校验阶段：strategy 为 `MySqlDuplicateKeyIdentity` 时才要求主键（现有检查移入此处）。
- 渲染阶段：`OnConflictDoNothing` / `MySqlDuplicateKeyIdentity` 走 sea-query
  `on_conflict`（现状不变）；两个 Prefix 策略在 `build_sql` 之后对生成串做
  `INSERT INTO ` → `INSERT OR IGNORE INTO ` / `INSERT IGNORE INTO ` 首前缀替换
  （构造期校验原串以 `INSERT INTO ` 开头，不匹配返回 Err——本仓只产 release，
  `debug_assert!` 会被编译掉；静默退回普通 INSERT 等于幂等承诺悄悄失效。
  这是唯一 sanctioned 的生成串改写点）。
- **删除** query.rs:1715 的 sqlite 构造期拒绝。

### 2. pg lit 在 INSERT 源上下文去 CAST

- `build_select_stmt` 增加 `insert_src: bool` 形参。置 true 的**唯一**位置是顶层
  INSERT 的 fromSelect 直接源（query.rs `ins.select_from(sel)` 处）；所有嵌套边界
  （compare/exists 子查询、union 成员、CTE 内的递归调用）一律传 false。
  理由（builder-check 修正）：pg 的 INSERT 目标列类型推断只覆盖直接源 select
  的顶层列清单；嵌套子查询里的裸参数无推断上下文，会报
  `could not determine data type of parameter`——故嵌套中的 lit 必须保持 CAST 原行为
  （确定性 TEXT 类型），这与今天一致，零回归。lit 本身可达于嵌套 select 列清单
  （validate_nested 不限制 lit），所以传播规则必须是"仅直接调用点为 true"。
- `lit_expr` 增加同参：仅当 `insert_src && dialect == Postgres` 时渲染裸
  `Expr::val(...)`（类型由 INSERT 目标列推断）；其余方言/上下文保持现形态
  （sqlite 裸参数、mysql `CAST(? AS CHAR)`、pg 独立 SELECT `CAST($n AS TEXT)`）。
- 只改 `ColSpec::Lit` 分支（lit 仅出现在 select 列清单，INSERT 源里必有目标列上下文）。

### 3. 文档与版本（红线：devkit 四件 + CHANGELOG 同步）

- `docs/devkit/api-manual.md`：删/改「orIgnore + fromSelect 组合在 sqlite 不支持」条目；
  orIgnore 章节补三方言渲染形态表（VALUES × fromSelect × 方言）、「OR IGNORE /
  INSERT IGNORE 语义较宽」「orIgnore 依赖唯一约束，无唯一约束时是空操作」。
- `docs/devkit/sql-builder-guide.md`：同步修正。
- 错误/限制表、SKILL.md 陷阱速查（lit 在 INSERT 源的类型推断行为）、
  scenarios.md 可照抄场景（幂等批量回执写入）。
- `CHANGELOG.md` + 版本号（取发布时下一版本）。
- `cargo xtask build` 归置 `bin/devkit/`，`cargo test --release -p xtask` 的
  devkit 契约用例校验。

### 4. 测试

- `src/bridge/query_tests.rs`（渲染快照，纯函数 `sql_in`）：
  - 改造 `or_ignore_with_from_select_renders_per_dialect`：sqlite 断言
    `INSERT OR IGNORE INTO … SELECT`；mysql 断言 `INSERT IGNORE … SELECT`、
    无主键不再报错；pg 断言 `ON CONFLICT DO NOTHING`（现状保留）。
  - 新增 lit 进 INSERT 源的 pg 渲染断言：无 `CAST`、裸 `$1`；独立 SELECT 的 lit
    仍带 CAST（回归保护）。
  - VALUES 路径三方言渲染断言保持不动（零回归）。
- 真机执行腿（env-gated，DSN 取自 `OJ_TEST_MYSQL` / `OJ_TEST_PG`，未设即 skip
  并诚实登记）：
  - pg：`oj/tests/e2e.rs` 既有 PG 腿附近补 fromSelect().orIgnore() 执行用例
    （建带唯一键的源/目标表，含 lit 常量列 bigint/text 混合，跑两次断言 1 行）。
  - mysql：在 `plugins/oj-db-mysql/src/lib_tests.rs` 或 e2e 对应位置补同形用例。
  - sqlite：根 crate 单测内存库补同形幂等用例。

## 边界与风险

- **语义放宽**：sqlite `OR IGNORE` / mysql `INSERT IGNORE` 不止吞唯一键冲突
  （NOT NULL/CHECK/截断降级为告警/忽略）。文档必须写明；这是两方言上表达
  "幂等 INSERT…SELECT" 的唯一手段，属已知且接受。
- **真幂等依赖唯一约束**：无唯一约束时三方言冲突子句全是空操作（PR 文档 §7.2#5）。
  本仓只在文档说明；唯一约束迁移是 oj-module 侧后续动作。
- 前缀补丁作用于 `build_sql` 完整产物，`toSQL()` 输出同样生效，参数序不受影响。
- 版本号不钉死：发布时取当时 CHANGELOG 的下一版本（工作树有在途修改可能先占号）。
- mysql VALUES 路径零改动，既有用户无感知。

## 留待 builder 的决定（均为小项）

- `build_select_stmt` 新参用 `bool` 还是给 `Build` 加字段——按周边代码风格自定。
- 前缀补丁 helper 放 query.rs 私有 fn，不进 ffi 契约（ABI 不动）。
- env-gated 腿具体挂 e2e.rs 还是插件 lib_tests，按两边现有用例密度自定。

## 我的裁量（用户已知情）

- 一次性闭环三个子项 + 文档 + 测试，单版本 v0.1.127 发布。
- mysql 采 `INSERT IGNORE` 而非 `VALUES()` 恒等式（后者 8.0.20 弃用）。
- 统一性落点 = 单分派函数 `ignore_strategy` + 前缀补丁 helper，不引入更大抽象。
