# REQ-12 批量导入（import / importStatus / importCancel）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 任务式批量导入三件套——`db.import(args) → {taskId}`（blob CSV/JSONL 或内联 rows，多值 INSERT 全参数化分批），`importStatus`（state/rows/errors/bytes/samples 绝对行号），`importCancel`（幂等、批间协作取消）。

**Architecture:** 纯 host 侧、**零 ABI**。镜像 `db_export.rs` 的 ExportRegistry 任务/配额/幂等/清扫模型为新 `db_import.rs`（taskId 前缀 `imp_`）。SQL 渲染复用 sea-query（`build_sql` 每方言 QueryBuilder + `value_to_json` 参数提取，升 `pub(crate)`），PG upsert 用多值 INSERT 直接挂 `ON CONFLICT ... DO UPDATE`（**修正案**：spec §4.3 的临时表路径是 COPY 时代的解法——多值 INSERT 的 ON CONFLICT 不需要临时表，少一次类型映射与往返；语义等价，裁决记 ledger）。守卫不开旁路：import 前置 `import_under_guard` 预检（守卫只能验「某参数=当前 tid」，证明不了导入每行 → scoped 表 fail-closed）+ 合成裸 SQL 走 `check_tenant_raw`/`check_access` 逐档复用既有裁决。

**Tech Stack:** Rust（sqlx Any、sea-query 1.0、tokio）、deno_core op2、bootstrap.js（7-bit ASCII）。

**Spec:** docs/superpowers/specs/2026-11-01-dbx-req12-bulk-import-design.md

## Global Constraints

- **零 ABI**：`ABI_VERSION = 12` 不动；`oj-plugin-ffi` 不改动。
- 目标版本 **v0.1.115**（Task 7 打版；CHANGELOG 标题注明未打标签）。
- **三硬约束写死**：不绕过 db_access op 层；不绕过配额与审计；不绕过租户守卫（镜像矩阵 §4.2）。任何一条不满足宁可不做该功能。
- **SQL 注入红线**：值只走绑定参数；拼进 SQL 文本的只有 `quote_ident` 过的表/列名（必须 `SchemaRegistry::has_column` 白名单命中）与引擎常量。CSV 表头**绝不**作为标识符来源（只作列序匹配）。
- `bootstrap.js` 必须保持 **7-bit ASCII**。
- taskId 前缀 **`imp_`**（与 `exp_`/`exec_`/`tx_` 隔离）；`samples` 上限 **20** 条；format 只注册 `csv`/`jsonl`。
- 所有构建/测试/clippy 一律 `--release`；异步测试 `tokio::test(flavor = "current_thread")`（JsRuntime `!Send`）；e2e 腿用 multi_thread。
- cargo test 接管道必须 `set -o pipefail`，不可并发两个 cargo test。
- env-gated 真库腿：`OJ_TEST_PG=postgres://postgres:postgres@127.0.0.1:5432/ojtest`、`OJ_TEST_MYSQL=mysql://root:root@127.0.0.1:3306/ojtest`（本机 Apple container）；缺 env 跳过不 panic。
- commit message 尾行 `unix@vip.qq.com ai`。

## Review Focus

1. **守卫矩阵边角（三硬约束之三）**：guard=Warn + 未登记表 → 放行+warn（不得被 import 预检误伤）；guard=Deny + 已登记但**非** scoped → 放行（同裸 SQL）；`rs.system`（asSystem）→ 预检与 `check_tenant_raw` 一致跳过——三种形态各钉一腿。
2. **CSV 恶意输入**：引号不闭合 / 嵌入换行 quoted newline / 超长字段——自写解析器必须不 panic、行缓冲有界（reject 不 OOM）。
3. **cancel 与批事务竞态**：cancel 落批间 → 已提交批保留、未提交批不落库；cancel 与最后一批 commit 同刻 → 先 settle 者赢（state 不互相覆盖，同 REQ-11 Review Focus 1 口径）。
4. **绑定上限收敛**：用户 `batchSize: 1_000_000` 或 100 列宽表 → 按 `65535/列数`（PG/MySQL 同）收敛批行数，不报错、参数序正确。
5. **配额边界**：内联 rows 恰 = `max_rows` 放行、`max_rows+1` 登记前拒；blob 源读超 `max_bytes` 中途 failed（错误点名配额键，不静默截断）。

