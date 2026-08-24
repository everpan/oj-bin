# oj server 用户手册

`oj` 是一个命令行工具：把按「模块 / 特性」目录组织好的 API 项目直接变成 REST 服务。
开发者在项目里按目录写 `api.ts`，`oj server` 把目录树原样映射成 HTTP 路由——改文件即生效
（dev 模式），编译产物可发布（release 模式）。

## 1. 快速开始

```bash
cargo build                     # 构建（debug）

# dev：直接跑 .ts 源码（目录无 manifests.yaml → 自动 dev/ts）
cargo run -p oj -- server -c sample/config.yaml -d sample/src

# release：先构建再跑产物 dist/（目录有 manifests.yaml → 自动 release/js）
cargo run -p oj -- build -d sample/src -o sample/dist
cargo run -p oj -- server -c sample/config.yaml -d sample/dist
```

启动时会打印模块清单与路由表，然后：

```bash
curl 'http://localhost:9778/v1/api/user/account/?id=1'
# → {"code":0,"msg":"ok","data":[{"id":1,"name":"neo","role":"admin"}]}
```

## 2. 命令与参数

```
oj server [-c config.yaml] [-b /v1/api] [-d <src|dist>]
oj build  [module] [-d src] [-o dist] [--no-minify]
```

| 参数 | 默认值 | 说明 |
|---|---|---|
| `-c` | `config.yaml` | （server）配置文件路径（host/port/base/root/db/redis） |
| `-b` | config `server.base`（默认 `/v1/api`） | （server）基础路由前缀，显式给出时覆盖 config（build 无此参数） |
| `-d` | `src` 存在取 `src`，否则 `dist` | 服务目录（server：模块树的根；build：源码目录） |
| `module` | 无 → 全部模块 | （build）要编译的模块名 |
| `-o` | `dist` | （build）产物目录 |
| `--no-minify` | 开（即默认 minify） | （build）关闭产物 minify，得到多行可读产物（排障） |

- `oj build`：**按模块**转译 src → `dist/<module>-<version>/`（版本从模块 `manifest.yaml`
  读取；同版本重建先清空旧目录）。构建零磁盘副作用（db 用内存库，不执行 seed）。
  - 产物**保留原名与目录结构**：全部 `.ts` 原路径换 `.js` 扩展（api.ts → 同目录
    `api.js`，仅多一步剥 `.route` 声明）；`manifest.yaml` 原样复制。
  - 转译产物默认 minify（单行、剥注释——含内联 sourcemap），`--no-minify` 关闭。
  - 生成模块内 `routes.js`（pattern 无首斜杠、不含 base、含模块名段；file 相对版本目录根）。
  - 更新 `dist/manifests.yaml`（模块 → 锁定版本，原子写，保留其他模块条目）——
    多版本目录可共存，锁文件决定 release 加载哪个。
  - 跨模块相对导入（如 order 引 `../../user/_shared/validate`）构建期改写为指向
    目标模块版本目录的相对路径；目标模块未构建过则报错（先 `oj build user`）。
  - 产出确定性 tgz：`dist/<module>-<version>.tgz`（同输入字节一致），用于整体发布。
- release 模式启动时按 `dist/manifests.yaml` 逐模块加载各版本目录的 `routes.js` 聚合路由；
  锁缺失/损坏、指向不存在的版本、任何条目非法 → 直接报错（提示先 `oj build`）。
- `-b` 已不是 build 参数（pattern 不含 base）；误用时显式报错退出。
- 命令行由 clap 解析：短旗标均有长形式（`--config/--base/--dir/--out`），
  `-h/--help`、`-V/--version` 随时可用；空参自动打印帮助（exit 2），非法参数
  （未知旗标、多余位置参数、未知子命令）直接报错退出。
- **模式自动判定**（server，无 `--dev` 旗标）：`-d` 目录含 `manifests.yaml`
  （构建锁）→ release 跑 `.js`；否则 dev 跑 `.ts`（改文件即生效）。目录不存在
  启动即报错。启动行会打印判定结果（`dev/ts` / `release/js`）。
