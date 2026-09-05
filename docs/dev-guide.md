# 开发手册（Developer Guide）

`only-js`（代号 **oj**）—— 基于 Rust + `deno_core` 的低代码后端框架，将 JS/TS 运行时（V8）
嵌入 Rust。业务逻辑以 JS/TS「handler」编写，使用注入的全局对象（`json` / `db` / `http` /
`kv` / `blob` / `bus` / `es` / `fetch` / `log` / `ws` / `plugins` / `cert` / `jwt` /
`bcrypt` / `crypto` / `finish`），Rust 侧捕获统一的 `{code,msg,data}` 信封，由 HTTP 服务写回。
数据库、KV、对象存储、事件总线、ES 等后端能力在启动时作为 **cdylib 插件** 通过 C-ABI FFI
契约（`oj-plugin-ffi`，ABI 7）加载。

本手册面向**日常开发**：环境、构建、写 handler、Rust 侧嵌入 API、加 op、测试、调试。
模块对照与 JS API 细节见 [bridge.md](bridge.md)，JS 全局对象完整参考见
[devkit/api-manual.md](devkit/api-manual.md)，插件开发见
[plugin-development.md](plugin-development.md) 与 [plugin-architecture.md](plugin-architecture.md)，
性能数据见 [benchmarks.md](benchmarks.md)。

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
cargo test --release --workspace        # 全部测试（根 crate + oj e2e + 插件；CI 同款，
                                        #   另加 -- --skip infinite_loop）
cargo test -p oj --test e2e <name>      # 某个具体的 e2e 用例
cargo test -- --nocapture   # 看 tracing 输出
cargo build --benches       # 编译 criterion 基准（不跑）
cargo bench                 # 跑基准（benches/bridge.rs）
```

### oj CLI 与 xtask

```bash
cargo run -p oj -- server -c config.yaml --api-path src   # 启动服务（dev/release 自动判定）
cargo run -p oj -- build -d sample/src -o sample/dist     # 构建模块产物
cargo run -p oj -- test -c config.yaml --format human     # 进程内 *.test.ts 测试运行器
cargo run -p oj -- migrate / fixture / schema diff        # 迁移 / 演示数据 / schema 对账
# 子命令全表见 oj/src/args.rs 与 docs/cli2.md
```

```bash
cargo xtask bin                    # 构建 oj（release）并拷入 bin/oj
cargo xtask plugin <name>          # 构建 oj-<name>（release）并拷入 bin/plugins/<host-triple>/
cargo xtask plugin <name> --check  # 预检（ABI / 身份 / semver / 符号）
cargo xtask build                  # 构建 oj + 全部第一方插件，统一归置 bin/
```

所有编译产物归置到 `bin/`：`bin/oj` 与 `bin/plugins/<host-triple>/`（发行布局与插件加载器
默认发现路径同形）。

---

## 2. 项目结构（workspace 布局）

```
src/                          # 根 crate only-js：核心库（纯 lib，无 bin、无 build.rs）
  lib.rs                      #   暴露 pub mod bridge / config
  config.rs                   #   配置解析（yaml）
  bridge/
    mod.rs                    #   Bridge / StableState / ReqState / Capture / extension 注册
    bootstrap.js              #   JS 全局对象装配（ESM，必须 7-bit ASCII）
    runtime.rs                #   RuntimePool（复用 JsRuntime）+ KillSwitch 超时熔断
    module_loader.rs          #   ESM/CJS 模块加载（?v=<mtime> 版本化 specifier）
    transpile.rs              #   TS→JS 转译（deno_ast）+ mtime 缓存 + 可选 minify
    db.rs / query.rs / accessor_sqlx.rs   # 数据访问 + 安全查询构造器（SchemaRegistry 白名单）
    kv.rs / blob.rs / bus.rs / es.rs / fetch.rs / http.rs / ws.rs / log.rs
    cert.rs / crypto.rs / auth.rs / guard.rs / envelope.rs / registry.rs
    ffi.rs / plugin_loader.rs #   dlopen + 按轴符号装载（AXES = es/db/blob/bus/kv/auth）
    inspector.rs              #   DevTools WS 桥
    loader.rs                 #   HandlerStore（嵌入 map / FS 目录，嵌入与测试场景用）
