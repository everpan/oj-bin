# Certificate Design Implementation Plan

> **For agentic workers:** REQUIRED SUB‑SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task‑by‑task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在运行时对服务器进行证书校验，实现证书到期后仅限制 GET 请求并在宽限期结束后禁止服务启动。

**Architecture:** 在 `AppState` 中引入 `CertificateStatus`，在启动时加载并验证 JWS 证书，热加载文件变更，GET 请求前检查状态，Health 接口输出证书状态。

**Tech Stack:** Rust, axum, ring, notify, serde_json, chrono

## Global Constraints
- 证书格式必须为 JWS（Header·Payload·Signature），Header 固定 `{"alg":"RS256","typ":"JWT"}`。
- 宽限期默认 30 天，可通过 `server.grace_days` 配置。
- 公钥与证书文件路径可在 `config.yaml` 中配置，或通过 CLI `--cert-path`、`--key-path` 覆盖。
- 服务启动时若证书已在宽限期结束后失效，必须记录 `ERROR` 并 `process::exit(1)`。

---

### Task 1: Add CertificateStatus enum & fields to AppState
**Files:**
- Modify: `server/src/lib.rs`

**Interfaces:**
- Produces: `CertificateStatus` 枚举，`AppState.certificate_status`，`AppState.certificate_valid_until`

- [ ] **Step 1: Write the failing test**
```rust
#[tokio::test]
async fn test_appstate_has_certificate_fields() {
    let state = dummy_app_state(); // helper builds minimal AppState
    let _ = &state.certificate_status;
    let _ = &state.certificate_valid_until;
}
```
- [ ] **Step 2: Run test to verify it fails**
  `cargo test --test appstate_cert_fields_test::test_appstate_has_certificate_fields -v`
- [ ] **Step 3: Write minimal implementation**
```rust
#[derive(Clone, Debug)]
pub enum CertificateStatus {
    Valid,
    Grace { remaining_secs: u64 },
    Expired,
}

#[derive(Clone)]
pub struct AppState {
    // existing fields …
    pub certificate_status: CertificateStatus,
    pub certificate_valid_until: Option<std::time::SystemTime>,
}
```
- [ ] **Step 4: Run test to verify it passes**
  `cargo test --test appstate_cert_fields_test::test_appstate_has_certificate_fields -v`
- [ ] **Step 5: Commit**
```bash
git add server/src/lib.rs tests/appstate_cert_fields_test.rs
git commit -m "feat: add CertificateStatus enum and fields to AppState"
```

---

### Task 2: Implement certificate loading & verification function
**Files:**
- Create: `server/src/certificate.rs`

**Interfaces:**
- Consumes: `ServerCfg`（公钥路径、证书路径、grace_days）
- Produces: `fn load_certificate(cfg: &ServerCfg) -> Result<(CertificateStatus, Option<SystemTime>), String>`

- [ ] **Step 1: Write failing test**
```rust
#[tokio::test]
async fn test_load_certificate_valid() {
    let cfg = dummy_server_cfg_with_valid_cert();
    let res = certificate::load_certificate(&cfg).await;
    assert!(matches!(res, Ok((CertificateStatus::Valid, Some(_)))));
}
```
- [ ] **Step 2: Run test (fails)**
  `cargo test --test certificate_load_test::test_load_certificate_valid -v`
