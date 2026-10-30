# REQ-11 非流式取消与执行进度 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `db.query/exec` 获得与 stream 同空间的 executionId、真服务端取消（`OJ_DB_CANCELLED`）、挂断联动（断开后停跑）与相位/行级进度回调。

**Architecture:** 池路径改显式 acquire + `tokio::select!`（fetch 流 vs cancel notify）；cancel = drop fetch future + `close_on_drop()` 真关 TCP（服务端语句即死，连接报废不回池）。执行登记进既有 `ExecRegistry`（新 Pool 变体，同 `exec_` id 空间）；挂断联动走请求级 `cancel_token`（`RequestInfo` 新增字段）+ serve 侧 `DisconnectGuard`（PutStreamGuard 先例）。JS 回调面用 begin/poll/take 三 op（无 Rust→JS 函数调用先例，回调全留 JS 侧）。

**Tech Stack:** Rust / sqlx 0.9（AnyPool）/ deno_core op2 / tokio / axum。

**Spec:** `docs/superpowers/specs/2026-11-01-dbx-req11-cancel-progress-design.md`（含计划期修正案 #2：pid+池外 kill → close_on_drop，验收等价）。

## Global Constraints

- 一切构建/测试/clippy 用 `--release`；禁止 debug 构建。
- 异步测试 `tokio::test(flavor = "current_thread")`（JsRuntime `!Send`）。
- cargo test 接管道必须 `set -o pipefail`，且不可并发两个 cargo test。
- commit message 尾行 `unix@vip.qq.com ai`。
- **零 ABI**：`oj-plugin-ffi` 的 `ABI_VERSION`（12）不动；`DataAccessor` 是 Rust trait 非 FFI 面，新增带默认实现的方法不碰 ABI。
- `bootstrap.js` 必须保持 7-bit ASCII。
- 失败的 runtime 丢弃不还池（本次同口径：被 cancel 的连接报废）。
- 每次版本更新：CHANGELOG.md + devkit 四件（api-manual/scenarios/SKILL/README）逐条对齐 + `cargo xtask build` + `cargo test --release -p xtask` 契约用例。
- env-gated 真库腿（PG `postgres://postgres:postgres@127.0.0.1:5432/ojtest` / MySQL `mysql://root:root@127.0.0.1:3306/ojtest`）条件不满足诚实 skip，不 panic。
- SQL 注入红线：动态标识符只来自 SchemaRegistry/`quote_ident`，值只走绑定参数。

## Review Focus

1. **cancel 与完成竞态**：语句 settle 与 cancel 同刻——先 settle 者赢；已完成执行再收 cancel 不得把成功改写成 CANCELLED（cancel 对已 settle entry 幂等空转）。
2. **开跑前 cancel**：登记后、语句 submit 前 cancel 到达 = 语句**不执行**，结果 CANCELLED（不碰连接）。
3. **超时与 cancel 同到**：`race_statement` 超时与 cancel 同时触发时先到先生效，category 不互相覆盖（query_impl 既有「查询已成功」守卫模式同款）。
4. **被弃连接绝不回池**：cancel/超时路径 `close_on_drop()` 钉死；取消后池可用连接数不缩容（sqlx 槽位回收）。
5. **onOpen 回调抛异常**：JS 侧 catch 记 warn，不影响执行本身；stream `onProgress` 节流 ≥100ms 且 close 后 `info()` 仍可读（lastInfo 快照语义不破）。

---

### Task 1: PoolExecShared + ExecRegistry Pool 变体

**Files:**
- Modify: `src/bridge/db_exec.rs`（ExecEntry → ExecHandle 枚举；register/cancel 分发；新增 cancel_by_token）
- Modify: `src/bridge/db.rs`（PoolExecShared / CallEvent / PoolOutcome 定义，DbStreamShared 旁）
- Test: `src/bridge/db_exec.rs`（`#[cfg(test)]` 模块内新增）

