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

## 备注

- 新增 `mail` 轴**零 ABI 变更**：`AXES` 加 `"mail"` 不 bump `ABI_VERSION`，既有插件无需重编。
- 附件字节由宿主解析后经 FFI 直传插件（引用式，不经 base64 / 不经 JS）；插件不读盘、不碰 bus。
- 已知限制：跨进程 `mail.result` 仅本地扇出；XOAuth2 仅静态 token；SMTP 错误细分（code 2/3）未启用，5xx 与鉴权失败均归 code 1。
