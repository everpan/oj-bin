# FFI 插件注册解耦设计：按轴 dlsym，消灭 PluginRegistrations

日期：2026-09-05
状态：已确认（方案 B：按轴导出符号）
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
4. `tools/xtask` `--check` 预检：符号存在性探测替代读 register 返回值。
5. `.github/workflows/plugin-matrix.yml`、CLAUDE.md/docs（plugin-architecture.md、
   plugin-development.md）同步。

## 测试

- `tests/plugins/mini` 扩展三种夹具：全轴、单轴、零轴（零轴应可加载但无任何注册）。
- 加载器单测：缺符号 = `None`；缺 `oj_plugin_init` / 版本不符仍 fail-fast（既有路径回归）。
- e2e：`cargo xtask build` 后 oj server 正常装配全部第一方插件（既有 workspace 测试覆盖）。

## 已知取舍

- 符号名含轴名 → 轴标识受 C 标识符约束（本就如此：现有轴名全为小写单词）。
- 宿主探测是编译期已知轴集合：宿主老、插件新轴 → 静默忽略该轴（与"插件不提供"不可
  区分）。可接受：ABI 严格门禁保证宿主与插件同代；跨代组合本就被拒绝。
