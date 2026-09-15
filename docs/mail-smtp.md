# 邮件投递（SMTP）手册

面向 `oj` 使用者的邮件能力说明：配置、JS API、附件、反馈、安全与运维。
业务侧速查见 `docs/devkit/api-manual.md` §6「mail」；本手册是完整参考。

- 实现形态：**cdylib 插件 `oj-mail`** + 新增 `mail` 轴（非内建 op）。
- 底层：`lettre`（rustls），`rustls = "=0.23.40"`（与框架同 provider：aws-lc-rs，**不引 ring**）。
- 版本：v0.1.19 起。

## 1. 架构与职责边界

| 角色 | 职责 |
|---|---|
| **宿主（core `src/bridge/mail.rs`）** | 配置装配、`StableState.mail` 注入、JS 全局 `Mail`/`mail`、**入参校验**（CRLF/地址/白名单）、**附件字节解析**（blob/本地文件）、结果存储、`mail.result` 本地扇出 |
| **插件（`plugins/oj-mail`）** | `lettre` 连接池、**有界队列 + worker 池**、实际投递、经 `FfiFuture`/`deliver` 回传结果 |

- 契约：`oj-plugin-ffi` 的 `MailVtable { submit(key, req_json, atts) -> FfiFuture }`（repr(C)）。
- **新增轴零 ABI 变更**：`AXES` 加 `"mail"` 不 bump `ABI_VERSION`（当前 8）——既有插件无需重编。
- **插件拿不到宿主后端**（`HostContext` 只有 `log` + `deliver`）：故附件字节解析与 `bus` 发布都在**宿主**完成。

## 2. 配置（`config.yaml`）

顶层 `smtp:` 段，存在即启用 `Mail`/`mail`；**除 `workers`/`queue_capacity` 外，每个键都是一个 profile**。

```yaml
smtp:
  workers: 4              # 队列 worker 数（默认 4）
  queue_capacity: 256     # 有界队列容量（背压，默认 256）
  default:                # profile 名；Mail("default")
    host: smtp.example.com
    port: 465
    tls: tls              # tls(隐式/465) | starttls(25/587) | none
    allow_none_tls: false # tls:none 必须显式 true，否则 fail-closed
    mechanism: login      # login | xoauth2
    user: api@x
    pass: <secret>        # login 用
    # mechanism: xoauth2 时（首版仅静态 token）：
    # xoauth2: { access_token: "ya29..." }   # 只给 refresh_token → fail-loud
    timeout: 30           # 单封发送超时（秒，默认 30）
    allowed_from: ["noreply@x.com"]                  # 发件人白名单（后缀）
    allowed_recipients: ["@x.com", "@partner.com"]   # 收件人白名单（后缀）
  alerts:                 # 第二个 profile（复用同一队列/线程池）
    host: smtp.other.com
    port: 587
    tls: starttls
    mechanism: login
    user: ...
    pass: ...
    allowed_from: ["alert@x.com"]
    allowed_recipients: ["@ops.example.com"]
  mock:                   # 测试/归档：写 .eml 到目录（须先建目录）
    file_transport: /tmp/oj-mail-eml
    tls: none
    allow_none_tls: true
    mechanism: login
    allowed_from: ["noreply@x.com"]
    allowed_recipients: ["@x.com"]
```

### 门禁（fail-closed）

- **白名单**：`allowed_from`/`allowed_recipients` **空表即拒绝**（防开放中继）。缺省不放行。
- **`tls: none`**：必须显式 `allow_none_tls: true`，否则配置解析失败。
- **密钥**：只在配置文件与插件内；**不进 JS**（`Mail.profiles()` 仅列 profile 名）。
- **`file_transport` 目录须先存在**（lettre 不自动建目录）。

## 3. JS API

```js
const m = new Mail("default");      // 等价于全局 mail（mail === new Mail("default")）
await m.send({ ... });              // 异步 transport
await m.sendSync({ ... });          // 同步 transport（插件 worker 内 spawn_blocking；对 JS 同样非阻塞）
const r = await m.enqueue({ ... }); // r.data.jobId；真实完成经 bus "mail.result"
await m.result(r.data.jobId);       // 查结果（未命中/过期 → null）
await m.sendRaw({ from, to, raw }); // 原始 MIME
await Mail.profiles();              // ["default","alerts","mock"]
```

