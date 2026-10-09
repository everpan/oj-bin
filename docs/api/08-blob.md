# blob —— 对象存储（`blob(name?)` 全局对象）

> 本文档由 src/bridge/ 梳理生成；权威 API 参考见 ../devkit/api-manual.md。

## 是什么

为 JS/TS handler 提供对象存储能力（`src/bridge/blob.rs`）。本地 `local` 驱动内建于核心
（object_store LocalFileSystem + 写 sidecar `<key>.ct` 持久化 Content-Type），`s3` 驱动由
cdylib 插件 `oj-blob-s3` 承载（blob 轴）。JS 侧暴露 `blob` 全局对象，支持**命名多后端**：
`blob("img").put(...)` 选命名后端，裸调用 `blob.put(...)` 等价 `blob("default")`。

配套内置路由（serve 层，不经 handler）：

- `GET {base}/blob/{key}` —— 下载（免鉴权；local 直出字节 + Content-Type，s3 302 跳
  presigned URL；local 内联腿支持单区间 `Range` 请求 → 206）。
- `PUT {base}/blob/{key}` —— 大文件直传上传（**过鉴权守卫**；体积上限独立
  `server.blob_upload_max_bytes`，默认 1 GiB；不经 JsActor，无 handler 30s 限制）。

与 `fs` 的分工：`blob` 走对象存储（大文件/共享存储），`fs` 走服务进程本地磁盘（小对象）。

## 配置

```yaml
blob:                      # 段存在即启用；缺省 = blob 全局/上传/下载路由均不挂
  driver: "local"          # local | s3
  root: "uploads"          # local 专用：存储根（相对 config.yaml 所在目录解析）
  # s3（oj-blob-s3 插件；MinIO 需 path_style: true）：
  # driver: "s3"
  # endpoint: "http://127.0.0.1:9000"
  # bucket: "oj-blobs"
  # region: "us-east-1"
  # access_key: "ENC[...]"
  # secret_key: "ENC[...]"
  # path_style: true

  # 命名多后端（backends 与平铺字段互斥，并存即启动报错）：
  # backends:
  #   default: { driver: local, root: uploads }
  #   img:     { driver: s3, endpoint: ..., bucket: images, region: ... }

server:
  max_upload_bytes: 10485760        # handler 面 multipart 上限（默认 10MB，超限字段流式落 blob）
  blob_upload_max_bytes: 1073741824 # PUT 直传 / 流式落 blob 的单文件上限（默认 1 GiB）
```

- 平铺字段是 `backends.default` 的旧语法糖；多后端请统一写 `backends:`。
- 配置声明了名字但装配时无对应后端（如 s3 插件未装）→ 启动 fail-fast。

## API

全部 async。key 白名单：`/` 分段，每段非空、非 `.`/`..`、不含 `\`/`​\0`；整串非空、
不以 `/` 开头（非法 key 报 `invalid blob key '<key>'`）。

| API | 签名 | 说明 |
|---|---|---|
| `blob(name?)` | 可调用取命名实例：`blob("media").put(...)`；裸调用等价 `blob("default")` | 对应 config `blob.backends.<name>` |
| `put` | `(key: string, bytes: Uint8Array, contentType?: string) => Promise<boolean>` | 写对象（local 落盘 / s3 上传）。local：显式 ct 且与扩展名推断不同才写 sidecar |
| `get` | `(key: string) => Promise<Uint8Array>` | 读对象（不存在报错 `blob get: ...`） |
| `del` | `(key: string) => Promise<boolean>` | 删对象（**幂等**：不存在视为成功；local 一并清 sidecar） |
| `url` | `(key: string) => Promise<string>` | 下载地址：local = `{base}/blob/{key}`（**仅 default 后端可用**，非 default 报错）；s3 = presigned URL（15min） |
| `uploadUrl` | `(key: string, opts?: {kind: string}) => Promise<{url: string}>` | 上传直传预签名（v0.1.30）：**仅 s3 后端**（15min 预签名 PUT URL）；`opts` 缺省 `{"kind":"put"}`，multipart 形态暂返回 Err。local 报 `local blob backend has no upload presign; use the direct PUT upload route`，改用 `PUT {base}/blob/{key}` 直传路由 |
| `contentType` | `(key: string) => Promise<string>` | local 读 sidecar / 按扩展名推断；缺失且无法推断返回**空串**。s3 后端恒返回空串（Content-Type 由 S3 对象自身元数据负责） |
| `copy` | `(src: string, dst: string) => Promise<boolean>` | **服务端复制（v0.1.47，ABI 11）：src 保留**。local = `fs::copy`（字节不进 V8）；s3 = CopyObject。`src == dst` 为 no-op；src 不存在报错。ct 随行 |
| `move` | `(src: string, dst: string) => Promise<boolean>` | **服务端搬移（v0.1.47，ABI 11）：src 不再存在**。local 同父目录 = `fs::rename`（原子、零字节），跨目录 = copy + unlink；s3 = CopyObject + DeleteObject。`src == dst` 为 no-op |
| `readRange` | `(key: string, offset: number, len: number) => Promise<Uint8Array>` | **区间读（v0.1.47，ABI 11）：短读截断**——`offset + len` 越尾只返回实际可读字节，`offset` 过尾返回空数组；key 不存在报错 |

## 错误

| 场景 | 消息关键词 |
|---|---|
| 未配置 `blob:` 段即调用 | `blob not configured (config blob: section missing)` |
| 命名后端不存在 | `blob backend '<name>' not configured (config blob.backends.<name> missing)` |
| key 越白名单（`..`/`\`/空段/前导 `/`） | `invalid blob key '<key>'` |
| 非 default 的 local 后端调 `url()` | `blob url() is only available for the 'default' backend (backend '<name>': use get() or an s3 presign)` |
| local 后端调 `uploadUrl()` | `local blob backend has no upload presign; use the direct PUT upload route` |
| `readRange` 的 offset/len 为负/非整数/NaN | `blob readRange: offset must be a non-negative integer (got <v>)` |
| config 平铺字段与 `backends:` 并存 | `blob: flat fields and backends: are mutually exclusive (use backends.default for the default backend)`（启动期） |
| 底层 IO / S3 错误 | `blob put: ...` / `blob get: ...` / `blob del: ...` / `blob copy: ...` 等（后端原始错误透出） |

## 限制

- **无 `list` / 无目录列举 API**：对象清单请自行落 db/kv 索引。
- 未配置 `blob:` 段时 `blob` 全局、下载路由、PUT 直传路由**都不挂**。
- `url()` 仅服务 default 后端（下载路由只挂 default）；非 default 后端用 `get()` 或 s3
  presign。
- `copy`/`move`/`readRange` 在后端不支持时宿主回落（`copy` → `get`+`put`、`readRange` →
  `get` 全量切片）——JS 侧永不失败，但回落**字节进 V8**（local/s3 均原生支持，回落只是保险）。
- `readRange` 是短读截断：先判空再按 `head.length` 定分支，不要假设一定拿满 `len`。
- 大文件不要走 handler 缓冲：multipart 超 `server.max_upload_bytes` 的字段由服务端**流式
  落 blob**（v0.1.38，local 临时文件 / s3 multipart，服务端内存恒定），此时 `http.file(i)`
  报错，改用 `http.files[i].key` / `.url`；服务端代分配 key 形如
  `uploads/<时间戳>-<序号>-<安全化文件名>`。

## 案例

### 上传四件套：multipart → 落 blob → 返回下载地址

```ts
// src/upload/api.ts
async function post() {
  const f = http.files[0]; // {field, filename, content_type, size, key?, url?}
  if (!f) { json.fail(400, "need a file field (multipart)"); return; }
  const b = await http.file(0);                       // ≤ max_upload_bytes 的小文件给字节
  await blob.put(f.filename, b, f.content_type);      // 3. 存储
  json.ok({ key: f.filename, url: await blob.url(f.filename), size: b.length });
}
async function del() {
  await blob.del(http.param("k", ""));                // 幂等
  json.ok({ ok: true });
}
export default { post, del };
```

```yaml
# config.yaml
blob:
  driver: "local"
  root: "uploads"
