# WebSocket 客户端任务（v0.1.7）实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 运行时获得标准 WHATWG `WebSocket` 全局（deno_websocket 扩展），并以长任务案例演示从 WS 服务端取数；作为 v0.1.7 特性提交。

**Architecture:** 升级 deno_core 0.410→0.411，注册 deno_web/deno_webidl/deno_fetch/deno_net/deno_websocket 五扩展（deno_fetch 剥 ops 避 `op_fetch` 重名），`PermissionsContainer::allow_all` 经 bridge_ext state 注入，bootstrap.js 挂载全局。重连语义 = 任务崩溃监督（复用 `oj/src/tasks.rs` 指数退避），不手写 reconnect。

**Tech Stack:** deno_core 0.411、deno_websocket 0.263、deno_web 0.289、deno_webidl 0.258、deno_fetch 0.282、deno_net 0.250、deno_permissions 0.117、sys_traits 0.1.28、tokio-tungstenite 0.30（已是依赖，测试回环用）。

**实证基础:** /tmp/ws-probe 已按本计划 Task 2 的注册配方编译并运行成功：`typeof WebSocket = function`、连接被拒正确触发 `onerror`/`onclose`。0.411 API 差异已核：`extension!` 只生成 `init()`；`Extension.ops` 是 pub 字段可清空；`run_event_loop` 已是 `PollEventLoopOptions` 形态；`handle_scope()` 已删除、`resolve_value` 已弃用。

## Global Constraints

- 所有构建一律 `--release`（`.cargo/config.toml` 已别名，禁止 debug 构建用于仓库脚本）；**含验证命令**：`cargo test --release`、`cargo clippy --release --all-targets -- -D warnings`（dev 产物吃磁盘，已清禁）
- `[profile.release] panic = "unwind"` 不得覆盖（插件 `catch_unwind` 依赖）
- `src/bridge/bootstrap.js` 必须保持 7-bit ASCII
- `JsRuntime` 是 `!Send`：池与测试一律 `current_thread` runtime
- 失败的 runtime 丢弃、不归还池（本次改动不触碰该路径）
- 版本目标：`oj/Cargo.toml` → `0.1.7`（根 crate `only-js` 版本独立，不动）
- v0.1.8 已立项：deno_fetch 全量替换自研 fetch + wss 根证书注入。本计划**不做**这两件事，`esm_only` helper 与 `deno_net::init(None, None)` 的 None 均为有意剪裁（文档注明）
- 提交信息以 `unix@vip.qq.com ai` 结尾

---

### Task 1: deno_core 0.410 → 0.411 升级（独立提交，可 bisect）

**Files:**
- Modify: `Cargo.toml`（deno_core 版本行）
- Modify: `Cargo.lock`（cargo 自动）
- 可能 Modify（按编译错误）: `src/bridge/runtime.rs`、`src/bridge/mod.rs`、`oj/src/test_cmd.rs`、`src/bridge/mq.rs` 等 30 余处 `handle_scope()`/`resolve_value` 调用点

**Interfaces:**
- Consumes: 现有 bridge 全部代码
- Produces: deno_core 0.411 上的绿色 workspace（`cargo test --workspace` 全过）；Task 2 依赖 0.411 的 `init()` 单一签名与 pub `Extension.ops`

- [ ] **Step 1: 升级版本并重锁**

```bash
sed -i '' 's/^deno_core = "0.410"$/deno_core = "0.411"/' Cargo.toml
cargo update -p deno_core
```

预期：lock 中 deno_core → 0.411.0、deno_v8 → 0.3.0（0.411 default features 已含 `v8`，无需显式 feature）。

- [ ] **Step 2: 编译并收集破坏面**

```bash
cargo check --workspace 2>&1 | tail -60
```

已知 0.411 变化（按此预估，以编译器输出为准逐个修）：
1. `rt.handle_scope()` 已删除 → 改用 `rt.main_realm().execute_script(...)` 或 `deno_core::scope!` 宏形态获取 scope（`resolve_value` 弃用 → `rt.resolve(global).await`）
2. `extension!` 仍生成 `init()`（bridge_ext::init 调用形态不变）
3. 若 V8 静态库重新下载：设置 `V8_FROM_SOURCE=0` 强制预编译包，切勿源码编译

- [ ] **Step 3: 修复至绿**

逐个修复编译错误后运行完整验证：

```bash
cargo fmt
cargo clippy --all-targets -D warnings
cargo test --workspace 2>&1 | tail -20
```

