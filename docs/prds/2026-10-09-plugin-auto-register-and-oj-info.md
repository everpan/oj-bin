# 设计：插件轴清单自报 + 泛型轴通道 + oj_info（phpinfo 对应物）

日期：2026-10-09 · 状态：待评审

## 背景与目标

当前插件机制（cdylib + FFI，ABI_VERSION=11 严格相等门禁）已实现 PHP 式「放入目录即加载」：
扫描模式（`plugins:` 段缺省/空 map）自动装配 `bin/plugins/<host-triple>/` 下全部插件，
新增**插件**（既有轴的新后端）零宿主改动。但仍有三个缺口：

1. **注册面手工表**：宿主靠 `AXES` 常量 + `probe_axes` 逐轴 dlsym + `provides()` 三处
   手工映射发现插件提供的轴，注释明示「加新轴 = 三处同改」，存在漂移风险。
2. **新增轴类型必须改宿主**：ffi vtable 类型、Registrations、probe_axes、bootstrap.js
   全局对象——即使插件作者只想加一种全新轴。
3. **无 phpinfo() 对应物**：版本/ABI/已加载插件/后端注册面只能靠日志与 `plugins()`
   零散拼凑，部署排障成本高。

目标：

- 轴注册改由**插件自报清单**，宿主删手工表；旧插件双轨兼容。
- 新增轴可走**泛型轴通道**：插件按模板独立开发，零宿主改动、零其他插件改动。
- 提供 **oj_info**：CLI `oj info` + JS `ojInfo()` 同源出口，phpinfo 风格诊断。

非目标（明确不做）：

- 不重构 9 个既有类型化轴（es/db/blob/bus/kv/auth/mq/mail/ldap），既有插件零源码改动。
- 泛型轴不进 HTTP 管线（不能做守卫类集成点）；守卫/管线能力仍走类型化 auth 轴。
- 不做插件携带 JS 代码注入（plugin-shipped JS glue 是另一条路，本次不选）。

## Part 1 — 轴清单自报

### FFI（`oj-plugin-ffi`，ABI_VERSION 保持 11）

新增两个 repr(C) 类型。**仅新增独立类型，不触碰任何既有 repr(C) 结构字段，
按仓库红线（「repr(C) 字段变更才 bump」）ABI 保持 11 不动**：

```rust
/// 一个轴声明：轴名 + 擦除后的 vtable 指针。
#[repr(C)]
pub struct AxisDecl {
    pub name: RString,                 // 轴名（小写；宿主匹配用）
    pub vtable: *const core::ffi::c_void,
}

/// 轴清单：静态数组 + 长度。
#[repr(C)]
pub struct AxisList {
    pub items: *const AxisDecl,
    pub len: usize,
}
```

`oj_plugin_entry!` 宏追加生成（宏展开时已知轴清单，零轴插件导空表）：

```rust
#[unsafe(no_mangle)]
pub extern "C" fn oj_plugin_axes() -> AxisList {
    static ITEMS: &[AxisDecl] = &[ /* 每轴一个 AxisDecl */ ];
    AxisList { items: ITEMS.as_ptr(), len: ITEMS.len() }
}
```

第一方 10 个插件只需重编译（宏自动产新符号），源码零改动。
`tools/xtask` 的 `--check` 预检走 PluginLoader，自然兼容。

### 宿主（`src/bridge/plugin_loader.rs`）

- `probe_axes` 重写：**优先** dlsym `oj_plugin_axes()`，按清单逐条按名匹配已知轴类型
  （match name → cast 为对应 vtable 类型填入 `Registrations`）。
- **符号缺失（旧插件）→ 回退**现有 AXES 逐轴 dlsym 探测，eprintln 一行 deprecated
  提示（含插件名）。旧插件兼容不断裂；回退路径保留至少一个大版本周期。
- 清单中的**未知轴名**（宿主不认识的类型化轴）：收集进
  `LoadedPlugin.unknown_axes: Vec<String>`，加载时 eprintln 告警。行为变化：
  旧宿主遇新插件新轴 = 「装上但没消费」；新宿主 = 「明确告诉你没消费」。
  泛型轴通道（Part 2）落地后，未知轴名应先尝试泛型解释，再落入 unknown_axes。
- `Registrations` / `provides()` 不变；`AXES` 常量删除。
- `tests/plugins/mini*` 夹具家族与 `plugin_loader` 单测更新：新增用例——
  自报清单优先、旧符号回退、未知轴告警、零轴插件空清单。

