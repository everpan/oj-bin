# MQ 命名客户端 + 长任务池 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** JS 运行时获得 `Kafka(name)` / `RabbitMQ(name)` 命名客户端与 `tasks:` 长任务池（每文件一线程、框架管生命周期），与 bus 共底层，oj v0.1.6。

**Architecture:** 单 `mq` FFI 轴（JSON method dispatch，ABI 保持 7）；插件内一 Core 两轴面（bus/mq）；宿主双 `NamedRegistry` + 任务上下文门禁；任务文件按 ESM side-module 常驻驱动，监督器钉线程重启/优雅停机。spec：`docs/superpowers/specs/2026-09-07-mq-client-tasks-design.md`。

**Tech Stack:** Rust（deno_core/tokio/stabby）、cdylib 插件（rdkafka/lapin）、cargo test。

## Global Constraints

- **ABI_VERSION 保持 7**——加轴零破坏（`oj-plugin-ffi/src/lib.rs:41`）；本计划任何任务不得 bump。
- 禁止 debug 构建：所有验证命令用 `cargo test` / `cargo build --release`；发布验证 `cargo clippy -p <crate> --all-targets -- -D warnings`。
- `JsRuntime` 是 `!Send`：任务/actor/WS 一律专用线程 + current_thread runtime；失败的 runtime 丢弃不归还。
- **TDD**：每任务先写测试、跑红、最小实现、跑绿、提交。
- **BDD 风格测试**：Rust 测试名 `given_<前置>_when_<动作>_then_<结果>`，测试体以 `// Given / // When / // Then` 三段注释组织。
- **SOLID**：新代码落点遵守 spec §2 模块边界（插件=driver Core、宿主 mq.rs=注册+op 面、tasks.rs=监督），禁止跨层借用。
- 提交信息以 `unix@vip.qq.com ai` 结尾；每任务一提交。
- **执行协议（用户裁决 2026-09-07）**：分 6 阶段执行，每阶段完成 → 更新任务状态 → 阶段小结（改了什么/测试状态）；**总体完成后统一审查一次**（阶段间不插审查）。

## 阶段与任务总览

| 阶段 | 任务 | 交付 |
|---|---|---|
| P1 契约层 | T1 mq 轴 + 宿主探测 + mini 夹具；T2 长轮询退避 | FFI 可用、ABI 7 不变 |
| P2 插件层 | T3 KafkaCore+mq 轴；T4 RabbitCore+mq 轴 | 两插件双轴面 |
| P3 宿主绑定 | T5 mq.rs op 面；T6 JS 全局+类型；T7 装配注入 | Kafka/RabbitMQ 可调 |
| P4 任务驱动 | T8 ESM 常驻驱动 + kill_after | 任务文件可跑可停 |
| P5 任务池 | T9 config tasks: + 监督器 | 生产级生命周期 |
| P6 构建收尾 | T10 build 镜像；T11 文档示例；T12 v0.1.6+全量验证 | 发布就绪 |

---

### Task 1: mq 轴契约 + 宿主探测 + mini 夹具

**Files:**
- Create: `oj-plugin-ffi/src/mq.rs`
- Modify: `oj-plugin-ffi/src/lib.rs`（`pub mod mq;` + `pub use mq::MqVtable;`）
- Modify: `src/bridge/plugin_loader.rs:96-103`（Registrations 加槽）、`plugin_loader.rs:429`（AXES）、`probe_axes`（加臂）
- Modify: `tests/plugins/mini/src/lib.rs`（夹具加 mq 轴）
- Test: `tests/plugins/mini/` 内既有测试风格、`src/bridge/plugin_loader.rs` tests

**Interfaces:**
- Produces: `oj_plugin_ffi::MqVtable { connect, call, close }`；宿主 `Registrations.mq`；`probe_axes` 识别 `"mq"`；mini 插件导出 `oj_plugin_axis_mq`
- call 约定：`call(handle, method, payload_json) -> FfiFuture`，ok 值 = 结果 JSON；未实现 method → Err(`unsupported method: <m>`)

- [ ] **Step 1: 写失败测试（mini 夹具 mq 轴 + 宿主探测）**

