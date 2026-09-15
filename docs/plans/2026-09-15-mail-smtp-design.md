# 设计文档：lettre SMTP 绑定（v0.1.19，插件实现）

- 日期：2026-09-15（初稿）→ v2（三方评审）→ v3（自洽性总检）→ **v4（实现期定稿，随阶段 0–6 落地同步）** → **v5（阶段 8 安全审计订正）**
- 状态：阶段 0–8 已实现；**v0.1.19 = 发布点**（`oj` 版本号已递增）
- 实现形态：**cdylib 插件 `oj-mail`**（非内建 op），新增 `mail` 轴。

## 0. 版本修订

**v5（阶段 8 安全回归审计实测订正；只订正文档，未改实现语义）**
- §10 的 `5xx→2`、`鉴权→3` **未实现**：实现统一归 `1`（`engine.rs:43`），已达成为 `0/1/4/5`；用户面文档（`docs/mail-smtp.md` §3、`docs/devkit/api-manual.md` §6）原宣称 `2`/`3` 可用，已同步订正。→ 待做。
- §11 的 `none` TLS「且 host 为内网 CIDR」**未实现**：只校验显式 `allow_none_tls`。→ 待做（会改配置语义，单列任务）。
- §11 新增实测结论：宿主 `lettre::Address` 与结构化路 `lettre::Mailbox` 的**接受集不互相包含**（引号本地部/域字面量：宿主放行、插件拒 → 投递期 `code:5`）。方向「插件更严」，**无越权面**；已列表化钉进回归用例。→ 统一解析器待做。
- §12 的「模拟 SMTP server 断言 5xx→`code:2`」**未做**（与上面第一条同因）。

**v4（实现期定稿；随阶段 0–6 同步，均为实测结论）**
- **lettre 依赖（阶段 0 spike）**：`lettre 0.11` + `rustls = "=0.23.40"` 单一版本、**无 ring**。feature 必须为 `builder,smtp-transport,tokio1,tokio1-rustls,rustls-no-provider,webpki-roots,aws-lc-rs,hostname,pool,file-transport`；**不得**用 `rustls-tls`（其 `= ["webpki-roots","rustls","ring"]` 会强拉 `rustls/ring` 与框架 aws-lc-rs 双 provider）。宿主侧仅用 `lettre::Address` 做校验（`default-features=false`，不引 TLS 栈）。
- **硬约束**：provider 安装**必先于**任何 transport 构建（`relay()` 立即建 ClientConfig）；`pool` 在 transport **构建期与 Drop 期**都会 `tokio::spawn` → transport 全生命周期（建/用/毁）必须在插件自身 `multi_thread` runtime 上下文内。
- **lettre 0.11.23 无 `Credentials::from_xoauth2`**：xoauth2 首版实为 `Credentials::new(user, access_token)` + `.authentication(vec![Xoauth2])`；`refresh_token`-only → fail-loud。
- **`build_raw` 不经 lettre 重编码**（`MessageBuilder::body` 在行长 ≥76 会改 QP/base64 把已编码 multipart 改烂；强制原样路径在非法编码时 `expect` panic）→ raw 路自行归一 CRLF 并原样输出。
- **raw 冲突头口径（定稿）**：剥离 `From/To/Cc/Bcc`（信封权威）；**保留 `Subject`**（Subject 非信封字段，剥离只会丢主题；注入由 CRLF 拒绝覆盖），结构化 `subject` 非空则覆盖。
- **附件对齐契约**：插件按**下标**取 `atts[i].bytes` 对应 `req.attachments[i]`，**数量必须一致**（不一致 → `code:5`）；宿主按同序解析生成 `RVec<MailAttachment>`，字节直传 `RBytes`（无 base64/JSON）。
- **宿主校验口径**：`subject`/`headers` **剥离** CRLF；`from/to/cc/bcc` 用 `lettre::Address` **拒绝**非法；正文与 `raw` 原文不剥（换行有语义）。校验失败 **resolve `{code:5}` 信封**；仅「未配置 mail」抛异常（对齐 `es not configured`）。
- **白名单 fail-closed**：`allowed_from`/`allowed_recipients` **空表即拒绝**（越权发送的唯一控制点）。
- **信封与模式**：统一 `{code,msg,data:{jobId,messageId}}`；`enqueue` 返回 `{code:0,data:{jobId}}`（非裸 jobId，JS 取 `res.data.jobId`）；**宿主按 op 覆写 `sync`/`enqueue_only`**（`send()` 夹带 `sync:true` 不生效）。JS 另有 `Mail.profiles()`（`op_mail_profiles` 的可达入口）。
- **`mail.result` 仅本地同步扇出**：`HostContext.deliver` 是同步 `extern "C"`（不能 await）→ 宿主仅做本地 `bus` 订阅扇出；**跨进程/分布式反馈显式延后**（见 §13）。
- **`ensure_within` 提 `pub(crate)` 且返回 canonical 路径**：附件 `{path}` 用**返回的 canonical 句柄** `fs::read`（校验路径 ≡ 读盘路径，避免 TOCTOU）。
- **`FfiFuture` 驱动复用** `ffi::await_ffi`（与 `FfiEsBackend`/`FfiBlobBackend`/`FfiEventBroker` 同一 poll+yield 驱动），未引入新的跨线程手段。

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
    # mechanism: xoauth2 时——首版仅静态 token：需 user + xoauth2.access_token
    # xoauth2: { access_token: "ya29..." }   # 只给 refresh_token → fail-loud（刷新后做）
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
- **阶段 7 实测订正**：上面 `mock:` 样例省了 `host`/`port`/`tls`/`mechanism`，但插件
  `ProfileCfg` 这四个字段**必填**（`file_transport` 走同一 `Deserialize`）——`smtp.mock`
  必须写全（可照 `sample/config.yaml`；`tls: none` 仍须 `allow_none_tls: true`，只是不联网）。
  另 `file_transport` 目录**须先存在**（lettre 不建目录）。
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
const r = await mail.enqueue({ from, to, subject, text });
const jobId = r.data.jobId;                       // 统一信封：{code,msg,data:{jobId}}
// 反馈：本地 bus 主题 mail.result 推扁平 { jobId, code, msg, messageId }（不含 to/subject）
// 另有 mail.result(id) 查结果、Mail.profiles() 列 profile key。
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

