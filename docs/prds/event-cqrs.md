# 基于事件驱动与可选 CQRS 的执行体池化需求说明（v2 评审修订版）

> 修订记录（2026-09-25）：经专家工程师 + 专家架构师双评审后修订。核心变化：
> ① 方案 A 收窄为「任务域事件化」，HTTP 业务路由与 WS 保持直调（现有 actor 池 / RoutePool 语义上已是执行体池，不重建）；
> ② 方案 B（CQRS 全量）整案移入附录 backlog，不排期；投影器 / Event Store / Saga 明确不做；
> ③ FR-LT-008「无论因何退出必定 teardown」降级为可达承诺（强杀路径由 Rust Drop 兜底）；
> ④ 存量 TLA 任务与新 setup/loop_body/teardown 任务双模式共存，`tasks.rs` 监督器原样保留；
> ⑤ 事件信封薄化为 4 字段可扩展契约，B 阶段字段不预支；
> ⑥ 事件池后端复用现有 mq/bus 插件轴（零 ABI 变更），rapidmq/mq-bridge 移出；
> ⑦ 演进路线由 6 阶段压缩为 2 阶段 + 触发式 backlog；
> ⑧ 补充阻断级遗漏：管理 API 鉴权/租户隔离、scriptPath 白名单、背压语义、dev 热重载交互、devkit 文档红线。

## 1. 文档说明

### 1.1 目的

将长时任务、定时任务纳入统一事件模型，复用现有 Worker 池模式执行，降低 JS Runtime 资源占用；管理 API 以最小 RESTful 集提供任务生命周期管理。方案 B（CQRS）作为远期可选项保留原案于附录，不在本期排期。

### 1.2 范围

- **本期（阶段 1，必选）**：事件模型（薄信封 + 内存事件池）、任务执行体池化（复用 Scheduler/Worker 模式）、长时任务 setup/loop_body/teardown 生命周期（与存量 TLA 任务双模式共存）、crontab 单机调度、最小任务管理 API、鉴权与路径白名单、文档同步。
- **暂缓（backlog，触发式）**：见 §9 与附录 B。
- 涉及现有代码：`oj/src/tasks.rs`（原样保留）、`src/bridge/frame_pool.rs`（复用）、`src/bridge/runtime.rs`（复用）、`server/src/actor.rs`（HTTP 直落不动）、`server/src/ws.rs`（不动）。

### 1.3 读者

架构师、后端开发、前端/API 使用方、运维、测试。

### 1.4 核心原则

1. **复用优先，不新建池**：HTTP actor 池与 WS RoutePool 已是「队列 + 有限 Worker」的成熟实现；本期只做模式泛化，不做第三套池。
2. **任务域事件化，热路径不事件化**：HTTP 业务路由、WS 帧保持直调；事件模型只覆盖异步、无请求-响应语义的面（任务执行、任务控制面、mail.enqueue 类异步完成回调、bus 扇出）。
3. **双模式兼容**：存量 TLA 任务维持一任务一线程监督模型，新结构任务进池；不强制迁移。
4. **薄信封、可扩展**：事件信封 4 字段起步，向后兼容靠 serde 扩展字段，不预支 CQRS 字段。
5. **承诺可达**：只承诺能兑现的可靠性（teardown 降级承诺、内存事件池不追消息级可靠性）。
6. **零 ABI 变更**：事件池后端复用现有 mq/bus 插件轴，不开新轴、不 bump ABI。

---

## 2. 背景与问题

- 当前 `tasks.rs` 中 task 是长循环体，每个任务占用一个 OS 线程 + 独立 Bridge（`oj/src/tasks.rs:108-116`），任务多时资源消耗高，Runtime 复用率低。
- `frame_pool.rs` 的 RoutePool 已实现「Scheduler 排队 + W 个无状态 Worker 共享 + 会话表外置」的池化模型（多连接共享 Worker、超时毒化半径=1、自动补员），与帧语义无关的部分可直接泛化到任务执行。
- 全仓无 crontab 支持（`crontab|cron` 零命中），定时调度为空白。
- 任务管理与任务执行、状态变更与状态查询目前耦合在线程模型内，缺少状态查询与管理 API。