oj/                           # CLI 二进制：server / build / test / migrate / fixture / schema
server/                       # axum HTTP 服务（路由管线 / auth / multipart / 证书 / WS）
oj-plugin-ffi/                # FFI 契约：ABI_VERSION=7、repr(C) vtable、oj_plugin_entry!
plugins/                      # 8 个 cdylib 插件：oj-es、oj-db-mysql、oj-db-postgres、
                              #   oj-blob-s3、oj-bus-kafka、oj-bus-rabbitmq、oj-kv-redis、oj-auth
tools/xtask/                  # 构建/拷贝/预检辅助（cargo xtask）
tools/oj-cert/                # 证书生成小工具 crate
tests/plugins/mini/ 等        # 演练加载/ABI/panic 路径的 cdylib 测试夹具
sample/                       # 示例应用（config.yaml + src/ + dist/ + tests/ + global.d.ts）
bin/                          # 全部编译产物归置（xtask 输出，不入库）
benches/bridge.rs             # criterion 基准
```

---

## 3. 写 handler（JS/TS）

handler 是 ESM 源码（dev 模式 `.ts` 按需转译，release 模式服务 `oj build` 产物 `.js`）。
**目录镜像路由**：`src/user/profile/detail/api.ts` → `/v1/api/user/profile/detail/`；
`api.ts` 导出 `get`/`post`/`put`/`del`/`patch`/`head`/`options`（HTTP 方法同名，`DELETE`→
`del`）；可选 `get.route = "{id}"` 声明路径参数。handler **必须调用一次**
`json.ok` / `json.fail` / `finish` 才能完成会话；顶层可直接 `await`（event loop 由
`run_to_completion` 驱动至 Promise 落定）。

### JS 全局对象速查（以 `src/bridge/bootstrap.js` 挂载为准）

| 全局 | 用途 | 关键点 |
|---|---|---|
| `json.ok(data)` | 成功信封 `{code:0,msg:"ok",data}`，status 200 | 标记会话完成 |
| `json.fail(code, msg, data?)` | 失败信封；`code<=0` 映射 500 | 标记会话完成 |
| `json.header(name, value)` | 设置返回头（覆盖语义） | 空名忽略 |
| `db` / `DB(name)` | 数据访问；`db === DB("default")` | 未配置的名字返回 `undefined` |
| `db.query(sql, params?)` / `db.exec(sql, params?)` | 原始 SQL + 绑定参数 | **优先用参数，勿拼接** |
| `db.table(name).select(...).where(...).orderBy(...).limit(...).offset(...).all()` | 安全构造器 | 标识符走白名单、值参数化 |
| `db.tx(async (tx) => {...})` | 回调式事务 | resolve 提交、throw/reject 回滚；不支持嵌套；`tx` 提供 `query/exec/table` |
| `http.method/params/query/headers/body/tenantId/user` | 只读请求上下文（懒 Proxy） | `body` 能解析 JSON 则为对象，否则字符串，空为 null |
| `http.param(name, def?)` | 路径参数优先、回落 query | 取不到返回 `def` |
| `http.file(i)` | multipart 上传文件（按序号） | 服务层已解析 |
| `kv` / `redis` | KV 存储：`get/set/del/expire/incr` | 两者同一实现；oj-kv-redis 插件提供真 Redis，未配回落进程内内存 KV |
| `blob(name)` | 对象存储：`put/get/del/url/contentType` | 裸 `blob.put(...)` === `blob("default").put(...)` |
| `bus.publish(topic, data)` / `bus.subscribe(topic)` / `bus.kind()` | 事件总线 | `kind()` ∈ `local/kafka/rabbitmq` |
| `es.search/index/del(index, ...)` | Elasticsearch 薄客户端 | 未配置 es 时报错 |
| `fetch(url, opts?)` | 浏览器兼容 Fetch（reqwest） | 响应整体缓冲；不支持 AbortController |
| `log.debug/info/warn/error(msg, ...kv)` | 结构化日志（tracing） | 交替键值对 |
| `cert.generate/renew` | JWS 证书签发/续期（Rust 侧 RSA） | |
| `jwt.sign/verify` + `jwt.accessDuration/refreshDuration` | JWT 签发验签 | 密钥/时长装配期注入 |
| `bcrypt.hash/verify` | 密码哈希（Rust `spawn_blocking`） | |
| `crypto.sha256Hex(s)` / `crypto.randomHex(n)` | 摘要与随机数 | 与原生 `getRandomValues` 合并 |
| `ws.send(data)` / `ws.close()` | WS 帧循环控制 | 仅 WS 连接内有意义，HTTP 路径 no-op |
| `plugins()` | 已装配插件自省（name/semver/abi/…） | 同源 `GET {base}/plugins` |
| `finish()` | 标记会话完成但不写响应 | 少用 |

另有 `__ojRequire(name, referrerPath)`（CJS 互操作，模块加载器内部使用，业务代码不直接调）。

### 查询构造器（`db.table(...)`）

`db.table(name)` 返回构造器：`select(cols)`、`where(cond)`（可链多次）、
`orderBy([{field,dir}])`、`limit(n)`、`offset(n)`、`all()`。`all()` 返回 `Promise<Row[]>`。

- **条件 `cond`**：`{ field, op, value }`。`op` ∈ `eq/ne/gt/gte/lt/lte/in/like/isNull`。
  `in` 的 `value` 为数组；`isNull` 不需 `value`。
- **白名单**：`table` 与 `field` 必须先在 `SchemaRegistry` 声明，否则报 `unknown table/column`。
- **limit**：默认 100、硬上限 1000。
- **排序**：仅 `dir:"asc"|"desc"`，字段须 `is_sortable`（注册表默认所有列可排序）。

```js
await db.table("order")
  .select(["id", "amount"])
  .where({ field: "user_id", op: "eq", value: 1 })
  .where({ field: "amount", op: "gte", value: 100 })
  .orderBy([{ field: "amount", dir: "desc" }])
  .limit(50).all();
