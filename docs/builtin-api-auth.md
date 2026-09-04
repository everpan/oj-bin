# 内置 API 与鉴权逻辑（新人向）

本文梳理 `only-js` 服务层**内置接口**（不走业务路由表、由 Rust 直接处理的端点）及
auth 鉴权全链路。代码依据：`server/src/lib.rs` 的 `handle()`、`server/src/auth.rs`、
`src/config.rs`。

## 一、内置接口总览

内置接口 = 不走业务路由表、由 Rust 直接处理的端点（`base` 默认 `/v1/api`）：

| 端点 | 方法 | 鉴权 | 说明 | 代码位置 |
|---|---|---|---|---|
| `{base}/health` | GET | 无 | 健康检查 + 证书状态（证书过期/宽限期仍可访问，供监控） | `lib.rs` `health_handler` |
| `{base}/auth/login` | POST | 无 | 登录：用户名+密码 → 双 token | `lib.rs` → `auth.rs::login` |
| `{base}/auth/refresh` | POST | 无 | refresh 轮换：旧换新，旧立即失效 | `auth.rs::refresh` |
| `{base}/auth/logout` | POST | 无 | 登出：删 session | `auth.rs::logout` |
| `{base}/blob/{key}` | GET | 无（公开） | blob 下载：local 直出字节 / s3 302 presign | `lib.rs` blob 分支 |
| 静态站点 | GET/HEAD | 无 | `server.app_path` 配的目录，优先级最低 | `resolve_static` |

其余所有请求进入**业务路由**：路由表命中 → 前置管线（鉴权/租户/上传）→ JS handler。

开启方式：`config.yaml` 里 `auth:` 非空才挂 auth 路由和 Bearer 守卫；`blob:` 非空才挂
下载路由。`auth` 配了但 `jwt_secret` 为空 → 启动 fail-fast，不静默跳过。

## 二、请求处理总流程（`handle()` 的分支优先级）

```mermaid
flowchart TD
    A[请求进入 axum catch-all] --> B{GET 且证书过期/宽限期?}
    B -- 是 --> B1[403 信封<br/>health 路由除外]
    B -- 否 --> C{路径是 base/auth/* 且已启用 auth?}
    C -- 是 --> C1[login/refresh/logout<br/>POST only, 其余 405]
    C -- 否 --> D{GET 且路径是 base/blob/key?}
    D -- 是 --> D1[blob 下载<br/>local 直出 / s3 302]
    D -- 否 --> E{路由表 lookup 命中?}
    E -- 冲突 --> E1[500 route conflict]
    E -- 方法不符 --> E2[405]
    E -- 命中 --> F[前置管线]
    E -- 未命中 --> G{dev 模式目录镜像兜底?}
    G -- 命中且未被 .route 替换 --> F
    G -- 否 --> H{GET/HEAD 且静态站点命中?}
    H -- 是 --> H1[返回静态文件<br/>带穿越防护]
    H -- 否 --> I[404 no route matched]

    F --> F1{auth 启用且路径非匿名?}
    F1 -- 是 --> F2{Bearer token 验签通过?}
    F2 -- 否 --> F3[401]
    F2 -- 是 --> F4[注入 http.user]
    F1 -- 否 --> F5[跳过鉴权]
    F4 --> F6{租户头启用?}
    F5 --> F6
    F6 -- 是且缺失 --> F7[400]
    F6 --> F8{body 超 max_upload?}
    F8 -- 是 --> F9[413]
    F8 -- 否 --> F10[multipart 解析<br/>文本入 body, 文件入 http.files]
    F10 --> F11[JsActor.run_module 执行 JS handler]
    F11 --> F12{结果}
    F12 -- Capture --> F13[按 status/headers/body 回写]
    F12 -- 超时 --> F14[408]
    F12 -- 其他错误 --> F15[500]
```

## 三、登录时序（`POST /auth/login`）

