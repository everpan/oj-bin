# 方案评审与修订：Rust 嵌入式 JS 后端

本文档基于 `docs/rust-core-runtime.md`（原始方案）与现有代码库 `/Users/ever/git/golang/only-js`
（已实现 `deno_core` 桥接 + 内存 fake 数据层）的现状，组织 **4 位专家评审**（Rust/V8 嵌入、
安全、后端架构/ORM、DevEx/工具链）并据此修订。

> 关键现状结论：**原始方案规划的技术路径（deno_runtime + SeaORM 实体名字符串路由）与已落地的
> 代码架构并不一致**。现有代码已用 `deno_core`（非 `deno_runtime`）、自定义 op（非 Deno Web API）、
> 原始 SQL `DataAccessor`（非 SeaORM）实现了阶段 0 + 阶段 1 骨架。四位专家一致建议**停止引入
> `deno_runtime` 与 SeaORM**，沿已有 `deno_core` + `sqlx/sea-query` 路线推进。

---

## 一、四位专家的核心共识

| # | 维度 | 原始方案 | 修订结论 |
|---|---|---|---|
| 1 | 运行时 | `deno_runtime`（权限 + Web API） | **弃用**。保留 `deno_core`。权限由"注册哪些 op"决定，无需 Deno 权限系统。 |
| 2 | 数据层 | SeaORM 实体名字符串路由 | **弃用 SeaORM**。改用 `sqlx` + `sea-query`（动态表/列名 + 参数化值）。 |
| 3 | SQL 注入 | 依赖参数化查询 | 当前 `db.query(sql)` 只有 `&str`，**无法传绑定参数，是现存的注入雷点**，须先修。 |
| 4 | 事务 | `db_tx_begin/commit/rollback` 自由 op | 改为 **回调式 `db.tx(fn)`**，Rust 持有生命周期，请求结束保底回滚。 |
| 5 | 调试/热更 | inspector + 热重载 + tsc（2~3 周） | 缩为 **3 天**：per-request runtime 使热重载近乎免费；inspector 走 `deno_core` 自带；删 runtime-tsc。 |
| 6 | 二进制体积 | < 80MB | 收紧为 **≤ 55MB** 作为 CI 回归门禁；deno_core-only 估算 35~50MB，本就达标。 |
| 7 | 最大风险 | 未识别 | 每请求新建 `JsRuntime`（V8 isolate 1~10ms 开销）须先测量，决定是否需要 isolate 池/快照。 |
| 8 | 最大缺口 | 未识别 | **HTTP 服务器**：`Capture` 已就绪却无人消费，是进入生产前的第一要务。 |

---

## 二、专家评审要点（节选）

### 2.1 Rust/V8 嵌入专家
- `deno_runtime` 提供的是 Deno Web API 表面（`Deno.*`/`fetch`/`File`），与本项目自研 SDK 全局对象竞争；
  且 `MainWorker` 共享一个 `OpState`，会**破坏**现有 per-request `Bridge` 的隔离性（除非每请求起 worker，代价高）。
- 现有 `Rc<RefCell<OpState>>` + per-request runtime 是 `deno_core` 的惯用且更优模型，应保留。
- `deno_runtime` 会拖入 V8 源码编译、API churn、+20~40MB 体积，直接威胁 16~20 周工期。

### 2.2 安全专家
- **`deno_runtime` 权限近乎冗余**：JS 沙箱无 `Deno.*` 命名空间、无原生 `fetch`/`import`/`Worker`，
  权限门禁的是"根本不存在的内置"。真实安全控制是 **op 注册期白名单**。
- **首要风险是 SQL 注入**：`db.query(sql)` 接受字符串，标识符（表/列名）无法参数化；
  改为"类型化查询构造器 + 服务端表/列白名单"从根上消除。
- **`fetch` = SSRF**：缺出网白名单、内网/链路本地 IP 阻断、超时与 body 上限、DNS 重绑定防护。
- **完全遗漏**：执行时限（V8 `terminate_execution` 看门狗）、内存上限（`ResourceLimiter`）、
  op 速率限制、多租户隔离、错误信息泄露。

### 2.3 后端架构/ORM 专家
- SeaORM 的价值在编译期类型化实体；而 JS 传入的是运行时字符串，桥接需"每实体每操作一个 match 臂"
  （EntityTrait 非对象安全），且结果最终仍 `Model → Value`，**白付代码生成成本、丢掉类型安全**。