预期：全绿（e2e 含 SIGTERM 用例，耗时正常）。

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "build(deps): deno_core 0.410→0.411（适配 init()/scope API 变化）

unix@vip.qq.com ai"
```

---

### Task 2: WebSocket 客户端全局（依赖 + 注册 + 挂载 + 测试）

**Files:**
- Modify: `Cargo.toml`（[dependencies] 新增 7 项）
- Modify: `src/bridge/mod.rs`（新增 `ws_client_extensions()` + bridge_ext state 闭包加 PermissionsContainer）
- Modify: `src/bridge/runtime.rs:70-76`（spawn 的 extensions vec）
- Modify: `src/bridge/bootstrap.js`（import + globalThis 挂载，ASCII）
- Modify: `oj/src/test_cmd.rs:113`（extensions vec 同步拼装，否则 bootstrap 的 ext: import 在 `oj test` 路径下模块缺失 → 求值失败）
- Test: `src/bridge/ws.rs`（文件末尾新增 `mod ws_client_tests`）

**Interfaces:**
- Consumes: Task 1 的 0.411 `init()` 签名、pub `Extension.ops`
- Produces: `bridge::ws_client_extensions() -> Vec<deno_core::Extension>`（runtime 与 test_cmd 共用）；全局 `new WebSocket(url)`（WHATWG，任务与 HTTP handler 均可用，无 task 门禁）

- [ ] **Step 1: 写失败测试（TDD 红）**

在 `src/bridge/ws.rs` 文件末尾追加：

```rust
mod ws_client_tests {
    use crate::bridge::{Bridge, Extras, InMemoryKV, RequestInfo, SchemaRegistry};
    use std::collections::HashMap;
    use std::sync::Arc;

    fn bridge() -> Bridge {
        Bridge::with_dbs_and_loader(
            HashMap::new(),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            None,
            Extras::default(),
        )
    }

    async fn run(b: &Bridge, src: &str) -> Result<String, String> {
        b.run_with(src, RequestInfo::default())
            .await
            .map(|c| String::from_utf8_lossy(&c.body).into_owned())
            .map_err(|e| e.to_string())
    }

    /// 全局已由 deno_websocket 扩展声明（bootstrap.js 挂载）。
    #[tokio::test(flavor = "current_thread")]
    async fn given_runtime_when_probed_then_websocket_global_declared() {
        let out = run(&bridge(), "json.ok(typeof WebSocket === \"function\");")
            .await
            .unwrap();
        assert!(out.contains("true"), "{out}");
    }

