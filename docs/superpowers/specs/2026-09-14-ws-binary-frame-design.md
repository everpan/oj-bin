# WS 二进制帧支持设计（v0.1.16）

日期：2026-09-14
状态：已实现（用户拍板全量：bus ABI 8 本期 + client.ws 完整测试面）
关联：`src/bridge/{mod,ws,http,bus,frame_pool,ffi}.rs`、`server/src/ws.rs`、
`oj-plugin-ffi/src/{lib,bus,mq}.rs`、`plugins/oj-bus-{kafka,rabbitmq}/src/lib.rs`、
`oj/src/{test_ext.rs,test_cmd.rs,app.rs}`；PRD
`plane/docs/ever/prd/oj-feature-request-ws-binary.md`

## 1. 背景与需求

Plane 后端选型 oj，其 Yjs/Hocuspocus 协同文档子系统依赖 WS 二进制帧。现状两处窒息点：

1. 入侧：`server/src/ws.rs` reader 把 Text/Binary 都坍缩成 `Vec<u8>`，`http.rs` 的
   `export_bytes` 对非 JSON 载荷 `from_utf8_lossy`——二进制帧到达 handler 时已被破坏。
2. 出侧：String 管道四连（`op_ws_send #[string]` → `ws_sends: Vec<String>` →
   `WsOutcome.sends` → resp 通道 → `Message::Text`）——`ws.send` 只收 string。

需求：入帧二进制透传；出帧 `ws.send(Uint8Array)` → Binary；`sess.state` 语义不变；
`bus.publish` 支持二进制（用户拍板本期做，ABI 7→8）；`oj test` 提供 client.ws 帧测试面
（用户拍板完整做）。

## 2. 核心设计：WsSend 统一枚举

```rust
#[derive(Clone, Debug, PartialEq)]
pub enum WsSend { Text(String), Binary(Vec<u8>) }
```

贯穿全部帧出口：`ReqState.ws_sends` / `WsOutcome.sends` / WS resp 通道 / bus 通道 /
`DELIVER_TARGETS`。Writer 与 bus forwarder 各做一次 `match`：

```rust
WsSend::Text(t) => Message::Text(t.into()),
WsSend::Binary(b) => Message::Binary(b.into()),
```

opcode 位在 reader 处一次性捕获（`msg_tx: mpsc<(Vec<u8>, bool)>`），沿
`Frame.binary` → `RequestInfo.body_binary` 传播到 handler 上下文。

## 3. 入帧（需求 1）

- reader：`Text → (bytes, false)`、`Binary → (bytes, true)`。
- `Frame`（帧池载荷）加 `binary: bool`；worker 填进 `RequestInfo.body_binary`
  （HTTP 路径恒 false，行为不变）。
- `op_http_info`：`body_binary` 时 `"body": null`——不给 handler 误用 lossy 垃圾的机会。
- 新 op `op_http_body_bytes()`：async + `#[buffer]` 返回（`op_http_file` 契约：sync
  buffer-return 在 fast-call 路径卡死）→ 原始帧字节（任意帧型，空帧 → 空数组）。
- bootstrap.js http proxy 挂 `bodyBytes` 分支。

## 4. 出帧（需求 2）

- 新 op `op_ws_send_bin(#[buffer] JsBuffer)`（buffer 入参 sync 无碍，`op_blob_put` 契约）
  → `ws_sends.push(WsSend::Binary)`。
- bootstrap.js `ws.send`：`typeof data === "string" ? op_ws_send(String) :
  op_ws_send_bin(ArrayBuffer → new Uint8Array 包一层)`——去掉旧 `String()` 强转。
- 帧型由 JS 参数类型决定，global.d.ts 收窄为 `send(data: string | Uint8Array)`。

## 5. bus 字节化（需求 4，ABI 7→8）

### wire 约定（两 broker 一致）

| JS publish | record payload（kafka/rabbitmq） | 订阅 WS 会话收到 |
|---|---|---|
| JSON 值 | `{"topic","data"}` 信封 UTF-8（现状不变，旧生产者兼容） | Text 帧 |
| Uint8Array/ArrayBuffer | 原始字节 | Binary 帧（原字节，不包信封） |

消费侧判定（host `envelope_or_binary` 启发式）：payload 为 UTF-8 且解析为含 `topic`+`data`
键的 JSON 对象 → `WsSend::Text(原文)`；否则 → `WsSend::Binary(原字节)`。