- 推荐 `sqlx` + `sea-query`（SeaORM 的查询构造器，可独立使用，运行时选表/列，参数化值，直接驱动 sqlx）。
- `DataAccessor` 须先加 `query(sql, params)`：当前仅 `&str` 逼 JS 做字符串拼接，是现存注入雷点。
- 查询对象 v1 收敛为：类型化枚举过滤（`Eq/Ne/Gt/Gte/Lt/Lte/In/Like/IsNull`）、`AND` only、无 `$or`、
  `[{field,dir}]` 排序、`limit` 默认 100 硬上限 1000、关联延后。
- 事务用 `db.tx(fn)` 回调式，Rust 持有 begin/commit/rollback，`Bridge::drop` 保底回滚，每请求单活跃事务。
- TS 类型生成：用 `cargo xtask gen-types` 从 schema 注册表生成，手写 `db.ts`；不要 `build.rs` 魔法。

### 2.4 DevEx/工具链专家
- inspector **不需 `deno_runtime`**：`RuntimeOptions.inspector=true` + `JsRuntime::inspector()` 即可，
  CDP/WS server 仅 ~200 行可自托管。
- 热重载已免费：源码经 `execute_script` 注入、per-request runtime 无模块图可失效；"热重载"= 下次请求重读文件
  （mtime/`notify`/`ArcSwap`）。原始方案担心的"清理旧状态"在该设计下不存在。
- 二进制体积：`strip` + `lto=fat` + `codegen-units=1` 为主；`panic=abort` 需先验证 op panic 不终止进程；
  `opt-level="z"` 勿为省 MB 牺牲 JS 吞吐。**不要**为裁剪 V8 特性而从源码编译 V8（多小时 GN/ninja）。
- 跨平台：**用原生 runner，勿交叉编译**。macos-14(arm64)/macos-13(x64)/ubuntu(x64)/windows(x64)；
  linux-arm64 有原生 arm runner 才做，否则推迟。缓存 `~/.cargo` + `rusty_v8` 下载目录。精确 pin `deno_core`。
- 可观测性：在 `#[op2]` 边界统一埋 `metrics`（op 调用数/延迟、db 延迟、`Bridge::run` 脚本编译 vs event-loop 耗时、
  v8 堆用量）。**最优先测 per-request `JsRuntime` 构造耗时**——它决定是否需要 isolate 池/快照。
- 模块加载：用 `include_dir!` 编译期嵌入 handlers；设 `MDM_HANDLER_DIR` 时改为 FS 读取 + 监听热更。
  同一加载器、无打包步骤、无 esbuild。
- 测试：`cargo test` 单入口。新增 **handler 集成测试**（遍历 `handlers/*.js`，注入 fake `InMemoryAccessor`，用 `run_with`/`run_named` 断言 `Capture`）；
  **勿用 `deno test`**（缺 `json/db/http` 全局对象，须 mock 桥接）。

---

## 三、修订后的路线图（约 6~10 周，原 16~20 周）

### P0 — 生产可达性（最高优先）
1. **HTTP 服务器**：axum/tokio，每请求建 `Bridge`、用 `run_with(src, RequestInfo{..})` 跑 handler、
   取 `Capture` 写回响应、优雅关机。（最大缺口，`Capture` 已就绪，`run_with` 已返回它。）
2. **每请求 runtime 开销测量**：扩充 `benches` 测 `JsRuntime` 构造耗时；若占主导，引入 isolate 池或
   `deno_core::snapshot` 启动快照。
3. **修 SQL 注入雷点**：`DataAccessor` 加 `query(sql, &[Value])` / `exec(sql, &[Value])`，JS 可传绑定参数；
   内存 fake 同步支持 params。
4. **`fetch` SSRF 防护**：出网白名单（仅 https + 域名）、阻断 RFC1918/169.254/127/::1、超时 + body 上限、
   DNS 解析后复检（防重绑定）、禁用重定向到内网。

### P1 — 数据层与查询（核心能力）
5. **接 `sqlx`**：以 `DataAccessor` 同接口接入真实 MySQL/PostgreSQL/SQLite，连接池，`set_db_accessors` 复用。
6. **schema 注册表**：表 → 列/类型/主键/可排序字段。所有动态标识符的唯一校验权威。
7. **`sea-query` 查询构造器**：`db.table(name).select([...]).where({field:op:value}).orderBy([...]).limit(n)`，
   JS → 类型化枚举 → 参数化 SQL（标识符全部来自注册表）。v1 仅 `AND`、无 `$or`、limit 硬上限。
