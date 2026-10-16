# 方案：模块路由分发注入只读 `http.module`（name + version）

- 日期：2026-10-16（v2：并入四路评审处置，见 §10）
- 状态：待用户批准后实施
- 目标版本：v0.1.76（未打标签）

## 1. 需求

框架在**模块路由分发**时向 handler 注入 `http.module`，包含：

- `http.module.name`：本请求命中的模块名（如 `"user"`）；
- `http.module.version`：该模块的版本（dev = `src/<m>/manifest.yaml` 的 version；release = `dist/<m>-<v>/` 内 manifest 的 version）；
- **只读**：handler 不得改写模块身份（框架侧归属守卫 / bound_db / 平台特权换装以此为据，JS 侧改写必须无效且可预期）；
- 无模块上下文时为 `null`：**判定依据是 handler 文件是否位于模块目录内**（祖先目录命中 `StableState.modules`），而非请求来源。已知为 null 的路径：任务桥（`run_task`）、WS（`run_ws`/`ws_connect`/`task_connect` + 帧池 `frame_pool.rs:425` 绕过 `checkout_reset` 直调 `reset`）、`run_with`、启动期内省/预热。**页面 meta 合成派发**（`serve/src/lib.rs:1072-1081`）的 handler 文件若在模块目录内则**有值**（继承所属模块身份，语义正确）。

业务动机：区分请求落在哪个模块/哪个版本（灰度观测、按模块分流日志、多版本并存时的兼容分支）。

## 2. 现状考古（关键结论）

| 事实 | 位置 |
|---|---|
| `ReqState.module: Option<String>`（模块名）已按目录命中注入，但**未出到 JS** | `src/bridge/mod.rs:322-323`，注入点 `:881` |
| 模块判定：api 文件祖先目录命中 `StableState.modules`（键 = 模块目录绝对路径） | `src/bridge/mod.rs:1315-1323` |
| `ModuleCtx { name, deps, db }` 无 version；装配期 `manifest::parse_one` 已读全 manifest（含 version） | `src/bridge/mod.rs:121-127`；`oj/src/app.rs:451-458` |
| 版本运行期只隐含在 `dist/<module>-<version>/` 路径里，无显式通道 | `oj/src/app.rs:1554-1579` |
| `http.user` 先例：RequestInfo → ReqState → `op_http_info` 快照 → bootstrap Proxy 普通属性自动透出 | `src/bridge/http.rs:67-94`；`src/bridge/bootstrap.js:462-476` |
| 每个模块目录强制 `manifest.yaml`（缺即启动失败），dev/release 双模式都有 version | `oj/src/manifest.rs:98-101` |
| `oj test` 运行时不挂 `http`（test_bootstrap.js 只挂 client/describe/it）；bootstrap.js 是唯一挂载点 | `oj/src/test_ext/test_bootstrap.js` |
| serve 层 `lookup` 不返回模块名/版本（只有文件绝对路径）——**不需要改 serve**，派生全在 bridge | `serve/src/routes.rs:514-537` |
| release 装配已校验 manifest name == 锁键，但**未校验 version == 目录后缀**（本方案补） | `oj/src/app.rs:1562-1569` |

## 3. 设计（serve 层零改动）

### 3.1 `ModuleCtx` 加 `version: String`（`src/bridge/mod.rs:121-127`）

```rust
pub struct ModuleCtx {
    pub name: String,
    /// manifest version：dev = src/<m>/manifest.yaml；release = dist/<m>-<v>/manifest.yaml。
    /// http.module.version 的数据源（装配期冻结，运行期不可变）。
    /// JS 可见面只经 `op_http_info` 显式投影——禁对本结构 derive Serialize（防 DTO 腐化）。
    pub version: String,
    pub deps: Arc<HashSet<String>>,
    pub db: Option<String>,
}
```

构造点共 **12 处**（生产 1 + 测试 11，已逐处确认在 `mod tests` 内）：

- 生产：`oj/src/app.rs:451-458`（`build_schema_and_modules`，`mf.version.clone()`）
- 测试：`src/bridge/mod.rs:3344/3458/3466/3516/3559/4196`、`src/bridge/exec.rs:824`、`src/bridge/reload_bridge.rs:202`、`src/bridge/fs.rs:420/477`、`src/bridge/yaml.rs:495`
- `platform.rs:233` 的 ReqState 字面量带 `..Default::default()`，无需动

### 3.2 模块判定处一并取 version（`src/bridge/mod.rs:1315-1323`）

```rust
let mut module: Option<&ModuleCtx> = None;   // 原 Option<&str>
while let Some(d) = anc {
    if let Some(ctx) = stable.modules.get(&d.to_string_lossy().into_owned()) {
        module = Some(ctx);
        break;
    }
    anc = d.parent();
}
```

