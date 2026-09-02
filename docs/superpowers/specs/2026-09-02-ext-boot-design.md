# ext_boot.js：运行时创建期动态补充设计

日期：2026-09-02
状态：已批准（设计）· **v2 — 按三方评审修订（架构 / 实现 / 质量风险）· 已实现**

> v2 相对 v1 的实质变更已就地融入正文，并在文末「评审裁定」一节保留逐条依据与
> 驳回理由。v1 的核心骨架（约定路径、checkout 单点、不做热重载、不增配置键）保留。
>
> **实现期新增发现**（v2 正文已就地更正，此处索引）：
> - **D-9** —— `looks_cjs` 启发式会把「无 import/export 的 boot 文件」包进非 async
>   函数，顶层 await 直接 SyntaxError。boot 文件必须带 ESM 语法标记（见「错误处理」）。
> - **D-10** —— 「永不到达」的 TLA（`await new Promise(()=>{})`）**不挂起**，deno_core
>   在事件循环排空时立即报 `Top-level await promise never resolved`。真正的挂起只有
>   同步死循环与悬而未决的 op 两类；B5 的 KillSwitch 针对前者。

## 背景与动机

`bootstrap.js` 经 `deno_core::extension!` 宏在**编译期**嵌入二进制
（`src/bridge/mod.rs` 的 `esm = [dir "src/bridge", "bootstrap.js"]`），每个
`JsRuntime` 创建时作为扩展入口执行，装配 `json`/`db`/`http` 等全局。
代价：给全局对象增补辅助方法（如 `json.page()`、`log.trace`）必须重编二进制。

目标：运行时创建实例时，若存在 `<config_dir>/ext_boot.js` 则加载执行一次，
作为 bootstrap 的**动态补充**——不重编即可扩展全局。

**v2 定位澄清**：ext_boot 是 **JS 层的组合/扩展层**，不是新的能力通道。它只能
基于已注入的全局（`json`/`db`/`fetch`/…）做组合，拿不到原始 op（见 B1）。

## 决策记录（v2）

| 决策点 | 结论 | 理由 |
|---|---|---|
| 能力档位 | **ESM 模块**（支持 import / TLA），**仅组合已有全局，无原始 op** | ext: 导入被 deno_core 硬拦（B1）；能力下发走编译期 op，不由 ext_boot 承担 |
| 文件位置 | 约定 `<config_dir>/ext_boot.js`，存在即加载 | dev/release 一致；与 config.yaml/plugins_dir 同级运维信任边界 |
| 执行点 | `RuntimePool::checkout` 改 async，boot 收进其中 | 唯一借出入口 = 不变量「checkout 出的 runtime 一定已 boot」在 crate 内单点成立 |
| **失败语义（v2 新增）** | **启动期 prewarm → `Err`；运行期 checkout → `RunError`，绝不 panic** | actor 线程 panic 会永久杀死 worker（B2） |
| **启动期预热（v2 新增）** | `App::from_config` 内、建表**之前**显式 prewarm 一次 | dev 内省线程吞 panic → 只 warn（B3），必须前移 |
| **预热形态（v2 新增）** | `std::thread::spawn` + `current_thread` + `block_on` | `oj/src/main.rs:51` 是 `#[tokio::main]`（multi_thread），`from_config` 的 future 须 `Send`，而 `Bridge: !Send` |
| **boot 超时（v2 新增）** | boot 前 `kill.arm(handle, BOOT_TIMEOUT)`；`BOOT_TIMEOUT` 单设常量（同为 2s） | 同步死循环下 `tokio::time::timeout` **不会触发**（event loop 不归还执行器），只能靠 `terminate_execution`。不复用 `INTROSPECT_TIMEOUT`：那是内省的常量，两者不应互相耦合 |
| 热重载 | **不做**，改文件需重启进程 | 池常驻；装配期冻结，杜绝池内新旧混杂（但见 D-6 的诚实说明） |
| 配置键 | 不加（约定优于配置） | 需要自定义路径/多文件时再议 |
| **日志（v2 新增）** | 装配期 `eprintln!` 打印路径与冻结 `?v=`；每 runtime 成功用 `tracing::debug!` | 静默失效（空文件/CJS 写法）的唯一兜底 |
| **`oj build`（v2 修正）** | **同样探测并加载** boot（`src.parent()/ext_boot.js`，与 `build_cmd.rs:215` 的 root 同源） | v1 称「零改动」实为「压根不跑」→ dev/build 路由表分叉（S-2） |
| **`oj test`（v2 修正）** | boot 抽成 `pub async fn boot_runtime`，`test_cmd.rs` 复用 | `test_cmd.rs:111-117` 直接 `JsRuntime::new`，绕过池 → `*.test.ts` 看不到 boot 全局（S-4） |

