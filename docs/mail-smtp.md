# 邮件投递（SMTP）手册

面向使用者的邮件能力说明：配置、JS API、附件、反馈、安全与运维。
业务速查见 `docs/devkit/api-manual.md` §6「mail」；本手册是完整参考。

- 实现形态：**cdylib 插件 `oj-mail`** + 新增 `mail` 轴（非内建 op）。
- 底层：`lettre`（rustls），`rustls = "=0.23.40"`，与框架同 provider（aws-lc-rs，**不引 ring**）。
- 版本：v0.1.19 起。

## 1. 架构与职责边界

| 角色 | 职责 |
|---|---|
| **宿主（core `src/bridge/mail.rs`）** | 配置装配、`StableState.mail` 注入、JS 全局 `Mail`/`mail`、**入参校验**（CRLF/地址/白名单）、**附件字节解析**（blob/本地文件）、结果存储与归属、`mail.result` 本地扇出、停机排空触发 |
| **插件（`plugins/oj-mail`）** | `lettre` 连接池、**有界队列 + worker 池**、实际投递，经 `FfiFuture` / `HostContext.deliver` 回传结果 |

- 契约：`oj-plugin-ffi` 的 `MailVtable { submit(key, req_json, atts) -> FfiFuture }`（repr(C)）。
- **新增轴零 ABI 变更**：`AXES` 加 `"mail"` 不 bump `ABI_VERSION`（当前 8）——既有插件无需重编。
- **插件拿不到宿主后端**（`HostContext` 只有 `log` + `deliver`）：故附件字节解析与 `bus` 发布都在**宿主**完成。

## 2. 配置（`config.yaml`）

顶层 `smtp:` 段，存在即启用 `Mail`/`mail`。段内分两类键：

- **全局键**（4 个，固定含义）：`workers`、`queue_capacity`、`max_attachment_bytes`、`max_total_attachment_bytes`；
- **其余每个键都是一个 profile**（键名即 `Mail(key)` / `mail.send` 的 profile 名）。

```yaml
smtp:
  workers: 4                      # 队列 worker 数（默认 4）
  queue_capacity: 256             # 有界队列容量（背压，默认 256）
  max_attachment_bytes: 10485760        # 单附件上限（字节，默认 10 MiB）
  max_total_attachment_bytes: 26214400  # 单封合计上限（字节，默认 25 MiB）
  default:                        # profile 名
    host: smtp.example.com
    port: 465
    tls: tls                      # tls(隐式/465) | starttls(25/587) | none
    allow_none_tls: false         # tls:none 必须显式 true，否则 fail-closed
    mechanism: login              # login | xoauth2
    user: api@x
    pass: <secret>                # login 用
    # mechanism: xoauth2 时（静态 token）：
    # xoauth2: { access_token: "ya29..." }   # 只给 refresh_token → fail-loud
    timeout: 30                   # 单封发送超时（秒，默认 30）
    allowed_from: ["noreply@x.com"]                  # 发件人白名单（完整地址 = 全等）
    allowed_recipients: ["@x.com", "@partner.com"]   # 收件人白名单（@domain = 域全等）
  alerts:                         # 第二个 profile（复用同一队列/线程池）
    host: smtp.other.com
    port: 587
    tls: starttls
    mechanism: login
    user: ...
    pass: ...
    allowed_from: ["alert@x.com"]
    allowed_recipients: ["@ops.example.com"]
  mock:                           # 测试/归档：写 .eml 到目录（目录须先存在）
    file_transport: /tmp/oj-mail-eml
    tls: none
    allow_none_tls: true
    mechanism: login
    allowed_from: ["noreply@x.com"]
    allowed_recipients: ["@x.com"]
```

### 门禁（fail-closed）

- **白名单**：`allowed_from`/`allowed_recipients` **空表即拒绝**（防开放中继）；语义见下。
- **`tls: none`**：必须显式 `allow_none_tls: true`，否则配置解析失败。
- **双配置源互斥**：顶层 `smtp:` 与**非空** `plugins.mail` 不得同时出现（后者会静默胜出，让 `smtp:` 的改动不生效）→ 装配期报错。`plugins: {mail: {}}`（空对象）不算冲突，它是「回落 `smtp:` 适配器」的写法。
- **附件上限两个键**：必须为正整数（0 / 负数 / 字符串在启动期即报错，不静默取默认）。
- **密钥**只在配置文件与插件内，**不进 JS**（`Mail.profiles()` 仅列 profile 名）。
- **`file_transport` 目录须先存在**（lettre 不自动建目录）。

