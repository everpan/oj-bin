# Mail SMTP（lettre）插件 Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: 用 superpowers:executing-plans 逐任务执行本计划。每阶段结束更新任务状态（TaskUpdate）并写「阶段小结」。

**Goal:** 以 cdylib 插件 `oj-mail` 新增 `mail` 轴，向 JS 提供 `Mail`/`mail` 全局，支持多 profile 的同步/异步 SMTP 发送、队列线程池与双通道反馈。

**Architecture:** 宿主（核心 `src/bridge/`）负责配置装配、入参校验、附件字节解析、bus 发布、结果存储与 JS 全局挂载；插件（`plugins/oj-mail`）持 `lettre`、连接池、有界队列 + worker 池并实际投递；二者经 `oj-plugin-ffi` 的 `MailVtable`（repr(C) + `FfiFuture`）契约通信。

**Tech Stack:** Rust 2024 · deno_core `#[op2]` · `oj-plugin-ffi`（stabby repr(C) + `FfiFuture`）· `lettre`（rustls/tokio1）· `rustls = "=0.23.40"` + aws-lc-rs · tokio。

**设计依据:** `docs/plans/2026-09-15-mail-smtp-design.md`（v3）。

---

## 全局约定（每个任务都遵守）

- **TDD 循环**：先写失败测试 → 跑测试确认失败（记下报错） → 写最小实现 → 跑测试确认通过 → 提交。
- **命令**（项目禁 debug）：
  - 单测：`cargo test --release -p <crate> <filter>`
  - 门禁：`cargo fmt --check` + `cargo clippy --release --all-targets -- -D warnings`
  - 插件：`cargo xtask plugin mail` / `cargo xtask plugin mail --check`
- **SOLID 落地**：`MailVtable`（接口）与 `oj-mail`（实现）分离；宿主 `MailBackend` trait 隔离 FFI 细节（依赖倒置）；每个 profile 一个 `MailProfile`（单一职责）；校验/附件解析/发送/存储各自独立函数（可组合）。
- **每阶段收尾**：跑本阶段全部测试 + `fmt`/`clippy`；`TaskUpdate` 标记完成；在计划文件末尾追加「阶段小结」（改了什么、测试结果、遗留）。
- **提交粒度**：每任务一次 `git commit`（中文信息，`type(scope): …`）。

---

## 阶段 0：准备与基线（先验编译，防返工）

### Task 0.1：验证 `lettre` + `rustls 0.23.40` + aws-lc-rs 兼容（spike）

**Files:**
- Modify: `plugins/oj-mail/Cargo.toml`（临时最小 crate，或先建后删的 `tools/spike-mail`）

**Step 1: 建最小 crate 并加依赖**

```toml
[dependencies]
lettre = { version = "0.11", default-features = false, features = ["builder", "smtp-transport", "tokio1", "tokio1-rustls-tls", "rustls-tls", "hostname", "pool"] }
rustls = "=0.23.40"
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

**Step 2: 写最小发送代码并编译**

```rust
// 仅编译期验证：不真发信
fn _spike() {
    let _t = lettre::AsyncSmtpTransport::<lettre::Tokio1Executor>::relay("smtp.example.com")
        .unwrap()
        .build();
}
```

Run: `cargo build --release -p <spike-crate>`
Expected: 编译通过。**若报 `rustls` 版本冲突 / `CryptoProvider` 相关错误** → 用 `cargo tree -i rustls` 核对，必要时 `cargo update -p rustls --precise 0.23.40` 收敛；若 provider 冲突（ring vs aws-lc-rs），确认 lettre 走 aws-lc-rs。

**Step 3: 记录结论到本计划「阶段 0 小结」并提交**

```bash
git add -A && git commit -m "chore(mail): spike 验证 lettre+rustls0.23.40 兼容"
```

### Task 0.2：建隔离 worktree 与基线

```bash
cargo test --release -p only-js --lib        # 基线绿
cargo test --release -p oj --lib             # 基线绿
```

**阶段 0 小结**（追加）：lettre 版本与 feature 定稿；rustls provider 结论；基线测试数。

---

## 阶段 1：FFI 契约（`oj-plugin-ffi`）

### Task 1.1：`MailAttachment` + `MailVtable` repr(C) 类型

**Files:**
- Create: `oj-plugin-ffi/src/mail.rs`
- Modify: `oj-plugin-ffi/src/lib.rs`（`pub mod mail;` + `pub use mail::{MailVtable, MailAttachment};`）

**Step 1: 写失败测试**（`oj-plugin-ffi/src/mail.rs` 底部 `#[cfg(test)]`）

```rust
#[test]
fn mail_attachment_roundtrips_bytes_without_base64() {
    let a = MailAttachment { filename: RString::from("a.pdf"), mime: RString::from("application/pdf"), bytes: RBytes::from(vec![0u8, 159, 255]) };
    assert_eq!(a.bytes.len(), 3);            // 字节原样，不编码
    assert_eq!(std::convert::Into::<String>::into(a.filename.clone()), "a.pdf");
}
```

Run: `cargo test --release -p oj-plugin-ffi mail_attachment`
Expected: FAIL（`MailAttachment` 未定义，编译失败）

**Step 2: 最小实现**

```rust
//! mail 轴 vtable（新增轴，ABI 不变——spec「加轴零破坏」）。
use crate::{FfiFuture, RBytes, RString, RVec};

/// 附件：宿主解析后的**原始字节**（非 base64），插件直接喂 lettre。
#[stabby::stabby]
#[repr(C)]
pub struct MailAttachment {
    pub filename: RString,
    pub mime: RString,
    pub bytes: RBytes,
}

/// mail 轴：`submit` 统一入口，行为由 req JSON 的 `sync`/`enqueue_only`/`raw` 决定。
/// ok 值 = 结果信封 JSON；`enqueue_only` 时 future 立即回 `{"jobId": "..."}`，
/// 真实完成经 `HostContext.deliver("mail.result", ...)` 上送。
#[stabby::stabby]
#[repr(C)]
pub struct MailVtable {
    pub submit: extern "C" fn(key: RString, req: RString, atts: RVec<MailAttachment>) -> FfiFuture,
}
```

**Step 3: 跑测试** → PASS。

**Step 4: 提交** `feat(ffi): 新增 mail 轴 vtable + MailAttachment`

### Task 1.2：`axis::mail` 类型配对 helper

**Files:** Modify: `oj-plugin-ffi/src/axis.rs`

**Step 1: 失败测试**（追加到现有 `helpers_bind_exact_vtable_types`）

```rust
let _: fn(&'static MailVtable) -> *const c_void = axis::mail;
```

