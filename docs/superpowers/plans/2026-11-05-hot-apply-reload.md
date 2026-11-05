# 热装配（reload 覆盖 manifests）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `_system/config/reload` 把 `manifests.yaml`（release 锁）纳入变化检测与重装——装/卸解决方案免重启生效，`applied` 报 `+/-name@ver`。

**Architecture:** 锁 sha 进 `do_reload` 幂等判定；`from_config` 复用路径影子装配 `PerGenAssembly`（registry+modules+tenant 三旗标）但只在 `commit` 返回后无 await 间隔换槽（单 `RwLock<Arc>` 槽 Arc 经 Extras 穿参跨全部 Bridge 共享）；快照钉 `ReqState`，8 读点改读快照；anon 短路收敛 `admit()`（严格 auth 名单）；cron driver 迁独立线程 + resume 闸防双发。

**Tech Stack:** Rust（axum / tokio / deno_core bridge / std RwLock）。

**Spec:** `docs/superpowers/specs/2026-11-05-hot-apply-reload-design.md`（实现者必读，含 D1–D7 决策与 B1–B19 用例表）。

## Global Constraints

- 测试一律 `cargo test --release`；门禁 `cargo fmt --check` + `cargo clippy --release --workspace --all-targets -- -D warnings`；**禁 debug 构建**。
- 条件等待一律 deadline/有界轮询，**禁固定 sleep 当同步**；异步测试 `#[tokio::test(flavor = "current_thread")]`。
- 不触 `Arc::get_mut`；不改 oj-plugin-ffi ABI（`AuthGuard` trait 不动）；`bootstrap.js` 不触碰。
- cargo test 接管道必须 `set -o pipefail`；不得并发两个 cargo test。
- 版本 bump：`oj/Cargo.toml` 0.1.121 → **0.1.122**；CHANGELOG.md 顶部加条目。

## Review Focus

1. 锁文件 yaml 解析错 / NotFound → reload 400 点名 / 视为空锁，不 panic、不静默当无变化（Task 4 单测钉）。
2. 多挂载树仅部分挂载有锁 → per-file 判定，跨挂载迁移不空转（Task 4 单测钉，B18）。
3. 换代瞬间的在途请求 → 快照一致性、无混代、`bound_db` 不改道（Task 1+2 快照钉 ReqState，B12）。
4. anon 名单含保留段（`_platform`/`_system`/`auth`/`_lifecycle`/`tasks`）→ 启动与 reload 均装配 Err（Task 6 单测钉，B10）。
5. `oj test`/嵌入式（Extras 默认、槽 None）→ 槽 fallback 构造必须可用，一切新代码默认路径可跑（Task 1 单测钉）。

---

### Task 1: `PerGenAssembly` 类型 + StableState/Extras 槽位（bridge 层）

**Files:**
- Modify: `src/bridge/mod.rs`（PerGenAssembly 定义 ~ModuleCtx 旁；StableState 字段替换 `registry`/`modules`/`ownership_deny`/`sql_guard`/`allow_as_tenant` → `assembly: Arc<RwLock<Arc<PerGenAssembly>>>`；Extras 增 `assembly: Option<...>`（Default None，既有 modules/sql_guard/ownership_deny/allow_as_tenant 字段保留作 fallback 源）；`with_dbs_and_loader`：Extras.assembly 为 None → 由 `registry` 参数 + fallback 字段包新槽，Some → 用传入（registry 参数忽略）；`ReqState` 增 `assembly: Option<Arc<PerGenAssembly>>`；`checkout_reset` 增参 `assembly: Arc<PerGenAssembly>` 并钉进 `rs.assembly`；`run_module`（~1604）先 `stable.assembly_snapshot()` 再祖先命中；全部 `checkout_reset` 调用点穿快照）
- Test: `src/bridge/mod.rs` `#[cfg(test)]`（或既有 tests 文件）

