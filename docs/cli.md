# Rust 版 CLI 实现预案（可行性分析 + 扩充版）

> **已归档**：这是早期「进程内 devserver」时代的实施预案（P0–P6 已完成），文中所述
> devserver / router / actor 架构已被 **`oj` CLI**（`oj/` crate：server / build / test /
> migrate / fixture / schema）取代，仅作历史复盘保留。现行 CLI 用法见 [cli2.md](cli2.md)
> 与 `oj/src/args.rs`。

> **状态（2026-08-21）：P0–P6 全部完成**，双绿 root 19 + server 22（debug/release）。
> 本文档保留计划原文与证伪更正的脉络（原稿 → 复核结论 → 实现记录），供复盘。

## 0. 文档性质与用法

本文件是 **working plan（边做边更新的活文档）**，不是 final design。每完成一个 phase 后，
在该 phase 末尾追加一行「实现后记录」（含 commit hash / 关键取舍 / 遇到的问题），方便后续复盘。
建议一个 phase 拆成一个 git commit（PR 形式），PR 标题写清本 phase 做了什么。

---

## 1. 现状评估（可行性基线）

### 1.1 已验证可用（release）

Rust 端为 **workspace 双 crate**（P2 起）：根 `only-js`（`src/lib.rs` 暴露 `bridge` 与
`config` 模块，`src/main.rs` 为默认 bin）+ `server`（`mdm-server`：axum HTTP/WS 层，
含 `bin/devserver.rs` 可执行入口）。

- `cargo build` 通过；**2026-08-21（P0–P6 完成时）：`cargo test --workspace`（debug）与
  `--release` 双绿，root 19 passed + server 22 passed, 0 failed**。
- 核心链路实现如下：
  - `runtime.rs` — RuntimePool：复用 JsRuntime，bootstrap 只编译一次，后续请求仅执行 handler 源码；
    `KillSwitch` 看门狗（P4）：跨线程 `terminate_execution` 超时熔断，超时 runtime 不归还池。
  - `query.rs` + `registry.rs` — sea-query 动态构建 SELECT，标识符走 SchemaRegistry 白名单，值参数化（SQL 注入根治点）。
  - `db.rs` / `kv.rs` — DataAccessor / KVStore 统一契约 + 内存 fake（Liskov 可替换为 sqlx/redis）。
  - `accessor_sqlx.rs` — sqlx Any 实现（P1）：sqlite 单连接池，`install_default_drivers` 内聚于 connect。
  - `fetch.rs` — reqwest 实现 fetch，带本地 HTTP 全链路测试。
  - `inspector.rs` — 基于 deno_core 内置 inspector 的 DevTools WS 桥。
  - `loader.rs` — HandlerStore：FS 热重载（notify）+ 嵌入 map。
  - `http.rs` — 请求上下文 RequestInfo（method/params/query/headers/body，server 层填充）。
  - `log.rs` / `json.rs` / `envelope.rs` — 日志绑定、fast JSON op、单遍序列化信封。
  - `ws.rs`（bridge）— `op_ws_send`/`op_ws_close` + bootstrap `ws` 全局（P5）。
  - 根 `src/config.rs` — 配置三层叠加（P3）：`Default()` ← `cfg.yml` ← `cfg.<env>.yml` 深合并。
  - `server/src/router.rs` — `Resolve` 移植（P2）；`actor.rs` — JS actor 线程 + channel 桥（P2）；
    `lib.rs` — axum fallback 装配（P2）+ 408 信封（P4）；`devserver.rs` — CLI 装配/播种/监听（P4）；
    `ws.rs` — echo 路由 + JS 帧循环 `js_route`（P5）。

### 1.2 已知缺陷 —— 已清零（P0 完成于 commit `46a8eb8`）

根因曾是 deno_core 0.409 对扩展源码的 7-bit ASCII 校验（debug 严格、release 放行），
`bootstrap.js` 中文注释触发 debug 全挂。已改为 7-bit ASCII 并修复 debug 测试。
**2026-08-21 复验：debug / release 双绿，各 7 passed。当前无已知缺陷。**