## Part 2 — 泛型轴通道（新轴 = 纯插件开发）

### 范式契约（模板规定）

```rust
/// 泛型轴 vtable：op 名 + JSON 参数 → JSON 结果。repr(C) 新类型，ABI 不变。
#[repr(C)]
pub struct GenericVtable {
    pub call: extern "C" fn(op: RString, args: RString)
        -> RFuture<RResult<RString, RString>>,
}
// 插件声明：oj_plugin_entry!(init, cache => &VT)
```

- panic 收敛沿用现有约定：vtable 方法宿主不 catch_unwind，**插件侧必须以
  `catch_future`/`catch_value` 包装**（宏只保护 init；模板必须演示这一点）。
- 连接/资源语义：YAGNI，不做 blob/kv 式 connect 契约；插件在 call 内自理
  （可借 init 期拿到的 cfg 自建连接池）。
- 错误：未知 op / 参数非法 → 插件返回 `RResult::Err` 文案，宿主原样透传为 JS 异常。

### 宿主（一次性实现，之后新轴永不再改宿主）

- 清单中轴名不在 9 个已知类型化轴 → 按 `GenericVtable` 解释，注册进
  `HashMap<String, GenericAxisEntry>`（轴名 → 提供插件 + vtable）。
  **同名轴多插件 = 装配 fail-fast**（与 loader「不静默跳过」哲学一致）。
- 新 op：`op_axis_call(name, op, args_json) -> serde_json::Value`：
  未知轴名 → 报错并列出可用泛型轴；调用经 `ffi::await_ffi` 异步完成。
- `bootstrap.js` 挂**通用代理**（一次性）：

  ```js
  // axis("cache").get("k") → op_axis_call("cache", "get", JSON.stringify(["k"]))
  globalThis.axis = (name) => new Proxy({}, {
    get: (_, op) => (...args) => op_axis_call(name, String(op), JSON.stringify(args)),
  });
  ```

  新轴 JS 面自动可用；不再有 per-axis 全局对象。bootstrap.js 保持 7-bit ASCII。
- 泛型轴注册表放 `StableState`（首次 run 前注入，与既有命名注册表同规矩）。

### 模板

`tools/plugin-template/`：骨架 cdylib——泛型 vtable + catch_future 包装 +
1~2 个示例 op + README 范式说明（轴名约定、panic 收敛、错误返回、JS 调用形态、
xtask 构建命令）。README 指向本文档与 plugin-development.md。

## Part 3 — 配置自报（充分自治：配置解析不再随轴增长）

### 问题

cfg 到插件的通路今天是宿主侧**按名硬编码**：`serve_cmd.rs` 的 `plugin_cfg(cfg, name,
es_profile)` 把插件名映射到 config 段（`auth:` / `es:` / `broker:` / `kafkas:` …），
新增带配置段的轴 = 主程序改 `plugin_cfg` + `Config` 加字段。顶层 `Config` 虽无
`deny_unknown_fields`（未知段解析不炸），但段值到不了插件手里。

### 设计

**FFI 新增（可选）静态符号**（须在 init 之前可探——init 就要吃 cfg，不能依赖
init 返回的 descriptor）：

```rust
#[unsafe(no_mangle)]
pub extern "C" fn oj_plugin_config_key() -> RString  // 例："cache"
```

由 `oj_plugin_entry!` 追加可选参数生成：
`oj_plugin_entry!(init, config: "cache", cache => &VT)`（不配 config: 则不导出符号，
行为同旧插件）。

**宿主 `Config`**：加 `#[serde(flatten)] extra: HashMap<String, serde_json::Value>`
catch-all——既有类型化字段优先，**未知顶层段落入 extra**（顶层字段全部显式声明，
不存在 SmtpSection 式「flatten 误收已知键」问题）。

**cfg 解析顺序（`cfg_for` 泛化，零按名硬编码）**，对加载中的插件：

1. `plugins:<name>` 值为非空对象 → 原样透传（既有语义，最高优先）；
2. 插件导出了 `oj_plugin_config_key` 且顶层存在该段 → extra 里取该段序列化为 JSON；
3. 否则回落宿主遗留按名映射（第一方旧插件的 es_profile 特判等）→ 最终 `"{}"`。

遗留映射只服务既有 10 个第一方插件，永不增长；**新轴/新插件加配置段 = 宿主零改动**
——插件声明 config key，用户写顶层段，装配期自动到达 init。
cfg 白名单校验归插件 init 自裁（未知键 fail-fast 在插件侧裁决，与 ldap
`LdapConfig::from_value` 哲学一致；宿主不为泛型轴做键校验）。

