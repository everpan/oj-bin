# log —— 结构化日志（含 dev SQL 追踪）

> 本文档由 src/bridge/ 梳理生成；权威 API 参考见 ../devkit/api-manual.md。

## 是什么

`log` 是 handler 的结构化日志出口，基于 Rust `tracing`（`src/bridge/log.rs`）。
风格仿 zap SugaredLogger：**msg + 交替键值对**，字段在 JS 侧一次性序列化为 JSON
（BigInt 安全，不会因大整数抛错）交给 Rust 打印，日志 target 为 `js`。

同源的**dev SQL 追踪**（`src/bridge/sql_trace.rs`，v0.1.51）虽不是 `log.*` 方法，
但属同一日志体系：每条 SQL 打 `target="oj::sql"` 的结构化日志，并把本请求画像喂给
`db.sqlProfile()` 与响应信封 `_sql`——一并记在本篇。

## 配置

```bash
# 级别由 RUST_LOG 控制（tracing-subscriber 标准过滤串）
RUST_LOG=oj=info ./bin/oj serve -c config.yaml --api-path src
```

```yaml
# SQL 追踪（顶层 db_trace 段；dev 默认开、release 默认关）
db_trace:
  enabled: true        # 缺省 = dev 自动开 / release 自动关；可强制
  redact_params: true  # 默认 true：画像/信封只记参数个数；false 仅让 dev 日志记原值（响应仍脱敏）
  slow_ms: 0           # 慢查询阈值(ms)；0 = 全收，>0 时日志与 slow 列表按阈值过滤
  to_log: true         # 是否写 dev 日志（oj::sql target）
```

## API 表

| API | 签名 | 说明 |
|---|---|---|
| `log.debug` | `debug(msg: string, ...kv: unknown[]): void` | debug 级（level 0） |
| `log.info` | `info(msg: string, ...kv: unknown[]): void` | info 级（level 1） |
| `log.warn` | `warn(msg: string, ...kv: unknown[]): void` | warn 级（level 2） |
| `log.error` | `error(msg: string, ...kv: unknown[]): void` | error 级（level 3） |

键值对**交替传参**：`log.info("order created", "id", id, "amount", amount)` →
字段 `{"id":..., "amount":...}`。键一律 `String(...)`；落单的尾参数（无对应值）被忽略。

SQL 追踪两出口（配置开启时）：

- **dev 日志**：每条 SQL 一行 `oj::sql` 结构化日志，含 `sql` / `params` / `db` / `ms` /
  `rows` / `tx` / `src`（模块名）/ `status`（ok|ERR）/ `err`。
- **请求画像**：`db.sqlProfile()` 返回 `{count, totalMs, slow, byDb, events}`；
  dev 下 `json.ok`/`json.fail` 信封自动附加同构的 `_sql` 字段。

**参数脱敏红线**：画像与信封 `_sql` 的 `params` **永远**只记「参数个数」（如
`"3 params"`），绝不把密码/手机号外泄到响应；仅 `redact_params: false` 时服务端 dev
日志记参数原值，仅供本地排障。

## 错误

`log.*` 本身不抛错（fast op，无失败路径）：

| 场景 | 行为 |
|---|---|
| 字段含 BigInt | 序列化为十进制字符串，不抛 `TypeError` |
| 字段含循环引用 | `ojStringify` 同 `JSON.stringify` 语义——循环引用会抛 `TypeError`（避免把不可序列化对象塞进 kv） |
| 奇数个 kv 参数 | 最后一个无值键被忽略（`i + 1 < kv.length` 截断） |

## 限制

- 只有 4 个级别，无 `trace`/`fatal`；级别过滤完全交给 `RUST_LOG`。
- kv 是**交替参数**不是对象：`log.info("m", {id: 1})` 会把对象当成「键」`String(...)`
  化——要传对象请展开成 `"id", 1` 或包进某个值位。
- `log.*` 写 stderr（tracing-subscriber），不落文件、不进响应；要审计落库请自行 `db` 写表。
- SQL 追踪仅覆盖 bridge 的 db op（`db.query/exec/stream`、`tx.*`、`db.nextSeq`）；
  `redact_params: false` 只在本地排障时临时开，生产勿开（参数原值进日志）。

## 案例

### 请求日志打点

```ts
// src/order/create/api.ts
async function post() {
  const b = http.body ?? {};
  log.info("order create", "user", http.user?.id, "tenant", http.tenantId,
    "sku", b.sku, "qty", b.qty);
  try {
    // ... 业务落库
    json.ok({ created: true });
  } catch (e) {
    log.error("order create failed", "user", http.user?.id, "err", String(e));
    json.fail(500, "create failed");
  }
}
export default { post };
```

stderr 输出形如：

```
INFO js: order create fields={"user":7,"tenant":"t1","sku":"A-1","qty":2}
```

### 慢查询排障：dev SQL 日志 + 请求画像

```yaml
# config.yaml（dev 默认已开；这里显式调高慢查询阈值降噪）
db_trace:
  slow_ms: 50
```

```ts
// src/report/slow/api.ts —— 把本请求 SQL 画像塞进响应（配合信封 _sql 对照）
async function get() {
  const rows = await db.table("account").select(["id", "name"]).limit(100).all();
  const prof = db.sqlProfile();   // { count, totalMs, slow, byDb, events }（params 恒脱敏）
  log.info("sql profile", "count", prof.count, "totalMs", prof.totalMs);
  json.ok({ rows: rows.length });
}
export default { get };
```

```bash
RUST_LOG=oj=info ./bin/oj serve -c config.yaml --api-path src
curl http://localhost:9778/v1/api/report/slow/
# stderr：INFO oj::sql: SQL trace sql="SELECT ..." params="2 params" ms=73.2 status=ok ...
# 响应：{"code":0,...,"data":{"rows":100},"_sql":{"count":1,"totalMs":73.2,...}}
```

### 关键路径告警（error 级 + 上下文字段）

```ts
// src/pay/callback/api.ts —— 回调验签失败必须 error 级留痕
async function post() {
  const raw = await http.bodyBytes();
  if (!verifySign(raw, http.headers["x-sign"])) {
    log.error("pay callback bad sign", "ip", http.headers["x-forwarded-for"],
      "len", raw.length);
    json.fail(403, "bad sign"); return;
  }
  json.ok({ received: true });
}
export default { post };
```
