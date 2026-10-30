# dbx 缺口第一批（BUG-02 + REQ-07a）实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 修掉 MySQL 约束串味（BUG-02），并让未解码类型从「整条查询 400」降级为带标记的前缀标量 + 语句级 `degraded` 汇总（REQ-07a），发布为 oj v0.1.111。

**Architecture:** 插件侧在 typed 行解码 try 链全 miss 后，`try_get_raw().as_bytes()` 取协议原始字节，编码为 `$<type>:<base64>` 标量（共享 grammar 落在 `oj-plugin-ffi::cellenv`，宿主/两插件同源）；宿主侧扫描行集推导去重的 `degraded` 清单，经 `X-OJ-Degraded` 响应头（`X-OJ-Row-Limit` 先例）与 export 任务状态外发。非流式 `query` 裸数组的线型不变——**零 ABI**。

**Tech Stack:** Rust 2024 / sqlx 0.9 / deno_core op2 / base64 0.23（workspace 已有）/ oj-plugin-ffi 共享契约 crate。

**Spec:** `docs/superpowers/specs/2026-10-30-dbx-gap-triage-design.md`（第一批章节 + 评审记录；冲突裁决与开放项以 spec 为准）。

## Global Constraints

- **禁止 debug 构建**：`cargo build --release`；测试一律 `cargo test --release`（裸 `cargo test` 不支持）。
- 异步测试 `tokio::test(flavor = "current_thread")`；条件等待用 deadline/有界轮询，**禁止固定 sleep**。
- env-gated 真库测试写法：`let Ok(dsn) = std::env::var("OJ_TEST_PG") else { eprintln!("skip: …"); return; };`（本机容器 DSN 见 memory：PG `postgres://postgres:postgres@localhost:55432/ojtest`，MySQL `mysql://root:root@localhost:53306/ojtest`，以 memory `local-test-db-containers` 为准）。
- 跑 cargo test 接管道必须 `set -o pipefail`，且不可并发两个 cargo test（memory `cargo-test-pipeline-exit-codes`）。
- fail-loud 红线：降级**必须**伴随语句级 `degraded` 汇总（spec 架构师条款）；拿原始字节失败时保留既有 `undecodable_column` 整行错误，**不得**产出空信封。
- 版本发布固定动作：CHANGELOG.md 条目 + `oj/Cargo.toml` version 递增提交 + docs/devkit 四件同步 + `cargo xtask build` + `cargo test --release -p xtask`（devkit 契约用例）。**新增对外契约**（`$<type>:` 前缀、`X-OJ-Degraded` 头）必须进 api-manual 及错误/限制表 + SKILL.md 陷阱速查。
- commit message 结尾：`unix@vip.qq.com ai`。
- `bootstrap.js` 7-bit ASCII（本批不触碰，仅提醒）。

## Review Focus（spec 隐含但未直说的五个咬人点）

1. **TEXT 列字面量撞前缀**：某 TEXT 列值恰为 `'$inet:…'` → 扫描误报进 `degraded`。缓解：api-manual 写明「degraded 清单是权威，前缀只是单元格编码」；parser 拒绝非法 type 字符（含 `$`，`$oj$i64` 不会误判）。
2. **降级值流进 export CSV**：单元格是字符串 `$inet:…`，原样进 CSV 不算错值但消费方需查 `status.degraded`——T6 落地该字段，T7 写进 db-guide。
3. **jsnum 出口护栏与信封串交互**：`sanitize_rows` 不得改动 `$…` 字符串（非数字）。T5 用例钉住。
4. **degraded 去重**：同列多行、同列多 type（不可能但防御）、多列同 type——输出按 `{col,type}` 去重。T2 用例钉住。
5. **`try_get_raw` 兜底可用性**：PG 对已识别 OID（INET 等）可能解得出但 try 链没接——若某类型连原始字节都拿不到，必须回落整行错误而非空信封（红线）。T3 env-gated 矩阵覆盖 inet/interval/money/point。

