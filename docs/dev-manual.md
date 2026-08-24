# oj server 开发手册

面向要在本仓库上继续开发的工程师。先读 `docs/user-manual.md` 了解对外行为，再读本文了解内部实现。

> 历史：早期 Go 版（`devserver`）与 deno_core 0.409 时代的文档见 `docs/dev-guide.md`，已过时，
> 仅作参考。本文描述的是当前 `oj server`（deno_core 0.410）架构。

## 1. 工作区结构（3 个 crate）

```
Cargo.toml            # [workspace] members = ["server", "cli"]；根 crate 本身也是 lib
src/                  # crate: mdm-base-rust（lib + bench）
├── lib.rs            # 导出 bridge + config
├── main.rs           # bench 入口（criterion harness，非服务）
├── config.rs         # 配置加载：server{host,port,base,root,timeout,pool_size} + db/redis 映射、timeout 解析
└── bridge/           # JS 运行时与 SDK（无 axum/http 依赖，纯执行层）
    ├── mod.rs        # 模块聚合、bridge 工厂（Send）
    ├── runtime.rs    # JsRuntime 生命周期 + RuntimePool（max_idle=16）
    ├── loader.rs     # side-module 加载驱动（per-request TLA driver）
    ├── module_loader.rs # ModuleLoader trait 实现：import 解析、CJS 包装、ensure_within
    ├── transpile.rs  # deno_ast TS→JS 类型剥离 + mtime-keyed TranspileCache
    ├── registry.rs   # deno_core op 注册
    ├── http.rs       # RequestInfo{method,params,query,headers,body}、export_bytes
    ├── envelope.rs   # {code,msg,data} 信封、HTTP 状态映射
    ├── json.rs       # json.ok/fail/header ops
    ├── db.rs         # db.query/exec/tx + DB(name) + Dialect（DSN 前缀解析）+ ActiveTx 事务路由
    ├── accessor_sqlx.rs # sqlx Any + 结果行导出（mysql/pg/sqlite 按方案分发）
    ├── query.rs      # 安全查询构造器 op（table.select.where…；按库方言选 QueryBuilder）
    ├── kv.rs         # KVStore trait：InMemoryKV（惰性过期） + RedisKV（fail-fast 真连，redis/kv 同源）
    ├── bus.rs        # 订阅发布总线：publish 广播 + subscribe（WS 会话经 ReqState.req.bus_tx 注册）
    ├── es.rs         # ES 薄封装：EsClient（endpoint + no_proxy reqwest）+ search/index/del ops
    ├── fetch.rs      # fetch op（reqwest 封装的 HTTP 客户端）
    ├── log.rs        # log op（结构化）
    ├── ws.rs         # ws.send/close ops
    ├── inspector.rs  # v8 inspector / 调试辅助
    └── bootstrap.js  # JS 侧 SDK globals（json/db/DB/http/redis/kv/log/fetch/ws/bus/es/finish/__ojRequire）
server/               # crate: mdm-server（axum HTTP 层）
├── lib.rs            # axum app 装配 + 静态站点兜底（server.root：resolve_static/mime_of）+
│                     # serve_with_listener（薄封装）→ serve_router(listener, router)（完整 Router 服务，
│                     # oj server 生产路径 app().merge(ws) 后经此起服务）
├── auth.rs           # JWT 核心（OJ-4）：签验/匿名匹配/login/refresh 轮换/session（KV）
├── routes.rs         # directory-mirror URL → handler 映射（walk_files pub(crate) 供 ws 复用）
├── actor.rs          # JsActor：线程化执行、Send bridge 工厂
└── ws.rs             # WebSocket + js_route/mirror_routes（<dir>/WS.ts→{base}/<dir>/ws 目录镜像挂载）
                      # frame_loop 经 cached_transpile 读源码（WS.ts 类型标注可用）
cli/                  # crate: oj（CLI 入口）
├── main.rs           # entry
├── lib.rs            # CLI lib
├── args.rs           # CLI（clap derive：Cli/Commands + 到 ServerArgs/BuildArgs 的映射）
├── manifest.rs       # manifest.yaml 解析 + module/version 白名单 + manifests.yaml 锁读写
├── pack.rs           # 确定性 tgz 打包（mtime=0/mode 0644/排序 → 同输入同字节）
├── build_cmd.rs      # build 子命令：按模块版本目录构建（转译+minify/剥 .route/routes.js/锁/tgz）
└── server_cmd.rs     # server 子命令：start() + 模式自动判定（is_release）+ release 聚合
```

