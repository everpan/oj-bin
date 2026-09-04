# FFI 插件注册解耦设计：按轴 dlsym，消灭 PluginRegistrations

日期：2026-09-05
状态：已确认（方案 B：按轴导出符号 + 插件配置通用化，2026-09-05 用户补充插件配置需求）
前置：auth 解耦计划（docs/superpowers/plans/2026-09-05-auth-decouple.md）收尾后另起计划实施

## 背景与问题

`PluginRegistrations` 是定长 `#[repr(C)]` 结构体，每轴一个 `*const XxxVtable` 槽位
（es/db/blob/bus/kv/auth）。加新轴 = 结构体加字段 = 布局变更 = `ABI_VERSION` bump
（严格相等门禁）= **全部插件源码修改（字面量补字段）+ 全量重编译**。

实证：auth 解耦 Task 4 加 auth 轴时，7 个存量插件被迫各补一行 `auth: null` 并重编。
插件数量随轴数增长，此成本 O(轴数 × 插件数)，不可持续。

根因：**注册面是闭集（定长结构体），而"插件提供哪些轴"天然是开集**。

## 目标 / 非目标

目标：
- 加新轴时，存量插件零源码改动、零重编译、ABI 不 bump。
- 插件只声明自己提供的轴（单轴插件不再看见无关槽位）。
- 既有轴 vtable 形状变更仍受严格 ABI 门禁保护（安全边界不减）。

非目标：
- 不做运行时动态轴（轴集合由宿主定义，静态即可）。
- 不改 HostContext、init cfg JSON 契约、stabby 类型选型。

## 设计（方案 B：按轴 dlsym）

### 契约形态

插件导出符号（dlopen 句柄进程级持有，宿主随时 dlsym）：

```
oj_plugin_abi_version() -> u32                      // 保留，不变
oj_plugin_init(host, cfg) -> RResult<Descriptor,_>  // 保留，不变（状态初始化）
oj_plugin_axis_<name>() -> *const c_void            // 新增；未提供的轴不导出该符号
```

- `<name>` = 轴标识：`es` / `db` / `blob` / `bus` / `kv` / `auth` / …（由宿主侧静态表枚举）。
- 返回 `*const XxxVtable` 擦除为 `*const c_void`；宿主按轴转型。vtable 均为 `static`，
  无生命周期问题。
- **删除**：`PluginDescriptor.register` 字段、`PluginRegistrations` 结构体及其 `none()`
  /访问器（宿主侧聚合已有 `oj/src/server_cmd.rs::Registries`，插件侧不再看见聚合结构）。

### 插件侧宏

`oj_plugin_entry!` 演进为逐轴声明，宏只为给出的轴生成符号：

```rust
// plugins/oj-kv-redis：只写自己
oj_plugin_ffi::oj_plugin_entry!(init, axes! { kv => KV_VTABLE });

// plugins/oj-auth：只写自己
oj_plugin_ffi::oj_plugin_entry!(init, axes! { auth => AUTH_VTABLE });
```

宏展开 = 现有 `oj_plugin_abi_version` + `oj_plugin_init`（含 catch_unwind）+ 每轴一个
`#[unsafe(no_mangle)] extern "C" fn oj_plugin_axis_<name>() -> *const c_void`。

### 宿主侧探测

`src/bridge/plugin_loader.rs`：

```rust
/// 宿主认识的轴（加轴 = 此表加一行 + 对应 vtable 类型；插件无需任何变更）。
const AXES: &[&str] = &["es", "db", "blob", "bus", "kv", "auth"];
```

装配时逐插件逐轴 dlsym：符号缺失 = 不提供该轴（与今天 null 槽语义一致）→
`Registries`（宿主聚合结构）按现有逻辑填 `None`。冲突检测（多插件同轴）、
fail-fast 规则全部不变。

### ABI 语义修正（本次重构的核心价值）

| 变更类型 | ABI_VERSION |
|---|---|
| 既有轴 vtable 形状变化（加方法/改签名/字段） | bump，严格相等门禁保留 |
| **新增轴**（新 vtable 类型 + 宿主探测表加行） | **不 bump**（纯加法） |
| cfg JSON 加字段 | 不 bump（沿用现有规则） |

本次迁移本身是 6→7（最后一次破坏性变更：删 `register` 字段 + 删
`PluginRegistrations`）。

### panic 边界（不变）

- `oj_plugin_init` 仍由宏内建 `catch_unwind`。
- vtable 方法无宿主侧 catch_unwind，插件实现侧仍必须经 `catch_value`/`catch_future`
  收敛 panic（oj-auth 现行做法，成为文档化义务）。
- 轴符号本身返回静态指针，无 panic 面。

## 迁移清单

1. `oj-plugin-ffi`：`PluginDescriptor` 删 `register`；删 `PluginRegistrations`；
   `oj_plugin_entry!` 支持逐轴声明；`ABI_VERSION = 7`。
2. 8 个存量插件 + `tests/plugins/mini` 夹具：改用 `axes!` 形态（每插件约 3 行 diff）。
3. `src/bridge/plugin_loader.rs`：`AXES` 探测表 + dlsym 装配；`LoadedPlugin.registrations`
   改由探测结果构建（保留宿主侧聚合，下游 `es_backend`/`db_backend`/`auth_guard` 等
   包装器不动）。
4. 插件配置：`src/config.rs` 增 `plugins: Option<HashMap<String, serde_json::Value>>`
   开放段；`oj/src/server_cmd.rs` 的 `plugin_cfg_json` 泛化为三级回落的 `plugin_cfg`
   （开放 map → 轴适配器表 → `{}`），适配器表与探测表同处声明。
5. `tools/xtask` `--check` 预检：符号存在性探测替代读 register 返回值。
6. `.github/workflows/plugin-matrix.yml`、CLAUDE.md/docs（plugin-architecture.md、
   plugin-development.md）同步。