### 2.1 现状事实修正（评审输入）

| PRD 原假设 | 代码现状 | 修订 |
|---|---|---|
| 执行体池待建 | HTTP：`JsActor::pool(n)` = N 线程 × Bridge × `RuntimePool`（`server/src/actor.rs:71`、`src/bridge/runtime.rs:35`）；WS：`RoutePool`（`src/bridge/frame_pool.rs:163`） | 复用，不新建 |
| 背压/超时/错误隔离待建 | mpsc(64) 背压、`KillSwitch` 看门狗、失败 runtime 丢弃不归还均已具备 | 复用 |
| 事件池需 MQ 后端 | `EventBroker` 为 WS 扇出总线（无 ack/重试/死信）；mq 轴有 poll/commit/ack/nack 但门禁在任务上下文（`src/bridge/mq.rs:113`） | 本期内存队列；MQ 后端入 backlog，复用 mq/bus 轴 |
| rapidmq / mq-bridge | 全仓（源码、Cargo.toml、docs）零命中 | 移出配置项与本期范围 |

---

## 3. 总体目标

| 编号 | 目标 |
|---|---|
| G1 | 建立任务域事件模型：薄信封（4 字段）+ 内存事件池（per-topic 队列） |
| G2 | 长时任务池化：复用 Scheduler/Worker 模式，`setup / loop_body / teardown` 生命周期，多任务 loop_body 交替执行 |
| G3 | 定时任务调度：解析 `crontab.yaml`，到点触发入池，执行完释放 Worker |
| G4 | 双模式兼容：存量 TLA 任务监督模型原样保留，新结构任务进池 |
| G5 | 任务控制面走命令语义（start/stop/reload/run-once 为意图而非即时结果）；HTTP 业务路由与 WS 保持直调 |
| G6 | 提供最小 RESTful 任务管理 API：查询、启停、立即触发、状态 |
| G7 | teardown 在自然停止与可捕获异常路径保证执行；强杀路径由 Rust Drop 兜底释放资源 |
| G8 | 可观测性最小集：任务状态、执行日志、执行历史可查 |
| G9 | 管理 API 走现有 AuthGuard + 租户管线；scriptPath 约束在 api-path 根内 |
| G10 | 事件信封可扩展，为远期 backlog 项（MQ 后端、命令追踪）留插槽，不预支实现 |

---

## 4. 总体架构

### 4.1 分层架构（本期）

```text
┌──────────────────────────────────────────┐
│ 接入层：任务管理 API（走 AuthGuard 管线）  │
├──────────────────────────────────────────┤
│ 事件模型层：内存事件池（per-topic 队列）   │
│ 薄信封 eventId/eventType/timestamp/payload│
├──────────────────────────────────────────┤
│ 执行体池层：TaskPool                       │
│ 复用 Scheduler 排队 + W Worker 线程       │
│ （每 Worker 预载任务模块，模块作用域 = ctx）│
├──────────────────────────────────────────┤
│ 任务模型层：长时任务（新结构）/ 定时任务   │
│ 内存状态机 + 可选 append-only 事件日志     │
├──────────────────────────────────────────┤
│ 存量层：TLA 任务监督器（tasks.rs 原样）    │
└──────────────────────────────────────────┘
```

### 4.2 数据流

```text
crontab.yaml 到点 / 管理 API 命令 / 任务状态转换
        │
        ▼
   内存事件池（topics: tasks.commands / tasks.execution / tasks.events）
        │
        ▼
  TaskPool（Scheduler 轮转 + W Worker，复用 frame_pool 模式）
        │
        ▼
  setup → 循环 loop_body（多任务交替）→ teardown
        │
        ▼
  状态机更新 + 事件日志追加 + 管理 API 查询
```