```rust
// tests/plugins/mini 既有集成测试文件内追加（照现有用例风格，路径见文件内 mod tests）
#[test]
fn given_mini_with_mq_axis_when_probe_then_mq_slot_filled() {
    // Given: mini cdylib 导出 oj_plugin_axis_mq（本任务实现）
    // When: PluginLoader 装载 mini
    let p = load_mini_plugin();
    // Then: mq 槽位就位，ABI 仍 7
    assert!(p.registrations.mq.is_some());
    assert_eq!(p.descriptor.abi_version, 7);
}

#[test]
fn given_mini_without_mq_symbol_when_probe_then_mq_slot_none() {
    // Given: 一个只带 bus 轴的夹具（沿用现有「缺符号=不提供」用例的夹具形态）
    // When/Then: registrations.mq.is_none()，装载不报错
}
// spec §8 ffi 行（并发/取消）——mini mq call 实现为无锁固定 JSON，配合两端线程同时 call：
#[tokio::test(flavor = "multi_thread")]
async fn given_same_mq_handle_when_concurrent_calls_then_no_UB_and_both_answer() {
    // Given: mini mq handle=1；When: 两任务并发 call(handle,"echo",payload)
    // Then: 两个结果都 Ok 且互不串扰（vtable 线程安全契约冒烟）
}
#[tokio::test]
async fn given_cancelled_await_when_dropped_then_plugin_state_freed_not_taken() {
    // Given: 一个永不完成的假 mq call future 经 FfiGuard；When: await 被取消（drop）
    // Then: 插件侧 state 被 free 且未被 take（FfiGuard Drop 语义回归护栏，ffi.rs:161-172）
}
```

- [ ] **Step 2: 跑红**

Run: `cargo test -p only-js --lib probe` 与 `cargo test -p only-js --test *mini*`
Expected: FAIL（`MqVtable`/`registrations.mq` 不存在，编译错误即红）

- [ ] **Step 3: 最小实现**

```rust
// oj-plugin-ffi/src/mq.rs —— 镜像 src/bus.rs 的 EventBrokerVtable 形态
//! mq 轴 vtable：JSON method dispatch（spec §5）。方法面演进零 ABI 变更。
use crate::{FfiFuture, RString};

#[stabby::stabby]
#[repr(C)]
pub struct MqVtable {
    /// 建立实例（cfg JSON 含 kind）。ok 值 = `{"handle": u64}` JSON；cfg.kind 不符 → Err。
    pub connect: extern "C" fn(cfg: RString) -> FfiFuture,
    /// method 派发：kind/send/poll/commit/ack/nack/metadata/dlq（spec §5）。
    pub call: extern "C" fn(handle: u64, method: RString, payload: RString) -> FfiFuture,
    pub close: extern "C" fn(handle: u64),
}
```

```rust
// plugin_loader.rs 三处（每处一行）
pub const AXES: &[&str] = &["es", "db", "blob", "bus", "kv", "auth", "mq"];
// Registrations:
pub mq: Option<&'static oj_plugin_ffi::MqVtable>,
// probe_axes 的轴→槽 match：
"mq" => r.mq = lib.get::<*const c_void>(format!("oj_plugin_axis_{axis}")).ok()
    .and_then(|f| unsafe { f() }.cast::<oj_plugin_ffi::MqVtable>().as_ref()),
//（照既有轴臂的写法逐字对齐——读 probe_axes 现臂后镜像）
```

mini 夹具：`oj_plugin_entry!(init, bus => &BUS_VT, mq => &MQ_VT)`，`MQ_VT` 三函数返回固定 JSON（connect→`{"handle":1}`、call→echo payload、close→空），函数体用 `catch_future` 包裹（同 mini 现有轴）。

- [ ] **Step 4: 跑绿**

