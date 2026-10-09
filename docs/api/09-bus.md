# bus —— 订阅发布总线

> 本文档由 src/bridge/ 梳理生成；权威 API 参考见 ../devkit/api-manual.md。

## 是什么

`bus` 全局对象（`src/bridge/bus.rs` + `bus_backend.rs` + `broker/mod.rs`）是**广播型**
订阅发布总线：**WS 会话订阅 topic，任意 handler 发布广播帧**。方向约定：
HTTP handler（或 ws 帧钩子）发布，WebSocket 连接订阅。

后端经统一契约 `EventBroker` 抽象、按 `broker.kind` 键选装配：

- **缺省 = 进程内 `Bus`**（零配置，`kind: "local"`）——topic → 订阅者发送器列表，
  fan-out 到本进程全部 WS 连接；跨实例不互通。
- `kind: "kafka"` / `"rabbitmq"` —— 由 cdylib 插件 `oj-bus-kafka` / `oj-bus-rabbitmq`
  的 **bus 轴**承载：publish 投递到远程 broker，各实例派生消费任务把消息转发给本地
  订阅的 WS 连接（分布式扇出，多实例互通）。插件未装而配置了对应 kind → 启动报错。

订阅方即 WS 连接的 `bus_tx`：serve 层 ws.rs 每连接建 channel，帧循环把 bus 帧转写回
socket。publish 对所有订阅者 `try_send`，满/closed 即清——**无背压、不阻塞发布者**。

**广播帧形态**（订阅的 WS 客户端实际收到的帧，v0.1.16 起）：

| publish 数据 | WS 帧 | 形态 |
|---|---|---|
| JSON（对象/数组/字符串/数字/undefined） | Text 帧 | 信封 `{"topic":"<topic>","data":<data>}` |
| `Uint8Array` / `ArrayBuffer` | Binary 帧 | **原字节透传，无信封**（topic 由订阅关系隐含） |

## 配置

```yaml
broker:            # 段缺省 = 进程内 Bus（kind local），保持零配置现状
  kind: "local"    # local | kafka | rabbitmq（后两者需对应 bus 插件）

  # kafka（oj-bus-kafka）：
  # kind: "kafka"
  # brokers: ["b1:9092", "b2:9092"]   # bootstrap servers，必需
  # group: "oj-bus"                   # 消费组，默认 "oj-bus"
  # topic_prefix: ""                  # 物理 topic 前缀，可选

  # rabbitmq（oj-bus-rabbitmq）：
  # kind: "rabbitmq"
  # url: "amqp://guest:guest@127.0.0.1:5672"   # 或取 brokers[0]
  # topic_prefix: "oj-bus"                      # 交换名，默认 "oj-bus"
```

- `broker:` 兼容命名多源 map 写法（`broker: { default: {...}, b2: {...} }`），旧单对象写法
  自动包成 `{ default: {...} }`；CLI `--broker <profile>` 选哪个作默认。
- `kind` 缺省/空串 = local；未知 kind → 启动报错 `unknown broker kind '<kind>' (known: [...])`。

## API

| API | 签名 | 说明 |
|---|---|---|
| `bus.publish` | `(topic: string, data?: unknown \| Uint8Array \| ArrayBuffer) => Promise<number>` | 广播给订阅该 topic 的全部 WS 会话，返回**本进程**接收方数（无订阅返回 0；远程 broker 经网络投递，本地 fan-out 恒 0）。JSON → Text 信封帧；字节 → Binary 原样帧 |
| `bus.subscribe` | `(topic: string) => Promise<void>` | 当前 WS 会话订阅 topic（**HTTP 路径调用报错**——订阅对象是连接本身）。连接断开自动清除；同一会话重复订阅幂等去重 |
| `bus.kind` | `() => Promise<string>` | 活跃 broker 类型：`"local"` / `"kafka"` / `"rabbitmq"`。**异步 op，判等须 `await`** |

## 错误

