# 05 · FFI 契约与插件（`oj-plugin-ffi/` + `plugins/`）

## 1. 契约 crate：`oj-plugin-ffi`

宿主与插件**共享同一 crate**，类型定义在 `#[repr(C)]` + `#[stabby::stabby]` 上。

| 项 | 值 | 说明 |
|---|---|---|
| `ABI_VERSION` | **7** | **严格相等**门禁。历史：2=db 轴、3=blob 轴、4=bus 轴+HostContext.deliver、5=kv 轴、6=auth 轴、7=按轴 dlsym |
| `HOST_FINGERPRINT` | rustc + crate 版本 + triple | 仅诊断，不匹配只告警 |
| `PluginDescriptor` | `{ name, semver, abi_version, fingerprint, desc }` | 任何字段变更都要 bump ABI |
| `HostContext` | `{ log(level,msg), deliver(topic,payload) }` | `RArc` 共享；插件互不可见（不提供 registry lookup） |
| `RString/RVec<T>/RBytes/RResult<T,E>/RArc<T>` | stabby 类型别名 | 跨边界安全 |

### `oj_plugin_entry!`（`lib.rs:89`）

```rust
oj_plugin_entry!(init);                                    // 零轴
oj_plugin_entry!(init, kv => &KV_VTABLE);                  // 单轴
oj_plugin_entry!(init, kv => &KV_VTABLE, auth => &AUTH_VTABLE);
```

展开出：`oj_plugin_abi_version()`、`oj_plugin_init()`（**内建 `catch_unwind`**，
panic → `RResult::Err`）、每轴一个 `oj_plugin_axis_<name>()`（返回擦除为 `*const c_void`
的静态 vtable 指针）。轴名强制小写。

⚠️ 宏**只保护 init**：vtable 方法须在实现侧用 `catch_value` / `catch_future` / `catch_void`
自行收敛 panic（`future.rs`）。

### 异步桥（`future.rs`）

`FfiFuture` 是唯一跨边界异步路径：`spawn_ffi_future` 把 Rust future 交给插件侧轮询；
`FfiGuard` 在 `Drop` 时释放。core 侧 `src/bridge/ffi.rs` 的 `await_ffi` 消费。

## 2. 宿主侧：`src/bridge/plugin_loader.rs`

### 加载门禁（spec §4，七类失败）

`FileMissing` / `PlatformMismatch`（含 glibc 基线）/ `DependencyResolution` /
`AbiMismatch` / `SymbolMissing` / `IdentityMismatch`（name 或 `@semver` pin）/
`InitFailed`（含 panic）。

流程 `load_one`（:326）：
`dlopen`（`ffi::load_forget`，句柄**进程期泄漏**）→ `oj_plugin_abi_version` 严格相等 →
`oj_plugin_init`（传 `HostContext` + cfg JSON）→ descriptor 内 abi 二次校验 →
指纹比对（只告警）→ 清单模式下 name/semver 核对 → **逐轴 dlsym**。

### 按轴 dlsym（ABI 7 起，加轴零破坏）

```rust
pub const AXES: &[&str] = &["es", "db", "blob", "bus", "kv", "auth"];   // :428
```

`probe_axes`（:432）对每个轴 `dlsym("oj_plugin_axis_<name>")`：
**缺符号或返回 null = 不提供该轴（非错误）**。加新轴 = `AXES` 加一行 + vtable 类型 +
`Registrations` 加字段，`probe_axes` 的 `match` 有 `unreachable!` 兜底防两表失步。

### 插件目录四级解析（`resolve_plugins_dir`，:233）

`OJ_PLUGINS_DIR` 环境变量 > config 的 `plugins_dir` > `<exe>/plugins` >
`<workspace_root>/bin/plugins`，各自再拼 `<host-triple>/`。
显式配置（1/2）目录不存在 → Err；默认（3/4）不存在 → `Ok(None)`（零插件）。

### 装配模式

- `load_manifest`（:464）：严格清单，文件缺失/校验失败 → fail fast。
- `load_scanned`（:482）：扫描目录下全部符合命名约定的库（文件名排序保确定性）；
  目录不存在/为空 → `Ok(vec![])`；**扫到但校验失败 → Err**（不静默跳过）。

### 适配器（把 vtable 包装成 core trait）

`es_backend` / `db_backend` / `bus_backend` / `auth_guard(_from_vtable)` /
`kv_backend_connect` / `blob_backend_connect` —— 全部在 core 构造
（`ffi.rs` 是 `pub(crate)`，unsafe 不外泄），装配层只经这些安全入口。

`PluginInfo`（:124）→ `op_plugins` → `GET {base}/plugins` + JS `plugins()`。

## 3. 第一方插件（`plugins/`）

| crate | 轴 | 依赖/要点 |
|---|---|---|
| `oj-es` | es | Elasticsearch HTTP 客户端（迁自 core `EsClient`） |
| `oj-db-mysql` | db | sqlx mysql；scheme 认领 |
| `oj-db-postgres` | db | sqlx pg；scheme 认领（迁移并发锁 `pg_advisory_xact_lock` 在 `oj/src/migrate.rs`，不在插件里） |
| `oj-blob-s3` | blob | `object_store` aws；presign 302 |
| `oj-bus-kafka` | bus | rdkafka；Windows 需 cmake 编 librdkafka（CI 有专门处理） |
| `oj-bus-rabbitmq` | bus | lapin |
| `oj-kv-redis` | kv | RedisKV（迁自 core `kv.rs`） |
| `oj-auth` | auth | Bearer 守卫；**进程级 GUARD 只认首次 init**（故 OIDC e2e 独立成测试目标） |

插件自描述 `descriptor.desc` 必填，经 `GET {base}/plugins` 公开。

## 4. 注册表与冲突策略（`oj/src/server_cmd.rs:264`）

| 轴 | 注册形态 | 冲突 |
|---|---|---|
| es | 键选单后端（handle 0） | 多个 es 插件 → fail fast |
| db | 认领式（按 scheme） | scheme 交集冲突 → fail fast |
| blob | 键选单 vtable 槽 | 多个 blob 插件 → fail fast |
| bus | 键选注册表（按 kind） | kind 冲突 → fail fast；kind 未装插件 → "unknown broker kind" |
| kv | 键选单 vtable 槽 | — |
| auth | 单槽 | — |

「配置声明了能力但插件未装」→ 启动期 fail fast（§2 闸门）。

## 5. 已知债 / 风险

- **ABI bump 是破坏性的**：任何 repr(C) 字段变更都要 bump，且宿主与插件必须同版本。
  向后兼容演进只走 cfg JSON 字段。
- `panic = "unwind"` 必须对所有插件 profile 成立；一旦有人覆盖为 `abort`，
  `catch_unwind` 失效 → 宿主 abort。根 `Cargo.toml` 有注释警示。
- vtable 方法的 panic 收敛靠插件自觉（宏不覆盖），新增插件需 review 这一点。
- `plugins/oj-auth` 的进程级 GUARD 单例会污染同进程的其他测试，是测试隔离的坑。