否决的替代方案：

- **同步工厂 + `futures::executor::block_on`**：diff 最小，但 boot 的 TLA 内
  做 IO（fetch/db）会死锁——隐蔽脚枪。
- **boot 放 `checkout_reset` + fresh 标记**：等价可行，但 `start_inspector`
  也直接 `checkout`，boot 调用点变两处；async checkout 单点更省。
- **放行 `ext:` scheme（v1 待定项，v2 删除）**：见 B1，不可实施且越权。
- **运行期 boot 失败继续 panic（v1）**：见 B2，会耗尽 actor 池。

## 数据流（v2）

```
装配期（App::from_config，一次性）:
  <config_dir>/ext_boot.js 存在？
    → versioned_specifier() 冻结 file://…?v=<mtime>
    → Extras.boot → StableState.boot（Arc 不可变）
    → eprintln: ext_boot loaded <abs path> (frozen ?v=<nanos>)
  ↓ 建表之前
  prewarm_boot(make_bridge)          // thread::spawn + current_thread + block_on
    Bridge → prewarm() → checkout → boot_runtime
    Err → App::from_config 返回 Err（真·启动失败，错误文案完整）

每个新 JsRuntime（RuntimePool::checkout 未命中空闲池时）:
  JsRuntime::new（扩展入口 bootstrap.js 先执行，全局已挂）
  → kill.arm(handle, BOOT_TIMEOUT)          // v2
  → boot_runtime: code = `await import("<spec>");`
    load_side_es_module_from_code + mod_evaluate + run_event_loop + eval.await
    （走 OjModuleLoader：.ts 缓存转译、相对/裸导入、CJS 互操作、ensure_within 全部复用）
  → kill.disarm()；fired → 视为 boot 失败
  → Err → 兜底再跑一轮 run_event_loop，runtime 丢弃，返回 Err（不 panic）  // v2
  → 空闲池命中（复用）不重跑
```

- **boot 的 driver spec**：固定 `file:///oj/ext_boot.js`。`run_side_driver` 用
  递增 `file:///oj/driver/{n}.js` 只为绕开「同一 runtime 多请求撞
  MainModuleAlreadyExists」；每 JsRuntime 有独立 module map，boot 每 runtime
  一次，固定 spec 与后续 `driver/{n}` 不冲突。
- **执行顺序**：boot 在 `ReqState.reset` 之前 → boot 期误调 `json.ok` 写入的是
  随后即被重置的 per-request 状态，无泄漏（`OpState` 在 `mod.rs:218` 已 put
  `ReqState::default()`）。

## 错误处理与边界（v2）

### 阻断项（必须按此实现）

- **B1 · `ext:core/ops` 逃生口删除**。deno_core 在 loader resolve **之后**还有第二道闸：
  `modules/map.rs:1577-1605` 的 `validate_ext_module_import` —— 若 resolved scheme 为
  `ext`，要求 referrer 是内部模块**且** `loading_internal_modules` 置位（运行期恒 false），
  否则 `TypeError: Importing ext: modules is only allowed from ext: and node: modules`。
  `file://` referrer 永远过不去。故 v1「加 2 行放行 `scheme == "ext"`」**即使写了也无效**，
  却会在 `resolve_inner`（`module_loader.rs:57`，全模块共用）留下一条对所有 handler 生效的
  宽松分支，与「信任边界」一节自相矛盾。**结论：删除该逃生口，测试 #5 取消。**
  ext_boot 需要新能力 → 那是编译期 op 的事。