| 场景 | 消息关键词 |
|---|---|
| HTTP handler 里调 `bus.subscribe` | `bus.subscribe requires a WebSocket connection` |
| `broker.kind` 配了插件未装的类型 | `unknown broker kind '<kind>' (known: [...])`（启动期 fail-fast） |
| 远程 broker 连接/投递失败 | 插件原始错误透出（`kafka ...` / `rabbitmq ...`） |

## 限制

- **订阅只能是 WS 连接**：HTTP 请求没有长连接可挂订阅，调用即报错。
- **fire-and-forget，无持久化**：无订阅者时消息直接丢弃（publish 返回 0）；需要可靠
  逐条消费（offset/ack 语义）用 `Kafka`/`RabbitMQ`（mq 轴），见 `10-mq.md`。
- **无背压**：订阅者 channel 满/closed 即被清理，不阻塞发布者。
- `bus.publish` 无上下文限制（HTTP/WS 钩子均可），只有 `bus.subscribe` 限 WS。
- 远程 broker 下 publish 返回值恒 0（本地 fan-out 数），不要拿它判断"有没有客户端收到"。
- 与 mq 轴的分工：bus 是**广播**（扇出给在线 WS 客户端，topic 即逻辑频道）；mq 是
  **持久队列消费**（消费组逐条拉取、commit/ack 确认）。`broker:` 段喂 bus 轴，
  `kafkas:`/`rabbits:` 段喂 mq 轴——同配一套 Kafka 可以两段都写、各司其职。

## 案例

### HTTP 发布 → WS 订阅（新闻推送）

```ts
// src/news/ws.ts —— WS 路由 /v1/api/news/ws；connection 钩子里订阅
export default {
  connection() {
    sess.state.ready = true;   // 会话状态外置：跨帧持久、按连接隔离
    bus.subscribe("news");
    json.ok({ subscribed: true });
  },
};
```

```ts
// src/news/publish/api.ts —— 任意 HTTP handler 发布
async function post() {
  const text = String(http.body?.text ?? "");
  if (!text) { json.fail(400, "text required"); return; }
  const n = await bus.publish("news", { text, at: Date.now() });
  json.ok({ receivers: n }); // 本进程收到广播的 WS 连接数
}
export default { post };
```

```bash
# 客户端收到 Text 帧：{"topic":"news","data":{"text":"hi","at":…}}
curl -X POST -H 'Content-Type: application/json' -d '{"text":"hi"}' \
  http://localhost:9778/v1/api/news/publish/
# → {"code":0,"data":{"receivers":1}}
```

```yaml
# config.yaml —— 缺省即进程内 Bus，无需任何 bus 配置；分布式扇出才写 broker:
# broker:
#   kind: "kafka"
#   brokers: ["127.0.0.1:9092"]
```

### 聊天室：ws.ts 帧内直接发布

`bus.publish` 无上下文限制，帧钩子可以当「广播泵」。注意**自回声**：本连接若订阅了同一
topic，会收到自己发布的帧（fan-out 不排除自己），客户端按 `from` 过滤即可。

```ts
// src/news/chat/ws.ts —— connection = 进房订阅；message = 收帧转发
export default {
  connection() {
    bus.subscribe("chat");
    json.ok({ joined: true });
  },
  message() {
    const frame = http.body; // JSON 文本帧已自动 parse 成对象
    if (frame && frame.text) {
      bus.publish("chat", { from: frame.from ?? "anon", text: frame.text });
      json.ok({ sent: true });
    }
  },
};
```

### 二进制广播（proto/音视频分片）

```ts
// src/relay/api.ts —— 字节载荷 → 订阅者收 Binary 帧原字节（无信封）
async function post() {
  const bytes = await http.bodyBytes();       // 请求体原始字节
  const n = await bus.publish("relay.bin", new Uint8Array(bytes));
  json.ok({ receivers: n, bytes: bytes.length });
}
export default { post };
```

```bash
curl -X POST --data-binary @chunk.bin http://localhost:9778/v1/api/relay/
# 订阅 "relay.bin" 的 WS 客户端收到 Binary 帧，载荷 = chunk.bin 原字节（topic 不随帧，
# 客户端按自己订阅的频道归位）
```
