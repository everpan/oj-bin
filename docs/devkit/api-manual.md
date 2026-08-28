# oj TS API 开发手册

适用版本：API 面 v0.2（二进制版本号见 `oj/Cargo.toml`）

本手册面向**用 oj 框架开发业务项目**的开发者与 AI agent：如何组织模块、编写
`api.ts` / `WS.ts` handler、使用注入的全局对象、写测试、配服务、构建发布与日常运维。
oj 是把 V8（deno_core）嵌进 Rust 的低代码后端框架：业务逻辑以 TS handler 编写，
运行时注入 `json` / `db` / `http` / `kv` / `blob` / `bus` / `es` 等全局对象，
统一以 `{code,msg,data}` 信封写回 HTTP。仓库内部实现见 `docs/dev-manual.md`，
部署排障细节见 `docs/ops-manual.md`。本手册随版本包 `devkit/` 一同发布；
API 签名以同目录 `global.d.ts` 为类型权威。

目录：1 快速开始 / 2 项目结构与模块约定 / 3 编写 api.ts / 4 导入解析 /
5 全局对象 API 参考 / 6 响应信封与错误码 / 7 鉴权与多租户 / 8 测试 /
9 配置 config.yaml / 10 构建与发布 / 11 运维要点 / 12 安全红线与已知限制

## 1. 快速开始

> 何时读我：第一次接触 oj，要从零跑通一个请求。

### 前置

二选一拿到可执行物：

- **发行包**：解包 `oj-v<version>.tar.gz`，得到 `oj`（主程序）、`plugins/<平台>/`
  （cdylib 插件）、`devkit/`（本手册）。
- **自建**（仓库内）：`cargo xtask build` —— 以 release 构建并归置
  `bin/oj` + `bin/plugins/<host-triple>/`。注意本项目禁止 debug 构建。

### 证书必配（无逃生口）

证书校验强制开启：`server.public_key_path` 与 `server.certificate_path` 缺任一路径，
启动直接报错退出，没有任何 config / CLI 开关可跳过。用 `oj-cert` 工具生成三件：

```bash
cargo run -p oj-cert -- gen -o config --days 365
# → config/private.pem（私钥，仅签发端保管，勿放服务器）
#   config/public.pem（公钥，放服务器）
#   config/cert.jws （JWS 证书，放服务器）
```

config 指向其中两件（公钥 + 证书）：

```yaml
server:
  public_key_path: "./config/public.pem"
  certificate_path: "./config/cert.jws"
```

### 最小项目

```
myproj/
├── config.yaml
├── config/
│   ├── public.pem
│   └── cert.jws
└── src/
    └── hello/
        ├── manifest.yaml
        └── api.ts
```

`config.yaml`（全部字段可省，仅证书两路径必配；生产建议显式写端口）：

```yaml
server:
  port: 9778                       # 代码默认 778 属特权端口，用 ≥1024
  public_key_path: "./config/public.pem"
  certificate_path: "./config/cert.jws"
db:
  default: "sqlite://db.sqlite"    # 相对 config 所在目录；缺文件自动建空库
```

`src/hello/manifest.yaml`（`name` 必须等于目录名，违反启动失败）：

```yaml
name: "hello"
desc: "第一个模块"
version: "0.1.0"
```

`src/hello/api.ts`：

```ts
export default {
  get() {
    json.ok({ hello: "oj" });
  },
};
```

### 启动与验证

```bash
./bin/oj server -c config.yaml -d src
# 仓库内等价：cargo run -p oj -- server -c config.yaml -d src
```

`server` 按 `-d` 目录**自动判定模式**：目录含 `dist/manifests.yaml`（构建锁）→
release（跑预构建 `.js`，不转译）；否则 dev（服务 `.ts` 源码，按需转译，改文件即生效）。
启动行会打印判定结果（`dev/ts` / `release/js`）与模块清单、路由表。

验证信封返回：

```bash
curl 'http://localhost:9778/v1/api/hello/'
# → {"code":0,"msg":"ok","data":{"hello":"oj"}}
```

### 交付（release）跑法

```bash
./bin/oj build -d src -o dist        # 生成 dist/<module>-<version>/ + manifests.yaml 锁 + tgz
./bin/oj server -c config.yaml -d dist
```

构建细节见第 10 章；完整可跑样例见仓库 `sample/`（`sample/README.md`）。

## 2. 项目结构与模块约定

> 何时读我：建新项目或新模块前。

### 源码树（dev 服务目录）

```
<project>/
├── config.yaml          # 服务配置（第 9 章）
├── seed.sql             # 可选，启动时对 default 库重放
├── src/                 # dev 服务目录（release 用 dist/，见第 10 章）
│   ├── user/            # 首层子目录 = 模块名
│   │   ├── manifest.yaml          # 模块清单（必配）
│   │   ├── _shared/validate.ts    # 无 api 文件 → 纯工具代码目录，不产生路由
│   │   ├── account/api.ts         # → {base}/user/account/
│   │   ├── profile/api.ts         # → {base}/user/profile/
│   │   └── profile/detail/api.ts  # → {base}/user/profile/detail/（任意深度）
│   └── order/
│       ├── manifest.yaml
│       └── list/api.ts            # → {base}/order/list/
├── tests/               # L1 测试（*.test.ts，第 8 章）
└── node_modules/        # 裸 specifier 解析起点（第 4 章）
```

约束：

- **首层子目录 = 模块**，每个必须有 `manifest.yaml`；缺失启动失败。
- 任意深度的子目录放 `api.ts`（dev）/ `api.js`（release）即成为一条路由；
  没有 `api` 文件的目录不是路由，可作共享工具目录（如 `_shared/`）。
- 同目录可放 `WS.ts` 产生一条 WebSocket 路由（第 3 章）。

### manifest.yaml（模块清单）

```yaml
name: "user"        # 必须等于父目录名（强校验，否则启动失败）
desc: "用户信息相关，记录账号、地址等个人信息"
version: "0.1.0"
# config: {}        # 可选，本模块的其他设置
```

`name` ≠ 目录名 → 启动报 `manifest name "x" != directory name "y"` 退出（防止模块名与路由脱节）。

### seed.sql（可选）

- 项目根存在即启动时对 `default` 库重放；**仅 default 库为 sqlite 时执行**
  （mysql/pg 的建库迁移归运维）。
- 语句按 `;` 切分 → **语句内不得含分号字面量**。
- 用 `INSERT OR IGNORE` 保证可重复执行（每次启动都重放）。
- `oj build` 不执行 seed（构建零磁盘副作用，db 用内存库）。

### dist 产物布局（预览）

```
dist/
├── manifests.yaml        # 模块 → 锁定版本（release 按此加载）
├── user-0.1.0/           # 版本目录 = <module>-<version>
├── user-0.1.0.tgz        # 确定性发布包
└── …                     # 多版本目录可共存
```

版本目录内部结构、锁语义与发布流程见第 10 章。

## 3. 编写 api.ts

> 何时读我：写第一个 handler 时。

### 动词 → 方法名映射

`api.ts` 导出一个对象，键是 HTTP 动词对应的方法名：

| HTTP 动词 | 方法名 |
|---|---|
| GET | `get` |
| POST | `post` |
| PUT | `put` |
| DELETE | `del` |
| PATCH | `patch` |
| HEAD | `head` |
| OPTIONS | `options` |

**DELETE 的方法名是 `del`，不是 `delete`**——写错时该请求返回 405
（`method DELETE not allowed`）。

### 两种 handler 写法

