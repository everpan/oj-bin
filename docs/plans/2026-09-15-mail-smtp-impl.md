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
- **评审策略（controller 决定，2026-09-15）**：**所有阶段实施完成后统一审查**（代码评审 + 三方专家/安全统一评审），阶段间不再逐阶段跑 spec/quality 评审仪式；但每阶段仍必须自测（TDD + 门禁）并写阶段小结。
- **版本**：本特性纳入 **v0.1.19**（`oj/Cargo.toml` 版本号递增提交即发布点）。

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

**Step 2: 实现**（**配置路径已定稿**：走 `plugin_cfg` 适配器臂，**不用 `plugins:` 透传**）：
- `oj/src/config.rs`：增**顶层** `smtp:` 段（类型化）。
- `oj/src/server_cmd.rs` 的 `plugin_cfg`（约 :473-495）`match name` 增 `"mail"` 臂：把顶层 `smtp:` 序列化后作为 `oj-mail` 的 cfg；**不要**用 `plugins:` 透传（`assemble_plugins` 在 `plugins` 非空时切**严格清单模式**，会让用户被迫列全所有插件）。
- `ADAPTER_AXES`（`server_cmd.rs` 约 :469，`#[cfg(test)]` 对账清单）追 `"mail"`，使子集断言覆盖它。
- 宿主另解析 `smtp:` 非密钥面为 `MailConfig`（供校验与 profile 列举）；把 `Registrations.mail` 包成 `Arc<dyn MailBackend>` 注入 `Extras.mail`（`app.rs` / `build_cmd.rs` 内省同步）。

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

**Files:** Modify: `tools/xtask/src/main.rs`（`PLUGINS` 表增 `"mail"`）、`CHANGELIST.md`、`README.md`/`docs/devkit/api-manual.md`

**Step 1:** `cargo xtask plugin mail --check` PASS。
**Step 2:** **只改 `tools/xtask/src/main.rs:28` 的 `PLUGINS`**（CI matrix / 归置的单一真相源）。**不要改 `.github/workflows/plugin-matrix.yml`**——该文件不硬编码插件名（注释明写「单一真相源是 xtask 的 PLUGINS」，且记有「此前硬编码 7 个漏 `auth`」的历史教训，加插件名到 CI 属重蹈失步）。注意 `PLUGINS` 有守护测试断言 `len()==8`，须连带更新为 9。
**Step 3:** CHANGELIST 记 v0.1.19 特性；api-manual 补 `Mail`/`mail` 用法。
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
| `src/bridge/plugin_loader.rs` | 三处同改：`AXES`（:432）追加 `"mail"`（末尾）；`probe_axes` 增 `"mail" => r.mail = Some(&*(vt as *const oj_plugin_ffi::MailVtable))`；`Registrations`（:104）增 `pub mail: Option<&'static oj_plugin_ffi::MailVtable>`。质量评审 I-2 另加 `impl Registrations::provides`（:469，轴→槽位映射单一事实源）。 |
| `src/bridge/plugin_loader/tests.rs` | 测试演变：`axes_includes_mail_and_probe_branch_is_wired`（名不符实）→ `axes_and_registrations_wire_mail` → **质量评审 I-2 删掉其中的恒真断言**，改为 one-hot `given_only_mail_slot_set_when_provides_then_only_mail_is_true` + `given_axes_table_when_provides_then_every_axis_has_a_branch`；另加 I-1 的 `probe_finds_mail_axis_and_zero_axis_mini_misses_it`。 |
| `tests/plugins/mini-mail/`（新增） | 质量评审 I-1：单轴 mail cdylib 夹具（仿 `mini-mq`），导出 `oj_plugin_axis_mail`，供 `probe_axes` 的 `"mail"` 臂做行为级覆盖；已入 workspace `members`。 |
| `tools/xtask/src/main.rs` | **复审修复**（见 §6）：`AXES` 的第 4 个消费点（`check()` 的汇总 match）此前漏改且含 `unreachable!` → 所有插件预检 panic。先改为 `axis_present() -> Option<bool>` + 普通 `Err`；**质量评审 I-2 治本**：删掉本地映射，改调 `Registrations::provides`（不再跨 crate 复制映射）。 |
| `src/bridge/ffi.rs` | 质量评审 M-5：订正 `to_rbytes` 的陈旧注释（只改注释，实现未动）。 |

TDD 节奏：每个任务均先写测试并跑出编译失败（`MailAttachment`/`axis::mail`/`Registrations.mail` 未定义），再最小实现转绿。

#### 2. 跑过的测试与结果

| 命令 | 结果 |
|---|---|
| `cargo test --release -p oj-plugin-ffi mail_attachment` | **1 passed; 0 failed**（M-3 后测试名 `mail_attachment_holds_raw_bytes`） |
| `cargo test --release -p oj-plugin-ffi` | **6 passed**（lib）+ 2（entry_good）+ 1（entry_panicky），0 failed |
| `cargo test --release -p only-js probe_finds_mail_axis` | **1 passed; 0 failed**（I-1：mini-mail 夹具跑通 dlsym→转型→填槽） |
| `cargo test --release -p only-js provides` | **2 passed; 0 failed**（I-2 one-hot + 分支完整性） |
| `cargo test --release -p only-js plugin` | **36 passed; 0 failed**（既有探测/清单/扫描/适配器用例无回归） |
| `cargo test --release -p xtask` | **7 passed; 0 failed**（守护测试已迁至 plugin_loader 侧，故由 8 减为 7） |
| `cargo xtask plugin <n> --check`（8 个第一方插件） | 全部 **exit=0，零 panic**（修复前 `es --check` 在 `main.rs:250` panic） |
| `cargo fmt --check` | exit 0 |
| `cargo clippy --release -p oj-plugin-ffi -p only-js -p xtask --all-targets -- -D warnings` | exit 0，**0 warning / 0 error** |

#### 3. ABI 保持 8 的证据

- `grep -n ABI_VERSION oj-plugin-ffi/src/lib.rs` → `49:pub const ABI_VERSION: u32 = 8;`（未改；本阶段**未触碰**该行）。
- 未改动任何既有轴的 repr(C) vtable 形状：`mail.rs` 为纯新增文件，`lib.rs` 仅加模块与 re-export，`axis.rs` 仅加 helper 与断言。
- 宿主侧仅**追加**探测表项/分支/槽位（`Registrations` 是宿主内部结构，非 FFI 类型、不进 ABI）。
- 沿用 mq 轴先例与 CLAUDE.md 红线「加轴零破坏——既有轴 vtable 形状变更才需要 bump ABI」；存量插件零感知、零重编译（实测 8 个存量插件 ABI 8 预检全过）。

#### 4. 与计划/指令的偏差（均已按「不弱化断言」处置）

1. **测试构造式微调**：计划稿与任务书给的是 `RBytes::from(vec![0u8, 159, 255])`，stabby 未实现 `From<std::vec::Vec<T>>`（编译报 `the trait bound stabby::vec::Vec<u8>: From<std::vec::Vec<u8>> is not satisfied`）。改用 stabby 已实现的 `impl<T: Copy, Alloc: IAlloc + Default> From<&[T]> for stabby::vec::Vec<T, Alloc>`（`stabby-abi 72.1.16`，`src/alloc/vec.rs:552`）：`RBytes::from(&[0u8, 159, 255][..])`。断言本身**未被弱化**（质量评审 M-3 后进一步**加强**为逐位相等，见 §8）。
   > 订正：本小结初版称「`src/bridge/ffi.rs:493` 注释即此先例」——**引用错误**。该行注释实为「stabby 无 `From<&[u8]>`，逐元素 push」，与实测**语义相反**（`RBytes::from(&[u8][..])` 实测可编译）。该注释已由质量评审 M-5 订正（见 §8）。
2. **无既有单元测试需要同步**：仓库内无断言 `AXES` 数量/顺序的用例；`oj/src/server_cmd.rs` 的 `cfg_adapters_subset_of_probed_axes` 是**子集**断言，追加 `mail` 自然满足，未改动。
   但**漏判了一个非测试消费点**（`tools/xtask` 内联在 `check()` 里的汇总 match）——这正是本次复审抓到的回归，见 §6。

#### 5. `AXES` 全消费点清单（阶段 2 起加轴请逐点核对）

加一个轴需要同步的**全部**位置。前 4 项是功能性的（漏改会被守护测试或运行期捕获），第 5 项是陷阱：

| # | 位置 | 性质 | 漏改后果 |
|---|---|---|---|
| 1 | `src/bridge/plugin_loader.rs:432` `pub const AXES` | 定义（单一事实源） | 轴完全不可见 |
| 2 | `src/bridge/plugin_loader.rs:436-462` `probe_axes` 的 `match *axis` | 功能性（dlsym 转型填槽） | :458 `unreachable!` → **装载期 panic** |
| 3 | `src/bridge/plugin_loader.rs:104` `Registrations` 槽位字段 | 功能性 | 编译失败（`probe_axes` 的赋值目标缺失） |
| 4 | `src/bridge/plugin_loader.rs:469` `impl Registrations::provides` 的 `match axis` | 功能性（轴→槽位映射的**单一事实源**） | one-hot / 分支完整性守护测试红；`xtask --check` 给普通 `Err` |
| 5 | `oj/src/server_cmd.rs:469` `ADAPTER_AXES` | **`#[cfg(test)]` 对账清单，非功能注册表** | 不 panic、不报错（见下） |

- **第 4 项是 I-2 治本后的形态**：轴→槽位映射收归 `Registrations::provides`，与 `probe_axes`
  **同文件相邻**（`plugin_loader.rs:436` 与 `:469`），加轴时可直接对照改。`tools/xtask` 不再
  维护本地副本（曾各自维护一份 → 加轴漏改即回归），改为调用 `provides`。
- **第 5 项是陷阱**：`ADAPTER_AXES` 带 `#[cfg(test)]`，**只在测试里存在**，登记它本身不产生任何运行期效果；它唯一的作用是被 `cfg_adapters_subset_of_probed_axes` 用来断言 `ADAPTER_AXES ⊆ AXES`（**子集**方向）。所以新增轴**不必**改它，改了也没有功能变化。真正决定插件 cfg 的是 `plugin_cfg` 的 `match name`（`oj/src/server_cmd.rs:479`）。
- **文档散文清单共 6 处**（均已因 `mq` 陈旧，同样缺 `mail`），属文档同步、非门禁。
  权威枚举方式：`grep -rn '\bAXES\b' docs/ CLAUDE.md`。当前需阶段 7 订正的 6 处：

  | # | 位置 | 现状问题 |
  |---|---|---|
  | 1 | `CLAUDE.md:159` | `AXES = [es, db, blob, bus, kv, auth]`——缺 `mq`/`mail` |
  | 2 | `docs/dev-guide.md:620` | 同上 |
  | 3 | `docs/plugin-architecture.md:17` | 同上 |
  | 4 | `docs/plugin-development.md:16` | `AXES`（es/db/blob/bus/kv/auth）——缺 `mq`/`mail` |
  | 5 | `docs/modules/05-ffi-and-plugins.md:51` | `AXES = [...]` 同上，且带行号注释 `// :428` |
  | 6 | `docs/modules/00-overview.md:111` | 引用 `AXES`（`src/bridge/plugin_loader.rs:428`）——**行号已过期**（现 :432） |

  另有 `docs/superpowers/**`、`docs/plans/**`、`docs/review-2026-09-06.md` 中的历史记录性
  引用——属**史实**（记录当时的轴表），**不订正**。