**Interfaces:**
- Consumes: 既有 `ExecRegistry::register/cancel/remove`、`Owner::same_subject`、`handle_core::{new_id, new_boot_tag, id_is_wellformed}`。
- Produces:
  ```rust
  pub(crate) enum ExecHandle { Stream(Arc<DbStreamShared>), Pool(Arc<PoolExecShared>) }
  pub(crate) struct PoolExecShared {
      pub owner: Owner, pub alias: String,
      pub cancel_requested: AtomicBool, pub notify: Arc<Notify>,
      pub cancel_token: Option<u64>, pub done: AtomicBool,
      pub events: Mutex<Vec<CallEvent>>,
      pub outcome: Mutex<Option<PoolOutcome>>,
  }
  pub(crate) struct CallEvent { pub phase: &'static str, pub elapsed_ms: u64, pub rows: Option<u64> }
  pub(crate) enum PoolOutcome { Rows(Vec<Row>), Affected(i64), Err { category: DbErrorCategory, msg: String } }
  impl ExecRegistry {
      pub(crate) fn register_pool(&self, shared: Arc<PoolExecShared>) -> Result<String, String>;
      pub(crate) fn cancel_by_token(&self, token: u64) -> usize; // 置标志+notify 所有未 done 且 token 匹配的 Pool entry
  }
  ```
  （Task 2 消费 `PoolExecShared` 字段；Task 3 消费 register_pool/cancel_by_token；Task 5 消费 cancel_token。）

- [ ] **Step 1: 写失败测试**（db_exec.rs `#[cfg(test)]`）

```rust
#[tokio::test(flavor = "current_thread")]
async fn register_pool_returns_exec_prefixed_id_and_cancel_sets_flag() { … }
#[tokio::test(flavor = "current_thread")]
async fn cancel_by_token_cancels_matching_unsettled_only() { … }
#[test]
fn pool_entry_swept_when_done() { … } // done=true 的 Pool entry 在下次 register 时被清扫
```

断言：`register_pool` 返回 `exec_` 前缀 id；`cancel(id, &owner)` 对 Pool 变体置 `cancel_requested` + 唤醒 notify，返回 `Some(alias)`；他人/未知 id 仍 `None`；`cancel_by_token(7)` 只触达 `cancel_token==Some(7)` 且 `done==false` 的 entry，返回触达数；done=true 的 Pool entry 被懒惰清扫。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --release --lib db_exec`
Expected: FAIL（`register_pool`/`cancel_by_token` 未定义）

- [ ] **Step 3: 实现**

`ExecEntry` 改为 `ExecHandle` 枚举；`register` 保留（变体 Stream），新增 `register_pool`（同清扫逻辑，按 `done` 判定）；`cancel` 内部分流：Stream = 既有路径，Pool = 置标志 + `notify_one()`；`PoolExecShared`/`CallEvent`/`PoolOutcome` 定义放 `db.rs`（`DbStreamShared` 旁，复用其模块的 atomic/notify 导入习惯）。清扫闭包对两型统一：`match handle { Stream(s) => s.done.load(..), Pool(p) => p.done.load(..) }`。

- [ ] **Step 4: 跑测试确认通过 + 既有测试不红**

Run: `set -o pipefail && cargo test --release --lib 2>&1 | tail -3`
Expected: 全部 PASS（含既有 exec 用例）

- [ ] **Step 5: Commit**

```bash
git add src/bridge/db_exec.rs src/bridge/db.rs
git commit -m "feat(db): ExecRegistry Pool 变体——非流式执行登记与 cancel 分发

