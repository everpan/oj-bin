# 开发手册（Developer Guide）

`only-js` —— 基于 Rust + `deno_core` 的嵌入式 JS 后端运行时。JS handler 通过一组注入的
全局对象（`json` / `db` / `DB` / `http` / `redis` / `log` / `fetch` / `finish`）编写业务逻辑，
Rust 侧捕获统一信封响应，供上层 HTTP server 写回。

本手册面向**日常开发**：环境、构建、写 handler、加 op、接数据库、测试、调试、发布。架构与设计
取舍见 [rust-core-runtime-revised.md](rust-core-runtime-revised.md)，模块对照与 JS API 见
[bridge.md](bridge.md)，性能数据见 [benchmarks.md](benchmarks.md)，Go 对标见 [comparison.md](comparison.md)。

---

## 1. 环境与依赖

- Rust toolchain（edition 2024）。`deno_core 0.409` 依赖 `rusty_v8`，首次编译需下载预编译 V8
  静态库（网络受限时设 `V8_FROM_SOURCE=0` 让它走预编译包；不要从源码编译 V8）。
- 关键依赖（`Cargo.toml`）：
  - `deno_core 0.409` / `deno_error 0.7`：JS 运行时与 op 错误类型。
  - `sqlx 0.8`（`any` + mysql/postgres/sqlite + runtime-tokio）：驱动无关数据访问。
  - `sea-query 0.32`：安全 SQL 构造器。
  - `reqwest 0.12`（rustls-tls）、`tokio-tungstenite 0.24`、`notify 0.6`、`tracing`。

### 常用命令

```bash
cargo build                  # 编译（debug）
cargo build --release        # 发布构建
cargo test                   # 单元测试（7 项）
cargo test -- --nocapture    # 看 tracing 输出
cargo build --benches        # 编译基准（不跑）
cargo bench                  # 跑 criterion 基准
cargo run                    # 跑 src/main.rs 演示（含 query 构造器 + 参数化 + inspector 开关）
cargo clippy --all-targets   # lint（CI 应 -D warnings）
cargo fmt --check            # 格式门禁
```

---

## 2. 项目结构

```
src/
  main.rs                    # 演示入口（构造 Bridge、跑 JS、读 Capture）
  lib.rs                     # crate 根
  bridge/
    mod.rs                   # Bridge / StableState / ReqState / Capture / extension 注册
    bootstrap.js             # JS 全局对象装配 + 查询构造器（ESM，必须 7-bit ASCII）
    envelope.rs              # {code,msg,data} 统一信封
    json.rs / db.rs / query.rs / registry.rs
    accessor_sqlx.rs         # 真实 DB 接入（sqlx Any）
    runtime.rs               # RuntimePool（复用 JsRuntime）
    loader.rs                # HandlerStore（加载 + 热重载）
    inspector.rs             # DevTools WS 桥
    fetch.rs / http.rs / kv.rs / log.rs
docs/                        # 设计文档（本册 + bridge.md / benchmarks.md / comparison.md / *-revised.md）
benches/bridge.rs            # criterion 基准
```

---

## 3. 写第一个 handler（JS）

handler 是一段 ESM 源码；它**必须调用一次 `json.ok` / `json.fail` / `finish`** 才能完成会话。
所有数据库/网络调用都是 `async`，用 `.then` 或 `await`（handler 顶层可直接 `await`，因为
由 `run_to_completion` 驱动 event loop 至 Promise 落定）。

```js
// GET /user/:id 的等价 handler
redis.set("last_query", "user")
  .then(() => db.table("user")
    .select(["id", "name"])
    .where({ field: "id", op: "eq", value: 1 })
    .limit(10).all())
  .then((rows) => {
    json.header("X-Handler", "user.get");
    json.ok({ users: rows, req: { method: http.method, query: http.query } });
  })
  .catch((e) => json.fail(500, String(e)));
```

### JS 全局对象速查

