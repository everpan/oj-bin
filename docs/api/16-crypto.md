# jwt / bcrypt / crypto —— 密码学原语

> 本文档由 src/bridge/ 梳理生成；权威 API 参考见 ../devkit/api-manual.md。

## 是什么

三个全局对象（`src/bridge/crypto.rs`，`bootstrap.js` 920~965 行挂载）——核心只留
**原语**，业务语义（登录/刷新/守卫）在 JS 端点与 oj-auth 插件：

- **`jwt`**：HMAC JWT 签发/验签。secret/算法/有效期由 config `auth:` 段在装配期注入，
  handler 不接触密钥；`iat`/`exp` 由 Rust 侧补（JS 不可控有效期）。
- **`bcrypt`**：密码哈希与校验（Rust 侧 `spawn_blocking`，CPU 密集不卡 isolate）。
  不依赖 `auth:` 段，始终可用。
- **`crypto`**：摘要 / 随机数 / 应用层对称加密。bootstrap 对原生 `crypto` 做
  `Object.assign` 合并（原生成员保留）；不依赖 `auth:` 段，始终可用。

## 配置

仅 `jwt` 需要配置（config `auth:` 段；缺省则 `jwt.*` 调用报 `jwt not configured`）：

```yaml
auth:
  jwt_secret: "ENC[...]"          # 建议密封（oj secret seal）；空串启动 fail-fast
  signing_method: HS256           # HS256 | HS384 | HS512（其余装配期拒启）
  access_token_duration: 60s      # jwt.accessDuration；sign 的 exp 取它
  refresh_token_duration: 720h    # jwt.refreshDuration（refresh token 是业务侧不透明串）
```

`bcrypt` / `crypto` 无配置。

## API 表

### jwt

| API | 签名 | 说明 |
|---|---|---|
| `jwt.sign` | `sign(payload: { sub: string; roles?: string[] }): string` | 签发 access token；payload 至少含字符串 `sub`，`roles` 缺省空数组（非字符串元素被过滤）；`iat`/`exp` 由 Rust 补 |
| `jwt.verify` | `verify(token: string): { sub, roles, iat, exp }` | 验签 + 过期检查（leeway 0）；篡改 / 过期 / 算法不符直接抛错 |
| `jwt.accessDuration` | `readonly number` | 配置的 access 有效期（秒；getter 惰性求值） |
| `jwt.refreshDuration` | `readonly number` | 配置的 refresh 有效期（秒） |

### bcrypt（async）

| API | 签名 | 说明 |
|---|---|---|
| `bcrypt.hash` | `hash(password: string, cost?: number): Promise<string>` | 生成 bcrypt 哈希；cost 缺省库默认（`bcrypt::DEFAULT_COST` = 12） |
| `bcrypt.verify` | `verify(password: string, hash: string): Promise<boolean>` | 校验；**非法 hash 返回 `false`，不抛错** |

### crypto

| API | 签名 | 说明 |
|---|---|---|
| `crypto.sha256Hex` | `sha256Hex(s: string): string` | UTF-8 编码后的 sha256 十六进制摘要 |
| `crypto.randomHex` | `randomHex(nBytes?: number): string` | `nBytes` 字节随机数的 hex；缺省 32 字节（→ 64 字符，refresh token 用），上限 1024 字节 |
| `crypto.getRandomValues` | `getRandomValues(view: ArrayBufferView): ArrayBufferView` | WebCrypto 形态：填充任意 TypedArray view 并返回原 view（v0.1.30）；单次 ≤65536 字节；非 view 抛 `TypeError` |
| `crypto.aesGcmEncrypt` | `aesGcmEncrypt(plaintext: string, key: string): string` | AES-GCM 应用层加密（v0.1.35）：明文 UTF-8，key 为 16/32 字节原始密钥的 hex 或 base64 编码；返回 `base64(nonce12 ‖ ct ‖ tag16)` |
| `crypto.aesGcmDecrypt` | `aesGcmDecrypt(ciphertext: string, key: string): string` | 输入 `aesGcmEncrypt` 的密文返回原文；密钥错 / 篡改（GCM tag 校验失败）抛错 |

另：全局 `atob(s)` / `btoa(s)` 标准 base64（v0.1.30；非法输入抛
`InvalidCharacterError`）。

## 错误