```mermaid
sequenceDiagram
    participant C as 客户端
    participant S as handle/auth 路由
    participant DB as 用户表 (auth.user_table)
    participant KV as KV (session 存储)

    C->>S: POST {username, password}
    S->>DB: select id, password_hash, roles where username = ?
    DB-->>S: 用户行
    Note over S: 用户不存在或 bcrypt 校验失败<br/>统一报 "invalid credentials"<br/>不泄露用户是否存在
    S->>S: 签 access token (JWT: sub/roles/iat/exp)
    S->>S: 生成 refresh token (32 随机字节 hex, 不透明串)
    S->>KV: set AUTH-SESSION:sha256(refresh)<br/>{uid, exp=now+refresh时长}
    S-->>C: 200 ok 信封<br/>{access_token, refresh_token, expires_in, user}
```

## 四、Refresh 轮换（`POST /auth/refresh`）

```mermaid
sequenceDiagram
    participant C as 客户端
    participant S as Auth
    participant DB as 用户表
    participant KV as KV

    C->>S: POST {refresh_token}
    S->>KV: get AUTH-SESSION:sha256(token)
    alt session 不存在或已过期 (惰性判定 exp)
        S-->>C: 401 invalid or expired refresh token
    else 有效
        S->>DB: select roles where id = uid
        Note over S: session 只存 uid, roles 重查库取最新
        S->>KV: del 旧 session (旧 refresh 立即失效, 一次一用)
        S->>S: 签新 token 对 + 落新 session
        S-->>C: 200 新 {access_token, refresh_token, ...}
    end
```

## 五、业务请求的 Bearer 守卫

```mermaid
flowchart TD
    A[业务请求] --> B{auth 已启用?}
    B -- 否 --> Z[直接进 handler]
    B -- 是 --> C{路径去 base 后<br/>命中 anonymous_paths?}
    C -- 是 --> Z
    C -- 否 --> D{Authorization: Bearer xxx<br/>验签通过且未过期?}
    D -- 否 --> E[401 missing or invalid bearer token]
    D -- 是 --> F["http.user = {id, roles, claims}"]
    F --> Z
```

匿名路径匹配规则（`is_anonymous`）：精确匹配，或尾部 `/*` 做**一层**前缀通配——
`/pub/*` 命中 `/pub/x`，不命中 `/pub` 本身。

## 六、关键认知

1. **双 token 模型**：access 是 JWT（无服务端状态，logout 后到期前仍有效，已知取舍）；
   refresh 是不透明随机串，服务端 session 存 KV，键 = `AUTH-SESSION:` + sha256(token)，
   可吊销、一次一用（轮换）。
2. **内置路由优先于业务路由**：`auth/*` 和 `blob/*` 在路由表查找之前被拦截，业务代码
   无法用同名路径覆盖它们。
3. **用户表约定**：`auth.user_table`（默认 `users`）需有
   `id / username / password_hash(bcrypt) / roles(JSON 数组字符串)` 四列；表名过白名单
   校验（`[A-Za-z0-9_]{1,64}`），查询全部走绑定参数。
4. **鉴权只设防 base 之内**：路径不在 `base` 前缀下（`path_no_base = None`）时不做
   Bearer 检查，交给后续静态/404 分支。
5. **信封统一**：成功走 `{code,msg,data}` ok 信封；所有失败（401/400/405/413/408/500）
   走 fail 信封，HTTP 状态码与信封 code 一致。

## 配置参考（`AuthCfg`，`src/config.rs`）

| 字段 | 默认 | 说明 |
|---|---|---|
| `jwt_secret` | 空（配了 auth 则必填，空 → fail-fast） | HMAC 签名密钥 |
| `signing_method` | `HS256` | HS256 / HS384 / HS512 |
| `access_token_duration` | `60s` | access token 有效期 |
| `refresh_token_duration` | `720h` | refresh token / session 有效期 |
| `anonymous_paths` | `[]` | 免鉴权路径（去 base 后），尾部 `/*` 一层通配 |
| `user_table` | `users` | 用户表名（标识符白名单校验） |
