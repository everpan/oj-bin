# dbx 缺口批次 2 实施计划（v0.1.112）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 交付分流设计文档第二批：REQ-13 rename 语义、REQ-10 一期（processlist/users/roles + KILL 分类）、REQ-08（例程/视图/触发器元数据 + truncated 信令）、REQ-09-2 schema 定向、REQ-07b 二进制一律 cellenv 信封。

**Architecture:** 全部 host 侧或插件 JSON 面改动，**零 ABI**。catalog 新 kind/扩展在 `introspect.rs`（SQL 串 + `catalog()` 整形）与 `db_introspect.rs`（op/信封）；rename 在 `db_ddl.rs` 显式 spec 路径；KILL 在 `db_policy.rs` 动词表；REQ-07b 在两 db 插件的列解码臂。

**Tech Stack:** Rust（deno_core bridge、sqlx 0.9）、serde_json、oj-plugin-ffi::cellenv（复用批次 1）。

**Spec:** `docs/superpowers/specs/2026-10-30-dbx-gap-triage-design.md`（批次 2 节；REQ-13 表面契约以下游 `../oj-dbx/docs/upstream/REQ-13-rename-column.md` 为准，spec 已修订确认）。

## Global Constraints

- 一切构建/测试/clippy 用 `--release`；异步测试 `tokio::test(flavor = "current_thread")`；禁固定 sleep。
- **零 ABI**：不改 `oj-plugin-ffi` 任何 repr(C)；cellenv 已是纯 fn 依赖。
- TDD：每个实现步骤先有 RED 测试且看过失败原因。
- commit message 尾行 `unix@vip.qq.com ai`；每任务一提交。
- `bootstrap.js` 7-bit ASCII。
- 动态标识符只走 `quote_ident`/`check_fragment`（SQL 注入红线）。
- 最后一个任务：CHANGELOG + `oj/Cargo.toml` 0.1.111→0.1.112 + devkit 四件 + `cargo xtask build` + `cargo test --release -p xtask` 契约校验。
- REQ-07b 是 **breaking**（合法 UTF-8 bytea/blob 从裸字符串变 `$<type>:` 信封），CHANGELOG 与 devkit 必须标注。

## 关键裁决（计划级，执行时照此实现）

1. **rename 契约 = `to.columns[].previousName`**（下游 REQ-13 §4.1–4.3 六边界全实现）；设计文档的 `renames:[{from,to}]` 草图撤销。启发式同型一删一增 **不进自动路径**，只产 `warnings[]`（REQ-13 §4.4 二选一中的「告警」支）。
2. **catalog 新 kind（processlist/users/roles/triggers）拒绝 `scope`**：op 显式检查，结构化报错——这三类是全实例/全库语义，`scoped_sql` 的 `?` 替换模型套不上。
3. **`catalog()` 签名改为返回 `(Vec<Value>, bool)`**（truncated 旗标），T3 落地、T4/T5 消费，避免 T5 再改一次签名。
4. **REQ-08 一期瘦身**（对照下游 REQ-08 原文的偏差，均如实进 devkit）：routines 的 `args` 给**原始声明文本**（PG `pg_get_function_arguments` / MySQL 一期不给参数展开），结构化解析留待真实需求；PG 视图补壳不做独立列别名清单（viewdef 已含输出名）；catalog 只加 `truncated` 信令，**不做 `limit` 入参**。
5. **MySQL REQ-07b 判别 = 二进制字符集探针**（positional）：sqlx-mysql 对 binary charset 拒绝 String 解码，`Vec<u8>` 探针成功即 BLOB/BINARY/VARBINARY 族，TEXT（utf8 charset）永不落入——与「按列声明类型」同判别力，文档写明两方言判别依据不同。PG = 声明类型名 `"BYTEA"` 短路。
6. **MySQL Any 池路径不动**（AnyRow 类型信息扁平化，无法判别；真实连接全走 typed 路径），devkit 登记。

## Review Focus

1. **rename × 索引/约束/FK 并存**：DROP（旧名，rename 前）/ ADD（新名，rename 后）的语句序与真库重放——T1 只做单测，真库重放归 env-gated 腿。
2. **MySQL TEXT 与 BLOB 同 ColumnType**：唯一判别是 String 探针成败——真库 TEXT 回读必须不回退（T7 env-gated 钉）。
3. **catalog() `_ => json!({})` 静默空对象**：新 kind 漏 shape arm 即踩——守卫测试枚举所有 kind（T3/T5）。
4. **processlist query 文本敏感 + users/roles 权限差异**：管理面固有风险，devkit 标注；取不到必须 fail-loud，不空冒充。
5. **低权限账号 SHOW CREATE VIEW（MySQL 1142）**：真库验不了（root 有权限），文案凭官方文档，进开放项。

