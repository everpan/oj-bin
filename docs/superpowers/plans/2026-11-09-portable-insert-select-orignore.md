# 可移植幂等 INSERT…SELECT（fromSelect + orIgnore 三方言闭环）实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `fromSelect().orIgnore()` 在 sqlite / mysql / pg 三方言构造并执行成功（源含 lit 常量、目标列类型混合），冲突处理渲染收拢到单一分派点。

**Architecture:** 新增 `ignore_strategy(dialect, from_select)` 分派表（VALUES×fromSelect × 三方言）；sqlite/mysql 的 fromSelect 形态对 `build_sql` 生成串做首前缀补丁（`INSERT OR IGNORE` / `INSERT IGNORE`），pg/mysql VALUES 形态保持 sea-query 现状零回归；pg 的 lit 在 INSERT 直接源上下文去 CAST（目标列推断类型），嵌套层保持 CAST。

**Tech Stack:** Rust + sea-query；测试为根 crate 单测（渲染快照 + sqlite 内存执行）与 env-gated 真库腿（`OJ_TEST_PG` / `OJ_TEST_MYSQL`）。

**Spec:** `docs/superpowers/specs/2026-11-09-portable-insert-select-orignore-design.md`

## Global Constraints

- 只跑 release：`cargo test --release`、`cargo clippy --release --workspace --all-targets -- -D warnings`、`cargo fmt --check`；**禁 debug**。
- 不接管道跑 cargo test；如必须接管道先 `set -o pipefail`；不可并发两个 cargo test。
- SQL 注入红线：标识符只来自 `SchemaRegistry`；值只走绑定参数；前缀补丁只作用于 `build_sql` 完整产物，锚 `INSERT INTO ` 前缀**构造期校验、不匹配返回 Err**（release 生效，绝不静默退回普通 INSERT）。
- commit message 末尾加一行 `unix@vip.qq.com ai`。
- 真机腿在 CI 恒 skip（workflows 无 `OJ_TEST_PG`/`OJ_TEST_MYSQL`）：**发布门禁以本机容器三方言真机腿 PASS 输出为准**，CI 全绿不构成该特性的验证依据；PASS 输出留证贴入 commit message。
- Task 4 涉及版本发布：`oj/Cargo.toml` version 递增 + `CHANGELOG.md` + devkit 四件（`api-manual.md` / `scenarios.md` / `SKILL.md` / `README.md`）同步 + `docs/api/06-db.md` 同步 + `cargo xtask build` 归置 `bin/devkit/`（gitignored，不入库）+ `cargo test --release -p xtask` 契约用例 + annotated tag。

## Review Focus

1. **mysql 源 SELECT 含目标表同名列**（如两边都有 `id`）：旧渲染 `ON DUPLICATE KEY UPDATE id = id` 报 1052；新渲染 `INSERT IGNORE` 无赋值子句——Task 3 真机腿（源含 `id` 列）覆盖。
2. **pg 嵌套子查询里的 lit**：裸参数在嵌套层拿不到 INSERT 目标列类型推断，必须保持 CAST——Task 2 回归测试 `lit_in_nested_subquery_keeps_cast_pg` 覆盖。
3. **无主键 mysql 表 + fromSelect + orIgnore**：旧逻辑要求主键（为恒等赋值取列名），新路径不要求——Task 1 单测 `or_ignore_from_select_mysql_no_pk_ok` 覆盖。
4. **orIgnore 依赖唯一约束**：无唯一键时三方言都是空操作（PR 文档 §7.2#5）——文档写明（Task 4）；真机腿建唯一键并断言连跑两次行数不变（Task 3）。
5. **参数序**：前缀补丁只动 SQL 文本前缀，sea-query 的 `Values` 不动——Task 1 渲染测试断言参数值序列不变（`sql_in` 只返回 SQL，用 Task 3 真机执行含 lit 参数隐式覆盖）。
6. **租户守卫自动注入的 lit 列**：db_tests 骨架 `Extras::default()` 的 `sql_guard = Off`（mod.rs:168-175）使租户注入在既有执行腿**从未激活**——Task 3 租户守卫腿（`sql_guard: Deny` + `RequestInfo.tenant_id`）覆盖。

---

### Task 1: 统一分派 `ignore_strategy` + sqlite/mysql fromSelect 前缀补丁

