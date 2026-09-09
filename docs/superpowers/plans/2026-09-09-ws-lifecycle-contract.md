# WS 生命周期钩子契约（v0.1.9）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** ws.ts 从「整文件每帧重跑」改为 `export default { connection, message, close, error }` 生命周期钩子——模块每连接加载一次、驻留会话按事件触发。

**Architecture:** 每连接 checkout 一个 JsRuntime 不还池（`WsSession`），连接期注入 side-module driver（import ws.ts → 装 `__ws_hooks`/`__ws_call` JS dispatcher）；每事件 `execute_script("__ws_call(ev)")` 复用现有捕获链（`ws_sends`/`ws_close`/信封）。零新增 op。server 的 Reader/Writer/bus-forwarder 三任务不动，只换 Processor 内核。

**Tech Stack:** Rust（deno_core 0.411 / axum / tokio current_thread）、JS ESM。

**Spec:** `docs/superpowers/specs/2026-09-09-ws-lifecycle-contract-design.md`

## Global Constraints

- 所有 cargo 命令一律 `--release`（禁 debug；测试/clippy 同样 `--release`）。
- commit message 结尾加 trailer：`unix@vip.qq.com ai`。
- 失败 runtime 不还池；`WsSession` 的 runtime **永不还池**（模块作用域含连接状态）。
- `JsRuntime` 是 `!Send`：`WsSession` 不跨线程（整条生命周期钉在 ws-js 线程的 current_thread runtime 上）。
- `bootstrap.js` 保持 7-bit ASCII（本计划不改它）。
- 不新增 op、不改 `ReqState` 字段、不改 ABI。
- 钩子语义（spec 决策）：返回值一律忽略（显式 `json.ok`/`ws.send` 回帧）；`error(e)` 收异常对象、处理后连接继续；帧超时 = 必断连（跳过 close）；至少导出一个钩子，全缺断连。

---

### Task 1: Bridge `WsSession`（ws_connect + fire）

**Files:**
- Modify: `src/bridge/mod.rs`（`WsOutcome` 定义附近，约 `mod.rs:825` 前后加 `WS_CONNECT_TIMEOUT` 与 `WsSession`；`impl Bridge` 内加 `ws_connect`）
- Test: `src/bridge/mod.rs`（`mod tests`，复用 `new_bridge` 同区域的 `with_dbs_and_loader` 写法，参考 blob 测试 `mod.rs:924`）

**Interfaces:**
- Consumes: `Bridge::checkout_armed(req, timeout, module)`（`mod.rs:575`，私有，同 crate 可用）、`Bridge::read_capture`/`Bridge::finalize_tx`（`mod.rs:612-642`，私有）、`module_loader::versioned_specifier(path)`、`ReqState::reset`、`WsOutcome { capture, sends, close }`（`mod.rs:829`）、`RunError::{Timeout, Core}`（`mod.rs:837`）、`Bridge.kill: Arc<runtime::KillSwitch>`（`mod.rs:426`）。
- Produces: `pub async fn Bridge::ws_connect(&self, ws_file: &std::path::Path) -> Result<WsSession, RunError>`；`impl WsSession { pub async fn fire(&mut self, ev: &str, req: RequestInfo, timeout: Duration) -> Result<WsOutcome, RunError> }`；`pub const WS_CONNECT_TIMEOUT: Duration`。Task 2 的 `frame_loop` 只用这三个名字。

- [ ] **Step 1: 写三个失败测试**（加到 `src/bridge/mod.rs` 的 `mod tests` 末尾附近）

```rust
/// WsSession 构造桥（带模块加载器，ws 文件放临时目录）。
fn ws_session_bridge(root: &std::path::Path) -> Bridge {
    Bridge::with_dbs_and_loader(
        HashMap::new(),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
        Some(Arc::new(LoaderShared {
            project_root: root.to_path_buf(),
            ts: true,
        })),
        Extras::default(),
    )
}

fn ws_temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("oj-wssess-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 生命周期：connection 恰好一次、message 每帧、close 收尾；模块作用域跨事件存活。
#[tokio::test(flavor = "current_thread")]
async fn ws_session_lifecycle_hooks() {
    let dir = ws_temp_dir("life");
    let ws_file = dir.join("ws.js");
    std::fs::write(
        &ws_file,
        r#"
let n = 0;
export default {
  connection() { json.ok({ hello: 1 }); },
  message() { n += 1; json.ok({ n }); },
  close() { json.ok({ bye: n }); },
};
"#,
    )
    .unwrap();
    let b = ws_session_bridge(&dir);
    let mut sess = b.ws_connect(&ws_file).await.unwrap();
    let o = sess
        .fire(
            "connection",
            RequestInfo::default(),
            std::time::Duration::from_secs(1),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&o.capture.body).unwrap();
    assert_eq!(v["data"]["hello"], 1);
    for expect in [1, 2] {
        let o = sess
            .fire(
                "message",
                RequestInfo::default(),
                std::time::Duration::from_secs(1),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&o.capture.body).unwrap();
        assert_eq!(v["data"]["n"], expect, "模块作用域跨事件存活");
    }
    let o = sess
        .fire(
            "close",
            RequestInfo::default(),
            std::time::Duration::from_secs(1),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&o.capture.body).unwrap();
    assert_eq!(v["data"]["bye"], 2);
}

/// 一刀切契约：无任何钩子导出 → ws_connect 报错（含指引文案）。
#[tokio::test(flavor = "current_thread")]
async fn ws_session_rejects_missing_hooks() {
    let dir = ws_temp_dir("nohooks");
    let ws_file = dir.join("ws.js");
    std::fs::write(&ws_file, "json.ok({});\n").unwrap();
    let b = ws_session_bridge(&dir);
    let e = b.ws_connect(&ws_file).await.unwrap_err();
    assert!(e.to_string().contains("connection/message/close/error"), "{e}");
}

/// message 抛异常 → error(e) 接住 → fire 返回 Ok，连接不断；error 可用 ws.send 带出信息。
#[tokio::test(flavor = "current_thread")]
async fn ws_session_error_hook_catches_frame_exception() {
    let dir = ws_temp_dir("errhook");
    let ws_file = dir.join("ws.js");
    std::fs::write(
        &ws_file,
        r#"
export default {
  message() { throw new Error("boom"); },
  error(e) { ws.send("err:" + e.message); },
};
"#,
    )
    .unwrap();
    let b = ws_session_bridge(&dir);
    let mut sess = b.ws_connect(&ws_file).await.unwrap();
    let o = sess
        .fire(
            "message",
            RequestInfo::default(),
            std::time::Duration::from_secs(1),
        )
        .await
        .unwrap();
    assert_eq!(o.sends, vec!["err:boom".to_string()]);
    assert!(o.capture.body.is_empty(), "返回值不自动包信封，异常也不产生信封");
}
```

