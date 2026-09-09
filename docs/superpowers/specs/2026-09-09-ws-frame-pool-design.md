# WS 运行时帧池（Frame Queue + 无状态 Worker 池）设计（design）

- 日期：2026-09-09
- 状态：已拍板（用户提出并采纳「帧池解耦」终稿，取代同日先前的「K 会话/host 分组」稿）
- 前置脑暴：两位独立专家分析（架构 opus / 开发 sonnet，报告存 `.superpowers/brainstorm/ws-sharing/`，含内存探针与 800 连接压测数据）
- 关联：前作 v0.1.9 驻留会话（`docs/superpowers/specs/2026-09-09-ws-lifecycle-contract-design.md`）——本设计取代其「每连接独占 runtime」执行模型；**钩子契约（default 导出 + 全局 API）不变**

## 背景与动机

v0.1.9 现状：每 WS 连接独占一个 JsRuntime + 专属线程。实测（两专家独立测量互相印证）：

- **≈6.2-6.3 MB RSS/连接**（线性；100 连接 = 612 MB，1000 ≈ 6 GB）
- 每连接 3 线程（ws-js + current_thread tokio + 看门狗）≈ 0.1-0.3 MB + 线程数
- 连接建立每连接重编译 bootstrap/扩展 ESM（9 ms boot，无 snapshot）
- **WS 无连接数上限**（ResourceLimiter 未接）——connect 洪水可打满进程（一并修）

曾被否决的「共享 isolate」方案（每路由 1 VM / K 会话分组）的死结：`terminate_execution` 是 isolate 级原子操作（v8 无 per-script budget、deno_core 0.411 无 per-promise deadline），状态住在 isolate 里则「单会话超时」必然放大为「组击杀」。**帧池模型把状态搬出 isolate，死结不成立**：

- 帧是无状态作业，进队列；W 个无状态 Worker 各自从队列取帧执行
- 毒化只杀「正在执行那一帧的 runtime」，队列中其它帧、其它连接无感 → **爆炸半径 = 1 帧**
- 会话状态外置到 Rust 侧会话表，每帧以 JSON 快照进出 isolate

## 已拍板的决策

| 决策点 | 结论 | 依据 |
|---|---|---|
| 执行模型 | **每 ws 路由 → 帧队列 + W 个无状态 Worker**（各 1 线程 + 1 个暖 JsRuntime，路由模块预载） | 用户提出并拍板「帧池解耦」 |
| 会话状态 | **Rust 侧会话表持有 `sess.state`（JSON）**；每帧注入 worker `__sess` 槽、done-op 带回写表；JS API 面（`sess.state`/`sess.id`）不变 | 状态外置是解开爆炸半径的前提 |
| 超时语义 | **每连接语义回归**：帧超时 → 该 Worker 毒化丢弃 + 该帧作废 + **该连接断开**（契约同 v0.1.9）；池异步补员，其它连接/帧无感 | terminate 只影响执行帧的 isolate |
| 保序 | **per-conn 在飞 = 1**：同连接帧严格 FIFO 串行（WS 语义），不同连接的帧在 W 个 Worker 上真并行 | WS 保序要求 |
| worker 生命周期 | 路由队列空 + 连接归零 + `ws.idle_linger_ms`（默认 0 = 立即退役）到期 → Worker 池消亡（runtime drop、线程退出）；毒化即时补员 | 「无会话则 runtime 消亡」规则在池模型下的映射 |
| 连接闸门 | **config 全局上限** `ws.max_connections`（默认 1000，0 = 不限），超限拒绝 upgrade 返 503 | 前轮拍板 |
| 派发/完成信号 | 严格串行 per Worker（一次一帧）；Worker 即执行者，帧完成自明（排空 event loop 后直读 ReqState 收 sends/close/capture）；`op_ws_sess_set` 由 dispatcher `finally` 把 `__sess` 回传 `ReqState.ws_sess` | ops 层仅新增此一个 op + `ReqState.ws_sess` 一个字段 |
| 否决项（存档） | 每路由全共享 1 VM / K 会话分组（被帧池取代：组级毒化 + HOL 串行两项全劣）；WebWorker（deno_core 0.411 无；Deno 的也是每 worker 独立 isolate，不省内存）；多 context（JsRealm 已 pub(crate) 单 realm）；并发交错 op 键控（帧池下无必要——并行由多 Worker 天然提供） | 专家分析 + 用户决策 |
| W 默认值 | `ws.workers_per_route` 默认 **2**（可配；吞吐随 W 近线性扩展） | 起步保守，按 CPU 调 |

## 架构

```
per ws 路由（ws.ts 文件）:
  FrameQueue / Scheduler
    push(Frame{conn, ev, body, done_tx})     // Reader 投递
    per-conn 在飞=1：连接上一帧 done 前，下一帧在队列等待（保序）
    conn detach → 丢弃其排队帧
  W × Worker（1 线程 + current_thread tokio + 1 JsRuntime + Bridge（独立 KillSwitch））
    启动：checkout → 预载路由 ws.ts（装 __ws_hooks/__ws_call/__sess 槽）→ 就绪
    loop: 取 Frame → ReqState.reset(带 bus_tx) → 注入 __sess(JSON) →
          execute_script("__ws_call(conn_lit, ev_lit)") → 排空 event loop →
          直读 ReqState（sends/close/capture）+ __sess 经 op_ws_sess_set 回传 →
          回写 Rust 会话表 → done_tx 回连接侧 → 取下一帧
    毒化（看门狗 fired）：丢弃 runtime（红线：不还池）→ 该帧作废 + 该连接断开 →
          通知池补员（spawn 新 Worker，异步）
  Rust 会话表：HashMap<ConnId, SessEntry { state: Value, bus_tx, … }>
    连接建立建条目；close 流程走完 → detach → 删条目
```

