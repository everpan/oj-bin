# ldap —— 目录查询与鉴证（`LDAP` 类与 `ldap` 默认实例）

> 本文档由 src/bridge/ 梳理生成；权威 API 参考见 ../devkit/api-manual.md。

## 是什么

`LDAP` 类与 `ldap` 默认实例（`ldap === new LDAP("default")`）为 handler 提供
LDAP 目录查询（search / searchPaged / whoami / compare）与 bind 鉴证。
**协议交互**在 `oj-ldap` 插件内（ldap3 客户端：每调用独立 connect → 服务账号绑定 →
操作 → unbind，无连接池）；**宿主**（`src/bridge/ldap.rs`）负责实例表白名单与
调用入参逐 op 校验（5 个方法全部经统一的 `op_ldap_call` 入口）。启用条件：
config 有顶层 `ldap:` 段 + oj-ldap 插件已装配。

**错误模型同 `db`**：除「未配置」外，校验/协议/网络错一律 **Promise reject**
——与 mail 的信封模型不同，别用 `try/catch` 当业务分支；唯一正常返回值的失败是
`ldap.bind` 的「凭据被拒」→ `false`。

## 配置

顶层 `ldap:` 段，**每个顶层键 = 一个实例**（键名即 `new LDAP(key)` 的 key，
缺省实例名 `default`）：

```yaml
ldap:
  default:
    url: ldaps://dc.example.com:636      # ldap://（明文）或 ldaps://（隐式 TLS）
    bind_dn: cn=svc,ou=app,dc=example,dc=com   # 服务账号：search/whoami/compare 前置绑定
    bind_pw: "ENC[...]"                  # 凭据只在 config → 插件，不进 JS
    timeout_ms: 5000                     # 连接与操作超时（100..=3600000，默认 5000）
  ad:
    url: ldap://ad.internal:389
    start_tls: true                      # 明文口上 StartTLS 升级（ldaps:// 上配置即报错）
    tls_skip_verify: true                # 跳过服务端证书校验，仅测试环境
```

- 未知键 / 坏 url（非 `ldap://` / `ldaps://`）/ 类型错误 → **启动 fail-fast**。
- `bind_dn` 可单独配（DN 非密码）；`bind_pw` 缺失时可在每次 `search` / `searchPaged`
  用 `opts.bindPw` 运行时补上（与 config 的 `bind_dn` 合并）。**只为「有 `bind_pw`
  却无 `bind_dn`」报错**。都不配则匿名绑定（多数目录默认拒匿名读）。

## API

| 函数 | 签名 | 说明 |
|---|---|---|
| `new LDAP(key)` | `LDAP(key?: string)` | 实例；`key` = `ldap:` 段里的实例名（缺省 `"default"`）；未声明的 key 调用时报错（不回落 default） |
| `bind` | `bind(dn: string, pw: string): Promise<boolean>` | simple_bind 鉴证。`true` = 绑定成功；`false` = LDAP 拒绝该凭据（含 rc 49 invalidCredentials）；连接/协议错误**抛异常** |
| `search` | `search(base: string, opts?: SearchOpts): Promise<Entry[]>` | 目录查询；大结果集请用 `searchPaged`（无分页时服务端可拒超量返回） |
| `searchPaged` | `searchPaged(base: string, opts?: SearchOpts & { pageSize?: number }): Promise<Entry[]>` | RFC 2696 **分页聚合**（逐页取回后合并为完整数组；服务端不支持分页控制时原样回落单次 search）。`pageSize` 1..=10000，默认 500 |
| `whoami` | `whoami(): Promise<string>` | whoami 扩展（RFC 4532）→ `"dn:cn=svc,…"` 形式的 authzid |
| `compare` | `compare(dn: string, attr: string, val: string): Promise<boolean>` | 属性值比对（compareTrue/False），不读出整条目 |

```ts
type SearchOpts = {
  scope?: "base" | "one" | "sub";   // 默认 "sub"（整棵子树）；"one" = 仅下一层
  filter?: string;                  // RFC 4515 过滤器，默认 "(objectClass=*)"
  attrs?: string[];                 // 要读的属性名；缺省/空数组 = 服务端默认属性集
  bindDn?: string;                  // 覆盖本次查询的绑定凭据（与 config 合并，取一即可；
  bindPw?: string;                  //   另一个回落 config）。仅 search / searchPaged 支持
};
type Entry = {
  dn: string;
  attrs: Record<string, string[]>;  // 文本属性（多值即多元素）
  bin: Record<string, string[]>;    // 二进制属性（jpegPhoto 等），base64 编码字符串
};
```

## 错误

全部以 **Promise reject** 呈现（`bind` 的凭据被拒除外，它 resolve `false`）：

| 场景 | 错误消息 |
|---|---|
| 未配置 `ldap:` 段或未装插件 | `ldap not configured (config ldap: section missing, or oj-ldap plugin not loaded)` |
| 实例名未声明 | `ldap: unknown instance '<key>'（known: <已知名单>）` |
| 必填字符串缺失/为空 | `ldap.<op>: 'dn' must be a non-empty string` / `'base' must not be empty` 等 |
| scope 非法 | `ldap.search: scope must be 'base'\|'one'\|'sub' (got '<s>')` |
| attrs 形态错 | `ldap.<op>: 'attrs' must be an array` / `'attrs' must be string[]` |
| pageSize 越界/形态错 | `ldap.search_paged: page_size must be 1..=10000` / `'page_size' must be a number` |
| compare 缺 val | `ldap.compare: 'val' must be a string` |
| 网络/拒连 | `ldap: connect ldap://…: io error` |
| 服务账号凭据错 | `ldap: service bind …: rc=49 …` |
| config 只配了密码 | `bind_pw requires bind_dn (set both in config, or supply bindPw per search call)` |

## 限制

- **filter 注入（红线）**：`filter` 是拼进 LDAP 查询的原始 RFC 4515 字符串，
  **无参数绑定**。用户输入拼进 filter 前**必须转义**——把 `*` `(` `)` `\` NUL
  逐字符转成 `\hh`（两位十六进制）。能用 `ldap.compare` 收口的判定（成员资格、
  属性比对）优先用 compare（值传输，天然无注入面）。
- 只读 + 鉴证面：无 add / modify / delete 写操作。
- 无连接池：每调用独立 connect → bind → 操作 → unbind，高频场景注意往返开销。
- referral 不自动跟随。
- `bindDn` / `bindPw` 仅 `search` / `searchPaged` 支持；`whoami` / `compare` /
  `bind` 用 config 服务账号或各自入参。
- 结果大小：聚合结果整体进内存（`Entry[]`），超大目录导出请分批按 base/scope 拆。

## 案例

### 用 AD / OpenLDAP 账号登录（search 找 DN + bind 鉴证）

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

签发 JWT 需另配 `auth:` 段（oj-auth 插件）；本路由自身也要进
`auth.anonymous_paths`。

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

```yaml
# config.yaml —— 对应「配置」节的 ldap: 段；命名实例用 new LDAP("ad") 取用
ldap:
  default:
    url: ldaps://dc.example.com:636
    bind_dn: cn=svc,ou=app,dc=example,dc=com
    bind_pw: "ENC[...]"
    timeout_ms: 5000
  ad:
    url: ldap://ad.internal:389
    start_tls: true
    tls_skip_verify: true   # 仅测试环境
```
