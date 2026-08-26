# oj server 开发手册

面向要在本仓库上继续开发的工程师。先读 `docs/user-manual.md` 了解对外行为，再读本文了解内部实现。

> 历史：早期文档见 `docs/dev-guide.md`，已过时，仅作参考。本文描述的是当前
> `oj server`（deno_core 0.410）架构。

## 1. 工作区结构（宿主 + 契约 crate + 插件）

```
Cargo.toml            # [workspace] members = ["server", "oj", "oj-plugin-ffi", "plugins/*", "tools/xtask"]
src/                  # crate: only-js（lib + bench）——核心执行层
├── lib.rs            # 导出 bridge + config
├── main.rs           # bench 入口（criterion harness，非服务）
├── config.rs         # 配置加载：server{host,port,base,root,timeout,pool_size} + db/redis/blob/es/broker/plugins 映射
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
    ├── accessor_sqlx.rs # sqlx Any（内置 sqlite 驱动）+ 结果行导出
    ├── db_backend.rs # DbBackend 注册表（内置 sqlite/memory + 插件 db 工厂；scheme 认领）
    ├── query.rs      # 安全查询构造器 op（table.select.where…；按库方言选 QueryBuilder）
    ├── kv.rs         # KVStore trait + 内置 InMemoryKV 兜底（真 Redis 迁插件）
    ├── bus.rs        # 订阅发布总线：内置 local Bus + EventBroker trait（插件 kafka/rabbitmq）
    ├── es.rs         # EsBackend trait + 内置 reqwest 实现（oj-es 插件经 FfiEsBackend 适配）
    ├── blob.rs       # BlobBackend trait + LocalBlob 内置（s3 迁插件）
    ├── ffi.rs        # 全部 unsafe 收敛（load_forget dlopen）+ FfiXxxBackend 适配器层
    ├── plugin_loader.rs # PluginLoader：四级路径解析 + 清单/扫描双模式 + ABI 门禁
    ├── fetch.rs      # fetch op（reqwest 封装的 HTTP 客户端）
    ├── log.rs        # log op（结构化）
    ├── ws.rs         # ws.send/close ops
    ├── inspector.rs  # v8 inspector / 调试辅助
    └── bootstrap.js  # JS 侧 SDK globals（json/db/DB/http/redis/kv/log/fetch/ws/bus/es/finish/__ojRequire）
server/               # crate: mdm-server（axum HTTP 层）
├── lib.rs            # axum app 装配 + 静态站点兜底 + serve_router（完整 Router 服务）
├── auth.rs           # JWT 核心（OJ-4）：签验/匿名匹配/login/refresh 轮换/session（KV）
├── routes.rs         # directory-mirror URL → handler 映射
├── actor.rs          # JsActor：线程化执行、Send bridge 工厂
└── ws.rs             # WebSocket + js_route/mirror_routes（frame_loop 经 cached_transpile 读源码）
oj/                   # crate: oj（CLI 入口）
├── main.rs / lib.rs  # entry + CLI lib
├── args.rs           # CLI（clap derive：Cli/Commands + 到 ServerArgs/BuildArgs 的映射）
├── manifest.rs       # manifest.yaml 解析 + module/version 白名单 + manifests.yaml 锁读写
├── pack.rs           # 确定性 tgz 打包（mtime=0/mode 0644/排序 → 同输入同字节）
├── build_cmd.rs      # build 子命令：按模块版本目录构建（转译+minify/routes.js/锁/tgz）
└── server_cmd.rs     # server 子命令：start() + 模式自动判定 + release 聚合 + 插件装配
oj-plugin-ffi/        # crate: FFI 契约（宿主与插件唯一共享；repr(C) 类型 + ABI_VERSION）
plugins/             # 第一方插件统一目录（cdylib）
  oj-es/ oj-db-mysql/ oj-db-postgres/ oj-blob-s3/   # es/db/blob 轴
  oj-bus-kafka/ oj-bus-rabbitmq/ oj-kv-redis/       # bus/kv 轴
tools/xtask/          # crate: cargo xtask bin/plugin/build 构建 + 归置到 bin/
bin/                  # 编译产物目录（bin/oj + bin/plugins/<triple>/，CI 平台矩阵与本地 xtask 写入）
```

