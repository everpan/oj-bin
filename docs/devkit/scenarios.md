# 场景速查（照抄就能跑）

本文件是 `api-manual.md` 的**配套场景集**：不讲概念，只给「什么时候用 / 配置怎么写 /
代码怎么写 / 怎么验证 / 常见坑」。所有片段都可直接照抄到业务项目里。

> 阅读顺序：先 `api-manual.md` §1 快速开始跑通 Hello World，再回来按场景抄。
> 每个场景都标了对应手册章节，想深挖就跳过去。

| 场景 | 一句话 | 相关章节 |
|---|---|---|
| [1](#场景-1公开分享页要按租户读数据) | 链接里带 token，匿名访问但数据必须按租户隔离（`anonymous_paths` + `allow_as_tenant` + `db.asTenant`） | §6 db / §8 鉴权与多租户 |
| [2](#场景-2spa-深链刷新与每页-seo-标题) | 前端单页应用，刷新 `/space/7` 不 404，且每个路由有自己的 `<title>` | §10 配置 / §12 运维 |
| [3](#场景-3测试不脏开发库) | `oj test` 默认落 `db.test`，测试数据不污染开发库 | §9 测试 |
| [4](#场景-4列表分页与-limit-陷阱) | 「怎么只返回了 100 条」——LIMIT 策略可配 + 截断可观测 | §3 数据层 / §6 db |
| [5](#场景-5匿名路径怎么写) | `anonymous_paths` 的四种通配形态 + v0.1.20 迁移 | §8 鉴权与多租户 |

---

## 场景 1：公开分享页要按租户读数据

**什么时候用**：分享链接（浏览器直接打开、带不了自定义请求头）要展示某个租户的数据。
请求本身是「匿名」的（没带 `X-TENANT-ID`），但**数据仍必须按租户隔离**。

**红线先说**：租户 id **必须服务端派生**（从 token 查表得到），**绝不能**直接读 URL 上的
`?tenant=xxx` —— 那等于把租户开关交给调用方，防护形同虚设。

### ① 配置

```yaml
tenant:
  enable: true
  sql_guard: "deny"
  allow_as_tenant: true          # 默认 false：不给这个开关，db.asTenant 一律抛错
  anonymous_paths:
    - "/share/**"                # 免租户头的路径（去 API 前缀）：/share 及其下任意层
  shared_allow: [share_link]     # ② 分享 token 表是共享表（无 tenant_id）
```

```yaml
# src/share/schema.yaml
tables:
  share_link:                    # ① 共享表：schema 显式声明 + config.shared_allow 双声明
    tenant: false
    pk: token
    columns:
      token:     { type: text, null: false }
      tenant_id: { type: text, null: false }   # 它指向某租户，但自身不属于任何租户
```

> 共享表**必须两步都做**：`schema.yaml` 的 `tenant: false` + `config.yaml` 的
> `shared_allow`。只做一步会被当作受租户约束的表，启动时还会打 warn。

### ② handler

```ts
// src/share/detail/api.ts —— GET {base}/share/detail/{token}
export const get = async () => {
  const token = http.param("token", "");
  if (!token) return json.fail(400, "token required");

  // 第 1 步：用共享表把 token 换成租户（此时还没有租户身份，查共享表不违规）
  const links = await db.table("share_link")
    .select(["tenant_id"])
    .where({ field: "token", op: "eq", value: token })
    .all();
  if (!links.length) return json.fail(404, "link not found");

  // 第 2 步：声明身份。此后所有查询仍自动注入 tenant_id 条件，只是值来自这里。
  const scoped = db.asTenant(String(links[0].tenant_id));
  const rows = await scoped.table("order")
    .select(["id", "item"])
    .orderBy([{ field: "id", dir: "desc" }])
    .limit(20)
    .all();

  json.ok(rows);
};
get.route = "{token}";
export default { get };
```

要点：

- `db.asTenant(id)` 返回的实例与 `db` 同面（`table/query/exec/tx` 全都有），链式写下去即可。
- 声明后 `http.tenantId` 也会变成该值，日志/审计能看到真实生效的租户。
- **请求级、只能设一次**：同一个请求里第二次调用会抛错（防 handler 中途换身份）。
- 与 `db.asSystem()` 的区别：`asSystem` 是**完全关掉**租户防护（跨租户对账/报表用），
  `asTenant` 是**补上身份**、防护照旧。公开页要的是后者。

### ③ 验证

```bash
curl -s http://localhost:9778/v1/api/share/detail/abc123 | head -c 200
```

### ④ 常见坑

| 报错 | 原因 | 怎么办 |
|---|---|---|
| `db.asTenant is disabled: set tenant.allow_as_tenant=true ...` | 总开关没开 | `tenant.allow_as_tenant: true` |
| `db.asTenant is only allowed on anonymous requests ...` | 请求带了租户头，或路径没命中 `anonymous_paths` | 路径写进 `anonymous_paths`（注意是**去 API 前缀**后的路径）；确认没带 `X-TENANT-ID` |
| `db.asTenant: id must not be empty` | 查表没查到、传了空串 | 先判断查询结果再声明 |
| 400 缺租户头 | `anonymous_paths` 没命中，请求在进 handler 前就被拦了 | 检查通配形态（见[场景 5](#场景-5匿名路径怎么写)） |
| `raw sql lacks tenant_id on [order]` | 绕过构造器写了裸 SQL | 优先用 `db.table()`；裸 SQL 必须显式带 `tenant_id = ?` |

---

## 场景 2：SPA 深链刷新与每页 SEO 标题

**什么时候用**：前端是单页应用（Vue/React），产物放 `dist/`；用户直接打开或刷新
`/space/7` 时，服务端不该 404（要回落到 `index.html` 交给前端路由），同时希望每个路由
有自己的 `<title>` / description / og 标签给搜索引擎和分享预览用。

### ① 配置

```yaml
server:
  api_prefix: "/v1/api"        # 这个前缀下的 404 不会被回落吞掉（见坑 1）
  app_path: "dist"             # 静态站点根（相对 config 文件目录）
  app_spa_fallback: true       # 默认 false：不显式打开就没有深链回落
  html_meta: "__meta"          # 默认关闭：开启后按请求路径读 __meta/<path>.json 注入
```

### ② 产物布局

```
dist/
├── index.html                 # 前端产物（必须含 </head>，注入点在这里）
├── assets/app.js
└── __meta/                    # 元信息目录：**不对外公开**（直接访问返回 404）
    ├── index.json             # 对应 /（首页）
    ├── space/
    │   ├── 7.json             # 对应 /space/7
    │   └── 9.json
    └── about.json             # 对应 /about
```

`__meta/<path>.json` 支持的键（**只注入标签，绝不注入脚本**）：

| 键 | 产出 |
|---|---|
| `title` | `<title>…</title>` |
| `description` | `<meta name="description" content="…">` |
| `canonical` | `<link rel="canonical" href="…">` |
| `og:*` | `<meta property="og:…" content="…">` |
| `twitter:*` | `<meta name="twitter:…" content="…">` |

```json
{
  "title": "7 号工作区",
  "description": "7 号工作区的公开看板",
  "canonical": "https://example.com/space/7",
  "og:title": "7 号工作区",
  "og:image": "https://example.com/og/7.png",
  "twitter:card": "summary_large_image"
}
```

> `__meta/` 下的 JSON 由**构建期/离线脚本**生成（前端构建产物的一部分），oj 不做 SSR。
> 值里出现 `<` / `"` 会被 HTML 转义，不必手工处理。

### ③ 验证

```bash
curl -s http://localhost:9778/space/7 | grep -o '<title>[^<]*</title>'
# <title>7 号工作区</title>
curl -sI http://localhost:9778/__meta/space/7.json | head -1   # 404（元信息不公开）
```

### ④ 常见坑

| 现象 | 原因 |
|---|---|
| 深链仍然 404 | 忘了 `app_spa_fallback: true`（默认关） |
| 拼错的 API 路径返回 200 + 首页 HTML | 不会：`/v1/api` 前缀下的路径**不参与回落**，该 404 还是 404 |
| 某个前端路由没回落 | 路径带扩展名（如 `/space/7.json`）——带扩展名视为资源请求，不回落 |
| `curl -X POST` 不回落 | 只有 `GET` / `HEAD` 回落 |
| 没看到注入 | `dist/index.html` 里没有 `</head>`；或 meta 文件路径不对（`/space/7` → `__meta/space/7.json`，末段先 `set_extension("json")`）；或 Accept 头声明只要别的类型 |
| 首页没有 title | 缺 `__meta/index.json`（`/` 与目录首页都映射到 `index.json`） |

---

## 场景 3：测试不脏开发库

**什么时候用**：`oj test` 会跑真实 DB。默认落在开发库上会读到非种子数据、甚至写坏开发库。

### ① 配置

```yaml
db:
  default: "sqlite://db.sqlite"        # 开发/运行库
  test:    "sqlite://db_test.sqlite"   # ★ 测试库：声明了它，oj test 自动用它
```

### ② 运行

```bash
./bin/oj test -c sample/config.yaml -d sample/src
# stderr: oj test: using db "test"（migrate / seed / fixtures 一并跟随）

./bin/oj test -c sample/config.yaml -d sample/src --db ci_scratch   # 指定别的库（须在 db 段声明）
./bin/oj test -c sample/config.yaml -d sample/src --anonymous       # 以匿名身份跑（测公开面）
```

- 建表、seed、fixtures、以及 handler 里的 `db` **全部**跟着走，不会出现「表建在 A 库、
  查询打 B 库」。
- 没配 `db.test` 也没给 `--db` → 打一条 WARN 后继续用 `default`（不静默）。

### ③ 测一个公开面 handler

```ts
// tests/share.test.ts（`oj test --anonymous` 下运行）
describe("share", () => {
  it("匿名请求声明租户后只读到本租户数据", async () => {
    // 不开 --anonymous 时，这条请求在鉴权/租户管线就被拦下（400/401）
    const r = await client.get("/share/detail/abc123");
    expect(r.status).toBe(200);
    expect(JSON.parse(r.body).code).toBe(0);
  });
});
```

### ④ 常见坑

| 现象 | 原因 |
|---|---|
| 测试读到一堆陌生数据 | 没配 `db.test`，跑在 `default` 上（看启动那行 `oj test: using db ...`） |
| `--db foo` 启动报错 | `foo` 没在 config `db:` 段声明（键即库名） |
| 公开面用例仍 400/401 | 忘了 `--anonymous`；或 config 没开 `tenant.allow_as_tenant` |

---

## 场景 4：列表分页与 LIMIT 陷阱

**什么时候用**：接口返回列表，客户端抱怨「明明有 300 条只回来 100 条」。

### ① 默认行为

- 顶层 `select` **没写** `limit()` → 自动补 `db_query.default_limit`（默认 **100**）。
- 顶层 `select` **写了** `limit(n)` → 生效，并被 clamp 到 `db_query.max_limit`
  （默认 **1000**，硬顶 100000）。
- **嵌套/子查询不隐式截断**（只影响顶层）。

### ② 调整策略

```yaml
db_query:                # 注意：不能写进 db: 段（db 是 name→DSN 的 map）
  default_limit: 200     # 未给 limit 时的隐式上限
  max_limit: 2000        # 显式 limit 的 clamp 上界
```

配置错了**启动即报错**（不给静默回落）：`db_query.default_limit must be >= 1`、
`db_query.max_limit 200000 exceeds hard cap 100000`、`db_query.default_limit 30 > max_limit 20`。
键名写错（比如写成 `default`）同样会报错——不会静默用默认值。

### ③ 让客户端知道「可能被截断了」

当返回行数 ≥ 生效上限时，响应会带一个头：

```bash
curl -is 'http://localhost:9778/v1/api/order/list/' | grep -i x-oj-row-limit
# X-OJ-Row-Limit: 100
```

看到这个头 = **可能还有更多**（正好等于上限时无法区分「正好这么多」和「被截断」）。
正确做法是显式分页，别依赖默认值：

```ts
const page = Number(http.param("page", 1));      // 页码从 1 开始
const size = Math.min(Number(http.param("size", 50)), 200);
const rows = await db.table("order")
  .select(["id", "item"])
  .orderBy([{ field: "id", dir: "desc" }])       // 分页必须有稳定排序！
  .limit(size)
  .offset((page - 1) * size)
  .all();
json.ok({ page, size, rows });
```

> 分页少了 `orderBy` 会翻页重复/漏行（SQL 不保证无序结果的行序稳定）。

### ④ 常见坑

| 现象 | 原因 |
|---|---|
| 只返回 100 条 | 没写 `limit()`，吃了 `default_limit`；看 `X-OJ-Row-Limit` 头确认 |
| 写了 `limit(5000)` 只回来 1000 条 | 被 `max_limit` clamp（响应头同样会提示） |
| 配置了 `db_query` 但不生效 | 键名拼错（会启动报错，不是静默）、或写进了 `db:` 段 |
| `toSQL()` 里没有 LIMIT 文本 | LIMIT/OFFSET 是**绑定参数**，看 `params` 数组而不是 SQL 字符串 |

---

## 场景 5：匿名路径怎么写

**什么时候用**：`tenant.anonymous_paths`（免租户头）与 `auth.anonymous_paths`（免 Bearer）
都要填。典型是 OIDC 跳转腿：浏览器发起的 302 带不了自定义头，也带不了 Authorization。

### ① 四种通配形态

路径写的是**去掉 API 前缀**后的部分（`/v1/api/health` → `/health`）。

| 写法 | 含义 | 命中 | 不命中 |
|---|---|---|---|
| `/health` | 字面全等 | `/health` | `/health/x` |
| `/public/*` | 严格**一层** | `/public/a` | `/public/a/b` |
| `/idp/**` | **跨任意层** | `/idp`、`/idp/a`、`/idp/a/b/c` | — |
| `/report/*/export` | 中段 `*` 恰好一段 | `/report/7/export` | `/report/a/b/export` |

```yaml
tenant:
  anonymous_paths:      # 免租户头；命中且没带租户头 ⇒ 该请求为「匿名请求」
    - "/health"
    - "/idp/**"
auth:
  anonymous_paths:      # 免 Bearer
    - "/health"
    - "/auth/login"
    - "/idp/**"
```

> 两条列表是**独立的**：OIDC 回调这种「浏览器跳转腿」通常**两个都要加**，
> 只加一个仍会被另一道守卫拦下。

### ② v0.1.20 的行为变更（务必检查存量配置）

尾 `/*` 以前在 oj-auth 侧被当作「任意层前缀」，v0.1.20 起**统一为严格一层**
（与 server 侧对齐）。启动时对含尾 `/*` 的列表会打一条聚合 WARN：

```
warn: auth.anonymous_paths 有 2 条尾 "/*" 条目 ["/oidc/*", "/idp/*"]：自 v0.1.20 起为
严格一层通配；需要匹配多层路径请改写为 "…/**"
```

- 你的路径本来就只有一层（如 `/oidc/callback`）→ **无需改动**，WARN 只是提示。
- 你依赖它匹配更深路径（如 `/idp/.well-known/openid-configuration`）→ 改成 `/idp/**`，
  或把深路径单独列出来。

### ③ 验证

```bash
curl -s -o /dev/null -w '%{http_code}\n' http://localhost:9778/v1/api/health          # 200（匿名命中）
curl -s http://localhost:9778/v1/api/order/list/                                       # 400 缺租户头（未命中豁免）
curl -s -H 'X-TENANT-ID: acme' http://localhost:9778/v1/api/order/list/                # 200
```

### ④ 常见坑

| 现象 | 原因 |
|---|---|
| 豁免没生效 | 路径写成了**带前缀**的 `/v1/api/xxx`——应写去前缀后的 `/xxx` |
| 深路径仍 400/401 | 尾 `/*` 只匹配一层，改 `/x/**` 或逐条列出 |
| 加了租户豁免但还 401 | 两条列表独立：`auth.anonymous_paths` 也得加 |
| WS 路由想加进列表 | 不用加：`ws.ts` 是真实路由，天然不过前置管线 |

---

## 相关文档

- `api-manual.md` —— 完整 API 手册（13 章）
- `docs/tenant-guide.md`（仓库）—— 多租户白话指南
- `docs/testing.md`（仓库）—— L1/L2 两层测试选型
- `docs/mail-smtp.md`（仓库）—— 邮件投递完整手册
