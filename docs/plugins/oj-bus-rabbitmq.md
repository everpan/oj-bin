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

## 案例

### bus 面：工单状态变更实时推给坐席台

```ts
// src/ticket/status/api.ts —— 状态流转 → bus.publish 扇出（topic_prefix 默认 oj-bus）
async function post() {
  const b = http.body as { id?: string; status?: string };
  if (!b?.id || !b?.status) { json.fail(400, "need id & status"); return; }
  const n = await bus.publish("ticket.status", { id: b.id, status: b.status });
  json.ok({ receivers: n });
}
export default { post };
```

```ts
// src/ticket/status/ws.ts —— 坐席台连接即订阅；断开自动清除
export default {
  connection() {
    bus.subscribe("ticket.status");
    json.ok({ subscribed: true });
  },
};
```

```yaml
# config.yaml —— broker 命名 map（v0.1.34）；bus 面共享这份连接
broker:
  default:
    kind: rabbit
    url: "amqp://guest:guest@127.0.0.1:5672"
```

```bash
# WS 先连 /v1/api/ticket/status/ws，然后：
curl -X POST -H 'Content-Type: application/json' \
  -d '{"id":"T-77","status":"resolved"}' http://localhost:9778/v1/api/ticket/status/
# WS 侧收到 Text 帧 {"topic":"ticket.status","data":{"id":"T-77","status":"resolved"}}
```

### mq 面：短信发送队列逐条消费（ack / 失败 nack 重投）

```ts
// src/tasks/task_sms.ts —— basic.get 拉取 → 成功 ack → 失败 nack 回队列（仅任务上下文可用）
export {};
const r = RabbitMQ("default");
while (!tasks.stopping()) {
  const { messages } = await r.poll(["sms.outbox"], { max: 10, timeoutMs: 1000 });
  for (const m of messages) {
    try {
      const resp = await fetch("https://sms.example.com/send", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ to: m.value.to, text: m.value.text }),
      });
      if (!resp.ok) throw new Error("sms gateway " + resp.status);
      await r.ack(m);
    } catch (e) {
      await r.nack(m, true);   // requeue：交回 broker 重投，at-least-once
      log.warn("sms send failed, requeued: " + e);
    }
  }
}
```

```yaml
# config.yaml —— mq 面走独立的 rabbits: 命名段（值透传本插件）
rabbits:
  default:
    url: "amqp://guest:guest@127.0.0.1:5672"
```

注意：未确认投递的 acker 随实例 `close` drop，由 broker 重投——处理逻辑必须幂等；
`timeoutMs` 要显著小于 `tasks.stop_grace_secs`（默认 30s），避免停机被看门狗强杀。

## 备注

- 双轴插件：`bus` push 扇出、`mq` pull 消费共用 `RabbitCore`。
- 真库 roundtrip 用例由 `OJ_TEST_RABBITMQ_URL` 门控。
