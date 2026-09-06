# 开发手册（Developer Guide）

`only-js`（代号 **oj**）—— 基于 Rust + `deno_core` 的低代码后端框架，将 JS/TS 运行时（V8）
嵌入 Rust。业务逻辑以 JS/TS「handler」编写，使用注入的全局对象（`json` / `db` / `http` /
`kv` / `blob` / `bus` / `es` / `fetch` / `log` / `ws` / `plugins` / `cert` / `jwt` /
`bcrypt` / `crypto` / `finish`），Rust 侧捕获统一的 `{code,msg,data}` 信封，由 HTTP 服务写回。
数据库、KV、对象存储、事件总线、ES 等后端能力在启动时作为 **cdylib 插件** 通过 C-ABI FFI
契约（`oj-plugin-ffi`，ABI 7）加载。

本文合并了原日常开发手册与 `oj server` 内部实现走读两份文档：既覆盖**日常开发**
（环境、构建、写 handler、Rust 侧嵌入 API、加 op、测试、调试），也覆盖**内部实现**
（执行模型、关键模块深读、安全模型、设计权衡）。JS 全局对象完整参考见
[devkit/api-manual.md](devkit/api-manual.md)（类型权威 `global.d.ts`），插件开发另见
[plugin-development.md](plugin-development.md)，部署运维见 [ops-manual.md](ops-manual.md)，
性能数据见 [benchmarks.md](benchmarks.md)，OIDC 实现走读与接入手册见
[oidc-implementation.md](oidc-implementation.md) / [oidc-integration.md](oidc-integration.md)。

---

## 1. 环境与构建

- Rust toolchain（edition 2024）。`deno_core 0.410` 依赖 `rusty_v8`，首次编译需下载预编译 V8
  静态库（网络受限时设 `V8_FROM_SOURCE=0` 让它走预编译包；**切勿**从源码编译 V8）。
- 关键依赖（根 `Cargo.toml`）：
  - `deno_core 0.410` / `deno_error 0.7` / `deno_ast 0.53`（TS 转译）。
  - `sqlx 0.9`（any + sqlite + runtime-tokio）、`sea-query 1.0`（安全 SQL 构造器）。
  - `reqwest 0.13`（rustls）、`tokio-tungstenite 0.30`、`notify 8`、`object_store 0.14`（aws）。
  - `libloading 0.9`（插件 dlopen）。

**禁止 debug 构建**：`.cargo/config.toml` 无法用 alias 覆盖内建 `build`，故 `cargo build`
靠**约定**等价于 `--profile release`——所有脚本/CI/工具一律 `--release`；debug 的
`rusty_v8` 静态库不可用。日常只用 `cargo build --release`（或 `cargo xtask build`）。

### 常用命令

```bash
cargo build --release        # 发布构建（等价 cargo build，按约定）
cargo fmt --check            # 格式门禁（cargo fmt 自动修复）
cargo clippy --all-targets -D warnings   # lint 门禁
cargo test --release --workspace        # 全部测试（根 crate + oj e2e + 插件；CI 同款。
                                        #   个别平台 SIGSEGV 时才按 workflow 的
                                        #   skip_infinite_loop 开关跳过，不再全局跳过）
cargo test -p oj             # 单测 + e2e
cargo test -p mdm-server     # server 单测
cargo test -- --nocapture    # 看 tracing 输出
cargo build --benches        # 编译 criterion 基准（不跑）
cargo bench                  # 跑基准（benches/bridge.rs，**必须 release**）
cargo llvm-cov --workspace --summary-only   # 覆盖率（需 cargo-llvm-cov；V8 需 llvm-cov）

cargo run -p oj -- server -c sample/config.yaml --api-path sample/src   # 启动服务（模式自动判定）
cargo run -p oj -- build -d sample/src -o sample/dist                   # 构建模块产物
cargo run -p oj -- test -c sample/config.yaml --format human            # 进程内 *.test.ts 运行器
cargo run -p oj -- migrate / fixture / schema diff   # 迁移 / 演示数据 / schema 对账
# 子命令全表见 oj/src/args.rs 与 docs/cli2.md

# 真服务集成测试（默认 #[ignore]，env 门控，见 §12）
OJ_TEST_REDIS=redis://127.0.0.1:6379/1 cargo test --release --workspace -- --ignored
```

### oj CLI 与 xtask

```bash
cargo xtask bin                    # 构建 oj（release）并拷入 bin/oj
cargo xtask plugin <name>          # 构建 oj-<name>（release）并拷入 bin/plugins/<host-triple>/
cargo xtask plugin <name> --check  # 预检（ABI / 身份 / semver / 符号）
cargo xtask build                  # 构建 oj + 全部第一方插件（release），统一归置 bin/
```

所有编译产物归置到 `bin/`：`bin/oj` 与 `bin/plugins/<host-triple>/`（发行布局与插件加载器
默认发现路径同形）。

---

## 2. 项目结构（workspace 布局）

