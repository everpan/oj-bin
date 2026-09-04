# auth 解耦实施计划：原语入 bridge，端点 JS 化，守卫 cdylib 化

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把内置 auth（login/refresh/logout 端点 + Bearer 守卫）从 `server` crate 解耦：端点变为 JS 业务模块，守卫变为 `oj-auth` cdylib 插件，Rust 核心只留 jwt/bcrypt/crypto 密码学原语 op。

**Architecture:** 见 `docs/superpowers/specs/2026-09-05-auth-decouple-design.md`（方案 A）。bridge 新增 `crypto.rs`（op 层）+ `auth.rs`（`AuthGuard` trait）；`oj-plugin-ffi` 增第 6 轴 `AuthGuardVtable`（同步函数指针，ABI 5→6）；`plugins/oj-auth` 实现守卫（验签 + 匿名匹配，纯同步无 db/kv）；`sample/src/auth/` 三个 JS 模块复刻现有 login/refresh/logout 逻辑。

**Tech Stack:** deno_core op2 / stabby FFI / jsonwebtoken / bcrypt / sha2 / getrandom / axum。

## Global Constraints

- 只许 release 构建：`cargo build --release` / `cargo xtask build`；禁止 debug 构建。
- 插件 crate 必须保持 `panic = "unwind"`（根 profile 已设，插件不得覆盖为 abort）。
- `src/bridge/bootstrap.js` 必须保持 7-bit ASCII（注释也不许中文）。
- 门禁：`cargo fmt --check`、`cargo clippy --all-targets -D warnings`、`cargo test --workspace`。
- 异步测试用 `tokio::test(flavor = "current_thread")`（JsRuntime 是 `!Send`）。
- ABI_VERSION 5→6 后所有插件必须重编译：`cargo xtask build`。
- 所有 SQL 值走绑定参数；表名标识符只允许来自白名单/配置校验后的串。

## 关键背景（实现者必读）

- `AuthGuard` trait 放 `src/bridge/auth.rs`（**不是** `guard.rs`——那是表归属守卫，已存在）。
- FFI host 包装器模式照抄 `src/bridge/plugin_loader.rs:146` 的 `es_backend`：
  `loaded.registrations.es.map(|vt| Arc::new(FfiEsBackend::new(0, vt)) as Arc<dyn EsBackend>)`。
  `LoadedPlugin.registrations` 是 host 侧镜像结构（`plugin_loader.rs:104` 附近），加 auth 槽要同步加这个镜像的字段与读取点（`plugin_loader.rs:337` 附近 `oj_plugin_init` 调用后读 `register()` 处）。
- `oj test`（`oj/src/test_cmd.rs` + `test_ext.rs`）经 `op_client_dispatch` 走**完整 axum App oneshot**——auth 守卫与路由行为与 server 完全一致，无需在 test 层复制管线。
- sample 的 `users` 表已由 `sample/src/_platform`（schema.yaml 声明 + seed.sql 灌入，demo/demo1234、trinity/demo1234）持有。新 `auth` 模块**不建表**，只在 manifest.yaml 声明 `deps: {_platform: "^0.1.0"}`（ownership_guard: deny 下跨模块读 users 必须声明，格式见 `sample/src/admin/manifest.yaml`）。
- `oj/src/app.rs:100` 起是 `App::from_config` 装配主流程；auth 装配在 235 行附近；Pipeline 构造在 424 行附近。
- 现状行为基线（必须保持一致，除「有意的行为变化」小节所列）：
  - login：用户不存在与密码错同报 401 "invalid credentials"；roles 列 JSON 解析失败回落 `[]`；uid 支持 number/string。
  - refresh：session 惰性判 exp；roles 重查库；旧 refresh 一次一用（先删后签新）。
  - logout：删 session，返回 data null。
  - 匿名匹配：精确或尾 `/*` 一层通配（`/pub/*` 命中 `/pub/x` 不命中 `/pub`）。
  - access token：Claims `{sub, roles, iat, exp}`，leeway 0，HS256/384/512。

## 有意的行为变化（验收时预期内）

1. `/auth/login|refresh|logout` 从内置路由变为业务路由：受**租户头校验**影响（`tenant.enable` 时缺头 → 400，原内置路由不校验）。sample 测试调用点需补 `X-TENANT-ID`。
2. `config.auth.user_table` 配置项**删除**（表名写在 JS 里）。
3. 守卫启用后业务路径 `/auth/*` 必须进 `anonymous_paths` 才能匿名访问（sample config 加三项）。
4. server crate 不再依赖 jsonwebtoken/bcrypt/sha2/getrandom。

---

### Task 1: bridge 密码学原语（crypto.rs + jwt 配置注入）

**Files:**
- Create: `src/bridge/crypto.rs`
- Modify: `Cargo.toml`（根 crate [dependencies] 加 `jsonwebtoken = "9"`、`bcrypt = "0.15"`、`sha2 = "0.10"`、`getrandom = "0.2"`）
- Modify: `src/bridge/mod.rs`（注册 op、`Extras.jwt`、`StableState.jwt`、`pub use`）
- Modify: `src/bridge/bootstrap.js`（挂 `jwt` / `bcrypt` / `crypto` 全局）

**Interfaces:**
- Produces:
  - `pub struct JwtCfg { pub secret: String, pub alg: String, pub access_secs: u64, pub refresh_secs: u64 }`，`JwtCfg::from_auth_cfg(&crate::config::AuthCfg) -> Result<Self, String>`
  - `Extras.jwt: Option<Arc<JwtCfg>>`；`StableState.jwt: Option<Arc<JwtCfg>>`
  - JS 全局：`jwt.sign({sub, roles}) -> Promise<string>`、`jwt.verify(token) -> Promise<claims>`、`jwt.accessDuration` / `jwt.refreshDuration`（getter，秒数）、`bcrypt.hash(pw, cost?)`、`bcrypt.verify(pw, hash)`、`crypto.sha256Hex(s)`、`crypto.randomHex(n?)`
  - 未配置 `auth:` 时 `jwt.*` 抛 "jwt not configured (config auth: section missing)"

