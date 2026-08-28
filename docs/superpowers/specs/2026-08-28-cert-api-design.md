# sample 证书管理 API 设计（生成 / renew / 备注）

日期：2026-08-28
状态：已批准（brainstorming 产出）

## 背景与目标

`oj` 的证书是 JWS 三段式（RS256 签 `{nbf, exp}`），由 `tools/oj-cert` 生成/重签，
`server/src/certificate.rs` 验签并判定 Valid/Grace/Expired。当前只有 CLI 形态。
目标：在 `sample/` 中提供**在线**证书管理 API——生成、renew、备注、列表/详情/删除，
多证书存于 sqlite。已确认的决策：

- 材料存 **sqlite 表**（不落盘文件、不入 blob）。
- **不包含**「应用到本服务」（不写 sample/config/，不触碰热重载路径）。
- 端点要求 **admin** 角色。

## 关键约束

- JS handler 全局（`json/db/http/kv/blob/bus/es/fetch/log/ws/plugins`）**无 crypto 能力**，
  RSA 密钥生成与 RS256 签名必须发生在 Rust 侧 → 新增 bridge 轴 `cert`。
- 证书格式契约（header `{"alg":"RS256","typ":"JWT"}` + payload `{nbf,exp}` + b64url no-pad）
  只在 `oj-cert` lib 一处维护；CLI 与新 bridge op 同源。
- `bootstrap.js` 必须保持 7-bit ASCII；构建仅 release；`cargo fmt`/`clippy -D warnings` 门禁。

## 方案（已选定：A）

**A**：`oj-cert` lib 抽纯内存函数，core 以路径依赖复用，bridge 挂 `globalThis.cert`。
（备选 B：core 重写 rsa/jws —— 契约两处漂移风险，弃；C：纯 JS —— 不可行。）

## 改动清单

### 1. `tools/oj-cert/src/lib.rs`（重构，CLI 行为不变）

从 `r#gen` / `renew` 中抽出并 `pub`：

- `keygen(bits: u32) -> Result<RsaPrivateKey, String>`（含 bits ≥ `MIN_BITS` 校验）
- `sign_jws(key: &RsaPrivateKey, nbf: u64, exp: u64) -> String`（现私有 `jws`）
- `private_pem(key: &RsaPrivateKey) -> Result<String, String>`（PKCS#8）
- `public_pem(key: &RsaPrivateKey) -> Result<String, String>`（SPKI）

`r#gen` / `renew` 在其上重组；现有测试与文件落盘行为（chmod 600、拒绝覆盖）不变。

### 2. core（根 crate）

- `Cargo.toml`：`oj-cert = { path = "tools/oj-cert" }`。
- 新增 `src/bridge/cert.rs`：
  - `op_cert_gen(bits: u32, nbf: u64, exp: u64) -> CertMaterial`
    （`CertMaterial { private_pem, public_pem, cert_jws }`，纯内存，不落盘）
  - `op_cert_renew(private_pem: String, nbf: u64, exp: u64) -> String`（新 cert_jws）
  - 校验：bits ≥ 2048、`exp > nbf`（复用 oj-cert 内校验）；私钥解析失败报错。
  - 单测：gen 产物 3 段式且 exp>nbf；renew 用同私钥输出不同 jws。
- `src/bridge/mod.rs`：extension! 注册两 op。
- `src/bridge/bootstrap.js`：挂 `globalThis.cert = { generate, renew }`（保持 ASCII）。

### 3. `sample/`

- `seed.sql` 追加：

```sql
CREATE TABLE IF NOT EXISTS certs (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  name TEXT NOT NULL,
  note TEXT NOT NULL DEFAULT '',
  public_pem TEXT NOT NULL,
  private_pem TEXT NOT NULL,
  cert_jws TEXT NOT NULL,
  nbf INTEGER NOT NULL,
  exp INTEGER NOT NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
-- trinity/user 角色测试账号（password_hash 复用 demo 行的同一 bcrypt 串，即密码
-- demo1234；仅供 403 用例，样例数据勿用于生产）
INSERT OR IGNORE INTO users (id, username, password_hash, roles)
  VALUES (2, 'trinity',
    '$2b$10$aKN7gpFP.dhK7Il8sc19neUPaziSONYdsfks1xm0H2COzkp2vlqV2', '["user"]');
```

- `sample/src/cert/api.ts`：
  - `get`：列表（不含 private_pem），每条附 `status`（JS 由 `nbf/exp` 对
    `Date.now()/1000` 推导：`< nbf` 或 `nbf<=now<exp` → valid；`exp<=now<exp+30d` → grace；
    其余 expired，与 server 判定一致）。
  - `post`：`{name, note?, days?=365, bits?=2048}` → `cert.generate` → 入库，
    返回 id 与元数据（不含私钥）。
- `sample/src/cert/item/api.ts`：
  - `get ?id=`：详情，含 private_pem（管理员取用）。
  - `patch {id, note}`：改备注，更新 `updated_at`。
  - `del ?id=`：删除。
- `sample/src/cert/renew/api.ts`：
  - `post {id, days?=365}`：读库中 private_pem → `cert.renew` → 更新
    `cert_jws/nbf/exp/updated_at`（公钥与 private_pem 不变）。
- `sample/global.d.ts`：补 `CertApi` 声明（`cert.generate(opts)` / `cert.renew(pem, nbf, exp)`）。

### 4. 鉴权与校验

- 全部端点先做 admin 门禁：`http.user` 缺失或 `roles` 不含 `"admin"` → 403。
- JS 层参数校验：`name` 非空（≤128 字符）、`days` 1..=3650、`note` ≤2000 字符、
  `id` 为正整数；`bits` 仅接受 2048/3072/4096。
- 双层校验：Rust 层再拦 bits 与 exp>nbf（防其他调用方绕过 JS 层）。

## 错误处理

统一 `{code,msg,data}` 信封：400 参数非法；403 非 admin；404 id 不存在；
500 op/DB 失败（`.catch((e) => json.fail(500, String(e)))`）。

## 测试

- `sample/tests/cert.test.ts`（`oj test -c sample/config.yaml -d sample/src`，沿用
  `client.login` 风格）：admin 建 → 列表可见且 status=valid → 改备注生效 →
  renew 后 exp 增长且公钥不变 → 删除后详情 404；trinity（user 角色）→ 403。
- core `src/bridge/cert.rs` 单测（`cargo test`）与 oj-cert 既有单测（`cargo test -p oj-cert`）。

## 非目标（明确不做）

- 应用到本服务自身证书（不写 sample/config/，不联动 watcher）。
- ACME/Let's Encrypt、X.509 证书、多租户隔离 certs 表。
- 私钥加密存储（样例以明文入 sqlite；生产部署需另行处理）。