`sendRaw`：结构化 `from/to` 作信封（权威，防双收件人/spoof）；**剥离 `raw` 中 `From/To/Cc/Bcc` 头行**（信封与原文解耦）。**保留 `raw` 的 `Subject`**（Subject 非信封字段，剥离只会丢主题；注入风险由 CRLF 拒绝覆盖）；若同时给了结构化 `subject`（非空）→ 以结构化为准覆盖原文 Subject。其余正文/头保留，行尾归一 CRLF（防 SMTP smuggling）。

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

> **实现期订正（阶段 8 审计，2026-09-15）**：本节的 `5xx→2`、`鉴权→3` **未实现**。
> 实现把「连接/网络/超时/一切投递期失败」统一归 `1`（`CODE_NETWORK`，见
> `plugins/oj-mail/src/engine.rs:43` 的常量注释），故 `2`/`3` 当前**不可达**。取舍理由：
> 失败信封只出脱敏分类文案（lettre 原始错误含 SMTP 对话/收件人，不进信封/总线），
> 细分 5xx 与鉴权需先解析 lettre 错误分类且仍要维持脱敏——归为**待做**（单列任务）。
> 已同步订正用户面文档：`docs/mail-smtp.md` §3、`docs/devkit/api-manual.md` §6
> （原表宣称 `2`/`3` 可用，与实现不符）。已达成的映射为 `0` 成功 / `1` 连接·网络·超时·
> 投递失败 / `4` 队列满 / `5` 入参·白名单·附件校验（阶段 8 已逐条补回归。

> **实现期订正（统一审查批次 B，2026-09-15）**：
> 1. 本节「`code`」表仍未变的映射见上；另**明确**：`code != 0` **一律不得自动重试**
>    —— `2`/`3` 未实现时，永久失败（5xx/鉴权）与瞬时失败（连接/超时）同归 `1`，
>    从 `code` 分不出可重试性（未新增 `data.retryable`，理由见 `docs/mail-smtp.md` §3）。
> 2. 「白名单（fail-closed）」的**匹配语义**（B1）由「后缀匹配」订正为**全等**：
>    条目只能是完整地址（地址全等）或 `@domain`（域全等），大小写不敏感，**不做子域通配**；
>    条目格式在装配期校验（空串/裸域/首尾空白 → 配置解析失败），空表仍 = 拒绝。
> 3. 新增**附件上限**（B2）：单件 `smtp.max_attachment_bytes`（默认 10 MiB）与单封合计
>    `smtp.max_total_attachment_bytes`（默认 25 MiB），超限 `code:5`；`path` 路先取长度再读，
>    读盘改为 `tokio::fs`（不阻塞 isolate）。原设计 §9 只说「字节由宿主解析」，未设上限。
> 4. 背压文案与队列语义不变（A 批次已订正）。

