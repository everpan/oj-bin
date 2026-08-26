# JavaScript Is All You Need

> `only-js`（代号 **oj**）—— 把 JS/TS 运行时（`deno_core` / V8）嵌入 Rust 的低代码后端框架。
> 按目录写 `api.ts`，目录树即路由表；改文件即生效，编译产物可发布。

---

## 初衷

toB 项目的实施过程中，往往需要 `低代码` 来快速交付。市场上低代码的实现五花八门、奇花异放，
但剥开外壳，本质都是一个高度可配置的系统——而在所有配置形态里，**能编程的配置是最高级的**。
以往的做法通常是在后端语言里嵌入脚本（lua、js），其中 js 因开发者基数庞大而热度最高。

问题是：无论选哪门后端语言，复杂性都躲不掉。你既要养一门 java / golang / c# 的后端技术栈，
又要解决「如何在这门语言里嵌入并驾驭 js 运行时」；同时前后端分离开发本身，还带来了沟通成本与
知识传承的组织问题。

本项目打算集大成，**将前后端统一到 JS/TS 一门语言上**。

### 那 Node.js 已经够用了，为什么还要再搞一个？

因为「能跑 JS」从来不是这里的难点，**「让业务只写 JS，其余全部由宿主兜住」才是**。
本项目的取舍与 Node 明显不同：

- **交付形态**：核心是一个 Rust 二进制。运行期不需要 `node_modules`、不需要装工具链；
  业务模块可构建成带版本的产物目录 + 确定性 `.tgz` 发布。
- **安全护栏下沉到 Rust**：SQL 的动态标识符（表名/列名）只能来自 Rust 侧
  `SchemaRegistry` 白名单，值只能走绑定参数——业务写错也拼不出注入。多租户、JWT 鉴权、
  证书校验、静态目录穿越防护都在宿主里，不靠业务自觉。
- **零配置路由**：目录镜像即路由，不写一行注册代码（见下文）。
- **能力可装卸**：数据库方言、S3、Redis、ES、Kafka/RabbitMQ 都是 **cdylib 插件**，
  按需装载；不装的能力不进二进制、也不引入依赖。
- **可控的执行环境**：`JsRuntime` 池化复用 + 超时看门狗（`KillSwitch`），
  单请求跑飞不会拖垮进程；失败的 runtime 直接丢弃而非复用。
- **dev / release 双模**：dev 直接跑 `.ts`（按需转译 + 热重载），release 跑预构建 `.js`
  （不转译、按锁聚合）。同一份源码，两种模式自动判定。

---

## 快速开始

```bash
cargo build                                                   # 首次构建会拉取预编译 V8

# dev：直接跑 .ts 源码（目录内无 manifests.yaml → 自动判定 dev/ts，改文件即生效）
cargo run -p oj -- server -c sample/config.yaml -d sample/src

# release：先构建产物，再跑 dist/（目录内有 manifests.yaml → 自动判定 release/js）
cargo run -p oj -- build  -d sample/src -o sample/dist
cargo run -p oj -- server -c sample/config.yaml -d sample/dist
```

启动时会打印模块清单与路由表，然后：

```bash
curl 'http://localhost:9778/v1/api/user/account/?id=1'
# → {"code":0,"msg":"ok","data":[{"id":1,"name":"neo","role":"admin"}]}
```

> 网络受限时设 `V8_FROM_SOURCE=0` 强制走预编译包——**不要**从源码编译 V8。

---

## 一个 handler 长什么样

`api.ts` 默认导出方法表（`get`/`post`/`put`/`del`/`patch`/`head`/`options`）。
全局对象由宿主注入，无需 import；`json.ok` / `json.fail` 之一必须被调用一次以结束会话。

```ts
function get(): void {
  const id = Number(http.param("id", 0));
  db.query("select id, name, role from account where id = ?", [id])
    .then((r) => (r.length ? json.ok(r[0]) : json.fail(404, "no such account")))
    .catch((e) => json.fail(500, String(e)));
}

function post(): void {
  const b = http.body as { name?: string };
  if (!b.name) { json.fail(400, "name required"); return; }
  db.exec("insert into account (name) values (?)", [b.name])
    .then(() => json.ok({ created: true }))
    .catch((e) => json.fail(500, String(e)));
}

export default { get, post };
```

响应统一为 `{code, msg, data}` 信封。注入的全局对象：

| 全局 | 用途 |
|---|---|
| `json` | `ok` / `fail` / `header`——统一信封与响应头 |
| `http` | 只读请求上下文：`method` / `param()` / `query` / `headers` / `body` / `tenantId` |
| `db` / `DB(name)` | SQL 访问；`db === DB("default")`，多库按名取用 |
| `kv` / `redis` | 键值存储（未配 Redis 时为内存实现） |
| `blob(name)` | 对象存储（local / s3） |
| `bus` | 发布订阅（`publish` / `subscribe`），跨实例广播 |
| `es` | Elasticsearch（`search` / `index` / `del`） |
| `ws` | WebSocket 帧上下文 |
| `fetch` | 浏览器兼容 Fetch |
| `log` | 结构化日志（tracing） |
| `plugins()` | 已装载插件自省 |
| `finish()` | 结束会话但不写响应 |

---

## 目录镜像路由

目录树 **就是** 路由表，无需注册：

```
sample/src/
  user/
    manifest.yaml            # name / desc / version（构建产物版本来源）
    account/api.ts           → /v1/api/user/account/
    profile/detail/api.ts    → /v1/api/user/profile/detail/
    item/api.ts              → /v1/api/user/item/{id}   （见下）
    _shared/validate.ts      # 下划线前缀 = 私有，不成路由
  news/
    api.ts                   → /v1/api/news
    WS.ts                    → /v1/api/news/ws          （WebSocket）
```