## 阶段跟踪

| 阶段 | 任务 | 故事 | 状态 |
|---|---|---|---|
| 1 | T1 BUG-02 约束串味 | S1 | ⬜ |
| 2 | T2 cellenv 共享契约 | S2 | ⬜ |
| 3 | T3 PG 插件降级 | S3 | ⬜ |
| 4 | T4 MySQL 插件降级 | S4 | ⬜ |
| 5 | T5 宿主头外发 + T6 export 标记 | S5 | ⬜ |
| 6 | T7 文档与发布 | — | ⬜ |

---

### Task 1（阶段 1）: BUG-02 · MySQL constraints JOIN 补 table_name

**故事 S1（BDD）**：作为 dbx 用户，当我查看 `t_child` 的约束时，应只看到 `t_child` 自己的列，以便结构对比/编辑读到正确结构。
**INVEST**：Independent ✅（纯 SQL 常量）/ Negotiable ✅ / Valuable ✅（数据正确性）/ Estimable ✅ / Small ✅（0.5d）/ Testable ✅（守卫 + 真库）。

**Files:**
- Modify: `src/bridge/introspect.rs:600-609`（MySql Constraints 的 SQL 常量）
- Test: `src/bridge/introspect.rs` 既有 `mod tests`（守卫测试风格同 `:1207` `pg_stitch_constants_guard`）

**Interfaces:**
- Produces: 无新接口；SQL 不变更参数位（仍 1 个 `?`）、不新增常量。

- [ ] **Step 1: 写失败守卫测试**

在 introspect.rs tests 内新增：

```rust
#[test]
fn mysql_constraints_join_guard() {
    let sql = sql(Kind::Constraints, Dialect::MySql);
    assert!(sql.contains("kcu.table_name = tc.table_name"),
        "BUG-02: JOIN 必须含 table_name 关联，否则全库同名约束串味");
}
```

（按既有守卫测试的实际辅助函数签名调整 `sql()` 的引用方式。）

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --release -p only-js mysql_constraints_join_guard`
Expected: FAIL（断言不成立）

- [ ] **Step 3: 修 SQL**

`introspect.rs:604-606` JOIN 条件补 `AND kcu.table_name = tc.table_name`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --release -p only-js mysql_constraints_join_guard`
Expected: PASS

- [ ] **Step 5: env-gated 真库回归**

在 introspect.rs tests 内新增（风格同 `pg_stitch_constants_guard` 旁的真库用例）：

```rust
/// BUG-02 env-gated（OJ_TEST_MYSQL）：同库两表各建 PK/UNIQUE，catalog("constraints")
/// 不得互串；跨库 scope 不混入异库同名约束。
#[tokio::test(flavor = "current_thread")]
async fn real_mysql_constraints_no_cross_table_leak() { … }
```

建表 `t_b02_a(id int PRIMARY KEY, code int UNIQUE)`、`t_b02_b(id int PRIMARY KEY, name varchar(8))`，
断言 `t_b02_b` 的约束列只含 `id`（长度 1）、`t_b02_a` 的 UNIQUE 列只含 `code`。
DSN 取自 `OJ_TEST_MYSQL`，unset 即 skip（诚实登记，不 panic）。

Run: `OJ_TEST_MYSQL='mysql://root:root@localhost:53306/ojtest' cargo test --release -p only-js real_mysql_constraints_no_cross_table_leak -- --nocapture`
Expected: PASS（或本机无容器时 skip 输出，登记待真机跑）

- [ ] **Step 6: Commit**

```bash
git add src/bridge/introspect.rs
git commit -m "fix(db): BUG-02 MySQL catalog(constraints) 跨表串味——JOIN 补 kcu.table_name

unix@vip.qq.com ai"
```

**阶段 1 小结**：________（执行时填写：实际耗时、真库腿结果、偏离点）

