# oj-bus-rabbitmq

## 概述

提供 `bus` 轴与 `mq` 轴的 cdylib 插件（双轴）。底层 `lapin`，按 `broker:` 段装配。bus 面做 push 扇出（收到即 ack）；mq 面做 pull（`basic_get` + 手动 ack/nack，at-least-once）。两面对接同一份 `RabbitCore` driver。

## 提供的后端轴

`bus` + `mq`（双轴）

## 配置

顶层 `broker:` 段（v0.1.34 起为命名 map，键 = profile 名）。bus / mq 连接收的 JSON 字段：

| 字段 | 说明 |
|---|---|
| `kind` | 必须 `rabbit`（装错插件 → init 报 `kind` 不匹配 fail-fast） |
| `url` | amqp URL；缺时回落取 `brokers` 首个（二者皆缺 → 报错） |
| `brokers` | 备用地址列表（取首个） |
| `group` | 可选 |
| `topic_prefix` | bus 面 topic 交换名（默认 `oj-bus`） |

mq 面的 `poll` 收 `queues` / `max` / `timeoutMs`；`send` 走 `exchange` / `routingKey` / `headers` / `value`（`value_b64` 二进制透传）。

字段权威定义见 [`../modules/02-config.md`](../modules/02-config.md) 的 `broker` 段。

## 依赖与构建注意

- `lapin`，纯 Rust（基于 `tokio`，无原生 TLS 库），构建比 kafka 轻。
- 装配期即拨号探活（fail-fast）；amqp URL 带 `user:pass@`，错误文案会脱敏（`amqp://u:***@host`）。
- 复用 channel 防 channel_max 打满；mq 面未确认投递的 acker 在 `close` 时随实例 drop，由 broker 重投。
- `close` 同样停掉 detach 的 push 消费任务。

## 状态

已随发行包发布（较早合入）。

## 备注

- 双轴插件：`bus` push 扇出、`mq` pull 消费共用 `RabbitCore`。
- 真库 roundtrip 用例由 `OJ_TEST_RABBITMQ_URL` 门控。
