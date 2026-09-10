# news —— WebSocket + 发布订阅 + 会话状态

教学目标（对应 [../../MODULES.md](../../MODULES.md) §⑤，WS 专题见
[../../../docs/websocket.md](../../../docs/websocket.md)）：

1. **WS 生命周期钩子**：`export default { connection, message, close, error }`——
   连接触发一次 `connection`，每帧触发 `message`，收尾 `close`，异常兜底 `error`。
2. **bus 发布订阅**：WS 连接 `bus.subscribe` 订阅主题；HTTP 路由或帧内
   `bus.publish` 广播到所有订阅连接（后端可换 kafka/rabbitmq 插件）。
3. **sess.state 会话状态外置**：跨帧持久、按连接隔离的可变状态放 `sess.state`
   （Rust 会话表持有，须可 JSON 序列化）。

## 文件导览

| 文件 | 角色 |
| --- | --- |
| `ws.ts` | 生命周期钩子最小演示：`connection()` 写 `sess.state.ready` 并订阅 `news` 主题 |
| `api.ts` | `POST /v1/api/news`：`bus.publish("news", {...})` 广播到所有订阅连接 |
| `chat/ws.ts` | 帧内发布聊天室：进房 = `connection()`（无需 join 帧），`message()` 每帧广播 |

## 怎么跑

```bash
# 启动（仓库根目录）
bin/oj server -c sample/config.yaml --api-path sample/src

# 终端 1：连上即完成订阅，收到欢迎帧
websocat ws://localhost:9778/v1/api/news/ws

# 终端 2：HTTP 发布 → 终端 1 收到 {"topic":"news","data":{...}}
# （业务端点受 auth 保护：$AUTH 两个头的获取见 ../../MODULES.md 开头）
curl "${AUTH[@]}" -X POST -d '{"text":"hello oj"}' http://localhost:9778/v1/api/news

# 聊天室（两终端各连一个，互发互收，含自回声）
websocat ws://localhost:9778/v1/api/news/chat/ws
# 连上后发送：{"from":"neo","text":"hi"}
```

## v0.1.10 帧池模型注意点

执行模型为**帧池**：每路由 W 个无状态 Worker 共享执行，同一连接的帧串行保序，
不同连接的帧可能落在不同 Worker 上。因此：

- **可变跨帧状态一律放 `sess.state`**（可 JSON 序列化；不可序列化的赋值会被
  静默丢弃且保留旧状态，不会整体清空）。
- **模块作用域只是 Worker 本地缓存**——只放只读数据（配置、常量、编译产物）；
  在模块作用域记连接/会话状态会因换 Worker 而丢失，属于 bug。
