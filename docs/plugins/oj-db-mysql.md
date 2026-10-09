# oj-db-mysql

## 概述

提供 `db` 轴的 cdylib 插件，底层 `sqlx` 0.9（mysql driver）。认领 `db:` 段里 `mysql://` scheme 的 profile；连接经 `DbBackendRegistry` 按 scheme 路由到本插件。

## 提供的后端轴

`db`

## 配置

顶层 `db:` 段（`HashMap<name, DSN>`，键 = 库名）。本插件认领 `mysql://` 开头的 DSN；DSN 在 `connect` 时按值传入，无专属字段。

- `mysql://` 走 sqlx typed driver（精确承载 `u64` / `BIGINT UNSIGNED`，不回绕成负数）。
- 其它 scheme（如离线测试用的 `sqlite://`）走 `Any` 回退路径，仅覆盖编排/绑定通路，不在生产装配面。

字段权威定义见 [`../modules/02-config.md`](../modules/02-config.md) 的 `db` 段。

## 依赖与构建注意

- `sqlx` 0.9（mysql feature），纯 Rust，无原生库。
- 自建 tokio runtime（跨 FFI 不共享宿主 tokio）。
- 列类型能力边界：DECIMAL / DATETIME / JSON / BIT 等未解码类型会**响亮报错**，绝不让步静默 `null`（踩仓库红线）。
- 流式游标 `stream_cancel` 对 `mysql://` 是方言级真取消（`KILL QUERY` + 断连）。

## 状态

已随发行包发布（较早合入）。

## 案例

### 用户注册写入档案并查询详情（构造器 CRUD）

```ts
// src/account/api.ts
async function post() {
  const b = http.body as { name?: string; role?: string };
  if (!b.name) { json.fail(400, "name required"); return; }
  const rows = await db.table("account")
    .insert({ name: b.name, role: b.role ?? "user" })
    .returning(["id"])            // MySQL: insert + SELECT LAST_INSERT_ID()，并发下请放 db.tx
    .run();
  json.ok({ id: rows[0].id });
}

async function get() {
  const id = Number(http.param("id", 0));
  const rows = await db.table("account").select(["id", "name", "role"])
    .where({ field: "id", op: "eq", value: id }).all();
  rows.length ? json.ok(rows[0]) : json.fail(404, "not found");
}
export default { get, post };
```

### 转账扣款（db.tx 事务，失败整体回滚）

构造器不支持 `balance = balance + ?` 这类自引用赋值，改用 `tx.exec` 参数化执行
（值仍只走绑定参数，不拼字符串）。

```ts
// src/pay/transfer/api.ts
async function post() {
  const b = http.body as { from?: number; to?: number; amount?: number };
  if (!b.from || !b.to || !b.amount || b.amount <= 0) { json.fail(400, "bad args"); return; }
  await db.tx(async (tx) => {
    const n = await tx.exec(
      "update account set balance = balance - ? where id = ? and balance >= ?",
      [b.amount, b.from, b.amount],
    );
    if (n !== 1) throw new Error("余额不足");        // 抛出 → 事务回滚
    await tx.exec("update account set balance = balance + ? where id = ?", [b.amount, b.to]);
  });
  json.ok({ transferred: b.amount });
}
export default { post };
```

```yaml
# config.yaml（凭据按红线用 oj secret seal 密封成 ENC[...]，不落明文）
db:
  default: "ENC[...]"   # 明文形态：mysql://user:pass@127.0.0.1:3306/app
```

```bash
curl -X POST http://localhost:9778/v1/api/account/ \
  -H 'content-type: application/json' -d '{"name":"neo"}'
curl 'http://localhost:9778/v1/api/account/?id=1'
curl -X POST http://localhost:9778/v1/api/pay/transfer/ \
  -H 'content-type: application/json' -d '{"from":1,"to":2,"amount":100}'
```

> 注意 MySQL 读侧列类型边界：`DECIMAL` / `DATETIME` / `JSON` 等未解码类型读出会响亮报错，
> 需要时在 SQL 里显式 `cast(amount as char)`；`BOOLEAN`/`TINYINT(1)` 读出是 `1`/`0`。

## 备注

- 与 `oj-db-postgres` 同构，按 spec 允许复制（各自自包含，不抽共享 crate）。
- 真库用例 `OJ_TEST_MYSQL` 门控：unsigned bigint 往返、列类型响亮报错、并发取号原子性、真取消。
