# MQ 命名客户端绑定 + 长任务池设计（Kafka('default') / RabbitMQ('default') / tasks:）

日期：2026-09-07
状态：已与用户逐节确认（架构方案、config、API 面、FFI 契约、任务池、错误语义、
测试矩阵均获批准；任务命名约定与启动日志为追加裁决）

## 1. 背景与目标

oj 的消息中间件能力目前只有 `bus` 高层广播（`bus.publish/subscribe`，单例 broker，
config `broker:` 单段，kind 可切 local/kafka/rabbitmq）。业务侧无法：按名持有多个
集群/实例的客户端、发送带 key/partition/header 的消息、以消费组语义拉取与提交。

目标：

1. **命名 MQ 客户端**：JS 全局 `Kafka(name)` / `RabbitMQ(name)`（同 `DB(name)` 模式），
   config `kafkas:` / `rabbits:` 命名段；完整客户端面（send/poll/commit/ack/metadata）。
2. **长任务池**：config `tasks:` 指定文件夹，其中符合命名约定的每个 `.ts`/`.js` 由
   框架拉起为**独立线程上的长驻 JS 任务**（文件顶层跑 `while (!tasks.stopping) poll...`
   循环），生命周期（启动/崩溃重启/优雅停机/热重载）由主框架管理，启动日志逐任务显示。
3. **架构遵循 SOLID/DRY**：与既有 bus 实现**共底层**——插件内一个 broker driver Core、
   bus 轴与 mq 轴两个薄适配面；宿主侧 bus/mq 共享注册表与 FFI 调用机制，互不重复。

### 非目标（v1 边界）

- DLQ / 死信（mq 轴 method 派发为将来留了 `dlq` 方法位，本期不实现）。
- 同集群连接指纹复用（`broker:` 段与 `kafkas:` 段指向同一集群时各持连接，插件内
  复用为后续优化）。
- 定时调度 / cron（长任务由文件内自驱循环，框架不提供 tick）。
- 第三种 broker（nats 等）：加插件即扩展，不动本设计。

## 2. 架构（共底层与 SOLID 落点）

```
                     ┌──────────────── oj-plugin-ffi (ABI 8) ────────────────┐
                     │  bus vtable（不变，窄面）    mq vtable（新，JSON dispatch）│
                     └───────▲────────────────────────────▲──────────────────┘
                             │                            │
   plugins/oj-bus-kafka ─────┤ KafkaCore（rdkafka：连接/生产/消费组/提交/metadata）
   plugins/oj-bus-rabbitmq ──┤ RabbitCore（lapin 同理）                 │
                             └── 两轴 vtable 都是 Core 的薄适配器（一个 driver，两个 face）
   宿主：
   src/bridge/bus_backend.rs   EventBroker trait（不动，bus 语义窄接口）
   src/bridge/mq.rs（新）       NamedRegistry<MqInstance> + op_mq_* op 面
   oj/src/tasks.rs（新）        长任务池监督器：线程模型/重启/停机/热重载/日志
```

- **SRP**：插件 = broker driver（连接管理 + 客户端语义）；轴 vtable = 协议适配器；
  宿主 `mq.rs` = 命名注册 + op 面；`tasks.rs` = 生命周期监督（与 MQ 逻辑零耦合）。
- **OCP**：mq 轴方法面走 JSON `method` 字符串派发——加方法零 ABI 变更；新 broker =
  加插件，宿主零改动。
- **ISP**：bus 轴（窄面：广播语义，WS/HTTP 路径依赖）与 mq 轴（宽面：全量客户端）
  分离；既有第三方 bus 插件零破坏。
- **DIP**：宿主只依赖 `oj-plugin-ffi` 契约与 trait，不依赖 rdkafka/lapin（现状保持）。

## 3. config schema

```yaml
kafkas:                        # 段存在即启用；key = 实例名 → Kafka(name)
  default: { brokers: [b1:9092, b2:9092], group: g1 }
  audit:   { brokers: [b3:9092] }
rabbits:                       # key = 实例名 → RabbitMQ(name)
  default: { url: "amqp://..." }            # 或 brokers: [amqp://...]
tasks:                         # 长任务池；缺省 = 不启用
  dir: tasks                   # 相对 api 根（release 相对 dist）；递归扫描
  stop_grace_secs: 30          # 优雅停机宽限（默认 30）
```

- cfg 值为 **JSON 透传**（同 `plugins:` 段「值 = 透传 cfg」哲学）；装配层按段来源向
  cfg 注入 `"kind": "kafka" | "rabbit"`，插件自检不符即**拒绝装配**（fail fast，
  报错指明装错插件）。
