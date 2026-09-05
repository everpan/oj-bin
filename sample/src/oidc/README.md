# oidc 模块（RP，OIDC Relying Party）

## 干什么

接 OIDC 身份源（内置 OP 或任意外部标准 IdP），换成本服务自己的 `auth` 会话。
通用 discovery 对接——换 IdP 只改 `config.yaml` 的 `oidc.rp.<tenant>`，不改代码。

| 端点 | 方法 | 语义 |
|---|---|---|
| `login` | GET | `?tenant=<rp 键>` → discovery → state/nonce/PKCE 快照进 KV（TTL 600s）→ 302 IdP authorize |
| `callback` | GET | state 一次一用 → code+verifier 换 token → `oidc.verify(id_token, jwks)`（nonce/iss/aud 对快照）→ JIT 本地行 → `issueTokens` 桥接 → 会话信封 |
| `logout` | POST | 同 `auth/logout`（删 refresh session） |

## 不变量

- **只信 state 快照**：IdP 配置（issuer/client_id/client_secret/redirect_uri）建 state
  时快照进 KV，callback 不再读 query——用户改不了已建立的登录流。
- 验签强制：jwks 拉取失败 → 502（**不回落本机验签**）；RS256/kid/exp 之外还比对
  `nonce`、`iss`（=快照 issuer）、`aud` 含快照 client_id。
- JIT 本地账号名 = **`oidc:<tenant>:<sub>`**（租户+sub 命名空间隔离，跨 IdP 不串号）；
  `password_hash` 填 `'!oidc'` 非法占位——`bcrypt.verify` 对非法 hash 恒 false，天然
  不可密码登录，零 schema 变更。
- 桥接会话不带 tenant（HS256 claims 固定 `{sub,roles}`）：租户继续走 `X-TENANT-ID` 头
  语义，业务隔离与本地登录完全同构。
- 匿名：`/oidc/*` 在 `auth.anonymous_paths` 与 `tenant.anonymous_paths`。

## 怎么改

- **换/加 IdP**：只改 `config.yaml` `oidc.rp.<tenant>`（issuer + client_id/secret/scope）；
  IdP 必须支持 code flow + PKCE S256 + RS256 + discovery。
- **改本地映射策略**（如按 email 而非 sub）：只动 `callback/api.ts` 的 JIT 段——注意保持
  命名空间键，避免跨 IdP 账号混淆。
- 会话签发收敛在 `../auth/_shared/session.ts` 的 `issueTokens`，不要在本模块复刻。
- `deps: {_platform}`（callback 读 `users` 表）。

## 测试

L1：未知 tenant 400、无效 state 401、logout；全链（302 → OP 登录 → callback 换会话 →
重放 401）在 `oj/tests/oidc_e2e.rs`（独立测试目标）。
