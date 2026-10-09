# 设计：插件轴清单自报 + 泛型轴通道 + oj_info（phpinfo 对应物）

日期：2026-10-09 · 状态：待评审

## 评审记录（2026-10-09，双专家独立评审，结论一致：修改后通过）

架构师 findings：H1（flatten 全字段语义位移→改两-pass，已采纳）、H2（config key
撞已知名静默 `{}` → 全量 Value 查找消解，已采纳）、M1（AxisDecl 表示定型 RVec，
已采纳）、M2（AXES 降级不删 + xtask 迁移，已采纳）、M3（宏双发写死，已采纳）、
M4（load_one 时序 + cfg_for 签名，已采纳）、L1/L2/L3/L4/L5/L6（均已采纳）。

工程师 findings：S1（static 清单编译不过 → RVec 调用期构造，与 M1 合流采纳）、
S2（cfg_for 签名穿透 4 调用点，已采纳）、S3（flatten 回归风险 → 两-pass，与 H1
合流采纳）、M4（手工表措辞修正，已采纳）、M5（mini-legacy 手写符号夹具，已采纳）、
M6（宏三形态匹配臂 + 轴名小写写死，已采纳）、M7（ojStringify / 保留名 /
FfiFuture，已采纳）。工作量粗估 7–9.5 人日。

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

新增 repr(C) 类型。**仅新增独立类型，不触碰任何既有 repr(C) 结构字段，
按仓库红线（「repr(C) 字段变更才 bump」）ABI 保持 11 不动**：

```rust
/// 一个轴声明：轴名 + 擦除后的 vtable 指针。
/// 注意：RString（stabby 堆类型）不可 const 构造——清单**不能**做成 static 数组
///（评审 M1：无法 const 初始化 + *const 字段使 AxisDecl 非 Sync），
/// 改为调用期构造、经 RVec 返回（先例：DataAccessorVtable.schemes
/// 返回 RVec<RString>，同 crate 既有形态）。
#[repr(C)]
pub struct AxisDecl {
    pub name: RString,                 // 轴名（小写；宿主匹配用）
    pub vtable: *const core::ffi::c_void,
}
```

`oj_plugin_entry!` 宏追加生成（宏展开时已知轴清单，零轴插件导空表）：

```rust
#[unsafe(no_mangle)]
pub extern "C" fn oj_plugin_axes() -> RVec<AxisDecl> {
    // 函数体内运行时构造（RString::from + 静态 vtable 取址）；
    // vtable 指针与既有 oj_plugin_axis_<name> 返回同一静态地址。
}
```

宏**双发**（评审 M3，写死）：过渡期同时导出 `oj_plugin_axes()` 与既有
`oj_plugin_axis_<name>` 符号——正向（旧插件→新宿主）走回退探测，反向
（新插件→旧宿主）靠 per-axis 符号保持兼容。回退路径退役时 per-axis 符号
随之删除。轴标识强制小写写死（paste `:lower` 兜底在清单路径不存在，
宏文档与模板 README 明示「轴标识必须小写」）。

宏匹配需覆盖三形态（评审 M6）：`(init)` / `(init, config: "k")` /
`(init, config: "k", kv => &VT, ...)`；带 config 的匹配臂须先于轴列表臂。

第一方 10 个插件只需重编译（宏自动产新符号），源码零改动。宏冒烟测试加
`oj_plugin_axes` 返回内容断言；零轴展开断言空表。
`tools/xtask` 的 `--check` 预检走 PluginLoader 自然兼容，但 `--check` 的
轴清单渲染改用自报清单（见下节评审 M2）。

### 宿主（`src/bridge/plugin_loader.rs`）

- `probe_axes` 重写：**优先** dlsym `oj_plugin_axes()` 取 RVec<AxisDecl>，
  逐条按名 match → cast 为对应 vtable 类型填入 `Registrations`。
  「按名转 typed vtable」的 9 臂映射不可消除（类型擦除还原必须按名，评审
  M4 修正 spec 原措辞）——真正消掉的只是「逐轴 dlsym 探测」。
- **符号缺失（旧插件）→ 回退**现有 AXES 逐轴 dlsym 探测，eprintln 一行
  deprecated 提示（含插件名）。旧插件兼容不断裂。
- **`AXES` 不删除，降级**（评审 M2/M4）：保留为回退探测与已知轴名匹配表，
  降为 crate 内部实现细节（`pub(crate)`）；现存消费者迁移——xtask
  `--check` 轴清单渲染改吃自报清单、`serve_cmd` 对账测试改经
  `Registrations::provides()`、loader 测试同理。回退路径退役时才删。