---

### Task 1: `db_import:` 配额段 + 装配接线

**Files:**
- Modify: `src/bridge/db_policy.rs`（`DbImportSection`，放 `DbExportSection` 后）
- Modify: `src/config.rs:1459` 附近（`db_import` 字段，挨着 `db_export`）
- Modify: `oj/src/app.rs:1478-1483` 附近（validate + registry 构造 + Extras 透传，mirror export 三行）
- Modify: `src/bridge/mod.rs`（StableState 加 `import_section`/`import_registry` 两字段 ~line 263；Extras 加 Option 对 ~line 372；构造 ~line 961-965、1026 附近）

**Interfaces:**
- Produces: `db_policy::DbImportSection { global_concurrency: usize(默认4,1..=64), per_subject_concurrency: usize(默认2,1..=16), max_rows: u64(默认1_000_000,≥1), max_bytes: u64(默认512MiB,≥1MiB), default_batch_size: usize(默认5000,100..=50000), ttl_secs: u64(默认86400,>0), sweep_interval_secs: u64(默认3600,>0) }` + `validate() -> Result<(), String>`（Err 文案点名 `db_import.<key>` 同 export 口径）；`StableState.import_section: Arc<DbImportSection>`、`StableState.import_registry: Arc<crate::bridge::db_import::ImportRegistry>`（T2 定义，本任务先用 `db_import::ImportRegistry::default()` 占位编译——T2 落地后同形替换，与 export 的 `unwrap_or_else(new)` 模式一致）。

- [ ] **Step 1: 写失败测试**——`src/bridge/db_policy.rs` tests：`import_section_parses_defaults_and_validates`——`{}` 解析得默认值快照断言；`global_concurrency: 0` / `default_batch_size: 99` / `max_bytes: 1024` 各 `validate()` Err 且文案含 `db_import.` 前缀与键名。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --release -p only-js import_section`
Expected: FAIL（`DbImportSection` 未定义）

- [ ] **Step 3: 实现**——`DbImportSection` 镜像 `DbExportSection`（derive `Debug, Clone, Deserialize`、`#[serde(default, deny_unknown_fields)]`、`Default`、`validate`）；`src/config.rs` 加 `pub db_import: crate::bridge::db_policy::DbImportSection`（serde default）；`src/bridge/mod.rs` 四处接线（字段/Extras/构造，照抄 export 相邻行）；`oj/src/app.rs` 照 export 三行加 validate + `Arc::new(cfg.db_import.clone())` + Extras 透传（import_registry 先 `Default::default()`）。

- [ ] **Step 4: 跑测试确认通过 + 全 crate 编译**

Run: `cargo test --release -p only-js import_section && cargo build --release`
Expected: PASS 3/3；build Finished。

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(db): db_import 配额段 + 装配接线（REQ-12 T1）