Run: `cargo test -p only-js probe` + `cargo test -p only-js mini` + `cargo clippy -p only-js -p oj-plugin-ffi --all-targets -- -D warnings`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(ffi): mq 轴契约（JSON method dispatch）+ 宿主探测 + mini 夹具，ABI 保持 7"
```

---

### Task 2: FFI 长轮询退避变体 `await_ffi_poll`

**Files:**
- Modify: `src/bridge/ffi.rs:141-172`（在 `await_ffi` 旁新增，不改原函数）
- Test: `src/bridge/ffi.rs` tests 模块

**Interfaces:**
- Produces: `pub(crate) async fn await_ffi_poll(fut: FfiFuture, backoff: std::time::Duration) -> Result<Vec<u8>, String>`——poll→0 时 `tokio::time::sleep(backoff)` 再试；取结果/free 语义与 `await_ffi` 逐字一致（FfiGuard Drop 只 free 不 take）

- [ ] **Step 1: 写失败测试**

```rust
#[tokio::test]
async fn given_slow_ffi_future_when_await_ffi_poll_then_no_busy_spin() {
    // Given: 一个 poll 前 50 次都返回 0（Pending）的假 FfiFuture，并计数 poll 次数
    let (fut, counter) = counting_pending_future(50, /*result*/ b"\"ok\"");
    let t0 = std::time::Instant::now();
    // When: await_ffi_poll(fut, 10ms)
    let out = await_ffi_poll(fut, std::time::Duration::from_millis(10)).await.unwrap();
    // Then: 结果正确，且耗时 ≥ 50×10ms×0.9（证明在退避睡眠而非 yield_now 空转）
    assert_eq!(String::from_utf8(out).unwrap(), "\"ok\"");
    assert!(t0.elapsed().as_millis() >= 450, "no backoff: {:?}", t0.elapsed());
}
```

- [ ] **Step 2: 跑红**

Run: `cargo test -p only-js await_ffi_poll`
Expected: FAIL（函数不存在）

- [ ] **Step 3: 最小实现**

```rust
/// mq 长轮询版 await_ffi：Pending 时 sleep 退避（评审 F4——yield_now 空转烧核）。
/// 其余语义（take→free→Guard Drop 只 free 不 take）与 await_ffi 完全一致。
pub(crate) async fn await_ffi_poll(fut: FfiFuture, backoff: std::time::Duration) -> Result<Vec<u8>, String> {
    let mut guard = FfiGuard(Some(fut));
    loop {
        let fut = guard.0.as_mut().expect("fut present until return");
        match (fut.poll)(fut.state) {
            0 => tokio::time::sleep(backoff).await,
            code => {
                let r = (fut.take)(fut.state);
                (fut.free)(fut.state);
                fut.state = std::ptr::null_mut();
                return match (code, std::result::Result::from(r)) {
                    (1, Ok(b)) => Ok(b.iter().copied().collect()),
                    (_, Ok(_)) => Err("ffi poll reported error but take succeeded".into()),
                    (_, Err(e)) => Err(e[..].to_string()),
                };
            }
        }
    }
}
```

- [ ] **Step 4: 跑绿**

Run: `cargo test -p only-js ffi` + clippy 同 Task 1
Expected: PASS（既有 await_ffi 测试不回归）

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(ffi): await_ffi_poll 长轮询退避变体——mq poll 不烧核（评审 F4）"
```

---

### Task 3: oj-bus-kafka → KafkaCore + mq 轴 + bus close 泄漏修复

**Files:**
- Modify: `plugins/oj-bus-kafka/src/lib.rs`（整体重构，见下）
- Test: `plugins/oj-bus-kafka/src/lib.rs` tests（既有 bus roundtrip/env-gated 用例全保留）

**Interfaces:**
- Produces: `KafkaCore { new(cfg: &CoreCfg), send(handle, SendReq), poll(handle, PollReq) -> Vec<MqMessage>, commit(handle, msg), metadata(handle), close(handle) }`；`static MQ_VTABLE: MqVtable`；入口宏改 `oj_plugin_entry!(init, bus => &VTABLE, mq => &MQ_VTABLE)`
- CoreCfg = bus 的 BrokerCfg 解析复用 + `kind` 自检（≠"kafka" → init Err）
- mq handle 与 bus handle **分开编号**（各自 `next_id` 计数器，spec §2）

实现要点（改造自现有代码，全部有落点）：
1. 现有 `KafkaBroker`（producer+consumer 构造，`lib.rs:62-100`）拆为 `KafkaCore`：producer 复用；新增可停的 consumer 会话（`poll` 用 `consumer.poll(Timeout)` 拉取 batch，`commit` 走 `consumer.store/commit`，**不再 enable.auto.commit**——mq 面 auto_commit=false，bus 面保持现状 true，两套 consumer 配置分列，spec §2 Core 边界）。
2. **bus close 泄漏修复**：`lib.rs:140-158` detach 的 stream 任务持有 consumer——改为 `CancellationToken`/`AtomicBool` 随 broker 存 map，`close` 先置停再 remove（写注释「评审 S1：修复 detach 消费任务泄漏」）；bus 既有 roundtrip 测试作回归护栏。
3. `MQ_VTABLE::call` = method 字符串 match → Core 方法；serde 请求/响应结构：

