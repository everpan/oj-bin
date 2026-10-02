# LDAP 目录与鉴证手册

> 给谁读：要用 AD/OpenLDAP 做登录鉴证、目录查询、分组授权的业务开发者，以及运维
> LDAP 配置的人。想快速查 API 先看 `docs/devkit/api-manual.md` §6「ldap」与
> `docs/devkit/scenarios.md` 场景 11；本手册是完整参考（架构、配置、JS API、
> 鉴证模式、注入防护、运维与已知限制）。

- 实现形态：**cdylib 插件 `oj-ldap`** + 新增 `ldap` 轴（非内建 op）。启动时由宿主 dlopen 加载。
- 底层：`ldap3` 0.12.1（纯 Rust tokio LDAP 客户端），TLS 走
  `tls-rustls-aws-lc-rs`（rustls 0.23 + aws-lc-rs，与框架同 provider，**不引 ring/native-tls**）。
- 版本：v0.1.28 起。

## 1. 架构与职责边界

| 角色 | 职责 |
|---|---|
| **宿主（core `src/bridge/ldap.rs`）** | 实例表白名单校验（`LdapConfig::from_value`）、调用入参校验（scope/filter/attrs/pageSize）、JS 全局 `LDAP`/`ldap`、错误转 JsError |
| **插件（`plugins/oj-ldap`）** | 连接生命周期（每调用 connect → 服务账号绑定 → 操作 → unbind）、ldap3 协议交互、结果 JSON 编码，经 `FfiFuture` 回传 |

- 契约：`oj-plugin-ffi` 的 `LdapVtable { call(req_json) -> FfiFuture }`（repr(C)，mail 式单入口
  JSON 分派——加操作不改 vtable 形状）。
- **新增轴零 ABI 变更**：`AXES` 加 `"ldap"` 不 bump `ABI_VERSION`（当前 11），既有插件无需重编。
- **连接模型（ponytail）**：**不做连接池**——每次调用独立成连。`bind(dn,pw)` 用户鉴证本就要求
  凭据不落到共享连接上；search 的服务账号绑定在 AD/LAN 上是毫秒级开销。热路径真有压力时的
  升级路径是 `ldap3::pool`（契约不变，插件内部实现）。

## 2. 配置（`config.yaml`）

顶层 `ldap:` 段，存在即启用 `LDAP`/`ldap`。段内**每个顶层键 = 一个实例**（键名即
`new LDAP(key)` 的 key，缺省实例名 `"default"`）：

```yaml
ldap:
  default:
    url: ldaps://dc.example.com:636      # ldap://（明文）或 ldaps://（隐式 TLS），其他 scheme 启动报错
    bind_dn: cn=svc-oj,ou=service,dc=example,dc=com   # 服务账号（可选，成对 bind_pw）
    bind_pw: "change-me"                 # 凭据只在 config → 插件，不进 JS、不进日志
    timeout_ms: 5000                     # 连接与操作超时 ms（100..=3600000，默认 5000）
  ad:
    url: ldap://ad.internal:389
    start_tls: true                      # 明文口上 StartTLS 升级（配在 ldaps:// 上 → 启动报错）
    tls_skip_verify: true                # 自签 CA 测试用；生产必须关掉
```

| 实例键 | 默认 | 说明 |
|---|---|---|
| `url` | —（必填） | 仅 `ldap://` / `ldaps://`；未知键、坏 url、超时越界均**启动即报错**（fail-loud，不静默忽略） |
| `bind_dn` / `bind_pw` | 无 | **成对出现**（只配一个 → 启动报错）；都不配时 search/whoami/compare 以**匿名绑定**执行（多数目录默认拒绝匿名读，届时报 insufficient access rights 类错误） |
| `timeout_ms` | `5000` | 同时约束连接建立与单次操作 |
| `start_tls` | `false` | 仅用于 `ldap://` 口 |
| `tls_skip_verify` | `false` | 跳过服务端证书校验；**仅测试环境** |

### 门禁（fail-closed）

- **白名单**：实例表只认上表 6 个键，未知键启动报错（配置段刻意做成 opaque，由
  `LdapConfig::from_value` 统一校验，serde 不会替你静默丢键）。
- **双配置源互斥**：顶层 `ldap:` 段与**非空** `plugins.ldap` 透传不得同时出现（否则透传静默
  胜出、`ldap:` 改动不生效）——装配期直接报错让你二选一。`plugins: {ldap: {}}`（空对象）
  不算冲突。