- [ ] **Step 2: 跑测试确认失败**（`ws_connect` 不存在，编译失败即预期失败）

Run: `cargo test --release -p only-js ws_session -- --nocapture 2>&1 | tail -5`
Expected: compile error `no method named ws_connect` / `cannot find type WsSession`

- [ ] **Step 3: 实现 `WS_CONNECT_TIMEOUT` + `WsSession` + `ws_connect` + `fire`**

在 `src/bridge/mod.rs`，紧挨 `pub struct WsOutcome`（约 `mod.rs:825`）之前加常量；`WsOutcome` 之后加 `WsSession`；`impl Bridge` 里（`run_ws` 之后）加 `ws_connect`。全部代码：

```rust
/// WS 会话连接阶段超时（模块加载 + 首次求值；与内省同量级）。
pub const WS_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
```

```rust
/// WS 驻留会话：每连接独占的 runtime（模块已加载、钩子已装配）。
/// `!Send`：整条生命周期钉在 ws-js 线程的 current_thread runtime 上。
/// **永不还池**——模块作用域持有连接状态，复用会串连接；连接结束直接 drop
/// （fire 每次 drain event loop，正常路径 drop 前无悬挂句柄；超时毒化路径同
/// run_ws：直接丢弃）。
pub struct WsSession {
    rt: JsRuntime,
    kill: Arc<runtime::KillSwitch>,
}

impl WsSession {
    /// 触发一个生命周期事件（connection/message/close/error）：重置 per-event
    /// 状态、武装看门狗、执行 `__ws_call(ev)`、收捕获。超时 → runtime 已被
    /// terminate（毒化），返回 Timeout，调用方必须丢弃会话断连。
    pub async fn fire(
        &mut self,
        ev: &str,
        req: RequestInfo,
        timeout: std::time::Duration,
    ) -> Result<WsOutcome, RunError> {
        {
            let op_state = runtime::op_state(&self.rt);
            let mut g = op_state.borrow_mut();
            g.borrow_mut::<ReqState>().reset(req);
        }
        let handle = self.rt.v8_isolate().thread_safe_handle();
        self.kill.arm(handle, timeout);
        // ev 为内部常量，仍走 JSON 字面量杜绝意外注入（同 run_module 的 method_lit）。
        let ev_lit = serde_json::to_string(ev).unwrap_or_else(|_| "\"\"".into());
        let result = match self.rt.execute_script(
            "ws_event.js",
            format!("globalThis.__ws_call({ev_lit});"),
        ) {
            Ok(_) => self
                .rt
                .run_event_loop(deno_core::PollEventLoopOptions::default())
                .await,
            Err(e) => Err(e),
        };
        if self.kill.disarm() {
            // 同 run_ws 超时路径：不 drain，runtime 丢弃。
            return Err(RunError::Timeout);
        }
        result.map_err(RunError::Core)?;
        let (sends, close) = {
            let op_state = runtime::op_state(&self.rt);
            let g = op_state.borrow();
            let rs = g.borrow::<ReqState>();
            (rs.ws_sends.clone(), rs.ws_close)
        };
        let capture = Bridge::read_capture(&self.rt);
        Bridge::finalize_tx(&self.rt).await;
        Ok(WsOutcome {
            capture,
            sends,
            close,
        })
    }
}
```

`impl Bridge` 内（`run_ws` 方法之后）：