    /// 回环：in-process tokio-tungstenite 服务端推一帧，WHATWG 客户端收到。
    /// 钉住 op 链路 + allow_all 权限 + 事件循环驱动。
    #[tokio::test(flavor = "current_thread")]
    async fn given_local_ws_server_when_connected_then_frame_received() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            use tokio_tungstenite::tungstenite::Message;
            ws.send(Message::text("hello-from-oj")).await.unwrap();
            let _ = ws.read().await; // 悬住连接，客户端读完帧后 close
        });
        let b = bridge();
        let out = run(
            &b,
            &format!(
                "(async () => {{\n                const ws = new WebSocket(\"ws://{addr}/\");\n                const msg = await new Promise((ok, err) => {{\n                    ws.onmessage = (e) => ok(e.data);\n                    ws.onerror = () => err(new Error(\"ws error\"));\n                    ws.onclose = () => err(new Error(\"ws closed\"));\n                }});\n                ws.close();\n                json.ok(String(msg));\n            }})()"
            ),
        )
        .await
        .unwrap();
        assert!(out.contains("hello-from-oj"), "{out}");
    }
}
```

- [ ] **Step 2: 跑测试确认红**

```bash
cargo test -p only-js ws_client_tests 2>&1 | tail -15
```

预期：第一条 FAIL（`typeof WebSocket` 是 `"undefined"` → body `false`）；第二条 FAIL（`new WebSocket` 非函数抛错）。编译需通过。

- [ ] **Step 3: 加依赖**

`Cargo.toml` `[dependencies]` 段在 `deno_core` 行后追加：

```toml
# WS 客户端扩展面（v0.1.7）：deno_websocket 的 JS 经 core.loadExtScript 依赖
# deno_web/deno_webidl/deno_fetch/deno_net 的扩展 JS，须一并注册（见 bridge::ws_client_extensions）。
deno_web = "0.289"
deno_webidl = "0.258"
deno_fetch = "0.282"
deno_net = "0.250"
deno_websocket = "0.263"
deno_permissions = "0.117"
# libc feature 门控 RealSys 的 EnvHomeDir（RuntimePermissionDescriptorParser 需要）
sys_traits = { version = "0.1.28", features = ["libc", "real"] }
```

注意：deno_websocket 0.263 对 deno_error 有 `=0.7.1` exact pin，resolver 会把 lock 中 deno_error 升到 0.7.1（与主仓 `"0.7"` req 兼容，无需改代码）。

- [ ] **Step 4: 注册扩展（src/bridge/mod.rs）**

在 `bridge_ext` 的 `extension!` 宏定义之后新增（模块级，`pub(crate)`）：

```rust
/// WS 客户端扩展面（v0.1.7，spec 2026-09-08）：deno_websocket 提供 WHATWG
/// `WebSocket` 全局；其 JS 经 core.loadExtScript 依赖 deno_web / deno_webidl /
/// deno_fetch / deno_net 的扩展 JS，故五个扩展一并注册、顺序即依赖序。
/// v0.1.8 计划全量替换自研 fetch，届时 esm_only 剥离一并移除。
pub(crate) fn ws_client_extensions() -> Vec<deno_core::Extension> {
    /// deno_fetch 的 JS 是 deno_websocket 的运行时依赖，但其 `op_fetch` 与本仓
    /// 自研 fetch op 同名——注册面保留 JS、剥掉 ops。
    fn esm_only(mut ext: deno_core::Extension) -> deno_core::Extension {
        ext.ops = std::borrow::Cow::Borrowed(&[]);
        ext
    }
    vec![
        deno_webidl::deno_webidl::init(),
        deno_web::deno_web::init(
            Arc::new(deno_web::BlobStore::default()),
            None,                                  // maybe_location
            false,                                 // enable_css_parser_features
            deno_web::InMemoryBroadcastChannel::default(),
        ),
        esm_only(deno_fetch::deno_fetch::init(deno_fetch::Options::default())),
        deno_net::deno_net::init(None, None), // v0.1.8: 第一参传 RootCertStoreProvider 启用 wss
        deno_websocket::deno_websocket::init(),
    ]
}
```

再改 bridge_ext 的 state 闭包（`src/bridge/mod.rs` 现有 `state = |state, options| {...}`，约 261 行）追加一行，使 HTTP 池 / 任务 / `oj test` 三路径统一拿到 allow_all 权限容器（deno_websocket 的 `op_ws_check_permission_and_cancel_handle` 要求 OpState 里有 `PermissionsContainer`）：

```rust
    state = |state, options| {
        state.put(options.stable.clone());
        state.put(ReqState::default());
        state.put(deno_permissions::PermissionsContainer::allow_all(Arc::new(
            deno_permissions::RuntimePermissionDescriptorParser::new(sys_traits::impls::RealSys),
        )));
    },
```

- [ ] **Step 5: 接入 runtime.rs 与 test_cmd.rs**

`src/bridge/runtime.rs` `spawn()`（约 70 行）改为：

```rust
        JsRuntime::new(RuntimeOptions {
            extensions: ws_client_extensions()
                .into_iter()
                .chain(std::iter::once(bridge_ext::init(stable.clone())))
                .collect(),
            inspector: inspect,
            module_loader,
            ..Default::default()
        })
```

`oj/src/test_cmd.rs`（约 113 行）同样拼装（bootstrap.js 顶部将静态 import `ext:deno_websocket/...`，缺模块会让 `oj test` 的 boot 直接失败，必须同步）：

```rust
    let mut rt = JsRuntime::new(RuntimeOptions {
        extensions: only_js::bridge::ws_client_extensions()
            .into_iter()
            .chain(std::iter::once(bridge_ext::init(stable.clone())))
            .chain(std::iter::once(oj_test_ext::init()))
            .collect(),
        module_loader,
        ..Default::default()
    });