- 相对路径（`-c`/`-d`）相对**当前工作目录**（CWD），不是相对 config 所在目录。

## 3. 配置 config.yaml

```yaml
server:
  host: "localhost"       # 监听地址（默认 localhost）
  port: 9778              # 监听端口（代码默认 778，但 macOS 特权端口不可用 → 用 ≥1024）
  base: "/v1/api"         # API 基础路由前缀（CLI -b 显式给出时覆盖；空前缀拒绝）
  timeout: "30s"          # 单请求执行超时（超时熔断 → 408）
  pool_size: 4            # JS 执行线程数（并发度）
  root: "public"          # 静态站点根目录（相对 config 目录；省略 = 不开静态服务）
db:
  default: "sqlite://db.sqlite"   # 命名库实例，可多库混用
  # analytics: "mysql://user:pass@127.0.0.1:3306/app"   # v0.2 多库
  # warehouse: "postgres://127.0.0.1:5432/app"
redis:
  default: "redis://127.0.0.1:6379/1"   # v0.1 仅 warn 并退回内存 KV
```

- `server` 字段全可省（都有默认值）。
- `db`：`name → DSN` 映射，多库可混用：`sqlite://<path>`（相对 config 目录）、`sqlite::memory:`、
  `mysql://…`、`postgres://…`（原样透传）。`DB("name")` 按名取库；构造器（`db.table()`）
  自动按库方言出 SQL。裸 SQL 的占位符方言归业务（sqlite/mysql `?`，postgres `$1`）。
- `redis`：`name → url` 映射。v0.1 **不真连 Redis**，仅打印 warn 并用进程内存 KV 模拟
  （`redis.get/set` 与 `kv.get/set` 同源）。
- `timeout` 支持 `s`/`sec`/`secs`/`ms`/`m`/`min`，如 `"30s"`、`"500ms"`。
- `server.root`：静态站点服务。API 路由（`-b` 前缀下）优先，未命中的 GET/HEAD 落到该目录
  按路径读文件（目录 → `index.html`）；目录不存在启动即报错。穿越段（含 `%2F` 编码）按 404。
- 项目根若存在 `seed.sql`，启动时对 `default` 库重放（语句按 `;` 切分，`INSERT OR IGNORE`
  可重复执行；**注意**：seed 内不得有分号字面量）。

## 4. 项目目录结构

```
<project>/
├── config.yaml          # 服务配置
├── seed.sql             # 可选，启动时对 default 库重放
├── src/                 # dev 服务目录（release 用 dist/，结构相同）
│   ├── user/            # 首层子目录 = 模块名
│   │   ├── manifest.yaml
│   │   ├── _shared/validate.ts   # 无 api 文件 → 纯工具代码目录，不产生路由
│   │   ├── account/api.ts        # → /v1/api/user/account/
│   │   ├── profile/api.ts        # → /v1/api/user/profile/
│   │   └── profile/detail/api.ts # → /v1/api/user/profile/detail/
│   └── order/
│       ├── manifest.yaml
│       ├── account/api.ts        # → /v1/api/order/account/
│       ├── list/api.ts           # → /v1/api/order/list/
│       └── detail/api.ts         # → /v1/api/order/detail/
└── node_modules/        # 裸 specifier 解析起点（见 §7 导入）
```

`oj build` 后的 `dist/` 结构（release 服务目录，**与 src 布局不同**）：

```
dist/
├── manifests.yaml              # 模块 → 锁定版本（release 按此加载）
├── user-0.1.0/                 # 版本目录 = <module>-<version>
│   ├── manifest.yaml           # 原样复制
│   ├── routes.js               # 本模块路由表
│   ├── _shared/validate.js     # 非 api.ts：原路径换 .js（minified）
│   ├── account/api.js          # api.ts：原名原目录（minified）
│   └── item/api.js
├── user-0.1.0.tgz              # 确定性发布包
└── …                           # 其他模块各自的版本目录（多版本可共存）
```