```rust
    /// WS 会话建立：借出 runtime（不还池，交给 `WsSession` 持有）、注入连接期
    /// driver——import ws.ts（default 导出钩子对象）→ 装 `__ws_hooks`/`__ws_call`
    /// → 经 json.ok 回报钩子清单。四钩子全缺 → Err（一刀切契约：失败要显式）。
    /// 钩子调用契约见 `WsSession::fire`；`__ws_call` 内建 try/catch → error() 兜底。
    pub async fn ws_connect(
        &self,
        ws_file: &std::path::Path,
    ) -> Result<WsSession, RunError> {
        let spec = module_loader::versioned_specifier(ws_file)
            .map_err(|e| RunError::Core(CoreError::from(std::io::Error::other(e))))?;
        let code = format!(
            "const h = (await import(\"{spec}\")).default ?? {{}};\n\
             const fns = {{}};\n\
             for (const k of [\"connection\", \"message\", \"close\", \"error\"])\n\
               fns[k] = typeof h[k] === \"function\" ? h[k] : null;\n\
             globalThis.__ws_hooks = fns;\n\
             globalThis.__ws_call = async (ev) => {{\n\
               const fn = globalThis.__ws_hooks[ev];\n\
               if (!fn) return;\n\
               try {{ await fn(); }} catch (e) {{\n\
                 const onErr = globalThis.__ws_hooks.error;\n\
                 if (onErr && ev !== \"error\") await onErr(e); else throw e;\n\
               }}\n\
             }};\n\
             json.ok({{ connection: !!fns.connection, message: !!fns.message,\n\
                        close: !!fns.close, error: !!fns.error }});\n"
        );
        let driver_spec = deno_core::ModuleSpecifier::parse("file:///oj/ws_connect.js")
            .map_err(|e| RunError::Core(CoreError::from(std::io::Error::other(e.to_string()))))?;
        let mut rt = self
            .checkout_armed(RequestInfo::default(), WS_CONNECT_TIMEOUT, None)
            .await?;
        // 顺序同 run_side_driver（0.410 签名）：mod_evaluate 先启动、驱动 event loop、
        // 再 await 求值 future 取 TLA/import 错误。
        let result: Result<(), CoreError> = async {
            let id = rt.load_side_es_module_from_code(&driver_spec, code).await?;
            let eval = rt.mod_evaluate(id);
            rt.run_event_loop(deno_core::PollEventLoopOptions::default())
                .await?;
            eval.await?;
            Ok(())
        }
        .await;
        if self.kill.disarm() {
            let _ = rt
                .run_event_loop(deno_core::PollEventLoopOptions::default())
                .await;
            return Err(RunError::Timeout); // runtime 丢弃（会话建立失败，无从交接）
        }
        if let Err(e) = result {
            let _ = rt
                .run_event_loop(deno_core::PollEventLoopOptions::default())
                .await;
            return Err(RunError::Core(e)); // 文件缺失/转译/求值错误原样带出
        }
        let cap = Self::read_capture(&rt);
        Self::finalize_tx(&rt).await;
        let v: serde_json::Value = serde_json::from_slice(&cap.body).unwrap_or_default();
        let d = &v["data"];
        let has_hook = ["connection", "message", "close", "error"]
            .iter()
            .any(|k| d[k].as_bool() == Some(true));
        if !has_hook {
            let _ = rt
                .run_event_loop(deno_core::PollEventLoopOptions::default())
                .await;
            return Err(RunError::Core(CoreError::from(std::io::Error::other(
                format!(
                    "ws handler '{}' must export at least one of \
                     connection/message/close/error (via default export)",
                    ws_file.display()
                ),
            ))));
        }
        Ok(WsSession {
            rt,
            kill: Arc::clone(&self.kill),
        })
    }
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --release -p only-js ws_session -- --nocapture`
Expected: 3 个测试 PASS（`ws_session_lifecycle_hooks` / `ws_session_rejects_missing_hooks` / `ws_session_error_hook_catches_frame_exception`）

- [ ] **Step 5: 全量回归 + lint**

Run: `cargo test --release -p only-js 2>&1 | tail -3 && cargo clippy --release --all-targets -- -D warnings 2>&1 | tail -3`
Expected: 全 PASS、clippy 无警告

- [ ] **Step 6: Commit**

```bash
git add src/bridge/mod.rs
git commit -m "feat(bridge): WsSession 驻留会话——ws_connect 装配钩子 + fire 按事件触发（v0.1.9 WS 契约 §1/2）

unix@vip.qq.com ai"
```

---

### Task 2: server frame_loop 切换 WsSession + 存量用例改写

**Files:**
- Modify: `server/src/ws.rs`（`frame_loop` 约 `ws.rs:141-227` 整体替换；模块头 import；6 个存量用例的 handler 内容改写为新契约）
- Test: 同文件（存量用例即回归钉，语义不变、只换 handler 写法与连接时序）

**Interfaces:**
- Consumes: Task 1 的 `Bridge::ws_connect` / `WsSession::fire` / `WS_CONNECT_TIMEOUT`（未直接用）；现有 `only_js::bridge::{Bridge, RequestInfo}`。
- Produces: 新的 `frame_loop` 时序（connection → message×N → close；超时断连跳过 close），Task 3 在此之上加新用例。

- [ ] **Step 1: 改写存量测试的 handler**（先改测试 = TDD 红灯在实现前成立）

`server/src/ws.rs` tests 内逐处替换：