依赖分层：`bridge`（纯执行，不依赖 HTTP 框架）← `server`（axum 路由 + actor）← `cli`（装配）；
插件侧只依赖 `oj-plugin-ffi` + 各自后端 SDK，脱离宿主 workspace 可独立编译（spec §3）。
外部后端（mysql/postgres/s3/kafka/rabbitmq/redis）一律经插件提供，core 只留内置兜底
（sqlite/memory/local Bus/InMemoryKV）——core 依赖收敛见 §9。

## 2. 构建与测试

```bash
cargo build                                   # release（.cargo/config.toml 将 build 别名为 --profile release；禁止 debug）
cargo build --release                         # 等价 release
cargo test -p oj                              # 单测 + e2e
cargo test -p mdm-server                      # server 单测
cargo test --workspace -- --skip infinite_loop # 全部（infinite_loop 在部分平台 SIGSEGV，跳过）
cargo run -p oj -- server -c sample/config.yaml -d sample/src        # dev（按目录自动判定）
cargo run -p oj -- build -d sample/src -o sample/dist
cargo bench -p only-js                  # bridge 基准（**必须 release**）

# 插件（Task 4.1-4.4：外部后端全部 cdylib 化；产物归置 bin/plugins/<triple>/）
cargo xtask bin                                # 构建 oj（release）→ bin/oj
cargo xtask plugin es                          # 编译 oj-es（release）+ 拷入 bin/plugins/<triple>/
cargo xtask plugin db-mysql db-postgres        # db 轴
cargo xtask plugin blob-s3                     # blob 轴
cargo xtask plugin bus-kafka bus-rabbitmq      # bus 轴
cargo xtask plugin kv-redis                    # kv 轴
cargo xtask plugin <name> --check              # PluginLoader 预检（ABI/身份/semver/符号）
cargo xtask build                              # oj + 全部插件 → bin/

# 覆盖率（需 cargo-llvm-cov；deno_core V8 需用 llvm-cov 而非内置 --coverage）
cargo llvm-cov --workspace --summary-only

# 集成测试（默认 #[ignore]，需真服务；见 §7）
OJ_TEST_REDIS=redis://127.0.0.1:6379/1 cargo test --workspace -- --ignored
```

> 注：only-js 全套测试已修复为可跑：当前 `cargo test --workspace` 全绿，
> 合计 **206 通过 + 3 忽略**（细节见 §7）。only-js 覆盖率：**行 92.66% / 区域 91.39%**（>90%）。
> 曾有的 `infinite_loop_times_out_and_bridge_survives` SIGSEGV 已于 0bdfa86 修复（看门狗改用
> `v8::IsolateHandle`，见 §3）。另：`only-js` 测试二进制曾在 glibc 进程退出时 SIGSEGV
> （每 `Bridge` 各泄漏一个看门狗线程，退出时与 V8 平台析构相互干扰），已于 b79f697 修复——
> `KillSwitch` 析构时 stop 并 join 看门狗线程、清空残留的 isolate 句柄，测试结束无残留线程。

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

- `kv.rs`：`KVStore` trait（get/set/del/expire/incr）双实现。**内置兜底** `InMemoryKV`
  （`RwLock<HashMap>` + tokio `Instant` 惰性过期）；真 Redis 已迁 **oj-kv-redis 插件**
  （Task 4.4，见 §9 插件系统），`redis.default` 配置存在时经 FFI vtable connect（探活
  fail-fast）。`redis.*` 与 `kv.*` 同源，auth 会话也存同一 KV
  （`AUTH-SESSION:sha256(refresh_token)`），配真 Redis 即多实例共享会话。
- `bus.rs`：进程内主题广播（**local 内置**，Task 4.3 后 kafka/rabbitmq 迁插件）。
  `Bus { topics: Mutex<HashMap<String, Vec<UnboundedSender<String>>>> }`，
  `publish` try_send 广播 JSON 帧并清理 closed sender（返回接收方数），`subscribe` 去重注册。
  WS 会话的帧通道经 `ReqState.req.bus_tx` 注入（`RequestInfo.bus_tx`，ws.rs frame_loop 里
  `bus_tx→resp_tx` 转发任务与 `ws.send` 同一写出通道，保序）；HTTP 上下文 `bus_tx=None` →
  `op_bus_subscribe` 报错。server 装配共享**一个** `Arc<dyn EventBroker>`（server_cmd 注入
  Extras.bus——local 或插件 broker；FFI 版经全局 `DELIVER_TARGETS` 按 topic 扇出，跨
  actor 池/全部 WS 连接共享语义与内置 Bus 一致，Task 4.3 `ffi_broker_shared_across_bridges`
  回归）。
