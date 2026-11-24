# REQ-PARSE 平台 SQL 解析服务实施计划（Phase 2 / v0.1.143）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 平台提供一等 SQL 解析全局 `sql.parse(sqlText, opts)` —— 结构化返回 `{dialect, tables, columns, ctes, lineage, ast}`，多方言、结构化错误、纯函数无副作用（docs/UPSTREAM-REQS.md §3.3 五条验收）。

**Architecture:** vendored node-sql-parser 5.4.0（Apache-2.0，webpack UMD bundle）编译期内嵌进 bridge_ext，注册为**懒加载** ESM（冷启动零回退，P0/P1 冷启动成果不得吃掉）；bootstrap.js 挂 `globalThis.sql` 门面（首次调用 `core.loadExtScript` 拉取）；血缘 walker 为手写 ESM 文件，静态 import vendor 包。解析纯 JS 同步执行，零 Rust op、零 ABI 变更、零 SchemaRegistry 触碰（不执行 SQL，红线无关）。

**Tech Stack:** JS（bridge_ext ESM）+ Rust 测试（mod_tests.rs 的 `new_bridge()` + `run_with` harness 样板，tokio current_thread）。

**Spec:** `docs/UPSTREAM-REQS.md` §3.3（REQ-PARSE）+ 程序总览（`2026-11-24-upstream-reqs-phase1-migrate-db-binding.md` 头部表格）评审裁决：vendored node-sql-parser + 血缘 walker；MVP 口径 = tables/columns + 单层 CTE 血缘 + golden SQL 准确率底线。

## Global Constraints

- 所有测试/检查一律 `--release`；禁裸 `cargo test` / `cargo clippy`；不可并发两个 cargo test（接管道须 `set -o pipefail`）。
- **7-bit ASCII 门禁**：bridge_ext 全部 ESM（bootstrap.js / vendor 包 / 新 impl）必须纯 ASCII（`ascii_str_include!` 编译期硬门禁 + mod_tests 既有护栏用例）；vendor 包含非 ASCII 字节 → vendor 时做 `\uXXXX` 转义；新文件注释一律英文。
- **懒加载是硬要求**：vendor 包 2.5MB minified，禁止静态 import 进 bootstrap.js（每 runtime 2.5MB 解析会吃掉冷启动优化成果）；必须走 `lazy_loaded_esm_files` + 首次调用 `core.loadExtScript`。
- vendored 文件**只生成不手改**：头注释记录版本/tarball sha512/许可证/transform 步骤，未来 re-vendor 可复现；Apache-2.0 许可证文本随仓。
- 新增 JsRuntime 入口不适用（无新入口；`ascii_str_include` 内嵌源即 `Computed`，无需 patch_fs_loaded_sources）。
- 版本 ritual：CHANGELOG「下一版（未发布）」累积；devkit 四件同步（本阶段有用户可见新 API → 必须全量对齐）+ `cargo test --release -p xtask` 契约；版本 bump 0.1.142 → **0.1.143**；tag/push 外发动作等用户确认。
- 提交尾行：`unix@vip.qq.com ai`（裸行，无 Co-Authored-By）。
- 异步测试 `tokio::test(flavor = "current_thread")`；禁固定 sleep。
- 落地后按既定循环：架构师 + 工程师评审，BDD 用例经测试工程师评审（用户既定流程）。

## 冻结接口契约（Task 2 起逐字段实现，不得漂移）

```js
sql.parse(sqlText, opts?)
// sqlText: string；opts: { dialect?: string }
// 返回：
{
  dialect: string,                       // 规范化后的方言名（别名已归一）
  tables: [{ name, schema, alias }],     // 基表（CTE/子查询不入列；schema/alias 无则 null）
  columns: [{ table, column }],          // 引用到的列（table 解析失败为 null），去重
  ctes: [string],                        // WITH 定义的 CTE 名（按出现序）
  lineage: [{                            // 顶层 SELECT 每个「输出列」一条
    output: { table, column },           // table = 输出侧来源名（基表名或 CTE/别名）
    sources: [{ table, column }],        // 穿透单层 CTE 解析后的基表来源
  }],
  ast: object,                           // parser 原生 AST（JSON round-trip 克隆，JSON 安全）
}
// 失败 throw Error，own props：
{ code: "OJ_SQL_PARSE", reason: "input"|"dialect"|"budget"|"syntax"|"engine",
  dialect?, location?: { start: {offset,line,column}, end }, supported?: string[] }
```

