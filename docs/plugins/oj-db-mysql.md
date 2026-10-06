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

## 备注

- 与 `oj-db-postgres` 同构，按 spec 允许复制（各自自包含，不抽共享 crate）。
- 真库用例 `OJ_TEST_MYSQL` 门控：unsigned bigint 往返、列类型响亮报错、并发取号原子性、真取消。