```rust
#[derive(serde::Deserialize)] struct SendReq { topic: String, #[serde(default)] key: Option<String>, #[serde(default)] partition: Option<i32>, #[serde(default)] headers: HashMap<String,String>, value: serde_json::Value }
#[derive(serde::Deserialize)] struct PollReq { topics: Vec<String>, #[serde(default="d_max")] max: usize, #[serde(default="d_timeout")] timeout_ms: u64 }
#[derive(serde::Serialize)] struct MqMessage { topic: String, partition: i32, offset: i64, #[serde(skip_serializing_if="Option::is_none")] key: Option<String>, value: serde_json::Value, #[serde(skip_serializing_if="HashMap::is_empty")] headers: HashMap<String,String>, ts: i64 }
// call 返回：send→{"sent":1}；poll→{"messages":[...]}；commit→{}；metadata→{"topics":[...]}
// 未知 method → Err(format!("unsupported method: {m}"))
```

- [ ] **Step 1: 写失败测试（BDD）**

```rust
#[test]
fn given_mq_cfg_with_wrong_kind_when_connect_then_err_names_kind() {
    // Given: cfg.kind = "rabbit"；When: MQ_VTABLE.connect；Then: Err 含 "kind"
}
#[tokio::test]
#[cfg_attr(not(feature = "it"), ignore)] // 沿用现有 OJ_TEST_KAFKA_BROKERS env-gated 风格
async fn given_real_kafka_when_send_poll_commit_then_message_roundtrips() {
    // Given: 真实 broker + topic；When: connect→send→poll→commit
    // Then: poll 收到同 value，commit 返回 ok JSON
}
#[test]
fn given_unknown_method_when_call_then_err_lists_unsupported() {
    // Given: handle=1；When: call(handle,"nope","{}")；Then: Err 含 "unsupported method: nope"
}
// 既有 bus roundtrip / requires_brokers 用例零改动必须仍绿（回归护栏）
```

- [ ] **Step 2: 跑红** → Run: `cargo test -p oj-bus-kafka`，Expected: 编译错（MQ_VTABLE 不存在）
- [ ] **Step 3: 实现**（按实现要点 1-3 重构；bus 面行为零变化）
- [ ] **Step 4: 跑绿** → Run: `cargo test -p oj-bus-kafka` + `cargo clippy -p oj-bus-kafka --all-targets -- -D warnings`；env-gated 用例在配 `OJ_TEST_KAFKA_BROKERS` 时本地跑一次
- [ ] **Step 5: Commit** → `feat(kafka-plugin): KafkaCore + mq 轴 + bus close 泄漏修复（评审 S1/F7）`

---

### Task 4: oj-bus-rabbitmq → RabbitCore + mq 轴

**Files / Interfaces / Steps:** 与 Task 3 完全同构（文件 `plugins/oj-bus-rabbitmq/src/lib.rs`；Core 方法 `publish(exchange,routingKey,headers,value) / poll(queues,max,timeoutMs)=循环 basic.get / ack / nack(requeue)`；kind 自检 "rabbit"；mq handle 独立编号；既有 bus ack-on-receive 行为保持、close 修复同款）。

- [ ] Step 1-5：同 Task 3 节奏（BDD：wrong-kind connect Err / env-gated `OJ_TEST_AMQP_URL` roundtrip / unsupported method / bus 回归零改动）
- Commit → `feat(rabbit-plugin): RabbitCore + mq 轴（评审 F7 同构）`

---

### Task 5: 宿主 mq.rs——注册表、op 面、InMemoryMq

**Files:**
- Create: `src/bridge/mq.rs`
- Modify: `src/bridge/mod.rs`（StableState/Extras 加字段 + op 注册 + `pub mod mq`）、`src/bridge/ws.rs`/`runtime.rs` 无需动（门禁走 StableState 标志）
- Test: `src/bridge/mq.rs` tests

**Interfaces:**
- Produces:
```rust
pub struct MqInstance {
    pub kind: &'static str,                                   // "kafka" | "rabbit"
    pub call: Arc<dyn Fn(&str, serde_json::Value) -> futures_util::future::BoxFuture<'static, BridgeResult<serde_json::Value>> + Send + Sync>,
}
// StableState 新增（blobs/bus 同款 Always-Arc 模式）：
pub kafkas: Arc<NamedRegistry<MqInstance>>,
pub rabbits: Arc<NamedRegistry<MqInstance>>,
pub tasks_flag: Option<Arc<std::sync::AtomicBool>>,           // 任务 Bridge 注 Some（评审采纳：实现为 StableState 字段）
// Extras 新增对应三个 Option 字段（None → 空 registry / flag=None）
// ops:
op_mq_has(name: String) -> bool        // 当前 kind 上下文？否——见下：JS 层分 kafkas/rabbits 两查
op_mq_call(kind: String, name: String, method: String, #[serde] payload: Value) -> Value
op_tasks_stopping() -> bool            // flag None/false → false
// 门禁：method ∈ {poll, commit, ack, nack} 且 tasks_flag 非 Some(true) → JsError("mq.poll requires a task context")
// poller 互斥：Registry 内每实例带 tokio::sync::Mutex<()> poller 锁（构造于 MqInstance::new），第二个 poll try_send 失败 → JsError("instance busy")
```

