# 设计文档：lettre SMTP 绑定（v0.1.20，插件实现）

- 日期：2026-09-15（初稿）→ 2026-09-15（v2：依架构师/工程师/安全工程师三方评审修订）
- 状态：已脑暴 + 三方评审，待实现
- 实现形态：**cdylib 插件 `oj-mail`**（非内建 op），新增 `mail` 轴。

## 0. 评审修订要点（v2）

依三方专家评审，相对初稿的关键变更：
1. **改为插件实现**：新增 `mail` 轴，`oj-plugin-ffi` 增 mail vtable 并 bump `ABI_VERSION`（7→8）；宿主把插件 vtable 装配进 `StableState`，核心 `bootstrap.js` 仍挂载 `Mail`/`mail` 全局（op 委托插件 vtable）。接受 ABI bump（初稿「不做成插件」被推翻）。
2. **只进 `StableState`，不进 `ReqState`**：op 经 `state.borrow::<Arc<StableState>>()` 取用，遵循状态模型契约。
3. **队列 graceful drain**：进程退出先拒新 job、放空在途、再 drop worker 运行时，避免 oneshot 悬挂。
4. **rustls provider 时序**：插件 init 建 transport 前显式 `install_default` 加密 provider（与 ws_client_extensions 全局一致），防「no provider」panic。
5. **`ensure_within` 提为 `pub(crate)`**：供 `mail` 附件 `{path}` 钳制。
6. **安全加固**：CRLF 头注入剥离 + 地址强校验；`sendRaw` 信封字段覆盖/剥离原文冲突头；profile 级 `allowed_from`/`allowed_recipients` 白名单；bus 反馈只带 `jobId/messageId/code`；`none` TLS 需显式 `allow_none_tls` + 内网 CIDR 校验。
7. **实现细节**：`blobKey` serde rename；`headers: HashMap<String,String>`；`bus.publish` 用 `BusPayload::Json` 且 await；`MailResultStore` 限长/TTL；背压用 `try_send`。

## 1. 背景与目标

业务 handler 需对外发邮件（通知/告警/事务）。在 `oj` 运行时提供与 `json`/`http`/`fetch`/`bus` 同风格的发信能力：配置驱动多客户端、同步/异步、队列线程池消费、双通道反馈；复用 `rustls`/`blob`/`bus`/`ensure_within`/`{code,msg,data}`。

## 2. 范围（YAGNI）

**做**：多 profile（key 区分）、明文/XOAuth2、tls/starttls/none 三模式、结构化构造 + `sendRaw` 兜底、引用式三源附件、队列线程池、双通道反馈、超时/背压/脱敏、FileTransport e2e。
**不做（首版）**：bounce/webhook 追踪、DKIM/SPF 签名（交中继）、模板引擎（交 JS）、自动重试策略（仅超时+一次性发送）、per-来源令牌桶限流（全局有界队列为 v1）、XOAuth2 静默刷新（首版仅静态 token）。

## 3. 架构定位（插件）

- 新增 **cdylib 插件 `oj-mail`**（`plugins/oj-mail`），实现 `oj-plugin-ffi` 的 **`mail` 轴**。
  - `oj-plugin-ffi`：增 `MailAxis` repr(C) vtable（`send`/`send_sync`/`enqueue`/`result`/`send_raw`，异步经 `HostContext` 的 deliver 回调）；`AXES` 增 `"mail"`；**`ABI_VERSION` 7→8**（严格相等门禁，新增轴 = repr(C) 字段变更，必须 bump）。`plugin_loader` 增 `axis::mail` helper 守类型擦除配对。
- **宿主侧**（核心 `src/bridge/mail.rs`）：提供 `#[op2]` `op_mail_send/send_sync/enqueue/result/send_raw`，经 `StableState.mail` 取 `Arc<dyn MailBackend>` 调插件 vtable；`bootstrap.js` 挂载 `Mail`/`mail` 全局（与 `bus` 模式一致：`op_bus_publish` 在核心、后端来自插件）。
- **插件侧**（`oj-mail`）：持有 `lettre` 依赖；init 时由透传的 `smtp:` cfg 构建 `MailEngine`（registry + 有界队列 + worker 池 + 加密 provider install）；vtable fn 入队/投递。
- `StableState` 增字段 `mail: Option<Arc<dyn MailBackend>>`，在 `with_dbs_and_loader` 注入（首次 run 前）；**不进 `ReqState`**。

## 4. 配置形态（config.yaml）

