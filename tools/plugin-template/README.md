# oj 插件开发模板（泛型轴）

可拷贝起手的 cdylib 插件骨架：一个 `cache` 泛型轴（进程内 HashMap，仅作范式载体）。
按本模板可**零宿主改动**开发新轴——泛型轴名只要不撞宿主 9 个保留类型化轴
（`es` / `db` / `blob` / `bus` / `kv` / `auth` / `mq` / `mail` / `ldap`），
自动走泛型通道，JS 侧经 `axis()` 全局调用。

> 模板 crate **不进 workspace members**（它不是构建目标，是被拷贝的骨架）。
> 骨架自带的空 `[workspace]` 段只为了让模板在 oj 仓库内能被独立
> `cargo build` 验收；**拷贝使用时按下面路径 A 删掉它**。

## 命名三方对齐（先读这段）

xtask 约定三者用同一个 `<name>`（本模板 = `cache`）：

| 位置 | 值 |
|---|---|
| crate 包名 | `oj-cache`（`oj-<name>` 惯例；产物 `liboj_cache.dylib`） |
| `cargo xtask plugin <name>` | `cache` |
| descriptor.name（lib.rs init） | `cache` → loader 文件名 `libcache.dylib` |

包名 ≠ descriptor 名（照 oj-db-mysql 包 / `"db-mysql"` 名）。`plugins:` 清单键与
`plugins.<name>` 透传段键也按 descriptor.name（`cache`）查。改轴名时三处一起改。

## 快速开始

### 路径 A：在 oj 仓库内开发（`cargo xtask plugin`，推荐）

```bash
cp -r tools/plugin-template plugins/oj-cache
cd plugins/oj-cache

# 1) 删 Cargo.toml 里的空 [workspace] 段（否则 crate 自成 workspace 根，
#    cargo xtask plugin 的 --workspace 构建不会构建它，产物也落不进根 target/）；
# 2) 把 "plugins/oj-cache" 加进根 Cargo.toml 的 workspace members；
# 3) 按需改轴名（见上表三处）。

cargo xtask plugin cache        # --workspace 构建 + 拷到 bin/plugins/<host-triple>/libcache.dylib
cargo xtask plugin cache --check  # 预检（ABI / 身份 / semver / 轴符号，经 PluginLoader 加载）
```

### 路径 B：独立 manifest（不进宿主仓库）

```bash
cp -r tools/plugin-template ~/my-oj-cache && cd ~/my-oj-cache
cargo build --release   # 保留空 [workspace] 段；调整 oj-plugin-ffi 的 path 依赖
# 手动按 loader 文件名拷入（<host-triple> 用 `rustc -vV | grep host` 取）：
cp target/release/liboj_cache.dylib <oj仓库>/bin/plugins/<host-triple>/libcache.dylib
```

config.yaml 声明（strict 清单或 scan 模式皆可），启动宿主：

```yaml
# strict 清单：键 = descriptor.name（只装配列出的插件）；
# 空对象值 = cfg 回落第 2/3 级解析（非空对象则原样透传给 init，最优先）
plugins:
  cache: {}
# 缺省 plugins 段或空 map = scan 模式（按 bin/plugins/<triple>/ 文件发现）
```

## 范式四契约（违反任一条 = 加载失败 / 调用出错 / UB）

1. **轴名小写**，且避开宿主保留的 9 个类型化轴（见上）。宏不做小写兜底，
   自报清单原样进 `oj_plugin_axes()`。
2. **vtable 必须静态可寻址**：`static VT: GenericVtable = ...;`，宏展开时取地址。
   堆上 vtable = 悬垂指针。
3. **vtable 方法必须 `catch_future` / `catch_void` 收敛 panic**：跨界 unwind
   跨 cdylib 边界是 UB。宿主对 vtable 方法**没有** catch_unwind（宏只保护 init）。
   ```rust
   extern "C" fn call(op: RString, args: RString) -> FfiFuture {
       oj_plugin_ffi::catch_future(|| dispatch(op, args)) // panic → 立即错误 future
   }
   ```
4. **业务错误经 future 的 Err 透传**（`ready_err(...)`），JS 侧收到
   `{code,msg}` 信封；panic 由 `oj_plugin_entry!` 与 `catch_future` 兜底成 Err。

异步实现（真实后端）用 `oj_plugin_ffi::spawn_ffi_future(&state().rt, async { ... })`
取代 `ready_ok/ready_err`——oneshot 收结果，poll/take/free 已由 ffi 统一实现
（照 `plugins/oj-db-mysql` 的 future 助手用法）。

## `oj_plugin_entry!` 范式