**Files:**
- Modify: `src/bridge/query.rs:1705-1742`（or_ignore 块）、`src/bridge/query.rs:1768`（build_sql 调用点）、新增私有 enum/fn（放 `query.rs` or_ignore 注释块旁，~line 539）、`src/bridge/bootstrap.js:1490-1492`（orIgnore 注释同步）
- Test: `src/bridge/query_tests.rs:2636-2656`（改造 `or_ignore_with_from_select_renders_per_dialect`）、`query_tests.rs:2783-2833`（改造 `from_select_copies_rows_and_pairs_with_or_ignore`，**行为改变必红，不可遗漏**）

**Interfaces:**
- Produces（均 `query.rs` 私有）:
  - `enum IgnoreStrategy { OnConflictDoNothing, SqliteOrIgnorePrefix, MySqlInsertIgnorePrefix, MySqlDuplicateKeyIdentity }`
  - `fn ignore_strategy(d: Dialect, from_select: bool) -> IgnoreStrategy` — 分派表：sqlite×VALUES / pg ⇒ `OnConflictDoNothing`；sqlite×fromSelect ⇒ `SqliteOrIgnorePrefix`；mysql×fromSelect ⇒ `MySqlInsertIgnorePrefix`；mysql×VALUES ⇒ `MySqlDuplicateKeyIdentity`
  - `fn apply_conflict_resolution(sql: String, s: IgnoreStrategy) -> Result<String, JsErrorBox>` — 仅两个 Prefix 策略调用；`if !sql.starts_with("INSERT INTO ") { return Err(...) }` 后按策略 `replacen` 首前缀（`INSERT OR IGNORE` / `INSERT IGNORE`）。**release 生效的构造期校验**（本仓只产 release，`debug_assert!` 会被编译掉——不匹配必须 Err，绝不静默退回普通 INSERT）。注释注明「这是唯一 sanctioned 的生成串改写点，新增用途需评审」。

- [ ] **Step 1: 改写失败测试**

在 `or_ignore_with_from_select_renders_per_dialect`（`query_tests.rs:2640`）中：

- sqlite 分支由「断言报错」改为断言渲染成功：`norm_ws(sql_in(&reg, req, Dialect::Sqlite).unwrap())` 含 `INSERT OR IGNORE INTO` 且含 `SELECT`；
- mysql 分支改为：`norm_ws` 含 `INSERT IGNORE INTO` 且含 `SELECT`，且不含 `ON DUPLICATE`；
- pg 分支断言保持（含 `SELECT`、含 `ON CONFLICT DO NOTHING`）；
- 新增 `or_ignore_from_select_mysql_no_pk_ok`：registry `SchemaRegistry::new().table("t", &[], &["id","name"]).table("s", &["id"], &["id","name"])`，同 req 走 `Dialect::MySql` 必须 `unwrap()` 且含 `INSERT IGNORE INTO`（旧逻辑此处要求主键会报错）。

req 串照抄现有测试（`query_tests.rs:2644`）：
```rust
let req = r#"{"table":"t","verb":"insert","from_select":{"table":"s","columns":["id","name"]},"insert_cols":["id","name"],"or_ignore":true}"#;
```