- [ ] **Step 1: 写失败测试**（`src/bridge/crypto.rs` 的 `#[cfg(test)]`，先建空文件骨架让编译过）

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::{Bridge, Extras, InMemoryAccessor, InMemoryKV, SchemaRegistry};
    use std::collections::HashMap;
    use std::sync::Arc;

    fn jwt_cfg() -> Arc<JwtCfg> {
        Arc::new(JwtCfg {
            secret: "test-secret".into(),
            alg: "HS256".into(),
            access_secs: 60,
            refresh_secs: 720 * 3600,
        })
    }

    fn bridge(jwt: Option<Arc<JwtCfg>>) -> Bridge {
        Bridge::with_dbs_and_loader(
            HashMap::from([(
                "default".to_string(),
                Arc::new(InMemoryAccessor::new()) as Arc<dyn crate::bridge::DataAccessor>,
            )]),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            None,
            Extras {
                jwt,
                ..Default::default()
            },
        )
    }

    #[tokio::test(flavor = "current_thread")]
    async fn jwt_sign_verify_roundtrip_and_tamper() {
        let b = bridge(Some(jwt_cfg()));
        let cap = b
            .run_with(
                r#"(async () => {
                    const t = await jwt.sign({ sub: "7", roles: ["admin"] });
                    const c = await jwt.verify(t);
                    let tampered = null;
                    try { await jwt.verify(t + "x"); } catch (e) { tampered = String(e); }
                    json.ok({ sub: c.sub, roles: c.roles, tampered, dur: [jwt.accessDuration, jwt.refreshDuration] });
                })().catch((e) => json.fail(500, String(e)));"#,
                Default::default(),
            )
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["data"]["sub"], "7", "{v}");
        assert_eq!(v["data"]["roles"][0], "admin", "{v}");
        assert!(v["data"]["tampered"].is_string(), "{v}");
        assert_eq!(v["data"]["dur"], serde_json::json!([60, 720 * 3600]), "{v}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn jwt_not_configured_errors() {
        let b = bridge(None);
        let cap = b
            .run_with(
                r#"(async () => { await jwt.sign({ sub: "1", roles: [] }); })()
                    .catch((e) => json.ok({ err: String(e) }));"#,
                Default::default(),
            )
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(v["data"]["err"].as_str().unwrap().contains("jwt not configured"), "{v}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn bcrypt_and_crypto_ops() {
        let b = bridge(None);
        let cap = b
            .run_with(
                r#"(async () => {
                    const h = await bcrypt.hash("pw123", 4);
                    const okV = await bcrypt.verify("pw123", h);
                    const bad = await bcrypt.verify("nope", h);
                    json.ok({
                        okV, bad,
                        sha: crypto.sha256Hex("abc"),
                        hexLen: crypto.randomHex(32).length,
                        randType: typeof crypto.getRandomValues,
                    });
                })().catch((e) => json.fail(500, String(e)));"#,
                Default::default(),
            )
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["data"]["okV"], true, "{v}");
        assert_eq!(v["data"]["bad"], false, "{v}");
        assert_eq!(
            v["data"]["sha"],
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(v["data"]["hexLen"], 64);
    }
}
```

- [ ] **Step 2: 跑测试确认编译失败**

Run: `cargo test --release crypto:: 2>&1 | tail -5`
Expected: FAIL（`mod crypto` 不存在 / op 未注册）

- [ ] **Step 3: 实现 `src/bridge/crypto.rs`**

```rust
//! jwt / bcrypt / crypto 密码学原语 op（auth 解耦：核心只留原语，业务语义在 JS/插件）。
//! jwt 配置经 Extras.jwt 注入（装配层从 config.auth 构建）；未配置 → "jwt not configured"。

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use deno_core::{OpState, op2};
use deno_error::JsErrorBox;

use super::StableState;

/// jwt 运行时配置（装配期从 AuthCfg 构建，fail-fast 同旧 Auth::new 语义）。
pub struct JwtCfg {
    pub secret: String,
    /// HS256 | HS384 | HS512。
    pub alg: String,
    pub access_secs: u64,
    pub refresh_secs: u64,
}

impl JwtCfg {
    pub fn from_auth_cfg(cfg: &crate::config::AuthCfg) -> Result<Self, String> {
        // alg 合法性在此 fail-fast（与旧 Auth::new 一致）
        match cfg.signing_method.as_str() {
            "HS256" | "HS384" | "HS512" => {}
            other => {
                return Err(format!(
                    "auth.signing_method '{other}' not supported (HS256|HS384|HS512)"
                ));
            }
        }
        let access = crate::config::parse_duration(&cfg.access_token_duration)
            .map_err(|e| format!("auth.access_token_duration: {e}"))?;
        let refresh = crate::config::parse_duration(&cfg.refresh_token_duration)
            .map_err(|e| format!("auth.refresh_token_duration: {e}"))?;
        Ok(Self {
            secret: cfg.jwt_secret.clone(),
            alg: cfg.signing_method.clone(),
            access_secs: access.as_secs(),
            refresh_secs: refresh.as_secs(),
        })
    }

    fn algorithm(&self) -> jsonwebtoken::Algorithm {
        match self.alg.as_str() {
            "HS384" => jsonwebtoken::Algorithm::HS384,
            "HS512" => jsonwebtoken::Algorithm::HS512,
            _ => jsonwebtoken::Algorithm::HS256,
        }
    }
}

