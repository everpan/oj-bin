---
title: 历史与未收录文档
updated: 2026-09-08
---

# 历史与未收录文档

本站点只收录**描述当前实现**的文档。下面这些文档不进站点（内容已过时或属于过程记录），
但仍然保留在仓库里，需要时直接看源文件：

| 文档 | 为什么不进站点 | 看它的理由 |
|---|---|---|
| `docs/review-2026-09-06.md` | 全模块评审与整改记录（过程性） | 想知道某个已知债的来龙去脉；里面有文档债与处置表 |
| `docs/review-2026-09-02.md` | 同上，时间更早的一轮 | 与上一条互补 |
| `docs/route-params-design.md` | 早期设计稿，其中 release/build 段落已被 `oj build` 取代 | 只关心 `.route` 路径参数的设计动机 |
| `docs/plugin-architecture.md` | 只有 §0 描述现行实现，§1 起是未执行的历史方案 | 现行部分已并入[插件开发](../reference/plugin-development.md) |
| `docs/cli2.md` | 与[用户手册](../reference/user-manual/index.md)高度重复 | CLI 细节的另一种表述 |
| `docs/archive/` | 早期 CLI 预案、rust-core-runtime 方案与评审、插件系统交接快照 | 考古 |
| `docs/superpowers/` | 实施计划与 spec 草稿（量很大） | 想知道某个特性当时是怎么设计与评审的 |

> 站点里的[模块地图](../modules/index.md)与[开发指南](../reference/dev-guide/index.md)是
> 这些文档的「现行版结论」，一般先看它们就够。