---

### Task 2（阶段 2）: `oj-plugin-ffi::cellenv` —— `$<type>:<base64>` 共享契约

**故事 S2（BDD）**：作为宿主与两 db 插件的共同约定，当我编码/解析一个降级单元格时，应走同一套 grammar，以便编码方（插件）与解码/扫描方（宿主）永不分叉。
**INVEST**：全 ✅（0.5d；纯函数模块，可独立测）。

**Files:**
- Create: `oj-plugin-ffi/src/cellenv.rs`
- Modify: `oj-plugin-ffi/src/lib.rs`（`pub mod cellenv;` 挂到 jsint 旁）
- Modify: `oj-plugin-ffi/Cargo.toml`（加 `base64 = "0.23.1"`，workspace 既有版本）

**Interfaces:**
- Produces:
  - `pub fn encode(type_name: &str, bytes: &[u8]) -> String` —— `$<type>:<base64>`；type 名小写化、非 `[a-z0-9_]+` 字符（含 `$`/大写）就地拒绝（debug_assert 级 panic 不可接受——返回前过滤或 assert 由调用方保证；**约定：调用方传 `col.type_info().name()`，模块内 `to_ascii_lowercase` 后校验，非法字符替换为 `_`**，绝不 panic 跨 FFI）。
  - `pub fn decode(s: &str) -> Option<(&str, &str)>` —— 识别信封返回 `(type, payload)`；非信封（无前导 `$`、type 段含非法字符或无 `:`）返回 `None`。
  - `pub fn degraded_of(rows: &[serde_json::Value]) -> Vec<serde_json::Value>` —— 扫所有行所有**字符串**值，`decode` 命中则收 `(col_name, type)`，按 `{col,type}` 去重，输出 `serde_json::json!({"col":…,"type":…})` 数组（列名取自所在对象的 key；非对象行跳过）。
- Consumes: 无。

- [ ] **Step 1: 写失败测试**

cellenv.rs 内 `#[cfg(test)] mod tests`：

```rust
#[test]
fn encode_lowercases_and_wraps() {
    assert_eq!(encode("INET", &[1, 2]), "$inet:AQI=");
}
#[test]
fn decode_roundtrip_and_rejections() {
    assert_eq!(decode("$inet:AQI="), Some(("inet", "AQI=")));
    assert_eq!(decode("plain"), None);
    assert_eq!(decode("$oj$i64"), None);          // jsint 标记无冒号且含 $
    assert_eq!(decode("$IN ET:x"), None);         // 非法字符
    assert_eq!(decode("$inet"), None);            // 无冒号
}
#[test]
fn degraded_of_dedups_by_col_and_type() {
    let rows = vec![
        serde_json::json!({"ip": "$inet:AQI=", "name": "ok"}),
        serde_json::json!({"ip": "$inet:AgM=", "name": "$inet:not-really"}),
        serde_json::json!({"g": "$geometry:AAAA"}),
    ];
    let d = degraded_of(&rows);
    assert_eq!(d, serde_json::json!([
        {"col": "ip", "type": "inet"},
        {"col": "name", "type": "inet"},
        {"col": "g", "type": "geometry"}
    ]));
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --release -p oj-plugin-ffi cellenv`
Expected: FAIL（模块不存在）

- [ ] **Step 3: 实现模块**

grammar 常量一处定义：`$` + `[a-z0-9_]+` + `:` + base64(STANDARD)。`degraded_of` 遍历对象 map 的 string 值。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --release -p oj-plugin-ffi cellenv`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add oj-plugin-ffi/
git commit -m "feat(ffi): cellenv——\$<type>:<base64> 降级单元格共享契约（encode/decode/degraded_of）

unix@vip.qq.com ai"
```

**阶段 2 小结**：________

---

### Task 3（阶段 3）: oj-db-postgres 未解码类型降级