**未消费段诊断**：装配后 `extra` 中未被任何已加载插件（经 config key 或
`plugins:<name>`）消费的段，进 oj_info 的 `config.unconsumed_sections` 列表——
堵住「段名打错被静默忽略」的洞，且无需宿主认识任何段名。

### 测试

- cfg 三级解析顺序单测；config key 段到达 init 的端到端（mini 夹具）；
- 未消费段出现在 oj_info；`plugins:` 透传优先序不回归。

## Part 4 — oj_info

### 数据源

装配期生成单一事实源 `OjInfo`（serde Serialize），两出口共用：

| 段 | 内容 |
|---|---|
| `build` | oj 版本（CARGO_PKG_VERSION）、profile、host-triple（`ffi::triple()`）、deno_core 版本、V8 版本（`deno_core::v8`）、exe 路径、workspace_root、config 路径 |
| `abi` | `ABI_VERSION`、`HOST_FINGERPRINT` |
| `plugins` | 现有 `PluginInfo` 全字段（name/semver/abi/fingerprint/desc/host_abi）+ 每插件 `unknown_axes` 告警 |
| `backends` | `build_registries` 声明面：db 各库 scheme、redis profiles、blob 后端清单、broker kind、es endpoint、auth 守卫有/无、mq kafkas/rabbits、mail profiles、ldap 实例、**泛型轴清单** |
| `config` | 段名 → 键名清单；**值一律不出**（零泄漏面；ENC[...] 密文也不回显）；`unconsumed_sections`：未消费顶层段清单（Part 3 诊断） |
| `serve` | dev/release 模式判定、base、api_path（`oj info` 提供；JS 侧运行时相同） |

**只报告声明面，不真连库/连 broker**（php -i 亦不连数据库）；连接可用性不在 oj_info 职责内。

### 出口

- **CLI `oj info -c config.yaml`**：复用 serve_cmd 的装配管线——将「config 加载 +
  插件装配 + 注册表构建」从 `start()` 抽成可独立调用的纯装配函数（不监听端口），
  php -i 风格纯文本打印分段键值 + 插件表。
- **JS `globalThis.ojInfo()`**：装配产物序列化注入 runtime（随 StableState 或专用
  静态注入），bootstrap.js 挂载；返回上述 JSON 对象。文档注明：自行包 HTTP 端点时
  鉴权是部署者责任；框架不提供公共 oj_info 端点。

## 错误处理

- 自报清单与 vtable 指针为插件责任：指针非法 → 按现有 vtable 调用路径的 UB 边界
  处理（与今相同；repr(C) FFI 信任边界，模板文档明示「必须返回静态 vtable 地址」）。
- 泛型轴同名冲突 → 装配 fail-fast，文案列出冲突双方。
- `op_axis_call` 未知轴 → 错误消息列可用泛型轴名。

## 测试

- `plugin_loader`：自报优先 / 旧符号回退 / 未知轴收集 / 零轴空清单 / 泛型轴注册与冲突。
- `oj_plugin_entry!` 宏测试：生成符号存在性（沿用现有 paste 符号测试方式）。
- e2e：扫描模式加载带泛型轴的 mini 夹具插件，`axis("<名>").<op>()` 端到端调用。
- `oj info` CLI 冒烟（有插件/零插件两形态）；JS `ojInfo()` e2e 调用。

## 文档与版本红线

版本 bump 一个 minor（建议 v0.1.54），同步：

- `CHANGELOG.md`
- devkit 四件（api-manual 增 `axis()`/`ojInfo()` 章节与错误表；SKILL 陷阱速查；
  scenarios 照抄场景；README）——`cargo xtask build` 归置 + devkit 契约用例校验
- `docs/api/20-ojinfo.md`（或并入 05-plugins 篇后单列 axis 篇——实现时按 api 目录
  既有编号习惯定）
- `docs/plugins/plugin-architecture.md`：自报清单 + 泛型轴通道机制
- `docs/plugins/plugin-development.md`：新轴开发范式指向 tools/plugin-template

## 开放问题（实现时定夺，不阻塞）

- 泛型轴是否需要命名多实例（如 blob.backends 形态）：v1 不做，冲突即 fail-fast；
  需要时按 named_registry 既有模式扩展，ABI 不变。
- `oj info` 是否打印路由表统计：v1 不做（routes 构建属于 serve 启动面）。