unix@vip.qq.com ai"
```

---

### Task 2: `ImportRegistry`——任务登记/配额/状态/取消/清扫

**Files:**
- Create: `src/bridge/db_import.rs`（本任务先落 registry 与 entry；ops/consumer 在 T5/T6）
- Modify: `src/bridge/mod.rs`（`pub mod db_import;` + T1 占位处换 `ImportRegistry::new(&import_section)`）

**Interfaces:**
- Consumes: `DbImportSection`（T1）。
- Produces（T5/T6 依赖，同名同型）:
  - `enum ImportState { Running, Done, Failed, Cancelled }`（`as_str()` 同 export）
  - `struct ImportSample { line: u64, reason: String, raw: Option<String> }`
  - `enum Finish { Done { rows: u64, bytes: u64 }, Failed { rows: u64, bytes: u64, error: String }, Cancelled { rows: u64, bytes: u64 } }`
  - `enum RegisterError { GlobalFull, SubjectFull }`（文案 `db.import: global import concurrency limit reached…` / `… per-subject …`）
  - `struct ImportEntry { owner, alias, table, state, rows, bytes, error, samples: Vec<ImportSample>(≤20), started_at_ms, finished_at_ms, cancel: Arc<Notify>, cancel_requested }`
  - `struct ImportRegistry { inner: Mutex<HashMap<String, ImportEntry>>, … global/per_subject, reaper_armed }`；`new(&DbImportSection)`、`register(owner, alias, table) -> Result<(String, Arc<Notify>), RegisterError>`（taskId `imp_`+`handle_core::new_boot_tag` 同款）、`progress(&self, id, rows, bytes)`、`push_sample(&self, id, sample)`（截到 20）、`cancel(&self, id, owner) -> Option<bool>`（置 flag + `notify_waiters`——多等待者口径同 REQ-11）、`finish(&self, id, Finish)`、`status(&self, id, owner) -> Option<Value>`（spec §3 逐字段：`taskId,state,alias,table,rows,errors,bytes,startedAtMs,finishedAtMs,error,samples`；`errors` = samples 语义计数需独立字段 `error_count: u64`——entry 加）、`sweep(now_ms, ttl_ms) -> Vec<String>`（完成态超时摘表）、`arm_reaper(Arc<Self>, interval, ttl)`（tokio::spawn 周期 sweep；import 无 blob 产物，扫完即丢）。

- [ ] **Step 1: 写失败测试**——`db_import.rs` `mod tests`：`import_register_quota_global_and_subject`（填到 global 满 → GlobalFull 文案；换 owner 填到 per-subject 满 → SubjectFull 文案；终态释放名额后可再登记）；`import_status_cancel_sweep_roundtrip`（status 字段逐键断言 camelCase；他人 id → None；cancel 幂等两次同返回；finish(Done) 后 sweep 摘表、Running 豁免）。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --release -p only-js import_register_quota`
Expected: FAIL（module 不存在）

- [ ] **Step 3: 实现 registry**——照抄 `db_export.rs:30-367` 的 ExportRegistry 骨架（lock/ms_now/boot/OnceLock 同款），差异：entry 无 blob_key/truncated/degraded，加 `table`/`samples`/`error_count`；status 的 `errors` 输出 `error_count`；reaper 只扫表不删 blob。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --release -p only-js import_`
Expected: PASS（本任务全部新测试）。

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(db): ImportRegistry 登记/配额/状态/取消/清扫（REQ-12 T2）

unix@vip.qq.com ai"
```

---

### Task 3: CSV/JSONL 解析器 + 类型转换（行号权威）

**Files:**
- Modify: `src/bridge/db_import.rs`（`mod parse`）

**Interfaces:**
- Produces（T6 消费）:
  - `struct ParsedRow { values: Vec<Value>, line: u64 }`（`line` = 源文件绝对行号，1-based，header 计 1——blob 腿：物理行号；`header: true` 时数据首行 line=2）
  - `fn parse_csv_line_offsets(buf: &[u8]) -> …` 不对外——对外是迭代器：
  - `struct CsvParser<'a> { … }`：`impl Iterator<Item = Result<ParsedRow, ParseError>>` + `fn new(src: &'a [u8], columns: &[String], header: bool, col_types: &[ColumnType]) -> Result<Self, String>`（header=true 时首行必须与 `columns` 完全一致（逐格比较），不符 → Err 参数错误文案点名表头格与登记列名——**不猜**）
  - `struct JsonlParser<'a>`：同面（每行 `JSON.parse` 等价的 `serde_json::from_slice`；对象键序 = 列序，缺键 → Value::Null，多键 → 参数错误）
  - `enum ParseError { Bad { line: u64, reason: String, raw: String } }`——convert 失败（如 integer 列拿到非数字）走 `Bad` 而非 panic
  - `fn coerce(value: &str, ty: ColumnType) -> Result<Value, String>`：Integer/BigInt→i64 parse、Double→f64、Boolean→`true/false/1/0`、Text/Unknown→String 原样、Blob→原样字符串（base64 不猜，落库错行进 samples）