---

### Task 1: REQ-13 rename 语义（db.ddlDiff `previousName`）

**Files:**
- Modify: `src/bridge/db_ddl.rs`（`ColSpec` :68、`parse_columns` :78、`ddl_diff` :261、alter 路径 :413–750、测试模块 :896+）
- Modify: `src/bridge/bootstrap.js:1090-1094`（ddlDiff JSDoc，7-bit ASCII）

**Interfaces:**
- Consumes: `quote_ident`（:29）、`col_def_sql`、测试助手 `both(spec)`（:901–906，PG+MySQL 双跑）。
- Produces（后续任务/下游消费）: 输出顶层新增 `warnings: string[]`（恒存在，干净时空数组）；`diff.columns.renamed: [{"from","to"}]`；changed 新增条目 `{"name":<新名>,"field":"name","from":<旧名>,"to":<新名>}`；rename 语句 `ALTER TABLE {t} RENAME COLUMN {旧} TO {新}`（PG 双引号 / MySQL 反引号）。

- [ ] **Step 1: RED——rename 主干测试**

```rust
// db_ddl.rs 测试模块
#[test]
fn rename_produces_single_statement_pg_mysql() {
    let spec = json!({"objectType":"table","name":"t",
      "from":{"columns":[{"name":"a","nativeType":"int4"}]},
      "to":{"columns":[{"name":"b","nativeType":"int4","previousName":"a"}]}});
    let (pg, my) = both(spec);
    assert_eq!(pg["statements"], json!(["ALTER TABLE \"t\" RENAME COLUMN \"a\" TO \"b\""]));
    assert_eq!(my["statements"], json!(["ALTER TABLE `t` RENAME COLUMN `a` TO `b`"]));
    assert_eq!(pg["diff"]["columns"]["renamed"], json!([{"from":"a","to":"b"}]));
    // changed 携带 name 变更条目；无 DROP/ADD
    assert!(pg["diff"]["columns"]["changed"].as_array().unwrap().iter()
        .any(|e| e == &json!({"name":"b","field":"name","from":"a","to":"b"})));
    assert_eq!(pg["warnings"], json!([]));
}
```

- [ ] **Step 2: 跑测试看失败**

Run: `cargo test --release db_ddl::tests::rename`
Expected: FAIL——`previousName` 被静默吞，statements 是 DROP+ADD。

- [ ] **Step 3: RED——边界测试（REQ-13 §4.3 全六款）**

```rust
#[test]
fn rename_boundary_errors() {
    // previousName 不在 from → Err 含 "previousName" 与 "not found"
    // 两个 to 列指同一 previousName → Err
    // 互换 a↔b → Err 含 "ddl_diff_rename_swap"
    // 目标名在 from 已存在（未换走）→ Err 含 "conflict"
    // to 既有 previousName:"a" 又有 name:"a" 的列 → Err
    // previousName === name → 视为普通列，无 rename 语句（OK 路径）
}
#[test]
fn ambiguous_same_type_drop_add_warns() {
    // from=[{a int4}] to=[{b int4}]（无 previousName）
    // → warnings 恰 1 条，含 "a"、"b"、"数据"
    // statements 仍 DROP+ADD（告警支，不拒绝）
}
#[test]
fn rename_plus_type_change_orders_and_uses_new_name() {
    // rename a→b + b 的 nativeType 变更
    // PG: [RENAME, ALTER COLUMN b TYPE ...]；MySQL: [RENAME, MODIFY COLUMN b ...]
    // RENAME 索引 < ALTER 索引
}
#[test]
fn rename_keeps_index_and_constraint() {
    // from: 列 a + 索引 ix(a)；to: 列 b(previousName:a) + 索引 ix(b)
    // → 无 DROP INDEX / CREATE INDEX；无约束 DROP/ADD
}
```

- [ ] **Step 4: 跑测试看失败**

Run: `cargo test --release db_ddl::tests::rename`
Expected: FAIL（warns 测试失败于 `warnings` 键不存在）。

- [ ] **Step 5: GREEN 实现**

