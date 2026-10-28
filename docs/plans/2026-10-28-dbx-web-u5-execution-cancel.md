# U-5 execution-id 取消状态机 实施计划（v0.1.96）

> 读者：接手实施的人。需求原文：`docs/prds/oj-feature-requirements.md` §U-5；
> 背景与不变量：`docs/handoff-2026-10-28-dbx-web-l7.md` §4/§6。

**Goal**：执行标识注册表（进程级），`db.cancel(executionId)` 跨请求可达；请求 B 取消请求 A 的流式查询，A 收 cancelled 错误信封，后端真取消（mysql `KILL QUERY` / pg `pg_cancel_backend` / 其余协作式），重复 cancel 幂等。

**Architecture**：**不动 ABI、不动插件**。跨请求取消 = 宿主级登记表（`StableState.exec_sessions`）把 `exec_<boot>_<hex>` id 映射到流共享句柄 `Arc<DbStreamShared>`；`db.cancel(id)` 归属校验后 `notify` 既有 pump，pump 停止拉取 → `StreamCursorGuard::drop`（ffi.rs）自动 fire `stream_cancel`（插件真取消）+ `stream_close`（连接回池）。一条代码路径复用全部既有机制。

**Tech Stack**：Rust（deno_core op2）、既有 `handle_core`（id/Owner）、`tokio::sync::Notify`。

**范围裁决**（与 PRD 的偏差，诚实登记）：
- **只登记流式执行**（`db.stream`）。非流式 `db.query/exec` 无后端句柄，宿主侧协作取消只能丢 future（连接中途报废、服务端查询仍在跑）= 假取消，**不做**；文档登记为已知限制，真覆盖需插件侧 one-shot 执行登记（随 ABI 演进另期）。
- **不加配置段**：执行登记无门禁（流本来经 `db_access` 逐语句授权）；`db.cancel` 只按 `Owner::same_subject` 判权（tenant+user），匿名同域可互消（与匿名请求同信任域，id 为 32-hex 随机不可枚举）。

## Global Constraints

- `bootstrap.js` 保持 7-bit ASCII；新增 op 名必须手动加进顶部 import 列表。
- id 红线：`exec_` 前缀 + `handle_core::new_id/id_is_wellformed/id_prefix`；日志/错误只出前 8 字符；外来 id 形状白名单先行。
- 他人 id / 未知 id / 形状非法 → **同一条** not-found 文案（不泄露存在性）。
- JS 面每个方法必须返回 Promise（同步 op 用 `dbSessionCall` 同款包装）。
- `StableState` 加字段必须同步全部测试字面量（`mod.rs` 测试 + `platform.rs`）。
- 测试一律 `--release`；异步测试 `tokio::test(flavor = "current_thread")`；sqlite 测试用临时文件（非 `:memory:`）。

## Review Focus

1. **跨主体取消**：B（不同 tenant/user）拿 A 的 id cancel → 必须 not-found，且流不被触碰。（Task 1 测试钉）
2. **取消后 A 的观感**：A 的下一次 `next()` 抛 cancelled 错误（不是静默 done——静默 done 会让消费方误以为数据取完了）。（Task 3 测试钉）
3. **幂等**：同一 id 连续两次 cancel → 两次都成功；entry 不因 cancel 移除。（Task 1 测试钉）
4. **孤儿 entry**：流被放弃、JS 永不 close → entry 由懒惰清扫回收（open/cancel 时顺带扫 done 与超龄），无后台 reaper。（Task 1 测试钉）
5. **取消竞态**：cancel 到达时流已自然结束（entry 已被 close 移除）→ 与未知 id 同文案（单次性登记，文档写明）。（Task 1 测试钉）

---

### Task 1: `src/bridge/db_exec.rs` —— ExecRegistry + 单测

**Files**: Create `src/bridge/db_exec.rs`；Modify `src/bridge/mod.rs`（`pub mod db_exec;`）