Run: `cargo test --release -p oj-plugin-ffi helpers_bind_exact_vtable`
Expected: FAIL（`axis::mail` 不存在）

**Step 2: 实现**

```rust
pub fn mail(vt: &'static MailVtable) -> *const c_void { vt as *const _ as *const c_void }
```
并在 `use crate::{...}` 补 `MailVtable`。

**Step 3: 跑测试** → PASS。 **Step 4: 提交**

### Task 1.3：宿主 `AXES` / `probe_axes` / `Registrations` 加 `mail`（**不 bump ABI**）

**Files:** Modify: `src/bridge/plugin_loader.rs`

**Step 1: 失败测试**（`src/bridge/plugin_loader/tests.rs` 或同文件 tests）

```rust
#[test]
fn axes_includes_mail_and_probe_branch_is_wired() {
    assert!(AXES.contains(&"mail"));
    // Registrations 含 mail 字段（编译期即可断言）
    let r = Registrations::default();
    assert!(r.mail.is_none());
}
```

Run: `cargo test --release -p only-js axes_includes_mail`
Expected: FAIL

**Step 2: 实现**（三处同改，缺一即 panic/不可见）

```rust
pub const AXES: &[&str] = &["es", "db", "blob", "bus", "kv", "auth", "mq", "mail"];
// probe_axes match 增：
"mail" => r.mail = Some(unsafe { &*(vt as *const oj_plugin_ffi::MailVtable) }),
// Registrations 增字段：
pub mail: Option<&'static oj_plugin_ffi::MailVtable>,
```

**Step 3: 跑测试** → PASS；再跑 `cargo test --release -p only-js plugin`。

**Step 4: 确认 ABI 未变**

Run: `grep -n ABI_VERSION oj-plugin-ffi/src/lib.rs`
Expected: 仍为 `8`（新增轴零破坏，不 bump）。

**Step 5: 提交** `feat(plugin): 宿主 AXES/probe_axes/Registrations 支持 mail 轴（ABI 不变）`

**阶段 1 小结**：契约类型、helper、宿主轴表就绪；ABI 保持 8。

---

## 阶段 2：插件骨架 `oj-mail`

### Task 2.1：crate 骨架 + `oj_plugin_entry!` + descriptor

**Files:**
- Create: `plugins/oj-mail/Cargo.toml`
- Create: `plugins/oj-mail/src/lib.rs`
- Modify: `Cargo.toml`（`members` 增 `"plugins/oj-mail"`）

**Step 1: Cargo.toml**（镜像 `plugins/oj-kv-redis/Cargo.toml`）

```toml
[package]
name = "oj-mail"
version = "0.1.0"
edition = "2024"
description = "mail 轴：lettre SMTP 发送（队列线程池 + FfiFuture）"

[lib]
crate-type = ["cdylib"]

[dependencies]
oj-plugin-ffi = { path = "../../oj-plugin-ffi" }
# 方案 B（阶段 0 定稿）：不用 tokio1-rustls-tls/rustls-tls（会强拉 rustls/ring，与框架 aws-lc-rs 冲突）
lettre = { version = "0.11", default-features = false, features = ["builder","smtp-transport","tokio1","tokio1-rustls","rustls-no-provider","webpki-roots","aws-lc-rs","hostname","pool","file-transport"] }
rustls = "=0.23.40"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tokio = { version = "1", features = ["rt-multi-thread","sync","time"] }

[dev-dependencies]
tokio = { version = "1", features = ["rt-multi-thread","sync","time","macros"] }
```

**Step 2: 失败测试** — 插件侧无宿主测试框架，改用「符号存在 + 预检」：

Run: `cargo xtask plugin mail --check`
Expected: FAIL（插件未构建/缺符号）

**Step 3: 最小实现 lib.rs**

```rust
//! oj-mail：mail 轴 cdylib 插件（lettre SMTP）。宿主负责配置/校验/附件字节解析/bus；
//! 本插件负责连接池、有界队列 + worker 池、投递，经 FfiFuture/deliver 回传。
use oj_plugin_ffi::{MailVtable, PluginDescriptor, RResult, RString, FfiFuture, RVec, MailAttachment, HOST_FINGERPRINT};

fn init(_host: oj_plugin_ffi::RArc<oj_plugin_ffi::HostContext>, cfg: RString)
    -> RResult<PluginDescriptor, RString> {
    // 阶段 3 起在此解析 cfg 建 MailEngine
    let _ = cfg;
    Ok(PluginDescriptor {
        name: RString::from("oj-mail"),
        semver: RString::from(env!("CARGO_PKG_VERSION")),
        abi_version: oj_plugin_ffi::ABI_VERSION,
        fingerprint: RString::from(HOST_FINGERPRINT),
        desc: RString::from("mail 轴：lettre SMTP 发送（多 profile + 队列线程池）"),
    })
}

extern "C" fn submit(_key: RString, _req: RString, _atts: RVec<MailAttachment>) -> FfiFuture {
    oj_plugin_ffi::ready_err("oj-mail: submit not implemented")
}

static MAIL_VTABLE: MailVtable = MailVtable { submit };

oj_plugin_ffi::oj_plugin_entry!(init, mail => oj_plugin_ffi::axis::mail(&MAIL_VTABLE));
```

**Step 4: 跑预检** → `cargo xtask plugin mail --check` PASS（ABI 8 / 身份 / semver / 符号齐）。

**Step 5: 提交** `feat(mail): oj-mail 插件骨架（cdylib + mail 轴符号）`

**阶段 2 小结**：插件可构建、可预检、`mail` 轴符号可见。

---

## 阶段 3：配置解析与 transport 构建（插件）

### Task 3.1：`MailConfig` 解析 + `MailProfile` 构建（含 tls 三模式）

**Files:** Create: `plugins/oj-mail/src/config.rs`（`mod config;` 接入 lib.rs）

**Step 1: 失败测试**

```rust
#[test]
fn parses_profiles_and_tls_modes() {
    let cfg = r#"{"workers":2,"queue_capacity":8,"default":{"host":"h","port":465,"tls":"tls","mechanism":"login","user":"u","pass":"p","timeout":5}}"#;
    let c = MailConfig::parse(cfg).unwrap();
    assert_eq!(c.workers, 2);
    assert_eq!(c.profiles["default"].port, 465);
    assert_eq!(c.profiles["default"].tls, TlsMode::Tls);
    assert!(MailConfig::parse(r#"{"default":{"host":"h","port":25,"tls":"none"}}"#).is_err()); // none 未显式允许
}
```

Run: `cargo test --release -p oj-mail parses_profiles_and_tls_modes`
Expected: FAIL

**Step 2: 实现**