`ColSpec` 加 `previous_name: Option<String>`；`parse_columns` 读 `previousName`（过 `quote_ident` 校验，错报 `ddlDiff: {dim}.columns[] invalid previousName`）。`ddl_diff` alter 路径，解析完 from_c/to_c 后、计算 added/removed 前：

- 收集 to 侧带 `previous_name` 的列，逐项校验（裁决 1 的错误文案）并构建 rename 映射；互换检测：rename 对 (a→b) 与 (b→a) 同现即拒。
- 命中 rename 的对子：不参与 added/removed；changed 比较以旧列 vs 新列（条目 name 用新名）。
- 语句序（REQ-13 §4.2）：既有 drops（FK→constraint→index）之后、DROP COLUMN 之前插 RENAME。
- 命名维度（constraints/indexes/foreignKeys 的 `columns` 数组）：diff 前对 **from 侧**列名做 rename 替换（PG/MySQL RENAME 自动跟随依赖对象，替换后不变的维度产零语句）。
- MySQL MODIFY 循环（:578–596）与 PG dispatch（:597–647）**跳过 `field == "name"` 的 changed 条目**（rename 语句已单独发）。
- 告警：消掉 rename 后，removed×added 中 `native_type` 逐字节相同的对 → warnings 文案「疑似重命名：`a` → `b`，将按删列 + 建列执行，数据会丢失」。
- 输出：`json!({"diff":…, "statements":…, "warnings":…})`，两路径（create/alter）都带 `warnings`（create 恒空数组）。bootstrap.js:1090–1094 JSDoc 补 `warnings` 与 `renamed` 说明。

- [ ] **Step 6: 跑全模块测试**

Run: `cargo test --release db_ddl`
Expected: 全绿，含既有排序测试 `statement_ordering_is_drop_then_alter_then_add_fk_last`（:1273）不回归。

- [ ] **Step 7: Commit**

```bash
git add src/bridge/db_ddl.rs src/bridge/bootstrap.js
git commit -m "feat(db): REQ-13 rename 语义——previousName 产 RENAME COLUMN，同型一删一增告警兜底

unix@vip.qq.com ai"
```

---

### Task 2: REQ-10 一期 a——`KILL` 语句分类放行

**Files:**
- Modify: `src/bridge/db_policy.rs`（`classify_stmt` :813–885 动词表 :860 区域、测试 :1443+）

**Interfaces:**
- Consumes: `Risk`（:67）、测试助手 `risk_of`（:1435）。
- Produces: `KILL`/`KILL QUERY n`/`KILL CONNECTION n` → `Risk::Write`（非 dangerous，不吃 confirm gate）；read_only 别名与会话仍拒（既有闸门不动）。REQ-11 旁路取消配方的服务端前提。

- [ ] **Step 1: RED 测试**

```rust
#[test]
fn kill_classified_write() {
    assert_eq!(risk_of("kill 42"), Risk::Write);
    assert_eq!(risk_of("KILL QUERY 42"), Risk::Write);
    assert_eq!(risk_of("kIlL 1"), Risk::Write); // 大小写不敏感
}
#[test]
fn kill_blocked_under_read_only() {
    // evaluate：read_only 别名 + KILL → Violation(ReadOnly)（仿 :1562 evaluate_policy_bits 写法）
}
```

- [ ] **Step 2: 跑测试看失败**

Run: `cargo test --release db_policy::tests::kill`
Expected: FAIL——`_ => (Risk::Unknown, false)`（:883）。

- [ ] **Step 3: GREEN**

`classify_stmt` 动词表加 `"KILL" => (Risk::Write, false),`（置于 `_` 兜底前）。

- [ ] **Step 4: 跑测试**

Run: `cargo test --release db_policy`
Expected: 全绿（含 `adversarial_samples_never_degrade_to_readonly` :1467 不回归——KILL 升 Write 不影响）。

- [ ] **Step 5: Commit**

```bash
git add src/bridge/db_policy.rs
git commit -m "feat(db-policy): KILL 分类为 Write——REQ-11 旁路取消的服务端前提落地

unix@vip.qq.com ai"
```

---

### Task 3: REQ-10 一期 b——catalog 新 kind：processlist / users / roles

**Files:**
- Modify: `src/bridge/introspect.rs`（`Kind` :29–42、`capability` :46–59、`parse_catalog` :72–83、`catalog_sql` :526–641、`catalog()` :646–720 改元组返回、`CATALOG` 测试数组 :1116–1124、kind 清单 :1625–1632 与 :1693–1700、测试模块 :992+）
- Modify: `src/bridge/db_introspect.rs`（op :21–92：kind 错误文案 :32、scope 显式拒绝、信封 :364–366 加 `truncated`）

