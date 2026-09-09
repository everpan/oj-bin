# WS 运行时分片（RuntimeHost / K 会话组）设计（design）

- 日期：2026-09-09
- 状态：已拍板（脑暴定稿：用户选「直接最终形态」——K 会话 host 分片 + 引用计数管理）
- 前置脑暴：两位独立专家分析（架构 opus / 开发 sonnet，报告存 `.superpowers/brainstorm/ws-sharing/`，含内存探针与 800 连接压测数据）
- 关联：前作 v0.1.9 驻留会话（`docs/superpowers/specs/2026-09-09-ws-lifecycle-contract-design.md`）——本设计取代其「每连接独占 runtime」执行模型，**钩子契约（default 导出 + 全局 API）不变**

## 背景与动机

v0.1.9 现状：每个 WS 连接独占一个 JsRuntime（WsSession，永不还池）+ 专属线程。实测成本（两位专家独立测量互相印证）：

- **≈6.2-6.3 MB RSS/连接**（线性；100 连接 = 612 MB，1000 ≈ 6 GB），含每连接重复物化的 bootstrap/扩展 JS
- 每连接 3 线程（ws-js + current_thread tokio + 看门狗）≈ 0.1-0.3 MB + 线程数
- 连接建立每连接重编译 bootstrap/扩展 ESM（无 snapshot），9 ms boot
- **WS 当前无连接数上限**（ResourceLimiter 未接）——connect 洪水即可打满进程（与分片无关的现实漏洞，本设计一并修）

共享 isololate 的收益实测：每会话 JS 态 ≈ 2.2 KB（省 ~99%），且模块每 host 编译一次、连接建立延迟下降。

## 已拍板的决策

| 决策点 | 结论 | 依据 |
|---|---|---|
| 执行模型 | **每 ws 路由 → RuntimeHost 组**，每 host ≤ K 个活跃会话共享一个 JsRuntime | 用户拍板「直接最终形态」 |
| host 生命周期 | **引用计数**：会话独立入组/独立 detach；**refcount==0 → runtime drop、host 线程退出** | 用户拍板「所有 session 关闭之后，整个 runtime 消亡」 |
| 连接闸门 | **config 全局上限** `ws.max_connections`（默认 1000），超限拒绝 upgrade 返 503 | 用户拍板「全局上限」 |
| 超时语义 | **降级为组级**：组内任一会话帧超时/死循环 → terminate 整 isolate → 全组断开 + 组内 JS 态丢弃；host 不留僵尸（毒化后 refcount 归零自然消亡，新连接开新组） | v8 无 per-script budget、deno_core 0.411 无 per-promise deadline（专家全源码证实），数学上无解 |
| JS 会话状态 | **新增 `sess` API**（`sess.state` 每连接独立容器、`sess.id` 连接 id）；**「模块作用域 = 连接状态」契约废弃（breaking）** | 共享后模块作用域为全组共享 |
| 派发模型 | host 内**严格串行**（事件 FIFO + done-op 完成信号）；并发交错（~20 op connId 键控）范围外 | ops 层零改动的唯一形态（开发专家） |
| 否决项 | WebWorker 路线（deno_core 0.411 无；Deno 的实现也是每 worker 独立 isolate，不省内存）；多 context（JsRealm 已 pub(crate) 单 realm，create_realm 已删除）；每路由全共享单 host（=K 无上限，爆炸半径失控） | 开发/架构专家一致 |
| K 默认值 | `ws.sessions_per_host` 默认 **8**（可配；K=1 时语义严格等价 v0.1.9 独占） | 架构专家建议 K≈8-16 |

## 架构

### RuntimeHost（每 ws 路由 1..R 个）

```
RuntimeHost（专用线程 + current_thread tokio runtime + 1 个 JsRuntime + Bridge（含独立 KillSwitch））
  rx:   mpsc::Receiver<HostMsg>            // Send 入口：帧来自任意连接的 Reader
  conns: HashMap<ConnId, ConnSlot>         // ConnSlot { bus_tx, sends: Vec<String>, close: bool, done: Option<oneshot::Sender<..>> }
  loop（select）:
    Frame { conn, ev, body, done_tx }  => 取出该连接 bus_tx 写入 ReqState → reset →
                                          execute_script("__ws_call(conn_lit, ev_lit)") →
                                          run_event_loop 排空 → 收 ws_sends/ws_close/信封 → done_tx 回 WsOutcome
    Detach { conn }                    => conns.remove；conns.is_empty() → host 消亡（runtime drop、线程退出）
    毒化（看门狗 disarm==fired）        => 断开全组连接（逐 ConnSlot 发关闭）→ host 消亡（毒化 runtime 按红线丢弃，不重建）
```

- **选组**：连接到来 → 路由内找「存活且未满（< K）」的 host 加入，多组有空位时取**当前会话数最多**者（填满即消亡倾向，减少组数）；全满/无存活 → 开新 host（上限 `R_max = ceil(max_connections / K)`，天然受闸门约束）。
- **完成信号**：新增 `op_ws_event_done(conn_id)` op，dispatcher `__ws_call` 尾部 `finally` 调用——`run_event_loop` 排空不再等价于「单事件完成」（多会话 pending 交错）。串行形态下 ReqState/ws_sends/ws_close/bus_tx 沿用现状，**ops 层仅新增此一个 op**。
- **ConnHandle（每连接，Send）**：`{ tx: mpsc::Sender<HostMsg>, conn_id }`——`WsSession` 从「持有 runtime（!Send）」变为「持有句柄」。`fire(ev)` = 投递 + `done_rx.await`。
- frame_loop 瘦身：每连接 `ws-js` 专属线程与 current_thread runtime 撤销；Reader/Writer/bus-forwarder 三任务保留在连接侧（均 Send），Processor 换成 ConnHandle 投递。

