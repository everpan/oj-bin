# 插件系统设计：JS 绑定层全量插件化（cdylib 动态库装配）

> 状态：已实现（阶段 0-6 全落地；review 意见 I-1/I-2/M-1..M-4 已全部吸收，全量回归绿，
> 见 `docs/plugin-system-handover.md` §5 接手点 A/B 与 `docs/superpowers/plans/2026-08-25-plugin-system.md` 阶段 6）。
> 本文是 `docs/plugin-architecture.md`（下称"原案"）的评审修订版。原案 §1 现状分析继续有效；
> 冲突处以本文为准。与原案最大分歧：原案 §6 否决运行时 `.so`，本版经评审改采 cdylib 动态库——
> 否决理由中两条（`Extension` 非 repr(C)、V8 线程亲和）因"ops 全部留在 core"而失效。

## 决策定稿（评审结论）

| 议题 | 结论 |
|---|---|
| 装配方式 | **cdylib 动态库 + 启动期加载**：插件编译为 `.so`（Windows 为 `.dll`），按 OS 分类存放，启动时 libloading 加载。**不做**运行中热插拔与在线升级 |
| 插件化范围 | **所有绑定到 JS 的模块均可插件化** |
| core 边界 | **实用 core**：`finish/json/http/log/module_loader/ws/fetch` + 五轴全部后端无关 ops 常驻 core |
| 插件形态 | **唯一形态：纯工厂 cdylib**。插件不含 ops/esm/deno Extension，只向宿主注册后端工厂 |
| 装配配置 | `plugins` 清单为**裁剪与约束工具**：显式给出 → 严格按清单装配（缺失/版本不符 fail fast）；**缺省 = 扫描 `<plugins_dir>/<triple>/` 全部加载**（目录不存在/为空 = 零插件，仅内置后端，不报错） |
| 内置默认 | core 内置零依赖后端：`memory` db、sqlite db、`local` blob、内存 bus、`InMemoryKV`——dev 开箱即用 |
| 产物与路径 | `plugins/<os-triple>/` 目录，详见 §4 |
| 版本与独立编译 | 契约抽为独立 crate **`oj-plugin-ffi`**，插件只依赖它 + 各自后端 SDK，可脱离宿主 workspace 单独编译、独立仓库发版。**ABI_VERSION（u32，严格相等）是唯一硬门禁**；构建指纹（rustc/契约 crate 版本/triple）降级为诊断元数据，不匹配仅告警；`@semver` 为清单可选 pin，与兼容性判定正交 |
| async/runtime 模型 | **插件自建 tokio runtime**（init 时创建，专用线程；插件内 sqlx/reqwest/redis/rdkafka 的 TLS 在插件自己那份 tokio 上，天然成立）。跨边界 async = FFI 同步方法返回 **FfiFuture 句柄**，宿主 op 侧 await 其完成。**宿主不向插件注入 spawn** |
| panic 纪律 | 一切 host→plugin 调用点由契约入口宏内建 `catch_unwind` 收敛为 `RResult` 错误；插件必须 `panic=unwind` 编译；init 期 panic 归入加载失败分类，运行期 panic 归为后端调用错误；宿主进程不因插件 panic 终止 |
| 句柄生命周期 | `Library` 句柄加载成功**立即 `mem::forget`**（进程期存活，任何路径含装配失败均不 dlclose）；「加载 + forget」封装为 ffi.rs 单一函数 |

## 1. 分层架构

```
Layer 3  装配层    oj / server：解析配置 → PluginLoader 定位/加载/校验 → 工厂注册 → builder
Layer 2  插件层    oj-db-mysql / oj-db-postgres / oj-blob-s3 / oj-bus-kafka
                   / oj-bus-rabbitmq / oj-es / oj-kv-redis   —— cdylib crate，
                   只依赖 oj-plugin-ffi + 各自后端 SDK，可单独编译、独立仓库（源码位于 `plugins/`；构建产物归置 `bin/plugins/<triple>/`）
Layer 1  框架层    core：五个注册表 + PluginLoader + ffi.rs（全部 unsafe 收敛于此）
契约层   oj-plugin-ffi（独立 crate）：稳定 ABI 数据容器、vtable 定义、FfiFuture、
                   HostContext、PluginDescriptor、入口符号宏（内建 panic 收敛）、ABI_VERSION
Layer 0  核心层    JsRuntime 壳 + 全部 ops + bootstrap.js（全量 JS 命名空间常驻，
                   未配置后端调用报 notConfigured）
```