- **未装插件不阻断启动**；调用时报
  `ldap not configured (config ldap: section missing, or oj-ldap plugin not loaded)`（可选能力）。

## 3. JS API

```js
const ok = await ldap.bind(dn, pw);                       // true=绑定成功；false=凭据被拒
const entries = await ldap.search(base, opts);            // 目录查询
const all = await ldap.searchPaged(base, { ...opts, pageSize: 500 });  // RFC 2696 分页聚合
const authzid = await ldap.whoami();                      // "dn:cn=svc-oj,…"
const same = await ldap.compare(dn, "uid", "eve");        // 属性值比对，不读出整条目

const ad = new LDAP("ad");                                // 具名实例（ad === ldap 无关）
```

| API | 签名 | 说明 |
|---|---|---|
| `new LDAP(key?)` | `LDAP(key?: string)` | 实例；`key` 未声明**报错**（不回落 default） |
| `ldap.bind` | `bind(dn, pw): Promise<boolean>` | simple_bind 鉴证。`true` = 成功；`false` = LDAP 拒绝凭据（rc≠0，含 49 invalidCredentials）；连接/协议错误**抛异常** |
| `ldap.search` | `search(base, opts?): Promise<Entry[]>` | 目录查询；大结果集用 `searchPaged`（服务端可拒超量返回，AD 默认上限 1000 条） |
| `ldap.searchPaged` | `searchPaged(base, opts & {pageSize?}): Promise<Entry[]>` | 分页 cookie 循环聚合到完；服务端不支持分页控制时**原样回落单次 search**。`pageSize` 1..=10000，默认 500 |
| `ldap.whoami` | `whoami(): Promise<string>` | whoami 扩展（RFC 4532）→ `"dn:cn=…"` 形式的 authzid（服务账号身份自检/连通性探活） |
| `ldap.compare` | `compare(dn, attr, val): Promise<boolean>` | compareTrue/False（rc 6/5）；比读出整条目再比对便宜，适合「组成员是否含某值」 |

```ts
type SearchOpts = {
  scope?: "base" | "one" | "sub";   // 默认 "sub"（整棵子树）；"one" = 仅下一层
  filter?: string;                  // RFC 4515 过滤器，默认 "(objectClass=*)"
  attrs?: string[];                 // 属性名清单；缺省/空 = 服务端默认属性集
};
type Entry = {
  dn: string;
  attrs: Record<string, string[]>;  // 文本属性（多值即多元素）
  bin: Record<string, string[]>;    // 二进制属性（jpegPhoto/userCertificate 等），base64 字符串
};
```

### 错误模型（与 mail 的信箱模型**不同**）

**除「未配置」外，一切失败都是 Promise reject**（校验错/网络错/协议错/服务端 rc≠0）：

| 错误文案（节选） | 含义 |
|---|---|
| `ldap: unknown instance 'x'（known: …）` | `new LDAP("x")` 的 key 未在配置声明 |
| `ldap.bind: 'dn' must be a non-empty string` | 入参校验（宿主侧，JS 抛出前） |
| `ldap: connect ldap://…: io error` / `Connection refused` | 网络/端口/防火墙/`url` 拼错 |
| `ldap: service bind cn=…: rc=49 …` | 服务账号凭据错（search 在绑定阶段就失败） |
| `ldap: search: … size limit exceeded` | 超服务端返回上限——换 `searchPaged` |
| `ldap: entry parse failed (malformed server data)` | 单条返回 BER 解析失败（整次调用拒绝） |

> **`ldap.bind` 的 `false` 是唯一正常返回值**，别用 `try/catch` 当业务分支：
> reject = 环境/配置问题（应当报错给人看），`false` = 用户密码错（正常 401 路径）。

## 4. 鉴证模式（最常见用途）

「用户名 + 密码」登录 → **先 search 把用户名解析成 DN，再用该 DN 绑定**（simple_bind 不能直接
拿用户名当 DN）。完整可抄代码见 `docs/devkit/scenarios.md` 场景 11，要点：

1. `filter: "(uid=${esc(username)})"` —— **注入先转义**（§5）。
2. 命中数 ≠ 1 一律按凭据错处理（不泄露用户存在性）。
3. `ldap.bind(dn, password)` 返回 `false` → 401。
4. 目录只**鉴证**，不签发会话——自己的票（`jwt.sign` 等）自行签发，可把 `memberOf` 等
   授权属性拷进声明。

变体：若目录允许按 UPN 绑定（AD 的 `user@domain`），可跳过 search 直接
`ldap.bind(`${username}@example.com`, password)`——但授权属性仍要 search 拿。