- **路径参数**：给 handler 挂 `.route` 即可替换目录镜像——
  `detail.route = "{id}"` 使 `/v1/api/user/item/{id}` 可达（此时 `/v1/api/user/item` 为 404）。
- **WebSocket**：`WS.ts` 每收到一个文本帧执行一次。首帧 `bus.subscribe("news")` 订阅主题后，
  任意 handler（含其它实例）的 `bus.publish("news", ...)` 都会广播到该连接。
- **前缀**：`/v1/api` 来自 config 的 `server.base`，可用 `-b` 覆盖。

---

## 配置概览（`config.yaml`）

块存在即启用、缺省即关闭——这是配置的总原则（完整说明见 `docs/user-manual.md`）。

```yaml
server:
  host: "localhost"
  port: 9778
  base: "/v1/api"      # API 前缀
  root: "dist"          # 静态站点根（省略 = 不开静态服务）
  timeout: "30s"        # 单请求执行超时（熔断 → 408）
  pool_size: 4          # JS 执行并发度
db:
  default: "sqlite://db.sqlite"     # 多库混用：按名 DB("name") 取用
redis:  {}    # 配置存在即真连（启动 fail-fast）；注释掉 → 内存 KV
es:     {}    # 存在即启用 es.*
blob:         # 存在即启用 blob.* + {base}/blob/{key} 下载路由
  driver: "local"       # local | s3
  root: "uploads"
tenant:       # 多租户：请求须带 header_key，值注入 http.tenantId
  enable: true
  header_key: "X-TENANT-ID"
auth:         # JWT：内置 /v1/api/auth/{login,refresh,logout} + Bearer 守卫
  jwt_secret: "change-me"
  anonymous_paths: ["/health"]
```

---

## 架构概览

```
only-js/
  src/                 核心库：src/bridge/（JS↔Rust 桥、各后端轴）+ src/config.rs
  oj/                  CLI 二进制：server / build / test 子命令（编排入口）
  server/              axum HTTP 服务：路由查找 → 执行 handler → 写回 Capture
  oj-plugin-ffi/       宿主与插件共享的 C-ABI 契约（ABI_VERSION 严格相等门禁）
  crates/plugins/      cdylib 插件：oj-es / oj-db-{mysql,postgres} / oj-blob-s3
                       / oj-bus-{kafka,rabbitmq} / oj-kv-redis
  xtask/               插件构建 / 拷贝 / 预检工具
  sample/              可跑的示例项目（config.yaml + src/ + dist/）
  docs/                设计与手册
```

**请求链路**：HTTP 请求 → `server` 兜底路由（含内置 `/auth/*`、`/blob/{key}`）→
`RouteTable.lookup` → 从 `RuntimePool` 取出一个 `JsRuntime`、重置 per-request 状态 →
执行 `api.ts` 的对应方法（dev 模式先转译）→ 捕获 `{code,msg,data}` 信封 → 写回响应。

**JS↔Rust 边界**：`src/bridge/mod.rs` 用 `deno_core::extension!` 注册所有 `op_*`，
`bootstrap.js` 把这些 op 装配成上表的全局对象。每个后端轴（db / kv / blob / bus / es /
fetch / http / ws）各自一个模块。

**插件**：启动时 `dlopen` 加载 cdylib，校验 ABI 版本与身份后，把插件 vtable 包装为核心后端；
插件内 panic 由 `oj_plugin_entry!` 的 `catch_unwind` 收敛为错误，不会 abort 宿主。

---

## 开发常用命令

```bash
cargo build --workspace                  # 构建全部成员（含插件）
cargo test                               # 根 crate 单元测试
cargo test --workspace                   # 全量测试（含 oj e2e）
cargo fmt --check                        # 格式门禁
cargo clippy --all-targets -D warnings   # lint 门禁
cargo bench                              # criterion 基准

cargo run -p oj -- test -c sample/config.yaml    # 进程内跑 *.test.ts（无需起服务）
cargo xtask plugin <name>                        # 构建插件并拷入 plugins/<triple>/
cargo xtask plugin <name> --check                # 插件预检（ABI / 身份 / semver / 符号）
```

异步测试请用 `tokio::test(flavor = "current_thread")`——`JsRuntime` 是 `!Send` 的。
不要用 `deno test`：handler 依赖的全局对象只存在于本 bridge 中。

---

## 设计红线

- **SQL**：动态标识符只来自 `SchemaRegistry` 白名单，值只走绑定参数，绝不拼接。
- **`JsRuntime` 是 `!Send`**：池及其持有者同处 `current_thread` 运行时；inspector/WS 用 `spawn_local`。
- **`panic = "unwind"`**：所有插件 profile 必须保持，否则跨边界 panic 收敛失效。
- **`bootstrap.js` 必须是 7-bit ASCII**（非 ASCII 会触发 deno_core 报错）。
- 失败的 runtime 一律丢弃，不回池。

---

## 文档

| 文档 | 内容 |
|---|---|
| `docs/user-manual.md` | `oj` CLI 与 `config.yaml` 完整参考 |
| `docs/dev-guide.md` / `docs/dev-manual.md` | 开发手册（含加新 op 的步骤） |
| `docs/bridge.md` | JS 全局对象与模块对照 |
| `docs/plugin-architecture.md` / `docs/plugin-development.md` | 插件架构与开发 |
| `docs/route-params-design.md` | 路径参数路由设计 |
| `docs/testing.md` | 测试约定 |
| `docs/ops-manual.md` | 运维 |
| `docs/benchmarks.md` | 性能数据 |
| `sample/README.md` | 示例项目说明 |

> `docs/dev-guide.md` 中部分内容描述的是较早的进程内 `Bridge` API 与初版插件方案，
> 命令与结构以本文件及代码为准。