```rust
#[derive(Deserialize)] pub struct MailConfig { #[serde(default = "default_workers")] pub workers: usize, #[serde(default = "default_cap")] pub queue_capacity: usize, #[serde(flatten)] pub profiles: HashMap<String, ProfileCfg> }
#[derive(Deserialize, Clone, PartialEq)] #[serde(rename_all="lowercase")] pub enum TlsMode { Tls, Starttls, None }
#[derive(Deserialize, Clone)] pub struct ProfileCfg { pub host: String, pub port: u16, pub tls: TlsMode, #[serde(default)] pub allow_none_tls: bool, pub mechanism: Mechanism, pub user: Option<String>, pub pass: Option<String>, pub xoauth2: Option<XOAuth2Cfg>, #[serde(default = "default_timeout")] pub timeout: u64, #[serde(default)] pub file_transport: Option<String> }
```
`parse` 内：`tls == None && !allow_none_tls` → Err（fail-closed）。

**Step 3: 跑测试** → PASS。 **Step 4: 提交** `feat(mail): 配置解析与 tls 模式（none 需显式允许）`

### Task 3.2：transport 构建 + rustls provider install 时序

**Files:** Modify: `plugins/oj-mail/src/lib.rs`（`build_profiles`）

**Step 1: 失败测试**（用 FileTransport 免网络）

```rust
#[test]
fn builds_profiles_and_installs_rustls_provider() {
    init_provider(); // 幂等
    let p = build_profile(&demo_cfg_with_file_transport()).unwrap();
    assert!(p.async.is_some() || p.sync.is_some());
}
```

Run: `cargo test --release -p oj-mail builds_profiles_and_installs_rustls_provider`
Expected: FAIL

**Step 2: 实现**（init 内先 install，再建 transport）

> **阶段 0 实测硬约束**：`relay()` 会**立即**构建 ClientConfig，故 provider 安装**必须早于**任何 transport 构建（纯 `rustls-no-provider` 未装时先 `relay()` 直接 panic）。
> **`pool` 运行时约束**：lettre 的 `pool` 在 transport `Drop` 里 `tokio::spawn`，无 runtime 上下文 drop 即 abort → transport 的**创建/使用/销毁都必须在该插件自己的 tokio runtime 内**（用 `rt.enter()` 或 `rt.block_on` 构建）。单测不得在纯同步 `fn` 里裸建 transport。

```rust
fn init_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default(); // 幂等；须早于 relay()
}
fn build_profile(c: &ProfileCfg) -> Result<MailProfile, String> {
    // file_transport：AsyncFileTransport/SmtpTransport::builder_dangerous 指向目录（测试用）
    // tls=tls：AsyncSmtpTransport::relay(host)?.port(port).tls(Tls::Wrapper(...))
    // tls=starttls：Tls::Required(...)；none：Tls::None（仅 allow_none_tls）
    // credentials：login → Credentials::new(user, pass)；xoauth2 → Credentials::from_xoauth2
}
```

**Step 3: 跑测试** → PASS（验证不 panic「no provider」）。 **Step 4: 提交** `feat(mail): transport 构建 + rustls provider 时序`

**阶段 3 小结**：配置→profile 通路可用；provider 时序经测试钉死。

---

## 阶段 4：有界队列 + worker 池 + FfiFuture + 背压 + drain（插件）

### Task 4.1：`MailEngine`（有界 mpsc + N worker），`submit` 经 FfiFuture 回结果

**Files:** Create: `plugins/oj-mail/src/engine.rs`

**Step 1: 失败测试**

```rust
#[tokio::test(flavor="multi_thread", worker_threads=2)]
async fn submit_through_queue_resolves_with_envelope() {
    let eng = MailEngine::new(test_profile(), 2, 8);
    let out = eng.submit_blocking("default", req_json_ok(), vec![]).await.unwrap();
    assert_eq!(out["code"], 0);
}
```

Run: `cargo test --release -p oj-mail submit_through_queue`
Expected: FAIL

**Step 2: 实现**

```rust
pub struct MailEngine { tx: tokio::sync::mpsc::Sender<Job>, rt: tokio::runtime::Runtime }
// Job { key, req, atts, respond: Option<oneshot::Sender<Envelope>>, enqueue_only, job_id }
// new(): 建 multi_thread Runtime；起 workers 个循环 recv→build_message→timeout(send)→回传
// submit→ FfiFuture（spawn_ffi_future 包装：oneshot 收结果）；enqueue_only 立即回 jobId
```

**Step 3: 跑测试** → PASS。 **Step 4: 提交** `feat(mail): 有界队列 + worker 池（FfiFuture 回结果）`

### Task 4.2：背压（`try_send` 满即回 code:4）+ graceful drain

**Files:** Modify: `plugins/oj-mail/src/engine.rs`

**Step 1: 失败测试**

```rust
#[tokio::test(flavor="multi_thread")]
async fn full_queue_returns_code4_without_blocking() {
    let eng = MailEngine::new(slow_profile(), 1, 1);
    let _h1 = eng.enqueue_async("default", req_json_ok(), vec![]); // 占满
    let e = eng.submit_now("default", req_json_ok(), vec![]).unwrap_err();
    assert!(e.contains("\"code\":4") || e.contains("queue full"));
}

#[tokio::test(flavor="multi_thread")]
async fn shutdown_drains_inflight_then_stops() {
    let eng = MailEngine::new(test_profile(), 2, 8);
    eng.enqueue_async("default", req_json_ok(), vec![]);
    eng.shutdown(Duration::from_secs(5)); // 等在途完成，drop runtime
}
```

Run: `cargo test --release -p oj-mail full_queue_returns_code4` / `shutdown_drains`
Expected: FAIL

**Step 2: 实现**：`tx.try_send` 失败 → 返回 `{"code":4,"msg":"queue full"}`；`shutdown`：发停机信号 → 等待 tx 关闭且 worker join → drop runtime。

**Step 3: 跑测试** → PASS。 **Step 4: 提交** `feat(mail): 背压(try_send) 与 graceful drain`

**阶段 4 小结**：队列/线程池/背压/drain 经测试钉死。

---

## 阶段 5：消息组装 + 附件 + sendRaw 冲突头（插件）

### Task 5.1：`SendRequest` 反序列化 + multipart 组装（text/html/附件）

**Files:** Create: `plugins/oj-mail/src/message.rs`

**Step 1: 失败测试**

```rust
#[test]
fn builds_multipart_with_html_and_attachment_bytes() {
    let m = build_message(&req_with_html_and_att(), &[att("a.pdf","application/pdf",b"%PDF-1.4")]).unwrap();
    let s = String::from_utf8(m.formatted()).unwrap();
    assert!(s.contains("multipart/alternative") && s.contains("a.pdf"));
    assert!(s.contains("JVBERi0xLjQ") == false); // 附件是原始字节，非 base64 文本流
}
```