**Interfaces:**
- Consumes: `MAX_ROWS`（:26）、`run_catalog_sql`（db_introspect.rs:308）。
- Produces: `parse_catalog` 增 `"processlist"|"users"|"roles"`；**`pub fn catalog(kind, rows) -> (Vec<Value>, bool)`**（裁决 3，T4/T5 消费，调用点全改）；信封增 `truncated: bool`；新 kind 传 scope → 结构化 Err（裁决 2）。

- [ ] **Step 1: RED 守卫测试**

```rust
#[test]
fn catalog_new_kinds_shape_and_scope() {
    // CATALOG 数组加三新 kind 后本测试才有意义：
    let (items, truncated) = catalog(Kind::Processlist, vec![json!({
        "id":1,"user":"u","database":"d","host":"h","state":"active","elapsed_sec":3,"query":"select 1"
    })]);
    assert_eq!(items[0]["state"], "active");
    assert!(items[0].get("elapsedSec").is_some()); // camelCase，对齐 Stats 的 rowsEstimate 先例
    let (_, t) = catalog(Kind::Users, vec![json!({"name":"u","host":"h"}); 1001]);
    assert!(t); // > MAX_ROWS 旗标
    let (_, f) = catalog(Kind::Roles, vec![json!({"name":"r"})]);
    assert!(!f);
}
```

- [ ] **Step 2: 跑测试看失败**

Run: `cargo test --release introspect::tests::catalog_new_kinds`
Expected: FAIL（enum 无变体，编译错即 RED）。

- [ ] **Step 3: GREEN**

- `Kind` 增 `Processlist | Users | Roles`；`capability()` 同名小写串；`parse_catalog` 加映射。
- `catalog_sql` 新臂（PG/MySQL；sqlite → None）：
  - processlist PG：`SELECT pid AS id, usename AS "user", datname AS database, client_addr::text AS host, state, EXTRACT(EPOCH FROM now() - query_start)::bigint AS elapsed_sec, query FROM pg_stat_activity`；MySQL：`SELECT id, user, db AS \`database\`, host, command AS state, time AS elapsed_sec, info AS query FROM information_schema.processlist`
  - users PG：`SELECT usename AS name, NULL AS host FROM pg_user`；MySQL：`SELECT user AS name, host FROM mysql.user`
  - roles PG：`SELECT rolname AS name FROM pg_roles`；MySQL（8.0+）：`SELECT DISTINCT role_name AS name FROM information_schema.applicable_roles`
- SQL **零 `?`**（`catalog_param()` 不扩）；`catalog_limit()` 默认 true 不动。
- `catalog()` 改元组返回：三新 kind shape arm（Processlist → `{id,user,database,host,state,elapsedSec,query}`；Users → `{name,host}`；Roles → `{name}`）；truncated = 入参行数 > MAX_ROWS（Constraints 组的取行数口径，注释标注近似）。
- op：新 kind + `scope.is_some()` → 结构化 Err（`{code:"unsupported", msg:"catalog kind X rejects scope"}` 形态，仿既有 unsupported 文案）；kind 错误文案 :32 补三新串；信封 :364–366 加 `"truncated"`。
- 测试清单 :1625–1632、:1693–1700 按既有语义处理（无 scope 谓词的新 kind 不进「必须有 scope 谓词」的清单，如机制要求全 kind 覆盖则在 op 拒绝 scope 处登记）。

- [ ] **Step 4: env-gated 真库腿**

```rust
// 仿 real_mysql_constraints_no_cross_table_leak（:1139）风格
#[tokio::test(flavor = "current_thread")]
async fn real_pg_processlist_and_users() { /* OJ_TEST_PG 未设即 skip 打印 */ }
#[tokio::test(flavor = "current_thread")]
async fn real_mysql_processlist_and_users() { /* OJ_TEST_MYSQL；processlist 非空且含 id/user 键 */ }
```

- [ ] **Step 5: 跑全模块 + 提交**

Run: `cargo test --release introspect` → 全绿（既有 `catalog_constants_param_count_and_no_like` :1205 等守卫不回归）。
Commit: `feat(db): catalog 增 processlist/users/roles kind + truncated 信令\n\nunix@vip.qq.com ai`

---

