# WebSocket 教学（ws.ts 生命周期钩子）

> 面向两类读者：**要写 WS handler 的业务开发者**（§1–§3、§7）与**要改 WS 实现的维护者**
> （§4–§6、§7 末）。JS 签名权威是 [devkit/api-manual.md](devkit/api-manual.md)（§4 `ws.ts` 小节、
> §6 `ws`/`WebSocket`/`fetch` 行）；维护者模块地图是 [modules/03-server-http.md](modules/03-server-http.md) §5。
> 可运行示例全程以 `sample/src/news/` 为锚。

## 1. 心智模型：一个文件 = 一条 WS 路由 = 生命周期钩子（帧池运行时）

目录内放 **`ws.ts`**（dev 源码）或 **`ws.js`**（release 构建产物，`oj build` 自动生成），
即产生一条 WebSocket 路由 `GET {base}/{...path}/ws`：

```
src/news/ws.ts   →  /v1/api/news/ws        （目录镜像，与 api.ts 同规则）
src/ws.ts        →  /v1/api/ws             （根级）
```

与 `api.ts` 的分工：

| | `api.ts` | `ws.ts` |
|---|---|---|
| 触发 | HTTP 请求命中路由 | 连接升级后**客户端每个文本/二进制帧** |
| 执行单元 | `default[method]()` 一个函数 | `default` 导出的钩子（`connection` 一次 / `message` 每帧 / `close` 一次 / `error` 兜底） |
| 回写 | `{code,msg,data}` 信封 HTTP 响应 | 信封文本帧 + `ws.send` 裸帧 |
| 运行位置 | HTTP actor 池（`server.pool_size` 个 VM 排队复用） | **路由级帧池**（连接状态外置 Rust 会话表） |
| Worker 池 | actor 即池，无独立 Worker 层 | **W 个无状态 Worker/路由**（`ws.workers_per_route`，默认 2）从帧队列拉帧执行 |

帧进来时，注入的请求上下文是：`http.method === "WS"`、**`http.body` = 帧字节**
（`Uint8Array`）；`http.query`/`http.headers` 为空对象（upgrade URL 未透传）。

连接状态放 **`sess.state`**（跨帧持久、按连接隔离，**必须可 JSON 序列化**），
`sess.id` 为连接 id；模块作用域只是 Worker 本地只读缓存——可变跨帧状态禁止放模块
作用域。约束详见 §4「sess 会话状态与帧池约束」。

## 2. 写一个 handler：sample/news 逐行

`sample/src/news/ws.ts` 的钩子主体：

```ts
export default {
  connection() {
    bus.subscribe("news");           // ① 订阅 "news" 主题（每连接恰好一次）
    json.ok({ subscribed: true });   // ② 回一帧标准信封 {"code":0,"data":{"subscribed":true}}
  },
};
```

- **① 订阅挪进 `connection()`**：连接建立后恰好触发一次，天然不会重复订阅；
  `Bus::subscribe`（`src/bridge/bus.rs:55`）按发送端通道去重（`same_channel`）的机制
  保留——同主题二次订阅是 no-op，作为幂等保证。
- **②** 钩子内 `json.ok`/`json.fail` 与 HTTP 语义一致：整段 `{code,msg,data}` 序列化成
  **一个文本帧**写回（返回值一律忽略，回帧必须显式调用）。

同一个目录的 `api.ts`（`POST /v1/api/news`）负责发布：

```ts
bus.publish("news", { text: "hello oj" });   // 广播到所有订阅了 "news" 的 WS 连接
```

订阅连接会收到一帧广播 JSON：`{"topic":"news","data":{"text":"hello oj"}}`。
bus 后端可换（`local`/kafka/rabbitmq 插件），所以跨进程实例的 HTTP 发布同样能
广播到本连接——handler 完全无感。

### 帧处理器可用的全部「主动作」

| 调用 | 效果 |
|---|---|
| `json.ok(data?)` / `json.fail(code,msg,data?)` | 回一个信封文本帧（每帧最多一个） |
| `ws.send(data)` | 额外发**裸文本帧**（先于信封写出，可多次） |
| `ws.close()` | 本帧处理完后服务端发 Close 帧，连接干净关闭 |
| `bus.subscribe(topic)` | 订阅主题（仅 WS 上下文可用；HTTP handler 里调用直接报错） |
| 其余全局 `db`/`kv`/`http`/`log`/… | 照常可用；TS 类型标注走统一转译管线 |

两个执行期语义：