**故事 S3（BDD）**：作为 dbx 用户，当表里有一个 `inet` 列时，`SELECT *` 应返回 `$inet:<base64>` 标量并带 `degraded` 汇总，以便整表可用而不是整条 400。
**INVEST**：全 ✅（1d；依赖 T2 的 encode）。

**Files:**
- Modify: `plugins/oj-db-postgres/src/lib.rs`（`row_to_json_pg` / `column_json_pg` 失败臂，约 `:413-500`；stream 行转换若用独立函数同样点）
- Test: 同文件 tests（env-gated `OJ_TEST_PG`，风格同 `:2138` `real_pg_dialect_types_decode_readable`）

**Interfaces:**
- Consumes: `oj_plugin_ffi::cellenv::encode(type_name, bytes)`
- Produces: 无新签名——`row_to_json_pg` 返回值形态不变（`Result<Value, String>`），仅 Err 变少。

- [ ] **Step 1: 写失败测试（env-gated 类型矩阵）**

```rust
/// REQ-07a env-gated（OJ_TEST_PG）：未解码类型不再整条 400——inet/interval/money/point
/// 返回 $<type>:<base64> 前缀标量；原始字节拿不到的类型保持 fail-loud。
#[tokio::test(flavor = "current_thread")]
async fn real_pg_undecoded_types_degrade_to_cellenv() {
    let Ok(url) = std::env::var("OJ_TEST_PG") else { eprintln!("skip"); return };
    // SELECT '1.2.3.4'::inet AS ip, '1 mon'::interval AS iv, '$1.50'::money AS m
    // 断言：不报错；每列值均为字符串且 oj_plugin_ffi::cellenv::decode 命中、type 为 inet/interval/money
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `OJ_TEST_PG='postgres://postgres:postgres@localhost:55432/ojtest' cargo test --release -p oj-db-postgres real_pg_undecoded_types_degrade_to_cellenv -- --nocapture`
Expected: FAIL（当前 400：`does not decode yet`）

- [ ] **Step 3: 实现降级**

`row_to_json_pg` 中 `column_json_pg` 返回 `None` 的列：改调

```rust
let raw = row.try_get_raw(ordinal).ok().and_then(|v| v.as_bytes().ok());
match raw {
    Some(b) => cellenv::encode(&col.type_info().name().to_ascii_lowercase(), b).into(),
    None => return Err(undecodable_column(&name, &col.type_info().name())),
}
```

要点：① `try_get_raw`/`as_bytes` 任一失败 → 维持原 `undecodable_column` 整行错误（红线，不产空信封）；② Any 池路径（`row_to_json`，sqlite 测试腿）**不改**——sqlite 无未解码类型，维持 400 语义；③ stream 行转换若走独立函数（先 grep `row_to_json_pg` 全部调用点），同点改；改前确认，改后在本任务说明实际动到的函数清单。

- [ ] **Step 4: 跑测试确认通过 + 既有类型矩阵不回退**

Run: `OJ_TEST_PG='postgres://postgres:postgres@localhost:55432/ojtest' cargo test --release -p oj-db-postgres -- --nocapture`（整包，含 `real_pg_dialect_types_decode_readable` 等既有矩阵）
Expected: 全 PASS；`point` 若原先钉「仍不支持」（`:2002` 注释），现行为变为降级——**该用例断言需同步改为「降级标量」**，这是行为变更登记点。

- [ ] **Step 5: Commit**

```bash
git add plugins/oj-db-postgres/
git commit -m "feat(db-postgres): REQ-07a 未解码类型降级——\$<type>:<base64> 前缀标量替代整条 400