### Task 4: REQ-08 a——例程元数据 + ddl kind 对象路由 + 视图 DDL 补壳

**Files:**
- Modify: `src/bridge/introspect.rs`（Routines SQL :546–556、`catalog()` Routines arm :702–705、新纯函数视图补壳、测试）
- Modify: `src/bridge/db_introspect.rs`（`pg_ddl_dispatch` :97–150 例程回退、`mysql_show_create` :227–236 对象路由）

**Interfaces:**
- Consumes: Task 3 的 `(Vec<Value>, bool)` 返回；`catalog_query_rows`（:233）。
- Produces: `catalog("routines")` items 稳定键集 `{name,type,args,returns,source,language,volatility,securityDefiner,comment}`（缺省 null）；`catalog("ddl", arg)` 认函数/过程/视图（PG `objectType:"function"|"procedure"`，MySQL SHOW CREATE 路由）；PG 视图 ddl = `CREATE [OR REPLACE] VIEW … AS`（物化走 `MATERIALIZED`）。

- [ ] **Step 1: RED 形状测试**

```rust
#[test]
fn routines_shape_carries_metadata() {
    let (items, _) = catalog(Kind::Routines, vec![json!({
        "name":"f_add","type":"FUNCTION","args":"a integer, b integer","returns":"integer",
        "source":"select a+b","language":"sql","volatility":"s","secdef":true,"comment":"c"
    })]);
    let it = &items[0];
    for k in ["args","returns","source","language","volatility","securityDefiner","comment"] {
        assert!(it.get(k).is_some(), "missing {k}");
    }
}
#[test]
fn pg_view_wrap_adds_create_shell() {
    assert_eq!(pg_view_wrap("v", " SELECT 1 AS v;", false), "CREATE OR REPLACE VIEW \"v\" AS  SELECT 1 AS v");
    assert!(pg_view_wrap("mv", "SELECT 1", true).starts_with("CREATE MATERIALIZED VIEW"));
}
```

- [ ] **Step 2: 跑测试看失败**

Run: `cargo test --release introspect::tests::routines`
Expected: FAIL（arm 只产 `{name,type}`；`pg_view_wrap` 不存在）。

- [ ] **Step 3: GREEN——SQL 与整形**

- PG Routines SQL 扩列：`pg_get_function_arguments(p.oid)` AS args、`pg_get_function_result(p.oid)` AS returns、`p.prosrc` AS source、`l.lanname` AS language（JOIN pg_language）、`p.provolatile` AS volatility、`p.prosecdef` AS secdef、`obj_description(p.oid,'pg_proc')` AS comment。
- MySQL Routines SQL 扩列：`routine_definition` AS source、`data_type` AS returns、`is_deterministic` AS deterministic、`security_type` AS security、`routine_comment` AS comment；args 一期 null（裁决 4）；`catalog()` Routines arm 按上键集整形（deterministic→volatility："YES"→"deterministic"/"NO"→"nondeterministic"；security→securityDefiner："DEFINER"→true）。
- 新纯函数 `pg_view_wrap(name, viewdef, materialized) -> String`：剥尾分号 + 壳拼接（quote 用既有 `pg_quote_ident` :413）。

- [ ] **Step 4: RED——ddl 路由 env-gated 真库腿**

```rust
#[tokio::test(flavor = "current_thread")]
async fn real_pg_ddl_returns_function_def() { /* 建 f_add → catalog_sql(Ddl)+arg 路由到 pg_get_functiondef，ddl 以 CREATE 开头，objectType=function */ }
#[tokio::test(flavor = "current_thread")]
async fn real_mysql_ddl_view_returns_create_view() { /* 建视图 → SHOW CREATE VIEW，ddl 含 CREATE 与 VIEW；此前 400 */ }
#[tokio::test(flavor = "current_thread")]
async fn real_pg_ddl_view_is_wrapped() { /* 视图 ddl 以 CREATE OR REPLACE VIEW 开头 */ }
```

- [ ] **Step 5: GREEN——路由**

- `pg_ddl_dispatch`：relkind 探测空 → 回退 `pg_proc` 名探测 → `SELECT pg_get_functiondef(p.oid) AS ddl, CASE prokind … END AS object_type`（无 sqlite 影响；PG 专属路径）。
- `mysql_show_create`：`arg` 白名单扩 `[A-Za-z0-9_$]+(\.[A-Za-z0-9_$]+)?`（Task 6 的两段形态预留）；op 内 MySQL Ddl：先查 `information_schema`（tables/routines/triggers/views）定对象类型，选 `SHOW CREATE TABLE|VIEW|PROCEDURE|FUNCTION`（触发器 ddl 归 Task 5 后的 triggers 展示，不进 ddl kind——登记）。
- PG Ddl 视图臂用 `pg_view_wrap` 包壳（物化 relkind 'm'）。