## 5. filter 注入（**必须转义**）

`filter` 是拼进 LDAP 查询的原始字符串，**没有参数绑定**。用户输入若含 `*`（通配）、`(` `)`
（改变逻辑）、`\`、NUL，会改变查询语义——LDAP 版的 SQL 注入，可拖出全目录或绕过命中限制：

```ts
const LDAP_FILTER_ESCAPES: Record<string, string> = {
  "\\": "\5c", "*": "\2a", "(": "\28", ")": "\29", "\0": "\00",
};
const esc = (s: string) => s.replace(/[\\*()\0]/g, (c) => LDAP_FILTER_ESCAPES[c]);
// filter: `(uid=${esc(username)})`
```

也可用 `ldap.compare(dn, attr, val)` 收口：值作为**协议级属性值**传输，不经 filter 字符串。

## 6. 二进制属性

`jpegPhoto`、`userCertificate`、`thumbnailPhoto`（AD 头像）等二进制值不混进 `attrs`，统一放
`entry.bin`，值为 **base64 编码字符串数组**：

```ts
const img = Buffer.from(entry.bin.jpegPhoto[0], "base64");  // 或 atob() → Uint8Array
```

> 注意整条目（含 base64 后的二进制）一次性聚进内存——`searchPaged` 只是把**协议交互**分页，
> 聚合结果仍是完整数组。大对象属性请用 `attrs` 收窄只取需要的键。

## 7. 构建、加载与运维

```bash
cargo xtask plugin ldap            # 构建 oj-ldap 并归置 bin/plugins/<triple>/libldap.{so,dylib}
cargo xtask plugin ldap --check    # ABI/身份/semver/符号 预检
cargo xtask build                  # 构建 oj + 全部第一方插件（含 ldap）
```

- 插件发现：`OJ_PLUGINS_DIR` > config `plugins_dir` > `<exe>/plugins` > `<workspace_root>/bin/plugins`，
  再拼 `<host-triple>/`。
- **超时只有一个旋钮** `timeout_ms`（连接与操作共用）；目录跨 WAN 时调大，内网默认 5s 通常够。
- 每次调用独立连接的代价 = 一次 TCP/TLS 握手 + 一次服务账号绑定。QPS 高的热路径若测出瓶颈，
  升级方向是插件内 `ldap3::pool`（§1），宿主与契约都不用动。
- `whoami()` 可当**目录连通性探活**：返回服务账号 authzid 即「网络 + 绑定 + 权限」全链路通。
- 多实例（default/ad/...）各自独立配置，互不相干；密钥只在 config → 插件内存。

## 8. 已知限制

| 项 | 现状 |
|---|---|
| 连接池 | **无**——每调用独立 connect/bind/unbind（§1；升级路径 `ldap3::pool`） |
| Referral 跟随 | **不跟随**——`search()` 把 referral 收集进结果但不自动跳转（ldap3 语义）；需要跨分区查询请显式对该 base 再查 |
| 写操作 | 无 `add`/`modify`/`delete`——本期只读 + 鉴证面（bind/search/searchPaged/whoami/compare）；写操作是新 op 字符串，插件内加分派即可（vtable 不变） |
| filter 参数化 | 无——原始 RFC 4515 字符串，须自行转义（§5）；`compare` 是值传输的安全替代 |
| 分页语义 | `searchPaged` 聚合**全部**结果后一次性返回（不是迭代器/流）；超大结果集注意内存（§6） |
| 二进制传输 | `entry.bin` base64 进 JSON——比 FFI 直传字节多约 33% 体积；巨大证书对象考虑 `attrs` 收窄 |
| TLS | `ldaps://` 与 `start_tls` 二选一；`tls_skip_verify` 无内网 CIDR 约束（仅显式开关） |
| AD 1000 条上限 | 服务端行为——用 `searchPaged`（本插件已内建 cookie 循环） |

## 9. 相关文档

- 业务速查：`docs/devkit/api-manual.md` §6「ldap」；可照抄代码：`docs/devkit/scenarios.md` 场景 11；
  agent 陷阱速查：`docs/devkit/SKILL.md`。
- 插件体系：`docs/plugin-architecture.md`、`docs/plugin-development.md`（`ldap` 轴在 AXES 列表）。
- 配置样例：`sample/config.yaml`（注释掉的 ldap 示例）、`sample/global.d.ts`（TS 类型）。
- 设计与实现记录：`docs/superpowers/specs/2026-09-27-oj-ldap-design.md`。