- [ ] **Step 1: 写失败测试**——纯单测：`csv_quoted_newline_and_escape`（`a,"b\nb",c` 一行三格 + `""` 转义 + `\r\n` 结尾）；`csv_header_mismatch_fails_fast`（表头 `id,name` vs columns `[id,fullname]` → Err 点名两格）；`csv_bad_cell_line_number_absolute`（header=true，第 3 数据行 integer 列给 `abc` → ParseError.line=5, reason 含 `integer`，raw 含原格）；`jsonl_missing_key_null_extra_key_err`；`coerce_boolean_forms`。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --release -p only-js csv_`
Expected: FAIL。

- [ ] **Step 3: 实现**——最小手写解析器（状态机：常态/引号内；三规则：`,` 分隔、`""`→`"`、quoted `\n` 属一格；**不闭合引号到 EOF = 该行 Bad**「unterminated quoted field」不 panic）；行缓冲有界（逐行切片，不累积）；jsonl 逐行 from_slice。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --release -p only-js csv_ && cargo test --release -p only-js jsonl && cargo test --release -p only-js coerce`
Expected: 全 PASS。

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(db): csv/jsonl 最小解析器 + ColumnType 转换（REQ-12 T3）

unix@vip.qq.com ai"
```

---

### Task 4: 方言 SQL 渲染（多值 INSERT / upsert / replace / 批收敛）

**Files:**
- Modify: `src/bridge/db_import.rs`（`mod render`）
- Modify: `src/bridge/query.rs:2619`（`fn value_to_json` 升 `pub(crate)`）

**Interfaces:**
- Produces（T6 消费）:
  - `struct BatchSql { sql: String, params: Vec<Value> }`
  - `enum ImportMode { Append, Upsert { conflict_keys: Vec<String> }, Replace }`
  - `fn render_batch(dialect: Dialect, table: &str, columns: &[String], rows: &[Vec<Value>], mode: &ImportMode, table_pk: &[String]) -> Result<BatchSql, String>`
    - 全方言：多值 `INSERT INTO <t> (cols) VALUES (...),(...)` 经 sea-query（`Query::insert()` 逐行 `.values([])`，`build_sql` 渲染，`value_to_json` 提参；`<t>`/cols 早已过 `has_column` 白名单）
    - PG upsert：SQL 尾追加 ` ON CONFLICT ("k1",…) DO UPDATE SET "c" = "excluded"."c", …`（keys=conflict_keys；SET 列=非 key 列；**纯标识符+常量文本，值不碰**）
    - MySQL upsert：尾追加 ` ON DUPLICATE KEY UPDATE \`c\` = VALUES(\`c\`), …`（要求 `table_pk` 非空，否则 Err `db import: upsert on mysql requires a unique key (primary key) on table '<t>'`）；conflict_keys 仅校验 ⊆ columns
    - sqlite upsert：仅 `conflict_keys == ["rowid"]` 时手渲染 `INSERT OR REPLACE INTO "t" (cols) VALUES (?,…),(?,…)`（? 占位 + quote_ident），否则 Err 参数错误（spec §4.3）
    - Replace 渲染与 mode 无关的 `fn render_delete(dialect, table) -> String`（`DELETE FROM <t>`，T6 批外事务内执行）
  - `fn batch_rows(cap: usize, batch_size: usize, ncols: usize) -> usize`——`min(batch_size, 65535 / max(ncols,1), cap)`（Review Focus 4）

