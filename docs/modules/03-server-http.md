# 03 · HTTP 服务（`server/`）

crate 名 `server`（对外 `use server::...`），依赖只有 `only_js`。
模块：`lib.rs`（装配 + `handle`）、`routes.rs`、`actor.rs`、`ws.rs`、
`certificate.rs`、`certificate_watcher.rs`、`logging.rs`、`test_support.rs`。

## 1. `app()` 装配顺序（`lib.rs:106`）

```
Router::new()
  .route({base}/health,  get)          // 监控端点：不受证书 GET 门禁
  .route({base}/plugins, get)          // 插件清单：匿名，保留路径，遮蔽同名业务路由
  .fallback(any(handle))
  .layer(log_requests)                 // method/path/status/耗时
  .layer(DefaultBodyLimit::max(max_upload * 2))
  .with_state(AppState { .. })
```

`AppState`（`lib.rs:43`）持有 `table: RouteTable`、`fallback: Option<Routes>`（dev 目录镜像）、
`actor: JsActor`、`timeout`、`static_root`、`pipeline: Pipeline`、`base`、
`certificate_status` / `certificate_valid_until`（`Arc<RwLock<..>>`，热加载共享）、
`plugins: Arc<Vec<PluginInfo>>`。

`Pipeline`（`lib.rs:66`）是 handle 前置管线的**唯一扩展点**：
`tenant_header` / `tenant_anon` / `auth: Option<Arc<dyn AuthGuard>>` / `max_upload` /
`blob: Option<Arc<dyn BlobBackend>>`。

## 2. `handle()` 分支优先级（`lib.rs:263`）

1. **证书门禁**（仅 GET）：`Expired`/`Grace` → 403（`/health`、`/plugins` 已先注册，不受限）。
2. **blob 下载**：`GET {base}/blob/{key}` → `decode_blob_key`（逐段 percent-decode +
   `valid_key`，防 `%2e%2e` 穿越）→ `BlobServed::Bytes` 直出 / `Redirect` 302 / Err 404。
3. **鉴权**：`AuthGuard::verify(path_no_base, Authorization)` → `Err` 401；`Ok(None)` 匿名放行。
   注意：路径**不在 base 下 → 不设防**。
4. **租户**：缺失/空 → 400；`tenant.anonymous_paths` 命中则豁免缺失（OIDC 302 带不了头），
   但已带的头仍注入。
5. **体积**：`body.len() > max_upload` → 413（超 2x 的已在 axum 层裸 413）。
6. **multipart**：文本字段并入 body，`Vec<UploadedFile>` 入 `RequestInfo.files`。
7. **路由**：`normalize` → `RouteTable.lookup` → `Hit` 执行 / `Conflict` 500 /
   `MethodNotAllowed` 405 / `NotFound` 继续。
8. **dev 目录镜像兜底**：`Routes::resolve`，且被 `.route` 替换掉的方法不复活（`is_replaced`）。
9. **静态兜底**：GET/HEAD + `resolve_static`（逐段解码 + 越界段拒绝）→ 文件响应。
10. 404 信封。

`path_matches`（`lib.rs:94`）语义：精确匹配，或尾 `/*` **严格一层**前缀通配
（`/oidc/*` 命中 `/oidc/callback`，不命中 `/oidc` 与 `/oidc/a/b`）。
⚠️ 该函数在 `server` 与 `oj-auth` 插件里各有一份（插件不能依赖 server crate），
注释互指，改一处须同步另一处。

## 3. 路由表（`routes.rs`）

