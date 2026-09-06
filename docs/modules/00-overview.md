# 00 · 总览：分层、依赖与红线

> 面向「第一次读这个仓库」的人。代码事实以 `CLAUDE.md` 与源码为准。

## 1. workspace 成员

`Cargo.toml:1-6`，`default-members = ["."]`（故裸 `cargo test` 只跑根 crate）。

| 成员 | 类型 | 职责 |
|---|---|---|
| `.`（`only-js`） | lib | 核心：JS↔Rust bridge + 各后端轴 + 配置模型 |
| `server` | lib | axum HTTP 服务（feature `test-support` 供测试复用） |
| `oj` | lib + bin | CLI（`server`/`build`/`test`/`migrate`/`fixture`/`schema diff`）；bin+lib 双 target 便于 `oj/tests/` 触达装配层 |
| `oj-plugin-ffi` | lib | 宿主与插件共享的 C-ABI 契约（`ABI_VERSION = 7`） |
| `plugins/oj-*`（8 个） | cdylib | es / db-mysql / db-postgres / blob-s3 / bus-kafka / bus-rabbitmq / kv-redis / auth |
| `tools/xtask` | bin | 构建/拷贝/预检，产物归置到 `bin/` |
| `tools/oj-cert` | lib + bin | JWS 证书生成/续签工具 |
| `tests/plugins/mini`、`mini-kv` | cdylib | 加载/ABI/panic 路径的测试夹具 |

## 2. 分层

```
┌─ 业务层（JS/TS）──────────────────────────────────────────┐
│  sample/src/<module>/api.ts  目录镜像即路由，注入全局写业务     │
└───────────────────────────────────────────────────────────┘
┌─ 装配层（oj/）────────────────────────────────────────────┐
│  config → 插件 → 开库 → 迁移/种子 → schema 归属图 → 路由表      │
└───────────────────────────────────────────────────────────┘
┌─ HTTP 层（server/）───────────────────────────────────────┐
│  handle() 前置管线（证书/租户/鉴权/上传）→ RouteTable → actor   │
└───────────────────────────────────────────────────────────┘
┌─ 运行时层（src/bridge/）──────────────────────────────────┐
│  bridge_ext（48 op）+ bootstrap.js（JS 全局）+ RuntimePool    │
└───────────────────────────────────────────────────────────┘
┌─ 后端轴（trait + 内置实现 / FFI 包装）────────────────────┐
│  db / kv / blob / bus / es / auth / oidc / cert / crypto      │
└───────────────────────────────────────────────────────────┘
```

**倒置原则**：bridge 只依赖 trait（`DataAccessor` / `KVStore` / `BlobBackend` /
`EsBackend` / `BusBackend` / `AuthGuard` / `EventBroker`），具体实现由装配层注入；
插件实现经 `src/bridge/ffi.rs` 的 `Ffi*` 包装器适配成同一 trait。因此加一个后端
= 加一个 cdylib，核心零改动。

## 3. 请求全链路（一次 HTTP 调用发生了什么）

1. axum 收到请求，命中 `fallback(any(handle))`（`server/src/lib.rs:128`）。
2. **证书门禁**：`GET` 且状态为 `Expired`/`Grace` → 403（`server/src/lib.rs:272-290`）。
3. **blob 下载**：`GET {base}/blob/{key}` → `BlobBackend::serve`，local 直出 / s3 302。
4. **`RouteTable.lookup`**：四态 `Hit` / `Conflict`(500) / `MethodNotAllowed`(405) / `NotFound`。
5. dev 兜底目录镜像（`Routes::resolve`，且被 `.route` 替换过的方法不复活）。
6. 静态站点兜底（`server.app_path`，GET/HEAD，带穿越防护）。
7. `run(file, params)` 前置管线：
   - **鉴权** `AuthGuard::verify(path, header)` → 失败 401，匿名放行 `Ok(None)`；
   - **租户** 缺失/空 → 400（`tenant.anonymous_paths` 一层通配豁免）；
   - **体积** 超 `max_upload` → 413；
   - **multipart** 解析为文本字段 + `Vec<UploadedFile>`；
   - 组装 `RequestInfo { method, params, query, headers, body, tenant_id, user, files }`。
8. `JsActor::run_module` → `Bridge::run_module`（`src/bridge/mod.rs:544`）：
   - `versioned_specifier(api_path)` 生成 `file://…?v=<mtime>`（缓存失效依据）；
   - 拼 TLA driver `import(m); m.default[method]()`，方法未导出 → `json.fail(405)`；
   - 按 api_path 祖先目录命中 `StableState.modules` → 注入 `ReqState.module`（归属守卫/bound_db 依据）；
   - `checkout_armed` 武装 `KillSwitch`，超时 → `RunError::Timeout`（408，runtime **不归还池**）；
   - `finalize_tx` 保底回滚未提交事务 → `read_capture` → `checkin`。
