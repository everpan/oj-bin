---
title: 04 · 模块解剖
updated: 2026-09-08
---

# 04 · 模块解剖

一个 oj 模块 = `src/<模块名>/` 这个目录。它同时是**路由单元**、**数据归属单元**和
**发布单元**。这一节把目录里能放的东西一次讲完。

## 文件清单

| 文件 | 必填 | 作用 | 不写会怎样 |
|---|---|---|---|
| `api.ts` / `api.js` | 二选一 | 业务入口，导出与 HTTP 方法同名的函数 | 该目录不产生路由 |
| `manifest.yaml` | 是（有表时必填） | 模块身份 + `tables:`/`deps:` 归属声明 | `oj build --check` 报 S005 |
| `schema.yaml` | 否 | 声明式表结构 | 只能靠 `migrations/` 手写建表 |
| `migrations/*.sql` | 否 | DDL 演进（删列、改名、类型变更等不安全变更） | 只有「安全前向」能自动收敛 |
| `seed.sql` | 否 | 幂等参考数据，每次启动重放 | 无初始数据 |
| `fixtures/` | 否 | 仅 `oj test` / `oj fixture` 灌入 | 测试无数据 |
| `ws.ts` / `ws.js` | 否 | WebSocket 帧循环（文件名固定小写） | 该目录不开 WS 路由 |
| `_shared/` | 否 | 模块内共享代码，**不产生路由**（下划线开头） | — |
| `tasks/` | 否 | 长任务（常驻 JS 任务，由监督器托管） | 无后台任务 |

> 约定：`_` 开头的目录不参与路由映射；`api.ts` 与 `api.js` 同时存在时按 dev/release
> 与构建产物规则取用，**不要故意混放**。

## 路由的三种形态

| 形态 | 写法 | 例子 |
|---|---|---|
| 精确 | 默认（目录镜像） | `src/user/account/api.ts` → `/v1/api/user/account/` |
| 路径参数 | 给函数挂 `.route` | `detail.route = "{id}"` → `/v1/api/user/item/{id}` |
| 通配 | `{*path}` | `get.route = "{*path}"` → `/v1/api/file/a/b/c`（至少一段） |

挂了 `.route` 之后**原路径失效**（`/v1/api/user/item` 变 404），这不是 bug 是设计。

## 一个真实模块长什么样

以 `sample/src/user` 为例：

```
user/
├── manifest.yaml          name/tables: [account]
├── schema.yaml            account 表：id/name/role
├── seed.sql               INSERT OR IGNORE（幂等）
├── _shared/validate.ts    requireRole / positiveId（也被 order 模块 import）
├── account/api.ts         get/post/put/del/patch/head/options 全家桶
├── item/api.ts            detail.route = "{id}"
└── profile/detail/api.ts  三层目录 → 三层路由，仅 4 行
```

## 跨模块：代码和数据是两回事

这点最容易被忽略，单独讲：

- **代码复用**就是普通 ESM：`import { requireRole } from "../../user/_shared/validate"`。
- **数据（表）跨模块**必须在 `manifest.yaml` 里声明 `deps:`：

```yaml
name: "order"
deps:
  user: "^0.1.0"      # 我在 SQL 里 join 了 user 模块的 account 表
tables:
  - orders
```

不声明会怎样？取决于 `server.ownership_guard`：默认 `warn`（只告警），设成 `deny`
（sample 就是）则**直接拒绝执行**。这条守卫的目的是让「谁动了我的表」在构建期可见，
而不是等到线上才发现有人偷偷 join。

## ws.ts 与 tasks/

- `ws.ts` 放在模块目录下即产生 `{base}/<模块>/ws` 路由，写的是**帧循环**而不是
  「一次请求一次响应」，详见[08 · 实时与消息](./08-realtime.md)。
- `tasks/` 放长任务（比如消费 MQ、定时心跳），由监督器托管：命名扫描、退避重启、
  优雅停机。JS 里用 `tasks.stopping()` 感知停机信号、`tasks.sleep()` 让出。

## 发布单元

`oj build` 按模块产出：

```
dist/
├── user-0.1.0/          版本目录（api.ts → api.js，剥离 .route，import 重写成带版本路径）
├── user-0.1.0.tgz       可分发归档
├── manifests.yaml       模块锁（release 模式按它聚合）
├── routes.js            release 模式的唯一路由来源
└── tasks/               tasks/ 目录的转译镜像（非版本化资产）
```

模块版本来自 `manifest.yaml` 的 `version`；`dist/` 是产物，**可再生，不要手改**。

## 延伸

- [05 · 全局对象速查](./05-globals-tour.md)
- [06 · 数据层与迁移](./06-data-layer.md)
- 权威：[JS API 手册 · 项目结构与模块约定](../reference/api-manual/index.md)