```
Cargo.toml            # [workspace] members = ["server", "oj", "oj-plugin-ffi", "plugins/*", "tools/xtask"]
src/                  # crate: only-js（lib）——核心执行层（纯 lib，无 bin、无 build.rs）
├── lib.rs            # 导出 bridge + config
├── config.rs         # 配置加载：server{host,port,base,root,timeout,pool_size} + db/redis/blob/es/broker/plugins 映射
└── bridge/           # JS 运行时与 SDK（无 axum/http 依赖，纯执行层）
    ├── mod.rs        # Bridge / StableState / ReqState / Capture + extension! 注册全部 op
    ├── bootstrap.js  # JS 全局对象装配（ESM，必须 7-bit ASCII）
    ├── runtime.rs    # JsRuntime 生命周期 + RuntimePool（max_idle=16）+ KillSwitch 超时熔断
    ├── loader.rs     # HandlerStore（嵌入 map / FS 目录，嵌入与测试场景用）
    ├── module_loader.rs # ModuleLoader trait 实现：import 解析、CJS 包装、ensure_within
    ├── transpile.rs  # deno_ast TS→JS 类型剥离 + mtime-keyed TranspileCache
    ├── registry.rs   # deno_core op 注册
    ├── http.rs       # RequestInfo{method,params,query,headers,body}、export_bytes
    ├── envelope.rs   # {code,msg,data} 信封、HTTP 状态映射
    ├── json.rs       # json.ok/fail/header ops
    ├── db.rs / query.rs / accessor_sqlx.rs   # 数据访问 + 安全查询构造器（SchemaRegistry 白名单）
    ├── db_backend.rs # DbBackend 注册表（内置 sqlite/memory + 插件 db 工厂；scheme 认领）
    ├── kv.rs         # KVStore trait + 内置 InMemoryKV 兜底（真 Redis 迁插件）
    ├── bus.rs        # 订阅发布总线：内置 local Bus + EventBroker trait（插件 kafka/rabbitmq）
    ├── es.rs         # EsBackend trait + 内置 reqwest 实现（oj-es 插件经 FfiEsBackend 适配）
    ├── blob.rs       # BlobBackend trait + LocalBlob 内置（s3 迁插件）
    ├── cert.rs / crypto.rs / auth.rs / guard.rs   # cert 全局、jwt/bcrypt/crypto ops、鉴权
    ├── fetch.rs / log.rs / ws.rs / inspector.rs   # fetch op、结构化日志、WS、DevTools 桥
    ├── ffi.rs        # 全部 unsafe 收敛（load_forget dlopen）+ FfiXxxBackend 适配器层
    └── plugin_loader.rs # PluginLoader：四级路径解析 + 清单/扫描双模式 + ABI 门禁 + AXES 逐轴 dlsym
oj/                   # CLI 二进制：server / build / test / migrate / fixture / schema
├── main.rs / lib.rs  # entry + CLI lib
├── args.rs           # CLI（clap derive：Cli/Commands + 到 ServerArgs/BuildArgs 的映射）
├── manifest.rs       # manifest.yaml 解析 + module/version 白名单 + manifests.yaml 锁读写
├── pack.rs           # 确定性 tgz 打包（mtime=0/mode 0644/排序 → 同输入同字节）
├── build_cmd.rs      # build 子命令：按模块版本目录构建（转译+minify/routes.js/锁/tgz）
├── server_cmd.rs     # server 子命令：start() + 模式自动判定 + release 聚合 + 插件装配
└── tests/e2e.rs      # 端到端验收（UC-1…15）
server/               # crate: mdm-server（axum HTTP 层）
├── lib.rs            # axum app 装配 + 前置管线 + 静态站点兜底 + serve_router
├── auth.rs           # JWT 核心：Claims 签验、匿名匹配、session（KV）
├── routes.rs         # directory-mirror URL → handler 映射
├── actor.rs          # JsActor：线程化执行、Send bridge 工厂
├── certificate.rs    # 证书验签与状态判定（valid/grace/expired）
└── ws.rs             # WebSocket + js_route/mirror_routes
oj-plugin-ffi/        # crate: FFI 契约（宿主与插件唯一共享；repr(C) 类型 + ABI_VERSION=7）
plugins/              # 8 个 cdylib 插件：oj-es、oj-db-mysql、oj-db-postgres、oj-blob-s3、
                      #   oj-bus-kafka、oj-bus-rabbitmq、oj-kv-redis、oj-auth
tools/xtask/          # crate: cargo xtask bin/plugin/build 构建 + 归置到 bin/
tools/oj-cert/        # 证书生成小工具 crate（gen/renew，sign_jws 单一事实来源）
tests/plugins/mini/   # 演练加载/ABI/panic 路径的 cdylib 测试夹具
sample/               # 示例应用（config.yaml + src/ + dist/ + tests/ + global.d.ts）
bin/                  # 编译产物目录（bin/oj + bin/plugins/<triple>/，不入库）
benches/bridge.rs     # criterion 基准
```

依赖分层：`bridge`（纯执行，不依赖 HTTP 框架）← `server`（axum 路由 + actor）← `oj`（CLI 装配）。
插件侧只依赖 `oj-plugin-ffi` + 各自后端 SDK，脱离宿主 workspace 可独立编译。
外部后端（mysql/postgres/s3/kafka/rabbitmq/redis）一律经插件提供，core 只留内置兜底
（sqlite/memory/local Bus/InMemoryKV）。

---

## 3. 执行模型（数据流）

```
HTTP 请求
  └─ server/lib.rs handle：依次 路由表 lookup（matchit）→ dev 目录镜像兜底（routes.rs）
     → 静态站点（server.app_path，仅 GET/HEAD）→ 404
       └─ 命中 api 文件 → 交给 JsActor
            └─ server/actor.rs：线程化执行（Send bridge 工厂），池化 JsRuntime
                 └─ bridge：driver 模块 file:///oj/driver/{N}.js（AtomicU64 递增）
                      const m = await import(spec);   // side module 触发 TLA
                      const fn = m.default?.[method];
                      if (typeof fn !== "function") json.fail(405, msg);
                      else await fn();
                      └─ bridge/transpile.rs：TS→JS 类型剥离（mtime-keyed 缓存）
                           └─ bridge/module_loader.rs：import 解析（相对/裸/CJS）
                                └─ deno_core JsRuntime：mod_evaluate + run_event_loop
```

关键点：

- **一个 JsRuntime 一个 main module**，driver 不能是 main；所有用户模块走 side module，
  driver 内 `await import(spec)` 触发顶层 await。
- **两级缓存**：`TranspileCache`（path→(mtime, JS)）+ V8 module cache（靠 mtime 版本化
  specifier `?v=<mtime-nanos>` 实现「改文件即失效」）。
- **KillSwitch**：超时用 `v8::IsolateHandle::terminate_execution` 强杀（`checkout_armed` 取
  线程安全句柄——**不是**裸指针：`OwnedIsolate` 包装地址 ≠ 真实 isolate 指针，手转裸指针会
  SIGSEGV）；被杀的 runtime 直接丢弃（不回池），HTTP 返回 408。这是对 `while(true)` 等
  死循环的唯一可靠熔断手段。

---

## 4. 写 handler（JS/TS）

handler 是 ESM 源码（dev 模式 `.ts` 按需转译，release 模式服务 `oj build` 产物 `.js`）。
**目录镜像路由**：`src/user/profile/detail/api.ts` → `/v1/api/user/profile/detail/`；
`api.ts` 导出 `get`/`post`/`put`/`del`/`patch`/`head`/`options`（HTTP 方法同名，`DELETE`→
`del`）；可选 `get.route = "{id}"` 声明路径参数（matchit 语法，挂载后**替换**目录镜像路由；
参数段不得混字面，`{*path}` 至少匹配一段）。handler **必须调用一次** `json.ok` / `json.fail` /
`finish` 才能完成会话；顶层可直接 `await`（event loop 由 driver 泵至 Promise 落定）。

同目录可放 `WS.ts` 产生一条 WebSocket 路由：连接升级后**客户端每个文本帧执行一次本文件**，
帧内 `json.ok` 正常回信封；`bus.subscribe` 只在 WS 帧内有意义。

JS 全局对象速查（以 `src/bridge/bootstrap.js` 挂载为准；完整签名以
[devkit/api-manual.md](devkit/api-manual.md) 与 `sample/global.d.ts` 为权威）：