- **两处 `unreachable!` 的处置**：`probe_axes` 的保留（有 `Registrations` 编译期兜底 + 装载期 fail-fast 语义，属有意设计）；`xtask` 的已随 I-2 直接消除（改调 `provides` 的 `Option`）。

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

1. ~~**`probe_axes` 的 `"mail"` 臂尚无端到端证据**~~ —— **已由质量评审 I-1 关闭**：
   新增 `tests/plugins/mini-mail` 夹具（导出 `oj_plugin_axis_mail`）+
   `probe_finds_mail_axis_and_zero_axis_mini_misses_it` 做行为级覆盖（见 §8）。
2. **文档散文里的 `AXES` 清单已陈旧**（缺 `mq`，现又缺 `mail`）：**共 6 处**，逐条见 §5 表。
   权威枚举：`grep -rn '\bAXES\b' docs/ CLAUDE.md`。非门禁、不影响运行，归阶段 7 文档任务
   统一订正（避免本阶段越界扩大文档漂移面）；`docs/superpowers/**` 等历史记录不订正。
3. ~~**`src/bridge/ffi.rs:493` 注释与实测不符**~~ —— **已由质量评审 M-5 订正**：注释改为
   说明逐元素 push 等价于 `RBytes::from(&bytes[..])`（`stabby-abi` 的 `From<&[T]>`），
   并注明早先「无 `From<&[u8]>`」的说法与实测相反。**只改注释**，`to_rbytes` 实现未动
   （避免动 blob 既有路径）。
4. 阶段 0 小结遗留项 2（`src/bridge/mod.rs:339-341` 关于 reqwest 启 ring 的过时注释）与
   3（测试夹具 debug profile 产物占用）**本阶段仍未处理**，仍按原归属。
5. **夹具测试的产物新鲜度依赖 mtime**（本阶段实测踩到一次）：`fixture_plugin_dir`
   （`src/bridge/plugin_loader/tests.rs:13`）按 `dst < src` 判断是否重拷。手工改夹具源码后
   若源文件 mtime 比 `target/debug` 产物旧（例如 `mv` 还原备份），cargo 不重编、拷贝逻辑也
   认为不旧 → 测试会用到**上一次**的旧 `.dylib` 并给出误导性失败。属既有测试基建行为，
   本阶段未改；此处记录以免下次误判为代码 bug（处置：`touch` 夹具源码或删
   `target/test-plugins-*`）。

#### 8. 质量评审修复记录（2026-09-15，第二轮）

按 controller 决策逐条落地。命令一律 `--release`，未 push。

**I-1（Important）——`probe_axes` 的 `"mail"` 臂补行为级覆盖。**
- 新增 `tests/plugins/mini-mail/`（`Cargo.toml` + `src/lib.rs`）：单轴 mail 夹具，
  `oj_plugin_entry!(init, mail => oj_plugin_ffi::axis::mail(&MAIL_VT))`，`submit` 用
  `ready_ok(b"{}")`；已加入根 `Cargo.toml` 的 workspace `members`（挨着 `mini-mq`）。
- `src/bridge/plugin_loader/tests.rs`：加 `mini_mail_plugin_dir()`（独立
  `test-plugins-mail` 目录，避免 scan 计数断言翻倍）与
  `probe_finds_mail_axis_and_zero_axis_mini_misses_it`（mini → `mail` None；
  mini-mail → `mail` Some 且 `abi_version == ABI_VERSION`）。
- **非空验证**：把夹具宏轴标识临时改成 `mailx`（符号变 `oj_plugin_axis_mailx`）→
  用例 **FAILED** `assertion failed: mmail.registrations.mail.is_some()`；还原后 GREEN。
  `nm` 确认夹具导出 `_oj_plugin_abi_version` / `_oj_plugin_axis_mail` / `_oj_plugin_init`。
  该用例端到端钉住 `probe_axes` 臂 + `axis::mail` helper + 符号名三者的配对。

**I-2（Important，治本）——轴映射迁至 `Registrations::provides` + one-hot 守护。**
- `src/bridge/plugin_loader.rs`：紧邻 `AXES`/`probe_axes` 加
  `impl Registrations { pub fn provides(&self, axis: &str) -> Option<bool> }`——轴→槽位映射
  的**单一事实源**；未知轴返回 `None`（不用 `unreachable!`，漏改后果是可读报错而非 panic）。
- `tools/xtask/src/main.rs`：删 `axis_present`，`check()` 改调 `p.registrations.provides(a)`，
  `None` → 普通 `Err`「AXES 与 provides 判定不同步，请同步：未知轴 '<a>'」；本地守护测试与
  `Registrations` import 一并删除（能力由 `plugin_loader` 侧承担，避免重复悬空）。
- `src/bridge/plugin_loader/tests.rs`：守护测试迁入并**升级为 one-hot**：
  - `given_only_mail_slot_set_when_provides_then_only_mail_is_true`——真实静态 `MailVtable`
    哨兵（`submit` 体 `unreachable!()`），只置 `mail` 槽，`for a in AXES` 断言
    `provides(a) == Some(a == "mail")`。**删除了恒真的
    `Registrations::default().mail.is_none()`**（原断言永远为真，钉不住任何东西）。
  - `given_axes_table_when_provides_then_every_axis_has_a_branch`——每轴都有分支 +
    未知轴得 `None`。
- **捕获能力实证**：把 `"mail" => self.mail.is_some()` 临时改成 `self.mq.is_some()` →
  one-hot 用例 **FAILED**「`axis mail 判定错配` left: `Some(false)` right: `Some(true)`」；
  而**较弱的**分支完整性用例同一 bug 下仍 **ok**（`provides("mail")` 返回 `Some(false)` 而非
  `None`）——证明 one-hot 严格更强，正是为抓这类 `is_some` 复制粘贴错而设。还原后两者皆 ok。

**M-1（必做）——`MailAxis` → `MailVtable` 全仓更名。** 对齐既有 7 轴 `*Vtable` 命名；
改 `oj-plugin-ffi/src/{mail,lib,axis}.rs`、`src/bridge/plugin_loader.rs` 与两份 plans 文档
（共 25 处）。轴标识仍是小写 `mail`，导出符号 `oj_plugin_axis_mail` **不变**，Rust 类型名
不进 ABI 字符串 → `ABI_VERSION` 保持 8。

**M-2（必做）——`submit` doc 补全。** 写明 `key` = `smtp` 配置里的 profile 名、**未知 key → Err**
（不回落 default），`req` 的 `sync` / `enqueue_only` / `raw` 语义，以及 `atts` 与 `raw` 互斥、
`atts` 是宿主解析好的原始字节。

**M-3（必做）——附件测试加强并改名。** `mail_attachment_roundtrips_bytes_without_base64` →
`mail_attachment_holds_raw_bytes`；断言改为 `assert_eq!(a.bytes.as_slice(), &[0u8, 159, 255][..])`
（逐位相等，比原 `len() == 3` 强），去掉多余的 `filename.clone()`（保留一句
`&a.filename[..] == "a.pdf"`）。

**M-4（必做）——`AXES` 文档散文清单补全。** 由 4 处更正为 **6 处**（补入
`docs/modules/05-ffi-and-plugins.md:51`、`docs/modules/00-overview.md:111`，后者另带**过期行号**
`:428`），并写明权威枚举方式 `grep -rn '\bAXES\b' docs/ CLAUDE.md`；见 §5。本阶段**只更新清单
文字**，未改这些散文（避免越界）。

**M-5（必做）——订正 `src/bridge/ffi.rs:493` 陈旧注释。** 只改注释，`to_rbytes` 实现未动。

复核结论：本阶段新增/修改的**每一处运行期功能行**均有测试——`probe_axes` 的 `"mail"` 臂由
I-1 夹具覆盖，`provides` 由 I-2 one-hot + 分支完整性覆盖。


### 阶段 2 小结

**结论：`oj-mail` 骨架落地——可构建、可预检、`mail` 轴符号可见；`ABI_VERSION` 保持 8。**
本阶段只落骨架：`submit` 一律显式报错（fail-loud），配置解析/transport/队列/投递在阶段 3-5 补齐。

#### 1. 改了什么

| 文件 | 要点 |
|---|---|
| `plugins/oj-mail/Cargo.toml`（新增） | `crate-type = ["cdylib"]`；依赖按**阶段 0 定稿的方案 B**：lettre `0.11` + `default-features = false` + `tokio1-rustls` / `rustls-no-provider` / `webpki-roots` / `aws-lc-rs` / `builder` / `smtp-transport` / `tokio1` / `hostname` / `pool` / `file-transport`，**不用** `rustls-tls` / `tokio1-rustls-tls`（经 lettre 的 `ring = ["rustls?/ring"]` 强拉第二 provider）；另 `rustls = "=0.23.40"` + serde/serde_json/tokio。feature 列表带注释写明弃用理由。 |
| `plugins/oj-mail/src/lib.rs`（新增） | `init` 返回 descriptor（身份 = `mail`，见 §5）；`submit` 恒 `ready_err("oj-mail: submit not implemented (阶段 2 骨架)")`；`static MAIL_VTABLE: MailVtable`；`oj_plugin_entry!(init, mail => oj_plugin_ffi::axis::mail(&MAIL_VTABLE))`——经 `axis::mail` helper 传 vtable，**类型错配编译期失败**（宏裸传无此检查）。附 2 个单测（§2 末）。 |
| `Cargo.toml`（根） | `members` 增 `"plugins/oj-mail"`（紧随 `"plugins/oj-auth"`）。`Cargo.lock` 同步（新增 lettre v0.11.23 / nom v8.0.0 / quoted_printable v0.5.2 等）。 |
| `docs/plans/2026-09-15-mail-smtp-impl.md` | 本小结。 |

未触碰 `oj-plugin-ffi/**`（`git diff --stat oj-plugin-ffi/` 空）——`ABI_VERSION` 与所有既有轴 repr(C) 形状零变更。

#### 2. 命令与结果（一律 `--release`）

| 命令 | 结果 |
|---|---|
| `cargo build --release -p oj-mail` | `Finished release profile`，exit 0（lettre v0.11.23 / tokio-rustls v0.26.4 解析成功） |
| `cargo tree -p oj-mail -i ring` | `warning: nothing to print` → **ring 不在依赖图**（方案 B 生效，与框架 aws-lc-rs 同源） |
| `cargo tree -p oj-mail -i rustls` | 单一 `rustls v0.23.40`（lettre + tokio-rustls + 本 crate 共用，无第二版本） |
| `nm -gU target/release/liboj_mail.dylib \| grep oj_plugin` | 三符号齐：`_oj_plugin_abi_version` / `_oj_plugin_axis_mail` / `_oj_plugin_init` |
| `cargo xtask plugin mail` | exit 0；`liboj_mail.dylib → bin/plugins/aarch64-apple-darwin/libmail.dylib` |
| `cargo xtask plugin mail --check` | **exit 0**（输出见 §3） |
| `cargo test --release -p oj-mail` | **2 passed; 0 failed** |
| `cargo clippy --release -p oj-mail --all-targets -- -D warnings` | exit 0，**0 warning / 0 error** |
| `cargo fmt --check` | exit 0 |