8. **事务 `db.tx(fn)`**：回调式，Rust 持有生命周期，请求结束保底回滚，每请求单活跃事务；原始 SQL 与构造器共用同一连接/事务。

### P2 — 安全加固与可观测（上线前必做）
9. **V8 沙箱**：执行看门狗（`terminate_execution`）+ `ResourceLimiter` 内存上限；op 参数尺寸上限；多租户隔离策略。
10. **op 边界埋点**：`metrics` + `/metrics` 端点（exec 时长、op 计数/延迟、db 延迟、v8 堆）。
11. **错误信息收敛**：`json.fail` 不泄露 Rust/DB 内部；结构化审计日志。
12. **handler 集成测试 harness**：遍历 `handlers/*.js` 用 `run_named` 断言 `Capture`。

### P3 — DevEx（按需，可延后）
13. **`include_dir!` 嵌入 + `MDM_HANDLER_DIR` 开发覆写**（热更）。
14. **dev-only inspector**（`--inspect` + feature flag），用于疑难调试。
15. **手写 `bridge.d.ts`**（8 个全局对象，~100 行）；实体增多后再用 `cargo xtask gen-types` 从注册表生成 `db.ts` 类型。
16. **CI**：原生 runner 多平台、`cargo test` + `clippy -D warnings` + `fmt --check` + 二进制体积门禁（≤55MB）、pin `deno_core`。

### 已删除/大幅缩减的原始阶段
- **阶段 4（deno_runtime 权限）**：由构造本身满足（无内置能力暴露），仅保留动态标识符校验。
- **阶段 5（inspector+热更+tsc）**：热更免费、inspector 走 deno_core、删除 runtime-tsc（交给编辑器/CI 的 `tsc --noEmit`）。
- **SeaORM 实体名路由**：替换为 `sqlx` + `sea-query`。
- **`<80MB` 目标**：收紧为 ≤55MB 门禁。

---

## 四、ADR（架构决策记录）摘要

- **ADR-1**：采用 `deno_core`，不引入 `deno_runtime`。理由：自研 op 已覆盖全部所需能力，且 deno_runtime
  的共享 `OpState` 会破坏 per-request 隔离、带来体积/构建/API churn 风险。
- **ADR-2**：采用 `sqlx` + `sea-query`，不采用 SeaORM 字符串实体路由。理由：运行期字符串键与 SeaORM
  编译期类型化实体不兼容，动态映射白付成本且丢类型安全。
- **ADR-3**：原始 SQL 作为永久逃生舱（报表/CTE/聚合），与构造器共用同一连接/事务。
- **ADR-4**：动态标识符的唯一来源是服务端 schema 注册表，非 JS 字符串——SQL 注入的根治点。

---

## 五、专家评审未覆盖/需后续确认
- 多租户/多 handler 路由（host/path → handler 映射）的归属与设计。
- 真实 DB 接入后 `benches` 数字会显著劣化（当前 `db.query 413ns` 是对内存 fake），需重测。
- 关联加载（原阶段 2）明确延后，v1 由 JS 发两次查询替代。

---

## 六、已落地实现（对照路线图）

> 开发遵循 TDD：每个能力均带单元测试（当前 `cargo test` 全绿：mdm-server 52 + oj 42 + e2e 15 + lib 60），并复用 `benches/bridge.rs` 保证 JS 全链路可编译。

### 已完成的条目（对应 P0~P3）

