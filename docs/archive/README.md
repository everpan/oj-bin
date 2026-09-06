# 归档文档（docs/archive/）

本目录放**已过时但仍有复盘价值**的文档：早期方案、可行性分析、中途交接快照。
**它们不描述当前实现** —— 读之前先看下面这张「现在该读哪篇」。

## 现在该读哪篇

| 你想知道 | 读这个（不要读归档） |
|---|---|
| CLI 命令与 `config.yaml` | `docs/user-manual.md` |
| 模块职责、边界、文件地图 | `docs/modules/` |
| JS 全局 API（业务开发者视角） | `docs/devkit/api-manual.md` |
| 日常开发流程与内部实现走读 | `docs/dev-guide.md` |
| bridge 的 op 与状态模型 | `docs/modules/01-core-bridge.md` |
| 插件开发（第三方） | `docs/plugin-development.md` |
| 插件系统当前注册机制 | `docs/plugin-architecture.md` §0（该文其余章节是历史方案） |
| 证书/运维 | `docs/ops-manual.md` |

## 归档清单

| 文件 | 原主题 | 归档理由 |
|---|---|---|
| `cli.md` | 「Rust 版 CLI 实现预案（可行性分析 + 扩充版）」 | 早期「进程内 devserver」时代的 P0–P6 预案，所述架构已被 `oj` CLI 取代。现行命令见 `docs/user-manual.md`。（原文件头部已自带归档说明。） |
| `rust-core-runtime.md` | 原始方案：Rust + SeaORM + deno_core + deno_runtime | 规划的 `deno_runtime` + SeaORM 路线未被采纳；实际走 `deno_core` + 自定义 op + `sqlx/sea-query`。 |
| `rust-core-runtime-revised.md` | 对上一份的四专家评审与修订 | 修订结论（停止引入 deno_runtime/SeaORM）已生效并落地，作为决策记录保留。 |
| `plugin-system-handover.md` | 插件系统中途交接快照（2026-08-25，HEAD 849017e） | 时点快照，剩余工作早已完成；现行机制见 `docs/plugin-development.md`。 |

> 归档于 2026-09-06（`docs/review-2026-09-06.md` §5 文档债整改）。
> `docs/superpowers/` 下的 plans/specs 同样是过程记录，但按其自身组织保留，未并入此处。