- **方言表（冻结）**：`mysql, mariadb, postgresql, sqlite, snowflake, transactsql, trino, bigquery, hive, flinksql, db2, redshift, athena, noql`；别名 `pg→postgresql`、`mssql→transactsql`。Task 1 用 grep 在 bundle 里复核此清单（duckdb 若在 5.4.0 grammar 中则加入，否则不支持并记录）。dialect 缺失/未知 → `reason:"dialect"` + `supported` 列表，**无魔法默认方言**。
- **预算**：输入 > 1 MiB（1048576 字符）→ `reason:"budget"`。解析同步跑在 runtime 线程（KillSwitch 看门狗兜底）。
- **单语句契约**：多语句（`;` 分隔）→ `reason:"syntax"`（api-manual 声明一次一句）。
- **血缘解析规则（无 schema 现实下的 MVP 口径）**：
  - `alias.col`：别名命中基表 → `{table:基表, column}`；命中 CTE/派生子查询 → **穿透一层**取其 SELECT 列表的来源（CTE 内已是基表列则直达；CTE 再引用别的 CTE → 停在该 CTE 名边界 `{table:<cteName>, column}`，不递归第二层）。
  - 裸 `col`：作用域内唯一来源（含单 CTE）→ 归属之；多来源（join）且无 schema → `{table:null}`、sources 空（诚实降级，sqlglot 无 schema 同行为）。
  - output.table 记输出侧来源名（别名如 `r` 记其背后的 CTE/基表名，不记 `r` 本身）。
  - DDL（create/alter/drop）：tables 取目标表，columns/lineage 空（api-manual 声明）；其余语句类型 tables/columns/lineage 空 + ast 在（诚实降级）。

## Review Focus

1. **深嵌套/病态 SQL（千层括号）**——pegjs 递归可能栈溢出；合理期望：V8 RangeError 被映射为 `reason:"engine"` 结构化错误，进程不崩。→ Task 4 固化。
2. **注释与引号标识符**（`-- x` / `"order id"` / 反引号）——合理期望：解析成功、名称去引号逐字保留。→ Task 4 固化。
3. **空串/纯注释/非字符串入参**——合理期望：结构化错误（`input`），不静默、不 crash。→ Task 4 固化。
4. **AST 大小无界**——AST JSON 可达输入 10 倍，契约含 ast 即调用方付费；合理期望：文档明示、不截断、预算仍按输入字符计。→ Task 5 api-manual 限制表固化（无代码）。
5. **池化 runtime 各自懒加载**——每个 runtime 首调各拉一次 vendor（2.5MB × pool_size）；合理期望：行为正确性不受影响（各自模块缓存），内存代价在 api-manual 声明。→ Task 5 api-manual 声明（行为正确性由 Task 1 spike 的 run_with 用例间接覆盖）。

## BDD 用例（INVEST）

- **S1 golden 锚（spec 原例）**：§3.3 的 CTE+join 示例 SQL（postgresql）→ tables 恰为 `orders`+`customers`（无 recent）；ctes=["recent"]；lineage 含 output{recent,order_id}→sources[{orders,order_id}] 与 output{customers,name}→sources[{customers,name}]。
- **S2 方言路由**：同一 SQL 过 postgresql 成功；`pg` 别名归一为 `postgresql`；未知方言与缺省 → `reason:"dialect"` 且 supported 含 postgresql。
- **S3 嵌套子查询解析**：FROM 子查询内层表出现在 tables、ast type="select"；（血缘可部分解析——契约内诚实降级）。
- **S4 多方言 DDL**：postgresql `CREATE TABLE` 与 snowflake 语句均解析成功，DDL → columns/lineage 空、tables 含目标表。
- **S5 结构化错误**：`SELECT FROM WHERE` 语法垃圾 → throw 含 `code:"OJ_SQL_PARSE"`、`reason:"syntax"`、location；**非静默**。
- **S6 幂等无副作用**：同输入连调两次 → 结果 deep-equal；全程无任何 op 调用（纯 JS 面，harness 上天然成立，用例固化为锚）。
- **S7 预算**：>1 MiB 输入 → `reason:"budget"`，快速返回。
- **S8 裸列解析**：单表 select 裸列 → columns/table 归属；join 下裸列 → table:null（契约口径锚）。
- **S9 中文标识符逐字**：中文列名/别名 → columns/lineage 名称逐字保留。
- **S10 深嵌套不崩**：千层嵌套 → 结构化错误（`syntax` 或 `engine`），进程存活（后续请求仍可跑）。

---

### Task 1：vendor 包 + 懒加载接线（spike 性质：先让 `sql.parse` 返回裸 AST）

