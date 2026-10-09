# oj-db-postgres

## 概述

提供 `db` 轴的 cdylib 插件，底层 `sqlx` 0.9（postgres driver）。认领 `db:` 段里 `postgres://` / `postgresql://` scheme 的 profile。

## 提供的后端轴

`db`

## 配置

顶层 `db:` 段（`HashMap<name, DSN>`，键 = 库名）。本插件认领 `postgres://` / `postgresql://` 开头的 DSN；DSN 在 `connect` 时按值传入，无专属字段。

- PG 的 `bigint` 即 `i64`：`$oj$u64`（toUBigInt）装不下 → 明确拒绝，勿静默坍缩。
- 宿主在 PG 方言下给 SQL 前置参数形态签名 `/*oj:<形态>*/`，规避 sqlx 语句缓存「同文本换参数类型 → 协议级错误」。

字段权威定义见 [`../modules/02-config.md`](../modules/02-config.md) 的 `db` 段。

## 依赖与构建注意

- `sqlx` 0.9（postgres feature），纯 Rust，无原生库。
- 自建 tokio runtime（跨 FFI 不共享宿主 tokio）。
- 流式游标 `stream_cancel` 对 `postgres://` 是方言级真取消（断连专用连接 → 服务端 `pg_cancel_backend`）。
- 自动给 PG DSN 补 `statement-cache-capacity=512`（用户显式设置则以用户为准）。

## 状态

已随发行包发布（较早合入）。

## 案例

### 工单创建与按状态查询（构造器 CRUD）

```ts
// src/ticket/api.ts
async function post() {
  const b = http.body as { title?: string };
  if (!b.title) { json.fail(400, "title required"); return; }
  const rows = await db.table("ticket")
    .insert({ title: b.title, status: "open" })
    .returning(["id"])              // PG 原生 RETURNING，run() 返回行数组
    .run();
  json.ok({ id: rows[0].id });
}

async function get() {
  const status = http.param("status", "open");
  const rows = await db.table("ticket").select(["id", "title", "status"])
    .where({ field: "status", op: "eq", value: status })
    .orderBy([{ field: "id", dir: "desc" }])
    .limit(50)
    .all();
  json.ok(rows);
}
export default { get, post };
```

### 批量关闭工单并写操作日志（db.tx 事务）

一个事务里更新多张表：任一失败整体回滚，不会出现「工单关了但日志丢了」的中间态。

```ts
// src/ticket/close/api.ts
async function post() {
  const b = http.body as { ids?: number[] };
  if (!b.ids?.length) { json.fail(400, "ids required"); return; }
  const ids = b.ids;
  const closed = await db.tx(async (tx) => {
    const n = await tx.table("ticket").update({ status: "closed" })
      .where({ field: "id", op: "in", value: ids }).run();
    await tx.table("ticket_log")
      .insert(ids.map((id) => ({ ticket_id: id, action: "close" })))  // 多行 insert，键集须一致
      .run();
    return n;                     // db.tx 返回回调的返回值
  });
  json.ok({ closed });
}
export default { post };
```

```yaml
# config.yaml（凭据按红线用 oj secret seal 密封成 ENC[...]，不落明文）
db:
  default: "ENC[...]"   # 明文形态：postgres://user:pass@127.0.0.1:5432/app
```

```bash
curl -X POST http://localhost:9778/v1/api/ticket/ \
  -H 'content-type: application/json' -d '{"title":"修复登录超时"}'
curl 'http://localhost:9778/v1/api/ticket/?status=open'
curl -X POST http://localhost:9778/v1/api/ticket/close/ \
  -H 'content-type: application/json' -d '{"ids":[1,2,3]}'
```

> PG 的 `bigint` 即 `i64`：超长主键读出为十进制字符串，写回用 `toBigInt(v)`；
> `toUBigInt` 在 PG 下会被明确拒绝（无符号 64 位装不进 i64），不要依赖隐式转换。

## 备注

- 与 `oj-db-mysql` 同构，复制而非共享 crate。
- 真库用例 `OJ_TEST_PG` 门控：bigint 标记精确往返、同文本混参数形态、并发取号原子性、真取消。