依赖分层：`bridge`（纯执行，不依赖 HTTP 框架）← `server`（axum 路由 + actor）← `cli`（装配）。

## 2. 构建与测试

```bash
cargo build                                   # debug
cargo build --release                         # release（产物在 target/release/oj）
cargo test -p oj                              # 单测 + e2e
cargo test -p mdm-server                      # server 单测
cargo test --workspace --exclude mdm-base-rust # 全部（见下条说明）
cargo run -p oj -- server -c sample/config.yaml -d sample/src        # dev（按目录自动判定）
cargo run -p oj -- build -d sample/src -o sample/dist
cargo bench -p mdm-base-rust                  # bridge 基准（**必须 release**）
```

> 注：mdm-base-rust 全套 lib 测试已修复为可跑（`cargo test -p mdm-base-rust --lib`，59 通过）。
> 曾有的 `infinite_loop_times_out_and_bridge_survives` SIGSEGV 已于 0bdfa86 修复（看门狗改用
> `v8::IsolateHandle`，见 §3）。

约定（本仓库硬性规范）：
- **debug/release 双绿**：改动后 `cargo test` 与 `cargo build --release` 都要通过才算完成。
- **bench 走 release**：criterion 在 debug 下会因优化缺失失真，基准只看 release 结果。
- **TDD red-first**：先写失败测试 → 确认失败 → 最小实现 → 确认通过 → commit。
- commit 尾随 `unix@vip.qq.com ai`。

## 3. 执行模型（数据流）

```
HTTP 请求
  └─ server/lib.rs handle：依次 路由表 lookup（matchit）→ dev 目录镜像兜底（routes.rs）
     → 静态站点（server.root，仅 GET/HEAD）→ 404
       └─ 命中 api 文件 → 交给 JsActor
            └─ server/actor.rs：线程化执行（Send bridge 工厂），池化 JsRuntime
                 └─ bridge/loader.rs：生成 per-request TLA driver 模块
                      file:///oj/driver/{N}.js  (N = AtomicU64 递增序列)
                      const m = await import(spec);
                      const fn = m.default?.[method];
                      if (typeof fn !== "function") json.fail(405, msg);
                      else await fn();
                      └─ bridge/transpile.rs：TS→JS 类型剥离（mtime-keyed 缓存）
                           └─ bridge/module_loader.rs：import 解析（相对/裸/CJS）
                                └─ deno_core JsRuntime：mod_evaluate + run_event_loop
```

关键点：
- **一个 JsRuntime 一个 main module**，driver 不能是 main；所有用户模块走
  `load_side_es_module_from_code`（side module），driver 内 `await import(spec)` 触发 TLA。
- **两级缓存**：`TranspileCache`（path→(mtime, JS)）+ V8 module cache（靠 mtime 版本化
  specifier `?v=<mtime-nanos>` 实现「改文件即失效」）。
- **KillSwitch**：超时用 `v8::IsolateHandle::terminate_execution` 强杀（`checkout_armed` 取
  `rt.v8_isolate().thread_safe_handle()`，Send+Sync 跨线程句柄——**不是**裸指针：`OwnedIsolate`
  包装地址 ≠ 真实 isolate 指针，手转裸指针会 SIGSEGV，0bdfa86）；被杀的 runtime 直接丢弃
  （不回池），HTTP 返回 408。这是对 `while(true)` 等死循环的唯一可靠熔断手段。

## 4. 关键模块职责（深入）

### 4.1 module_loader.rs — import 解析

- `resolve_inner`：已是绝对 `file://` URL 直接返回（**不再 `ensure_within`**，这是信任模型：
  内部生成的 specifier 可信，外部请求体/路径才需钳制）。
- `resolve_relative`：`./` `../` + 补全 `.ts`→`.js`→`/index.ts`→`/index.js` + 词法归一化 `..`。
- `resolve_bare`：裸 specifier 从当前文件目录逐级向上找 `node_modules/<pkg>`（至 project root），
  按 `package.json` `module`→`main`→`index.js` 取入口，支持 `@scope/name` 与子路径。
- `versioned_specifier`：给 specifier 追加 `?v=<mtime-nanos>` 实现热重载。
- `wrap_cjs` / `looks_cjs` / `op_resolve_cjs`：CJS 互操作（`module.exports`→`default`，
  `require` 走 `__ojRequire`，进程级缓存）。启发式识别，**仅裸 specifier**；相对 `require("./x")`
  是 v0.1 已知限制。
