# API 文档索引

本目录按 `src/bridge/` 的 JS 全局对象逐一梳理 handler 侧 API：每篇含概述、配置、API 表、
错误、限制与场景化案例。系统级 bridge 机制见 [`../bridge.md`](../bridge.md)；
**权威 API 参考为 [`../devkit/api-manual.md`](../devkit/api-manual.md)**，本目录与其对齐，
如有出入以 api-manual 为准。

| # | 全局对象 | 文档 | 对应 bridge 源 |
|---|---|---|---|
| 01 | `json` / `finish` | [01-json.md](01-json.md) | `json.rs` / `envelope.rs` |
| 02 | `http` | [02-http.md](02-http.md) | `http.rs` |
| 03 | `log` | [03-log.md](03-log.md) | `log.rs` / `sql_trace.rs` |
| 04 | `vars` | [04-vars.md](04-vars.md) | `vars.rs` |
| 05 | `plugins()` | [05-plugins.md](05-plugins.md) | `plugins_op.rs` |
| 06 | `db` / `DB(name)` | [06-db.md](06-db.md) | `db.rs` / `query.rs` / `accessor_sqlx.rs` |
| 07 | `kv`（`redis` 别名） | [07-kv.md](07-kv.md) | `kv.rs` |
| 08 | `blob(name?)` | [08-blob.md](08-blob.md) | `blob.rs` |
| 09 | `bus` | [09-bus.md](09-bus.md) | `bus.rs` / `bus_backend.rs` / `broker/` |
| 10 | `Kafka/RabbitMQ`（mq 轴） | [10-mq.md](10-mq.md) | `mq.rs` / `broker/` |
| 11 | `es` | [11-es.md](11-es.md) | `es.rs` |
| 12 | `Mail` / `mail` | [12-mail.md](12-mail.md) | `mail.rs` |
| 13 | `axis("ldap")`（`ldap`/`LDAP` 遗留双轨） | [13-ldap.md](13-ldap.md) | `ldap.rs` |
| 14 | `ws` / `WebSocket` | [14-ws.md](14-ws.md) | `ws.rs` |
| 15 | `tasks` | [15-tasks.md](15-tasks.md) | `task_pool.rs` |
| 16 | `jwt` / `bcrypt` / `crypto` | [16-crypto.md](16-crypto.md) | `crypto.rs` |
| 17 | `cert` | [17-cert.md](17-cert.md) | `cert.rs` |
| 18 | `oidc` | [18-oidc.md](18-oidc.md) | `oidc.rs` |
| 19 | `fs` | [19-fs.md](19-fs.md) | `fs.rs` |
| 20 | `ojInfo()` / `oj info` | [20-ojinfo.md](20-ojinfo.md) | `oj/src/serve_cmd.rs`（`assemble_ojinfo`）/ `plugins_op.rs` |

其它内建全局（`fetch`、`URL`、`TextEncoder`、`toBigInt`/`toUBigInt`/`toDouble` 等）：
`fetch` 与 Web API 类由 deno_fetch/deno_web 扩展提供，见 api-manual 相应章节；
数值边界助手随 [06-db.md](06-db.md) 的数值一节说明（另见 [../numeric-limits.md](../numeric-limits.md)）。
