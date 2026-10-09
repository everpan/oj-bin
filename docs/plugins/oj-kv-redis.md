# oj-kv-redis

## 概述

提供 `kv` 轴的 cdylib 插件，底层 `redis` 0.27。迁自 core `RedisKV`：`set` / `get` / `del` / `expire` / `incr`，连接经 `ConnectionManager` 复用，连接时单次探活 fail-fast。

## 提供的后端轴

`kv`

## 配置

顶层 `redis:` 段（`HashMap<name, URL>`，键 = profile 名，供 `--redis <profile>` 选默认源）。每 profile 的值：

| 字段 | 说明 |
|---|---|
| `url` | redis URL（如 `redis://host:6379/1`），连接时探活 |

字段权威定义见 [`../modules/02-config.md`](../modules/02-config.md) 的 `redis` 段。

## 依赖与构建注意

- `redis` 0.27，纯 Rust（async `ConnectionManager`）。
- 连接 / 认证失败 → Err，装配 fail-fast（先单次探活，不让 `ConnectionManager` 的无限重试把启动挂死）。
- URL 脱敏：`redis://***@host`（密码来自 `ENC[]` 密封值，错误串经终端镜像落 `logs/`，必须掩码）。

## 状态

已随发行包发布（较早合入）。

## 案例

### 用户登录后缓存会话（set + expire）

```ts
// src/session/login/api.ts
async function post() {
  const b = http.body as { name?: string };
  if (!b.name) { json.fail(400, "name required"); return; }
  const rows = await db.table("account").select(["id", "name"])
    .where({ field: "name", op: "eq", value: b.name }).all();
  if (!rows.length) { json.fail(401, "bad credentials"); return; }
  const sid = crypto.randomHex(32);                        // 64 字符不透明串
  await kv.set(`sess:${sid}`, JSON.stringify({ uid: rows[0].id, name: rows[0].name }));
  await kv.expire(`sess:${sid}`, 3600);                    // 秒；Redis EXPIRE 只认整秒
  json.ok({ sid, ttl: 3600 });
}
export default { post };
```

### 短信发送接口限流（incr + expire）

`kv.incr` 键不存在时从 0 起、返回新值；首次命中补过期时间即构成滑动窗口计数。

```ts
// src/sms/send/api.ts
async function post() {
  const phone = String((http.body as { phone?: string }).phone ?? "");
  if (!phone) { json.fail(400, "phone required"); return; }
  const key = `rl:sms:${phone}`;
  const n = await kv.incr(key);
  if (n === 1) await kv.expire(key, 60);                   // 窗口期 60 秒
  if (n > 5) { json.fail(429, "too many requests"); return; }
  // …调用短信通道…
  json.ok({ sent: true, count: n });
}
export default { post };
```

### 订单详情读穿缓存（get → miss 回源 DB → set）

```ts
// src/order/detail/api.ts —— 摘自 sample 改编
async function get() {
  const id = Number(http.param("id", 0));
  const key = `order:${id}`;
  const hit = await kv.get(key);
  if (hit !== null) { json.ok({ cached: true, data: JSON.parse(hit) }); return; }
  const rows = await db.table("orders").select(["id", "no", "amount"])
    .where({ field: "id", op: "eq", value: id }).all();
  if (!rows.length) { json.fail(404, "not found"); return; }
  await kv.set(key, JSON.stringify(rows[0]));
  await kv.expire(key, 300);                               // 5 分钟
  json.ok({ cached: false, data: rows[0] });
}
export default { get };
```

```yaml
# config.yaml（配置存在即真连 Redis，启动探活 fail-fast；删除该段回落进程内存 KV）
redis:
  default: "ENC[...]"   # 明文形态：redis://:pass@127.0.0.1:6379/0
```

```bash
curl -X POST http://localhost:9778/v1/api/session/login/ \
  -H 'content-type: application/json' -d '{"name":"neo"}'
curl -X POST http://localhost:9778/v1/api/sms/send/ \
  -H 'content-type: application/json' -d '{"phone":"13800000000"}'
curl 'http://localhost:9778/v1/api/order/detail/?id=1'   # 第二次请求 cached: true
```

## 备注

- 未声明 `redis.default` 时，宿主回落内置 `InMemoryKV`，不进插件。
- 跨线时长以秒计（宿主侧已向上取整，Redis `EXPIRE` 只认整秒）。
