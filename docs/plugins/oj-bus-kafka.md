# oj-bus-kafka

## 概述

提供 `bus` 轴与 `mq` 轴的 cdylib 插件（双轴）。底层 `rdkafka`，按 `broker:` 段装配。bus 面做 push 扇出（订阅经 `deliver` 回调上送）；mq 面做 pull 消费会话（显式 commit，at-least-once）。两面对接同一份 `KafkaCore` driver。

## 提供的后端轴

`bus` + `mq`（双轴）

## 配置

顶层 `broker:` 段（v0.1.34 起为命名 map，键 = profile 名，供 `--broker <profile>` 选默认源）。bus / mq 连接收的 JSON 字段：

| 字段 | 说明 |
|---|---|
| `kind` | 必须 `kafka`（装错插件 → init 报 `kind` 不匹配 fail-fast） |
| `brokers` | 逗号分隔 bootstrap servers，**必填**（缺 → 报错） |
| `group` | 消费组，默认 `oj-bus` |
| `topic_prefix` | bus 面 topic 前缀（默认空） |

mq 面的 `send` / `poll` / `commit` 走 `kind` 自检后的命名客户端；per-call 的 `poll` 还收 `topics` / `max` / `timeoutMs`。

字段权威定义见 [`../modules/02-config.md`](../modules/02-config.md) 的 `broker` 段。

## 依赖与构建注意

- `rdkafka`，**原生依赖 OpenSSL / zlib / SASL**：构建机需装这些系统库，否则 link 失败。
- bus 面 auto-commit（push 语义）；mq 面 auto.commit 关、JS 显式 commit。
- `close` 会停掉 detach 的 push 消费任务（watch 信号 + 发送端 drop），修复过消费任务泄漏。

## 状态

已随发行包发布（较早合入）。

## 备注

- 这是双轴插件：`bus` 负责订阅推送，`mq` 负责命名客户端点对点消费，二者共用 `KafkaCore`。
- 真库 roundtrip 用例由 `OJ_TEST_KAFKA_BROKERS` 门控，未设则跳过（不进网络）。