- **任务文件命名约定**：`tasks.dir` 递归扫描下，只有 `task_{name}.ts|js` 或
  `{name}_task.ts|js` 是任务（`name` 从文件名剥前/后缀提取）；其余文件为任务可
  import 的共享库代码，只转译不执行（与 `api.ts` 入口标记、`_shared/` 共享码同哲学）。
  `task_orders.ts` 与 `orders_task.ts` 并存同名 → fail fast（注册表重复名语义）。

## 4. JS API 面

```js
const k = Kafka("default");        // 未配置的名 → undefined（同 DB(name)）
await k.send("orders", { key: id, partition: null, headers: {trace: t}, value: data });
const batch = await k.poll(["orders"], { max: 100, timeoutMs: 1000 });
//   msg = { topic, partition, offset, key, value, headers, ts }
await k.commit(msg);               // 显式提交（at-least-once）
const md = await k.metadata();     // 可选 method；插件可返回 unsupported

const r = RabbitMQ("default");
await r.publish("ex", "rk", payload, { headers });   // publish = send 的 rabbit 命名（见 §5）
const batch = await r.poll(["q1"], { max: 10, timeoutMs: 1000 });  // basic.get 语义
await r.ack(msg);
await r.nack(msg, /* requeue */ false);
```

- 两个全局共享同一 `NamedRegistry<MqInstance>`，只是方法命名空间不同；实例对象 JS 侧
  缓存保证同一性（同 `dbCache` 模式：`Kafka("x") === Kafka("x")`）。
- 统一消息形态 `MqMessage`；rabbit 的 `send` 名为 `publish`（贴近 AMQP 语义），
  kafka 的确认语义为 `commit`、rabbit 为 `ack/nack`——两个全局各自只暴露有意义的面。
- `bus.publish/subscribe` 高层广播**保持不动**（面向 WS 订阅/进程内 fan-out 的窄接口）。
- 任务上下文新增全局 `tasks`：`tasks.stopping` → `bool`（检测停机信号）；HTTP/WS
  请求上下文中恒 `false`（同 `ws.send` 在 HTTP 路径 no-op 的哲学）。
- 任务上下文 `http` 为空上下文（无 query/body/user）；`json.ok` 可调但输出丢弃
  （无响应捕获方）。任务典型形态：

```js
// tasks/task_orders.ts —— 单文件长驻任务
const k = Kafka("default");
while (!tasks.stopping) {
  const batch = await k.poll(["orders"], { max: 100, timeoutMs: 1000 });
  for (const m of batch) {
    await db.query("insert ...", [m.value.orderId]);
    await k.commit(m);
  }
}
```

## 5. mq 轴 FFI 契约（ABI 7 → 8）

```rust
#[repr(C)]
pub struct MqVTable {
    connect: fn(cfg: RString) -> FfiFuture,   // cfg JSON（含 kind）→ handle
    call: fn(handle: u64, method: RString, payload: RString) -> FfiFuture,  // → json
    close: fn(handle: u64),
}
```

- 复用 bus 轴既有的 `RString` / `FfiFuture` / handle 惯例与 `FfiGuard` 配对语义。
- 方法面统一：`kind` / `send` / `poll` / `commit` / `ack` / `nack` / `metadata`（可选）/ `dlq`（预留）。
  `send` 为唯一发送 method（kafka payload 带 `topic`，rabbit payload 带
  `exchange`/`routingKey`）；JS 层 `RabbitMQ.publish()` 是 `send` 的别名命名。
  可选 method（如 `metadata`）未实现 → `call` 返回 `Err("unsupported method: X")`，
  走既有 Err→JsError 通路，不另设错误信封。payload JSON 各插件自解释。
- `plugin_loader::AXES` 追加 `"mq"`（现 `[es, db, blob, bus, kv, auth]`）；
  `oj_plugin_entry!` 增加 `mq => &VT` 变体；缺符号 = 不提供该轴（加轴零破坏语义不变）。
- `ABI_VERSION` 7 → 8（严格相等门禁）：**所有既有插件需随新宿主重编译**（repr(C) 未
  变，源码级兼容；发布物级不兼容，随版本发布链统一重建）。
- `oj-bus-kafka` / `oj-bus-rabbitmq` 改造为：Core（rdkafka/lapin driver）+ 两个轴适配
  （bus 轴面形状不变；mq 轴面 = method → Core 方法轻派发）。两插件同构，Core 拆分
  时 bus 现有 roundtrip 测试作回归护栏。

## 6. 长任务池生命周期（`oj/src/tasks.rs`）

