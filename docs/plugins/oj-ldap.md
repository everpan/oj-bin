# oj-ldap

## 概述

提供 `ldap` 轴（9 个轴里最新加的）的 cdylib 插件，底层 `ldap3`（纯 Rust tokio LDAP 客户端）。负责目录查询与 bind 鉴证：search / searchPaged / whoami / compare / bind。连接模型为每调用独立 connect → 服务账号绑定 → 操作 → unbind，不做连接池。

## 提供的后端轴

`ldap`

## 配置

顶层 `ldap:` 段，存在即启用 `LDAP` / `ldap`。段内**每个顶层键 = 一个实例**（键名即 `new LDAP(key)` 的 key，缺省实例名 `default`）：

| 实例字段 | 说明 |
|---|---|
| `url` | 仅 `ldap://` / `ldaps://`；未知 scheme / 坏 url → 启动报错 |
| `bind_dn` / `bind_pw` | 服务账号，成对出现（只配一个 → 报错）；都不配则匿名绑定 |
| `timeout_ms` | 连接与操作超时（100..=3600000，默认 5000） |
| `start_tls` | 仅用于 `ldap://` 口；配在 `ldaps://` 上 → 启动报错 |
| `tls_skip_verify` | 跳过服务端证书校验，仅测试环境 |

字段权威定义见 [`../ldap-integration.md`](../ldap-integration.md) 的 `ldap` 配置段（§2）。

## 依赖与构建注意

- `ldap3` 0.12.1，TLS 走 `tls-rustls-aws-lc-rs`（rustls 0.23 + aws-lc-rs，与框架同 provider，**不引 ring / native-tls**）。
- init 时显式 `install_default` 装 aws-lc-rs CryptoProvider：本插件是独立 cdylib、自带一份 rustls，宿主侧的 provider 不覆盖此 copy，不装则在 `ldaps://` / `start_tls` 路径 panic。
- 鉴权 `bind(dn, pw)` 的凭据绝不落到共享连接上（连接模型决定）。
- filter 是原始 RFC 4515 字符串，**无参数绑定**，业务侧须自行转义防注入（`compare` 是值传输的安全替代）。

## 状态

首版可用（`docs/ldap-integration.md` 记 v0.1.28 起）；随发行包发布。

## 备注

- 新增 `ldap` 轴**零 ABI 变更**：`AXES` 加 `"ldap"` 不 bump `ABI_VERSION`，既有插件无需重编。
- 只读 + 鉴证面：无 add/modify/delete 写操作；referral 不自动跟随；连接池未做（升级路径 `ldap3::pool`）。