unix@vip.qq.com ai"
```

**阶段 3 小结**：________（必填：grep 到的 row_to_json_pg 调用点清单、point 用例改法、Any 路径未动的理由确认）

---

### Task 4（阶段 4）: oj-db-mysql 未解码类型降级

**故事 S4（BDD）**：同上，MySQL `GEOMETRY` 族列不再让 `SELECT *` 整体 400。
**INVEST**：全 ✅（0.5-1d；T3 已趟路）。

**Files:**
- Modify: `plugins/oj-db-mysql/src/lib.rs`（`row_to_json_mysql` / 对应 `column_json_*` 失败臂，undecodable 错误点在 `:224` 一带）
- Test: 同文件 tests（env-gated `OJ_TEST_MYSQL`）

**Interfaces:**
- Consumes: T2 `encode`、T3 的改法（同构）
- Produces: 无新签名。

- [ ] **Step 1: 写失败测试（env-gated）**

```rust
/// REQ-07a env-gated（OJ_TEST_MYSQL）：GEOMETRY 列 SELECT 不再 400，返回 $geometry:<base64>。
#[tokio::test(flavor = "current_thread")]
async fn real_mysql_geometry_degrades_to_cellenv() {
    let Ok(url) = std::env::var("OJ_TEST_MYSQL") else { eprintln!("skip"); return };
    // CREATE TABLE IF NOT EXISTS t_req07a_geo (id int, g GEOMETRY);
    // INSERT 一行 ST_GeomFromText('POINT(1 1)'); SELECT 断言 cellenv::decode 命中 type=geometry
}
```

（落库对象 `t_req07a_geo` 用例内自清理 `DROP TABLE IF EXISTS`。）

- [ ] **Step 2: 跑测试确认失败**

Run: `OJ_TEST_MYSQL='mysql://root:root@localhost:53306/ojtest' cargo test --release -p oj-db-mysql real_mysql_geometry_degrades_to_cellenv -- --nocapture`
Expected: FAIL（当前 400）

- [ ] **Step 3: 实现**

镜像 T3：try 链全 miss 的列 → `try_get_raw(ordinal).as_bytes()` → `encode(小写 type 名, bytes)`；拿不到原始字节维持 `undecodable_column`。先 grep 确认 mysql 插件的 typed/Any 池形态（`Pooled` 枚举臂），**typed 臂（生产）与 Any 臂（如存在且可拿到 raw）都接**；Any 臂拿不到 raw 时维持错误。

- [ ] **Step 4: 跑测试 + 整包回归**

Run: `OJ_TEST_MYSQL='mysql://root:root@localhost:53306/ojtest' cargo test --release -p oj-db-mysql -- --nocapture`
Expected: 全 PASS；既有「GEOMETRY 仍不支持」类钉子用例同步改为降级断言（行为变更登记）。

- [ ] **Step 5: Commit**

```bash
git add plugins/oj-db-mysql/
git commit -m "feat(db-mysql): REQ-07a GEOMETRY 族降级——\$<type>:<base64> 前缀标量替代整条 400

unix@vip.qq.com ai"
```

**阶段 4 小结**：________

---

### Task 5（阶段 5a）: 宿主 `X-OJ-Degraded` 响应头外发

**故事 S5（BDD）**：作为 dbx 前端，当一次查询结果含降级单元格时，我应在响应头里拿到去重的 `degraded` 清单，以便权威判定哪些列被降级（前缀字面量撞车时不误判）。
**INVEST**：全 ✅（0.5-1d）。

**Files:**
- Modify: `src/bridge/db_query.rs:161-175`（`query_impl` 出口，`sanitize_rows` 后、`Ok(rows)` 前）
- Modify: `src/bridge/query.rs:2345` 一带（`op_db_query_build` 的 `ExecRoute::Rows` 成功臂，`Ok(Value::Array(r))` 前）
- Test: `src/bridge/db_tests.rs`（op 级，M0 fake accessor 风格）+ `src/bridge/cellenv.rs` 无（扫描已在 T2 钉）

**Interfaces:**
- Consumes: `oj_plugin_ffi::cellenv::degraded_of`；`ReqState.headers.push`（`db_query.rs:139` 先例）
- Produces: 响应头 `X-OJ-Degraded: <compact JSON [{"col","type"}]>`（仅在非空时推）；无 JS API 变更。

- [ ] **Step 1: 写失败测试（op 级，fake accessor）**

在 db_tests.rs 照既有 M0 fake（`src/bridge/db.rs:249` 一带的内存实现）注册一个返回 `[{"ip": "$inet:AQI="}]` 的 accessor，跑 `op_db_query`，断言 `ReqState.headers` 含 `("X-OJ-Degraded", …)` 且值为 `[{"col":"ip","type":"inet"}]`；再断言 `sanitize_rows` 后信封串原样未动（Review Focus #3）。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --release -p only-js <新用例名>`
Expected: FAIL（无头）