- 清单中的**未知轴名**：先尝试泛型轴解释（Part 2；仅非 9 个保留名，
  保留名撞名按 typed 解释——信任边界与今日插件伪造 axis 符号相同，
  模板 README 写死 9 个保留名），解释不了才收集进
  `LoadedPlugin.unknown_axes: Vec<String>` 并 eprintln 告警。
- `Registrations` / `provides()` 不变。
- 回退路径无法靠重编译的 mini* 夹具测试（宏一改夹具自动获得新符号，
  评审 M5）——新增**手写符号的 mini-legacy 夹具**（绕开宏，手写
  abi_version/init/axis_kv，不导出 oj_plugin_axes）钉死回退路径；
  另需一个双发符号且内容可区分的夹具钉死「自报优先」。

## Part 2 — 泛型轴通道（新轴 = 纯插件开发）

### 范式契约（模板规定）

```rust
/// 泛型轴 vtable：op 名 + JSON 参数 → JSON 结果。repr(C) 新类型，ABI 不变。
/// FfiFuture 复用 oj_plugin_ffi::FfiFuture（poll/take/free 三件套，与全部既有
/// vtable 同形；评审 L5——spec 草案的 RFuture 是笔误）。
#[repr(C)]
pub struct GenericVtable {
    pub call: extern "C" fn(op: RString, args: RString)
        -> FfiFuture<RResult<RString, RString>>,
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
- `bootstrap.js` 挂**通用代理**（一次性；评审 M7/L2 细节内建）：

  ```js
  // axis("cache").get("k") → op_axis_call("cache", "get", ojStringify(["k"]))
  globalThis.axis = (name) => new Proxy({}, {
    get: (_, op) => {
      // BigInt 安全序列化用既有 ojStringify（JSON.stringify 遇 BigInt 即抛）；
      // then/Symbol.* 属性返回 undefined，防误 await 代理本体得到怪行为。
      if (typeof op !== "string" || op === "then") return undefined;
      return (...args) => op_axis_call(name, op, ojStringify(args));
    },
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

**宿主 extra：两-pass，不用 serde flatten**（评审 H1/S3，双方一致裁定）。
serde flatten 会把**全部**字段改经 Content 缓冲二次反序列化，serde_yaml 0.9
的宽松行为随之丢失（活例：`vars: {PORT: 3000}` 裸数字标量按字面读成串的
行为会炸成 `invalid type: integer`；ldap `Option<serde_yaml::Value>` 段、
ENC 解密后的回经路径同险）。改用两-pass：

1. `load_from` 已先解析 `serde_yaml::Value`（ENC 解密也在 Value 层）——顺手
   把顶层 mapping 键集减去**宿主已知顶层键静态表**（config.rs 内一张表，
   与 Config 字段一一对应）得 extra（serde_json::Value）；
2. 从该 Value 反序列化 Config（既有路径零变更）。

已知键表加「字段 ↔ 键」对账单测防漂移（AXES 对账测试同款手法）。
零 derive 风险、零解析路径变更、ENC 天然覆盖。

**load_one 时序重排 + `cfg_for` 签名变更**（评审 S2/M4，spec 原稿漏写）：
config key 必须在 init **之前**探测（init 就要吃 cfg，不能依赖 init 返回的
descriptor），而当前 `load_one` 顺序是 abi → init → descriptor → probe_axes，
且 `cfg_for: &dyn Fn(&str) -> String` 只收插件名。改为：

```
dlopen → abi 门禁 → dlsym(oj_plugin_config_key)（可选）→ cfg_for(name, key)
→ init → descriptor → probe_axes
```

`cfg_for` 签名改 `Fn(&str, Option<&str>) -> String`，穿透 4 个调用点：
`load_manifest` / `load_scanned`（plugin_loader）、serve_cmd 的 cfg_for 闭包、
xtask --check 的 cfg_for。serve_cmd 既有 `plugin_cfg` fallback chain 测试族
（serve_cmd.rs:1016 起）是第一方 10 插件 cfg 投递的保险丝，全程保持绿。

**cfg 解析顺序（`cfg_for` 泛化，零按名硬编码）**，对加载中的插件：

1. `plugins:<name>` 值为非空对象 → 原样透传（既有语义，最高优先）；
2. 插件导出了 `oj_plugin_config_key` 且**顶层 Value 存在该键** → 取该段
   序列化为 JSON。注意查找范围是**全量顶层 Value（含宿主已知段）**——
   撞名宿主已知段（如第一方 kv 插件声明 `"redis"`）是合法场景，宿主
   消费不受影响（段是读不是占）；键不存在 → `"{}"`（段可选是既有语义）。
   这使评审 H2 的「声明已知名却静默拿 {}」洞直接消失，无需 fail-fast：
   真打错键名时插件拿 `{}` 且真实段出现在 unconsumed_sections，部署侧可见；
3. 否则回落宿主遗留按名映射（第一方旧插件的 es_profile 特判等）→ 最终 `"{}"`。

遗留映射只服务既有 10 个第一方插件，永不增长；**新轴/新插件加配置段 = 宿主
零改动**——插件声明 config key，用户写顶层段，装配期自动到达 init。
cfg 白名单校验归插件 init 自裁（未知键 fail-fast 在插件侧裁决，与 ldap
`LdapConfig::from_value` 哲学一致；宿主不为泛型轴做键校验）。

**未消费段诊断**：装配后顶层 Value 中未被任何已加载插件（经 config key 或
`plugins:<name>`）消费的段：eprintln 一行告警（评审 L3，不 fail-fast）+
进 oj_info 的 `config.unconsumed_sections`——堵住「段名打错被静默忽略」的洞，
且无需宿主认识任何段名。

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

**只报告声明面，不真连库/连 broker**（php -i 亦不连数据库）；连接可用性不在
oj_info 职责内。注意 `oj info` 复用装配管线 = **会执行插件 init 代码**（线程/
连接池等副作用与 serve 相同；评审 L1）——信任边界同 serve，文档注明。

### 出口

- **CLI `oj info -c config.yaml`**：装配切割点天然存在（评审 L8）：`assemble_plugins`
  已是独立 pub fn（只 init 不 connect）、`build_registries` 仅私有可见性阻隔——
  提可见性 + 组合成纯装配函数（不监听端口），php -i 风格纯文本打印分段键值
  + 插件表。
- **JS `globalThis.ojInfo()`**：装配产物序列化注入 runtime（随 StableState 或专用
  静态注入），bootstrap.js 挂载；返回上述 JSON 对象。文档注明：自行包 HTTP 端点时
  鉴权是部署者责任；框架不提供公共 oj_info 端点。

## 错误处理

- 自报清单与 vtable 指针为插件责任：指针非法 → 按现有 vtable 调用路径的 UB 边界
  处理（与今相同；repr(C) FFI 信任边界，模板文档明示「必须返回静态 vtable 地址」）。
- 泛型轴同名冲突 → 装配 fail-fast，文案列出冲突双方。
- `op_axis_call` 未知轴 → 错误消息列可用泛型轴名。

## 测试

- `plugin_loader`：自报优先 / 旧符号回退 / 未知轴收集 / 零轴空清单 / 泛型轴注册
  与冲突。**回退路径用 mini-legacy 手写符号夹具**（评审 M5：mini* 夹具经宏重编译
  自动获得新符号，回退无从触发；mini-legacy 绕开宏手写 abi_version/init/axis_kv，
  兼作「宏改动不破坏手写符号插件」的守门员）。
- `oj_plugin_entry!` 宏测试：生成符号存在性 + `oj_plugin_axes` 返回内容断言 +
  零轴空表 + 三形态匹配臂（纯 `(init)` / 带 config 零轴 / 带 config 多轴）。
- e2e：扫描模式加载带泛型轴的 mini 夹具插件，`axis("<名>").<op>()` 端到端调用；
  **三个 JsRuntime 入口（HTTP 池 / tasks / `oj test`）均有 `axis()`/`ojInfo()`
  可用的覆盖**（评审 L4，呼应「新增 runtime 入口必须打补丁函数」教训）。
- cfg 三级解析顺序单测（plugins: 透传优先序不回归；serve_cmd plugin_cfg 既有
  fallback chain 测试族保持绿）；config key 段到达 init 端到端；两-pass extra 与
  已知键对账单测。
- 未消费段 eprintln + 出现在 oj_info。
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

## 已知限制（评审确认后明示）

- 泛型轴插件在**老宿主**（无 Part 1/2 代码的版本）上零信号：probe 不到任何
  类型化轴 = 装上但没消费且无告警（ABI 严格相等门禁决定的边界）。模板 README
  提示插件作者在 `desc` 注明所需宿主最低版本（评审 L6）。

## 开放问题（实现时定夺，不阻塞）

- 泛型轴是否需要命名多实例（如 blob.backends 形态）：v1 不做，冲突即 fail-fast；
  需要时按 named_registry 既有模式扩展，ABI 不变。
- `oj info` 是否打印路由表统计：v1 不做（routes 构建属于 serve 启动面）。
- 回退路径（逐轴 dlsym + per-axis 符号双发）的退役时点：待生态插件普遍重编译
  后另立版本决定，本版只标记 deprecated。