- [ ] **Step 6: 跑全模块 + 提交**

Run: `cargo test --release introspect` → 全绿。
Commit: `feat(db): REQ-08 一期——例程元数据 + ddl 对象路由 + 视图 DDL 补壳\n\nunix@vip.qq.com ai`

---

### Task 5: REQ-08 b——triggers kind + catalog truncated 信令收口

**Files:**
- Modify: `src/bridge/introspect.rs`（Kind/`parse_catalog`/`catalog_sql`/`catalog()`/测试，复用 Task 3 元组返回）
- Modify: `src/bridge/db_introspect.rs`（:32 文案、scope 拒绝清单扩 triggers）

**Interfaces:**
- Consumes: Task 3 的 kind 增补模式与 scope 拒绝机制、信封 `truncated`。
- Produces: `catalog("triggers")` items `{name,table,timing,events,body,order,enabled}`（跨方言同键集，不适用的键 null）；PG timing/events 从 `pg_get_triggerdef` 文本令牌扫描（BEFORE/AFTER/INSTEAD OF + INSERT/UPDATE/DELETE）。

- [ ] **Step 1: RED 形状 + 解析测试**

```rust
#[test]
fn triggers_shape_pg_parses_definition() {
    // PG 行：name/table/tgenabled=O/definition="CREATE TRIGGER trg BEFORE INSERT OR UPDATE ON t FOR EACH ROW EXECUTE FUNCTION f()"
    let (items, _) = catalog(Kind::Triggers, vec![json!({…})]);
    assert_eq!(items[0]["timing"], "BEFORE");
    assert_eq!(items[0]["events"], json!(["INSERT","UPDATE"]));
    assert_eq!(items[0]["enabled"], true);
}
#[test]
fn triggers_shape_mysql_direct_columns() { /* action_timing/event/body/order 直映 */ }
```

- [ ] **Step 2: 跑测试看失败**

Run: `cargo test --release introspect::tests::triggers`
Expected: FAIL（编译级）。

- [ ] **Step 3: GREEN**

- `Kind::Triggers` + capability/parse_catalog/文案；`catalog_sql`：PG `SELECT t.tgname AS name, c.relname AS table, t.tgenabled AS enabled, pg_get_triggerdef(t.oid) AS definition FROM pg_trigger t JOIN pg_class c … WHERE NOT t.tgisinternal`；MySQL `SELECT trigger_name AS name, event_object_table AS table, action_timing AS timing, event_manipulation AS event, action_statement AS body, action_order AS ord FROM information_schema.triggers WHERE trigger_schema = DATABASE()`。
- `catalog()` arm：PG 从 definition 令牌扫 timing/events（纯函数 `pg_trigger_meta(def) -> (timing, events)`，可单测）；body PG 给 null（definition 即全文）；order PG null；enabled PG `tgenabled == 'O'`。
- scope 拒绝清单扩 `Triggers`（裁决 2）；`:32` 文案补 `"triggers"`。

- [ ] **Step 4: env-gated 真库腿**

```rust
#[tokio::test(flavor = "current_thread")]
async fn real_pg_triggers_list() { /* 建表+触发器 → items 非空，timing/events/table 正确 */ }
#[tokio::test(flavor = "current_thread")]
async fn real_mysql_triggers_list() { /* 同上，MySQL 版 */ }
```

- [ ] **Step 5: 跑全模块 + 提交**

Run: `cargo test --release introspect` → 全绿。
Commit: `feat(db): catalog 增 triggers kind——REQ-08 收口\n\nunix@vip.qq.com ai`

---

### Task 6: REQ-09-2 schema 定向（ddlDiff + mysql_show_create scoped）

**Files:**
- Modify: `src/bridge/db_ddl.rs`（`ddl_diff` spec 解析 :261+、表名渲染点全路径）
- Modify: `src/bridge/introspect.rs`（`mysql_show_create` :227–236 白名单）
- Modify: `src/bridge/db_introspect.rs`（MySQL Ddl scope 拒绝 :51–54 改透传）

