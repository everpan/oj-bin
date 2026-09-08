---
title: 05 · 全局对象速查
updated: 2026-09-08
---

# 05 · 全局对象速查

oj 没有 `import` 任何 SDK：全局对象在 runtime 启动时就被注入好了，直接用。
这一节是**分诊表** —— 先按「我想干什么」找到对象，再进 [JS API 手册](../reference/api-manual/index.md)
的对应章节看完整签名。

## 我想……

| 我想 | 用什么 | 一句话用法 |
|---|---|---|
| 返回成功/失败 | `json` | `json.ok(data)` / `json.fail(400, "缺参数")` |
| 出裸 JSON（对接第三方协议） | `json.raw` | `json.raw(obj, 200)` —— 不包信封 |
| 读查询参数 / 请求体 / 头 | `http` | `http.param("id")`、`http.body`、`http.header("x")`、`http.user` |
| 查库 / 写库 | `db` | `db.query(sql, [p])`、`db.exec(sql, [p])` |
| 用白名单构造器写 SQL | `db.table()` | `db.query` 之外的第二条路，动态标识的**唯一**合法来源 |
| 开事务 | `db.tx` | 每请求单活跃事务，异常自动回滚 |
| 缓存 / 会话 | `kv` | `kv.get/set/del/expire/incr` |
| 存文件 | `blob` | `blob.put/get/del/url` |
| 发布广播 | `bus` | `bus.publish(topic, msg)`（HTTP 与 WS 帧里都能发） |
| 订阅频道 | `bus` | `bus.subscribe(topic)` —— **只能在 WS 会话里调**，HTTP 路径调用会报错（订阅对象是连接本身） |
| 检索 | `es` | `es.search/index/del` |
| 调外部 HTTP | `fetch` | WHATWG `fetch`（deno_fetch 实现，含根证书） |
| 连外部 WebSocket | `WebSocket` | 标准 `new WebSocket(url)`（出站客户端） |
| 服务端 WS 帧循环 | `ws` | 在 `ws.ts` 里用，配合 `bus.subscribe` |
| 打日志 | `log` | `log.info/warn/error` |
| 签/验 JWT | `jwt` | `jwt.sign/verify` |
| 口令哈希 | `bcrypt` | `bcrypt.hash/verify` |
| 摘要与随机 | `crypto` | `crypto.sha256Hex/randomHex` |
| OIDC 签发 / 验签 | `oidc` | `oidc.sign/verify/jwks` |
| 证书 | `cert` | `cert.gen/renew`（纯内存，不落盘） |
| 查装了哪些插件 | `plugins` | `plugins()` 或 HTTP `GET {base}/plugins` |
| 消息队列 / 长任务 | `Kafka(name)` / `RabbitMQ(name)` / `tasks` | 生产可在 handler，消费仅任务上下文；`tasks.sleep(ms)`、`tasks.stopping()` |
| 运行时补一段 JS | `ext_boot.js` | runtime 创建期加载，动态补充全局 |

> 完整 19 组总表与每个方法的签名、错误行为，见
> [JS API 手册 · 全局对象 API 参考](../reference/api-manual/index.md)。

## 信封：所有响应的形状

```jsonc
{ "code": 0,   "msg": "ok",        "data": { ... } }   // 成功
{ "code": 400, "msg": "name required", "data": null }  // 业务失败
```

约定：

- `code = 0` 表示成功，其余都是业务/框架错误码。
- **`json.ok` / `json.fail` 会自动补 `content-type: application/json`**，不用自己设。
- 对接标准协议（OIDC discovery、jwks 等）时用 `json.raw` 出裸 JSON，别让信封污染协议。
- 框架级错误也有统一形状：405 方法不允许、408 执行超时（看门狗）、413 体积超限、
  500 路由冲突或 handler 未捕获异常。

## 两个最容易写错的地方

**1. 异步没兜住**

```ts
// ❌ 出错时看不到原因，只得到一个 500 空壳
db.query("select ...", []).then((r) => json.ok(r));

// ✅
db.query("select ...", [])
  .then((r) => json.ok(r))
  .catch((e) => json.fail(500, String(e)));
```

**2. SQL 拼字符串**

```ts
// ❌ 红线：值拼接
db.query(`select * from account where name = '${http.param("name")}'`);
// ❌ 红线：动态标识来自用户输入
db.query(`select * from ${http.param("table")}`);

// ✅ 值走绑定参数；动态标识交给 db.table()（只认 SchemaRegistry 白名单）
db.query("select * from account where name = ?", [http.param("name")]);
```

## 延伸

- [06 · 数据层与迁移](./06-data-layer.md)
- [bridge 与全局对象](../reference/bridge.md)（Rust 侧视角：op 是怎么挂上去的）