| 全局 | 用途 | 关键点 |
|---|---|---|
| `json.ok(data)` / `json.fail(code,msg,data?)` / `json.header(n,v)` / `json.raw(data)` | 信封与响应头；`raw` = 裸 JSON 200（无信封，对外标准协议端点用） | `code<=0` 映射 500；HTTP 状态 = `code` |
| `db` / `DB(name)` | 数据访问；`db === DB("default")` | 未配置的名字返回 `undefined` |
| `db.query / exec / table / tx` | 原始 SQL / 安全构造器 / 事务 | 标识符走白名单、值参数化；`tx` 回调式，resolve 提交 throw 回滚 |
| `http.method/params/query/headers/body/tenantId/user/files` | 只读请求上下文（懒 Proxy） | `param(name, def?)` 路径优先、query 兜底 |
| `kv` / `redis` | KV：`get/set/del/expire/incr` | 同源同面；oj-kv-redis 真连，未配回落内存 KV |
| `blob(name?)` | 对象存储：`put/get/del/url/contentType` | `blob:` 段启用；下载走 `{base}/blob/{key}` |
| `bus.publish/subscribe/kind` | 事件总线 | HTTP 发布、WS 订阅；`kind()` 是异步 op |
| `es.search/index/del` | Elasticsearch 薄客户端 | `es:` 段启用，未配置报错 |
| `fetch(url, opts?)` | 浏览器兼容 Fetch（reqwest） | 响应整体缓冲；不支持 AbortController |
| `log.debug/info/warn/error(msg, ...kv)` | 结构化日志（tracing） | 交替键值对 |
| `cert.generate/renew` | JWS 证书签发/续期（Rust RSA） | 纯内存，不落盘 |
| `jwt.sign/verify` + `jwt.accessDuration/refreshDuration` | JWT 签发验签 | 密钥/时长装配期注入；claims 固定 `{sub,roles,iat,exp}` |
| `bcrypt.hash/verify` | 密码哈希（`spawn_blocking`） | 不依赖 `auth:` 段 |
| `oidc.sign/verify/jwks` + `oidc.issuer/rp/clients` | RS256 JWS 原语 + 装配期配置 | `oidc:` 段启用；私钥留在 Rust（`src/bridge/oidc.rs`） |
| `crypto.sha256Hex / randomHex` | 摘要与随机数 | 与原生 `getRandomValues` 合并 |
| `ws.send/close` | WS 帧循环控制 | 仅 WS 连接内有意义 |
| `plugins()` | 已装配插件自省 | 同源 `GET {base}/plugins` |
| `finish()` | 标记会话完成但不写响应 | 少用 |

另有 `__ojRequire(name, referrerPath)`（CJS 互操作，模块加载器内部使用，业务代码不直接调）。

### 查询构造器（`db.table(...)`）

`db.table(name)` 返回构造器：`select(cols)`、`where(cond)`（可链多次，多个 where 之间 AND）、
`orderBy([{field,dir}])`、`limit(n)`（默认 100、硬上限 1000）、`offset(n)`、`all()`。
条件 `{field, op, value}`：`op` ∈ `eq/ne/gt/gte/lt/lte/in/like/isNull`（`in` 的 value 为数组，
`isNull` 不需 value）。**白名单**：`table` 与 `field` 必须先在 `SchemaRegistry` 声明，否则报
`unknown table/column`；排序列须 `is_sortable`。

```js
await db.table("order")
  .select(["id", "amount"])
  .where({ field: "user_id", op: "eq", value: 1 })
  .where({ field: "amount", op: "gte", value: 100 })
  .orderBy([{ field: "amount", dir: "desc" }])
  .limit(50).all();
```

### SQL 注入红线（必须读）

- **标识符**（表名/列名）**绝不**来自 JS 字符串拼接——只能来自 `SchemaRegistry` 白名单
  （经 `db.table(...)` 构造器）。
- **值**通过绑定参数传递：`db.query("... where id = $1", [id])` 或构造器的 `value`。
- 占位符风格随底层驱动而定：`$1`（Postgres）/ `?`（MySQL、SQLite）。

---

## 5. 在 Rust 侧使用 Bridge（嵌入 / 测试）

`Bridge` 是核心执行入口：oj server 用它跑 handler；单测与嵌入场景直接构造。公开 API
（`src/bridge/mod.rs`）：

```rust
use std::sync::Arc;
use only_js::bridge::{Bridge, InMemoryAccessor, InMemoryKV, RequestInfo, SchemaRegistry};

// 单 db：注册为 dbs["default"]（等价 DB("default")）。
let db = Arc::new(InMemoryAccessor::new());
db.seed([serde_json::json!({"id": 1, "name": "ever", "age": 18})]);
let registry = SchemaRegistry::new().table("user", Some("id"), &["id", "name", "age"]);
let b = Bridge::with_opts(db, Arc::new(InMemoryKV::new()), registry, false);

// 注入请求上下文执行；run(src) 等价 run_with(src, RequestInfo::default())。
let cap = b.run_with(r#"
    db.table("user").select(["id","name"]).where({field:"age",op:"gte",value:18})
      .all().then((rows) => json.ok({ users: rows }))
      .catch((e) => json.fail(500, String(e)));
"#, RequestInfo { method: "GET".into(), ..Default::default() }).await?;

println!("status={} body={}", cap.status, String::from_utf8_lossy(&cap.body));
```

构造族（依赖倒置，传接口而非实现）：

- `Bridge::new(db, kv)` —— 便捷形式：空注册表、`inspect=false`。
- `Bridge::with_opts(db, kv, registry, inspect)` —— 单 db。
- `Bridge::with_dbs(dbs, kv, registry, inspect)` —— **全量命名 DB 构造期注入**（无
  `"default"` 键时取第一个补位）。
- `Bridge::with_dbs_and_loader(..., loader, extras)` —— oj server 专用：模块加载器 +
  `Extras`（blob/es/bus/plugins/modules 等可选能力）。

执行族：`run` / `run_with` / `run_with_timeout`（超时返回 `RunError::Timeout`）/
`run_named`（按 HandlerStore 名执行）/ `run_module`（按模块路径执行，oj server 主路径）/
`run_ws`（WS 帧）/ `prewarm`。返回 `Capture { status, headers, body }`。

**状态模型（重要）**：

- `StableState`（`Arc`，跨请求共享）：`kv` / `dbs` / `client` / `registry` / `blobs` / `bus`
  / `es` / `modules` 等。**一经 runtime 池共享即不可变**——命名 DB / blob / es / 模块上下文
  都必须在**构造期**传入（早期版本的 `set_db_accessors` 已删除：池化后 `Arc::get_mut`
  必 panic）。
