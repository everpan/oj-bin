# cert —— JWS 证书签发与续签

> 本文档由 src/bridge/ 梳理生成；权威 API 参考见 ../devkit/api-manual.md。

## 是什么

`cert` 全局对象（`src/bridge/cert.rs`，`bootstrap.js` 913~918 行挂载）：JWS 证书
（header `{"alg":"RS256","typ":"JWT"}` + payload `{nbf,exp}`，b64url no-pad 三段式）
的**签发与续签原语**，桥接自 `tools/oj-cert`（格式契约的单一事实来源；CLI 与本全局
同源）。RSA 密钥生成与 RS256 签名都在 Rust 侧完成，返回纯内存字符串，**不落盘**。

用途：oj 服务的**证书门禁**（`server.public_key_path` + `server.certificate_path`，
两者必配，证书过期后 GET 进宽限期/拒绝）所需的 `cert.jws` 与密钥对，可在 handler /
任务 / `oj exec` 脚本里生成与轮换，而不必出进程调 CLI。验签与状态判定在服务端
（`serve/src/certificate.rs`），不在本全局。

## API 表

| API | 签名 | 说明 |
|---|---|---|
| `cert.generate` | `generate(bits: number, nbf: number, exp: number): { private_pem, public_pem, cert_jws }` | 生成 RSA 密钥对 + JWS 三件套（内存字符串）。`bits` 下限 2048；`nbf`/`exp` 为 Unix 秒 |
| `cert.renew` | `renew(privatePem: string, nbf: number, exp: number): string` | 读 PKCS#8 私钥**重签**新 `cert_jws`（公钥不变——服务端 `public_key_path` 无需换） |

## 错误

| 场景 | 形态 | 消息关键词 |
|---|---|---|
| `nbf` / `exp` 为负 | 抛 `Error` | `timestamp must be >= 0` |
| `exp <= nbf` | 抛 `Error` | `exp must be greater than nbf` |
| `bits < 2048` | 抛 `Error` | `bits must be >= 2048` |
| `renew` 私钥非 PKCS#8 PEM | 抛 `Error` | `parse private key (PKCS#8 PEM): …` |

## 限制

- 只签不验：验签/有效期判定是服务端证书门禁的职责；JS 侧拿到的 `cert_jws` 是
  不透明三段串。
- 纯内存返回，**落盘由调用方负责**（`fs.writeTextFile` 或部署流程）；`renew` 只换
  JWS 不换公钥，热替换证书文件即可生效（服务端热加载），私钥 PEM 须自行妥善保管。
- `nbf`/`exp` 是 Unix **秒**（普通 number 直入，不支持毫秒）。

## 案例

### 生成证书三件套并落盘

```ts
// src/admin/cert/api.ts —— 首次部署/轮换：生成 → 写 config 目录（生产走部署流程而非在线端点）
async function post() {
  const now = Math.floor(Date.now() / 1000);
  const m = await cert.generate(2048, now, now + 365 * 86400);
  await fs.writeTextFile("cert/private.pem", m.private_pem);
  await fs.writeTextFile("cert/public.pem", m.public_pem);
  await fs.writeTextFile("cert/cert.jws", m.cert_jws);
  json.ok({ exp: now + 365 * 86400 });
}
export default { post };
```

### 到期前续签（公钥不变）

```ts
// src/admin/cert/renew/api.ts —— 复用私钥重签新窗口；public_key_path 不动，服务端热加载新 jws
async function post() {
  const now = Math.floor(Date.now() / 1000);
  const priv = await fs.readTextFile("cert/private.pem");
  const jws = await cert.renew(priv, now, now + 365 * 86400);
  await fs.writeTextFile("cert/cert.jws", jws);
  json.ok({ renewed: true });
}
export default { post };
```

```yaml
# config.yaml（证书门禁消费侧；两路径必配，缺任一启动失败）
server:
  public_key_path: "./config/public.pem"
  certificate_path: "./config/cert.jws"
```
