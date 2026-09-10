# MQ 消费任务教学（Kafka/RabbitMQ 命名客户端 + tasks 长任务池）

> 面向两类读者：**要接 MQ 消费、写长任务的业务开发者**（§1–§3）与**要改 MQ/任务
> 实现的维护者**（§4–§6）。JS 签名权威是 [devkit/api-manual.md](devkit/api-manual.md)
> §6「命名 MQ 客户端与长任务」；设计裁决记录是
> `docs/superpowers/specs/2026-09-07-mq-client-tasks-design.md`。可运行示例全程以
> `sample/src/tasks/` 与 `sample/config.yaml` 为锚。

## 1. 心智模型：命名客户端 + 常驻任务

**命名客户端**与 `DB("default")` 同一手感——config 里配实例，JS 里按名取用：

```js
const k = Kafka("default");     // kafkas.default → oj-bus-kafka 插件
const r = RabbitMQ("default");  // rabbits.default → oj-bus-rabbitmq 插件
const x = Kafka("nope");        // 未配置的名 → undefined（不报错）
```

**长任务**是与 HTTP handler 完全相反的执行模型：

| | `api.ts` handler | `src/tasks/task_*.ts` 任务 |
|---|---|---|
| 触发 | HTTP 请求命中路由 | 服务启动即拉起，**常驻循环** |
| 生命周期 | 请求结束即收场 | 跑到 `tasks.stopping()` 或进程停机 |
| 执行位置 | HTTP actor 池（VM 排队复用） | **每任务一条专用线程 + 独立 VM** |
| 可用 MQ 面 | `send` / `publish` / `metadata` / `kind`（只生产） | 加上 `poll` / `commit` / `ack` / `nack`（消费） |
| 崩溃后果 | 该请求 500 | 监督器退避重启（1s→2s→…cap 60s） |

与 `bus.*` 的分工：`bus` 是**广播**（fire-and-forget，WS 订阅扇出）；MQ 客户端是
**持久队列消费**（有 offset / ack 语义，broker 宕机消息不丢）。要可靠逐条消费用后者。

## 2. 写一个消费任务：逐行拆解

### Kafka：at-least-once + 按分区 commit

```ts
export {};                                        // 任务按 ESM 加载——TLA 必须有 ESM 标记
const k = Kafka("default");

while (!tasks.stopping()) {                       // 唯一合法的退出条件（停机信号）
  // timeoutMs 要远小于 stop_grace_secs（默认 30s）：poll 长过宽限就会错过退出窗口，
  // 到期被看门狗强杀（记 killed）
  const { messages } = await k.poll(["orders"], { max: 100, timeoutMs: 1000 });
  // commit(offset+1) 只推进该消息所在分区——多分区主题必须按分区各提交一次，
  // 只提交最后一条会丢其余分区的进度（at-least-once：处理须幂等）
  const deepest = new Map();                      // partition → 该分区最深的消息
  for (const m of messages) {
    /* 业务处理（幂等） */
    const cur = deepest.get(m.partition);
    if (!cur || m.offset > cur.offset) deepest.set(m.partition, m);
  }
  for (const m of deepest.values()) await k.commit(m);
}
```

### RabbitMQ：basic_get 拉取 + 手动 ack

```ts
export {};
const r = RabbitMQ("default");

while (!tasks.stopping()) {
  // poll = 逐队列轮转 basic_get（每条一个 round-trip），max 条或 timeoutMs 到期先到为准
  const { messages } = await r.poll(["q1"], { max: 10, timeoutMs: 1000 });
  for (const m of messages) {
    /* 业务处理 */
    await r.ack(m);                    // 失败：await r.nack(m, true) 重新入队
  }
}
```

### 没有 broker？纯生命周期任务

```ts
export {};
while (!tasks.stopping()) {
  await tasks.sleep(1000);             // 运行时无 timer 全局，setTimeout 不可用
  log.info("demo tick");
}
```

消息形状（`OjMqMessage`，两个 broker 统一）：`{ topic, partition?, offset?, key?,
value, headers?, ts?, delivery_tag? }`——`delivery_tag` 仅 rabbit（ack/nack 用）。

### 帧处理器可用的全部「主动作」