### 1.3 移植对照（2026-08-21 P0–P6 完成后更新；原「未移植清单」反转为现状表）

| 参考实现 | Rust 现状 | 说明 |
|---|---|---|
| `redis.go` `Redis(name)` | **未移植**（唯一残留） | string/hash/list/set/zset；pubsub **仅 publish、无 subscribe**；名字未注册返回 undefined |
| `xorm.go` `XORM(name)` | **未移植**（唯一残留） | 链式 `table/from/where/limit/orderBy`；终结 `find/get/count/insert/update/delete`；raw `query/exec`；engine 级 `ping/stats/isTableExist/dbMetas/tableInfo`；裸 `xorm` = default；实例按名缓存 |
| `ws.go` `ws.*` WebSocket | ✅ P5 `a99a976` | 仅 `send(data)`/`close()` 两个 op（对齐）；Reader/Writer task + Processor 内联，msgChan/respChan 各 cap 64；**每连接独占 VM**（专用线程 + 专用 Bridge，不占 HTTP actor 池）；缺 handler 发 Close 帧退出 |
| server 层超时熔断 | ✅ P4 `8f27a32` | `KillSwitch` 看门狗 → 跨线程 `v8::Isolate::terminate_execution`（v8-150.4.0 `isolate.rs:993`）→ **408** 信封；超时 runtime 丢弃不回池。unsafe 集中 runtime.rs 单点 |
| config 层 | ✅ P3 `1ba0d72` | `src/config.rs`：三层文件叠加（Value 深合并）；`--config/--env/--generate-config` + APP_ENV 回落 |
| router 层 | ✅ P2 `61d7c2b` | `server/src/router.rs`：最少 4 段、首段 `-` 切分、`ToUpper(method)+".js"`、多余段进 rest；6 测全移植 |
| `internal/hot`（HMR/ProgramCache） | ✅ 等价物已齐 | FS 热重载由 per-request 读盘天然覆盖（dev 模式）；ProgramCache ≈ V8 代码缓存——**P6 证伪不可达**（`JsRealm` 为 `pub(crate)`，见 §6） |
| demo DB 种子（default 实例） | ✅ P4 `8f27a32` | devserver 仅对 default 播种 user_profile（neo/trinity/morpheus），可重复运行 |



## 2. 分阶段可行性分析

> 风险等级：🟢 低 / 🟡 中 / 🔴 高。前置依赖指本阶段开始前必须完成的阶段。

| Phase | 内容 | 可行性 | 风险 | 前置 | 关键工作量 |
|---|---|---|---|---|---|
| 0 | ASCII 校验 / build 绿 | 高 | 🟢 | — | trivial |
| 1 | sqlx 接入 | 高 | ✅ done `7867852` | P0 | 小（实际改动比预估更小：`install_default_drivers` 进 connect + sqlite 方言 + unsigned 修复） |
| 2 | Server 层（axum） | 高 | ✅ done `61d7c2b` | P1 | 中（实际三模块 router/actor/axum，actor 模式一次成型） |
| 3 | config 层 | 高 | ✅ done `1ba0d72` | — | 小 |
| 4 | Server 生命周期 | 高 | ✅ done `8f27a32` | P3+P4-glue | 中（watchdog 408 + devserver 装配 + 冒烟全通） |
| 5 | WebSocket 帧循环 | 中 | ✅ done `a99a976` | P2 | 大（5a echo 链路验证 + 5b 三任务流水线，一次通过） |
| 6 | 性能优化 | 高 | ✅ done（2026-08-21 核实：无需改动，见 §6） | — | 0（代码缓存 API 对外不可达；fetch signal 亦无） |

### 2.1 Phase 0 — ASCII 校验 / build 绿（🟢 trivial）

