# 设计文档：lettre SMTP 绑定（v0.1.20）

- 日期：2026-09-15
- 状态：已脑暴确认，待实现
- 范围：`oj` 内建 JS 全局 `Mail` / `mail`，以 `lettre` 为底层，支持第三方 SMTP 同步与异步发送、队列线程池消费、双通道反馈。

## 1. 背景与目标

业务 handler 常需对外发邮件（通知、告警、事务邮件）。目标是在 `oj` 的 JS 运行时里提供一套与现有 `json`/`http`/`fetch`/`db` 同风格的发信能力：

- 配置驱动多客户端（profile 以 `key` 区分），密钥不进 JS、不外泄；
- 同步（`sendSync`）与异步（`send`）两种发送姿态，且内部统一走「有界队列 + 工作线程池」；
- 批量高吞吐场景支持 fire-and-forget（`enqueue`）+ 事件反馈；
- 复用框架既有能力：`rustls`（TLS）、`blob` 后端（附件读取）、`bus`（结果反馈）、`ensure_within`（本地路径钳制）、`{code,msg,data}` 信封。

## 2. 范围（YAGNI）

**做**：多 profile、明文/XOAuth2 认证、tls/starttls/none 三模式、结构化构造 + `sendRaw` 兜底、引用式三源附件、队列线程池、双通道反馈、超时/背压、文件传输测试。

**不做（首版）**：投递状态追踪（bounce/webhook 解析）、DKIM/SPF 签名（交给中继）、模板引擎（交给 JS）、退信自动重试策略（仅做超时与一次性发送）、多收件人分批限流精细策略。

## 3. 架构定位

- 新增 `src/bridge/mail.rs`，用 `#[op2]` 绑定，与 `http.rs`/`fetch.rs` 同级，**不做成 cdylib 插件**。
  - 理由：SMTP 不属于现有固定轴（db/kv/blob/bus/es/auth），新轴需 bump `ABI_VERSION`，过重；内建 op 与 `http`/`fetch` 一致最自然。
- JS 全局暴露 `Mail` 构造器（用法 `mail = new Mail('default')`），并附一个绑定 `default` profile 的便利实例 `mail`。
- profile 在装配期（server_cmd assemble）实例化为传输对象，存入 `StableState.MailRegistry`（首跑后不可变，`Arc` 跨请求共享）。

## 4. 配置形态（config.yaml）

```yaml
smtp:
  workers: 4                 # 队列工作线程数（可选，默认 4）
  queue_capacity: 256       # 有界队列容量（背压，可选）
  default:
    host: smtp.example.com
    port: 465
    tls: tls                 # tls(隐式465) | starttls(25/587) | none(仅内网)
    mechanism: login         # login | xoauth2
    user: api@x
    pass: <secret>           # login 用；xoauth2 改为下方
    # xoauth2: { client_id, client_secret, refresh_token }  或 access_token
    timeout: 30              # 单封发送超时（秒，可选）
  alerts:                    # 第二套客户端，复用同一队列/线程池
    host: smtp.other.com
    port: 587
    tls: starttls
    mechanism: login
    user: ...
    pass: ...
  mock:                      # 测试用：lettre FileTransport 写 .eml 到目录
    file_transport: /tmp/oj-mail-eml
```

装配：解析为 `MailProfile { async: Arc<AsyncSmtpTransport<Tokio1Executor>>, sync: Arc<SmtpTransport> }`，按 key 存入 `MailRegistry`。

## 5. JS API

```js
globalThis.Mail = class {
  constructor(key = "default") { this.key = key; }
  send(m)      { return ops.op_mail_send(this.key, m); }        // async，内部走队列，await 结果
  sendSync(m)  { return ops.op_mail_send_sync(this.key, m); }   // 阻塞发送（仍经队列+spawn_blocking）
  enqueue(m)   { return ops.op_mail_enqueue(this.key, m); }     // fire-and-forget，返回 jobId
  result(id)   { return ops.op_mail_result(this.key, id); }     // 取 enqueue 结果
  sendRaw(o)   { return ops.op_mail_send_raw(this.key, o.from, o.to, o.raw); }
};
globalThis.mail = new Mail("default");
```

结构化发送：

```js
await mail.send({
  from: "noreply@x.com",
  to: ["a@x.com", "b@x.com"],
  cc: ["c@x.com"], bcc: [],
  subject: "hi",
  text: "plain", html: "<b>hi</b>",
  headers: { "X-Custom": "v" },
  attachments: [
    { filename: "r.pdf", blobKey: "r2d2" },   // 引用 blob 后端
    { filename: "x.pdf", path: "reports/x.pdf" } // 钳制在 project root
  ]
});
await mail.sendRaw({ from, to, raw: "<rfc822 字符串>" });
const jobId = await mail.enqueue({ from, to, subject, text });
// 反馈：bus 主题 mail.result 推 { jobId, code, msg, messageId }；或 mail.result(jobId)
```

## 6. Rust 组件

- `SendRequest`（`serde`）：`from, to[], cc[], bcc[], subject, text?, html?, headers: Map, attachments: Vec<AttachmentRef>, sync: bool`。
- `AttachmentRef`：`{ filename, mime?, blob_key?, path? }`（引用式，Rust 读取封装；不传大 base64）。
- `MailProfile`：`{ async: Arc<AsyncSmtpTransport<Tokio1Executor>>, sync: Arc<SmtpTransport> }`。
- `MailRegistry`：`HashMap<String, MailProfile>`（StableState，不可变）。
- `MailEngine`（StableState，装配期启动）：`{ tx: mpsc::Sender<Job>, results: Arc<MailResultStore>, _rt: multi_thread runtime, bus: Option<Arc<dyn Bus>> }`。
- `Job { profile, req, sync, job_id, respond: Option<oneshot<Envelope>>, enqueue_only }`。