Run: `cargo test --release -p oj-mail builds_multipart`
Expected: FAIL

**Step 2: 实现**：`#[serde(rename="blobKey")]`；`Message::builder().from(addr?)...multipart(MultiPart::alternative().singlepart(text).singlepart(html))`；附件 `Attachment::new(filename).body(bytes, mime.parse().unwrap())`。

**Step 3: 跑测试** → PASS。 **Step 4: 提交** `feat(mail): SendRequest 反序列化与 multipart 组装`

### Task 5.2：`sendRaw` 剥离冲突头（防双收件人/spoof）

**Files:** Modify: `plugins/oj-mail/src/message.rs`

**Step 1: 失败测试**

```rust
#[test]
fn raw_strips_from_to_cc_bcc_subject_headers() {
    let raw = "From: evil@x\nTo: victim@x\nSubject: s\nX-Keep: 1\n\nbody";
    let m = build_raw(&envelope_from_to(), raw, &[]).unwrap();
    let s = String::from_utf8(m.formatted()).unwrap();
    assert!(s.contains("X-Keep: 1") && s.contains("body"));
    assert_eq!(s.matches("evil@x").count(), 0);           // 原文 From 被剥离
    assert_eq!(s.matches("victim@x").count(), 0);         // 原文 To 被剥离（防双收件人）
}
```

Run: `cargo test --release -p oj-mail raw_strips`
Expected: FAIL

**Step 2: 实现**：按行扫描 header 区（首个空行前），丢弃 `From/To/Cc/Bcc/Subject`（大小写不敏感），余下保留。

**Step 3: 跑测试** → PASS。 **Step 4: 提交** `feat(mail): sendRaw 剥离冲突头`

**阶段 5 小结**：消息组装与 raw 安全处理就绪。

---

## 阶段 6：宿主 ops + StableState + 校验 + 附件解析 + 结果存储 + JS 全局

### Task 6.1：`ensure_within` 提为 `pub(crate)`

**Files:** Modify: `src/bridge/module_loader.rs:367`

**Step 1: 失败测试** — 在 `src/bridge/mail.rs` 先写引用它的测试（见 6.2），或直接改可见性后靠 6.2 覆盖。
**Step 2: 实现**：`fn ensure_within` → `pub(crate) fn ensure_within`。
**Step 3:** `cargo build --release -p only-js` PASS。
**Step 4: 提交** `refactor(loader): ensure_within 提 pub(crate) 供 mail 附件钳制`

### Task 6.2：`StableState`/`Extras` 增 `mail` 字段 + `MailBackend` 包装 + 构造注入

**Files:** Modify: `src/bridge/mod.rs`

**Step 1: 失败测试**

```rust
#[test]
fn stable_state_exposes_mail_field() {
    let b = Bridge::with_dbs_and_loader_in_memory_with_mail(Some(fake_mail_backend()));
    // 或对 StableState 直接断言字段存在（编译期）
}
```

Run: `cargo test --release -p only-js stable_state_exposes_mail`
Expected: FAIL

**Step 2: 实现**：`pub struct StableState { …, pub mail: Option<Arc<dyn MailBackend>> }`；`Extras` 同；`with_dbs_and_loader` 增参或 `Extras.mail` 透传；`MailBackend` trait（`send/enqueue/result` 语义经 vtable）。

**Step 3: 跑测试** → PASS。 **Step 4: 提交** `feat(bridge): StableState/Extras 注入 mail 后端`

### Task 6.3：`src/bridge/mail.rs` ops + bootstrap 全局

**Files:** Create: `src/bridge/mail.rs`；Modify: `src/bridge/mod.rs`（`mod mail;` + extension ops 注册）、`src/bridge/bootstrap.js`

**Step 1: 失败测试**（宿主侧纯函数优先——SOLID 可测）

```rust
#[test]
fn strips_crlf_and_rejects_bad_address() {
    assert_eq!(sanitize_header("a\r\nBcc: x"), "aBcc: x");
    assert!(validate_addr("not-an-addr").is_err());
}
#[test]
fn whitelist_enforced() {
    let cfg = mail_cfg_with(allowed_from=["noreply@x.com"], allowed_recipients=["@x.com"]);
    assert!(check_whitelist("noreply@x.com", &["a@x.com"], &cfg).is_ok());
    assert!(check_whitelist("evil@y.com", &["a@x.com"], &cfg).is_err());
}
```

Run: `cargo test --release -p only-js crlf` / `whitelist_enforced`
Expected: FAIL

**Step 2: 实现**
- `#[op2(async)] fn op_mail_send(state, key, req_json) -> Result<serde_json::Value, JsErrorBox>`：取 `StableState.mail` → 校验 → 解析附件（`blobs.get(name)?.get(k).await` / `ensure_within`+`fs::read`）→ 造 `RVec<MailAttachment>` → `submit` → await。
- `op_mail_send_sync`（`req.sync=true`）、`op_mail_enqueue`、`op_mail_result`、`op_mail_send_raw`、`op_mail_profiles`。
- `MailResultStore`（`DashMap` + 限长/TTL）；`deliver("mail.result")` 路由：存 + 本地 bus 扇出。
- `bootstrap.js`：

```js
globalThis.Mail = class {
  constructor(key = "default") { this.key = key; }
  send(m)     { return op_mail_send(this.key, JSON.stringify(m)); }
  sendSync(m) { return op_mail_send_sync(this.key, JSON.stringify(m)); }
  enqueue(m)  { return op_mail_enqueue(this.key, JSON.stringify(m)); }
  result(id)  { return op_mail_result(this.key, id); }
  sendRaw(o)  { return op_mail_send_raw(this.key, JSON.stringify(o)); }
};
globalThis.mail = new Mail("default");
```

**Step 3: 跑测试** → PASS。 **Step 4: 提交** `feat(bridge): mail ops + Mail/mail 全局 + 校验/附件解析`

**阶段 6 小结**：宿主侧 ops、校验、附件解析、结果存储、JS 全局就绪。

---

## 阶段 7：装配与端到端

### Task 7.1：`server_cmd` 装配 mail 插件与后端注入

**Files:** Modify: `oj/src/server_cmd.rs`（`assemble_plugins` / `build_registries`）、`oj/src/app.rs`（`Extras.mail`）、`oj/src/build_cmd.rs`（内省 `Extras`）

**Step 1: 失败测试**

```rust
#[tokio::test]
async fn server_assembles_mail_backend_from_plugins() {
    // 用 sample 的 smtp.mock（file_transport）profile
}
```

