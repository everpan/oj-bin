# 设计文档：lettre SMTP 绑定（v0.1.20，插件实现）

- 日期：2026-09-15（初稿）→ v2（依架构师/工程师/安全工程师评审）→ **v3（自洽性总检，按真实代码事实校正）**
- 状态：已脑暴 + 三方评审 + 自洽复核，待实现
- 实现形态：**cdylib 插件 `oj-mail`**（非内建 op），新增 `mail` 轴。

## 0. 版本修订

**v3（自洽性校正，按代码事实）**
- **新增轴零 ABI 变更**：CLAUDE.md 明示「加轴零破坏，既有轴 vtable 形状变更才 bump ABI」，`mq` 轴即先例（其注释：新增轴，ABI 保持 7）。故 `mail` 轴**不 bump `ABI_VERSION`**（保持 8）——v2/v3 早稿的「8→9」有误。
- `ABI_VERSION` **当前为 8**（`oj-plugin-ffi/src/lib.rs:47`）。
- `AXES` **当前含 `mq`**（`plugin_loader.rs:431`：`["es","db","blob","bus","kv","auth","mq"]`），增 `"mail"` 并**同步 `probe_axes` 分支**（否则 :456 `unreachable!`）与 `Registrations` 加字段。
- **插件无宿主后端访问**：`HostContext` 只有 `log`+`deliver`（`lib.rs:76-83`）。故 **附件字节解析在宿主**（`StableState.blobs` / `ensure_within`+`fs::read`），以 `RBytes` 经 FFI 传插件；**bus 发布在宿主**。删除 v2「插件读 blob / 插件持 bus」。
- **异步契约用 `FfiFuture`**（`oj-plugin-ffi/src/future.rs`：poll/take/free + `catch_future`），非 `deliver` 通用回传；`deliver` 仅作插件→宿主的**结果上送**通道。

**v2（三方评审）**：改插件实现；`StableState` 注入（不进 `ReqState`）；队列 graceful drain；rustls provider 时序；`ensure_within` 提 `pub(crate)`；安全加固（CRLF 注入、sendRaw 冲突头、profile 白名单、bus 反馈脱敏、none TLS 门禁）。

## 1. 背景与目标

业务 handler 需对外发邮件（通知/告警/事务）。在 `oj` 提供与 `json`/`http`/`fetch`/`bus` 同风格的发信能力：配置驱动多客户端、同步/异步、队列线程池消费、双通道反馈；复用 `rustls`/`blob`/`bus`/`ensure_within`/`{code,msg,data}`。

## 2. 范围（YAGNI）

**做**：多 profile（key 区分）、明文/XOAuth2、tls/starttls/none、结构化构造 + `sendRaw`、引用式三源附件、队列线程池、双通道反馈、超时/背压/脱敏、FileTransport e2e。
**不做（首版）**：bounce/webhook、DKIM/SPF（交中继）、模板引擎（交 JS）、自动重试（仅超时+一次性）、per-来源令牌桶限流（全局有界队列为 v1）、XOAuth2 静默刷新（首版静态 token）。

## 3. 架构（插件 + 清晰的职责边界）

**职责边界（本版核心修正）**
- **宿主（核心 `src/bridge/`）**：配置装配、`StableState` 字段、JS 全局挂载、**入参校验**（CRLF/地址/白名单）、**附件字节解析**（blob / 本地文件）、**`bus` 发布**、结果存储。
- **插件（`plugins/oj-mail`，cdylib）**：持有 `lettre`、连接池（transport）、**有界队列 + worker 池**、实际投递；经 `FfiFuture`/`deliver` 回传结果。**不直接访问宿主 blob/bus**（`HostContext` 不提供）。

**新增轴**
- `oj-plugin-ffi`：增 `MailVtable` repr(C) vtable + `MailAttachment` repr(C)；`plugin_loader` 增 `axis::mail` 类型配对 helper；`AXES` 增 `"mail"` + `probe_axes` 分支 + `Registrations` 字段。**不 bump `ABI_VERSION`**（新增轴零破坏，保持 8）。
- `plugins/oj-mail`：实现 `MailVtable`，`oj_plugin_entry!(init, mail => oj_plugin_ffi::axis::mail(&MAIL_VTABLE))`。
- 宿主 `src/bridge/mail.rs`：`#[op2]` `op_mail_send/send_sync/enqueue/result/send_raw`，经 `StableState.mail`（`Arc<dyn MailBackend>`，包装 vtable）调用；`bootstrap.js` 挂载 `Mail`/`mail`。
- `StableState` 与 `Extras` **各增字段** `mail: Option<Arc<dyn MailBackend>>`（`mod.rs:123/164`），在 `with_dbs_and_loader` 注入（首次 run 前）；**不进 `ReqState`**。

