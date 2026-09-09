---
title: 07 · 鉴权与多租户
updated: 2026-09-08
---

# 07 · 鉴权与多租户

oj 的鉴权分成两半，这个分工新人最容易懵：

- **验签（守卫）在 Rust 侧**：`oj-auth` 插件实现 `AuthGuard`，在前置管线里验证
  `Authorization: Bearer <token>`，通过后才把身份塞进 `http.user`。
- **签发（登录）在 JS 侧**：`sample/src/auth/` 里的 `login`/`refresh`/`logout` 就是普通
  handler，用 `jwt.sign`、`bcrypt.verify`、`kv`（会话）自己实现。

好处是：登录策略（口令、短信、OIDC、三方）完全由你写，而验签这道门保持一致，且不会因为
业务代码写错而漏过。

## 走一遍 sample

```bash
# 1. 登录拿 token（auth 模块的 JS 端点）
TOKEN=$(curl -s -H 'X-TENANT-ID: default' \
  -d '{"username":"demo","password":"demo1234"}' \
  http://localhost:9778/v1/api/auth/login \
  | python3 -c 'import json,sys;print(json.load(sys.stdin)["data"]["access_token"])')

# 2. 带 token 访问受保护路由
curl -H "Authorization: Bearer $TOKEN" -H 'X-TENANT-ID: default' \
  http://localhost:9778/v1/api/auth_demo/me/

# 3. 不带 token（预期 401 信封）——用来确认守卫真的在工作
curl -H 'X-TENANT-ID: default' http://localhost:9778/v1/api/auth_demo/me/

# 4. refresh（会话轮换）
curl -H 'X-TENANT-ID: default' -d "{\"refresh_token\":\"$REFRESH\"}" \
  http://localhost:9778/v1/api/auth/refresh

# 5. logout（会话失效）
curl -H "Authorization: Bearer $TOKEN" -H 'X-TENANT-ID: default' \
  http://localhost:9778/v1/api/auth/logout
```

测试账号（seed 灌入）：`demo/demo1234`（admin）、`trinity/demo1234`（user）。

## 哪些是匿名的

别猜，按这个顺序判断：

1. **框架内置** `/v1/api/health`（证书状态）—— 永远匿名。
2. config 的 `auth.anonymous_paths` —— 显式放行的路径。
3. 其余**全部受保护**。

sample 里连 `/v1/api/auth_demo/health/` 都是受保护的（它就是拿来演示 401 的）。

## handler 里能拿到什么

```ts
http.user      // { id, roles, claims } —— 守卫验签后注入
http.tenantId  // 租户标识，来自租户头
http.header("X-Tenant-Id")
```

## 多租户

config 里开启后，请求**必须**带租户头，否则 400：

```yaml
tenant:
  enable: true
  header_key: "X-TENANT-ID"
  anonymous_paths:
    - "/oidc/*"
    - "/idp/*"
```

两个细节：

- **OIDC 的 302 跳转腿带不了自定义头**，所以这些路径要显式列进 `anonymous_paths`，
  否则浏览器跳转会被 400 拦下。
- **WS 连接天然匿名**：WS 路由是 merge 进 Router 的真实路由，不经过这套前置管线，
  所以不需要（也不会生效）出现在 `anonymous_paths` 里。要鉴权就在 `ws.ts` 首帧里自己做。

## OIDC（可选能力）

sample 自带一套**同进程双角色**演示：`src/idp`（内置 OP：discovery / jwks / authorize /
token / userinfo）与 `src/oidc`（RP：login / callback / logout）。

要点：

- 协议端点用 `json.raw` 出裸 JSON（discovery、jwks 不能被信封包住）。
- `state` / `code` 一次一用，重放即 401；PKCE 校验在 OP 侧。
- 自托管 OIDC 登录会 **JIT 建号**，用户名 `oidc:<tenant>:<sub>`，占位 hash 不能口令登录，
  与本地口令账号天然隔离。
- 对接外部 IdP 只改 config 的 `oidc.rp`。

完整链路的 curl 接力见 [sample 项目](../sample/index.md)，实现细节见
[OIDC 实现](../reference/oidc-implementation.md) 与 [OIDC 接入](../reference/oidc-integration.md)。

## 延伸

- [auth 模块](../sample/auth.md)（JS 端签发）/ [auth_demo 模块](../sample/auth-demo.md)（受保护路由）
- [内置 API 与鉴权](../reference/builtin-api-auth.md)
- [JS API 手册 · 鉴权与多租户](../reference/api-manual/index.md)