| 方法 | 说明 |
|---|---|
| `new Mail(key?)` | profile 实例，`key` 缺省 `"default"`；**未声明的 key 报错**（不回落 default） |
| `send(m)` / `sendSync(m)` | resolve 投递结果信封；区别仅内部走 async / sync transport |
| `enqueue(m)` | 入队即回 `{code:0,data:{jobId}}` |
| `result(jobId)` | 宿主侧结果（`Json \| null`） |
| `sendRaw(o)` | `raw` 原文 + 结构化 `from`/`to` 作信封（与 `attachments` 互斥） |
| `Mail.profiles()` | 已配置 profile 名清单（非密钥面） |

### 请求结构

```ts
interface MailSendRequest {
  from: string;
  to: string[];
  cc?: string[]; bcc?: string[];
  subject?: string;
  text?: string; html?: string;
  headers?: Record<string, string>;
  attachments?: Array<
    | { filename: string; mime?: string; blobKey: string }
    | { filename: string; mime?: string; path: string }
  >;
}
```

### 信封与错误码

所有方法都 resolve 信封 `{code,msg,data}`（**只有「未配置 mail」抛异常**，文案
`mail not configured (config smtp: section missing, or oj-mail plugin not loaded)`）：

| code | 含义 |
|---|---|
| `0` | 成功（`data.messageId` / `data.jobId`） |
| `1` | 连接/网络错误、超时，**以及一切投递期失败**（含 SMTP 5xx 与鉴权失败） |
| `2` / `3` | **当前未启用**：首版不细分 SMTP 5xx（`2`）与鉴权失败（`3`），两类都归 `1`——这样 `msg` 只需出脱敏分类文案（lettre 原始错误含 SMTP 对话/收件人，不进信封）。判失败请用 `code !== 0` |
| `4` | 队列满（背压；`try_send` 拒绝，不阻塞调用方） |
| `5` | 地址/入参/白名单/附件校验失败 |

## 4. 附件（引用式，字节由宿主解析）

三种来源，**均以引用传入**，宿主解析成原始字节后经 FFI 直传插件（**不走 base64 / 不经 JS**）：

| 来源 | 字段 | 说明 |
|---|---|---|
| blob 后端 | `{ filename, blobKey }` | 经 `StableState.blobs`（本地/S3）读取 |
| 本地文件 | `{ filename, path }` | 经 `ensure_within` 钳制在 **project root** 内后读盘（越界 → `code:5`） |

- `mime` 显式优先，否则按扩展名/字节嗅探。
- **下标对齐契约**：宿主按 `attachments` 顺序生成字节数组，插件按同下标取用；**数量必须一致**（不一致 → `code:5`，防错配）。
- 路径校验与读盘使用**同一 canonical 句柄**（避免 TOCTOU）。

## 5. `sendRaw` 语义（防双收件人/spoof）

- 结构化 `from`/`to` 作 **SMTP 信封**（MAIL FROM/RCPT TO），与 `raw` 头解耦。
- 剥离 `raw` 中的 `From/To/Cc/Bcc` 头（含折叠续行）。
- **保留 `raw` 的 `Subject`**；若同时给了结构化 `subject`（非空）→ 以结构化为准覆盖。
- 行尾归一 CRLF（防 SMTP smuggling）；`raw` 原文**不经 lettre 重编码**（保持字节）。
- `raw` 与 `attachments` 互斥。

## 6. 反馈通道（`enqueue`）

- `enqueue` 立即回 `{code:0,data:{jobId}}`；worker 完成后经 `HostContext.deliver("mail.result", 信封)` 上送宿主。
- 宿主：存 `MailResultStore`（**限长 + TTL**）供 `mail.result(jobId)` 查询，并向**本地 `bus`** 扇出**扁平**结果：

```js
bus.subscribe("mail.result");   // 回调收到 { jobId, code, msg, messageId }
```