- `es.rs`：`EsBackend` trait（search/index_doc/delete_doc）+ 内置 `reqwest` 实现；
  **es 插件（oj-es）经 `FfiEsBackend` 适配同一 trait**。`url_for` 纯函数拼
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

## 5.1 证书驱动的 GET 限制（运行时校验）

生产可开启基于非对称加密（RSA-2048 + RS256 JWS）的证书校验，对过期证书做 GET 限流：

- **有效期内 / 未配置证书**：正常服务（`certificate_status = valid`）。
- **宽限期内（默认 30 天，可配 `grace_days`）**：所有 **GET** 请求返回 `403`，
  JSON 体 `{"error":"certificate expired","detail":"grace period: N days remaining"}`；其余方法正常。
- **宽限期结束后**：启动期 `from_config` 检测即 `ERROR` 并中止进程（`process::exit`）；
  运行中（热替换成过期证书）则 GET 持续 `403`（服务不中断，运维可替换证书恢复）。
- **热加载**：`notify` 监听公钥/证书文件变更（事件驱动，不轮询 mtime），原子更新
  `AppState` 内共享状态（`Arc<RwLock>`），重载失败保留旧状态。

### 配置（`config.yaml`）

```yaml
server:
  public_key_path: "./config/public_key.pem"   # SPKI PEM 公钥（仅验签，私钥不落服务器）
  certificate_path: "./config/certificate.jws" # JWS：Base64URL(Header).Base64URL(Payload).Base64URL(Signature)
  grace_days: 30                               # 默认 30；缩窄可加速告警
```

### CLI 覆盖

```sh
oj server -c config.yaml --cert-path ./config/certificate.jws --key-path ./config/public_key.pem --grace-days 15
```

### 证书格式（JWS）

```
Header : {"alg":"RS256","typ":"JWT"}        # alg 仅接受 RS256（拒绝 alg=none 降级）
Payload: {"nbf": <unix>, "exp": <unix>}     # nbf 生效、exp 过期（秒）
Signature: RSASSA-PKCS1-v1_5(SHA256, Header.Payload)  用私钥签名
```

### 监控

`GET {base}/health` 返回证书状态（即便进入宽限/过期仍可访问，便于探测）：

```json
{ "status": "OK", "certificate_status": "valid|grace|expired",
  "certificate_expiry": "2027-01-01T00:00:00Z", "grace_remaining_secs": 123456 }
```

（设计背景见 `docs/superpowers/specs/2026-08-26-certificate-design.md`；运维排障与证书生命周期见 `ops-manual.md` §3 / §4 / §7。）

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
- 单元测试随模块内联（`#[cfg(test)]`）；独立优先——内存后端（`InMemoryAccessor` /
  `InMemoryKV` / `SqlxAccessor::arc("sqlite::memory:")`）、临时目录、`httptest` 桩 ES/fetch、
  本地 `TcpListener` 桩，全程不依赖外部服务。
- 插件适配器测试（`bridge::ffi::adapter_tests`）：mock vtable（Rust 函数指针 + 预置 FfiFuture）
  验证 FfiXxxBackend 转发 + Drop close + 返回编码解码；共享静态用 `T_LOCK` 串行化并在测试
  开头清理（避免跨测试污染，见 bus `DELIVER_TARGETS` 经验）。
- 真服务集成测试 + 环境变量门控（**在插件 crate 内**，本地无服务时默认跳过）：
  - `OJ_TEST_ES=http://127.0.0.1:9200` → `oj-es`（vtable roundtrip）
  - `OJ_TEST_REDIS=redis://127.0.0.1:6379/1` → `oj-kv-redis`（vtable roundtrip）
  - `OJ_TEST_S3=endpoint|bucket|region|access|secret|path_style`
    → `oj-blob-s3`（vtable roundtrip）
  - `OJ_TEST_KAFKA_BROKERS=b1:9092,b2:9092` → `oj-bus-kafka`；`OJ_TEST_RABBITMQ_URL=…` →
    `oj-bus-rabbitmq`
  - 运行：`cargo test --workspace -- --skip infinite_loop`（env-gated 测试未设 env 即内联跳过）。
