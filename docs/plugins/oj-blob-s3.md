# oj-blob-s3

## 概述

提供 `blob` 轴的 cdylib 插件，底层走 `object_store` 的 `AmazonS3`。`put` / `get` / `del` / `url`（预签名 15min）/ `upload_url` / 流式上传（8 MiB 定长 part）/ 服务端 copy·move / 区间读，行为与下线前的 `S3Blob` 对齐。

## 提供的后端轴

`blob`

## 配置

顶层 `blob:` 段（平铺旧单后端，或 `blob.backends.<name>` 命名多后端，二者并存且平铺非默认会歧义报错）。每后端连接收的 JSON 字段：

| 字段 | 说明 |
|---|---|
| `driver` | 取 `s3` |
| `root` | 对象键前缀（可空） |
| `endpoint` | S3 兼容端点；`http://` 明文端点会自动放开 `allow_http` |
| `bucket` | **必填**（缺 → 启动报错） |
| `region` | **必填**（缺 → 启动报错） |
| `access_key` / `secret_key` | 可选；缺则匿名访问 |
| `path_style` | 默认 `false`（virtual-hosted）；MinIO / 自建切 `true` |

字段权威定义见 [`../modules/02-config.md`](../modules/02-config.md) 的 `blob` 段。

## 依赖与构建注意

- `object_store` + `reqwest`，纯 Rust，无原生库。
- `content_type` 恒为 `null`：MIME 由 S3 对象自身元数据负责。
- 多 part 上传当前仅支持单发 `put` 预签名；`multipart` 预签名（Create/UploadPart/Complete）未实现，预留 op 返回 Err（>100MB 超大文件待真实需求再加 SigV4 三段预签名）。

## 状态

已随发行包发布（较早合入）。

## 案例

### 商品图上传后返回预签名下载地址

```ts
// src/product/image/api.ts —— multipart 上传商品图 → 存 S3 → 回预签名 URL
async function post() {
  const f = http.files[0];                  // {field, filename, content_type, size, ...}
  if (!f) { json.fail(400, "need a file (multipart)"); return; }
  const safe = f.filename.replace(/[^\w.-]+/g, "_");
  const key = `products/${http.param("id", "0")}/${Date.now()}-${safe}`;
  await blob.put(key, await http.file(0), f.content_type);
  json.ok({ key, url: await blob.url(key) });  // url = 15min 预签名下载地址
}
export default { post };
```

```bash
curl -F "img=@cover.png" http://localhost:9778/v1/api/product/image/?id=42
# → {"code":0,"data":{"key":"products/42/...-cover.png","url":"https://s3.../...?X-Amz-Signature=..."}}
```

### 大视频客户端直传 S3（绕开请求体上限与 30s 超时）

```ts
// src/media/ticket/api.ts —— 发直传票：客户端拿预签名 PUT URL 自行上传
async function post() {
  const b = http.body as { filename?: string };
  const safe = (b?.filename ?? "video.mp4").replace(/[^\w.-]+/g, "_");
  const key = `media/${Date.now()}-${safe}`;
  const { url } = await blob.uploadUrl(key);  // 15min 预签名 PUT
  json.ok({ key, url });
}
export default { post };
```

```bash
# ① 拿票；② 客户端直传（不经 oj）
curl -X POST -H 'Content-Type: application/json' \
  -d '{"filename":"a.mp4"}' http://localhost:9778/v1/api/media/ticket/
curl -T a.mp4 "<返回的 url>"
# 下载走内置公开路由：GET /v1/api/blob/<key> → 302 跳预签名 URL
```

### 流式落盘的临时对象服务端转正（字节不进 V8）

```ts
// src/docs/commit/api.ts —— 大附件先被服务端流式落 S3，审核通过后 move 到正式目录
async function post() {
  const b = http.body as { key?: string; docId?: string };
  if (!b?.key || !b?.docId) { json.fail(400, "need key & docId"); return; }
  const dst = `docs/${b.docId}/${b.key.split("/").pop()}`;
  await blob.move(b.key, dst);   // s3 = CopyObject + DeleteObject，2 次服务端调用
  json.ok({ key: dst });
}
export default { post };
```

```yaml
# config.yaml —— 三个案例共用；MinIO / 自建端点切 path_style
blob:
  driver: s3
  endpoint: http://minio:9000
  bucket: my-app
  region: cn-north-1
  access_key: ENC[...]     # oj secret seal 生成；缺省 = 匿名
  secret_key: ENC[...]
  path_style: true
```

## 备注

- 与 `oj-kv-redis` 等服务不同：blob 插件的 `init` 无装配期配置，后端配置在 `connect` 时按值传入。
- key 白名单同 core：段非空、非 `.`/`..`、不含 `\`/`\0`、不以 `/` 开头。
