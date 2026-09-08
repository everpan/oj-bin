# WS 客户端任务设计（deno_websocket 全局 + 监督重连案例）

> 需求：以 `docs/mq-tasks.md` 的长任务模式，编写 WebSocket **客户端**任务案例，从 WS
> 服务端取数。现状：运行时无出站 WS 客户端——`ws` 全局只是 `ws.ts` 帧循环的服务端
> `send/close`，`fetch` 是纯 HTTP。用户已裁决走 **deno_websocket 扩展**路线（另两条
> 被否：仿 MQ 自写 tokio-tungstenite 客户端 = 重复造轮子；HTTP 轮询 = 非真 WS）。

## 裁决

1. **deno_websocket 0.263 + deno_core 0.410**：版本兼容已验证（cargo 解析通过）。
   标准 WHATWG `WebSocket` 全局，deno 官方实现，任务与 HTTP handler 通用。
2. **只注册主池**：`src/bridge/runtime.rs:70` 的 `extensions` 数组（HTTP actor 池与
   任务 Bridge 共用此池）。`inspector.rs` 两处、`oj/src/test_cmd.rs` 一处**不动**
   （不跑业务，需要时再加）。
3. **permissions allow-all**：出站 WS 与 `fetch` 允许任意出站 URL 同一信任边界，
   不新增审批面。
4. **bootstrap.js 零改动**：`WebSocket` 全局由扩展自带 JS 声明，7-bit ASCII 红线
   无虞。
5. **重连 = 崩溃监督，不手写 reconnect**：连接断开时 reject 挂起的取帧 promise →
   TLA throw → `Crashed` → 监督器指数退避重启（1s→2s→…cap 60s）→ 新实例重连。
   复用 `oj/src/tasks.rs` 现成语义，案例零重连代码。
6. **案例取数目标 = sample 自身**：`sample/src/news/ws.ts`（`/v1/api/news/ws`）是
   订阅型服务端——首帧触发 `bus.subscribe("news")`，此后 `bus.publish("news", ...)`
   广播到本连接。自包含 demo，零外部依赖。
7. **无 task 门禁**：WS 客户端连接无 MQ 消费会话语义（无 offset/ack），不设
   `requires a task context` 门禁，HTTP handler 内同样可用。

## 交付物

| 文件 | 变更 |
|---|---|
| `Cargo.toml` | + `deno_websocket = "0.263"`（连带 `deno_web`/`deno_webidl` 等按其要求补齐，probe 钉死） |
| `src/bridge/runtime.rs` | extensions 注册 deno_websocket 及前置扩展（bridge_ext 之前） |
| `sample/src/tasks/task_wsclient.ts` | 客户端任务案例：连本机 news WS、订阅、消息泵取帧、`tasks.stopping()` 退出时 `ws.close()` |
| `docs/devkit/api-manual.md` | 全局对象表 + `WebSocket` 行与短小节（含「断连 = Crashed = 监督重连」） |
| `src/bridge/` 测试 | ① 全局存在性烟测 ② in-process WS 服务端 + 任务 bridge 连接收帧断言 |

案例形态（文件内注释承载教学，同 `task_demo.ts` 风格）：

```ts
export {};
const ws = new WebSocket("ws://127.0.0.1:9778/v1/api/news/ws");
await new Promise((ok, err) => { ws.onopen = ok; ws.onerror = err; });
ws.send("{}");                        // 首帧 → 服务端 bus.subscribe("news")
while (!tasks.stopping()) {
  const frame = await nextFrame(ws);  // Promise 队列把事件流转成 await
  log.info("got", frame);
}
ws.close();                           // Stopped 出口干净断连
```

## 实现前置（第一步）

/tmp probe 编译 deno_core 0.410 + deno_websocket 0.263 最小 runtime，钉死：
deno_websocket 0.263 的 init 签名、必需前置扩展（deno_webidl / deno_web / tls 根
证书面）、permissions trait 形状。再动主仓。

## 红线核对

- bootstrap.js 不动 → 7-bit ASCII ✓；`!Send`：扩展由 deno_core 在 runtime 线程内
  管理，无新增跨线程持有 ✓；`panic = "unwind"` 不触碰 ✓。
- 网络受限环境：deno_websocket 是纯 Rust（rustls 系），不触发 V8 下载变化。

## 非目标

- 命名 WS 客户端注册表 / config 驱动 URL（URL 直接写在任务文件里，够用）
- inspector、`oj test` 运行时注册
- 手写 reconnect 循环、心跳/指数退避应用层实现（监督器已覆盖）
- mq-tasks.md 改动（MQ 专题文档，不混 WS）

## 测试计划

| 用例 | 钉住的行为 |
|---|---|
| bridge js_global_tests：`typeof WebSocket === "function"` | 全局已声明 |
| bridge task 集成：in-process WS 服务端，任务连上收一帧 | 客户端链路端到端 |
| 案例手跑：`cargo run -p oj -- server -c sample/config.yaml --api-path sample/src` + 触发一次 `bus.publish("news", ...)` | 任务日志收到广播帧 |