```

### SQL 注入防护（必须读）

- **标识符**（表名/列名）**绝不**来自 JS 字符串拼接——只能来自 `SchemaRegistry` 白名单
  （经 `db.table(...)` 构造器）。
- **值**通过绑定参数传递：`db.query("... where id = $1", [id])` 或构造器的 `value`。
- 占位符风格随底层驱动而定：`$1`（Postgres）/ `?`（MySQL、SQLite）——见 §5。

---

## 4. 在 Rust 侧使用 Bridge（嵌入 / 测试）

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

`HandlerStore`（`loader.rs`）仍在：`from_embedded(map)`（编译期嵌入，配 `set_handlers` +
`run_named`）与 `MDM_HANDLER_DIR` 环境变量（FS 目录 + notify 监听）。这条路径服务**嵌入
与测试场景**；oj server 的 handler 加载走 `module_loader.rs` + `run_module`（见 §6）。

---

## 5. 接入真实数据库（SqlxAccessor）

`SqlxAccessor` 实现了 `DataAccessor`，以 `sqlx::any::Pool<Any>` 驱动无关接入
MySQL/PostgreSQL/SQLite（生产部署经 **oj-db-mysql / oj-db-postgres 插件**接入；直接用
`SqlxAccessor` 属嵌入/测试场景）。驱动安装（`install_default_drivers`）内聚于 `connect`。

```rust
use only_js::bridge::SqlxAccessor;

let db = SqlxAccessor::arc("postgres://user:pass@localhost/oj").await?; // Arc<dyn DataAccessor>
let db2 = SqlxAccessor::connect("sqlite:///tmp/oj.db").await?;          // Self，再自行 Arc
```

- 占位符风格见 §3。构造器经 sea-query 的方言 `QueryBuilder` 生成驱动原生 SQL；裸 SQL 直连
  非 PG 驱动时注意 `$N` 与 `?` 的差异。

---

## 6. 模块加载与热重载（oj server）

现行加载链路在 `module_loader.rs` + `transpile.rs`：

- **ESM/CJS 双支持**：handler 与项目内模块按 ESM 解析；CJS 依赖（node_modules）经
  `__ojRequire` 同步互操作。
- **版本化缓存**：模块 specifier 带 `?v=<mtime>`，文件变更即自然失效缓存，无需清理模块图。
- **TS 转译**：dev 模式 `deno_ast` 剥类型按需转译，结果按 mtime 全局缓存，可选 minify。
- **热重载**：oj server dev 模式用 `notify` 监听源码树，变更后按上述 mtime 版本化天然生效。
- **release 模式**：服务 `oj build` 产出的 `dist/`（预转译 JS + `routes.js` +
  `manifests.yaml` 版本锁），不转译。

模式自动判定：目录含 `dist/manifests.yaml` ⇒ release，否则 dev。命令与构建产物见
`cargo run -p oj -- --help` 与 [cli2.md](cli2.md)。

另有运行时扩展点 **`ext_boot.js`**：`<config_dir>/ext_boot.js` 存在即被每个新建 JsRuntime
加载一次，可在已有全局上组合增补（约束与用法见 [bridge.md](bridge.md)）。

---

## 7. DevTools 调试（inspector）

- 构造时开开关：`Bridge::with_opts(db, kv, registry, true)`（或 `with_dbs*` 传 `true`）。
- 起服务：`only_js::bridge::start_inspector(&b, "127.0.0.1:9229".parse()?).await`——借一个
  runtime 取其 inspector 句柄并起 WS（`inspector.rs`）。未开开关则只 warn 不生效。
- 浏览器 `chrome://inspect` → 配置 `127.0.0.1:9229` → 断点/单步/看 console。
- inspector 是 `!Send`，WS 服务跑在 `spawn_local`（current_thread runtime）。
- **仅开发用**：生产构建不要开。

