# sample 内置 OIDC 设计（OP + RP 双角色，多租户可用）

日期：2026-09-05
状态：设计定稿（已与需求方对齐四个决策点，见 §1）
关联：`docs/devkit/api-manual.md` §8（鉴权）、`sample/src/auth/`（现有会话链路）、
`server/src/lib.rs`（前置管线）

## 1. 需求与已定决策

在 `sample/` 下实现 OIDC，同进程扮演双角色，且必须在多租户（`tenant.enable: true`）下工作：

| 决策点 | 结论 | 理由 |
|---|---|---|
| 角色 | **OP + RP 都要** | sample 既作为身份提供方（对外签发），又作为 RP（接外部 IdP） |
| RP 对接方式 | **通用 discovery** | 只配 issuer + client_id/secret，endpoints 从 `/.well-known/openid-configuration` 自动发现 |
| 多租户 | **硬需求** | 见 §6：浏览器跳转腿带不了 `X-TENANT-ID`，必须给 server 加豁免机制 |
| 部署形态 | **同进程双角色** | 一条命令跑通；OP 与 RP 各自独立模块目录 |

否决的备选：纯 JS 不动 core（`jwt.sign` claims 固定、无 RS256/HMAC 原语，OP 不可行）；
OP/RP 做成 Rust 内置端点（违背「业务逻辑在 JS handler」的框架理念）。

## 2. 架构与数据流

```
浏览器/客户端
  │ GET /v1/api/oidc/login?tenant=acme            [tenant/auth 豁免]
  │   校验 tenant → 建 state/nonce/PKCE（KV，绑定 IdP 配置快照）
  │   fetch <issuer>/.well-known/openid-configuration → 302 authorization_endpoint
  ▼
OP  GET /v1/api/idp/authorize?client_id&redirect_uri&response_type=code&…   [豁免]
  │   白名单/会话校验 → 签发 code（KV，一次一用）→ 302 redirect_uri?code&state
  │   （无登录会话 → 401；调用方先 POST /idp/login 建 cookie 会话再回 authorize）
  ▼
RP  GET /v1/api/oidc/callback?code&state          [豁免]
  │   KV 查 state（一次一用）恢复 {tenant, nonce, verifier, issuer, client_id, client_secret}
  │   POST token endpoint（form：code + code_verifier + redirect_uri + client_id + client_secret）
  │   oidc.verify(id_token, jwks)（iss/aud/exp/nonce 全验）
  │   JIT 映射本地 users 行 → issueTokens() 桥接自家会话 → json.ok(token 响应)
```

**自举演示**：`rp.default` 的 issuer 指向自身 OP（fetch 回环 localhost；reqwest client 已
`no_proxy`）。可另配 `acme` 指向任意外部标准 IdP，演示通用 discovery。

## 3. Core 改动（Rust）

### 3.1 `oidc` 配置段（`src/config.rs` + `oj/src/server_cmd.rs` 装配注入）

```yaml
oidc:                                            # 段存在即启用；缺 private_key_path 启动报错
  issuer: "http://localhost:9778/v1/api/idp"     # OP 身份标识（也是本机 OP 的对外 URL）
  private_key_path: "./config/oidc_rs256.pem"    # PKCS#8 RSA PEM；公钥由私钥推导，不单独配
  rp:                                            # RP：tenant → 外部 IdP 客户端注册
    default:
      issuer: "http://localhost:9778/v1/api/idp"
      client_id: "sample-rp"
      client_secret: "rp-secret"                 # 明文存 config（与 auth.jwt_secret 同信任级）
      scope: "openid profile"
  clients:                                       # OP：client 白名单 + tenant 绑定
    sample-rp:
      secret: "rp-secret"
      redirect_uris: ["http://localhost:9778/v1/api/oidc/callback"]   # 精确串白名单
      tenant: "default"
```

装配期读 PEM（`rsa::RsaPrivateKey::from_pkcs8_pem`，同 `cert.renew` 既有路径；`oj-cert gen`
的 `private.pem` 即 PKCS#8 可直接用）注入 `StableState.oidc: Arc<OidcCfg>`（构造期，遵守
不可变红线）。`oidc:` 缺省 = 不挂 `oidc` 全局（调用报错），RP/OP 路由自然不可用。

### 3.2 `oidc` JS 全局（`src/bridge/oidc.rs` 新模块 + bootstrap.js 挂载）

| API | 签名 | 语义 |
|---|---|---|
| `oidc.sign` | `sign(claims: object): string` | RS256 compact JWS，claims 原样签（OP 自控 iss/aud/exp/nonce） |
| `oidc.verify` | `verify(token: string, jwks?: object): object` | 验签并返回 claims。无 jwks 用本机公钥；有 jwks 按 `kid` 匹配 + `jsonwebtoken::DecodingKey::from_jwk`。算法锁定 RS256（header.alg ≠ RS256 拒绝，防混淆）、leeway 0、验 exp |
| `oidc.jwks` | `jwks(): object` | 本机公钥 JWKS `{keys:[{kty:"RSA",n,e,kid,alg:"RS256",use:"sig"}]}` |
| `oidc.issuer` | `readonly string` | 配置透出 |
| `oidc.rp` | `readonly Record<tenant, {issuer, client_id, client_secret, scope}>` | 配置透出（secret 进 JS 作用域与 jwt_secret 同级信任） |