**Files:**
- Create: `src/bridge/vendor_sql_parser.js`（transform 产物）+ `src/bridge/vendor_sql_parser.LICENSE`（Apache-2.0 全文）
- Modify: `src/bridge/mod.rs`（`BRIDGE_LAZY_ESM` + `bridge_ext_init` 注入 `lazy_loaded_esm_files`）
- Modify: `src/bridge/bootstrap.js`（`globalThis.sql` 门面，暂时只透传 astify）
- Test: `src/bridge/mod_tests.rs`（补两条护栏：vendor 源 7-bit ASCII、lazy 注册存在）+ `src/bridge/sql_parse_tests.rs`（新，spike 用例）

**Interfaces:**
- Produces: `core.loadExtScript("ext:bridge_ext/sql_parse_impl.js").default` 形状的懒加载面（Task 2 换成 impl 文件）；`BRIDGE_LAZY_ESM` 注册点。

- [ ] **Step 1：复核 bundle 方言清单与 transform 定案**

```bash
cd /tmp/nsp-probe && grep -o 'case"[a-z]*"' package/umd/index.umd.js | sort -u | head -40
# 并 grep 'duckdb' 确认 5.4.0 是否支持（决定方言表最终版）
```

- [ ] **Step 2：执行 vendor transform（一次性命令，步骤抄进 vendor 头注释）**

shim 头：`var module={exports:{}};var exports=module.exports;`；尾：`export default module.exports;`；去掉 `//# sourceMappingURL` 行；非 ASCII 字节 → `\uXXXX`（node 一次性脚本，写进头注释）。产物落 `src/bridge/vendor_sql_parser.js`，头注释含：版本 5.4.0、tarball URL + sha512（`jVe6Z61g...`，从 npm metadata 记录）、License Apache-2.0、transform 四步。LICENSE 文本从 tarball `package/LICENSE` 拷入。

- [ ] **Step 3：mod.rs 注册 lazy ESM + bootstrap.js 挂门面**

`BRIDGE_LAZY_ESM` 两项：`ext:bridge_ext/vendor_sql_parser.js` 与 `ext:bridge_ext/sql_parse_impl.js`（本 Task 先以 vendor 文件顶 impl 位，Task 2 换真 impl）；`bridge_ext_init` 注入 `ext.lazy_loaded_esm_files = Cow::Borrowed(BRIDGE_LAZY_ESM)`。bootstrap.js 追加（英文注释，ASCII）：

```js
// ----- sql: pure SQL parsing (v0.1.143) -----
let ojSqlImpl = null;
globalThis.sql = {
  parse: (sqlText, opts) => {
    if (ojSqlImpl === null) {
      ojSqlImpl = core.loadExtScript("ext:bridge_ext/sql_parse_impl.js").default;
    }
    return ojSqlImpl.parse(sqlText, opts);
  },
};
```

⚠️ 若 lazy 图内静态 import（impl → vendor）在 deno_core 0.411 拉不动（spike 验证点）：fallback = Task 2 的 impl 与 vendor 在 vendor 时合并为单文件，门面 specifier 不变。

- [ ] **Step 4：写失败测试（sql_parse_tests.rs）**

按 `mod_tests.rs:22` `new_bridge()` 样板（tokio current_thread）：`run_with` 跑 `json.ok(sql.parse("select 1 as x", { dialect: "postgresql" }).ast.type)` → 期望 `"select"`。先红（文件不存在/未注册 → loadExtScript 抛错）。

- [ ] **Step 5：实现到绿**；mod_tests.rs 补两条护栏：`vendor_sql_parser_js_is_7bit_ascii`（仿 `bootstrap_js_is_7bit_ascii`）、`bridge_lazy_esm_registered`（`bridge_ext_init` 后 `lazy_loaded_esm_files` 含两个 specifier 且 `is_runtime_loadable()`）。

- [ ] **Step 6：`cargo test --release -p only-js sql_parse`（+ mod_tests 护栏）绿 → 提交** `feat(bridge): vendored node-sql-parser 5.4.0 懒加载接线，sql.parse 门面 spike`

### Task 2：sql_parse_impl.js 全量门面（校验/方言/错误结构/tables+columns）

**Files:**
- Create: `src/bridge/sql_parse_impl.js`（手写，ASCII 英文注释）
- Modify: `src/bridge/mod.rs`（BRIDGE_LAZY_ESM 的 impl specifier 指向新文件；vendor specifier 保留）
- Test: `src/bridge/sql_parse_tests.rs`

**Interfaces:**
- Consumes: vendor 包 default（`new Parser()`，`astify(sql, {dialect})`）。
- Produces: 冻结契约的 `parse()` 完整实现（lineage 先占位空数组，Task 3 填充）；方言表/预算常量。