### 4.3 明确不在本期

- HTTP 业务路由（`/v1/api/*`）继续直落 `JsActor` 池；WS 继续走 RoutePool。二者语义上已是执行体池，事件化只会给热路径增加一跳延迟与失败面。
- CQRS 全套（Command/Query Bus、投影器、读模型、Event Store、Saga）——见附录 B，不排期。

---

## 5. 核心概念与术语

| 术语 | 说明 |
|---|---|
| 事件 Event | 已发生的事实，如 `task.created`、`task.execution_started`。**信封仅 4 字段：** `eventId`、`eventType`、`timestamp`、`payload`（serde 可扩展，远期字段加在 payload 层不破坏兼容） |
| 命令 Command | 写意图（控制面），如 `task.stop`。本期直接作用于内存状态机；不引入独立 Command Bus |
| 事件池 Event Pool | 进程内 per-topic mpsc/VecDeque 队列，fire-and-forget；任务状态权威在 Rust 侧状态机表，不追消息级可靠性 |
| 执行体池 TaskPool | 复用 `Scheduler` 排队 + W 个无状态 Worker 线程；每 Worker 一个 Bridge，任务模块预载一次，模块作用域即任务 ctx |
| 任务状态机 | `LongTask` / `CronTask` / `TaskExecution` 三种聚合，内存权威，重启重建（任务定义 source of truth = 文件系统） |
| 事件日志 | 可选 append-only 记录（JSONL 文件或 db 表），用于执行历史与审计，非 Event Sourcing |

---

## 6. 功能需求

### 6.1 事件模型与内存事件池

| 编号 | 需求 |
|---|---|
| FR-EB-001 | 抽象生产者、消费者、事件、主题；进程内 per-topic 队列 |
| FR-EB-002 | 仅内存模式（本期）；MQ 后端（复用 mq/bus 插件轴）入 backlog |
| FR-EB-003 | 逻辑主题：`tasks.commands`、`tasks.execution`、`tasks.events` |
| FR-EB-004 | 事件不可变，只追加 |
| FR-EB-005 | 事件信封 4 字段：`eventId`、`eventType`、`timestamp`、`payload`；扩展字段经 serde default 向后兼容 |
| FR-EB-006 | 优雅停机：未消费消息随进程退出丢弃（内存语义），状态权威在状态机表，重启后从文件系统 + 状态机重建 |
| FR-EB-007（暂缓） | ack、重试、死信、延迟消息、消费者组、事件重放 —— 随 MQ 后端一并入 backlog |
| FR-EB-008（移除） | rapidmq / mq-bridge 调研 —— 全仓零命中，从配置项移除；远期如需要，经 mq 轴插件接入 |

### 6.2 执行体池（复用现有设施）

| 编号 | 需求 |
|---|---|
| FR-EP-001 | TaskPool = `Scheduler` 排队（per-task 在飞 ≤1，保序）+ W 个无状态 Worker 线程；模式复用 `frame_pool.rs`，不新建池基础设施 |
| FR-EP-002 | 每 Worker 一个 Bridge；`RuntimePool` 的 checkout/checkin/boot/KillSwitch 原样复用 |
| FR-EP-003 | Worker 预载任务模块一次（同 `WsSession` 预载 ws.ts）；多次 loop_body 之间经模块作用域保持状态（V8 模块缓存，`?v=mtime` 不变则零重编译） |
| FR-EP-004 | loop_body 驱动语义钉死：**同 runtime 上重复调用模块导出函数**，不重新 import/评估；ReqState 每次调用重置 |
| FR-EP-005 | 单任务异常不得拖垮整个池：loop_body 抛错 → teardown → 毒化半径=1（KillSwitch 模式现成），Worker 自动补员 |
| FR-EP-006 | 背压语义：事件池队列深度可配，满时策略 = 阻塞生产者（控制面命令）/ 拒绝并返回 503（HTTP 入口） |
| FR-EP-007 | Worker 数、队列深度为配置位（现有 `DEFAULT_MAX_IDLE=16` 为常量，本期补配置） |
| FR-EP-008 | 执行结果写入状态机表并追加事件日志：`execution_started/succeeded/failed/stopped` |