- [ ] **Step 1: 写失败测试**——纯渲染断言：`render_pg_append_multivalue`（2 行 × 2 列 → `INSERT INTO "t" ("a","b") VALUES ($1,$2),($3,$4)` 形态 + params 序）；`render_pg_upsert_on_conflict_clause`（DO UPDATE SET 只含非 key 列、`excluded` 引用正确）；`render_mysql_upsert_requires_pk`（pk 空 → Err）；`render_sqlite_or_replace_only_rowid`；`batch_rows_caps_at_65535`（ncols=10 → ≤6553；ncols=100 → ≤655；user batchSize 1_000_000 → 收敛不报错）。
- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --release -p only-js render_`
Expected: FAIL。

- [ ] **Step 3: 实现 render 模块**——PG `excluded` 引用：`quote_ident` 列名 + `"excluded"` 字面量；ON CONFLICT target 同样 quote_ident。sea-query 不可表达的两段（ON CONFLICT DO UPDATE / ON DUPLICATE KEY UPDATE / OR REPLACE）= 字符串尾缀，**只拼标识符与常量**。`value_to_json` 升 `pub(crate)`。
- [ ] **Step 4: 跑测试确认通过 + sqlite 真腿**——再加 sqlite 真库腿（照 `accessor_sqlx.rs:587` `SqliteAccessor::connect("sqlite::memory:")` 模式）：`sqlite_real_multivalue_insert_and_or_replace`（建表 → render → exec_with_params → query 断言行在；rowid upsert 覆盖）。`tokio::test(flavor = "current_thread")`。

Run: `cargo test --release -p only-js render_ && cargo test --release -p only-js sqlite_real_multivalue`
Expected: 全 PASS。

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(db): 导入批渲染——多值 INSERT/ON CONFLICT/ONDUP/REPLACE + 65535 收敛（REQ-12 T4）

unix@vip.qq.com ai"
```

---

### Task 5: `op_db_import` 同步面——守卫矩阵/配额/登记 + 三 op 全局面

**Files:**
- Modify: `src/bridge/db_import.rs`（三 op + `ImportArgs`）
- Modify: `src/bridge/mod.rs`（`pub mod` 已在 T2；extension op 列表加 `db_import::op_db_import/_status/_cancel`，~line 549 照 export 三行）
- Modify: `src/bridge/bootstrap.js`（import 三行 + op import 三行，**7-bit ASCII**）
- Modify: `sample/global.d.ts`（`DBInstance.import/importStatus/importCancel` 类型）

**Interfaces:**
- Consumes: T2 registry、T3/T4（本任务只用签名类型，consumer 在 T6）、`guard::check_table`、`guard::check_tenant_raw`、`db_policy::check_access`、`db::resolve_target`。
- Produces: `op_db_import(state, name, args: ImportArgs) -> Result<Value, JsErrorBox>`（返 `{taskId}`；consumer 先落最小占位——spawn 一个立即 `finish(Done{0,0})` 的空任务，T6 填真实现）、`op_db_import_status`、`op_db_import_cancel`；JS 面 `db.import/importStatus/importCancel`（挂在 dbCache 实例面，与 export 三行相邻）。
- `ImportArgs`（spec §3 逐字段 serde rename camelCase）：`table, schema?, blob_key?, rows?, format, columns?, mode?(默认append), conflict_keys?, batch_size?, header?(csv默认true), on_error?(默认abort), max_errors?`；`blob_key` 与 `rows` 恰一（都缺/都给 → 参数错误）；`format` 非 csv/jsonl → `format_unsupported` 同款（T6 复用，本任务先内联小函数）。