| 场景 | 形态 | 消息关键词 |
|---|---|---|
| 未配置 `auth:` 段调用 `jwt.*` | 抛 `Error` | `jwt not configured (config auth: section missing)` |
| `jwt.sign` 的 payload 无字符串 `sub` | 抛 `Error` | `jwt.sign: payload.sub must be a string` |
| `jwt.verify` 篡改 / 过期 / 算法不符 | 抛 `Error`（jsonwebtoken 原始文案） | 如 `ExpiredSignature` / `InvalidSignature` |
| 装配期 `signing_method` 非法 | 启动 fail-fast | `auth.signing_method '{m}' not supported (HS256|HS384|HS512)` |
| 装配期 duration 非法 | 启动 fail-fast | `auth.access_token_duration: …` / `auth.refresh_token_duration: …` |
| `bcrypt.verify` hash 非法 | **返回 `false`**（不抛错） | —— |
| `crypto.getRandomValues` 非 view 参数 | 抛 `TypeError` | `crypto.getRandomValues: argument must be a TypedArray view` |
| AES key 非 hex/base64 | 抛 `Error` | `key not hex/base64: …` |
| AES key 长度非 16/32 字节 | 抛 `Error` | `aes key must be 16/32 raw bytes (AES-192/24-byte not supported; got {n})` |
| `aesGcmDecrypt` 密文非法 | 抛 `Error` | `ciphertext not base64: …` / `ciphertext too short` |
| AES-GCM 加/解密失败（含密钥错、篡改） | 抛 `Error` | `aes-gcm encrypt failed: …` / `aes-gcm decrypt failed: …` |
| `aesGcmDecrypt` 明文非 UTF-8 | 抛 `Error` | `plaintext not utf8: …` |

## 限制

- `jwt.sign` 只签 **access token**（`exp = now + access_token_duration`）；refresh
  token 按 sample 惯例用 `crypto.randomHex()` 生成不透明串，会话状态存 kv。
- `jwt.verify` leeway 0、不验 `aud`；验签结果里超过 JS 安全整数范围的数值声明会被
  出口护栏规整（雪花 id 类声明用字符串）。
- `jwt` 只支持 HMAC 系（HS256/384/512）；RS256 原语在 `oidc` 全局（见 18-oidc.md）。
- `crypto.getRandomValues` 单次上限 65536 字节（对齐 WebCrypto），超限静默截断
  （op 层 cap，JS 侧不再复核）。
- `crypto.randomHex` 上限 1024 字节。
- AES-GCM **不支持 AES-192**（上游 aes-gcm crate 未提供），24 字节密钥明确报错；
  nonce 由运行时随机生成（12 字节）并随密文打包，解密方无需单独传。
- **AES-GCM 密钥由调用方自行保管**（不进 config 专属段、不托管）——典型做法是从
  `vars.get(...)` 读（值可在 config 里用 `ENC[...]` 密封）。
- `bcrypt.hash` cost 越高越慢（每 +1 约翻倍）；cost 是 CPU 密集操作但已
  `spawn_blocking`，不会卡 isolate 线程。

## 案例

### 登录端点：bcrypt 校验 + jwt 签发

```ts
// src/auth/login/api.ts —— 查用户 → 校验密码 → 签 access + 发不透明 refresh
async function post() {
  const { username, password } = http.body ?? {};
  const row = db.table("users").select(["id", "roles", "password_hash"])
    .where("username", "=", String(username ?? "")).first();
  if (!row || !(await bcrypt.verify(String(password ?? ""), row.password_hash))) {
    json.fail(401, "bad credentials");
    return;
  }
  const access = jwt.sign({ sub: String(row.id), roles: row.roles ?? [] });
  const refresh = crypto.randomHex();                       // 64 字符不透明串
  await kv.set("AUTH-SESSION:" + crypto.sha256Hex(refresh), String(row.id),
    { ex: jwt.refreshDuration });
  json.ok({ access, refresh, expires_in: jwt.accessDuration });
}
export default { post };
```

```yaml
# config.yaml
auth:
  jwt_secret: "ENC[...]"
  signing_method: HS256
  access_token_duration: 60s
  refresh_token_duration: 720h
```

```bash
curl -X POST -H 'Content-Type: application/json' \
  -d '{"username":"demo","password":"demo1234"}' \
  http://localhost:9778/v1/api/auth/login/
# → {"code":0,"data":{"access":"eyJ…","refresh":"…64hex…","expires_in":60}}
```

### 注册 / 改密：bcrypt 哈希落库

```ts
// src/auth/register/api.ts
async function post() {
  const { username, password } = http.body ?? {};
  if (!username || !password) { json.fail(400, "username/password required"); return; }
  const hash = await bcrypt.hash(String(password));         // cost 缺省 12
  const id = db.table("users").insert({ username, password_hash: hash, roles: ["user"] });
  json.ok({ id });
}
export default { post };
```

### 字段级 AES-GCM 加密落库

```ts
// src/user/ssn/api.ts —— 密钥从 vars 读（config 里 ENC[...] 密封，运行期自动解封）
async function post() {
  const key = vars.get("FIELD_KEY");                        // 32 字节 hex
  if (!key) { json.fail(500, "FIELD_KEY not set"); return; }
  const blob = crypto.aesGcmEncrypt(String(http.body.ssn), key);
  db.table("profile").where("id", "=", String(http.body.id)).update({ ssn_enc: blob });
  json.ok({ bytes: blob.length });
}
async function get() {
  const key = vars.get("FIELD_KEY");
  const row = db.table("profile").select(["ssn_enc"]).where("id", "=", String(http.query.id)).first();
  json.ok({ ssn: row ? crypto.aesGcmDecrypt(row.ssn_enc, key) : null });
}
export default { post, get };
```

```yaml
# config.yaml
vars:
  FIELD_KEY: "ENC[...]"      # 明文为 64 位 hex（32 字节），oj secret seal 生成
```
