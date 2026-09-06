# 模块说明索引（docs/modules/）

> 本目录是 `only-js`（代号 **oj**）的**按模块（crate / 目录）说明**。每篇文档面向「要读代码
> 或改代码的人」，回答三件事：**它负责什么 → 它由哪些零件构成 → 改它时要注意什么**。
>
> 与既有文档的分工：
> - `docs/user-manual.md` —— 面向使用者（CLI + config.yaml 权威参考）
> - `docs/dev-guide.md` —— 日常开发流程 + 内部实现走读（合并版）
> - `docs/devkit/api-manual.md` —— 面向 JS 业务开发者的全局 API 手册
> - **本目录** —— 面向维护者的**模块地图与边界**，按 crate 切分，含风险与债
>
> 命令与结构的最终权威是 `CLAUDE.md` 与代码本身；本目录给出的是导航与解释。

---

## 1. 模块一览

| # | 模块 | 路径 | 体量(LOC) | 一句话职责 |
|---|---|---|---|---|
| 00 | **总览** | 全仓 | — | 分层、依赖方向、请求全链路、状态模型、红线、文件地图 |
| 01 | **核心运行时** | `src/`（crate `only-js`） | ~11.5k | deno_core 桥：op 注册、JS 全局装配、状态模型、runtime 池、SQL 白名单、插件加载 |
| 02 | **配置模型** | `src/config.rs` | 661 | `config.yaml` 的权威 schema 与解析（段存在即启用） |
| 03 | **HTTP 服务** | `server/` | ~4.2k | axum：路由表、前置管线（鉴权/租户/上传）、JS actor 派发、证书门禁、WS、日志 |
| 04 | **CLI 编排** | `oj/` | ~7.2k | `server` / `build` / `test` / `migrate` / `fixture` / `schema diff`；装配与构建 |
| 05 | **FFI 契约 + 插件** | `oj-plugin-ffi/` + `plugins/*` | 553 + ~3.5k | C-ABI 契约（`ABI_VERSION`、vtable、入口宏）+ 8 个 cdylib 第一方插件 |
| 06 | **工具链** | `tools/`、`benches/`、`tests/plugins/` | ~1k | xtask 构建归置、oj-cert 证书工具、criterion 基准、测试夹具插件 |
| 07 | **模块数据层** | `oj/src/{manifest,schema,migrate,seed,checks}.rs` | ~2k | manifest / schema.yaml / migrations / seed / fixtures / 结构检查 S* |
| 08 | **测试体系** | `oj/tests/`、`tests/`、`sample/{unit,tests}/` | — | 四层测试（L0–L3）与分类方案、`sample/test`→`unit` 改名处置 |

---

## 2. 依赖方向（严格单向，无环）

```
        oj/  (CLI 编排：装配 + 构建 + 测试运行器)
         │
    ┌────┴─────┬──────────────┐
    ▼          ▼              ▼
 server/    src/          oj-plugin-ffi/  ◄── 被 oj/ 与 plugins/* 同时依赖
 (axum)   (only-js 核心)      ▲
    │          │              │
    └──────────┴── plugins/* ─┘   （插件只依赖契约，不依赖核心/服务）

tools/xtask   —— 独立构建工具（不参与运行时依赖图）
tools/oj-cert —— 证书生成/重签（`server` 的 dev-dependency 用其 test-support 夹具）
```

关键约束：

- **插件不依赖核心**。`plugins/*` 只 `use oj_plugin_ffi::*`；核心通过 `dlopen` + vtable 反向调用
  插件。这保证「不装的能力不进二进制、不进依赖树」。
- **`src/` 不依赖 `server/` 或 `oj/`**。核心只提供 `pub mod bridge` + `pub mod config`
  （`src/lib.rs`）。装配决策（连哪个库、挂不挂鉴权）全在 `oj/src/app.rs`。
- **`server/` 依赖 `src/`**，不反过来。WS 桥接所需的一切都由 `oj` 以 `make_bridge` 工厂传入。

---

## 3. 请求链路（一次 HTTP 请求穿过哪些模块）