ops：`op_mail_send`、`op_mail_send_sync`、`op_mail_enqueue`、`op_mail_result`、`op_mail_send_raw`、`op_mail_profiles`（可选自省）。

## 7. 数据流

`send`：`op_mail_send` → 构造 `Job{respond: oneshot}` → `tx.send` → `await oneshot` → 返回信封。worker 取到 job → `registry.get(profile)` → 组装 `Message`（附件按需 `blob.read(blob_key)` / `fs::read(path)`）→ `timeout(profile.timeout, transport.send(msg))` → `respond.send(envelope)`。

`enqueue`：同上加 `enqueue_only=true`，无 oneshot；完成后写 `results[job_id]` 并 `bus.publish("mail.result", envelope)`（bus 已配时）。

`sendSync`：job 标记 `sync=true`；worker 内 `tokio::task::spawn_blocking(move || sync_transport.send(msg))` 再 `.await`，避免冻结 `!Send` 的 JsRuntime。

`sendRaw`：直接把 `raw` 当 RFC822 投递（仍需 `from`/`to` 信封用于传输），不走结构化组装。

## 8. 异步/线程模型与生命周期

- worker 跑在引擎自起的 `multi_thread` 运行时，**不挤占 JsRuntime 的 `current_thread`**（worker 只发信、回写 oneshot/bus，不碰 JsRuntime）。
- 异步发送用 `AsyncSmtpTransport<Tokio1Executor>`，连接池在 transport 内部复用；同步用 `SmtpTransport`（阻塞路径经 `spawn_blocking`）。
- profile 在装配期实例化进 `StableState`，跨请求 `Arc` 共享，**不每次新建连接**；请求期 `ReqState` 持 registry / engine 的 `Arc` 克隆。
- 传输对象 `Send`，可安全跨 worker 运行时。

## 9. 附件（引用式，Rust 读取封装）

- `{ blob_key }`：调现有 `blob` 后端 `read(blob_key)` 取字节（本地/S3 统一）。
- `{ path }`：`ensure_within(path, project_root)` 钳制后 `fs::read`（对齐相对导入逃逸门禁，防越界读盘）。
- `mime` 显式优先，否则按扩展名/字节嗅探；字节直接喂 lettre `Attachment`，JS 全程只传 key/path 字符串，**避免大附件以 base64 穿越 JS 边界拖慢效率**。
- 内联大 base64 不作为首版正式源。

## 10. 错误处理 / 超时 / 背压

- lettre `Error` 映射信封 `code`：连接/网络→`1`(瞬时)、5xx→`2`(永久)、鉴权→`3`、队列满→`4`、地址校验→`5`；`data:{jobId, messageId?}`。
- `msg` 脱敏：不含账号/密码/令牌/完整 SMTP 对话。
- 每 job `tokio::time::timeout(profile.timeout)` 包裹；超时按 `code:1` + `msg:"timeout"`。
- 背压：mpsc 有界；满则 `send` 直接回 `{code:4,msg:"queue full"}`，不阻塞调用方；`enqueue` 回失败 jobId。

## 11. 安全

- 密钥仅在 `config.yaml`，经 `server_cmd` 装配进 `StableState`，**不暴露给 JS、不写日志**。
- 本地附件路径强制 `ensure_within`（project root 内）。
- 信封 `msg` 脱敏，防凭据泄漏到调用方/日志。
- TLS 默认 `rustls`（复用框架 `rustls = "=0.23.40"`，不引 openssl 原生依赖）；`none` 仅限内网可信中继，配置显式选择。

## 12. 测试策略

- 单测（`mail.rs`）：`SendRequest` serde 往返；`Message` 组装断言含 From/To/Subject、multipart 边界、附件字节；三源分派（blob mock / 临时文件 `path`）；信封映射；队列满背压。
- 桥接 e2e：用 lettre 内建 `FileTransport` 写 `.eml` 到临时目录（**不依赖网络**），断言产物内容与返回信封；加 `smtp.mock` profile 走 file transport 做 `oj build` + `oj test` 冒烟。
- 模拟 SMTP server（可选）：起本地 `tokio` TcpListener 充当 SMTP，断言 `AsyncSmtpTransport` 真实投递与 5xx 永久错误映射。
- 复用现有 Windows 路径归一逻辑（本方案不新增跨平台风险点）。

## 13. 风险 / 待决

- `lettre` 版本与 `rustls` feature 是否锁定到框架既有 `rustls 0.23.40`（实现期确认 Cargo 兼容）。
- `bus.publish` 的 payload 形态与 JS `bus.subscribe` 回调约定需对齐（沿用既有 `bus` 全局）。
- worker 的 `multi_thread` 运行时生命周期归属（随 `StableState`/进程退出时 drain 队列）。

## 14. 里程碑

纳入 **v0.1.20**：先落 `mail.rs` + bridge 注册 + 配置装配 + 结构化/raw/引用式附件 + 队列线程池 + 双通道反馈 + 单测/FileTransport e2e；XOAuth2 与多 profile 一并支持。
