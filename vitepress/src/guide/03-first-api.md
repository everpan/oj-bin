---
title: 03 · 第一个接口
updated: 2026-09-08
---

# 03 · 第一个接口

这一节从零建一个模块，跑通「建表 → 写 handler → 访问 → 拿到信封」，
并把新手最常见的 404/405/401 一次说清。

## 目录就是路由

```
src/greeting/
├── manifest.yaml          # 模块身份 + 表归属
├── schema.yaml            # 声明表结构（可省）
├── seed.sql               # 幂等种子（可省）
└── api.ts                 # 业务入口
```

`src/greeting/api.ts` → `GET/POST/... /v1/api/greeting/`（前缀由 config 的 `server.api_prefix` 决定）。
想再深一层就再建目录：`src/greeting/detail/api.ts` → `/v1/api/greeting/detail/`。
**没有路由表文件**，目录镜像就是路由表。

## 写一个最小 handler

```ts
// src/greeting/api.ts
function get(): void {
  const name = http.param("name", "world");
  json.ok({ hello: name });
}

function post(): void {
  const b = http.body as { name?: string };
  if (!b.name) { json.fail(400, "name required"); return; }
  db.exec("insert into greet (name) values (?)", [b.name])
    .then(() => json.ok({ created: true }))
    .catch((e) => json.fail(500, String(e)));
}

export default { get, post };
```

要留意的四点（都是新人常问的「为什么」）：

1. **函数名即 HTTP 方法**：`get`/`post`/`put`/`del`/`patch`/`head`/`options`。
   没导出的方法 → 405，不是 404。
2. **`json.ok` 是终点不是 return**：它把结果写进响应信封并结束本次调用。
   早期 return 用 `json.fail(...) + return`。
3. **异步用 `.then/.catch` 或 async 都可以**，但**必须自己兜住异常** —— 未捕获的 rejection
   会变成 500 信封，看不出原始错误。
4. **SQL 值一律 `?` 占位 + 数组传参**。字符串拼接是红线；表名/列名也不能拼，
   动态标识只能来自 `SchemaRegistry`（用 `db.table()` 查询构造器）。

## 建表：声明式

```yaml
# src/greeting/schema.yaml
tables:
  greet:
    pk: id
    columns:
      id:   { type: integer, autoincrement: true }
      name: { type: text, null: false }
```

```yaml
# src/greeting/manifest.yaml
name: "greeting"
desc: "打招呼"
version: "0.1.0"
tables:
  - greet          # 与 schema.yaml 双向一致，漏写会在 build --check 报错
```

`tables:` 不是装饰：它是「表归属图」的来源，也是 `db.table()` 白名单的来源。
dev 模式启动会自动收敛（缺表建表、缺可空列加列、缺索引加索引），不用手写 CREATE TABLE。

## 跑起来

```bash
cargo run -p oj -- server -c sample/config.yaml --api-path sample/src
curl http://localhost:9778/v1/api/greeting/?name=oj
# {"code":0,"msg":"ok","data":{"hello":"oj"}}
```

如果你的 `config.yaml` 开了多租户（`tenant.enable: true`），还得带租户头：

```bash
curl -H 'X-TENANT-ID: default' http://localhost:9778/v1/api/greeting/?name=oj
```

## 三种「访问不到」的区别

| 现象 | 真实原因 | 怎么确认 |
|---|---|---|
| **404** | 路径没匹配上：目录名拼错、没有 `api.ts`（或 `api.js`）、文件不在 `--api-path` 指向的目录下 | 启动日志会打印路由表（方法 + pattern），对着看 |
| **405** | 路径对了，但该方法没导出（比如只导出 `get` 却发了 POST） | 给 `api.ts` 加一个 `options()` 返回支持的方法列表，自证 |
| **401** | 走到了鉴权守卫，token 缺失/过期/签名不对。sample 的业务端点默认全受保护 | 先打内置匿名端点 `/v1/api/health` 确认服务活着，再按[鉴权章节](./07-auth-tenant.md)取 token |

还有一个容易混淆的：**500 冲突**是路由表里有两条规则撞在一起（release 聚合多模块时
常见），不是你的代码报错。

## 用 sample 的 user 模块对照

官方最小完整模块是 `sample/src/user`：

```bash
curl 'http://localhost:9778/v1/api/user/account/?id=1'      # 查询（值走绑定参数）
curl -X POST -d '{"name":"morpheus","role":"admin"}' \
     http://localhost:9778/v1/api/user/account/             # 新增
curl http://localhost:9778/v1/api/user/item/1               # 路径参数 {id}
```

`item/api.ts` 演示路径参数：给函数挂一个 `route` 属性即可 —— `detail.route = "{id}"`。
注意挂上之后**目录镜像被替换**：`/v1/api/user/item/{id}` 可达，而 `/v1/api/user/item` 变 404。
取值用 `http.param("id")`。

## 下一步

- [04 · 模块解剖](./04-module-anatomy.md) —— 一个模块还能放什么
- 权威细节：[JS API 手册 · 编写 api.ts](../reference/api-manual/index.md)