```

```bash
curl -F "doc=@a.pdf" http://localhost:9778/v1/api/upload/
# → {"code":0,"data":{"key":"a.pdf","url":"/v1/api/blob/a.pdf","size":…}}
curl -OJ http://localhost:9778/v1/api/blob/a.pdf      # 内置下载路由，免鉴权
```

> 大于 `server.max_upload_bytes` 的文件字段不进 handler（`http.file(0)` 报错）——
> 改用 `http.files[0].key` 读已流式落盘的 blob，或直接走下面的直传。

### 大文件直传（两阶段，不经 handler）

阶段一由 handler 发「传票」，阶段二客户端直传 blob——绕开 `max_upload_bytes` 与 handler
30s 超时：

```ts
// src/upload/ticket/api.ts
async function post() {
  const name = String(http.body?.name ?? "");
  if (!name) { json.fail(400, "name required"); return; }
  const key = `docs/${Date.now()}-${name.replace(/[^\w.-]+/g, "_")}`;
  // s3 后端：15min 预签名 PUT URL；local 后端：uploadUrl 会报错，直接用直传路由
  json.ok({ key, putUrl: `/v1/api/blob/${key}` });
}
export default { post };
```

```bash
# ① 拿传票
curl -X POST -H 'Content-Type: application/json' -d '{"name":"big.zip"}' \
  http://localhost:9778/v1/api/upload/ticket/
# → {"code":0,"data":{"key":"docs/…-big.zip","putUrl":"/v1/api/blob/docs/…-big.zip"}}
# ② 客户端直传（过鉴权守卫：带 Bearer/cookie；上限 server.blob_upload_max_bytes，默认 1 GiB）
curl -X PUT -H "Authorization: Bearer $TOKEN" --data-binary @big.zip \
  http://localhost:9778/v1/api/blob/docs/…-big.zip
# s3 后端则阶段一取 blob.uploadUrl(key) 的预签名 URL，客户端 PUT 到该 URL 直传 S3
```

### 流式落盘对象转正 + 魔数嗅探（字节不进 V8）

```ts
// src/report/finalize/api.ts —— 大文件三件套：readRange 嗅探 → move 转正
async function post() {
  const key = http.param("key", ""); // 流式落盘的临时对象 key（uploads/<ts>-<n>-<name>）
  const head = await blob.readRange(key, 0, 8);       // 只读前 8 字节，不 get 全文
  if (head.length === 0) { json.fail(400, "empty object"); return; }
  const isZip = head[0] === 0x50 && head[1] === 0x4b; // "PK"
  const finalKey = `docs/${http.param("id", "x")}/${isZip ? "pack.zip" : "data.bin"}`;
  await blob.move(key, finalKey);                     // src 消失；同目录 rename 零字节搬运
  json.ok({ key: finalKey, url: await blob.url(finalKey) });
}
export default { post };
```

```bash
curl -X POST "http://localhost:9778/v1/api/report/finalize/?key=uploads/1717-0-big.zip&id=42"
# → {"code":0,"data":{"key":"docs/42/pack.zip","url":"/v1/api/blob/docs/42/pack.zip"}}
```