### 6.3 长时任务（双模式）

| 编号 | 需求 |
|---|---|
| FR-LT-001 | 存量 TLA 任务（`while(!tasks.stopping()){...}`）维持 `tasks.rs` 监督模型原样：一任务一线程、指数退避重启、停机 flag + grace 看门狗。**不做迁移** |
| FR-LT-002 | 新结构任务文件导出 `setup(ctx?)` / `loop_body(ctx?)` / `teardown(ctx?)`；`loop_body` 为单次执行，不含循环语句 |
| FR-LT-003 | 框架组装生命周期：预载时执行 `setup` → Scheduler 轮转分发 `loop_body`（多任务交替执行，避免饥饿）→ 停止/退出时执行 `teardown` |
| FR-LT-004 | 文件命名沿用 `task_{name}.{ts,js}` / `{name}_task.{ts,js}` 约定；扫描器按导出结构判别模式（有 `loop_body` 导出 → 池化模式，否则 → TLA 监督模式） |
| FR-LT-005 | **teardown 可达承诺**：自然停止（stop 命令 → loop_body 返回 → teardown）与可捕获异常路径保证执行 teardown；看门狗强杀路径（terminate_execution 后 isolate 已终止）JS 无法执行，由 Rust 侧 Drop/closer 兜底释放资源（先例：`MqInstance::drop`，`mq.rs:50`）；不承诺 SIGKILL 场景 |
| FR-LT-006 | 任务不长期独占 Worker：loop_body 单次执行有看门狗超时（可配），超时毒化半径=1 |
| FR-LT-007 | 任务绑定 Worker 直至 teardown，**不允许跨 Runtime 迁移**——ctx 即模块作用域，天然不可序列化，消灭 FR-LT-011（原案）的序列化问题 |
| FR-LT-008 | dev 模式热重载与池化任务交互：任务文件变更 → 该任务走 reload 语义（teardown → 重载模块 → setup），需与 dev notify 管线对齐 |

### 6.4 定时任务

| 编号 | 需求 |
|---|---|
| FR-CRON-001 | 通过 `task/crontab.yaml` 配置，每行一个配置 |
| FR-CRON-002 | 格式：`<5 段 cron 表达式> <脚本路径>`，示例 `*/5 * * * * ./aa/bb/cc/dd/xx.ts`；宏（@daily）与秒字段显式报错，只支持标准 5 段 |
| FR-CRON-003 | 解析 cron 表达式，计算下次执行时间；自研 5 字段展开（~150 行）或引 `cron` crate，二选一按实现期评审 |
| FR-CRON-004 | 调度循环：单 tokio 任务 + 按 next-fire 排序唤醒，到点生成执行事件入池 |
| FR-CRON-005 | 执行完成即释放 Worker，不长期占用 |
| FR-CRON-006 | 支持立即触发（run-once）、启停、修改、删除、查询下次执行时间 |
| FR-CRON-007（暂缓） | 分布式锁、幂等、命令 ID 防重复触发 —— 单机为本期目标；分布式场景用 kv 轴 `SET NX PX`（~20 行增量），入 backlog |

### 6.5 接入边界