| 全局 | 用途 | 关键点 |
|---|---|---|
| `json.ok(data)` | 成功信封 `{code:0,msg:"ok",data}`，status 200 | 标记会话完成 |
| `json.fail(code, msg, data?)` | 失败信封；`code<=0` 映射 500 | 标记会话完成 |
| `json.header(name, value)` | 设置返回头（覆盖语义） | 空名忽略 |
| `db` / `DB(name)` | 数据访问；`db === DB("default")` | 未配置的名字返回 `undefined` |
| `db.query(sql, params?)` / `db.exec(sql, params?)` | 原始 SQL + 绑定参数 | **优先用参数，勿拼接** |
| `db.table(name).select(...).where(...).orderBy(...).limit(...).all()` | 安全构造器 | 标识符走白名单、值参数化 |
| `http.method/params/query/headers/body` | 只读请求上下文 | `body` 能解析 JSON 则为对象，否则字符串，空为 null |
| `redis.get/set` | M0 内存 KV（Promise） | 真实 Redis 待 server 层接入 |
| `log.debug/info/warn/error(msg, ...kv)` | 结构化日志（tracing） | 交替键值对 |
| `fetch(url, opts?)` | 浏览器兼容 Fetch（reqwest） | 不支持 AbortController |
| `finish()` | 标记会话完成但不写响应 | 少用 |

### 查询构造器（`db.table(...)`）

`db.table(name)` 返回构造器：`select(cols)`、`where(cond)`、`orderBy([{field,dir}])`、
`limit(n)`、`offset(n)`、`all()`。`all()` 返回 `Promise<Row[]>`。

- **条件 `cond`**：`{ field, op, value }`。`op` ∈ `eq/ne/gt/gte/lt/lte/in/like/isNull`。
  `in` 的 `value` 为数组；`isNull` 不需 `value`。
- **白名单**：`table` 与 `field` 必须先在 `SchemaRegistry` 声明，否则 `json.fail(400, "unknown table/column ...")`。
- **limit**：默认 100、硬上限 1000。
- **排序**：仅 `dir:"asc"|"desc"`，字段须 `is_sortable`（注册表默认所有列可排序）。

```js
db.table("order")
  .select(["id", "amount"])
  .where({ field: "user_id", op: "eq", value: 1 })
  .where({ field: "amount", op: "gte", value: 100 })
  .orderBy([{ field: "amount", dir: "desc" }])
  .limit(50).all()
  .then((rows) => json.ok({ rows }))
  .catch((e) => json.fail(500, String(e)));
```

### SQL 注入防护（必须读）

- **标识符**（表名/列名）**绝不**来自 JS 字符串拼接——只能来自 `SchemaRegistry` 白名单
  （经 `db.table(...)` 构造器）。
- **值**通过绑定参数传递：`db.query("... where id = $1", [id])` 或构造器的 `value`。
- `db.query` 的占位符风格随驱动而定：`$1`（Postgres） / `?`（MySQL、SQLite）。sea-query 构造器
  固定生成 Postgres 风格 `$N`；接入非 PG 驱动时需在 `accessor_sqlx.rs` 转换占位符。

---

## 4. 在 Rust 侧使用 Bridge

```rust
use std::sync::Arc;
use only_js::bridge::{
    Bridge, InMemoryAccessor, InMemoryKV, RequestInfo, SchemaRegistry,
};

let db = Arc::new(InMemoryAccessor::new());
db.seed([serde_json::json!({"id": 1, "name": "ever", "age": 18})]);

let registry = SchemaRegistry::new().table("user", Some("id"), &["id", "name", "age"]);
let inspect = std::env::var("MDM_INSPECT").is_ok();
let b = Bridge::with_opts(db, Arc::new(InMemoryKV::new()), registry, inspect);

// 开发期启用 inspector：起 WS 服务（chrome://inspect）。
if inspect {
    only_js::bridge::start_inspector(&b, "127.0.0.1:9229".parse().unwrap());
}

let cap = b.run_with(r#"
    db.table("user").select(["id","name"]).where({field:"age",op:"gte",value:18})
      .all().then((rows) => json.ok({ users: rows }))
      .catch((e) => json.fail(500, String(e)));
"#, RequestInfo { method: "GET".into(), ..Default::default() }).await?;

println!("status={} body={}", cap.status, String::from_utf8_lossy(&cap.body));
```

**状态模型（重要）**：

- `StableState`（`Arc`，跨请求共享）：`kv` / `dbs` / `client` / `registry`。构造后不可变。
- `ReqState`（每请求，存 `OpState`）：`req` / 响应捕获。每次 `run_with` 借出 runtime 时整体 `reset(req)`。
- 两者分离使 `JsRuntime` 可池化复用（`RuntimePool`）。**切勿**把跨请求可变的共享状态塞进 `ReqState`。
- `set_db_accessors` / `set_handlers` 必须在**首次 `run` 之前**调用（`StableState` 一旦被 runtime
  共享即不可变，`Arc::get_mut` 会 panic）。

---