**目标：** debug + release 测试全绿。
**改动：** 删 `bootstrap.js` 中文注释；跑 `cargo test` 与 `cargo test --release` 确认双绿。
**验证：** 两配置均 7 passed。

### 2.2 Phase 1 — sqlx 接入（🟢 小；原判 🟡 基于错误前提）

**目标：** 把 `SqlxAccessor`（`src/bridge/accessor_sqlx.rs`，**已实现**）接入 `Bridge`，替换内存 fake。

**核实更正（2026-08-21，对照锁定版本 sqlx 0.8.6 源码）：**
- ❌ 原稿「连接前须调 `sqlx::runtime::set_handler(&Handler::tokio())`」**证伪**——0.8.6 无
  `runtime` 模块/`set_handler`/`Handler`（grep 全源码无命中）。0.8 的 runtime 选择纯靠编译期
  feature（`runtime-tokio` 已开于根 Cargo.toml），**无需任何注册调用，原第 1 步删除**。
- ✅ 真实前置约束是 **`sqlx::any::install_default_drivers()`**（`any/mod.rs:35`）：
  AnyPool 首次连接前调用一次，激活已编译进来的 sqlite/mysql/postgres 驱动。
- `Bridge::set_db_accessors`（`mod.rs:170`）经 `Arc::get_mut` 注入，**须在首次 checkout 之前**（现注释已写明）。
  **[P4 更正]** 此 API 已删除（`Arc::get_mut` 在池化下永返回 None，必 panic），改为 `with_dbs` 构造期注入。
- 无 `default` 键时取 map 第一个实例作 default；**只注册 sqlite 驱动**
  （sqlite）——「扩 pg/mysql」超出对齐范围，sqlx Any 特性保留即可、不投入。

**改动（可立即执行）：**
1. main/tests 入口调 `sqlx::any::install_default_drivers()`。
2. `SqlxAccessor::arc("sqlite://...")` 构造 `Arc<dyn DataAccessor>`，`set_db_accessors` 注入。
3. 补 sqlite 落库集成测试（`#[tokio::test]`）：建表 → `db.exec` 插入 → `db.table(...)` 读回断言 +
   绑定参数 + 未知表拒绝。

### 2.3 Phase 2 — Server 层（axum，🟡 中，新 crate）

**目标：** 新建 `server/` crate，实现 `GET/POST/.../*.js` 路由解析 + 统一信封输出。

**关键事实与风险（2026-08-21 修正）：**
- **新 crate = 独立 `server/Cargo.toml`**，依赖 `only-js`（workspace member）。axum 0.8 可被
  edition 2024 crate 依赖（edition 按 crate 各自生效）；开工第一步 `cargo add axum` 冒烟定版。
- ⚠️ **原稿「HTTP 路径无 !Send 风险」证伪**：`Bridge` 持 `RefCell` 池（→ !Sync），且 `run_with` 的
  future 跨 await 持有 `&mut JsRuntime`（!Send）→ **axum handler future 必须 Send，直接 `.await` 编译不过**。
- ✅ **正确架构（同时是 P5 的地基）——JS actor 线程**：N 个专用 OS 线程，各跑一个
  `current_thread` tokio runtime 并持有 `Bridge`；axum handler 只做
  `mpsc::send((RequestInfo, oneshot::Sender))` → `await oneshot` 收 `Capture`——future 只含
  channel，天然 Send。线程数 = `ServerConfig.PoolSize` 的等价物。
- router 直接移植 `Resolve`（已核实：最少 4 段、`{mode}-{version}/{sub}/{feature}/{entity}`、
  文件名 `ToUpper(method)+".js"`、多余段进 Rest）。server 是 fiber v3 catch-all `All("/*")`，
  axum 用 `fallback` 路由等价。

**改动：**
1. `server/` crate：`router.rs`（Resolve 移植）+ `actor.rs`（JS 线程 + mpsc/oneshot 桥）+ `lib.rs`（Server/handle）。
2. `handle`：resolve → 经 actor 执行 `Bridge::run_named` → 回写 `capture.status`/`headers`/`body`。
3. 信封语义对齐：404（无 handler）、compile-error；408 超时信封（P4 watchdog 已接入）。

