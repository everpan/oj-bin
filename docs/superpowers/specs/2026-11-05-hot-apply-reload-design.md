# 设计定稿：`_system/config/reload` 覆盖 manifests 装配（免重启装/卸解决方案）

> **状态：已实现**（v0.1.122，2026-11-05；D1–D6 + B1–B19 落地，两处如实登记见
> CHANGELOG v0.1.122——cron driver 宿主为 bug 修复、applied 真实性为行为修正）。

> 依据：`oj-module/docs/plans/20261105-upstream-oj-pr.md`（上游 PR 提案）§2.1–§2.3 + §2.4 验证 + §5 验收。
> 评审：架构师 + 工程师双 expert review（2026-11-05）已并入，全部 HIGH/MED findings 落在 §4。
> 范围裁定（脑暴定稿）：**reload config 为核心**；多进程 supervise（§2.6）、US-4 启动降级、
> WS 主动断开、cron/seed 随装**本轮不做**（US-4 作为紧随的第二个提交）。
> 部署形态：release 树（`dist/api/manifests.yaml` 锁）；dev 树免重启增删模块明确不做。

## 1. 意图与现状

oj-module 模板侧已完成安装/卸载/补偿/门禁编排；「装/卸真正生效」依赖 oj 的
`_system/config/reload` 把 `manifests.yaml`（锁）纳入重装范围。现状缺口（已核实）：

- **变化检测短路**：`do_reload`（`oj/src/serve_cmd.rs:455`）幂等判定只算 `config.yaml`
  raw doc sha256。卸载只改锁 → `unchanged=true` → 提前返回 → 路由仍挂载（卸载空转）。
  安装场景因 config 同被改而能走到装配，但 `applied` 报 config 段名而非模块名，
  模板侧 `reloadAndAssert`（`unchanged===false` 且 `applied ⊇ modules`）必然失败。
- **`App::from_config` 本就会重读锁**：release 路由构建（`oj/src/app.rs:2001`）与
  `manifest::discover` release 分支（`oj/src/manifest.rs`，锁驱动）都从磁盘重读
  `manifests.yaml` —— 装配机器已在，缺的只是让 reload 走到它 + 装配产物换代生效。
- **既有 bug（评审发现，本设计顺带修复）**：cron driver 被 `tokio::spawn` 在 reload 的
  临时 multi-thread runtime 上（`oj/src/tasks.rs:324` → `spawn_cron_driver`，runtime 于
  `build_reload_executor` 每次 reload 新建、`do_reload` 返回即 drop）→ **当前每次成功
  reload 都会杀死新一代 cron driver**（首代活在主 axum runtime 无恙）。修复后 CHANGELOG
  如实登记为 bug 修复。

## 2. 目标 / 非目标 / 反目标

**目标**：装/卸落盘后调用一次 `POST {base}/_system/config/reload`：PID 不变、亚秒级生效；
`unchanged` 语义 = config 与全部锁均未变才 `true`；`applied` 报告模块级变化
（`+name@ver` / `-name@ver`）且**只报真实本代生效**；anon（auth 名单）每代重编译；
cron 换代不断摆、不双发。

**非目标**：`oj supervise` 换进程、`/_system/restart` 端点；启动期锁/目录缺失降级（US-4，
第二提交）；WS 主动断开（P1）；seed/cron 随装；文件监听驱动重扫；dev 树热增删模块；
`tenant.anonymous_paths` 语义变更（维持「免租户头但仍验签」现义）。

**反目标**（出现即失败）：特权面（platform/fs）随热更生效或热装同名模块继承特权；
anon 条目声明进保留段（`_platform`/`_system`/`auth`/`_lifecycle`/`tasks`）；
换代期间出现「新 modules + 旧 registry」或「旧路由 × 新 assembly」守卫失守窗口被放大到
可观测；cron 换代窗口双发；`Arc::get_mut` 触 panic；reload 失败留下任何指针错位。

## 3. 约束（红线承接 CLAUDE.md）

- `StableState` 构造后不可变——**不触碰 `Arc::get_mut`**；换代走内部可换槽位
  （`reload_slot` 的「指针冻结、内容后填」惯例的只读版）。
- oj-plugin-ffi `ABI_VERSION` 不动：不改 `AuthGuard` trait / 各轴 vtable 形状；
  anon 短路全部落在 serve 侧。