- [ ] **Step 1：写失败测试**：S2 全部断言（pg 别名归一、未知/缺省 dialect → `reason:"dialect"` + supported）、S5（垃圾 SQL → syntax+location）、S8（单表裸列归属；join 裸列 table:null）、S1 的 tables/columns/ctes 部分（lineage 断言留 Task 3）。
- [ ] **Step 2：实现 `parse()`**：入参校验（input）→ 方言归一（dialect）→ 预算（budget）→ try `astify`（syntax，带 location）→ walk AST 提 tables/columns/ctes → 返回契约对象。walker 只处理 select/insert/update/delete/create 的已知字段，未知语句类型诚实降级（tables 空 + ast 在）。throw 用 `Object.assign(new Error(msg), {...})` 挂 own props。
- [ ] **Step 3：测试绿**；跑既有全套 `-p only-js`（bootstrap 改动回归）。
- [ ] **Step 4：提交** `feat(bridge): sql.parse 契约面——方言归一/预算/结构化错误/tables+columns`

### Task 3：血缘 walker（单层 CTE 穿透 + golden 锚）

**Files:**
- Modify: `src/bridge/sql_parse_impl.js`（walker 实现，替换占位）
- Test: `src/bridge/sql_parse_tests.rs`

**Interfaces:**
- Consumes: Task 2 的 AST 遍历骨架与作用域收集。
- Produces: 冻结契约 `lineage` 字段全量语义（口径见「血缘解析规则」，实现者照口径写，无自由度）。

- [ ] **Step 1：写失败测试**：S1 全量（含 lineage 两条 golden）、S3（嵌套子查询诚实降级锚）、S4（DDL lineage 空）、S9（中文标识符逐字）。中文 SQL 字面量在 Rust 源里直接写（源文件 UTF-8 无碍；只有 ESM 有 ASCII 门禁）。
- [ ] **Step 2：实现 walker**：收集 WITH 定义（名→SELECT 节点）→ FROM/JOIN 作用域（基表/别名/派生子查询）→ SELECT 列表逐项解析输出列与 sources（穿透一层 CTE；规则照口径）→ dedupe。AST 数值/字符串字段全按 node-sql-parser v5 文档形状处理（实现时以 spike 的实际 AST 为准，Test 先用真实返回调 golden 期望再固化）。
- [ ] **Step 3：测试绿** → 全套 `-p only-js` 绿。
- [ ] **Step 4：提交** `feat(bridge): sql.parse 血缘 walker——单层 CTE 穿透、别名解析、golden 锚`

### Task 4：预算与边界输入（Review Focus 固化）

**Files:**
- Modify: `src/bridge/sql_parse_impl.js`
- Test: `src/bridge/sql_parse_tests.rs`

- [ ] **Step 1：写失败测试**：S7（1 MiB+1 → budget）、S10（千层嵌套 → 结构化错误且进程存活：同 runtime 再跑一次正常 parse 成功）、Review Focus 2/3（注释、引号标识符、空串、纯注释、非字符串入参 → 各自结构化错误/逐字语义）、S6（幂等 deep-equal 锚）、多语句 `select 1; select 2` → syntax。
- [ ] **Step 2：实现**：深嵌套 catch RangeError → `reason:"engine"`（或 parser 自身 syntax，以实际为准断言其一是结构化错误即可）；空/非串 → input。预算检查在 astify **之前**（先于解析）。
- [ ] **Step 3：测试绿** → 提交 `feat(bridge): sql.parse 边界输入固化——预算/深嵌栈/注释引号/幂等锚`

### Task 5：发布 ritual（docs 四件 + CHANGELOG + bump 0.1.143 + 全门禁 + 评审）

**Files:**
- Modify: `docs/devkit/api-manual.md`（`sql.parse` 全节：签名/方言表/返回契约/错误表 OJ_SQL_PARSE×5 reasons/限制行：1 MiB 预算、单语句、同步 CPU 面、AST 体积无界、每 runtime 各自懒加载内存代价）
- Modify: `docs/devkit/SKILL.md`（陷阱行：pg≠postgresql 之外的方言名映射、CTE 只穿一层边界、DDL 血缘为空）
- Modify: `docs/devkit/scenarios.md`（新场景：registerSql 血缘摄入形状示例）
- Modify: `docs/devkit/README.md`（能力清单一行）
- Modify: `CHANGELOG.md`、`oj/Cargo.toml` + `Cargo.lock`（0.1.143）
- Test: xtask devkit 契约（自动）