- **B2 · 运行期 boot 失败绝不 panic**。`server/src/actor.rs:70-95` 每个 actor 线程各自
  `make()` 一个 Bridge，**只有收到 job 才 checkout** → boot 落在首个请求上。`expect`
  panic 在 `block_on` 内传播 → 线程死亡 → 轮询到该 actor 的请求全返回
  `js actor stopped`（`actor.rs:151`），pool_size 个 actor 被逐个耗尽。
  修法：checkout 内 boot 失败返回 `Result`（→ `RunError::Core`），与 handler 失败同路径。
- **B3 · 启动期失败必须前移，不能靠内省间接暴露**。`routes.rs:470-471` 的线程 panic 被
  `.join().unwrap_or_else(|_| Err("introspect thread panicked"))` 吞成 `Err`，而
  `oj/src/app.rs:315-323` 只 `eprintln!` + warn 继续启动。且 `bridge_introspector`
  每 api.ts 起一线程一 Bridge（我司 `routes.rs:456-469`），一个 boot 语法错 → 全部线程
  panic → **路由全空、服务照常监听**。修法：prewarm 在 `RouteTable::build` **之前**执行，
  boot 错误单独 fail-fast，不复用内省失败路径。
- **B4 · boot 失败的 isolate 未轮询即 drop**。`runtime.rs:74-76` 自注「未轮询的 isolate
  析构会触发 V8 句柄错误」，且本项目有 SIGSEGV 前科（`runtime.rs:100-110`，Linux CI 暴露）。
  `expect` panic 时该 isolate 从未 `run_event_loop` 过。修法：Err 分支先
  `let _ = rt.run_event_loop(..).await;`（必要时 `terminate_execution()`）再丢弃/返回。
- **B5 · boot 无超时 = 永久挂起**。见决策表；用 KillSwitch 而非 `tokio::time::timeout`。

### 重要约束

- **boot 必须幂等、无外部副作用**。执行次数 = N(api.ts 内省) + N(actor pool) +
  N(**每个 WS 连接**，`ws.rs:201` 的 `let bridge = make();`)。写库 / publish bus /
  `fetch` 外部服务会被放大 N 倍，且 WS 每次连接再来一遍。
- **handler 顶层不得依赖 boot 注入的全局**（dev/build/release 一致性前提）。
- 运行期 ext_boot.js 被删/替换：装配期冻结的是 `?v=`，`load` 阶段仍读盘
  （`module_loader.rs:76-80`）→ 读盘失败 → 按 B2 返回 `RunError`，该请求 500，服务存活。
- 绝对 `file://` 导入在 `module_loader.rs:58-62` 原样放行、不过 `ensure_within`。
  boot 属运维信任域，接受，但在此写死。
- 无 ext_boot.js（现网默认）→ `Extras::default()` → 行为零变化。

### 建议级

- **D-6 · 冻结 `?v=` 并未真正杜绝新旧混杂**：装配期冻结 spec，但 `load` 阶段读的是
  **当前磁盘内容** → 老 runtime 旧码、新 runtime 新码，混杂照旧只是不可见。v2 选择
  「诚实说明 + 可观测」而非「每次 checkout 重新 stat」：启动日志打印冻结的 `?v=`，
  文档明写 **改 ext_boot.js 必须重启进程，框架不保证池内版本一致**。
- **D-7 · boot 注入的全局应锁死**：handler 删掉 `json.page` 后该 isolate 后续请求
  永久丢失（boot 不重跑）。建议 boot 内用
  `Object.defineProperty(..., {writable:false, configurable:false})`。
- **D-9 · boot 文件必须带 ESM 语法标记（实现期发现）**：`module_loader::load_specifier`
  对**所有** `.js` 施加 `looks_cjs` 启发式（无 `import`/`export` 即判为 CJS），再包进
  `wrap_cjs` 的 `(function (module, exports, require) { … })` —— 那是**非 async 函数**，
  顶层 `await` 会直接 SyntaxError。故：
  - 只用副作用的 boot（`globalThis.foo = 1`）没问题，被包装也照常执行；
  - **要用 TLA 的 boot 必须带一句 `export {};`**（或有真实 import/export），否则报的是
    SyntaxError 而非有用的错误。
  此项已写进 `boot_runs_once_per_runtime_not_per_request` 的注释与 `cjs_boot_has_no_global_effect`
  用例。根治（改 `looks_cjs` 以放行顶层 await，或让 boot 绕过该启发式）影响的是
  node_modules 包与全部项目 `.js`，超出本次范围，未做。