- [ ] **Step 3: Implement minimal loading**
```rust
use ring::signature;
use std::{fs, time::{SystemTime, UNIX_EPOCH, Duration}};
use serde_json::Value;

pub async fn load_certificate(cfg: &ServerCfg) -> Result<(CertificateStatus, Option<SystemTime>), String> {
    // 1. read public key PEM
    let key_data = fs::read(&cfg.public_key_path).map_err(|e| format!("read key: {}", e))?;
    let pub_key = load_verification_key(&key_data).map_err(|_| "invalid public key".to_string())?;

    // 2. read JWS certificate string
    let cert_str = fs::read_to_string(&cfg.certificate_path).map_err(|e| format!("read cert: {}", e))?;
    let parts: Vec<&str> = cert_str.trim().split('.').collect();
    if parts.len() != 3 { return Err("invalid JWS format".into()); }

    // 3. base64url decode
    let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(parts[0]).map_err(|e| e.to_string())?;
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(parts[1]).map_err(|e| e.to_string())?;
    let signature = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(parts[2]).map_err(|e| e.to_string())?;

    // 4. verify signature (RS256)
    let signing_input = format!("{}.{}}", parts[0], parts[1]);
    signature::verify(&signature::RSA_PKCS1_2048_8192_SHA256, &pub_key, signing_input.as_bytes(), &signature)
        .map_err(|_| "signature verification failed".to_string())?;

    // 5. parse payload JSON
    let payload_json: Value = serde_json::from_slice(&payload).map_err(|e| e.to_string())?;
    let nbf = payload_json["nbf"].as_u64().ok_or("missing nbf")?;
    let exp = payload_json["exp"].as_u64().ok_or("missing exp")?;

    // 6. determine status
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let grace_secs = cfg.grace_days.unwrap_or(30) as u64 * 86400;
    let status = if now < nbf {
        CertificateStatus::Valid
    } else if now < exp {
        CertificateStatus::Valid
    } else {
        let grace_end = exp + grace_secs;
        if now < grace_end {
            CertificateStatus::Grace { remaining_secs: grace_end - now }
        } else {
            CertificateStatus::Expired
        }
    };
    let expiry_time = UNIX_EPOCH + Duration::from_secs(exp);
    Ok((status, Some(expiry_time)))
}

fn load_verification_key(pem: &[u8]) -> Result<signature::UnparsedPublicKey<Vec<u8>>, ring::error::Unspecified> {
    // strip PEM header/footer, base64 decode to DER bytes
    let pem_str = std::str::from_utf8(pem).map_err(|_| ring::error::Unspecified)?;
    let der_b64: String = pem_str.lines()
        .filter(|l| !l.starts_with("-----"))
        .collect();
    let der = base64::engine::general_purpose::STANDARD.decode(der_b64).map_err(|_| ring::error::Unspecified)?;
    Ok(signature::UnparsedPublicKey::new(&signature::RSA_PKCS1_2048_8192_SHA256, der))
}
```
- [ ] **Step 4: Run test (passes)**
  `cargo test --test certificate_load_test::test_load_certificate_valid -v`
- [ ] **Step 5: Commit**
```bash
git add server/src/certificate.rs tests/certificate_load_test.rs
git commit -m "feat: implement certificate loading & verification"
```

---

### Task 3: Integrate certificate loading into server startup
**Files:**
- Modify: `server/src/lib.rs` (inside `app` function)
- Modify: `server/src/server_cmd.rs` (run/start entry)

**Interfaces:**
- Consumes: `certificate::load_certificate`
- Produces: `AppState` 已填充 `certificate_status` 与 `certificate_valid_until`

- [ ] **Step 1: Write failing test**
```rust
#[tokio::test]
async fn test_start_fails_when_cert_expired_and_grace_over() {
    let cfg = dummy_cfg_expired_past_grace();
    let result = server_cmd::run(ServerArgs { config: cfg.path.clone(), ..Default::default() }).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("certificate expired"));
}
```
- [ ] **Step 2: Run test (fails)**
  `cargo test --test start_cert_expired_test::test_start_fails_when_cert_expired_and_grace_over -v`
- [ ] **Step 3: Implement integration**
  - In `app` (or `run`) call `certificate::load_certificate(&cfg.server).await?`.
  - Abort with error if returned `CertificateStatus::Expired`.
  - Populate `AppState` fields accordingly.
- [ ] **Step 4: Run test (passes)**
  `cargo test --test start_cert_expired_test::test_start_fails_when_cert_expired_and_grace_over -v`
- [ ] **Step 5: Commit**
```bash
git add server/src/lib.rs server/src/server_cmd.rs tests/start_cert_expired_test.rs
git commit -m "feat: load certificate at startup and abort on full expiry"
```

---

### Task 4: Add GET request restriction logic
**Files:**
- Modify: `server/src/lib.rs` (inside `handle` before auth/blobs)

**Interfaces:**
- Consumes: `AppState.certificate_status`

- [ ] **Step 1: Write failing test**
```rust
#[tokio::test]
async fn test_get_blocked_when_cert_expired() {
    let mut state = dummy_app_state();
    state.certificate_status = CertificateStatus::Expired;
    let resp = handle(State(state), Method::GET, Uri::from_static("/v1/api/foo"), HeaderMap::new(), Bytes::new()).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}
```
- [ ] **Step 2: Run test (fails)**
  `cargo test --test get_block_test::test_get_blocked_when_cert_expired -v`