- payload **不含** `to`/`subject`（脱敏，避免泄露给非预期订阅者）。
- **跨进程/分布式反馈暂不支持**：`deliver` 是同步 `extern "C"`，宿主无法在其中 `await` 分布式总线；首版仅本地扇出（见 §9）。

## 7. 安全清单

- 白名单 fail-closed（§2）；`tls:none` 显式门禁。
- **CRLF 注入**：`subject`/`headers` 剥离 CRLF；`from/to/cc/bcc` 经 `lettre::Address` **强校验**（非法即 `code:5`）。正文/`raw` 原文不剥（换行有语义）。
- **`headers` 不得覆盖** `From/To/Cc/Bcc/Subject`（否则可绕过白名单）。
- **附件路径** 经 `ensure_within`（双侧 canonicalize，覆盖符号链接）。
- 密钥不进 JS/日志；错误文案脱敏（不含账号/密码/令牌/SMTP 对话）。

## 8. 构建、加载与运维

```bash
cargo xtask plugin mail            # 构建 oj-mail 并归置 bin/plugins/<triple>/
cargo xtask plugin mail --check    # ABI/身份/semver/符号 预检
cargo xtask build                  # 构建 oj + 全部第一方插件（含 mail）
```

- 插件发现：`OJ_PLUGINS_DIR` > config `plugins_dir` > `<exe>/plugins` > `<workspace_root>/bin/plugins`，再拼 `<host-triple>/`。
- **未装插件不阻断启动**；调用 mail 时报 `mail not configured`（可选能力）。
- 多 profile 复用同一队列/线程池；`workers`/`queue_capacity` 全局，`timeout` 每 profile。
- **停机 graceful drain**：`oj server` 收到 SIGTERM/正常退出时，在 HTTP 停收 + 长任务收场**之后**
  会触发排空 —— 插件停收新投递 → 等在途 job 跑完 → 销毁 transport；总超时 **10s**，
  超时只告警（`warn: mail drain 未完成：…（在途邮件可能被丢弃）`）并不阻断进程退出。
  排空后仍在跑的 handler 再发信会拿到 `{code:1}`（文案含「停机」）。

## 9. 已知限制 / 路线

| 项 | 现状 |
|---|---|
| 跨进程（分布式 bus）`mail.result` | 仅本地扇出；需宿主持 runtime handle 后异步发布（待做） |
| XOAuth2 token 刷新 | 首版仅静态 `access_token`；`refresh_token`-only fail-loud（待做） |
| per-来源限流 | 仅全局有界队列 + `code:4`；令牌桶按模块/租户待做 |
| 自动重试 / bounce / DKIM | 不做（交中继/上层） |
| `pool` 生命周期 | lettre `pool` 在 transport 构建与 Drop 时 `tokio::spawn` → 必须全程在插件自身 runtime 内（已保证） |
| SMTP 错误细分（`code:2`/`3`） | 未启用：5xx 与鉴权失败均归 `1`（§3）。细分要解析 lettre 错误分类，且需在 `msg` 脱敏前提下做（待做） |
| `tls: none` 的「内网 CIDR」约束 | 未实现：当前只校验显式 `allow_none_tls: true`（设计 §11 曾要求 host 落在内网网段，待做） |
| 地址接受集两侧一致 | **已知分裂**：宿主（`lettre::Address`）放行而结构化路（`lettre::Mailbox`）拒绝的形态（引号本地部 `"a b"@x.com`、域字面量 `a@[127.0.0.1]`）会在投递期报 `code:5`。方向为「插件更严」，无越权面；统一解析器待做 |

## 10. 相关文档

- 业务速查：`docs/devkit/api-manual.md` §6「mail」；agent：`docs/devkit/SKILL.md`。
- 配置：`docs/user-manual.md`（`smtp:` 段）。
- 插件体系：`docs/plugin-architecture.md`、`docs/plugin-development.md`。
- 设计/实现记录：`docs/plans/2026-09-15-mail-smtp-design.md`、`docs/plans/2026-09-15-mail-smtp-impl.md`。