**层级归属澄清**：PluginLoader 实体在 core（框架层）；装配层（oj/server）只做
"解析配置 → 调用 PluginLoader → 拿注册结果"，不持有加载逻辑。

统一范式：**core 持有全部 op 与 JS 命名空间，插件只是跨 FFI 边界的后端工厂**。

## 2. 五轴注册表

两种归属范式，按轴取用（不强求同构）：

- **认领式**（db）：`DbBackendRegistry` 按 DSN 前缀认领，`DB(name)` 多连接多方言并存。
  `DbBackend` 元数据须**声明 schemes 列表**（如 `&["mysql://"]`），注册时与全部已注册后端
  （含其他插件）做交集检查，冲突即启动报错；
- **键选式**（blob/bus/kv/es）：name 或配置 kind 显式选择，不适用"首个命中"语义。
  - **blob**：命名多后端，`blob(name)` 工厂 + `globalThis.blob = blob("default")` 兼容旧代码；
    配置 `[blob.backends.local]`/`[blob.backends.s3]`；配置声明了名字但插件未装 → 启动期报错；
    JS 调 `blob("未配置名")` → 首次调用期报错。**HTTP 下载路由仅服务名为 `default` 的后端**：
    local 类后端在 name ≠ "default" 时 `url()` 明确报错（提示用 `get()` 或 s3 presign），
    s3 presign 不受影响；registry 工厂签名携带注册名；
  - **bus**：kind → 工厂查表，按配置 `broker.kind` 单选单一后端并跨 actor 池/WS 共享
    （语义与现状 `build_broker` 一致）；`op_bus_kind` 返回值语义不变
    （当前选中后端的 kind 字符串），注册表自省并入 `op_plugins`；
  - **kv**：`InMemoryKV` 内置兜底，RedisKV 迁插件；单选；
  - **es**：**先抽 `EsBackend` trait**（现状 `EsClient` 为具体类型），HTTP 实现迁插件作首个后端。

**现状修正**（吸收评审 [20][34]）：kv/es 的 ops 本就在 core 且 kv 已经 `KVStore` trait 分发，
不存在"上收"迁移；阶段 0 的真实工作是"五轴统一为注册表解析 + es 抽 trait"。

**注册表泛型化裁决**：五轴 Registry 的"存储 + 重名 fail fast + 自省遍历"同构，
抽最小公共 `NamedRegistry<T>`（insert 重名报错 / get / iter）消除样板；
各轴在其上包一层实现自己的冲突/认领语义（db 查 scheme 交集、其余查名字）。
不追求全泛型化——冲突语义本就不同构，强行统一是偶然复杂度。

**注册冲突语义**：plugins 清单先按名去重（同名两次 → fail fast，防 init 副作用重放）；
名键注册表重名注册 → fail fast（插件 vs 插件、插件 vs 内置均不允许覆盖）；
注册顺序 = 清单顺序，插件先于内置。

**`resolve_dsn` 归属**：未知 scheme 的 fail-fast 移入 `DbBackendRegistry.connect`
（无认领即报错；装配层不再硬编码 scheme 白名单，否则第三方插件的新 scheme 会被前置拒绝）；
sqlite 路径绝对化/建空库是 sqlite 专属逻辑，随内置 `SqliteBackend.connect` 进 core；
`oj/src/server_cmd.rs`、`oj/src/build_cmd.rs` 两处改道 `registry.connect`。