两个单测（任务书写"暂无测试可先占位"，此处补齐——本阶段唯一的运行期行为是「fail-loud + 身份」，两者都值得钉死）：

- `descriptor_name_is_plugin_name_not_crate_name`——identity / semver / abi / fingerprint 四项断言（正是 §5 那个规格笔误的守护）。
- `submit_fails_loud_until_implemented`——骨架期 `submit` 必须 `Err` 且信息可读（绝不静默回 Ok 让宿主以为投递成功）；阶段 3-5 随实现更新为真投递断言。

#### 3. `--check` 实际输出（GREEN 证据）

```text
$ cargo xtask plugin mail --check
ok: mail 0.1.0 (abi 8) — mail 轴：lettre SMTP 发送（多 profile + 连接池/队列线程池）
provided axes: [mail]
EXIT=0
```

含 **ABI=8**（`abi 8`）与 **provided axes: [mail]**（宿主 `AXES` 逐轴 dlsym 探测结果，经 `Registrations::provides` 汇总）。

#### 4. TDD RED→GREEN 记录

| # | RED 形态 | 实际报错 |
|---|---|---|
| 1 | crate 不存在 | `cargo build --release -p oj-mail` → `error: package ID specification 'oj-mail' did not match any packages` |
| 2 | 产物未归置 | `cargo xtask plugin mail --check` → `Error: "xtask error: precheck failed: plugin file missing: .../bin/plugins/aarch64-apple-darwin/libmail.dylib"`，exit 1 |
| 3 | **身份写错**（规格笔误） | 同上 → `Error: "xtask error: precheck failed: plugin identity mismatch: expected 'mail', got 'oj-mail'"`，exit 1；改为 `"mail"` 后 GREEN |
| 4 | 守护测试非空验证 | 临时把 descriptor 改回 `"oj-mail"` → `cargo test --release -p oj-mail descriptor_name` **FAILED**（`left: "oj-mail" / right: "mail"`）；还原后 GREEN |

#### 5. 与任务书/计划稿的偏差（1 处，已按实测修正）

**`descriptor.name` 必须是 `"mail"` 而非任务书模板写的 `"oj-mail"`。**

- 复现：照任务书模板写 `name: RString::from("oj-mail")` → `cargo xtask plugin mail --check` 报
  `plugin identity mismatch: expected 'mail', got 'oj-mail'`（exit 1），即**本阶段的硬验收无法通过**。
- 根因（读码确认，非猜测）：`PluginLoader::load_one` 以清单键做**严格相等**校验
  （`src/bridge/plugin_loader.rs:404` 的 `if name != entry.name.as_str()`）；清单键来自
  `cargo xtask plugin <name>` 的 `<name>`（此处 `mail`），落盘文件名同样由它派生
  （`ffi::plugin_file_name("mail")` = `libmail.dylib`）。二者是**插件名**，与 crate 名 `oj-mail` 无关。
- 约定核对（本次 `grep` 实测全 8 个既有插件，无一例外 = crate 名去 `oj-` 前缀）：
  `oj-es → "es"`、`oj-auth → "auth"`、`oj-db-mysql → "db-mysql"`、`oj-blob-s3 → "blob-s3"`、
  `oj-bus-kafka → "bus-kafka"`、`oj-bus-rabbitmq → "bus-rabbitmq"`、`oj-kv-redis → "kv-redis"`。
  插件名还是**功能性**的：`plugin_loader::bus_backend` 即按名去 `bus-` 前缀推断 broker kind。
- 处置：descriptor 改为 `"mail"`，并在源码注释写明「身份 = 插件名，不是 crate 名（`plugin_loader.rs:404`）」+
  守护测试 §2。**未弱化任何断言**，属修正规格笔误；计划稿 Task 2.1 Step 3 的同款模板同步按此口径理解。

另：任务书给 `ready_err` 的用法与 `oj-plugin-ffi/src/future.rs:117` 实际签名
（`fn ready_err(msg: impl Into<String>) -> FfiFuture`）一致，**无需调整**。

#### 6. 需登记位置的调查结论（本阶段只做调查，除已列者不动）

| 位置 | 性质 | 本阶段处置 / 依据 |
|---|---|---|
| `tools/xtask/src/main.rs:28` `PLUGINS`（8 项） | `cargo xtask build` 的插件清单——**CI 与 sample-tests job 的插件归置都走它** | **未登记**（归 Task 7.3）。`cargo xtask plugin mail` 不查该表，本阶段验收不受影响；且另有 `given_first_party_plugins_when_listed_then_covers_all_axes` 断言 `PLUGINS.len() == 8` 且注释为「8 个第一方插件 = es/db×2/blob/bus×2/kv/auth 全轴覆盖」，加 `mail` 须连带改该断言与注释 → 越出本阶段范围。 |
| `.github/workflows/plugin-matrix.yml` | CI 平台矩阵 | **无需改动**——**发现与计划稿 Task 7.3 的描述不符**：该文件**不含插件名清单**（注释明写「插件清单的单一真相来源是 `tools/xtask/src/main.rs` 的 PLUGINS——CI 不硬编码副本」，并记有「此前硬编码 7 个、漏 `auth`，与 xtask 失步」的历史教训）。故阶段 7 只需改 `PLUGINS`，**不应对本文件增插件名**（否则正是重蹈该文件已明令禁止的硬编码失步）。 |
| `oj/src/server_cmd.rs:479` `plugin_cfg` 的 `match name` | 插件 cfg 适配器（`mail` 现落 `_ => "{}"`） | **未登记**（归 Task 7.1）。 |
| `oj/src/server_cmd.rs:469` `ADAPTER_AXES` | `#[cfg(test)]` 对账清单，**非功能注册表** | 同上（阶段 1 小结 §5 已论证：登记它本身无运行期效果）。 |
| `oj/src/config.rs` 顶层 `smtp:` 段 | 宿主类型化读配置 | 归 Task 7.1。 |
| `plugins:` 段严格清单 | **不是白名单**：`assemble_plugins` 在 `cfg.plugins` 非空时切「严格清单模式」（只装配列出的插件），属运行期配置选择，无待登记的硬编码清单 | 无需登记；但阶段 2 骨架联调若临时用 `plugins: { mail: … }`，须注意会顺带进严格模式（阶段 1 小结 §6 已列）。 |
| `src/bridge/plugin_loader.rs`（`AXES` / `probe_axes` / `Registrations` / `provides`） | 轴表 4 个消费点 | **阶段 1 已完成**（含 `"mail"`），本阶段零改动。 |

#### 7. 遗留 / 转下阶段

1. **阶段 3 起实现 `submit`**：`init` 内先 `install_default(aws_lc_rs)` 再建 transport（阶段 0 实测硬约束：
   `relay()` 立即构建 ClientConfig），且 transport 的**创建/使用/销毁必须在本插件自己的 tokio runtime 内**
   （lettre `pool` 在 Drop 里 `tokio::spawn`，无 runtime 上下文即 abort）。
2. `PLUGINS`（xtask）与 `plugin_cfg` / `config.rs` 的 `mail` 登记归**阶段 7**（§6）。
3. **计划稿 Task 7.3 对 `plugin-matrix.yml` 的描述与实测不符**（§6），阶段 7 执行时按实测收敛
   （只改 `PLUGINS`，不给 CI 加插件名副本）。


### 阶段 3 小结

**结论：配置 → profile 通路可用（tls 三模式 + `none` fail-closed + xoauth2 静态令牌），
`build_profiles` 的 provider 时序与 `pool` 运行时约束均落实并有测试钉死；`submit` 仍显式报错（阶段 4）。**

#### 1. 改了什么

| 文件 | 要点 |
|---|---|
| `plugins/oj-mail/src/config.rs`（新增） | `MailConfig`（`workers`=4 / `queue_capacity`=256 / `#[serde(flatten)] profiles: HashMap<String, ProfileCfg>`——**顶层每个剩余键即 profile**，写错的键会因缺必填字段报错而非被忽略）、`TlsMode{tls,starttls,none}`、`Mechanism{login,xoauth2}`、`ProfileCfg`（host/port/tls/allow_none_tls/mechanism/user/pass/xoauth2/timeout=30/file_transport）、`XOAuth2Cfg`。`MailConfig::parse` + `ProfileCfg::validate`：`tls: "none"` 未显式 `allow_none_tls: true` → Err（文案点明开关）；`login` 只给半份凭据 → Err（半份凭据必是笔误）；`xoauth2` 缺 user/凭据块 → Err。 |
| `plugins/oj-mail/src/config.rs` | `XOAuth2Cfg::static_token()`：只给 `refresh_token`（或 `access_token` 为空串）→ `Err("xoauth2 刷新暂未支持，请提供 access_token")`（fail-loud，不静默降级为无凭据投递）。 |
| `plugins/oj-mail/src/lib.rs` | `install_crypto_provider()`（幂等，忽略 `Err(已装)`）；`build_profiles()`（**首语句**装 provider → 逐 profile 复校验 + 构建，错误串含 profile 名）；`AsyncMailTransport{Smtp,File}` / `SyncMailTransport{Smtp,File}` 形态枚举 + `MailProfile{async_transport,sync_transport}`（`Arc`）；`build_tls()`（tls→`Wrapper`、starttls→`Required`、none→`None`）；`credentials()`（login→`Credentials::new`+`vec![Login]`；xoauth2→静态 token + `vec![Xoauth2]`；两者皆缺=无认证中继，不下发认证机制）；`init` 改为**解析并校验配置**（坏配置 → `Err` 让装载失败）+ 存入 `OnceLock<MailConfig>` 供阶段 4。 |
| `src/bridge/mod.rs:339` | 阶段 0 遗留 2：订正过时注释（原文称「reqwest 系又启 ring」——实测全图 rustls 只启用 `aws_lc_rs`）。**仅注释**。 |

#### 2. 跑过的测试与结果

| 命令 | 结果 |
|---|---|
| `cargo test --release -p oj-mail builds_file_transport_profile_after_installing_provider` | **1 passed; 0 failed**（含真投递：写出 `.eml` 并读回校验原文；sync 路同验） |
| `cargo test --release -p oj-mail` | **22 passed; 0 failed**（`config::tests` 10 + `tests` 12） |
| `cargo clippy --release -p oj-mail --all-targets -- -D warnings` | exit 0，0 warning |
| `cargo fmt --check` | exit 0 |
| `cargo xtask plugin mail --check` | 见 §5（ABI 8 / 身份 / 符号） |

TDD 节奏：Task 3.1 先落测试跑出 `E0599 no associated function 'parse'`（13 处），再实现转绿；
Task 3.2 先落测试跑出 `E0308`（SMTP/file 两路 `send_raw` 的 Ok 类型不同）/ `E0277`（`MailProfile`
未实现 `Debug`，改测试侧 helper `expect_build_err`），再实现转绿。