1. `js_route_runs_handler_per_frame`（约 `ws.rs:431`）：
```rust
std::fs::write(&handler, r#"export default { message() { json.ok({ pong: true }); } };"#).unwrap();
```
2. `js_route_ws_send_order_and_close`（约 `ws.rs:472`）：
```rust
std::fs::write(
    &handler,
    r#"export default { message() { ws.send("side"); json.ok({ done: 1 }); ws.close(); } };"#,
)
.unwrap();
```
3. `ws_bus_subscribe_receives_http_publish`（约 `ws.rs:316`）：handler 改为 connection 钩子；**删除** `c.send_text("subscribe").await;`（订阅在连接建立时完成，欢迎信封不需要帧触发）：
```rust
std::fs::write(
    &handler,
    r#"export default { connection() { bus.subscribe("news"); json.ok({ sub: 1 }); } };"#,
)
.unwrap();
```
（后续 `let env = c.read_text().await;` 紧跟 `WsClient::connect` 之后即可。）
4. `mirror_routes_mount_directory_ws`（约 `ws.rs:530`）：news/ws.ts 改为：
```rust
(
    "news/ws.ts",
    "const n: number = 1;\nexport default {\n  connection() {\n    bus.subscribe(\"news\");\n    json.ok({ sub: n });\n  },\n};\n",
),
```
（**删除** `c.send_text("hi").await;`；`let env = c.read_text().await;` 紧跟 connect。注释「ws.ts 经统一转译管线」保留——TS 仍可用。）
5. `mirror_routes_root_ws`（约 `ws.rs:601`）：
```rust
let t = crate::tests::routes(&[(
    "ws.ts",
    "export default { message() { json.ok({ root: true }); } };\n",
)]);
```
（此用例保留发帧——message 钩子由帧驱动。）
6. `ws_frame_publish_broadcasts_to_subscribers`（约 `ws.rs:703`）：news/ws.ts 改为：
```rust
(
    "news/ws.ts",
    r#"export default {
  connection() {
    bus.subscribe("chat");
    json.ok({ joined: true });
  },
  message() {
    const frame = http.body;
    if (frame && frame.text) {
      bus.publish("chat", { from: frame.from ?? "anon", text: frame.text });
      json.ok({ sent: true });
    }
  },
};
"#,
),
```
流程调整：删掉 A、B 两处 `send_text("join")`（进房即 connection），改为 `WsClient::connect` 后直接 `read_text` 断言 `joined:true`；后续聊天帧、广播、自回声断言不变。用例 doc 注释里「帧代码经典 script 重跑安全」「声明放块作用域」两点已过时，改为：「模块作用域 = 连接状态；进房 = connection 钩子（无需 join 帧）」。

- [ ] **Step 2: 改写 `frame_loop`**（`ws.rs:141-227` 整体替换；模块头 import 更新）

文件头 import 行改为：
```rust
use only_js::bridge::{Bridge, RequestInfo, RunError, WsOutcome};
```

新增模块级写出辅助（`frame_loop` 上方；顺序契约：sends 先于信封，与原实现逐行等价）：
```rust
/// 事件结果写出：ws.send 集合先于信封帧（顺序契约，原 run_ws 消费端逐行等价）。
fn emit(resp_tx: &mpsc::Sender<String>, o: WsOutcome) {
    for s in o.sends {
        let _ = resp_tx.try_send(s); // 满则丢弃
    }
    if !o.capture.body.is_empty() {
        let _ = resp_tx.try_send(String::from_utf8_lossy(&o.capture.body).into_owned());
    }
}
```