- `code`：连接/网络→`1`、5xx→`2`（**未实现**）、鉴权→`3`（**未实现**）、队列满→`4`、地址/白名单校验→`5`。`data:{jobId,messageId?}`。
- **CRLF 注入防护**：`subject`/`headers`/地址先剥 `\r\n`；`from/to/cc/bcc` 经 `lettre::Address` 强校验，非法即 `code:5`。
- **白名单（fail-closed）**：`from` 匹配 `allowed_from` 后缀、`to/cc/bcc` 匹配 `allowed_recipients` 后缀，否则 `code:5`；**空表即拒绝**（缺省不放行）。
- `msg` 脱敏（无账号/密码/令牌/SMTP 对话）；bus 反馈仅 `jobId/messageId/code`。
- 每 job `tokio::time::timeout(profile.timeout)`；超时 `code:1`+`"timeout"`。
- 背压：插件 mpsc 有界，`try_send`；满则 `submit` 立即回 `code:4`，不冻结 JS 事件循环；`enqueue` 回失败 jobId。

## 11. 安全

> **实现期订正（阶段 8 审计，2026-09-15）**：以下两条与实现不一致，均**登记为待做**，
> 本版**不改实现语义**（阶段 8 硬约束：只做用例审计补缺 + 门禁 + 版本递增）：
> 1. `none` TLS 的「**且 host 为内网 CIDR**」**未实现**：只校验显式 `allow_none_tls: true`
>    （`plugins/oj-mail/src/config.rs::ProfileCfg::validate`，`tls: none` 无许可即拒）。
>    落地 CIDR 需引入 IP/CIDR 解析并处理域名（`localhost`）解析口径，会改配置语义，单列任务。
> 2. **地址接受集两侧不互相包含**（阶段 8 新增对账用例实测）：宿主 `lettre::Address` 放行而
>    结构化路 `lettre::Mailbox` 拒绝的形态（引号本地部、域字面量）会在投递期报 `code:5`。
>    方向为「插件比宿主更严」→ **无越权面**（信封派生的地址必然先过宿主白名单与后缀匹配）；
>    只是对用户表现为无理由的 `code:5`。raw 路与宿主**逐条一致**（同为 `Address`）。
>    已钉进回归：`plugins/oj-mail/src/message.rs` 的
>    `host_accepted_addresses_are_accepted_by_plugin_or_listed_as_known_split`。

> **实现期订正（统一审查批次 B，2026-09-15）**：本节的安全要求按 B 批次落地/收窄：
> 1. **白名单**（本节「越权」条）：由「后缀匹配」改为**全等**（完整地址全等 / `@domain`
>    域全等，无子域通配）；条目格式装配期 fail-fast（B1）。
> 2. **头注入**（本节「头注入」条）：正文/`raw` 的**裸 CR 一律归一 CRLF**（B4，与裸 LF
>    同处置）—— 裸 CR 是 SMTP smuggling 半开面（`X\r.\r\n` 可让对端提前结束 DATA）；
>    `raw` **头区**的裸 CR 仍**拒绝**（归一 = 凭空造头）。
> 3. **`headers` 禁覆盖清单**（本节「头注入」条）由 From/To/Cc/Bcc/Subject 扩为含
>    `Sender`/`Return-Path`/`Reply-To`（B5）；`raw` 与结构化 `headers` 由「静默忽略」
>    改 fail-loud。
> 4. **结果通道**（本节「bus 反馈泄露」条）：jobId 由宿主生成（随机前缀，不可猜）、
>    调用方传入值剥离；宿主先登记票、插件只能填充一次（不可覆写）；`mail.result` 按
>    「profile + 模块 + 租户」归属过滤（B3）。**残留限制**：bus 扇出仍对所有订阅者可见
>    （payload 已脱敏）+ 归属拿不到「具体 handler/用户」粒度 —— 见 `docs/mail-smtp.md` §9。
> 5. **`none` TLS 的内网 CIDR**（本节末条）：仍未实现；**缓解** = 启动时对每个走网络的
>    明文 profile 打 warn 级告警（经 `HostContext.log`，含 profile 与 host:port）（B6）。