---

## 8. 加一个新的 op（扩展 JS SDK）

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

---

## 9. 测试约定

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

---

## 10. 插件系统（指针）

后端能力（db 方言 / s3 / redis / es / kafka / rabbitmq / auth）是 cdylib 插件，经
C-ABI FFI 契约（`oj-plugin-ffi`）加载：

- **ABI 7 严格相等门禁**：任何 repr(C) vtable 变更必须 bump `ABI_VERSION`；向后兼容演进走
  cfg 的 JSON 字段。
- **按轴注册**：装载器对 `AXES = [es, db, blob, bus, kv, auth]` 逐轴
  `dlsym("oj_plugin_axis_<axis>")`，缺符号 = 不提供该轴；加轴零破坏。
- **插件自描述**：`descriptor.desc` 必填，经 `GET {base}/plugins` 公开；JS 侧 `plugins()`。
- **配置一段三用**：`plugins:` 键 = 严格清单（非空 map 只装配列出的插件）/ 值 = 透传 cfg
  （非空对象原样透传，空对象回落轴适配器）/ 缺省或空 map = 扫描模式。
- **发现路径**（4 级，先到先得）：`OJ_PLUGINS_DIR` > config `plugins_dir` > `<exe>/plugins`
  > `<workspace_root>/bin/plugins`，各拼接 `<host-triple>/`。

细节与开发步骤见 [plugin-development.md](plugin-development.md)（契约）与
[plugin-architecture.md](plugin-architecture.md)（装配层）。

---

## 11. 已知约束与待办

已落地（旧待办销账）：HTTP server（`server/` + `oj`）；`db.tx(fn)` 回调式事务；执行看门狗
（`KillSwitch` 跨线程 `terminate_execution` 超时熔断，超时回 408 信封）；handler TS 类型
（`sample/global.d.ts`）。

仍开放：

- **fetch SSRF 防护**：出网白名单、内网/RFC1918/链路本地 IP 阻断、body 上限、
  DNS 解析后复检（防重绑定）、重定向复检——`src/bridge/fetch.rs` 目前均未做
  （仅 `no_proxy` + 响应整体缓冲）。
- **V8 内存上限**：`ResourceLimiter` 未接（超时熔断已有，内存无界）。
- **op 边界埋点**：`metrics` + `/metrics` 端点（exec 时长、op 计数/延迟、db 延迟、v8 堆）。
- **错误信息收敛**：`json.fail` 与 500 路径不泄露 Rust/DB 内部细节的审计。

### 设计红线（不要破）

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

## 12. 排错

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

---

## 13. 提交与 CI

- 门禁：`cargo fmt --check` + `cargo clippy --all-targets -D warnings` +
  `cargo test --release --workspace`（CI 额外 `-- --skip infinite_loop`）。
- CI（`.github/workflows/`）：`release.yml`（linux-gnu / macos / windows 三平台测试 + 打包）
  与 `plugin-matrix.yml`（宿主 + 全部插件矩阵，`cargo xtask` 同款布局）。全部 `--release`。
- 精确 pin `deno_core` 版本（V8 ABI 随其变化）。
- 产物统一 `cargo xtask build` 归置 `bin/`，发行布局与插件发现路径同形。
