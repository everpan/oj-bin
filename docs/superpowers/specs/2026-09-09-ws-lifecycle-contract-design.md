# WS 生命周期钩子契约（design）

- 日期：2026-09-09
- 状态：已评审通过（脑暴定稿）
- 影响面：`src/bridge/mod.rs`（WsSession）、`server/src/ws.rs`（frame_loop）、`sample/src/news/ws.ts`、server ws 测试、docs

## 背景与目标

现状：ws.ts 整文件作为经典 script **每帧重跑**（`Bridge::run_ws`：checkout → `execute_script` → 读捕获 → checkin，`src/bridge/mod.rs:667`；`server/src/ws.rs:202` Processor 每帧 `run_ws`）。后果：

- 模块内状态每帧归零，跨帧状态只能挤进 kv/bus；
- `bus.subscribe` 每帧重复执行；
- 异常仅 `eprintln` 丢帧（`server/src/ws.rs:221`），无 error 兜底；
- 无 connection/close 生命周期概念，欢迎帧/离帧只能靠「首帧是文件开头」的巧合模拟。

目标契约：ws.ts 以 **ESM default 导出钩子对象**（与 api.ts 的 `export default { get, post, ... }` 同构，driver 亦复用 `m.default[key]` 查找模式），server 端**模块加载一次、按事件调用**：

```ts
export default {
  connection() { /* upgrade 后恰好一次；bus.subscribe 在此 */ },
  message()    { /* 每帧一次；http.body 读帧 */ },
  error(e)     { /* 任一钩子抛异常时兜底；之后连接继续 */ },
  close()      { /* 收尾恰好一次（客户端 Close / socket 断 / ws.close() 三来源统一） */ },
};
```

## 已拍板的决策（2026-09-09）

| 决策点 | 结论 |
|---|---|
| 旧契约兼容 | **一刀切**：不保留 legacy 每帧重跑路径；至少导出一个钩子，全缺 → 断连 + 错误日志 |
| 钩子签名 | **零新参数**：钩子内用既有全局（`json`/`http`/`ws`/`bus`/…），与 HTTP handler 心智一致 |
| 返回值 | **忽略**：回帧必须显式 `json.ok` / `ws.send`，不自动包信封 |
| error 之后 | **连接继续**；error 自身抛 → 记日志丢帧，不断连 |
| 帧超时 | **必断连**：V8 terminate 后 runtime 毒化（红线），跳过 close() 直接断 |

## 方案取舍

- **A. 驻留会话 + JS dispatcher（采纳）**：每连接 checkout 一个 runtime 不还池；连接建立时注入 side-module driver（import ws.ts → 装 `__ws_hooks`/`__ws_call`），每事件 `execute_script("__ws_call(ev)")` 复用现有捕获链（`ws_sends`/`ws_close`/信封）。零新增 op、零 ReqState 新字段。
- B. Rust 直调 v8 导出（拒绝）：需 get_module_namespace + 手工 marshal 参数/返回值/promise，管线重；其收益 dispatcher 均可一行实现。
- C. 每帧重 load 模块（拒绝）：帧可能落在不同池化 runtime，连接状态语义破碎。

## 设计

### §1 JS 契约

- 四钩子全可选，至少导出一个（连接时校验）。「零新参数」指帧内容/连接上下文不作为参数传入（走 `http.body` 等全局）；唯一例外是 `error(e)` 收到异常对象本身——这是其职责所在。
- 连接时校验「至少一个钩子」由 Rust 侧在 `ws_connect` 完成后判定（解析 driver 回报的钩子清单），全缺即断连。
- 模块作用域 = 连接状态（跨帧存活）；跨连接共享走 kv/bus。
- 触发时序：`connection()`（首帧前恰好一次）→ 每帧 `message()` → `close()`（恰好一次，尽力而为）。任一钩子抛异常 → `error(e)`。
- 热重载：连接持有连接时刻模块（`?v=mtime`），改动对新连接生效。

### §2 Bridge 侧

```rust
pub struct WsSession { rt: JsRuntime, kill: Arc<KillSwitch> } // !Send，钉在 ws-js 线程
impl Bridge {
    pub async fn ws_connect(&self, source: &str) -> Result<WsSession, RunError>;
}
impl WsSession {
    pub async fn fire(&mut self, ev: &str, req: RequestInfo, timeout: Duration)
        -> Result<WsOutcome, RunError>;
}
```

- `ws_connect`：checkout（booted）→ 注入 driver（run_module 同款 format! 拼装；`const h = (await import(spec)).default ?? {}` 逐键取函数，`__ws_call` 空钩子 no-op、try/catch 转 error()）→ 钩子清单经 `json.ok` 信封回报、Rust 解析校验「至少一个」。
- `fire`：reset ReqState + arm 看门狗 + `__ws_call(ev)` + drain event loop + 读 `WsOutcome`。实现上把 `run_ws` 尾部（execute → 读捕获 → 兜底）抽成 run_ws 与 fire 共用 helper。
- **会话 runtime 永不还池**（模块作用域含连接状态）；每次 fire 均 drain，正常路径 drop 前无悬挂句柄；超时路径按 `run_ws` 现状直接丢弃。

### §3 server 侧 frame_loop

Reader / Writer / bus forwarder 三任务**一行不动**，仅换 Processor 内核：

```
transpile（不变）→ make() → ws_connect()
  ├─ Err → 发 Close 帧 + 返回（对齐现转译失败路径）
  └─ fire("connection", req{method:"WS", bus_tx}) → 写出 sends/信封
每帧: fire("message", req{body, bus_tx}) → 写出 → o.close → 跳出
退出 loop（除超时外）→ 尽力 fire("close") → drop 会话
```

`mirror_routes` / `js_route` / `ws_files` 挂载逻辑零改动；release（dist）路径不受影响（契约在运行时按模块导出生效）。

## 破坏性变更清单

1. `server/src/ws.rs` 现有 legacy 用例改写为新契约（echo 信封、send 顺序、bus 广播、缺失文件、双连接聊天等回归钉全部保留语义）。
2. 新增用例：error 兜底后连接继续、close 恰好触发一次、无导出断连、connection 阶段 ws.send 先于首帧。
3. `sample/src/news/ws.ts` 改写为新契约示例。
4. 文档：`docs/dev-guide.md`、`docs/devkit/api-manual.md` WS 章节、`CHANGELIST.md`（breaking 标注）。

## 范围外（YAGNI）

钩子显式参数、连接注册表/按 id 推送、会话池复用、wss 根证书（v0.1.8 路线图另事）、release 版本段 URL 限制（v0.2 已知）。

## 已知天花板

- `ponytail:` 每连接独占 VM 至断开（≈现状成本：现路径每连接也新建 Bridge/VM），高并发长连接场景需评估共享 module snapshot 或会话池——先量化再动。