## 5. 接入真实数据库（SqlxAccessor）

`SqlxAccessor` 已实现 `DataAccessor`，以 `sqlx::any::Pool<Any>` 驱动无关接入 MySQL/PostgreSQL/SQLite。

```rust
use only_js::bridge::SqlxAccessor;

let db = SqlxAccessor::arc("postgres://user:pass@localhost/mdm").await?;
// 或 sqlite
let db = SqlxAccessor::arc("sqlite:///tmp/mdm.db").await?;
let b = Bridge::new(Arc::new(db), Arc::new(InMemoryKV::new()));
```

- 占位符风格见上文 §3。若用 MySQL/SQLite，需在 `accessor_sqlx.rs` 的 `query_with_params` 中
  把 `$N` 转为 `?`（或先用 sea-query 的对应 `QueryBuilder` 生成驱动原生 SQL）。
- `SqlxAccessor::connect` / `from_pool` 支持接管既有连接池。

---

## 6. Handler 加载与热重载（HandlerStore）

handler 源码的加载策略由环境变量决定：

- **默认**（生产）：`HandlerStore::from_embedded(map)`——从编译期嵌入的 `HashMap<name, src>` 读取
  （建议用 `include_dir!` 在构建期把 `handlers/` 打进二进制，无运行期打包步骤）。
- **`MDM_HANDLER_DIR=/path/to/handlers`**（开发）：从目录读取 `.js`/`.ts` 文件（文件名去扩展名为名），
  并用 `notify` 递归监听变更，**热重载近乎免费**——per-request runtime 使下次请求重读文件即可，
  无需失效模块图或清理旧状态。

```rust
use only_js::bridge::HandlerStore;

// 开发：FS + 热重载
let b = /* ... */;
b.set_handlers(HandlerStore::from_dir(std::path::PathBuf::from("/path/handlers")));
let cap = b.run_named("user.get").await?;   // 按文件名（去扩展名）执行
```

---

## 7. DevTools 调试（inspector）

- 构造 `Bridge::with_opts(db, kv, registry, true)`（或设 `MDM_INSPECT` 环境变量），并调用
  `start_inspector(&b, "127.0.0.1:9229")` 起 WS 服务。
- 浏览器打开 `chrome://inspect` → 配置 `127.0.0.1:9229` → 即可断点/单步/看 console。
- 实现：`JsRuntimeInspector::create_local_session` + tungstenite WS（`inspector.rs`）。
  因 inspector 是 `!Send`，服务跑在 `tokio::task::spawn_local`（current_thread runtime）。
- **仅开发用**：生产构建应去掉 inspector 以缩减攻击面与体积。

---

## 8. 加一个新的 op（扩展 JS SDK）

扩展点在 `src/bridge/mod.rs` 的 `deno_core::extension!` 宏与 `bootstrap.js`。

1. **写 op**（如 `src/bridge/foo.rs`）：
   - 同步 op：`fn op_foo(state: &mut OpState, #[string] x: String)`。
   - 异步 op：`async fn op_foo(state: Rc<RefCell<OpState>>, ...) -> Result<T, JsErrorBox>`。
   - 读共享状态：`state.borrow().borrow::<Arc<StableState>>()`；
     写每请求状态：`state.borrow_mut::<ReqState>()`。
   - 错误用 `deno_error::JsErrorBox::generic(msg)`，会被抛给 JS 的 `catch`。
2. **注册**：在 `mod.rs` 的 `extension! { ops = [ ... foo::op_foo ] }` 加入，并在 `mod.rs` 加 `mod foo;`。
3. **装配 JS 侧**：在 `bootstrap.js` 顶部 `import { op_foo } from "ext:core/ops";`，
   并挂到某个全局对象。
4. **测试**（TDD）：在对应模块的 `#[cfg(test)]` 加 `#[tokio::test(flavor = "current_thread")]`，
   用 `Bridge::new(...).run(...)` 验证 JS 全链路。

> `bootstrap.js` 必须是 **7-bit ASCII**：deno_core 的 ESM 扩展要求，中文注释会触发
> "Extension code must be 7-bit ASCII" panic。注释统一用英文。

---

## 9. 测试约定（TDD）

- 单元测试就近放在各模块 `#[cfg(test)]`：`cargo test` 当前 7 项。
- handler 集成测试：遍历 `handlers/*.js` 或内联 JS 字符串，注入 fake `InMemoryAccessor` /
  `InMemoryKV`，断言 `Capture.body` 的 JSON。复用 `Bridge::new(...).run_with(...)`。