`frame_loop` 整体替换为（Reader/Writer/forwarder 三任务原样保留，注释里的「每帧独立 ReqState」改为「每事件独立 ReqState」）：
```rust
async fn frame_loop(
    socket: WebSocket,
    handler_file: PathBuf,
    timeout: std::time::Duration,
    make: Arc<dyn Fn() -> Bridge + Send + Sync>,
) {
    let (msg_tx, mut msg_rx) = mpsc::channel::<Vec<u8>>(64);
    let (resp_tx, mut resp_rx) = mpsc::channel::<String>(64);
    // bus 会话端：订阅注册用的发送端注入每事件 RequestInfo；收到的广播帧转写回 socket。
    let (bus_tx, mut bus_rx) = mpsc::unbounded_channel::<String>();
    let (mut sink, mut stream) = socket.split();

    // Reader：读帧 → msgChan（满则背压至 TCP 层）。
    tokio::spawn(async move {
        while let Some(Ok(msg)) = stream.next().await {
            let bytes = match msg {
                Message::Text(t) => t.as_bytes().to_vec(),
                Message::Binary(b) => b.to_vec(),
                Message::Close(_) => break,
                _ => continue, // ping/pong 自动处理
            };
            if msg_tx.send(bytes).await.is_err() {
                break;
            }
        }
    });

    // Writer：respChan → 串行写回；通道排空（连接结束）后发 Close 帧干净关闭。
    let writer = tokio::spawn(async move {
        while let Some(text) = resp_rx.recv().await {
            if sink.send(Message::Text(text.into())).await.is_err() {
                return;
            }
        }
        let _ = sink.send(Message::Close(None)).await;
    });

    // Bus forwarder：订阅的广播帧 → 同一写出通道（与 ws.send 天然保序）。
    let forwarder = tokio::spawn({
        let resp_tx = resp_tx.clone();
        async move {
            while let Some(frame) = bus_rx.recv().await {
                let _ = resp_tx.try_send(frame);
            }
        }
    });

    // Processor：驻留会话按事件触发——connection（升级后恰好一次）→
    // message（每帧）→ close（收尾恰好一次）。会话 runtime 永不还池，
    // 连接结束随会话 drop（每连接独占 VM，与 Go 模式一致）。
    let bridge = make();
    let mut sess = match bridge.ws_connect(&handler_file).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("ws connect {}: {e}", handler_file.display());
            // 先发 Close 帧再丢弃，避免未读数据触发 TCP RST（客户端拿到干净关闭）。
            let _ = sink.send(Message::Close(None)).await;
            return;
        }
    };
    let mk_req = |body: Vec<u8>| RequestInfo {
        method: "WS".into(),
        body,
        bus_tx: Some(bus_tx.clone()),
        ..Default::default()
    };
    // 超时毒化后 runtime 已死：跳过后续一切 fire（含 close）。
    let mut alive = true;
    match sess
        .fire("connection", mk_req(Vec::new()), timeout)
        .await
    {
        Ok(o) => emit(&resp_tx, o),
        Err(RunError::Timeout) => alive = false,
        Err(e) => eprintln!("ws connection {}: {e}", handler_file.display()),
    }
    while alive {
        let Some(msg) = msg_rx.recv().await else {
            break; // 客户端 Close / socket 断
        };
        match sess.fire("message", mk_req(msg), timeout).await {
            Ok(o) => {
                let closing = o.close;
                emit(&resp_tx, o);
                if closing {
                    break;
                }
            }
            // 帧超时 = 必断连（V8 已 terminate，error() 救不了毒化的 VM）。
            Err(RunError::Timeout) => {
                eprintln!("ws frame timeout: {}", handler_file.display());
                alive = false;
                break;
            }
            // error() 缺失或自身抛：丢帧继续（与「error 后连接继续」决策一致）。
            Err(e) => eprintln!("ws frame error: {e}"),
        }
    }
    if alive {
        // close 恰好一次，尽力而为：钩子可 ws.send 离帧，失败只记日志。
        if let Err(e) = sess.fire("close", mk_req(Vec::new()), timeout).await {
            eprintln!("ws close {}: {e}", handler_file.display());
        } else if let Ok(o) = sess.fire("close", mk_req(Vec::new()), timeout).await {
            emit(&resp_tx, o);
        }
    }
    drop(resp_tx); // Writer 排空后自然退出
    forwarder.abort(); // 释放 bus_rx 与 resp_tx 克隆，Writer 才能排空退出
    let _ = writer.await;
}
```

**注意**：上面 close 段的写法有重复 fire 的笔误风险，落地时必须是**单次 fire**：
```rust
    if alive {
        // close 恰好一次，尽力而为：钩子可 ws.send 离帧，失败只记日志。
        match sess.fire("close", mk_req(Vec::new()), timeout).await {
            Ok(o) => emit(&resp_tx, o),
            Err(e) => eprintln!("ws close {}: {e}", handler_file.display()),
        }
    }
```
（原 `run_ws` 每帧路径与 `cached_transpile` 预检在此文件已无调用方——统一转译由模块加载器在 `ws_connect` 的 import 时完成并共享 mtime 缓存。）

- [ ] **Step 3: 跑 server 测试确认通过**

Run: `cargo test --release -p server 2>&1 | tail -5`
Expected: 全 PASS（存量 7 个用例语义不变：echo 信封、send 顺序+close、bus 订阅广播、目录镜像、根级 ws、缺失文件静默关、双连接聊天广播）

- [ ] **Step 4: 全 workspace 回归 + lint + fmt**

Run: `cargo fmt --check && cargo test --release --workspace 2>&1 | tail -3 && cargo clippy --release --all-targets -- -D warnings 2>&1 | tail -3`
Expected: 全 PASS；`cargo test -p only-js` 的旧 `run_ws` 单测不受影响（`run_ws` API 保留，HTTP `run_with_timeout` 路径仍走它）

- [ ] **Step 5: Commit**

```bash
git add server/src/ws.rs
git commit -m "feat(server): frame_loop 切驻留会话——connection/message/close 按事件触发，超时必断（v0.1.9 WS 契约 §3）

unix@vip.qq.com ai"
```

---

### Task 3: 新语义用例（error 继续 / close 钩子 / 无导出断连）

**Files:**
- Modify: `server/src/ws.rs`（tests 末尾新增 3 个用例；`WsClient` 加 `send_close`）

**Interfaces:**
- Consumes: Task 2 的 `frame_loop` 时序；`WsClient`（`ws.rs:237`）。
- Produces: 无（纯测试）。

- [ ] **Step 1: `WsClient` 加 `send_close`**（`impl WsClient` 内，`send_text` 之后）

```rust
/// 客户端主动断连：FIN+close, MASK|len=0, 4 字节 mask（空 payload）。
async fn send_close(&mut self) {
    let mask = [0x37u8, 0xfa, 0x21, 0x3d];
    let frame = [0x88u8, 0x80, mask[0], mask[1], mask[2], mask[3]];
    self.0.write_all(&frame).await.unwrap();
}
```

- [ ] **Step 2: 写三个失败测试**（加到 `ws_frame_publish_broadcasts_to_subscribers` 之后）