### 3.3 传递链换型：`Option<&str>` → `Option<&ModuleCtx>`

- `run_module_validated` → `run_side_driver(req, code, timeout, module: Option<&ModuleCtx>)`（`mod.rs:1324`）
- `run_side_driver` → `checkout_reset(req, module)`（`mod.rs:1492`）
- `checkout_reset` / `checkout_armed`（`mod.rs:868-913`）：内部按 ctx 填 ReqState + 换装 fs 容器
- **`container_for_module` 适配**（评审补）：`mod.rs:891-895` 现收 `Option<&str>`，改为 `module.map(|c| c.name.as_str())`——该路径装配 fs 特权容器（安全面，漏了编译即败）
- 传 `None` 的调用点共 8 处（全部保持现状）：`run_task`（`:601`）、`run_with`（`:846`）、`run_ws`（`:1022`）、`ws_connect`（`:1089`）、`task_connect`（`:1163`）、boot 预热（`:921`）、introspect（`:1217/1444/1463`）

生命周期核过：`&ModuleCtx` 借自 `stable()` 返回的 `&Arc<StableState>`（`runtime.rs:105`），modules 装配后不可变，future 本就 `!Send`，与现 `Option<&str>` 同型，跨 await 安全。

### 3.4 `ReqState` 加 `module_version: Option<String>`（`src/bridge/mod.rs:322` 旁）

```rust
/// 本请求所属模块版本（与 module 同点注入；None = 无上下文）。
pub module_version: Option<String>,
```

`reset()`（`:366` 旁）清空；注入点 `:881`（唯一写点，`debug_assert!` 两者同 Some/None）：

```rust
rs.module = module.map(|c| c.name.clone());
rs.module_version = module.map(|c| c.version.clone());
debug_assert_eq!(rs.module.is_some(), rs.module_version.is_some());
```

### 3.5 `op_http_info` 快照加 `module` 键（`src/bridge/http.rs:71-93`）

```rust
"module": match (&s.module, &s.module_version) {
    (Some(n), Some(v)) => json!({ "name": n, "version": v }),
    _ => Value::Null,   // 无上下文；或（框架缺陷态）任缺其一——整体 null 比半对象诚实
},
```

成对匹配：name/version 恒同生同灭，**不存在「有 name 无 version」的对外契约**。

### 3.6 bootstrap.js 只读冻结（`src/bridge/bootstrap.js:462-476`）

Proxy get 陷阱加一支：

```js
if (p === "module") { const m = httpInfo().module; return m === null ? null : Object.freeze(m); }
```

- memo 对象 per-epoch 重建，freeze 幂等；
- `__ojRun` driver 是 `"use strict"` prologue 的 AsyncFunction（`bootstrap.js:1006-1008`），api 模块 ESM 恒 strict——写/delete/defineProperty 冻结对象均抛 TypeError；
- CJS 互操作走 `new Function`（`bootstrap.js:1025`）是 sloppy——写静默无效（探针用例分别钉死）；
- 深层无需（值皆为字符串）。

### 3.7 类型声明 `sample/global.d.ts`（devkit 唯一 d.ts 源，xtask 拷贝进 `bin/devkit/`）

```ts
// 本请求命中的模块身份（路由分发时注入；无模块上下文为 null）。只读（已冻结）。
module: { name: string; version: string } | null;
```

`version` 收窄为 `string`（Rust 内部 Option 保留，对外不承诺半对象——死分支不外溢给业务转嫁 null-check 税）。

### 3.8 release version 一致性门禁（评审补，`oj/src/app.rs:1562-1569` 旁）

装配期已校验 `m.name != *module` → fail-fast；补一行同款校验 `m.version` != 目录后缀 `<m>-<v>` → fail-fast。理由：错位/手改的 dist manifest 会让 `http.module.version` 与真实加载版本**静默不符**——正是本特性要防的误判源。

## 4. 安全论证

1. **框架授信面不受 JS 影响**：归属守卫 / bound_db / 平台特权换装读的是 Rust 侧 `ReqState.module`（String），JS 拿到的是 `op_http_info` 快照的**深拷贝**（`json!` 宏逐字段 `to_value` 深拷贝 + serde_v8 生成全新 V8 对象，与 ReqState 零共享）——改拷贝不影响 Rust 状态，冻结只是让「看起来能改」这件事不出错。
2. **只读可预期**：freeze 后写属性在 strict ESM 下抛 TypeError，handler 立刻可见，不会静默丢写；CJS sloppy 路径静默无效（同样不改框架状态）。
3. **信息泄漏面**：`http.module` 只暴露**本请求自身所属**模块的 name/version，不存在跨模块版本可见性；同进程所有 handler 本就在同一信任域（deps 守卫只护表，不护 manifest 元信息）；与启动日志已打印 version 相比不扩大授信面，但版本号可能被 handler 回显给外部客户端形成指纹——devkit 陷阱条目警示「勿回显 version」。
4. **边界声明**：改 manifest.yaml / api 文件经 fs 轴 + `do_reload` 重解析（`serve_cmd.rs:409`）影响**未来**派发，是既有面——本方案不扩大也未修复；freeze 只覆盖 JS 对象路径，勿误以为身份判定整体不可变。