### JS 侧契约（v0.2）

```ts
export default {
  connection() {
    sess.state.room = "lobby";        // sess.state：每连接独立对象（registry by connId）
    bus.subscribe("news");            // 订阅按连接通道去重，语义不变
    json.ok({ subscribed: true });    // 回帧契约不变
  },
  message() { /* http.body / ws.send / sess.state / bus 全局用法不变 */ },
  error(e) { /* 语义不变：兜底后连接继续 */ },
  close()  { /* 语义不变：三来源统一、恰好一次 */ },
};
```

- `sess.id`：连接 id（ConnId，host 内唯一；跨 host 不保证唯一，仅日志/关联用）。
- dispatcher（host 启动时装一次）：`globalThis.__ws_hooks` + `__ws_call(conn, ev)` + `globalThis.__ws_sessions = new Map()`（sess 代理）。
- **breaking**：模块作用域跨帧存「连接状态」的写法失效（同组会话共享同一模块实例）；跨组/跨连接共享仍走 kv/bus（不变）。

### 超时与毒化（组级，明示契约）

- 看门狗语义不变（每事件 arm/disarm），但目标 isolate 上有 K 个会话：**毒化 = 全组断连 + 组内 JS 态丢弃**。
- host 不重建：毒化 runtime 丢弃 → 全组 Detach → refcount 归零 → 消亡；新连接开新组（spawn+boot ≈ 9ms，一次性）。
- 文档明示：JS 侧内存炸弹/死循环无单会话隔离，关键状态放 kv/bus，`sess.state` 只存可重建态；客户端按 WS 惯例自动重连。

## 配置面（config.yaml）

```yaml
ws:
  max_connections: 1000        # 全局并发连接上限，超限 upgrade 返 503；0 = 不限制（默认 1000）
  sessions_per_host: 8         # K：每 host 会话上限（1 = 严格退化 v0.1.9 语义）
  host_linger_ms: 0            # 组空后保活毫秒数（默认 0 = 立即消亡；churn 场景可调大吃暖启动收益）
```

## 迁移面

| 文件 | 改动 |
|---|---|
| `src/bridge/mod.rs` | 新增 `RuntimeHost`/`ConnHandle`/`HostMsg`/`ConnSlot`；`op_ws_event_done` 注册；`ws_connect` 拆为 host 启动时装 driver + `acquire(conn)`；`WsSession` 退役（或变别名） |
| `server/src/ws.rs` | frame_loop Processor → ConnHandle 投递；`conn_on_pinned` 每连接线程撤销；`js_route`/`mirror_routes` 从 `make_bridge` 工厂改 host 池工厂 |
| `oj/src/server_cmd.rs` / `oj/src/app.rs` | 装配点：每 ws 文件建 host 池（读 config ws 段）；闸门中间件（upgrade 前计数） |
| `sample/src/news/ws.ts`、`chat/ws.ts` | 模块作用域状态迁移 `sess.state`；文档注释同步 |
| docs（api-manual/websocket.md/dev-guide/user-manual/SKILL/MODULES） | sess API、组级超时契约、配置面、breaking 标注 |
| CHANGELIST | v0.2 段（breaking） |

### 失效/重写测试（现状 70 server + 4 bridge ws_session）

- 「模块作用域=连接状态」依赖用例：`ws_frame_publish_broadcasts_to_subscribers` 等 → 改 `sess.state` 或改断言语义
- 每连接线程/!Send 相关结构断言 → 移除
- 新增钉：refcount 消亡（全组 close → runtime 释放）、分片上限（K 满开新 host）、组级毒化（一分片超时 → 同组断、他组无恙）、闸门 503、sess.state 跨帧隔离（同 host 两会话 state 互不可见）、linger
- 保持语义（fire 契约不变应继续可过，仅装配改 host 工厂）：echo 信封、send 顺序+close、error 兜底、missing handler、无导出断连、根级 ws

## 验收基准

- 内存：100 并发连接 RSS 增量 ≤ 64 MB（K=8，对照组现状 ≈ 612 MB）
- 语义：上表「保持语义」用例全绿；新增钉全绿
- 闸门：超限连接被拒且存量连接不受影响

## 范围外（YAGNI，记录即走）

- 并发交错派发（op connId 键控 + db.tx 归属重设计）——等串行 HOL 成为实测瓶颈
- host 跨路由共享、路由间负载均衡、runtime snapshot 预热
- 每路由独立连接上限（已拍板全局一层）
- 横向扩容（bus kafka/rabbitmq 已支持，运维课题）

## 已知天花板

- `ponytail:` 串行派发 = 单 isolate ~5k 帧/s 上限 + 慢 handler 阻塞同组（现状只阻塞自己）——切片 3（并发键控）是升级路径。
- `ponytail:` 组级毒化语义是产品级降级，文档必须显眼；不接受「悄悄断一群」。
