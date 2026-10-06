# oj-ldap：LDAP 插件轴设计（2026-09-27）

## 目标

为 JsRuntime 提供 LDAP 客户端能力：JS 侧 `ldap.*` 全局 + `LDAP(name)` 命名实例，
后端为 cdylib 插件 `plugins/oj-ldap`（基于 `ldap3 = "0.12.1"` 纯 Rust 客户端）。

## 形态决策

- **cdylib 插件**（用户选定）：新轴 `ldap` 追加进 `plugin_loader::AXES`。
  ABI 7 起按轴 dlsym，**加轴零破坏，ABI_VERSION 不变**。
- vtable 采用 mail 式**单入口 JSON dispatch**：
  `LdapVtable { call: extern "C" fn(req: RString) -> FfiFuture }`。
  日后加 LDAP 方法只扩 req JSON 字段，不动 repr(C)。

## ldap3 调研结论

- `0.12.1`（2026-07-26），纯 Rust、tokio 原生异步、MIT/Apache-2.0，无 C 依赖
- TLS 特性与仓库纪律对齐：`tls-rustls-aws-lc-rs`（rustls 0.23 + aws-lc-rs，零 ring）
- 核心 API：`LdapConnAsync::new(url)`、`simple_bind`/`bind`、`search(base, scope, filter, attrs)`
  → `SearchEntry { dn, attrs, bin_attrs }`、`Scope::{Base, OneLevel, Subtree}`、
  内置连接池 `pool::{Pool, PoolSettings}` 与 paged search

## config（`ldap:` 段，仿 `smtp:` 一段两用）

```yaml
ldap:
  default:                        # 键 = 实例名
    url: ldaps://dc.example.com:636
    bind_dn: cn=svc,ou=app,dc=example,dc=com
    bind_pw: ${LDAP_PW}
    timeout_ms: 5000
    start_tls: false
    tls_skip_verify: false
  ad: { url: ldap://ad.internal:389, start_tls: true }
```

整段序列化 JSON 传插件 init；宿主 `Config` 白名单校验
（url/bind_dn/bind_pw/timeout_ms/start_tls/tls_skip_verify）。

## JS API（bootstrap.js 装配）

```js
await ldap.bind(dn, pw)              // simple_bind → bool（内部 unbind）
await ldap.search(base, { scope: "sub"|"one"|"base", filter: "(uid=eve)", attrs: [...] })
                                     // → [{ dn, attrs: {k:[v]}, bin: {k:[bytes]} }]
await ldap.searchPaged(base, { ..., pageSize: 500 })   // paged 聚合
await ldap.whoami()                  // whoami 扩展 → "dn:..."
await ldap.compare(dn, attr, value)  // → bool
LDAP("ad").search(...)               // 命名实例
```

校验失败 → `{code:5}` 信封不抛；LDAP 协议错 → Promise reject。

## 组件

| 位置 | 内容 |
|---|---|
| `oj-plugin-ffi/src/ldap.rs` | `LdapVtable` + re-export（新文件） |
| `src/bridge/ldap.rs` | `LdapBackend` trait + `FfiLdapBackend` + 5×`op_ldap_*`（白名单校验层） |
| `src/bridge/mod.rs` | `StableState.ldap` + Extras + ops 注册 |
| `src/config.rs` | `ldap: Option<LdapSection>` |
| `oj/src/serve_cmd.rs` | `ADAPTER_AXES` + `"ldap"`、vtable 槽、`app::build_ldap_backend` |
| `src/bridge/bootstrap.js` | `ldap`/`LDAP(name)` 装配 |
| `plugins/oj-ldap/` | `lib.rs` + `config.rs` + `engine.rs`（ldap3 池化） |

插件内部：每实例一个 ldap3 `Pool`；search 前自动以 `bind_dn` 服务账号绑定；
`bind(dn,pw)` 用传入凭据独立连接校验。

## 测试

- 插件侧：req JSON 解析 + URL 校验单测
- 宿主侧：未配置报错、mock backend 走通 5 op、bootstrap 全局
- `cargo xtask plugin ldap --check` + smoke

## 文档

CHANGELOG + `docs/devkit/` 四件套 + `docs/plugin-development.md` + devkit 契约用例。