- **勿用 `deno test`**：缺 `json/db/http` 全局对象，须 mock 桥接。统一走 `cargo test`。
- 异步测试用 `tokio::test(flavor = "current_thread")`——与 `Bridge` 的 `JsRuntime` `!Send` 模型一致。

示例（来自 `mod.rs`）：

```rust
#[tokio::test(flavor = "current_thread")]
async fn db_params_and_build() {
    let db = Arc::new(InMemoryAccessor::new());
    db.seed([json!({"id": 1, "name": "ever"})]);
    let registry = SchemaRegistry::new().table("user", Some("id"), &["id", "name", "age"]);
    let b = Bridge::with_opts(db, Arc::new(InMemoryKV::new()), registry, false);
    let cap = b.run(r#"
        db.table("user").select(["id","name"]).where({field:"id",op:"eq",value:1})
          .all().then((rows) => json.ok({ rows })).catch((e) => json.fail(500, String(e)));
    "#).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0);
}
```

---

## 10. 已知约束与待办（上线前）

按路线图优先级排序（详情见 [rust-core-runtime-revised.md](rust-core-runtime-revised.md) §六）：

- **P0-1 HTTP server**：当前 `Capture` 已就绪但尚无 axum/tokio 服务器消费 `run_with` 的输出（最高优先）。
- **P0-4 `fetch` SSRF 防护**：出网白名单、内网/RFC1918/链路本地 IP 阻断、超时与 body 上限、
  DNS 解析后复检（防重绑定）、禁用重定向到内网。
- **P1-8 事务 `db.tx(fn)`**：回调式事务尚未实现（`DataAccessor` 已预留连接复用位，但 `tx` op 未加）。
- **P2-9 V8 沙箱**：执行看门狗（`terminate_execution`）+ `ResourceLimiter` 内存上限。
- **P2-10 op 边界埋点**：`metrics` + `/metrics` 端点（exec 时长、op 计数/延迟、db 延迟、v8 堆）。
- **P2-11 错误信息收敛**：`json.fail` 不泄露 Rust/DB 内部堆栈。
- **P3-15 手写 `bridge.d.ts`**：8 个全局对象的 TS 类型（~100 行）。

### 设计红线（不要破）

- 动态标识符（表/列）**只**来自 `SchemaRegistry`，绝不来自 JS 字符串。
- 值**只**通过绑定参数传递，不拼接 SQL。
- `JsRuntime` 是 `!Send`：池与持有它的 event loop 同线程（current_thread）；inspector/WS 用 `spawn_local`。
- 失败的 runtime **不回池**（drop 而非 checkin），避免复用可能损坏的 isolate。
- `StableState` 在首次 checkout 后不可变；命名实例/注册表须在 `run` 前注入。

---

## 11. 排错

| 现象 | 原因 / 处理 |
|---|---|
| `Extension code must be 7-bit ASCII` | `bootstrap.js` 含非 ASCII（如中文注释）。改为英文。 |
| `Cannot create a handle without a HandleScope`（进程退出时） | 创建了未轮询 event loop 的空闲 JsRuntime 并 drop。本仓库已用 RuntimePool 按需增长规避；不要手写 `warm()` 预热。 |
| `unknown table 'x'` / `unknown column 'y'` | 该标识符不在 `SchemaRegistry`。先在 `with_opts` 的 `registry.table(...)` 声明。 |
| handler 无响应 / 永远 pending | 未调用 `json.ok/fail/finish`；或 `.catch` 缺失导致 Promise rejection 未被捕获。 |
| `fetch` 回环慢（ms 级） | 系统代理拦截。已 `no_proxy`；确认 `reqwest::Client` 未被注入代理。 |
| inspector 连不上 | 须 `with_opts(..., true)` 构造 **且** 调用 `start_inspector`；runtime 须 `inspector: true`。 |

---

## 12. 提交与 CI 建议

- `cargo fmt --check` + `cargo clippy -D warnings` 作为门禁。
- 二进制体积门禁 ≤ 55MB（`strip` + `lto=fat` + `codegen-units=1`；`opt-level="z"` 勿为省 MB 牺牲吞吐）。
- 原生 runner 多平台（macos arm64/x64、ubuntu x64、windows x64）；缓存 `~/.cargo` + `rusty_v8` 下载。
- 精确 pin `deno_core` 版本（V8 ABI 随其变化）。
- `cargo test` 单入口全绿方可合并。
