# config 热更新 — 实现计划

- 关联设计：`docs/superpowers/specs/2026-10-12-config-hot-reload-design.md`（v1，第二轮复核后）
- 日期：2026-10-12
- 落点 crate：`serve`（传输/外层 fallback/watch 通道/reload_api 门禁）、`oj`（重载执行闭包/App 重建/任务池交接/段 diff）

## 架构落点（最小侵入）

- `App::serve_graceful` 现有路径（非 reload 场景）**完全不动**：当 `server.reload_api=false` 时仍走 `serve::serve_router(listener, router, shutdown)`。
- 新增 `App::serve_hot_reload(...)`（仅 `reload_api=true` 时由 `serve_cmd::run` 调用）：构造 `ReloadState` → 注入 `oj` 侧提供的 `reload_executor` 闭包 → 调 `serve::serve_swappable(listener, reload_state, shutdown)`。
- 职责切分：
  - **serve 侧**：`ReloadState`（`watch::Sender<Arc<Router>>` + `watch::Receiver` + 已知 mount prefix 集合 + `auth_guard` + `reload_api` 开关 + `Mutex`（代际锁 + generation + 归一 cfg 快照 + config 路径 + secret 解析器））、外层单一 fallback（分流 `_system`/health + 其余 `oneshot` 给当前内层）、`serve_swappable`（用 `into_make_service_with_connect_info(SocketAddr)` 起 `axum::serve`）。
  - **oj 侧**：`reload_executor` 闭包（捕获 config_path / ServeArgs / config_dir / ReloadState 句柄 / 当前任务池 holder），执行 §5 全流程：归一 → 影子装配（blocking 线程 + 独立 runtime）→ 任务池交接 → `watch::Sender::send(新 Router)` → 更新 generation/cfg 快照 → 返回三清单。

## 任务拆分

### T1 — `server.reload_api` 配置字段（§7.0）
- `src/config.rs:109` `ServerCfg` 增 `pub reload_api: bool`，`#[serde(default)]`。
- `src/config.rs:198` `Default` 置 `false`。
- 验收：`cargo check` 通过；`oj serve` 未配该字段行为不变。

### T2 — `serve_cmd` CLI 覆盖可重放（§7.1）
- `oj/src/serve_cmd.rs:71-76` `a.cert_path`/`a.key_path` move → `.clone()`，保留 `a` 供重载闭包重放。
- 验收：编译；`a` 在 `run` 末尾仍可被 reload 闭包捕获。

### T3 — serve 侧 `ReloadState` + 外层 fallback + `serve_swappable`（§3/§4/§7.3）
- 新增 `serve/src/reload.rs`：
  - `pub struct ReloadState { inner_tx: watch::Sender<Arc<Router>>, inner_rx: watch::Receiver<Arc<Router>>, prefixes: Vec<String>, auth_guard: Option<Arc<dyn AuthGuard>>, enabled: bool, lock: Mutex<ReloadMeta> }`（`ReloadMeta { generation, cfg_sha256, loaded_at, config_path, ... }`）。
  - `pub enum ReloadOutcome { Applied, Unchanged, Rejected(String) }` + 三清单结构。
  - `pub type ReloadExecutor = Arc<dyn Fn() -> Result<ReloadReport, String> + Send + Sync>`。
  - 外层 fallback handler：`fn system_fallback(State(rs), req) -> Response`：
    - 取 path，匹配某 prefix 的 `{p}/_system/health` → 200；`{p}/health` → 200 + warn。
    - `{p}/_system/config` GET → 摘要（generation/cfg_sha256/loaded_at）；POST → 守卫 → 调 `reload_executor()` → 报告。
    - 其余 → `rs.inner_rx.borrow().clone().oneshot(req).await`。
  - 守卫：auth 配置 → `auth_guard` 验 Bearer；未配 → `ConnectInfo<SocketAddr>` 非回环 403。
  - `pub async fn serve_swappable(listener, rs, shutdown)`：用 `Router::new().fallback(system_fallback).with_state(rs)` + `into_make_service_with_connect_info(SocketAddr)` 跑 `axum::serve`。
- `App::serve_hot_reload`：`oj/src/app.rs` 新增，构造 `ReloadState`（初始 `inner_tx` 持 `self.router.clone()`），注入 `reload_executor`，调 `serve_swappable`。
- 验收：`cargo check`；`reload_api=false` 仍走旧 `serve_graceful`。

### T4 — oj 侧 `reload_executor` 闭包（§5 全流程）
- `oj/src/serve_cmd.rs` 构造闭包（T3 注入）：
  - ① 归一：`load_cfg` → `fold_cli_mounts` 重放 → cert/key 重放 → `admission_gate`；归一 cfg 的 sha256 与当前代相同 → `Unchanged`；静态校验失败 → `Rejected`。
  - ② 影子装配：独立线程 + `current_thread` runtime `block_on` `App::from_config(…, reuse_registries=Some)`（复用旧 StableState 后端连接，不重建 pool）；迁移 verify-only；失败 → `Rejected`。
  - ③ 任务池交接：`tasks_flag.store(true)` → `spawn_blocking` 旧池 `shutdown_and_join` → 装配新池；失败 → 重装旧池 + `Rejected`（500 语义由外层转）。
  - ④ `inner_tx.send(新 App.router)`；更新 `ReloadMeta`（generation+1、新 sha256、loaded_at）；返回三清单。
- `App::from_config` 增可选 `reuse_registries` 参数（§7.2）：`Some` 跳过 `assemble_plugins`、复用 `PluginInfo`、复用旧 `StableState` 后端连接。
- 验收：`cargo check`。

### T5 — 段 diff 三清单（§6）
- `oj/src/serve_cmd.rs` 新增 `diff_configs(old, new) -> (applied, restart_pending, kept_listener)`，基于 `known_top_level_keys()` 逐顶层段 `serde_json::Value` 比较；归属表按 §6 矩阵。
- 单测：分类、幂等（归一 sha256）、保留路径冲突 warn。

### T6 — 证书 watcher 进程级单例（§7.5）
- `serve/src/certificate_watcher.rs`：新增 `pub fn ensure_watcher(...)` 按 resolved 路径 `OnceLock`/懒去重，不重复 `spawn_watcher`；`app.rs:789` 改为调 `ensure_watcher`。
- 验收：`cargo check`；反复 reload 不再累积线程（单测/观察）。

### T7 — e2e 验收（§10）
- `oj/tests/e2e.rs` 增真实进程 + 真 TCP 用例：核心链路 / 验证失败保旧 / 幂等 / restart_pending+kept_listener / 回滚 / 真 TCP WS 升级 / 守卫 / health 迁移 / 并发 409。
- 验收：用例通过（受构建时长限制，至少跑通核心链路 + 单测）。

## 风险与顺序
- 先 T1/T2（机械、低风险）→ T3（serve 侧，可独立 check）→ T4/T5（oj 侧核心）→ T6（单例）→ T7（e2e）。
- 现有 `serve`/`dispatch`/`App::serve_graceful` 路径全程不动，`reload_api=false` 时零行为变化。
- 门禁：`cargo xtask build` 归置 `bin/devkit/` + devkit 契约用例校验（§11）。