#### 3. provider 时序（硬约束 1）如何保证

- **结构性保证**：`build_profiles` 的第一条语句是 `install_crypto_provider()`，其后才可能触碰
  `build_tls` → `TlsParameters::new`（lettre 的 `relay`/`starttls_relay` 内部同样先调它；
  该调用**立即**构建 rustls `ClientConfig`）。代码位置单一，无需跨函数推断。
- **可观测证据**：新增 `provider_install_is_on_build_path_in_fresh_process`——**子进程**
  （`current_exe --exact <child>`，rustls 默认 provider 是进程级全局、**无卸载 API**，只有新进程
  才能观测起点）断言「起点无默认 provider → 调 `build_profiles` → 默认 provider 已装，且
  `kx_groups[0]` 与 `aws_lc_rs::default_provider()` **指针相等**（与框架 `ws_client_extensions`
  同源实例）」。该子进程机制经**变异验证**：把子进程断言写反 → 父用例即失败（证明 env 传递与
  真断言确实生效，不是空跑）。
- **诚实边界**：单进程内**无法**反证「install 晚于 transport 构建」——方案 B 下 lettre 自带
  aws-lc-rs 回落，漏装既不 panic 也仍能建 ClientConfig（rustls 也没提供「provider 未装」的可观测
  信号）。故此处钉的是「install 在构建路径上 + 落 aws-lc-rs 同源实例」，顺序由代码结构保证。
- 另一条硬约束 1 的行为面用例 `tls_relay_profile_builds_after_provider_install` 走隐式 TLS 全路径
  构建，**无 panic**、返回 `Smtp` 形态 profile。

#### 4. `pool` 运行时约束（硬约束 2）如何保证

- 所有触碰 SMTP transport 的用例一律 `#[tokio::test(flavor = "multi_thread")]`（池 Drop 要
  `tokio::spawn`）；file 通道无池，同样在 runtime 内跑，不与无 runtime `fn` 混。
- **`init` 刻意不建 transport**（与任务书「init 调 build_profiles」的最小差异，见 §5.3）：`init`
  是插件装载期的**同步**调用，未必处于 tokio runtime 上下文；若在此建 transport，`init` 返回时
  transport 立即被 Drop → 无 runtime 即 abort。故 `init` 只解析/校验/存配置，transport 的
  创建/使用/销毁统一归阶段 4 的 `MailEngine`（自建 multi_thread runtime，在其内调 `build_profiles`）。
- 子进程用例里显式 `Builder::new_multi_thread().enable_all()` + `rt.block_on(...)` + 在 runtime 内
  `drop(profiles)`，即生产侧调用形态的缩影。

#### 5. lettre 0.11.23 实际 API 与计划稿的偏差（均按实测落地，未弱化断言）

1. **`Credentials::from_xoauth2` 不存在**（任务书指定）。读 `lettre-0.11.23/src/transport/smtp/
   authentication.rs`：只有 `Credentials::new(username, secret)` + `From<(S,T)>`；XOAUTH2 的
   `Mechanism::Xoauth2::response` 直接用 `secret` 作 Bearer token。故实现为
   `Credentials::new(user, access_token)` **并且**显式
   `.authentication(vec![Mechanism::Xoauth2])`（否则 lettre 默认机制是 PLAIN+LOGIN，会拿 token
   当口令使）。语义与任务书一致（仅静态 token；只给 refresh_token → 明确 Err）。
2. **file transport 是两套独立类型**：`lettre::transport::file::FileTransport`（impl `Transport`，
   Ok=`String` id）与 `AsyncFileTransport<E: Executor>`（impl `AsyncTransport`）。二者**不是**同一
   泛型的两态，故 `MailProfile` 无法用单一 `Arc<SmtpTransport>` 字段同时承载，改为
   `AsyncMailTransport{Smtp,File}` / `SyncMailTransport{Smtp,File}` 枚举 + `send_raw` 派发
   （信封 = `lettre::address::Envelope`）。`SmtpTransport` 的 Ok 是 `Response`（多行应答）、
   file 的 Ok 是落盘 id，已在派发处归一为 `String`（SMTP=应答文本，file=id，即阶段 5 结果信封的素材）。
3. `init` 不建 transport（见 §4）——任务书原文允许「可先调用它但暂不持有引擎」，此处取**更保守**的
   一种：连建都不建，把构建留在 `MailEngine` 的 runtime 内。
4. 测试目录用 `std::env::temp_dir()/oj-mail-test-<pid>-<tag>`（任务书示例写死 `/tmp/oj-mail-eml`）：
   避免并发/重复跑互相覆盖，跑完 `remove_dir_all`，不污染 `sample/`。
5. 补了两条 fail-loud 校验（任务书未要求、但属「配置是系统边界」的正常校验）：`login` 半份凭据、
   `access_token` 空串。
6. `build_profiles` 会**再次**调用 `ProfileCfg::validate`（规则不写第二份）：`MailConfig` 的字段是
   `pub` 且实现 `Deserialize`，直接 `serde_json::from_str` 的配置会绕过 `parse`——用例
   `build_profiles_revalidates_plaintext_bypassing_parse` 钉死这条防线。

#### 6. 提交粒度偏差（有实证）

计划要求 Task 3.1 / Task 3.2 各一次提交。实测**不可行**：`config.rs` 的类型字段（`ProfileCfg` 的
`host`/`port`/`tls`/… ）在 `build_profiles` 消费它们之前**无人读取**——`mod config;` 是私有模块，
`pub` 字段不豁免 dead-code 分析。把「只落 `config.rs` + `mod config;`」的中间态跑门禁，实测
`cargo clippy --release -p oj-mail --all-targets -- -D warnings` 报 **10 条 dead-code 错误**
（`could not compile oj-mail (lib) due to 10 previous errors`），即中间提交**不绿**。
故本阶段落**一次代码提交**（配置解析与 transport 构建互为因果、本就是一个可编译单元）
\+ 一次文档提交（本小结），而非按任务书硬拆。

#### 7. 遗留 / 转下阶段

1. `submit` 仍是显式骨架错误（`not implemented`），阶段 4 接 `MailEngine`（有界队列 + worker），
   在自建 runtime 内调 `build_profiles`，并从 `MAIL_CFG` 取 `workers`/`queue_capacity`。
2. xoauth2 **刷新流程**（`refresh_token` → 换 token）未实现，当前 fail-loud；如需支持单列任务。
3. `file_transport` 的落盘 id 无 `.json` 信封（未启 lettre `file-transport-envelope` feature）；
   e2e（阶段 7）若需校验信封内容，届时再评估是否加 feature。
4. 阶段 7 的配置登记：`plugin_cfg`/`config.rs` 的 `mail` 段、`xtask PLUGINS` 见阶段 1 小结 §6。

### 阶段 4 小结

**结论：有界队列 + worker 池 + `FfiFuture` 回传 + 背压（`try_send` → `code:4`）+ graceful drain
已落地并有测试钉死（含 4 条变异验证）。** 引擎自建 multi_thread runtime：transport 的
构建/使用/销毁全在其上下文内（lettre `pool` 在 `Pool::new` 与 `Pool::drop` 两处都
`E::spawn`），而 runtime 的销毁用 `shutdown_background()`（tokio 禁止在 runtime 上下文里
drop runtime）。本期 `submit` 只走「原始 MIME」路（`raw`），消息组装归阶段 5。

#### 1. 改了什么

| 文件 | 要点 |
|---|---|
| `plugins/oj-mail/src/engine.rs`（新增） | `MailEngine`：**有界** `mpsc::channel(queue_capacity)` + `workers` 个 worker；`Job{key,req,atts,respond,enqueue_only,job_id,sync}`；`Req`（最小 req 视图：`sync`/`enqueue_only`(`enqueueOnly` 别名)/`raw`/`from`/`to[]`/`jobId`）；`DeliverSink`（结果上送抽象，生产转发 `HostContext.deliver`、测试注入收集器）；`MailTarget{timeout,send,send_sync}` + `SendFn`/`SyncSendFn`（依赖倒置的投递函数，测试可注闸门桩）；`submit`/`shutdown`/`dispose`/`Drop`；信封构造 `ok_envelope`/`fail_envelope`/`queue_full_envelope`（失败只回分类文案 + jobId）。 |
| `plugins/oj-mail/src/testutil.rs`（新增） | `#[cfg(test)]` 共享脚手架：`drive`（FfiFuture 轮询桥）、`temp_dir`（隔离临时目录）、`host`（空宿主）。原先 `lib.rs` 测试内的三份私有副本收敛到此处（`engine.rs` 用例复用同一套）。 |
| `plugins/oj-mail/src/lib.rs` | `MailProfile` 增 `timeout`；新增 `MailProfile::into_target()`（把两路 lettre transport 包成引擎的 `SendFn`/`SyncSendFn`，闭包持有 `Arc` ⇒ transport 的最终 Drop 落在目标表销毁处，必须在 rt 上下文内）；`MAIL_CFG` 换成进程级 `MAIL_ENGINE: OnceLock<MailEngine>`；`init` 改为**先校验后查幂等**（坏配置永远 fail-loud，与是否已装配无关）+ 建引擎（`MailEngine::new` 内先 `rt.enter()` 再 `build_profiles`）；`submit` 由骨架错误改为委派引擎（`catch_future` 收敛 panic；引擎未装配 → `ready_err`）；`build_profiles` 文档补「构建期也需 runtime 上下文」的实测证据；骨架期用例换成入口守卫用例。 |

#### 2. 跑过的测试与结果（一律 `--release`）

| 命令 | 结果 |
|---|---|
| `cargo test --release -p oj-mail`（实现前，RED） | **FAILED. 22 passed; 9 failed** —— 9 条阶段 4 用例全红（8 条 `引擎/阶段 4 未实现`，1 条 `missing_raw` 收到假成功的 `code:0`） |
| `cargo test --release -p oj-mail`（实现后，GREEN） | **33 passed; 0 failed**（阶段 1-3 的 22 + 阶段 4 的 11） |
| `cargo clippy --release -p oj-mail --all-targets -- -D warnings` | exit 0（0 warning） |
| `cargo fmt --check` | exit 0 |
| `cargo xtask plugin mail --check` | `ok: mail 0.1.0 (abi 8) — mail 轴：lettre SMTP 发送（多 profile + 连接池/队列线程池）` / `provided axes: [mail]`（**ABI 仍为 8**，本阶段未触碰 `oj-plugin-ffi`） |

阶段 4 的 11 条用例（`engine::tests` 10 + `tests` 1）：`submit_through_queue_resolves_with_envelope`
（真 file transport：信封 `messageId` 必须对应真落盘的 `.eml`）、`full_queue_returns_code4_without_blocking`、
`enqueue_delivers_completion_via_sink`（断言上送**不含**收件人/主题）、`shutdown_drains_inflight_then_stops`、
`unknown_key_returns_error`、`worker_reports_unknown_key_as_validation_envelope`（worker 兜底分支直调）、
`missing_raw_fails_loud_and_raw_excludes_atts`、`smtp_connection_refused_returns_network_code`
（真 SMTP 路：端口 1 无监听 → `code:1` + `msg:"投递失败"`）、
`dropping_engine_in_async_context_does_not_abort`、`shutdown_releases_pool_transports_inside_runtime_context`、
`shutdown_from_sync_context_releases_pool_transports_in_rt_context`。