- **单帧超时**：一个钩子卡死（如死循环）**必断连**——仅断**该连接**：runtime 被
  terminate 后已毒化、Worker 随之弃置（池自动补员，其它连接无感），钩子收不到后续
  事件，连接被服务端关闭（默认 30s，`oj/src/app.rs` 传给 `mirror_routes`）。
- **顺序契约**：写出顺序 = `ws.send` 按调用序 → 信封 → （后续广播帧）。广播帧与
  主动发送走同一条写出通道，天然保序（`server/src/ws.rs` Bus forwarder）。

### 帧内发布：`ws.ts` 里直接 `bus.publish`

`bus.publish` 没有上下文限制（只有 `bus.subscribe` 限 WS 连接），所以帧处理器可以
直接当「广播泵」用。可运行案例：`sample/src/news/chat/ws.ts`——聊天室，连上即进房
（`connection()` 订阅，无需 join 帧），任意连接发帧，所有订阅者收广播：

```ts
// connection = 进房，订阅每连接一次；message = 每帧
export default {
  connection() {
    bus.subscribe("chat");
    json.ok({ joined: true });
  },
  message() {
    const frame = http.body;              // JSON 文本帧已自动 parse 成对象
    if (frame && frame.text) {
      bus.publish("chat", { from: frame.from ?? "anon", text: frame.text });
      json.ok({ sent: true });
    }
  },
};
```

```bash
# 终端 1 / 2 各连一处（连上即进房）；任一终端发 {"from":"neo","text":"hi"}
websocat ws://localhost:9778/v1/api/news/chat/ws
# → 两个终端都收到 {"topic":"chat","data":{"from":"neo","text":"hi"}}
```

帧内发布特有的三条语义：

1. **自回声**：本连接若订阅了同一主题，会收到自己发布的广播帧（Bus fan-out 不排除
   自己）——按 `from` 字段客户端过滤，或发布到别的 topic。
2. **钩子可直接 `await`**：钩子会被驱动至 Promise 落定才捕获回帧——要拿 `publish`
   返回的「本地接收方数」，直接 `await bus.publish(...)`（kafka/rabbitmq 下该数恒 0）。
3. **无状态也够用**：钩子共享该路由的 Worker 池，模块作用域只是 Worker 本地只读
   缓存——可变跨帧状态放 `sess.state`（按连接隔离）或 kv/bus（跨连接）；聊天室本例
   全靠 bus 广播，无需任何本地状态。

回归：`ws_frame_publish_broadcasts_to_subscribers`（§6）。

## 3. 跑通 sample

```bash
# 终端 1：dev 模式启动（服务 src，按需转译）
cargo run -p oj -- server -c sample/config.yaml --api-path sample/src

# 终端 2：连上即完成订阅（connection() 钩子回报一帧信封；websocat 任选你顺手的客户端）
websocat ws://localhost:9778/v1/api/news/ws
# < {"code":0,"msg":"ok","data":{"subscribed":true}}

# 终端 3：HTTP 发布（业务端点，需 Bearer + 租户头，token 获取见 sample/MODULES.md §④）
curl "${AUTH[@]}" -X POST -d '{"text":"hello oj"}' http://localhost:9778/v1/api/news
# → 终端 2 收到 {"topic":"news","data":{"text":"hello oj"}}
```

release 模式：先 `oj build`（`ws.ts` 随模块一起转译成 `dist/<mod>-<ver>/ws.js`）。
⚠️ 已知限制：release 下 root=dist，WS URL **含模块版本段**（`/v1/api/news-0.1.0/ws`），
见 `docs/user-manual.md`。

### 鉴权现状（务必知道）

`/v1/api/news/ws` 是 **merge 进 Router 的真实路由**，不是 fallback——而 Bearer 守卫、
租户头校验、证书 GET 门禁全部实现在 fallback `handle()`（`server/src/lib.rs`）的前置
管线里。**WS upgrade 因此不经过这套管线**：连接本身是匿名的，鉴权需在帧处理器里
自行做（如校验 `ws.send` 握手帧里带的 token）。把它当红线记住。

## 4. 实现走读（改代码前读这节）

涉及三层，自上而下：

```
oj/src/app.rs             装配：mirror_routes(base, dir, timeout, make_bridge, opts) merge 进 Router
server/src/ws.rs          连接生命周期：upgrade → 闸门 → 三任务流水线（全 Send，跑在 axum runtime）
src/bridge/frame_pool.rs  帧调度：Scheduler（per-conn 在飞=1 保序）+ W Worker + Rust 会话表
src/bridge/mod.rs         帧执行：ws_connect 预载钩子 → ws_event（每事件一次）→ WsOutcome
```