### 2.4 Phase 3 — config 层（🟢 小）

**目标：** 移植 config 层（结构已核实）：
- `Config{Server, DB: map<name, DBConfig{DSN}>, Redis: map<name, RedisConfig{Addr,Password,DB}>}`；
  `ServerConfig{Addr, BaseDir, Timeout, PoolSize, HMR}`。
- ⚠️ **「env 覆盖」语义修正（核实）**：不是 OS 环境变量，而是 **`cfg.<env>.yml` 文件叠加**
  （`Default()` ← `cfg.yml` ← `cfg.<env>.yml`；env 取自 `--env` 参数或 `APP_ENV`）。
- 默认/回落：`BaseDir` 默认且空值回落 `"routes"`；HMR Root 回落 BaseDir；缺默认 cfg.yml 静默用默认值，
  显式 `--config` 指向缺失文件则报错。
**技术选型：** `serde_yaml` 0.9（deprecated 但功能可用）；`Cargo.toml` 注释标注。
**风险：** 低。`config_test` 共 25 例，移植主干即可（见 §5）。

### 2.5 Phase 4 — Server 生命周期（🟡 中）

**目标：** 装配 config → sqlx pool → Bridge → server。对齐 `buildServer`（已核实流程）：
parseArgs（`--config/--env/--generate-config`）→ `config.Load` → 逐 `db.<name>` 开库
【sqlite `SetMaxOpenConns(1)`】→ 仅 default 播种 demo 数据 → HMR（可选）→
`NewSessionPool(PoolSize)` → 逐名 redis client → `server.New`。

**改动：**
1. main 读 config → 逐 `db.<name>` 开 sqlx `Pool<Any>`（sqlite 建 pool 时 `max_connections(1)`
   对齐写锁语义）→ `SqlxAccessor::from_pool` → `set_db_accessors`（无 default 键时取第一个，
   对齐回落）。
2. **超时熔断**：每请求起 watchdog（`cfg.Server.Timeout`），到期经 actor 存的 isolate 裸指针调
   `v8::Isolate::terminate_execution()`（V8 允许跨线程），该 runtime **不归还池**，回 408 信封
   对齐。unsafe 集中在 actor.rs 单点、注明 SAFETY。
3. JS actor 线程数 = `cfg.Server.PoolSize`。

**风险：** watchdog 的 unsafe isolate 指针是唯一硬点；先不接也不阻塞联调（死循环 handler 占死一个
actor，新请求会新建 runtime，劣化但不死锁）——**上线前必须有**。

> **[实现更正，P4 `8f27a32`]** ① `set_db_accessors` 改为 `Bridge::with_dbs` 构造期全量注入
> （原 API 必 panic：`RuntimePool::new` 已 clone stable，refcount≥2，`Arc::get_mut` 永 None——
> §2.2 中「须在首次 checkout 之前」的描述随之作废）。② unsafe 实际集中在
> `src/bridge/runtime.rs` 的 `KillSwitch`（非 actor.rs），SAFETY 注释在位。③ 风险项已消除：
> E2E 408 测试 + 冒烟（236ms 熔断、server 存活）均通过。

### 2.6 Phase 5 — WebSocket 帧循环（🔴 高，全案最大风险）

**目标：** 移植 `ws.go`（已核实）：`ws.*` 仅 `send(data)`（5s 写超时）/`close()` 两个 op；
三协程 reader/processor/writer，msgChan/respChan 各 **cap 64**；**每连接独占一个 VM**（不进
HTTP SessionPool），handler 预编译、逐帧在同一 VM 复跑；每帧默认 10s 超时熔断。