**变异验证（4 条，证明断言真的在钉约束）**

| 变异 | 期望红的用例 | 实测 |
|---|---|---|
| `dispose` 去掉 `rt.enter()` | `shutdown_from_sync_context_releases_pool_transports_in_rt_context` | **FAILED**：`there is no reactor running`（lettre `executor.rs:117`，panic 在析构链上） |
| `dispose` 的 `rt.shutdown_background()` 换回裸 `drop(rt)` | `dropping_engine_in_async_context_does_not_abort` | **FAILED**：`Cannot drop a runtime in a context where blocking is not allowed`（tokio `blocking/shutdown.rs:51`） |
| 队列容量改 `1 << 20`（形同无界） | `full_queue_returns_code4_without_blocking` | **FAILED**（不再有 `Full`，future 悬挂至 30s drive 超时） |
| `shutdown` 跳过 drain 等待循环 | `shutdown_drains_inflight_then_stops` | **FAILED**（在途 job 未完成、无上送） |

#### 3. 关键设计落点（`文件:行`）

- 引擎结构与「同生共死」字段：`engine.rs:156`（`tx`/`targets`/`rt`/`exits`/`workers`；`Option`/`Mutex` 均为
  「drain 时能 take 出去销毁」「`Sync` 以进 `OnceLock`」两处约束服务）。
- 构建顺序（先 `rt.enter()` 再 `build_profiles`）：`engine.rs:181`。
- worker 起法与退出信号次序：`engine.rs:212`（`with_rt`）——worker 帧在 `worker_loop` 返回时释放
  `targets` 强引用，**之后**才发退出信号 ⇒ `shutdown` 收齐信号时 transport 只剩引擎那一份。
- worker 循环 / 单 job 处理 / 投递与信封：`engine.rs:418`（`worker_loop`，`Arc<tokio::sync::Mutex<Receiver>>`
  公平取件，锁只在 `recv().await` 期间持有）、`:434`（`run_job`：sync 路 → oneshot、enqueue 路 → `deliver`）、
  `:447`（`deliver_one`：`timeout` + sync/async 选路 + 唯一信封构造点）、`:499`（`deliver_input`）。
- 非阻塞入队与背压：`engine.rs:273`（`submit`）→ `:307`（`try_send`）→ `:316`（`Err(Full)` ⇒ 立即 `code:4` 信封）；
  `send` 语义的 await 点在 `spawn_ffi_future` 的引擎任务里，不占调用线程。
- drain / 销毁：`engine.rs:328`（`shutdown`：take-drop `tx` → std 通道收 `workers` 个退出信号（同步等待，
  不用 `block_on`）→ `dispose`）、`:365`（幂等 `dispose`）、`:407`（`dispose`：`rt.enter()` 下 drop targets
  → `rt.shutdown_background()`）、`:372`（`Drop`：同序但不等待在途）。
- 宿主接线：`lib.rs:42`（`MAIL_ENGINE`）、`:251`（`init`：先校验后幂等）、`:283`（`submit` 委派）、
  `:74`（`into_target`）、`:67`（`profile.timeout`）。

#### 4. 背压「不阻塞」如何证明

1. **实测（行为面）**：`full_queue_returns_code4_without_blocking` 用 0 许可 `Semaphore` 闸门把唯一 worker
   钉在 A 的投递上（收到「已开始投递」信号才继续，确定性，不靠 sleep），B 占满唯一槽位，然后只测第三个
   `submit` **调用本身**的耗时（`Instant` 包住调用）并断言 < 2s；闸门在断言之后才放行 ⇒ 若 `submit`
   有任何等待空闲槽位的行为，该断言必然超时（并会拖到 30s drive 超时）。
2. **结构性**：`MailVtable::submit` 是同步 `extern "C" fn`（签名上不可能 await），实现里唯一的入队点
   是 `try_send`（`:307`），满即走 `ready_ok(code:4)`（`:316`）——没有 `await send`、没有轮询、没有重试。
3. **变异（反向）**：容量改成 `1 << 20`（形同无界）后该用例失败 ⇒ 断言确实钉住「有界 → 满即 code 4」，
   而不是碰巧命中。

#### 5. drain「无 panic / 无 abort」如何证明

- `shutdown` 返回 `Ok(())` 的**语义** = 收齐了 `workers` 个退出信号（每个 worker 只在 `worker_loop` 返回后发），
  故 `Ok` 本身即「worker 全部已退出」的断言；`Err` 会带上「x/y 个 worker 未退出」。
- **在途必达**：`shutdown_drains_inflight_then_stops` 的投递函数 sleep 50ms 才成功，入队后**立刻** shutdown：
  返回 `Ok` 且 sink 随即能 `try_recv` 到该 job 的 `code:0` 信封（上送发生在 worker 退出之前）⇒ 在途 job
  完成而非被丢弃；随后 `submit` 必须 `Err`（停机不再入队）。
- **销毁无 panic**：drain 与 Drop 都走 `dispose`（`rt.enter()` 下释放 transport → `shutdown_background()`
  关闭 runtime）。两条约束各有独立变异证据（§2 表前两行）：去掉 `rt.enter()` 复现 lettre 的
  "there is no reactor running"；换回裸 `drop(rt)` 复现 tokio 的 "Cannot drop a runtime in a context where
  blocking is not allowed"。诚实边界：`shutdown_drains_inflight_then_stops` 自身用的是注入桩（无 lettre
  transport），故「pool transport 的销毁上下文」由上述两条专用用例（真 SMTP transport，构建期不连网）钉住。

#### 6. 与任务书/计划的偏差（均有实证）

1. **提交粒度**：任务书建议两次提交（队列 / 背压+drain）。实测**无绿色的中间态**：本阶段 clippy 首跑即报
   `fields exits and workers are never read` + `associated items with_targets and shutdown are never used`
   （`-D warnings` 失败）——即「只有队列、没有 drain」的中间态不绿；而背压（`try_send`）与队列容量本就是
   同一行 `mpsc::channel(cap)` 的两面。故落**一次引擎提交 + 一次小结提交**，并把注入口/`shutdown` 的
   dead-code 处置写进代码注释（`with_targets` 用 `#[cfg(test)]` 门控；`shutdown` 加
   `#[allow(dead_code)]` + 理由）。
2. **`submit` 的 req 处理取「最小但真实」**：不加组装桩。只走 vtable 契约里真实存在的 `raw` 路；
   缺 `raw` → `code:5` + 文案点明「阶段 5 未实现消息组装」，并按 vtable 契约校验「`raw` 与附件互斥」
   （`atts` 非空即报错）。即「缺什么就 fail-loud」，不做假成功。
3. **`sync: true` 一并落地**（计划 §6/§7 的「按 sync 选路」）：worker 内 `spawn_blocking` 调用同步
   transport；`MailTarget::async_only` 让测试桩的同步路**显式报错**（避免「以为发了 sync 实际没发」）。
   诚实边界：`timeout` 只停止等待，已在 blocking 池里开始的调用会跑完（无法取消）。
4. **上送形态**：按任务书取 `deliver("mail.result", <统一信封 JSON>)`，即
   `{code,msg,data:{jobId,messageId}}`。⇒ 宿主在阶段 6 需从 `data` 取 `jobId`/`messageId`，再按 design §5
   的扁平形态（`{jobId,code,msg,messageId}`）扇出到 bus。
5. **脱敏先行**：失败信封一律只回分类文案（`投递失败`/`投递超时`/`queue full`/校验文案），lettre 原始错误
   文本既不进信封也不进总线（design §10/§11）。代价：当前诊断细节丢失（遗留 1）。
6. **新发现（已写进 `build_profiles` 文档）**：无 runtime 上下文时，transport 的**构建期**就会 panic
   （lettre `Pool::new` 自己也要 `E::spawn`，`pool/async_impl.rs:56`），不只是销毁期。这由一次脚手架误用
   实测暴露（同步 `#[test]` 里直接调 `build_profiles` → abort），故 `MailEngine::new` 的 `rt.enter()`
   覆盖构建期而非仅销毁期。
7. **需求原文的两处顺带证据**：任务书要求「`Drop for MailEngine` 亦按此顺序」已实现（`Drop` 调同一
   `dispose`，且不再重启新线程——`shutdown_background` 让销毁在任一上下文都安全，比「挪到独立线程
   + join」更少活动件）；任务书设计的 `MailEngine.deliver` 字段未保留（worker 各持克隆即可，留字段反而
   是无读取的死字段）。

#### 7. 遗留 / 转下阶段

1. **诊断细节**：lettre 原始错误（SMTP 对话）当前被丢弃，只出分类文案。阶段 5 接 `HostContext.log`
   上送宿主日志（信封/总线仍只出脱敏分类）。
2. `jobId` 缺省生成 = `pid-计数器`（进程内唯一即可）。若阶段 6 的 `MailResultStore` 需要跨进程唯一，再换。
3. 队列取件：`mpsc::Receiver` 是单消费者，多 worker 经 `tokio::sync::Mutex`（FIFO）取用。SMTP 是 I/O
   密集，不是瓶颈；阶段 8 压测若显示取件成瓶颈再评估专用有界队列。
4. 宿主驱动的停机（`oj server` 退出时调 `shutdown`）归阶段 7；vtable → 引擎 → 真 transport 的端到端
   （`oj test` e2e）亦归阶段 7 Task 7.2（本阶段 vtable 只钉入口守卫 + 引擎层真 file transport 落盘）。


### 阶段 5 小结

**结论：`req.raw` 与结构化组装两条投递路都落地，附件字节从 vtable 原样消费（无 base64 往返），
raw **只剥信封头**（`From`/`To`/`Cc`/`Bcc`，含折叠头归属）、`Subject` 保留/结构化非空则覆盖
（§9 修正）；`oj-plugin-ffi` 仅注释同步（`repr(C)` 零改动）、`ABI_VERSION` 保持 8。**
`cargo test --release -p oj-mail` **58 passed / 0 failed**（含 §9 的 Subject 语义修正）；`fmt` 与
`clippy --all-targets -D warnings` 均 exit 0。

#### 1. 改了什么

| 文件 | 要点 |
|---|---|
| `plugins/oj-mail/src/message.rs`（新增，`mod message;`） | `SendRequest`/`AttachmentRef`（serde，含 `blobKey` rename）；`envelope_of`（结构化 `from`/`to` → 信封，从阶段 4 的 `deliver_input` 抽出）；`build_message`（结构化 → `lettre::Message`）；`build_raw`（raw → 剥离冲突头后的最终字节）；辅助 `align_attachments`/`attachment_part`/`mailbox`/`custom_header`/`ensure_no_crlf`/`kept_header_lines`/`split_head_body`/`bare_line`/`normalize_crlf`。 |
| `plugins/oj-mail/src/engine.rs` | `Req` 改为「引擎开关（`sync`/`enqueue_only`/`jobId`）+ `#[serde(flatten)] message: SendRequest`」；`deliver_input` 由 raw-only 改为两路分派（`:497`）；raw 分支拒结构化 cc/bcc（`:502`）与附件（refs/bytes 两侧）；raw 分支调 `build_raw`（`:517`，传结构化 `subject`）；组装分支调 `build_message` 并用 lettre 派生的信封（`:520`）。新增 6 条用例（`:864` 起）。 |
| `plugins/oj-mail/src/lib.rs` | 模块头更新（两条路都落地）；入口守卫用例的 req 由 `{}` 改为合法形态（理由见 §7.3）。 |