**写法一：同步函数 + `.then().catch()`**。方法体同步返回，异步调用走 Promise 链；
runtime 会泵 event loop 直到所有 Promise 落定后再写回响应（摘自 `sample/src/user/account/api.ts`）：

```ts
function get(): void {
  const id = Number(http.param("id", 0));
  db.query("select id, name, role from account where id = ?", [id])
    .then((r) => json.ok(r))
    .catch((e) => json.fail(500, String(e)));
}
export default { get };
```

**写法二：`async` 函数**（driver 会 `await fn()`；摘自 `sample/src/news/api.ts`）：

```ts
export default {
  async post(): Promise<void> {
    const b = http.body as { text?: string };
    await bus.publish("news", { text: b?.text ?? "hello" });
    json.ok({ published: true });
  },
};
```

两种写法等价可用；同一文件可混用。响应一律经 `json.ok` / `json.fail` 收口（第 6 章）。

### 请求体 http.body

解析规则：空 body → `null`；能按 JSON 解析 → 对象/数组；否则 → UTF-8 字符串。
multipart 请求时 `http.body` 是文本字段对象（`{name: value}`），文件走 `http.files`（第 5 章）。

```ts
const b = http.body as { name?: string };
if (!b.name) { json.fail(400, "name required"); return; }
```

### 路由：目录镜像与 `.route` 参数路由

URL = `{base}/{module}/{...path}/{feature}/` → `<dir>/{module}/{...path}/{feature}/api.ts|js`。
尾斜杠有无皆可；`{base}` 默认 `/v1/api`（`server.base` 可配）。

handler 函数挂 `.route` 属性即**替换**目录镜像路由，支持 matchit 语法
（摘自 `sample/src/user/item/api.ts`）：

```ts
function detail(): void {
  const id = Number(http.param("id", 0));
  if (!(id > 0)) { json.fail(400, "id required"); return; }
  db.query("select id, name, role from account where id = ?", [id])
    .then((r) => (r.length ? json.ok(r[0]) : json.fail(404, "no such account")))
    .catch((e) => json.fail(500, String(e)));
}
detail.route = "{id}";
export default { get: detail };
```

| 语法 | 匹配 | 示例 |
|---|---|---|
| `{id}` | 单段（不含 `/`） | `/user/item/42` → `http.param("id") === "42"` |
| `{*path}` | 尾部一段及以上（含 `/`） | `/file/a/b/c` → `http.param("path") === "a/b/c"` |
| `"/x/{id}"` | 以 `/` 开头挂到 base 根下 | 挂 `/x/{id}` 而非当前目录下 |
| `""` | 视同未挂载 | 目录镜像路由保留 |

`.route` 规则：

- 挂载后**目录镜像被替换**：`/user/item`（镜像路径）→ 404，只有参数路由可达。
- `{*path}` catch-all 至少匹配一段：`/file`（零段）→ 404。
- **参数段内不得混字面**：`{id}.json`、`v{major}.{minor}` 均属非法 pattern，
  启动时被丢弃并记日志 `InvalidParamSegment`。需要前缀/后缀字面的 URL 拆成静态多段，
  由 handler 自行校验扩展名。
- TS 项目把 `global.d.ts` 拷进源码根即可获得 `Function.route` 声明，编辑器不报错。
- `oj build` 会把 `.route` 从产物中剥离——**release 下路由事实唯一来源是构建生成的
  `routes.js`**（第 10 章）。

解析顺序：路由表（含 `.route` 参数路由）→ dev 目录镜像兜底（dev 模式）→
静态站点（`server.root`，仅 GET/HEAD）→ 404。API 永远优先于静态文件。
目录穿越 / 空段 / 非法段（`..`、`.`、`\`、NUL）→ 404。

### WS.ts（WebSocket 帧循环）

目录内放 `WS.ts`（dev）/ `WS.js`（release，约定同 `api.ts`）即产生一条 WebSocket 路由
`GET {base}/{...path}/ws`：`src/news/WS.ts` → `/v1/api/news/ws`；根级 `WS.ts` → `/v1/api/ws`。
同目录 `WS.ts` 与 `WS.js` 并存时 `.ts` 优先。连接升级后，**客户端每个文本帧执行一次本文件**
（帧内 `json.ok` 正常回信封；摘自 `sample/src/news/WS.ts`）：

```ts
bus.subscribe("news");
json.ok({ subscribed: true });
```

注意：release 下 root=dist，WS URL 含模块版本段（如 `…/news-0.1.0/ws`）——v0.2 已知限制
（第 12 章）。bus 的发布/订阅方向约定见第 5 章 bus 小节。

## 4. 导入解析

> 何时读我：import 报错或想抽公共代码时。

### 相对导入

`./x`、`../x` 自动补全，顺序：`.ts` → `.js` → `/index.ts` → `/index.js`。

```ts
import { positiveId, requireRole } from "../_shared/validate";   // → ../_shared/validate.ts
```

**跨模块相对导入**（如 `order` 引 `user` 的工具）：dev 下直接可跑；build 时改写为指向
目标模块版本目录的相对路径——目标模块未构建过则报错，先 `oj build user` 再
`oj build order`（摘自 `sample/src/order/list/api.ts`）：

```ts
import { requireRole } from "../../user/_shared/validate";
```

### 裸 specifier（node_modules）

`import { escapeHtml } from "escape-goat"` —— 从当前文件目录**逐级向上**找
`node_modules/<pkg>`（至 project root），按 `package.json` 的 `module` → `main` →
`index.js` 取入口；支持 `@scope/name` 与子路径 `pkg/lib/util.js`。

### CJS 互操作

CJS 包自动包装：`module.exports` → `default`；`require("pkg")` 走 `__ojRequire`
（进程级缓存）。启发式识别，**仅支持裸 specifier**；相对 `require("./x")` v0.2 不支持
（ESM 相对导入不受影响）。不读 `package.json` 的 `exports`/`conditions`；pnpm 布局不支持。

### 安全边界

所有解析结果被**钳制在 project root 内**——`..` 逃逸直接报错。

## 5. 全局对象 API 参考

> 何时读我：写任何 handler 期间，查签名与语义。

签名与 `global.d.ts` 一致（类型权威）。SQL 占位符方言：**sqlite / mysql 用 `?`，
postgres 用 `$1`**；值一律经参数数组绑定。

### 总表（13 组）

| 全局 | 说明 |
|---|---|
| `json.ok(data?)` / `json.fail(code, msg, data?)` / `json.header(name, value)` | 统一响应信封与响应头 |
| `http.method / query / headers / body / params / param / tenantId / user / files / file(i)` | 当前请求上下文（只读、懒加载、每请求最新） |
| `db.query / exec / table / tx` | 默认库（`db === DB("default")`） |
| `DB(name)` | 命名库实例（未配置的名字返回 `undefined`） |
| `kv.get/set/del/expire/incr` | KV 存储（配 `redis.default` → 真 Redis，否则进程内存 KV） |
| `redis.get/set/del/expire/incr` | 与 `kv` 同源同面（真连时二者同栈，auth 会话同库） |
| `blob.put/get/del/url/contentType`（可调用：`blob("name")`） | 对象存储（`blob:` 段启用） |
| `bus.publish / subscribe / kind` | 主题广播（HTTP 发布、WS 订阅） |
| `es.search / index / del` | Elasticsearch 薄客户端（`es:` 段启用） |
| `log.debug / info / warn / error` | 结构化日志 |
| `fetch(url, options?)` | 浏览器风格 HTTP 客户端 |
| `ws.send / close` | WebSocket 帧控制（HTTP 路径下 no-op） |
| `plugins()` | 已加载插件自省 + 宿主 ABI |
| 测试 SDK（`client.*` / `describe / it / expect / beforeEach` / `finish`） | **仅测试文件可用**，见第 8 章 |

### json —— 信封与响应头

| API | 签名 | 说明 |
|---|---|---|
| `json.ok` | `ok(data?: unknown): void` | 成功信封 `{code:0,msg:"ok",data}`，HTTP 200 |
| `json.fail` | `fail(code: number, msg: string, data?: unknown): void` | 失败信封，HTTP 状态 = `code`（`code<=0` 映射 500） |
| `json.header` | `header(name: string, value: string): void` | 设置响应头（同名后写覆盖） |

```ts
json.ok({ created: true });
json.fail(400, "name required");
json.fail(404, "no such account", { id });
json.header("X-Request-Id", "abc");
```

### http —— 请求上下文

| API | 类型 / 签名 | 说明 |
|---|---|---|
| `http.method` | `string` | 请求方法（`GET`/`POST`/…） |
| `http.query` | `Record<string, string>` | query 参数对象（form-urlencoded 解码：`+`→空格、`%XX`） |
| `http.headers` | `Record<string, string>` | 请求头对象 |
| `http.body` | `any` | 请求体（解析规则见第 3 章） |
| `http.params` | `Record<string, string>` | 路径参数对象（已 percent-decode；目录镜像路由下恒空） |
| `http.param` | `param(name: string, def?: unknown): any` | **路径参数优先，query 兜底**，均缺失返回 `def` 原值 |
| `http.tenantId` | `string \| null` | 租户 id（`tenant.enable` 时从租户头提取；未启用为 `null`） |
| `http.user` | `AuthUser \| null` | 已验签用户 `{id, roles, claims}`（auth 启用且过 Bearer 守卫；否则 `null`） |
| `http.files` | `UploadedFileMeta[]` | multipart 上传元信息 `[{field, filename, content_type, size}]`；非 multipart 为空数组 |
| `http.file` | `file(i: number): Promise<Uint8Array>` | 第 i 个上传文件的字节（越界报错 `no such file`） |

```ts
const id = Number(http.param("id", 0));   // /item/42?id=9 → "42"（路径优先）
const page = http.param("page", 1);       // 无路径参数 → query 兜底 → 默认 1
```

### db / DB(name) —— 数据库

| API | 签名 | 说明 |
|---|---|---|
| `db.query` | `query(sql: string, params?: unknown[]): Promise<Row[]>` | 参数化查询 → 行数组 |
| `db.exec` | `exec(sql: string, params?: unknown[]): Promise<number>` | 参数化执行 → 受影响行数 |
| `db.table` | `table(name: string): QueryBuilder` | 安全查询构造器（标识符白名单 + 参数化值） |
| `db.tx` | `tx(fn: (tx: DBInstance) => unknown): Promise<unknown>` | 事务（语义见下） |
| `DB(name)` | `(name: string) => DBInstance \| undefined` | 命名库实例；四方法与 `db` 同签名 |

**查询构造器**（流式、结构化；SQL 由服务端按库方言生成）：

```ts
const rows = await db.table("account")
  .select(["id", "name"])
  .where({ field: "role", op: "eq", value: "admin" })
  .where({ field: "id", op: "gt", value: 0 })     // 多个 where 之间 AND
  .orderBy([{ field: "id", dir: "desc" }])
  .limit(10)
  .offset(0)
  .all();                                          // → Promise<Json[]>