约束：
- **首层子目录 = 模块**，每个都必须有 `manifest.yaml`；缺了启动失败。
- 任意深度的子目录中放 `api.ts`（dev）/ `api.js`（release），即成为一条路由。
- 没有 `api` 文件的目录不是路由，可作共享工具代码目录（如 `_shared`）。

## 5. manifest.yaml（模块清单）

```yaml
name: "user"        # 必须等于父目录名（强约束，否则启动失败）
desc: "用户信息相关，记录账号、地址等个人信息"
version: "0.1.0"
# config: {}        # 可选，本模块的其他设置
```

## 6. 编写 api.ts

`api.ts` 导出一个对象，键是 HTTP 动词对应的方法名：

```ts
import { escapeHtml } from "escape-goat";      // 裸 specifier（node_modules）
import { requireRole } from "../_shared/validate";  // 相对导入

function get(): void {
  const id = Number(http.param("id", 0));
  db.query("select id, name from account where id = ?", [id])
    .then((r) => json.ok(r))
    .catch((e) => json.fail(500, String(e)));
}

function post(): void {
  const b = http.body as { name?: string };   // 请求体（JSON 自动解析为对象）
  if (!b.name) { json.fail(400, "name required"); return; }
  db.exec("insert into account (name) values (?)", [b.name])
    .then(() => json.ok({ created: true }))
    .catch((e) => json.fail(500, String(e)));
}

export default { get, post };
```

要点：
- **方法名**：`get/post/put/del/patch/head/options`（`DELETE` 映射为 `del`，不是 `delete`）。
- **同步函数 + `.then().catch()`**：方法体同步返回，异步 db 调用走 Promise 链；runtime 会
  泵 event loop 直到所有 Promise 落定后再写回响应。写 `async` 函数也可（driver 会 `await fn()`）。
- **请求体** `http.body`：空 → `null`；能解析为 JSON → 对象/数组；否则 → UTF-8 字符串。
- **响应** 用 `json.ok(data)` / `json.fail(code, msg, data?)`，见 §9。

## 7. 路由规则

URL = `{base}/{module}/{...path}/{feature}/` → `<root>/{module}/{...path}/{feature}/api.ts|js`。

| HTTP 动词 | 调用的方法 |
|---|---|
| GET | `get` |
| POST | `post` |
| PUT | `put` |
| DELETE | `del` |
| PATCH | `patch` |
| HEAD | `head` |
| OPTIONS | `options` |