**为什么是最大风险 —— 核心矛盾：**
- deno_core 的 `JsRuntime` 是 **`!Send`**（不可跨线程）且**绑定 current_thread tokio**（见 `runtime.rs` 注释）。
- axum 的 WebSocket 默认在 **hyper 多线程 runtime** 上跑 handler，帧可能在不同线程被拾取 → **V8 isolate 跨线程 = 直接崩**。
- 必须把整条帧循环钉在单个 current_thread 句柄内（V8 isolate 线程亲和）：`tokio::Runtime::block_in_place` 或 专用 `Builder::new_current_thread().build()` + 该 handle 上 `spawn`。

**推荐架构（复用 P2 actor）：**
- **复用 P2 的 JS actor 线程**：每 WS 连接在某个 actor 线程上 checkout 一个 runtime 绑定连接
  生命周期（每连接独立 VM），帧循环全部钉在该 actor 的 current_thread handle 上。
- actor 线程内 3 个 task：Reader（axum socket 读帧 → msgChan，cap 64）/ Processor（逐帧
  `run_to_completion` 复跑 handler → respChan，cap 64）/ Writer（respChan → socket 写回）。
- axum 侧 handler 只搬字节与 channel（future 天然 Send）。
- ⚠️ **不要**让 axum 的 WS handler 直接 `await` 跨帧持有 JsRuntime 的 future——一律走 actor 通道。

**建议分两步降低风险：**
1. **5a（MVP）：** 先做一个最小 WS echo（不执行 JS handler，仅透传帧），先打通 axum+upgrade+single-thread pin 这条最危险的链路。
2. **5b：** 再叠加 JS handler 帧循环。
若 5a 的 pin 模式验证通过，5b 只是逻辑填充。

> **[实现记录，P5 `a99a976`]** 按此分两步执行且一次通过；架构微调：未复用 HTTP actor 线程池，
> 而是每连接专用 OS 线程 + 专用 Bridge（每连接独立 VM，不占 HTTP 池）；Processor
> 内联在连接线程（非独立 task），Reader/Writer 两 task 经 mpsc（cap 64）解耦。

### 2.7 Phase 6 — 性能优化（🟢 小，多数已实现）

**目标：** 扩 fetch 能力（signal/AbortController）、代码缓存。
**关键事实：** 池化复用（`runtime.rs`）、单遍信封序列化（`envelope.rs`）、`serde_v8` 规避（`json.rs` fast op）
**均已实现**。fetch 已可用（`fetch.rs`），仅缺 signal/AbortController。
**改动：** fetch 加 optional `signal`；`run_to_completion` 的 `execute_script` 换
`execute_script_with_cache`（**已核实存在**：deno_core 0.409 `runtime/jsrealm.rs:493`，经 realm 调用；
当前 runtime.rs 只用了无缓存版 `jsruntime.rs:2020`）。

> **[实现更正，P6 零改动收尾]** 「经 realm 调用」这一前提**证伪**：`JsRealm` 与
> `JsRuntime::main_realm()` 均为 `pub(crate)`，embedder 拿不到 realm 实例，该方法实际不可调用；
> 唯一公开钩子 `set_eval_context_code_cache_cbs` 只覆盖 `op_eval_context`。fetch signal
> 同样没有，功能即对齐。详见 §6。

---

## 3. 扩充后的实施顺序（依赖图）

```
P0 ✅ (46a8eb8, 双绿复验 2026-08-21)
  │
  ├─→ P1 (sqlx) ✅ 7867852 ──→ P2 (axum server + JS actor 线程) ✅ 61d7c2b ──→ P5a (WS 最小链路) ──→ P5b (WS JS 帧循环) ✅ a99a976
  │                        ↑
  └─→ P3 (config) ✅ 1ba0d72 ──→ P4 （生命周期装配 + 408 熔断） ✅ 8f27a32
                                          │
                                          └──────→ P6 (perf) ✅ 零改动收尾（代码缓存 API 不可达）
```