```yaml
smtp:
  workers: 4
  queue_capacity: 256
  default:
    host: smtp.example.com
    port: 465
    tls: tls                 # tls | starttls | none
    allow_none_tls: false    # none 模式必须显式 true，且 host 须为内网 CIDR
    mechanism: login         # login | xoauth2
    user: api@x
    pass: <secret>
    # xoauth2: { client_id, client_secret, refresh_token }  # 首版仅静态，刷新后做
    timeout: 30
    allowed_from: ["noreply@x.com"]          # 发件人白名单（后缀匹配）
    allowed_recipients: ["@x.com", "@partner.com"]  # 收件人白名单
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

`oj-mail` 经插件 `cfg` 透传拿到 `smtp:` 段，init 时构建 `MailProfile`（`async: Arc<AsyncSmtpTransport<Tokio1Executor>>` + `sync: Arc<SmtpTransport>`），按 key 存 registry。

## 5. JS API（与初稿一致）

```js
globalThis.Mail = class {
  constructor(key = "default") { this.key = key; }
  send(m)     { return ops.op_mail_send(this.key, m); }
  sendSync(m) { return ops.op_mail_send_sync(this.key, m); }
  enqueue(m)  { return ops.op_mail_enqueue(this.key, m); }
  result(id)  { return ops.op_mail_result(this.key, id); }
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
await mail.sendRaw({ from, to, raw: "<rfc822>" });
const jobId = await mail.enqueue({ from, to, subject, text });
// 反馈：bus 主题 mail.result 推 { jobId, code, msg, messageId }（不含 to/subject）
```

## 6. Rust 组件

**`oj-plugin-ffi`（宿主侧契约）**
- `MailAxis` repr(C) vtable：`send(key:*const c_char, req:*const c_char, cb: DeliverFn) -> RResult`、`send_sync(...)`、`enqueue(...)`、`result(key:*const c_char, id:*const c_char, out:*mut c_char) -> RResult`、`send_raw(...)`。异步经 `HostContext` deliver 回调回传 JSON 信封。
- `AXES` 增 `"mail"`；`ABI_VERSION` 7→8；`plugin_loader::axis::mail` 配对。

**`plugins/oj-mail`（插件实现）**
- `SendRequest`（`serde`，字段 `#[serde(rename="blobKey")]` 等）：`from, to[], cc[], bcc[], subject, text?, html?, headers: HashMap<String,String>, attachments: Vec<AttachmentRef>, sync: bool`。
- `AttachmentRef`：`{ filename, mime?, blob_key?, path? }`。
- `MailProfile`：`{ async: Arc<AsyncSmtpTransport<Tokio1Executor>>, sync: Arc<SmtpTransport> }`。
- `MailEngine`：`{ registry, tx: mpsc::Sender<Job>, results: Arc<MailResultStore>, _rt: multi_thread Runtime, bus: Option<Arc<dyn Bus>> }`（引擎在插件进程内，dlopen 同一进程）。
- `Job { profile, req, sync, job_id, respond: Option<oneshot<Envelope>>, enqueue_only }`。
- `MailResultStore`：`DashMap<job_id, Envelope>` + 限长/TTL 清理。

**`src/bridge/mail.rs`（核心宿主）**
- `op_mail_send/send_sync/enqueue/result/send_raw`：`state.borrow::<Arc<StableState>>().mail` 取后端，调 vtable；`send` 经 deliver 回调 `await` 信封。
- `StableState` 增 `mail: Option<Arc<dyn MailBackend>>`（MailBackend = 包装 mail vtable 的 trait）。

## 7. 数据流

`send`：`op_mail_send` → 取 `StableState.mail` → **校验**（CRLF 剥离 + 地址强校验 + `allowed_from`/`allowed_recipients` 白名单）→ JSON → 插件 vtable `send(key, req, cb)` → worker 取 job → 组装 `Message`（附件 `blob.read(blob_key)` / `ensure_within(path)` 后 `fs::read`，复用已 canonicalize 句柄防 TOCTOU）→ `timeout(timeout, transport.send)` → deliver(信封) → op `await` 回 JS。

`enqueue`：同上加 `enqueue_only`，完成写 `results[job_id]` 并 `bus.publish("mail.result", BusPayload::Json({jobId,code,msg,messageId}))`（**不含 to/subject**）。

`sendSync`：job `sync=true`，worker 内 `spawn_blocking(move || sync_transport.send(msg))` 再 `.await`。

`sendRaw`：结构化 `from/to` 作信封；`raw` 原文在组装前**剥离 `From/To/Cc/Bcc/Subject` 头行**（防双收件人/spoof），其余正文/头保留。

## 8. 异步/线程模型与生命周期（修正）

- 引擎自起 `multi_thread` 运行时跑 worker，**不挤占 JsRuntime 的 `current_thread`**；worker 只发信 + 回写 oneshot/bus，不碰 JsRuntime（跨 runtime oneshot 回传安全，Envelope 为 Send）。
- `StableState.mail` 只注入一次（首次 run 前），请求期 op 经 `borrow` 取 `Arc`，不进 `ReqState`。
- **graceful drain**：收到停机信号 → 停止 `try_send` 接收新 job → 等待在途完成（带总超时）→ drop worker `Runtime`。避免 oneshot 悬挂与在途邮件丢失。
- **rustls provider**：插件 init 建 transport 前 `rustls::crypto::CryptoProvider::install_default(aws_lc_rs::default_provider())`（幂等），与 ws_client_extensions 全局 provider 一致，防 lettre 建 TLS panic。

## 9. 附件（引用式，Rust 读取封装）

- `{ blob_key }`：worker 内 `blob.get(blob_key).await` 取字节（本地/S3 统一），`#![serde(rename="blobKey")]`。
- `{ path }`：`ensure_within(path, project_root)`（`pub(crate)`，双侧 canonicalize 覆盖符号链接）钳制后 `fs::read`；复用已 canonicalize 句柄读取防 TOCTOU。
- `mime` 显式优先，否则按扩展名/字节嗅探；字节直接喂 lettre `Attachment`，JS 只传 key/path，**避免大 base64 穿越 JS 边界**。
- 内联大 base64 不作为首版正式源。

## 10. 错误处理 / 超时 / 背压

- lettre `Error` 映射 `code`：连接/网络→`1`、5xx→`2`、鉴权→`3`、队列满→`4`、地址/白名单校验→`5`。`data:{jobId,messageId?}`。
- **CRLF 注入防护**：`subject`/`headers`/地址入参先剥离 `\r\n`；`from/to/cc/bcc` 经 lettre `Mailbox`/`Address` 强校验，非地址即 `code:5`。
- **白名单校验**：`from` 须匹配 `allowed_from` 后缀、`to/cc/bcc` 须匹配 `allowed_recipients` 后缀，否则 `code:5`（对齐 `deps` 授权思路）。
- `msg` 脱敏：不含账号/密码/令牌/完整 SMTP 对话；bus 反馈 payload 仅 `jobId/messageId/code`。
- 每 job `tokio::time::timeout(profile.timeout)` 包裹；超时 `code:1` + `msg:"timeout"`。
- 背压：mpsc 有界，用 `try_send`；满则 `send` 直接回 `{code:4,msg:"queue full"}`，不冻结 JS 事件循环；`enqueue` 回失败 jobId。

## 11. 安全

- 密钥仅在 `config.yaml` → 插件 init `MailProfile`，经 `StableState`；**不反序列化进 JS 自省**（`op_mail_profiles` 只列 key）。
- **SMTP 头注入**：CRLF 剥离 + 地址强校验（§10）。
- **sendRaw 信封冲突**：剥离原文冲突头（§7）。
- **越权**：profile 级 `allowed_from`/`allowed_recipients` 白名单（§4/§10）。
- **`none` TLS**：需 `allow_none_tls: true` 且 host 为内网 CIDR，否则拒绝（防误配明文出网）。
- **路径穿越**：`ensure_within` 双侧 canonicalize + 复用句柄防 TOCTOU（§9）。
- **bus 反馈泄露**：payload 仅 `jobId/messageId/code`，与 `bus` 既有鉴权一致（§10）。
- **XOAuth2**：`refresh_token` 驻 `StableState` 同等于密码；首版静态 token，刷新后做，令牌不进日志/信封。

## 12. 测试策略

- `oj-mail` 单测：`SendRequest` serde（含 `blobKey` rename）、`Message` 组装断言含 From/To/Subject、multipart 边界、附件字节、CRLF 剥离、白名单拦截、`ensure_within` 路径钳制。
- 桥接 e2e（核心 + 插件协同）：`smtp.mock` profile 走 lettre `FileTransport` 写 `.eml` 到临时目录（**不依赖网络**），按 `job_id` 命名防竞态；断言 `.eml` 内容与返回信封；`oj build`/`oj test` 冒烟。
- 模拟 SMTP server（可选）：本地 `tokio` TcpListener 充当 SMTP，断言真实投递与 5xx 永久错误 `code:2` 映射。
- 复用现有 Windows 路径归一逻辑（无新增跨平台风险点）。

## 13. 风险 / 待决

- **ABI bump 7→8**：所有既有插件须同步重编；CI 插件矩阵（`plugin-matrix.yml`）须覆盖 `oj-mail`。
- **rustls provider 时序**：插件 init 须早于首次 transport 构建 install provider；与 ws_client_extensions 全局 provider 一致（验证不 panic）。
- **lettre 版本**：新增 `lettre`（含 `rustls`/`tokio1` feature），须与框架钉死 `rustls = "=0.23.40"` / `aws-lc-rs` 同 provider；实现前先最小编译验证 `lettre + rustls 0.23.40 + aws-lc-rs`。
- `bus.publish` payload 形态与 JS `bus.subscribe` 回调约定对齐（沿用既有 `bus` 全局）。
- worker `multi_thread` 运行时生命周期归属（随插件卸载/进程退出 drain，§8）。

## 14. 里程碑

纳入 **v0.1.20**：①`oj-plugin-ffi` 增 `MailAxis` + bump ABI 7→8 + `axis::mail`；②`plugins/oj-mail`（lettre + MailEngine + vtable）；③核心 `src/bridge/mail.rs` ops + `StableState.mail` 字段 + `bootstrap.js` 挂载；④配置装配 + 结构化/raw/引用式附件 + CRLF/白名单加固；⑤队列线程池双通道反馈 + 超时/背压/脱敏；⑥单测 + FileTransport e2e。