```rust
/// error() 兜底后连接继续：message 抛异常 → error(e) 经 ws.send 带出 → 下一帧仍正常处理。
#[tokio::test]
async fn js_route_error_hook_keeps_connection_alive() {
    let t = crate::tests::routes(&[]);
    let handler = t.0.join("ws.js");
    std::fs::write(
        &handler,
        r#"export default {
  message() {
    if (http.body.bad) throw new Error("boom");
    json.ok({ ok: 1 });
  },
  error(e) { ws.send("err:" + e.message); },
};"#,
    )
    .unwrap();
    let addr = spawn(
        app(
            "/v1/api",
            t.0.clone(),
            true,
            crate::tests::build_table(&t.0, true, "/v1/api"),
            crate::tests::make_actor(t.0.clone(), true),
            None,
            None,
            crate::Pipeline::default(),
            Arc::new(std::sync::RwLock::new(crate::CertificateStatus::Valid)),
            Arc::new(std::sync::RwLock::new(None)),
            Arc::default(),
        )
        .merge(js_route(
            "/ws/err",
            handler,
            std::time::Duration::from_secs(1),
            make_bridge,
        )),
    )
    .await;
    let mut c = WsClient::connect(addr, "/ws/err").await;
    c.send_text(r#"{"bad":true}"#).await;
    assert_eq!(c.read_text().await, "err:boom"); // 异常帧：error 兜底，无信封
    c.send_text(r#"{"bad":false}"#).await;
    let resp = c.read_text().await; // 连接继续：下一帧正常回信封
    assert!(resp.contains("\"ok\":1"), "{resp}");
}

/// close 钩子恰好一次：客户端发 Close 帧 → close() 触发、ws.send 离帧先于 Close 写出。
#[tokio::test]
async fn js_route_close_hook_fires_on_client_disconnect() {
    let t = crate::tests::routes(&[]);
    let handler = t.0.join("ws.js");
    std::fs::write(&handler, r#"export default { close() { ws.send("bye"); } };"#).unwrap();
    let addr = spawn(
        app(
            "/v1/api",
            t.0.clone(),
            true,
            crate::tests::build_table(&t.0, true, "/v1/api"),
            crate::tests::make_actor(t.0.clone(), true),
            None,
            None,
            crate::Pipeline::default(),
            Arc::new(std::sync::RwLock::new(crate::CertificateStatus::Valid)),
            Arc::new(std::sync::RwLock::new(None)),
            Arc::default(),
        )
        .merge(js_route(
            "/ws/bye",
            handler,
            std::time::Duration::from_secs(1),
            make_bridge,
        )),
    )
    .await;
    let mut c = WsClient::connect(addr, "/ws/bye").await;
    c.send_close().await;
    assert_eq!(c.read_text().await, "bye"); // close 钩子的离帧
    // 随后 Close 帧或 EOF（Writer 收尾），不 panic。
    let mut buf = [0u8; 8];
    let n = c.0.read(&mut buf).await.unwrap_or(0);
    assert!(n == 0 || buf[0] == 0x88, "expected close, got {n} bytes");
}

/// 一刀切契约：无任何钩子导出 → 连接建立即断（Close 帧 / EOF），不静默空转。
#[tokio::test]
async fn js_route_no_hooks_disconnects() {
    let t = crate::tests::routes(&[]);
    let handler = t.0.join("ws.js");
    std::fs::write(&handler, r#"json.ok({});"#).unwrap(); // 无 default 导出
    let addr = spawn(
        app(
            "/v1/api",
            t.0.clone(),
            true,
            crate::tests::build_table(&t.0, true, "/v1/api"),
            crate::tests::make_actor(t.0.clone(), true),
            None,
            None,
            crate::Pipeline::default(),
            Arc::new(std::sync::RwLock::new(crate::CertificateStatus::Valid)),
            Arc::new(std::sync::RwLock::new(None)),
            Arc::default(),
        )
        .merge(js_route(
            "/ws/nohooks",
            handler,
            std::time::Duration::from_secs(1),
            make_bridge,
        )),
    )
    .await;
    let mut c = WsClient::connect(addr, "/ws/nohooks").await;
    c.send_text("any").await;
    // ws_connect 失败 → 服务端发 Close 帧后关连接；传输层终止类错误同样算干净断连
    // （Windows RST 语义，断言同 js_route_missing_handler_closes_quietly）。
    let mut buf = [0u8; 64];
    let res = c.0.read(&mut buf).await;
    let clean = match &res {
        Ok(0) => true,
        Ok(_n) => buf[0] == 0x88,
        Err(e) => matches!(
            e.kind(),
            std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::UnexpectedEof
        ),
    };
    assert!(clean, "expected close or reset, got {res:?}");
}
```

（三个用例的 `app(...)` 拼装与相邻存量用例逐字相同，只换路由路径与 handler 文件。）

- [ ] **Step 3: 跑测试确认通过**

Run: `cargo test --release -p server js_route_error_hook js_route_close_hook js_route_no_hooks -- --nocapture`
Expected: 3 个测试 PASS

- [ ] **Step 4: fmt + clippy**

Run: `cargo fmt && cargo clippy --release --all-targets -- -D warnings 2>&1 | tail -3`
Expected: 无警告

- [ ] **Step 5: Commit**

```bash
git add server/src/ws.rs
git commit -m "test(server): WS 契约新语义钉——error 后继续、close 恰好一次、无导出断连（v0.1.9 WS 契约）

unix@vip.qq.com ai"
```

