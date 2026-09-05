# sample 内置 OIDC 实现计划（OP + RP 双角色，多租户可用）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 sample 下同进程实现 OIDC OP（`src/idp/`）与 RP（`src/oidc/`），多租户开启时全链路可用；core 仅加 `oidc` 配置段/全局、`tenant.anonymous_paths` 豁免、`json.raw` 三件事。

**Architecture:** 密钥与验签留在 Rust（`oidc` 全局只暴露 sign/verify/jwks 接口面，经 `Extras` 构造期注入，与 `auth:` → `jwt` 同构）；OP/RP 业务逻辑全部是 JS handler；浏览器跳转腿用 `tenant.anonymous_paths` 豁免租户头，租户经 state 快照在 KV 中传递。

**Tech Stack:** rsa 0.9（workspace）、jsonwebtoken 9（RS256 + `from_jwk`）、base64 0.23、deno_core op、axum Pipeline、oj L1 测试运行器、reqwest e2e。

**Spec:** `docs/superpowers/specs/2026-09-05-sample-oidc-design.md`

## Global Constraints

- 全程 `--release`（本仓库禁止 debug 构建；rusty_v8 静态库不可用）。
- 每任务完成线：`cargo fmt --check` + `cargo clippy --all-targets -D warnings` +
  `cargo test --release --workspace -- --skip infinite_loop` 全绿。
- TDD red-first：先写失败测试 → 确认失败 → 最小实现 → 确认通过 → commit。
- commit 尾随 `unix@vip.qq.com ai`。
- `bootstrap.js` 必须 7-bit ASCII，注释一律英文。
- `panic = "unwind"` 不动；`StableState` 字段只能构造期注入。
- JS 侧 SQL：sqlite 占位符 `?`，值一律绑定参数；动态标识符只走 `db.table` 白名单。
- 密钥不出 Rust：JS 拿不到 PEM/私钥，只拿签好/验好的结果。
- 文件内引用行号以写作时为准（config.rs / app.rs / server/lib.rs），执行时以符号名定位。

## SOLID 映射（执行者自查清单）

| 原则 | 在本计划中的落点 |
|---|---|
| SRP | `src/bridge/oidc.rs` 只做 RS256 原语与配置态；OP/RP 各自独立模块目录；`auth/_shared/util.ts` 只放通用小工具 |
| OCP | 豁免/匿名匹配是数据驱动的路径列表（`tenant.anonymous_paths`），加路径不改代码；`oidc.verify` 支持 jwks 参数即支持任意远端 IdP，无需改代码 |
| LSP | 不新增继承层；op 消费 `Arc<StableState>` 与既有 op 同一状态契约 |
| ISP | `oidc` 全局面最小（sign/verify/jwks/issuer/rp/clients），无胖对象 |
| DIP | JS handler 依赖注入的 `oidc` 全局抽象（密钥在 Rust）；`OidcState` 经 `Extras` 注入（与 `jwt` 同一依赖倒置模式）；server 端 `path_matches` 与 oj-auth 插件 `is_anonymous` 各自持有同一 6 行纯函数语义——插件只依赖 oj-plugin-ffi，不能反向依赖 server crate，刻意不复用（注释注明） |

## 分阶段与集中审查

| Phase | 内容 | 集中审查点（Phase Gate） |
|---|---|---|
| 1 | core：oidc 配置段 + `oidc` 全局原语 + `json.raw` | Gate 1 |
| 2 | server：`tenant.anonymous_paths` 豁免 | Gate 2 |
| 3 | sample OP：`src/idp/` 六端点 + L1 | Gate 3（含 302 VERIFY 实测） |
| 4 | sample RP：`src/oidc/` + config 接线 | Gate 4 |
| 5 | Rust e2e 全链路 + 文档 | Gate 5（终审） |

**每个 Gate 的固定动作（不得跳过）：**

```bash
cargo fmt --check && cargo clippy --all-targets -D warnings && \
cargo test --release --workspace -- --skip infinite_loop
```

全绿后对本 Phase 的 diff 跑一次代码审查（code-review skill 或人工过一遍），审查结论记录在
commit message 或 PR 描述里；有 finding 先修再进下一 Phase。Phase 5 Gate 额外过一遍
安全自查：密钥不入 JS/日志、豁免面最小、code/state/会话一次一用、302 的 Location 只拼白名单里的 URL。

---

## Phase 1 — core：oidc 配置段 + `oidc` 全局 + `json.raw`

### Task 1: config.rs — `OidcSection` 与 `TenantCfg.anonymous_paths` 解析

**Files:**
- Modify: `src/config.rs`（`TenantCfg` 定义约 219-231 行；`Config` 结构约 260-282 行；tests 约 323 行起）

**Interfaces:**
- Produces: `config::OidcSection { issuer: String, private_key_path: String, rp: HashMap<String, OidcRpCfg>, clients: HashMap<String, OidcClientCfg> }`；
  `config::OidcRpCfg { issuer, client_id, client_secret, scope }`（scope 默认 `"openid"`）；
  `config::OidcClientCfg { secret, redirect_uris: Vec<String>, tenant }`；
  `TenantCfg.anonymous_paths: Vec<String>`（默认空）。Task 2/4 消费。

- [ ] **Step 1: 写失败测试**（`src/config.rs` tests 模块内追加）

```rust
#[test]
fn oidc_section_and_tenant_anonymous_paths_parse() {
    let c: Config = serde_yaml::from_str(
        "oidc:\n\
         \x20 issuer: \"https://idp.example\"\n\
         \x20 private_key_path: \"config/oidc.pem\"\n\
         \x20 rp:\n\
         \x20   acme:\n\
         \x20     issuer: \"https://acme.example\"\n\
         \x20     client_id: \"cid\"\n\
         \x20     client_secret: \"sec\"\n\
         \x20 clients:\n\
         \x20   web:\n\
         \x20     secret: \"s2\"\n\
         \x20     redirect_uris: [\"http://x/cb\"]\n\
         \x20     tenant: \"acme\"\n\
         tenant:\n\
         \x20 enable: true\n\
         \x20 anonymous_paths: [\"/oidc/*\"]\n",
    )
    .unwrap();
    let o = c.oidc.as_ref().unwrap();
    assert_eq!(o.issuer, "https://idp.example");
    assert_eq!(o.rp["acme"].client_id, "cid");
    assert_eq!(o.rp["acme"].scope, "openid"); // 缺省 scope
    assert_eq!(o.clients["web"].redirect_uris[0], "http://x/cb");
    assert_eq!(c.tenant.anonymous_paths, vec!["/oidc/*".to_string()]);
}

#[test]
fn oidc_section_absent_is_none_and_tenant_anon_defaults_empty() {
    let c: Config = serde_yaml::from_str("tenant:\n  enable: true\n").unwrap();
    assert!(c.oidc.is_none());
    assert!(c.tenant.anonymous_paths.is_empty());
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --release --lib oidc_section`
Expected: FAIL（`OidcSection`/`anonymous_paths` 不存在，编译错误）

- [ ] **Step 3: 最小实现**

`TenantCfg`（`src/config.rs:219`）加字段与默认值：

```rust
/// 浏览器跳转腿豁免（去 base 后路径；尾 "/*" 一层通配）——OIDC 回跳带不了自定义头。
pub anonymous_paths: Vec<String>,
```

`impl Default for TenantCfg` 加 `anonymous_paths: Vec::new(),`。

`Config` 前新增三个类型（放在 `AuthCfg` 之后）：

```rust
/// RP 客户端注册：tenant → 外部 IdP（issuer + 凭证）。
#[derive(Debug, Deserialize, Clone)]
pub struct OidcRpCfg {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    #[serde(default = "default_oidc_scope")]
    pub scope: String,
}

fn default_oidc_scope() -> String {
    "openid".into()
}

/// OP 侧 client 白名单：redirect_uri 精确串 + 租户绑定。
#[derive(Debug, Deserialize, Clone)]
pub struct OidcClientCfg {
    pub secret: String,
    pub redirect_uris: Vec<String>,
    pub tenant: String,
}

/// OIDC（spec 2026-09-05 §3.1）：段存在即启用；private_key_path 相对 config 目录。
#[derive(Debug, Deserialize, Default, Clone)]
#[serde(default)]
pub struct OidcSection {
    pub issuer: String,
    pub private_key_path: String,
    pub rp: HashMap<String, OidcRpCfg>,
    pub clients: HashMap<String, OidcClientCfg>,
}
```

`Config` 加字段：`pub oidc: Option<OidcSection>,`（`auth` 字段之后）。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --release --lib oidc_section`
Expected: PASS（2 个用例）

- [ ] **Step 5: Commit**

```bash
git add src/config.rs
git commit -m "feat(config): oidc 段（rp/clients/issuer/private_key_path）+ tenant.anonymous_paths 解析

unix@vip.qq.com ai"
```

### Task 2: `src/bridge/oidc.rs` — `OidcState`（PEM 装载 / kid / JWKS / 配置透出）

**Files:**
- Create: `src/bridge/oidc.rs`
- Modify: `src/bridge/mod.rs`（加 `mod oidc;`；`StableState` 加字段；`Extras` 加字段；两处构造点接线）

**Interfaces:**
- Consumes: `config::OidcSection`（Task 1）。
- Produces: `bridge::oidc::OidcState`，字段 `pub issuer: String`、`pub rp: HashMap<String, RpClient>`、`pub clients: HashMap<String, OidcClient>`；
  `RpClient { issuer, client_id, client_secret, scope }`、`OidcClient { secret, redirect_uris, tenant }`（字段 pub）；
  `OidcState::from_section(cfg: &OidcSection, config_dir: &Path) -> Result<Self, String>`；
  `fn jwks(&self) -> serde_json::Value`；`fn signing_pem(&self) -> &str`；
  `fn n_e(&self) -> (Vec<u8>, Vec<u8>)`（大端字节）。Task 3 的 op 消费。

- [ ] **Step 1: 写失败测试**（新文件 `src/bridge/oidc.rs` 的 `#[cfg(test)]`）