**Interfaces**（产出，后续任务依赖）:
```rust
pub struct ExecRegistry { /* Mutex<HashMap<String, ExecEntry>> + boot: String */ }
impl Default for ExecRegistry
pub struct ExecEntry { pub shared: Arc<db::DbStreamShared>, /* owner/alias/id/created */ }
impl ExecRegistry {
    pub fn register(&self, owner: handle_core::Owner, alias: String, shared: Arc<db::DbStreamShared>) -> String; // 返回新 exec id，同时懒惰清扫
    /// Some(alias) = 已通知取消（幂等，重复调用同值）；None = not-found（文案由调用方统一构造）
    pub(crate) fn cancel(&self, id: &str, owner: &handle_core::Owner) -> Option<String /*alias*/>;
    pub fn remove(&self, id: &str); // op_db_stream_close 调
    pub fn len(&self) -> usize; // 测试/诊断
}
pub const PREFIX: &str = "exec_";
```
`DbStreamShared` 需加两个字段（Task 3）：`cancelled: AtomicBool`（跨请求取消标志）、`exec_id: Option<String>`（反向句柄，供 close 摘除）。

- [ ] 测试：register→cancel 同主体 Ok、二次 cancel 仍 Ok（幂等）；异主体 Err；畸形 id Err；close(remove) 后 cancel Err；register 时 done 流被清扫（孤儿回收）。
- [ ] `cargo test --release -p only-js db_exec` 绿 → 提交。

### Task 2: 状态装配 —— StableState/Extras/测试字面量

**Files**: `src/bridge/mod.rs`（StableState 字段 `exec_sessions: Arc<db_exec::ExecRegistry>` + `Extras.exec_sessions: Option<...>` + 装配 unwrap_or_default + op 注册 + mod.rs/platform.rs 两处测试字面量）；`oj/src/app.rs`（两处 Extras 注入同一 registry，与 `tx_sessions` 同款——热更存活）。

- [ ] 全量编译 + `cargo test --release -p only-js`（platform 字面量只在 `--all-targets` 暴露，最后统一跑 workspace 门禁）。

### Task 3: 流 open/next/close 接线 + `op_db_cancel`

**Files**: `src/bridge/db.rs`

- `op_db_stream_open`：返回 `#[serde] Value` `{id, executionId}`（原 f64）；开流成功后 `exec_sessions.register(...)`，id 写回 `shared.exec_id`。
- `op_db_stream_next`：recv 前 `cancelled` 标志 → `Err("db stream <exec_前缀> was cancelled via db.cancel(...)")`。
- `op_db_stream_close`：顺带 `exec_sessions.remove(exec_id)`。
- 新增 `op_db_cancel(state, #[string] id) -> Result<Value, JsErrorBox>`：`owner_of` → `cancel()` → Ok `{cancelled:true, alias}` / Err → 统一 not-found 文案（`handle_core::id_prefix`）。
- [ ] 既有 stream 测试适配新返回形状；新增 Bridge 级测试：请求 A 开流拿 executionId → 请求 B（同主体）cancel → A `next` 抛 cancelled；B（异主体）cancel → not-found。
- [ ] 测试绿 → 提交。

### Task 4: JS 面 —— bootstrap.js + global.d.ts

**Files**: `src/bridge/bootstrap.js`（import `op_db_cancel`；`stream` 包装适配 `{id,executionId}`；迭代器对象加 `executionId: () => Promise<string>`；回调形式加 `opts.onOpen(executionId)`；per-name 对象加 `cancel: dbSessionCall((id) => op_db_cancel(String(id)))`）；`sample/global.d.ts`（StreamOptions.onOpen、迭代器 `.executionId()`、DBInstance.cancel）。

- [ ] ASCII 检查 + `cargo build --release` 内嵌表随之重建；`cargo test --release -p oj --test e2e` 绿。

### Task 5: 文档 + 版本 + 门禁 + 提交

- CHANGELOG `v0.1.96`；`oj/Cargo.toml` 0.1.96（Cargo.lock 同步）；devkit 四件（api-manual §6 db.cancel/stream.executionId、scenarios 场景 50、SKILL.md 陷阱：取消后 next 抛错而非 done、README 版本行）；db-guide 流式章节 + 已知限制（非流式不覆盖）。
- `cargo fmt` / `clippy --release --workspace --all-targets -D warnings` / `cargo test --release --workspace` / `cargo xtask build` + smoke + `cargo test --release -p xtask`。
- 提交：`feat(db): U-5 execution-id 跨请求取消（db.cancel，v0.1.96）`。