| 编号 | 需求 |
|---|---|
| FR-UNI-001 | HTTP 业务路由、WS 帧**不纳入**事件模型，保持直调（actor 池 / RoutePool） |
| FR-UNI-002 | 任务控制面（管理 API 的写操作）以命令语义作用到状态机；查询直读状态机 |
| FR-UNI-003 | 长时任务、定时任务：管理走管理 API，执行走事件池 + TaskPool |
| FR-UNI-004 | `mail.enqueue` 异步完成、`bus.publish` 扇出可挂同一事件信封（主题命名对齐），作为事件池的第二类生产者/消费者 |
| FR-UNI-005 | 接入层不直接操作 Runtime，只与事件池/状态机交互 |

### 6.6 任务管理 API（最小集）

| 编号 | 接口 |
|---|---|
| FR-API-001 | `GET /api/tasks?type=long\|cron` — 任务列表（状态机只读视图） |
| FR-API-002 | `GET /api/tasks/{id}` — 任务详情 + 当前状态 |
| FR-API-003 | `GET /api/tasks/{id}/logs` — 执行历史（事件日志投影） |
| FR-API-004 | `POST /api/tasks/{id}/start` / `stop` / `reload` — 生命周期命令 |
| FR-API-005 | `POST /api/tasks/cron/{id}/run-once` — 立即触发 |
| FR-API-006 | `POST /api/tasks/cron/{id}/enable` / `disable` — 启停 |
| FR-API-007 | `PATCH /api/tasks/{id}` / `DELETE /api/tasks/{id}` — 修改/删除（cron 配置与任务注册） |

安全与隔离（阻断级，验收条件）：

- FR-API-AUTH-001：全部管理端点走现有 `AuthGuard` 管线（`server/src/lib.rs` 的 `Pipeline`），匿名规则显式配置，默认拒绝；
- FR-API-AUTH-002：多租户下任务归属租户作用域，事件主题带租户前缀（`{tenant}.tasks.*`）；实例级任务（无租户）为主题默认命名空间；
- FR-API-SEC-001：`scriptPath` 白名单——仅允许 api-path 根内相对路径，拒绝 `..` 穿越（复用静态服务的穿越防护逻辑），否则等于任意文件执行。

### 6.7 可观测性（最小集）

| 编号 | 需求 |
|---|---|
| FR-OBS-001 | 任务状态、执行日志、错误记录可查询（管理 API + tracing） |
| FR-OBS-002 | TaskPool 基础指标：Worker 数、在飞任务数、执行次数、失败次数——经状态机表暴露于查询 API |
| FR-OBS-003（暂缓） | 指标/追踪体系（prometheus/OTel）、事件池积压/速率/死信指标 —— 独立工程，入 backlog |

### 6.8 配置

```yaml
tasks:
  dir: task            # 沿用
  max: 64              # 沿用（新结构 + TLA 合计）
  stop_grace_secs: 30  # 沿用
  pool:
    workers: 4         # 新增：TaskPool Worker 线程数
    queue_depth: 256   # 新增：事件池 per-topic 深度
    loop_body_timeout_ms: 5000   # 新增：单轮 loop_body 看门狗
  crontab: task/crontab.yaml     # 新增：调度配置文件
  event_log:           # 新增：可选执行历史落盘
    enabled: true
    path: logs/task-events.jsonl
```

---

## 7. 数据模型

### 7.1 任务定义（文件系统为 source of truth）

| 字段 | 说明 |
|---|---|
| name | 任务名（文件约定推导，唯一） |
| mode | `tla`（存量）/ `loop`（新结构） |
| scriptPath | 脚本路径（api-path 根内） |
| cronExpr | cron 表达式（定时任务） |
| enabled | 是否启用 |

### 7.2 执行实例（内存状态机 + 可选事件日志）

| 字段 | 说明 |
|---|---|
| executionId | 执行 ID |
| taskId / taskName | 任务标识 |
| status | `pending` / `running` / `succeeded` / `failed` / `stopped` |
| startedAt / finishedAt | 时间 |
| error | 错误信息 |
| workerId | 执行 Worker |

### 7.3 事件信封（薄信封，可扩展）

