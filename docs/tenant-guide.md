# 多租户（tenant）新手指南

这份文档用大白话讲清楚 oj 的多租户是怎么一回事、怎么开、开了之后写代码要注意什么。
适合第一次接触这个功能的读者；实现细节见 `docs/superpowers/specs/2026-09-13-tenant-sql-guard-design.md`。

## 一句话版本

开多租户后，每个请求都要带一个「我是哪个租户」的请求头。框架会记住它，并且（可选地）
**自动给你的 SQL 加上租户过滤条件**，让 A 租户的业务代码无论怎么写，都读不到 B 租户的数据。

## 租户从哪来

客户端每次请求带一个 HTTP 头，默认长这样：

```
X-TENANT-ID: acme
```

服务端收到后，把这个值放进 `http.tenantId`，handler 里随时能拿到：

```ts
const tid = http.tenantId; // "acme"
```

头缺失或为空 → 直接 400（除了配置的豁免路径，比如 OIDC 浏览器跳转腿）。
**注意：这个头目前是客户端自报的，框架只保证「请求内一致」，不保证「用户真的属于这个租户」。**
要防「用户伪造头越权」，需要把租户和登录凭证（JWT claims）绑起来，这步还在规划里（见设计文档 §7）。
在那之前，这套防护的正确姿势是：**每个租户发自己的应用凭证，网关/接入层按凭证固定注入租户头**。

## 两层开关：enable 和 sql_guard

`config.yaml` 里 tenant 段有两个层面，别搞混：

```yaml
tenant:
  enable: true          # 第一层：认租户头（不带头 400，http.tenantId 有值）
  sql_guard: deny       # 第二层：SQL 自动防护（见下）
```

- **`enable`** 只做「识别」：解析头、注入 `http.tenantId`。**它不改你的 SQL**，
  查什么数据完全靠 handler 自己写对。
- **`sql_guard`** 才做「防护」，三个档位：
  - `false`（默认）：关。数据隔离全靠业务代码自觉。
  - `"warn"`：自动注入照常，但不拦截——只把「漏了租户条件的查询」打到日志里。
    适合存量项目接入时先观察一段时间。
  - `"deny"`（或 `true`）：自动注入 + 拦截。漏了租户条件的查询直接报错。
    新项目建议直接上这个。

> 一个容易踩的坑：`enable: false` 而 `sql_guard` 开着——这时 `http.tenantId` 永远是 null，
> 防护形同虚设。启动时框架会打 warn 日志提醒你。

## 开了 sql_guard 之后，框架帮你做什么

以 `sql_guard: deny`、请求头 `X-TENANT-ID: acme` 为例：

1. **查（select）**：用 `db.table("order")` 构造器查询时，框架自动补上
   `order.tenant_id = 'acme'`。join 的表也一样（条件加在 ON 里，不影响 left join 语义），
   子查询、条件树递归生效。你自己写了 tenant 条件，也不会重复加。
2. **增（insert）**：框架强制把 `tenant_id` 写成当前租户。你想塞别的租户值 → 报错。
3. **改（update）**：框架自动把 `where` 收窄到当前租户；显式 `update({tenant_id: ...})`
   把行迁到别的租户 → 报错（这是最容易被忽略的越权写法）。
4. **删（delete）**：自动只删当前租户的行。
5. **裸 SQL（db.query / db.exec）**：框架扫描 SQL 文本，发现查询涉及的表
   「完全没提 tenant_id」就拦下（deny）或告警（warn）。这是尽力而为的检查——
   只防「彻底忘了写」，不验证你写的条件对不对；deny 档还要求参数数组里带上当前租户值。

## 建表要求：tenant_id 列

`sql_guard` 开着时，schema.yaml 里声明的表**默认都必须有 `tenant_id` 列**：

```yaml
tables:
  order:
    columns:
      id:    { type: integer }
      item:  { type: text }
      tenant_id: { type: text }   # 必须，漏了启动/构建/迁移都会报错
```

报错长这样，照着补列、或者把它声明成共享表（见下）：

```
schema: [order] 表 "order" 缺 tenant_id 列（tenant.sql_guard 启用中；
共享表请显式 tenant: false 并加入 config tenant.shared_allow）
```

校验挂在三处：`oj server` 启动、`oj build`、`oj migrate`——哪个入口都绕不过去。

## 共享表（字典表）怎么办

像国家代码、字典这类全租户通用的表，没有也不该有 `tenant_id`。声明成共享表：

```yaml
tables:
  dict:
    tenant: false        # ① schema 里显式声明
    columns:
      code: { type: text }
```

```yaml
tenant:
  shared_allow: [dict]   # ② config 白名单里点名（两步都要，缺一不可）
```

两步都做，框架才认它是共享表：不加租户条件、也不校验 tenant_id 列。
只做了①没做② → 按受租户约束处理，并且启动时打 warn 提醒你白名单没配。
**白名单故意做成 fail-closed**：忘了配只是回到「受约束」，不会变成「意外共享」。

## 逃生口：db.asSystem()

有些正当场景需要跨租户：系统级对账任务、运营后台全量报表、迁移脚本。
这时用显式逃生口：

```ts
const rows = await db.asSystem().table("order").select(["id"]).all();
```

- 只对本请求生效（下一个请求自动还原，不可能漏关）。
- 每次调用都打审计日志到服务端控制台——生产上盯这个日志。
- **别在普通业务 handler 里用**。review 时看到 asSystem 应该问一句「为什么」。

`oj test`（进程内测试运行器）没有租户头，deny 模式下要跑涉及租户表的用例，
也是在测试代码里 `db.asSystem()` 开一道。

## 常见报错对照表

| 报错信息 | 什么意思 | 怎么修 |
|---|---|---|
| 400 缺租户头 | 请求没带 `X-TENANT-ID`（或配的头名） | curl/前端补上头；测试代码用 `db.asSystem()` |
| `tenant guard: insert tenant_id mismatch` | insert 想写别人的租户 | 别传 tenant_id（框架自动填当前的），或确认你确实该用 asSystem |
| `tenant guard: update sets.tenant_id not allowed` | update 想把行迁到别的租户 | 删掉 sets 里的 tenant_id；跨租户迁移是系统操作，走 asSystem + 手工 SQL |
| `raw sql lacks tenant_id on [t]` | 裸 SQL 查了租户表但完全没提 tenant_id | 补 `where tenant_id = ?`（参数传 `http.tenantId`）；或改用 `db.table()` 构造器 |
| `schema: [m] 表 "t" 缺 tenant_id 列` | guard 开着但表声明没这列 | 补列；共享表则 `tenant: false` + `shared_allow` 双声明 |
| `warn: ... 共享表声明 "x" 未列入 tenant.shared_allow` | 声明了共享但白名单没点名 | config 补 `tenant.shared_allow: [x]`；不补则该表按受约束处理 |
| `warn: tenant.sql_guard ... 但 tenant.enable=false` | 开了防护但没开租户识别 | `tenant.enable: true`，或者关掉 sql_guard |

## 选型建议

- 单租户/内部工具：`tenant.enable: false`（默认），什么都不用管。
- 多租户但靠应用层自觉：只开 `enable`，不开 guard。适合快速验证，长期不建议。
- 多租户生产：**`enable: true` + `sql_guard: "deny"`**，共享表双声明，跨租户操作用 asSystem。
  存量项目先 `"warn"` 跑两周看日志，清完告警再切 deny。