- **D-10 · 「永不到达的 TLA」并不挂起**（实现期实测）：`await new Promise(()=>{})` 会在
  事件循环排空时被 deno_core 立即判为 `Top-level await promise never resolved`（Core
  error，非 Timeout）。真正的挂起只有两类：同步死循环（B5 的 KillSwitch 兜底）与
  悬而未决的 op（如永不响应的 `fetch`）。后者无通用兜底，只能靠 D-6「boot 不做长等待」
  的约定。
- **D-8 · 空文件 / 纯注释 / CJS 写法静默无效**：`looks_cjs`（`module_loader.rs:257`）
  对空串与纯注释都返回 true → 被 `wrap_cjs` 包装 → 零副作用；`module.exports = {...}`
  同理变成 default 导出。装配期对「内容既无 import/export 也无 `globalThis` 赋值」
  的情形打 warn。文档明示 **ESM-only**。

## 改动面（v2 精确清单）

- `src/bridge/runtime.rs`
  - **删掉 `make: Box<dyn Fn() -> JsRuntime>`**（它只是 `stable.clone()` 的重复捕获，
    `RuntimePool` 已持 `stable`），改存 `inspect: bool`。
  - `RuntimePool::new(stable, inspect, kill: Arc<KillSwitch>)` —— 池需持 KillSwitch
    以便在 boot 期 arm（B5）。
  - `checkout()` → `pub async fn checkout(&self) -> Result<JsRuntime, CoreError>`，
    内联 boot（arm → `boot_runtime` → disarm → Err 兜底 event loop）。
  - 新增 `pub async fn boot_runtime(rt: &mut JsRuntime, spec: &str) -> Result<(), CoreError>`：
    与 `run_side_driver`（`mod.rs:561`）共用逻辑，保留 `mod.rs:575-587` 的顺序
    （load → `mod_evaluate` → `run_event_loop` → `eval.await`，0.410 签名陷阱）。
    供 `checkout`、`Bridge::prewarm`、`oj/src/test_cmd.rs` 三处复用。
  - **加注释锁死** `if let Some(rt) = self.idle.borrow_mut().pop() { return rt; }`
    的形状：临时 `RefMut` 必须在 await 前 drop，否则 `checkin()` 二次 `borrow_mut()`
    会 panic。**不要指望 lint**：`src/lib.rs:1` 是 `#![allow(clippy::all)]`，
    CI（`.github/workflows/`）也未跑 clippy。
- `src/bridge/mod.rs`
  - `Extras` 增 `boot: Option<String>`；`StableState` 增 `boot: Option<String>`。
  - `StableState` **无 `Default` 且字段全公开** → 3 处字面量构造点都要改：
    `mod.rs:296`、`oj/src/app.rs:407`、`mod.rs:1161`。
    （不放在 `LoaderShared`：`StableState.loader` 是 `Option`，旧路径 None 会静默跳过 boot。）
  - `checkout_reset` / `checkout_armed` 改 async + `.await`（4 个调用者全在 async fn 内）。
  - 新增 `pub async fn prewarm(&self) -> Result<(), CoreError>`（`Result`，不 panic）。
  - `start_inspector`（`mod.rs:630`）：全仓 `*.rs` **无真实调用者**（`docs/dev-guide.md:146`
    只是示例），改 `pub async fn` 成本≈0；顺手拆成
    `pub async fn inspector(&self) -> JsRuntimeInspector`，避免「async fn 返回 JoinHandle」。
  - `Bridge::with_dbs_and_loader` 内 `KillSwitch::spawn()` 提前到 `RuntimePool::new` 之前，
    与池共享同一 `Arc`。
  - 测试 `mod.rs:1179` 的 `pool.checkout()` 加 `.await`。