- `ReqState`（每请求，存 `OpState`）：`req` / 事务句柄 / 响应捕获。每次借出 runtime 时
  整体 `reset(req)`。
- 两者分离使 `JsRuntime` 可池化复用（`RuntimePool`）。**切勿**把跨请求可变的共享状态塞进
  `ReqState`。

`HandlerStore`（`loader.rs`）仍服务于**嵌入与测试场景**：`from_embedded(map)`（编译期嵌入，
配 `set_handlers` + `run_named`）与 `MDM_HANDLER_DIR` 环境变量（FS 目录 + notify 监听）；
oj server 的 handler 加载走 `module_loader.rs` + `run_module`（见 §7）。

---

## 6. 接入真实数据库（SqlxAccessor）

`SqlxAccessor` 实现了 `DataAccessor`，以 `sqlx::any::Pool<Any>` 驱动无关接入
MySQL/PostgreSQL/SQLite（生产部署经 **oj-db-mysql / oj-db-postgres 插件**接入；直接用
`SqlxAccessor` 属嵌入/测试场景）。驱动安装（`install_default_drivers`）内聚于 `connect`。

```rust
use only_js::bridge::SqlxAccessor;

let db = SqlxAccessor::arc("postgres://user:pass@localhost/oj").await?; // Arc<dyn DataAccessor>
let db2 = SqlxAccessor::connect("sqlite:///tmp/oj.db").await?;          // Self，再自行 Arc
```

- 占位符风格见 §4。构造器经 sea-query 的方言 `QueryBuilder` 生成驱动原生 SQL；裸 SQL 直连
  非 PG 驱动时注意 `$N` 与 `?` 的差异。

---

## 7. 模块加载与热重载（oj server）

现行加载链路在 `module_loader.rs` + `transpile.rs`：

- **ESM/CJS 双支持**：handler 与项目内模块按 ESM 解析；CJS 依赖（node_modules）经
  `__ojRequire` 同步互操作。
- **版本化缓存**：模块 specifier 带 `?v=<mtime-nanos>`，文件变更即自然失效缓存，无需清理
  模块图。
- **TS 转译**：dev 模式 `deno_ast` 剥类型按需转译，结果按 mtime 全局缓存，可选 minify。
- **热重载**：oj server dev 模式用 `notify` 监听源码树，变更后按上述 mtime 版本化天然生效。
- **release 模式**：服务 `oj build` 产出的 `dist/`（预转译 JS + `routes.js` +
  `manifests.yaml` 版本锁），不转译。

模式自动判定（`oj/src/server_cmd.rs` 的 `is_release`）：目录含 `dist/manifests.yaml` ⇒
release，否则 dev。命令与构建产物见 `cargo run -p oj -- --help` 与 [cli2.md](cli2.md)。

### import 解析细节（module_loader.rs）

- `resolve_inner`：已是绝对 `file://` URL 直接返回（**不再 `ensure_within`**，这是信任模型：
  内部生成的 specifier 可信，外部请求体/路径才需钳制）。
- `resolve_relative`：`./` `../` + 补全 `.ts`→`.js`→`/index.ts`→`/index.js` + 词法归一化 `..`。
- `resolve_bare`：裸 specifier 从当前文件目录逐级向上找 `node_modules/<pkg>`（至 project
  root），按 `package.json` `module`→`main`→`index.js` 取入口，支持 `@scope/name` 与子路径。
- `wrap_cjs` / `looks_cjs` / `op_resolve_cjs`：CJS 互操作（`module.exports`→`default`，
  `require` 走 `__ojRequire`，进程级缓存）。启发式识别，**仅裸 specifier**；相对
  `require("./x")` 是已知限制。
- `ensure_within`：两侧都 `canonicalize` 后做前缀判断，拒绝逃逸；macOS `/var` vs
  `/private/var` 的符号链接差异已处理。

另有运行时扩展点 **`ext_boot.js`**（详见 §8.3）。

---

## 8. 关键模块职责（深读）

### 8.1 http.rs / envelope.rs / blob.rs —— 请求、响应与对象存储

- `RequestInfo { method, params, query, headers, body, tenant_id, user, files }`（`params` 在
  目录镜像路由下恒空）。multipart 时 `files: Vec<UploadedFile{field,filename,content_type,bytes}>`，
  文本字段并入 `body`（`{name: value}`）；`op_http_file` 按索引取字节（async + `#[buffer]`
  返回，sync buffer-return 在 fast-call 路径会卡死）。
- `export_bytes`：空 body → `Value::Null`；可 JSON 解析 → 解析后的 `Value`；否则 UTF-8 字符串。
- 信封 `{code,msg,data}`：`code<=0` → 500；HTTP 状态 = `code`（`code>0`）。
- `blob.rs`：`BlobBackend` 统一契约（put/get/del/url/content_type/serve），local/s3 双驱动可
  替换。local 用 object_store `LocalFileSystem` + `<key>.ct` sidecar 持久化 Content-Type；
  s3 用 `AmazonS3`（具体类型才能拿 `Signer` presign）+ GET 15min → `serve` 返回 302。
  `valid_key` 逐段白名单（`.`/`..`/`\`/NUL/空段拒绝）；下载路由 `decode_blob_key` 先
  percent-decode 再校验。`Extras { blob, bus, es }` 是 bridge 构造期扩展点终态（构造期注入，
  `StableState` 内不可变 Arc）。

### 8.2 bootstrap.js —— JS SDK globals 装配

`json.ok` 在 JS 侧 `JSON.stringify` 后交 Rust 拼接信封，省一次 serde_v8 反序列化；
`http` 是**每请求惰性 Proxy**（`op_http_info()`），`db === DB("default")` 由 JS 侧 `dbCache`
Map 保证同源。

事务（db.tx）：活跃事务存 `ReqState.tx`（`Arc<ActiveTx>`，故 ReqState 不再 Clone），
query/exec/query_build 按 `resolve_target` 路由（本库 tx 会话 / 他库报错 / 无 tx 走池）；
`Bridge::finalize_tx` 在三条成功路径 checkin 前保底回滚未完结事务。

前置管线：`server::Pipeline` 是 handle() 进 JS 前的单一扩展点（租户/鉴权/blob 已接入，后续
只加字段不改编构）；提取/守卫逻辑在 run 闭包的 async 块开头，失败走
`fail_response(400/401, …)` 信封。鉴权端点（login/refresh/logout）是普通业务路由而非内置
路由；blob 下载是内置公开路由（auth 之后、路由表 lookup 之前，免鉴权）。
`max_upload` 双闸：axum `DefaultBodyLimit::max(2x)` 兜 2x 外裸 413，handle() 内
`body.len() > max_upload` 出信封 413。

角色鉴权（handler 内按 `http.user.roles` 自行判定）是刻意不加框架层的——路由级 RBAC 等
真需求出现再议（YAGNI）。

#### ext_boot.js —— bootstrap 的运行时补充

`bootstrap.js` 编译期嵌入，改它要重编二进制。`ext_boot.js` 是**运行时**补充：装配期探测
`<config_dir>/ext_boot.js`，冻结 `versioned_specifier`（`file://…?v=<mtime>`）存进
`StableState.boot`；`RuntimePool::checkout` 内每个**新建**的 runtime 加载执行一次
（`boot_runtime`：side module + `mod_evaluate` + `run_event_loop`，顺序同 `run_side_driver`）。
用户侧用法见 [devkit/api-manual.md](devkit/api-manual.md) §6 末；设计与评审依据见
`docs/superpowers/specs/2026-09-02-ext-boot-design.md`。