- [ ] **Step 3: 实现两处出口**

两处同构插入（抽不抽小函数由执行者定，两处以上重复≥3 行才抽；当前恰 2 处，内联可接受——DRY 的判定线是 cellenv 共享，不是这两行 push）：

```rust
let d = oj_plugin_ffi::cellenv::degraded_of(&rows);
if !d.is_empty() {
    state.borrow_mut().borrow_mut::<super::ReqState>()
        .headers.push(("X-OJ-Degraded".into(), serde_json::to_string(&d).unwrap()));
}
```

builder 臂处 `state` 可用性按现场变量名调整（query.rs 该臂处 state 在作用域内，若不在则以 trace_sql 的调用形态对齐）。

- [ ] **Step 4: 跑测试 + 相关回归**

Run: `cargo test --release -p only-js`（整包，含 db_tests/mod.rs 的 op 用例）
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/bridge/db_query.rs src/bridge/query.rs src/bridge/db_tests.rs
git commit -m "feat(bridge): X-OJ-Degraded 响应头——语句级降级清单外发（db.query 裸/构造器两路）

unix@vip.qq.com ai"
```

---

### Task 6（阶段 5b）: db.export 产物降级标记

**故事 S6（BDD）**：作为 dbx 用户，当我导出含 `inet` 列的表时，导出任务状态里应有 `degraded` 清单，以便 CSV 消费方知道哪些列是编码值。
**INVEST**：全 ✅（0.5d）。

**Files:**
- Modify: `src/bridge/db_export.rs`（任务 entry 结构加 `degraded: Vec<Value>` 字段；pump 写行循环处累积 `degraded_of`；`status()` 的 `json!` 加 `"degraded": e.degraded`，约 `:290`）
- Test: `src/bridge/db_export.rs` tests（既有 export e2e 风格 `:862`）

- [ ] **Step 1: 写失败测试**

fake 行含信封的 export 用例：`exportStatus` 返回 JSON 含 `"degraded":[{"col":"ip","type":"inet"}]`；无信封的对照用例断言 `"degraded":[]`。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --release -p only-js <export degraded 用例名>`
Expected: FAIL（status 无该键）

- [ ] **Step 3: 实现**

entry 结构加字段（默认值空 Vec，不影响既有构造点——grep 所有构造处补字段）；pump 每批行 `degraded_of` 去重并入（借用 cellenv 的去重语义：简单 extend + 最终 status 输出前去重，或维护 `BTreeSet<(String,String)>`）。CSV/JSONL 产物**不改**（单元格字符串原样，Review Focus #2 口径）。

- [ ] **Step 4: 跑测试 + export 全回归**

Run: `cargo test --release -p only-js export`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/bridge/db_export.rs
git commit -m "feat(export): 任务状态携带 degraded 清单（降级列权威标记）