- [ ] **Step 1: 写失败测试（BDD，全部用 InMemoryMq——内存 Vec 队列实现同一 call 面）**

```rust
#[test]
fn given_named_mq_instance_when_call_send_then_backend_receives() { /* Given InMemoryMq 注入 kafkas "default"；When op_mq_call(kind=kafka,name=default,method=send)；Then InMemory 队列长度 1 */ }
#[test]
fn given_unknown_name_when_call_then_err() { /* When op_mq_call name=nope；Then Err（→JS undefined 语义由 op_mq_has 承担） */ }
#[tokio::test]
async fn given_http_context_when_poll_then_js_error_requires_task() { /* flag=None 时 poll → Err 含 "requires a task context"（评审 M2） */ }
#[tokio::test]
async fn given_task_context_when_two_concurrent_polls_then_second_busy() { /* flag=Some(true)；第一个 poll 持锁期间第二个 → Err 含 "busy"（评审 M2） */ }
#[test]
fn given_kafka_and_rabbit_same_name_when_lookup_then_both_resolve() { /* Kafka("x") 与 RabbitMQ("x") 并存（双 registry） */ }
```

- [ ] **Step 2: 跑红** → `cargo test -p only-js mq`，Expected: 编译错
- [ ] **Step 3: 实现**（mq.rs 全量：`InMemoryMq`（测试注入用，pub(crate)）+ `FfiMqInstance::new(vt, handle)`（call 经 `await_ffi_poll`，Task 2 产出）+ 三 op + StableState/Extras 字段 + extension ops 数组追加三行）
- [ ] **Step 4: 跑绿** → `cargo test -p only-js` + clippy
- [ ] **Step 5: Commit** → `feat(mq): 宿主 mq.rs——双 registry、op 面、任务门禁、InMemoryMq`

---

### Task 6: JS 全局 Kafka/RabbitMQ + tasks.stopping + 类型声明

**Files:**
- Modify: `src/bridge/bootstrap.js`（ASCII 红线：只写 ASCII 注释或中文放 Rust 侧）
- Modify: `sample/global.d.ts`、`bin/devkit/global.d.ts` 由 xtask 拾取（不动 bin）
- Test: `src/bridge/mod.rs` 既有 run_with 风格测试

**Interfaces:**
- Produces（bootstrap.js 追加，镜像 dbCache 模式）:
```js
// ----- Kafka / RabbitMQ: named mq clients (per-name JS cache guarantees identity) -----
const mqCache = new Map();
function mqClient(kind) {
  return function (name) {
    name = String(name);
    const key = kind + " " + name;
    if (!mqCache.has(key)) {
      if (!op_mq_has(kind, name)) return undefined;
      mqCache.set(key, {
        kind: () => op_mq_call(kind, name, "kind", null),
        send: (topic, o) => op_mq_call(kind, name, "send", { topic, ...Object(o) }),
        poll: (topics, o) => op_mq_call(kind, name, "poll", { topics, ...(o || {}) }),
        commit: (m) => op_mq_call(kind, name, "commit", m),
        metadata: () => op_mq_call(kind, name, "metadata", null),
      });
    }
    return mqCache.get(key);
  };
}
globalThis.Kafka = mqClient("kafka");
globalThis.RabbitMQ = mqClient("rabbit");
// rabbit 实例方法面（spec §4：publish = send 的 rabbit 命名，payload 带 exchange/routingKey）：
//   kind==="rabbit" 时给实例 Object.assign({
//     publish: (ex, rk, value, o) => op_mq_call("rabbit", name, "send", { exchange: ex, routingKey: rk, value, ...(o || {}) }),
//     ack: (m) => op_mq_call("rabbit", name, "ack", m),
//     nack: (m, requeue) => op_mq_call("rabbit", name, "nack", { ...m, requeue: !!requeue }),
//   }) 并删除 kafka 专属的 send/commit 暴露（各全局只暴露有意义的面）
globalThis.tasks = { stopping: () => op_tasks_stopping() };
```
- `global.d.ts` 补 `declare function Kafka(name: string): KafkaClient | undefined;` 等接口（含 MqMessage）