## 4. 配置形态（config.yaml）

```yaml
smtp:
  workers: 4
  queue_capacity: 256
  default:
    host: smtp.example.com
    port: 465
    tls: tls                 # tls | starttls | none
    allow_none_tls: false    # none 必须显式 true 且 host 为内网 CIDR
    mechanism: login         # login | xoauth2
    user: api@x
    pass: <secret>
    # xoauth2: { client_id, client_secret, refresh_token }
    timeout: 30
    allowed_from: ["noreply@x.com"]              # 发件人白名单（后缀）
    allowed_recipients: ["@x.com", "@partner.com"] # 收件人白名单（后缀）
  alerts:
    host: smtp.other.com
    port: 587
    tls: starttls
    mechanism: login
    user: ...
    pass: ...
    allowed_from: ["alert@x.com"]
  mock:
    file_transport: /tmp/oj-mail-eml
```

- **插件**经 `oj_plugin_init(host, cfg)` 的 `cfg` 拿到 `smtp:` 段（含凭据）构建 transport。
- **宿主**另解析 `smtp:` 的**非密钥面**（profile keys、`allowed_*`、`tls`/`allow_none_tls`、host/port）作**前置校验**用；凭据不落宿主 JS 面。
- 二者读同一段配置（宿主校验、插件投递），职责不重叠。

## 5. JS API

```js
globalThis.Mail = class {
  constructor(key = "default") { this.key = key; }
  send(m)     { return ops.op_mail_send(this.key, m); }        // 异步 transport
  sendSync(m) { return ops.op_mail_send_sync(this.key, m); }   // 同步 transport（worker 内 spawn_blocking）
  enqueue(m)  { return ops.op_mail_enqueue(this.key, m); }     // fire-and-forget → jobId
  result(id)  { return ops.op_mail_result(this.key, id); }     // 查结果（宿主侧存储）
  sendRaw(o)  { return ops.op_mail_send_raw(this.key, o.from, o.to, o.raw); }
};
globalThis.mail = new Mail("default");
```

```js
await mail.send({
  from: "noreply@x.com", to: ["a@x.com"], cc: ["c@x.com"], bcc: [],
  subject: "hi", text: "plain", html: "<b>hi</b>",
  headers: { "X-Custom": "v" },
  attachments: [
    { filename: "r.pdf", blobKey: "r2d2" },
    { filename: "x.pdf", path: "reports/x.pdf" }
  ]
});
const jobId = await mail.enqueue({ from, to, subject, text });
// 反馈：bus 主题 mail.result 推 { jobId, code, msg, messageId }（不含 to/subject）
```

> `send` 与 `sendSync` **对 JS 均非阻塞**（经队列 + FfiFuture await）；区别仅内部走 async / sync transport。

## 6. Rust 组件

**`oj-plugin-ffi`（契约）**
- `MailAttachment`（repr(C)）：`{ filename: RString, mime: RString, bytes: RBytes }`——**字节由宿主解析后传入**，不经 JSON/base64。
- `MailVtable`（repr(C)）：`submit(key: RString, req: RString, atts: RVec<MailAttachment>) -> FfiFuture`。`req` JSON 含 `{ sync, enqueue_only, raw?, from, to[], cc[], bcc[], subject, text?, html?, headers{}, jobId }`。
  - `submit` 返回 `FfiFuture`（`catch_future` 包装）；`send` 语义：future resolve = 投递结果信封；`enqueue` 语义：future 立即 resolve `{jobId}`，真实完成经 `HostContext.deliver`。
- `AXES` 增 `"mail"`（同步 `probe_axes` 分支 + `Registrations.mail` 字段）；`axis::mail` helper；**ABI 不变（8）**。

**`plugins/oj-mail`（实现）**
- 反序列化 `req`；`Message::builder()` 组装 MIME（附件直接取 `MailAttachment.bytes`）。
- `MailProfile { async: Arc<AsyncSmtpTransport<Tokio1Executor>>, sync: Arc<SmtpTransport> }`；`MailEngine { registry, tx: mpsc::Sender<Job>, _rt: multi_thread Runtime }`（**无 bus、无 blob**）。
- worker 循环：`recv → 取 transport（按 sync）→ timeout 包裹投递 →`（send）resolve FfiFuture /（enqueue）`host.deliver("mail.result", envelope_json)`。
- init 建 transport 前 `rustls::crypto::CryptoProvider::install_default(aws_lc_rs::default_provider())`（幂等）。