- 改造 `from_select_copies_rows_and_pairs_with_or_ignore`（`query_tests.rs:2783-2833`）：其中 sqlite 段（2815-2824）旧断言「orIgnore+fromSelect 返回 code==500 且 msg 含 `orIgnore with fromSelect is not supported on sqlite`」改为 `code==0`、写入 0 行（4 行全撞主键被 IGNORE 跳过）；该用例后续「仍是 8 行」等断言不动。
- 新增 `or_ignore_from_select_with_returning_renders`：上述 req 加 `"returning":["id"]`，三方言各自 `unwrap()`（mysql 单列限制照旧满足），断言冲突前缀形态与 `RETURNING`（mysql 走 LAST_INSERT_ID 两步取，无 `RETURNING` 子句，断言语义不变）共存渲染。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --release -p only-js or_ignore_with_from_select`
Expected: FAIL——sqlite 分支报 `orIgnore with fromSelect is not supported on sqlite`，mysql 分支断言 `INSERT IGNORE INTO` 不满足。

- [ ] **Step 3: 实现**

`query.rs` Insert 动词分支（`build_statement` 内，`query.rs:1656-1769`）：

1. 删除 `query.rs:1715-1720` 的 sqlite 构造期拒绝。
2. 新增 enum 与 `ignore_strategy`（按 Interfaces 分派表）及 `insert_with_conflict_resolution`（按 Interfaces 签名；注释一行：生成串必以 `INSERT INTO ` 开头，只补丁首前缀，参数由 sea-query Values 独立携带）。
3. or_ignore 块改为：
   - `let strategy = req.or_ignore.then(|| ignore_strategy(dialect, req.from_select.is_some()));`
   - 仅当 `strategy == Some(MySqlDuplicateKeyIdentity)` 时保留现有主键检查（`query.rs:1723-1728` 原样搬入）；
   - `OnConflictDoNothing` → 现有 `ins.on_conflict(OnConflict::new().do_nothing().to_owned())`（`query.rs:1738-1740` 原样）；
   - `MySqlDuplicateKeyIdentity` → 现有 `oc.do_nothing_on(pk)` 代码（`query.rs:1730-1736` 原样）；
   - 两个 Prefix 策略不挂 sea-query 子句。
4. `query.rs:1768` 改为：先 `let (sql, values) = build_sql(dialect, &ins);`，仅当 strategy 为两个 Prefix 策略时 `let sql = apply_conflict_resolution(sql, strategy)?;`，再 `params_of((sql, values))`。
5. 同步 `bootstrap.js:1490-1492` 的 orIgnore 注释：fromSelect 形态下 sqlite 渲染 `INSERT OR IGNORE`、mysql 渲染 `INSERT IGNORE`（不要求主键）；VALUES 形态渲染不变。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --release -p only-js or_ignore && cargo test --release -p only-js from_select`
Expected: PASS——含改造后的 `from_select_copies_rows_and_pairs_with_or_ignore`（旧 500 断言已翻转为 code==0）与既有 `or_ignore_renders_per_dialect` 的 VALUES 路径断言。

- [ ] **Step 5: 门禁 + 提交**

Run: `cargo fmt --check && cargo clippy --release --workspace --all-targets -- -D warnings`
Expected: 无输出 / 无警告。

```bash
git add src/bridge/query.rs src/bridge/query_tests.rs
git commit -m "feat(bridge): fromSelect+orIgnore 三方言闭环——统一 ignore_strategy 分派，sqlite OR IGNORE / mysql INSERT IGNORE 前缀渲染

unix@vip.qq.com ai"
```

---

### Task 2: pg lit 在 INSERT 直接源去 CAST

**Files:**
- Modify: `src/bridge/query.rs:733`（`lit_expr`）、`src/bridge/query.rs:1830`（`build_select_stmt` 签名）、`src/bridge/query.rs` 六处 `build_select_stmt` 调用点（`1650`、`1674`、`1078`、`1166`、`1201`、`2151`、`2183`）、`src/bridge/query.rs:1922`（`ColSpec::Lit` 分支）
- Test: `src/bridge/query_tests.rs`（新增两个渲染测试，放 `or_ignore_with_from_select_renders_per_dialect` 之后）

**Interfaces:**
- Consumes: Task 1 未改动的 `sql_in(reg, req, d)` 测试 helper（`query_tests.rs:2574`）。
- Produces（`query.rs` 内部）:
  - `fn lit_expr(v: &Value, dialect: Dialect, insert_src: bool) -> SimpleExpr` — pg 且 `insert_src` 时返回裸 `Expr::val(...)`；其余形态不变（sqlite 裸参数 / mysql `CAST(? AS CHAR)` / pg 独立 SELECT `CAST($n AS TEXT)`）
  - `fn build_select_stmt(req: &QueryReq, bx: Build<'_>, depth: u8, insert_src: bool) -> Result<SelectStatement, JsErrorBox>`

- [ ] **Step 1: 写失败测试**

registry：`SchemaRegistry::new().table("t", &["id"], &["id","user_id","note"]).table("s", &["id"], &["id","user_id"]).table("u", &["id"], &["id"])`。

测试 A `lit_in_insert_source_pg_bare_param`，req：
```rust
let req = r#"{"table":"t","verb":"insert","from_select":{"table":"s","columns":["id",{"lit":7,"as":"user_id"},{"lit":"x","as":"note"}]},"insert_cols":["id","user_id","note"]}"#;
```
断言：pg——`norm_ws` 含 `$1 AS "user_id"`、含 `$2 AS "note"`、**不含** `CAST`；mysql——含 `CAST(? AS CHAR)`；sqlite——不含 `CAST`。

测试 B `lit_in_nested_subquery_keeps_cast_pg`，req：
```rust
let req = r#"{"table":"t","verb":"insert","from_select":{"table":"s","columns":["id"],"conditions":[{"field":"id","op":"in","subquery":{"table":"u","columns":[{"lit":7,"as":"v"}],"conditions":[]}}]},"insert_cols":["id"]}"#;
```
断言：pg——`norm_ws` **含** `CAST($1 AS TEXT)`（嵌套层 lit 保持 CAST；该测试在实现前即通过，是回归保护）。

