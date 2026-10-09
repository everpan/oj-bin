# oidc —— OIDC RS256 原语与 OP/RP 配置透出

> 本文档由 src/bridge/ 梳理生成；权威 API 参考见 ../devkit/api-manual.md。
> 接入手册（外部 IdP / 内置 OP / 多租户 / 排障）见 ../oidc-integration.md；
> 可运行示例：sample/src/idp（内置 OP）与 sample/src/oidc（RP）。

## 是什么

`oidc` 全局对象（`src/bridge/oidc.rs`，`bootstrap.js` 934~945 行挂载）：OIDC 所需的
**RS256 JWS 签发/验签原语** + 装配期配置只读透出。密钥不出 Rust（DIP）——JS 只见
sign/verify/jwks 接口与配置值，私钥 PEM 永远拿不到。

两侧用途：

- **OP 侧**（本机当身份提供方，如 `sample/src/idp`）：`oidc.sign` 签 id_token /
  access_token，`oidc.jwks()` 出 discovery 的 jwks 文档，`oidc.issuer` /
  `oidc.clients`（client 白名单：secret/redirect_uris/tenant）驱动协议端点。
- **RP 侧**（接外部 IdP，如 `sample/src/oidc`）：`oidc.verify(token, jwks)` 验外部
  id_token，`oidc.rp`（tenant → IdP 注册：issuer/client_id/client_secret/scope）
  驱动授权码换 token 流程。

## 配置

```yaml
oidc:
  issuer: "http://localhost:9778/v1/api/idp"   # OP 标识/发现基址；只当 RP 也必填（OP 侧身份）
  private_key_path: "./config/oidc_rs256.pem"  # RS256 私钥（PKCS#8 PEM，相对 config 目录）
  rp:                                          # RP 侧：tenant → 外部 IdP 注册（可省）
    default:
      issuer: "https://idp.example.com"
      client_id: "my-app"
      client_secret: "..."
      scope: "openid profile"
  clients:                                     # OP 侧：client 白名单（可省）
    my-app:
      secret: "..."
      redirect_uris: ["http://app/v1/api/oidc/callback/"]  # 精确串，未命中绝不重定向
      tenant: "default"
```

段存在即启用；缺省 = 不启用（调用报 `oidc not configured`）。`issuer` /
`private_key_path` 为空、私钥文件不存在或非 PKCS#8 PEM → **启动 fail-fast**。
私钥只在 Rust 侧解析使用；`client_secret` 经 `oidc.rp`/`oidc.clients` 对 JS 可读——
与 `auth.jwt_secret` 同一信任级（能跑 handler 即能拿）。

## API 表

| API | 签名 | 说明 |
|---|---|---|
| `oidc.sign` | `sign(claims: object): string` | RS256 紧凑 JWS；claims 原样签（OP 自控 iss/aud/exp/nonce），header 自动带 `kid` |
| `oidc.verify` | `verify(token: string, jwks?: object): object` | 验签返回 claims。无 `jwks` 用**本机公钥**；有 `jwks` 按 token header 的 `kid` 匹配 RSA/RS256 键。算法锁定 RS256，leeway 0，验 `exp`，**不验 `aud`**（RP handler 按 client_id 自查） |
| `oidc.jwks` | `jwks(): { keys: [{kty,use,alg,kid,n,e}] }` | RFC 7517 JWKS 文档（discovery/jwks 端点直出） |
| `oidc.issuer` | `readonly string` | 配置的 issuer（getter 惰性求值） |
| `oidc.rp` | `readonly object` | RP 注册表：`tenant → {issuer, client_id, client_secret, scope}` |
| `oidc.clients` | `readonly object` | OP client 白名单：`client_id → {secret, redirect_uris, tenant}` |

`kid = sha256(base64url(n))` 前 16 hex——稳定可复现，RP 侧凭 jwks 按 kid 选钥。

## 错误

| 场景 | 形态 | 消息关键词 |
|---|---|---|
| 未配置 `oidc:` 段调用 | 抛 `Error` | `oidc not configured (config oidc: section missing)` |
| `oidc.sign` 参数非对象 | 抛 `Error` | `oidc.sign: claims must be an object` |
| `oidc.verify` 篡改 / 过期 / 非 RS256 | 抛 `Error` | `oidc.verify: …`（jsonwebtoken 原始文案） |
| `verify(token, jwks)` 但 token header 无 `kid` | 抛 `Error` | `oidc.verify: token header has no kid` |
| jwks 中无匹配 kid 的 RSA/RS256 键（含 kty 非 RSA） | 抛 `Error` | `oidc.verify: no RS256 key for kid {kid}` |
| 装配期 issuer/私钥路径为空、文件缺失、PEM 非法 | 启动 fail-fast | `oidc.issuer must not be empty` / `oidc.private_key_path …` / `parse pkcs8 pem: …` |

## 限制

- 算法**只有 RS256**（`Validation::new` 即白名单）；HMAC 系 JWT 用 `jwt` 全局
  （见 16-crypto.md）。
- leeway 0；`exp` 强制校验；`aud` 不校验——RP handler 必须自己对 client_id。
- `verify` 带 jwks 时只接受 `kty: "RSA"` 且 `alg` 为 `"RS256"` 或缺省的键——
  kty 不符（如 EC）**不会**静默回落本地钥，直接抛错。
- 只是原语：authorize/token/userinfo 协议端点、PKCE、会话桥接都是 JS 业务实现
  （参照 `sample/src/idp` / `sample/src/oidc`），RP 强制 PKCE S256。
- OIDC 的 302 跳转腿带不了自定义头——`/idp/*`、`/oidc/*` 需加进
  `auth.anonymous_paths` 与 `tenant.anonymous_paths`（细节见 oidc-integration.md）。

## 案例

### OP 侧：jwks 端点 + id_token 签发与双路验签

```ts
// src/idp/jwks.json/api.ts —— discovery 的公钥文档（标准协议端点，裸 JSON 出）
async function get() {
  json.raw(oidc.jwks());
}
export default { get };
```

```ts
// src/idp/token/api.ts（节选）—— 授权码换 id_token：claims 自组，oidc.sign 签出
const now = Math.floor(Date.now() / 1000);
const idToken = oidc.sign({
  iss: oidc.issuer, sub: userId, aud: clientId,
  iat: now, exp: now + 3600, nonce, tenant: "default",
});
```

```ts
// 验签两路：本机公钥（OP 自验） / 对端 jwks（RP 验外部 id_token）
const local = oidc.verify(idToken);
const remote = oidc.verify(idToken, oidc.jwks());   // RP 场景换成 fetch 来的 IdP jwks
```

```bash
curl http://localhost:9778/v1/api/idp/jwks.json/
# → {"keys":[{"kty":"RSA","use":"sig","alg":"RS256","kid":"…16hex…","n":"…","e":"AQAB"}]}
```

完整 OIDC 授权码 + PKCE 全链路（login → 302 → callback → 会话桥接）见
`sample/README.md` 的「OIDC 演示」与 ../oidc-integration.md。
