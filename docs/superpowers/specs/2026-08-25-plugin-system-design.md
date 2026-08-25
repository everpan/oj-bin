# 插件系统设计：JS 绑定层全量插件化（cdylib 动态库装配）

> 状态：已评审待落地。本文是 `docs/plugin-architecture.md`（下称"原案"）的评审修订版。
> 原案的现状分析（§1）、`Plugin`/`DbBackend` trait 思路、风险清单继续有效；
> 本文记录评审后的**决策定稿**。冲突处以本文为准。
> ⚠️ 与原案最大分歧：原案 §6 否决运行时 `.so`，本版经评审**改采 cdylib 动态库**——
> 否决理由中两条（`Extension` 非 repr(C)、V8 线程亲和）因"ops 全部上收 core"而失效，
> 仅剩 allocator/ABI 一条，由稳定 ABI 框架兜住（§3）。

## 决策定稿（评审结论）

| 议题 | 结论 |
|---|---|
| 装配方式 | **cdylib 动态库 + 启动期加载**：插件编译为 `.so`（Windows 为 `.dll`），按 OS 分类存放，启动时 libloading 加载。**不做**运行中热插拔（op 表在 `JsRuntime::new` 冻结，原案 §1.3 仍成立） |
| 插件化范围 | **所有绑定到 JS 的模块均可插件化** |
| core 边界 | **实用 core**：保留 `finish/json/http/log/module_loader/ws/fetch` + 五轴全部后端无关 ops（`op_kv_*`/`op_db_*`/`op_blob_*`/`op_bus_*`/`op_es_*` 全留 core） |
| 插件形态 | **唯一形态：纯工厂 cdylib**。插件不含 ops、不含 esm，只向宿主注册后端工厂（原案的"能力插件"概念取消） |
| 装配配置 | **`plugins` 清单**为唯一入口；缺省 = 全量清单；插件参数（bucket、brokers 等）不搬家，`from_config` 语义由宿主解析后经 FFI 传给插件 |
| 内置默认 | core 保留零依赖后端作 dev 默认：`memory` db、`local` blob、内存 bus、`InMemoryKV`；sqlite 亦内置（dev 开箱即用）。mysql/postgres/s3/kafka/rabbitmq/es/redis 为 cdylib 插件 |
| 产物与路径 | `plugins/<os-triple>/` 目录，详见 §4 |

## 1. 分层架构

```
Layer 3  装配层    oj / server：解析 plugins 清单 → PluginLoader 定位 .so/.dll
                   → libloading 加载 → ABI 校验 → 注册工厂 → builder
Layer 2  插件层    oj-db-mysql / oj-db-postgres / oj-blob-s3 / oj-bus-kafka
                   / oj-bus-rabbitmq / oj-es / oj-kv-redis   —— 均为 cdylib crate，
                   crate-type = ["cdylib"]，导出一个入口符号
Layer 1  框架层    core：五个注册表（kv/db/blob/bus/es，同构：accepts/connect/首个命中）
                   + PluginLoader（路径解析、libloading、ABI 校验）
                   + ffi.rs（全部 unsafe 收敛于此，稳定 ABI 类型定义）
Layer 0  核心层    JsRuntime 壳 + 全部 ops（含五轴后端无关 ops）
                   + bootstrap.js（全量 JS 命名空间常驻；未配置后端调用报 notConfigured）
```

统一范式一句话：**core 持有全部 op 与全部 JS 命名空间，插件只是跨 FFI 边界的后端工厂**。

## 2. 五轴注册表与命名多后端

- **db 多方言**：`DbBackendRegistry` 按 DSN 前缀认领，`DB(name)` 多连接多方言并存；
- **blob 命名多后端**（对原案阶段 3 的修订）：`StableState.blob` 改为
  `blob_backends: Arc<BlobBackendRegistry>`；`op_blob_*` 加 name 参数；
  bootstrap.js 提供 `blob(name)` 工厂，`globalThis.blob = blob("default")` 兼容旧代码；
  配置 `[blob.backends.local]`/`[blob.backends.s3]`；配置声明了名字但插件未装 → 启动期报错，
  JS 调 `blob("未配置名")` → 首次调用期报错；
- **bus**：`bus.rs` 的 broker 抽象提为 `BusBackend` + `BusBackendRegistry`，
  Kafka/RabbitMQ 由插件注册，内存 broker 内置，`op_bus_kind` 改自省注册表；
- **kv/es**：`op_kv_*`（redis 变体）与 `op_es_*` 上收 core 改为后端无关，
  经 `KVStore`/`EsClient` 注册表分发；`globalThis.redis`/`globalThis.es` 常驻，
  未装配时调用报 `"... not configured"`（语义不变）。

## 3. FFI 边界（全部 unsafe 收敛于 core `ffi.rs`）

- **稳定 ABI**：采用 stabby（或 abi_stable，实施计划阶段定稿）提供的 `repr(C)` 类型
  （`RString`/`RVec`/`RArc` 等），禁止裸 Rust 类型跨边界——杜绝 allocator 不匹配 UB；
- **插件入口**：每个 cdylib 导出两个符号：
  - `oj_plugin_abi_version() -> u32`：宿主先校验，不匹配 → 拒绝加载并明确报错；
  - `oj_plugin_init(host: &HostContext, cfg: &RStr) -> PluginDescriptor`：
    返回 {name, 版本, 注册回调}；宿主调用注册回调把工厂填入对应注册表；
