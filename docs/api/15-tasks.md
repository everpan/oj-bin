# tasks —— 任务上下文（停机信号与等待原语）

> 本文档由 src/bridge/ 梳理生成；权威 API 参考见 ../devkit/api-manual.md。
> 教学走读（心智模型 / 实现分层 / 测试地图）见 ../mq-tasks.md。

## 是什么

`tasks` 全局对象（`src/bridge/task_pool.rs` 的任务池 + `src/bridge/mq.rs` 的两个 op，
`bootstrap.js` 527~532 行挂载）只有**两个方法**，是长任务循环里的上下文原语：

- `tasks.stopping()` —— 停机信号。服务收到 SIGINT/SIGTERM 后 flag 置位，任务循环据此
  自然收场；**任务桥之外恒为 `false`**（HTTP/WS handler 里调用无意义但不报错）。
- `tasks.sleep(ms)` —— 等待原语。**本运行时没有 timer 全局**（`setTimeout` /
  `setInterval` 不可用），任务里一切等待都用 `await tasks.sleep(ms)`。

任务 = `tasks.dir`（缺省 `src/tasks/`）下符合命名约定的文件（`task_{name}.ts/.js` 或
`{name}_task.ts/.js`，递归扫描）。按**导出探测**分两种模式：

- **TLA 监督器任务**：顶层 await 循环写法（`while (!tasks.stopping())`），每任务一条
  专用线程 + 独立 V8 runtime，崩溃按 1s→2s→…cap 60s 退避重启。
- **池化任务**（v0.1.28）：导出 `loop_body` 命名导出即走任务池——`tasks.pool.workers`
  个 Worker 线程每轮驱动一次 `setup/loop_body/teardown` 三钩子，**不需要**
  `tasks.stopping()` 轮询；停机时先跑 `teardown` 再退出。cron 定时任务由
  `tasks.crontab` 清单（5 字段 crontab）到点整模块跑一次。

**与 mq 轴的分工**：`Kafka(name)` / `RabbitMQ(name)` 客户端的 `poll/commit/ack/nack`
**只在任务上下文可用**（HTTP/WS handler 调用直接报错
`mq.{method} requires a task context`）——消费会话归属任务实例，实例级单 poller，
第二个并发 poll 报 `instance busy`。即：`tasks` 提供循环骨架与生命周期，`mq` 轴
提供持久队列消费（offset/ack 语义）；`bus` 则是 fire-and-forget 广播，不可靠逐条消费。

## 配置

```yaml
tasks:
  # dir: tasks                # 任务目录（api-path 相对；递归扫描 task_{name}.* / {name}_task.*）
  # max: 64                   # 任务数上限（同名双写拒启）
  # stop_grace_secs: 30       # 停机宽限（TLA 看门狗 / 池化 teardown 共用）
  pool:
    workers: 4                # 池化/cron 任务 worker 线程数（静态轮转分配）
    loop_body_timeout_ms: 5000  # 单轮 loop_body 看门狗超时（超时 → teardown → failed）
    interval_ms: 100          # 轮间节奏：每轮返回后 sleep 再调下一轮（防空转独占 Worker）
                              #   0 = 不限制（压测语义，生产会打满多核）
  crontab: task/crontab.yaml  # cron 清单（相对 tasks.dir，5 字段：分 时 日 月 周  相对路径）；缺省不启用
  event_log:
    enabled: true             # 任务事件 JSONL 日志（内存池的落盘侧）
    path: logs/task-events.jsonl
    max_mb: 16                # 超量轮转 .jsonl.1
```

两者皆无（无 `loop_body` 导出任务、无 crontab 文件）则不创建任务池。

## API 表

| API | 签名 | 说明 |
|---|---|---|
| `tasks.stopping` | `stopping(): boolean` | 停机信号：置位后任务应尽快自然收场；任务桥之外恒 `false` |
| `tasks.sleep` | `sleep(ms: number): Promise<void>` | 睡眠 `ms` 毫秒（tokio sleep；`ms <= 0` 立即返回）。本 runtime 唯一合法等待原语 |

任务管理面（HTTP 控制面，鉴权/租户语义与业务路由一致，v0.1.28）：
`GET {base}/tasks`、`PATCH {base}/tasks/{name}`（仅 `{enabled, cron}`）、
`POST {base}/tasks/{name}/start|stop|enable|disable|reload|run-once`、
`GET {base}/tasks/{name}/logs`——详见 api-manual 第 6 章「池化任务与 cron」。

## 错误