- `Routes`（:14）：dev 目录镜像解析器（`ts` 找 `api.ts`，否则 `api.js`）。
- `normalize`（:55）：拒绝 `\`、`\0`、空段、`.`、`..`；尾斜杠归一。
- `decode_params`（:75）：percent-decode + 走私校验（拒绝解码后出现 `/` 的单段参数，
  即 `%2F` 走私；catch-all 放行）。
- `method_name`（:93）：HTTP 动词 → handler 方法名（DELETE→`del`），未映射 → 405。
- `RouteTable`（:140）：单个 `matchit` matcher，pattern → `{method → Entry}`；
  `Entry::{File(FileId, ..), Conflict}`，故 405 判定 O(1)；`files` 表消除 PathBuf 重复存储。
- `Lookup` 四态：`Hit` / `Conflict` / `MethodNotAllowed` / `NotFound`。
- `entries_from_value`（:414）：release 从 `routes.js` default 导出行集反解。
- `bridge_introspector`（:453）：**每文件一线程 + 独立 current_thread runtime**
  （`Bridge` 是 `!Send`，嵌套 runtime 会 panic）。
- `bridge_default_reader`（:477）：release 直载 `dist/<m>-<v>/routes.js`，同构。

## 4. JS actor（`actor.rs`）

把 `!Send` 的 `Bridge` 钉在专用 OS 线程上，axum 侧只经 channel 通信（future 天然 `Send`）。
actor 线程内跑 `current_thread` runtime，**串行**执行 job；并发度 = actor 数 =
`server.pool_size`。`JsActor::pool(n, make_bridge)` 建池；`RunFail { timeout, msg }`
区分 408 与 500。

## 5. WebSocket（`ws.rs`）

- `mirror_routes`（:46）：`<root>/<dir>/WS.ts`（优先）/`WS.js` → `GET {base}/<dir>/ws`；
  根级 `WS.ts` → `{base}/ws`。⚠️ release 下 root=dist，URL 含版本段（`news-0.1.0/ws`），
  v0.2 已知限制。
- 每连接独占一个 VM（不进 HTTP 池），整个连接搬到专用 OS 线程；
  Reader / Processor（串行 JS）/ Writer 三任务流水线，`msgChan`/`respChan` 各 cap 64（背压）。
- 单帧超时 → 丢弃该帧、连接继续。

## 6. 证书（`certificate.rs` / `certificate_watcher.rs`）

- 证书 = JWS 三段式 `Base64URL(Header).Base64URL(Payload).Base64URL(Signature)`，
  `alg: RS256`，payload 至少 `{nbf, exp}`。
- `load_certificate_at(status_cfg, config_dir)` → `(CertificateStatus, valid_until)`；
  **两路径缺任一即 Err，绝不 fail-open**。
- 状态：`Valid` / `Grace { remaining_secs }` / `Expired`。
  启动期 `Expired`（宽限已过）→ 拒启；运行中变 `Expired` → 仅限制 GET（403），服务不中断。
- 热加载：`notify` 监听公钥/证书文件（inotify/FSEvents/ReadDirectoryChangesW），
  覆盖即原子更新；加载失败保留旧状态只告警。
- `spki_to_pkcs1` 是手写的最小 DER 解析，把 SPKI 剥成裸 RSA PKCS#1 DER 给 ring。

## 7. 日志（`logging.rs`）

- **终端输出的完整镜像落盘**：stdout/stderr 各自重定向到独立管道，后台线程逐块写回原终端
  与日志文件（去 ANSI）。`println!`/`eprintln!`/panic/tracing 全部同形落盘。
- 每次启动一个新文件 `server-<启动秒>_<pid>.log`；按大小滚动（`base.1.log`…），保留 N 个。
- `init` 幂等；镜像线程与 fd 按进程生命周期泄漏（对齐 `non_blocking` guard 惯例）。
- **默认关闭终端输出**（`server.console_log` / `--console-log` 打开）；非 unix 无落盘，
  强制保留终端输出并告警。
- 仅 unix 实现；`install_terminal_tee` 公开是为 `server/tests/log_tee.rs`
  （进程级 tee 必须独占一个测试二进制）。

## 8. `test_support`（feature `test-support`）

全仓唯一的测试证书夹具：`write_cert(dir, nbf, exp)` / `write_cert_into(&mut cfg, dir, ..)`
生成**真实签名**的 JWS + SPKI 公钥并写回 `ServerCfg`。
证书门禁后所有启动测试的公共前置，由 `oj`（server_cmd/e2e）与根 crate（dev-dep）复用。

## 9. 已知债

- `path_matches` 与 `oj-auth` 的 `is_anonymous` 逻辑重复（见 §2 警告）。
- `serve` / `serve_with_listener` 仍保留旧的多参签名（内部委托 `App`），与 `App::serve`
  存在两套入口；新代码应走 `serve_router`。
- WS release 路径含版本段，是已知限制。
- `handle` 中证书门禁只拦 GET（有意为之，让运维有时间换证书）。