**Interfaces:**
- Consumes: `quote_ident`（:29）；`mysql_show_create` 调用点（db_introspect.rs:51–67）。
- Produces: spec 新增可选 `schema` 段（独立字段，REQ-09 §4.5 形态）；表引用渲染为 `{q(schema)}.{q(name)}`；MySQL `forSchema(s).catalog("ddl", t)` 可用。

- [ ] **Step 1: RED**

```rust
#[test]
fn schema_field_two_part_quoting_pg_mysql() {
    let spec = json!({"objectType":"table","schema":"public","name":"t","from":null,
      "to":{"columns":[{"name":"id","nativeType":"int4"}]}});
    let (pg, my) = both(spec);
    assert!(pg["statements"][0].as_str().unwrap().contains("\"public\".\"t\""));
    assert!(my["statements"][0].as_str().unwrap().contains("`public`.`t`"));
    // 缺省 schema 行为零变化由既有测试钉住
}
#[test]
fn schema_segment_nul_rejected() { /* schema 含 NUL → Err 含 "schema" */ }
```

- [ ] **Step 2: 跑测试看失败**

Run: `cargo test --release db_ddl::tests::schema`
Expected: FAIL（schema 键被忽略，渲染 `"t"`）。

- [ ] **Step 3: GREEN（db_ddl.rs）**

spec 解析可选 `schema: String`（过 `quote_ident`，Err `ddlDiff: invalid schema`）；拼 `table_ref = quote(schema)? + "." + quote(name)?`，alter/create 两路径所有语句渲染点替换裸 `{t}`（diff 输出键仍只用裸名，零变化）。

- [ ] **Step 4: RED + GREEN（mysql_show_create scoped）**