- `JsRuntime` `!Send`：池与一切持有点留在 current_thread 运行时；reload executor 维持
  「独立线程 + 多线 runtime」既有形态；**cron driver 挪到专用 std thread + 自持
  current_thread runtime**（同 worker 先例 `task_pool.rs:662-671`），生命周期随进程。
- SQL 红线：动态标识符只来自 `SchemaRegistry` → registry 换代必须与 modules 同代原子换。
- `bootstrap.js` 7-bit ASCII 不受影响（本设计不触扩展 JS）。

## 4. 设计

### D1 变化检测纳入 manifests 锁

- `HotReloadCtx` 增 `current_locks: tokio::sync::Mutex<HashMap<PathBuf, BTreeMap<String, String>>>`
  （键 = release api 挂载的 `manifests.yaml` 绝对路径；`serve_hot_reload_branch` 启动期
  从 resolved mounts 的 release 树初读）。
- `do_reload`：归一 cfg 后对每个 release 挂载重读 `manifest::load_lock`；
  变化判定 **per-file**（任一挂载锁文件内容变化即变化——不按合并 map 判，避免跨挂载
  迁移模块时「合并后相同 → unchanged=true 空转」）；**config sha 未变且全部锁未变 →
  `unchanged=true` 返回**。
- 锁读取失败（yaml 解析错）→ 400 点名文件；锁文件 NotFound 视为空锁（`load_lock`
  既有语义）。
- dev 树挂载不参与锁检测（dev 模块集仍以目录为准，本轮不动）。

### D2 `applied` 模块契约

- 锁集合纯 diff：per-file 新旧锁 `BTreeMap` 差集后**跨 release 挂载合并**输出
  （模块名跨挂载唯一由 `build_schema_and_modules` S002 同款拦截兜底）：
  - 新增：`+{module}@{version}`；移除：`-{module}@{version}`。
  - 升级（同名换版）自然同时产出 `-name@old` 与 `+name@new`——模板侧只做子集断言。
  - 跨挂载迁移（A 摘 `m@1`、B 挂 `m@1`）：per-file 判变化 → `unchanged=false`，
    merged diff 为空 → `applied` 模块段为空（config 段名可能有）——契约写进 oj-module 回写。
- 模块条目追加在 config 段名条目之后，同一 `applied: Vec<String>`（`ReloadOutcome`
  五键形状不变，零 wire 破坏）。
- **applied 真实性修正**（评审发现）：`vars` / `oidc` / `db_query` / `db_trace` / `ws`
  五段字段冻结在 StableState、reload 不生效，却落 `diff_configs` 的 `_ => applied`
  兜底臂（`oj/src/serve_cmd.rs:603`）→ 全部改归 `restart_pending`（CHANGELOG 登记
  行为修正）。`tenant` 段随 D3 旗标换代后**真生效**，维持 applied。

### D3 `PerGenAssembly` 单指针换代（暗礁一修复 + 双评审 HIGH 落点）

```rust
// src/bridge/mod.rs
pub struct PerGenAssembly {
    pub registry: Arc<SchemaRegistry>,   // Arc：query.rs:649 下游签名链不动
    pub modules: Arc<HashMap<String, ModuleCtx>>,
    pub sql_guard: SqlGuard,             // tenant 段三旗标随代（F5 修正：段真正可热更）
    pub ownership_deny: bool,
    pub allow_as_tenant: bool,
}
// 槽：Arc<std::sync::RwLock<Arc<PerGenAssembly>>>，中毒按仓库惯例
// unwrap_or_else(|p| p.into_inner())（先例 app.rs:2144）
```

- **槽单实例（关键）**：每个 Bridge 自持一份 StableState（`mod.rs:997-1000`）、
  `make_bridge_of` 闭包影绑首代 `registry/modules`（`app.rs:1611-1612,1663,1672`）——
  槽 Arc 若每 Bridge 自建，换代只落在 Backend.stable 一份上，流量静默不生效。故槽 Arc
  **构造一次**，照 `reload_slot` 同款穿参链：`assemble_backend` 构造 → Extras 新增字段
  （`mod.rs:1064` 同位）→ `with_dbs_and_loader` 接收（不自建；测试构造点传 None 则
  由 registry+modules 包一份新槽，机械改造）→ 闭包影绑同一 Arc。