**挂载**（`mirror_routes`）：递归扫 `ws.ts`（优先）/`ws.js`
（`ws_files`，同目录并存时 `.ts` 胜），每个文件一条 `GET …/ws` 路由，**每个文件一池**
（`RoutePool::new(file, make, timeout, workers_per_route, idle_linger_ms)`）。源码与
`api.ts` 共用同一套 `transpile::cached_transpile`（mtime 缓存、TS 剥类型）。

**线程模型**：`JsRuntime` 是 `!Send` 的——这条约束沉到 Worker：每 Worker 一条专用
OS 线程（`ws-worker`，内建 `current_thread` runtime），V8 只在 Worker 里跑钩子。
连接侧 `frame_loop` 全 Send，直接跑在 axum 多线程 reactor 上，**不再每连接起专用
线程**；连接只持有收发通道与会话表条目（`sess.state` ≈2KB 量级），内存与连接数解耦。

**三任务流水线**（`frame_loop`，通道各 cap 64 做背压；帧经路由池调度执行）：

```
socket ──Reader──▶ msgChan(64) ──▶ 帧队列 ──▶ W × Worker（帧队列拉取）──▶ respChan(64) ──Writer──▶ socket
                                                ▲
bus 广播帧 ────────────Bus forwarder（unbounded）┘        （与 ws.send 同通道 → 保序）
```

- **Reader**：文本/二进制帧 → `msgChan`；满了背压到 TCP 层（对端 send 变慢）。
- **Worker（帧执行）**：升级后先 `fire("connection", …)`，每帧组一个
  `RequestInfo { method: "WS", body: 帧字节, bus_tx }` 提交池。调度器 **per-conn
  在飞=1**：同连接帧严格保序（在飞期间后续帧在 waiting 队列排队）。Worker 执行时从
  Rust 会话表读出该连接的 `sess.state` 注入 JS，帧末把快照回写会话表（`op_ws_sess_set`）；
  成功后把 `o.sends`（`ws.send` 收集）逐条、再把 `o.capture.body`（信封）压进
  `respChan`。`o.close` 置位则跳出循环（断连前 `fire("close", …)` 收尾恰好一次）。
- **Writer**：串行写回；`respChan` 排空后发 Close 帧。ping/pong 由 axum 自动处理。
- **Bus forwarder**：把本连接订阅的广播帧转进同一条 `respChan`（订阅时 bus 发送端经
  `attach` 存入该连接的会话表条目）。连接结束时 `forwarder.abort()` 收尾——否则 bus
  订阅表里的发送端滞留，Writer 永不排空。

**op 层**（`src/bridge/ws.rs`，三个）：`op_ws_send` 把字符串 push 进
`ReqState.ws_sends`，`op_ws_frame_close` 置位 `ReqState.ws_close`，`op_ws_sess_set` 由
dispatcher `finally` 把 `__sess` 快照交还 `ReqState.ws_sess`（帧池状态外置回传）——
都是「先收集、帧末统一执行」（v0.1.7 起 `op_ws_frame_close` 改名，避开 deno_websocket
的同名 op）。HTTP 路径不读这些项，所以同一份 handler 代码在 HTTP 里调用 `ws.*`
等价 no-op。

**失败语义**（表）：

| 情形 | 行为 |
|---|---|
| handler 编译失败（文件缺失/语法错/全缺钩子） | Worker 预载失败**锁存**：该路由后续帧一律 PoolClosed → 发 Close 帧干净断连，不 panic（`frame_loop` 开头） |
| 单帧执行出错 | `eprintln` 记录，丢弃该帧，连接继续 |
| 单帧超时 | runtime 被 terminate 后毒化丢弃（不归还池），**该连接断开**；池自动补员，其它连接无感 |
| 写出通道满（cap 64） | `try_send` 满则**丢弃该帧**（含 bus 广播帧），不阻塞不崩溃 |

### sess 会话状态与帧池约束

- **必须可 JSON 序列化**：`sess.state` 每帧以 JSON 快照往返 Rust 会话表——函数、
  Symbol 等不可序列化值**静默丢失**；超大对象会放大每帧序列化成本（会话态 ≈2KB/连接
  是设计锚点）。
- **模块作用域 = Worker 本地只读缓存**：模块每 Worker 预载一次，W 个 Worker 各持
  一份模块实例——顶层变量跨帧**写入不可依赖**（下一帧可能由别的 Worker 执行，读到
  另一份副本）。可变跨帧状态一律 `sess.state`（连接级）或 kv/bus（路由级/跨连接）。
- **毒化半径 = 1**：帧超时只终止执行该帧的 runtime 与 Worker，会话表条目随连接断开
  清除，排队帧作废；池补员后其它连接照常服务。