unix@vip.qq.com ai"
```

### Task 2: SqlxAccessor 可取消执行核心

**Files:**
- Modify: `src/bridge/accessor_sqlx.rs`（`query_cancelable` / `exec_cancelable`）
- Modify: `src/bridge/db.rs`（`DataAccessor` trait 新增两方法，默认实现委托原方法）
- Test: `src/bridge/accessor_sqlx.rs`（sqlite 离线腿）

**Interfaces:**
- Consumes: Task 1 的 `PoolExecShared`/`CallEvent`/`PoolOutcome`；既有 `ByteMeter`、`normalize_and_trace` 外的行整形（`row_to_json`）。
- Produces:
  ```rust
  async fn query_cancelable(&self, sql: &str, params: &[Value],
      meter: &mut ByteMeter, shared: &PoolExecShared) -> BridgeResult<Vec<JsRow>>;
  async fn exec_cancelable(&self, sql: &str, params: &[Value],
      shared: &PoolExecShared) -> BridgeResult<i64>;
  ```
  （Task 3 经 `Target::Pool(da)` 调用。插件轴继承默认实现 = 不可取消，符合设计 §4.5。）

- [ ] **Step 1: 写失败测试**（sqlite in-memory 离线腿）

```rust
#[tokio::test(flavor = "current_thread")]
async fn cancelable_query_records_sent_fetching_done_events() { … }
#[tokio::test(flavor = "current_thread")]
async fn cancel_before_submit_never_touches_connection() { … }
#[tokio::test(flavor = "current_thread")]
async fn cancel_mid_execution_returns_cancelled_category() { … } // sqlite：notify 置位后 select! 命中（用大结果集 + 轮询点构造竞态；做不到则只钉「submit 后 cancel → CANCELLED」）
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --release --lib accessor_sqlx`
Expected: FAIL（方法未定义）

- [ ] **Step 3: 实现**

`query_cancelable`：`pool.acquire()` → 提交前查 `cancel_requested`（true → `Err(cancelled_error(alias))`，category=Cancelled，**不碰连接**）→ `sent` 事件 →
`tokio::select!`：`fetch` 流（逐行 `row_to_json` + meter feed；首行发 `fetching`；meter 触顶 fail 档 = 既有早停语义）vs `shared.notify.notified()`。
cancel 分支：drop fetch 流 → `conn.close_on_drop()` → drop(conn) → `Err(cancelled_error)`。
正常：发 `done(rows)` 事件 → 归还连接。`exec_cancelable` 同构（无行/meter，结果是 affected）。
取消后**绝不**走 `return_to_pool`。`cancelled_error` 放 `db_error.rs`（category=Cancelled、retryable=false、消息不含完整 id）。

- [ ] **Step 4: 跑测试确认通过**

Run: `set -o pipefail && cargo test --release --lib accessor_sqlx 2>&1 | tail -3`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/bridge/accessor_sqlx.rs src/bridge/db.rs src/bridge/db_error.rs
git commit -m "feat(db): SqlxAccessor 可取消执行——select! 竞速 + close_on_drop 真杀

unix@vip.qq.com ai"
```

### Task 3: op 层——legacy 路径全量登记 + begin/poll/take + CANCELLED 映射

**Files:**
- Modify: `src/bridge/db_query.rs`（query_impl/exec_impl 池臂接 cancelable + 登记；新 op 三件套 ×2）
- Modify: `src/bridge/mod.rs`（`bridge_ext` op 注册表加 6 个新 op）
- Modify: `src/bridge/db_stream.rs`（cancel 错误改结构化 Cancelled）
- Test: `src/bridge/db_exec.rs` / `src/bridge/db_query.rs`（bridge harness 内嵌 JS）

**Interfaces:**
- Consumes: Task 1 `register_pool`/`cancel_by_token`；Task 2 `query_cancelable`/`exec_cancelable`；`ReqState.req.cancel_token`（Task 5 填值；本任务读 `state.borrow::<ReqState>().req.cancel_token`，字段本任务先在 `http.rs` 加好 `pub cancel_token: Option<u64>`——`RequestInfo` 各处构造点补 `cancel_token: None`，机械）。
- Produces:
  ```rust
  op_db_query_begin(state, name, sql, params, opts: Option<DbCallOpts>) -> Result<{executionId: String}, JsErrorBox> // 守卫/授权/超时核定后：建 PoolExecShared + spawn 执行 + register_pool + spawn_local 后台跑
  op_db_query_poll(state, executionId, wait_ms: u64) -> Result<{done: bool, events: Vec<CallEvent>}, JsErrorBox> // drain events；未 done 时 Notify 等待至 wait_ms
  op_db_query_take(state, executionId) -> Result<Vec<Row>, JsErrorBox> // 仅 done；搬走 outcome；未 done → 参数错误
  // exec 同款三 op，take 返回 i64
  ```
  （Task 4 bootstrap 消费这 6 op。）

