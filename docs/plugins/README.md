# 插件状态文档索引

第一方 cdylib 插件的轴、配置与案例已并入 `docs/api/` 对应轴的「插件实现」节——
一份内容，不再逐字维护两份：

| 插件 | 并入 |
|---|---|
| oj-db-mysql / oj-db-postgres | [api/06-db.md](../api/06-db.md) |
| oj-kv-redis | [api/07-kv.md](../api/07-kv.md) |
| oj-blob-s3 | [api/08-blob.md](../api/08-blob.md) |
| oj-bus-kafka / oj-bus-rabbitmq | [api/09-bus.md](../api/09-bus.md)、[api/10-mq.md](../api/10-mq.md) |
| oj-es | [api/11-es.md](../api/11-es.md) |
| oj-mail | [api/12-mail.md](../api/12-mail.md) |
| oj-ldap | [api/13-ldap.md](../api/13-ldap.md) |

留在本目录的：

- [oj-auth.md](oj-auth.md) —— auth 轴插件（验签守卫）。auth 没有 handler 侧 API 专题页，
  状态文档保留在这里。
- [plugin-development.md](plugin-development.md) —— 系统级插件机制与开发手册（架构总览已并入其中）。

AXES 共 9 个：es / db / blob / bus / kv / auth / mq / mail / ldap（类型化）。v0.1.54 起另有
**泛型轴通道**（`generic(name)` 声明，JS `axis("name").op(...)`，零 ABI 变更）——当前
oj-ldap 已迁移为泛型轴；清单见 `oj info` / JS `ojInfo().generic_axes`。

系统级插件机制见 [plugin-development.md](plugin-development.md) 与
`docs/modules/05-ffi-and-plugins.md`。