**`Extras` 迁移**：`Extras` 改作注册表载体（`blob_backends`/`bus_registry`/`es` 等），
`with_dbs_and_loader` 签名随阶段 0 调整；**`StableState` 字段同步改形**：
`blob: Option<Arc<dyn BlobBackend>>` → `Arc<BlobRegistry>`、
`es: Option<Arc<EsClient>>` → `Option<Arc<dyn EsBackend>>` 等——这是 op 层取数路径的
实际改动点；逐类调用点（oj/server 生产路径、server/src/ws.rs 共享注入、测试夹具）
在阶段 0 一并迁移；**"同一已连接 broker 实例跨 actor 池与全部 WS 连接共享"的现有语义
必须保留**。

## 3. FFI 边界（全部 unsafe 收敛于 core `ffi.rs`；契约类型定义于 `oj-plugin-ffi`）

**钉死的第一原则**：只有 `oj-plugin-ffi` crate 的类型允许跨边界；tokio/tracing/log 等
含 TLS/全局态的 crate 不跨边界（各自进程内各有一份，插件日志经 HostContext 回调上送宿主）。

- **数据容器**：stabby `repr(C)` 类型（RString/RVec/RBytes/RResult）。跨边界载荷统一：
  `serde_json::Value` 序列化为 JSON 经 RString 过界（宿主侧反序列化喂 op）；
  错误为「错误码 + RString 消息」，弃用 `Box<dyn Error>` 直传；
- **后端对象形态（默认保守）**：opaque handle（u64）+ 契约 crate 内的 `extern "C"` 函数
  指针表（vtable struct）。**stabby dynptr 升级为阶段 3 前置 spike 的门槛任务**——
  spike 证明可行才升级，否则保持保守形态；
- **core 侧适配器层（ffi.rs 的主体）**：现状 op 层消费 `Arc<dyn DataAccessor>` /
  `Arc<dyn BlobBackend>` 等 core trait 对象；FFI 插件产出的是 opaque handle + vtable。
  中间须有适配器：**每轴一个 `FfiXxxBackend` struct（`FfiDataAccessor`/`FfiBlobBackend`/…），
  实现 core 既有 trait，内部持 opaque handle、经 vtable + FfiFuture + tx 句柄查表转发**。
  全部 unsafe 的实际落脚点即此层，归 ffi.rs；**插件侧永远不直接产出 `dyn Trait` 跨边界**；
- **async 跨边界**：FFI 层方法全部同步签名，返回 FfiFuture 句柄（repr(C)，内部为插件侧
  runtime 驱动的 oneshot/共享状态）；宿主 op 侧 await 该句柄拿结果。插件异步工作跑在
  **插件自建的 tokio runtime**（init 时创建）上；宿主不注入 spawn。
  **FfiFuture 的 drop 语义**：宿主 op 被取消（请求超时/WS 断开）导致 FfiFuture drop =
  宿主放弃结果，插件侧任务允许跑完，不保证取消；现状"ReqState reset 丢弃 ActiveTx =
  drop 自带回滚"的语义显式保留为：core 侧适配器在 ReqState reset 路径对存活 tx 句柄
  调 vtable `tx_rollback`；
- **两个难点特判**：
  - `DataAccessor::begin -> Box<dyn TxSession>` 改为**句柄化**：`begin -> tx_id(u64)`，
    后续 `tx_query/tx_exec/tx_commit/tx_rollback` 携带 tx_id，后端内部查表——
    消掉嵌套 trait object 与双重析构归属；
  - `EventBroker::subscribe` 的 `UnboundedSender` 过不了边界 → 改为宿主经 HostContext
    注入 `deliver(topic, payload)` 回调（宿主侧函数指针，插件线程调用，须非阻塞投递）；
- **插件入口**（每个 cdylib 导出两个符号，由契约入口宏生成，禁止手写 `#[no_mangle]` 绕过）：
  - `oj_plugin_abi_version() -> u32`；
  - `oj_plugin_init(host: RArc<HostContext>, cfg: RString) -> RResult<PluginDescriptor, RString>`；
- **HostContext**：`RArc<HostContext>` 共享所有权传入，进程级有效，插件可无限期持有；
  内容为宿主回调集（日志上送、bus deliver 等）；**不提供 registry lookup——插件互不可见**，
  跨后端参数组合由装配层在宿主侧解析后经 cfg JSON 注入（有意的边界）；
  cfg 按值传入（插件须持久化时自行持有副本）；全部工厂注册须在 init 调用窗口内完成；