| 客户端 | 生产面（任何上下文） | 消费面（**仅任务文件内**） |
|---|---|---|
| `Kafka(name)` | `send(topic, { value, key?, headers? })`；`metadata()`；`kind()` | `poll(topics, { max?, timeoutMs? })` → `{ messages }`；`commit(m)` |
| `RabbitMQ(name)` | `publish(exchange, routingKey, value, { headers? })`；`metadata()`；`kind()` | `poll(queues, { max?, timeoutMs? })`；`ack(m)`；`nack(m, requeue?)` |

在 HTTP/WS handler 里调消费方法直接报错
`requires a task context`——消费会话归属任务实例（同一实例第二个并发 poll 报
`instance busy`）。

## 3. 跑通 sample

```bash
# 无 broker 也能跑：config 不配 kafkas/rabbits，只有 task_demo 心跳任务
./bin/oj server -c sample/config.yaml --api-path sample/src --console-log
```

启动日志逐行解读：

```
task: demo (task_demo.ts) → started        # 逐任务一行（name + 文件名）
task: 1 task(s) → started                  # 汇总行
```

`Ctrl-C` / `kill -TERM` 后：

```
shutdown: stop flag set — draining tasks and in-flight requests
task: demo → stopped                       # 宽限内自然收场；不退者记 killed
```

接真 broker：打开 `sample/config.yaml` 的 `kafkas:`/`rabbits:` 注释段，把任务文件的
topic/queue 换成你的。崩溃重启长这样（指数退避，成功运行 ≥60s 归零）：

```
task: orders crashed, restart in 2s (attempt 3)
```

release 部署：`oj build` 自动把 `src/tasks/` 转译镜像到 `dist/tasks/`（相对 import
补 `.js` 后缀；不进 tgz/manifests——任务非版本化模块）。

## 4. 实现走读（改代码前读这节）

分层自上而下（每层只认识相邻层）：

```
bootstrap.js 全局（mqCache 同一性；Kafka/RabbitMQ/tasks）
  → op_mq_call / op_mq_has / op_tasks_stopping / op_tasks_sleep   (src/bridge/mq.rs)
    → NamedRegistry<MqInstance>（kafkas / rabbits 双注册表，app.rs 装配期 connect）
      → MqInstance { call 闭包, poller 互斥, closer }             (src/bridge/mq.rs)
        → FFI：MqVtable { connect, call, close }（await_ffi_poll 退避轮询）
          → 插件 Core（oj-bus-kafka / oj-bus-rabbitmq，一 Core 两轴面）
```

关键裁决（改实现前必读，出处 spec 评审记录）：

- **单 `mq` 轴 + JSON method dispatch**：加方法零 ABI 变更；`ABI_VERSION` 保持 7
  （`oj-plugin-ffi/src/mq.rs` 的 `MqVtable` 是全新类型，不动既有 repr(C) 形状）。
- **消费门禁 = `flag.is_some()`**（`mq.rs` op_mq_call）：任务 Bridge 的
  `StableState.tasks_flag` 是 `Some(flag)`，HTTP 桥是 `None`——判**存在**不判**值**。
  flag 的值是停机信号（生产装配里与 SIGTERM 置位的是同一个 `Arc`，初值 false）。
  统一审查曾在此抓出 `is_some_and(load)` 生产级 bug：正常运行期消费全被拒。
- **`MqInstance` Drop 兜底 close**：ffi 构造的实例持 `closer` 闭包，释放时调
  `vtable.close(handle)`（kafka 离开消费组 / rabbit 丢 ackers）。
- **rabbit 复用 channel**（`reuse_channel`）：lapin 2.5 的 Channel 没有 Drop→close，
  逐次新建即弃会泄漏连接上的 channel 直到打满 `channel_max`——失效自愈重建。

**任务驱动**（`Bridge::run_task`，`src/bridge/mod.rs`）：任务文件**一律按 ESM
side-module 直载**（转译源 + 文件自身 versioned URL，绕过 loader 的 looks_cjs 启发
式——minified release 产物会被它误伤）。三态出口：

- `Stopped`：TLA 自然完成（任务自己检测 `tasks.stopping()` 退出）；
- `Crashed`：顶层 throw / 加载失败（监督重启信号）；
- `Killed`：宽限到期强杀。