```

- `WhereCond`：`{ field: string; op?: string; value?: unknown; and?; or? }`。
  v0.2 服务端支持的操作符：`eq / ne / gt / gte / lt / lte / in（值须数组）/
  like / isnull`；未知操作符直接报错；`and`/`or` 嵌套字段 v0.2 未展开（多 where 即 AND）。
- `OrderByItem`：`{ field: string; dir?: "asc" | "desc" | null }`。
- 表名/列名经 SchemaRegistry 白名单校验（启动内省所得）——未知表/列报错；
  排序列另有可排序白名单。`limit` 缺省 100、硬上限 1000。
- 构造器自动按库方言出 SQL（sqlite/mysql/postgres），业务无需手写方言差异。

**事务 `db.tx`**：

```ts
await db.tx(async (tx) => {
  await tx.exec("update account set balance = balance - ? where id = ?", [50, 1]);
  await tx.exec("update account set balance = balance + ? where id = ?", [50, 2]);
  const rows = await tx.table("account").select(["id", "balance"]).all(); // 同连接读未提交
});
```

- 回调正常返回 → **提交**；throw / reject → **回滚**并把原错误抛给 handler。
- `tx` 与 `db` 的 `query / exec / table` **同签名**——事务内自动走同一连接，
  无需改写其余代码。
- 每请求**至多一个**活跃事务：嵌套 `db.tx` 报错 `transaction already active`；
  事务未完结时访问其它库报错（先结当前事务）。
- handler 忘记 `await` 或中途崩溃：请求结束时未完结事务**自动回滚**（服务端打 warn 日志）。

### kv / redis —— KV 存储

| API | 签名 | 说明 |
|---|---|---|
| `kv.get` | `get(key: string): Promise<string \| null>` | 取值（缺失 `null`） |
| `kv.set` | `set(key: string, value: string): Promise<boolean>` | 写值 |
| `kv.del` | `del(key: string): Promise<boolean>` | 删键 |
| `kv.expire` | `expire(key: string, ttlSec: number): Promise<boolean>` | 设过期（秒）；真 Redis 走 EXPIRE，内存 KV 惰性过期 |
| `kv.incr` | `incr(key: string): Promise<number>` | 自增返回新值（键不存在从 0 起） |

（`redis.get / redis.set / redis.del / redis.expire / redis.incr` 与上表同签名——二者同源同面。）

```ts
// 读穿缓存（摘自 sample/src/order/detail/api.ts，改编）
const hit = await kv.get(key);
if (hit !== null) { json.ok({ cached: true, data: JSON.parse(hit) }); return; }
```

`redis` 全局与 `kv` 同源：`redis.default` 配置即真连（fail-fast），此时两者都走 Redis，
auth 会话也在同一 Redis（多实例共享的前提）；未配置时均为进程内存 KV。

### blob —— 对象存储（`blob:` 段启用，未配置调用即报错）

| API | 签名 | 说明 |
|---|---|---|
| `blob(name?)` | 可调用取命名实例：`blob("media").put(...)`；裸调用 `blob.put(...)` 等价 `blob("default")` | 命名多后端对应 config `blob.backends.<name>` |
| `blob.put` | `put(key: string, bytes: Uint8Array, contentType?: string): Promise<boolean>` | 写对象（local 落盘 / s3 上传） |
| `blob.get` | `get(key: string): Promise<Uint8Array>` | 读对象（不存在报错） |
| `blob.del` | `del(key: string): Promise<boolean>` | 删对象（幂等：不存在视为成功） |
| `blob.url` | `url(key: string): Promise<string>` | 下载地址：local = `{base}/blob/{key}`；s3 = presigned URL（15min） |
| `blob.contentType` | `contentType(key: string): Promise<string \| null>` | Content-Type（local 读 sidecar / 按扩展名推断；缺失且无法推断返回空串；s3 无 Content-Type 返回 `null`） |

**上传四件套完整例子**（摘自 `sample/src/upload/api.ts`）：

```ts
export default {
  async post() {
    const f = http.files[0];                    // 1. 元信息
    if (!f) json.fail(400, "need a file field (multipart)");
    const b = await http.file(0);               // 2. 字节
    await blob.put(f.filename, b, f.content_type);  // 3. 存储
    json.ok({ key: f.filename, url: await blob.url(f.filename), size: b.length });  // 4. 下载地址
  },
  async del() {
    await blob.del(http.param("k", ""));
    json.ok({ ok: true });
  },
};
```

下载走内置公开路由 `GET {base}/blob/{key}`（免鉴权、不落业务表；local 直出字节 +
Content-Type，s3 302 跳 presigned URL）。key 按 `/` 分段白名单校验
（第 12 章路径安全）。上传体积上限 `server.max_upload_bytes`（超限 413）。

### bus —— 订阅发布

| API | 签名 | 说明 |
|---|---|---|
| `bus.publish` | `publish(topic: string, data?: unknown): Promise<number>` | 广播 JSON 帧 `{"topic":…,"data":…}` 给订阅该 topic 的**全部 WS 会话**，返回接收方数（无订阅返回 0） |
| `bus.subscribe` | `subscribe(topic: string): Promise<void>` | 当前 WS 会话订阅 topic（**HTTP 路径调用报错**——订阅对象是连接本身） |
| `bus.kind` | `kind(): Promise<string>` | 活跃 broker 类型：`"local"` / `"kafka"` / `"rabbitmq"`（异步 op，判等须 `await`） |

方向约定：**HTTP handler 发布，WS 会话订阅**。订阅在连接断开自动清除；同一会话重复订阅
幂等去重。`bus.kind()` 为异步，返回 `Promise<string>`，判等须 `await bus.kind()`。

```ts
// HTTP 发布（任意 api.ts）
const n = await bus.publish("news", { text: "hi" });
json.ok({ receivers: n });