### 白名单语义（**全等**匹配，无子域通配）

白名单是「越权发送」的**唯一**控制点，条目一律**全等**比较（大小写不敏感）：

| 条目写法 | 语义 | 命中 | 不命中 |
|---|---|---|---|
| `noreply@x.com` | 完整地址：与地址**全等** | `noreply@x.com`、`NoReply@X.COM` | `evil-noreply@x.com`（同域仿冒） |
| `@x.com` | `@domain`：收件人**域全等** | `a@x.com` | `a@sub.x.com`、`a@evilx.com` |

- **子域不通配**：`@x.com` 只覆盖本域，子域须显式写 `@sub.x.com`（反向亦不覆盖父域）。
- **条目格式**（装配期校验，写错即启动失败而非静默不命中）：非空、无首尾空白，且必须是「完整地址（含 `@`）」或「`@domain`」之一。`""`、`x.com`（裸域）、`"@"`、`" a@x.com"` 均被拒；错误文案点名 `smtp.<profile>.<字段>[<下标>]` 与下一步。
- `allowed_from` 只比 `from`；`allowed_recipients` 比 `to ∪ cc ∪ bcc` 的**每一个**地址。

## 3. JS API

```js
const m = new Mail("default");      // 等价于全局 mail（mail === new Mail("default")）
await m.send({ ... });              // 异步 transport（对 JS 非阻塞）
await m.sendSync({ ... });          // 同步 transport（worker 内 spawn_blocking；对 JS 同样非阻塞）
const r = await m.enqueue({ ... }); // 入队即回信封；r.data.jobId
await m.result(r.data.jobId);       // 查结果（未命中/过期/非本归属 → null）
await m.sendRaw({ from, to, raw }); // 原始 MIME
await Mail.profiles();              // ["default","alerts","mock"]
```

| 方法 | 说明 |
|---|---|
| `new Mail(key?)` | profile 实例，`key` 缺省 `"default"`；**未声明的 key 报错**（不回落 default） |
| `send(m)` / `sendSync(m)` | resolve 投递结果信封；区别仅内部走 async / sync transport |
| `enqueue(m)` | 入队即回 `{code:0,data:{jobId}}`；jobId 由**宿主**生成，调用方传入值被剥离 |
| `result(jobId)` | 宿主侧结果（`Json \| null`）；**只回本归属**（profile + 模块 + 租户） |
| `sendRaw(o)` | `raw` 原文 + 结构化 `from`/`to` 作信封（与 `attachments`/`headers` 互斥） |
| `Mail.profiles()` | 已配置 profile 名清单（非密钥面） |

