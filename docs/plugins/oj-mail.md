# oj-mail

## 概述

提供 `mail` 轴的 cdylib 插件，底层 `lettre` 0.11 SMTP。负责连接池、有界队列 + worker 池、实际投递，经 `FfiFuture` 回结果、经 `HostContext.deliver("mail.result", ...)` 上送异步完成。

## 提供的后端轴

`mail`

## 配置

顶层 `smtp:` 段（存在即启用 `Mail` / `mail`）。段分两类键：

- **全局键**（4 个）：`workers`（worker 数，默认 4）、`queue_capacity`（有界队列容量，默认 256）、`max_attachment_bytes`（默认 10 MiB）、`max_total_attachment_bytes`（默认 25 MiB）。
- **其余每个键都是一个 profile**（键名即 `Mail(key)` / `mail.send` 的 profile 名）。单 profile 字段：

| 字段 | 说明 |
|---|---|
| `host` / `port` | SMTP 服务器 |
| `tls` | `tls`（隐式 465） / `starttls`（25/587） / `none`；`none` 须显式 `allow_none_tls: true` 否则 fail-closed |
| `allow_none_tls` | 显式允许明文，默认 false |
| `mechanism` | `login` / `xoauth2` |
| `user` / `pass` | `login` 用（成对） |
| `xoauth2` | `{ access_token }`；只给 `refresh_token` → 显式报错（本版不刷新） |
| `timeout` | 单封超时（秒，默认 30） |
| `file_transport` | 给定 → 写 `.eml` 到目录（不发网络，测试/归档用） |
| `allowed_from` / `allowed_recipients` | 发件人 / 收件人白名单（全等匹配，空表即拒绝） |

也可经 `plugins:` 段的 `plugins.mail` 值原样透传（非空透传与顶层 `smtp:` 互斥，装配期报错）。

字段权威定义见 [`../mail-smtp.md`](../mail-smtp.md) 的 `smtp` 配置段（§2）。

## 依赖与构建注意

- `lettre` 0.11，rustls = `=0.23.40`，与框架同 provider（aws-lc-rs，**不引 ring**）。
- init 首语句必须装 aws-lc-rs 默认 CryptoProvider；`TlsParameters::new` 立即构建 rustls `ClientConfig`，未装则 panic。
- lettre `pool` 在构建与 Drop 时都 `tokio::spawn` → transport 的创建/使用/销毁必须全程在插件自身 runtime 内（已保证）。
- `tls: none` 且走网络的 profile，启动时打 warn 级告警（含 profile 名与 host:port）；`file_transport` 不告警。

## 状态

首版可用（`docs/mail-smtp.md` 记 v0.1.19 起）；随发行包发布。

## 案例

### 注册后发欢迎邮件（同步投递）

```yaml
# config.yaml —— smtp: 段存在即启用 mail；每个非全局键是一个 profile
smtp:
  default:
    host: smtp.example.com
    port: 465
    tls: tls                 # tls（隐式 465）| starttls | none（须 allow_none_tls: true）
    mechanism: login
    user: api@example.com
    pass: "ENC[...]"
    allowed_from: ["noreply@x.com"]            # 白名单 fail-closed：空表 = 全拒
    allowed_recipients: ["@x.com", "@partner.com"]
```

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
  if (r.code !== 0) {           // 信封模型：失败也是 resolve，不抛；不得按 code 自动重试
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

大批量通知用 `enqueue` 入队即回（`{jobId}`），真实完成经 `mail.result(jobId)`
或 bus topic `mail.result` 上送：

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
  json.ok(res);   // {jobId, code, msg, messageId?}（扁平结果，不含收件人/主题）
}
export default { get };
```

### 本地开发用 file_transport 落盘调试

不发网络、写 `.eml` 到目录，联调模板/白名单足够：

```yaml
smtp:
  mock:
    host: localhost            # file_transport 也须写全 host/port/tls/mechanism（schema 必填）
    port: 25
    tls: none
    allow_none_tls: true
    mechanism: login
    file_transport: "/tmp/oj-mail-eml"   # 目录须先存在（lettre 不建目录）
    allowed_from: ["noreply@x.com"]
    allowed_recipients: ["@x.com"]
```

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

```bash
curl -s -X POST http://localhost:9778/v1/api/dev/mailtest/
ls /tmp/oj-mail-eml/   # 查看落盘的 .eml
```

## 备注

- 新增 `mail` 轴**零 ABI 变更**：`AXES` 加 `"mail"` 不 bump `ABI_VERSION`，既有插件无需重编。
- 附件字节由宿主解析后经 FFI 直传插件（引用式，不经 base64 / 不经 JS）；插件不读盘、不碰 bus。
- 已知限制：跨进程 `mail.result` 仅本地扇出；XOAuth2 仅静态 token；SMTP 错误细分（code 2/3）未启用，5xx 与鉴权失败均归 code 1。
