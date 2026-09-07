# sample 模块导览 —— 通过 12 个模块学会 oj

`src/` 下每个首层目录 = 一个后端模块。业务逻辑是 TS handler，目录即路由
（`src/user/account/api.ts` → `GET /v1/api/user/account/`），Rust 侧捕获统一的
`{code,msg,data}` 信封写回。本文件按「由浅入深」讲每个模块**教什么、看哪几个文件、
怎么跑通**。快速启动与部署命令见 [README.md](README.md)。

> 测试账号（`_platform` 模块 seed）：`demo` / `demo1234`（admin 角色）、
> `trinity` / `demo1234`（user 角色）。除内置 `/v1/api/health`（匿名健康检查）与
> `/auth`、`/oidc`、`/idp` 路径外，**所有业务端点都受保护**：请求需带
> `-H "Authorization: Bearer $TOKEN"` 与 `-H 'X-TENANT-ID: default'`（多租户头）。
> 下文示例统一用 `$AUTH` 指代这两个头，token 获取见 §④：

```bash
TOKEN=$(curl -s -H 'X-TENANT-ID: default' \
  -d '{"username":"demo","password":"demo1234"}' \
  http://localhost:9778/v1/api/auth/login \
  | python3 -c 'import json,sys;print(json.load(sys.stdin)["data"]["access_token"])')
AUTH=(-H "Authorization: Bearer $TOKEN" -H 'X-TENANT-ID: default')
```

## 学习路径

```
① user      最小完整模块：路由 / CRUD / 共享代码
② file      通配路由
③ order     缓存 / 跨模块表与代码复用 / 第三方 npm 包
④ auth_demo + auth   受保护路由与 http.user → JWT 业务端点
⑤ news      WebSocket + 发布订阅
⑥ upload    文件上传与对象存储
⑦ admin     综合实战（多表 / 动态菜单 / 路由覆写）
⑧ cert      证书管理业务（角色门禁 + crypto）
⑨ idp + oidc 内置 OIDC 提供方与依赖方（json.raw 裸 JSON）
⑩ _platform 无路由的"表共享"模块（架构概念）
```

---

## ① user —— 一切从这里开始

**教什么**：目录镜像路由、`db` 参数化 CRUD、`json` 信封、`_shared` 共享代码、
`schema.yaml` 声明表、`seed.sql` 幂等数据。

| 文件 | 讲解 |
|---|---|
| `manifest.yaml` | 模块身份：name/desc/version + `tables: [account]`（表归属声明，与 schema.yaml 双向一致 S005） |
| `schema.yaml` | 声明式表结构：account 表的列/主键/索引——启动时自动收敛，不用手写 CREATE |
| `seed.sql` | 幂等种子（`INSERT OR IGNORE`），每次启动重放：neo / trinity 两个账号 |
| `account/api.ts` | 完整 get/post/put：`db.query(sql, params)` 值走绑定参数（防注入红线），`json.ok` / `json.fail` 信封 |
| `item/api.ts` | 路径参数：`detail.route = "{id}"` → `/v1/api/user/item/{id}`，`http.param("id")` 取值 |
| `profile/detail/api.ts` | 目录任意深度嵌套成三层路由，仅 4 行 |
| `_shared/validate.ts` | 模块内共享工具（`requireRole`），被本模块和 order 模块同时 import |

```bash
curl "${AUTH[@]}" 'http://localhost:9778/v1/api/user/account/?id=1'
curl "${AUTH[@]}" http://localhost:9778/v1/api/user/item/1
curl "${AUTH[@]}" -X POST -d '{"name":"morpheus","role":"admin"}' \
  http://localhost:9778/v1/api/user/account/
```

## ② file —— 通配路由

`api.ts` 里 `get.route = "{*path}"`：`/v1/api/file/a/b/c` → `path="a/b/c"`（至少一段，
`/v1/api/file` 本身 404）。演示 oj 路由的最后一种形态（精确 / `{param}` / `{*path}`）。

## ③ order —— 缓存、跨模块复用、第三方包

| 文件 | 讲解 |
|---|---|
| `manifest.yaml` | `deps: { user: "^0.1.0" }`：SQL 里 join 了 `user` 模块的 `account` 表，必须显式声明（表归属守卫，`ownership_guard: deny` 下违规直接拒绝） |
| `detail/api.ts` | cache-aside 模式：`kv.get` 未命中 → 查库 → `kv.set` 回填，响应里带 `cached` 标记 |
| `account/api.ts` | `import { escapeHtml } from "escape-goat"` —— vendor 的纯 ESM npm 包直接参与请求处理 |
| `list/api.ts` | 裸 SQL join + **跨模块相对导入** `import { requireRole } from "../../user/_shared/validate"`（代码复用与表 deps 双演示） |

```bash
curl "${AUTH[@]}" -X POST -d '{"account_id":1,"amount":9.9,"no":"A001"}' \
  http://localhost:9778/v1/api/order/account/          # 建单
curl "${AUTH[@]}" http://localhost:9778/v1/api/order/detail/1   # 第二次起 cached:true
curl "${AUTH[@]}" 'http://localhost:9778/v1/api/order/list/?role=admin'
```