- 凭据仅在 `config.yaml` →（插件 cfg）`MailProfile`；**不进 JS 自省**（`op_mail_profiles` 只列 key）。
- **头注入**：CRLF 剥离 + `lettre::Address` 强校验（§10）。
- **sendRaw 冲突头**：剥离原文 `From/To/Cc/Bcc`（信封权威）；`Subject` 保留但做 CRLF 校验，结构化 `subject` 非空则覆盖（§7）。
- **越权**：profile 级 `allowed_from`/`allowed_recipients`（宿主前置校验，§4/§10）。
- **`none` TLS**：需 `allow_none_tls: true` 且 host 为内网 CIDR（**后半句未实现**，见上），否则拒。
- **路径穿越/TOCTOU**：`ensure_within` + 复用句柄（§9）。
- **bus 反馈泄露**：payload 仅 `jobId/messageId/code`。
- **XOAuth2**：首版静态 token，刷新后做；令牌不进日志/信封。

## 12. 测试策略

- `oj-mail` 单测：`req` serde（含 `blobKey` rename）、`Message` 组装（From/To/Subject、multipart 边界、附件字节来自 `MailAttachment`）。
- 宿主 `mail.rs` 单测：CRLF 剥离、地址校验、白名单拦截、附件解析（blob mock / 临时文件 `path` + `ensure_within` 越界拒绝）、`MailAttachment` 组装、`code` 映射。
- 桥接 e2e：`smtp.mock` profile 走 lettre `FileTransport` 写 `.eml` 到临时目录（**不依赖网络**），按 `job_id` 命名防竞态；断言 `.eml` 内容与信封；`oj build`/`oj test` 冒烟。
- 模拟 SMTP server（可选，**未做**：见 §0 v5——`5xx→code:2` 未实现）：本地 `tokio` TcpListener，断言真实投递与 5xx→`code:2`。
- 插件预检：`cargo xtask plugin mail --check`（ABI/身份/semver/符号）。

## 13. 风险 / 待决

- **新增轴零 ABI 变更**：既有插件**无需重编**；`plugin-matrix.yml` 增 `oj-mail` 构建/预检；`bin/plugins/<triple>/` 归置。
- **`probe_axes` 同步**：`AXES` / `probe_axes` 分支 / `Registrations` 字段必须同加 `"mail"`（:456 `unreachable!` 保护；漏一处即 panic 或轴不可见）。
- **rustls provider 时序**：插件 init 须先 `install_default`；验证与 `ws_client_extensions` 不 panic。
- **lettre 依赖（阶段 0 spike 定稿，方案 B）**：`lettre 0.11`（实测 0.11.23）+ `rustls = "=0.23.40"` 单一版本（无 ring）。feature 集必须为 `builder, smtp-transport, tokio1, tokio1-rustls, rustls-no-provider, webpki-roots, aws-lc-rs, hostname, pool, file-transport`——**不得用 `rustls-tls`/`tokio1-rustls-tls`**（其 `rustls-tls = ["webpki-roots","rustls","ring"]` 会强拉 `rustls/ring`，与框架 aws-lc-rs 形成双 provider）。
- **provider 安装顺序（硬约束）**：`relay()` 立即构建 ClientConfig → 插件 init 必须**先** `install_default(aws_lc_rs)` **再**建 transport。
- **`pool` 运行时约束**：lettre `pool` 在 transport `Drop` 时 `tokio::spawn` → transport 的**创建/使用/销毁都必须在该插件自己的 tokio runtime 内**（否则析构期 abort）。
- `deliver("mail.result")` 与既有 bus 订阅扇出的路由约定需对齐（宿主统一路由：存结果 + 本地扇出）。
- **跨进程/分布式反馈（延后）**：`HostContext.deliver` 是同步 `extern "C"`，宿主无法在其中 `await` 分布式 `EventBroker::publish`；首版仅本地 `bus` 扇出。若需跨进程 `mail.result`，后续另立方案（如宿主持 runtime handle 后 `spawn` 异步发布）。

## 14. 里程碑

纳入 **v0.1.19**：①`oj-plugin-ffi` 增 `MailVtable`/`MailAttachment` + `AXES`/`probe_axes`/`Registrations` 加 `"mail"` + `axis::mail`（ABI 保持 8）；②`plugins/oj-mail`（lettre + MailEngine + vtable）；③宿主 `src/bridge/mail.rs` ops + `StableState`/`Extras` 加 `mail` 字段 + `bootstrap.js` 挂载；④配置装配 + 附件宿主解析 + CRLF/白名单加固；⑤队列线程池双通道反馈 + 超时/背压/脱敏；⑥单测 + FileTransport e2e + `xtask plugin mail --check`。
