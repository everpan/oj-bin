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

## 案例

### bus 面：订单事件广播到所有在线管理端

```ts
// src/order/event/api.ts —— 下单成功 → bus.publish 扇出到全部订阅连接
async function post() {
  const b = http.body as { orderId?: string; amount?: number };
  if (!b?.orderId) { json.fail(400, "need orderId"); return; }
  const n = await bus.publish("order.new", { orderId: b.orderId, amount: b.amount ?? 0 });
  json.ok({ receivers: n });   // n = 收到广播的 WS 会话数（无订阅返回 0）
}
export default { post };
```

```ts
// src/order/event/ws.ts —— 管理端连上即订阅；方向约定：HTTP 发布，WS 订阅
export default {
  connection() {
    bus.subscribe("order.new");
    json.ok({ subscribed: true });
  },
};
```

```yaml
# config.yaml —— broker 命名 map（v0.1.34）；bus 面共享这份连接
broker:
  default:
    kind: kafka
    brokers: "127.0.0.1:9092,127.0.0.2:9092"
    group: "oj-bus"
```

```bash
# 浏览器/客户端先连 WS /v1/api/order/event/ws，然后：
curl -X POST -H 'Content-Type: application/json' \
  -d '{"orderId":"SO-1001","amount":199}' http://localhost:9778/v1/api/order/event/
# WS 侧收到 Text 帧 {"topic":"order.new","data":{"orderId":"SO-1001","amount":199}}
```

### mq 面：订单对账任务逐条消费（显式 commit，at-least-once）

```ts
// src/tasks/task_reconcile.ts —— poll → 幂等处理 → 按分区 commit（仅任务上下文可用）
export {};
const k = Kafka("default");
while (!tasks.stopping()) {
  const { messages } = await k.poll(["orders"], { max: 100, timeoutMs: 1000 });
  const deepest = new Map();   // partition → 该分区最深的消息
  for (const m of messages) {
    await db.table("ledger").insert({ order_id: m.value.orderId, amount: m.value.amount }).run();
    const cur = deepest.get(m.partition);
    if (!cur || m.offset > cur.offset) deepest.set(m.partition, m);
  }
  for (const m of deepest.values()) await k.commit(m);  // 按 m.offset+1 提交
}
```

```yaml
# config.yaml —— mq 面走独立的 kafkas: 命名段（值透传本插件）
kafkas:
  default:
    brokers: ["127.0.0.1:9092"]
    group: "reconcile"
```

注意：commit 按 offset+1 只推进该消息所在分区——多分区 topic 必须按分区各提交一次，
只提交最后一条会丢其余分区进度；`timeoutMs` 要显著小于 `tasks.stop_grace_secs`
（默认 30s），否则停机时被看门狗强杀。

## 备注

- 这是双轴插件：`bus` 负责订阅推送，`mq` 负责命名客户端点对点消费，二者共用 `KafkaCore`。
- 真库 roundtrip 用例由 `OJ_TEST_KAFKA_BROKERS` 门控，未设则跳过（不进网络）。
