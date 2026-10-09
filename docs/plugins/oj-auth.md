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

## 案例

### 登录签发 Bearer token 并访问受保护路由

```yaml
# config.yaml
auth:
  jwt_secret: "ENC[...]"        # 生产必改且密封；空串启动 fail-fast
  signing_method: "HS256"
  access_token_duration: "60s"  # access token 有效期（s/m/h/d）
  refresh_token_duration: "720h"
  anonymous_paths:              # 免鉴权路径（去 /v1/api 前缀）
    - "/auth/login"             # auth 端点是普通业务路由，须显式匿名
```

```ts
// src/auth/login/api.ts —— 查用户表、bcrypt 校验、jwt.sign 签发
async function post() {
  const body = http.body || {};
  const rows = await db.table("users")
    .select(["id", "password_hash", "roles"])
    .where({ field: "username", op: "eq", value: String(body.username ?? "") })
    .all();
  const row = rows[0];
  // 用户不存在与密码错同报（不泄露用户存在性）
  if (!row || !(await bcrypt.verify(String(body.password ?? ""), String(row.password_hash || "")))) {
    json.fail(401, "invalid credentials");
    return;
  }
  let roles: string[] = [];
  try { roles = JSON.parse(String(row.roles || "[]")); } catch { roles = []; }
  json.ok({
    access_token: await jwt.sign({ sub: String(row.id), roles }),
    expires_in: jwt.accessDuration,   // 配置的 access 有效期（秒）
    user: { id: String(row.id), roles },
  });
}
export default { post };
```

非匿名路径由守卫在进 handler 前统一验签，handler 直接读 `http.user`：

```ts
// src/auth_demo/me/api.ts —— 受保护路由：守卫已过，读验签后的身份
function get() {
  json.ok({ user: http.user });   // {id, roles, claims}；未过守卫根本进不来
}
export default { get };
```

```bash
# 登录拿 token（users 表为业务约定：username / password_hash / roles，见 api-manual §8）
TOKEN=$(curl -s -X POST http://localhost:9778/v1/api/auth/login \
  -H 'Content-Type: application/json' \
  -d '{"username":"demo","password":"demo1234"}' | jq -r .data.access_token)

# 带 Bearer 访问受保护路由
curl -s http://localhost:9778/v1/api/auth_demo/me/ -H "Authorization: Bearer $TOKEN"
# → {"code":0,"data":{"user":{"id":"1","roles":["admin"], …}}}

# 不带 token → 守卫 401
curl -s http://localhost:9778/v1/api/auth_demo/me/
# → {"code":401,"msg":"missing or invalid bearer token"}
```

### 按角色收口管理端点

守卫只验「是不是合法用户」，角色判定是 handler 自己的事：

```ts
// src/admin/users/api.ts —— 仅 admin 角色可见的用户列表
async function get() {
  if (!(http.user?.roles ?? []).includes("admin")) {
    json.fail(403, "admin only");
    return;
  }
  const rows = await db.table("users").select(["id", "username", "roles"]).all();
  json.ok(rows);
}
export default { get };
```

### 浏览器 cookie 会话登录（`auth.cookie` 开启时）

cookie 形态下守卫按「匿名 → Bearer → cookie 会话」判定；登录端点负责写双 cookie
（HttpOnly 会话 + 非 HttpOnly 的 CSRF 双提交）：

```yaml
auth:
  jwt_secret: "ENC[...]"
  cookie:
    enabled: true
    name: oj_sess              # HttpOnly 会话 cookie（值 = 与 Bearer 同 secret 的 JWT）
    same_site: Lax
    secure: true               # 生产 HTTPS 打开
    csrf_cookie: oj_csrf       # 非 HttpOnly，JS 要读
    csrf_header: x-csrf-token
```

```ts
// src/auth/login/api.ts —— 校验通过后（同案例 1），同响应双发 Set-Cookie
const accessToken = await jwt.sign({ sub: uid, roles });
json.header("Set-Cookie", `oj_sess=${accessToken}; HttpOnly; SameSite=Lax; Path=/; Max-Age=86400`);
json.header("Set-Cookie", `oj_csrf=${crypto.randomHex(16)}; SameSite=Lax; Path=/; Max-Age=86400`);
json.ok({ id: uid, roles });
```

之后浏览器请求自动带 cookie；**非 GET/HEAD/OPTIONS** 须再带
`x-csrf-token: <oj_csrf cookie 值>`，否则 401 `missing or invalid csrf token`
（Bearer 命中的请求不查 CSRF）。登出端点把两个 cookie 以 `Max-Age=0` 清掉。

## 备注

- 只做守卫，不碰 db/kv；凭据签发（登录 / 刷新）在 JS 侧完成。
- `**` 跨段匹配是 v0.1.20 收紧「任意深度」为「严格一层」后引入的显式写法；旧式尾 `/*` 只命中一层。