组装规则（`build_message`，`:122`）：`text`+`html` → `MultiPart::alternative_plain_html`；仅其一 →
`SinglePart::plain`/`html`；有附件 → 外层 `MultiPart::mixed()` 包住正文段 + 各附件段（只发附件也合法）；
两版正文与附件都没有 → `Err`。**信封不显式设置**：交给 lettre 由报头派生（To ∪ Cc ∪ Bcc，
`Bcc` 报头在派生后按 lettre 默认丢弃 → Bcc 收得到、互不可见），故 `cc`/`bcc` 是真投递而非装饰。

#### 2. 跑过的测试与结果（一律 `--release`）

| 命令 | 结果 |
|---|---|
| `cargo test --release -p oj-mail --lib message::tests`（实现前，RED） | **FAILED. 3 passed; 14 failed** —— 全部报 `阶段 5：结构化组装未实现` |
| `cargo test --release -p oj-mail --lib path_`（实现前，RED） | **FAILED. 1 passed; 4 failed** —— 信封 `{"code":5,...,"msg":"阶段 5：…未实现"}` |
| `cargo test --release -p oj-mail --lib attachment_count`（实现前，RED） | **FAILED. 0 passed; 2 failed**（message + engine 两侧） |
| `cargo test --release -p oj-mail`（5.1 后） | **47 passed; 0 failed** |
| `cargo test --release -p oj-mail`（5.2 后） | **54 passed; 0 failed**（`config` 10 + `engine` 15 + `message` 17 + lib 12） |
| `cargo test --release -p oj-mail`（**§9 修正后，当前**） | **58 passed; 0 failed**（新增 4 条 Subject 语义用例） |
| `cargo fmt --check` / `cargo clippy --release -p oj-mail --all-targets -- -D warnings` | 均 exit 0（0 warning） |
| `cargo xtask plugin mail --check` | `ok: mail 0.1.0 (abi 8)` / `provided axes: [mail]` |
| `grep -n "ABI_VERSION: u32" oj-plugin-ffi/src/lib.rs` | `49:pub const ABI_VERSION: u32 = 8;`（未改） |
| `git diff 06209d0 --stat -- oj-plugin-ffi/` | **空**（本阶段零改动 FFI 契约） |

**RED 的一处坑（记录以免误判为死锁）**：首次实现前直接跑全量 `cargo test` 会**永久悬挂** ——
阶段 4 的 `full_queue_returns_code4_without_blocking` 用 0 许可闸门桩，靠「worker 真走到
`send`」才放行；组装桩返回 `Err` 时 worker 不调 `send`，`started.recv().await` 永不返回
（`sample` 实锤：主线程阻塞在 libtest 的 CompletedTest 通道，worker 线程名即该用例）。
故 RED 一律**定向过滤**跑（`--lib <filter>`），不跑全量。

真实投递证据（读回落盘 `.eml`，非只断言函数返回值）：`assemble_path_*` 用例断言落盘内容含
`multipart/mixed` + `multipart/alternative` + 附件名/类型，且**不含** `%PDF-1.4` 的 base64
（`JVBERi0xLjQ`）；`raw_path_*` 用例断言落盘内容无 `evil@x`/`victim@x`/原文 Subject、有
`X-Keep` 与正文、`From:`/`To:` 由结构化信封重建。

#### 3. 附件「下标对齐 + 长度校验」怎么实现的（`message.rs:199`）

契约写进模块头（宿主 ↔ 插件的对齐表）与 `oj-plugin-ffi/src/mail.rs` 的既有注释口径一致：
`attachments[i]` 是**引用**，宿主按下标把解析结果放进 `atts[i]`。插件侧 `align_attachments`：

1. **长度必须相等**：`req.attachments.len() != atts.len()` ⇒ `Err`，文案同时给出两个数量与
   下一步（「逐个检查 attachments[i] 的 blobKey/path 是否存在且可读」）。下标错位无法从字节
   反推（长度相同时错位不可检测），静默错配会把 A 的字节挂到 B 的名字上 → 宁可整封拒投。
2. **字节**一律取 `atts[i].bytes`（`RBytes` → `&[u8]` → `Attachment::body(Vec<u8>, ..)`），
   全程不落 JSON/base64；ASCII 内容走 7bit 原样送出（实测落盘 `.eml` 里就是 `%PDF-1.4` 原文）。
3. **MIME**：`atts[i].mime` 非空优先（宿主已按「显式 `mime` 优先，否则嗅探」填好），空则回落
   `attachments[i].mime`，再空则 `application/octet-stream`；非法 mime ⇒ `Err`。
4. **文件名**：`attachments[i].filename` 优先，空则回落 `atts[i].filename`，两处都空 ⇒ `Err`。
5. 顺带校验引用**必须且只能给 `blobKey`/`path` 之一**（`Err` 点明二选一）—— 既是对契约的
   fail-loud 检查，也让这两个宿主侧字段真的被读（否则 `-D warnings` 下 `dead_code`）。

#### 4. raw 信封头剥离（含折叠头）与 Subject 语义怎么实现的（`message.rs:326`/`:383`/`:369`）

- **剥离范围 = 信封头** `From`/`To`/`Cc`/`Bcc`（`ENVELOPE_HEADERS`，`message.rs:51`）：
  信封（MAIL FROM / RCPT TO）的权威来源是结构化 `from`/`to`，原文里这些头留着就能造出
  双收件人 / 发件人 spoof。
- **`Subject` 保留**（controller 决策 2026-09-15，见 §9）：它不是信封字段，剥掉只会让邮件丢
  主题；注入面由 CRLF 校验覆盖（`message.rs:393`/`:404`：原文 Subject 行与其折行续行都要过
  `ensure_no_crlf`，裸 CR ⇒ `Err`）。`subject` **非空** ⇒ 以结构化值为准**覆盖**：原文 Subject
  整段（含续行）丢弃，由 `subject_header`（`message.rs:369`，走 lettre `Headers` 复用同一套
  RFC2047 编码 + 折行）产出唯一一个 `Subject:` 头；`subject` 为空 ⇒ 原文 Subject 原样保留。
- **分界**：`split_head_body` 用 `split_inclusive('\n')` 找**首个空行**（兼容 `\r\n` 与 `\n`），
  之前为头部区、之后为正文；无空行 ⇒ 按「全是头、空正文」处理（正文为空串，不猜）。
- **逐行状态机**（`kept_header_lines`）：行首为空格/TAB ⇒ **折行续行**，归属上一个头 ——
  `keep_prev` 为真才保留，否则一并丢弃（否则被剥头的续行会变成无主行，可能被收件端当成前一个
  保留头的续行，或触发解析错误）。否则取 `名: 值` 的名（`split(':').next()`）：名为 `subject`
  走上面的保留/覆盖分支，其余名与 `ENVELOPE_HEADERS` 做 `eq_ignore_ascii_case` 比较。
  缺 `:` 的行无从判定 ⇒ 按「保留」处理（raw 是原样透传，不额外否定调用方自己的 MIME）。
- **报头重建**：`From:`/`To:` 由结构化信封生成（`envelope.from()` / `envelope.to()`），
  信封（MAIL FROM / RCPT TO）用显式 `.envelope(envelope.clone())` 等价物 —— 即 `engine` 传下去
  的那一份，与原文报头**彻底解耦**（原文 `To:` 换成什么都没用，RCPT TO 只认结构化 `to`）。
- **不经 lettre 组装**（关键决定）：`MessageBuilder::body` 会按「最优编码」重编码正文
  （`email_encoding::body::chooser::line_too_long` 在**行 ≥76 字节**时就改用 quoted-printable/
  base64；base64 分块恰好 76 列 → 必然触发），而原文的 `Content-Type: multipart/...;boundary=`
  报头是**原样保留**的 ⇒ 输出会变成「multipart 报头 + base64 正文」，收件端解析不出任何 part。
  而强制「原样」的另一条路 `Body::new_with_encoding` 在编码不合法时是 `expect` **panic**
  （`lettre-0.11.23/src/message/body.rs:190`），在 worker 里 panic 不可接受。故 raw 路自己拼字节。
- **行尾归一 CRLF**（正文除行尾外逐字节保留）：lettre 的送出侧只做点填充
  （`transport/smtp/client/mod.rs:74` 的 `ClientCodec`），**不做**行尾归一，且只在 `\r\n` 之后
  才认「行首」—— 裸 LF 之后的行首 `.` 不会被填充，中间 MTA 可据此提前结束 DATA、把余下内容
  当 SMTP 命令执行（smuggling）。JS 侧传来的 raw 天然是 LF，故必须自己归一。

#### 5. `code:5`（入参/契约校验）映射点

| 位置 | 触发 |
|---|---|
| `engine.rs:502` | raw 路给结构化 `cc`/`bcc`（无报头可放：静默丢件/泄露 Bcc 都不可接受） |
| `engine.rs:508` | raw 与附件互斥（refs 侧）；`message.rs:333` 同判据（bytes 侧，vtable 契约兜底） |
| `engine.rs:515` | `envelope_of` 失败（缺 `to`、`from`/`to[i]` 地址非法、含 CR/LF） |
| `engine.rs:517` | `build_raw` 失败（`envelope` 无发件人、附件非空、原文/结构化 Subject 含 CRLF） |
| `engine.rs:520` | `build_message` 失败（见下逐条） |
| `message.rs:218` / `:228` / `:239` | 附件数量不匹配（下标对齐失败）/ 引用来源不是「blobKey/path 二选一」/ 缺 filename |
| `message.rs:139` / `:176` | `to` 为空 / 缺少正文（`text`/`html` 都空且无附件） |
| `message.rs:275` | `from`/`to[i]`/`cc[i]`/`bcc[i]` 地址非法或含 CR/LF（`mailbox` 共用，字段名由调用点传入） |
| `message.rs:120` / `:126` | `envelope_of` 的 `from` / `to[i]` 地址非法或含 CR/LF |
| `message.rs:155` / `:393` / `:404` | 结构化 `subject` 含 CR/LF / 保留的原文 Subject 行及其折行续行含 CR（头注入） |
| `message.rs:283` / `:284` / `:290` | `headers` 名含 CR/LF / 值含 CR/LF / 覆盖 `From`/`To`/`Cc`/`Bcc`/`Subject` |
| `message.rs:264` | 附件 `mime` 非法 |

两处**不属于** `code:5` 的失败（属 req 形态错误，沿用阶段 4 口径在 `submit` 期即 FFI `Err`，
不占队列槽位）：`Req` 反序列化失败（`engine.rs:270`，含缺 `from`）与未知 profile（`engine.rs:279`）；
`deliver_one` 侧的同名兜底在 `engine.rs:451`。