- **请求代际原子（快照钉 ReqState）**：`checkout_reset`（`mod.rs:1134-1152`）把
  `Arc<PerGenAssembly>` 快照随 `module` 一并写进 `ReqState`；读点改读 `ReqState.assembly`：
  `guard.rs:243/250/401/512`、`db_import.rs:1252/1283`、`query.rs:649`、`mod.rs:1604`
  （共 8 处；`task_pool.rs` 的 registry 是 TaskRegistry，非消费点）+ 旗标执法点
  `guard.rs:260/398` 与 `allow_as_tenant` 消费点。同一请求内多次读持同一快照，
  请求中途换代不混代；在途请求在旧代自然 drain。
- **影子装配 + commit 后无间隔换（时序）**：`App::from_config` 复用路径**只构建**
  新 `PerGenAssembly`（重跑 `build_schema_and_modules`，参数取新代 cfg 的
  `trees / sql_guard / shared_allow / module_index`）作返回产物，**不 swap**；
  `do_reload` 在 `committer.commit(new_inner)` 返回后、`current_doc/sha/locks`
  落盘前**无 await 间隔**执行 `swap_assembly`。失败路径（归一/装配/路由构建/审计/
  任务交接）全部落在 swap 之前 → 指针零移动；成功路径残余窗口 = commit 到 swap 的
  微秒级，方向 fail-closed（新模块 `db.table` 短暂 400 白名单未命中，可见错误）。
- **抢注交叉检查（安全，评审发现）**：复用路径重跑装配时补启动期同款拦截——
  新 installed 名单 ∩ 冻结 `platform.modules`（特权名单）非空 → Err 点名（
  `build_platform_grants` 的 `app.rs:557-565` 语义；grants 本身不重建、仍
  restart_pending）。否则「市场装一个名为 admin 的 bundle + reload」即静默继承特权。
- 依赖语义：deps 守卫、db 绑定重定向在 `ModuleCtx` 内随代重建；卸载后依赖方同代失效。
- 首启路径：`build_schema_and_modules` 产物照旧局部变量 → 包初代槽，单一构造点。

### D4 serve 侧 admit() 匿名预短路（暗礁二修复，路线 C）

- **名单严格 = `auth.anonymous_paths`**（flatten 后）。`tenant.anonymous_paths` 维持现义
  （免租户头但仍过 `guard.verify`）——两者现状语义不同，并集会把 tenant-only 条目从
  401 面静默升格为全匿名（存量配置行为改变，违反自设红线）。
- `Pipeline` 增 `auth_anon: Vec<String>`（装配期编译，每代 `from_config` 重建 → 免重启生效）。
- 实现位置收敛在 **`admit()`**（`serve/src/dispatch.rs:234`，run_route / blob PUT / MCP /
  tasks 四调用方的唯一咽喉）：`path_matches(&pipeline.auth_anon, path)` 命中 → 跳过
  `guard.verify`，`http.user = None`；**保留 hdr_map 构建段**（匿名请求 handler 侧
  headers 不得消失）。matcher 复用既有 `serve::path_matches`（字面 / `*` 单段 /
  `**` 跨段，v0.1.20 语义）；`verify` 无副作用（会话在登录端点签发），短路不涉 cookie 回归。
- **覆盖边界明文登记**：tasks API（`serve/src/tasks.rs:90` 自带 verify）、WS
  （`ws.rs:117`）、reload 端点（`reload.rs:260`）各自独立验签通道，不走 admit() →
  运行期新增 anon 对这三类**不生效，属重启级**；故保留段黑名单扩为
  `_platform` / `_system` / `auth` / `_lifecycle` / **`tasks`**。
- **保留段黑名单**：`config::validate_anon_paths` 扩展——新条目落在黑名单前缀 →
  装配 Err（fail-fast，与模板 preflight 双保险）。
- 删除语义沿提案 §2.2：运行期新增条目删除即失效；启动期已在 config 的条目删除后
  oj-auth 插件内静态表仍含（进程级 `OnceLock` 冻结），需重启才彻底撤销——暴露面受
  「匿名只对已注册路由有效 + 卸载后路由已无」约束。
- `AuthGuard` trait 不动；oj-auth 插件表照旧（双实现互指注释同步更新）。

### D5 cron 换代：宿主修复 + armed-but-paused 闸口（US-3）

- **前置——driver 宿主修复**（既有 bug，见 §1）：`spawn_cron_driver` 改专用 std thread +
  自持 current_thread runtime 承载 driver 循环（worker 同款 `task_pool.rs:662-671`），
  生命周期随进程；CHANGELOG 登记为 bug 修复。