```
HTTP 请求
 └─ server/src/lib.rs::handle()                     ← axum fallback（catch-all）
     ├─ 证书 GET 门禁（Expired/Grace → 403）          ← server/src/certificate*.rs
     ├─ {base}/blob/{key} 公开下载                    ← src/bridge/blob.rs
     ├─ RouteTable.lookup(path, verb)                 ← server/src/routes.rs（matchit）
     │    └─ 四态：Hit / Conflict(500) / MethodNotAllowed(405) / NotFound
     ├─ 前置管线 Pipeline
     │    ├─ AuthGuard.verify()   → 401 或 http.user  ← plugins/oj-auth（经 FFI）
     │    ├─ 租户头提取           → 400 或 http.tenantId
     │    └─ 体积上限 413 + multipart 解析
     ├─ JsActor.run_module()                          ← server/src/actor.rs
     │    └─ mpsc → 专用 OS 线程（current_thread rt，串行）
     │         └─ Bridge::run_module()                ← src/bridge/mod.rs
     │              ├─ RuntimePool.checkout()（复用 V8 isolate；必要时跑 ext_boot）
     │              ├─ ReqState.reset(req)（每请求隔离）
     │              ├─ KillSwitch.arm()（超时 → terminate_execution → 408）
     │              └─ side driver: import api.ts → default[method]() → op_* → 真实后端
     └─ Capture（{code,msg,data}）写回响应
```

---

## 4. 全局状态模型（跨模块最重要的一条约定）

| 状态 | 位置 | 生命周期 | 谁写 |
|---|---|---|---|
| `StableState`（`src/bridge/mod.rs:100`） | `Arc`，注入 `OpState` | **首次 runtime checkout 后不可变** | `oj/src/app.rs` 装配期一次性构造 |
| `ReqState`（`src/bridge/mod.rs:158`） | `OpState` | 每请求，checkout 时 `reset()` | bridge 自身 |

推论（踩过的坑，写在代码注释里）：命名 DB / KV / blob / 插件注册表**必须在首次 run 之前注入**，
之后 `Arc` 已共享，`Arc::get_mut` 会 panic。

---

## 5. 设计红线（跨模块，改动前必读）

1. **SQL 注入**：动态标识符**只**来自 `SchemaRegistry`（`src/bridge/query.rs`）；值**只**走绑定参数。
   裸 SQL 另有「表归属守卫」（`src/bridge/guard.rs`，默认 warn / `ownership_guard: deny` 时拒绝）。
2. **`JsRuntime` 是 `!Send`**：池与持有者必须同线程。server 侧用 `JsActor`（专用 OS 线程 + channel），
   inspector/WS 用 `spawn_local`。异步测试一律 `tokio::test(flavor = "current_thread")`。
3. **`panic = "unwind"`**：所有插件 profile 必须保持。`oj_plugin_entry!` 用 `catch_unwind` 收敛
   跨边界 panic；改成 `abort` 会让插件 panic 直接打挂宿主。
4. **`bootstrap.js` 必须 7-bit ASCII**（非 ASCII 触发 deno_core "Extension code must be 7-bit ASCII"）。
5. **失败的 runtime 丢弃，不归还池**（未轮询完 event loop 的 isolate 析构会触发 V8 句柄错误，有
   SIGSEGV 前科）。
6. **证书强制校验**：`server.public_key_path` + `server.certificate_path` 必须都配，无 config/CLI 逃生口
   （`src/config.rs:80` `cert_paths_configured()`）。
7. **禁止 debug 构建**：脚本/CI 一律 `--release`（`.cargo/config.toml` 已把 `build` 别名到 release profile）。

---

## 6. Review 摘要（2026-09-06）

> 详细发现分散在各模块文档的「风险与债」小节；此处只给结论。

### 6.1 做得好的

- **状态模型干净**：`StableState` / `ReqState` 二分 + checkout 时 reset，是「V8 池化复用而不串号」
  的关键，也是本项目最难的部分，注释把踩过的坑（RefCell 跨 await、看门狗自 join EDEADLK、
  isolate 析构 SIGSEGV）都钉死了。
- **能力可插拔且不泄漏依赖**：8 个后端轴全部 cdylib 化，核心不直接依赖 rdkafka/lapin/redis/
  opensearch；「按轴 dlsym」让加新轴对插件零破坏。
- **安全默认值**：证书门禁无逃生口、上传双上限（axum 2x 硬顶 + 信封 413）、路径穿越守卫在
  `resolve_static` / `decode_blob_key` / `routes.rs` 三处各自收紧、SQL 标识符白名单。
- **失败分类精细**：插件加载 7 类错误独立文案；`RunError::Timeout` 与 `Core` 分开 → 408/500 语义正确。
- **测试有层次**：Rust 单测 + `oj test` 进程内真实运行时（对标 Go Fiber `app.Test`）+ vitest 纯 mock。