**`src/bridge/mail.rs`（宿主）**
- `MailBackend` trait 包装 `MailVtable` vtable；`StableState.mail` 持有。
- ops：校验（CRLF/地址/白名单）→ 解析附件（`StableState.blobs.get(name)?.get(key).await` / `ensure_within`+`fs::read`，复用已 canonicalize 句柄防 TOCTOU）→ 组装 `MailAttachment` → `submit` → `send` await FfiFuture 回信封。
- 宿主侧 `MailResultStore`（`DashMap<job_id, Envelope>` + 限长/TTL）：`deliver("mail.result")` 路由到「存结果 + 本地 bus 扇出（供 JS `bus.subscribe`）」；`op_mail_result` 读它。需 `bus` 分布式时由宿主 `EventBroker::publish("mail.result", &BusPayload::Json(..)).await`。
- `MailConfig`（非密钥面）供校验与 profile 列举。

## 7. 数据流

`send`：`op_mail_send` → `StableState.mail` → 校验（CRLF 剥离 + `lettre::Address` 强校验 + `allowed_*` 白名单）→ 附件解析为字节 → 组装 `MailAttachment` → vtable `submit(key, req, atts)` → FfiFuture → op await → 信封回 JS。

`enqueue`：`req.enqueue_only=true` → `submit` future 立即回 `{jobId}`；worker 完成后 `host.deliver("mail.result", {jobId,code,msg,messageId})` → 宿主存 `MailResultStore` + 本地 bus 扇出。

`sendSync`：`req.sync=true`；worker 内 `spawn_blocking(move || sync_transport.send(msg))` 再 `.await`（不冻结 JsRuntime）。

`sendRaw`：结构化 `from/to` 作信封；宿主在组装前**剥离 `raw` 中 `From/To/Cc/Bcc/Subject` 头行**（防双收件人/spoof），余下正文/头保留。

## 8. 异步/线程模型与生命周期

- 插件自起 `multi_thread` 运行时跑 worker（**不挤占 JsRuntime 的 `current_thread`**）；worker 不碰 JsRuntime，回传经 FfiFuture（Send）/`deliver`。
- `StableState.mail` 仅注入一次（首次 run 前）；请求期 op `borrow` 取 `Arc`，不进 `ReqState`。
- **graceful drain**：停机信号 → 停收新 job（`try_send` 拒绝）→ 等在途完成（总超时）→ drop worker `Runtime`。防 oneshot 悬挂与在途邮件丢失。
- **rustls provider**：插件 init 建 transport 前 `install_default`，与 `ws_client_extensions` 全局 provider 同为 aws-lc-rs。

## 9. 附件（宿主解析 → 字节传插件）

- `{ blobKey }`：宿主 `StableState.blobs.get(name)?.get(key).await` 取字节（本地/S3 统一），serde `#[serde(rename="blobKey")]`。
- `{ path }`：宿主 `ensure_within(path, project_root)`（提 `pub(crate)`，双侧 canonicalize 覆盖符号链接）后 `fs::read`；复用已 canonicalize 句柄防 TOCTOU。
- 字节经 `MailAttachment.bytes`（`RBytes`）传插件——**既不进 JS、也不走 base64**，大附件只过一次 FFI 指针拷传。
- `mime` 显式优先，否则宿主按扩展名/字节嗅探后填入。

## 10. 错误处理 / 超时 / 背压

- `code`：连接/网络→`1`、5xx→`2`、鉴权→`3`、队列满→`4`、地址/白名单校验→`5`。`data:{jobId,messageId?}`。
- **CRLF 注入防护**：`subject`/`headers`/地址先剥 `\r\n`；`from/to/cc/bcc` 经 `lettre::Address` 强校验，非法即 `code:5`。
- **白名单**：`from` 匹配 `allowed_from` 后缀、`to/cc/bcc` 匹配 `allowed_recipients` 后缀，否则 `code:5`。
- `msg` 脱敏（无账号/密码/令牌/SMTP 对话）；bus 反馈仅 `jobId/messageId/code`。
- 每 job `tokio::time::timeout(profile.timeout)`；超时 `code:1`+`"timeout"`。
- 背压：插件 mpsc 有界，`try_send`；满则 `submit` 立即回 `code:4`，不冻结 JS 事件循环；`enqueue` 回失败 jobId。