- 当前计数（`--skip infinite_loop` 全绿）：`only-js` lib **150** + bin **2**、
  `mdm-server` **52**、`oj` lib **59** + bin **3**、`e2e` **15**（E2E_LOCK 串行锁避免端口/文件冲突）；
  插件 crate：es **3**、db-mysql/postgres **各 2**、blob-s3 **3**、bus-kafka/rabbitmq **各 2**、
  kv-redis **2**、ffi 契约 **1 + 入口测试 2**、mini 夹具 **1**。
  覆盖率：**行 >90% / 区域 >90%**（`cargo llvm-cov --workspace --summary-only`）。

## 8. 已知设计权衡（v0.1 终审裁决）

见 spec `docs/superpowers/specs/2026-08-22-oj-server-sample-design.md` §8 的 D1–D4：
- 相对 `require()` 不支持——v0.1 已知限制（db 仅 sqlite 已于 v0.2 解除：多库 DSN 按 scheme
  分发，设计见 `2026-08-24-oj-p2-design.md` §1）。外部后端（mysql/postgres/s3/kafka/rabbitmq/
  redis）已于插件系统阶段（2026-08-25）全部 cdylib 化，见 §9 与
  `docs/plugin-development.md`。
- `build` 已于 2026-08-24 实现（按模块版本目录 + 产物保留原名原结构 + 默认 minify +
  manifests.yaml 锁 + 确定性 tgz + release 聚合），设计见
  `docs/superpowers/specs/2026-08-23-oj-build-design.md`（顶部有去 hash 修订注记）。

## 9. 插件系统（阶段 4 全量 cdylib 化）

**分层**：开发侧五轴解耦（es/db/blob/bus/kv 各自 trait + 注册表），运行侧动态链接库可配置
装配。全部 FFI 跨界类型收在 `oj-plugin-ffi`（spec §3）；`src/bridge/ffi.rs` 收敛全部
unsafe（`load_forget` dlopen + `Box::leak` 进程期存活，任何路径不 dlclose；插件必须
panic=unwind profile）。适配器层 `FfiXxxBackend` 把插件 vtable 包装成 core trait 供 op
消费（构造放 core，装配层只经安全入口）。

**装配语义**（`plugin_loader.rs`，spec §5）：
- 路径四级解析：`OJ_PLUGINS_DIR` > `config plugins_dir` > `<exe>/plugins`（bin/oj 旁即 `bin/plugins`）
  > `<workspace_root>/bin/plugins`（与 xtask 产物归置同形），相对路径相对 config 目录，
  最终目录 = `<plugins_dir>/<host-triple>/`。
- 双模式：`plugins:` 清单显式给出 → 严格按名装配（缺文件/身份不符/`@semver` pin 不符 fail
  fast）；缺省 → 扫描目录全部加载（目录不存在/为空 = 零插件，仅内置后端）。
- 门禁：`ABI_VERSION` 严格相等唯一硬门禁；指纹不符仅告警。`op_plugins` 输出
  插件名/semver/ABI/指纹 + 宿主 ABI_VERSION（升级核对窗口）。

**五轴接线**（server_cmd `build_registries`）：
- es 键选单后端；「cfg es 声明但无 es 插件」→ fail fast。
- db 认领式注册表：内置 sqlite/memory 打底 + 插件 db 工厂（scheme 交集冲突 fail fast；
  未知 scheme 明确报错）。
- blob 键选单 vtable 槽：多 blob 插件冲突 fail fast；driver != local 且无插件 → fail fast。
- bus 键选注册表：内置 local + 插件 kafka/rabbitmq（kind 冲突 fail fast；声明 kind 但无插件
  → "unknown broker kind"）。FFI broker 经全局 `DELIVER_TARGETS` 按 topic 扇出，跨 actor/WS
  共享语义与内置 Bus 一致。
- kv 键选单 vtable 槽：redis.default 声明 → 经 oj-kv-redis connect（探活 fail-fast）；
  未声明 → InMemoryKV 内置兜底。

**升级回滚**（部署侧）：插件替换用 `.new`/`.bak` 原子换名；`cargo xtask plugin --check` 预检；
ABI bump 部署顺序 = 先升插件到新 ABI 并验证，再升宿主（或同版本原子升级）。平台矩阵与
`bin/plugins/<triple>/` 布局见 `.github/workflows/plugin-matrix.yml`。

**第三方插件**：见 `docs/plugin-development.md`（FFI 契约、ABI 纪律、入口宏、panic 归因）。