- [ ] **Step 1: 写失败测试**

```rust
#[tokio::test]
async fn given_kafka_instance_when_js_send_then_op_called_with_kind_topic() {
    // Given: InMemoryMq 注入；When: run_with(`Kafka("default").send("t",{a:1})`)
    // Then: 队列收到 {"topic":"t","value":{"a":1}}；两次 Kafka("default") 返回同一对象（缓存同一性）
}
#[tokio::test]
async fn given_unconfigured_name_when_js_kafka_then_undefined() { /* run_with(`Kafka("nope")`) → null */ }
#[tokio::test]
async fn given_http_bridge_when_js_tasks_stopping_then_false() { /* flag=None → false */ }
```

- [ ] **Step 2: 跑红** → [ ] **Step 3: 实现** → [ ] **Step 4: 跑绿**（`cargo test -p only-js` + `bootstrap.js` ASCII 检查 `LC_ALL=C grep -nP '[^\x00-\x7F]' src/bridge/bootstrap.js` 为空）→ [ ] **Step 5: Commit** → `feat(bridge): Kafka/RabbitMQ/tasks JS 全局 + 类型声明`

---

### Task 7: 装配注入——kafkas/rabbits kind 路由与 fail-fast

**Files:**
- Modify: `oj/src/server_cmd.rs`（`build_registries`/`assemble_plugins` 邻域：mq 实例装配函数）+ `oj/src/app.rs`（Extras 传递）
- Modify: `src/config.rs:303-320`（`kafkas: HashMap<String, serde_json::Value>`、`rabbits: HashMap<...>`，镜像 `plugins:` 写法；`#[serde(default)]`）
- Test: `oj/src/server_cmd.rs` tests（装配测试已有证书夹具模式 `test_support`）

**Interfaces:**
- Produces: `async fn build_mq_registries(cfg: &Config, loaded: &[LoadedPlugin]) -> Result<(Arc<NamedRegistry<MqInstance>>, Arc<NamedRegistry<MqInstance>>), String>`
- kind 路由（评审 F7）：在提供 mq 轴的插件里按 `descriptor.name` 推断——名字形如 `oj-bus-<kind>`（strip "oj-bus-"）；段声明的实例逐个 `connect(json({kind, ...cfg}))`；无对应 kind 插件 → `Err("kafkas: no mq plugin for kind 'kafka' (expected plugin 'oj-bus-kafka')")`；插件拒绝（kind 不符/参数缺失）→ fail-fast 带插件名

- [ ] **Step 1: 写失败测试（BDD）**

```rust
#[tokio::test]
async fn given_kafkas_section_with_kafka_plugin_when_assemble_then_instance_registered() { /* mini mq 夹具插件冒充 oj-bus-kafka？不行——kind 从名字推断，故测试注入 name 可配的加载结果：直接调 build_mq_registries 用手工 LoadedPlugin（descriptor.name="oj-bus-kafka", registrations.mq=Some(mini MQ_VT)）→ registry.contains("default") */ }
#[tokio::test]
async fn given_kafkas_section_without_matching_plugin_when_assemble_then_fail_fast_named() { /* Err 含 "no mq plugin for kind 'kafka'"（评审 F1/N2） */ }
#[tokio::test]
async fn given_rabbits_only_kafka_plugin_loaded_when_assemble_then_fail_fast() { /* 负例 */ }
#[test]
fn given_cfg_with_kafkas_rabbits_when_parse_then_hashmaps_filled() { /* config.rs 解析用例，镜像 plugins: 段既有测试 */ }
```

- [ ] **Step 2: 跑红** → [ ] **Step 3: 实现** → [ ] **Step 4: 跑绿**（`cargo test -p oj`）→ [ ] **Step 5: Commit** → `feat(assemble): kafkas/rabbits 装配注入——kind 路由 + fail-fast`

---

### Task 8: 任务驱动——ESM 常驻执行 + `Bridge::kill_after`

**Files:**
- Modify: `src/bridge/runtime.rs`（`boot_runtime` 邻域新增 `run_task`；KillSwitch 面扩展）
- Modify: `src/bridge/mod.rs`（`Bridge::run_task(source, task_flag) -> TaskExit` + `Bridge::kill_after(grace)`）
- Test: `src/bridge/runtime.rs` tests + `src/bridge/mod.rs` tests

