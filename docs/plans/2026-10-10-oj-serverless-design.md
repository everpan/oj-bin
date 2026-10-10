# oj Serverless 支持设计方案

- 日期：2026-10-10
- 状态：设计稿（脑暴，暂不实现）
- 范围：本期聚焦 **① 弹性伸缩（Knative Serving 合规）** 与 **② 事件标准化（CloudEvents 1.0）**
- 非目标（本期不做）：③ 自研 FaaS 控制面、④ 函数契约可移植（OpenFunction / Serverless Framework / Lambda runtime API）

## 背景与目标

oj 当前是单进程长驻的 HTTP 服务（`oj serve`），每进程 N 个 actor 线程各持一个 `RuntimePool`
（最多 16 个预热 V8 isolate），共享一个不可变 `StableState`（DB/KV/blob/bus 连接池）。
它已是「函数友好」形态：确定性的 `dist/<module>-<version>/` + `routes.js` + `manifests.yaml` + `.tgz`。

Serverless 的核心是「只写函数、平台管运行/伸缩/缩零/计费」，标准（规范）解决平台与函数间
如何对话，避免厂商锁定。本期目标：让 oj 以**最小改动**获得弹性伸缩能力，并把事件总线升级为
**厂商中立的 CloudEvents 标准**，使 oj 模块既能被 HTTP 调，也能被 Kafka/S3/定时器按标准事件触发。

### 关键约束（必须在设计里正视）

- **隔离偏弱**：isolate 跨请求复用，仅 `ReqState` 被 reset，JS 模块级全局变量会跨请求残留。
  合规做法是约定「handler 模块作用域无副作用」（沿用复用、零成本），本期不引入每调用新 isolate。
- **跨实例共享缺失**：`kv` 默认进程内存、`bus.publish` 仅进程内（见 `docs/ops-manual.md:163-164,259`）。
  任何「多副本 / 缩零后多副本」场景都必须先配 **Redis（会话/KV 共享）+ 外部 broker（事件跨实例）**。
- **预热/启动**：`prewarm_boot` 在 `App::from_config` 期间跑一次 checkout（`oj/src/app.rs:1307`），
  `BOOT_TIMEOUT = 2s`（`src/bridge/runtime.rs:76`）。缩零后冷启动需在就绪探针前完成 boot。

---

## 设计 A：弹性伸缩（Knative Serving 合规）

### 目标
让 `oj serve` 成为合规 Knative Service，零改 V8/桥接核心，即可享受 scale-to-zero 与按流量弹性。

### 组件与改动点

1. **端口注入**（HTTP/配置层）
   - 当前端口来自 `config.yaml`（`oj/src/serve_cmd.rs` 绑定 `addr`）。
   - 改为：若环境变量 `PORT` 存在且合法，则覆盖配置端口（Knative 通过 `$PORT` 注入）。
   - 同时尊重已有的 `OJ_*` 系列环境变量惯例（`OJ_PLUGINS_DIR` 等）。

2. **探针端点**（新增保留路径，`serve/src/lib.rs` 路由表）
   - `/healthz`：存活探针，进程在即回 `200 OK`；仅在进程即将退出（drain 中）回 `503`。
   - `/readyz`：就绪探针，回报取决于：
     - `prewarm_boot` 完成（`app.rs:1307` 的启动预热）；
     - 必需后端（配置的 `db` / `kv` 若要求外部）连接可用。
     未完成前回 `503`，完成后回 `200`。复用 `bus_tx` / 后端 health 已有能力。
   - 保留既有 `/health`、`/plugins` 不变，避免破坏现有监控与插件端点契约。

3. **优雅退出**（已合规，核对即可）
   - Knative 缩容发送 `SIGTERM`，oj 已有 in-flight 请求 drain 后退出（`serve_cmd.rs:139-174`）。
   - 核对点：drain 期间 `/healthz` 应转 `503` 以加速 Knative 摘流；确认 shutdown 信号已覆盖。

4. **并发对齐**
   - Knative `containerConcurrency`（每实例并发上限）↔ oj `server.pool_size`（actor 线程数）。
   - 给出配置建议：concurrency=1 ⇒ pool_size=1；concurrency=N ⇒ pool_size=N。
   - 在样例 `serving.yaml` 中显式固定 `containerConcurrency`，避免与 `pool_size` 失配导致排队或 503。

5. **镜像与样例**
   - 现有 `Dockerfile` 已是 multi-stage + distroless（`cc-debian11`），体积小、拉起快，契合缩零冷启。
   - 新增 `docs/plans/` 附 `serving.yaml` 样例：Service/Revision、探针、`containerConcurrency`、
     `resources`（建议留出 V8 heap 余量）、`autoscaling.knative.dev/*` 注解（min-scale=0 默认即可）。