- [ ] **Step 1: 写失败测试**（db_exec.rs test 模块，照 `exec_bridge` harness 形态）

```rust
#[tokio::test(flavor = "current_thread")]
async fn legacy_query_registers_pool_execution() { … } // op_db_query 执行期间 ExecRegistry 有 exec_ entry（经 test 钩读 len）
#[tokio::test(flavor = "current_thread")]
async fn begin_returns_id_and_poll_drains_phases_then_take_rows() { … }
#[tokio::test(flavor = "current_thread")]
async fn cancel_before_submit_rejects_oj_db_cancelled() { … }
#[tokio::test(flavor = "current_thread")]
async fn take_before_done_is_param_error() { … }
#[tokio::test(flavor = "current_thread")]
async fn cancelled_stream_next_maps_to_structured_cancelled() { … } // db_stream.rs 现有 generic 文案 → OJ_DB_CANCELLED
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --release --lib db_exec`
Expected: FAIL（新 op 未注册/未实现）

- [ ] **Step 3: 实现**

query_impl 池臂：建 `PoolExecShared{owner, alias, cancel_token: req.cancel_token, …}` → `register_pool` → `race_statement(query_cancelable(...), …)` → settle 后 `remove(id)`（正常或出错都摘）；catch 到 Cancelled category → 保持原 category 透传（**查询已成功守卫**：成功结果不因后到的 cancel 改写）。
begin/poll/take：begin 做与 query_impl 相同的前置（守卫/授权/超时核定 + normalize_and_trace），随后 `tokio::spawn` 执行体（conn/meter 均 Send；不碰 OpState），op 立即返回 id；poll drain `events` + `done` 等待；take 搬 `PoolOutcome::Rows`。exec 三 op 同构。
`op_db_stream_next` 的 cancelled 分支：`JsErrorBox::from(BridgeError{category: Cancelled, …})`（文案保留「rows already delivered are kept」语义）。

- [ ] **Step 4: 跑测试确认通过 + 全 lib 绿**

Run: `set -o pipefail && cargo test --release --lib 2>&1 | tail -3`
Expected: 全绿

- [ ] **Step 5: Commit**

```bash
git add src/bridge/db_query.rs src/bridge/mod.rs src/bridge/db_stream.rs src/bridge/http.rs
git commit -m "feat(db): query/exec 全量登记可取消 + begin/poll/take 回调面 op

unix@vip.qq.com ai"
```

### Task 4: bootstrap.js 回调面 + stream onProgress

**Files:**
- Modify: `src/bridge/bootstrap.js`（`db.query` / `db.exec` 包装 + stream 两个消费模式的 onProgress）

**Interfaces:**
- Consumes: Task 3 的 6 op（`op_db_query_begin/poll/take`、`op_db_exec_begin/poll/take`）；既有 `dbOpWrap`、`callOpts`。
- Produces: JS 可见行为——`db.query(sql, params, {onOpen, onProgress})`：`onOpen(executionId)` 开跑后即调；`onProgress({phase, elapsedMs, rows?})` 节流 ≥100ms（`Date.now()` 判定；`done` 相位不受节流必发）；`db.stream(..., {onProgress})`：`{rows, bytes, elapsedMs}` 节流 ≥100ms。

- [ ] **Step 1: 写失败测试**（db_exec.rs harness 内嵌 JS，照既有 `xidOk` 形态）

```js
// 模块源：const seen = {open:null, phases:[]};
// await db.query("select 1 as x", null, {onOpen:(id)=>seen.open=id, onProgress:(p)=>seen.phases.push(p)});
// finish(seen.open.startsWith("exec_") && seen.phases.length>=2 && seen.phases.at(-1).phase==="done");
```
```js
// onOpen 抛异常：db.query("select 1", null, {onOpen(){throw new Error("boom")}}) 仍正常返回行
```
```js
// stream onProgress：for-await 消费 + onProgress 收集；断言最后一条 bytes>=rows 且 elapsedMs 单调
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --release --lib db_exec`
Expected: FAIL（opts.onOpen 被忽略——query 包装未分流）

