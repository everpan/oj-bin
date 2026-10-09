# oj-ldap

## 概述

提供 `ldap` **泛型轴**的 cdylib 插件（v0.1.54 起从类型化轴迁移），底层 `ldap3`
（纯 Rust tokio LDAP 客户端）。负责目录查询与 bind 鉴证：search / search_paged /
whoami / compare / bind。连接模型为每调用独立 connect → 服务账号绑定 → 操作 →
unbind，不做连接池。

**泛型轴声明**：`oj_plugin_entry!(init, config: "ldap", generic(ldap) => &LDAP_VT)`——
不占 9 个类型化轴槽、零 ABI 变更；JS 调用面为 `axis("ldap").<op>(...args, opts?)`
（协议面与 vtable 形状见 `oj-plugin-ffi` 的 `GenericVtable` / `AxisDecl`）。
宿主类型化 `ldap`/`LDAP` 全局保留但装配本插件后报 `ldap not configured`（双轨期）。

## 提供的后端轴

`ldap`（**泛型轴**，`kind=GENERIC`；v0.1.54 之前为类型化轴）。

## 配置

入口宏自报 `config: "ldap"`——cfg 三级解析第 2 级直接取顶层 `ldap:` 段全量 Value
（未配置给 `{}`，段可选；`plugins.ldap` 非空透传仍优先）。段内**每个顶层键 = 一个
实例**（键名即调用 opts 的 `key`，缺省 `"default"`）：

| 实例字段 | 说明 |
|---|---|
| `url` | 仅 `ldap://` / `ldaps://`；未知 scheme / 坏 url → 启动报错 |
| `bind_dn` / `bind_pw` | 服务账号，成对出现（只配一个 → 报错）；都不配则匿名绑定 |
| `timeout_ms` | 连接与操作超时（100..=3600000，默认 5000） |
| `start_tls` | 仅用于 `ldap://` 口；配在 `ldaps://` 上 → 启动报错 |
| `tls_skip_verify` | 跳过服务端证书校验，仅测试环境 |

字段权威定义见 [`../ldap-integration.md`](../ldap-integration.md) 的 `ldap` 配置段（§2）。

## 泛型轴 JSON 协议（vtable: `call(op, args)`）

`args` 恒为位置参数 JSON 数组；末位可选 opts 对象。实例选单经 `opts.key`（缺省
`"default"`）。未知 opts 键忽略。连接/协议/校验错误经 future Err 透传（JS 侧 reject）；
`bind` 凭据被 LDAP 拒绝 = `false`，非错误。

| op | args | opts | 结果 JSON |
|----|------|------|-----------|
| `bind` | `[dn, pw]` | `{key?}` | `true \| false` |
| `search` | `[base]` | `{key?, scope?, filter?, attrs?, bindDn?, bindPw?}` | `[{dn, attrs:{k:[v]}, bin:{k:[base64]}}]` |
| `search_paged` | `[base]` | 同上 + `{pageSize?}`（缺省 500，范围 1..=10000） | 同 `search` |
| `whoami` | `[]` | `{key?}` | authzid 字符串 |
| `compare` | `[dn, attr, val]` | `{key?}` | `true \| false` |

未知 op → `ldap: unknown op '<op>'（known: bind|search|search_paged|whoami|compare）`。

JS 侧：

```js
await axis("ldap").bind("uid=eve,dc=example,dc=com", "pw");            // → true|false
await axis("ldap").search("ou=users,dc=example,dc=com", { scope: "one", attrs: ["uid"] });
await axis("ldap").search_paged("dc=example,dc=com", { pageSize: 1000, key: "ad" });
await axis("ldap").whoami({ key: "ad" });                              // → "dn:cn=svc,…"
await axis("ldap").compare(dn, "uid", "eve");                          // → true|false
```

## 依赖与构建注意

- `ldap3` 0.12.1，TLS 走 `tls-rustls-aws-lc-rs`（rustls 0.23 + aws-lc-rs，与框架同 provider，**不引 ring / native-tls**）。
- init 时显式 `install_default` 装 aws-lc-rs CryptoProvider：本插件是独立 cdylib、自带一份 rustls，宿主侧的 provider 不覆盖此 copy，不装则在 `ldaps://` / `start_tls` 路径 panic。
- 鉴权 `bind(dn, pw)` 的凭据绝不落到共享连接上（连接模型决定）。
- filter 是原始 RFC 4515 字符串，**无参数绑定**，业务侧须自行转义防注入（`compare` 是值传输的安全替代）。
- 泛型轴名 `ldap` 撞类型化保留名——本插件**只能配 v0.1.54+ 宿主**（pre-kind 宿主按名 cast 会误解释 vtable；desc 已注明最低版本）。

## 状态

v0.1.28 首版（类型化轴，`docs/ldap-integration.md` 记录）；v0.1.54 迁移泛型轴。
随发行包发布。
