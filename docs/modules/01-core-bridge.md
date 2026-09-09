# 01 · 核心运行时（`src/`，根 crate `only-js`）

`src/lib.rs` 只导出两样：`pub mod bridge;` 与 `pub mod config;`。
clippy 门禁对本 crate **生效**（`cargo clippy --workspace --all-targets -- -D warnings`；
2026-09-06 已移除曾有的 crate 级 `#![allow(clippy::all)]`，见 [05-ffi-and-plugins.md](05-ffi-and-plugins.md) 与
`docs/review-2026-09-06.md` P1-2）。`RefCell` 借位跨 await 这类问题 clippy 兜不全，
仍靠注释与评审兜（见 `runtime.rs` 对 `checkout()` 借位纪律的显式警告）。

## 1. `bridge_ext`：op 表（唯一的 JS↔Rust 边界）

`src/bridge/mod.rs:192` 用 `deno_core::extension!` 注册 **48 个 op**（`mod.rs:195-242`），`bootstrap.js` 作为
ESM 入口把它们装配成全局对象。加一个 JS 能力 = 加一个 `op_` + 进 ops 表 + 在
`bootstrap.js` 挂全局。

| 轴 | op | 所在文件 |
|---|---|---|
| 会话/信封 | `op_finish`、`op_json_ok/fail/header/raw` | `mod.rs`、`json.rs` |
| 请求上下文 | `op_http_info`、`op_http_file` | `http.rs` |
| KV | `op_kv_get/set/del/expire/incr` | `kv.rs` |
| DB | `op_db_has`、`op_db_query`、`op_db_exec`、`op_db_tx_begin/commit/rollback`、`op_db_query_build` | `db.rs`、`query.rs` |
| blob | `op_blob_put/get/del/url/content_type` | `blob.rs` |
| bus | `op_bus_publish/subscribe/kind` | `bus.rs` |
| es | `op_es_search/index/del` | `es.rs` |
| 内省/fetch/日志 | `op_plugins`、`op_fetch`、`op_log` | `plugins_op.rs`、`fetch.rs`、`log.rs` |
| 模块解析 | `op_resolve_cjs` | `module_loader.rs` |
| OIDC | `op_oidc_sign/verify/info` | `oidc.rs` |
| WS | `op_ws_send`、`op_ws_close` | `ws.rs` |
| 证书 | `op_cert_gen`、`op_cert_renew` | `cert.rs` |
| 密码学 | `op_jwt_sign/verify/durations`、`op_bcrypt_hash/verify`、`op_sha256_hex`、`op_random_hex` | `crypto.rs` |

## 2. `bootstrap.js` 装配出的 JS 全局

文件必须保持 **7-bit ASCII**（非 ASCII 会触发 deno_core 报错）。

| 全局 | 方法 | 备注 |
|---|---|---|
| `json` | `ok` / `fail(code,msg,data?)` / `header` / `raw` | 统一信封；`raw` 出裸 JSON（OIDC 标准端点用） |
| `http` | 只读代理：`method`/`param(name,def)`/`query`/`headers`/`body`/`tenantId`/`user`/`file(i)` | `param` 先查 path params 再回落 query |
| `db` / `DB(name)` | `query(sql, params?)` / `exec` / `table(t)` / `tx(fn)` | `db === DB("default")`，JS 侧 Map 缓存保证同一性 |
| `kv` / `redis` | `get` / `set` / `del` / `expire(key, 秒)` / `incr` | 二者是同一实现的两个名字 |
| `blob(name)` | `put` / `get` / `del` / `url` / `contentType` | 裸 `blob.put(...)` = `blob("default").put(...)` |
| `bus` | `publish` / `subscribe` / `kind()` | `kind` 返回 local/kafka/rabbitmq |
| `es` | `search` / `index` / `del` | 未配置报 "es not configured" |
| `ws` | `send` / `close` | 非 WS 场景 no-op |
| `fetch(url, opts)` | 浏览器兼容子集 | 经 `reqwest` |
| `log` | `debug/info/warn/error(msg, ...kv)` | zap 风格交替键值 |
| `plugins()` | — | 返回已加载插件自描述 |
| `cert` | `generate(bits, nbf, exp)` / `renew(pem, nbf, exp)` | RSA keygen + RS256 在 Rust |
| `jwt` | `sign` / `verify` / `accessDuration` / `refreshDuration` | |
| `bcrypt` | `hash` / `verify` | Rust 侧 `spawn_blocking` |
| `oidc` | `sign` / `verify(token, jwks?)` / `jwks()` / `issuer` / `rp` / `clients` | 私钥不出 Rust |
| `crypto` | `sha256Hex` / `randomHex` | 合并进原生 `crypto` |
| `finish()` | — | 结束会话且不写响应 |
| `__ojRequire(name, referrer)` | — | CJS 互操作（进程级缓存） |

## 3. 状态模型

见 [00-overview.md §4](00-overview.md)。关键补充：

- `StableState`（`mod.rs:100`）经 `bridge_ext` 的 `options = { stable }` 注入每个 runtime
  并 `state.put(ReqState::default())`。
