# bridge —— deno_core JS SDK 桥接模块

向 V8 JsRuntime 注入 JS SDK 全局对象，供 JS handler 编写业务逻辑，Rust 侧捕获统一信封响应。

## 架构

```
┌──────────────── JS handler ───────────────┐
│  json / db / DB / http / kv / redis       │
│  / blob / bus / es / ws / cert / jwt      │
│  / bcrypt / crypto / fetch / log / finish │
│        (bootstrap.js 装配，全参考见        │
│         devkit/api-manual.md)             │
└───────────────── op 调用 ────────────────┘
                   │ #[op2]
┌──────────────────┴───────────────────────┐
│  ops (json/db/kv/fetch/http/log/query)   │
│   ├─ StableState (Arc, 跨请求共享)        │
│   │     kv / dbs / client / registry      │
│   └─ ReqState  (OpState 内, 每请求 reset)  │
│  DataAccessor / KVStore 契约（trait）     │
└────────────────────┬──────────────────────┘
                     │ RuntimePool（复用 JsRuntime）
              checkout → reset(req) → run → checkin
```

- **状态分片**：`StableState` 跨请求不可变（内部句柄均为 `Arc`），`ReqState` 每请求可变，
  存于 `OpState`，`run_with` 借出 runtime 时整体 `reset(req)`。两者分离使 `JsRuntime` 可池化复用。
- **RuntimePool**：复用 `JsRuntime`（V8 isolate 1~10ms 开销），idle 上限 `DEFAULT_MAX_IDLE=16`。
  "快照"等价于已加载 `bootstrap.js` 的预热 runtime——bootstrap 仅编译一次，后续请求只执行 handler 源码。
  **仅成功执行（已轮询 event loop）的 runtime 才归还**；失败的可能 isolate 损坏，直接 drop。
- **异步模型**：op 以 `#[op2] async fn` 直接返回 JS Promise，由 deno_core event loop 驱动，无需手写回切。
- **JS 侧 API 形状**由 `bootstrap.js`（扩展入口）装配，将 `op_*` 暴露为 `globalThis` 上的全局对象。

## JS API

### json —— 统一信封

```js
json.ok(data)              // {code:0, msg:"ok", data} → status 200，标记会话完成
json.fail(code, msg, data) // code<=0 映射 500
json.header(name, value)   // 设置返回头（覆盖语义），空名忽略
```

信封在 Rust 侧单遍序列化（不构中间 `Value` 树，直接写 buffer），marshal 成本约 70 ns。

### db / DB(name) —— 数据访问（Promise）

```js
// 原始 SQL + 绑定参数（参数防注入，推荐）。
const rows = await db.query("select * from user where id = $1", [1]); // Row[]（JSON 对象数组）
const n = await db.exec("update user set age = $1 where id = $2", [19, 1]); // 受影响行数

// 安全查询构造器（标识符白名单 + 值参数化，强烈推荐）。
const rows = await db.table("user")
  .select(["id", "name"])
  .where({ field: "age", op: "gte", value: 18 })
  .orderBy([{ field: "id", dir: "desc" }])
  .limit(10).all();

db === DB("default")                               // true（JS 侧缓存保证引用相等）
DB("reports")                                      // 命名实例；未配置返回 undefined
```

参数占位符随底层驱动而定：`$1` 用于 Postgres，`?` 用于 MySQL/SQLite（sea-query 构造器固定生成
Postgres 风格 `$N`，接入非 PG 驱动时需转换占位符，见 accessor_sqlx.rs 注释）。

### http —— 请求上下文（只读快照）

```js
http.method  // string
http.params  // {} 路径参数
http.query   // {} 查询参数
http.headers // {} 请求头
http.body    // 能解析为 JSON 则为对象/数组，否则为字符串；空为 null
```

### kv / redis —— KV 存储（Promise，同一实现）

```js
await kv.set("k", "v");        // true
const v = await kv.get("k");   // string | null
await kv.del("k");             // 幂等
await kv.expire("k", 60);      // 相对秒数；键不存在 → false
await kv.incr("k");            // 原子自增，缺失从 0 起
```

`kv` 与 `redis` 是同一 KV 的两个名字。真 Redis 由 **oj-kv-redis 插件**提供；未装配时回落
进程内内存 KV（`kv.rs` 的 `InMemoryKV`，联调用）。

### log —— 结构化日志（msg + 交替键值对，同 zap SugaredLogger）

```js
log.info("user login", "user_id", uid, "ip", ip);
log.error("db query failed", "sql", sql, "err", err);
```

### fetch —— 浏览器 Fetch API 兼容（reqwest 实现）

```js
const resp = await fetch("https://api.example.com/data", {
  method: "POST", headers: { "X-A": "1" }, body: "ping",
});
resp.ok; resp.status; resp.statusText; resp.headers;
const data = await resp.json();   // 空 body → null
const text = await resp.text();
const buf  = await resp.arrayBuffer(); // Uint8Array
const r2   = resp.clone();
const { done, value } = await resp.body.getReader().read(); // 缓冲模拟：首读全量，再读 done
```

未设置 Content-Type 且有 body 时自动补 `text/plain;charset=UTF-8`。不支持 AbortController。
HTTP 客户端为单个共享 reqwest Client（连接池复用），构造时固定 `no_proxy`——不走系统代理
（macOS 系统代理会把连回环的请求也拦截转发，实测 65 µs/req 劣化到 1.9 ms/req）。