#### 6. 变异验证（5 条，证明新断言真在钉行为）

| 变异 | 期望红的用例 | 实测 |
|---|---|---|
| `CONFLICTING_HEADERS` 的 `"subject"` 改成 `"subjex"`（旧口径下的表） | `raw_strips_from_to_cc_bcc_subject_case_insensitively` | **首次 FAILED 的只有 engine 落盘用例** ⇒ 暴露消息侧断言过弱（原 raw 的 `SUBJECT: s` 与其它断言无交集）→ 已加强（见 §7.8）；复验同变异下**两条都 FAILED** |
| 折行续行改为无条件保留 | `raw_strips_folded_continuation_of_stripped_header_only` | **FAILED**（输出里出现 `leak-me`） |
| 删掉 `align_attachments` 的长度校验 | `rejects_attachment_count_mismatch` + `attachment_count_mismatch_returns_code5` | **双双 FAILED**（2 failed） |
| `kept_header_lines(head, true)`（原文 Subject 永不丢弃 ⇒ 覆盖语义失效） | `raw_structured_subject_overrides_raw_subject`（message + engine 两条） | **双双 FAILED**（2 failed，见 §9） |
| `ENVELOPE_HEADERS` 加回 `"subject"`（回到旧口径） | `raw_strips_envelope_headers_but_keeps_subject_and_body` | **FAILED**（见 §9） |

#### 7. 与任务书/计划的偏差（均有实测依据，未弱化任何断言）

1. **`build_raw` 返回 `Vec<u8>`（最终字节）而非 `lettre::Message`**：任务书示例用例是
   `build_raw(..).unwrap().formatted()`。改用 `Message` 无法做到「原文原样」——`MessageBuilder::body`
   会重编码正文（行 ≥76 字节即 QP/base64）而原文报头保留（见 §4），实测依据是
   `email_encoding-0.4.2/src/body/chooser.rs` 的 `line_too_long` 与
   `lettre-0.11.23/src/message/body.rs:190` 的 `expect("invalid encoding")` panic 面。
   故示例用例的绑定行改为 `String::from_utf8(build_raw(&envelope(), raw, &[]).unwrap())`，
   **断言一条未动**（X-Keep/body 保留、evil/victim 消失），并新增运行期用例读回落盘 `.eml` 佐证。
2. **`SendRequest.subject` 加 `#[serde(default)]`**（任务书给必填 `String`）：design §5 的
   `sendRaw(o)` 只传 `from`/`to`/`raw`，必填会把 raw 路堵死；阶段 4 的 `req_raw` 夹具同样不带
   subject，必填会让**全部**既有引擎用例解析失败。
3. **`from` 保持必填**：缺 `from` 现在是 `submit` 期的 FFI `Err`（`missing field \`from\``）而非
   `code:5` 信封 —— 与 vtable 文档「req 形态错误 → Err」一致；连带把
   `vtable_submit_fails_loud_without_matching_profile` 的 req 从 `{}` 改为合法形态（该用例要钉的是
   profile 守卫，`{}` 会被 JSON 形态分支先拦下，测不到目标分支）。用例注释已写明这条。
4. **raw 路的结构化 `cc`/`bcc` 直接 fail-loud**（任务书未提）：raw 的报头由原文承载、而原文
   Cc/Bcc 会被剥离 ⇒ 结构化 cc/bcc 无处安放。静默丢件与塞进 `To:`（泄露 Bcc）都不可接受。
   design §5 的 `sendRaw` 本就只传 `from`/`to`/`raw`，与此一致。
5. **`headers` 禁止覆盖 `From`/`To`/`Cc`/`Bcc`/`Subject`，且值/名/地址拒 CR/LF**（任务书未提）：
   组装路的信封是 lettre **由报头派生**的，允许覆盖 = 绕过宿主收件人白名单的面；CRLF 拒绝是
   设计 §10/§11「宿主剥离」之外的纵深防线（插件是 MIME 输出的最后一环）。
6. **附件引用必须恰给 `blobKey`/`path` 之一**：契约完整性检查，同时避免 `blob_key`/`path`
   成为「只写不读」字段（cdylib + 私有模块下会 `dead_code` 报错）。
7. **`build_raw` 增加 `subject` 形参**（原为 `(envelope, raw, atts)`）：覆盖语义要求把结构化主题
   传进来。**已按 controller 决策落地**：`subject` 非空 ⇒ 覆盖原文 Subject，为空 ⇒ 保留原文
   （见 §9；本节原「raw 结构化 subject 被忽略」的偏差已闭环）。
8. **提交粒度**：按任务书拆成两次 feat 提交（5.1 / 5.2），另加一次测试加强（`3599b47`，变异
   验证暴露的弱断言）与一次小结提交。5.1 的中间态是「组装路已通、raw 仍原文直通」，两种状态
   都跑了全量门禁（47 / 54 绿），不是为了拆分而拆分。

#### 8. 遗留 / 转下阶段

1. ~~**raw 路主题语义待定**~~ → **已闭环**（§9）：`subject` 非空覆盖原文 Subject，为空保留原文；
   已按此实现并落盘验证。
2. **`attachments` 的 mime 嗅探在宿主**（阶段 6）：插件只认 `atts[i].mime`/显式 `mime`，
   `path`/`blobKey` 的解析、越界与白名单校验全在宿主 `src/bridge/mail.rs`。
3. **诊断细节**：阶段 4 遗留的「lettre 原始错误只出分类文案」未变；`HostContext.log` 上送
   归阶段 6。
4. `Message` 组装路的日期/`Message-ID` 由 lettre 补齐（`date_now`/`hostname`），宿主无需干预；
   file transport 落盘 id 仍与 `messageId` 同源（阶段 4 既有断言继续守着）。

#### 9. 阶段 5 修正记录（2026-09-15，controller 决策：raw 路的 `Subject` 语义）

**决策**（覆盖本节早先「raw 一律剥离 `Subject`」的口径，设计文档 §7/§11 已同步）：
raw 路**只剥离信封头 `From`/`To`/`Cc`/`Bcc`**；`Subject` **保留**（非信封字段，剥它只会丢主题；
注入风险由 CRLF 校验覆盖）。若结构化 `subject` 非空 ⇒ 以它为准**覆盖**原文 Subject；为空 ⇒ 保留原文。

**改动**：`message.rs` 拆出两个常量（`ENVELOPE_HEADERS` 4 项用于剥离 / `STRUCTURED_HEADERS` 5 项用于
`headers` 覆盖禁令，`message.rs:51`/`:55`）；`build_raw` 收 `subject: &str` 形参（`:326`），新增
`subject_header`（`:369`，复用 lettre `Headers` 做 RFC2047 编码）；`kept_header_lines` 改为
`(head, keep_subject) -> Result<...>`（`:383`，Subject 行及其折行续行在保留时须过 CRLF 校验）；
`engine.rs` 传 `&m.subject`（`:517`）；`oj-plugin-ffi/src/mail.rs` 的 vtable 文档注释同步
（**仅注释**，`repr(C)` 未动，`ABI_VERSION` 仍 8，`cargo xtask plugin mail --check` → `ok … (abi 8)`）。

**TDD 证据**：

| 步骤 | 命令 | 结果 |
|---|---|---|
| RED（先改断言，实现未动） | `cargo test --release -p oj-mail --lib raw_` | **FAILED. 6 passed; 2 failed** —— `raw_strips_envelope_headers_but_keeps_subject_and_body`（"原文 Subject（含折行续行）必须保留"）、`raw_rejects_crlf_injection_in_raw_subject`（旧口径下被静默剥离 ⇒ 无 `Err`） |
| GREEN（实现后） | `cargo test --release -p oj-mail` | **58 passed; 0 failed** |
| 变异 A：`kept_header_lines(head, true)` | `--lib raw_` | **FAILED. 9 passed; 2 failed**（message + engine 两条 override 用例） |
| 变异 B：`ENVELOPE_HEADERS` 加回 `"subject"` | `--lib raw_` | **FAILED. 10 passed; 1 failed**（`raw_strips_envelope_headers_but_keeps_subject_and_body`） |
| 门禁 | `cargo fmt --check` / `cargo clippy --release -p oj-mail --all-targets -- -D warnings` | 均 exit 0 |

**落盘 `.eml` 实证**（`file_engine` 读回，`^M` = CRLF；三条均为 `raw_path_*` 用例产出）：

```
# 案例 A：raw 含 "From: evil@x / To: victim@x / Subject: raw-sub"，结构化 subject 为空
#          ⇒ 信封头被剥、Subject 保留（位置即原文顺序：在 X-Keep 之前）
From: from@example.com
To: to@example.com
Subject: raw-sub
X-Keep: 1

body

# 案例 B：raw 含 "Subject: raw-sub\n  folded-leak"，结构化 subject="struct-sub"
#          ⇒ 覆盖：只有一个 Subject，原文 Subject 与折行续行都不残留
From: from@example.com
To: to@example.com
Subject: struct-sub
X-Keep: 1

body

# 案例 B2：结构化 subject="结构主题"（非 ASCII）⇒ RFC2047 编码后覆盖
From: from@example.com
To: to@example.com
Subject: =?utf-8?b?57uT5p6E5Li76aKY?=

body
```

**一致性**：与设计文档 §7（`docs/plans/2026-09-15-mail-smtp-design.md:132`）「剥离 `From/To/Cc/Bcc`；
保留 `raw` 的 `Subject`；结构化 `subject` 非空则覆盖；行尾归一 CRLF」与 §11（同文件 `:161`
「`Subject` 保留但做 CRLF 校验，结构化 `subject` 非空则覆盖」）**逐条对应**；§8 数据流与 §12 测试策略
无冲突。唯一额外动作是 FFI vtable 的**注释**同步（原文「`subject` 等组装字段被忽略」在决策后已不成立）。


### 阶段 6 小结

**结论：宿主侧 mail 能力全部落地 —— ops（6 个）、StableState/Extras 注入、权威校验、附件
字节解析、结果存储 + `deliver` 路由、`Mail`/`mail` JS 全局。`oj-plugin-ffi` 零改动、
`ABI_VERSION` 保持 8；`bootstrap.js` 保持 7-bit ASCII（0 个非 ASCII 字节）。
`cargo test --release -p only-js` = **350 + 5 + 1 passed / 0 failed**（lib 由 330 → 350，
新增 20 条全为本阶段用例；既有 336 无回归）；`-p oj --lib` 156 passed；
`fmt --check` 与 `clippy --release --all-targets -- -D warnings` 均 exit 0。**

#### 1. 改了什么