### 6.2 风险与债（按优先级）

| 级别 | 问题 | 位置 | 说明 |
|---|---|---|---|
| 高 | ~~**`src/lib.rs#![allow(clippy::all)]`**~~ | `src/lib.rs` | ✅ 已整改：移除 crate 级豁免，修掉 4 处真实告警，`ffi.rs` 测试串行锁加带理由的局部 `#[allow]`；门禁 `cargo clippy --workspace --all-targets -- -D warnings` 全绿。 |
| 高 | ~~**L1/L2 测试未进 CI**~~ | `.github/workflows/plugin-matrix.yml` | ✅ 已整改：补 `sample-tests` job（xtask build + `./bin/oj test` + vitest）。 |
| 中 | ~~**`sample/test/` 与 `sample/tests/` 仅一字之差**~~ | `sample/` | ✅ 已整改：L2 改名 `sample/unit/`（后缀 `.spec.ts`）+ `sample/package.json` 统一入口 + 去重复用例。见 `08-testing.md`。 |
| 中 | **插件 vtable 的 panic 需插件自己收敛** | `oj-plugin-ffi/src/lib.rs:86` | `oj_plugin_entry!` 只保护 `init`；vtable 方法要插件用 `catch_value/catch_future` 自行包。漏包 = 宿主 UB/abort。 |
| 中 | **FFI 异步协议是手写 `FfiFuture`** | `oj-plugin-ffi/src/future.rs` | 自建 poll/take/free 三指针协议，正确但靠约定；`FfiGuard` 的 drop 语义（未取结果即 free）需严格配对。 |
| 中 | **docs 存在同主题多版本** | `docs/` | ~~`cli.md`（已归档）/ `cli2.md` / `user-manual.md`；`rust-core-runtime{,-revised}.md`；`plugin-architecture.md`（已归档）/ `plugin-development.md` / `plugin-system-handover.md`~~ —— 2026-09-06 已把 3 份历史预案移入 `docs/archive/`（附 `README.md` 指路），见下 §8。 |
| 低 | **`spikes/` 含 3 套 target 目录** | `spikes/*/target` | 归档验证性工程（stabby 选型 / FFI async / FFI tx），结论已落入 `oj-plugin-ffi`；目录体积大、易被误当活跃代码。 |
| 低 | **裸 SQL 表名提取是 best-effort** | `src/bridge/guard.rs:82` | 轻量词法扫描 + memo，复杂 SQL 可能漏判；设计上接受（查询构造器路径精确），但 `deny` 模式下有漏网风险。 |

### 6.3 整改记录（2026-09-06）

| 建议 | 状态 |
|---|---|
| 1. 把 L1/L2 接进 CI | ✅ 已补 `sample-tests` job |
| 2. 收敛历史文档到 `docs/archive/` | ✅ 已迁移 3 份历史预案，见 §8 |
| 3. `allow(clippy::all)` 降级为局部 allow | ✅ 已移除 crate 级豁免；修掉 4 处真实告警，测试串行锁加带理由的 `#[allow]`；`cargo clippy --workspace --all-targets -- -D warnings` 全绿 |
| 4. `spikes/` 标注为历史存档 | ⏳ 未做（`spikes/` 不参与构建，优先级低） |

---

## 7. 各模块文档

- [00 · 总览：分层、依赖与红线](00-overview.md) —— 第一次读仓库从这里开始
- [01 · 核心运行时 `src/bridge/`](01-core-bridge.md)
- [02 · 配置模型 `src/config.rs`](02-config.md)
- [03 · HTTP 服务 `server/`](03-server-http.md)
- [04 · CLI 与装配 `oj/`](04-oj-cli.md)
- [05 · FFI 契约与插件 `oj-plugin-ffi/` + `plugins/*`](05-ffi-and-plugins.md)
- [06 · 工具链 `tools/` · `benches/` · `tests/plugins/`](06-toolchain.md)
- [07 · 模块数据层 `oj/src/{manifest,schema,migrate,seed,checks}.rs`](07-data-layer.md)
- [08 · 测试分类与 `sample/test` vs `sample/tests` 处置](08-testing.md)

归档（**不描述当前实现**）：[`docs/archive/`](../archive/README.md) —— 早期 CLI 预案、
rust-core-runtime 方案与评审、插件系统交接快照。

审查清单见 [`docs/review-2026-09-06.md`](../review-2026-09-06.md)。