- [ ] **Step 2: 跑测试确认 A 失败 B 通过**

Run: `cargo test --release -p only-js lit_in_`
Expected: A FAIL（pg 现渲染 `CAST($1 AS TEXT)`，不含 `$1 AS "user_id"`）；B PASS。

- [ ] **Step 3: 实现**

1. `lit_expr` 加 `insert_src: bool` 形参；pg 分支改为 `if insert_src { e } else { e.cast_as("TEXT") }`（注释一行：INSERT 直接源的目标列提供类型推断上下文，裸参数可推断）。
2. `build_select_stmt` 加 `insert_src: bool` 形参；`ColSpec::Lit` 分支（`query.rs:1922`）把该参数传给 `lit_expr`。
3. 调用点传值：`1650`（顶层 select）`false`；`1674`（fromSelect 直接源）`true`；`1078` / `1166` / `1201`（compare/exists 子查询）`false`；`2151`（union 成员）`false`；`2183`（CTE 查询）`false`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --release -p only-js lit_`
Expected: PASS。再跑 `cargo test --release -p only-js --lib`（全量根 crate 单测，防签名改动漏改调用点）。
Expected: 全绿。

- [ ] **Step 5: 门禁 + 提交**

Run: `cargo fmt --check && cargo clippy --release --workspace --all-targets -- -D warnings`

```bash
git add src/bridge/query.rs src/bridge/query_tests.rs
git commit -m "feat(bridge): lit 列在 INSERT 直接源渲染裸参数——pg 由目标列推断类型，嵌套层保持 CAST

unix@vip.qq.com ai"
```

---

### Task 3: 三方言执行腿（sqlite 内存常跑 + PG/MySQL env-gated）

**Files:**
- Modify: `src/bridge/db_tests.rs`（新增一个测试，放 `stream_matches_query_on_real_db` 之后）

**Interfaces:**
- Consumes: Task 1/2 的构造器行为；既有 helper `crate::bridge::SqlxAccessor::arc(dsn)`、`Bridge::with_dbs_and_loader`（用法照抄 `db_tests.rs:297-312`）、`SchemaRegistry::new().table(name, pks, cols)`。
- Produces: 无（终态测试）。

- [ ] **Step 1: 写执行测试**

新增 `from_select_or_ignore_idempotent_all_dialects`，`#[tokio::test(flavor = "current_thread")]`。结构照抄 `db_tests.rs:270-286` 的 env-gated 骨架：PG/MySQL 分支各在 `OJ_TEST_PG` / `OJ_TEST_MYSQL` 非空时跑，否则 `eprintln!("skip: ...")` 并 return；sqlite 分支（`SqlxAccessor::arc("sqlite::memory:")`，`db_tests.rs:108` 用法）常跑。

每条腿做：
1. 用 accessor 的 `exec_with_params` 建表（先 `drop table if exists`）：源表 `ojm_nb(id, tenant_id)`，目标表 `ojm_nbr(id, tenant_id, broadcast_id, user_id, note)`，目标表带 `unique(tenant_id, broadcast_id, user_id)`；往源表插 3 行（`d1`×2、`d2`×1）。**d1 两行的 `id` 必须不同**——`broadcast_id` 按位置取自源 `id`，若两行 id 相同，第二次 insert 即被 ignore，`n==2` 假红成 `n==1`。
2. `Bridge::with_dbs_and_loader`，registry 含两表全部列。
3. `run_with` 执行 JS（三方言同一串——这就是可移植性断言）：
```js
(async () => {
  const cols = ["tenant_id","broadcast_id","user_id","note"];
  const src = () => db.table("ojm_nb").select(["tenant_id","id",{lit:7,as:"user_id"},{lit:"x",as:"note"}])
      .where({field:"tenant_id",op:"eq",value:"d1"});
  const r1 = await db.table("ojm_nbr").fromSelect(src(), cols).orIgnore().run();
  await db.table("ojm_nbr").fromSelect(src(), cols).orIgnore().run();
  await db.table("ojm_nbr").fromSelect(src(), cols).orIgnore().run();
  const rows = await db.table("ojm_nbr").select(["*"]).where({field:"tenant_id",op:"eq",value:"d1"}).all();
  return json.ok({ affected1: r1.affected ?? -1, n: rows.length });
})().catch(e => json.fail(500, String(e)))
```
4. 断言 `v["code"] == 0`、`v["data"]["n"] == 2`（跑 3 次仍 2 行——幂等 + 唯一键生效 + lit 注入类型正确）、`v["data"]["affected1"] == 2`（**吞行哨兵**：NOT NULL/截断被 `OR IGNORE`/`INSERT IGNORE` 静默吞行时首跑 affected 即 < 2——光凭 `n==2` 抓不到，因为被吞的行后续重跑会补上）。