- **连接闸门**：`ws.max_connections` 为全局并发连接上限（默认 1000，0=不限），超限
  upgrade 直接 503；`ws.idle_linger_ms`（默认 0）控制路由连接归零后 Worker 池的保活
  时长，到期退役、新连接 attach 复活。

## 5. 约定与红线

1. **文件名一律小写 `ws.ts` / `ws.js`**（`walk_files` 精确匹配文件名，大小写敏感；
   同目录并存 `.ts` 优先）。
2. **WS 端点不过 HTTP 前置管线**（§3），鉴权在帧内自己做。
3. **内存与连接数解耦**：V8 只驻留在 Worker（每路由 `ws.workers_per_route` 个），
   每连接只持会话态（≈2KB 量级）；容量规划看 `ws.max_connections` 闸门，连接数不再
   ≈ 常驻内存。
4. **V8 只在 ws-worker 线程碰**——`JsRuntime` `!Send`；连接侧 `frame_loop` 已全 Send
   跑在 axum runtime 上，但任何 JS 执行（含 `ws_connect` 预载）都必须发生在 Worker
   线程（`docs/modules/00-overview.md` 红线 2）。
5. 超时/出错的 VM 一律**丢弃不归还池**（全局红线 5）：毒化 Worker 连 runtime 一起
   弃置，池自动补员。

## 6. 测试

`server/src/ws.rs` 的 14 个单测与本文件一一对应，改实现前先读、改完必跑
（`cargo test -p server --lib ws`）：

| 用例 | 教学点 |
|---|---|
| `ws_echo_roundtrip_on_pinned_thread` | 裸 echo 链路（P5a），upgrade + 钉线程 |
| `js_route_runs_handler_per_frame` | 每帧执行（第二帧仍回信封） |
| `js_route_ws_send_order_and_close` | §2 顺序契约：`ws.send` 先、信封后、`ws.close` 终连 |
| `mirror_routes_mount_directory_ws` | §1 挂载 + bus 跨会话广播（HTTP publish → WS 收帧） |
| `mirror_routes_root_ws` | 根级 `ws.ts` → `{base}/ws`（无双斜杠） |
| `js_route_missing_handler_closes_quietly` | 编译失败 → 干净关闭 |
| `ws_bus_subscribe_receives_http_publish` | `bus.subscribe` 幂等与广播帧形状 |
| `ws_frame_publish_broadcasts_to_subscribers` | §2 帧内发布：帧内 `publish` 广播到他连 + 自回声 + 进房 = `connection` 钩子（无需 join 帧） |
| `js_route_error_hook_keeps_connection_alive` | 契约：`error(e)` 兜底钩子异常，之后连接继续 |
| `js_route_close_hook_fires_exactly_once` | 契约：`close()` 收尾恰好一次（客户端断 / `ws.close()` 统一） |
| `js_route_no_hooks_disconnects` | 契约：全缺钩子 → 连接建立即断 |
| `gate_rejects_over_limit_with_503` | §4 闸门：超限 upgrade 返 503，存量连接不受影响；0 = 不限 |
| `frame_pool_timeout_isolates_connections` | §4 毒化半径 = 1：超时只断该连接，其它连接无感，`sess.state` 不串 |
| `frame_pool_preserves_per_connection_order` | §4 per-conn 保序：同连接帧严格按序执行 |

手测冒烟用 §3 的三条命令即可。

## 7. 出站客户端：`new WebSocket`（v0.1.7 起）与 wss（v0.1.8 起）

前六章都是「**别人连进来**」；这一章反过来——handler / 长任务作为 **WS 客户端**
连出去取数。运行时注入标准 WHATWG `WebSocket` 全局（deno 官方 deno_websocket
实现）：`new WebSocket(url)`、`onopen / onmessage / onerror / onclose`、`send / close`。
任务文件与 HTTP handler 均可用（连接无 MQ poll 那样的 offset/ack 消费会话，故无
task 门禁）。

与服务端 `ws.ts` 的分工：

| | 服务端（§1–§2 `ws.ts`） | 出站客户端（本节） |
|---|---|---|
| 方向 | 客户端连 oj | oj 连别人 |
| 入口 | 目录镜像路由 `GET {base}/…/ws` | `new WebSocket(url)` 全局 |
| 每帧执行 | 生命周期钩子（`message` 每帧） | 你的 `onmessage` 回调 |
| 发帧 | `ws.send` / `json.ok`（收集制） | `socket.send(str)`（直发） |
| VM | 路由级帧池（W 个 Worker 共享执行） | 跑在所在 handler/任务的 VM 里 |