### 数据流（缩零 → 冷启 → 服务）
1. 无流量时 Knative 缩到 0 副本。
2. 新请求到达 → Knative 拉起 oj 容器，注入 `$PORT`。
3. oj 启动：`App::from_config` → `prewarm_boot`（预热一个 isolate、连后端）→ 绑定 `$PORT`。
4. `/readyz` 在 boot 完成后转 `200`，Knative 开始导流。
5. 流量下降 → Knative 发 `SIGTERM` → oj drain in-flight → 退出；`/healthz` 退出前转 `503` 加速摘流。

### 错误处理
- boot 失败（如 `ext_boot` 报错、`prewarm_boot` panic）：进程退出码非 0，Knative 标记 Revision 不健康并告警，不导流。
- `/readyz` 在后端连不上时持续 `503`，Knative 不会把流量打给未就绪实例。
- 沿用现有 `KillSwitch` 超时（handler 超时 → 408/500，runtime 丢弃不复用）。

### 配置示例（config.yaml 增量）
```yaml
server:
  pool_size: 1          # 与 Knative containerConcurrency 对齐
  # port 不写，由 $PORT 注入；若写则作为本地/非 K8s 回退
```

### 样例 serving.yaml（节选）
```yaml
apiVersion: serving.knative.dev/v1
kind: Service
metadata: { name: oj-app }
spec:
  template:
    spec:
      containerConcurrency: 1
      containers:
        - image: registry/oj:vX
          ports: [{ name: http1, containerPort: 8080 }]   # 实际由 $PORT 决定
          readinessProbe:
            httpGet: { path: /readyz, port: 8080 }
          livenessProbe:
            httpGet: { path: /healthz, port: 8080 }
          env: [{ name: OJ_PLUGINS_DIR, value: /bin/plugins }]
```

---

## 设计 B：事件标准化（CloudEvents 1.0）

### 目标
把 oj 的事件能力升级为 CNCF CloudEvents 1.0 标准：出站发标准事件（对接 Knative Eventing Broker），
入站可接收标准事件并路由到 JS 事件处理器（即 Knative Eventing Trigger 的投递目标）。

### CloudEvents 信封（HTTP 绑定）
- 必填属性：`id`、`source`、`specversion`(=1.0)、`type`。
- 可选：`datacontenttype`、`data`、`subject`、`time`。
- HTTP 传输绑定：`Ce-Id` / `Ce-Source` / `Ce-Specversion` / `Ce-Type` / `Ce-Subject` / `Ce-Time` 头 + `Content-Type` 描述 `data`。

### 组件与改动点

1. **CE 编解码层**（新增 `src/bridge/ce.rs` 或并入 `bus.rs`）
   - `CloudEvent` 结构：`{ id, source, specversion, type, datacontenttype, data, subject, time }`。
   - 出站封装：从 JS 调用补全 `specversion=1.0`、`id`(UUIDv4)、`time`(RFC3339)、`source`（取自 `bus.source` 配置，缺省用 `oj://<host>`）。
   - 入站解析：从 `Ce-*` 头 + body 还原 `CloudEvent`；校验 `specversion`、必填项，缺失则 400。

2. **出站：bus 插件改 header 绑定**
   - `oj-bus-kafka`：Kafka record **headers** 携带 CE 属性，value 为 `data`（按 `datacontenttype` 序列化，默认 JSON）。
   - `oj-bus-rabbitmq`：RabbitMQ message **headers** 携带 CE 属性，body 为 `data`。
   - 新增可选 **HTTP sink**：`bus` 配置一个 `sink.url`，直接 POST CloudEvents（带 `Ce-*` 头）到 Knative Broker / 任意 HTTP 事件接收方。
   - 兼容：现有 `bus.publish(topic, payload)` 语义保留，仅在外层包成 CloudEvent（`data=payload`），旧消费者经 `data` 字段无感兼容。

3. **入站：标准事件接收端点**（新增保留路径，如 `/v1/events` 或 `/events`，配置可调）
   - 解析 `Ce-*` 头 + body → `CloudEvent`。
   - 按 `type` / `source` / `subject` 查 **EventRouteTable**（复用 `RouteTable` 思路，独立订阅表）。
   - 命中后派发到对应 JS 事件处理器；未命中回 204（Knative Eventing 期望 2xx/404 语义，确认后定）。

4. **JS 契约（事件处理器）**
   - 新增处理器类型：模块导出 `onEvent(ce)`，入参 `ce` 为标准 CloudEvent 对象（`ce.type` / `ce.data` / `ce.source` / `ce.id` / `ce.subject` / `ce.time`）。
   - 或扩展现有 `bus.on(type, fn)`：`fn` 入参统一为 `CloudEvent`（含 `data`）。两者择一或并存，优先扩展 `bus.on` 以降低心智负担。
   - 事件处理器在 `RuntimePool` 内执行，复用 `ReqState` reset 机制（事件元数据放入 `ReqState`，供 `log`/`auth` 等 op 使用）。

