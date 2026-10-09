# http —— 当前请求上下文

> 本文档由 src/bridge/ 梳理生成；权威 API 参考见 ../devkit/api-manual.md。

## 是什么

`http` 是**只读、懒加载**的请求上下文对象（`bootstrap.js` 394 行的 `Proxy`）：除
`param` / `file` / `bodyBytes` 三个方法外，任意属性访问都实时取当前请求的快照
（`op_http_info`），保证拿到的是**本请求**的最新数据——池化复用的 runtime 不会串请求。
WS 帧钩子（`ws.ts`）里同样可用（帧即「请求」）。

## 配置

`http` 本身无配置段，以下两处配置直接塑形它的行为：

```yaml
server:
  max_upload_bytes: 8388608   # multipart 小文件阈值：≤ 此值进 http.file(i)；超出由服务端流式直落 blob
tenant:
  enable: true                # 启用后从租户头提取 http.tenantId
  header_key: "X-TENANT-ID"   # 默认即此名；缺头且未豁免 → 400
```

## API 表

| API | 类型 / 签名 | 说明 |
|---|---|---|
| `http.method` | `string` | 请求方法（`GET`/`POST`/…） |
| `http.params` | `Record<string, string>` | 路径参数对象（`.route` 声明，已 percent-decode；目录镜像路由下恒空） |
| `http.query` | `Record<string, string>` | query 参数对象（form-urlencoded 解码：`+`→空格、`%XX`） |
| `http.param` | `param(name, def?): any` | **路径参数优先，query 兜底**；均缺失返回 `def` 原值（不做数字转换） |
| `http.headers` | `Record<string, string>` | 请求头对象（小写名）。**没有独立的 `http.cookie`**——cookie 读 `http.headers.cookie` |
| `http.body` | `any` | 请求体：空 → `null`；能按 JSON 解析 → 对象/数组；否则 → UTF-8 字符串。multipart 请求时是**文本字段对象** `{name: value}`；WS Binary 帧为 `null` |
| `http.bodyBytes` | `bodyBytes(): Promise<Uint8Array>` | 原始请求体字节（WS 文本/二进制帧通用；HTTP 大 body 也可经此取字节） |
| `http.tenantId` | `string \| null` | 租户 id（`tenant.enable` 时从头提取注入；未启用为 `null`） |
| `http.user` | `{id, roles, claims} \| null` | 已验签用户：auth 启用且请求通过 Bearer 守卫才有值，匿名路径/未启用为 `null` |
| `http.files` | `Array<{field, filename, content_type, size, key, url}>` | multipart 上传元信息（非 multipart 为空数组）。`key`/`url`：流式大文件（> `max_upload_bytes`，服务端直落 blob）的对象 key 与下载地址；小文件为 `null` |
| `http.file` | `file(i: number): Promise<Uint8Array>` | 第 i 个上传文件字节（仅小文件缓冲路径；流式大文件报错并指路 `files[i].key`/`.url`） |

## 错误

| 场景 | 报错 / 行为 |
|---|---|
| `http.file(i)` 索引越界 | `no such file: {i}` |
| `http.file(i)` 取流式大文件 | `file {i} ('{name}') was streamed to blob key '{key}' — use blob.get() or http.files[{i}].url (http.file() only serves small buffered uploads)` |
| `tenant.enable` 下缺租户头（且未命中 `anonymous_paths`） | 前置管线 400，handler 不执行 |
| 受保护路径未过 Bearer 守卫 | 前置管线 401，handler 不执行（`http.user` 因此必有值时才进 handler） |

## 限制

- **只读**：`http` 是 Proxy 门面，赋值无意义也不生效；响应头请走 `json.header`。
- 路径参数/query 的值**都是字符串**（或 `def` 原值），需要数字自行 `Number(...)`。
- `http.body` 的 JSON 解析是「能解则解」——`"123"` 的文本体会变成 number `123`；
  要严格区分请用 `await http.bodyBytes()` 自行处理。
- `http.file(i)` 返回的是**克隆的字节**（整个文件进内存）；大文件请走流式 blob 路径
  （`files[i].key` / `files[i].url`），不要在 handler 里反复 `http.file(i)`。
- `http.user` 的形状由 oj-auth 守卫决定（`{id, roles, claims}`）；未启用 auth 时恒 `null`，
  不要用 `http.user` 是否存在来判断「auth 是否配置」。

## 案例

### 分页列表：param 路径优先、query 兜底

```ts
// src/item/list/api.ts
async function get() {
  const id = Number(http.param("id", 0));      // 路径 42 优先；?id=9 兜底；都没有 → 0
  const page = Number(http.param("page", 1));  // 无路径参数 → query 兜底 → 默认 1
  const rows = await db.table("item").select(["id", "name"])
    .orderBy([{ field: "id", dir: "desc" }])
    .limit(20).offset((page - 1) * 20).all();
  json.ok({ id, page, rows });
}
get.route = "{id}";   // 可选参数路由：/v1/api/item/list/42 → http.params.id = "42"
export default { get };
```

```bash
curl 'http://localhost:9778/v1/api/item/list/42?page=2'
# → {"code":0,"msg":"ok","data":{"id":42,"page":2,"rows":[...]}}
```

### 上传文件元信息读取（小文件直取字节，大文件走 blob）

```ts
// src/upload/api.ts
async function post() {
  const f = http.files[0];
  if (!f) { json.fail(400, "need a file (multipart)"); return; }
  if (f.key) {
    // 超过 max_upload_bytes：服务端已流式落 blob，http.file(0) 会报错指路
    json.ok({ name: f.filename, size: f.size, key: f.key, url: f.url });
    return;
  }
  const bytes = await http.file(0);
  json.ok({ name: f.filename, size: bytes.length, type: f.content_type });
}
export default { post };
```

```bash
curl -F "doc=@a.pdf" http://localhost:9778/v1/api/upload/
# → {"code":0,"msg":"ok","data":{"name":"a.pdf","size":10240,"type":"application/pdf"}}
```

### 租户 + 用户上下文打点

```ts
// src/order/create/api.ts —— 前置管线已保证 tenantId/user 齐备（缺则 400/401 进不来）
async function post() {
  log.info("order create", "tenant", http.tenantId, "user", http.user.id,
    "ua", http.headers["user-agent"]);
  const b = http.body ?? {};
  // ... 业务落库
  json.ok({ tenant: http.tenantId, by: http.user.id, got: b });
}
export default { post };
```