**Interfaces:**
- Produces:
  - `pub struct PerGenAssembly { pub registry: Arc<SchemaRegistry>, pub modules: Arc<HashMap<String, ModuleCtx>>, pub sql_guard: SqlGuard, pub ownership_deny: bool, pub allow_as_tenant: bool }`
  - `impl StableState { pub fn assembly_snapshot(&self) -> Arc<PerGenAssembly>`（中毒 `unwrap_or_else(|p| p.into_inner())`，先例 app.rs:2144）；`pub fn swap_assembly(&self, next: PerGenAssembly)` }
  - `pub type AssemblySlot = Arc<std::sync::RwLock<Arc<PerGenAssembly>>>`；`Extras.assembly: Option<AssemblySlot>`
  - `ReqState.assembly: Option<Arc<PerGenAssembly>>`（读点 `rs.assembly.clone().expect("assembly pinned at checkout")`）

- [ ] Step 1: 写失败单测：`slot_fallback_from_registry_and_extras`（Extras 默认 + registry → snapshot 读到该 registry/modules/旗标）；`swap_assembly_visible_in_snapshot`（swap 后 snapshot 读到新表）。
- [ ] Step 2: `cargo test --release -p only-js slot_fallback swap_assembly` → FAIL（类型不存在）。
- [ ] Step 3: 按上签名实现；RwLock 读路径锁内 clone 外层 Arc 即出锁。
- [ ] Step 4: 同命令 PASS；`cargo check --release -p only-js` 顶住（此时 8 读点编译失败是预期——Task 2 收）。
- [ ] Step 5: Commit `feat(bridge): PerGenAssembly 换代槽——StableState registry/modules/tenant 旗标打包单指针`

### Task 2: 8 读点 + 旗标执法点改读 ReqState 快照

**Files:**
- Modify: `src/bridge/guard.rs:243,250,260,398,401,512`；`src/bridge/db_import.rs:1252,1283`；`src/bridge/query.rs:649`（编译器穷尽兜底——凡读 `stable.registry/modules/sql_guard/ownership_deny/allow_as_tenant` 处一律改 `ReqState.assembly`；guard.rs 辅助函数改收 `&PerGenAssembly` 参数，op 调用方穿 `&rs.assembly`）

**Interfaces:**
- Consumes: Task 1 的 `ReqState.assembly` / `PerGenAssembly` 字段。
- Produces: 无新接口；`query.rs:649` 返回 `Arc<SchemaRegistry>` 签名不变。

- [ ] Step 1: 逐文件改读点（机械替换；helper 签名扩参）。
- [ ] Step 2: `set -o pipefail; cargo test --release -p only-js 2>&1 | tail -20` → 全绿（既有 guard/query/db_import 测试即回归网）。
- [ ] Step 3: Commit `refactor(bridge): registry/modules/tenant 旗标读点统一走 ReqState 快照`

### Task 3: from_config 影子装配 + 抢注检查 + 空锁放行 + Backend 换代句柄

**Files:**
- Modify: `oj/src/app.rs`（make_bridge_of 闭包捕获槽 Arc → Extras.assembly 传同一 Arc，删 registry/modules 影绑；Backend 侧 StableState ~1734 同槽；`impl Backend { pub fn swap_assembly(&self, next: PerGenAssembly) }`；`from_config` 复用路径：重跑 `build_schema_and_modules`（参数取新代 cfg）→ `App.pending_assembly: Option<PerGenAssembly>` 暂存**不 swap**；新 `fn check_platform_squatting(stable:&StableState, asm:&PerGenAssembly) -> Result<(), String>`（新模块名 ∩ `stable.platform` 特权名单 → Err 点名，语义对齐 build_platform_grants app.rs:557-565）；release 路由构建空锁检查改 `if lock.is_empty() && !reused { Err }`）
- Test: `oj/src/app.rs` 测试区或新 `oj/src/app_assembly_tests.rs`

**Interfaces:**
- Consumes: Task 1 槽位；`build_schema_and_modules`（既有，返回 `(SchemaRegistry, Arc<HashMap<..>>)`）。
- Produces: `App::take_pending_assembly(&mut self) -> Option<PerGenAssembly>`；`Backend::swap_assembly`；`check_platform_squatting`。

- [ ] Step 1: 失败单测：`squatting_check_rejects_privileged_name`（构造 stable.platform 含 `admin`、asm.modules 含 admin → Err 含点名）；`empty_lock_allowed_on_reuse_rejected_on_cold`（函数级：抽 `fn release_route_lock_check(lock:&BTreeMap<..>, reused:bool) -> Result<(),String>` 供测）。
- [ ] Step 2: `cargo test --release -p oj squatting empty_lock` → FAIL。
- [ ] Step 3: 实现上述全部（from_config 签名不动，pending_assembly 仅复用路径 Some）。
- [ ] Step 4: PASS + `cargo test --release -p oj 2>&1 | tail` 全绿。
- [ ] Step 5: Commit `feat(oj): 复用路径影子装配 PerGenAssembly + 热装抢注拦截 + 空锁卸载放行`