### 7.1 可运行案例：`sample/src/tasks/task_wsclient.ts`

任务连 **sample 自己**的 `/v1/api/news/ws` 订阅 "news"，收到 §2 那条
`bus.publish` 广播链路的帧——一条命令链条跑通「入站 + 出站」两个半边：

```bash
cargo run -p oj --release -- server -c sample/config.yaml --api-path sample/src
TOKEN=$(curl -s -X POST http://localhost:9778/v1/api/auth/login -H 'X-TENANT-ID: default' \
  -d '{"username":"demo","password":"demo1234"}' | jq -r '.data.access_token')
curl -X POST http://localhost:9778/v1/api/news -H "Authorization: Bearer $TOKEN" \
  -H 'X-TENANT-ID: default' -d '{"text":"hi"}'
# → 任务日志：ws frame {"topic":"news","data":{"text":"hi"}}；Ctrl-C → stopped
```

案例骨架（全文见 `sample/src/tasks/task_wsclient.ts`）与 MQ 消费任务
（[mq-tasks.md](mq-tasks.md)）同构，但 **重连不用手写**：

```ts
const ws = new WebSocket(url);
const opened = new Promise<void>((ok, err) => { ws.onopen = ok; ws.onerror = err; });
ws.onmessage = (e) => { frames.push(String(e.data)); wake?.(); wake = null; };
ws.onclose = () => { closed = "ws closed"; wake?.(); wake = null; };
await opened;
ws.send("{}");                    // 连上即已订阅（connection 钩子）；此帧触发 message（无则 no-op）
while (!tasks.stopping()) {
  if (closed) throw new Error(closed);   // 断连 → Crashed → 监督器重启 → 新实例重连
  if (frames.length) { log.info("ws frame " + frames.shift()); continue; }
  await Promise.race([new Promise(ok => (wake = ok)), tasks.sleep(250)]);
}
ws.close();                        // Stopped 出口：干净断连
```

**重连 = 崩溃监督**：`onerror`/`onclose` 置 `closed` → 下一轮抛错 → 任务
`Crashed` → 监督器指数退避重启（1s→2s→…cap 60s）→ 新实例重新建连。你只需要
「让断连可观测」，不需要 reconnect 循环。

### 7.2 wss 与鉴权的真话

- **wss**：v0.1.8 起 webpki-roots（Mozilla 根集）编译进二进制，经
  `FetchOptions.root_cert_store_provider` 注入——https `fetch` 与 wss 握手共用
  同一份（`op_ws_create` 从 OpState 读 `FetchOptions`）。`new WebSocket("wss://…")`
  开箱即用，无系统证书依赖。
- **鉴权**：WS 路由是真实路由、**不过** Bearer/租户前置管线（§3 红线），所以
  `anonymous_paths` 对它无效也无需配置；而 WHATWG `WebSocket` 又带不了自定义头
  （含 `Authorization`）——连第三方受保护 WS 端点时，token 只能走应用层：
  首帧透传或子协议（`new WebSocket(url, protocols)`）。

### 7.3 实现走读（维护者）

装配点 `bridge::ws_client_extensions`（`src/bridge/mod.rs`）按依赖序注册五个
deno 扩展：`deno_webidl → deno_web → deno_fetch → deno_net → deno_websocket`。
deno_websocket 的 JS 是 ESM（bootstrap.js `import` 挂 `globalThis.WebSocket`）；
而 fetch 相关 JS 是**经典脚本片**（`lazy_loaded_js`），只能经 `core.loadExtScript`
拉取——bootstrap.js 由此挂载 `fetch / AbortController / URL / URLSearchParams`。
三个嵌入坑（升级 deno_core 时先看这里）：

1. `26_fetch.js` 无条件 `loadExtScript("ext:deno_telemetry/…")`（deno CLI 宿主才有
   该扩展）——bootstrap 先垫 `internals.__telemetry{,Util}` no-op 桩，全部触点被
   `TRACING_ENABLED` 门禁短路；
2. `fetch` JS 依赖 `new URL`——`URL/URLSearchParams` 从 `deno_web/00_url.js` 拉;
3. rustls 双 CryptoProvider（deno_tls 的 aws_lc_rs vs reqwest 系 ring）——装配时
   显式 `install_default(aws_lc_rs)`，否则 `ClientConfig::builder()` panic。

回归：`src/bridge/mod.rs` 的 `fetch_*` 4 用例（WHATWG 语义：真 Headers、TypeError
文案、任意 token method 原样发出）与 `src/bridge/ws.rs` 的 `ws_client_tests`
（`WebSocket` 全局挂载 + tokio-tungstenite 回环）。