- **PluginDescriptor**：{name: RString, semver: RString, abi_version: u32,
  构建指纹: RString（rustc + oj-plugin-ffi 版本 + triple，诊断用）, 注册回调: extern fn 指针}；
- **panic 围堵**：契约入口宏对每个导出符号与注册回调统一包 `catch_unwind(AssertUnwindSafe(..))`，
  panic 映射为 RResult 错误（宿主侧 catch_unwind 拦不到跨界 panic，围堵必须在插件侧 shim 内）；
  宿主安装 panic hook 输出当前插件上下文与构建指纹用于归因；宿主不保证从插件 panic 恢复
  状态一致性（如注册表半注册），策略为归类报错退出；plugins 目录同级可选 `symbols/`
  子目录保留调试符号（split-debuginfo/.dSYM/.pdb）供线下归因；
- **工具链约定**：ABI_VERSION 严格相等是唯一硬门禁（stabby 自带 layout/canary 校验兜底
  实际不兼容）；构建指纹不匹配仅告警。**契约演进总则**：repr(C) 契约 struct
  （PluginDescriptor/HostContext/vtable）的任何字段变更 = ABI_VERSION bump（stabby
  canary 强制，无侥幸空间）；需向后兼容的扩展一律走 cfg JSON 加字段（天然兼容）
  或新增 HostContext 回调（同属 bump，但语义清晰）——这个不对称是契约设计的第一考量。
  明示代价：宿主升级 rustc 或契约 ABI 时，
  全部插件按发布矩阵重编重发——写成已知约束而非假装不存在；
- **审计纪律**：任何新跨边界调用必须先过 ffi.rs 评审；插件 crate 内部零 unsafe；
  ffi.rs 审计清单含「Library 句柄不 drop」「panic=unwind profile」两项。

## 4. 产物与存放/加载路径

**布局**（CI 按平台矩阵产出，同一份源码多套二进制；Linux 须以最旧受支持 glibc 的
镜像为构建基线，部署含 Alpine 时矩阵补 `x86_64-unknown-linux-musl` 行）：

```
plugins/
  x86_64-unknown-linux-gnu/   liboj_db_mysql.so      liboj_blob_s3.so    ...
  x86_64-unknown-linux-musl/  （按需）
  aarch64-apple-darwin/       liboj_db_mysql.dylib   ...
  x86_64-pc-windows-msvc/     oj_db_mysql.dll        ...
```

- 文件名约定：插件名 `db-mysql` → crate `oj-db-mysql` → `liboj_db_mysql.so` /
  `oj_db_mysql.dll`（`-` 转 `_`，前缀/后缀按平台）；映射双向成立，扫描零新增概念；
  **宿主不维护已知插件名单**，任何合法名字均按约定映射（第三方插件不动 core 由此成立）；
- **保存路径**：CI 归置发行；本地 `cargo xtask plugin <name>` 一条命令完成单独编译 +
  拷入**仓库根 `bin/plugins/<host-triple>/`**（与发行布局同形，cargo clean 安全）；
- **加载路径解析顺序**（先命中先赢；相对路径一律相对 oj.toml 所在目录解析）：
  1. 环境变量 `OJ_PLUGINS_DIR`（仅开发期，生产部署不应设置——见 §6 信任边界）；
  2. `oj.toml` 的 `plugins_dir`；
  3. `<可执行文件目录>/plugins`；
  4. dev 后备：build.rs 捕获 workspace root 编译期写入常量，指向 `<workspace>/plugins`
     （dev 构建下 `cargo xtask plugin` + `cargo run` 零环境变量闭环）；
  最终目录 = `<plugins_dir>/<宿主 target-triple>/`；显式配置了 1/2 而目录不存在 → 报错；
  走默认 3/4 而目录不存在 → 视为零插件；
