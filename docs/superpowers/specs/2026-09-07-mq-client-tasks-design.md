# MQ 命名客户端绑定 + 长任务池设计（Kafka('default') / RabbitMQ('default') / tasks:）

日期：2026-09-07
状态：已与用户逐节确认 + 架构师/开发专家双评审修订（2026-09-07，修订记录见 §11）

## 1. 背景与目标

oj 的消息中间件能力目前只有 `bus` 高层广播（`bus.publish/subscribe`，单例 broker，
config `broker:` 单段，kind 可切 local/kafka/rabbitmq）。业务侧无法：按名持有多个
集群/实例的客户端、发送带 key/partition/header 的消息、以消费组语义拉取与提交。

目标：

1. **命名 MQ 客户端**：JS 全局 `Kafka(name)` / `RabbitMQ(name)`（同 `DB(name)` 模式），
   config `kafkas:` / `rabbits:` 命名段；完整客户端面（send/poll/commit/ack/metadata）。
2. **长任务池**：config `tasks:` 指定文件夹，其中符合命名约定的每个 `.ts`/`.js` 由
   框架拉起为**独立线程上的长驻 JS 任务**（文件顶层跑 `while (!tasks.stopping) poll...`
   循环），生命周期（启动/崩溃重启/优雅停机）由主框架管理，启动日志逐任务显示。
3. **架构遵循 SOLID/DRY**：与既有 bus 实现**共底层**——插件内一个 broker driver Core、
   bus 轴与 mq 轴两个薄适配面；宿主侧 bus/mq 共享注册表与 FFI 调用机制，互不重复。

### 非目标（v1 边界）

- **任务文件热重载**：dev 下任务文件变更需重启进程生效（api 的热更是 `?v=<mtime>`
  拉式失效，对常驻 isolate 无效；为任务单建 watcher 属 v1 范围外——评审 F6 裁决剪除）。
- DLQ / 死信（mq 轴 method 派发为将来留了 `dlq` 方法位，本期不实现）。
- 同集群连接指纹复用（`broker:` 段与 `kafkas:` 段指向同一集群时各持连接，插件内
  复用为后续优化）。
- 定时调度 / cron（长任务由文件内自驱循环，框架不提供 tick）。
- 第三种 broker（nats 等）：v1 固定 `kafka` / `rabbit` 两 kind；扩展需动宿主三处
  固定触点（bootstrap 全局名、config 段、装配 kind 路由），见 §2。

## 2. 架构（共底层与 SOLID 落点）

```
                     ┌──────────────── oj-plugin-ffi（ABI 保持 7）────────────┐
                     │  bus vtable（不变，窄面）    mq vtable（新，JSON dispatch）│
                     └───────▲────────────────────────────▲──────────────────┘
                             │                            │
   plugins/oj-bus-kafka ─────┤ KafkaCore：连接/生产/channel + cfg 解析共享；
                             │   消费路径两套分列——bus 面 push 扇出（现状），
                             │   mq 面 pull 消费会话（新增，显式 commit）
   plugins/oj-bus-rabbitmq ──┤ RabbitCore 同构（lapin）
                             └── 两轴 vtable 都是 Core 的薄适配器（一个 driver，两个 face）
   宿主：
   src/bridge/bus_backend.rs   EventBroker trait（不动，bus 语义窄接口）
   src/bridge/mq.rs（新）       两个 NamedRegistry（kafkas/rabbits 各一）+ op_mq_*
   src/bridge/runtime.rs（扩）  任务驱动：ESM 常驻执行 + kill_after(grace) 新面
   oj/src/tasks.rs（新）        长任务池监督器：线程模型/重启/停机/日志
   oj/src/build_cmd.rs（扩）    tasks 目录转译镜像 → dist/tasks/
```

- **SRP**：插件 = broker driver；轴 vtable = 协议适配器；宿主 `mq.rs` = 命名注册 +
  op 面；`tasks.rs` = 生命周期监督（与 MQ 逻辑零耦合）。