**租户守卫腿（硬需求，同一测试函数内的第二段）**：db_tests 骨架的 `Extras::default()` 使 `sql_guard = Off`（`mod.rs:168-175`），租户注入路径在既有执行腿**从未激活**，必须补：
1. 独立 fixture 表：源表 `ojm_nb_t` 用共享表声明（参照 `query_tests.rs:1460` 的 `table_owned_shared(..., true)`，避免源 select 被注入租户过滤——"广播共享 + 回执租户隔离"语义），目标表 `ojm_nbr_t` 声明 `tenant_id` 列即自动受约束（`registry.rs:85-88`，无需特殊标记），带 `unique(tenant_id, broadcast_id, user_id)`。
2. `Bridge::with_dbs_and_loader` 第 6 参 `Extras { sql_guard: SqlGuard::Deny, ..Default::default() }`。
3. `run_with` 带 `RequestInfo { tenant_id: Some("d1".into()), ..Default::default() }`（照 `query_tests.rs:1474`）。
4. JS 列清单**不含** tenant_id（守卫自动补列；显式给会被 `query.rs:1323-1327` 拒绝）。
5. 断言同幂等语义（跑 3 次 `n==2`）。`tenant_id` 列声明 text 即可（三方言通用）。注：`sql_in` 纯构造不跑 `apply_tenant`，租户注入的渲染断言只能进执行腿，快照层覆盖不到。

DDL 三方言差异（id 列）：sqlite `id integer primary key`；pg `id bigint generated always as identity primary key`；mysql `id bigint auto_increment primary key`。mysql 唯一键写 `unique key uk_nbr(tenant_id,broadcast_id,user_id)`，pg/sqlite 写 `unique(tenant_id,broadcast_id,user_id)`。

- [ ] **Step 2: sqlite 分支先行验证**

Run: `cargo test --release -p only-js from_select_or_ignore`
Expected: sqlite 分支 PASS；PG/MySQL 打印 skip。若 sqlite 红，回 Task 1/2 修。

- [ ] **Step 3: 真机三方言验证**

Run: `OJ_TEST_PG='postgres://postgres:postgres@127.0.0.1:5432/ojtest' OJ_TEST_MYSQL='mysql://root:root@127.0.0.1:3306/ojtest' cargo test --release -p only-js from_select_or_ignore -- --nocapture`
Expected: 三分支全 PASS，无 skip。

- [ ] **Step 4: 提交**

```bash
git add src/bridge/db_tests.rs
git commit -m "test(bridge): fromSelect+orIgnore 三方言幂等执行腿（sqlite 常跑，PG/MySQL env-gated）

unix@vip.qq.com ai"
```

---

### Task 4: 文档四件 + CHANGELOG + devkit 归置

**Files:**
- Modify: `docs/devkit/api-manual.md`（`§3944` 错误/限制表行 + orIgnore 章节）、`docs/devkit/db-guide.md`（`§848`、`§1359`、`§2140/2181` 附近）、`docs/devkit/sql-builder-guide.md:113`、`docs/devkit/SKILL.md:181`、`docs/devkit/scenarios.md:3119`、`docs/devkit/README.md`（如版本说明涉及）、`docs/api/06-db.md`（103、257 行旧条目，**别漏**）、`CHANGELOG.md`、`oj/Cargo.toml` version `0.1.127` → **`0.1.128`**
- Verify: `bin/devkit/` 由 xtask 重新归置（gitignored，不入库）

**前置（已裁决并已完成：WIP 先独立成版）**：冷启动 WIP 已独立提交为 **v0.1.127**（commit `1a835bb` + annotated tag，push 待定），占号完毕；本计划固定取 **v0.1.128**。CHANGELOG「下一版（未发布）」节已归零，Task 4 的文件级 `git add` 安全。

**Interfaces:**
- Consumes: Task 1-3 的定型行为。
- Produces: 无代码接口。