- **Windows**：以绝对路径 + `load_with_flags(LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR |
  LOAD_LIBRARY_SEARCH_SYSTEM32)` 加载（排除应用目录与 CWD）；CI 构建优先 vendored
  依赖（如 openssl-vendored）减少传递依赖；第三方插件开发指南声明
  「插件须自包含或显式声明系统依赖」；
- **加载失败分类**（各自独立错误文案，fail fast 作用域见 §5）：
  文件缺失 / 平台或运行库不符（含 glibc 基线不满足，透出编译期基线版本）/
  **依赖解析失败**（透出 loader 原始错误文本，与格式错误分开）/ ABI 版本不符 /
  符号缺失 / **插件身份不符**（descriptor.name ≠ 清单条目名）/ init 返回错误或 panic；
- **升级与回滚**（重启式，无在线升级）：新产物先放为 `*.so.new`，
  `cargo xtask plugin --check`（loader dry-run）预检 ABI/semver；停服 → 旧文件改名 `.bak`
  → 替换 → 启动验证 → 删 `.bak`；回滚 = 停服换回 `.bak`；
  不做「加载失败自动回退次高版本」的静默降级。
  **宿主 ABI_VERSION bump 为主版本事件**：ABI 等值硬门禁 + 扫描模式 fail fast 的组合
  意味着未重编插件会直接挡住启动——必须先在全部部署目标铺齐同 ABI 的插件产物，
  再滚动升宿主；`op_plugins` 自省输出宿主当前 ABI_VERSION 供运维核对。

## 5. 启动期装配流程

```
解析 plugins_dir（§4 四级顺序）
   │
   ├─ plugins 清单显式给出 → 按清单装配（缺文件/版本不符 → fail fast）
   └─ 缺省 → 扫描 <plugins_dir>/<triple>/ 全部符合命名约定的库文件
      （目录不存在/为空 = 零插件；扫描到但校验失败的损坏插件仍 fail fast，不静默跳过）
   ▼  清单按名去重（重名 → fail fast）
   ▼  逐个：按名映射文件名 → libloading 加载（Windows 用上述 flags）
      → 句柄立即 mem::forget → oj_plugin_abi_version 等值校验 → 指纹比对（不符仅告警）
      → oj_plugin_init(RArc<HostContext>, cfg JSON)
      → descriptor.name 与清单条目/文件名身份核对 → semver 对照清单 @约束
      → 注册回调填工厂进对应 Registry（重名/scheme 交集冲突 → fail fast）
   ▼  内置后端注册（memory/sqlite/local/内存 bus/InMemoryKV，插件之后，同名冲突报错）
   ▼  op 表为 core 单扩展（插件不含 Extension）→ RuntimePool::new(...)   ← 冻结
```

「配置声明了能力但插件未装 → 启动期报错」（§2）是扫描缺省模式下功能正确性的兜底闸门。

## 6. 健壮性

- **fail fast 清单**：清单去重失败、显式清单条目文件缺失、semver 不满足 @约束、
  ABI 不符、插件身份不符、注册名/scheme 冲突、配置声明但插件未装、
  扫描到的损坏插件——全部启动期明确报错退出，无静默降级；
- **panic 不跨 FFI**：插件 panic 收敛为错误返回，宿主进程不因此终止（§3 围堵机制）；
- **Library 句柄一经加载即 mem::forget**，任何路径（含装配失败、正常退出）不触发 dlclose；
- **信任边界 = 文件系统权限**：插件 dlopen 后与宿主同权限执行——`plugins_dir` 须与宿主
  二进制同等权限保护（不可被低权限写）；`OJ_PLUGINS_DIR` 仅供开发期；
  `.so` 完整性校验（签名/sha256 清单）本期不做，列为已知限制；引入第三方 `.so`
  前需自行审计来源；
- 无热插拔、无在线升级：进程级重载靠重启（§4 升级流程）。

## 7. 灵活性

- 发布期：平台矩阵分发，用户只拷需要的 `.so`（缺省扫描模式下不拷即不被扫描到，天然不加载）；
- 启动期：同一套二进制 + `plugins` 清单换装配，不重编译宿主；
- 扩展期：第三方按 `oj-plugin-ffi` 契约写 cdylib 即接入，不动 core、不进宿主 workspace；
- 独立编译承诺的准确口径：**ABI 版本内**，插件独立仓库、单独编译、独立 semver 发版成立；
  宿主升级 rustc/契约 ABI 时全部插件随发布矩阵重编（已知约束，见 §3）；