- **OCP（修正表述，评审 S5）**：mq 轴方法面走 JSON `method` 派发——**加方法零 ABI
  变更**；但「新 broker = 加插件，宿主零改动」不成立——固定触点三处（bootstrap
  全局名 / config 段 / 装配 kind 路由）。v1 固定两 kind。
- **ISP**：bus 轴（窄面：广播语义，WS/HTTP 路径依赖）与 mq 轴（宽面：全量客户端）
  分离；既有第三方 bus 插件零破坏（ABI 不变是前提，见 §5）。
- **DIP**：宿主只依赖 `oj-plugin-ffi` 契约与 trait，不依赖 rdkafka/lapin。
- **Core 边界写实（评审 S1）**：连接/生产/channel 与 cfg 解析共享；消费路径两套
  分列——bus 面维持 push 扇出 + auto-commit + 物理名改写（现状），mq 面 pull 会话 +
  显式 commit/ack + 物理名=逻辑名（`kafkas:`/`rabbits:` v1 无 topic_prefix 字段）。
  bus/mq 的 handle 空间在插件内**分开编号**（各自 map，互不共用）。Core 化拆分时
  **顺带修复 bus `close(handle)` 不停 detach 消费任务的既有泄漏**（写明为修复，
  非行为回归；`oj-bus-kafka` close 现只删 map，spawn 的 stream 任务持 consumer 不放）。
- **监督器位置取舍（评审 nit）**：放 `oj/`（装配层）——线程模型先例在 server/
  （actor/ws/introspector），但任务池的启动/停机生命周期与装配（插件/registry 构建）
  同寿，且不依赖 axum；`oj test` 子命令**不**拉起任务池。

## 3. config schema

```yaml
kafkas:                        # 段存在即启用；key = 实例名 → Kafka(name)
  default: { brokers: [b1:9092, b2:9092], group: g1 }
  audit:   { brokers: [b3:9092] }
rabbits:                       # key = 实例名 → RabbitMQ(name)
  default: { url: "amqp://..." }            # 或 brokers: [amqp://...]
tasks:                         # 长任务池；缺省 = 不启用
  dir: tasks                   # 相对 api 根（release 相对 dist）；递归扫描
  max: 64                      # 任务数上限，超出 fail-fast（每任务=1 线程+1 V8 isolate）
  stop_grace_secs: 30          # 优雅停机宽限（默认 30）
```

- cfg 值为 **JSON 透传**（`HashMap<String, serde_json::Value>`，同 `plugins:` 段）；
  装配层按段来源向 cfg 注入 `"kind": "kafka" | "rabbit"`，插件自检不符即**拒绝装配**。
- **kind 路由（评审 F7）**：沿用 bus 的按插件名推断——`oj-bus-<kind>` 提供该 kind 的
  mq 轴；段声明但无对应插件 → 装配 fail-fast（文案先例 `"no kv plugin loaded"` 风格）。
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
const md = await k.metadata();     // 可选 method；未实现 → Err("unsupported method")