- 守卫测试 `mysql_show_create_whitelist`（:1243）扩：`s.t` / `` ` `` 反引号形态之外的两段 `[A-Za-z0-9_$]+\.[A-Za-z0-9_$]+` 放行。
- op MySQL Ddl：scope 存在 → scope 过白名单 → `arg = format!("{scope}.{arg}")`；**仅 Ddl** 放开 scope（其他 kind 的 scope 拒绝语义不动）。

- [ ] **Step 5: env-gated 真库腿 + 提交**

```rust
#[tokio::test(flavor = "current_thread")]
async fn real_mysql_scoped_ddl_returns_create() { /* forSchema("ojtest").catalog("ddl","t") → 非 400，ddl 含 CREATE TABLE */ }
```

Run: `cargo test --release db_ddl introspect` → 全绿。
Commit: `feat(db): REQ-09 schema 定向——ddlDiff schema 段两段引用 + MySQL scoped ddl\n\nunix@vip.qq.com ai`

---

### Task 7: REQ-07b——bytea/blob/binary/varbinary 一律 cellenv 信封

**Files:**
- Modify: `plugins/oj-db-postgres/src/lib.rs`（`column_json_pg` :452–496、Any 路径 `column_json` :354–385 与 `row_to_json` 循环 :342–352、测试）
- Modify: `plugins/oj-db-mysql/src/lib.rs`（`column_json_mysql` :264–329 的 Vec<u8> 臂 :321–326、测试）

**Interfaces:**
- Consumes: `oj_plugin_ffi::cellenv::encode`（已存在）；批次 1 的 `X-OJ-Degraded` 扫描自动覆盖新信封（`degraded_of` 扫 `$<type>:`，bytea/blob 列入清单零 host 改动）。
- Produces: PG `BYTEA` 列（typed + Any 两路）与 MySQL `BLOB/BINARY/VARBINARY` 族（Vec<u8> 探针臂）恒定 `$<type>:<base64>`，**含合法 UTF-8 内容**；TEXT 列行为不变。

- [ ] **Step 1: RED env-gated 真库腿**

```rust
#[tokio::test(flavor = "current_thread")]
async fn real_pg_bytea_always_cellenv() {
    // OJ_TEST_PG；'\x68656c6c6f'::bytea（合法 UTF-8）→ cellenv::decode(v) == Some(("bytea", _))
    // 且 v != "hello"；'\xDE\xAD' 非 UTF-8 → 同形态；stream 路径同断言
}
#[tokio::test(flavor = "current_thread")]
async fn real_mysql_binary_columns_cellenv() {
    // OJ_TEST_MYSQL；建 t(b blob, vb varbinary(8), bn binary(4), t text)
    // b/vb/bn → decode 命中且 type ∈ {blob,varbinary,binary}（utf8 内容也信封）；
    // t 列保持可读裸字符串
}
```

- [ ] **Step 2: 跑测试看失败**

Run: `cargo test --release -p oj-db-postgres real_pg_bytea` / `cargo test --release -p oj-db-mysql real_mysql_binary`
Expected: FAIL（今天两条路径都是 `from_utf8_lossy` 裸串）。

- [ ] **Step 3: GREEN（PG）**

`row_to_json_pg` 循环内（或 `column_json_pg` 头部）：`col.type_info().name() == "BYTEA"` → `try_get::<Option<Vec<u8>>>` → `Some(b)` 走 `cellenv::encode(col.type_info().name(), &b)`、`None` → Null（短路与原探针链互斥，探针链的 Vec<u8> lossy 臂随之不再可达 BYTEA）。Any 路径 `row_to_json` 循环同款短路（开放项①的登记：Any 路径有 type_info 可用，本次一并落地）。

- [ ] **Step 4: GREEN（MySQL）**

`column_json_mysql` 的 Vec<u8> 臂（:321–326）`from_utf8_lossy` → `cellenv::encode(col.type_info().name(), &b)`；**不前置 name 短路**——sqlx 对 binary charset 拒 String 解码，TEXT 永不落入该臂（裁决 5；注释写明判别依据）。MySQL Any 路径不动（裁决 6，devkit 登记）。

- [ ] **Step 5: 回归 + 提交**

Run: `cargo test --release -p oj-db-postgres -p oj-db-mysql` → 全绿（类型矩阵 `real_pg_dialect_types_decode_readable` :2206、`real_mysql_dialect_types_decode_readable` :2096 不回归）。
Commit: `feat(db-plugins)!: REQ-07b——bytea/blob/binary 一律 cellenv 信封（breaking：合法 UTF-8 二进制不再裸串）\n\nunix@vip.qq.com ai`

---

### Task 8: release v0.1.112——CHANGELOG + devkit 四件 + 归置

**Files:**
- Modify: `CHANGELOG.md`、`oj/Cargo.toml`（0.1.111 → 0.1.112）
- Modify: `docs/devkit/api-manual.md`（catalog kinds 联合、ddlDiff `previousName`/`warnings`/`renamed`、cellenv 二进制契约与两方言判别依据、错误/限制表）
- Modify: `docs/devkit/SKILL.md`（陷阱速查：rename 需 previousName、TEXT 不撞信封的判别、低权限 SHOW CREATE VIEW）
- Modify: `docs/devkit/scenarios.md`（可照抄场景：改名不动数据、结构 diff 告警消费、processlist 排障）
- Modify: `docs/devkit/README.md`（索引）

**Interfaces:** 消费全部前序任务的对外行为变更。

- [ ] **Step 1: CHANGELOG + 版本号**

v0.1.112 段：feat（REQ-13 rename、KILL 分类、processlist/users/roles、routines 元数据、ddl 路由、视图补壳、triggers、truncated、schema 定向）+ **breaking**（REQ-07b，点名合法 UTF-8 bytea/blob 行为翻转）。

- [ ] **Step 2: devkit 四件逐条对齐**（每用户可见变更进 api-manual 对应章节及错误/限制表；新 trap 进 SKILL；场景进 scenarios；索引进 README）

- [ ] **Step 3: 归置 + 契约校验**

```bash
cargo xtask build
cargo test --release -p xtask   # devkit 产物与源一致
cargo test --release --workspace 2>&1 | tail -5
cargo clippy --release --workspace --all-targets -- -D warnings
cargo fmt
```

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "release: v0.1.112——批次 2（rename/processlist/REQ-08/schema 定向/REQ-07b 二进制信封）

unix@vip.qq.com ai"
```

---

## Self-Review 记录

- **Spec coverage**：批次 2 五项全有任务；REQ-09-3（四类对象进拼装器）按范围检查**另立计划**；REQ-10 的 health/poolStats/SSH/TLS 非一期（spec 总排期已剔）；REQ-07b 的 `db.stream` 汇总——stream 与 pool 共用行解码 helpers，T7 落点即覆盖（实现时核对 stream 调用路径并登记）。
- **Review Focus 钉扎**：F1→T1 Step 3/5；F2→T7 Step 1 MySQL 腿；F3→T3/T5 守卫；F4→devkit Step 2；F5→开放项登记 devkit。
- **类型一致**：`catalog()` 元组返回在 T3 定义、T4/T5 消费；`warnings`/`renamed`/`truncated`/`previousName` 键名全文一致。