## 5. 测试计划

### 5.1 Rust 单测（`src/bridge/mod.rs` 测试区）

- 构造带 module+version 的 ReqState → `op_http_info` 快照含 `module: {name, version}`；
- module=None → 快照 `module: null`；
- `reset()` 后 module/module_version 归 None。

### 5.2 e2e（`oj/tests/e2e.rs`）

- sample 加演示端点（见 §6）回显 `http.module`；
- dev 模式（`boot(true)`）断言 `{name:"user", version:"0.1.0"}`（以 manifest 实际值为准）；
- release 模式：uc6 同款 `boot(false)`（现场 `oj build` 到根 `.e2e-dist`，**不读 sample/dist**——已 gitignore）断言同值（dev/release 双 boot 同版本，评审建议）；
- 挂载根级直置 api（无模块目录，tmp_project 夹具）→ `http.module === null`；
- 页面 meta 合成派发路径：meta handler 指向模块目录内 api → `http.module` 有值（评审补，钉死「合成派发继承所属模块身份」）。

### 5.3 对抗性探测（feedback「新功能要配对抗性探测用例」）

- handler 内 `http.module.name = "evil"` → 期望 TypeError 抛出（strict + freeze）；
- `delete http.module.version` / `Object.defineProperty(http.module, "name", ...)` → 期望 TypeError；
- `http.module = { name: "evil" }` 赋值后读回仍是 `{name, version}` 原值 + `Object.isFrozen(http.module) === true`（Proxy 无 set 陷阱的惰性无效，钉死）；
- CJS 探针：CJS handler 内写 `http.module.name = "x"` → 静默无效不抛，读回不变（sloppy 路径契约）；
- 连续两次访问 `http.module` → 同一 epoch 内恒等（`===`）。

## 6. sample 演示（一步到位）

- `sample/src/user/module-info/api.ts`：`get()` 回显 `json.ok({ module: http.module })`（注释示范只读语义）；
- release 产物 `sample/dist/user-*/module-info/`：由使用者按需 `oj build` 再生（dist 可再生，不手改；本仓提交内无需再生——e2e 的 release 走 `.e2e-dist` 现场构建）；
- 同步 `sample/global.d.ts`（§3.7）。

## 7. 文档与版本（一步到位，devkit 四件全）

- `docs/devkit/api-manual.md`：http 章节加 `http.module` 小节（语义 + null 场景 + 只读）；**错误/限制表**加「写冻结 module 抛 TypeError」条目；只读处补一句「module 是框架授信依据故冻结，user 维持现状」；
- `docs/devkit/SKILL.md`：陷阱速查加三条——①任务/WS（含**模块目录内的 ws.ts**）/内省期 `http.module` 为 null；②对象已冻结，写抛 TypeError（strict）/静默无效（CJS）；③勿回显 `version` 给外部客户端（指纹）；
- `docs/devkit/scenarios.md`：加「按模块版本观测打点 + 兼容分支」可照抄场景（**非 AB 分流**——版本分流在锁/挂载层）；
- `docs/devkit/README.md`：per-version「已落地部分」段补 v0.1.76 条目（红线第四件，勿漏）；
- `CHANGELOG.md`：新增小节，条目预拟——
  - `http.module`：模块路由分发注入只读 `{name, version}`（无模块上下文为 null；dev 版本取 `src/<m>/manifest.yaml`，release 取 `dist/<m>-<v>/manifest.yaml`）；
  - 只读语义：对象已冻结，strict 下写抛 TypeError；
  - release 装配新增 manifest version 与目录后缀一致性 fail-fast；
- `oj/Cargo.toml` + `Cargo.lock`：version 0.1.75 → 0.1.76；
- `cargo xtask build` 归置 `bin/`（含 `bin/devkit/`），`cargo test --release -p xtask` 契约用例自校验。

## 8. 不做清单（scope control）

