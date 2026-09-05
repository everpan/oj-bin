# idp 模块（内置 OP，OIDC Provider）

## 干什么

同进程身份提供方：给本服务或其他 RP 签发 OIDC 身份。六个端点，标准协议面
（discovery/jwks/token/userinfo 成功用 `json.raw` 出**裸 JSON**——RFC 6749 要求
`access_token` 是 token 响应顶层字段；错误仍走 `{code,msg,data}` 信封）。

| 端点 | 方法 | 语义 |
|---|---|---|
| `.well-known/openid-configuration` | GET | discovery：issuer/endpoints/`RS256`/`code`/`S256`（勿硬编码下游，走发现） |
| `jwks.json` | GET | 验签公钥 JWKS；`kid` = sha256(base64url(n)) 前 16 hex |
| `authorize` | GET | client_id/redirect_uri **精确白名单**（`oidc.clients`）→ PKCE S256 强制 → OP 会话门禁 → 签发 code |
| `login` | POST | `{username,password}` → `_platform.users` + `bcrypt.verify` → `Set-Cookie: IDP_SESSION`（HttpOnly/SameSite=Lax/Path=OP 路径） |
| `token` | POST | form `authorization_code`：code 一次一用（先删后判，TTL 60s）+ client 凭证 + redirect_uri 绑定 + PKCE → `{access_token, id_token, token_type, expires_in}` |
| `userinfo` | GET | Bearer access_token → `{sub, tenant}` |

## 不变量

- 白名单（client_id/redirect_uri）不命中**绝不重定向**——先 400，防开放重定向。
- 会话门禁在白名单之前：未认证统一 401 `login required`（不向未认证方泄露 client 信息）。
- code 绑定 `{client_id, redirect_uri, state, nonce, challenge, uid, tenant}`，TTL 60s，
  一次一用；token 校验顺序：grant → client 凭证 → code → redirect_uri → code↔client
  绑定（RFC 6749 §4.1.3）→ PKCE。
- token claims：`{iss, sub, aud, iat, exp, nonce, tenant}`；`aud` = client_id，
  `exp = iat + 3600`；access_token 与 id_token 同 claims 各自签名（RS256，header 带 kid）。
- 匿名：本模块路径须在 `auth.anonymous_paths` 与 `tenant.anonymous_paths`（浏览器跳转腿
  带不了 Bearer/租户头），见 `sample/config.yaml`。

## 怎么改

- **加 client**：只改 `config.yaml` 的 `oidc.clients.<id>`（secret/redirect_uris/tenant），
  不动代码。
- **换用户源**（手机号/ldap/三方）：只改 `login/api.ts` 的取数与校验段；OP 不做 JIT——
  本地账号映射归 RP（见 `../oidc/README.md`）。
- **轮换签名密钥**：换 `oidc.private_key_path` 指向的 PEM 重启；`kid` 随公钥变化，
  RP 每次 callback 都重新拉 jwks，无需协调；旧 access_token 立即失效（验签不过）。
- 本模块 `deps: {_platform}`（ownership_guard: deny 下读 `users` 表必须声明）。

## 测试

`sample/tests/oidc.test.ts`（L1，进程内）：discovery/jwks、login 成功与失败、authorize
闸门、全 code flow（含 code 重放 401 与 PKCE 错 verifier）；真 TCP 全链在
`oj/tests/oidc_e2e.rs`（独立测试目标，原因见文件头注释）。