```json
{
  "eventId": "evt-123",
  "eventType": "task.execution_started",
  "timestamp": "2026-09-25T12:00:00Z",
  "payload": { }
}
```

远期字段（`aggregateId`、`commandId`、`version`、`source`）通过 payload 层或 serde default 扩展，不破坏兼容。

---

## 8. 非功能需求

| 类别 | 需求 |
|---|---|
| 性能 | 有限 Worker 支持多任务交替执行；loop_body 轮转不引入跨线程切换开销（Worker 内串行） |
| 可靠性 | teardown 可达承诺见 FR-LT-005；Worker 崩溃自动补员；事件池不承诺消息级可靠性（状态机权威） |
| 可用性 | 单任务异常不影响池（毒化半径=1）；TLA 任务崩溃不影响 TaskPool（反之亦然，双模式隔离） |
| 兼容性 | 存量任务文件零改动；`tasks.rs` 监督器零改动；HTTP/WS 行为零变化 |
| 安全性 | 管理 API 鉴权（FR-API-AUTH）；scriptPath 白名单（FR-API-SEC）；资源限额沿用 tasks.max |
| 一致性 | 本期强一致（内存状态机）；无最终一致问题 |
| 调度公平 | 多任务 loop_body 经 Scheduler 轮转交替，避免饥饿（复用 per-conn 保序排队语义） |
| 文档 | 版本发布须同步 CHANGELOG.md + `docs/devkit/` 四件套（CLAUDE.md 红线，验收条件） |

---

## 9. 演进路线

| 阶段 | 名称 | 是否必选 | 内容 | 目标 |
|---|---|---|---|---|
| 1 | 任务域事件化 | 必选 | 薄信封、内存事件池、TaskPool（复用 Scheduler/Worker）、长任务双模式、crontab 单机调度、最小管理 API（含鉴权/白名单）、事件日志 | 解决资源占用，提供管理能力 |
| — | backlog（触发式） | 可选 | ① MQ 后端（复用 mq/bus 插件轴，含 ack/重试/死信/消费组）；② 命令追踪（`commandId` 返回 202+statusUrl，仅当管理面需要异步追踪时）；③ 分布式 cron 锁（kv 轴 SET NX）；④ 指标/追踪体系（prometheus/OTel）；⑤ 管理 API 命令/查询命名空间形态 | 真实需求出现时逐项立项，每项独立评审 |

**明确不做**（评审裁决，除非出现推翻性新需求）：CQRS 写/读模型分离、投影器、Event Store、Saga/Process Manager、HTTP/WS 全域事件化、rapidmq 适配。理由：任务系统聚合仅三种、不变式近零，读模型即状态机即时投影；FR 原案「关闭 CQRS 系统完全正常」已证明该层不承载真实复杂度。

---

## 10. 验收标准（本期）

- 新结构任务（setup/loop_body/teardown）在有限 Worker 下可多任务交替执行；存量 TLA 任务行为与 v0.1.x 完全一致（回归通过）。
- 定时任务到点自动入池执行，执行完成释放 Worker；run-once/enable/disable 可用。
- 任务管理 API 全部端点经 AuthGuard 鉴权；无租户凭据访问租户任务被拒。
- scriptPath 越出 api-path 根的创建请求被拒。
- 自然停止与异常路径 teardown 必执行（e2e 钉）；看门狗强杀路径 teardown 不承诺、资源由 Rust Drop 释放（Drop 行为有测试钉）。
- 任务状态、执行历史可查询；事件日志落盘（启用时）。
- `cargo fmt --check`、`cargo clippy --release --all-targets -- -D warnings`、`cargo test --release --workspace` 全绿；CHANGELOG 与 `docs/devkit/` 四件套同步，`cargo xtask build` 归置 bin/devkit/ 且 devkit 契约用例通过。

---

## 11. 风险清单