// WS 订阅（WS.ts 内，见第 3 章 WS.ts 小节）
bus.subscribe("news");
```

缺省 `broker` 为进程内总线（跨实例不互通）；`broker.kind: kafka|rabbitmq` 需对应插件。

### es —— Elasticsearch（`es:` 段启用，未配置调用报 `es not configured`）

| API | 签名 | 说明 |
|---|---|---|
| `es.search` | `search(index: string, dsl?: unknown): Promise<Json>` | `POST {endpoint}/{index}/_search`，直通 ES 响应体 |
| `es.index` | `index(index: string, id: string, doc?: unknown): Promise<Json>` | `PUT {endpoint}/{index}/_doc/{id}?refresh=true`（写完即可查） |
| `es.del` | `del(index: string, id: string): Promise<Json>` | `DELETE` 同路径（幂等，缺失返回 404 体） |

index / id 限 `[a-zA-Z0-9_-]+`（防路径注入）；非 2xx 报错带 ES 返回体。

### log —— 结构化日志

| API | 签名 |
|---|---|
| `log.debug` | `debug(msg: string, ...kv: unknown[]): void` |
| `log.info` | `info(msg: string, ...kv: unknown[]): void` |
| `log.warn` | `warn(msg: string, ...kv: unknown[]): void` |
| `log.error` | `error(msg: string, ...kv: unknown[]): void` |

键值对交替传参（zap SugaredLogger 风格）：`log.info("order created", "id", id, "amount", amount)`。
输出经 `tracing-subscriber` 结构化打印（第 11 章日志）。

### fetch —— HTTP 客户端

签名：`fetch(url: string, options?: { method?: string; headers?: Record<string, string>;
body?: string | null }): Promise<OjFetchResponse>`。

返回浏览器风格 Response 子集：`ok / status / statusText / headers / json() / text() /
arrayBuffer() / clone()`。注意 v0.2 的 body 以字符串发送（非字符串值会被 `String()`
转换），二进制请求体请先自行编码（如 base64）。

```ts
const r = await fetch("https://api.example.com/v1/ping", { method: "GET" });
if (r.ok) {
  const body = await r.json();
  json.ok(body);
}
```

### ws —— WebSocket 帧控制

| API | 签名 | 说明 |
|---|---|---|
| `ws.send` | `send(data: string): void` | 向当前连接发一帧（HTTP 路径下 no-op） |
| `ws.close` | `close(): void` | 结束当前连接 |

仅在 `WS.ts` 帧循环内有意义（第 3 章）。

### plugins —— 插件自省

签名：`plugins(): any[]`。返回已加载插件清单
`[{name, semver, abi_version, fingerprint, host_abi_version}]`——用于升级核对窗口
（第 11 章插件升级）。

```ts
json.ok(plugins());
```

## 6. 响应信封与错误码

> 何时读我：设计错误返回或排查非 200 时。

统一信封 `{code, msg, data}`；**HTTP 状态码 = `code`**（`code=0` → 200；
`code<=0` 映射 500）。业务错误直接 `json.fail(400, "…")` 返回对应状态码，
无需另设错误通道。

| 场景 | HTTP | 信封示例 |
|---|---|---|
| 成功 | 200 | `{"code":0,"msg":"ok","data":…}` |
| 无路由匹配 / 目录穿越 | 404 | `{"code":404,"msg":"no route matched","data":null}` |
| 路径命中但动词未注册 | 405 | `{"code":405,"msg":"method DELETE not allowed","data":null}` |
| 非映射动词（`TRACE` 等） | 405 | `{"code":405,"msg":"method TRACE not allowed","data":null}` |
| 路由冲突（同 pattern 同方法双声明） | 500 | `{"code":500,"msg":"route conflict: GET /v1/api/user/{id} declared in a/api.ts and b/api.ts","data":null}` |
| TS 编译错误 / 模块解析失败 | 500 | `{"code":500,"msg":"…/api.ts: 语法错误…","data":null}` |
| handler 死循环 / 超时 | 408 | `{"code":408,"msg":"handler execution timed out","data":null}` |
| 上传超 `max_upload_bytes` | 413 | `{"code":413,"msg":"upload too large","data":null}` |
| blob 不存在 | 404 | `{"code":404,"msg":"blob not found","data":null}` |
| 证书过期进宽限期（仅 GET） | 403 | `{"code":403,"msg":"certificate expired: service available in grace period, but GET requests are restricted","data":null}` |
| 证书已过期（Expired，仅 GET，运行中热替换所致） | 403 | `{"code":403,"msg":"certificate expired: service unavailable","data":null}` |
| 租户头缺失/为空（`tenant.enable`） | 400 | `missing tenant header: X-TENANT-ID` |
| Bearer 缺失/无效/过期（`auth:` 启用） | 401 | `missing or invalid bearer token` |

业务层常用码约定：400 入参不合法、404 资源不存在、401 未认证、403 已认证但无权、
500 服务器内部错误。`json.fail` 的 `msg` 会原样进入信封，勿把内部细节（堆栈、SQL）
透给客户端。

## 7. 鉴权与多租户

> 何时读我：接口要登录态或多租户隔离时。

### auth：块存在即启用（两层能力）

config `auth:` 段存在即同时启用：**内置路由** + **Bearer 守卫**。

**内置路由**（`{base}/auth/*`，POST only，不占业务模块名空间）：

| 路由 | 请求体 | 响应 data |
|---|---|---|
| `POST /auth/login` | `{"username","password"}` | `{"access_token","refresh_token","expires_in"(秒),"user":{"id","roles"}}` |
| `POST /auth/refresh` | `{"refresh_token"}` | 同上（**轮换**：旧 refresh 立即失效） |
| `POST /auth/logout` | `{"refresh_token"}` | `null`（删 refresh session；access 到期前仍有效） |

**Bearer 守卫**：`{base}` 内非匿名路径必须带 `Authorization: Bearer <access_token>`
（缺失 / 验签失败 / 过期 → 401）。通过后 handler 里读 `http.user`
（`{id, roles, claims}`）。登录失败统一报 `invalid credentials`（不区分用户不存在/密码错）。

**匿名路径** `auth.anonymous_paths`：去 `{base}` 前缀的路径列表，尾 `/*` 为**一层**通配
（`/pub/*` 命中 `/pub/x` 但不命中 `/pub`）。`{base}` 之外的路径（静态站点、`/auth/*` 内置路由）
不设防。

**用户表** `auth.user_table`（默认 `users`）最小 schema：

```sql
CREATE TABLE IF NOT EXISTS users (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  username TEXT NOT NULL UNIQUE,
  password_hash TEXT NOT NULL,   -- bcrypt
  roles TEXT NOT NULL DEFAULT '[]'  -- JSON 数组串，如 '["admin"]'
);
```

auth 段其余字段（`jwt_secret` 生产必改且空串启动 fail-fast、`signing_method`、
access/refresh 时长）见第 9 章配置表。

```ts
// 受保护路由：读验签后的用户身份（sample/src/auth_demo/me/api.ts）
export default {
  get() {
    json.ok({ user: http.user });
  },
};
```

### tenant：多租户头

```yaml
tenant:
  enable: true
  header_key: "X-TENANT-ID"   # 默认即此名
```

启用后所有 `{base}` 请求必须带该 header（缺失/空 → 400），值注入 `http.tenantId`
供 handler 做数据隔离。**框架不自动改写 SQL**——行级过滤归业务（自行在查询里带
tenant 条件）。启用期间测试请求也必须带头（第 8 章两约束）。

### auth_demo 走读（sample）

`sample/src/auth_demo/`：`me`（受保护，返回 `http.user`）与 `health`（匿名，
在 `anonymous_paths`）两个路由；`sample/seed.sql` 已建 `users` 表并写入 demo 用户
（`demo` / `demo1234`，角色 admin）。走读顺序：login 拿 token → 带 Bearer 打
`/auth_demo/me/` → refresh 轮换 → logout 失效。sample 同时开了 tenant，
curl 需另带 `-H 'X-TENANT-ID: acme'`（完整命令见 `sample/src/auth_demo/README.md`）。

## 8. 测试

> 何时读我：写模块测试或搭 CI 时。

两层测试互补：**L1 `oj test`**（进程内真实 v8 + 真实路由/鉴权/租户管线 + 真实后端，
零 TCP）；**L2 vitest 纯 mock**（Node 直调 handler，mock 全局，毫秒级）。
一句话：**L2 测 handler 纯逻辑（快、稳），L1 测端到端行为（真、全）**。

### L1：`oj test`

目录约定：测试文件放 `tests/*.test.ts`（目录由 `-t/--tests` 指定，相对 config 所在目录，
默认 `tests`）。

```bash
cargo run -p oj -- test -c sample/config.yaml -d sample/src                 # human 摘要
cargo run -p oj -- test -c config.yaml -d src --format junit --output l1.xml # CI 报告
```

| 旗标 | 说明 |
|---|---|
| `-c/--config` | 配置文件（默认 `config.yaml`） |
| `-b/--base` | API 基础前缀覆盖（默认用 config `server.base`） |
| `-d/--dir` | 源码目录 `src` 或产物 `dist`（默认自动判定） |
| `-t/--tests` | 测试目录，相对 config 目录（默认 `tests`） |
| `--format` | `human`（默认）/ `tap` / `junit` / `json` |
| `--output` | 报告落盘文件；省略打到 stdout（机器格式 stdout 纯净） |

退出码：**全部通过 = 0，任一失败 = 1**——可直接做 CI 门禁。

测试文件用注入的全局 `client` 与 `describe/it/expect`（类型见 `global.d.ts`）：

```ts
describe("user account", () => {
  it("lists accounts (auth + tenant)", async () => {
    const token = await client.login("demo", "demo1234");
    const r = await client.get("/user/account", {
      headers: { Authorization: "Bearer " + token, "X-TENANT-ID": "default" },
    });
    expect(r.status).toBe(200);
    const body = JSON.parse(r.body);   // 信封：{ code, msg, data }
    expect(body.code).toBe(0);
    expect(Array.isArray(body.data)).toBeTruthy();
  });
});
```

注入的全局 API：

| API | 说明 |
|---|---|
| `client.get/post/put/del/patch/head/options(path, opts?)` | 进程内派发；`opts = { headers?, body? }`，返回 `ClientResp { status, headers, body, upgrade }`；`path` 相对 base（如 `"/user/account"`） |
| `client.login(username, password)` | POST 内置 `/auth/login` → 返回 `access_token`（失败抛错；自身无需认证头） |
| `describe(name, fn)` / `it(name, fn)` / `beforeEach(fn)` | vitest 风格子集 |
| `expect(actual)` | `.toBe / .toEqual / .toBeTruthy / .toBeFalsy / .toContain` |
| `finish()` | 标记会话结束 |

**两个必知约束（由 config 决定）**：

1. 多租户：config 启用 `tenant` 后，每个请求都必须带 `X-TENANT-ID` 头，否则 400。
2. 鉴权：config 启用 `auth` 后，除匿名路径外都要带 `Authorization: Bearer <token>`，否则 401。

> `beforeEach` 注册的是**单一全局钩子**，跨多个 `describe` 会被覆盖；多 describe 文件建议
> 在各 `it` 内联准备（如每个用例自己 `client.login`），避免互相干扰。

适用：API 端到端行为（路由/鉴权/租户/真实 DB/bus 广播/信封）与契约回归；
代价是启动开销，适合 CI 与关键链路守护。

### L2：vitest 纯 mock（`test/` 独立 npm 包）

```bash
cd test && npm ci && npx vitest run
```

结构：`mocks/oj-globals.ts` 提供 `installGlobals(opts?)`（把 `db/json/http/bus/log`
换成可控桩，返回响应捕获 `{code,msg,data}`；`lastPublished()` 取 `bus.publish` 记录）；
`invoke.ts` 提供 `invoke(handler, method, opts?)`（装桩 → 调 handler → flush 微任务 →
返回 `{ ...capture, published }`）；测试文件直接 import 真实 `../src/.../api` 的 handler。

```ts
import { describe, it, expect } from "vitest";
import account from "../src/user/account/api";
import { invoke } from "./invoke";

describe("user/account (L2 mock)", () => {
  it("get lists accounts from dbRows", async () => {
    const r = await invoke(account, "get", { dbRows: [{ id: 1, name: "neo", role: "admin" }] });
    expect(r.code).toBe(0);
    expect(r.data[0].name).toBe("neo");
  });

  it("post rejects invalid role → 400", async () => {
    const r = await invoke(account, "post", { body: { name: "x", role: "king" } });
    expect(r.code).toBe(400);
  });
});
```

适用：handler 纯逻辑单测（入参校验、响应塑形、bus 事件触发、纯函数）与 TDD 快速回归；
局限：mock 不是真实后端，发现不了鉴权/租户/路由装配等集成层问题。

### 选型与推荐组合

| 维度 | L1 `oj test` | L2 vitest |
|---|---|---|
| 运行时 | 真实 deno_core(v8) + axum | Node + vitest（无 v8） |
| 后端 | 真实 DB/KV/bus | mock 桩 |
| 速度 | 较慢（启动开销） | 极快 |
| 覆盖 | 路由/鉴权/租户/DB/总线 | handler 逻辑 |
| 稳定性 | 受后端/配置影响 | 稳定、无副作用 |

**推荐组合：开发期 L2 快速验证逻辑，CI 用 L1 守护端到端契约；两层都绿才有信心发布。**

CI 示例（GitHub Actions 片段）：

```yaml
steps:
  - name: L2 vitest
    working-directory: sample/test
    run: npm ci && npx vitest run
  - name: L1 oj test (junit)
    run: cargo run -p oj -- test -c sample/config.yaml -d sample/src --format junit --output l1.xml
  - name: Upload L1 report
    if: always()
    uses: actions/upload-artifact@v4
    with:
      name: l1-junit
      path: l1.xml
```

依赖管理：L2 的 vitest 声明在 `test/package.json` 的 `devDependencies`，与运行时依赖
（如 `escape-goat`）隔离——被测物不携带测试工具。

## 9. 配置 config.yaml

> 何时读我：起服务前定配置，或排查启动 fail-fast 时。

全字段可省（均有默认），除证书两路径**必配**。相对路径相对 **config 所在目录**；
命令行相对路径相对 CWD。

### server

| 字段 | 默认 | 说明 |
|---|---|---|
| `host` | `"localhost"` | 监听地址 |
| `port` | `778` | 代码默认属特权端口（<1024 需 root）；**生产用 ≥1024**（如 9778） |
| `base` | `"/v1/api"` | API 基础路由前缀；CLI `-b` 显式给出时覆盖；空前缀（空串/纯斜杠）拒绝启动 |
| `timeout` | `"30s"` | 单请求执行超时（超时熔断 → 408）；单位支持 `s/sec/secs/ms/m/min/h/d` |
| `pool_size` | `4` | JS 执行线程数 = 并行请求上限 |
| `max_upload_bytes` | `10485760`（10MB） | 上传体积上限；axum 层再乘 2 做硬顶（双闸，见第 12 章） |
| `root` | 无 | 静态站点根目录；**省略 = 不开静态服务**。API 未命中的 GET/HEAD 落此目录（目录 → `index.html`）；目录不存在启动即报错；穿越段（含 `%2F`）404；无 SPA 回退/Range/ETag |
| `logs_dir` | 无（= config 目录下 `./logs`） | 日志目录（终端输出完整镜像落盘；每次启动新建文件 `server-<启动秒>_<pid>.log`，按 `logs_max_m` 滚动、保留 `logs_keep_files` 个）；不存在自动创建
| `logs_max_m` | `100` | 单个日志文件大小上限（单位 M；**<100 按 100 生效**），超过滚动为 `base.1.log` 依次后移 |
| `logs_keep_files` | `10` | 日志文件保留个数（含活动文件，超出删除；最小生效值 2） | |
| `public_key_path` | **必配** | 证书校验公钥（SPKI PEM；仅验签，私钥不落服务器） |
| `certificate_path` | **必配** | JWS 证书（`Base64URL(Header).Payload.Signature`，RS256） |
| `grace_days` | `30` | 证书过期后宽限天数（缩窄可加速告警） |

### db —— 命名库（多库混用）

`name → DSN` 映射：`sqlite://<path>`（相对 config 目录，缺文件自动建空库）、
`sqlite::memory:`（仅测试，重启即丢）、`mysql://…`、`postgres://…`（原样透传）。

- 内置只有 sqlite/memory；`mysql`/`postgres` 需对应插件（oj-db-mysql / oj-db-postgres），
  未装插件 → 启动报 `unknown db scheme`。
- `DB("name")` 按名取库；查询构造器自动按库方言出 SQL；裸 SQL 占位符方言归业务
  （sqlite/mysql `?`，postgres `$1`）。
- `seed.sql` 仅对 **default 库且为 sqlite** 时重放。
- mysql/pg 连不上启动 fail-fast（连接串错/库未建）。

### redis

```yaml
redis:
  default: "redis://127.0.0.1:6379/1"
```

`default` **配置存在即真连**（启动 fail-fast：连不上直接报错退出，不静默退回内存），
需 oj-kv-redis 插件；未装插件 → 启动报 `no kv plugin loaded`。段缺失/全注释 →
进程内存 KV。真连后 `kv`/`redis` 全局与 auth 会话共享同一 Redis（多实例水平扩展的前提）。
仅 `default` 被使用，其余键 warn 忽略。

### es

```yaml
es:
  endpoint: "http://127.0.0.1:9200"   # 块存在即启用；尾斜杠自动剪除
```

存在即启用 `es.search/index/del`，需 oj-es 插件；缺失时调用报 `es not configured`。
config 声明了 `es` 但无 es 插件 → 启动 fail fast。

### blob —— 对象存储

```yaml
blob:
  driver: "local"        # local | s3（s3 需 oj-blob-s3 插件）
  root: "uploads"        # local 专用：存储根（相对 config 目录绝对化，缺目录自动建，需写权限）
  # s3：endpoint / access_key / secret_key 可选，bucket / region 必填（缺失 fail-fast）
  # path_style: true     # MinIO/自建 S3 需 true（默认 virtual-hosted）
```

块存在即启用 `blob.*` 全局与 `{base}/blob/{key}` 下载路由。命名多后端：
`blob.backends.<name>` 各写一段（与平铺字段互斥，并存且平铺非默认 → 歧义报错），
JS 侧 `blob("name")` 取用。

### broker —— 事件总线（三种 kind）

```yaml
broker:
  kind: kafka                       # local（默认，进程内）/ kafka / rabbitmq
  brokers: ["127.0.0.1:9092"]       # kafka 必需；rabbitmq 可省（取 url，再取 brokers[0]）
  # group: "oj-bus"                 # kafka 消费组默认
  # topic_prefix: "oj-bus"          # kafka 物理 topic 前缀 / rabbitmq 交换名，默认 oj-bus
  # url: "amqp://…"                 # rabbitmq 用
```

缺省（无 `broker:` 段）= 进程内 Bus（跨实例不互通）。`kind: kafka`/`rabbitmq` 需对应
插件，未装 → 启动报 `unknown broker kind`。

### tenant / auth

字段与语义见第 7 章（tenant 默认关闭、header 默认 `X-TENANT-ID`；auth 的
`jwt_secret`（空串启动 fail-fast，生产必改）、`signing_method`（HS256|HS384|HS512，
默认 HS256）、`access_token_duration`（默认 60s）、`refresh_token_duration`（默认 720h）、
`anonymous_paths`、`user_table`（默认 users））。

### plugins / plugins_dir —— 插件装配（双模式）

- `plugins:` 显式清单给出 → **严格按清单装配**（缺文件/身份不符/`@semver` pin 不符 fail fast）。
- 省略 → 扫描插件目录全部加载（目录不存在/为空 = 零插件，仅内置后端）。

`plugins_dir` 目录四级发现（先到先得），最终目录 = `<plugins_dir>/<host-triple>/`：

1. 环境变量 `OJ_PLUGINS_DIR`
2. config `plugins_dir`（相对 config 目录）
3. `<exe>/plugins`（`bin/oj` 旁即 `bin/plugins`）
4. `<workspace_root>/bin/plugins`

### 证书三字段：必配不可绕过

- `public_key_path` / `certificate_path` 缺任一 → 启动报错退出；**没有任何 config/CLI
  开关可跳过证书校验**；CLI `--cert-path` / `--key-path` 可覆盖路径，但不豁免校验。
- 证书有效期内正常；过期进入宽限期（`grace_days`，默认 30 天）→ **所有 GET 返回 403**
  （其余方法正常），替换证书即恢复，服务不中断。
- 宽限期结束再启动 → 记 ERROR 后进程退出（不服务）。
- 证书 / 公钥文件被覆盖即**热加载**（notify 事件驱动，无需重启）。

证书生成/续期见第 1 章与第 11 章。

### fail-fast 行为清单（启动即退，不等第一个请求）

| 触发 | 表现 |
|---|---|
| 证书两路径缺任一 | `certificate is mandatory but not configured` / `certificate not configured` 退出 |
| 证书过期且宽限期尽 | `certificate has expired and grace period elapsed` 退出 |
| `redis.default` 配置但连不上 | 报连接错误退出（不静默退回内存） |
| 未知 db scheme | `unknown db scheme` 退出 |
| db 方案冲突（内置与插件 scheme 交集） | fail fast 退出 |
| `broker.kind` 非法或声明但无对应插件 | `unknown broker kind` 退出 |
| `blob.driver` 非 local/s3；s3 缺 bucket/region | 启动报错退出 |
| `auth.jwt_secret` 为空串 | `auth.jwt_secret must not be empty` 退出 |
| config 声明 `es:` 但无 es 插件 | fail fast 退出 |
| `-d` 目录不存在 | 启动即报错 |
| 首层子目录缺 `manifest.yaml` | `missing manifest.yaml` 退出 |
| `manifest.yaml` 的 `name` ≠ 目录名 | `manifest name "x" != directory name "y"` 退出 |
| 版本目录名碰撞 | `version dir collision` 退出 |
| release：`manifests.yaml` 缺失/损坏/指向不存在版本 | 报错提示先 `oj build` |
| `server.root` 目录不存在 | 启动报错退出 |

## 10. 构建与发布

> 何时读我：dev 跑通后要交付时。

### `oj build [module] [-d src] [-o dist] [--no-minify]`

```bash
./bin/oj build -d src -o dist          # 全部模块
./bin/oj build user -d src -o dist     # 单模块
./bin/oj build user --no-minify        # 排障：多行可读产物（含内联 sourcemap）
```

每个模块构建为版本目录 `dist/<module>-<version>/`（版本读自模块 `manifest.yaml`；
同版本重建先清空旧目录）。构建零磁盘副作用（db 用内存库，不执行 seed）。

| 产物 | 说明 |
|---|---|
| `<module>-<version>/api.js` 等 | 全部 `.ts` 原路径换 `.js`（api.ts → 同目录 api.js）；默认转译 + minify 成单行（剥注释） |
| `<module>-<version>/routes.js` | 本模块路由表（pattern 无首斜杠、不含 base、含模块名段）；**`.route` 已从 api.js 剥离**——release 下路由事实唯一来源 |
| `<module>-<version>/manifest.yaml` | 原样复制 |
| `dist/manifests.yaml` | 模块 → 锁定版本（原子写，保留其他模块条目）；多版本目录可共存，锁决定 release 加载哪个 |
| `dist/<module>-<version>.tgz` | 确定性发布包（同输入字节一致，可校验完整性） |

跨模块相对导入构建期改写为指向目标模块版本目录的相对路径；目标模块未构建过则报错
（先 `oj build user`）。npm 依赖不打包进 tgz（裸 specifier 运行时沿 `node_modules`
解析，发布物需自带）。

### release 加载语义

`server -d dist`（含 `manifests.yaml` → 自动 release）按锁逐模块加载各版本目录的
`routes.js` 聚合路由；锁缺失/损坏、指向不存在的版本、任何条目非法 → 直接报错
（提示先 `oj build`）。**目录镜像路由在 release 不存在**——路由只认 routes.js，
所以发布前必须 `oj build`。

### 发行包布局与 deploy.sh

```bash
bash scripts/deploy.sh     # 构建（release）+ 打包 → dist/oj-v<version>.tar.gz
```

包内容（解包后）：

```
oj-v<version>/
├── oj                    # 主程序（独立二进制，deno_core 内嵌，无运行时依赖）
├── plugins/<host-triple>/# 插件 cdylib（oj-es、oj-db-*、oj-kv-redis、oj-blob-s3、oj-bus-*）
└── devkit/               # 本手册 + SKILL.md + global.d.ts（agent/编辑器类型提示）
```

目标机部署：解包 → 项目目录放 `config.yaml` + `dist/` + `seed.sql`（可选）+
vendored `node_modules/`（不打进 tgz）→ `./oj server -c config.yaml -d dist`。
启动时打印模块清单 + 路由表，可据此核对发布是否完整。

## 11. 运维要点

> 何时读我：服务跑起来之后的证书/日志/排障/升级。

### 证书生命周期

```bash
# 首次签发（三件：private.pem / public.pem / cert.jws）
cargo run -p oj-cert -- gen -o config --days 365
# 到期续签：用现有私钥重签（公钥不变，服务器无需换公钥）
cargo run -p oj-cert -- renew -k config/private.pem --days 365
```

- 私钥**仅签发端保管**；服务器只放 `public.pem` + `cert.jws`。
- CLI `--cert-path` / `--key-path` 仅覆盖路径，**不豁免校验**。
- 运行中替换证书文件 → **热加载**即时生效（无需重启）；重载失败保留旧状态并记 warn。
- 探测：`GET {base}/health` 实时返回证书状态（宽限/过期仍可访问，便于监控）：

```json
{ "status": "OK", "certificate_status": "valid|grace|expired",
  "certificate_expiry": "2027-01-01T00:00:00Z", "grace_remaining_secs": 123456 }
```

- 宽限语义：过期 → GET 全部 403（其余方法正常）；宽限尽再启动 → 进程退出。
  调大 `grace_days` 仅延长宽限、不改 `exp`。

### 热重载语义

| 变更 | 是否热生效 |
|---|---|
| dev 模式改 `api.ts` 及其依赖 | 是（mtime 版本化 specifier 失效缓存，下次请求用新代码） |
| release 同版本重建 dist（清场重写同目录） | 是（同样按 mtime 失效） |
| release 换版本（改锁指向新版本目录） | **否**——`dist/manifests.yaml` 仅启动时读取，需重启 |
| 证书 / 公钥文件被覆盖 | 是（notify 事件驱动，不轮询 mtime） |
| `config.yaml` | 否（重启生效） |
| `seed.sql` | 否（仅启动重放） |
| `manifest.yaml` 新增/删除模块 | 否（重启生效） |
| `node_modules` 新增包 | 否（已加载包缓存于进程，重启生效） |

### 超时与资源

- 超过 `server.timeout` 的 handler 被 `terminate_execution` 强杀，HTTP 返回 408；
  被杀的 JsRuntime **丢弃不回池**，server 不崩、后续请求正常（这是对死循环的唯一熔断手段）。
- `RuntimePool` 最大空闲 16，负载后自动收缩；`pool_size` 等于并行请求上限
  （过高吃内存，过低排队）。

### 日志

`tracing-subscriber` 结构化输出：启动横幅（模块/路由表）、请求日志（方法/路径/状态/耗时）、
handler 内 `log.debug/info/warn/error(msg, ...kv)`。生产用 `RUST_LOG` 控制级别：

```bash
RUST_LOG=oj=info ./oj server -c config.yaml -d dist
```

访问日志目录见第 9 章 `logs_dir`。

### 排障表（高频条目）

| 症状 | 原因 | 处置 |
|---|---|---|
| 启动报 `missing manifest.yaml` | 首层子目录缺清单或残留空目录 | 补齐 / 删除空目录 |
| 启动报 `manifest name "x" != directory name "y"` | `name` ≠ 目录名 | 对齐 |
| 启动报 `manifests.yaml … run oj build first` | release 锁缺失/损坏/指向不存在版本 | 跑 `oj build <module>` |
| 404 | 无对应 api 文件，或穿越/非法段 | 核对路径与 `-b` 前缀；release 确认模块在锁内 |
| 405 `method 'del' not exported` | DELETE 请求但没导出 `del`（不是 `delete`） | 改导出名 |
| 500 信封含 `api.ts` 字样 | TS 编译/解析错误 | 看 msg 定位行号 |
| 408 | handler 死循环/超时 | 查死循环，或调大 `server.timeout` |
| 413 | 超 `max_upload_bytes` | 调上限或压缩上传 |
| GET 全部 403 `certificate expired` | 证书进入宽限/过期 | 替换证书文件（热加载即时生效）；查 `{base}/health` |
| 启动报 `certificate …` 系列错误 | 缺路径/密钥不匹配/JWS 格式错 | 见第 9 章证书门禁；`oj-cert` 重签 |
| 启动报 `redis 'default': …` | Redis 不可达（fail-fast） | 起 Redis 或核对 URL；不想依赖就注释掉 `redis:` 段 |
| 400 `missing tenant header` | `tenant.enable` 且未带租户头 | 客户端补 header |
| 401 `missing or invalid bearer token` | auth 启用且路径不匿名 | 走 `/auth/login` 换 token |
| 500 `transaction already active` | 嵌套 `db.tx` | 合并为一个事务回调 |
| 日志 `open transaction … rolled back at request end` | `db.tx` 漏 await | 修 handler；数据已按未提交丢弃 |
| `bus.publish` 收不到广播 | bus 缺省为进程内，跨实例不互通 | 发布与订阅须同实例 |
| `GET {base}/…/ws` 404 | release 未重新 build，或 URL 含版本段 | 先 `oj build`；release URL 为 `…/news-0.1.0/ws` |
| 改 `api.ts` 不生效 | release 下 dist 未更新 / 换版本未重启 | 同步 dist；必要时重启 |
| `blob not configured` / `es not configured` | config 无对应段 | 加 `blob:` / `es.endpoint` |

完整排障表见 `docs/ops-manual.md` §7。

### 插件升级与回滚

- 插件替换用 `.new` / `.bak` **原子换名**；升级前 `cargo xtask plugin <name> --check`
  预检（ABI / 身份 / semver / 符号）。
- **ABI bump 部署顺序：先升插件到新 ABI 并验证，再升宿主**（或同版本原子升级）。
  升级窗口内可用 `plugins()` 自省核对（第 5 章）。
- 多版本共存回滚（代码）：`dist/` 内旧版本目录不被构建清除，回滚单模块 =
  把 `dist/manifests.yaml` 该模块指回旧版本 + 重启 server（锁仅启动时读）。
- 回滚（二进制）：换回上一版打包产物；保持二进制与 `dist/` 同版本发布。
- 升级前备份 sqlite 数据文件（`db.default` 路径）；配了 Redis/ES 的实例，它们的可用性
  进入启动契约（fail-fast），发布/巡检时先确认可达。

## 12. 安全红线与已知限制

> 何时读我：任何写 SQL / 拼路径 / 处理上传的时刻；发布前自查。

### SQL 注入红线（置顶，不可违反）

- **动态标识符（表名/列名）只来自 `db.table()` 查询构造器**（SchemaRegistry 白名单），
  **绝不来自 JS 字符串**。
- **值只通过绑定参数传递**（`db.query("... where id = ?", [id])`），**绝不字符串拼接**。

```ts
// 正确：标识符走构造器，值走绑定参数
const rows = await db.table("account").select(["id", "name"])
  .where({ field: "id", op: "eq", value: id }).all();

// 正确：裸 SQL 也必须参数化
await db.query("select id, name from account where id = ?", [id]);

// 错误：字符串拼接值（注入入口）
await db.query("select id from account where id = " + id, []);   // 禁止
```

排序/过滤白名单同理：构造器的表、列、可排序列都经白名单校验，绕过即失守。

### 路径安全

- 路由路径里的 `..` / `.` / `\` / NUL / 空段 → 404；静态兜底与 blob key 先 percent-decode
  再校验——解码出 `/`（`%2F` 走私）、`..`（`%2e%2e` 走私）同样 404。
- 路径参数（`http.param` / `http.params`）**已解码**（可含 `/`、`..` 字面）——仅用于
  参数化查询与类型转换，**勿拼接文件路径 / URL**。
- blob key 白名单：按 `/` 分段，`.` / `..` / `\` / NUL / 空段拒绝。下载路由
  `{base}/blob/{key}` **公开免鉴权**——不要把需鉴权的对象放进去。
- import 解析结果钳制在 project root 内，`..` 逃逸报错（第 4 章）。

### v0.2 已知限制全表

| 限制 | 说明 / 绕行 |
|---|---|
| 相对 `require("./x")` 不支持 | ESM 相对导入不受影响；CJS `require` 仅裸 specifier，不读 `exports`/`conditions`，不支持 pnpm 布局 |
| build 剥 `.route` 仅识别语句起始的标准赋值写法 | `fn.route = "…"` 顶层标准写法可用；花式写法可能漏剥 |
| npm 依赖不打包进 tgz | 发布物需自带 `node_modules/` |
| 旧版本目录不自动回收 | 锁不指向即为死数据，手工删 |
| 端口 778 属特权端口 | 实际用 ≥1024（如 9778） |
| `.tsx` / `.mts` 不转译 | 直通 V8；统一用 `.ts` |
| 静态站点无 SPA 回退 / 目录列表 / Range / ETag / 缓存头 | 未知路径不回落 `index.html`；未知扩展名按 `application/octet-stream`；SPA 回退经前置反代补 |
| release 下 WS URL 含版本段 | `…/news-0.1.0/ws`；客户端发现 WS 地址时注意拼版本段 |
| `db.tx` 每请求至多一个；嵌套报错 | 合并事务回调 |
| `bus` 缺省进程内，跨实例不互通 | 需要跨实例广播配 `broker.kind` |
| `WhereCond.and/or` 嵌套未展开 | 多个 `where()` 即 AND；复杂条件用 `db.query` 参数化 SQL |

### 常见陷阱清单

- `del` 不是 `delete`——DELETE 请求映射方法名 `del`，写错返回 405。
- `{id}.json` 混字面 pattern 非法——matchit 参数段不得混字面，拆成静态多段由 handler 校验。
- `seed.sql` 不得含分号字面量（按 `;` 切分）；用 `INSERT OR IGNORE` 保证幂等。
- postgres 占位符是 `$1`，sqlite/mysql 才是 `?`。
- 上传 413 双闸：`max_upload_bytes`（信封 413）+ axum 层 2x 硬顶（裸 413）——客户端看到
  的 413 未必带 oj 信封。
- 未配置 `blob:` / `es:` 段时调用 `blob.*` / `es.*` 即报错（配置即启用）。
- 挂 `.route` 后目录镜像路径 404（替换语义）；`{*path}` 至少匹配一段。
- `redis.default` 配置即真连且 fail-fast——CI/离线环境注释掉该段即用内存 KV。
- 每请求至多一个 `db.tx`；漏 await 会在请求结束时自动回滚并打 warn。
- `beforeEach` 是单一全局钩子，跨 `describe` 被覆盖——多 describe 文件在各 `it` 内联准备。