> 注意：reqwest 默认会读取 macOS 系统代理配置，本机有代理软件时连回环地址都会被
> 拦截转发——这是 `no_proxy` 的直接原因。

### finish —— 标记会话完成

```js
finish(); // 等价于 json.ok/fail 的 SignalDone 语义，但不写响应
```

### ext_boot.js —— 上面各全局的运行时补充

上表全局由 `bootstrap.js` 编译期装配（改它要重编二进制）。`ext_boot.js` 是运行时补充：
`<config_dir>/ext_boot.js` 存在即被每个**新建**的 JsRuntime 加载执行一次（ESM，支持顶层
`await` 与 import 项目内模块），可在已有全局上做组合增补。

```js
// ext_boot.js
export {};
json.page = (rows, total) => json.ok({ list: rows, total });
```

三条硬边界：必须幂等（执行次数 = 模块数 + actor 池大小 + WS 连接数）；拿不到
`ext:core/ops`（deno_core 拒绝 `file://` → `ext:` 导入），需要新 op 属改 bootstrap；
用顶层 `await` 须带 `export {};`，否则被 CJS 启发式包进非 async 函数。

完整用法与约束见 `docs/user-manual.md` §9「扩展全局对象」，设计与评审依据见
`docs/superpowers/specs/2026-09-02-ext-boot-design.md`。

## Rust API

```rust
use std::sync::Arc;
use only_js::bridge::{
    Bridge, InMemoryAccessor, InMemoryKV, RequestInfo, SchemaRegistry,
};

let db = Arc::new(InMemoryAccessor::new());
db.seed([serde_json::json!({"id": 1})]);
let kv = Arc::new(InMemoryKV::new());

// schema 注册表：动态标识符白名单（SQL 注入根治点）。仅在构造时声明。
let registry = SchemaRegistry::new()
    .table("user", Some("id"), &["id", "name", "age"]);

// 含注册表 + inspector 开关；inspect=true 时运行时启用 DevTools inspector。
let mut b = Bridge::with_opts(db, kv, registry, false);

// 命名实例须构造期全量注入（StableState 一经 runtime 池共享即不可变；
// 早期版本的 set_db_accessors 已删除——池化后 Arc::get_mut 必 panic）。
let b = Bridge::with_dbs(
    HashMap::from([("default".into(), default_da), ("reports".into(), reports_da)]),
    kv, registry, false,
);

// 注入请求上下文并执行；run() 等价于 run_with(src, RequestInfo::default())。
let cap = b.run_with(handler_js_source, RequestInfo {
    method: "GET".into(),
    query: [("id".into(), "1".into())].into_iter().collect(),
    ..Default::default()
}).await?;

// 捕获响应由 run_with 直接返回，无需再取。
let Capture { status, headers, body } = cap;

// 按名执行已加载的 handler（生产用嵌入 map，开发用 MDM_HANDLER_DIR 热重载）。
let cap = b.run_named("user.get").await?;
```

`Bridge::new(db, kv)` 是 `with_opts` 的便捷形式：默认空注册表、`inspect=false`、并将传入 db 注册为 `"default"`。

### 契约（依赖倒置）

```rust
#[async_trait]
pub trait DataAccessor: Send + Sync {
    async fn query(&self, sql: &str) -> BridgeResult<Vec<Row>> { ... }        // 默认转 *_with_params
    async fn exec(&self, sql: &str) -> BridgeResult<i64> { ... }             // 默认转 *_with_params
    async fn query_with_params(&self, sql: &str, params: &[Value]) -> BridgeResult<Vec<Row>>;
    async fn exec_with_params(&self, sql: &str, params: &[Value]) -> BridgeResult<i64>;
}

#[async_trait]
pub trait KVStore: Send + Sync {                   // 后续真实 Redis 以同接口接入
    async fn get(&self, key: &str) -> BridgeResult<Option<String>>;
    async fn set(&self, key: &str, value: &str) -> BridgeResult<()>;
}
```

`BridgeResult<T> = Result<T, Box<dyn Error + Send + Sync>>`，op 层转为 `JsErrorBox` 抛给 JS。
`SqlxAccessor`（accessor_sqlx.rs）已实现 `DataAccessor`，以 `Pool<Any>` 驱动无关接入真实 MySQL/PG/SQLite。

## 测试与性能

```bash
cargo test --release --workspace   # 全部测试（根 crate 单测 + oj e2e + 插件）
cargo build --benches && cargo bench   # criterion 性能测试（含吞吐统计），见 docs/benchmarks.md
```

当前量级（Apple Silicon，release）：异步 op 边界约 140 ns（redis/db.exec 7 M/s 级），
json.ok 230 ns，db.query 413 ns，fetch 本地回环复用连接 30 µs/req（33.7 K/s）。
详见 [benchmarks.md](benchmarks.md)（含优化前后对比与根因分析）。

## deno_core 0.410 移植要点

- `#[op2]` 无 `(async)` 标志，`async fn` 自动识别。
- `#[serde]` 位置须写全限定 `serde_json::Value`；`Option<String>` 参数用 `#[string]`。
- 扩展 JS 必须 7-bit ASCII；`esm_entry_point` specifier 为 `ext:<扩展宏名>/<file>`。
- 同步 op 第一参 `&mut OpState`，异步 op 第一参 `Rc<RefCell<OpState>>`。
