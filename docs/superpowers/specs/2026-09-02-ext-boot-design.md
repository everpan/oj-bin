# ext_boot.js：运行时创建期动态补充设计

日期：2026-09-02
状态：已批准（设计）

## 背景与动机

`bootstrap.js` 经 `deno_core::extension!` 宏在**编译期**嵌入二进制
（`src/bridge/mod.rs` 的 `esm = [dir "src/bridge", "bootstrap.js"]`），每个
`JsRuntime` 创建时作为扩展入口执行，装配 `json`/`db`/`http` 等全局。
代价：给全局对象增补辅助方法（如 `json.page()`、`log.trace`）必须重编二进制。

目标：运行时创建实例时，若存在 `<config_dir>/ext_boot.js` 则加载执行一次，
作为 bootstrap 的**动态补充**——不重编即可扩展全局。

## 决策记录

| 决策点 | 结论 | 理由 |
|---|---|---|
| 能力档位 | **ESM 模块**（支持 import / TLA） | 需 import 项目内模块与 `ext:core/ops` 原始 op |
| 文件位置 | 约定 `<config_dir>/ext_boot.js`，存在即加载 | dev/release 一致、`oj build` 零改动；与 config.yaml/plugins_dir 同级运维信任边界 |
| 执行点 | `RuntimePool::checkout` 改 async，boot 收进其中 | 唯一借出入口 = 不变量「checkout 出的 runtime 一定已 boot」单点成立 |
| 热重载 | **不做**，改文件需重启进程 | 池常驻；spec 装配期冻结，杜绝池内新旧混杂 |
| 配置键 | 不加（约定优于配置） | 需要自定义路径/多文件时再议 |

否决的替代方案：

- **同步工厂 + `futures::executor::block_on`**：diff 最小，但 boot 的 TLA 内
  做 IO（fetch/db）会死锁——隐蔽脚枪。
- **boot 放 `checkout_reset` + fresh 标记**：等价可行，但 `start_inspector`
  也直接 `checkout`，boot 调用点变两处；async checkout 单点更省。

## 数据流

```
装配期（App::from_config，一次性）:
  <config_dir>/ext_boot.js 存在？
    → versioned_specifier() 冻结 file://…?v=<mtime> → Extras.boot → StableState（Arc 不可变）

每个新 JsRuntime（RuntimePool::checkout 未命中空闲池时）:
  JsRuntime::new（扩展入口 bootstrap.js 先执行，全局已挂）
  → boot: code = `await import("<spec>");`
    load_side_es_module_from_code + mod_evaluate + run_event_loop
    （走 OjModuleLoader：.ts 缓存转译、相对/裸导入、CJS 互操作、ensure_within 全部复用）
  → Err → expect panic，文案含 ext_boot 路径与错误
  → 空闲池命中（复用）不重跑
```

- **`ext:core/ops` 导入**：bootstrap.js 自身能 import，预期 ext: 由 deno_core
  内部 module map 在自定义 loader 之前解析；若实测 file: 模块 import ext:
  撞上 `OjModuleLoader::resolve_inner` 的 "unsupported scheme"，则加 2 行放行
  `scheme == "ext"`（以测试定夺）。
- **信任边界**：ext_boot 在 config 旁 = 运维可控，可拿原始 op，与插件同权；
  项目模块（handler）不因此获得新能力。

## 错误处理与边界（ceiling 明示）

- **boot 失败 = 启动失败**：启动期 manifest 校验/路由内省必 checkout 首个
  runtime → 语法错/导入失败/TLA 抛错全部在 `App::from_config` 期 panic，
  服务拒绝启动。与 `expect("build reqwest client")` 同哲学（fail fast）。
- **改 ext_boot.js 需重启进程**（见决策表）。
- **boot 的 TLA 内避免无限等待**：此阶段 KillSwitch 未武装，挂起会卡启动。
  boot 只做全局装配，不做长等待。
- boot 执行于 `ReqState.reset` 之前：boot 期误调 `json.ok` 等写入的是随后即被
  重置的 per-request 状态，无泄漏。
- 无 ext_boot.js（现网默认）→ `Extras::default()` → 行为零变化。

## 改动面

- `src/bridge/runtime.rs`：`make` 变异步工厂；`checkout()` 变 async 并内联
  boot（~12 行：load side module + eval + event loop + expect）。
- `src/bridge/mod.rs`：`Extras` 增 `boot: Option<String>`（版本化 spec）；
  `checkout_reset`/`start_inspector` 两处 `.await` 适配。
- `oj/src/app.rs`：装配期探测 `<config_dir>/ext_boot.js` → 填 `Extras.boot`。
- 视测试结果：`src/bridge/module_loader.rs` 放行 `ext:` scheme（≤2 行）。

## 测试

1. Bridge 带 boot（`globalThis.foo = 1`）→ handler 读到 `foo`。
2. boot import 项目内模块（`./src/_shared/fmt.js` 风格）→ 全局可用。
3. boot 语法错 → `#[should_panic]` 构造 Bridge 即失败。
4. 无 boot → 全部既有测试零改动即回归覆盖。
5. （若放行 ext:）boot `import { op_db_has } from "ext:core/ops"` 可用。