**守卫裁决顺序（op 内，全部在登记前）**：
1. `guard::check_table(state, &table)`（归属）
2. **import 预检**：`guard != Off && !system && registry.get(table).is_some_and(is_tenant_scoped)` → `Err(OJ_DB_UNSUPPORTED)`：`JsErrorBox` 结构化 `e.data.capability = "import_under_guard"`（复用 `db::to_js_err` 的 structured 路径——找 `unsupported()` helper 同款构造）
3. 合成 SQL 镜像：`let synthetic = match mode { Replace => format!("truncate table {table}"), _ => format!("insert into {table} values (?)") }`；`guard::check_tenant_raw(state, &synthetic, &[])`（Deny+未登记 → 拒「cannot verify tenant scope」同裸 SQL；Warn+未登记 → warn 放行；非 scoped 已登记 → 放行——Review Focus 1 三形态）
4. `db_policy::check_access(state, &alias, &synthetic)`（replace = truncate 分类 → allow_ddl + G9 危险写确认门**按 truncate 同档**——spec §4.2 原话；append/upsert = insert 分类，正常 write 判定）
5. `db::resolve_target` → `Target::Pool(da)`，Tx → Err（同 export「pool only」口径）
6. 配额：`rows` 内联源行数 > `max_rows` → 登记前 Err（Review Focus 5 边界）；`batch_size`/`max_errors` 钳到段上限；`alias` 经 `guard::bound_db`
7. 登记（registry.register）→ 审计 `db.import.start`（detail `task=<前12> mode=<m> format=<f>`，照 export 审计块）→ spawn 占位 consumer → 返 `{taskId}`

- [ ] **Step 1: 写失败测试**——offline（`InMemoryAccessor` + `LocalBlob` fixture，照 `db_export.rs:814-833` 的 `export_bridge` 模式造 `import_bridge`）：
  - `import_guard_scoped_table_fail_closed`：fixture registry 注册含 `tenant_id` 列的表，guard=Warn 与 Deny 两档 → import 均结构化拒，`e.data.capability == "import_under_guard"`
  - `import_guard_unregistered_warn_passes_deny_rejects`：未登记 + Deny → 拒（文案 cannot verify tenant scope）；未登记 + Warn → 放行（任务建起）
  - `import_guard_registered_non_scoped_passes`：已登记无 tenant_id 列 + Deny → 放行
  - `import_as_system_skips_guard`：`rs.system=true` + scoped 表 + Deny → 放行
  - `import_rows_over_quota_rejected_before_register`：`max_rows=2` 传 3 行 → Err 且 registry 无新 entry
  - `import_prefix_isolation`：`importStatus("exp_…")` 与 `exportStatus("imp_…")` 互 not found
  - `import_blob_and_rows_mutex` / `import_bad_format_rejected`

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --release -p only-js import_guard`
Expected: FAIL（op 未注册）。

- [ ] **Step 3: 实现 op 同步面 + 占位 consumer + bootstrap 面 + global.d.ts 类型**。
- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --release -p only-js import_`
Expected: 全 PASS。

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(db): op_db_import 守卫矩阵 + 三 op + JS 全局面（REQ-12 T5）