### Task 4: do_reload 锁检测/diff/applied + commit 后无间隔换 + diff_configs 修正

**Files:**
- Modify: `oj/src/serve_cmd.rs`（`HotReloadCtx` 增 `current_locks: tokio::sync::Mutex<HashMap<PathBuf, BTreeMap<String,String>>>`；`serve_hot_reload_branch` 初读（helper `fn release_locks(cfg:&Config, config_dir:&Path) -> Result<HashMap<..>, String>`：resolve_mounts → 逐 release 树 `manifest::load_lock`）；`do_reload`：cfg 归一后重读锁 → per-file 比较（任一变化或 config sha 变化 → 继续；否则 `unchanged=true`）→ `lock_diff` 产 `+/-name@ver` 追加 applied → from_config → 任务交接（Task 7/8）→ commit → **无 await 间隔** `take_pending_assembly` + `swap_assembly` + `current_doc/sha/current_locks` 落盘；`diff_configs` 把 `vars`/`oidc`/`db_query`/`db_trace`/`ws` 归 restart_pending）
- Modify: `oj/src/serve_cmd_hot_reload_tests.rs`

**Interfaces:**
- Consumes: Task 3 `take_pending_assembly`/`Backend::swap_assembly`；`manifest::load_lock`。
- Produces:
  - `fn lock_diff(old:&std::collections::BTreeMap<String,String>, new:&std::collections::BTreeMap<String,String>) -> Vec<String>`（增 `+{m}@{v}`，删 `-{m}@{v}`，按名排序）
  - `fn release_locks(cfg:&Config, config_dir:&Path) -> Result<HashMap<PathBuf, std::collections::BTreeMap<String,String>>, String>`
  - `fn locks_changed(old:&HashMap<..>, new:&HashMap<..>) -> bool`（per-file 比较，键集差或任一值差 = true）

- [ ] Step 1: 失败单测（serve_cmd_hot_reload_tests.rs）：`lock_diff_add_remove_upgrade`（B6：`{a:1.0.0}`→`{a:1.0.1}` → `["-a@1.0.0","+a@1.0.1"]`）；`locks_changed_per_file_cross_mount_move`（B18：两文件互换条目 → true）；`diff_configs_frozen_sections_now_restart_pending`（vars/oidc/db_query/db_trace/ws 改动 → restart_pending，B19 后半）。
- [ ] Step 2: `cargo test --release -p oj lock_diff locks_changed frozen_sections` → FAIL。
- [ ] Step 3: 实现三函数 + do_reload 接线（锁解析错 → `(400, 点名文件)`；NotFound=空锁；`swap` 在 `committer.commit` 返回后与指针落盘同一同步段）。
- [ ] Step 4: PASS + 既有 hot_reload_tests 全绿（B3/B5 回归）。
- [ ] Step 5: Commit `feat(oj): reload 变化检测纳入 manifests 锁——applied 报 +/-name@ver、commit 后换代`

### Task 5: GET `_system/config` modules 摘要 + ojInfo capabilities

**Files:**
- Modify: `serve/src/reload.rs`（`ReloadMeta` 增 `modules: Vec<String>`；`ReloadState::new` 增参；`Committer::commit` 增参 `modules: Vec<String>`；GET 返回体加 `"modules"` 键）
- Modify: `oj/src/serve_cmd.rs`（branch 初值 = 基线锁并集 `name@ver` 排序；do_reload commit 传新锁并集；`assemble_ojinfo` build json 增 `"capabilities": ["reload.assemble_modules","reload.anon_per_reload","reload.cron_survives"]`）
- Test: `serve/src/reload.rs` tests + `oj/src/serve_cmd_hot_reload_tests.rs`