**启发式误判面（已知、无害）**：二进制载荷恰为该形状 JSON 时以 Text 帧投递——内容无损，
订阅方可自辨。选启发式而非带外帧型标记的原因：kafka/rabbitmq record 无可靠帧型旁路
（headers 可用但旧生产者缺省不写，反而制造两套语义）。

### FFI 变更（ABI 8，严格相等门禁）

- `EventBrokerVtable.publish(handle, topic: RString, data: RBytes)`——data 从 RString 改
  `RBytes`（stabby `vec::Vec<u8>`，类型已存在，`to_rbytes` 元素级拷贝构造）。
- `HostContext.deliver(topic: RString, payload: RBytes)`——同上。
- `ABI_VERSION = 8`；插件与宿主不同版本 → 加载期 fail-fast
  （`plugin ABI mismatch: plugin=7 host=8`），重编插件即解。
- 本地 `Bus`（进程内）无 wire，`Bus::publish(topic, &BusPayload)` 直接分派
  `WsSend::Text/Binary`——JS 侧类型已知，无启发式。

### mq 轴（零 ABI，词汇表扩展）

`MqMessage` 明确非 repr(C)（JSON method dispatch），二进制走 serde 词汇表：

- send：`value_b64: Option<String>`（`skip_serializing_if`）——JS 传 Uint8Array 时
  bootstrap.js 纯 JS base64 编码（运行时无 btoa/atob，10 行查表实现，node 交叉验证）
  填 `value_b64`，插件解码为 record 原始字节。
- poll：载荷非 UTF-8 且非 JSON → `value = null`、`value_b64 = base64`；其余现状不变。

## 6. client.ws（验收项 2）

`oj test` 的 oneshot 派发止步于 101 upgrade，收发不了帧。新测试面：

- `App::router()` 访问器（Router clone）。
- 首次 `client.ws(path).send/next` 时（`op_client_ws_open`）：`127.0.0.1:0` bind +
  `tokio::spawn(axum::serve(listener, app.router()))`，tokio-tungstenite 连
  `ws://127.0.0.1:{port}{base}{path}` → 连接表 `HashMap<u64, WsStream>`（OpState）。
- `op_client_ws_send` / `op_client_ws_send_bin(#[buffer])`：发 Text/Binary 帧。
- `op_client_ws_next(id, timeout_ms)`：timeout 包裹读帧 →「最后一帧」槽位
  `(binary, bytes)` + `{frame, binary}` 元数据；`op_client_ws_last_bytes()` async
  `#[buffer]` 取字节。JS 组装 `{binary, data} | {closed: true} | null`。
- 借位纪律（修正 #4）：await 前把流从 OpState 表 remove，完事 insert 回——不持 Ref 跨 await。
- current_thread 共存：serve/tungstenite/帧池全部是同运行时上的 async 任务，JS 在
  op await 点位让路，顺序调用无死锁（`ws-bin.test.ts` 41/41 坐实）。

## 7. sess.state（需求 3，纯文档）

语义不变。手册写明：二进制状态（Yjs awareness 等）走 base64 字符串存 `sess.state` 或 kv。

## 8. 测试与验收

- `server/src/ws.rs`：`WsClient` 泛化为 `send_frame/read_frame`（opcode 断言）+
  `send_binary`；`js_route_ws_binary_echo_roundtrip`（0x82 入 → 0x2 出，含非 UTF-8 字节
  相等）；`ws_bus_binary_publish_reaches_subscriber`（publish Uint8Array → 0x2 原字节）。
- `src/bridge/bus.rs`：`bytes_payload_delivers_binary_frame_raw`（本地 Bytes → Binary）。
- `src/bridge/ffi.rs`：`bus_deliver_non_utf8_payload_arrives_as_binary_frame`；
  既有 deliver/publish 用例随枚举改型（`{"x":1}` 非信封 → Binary 断言钉死启发式）。
- L1：`sample/src/echo-bin/`（ws.ts 回显）+ `sample/tests/ws-bin.test.ts`
  （client.ws 发 [1,2,3] 与 [0,159,255] 字节相等 + 文本帧路径）——`oj test` 41/41。
- 门禁：`cargo test --release --workspace -- --skip infinite_loop` 全绿；
  `cargo xtask plugin oj-bus-kafka --check` ABI 8 预检通过。

## 9. 兼容性

- ABI 7 插件在 ABI 8 宿主下拒载（设计内，`cargo xtask build` 重编即解）。
- JSON publish/subscribe、文本帧路径行为逐字节不变（既有用例全数通过）。
- 旧 JS `ws.send(string)` 不受影响；global.d.ts 类型收窄仅影响新代码。
