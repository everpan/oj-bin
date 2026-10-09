# mail —— SMTP 邮件投递（`Mail` 类与 `mail` 默认实例）

> 本文档由 src/bridge/ 梳理生成；权威 API 参考见 ../devkit/api-manual.md。

## 是什么

`Mail` 类与 `mail` 默认实例（`mail === new Mail("default")`）为 handler 提供
SMTP 邮件投递。职责边界：**宿主**（oj 进程，`src/bridge/mail.rs`）负责配置装配、
入参校验（CRLF 剥离 / 地址强校验 / 收发白名单）、附件字节解析、异步结果存储；
**`oj-mail` 插件**负责 lettre 连接池、有界队列 + worker 池与真实投递。
启用条件：config 有顶层 `smtp:` 段 + oj-mail 插件已装配。

**信封模型**：所有方法都 resolve `{code,msg,data}` 信封——校验失败（`code:5`）、
投递失败（`code:1`）、队列满（`code:4`）**都不抛异常**；唯一抛异常的场景是
「未配置 mail」。判断失败用 `code !== 0`（`2`/`3` 为保留码，未启用）。

## 配置

顶层 `smtp:` 段，段内分两类键：

- **全局键（4 个）**：`workers`（默认 4）、`queue_capacity`（默认 256）、
  `max_attachment_bytes`（单附件上限，默认 10 MiB）、
  `max_total_attachment_bytes`（单封附件合计上限，默认 25 MiB）。
- **其余每个键 = 一个 profile**（键名即 `new Mail(key)` 的 key）。

```yaml
smtp:
  workers: 4
  queue_capacity: 256
  max_attachment_bytes: 10485760        # 单附件上限（字节）
  max_total_attachment_bytes: 26214400  # 单封附件合计上限（字节）
  default:                              # ← profile 名
    host: smtp.example.com
    port: 465
    tls: tls                 # tls（隐式 TLS）| starttls | none（须 allow_none_tls: true）
    mechanism: login         # login（user + pass）| xoauth2（user + xoauth2.access_token）
    user: api@example.com
    pass: "ENC[...]"         # 凭据只走 config → 插件，不进 JS
    timeout: 30              # 单封超时（秒）
    allowed_from: ["noreply@x.com"]               # 发件人白名单（全等，空表 = 全拒）
    allowed_recipients: ["@x.com", "@partner.com"] # 收件人白名单（同上）
  mock:                      # 本地落盘通道：不发网络（目录须先存在），测试/归档用
    host: localhost
    port: 25
    tls: none
    allow_none_tls: true
    mechanism: login
    file_transport: "/tmp/oj-mail-eml"
    allowed_from: ["noreply@x.com"]
    allowed_recipients: ["@x.com"]
```

- 白名单条目两种合法写法：完整地址（`noreply@x.com`，只命中自身）或 `@domain`
  （域全等，**不**含子域）。**空表 = 拒绝**（fail-closed）。条目格式非法
  （空串/裸域/首尾空白）→ 装配期报错。
- `tls: none` 走网络必须显式 `allow_none_tls: true`（fail-closed）。

## API