## 11. 安全

- 凭据仅在 `config.yaml` →（插件 cfg）`MailProfile`；**不进 JS 自省**（`op_mail_profiles` 只列 key）。
- **头注入**：CRLF 剥离 + `lettre::Address` 强校验（§10）。
- **sendRaw 冲突头**：宿主剥离原文 `From/To/Cc/Bcc/Subject`（§7）。
- **越权**：profile 级 `allowed_from`/`allowed_recipients`（宿主前置校验，§4/§10）。
- **`none` TLS**：需 `allow_none_tls: true` 且 host 为内网 CIDR，否则拒。
- **路径穿越/TOCTOU**：`ensure_within` + 复用句柄（§9）。
- **bus 反馈泄露**：payload 仅 `jobId/messageId/code`。
- **XOAuth2**：首版静态 token，刷新后做；令牌不进日志/信封。

## 12. 测试策略

- `oj-mail` 单测：`req` serde（含 `blobKey` rename）、`Message` 组装（From/To/Subject、multipart 边界、附件字节来自 `MailAttachment`）。
- 宿主 `mail.rs` 单测：CRLF 剥离、地址校验、白名单拦截、附件解析（blob mock / 临时文件 `path` + `ensure_within` 越界拒绝）、`MailAttachment` 组装、`code` 映射。
- 桥接 e2e：`smtp.mock` profile 走 lettre `FileTransport` 写 `.eml` 到临时目录（**不依赖网络**），按 `job_id` 命名防竞态；断言 `.eml` 内容与信封；`oj build`/`oj test` 冒烟。
- 模拟 SMTP server（可选）：本地 `tokio` TcpListener，断言真实投递与 5xx→`code:2`。
- 插件预检：`cargo xtask plugin mail --check`（ABI/身份/semver/符号）。

## 13. 风险 / 待决

- **新增轴零 ABI 变更**：既有插件**无需重编**；`plugin-matrix.yml` 增 `oj-mail` 构建/预检；`bin/plugins/<triple>/` 归置。
- **`probe_axes` 同步**：`AXES` / `probe_axes` 分支 / `Registrations` 字段必须同加 `"mail"`（:456 `unreachable!` 保护；漏一处即 panic 或轴不可见）。
- **rustls provider 时序**：插件 init 须先 `install_default`；验证与 `ws_client_extensions` 不 panic。
- **lettre 依赖（阶段 0 spike 定稿，方案 B）**：`lettre 0.11`（实测 0.11.23）+ `rustls = "=0.23.40"` 单一版本（无 ring）。feature 集必须为 `builder, smtp-transport, tokio1, tokio1-rustls, rustls-no-provider, webpki-roots, aws-lc-rs, hostname, pool, file-transport`——**不得用 `rustls-tls`/`tokio1-rustls-tls`**（其 `rustls-tls = ["webpki-roots","rustls","ring"]` 会强拉 `rustls/ring`，与框架 aws-lc-rs 形成双 provider）。
- **provider 安装顺序（硬约束）**：`relay()` 立即构建 ClientConfig → 插件 init 必须**先** `install_default(aws_lc_rs)` **再**建 transport。
- **`pool` 运行时约束**：lettre `pool` 在 transport `Drop` 时 `tokio::spawn` → transport 的**创建/使用/销毁都必须在该插件自己的 tokio runtime 内**（否则析构期 abort）。
- `deliver("mail.result")` 与既有 bus 订阅扇出的路由约定需对齐（宿主统一路由：存结果 + 扇出）。

## 14. 里程碑

纳入 **v0.1.20**：①`oj-plugin-ffi` 增 `MailVtable`/`MailAttachment` + `AXES`/`probe_axes`/`Registrations` 加 `"mail"` + `axis::mail`（ABI 保持 8）；②`plugins/oj-mail`（lettre + MailEngine + vtable）；③宿主 `src/bridge/mail.rs` ops + `StableState`/`Extras` 加 `mail` 字段 + `bootstrap.js` 挂载；④配置装配 + 附件宿主解析 + CRLF/白名单加固；⑤队列线程池双通道反馈 + 超时/背压/脱敏；⑥单测 + FileTransport e2e + `xtask plugin mail --check`。