unix@vip.qq.com ai"
```

---

### Task 6: consumer 执行体——blob 流式读/批事务/abort/skip/maxErrors/cancel

**Files:**
- Modify: `src/bridge/db_import.rs`（consumer 真实现，替换 T5 占位）

**Interfaces:**
- Consumes: T2 registry（progress/push_sample/finish/cancel Notify）、T3 parser、T4 render、`BlobBackend::read_range`（ABI 11，1 MiB 窗口递增 offset 流式读——**不整读内存**）、`DataAccessor::begin/TxSession`。
- Produces: 无新增接口（行为完备）。

**执行语义（spec §4.3/§4.4 落法）**：
- 源：blob → `read_range(key, off, 1MiB)` 递增窗口，解析器跨窗口续行（行缓冲有界：半行暂存 ≤1 行）；`rows` 内联 → 直接迭代。`bytes` 随读随记；超 `max_bytes` → Finish::Failed（点名 `max_bytes`）。
- 批：`n = batch_rows(section.default_batch_size, args.batch_size, ncols)`；**append/upsert = 每批一个事务**（`da.begin()` → render_batch → tx.exec → commit；批内错 → rollback → Finish::Failed，已提交前序批保留，`rows`=已提交）；**replace = 单事务**（begin → render_delete → 各批 exec → commit；任何错 → rollback 全回）。
- **cancel**：批间检查 `cancel_requested`（`Notify::notify_waiters` 已醒）→ 停读、Finish::Cancelled（rows=已提交）；与最后一批 commit 同刻 → entry state 先 settle 者赢（finish 里 Running 才改写，同 export）。
- **skip**：批内逐行 INSERT（render 单行批）——单行错 → `push_sample(ParseError/BatchError{line,reason,raw})` + `error_count+=1` 行不落库，继续；`error_count > max_errors`（默认 10）→ 整批 rollback → Finish::Failed。**abort**：多值批，批错即整批 rollback → Failed。
- sqlite 单事务包住全部批次 = 批收敛后 `begin` 一次（append/upsert 在 sqlite 走「全单事务」特例——spec §4.3「单事务包住全部批次是性能关键」）。
- samples 的 `line` = parser 绝对行号（Review Focus：与源文件一一对应）。

- [ ] **Step 1: 写失败测试**：
  - `import_blob_csv_happy_path_committed_rows`（LocalBlob 写 csv 含 header → import → status Done、rows=N、bytes>0、samples 缺省）
  - `import_skip_bad_rows_samples_absolute_line`（csv 第 3 数据行坏格（header=true）→ Done、errors=1、samples[0].line=5、raw 含原格、好行落库——InMemory 无断言落库则改 sqlite 真腿断言 `query_with_params`）
  - `import_abort_batch_rolls_back_committed_kept`：sqlite 真腿——两批，第二批约束冲突 → Failed、`rows`=第一批数、库内只有第一批行
  - `import_cancel_between_batches_keeps_committed`：小 batchSize + 多批，cancel 于批间（`registry.cancel` + status 轮询到 terminal）→ Cancelled、0<rows<total、sqlite 查库行数 == rows
  - `import_replace_mode_deletes_then_inserts`：sqlite 真腿——预置 2 旧行，replace 导入 3 新行 → 库内恰 3 新行
  - `import_upsert_idempotent_twice`：sqlite rowid upsert 跑两遍行数不变（PG/MySQL 幂等归 T7 env 腿）
  - `import_cancel_vs_final_commit_race`：cancel 在最后一批已 commit 后到达 → state 不翻 Cancelled（Done 赢）——用「批极少 + cancel 于 finish 后轮询」构造
  - `import_blob_over_max_bytes_fails_named`：`max_bytes` 掐小 → Failed、error 含 `max_bytes`

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --release -p only-js import_blob_csv_happy`
Expected: FAIL（占位 consumer 返 0 行 / 未读 blob）。

- [ ] **Step 3: 实现 consumer**。
- [ ] **Step 4: 跑测试确认通过 + 全 crate 绿**

Run: `cargo test --release -p only-js import_`
Expected: 全 PASS。

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(db): import consumer——blob 流式/批事务/abort/skip/cancel（REQ-12 T6）