```

（`ws_client_extensions` 需在 `src/lib.rs` 可达路径上：`pub mod bridge` 已暴露 mod.rs，故 `pub(crate)` 即可被 oj crate 经 `only_js::bridge::` 使用？**否**——跨 crate 需 `pub`。定案：函数用 `pub`，注释标明内部装配用途。）

- [ ] **Step 6: bootstrap.js 挂载全局（7-bit ASCII）**

文件顶部既有 import 区（`ext:core/ops` 旁）加：

```js
import { WebSocket as ojWsClient } from "ext:deno_websocket/01_websocket.js";
```

在 `globalThis.ws = {...}`（约 187 行）之后加：

```js
// WHATWG 出站 WebSocket 客户端（deno_websocket）：任务与 handler 均可用；
// 无 MQ 式 task 门禁（连接无 offset/ack 消费会话语义）。
globalThis.WebSocket = ojWsClient;
```

- [ ] **Step 7: 跑测试确认绿**

```bash
cargo test -p only-js ws_client_tests 2>&1 | tail -8
cargo clippy --all-targets -D warnings 2>&1 | tail -5
```

预期：2 个用例 PASS（回环用例收到 `hello-from-oj`）；clippy 无告警。

- [ ] **Step 8: 全量回归**

```bash
cargo test --workspace 2>&1 | tail -15
```

预期：全绿。若 e2e SIGTERM 用例受新增扩展影响启动耗时，允许调大该用例自身超时常量（不动生产语义）。

- [ ] **Step 9: Commit**

```bash
git add -A
git commit -m "feat(bridge): WHATWG WebSocket 客户端全局（deno_websocket 五扩展注册）

- ws_client_extensions(): webidl→web→fetch(esm_only 剥 ops)→net→websocket
- bridge_ext state 注入 PermissionsContainer::allow_all（三路径统一）
- bootstrap.js 挂载 globalThis.WebSocket；oj test 同步拼装
- 测试：全局存在性烟测 + tokio-tungstenite 回环收帧

unix@vip.qq.com ai"
```

---

### Task 3: 任务案例 + 文档 + v0.1.7

**Files:**
- Create: `sample/src/tasks/task_wsclient.ts`
- Modify: `docs/devkit/api-manual.md`（全局表 +1 行；`### ws` 小节后 +1 小节）
- Modify: `CLAUDE.md`（注入全局清单 +1 词）
- Modify: `oj/Cargo.toml`（version 0.1.6 → 0.1.7）
- Modify: `Cargo.lock`（cargo 自动）

**Interfaces:**
- Consumes: Task 2 的 `globalThis.WebSocket`、`tasks.stopping()`/`log`（既有）
- Produces: 可手跑的自包含 demo（任务连本机 `/v1/api/news/ws`，收 `bus.publish("news")` 广播）

- [ ] **Step 1: 写案例任务**

创建 `sample/src/tasks/task_wsclient.ts`：

```ts
export {};
// WS 客户端任务案例（v0.1.7）：连本机 WS 服务端 /v1/api/news/ws，订阅 "news" 主题，
// 循环收取 bus.publish("news", ...) 的广播帧。写法同 Kafka/RabbitMQ 消费任务
// （docs/mq-tasks.md），但重连不手写——断连即 reject → TLA throw → Crashed →
// 监督器指数退避重启（1s→2s→…cap 60s）→ 新实例重连。
//
//   cargo run -p oj -- server -c sample/config.yaml --api-path sample/src
//   curl -X POST http://localhost:9778/v1/api/news -d '{"text":"hi"}'
//   → 任务日志：ws frame {"text":"hi"}；Ctrl-C → stopped（Stopped 出口 ws.close()）
//
// 注意：wss:// 需宿主注入根证书（v0.1.7 未配，握手会失败）——见 api-manual「WebSocket」节。
const url = "ws://127.0.0.1:9778/v1/api/news/ws";

const frames: string[] = [];
let wake: (() => void) | null = null;
const ws = new WebSocket(url);
const opened = new Promise<void>((ok, err) => {
  ws.onopen = () => ok();
  ws.onerror = () => err(new Error("ws connect failed: " + url));
});
ws.onmessage = (e) => {
  frames.push(String(e.data));
  wake?.();
  wake = null;
};
ws.onclose = () => {
  frames.push(Promise.reject(new Error("ws closed")));
};

await opened;
ws.send("{}"); // 首帧：服务端 ws.ts 执行 bus.subscribe("news")
log.info("ws task connected", url);

while (!tasks.stopping()) {
  const frame = frames.length
    ? frames.shift()
    : await new Promise((ok) => (wake = ok));
  log.info("ws frame", frame);
}
ws.close(); // Stopped 出口：干净断连（服务端 log: ws closed）
```

（队列元素按 `string | Promise<never>` 混存是有意的：断连塞入 reject promise，让下一轮 `await` 抛出 → Crashed → 监督重启，教学点即此。）

