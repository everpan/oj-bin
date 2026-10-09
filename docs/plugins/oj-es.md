# oj-es

## 概述

提供 `es` 轴的 cdylib 插件，底层 `reqwest` 直连 Elasticsearch HTTP。迁自 core `EsClient`：`search` / `index_doc` / `delete_doc` 三方法，句柄固定在 handle 0（单后端）。

## 提供的后端轴

`es`

## 配置

顶层 `es:` 段（v0.1.34 起为命名 map，键 = profile 名，供 `--es <profile>` 选取；旧单对象写法自动包成 `{ default: ... }`）。每 profile 的值：

| 字段 | 说明 |
|---|---|
| `endpoint` | Elasticsearch HTTP 地址（尾斜杠幂等剪除） |

字段权威定义见 [`../modules/02-config.md`](../modules/02-config.md) 的 `es` 段。

## 依赖与构建注意

- `reqwest`，纯 Rust（插件内 `Client::builder().no_proxy()`）。
- es 轴无 `connect` 方法：init 时为 cfg 声明的 endpoint 建 handle 0（未来多客户端走 cfg 加 endpoint 条目再分配）。
- 2xx → JSON 直回；非 2xx → 错误带状态码与响应体，便于排障。

## 状态

已随发行包发布（较早合入）。

## 案例

### 商品保存后同步搜索引擎

业务事实以 DB 为准，写库成功后同步一份文档到 ES（文档 id 即商品 id，重复提交覆盖同一文档）。

```ts
// src/goods/save/api.ts
async function post() {
  const b = http.body as { id?: number; title?: string; price?: number };
  if (!b.id || !b.title) { json.fail(400, "id and title required"); return; }
  await db.table("goods").insert({ id: b.id, title: b.title, price: b.price }).run();
  const resp = await es.index("goods", String(b.id), { title: b.title, price: b.price });
  json.ok({ es: resp.result });        // PUT _doc?refresh=true，写完即可查
}
export default { post };
```

### 商品标题全文搜索

`es.search` 直通 ES 响应体，handler 按需裁剪后回给前端。

```ts
// src/goods/search/api.ts
async function get() {
  const kw = http.param("kw", "");
  if (!kw) { json.fail(400, "kw required"); return; }
  const size = Math.min(Number(http.param("size", 20)), 100);
  const body = await es.search("goods", {
    query: { match: { title: kw } },
    from: 0,
    size,
  });
  const list = body.hits.hits.map((h: { _source: unknown }) => h._source);
  json.ok({ total: body.hits.total.value, list });
}
export default { get };
```

### 商品下架后删除索引

```ts
// src/goods/off/api.ts
async function post() {
  const id = Number(http.param("id", 0));
  if (!id) { json.fail(400, "id required"); return; }
  await db.table("goods").delete().where({ field: "id", op: "eq", value: id }).run();
  await es.del("goods", String(id));   // 幂等；文档已不存在返回 404 体
  json.ok({ removed: id });
}
export default { post };
```

```yaml
# config.yaml
es:
  default:
    endpoint: "http://127.0.0.1:9200"
```

```bash
curl -X POST http://localhost:9778/v1/api/goods/save/ \
  -H 'content-type: application/json' -d '{"id":1,"title":"机械键盘","price":299}'
curl 'http://localhost:9778/v1/api/goods/search/?kw=键盘&size=10'
curl -X POST 'http://localhost:9778/v1/api/goods/off/?id=1'
```

## 备注

- 路径拼装：`/{index}/_search`（无 id）或 `/{index}/_doc/{id}?refresh=true`（有 id）。
- 索引名 / id 的合法性校验留在宿主 op 层，插件信任宿主已校验。