| 级别 | 风险 | 缓解 |
|---|---|---|
| 高 | 存量与新结构任务判别逻辑误伤（共享库文件、无导出文件） | 扫描器仅以「存在 `loop_body` 导出」为判别条件；BDD 测试钉（helpers.ts、空导出文件、双写冲突） |
| 高 | loop_body 单次执行过长阻塞 Worker | loop_body_timeout_ms 看门狗 + 毒化半径=1（KillSwitch 现成模式） |
| 中 | 模块作用域 ctx 与 ReqState 重置的交互（如 loop_body 内 db 事务） | FR-EP-004 钉死驱动语义；活跃事务在 ReqState reset 时 drop 回滚（现有行为） |
| 中 | 事件池语义 creep（需求方要求 ack/重试/死信） | 本期范围评审锁定；真实需求走 backlog ①（MQ 后端） |
| 中 | dev 热重载与池化任务交互 undefined | FR-LT-008 已列；实现前补设计评审 |
| 低 | cron 表达式边界（DST、宏、秒字段） | 仅支持标准 5 段，其余显式报错（FR-CRON-002） |
| 低 | 事件日志无限增长 | 文件滚动策略对齐现有 logging（logs_max_m/logs_keep_files） |

---

## 附录 A：示例流程

### A.1 新结构长任务流程

```text
扫描 → 判别为 loop 模式 → 预载模块（执行 setup）
  → Scheduler 轮转分发 loop_body（多任务交替）
  → stop 命令 / loop_body 抛错 → 执行 teardown → 状态机 + 事件日志
  → 看门狗强杀 → teardown 不承诺，Rust Drop 兜底
```

### A.2 定时任务流程

```text
解析 crontab.yaml → 注册 CronTask → 计算 next-run
  → 到点生成执行事件 → TaskPool 消费 → 执行脚本
  → 状态机更新 + 事件日志 → 计算下次 next-run
```

### A.3 存量 TLA 任务流程（不变）

```text
tasks.rs 扫描 → 判别为 TLA 模式 → 一任务一线程 + 监督退避重启（原样）
```

---

## 附录 B：方案 B 原案（CQRS，暂缓，不排期）

以下为 v1 原案保留，供远期真实需求出现时参考。**当前评审裁决：不排期、不预支契约字段。** 若未来管理面需要异步命令追踪，仅启用其中 B1 的最小形态（`POST /api/tasks/{id}/stop` 等返回 `{commandId, status: "accepted", statusUrl}`），不做 Command Bus、投影器、读模型分离、Event Store、Saga。

- 命令 API：`POST /api/commands/long-tasks`、`PATCH/DELETE /api/commands/long-tasks/{id}`、`start/stop/reload`；cron 同构。
- 查询 API：`GET /api/queries/tasks?type=*`、`/{id}/status`、`/{id}/logs`、`/cron-tasks/{id}/next-run`、`/executions`、`/runtimes`、`/commands/{commandId}`。
- 事件信封厚化：`aggregateId`、`commandId`、`version`、`source` 字段。
- 事件池 MQ 后端：复用 mq 轴（队列语义：commands/execution）与 bus 轴（扇出语义：events 广播），插件 cfg JSON 声明延迟消息/死信 capability，零 ABI 变更。
- 分布式 cron：kv 轴 `SET NX PX` 锁 + commandId 幂等。
- 读模型/投影器/Event Store/Saga：评审裁决不做；审计由本期事件日志覆盖，读模型重建由「文件系统 + 事件日志」覆盖。

---

**总结：**
本修订版以「任务域事件化 + 复用现有池设施 + 双模式兼容」为落地路径：执行体池复用 Scheduler/Worker 模式，事件模型以薄信封 + 内存事件池起步，teardown 承诺降级到可达边界，存量任务零迁移；CQRS 全量方案移入附录 backlog 不排期。评审遗留唯一待办：实现前补「dev 热重载 × 池化任务交互」的小设计评审。