- **async 桥接**：宿主在 `HostContext` 中注入 `spawn` 回调，插件的异步工作统一
  跑在宿主 tokio runtime 上（插件不自建 runtime，避免多 runtime 与线程亲和问题）；
- **工具链一致性**：插件与宿主必须同版本 rustc + 同版本 core-ffi crate 构建；
  `PluginDescriptor` 携带构建指纹，宿主校验不一致即拒绝；
- **审计纪律**：任何新跨边界调用必须先过 `ffi.rs` 评审，插件 crate 内部零 unsafe。

## 4. 产物与存放/加载路径

**构建产物**（CI 按平台矩阵产出，同一份源码三套二进制）：

```
plugins/
  x86_64-unknown-linux-gnu/   liboj_db_mysql.so      liboj_blob_s3.so    ...
  aarch64-apple-darwin/       liboj_db_mysql.dylib   liboj_blob_s3.dylib ...
  x86_64-pc-windows-msvc/     oj_db_mysql.dll        oj_blob_s3.dll      ...
```

- 文件名约定：插件名 `db-mysql` → crate `oj-db-mysql` → 产物
  `liboj_db_mysql.so` / `oj_db_mysql.dll`（`-` 一律转 `_`，前缀/后缀按平台）；
- **保存路径**：CI 构建后归置到上述布局随发行包分发；本地开发
  `cargo build -p oj-db-mysql` 后由 `xtask`/脚本拷入对应 triple 目录；
- **加载路径解析顺序**（先命中先赢）：
  1. 环境变量 `OJ_PLUGINS_DIR`；
  2. `oj.toml` 的 `plugins_dir` 配置项；
  3. 默认 `<可执行文件目录>/plugins`；
- 最终目录 = `<plugins_dir>/<宿主自身 target-triple>/`，宿主 triple 由 build.rs
  在编译期捕获（`TARGET` 环境变量）写入常量；
- 加载失败分类报错：目录不存在 / 文件缺失 / 平台不符（libloading 格式错误）/
  ABI 版本不符 / 符号缺失——各自独立错误文案，fail fast。

## 5. 启动期装配流程

```
oj.toml: plugins = ["db-mysql", "blob-s3", "bus-kafka", "es"]   (缺省 = 全量)
   │
   ▼  解析 plugins_dir（§4 顺序）→ 定位 <dir>/<triple>/
   ▼  逐个按名映射文件名 → libloading 加载 → oj_plugin_abi_version 校验
      → oj_plugin_init(HostContext{spawn, 配置JSON}, ...) → 工厂注册进对应 Registry
      （任一步失败：明确报错退出，无静默降级）
   ▼  内置后端（memory/local/sqlite/InMemoryKV）默认注册
   ▼  extensions 固定为 core 单扩展（插件不含 Extension）
   ▼  RuntimePool::new(stable, [bridge_ext], inspect)   ← op 表冻结
```

## 6. 健壮性

- 启动期 fail fast：未知插件名、文件缺失、ABI 不符、配置声明但插件未装，全部明确报错；
- 边界安全：稳定 ABI 类型 + unsafe 收敛单文件 + 构建指纹校验；
- op 命名唯一性问题消失：插件不再携带 ops，无重名可能；
- 未配置能力的 JS 调用报 `"... not configured"`；
- 无热插拔：运行中不 `dlclose`（卸载与 `Arc` 引用计数冲突），进程级重载靠重启。

## 7. 灵活性

- 发布期：按平台矩阵分发，用户只拷需要的 .so；
- 启动期：`plugins` 清单换装配，不重编译宿主；
- 扩展期：第三方按 `ffi.rs` 契约写 cdylib 即接入，不动 core、不进宿主 workspace 亦可；
- 编译期瘦身不再依赖 feature-gate——不拷 .so 即不加载。

## 8. 落地顺序（每步可编译、全测试绿）

- **阶段 0 — 五轴注册表（静态链接）**：kv/db/blob/bus/es 五个注册表全部建在 core，
  ops 全部上收为后端无关（es/kv-redis ops 上收在此步完成），现有后端全部改为
  经注册表解析——**此步仍是静态链接，行为完全不变**，200+ 测试兜底；
- **阶段 1 — blob 命名多后端**：`blob(name)` 工厂 + `BlobBackendRegistry` + 配置段；
- **阶段 2 — bus 注册表轴**：`BusBackend`/`BusBackendRegistry`，Kafka/RabbitMQ 注册化；
- **阶段 3 — FFI 边界 + 首个 cdylib 试点**：`ffi.rs`（stabby 定型）+ `PluginLoader`
  + `oj-es` 改造为 cdylib 走通全链路（含 ABI 校验与错误分类；sqlite 为内置后端，不作试点）；
- **阶段 4 — 全量 cdylib 化**：其余插件全部改造 + plugins 目录布局 + 加载路径解析
  + CI 平台矩阵产物；
- **阶段 5 — 文档**：`dev-manual.md`/`user-manual.md`（插件清单、目录布局、
  FFI 契约、第三方插件开发指南）；`OJ_TEST_*` env-gated 集成测试保留。