- `oj/src/app.rs`
  - 在 `app.rs:96`（loader 构造之后、`make_bridge` 之前）探测并计算
    `versioned_specifier`；文件存在但 stat/canonicalize 失败 → `Err`（fail-fast）；
    不存在 → `None` + `tracing::debug!`。
  - `make_bridge`（`app.rs:241`）与 `StableState`（`app.rs:407`）两处填 `boot`。
  - 在 `RouteTable::build` **之前**插入 `prewarm_boot(&make_bridge)?`（B3）。
- `oj/src/build_cmd.rs`：`Extras::default()` → 填 `boot`（探测
  `src.parent()/ext_boot.js`，同 `build_cmd.rs:215` 的 root）。
- `oj/src/test_cmd.rs`：在 `JsRuntime::new` 之后、`*.test.ts` 加载之前调用
  `boot_runtime`（若 `stable.boot` 为 Some）。
- ~~`src/bridge/module_loader.rs` 放行 `ext:`~~ —— **不做**（B1）。

## 测试（v2 用例清单，在 `src/bridge/mod.rs` 的 `mod tests` 内）

回归基线：

1. 既有测试除 `mod.rs:1179` 加 `.await`、`mod.rs:1161` 加字段外**零改动保持绿**
   （`Extras` 有 `#[derive(Default)]`，`..Default::default()` 对新字段安全）。

boot 基本语义：

2. `boot_sets_global_visible_to_handler`——boot `globalThis.foo = 1`，handler 读到 `foo`。
3. `boot_imports_project_module`——boot import 项目内模块（`./src/_shared/fmt.js` 风格）。
4. `boot_tla_awaits_db_query`——boot 的 TLA 里 `await db.query(...)`（`InMemoryAccessor`）。
5. `boot_tla_fetch_via_httptest`——boot 的 TLA 里 `fetch`（用 `httptest`，
   同 `src/bridge/fetch.rs:143-146` 先例，不碰真网）。
6. `boot_envelope_write_is_reset`——boot 里 `json.ok({boot:1})`，handler 再
   `json.ok({h:1})`，断言响应是 handler 的。

次数与复用（v1 缺）：

7. `boot_runs_once_per_runtime_not_per_request`——用 **`InMemoryKV` 跨 runtime 共享计数**
   （模块级 `let n = 0` 是 per-isolate 的，区分不了「没重跑」和「重跑了但重置」）：
   请求 1 → 1；请求 2（复用空闲池）→ 仍 1；跑一次超时请求丢弃 runtime → 请求 3 → 2。
8. `boot_side_effect_count_matches_runtime_count`——kv 计数 + 多 actor，固化
   「N(api.ts) + N(actor) + N(WS)」这笔账。

失败路径（v1 严重缺失）：

9. `boot_syntax_error_fails_from_config`——**e2e 层**断言 `App::from_config` 返回 **`Err`**，
   不是 warn、不是「服务照常监听」（B3）。
10. `boot_failure_returns_run_error_not_panic`——运行期 boot 失败 → `RunError::Core`，
    Bridge 仍可服务后续请求（B2）。
11. `boot_failure_does_not_segv`——boot 失败后进程存活（B4 回归）。
12. `boot_infinite_loop_yields_timeout`——boot 里 `while (true) {}`
    → `BOOT_TIMEOUT` 触发 `RunError::Timeout`（不是永久挂起，也不是 Core error）。
    （B5 回归；注意 `await new Promise(()=>{})` 不挂起，见 D-10。）
    另：实现期修掉一个由此暴露的真 bug —— `checkout` 里 `fired` 必须**先于** `result`
    判定，否则 boot 超时会被 Core 分支抢先、误报成 500 而非 408。
13. `boot_file_removed_at_runtime_yields_run_error`——运行期删文件 → `RunError`，非 panic。

静默失效与边界（v1 缺）：

14. `empty_or_comment_only_boot_is_noop` / `cjs_boot_has_no_global_effect`（D-8）。
15. `boot_absolute_file_import_bypasses_root_check`——记录行为，防回归。
16. `boot_cannot_import_ext_core_ops`——断言被 deno_core 拒绝（B1 的回归锁，
    防止后人重新尝试放行 `ext:`）。

