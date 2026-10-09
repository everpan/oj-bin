# vars —— 部署期常量

> 本文档由 src/bridge/ 梳理生成；权威 API 参考见 ../devkit/api-manual.md。

## 是什么

`vars`（v0.1.25）是「部署期决定、业务代码不硬编码」常量（`WEB_URL`、支持邮箱这类
换环境即变的值）的读口。值写在 config 的顶层 `vars:` 段，装配期冻结成表，handler 用
`vars.get(name)` **同步**读取（无 IO，op 是同步的，同 `plugins()`）——可以直接
`?? 兜底` 当普通值用。

**fail-closed 安全边界**：只有 `vars:` 段里显式声明的键可读，其余一律 `null`；平台
**没有**「读任意 OS env / 读任意 config 键」的通道——`db:` 的 DSN、`smtp:` 凭据、
`server.public_key_path` 等敏感面不可能经此泄漏到 JS。

## 配置

```yaml
# config.yaml 顶层 vars: 段
vars:
  WEB_URL: "https://app.example.com"
  SUPPORT_EMAIL: "support@example.com"
  PORT: 3000        # 数字按 YAML 字面量成串 → JS 收到 "3000"
  FLAG: true        # 布尔同理 → "true"
```

- 段缺省 / 空段：一切键恒 `null`，不报错。
- **值只能是标量**（字符串/数字/布尔）；嵌套 map/list 属配置解析错误，启动即报错。

## API 表

| API | 签名 | 说明 |
|---|---|---|
| `vars.get` | `get(name: string): string \| null` | 已声明键 → 该字符串（**同步返回，不是 Promise**）；未声明 / 未配置 `vars:` 段 → `null` |

## 错误

`vars.get` 不抛错（设计如此）：

| 场景 | 行为 |
|---|---|
| 读未声明的键 | 返回 `null`（不是 `undefined`、不抛错）——`?? 兜底` 即可表达「可选部署项」 |
| 读声明为空串的键 | 原样返回 `""`（与「未声明」可区分：空串 ≠ `null`） |
| `vars:` 段含嵌套 map/list | **启动期**配置解析报错（fail-fast），不是运行期错误 |

## 限制

- **只读、装配期冻结**：没有 `vars.set`；运行期改常量请改 config 重启（要运行时可变
  状态用 `kv`）。
- 值一律是**字符串**：数字/布尔按 YAML 字面量成串（`3000` → `"3000"`），需要数字
  自行 `Number(...)`。
- 键名无命名空间/前缀约定，建议全大写蛇形（`WEB_URL`）与业务变量区分。
- 不要把密钥塞进 `vars:`——它虽 fail-closed，但设计定位是**非敏感**部署常量；
  敏感凭据走 `ENC[...]` 密封值（`oj secret seal`）。

## 案例

### 邮件链接拼域名（换环境不重新构建）

```yaml
# config.yaml
vars:
  WEB_URL: "https://app.example.com"
```

```ts
// src/auth/forgot/api.ts
async function post() {
  const { email } = http.body ?? {};
  // ... 生成重置 token t
  const web = vars.get("WEB_URL") ?? "http://localhost:3000";
  const link = `${web}/reset-password?token=${t}`;
  log.info("reset link issued", "email", email);
  json.ok({ sent: true, link });
}
export default { post };
```

```bash
curl -X POST -H 'content-type: application/json' -d '{"email":"a@b.c"}' \
  http://localhost:9778/v1/api/auth/forgot/
# → {"code":0,"msg":"ok","data":{"sent":true,"link":"https://app.example.com/reset-password?token=..."}}
```

### 可选部署项：配置了就用，没配走默认

```ts
// src/support/contact/api.ts —— 「没配」与「配了空串」可区分
function get() {
  const mail = vars.get("SUPPORT_EMAIL");          // 未声明 → null（不抛错）
  const tagline = vars.get("TAGLINE");             // 声明为空串 → ""（原样）
  json.ok({
    support: mail ?? "help@example.com",           // 可选部署项：?? 兜底
    tagline: tagline === null ? "default slogan" : tagline,
  });
}
export default { get };
```

### 数字型常量：成串读回再转换

```yaml
vars:
  PAGE_SIZE: 50
```

```ts
// src/item/page/api.ts
async function get() {
  const size = Number(vars.get("PAGE_SIZE") ?? 20);   // "50" → 50；未配 → 20
  const rows = await db.table("item").select(["id", "name"]).limit(size).all();
  json.ok({ size, rows });
}
export default { get };
```