请求里的 `sync`/`enqueue_only` **由宿主按调用方法覆写**，JS 侧填了也无效。

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
    | { filename: string; mime?: string; blobKey: string }   // blob 后端
    | { filename: string; mime?: string; path: string }      // 项目根内文件
  >;
}
```

### 信封与错误码

所有方法都 resolve 信封 `{code,msg,data}`；**只有「未配置 mail」抛异常**
（`mail not configured (config smtp: section missing, or oj-mail plugin not loaded)`）。

| code | 含义 |
|---|---|
| `0` | 成功（`data.messageId` / `data.jobId`） |
| `1` | 连接/网络错误、超时，**以及一切投递期失败**（含 SMTP 5xx 与鉴权失败） |
| `2` / `3` | **当前未启用**：不细分 5xx（`2`）与鉴权失败（`3`），二者都归 `1`——`msg` 只出脱敏分类文案（lettre 原始错误含 SMTP 对话/收件人，不进信封）。判失败请用 `code !== 0` |
| `4` | 队列满（背压，`try_send` 拒绝，不阻塞调用方） |
| `5` | 地址/入参/白名单/附件校验失败 |

> **`code !== 0` 一律不得自动重试**：`2`/`3` 未实现、与 `1` 合并为同码，故无法从 `code` 区分永久与瞬时失败；自动重试会把「收件人不存在」这类永久失败反复重投。需要重试请显式判定（如仅对 `code:1` 且确认瞬时性），并自行做幂等（重发会产生新 `jobId`，`mail.result` 不替你合并）。

## 4. 附件（引用式，字节由宿主解析）

以引用传入，宿主解析成原始字节后经 FFI 直传插件（**不走 base64 / 不经 JS**）：

| 来源 | 字段 | 说明 |
|---|---|---|
| blob 后端 | `{ filename, blobKey }` | 经 `StableState.blobs`（本地/S3）读取 |
| 本地文件 | `{ filename, path }` | 经 `ensure_within` 钳制在 **project root** 内后读盘（越界 → `code:5`） |

- `mime` 显式优先，否则按扩展名/字节嗅探。
- **下标对齐**：宿主按 `attachments` 顺序生成字节数组，插件按同下标取用；**数量必须一致**（不一致 → `code:5`，防错配）。
- 路径校验与读盘使用**同一 canonical 句柄**（避免 TOCTOU）。
- **读盘不阻塞 isolate**：走 `tokio::fs`（内部 `spawn_blocking`）；`path` 路先取文件长度，超限文件**不会被读进内存**。

### 大小上限（超限 → `code:5`）

| 配置键（`smtp:` 顶层） | 默认 |
|---|---|
| `max_attachment_bytes` | 10 MiB（单个附件） |
| `max_total_attachment_bytes` | 25 MiB（单封信合计） |

超限文案点名哪个附件、两侧字节数与对应配置键。**为什么必须有**：附件字节由宿主读盘后经**有界队列**（默认容量 256）交给插件，无上限时 project root 内任意大文件（含 `config.yaml` 里的 `jwt_secret` / smtp 口令）都能被一次调用读入内存并被队列放大成内存 DoS。

## 5. `sendRaw` 语义（防双收件人/spoof）

- 结构化 `from`/`to` 作 **SMTP 信封**（MAIL FROM/RCPT TO），与 `raw` 头解耦。
- 剥离 `raw` 中的 `From/To/Cc/Bcc/Sender/Return-Path` 头（含折叠续行）。
- **保留 `raw` 的 `Subject`**；若同时给了结构化 `subject`（非空）→ 以结构化为准覆盖。
- 行尾归一 CRLF（防 SMTP smuggling）；`raw` 原文**不经 lettre 重编码**（保持字节）。
- `raw` 与 `attachments`、`headers` **互斥**（同时给 → `code:5`，不静默忽略）。

## 6. 反馈通道（`enqueue`）

- `enqueue` 立即回 `{code:0,data:{jobId}}`；worker 完成后经 `HostContext.deliver("mail.result", 信封)` 上送宿主。
- **jobId 由宿主生成**（随机前缀 + 单调计数）：调用方自带值一律被剥离，回执里的 `jobId` 也钉成宿主值 —— jobId **不可猜**。
- **归属校验**：宿主在 `submit` 之前为该 jobId 登记「profile + 模块 + 租户」的票；插件上送只能**填充**这张票（不能新建、不能覆写），`mail.result(jobId)` 只把结果显示给**同一归属**的调用方（换 profile / 换模块 / 换租户 → `null`，不泄露存在性）。
- 结果存 `MailResultStore`（**限长 1024 + TTL 1 小时**，惰性清理）；并向**本地 `bus`** 扇出扁平结果：

```js
bus.subscribe("mail.result");   // 回调收到 { jobId, code, msg, messageId }
```

- payload **不含** `to`/`subject`（脱敏）。
- **跨进程/分布式反馈不支持**：`deliver` 是同步 `extern "C"`，宿主无法在其中 `await` 分布式总线；首版仅本地扇出。

## 7. 安全清单

- **白名单 fail-closed + 全等匹配**（§2）：空表拒绝；完整地址只命中自身；`@domain` 只命中该域；非法条目装配期即拒。
- **头注入**：`subject`/`headers` 剥离 CRLF；`from/to/cc/bcc` 经 `lettre::Address` **强校验**（非法 → `code:5`）；正文/`raw` 原文不剥（换行有语义）。
- **`headers` 不得覆盖** `From/To/Cc/Bcc/Subject/Sender/Return-Path/Reply-To`（否则可绕过白名单）。
- **结果通道**：jobId 宿主生成且不可猜、调用方传入值被剥离；宿主先登记票、插件只能填充一次（**不可覆写**，杜绝把他人结果改成 `code:0`）；`mail.result` 按归属过滤。
- **附件**：路径经 `ensure_within`（双侧 canonicalize，覆盖符号链接）；单件/单封两道大小上限。
- **密钥**不进 JS/日志；错误文案脱敏；白名单未命中**不回显**白名单内容（只给 profile 名）。

## 8. 构建、加载与运维

```bash
cargo xtask plugin mail            # 构建 oj-mail 并归置 bin/plugins/<triple>/
cargo xtask plugin mail --check    # ABI/身份/semver/符号 预检
cargo xtask build                  # 构建 oj + 全部第一方插件（含 mail）
```

- 插件发现：`OJ_PLUGINS_DIR` > config `plugins_dir` > `<exe>/plugins` > `<workspace_root>/bin/plugins`，再拼 `<host-triple>/`。
- **未装插件不阻断启动**；调用 mail 时报 `mail not configured`（可选能力）。
- 多 profile 复用同一队列/线程池；`workers`/`queue_capacity`/上限为全局，`timeout` 每 profile。
- **停机 graceful drain**：`oj server` 收到停机信号后（HTTP 停收 + 长任务收场之后）触发排空 —— 插件停收新投递 → 等在途 job 跑完 → 销毁 transport；**总超时 10s**，超时只告警（`warn: mail drain 未完成…（在途邮件可能被丢弃）`）并不阻断退出。排空后仍在跑的 handler 再发信会拿到 `{code:1}`（文案含「停机」）。
- `tls: none` 且走网络的 profile，启动时会打 **warn 级**告警（含 profile 名与 host:port）；`file_transport` profile 不告警。

## 9. 已知限制

| 项 | 现状 |
|---|---|
| 跨进程（分布式 bus）`mail.result` | 仅本地扇出（§6） |
| XOAuth2 token 刷新 | 仅静态 `access_token`；`refresh_token`-only fail-loud |
| per-来源限流 | 仅全局有界队列 + `code:4`；按模块/租户的令牌桶未做 |
| 结果归属粒度 | 归属 = profile + 模块名 + 租户 id（宿主 `ReqState` 能拿到的全部）。拿不到「具体 handler / 用户」，故同模块内不同 handler 视为同一归属；**跨模块回查不支持**（跨模块通知请用 `bus.subscribe("mail.result")`）。无模块上下文/未启用租户时归属退化为「profile 相同」 |
| `bus` 订阅面隔离 | 未做：任何订阅者都会收到所有归属的结果（payload 已脱敏、jobId 不可猜）。按租户/模块分 topic 未做 |
| SMTP 错误细分（`code:2`/`3`） | 未启用，5xx 与鉴权失败均归 `1`（§3） |
| `tls: none` 内网 CIDR 约束 | 未实现（仅校验显式开关 + 启动告警） |
| 地址接受集两侧一致 | 宿主（`lettre::Address`）放行而结构化路（`lettre::Mailbox`）拒绝的形态（引号本地部 `"a b"@x.com`、域字面量 `a@[127.0.0.1]`）会在投递期报 `code:5`。方向为「插件更严」，无越权面 |
| 附件字节内存峰值 | 上限只约束单件/单封；`blobKey` 路由后端取字节时仍会先分配整块（后端无 size 接口） |
| 自动重试 / bounce / DKIM | 不做（交中继/上层）；`code≠0` 一律不得自动重试（§3） |
| `pool` 生命周期 | lettre `pool` 在 transport 构建与 Drop 时 `tokio::spawn` → 必须全程在插件自身 runtime 内（已保证） |

## 10. 相关文档

- 业务速查：`docs/devkit/api-manual.md` §6「mail」；agent：`docs/devkit/SKILL.md`。
- 配置：`docs/user-manual.md`（`smtp:` 段）。
- 插件体系：`docs/plugin-architecture.md`、`docs/plugin-development.md`。
- 设计与实现记录：`docs/plans/2026-09-15-mail-smtp-design.md`、`docs/plans/2026-09-15-mail-smtp-impl.md`。