- [ ] Step 1: 失败测试：reload.rs `get_summary_includes_modules`；hot_reload_tests `ojinfo_has_capabilities`（assemble_ojinfo 需 cfg/top/loaded/reg —— 用最小夹具）。
- [ ] Step 2: `cargo test --release -p serve get_summary -p oj ojinfo_has` → FAIL。
- [ ] Step 3: 实现（`ReloadState::new` 调用点/`commit` 调用点随之更新——唯一调用方在 serve_cmd）。
- [ ] Step 4: PASS。
- [ ] Step 5: Commit `feat(serve,oj): _system/config GET 报挂载模块清单；ojInfo 增 reload capabilities`

### Task 6: anon 预短路（admit 咽喉）+ 保留段黑名单

**Files:**
- Modify: `serve/src/lib.rs`（`Pipeline` 增 `auth_anon: Vec<String>`，Default 空）
- Modify: `serve/src/dispatch.rs` `admit`（verify 之前：`path_matches(&st.pipeline.auth_anon, p)` 命中 → `user=None`、**hdr_map 照建**、跳过 `guard.verify`；tenant 段逻辑一字不动）
- Modify: `oj/src/app.rs:2210`（`auth_anon: config::anon_paths(&cfg.auth.anonymous_paths)`——沿用 tenant 同款 helper；若 auth 段路径字段名不同以 config.rs 实际为准）
- Modify: `src/config.rs` `validate_anon_paths`（:704——`auth.anonymous_paths` 条目首段 ∈ {_platform,_system,auth,_lifecycle,tasks} → Err 点名）
- Test: `serve/src/lib_tests.rs` + `src/config.rs` 测试区

**Interfaces:**
- Consumes: `serve::path_matches`（既有）、`config::anon_paths`（既有）。
- Produces: `Pipeline.auth_anon`；validate 新文案 `anonymous path {p} under reserved prefix /{seg} — denied`。

- [ ] Step 1: 失败测试：config `anon_paths_reserved_prefix_rejected`（`_system/**` 与 `tasks/**` → Err）；lib_tests `admit_short_circuits_auth_anon_without_verify`（假 guard 记录 verify 调用数，命中名单 → 0 次、hdr_map 仍 Some）+ `admit_tenant_anon_still_verifies`（回归：tenant_anon 命中 → verify 照跑）。
- [ ] Step 2: `cargo test --release -p serve -p only-js anon` → FAIL。
- [ ] Step 3: 实现三处。
- [ ] Step 4: PASS + 既有 tenant_anon 测试绿（B9 的 tenant 半边回归）。
- [ ] Step 5: Commit `feat(serve): auth.anonymous_paths 每代 admit 预短路；保留段黑名单拒装`

### Task 7: cron 宿主修复 + resume 闸（driver 线程化 + TLA 同门）

**Files:**
- Modify: `src/bridge/task_pool.rs`（`TaskPool::new` 增参 `resume: Arc<AtomicBool>`，存字段并暴露 `pub fn resume_handle(&self) -> Arc<AtomicBool>`；既有测试构造点传 `Arc::new(AtomicBool::new(true))`）
- Modify: `oj/src/tasks.rs`（`assemble_tasking` 增参 `resume: Arc<AtomicBool>` 透传 pool 与 sup；`spawn_cron_driver` 改 `std::thread::Builder::name("cron-driver").spawn` + 自持 current_thread runtime（worker 先例 task_pool.rs:662-671），driver 循环顶 flag 检查后：`!resume` → `tokio::select!{ resume 置位 / flag 置位 }` 阻塞等待；`TaskSupervisor::spawn_selected` 增 resume 参数——子进程拉起 hold 到放闸（实现：spawn 前在自身线程 wait resume/flag））
- Test: `oj/src/tasks.rs` 测试区 + task_pool 既有测试

**Interfaces:**
- Consumes: `TaskRegistry::set_next_run`（task_pool.rs:335，B11 注入点）。
- Produces: `assemble_tasking(cfg, root, make_bridge, flag, resume)`（5 参）；`TaskPool::new(workers, loop_timeout, interval, make_bridge, registry, resume)`（6 参）；`TaskSupervisor::spawn_selected(cfg, tla, make_bridge, flag, backoff, resume)`。