### 数据流（入站：事件触发 oj 函数）
1. 外部事件源（Kafka / S3 通知 / 定时器 / 另一服务）产生 CloudEvent。
2. Knative Eventing Broker → Trigger（按 `type` 过滤）→ HTTP POST 到 oj Service 的 `/v1/events`。
3. oj 入站端点解析 CE → EventRouteTable 查 `type` → 取一个 isolate（checkout）→ reset ReqState（塞入 CE 元数据）→ 执行 `onEvent(ce)`。
4. handler 返回 → 捕获 `Capture` → 回 2xx；可选经 HTTP sink 再发新事件形成链路。

### 数据流（出站：oj 发事件）
1. JS `bus.publish(topic, data)` 或新 `bus.publishEvent(ce)`。
2. 桥接层补全 CE 属性 → 编码。
3. 按 `bus` 配置路由：Kafka/RabbitMQ（header 绑定）或 HTTP sink（Knative Broker）。

### 错误处理
- 入站 CE 校验失败（缺必填 / `specversion≠1.0`）→ 400，不影响其他事件。
- 出站 sink 失败（Broker 不可达）→ 按现有 `bus` 投递失败语义处理（记录 + 返回错误 / 异步重试，沿用插件既有行为）。
- 事件 handler 抛错 / 超时 → 复用 `KillSwitch` 与 runtime 丢弃策略；同一 isolate 不归还（保持隔离）。

### 配置示例（config.yaml 增量）
```yaml
bus:
  source: "oj://my-app"        # CloudEvent source 默认值
  sink:
    url: "http://broker-ingress/..."   # 可选 HTTP sink（Knative Broker）
  kafka: { ... }               # 现有配置保留，header 绑定为新增行为
  rabbitmq: { ... }
events:
  ingest_path: "/v1/events"    # 入站端点，默认 /v1/events
```

---

## 共享前提（A、B 共同的硬约束）

1. **Redis**：`kv` / auth 会话跨副本共享的前提（`docs/ops-manual.md:163-164`）。弹性伸缩前必须配置 `redis.default`。
2. **外部 broker**：`bus` 跨实例分发的前提（当前 `bus.publish` 仅进程内，`docs/ops-manual.md:259`）。
   接入 Kafka/RabbitMQ 插件或 HTTP sink 后事件才可跨副本。
3. **隔离约定**：明确文档化「handler 模块作用域应保持无副作用」；本期不引入每调用新 isolate（避免牺牲预热、抬高冷启）。
4. **可观测性**：就绪/存活探针 + 现有 `/health` 已覆盖基本健康检查；建议补充事件吞吐/失败指标（后续）。

---

## 测试策略（设计阶段，落地时细化）

- **A-端口/探针**：单测 `serve` 层——`$PORT` 覆盖、无 `PORT` 回退配置、`/healthz` 与 `/readyz` 在 boot 前后状态码切换。
- **A-优雅退出**：集成测试模拟 `SIGTERM`，断言 in-flight 请求完成、新请求被拒、`/healthz` 转 503。
- **B-CE 编解码**：单测 `ce.rs`——出站补全属性、入站解析与必填校验、错误用例（缺 `type`/`specversion`）。
- **B-插件绑定**：为 `oj-bus-kafka` / `oj-bus-rabbitmq` 加 CE header 单元测试（不连真实 broker，用内存桩）。
- **B-入站路由**：用 `tests/plugins/mini*` 思路加 mini 事件夹具，验证 `/v1/events` → `onEvent` 派发与未命中回退。
- **端到端（可选）**：本地 kind + Knative 跑最小 `serving.yaml` + 一个发事件示例，验证缩零冷启与事件触发。

---

## 风险与开放问题

1. **就绪门槛粒度**：`/readyz` 是否要等所有 `db` 连接池建好？建议仅等必需后端，可选后端失败不阻塞就绪（需确认）。
2. **EventRouteTable 与现有 RouteTable 关系**：新建独立表还是复用？倾向独立，避免污染 HTTP 路由。
3. **`bus.on` 扩展 vs `onEvent` 新类型**：需与 JS API 文档（`docs/devkit/api-manual.md`）协同，落地时定。
4. **Knative Eventing 回复语义**：入站 404 vs 204 的取舍需对照 Eventing 文档确认（影响 Trigger 重试行为）。
5. **冷启耗时**：缩零后首请求的 boot（含 `ext_boot`、连 Redis/broker）是否在 Knative 默认超时内？建议实测并文档化最小资源。