- `ensure_within`：两侧都 `canonicalize` 后做前缀判断，拒绝逃逸；macOS `/var` vs `/private/var`
  的符号链接差异已处理。

### 4.2 http.rs / envelope.rs / blob.rs — 请求与响应与对象存储

- `RequestInfo { method, params, query, headers, body, tenant_id, user, files }`（`params` 在目录
  镜像路由下恒空）。multipart 时 `files: Vec<UploadedFile{field,filename,content_type,bytes}>`，
  文本字段并入 `body`（`{name: value}`）；`op_http_file` 按索引取字节（async + `#[buffer]` 返回，
  sync buffer-return 在 fast-call 路径会卡死）。
- `export_bytes`：空 body → `Value::Null`；可 JSON 解析 → 解析后的 `Value`；否则 UTF-8 字符串。
- 信封 `{code,msg,data}`：`code<=0` → 500；HTTP 状态 = `code`（`code>0`）。
- `blob.rs`：`BlobBackend` 统一契约（put/get/del/url/content_type/serve），local/s3 双驱动可替换。
  local 用 object_store `LocalFileSystem` + `<key>.ct` sidecar 持久化 Content-Type；s3 用
  `AmazonS3`（具体类型才能拿 `Signer` presign）+ GET 15min → `serve` 返回 302。`valid_key`
  逐段白名单（`.`/`..`/`\`/NUL/空段拒绝）；下载路由 `decode_blob_key` 先 percent-decode 再校验。
  `Extras { blob, bus, es }` 是 bridge 构造期扩展点终态（构造期注入，`StableState` 内不可变 Arc）。

### 4.3 bootstrap.js — JS SDK globals

见 `docs/user-manual.md` §9 的完整表。注意 `http` 是**每请求惰性 Proxy**（`op_http_info()`），
`db === DB("default")` 由 JS 侧 `dbCache` Map 保证同源。`json.ok` 在 JS 侧 `JSON.stringify`
后交 Rust 拼接信封，省一次 serde_v8 反序列化。

事务（db.tx）：活跃事务存 `ReqState.tx`（`Arc<ActiveTx>`，故 ReqState 不再 Clone），
query/exec/query_build 按 `resolve_target` 路由（本库 tx 会话 / 他库报错 / 无 tx 走池）；
`Bridge::finalize_tx` 在三条成功路径 checkin 前保底回滚未完结事务。

前置管线：`server::Pipeline` 是 handle() 进 JS 前的单一扩展点（OJ-3 租户/OJ-4 鉴权/OJ-5
blob 已接入，后续只加字段不改编构）；提取/守卫逻辑在 run 闭包的 async 块开头，
失败走 `fail_response(400/401, …)` 信封。鉴权另含内置路由（handle() 顶部
`{base}/auth/*` 先于路由表）与 `auth.rs`（Claims 签验、session 存 KV 键
`AUTH-SESSION:sha256(refresh_token)`，Phase 6 换 RedisKV 时单点替换）。blob 下载路由同样
内置（`{base}/blob/{key}` GET，auth 内置路由之后、路由表 lookup 之前，公开免鉴权）——
local 直出字节 + Content-Type，s3 302 presign。`max_upload` 双闸：axum `DefaultBodyLimit::max(2x)`
兜 2x 外裸 413，handle() 内 `body.len() > max_upload` 出信封 413。

角色鉴权（handler 内按 `http.user.roles` 自行判定）是刻意不加框架层的——路由级
RBAC 等真需求出现再议（YAGNI）。

### 4.4 kv.rs / bus.rs / es.rs — 外部状态与广播（OJ-6）

- `kv.rs`：`KVStore` trait（get/set/del/expire/incr）双实现。`InMemoryKV`（`Mutex<HashMap>` +
  tokio `Instant` 惰性过期）；`RedisKV::arc`（**单例 fail-fast**：先一次性 connect 探测，再包
  `ConnectionManager`——`ConnectionManager::new` 会无限重试，直接用它启动会挂死，故先探测；
  用非弃用的 `get_multiplexed_tokio_connection`）。`redis.*` 与 `kv.*` 同源，auth 会话也存同一
  KV（`AUTH-SESSION:sha256(refresh_token)`），配真 Redis 即多实例共享会话。
- `bus.rs`：进程内主题广播。`Bus { topics: Mutex<HashMap<String, Vec<UnboundedSender<String>>>> }`，
  `publish` try_send 广播 JSON 帧并清理 closed sender（返回接收方数），`subscribe` 去重注册。
  WS 会话的帧通道经 `ReqState.req.bus_tx` 注入（`RequestInfo.bus_tx`，ws.rs frame_loop 里
  `bus_tx→resp_tx` 转发任务与 `ws.send` 同一写出通道，保序）；HTTP 上下文 `bus_tx=None` →
  `op_bus_subscribe` 报错。server 装配共享**一个** `Arc<Bus>`（server_cmd 注入 Extras.bus），
  否则池内各 Bridge 各持空 Bus、订阅与发布不通。
- `es.rs`：`EsClient { endpoint, http }`（`no_proxy` 独立连接），`url_for` 纯函数拼
  `/{index}/_search` 或 `/{index}/_doc/{id}?refresh=true`（endpoint 尾斜杠幂等剪除）。
  index/id 白名单 `[a-zA-Z0-9_-]+` 防路径注入；响应直通（非 2xx 带 ES 返回体）；未配置 → 
  `es not configured`。真连 roundtrip 用 `OJ_TEST_ES` 环境变量驱动（`#[ignore]`）。

## 5. 安全模型

- **project_root 钳制**：所有 import 解析结果必须落在 project root 内（`ensure_within`）。
- **目录穿越防护**：路由路径里的 `..`/`.`/`\`/NUL/空段 → 404。静态兜底（`resolve_static`）
  在此之上先逐段 percent-decode 再校验——解码出 `/`（`%2F` 走私）、`.`、`..`、`\`、NUL、
  空段同样 404。
- **超时熔断**：`server.timeout` + `v8::IsolateHandle::terminate_execution`（408）。
- **SQL 注入**：`db.query/exec` 全部参数化；`db.table().select().where()` 构造器走标识符白名单 +
  参数化值（sea-query）。
- **manifest 强校验**：`manifest.yaml` 的 `name` 必须等于父目录名，防止模块名与路由脱节。

## 6. deno_core 0.410 关键 API 差异

（比 0.409 有破坏性变化，见 memory `deno-core-409-api-quirks` 的对照。）

- `ModuleLoader` 现在是 **trait**（不是 struct），实现它即可接管 import 解析。
- **op2 无 `(async)`**：异步 op 用同步 op2 + `async fn` 包装的模式，不要写 `#[op2(async)]`。
- **错误类型**：`JsErrorBox`（不是 `JsError`）。
- **扩展 JS 的 ASCII 校验仅 debug 生效**：`bootstrap.js` 里中文注释在 release 可用，但 debug 构建
  会因非 ASCII 报错——所以 bootstrap.js 保持 ASCII-only 注释。
- **esm specifier 命名规则**：扩展 JS 用 `ext:core/ops` 之类的命名空间，主模块不能同名。
- 单 JsRuntime 单 main module；side module 用 `load_side_es_module_from_code`。

## 7. 测试结构

- `../oj/tests/e2e.rs`：端到端验收（UC-1…15）。`start()` 返回 2 元组 `(SocketAddr, JoinHandle)`；
  测试用 `cfg.server.port = 0` + `db default = "sqlite::memory:"` 隔离；每个用例都要自带
  `manifest.yaml`（缺失会启动失败）。负向路径覆盖：404（无路由/穿越）、405（方法未导出）、
  500（编译错误）、408（死循环超时后 server 存活）、build→release 全链路。
- 单元测试随模块内联（`#[cfg(test)]`）。
- 当前计数：108 通过（mdm-server 51 + oj lib 42 + e2e 15；E2E_LOCK 串行锁，
  避免端口/文件冲突）+ `mdm-base-rust` lib 59（3 忽略 = 真 ES/外部依赖驱动的 roundtrip）。
  `cargo test --workspace --exclude mdm-base-rust` 全绿；mdm-base-rust 全套 lib 也可跑（§2）。

## 8. 已知设计权衡（v0.1 终审裁决）

见 spec `docs/superpowers/specs/2026-08-22-oj-server-sample-design.md` §8 的 D1–D4：
- 相对 `require()` 不支持——v0.1 已知限制（db 仅 sqlite 已于 v0.2 解除：多库 DSN 按 scheme
  分发，设计见 `2026-08-24-oj-p2-design.md` §1）。Redis 退回内存 KV 已于 v0.2 解除：配置即
  真连 fail-fast（见 §4.4）。
- `build` 已于 2026-08-24 实现（按模块版本目录 + 产物保留原名原结构 + 默认 minify +
  manifests.yaml 锁 + 确定性 tgz + release 聚合），设计见
  `docs/superpowers/specs/2026-08-23-oj-build-design.md`（顶部有去 hash 修订注记）。