| 文件 | 要点 |
|---|---|
| `src/bridge/mail.rs`（新增，约 940 行 + 700 行测试） | `MailBackend` trait（`:45`，`submit`/`config`/`router`）+ `FfiMailBackend` 适配器（`:312`）；`MailConfig`/`MailProfileCfg`（`:65`，仅非密钥面）；`MailResultStore`（`:156`，限长 + TTL）；`flatten_result`（`:222`）；`MailResultRouter`（`:244`，存 + 同步扇出）；进程级 deliver 槽（`:287`/`:291`/`:297`）；校验（`strip_crlf:373`/`validate_address:379`/`check_whitelist:396`/`normalize_headers:447`）；附件（`parse_attachment_refs:501`/`resolve_mime:564`/`resolve_attachments:627`）；编排 `handle_send:815` + 6 个 op（`:878` 起）。 |
| `src/bridge/mod.rs` | `mod mail;` + `bridge_ext` 注册 6 个 mail op（`:271` 起）；`StableState.mail`（`:161`）与 `Extras.mail`（`:189`）字段；构造期 `install_mail_deliver`（`:570`）。 |
| `src/bridge/bus.rs` | `EventBroker::publish_local`（默认 0；`Bus` 实现为 `Bus::publish`）——`deliver` 回调是插件线程上的 `extern "C"`，不能 await。 |
| `src/bridge/ffi.rs` | `host_deliver` 对 `mail.result` 早退到宿主路由（`:565` 起）；抽出 `fanout_targets`（`:583`）供 `FfiEventBroker::publish_local`（`:707`）复用。 |
| `src/bridge/module_loader.rs` | `ensure_within` → `pub(crate)` 且**返回 canonical 句柄**（`:367`）；调用点仅丢弃返回值。 |
| `Cargo.toml` | 新增 `lettre = { version = "0.11", default-features = false }`（**只用 `lettre::Address`**；不拉 smtp-transport/rustls/native-tls，`cargo tree -i rustls` 仍单一 `0.23.40`）。 |
| `oj/src/app.rs` | 两处结构体字面量补 `mail: None`（阶段 7 装配入口）。 |
| `src/bridge/bootstrap.js` | 导入 6 个 op + 挂 `globalThis.Mail` / `globalThis.mail`（`:298` 起）。 |

#### 2. 测试与 RED→GREEN 证据（一律 `--release`）

| 步骤 | 命令 | 结果 |
|---|---|---|
| 6.1 RED | `cargo test --release -p only-js --lib ensure_within` | **编译失败** `E0599 method not found in '()'` / `E0308 expected '()', found 'PathBuf'`（旧签名返回 `()`） |
| 6.1 GREEN | 同上 | **1 passed**（canonical 句柄 + `../` 越界/不存在/根外绝对路径三拒绝） |
| 6.2/6.3 RED | `--lib mail` | **编译失败**：`cannot find type 'MailConfig'/'MailResultRouter'/'ParsedAttachment'`、`cannot find trait 'MailBackend'`、`cannot find value 'MAIL_RESULT_TOPIC'` |
| 6.2/6.3 GREEN | `--lib mail` | **7 passed**（注入/路由/扇出/限长/TTL/覆盖/扁平化/配置面/vtable 适配） |
| 6.4 RED | `--lib mail` | **编译失败**：`cannot find function 'strip_crlf'/'validate_address'/'check_whitelist'`… |
| 6.4 GREEN | `--lib bridge::mail` | **17 passed** |
| 6.5 RED | `--lib bridge::mail::tests::js_mail` | **FAILED. 0 passed; 3 failed** —— `ReferenceError: mail is not defined` |
| 6.5 GREEN | `--lib bridge::mail` | **20 passed**（全量 lib 350） |
| 门禁 | `fmt --check` / `clippy --release --all-targets -- -D warnings` | exit 0（0 warning） |
| 回归 | `cargo test --release -p only-js` / `-p oj --lib` | 350+5+1 passed / 156 passed，均 0 failed |

**变异验证（3 条，均被现有用例抓住 —— 证明用例真的绑住了行为，非恒真）**：

| 变异 | 结果 |
|---|---|
| A. `strip_crlf` 退化为 `s.to_string()` | **FAILED. 17 passed; 3 failed**（`strip_crlf_removes_header_injection`、`handle_send_normalizes_and_dispatches`、JS 端到端） |
| B. 白名单空表放行（fail-open） | **FAILED. 0 passed; 1 failed**（`whitelist_is_suffix_based_and_fail_closed`） |
| C. 附件解析结果倒序（下标错位） | **FAILED. 17 passed; 3 failed**（指向解析顺序 3 条用例） |

#### 3. `FfiFuture` 宿主侧驱动方式

**完全复用既有适配器**：`FfiMailBackend::submit`（`mail.rs:330`）调
`super::ffi::await_ffi(fut)`——与 `FfiEsBackend`（`ffi.rs:225` 起）、`FfiBlobBackend`、
`FfiDataAccessor`、`FfiEventBroker::publish` **同一份** poll + `yield_now` 驱动，
经 `FfiGuard` 持有（await 被取消时 Drop 只 `free` 不 `take`，插件任务允许跑完）。
没有任何新增的跨线程手段：`JsRuntime` 的 `current_thread` 语义与
「插件 worker 在插件自己的 `multi_thread` runtime」的原分工不变。

#### 4. 校验口径与设计取向（本轮新增的 5 个决策，需 controller 确认）

1. **CRLF 一律「剥离」而非拒绝**（`strip_crlf`，`mail.rs:373`）：design §10 原文即「先剥 `\r\n`」，
   计划 Task 6.3 的样例断言也是 `sanitize_header("a\r\nBcc: x") == "aBcc: x"`。**地址例外**：
   `from/to/cc/bcc` 走**拒绝**（`validate_address`，地址没有「含换行的合法值」）；
   插件侧 `ensure_no_crlf` 是拒绝，两边不冲突——宿主先把头字段消毒，插件看到的永远是干净的。
   正文（`text`/`html`）与 `raw` 原文**不剥**（换行在正文有语义；raw 的冲突头剥离是插件职责）。
2. **白名单空表 = 拒绝（fail-closed）**：`allowed_from`/`allowed_recipients` 缺省不放行任何收件人。
   理由：白名单是「越权发送」的唯一控制点，缺省放行等于把每封邮件都变成开放中继；与本特性
   `tls: none` 需显式许可同一取向。**影响阶段 7**：`sample/config.yaml` 的 `smtp.mock` profile
   必须显式写 `allowed_from`/`allowed_recipients`，否则 FileTransport e2e 会（正确地）被拒。
3. **返回信封 vs 抛异常**：`mail.send/sendSync/enqueue/sendRaw` **一律 resolve** 结果信封
   （校验/附件/JSON 形态错误 → `{code:5}`；FFI 层失败 → `{code:1,msg:"投递未能送达插件"}），
   与插件同款 `{code,msg,data}` 形态，JS 侧不需要 try/catch 就能统一判码；**只有「未配置 mail」
   抛异常**（与 `es not configured` 一致）。
4. **`enqueue` 的返回值是信封**（`{code:0,data:{jobId}}`，不是裸 `jobId`）：与 send/sendSync
   同形态；design §5 的 `const jobId = await mail.enqueue(...)` 属示意写法，api-manual（阶段 7.3）
   按 `res.data.jobId` 写并与 `mail.result(jobId)` 配对。
5. **宿主按 op 覆写 `sync`/`enqueue_only`**（`handle_send`，`mail.rs:848`）：JS 侧在 `send()` 里
   夹带 `sync:true` 不会改变 transport 选择（否则 `send`/`sendSync` 的区分形同虚设）。

#### 5. 附件顺序对齐与越界拒绝的实证

- **下标对齐**：宿主 `resolve_attachments` 按 `refs` 原序 push（`mail.rs:627`），
  `FfiMailBackend::submit` 按同序 push 进 `RVec<MailAttachment>`（`:337`）；用例断言
  `attachments=[{path:b.pdf},{blobKey:a.bin}]` 时 `atts[0].filename=="b.pdf"`、
  `atts[1].filename=="a.bin"`（含 blob 侧非 UTF-8 字节 `BLOBBYTES` 原样过线、显式 mime 覆盖嗅探值），
  JS 端到端用例同断言；变异 C 证明错序必被抓。
- **越界拒绝**：`{path}` 先 `ensure_within(p, project_root)`（双侧 canonicalize，覆盖符号链接），
  **并按返回的 canonical 句柄 `fs::read`**（校验路径 ≡ 读盘路径，design §9 TOCTOU）；
  `../outside.txt` → `{code:5}`「附件路径非法：… escapes project root（下一步：把附件放到项目根内）」
  （`resolve_attachments_reads_blob_and_path_in_declared_order`）；`module_loader` 用例另证
  `../` 与根外绝对路径被拒。无 loader（无 project root）时 `{path}` 附件明确报错。

#### 6. `bootstrap.js` 7-bit ASCII 证据

```bash
$ python3 -c "d=open('src/bridge/bootstrap.js','rb').read(); print(sum(1 for b in d if b>127), len(d))"
0 23023
```

且 `BRIDGE_ESM` 用 `ascii_str_include!` 内嵌（非 ASCII 会**编译期**失败）——本阶段
`cargo build/clippy/test` 全部通过即第二重证据。JS 侧注释一律英文。

#### 7. 提交

| SHA | 信息 |
|---|---|
| `028f73e` | `refactor(loader): ensure_within 提 pub(crate) 并返回 canonical 句柄` |
| `3111477` | `feat(bridge): MailBackend + StableState/Extras.mail + 结果存储与 deliver 路由` |
| `497fcd5` | `feat(bridge): mail ops + 宿主权威校验 + 附件字节解析` |
| `3f6b57a` | `feat(bridge): Mail/mail JS 全局（7-bit ASCII）` |

#### 8. 遗留 / 交给阶段 7（不阻塞本阶段）

1. **装配**（Task 7.1）：`oj/src/app.rs` 两处仍为 `mail: None`；需按 `smtp:` 段 + `oj-mail` 插件
   构造 `MailConfig::from_value(&cfg)` + `FfiMailBackend::new(vtable, config, bus)` 注入 `Extras.mail`
   （`bus` 必须与 `Extras.bus` 同一实例，否则 `mail.result` 扇出到不了 JS 订阅者）。
   注意 `oj/src/app.rs:676` 的 `StableState` 字面量（测试运行时注入路径）也要同源注入。
2. **宿主校验与插件一致性回归**：插件 `message.rs::envelope_of` 对 `from/to` 用 `Address` 解析
   （不接受 `Name <a@b>` 显示名），宿主同用 `Address` —— 阶段 8 用一条 e2e 固化「宿主放行 ≡ 插件放行」。
3. **`headers` 名字校验**：宿主只做「剥 CRLF + 禁覆盖结构化头」，ASCII/`:`/空格等合法性仍由插件
   `HeaderName::new_from_ascii` 兜底（纵深防御，未在宿主重复）。
4. **`mail.result` 结果只存不推远端 broker**：`route` 走 `publish_local`（同步本地扇出）。
   分布式 bus（kafka/rabbit）下 JS `bus.subscribe("mail.result")` 走 `FfiEventBroker::publish_local`
   → `DELIVER_TARGETS`，本地订阅者可收；**跨进程**订阅者收不到（设计 §6 的
   `EventBroker::publish(...).await` 需 async 上下文，而 `deliver` 回调是同步 `extern "C"`）。
   phase 8 若要跨进程反馈，需另立「异步转发任务」方案（当前 YAGNI，已记录）。
5. **`HostContext.log` 上送**（阶段 5 遗留）不在本阶段范围，仍未接。

### 阶段 7 小结
（待填）

### 阶段 8 小结
（待填）