实现时必须守住的不变量（每条都对应一个已踩过或已论证的坑）：

- **boot 只在 `checkout` 一处调用**——它是唯一的借出入口，「借出的 runtime 一定已 boot」
  才有单点保证。`oj test` 不走池（直接 `JsRuntime::new`），故单独补调一次。
- **运行期 boot 失败返回 `RunError`，绝不 panic**。actor 线程在 `block_on` 里 panic 会
  永久杀死该 worker，pool_size 个 actor 会被逐个耗尽。
- **启动期必须显式 `prewarm`**（`App::from_config`，建表之前）。否则 boot 错误只能借
  dev 内省间接暴露，而 `bridge_introspector` 会把线程 panic 吞成路由 failure，装配层只
  warn 不致命 → 「路由全空、服务照常监听」。
- **boot 期 arm 看门狗**（`BOOT_TIMEOUT`，2s）。同步死循环不归还执行器，
  `tokio::time::timeout` 无效，只能靠 `terminate_execution`。且 `fired` 必须先于
  `result` 判定，否则超时被 `Core` 分支抢先、误报 500 而非 408。
- **失败的 runtime 兜底跑一轮 `run_event_loop` 再丢弃**——未轮询完 event loop 的 isolate
  析构会触发 V8 句柄错误（本项目有 SIGSEGV 前科，见 `runtime.rs` 头注释）。
- **`idle.borrow_mut()` 的 `RefMut` 必须在 await 前 drop**。别指望 lint：
  `src/lib.rs` 是 `#![allow(clippy::all)]`。
- **boot 拿不到 `ext:` 模块**：deno_core 在 loader resolve 之后还有
  `validate_ext_module_import` 一道闸（`file://` referrer 永远过不去）。别去放行
  `resolve_inner` 的 `ext` scheme——那对全部 handler 生效，且放行也无效。

### 8.3 kv.rs / bus.rs / es.rs —— 外部状态与广播

- `kv.rs`：`KVStore` trait（get/set/del/expire/incr）双实现。**内置兜底** `InMemoryKV`
  （`RwLock<HashMap>` + tokio `Instant` 惰性过期）；真 Redis 已迁 **oj-kv-redis 插件**，
  `redis.default` 配置存在时经 FFI vtable connect（探活 fail-fast）。`redis.*` 与 `kv.*`
  同源，auth 会话也存同一 KV（`AUTH-SESSION:sha256(refresh_token)`），配真 Redis 即多实例
  共享会话。
- `bus.rs`：进程内主题广播（**local 内置**，kafka/rabbitmq 迁插件）。
  `Bus { topics: Mutex<HashMap<String, Vec<UnboundedSender<String>>>> }`，
  `publish` try_send 广播 JSON 帧并清理 closed sender（返回接收方数），`subscribe` 去重注册。
  WS 会话的帧通道经 `ReqState.req.bus_tx` 注入（`RequestInfo.bus_tx`，ws.rs frame_loop 里
  `bus_tx→resp_tx` 转发任务与 `ws.send` 同一写出通道，保序）；HTTP 上下文 `bus_tx=None` →
  `op_bus_subscribe` 报错。server 装配共享**一个** `Arc<dyn EventBroker>`（server_cmd 注入
  Extras.bus——local 或插件 broker；FFI 版经全局 `DELIVER_TARGETS` 按 topic 扇出，跨
  actor 池/全部 WS 连接共享语义与内置 Bus 一致，`ffi_broker_shared_across_bridges` 回归）。
- `es.rs`：`EsBackend` trait（search/index_doc/delete_doc）+ 内置 `reqwest` 实现；
  es 插件（oj-es）经 `FfiEsBackend` 适配同一 trait。`url_for` 纯函数拼
  `/{index}/_search` 或 `/{index}/_doc/{id}?refresh=true`（endpoint 尾斜杠幂等剪除）。
  index/id 白名单 `[a-zA-Z0-9_-]+` 防路径注入；响应直通（非 2xx 带 ES 返回体）；未配置 →
  `es not configured`。真连 roundtrip 用 `OJ_TEST_ES` 环境变量驱动（`#[ignore]`）。

---

## 9. DevTools 调试（inspector）

- 构造时开开关：`Bridge::with_opts(db, kv, registry, true)`（或 `with_dbs*` 传 `true`）。
- 起服务：`only_js::bridge::start_inspector(&b, "127.0.0.1:9229".parse()?).await`——借一个
  runtime 取其 inspector 句柄并起 WS（`inspector.rs`）。未开开关则只 warn 不生效。
- 浏览器 `chrome://inspect` → 配置 `127.0.0.1:9229` → 断点/单步/看 console。
- inspector 是 `!Send`，WS 服务跑在 `spawn_local`（current_thread runtime）。
- **仅开发用**：生产构建不要开。

---

## 10. 加一个新的 op（扩展 JS SDK）

扩展点在 `src/bridge/mod.rs` 的 `deno_core::extension!` 宏与 `bootstrap.js`。

1. **写 op**（如 `src/bridge/foo.rs`）：
   - 同步 op：`fn op_foo(state: &mut OpState, #[string] x: String)`。
   - 异步 op：`async fn op_foo(state: Rc<RefCell<OpState>>, ...) -> Result<T, JsErrorBox>`。
   - 读共享状态：`state.borrow().borrow::<Arc<StableState>>()`；
     写每请求状态：`state.borrow_mut::<ReqState>()`。
   - 错误用 `deno_error::JsErrorBox::generic(msg)`，会抛给 JS 的 `catch`。