| 路线条目 | 状态 | 落地点 |
|---|---|---|
| P0-3 修 SQL 注入雷点 | 完成 | `DataAccessor::query_with_params` / `exec_with_params`，`op_db_query`/`op_db_exec` 收 `params: Option<Vec<Value>>`（`db.rs`）；`db.query(sql, params?)`/`db.exec(sql, params?)`（`bootstrap.js`） |
| P0-2 每请求 runtime 开销 | 完成（实测结论：需池化） | `RuntimePool`（`runtime.rs`）：复用 `JsRuntime`，idle 上限 `DEFAULT_MAX_IDLE=16`；实测每请求新建 isolate 1~10ms，故 checkout/checkin 复用；**快照等价于 bootstrap 已加载的预热 runtime**。 |
| P1-5 接 `sqlx` | 完成 | `SqlxAccessor`（`accessor_sqlx.rs`）：`Pool<Any>` 驱动无关接入；`bind_value` 参数化；`row_to_json` 类型探测；v0.2 增方言字段（`dialect_of` 按 DSN 前缀选 builder） |
| P1-6 schema 注册表 | 完成 | `SchemaRegistry`（`registry.rs`）：`table(name,pk,columns)`、`has_table`、`has_column`、`is_sortable`、`primary_key` |
| P1-7 `sea-query` 构造器 | 完成 | `query.rs`：`QueryReq`/`Op`/`Cond`/`OrderBy` → `op_db_query_build`；`db.table(name).select().where().orderBy().limit()`（`bootstrap.js`）；标识符全来自注册表，值全参数化；v0.2 按方言分发（sqlite/mysql/postgres 占位符） |
| P1-8 事务 `db.tx(fn)` | 完成 | 回调式（`bootstrap.js` `db.tx(fn)`：resolve 提交 / throw 回滚）；`ActiveTx` + `tokio::sync::Mutex<Box<dyn TxSession>>` 每请求单活跃事务、跨库报错；请求收尾 `finalize_tx` 保底回滚（`mod.rs`）；sqlx 实现 `SqlxTx`（commit/rollback take 防双重完结，drop 亦释连接）；`InMemoryTx` 同契约 |
| P3-13 模块加载 + 热更 | 完成 | `HandlerStore`（`loader.rs`）：`from_env`(`MDM_HANDLER_DIR`)/`from_embedded`/`from_dir` + `notify` 监听热更；`run_named` |
| P3-14 dev-only inspector | 完成 | `inspector.rs`：`RuntimeOptions.inspector` + `JsRuntimeInspector::create_local_session` + tungstenite WS；`Bridge::with_opts(..., inspect)` + `start_inspector`；`MDM_INSPECT` 启用 |
| 弃用 runtime-tsc | 完成 | 不再依赖 tsc；JS 为 ASCII ESM，靠编辑器/CI 的 `tsc --noEmit`（计划外，非运行时） |
| ADR-1 deno_core-only | 完成 | 全程 `deno_core 0.409`，无 `deno_runtime` |
| ADR-2 sqlx + sea-query | 完成 | 见上 |

### 关键设计决策（实现期修正）

- **快照/池化合并**：因 `JsRuntime` 是 `!Send`，池与持有它的 event loop 同线程（current_thread runtime）。
  "快照"由 `RuntimePool` 复用已加载 `bootstrap.js` 的 runtime 实现——bootstrap 仅编译一次，后续请求只执行 handler 源码。
- **状态分片**：`StableState`（`Arc`，跨请求共享：kv/dbs/client/registry）vs `ReqState`（per-request 可变，checkout 时 `reset`）。
- **失败 runtime 不回收**：`run_with` 仅在成功执（已轮询 event loop）时 `checkin`；失败的可能 isolate 损坏，直接 drop，避免复用损坏隔离。
- **移除 `warm()` 预热 API**：实测调用 `warm(2)` 创建空闲 runtime 却从未驱动 event loop，析构未轮询的 isolate 会触发 V8 FATAL
  `Cannot create a handle without a HandleScope`。改为"按需增长 + 借出即轮询"模型，无预热入口。

### 仍待办（P0 其余 + P2 安全加固）

- **P0-1 HTTP 服务器**：已完成（`server/` crate：axum `serve_router`/`serve_with_listener`、路由表 + dev 目录镜像、静态站点兜底；v0.1 交付，v0.2 在其上加租户/鉴权/blob/WS）。
- **P0-4 `fetch` SSRF 防护**：出网白名单、内网 IP 阻断、超时/body 上限、DNS 重绑定复检。
- **P2-9 V8 沙箱**：执行看门狗 `terminate_execution` 已完成（`KillSwitch`，`checkout_armed` 武装 + 到期跨线程 terminate，`v8::IsolateHandle` 修复 SIGSEGV）；`ResourceLimiter` 内存上限仍待办。
- **P2-10 op 边界埋点**：`metrics` + `/metrics`。
- **P2-11 错误信息收敛**：`json.fail` 不泄露 Rust/DB 内部。
- **P2-12 handler 集成测试 harness**。
- **P3-15 手写 `bridge.d.ts`**。