## 测试

- `tests/plugins/mini` 扩展三种夹具：全轴、单轴、零轴（零轴应可加载但无任何注册）。
- 加载器单测：缺符号 = `None`；缺 `oj_plugin_init` / 版本不符仍 fail-fast（既有路径回归）。
- 配置解析单测：三级回落逐级生效；`plugins.<name>` 透传不被宿主改写；scan 模式以
  文件 stem 命中 cfg 键。
- e2e：`cargo xtask build` 后 oj server 正常装配全部第一方插件（既有 workspace 测试覆盖）。

## 插件配置（与按轴 dlsym 同批落地）

### 问题

配置面与旧注册面是**同一个闭集问题**：`plugin_cfg_json(cfg, name)` 在宿主代码里按插件名
逐个硬编码（`"es" → endpoint`、`"auth" → jwt_secret/...`）。加一个需要配置的插件 =
宿主源码改 `plugin_cfg_json`。auth 解耦 Task 6 还暴露了 scan 模式的鸡生蛋问题：init 需要
cfg，而宿主在 init 前不知道插件名。

### 设计：配置不进 FFI 契约，进宿主解析规则

FFI 层**零变更**：`oj_plugin_init(host, cfg: RString)` 仍是唯一配置入口（JSON 字符串载荷）；
轴符号（`oj_plugin_axis_*`）不带配置。配置演化继续走 JSON 加字段（既有 ABI 规则）。

宿主侧把 `plugin_cfg_json` 泛化为单一解析函数，三级回落：

```rust
/// cfg 解析（装配期一次，与 AXES 探测表同处声明）：
/// 1) config.plugins.<name>   —— 开放式 map，宿主原样透传，不做任何字段解释
///    （第三方插件的正规通道；schema 由插件自定义）
/// 2) 已知轴适配器            —— 第一方顶层段（auth:/es:/blob:/broker:/redis.default）
///    由宿主按轴适配成 cfg（现 plugin_cfg_json 各分支收拢为与 AXES 同表的一张
///    axis → adapter 映射；顶层段仍是能力开关，宿主消费其做装配门禁，
///    插件相关字段经适配器透传）
/// 3) "{}"                     —— 无任何声明
fn plugin_cfg(cfg: &Config, name: &str) -> String
```

- **schema 归插件所有**：插件在 `init` 里解析并校验 cfg，非法即 `RResult::Err`
  fail-fast（oj-auth 的 `GuardCfg` 已是先例）。宿主只保证 JSON 可序列化，永不解释
  不认识的字段。
- **命名契约（鸡生蛋的解法）**：插件文件 stem == descriptor name == cfg 键。xtask 落盘
  已按 descriptor 命名（`libauth.dylib`），scan 模式以文件 stem 为 probe 键注入 cfg
  （沿 Task 6 `cfg_for` 现状，本文档化为正式契约）。
- **第一方顶层段的去留**：`auth:/es:/blob:/broker:/redis:` 保留——它们同时是宿主的
  能力开关与核心侧状态来源（如 `auth:` 供 `Extras.jwt` 与守卫装配门禁），不是纯插件
  配置。新增插件一律走 `config.plugins.<name>`，宿主零代码。

### 与按轴 dlsym 的协同

加一个「新轴 + 新插件 + 插件配置」的完整成本：

| 步骤 | 改动 |
|---|---|
| 新 vtable 类型 | oj-plugin-ffi 加一个文件 |
| 宿主探测 | `AXES` 表加一行 |
| cfg 适配（仅第一方） | 轴适配器表加一行；第三方则用户写 `config.plugins.<name>` |
| 存量插件 | **零改动、零重编译、ABI 不 bump** |

## 插件自描述与查询（2026-09-05 用户需求，同批落地）

### 需求

插件自描述 name / version / description；宿主加载时收集；提供 API 可查询。

### 设计

- **自描述进 descriptor**：`PluginDescriptor` 增 `pub desc: RString`（人类可读描述，
  插件作者填写；`name`/`semver`/`abi_version`/`fingerprint` 既有）。恰好 ABI 7 正在
  变更 descriptor（删 register），`desc` 搭车零额外成本。各第一方插件取其 Cargo.toml
  的 `description` 文案。
- **host 加载时收集**：`assemble_plugins` 装配完成后聚合 `Vec<PluginInfo>`
  （`PluginInfo` 增 `description: String`，From<&LoadedPlugin> 同步）。
  两条消费路径：
  1. `Extras.plugins`（既有通道，op_plugins / JS `globalThis.plugins()` 的数据源）——
     修掉生产装配传 `Vec::new()` 的既有缺口，`oj server` 从此返回真实清单；
  2. `AppState.plugins`（新增）——供内置查询端点。
- **查询 API**：内置 `GET {base}/plugins`（axum route，先于 catch-all fallback 注册），
  返回 ok 信封 `{code:0, data:[{name, version, description, abi_version, fingerprint}...]}`。
  定位与 `/health` 一致的**公共基础设施端点**：不走 Bearer 守卫、不受证书 GET 限制
  （监控/运维用途），GET only；`{base}/plugins` 为保留路径（同名业务路由被遮蔽，
  文档明示）。
- **不改 FFI 机制**：自描述走 descriptor（init 返回），不新增符号；按轴 dlsym 不携带
  元数据。

## 已知取舍

- 符号名含轴名 → 轴标识受 C 标识符约束（本就如此：现有轴名全为小写单词）。
- 宿主探测是编译期已知轴集合：宿主老、插件新轴 → 静默忽略该轴（与"插件不提供"不可
  区分）。可接受：ABI 严格门禁保证宿主与插件同代；跨代组合本就被拒绝。