2. **注册**：在 `mod.rs` 的 `extension! { ops = [ ... foo::op_foo ] }` 加入，并加 `mod foo;`。
3. **装配 JS 侧**：在 `bootstrap.js` 顶部 `import { op_foo } from "ext:core/ops";`，
   并挂到某个全局对象。
4. **测试**：在对应模块的 `#[cfg(test)]` 加 `#[tokio::test(flavor = "current_thread")]`，
   用 `Bridge::new(...).run(...)` 验证 JS 全链路。

> `bootstrap.js` 必须保持 **7-bit ASCII**：deno_core 的 ESM 扩展要求，非 ASCII（如中文注释）
> 会触发 "Extension code must be 7-bit ASCII" panic。注释统一用英文。

### deno_core 0.410 关键 API 差异

（比 0.409 有破坏性变化。）

- `ModuleLoader` 现在是 **trait**（不是 struct），实现它即可接管 import 解析。
- **op2 无 `(async)`**：异步 op 用同步 op2 + `async fn` 包装的模式，不要写 `#[op2(async)]`。
- **错误类型**：`JsErrorBox`（不是 `JsError`）。
- **扩展 JS 的 ASCII 校验仅 debug 生效**：`bootstrap.js` 里中文注释在 release 可用，但
  debug 构建会因非 ASCII 报错——所以 bootstrap.js 保持 ASCII-only 注释。
- **esm specifier 命名规则**：扩展 JS 用 `ext:core/ops` 之类的命名空间，主模块不能同名。
- 单 JsRuntime 单 main module；side module 用 `load_side_es_module_from_code`。

---

## 11. 安全模型与设计红线

### 11.1 通用模型