- **选帧**：Worker 从队列取「其连接无在飞帧」的队头（调度器保证）。
- **连接建立**：暖池窗口内 = 纯 Rust 登记（会话表条目 + Reader/Writer 启动），零模块加载；池为**懒启动**（首次 attach 时 spawn+boot 一次 ≈9ms），空池按 linger 退役后首个连接重建——「零加载」是 linger 窗口内的性质。
- **dispatcher**（每 Worker 预载一次）：`__ws_hooks` + `__ws_call(conn, ev)`（尾部 `finally` 调 done-op）+ `__sess` 单槽代理。

### JS 侧契约（v0.2）

```ts
export default {
  connection() {
    sess.state.room = "lobby";      // Rust 侧会话表持久；每帧快照进出
    sess.id;                        // 连接 id
    bus.subscribe("news");          // 订阅按连接通道去重，语义不变（bus_tx 随帧走）
    json.ok({ subscribed: true });  // 回帧契约不变
  },
  message() { /* http.body / ws.send / sess.state / bus 用法不变 */ },
  error(e) { /* 语义不变：兜底后连接继续 */ },
  close()  { /* 语义不变：三来源统一、恰好一次（在该连接在飞=0 时执行） */ },
};
```

- **breaking（必须文档显眼）**：
  1. 「模块作用域 = 连接状态」（v0.1.9）→ **模块作用域 = Worker 本地只读缓存**（W 份副本、非确定归属）。跨帧可变状态用 `sess.state`，路由级可变状态用 kv/bus。
  2. `sess.state` 必须可 JSON 序列化（函数/DOM 等不可序列化值静默丢失）。
- globalThis/原型污染跨会话残留 = HTTP 池现状同级（既接受风险，非新增）。

## 配置面（config.yaml）

```yaml
ws:
  max_connections: 1000        # 全局并发连接上限，超限 upgrade 返 503；0 = 不限制（默认 1000）
  workers_per_route: 2         # W：每路由 Worker 数（默认 2）
  idle_linger_ms: 0            # 路由连接归零后 Worker 池保活毫秒数（默认 0 = 立即退役）
```

## 迁移面

| 文件 | 改动 |
|---|---|
| `src/bridge/mod.rs` | 新增 `FrameQueue`/`Scheduler`/`Worker`/`ConnHandle`/会话表；`op_ws_event_done`（带回 sends/close/capture/sess_state）注册；`ws_connect` → Worker 预载路径；`WsSession` 退役 |
| `server/src/ws.rs` | frame_loop Processor → 投递队列 + done_tx；`conn_on_pinned` 每连接线程撤销；`js_route`/`mirror_routes` 改持路由 Worker 池 |
| `oj/src/server_cmd.rs` / `oj/src/app.rs` | 装配：读 config ws 段建路由池；闸门中间件（upgrade 前全局计数，超限 503） |
| `sample/src/news/ws.ts`、`chat/ws.ts` | 状态迁移 `sess.state`；注释同步 |
| docs（api-manual/websocket.md/dev-guide/user-manual/SKILL/MODULES） | 帧池模型、sess 约束（可序列化）、模块作用域新语义、配置面、超时契约（每连接回归）、breaking 标注 |
| CHANGELIST | v0.2 段（breaking） |

### 测试改造（现状 70 server + 4 bridge ws_session）

- 改写：依赖「每连接独立模块实例」的用例（如 `ws_frame_publish_broadcasts_to_subscribers`）→ `sess.state` 版
- 新增钉：
  - **毒化半径 = 1**：worker A 超时（死循环帧）→ 仅该连接断，同路由其它连接照常收发，池自动补员后新帧可达
  - **per-conn 保序**：同连接连发 N 帧 → 顺序处理（在飞=1）；不同连接并发交错
  - **sess.state 外置**：同 host 两会话 state 互不可见；跨帧持久；不可序列化值静默丢弃的文档钉
  - **池退役**：连接归零 + linger=0 → worker 消亡（可探测线程数/日志）；新连接重建
  - **闸门**：超限 503、存量连接不受影响、0 = 不限
  - 保持语义（应继续可过，装配改池工厂）：echo 信封、send 顺序+close、error 兜底、missing handler、无导出断连、根级 ws、close 恰好一次

## 验收基准

- **内存平坦**：稳态 RSS 随连接数近线性 → 近平坦；1000 连接与 100 连接的稳态 RSS 差 ≤ 8 MB（会话表 2.2 KB/连接 + 余量）
- 连接建立：无模块编译（对齐「纯登记」语义）
- 语义：毒化半径钉、保序钉、外置钉、闸门钉全绿；「保持语义」清单全绿
- 吞吐：W=2 时双连接帧处理可并行（并行钉）

## 范围外（YAGNI，记录即走）

- Worker 跨路由共享、动态伸缩 W、runtime snapshot 预热
- 帧优先级/公平调度（FIFO 够用）
- sess.state 结构化 schema 校验
- 横向扩容（bus kafka/rabbitmq 已支持，运维课题）

## 已知天花板

- `ponytail:` per-conn 在飞 = 1：慢 handler 阻塞的是**自己连接**的后续帧（同 v0.1.9），不阻塞他人——若某路由需单连接高吞吐，属业务反模式。
- `ponytail:` sess.state 快照往返是 O(state) 每帧拷贝——state 应保持小（KB 级）；大状态是 kv 的活。
- `ponytail:` 模块作用域 W 份副本的「只读缓存」纪律无引擎强制，靠文档约定（与 HTTP 池现状同级）。