| 函数 | 签名 | 说明 |
|---|---|---|
| `new Mail(key)` | `Mail(key?: string)` | profile 实例；`key` = `smtp:` 段的 profile 名（缺省 `"default"`）；未声明的 key 在调用时回 `code:5`（不回落 default） |
| `send` | `send(m: SendRequest): Promise<Envelope>` | 异步 transport，resolve 即投递结果（`sync:false`） |
| `sendSync` | `sendSync(m: SendRequest): Promise<Envelope>` | 同步 transport（插件 worker 内 `spawn_blocking`，`sync:true`） |
| `enqueue` | `enqueue(m: SendRequest): Promise<Envelope>` | 入队即回 `{code:0,data:{jobId}}`（`enqueue_only:true`）；真实完成经 `result()` / bus `mail.result` 上送。**jobId 由宿主生成**，调用方传入值被剥离 |
| `result` | `result(jobId: string): Promise<Json \| null>` | 查异步投递结果；未命中/已过期/**非本归属** → `null`。命中返回扁平 `{jobId, code, msg, messageId?}`（不含收件人/主题） |
| `sendRaw` | `sendRaw(o: SendRawRequest): Promise<Envelope>` | 原始 MIME（`raw` 原文 + 结构化 `from`/`to` 作信封）；与 `attachments`/`headers` 互斥 |
| `Mail.profiles` | `Mail.profiles(): Promise<string[]>` | 已配置 profile 名清单（**非密钥面**，只列名字） |

`SendRequest` 字段：`from`（必填）、`to`（至少一个）/ `cc` / `bcc`、
`subject`（换行被**剥离**）、`text` / `html`（至少给一个，或带附件）、
`headers`（自定义报头；**不得**覆盖 From/To/Cc/Bcc/Subject/Sender/Return-Path/Reply-To）、
`attachments`（`[{filename, blobKey|path, mime?}]`，`blobKey` 走 blob 注册表、
`path` 必须在项目根内）。`sync` / `enqueue_only` 字段 **JS 侧无效**：宿主按调用
方法覆写（`sendSync` → `sync:true`，`enqueue` → `enqueue_only:true`）。

## 错误

所有方法 resolve 信封，按 `code` 判定：

| `code` | 含义 |
|---|---|
| `0` | 成功：`data.messageId` 为投递凭据（SMTP 应答文本 / FileTransport 落盘 `.eml` 文件名主干），`data.jobId` 为作业号 |
| `1` | 网络/连接/超时及**一切投递期失败**（含 SMTP 5xx、鉴权失败、`投递未能送达插件`），`msg` 为脱敏分类文案 |
| `2` / `3` | **保留未启用**：SMTP 5xx（2）与鉴权失败（3）当前均归入 `1`；按错误码分支请以 `code !== 0` 判失败 |
| `4` | 队列满（背压） |
| `5` | 入参校验失败：地址非法 / 白名单未命中 / 缺正文 / 附件形态错 / 路径越界 / 附件超限 / 未知 profile（`mail: unknown mail profile '<key>'（已知：[...]）`） |

唯一 **抛异常**（Promise reject）的场景：

| 场景 | 错误消息 |
|---|---|
| 未配置 `smtp:` 段或未装插件 | `mail not configured (config smtp: section missing, or oj-mail plugin not loaded)` |

## 限制

- **`code !== 0` 一律不得自动重试**：永久失败与瞬时失败都归 `1`，从 code 分不出
  可重试性。
- 结果归属：`result(jobId)` 只回「profile + 模块 + 租户」三者全同的调用方的结果；
  跨模块通知请订阅 bus topic `mail.result`（扁平 `{jobId,code,msg,messageId?}`）。
- 结果存储限长 1024 条（超限淘汰最旧）、TTL 1 小时（惰性清理）——
  `result()` 只对近期查询有意义。
- 附件字节上限：单附件 10 MiB / 单封合计 25 MiB（可用 `smtp.max_*` 调整），
  超限回 `code:5`；`path` 附件必须落在项目根内。
- SMTP 凭据只走 config → 插件，**不支持按调用方传入凭据**（红线：防开放中继）。
- `Mail.profiles()` 只列 profile 名，凭据/连接字段不进 JS。

## 案例

### 注册后发欢迎邮件（同步拿投递结果）

```ts
// src/user/register/api.ts —— 注册成功后发欢迎信
async function post() {
  const { email, name } = http.body || {};
  if (!email) { json.fail(400, "email required"); return; }
  // …（建账号落库略）
  const r = await mail.send({   // mail === new Mail("default")
    from: "noreply@x.com",      // 须命中 allowed_from
    to: [String(email)],        // 须命中 allowed_recipients
    subject: "欢迎注册",
    text: `${name ?? ""}，欢迎加入！`,
  });
  if (r.code !== 0) {           // 信封模型：失败也是 resolve，不抛
    json.fail(r.code, r.msg);
    return;
  }
  json.ok({ messageId: r.data.messageId });
}
export default { post };
```

```bash
curl -s -X POST http://localhost:9778/v1/api/user/register \
  -H 'Content-Type: application/json' \
  -d '{"email":"a@x.com","name":"Ada"}'
# → {"code":0,"data":{"messageId":"<…@x.com>"}}
```

### 批量通知走队列，结果异步回查

```ts
// src/notify/batch/api.ts —— 批量入队，立即返回 jobId 清单
async function post() {
  const { recipients } = http.body || {};
  if (!Array.isArray(recipients) || !recipients.length) {
    json.fail(400, "recipients required");
    return;
  }
  const jobs = [];
  for (const addr of recipients) {
    const r = await mail.enqueue({
      from: "noreply@x.com",
      to: [String(addr)],
      subject: "系统维护通知",
      text: "本周六 02:00-04:00 停机维护。",
    });
    if (r.code !== 0) { json.fail(r.code, r.msg); return; }
    jobs.push({ to: addr, jobId: r.data.jobId });   // jobId 由宿主生成
  }
  json.ok({ queued: jobs.length, jobs });
}
export default { post };
```

```ts
// src/notify/status/api.ts —— 事后按 jobId 查投递结果
async function get() {
  const jobId = String(http.query.jobId ?? "");
  const res = await mail.result(jobId);   // 未命中/过期/非本归属 → null
  if (!res) { json.fail(404, "job not found or expired"); return; }
  json.ok(res);   // {jobId, code, msg, messageId?}
}
export default { get };
```

### 本地开发用 file_transport 落盘调试（命名 profile）

```ts
// src/dev/mailtest/api.ts —— 打 mock profile，不发网络
async function post() {
  const r = await new Mail("mock").send({
    from: "noreply@x.com",
    to: ["a@x.com"],
    subject: "模板自测",
    html: "<h1>hello</h1>",
  });
  if (r.code !== 0) { json.fail(r.code, r.msg); return; }
  json.ok({ eml: r.data.messageId });   // FileTransport 下为落盘 .eml 文件名主干
}
export default { post };
```

```yaml
# config.yaml（对应「配置」节的 smtp: 段；mock 的 file_transport 目录须先存在）
smtp:
  default:
    host: smtp.example.com
    port: 465
    tls: tls
    mechanism: login
    user: api@example.com
    pass: "ENC[...]"
    allowed_from: ["noreply@x.com"]
    allowed_recipients: ["@x.com"]
  mock:
    host: localhost
    port: 25
    tls: none
    allow_none_tls: true
    mechanism: login
    file_transport: "/tmp/oj-mail-eml"
    allowed_from: ["noreply@x.com"]
    allowed_recipients: ["@x.com"]
```

```bash
curl -s -X POST http://localhost:9778/v1/api/dev/mailtest/
ls /tmp/oj-mail-eml/   # 查看落盘的 .eml
```