```rust
//! oidc 全局对象：RS256 JWS 签发/验签原语 + 装配期配置透出（spec 2026-09-05 S3.2）。
//! Keys stay in Rust (DIP): JS only sees sign/verify/jwks interfaces.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{OidcClientCfg, OidcRpCfg, OidcSection};
    use rsa::pkcs8::EncodePrivateKey;

    fn write_key(dir: &std::path::Path) -> String {
        let key = rsa::RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).unwrap();
        let pem = key.to_pkcs8_pem(rsa::pkcs8::LineEnding::LF).unwrap();
        let path = dir.join("oidc_rs256.pem");
        std::fs::write(&path, pem.as_bytes()).unwrap();
        path.to_str().unwrap().to_string()
    }

    fn section(dir: &std::path::Path, key_path: &str, issuer: &str) -> OidcSection {
        OidcSection {
            issuer: issuer.into(),
            private_key_path: key_path.into(),
            rp: [(
                "default".into(),
                OidcRpCfg {
                    issuer: issuer.into(),
                    client_id: "sample-rp".into(),
                    client_secret: "rp-secret".into(),
                    scope: "openid profile".into(),
                },
            )]
            .into_iter()
            .collect(),
            clients: [(
                "sample-rp".into(),
                OidcClientCfg {
                    secret: "rp-secret".into(),
                    redirect_uris: vec!["http://h/v1/api/oidc/callback".into()],
                    tenant: "default".into(),
                },
            )]
            .into_iter()
            .collect(),
        }
    }

    #[test]
    fn from_section_loads_key_and_exposes_config() {
        let dir = std::env::temp_dir().join(format!("oj-oidc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let key_path = write_key(&dir);
        let st = OidcState::from_section(&section(&dir, &key_path, "http://h/v1/api/idp"), &dir)
            .unwrap();
        assert_eq!(st.issuer, "http://h/v1/api/idp");
        assert_eq!(st.rp["default"].client_id, "sample-rp");
        assert_eq!(st.clients["sample-rp"].tenant, "default");
        let jwks = st.jwks();
        assert_eq!(jwks["keys"][0]["kty"], "RSA");
        assert_eq!(jwks["keys"][0]["alg"], "RS256");
        assert_eq!(jwks["keys"][0]["kid"].as_str().unwrap().len(), 16);
        assert!(st.signing_pem().starts_with("-----BEGIN"));
        let (n, e) = st.n_e();
        assert!(!n.is_empty() && !e.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn from_section_fail_fast_on_bad_inputs() {
        let dir = std::env::temp_dir().join(format!("oj-oidc-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let key_path = write_key(&dir);
        // issuer 空
        let e = OidcState::from_section(&section(&dir, &key_path, ""), &dir).unwrap_err();
        assert!(e.contains("issuer"), "{e}");
        // key 文件不存在
        let e = OidcState::from_section(&section(&dir, "nope.pem", "http://h"), &dir).unwrap_err();
        assert!(e.contains("oidc_rs256.pem").or(e.contains("nope.pem")), "{e}");
        // 非法 PEM
        let bad = dir.join("bad.pem");
        std::fs::write(&bad, "not a pem").unwrap();
        let e = OidcState::from_section(&section(&dir, "bad.pem", "http://h"), &dir).unwrap_err();
        assert!(e.contains("pkcs8") || e.contains("private key"), "{e}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
```

同时在 `src/bridge/mod.rs`：加 `mod oidc;`、`StableState` 加
`pub oidc: Option<Arc<oidc::OidcState>>,`（`jwt` 字段后）、`Extras` 加同名字段、
两处构造点分别加 `oidc: extras.oidc,` 与 `oidc: None,`（跟 `jwt` 行）。
（此时编译过、测试红——`OidcState` 还没实现。）

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --release --lib oidc`
Expected: FAIL（编译错误：`OidcState` 未定义）

- [ ] **Step 3: 最小实现**（`src/bridge/oidc.rs` 主体）

```rust
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::Path;

/// RP 侧：tenant → 外部 IdP 客户端注册（JS 经 oidc.rp 只读透出）。
pub struct RpClient {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    pub scope: String,
}

/// OP 侧：client 白名单（redirect_uri 精确串 + 租户绑定）。
pub struct OidcClient {
    pub secret: String,
    pub redirect_uris: Vec<String>,
    pub tenant: String,
}

/// RS256 密钥与配置态。构造期注入 StableState 后不可变。
pub struct OidcState {
    pub issuer: String,
    pub rp: HashMap<String, RpClient>,
    pub clients: HashMap<String, OidcClient>,
    signing_pem: String,
    kid: String,
    n: Vec<u8>,
    e: Vec<u8>,
}

fn b64u(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

impl OidcState {
    pub fn from_section(
        cfg: &crate::config::OidcSection,
        config_dir: &Path,
    ) -> Result<Self, String> {
        if cfg.issuer.trim().is_empty() {
            return Err("oidc.issuer must not be empty".into());
        }
        if cfg.private_key_path.trim().is_empty() {
            return Err("oidc.private_key_path must not be empty".into());
        }
        let kp = Path::new(&cfg.private_key_path);
        let kp = if kp.is_absolute() { kp.to_path_buf() } else { config_dir.join(kp) };
        let pem = std::fs::read_to_string(&kp)
            .map_err(|err| format!("oidc.private_key_path ({}): {err}", kp.display()))?;
        // PKCS#8 解析校验（与 cert.renew 同一入口）；签名直接用 PEM（jsonwebtoken
        // from_rsa_pem 同时接受 PKCS#1/PKCS#8）。
        rsa::RsaPrivateKey::from_pkcs8_pem(&pem)
            .map_err(|err| format!("oidc private key ({}): parse pkcs8 pem: {err}", kp.display()))?;
        let key = rsa::RsaPrivateKey::from_pkcs8_pem(&pem).unwrap();
        let pubk = rsa::RsaPublicKey::from(&key);
        let n = pubk.n().to_bytes_be();
        let e = pubk.e().to_bytes_be();
        let n_b64u = b64u(&n);
        // kid = sha256(base64url(n)) 前 16 hex——稳定可复现，够 demo 分辨密钥。
        let kid = {
            let d = Sha256::digest(n_b64u.as_bytes());
            d.iter().map(|b| format!("{b:02x}")).collect::<String>()[..16].to_string()
        };
        Ok(Self {
            issuer: cfg.issuer.clone(),
            rp: cfg
                .rp
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        RpClient {
                            issuer: v.issuer.clone(),
                            client_id: v.client_id.clone(),
                            client_secret: v.client_secret.clone(),
                            scope: v.scope.clone(),
                        },
                    )
                })
                .collect(),
            clients: cfg
                .clients
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        OidcClient {
                            secret: v.secret.clone(),
                            redirect_uris: v.redirect_uris.clone(),
                            tenant: v.tenant.clone(),
                        },
                    )
                })
                .collect(),
            signing_pem: pem,
            kid,
            n,
            e,
        })
    }

    /// RFC 7517 JWKS 文档（discovery/jwks 端点直出）。
    pub fn jwks(&self) -> serde_json::Value {
        serde_json::json!({
            "keys": [{
                "kty": "RSA", "use": "sig", "alg": "RS256",
                "kid": self.kid, "n": b64u(&self.n), "e": b64u(&self.e),
            }]
        })
    }

    pub fn signing_pem(&self) -> &str {
        &self.signing_pem
    }

    /// (modulus, exponent) 大端字节——本地验签用 from_rsa_components。
    pub fn n_e(&self) -> (Vec<u8>, Vec<u8>) {
        (self.n.clone(), self.e.clone())
    }

    pub fn kid(&self) -> &str {
        &self.kid
    }
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --release --lib oidc`
Expected: PASS（2 个用例）

- [ ] **Step 5: Commit**

```bash
git add src/bridge/oidc.rs src/bridge/mod.rs
git commit -m "feat(bridge): OidcState（PKCS#8 装载/kid/JWKS/rp+clients 配置态），StableState 构造期注入