| 场景 | 形态 | 消息关键词 |
|---|---|---|
| HTTP/WS handler 里调 mq 消费面（`poll/commit/ack/nack`） | 抛错 | `mq.{method} requires a task context (long-running tasks only; use send/publish from HTTP/WS)` |
| 同实例第二个并发 `poll` | 抛错 | `mq: instance busy (another active poller)` |
| cron 表达式非 5 字段 | 解析报错（启动 fail-fast） | `cron: expected 5 fields (min hour dom mon dow), got {n}` |
| cron 宏（`@daily` 等） | 解析报错 | `cron: macros (@daily etc.) not supported, use 5-field form` |
| cron 字段越界 / 非法 | 解析报错 | `cron: '{part}' out of range {lo}-{hi}` / `cron: bad value '{part}'` / `cron: step 0 in '{part}'` |
| 停机后仍有 run-once 作业入队 | 拒绝执行（不触碰任务状态） | `task pool stopping: run_once '{name}' rejected` |
| `loop_body` 单轮超时 | 任务置 `failed`（teardown 尽力执行，runtime 毒化丢弃） | `loop_body timeout ({ms}ms)` |
| `loop_body` JS 堆超限 | 任务置 `failed` | `loop_body js heap limit exceeded (limit … server.js_heap_limit_bytes)` |

## 限制

- **无 timer 全局**：`setTimeout/setInterval` 不存在；任务里等待一律
  `await tasks.sleep(ms)`。
- **任务文件是 ES 模块**：用顶层 `await` 必须带一句 `export {};`（否则按 CJS 包装
  直接 SyntaxError）。池化任务靠命名导出天然是模块。
- **无热重载**：改任务文件需重启进程（转译缓存按 mtime 自动失效；池化任务可用管理面
  `reload` 重连单个任务）。
- **`timeoutMs`（MQ poll）与 `stop_grace_secs`（默认 30s）的互动**：停机 flag 置位后
  任务有宽限期自然收场；`timeoutMs` 应显著小于宽限（如 ≤1s），否则任务在一次长 poll
  中错过退出窗口，到点被看门狗强杀（记 `killed`）。
- **单轮等待超过 `loop_body_timeout_ms`（默认 5s）的任务不要用池化**——长轮询
  （MQ 消费、外部长连接）留在 TLA 模式，用 `tasks.sleep()` 自定节奏。
- **轮间节奏 `interval_ms`（默认 100ms）**：每任务 ≈10 轮/s 上限；调小前先算
  kv/db/日志的写放大（轮率 × 每轮 IO）。`0` = 不限制（压测语义）。
- **cron 是分钟粒度、UTC**：标准 5 字段（分 时 日 月 周），支持 `*`、`*/n`、
  列表 `a,b,c`、区间 `a-b`；周字段 0 与 7 均为周日；宏与秒字段显式报错。cron 文件
  是脚本式（整模块跑一次），不是三钩子任务；命名用普通文件名——用 `task_*.ts` 会被
  任务扫描器同时收编成长任务，同名直接拒启。
- **kafka 消费会话 topic 固化**：首次 `poll` 按当时的 topics 建订阅，其后换 topics
  被忽略；需换主题就换实例名。
- 事件日志仅内存池（JSONL 落盘 + 1000 条环形缓冲），无 MQ 后端接入。

## 案例

### TLA 长任务：Kafka 消费骨架

```ts
// src/tasks/task_orders.ts —— 顺序语义：poll → 处理 → commit（至少一次，处理须幂等）
export {};
const k = Kafka("default");
while (!tasks.stopping()) {
  const { messages } = await k.poll(["orders"], { max: 100, timeoutMs: 1000 });
  // commit(offset+1) 只推进该消息所在分区——多分区主题必须按分区各提交一次
  const deepest = new Map(); // partition → 该分区最深的消息
  for (const m of messages) {
    /* 业务处理（幂等） */
    const cur = deepest.get(m.partition);
    if (!cur || m.offset > cur.offset) deepest.set(m.partition, m);
  }
  for (const m of deepest.values()) await k.commit(m);
}
```

### 池化任务：每轮增量心跳

```ts
// src/tasks/task_watch.ts —— 导出 loop_body 即走任务池（可运行案例 sample/src/tasks/task_watch.ts）
export async function setup() {
  log.info("watch connected");
}
export async function loop_body() {
  const n = Number((await kv.get("watch:last")) ?? "0");   // 游标存 kv，每轮只做增量
  await kv.set("watch:last", String(n + 1));
  log.info(`watch tick #${n + 1}`);
}
export async function teardown() {
  log.info("watch down（尽力而为：超时不保证跑完）");
}
```

```bash
curl http://localhost:9778/v1/api/tasks                 # 任务清单（runCount 增速 = 实际轮率）
curl -X POST http://localhost:9778/v1/api/tasks/watch/stop
curl http://localhost:9778/v1/api/tasks/watch/logs
```

### cron 定时任务

```yaml
# src/tasks/task/crontab.yaml —— 逐行：分 时 日 月 周  任务文件路径（相对 tasks.dir）
*/5 * * * *  ./jobs/report.ts   # 每 5 分钟跑一次 report.ts（整模块跑一次）
30 2 * * 1   ./jobs/backup.ts   # 每周一 02:30
```

```ts
// src/tasks/jobs/report.ts —— cron 脚本：顶层 await 即执行体，跑完释放 Worker
export {};
const rows = db.table("account").select(["id"]).all();
log.info(`report: ${rows.length} accounts`);
```

```bash
curl -X POST http://localhost:9778/v1/api/tasks/report/run-once   # 立即派一轮（仅 cron 任务）
```