`kid` 生成：`crypto.sha256Hex(n).slice(0, 16)`（n 为 modulus base64url 串）。

`oidc.verify` 用 `jsonwebtoken`（已是依赖）实现；`from_jwk` 由其 9.x 提供。

### 3.3 `tenant.anonymous_paths`（`server/src/lib.rs` + config）

与 `auth.anonymous_paths` 完全同构：去 `{base}` 前缀、尾 `/*` 为一层通配、命中则跳过租户头
强制。`tenant` 段新增可选字段 `anonymous_paths: []`（缺省空 = 行为不变，零破坏）。

动机：`tenant.enable` 时全部 `{base}` 请求强制 `X-TENANT-ID`（`server/src/lib.rs:333`），
而 IdP 302 回跳的浏览器请求天然带不了自定义头。豁免只用于**浏览器跳转腿**；业务请求仍按
现有契约带头。

## 4. OP 模块（`sample/src/idp/`，manifest `name: idp`，deps 声明 `_platform`）

| 路由（目录镜像） | 方法 | 职责 |
|---|---|---|
| `idp/.well-known/openid-configuration` | get | discovery：issuer、authorization_endpoint、token_endpoint、jwks_uri、userinfo_endpoint、`id_token_signing_alg_values_supported:["RS256"]`、`response_types_supported:["code"]` |
| `idp/jwks.json` | get | `oidc.jwks()` 直出 |
| `idp/authorize` | get | 校验 `response_type=code`、`scope` 含 openid、`client_id` 与 `redirect_uri` 精确白名单（config `clients`）；`code_challenge_method` 必须 `S256`；读 cookie 会话（**无 → 401 `login required`**，API 风格：调用方先 `POST /idp/login` 持 cookie 重来）；签发 code（KV `OJ-OIDC:CODE:<code>`，值 `{client_id, redirect_uri, state, nonce, challenge, uid, tenant, exp}`，TTL 60s）→ 302 `redirect_uri?code&state` |
| `idp/login` | post | body `{username, password}` → users 表 + `bcrypt.verify` → `Set-Cookie: IDP_SESSION=<rand>; HttpOnly; Path=/v1/api/idp; Max-Age=<jwt.refreshDuration>`，KV `OJ-IDP:SESS:<rand>` → `{uid}`，TTL 同 |
| `idp/token` | post | form `grant_type=authorization_code`（仅此 grant）：`code` 一次一用（先 del 再判）、`client_id`/`client_secret` 匹配、`redirect_uri` 与 code 一致、`code_verifier` sha256 后等于 challenge → `oidc.sign` 签 `access_token` 与 `id_token`（claims：`iss/sub/aud/iat/exp/nonce/tenant`；aud = client_id；exp = iat + 3600）→ `{access_token, id_token, token_type:"Bearer", expires_in:3600}` |
| `idp/userinfo` | get | Bearer access_token → `oidc.verify`（本机公钥）→ `{sub, tenant}` |

跳过项（YAGNI，出现真需求再加）：consent 页（登录即同意）、refresh_token grant、
end_session、client_credentials、JWKS 轮换（kid 机制已就位，轮换是配置问题）。

## 5. RP 模块（`sample/src/oidc/`，manifest `name: oidc`，deps 声明 `_platform`）

| 路由 | 方法 | 职责 |
|---|---|---|
| `oidc/login` | get | `?tenant=X` → `oidc.rp[X]`（未知 → 400）；verifier=`crypto.randomHex(32)`、challenge=base64url(sha256(verifier))（`_shared` 小工具 hex→base64url）；state=`crypto.randomHex()`；KV `OJ-OIDC:STATE:<state>` = IdP 配置**快照**+`{nonce, verifier, challenge, tenant, exp}`，TTL 600s → fetch discovery → 302 authorization_endpoint |
| `oidc/callback` | get | state 查 KV（一次一用：先 del）；POST token endpoint（form 编码，`fetch` body 为字符串）；`oidc.verify(id_token, jwks)`（jwks 取自 discovery 的 jwks_uri）→ 校验 `nonce`/`iss`=快照 issuer/`aud` 含 client_id → JIT：按 id_token `sub` 查 users，无则 INSERT（`password_hash` 填 `'!oidc'` 非法占位——`bcrypt.verify` 对非法 hash 恒 false，天然不可密码登录，零 schema 变更）→ `issueTokens(row.id, roles)`（复用 `auth/_shared/session.ts`）→ `json.ok(token 响应)`（callback 是跳转腿终点，浏览器直接看到信封 JSON，非浏览器调用方同样适用） |
| `oidc/logout` | post | 同 `auth/logout` 语义（删 refresh session） |