- [ ] **Step 3: Implement check**
```rust
if verb == "GET" {
    match st.certificate_status {
        CertificateStatus::Valid => {}
        CertificateStatus::Grace { remaining_secs } => {
            let days = remaining_secs / 86_400;
            return fail_response(403, &format!("Certificate expired, grace period: {} days remaining", days));
        }
        CertificateStatus::Expired => {
            return fail_response(403, "Certificate expired, service unavailable");
        }
    }
}
```
- [ ] **Step 4: Run test (passes)**
  `cargo test --test get_block_test::test_get_blocked_when_cert_expired -v`
- [ ] **Step 5: Commit**
```bash
git add server/src/lib.rs tests/get_block_test.rs
git commit -m "feat: enforce GET restriction based on certificate status"
```

---

### Task 5: Implement hot‑load watcher
**Files:**
- Create: `server/src/certificate_watcher.rs`

**Interfaces:**
- Consumes: `Arc<RwLock<AppState>>`, `ServerCfg`
- Produces: 更新 `AppState.certificate_status` 与 `certificate_valid_until`

- [ ] **Step 1: Write failing test** (integration style using temp files & watcher)
```rust
#[tokio::test]
async fn test_hot_reload_updates_status() {
    // 1. create temp config with valid cert
    // 2. start watcher in background
    // 3. replace cert file with expired cert
    // 4. wait a bit, then send a GET request – expect 403
}
```
- [ ] **Step 2: Run test (fails)**
- [ ] **Step 3: Implement watcher**
```rust
use notify::{Watcher, RecommendedWatcher, RecursiveMode};
use std::sync::{Arc, RwLock};

pub async fn spawn_watcher(state: Arc<RwLock<AppState>>, cfg: ServerCfg) -> notify::Result<()> {
    let mut watcher: RecommendedWatcher = Watcher::new_immediate(move |res| {
        if let Ok(event) = res {
            if event.paths.iter().any(|p| p == &cfg.certificate_path || p == &cfg.public_key_path) {
                match certificate::load_certificate(&cfg) {
                    Ok((status, valid)) => {
                        let mut w = state.write().unwrap();
                        w.certificate_status = status;
                        w.certificate_valid_until = valid;
                    }
                    Err(e) => log::warn!("certificate reload failed: {}", e),
                }
            }
        }
    })?;
    watcher.watch(Path::new(&cfg.certificate_path), RecursiveMode::NonRecursive)?;
    watcher.watch(Path::new(&cfg.public_key_path), RecursiveMode::NonRecursive)?;
    Ok(())
}
```
- [ ] **Step 4: Run test (passes)**
- [ ] **Step 5: Commit**
```bash
git add server/src/certificate_watcher.rs tests/hot_reload_test.rs
git commit -m "feat: hot‑load certificate and key files via notify"
```

---

### Task 6: Extend health endpoint with certificate status
**Files:**
- Modify: `server/src/lib.rs` (add `/health` route handler)

**Interfaces:**
- Consumes: `AppState.certificate_status`, `certificate_valid_until`

- [ ] **Step 1: Write failing test**
```rust
#[tokio::test]
async fn test_health_includes_cert_status() {
    let mut state = dummy_app_state();
    state.certificate_status = CertificateStatus::Grace { remaining_secs: 86400 };
    let resp = health_handler(State(state)).await;
    let json: serde_json::Value = serde_json::from_slice(resp.body()).unwrap();
    assert_eq!(json["certificate_status"], "grace");
}
```
- [ ] **Step 2: Run test (fails)**
- [ ] **Step 3: Implement handler**
```rust
async fn health(State(st): State<AppState>) -> Response {
    let status_str = match st.certificate_status {
        CertificateStatus::Valid => "valid",
        CertificateStatus::Grace { .. } => "grace",
        CertificateStatus::Expired => "expired",
    };
    let expiry = st.certificate_valid_until
        .map(|t| chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339())
        .unwrap_or_default();
    let body = json!({
        "status": "OK",
        "certificate_status": status_str,
        "certificate_expiry": expiry,
    });
    Response::new(axum::body::Body::from(serde_json::to_vec(&body).unwrap()))
}
```
- [ ] **Step 4: Run test (passes)**
- [ ] **Step 5: Commit**
```bash
git add server/src/lib.rs tests/health_cert_test.rs
git commit -m "feat: expose certificate status via health endpoint"
```