Run: `cargo test --release -p oj server_assembles_mail`
Expected: FAIL

**Step 2: 实现**：`plugins:` 段透传 `smtp:` cfg 给 `oj-mail`；宿主另解析非密钥面为 `MailConfig`；把 `Registrations.mail` 包成 `Arc<dyn MailBackend>` 注入 `Extras.mail`。

**Step 3: 跑测试** → PASS。 **Step 4: 提交** `feat(server): 装配 mail 插件与后端`

### Task 7.2：FileTransport 端到端（`oj test` / `oj build` 冒烟）

**Files:** Create: `oj/tests/mail_e2e.rs`；Modify: `sample/config.yaml`（`smtp.mock` profile）

**Step 1: 失败测试**

```rust
#[tokio::test] async fn mail_send_writes_eml_and_returns_envelope() {
    // 起内省/测试 bridge，注入 smtp.mock(file_transport=tempdir)
    // await mail.send({from,to,subject,text}); 断言 tempdir 有 .eml 且含 Subject/To；信封 code==0
}
```

Run: `cargo test --release -p oj --test mail_e2e`
Expected: FAIL

**Step 2: 实现/接线**（并按 `job_id` 命名 .eml 防并发竞态）。
**Step 3: 跑测试** → PASS。 **Step 4: 提交** `test(mail): FileTransport 端到端`

### Task 7.3：xtask/CI 覆盖 + CHANGELIST + 文档

**Files:** Modify: `tools/xtask/src/main.rs`（插件列表含 mail）、`.github/workflows/plugin-matrix.yml`、`CHANGELIST.md`、`README.md`/`docs/devkit/api-manual.md`

**Step 1:** `cargo xtask plugin mail --check` PASS。
**Step 2:** `plugin-matrix.yml` 增 `oj-mail`。
**Step 3:** CHANGELIST 记 v0.1.20 特性；api-manual 补 `Mail`/`mail` 用法。
**Step 4: 提交** `docs(mail): CHANGELIST/api-manual/CI 覆盖`

**阶段 7 小结**：端到端可用；CI 与文档齐。

---

## 阶段 8：加固与验收

### Task 8.1：安全回归用例（注入/越权/脱敏/none TLS）

**Files:** Modify: `src/bridge/mail.rs`（tests）、`oj/tests/mail_e2e.rs`

用例：`subject`/`headers` 含 CRLF 被剥离；`to` 越出 `allowed_recipients` 拒（code:5）；`none` TLS 未显式允许拒；bus 反馈 payload 不含 `to/subject`；`{path}` 越界（`../`）拒。

### Task 8.2：门禁与跨平台

```bash
cargo fmt --check
cargo clippy --release --all-targets -- -D warnings
cargo test --release --workspace
cargo xtask smoke --bin bin/oj
```
Windows 路径复用既有 `strip_verbatim` 逻辑；确认无新增跨平台风险点。

### Task 8.3：终审

- 逐条对照 design v3 §10/§11 的安全项与本计划测试；
- 三专家评审遗留项（Med）逐条确认已处置或显式延期并记录。

**Step: 提交** `chore(mail): 安全回归与门禁验收`

**阶段 8 小结**：门禁全绿；安全项闭合；遗留延期清单。

---

## 阶段汇总表

| 阶段 | 交付 | 关键验收 |
|---|---|---|
| 0 | spike 结论 + 基线 | lettre+rustls0.23.40 编译通过 |
| 1 | FFI 契约 + 宿主轴表 | `AXES` 含 mail；ABI 仍为 8 |
| 2 | `oj-mail` 骨架 | `xtask plugin mail --check` 通过 |
| 3 | 配置 + transport | tls 三模式；provider 时序测试 |
| 4 | 队列/worker/背压/drain | 满队列 code:4；shutdown drain |
| 5 | 消息组装 + raw | multipart + 冲突头剥离 |
| 6 | 宿主 ops + 全局 | CRLF/白名单/附件解析测试 |
| 7 | 装配 + e2e + 文档 | FileTransport e2e 绿；CI 覆盖 |
| 8 | 加固 + 门禁 | workspace 全绿；安全项闭合 |

---

## 阶段小结（执行时逐条追加）

### 阶段 0 小结

**结论：`lettre` 与框架既有 `rustls = "=0.23.40"`（aws-lc-rs）编译+运行期均兼容，风险已排除。**
但**计划中原定的 feature 集需要收敛为方案 B**（见下），否则会新引入 `rustls/ring` 双 provider。

#### 1. 最终 lettre 版本与 feature 列表（方案 B，与计划原稿不同）

```toml
lettre = { version = "0.11", default-features = false, features = [
  "builder", "smtp-transport", "tokio1",
  "tokio1-rustls", "rustls-no-provider", "webpki-roots", "aws-lc-rs",
  "hostname", "pool", "file-transport"
] }
rustls = "=0.23.40"
```

- 实际解析版本：**lettre v0.11.23**（0.11 线最新），`rustls v0.23.40`，`tokio-rustls v0.26.4`。
- **为何偏离计划原稿**（原稿为 `tokio1-rustls-tls` + `rustls-tls`）：
  lettre 的 `rustls-tls = ["webpki-roots", "rustls", "ring"]` —— 它**强制启用 `rustls/ring`**
  （lettre Cargo.toml 的 `ring = ["rustls?/ring"]`）。而实测**框架今天的依赖图里 rustls
  只启用 `aws_lc_rs`**（`cargo tree -p only-js -e features -i rustls`：只有
  `rustls feature "aws_lc_rs"` / `"aws-lc-rs"` / `"prefer-post-quantum"`，**无 ring**）。
  照原稿落地 = 新引入第二个 provider，把 0 风险变成 1 个隐藏运行期 panic 面。
- 方案 B 与原稿**功能等价**（`rustls-tls` 的全部内容 = `webpki-roots` + `rustls` + `ring`，
  逐一替换为显式 `webpki-roots` + `rustls-no-provider` + 去掉 ring），并额外显式声明
  `aws-lc-rs`，使 `cargo test -p oj-mail`（不链接根 crate）也自洽。
- **对 `rustls` 无需额外版本处理**：`=0.23.40` 已由根 crate 满足，方案 B 未触发第二个
  rustls 版本，**无需** `cargo update -p rustls --precise 0.23.40`。

#### 2. `cargo tree` 核验结论（方案 B）

```
rustls v0.23.40
├── lettre v0.11.23
│   └── spike-mail
├── spike-mail
└── tokio-rustls v0.26.4
    └── lettre v0.11.23
```
- **单一 rustls 0.23.40**，无第二版本。
- `cargo tree -p spike-mail -i ring` → `nothing to print`，**ring 不在图中**。
- rustls 只启用 `aws-lc-rs`（无 `ring`）→ 与框架 provider 完全一致。