**桥接会话不带 tenant**（刻意）：`issueTokens` 的 HS256 claims 固定 `{sub, roles}`，不改
core Claims。租户继续走既有 `X-TENANT-ID` 头语义；OIDC 登录只负责认证与租户到 IdP 的
路由。claims 级 tenant（`http.user.claims.tenant`）需扩 Claims 结构，明确 out of scope。

## 6. 多租户语义（汇总）

- **RP 侧**：tenant 决定用哪个 IdP（`oidc.rp.<tenant>`）。tenant 从 query 进入豁免路由，
  建 state 时快照进 KV，callback 只信快照——query 不可伪造已建立的登录流程。
- **OP 侧**：tenant 来自 client 注册（`clients.<id>.tenant`），进入 id_token/access_token
  claims。client 是租户的身份接入身份。
- **豁免边界**：`tenant.anonymous_paths` 仅豁免跳转腿（`/oidc/*`、`/idp/*`、
  `/idp/.well-known/*`）；登录后的业务请求（含 `/auth/*`）照常强制租户头，与现状一致。
- 演示链：`default` tenant 自举本机 OP；`acme` tenant 配外部 IdP（测试用 httptest 桩）。

## 7. 错误处理（全部信封化）

| 场景 | 返回 |
|---|---|
| 未知 tenant / 缺 state / 缺 code / response_type ≠ code / scope 无 openid | 400 |
| redirect_uri 不在白名单（**不重定向**，直接报错——OAuth 惯例） | 400 |
| code 过期/已用、state 无效/过期、登录会话无效、id_token 验签失败/nonce 不符 | 401 |
| 上游 IdP discovery/token 非 2xx | 502 |
| discovery 不可达 / fetch 网络错 | 502 |

**VERIFY 项（实现首日验证）**：302 经 `json.header("Location", url)` + `code=302` 信封
（HTTP 状态 = code 的既有映射）是否透传 Location。若不通，在 server `capture_response` 补
对 3xx 的 Location 透传（预计现有 `Capture.headers` 通路已覆盖，故只标验证不预设改动）。

## 8. 测试

- **Rust 单测**（`src/bridge/oidc.rs` `#[cfg(test)]`，current_thread）：
  sign→verify roundtrip；jwks 路径验证（from_jwk）；篡改/过期/alg≠RS256/kid 不匹配拒绝；
  `oidc:` 缺 PEM fail-fast；`oidc:` 段缺省不挂全局。测试用 RSA 密钥现场生成（rsa crate）。
- **server 单测**：`tenant.anonymous_paths` 匹配语义（精确、尾 `/*` 一层、不命中），
  镜像 `auth.anonymous_paths` 既有用例。
- **Rust e2e**（`oj/tests/e2e.rs` 新用例）：起真 server（oidc 配置 + 生成密钥 + SQLite
  内存库 + seed 用户）→ reqwest（手动 cookie jar、不自动跟随重定向）驱动
  login→authorize→login→authorize→callback 全链 → 断言最终信封与本地会话可用
  （带租户头访问受保护路由）。
- **L1**（`sample/tests/oidc.test.ts`，进程内）：discovery/jwks 内容、`/idp/login` 成功/
  失败信封、tenant 豁免路径行为、`/oidc/login` 未知 tenant 400。fetch 回环（callback 全链）
  归 Rust e2e（`oj test` 无 TCP listener）。
- **L2 vitest**：跳过（RP 逻辑被 e2e 覆盖；不重复建 mock 层）。

## 9. 文件清单

**Core**：`src/config.rs`（oidc/tenant 字段）、`src/bridge/oidc.rs`（新）、
`src/bridge/mod.rs`（注册）、`src/bridge/bootstrap.js`（挂 `oidc` 全局，ASCII）、
`server/src/lib.rs`（tenant 豁免）、`oj/src/server_cmd.rs`（PEM 装配注入）。

**Sample**：`src/idp/`（manifest.yaml + 5 个 api.ts + `_shared/`）、`src/oidc/`
（manifest.yaml + 3 个 api.ts，复用 `auth/_shared/session.ts` 的 issueTokens——跨模块导入
`../auth/_shared/session`；**构建顺序**：`oj build auth` → `oj build oidc`/`oj build idp`，
跨模块相对导入要求目标模块先构建）、`config.yaml`（oidc 段 + tenant/auth anonymous_paths）、
`config/oidc_rs256.pem`（demo 私钥，oj-cert 生成，做法同现有示例证书）、
`sample/tests/oidc.test.ts`、`sample/README.md` 增补演示步骤（curl 全链：cookie jar +
不跟随重定向逐步走）。

**文档**：`user-manual.md`（oidc 配置段 + 演示）、`devkit/api-manual.md`（`oidc` 全局）、
`docs/dev-guide.md`（tenant.anonymous_paths + oidc 全局一行）。

## 10. Out of scope（出现真需求再做）

consent 页、refresh_token / client_credentials grant、OP end_session、JWKS 轮换运维、
claims 级 tenant（扩 Claims）、RP 对外部 IdP 的完整回归套件（httptest 桩已够演示）、
`acme` 外部 IdP 的真实配置样例（留注释）。