---

### Task 7: Update configuration structs & CLI parsing
**Files:**
- Modify: `only_js/config.rs` (add `public_key_path`, `certificate_path`, `grace_days` to `ServerCfg`)
- Modify: `oj/src/args.rs` (add CLI flags `--cert-path`, `--key-path`, `--grace-days`)

**Interfaces:**
- Consumes: 用户提供的路径/天数
- Produces: `ServerCfg` 包含新字段

- [ ] **Step 1: Write failing test**
```rust
#[test]
fn test_config_parses_cert_paths() {
    let yaml = r#"server:
      public_key_path: ./config/pub.pem
      certificate_path: ./config/cert.jws
      grace_days: 45
"#;
    let cfg = Config::from_str(yaml).unwrap();
    assert_eq!(cfg.server.public_key_path.unwrap(), "./config/pub.pem");
    assert_eq!(cfg.server.certificate_path.unwrap(), "./config/cert.jws");
    assert_eq!(cfg.server.grace_days.unwrap(), 45);
}
```
- [ ] **Step 2: Run test (fails)**
  `cargo test --test config_cert_test::test_config_parses_cert_paths -v`
- [ ] **Step 3: Extend structs & arg parsing**
  - Add `pub public_key_path: Option<String>` etc. to `ServerCfg`.
  - In `Args::parse()` add `#[clap(long)] cert_path: Option<String>` …
  - Merge CLI values into config (override if Some).
- [ ] **Step 4: Run test (passes)**
  `cargo test --test config_cert_test::test_config_parses_cert_paths -v`
- [ ] **Step 5: Commit**
```bash
git add only_js/config.rs oj/src/args.rs tests/config_cert_test.rs
git commit -m "feat: add certificate path & grace_day config + CLI flags"
```

---

### Task 8: Add logging for certificate events
**Files:**
- Modify: `server/src/lib.rs` (log at startup based on status)
- Modify: `server/src/certificate_watcher.rs` (log reload success/failure)

- [ ] **Step 1: Write test to capture logs** (optional, using `logtest` crate)
- [ ] **Step 2: Implement log statements**
```rust
log::info!("Certificate loaded, status: {:?}", cert_status);
log::warn!("Certificate reload failed: {}", err);
```
- [ ] **Step 3: Commit**
```bash
git add server/src/lib.rs server/src/certificate_watcher.rs
git commit -m "chore: log certificate load/reload events"
```

---

### Task 9: Documentation updates
**Files:**
- Edit: `docs/dev-manual.md`
- Edit: `README.md`

- Add a new section “Certificate based GET restriction” showing config example, CLI usage, and runtime behavior.

- [ ] **Step 1: Commit docs**
```bash
git add docs/dev-manual.md README.md
git commit -m "docs: add certificate usage guide and CLI examples"
```

---

### Task 10: Full integration test suite
**Files:**
- Create: `tests/integration_certificate.rs`

- Test flow:
  1. Start server with a **valid** cert → verify GET works.
  2. Replace cert file with an **expired** one → wait for watcher → verify GET returns 403.
  3. After grace period elapsed (simulate by setting `grace_days = 0` in config) → restart server → expect startup abort.

- [ ] **Step 1: Write test skeleton**
- [ ] **Step 2: Implement using temporary directories, `tokio::process::Command` to launch `cargo run --bin oj` with flags.
- [ ] **Step 3: Run test (fails), then implement missing pieces until passing.
- [ ] **Step 4: Commit**
```bash
git add tests/integration_certificate.rs
git commit -m "test: full integration covering cert load, hot‑reload, and GET block"
```

---

## Execution Hand‑off

Plan complete and saved to `docs/superpowers/plans/2026-08-26-certificate-design.md`. Two execution options:

1. **Sub‑agent‑Driven (recommended)** – I will dispatch a fresh sub‑agent per task, review between tasks, fast iteration.
2. **Inline Execution** – Execute tasks sequentially in this session using `executing-plans`.

*Which approach would you like?*