**Interfaces:**
- Produces:
```rust
pub enum TaskExit { Stopped, Crashed(String) }
impl Bridge {
    /// ESM side-module 常驻驱动（boot_runtime 同款：load_side_es_module_from_code +
    /// 先 mod_evaluate 后 run_event_loop）。任务文件一律按 ESM 加载（绕过 looks_cjs，
    /// 评审 F3）。不武装 handler 超时。eval future Err → TaskExit::Crashed。
    pub async fn run_task(&self, source: String, flag: Arc<AtomicBool>) -> TaskExit;
    /// 停机面（评审 F5）：宽限期满 arm KillSwitch（terminate_execution），
    /// 调用方随后在本 owning 线程兜一轮 event loop 再 drop（SIGSEGV 纪律）。
    pub fn kill_after(&self, grace: std::time::Duration);
}
```

- [ ] **Step 1: 写失败测试（BDD）**

```rust
#[tokio::test(flavor = "current_thread")]
async fn given_tla_while_loop_task_when_flag_set_then_exits_stopped() {
    // Given: 任务源 = `export {}; while (!tasks.stopping) { await Kafka("default").poll(["t"],{timeoutMs:30}); }`
    //        （InMemoryMq 注入 + flag=Some(AtomicBool)）；spawn run_task
    // When: 100ms 后 flag.store(true)
    // Then: run_task 返回 TaskExit::Stopped（不再等超时——事件循环随 TLA 完成而结束）
}
#[tokio::test(flavor = "current_thread")]
async fn given_task_throws_when_run_then_exits_crashed_with_message() {
    // Given: `export {}; throw new Error("boom");`；Then: Crashed(msg 含 "boom")
}
#[tokio::test(flavor = "current_thread")]
async fn given_cjs_style_task_file_when_run_then_still_tla_capable() {
    // Given: 无 import/export 的任务源（含顶层 await）；Then: 正常跑（驱动强制 ESM，评审 F3）
}
```

- [ ] **Step 2: 跑红** → [ ] **Step 3: 实现**（run_task 镜像 `boot_runtime` 的 mod_evaluate/event_loop 顺序；`kill_after` = spawn 看门狗 sleep→arm，复用 KillSwitch，注意其 Drop join 语义 `runtime.rs:234-249`）→ [ ] **Step 4: 跑绿** → [ ] **Step 5: Commit** → `feat(runtime): 任务 ESM 常驻驱动 + Bridge::kill_after 停机面`

---

### Task 9: 长任务池——config tasks: + `oj/src/tasks.rs` 监督器

**Files:**
- Modify: `src/config.rs`（`tasks: Option<TasksCfg>`；`TasksCfg { dir: String("tasks"), max: usize(64), stop_grace_secs: u64(30) }`）
- Create: `oj/src/tasks.rs`
- Modify: `oj/src/server_cmd.rs`（serve 路径拉起监督器 + 停机序列编排）、`oj/src/main.rs`/server_cmd serve（信号）
- Test: `oj/src/tasks.rs` tests + `oj/src/server_cmd.rs` tests

**Interfaces:**
- Produces:
```rust
pub struct TaskHandle { pub name: String, pub join: std::thread::JoinHandle<()> }
pub fn scan_tasks(dir: &Path) -> Result<Vec<(String, PathBuf)>, String>; // task_*/ *_task 约定 + 同名冲突 Err + >max Err
pub struct TaskSupervisor { /* spawn_all / shutdown(grace) / 崩溃退避 1s→2s→…cap 60s */ }
pub async fn supervise(cfg: TasksCfg, root: PathBuf, make_bridge: impl Fn() -> Bridge + Send + Sync + 'static + Clone, flag: Arc<AtomicBool>) -> !; // 监督循环
// 停机序列（评审 F5/S2，server_cmd 编排）：
// ① tokio::signal::ctrl_c() + unix SIGTERM 二选一  ② flag 置位 ③ grace 内等任务自然退
// ④ 到期 kill_after → 任务线程兜 event loop → drop ⑤ mq handle Drop→close ⑥ axum with_graceful_shutdown 同信号触发 ⑦ 退出
```

- [ ] **Step 1: 写失败测试（BDD）**