#### 3. provider 冲突与处置

- **编译期无冲突**：ring / aws-lc-rs 并存也只是各自编译；真正的问题是运行期
  `ClientConfig::builder()`（自动判定）在双 provider 且未装默认时会 panic。框架已在
  `src/bridge/mod.rs:342`（`ws_client_extensions`）显式 `install_default(aws_lc_rs)`。
- **lettre 侧语义（读 lettre 0.11.23 源码确认，非猜测）**：
  `src/rustls_crypto.rs::crypto_provider()` 先取 `CryptoProvider::get_default()`，
  取不到再按 **lettre 自己的 feature** 回落——有 `aws-lc-rs` 则 aws-lc-rs，
  `all(not(aws-lc-rs), ring)` 则 ring，两者皆无则 `expect` panic。
  → **原稿方案 A 下 lettre 在「未装默认 provider」时会静默回落 ring**，
  与「确认 lettre 走 aws-lc-rs」的预期不符；方案 B 显式 `aws-lc-rs`
  让回落与框架一致（且不再需要 ring）。
- **硬顺序约束（实测钉死）**：`AsyncSmtpTransport::relay(host)` 会**立即**构建
  ClientConfig（`Tls::Wrapper(TlsParameters::new_rustls(host))`），因此
  **provider 安装必须早于任何 transport 构建**。实测纯 `rustls-no-provider`
  （不带 lettre `aws-lc-rs`）时先 `relay()` 会 panic：
  `No rustls crypto provider configured. When using the rustls-no-provider feature, ...`
  → 阶段 3 Task 3.2 的「init 内先 install，再建 transport」是**必须**而非防御性建议。
  方案 B 带 `aws-lc-rs` 时 lettre 自带回落（不加 install 也能跑），
  但仍统一按「先 install」实现，保证与框架同实例。

#### 4. `pool` feature 的运行时约束（新增发现，影响阶段 3/4 设计）

lettre 的 `pool` 会在 `AsyncSmtpTransport` 的 `Drop` 里 `tokio::spawn` 回收任务。
在**无 tokio runtime 上下文**中 drop 该 transport 会 panic
（`there is no reactor running`，且 panic 发生在析构中 → 直接 abort）。
→ 插件侧 transport 的**创建/使用/销毁都必须在该插件自己的 tokio runtime 内**；
方案（阶段 4 的 `MailEngine` 自建 multi_thread Runtime）本就满足，
但**单测/示例不得在纯同步 `fn main` 或 `current_thread` 之外裸建 transport**。

#### 5. spike 结构与清理

- 采用**优先方案**：临时 `tools/spike-mail/`（`Cargo.toml` + `src/main.rs`），
  临时加入根 `Cargo.toml` 的 `members` 以纳入本仓库 `Cargo.lock`（结论可信度高于仓库外独立 crate）。
- `src/main.rs` 验证 4 项：① async/sync transport 编译期可构建；② `install_default` 幂等
  （首次 `Ok`、再次 `Err`）且不 panic；③ 安装后默认 provider **即 aws-lc-rs**
  （`assert_eq!` cipher_suites 数 + `std::ptr::eq` 首组 KX 实例）；④
  `TlsParameters::new` 走通 lettre 的 `builder_with_provider` 路径。
  另提供 `no-install` 参数对照观察回落 / fail-loud 行为。
  二进制实跑输出：`spike-mail：全部编译期/运行期验证通过`，退出码 0。
- 清理：已删除 `tools/spike-mail/`，`Cargo.toml` 的 `members` 与 `Cargo.lock` 均已
  `git checkout` 还原，`git status` 仅剩本计划文档一项改动。

#### 6. 基线测试

| 命令 | 结果 |
|---|---|
| `cargo test --release -p only-js --lib` | **326 passed; 0 failed** |
| `cargo test --release -p oj --lib` | **156 passed; 0 failed** |

#### 7. 遗留 / 需决策

1. ~~阶段 2 Cargo 改用方案 B~~ —— **已采纳（controller 决策）**：Task 2.1 的 Cargo feature 列表、
   阶段 3 Task 3.2 的 provider 顺序/`pool` 运行时约束、设计文档 §13 均已同步为方案 B。
2. `src/bridge/mod.rs:339-341` 的注释称「reqwest 系又启 ring」——实测根 crate 图中
   rustls **未启用 ring**（reqwest 0.13 走 `__rustls-aws-lc-rs`），该注释已过时。
   属阶段 3 范围（provider 注释订正），阶段 0 未改代码以免越界。
3. 观察（非本阶段引入、不阻塞）：既有测试夹具按设计以 **debug profile** 编译插件
   （`src/bridge/plugin_loader/tests.rs:10` 有注释、`oj/src/server_cmd.rs:1452` 等），
   因此 `target/debug` 已有约 **5.6G** 存量产物。

### 阶段 1 小结

**结论：`mail` 轴契约（vtable 类型 + 类型配对 helper + 宿主探测表）就绪，`ABI_VERSION` 保持 8。**

#### 1. 改了什么

| 文件 | 要点 |
|---|---|
| `oj-plugin-ffi/src/mail.rs`（新增） | `MailAttachment{filename: RString, mime: RString, bytes: RBytes}`、`MailVtable{submit: extern "C" fn(key, req, atts) -> FfiFuture}`（均 `#[stabby::stabby] #[repr(C)]`）。附件字节由宿主解析后**原样过线**，不经 JSON/base64；方法面演进走 req JSON 字段（同 mq 的 JSON dispatch 思路）。模块注释写明契约形态与 ABI 立场。 |
| `oj-plugin-ffi/src/lib.rs` | `pub mod mail;` + `pub use mail::{MailAttachment, MailVtable};`（按字母序插在 `kv`/`mq` 之间）。`ABI_VERSION` **未改**。 |
| `oj-plugin-ffi/src/axis.rs` | `pub fn mail(&'static MailVtable) -> *const c_void`；`use crate::{…, MailVtable}`；`helpers_bind_exact_vtable_types` 追加 `let _: fn(&'static MailVtable) -> *const c_void = axis::mail;` 编译期配对断言。复审期一并补上历史缺口 `axis::mq` 的同形断言。 |
| `src/bridge/plugin_loader.rs` | 三处同改：`AXES`（:432）追加 `"mail"`（末尾）；`probe_axes` 增 `"mail" => r.mail = Some(&*(vt as *const oj_plugin_ffi::MailVtable))`；`Registrations`（:104）增 `pub mail: Option<&'static oj_plugin_ffi::MailVtable>`。 |
| `src/bridge/plugin_loader/tests.rs` | 新增 `axes_and_registrations_wire_mail`（初版名 `axes_includes_mail_and_probe_branch_is_wired` **名不符实**——实际不覆盖 `probe_axes` 臂；复审期改名并在注释中说明该局限）。 |
| `tools/xtask/src/main.rs` | **复审修复**（见 §6）：`AXES` 的第 4 个消费点（`check()` 的汇总 match）此前漏改且含 `unreachable!` → 所有插件预检 panic；改为 `axis_present() -> Option<bool>` + 普通 `Err`，并加表驱动守护测试。 |

