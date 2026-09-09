---
title: 06 · 数据层与迁移
updated: 2026-09-08
---

# 06 · 数据层与迁移

oj 的数据层是「**声明优先，迁移兜底**」：日常加表加列写在 `schema.yaml` 里自动收敛，
不安全变更（删列、改名、类型变更、数据回填）才写 SQL 迁移。

## 四个数据通道

| 通道 | 文件 | 什么时候跑 | 幂等要求 |
|---|---|---|---|
| 声明式结构 | `schema.yaml` | dev 启动 / `oj migrate` 时 reconcile | 框架保证 |
| 演进式 DDL | `migrations/{seq:04}__{desc}[.方言].sql` | 按账本顺序执行一次 | 你自己保证 |
| 参考数据 | `seed.sql` | **每次启动重放** | 必须幂等（`INSERT OR IGNORE`） |
| 测试数据 | `fixtures/` | 仅 `oj test` / `oj fixture` | 由测试流程清理 |

容易搞混的是后两个：`seed.sql` 每次启动都跑（所以必须幂等），`fixtures/` 只在测试时灌。

## schema.yaml 能写什么

```yaml
tables:
  account:
    pk: id                       # 或 pk: [tenant_id, id] 联合主键
    columns:
      id:   { type: integer, autoincrement: true }
      name: { type: text, null: false }
      role: { type: text, null: false }
    indexes:                     # 可选
      - { name: idx_role, columns: [role] }
```

列类型最小集：`integer | bigint | text | boolean | double | blob`。

## 哪些变更能自动收敛，哪些不能

| 变更 | 处理 |
|---|---|
| 加表 | ✅ 自动 CREATE |
| 加**可空**列 | ✅ 自动 ALTER |
| 加索引 | ✅ 自动 CREATE INDEX |
| 加 **NOT NULL** 列（无默认值） | ❌ **fail-fast** —— 框架不知道老数据填什么，打印迁移模板让你手写 |
| 疑似改名（删一列 + 加一列相似名） | ❌ **fail-fast** —— 怕你手滑丢数据，请显式写迁移 |
| 删列 / 改类型 / 数据回填 | ❌ 写 `migrations/` |

这个「安全才自动、不安全就停下」的策略是刻意的：**框架不猜你的数据意图**。

## 账本与迁移门禁

所有执行过的迁移记在 `_oj_migrations`（带 `module` 列区分模块）。启动行为由
`server.migrate_on_start` 决定：

| 值 | 行为 |
|---|---|
| `auto` | dev 默认：启动即应用迁移 |
| `verify` | **release 默认**：账本落后直接拒绝启动，先跑 `oj migrate` |
| `off` | 不管，自己负责 |

release 部署的正确顺序：

```bash
./bin/oj migrate -c config.yaml -d dist     # 先迁移
./bin/oj server -c config.yaml --api-path dist
```

## 对账：声明 vs 实库

```bash
./bin/oj schema diff -c config.yaml     # 有漂移 exit 1（CI 友好）
```

schema.yaml 是「期望」，实库是「现状」，漂移 = 有人绕过声明直接改了库。
把它放进 CI，能挡住绝大多数环境不一致类事故。

## 表归属：谁的表谁负责

双向一致检查（`oj build --check` 的 S005）：

- `manifest.yaml` 的 `tables:` 与 `schema.yaml` 里声明的表必须一致；
- 一张表只能属于一个模块（重名即拒）；
- SQL 里用到**别的模块**的表，必须在 `manifest.yaml` 的 `deps:` 声明，
  否则按 `server.ownership_guard` 处理（`warn` 告警 / `deny` 拒绝执行）。

## 多库与事务

config 里可以配多个 DSN，JS 里按名取用：

```yaml
db:
  default: "sqlite://db.sqlite"
  analytics: "mysql://user:pass@127.0.0.1:3306/app"
```

```ts
db.query("select ...", []);              // default
DB("analytics").query("select ...", []); // 命名库

await db.tx(async (tx) => {              // 事务：回调里用 tx.query / tx.exec
  await tx.exec("update account set role = ? where id = ?", ["admin", 1]);
});
```

事务有两条硬规矩：每请求**至多一个**活跃事务（嵌套直接报
`transaction already active`）；**必须 await** —— 漏 await 的话请求结束时会强制回滚并打 warn。

注意：mysql/postgres 的实现在**插件**里（`oj-db-mysql` / `oj-db-postgres`），
没装插件就只有 sqlite 与内存两种形态。

## 延伸

- [数据迁移](../reference/migration.md)（通道细则与命令）
- [07 · 模块数据层](../modules/07-data-layer.md)（实现视角）
- [sample 模块导览](../sample/modules-tour.md)（`_platform` 演示「只有表没有路由」的模块）
