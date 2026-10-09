# json —— 统一响应信封（含 `finish()`）

> 本文档由 src/bridge/ 梳理生成；权威 API 参考见 ../devkit/api-manual.md。

## 是什么

`json` 是 handler 的**响应写口**。业务 handler 不直接返回 HTTP 响应，而是调用
`json.ok` / `json.fail` 等写入 `{code,msg,data}` 统一信封（`src/bridge/envelope.rs`），
由 serve 层捕获（`Capture`）后写回 HTTP 响应。每次调用都会标记会话完成（`done`），
一次请求只应写一次响应。

`finish()`（`bootstrap.js` → `op_finish`）只标记会话完成、**不写任何响应体**——
主要用于 `oj test` 测试文件标记用例结束（HTTP handler 里不需要它）。

## API 表

| API | 签名 | 说明 |
|---|---|---|
| `json.ok` | `ok(data?: unknown): void` | 成功信封 `{"code":0,"msg":"ok","data":...}`，HTTP 200 |
| `json.fail` | `fail(code: number, msg: string, data?: unknown): void` | 失败信封 `{"code":code,"msg":...,"data":...}`，HTTP 状态 = `code`（`code<=0` 映射 500；超 u16 范围 clamp 到 65535，信封内 code 保留原值） |
| `json.header` | `header(name: string, value: string): void` | 追加响应头（有序、同名可重复）：`Set-Cookie` 合法多值并存（登录双发 `oj_sess` + `oj_csrf` 的通道）；其余同名头**最后一个生效**。空名忽略 |
| `json.redirect` | `redirect(url: string, code?: number): void` | 3xx 重定向：写 `Location` 头 + RFC 9110 §15.4 短超文本注记（HEAD 请求为空 body），content-type 默认 `text/html; charset=utf-8`。`code` 缺省 302；**非 3xx 一律回落 302**（杜绝「200 + Location」畸形响应）；空 url 不写 `Location` |
| `json.redirect.*` | `movedPermanently(url)` / `found(url)` / `seeOther(url)` / `temporaryRedirect(url)` / `permanentRedirect(url)` | 具名封装：301 / 302 / 303（跟随后改 GET）/ 307（方法/体保持）/ 308（永久 + 方法保持） |
| `json.raw` | `raw(data?: unknown): void` | **裸 JSON 200，无信封**。对外标准协议端点用（OIDC discovery/jwks/token 等）；错误仍走 `json.fail` 信封 |
| `json.stream` | `stream(opts?: { status?: number; contentType?: string }): { write(chunk), end() }` | 流式响应（v0.1.35）：绕过信封直接写裸 body；`write` 接受 string/ArrayBuffer/ArrayBufferView；不调 `end()` 也会在响应收尾阶段自动关流 |
| `json.sse` | `sse(opts?: { status?: number; heartbeatSecs?: number }): { write(chunk), end() }` | SSE 流：强制 `Content-Type: text/event-stream`，每次 `write` 自动包成 `data: <内容>\n\n` 帧 |
| `finish` | `finish(): void` | 仅标记会话完成（不写响应）。HTTP handler 用 `json.*` 即可；`finish` 是测试 SDK 的结束标记 |

行为要点：

- **content-type 默认值**：`ok`/`fail`/`raw` 在未显式 `json.header("content-type", ...)`
  时自动补 `application/json`（大小写不敏感判定，显式设置优先）；`redirect` 默认补
  `text/html; charset=utf-8`。
- **BigInt 安全**（v0.1.22）：`data` 在 JS 侧经 `ojStringify` 序列化，其中的 BigInt
  一律写成**十进制字符串**（如 `{id: 4886674138783273204n}` → `"4886674138783273204"`），
  不会因 `JSON.stringify(BigInt)` 抛 TypeError 把响应打成 500。