- `ReqState::tx` 为 `Option<Arc<ActiveTx>>`，`reset()` 时 `tx = None` **即 drop = 回滚**；
  `Bridge::finalize_tx`（`mod.rs:458`）在 checkin 前再兜底一次。
- `StableState.sql_memo: Mutex<HashMap<String, Arc<Vec<String>>>>` 缓存裸 SQL 表名提取
  （守卫热路径）。

## 4. `Bridge` 执行入口（`mod.rs:269`）

| 方法 | 用途 | 超时 | 失败 runtime |
|---|---|---|---|
| `run` / `run_with(src, req)` | 执行源码（旧路径/内嵌） | 无 | 丢弃 |
| `run_named(name)` | 走 `HandlerStore` | 无 | 丢弃 |
| `run_with_timeout` / `run_ws` | 带 `KillSwitch`；WS 额外带出 `sends` / `close`（生产 WS 自 v0.1.10 改走帧池 `ws_connect`+`ws_event`，`run_ws` 仅测试/内嵌用） | 有 | 丢弃（不 checkin） |
| `run_module(api_path, method, req, timeout)` | **生产路径**：ESM 模块 + TLA driver | 有 | 丢弃 |
| `introspect_module(api_path)` | 启动期读 `default[m].route` | 2s（`INTROSPECT_TIMEOUT`） | 丢弃 |
| `read_module_default(path)` | release 直载 `routes.js` | 2s | 丢弃 |
| `prewarm()` | 装配期跑一次 boot，把错误前移 | — | — |

`run_side_driver`（`mod.rs:630`）是后三者的公共管道：driver 以 **side module** 加载
（`file:///oj/driver/{n}.js` 递增），因为池化 runtime 每 JsRuntime 只能有一个 main module。

## 5. `RuntimePool` 与 `KillSwitch`（`runtime.rs`）

- 空闲上限 `DEFAULT_MAX_IDLE = 16`，超出丢弃（drop 析构 V8 isolate）。
- `checkout()` 是**唯一**借出口，故 ext_boot 只需在此加载一次；借出的 runtime 保证已 boot。
- boot 超时 `BOOT_TIMEOUT = 2s`；boot 失败/熔断两条路径都先补跑一轮 event loop 再丢弃
  （未轮询完的 isolate 析构会触发 V8 句柄错误，本项目有 SIGSEGV 前科）。
- `KillSwitch` 每 Bridge 一个看门狗线程，25ms 轮询，到期跨线程 `terminate_execution`；
  线程只持 `Weak`，`Drop` 时置位 + join，并跳过 self-join（EDEADLK，有回归测试）。

## 6. 各轴模块速查

| 文件 | 职责 |
|---|---|
| `db.rs` | `DataAccessor` / `TxSession` trait、`Dialect`、`InMemoryAccessor`、事务 op |
| `db_backend.rs` | `DbBackend` 工厂 trait + `DbBackendRegistry`；内置 `SqliteBackend` / `MemoryBackend` |
| `accessor_sqlx.rs` | `SqlxAccessor`：sqlx 真实实现（any/sqlite） |
| `query.rs` | `op_db_query_build`：sea-query + 白名单的安全查询构造器 |
| `registry.rs` / `named_registry.rs` | `SchemaRegistry`（表/列白名单 + 归属 owner）；命名实例注册表 |
| `guard.rs` | `extract_tables` / `check_raw` / `check_table` / `bound_db` —— **SQL 注入根治点** |
| `kv.rs` / `blob.rs` / `bus.rs` / `bus_backend.rs` / `broker/` / `es.rs` | 各轴 trait + 内置实现；`broker::build_broker` 按 cfg 选进程内/插件 |
| `http.rs` | `RequestInfo` / `UploadedFile` |
| `envelope.rs` | `{code,msg,data}` 与 code→HTTP status 映射 |
| `crypto.rs` / `cert.rs` / `oidc.rs` / `auth.rs` | 密码学原语、JWS 证书、OIDC 状态、守卫 trait |
| `module_loader.rs` / `loader.rs` / `transpile.rs` | ESM/CJS 解析（`?v=<mtime>`）、HandlerStore、TS 转译 + mtime 缓存 |
| `ffi.rs` / `plugin_loader.rs` | 见 [05-ffi-and-plugins.md](05-ffi-and-plugins.md) |
| `inspector.rs` | DevTools inspector WS 桥 |
| `log.rs` | `op_log` → tracing |
| `plugins_op.rs` | `op_plugins` |

## 7. 已知债 / 需注意

- ~~`Shared` / `_SharedCompat` 死别名~~ —— 已于 2026-09-06 删除（连同 unused `RefCell` import）。
- `kv` 与 `redis` 两个全局指向同一实现，命名双轨；新代码统一用 `kv`（改名会破坏兼容，未动）。
- 测试分布较散：`src/bridge/mod.rs` 的 `mod tests`（约 1900 行文件中近半是测试）、
  `src/bridge/ffi.rs` 的 `mod adapter_tests`（FFI 适配器 + mock vtable，约 700 行）、
  `src/bridge/plugin_loader/tests.rs`（381 行）三处分离；拆文件可提升可读性。