/// access token 载荷（与旧 server/auth.rs Claims 同形，守卫插件侧解码契约）。
#[derive(serde::Serialize, serde::Deserialize)]
struct Claims {
    sub: String,
    roles: Vec<String>,
    iat: u64,
    exp: u64,
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn jwt(state: &OpState) -> Result<Arc<JwtCfg>, JsErrorBox> {
    state
        .borrow::<Arc<StableState>>()
        .jwt
        .clone()
        .ok_or_else(|| JsErrorBox::generic("jwt not configured (config auth: section missing)"))
}

/// jwt.sign({sub, roles})：iat/exp 由 Rust 补（JS 不可控有效期）。
#[op2]
#[string]
pub fn op_jwt_sign(
    state: Rc<RefCell<OpState>>,
    #[serde] payload: serde_json::Value,
) -> Result<String, JsErrorBox> {
    let cfg = jwt(&state.borrow())?;
    let sub = payload["sub"]
        .as_str()
        .ok_or_else(|| JsErrorBox::generic("jwt.sign: payload.sub must be a string"))?;
    let roles: Vec<String> = payload["roles"]
        .as_array()
        .map(|a| a.iter().filter_map(|r| r.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let now = now_unix();
    let claims = Claims {
        sub: sub.to_string(),
        roles,
        iat: now,
        exp: now + cfg.access_secs,
    };
    jsonwebtoken::encode(
        &jsonwebtoken::Header::new(cfg.algorithm()),
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(cfg.secret.as_bytes()),
    )
    .map_err(|e| JsErrorBox::generic(e.to_string()))
}

/// jwt.verify(token) → claims；篡改/过期/算法不符均抛错（leeway 0）。
#[op2]
#[serde]
pub fn op_jwt_verify(
    state: Rc<RefCell<OpState>>,
    #[string] token: String,
) -> Result<serde_json::Value, JsErrorBox> {
    let cfg = jwt(&state.borrow())?;
    let mut v = jsonwebtoken::Validation::new(cfg.algorithm());
    v.leeway = 0;
    v.validate_exp = true;
    v.validate_aud = false;
    let d = jsonwebtoken::decode::<Claims>(
        &token,
        &jsonwebtoken::DecodingKey::from_secret(cfg.secret.as_bytes()),
        &v,
    )
    .map_err(|e| JsErrorBox::generic(e.to_string()))?;
    serde_json::to_value(d.claims).map_err(|e| JsErrorBox::generic(e.to_string()))
}

/// jwt.accessDuration / jwt.refreshDuration（秒；getter 每 runtime 惰性取）。
#[op2]
#[serde]
pub fn op_jwt_durations(state: Rc<RefCell<OpState>>) -> Result<serde_json::Value, JsErrorBox> {
    let cfg = jwt(&state.borrow())?;
    Ok(serde_json::json!({
        "access": cfg.access_secs,
        "refresh": cfg.refresh_secs,
    }))
}

/// bcrypt.hash(pw, cost?)：CPU 密集，spawn_blocking 避免卡住 isolate 所在线程。
#[op2(async)]
#[string]
pub async fn op_bcrypt_hash(
    #[string] password: String,
    cost: Option<u32>,
) -> Result<String, JsErrorBox> {
    tokio::task::spawn_blocking(move || {
        bcrypt::hash(password, cost.unwrap_or(bcrypt::DEFAULT_COST))
    })
    .await
    .map_err(|e| JsErrorBox::generic(e.to_string()))?
    .map_err(|e| JsErrorBox::generic(e.to_string()))
}

/// bcrypt.verify(pw, hash)：非法 hash → false（不抛错，对齐旧 unwrap_or(false)）。
#[op2(async)]
pub async fn op_bcrypt_verify(
    #[string] password: String,
    #[string] hash: String,
) -> Result<bool, JsErrorBox> {
    tokio::task::spawn_blocking(move || bcrypt::verify(password, &hash).unwrap_or(false))
        .await
        .map_err(|e| JsErrorBox::generic(e.to_string()))
}

/// crypto.sha256Hex(s)。
#[op2]
#[string]
pub fn op_sha256_hex(#[string] s: String) -> String {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    h.update(s.as_bytes());
    format!("{:x}", h.finalize())
}

/// crypto.randomHex(nBytes)：默认 32 字节 → 64 hex 字符（refresh token 用）。
#[op2]
#[string]
pub fn op_random_hex(n: Option<u32>) -> String {
    let n = n.unwrap_or(32).min(1024) as usize;
    let mut b = vec![0u8; n];
    getrandom::getrandom(&mut b).expect("system rng");
    b.iter().map(|x| format!("{x:02x}")).collect()
}
```

`src/bridge/mod.rs`：
- `mod crypto;` 加入模块列表；`pub use crypto::JwtCfg;`
- extension! ops 列表加：`crypto::op_jwt_sign, crypto::op_jwt_verify, crypto::op_jwt_durations, crypto::op_bcrypt_hash, crypto::op_bcrypt_verify, crypto::op_sha256_hex, crypto::op_random_hex,`
- `Extras` 加字段：`pub jwt: Option<Arc<JwtCfg>>,`（doc：None = jwt.* 报 not configured）
- `StableState` 加字段：`pub jwt: Option<Arc<JwtCfg>>,`，`with_dbs_and_loader` 里 `jwt: extras.jwt,`
- 注意 `mod tests` 里 `oj_require_cjs_interop_end_to_end` 手工构造 `StableState`（mod.rs:1224），须补 `jwt: None,`。

`src/bridge/bootstrap.js`（保持 7-bit ASCII）：
- import 列表加 7 个 op 名。
- 文件尾部追加：

```js
// ----- jwt: sign/verify (secret/alg/durations injected at assembly; not configured errors) -----
globalThis.jwt = {
  sign: (claims) => op_jwt_sign(claims === undefined ? null : claims),
  verify: (token) => op_jwt_verify(String(token)),
  get accessDuration() { return op_jwt_durations().access; },
  get refreshDuration() { return op_jwt_durations().refresh; },
};

// ----- bcrypt: password hashing (spawn_blocking on Rust side) -----
globalThis.bcrypt = {
  hash: (password, cost) => op_bcrypt_hash(String(password), cost === undefined ? null : cost | 0),
  verify: (password, hash) => op_bcrypt_verify(String(password), String(hash)),
};

// ----- crypto: sha256/random helpers (merge, keep native getRandomValues if present) -----
globalThis.crypto = Object.assign(globalThis.crypto || {}, {
  sha256Hex: (s) => op_sha256_hex(String(s)),
  randomHex: (n) => op_random_hex(n === undefined ? null : n | 0),
});
```

- [ ] **Step 4: 跑测试确认通过 + 门禁**

Run: `cargo test --release crypto:: && cargo fmt --check && cargo clippy --all-targets -D warnings`
Expected: 3 个测试 PASS，门禁绿。

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock src/bridge/crypto.rs src/bridge/mod.rs src/bridge/bootstrap.js
git commit -m "feat(bridge): jwt/bcrypt/crypto 密码学原语 op（auth 解耦第 1 步）

unix@vip.qq.com ai"
```

---

### Task 2: FFI auth 轴（AuthGuardVtable + ABI 6）

**Files:**
- Create: `oj-plugin-ffi/src/auth.rs`
- Modify: `oj-plugin-ffi/src/lib.rs`

**Interfaces:**
- Produces: `AuthGuardVtable { verify }`；`PluginRegistrations.auth` 槽位 + `PluginRegistrations::auth()` 访问器；`ABI_VERSION = 6`
- Consumes（Task 3/4 依赖）：

```rust
pub struct AuthGuardVtable {
    pub verify: extern "C" fn(path_no_base: RString, authorization: RString) -> RResult<RString, RString>,
}
```

- [ ] **Step 1: 写 `oj-plugin-ffi/src/auth.rs`**

```rust
//! auth 轴 vtable（Task auth-1）：请求守卫，同步纯密码学验签——无 async 跨边界，
//! 是全轴里最适合 FFI 的形态。ok 值 JSON：`null` = 匿名路径放行；
//! 对象 = 注入 http.user（`{"id","roles","claims"}`）；Err = 401 消息。
//! authorization 空串 = 无 Authorization 头。

use crate::{RResult, RString};

#[stabby::stabby]
#[repr(C)]
pub struct AuthGuardVtable {
    pub verify: extern "C" fn(path_no_base: RString, authorization: RString) -> RResult<RString, RString>,
}
```

- [ ] **Step 2: `oj-plugin-ffi/src/lib.rs` 接线**

- `pub mod auth;` + `pub use auth::AuthGuardVtable;`
- `PluginRegistrations` 加字段 `pub auth: *const AuthGuardVtable, // Task auth-1 起`，`none()` 加 `auth: std::ptr::null(),`，加访问器：

```rust
    pub fn auth(&self) -> Option<&'static AuthGuardVtable> {
        unsafe { self.auth.as_ref() }
    }
```

- `ABI_VERSION` 改 `6`，注释加一行 `/// 6 = auth 解耦起（PluginRegistrations 增 auth 槽位 + AuthGuardVtable）。`

- [ ] **Step 3: 编译验证 + commit**

Run: `cargo build --release -p oj-plugin-ffi && cargo fmt --check`
Expected: PASS（此时 workspace 其他成员会因 ABI 不等拒绝旧插件，属预期，后续任务跟进）

```bash
git add oj-plugin-ffi/
git commit -m "feat(ffi): auth 轴 AuthGuardVtable（同步验签守卫），ABI 5→6

unix@vip.qq.com ai"
```

---

### Task 3: core AuthGuard trait + host 包装器

**Files:**
- Create: `src/bridge/auth.rs`
- Modify: `src/bridge/ffi.rs`（加 `FfiAuthGuard`）
- Modify: `src/bridge/plugin_loader.rs`（`Registrations` host 镜像加 auth 字段与读取点；加 `auth_guard()`）
- Modify: `src/bridge/mod.rs`（`pub mod auth; pub use auth::AuthGuard;`）

**Interfaces:**
- Produces:
  - `pub trait AuthGuard: Send + Sync { fn verify(&self, path_no_base: &str, authorization: Option<&str>) -> Result<Option<serde_json::Value>, String>; }`
    （Ok(None) = 匿名放行；Ok(Some(user)) = 注入 http.user；Err = 401）
  - `plugin_loader::auth_guard(loaded: &LoadedPlugin) -> Option<Arc<dyn AuthGuard>>`
  - `ffi::FfiAuthGuard::new(vtable: &'static AuthGuardVtable) -> Self`（pub，供 oj 装配直接包 vtable）
- Consumes: Task 2 的 `AuthGuardVtable`。

- [ ] **Step 1: 写失败测试**（`src/bridge/auth.rs` 尾部 `#[cfg(test)]`：静态假 vtable 驱动 FfiAuthGuard）

```rust
#[cfg(test)]
mod tests {
    // 静态假 vtable：匿名 "/health"，token "good" → user，其余 Err。
    fn fake_verify(
        path: oj_plugin_ffi::RString,
        auth: oj_plugin_ffi::RString,
    ) -> oj_plugin_ffi::RResult<oj_plugin_ffi::RString, oj_plugin_ffi::RString> {
        let p: &str = &path;
        let a: &str = &auth;
        if p == "/health" {
            return oj_plugin_ffi::RResult::Ok("null".into());
        }
        if a == "Bearer good" {
            return oj_plugin_ffi::RResult::Ok(r#"{"id":"1","roles":["admin"]}"#.into());
        }
        oj_plugin_ffi::RResult::Err("missing or invalid bearer token".into())
    }

    static FAKE: oj_plugin_ffi::AuthGuardVtable = oj_plugin_ffi::AuthGuardVtable { verify: fake_verify };

    #[test]
    fn ffi_auth_guard_maps_results() {
        let g = crate::bridge::ffi::FfiAuthGuard::new(&FAKE);
        use crate::bridge::AuthGuard;
        assert!(g.verify("/health", None).unwrap().is_none());
        let u = g.verify("/me", Some("Bearer good")).unwrap().unwrap();
        assert_eq!(u["id"], "1");
        assert!(g.verify("/me", Some("Bearer bad")).is_err());
        assert!(g.verify("/me", None).is_err());
    }
}
```

注：若 `RString` 不能直接 `&str` deref，用 `path.as_str()` / `path[..]` 按 stabby API 调整（参考 `plugin_loader.rs` 里 `&loaded.descriptor.name[..]` 用法）。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --release auth:: 2>&1 | tail -5`
Expected: FAIL（trait / FfiAuthGuard 不存在）

- [ ] **Step 3: 实现**

`src/bridge/auth.rs`：

```rust
//! AuthGuard：HTTP 前置鉴权守卫契约（auth 解耦：实现迁入 oj-auth cdylib 插件，
//! core 只留 trait + FFI 适配）。server Pipeline 经 `Arc<dyn AuthGuard>` 消费。

/// 请求鉴权守卫。Ok(None) = 匿名路径放行；Ok(Some(user)) = 注入 http.user；Err = 401 消息。
pub trait AuthGuard: Send + Sync {
    fn verify(
        &self,
        path_no_base: &str,
        authorization: Option<&str>,
    ) -> Result<Option<serde_json::Value>, String>;
}
```

`src/bridge/ffi.rs`（仿 `FfiEsBackend`，同步版更简单）：

```rust
/// auth 守卫适配器：实现 core AuthGuard，经同步 vtable 转发（无 FfiFuture）。
pub struct FfiAuthGuard {
    vtable: &'static oj_plugin_ffi::AuthGuardVtable,
}

impl FfiAuthGuard {
    pub fn new(vtable: &'static oj_plugin_ffi::AuthGuardVtable) -> Self {
        Self { vtable }
    }
}

impl crate::bridge::auth::AuthGuard for FfiAuthGuard {
    fn verify(
        &self,
        path_no_base: &str,
        authorization: Option<&str>,
    ) -> Result<Option<serde_json::Value>, String> {
        let r = (self.vtable.verify)(
            oj_plugin_ffi::RString::from(path_no_base),
            oj_plugin_ffi::RString::from(authorization.unwrap_or("")),
        );
        match std::result::Result::from(r) {
            Ok(json) => {
                let v: serde_json::Value = serde_json::from_str(&json[..])
                    .map_err(|e| format!("auth plugin returned bad json: {e}"))?;
                Ok(if v.is_null() { None } else { Some(v) })
            }
            Err(e) => Err(e[..].to_string()),
        }
    }
}
```

`src/bridge/plugin_loader.rs`：
- host 侧 `Registrations` 镜像结构加 `pub auth: Option<&'static AuthGuardVtable>,`，并在读取 `register()` 结果处补 `auth: regs.auth(),`（找到构建该镜像的位置——`oj_plugin_init` 调用后）。
- 加：

```rust
/// 从已加载插件的 auth 槽构造 core 守卫（Task auth-1）。
pub fn auth_guard(loaded: &LoadedPlugin) -> Option<Arc<dyn crate::bridge::AuthGuard>> {
    loaded
        .registrations
        .auth
        .map(|vt| Arc::new(super::ffi::FfiAuthGuard::new(vt)) as Arc<dyn crate::bridge::AuthGuard>)
}
```

`src/bridge/mod.rs`：`pub mod auth;` + `pub use auth::AuthGuard;`。

- [ ] **Step 4: 跑测试 + 门禁 + commit**

Run: `cargo test --release auth:: && cargo clippy --all-targets -D warnings`
Expected: PASS

```bash
git add src/bridge/auth.rs src/bridge/ffi.rs src/bridge/plugin_loader.rs src/bridge/mod.rs
git commit -m "feat(bridge): AuthGuard trait + FfiAuthGuard host 包装器（auth 解耦第 3 步）

unix@vip.qq.com ai"
```

---

### Task 4: plugins/oj-auth 插件

**Files:**
- Create: `plugins/oj-auth/Cargo.toml`、`plugins/oj-auth/src/lib.rs`
- Modify: 根 `Cargo.toml`（members 加 `"plugins/oj-auth"`）
- Modify: `tools/xtask/src/main.rs`（`PLUGINS` 数组加 `"auth"`，注意保持现有排序/格式）

**Interfaces:**
- Consumes: Task 2 `AuthGuardVtable`、Task 3 `AuthGuard` 语义契约。
- Produces: cdylib `oj-auth`，descriptor name `"auth"`；init cfg JSON = `{"jwt_secret","signing_method","anonymous_paths":[]}`。

- [ ] **Step 1: 写 `plugins/oj-auth/Cargo.toml`**

```toml
[package]
name = "oj-auth"
version = "0.1.0"
edition = "2024"
description = "auth 轴守卫 cdylib 插件：JWT 验签 + 匿名路径匹配（迁自 server/auth.rs）"

[lib]
crate-type = ["cdylib"]

[dependencies]
oj-plugin-ffi = { path = "../../oj-plugin-ffi" }
jsonwebtoken = "9"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

- [ ] **Step 2: 写 `plugins/oj-auth/src/lib.rs`（含单元测试）**

整体结构仿 `plugins/oj-kv-redis/src/lib.rs`（OnceLock 插件态 + `oj_plugin_entry!`）。守卫逻辑逐字对齐旧 `server/src/auth.rs` 的 `verify_access` / `is_anonymous`：

```rust
//! oj-auth：auth 轴守卫 cdylib 插件（auth 解耦）。只含守卫（验签 + 匿名匹配）；
//! login/refresh/logout 端点已 JS 化（sample/src/auth/），本插件无 db/kv 依赖。
//! cfg 契约：init cfg = {"jwt_secret","signing_method","anonymous_paths":[...]} JSON。

use oj_plugin_ffi::{
    AuthGuardVtable, HostContext, PluginDescriptor, PluginRegistrations, RArc, RResult, RString,
};
use std::sync::OnceLock;

/// access token 载荷（与 core bridge crypto.rs Claims 同形）。
#[derive(serde::Serialize, serde::Deserialize)]
struct Claims {
    sub: String,
    roles: Vec<String>,
    iat: u64,
    exp: u64,
}

#[derive(serde::Deserialize)]
struct GuardCfg {
    jwt_secret: String,
    #[serde(default = "default_alg")]
    signing_method: String,
    #[serde(default)]
    anonymous_paths: Vec<String>,
}

fn default_alg() -> String {
    "HS256".into()
}

struct Guard {
    dec: jsonwebtoken::DecodingKey,
    alg: jsonwebtoken::Algorithm,
    anon: Vec<String>,
}

static GUARD: OnceLock<Guard> = OnceLock::new();

impl Guard {
    fn new(cfg: &GuardCfg) -> Result<Self, String> {
        let alg = match cfg.signing_method.as_str() {
            "HS256" => jsonwebtoken::Algorithm::HS256,
            "HS384" => jsonwebtoken::Algorithm::HS384,
            "HS512" => jsonwebtoken::Algorithm::HS512,
            other => return Err(format!("signing_method '{other}' not supported")),
        };
        Ok(Self {
            dec: jsonwebtoken::DecodingKey::from_secret(cfg.jwt_secret.as_bytes()),
            alg,
            anon: cfg.anonymous_paths.clone(),
        })
    }

    /// 精确匹配或尾 "/*" 一层前缀通配（"/pub/*" 命中 "/pub/x"，不命中 "/pub"）。
    fn is_anonymous(&self, path: &str) -> bool {
        self.anon.iter().any(|p| {
            if let Some(prefix) = p.strip_suffix("/*") {
                path.starts_with(prefix) && path.len() > prefix.len()
            } else {
                path == p
            }
        })
    }

    fn verify(&self, path: &str, authorization: &str) -> Result<serde_json::Value, String> {
        if self.is_anonymous(path) {
            return Ok(serde_json::Value::Null);
        }
        let token = authorization
            .strip_prefix("Bearer ")
            .ok_or("missing or invalid bearer token")?;
        let mut v = jsonwebtoken::Validation::new(self.alg);
        v.leeway = 0;
        v.validate_exp = true;
        v.validate_aud = false;
        let claims = jsonwebtoken::decode::<Claims>(token, &self.dec, &v)
            .map(|d| d.claims)
            .map_err(|_| "missing or invalid bearer token".to_string())?;
        Ok(serde_json::json!({
            "id": claims.sub,
            "roles": claims.roles,
            "claims": claims,
        }))
    }
}

extern "C" fn verify(path: RString, authorization: RString) -> RResult<RString, RString> {
    oj_plugin_ffi::catch_value(|| {
        let g = GUARD.get().ok_or("oj-auth: init not called")?;
        match g.verify(&path, &authorization) {
            Ok(v) => Ok(RString::from(v.to_string())),
            Err(msg) => Err(RString::from(msg)),
        }
    })
}

static VTABLE: AuthGuardVtable = AuthGuardVtable { verify };

fn init(_host: RArc<HostContext>, cfg: RString) -> RResult<PluginDescriptor, RString> {
    let parsed: GuardCfg = match serde_json::from_str(&cfg) {
        Ok(c) => c,
        Err(e) => return RResult::Err(RString::from(format!("oj-auth cfg: {e}"))),
    };
    let guard = match Guard::new(&parsed) {
        Ok(g) => g,
        Err(e) => return RResult::Err(RString::from(e)),
    };
    let _ = GUARD.set(guard);
    RResult::Ok(PluginDescriptor {
        name: RString::from("auth"),
        semver: RString::from(env!("CARGO_PKG_VERSION")),
        abi_version: oj_plugin_ffi::ABI_VERSION,
        fingerprint: RString::from(oj_plugin_ffi::HOST_FINGERPRINT),
        register: || PluginRegistrations {
            auth: &VTABLE,
            ..PluginRegistrations::none()
        },
    })
}

oj_plugin_ffi::oj_plugin_entry!(init);

#[cfg(test)]
mod tests {
    use super::*;

    fn guard() -> Guard {
        Guard::new(&GuardCfg {
            jwt_secret: "s3cret".into(),
            signing_method: "HS256".into(),
            anonymous_paths: vec!["/health".into(), "/auth/*".into()],
        })
        .unwrap()
    }

    fn sign(g: &Guard, sub: &str, roles: &[&str], exp_offset: i64) -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let claims = serde_json::json!({
            "sub": sub, "roles": roles,
            "iat": now, "exp": now + exp_offset,
        });
        jsonwebtoken::encode(
            &jsonwebtoken::Header::new(g.alg),
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(b"s3cret"),
        )
        .unwrap()
    }

    #[test]
    fn anonymous_matching() {
        let g = guard();
        assert!(g.is_anonymous("/health") && g.is_anonymous("/auth/login"));
        assert!(!g.is_anonymous("/auth") && !g.is_anonymous("/me"));
    }

    #[test]
    fn verify_anonymous_valid_tampered_expired() {
        let g = guard();
        assert_eq!(g.verify("/health", "").unwrap(), serde_json::Value::Null);
        let t = sign(&g, "1", &["admin"], 60);
        let u = g.verify("/me", &format!("Bearer {t}")).unwrap();
        assert_eq!(u["id"], "1");
        assert_eq!(u["roles"][0], "admin");
        assert!(g.verify("/me", &format!("Bearer {t}x")).is_err());
        assert!(g.verify("/me", "no-bearer").is_err());
        assert!(g.verify("/me", "").is_err());
        let past = sign(&g, "1", &[], -60);
        assert!(g.verify("/me", &format!("Bearer {past}")).is_err());
    }
}
```

注意：若 `catch_value` 签名不匹配上述用法，按 `oj-plugin-ffi/src/future.rs` 实际签名调整（它用于把 panic/错误收敛为 RResult）。`Claims` 序列化进 `claims` 字段需要 `serde::Serialize`（已 derive）。

- [ ] **Step 3: 跑插件测试**

Run: `cargo test --release -p oj-auth`
Expected: 2 个测试 PASS

- [ ] **Step 4: workspace 成员 + xtask + 构建预检**

根 `Cargo.toml` members 加 `"plugins/oj-auth"`；`tools/xtask/src/main.rs` 的 `PLUGINS` 加 `"auth"`。

Run: `cargo xtask plugin auth --check`
Expected: 构建 + ABI/符号预检通过，产物入 `bin/plugins/<triple>/`

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock plugins/oj-auth tools/xtask/src/main.rs
git commit -m "feat(plugins): oj-auth 守卫插件（JWT 验签 + 匿名匹配，迁自 server/auth.rs）

unix@vip.qq.com ai"
```

---

### Task 5: server crate 手术（删内置 auth，Pipeline trait 化）

**Files:**
- Delete: `server/src/auth.rs`
- Modify: `server/src/lib.rs`（`pub mod auth;` 删除、Pipeline/handle 重写、`auth_json` 删除、测试重写）
- Modify: `server/Cargo.toml`（删 `jsonwebtoken`/`bcrypt`/`sha2`/`getrandom` 四依赖——已确认仅 auth.rs 与 lib.rs 测试使用）

**Interfaces:**
- Consumes: Task 3 的 `only_js::bridge::AuthGuard`。
- Produces: `Pipeline.auth: Option<Arc<dyn AuthGuard>>`；`handle()` 无 `/auth/*` 内置分支（走业务路由表 → 无模块时 404）。

- [ ] **Step 1: 重写 server 测试**（`server/src/lib.rs` 内：删 `auth_full_pipeline`，换成 stub guard 测试）

```rust
    /// Stub 守卫：/health 匿名；Bearer good → user；其余 Err。
    struct StubGuard;
    impl only_js::bridge::AuthGuard for StubGuard {
        fn verify(
            &self,
            path: &str,
            auth: Option<&str>,
        ) -> Result<Option<Value>, String> {
            if path == "/health" {
                return Ok(None);
            }
            match auth {
                Some("Bearer good") => Ok(Some(serde_json::json!({
                    "id": "1", "roles": ["admin"],
                    "claims": {"sub": "1", "roles": ["admin"], "iat": 0, "exp": 0},
                }))),
                _ => Err("missing or invalid bearer token".into()),
            }
        }
    }

    /// 守卫管线：401 / 匿名放行 / http.user 注入；内置 /auth/* 路由已删除（404）。
    #[tokio::test]
    async fn auth_guard_pipeline() {
        let t = routes(&[
            ("me/api.ts", "export default { get() { json.ok({ u: http.user }); } };"),
            ("health/api.ts", "export default { get() { json.ok({ ok: 1 }); } };"),
        ]);
        let addr = spawn_pipeline(
            "/v1/api",
            t.0.clone(),
            true,
            None,
            Pipeline {
                auth: Some(Arc::new(StubGuard)),
                ..Default::default()
            },
        )
        .await;
        let get = |p: &str, token: Option<&str>| {
            format!(
                "GET {p} HTTP/1.1\r\nHost: t\r\n{}Connection: close\r\n\r\n",
                token.map(|t| format!("Authorization: {t}\r\n")).unwrap_or_default()
            )
        };
        // 无 token → 401；坏 token → 401
        let r = raw_http(addr, &get("/v1/api/me/", None)).await;
        assert!(r.starts_with("HTTP/1.1 401"), "{r}");
        let r = raw_http(addr, &get("/v1/api/me/", Some("Bearer bad"))).await;
        assert!(r.starts_with("HTTP/1.1 401"), "{r}");
        // 匿名路径放行
        let r = raw_http(addr, &get("/v1/api/health/", None)).await;
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        // 注入 http.user
        let r = raw_http(addr, &get("/v1/api/me/", Some("Bearer good"))).await;
        assert!(r.starts_with("HTTP/1.1 200") && r.contains("\"id\":\"1\""), "{r}");
        // 内置 auth 路由已删除：无对应业务模块 → 404（不再是 200/405）
        let r = raw_http(
            addr,
            "POST /v1/api/auth/login HTTP/1.1\r\nHost: t\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
        )
        .await;
        assert!(r.starts_with("HTTP/1.1 404"), "{r}");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --release -p mdm-server 2>&1 | tail -5`（crate 名以 server/Cargo.toml 为准）
Expected: FAIL（`server::auth` 尚在但 `Pipeline.auth` 类型未变时测试编译失败）

- [ ] **Step 3: 实施手术**

`server/src/lib.rs`：
1. 删 `pub mod auth;`；`use crate::auth::Auth;` 改为 `use only_js::bridge::AuthGuard;`。
2. `Pipeline.auth` 字段类型改 `Option<Arc<dyn AuthGuard>>`，doc 改为 `/// Some = 鉴权启用：Bearer 守卫 + http.user（实现由 oj-auth 插件提供）`。
3. `handle()` 删整段内置 auth 路由分支（`if let Some(auth) = st.pipeline.auth.clone() && let Some(rest) = ...` 到对应闭合），连同 `auth_json` 函数删除。
4. `run` 闭包内 Bearer 分支替换为：

```rust
            // 前置管线：鉴权（base 内非匿名路径必须过守卫 → 401；Ok(None) = 匿名放行）。
            let user = match (st.pipeline.auth.as_ref(), path_no_base.as_deref()) {
                (Some(guard), Some(p)) => {
                    let header = headers
                        .get(axum::http::header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok());
                    match guard.verify(p, header) {
                        Ok(Some(u)) => Some(u),
                        Ok(None) => None,
                        Err(msg) => return fail_response(401, &msg),
                    }
                }
                _ => None,
            };
```

5. 删 `server/src/auth.rs`；`server/Cargo.toml` 删四依赖。
6. lib.rs 测试里 `use crate::auth::Auth` / bcrypt 建表代码（旧 `auth_full_pipeline`）整体删除。

- [ ] **Step 4: 跑测试 + 门禁 + commit**

Run: `cargo test --release -p mdm-server && cargo clippy --all-targets -D warnings`
Expected: PASS（注意：此时 oj crate 编译会断——`server::auth::Auth` 引用待 Task 6 修）

```bash
git add server/
git commit -m "refactor(server): 删内置 auth 路由与 Auth，Pipeline.auth 改 AuthGuard trait

unix@vip.qq.com ai"
```

---

### Task 6: oj 装配 + config（user_table 删除）

**Files:**
- Modify: `src/config.rs`（`AuthCfg` 删 `user_table` 字段与 Default；相关测试同步删）
- Modify: `oj/src/server_cmd.rs`（`Registries.auth`、`build_registries`、`plugin_cfg_json`）
- Modify: `oj/src/app.rs`（auth 装配改插件守卫 + JwtCfg 注入 Extras）

**Interfaces:**
- Consumes: Task 3 `plugin_loader::auth_guard` / `ffi::FfiAuthGuard`、Task 1 `JwtCfg`、Task 4 oj-auth 产物。
- Produces: `Registries.auth: Option<&'static oj_plugin_ffi::AuthGuardVtable>`。

- [ ] **Step 1: 写失败测试**（`oj/src/server_cmd.rs` tests：仿 463 行附近既有 auth 配置测试 + 1178 行 `es_plugin_wires_backend` 模式）

```rust
    /// auth 声明但无 auth 插件 → fail-fast；插件在 → vtable 进 Registries。
    #[tokio::test]
    async fn auth_plugin_required_when_configured() {
        // 无插件清单（空目录扫描）+ auth 配置 → Err
        let mut cfg = Config::default();
        cfg.auth = Some(serde_yaml::from_str("jwt_secret: \"x\"\n").unwrap());
        let tmp = std::env::temp_dir().join(format!("oj-authreg-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let mut reg = Registries::default();
        let r = assemble_plugins(&cfg, &tmp, &mut reg).await;
        assert!(r.is_err(), "auth configured without plugin must fail");
        let _ = std::fs::remove_dir_all(&tmp);
    }
```

若 `bin/plugins/<triple>` 已有构建好的 `auth` 插件，再加一个正向用例（仿 `es_plugin_wires_backend` 的加载路径）：装配后 `registries.auth.is_some()`，并经 `FfiAuthGuard` verify 验签一个测试 token（sign 用 `jsonwebtoken`，需给 `oj/Cargo.toml` `[dev-dependencies]` 加 `jsonwebtoken = "9"`）。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --release -p oj auth_plugin 2>&1 | tail -5`
Expected: FAIL（行为未实现）

- [ ] **Step 3: 实施**

`src/config.rs`：`AuthCfg` 删 `pub user_table: String` 与 Default 里的 `user_table: "users".into()`；该文件 463 行附近测试同步删 user_table 引用。

`oj/src/server_cmd.rs`：
1. `Registries` 加 `pub auth: Option<&'static oj_plugin_ffi::AuthGuardVtable>,`（doc：auth 键选单槽，多 auth 插件冲突 fail fast）。
2. `build_registries` 加（仿 es 块）：

```rust
    let auth_plugins: Vec<&LoadedPlugin> = loaded
        .iter()
        .filter(|p| p.registrations.auth.is_some())
        .collect();
    if cfg.auth.is_some() && auth_plugins.is_empty() {
        return Err(
            "config declares [auth] but no auth plugin loaded (run `cargo xtask plugin auth`)"
                .to_string(),
        );
    }
    if auth_plugins.len() > 1 {
        return Err("plugins conflict: multiple plugins register auth guard".to_string());
    }
    registries.auth = auth_plugins.first().and_then(|p| p.registrations.auth);
```

（按 `build_registries` 现有返回值/赋值风格对齐——它是返回 `Registries` 还是原地改，以现状为准。）
3. `plugin_cfg_json` 加：

```rust
    if name == "auth" {
        if let Some(a) = &cfg.auth {
            return serde_json::json!({
                "jwt_secret": a.jwt_secret,
                "signing_method": a.signing_method,
                "anonymous_paths": a.anonymous_paths,
            })
            .to_string();
        }
    }
```

`oj/src/app.rs`（235 行附近）替换 auth 装配：

```rust
        // 鉴权：守卫由 oj-auth 插件提供（缺插件 fail-fast 已在 build_registries 完成）；
        // jwt 原语配置注入 bridge Extras（JS 端点 jwt.sign/verify 用）。
        let auth: Option<Arc<dyn server::AuthGuard>> = match &cfg.auth {
            Some(a) if a.jwt_secret.trim().is_empty() => {
                return Err("auth.jwt_secret must not be empty".into());
            }
            Some(_) => registries
                .auth
                .map(|vt| Arc::new(only_js::bridge::ffi_auth_guard(vt)) as Arc<dyn server::AuthGuard>),
            None => None,
        };
        let jwt = cfg
            .auth
            .as_ref()
            .map(only_js::bridge::JwtCfg::from_auth_cfg)
            .transpose()
            .map_err(|e| format!("auth: {e}"))?
            .map(Arc::new);
```

注：`server::AuthGuard` 若 server 不 re-export，则 `use only_js::bridge::AuthGuard` 并在 server Pipeline 字段处同类型；`ffi_auth_guard` 不存在——直接用 `only_js::bridge::plugin_loader` 拿不到 vtable 包装（auth_guard 吃 LoadedPlugin）。**最简路径**：把 `FfiAuthGuard::new` 经 `src/bridge/mod.rs` re-export（`pub use ffi::FfiAuthGuard;` 需先把 `mod ffi` 改 `pub(crate)` → 不行，ffi 含 libloading 私有面。改为在 `plugin_loader.rs` 加 `pub fn auth_guard_from_vtable(vt: &'static AuthGuardVtable) -> Arc<dyn AuthGuard>`），Task 3 实现时一并落。app.rs 里用 `registries.auth.map(plugin_loader::auth_guard_from_vtable)`。
`make_bridge` 闭包的 `Extras { .. }` 加 `jwt: jwt.clone(),`（闭包捕获列表加 `jwt`）。

- [ ] **Step 4: 全 workspace 编译 + 测试 + 门禁**

Run: `cargo xtask build && cargo test --release -p oj auth_plugin && cargo clippy --all-targets -D warnings`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/config.rs oj/src/server_cmd.rs oj/src/app.rs oj/Cargo.toml src/bridge/plugin_loader.rs src/bridge/mod.rs
git commit -m "feat(oj): auth 装配切换到 oj-auth 插件守卫 + JwtCfg 注入；删 auth.user_table

unix@vip.qq.com ai"
```

---

### Task 7: sample auth 模块（JS 端点，逐行对齐旧逻辑）

**Files:**
- Create: `sample/src/auth/manifest.yaml`、`sample/src/auth/_shared/session.ts`、`sample/src/auth/login/api.ts`、`sample/src/auth/refresh/api.ts`、`sample/src/auth/logout/api.ts`、`sample/src/auth/README.md`
- Modify: `sample/config.yaml`（auth 段：删 `user_table`、`anonymous_paths` 加 `/auth/login`、`/auth/refresh`、`/auth/logout`，注释更新）

**Interfaces:**
- Consumes: Task 1 的 `jwt/bcrypt/crypto/kv/db` 全局；`_platform` 的 users 表。
- Produces: 路由 `POST /v1/api/auth/{login,refresh,logout}/`（尾斜杠有无等价，`routes::normalize` 归一）。

- [ ] **Step 1: 写 `sample/src/auth/manifest.yaml` + `_shared/session.ts`**

```yaml
name: "auth"
desc: "JWT 鉴权端点（JS 业务实现）：login/refresh/logout；守卫由 oj-auth 插件提供"
version: "0.1.0"
deps:              # login/refresh 读 _platform.users（ownership_guard: deny 下必须声明）
  _platform: "^0.1.0"
```

```ts
// 会话与签发共享逻辑（对齐旧 server/auth.rs token_pair/session_* 语义）。

export function nowSecs(): number {
  return Math.floor(Date.now() / 1000);
}

export function sessionKey(refreshToken: string): string {
  return "AUTH-SESSION:" + crypto.sha256Hex(refreshToken);
}

// 签 access + 生成 refresh 并落 session（exp = now + refresh 时长，读取侧惰性判定）。
export async function issueTokens(uid: string, roles: string[]) {
  const accessToken = await jwt.sign({ sub: uid, roles });
  const refreshToken = crypto.randomHex(32);
  await kv.set(
    sessionKey(refreshToken),
    JSON.stringify({ uid, exp: nowSecs() + jwt.refreshDuration }),
  );
  return {
    access_token: accessToken,
    refresh_token: refreshToken,
    expires_in: jwt.accessDuration,
    user: { id: uid, roles },
  };
}
```

- [ ] **Step 2: 写三个端点**

`login/api.ts`：

```ts
import { issueTokens } from "../_shared/session";

export default {
  async post() {
    const body = http.body || {};
    const rows = await db.query(
      "select id, password_hash, roles from users where username = ?",
      [String(body.username ?? "")],
    );
    const row = rows[0];
    // 用户不存在与密码错同报（不泄露用户存在性）。
    if (!row || !(await bcrypt.verify(String(body.password ?? ""), row.password_hash || ""))) {
      json.fail(401, "invalid credentials");
      return;
    }
    // roles 列按 JSON 数组串解析，失败回落空。
    let roles: string[] = [];
    try { roles = JSON.parse(row.roles || "[]"); } catch { roles = []; }
    json.ok(await issueTokens(String(row.id), roles));
  },
};
```

`refresh/api.ts`：

```ts
import { issueTokens, nowSecs, sessionKey } from "../_shared/session";

export default {
  async post() {
    const token = String((http.body || {}).refresh_token ?? "");
    const key = sessionKey(token);
    const raw = await kv.get(key);
    const sess = raw ? JSON.parse(raw) : null;
    if (!sess || !(sess.exp > nowSecs())) {
      json.fail(401, "invalid or expired refresh token");
      return;
    }
    // session 只存 uid——roles 重查库取最新。
    const rows = await db.query("select roles from users where id = ?", [sess.uid]);
    let roles: string[] = [];
    try { roles = JSON.parse((rows[0] || {}).roles || "[]"); } catch { roles = []; }
    // 轮换：先删旧 session（旧 refresh 立即失效，一次一用）再签新对。
    await kv.del(key);
    json.ok(await issueTokens(String(sess.uid), roles));
  },
};
```

`logout/api.ts`：

```ts
import { sessionKey } from "../_shared/session";

export default {
  async post() {
    const token = String((http.body || {}).refresh_token ?? "");
    await kv.del(sessionKey(token));
    json.ok(null);
  },
};
```

`README.md`：三段式（干什么 / 怎么改：换登录方式或用户表就改 login/api.ts / 守卫在 oj-auth 插件）。

- [ ] **Step 3: 更新 `sample/config.yaml`**

auth 段改为：

```yaml
auth:   # JWT 鉴权：oj-auth 插件守卫（验签/匿名）+ JS 端点（src/auth/，jwt 原语注入）
  jwt_secret: "change-me"        # 生产必改；空串启动 fail-fast
  signing_method: "HS256"        # HS256 | HS384 | HS512
  access_token_duration: "60s"   # access token 有效期（s/m/h/d）
  refresh_token_duration: "720h" # refresh session 有效期（轮换制）
  anonymous_paths:               # 免鉴权路径（去 /v1/api 前缀；尾 /* 为一层通配）
    - "/health"
    - "/auth/login"              # auth 端点已是业务路由，须显式匿名
    - "/auth/refresh"
    - "/auth/logout"
```

（删 `user_table` 行。）

- [ ] **Step 4: 手工冒烟**

```bash
cargo xtask build
./bin/oj server -c sample/config.yaml --api-path sample/src &
curl -s -X POST http://localhost:9778/v1/api/auth/login \
  -H 'Content-Type: application/json' -H 'X-TENANT-ID: default' \
  -d '{"username":"demo","password":"demo1234"}'
# 期望：{"code":0,"data":{"access_token":...,"refresh_token":...,"expires_in":60,"user":{"id":"1","roles":["admin"]}}}
# 再验：无 Bearer 访问 /v1/api/auth_demo/me/ → 401；带 token → 200；refresh 轮换 + 旧 token 二次使用 401；logout 后 refresh 401。
kill %1
```

- [ ] **Step 5: Commit**

```bash
git add sample/src/auth sample/config.yaml
git commit -m "feat(sample): auth 模块 JS 端点（login/refresh/logout，对齐旧内置逻辑）

unix@vip.qq.com ai"
```

---

### Task 8: 测试收口 + 文档 + 全量门禁

**Files:**
- Modify: `oj/src/test_ext/test_bootstrap.js`（`client.login` 加可选 headers 参数）
- Modify: `sample/tests/*.ts`（login 调用点补 `X-TENANT-ID`；auth.test.ts 头部注释更新）
- Modify: `docs/builtin-api-auth.md`（重写 auth 章节：内置路由 → JS 模块 + oj-auth 插件）
- Modify: `docs/user-manual.md`（477 行附近 auth 描述同步）、`docs/testing.md`（client.login 签名）

**Interfaces:**
- Consumes: 全部前序任务。

- [ ] **Step 1: `client.login` 签名扩展**（`oj/src/test_ext/test_bootstrap.js`，保持 ASCII）

```js
  // login helper: POST /auth/login -> returns data.access_token.
  // usage: const token = await client.login("demo", "demo1234", {"X-TENANT-ID": "default"});
  c.login = async (username, password, headers) => {
    const r = await c.post("/auth/login", {
      headers: headers || {},
      body: JSON.stringify({ username, password }),
    });
    if (r.status !== 200) throw new Error("login failed: " + r.status);
    return JSON.parse(r.body).data.access_token;
  };
```

（先读该文件现状，保持其既有结构与注释风格，只加 headers 透传。）

- [ ] **Step 2: sample/tests 全部 login 调用点补租户头**

`auth.test.ts`、`news.test.ts`、`cert.test.ts`、`user.test.ts`、`admin.test.ts`：
- `client.login("demo", "demo1234")` → `client.login("demo", "demo1234", { "X-TENANT-ID": "default" })`（trinity 同理）。
- 直接 `client.post("/auth/login", ...)` 的（auth.test.ts 错密码用例）headers 加 `"X-TENANT-ID": "default"`。
- auth.test.ts 注释「匿名仅 /health」更新为含 `/auth/*` 三项。
- 补 refresh 轮换用例（对齐旧 Rust 测试语义）：

```ts
  it("refresh rotates and old token is single-use", async () => {
    const H = { "X-TENANT-ID": "default" };
    const r1 = await client.post("/auth/login", {
      headers: H, body: JSON.stringify({ username: "demo", password: "demo1234" }),
    });
    const rt1 = JSON.parse(r1.body).data.refresh_token;
    const r2 = await client.post("/auth/refresh", {
      headers: H, body: JSON.stringify({ refresh_token: rt1 }),
    });
    expect(r2.status).toBe(200);
    const r3 = await client.post("/auth/refresh", {
      headers: H, body: JSON.stringify({ refresh_token: rt1 }),
    });
    expect(r3.status).toBe(401);
  });

  it("logout kills the session", async () => {
    const H = { "X-TENANT-ID": "default" };
    const r1 = await client.post("/auth/login", {
      headers: H, body: JSON.stringify({ username: "demo", password: "demo1234" }),
    });
    const rt = JSON.parse(r1.body).data.refresh_token;
    await client.post("/auth/logout", { headers: H, body: JSON.stringify({ refresh_token: rt }) });
    const r2 = await client.post("/auth/refresh", {
      headers: H, body: JSON.stringify({ refresh_token: rt }),
    });
    expect(r2.status).toBe(401);
  });
```

- [ ] **Step 3: 跑 L1 测试**

Run: `./bin/oj test -c sample/config.yaml -d sample/src --format human`
Expected: 全部 PASS（含新旧 auth 用例）

- [ ] **Step 4: 文档更新**

- `docs/builtin-api-auth.md`：把「内置接口总览」里三条 `/auth/*` 改为「业务模块 `sample/src/auth/`（JS 实现）+ oj-auth 插件守卫」；流程图中「路径是 base/auth/* 且已启用 auth」分支删除，守卫走统一前置管线；时序图改标 JS 实现；关键认知第 2 条改为「守卫（oj-auth 插件）在路由表命中后的前置管线执行；auth 端点是普通业务路由」。
- `docs/user-manual.md` 477 行附近：auth 描述改为「端点 JS 化可改写（sample/src/auth/），守卫由 oj-auth 插件提供」。
- `docs/testing.md`：`client.login` 签名补 headers 参数。
- `CLAUDE.md`「内置的 `/auth/*`」表述（工作区布局 server 条目）同步为「守卫 trait + http.user」。

- [ ] **Step 5: 全量门禁 + commit**

```bash
cargo xtask build
cargo fmt --check
cargo clippy --all-targets -D warnings
cargo test --workspace --release
./bin/oj test -c sample/config.yaml -d sample/src --format human
```

Expected: 全绿。

```bash
git add -A
git commit -m "test+docs: auth 解耦收口（L1 测试租户头/轮换用例、文档同步）

unix@vip.qq.com ai"
```

---

## Self-Review 记录

- Spec 覆盖：§1 原语 → Task 1；§2 FFI 轴/守卫插件/server 手术 → Task 2/3/4/5；§3 JS 示例 → Task 7；§4 配置 → Task 6/7；测试节 → Task 1/4/5/6/8。
- 类型一致性：`AuthGuard::verify(path, authorization) -> Result<Option<Value>, String>` 在 Task 3/4/5 一致；`JwtCfg` 字段 Task 1 定义、Task 6 消费；vtable `verify(RString, RString) -> RResult<RString, RString>` Task 2 定义、Task 3/4 一致。
- 已知风险：① Task 5 与 Task 6 之间 workspace 不可编译（oj 引用已删的 server::auth）——执行时按序连续完成，或 Task 5/6 合并提交；② `stabby` RString 的 `&str` 取值 API 以 `plugin_loader.rs` 现有用法为准；③ e2e.rs 设 `cfg.auth = None` 不受影响。