- **关键路径：** P1 → P2 → P4 → P5a → P5b；P3 与 P1/P2 并行，P6 随时可插。
- **P2 的 JS actor 线程模式是全案地基**：它同时化解 HTTP 的 Send 约束（本轮核实：RefCell 池 →
  Bridge !Sync，直接 await 编译不过）与 P5 的跨帧钉死。P2 做完，P5 风险从「未知架构」降为
  「填充逻辑」——P5a 探路仍应紧跟 P2 验证 channel 化的 WS 升级链路。

---

## 4. 风险与对策

| 风险 | 影响 | 对策 | 状态 |
|---|---|---|---|
| axum handler future 必须 Send（Bridge !Sync / JsRuntime !Send） | 直接 `.await` 编译不过；硬绕则 isolate 跨线程崩 | P2 建 JS actor 线程 + channel 桥，P5 复用同套 | ✅ 已实现并测试（P2/P5） |
| sqlx Any 首连前未装驱动 | `AnyPool::connect` 报 no installed driver | 连接前调 `sqlx::any::install_default_drivers()`（0.8.6 `any/mod.rs:35`） | ✅ 内聚于 `SqlxAccessor::connect`（P1） |
| 死循环 handler 无熔断 | 占死一个 actor 线程 | P4 watchdog + `v8::Isolate::terminate_execution` + 408 + runtime 不回池 | ✅ 已实现（KillSwitch，E2E + 冒烟） |
| edition 2024 × axum | 编译失败 | P2 第一步 `cargo add axum` 冒烟（edition 按 crate 生效，预期无碍） | ✅ 无碍（P2 起 workspace 编译通过） |
| serde_yaml deprecated | 无安全更新 | 内部配置解析，可接受；`Cargo.toml` 注释标注 | ✅ 接受（P3） |
| ~~sqlx runtime handler 时序~~ | — | **已证伪**：sqlx 0.8.6 无此 API，feature 编译期已定 | 移除 |
| ~~debug/release ASCII 不一致~~ | — | P0 已修（`46a8eb8`），双绿 2026-08-21 复验 | 移除 |
| ~~V8 代码缓存（P6）~~ | — | **已证伪**：`JsRealm` 为 `pub(crate)`，`execute_script_with_cache` 对外不可达 | 移除（零改动收尾） |

---

## 5. 测试计划

每阶段补测试，目标覆盖已有断言（2026-08-21 全部完成，debug/release 双绿）：

- **P0：** ✅ debug/release 双绿（7 测，2026-08-21 复验）。
- **P1：** ✅ `sqlite_roundtrip_via_bridge`（DDL/insert + `db.table` 构造器 + 参数化 `db.query`）；
  另有 TDD 修复的 unsigned 绑定回归覆盖。
- **P2：** ✅ router 移植 `router_test.go` 全 6 例 + actor 桥测 3 例（单 actor / 错误上抛 / 池分发）+
  axum E2E（200/404/500 信封，raw TCP）。
- **P3：** ✅ config 8 测（Defaults / **EnvOverlay=文件叠加** / BaseDirEmpty 回落 / 显式缺失报错 /
  非法 yaml / parse_duration / write_default 往返）。
- **P4：** ✅ 熔断 E2E：`while(true)` handler + 150ms → 408 信封；bridge 级
  `infinite_loop_times_out_and_bridge_survives`（超时 runtime 丢弃、后续请求存活）；
  `with_dbs` 命名注入 + default 回落；devserver 5 测（parseArgs 表驱动 / generate-config /
  零配置装配 / 缺失配置报错 / bad DSN 上抛）；冒烟：demo 路由 3 行种子 + 408。
- **P5：** ✅ WS 4 测：echo 链路（裸 TCP 掩码帧）、逐帧 handler 复跑、`ws.send` 顺序 + `ws.close`、
  缺 handler 安静关闭；bridge 级 `ws_global_send_close_and_reset`。
- 端到端：✅ `GET /crm-v1/user/profile/list` → 统一信封（冒烟 + 测试双覆盖）。