6. **桥接层 / 插件边界（已决策）**：CloudEvents 编解码层**不做成独立 cdylib 插件**。oj 插件模型按「后端轴」
   (`plugin_loader::AXES = [es, db, blob, bus, kv, auth, mq, mail, ldap]`) 划分，CE 是协议/格式层、无对应轴；
   且需被出站（bus 插件）与入站（serve HTTP 端点）两侧共用，放在核心 `src/bridge/ce.rs` 最自然。
   出站 header 绑定改在**现有** `oj-bus-kafka` / `oj-bus-rabbitmq` 插件内；入站 `/v1/events` 属 `serve/` 路由层。
   未来若需多事件格式可插拔，再引入 `EventEncoder` trait（YAGNI，本期不做）。

## 使用场景与案例代码

### 场景一：弹性 HTTP 服务（Knative Serving，缩零到零）

某用户查询 API 流量波峰波谷明显，希望无流量时缩到 0 副本、省钱，有流量时自动拉起。

- 部署：复用 `serving.yaml` 样例（默认 `min-scale=0`），`containerConcurrency: 1` ↔ `server.pool_size: 1`。
- Handler（保持模块作用域无副作用，isolate 跨请求复用才安全）：
```ts
// src/user/account/api.ts
export async function get(ctx) {
  const id = ctx.params.id;                       // 路径参数来自 ReqState，非模块全局
  const row = await db.table("user").where({ id }).one();  // db 连接池在 StableState 共享
  return json.ok(row);                            // json 全局即 {code,msg,data} 信封
}
```
- 前提：配置 `redis.default` 后，auth 会话 / `kv` 跨副本共享；否则多副本间状态丢失。

### 场景二：事件驱动订单处理（CloudEvents 入站）

上游订单服务向 Knative Broker 发 `order.created` CloudEvent，Trigger 按 `type` 过滤后 POST 到 oj 的 `/v1/events`，
oj 事件处理器落账并再发下游事件，形成事件链路。

- 事件处理器（二选一，落地时定；优先 `bus.on` 以降低心智负担）：
```ts
// src/order/onEvent/api.ts —— 方案 a：导出 onEvent(ce)
export async function onEvent(ce) {
  // ce 为标准 CloudEvent：{ id, source, specversion, type, data, subject, time }
  if (ce.type !== "order.created") return json.ok({ skipped: ce.type });
  const { orderId, amount } = ce.data;            // data 即原 payload
  await db.table("ledger").insert({ orderId, amount, status: "pending" });
  await bus.publish("ledger.updated", { orderId, status: "pending" }); // 出站自动包成 CloudEvent
  return json.ok({ handled: ce.id });
}
```
```ts
// 方案 b：扩展 bus.on，入参统一为 CloudEvent
import { bus } from "ext:bridge/bus";
bus.on("order.created", async (ce) => {
  const { orderId, amount } = ce.data;
  await db.table("ledger").insert({ orderId, amount, status: "pending" });
});
```
- Knative Eventing Trigger 投递到 oj：
```yaml
apiVersion: eventing.knative.dev/v1
kind: Trigger
metadata: { name: order-to-oj }
spec:
  broker: default
  filter:
    attributes: { type: order.created }
  subscriber:
    ref: { apiVersion: serving.knative.dev/v1, kind: Service, name: oj-app }
    uri: /v1/events                            # oj 入站端点，可配置
```

### 场景三：出站事件标准化（对接外部 CloudEvents 消费者）

现有 `bus.publish(topic, payload)` 语义不变，仅在外层自动包成 CloudEvent；
Kafka 插件的 record headers 携带 CE 属性（`Ce-Id` / `Ce-Type` / `Ce-Source` …），下游任意 CloudEvents 兼容消费者可读。

```yaml
# config.yaml 增量
bus:
  source: "oj://shop"                           # CloudEvent source 默认值
  kafka: { brokers: ["kafka:9092"], topic: orders }
```
```ts
// 业务里照旧调用，无需感知 CE
await bus.publish("order.created", { orderId: 1, amount: 99 });
// 实际发出：{ specversion: "1.0", id: <uuid>, source: "oj://shop", type: "order.created", data: {...} }
```
- 可选 HTTP sink：再加 `bus.sink.url` 指向 Knative Broker，oj 即可作为 Eventing 的事件源。

## 分阶段落地建议（仅规划，不实现）

1. **Phase 1（弹性）**：端口 `$PORT` + `/healthz` `/readyz` + `serving.yaml` 样例 + 文档（Redis/broker 前提）。
2. **Phase 2（事件信封）**：`ce.rs` 编解码 + `bus.publish` 出站包装 + 插件 header 绑定。
3. **Phase 3（事件入站）**：`/v1/events` 端点 + EventRouteTable + `bus.on`/ `onEvent` JS 契约 + HTTP sink。
4. **Phase 4（联调）**：本地 Knative 端到端验证弹性 + 事件链路。