跨命令一致性：

17. `build_does_run_ext_boot`——`oj build` 侧 boot 生效（S-2）。
18. `test_runtime_sees_boot_globals`——`*.test.ts` 看得到 boot 全局（S-4）。

取消：~~v1 测试 5「boot `import { op_db_has } from "ext:core/ops"` 可用」~~ —— 见 B1。

## 评审裁定记录

三位评审独立出具，以下为对 v1 逐条的最终裁定（含被驳回的原提案）：

| # | 评审意见 | 裁定 |
|---|---|---|
| B1 | 放行 `ext:` 既越权又不可行（deno_core `validate_ext_module_import`） | **采纳**，删逃生口，测试 #5 取消 |
| B2 | 运行期 panic 永久杀死 actor 线程 | **采纳**，拆 prewarm(Err) / checkout(RunError) 两段语义 |
| B3 | dev 内省吞 panic → 路由静默为空 | **采纳**，prewarm 前移至建表之前 |
| B4 | boot 失败 isolate 未轮询即 drop | **采纳**，Err 分支兜底跑一轮 event loop |
| B5 | boot 无超时（且 `tokio::time::timeout` 对死循环无效） | **采纳**，KillSwitch arm + `INTROSPECT_TIMEOUT` |
| S-1 | 执行次数 = N(api.ts)+N(actor)+N(WS)，须幂等 | **采纳**，写入「重要约束」 |
| S-2 | 「`oj build` 零改动」实为「压根不跑」→ 分叉 | **采纳**，build 同样注入 boot |
| S-3 | boot 无 KillSwitch | 并入 B5（保留「`tokio::time::timeout` 无效」这一关键理由） |
| S-4 | `oj test` 绕过池 → 行为分叉 | **采纳**，抽 `boot_runtime` 共用 |
| O-1 | 冻结 `?v=` 未杜绝混杂 | **采纳为 D-6**，选择诚实说明 + 可观测，不做每次 stat |
| O-2 | boot 全局可写 | **采纳为 D-7**（建议，非强制） |
| O-3 | async 化 checkout 连带成本 | **采纳**：`start_inspector` 无真实调用者，成本≈0 |
| — | 把 `boot` 放 `LoaderShared` | **驳回**：`loader: Option` 为 None 时静默跳过 boot |
| — | 每次 checkout 重新 stat spec | **驳回**：一次 stat 换不来心智一致，反而让池内版本可漂移；选 D-6 |
| — | 并发 checkout 的 `RefCell` 跨 await 风险 | **降级**：每条 actor 线程独占 Bridge，current_thread 下无并发推进；加注释锁死即可，不设用例 |
| — | 「CI 有 clippy 门禁」 | **勘误**：`src/lib.rs:1` 为 `#![allow(clippy::all)]`，CI 未跑 clippy；不得依赖 lint 兜底 |

#18. `test_runtime_sees_boot_globals`——`*.test.ts` 看得到 boot 全局（S-4）。
19. `app::tests::ext_boot_spec_absent_vs_present`（`oj/src/app.rs`）——装配期探测：
    无文件 → `None`；有文件 → `file://…ext_boot.js?v=…`。

### 实现结果

- 全量 `cargo test --workspace --release`：**174 + 100 通过，0 失败**（原 165 → 174）。
- 既有测试除 `mod.rs:1179` 加 `.await`、3 处 `StableState` 字面量加 `boot` 字段外零改动。
- `boot_cannot_import_ext_core_ops` 已实测通过：deno_core 拒绝 `ext:core/ops`
  （错误信息含 `ext:`），B1 的阻断结论得到确认——那条逃生口确实走不通。

## 待确认 → 已裁定

- `BOOT_TIMEOUT` **单设常量**（2s，不复用 `INTROSPECT_TIMEOUT`）：两者用途不同，
  耦合会让改内省超时连带改 boot 超时。
- prewarm 只验证一个 Bridge（一个 runtime）；后续 WS/内省路径的 boot 失败由 B2 的
  `RunError` 兜底，不再预热。