- **线程模型**：每任务文件 = 专用 OS 线程 + current_thread runtime + 独立 Bridge
  （`routes.rs bridge_introspector` 与 `ws.rs conn_on_pinned` 的既有同款模式，
  `JsRuntime !Send` 红线天然满足）；失败 runtime 丢弃不归还（红线 5）。
- **监督**：启动即拉起全部任务；panic / JS 顶层错误退出 → 指数退避重启
  （1s → 2s → 4s … cap 60s，成功运行归零），重启日志含 attempt 数。
- **优雅停机**：SIGINT/SIGTERM → 进程级 `tasks.stopping` 置位（AtomicBool，op 读）
  → JS 循环检测退出 → `stop_grace_secs` 宽限后 `terminate_execution` 强杀 →
  close 所有 mq handle → 进程退出。
- **热重载（dev）**：任务文件 mtime 变更 → 停该任务线程 → 起新线程（复用 notify）；
  release 服务 `dist` 转译产物（与 api 的 dev/release 双态一致）。
- **启动日志**（逐任务 + 汇总，对齐 `oj build:` 风格；崩溃重启/停机同落）：

```
task: orders (task_orders.ts) → started
task: audit  (audit_task.ts)  → started
task: 2 task(s) → started
task: orders crashed, restart in 2s (attempt 3)
task: orders → stopped
```

## 7. 错误与重试语义

- 消息语义 **at-least-once**：poll → 处理 → commit/ack 三段，提交前崩溃必重投。
- `connect` 失败 → **装配 fail-fast**（同 db 插件缺失语义，不吞错、拒绝启动）。
- 运行期 broker 断连 → `call` 返回 Err → op 报 JsError，由任务循环自行重试；框架
  只管线程级监督（退出 → 重启），不替业务做消息级重试。
- JS 帧内调用语义：`Kafka("未配置名")` → `undefined`；对 undefined 调方法即普通
  TypeError（同 `DB` 现状，不做静默 mock）。

## 8. 测试矩阵

| 层 | 内容 |
|---|---|
| oj-plugin-ffi | mini 夹具插件扩 mq 轴：ABI 门禁 / 缺符号=不提供 / catch_unwind 路径 |
| core | `InMemoryMq`（内存队列实现同一 method 面）：op 面 / 命名门禁 / JS 缓存同一性 / `tasks.stopping` 上下文差异 |
| core | tasks 监督：正常退出 / panic 重启（退避）/ stopping 优雅停机 / mtime 热重载 / 命名约定扫描与非任务文件不执行 |
| plugins | kafka / rabbit roundtrip（真实 broker，feature-gated，沿用现有 `real_kafka_publish_subscribe_roundtrip` 模式）；bus 轴既有测试回归 |
| oj 装配 | kafkas/rabbits 注入时序（StableState 首跑前，`Arc::get_mut` 红线）、kind 不符 fail-fast、tasks dir 扫描与同名冲突 |
| e2e | mini mq 插件 + 任务文件消费 → 写 kv → 断言；停机协议端到端 |

## 9. 实现切分（供 writing-plans 展开）

1. **FFI 契约**：`oj-plugin-ffi` 加 mq 轴 + ABI 8 + mini 夹具扩展。
2. **插件 Core 化**：两插件拆 Core / 双轴适配（bus 回归护栏先行）。
3. **宿主 mq.rs**：registry + op 面 + bootstrap 全局 + 装配注入（kafkas/rabbits）。
4. **长任务池**：`oj/src/tasks.rs` 监督器 + config `tasks:` + 扫描约定 + 日志。
5. **文档与示例**：api-manual（Kafka/RabbitMQ/tasks 章）、sample 任务示例、
   global.d.ts 补声明。

## 10. 用户裁决记录（2026-09-07）

| 裁决 | 内容 |
|---|---|
| API 面 | 完整 Kafka 客户端（send/poll/commit/metadata + rabbit ack/nack），非纯命名 bus |
| FFI 形态 | 方案 A：单 `mq` 轴 + JSON method dispatch；与 bus 共底层（一 driver 两 face），架构遵循 SOLID/DRY |
| 全局命名 | `Kafka(name)` 与 `RabbitMQ(name)` 双全局（各自方法命名空间） |
| config | `kafkas:` / `rabbits:` 命名段；`tasks:` 长任务池段 |
| 消费驱动 | 长任务池文件夹：每 `.ts/.js` 独立线程拉起执行，主框架管生命周期 |
| 任务命名 | 必带 `task_{name}.*` 前缀或 `{name}_task.*` 后缀（其余文件为共享库不执行） |
| 可观测 | 启动日志逐任务显示（started / crashed+attempt / stopped） |