unix@vip.qq.com ai"
```

### Task 3: ops + `oidc` 全局 + `json.raw`

**Files:**
- Modify: `src/bridge/oidc.rs`（追加三个 op）
- Modify: `src/bridge/json.rs`（追加 `op_json_raw`）、`src/bridge/envelope.rs`（raw 通路，如需）
- Modify: `src/bridge/mod.rs`（ops 列表追加 4 个 op）
- Modify: `src/bridge/bootstrap.js`（挂 `oidc` 全局 + `json.raw`）

**Interfaces:**
- Consumes: Task 2 的 `OidcState`。
- Produces（JS 全局面，Task 5-9 消费）：
  `oidc.sign(claims) → string`；`oidc.verify(token, jwks?) → object`；
  `oidc.jwks() → object`；`oidc.issuer / oidc.rp / oidc.clients`（getter）；
  `json.raw(data)` → 裸 JSON 200（无信封），OP 对外端点用。

**Spec 修订说明（重要，进 commit message 与终审）：** spec §7 写了「全部信封化」，但 OP 对外
端点（discovery/jwks/token/userinfo）必须说标准 OIDC 裸 JSON，否则外部标准 RP 无法消费
（token 响应的 `access_token` 是 RFC 6749 要求的顶层字段）。修订为：**成功载荷裸 JSON
（`json.raw`），错误仍走 oj 信封**（自研 RP 只判 `!res.ok`）。此为 spec 偏差点，Gate 1 审查时向需求方复述。

- [ ] **Step 1: 写失败测试**（`src/bridge/oidc.rs` tests 追加；用 Task 2 的 `write_key`/`section` 夹具）

```rust
    async fn bridge_with_oidc() -> (crate::bridge::Bridge, OidcState) {
        let dir = std::env::temp_dir().join(format!("oj-oidc-op-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let key_path = write_key(&dir);
        let st = OidcState::from_section(&section(&dir, &key_path, "http://h/v1/api/idp"), &dir)
            .unwrap();
        let b = crate::bridge::Bridge::with_dbs_and_loader(
            std::collections::HashMap::new(),
            std::sync::Arc::new(crate::bridge::InMemoryKV::new()),
            crate::bridge::SchemaRegistry::new(),
            false,
            None,
            crate::bridge::Extras {
                oidc: Some(std::sync::Arc::new(clone_state(&st))),
                ..Default::default()
            },
        );
        (b, st)
    }

    // OidcState 不 Clone（含私钥）；测试用两个独立实例（同 key 文件重读）。
    fn clone_state(st: &OidcState) -> OidcState { unimplemented!() }
```

——`clone_state` 是坏味道；改为直接二次 `from_section` 构造独立实例，测试夹具改成：

```rust
    fn make_state(dir_tag: &str) -> OidcState {
        let dir = std::env::temp_dir().join(format!("oj-oidc-{dir_tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let key_path = write_key(&dir);
        OidcState::from_section(&section(&dir, &key_path, "http://h/v1/api/idp"), &dir).unwrap()
    }

    async fn run_with_oidc(js: &str) -> serde_json::Value {
        let st = make_state("op");
        let b = crate::bridge::Bridge::with_dbs_and_loader(
            std::collections::HashMap::new(),
            std::sync::Arc::new(crate::bridge::InMemoryKV::new()),
            crate::bridge::SchemaRegistry::new(),
            false,
            None,
            crate::bridge::Extras { oidc: Some(std::sync::Arc::new(st)), ..Default::default() },
        );
        let cap = b.run_with(js, crate::bridge::RequestInfo::default()).await.unwrap();
        serde_json::from_slice(&cap.body).unwrap()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sign_then_verify_roundtrip_local_and_jwks() {
        let v = run_with_oidc(
            r#"(async () => {
              const now = Math.floor(Date.now() / 1000);
              const claims = { iss: oidc.issuer, sub: "u1", aud: "sample-rp", iat: now, exp: now + 3600, nonce: "n1", tenant: "default" };
              const tok = oidc.sign(claims);
              const local = oidc.verify(tok);
              const remote = oidc.verify(tok, oidc.jwks());
              json.ok({ local, remote, kid: oidc.jwks().keys[0].kid });
            })().catch((e) => json.fail(500, String(e)));"#,
        )
        .await;
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(v["data"]["local"]["sub"], "u1");
        assert_eq!(v["data"]["remote"]["tenant"], "default");
        assert_eq!(v["data"]["kid"].as_str().unwrap().len(), 16);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn verify_rejects_tampered_expired_and_wrong_alg() {
        let v = run_with_oidc(
            r#"(async () => {
              const now = Math.floor(Date.now() / 1000);
              const good = oidc.sign({ iss: oidc.issuer, sub: "u1", aud: "a", iat: now, exp: now + 3600 });
              // 篡改 payload（换最后一个可解码段）
              const parts = good.split(".");
              const payload = JSON.parse(atobUrl(parts[1]));
              payload.sub = "evil";
              const b64u = (s) => { const B = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"; let bin = ""; for (const c of s) bin += String.fromCharCode(c.charCodeAt(0)); const bytes = new Uint8Array(bin.length); for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i); let out = ""; for (let i = 0; i < bytes.length; i += 3) { const n = (bytes[i] << 16) | ((bytes[i+1] ?? 0) << 8) | (bytes[i+2] ?? 0); out += B[(n >> 18) & 63] + B[(n >> 12) & 63] + ((bytes[i+1] !== undefined) ? B[(n >> 6) & 63] : "") + ((bytes[i+2] !== undefined) ? B[n & 63] : ""); } return out; };
              function atobUrl(s) { const B = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"; let out = ""; let bits = 0, acc = 0; for (const c of s) { const v = B.indexOf(c); if (v < 0) continue; acc = (acc << 6) | v; bits += 6; if (bits >= 8) { bits -= 8; out += String.fromCharCode((acc >> bits) & 0xff); } } return out; }
              const tampered = parts[0] + "." + b64u(JSON.stringify(payload)) + "." + parts[2];
              const expired = oidc.sign({ iss: oidc.issuer, sub: "u1", aud: "a", iat: now - 7200, exp: now - 3600 });
              const results = {};
              try { oidc.verify(tampered); results.tampered = "ok"; } catch (e) { results.tampered = "err"; }
              try { oidc.verify(expired); results.expired = "ok"; } catch (e) { results.expired = "err"; }
              try { oidc.verify("eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJ4In0.aaaa"); results.wrong_alg = "ok"; } catch (e) { results.wrong_alg = "err"; }
              json.ok(results);
            })().catch((e) => json.fail(500, String(e)));"#,
        )
        .await;
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(v["data"]["tampered"], "err", "{v}");
        assert_eq!(v["data"]["expired"], "err", "{v}");
        assert_eq!(v["data"]["wrong_alg"], "err", "{v}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn verify_without_config_errors() {
        let b = crate::bridge::Bridge::new(
            std::sync::Arc::new(crate::bridge::InMemoryAccessor::new()),
            std::sync::Arc::new(crate::bridge::InMemoryKV::new()),
        );
        let cap = b
            .run_with(
                r#"(async () => { try { oidc.sign({}); json.ok("no"); } catch (e) { json.ok(String(e)); } })()"#,
                crate::bridge::RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(v["data"].as_str().unwrap().contains("oidc not configured"), "{v}");
    }
```

`json.raw` 测试（`src/bridge/json.rs` tests 或 oidc.rs 同款 Bridge 用例）：

```rust
#[tokio::test(flavor = "current_thread")]
async fn json_raw_writes_bare_body_with_200() {
    let b = crate::bridge::Bridge::new(
        std::sync::Arc::new(crate::bridge::InMemoryAccessor::new()),
        std::sync::Arc::new(crate::bridge::InMemoryKV::new()),
    );
    let cap = b
        .run_with(
            r#"json.raw({ issuer: "x", bare: true });"#,
            crate::bridge::RequestInfo::default(),
        )
        .await
        .unwrap();
    assert_eq!(cap.status, 200);
    let v: serde_json::Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["bare"], true);
    assert!(v.get("code").is_none());
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --release --lib 'oidc::tests' json_raw`
Expected: FAIL（op 未注册 / `json.raw` 未定义）

- [ ] **Step 3: 最小实现**

`src/bridge/oidc.rs` 追加 ops：

```rust
use deno_error::JsErrorBox;

fn oidc_of(
    state: &deno_core::OpState,
) -> Result<std::sync::Arc<OidcState>, JsErrorBox> {
    state
        .borrow::<std::sync::Arc<crate::bridge::StableState>>()
        .oidc
        .clone()
        .ok_or_else(|| JsErrorBox::generic("oidc not configured (config oidc: section missing)"))
}

fn verify_with(
    token: &str,
    key: &jsonwebtoken::DecodingKey,
) -> Result<serde_json::Value, String> {
    let mut v = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256);
    v.leeway = 0;
    v.validate_exp = true;
    v.validate_aud = false; // aud 由调用方（RP handler）按 client_id 自查
    jsonwebtoken::decode::<serde_json::Value>(token, key, &v)
        .map(|d| d.claims)
        .map_err(|err| format!("oidc.verify: {err}"))
}

/// oidc.sign(claims)：RS256 紧凑 JWS，claims 原样签（OP 自控 iss/aud/exp/nonce）。
#[deno_core::op2]
#[string]
pub fn op_oidc_sign(
    state: deno_core::OpState,
    #[serde] claims: serde_json::Value,
) -> Result<String, JsErrorBox> {
    let st = oidc_of(&state)?;
    if !claims.is_object() {
        return Err(JsErrorBox::generic("oidc.sign: claims must be an object"));
    }
    jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256),
        &claims,
        &jsonwebtoken::EncodingKey::from_rsa_pem(st.signing_pem().as_bytes())
            .map_err(|e| JsErrorBox::generic(format!("oidc.sign: {e}")))?,
    )
    .map_err(|e| JsErrorBox::generic(format!("oidc.sign: {e}")))
}

/// oidc.verify(token, jwks?)：无 jwks 用本机公钥；有 jwks 按 kid 匹配（from_jwk）。
/// 算法锁定 RS256（Validation::new 即白名单），leeway 0，验 exp。
#[deno_core::op2]
#[serde]
pub fn op_oidc_verify(
    state: deno_core::OpState,
    #[string] token: String,
    #[serde] jwks: Option<serde_json::Value>,
) -> Result<serde_json::Value, JsErrorBox> {
    let st = oidc_of(&state)?;
    match jwks {
        None => {
            let (n, e) = st.n_e();
            let key = jsonwebtoken::DecodingKey::from_rsa_components(&n, &e);
            verify_with(&token, &key).map_err(JsErrorBox::generic)
        }
        Some(doc) => {
            let header = jsonwebtoken::decode_header(&token)
                .map_err(|e| JsErrorBox::generic(format!("oidc.verify: {e}")))?;
            let kid = header.kid.ok_or_else(|| {
                JsErrorBox::generic("oidc.verify: token header has no kid")
            })?;
            let entry = doc["keys"]
                .as_array()
                .and_then(|keys| {
                    keys.iter().find(|k| {
                        k["kid"] == kid.as_str()
                            && k["kty"] == "RSA"
                            && (k["alg"] == "RS256" || k["alg"].is_null())
                    })
                })
                .ok_or_else(|| {
                    JsErrorBox::generic(format!("oidc.verify: no RS256 key for kid {kid}"))
                })?;
            let jwk: jsonwebtoken::jwk::Jwk = serde_json::from_value(entry.clone())
                .map_err(|e| JsErrorBox::generic(format!("oidc.verify: jwk parse: {e}")))?;
            let key = jsonwebtoken::DecodingKey::from_jwk(&jwk)
                .map_err(|e| JsErrorBox::generic(format!("oidc.verify: {e}")))?;
            verify_with(&token, &key).map_err(JsErrorBox::generic)
        }
    }
}

/// oidc 信息（issuer/jwks/rp/clients 一次取齐；bootstrap 侧拆成 getter）。
#[deno_core::op2]
#[serde]
pub fn op_oidc_info(state: deno_core::OpState) -> Result<serde_json::Value, JsErrorBox> {
    let st = oidc_of(&state)?;
    Ok(serde_json::json!({
        "issuer": st.issuer,
        "jwks": st.jwks(),
        "rp": st.rp.iter().map(|(k, v)| (k.clone(), serde_json::json!({
            "issuer": v.issuer, "client_id": v.client_id,
            "client_secret": v.client_secret, "scope": v.scope,
        }))).collect::<serde_json::Map<String, serde_json::Value>>(),
        "clients": st.clients.iter().map(|(k, v)| (k.clone(), serde_json::json!({
            "secret": v.secret, "redirect_uris": v.redirect_uris, "tenant": v.tenant,
        }))).collect::<serde_json::Map<String, serde_json::Value>>(),
    }))
}
```

`src/bridge/json.rs` 追加（读该文件对齐既有 `op_json_ok` 写 ReqState 的方式，raw 即「response
直接放序列化字节、status 200、done=true」）：

```rust
/// json.raw(data)：裸 JSON 200（无信封）。OP 对外端点说标准 OIDC JSON 用。
#[deno_core::op2]
pub fn op_json_raw(
    state: Rc<RefCell<OpState>>,
    #[serde] data: serde_json::Value,
) -> Result<(), JsErrorBox> {
    let mut st = state.borrow_mut();
    let req = st.borrow_mut::<ReqState>();
    req.response = Some(data.to_string().into_bytes());
    req.status = 200;
    req.done = true;
    Ok(())
}
```

`src/bridge/mod.rs` ops 列表追加：`oidc::op_oidc_sign, oidc::op_oidc_verify, oidc::op_oidc_info, json::op_json_raw`。

`bootstrap.js` 追加（ASCII，英文注释）：

```js
// ----- oidc: RS256 sign/verify primitives + assembly-time config (keys stay in Rust) -----
globalThis.oidc = (() => {
  const info = () => op_oidc_info();
  return {
    sign: (claims) => op_oidc_sign(claims === undefined ? null : claims),
    verify: (token, jwks) => op_oidc_verify(String(token), jwks === undefined ? null : jwks),
    jwks: () => info().jwks,
    get issuer() { return info().issuer; },
    get rp() { return info().rp; },
    get clients() { return info().clients; },
  };
})();
```

`json` 对象加一行：`raw: (data) => op_json_raw(data === undefined ? null : data),`，
import 区加 `op_json_raw, op_oidc_info, op_oidc_sign, op_oidc_verify,`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --release --lib 'oidc::tests' json_raw`
Expected: PASS（4 个用例）

- [ ] **Step 5: Commit**

```bash
git add src/bridge/oidc.rs src/bridge/json.rs src/bridge/mod.rs src/bridge/bootstrap.js
git commit -m "feat(bridge): oidc 全局（sign/verify/jwks/issuer/rp/clients）+ json.raw 裸 JSON 通路

spec 偏差点：OP 对外端点成功载荷必须裸 JSON（RFC 6749 顶层 access_token），错误仍走信封。

unix@vip.qq.com ai"
```

### Phase 1 Gate（集中审查 1）

- [ ] `cargo fmt --check && cargo clippy --all-targets -D warnings && cargo test --release --workspace -- --skip infinite_loop` 全绿
- [ ] 审查 Phase 1 diff：密钥是否只留在 Rust；`json.raw` 是否只被 OP 对外端点使用；spec 偏差（裸 JSON）已向需求方复述确认

---

## Phase 2 — server：`tenant.anonymous_paths` 豁免

### Task 4: Pipeline.tenant_anon + run 闭包豁免分支

**Files:**
- Modify: `server/src/lib.rs`（`Pipeline` 66-86 行；租户检查 333-345 行；tests 752 行 `tenant_header_injected_or_400` 附近）
- Modify: `oj/src/app.rs`（Pipeline 构造 434-436 行）

**Interfaces:**
- Consumes: Task 1 的 `TenantCfg.anonymous_paths`。
- Produces: `server::Pipeline { tenant_anon: Vec<String>, .. }`；`pub fn path_matches(list: &[String], path: &str) -> bool`。Task 8-10（豁免路径上的 OP/RP 端点）间接消费。

- [ ] **Step 1: 写失败测试**（`server/src/lib.rs` tests，镜像既有租户用例的夹具风格）

```rust
/// tenant.anonymous_paths：命中豁免路径的跳转腿免租户头；未命中仍 400。
#[tokio::test]
async fn tenant_anonymous_paths_skip_header_requirement() {
    let t = routes(&[(
        "oidc/callback/api.ts",
        "export default { get() { json.ok({ t: http.tenantId === undefined ? null : http.tenantId }); } };",
    )]);
    let addr = spawn_pipeline(
        "/v1/api",
        t.0.clone(),
        true,
        None,
        Pipeline {
            tenant_header: Some("X-TENANT-ID".into()),
            tenant_anon: vec!["/oidc/*".into()],
            ..Default::default()
        },
    )
    .await;
    // 命中 "/oidc/*"（一层）：/oidc/callback 免头，tenantId 为 null。
    let exempt = raw_http(
        addr,
        "GET /v1/api/oidc/callback/ HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(exempt.starts_with("HTTP/1.1 200") && exempt.contains("\"t\":null"), "{exempt}");
    // 未命中路径仍强制租户头。
    let miss = raw_http(
        addr,
        "GET /v1/api/oidc/callback/?x=1 HTTP/1.1\r\nHost: t\r\nX-TENANT-ID: acme\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(miss.starts_with("HTTP/1.1 200") && miss.contains("\"t\":\"acme\""), "{miss}");
}

/// path_matches：精确、尾 "/*" 一层通配（不命中裸前缀、不命中两层）。
#[test]
fn path_matches_semantics() {
    let l = vec!["/oidc/*".to_string(), "/idp/.well-known/*".to_string()];
    assert!(crate::path_matches(&l, "/oidc/callback"));
    assert!(!crate::path_matches(&l, "/oidc"));
    assert!(!crate::path_matches(&l, "/oidc/a/b"));
    assert!(crate::path_matches(&l, "/idp/.well-known/openid-configuration"));
    assert!(crate::path_matches(&["/health".to_string()], "/health"));
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --release -p mdm-server tenant_anonymous`
Expected: FAIL（编译错误：无 `tenant_anon` 字段 / `path_matches`）

- [ ] **Step 3: 最小实现**

`server/src/lib.rs` `Pipeline` 加字段：

```rust
/// 跳转腿豁免（tenant.anonymous_paths；命中则免租户头——OIDC 302 带不了自定义头）。
pub tenant_anon: Vec<String>,
```

`impl Default` 加 `tenant_anon: Vec::new(),`。模块级自由函数（与 oj-auth `is_anonymous`
同语义；刻意不复用——插件只依赖 oj-plugin-ffi，见 SOLID 注）：

```rust
/// 精确匹配或尾 "/*" 一层前缀通配（与 oj-auth is_anonymous 同语义；插件不能依赖
/// server crate，两处各自持有这份 6 行纯函数，注释互指）。
pub fn path_matches(list: &[String], path: &str) -> bool {
    list.iter().any(|p| match p.strip_suffix("/*") {
        Some(prefix) => path.starts_with(prefix) && path.len() > prefix.len(),
        None => path == p,
    })
}
```

租户检查（333-345 行）改：

```rust
// 前置管线：租户提取（启用后缺失/空 → 400；anonymous_paths 命中的跳转腿豁免）。
let tenant_id = match st.pipeline.tenant_header.as_deref() {
    Some(key)
        if !path_matches(&st.pipeline.tenant_anon, path_no_base.as_deref().unwrap_or("")) =>
    {
        let Some(tid) = headers
            .get(key)
            .and_then(|v| v.to_str().ok())
            .filter(|s| !s.is_empty())
        else {
            return fail_response(400, &format!("missing tenant header: {key}"));
        };
        Some(tid.to_string())
    }
    _ => None,
};
```

`oj/src/app.rs:435` Pipeline 构造加：

```rust
tenant_anon: cfg.tenant.anonymous_paths.clone(),
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --release -p mdm-server tenant`
Expected: PASS（新旧租户用例全绿）

- [ ] **Step 5: Commit**

```bash
git add server/src/lib.rs oj/src/app.rs
git commit -m "feat(server): tenant.anonymous_paths 豁免（OIDC 浏览器跳转腿免租户头，与 auth 匿名同构）

unix@vip.qq.com ai"
```

### Phase 2 Gate（集中审查 2）

- [ ] 全门禁绿
- [ ] 审查 diff：豁免语义与 auth 匿名严格同构；`path_no_base` 为 None（base 外）时仍强制；无新增依赖

---

## Phase 3 — sample OP：`sample/src/idp/`

### Task 5: 配置接线 + manifest + 共享工具 + discovery + jwks + L1

**Files:**
- Create: `sample/src/idp/manifest.yaml`
- Create: `sample/src/auth/_shared/util.ts`（通用小工具：redirect/nowSecs/b64uFromHex/parseForm/parseCookies——auth 模块已是会话/安全工具宿主，idp/oidc 跨模块导入，构建顺序 auth 最先）
- Create: `sample/src/idp/.well-known/openid-configuration/api.ts`
- Create: `sample/src/idp/jwks.json/api.ts`
- Create: `sample/config/oidc_rs256.pem`（openssl 生成 PKCS#8 demo 私钥，与现有 sample 证书同性质入库）
- Modify: `sample/config.yaml`（oidc 段 + auth/tenant 两处 anonymous_paths）
- Test: `sample/tests/oidc.test.ts`（新建）

**Interfaces:**
- Consumes: `oidc` 全局（Task 3）、`json.raw`。
- Produces: `GET /v1/api/idp/.well-known/openid-configuration/`（裸 JSON discovery）、
  `GET /v1/api/idp/jwks.json/`（裸 JWKS）；`auth/_shared/util.ts` 导出
  `redirect(url): void`、`nowSecs(): number`、`b64uFromHex(hex): string`、
  `parseForm(body): Record<string,string>`、`parseCookies(header): Record<string,string>`（Task 6-9 消费）。

- [ ] **Step 1: 生成 demo 私钥 + config.yaml 接线**

```bash
openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out sample/config/oidc_rs256.pem
```

`sample/config.yaml` 追加（`auth:` 段后）：

```yaml
oidc:   # OIDC（spec 2026-09-05）：同进程 OP + RP。demo 私钥与示例证书同性质，勿用于生产。
  issuer: "http://localhost:9778/v1/api/idp"
  private_key_path: "./config/oidc_rs256.pem"
  rp:   # RP：tenant → IdP。default 自举本机 OP；acme 留外部 IdP 样例（注释）。
    default:
      issuer: "http://localhost:9778/v1/api/idp"
      client_id: "sample-rp"
      client_secret: "rp-secret"
      scope: "openid profile"
    # acme:
    #   issuer: "https://idp.acme.example"
    #   client_id: "..."
    #   client_secret: "..."
  clients:   # OP 侧 client 白名单（redirect_uri 精确串）
    sample-rp:
      secret: "rp-secret"
      redirect_uris:
        - "http://localhost:9778/v1/api/oidc/callback"
      tenant: "default"
```

`auth.anonymous_paths` 与 `tenant:` 段各加：

```yaml
    - "/oidc/*"
    - "/idp/*"
    - "/idp/.well-known/*"
```

（`tenant:` 段加 `anonymous_paths:` 键；`auth.anonymous_paths` 列表追加三项。）

- [ ] **Step 2: 写失败测试**（`sample/tests/oidc.test.ts`）

```ts
function headerOf(r: { headers: Record<string, string> }, name: string): string {
  const k = Object.keys(r.headers).find((h) => h.toLowerCase() === name.toLowerCase());
  return k === undefined ? "" : String(r.headers[k]);
}

describe("idp discovery/jwks", () => {
  it("exposes bare discovery document with RS256 + code", async () => {
    const r = await client.get("/idp/.well-known/openid-configuration");
    expect(r.status).toBe(200);
    const body = JSON.parse(r.body);
    expect(body.issuer).toContain("/v1/api/idp");
    expect(body.response_types_supported[0]).toBe("code");
    expect(body.id_token_signing_alg_values_supported[0]).toBe("RS256");
    expect(body.jwks_uri).toContain("/idp/jwks.json");
  });

  it("exposes bare RSA JWKS with 16-char kid", async () => {
    const r = await client.get("/idp/jwks.json");
    expect(r.status).toBe(200);
    const body = JSON.parse(r.body);
    expect(body.keys[0].kty).toBe("RSA");
    expect(body.keys[0].alg).toBe("RS256");
    expect(body.keys[0].kid.length).toBe(16);
  });
});
```

Run: `cargo run -p oj -- test -c sample/config.yaml -d sample/src -t tests`
Expected: FAIL（路由 404——模块不存在）

- [ ] **Step 3: 最小实现**

`sample/src/idp/manifest.yaml`：

```yaml
name: "idp"
desc: "内置 OP（OIDC Provider）：discovery/jwks/authorize/login/token/userinfo，RS256（oidc 全局），标准对外裸 JSON"
version: "0.1.0"
```

`sample/src/auth/_shared/util.ts`：

```ts
// 通用小工具（302/时间/base64url/form/cookie）。idp 与 oidc 模块跨模块导入本文件，
// 构建顺序：auth 最先（oj build auth → idp/oidc）。

export function nowSecs(): number {
  return Math.floor(Date.now() / 1000);
}

// 302 跳转腿：Location 头 + fail(302)（HTTP 状态 = code 的既有映射，响应体为信封 JSON，
// 浏览器只认 302 + Location）。
export function redirect(url: string): void {
  json.header("Location", url);
  json.fail(302, "redirect");
}

// hex → base64url（无 padding）。纯字符表实现：运行时无 btoa/Buffer。
const B64U = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
export function b64uFromHex(hex: string): string {
  let out = "";
  for (let i = 0; i + 2 <= hex.length; i += 6) {
    const chunk = hex.slice(i, i + 6);
    const n = parseInt(chunk, 16);
    const bits = chunk.length * 4; // 24（满 3 字节）或 16（尾 2 字节）
    for (let j = 0; j < bits; j += 6) out += B64U[(n >>> (bits - j - 6)) & 0x3f];
  }
  return out;
}

// x-www-form-urlencoded → record（+ 为空格、%xx 解码）。
export function parseForm(body: unknown): Record<string, string> {
  const out: Record<string, string> = {};
  if (typeof body !== "string") return out;
  for (const pair of body.split("&")) {
    if (!pair) continue;
    const i = pair.indexOf("=");
    const k = i < 0 ? pair : pair.slice(0, i);
    const v = i < 0 ? "" : pair.slice(i + 1);
    out[decodeURIComponent(k.replace(/\+/g, " "))] =
      decodeURIComponent(v.replace(/\+/g, " "));
  }
  return out;
}

// Cookie 头 → record。
export function parseCookies(header: string): Record<string, string> {
  const out: Record<string, string> = {};
  for (const part of String(header || "").split(";")) {
    const i = part.indexOf("=");
    if (i > 0) out[part.slice(0, i).trim()] = part.slice(i + 1).trim();
  }
  return out;
}
```

`sample/src/idp/.well-known/openid-configuration/api.ts`：

```ts
export default {
  get() {
    const issuer = oidc.issuer;
    json.raw({
      issuer,
      authorization_endpoint: `${issuer}/authorize`,
      token_endpoint: `${issuer}/token`,
      userinfo_endpoint: `${issuer}/userinfo`,
      jwks_uri: `${issuer}/jwks.json`,
      response_types_supported: ["code"],
      grant_types_supported: ["authorization_code"],
      code_challenge_methods_supported: ["S256"],
      id_token_signing_alg_values_supported: ["RS256"],
      subject_types_supported: ["public"],
      token_endpoint_auth_methods_supported: ["client_secret_post"],
    });
  },
};
```

`sample/src/idp/jwks.json/api.ts`：

```ts
export default {
  get() {
    json.raw(oidc.jwks());
  },
};
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo run -p oj -- test -c sample/config.yaml -d sample/src -t tests`
Expected: PASS（oidc.test.ts 2 个用例全绿；其余既有测试不回归）

- [ ] **Step 5: Commit**

```bash
git add sample/src/idp sample/src/auth/_shared/util.ts sample/config.yaml sample/config/oidc_rs256.pem sample/tests/oidc.test.ts
git commit -m "feat(sample/idp): OP 骨架——discovery + jwks（裸 JSON）+ 通用工具 + config 接线

unix@vip.qq.com ai"
```

### Task 6: OP 会话——`idp/login` + `idp/authorize`

**Files:**
- Create: `sample/src/idp/login/api.ts`
- Create: `sample/src/idp/authorize/api.ts`
- Test: `sample/tests/oidc.test.ts`（追加）

**Interfaces:**
- Consumes: `parseCookies`/`nowSecs`（Task 5）、KV（`OJ-IDP:SESS:*`、`OJ-OIDC:CODE:*`）、users 表。
- Produces: `POST /v1/api/idp/login/`（信封 `{uid}` + `Set-Cookie: IDP_SESSION=...`）；
  `GET /v1/api/idp/authorize/`（302 或 400/401 信封）。Task 7/10 消费。

- [ ] **Step 1: 写失败测试**（追加到 `sample/tests/oidc.test.ts`）

```ts
async function idpCookie(): Promise<string> {
  const r = await client.post("/idp/login", {
    body: { username: "demo", password: "demo1234" },
  });
  expect(r.status).toBe(200);
  const setCookie = headerOf(r, "set-cookie");
  expect(setCookie).toContain("IDP_SESSION=");
  return setCookie.split(";")[0];
}

describe("idp login/authorize", () => {
  it("login rejects bad credentials without leaking existence", async () => {
    const r = await client.post("/idp/login", {
      body: { username: "demo", password: "wrong" },
    });
    expect(r.status).toBe(401);
    expect(JSON.parse(r.body).msg).toBe("invalid credentials");
  });

  it("authorize enforces whitelist, PKCE and session; issues one-time code", async () => {
    const noSess = await client.get("/idp/authorize?response_type=code&client_id=sample-rp&scope=openid");
    expect(noSess.status).toBe(401);
    expect(JSON.parse(noSess.body).msg).toBe("login required");
    const cookie = await idpCookie();
    const base =
      "/idp/authorize?response_type=code&client_id=sample-rp" +
      "&redirect_uri=" +
      encodeURIComponent("http://localhost:9778/v1/api/oidc/callback") +
      "&scope=openid&state=st1&nonce=n1&code_challenge=" +
      "x".repeat(43) +
      "&code_challenge_method=S256";
    const badClient = await client.get(base.replace("sample-rp", "nope"), { headers: { Cookie: cookie } });
    expect(badClient.status).toBe(400);
    const badUri = await client.get(
      base.replace(encodeURIComponent("http://localhost:9778/v1/api/oidc/callback"), encodeURIComponent("http://evil/cb")),
      { headers: { Cookie: cookie } },
    );
    expect(badUri.status).toBe(400);
    const noPkce = await client.get(base.split("&code_challenge")[0], { headers: { Cookie: cookie } });
    expect(noPkce.status).toBe(400);
    const ok = await client.get(base, { headers: { Cookie: cookie } });
    expect(ok.status).toBe(302);
    const loc = headerOf(ok, "location");
    expect(loc).toContain("code=");
    expect(loc).toContain("state=st1");
  });
});
```

Run: `cargo run -p oj -- test -c sample/config.yaml -d sample/src -t tests`
Expected: FAIL（404——端点不存在）

- [ ] **Step 2: 最小实现**

`sample/src/idp/login/api.ts`：

```ts
import { nowSecs } from "../../auth/_shared/util";

export default {
  async post() {
    const b = http.body || {};
    const rows = await db.query(
      "select id, password_hash from users where username = ?",
      [String(b.username ?? "")],
    );
    const row = rows[0];
    // 用户不存在与密码错同报（不泄露用户存在性，对齐 auth/login）。
    if (!row || !(await bcrypt.verify(String(b.password ?? ""), <string>row.password_hash || ""))) {
      json.fail(401, "invalid credentials");
      return;
    }
    const sid = crypto.randomHex(32);
    const ttl = jwt.refreshDuration;
    await kv.set(
      "OJ-IDP:SESS:" + sid,
      JSON.stringify({ uid: String(row.id), exp: nowSecs() + ttl }),
    );
    await kv.expire("OJ-IDP:SESS:" + sid, ttl);
    // Cookie Path 取 issuer 的路径段（登录会话只在 OP 端点内可见）。
    const path = oidc.issuer.replace(/^https?:\/\/[^/]+/, "");
    json.header("Set-Cookie", `IDP_SESSION=${sid}; HttpOnly; Path=${path}; Max-Age=${ttl}`);
    json.ok({ uid: String(row.id) });
  },
};
```

`sample/src/idp/authorize/api.ts`：

```ts
import { nowSecs, parseCookies, redirect } from "../../auth/_shared/util";

const CODE_TTL = 60;

export default {
  async get() {
    const q = http.query;
    const clientCfg = (oidc.clients || {})[String(q.client_id ?? "")];
    // 白名单不命中：绝不重定向（redirect_uri 未验证前跳转 = 开放重定向）。
    if (!clientCfg) {
      json.fail(400, "unknown client");
      return;
    }
    if (!(clientCfg.redirect_uris || []).includes(String(q.redirect_uri ?? ""))) {
      json.fail(400, "redirect_uri not registered");
      return;
    }
    if (String(q.response_type ?? "") !== "code") {
      json.fail(400, "response_type must be code");
      return;
    }
    if (!String(q.scope ?? "").split(" ").includes("openid")) {
      json.fail(400, "scope must include openid");
      return;
    }
    const challenge = String(q.code_challenge ?? "");
    if (String(q.code_challenge_method ?? "") !== "S256" || challenge.length < 43) {
      json.fail(400, "PKCE S256 required");
      return;
    }
    const cookies = parseCookies(String(http.headers.cookie ?? ""));
    const sessRaw = await kv.get("OJ-IDP:SESS:" + (cookies.IDP_SESSION ?? ""));
    const sess = sessRaw ? JSON.parse(sessRaw) : null;
    if (!sess || !(sess.exp > nowSecs())) {
      json.fail(401, "login required");
      return;
    }
    const code = crypto.randomHex(32);
    await kv.set(
      "OJ-OIDC:CODE:" + code,
      JSON.stringify({
        client_id: q.client_id,
        redirect_uri: q.redirect_uri,
        state: q.state ?? "",
        nonce: q.nonce ?? "",
        challenge,
        uid: sess.uid,
        tenant: clientCfg.tenant,
        exp: nowSecs() + CODE_TTL,
      }),
    );
    await kv.expire("OJ-OIDC:CODE:" + code, CODE_TTL);
    redirect(
      `${q.redirect_uri}?code=${code}&state=${encodeURIComponent(String(q.state ?? ""))}`,
    );
  },
};
```

- [ ] **Step 3: 跑测试确认通过**

Run: `cargo run -p oj -- test -c sample/config.yaml -d sample/src -t tests`
Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add sample/src/idp/login sample/src/idp/authorize sample/tests/oidc.test.ts
git commit -m "feat(sample/idp): login（bcrypt+cookie 会话）+ authorize（白名单/PKCE/code 一次一用）

unix@vip.qq.com ai"
```

### Task 7: OP 出口——`idp/token` + `idp/userinfo`

**Files:**
- Create: `sample/src/idp/token/api.ts`
- Create: `sample/src/idp/userinfo/api.ts`
- Test: `sample/tests/oidc.test.ts`（追加全链用例）

**Interfaces:**
- Consumes: `oidc.sign`/`oidc.verify`（Task 3）、`b64uFromHex`/`parseForm`/`nowSecs`（Task 5）、code KV（Task 6）。
- Produces: `POST /v1/api/idp/token/`（裸 JSON：`access_token/id_token/token_type/expires_in`）；
  `GET /v1/api/idp/userinfo/`（裸 JSON `{sub, tenant}`）。Task 9（RP 交换）/Task 10（e2e）消费。

- [ ] **Step 1: 写失败测试**（追加——进程内完整 code flow）

```ts
import { b64uFromHex } from "../src/auth/_shared/util";

describe("idp token/userinfo (full code flow in-process)", () => {
  it("exchanges code for RS256 tokens; replay rejected; userinfo reads sub/tenant", async () => {
    const verifier = crypto.randomHex(32);
    const challenge = b64uFromHex(crypto.sha256Hex(verifier));
    const cookie = await idpCookie();
    const cb = encodeURIComponent("http://localhost:9778/v1/api/oidc/callback");
    const ar = await client.get(
      `/idp/authorize?response_type=code&client_id=sample-rp&redirect_uri=${cb}` +
        `&scope=openid&state=st2&nonce=n2&code_challenge=${challenge}&code_challenge_method=S256`,
      { headers: { Cookie: cookie } },
    );
    expect(ar.status).toBe(302);
    const loc = headerOf(ar, "location");
    const code = loc.split("code=")[1].split("&")[0];
    const tokenBody =
      `grant_type=authorization_code&code=${code}&client_id=sample-rp` +
      `&client_secret=rp-secret&redirect_uri=${cb}&code_verifier=${verifier}`;
    const tr = await client.post("/idp/token", {
      body: tokenBody,
      headers: { "Content-Type": "application/x-www-form-urlencoded" },
    });
    expect(tr.status).toBe(200);
    const tok = JSON.parse(tr.body); // 裸 JSON（json.raw）
    expect(tok.access_token).toBeTruthy();
    expect(tok.id_token).toBeTruthy();
    expect(tok.token_type).toBe("Bearer");
    // code 一次一用：重放 401。
    const replay = await client.post("/idp/token", {
      body: tokenBody,
      headers: { "Content-Type": "application/x-www-form-urlencoded" },
    });
    expect(replay.status).toBe(401);
    // PKCE 错 verifier → 401（拿新 code 试）。
    // userinfo：access_token 验签后读 sub/tenant。
    const ui = await client.get("/idp/userinfo", {
      headers: { Authorization: "Bearer " + tok.access_token },
    });
    expect(ui.status).toBe(200);
    const u = JSON.parse(ui.body);
    expect(String(u.sub)).toBeTruthy();
    expect(u.tenant).toBe("default");
    const bad = await client.get("/idp/userinfo", { headers: { Authorization: "Bearer junk" } });
    expect(bad.status).toBe(401);
  });
});
```

Run: `cargo run -p oj -- test -c sample/config.yaml -d sample/src -t tests`
Expected: FAIL（404）

- [ ] **Step 2: 最小实现**

`sample/src/idp/token/api.ts`：

```ts
import { b64uFromHex, nowSecs, parseForm } from "../../auth/_shared/util";

const TOKEN_TTL = 3600;

export default {
  async post() {
    const f = parseForm(http.body);
    if (f.grant_type !== "authorization_code") {
      json.fail(400, "unsupported grant_type");
      return;
    }
    const clientCfg = (oidc.clients || {})[String(f.client_id ?? "")];
    if (!clientCfg || clientCfg.secret !== String(f.client_secret ?? "")) {
      json.fail(401, "invalid client");
      return;
    }
    const key = "OJ-OIDC:CODE:" + String(f.code ?? "");
    // 一次一用：先 del 再判（并发/重放都落空）。
    const raw = await kv.get(key);
    if (raw !== null) await kv.del(key);
    const code = raw ? JSON.parse(raw) : null;
    if (!code || !(code.exp > nowSecs())) {
      json.fail(401, "invalid or expired code");
      return;
    }
    if (code.redirect_uri !== f.redirect_uri) {
      json.fail(400, "redirect_uri mismatch");
      return;
    }
    // PKCE：S256(verifier) == 挑战。
    if (b64uFromHex(crypto.sha256Hex(String(f.code_verifier ?? ""))) !== code.challenge) {
      json.fail(401, "pkce verification failed");
      return;
    }
    const now = nowSecs();
    const claims = {
      iss: oidc.issuer,
      sub: code.uid,
      aud: code.client_id,
      iat: now,
      exp: now + TOKEN_TTL,
      nonce: code.nonce,
      tenant: code.tenant,
    };
    json.raw({
      access_token: oidc.sign(claims),
      id_token: oidc.sign(claims),
      token_type: "Bearer",
      expires_in: TOKEN_TTL,
    });
  },
};
```

`sample/src/idp/userinfo/api.ts`：

```ts
export default {
  get() {
    const auth = String(http.headers.authorization ?? "");
    const token = auth.startsWith("Bearer ") ? auth.slice(7) : "";
    if (!token) {
      json.fail(401, "missing bearer token");
      return;
    }
    let claims: { sub?: unknown; tenant?: unknown };
    try {
      claims = oidc.verify(token);
    } catch {
      json.fail(401, "invalid token");
      return;
    }
    json.raw({ sub: claims.sub, tenant: claims.tenant ?? null });
  },
};
```

- [ ] **Step 3: 跑测试确认通过**

Run: `cargo run -p oj -- test -c sample/config.yaml -d sample/src -t tests`
Expected: PASS（含既有 auth/tenant 等全部 L1 无回归）

- [ ] **Step 4: Commit**

```bash
git add sample/src/idp/token sample/src/idp/userinfo sample/tests/oidc.test.ts
git commit -m "feat(sample/idp): token（code 换 RS256 双 token + PKCE 校验）+ userinfo（Bearer 裸 JSON）

unix@vip.qq.com ai"
```

### Phase 3 Gate（集中审查 3）

- [ ] 全门禁绿 + L1 全绿
- [ ] **VERIFY 实测（spec §7 的遗留验证点）**：

```bash
cargo run -p oj -- server -c sample/config.yaml --api-path sample/src &
sleep 3
curl -si 'http://localhost:9778/v1/api/idp/.well-known/openid-configuration' | head -3
# 期望：HTTP/1.1 200 + 裸 JSON（无信封）
curl -si 'http://localhost:9778/v1/api/idp/authorize?client_id=sample-rp' | head -5
# 期望：HTTP/1.1 400 信封（unknown client 或 redirect_uri not registered）
kill %1
```

若 302/Location 或裸 JSON 有异常，先修 server 通路再继续。
- [ ] 审查 diff：白名单未命中绝不 302；code/state/会话都一次一用 + TTL；错误信封不带内部细节

---

## Phase 4 — sample RP：`sample/src/oidc/`

### Task 8: RP manifest + `oidc/login`（state/nonce/PKCE + discovery 302）

**Files:**
- Create: `sample/src/oidc/manifest.yaml`
- Create: `sample/src/oidc/login/api.ts`
- Test: `sample/tests/oidc.test.ts`（追加）

**Interfaces:**
- Consumes: `oidc.rp`（Task 3）、`redirect`/`nowSecs`/`b64uFromHex`（Task 5）、KV `OJ-OIDC:STATE:*`。
- Produces: `GET /v1/api/oidc/login/?tenant=X`（302 IdP authorize URL 或 400/502 信封）。Task 9/10 消费（state KV 快照形状 `{tenant, nonce, verifier, issuer, client_id, client_secret, exp}`）。

- [ ] **Step 1: 写失败测试**

```ts
describe("oidc login (RP)", () => {
  it("rejects unknown tenant", async () => {
    const r = await client.get("/oidc/login?tenant=nope");
    expect(r.status).toBe(400);
    expect(JSON.parse(r.body).msg).toBe("unknown tenant");
  });

  it("redirects to discovered authorization endpoint with PKCE", async () => {
    const r = await client.get("/oidc/login?tenant=default");
    expect(r.status).toBe(302);
    const loc = headerOf(r, "location");
    expect(loc).toContain("/idp/authorize?");
    expect(loc).toContain("client_id=sample-rp");
    expect(loc).toContain("code_challenge_method=S256");
    expect(loc).toContain("state=");
  });
});
```

（进程内 fetch 自身 discovery 不可达——`oj test` 无 TCP listener——所以 302 用例在 Rust e2e
Task 10 才能真正走通；此处预期该用例在 L1 里 **fetch 失败走 502**。处理：本任务先只落
「unknown tenant 400」用例，302 用例移到 Task 10 的 e2e。）

修正后的 L1（本任务实际落两个断言中的第一个）：

```ts
describe("oidc login (RP)", () => {
  it("rejects unknown tenant", async () => {
    const r = await client.get("/oidc/login?tenant=nope");
    expect(r.status).toBe(400);
    expect(JSON.parse(r.body).msg).toBe("unknown tenant");
  });
});
```

- [ ] **Step 2: 最小实现**

`sample/src/oidc/manifest.yaml`：

```yaml
name: "oidc"
desc: "RP（OIDC Relying Party）：login/callback/logout，通用 discovery 对接任意标准 IdP，会话桥接复用 auth/_shared"
version: "0.1.0"
deps:
  _platform: "^0.1.0"
```

`sample/src/oidc/login/api.ts`：

```ts
import { b64uFromHex, nowSecs, redirect } from "../../auth/_shared/util";

const STATE_TTL = 600;

// 回跳地址：从请求 Host 头拼（RP 侧零配置；OP 白名单侧显式配置才安全）。
// ponytail: base 硬编码 /v1/api（demo 配置固定）；server.base 可配时这里要跟着改。
function redirectUri(): string {
  const host = String(http.headers.host ?? "localhost:9778");
  const proto = host.startsWith("localhost") || host.startsWith("127.") ? "http" : "https";
  return `${proto}://${host}/v1/api/oidc/callback`;
}

export default {
  async get() {
    const tenant = String(http.query.tenant ?? "");
    const rp = (oidc.rp || {})[tenant];
    if (!rp) {
      json.fail(400, "unknown tenant");
      return;
    }
    // 通用 discovery：endpoints 不硬编码（对接任意标准 IdP）。
    const res = await fetch(rp.issuer + "/.well-known/openid-configuration");
    const disc = res.ok ? await res.json() : null;
    if (!disc || !disc.authorization_endpoint) {
      json.fail(502, "discovery failed");
      return;
    }
    const state = crypto.randomHex();
    const verifier = crypto.randomHex(32);
    const nonce = crypto.randomHex(16);
    await kv.set(
      "OJ-OIDC:STATE:" + state,
      JSON.stringify({
        tenant,
        nonce,
        verifier,
        issuer: rp.issuer,
        client_id: rp.client_id,
        client_secret: rp.client_secret,
        redirect_uri: redirectUri(),
        exp: nowSecs() + STATE_TTL,
      }),
    );
    await kv.expire("OJ-OIDC:STATE:" + state, STATE_TTL);
    const q = [
      "response_type=code",
      "client_id=" + encodeURIComponent(rp.client_id),
      "redirect_uri=" + encodeURIComponent(redirectUri()),
      "scope=" + encodeURIComponent(rp.scope),
      "state=" + state,
      "nonce=" + nonce,
      "code_challenge=" + b64uFromHex(crypto.sha256Hex(verifier)),
      "code_challenge_method=S256",
    ].join("&");
    redirect(disc.authorization_endpoint + "?" + q);
  },
};
```

（快照带 `redirect_uri`——callback 交换时用快照值而非现拼，防 Host 头漂移。）

- [ ] **Step 3: 跑测试确认通过**

Run: `cargo run -p oj -- test -c sample/config.yaml -d sample/src -t tests`
Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add sample/src/oidc sample/tests/oidc.test.ts
git commit -m "feat(sample/oidc): RP login——tenant→IdP 路由、discovery、state/nonce/PKCE 快照进 KV

unix@vip.qq.com ai"
```

### Task 9: `oidc/callback`（交换 + 验签 + JIT + 会话桥接）+ `oidc/logout`

**Files:**
- Create: `sample/src/oidc/callback/api.ts`
- Create: `sample/src/oidc/logout/api.ts`
- Test: `sample/tests/oidc.test.ts`（追加——L1 只能测参数闸门；全链在 Task 10 e2e）

**Interfaces:**
- Consumes: Task 8 的 state 快照；`issueTokens`（`auth/_shared/session.ts`，签名 `issueTokens(uid: string, roles: string[])`）；`oidc.verify(token, jwks)`。
- Produces: `GET /v1/api/oidc/callback/?code&state`（信封：`access_token/refresh_token/expires_in/user`）；`POST /v1/api/oidc/logout/`（同 `auth/logout` 语义）。

- [ ] **Step 1: 写失败测试**

```ts
describe("oidc callback gates (L1: no self-fetch)", () => {
  it("rejects invalid state", async () => {
    const r = await client.get("/oidc/callback?code=x&state=bogus");
    expect(r.status).toBe(401);
    expect(JSON.parse(r.body).msg).toBe("invalid or expired state");
  });
  it("logout clears refresh session", async () => {
    const r = await client.post("/oidc/logout", { body: { refresh_token: "junk" } });
    expect(r.status).toBe(200);
  });
});
```

- [ ] **Step 2: 最小实现**

`sample/src/oidc/callback/api.ts`：

```ts
import { issueTokens } from "../../auth/_shared/session";
import { nowSecs } from "../../auth/_shared/util";

export default {
  async get() {
    const state = String(http.query.state ?? "");
    const key = "OJ-OIDC:STATE:" + state;
    // 一次一用：先 del 再判（CSRF/重放共用此闸）。
    const raw = await kv.get(key);
    if (raw !== null) await kv.del(key);
    const snap = raw ? JSON.parse(raw) : null;
    if (!snap || !(snap.exp > nowSecs())) {
      json.fail(401, "invalid or expired state");
      return;
    }
    const code = String(http.query.code ?? "");
    if (!code) {
      json.fail(400, "code required");
      return;
    }
    const disc = await fetch(snap.issuer + "/.well-known/openid-configuration").then((r) => r.json());
    const form = [
      "grant_type=authorization_code",
      "code=" + encodeURIComponent(code),
      "client_id=" + encodeURIComponent(snap.client_id),
      "client_secret=" + encodeURIComponent(snap.client_secret),
      "redirect_uri=" + encodeURIComponent(snap.redirect_uri),
      "code_verifier=" + snap.verifier,
    ].join("&");
    const tres = await fetch(disc.token_endpoint, {
      method: "POST",
      headers: { "Content-Type": "application/x-www-form-urlencoded" },
      body: form,
    });
    const tokens = tres.ok ? await tres.json() : null;
    if (!tokens || !tokens.id_token) {
      json.fail(502, "token endpoint failed");
      return;
    }
    const jres = await fetch(disc.jwks_uri);
    const jwks = jres.ok ? await jres.json() : null;
    let claims: Record<string, unknown>;
    try {
      claims = oidc.verify(String(tokens.id_token), jwks);
    } catch {
      json.fail(401, "id_token verification failed");
      return;
    }
    if (claims.nonce !== snap.nonce || claims.iss !== snap.issuer) {
      json.fail(401, "id_token claims mismatch");
      return;
    }
    const auds = Array.isArray(claims.aud) ? claims.aud : [claims.aud];
    if (!auds.includes(snap.client_id)) {
      json.fail(401, "id_token aud mismatch");
      return;
    }
    // JIT 本地映射：sub → users 行；无则建（'!oidc' 非法占位 hash 不可密码登录——
    // bcrypt.verify 对非法 hash 恒 false，零 schema 变更）。
    const sub = String(claims.sub);
    let rows = await db.query("select id, roles from users where username = ?", [sub]);
    if (!rows.length) {
      await db.exec(
        "insert into users (username, password_hash, roles) values (?, ?, '[]')",
        [sub, "!oidc"],
      );
      rows = await db.query("select id, roles from users where username = ?", [sub]);
    }
    let roles: string[] = [];
    try {
      roles = JSON.parse(<string>rows[0].roles || "[]");
    } catch {
      roles = [];
    }
    json.ok(await issueTokens(String(rows[0].id), roles));
  },
};
```

`sample/src/oidc/logout/api.ts`：

```ts
import { sessionKey } from "../../auth/_shared/session";

export default {
  async post() {
    const token = String((http.body || {}).refresh_token ?? "");
    await kv.del(sessionKey(token));
    json.ok(null);
  },
};
```

- [ ] **Step 3: 跑测试确认通过**

Run: `cargo run -p oj -- test -c sample/config.yaml -d sample/src -t tests`
Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add sample/src/oidc sample/tests/oidc.test.ts
git commit -m "feat(sample/oidc): callback（token 交换/id_token 验签/JIT 桥接 issueTokens）+ logout

unix@vip.qq.com ai"
```

### Phase 4 Gate（集中审查 4）

- [ ] 全门禁绿 + L1 全绿
- [ ] 审查 diff：callback 只信 state 快照（不信 query）；client_secret 只出现在对 IdP 的服务端请求；JIT 占位 hash 确实不可登录（可用 L1 快验：POST /auth/login username=外部 sub → 401）

---

## Phase 5 — Rust e2e 全链路 + 文档

### Task 10: `oj/tests/e2e.rs` OIDC 全链用例

**Files:**
- Modify: `oj/Cargo.toml`（dev-dependencies 加 `rsa = "0.9"`，与 workspace 同版）
- Modify: `oj/tests/e2e.rs`（新用例；夹具风格随文件内既有用例）

**Interfaces:**
- Consumes: Task 1-9 全部（真 server + 真 TCP + 真 fetch 回环）。
- Produces: 回归用例 `oidc_full_chain_login_bridge_and_tenant`。

- [ ] **Step 1: dev-dependency**

`oj/Cargo.toml` `[dev-dependencies]` 加：

```toml
# OIDC e2e：现场生成 RS256 测试密钥（与根 crate 同 0.9，依赖树单版本）。
rsa = "0.9"
```

- [ ] **Step 2: 写失败测试**（`oj/tests/e2e.rs` 追加；夹具目录/seed/config 构造随既有用例模式——tempdir + `manifest.yaml` + `seed.sql` + `cfg.server.port = 0` 改为**先探空闲端口**：issuer/redirect_uri 需要真实端口）

```rust
#[tokio::test]
async fn oidc_full_chain_login_bridge_and_tenant() {
    let lock = E2E_LOCK.lock().await;
    let t = tempfile::tempdir().unwrap();
    // 空闲端口预探（issuer/redirect_uri 是配置期字符串，不能用 port=0）。
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
    let self_base = format!("http://127.0.0.1:{port}/v1/api");
    // RS256 测试密钥（PKCS#8 PEM）。
    use rsa::pkcs8::EncodePrivateKey;
    let key = rsa::RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).unwrap();
    let pem = key.to_pkcs8_pem(rsa::pkcs8::LineEnding::LF).unwrap();
    let key_path = t.path().join("oidc_rs256.pem");
    std::fs::write(&key_path, pem.as_bytes()).unwrap();
    // 模块夹具：_platform（users 表 seed）+ idp + oidc。
    std::fs::create_dir_all(t.path().join("src/_platform")).unwrap();
    std::fs::write(
        t.path().join("src/_platform/manifest.yaml"),
        "name: \"_platform\"\ndesc: \"users\"\nversion: \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(
        t.path().join("src/_platform/seed.sql"),
        "CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY AUTOINCREMENT, \
         username TEXT NOT NULL UNIQUE, password_hash TEXT NOT NULL, roles TEXT NOT NULL DEFAULT '[]');\n\
         INSERT OR IGNORE INTO users (id, username, password_hash, roles) VALUES \
         (1, 'demo', '$2b$10$aKN7gpFP.dhK7Il8sc19neUPaziSONYdsfks1xm0H2COzkp2vlqV2', '[\"admin\"]');\n",
    )
    .unwrap();
    for (mod_name, dirs) in [
        ("idp", vec![".well-known/openid-configuration", "jwks.json", "authorize", "login", "token", "userinfo"]),
        ("oidc", vec!["login", "callback", "logout"]),
    ] {
        let root = t.path().join("src").join(mod_name);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("manifest.yaml"),
            format!("name: \"{mod_name}\"\ndesc: \"oidc\"\nversion: \"0.1.0\"\n"),
        )
        .unwrap();
        for d in dirs {
            let p = root.join(d);
            std::fs::create_dir_all(&p).unwrap();
            std::fs::write(p.join(".keep"), "").unwrap();
        }
    }
    // handler 源码直接拷 sample（测试即真实产物）。
    let sample = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../sample/src");
    copy_dir(&sample.join("idp"), &t.path().join("src/idp"));
    copy_dir(&sample.join("oidc"), &t.path().join("src/oidc"));
    copy_dir(&sample.join("auth"), &t.path().join("src/auth"));
    // config：cert + auth + tenant(豁免) + oidc + sqlite 内存库。
    let cert = gen_cert(t.path()); // 随既有 e2e 证书夹具做法（文件内已有 helper 则复用）
    let mut cfg = default_test_cfg(t.path(), &cert);
    cfg.server.port = port;
    cfg.auth = Some(serde_yaml::from_str(
        "jwt_secret: \"e2e\"\nanonymous_paths: [/oidc/*, /idp/*, /idp/.well-known/*]\n",
    )
    .unwrap());
    cfg.tenant.anonymous_paths = vec!["/oidc/*".into(), "/idp/*".into(), "/idp/.well-known/*".into()];
    cfg.oidc = Some(serde_yaml::from_str(&format!(
        "issuer: \"{self_base}/idp\"\n\
         private_key_path: \"{}\"\n\
         rp:\n  default:\n    issuer: {self_base}/idp\n    client_id: sample-rp\n    client_secret: rp-secret\n    scope: openid\n\
         clients:\n  sample-rp:\n    secret: rp-secret\n    redirect_uris: [{self_base}/oidc/callback]\n    tenant: default\n",
        key_path.display()
    ))
    .unwrap());
    let (addr, handle) = start(cfg, t.path().into(), t.path().join("src"), "/v1/api".into(), true)
        .await
        .unwrap();
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .unwrap();
    // 1) RP login → 302 IdP authorize。
    let r1 = http
        .get(format!("{addr}/v1/api/oidc/login?tenant=default"))
        .send()
        .await
        .unwrap();
    assert_eq!(r1.status(), 302);
    let authorize_url = r1.headers()["location"].to_str().unwrap().to_string();
    // 2) authorize 无会话 → 401 login required。
    let r2 = http.get(&authorize_url).send().await.unwrap();
    assert_eq!(r2.status(), 401);
    // 3) OP login → cookie。
    let r3 = http
        .post(format!("{addr}/v1/api/idp/login"))
        .json(&serde_json::json!({"username": "demo", "password": "demo1234"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r3.status(), 200);
    let cookie = r3.headers()["set-cookie"].to_str().unwrap();
    let sid = cookie.split(';').next().unwrap().to_string();
    // 4) authorize 带 cookie → 302 回 RP callback。
    let r4 = http.get(&authorize_url).header("cookie", &sid).send().await.unwrap();
    assert_eq!(r4.status(), 302);
    let cb_url = r4.headers()["location"].to_str().unwrap().to_string();
    // 5) callback（无租户头——豁免路径）→ 桥接会话信封。
    let r5 = http.get(&cb_url).send().await.unwrap();
    assert_eq!(r5.status(), 200, "{}", r5.text().await.unwrap());
    let body: serde_json::Value = r5.json().await.unwrap();
    assert_eq!(body["code"], 0, "{body}");
    let access = body["data"]["access_token"].as_str().unwrap().to_string();
    // 6) 桥接会话打受保护路由（租户头恢复强制）。
    let r6 = http
        .get(format!("{addr}/v1/api/auth_demo/me/"))
        .bearer_auth(&access)
        .header("X-TENANT-ID", "A1E9BFE9-391B-4F03-5DF5-D0AB6B54F5F8")
        .send()
        .await
        .unwrap();
    assert_eq!(r6.status(), 200, "{}", r6.text().await.unwrap());
    // 7) state 重放被拒。
    let r7 = http.get(&cb_url).send().await.unwrap();
    assert_eq!(r7.status(), 401);
    handle.abort();
    drop(lock);
}
```

执行说明（给实现者的两个自由度）：`gen_cert` / `default_test_cfg` / `copy_dir` 若文件内已有
等价 helper 就复用；`auth_demo` 不在夹具里就换成任一读 `http.user` 的受保护 handler
（在夹具里加一个 `me/api.ts`：`export default { get() { json.ok({ u: http.user }); } };`，
断言 `r6` 含 `"id":"1"`）。auth_demo 依赖更少，优先直接夹具内加 `me`。

- [ ] **Step 3: 跑测试确认失败 → 实现（如有）→ 确认通过**

Run: `cargo test --release -p oj --test e2e oidc_full_chain -- --nocapture`
Expected: 首跑可能因夹具细节（端口占用/拷贝路径）失败——这是集成用例，失败即修夹具；
逻辑缺陷回对应 Task 修。终态 PASS。

- [ ] **Step 4: Commit**

```bash
git add oj/Cargo.toml oj/tests/e2e.rs
git commit -m "test(e2e): OIDC 全链路（RP login→OP authorize/login→callback 交换→会话桥接→state 重放拒绝）

unix@vip.qq.com ai"
```

### Task 11: 文档增补

**Files:**
- Modify: `docs/devkit/api-manual.md`（§6 总表加 `oidc` 行；§7 信封表加 `json.raw` 说明；§10 config 加 `oidc:` 段）
- Modify: `docs/user-manual.md`（config 参考 oidc 段 + 演示一节）
- Modify: `docs/dev-guide.md`（§4 全局速查表加 `oidc`/`json.raw` 行；§11.1 加 tenant.anonymous_paths 一行）
- Modify: `sample/README.md`（OIDC 演示步骤）

内容要点（各文档 5-15 行，不展开成教程）：
- `oidc` 全局表行：`oidc.sign/verify/jwks/issuer/rp/clients`（`oidc:` 段启用；密钥在 Rust）。
- `json.raw(data)`：裸 JSON 200（无信封）——对外标准协议端点用。
- `tenant.anonymous_paths`：与 auth.anonymous_paths 同构的跳转腿豁免。
- sample 演示（`sample/README.md`）：起服务 → 三段 curl（login 拿 cookie、authorize 拿
  code、callback 换会话）→ 带 Bearer + 租户头访问受保护路由。

- [ ] **Step 1: 按上述要点改四个文档**
- [ ] **Step 2: 全门禁 + Commit**

```bash
cargo fmt --check && cargo clippy --all-targets -D warnings && cargo test --release --workspace -- --skip infinite_loop
git add docs/devkit/api-manual.md docs/user-manual.md docs/dev-guide.md sample/README.md
git commit -m "docs: oidc 全局/配置段/tenant 豁免 + sample OIDC 演示

unix@vip.qq.com ai"
```

### Phase 5 Gate（集中审查 5，终审）

- [ ] 全门禁绿（含新 e2e 用例）
- [ ] 安全自查：密钥未进 JS/日志；豁免面最小（仅跳转腿三段路径）；code/state/会话一次一用 + TTL；Location 只拼白名单 URL 与快照值；`json.raw` 未泄露内部错误
- [ ] spec 与实现一致性复核（含 json.raw 偏差已在 spec 补记一节）
- [ ] `sample/dist` 若要发 release 版本，另跑 `cargo run -p oj -- build auth -d sample/src -o sample/dist` → `... idp` → `... oidc`（构建顺序 auth 最先）

---

## Self-Review 记录

- **Spec coverage**：§3.1→Task 1/2；§3.2→Task 2/3；§3.3→Task 1/4；§4→Task 5/6/7；§5→Task 8/9；§6→Task 4/8/9；§7→Task 6/7/9 + Gate 3 VERIFY；§8→Task 3/4/5/7/10；§9→Task 11。spec §7「全部信封化」的偏差（json.raw 裸 JSON）已在 Task 3 显式标记并要求 Gate 1 复述确认。
- **Placeholder scan**：Task 10 的 `gen_cert`/`copy_dir` 标注为「复用文件内既有 helper 或按说明内联」并给出替代方案，非 TBD。
- **Type consistency**：`OidcState::from_section(&OidcSection, &Path)`、`issueTokens(uid, roles)`、`redirect/nowSecs/b64uFromHex/parseForm/parseCookies`（`auth/_shared/util.ts`）、KV 键 `OJ-OIDC:STATE:*`/`OJ-OIDC:CODE:*`/`OJ-IDP:SESS:*` 各任务一致。