unix@vip.qq.com ai"
```

**阶段 5 小结**：________

---

### Task 7（阶段 6）: 文档四件 + 发布 v0.1.111

**故事**：作为 oj 的用户/下游，当 v0.1.111 发布时，我应在 devkit 文档与 CHANGELOG 读到新契约的全部语义与行为变更，以便一次性改对读侧。
**INVEST**：全 ✅（0.5-1d）。

**Files:**
- Modify: `CHANGELOG.md`（v0.1.111 段：BUG-02 + REQ-07a，行为变更如实写「未解码类型 400 → 降级标量 + X-OJ-Degraded 头」）
- Modify: `oj/Cargo.toml`（version 0.1.110 → 0.1.111）
- Modify: `docs/devkit/api-manual.md`（新契约进对应章节**及错误/限制表**：`$<type>:` 保留前缀 grammar、判别按列声明类型、degraded 头为权威、TEXT 字面量撞前缀的判定路径、Any/sqlite 路径仍 400 的边界）
- Modify: `docs/devkit/SKILL.md`（陷阱速查：① 消费方判定降级列**只看 X-OJ-Degraded 头 / export status.degraded**，不要信前缀嗅探；② 前缀串会原样进 CSV）
- Modify: `docs/devkit/db-guide.md` §10.6（读侧口径同步：`undecodable_column` 错误只剩「原始字节也拿不到」的窄场景）
- Modify: `docs/devkit/scenarios.md`（若有三行可照抄的消费示例则加，没有则不动）、`docs/devkit/README.md`（四件清单勾稽）
- 产出物：bin/devkit/ 由 `cargo xtask build` 重新归置

- [ ] **Step 1: 跑 devkit 契约基线**

Run: `cargo test --release -p xtask`
Expected: PASS（改动前基线绿）

- [ ] **Step 2: 写文档（内容按上面 Files 清单；api-manual 的错误/限制表新增一行「未解码类型」）**

- [ ] **Step 3: 版本递增 + CHANGELOG + 归置 + 契约校验**

```bash
# oj/Cargo.toml version → 0.1.111
cargo fmt
cargo clippy --release --workspace --all-targets -- -D warnings
cargo test --release --workspace
cargo xtask build        # 归置 bin/oj + bin/plugins + bin/devkit
cargo test --release -p xtask   # devkit 契约用例校验产物与源一致
```

Expected: 全绿（workspace 测试在无真库 env 时 honest skip）

- [ ] **Step 4: Commit（版本分界提交）**

```bash
git add -A
git commit -m "release: v0.1.111——BUG-02 约束串味修复 + REQ-07a 未解码类型降级契约

unix@vip.qq.com ai"
```

**阶段 6 小结**：________

---

## 自检记录（writing-plans 自检清单）

1. **Spec 覆盖**：第一批 = BUG-02 + REQ-07a + 契约文档——T1/T3/T4 覆盖缺口本体，T2/T5/T6 覆盖架构师条款（degraded 汇总），T7 覆盖红线（devkit/CHANGELOG）。spec 的「Any 池退化为单元格信封」开放项经核实不成立（sqlite 无未解码类型），T3 Step 3 记录为「Any 路径不改」并写明理由。✅
2. **步骤粒度**：每步一个可检动作；无「处理边界情况」类空步。✅
3. **类型一致**：`encode/decode/degraded_of` 签名在 T2 定义，T3-T6 引用一致；头名 `X-OJ-Degraded` 全文一致。✅
4. **Review Focus**：5 点各有归属用例（T2 parser/T6 状态/T5 sanitize/T2 去重/T3 矩阵+红线回落）。✅
5. **比例**：spec 估第一批 1.5d 未计 degraded 外发与文档红线成本；本计划 7 任务约 **4-5d**——差异已在 spec 开放项精神内，T5/T6 是架构师条款的强制部分，非镀金。✅

## 非目标（本计划明确不做）

- 流式 `db.stream` 的降级汇总（流批次线型不动；与 REQ-07b 二进制同批再议）。
- 合法 UTF-8 bytea 的表示变更（REQ-07b，breaking，另批）。
- inet/macaddr/money 手解可读文本（后续演进，前缀契约不变）。
- 任何 `#[repr(C)]` 变更（零 ABI 是硬约束）。