- [ ] **Step 2: 手跑验证**

```bash
cargo run -p oj -- server -c sample/config.yaml --api-path sample/src &
sleep 3
curl -s -X POST http://localhost:9778/v1/api/news -d '{"text":"hi"}'
sleep 1
kill -TERM %1
```

预期：启动日志含 `task: wsclient (task_wsclient.ts) → started`；curl 后任务线程日志 `ws frame {"text":"hi"}`；SIGTERM 后 `task: wsclient → stopped`。
（注意：任务先于 axum 监听启动时首次连接会失败 → Crashed → 退避重启后连上，这正是崩溃监督重连语义在起作用，非缺陷。）

- [ ] **Step 3: 文档（api-manual.md）**

全局对象表（约 473 行 `ws.send / close` 行之后）加一行：

```markdown
| `new WebSocket(url)` | WHATWG 出站 WS 客户端（任务与 handler 均可用，见下「WebSocket —— 出站客户端」） |
```

`### ws —— WebSocket 帧控制` 小节（约 688 行表格之后）加新小节：

```markdown
### WebSocket —— 出站客户端（WHATWG，v0.1.7）

标准 WHATWG `WebSocket`（deno 官方实现）：`new WebSocket(url)`、`onopen/onmessage/onclose/onerror`、
`send/close`。任务文件与 HTTP handler 均可用，无 MQ 式 task 门禁（连接无 offset/ack
消费会话语义）。

任务里作消费端取数（`src/tasks/task_*.ts`，写法同 [docs/mq-tasks.md](../mq-tasks.md)）：

​```ts
const ws = new WebSocket("ws://host/feed");
await new Promise((ok, err) => { ws.onopen = ok; ws.onerror = err; });
ws.onmessage = (e) => log.info("frame", String(e.data));
​```

**重连 = 崩溃监督，不手写 reconnect**：`onerror`/`onclose` 后让任务抛错（reject 挂起的
await 即可）→ `Crashed` → 监督器指数退避重启（1s→2s→…cap 60s）→ 新实例重连。
停机在 `stop_grace_secs` 内 `ws.close()` 自然收场（`Stopped`）。

限制：v0.1.7 出站仅明文 `ws://`（wss 需宿主注入根证书，v0.1.8 与 deno_fetch 替换一并处理）。
`oj test` 运行时同样挂载该全局。
```

（写入时去掉示例代码块前多余的反斜杠转义——上面 `​```ts` 处为本文档嵌套示意，实际写普通三反引号代码块。）

- [ ] **Step 4: CLAUDE.md 全局清单**

`CLAUDE.md` 第 5-6 行注入全局清单里 `fetch` 后加 `WebSocket`：

```
（`json`、`db`、`http`、`kv`/`redis`、`blob`、`bus`、`es`、`fetch`、`WebSocket`、`log`、...
```

- [ ] **Step 5: 版本 0.1.7**

```bash
sed -i '' 's/^version = "0.1.6"$/version = "0.1.7"/' oj/Cargo.toml
cargo check -p oj -q   # 刷新 Cargo.lock
```

- [ ] **Step 6: 门禁 + Commit**

```bash
cargo fmt --check && cargo clippy --all-targets -D warnings
git add -A
git commit -m "feat(v0.1.7): WebSocket 客户端任务案例 + API 手册

- sample/src/tasks/task_wsclient.ts：连本机 news WS 订阅取数（崩溃监督=重连）
- api-manual：全局表 + 出站客户端小节（含重连语义与 wss 限制说明）
- oj 0.1.7

unix@vip.qq.com ai"
```

---

## Self-Review 记录

- **Spec 覆盖**：Cargo 依赖/runtime 注册/案例/文档/测试 五交付物 ↔ Task 2、Task 3；deno_core 升级（用户追加裁决）↔ Task 1；v0.1.7 版本落 oj crate ✓；wss 剪裁已显式记 v0.1.8（spec「非目标」未列，此为计划期新增裁决，已在 Global Constraints 与文档双处注明）
- **占位符扫描**：无 TBD；Task 1 Step 2 的修复面为「以编译器输出为准」的封闭清单（handle_scope/resolve_value 两类，探针已核实）
- **类型一致性**：`ws_client_extensions()` 名称与两处调用点一致；`esm_only` 签名与 probe 实证一致；测试 helper 与 mq.rs 既有模式同形