强杀为什么只能靠看门狗：TLA 紧循环（如 `while(true){ await poll() }` 且 op 同步
就绪）会把 `mod_evaluate` 的初始 microtask checkpoint **同步自旋到永不返回**——宿主
线程连 `select!` 的协作分支都轮不到。所以 `KillSwitch::arm_on_flag(handle, flag,
grace)` 把停机 flag + grace 交给看门狗线程**代盯**：flag 置位即起算宽限，到点跨线程
`terminate_execution`。第二个坑：terminate 落在 isolate 空闲窗口会**悬空 TLA
promise**（`eval` 永不 settle）——fired 后宿主放弃 await eval，兜底轮加 1s 超时。
SIGSEGV 纪律：terminate 后本线程兜一轮 event loop 再丢弃 runtime，绝不归还池。

**监督器**（`oj/src/tasks.rs`）：`scan_tasks` 递归扫描 `task_{name}.*` /
`{name}_task.*`（其余文件是共享库；同名双写、超 `max` fail-fast）→ 每任务
`std::thread`（名 `task-{name}`）内 current_thread runtime + 独立 Bridge（
`catch_unwind` 兜 panic）→ 循环 `run_task`。停机序列（`server_cmd.rs` 全仓首个信号
处理器）：SIGINT/SIGTERM → 停机 flag 置位 → axum `with_graceful_shutdown` 同信号
排空在途请求 → 任务在 `stop_grace_secs` 内自然收场（不退者看门狗强杀）→ join → 退出。

## 5. 约定与红线

- 任务文件命名 `task_{name}.{ts,js}` / `{name}_task.{ts,js}`（`tasks.dir` 可改，默认
  `tasks`；`tasks` 也是模块扫描的保留目录名——别拿它当模块名）。
- 任务文件必须有 ESM 标记（`export {};` 或真实 import/export）；CJS 写法
  （`module.exports`）加载即 `ReferenceError` → Crashed。
- `timeoutMs` ≪ `stop_grace_secs`；处理逻辑必须幂等（at-least-once）。
- kafka 消费会话在首次 poll 时按当时的 topics 固化订阅，后续换 topics 被忽略——
  要换主题就换实例名。
- 任务无热重载：改文件重启进程（转译缓存按 mtime 自动失效）。
- 运行时无 timer 全局：等待一律 `await tasks.sleep(ms)`。

## 6. 测试

| 位置 | 用例 | 钉住的行为 |
|---|---|---|
| `src/bridge/mq.rs` tests | `given_http_context_when_poll_then_err_requires_task` | 消费门禁（HTTP 桥拒绝） |
| 同上 | `given_running_task_bridge_when_poll_then_allowed` | **正常运行期**（flag=false）poll 放行——门禁语义回归钉 |
| 同上 | `given_second_concurrent_poll_when_first_active_then_err_busy` | 实例级单 poller |
| 同上 js_global_tests | `given_tasks_sleep_when_awaited_then_resolves` | 等待原语真实睡眠 |
| 同上 task_driver_tests | `given_tla_while_loop_task_when_flag_set_then_exits_stopped` | flag 置位自然收场 |
| 同上 | `given_task_ignoring_flag_when_grace_expires_then_killed` | 看门狗强杀（紧循环不挡） |
| 同上 | `given_cjs_style_task_when_run_then_crashed_with_guidance` | 一律 ESM；CJS 自然 Crashed |
| `oj/src/tasks.rs` | `given_crashing_task_when_supervised_then_restarts_with_backoff` | 退避重启 |
| 同上 | `given_running_tasks_when_shutdown_flag_then_all_join_within_grace` | 停机 join |
| `plugins/oj-bus-kafka|rabbitmq` | `poll_req_accepts_js_camel_case_timeout` | `timeoutMs` 真正生效 |
| `plugins/oj-bus-rabbitmq` | `mask_url_hides_password_keeps_plain` | metadata 不泄凭据 |
| `oj/tests/e2e.rs` | `given_running_server_when_sigterm_then_tasks_stop_and_process_exits` | **进程级**：SIGTERM → 任务收场 → 退出全序列 |