TDD 节奏：每个任务均先写测试并跑出编译失败（`MailAttachment`/`axis::mail`/`Registrations.mail` 未定义），再最小实现转绿。

#### 2. 跑过的测试与结果

| 命令 | 结果 |
|---|---|
| `cargo test --release -p oj-plugin-ffi mail_attachment` | **1 passed; 0 failed** |
| `cargo test --release -p oj-plugin-ffi` | **6 passed**（lib）+ 2（entry_good）+ 1（entry_panicky），0 failed |
| `cargo test --release -p only-js axes_and_registrations_wire_mail` | **1 passed; 0 failed** |
| `cargo test --release -p only-js plugin` | **36 passed; 0 failed**（既有探测/清单/扫描/适配器用例无回归） |
| `cargo test --release -p xtask` | **8 passed; 0 failed**（含复审新增的守护测试） |
| `cargo xtask plugin <n> --check`（8 个第一方插件） | 全部 **exit=0，零 panic**（修复前 `es --check` 在 `main.rs:250` panic） |
| `cargo fmt --check` | exit 0 |
| `cargo clippy --release -p oj-plugin-ffi -p only-js -p xtask --all-targets -- -D warnings` | exit 0，**0 warning / 0 error** |

#### 3. ABI 保持 8 的证据

- `grep -n ABI_VERSION oj-plugin-ffi/src/lib.rs` → `49:pub const ABI_VERSION: u32 = 8;`（未改；本阶段**未触碰**该行）。
- 未改动任何既有轴的 repr(C) vtable 形状：`mail.rs` 为纯新增文件，`lib.rs` 仅加模块与 re-export，`axis.rs` 仅加 helper 与断言。
- 宿主侧仅**追加**探测表项/分支/槽位（`Registrations` 是宿主内部结构，非 FFI 类型、不进 ABI）。
- 沿用 mq 轴先例与 CLAUDE.md 红线「加轴零破坏——既有轴 vtable 形状变更才需要 bump ABI」；存量插件零感知、零重编译（实测 8 个存量插件 ABI 8 预检全过）。

#### 4. 与计划/指令的偏差（均已按「不弱化断言」处置）

1. **测试构造式微调**：计划稿与任务书给的是 `RBytes::from(vec![0u8, 159, 255])`，stabby 未实现 `From<std::vec::Vec<T>>`（编译报 `the trait bound stabby::vec::Vec<u8>: From<std::vec::Vec<u8>> is not satisfied`）。改用 stabby 已实现的 `impl<T: Copy, Alloc: IAlloc + Default> From<&[T]> for stabby::vec::Vec<T, Alloc>`（`stabby-abi 72.1.16`，`src/alloc/vec.rs:552`）：`RBytes::from(&[0u8, 159, 255][..])`。**断言未动**（`bytes.len() == 3` + filename 回环）。
   > 订正：本小结初版称「`src/bridge/ffi.rs:493` 注释即此先例」——**引用错误**。该行注释实为「stabby 无 `From<&[u8]>`，逐元素 push」，与实测**语义相反**（`RBytes::from(&[u8][..])` 实测可编译）。见 §7 遗留 3。
2. **无既有单元测试需要同步**：仓库内无断言 `AXES` 数量/顺序的用例；`oj/src/server_cmd.rs` 的 `cfg_adapters_subset_of_probed_axes` 是**子集**断言，追加 `mail` 自然满足，未改动。
   但**漏判了一个非测试消费点**（`tools/xtask` 内联在 `check()` 里的汇总 match）——这正是本次复审抓到的回归，见 §6。

#### 5. `AXES` 全消费点清单（阶段 2 起加轴请逐点核对）

加一个轴需要同步的**全部**位置。前 4 项是功能性的（漏改会被守护测试或运行期捕获），第 5 项是陷阱：

| # | 位置 | 性质 | 漏改后果 |
|---|---|---|---|
| 1 | `src/bridge/plugin_loader.rs:432` `pub const AXES` | 定义（单一事实源） | 轴完全不可见 |
| 2 | `src/bridge/plugin_loader.rs:435-460` `probe_axes` 的 `match *axis` | 功能性 | :458 `unreachable!` → **装载期 panic** |
| 3 | `src/bridge/plugin_loader.rs:104` `Registrations` 槽位字段 | 功能性 | 编译失败（`probe_axes` 的赋值目标缺失） |
| 4 | `tools/xtask/src/main.rs:224` `axis_present()` 的 `match axis` | 功能性（预检展示） | 修复后 = 普通 `Err`；由 §6 守护测试红 |
| 5 | `oj/src/server_cmd.rs:469` `ADAPTER_AXES` | **`#[cfg(test)]` 对账清单，非功能注册表** | 不 panic、不报错（见下） |

- **第 5 项是陷阱**：`ADAPTER_AXES` 带 `#[cfg(test)]`，**只在测试里存在**，登记它本身不产生任何运行期效果；它唯一的作用是被 `cfg_adapters_subset_of_probed_axes` 用来断言 `ADAPTER_AXES ⊆ AXES`（**子集**方向）。所以新增轴**不必**改它，改了也没有功能变化。真正决定插件 cfg 的是 `plugin_cfg` 的 `match name`（`oj/src/server_cmd.rs:479`）。
- 另有 4 处**文档散文清单**（已因 `mq` 陈旧，同样缺 `mail`），属文档同步、非门禁：`CLAUDE.md:159`、`docs/dev-guide.md:620`、`docs/plugin-architecture.md:17`、`docs/plugin-development.md:16`。归阶段 7 统一订正（§7 遗留 2）。
- **两处 `unreachable!` 的处置**：`probe_axes` 的保留（有 `Registrations` 编译期兜底 + 装载期 fail-fast 语义，属有意设计）；`xtask` 的已换为 `Option`/`Err` + 守护测试。

#### 6. 复审修复记录（规格评审发现的回归）

**必修 1（回归）——`AXES` 第 4 个消费点未同步，xtask 预检对所有插件 panic。**