```rust
#[test]
fn given_dir_with_task_and_lib_files_when_scan_then_only_task_files_listed() {
    // Given: tmpdir 含 task_orders.ts / audit_task.ts / helpers.ts / _shared/x.ts
    // When: scan_tasks；Then: 恰 2 项，name = orders / audit（评审用户裁决：命名约定）
}
#[test]
fn given_both_prefix_and_suffix_same_name_when_scan_then_err_conflict() { /* task_orders.ts + orders_task.ts → Err 含 "duplicate task" */ }
#[test]
fn given_more_tasks_than_max_when_scan_then_err_limit() { /* max=2 + 3 个任务 → Err 含 "max" */ }
#[tokio::test(flavor = "current_thread")]
async fn given_crashing_task_when_supervised_then_restarts_with_backoff() {
    // Given: 任务源 throw（InMemoryMq Bridge）；监督器注入 make_bridge
    // When: 首次 crash；Then: 日志 attempt=1，1s 后重启（用注入时钟或短退避起步常量测）
}
#[tokio::test(flavor = "current_thread")]
async fn given_running_tasks_when_shutdown_flag_then_all_join_within_grace() { /* 优雅停机：flag 置位 → join 全部 → Stopped 日志 */ }
```

- [ ] **Step 2: 跑红** → [ ] **Step 3: 实现**（`std::thread::Builder::name(format!("task-{name}"))` + current_thread runtime，同 routes.rs introspector 模式；信号处理用 `tokio::signal::ctrl_c` + `tokio::signal::unix::signal(SignalKind::terminate())`；每任务一行启动日志格式见 spec §6）→ [ ] **Step 4: 跑绿**（`cargo test -p oj`）→ [ ] **Step 5: Commit** → `feat(tasks): 长任务池监督器——命名扫描/退避重启/优雅停机/启动日志`

---

### Task 10: oj build 携带 tasks → `dist/tasks/`

**Files:**
- Modify: `oj/src/build_cmd.rs`（`run` 末尾追加 tasks 镜像步骤；复用 transpile）
- Test: `oj/src/build_cmd.rs` tests

**Interfaces:**
- Produces: build 时把 `<api_root>/<tasks.dir>/` 下**全部** `.ts` 转译镜像到 `<out>/tasks/`（保名，含非任务共享库）；无 tasks 目录 → 跳过；**不进 tgz/manifests**（评审 F2 裁决：tasks 非版本化模块）

- [ ] **Step 1: 写失败测试（BDD）**

```rust
#[test]
fn given_src_with_tasks_when_build_then_dist_tasks_transpiled() {
    // Given: src/tasks/task_demo.ts + lib.ts；When: oj build；Then: dist/tasks/task_demo.js 与 lib.js 存在且为转译产物
}
#[test]
fn given_src_without_tasks_when_build_then_no_tasks_dir() { /* dist/tasks 不存在 */ }
```

- [ ] **Step 2: 跑红** → [ ] **Step 3: 实现** → [ ] **Step 4: 跑绿**（`cargo test -p oj`）→ [ ] **Step 5: Commit** → `feat(build): tasks 目录转译镜像 → dist/tasks（评审 F2）`

---

### Task 11: 文档与 sample 示例

**Files:**
- Create: `sample/src/tasks/task_demo.ts`（InMemory/插件无关：`while (!tasks.stopping) { await new Promise(r => setTimeout(r, 1000)); log.info("demo tick"); }`——无 broker 依赖，纯生命周期演示）
- Modify: `docs/devkit/api-manual.md`（§6 全局总表加 Kafka/RabbitMQ/tasks + 新章「命名 MQ 客户端与长任务」，含顺序语义/超时互动一句）、`docs/user-manual.md`（config 参考 kafkas/rabbits/tasks）、`sample/MODULES.md`（任务池一节）
- Test: 文档任务无自动化测试；验证 = `oj build` + `oj server` 启动日志出现 `task: demo (task_demo.ts) → started`

- [ ] **Step 1: 写示例与文档（内容依 spec §4/§6 逐条落）**
- [ ] **Step 2: 手工验证** → `cargo run -p oj -- server -c sample/config.yaml --api-path sample/src`（Ctrl-C 观察停机序列日志）
- [ ] **Step 3: Commit** → `docs(mq): 命名 MQ 客户端与长任务池手册 + sample 任务示例`

---

### Task 12: v0.1.6 版本 + 全量验证（统一审查入口）

**Files:**
- Modify: `oj/Cargo.toml`（version = "0.1.6"）、`Cargo.lock`（随构建刷新）

- [ ] **Step 1: 版本 bump** → `oj/Cargo.toml` 0.1.6
- [ ] **Step 2: 全量门禁** →

```bash
cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
cargo build --workspace && cargo xtask build && cargo xtask plugin mini --check
```

Expected: 全绿（统一审查在此产物上进行——用户裁决：阶段间不插审查，完成后一次审）
- [ ] **Step 3: Commit** → `chore(release): oj 0.1.6——MQ 命名客户端 + 长任务池`