## ④ auth_demo + auth —— 鉴权两兄弟

- **auth_demo** 是最小受保护路由：`me/api.ts` 需 `Authorization: Bearer <token>`，
  handler 里 `http.user` = `{id, roles, claims}`（验签后的身份，由 oj-auth 插件守卫
  注入）；`health/api.ts` 同为受保护路由（不带 token → 401 信封——先试试这个
  401，再带 token 对比）。真正匿名的只有框架内置 `/v1/api/health`（证书状态）与
  config `auth.anonymous_paths` 列出的路径。
- **auth** 是 JWT 端点的 **JS 业务实现**：`login`（bcrypt.verify 校验
  `_platform.users` 口令 → `jwt.sign` 签 access token → refresh 落 kv 会话）、
  `refresh`（轮换制）、`logout`。守卫（谁放行）是 Rust 插件，签发（发什么）是
  JS handler——两层职责分离。

```bash
curl "${AUTH[@]}" http://localhost:9778/v1/api/auth_demo/me/
# {"code":0,"data":{"user":{"id":"1","roles":["admin"],...}}}
curl -H 'X-TENANT-ID: default' http://localhost:9778/v1/api/auth_demo/health/   # 不带 token → 401
```

## ⑤ news —— WebSocket + 发布订阅

> WebSocket 专题教学见 [../docs/websocket.md](../docs/websocket.md)。

- `ws.ts`：**帧循环**文件——客户端连 `/v1/api/news/ws` 后每发一帧文本就执行一次；
  首帧 `bus.subscribe("news")` 订阅主题。WS 也是普通路由文件，走同一转译管线。
- `chat/ws.ts`：**帧内发布**聊天室——WS 帧里直接 `bus.publish`，任意连接发帧即广播
  给所有订阅者（发 `{"join":1}` 进房，发 `{"from":"neo","text":"hi"}` 聊天）。三条帧内
  发布语义（自回声 / 顶层无 await / 块作用域）见 [../docs/websocket.md](../docs/websocket.md) §2。
- `api.ts`：`POST /v1/api/news` → `bus.publish("news", {...})` 广播到所有订阅连接
  （含其它实例——bus 后端可换 kafka/rabbitmq 插件）。

```bash
# 终端 1：websocat ws://localhost:9778/v1/api/news/ws  （发任意一帧完成订阅）
# 终端 2：
curl "${AUTH[@]}" -X POST -d '{"text":"hello oj"}' \
  http://localhost:9778/v1/api/news    # 终端 1 收到 {"topic":"news",...}
```

## ⑥ upload —— multipart + blob 对象存储

`api.ts`：`http.files` / `http.file(0)` 取 multipart 上传，`blob.put/url/del` 读写
对象（driver 可换 local / s3）；`GET {base}/blob/{key}` 是内置公开下载路由
（不落业务表）。业务 handler 只管存取，存储介质由 config 切换。

```bash
curl "${AUTH[@]}" -F file=@./README.md http://localhost:9778/v1/api/upload/
curl http://localhost:9778/v1/blob/README.md      # 内置下载路由（匿名公开）
```

## ⑦ admin —— 综合实战（react-antd-admin 后端移植）

最大的模块：4 张表（role / menu / role_menu / notification）+ 多个带状态接口，
是 react-antd-admin 前端的后端移植。值得细看的点：

- **整模块 `.route` 覆写到 base 根**：`get.route = "/menu-list"`、`"/user-info"`、
  `"/home/pie"`……挂了 `.route` 后目录镜像 URL 被替换——以 `/` 开头挂到 `{base}`
  根（`/home/pie` → `/v1/api/home/pie`，`/v1/api/admin/home/pie` 实测 404）；
  相对段 `{id}` / `{*path}` 则替换目录段（见 user/item）。
- `user-info/api.ts` 读 `_platform.users` → manifest `deps: _platform` 声明（跨模块表
  守卫的第二个示范；`ownership_guard: deny` 下没声明就 500 附修复指引）。
- `get-async-routes` / `menu-by-role-id`：按 `http.user.roles` 返回动态菜单/路由——
  前端按角色渲染的真实后端形态。
- 多表 schema.yaml（role/menu/role_menu/notification）+ 各自 CRUD handler 的组合范本。

```bash
curl "${AUTH[@]}" http://localhost:9778/v1/api/menu-list       # 菜单树
curl "${AUTH[@]}" http://localhost:9778/v1/api/user-info       # 跨模块读 _platform.users
curl "${AUTH[@]}" http://localhost:9778/v1/api/get-async-routes
```

## ⑧ cert —— 证书管理业务