---

## 6. 实现后记录（填 commit hash / 取舍 / 问题）

- **[P0]** commit `6d9166b` 之后：删 `bootstrap.js` 中文注释（改英文），根因 deno_core 0.409
  的 7-bit ASCII 校验 debug 严格、release 放行。取舍：放弃"release 允许中文"，改源头清 ASCII，
  避免 debug/release 行为分裂。`cargo test` 与 `--release` 均 7 passed。
- **[复核 2026-08-21]** 全文结论逐条对照源码验证：① 双绿复验（debug/release 各 7 passed）；
  ② **证伪** sqlx 0.8.6 需 `set_handler`（源码无此 API，真实约束为 `any::install_default_drivers`）；
  ③ **证伪** P2「无 !Send 风险」（`RefCell` 池 → Bridge !Sync，须 JS actor 线程，P5 复用）；
  ④ **修正**「env 覆盖」= `cfg.<env>.yml` 文件叠加而非 OS 环境变量；⑤ **确认** `execute_script_with_cache`
  存在（deno_core 0.409 jsrealm.rs:493）、`v8::Isolate::terminate_execution` 存在（v8-150.4.0
  isolate.rs:993）；⑥ 细节逐条核实（ws 仅 send/close、chan cap 64、每连接独占 VM、
  408 熔断、SetMaxOpenConns(1)、仅 sqlite 驱动、router 4 段格式）。P1 风险 🟡→🟢，P2 架构改为 actor 线程。
- **[P1]** commit `7867852`：`connect()` 内幂等安装 Any 驱动 + sqlite 单连接池（对齐
  `SetMaxOpenConns(1)`，`:memory:` 必需）；builder 换 `SqliteQueryBuilder`（真库不吃 `$1`）。
  TDD 暴露并修复隐藏 bug：`value_to_json` 缺 unsigned 分支 → LIMIT/OFFSET 绑 NULL → sqlite
  code 20。集成测 `sqlite_roundtrip_via_bridge` 覆盖 DDL/insert + `db.table` + 参数化 `db.query`。
  双绿 8 passed。取舍：驱动安装放 `connect()` 内（调用方不可能忘记，DIP 根因位）。
- **[P2]** commit `61d7c2b`：workspace（root + server）+ router（6 测全移植）+ actor
  （`JsActor::new(impl Fn() -> Bridge + Send)`——Bridge !Send 不可搬预构实例，改为线程内工厂构造；
  actor 跨线程往返在多线程 runtime 下实测通过，P5 模式已验证）+ axum fallback 全链路
  （200/404/500 信封，raw TCP 端到端测）。取舍：dev 模式 resolve 出文件即读即执行
  （per-request 读盘=免费热重载，对齐）；HandlerStore 嵌入 map 留 P4。params 键对齐
  （仅 sub/feature/entity）。TDD 偏差说明：router/actor 严格 RED→GREEN；axum 装配层系实现与测试
  同批写入（E2E 真实 HTTP 覆盖 200/404/500），未先观 RED。
- **[P3]** commit `1ba0d72`：config 层完成：`load_from(dir, path, env)` 三层叠加（Value 级深合并 =
  yaml「.Unmarshal 进已填充 struct」的语义等价）；显式缺失报错/env 缺失静默/归一化顺序全对齐。
  取舍：Duration 存字符串（裸数字纳秒形式不支持）；`load_from(dir,..)` 代替依赖进程 CWD
  （Rust 测试并行，t.Chdir 等价物不存在）。默认 DSN 已改 `sqlite::memory:`
  （`file::memory:?cache=shared` 是 sqlite 驱动格式，sqlx 不识别）。