- [ ] **Step 3: 实现**（bootstrap.js，守 7-bit ASCII）

`db.query` / `db.exec` 包装分流：`opts && (typeof opts.onOpen === "function" || typeof opts.onProgress === "function")` → begin/poll/take 循环：begin 拿 `{executionId}` → `opts.onOpen(executionId)`（**try/catch——抛异常记 `console.warn` 不影响执行**，设计 §7 钉死）→ `for(;;){ const r = await dbOpWrap(op_db_query_poll(id, 100)); r.events.forEach(emitProgress); if (r.done) break; }` → take。节流在 emitProgress 内（距上次 ≥100ms 才调用户回调；`done` 恒发）。无回调 → 原 `op_db_query` 直路（Task 3 已使其全量登记）。
stream：onRow 模式与 asyncIterator 模式的消费循环里，每次迭代 `Date.now()` 节流 ≥100ms 时 `dbOpWrap(op_db_stream_info(v.id))` 取 `{rows,bytes}` + 自算 elapsedMs 发 `opts.onProgress`。

- [ ] **Step 4: 跑测试确认通过**

Run: `set -o pipefail && cargo test --release --lib 2>&1 | tail -3`
Expected: 全绿

- [ ] **Step 5: Commit**

```bash
git add src/bridge/bootstrap.js src/bridge/db_exec.rs
git commit -m "feat(db): db.query/exec onOpen+onProgress 回调面，stream onProgress 节流上报

unix@vip.qq.com ai"
```

### Task 5: serve G2 挂断联动 + env-gated 真库腿

**Files:**
- Modify: `src/bridge/db_exec.rs`（`cancel_by_token` 已备；本任务无新 Rust）
- Modify: `serve/src/lib.rs`（`run_capture` 生成 token + `DisconnectGuard`）
- Modify: `serve/src/actor.rs`（无——token 走 RequestInfo 既有通道）
- Test: `oj/tests/e2e.rs`（挂断 e2e，env-gated PG）；`src/bridge/db_exec.rs`（真库取消腿）

**Interfaces:**
- Consumes: Task 1 `cancel_by_token`；Task 3 池臂登记读 `req.cancel_token`。
- Produces:
  ```rust
  // serve/src/lib.rs
  struct DisconnectGuard { token: u64, registry: Arc<ExecRegistry>, armed: bool }
  impl DisconnectGuard { fn disarm(&mut self); }
  impl Drop for DisconnectGuard { /* armed → tokio::spawn(cancel_by_token(token)) */ }
  ```
  ExecRegistry 需可 Arc 共享到 serve（核对 `StableState.exec_sessions` 已挂在 AppState 路径；若不可达，经 `only_js::bridge` 导出只读访问器——计划执行时核对，是则记 ruling）。

- [ ] **Step 1: 写失败测试**

```rust
// oj/tests/e2e.rs（env-gated PG；诚实 skip 样板照既有）
#[tokio::test(flavor = "current_thread")]
async fn disconnect_cancels_running_query_within_5s() { … }
// 起服务（handler: db.query("select pg_sleep(30)"))；发请求后 drop 连接；
// 轮询 pg_stat_activity（deadline 10s）断言查询消失 ≤5s。
#[tokio::test(flavor = "current_thread")]
async fn real_pg_cancel_maps_oj_db_cancelled() { … } // db_exec.rs：begin 长查询 → cancel → take 得 OJ_DB_CANCELLED
#[tokio::test(flavor = "current_thread")]
async fn real_mysql_cancel_same() { … }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --release -p oj --test e2e disconnect 2>&1 | tail -3`
Expected: FAIL（断开不取消——guard 不存在）

- [ ] **Step 3: 实现**