对 `certs` 表的完整管理端：生成 RSA 密钥对 + JWS 证书入库、renew 重签、列表带
`status`（有效/宽限/过期）。handler 用 `isAdmin()` 演示**角色 gate**写法——
demo（admin）200、trinity（user）实测 403。
它管理的就是 oj 自身启动门禁用的那类证书（config `certificate_path`），业务与
框架互为注脚。

## ⑨ idp + oidc —— 内置 OIDC 提供方与依赖方

- **idp**（OP，OpenID Provider）：`discovery / jwks / authorize(PKCE) / login /
  token / userinfo` 全套标准端点，RS256 签名走内置 `oidc` 全局对象。
  `openid-configuration/api.ts` 用 **`json.raw`** 直接出裸 JSON——标准协议端点
  不包 `{code,msg,data}` 信封的正确姿势。
- **oidc**（RP，Relying Party）：`login → 302 到 OP authorize → callback 换
  token`，对接任意标准 IdP 只改 config `oidc.rp`；会话桥接复用 `auth/_shared`。

完整 curl 接力（302 手动跟 Location）见 [README.md](README.md)「OIDC 演示」。

## ⑩ _platform —— 没有路由的"表共享"模块

框架级共有表（`tenant` 多租户、`users` 鉴权账号）上收到一个**无 api 文件**的模块：
不产生任何路由，只贡献表归属（归属图单源）与 seed 数据（demo/trinity 账号、
两个示例租户）。谁要用它的表（auth 登录、admin user-info、idp 校验口令），
谁就在 manifest 里 `deps: _platform`——这是 oj 模块自治模型里「共享数据如何归属」
的答案。

## ⑪ src/tasks —— 长任务池（v0.1.6）

- **task_demo.ts**：与 broker 无关的最小心跳任务——`while (!tasks.stopping()) {
  await tasks.sleep(1000); log.info("demo tick"); }`。启动服务即随进程拉起：

  ```
  task: demo (task_demo.ts) → started
  task: 1 task(s) → started
  ```

- 命名约定：`task_{name}.*` / `{name}_task.*`（递归扫描；其余文件是共享库，
  不执行）。每任务一条专用线程 + 独立 V8 runtime；崩溃 1s→2s→…（cap 60s）
  退避重启；Ctrl-C → `tasks.stopping()` 置位 → 宽限内自然收场（不退者被看门狗
  强杀）→ `task: demo → stopped`。
- 消费型任务（Kafka/RabbitMQ poll/commit/ack）配 `config.yaml` 的 `kafkas:`/
  `rabbits:` 段使用；写法见 `docs/devkit/api-manual.md` §6「命名 MQ 客户端与长任务」。

---

## 特性 → 模块速查

| 想学什么 | 去哪个模块 | 入口 |
|---|---|---|
| 目录镜像路由 / 三层嵌套 | user | `profile/detail/api.ts` |
| 路径参数 `{id}` / 通配 `{*path}` | user · file | `item/api.ts` · `file/api.ts` |
| 绝对路径挂载 `get.route` | admin | `home/pie/api.ts` |
| `db` 参数化 CRUD / 声明式表 | user · admin | `account/api.ts` + `schema.yaml` |
| 跨模块表（deps 声明） | order · admin · auth | 各 `manifest.yaml` |
| 跨模块代码复用 | order | `list/api.ts` |
| 第三方 npm 包（vendor ESM） | order | `account/api.ts` |
| kv 缓存 / 会话存储 | order · auth | `detail/api.ts` · `_shared/session.ts` |
| JWT 签发与校验（bcrypt/jwt/crypto） | auth | `login/api.ts` + `_shared/session.ts` |
| `http.user` 身份 / 角色门禁 | auth_demo · admin · cert | `me/api.ts` · `cert/api.ts` |
| WS 帧循环 + bus 发布订阅 | news | `ws.ts` + `api.ts` |
| multipart + blob 对象存储 | upload | `api.ts` |
| `json.raw` 裸 JSON（协议端点） | idp | `.well-known/openid-configuration/api.ts` |
| OIDC OP / RP 全流程 | idp · oidc | 见 README「OIDC 演示」 |
| 长任务池 / `tasks.stopping/sleep` | src/tasks | `task_demo.ts` |
| Kafka/RabbitMQ 命名客户端 | src/tasks + config | `kafkas:`/`rabbits:` 段（消费见 api-manual §6） |
| seed.sql / schema.yaml / migrations | user · order · _platform | 各模块目录 |

## 数据层文件速览

| 模块 | schema.yaml | migrations/ | seed.sql | fixtures/ |
|---|---|---|---|---|
| _platform | tenant + users | ✓ | 账号 + 租户 | ✓（oj test 灌入） |
| user | account | ✓ | neo/trinity | |
| order | orders | ✓ | ✓ | |
| admin | role/menu/role_menu/notification | ✓ | ✓ | |
| cert | certs | ✓ | | |

（auth / auth_demo / file / idp / news / oidc / upload 无表，纯路由模块。）

迁移账本 = 单表 `_oj_migrations`（module 列区分模块）；结构演进规则与运维手册见
[../docs/migration.md](../docs/migration.md)。