- **[P4]** commit `8f27a32`：Server 生命周期：① **watchdog 熔断**——`KillSwitch`（每 Bridge 一个看门狗线程，
  25ms 粒度）arm 记 isolate 裸指针 + deadline，到期跨线程 `v8::Isolate::terminate_execution()`
  （V8 允许），该 runtime **不归还池**；`RunError::Timeout` → actor `RunFail{timeout:true}` →
  axum 回 **408 信封**（E2E 测 + 冒烟 236ms 熔断 + server 存活均通）。unsafe 集中 runtime.rs
  单点 + SAFETY 注释。② **devserver 装配**（`server/src/devserver.rs` + bin 薄壳）：
  parse_args（--config/--env/--generate-config + APP_ENV 回落）→ config 加载 → 逐 `db.<name>`
  开共享池（对齐 共享 *sql.DB，非 per-actor）→ 仅 default 播种 user_profile（neo/trinity/morpheus）
  → `Bridge::with_dbs` → `JsActor::pool(PoolSize)` → normalize_addr（":8080"→"0.0.0.0:8080"）。
  冒烟：demo 路由原样返回 3 行种子 + 404/408 信封全对齐。③ **删除 `set_db_accessors`**（零调用方
  且必 panic：`RuntimePool::new` 已 clone stable → refcount≥2 → `Arc::get_mut` 永 None），
  改 `with_dbs` 构造期全量注入 + 无 "default" 键回落第一个（防御对齐）。取舍：redis 配置
  warn 后忽略（M0 内存 KV）；HMR 不建 reloader（per-request 读盘=免费热重载）；JsActor 无 shutdown
  接口（进程退出即散，需要时再加）。双绿 18+18。TDD：408/with_dbs/devserver 全 RED→GREEN
  （devserver 5 测含 parseArgs 表驱动移植 + bad DSN 上抛）。
- **[P5]** commit `a99a976`：WebSocket 帧循环完成（全案最大风险项一次通过）：**5a** echo——upgrade 后整个 socket
  （`WebSocket: Send`）搬到专用 OS 线程的 current_thread runtime，「upgrade + 钉单线程」链路以裸 TCP
  帧级测试（掩码帧手写）验证。**5b** JS 帧循环（`server/src/ws.rs::js_route` + `frame_loop`）——
  每连接专用线程 + 专用 Bridge（对齐 每连接独占 VM，不占 HTTP actor 池）；Reader/Writer 两 task +
  Processor 内联，msgChan/respChan cap 64（满则丢 + warn，对齐）；`bridge::run_ws` 带出
  `WsOutcome{sends, close}`（ws.send 先于信封写出、ws.close 结束连接）；缺 handler 文件发 Close 帧
  后退出（修复了裸 drop 导致的 TCP RST）。根 crate 侧：`op_ws_send/op_ws_close` + bootstrap `ws`
  全局（HTTP 路径不读 ws 字段 = nil 连接 no-op）。双绿 root 19 + server 22。TDD 偏差说明：
  run_ws/ws 全局严格 RED→GREEN；js_route 帧循环实现与测试同批写入（但测试当场抓出 RST bug 并修复）。
- **[P6]** commit `15fe22f`：结论 **零改动收尾**。① `execute_script_with_cache` 存在（jsrealm.rs:493）
  但**对外不可达**——接收者 `JsRealm` 为 `pub(crate)`（runtime/mod.rs:25）、`JsRuntime::main_realm()`
  亦 `pub(crate)`（jsruntime.rs:1417），embedder 拿不到 realm 实例；唯一公开缓存钩子
  `set_eval_context_code_cache_cbs` 只覆盖 `op_eval_context` 不覆盖 `execute_script`。计划「换
  with_cache 版」的前提证伪，handler 复跑提速由 runtime 池化（已实现且测试覆盖）承担。
  ② fetch signal/AbortController：同样没有，功能即对齐，不扩。

---

**[收官 2026-08-21]** P0–P6 全部完成（`46a8eb8` → `15fe22f`，7 个实现 commit + 文档收尾）。
双绿 root 19 + server 22（debug/release 各跑）。残留两项
（`Redis(name)` / `XORM(name)`）亦属待定扩展，如需移植另起计划。
