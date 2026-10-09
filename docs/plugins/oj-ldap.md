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

## 案例

### 用 AD / OpenLDAP 账号登录（search 找 DN + bind 鉴证）

```yaml
# config.yaml —— ldap: 段存在即启用 ldap；每个顶层键 = 一个实例
ldap:
  default:
    url: ldaps://dc.example.com:636          # ldap://（明文）或 ldaps://（隐式 TLS）
    bind_dn: cn=svc,ou=app,dc=example,dc=com # 服务账号：search/whoami/compare 前置绑定
    bind_pw: "ENC[...]"                      # 凭据只在 config → 插件，不进 JS
    timeout_ms: 5000
```

```ts
// src/ldap_auth/login/api.ts —— 目录账号登录：先按 uid 找 DN，再用用户密码 bind
// filter 是原始 RFC 4515 字符串、无参数绑定，用户输入必须先转义
function esc(s: string): string {
  return s.replace(/[\0*()\\]/g, (c) =>
    "\\" + c.charCodeAt(0).toString(16).padStart(2, "0"));
}

async function post() {
  const { username, password } = http.body || {};
  if (!username || !password) { json.fail(400, "username/password required"); return; }
  const found = await ldap.search("ou=users,dc=example,dc=com", {
    filter: `(uid=${esc(String(username))})`,
    attrs: ["uid", "displayName"],
  });
  if (found.length !== 1) { json.fail(401, "invalid credentials"); return; }
  // bind 的「凭据被拒」正常返回 false（不抛）；连接/协议错才抛异常
  const ok = await ldap.bind(found[0].dn, String(password));
  if (!ok) { json.fail(401, "invalid credentials"); return; }
  json.ok({
    access_token: await jwt.sign({ sub: found[0].attrs.uid[0] }),
    name: found[0].attrs.displayName?.[0],
  });
}
export default { post };
```

```bash
curl -s -X POST http://localhost:9778/v1/api/ldap_auth/login \
  -H 'Content-Type: application/json' \
  -d '{"username":"ada","password":"s3cret"}'
# → {"code":0,"data":{"access_token":"<jwt>","name":"Ada Lovelace"}}
```

签发 JWT 需另配 `auth:` 段（oj-auth 插件）；本路由自身也要进 `auth.anonymous_paths`。

### 通讯录模糊查询（searchPaged 聚合大结果集）

```ts
// src/directory/search/api.ts —— 按姓名关键字查员工目录
function esc(s: string): string {
  return s.replace(/[\0*()\\]/g, (c) =>
    "\\" + c.charCodeAt(0).toString(16).padStart(2, "0"));
}

async function get() {
  const kw = String(http.query.kw ?? "");
  if (!kw) { json.fail(400, "kw required"); return; }
  // 大结果集用 searchPaged（RFC 2696 分页聚合，pageSize 默认 500）
  const entries = await ldap.searchPaged("ou=users,dc=example,dc=com", {
    filter: `(displayName=*${esc(kw)}*)`,
    attrs: ["uid", "displayName", "mail", "department"],
  });
  json.ok(entries.map((e) => ({
    uid: e.attrs.uid?.[0],
    name: e.attrs.displayName?.[0],
    mail: e.attrs.mail?.[0],
    dept: e.attrs.department?.[0],
  })));
}
export default { get };
```

```bash
curl -s 'http://localhost:9778/v1/api/directory/search/?kw=ada' \
  -H "Authorization: Bearer $TOKEN"
# → {"code":0,"data":[{"uid":"ada","name":"Ada Lovelace","mail":"ada@example.com", …}]}
```

### 组成员判定用 compare 收口（不读整条目）

判定「某用户是否在某组」不必 search 整条目再本地比对，`compare` 是值传输，
天然无 filter 注入面：

```ts
// src/directory/in_group/api.ts —— 校验用户是否属于指定组
async function get() {
  const userDn = String(http.query.user_dn ?? "");
  const groupDn = String(http.query.group_dn ?? "cn=admins,ou=groups,dc=example,dc=com");
  if (!userDn) { json.fail(400, "user_dn required"); return; }
  const member = await ldap.compare(groupDn, "member", userDn);
  json.ok({ member });
}
export default { get };
```

## 备注

- 新增 `ldap` 轴**零 ABI 变更**：`AXES` 加 `"ldap"` 不 bump `ABI_VERSION`，既有插件无需重编。
- 只读 + 鉴证面：无 add/modify/delete 写操作；referral 不自动跟随；连接池未做（升级路径 `ldap3::pool`）。