---

### Task 4: sample 迁移 + dist 重建

**Files:**
- Modify: `sample/src/news/ws.ts`（全文替换）
- Modify: `sample/src/news/chat/ws.ts`（全文替换）
- Modify: `sample/dist/`（`oj build` 重新生成，不手工编辑）

**Interfaces:**
- Consumes: 无（纯 JS 迁移）。
- Produces: 与 Task 2/3 测试用例同构的生产示例。

- [ ] **Step 1: 改写 `sample/src/news/ws.ts`**（全文替换；语义 = 原「首帧订阅+欢迎」平移到 connection 钩子）

```ts
// WS 生命周期契约（目录镜像路由 /v1/api/news/ws）：
// export default { connection, message, close, error }——connection 在连接建立后
// 恰好触发一次（bus.subscribe 在此，订阅每连接一次，不再每帧重复）；
// message 每帧触发，帧内容经 http.body 读取（JSON 文本帧自动 parse）。
// 模块作用域即连接状态，跨帧存活；跨连接共享走 kv / bus。TS 语法可用（统一转译管线）。
export default {
  connection() {
    bus.subscribe("news");
    json.ok({ subscribed: true });
  },
};
```

- [ ] **Step 2: 改写 `sample/src/news/chat/ws.ts`**（全文替换；聊天语义不变，进房从「join 帧」变为 connection 钩子）

```ts
// WS 聊天室（目录镜像路由 /v1/api/news/chat/ws）。
// 生命周期契约：connection = 进房（订阅 "chat"，每连接一次，无需 join 帧）；
// message = 每帧——发 {"from":"neo","text":"hi"} 即广播给所有订阅连接
// （含本连接——Bus fan-out 不排除自己）。
// 模块作用域即连接状态：不再是「每帧重跑整个文件」，顶层 const 可安全使用。
// 语义详解见 docs/websocket.md §2「帧内发布」。
export default {
  connection() {
    bus.subscribe("chat");
    json.ok({ joined: true });
  },
  message() {
    const frame = http.body;
    if (frame && frame.text) {
      bus.publish("chat", { from: frame.from ?? "anon", text: frame.text });
      json.ok({ sent: true });
    }
  },
};
```

- [ ] **Step 3: 重建 dist**

Run: `cargo run -p oj -- build -d sample/src -o sample/dist 2>&1 | tail -5 && git status --short sample/dist | head`
Expected: 构建成功；`sample/dist` 下 news/news-*/ws.js 与 chat 模块的 ws.js 更新（版本号目录随 manifest 版本变化，属正常）

- [ ] **Step 4: 冒烟验证（dev 模式起服务，用例语义已由 Task 2/3 覆盖，此处只验装配不炸）**

Run: `timeout 15 cargo run -p oj -- server -c sample/config.yaml --api-path sample/src 2>&1 | head -20 || true`
Expected: 服务正常监听（无 WS 装配报错/panic）；15s 后被 timeout 杀掉属预期

- [ ] **Step 5: Commit**

```bash
git add sample/src/news/ws.ts sample/src/news/chat/ws.ts sample/dist
git commit -m "feat(sample): news 与 chat 的 ws.ts 迁移生命周期钩子契约 + dist 重建（v0.1.9 WS 契约）

unix@vip.qq.com ai"
```

---

### Task 5: 文档 + CHANGELIST + v0.1.9 版本发布点

**Files:**
- Modify: `docs/devkit/api-manual.md`（§ws.ts 小节，约 `:365-395`）
- Modify: `docs/websocket.md`（§1 心智模型、§2 帧内发布的「每帧重跑」表述）
- Modify: `docs/dev-guide.md`（约 `:178` 一行表述）
- Modify: `CHANGELIST.md`（新增 v0.1.9 段）
- Modify: `oj/Cargo.toml`（`version = "0.1.8"` → `"0.1.9"`，`oj/Cargo.toml:3`）

**Interfaces:**
- Consumes: Task 1-4 的最终行为（真源是代码与测试）。
- Produces: v0.1.9 发布点提交（CHANGELIST 约定：`oj/Cargo.toml` version 递增提交即版本分界）。

- [ ] **Step 1: 改写 `docs/devkit/api-manual.md` §ws.ts**

替换「### ws.ts（WebSocket 帧循环）」整小节正文（保留标题与 websocket.md 交叉引用），新正文：

```markdown
目录内放 `ws.ts`（dev）/ `ws.js`（release，约定同 `api.ts`）即产生一条 WebSocket 路由
`GET {base}/{...path}/ws`：`src/news/ws.ts` → `/v1/api/news/ws`；根级 `ws.ts` → `/v1/api/ws`。
同目录 `ws.ts` 与 `ws.js` 并存时 `.ts` 优先。

**契约（v0.1.9 起）**：default 导出生命周期钩子对象——模块每连接加载一次，钩子按事件触发：

```ts
export default {
  connection() { /* 连接建立后恰好一次：bus.subscribe 在此 */ },
  message()    { /* 每帧一次：http.body 读帧（JSON 自动 parse） */ },
  error(e)     { /* 任一钩子抛异常时兜底，之后连接继续 */ },
  close()      { /* 收尾恰好一次：客户端断 / socket 断 / ws.close() 三来源统一 */ },
};
```

- 四钩子全部可选，但**至少导出一个**（全缺 → 连接建立即断）。
- 钩子内用既有全局（`json`/`http`/`ws`/`bus`/…），与 HTTP handler 一致；**返回值一律忽略**，
  回帧必须显式 `json.ok` / `ws.send`。
- 模块作用域即连接状态（跨帧存活）；跨连接共享走 kv / bus。
- 帧超时 = 必断连（钩子收不到该事件）；`error(e)` 是唯一带参钩子（e 为异常对象）。

运行案例（摘自 `sample/src/news/ws.ts`）：

```ts
export default {
  connection() {
    bus.subscribe("news");
    json.ok({ subscribed: true });
  },
};
```

注意：release 下 root=dist，WS URL 含模块版本段（如 `…/news-0.1.0/ws`）——v0.2 已知限制
（第 13 章）。bus 的发布/订阅方向约定见第 6 章 bus 小节。
```