```rust
// 零轴 / 单类型化轴 / 多轴 / 带配置键 / 泛型轴（generic(...) 必须带括号）
oj_plugin_entry!(init);
oj_plugin_entry!(init, kv => &KV_VT);
oj_plugin_entry!(init, kv => &KV_VT, auth => &AUTH_VT);
oj_plugin_entry!(init, config: "cache", kv => &KV_VT);
oj_plugin_entry!(init, config: "cache", generic(cache) => &CACHE_VT);
// 类型化与泛型可混用：
oj_plugin_entry!(init, config: "cache", kv => &KV_VT, generic(cache) => &CACHE_VT);
```

- `config: "<key>"` 声明插件的配置键，**宏里必须前置**（单独规则避免与轴名歧义）。
- `kv => &VT` 是类型化 kind 臂（9 个保留轴才用）；`generic(cache) => &VT` 是
  泛型 kind 臂。泛型轴必须写 `generic(...)`——裸名字会被当成类型化 kind，
  loader 会把 `GenericVtable` 当类型化 vtable 用，行为未定义。
- 宏生成：`oj_plugin_abi_version` / `oj_plugin_init`（内置 catch_unwind）/
  每轴 `oj_plugin_axis_<name>` / 轴自报清单 `oj_plugin_axes()`；**仅当**写了
  `config:` 声明才额外生成 `oj_plugin_config_key()`，未列出的轴不导出符号
  = 不提供该轴。

### RResult 注意（stabby）

`RResult<T, E>` 是 `stabby::result::Result` 别名：`Ok`/`Err` 是**关联函数**，
只能构造、不能模式匹配。需要 match 时先转 std：

```rust
if let Err(e) = do_something() { ... }                    // do_something 返回 std Result
let r: Result<_, RString> = ffi_returns_rresult().into(); // 或 .into() 后 match
return RResult::Ok(descriptor);                           // 构造照旧
```

## 配置：config key 声明 + 三级解析

`config: "cache"` 声明后，插件 init 收到的 `cfg` 按**三级解析**（`oj/src/serve_cmd.rs` 的
`plugin_cfg`，先到先用）：

1. `plugins.<name>`（descriptor.name）非空对象 → 原样透传（最优先）；
2. 插件自报 config key → 查 **config.yaml 全量顶层段**（含宿主已知段——段是读
   不是占，撞已知名是合法场景）；段不存在 → `{}`（段可选）；
3. 按名遗留臂（仅宿主内置的 es/auth/mail/ldap 插件）。

用户侧示例（`config.yaml`）：

```yaml
# 第 2 级：顶层段，键 = 插件自报的 config key
cache:
  max_entries: 1000

# 或第 1 级：plugins 段按 descriptor.name 透传（非空对象原样进 init，优先级更高）
# plugins:
#   cache:
#     max_entries: 1000
```

**fail-fast 哲学**（照 oj-ldap）：init 里对 cfg 做键白名单校验，未知键返回
`RResult::Err`——装配期拒绝加载，而不是静默忽略拼错的配置。骨架的
`validate_cfg` 即演示。

## JS 调用

```js
// bootstrap.js 形态：axis("cache").get("k") → op_axis_call("cache", "get", "[\"k\"]")
// 参数是 JSON 数组字符串；返回值按 JSON 解析进 data。
const v = axis("cache").get("k");        // null（未命中）
axis("cache").set("k", { any: "json" }); // {"ok":true}
```

错误（未知 op / 入参不合法）走 future Err → JS 侧 `{code,msg}` 异常。
`axis()` 只认泛型轴：撞保留名会被当类型化轴路由，不是泛型通道。

## ABI / 加载注意事项

- `ABI_VERSION` 是**严格相等**门禁：插件与宿主编译自同一 `oj-plugin-ffi` 版本
  才允许加载。升级宿主后插件须重编译（`cargo xtask plugin <name> --check` 预检）。
- 向后兼容演进走 cfg 的 JSON 字段；repr(C) vtable 字段变更才需要 bump ABI。
- 插件 panic 不杀宿主：`oj_plugin_entry!` 的 init catch_unwind + 每方法的
  `catch_future` 双层收敛。
- 加载是单一 `dlopen`，句柄进程生命周期泄漏（设计如此）。
- 插件发现四级（先到先用）：`OJ_PLUGINS_DIR` 环境变量 > config `plugins_dir` >
  `<exe>/plugins` > `<workspace_root>/bin/plugins`，各拼 `<host-triple>/`。
- **宿主最低版本**：泛型轴通道（`axis()` + `oj_plugin_axes`）与 config key 自报
  是 v0.1.54 起的新能力——在 `desc` 里注明（骨架已含）。旧宿主（无
  `oj_plugin_axes`，逐轴 dlsym 只探 9 个类型化名）能加载本插件、init 会执行，
  但 `cache` 轴不可达——dlsym 表里没有这个名字；要 JS 可调必须新宿主。
  版本号以 CHANGELOG 为准（T9 统一发版时对齐）。