- 复现证据（修复前）：`cargo xtask plugin es --check` →
  `panicked at tools/xtask/src/main.rs:250:18: internal error: entered unreachable code: AXES 与 check 汇总分支不同步`。
- 修复：把「轴 → 是否提供」从 `check()` 内联 match（末尾 `unreachable!`）抽成
  `fn axis_present(axis: &str, r: &Registrations) -> Option<bool>`，未知轴返回 `None`；
  `check()` 据 `None` 给**普通 `Err`**（`AXES 与 axis_present 判定不同步，请同步：未知轴 '<a>'`）。
  即「漏改」的后果从**崩溃**降级为**可读报错**。
- 防复发：新增表驱动守护测试 `given_axes_table_when_judged_then_every_axis_has_a_branch`
  —— 遍历 `AXES` 断言每轴都有判定分支，并断言未知轴得 `None`（而非 panic）。
- **RED→GREEN 证据**：先在 `axis_present` 中**故意不加** `"mail"` 臂 →
  `cargo test --release -p xtask given_axes_table` **FAILED**：
  `panicked at tools/xtask/src/main.rs:507: axis mail 缺判定分支`（证明守护测试真能抓到漏改）；
  补 `"mail" => r.mail.is_some(),` 后 → **8 passed; 0 failed**。
- 端到端证据：`cargo xtask plugin <n> --check` 对 `es / db-mysql / db-postgres / blob-s3 /
  bus-kafka / bus-rabbitmq / kv-redis / auth` 8 个存量插件全部 `exit=0`，`provided axes` 正常输出
  （如 `bus-kafka -> [bus, mq]`、`es -> [es]`），**零 panic**。

**必修 2** —— `helpers_bind_exact_vtable_types` 补 `axis::mq`（见 §1 表）。

**必修 3** —— 本小结订正三处：① `RBytes::from(&[T][..])` 的依据改为 `stabby-abi` 的真实 impl
（原引 `src/bridge/ffi.rs:493` 语义相反，见 §4.1）；② 测试改名 `axes_and_registrations_wire_mail`；
③ `unreachable!` 行号由 `:456` 订正为实际 **`:458`**。

**必修 4** —— 见 §5 全消费点清单。

**顺带确认：`ADAPTER_AXES` 未登记 `mail` 的行为与判断**（结论，本阶段不实现）：

- **确认行为**：`ADAPTER_AXES` 是 `#[cfg(test)]` 的**测试专用对账清单**，「未登记 `mail`」本身
  **没有**运行期后果。实际行为由 `plugin_cfg`（`oj/src/server_cmd.rs:473-495`）决定：`name = "mail"`
  既不命中 `cfg.plugins` 透传分支，也不命中 `match name` 的 `"es"`/`"auth"` 臂 → 落 `_ => "{}"`，
  即 **mail 插件拿到空 cfg，`smtp:` 配置不生效**。你的描述准确。
- **判断：应当走「登记适配器」**（在 `plugin_cfg` 加 `"mail"` 臂读顶层 `smtp:` + `src/config.rs`
  加顶层 `smtp:` 段），**而不是**让用户写 `plugins:` 透传。依据（读代码后）：
  1. **设计文档就要求宿主读同一段**：`2026-09-15-mail-smtp-design.md` §4 把 `smtp:` 放**顶层**，并明确
     「宿主另解析 `smtp:` 的**非密钥面**（profile keys、`allowed_*`、`tls`/`allow_none_tls`、host/port）
     作前置校验用」——宿主必须**类型化**读到该段。走 `plugins:` 透传只有插件拿得到、宿主拿不到，
     前置校验无法落地。`es:`/`auth:` 走适配器臂正是同一原因。
  2. **`plugins:` 透传有副作用**：`assemble_plugins`（`oj/src/server_cmd.rs:602`）在
     `!cfg.plugins.is_empty()` 时切**严格清单模式**——只装配键列出的插件。拿 `plugins: { mail: … }`
     装配置，等于顺手把运维的插件装配模式切了：用户必须把所有要加载的插件都列进去，否则其余插件
     静默不装。这个耦合是 `es:`/`auth:` 顶层段刻意避免的。
  3. **只登记 `ADAPTER_AXES` 是假动作**：它 `#[cfg(test)]`、无功能。真正要改的是 `plugin_cfg` 的
     `match name` 臂（`"mail" => match &cfg.smtp { Some(s) => …非密钥面…, None => "{}" }`）
     + `src/config.rs` 的顶层 `smtp:` 段，**然后**把 `"mail"` 加进 `ADAPTER_AXES` 以让子集断言覆盖它。
  4. **归属**：装配期工作（`plugin_cfg`/`config.rs`），按计划表归**阶段 7**。阶段 2 骨架联调可临时用
     `plugins: { mail: … }`（注意会顺带进严格模式，须把其余待加载插件一并列出）。

#### 7. 遗留 / 需决策

1. **`probe_axes` 的 `"mail"` 臂尚无端到端证据**。新测试只锁「`AXES` 含 mail + `Registrations` 有槽位」；
   `unreachable!` 分支要真正被走过，需有插件导出 `oj_plugin_axis_mail` 符号——现实插件在阶段 2。
   建议阶段 2 仿 `mini-mq` 加 `mini-mail` 夹具（或用真 `oj-mail`）补一条 `probe_finds_mail_axis` 回归
   （新用例注释已注明此局限）。
2. **文档散文里的 `AXES` 清单已陈旧**（缺 `mq`，现又缺 `mail`）：`CLAUDE.md:159`、
   `docs/dev-guide.md:620`、`docs/plugin-architecture.md:17`、`docs/plugin-development.md:16`。
   非门禁、不影响运行，归阶段 7 文档任务统一订正（避免本阶段越界扩大文档漂移面）。
3. **`src/bridge/ffi.rs:493` 注释与实测不符**：`to_rbytes` 注释称「stabby 无 `From<&[u8]>`」，
   但 `RBytes::from(&[u8][..])` 实测可编译（`stabby-abi` 的 `From<&[T]>`）。函数实现本身无害
   （逐元素 push 等价），仅注释陈旧易误导。属既有代码，本阶段未改；可顺手订正。
4. 阶段 0 小结遗留项 2（`src/bridge/mod.rs:339-341` 关于 reqwest 启 ring 的过时注释）与
   3（测试夹具 debug profile 产物占用）**本阶段仍未处理**，仍按原归属。


### 阶段 2 小结
（待填）

### 阶段 3 小结
（待填）

### 阶段 4 小结
（待填）

### 阶段 5 小结
（待填）

### 阶段 6 小结
（待填）

### 阶段 7 小结
（待填）

### 阶段 8 小结
（待填）