- [ ] **Step 1: 改文档（每处一句话方向，具体文字由实现者按上下文写准）**

- `api-manual.md`：§3944 限制表行——删「sqlite 不支持（构造期拒绝）」，改为「三方言均支持；sqlite 渲染 `INSERT OR IGNORE`、mysql 渲染 `INSERT IGNORE`、pg 渲染 `ON CONFLICT DO NOTHING`」；orIgnore 章节补：① **形态表（VALUES × fromSelect × 三方言渲染）并加"冲突处理宽度"列**（VALUES：sqlite/pg 窄、mysql 窄；fromSelect：sqlite 宽、mysql 宽、pg 窄——mysql 两格宽度不同是**有意保留**，CHANGELOG 须如实写明）；② 「`OR IGNORE`/`INSERT IGNORE` 语义比 `ON CONFLICT DO NOTHING` 宽——NOT NULL/CHECK/截断违例也被静默跳过或降级为告警」；③ 「**只幂等唯一键冲突**：无唯一约束时冲突子句是空操作，连跑两次会产生重复行」。
- `db-guide.md` 三处（`§848`、`§1359`、`§2181` 附近）：把「构造期拒绝 + 绕行走裸 `db.exec`」改为组合已支持 + 上述两条警告；§1359 错误表同行。
- `sql-builder-guide.md:113`：同步。
- `SKILL.md:181` 陷阱速查：条目反转——sqlite 可用但**故障模式跨方言相反**：pg 上 NOT NULL/CHECK 违例会照常报错，sqlite/mysql 上同样脏数据**静默丢行**；措辞用「只幂等唯一键冲突」而非笼统「幂等」。
- `scenarios.md:3119`：改为支持，并加可照抄场景「幂等批量回执写入（广播 × 按人回执）」：fromSelect + orIgnore + lit 注入 uid/时间戳的完整代码块。
- `docs/api/06-db.md`（103、257 行）：同步改旧条目与旧报错串。
- 全 `docs/devkit` + `docs/api` 内 grep `orIgnore with fromSelect is not supported` 与「绕行走裸」确保零残留。

- [ ] **Step 2: CHANGELOG + 版本 + 下游联动登记**

`oj/Cargo.toml` version `0.1.127` → `0.1.128`；`CHANGELOG.md` 顶部新增「## 下一版（未发布）」节并写入条目（发布时改标题为 v0.1.128（日期））：fromSelect().orIgnore() 三方言闭环（统一 ignore_strategy 分派；sqlite `INSERT OR IGNORE`、mysql `INSERT IGNORE` 前缀渲染；pg INSERT 源 lit 去 CAST 由目标列推断类型）+ 文档误标更正 + 「只幂等唯一键冲突」说明 + **统一性落在分派点、mysql 两形态语义宽度有意保留差异** + **下游联动：oj-module 模板迁移须在本版 tag 之后，并抬 MIN_OJ_VERSION ≥ 本版**（v0.1.126 消费侧联动惯例）。

- [ ] **Step 3: 归置 + 契约校验**

Run: `cargo xtask build && cargo test --release -p xtask`
Expected: `bin/devkit/` 四件与源一致，xtask devkit 契约用例全绿。

- [ ] **Step 4: 全量门禁（CI 全绿 ≠ 本特性已验证）**

Run: `cargo fmt --check && cargo clippy --release --workspace --all-targets -- -D warnings && cargo test --release --workspace`
Expected: 全绿（真机腿无 env 时 skip 属预期）。**注意：CI 无 `OJ_TEST_PG`/`OJ_TEST_MYSQL`，真机腿 CI 恒 skip——本特性的发布依据是 Task 3 Step 3 在本机容器的三方言 PASS 输出，留证贴入 commit message。**

- [ ] **Step 5: 提交 + tag**

```bash
git add docs/devkit docs/api/06-db.md CHANGELOG.md oj/Cargo.toml Cargo.lock src/bridge/query.rs src/bridge/query_tests.rs src/bridge/db_tests.rs src/bridge/bootstrap.js
git commit -m "chore(release): vX.Y.Z——fromSelect().orIgnore() 三方言闭环 + devkit 四件同步

真机验证：<粘贴 Task 3 Step 3 三方言 PASS 摘要>

unix@vip.qq.com ai"
git tag -a vX.Y.Z -m "vX.Y.Z: fromSelect().orIgnore() 三方言闭环"
```
tag 的 `git push` 由用户决定（或按 CHANGELOG 规则在标题注明「未打标签」）。