unix@vip.qq.com ai"
```

---

### Task 7: env-gated PG/MySQL 真库腿 + release v0.1.115

**Files:**
- Modify: `src/bridge/db_import.rs`（env-gated 测试腿）
- Modify: `oj/Cargo.toml`（version 0.1.115）、`Cargo.lock`
- Modify: `CHANGELOG.md`（v0.1.115 段，注明未打标签）
- Modify: `docs/devkit/api-manual.md`（§13 错误/限制表 + db 全局面指针）、`docs/devkit/db-guide.md`（新 §3.4 导入三件套 + §11.6.5 错误码如有新增 + 能力矩阵表加行）、`docs/devkit/SKILL.md`（陷阱：scoped 表 import_under_guard / append 幂等自负 / samples 行号口径）、`docs/devkit/scenarios.md`（场景 68：CSV 批量导入 + 取消按钮组合）、`docs/devkit/README.md`（索引行 + v0.1.115 落地段）
- Modify: `sample/global.d.ts`（ImportArgs/ImportStatus 类型——T5 已加签名，本任务补全注释）

**Interfaces:**
- Consumes: T1-T6 全部用户可见行为。
- Produces: 发布物。

- [ ] **Step 1: env-gated 真库腿**（`OJ_TEST_PG`/`OJ_TEST_MYSQL` 缺 → 跳过）：
  - `real_pg_mysql_import_perf_500k_under_60s`：生成 500_000×10 CSV（内存 ~25MB）→ LocalBlob 落 blob → import（默认 batch）→ 轮询 status 至 Done，断言 rows=500_000、耗时 <60s（`tokio::time::Instant`）
  - `real_pg_mysql_upsert_idempotent`：同表同数据 import 两遍 → 行数不变
  - `real_pg_mysql_cancel_mid_import`：100k 行 + `batchSize: 100`（多批）→ 起跑后 cancel → state=Cancelled、已提交行可查（`rows>0` 且库里行数==rows）、未提交批不落库
  - PG/MySQL 各自 connect 建表（`create table if not exists` 经 accessor exec）+ 测试后 drop——**走 `da.exec_with_params` 参数化，不拼值**

- [ ] **Step 2: 跑 env 腿**

Run: `set -o pipefail && OJ_TEST_PG=postgres://postgres:postgres@127.0.0.1:5432/ojtest OJ_TEST_MYSQL=mysql://root:root@127.0.0.1:3306/ojtest cargo test --release -p only-js real_pg_mysql_import 2>&1 | tail -3`
Expected: 3 腿全 pass（性能腿输出实测秒数记入 ledger）。

- [ ] **Step 3: 版本 + CHANGELOG + devkit 四件 + global.d.ts**——CHANGELOG v0.1.115 段：G1 三件套/G2 方言路径（PG ON CONFLICT 直挂多值 INSERT——临时表路径裁决）/G3 samples 绝对行号/G4 幂等口径/守卫矩阵（import_under_guard + truncate 同档确认门）/配额段；devkit 按 Global Constraints 的同步要求逐条对齐。

- [ ] **Step 4: 全量验证**

```bash
cargo fmt && cargo xtask build && cargo test --release -p xtask   # devkit 契约 10/10
set -o pipefail && OJ_TEST_PG=… OJ_TEST_MYSQL=… cargo test --release --workspace 2>&1 | tail -3
cargo clippy --release --workspace --all-targets -- -D warnings
```
Expected: 10/10；workspace 全绿；clippy 0 警。

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "release: v0.1.115——REQ-12 批量导入三件套全交付

unix@vip.qq.com ai"
```

---

## Self-Review 记录

- **Spec 覆盖**：G1 三件套（T5/T6）、G2 方言（T4/T6/T7）、G3 行号（T3/T6）、G4 幂等（T6/T7）、三硬约束（T5 守卫矩阵 + T2/T6 配额 + T5 审计）、验收六条（T5 前缀隔离/守卫离线腿、T6 skip/行号、T7 性能/取消/幂等）。无缺口。
- **类型一致性**：`ImportRegistry::register` T2 定义/T5 消费/T6 消费；`Finish`/`ImportSample` T2 定义、T6 消费；`render_batch`/`batch_rows` T4 定义、T6 消费；`ParsedRow`/`ParseError` T3 定义、T6 消费。op 名 T5 定义、mod.rs/bootstrap T5 同任务消费。
- **修正案（对 spec）**：① PG upsert 不建临时表——多值 INSERT 直挂 ON CONFLICT（spec §4.3 临时表是 COPY 路径解；少类型映射+往返，语义等价）→ ledger ruling；② sqlite append/upsert 走全单事务特例（spec §4.3 性能关键句）；③ skip 模式 = 批内逐行 INSERT（多值 INSERT 批原子，无法单行跳过——spec「行不落库」语义的唯一正确实现）。均记 ledger。
- **Review Focus 落点**：1→T5 guard 三形态+asSystem 腿；2→T3 解析器不闭合引号/长字段腿；3→T6 cancel/commit 竞态腿；4→T4 batch_rows 腿；5→T5 配额边界腿 + T6 max_bytes 腿。