- **project_root 钳制**：所有 import 解析结果必须落在 project root 内（`ensure_within`）。
- **目录穿越防护**：路由路径里的 `..`/`.`/`\`/NUL/空段 → 404。静态兜底（`resolve_static`）
  在此之上先逐段 percent-decode 再校验——解码出 `/`（`%2F` 走私）、`.`、`..`、`\`、NUL、
  空段同样 404。
- **超时熔断**：`server.timeout` + `v8::IsolateHandle::terminate_execution`（408）。
- **SQL 注入**：`db.query/exec` 全部参数化；`db.table().select().where()` 构造器走标识符
  白名单 + 参数化值（sea-query）——见 §4 红线。
- **manifest 强校验**：`manifest.yaml` 的 `name` 必须等于父目录名，防止模块名与路由脱节。
- **租户跳转腿豁免**：`tenant.anonymous_paths` 与 auth 匿名列表同为「去 base 前缀 + 尾 `/*`」
  形式，但 tenant 匹配是**严格一层**通配（更深路径需显式列出，如 `/idp/.well-known/*`；
  oj-auth 插件实现为多层前缀），命中路径免 "缺租户头 400"——OIDC 302 浏览器跳转带不了自定义头；
  已带的头仍照常注入。

### 11.2 证书驱动的 GET 限制（运行时校验）

基于非对称加密（RSA-2048 + RS256 JWS）的证书校验**强制开启、不可绕过**：未配齐两个
证书路径即启动报错退出——没有任何 config/CLI 开关可跳过（防止误跑无证书校验的实例）。

- **有效期内**：正常服务（`certificate_status = valid`）。
- **宽限期内（默认 30 天，可配 `grace_days`）**：所有 **GET** 请求返回 `403`，标准信封；
  其余方法正常。
- **宽限期结束后**：启动期 `from_config` 检测即 `ERROR` 并中止进程（`process::exit`）；
  运行中（热替换成过期证书）则 GET 持续 `403`（服务不中断，运维可替换证书恢复）。
- **热加载**：`notify` 监听公钥/证书文件变更（事件驱动，不轮询 mtime），原子更新
  `AppState` 内共享状态（`Arc<RwLock>`），重载失败保留旧状态。

```yaml
server:
  public_key_path: "./config/public_key.pem"   # SPKI PEM 公钥（仅验签，私钥不落服务器；必配）
  certificate_path: "./config/certificate.jws" # JWS：Base64URL(Header).Payload.Signature；必配
  grace_days: 30                               # 默认 30；缩窄可加速告警
```

生成与续期（`tools/oj-cert`；格式契约单一事实来源在 `oj-cert`，CLI 与 `cert` 全局同源；
验签与状态判定在 `server/src/certificate.rs`）：

```sh
cargo run -p oj-cert -- gen -o config --days 365   # 生成 private.pem / public.pem / cert.jws
cargo run -p oj-cert -- renew -k config/private.pem --days 365   # 用现有私钥重签续期
```

- Header `{"alg":"RS256","typ":"JWT"}`（拒绝 alg=none 降级）；Payload `{nbf, exp}`（Unix 秒）；
  Signature 为 RSASSA-PKCS1-v1_5(SHA256)。私钥**仅签发端保管**，服务器只放公钥 + 证书。
- CLI `--cert-path` / `--key-path` 仅覆盖路径、不豁免校验。
- 监控：`GET {base}/health` 返回证书状态（宽限/过期仍可访问，便于探测）。

（设计背景见 `docs/superpowers/specs/2026-08-26-certificate-design.md`。）

### 11.3 桥层设计红线（不要破）

- 动态标识符（表/列）**只**来自 `SchemaRegistry`，绝不来自 JS 字符串；值**只**通过绑定参数
  传递，不拼接 SQL。
- `JsRuntime` 是 `!Send`：池与持有它的 event loop 同线程（current_thread）；inspector/WS 用
  `spawn_local`。
- `panic = "unwind"` 必须在所有插件 profile 中保持——`oj_plugin_entry!` 依赖 `catch_unwind`
  收敛跨边界 panic（任何下游 profile 不得覆盖为 abort）。
- `bootstrap.js` 必须保持 7-bit ASCII。
- 失败的 runtime **不回池**（drop 而非 checkin），避免复用可能损坏的 isolate。
- `StableState` 首次共享后不可变；命名实例/注册表/插件能力须在**构造期**注入。

---

## 12. 测试

### 约定

- **Rust 单元测试**就近放各模块 `#[cfg(test)]`；`cargo test --release --workspace` 跑全部
  （根 crate 单测 + oj/tests/e2e.rs 端到端 + 各插件测试）。测试数量随开发增长，勿在任何
  文档/断言里写死。
- handler 级验证：注入 `InMemoryAccessor` / `InMemoryKV`，断言 `Capture.body` 的 JSON，
  复用 `Bridge::new(...).run_with(...)`。
- **业务 API 测试**（`*.test.ts`）用 `oj test` 进程内运行器（真实运行时 + 真实路由管线，
  零 TCP），另可配 vitest 纯 mock 层——见 [testing.md](testing.md)。
- **勿用 `deno test`**：`json`/`db`/`http` 等全局只存在于本 bridge。
- 异步测试用 `tokio::test(flavor = "current_thread")`——`JsRuntime` 是 `!Send` 的，且池
  运行在 current_thread 运行时上（`src/bridge/runtime.rs`）。
- **TDD red-first**：先写失败测试 → 确认失败 → 最小实现 → 确认通过 → commit。

### 结构

- `oj/tests/e2e.rs`：端到端验收（UC-1…15）。`start()` 返回 2 元组 `(SocketAddr, JoinHandle)`；
  测试用 `cfg.server.port = 0` + `db default = "sqlite::memory:"` 隔离；每个用例都要自带
  `manifest.yaml`（缺失会启动失败）。负向路径覆盖：404（无路由/穿越）、405（方法未导出）、
  500（编译错误）、408（死循环超时后 server 存活）、build→release 全链路。
  `E2E_LOCK` 串行锁避免端口/文件冲突。
- 单元测试随模块内联（`#[cfg(test)]`）；独立优先——内存后端（`InMemoryAccessor` /
  `InMemoryKV` / `SqlxAccessor::arc("sqlite::memory:")`）、临时目录、`httptest` 桩 ES/fetch、
  本地 `TcpListener` 桩，全程不依赖外部服务。
- 插件适配器测试（`bridge::ffi::adapter_tests`）：mock vtable（Rust 函数指针 + 预置
  FfiFuture）验证 FfiXxxBackend 转发 + Drop close + 返回编码解码；共享静态用 `T_LOCK`
  串行化并在测试开头清理（避免跨测试污染，见 bus `DELIVER_TARGETS` 经验）。
- 真服务集成测试 + 环境变量门控（**在插件 crate 内**，本地无服务时默认跳过）：
  - `OJ_TEST_ES=http://127.0.0.1:9200` → `oj-es`（vtable roundtrip）
  - `OJ_TEST_REDIS=redis://127.0.0.1:6379/1` → `oj-kv-redis`（vtable roundtrip）
  - `OJ_TEST_S3=endpoint|bucket|region|access|secret|path_style` → `oj-blob-s3`
  - `OJ_TEST_KAFKA_BROKERS=b1:9092,b2:9092` → `oj-bus-kafka`；
    `OJ_TEST_RABBITMQ_URL=…` → `oj-bus-rabbitmq`
  - 运行：`cargo test --release --workspace`（env-gated 测试未设 env 即内联跳过；
    `infinite_loop` 曾在部分平台 SIGSEGV，CI 现按平台开关 `skip_infinite_loop` 控制，
    默认跑全量，见 `.github/workflows/plugin-matrix.yml` 的 matrix 注释）。

覆盖率经 `cargo llvm-cov --workspace --summary-only` 观测（目标行/区域 >90%）。

---

## 13. 插件系统（cdylib + FFI）

**分层**：开发侧五轴解耦（es/db/blob/bus/kv 各自 trait + 注册表），运行侧动态链接库可配置
装配。全部 FFI 跨界类型收在 `oj-plugin-ffi`；`src/bridge/ffi.rs` 收敛全部 unsafe
（`load_forget` dlopen + `Box::leak` 进程期存活，任何路径不 dlclose；插件必须 panic=unwind
profile）。适配器层 `FfiXxxBackend` 把插件 vtable 包装成 core trait 供 op 消费（构造放
core，装配层只经安全入口）。

**契约**（`oj-plugin-ffi`）：`ABI_VERSION`（当前 7，**严格相等**门禁）、`PluginDescriptor
{name, semver, abi_version, fingerprint, desc}`、各轴 repr(C) vtable（es/db/blob/bus/kv/auth）、
`HostContext`（log + deliver 回调）。`oj_plugin_entry!(init, kv => &VT)` 生成
`oj_plugin_abi_version()`、`oj_plugin_init()` 与每轴一个 `oj_plugin_axis_<name>` 符号
（paste 已 re-export 进 oj-plugin-ffi，插件无需自加依赖），并内建 `catch_unwind`。

**装配语义**（`plugin_loader.rs`）：

- 路径四级解析（先到先得）：`OJ_PLUGINS_DIR` > config `plugins_dir` > `<exe>/plugins`
  （bin/oj 旁即 bin/plugins）> `<workspace_root>/bin/plugins`（与 xtask 产物归置同形），
  相对路径相对 config 目录，最终目录 = `<plugins_dir>/<host-triple>/`。
  `host-triple` 由 `ffi::triple()` 运行时按 `std::env::consts` 重建（与 xtask `rustc -vV` 的
  host 一致），不依赖 `build.rs`（根 crate 已无 build script）。
- 双模式（`plugins:` 一段三用）：非空 map = 严格清单，只装配键列出的插件（缺文件/身份不符/
  `@semver` pin 不符 fail fast），值为插件 cfg（非空对象原样透传，空对象 = 回落轴适配器）；
  缺省/空 map → 扫描目录全部加载（目录不存在/为空 = 零插件，仅内置后端）。旧 list 写法
  `plugins: [a, b]` 废弃（解析报错）。
- 注册：abi 门禁（严格相等）→ `init` → 对 `AXES = [es, db, blob, bus, kv, auth]` 逐轴
  `dlsym("oj_plugin_axis_<axis>")`，缺符号 = 不提供该轴；加轴零破坏（既有轴 vtable 形状
  变更才 bump ABI）。
- 门禁：`ABI_VERSION` 严格相等唯一硬门禁；指纹不符仅告警。`op_plugins` / JS `plugins()` /
  公共端点 `GET {base}/plugins` 输出插件名/semver/ABI/指纹/**自描述 desc** + 宿主
  ABI_VERSION（升级核对窗口）。

**五轴接线**（server_cmd `build_registries`）：

- es 键选单后端；「cfg es 声明但无 es 插件」→ fail fast。
- db 认领式注册表：内置 sqlite/memory 打底 + 插件 db 工厂（scheme 交集冲突 fail fast；
  未知 scheme 明确报错）。
- blob 键选单 vtable 槽：多 blob 插件冲突 fail fast；driver != local 且无插件 → fail fast。
- bus 键选注册表：内置 local + 插件 kafka/rabbitmq（kind 冲突 fail fast；声明 kind 但无
  插件 → "unknown broker kind"）。FFI broker 经全局 `DELIVER_TARGETS` 按 topic 扇出。
- kv 键选单 vtable 槽：redis.default 声明 → 经 oj-kv-redis connect（探活 fail-fast）；
  未声明 → InMemoryKV 内置兜底。

**升级回滚**（部署侧）：插件替换用 `.new`/`.bak` 原子换名；`cargo xtask plugin --check`
预检；ABI bump 部署顺序 = 先升插件到新 ABI 并验证，再升宿主（或同版本原子升级）。
平台矩阵与 `bin/plugins/<triple>/` 布局见 `.github/workflows/plugin-matrix.yml`。

**第三方插件**：见 [plugin-development.md](plugin-development.md)（FFI 契约、ABI 纪律、
入口宏、panic 归因）。

---

## 14. 设计权衡与已知约束

### 已定裁决

见 spec `docs/superpowers/specs/2026-08-22-oj-server-sample-design.md` §8 的 D1–D4：

- 相对 `require()` 不支持——已知限制（db 仅 sqlite 已解除：多库 DSN 按 scheme 分发）。
  外部后端已于插件系统阶段全部 cdylib 化（见 §13）。
- `build` 已实现（按模块版本目录 + 产物保留原名原结构 + 默认 minify + manifests.yaml 锁 +
  确定性 tgz + release 聚合），设计见
  `docs/superpowers/specs/2026-08-23-oj-build-design.md`。

### 已落地（旧待办销账）

HTTP server（`server/` + `oj`）；`db.tx(fn)` 回调式事务；执行看门狗（`KillSwitch` 跨线程
`terminate_execution` 超时熔断，超时回 408 信封）；handler TS 类型（`sample/global.d.ts`）。

### 仍开放

- **fetch SSRF 防护**：出网白名单、内网/RFC1918/链路本地 IP 阻断、body 上限、
  DNS 解析后复检（防重绑定）、重定向复检——`src/bridge/fetch.rs` 目前均未做
  （仅 `no_proxy` + 响应整体缓冲）。
- **V8 内存上限**：`ResourceLimiter` 未接（超时熔断已有，内存无界）。
- **op 边界埋点**：`metrics` + `/metrics` 端点（exec 时长、op 计数/延迟、db 延迟、v8 堆）。
- **错误信息收敛**：`json.fail` 与 500 路径不泄露 Rust/DB 内部细节的审计。

---

## 15. 排错

| 现象 | 原因 / 处理 |
|---|---|
| debug 构建失败（rusty_v8 链接错误） | 本仓库禁止 debug 构建；一律 `--release`（见 §1）。 |
| `Extension code must be 7-bit ASCII` | `bootstrap.js` 含非 ASCII。改为英文。 |
| `Cannot create a handle without a HandleScope`（进程退出时） | 创建了未轮询 event loop 的空闲 JsRuntime 并 drop。RuntimePool 按需增长已规避；不要手写预热。 |
| `unknown table 'x'` / `unknown column 'y'` | 标识符不在 `SchemaRegistry`。先在构造期 `registry.table(...)` 声明。 |
| handler 无响应 / 永远 pending | 未调用 `json.ok/fail/finish`；或 `.catch` 缺失导致 Promise rejection 未被捕获。死循环会被 KillSwitch 超时熔断（408）。 |
| `fetch` 回环慢（ms 级） | 系统代理拦截。client 已 `no_proxy`；确认未另建带代理的 `reqwest::Client`。 |
| inspector 连不上 | 须构造传 `inspect=true` **且** `await start_inspector(&b, addr)`。 |
| 插件拒载（ABI / 身份 / semver） | `ABI_VERSION` 严格相等门禁；`cargo xtask plugin <name> --check` 预检定位。 |
| 某轴报 "not configured" / 找不到后端 | 该轴无插件提供（缺符号 = 不提供该轴），或 `plugins:` 清单未列。 |
| 启动报 `missing manifest.yaml` / manifest name 不符 | 首层子目录缺清单，或 `name` ≠ 目录名。补齐 / 对齐。 |
| 启动报 `manifests.yaml … run oj build first` | release 锁缺失/损坏/指向不存在版本。跑 `oj build`。 |
| 启动报 `certificate …` 系列错误 | 缺路径/密钥不匹配/JWS 格式错/宽限尽。见 §11.2；`oj-cert` 重签。 |
| 启动报 `redis 'default': …` | Redis 不可达（fail-fast）。起 Redis 或核对 URL；不依赖就注释掉 `redis:` 段。 |
| 400 `missing tenant header` | `tenant.enable` 且未带租户头。客户端补 header。 |
| 401 `missing or invalid bearer token` | auth 启用且路径不匿名。走登录端点换 token。 |
| 500 `transaction already active` | 嵌套 `db.tx`。合并为一个事务回调。 |
| 日志 `open transaction … rolled back at request end` | `db.tx` 漏 await。修 handler；数据已按未提交丢弃。 |
| `bus.publish` 收不到广播 | bus 缺省为进程内，跨实例不互通。发布与订阅须同实例。 |
| `GET {base}/…/ws` 404 | release 未重新 build，或 URL 含版本段。先 `oj build`。 |
| 改 `api.ts` 不生效 | release 下 dist 未更新 / 换版本未重启。同步 dist；必要时重启。 |
| `blob not configured` / `es not configured` | config 无对应段。加 `blob:` / `es.endpoint`。 |
| 启动报 M004 / 迁移账本落后 | release verify 门禁：有迁移未应用。先 `oj migrate` 再启动。 |
| 启动报 `ext_boot: …` 并拒启 | ext_boot.js 语法错/导入失败/顶层 await 抛错。看报错定位；改完重启。 |
| 改了 `ext_boot.js` 没生效 | 不做热重载，装配期已冻结 spec。重启进程。 |

完整运维排障表见 [ops-manual.md](ops-manual.md) §7。

---

## 16. 提交与 CI

- 门禁：`cargo fmt --check` + `cargo clippy --all-targets -D warnings` +
  `cargo test --release --workspace`（+ sample 的 L1 `oj test` 与 L2 vitest，见
  `sample-tests` job）。`infinite_loop` 曾全局 `--skip`，现已改为按平台开关，默认跑全量。
- 纪律：改动后 **release 下测试与构建都要绿**才算完成；**TDD red-first**（先失败测试再
  最小实现）；commit 尾随 `unix@vip.qq.com ai`。
- CI（`.github/workflows/`）：`release.yml`（linux-gnu / macos / windows 三平台测试 + 打包）
  与 `plugin-matrix.yml`（宿主 + 全部插件矩阵，`cargo xtask` 同款布局）。全部 `--release`。
- 精确 pin `deno_core` 版本（V8 ABI 随其变化）。
- 产物统一 `cargo xtask build` 归置 `bin/`，发行布局与插件发现路径同形。