- 编译期瘦身 = 不拷 `.so` 即不加载；阶段 4 验收含「core 二进制不再链接 mysql/postgres 驱动」。

**新增第六轴（如 llm 调用、新型存储）的可复制 checklist**：
① core 加后端无关 ops → ② bootstrap.js 挂命名空间（notConfigured 占位）→
③ 定义 trait + 注册表（归属信息在连接串里 → 认领式；在配置名/kind 里 → 键选式）→
④ 内置零依赖兜底后端 → ⑤ oj-plugin-ffi 加该轴 vtable（= ABI bump，见 §3 契约演进总则）→
⑥ 首个 cdylib 插件按 §4 布局落地。五轴走通后此路径每一步都有先例可循。

## 8. 落地顺序（每步可编译、全测试绿）

- **阶段 0 — 五轴注册表（静态链接，行为不变）**：五轴统一为注册表解析；es 抽
  `EsBackend` trait；`Extras` 改注册表载体并迁移全部调用点；`resolve_dsn` 两处改道
  `registry.connect`（未知 scheme 报错移入注册表，sqlite 归一化随 SqliteBackend 进 core）；
  200+ 测试兜底，验收含 `resolve_dsn` 单测迁移后仍绿、broker 跨 actor/WS 共享语义回归；
- **spike（阶段 3 前置门槛）**：stabby vs abi_stable 选型 + FfiFuture + tx 句柄化的
  DataAccessor FFI 版最小可运行样例；产物决定后端对象形态是否升级 dynptr；
  失败回退预案 = 保留编译期链接 feature；
- **阶段 1 — blob 命名多后端**：`blob(name)` 工厂 + 注册表 + 配置段；下载路由仅服务
  `default` 的裁决落地；验收含非 default local 的 `url()` 报错行为 + default 路由字节一致回归；
- **阶段 2 — bus 注册表轴**：`BusBackend`/kind 查表注册表，Kafka/RabbitMQ 注册化，
  `op_bus_kind` 语义不变；
- **阶段 3 — 契约 crate + FFI 边界 + 首个 cdylib 试点**：`oj-plugin-ffi`（入口宏内建
  panic 收敛、FfiFuture、句柄化 tx、deliver 回调）+ `PluginLoader`（加载即 forget、
  四级路径解析、错误分类）+ **core 侧适配器层**（ffi.rs 内每轴 `FfiXxxBackend`
  实现 core trait、转发 vtable）+ `cargo xtask plugin`（拷贝目标 = 仓库根 `bin/plugins/<triple>/`，
  与 dev 后备查找对应）+ `oj-es` 改造为 cdylib 走通全链路 + `op_plugins` 自省 op
  （列插件名/semver/ABI/指纹）。**硬验收**：插件 cdylib 内真实执行 sqlx 连接查询 +
  reqwest 请求 + `tokio::time::sleep` 不 panic；host op → FFI 同步方法 → 插件 runtime
  异步执行 → FfiFuture 完成 → host 拿结果的全链路集成测试；
  适配器层单测（vtable mock 验证转发/drop-rollback 语义）；
- **阶段 4 — 全量 cdylib 化**：其余插件全部改造（mysql/postgres 插件内嵌各自
  DataAccessor 实现，插件边界内可用 `sqlx::Any` 单方言 feature；共享逻辑抽插件侧公共
  crate 或接受复制的决策在此步做）；core 的 sqlx features 去掉 mysql/postgres
  （收敛 sqlite-only）；CI 平台矩阵 + Linux glibc 基线；
- **阶段 5 — 文档**：`dev-manual.md`/`user-manual.md`（插件清单与扫描语义、目录布局、
  升级回滚流程、FFI 契约、第三方插件开发指南、panic 归因流程）；
  `OJ_TEST_*` env-gated 集成测试保留。