`RequestInfo.cancel_token: Option<u64>`（http.rs；各构造点补 None——serve 两处 + bridge 测试各处）。serve 静态原子 `CANCEL_TOKEN: AtomicU64`；`run_capture` admit 后生成 token 填 `RequestInfo`，创建 guard（token + ExecRegistry Arc）；`st.actor.run_module(...)` 返回后 `guard.disarm()`。Drop 实现照 PutStreamGuard：`armed` 时 `tokio::spawn(async move { registry.cancel_by_token(token); })`。
**Task 0 验证点**：`disconnect_cancels_running_query_within_5s` 即设计 §7 Task 0 的验证——若 hyper 不 drop handler future，本测试红 → 升级裁决（guard 移 response body），ledger 记 ruling。

- [ ] **Step 4: 跑测试确认通过**

Run: `set -o pipefail && cargo test --release --workspace 2>&1 | tail -3`
Expected: 全绿（含 env-gated 腿；容器不可达则 skip 计数可见）

- [ ] **Step 5: Commit**

```bash
git add serve/src/lib.rs src/bridge/http.rs src/bridge/db_exec.rs oj/tests/e2e.rs
git commit -m "feat(serve): 挂断即取消——请求级 cancel token + DisconnectGuard

unix@vip.qq.com ai"
```

### Task 6: release v0.1.114

**Files:**
- Modify: `oj/Cargo.toml`（version 0.1.114）、`Cargo.lock`
- Modify: `CHANGELOG.md`（v0.1.114 段）
- Modify: `docs/devkit/api-manual.md`（db.query/exec opts 扩 onOpen/onProgress；db.stream onProgress；db.cancel 受理池执行；新错误 OJ_DB_CANCELLED 行为；错误/限制表）
- Modify: `docs/devkit/SKILL.md`（陷阱速查：begin/poll 模型、节流语义、挂断取消尽力而为口径）
- Modify: `docs/devkit/scenarios.md`（取消按钮/进度条场景）
- Modify: `docs/devkit/README.md`（索引）

**Interfaces:**
- Consumes: Task 1-5 全部用户可见行为。
- Produces: 发布物（bin/ 归置 + 契约测试绿）。

- [ ] **Step 1: 版本 + CHANGELOG**

`oj/Cargo.toml` version = "0.1.114"；CHANGELOG v0.1.114 段：REQ-11 全交付（G1/G2/G3 + 机制说明 + MySQL KILL 语义说明——挂断/取消 = 连接关闭级）。

- [ ] **Step 2: devkit 四件对齐**

api-manual：`DBCallOpts` 增 `onOpen`/`onProgress`（query/exec）与 stream `onProgress`；`db.cancel` 现受理池执行；`OJ_DB_CANCELLED` 与 `OJ_DB_TIMEOUT` 区分表；挂断取消「尽力而为、已提交不写回」口径进限制表。SKILL：陷阱 3 条（onOpen 异常不杀执行；poll 模型下 done 相位必发；被取消连接报废导致池瞬时补员）。scenarios：「取消按钮」「进度条」两场景。README 索引行。

- [ ] **Step 3: 归置 + 契约 + 全量**

```bash
cargo fmt && cargo xtask build && cargo test --release -p xtask
set -o pipefail && cargo test --release --workspace 2>&1 | tail -3   # 全绿
cargo clippy --release --workspace --all-targets -- -D warnings       # 0 警
```

Expected: xtask 10/10；workspace 全绿；clippy Finished 0 警。

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "release: v0.1.114——REQ-11 非流式取消与执行进度全交付

unix@vip.qq.com ai"
```

## Self-Review 记录

- **Spec 覆盖**：G1（T1-T3）、G2（T5）、G3 query/exec 相位（T3/T4）+ stream onProgress（T4）、CANCELLED 映射（T3）、被 kill 连接报废（T2 close_on_drop）、幂等/bearer（T1 既有语义保持 + 新用例）。无缺口。
- **接口一致性**：PoolExecShared 字段在 T1 定义、T2 消费、T3 读 cancel_token、T5 cancel_by_token——同名同型。6 个新 op 名 T3 定义、T4 消费。
- **修正案 #2 影响面**：T2 无 pid/DSN；T5 无池外连接——spec §4.1/§4.2 修订后自洽。
- **已知诚实偏差**：sqlite 无服务端语句（cancel = 客户端早停，category 仍 CANCELLED）；插件轴不可取消（默认实现）——均入 devkit 限制表。
