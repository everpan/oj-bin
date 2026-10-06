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

## 备注

- 路径拼装：`/{index}/_search`（无 id）或 `/{index}/_doc/{id}?refresh=true`（有 id）。
- 索引名 / id 的合法性校验留在宿主 op 层，插件信任宿主已校验。