同时把该文件「### 帧内发布」小节（约 `:383`）里「每帧重跑无害」注释与 chat 案例代码块替换为新契约写法（与 Task 4 Step 2 的 `chat/ws.ts` 一致，注释改为「connection = 进房，订阅每连接一次」）。

- [ ] **Step 2: 改 `docs/websocket.md`**

- §1 标题「一个文件 = 一条 WS 路由 = 每帧执行一次」→「一个文件 = 一条 WS 路由 = 生命周期钩子」；正文「整个文件（顶层代码每帧重跑一遍）」的执行单元表述改为：执行单元 = `default` 导出的钩子（`connection` 一次 / `message` 每帧 / `close` 一次 / `error` 兜底），对照 api.ts 的 `default[method]`。
- §2 中「为什么不会重复订阅」问答更新为：订阅挪进 `connection()`，每连接恰好一次（`Bus::subscribe` 按发送端通道去重 `same_channel` 的机制说明保留，作为幂等保证）。
- chat 案例代码块同步为 Task 4 Step 2 版本。

- [ ] **Step 3: 改 `docs/dev-guide.md:178`**

「连接升级后**客户端每个文本帧执行一次本文件**」→「连接升级后按**生命周期钩子**执行：`export default { connection, message, close, error }`（connection 一次、message 每帧、close 收尾，详见 devkit/api-manual.md §ws.ts）」。

- [ ] **Step 4: `CHANGELIST.md` 新增 v0.1.9 段**（插在 `## v0.1.8` 之前）

```markdown
## v0.1.9（2026-09-09）

**特性（breaking）**
- `ws.ts` 契约改为生命周期钩子：`export default { connection, message, close, error }`——
  模块每连接加载一次、按事件触发，模块作用域即连接状态（原「整文件每帧重跑」写法废除）。
  订阅挪进 `connection()`（每连接一次）；`error(e)` 兜底帧异常、连接继续；`close()` 收尾
  恰好一次；帧超时必断连；至少导出一个钩子，全缺断连。返回值一律忽略，回帧显式
  `json.ok` / `ws.send`。

**实现**
- bridge 新增 `WsSession` 驻留会话（`ws_connect`/`fire`）：每连接独占 runtime、永不还池；
  JS dispatcher 装配钩子，零新增 op。server frame_loop 三任务（Reader/Writer/bus）不变。
```

- [ ] **Step 5: bump `oj/Cargo.toml` 版本**

`oj/Cargo.toml:3` 改为 `version = "0.1.9"`。

- [ ] **Step 6: 验证**

Run: `cargo build --release --workspace 2>&1 | tail -2 && cargo test --release -p oj 2>&1 | tail -3`
Expected: 构建成功、oj e2e 全 PASS（e2e 只复制 demo + wsclient 模块，不受 sample 迁移影响）

- [ ] **Step 7: Commit（docs 提交）**

```bash
git add docs/devkit/api-manual.md docs/websocket.md docs/dev-guide.md
git commit -m "docs: WS 生命周期钩子契约手册化——api-manual §ws.ts、websocket.md 心智模型、dev-guide（v0.1.9）

unix@vip.qq.com ai"
```

- [ ] **Step 8: Commit（v0.1.9 发布点）**

```bash
git add CHANGELIST.md oj/Cargo.toml
git commit -m "chore(release): v0.1.9——WS 生命周期钩子契约

unix@vip.qq.com ai"
```

---

## Self-Review 记录

- **Spec 覆盖**：§1 契约语义 → Task 1（dispatcher/校验）+ Task 2（时序）+ Task 3（语义钉）；§2 Bridge → Task 1；§3 frame_loop → Task 2；§4 破坏性清单 1-3 → Task 2/3/4，清单 4（文档）→ Task 5；超时必断 → Task 2 `alive` 标志（无独立测试——KillSwitch 跨线程 terminate 在既有 run_ws 路径已覆盖，WS 侧复用同一开关，不另造超时用例）。
- **占位符**：无 TBD/TODO；Task 2 Step 2 内标注了 close 段的正确写法（单次 fire），落地者以「注意」段为准。
- **类型一致性**：`ws_connect(&Path)` / `fire(&mut self, &str, RequestInfo, Duration)` / `WsOutcome { capture, sends, close }` / `emit(&mpsc::Sender<String>, WsOutcome)` 在 Task 1/2 间一致；`RunError`/`WsOutcome` 均为 `only_js::bridge` 已导出类型。
