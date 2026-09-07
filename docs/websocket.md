# WebSocket 教学（ws.ts 帧循环）

> 面向两类读者：**要写 WS handler 的业务开发者**（§1–§3）与**要改 WS 实现的维护者**
> （§4–§6）。JS 签名权威是 [devkit/api-manual.md](devkit/api-manual.md)（§4 `ws.ts` 小节、
> §6 `ws`/`bus` 行）；维护者模块地图是 [modules/03-server-http.md](modules/03-server-http.md) §5。
> 可运行示例全程以 `sample/src/news/` 为锚。

## 1. 心智模型：一个文件 = 一条 WS 路由 = 每帧执行一次

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
| 执行单元 | `default[method]()` 一个函数 | **整个文件**（顶层代码每帧重跑一遍） |
| 回写 | `{code,msg,data}` 信封 HTTP 响应 | 信封文本帧 + `ws.send` 裸帧 |
| 运行位置 | HTTP actor 池（`server.pool_size` 个 VM 排队复用） | **每连接独占一个 VM**（不进池，连接结束即弃） |

帧进来时，注入的请求上下文是：`http.method === "WS"`、**`http.body` = 帧字节**
（`Uint8Array`）；`http.query`/`http.headers` 为空对象（upgrade URL 未透传）。

## 2. 写一个 handler：sample/news 逐行

`sample/src/news/ws.ts` 全文只有两行：

```ts
bus.subscribe("news");   // ① 订阅 "news" 主题（幂等，见下）
json.ok({ subscribed: true });   // ② 回一帧标准信封 {"code":0,"data":{"subscribed":true}}
```

- **① 每帧都会执行**，为什么不会重复订阅？——`Bus::subscribe`（`src/bridge/bus.rs:55`）
  按发送端通道去重（`same_channel`）：同一连接的每帧携带的是同一个通道的克隆，
  第二次起是 no-op。所以「顶层 subscribe」这个写法是安全的，不需要你自己记状态。
- **②** 帧内 `json.ok`/`json.fail` 与 HTTP 语义一致：整段 `{code,msg,data}` 序列化成
  **一个文本帧**写回。

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

- **单帧超时**：一帧卡死（如死循环）只熔断这一帧——该帧被丢弃、当帧的 VM 被丢弃，
  **连接继续**（默认 30s，`oj/src/app.rs` 传给 `mirror_routes`）。
- **顺序契约**：写出顺序 = `ws.send` 按调用序 → 信封 → （后续广播帧）。广播帧与
  主动发送走同一条写出通道，天然保序（`server/src/ws.rs` Bus forwarder）。

### 帧内发布：`ws.ts` 里直接 `bus.publish`

`bus.publish` 没有上下文限制（只有 `bus.subscribe` 限 WS 连接），所以帧处理器可以
直接当「广播泵」用。可运行案例：`sample/src/news/chat/ws.ts`——聊天室，任意连接
发帧，所有订阅者收广播：

```ts
bus.subscribe("chat");   // 幂等（同通道去重），每帧重跑无害
{
  const frame = http.body;              // JSON 文本帧已自动 parse 成对象
  if (frame && frame.text) {
    bus.publish("chat", { from: frame.from ?? "anon", text: frame.text });
    json.ok({ sent: true });
  } else {
    json.ok({ joined: true });
  }
}
```

```bash
# 终端 1 / 2 各连一处，各发一帧 {"join":1}；任一终端再发 {"from":"neo","text":"hi"}
websocat ws://localhost:9778/v1/api/news/chat/ws
# → 两个终端都收到 {"topic":"chat","data":{"from":"neo","text":"hi"}}
```

帧内发布特有的三条语义：

1. **自回声**：本连接若订阅了同一主题，会收到自己发布的广播帧（Bus fan-out 不排除
   自己）——按 `from` 字段客户端过滤，或发布到别的 topic。
2. **顶层不能 `await`**：帧代码经 `execute_script` 执行（经典 script，非 ESM）。
   不 await 也会照常广播（event loop 会把 op future 驱动完才捕获信封）；要拿
   `publish` 返回的「本地接收方数」，用 async IIFE 包住 `await bus.publish(...)`
   （kafka/rabbitmq 下该数恒 0）。
3. **声明放进块作用域**：同一连接的帧跑在**同一个 VM** 里，顶层 `const`/`let`
   第二帧重跑会因重复声明报 `SyntaxError`——示例外层那对 `{}` 是必须的。

回归：`ws_frame_publish_broadcasts_to_subscribers`（§6）。

## 3. 跑通 sample

```bash
# 终端 1：dev 模式启动（服务 src，按需转译）
cargo run -p oj -- server -c sample/config.yaml --api-path sample/src

# 终端 2：连上并发任意一帧完成订阅（websocat 任选你顺手的客户端）
websocat ws://localhost:9778/v1/api/news/ws
# > hi
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
oj/src/app.rs            装配：mirror_routes(base, dir, timeout, make_bridge) merge 进 Router
server/src/ws.rs         连接生命周期：upgrade → 钉线程 → 三任务流水线
src/bridge/{mod,ws}.rs   帧执行：run_ws → ReqState 收集 → WsOutcome
```