- [ ] Step 1: 失败测试 `cron_gate_holds_then_fires_once`：闭门池 + `registry.set_next_run(now+2s)` + spawn driver（直接调 assemble_tasking 不可行则抽 driver 可测形态）→ deadline 轮询 3s 无 `cron.triggered`；放闸 → 恰一次触发（事件日志或 registry.bump_run 断言）。
- [ ] Step 2: `cargo test --release -p oj cron_gate` → FAIL。
- [ ] Step 3: 实现宿主迁移 + 闸（`resume` 初值语义：TaskPool 不自设，调用方定——启动路径传 `true`、reload 路径传 `false`）。
- [ ] Step 4: PASS + task_pool/tasks 既有测试全绿。
- [ ] Step 5: Commit `fix(tasks): cron driver 迁独立线程随进程存活；换代 resume 闸关双发窗口`

### Task 8: do_reload 任务交接接线 + 有界停机 + 失败兜底

**Files:**
- Modify: `oj/src/serve_cmd.rs`（`do_reload`：`resume` Arc 建 closed → `assemble_tasking(..., new_flag, resume.clone())`；旧池 `shutdown_and_join` 包 10s 有界等待（超时 → `(500, "old task pool did not settle; scheduling paused until next successful reload")`）；旧 sup shutdown 后 `resume.store(true)`；`assemble_tasking` Err / join Err 路径对新池 `shutdown_and_join()` 兜底后返 Err；启动路径 `serve_cmd.rs:204` 传 open gate）

**Interfaces:**
- Consumes: Task 7 全部签名。

- [ ] Step 1: `set -o pipefail; cargo test --release -p oj tasks 2>&1 | tail` 既有绿（次序契约由 Task 7 测试 + B11 e2e 覆盖）。
- [ ] Step 2: Commit `feat(oj): reload 任务池交接——新池闭门建、旧池有界停、放闸后才 commit`

### Task 9: e2e BDD 用例（B1/B2/B16/B17 + B11 分钟腿）

**Files:**
- Modify: `oj/tests/e2e.rs`（沿既有 release 夹具形态：`oj build` 产 dist → serve → deadline 轮询就绪）

- [ ] Step 1: 夹具 + 用例：`hot_apply_install_uninstall_cycle`（B1+B2：装 → reload 断言 `unchanged=false` 且 applied⊇`+b@1.0.0` → GET b 200 → 卸 → applied⊇`-b@1.0.0` → GET b 404）；`hot_apply_squatting_rejected`（B16）；`hot_apply_uninstall_last_module`（B17）；`cron_survives_reload_fires_once`（B11 e2e 腿：`* * * * *` 对齐分钟边界 + deadline 轮询恰一次）。夹具构建/就绪等待沿仓库既有 e2e helper，禁固定 sleep。
- [ ] Step 2: `set -o pipefail; cargo test --release -p oj --test e2e hot_apply cron_survives 2>&1 | tail -20` → PASS（e2e 较慢属预期）。
- [ ] Step 3: Commit `test(e2e): 热装配 BDD——装/卸/抢注/空锁/cron 存活`

### Task 10: 版本 bump + CHANGELOG + devkit 四件同步 + 门禁

**Files:**
- Modify: `oj/Cargo.toml`（0.1.121 → 0.1.122）
- Modify: `CHANGELOG.md`（顶部条目：feat 热装配 + 两处如实登记——cron driver 宿主 bug 修复、applied 真实性行为修正）
- Modify: `docs/devkit/` 四件（api-manual.md：`applied` 模块条目格式/GET modules 键/capabilities/anon 保留段新报错/空锁卸载语义进对应章节与错误表；scenarios.md：热装/卸场景；SKILL.md：陷阱速查——「卸载后 anon 删除需重启」「tenant.anonymous_paths 不免验签不变」；README.md：概览对齐）
- Modify: `docs/superpowers/specs/2026-11-05-hot-apply-reload-design.md` 状态行（已实现）

- [ ] Step 1: bump + CHANGELOG + 四件按用户可见变更逐条对齐。
- [ ] Step 2: `cargo xtask build` 归置 bin；`set -o pipefail; cargo test --release -p xtask 2>&1 | tail` devkit 契约用例绿。
- [ ] Step 3: `cargo fmt` → `cargo clippy --release --workspace --all-targets -- -D warnings` → `set -o pipefail; cargo test --release --workspace 2>&1 | tail -20` 全绿。
- [ ] Step 4: Commit `chore(release): v0.1.122——热装配（reload 覆盖 manifests）+ devkit 四件同步`
