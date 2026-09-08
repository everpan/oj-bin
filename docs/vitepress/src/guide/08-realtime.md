---
title: 08 · 实时与消息
updated: 2026-09-08
---

# 08 · 实时与消息

oj 里「实时」有三层，从轻到重：

| 需求 | 用什么 | 场景 |
|---|---|---|
| 浏览器连进来、按帧交互 | `ws.ts` + 模块目录 | 站内通知、聊天、实时看板 |
| 进程内/跨实例广播 | `bus`（redis / kafka / rabbitmq 实现） | 一条消息推给所有在线连接 |
| 可靠的消息生产消费 / 后台常驻 | 命名 MQ 客户端 + 长任务池 | 消费业务队列、定时心跳、离线任务 |

## 服务端 WS：`ws.ts`

放在模块目录下就产生 `{base}/<模块>/ws` 路由（文件名固定小写 `ws.ts` / `ws.js`）：

```ts
// src/news/ws.ts —— 整个文件体 = 一帧的处理逻辑
bus.subscribe("news");          // 幂等（同通道去重），每帧重跑无害

const frame = http.body;        // 文本帧已自动 parse 成对象
if (frame && frame.text) {
  bus.publish("news", { topic: "news", from: frame.from ?? "anon", text: frame.text });
  json.ok({ sent: true });      // 帧内 json.ok 正常回信封
} else {
  json.ok({ joined: true });
}
```

三点与「写 HTTP handler」完全不同，必须记住：

1. **整个文件每帧执行一次**（不是导出函数）：所以 `bus.subscribe` 这类动作要写成幂等的，
   不要在顶层维护「只做一次」的状态。
2. **订阅的是连接**：`bus.subscribe` 把当前 WS 会话挂到频道上，**只能在 WS 上下文里调**，
   在 HTTP handler 里调会报错。
3. **发布没有上下文限制**：`bus.publish` 在 HTTP handler 和帧处理器里都能用，
   `ws.ts` 可以直接当广播泵。

跑通 sample 的演示：连 `/v1/api/news/ws` 发任意一帧，再 `POST /v1/api/news`，
连接就会收到 `{"topic":"news", …}`。

## 出站 WS 客户端

JS 里直接 `new WebSocket(url)` —— 标准 WHATWG 客户端（v0.1.7 起由 deno_websocket 提供），
https 与 wss 共用同一套根证书（webpki-roots 编进二进制，零系统依赖）。
案例见 `sample/src/tasks/task_wsclient.ts`。

## 命名 MQ 客户端

config 里按名字声明，JS 里按名字取用：

```yaml
kafkas:
  main: { brokers: "127.0.0.1:9092" }
rabbits:
  default: { url: "amqp://127.0.0.1:5672" }
```

```ts
Kafka("main").send("topic", { value: "payload" });        // 生产：handler 里也能发
const { messages } = await Kafka("main").poll(["topic"]); // 消费：仅任务上下文
const { messages: m2 } = await RabbitMQ("default").poll("queue");
```

要点：

- **生产可以在 handler 里做，消费只能在长任务上下文** —— 这是硬约束，不是风格建议。
- `poll` 是长轮询带退避，不会空转烧 CPU；Kafka 侧用 `commit(m)` 提交位点。
- 未配置的名返回 `undefined`（不是抛错），所以取用前先判空。
- JS 侧用 `tasks.stopping()` 感知停机信号、`tasks.sleep(ms)` 让出时间片。

## 长任务池

`src/tasks/*.ts` 里的任务：

- 启动时按文件名扫描并拉起；
- 崩了按退避策略重启，不会带崩宿主；
- 收到停机信号后等任务自己退出（`tasks.stopping()` 轮询）；
- `oj build` 会把 `tasks/` 原样转译镜像到 `dist/tasks/`（非版本化资产）。

## 延伸

- [WebSocket](../reference/websocket.md)（含帧循环约定与实现走读）
- [MQ 与长任务](../reference/mq-tasks.md)（命名客户端与长任务手册）
- [news 模块演示](../sample/modules-tour.md)