- **闸口**：`TaskPool::new` / `assemble_tasking` 增 `resume: Arc<AtomicBool>`（同一 Arc
  供调用方持有）；**插入点在 cron driver 循环顶 flag 检查之后**（触发判定不在 worker，
  在 `spawn_cron_driver` 的 driver 循环 `tasks.rs:346-403`）：未放闸 →
  `tokio::select!{ wait_resume, wait_flag }` 阻塞等待（不空转）；放闸后正常「睡到 next_run」。
  **`TaskSupervisor` 子进程拉起同样持同一门**（hold 到放闸）——TLA 任务探测秒级，
  「assemble 即 armed」的双实例重叠窗口一并关闭。
- **闸门初值语义**：启动路径（`serve_cmd.rs:204`）放 open gate；reload 路径建池即
  closed gate，旧池 `shutdown_and_join` 完成后放闸。新池装配失败 → 旧池从未被停
  （回滚语义不变）；双发窗口结构性归零。
- **停机有界化**：reload 路径旧池 `shutdown_and_join` 加超时（超时 → 500 点名
  「旧任务池未收场，任务调度暂停至下次成功 reload」——旧池 flag 已 latch 无法复活，
  这是诚实上限）；交接失败路径对新池 best-effort `shutdown_and_join` 兜底后再返 Err
  （防新池 std 线程泄漏 + SIGTERM 停错代）。

### D6 capabilities + `_system/config` GET 摘要增强

- `assemble_ojinfo`（`oj/src/serve_cmd.rs:1983`）增
  `"capabilities": ["reload.assemble_modules", "reload.anon_per_reload", "reload.cron_survives"]`。
  静态表冻结在首代 `StableState.oj_info`（复用路径不重建——静态声明，成立）。
- GET `_system/config` 摘要增 `"modules": ["name@ver", ...]`（当前代全 release 挂载锁并集
  快照；**`ReloadMeta` 增字段**，启动期初读、commit 时更新——不进 ojInfo，后者不随代刷新）。
  响应加键属向后兼容增量；POST/响应 wire 形状不变。

### D7 决策记录（本设计的裁量）

| # | 决策 | 理由 |
|---|---|---|
| 1 | 换代槽用 `std RwLock<Arc<PerGenAssembly>>`，不引 arc-swap | 读路径 clone 即出锁、写侧单指针赋值；零新依赖 |
| 2 | 锁检测 per-file + diff 输出用 merged map | 跨挂载迁移不空转；模板断言拿得到加/删条目 |
| 3 | 复用路径无条件重跑 `build_schema_and_modules` | 幂等便宜；单一同源路径（仓库先例 input_contracts `app.rs:2142`） |
| 4 | US-4 启动降级第二提交 | reload 失败干净 400 + 模板锁写回回滚编排已兜住卸载链路 |
| 5 | dev 树不参与锁检测 | 装/卸只发生在 release 树；dev 语义维持「增目录需重启」 |
| 6 | **reload 路径放行空锁**（空路由合法）；启动期维持空锁 fail-fast | 卸载最后一块板必须可卸；启动空锁多半是构建事故，报错文案保留 |
| 7 | tenant 三旗标（sql_guard/ownership_deny/allow_as_tenant）入 PerGenAssembly 随代 | tenant 段真正可热更，构建/执法同代，消除分裂；diff_configs 维持 applied 不谎报 |
| 8 | TLA 子进程拉起持同一 resume 门 | 关闭双实例重叠窗口；与 cron 同一门零额外机制 |
| 9 | swap 时点 = commit 返回后无 await 间隔 | 失败路径指针零移动；残余窗口 fail-closed（新模块短暂 400） |

## 5. SOLID 落点

- **S**：`PerGenAssembly` 只承担「模块集 → 白名单+上下文+行为旗标」单一职责；锁检测/
  diff 留在 `do_reload`（编排层），不渗进装配函数；影子装配与激活分离（from_config
  构建、do_reload 激活）。
- **O**：每代装配经槽位替换，新增换代产物扩 `PerGenAssembly` 字段即可，读侧签名不变。
- **L/I**：`AuthGuard` trait 不动（接口隔离）；anon 短路是调用方侧前置逻辑，不要求
  实现方感知。
- **D**：serve 依赖 `AuthGuard` trait 与自有 `path_matches`，不依赖 oj-auth 插件；bridge
  只暴露 `ReqState.assembly` 快照，不知晓 oj 装配层。

## 6. BDD 验收用例（Given / When / Then）