- 不做 `http.module` 的可写变体 / setter（测试伪造身份走 e2e 真实路由，文档明说）；
- 不改 serve 层 RouteTable/lookup 返回类型（bridge 派生已足够，避免动热路径；多挂载已按目录键支持）；
- 不整包序列化 `ModuleCtx`（deps/db 属特权上下文，且诱发 DTO 腐化——doc 已钉禁 derive Serialize）；
- 不动 `http.user` 的冻结策略（本方案只冻结 module，user 维持现状——api-manual 注明理由）；
- WS/任务桥不补模块上下文（帧池绕过 `checkout_reset` 直调 `reset` 是既有实现，null 为显式语义）；
- 防 stale dist（用户改了 src manifest 不 rebuild）不属框架职责；只防「manifest 与目录后缀不符」（§3.8）；
- 不在 URL 上暴露版本（`http.module` 只是运行期注入，路由形态不变）。

## 9. 实施顺序

1. worktree `.worktrees/feat-http-module-ctx` + 分支 `feat/http-module-ctx`，软链共享主仓 `target/`；
2. Rust 侧（§3.1–3.5、§3.8）→ `cargo fmt` + `cargo clippy --release --workspace --all-targets -- -D warnings`；
3. 单测 + e2e + 对抗性用例 → `cargo test --release -p oj` + `cargo test --release -p only-js` + `cargo test --release -p oj --test e2e`；
4. sample 演示端点 + global.d.ts；
5. devkit 四件 + CHANGELOG + 版本 bump + `cargo xtask build` + `cargo test --release -p xtask`；
6. 全量门禁：`cargo test --release --workspace`。

## 10. 四路评审处置表（v2 并入）

| # | 来源 | 意见 | 处置 |
|---|---|---|---|
| 1 | 工程 | 漏 `container_for_module` 适配（`mod.rs:891-895`，安全面） | 采纳 → §3.3 |
| 2 | 工程 | None 调用点漏列 3 处 + `:601` 实为 `run_task` | 采纳 → §3.3 列全 8 处 |
| 3 | 工程 | 构造点计数错（实为 12 处） | 采纳 → §3.1 |
| 4 | 工程+架构 | 「系统合成派发=null」不恒成立（meta handler 可在模块目录内） | 采纳 → §1 改按「文件是否在模块目录内」判定；e2e 补覆盖 |
| 5 | 架构 | release 缺 version 与目录后缀一致性门禁 | 采纳 → §3.8 一行 fail-fast |
| 6 | 架构 | WS null 理由不成立（帧池绕过 checkout_reset） | 采纳 → §1/§8 改表述 |
| 7 | 架构+产品+安全 | d.ts `version: string\|null` 死分支外溢 | 采纳 → §3.5 成对匹配、§3.7 收窄 `string` |
| 8 | 产品 | devkit 第四件 README 漏列（红线） | 采纳 → §7 补 README per-version 条目 |
| 9 | 工程+产品 | TypeError 报错须进 api-manual 错误/限制表 | 采纳 → §7 |
| 10 | 产品 | SKILL 陷阱点名「模块目录内 ws.ts 仍为 null」 | 采纳 → §7 |
| 11 | 产品 | scenario 定位纠偏（观测打点+兼容分支，非 AB） | 采纳 → §7 |
| 12 | 安全 | 对抗用例漏「赋值读回不变 + isFrozen」 | 采纳 → §5.3 |
| 13 | 安全 | TypeError 预期只覆盖 strict，CJS sloppy 无探针 | 采纳 → §5.3 补 CJS 探针 |
| 14 | 安全 | §4.3 泄漏论据不实（运维面≠JS 面） | 采纳 → §4.3 改写为「自身模块+同信任域」+ 指纹警示 |
| 15 | 安全 | 改 manifest/api 文件的既有面须声明 | 采纳 → §4.4 边界声明 |
| 16 | 产品 | api-manual 注明「module 冻结而 user 不冻」的理由 | 采纳 → §7 |
| 17 | 安全 | devkit 加「勿回显 version」指纹陷阱 | 采纳 → §7 |
| 18 | 产品 | CHANGELOG 条目预拟 | 采纳 → §7 拟三点 |
| 19 | 工程 | `debug_assert!` 同生同灭 | 采纳 → §3.4 |
| 20 | 架构 | ModuleCtx doc 钉「禁 derive Serialize」 | 采纳 → §3.1 |
| 21 | 架构 | e2e 加 dev/release 双 boot 同版本断言 | 采纳 → §5.2 |
| 22 | 架构 | SKILL 写「内省期 http.module 为 null」 | 采纳 → §7 |
| 23 | 工程 | 补 `-p oj` 全量单测门禁 | 采纳 → §9 |
| 24 | 架构 | stale dist 属用户错误，框架不防 | 同意（与 #5 不冲突：#5 防 manifest 与目录后缀不符，非防 stale）→ §8 |

无驳回项。