**挂载**（`server/src/ws.rs:46` `mirror_routes`）：递归扫 `ws.ts`（优先）/`ws.js`
（`ws_files`，同目录并存时 `.ts` 胜），每个文件一条 `GET …/ws` 路由。源码与
`api.ts` 共用同一套 `transpile::cached_transpile`（mtime 缓存、TS 剥类型）。

**线程模型**：`JsRuntime` 是 `!Send` 的，而 axum 的 upgrade future 在多线程 reactor
上。所以 `conn_on_pinned` 把整个 socket 搬到**专用 OS 线程**（`ws-js`），线程内建
`current_thread` runtime，连接的一生都在这条线程上。每连接 `make()` 一个独立
`Bridge`（独占 VM，不走 HTTP actor 池），连接结束即整体丢弃——不存在归还与复用。

**三任务流水线**（`frame_loop`，通道各 cap 64 做背压）：

```
socket ──Reader──▶ msgChan(64) ──Processor(当前任务，串行 run_ws)──▶ respChan(64) ──Writer──▶ socket
                                                       ▲
bus 广播帧 ──────────────Bus forwarder（unbounded）────┘   （与 ws.send 同通道 → 保序）
```

- **Reader**：文本/二进制帧 → `msgChan`；满了背压到 TCP 层（对端 send 变慢）。
- **Processor**：每帧组一个 `RequestInfo { method: "WS", body: 帧字节, bus_tx }` 跑
  `run_ws`；成功后把 `o.sends`（`ws.send` 收集）逐条、再把 `o.capture.body`（信封）
  压进 `respChan`。`o.close` 置位则跳出循环。
- **Writer**：串行写回；`respChan` 排空后发 Close 帧。ping/pong 由 axum 自动处理。
- **Bus forwarder**：把本连接订阅的广播帧转进同一条 `respChan`。连接结束时
  `forwarder.abort()` 收尾——否则 bus 订阅表里的发送端滞留，Writer 永不排空。

**op 层**（`src/bridge/ws.rs`，仅两个）：`op_ws_send` 把字符串 push 进
`ReqState.ws_sends`，`op_ws_close` 置位 `ReqState.ws_close`——都是「先收集、帧末统一
执行」。HTTP 路径不读这两项，所以同一份 handler 代码在 HTTP 里调用 `ws.*` 等价 no-op。

**失败语义**（表）：

| 情形 | 行为 |
|---|---|
| handler 编译失败（文件缺失/语法错） | 发 Close 帧后结束连接，不 panic（`frame_loop` 开头） |
| 单帧执行出错 | `eprintln` 记录，丢弃该帧，连接继续 |
| 单帧超时 | 该帧 VM 被 terminate 后丢弃（不归还池），连接继续 |
| 写出通道满（cap 64） | `try_send` 满则**丢弃该帧**（含 bus 广播帧），不阻塞不崩溃 |

## 5. 约定与红线

1. **文件名一律小写 `ws.ts` / `ws.js`**（`walk_files` 精确匹配文件名，大小写敏感；
   同目录并存 `.ts` 优先）。
2. **WS 端点不过 HTTP 前置管线**（§3），鉴权在帧内自己做。
3. **每连接一个 VM**：连接数 ≈ 常驻内存上限（每个 V8 isolate 数 MB 级），容量规划按
   连接数而非 QPS。
4. **别把帧处理移回 axum 线程**——`JsRuntime` `!Send`，现有钉线程 + 流水线是唯一
   正确姿势（`docs/modules/00-overview.md` 红线 2）。
5. 超时/出错的 VM 一律**丢弃不归还池**（全局红线 5，WS 的每帧独占 VM 天然满足）。

## 6. 测试

`server/src/ws.rs` 的 7 个单测与本文件一一对应，改实现前先读、改完必跑
（`cargo test -p server --lib ws`）：

| 用例 | 教学点 |
|---|---|
| `ws_echo_roundtrip_on_pinned_thread` | 裸 echo 链路（P5a），upgrade + 钉线程 |
| `js_route_runs_handler_per_frame` | 每帧执行 + VM 复用（第二帧仍回信封） |
| `js_route_ws_send_order_and_close` | §2 顺序契约：`ws.send` 先、信封后、`ws.close` 终连 |
| `mirror_routes_mount_directory_ws` | §1 挂载 + bus 跨会话广播（HTTP publish → WS 收帧） |
| `mirror_routes_root_ws` | 根级 `ws.ts` → `{base}/ws`（无双斜杠） |
| `js_route_missing_handler_closes_quietly` | 编译失败 → 干净关闭 |
| `ws_bus_subscribe_receives_http_publish` | `bus.subscribe` 幂等与广播帧形状 |
| `ws_frame_publish_broadcasts_to_subscribers` | §2 帧内发布：帧内 `publish` 广播到他连 + 自回声 + 块作用域重跑安全 |

手测冒烟用 §3 的三条命令即可。
