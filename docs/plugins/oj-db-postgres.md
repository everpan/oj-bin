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

## 备注

- 与 `oj-db-mysql` 同构，复制而非共享 crate。
- 真库用例 `OJ_TEST_PG` 门控：bigint 标记精确往返、同文本混参数形态、并发取号原子性、真取消。
