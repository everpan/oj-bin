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

## 备注

- 与 `oj-kv-redis` 等服务不同：blob 插件的 `init` 无装配期配置，后端配置在 `connect` 时按值传入。
- key 白名单同 core：段非空、非 `.`/`..`、不含 `\`/`\0`、不以 `/` 开头。