| # | Given | When | Then | 落点 |
|---|---|---|---|---|
| B1 | release 服务运行中，锁含 `a@1.0.0` | 目录就位、锁加 `b@1.0.0`，POST reload | `unchanged=false`；`applied` 含 `+b@1.0.0`；`GET b` 路由 200；`http.module.version=1.0.0` | e2e |
| B2 | B1 后状态 | 锁摘 `b`，POST reload | `applied` 含 `-b@1.0.0`；`GET b` 404；b 的表白名单同步消失（`db.table` 越权 400） | e2e |
| B3 | 仅 config.yaml 变 | POST reload | 现行为不变（config 段名 diff） | 既有回归 |
| B4 | 仅锁变（卸载） | POST reload | `unchanged=false`（不再空转） | 单测 + e2e |
| B5 | config 与锁均未变 | POST reload | `unchanged=true`，generation 不动 | 既有回归 |
| B6 | 升级：锁 `a@1.0.0`→`a@1.0.1` | POST reload | `applied` 同时含 `-a@1.0.0`、`+a@1.0.1` | 单测 |
| B7 | 新装模块声明了别的模块的表（S002） | POST reload | 400 点名，旧代路由+旧白名单仍服务（影子装配失败零指针移动） | e2e |
| B8 | 锁指向缺失版本目录 | POST reload | 400 点名（US-4 前语义），旧代仍在 | 单测 |
| B9 | config `auth.anonymous_paths` 新增路径（非保留段） | POST reload 后匿名请求 | 200 免验签，`http.user` 为空；`tenant.anonymous_paths` 命中行为与现状逐字节一致（仍验签） | e2e |
| B10 | anon 条目声明 `_system/**` 或 `tasks/**` | 启动/reload 装配 | 装配 Err（保留段黑名单） | 单测 |
| B11 | reload 前注册 cron 任务 | **首次** reload 后到原触发点 | 任务仍触发**且恰一次**（宿主修复 + 闸口；单测经 `TaskRegistry::set_next_run` 注入 now+2s 短拍验证首拍；e2e 用 `* * * * *` 对齐分钟边界 + deadline 轮询） | 单测 + e2e |
| B12 | 换代进行中在途请求 | 同请求多次 registry/modules 读 | 请求全程持单一快照（ReqState 钉快照；卸载模块的在途请求旧代语义 drain，无混代、无 `bound_db` 改道） | 并发单测（B1 状态翻转时持续打 `db.table` 断言无越权错） |
| B13 | `ojInfo()` | 任意特权 handler | `capabilities` 含三键 | 单测 |
| B14 | GET `_system/config` | — | 摘要含 `modules` 数组且与锁一致 | 单测 |
| B15 | reload 期间并发第二次 POST | 撞 `reload_lock` | 409 busy（既有语义不回归） | 既有回归 |
| B16 | config 声明特权名单 `platform.modules=[admin]` + 受控根 | 市场安装树放名为 `admin` 的 bundle、改锁、POST reload | reload 400 点名抢注冲突；特权不继承（`exec.run`/`fs` 特权路径仍 deny） | e2e |
| B17 | 卸载挂载的最后一个模块（锁→空） | POST reload | `unchanged=false`、空路由合法（reload 路径放行空锁）；同场景冷启动仍 fail-fast | e2e + 单测 |
| B18 | 模块从挂载 A 迁到挂载 B（同版本） | POST reload | `unchanged=false`（per-file 判定）、applied 模块段为空、路由在 A 消失 B 出现 | 单测 |
| B19 | config `tenant.sql_guard` off→deny | POST reload | 构建按 deny 校验、运行期执法同代为 deny（旗标随代）；`vars`/`oidc`/`db_query`/`db_trace`/`ws` 段变更归 `restart_pending` 不谎报 | 单测 |

## 7. 交付物

1. 本仓实现：D1–D6 + B1–B19 用例 + CHANGELOG（含两处如实登记：cron driver 宿主为
   **bug 修复**、applied 真实性为**行为修正**）+ devkit 四件同步（用户可见变更：
   `applied` 模块条目与真实性口径、GET `modules` 键、capabilities、anon 保留段新报错、
   空锁卸载放行语义）。
2. oj-module 提案文档回写（人工提交）：§2.1 补「PerGenAssembly 单指针换代/同源重建/
   影子装配 + commit 后激活」；§2.2 补「名单严格 auth.anonymous_paths、tasks 入保留段、
   WS/tasks/reload 端点为重启级例外」；§5 验收 7 补 cron 双发红线与宿主修复；applied
   契约补 `+/-name@ver` 格式、升级形态与跨挂载迁移语义。