- 路径任意深度：`/v1/api/user/profile/detail/` → `src/user/profile/detail/api.ts` 的 `get`。
- 尾斜杠有无皆可。
- 目录穿越 / 空段 / 非法段（`..`、`.`、`\`、NUL）→ **404**。
- **解析顺序**：路由表（含 `.route` 参数路由）→ dev 目录镜像兜底（dev 模式）→
  静态站点（`server.root`，仅 GET/HEAD，见 §3）→ 404。API 永远优先于静态文件。

### 7.1 路径参数路由（`.route`）

handler 函数挂 `.route` 属性即替换目录镜像，支持 matchit 语法：

```ts
function detail() { json.ok({ id: Number(http.param("id", 0)) }); }
detail.route = "{id}";          // /v1/api/user/item/{id}
export default { get: detail };
```

| 语法 | 匹配 | 示例 |
|---|---|---|
| `{id}` | 单段（不含 `/`） | `/user/item/42` → `http.param("id") === "42"` |
| `{*path}` | 尾部一段及以上（含 `/`） | `/file/a/b/c` → `http.param("path") === "a/b/c"` |

v0.1 的 matchit 被 axum 钉在 `=0.8.4`：**参数段内不得混字面**（`{id}.json`、
`v{major}.{minor}` 均属非法 pattern，启动时按设计 §5 丢弃并记日志
`InvalidParamSegment`）。需要"前缀/后缀字面"的 URL 拆成静态多段
（`/file/{name}` 由 handler 自行校验扩展名）。matchit 0.8.6 已放宽该限制，
axum 放开 pin 后可启用。

- 挂 `.route` 后**目录镜像被替换**：`/v1/api/user/item`（镜像路径）→ 404。
- `"{id}"` 相对当前目录；`"/user/{id}"` 以 `/` 开头挂到 base 根下；`fn.route = ""` 视同未挂。
- TS 项目在 `sample/global.d.ts` 声明 `Function.route` 消除编辑器报错（无 tsconfig 时
  TS 语言服务通常也能拾取；严格工程可在 tsconfig `include` 里显式列入）。
- dev（目录无 `manifests.yaml`，§2 自动判定）启动内省建表；release 用 `oj build` 生成的各模块版本目录内 `routes.js`
  按 `dist/manifests.yaml` 聚合直载（见 §2），`dist` 产物中的 `.route` 已被剥离——
  路由事实唯一来源是 `routes.js`。

## 8. 导入（import）

- **相对导入** `./x`、`../x`：自动补全 `.ts` → `.js` → `/index.ts` → `/index.js`。
- **裸 specifier** `import "escape-goat"`：从当前文件目录逐级向上找 `node_modules/<pkg>`
  （至 project root），按 `package.json` 的 `module` → `main` → `index.js` 取入口；支持
  `@scope/name` 与子路径 `pkg/lib/util.js`。
- **CJS 包**：自动包装互操作（`module.exports` → `default`；`require("pkg")` 走
  `__ojRequire`）。启发式识别，仅裸 specifier；相对 `require("./x")` 暂不支持（v0.1 限制）。
- 所有解析结果被**钳制在 project root 内**（`..` 逃逸报错）。

## 9. handler 可用全局对象

| 全局 | 说明 |
|---|---|
| `json.ok(data)` | 成功信封（`{code:0,msg:"ok",data}`），HTTP 200 |
| `json.fail(code, msg, data?)` | 失败信封，HTTP 状态 = `code`（`code<=0` 映射 500） |
| `json.header(name, value)` | 设置响应头（同名后写覆盖） |
| `http.method` | 请求方法字符串（`GET`/`POST`/…） |
| `http.query` | query 参数对象（`{id: "1"}`） |
| `http.headers` | 请求头对象 |
| `http.body` | 请求体（见 §6） |
| `http.params` | 路径参数对象（`{id: "42"}`，已 percent-decode） |
| `http.param(name, default)` | 取参数：**路径参数优先**，query 兜底（`http.params[name] ?? http.query[name] ?? default`） |
| `http.tenantId` | 租户 id（`tenant.enable` 时从 `header_key` 提取注入；未启用为 `null`） |
| `db.query(sql, params?)` | 参数化查询 → Promise<rows> |
| `db.exec(sql, params?)` | 参数化执行 → Promise |
| `db.table(name).select(cols).where(cond).orderBy(..).limit(n).all()` | 安全查询构造器（白名单+参数化） |
| `db.tx(async (tx) => { … })` | 事务：回调 resolve 提交 / throw 回滚再抛；`tx.query/exec/table` 同连接执行 |
| `DB(name)` | 命名库实例（`db === DB("default")`） |
| `kv.get/set/del(key)` | 内存 KV（v0.1 无真 Redis 时的缓存抽象） |
| `redis.get/set(key)` | 同内存 KV（与 `kv` 同源） |
| `log.debug/info/warn/error(msg, ...kv)` | 结构化日志 |
| `fetch(url, options?)` | 浏览器风格 HTTP 客户端 |
| `ws.send/close` | WebSocket 帧控制（HTTP 路径下 no-op） |

SQL 占位符：sqlite 用 `?`（参数数组按序绑定）。

### 事务（db.tx）

```js
await db.tx(async (tx) => {
  const n = await tx.exec("update account set balance = balance - ? where id = ?", [50, 1]);
  await tx.exec("update account set balance = balance + ? where id = ?", [50, 2]);
  const rows = await tx.table("account").select(["id", "balance"]).all(); // 同连接读未提交
});
```

- 回调正常返回 → **提交**；throw/reject → **回滚**并把原错误抛给 handler。
- 每请求**至多一个**活跃事务：嵌套 `db.tx` 报错（`transaction already active`）；
  事务未完结时访问其它库报错（先结当前事务）。
- handler 忘记 await 或中途崩溃：请求结束时未完结事务**自动回滚**（服务端打 warn 日志）。
- `tx` 与 `db` 的 `query/exec/table` 同签名——事务内自动走同一连接，无需改写其余代码。

路径参数已解码（可含 `/`、`..` 字面）——仅用于参数化查询与类型转换，**勿拼接文件路径/URL**；
单段参数解码后含 `/`（`%2F` 走私）按 404 拒绝。query 现按 form-urlencoded 解码
（`+`→空格、`%XX` 解码；旧版不解码，迁移注意）。

## 10. 响应信封与错误

统一信封 `{code, msg, data}`；HTTP 状态码 = `code`（`code=0` → 200）。

| 场景 | HTTP | 信封示例 |
|---|---|---|
| 成功 | 200 | `{"code":0,"msg":"ok","data":…}` |
| 无路由匹配 / 目录穿越 | 404 | `{"code":404,"msg":"no route matched","data":null}` |
| 路径命中但动词未注册（如 `DELETE`） | 405 | `{"code":405,"msg":"method DELETE not allowed","data":null}` |
| 非映射动词（`TRACE` 等） | 405 | `{"code":405,"msg":"method TRACE not allowed","data":null}` |
| 路由冲突（同 pattern 同方法双声明） | 500 | `{"code":500,"msg":"route conflict: GET /v1/api/user/{id} declared in a/api.ts and b/api.ts","data":null}` |
| TS 编译错误 / 模块解析失败 | 500 | `{"code":500,"msg":"…/api.ts: 语法错误…","data":null}` |
| handler 死循环 / 超时 | 408 | `{"code":408,"msg":"handler execution timed out","data":null}` |

业务层自定义错误用 `json.fail(400, "…")` 等直接返回对应状态码。

## 11. 样例走读（sample/）

`sample/` 是验收载体，两个模块 `user` / `order`：

- **user/account**：账号 CRUD（`get/post/put/del/patch/head/options` 全表），`_shared/validate`
  里 `positiveId`/`requireRole` 做参数校验（相对导入）。
- **user/profile** 与 **user/profile/detail**：单点查询 + 更名；detail 演示三级路径路由。
- **order/account**：建单时 `escapeHtml` 转义单号（裸 specifier `escape-goat`，vendored 在
  `node_modules/`）；按 `account_id` 查单。
- **order/list**：跨模块相对导入 `../../user/_shared/validate`（`requireRole`）+ `orders`/`account`
  join 联查。
- **order/detail**：`kv` 读穿缓存（命中返回 `cached:true`，未命中查库并回填）。
- **user/item**：路径参数路由（`detail.route = "{id}"`，§7.1）——`/v1/api/user/item/1` 查账号，
  镜像路径 404（替换语义）。
- **file**：catch-all 路由（`get.route = "{*path}"`）——`/v1/api/file/a/b/c` 拆段返回，
  `/v1/api/file` 404（catch-all 至少一段）。

跑法见 §1。验收用例见 `../oj/tests/e2e.rs`（UC-1…15，含 404/405/500/408 负向路径）。

## 12. 已知限制（v0.1）

- `redis` 不真连 Redis，退回内存 KV。
- 相对 `require()` 不支持；`build` 剥离 `.route` 仅处理语句起始的标准赋值写法（§7.1）。
- 相对 `require("./x")`、`package.json` `exports`/`conditions`、pnpm 布局不支持。
- npm 依赖不打包进 tgz（裸 specifier 运行时沿 `node_modules` 解析，发布物需自带）。
- 旧版本目录不自动回收（锁文件不指向即为死数据，手工删）。
- 端口 778（代码默认）在 macOS 属特权端口，实际需 ≥1024。
- `.tsx`/`.mts` 不转译（直通 V8）。
- 静态站点（`server.root`）无 SPA 回退（未知路径不回落 `index.html`）、无目录列表、
  无 Range/ETag/缓存头；未知扩展名按 `application/octet-stream` 下载。
