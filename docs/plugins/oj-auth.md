# oj-auth

## 概述

提供 `auth` 轴守卫的 cdylib 插件：Bearer 验签 + 匿名路径匹配。登录 / 刷新 / 登出端点已 JS 化（见 `sample/src/auth/`），本插件不含 db/kv 依赖，逻辑迁自 `server/auth.rs`。

## 提供的后端轴

`auth`

## 配置

顶层 `auth:` 段。关键字段：

| 字段 | 说明 |
|---|---|
| `jwt_secret` | 签名密钥，空 → 装配期 fail-fast |
| `signing_method` | `HS256` / `HS384` / `HS512`（默认 HS256） |
| `anonymous_paths` | 条目为字符串或 `{ path, one_layer }`；匹配语义与 server 侧 `path_matches` 同形 |
| `cookie` | 会话形态（oj-4 / ABI 9），默认关闭 = 纯 Bearer；开启后非安全方法叠加 CSRF 双提交 |

JWT 声明：插件验签后回传 JS 的结构为 `{ id: claims.sub, roles: claims.roles, claims }`，即 `sub` 映射为 `id`，并带上 `roles` 与原始 `iat`/`exp`。

字段权威定义见 [`../modules/02-config.md`](../modules/02-config.md) 的 `auth` 段。

## 依赖与构建注意

- `jsonwebtoken`，纯 Rust，无原生库依赖。
- `cookie` 会话形态依赖 ABI 9（oj-4）：装错 ABI 的旧宿主会判不兼容。
- 匿名路径通配 `*`（恰好一段）/ `**`（跨段）与 server 侧两份实现逐字同义，改一侧须同步另一侧。

## 状态

已随发行包发布（较早合入）。

## 备注

- 只做守卫，不碰 db/kv；凭据签发（登录 / 刷新）在 JS 侧完成。
- `**` 跨段匹配是 v0.1.20 收紧「任意深度」为「严格一层」后引入的显式写法；旧式尾 `/*` 只命中一层。