const r = RabbitMQ("default");
await r.publish("ex", "rk", payload, { headers });   // publish = send 的 rabbit 命名
const batch = await r.poll(["q1"], { max: 10, timeoutMs: 1000 });  // = 循环 N 次 basic.get
await r.ack(msg);
await r.nack(msg, /* requeue */ false);
```

- **消费会话归属（评审 M2，must-fix）**：`poll` / `commit` / `ack` / `nack` 是有状态
  操作，**仅任务上下文可用**（`bus.subscribe` 仅 WS 上下文的直接先例；HTTP/WS 调用
  → JsError）；且**每实例至多一个活跃 poller**，第二个并发 poll → Err（防 rdkafka
  分区互偷 / commit 竞争）。`send` / `publish` / `metadata` / `kind` 无状态，
  全上下文可用。
- **注册表形态（评审 N2）**：kafkas / rabbits **各自一个** `NamedRegistry`（对应两个
  全局各查各的），`Kafka("x")` 与 `RabbitMQ("x")` 并存合法；实例对象 JS 侧缓存保证
  同一性（同 `dbCache`）。
- `bus.publish/subscribe` 高层广播**保持不动**（窄接口）。
- 任务上下文新增全局 `tasks`：`tasks.stopping` → `bool`。实现为 StableState 上的
  `Option<Arc<AtomicBool>>`——任务 Bridge 注 `Some`，HTTP/WS Bridge 注 `None`
  （恒 false），语义精确对齐且无需运行时上下文判断（评审采纳项）。
- 任务上下文 `http` 为空上下文；`json.ok` 可调但输出丢弃。任务典型形态：

```js
// tasks/task_orders.ts —— 单文件长驻任务（ESM，见 §6 执行模型）
const k = Kafka("default");
while (!tasks.stopping) {
  const batch = await k.poll(["orders"], { max: 100, timeoutMs: 1000 });
  for (const m of batch) {
    await db.query("insert ...", [m.value.orderId]);
    await k.commit(m);
  }
}
```

- **超时与停机互动（评审 N3）**：`timeoutMs` ≫ `stop_grace_secs` 的任务会在 poll
  挂梦中被强杀——at-least-once 下安全（未 commit 必重投）；文档写明示例 1s 为
  合理上界。**顺序语义**：per-partition 顺序仅在「处理完 → commit → 再 poll」的
  串行形态下成立，api-manual 落一句。

## 5. mq 轴 FFI 契约（**ABI 保持 7**，评审 M1/F1 修订）

```rust
#[repr(C)]
pub struct MqVTable {
    connect: fn(cfg: RString) -> FfiFuture,   // cfg JSON（含 kind）→ handle
    call: fn(handle: u64, method: RString, payload: RString) -> FfiFuture,  // → json
    close: fn(handle: u64),
}
```

- **ABI_VERSION 保持 7**：按轴 dlsym 正是 ABI 7 引入的机制（`lib.rs:41`「加轴自此
  零破坏」；`plugin_loader.rs:429`「加新轴 = 此表加一行……插件零感知、零重编译」）。
  MqVTable 是全新类型，无任何既有 repr(C) 形状变更 → bump 违反仓库自己的法则且
  强制第三方重编译。宿主侧改动面：`AXES` 加 `"mq"` + `probe_axes` 加臂 +
  `Registrations` 加宿主字段（非 repr(C)）。
- **`oj_plugin_entry!` 宏无需改动（评审 N1）**：宏轴无关（`$axis:ident` paste），
  插件侧直接 `mq => &MQ_VT` 即可。
- 复用 bus 轴既有的 `RString` / `FfiFuture` / handle 惯例与 `FfiGuard` 配对语义；
  FFI 边界只能 JSON-in-RString（stabby IStable 限制），JS 侧预 stringify 留待 profile。
- 方法面统一：`kind` / `send` / `poll` / `commit` / `ack` / `nack` / `metadata`（可选）/
  `dlq`（预留）。`send` 为唯一发送 method（kafka payload 带 `topic`，rabbit 带
  `exchange`/`routingKey`）；可选 method 未实现 → `call` 返回 `Err`，走既有
  Err→JsError 通路。
- **长轮询退避（评审 F4，must-fix 级实现项）**：`await_ffi` 是 `yield_now` 忙轮询
  （`ffi.rs:146`），mq `poll(timeoutMs)` 是第一个长挂用户——等待期烧满一核。
  mq 路径改用带 sleep 的轮询变体（`await_ffi_poll`：poll→0 时 `tokio::time::sleep`
  ~10ms 退避再试），bus 快操作路径不动。
- **fail-fast 边界修正（评审 S3）**：cfg 校验（brokers/group/url 缺失）→ 装配期
  fail-fast（先例：kv/blob 的 `*_backend_connect`）；**kafka 可达性是惰性的**
  （rdkafka create 离线构造）——连接错误在任务首轮 poll 出现，由监督重启兜底；
  rabbit（lapin 拨号）装配期即可探活 fail-fast。不做装配期 metadata ping
 （避免装配依赖集群可达）。

## 6. 长任务池生命周期（`oj/src/tasks.rs` + `src/bridge/runtime.rs` 扩展）

- **执行模型（评审 F3/F4，must-fix）**：任务文件走 **ESM side-module 常驻驱动**
  （`boot_runtime` 同款：`load_side_es_module_from_code` → 先 `mod_evaluate` 后
  `run_event_loop`）。经典 script 路径（`execute_script`）不支持顶层 await，跑不了
  `while (!tasks.stopping) { await poll... }`。**任务文件一律按 ESM 加载**——驱动
  显式指定，不走 `looks_cjs` 启发式（无 import/export 的任务文件会被误判 CJS 包装
  进非 async 函数，TLA 即 SyntaxError，`mod.rs` boot 测试注释有先例）。每帧 `await
  poll` 的 pending op 维持事件循环，不触发「TLA never resolved」排空错误；**崩溃
  信号 = eval future Err** → 监督重启。任务执行**不武装 handler 超时**
  （KillSwitch 现为 Bridge 私有、`pub(crate)`——新增 `Bridge::kill_after(grace)`
  面供监督器停机用）。
- **线程模型**：每任务文件 = 专用 OS 线程 + current_thread runtime + 独立 Bridge
  （三处既有同款先例）；失败 runtime 丢弃不归还（红线 5）。
- **监督**：启动即拉起全部任务；panic / JS 顶层错误退出 → 指数退避重启
  （1s → 2s → 4s … cap 60s，成功运行归零），重启日志含 attempt 数。
- **优雅停机（评审 F5/S2，全仓首个信号处理器）**：完整序列——
  ① `tokio::signal` 捕获 SIGINT + SIGTERM（unix::signal，Windows 仅 ctrl_c）
  ② 进程级 `tasks.stopping`（AtomicBool）置位
  ③ `stop_grace_secs` 宽限：任务循环检测退出；到期 `kill_after` → terminate_execution，
     **本线程兜一轮 event loop 再 drop runtime**（SIGSEGV 纪律，`runtime.rs:100-113`）
  ④ mq handle 经 Drop→vtable.close 回收（`FfiEventBroker::drop` 既有通路同款）；
     任务正常退出路径走显式 close，terminate/Drop 为兜底
  ⑤ HTTP 侧 `axum::with_graceful_shutdown` 由同一信号触发排空在途请求（axum 侧
     亦为新地基），⑥ 进程退出。
- **热重载**：v1 无（见 §1 非目标）；转译缓存按 (path, mtime) 自动失效，重启进程即生效。
- **启动日志**（逐任务 + 汇总；崩溃重启/停机同落）：

```
task: orders (task_orders.ts) → started
task: audit  (audit_task.ts)  → started
task: 2 task(s) → started
task: orders crashed, restart in 2s (attempt 3)
task: orders → stopped
```

## 7. 错误与重试语义

- 消息语义 **at-least-once**：poll → 处理 → commit/ack 三段，提交前崩溃必重投。
- 装配期：cfg 校验失败 / 段声明无对应 kind 插件 → fail-fast 拒启（不吞错）。
- 运行期：broker 断连 → `call` Err → JsError，由任务循环自行重试；框架只管线程级
  监督（退出 → 退避重启），不替业务做消息级重试。
- `Kafka("未配置名")` → `undefined`；对 undefined 调方法即普通 TypeError（同 `DB`）。

## 8. 测试矩阵

| 层 | 内容 |
|---|---|
| oj-plugin-ffi | mini 夹具插件扩 mq 轴：ABI 7 不变过门禁 / 缺符号=不提供 / catch_unwind 路径 / 同一 handle 并发 call（vtable 线程安全契约）/ poll 中途 await 取消（FfiGuard Drop free 不 take，`ffi.rs:161-172`） |
| core | `InMemoryMq`（内存队列实现同一 method 面）：op 面 / 命名门禁 / JS 缓存同一性 / `Kafka("x")` 与 `RabbitMQ("x")` 并存（双 registry）/ poll·commit 任务上下文门禁 / `tasks.stopping` 双态（Some/None） |
| core | 任务驱动：**TLA 常驻执行**（while+await 循环可跑可停）/ 崩溃（eval Err）→ 监督重启（退避）/ kill_after 强杀 + event loop 兜底收场 / 命名约定扫描与非任务文件不执行 / max 超限 fail-fast |
| plugins | kafka / rabbit roundtrip（真实 broker，feature-gated，沿用现有模式）；bus 轴既有测试回归；**bus close 泄漏修复回归** |
| oj 装配 | kafkas/rabbits 注入时序（StableState 首跑前）、kind 不符 fail-fast、段无对应插件 fail-fast、**rabbits: 只装 kafka 插件负例**、同名冲突 fail-fast |
| oj build | **tasks 目录镜像 → dist/tasks/**（转译保留命名约定；不进 tgz/manifests） |
| e2e | mini mq 插件 + 任务文件消费 → 写 kv → 断言；停机协议端到端（SIGTERM → 任务收场 → 退出） |

## 9. 实现切分（供 writing-plans 展开）

1. **FFI 契约**：mq 轴 vtable + 宿主 AXES/Registrations/probe 扩展 + mini 夹具扩展
   （ABI 不动）。
2. **插件 Core 化**：两插件拆 Core / 双轴适配（bus 回归护栏先行；顺带修 bus close 泄漏）；
   `await_ffi` 长轮询退避变体。
3. **宿主 mq.rs**：双 registry + op 面（含任务上下文门禁与单 poller 互斥）+
   bootstrap 双全局 + 装配注入（kafkas/rabbits，kind 路由与 fail-fast）。
4. **core 任务驱动**：ESM 常驻执行路径 + `Bridge::kill_after` + event loop 兜底收场。
5. **长任务池**：`oj/src/tasks.rs` 监督器 + config `tasks:` + 扫描约定 + 信号处理 +
   停机序列 + 日志。
6. **build**：tasks 目录转译镜像 → dist/tasks/。
7. **文档与示例**：api-manual（Kafka/RabbitMQ/tasks 章 + 顺序语义/超时互动）、
   sample 任务示例、global.d.ts 补声明。

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

## 11. 评审修订记录（2026-09-07，架构师 + 开发专家双评审）

| # | 级别 | 修订 |
|---|---|---|
| M1/F1 | must | **ABI 保持 7**（原 spec bump 8 与「加轴零破坏」法则自相矛盾）；宿主改动面收窄为 AXES/Registrations/probe |
| M2 | must | 新增「消费会话归属」：poll/commit/ack/nack 仅任务上下文 + 每实例单活跃 poller；send/publish/metadata 全上下文 |
| F2/S5 | must/should | §9 补 build 工作项：tasks → dist/tasks/ 镜像（不进 tgz/manifests）；OCP 表述修正（三处固定触点，v1 固定两 kind） |
| F3/S4 | must/should | 执行模型钉死：ESM side-module 常驻驱动 + 任务文件一律按 ESM（绕过 looks_cjs）；§9 补 core 任务驱动工作项；不武装 handler 超时 |
| F4 | should | `await_ffi` 忙轮询烧核 → mq 专用 sleep 退避轮询变体 |
| F5/S2 | should | 停机序列写实（首个信号处理器 + `Bridge::kill_after` 新面 + SIGSEGV 收场纪律 + mq handle Drop 回收 + axum graceful） |
| F6 | should | **任务热重载剪出 v1**（api 热更是拉式失效，无可复用 watcher）→ 非目标 |
| S1 | should | Core 边界写实（消费路径 bus-push / mq-pull 分列；handle 空间分开；kafkas 段无 topic_prefix）；顺带修 bus close 泄漏（标注为修复） |
| S3 | should | fail-fast 措辞修正：cfg 校验 fail-fast；kafka 可达性惰性由监督兜底；rabbit 拨号可装配期探活；不做装配期 ping |
| F7 | should | kind 路由：`oj-bus-<kind>` 按名推断（沿 bus 先例）；段无对应插件 fail-fast |
| N1-N4 / nits | nit | 宏无需改；双 registry（同名并存合法）；测试矩阵补并发 call/取消路径/负例/build 行；poll timeout 与 grace 互动、rabbit poll=N×get、顺序语义、`oj test` 不拉任务池、max 上限 |
