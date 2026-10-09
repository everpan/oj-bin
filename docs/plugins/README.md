# 插件状态文档索引

本目录是按插件拆分的状态文档：每个第一方 cdylib 插件一份，记录它提供的后端轴、配置段、依赖与构建注意、当前状态。系统级插件机制文档见 [`plugin-development.md`](plugin-development.md)（含架构总览与设计决策记录） / [`../modules/05-ffi-and-plugins.md`](../modules/05-ffi-and-plugins.md)。

AXES 共 9 个：es / db / blob / bus / kv / auth / mq / mail / ldap（类型化）。v0.1.54 起另有
**泛型轴通道**（`generic(name)` 声明，JS `axis("name").op(...)`，零 ABI 变更）——当前
oj-ldap 已迁移为泛型轴；清单见 `oj info` / JS `ojInfo().generic_axes`。

| 插件 | 后端轴 | 配置段 | 状态 | 文档 |
|---|---|---|---|---|
| [oj-auth](oj-auth.md) | auth | `auth:` | 已随发行包发布（较早合入） | [oj-auth.md](oj-auth.md) |
| [oj-blob-s3](oj-blob-s3.md) | blob | `blob:`（或 `blob.backends.<name>`） | 已随发行包发布（较早合入） | [oj-blob-s3.md](oj-blob-s3.md) |
| [oj-bus-kafka](oj-bus-kafka.md) | bus + mq | `broker:` | 已随发行包发布（较早合入） | [oj-bus-kafka.md](oj-bus-kafka.md) |
| [oj-bus-rabbitmq](oj-bus-rabbitmq.md) | bus + mq | `broker:` | 已随发行包发布（较早合入） | [oj-bus-rabbitmq.md](oj-bus-rabbitmq.md) |
| [oj-db-mysql](oj-db-mysql.md) | db | `db:`（认领 `mysql://`） | 已随发行包发布（较早合入） | [oj-db-mysql.md](oj-db-mysql.md) |
| [oj-db-postgres](oj-db-postgres.md) | db | `db:`（认领 `postgres://`） | 已随发行包发布（较早合入） | [oj-db-postgres.md](oj-db-postgres.md) |
| [oj-es](oj-es.md) | es | `es:` | 已随发行包发布（较早合入） | [oj-es.md](oj-es.md) |
| [oj-kv-redis](oj-kv-redis.md) | kv | `redis:` | 已随发行包发布（较早合入） | [oj-kv-redis.md](oj-kv-redis.md) |
| [oj-ldap](oj-ldap.md) | ldap（泛型轴，v0.1.54 迁移） | `ldap:` | 首版可用（v0.1.28 起）；随发行包发布 | [oj-ldap.md](oj-ldap.md) |
| [oj-mail](oj-mail.md) | mail | `smtp:`（或 `plugins.mail` 透传） | 首版可用（v0.1.19 起）；随发行包发布 | [oj-mail.md](oj-mail.md) |

> `fs` 轴（核心内置，非插件）的文档已移至 [`../api/19-fs.md`](../api/19-fs.md)。
