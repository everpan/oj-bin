---
title: 术语表
updated: 2026-09-08
---

# 术语表

新人卡住的地方往往不是概念难，而是同一个词在不同文档里指的东西不一样。这张表把 oj 里
高频出现的词一次说清。

## 项目与形态

| 词 | 含义 |
|---|---|
| **oj / only-js** | 项目代号。`only-js` 是核心库 crate 名，`oj` 是 CLI 命令与入口 crate 名 |
| **handler** | 业务入口函数。`api.ts` 里导出的 `get`/`post`/… 就是 handler，一个文件可导出多个 |
| **模块（module）** | `src/<module>/` 这样一个目录：自带 `api.ts`（或多层）、`schema.yaml`、`migrations/`、`seed.sql`、`manifest.yaml` |
| **manifest** | 模块的自我声明文件（模块名、版本、`deps:` 依赖哪些模块的表） |
| **envelope / 信封** | 统一响应结构 `{code, msg, data}`。`json.ok/fail` 产出它，`json.raw` 可绕过出裸 JSON |

## 运行时

| 词 | 含义 |
|---|---|
| **bridge** | Rust 与 JS 之间的桥：注册 op、装配全局对象、管理 runtime 池 |
| **op** | 一个 Rust 函数暴露给 JS 的入口（deno_core 的 `op2`）。JS 里 `db.query()` 背后就是一个 op |
| **全局对象** | 注入到 JS 的 `json`/`http`/`db`/`kv`/`blob`/`bus`/`es`/`log`/`fetch`/`ws`/`jwt`/`crypto` 等 |
| **`StableState`** | 进程级稳定状态（连接池、注册表、插件）。首次 runtime 取出后就不可再改 |
| **`ReqState`** | 每请求状态（请求体、租户、身份）。每次执行前 `reset()` |
| **看门狗 / KillSwitch** | 超时强杀机制：handler 跑太久 → `terminate_execution` → 返回 408 |
| **`bootstrap.js`** | bridge 的 ESM 入口，负责把 op 挂成全局对象。**必须保持 7-bit ASCII** |

## 数据与插件

| 词 | 含义 |
|---|---|
| **`SchemaRegistry`** | 表/列白名单。动态 SQL 标识符的唯一合法来源，也是防注入的核心 |
| **表归属守卫** | 检查裸 SQL 里出现的他模块表是否在 `deps:` 声明过（默认 warn，可 `deny`） |
| **轴（axis）** | 一类可插拔后端能力：db / kv / blob / bus / es / auth / mq |
| **ABI** | 插件与宿主的 C-ABI 契约版本，**严格相等**才能加载；按轴 dlsym 探测插件提供了哪些轴 |
| **vtable** | 某个轴的函数指针表，插件通过 `oj_plugin_entry!` 导出 |
| **`bin/plugins/<triple>/`** | 插件 cdylib 的落盘位置，`oj` 启动时默认在这里发现插件 |

## 模式与命令

| 词 | 含义 |
|---|---|
| **dev 模式** | 服务 `src/`，按需转译 TS、热重载，启动可自动迁移 |
| **release 模式** | 服务 `dist/`（预构建 JS），按 `manifests.yaml` 锁聚合；`migrate_on_start: verify` 时账本落后拒启 |
| **L0–L3 测试** | L0 Rust 单测、L1 `oj test`（进程内真实运行时）、L2 vitest 纯 mock、L3 e2e 起真服务 |
| **账本** | 记录已执行迁移的表 `_oj_migrations`（带 `module` 列区分模块） |
| **seed / fixtures** | `seed.sql` 是幂等参考数据（启动重放）；`fixtures/` 只在 `oj test`/`oj fixture` 时灌入 |

想看这些词在架构里的位置，接着读[模块地图](../modules/index.md)。