- [ ] **Step 1：docs 四件逐条对齐**（错误文案与实现逐字一致）。
- [ ] **Step 2：CHANGELOG + bump 0.1.143**（未打标签，tag 等用户确认）。
- [ ] **Step 3：全门禁**：`cargo test --release -p only-js`、`cargo test --release -p oj`、`cargo test --release -p xtask`、`cargo clippy --release --workspace --all-targets -- -D warnings`、`cargo fmt --check`（串行跑，禁并发）。
- [ ] **Step 4：提交** `chore(release): v0.1.143 版本分界——sql.parse 文档与 CHANGELOG`
- [ ] **Step 5：评审循环**：架构师 + 工程师 + 测试工程师（BDD 对照）三路评审 → must/should-fix 修订提交 → 计划文档记录执行偏差 → 向用户分阶段总结。

### 后续任务（不在本计划）

- 下游（oj-wiki-docs）`registerSql` 掩码正则路径替换为 `sql.parse` —— 属下游仓改造，平台侧以验收标准 5（「可直接替换」）由 golden 契约保障。
- REQ-HNSW 启动门禁绑定路由（Phase 1 遗留，见程序总览）。

---

## 执行偏差记录（v0.1.143 实落，2026-11-24）

1. **vendor 源装载改 `LazyLock` + `Computed`**（commit 852f513，Task 1 修正）：计划写的是
   `ascii_str_include!` 零拷贝常量（带 `#[allow(long_running_const_eval)]`）。实际 2.5MB
   const-eval 触发 rustc `long_running_const_eval` 且该 lint 对 allow 不生效
   （rustc#125290）——clippy `-D warnings` 门禁必挂。改为：编期 `include_str!` 直嵌进
   二进制（零读盘不变），运行期首次访问 `LazyLock` 拷贝一次 `Arc<str>`、走
   `ExtensionFileSource::new_computed`；懒加载语义不变（仍只在首次 `sql.parse` 进 V8）。
   ASCII 门禁从编译期断言移到 mod_tests 的 7-bit 守卫 + `load()` 与磁盘逐字一致断言。
   代价：首个调用 runtime 一次性 2.5MB 拷贝（此前是零拷贝外部常量），可忽略。
2. **`core.createLazyLoader` 门面即 impl 文件**：Task 1 期间发现 `loadExtScript` 是
   classic-script 通道看不到 lazy ESM，门面定为 `createLazyLoader("ext:bridge_ext/
   sql_parse_impl.js")`（impl 内再 import vendor），loader 必须在 `parse()` 内部调用
   （bootstrap 求值期 loader 还不是模块命名空间）。
3. **测试期望修正两处**（Task 3，测试窄于契约而非实现错）：ON 子句列引用（`u.cid`/`c.id`）
   属引用列进 `columns`；`tables` 保留基表别名（`customers` 带 `alias:"c"`）。
4. **权限分类器阻断 /tmp 下执行 npm 包 JS**（Task 1 环境性）：bundle 变换改 python3 纯文本
   处理（无代码执行）；语义等价由阻断前的 Node smoke（SMOKE-OK/SMOKE2-OK）与平台内
   cargo test 全绿背书——vendored 代码在生产 runtime 里执行本身就是最真验证。
5. 其余按计划落：契约面/血缘 walker/边界测试 Task 2-4 提交 21e73fc/9aa72f8/da1e57c；
   Task 5 提交 2adebc4；门禁全绿（only-js 1165、oj 331+e2e、xtask 11 devkit 契约、
   clippy -D warnings、fmt）。tag 待用户确认。

### 评审循环裁决（Task 5 Step 5，2026-11-24）

三路评审均 **APPROVE-WITH-FIXES**，修订已落（52db95d + docs 对齐提交）：

- 架构师：2 should-fix（自连接别名塌缩、嵌套 WITH 泄漏）+ 1 nit（Cow::Owned）——全部采纳。
- 工程师：1 must（自连接）+ 3 should（幻影表/嵌套 WITH/star 文档漂移）+ nits——采纳；
  其中 **star 文档漂移按文档改**（探针实证 impl 产出 `column:"*"`，api-manual 原措辞
  「null」与实现不符，改文档保留更多信息量）；walker engine 兜底 + astify RangeError→engine
  采纳（engine 档从预留转实义，docs 同步）。
- 测试工程师（BDD 对照）：S1-S10 与 Review Focus 全 PINNED（S3 部分补 tables 断言）；
  mssql 别名 must-pin 补齐；statements 计数/SELECT */write 目标/commentOnly 各补。
  其 commentOnly「应为 input」的假设经实测证伪——纯注释 astify 返回空语句数组，
  走多语句拒绝（syntax + statements:0），与 api-manual input 行定义一致，测试按实断。
