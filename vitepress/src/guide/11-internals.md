---
title: 11 · 内部实现走读
updated: 2026-09-08
---

# 11 · 内部实现走读

到这一步你已经能用 oj 写业务了。这一节给的是**改 oj 本身**所需的最小地图。

## 分层与依赖方向（严格单向，无环）

```
        oj/  (CLI 编排：装配 + 构建 + 测试运行器)
         │
    ┌────┴─────┬──────────────┐
    ▼          ▼              ▼
 server/    src/          oj-plugin-ffi/  ◄── 被 oj/ 与 plugins/* 同时依赖
 (axum)   (only-js 核心)      ▲
    │          │              │
    └──────────┴── plugins/* ─┘   （插件只依赖契约，不依赖核心/服务）
```

三条硬约束：

- **插件不依赖核心**：`plugins/*` 只用 `oj_plugin_ffi::*`，核心通过 `dlopen` + vtable 反向调用。
  这保证「不装的能力不进二进制、不进依赖树」。
- **`src/` 不依赖 `server/` 或 `oj/`**：核心只暴露 `pub mod bridge` 与 `pub mod config`，
  装配决策（连哪个库、挂不挂鉴权）全在 `oj/src/app.rs`。
- **`server/` 依赖 `src/`**，不反过来。

## 状态模型（最重要的一条约定）

| 状态 | 位置 | 生命周期 | 谁写 |
|---|---|---|---|
| `StableState` | `Arc`，注入 `OpState` | **首次 runtime 取出后不可变** | 装配期一次性构造 |
| `ReqState` | `OpState` | 每请求，取出时 `reset()` | bridge 自身 |

推论（踩过的坑）：命名 DB / KV / blob / 插件注册表**必须在首次 run 之前注入**，
之后 `Arc` 已共享，`Arc::get_mut` 会直接 panic。

## 七条红线（改动前必读）

1. **SQL 注入**：动态标识符只来自 `SchemaRegistry`；值只走绑定参数。
2. **`JsRuntime` 是 `!Send`**：池与持有者必须同线程（server 侧用 `JsActor` 专用线程 +
   channel，inspector/WS 用 `spawn_local`）。
3. **`panic = "unwind"`**：所有插件 profile 必须保持。`oj_plugin_entry!` 用 `catch_unwind`
   收敛跨边界 panic；改成 `abort` 会让插件 panic 直接打挂宿主。
4. **`bootstrap.js` 必须 7-bit ASCII**（非 ASCII 会触发 deno_core 报错）。
5. **失败的 runtime 丢弃，不归还池**（未轮询完 event loop 就析构 isolate 有 SIGSEGV 前科）。
6. **证书强制校验**，无 config/CLI 逃生口。
7. **禁止 debug 构建**：脚本/CI 一律 `--release`。

## 插件是怎么加载的

1. 四级路径找到 `bin/plugins/<triple>/` 下的 cdylib；
2. 校验 `ABI_VERSION` **严格相等**；
3. 调 `oj_plugin_init` 拿 `PluginDescriptor`（名字、semver、描述）；
4. 按轴 `dlsym("oj_plugin_axis_<轴>")` 逐轴探测 —— **缺符号 = 不提供该轴**，
   所以「加新轴」对既有插件零破坏；
5. vtable 包进核心后端，冲突或缺失必需插件时 fail-fast。

插件自己要收敛 panic：`oj_plugin_entry!` 只保护 `init`，vtable 方法要插件自己用
`catch_value` / `catch_future` 包。详见[插件开发](../reference/plugin-development.md)。

## 从哪一页开始读

| 你想动 | 先读 |
|---|---|
| 全局对象 / op | [01 · 核心运行时](../modules/01-core-bridge.md) |
| config.yaml 字段 | [02 · 配置模型](../modules/02-config.md) |
| 路由 / 鉴权管线 / WS | [03 · HTTP 服务](../modules/03-server-http.md) |
| CLI 与装配 | [04 · CLI 与装配](../modules/04-oj-cli.md) |
| FFI 与插件 | [05 · FFI 与插件](../modules/05-ffi-and-plugins.md) |
| 迁移 / schema / 检查 | [07 · 模块数据层](../modules/07-data-layer.md) |
| 测试分层 | [08 · 测试体系](../modules/08-testing.md) |
| 全貌 | [模块地图（索引）](../modules/index.md)、[00 · 总览](../modules/00-overview.md) |

## 延伸

- [开发指南](../reference/dev-guide/index.md)（日常开发流程 + 内部走读，704 行，已按章分页）
- [bridge 与全局对象](../reference/bridge.md)
- [基准测试](../reference/benchmarks.md)