9. `capture_response` 把 `Capture { status, headers, body }` 原样写回。

## 4. 状态模型（最重要的一条不变量）

| 状态 | 生命周期 | 位置 | 何时可写 |
|---|---|---|---|
| `StableState` | 进程级，`Arc` 共享 | 每个 `JsRuntime` 的 `OpState` | **首次 runtime checkout 之前**装配完；之后只读（`Arc::get_mut` 会 panic） |
| `ReqState` | 每请求 | `OpState` | `checkout` 时 `reset()`，每次 `run_with` 重置 |

`StableState` 持有：`kv` / `dbs: HashMap<String, Arc<dyn DataAccessor>>` / `client` /
`registry: Arc<SchemaRegistry>` / `loader` / `blobs` / `bus` / `es` / `plugins` /
`modules` / `ownership_deny` / `sql_memo` / `boot` / `jwt` / `oidc`。
`ReqState` 持有：`req` / `response` / `status` / `headers` / `done` / `tx` /
`ws_sends` / `ws_close` / `module`（**不可 Clone**：活跃事务句柄 clone = 漏回滚）。

## 5. 设计红线（不可突破）

| 红线 | 落点 |
|---|---|
| SQL 动态标识符只来自 `SchemaRegistry` 白名单；值只走绑定参数 | `src/bridge/guard.rs`、`query.rs`、`registry.rs` |
| `JsRuntime` 是 `!Send`：池与持有者钉在 `current_thread`；inspector/WS 用 `spawn_local` | `src/bridge/runtime.rs`、`server/src/actor.rs`、`server/src/ws.rs` |
| 所有插件 profile 必须 `panic = "unwind"` | 根 `Cargo.toml` `[profile.release]`；`oj_plugin_entry!` 内建 `catch_unwind` |
| `bootstrap.js` 必须 7-bit ASCII | `src/bridge/bootstrap.js` |
| 失败的 runtime 一律丢弃，不归还池 | `src/bridge/mod.rs:386`、`runtime.rs:103-113` |
| 证书必配、无逃生口：两路径缺任一即拒绝启动 | `src/config.rs:80`、`oj/src/app.rs:113` |

## 6. dev / release 双模式

判据唯一：服务目录含 `dist/manifests.yaml` → release（跑预构建 JS，不转译，按锁聚合）；
否则 dev（服务 `src`，按需转译 TS，`notify` 热重载）。见 `oj/src/server_cmd.rs:105`。

| 维度 | dev | release |
|---|---|---|
| 入口 | `src/<module>/api.ts` | `dist/<module>-<version>/api.js` |
| 路由来源 | 启动内省每个 `api.ts` 的 `default[m].route` | 直载各模块 `routes.js`（一次 import/模块） |
| 迁移门禁默认 | `auto`（apply） | `verify`（账本落后/有待应用 → 拒启） |
| 静态兜底 | 表 miss 时目录镜像回退 | 无 |

## 7. 文件地图（改哪里找哪里）

| 我想改… | 打开 |
|---|---|
| 给 JS 加一个全局/方法 | `src/bridge/bootstrap.js` + 对应 `op_*`（`src/bridge/*.rs`）+ `bridge_ext` ops 表（`src/bridge/mod.rs:194`） |
| 加一个配置字段 | `src/config.rs` +（需要时）`oj/src/app.rs` 装配 + `docs/user-manual.md` |
| 改请求前置逻辑 | `server/src/lib.rs` 的 `handle` / `Pipeline` |
| 改路由匹配/冲突 | `server/src/routes.rs` |
| 加一个后端轴 | `oj-plugin-ffi/src/<axis>.rs` + `AXES`（`src/bridge/plugin_loader.rs:428`）+ `probe_axes` + 插件 crate |
| 改构建产物 | `oj/src/build_cmd.rs`、`pack.rs`、`manifest.rs` |
| 改迁移/种子/schema | `oj/src/migrate.rs`、`seed.rs`、`schema.rs`、`checks.rs` |
| 改测试运行器 | `oj/src/test_cmd.rs`、`oj/src/test_ext.rs`、`oj/src/test_ext/test_bootstrap.js` |
| 跑 sample 测试（L1/L2） | `cd sample && npm run test`（= `test:unit` + `test:api`），判据见 [08-testing.md](08-testing.md) |