- **dev SQL 追踪**（v0.1.51）：追踪开启且本请求有 SQL 时，`ok`/`fail` 信封自动附加
  `_sql` 兄弟字段（本请求 SQL 画像，参数恒脱敏）。详见 [03-log.md](03-log.md)。

## 错误

| 场景 | 行为 |
|---|---|
| `json.fail(code<=0, ...)` | 信封 code 与 HTTP 状态均取 500 |
| `json.fail(70000, ...)` | HTTP 状态 clamp 到 65535；信封 body 内 code 保留 70000 全精度 |
| `json.redirect(url, 200/404/...)` | 非 3xx 一律回落 302 |
| handler 抛异常 / 超时 | serve 层兜底：异常 → 500 信封 `{code:500,msg,...}`；超时熔断 → 408 |
| handler 未写任何响应 | HTTP 500、空 body（`Capture` 默认 status=0 非法，serve 层映射 500）——业务代码必须调用一次 `json.*` |

## 限制

- **一次请求写一次响应**：`ok`/`fail`/`raw`/`redirect` 都置 `done`，后写的覆盖先写的
  （`s.response` 被替换）；不要指望两次 `json.ok` 合并输出。
- `json.raw` 只负责「裸 JSON」；要裸的非 JSON 内容请用 `json.stream` + `contentType`。
- `json.stream`/`json.sse` 绕过信封：首字节即裸 body，无法在流中途改状态码；
  SSE 不支持 `event:`/`id:` 字段（需自行拼进 `write` 内容），心跳保活间隔 15s（`:\n\n`）。
- `finish()` 在 HTTP handler 里不等于「返回 200」——它只是 done 标记，响应体仍为空
  （serve 层会按上表兜底 500）。HTTP 路径请用 `json.ok(...)` 收尾。

## 案例

### 统一错误返回 + 请求 id 头

```ts
// src/user/detail/api.ts
async function get() {
  const id = Number(http.param("id", 0));
  json.header("X-Request-Id", crypto.randomHex(8));
  if (!id) { json.fail(400, "id required"); return; }
  const rows = await db.table("account").select(["id", "name"])
    .where({ field: "id", op: "eq", value: id }).all();
  if (!rows.length) { json.fail(404, "no such account", { id }); return; }
  json.ok(rows[0]);
}
export default { get };
```

```bash
curl -i 'http://localhost:9778/v1/api/user/detail/?id=1'
# HTTP/1.1 200 OK
# content-type: application/json
# x-request-id: 3f9a1c2b7e4d05f6
#
# {"code":0,"msg":"ok","data":{"id":1,"name":"alice"}}
```

### 登录端点：Set-Cookie 双发 + 302 跳转

```ts
// src/auth/login/api.ts —— Set-Cookie 是同响应多值的合法通道（其余同名头最后一个生效）
async function post() {
  const { name, password } = http.body ?? {};
  const rows = await db.table("account").select(["id", "pwd"]).limit(1).all();
  if (!rows.length || !(await bcrypt.verify(password, rows[0].pwd))) {
    json.fail(401, "bad credentials"); return;
  }
  json.header("Set-Cookie", "oj_sess=...; HttpOnly; Path=/");
  json.header("Set-Cookie", "oj_csrf=...; SameSite=Lax; Path=/");
  json.redirect.seeOther("/v1/api/user/me/");   // POST 后 303：跟随后一律 GET
}
export default { post };
```

### OIDC 风格裸 JSON 端点 + SSE 推送

```ts
// src/wellknown/jwks/api.ts —— 标准协议端点说裸 JSON，不带 {code,msg,data} 信封
function get() {
  json.raw({ keys: [] });
}
export default { get };
```

```ts
// src/notice/stream/api.ts —— SSE：绕过信封，write 自动包 data: 帧
async function get() {
  const s = json.sse();
  s.write(JSON.stringify({ hello: "world" }));
  s.end();
}
export default { get };
```

```bash
curl -N http://localhost:9778/v1/api/notice/stream/
# data: {"hello":"world"}
```
