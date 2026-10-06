# oj-kv-redis

## 概述

提供 `kv` 轴的 cdylib 插件，底层 `redis` 0.27。迁自 core `RedisKV`：`set` / `get` / `del` / `expire` / `incr`，连接经 `ConnectionManager` 复用，连接时单次探活 fail-fast。

## 提供的后端轴

`kv`

## 配置

顶层 `redis:` 段（`HashMap<name, URL>`，键 = profile 名，供 `--redis <profile>` 选默认源）。每 profile 的值：

| 字段 | 说明 |
|---|---|
| `url` | redis URL（如 `redis://host:6379/1`），连接时探活 |

字段权威定义见 [`../modules/02-config.md`](../modules/02-config.md) 的 `redis` 段。

## 依赖与构建注意

- `redis` 0.27，纯 Rust（async `ConnectionManager`）。
- 连接 / 认证失败 → Err，装配 fail-fast（先单次探活，不让 `ConnectionManager` 的无限重试把启动挂死）。
- URL 脱敏：`redis://***@host`（密码来自 `ENC[]` 密封值，错误串经终端镜像落 `logs/`，必须掩码）。

## 状态

已随发行包发布（较早合入）。

## 备注

- 未声明 `redis.default` 时，宿主回落内置 `InMemoryKV`，不进插件。
- 跨线时长以秒计（宿主侧已向上取整，Redis `EXPIRE` 只认整秒）。
