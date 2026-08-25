# L1 测试框架 — DX / 测试体验评审（仅评审，未改动任何源文件）

> 评审对象：`.codebuddy/plans/stellar-beacon-einstein-ab6EKJ0B.md` 中的 L1「sample API 测试框架」方案
> 核对样本：`sample/src/user/account/api.ts`、`sample/src/news/api.ts`、`sample/src/news/WS.ts`、`sample/global.d.ts`、`sample/README.md`
> 角色：测试框架 / DX 专家（对标 Go Fiber `app.Test`、Express supertest、vitest）

---

## 0. 总评（结论先行）

方向**正确且必要**：在真实 `deno_core` 运行时内跑用户 TS 测试、经 `client` 做黑盒 HTTP、自带轻量 `describe/it/expect`，是对标 Fiber `app.Test` / supertest 的合理映射。**不能复用 vitest**（vitest 跑在 Node+Vite，L1 跑在 `deno_core` isolate，`client/bus/db` 等全局只存在于 isolate 内）也站得住脚。

但既有草案（已存在于本文件）有 **3 处会在落地时直接绊倒开发者** 的硬伤，必须在实现前修正：

1. **信封形状假设错误**：黑盒下 `client` 拿到的是 `json.ok/fail` 写出的信封文本，`res.json()` 返回的是信封（如 `{ok,data,code,msg}`），不是裸 data。草案示例 `expect(await res.json()).toEqual({created:true})` 会**直接失败**。
2. **约定不一致**：本代码库所有运行时全局（`db/http/json/ws/fetch`）都是 `global.d.ts` 里的**环境全局（ambient global）**，不需要 import。草案却让测试 `import { client, describe } from "ext:core/test"`——这会和全库约定割裂，应改为同为 ambient global。
3. **`bus` 全局根本没声明**：`news/api.ts:6`、`WS.ts:4` 都用了 `bus`，但 `global.d.ts`（88–114 行）完全没有 `bus` 声明。这既是生产代码的编辑器报错隐患，也是测试 `client.subscribe` 设计的前提（总线必须先在类型与运行时里被正式建模）。

下面逐题给「合理 / 风险 / 反对 + 具体改进」，并附修正后的 TS 草案与必须修正清单。

---

## 1. 对标 Fiber `app.Test` 与 supertest：`client` API

### 1.1 `client.get(path, {headers, body})` 是否顺手？
- **合理**：GET 无 body，只传 `{headers}/{query}` 直观；POST/PUT 带 body，`{headers, body}` 也直观。
- **修正**：方法名**刻意对齐 handler 导出名**。核对 `api.ts:51`：`export default { get, post, put, del, patch, head, options }`。让 `client.get/post/put/del/patch/head/options` 与之一一对应，开发者"看到路由方法就能写出 client 调用"，迁移成本最低。
- 同时保留统一入口 `client.request(method, path, opts)` 供自定义方法（REPORT/WebDAV）与框架内部统一 dispatch，避免 N 个方法各写一遍。

### 1.2 返回对象应暴露哪些字段？
- **合理（复用 `bootstrap.js` 的 `fetch` 响应形态）**：`status / statusText / headers / ok / json() / text() / body / arrayBuffer()`。和 `fetch` 同构，测试里既会 `client.get` 也会 `fetch`，两套响应形态会分裂心智。
- **⚠️ 关键修正（信封）**：黑盒下 `ClientResponse.body` 必须是 handler 实际写出的**信封文本**（`api.ts:8` `json.ok(r)`、`api.ts:13` `json.fail(400,...)`）。即 `await res.json()` 返回的是信封 `{ ok: true, data: {...}, code?, msg? }`，**不是裸 data**。因此：
  - 草案示例 `expect(await res.json()).toEqual({ created: true })` 会失败，应改为 `expect((await res.json()).data).toEqual({ created: true })`；
  - 或在 `ClientResponse` 上额外提供 `res.data`（= 解析信封后的 `data` 字段）作为便捷只读属性，保留 `res.json()` 的忠实信封形态。
  - `res.status` 取信封 `code`（或 server 为 `json.fail(code,…)` 设的 HTTP 状态），保持与真实 HTTP 一致。

### 1.3 是否需要链式 `client.get(...).expect(200).json()`？
- **反对把链式作为主路径**（supertest 风格）。`client.get` 必然返回 `Promise<ClientResponse>`（handler 是 async，见 `news/api.ts:4` `async post`），在 Promise 上挂 `.expect` 需自定义 thenable，与 `await` 混用易踩坑。
- 主路径就是 `await res + expect(...)`（现代、可组合、报错栈干净）。链式 thenable 仅作为**可选糖**，非 MVP。

### 1.4 推荐的 `client` 接口（TS 草案，见第 9 节）

---

## 2. 自带 `describe/it/expect` 是否必要？

**结论：必要，理由硬。**
- **不能用 vitest**：vitest 依赖 Node + Vite，`client/bus/db` 等全局只存在于 `deno_core` isolate，vitest 的 Node 环境拿不到；在 Node 里嵌 deno_core 再抹平模块系统的成本远超自带 ~200 行微型框架。
- **不能用 `Deno.test`**：本架构用的是裸 `deno_core`（不是完整 Deno CLI），没有 `Deno.test`。
- 因此必须自带，且只做两件事：① 注册用例；② 断言。

### 2.1 最小功能集（MVP）
- `describe(name, fn)`：分组（可嵌套）。
- `it(name, fn)`：用例，`fn` 可 async（handler 是 async，测试必 `await`）。
- `beforeEach(fn)` / `afterEach(fn)`：**刚需**。黑盒测试每个用例要独立状态（DB 重置、种子、bus 订阅清理）。
- `expect(actual)` + 断言方法。

### 2.2 注册式 JS 草稿（绝不自带 exit）
```ts
// describe/it 只入队，由宿主（Rust test_cmd）统一驱动执行，规避"运行时内 exit 跳过析构"。
type MatcherResult = { pass: boolean; message: string; actual?: unknown; expected?: unknown };

interface Expect {
  toBe(expected: unknown): void;          // 引用/原始值相等
  toEqual(expected: unknown): void;       // 结构深比较（JSON 序列化比对满足 MVP）
  toContain(item: unknown): void;
  toBeUndefined(): void;
  toBeNull(): void;
  toBeTruthy(): void;
  toBeFalsy(): void;
  toMatch(re: RegExp): void;
  toThrow(): void;
  toHaveStatus(code: number): void;       // 糖：actual 为 ClientResponse 时等价 expect(res.status).toBe(code)
  toHaveData(expected: unknown): void;    // 糖：expect(res).toHaveData({created:true}) == expect(res.data).toEqual(...)
}
declare function describe(name: string, fn: () => void): void;
declare function it(name: string, fn: () => Promise<void> | void): void;
declare function beforeEach(fn: () => Promise<void> | void): void;
declare function afterEach(fn: () => Promise<void> | void): void;
declare function expect(actual: unknown): Expect;

// 宿主读取的聚合结果（避免任何 in-runtime exit）：
//   globalThis.__ojTestReport = { files: [{path, passed, failed, cases:[...]}], ... }
```

---

## 3. 黑盒 vs 白盒；典型 `.test.ts`

### 3.1 黑盒（经 `client` HTTP）为 L1 唯一官方形态 — **合理**
- 黑盒走完整管线：路由解析 → 鉴权中间件 → 租户注入 → handler → 信封格式化，**测的是用户真正怎么调**。
- 白盒（`import` handler 直接调 `get()`）会绕过整条 server 管线：`http.body`/`json` 等全局在测试 isolate 无请求上下文、鉴权/租户/middleware 全失效，收益低且与运行时模型冲突。白盒留作"未来可选"，不在 MVP。

### 3.2 发现与布局
- **推荐 `**/*.test.ts` 全局发现，与 `api.ts` 共置**（vitest 习惯）：`sample/src/user/account/api.test.ts` 紧挨 `api.ts`。黑盒不依赖 import，无循环依赖顾虑。

### 3.3 隔离策略（重要坑）
- 每测试文件一个 isolate，或每 `describe` 重置 `ReqState`。
- ⚠️ **新增发现 — DB 状态串味**：样本用 `db`（默认 `db.sqlite`，由 `seed.sql` 初始化，README）。黑盒测试会 `INSERT/DELETE`（`api.ts:16,32`），若与 dev 共用 `db.sqlite` 且不清，用例顺序会影响结果。必须：① 测试指向临时/in-memory DB，或 ② 提供 `resetFixtures()` 项目助手 + `beforeEach` 清理。这是落地前的**必须项**，草案未提及。

### 3.4 典型 `.test.ts`（修正后形态，注意信封）
```ts
// sample/src/user/account/api.test.ts
describe("user/account", () => {
  beforeEach(async () => {
    await resetFixtures();            // 见 3.3：DB 状态隔离，刚需
  });

  it("GET 列表返回 200 且为数组", async () => {
    const res = await client.get("/v1/api/user/account/");
    expect(res.status).toBe(200);
    const env = await res.json();     // 信封，不是裸 data
    expect(Array.isArray(env.data)).toBeTruthy();
  });

  it("POST 缺 name 返回 400", async () => {
    const res = await client.post("/v1/api/user/account/", { body: { role: "user" } });
    expect(res.status).toBe(400);
    expect(await res.json()).toHaveData(undefined); // 或断言 env.msg
  });

  it("POST 正常创建", async () => {
    const res = await client.post("/v1/api/user/account/", { body: { name: "alice", role: "admin" } });
    expect(res.status).toBe(200);
    expect(await res.json()).toHaveData({ created: true }); // 修正：比对待测的是 env.data
  });

  it("带租户头隔离数据", async () => {
    const res = await client.get("/v1/api/user/account/", { tenant: "t1" });
    expect(res.status).toBe(200);
  });
});
```

---

## 4. 断言 ergonomics 与鉴权用例

### 4.1 `toHaveStatus(200)` 还是 `assertEq`？
- **主路径 `expect(res.status).toBe(200)`**（与 jest/vitest 一致，零学习成本）。
- **反对引入 `assertEq` 平行 API**：两套断言体系并存是 DX 灾难。只保留 `expect(...).toBe/...`。
- `toHaveStatus` 仅作糖（已含），非必须。

### 4.2 login → token → Authorization，怎么优雅？
- **反对 `client.login()` 做成框架内置**：各家登录端点/签名/JWT 各异，框架不该假定。
- **两层方案（合理）**：
  1. 框架给通用**注入原语**（已在 `ClientRequestOptions`）：`client.get(path, { auth: {id, roles} })` 注入 user claims；`client.get(path, { tenant: "t1" })` 注入 `tenant_id`。两者走与真实请求同一 `RequestInfo.user/tenant_id` 字段，等于在"鉴权中间件之后"切入，既测业务又无需伪造 token。
  2. 项目级 `tests/helpers.ts` 自写 `login()`：
     ```ts
     export async function login(email: string, pw: string) {
       const res = await client.post("/v1/api/auth/login", { body: { email, pw } });
       const { token } = await res.json();
       return { token, auth: () => ({ headers: { Authorization: `Bearer ${token}` } }) };
     }
     ```
- ⚠️ **补充说明（样本现状）**：`api.ts` 里 `requireRole` 校验的是 **body 的 role 字段**，并无读取 `Authorization`/`RequestInfo.user` 的路由守卫。即样本当前**没有真实鉴权中间件**，`auth` 注入的收益是"为未来中间件预留"，而非当下可演示。框架仍应按此设计（不绑架业务），但示例文档需说清这一点，避免给人"已端到端测了鉴权"的错觉。

---

## 5. 报告与退出码

### 5.1 TAP 还是 vitest 风格？
- **主报告 vitest 风格**（人读友好）：`✓ file A (3 passed)  ✗ file B (1 failed)` + 末尾 `Test Files X failed / Y total; Tests Z failed`。
- 提供 `--tap` 开关输出 TAP（`ok 1 - name` / `not ok 2 - name`），供 CI 消费。**两者都要**。

### 5.2 失败信息呈现
- 每条失败：`用例名` + `期望 vs 实际` + 简易 diff。
- MVP 用 `JSON.stringify(actual/expected, 2)` 并高亮首个分歧字段即可；要真 diff 可 vendored 一个 ~3KB 的 `diff`（样本已用 vendor ESM 模式，见 README `node_modules/escape-goat`）。
- 断言抛出的 `Error.message` 应已含 `actual/expected`，宿主只负责排版。

### 5.3 `process::exit` 时机（架构师已指出）
- **反对**在 JS 运行时内任何位置 `process.exit` / `Deno.exit`——会跳过 V8 isolate 与 Rust 侧析构（`RuntimePool` 未 `checkin`、bus 订阅未清理、句柄泄漏）。
- **正确模型**：`describe/it` 只注册（`globalThis.__ojTestReport` 累积）；宿主 `test_cmd::run` 驱动执行、收集结果、**返回 `i32` 退出码**（`0` 全过 / `1` 有用例失败 / `2` 用法或配置错误）；`oj/src/main.rs` 主流程 `std::process::exit(code)` 复用现有 `run_command -> i32` 约定。即"exit 决策永远在 Rust `main` 层，JS 层只生产报告"。

---

## 6. WS / 发布订阅在 L1 不可行

### 6.1 真实 WS upgrade 确实不可行（确认）
- `ws` 全局的 `send/close` 只是把帧收集进 `ReqState`，真正的 upgrade 握手在 server 传输层；oneshot `deno_core` 测试运行时没有监听 socket、没有 upgrade 监听器，**真实 WS 连接无法建立**。方案"WS 用例移到桥层"事实正确。

### 6.2 但"改到桥层就完了"对 DX 不够 — 必须暴露 `client.subscribe(topic)`
- 业务真正关心的是 `publish → 总线 → 订阅者收到广播`（`news/api.ts:6` `bus.publish("news", …)` ↔ `WS.ts:4` `bus.subscribe("news")`）。
- 扇出下沉桥层后，桥层本就持有 bus，最自然的 DX 是 `client.subscribe("news")` **直接 tap 进程内总线**：
  ```ts
  it("发布后订阅者收到广播", async () => {
    const sub = client.subscribe("news");    // 直接 tap 总线（进程内）
    await client.post("/v1/api/news", { body: { text: "hi" } });
    const msg = await sub.next();             // 等下一条广播
    expect(msg).toEqual({ text: "hi" });      // 测的是真实 bus 链路
    sub.unsubscribe();
  });
  ```
- 这样测的是 handler 真实调用的 `bus.publish`（同总线），未覆盖的仅是"WS 升级 + 帧编码"这一纯传输细节（交给独立 Rust 集成测试）。**结论：扇出下沉桥层可接受，但必须配套 `client.subscribe(topic)`，否则发布订阅在 L1 完全不可测，是 DX 倒退。**

---

## 7. 对既有草案的关键修正（新增发现）

| # | 发现 | 影响 | 修正 |
|---|------|------|------|
| A | **信封形状假设错误** | 草案 `toEqual({created:true})` 在黑盒下会失败 | `res.json()` 返回信封；测试断言 `env.data`，或 `ClientResponse` 提供 `res.data` 便捷字段（见 1.2/3.4） |
| B | **约定不一致（import vs global）** | 草案让测试 `import` 测试全局，与全库 ambient-global 约定割裂 | 测试全局（`client/describe/it/...`）改为 `global.d.ts` 里的 ambient global，无需 import（见第 9 节） |
| C | **`bus` 全局未声明** | `global.d.ts` 缺 `bus`，生产+测试编辑器均报错；且 `client.subscribe` 前提是总线被正式建模 | 在 `global.d.ts` 增补 `BusApi` 与 `const bus`（见第 9 节） |
| D | **DB 状态隔离缺失** | 黑盒 INSERT/DELETE 与 dev `db.sqlite` 共用会串味 | 测试指向临时/in-memory DB 或 `resetFixtures()` + `beforeEach` 清理（见 3.3） |
| E | **matcher 集合补充** | 仅 `toHaveStatus` 不够顺手 | 增加 `toHaveData(expected)` 糖（见 2.2） |

---

## 8. DX / 测试框架层「必须修正」清单

| # | 项 | 状态 | 修正 |
|---|----|------|------|
| 1 | `client` 返回形态 | 风险 | 复用 `bootstrap.js` `fetch` 的 `ClientResponse`；并明确**信封契约**（`res.json()`=信封，`res.data`=内裹数据）|
| 2 | `client` 入口 | 合理 | 便利方法对齐 handler 导出名（`get/post/put/del/patch/head/options`，见 `api.ts:51`）+ 统一 `client.request` |
| 3 | `client` 实现 | 合理 | 进程内构造 `RequestInfo`（含 `tenant_id/user`）复用 `run_module`+信封捕获，等价 `app.Test(req)`，不起 server |
| 4 | 自带框架必要性 | 合理 | 确认；vitest（Node）与 `Deno.test`（无完整 Deno）均不可用 |
| 5 | 框架最小集 | 合理 | `describe/it/beforeEach/afterEach/expect`；`beforeEach/afterEach` 为刚需 |
| 6 | 退出码 | 反对现状 | **绝不 in-runtime exit**；`test_cmd::run -> i32`，`main` 调 `process::exit` |
| 7 | 报告格式 | 风险 | vitest 风格为主 + `--tap` 开关；失败附 actual/expected + 简易 diff |
| 8 | WS/PubSub | 风险 | 扇出下沉桥层可接受，但**必须**暴露 `client.subscribe(topic)` tap 进程内 bus |
| 9 | 鉴权 ergonomics | 风险 | 框架给 `auth`/`tenant`/`headers` 原语；`client.login()` 不内置，放项目 `tests/helpers.ts` |
| 10 | 断言 API | 反对 | 只保留 `expect(...).toBe/toEqual/...`；**不引入 `assertEq`**；`toHaveStatus`/`toHaveData` 仅糖 |
| 11 | 测试发现/布局 | 合理 | glob `**/*.test.ts`，与 `api.ts` 共置；每文件/每 describe 重置 `ReqState` 强隔离 |
| 12 | 编辑器类型 | 风险 | `global.d.ts` 增补测试全局（**ambient global，非 import**）+ **`bus` 声明** |
| 13 | **DB 状态隔离** | 风险(新增) | 测试用临时/in-memory DB 或 `resetFixtures()` + `beforeEach`，避免与 dev `db.sqlite` 串味 |
| 14 | **信封契约文档** | 风险(新增) | 明确 `res.json()`=信封、提供 `res.data`；修正文档中所有 `toEqual(裸 data)` 示例 |

---

## 9. 推荐落地接口总览（TS 草案）

### 9.1 `sample/global.d.ts` 增补（同为 ambient global，沿用全库约定）
```ts
// ---- 新增：bus 全局（样本 news/api.ts、WS.ts 已使用但未声明）----
interface BusApi {
  publish(topic: string, payload: unknown): Promise<void> | void;
  subscribe(topic: string): void;
  unsubscribe(topic: string): void;
}

// ---- 新增：L1 自带测试框架（ambient global，无需 import）----
interface ClientResponse {
  status: number;                 // 取自信封 code / server 为 json.fail 设的状态
  statusText: string;
  ok: boolean;                    // status < 400
  headers: Record<string, string>;
  body: string;                   // 原始响应体（信封文本）
  json(): Promise<any>;           // 解析信封 { ok, data?, code?, msg? }
  text(): Promise<string>;
  arrayBuffer(): Promise<Uint8Array>;
  data?: any;                     // 便捷：解析后的信封 data 字段（黑盒友好）
}

interface ClientRequestOptions {
  headers?: Record<string, string>;
  query?: Record<string, string>;
  body?: unknown;                 // object→JSON.stringify；string→原样；undefined→空
  auth?: { id: string; roles: string[]; [k: string]: unknown }; // 注入 user claims（黑盒鉴权）
  tenant?: string;                // 注入 tenant_id（黑盒租户）
}

interface ClientSubscription {
  next(): Promise<unknown>;            // 解析为下一条被广播的 payload
  collect(timeoutMs?: number): Promise<unknown[]>; // 收集窗口内全部消息
  unsubscribe(): void;
}

interface Client {
  request(method: string, path: string, opts?: ClientRequestOptions): Promise<ClientResponse>;
  get(p: string, o?: ClientRequestOptions): Promise<ClientResponse>;
  post(p: string, o?: ClientRequestOptions): Promise<ClientResponse>;
  put(p: string, o?: ClientRequestOptions): Promise<ClientResponse>;
  del(p: string, o?: ClientRequestOptions): Promise<ClientResponse>;
  patch(p: string, o?: ClientRequestOptions): Promise<ClientResponse>;
  head(p: string, o?: ClientRequestOptions): Promise<ClientResponse>;
  options(p: string, o?: ClientRequestOptions): Promise<ClientResponse>;
  subscribe(topic: string): ClientSubscription;   // 直接 tap 进程内总线
}

interface Expect {
  toBe(expected: unknown): void;
  toEqual(expected: unknown): void;
  toContain(item: unknown): void;
  toBeUndefined(): void;
  toBeNull(): void;
  toBeTruthy(): void;
  toBeFalsy(): void;
  toMatch(re: RegExp): void;
  toThrow(): void;
  toHaveStatus(code: number): void;
  toHaveData(expected: unknown): void;
}

declare global {
  // —— 既有全局 ——
  const json: JsonApi;
  const http: HttpApi;
  const log: Logger;
  const kv: KVWithDel;
  const redis: KVApi;
  const ws: WSApi;
  const bus: BusApi;              // 新增：补样本缺口
  function DB(name: string): DBInstance | undefined;
  const db: DBInstance;
  function finish(): void;
  function __ojRequire(name: string, referrerPath?: string): any;
  function fetch(url: string, options?: { method?: string; headers?: Record<string, string>; body?: string | null }): Promise<any>;

  // —— 新增：测试框架全局 ——
  const client: Client;
  function describe(name: string, fn: () => void): void;
  function it(name: string, fn: () => Promise<void> | void): void;
  function beforeEach(fn: () => Promise<void> | void): void;
  function afterEach(fn: () => Promise<void> | void): void;
  function expect(actual: unknown): Expect;
}
```

### 9.2 宿主契约
- `describe/it` 仅入队 → 宿主 `test_cmd::run` 驱动执行、累计结果，写入 `globalThis.__ojTestReport`。
- `test_cmd::run() -> i32`：`0` 全过 / `1` 有用例失败 / `2` 用法或配置错误；`oj/src/main.rs` 调 `std::process::exit(code)`。
- Rust 侧 `client.request` 复用 server 的"路由解析 + `run_module` + 捕获信封"路径，在进程内构造 `RequestInfo` dispatch，包成 `ClientResponse`——即 Fiber `app.Test(req)` 语义，无需起 socket/server。

---

## 10. 一句话总结
方案方向对、必要性硬；落地前**必须**修正：① 信封契约（`res.json()`=信封、`res.data`=内裹）；② 测试全局改为 ambient global（沿用全库约定）；③ 补 `bus` 类型声明；④ DB 状态隔离；⑤ 退出码只在 Rust `main` 层决策。其余（对齐 handler 方法名、`subscribe(topic)` 测真实总线、自带轻量框架、`auth/tenant` 原语 + 项目级 `login` 助手、vitest 风格报告 + `--tap`）均为合理设计，照此实现即可。

---

## 11. 阶段 D — L2（Node + vitest 纯 mock 单测骨架）评审

> 说明：本文件此前只有 L1 评审（第 1–10 节），**并不存在「阶段 D / L2」一节**。以下为按用户要求补写的 L2 设计评审 + 修正草案（仅评审，未改动 `sample/` 任何源文件）。
> 核对样本：`global.d.ts`、`src/user/account/api.ts`、`src/user/_shared/validate.ts`、`src/news/api.ts`、`src/news/WS.ts`、`package.json`、`tsconfig.json`。

### 11.0 总评（结论先行）

L2 与 L1 **本质不同**，草案最容易踩的坑是把 L1 的思路照搬到 L2：

- **L1** 跑在 `deno_core` isolate 内，vitest 进不去，所以**必须自带** `describe/it/expect`。
- **L2** 跑在 **Node + vitest**，vitest 自带完整的 `describe/it/expect/beforeEach` 与报告/退出码。**L2 应直接复用 vitest，只 mock oj 全局**，不要把 L1 那套 ~200 行微型框架再写一遍。这是范围上最大的一处纠偏（见 11.6 #1）。

可行性结论：**高**。handler 只依赖两类东西——ambient 全局（`json/db/http/bus…`）+ 相对 import（`../_shared/validate`）。vitest 默认用 esbuild 转译 TS，能直接加载 `.ts` 并解析相对 import；把 mock 挂到 `globalThis` 后，源码里裸写的 `json`/`db` 在运行时自然解析到 `globalThis.json`（Node ESM 未声明标识符回退到 globalThis），无需改任何业务代码。

关键约束（务必写进文档）：**L2 是白盒**，绕过整条 server 管线（路由解析→鉴权→租户注入→信封外包装）。它测的是「给定 http 上下文 + mock 全局，handler 内部逻辑对不对」，不是「用户真实怎么调」。auth/tenant/413/WS/路由 不在 L2 能力内（见 11.4）。

---

### 11.1 Q1：按 global.d.ts 实现 oj 全局内存 stub 的可行性

**结论：可行，且多数全局只需极薄实现。重点在 `db` 与 `bus`。**

#### 各全局最小可用集（基于真实用法 grep）
| 全局 | 真实用法 | L2 最小实现 |
|---|---|---|
| `json` | `ok(data)` / `fail(code,msg,data)` / `header(k,v)` | 把调用转交给「当前用例的 deferred sink」（invoke 用），并缓存 `header` |
| `http` | `param(n,def)` / `body` / `user`(auth_demo) / `files`+`file(i)`(upload) | 可变 per-request 对象，`param` 从 `params[name]??def` 取 |
| `db` | `query(sql,params)` / `exec(sql,params)`（**示例 handler 完全没用 `table()`**） | 见下 |
| `bus` | `publish(topic,payload)` / `subscribe(topic)` | in-process pub/sub，**记录消息供断言**（news） |
| `kv` | `get/set/del`（order/detail） | `Map` -backed |
| `redis` | 无真实使用 | `get→null / set→true` no-op |
| `ws` | 无真实使用（仅注释） | `send→记录` / `close→no-op` |
| `log` | 无断言需求 | 4 个空函数 |
| `fetch` | 无真实使用 | 返回 dummy `Response` |
| `finish` / `__ojRequire` | 无真实使用 | `finish→no-op`；`__ojRequire→throw`（留作 TODO） |

#### `db` 内存 stub：要不要实现 `table()` 链式构造器？→ **MVP 不实现**
真实代码里 `db.table(...) ` 一处都没出现（grep 全为 `db.query`/`db.exec`）。`DBInstance.table` 是 **global.d.ts 的契约冗余**，对 L2 MVP 是死代码。建议：
- MVP：`query`/`exec` 做成「**脚本化 mock + 轻量内存表**」二合一（见草案）。`table()` 先抛 `not implemented` 或返回空构造器占位即可，不在 MVP。
- 识别表名要不要做？→ **做，但只解析表名 + 单个 `= ?` 等值条件就够覆盖全部示例 SQL**（select/list/insert/delete）。真正的 UPDATE SET 解析可省略（示例 `post` 只关心返回 `{created:true}`，不读回写结果）；需要精确断言时改用 `onExec(sql, n)` 强制返回。

#### in-memory 数据如何设计？→ 最小可用 = `Map<table, Row[]>` + 正则取表名 + 单个等值 where
不要上真 SQL 引擎。下面草案约 60 行即可让 `account/api` 真正「插入→读取」跑通，比纯脚本回放更有单测价值；同时保留 `onQuery/onExec` 覆盖引擎解析不了的 SQL。

```ts
// sample/test/mock/db.ts
type Row = Record<string, unknown>;

export interface DbMock {
  seed(table: string, rows: Row[]): void;
  onQuery(sql: string, rows: Row[]): void;   // 纯 mock：强制返回
  onExec(sql: string, n: number): void;      // 纯 mock：强制返回
  reset(): void;
  instance: any;
}

export function createDbMock(): DbMock {
  const tables = new Map<string, Row[]>();
  const qOv = new Map<string, Row[]>();
  const eOv = new Map<string, number>();

  const tableOf = (sql: string) => sql.match(/\b(?:from|into|update)\s+([a-zA-Z_]\w*)/i)?.[1] ?? null;
  const whereEq = (sql: string) => {
    const m = sql.match(/where\s+([a-zA-Z_]\w*)\s*=\s*\?/i);
    if (!m) return null;
    const q = [...sql.matchAll(/\?/g)];
    return { col: m[1], idx: q.length - 1 };
  };

  const instance = {
    async query(sql: string, params: unknown[] = []): Promise<Row[]> {
      if (qOv.has(sql)) return qOv.get(sql)!;
      const t = tableOf(sql); if (!t || !tables.has(t)) return [];
      let rows = tables.get(t)!.map(r => ({ ...r }));
      const w = whereEq(sql);
      if (w) rows = rows.filter(r => r[w.col] == params[w.idx]);
      return rows;
    },
    async exec(sql: string, params: unknown[] = []): Promise<number> {
      if (eOv.has(sql)) return eOv.get(sql)!;
      const t = tableOf(sql); if (!t) return 0;
      if (!tables.has(t)) tables.set(t, []);
      const rows = tables.get(t)!;
      if (/^insert/i.test(sql)) {
        const cols = (sql.match(/\(([^)]*)\)\s*values/i)?.[1] ?? '').split(',').map(s => s.trim());
        const row: Row = {}; cols.forEach((c, i) => (row[c] = params[i]));
        if (!('id' in row)) row.id = rows.length + 1;
        rows.push(row); return 1;
      }
      if (/^update/i.test(sql)) { const w = whereEq(sql); return w ? rows.filter(r => r[w.col] == params[w.idx]).length : 0; }
      if (/^delete/i.test(sql)) { const w = whereEq(sql); if (!w) return 0; const b = rows.length; tables.set(t, rows.filter(r => r[w.col] != params[w.idx])); return b - tables.get(t)!.length; }
      return 0;
    },
    table(name: string) {                       // 占位：示例未用；返回空构造器
      const chain: any = { select: () => chain, where: () => chain, orderBy: () => chain, limit: () => chain, offset: () => chain, all: async () => (tables.get(name) ?? []).map(r => ({ ...r })) };
      return chain;
    },
  };

  return {
    seed: (t, r) => tables.set(t, r.map(x => ({ ...x }))),
    onQuery: (s, r) => qOv.set(s, r),
    onExec: (s, n) => eOv.set(s, n),
    reset: () => { tables.clear(); qOv.clear(); eOv.clear(); },
    instance,
  };
}
```

---

### 11.2 Q2：invoke 助手 —— 设置/还原全局、捕获 json.ok/fail、处理 async

handler 用 `db.query(...).then(r => json.ok(r))` 这种 **Promise.then 异步写包络**，且 `post` 的 `json.fail(400)` 是**同步**调用；`del` 的 `positiveId` 非法会**同步抛异常**。invoke 必须同时兜住三种情况。

设计要点：
1. 每次 invoke **重置 request 上下文**（`resetRequest`）+ 重新挂 `json` 的 sink（deferred）。
2. `json.ok/fail` 调用时通过 sink 解封 Promise；设 **2s 安全超时**（防 handler 忘了写包络导致挂死）。
3. handler 内部同步抛（`del` 非法 id）→ 捕获后 `reject`，交给 vitest 的 `.rejects.toThrow()` 断言。
4. handler 返回 `undefined`（void）或 thenable → 用 `Promise.resolve(ret).catch` 兜底 500。

```ts
// sample/test/mock/ojMock.ts （节选：全局安装 + 包络 sink）
import { createDbMock } from './db';

export const oj: any = { db: null, http: {}, jsonHeaders: {}, busLog: {}, _sink: null };

export function installGlobals() {
  oj.db = createDbMock();
  oj.http = {
    method: 'GET', params: {}, query: {}, headers: {}, body: null, user: undefined, files: undefined,
    param(name: string, def = '') { return this.params[name] ?? def; },
    file(i: number) { return Promise.resolve((this.files ?? [])[i]); },
  };
  oj.jsonHeaders = {}; oj.busLog = {}; oj._sink = null;

  const json = {
    ok: (data?: unknown) => oj._sink?.({ ok: true, data, code: 200, headers: { ...oj.jsonHeaders } }),
    fail: (code: number, msg: string, data?: unknown) => oj._sink?.({ ok: false, code, msg, data, headers: { ...oj.jsonHeaders } }),
    header: (k: string, v: string) => { oj.jsonHeaders[k] = v; },
  };
  const bus = {
    publish: (t: string, p: unknown) => { (oj.busLog[t] ??= []).push(p); },
    subscribe: () => {}, unsubscribe: () => {},
  };
  const kvStore = new Map<string, string>();
  const kv = { get: async (k: string) => kvStore.get(k) ?? null, set: async (k: string, v: string) => (kvStore.set(k, v), true), del: async (k: string) => (kvStore.delete(k), true) };
  const log = { debug() {}, info() {}, warn() {}, error() {} };
  const ws = { send: () => {}, close: () => {} };
  const redis = { get: async () => null, set: async () => true };
  const fetchMock = async () => ({ ok: true, json: async () => ({}), text: async () => '' });
  const finish = () => {};
  const __ojRequire = () => { throw new Error('__ojRequire not supported in L2'); };

  Object.assign(globalThis, {
    json, http: oj.http, bus, kv, redis, ws, log,
    db: oj.db.instance, DB: () => oj.db!.instance,
    fetch: fetchMock, finish, __ojRequire,
  });
}

export function setJsonSink(fn: (r: any) => void) { oj._sink = fn; }
export function resetRequest() { Object.assign(oj.http, { params: {}, query: {}, headers: {}, body: null, user: undefined }); oj.jsonHeaders = {}; }
export function busMessages(topic: string) { return oj.busLog[topic] ?? []; }
```

```ts
// sample/test/invoke.ts
import { oj, setJsonSink, resetRequest } from './mock/ojMock';

export interface InvokeReq {
  params?: Record<string, string>; query?: Record<string, string>; body?: unknown;
  headers?: Record<string, string>; user?: unknown; files?: unknown[];
  file?: (i: number) => Promise<unknown>; method?: string;
}
export interface InvokeRes { ok: boolean; data?: unknown; code?: number; msg?: string; headers: Record<string, string>; }

export function invoke(handlerMod: any, method: string, req: InvokeReq = {}): Promise<InvokeRes> {
  resetRequest();
  Object.assign(oj.http, {
    method: (req.method ?? method).toUpperCase(),
    params: req.params ?? {}, query: req.query ?? {}, headers: req.headers ?? {},
    body: req.body ?? null, user: req.user, files: req.files, file: req.file,
  });
  return new Promise<InvokeRes>((resolve, reject) => {
    let done = false;
    const safety = setTimeout(() => {
      if (!done) { done = true; resolve({ ok: false, code: 500, msg: 'invoke timeout: json.ok/fail 未调用', headers: { ...oj.jsonHeaders } }); }
    }, 2000);
    setJsonSink((r) => { if (!done) { done = true; clearTimeout(safety); resolve(r); } });
    try {
      const ret = handlerMod[method]?.();
      Promise.resolve(ret).catch((e) => { if (!done) { done = true; clearTimeout(safety); resolve({ ok: false, code: 500, msg: String(e), headers: { ...oj.jsonHeaders } }); } });
    } catch (e) { clearTimeout(safety); reject(e); }   // 同步抛（如 positiveId 非法）→ 交给 .rejects.toThrow()
  });
}
```

示例断言（注意：L2 白盒里 `json.ok(data)` 的 `data` **就是裸数据**，不要再套信封——这与 L1 黑盒不同）：
```ts
const res = await invoke(mod, 'post', { body: { name: 'alice', role: 'admin' } });
expect(res).toMatchObject({ ok: true, data: { created: true } });

await expect(invoke(mod, 'del', { params: { id: '-1' } })).rejects.toThrow();  // 同步抛
```

---

### 11.3 Q3：vitest 加载 .ts handler（含相对 import / `__ojRequire`）与配置

**加载顺畅，无需特殊 esbuild/swc 配置。** 理由：
- handler `import { requireRole } from "../_shared/validate"` 是普通相对 import，vitest（esbuild）原生解析，且 `validate.ts` 是纯 TS（无全局依赖），零障碍。
- `__ojRequire`：真实 `src/**` 中**没有任何 handler 用到它**，L2 只需提供 `globalThis.__ojRequire = () => throw`（见 11.1），不会触发。
- 裸全局 `json/db/...`：源码未 import，运行时通过 globalThis 解析（Node ESM 语义），只要 `setupFiles` 在用例前把它们挂上即可。

**需要哪些配置：**
```ts
// sample/vitest.config.ts
import { defineConfig } from 'vitest/config';
export default defineConfig({
  test: {
    environment: 'node',                 // L2 纯逻辑，不需要 jsdom
    include: ['test/**/*.test.ts'],
    setupFiles: ['./test/setup.ts'],     // 每个用例前 installGlobals()
  },
});
```
```ts
// sample/test/setup.ts
import { beforeEach } from 'vitest';
import { installGlobals } from './mock/ojMock';
beforeEach(() => installGlobals());      // 关键：每用例重置全局，避免串味
```
**package.json 该加的 devDeps（最小）：** `vitest`（含 esbuild + 自带 runner/expect/报告）。`tsx` **不需要**（vitest 自己转 TS）。建议补 `@types/node`（用 `setTimeout`/Promise 等 Node 类型）。并加 `"scripts": { "test": "vitest run" }`。

**与现有 tsconfig 的衔接（重要坑）：** 现有 `tsconfig.json` 的 `include` 只有 `src/**`，`test/**` 不会被 `tsc`/编辑器纳入，导致测试文件里裸写 `json`/`db` 报「找不到名称」。修正：把 `"test/**/*.ts"` 加进 `include`。另：`global.d.ts` 已 `declare global { const json... }`，纳入后裸标识符即有类型。但 `HttpApi` 缺 `user/files/file`（见 11.6 #3），测试里 `oj.http.user = ...` 会类型报警——L2 内部用 `any` 规避即可，根本修复是扩 `global.d.ts`（非 L2 专属，建议一并提）。

> 结论：L2 不需要自研测试框架、不需要 swc、不需要 tsx；配置量 ≈ 1 个 config + 1 个 setup + 加 1 个 devDep。

---

### 11.4 Q4：L2 测得到 / 测不到（确认并补充）

**用户已提的（确认）：auth / tenant / 413 / WS 是 Rust `handle()` 层，L2 测不到。** 补充细化：

**✅ L2 白盒能测（纯逻辑 + 真实 handler 代码路径）：**
- 输入校验分支：`post` 缺 `name`→400、非法 `role`→400、`del` 非法 id→同步抛、`put` 缺参→400。
- 控制流：`get` 的 `id>0` vs 列表分支；`head`→`get`；`options` 返回方法清单。
- **包络正确性**：`json.ok(data)` 的 data 形状、`json.fail(code,msg)` 的 code/msg（这是 L2 比 L1 更精确的优势——直接断言 envelope 字段）。
- `bus.publish` 的 payload（`news/api`）：通过 `busMessages('news')` 断言，等于测了「业务调用了正确广播」。
- 共享模块 `_shared/validate` 的 `requireRole/positiveId`：可独立单测（边界值）。
- `kv` 缓存逻辑（`order/detail`）：seed 后断言命中/回源。
- `http.user` 消费（`auth_demo/me`）：手动注入 `user` 后断言返回值——**注意只测了「handler 读 user」，没测「中间件如何签发/校验 user」**。

**❌ L2 测不到（必须在文档里写清，避免误判覆盖）：**
- **鉴权/租户中间件**：L2 没有中间件，是手动注入 `http.user/tenant`，所以「未授权请求被拒」「租户间数据隔离」**根本没被测**（只是 handler 内部 if 分支）。
- **413 / body 体积限制**：传输层。
- **WS upgrade + 帧编码**：`WS.ts` 在帧循环里跑（`bus.subscribe` 写在顶层），它不是路由 handler，**L2 不应 import 它**（import 即触发 `bus.subscribe`，且无法驱动帧）。L2 只能测到 `news/api` 的 `bus.publish`，测不到「publish→WS 连接收到帧」那一跳。
- **路由解析 / URL→方法映射 / `export default` dispatch**：L2 直接 `mod.post()` 调用，绕过路由。
- **真实 SQL / schema / 迁移**：L2 是玩具内存表，连 UPDATE SET 都不解析；SQL 正确性只能靠 L1（真实 SQLite）。
- **multipart**（`http.files`/`http.file`）：需手动 stub `files/file`，不真实。
- **跨 handler 集成 / 总线扇出到真实订阅者**：L2 只捕获总线消息，不模拟扇出。

---

### 11.5 Q5：与 L1 分工 —— L2 是否值得做？范围是否过大？

**值得做，但定位是「L1 的快速纯逻辑补充」，不是平替。** 理由：
- L1（in-Rust 黑盒 + 真实 SQLite + 真实中间件）才是**权威集成层**；L2 价值在「不编译 Rust、秒级反馈、精准断言 envelope 与校验分支、TDD handler 逻辑」。
- **范围过大风险**：若把 `src/**` 全量铺 L2，会与 L1 重复覆盖且更容易因「mock 与真实运行时不一致」而漂移（例如 L2 内存表 ≠ 真实 SQLite）。

**建议 MVP 只做 1–2 个示例 + 骨架：**
1. 骨架：`mock/ojMock.ts` + `mock/db.ts` + `invoke.ts` + `vitest.config.ts` + `setup.ts`（一次到位，后续加用例零成本）。
2. 示例 1：`src/user/account/api.ts`（覆盖 `db.query/exec` + 校验 + envelope + 同步/异步/抛异常三种路径）。
3. 示例 2：`src/news/api.ts`（覆盖 `bus.publish` 捕获）。
4. 可选示例 3：直接单测 `src/user/_shared/validate.ts`（纯函数，零全局，最干净）。

其余 handler（order/*、upload、auth_demo、profile）**先不做**，骨架就位后按需在 `test/` 下加 `.test.ts` 即可。

---

### 11.6 L2 / TS 层「必须修正」清单

| # | 项 | 性质 | 修正 |
|---|---|---|---|
| 1 | **复用 vitest，不自研框架** | 反对照搬 L1 | L2 在 Node+vitest，`describe/it/expect/beforeEach`/报告/退出码**全用 vitest 自带**；只 mock oj 全局。删掉 L1 那套 ~200 行微型框架在 L2 的复刻 |
| 2 | **全局注入时机** | 合理(补强) | `setupFiles` 里 `beforeEach(() => installGlobals())`，每用例重置 `db/http/json/bus`，避免串味 |
| 3 | **`global.d.ts` `HttpApi` 缺 `user/files/file`** | 风险(契约缺口) | 扩 `HttpApi` 加可选 `user?`、`files?`、`file?()`（顺带修真实 handler 编辑器报错）；L2 mock 内用 `any` 暂避 |
| 4 | **`db` stub 范围** | 反对过度 | MVP 不实现 `table()` 链式构造器（示例全用 `query/exec`）；用 `Map<table,Row[]>` + 表名/单等值 where 正则解析即可；保留 `onQuery/onExec` 强制返回 |
| 5 | **invoke 异步/同步/抛异常三态** | 合理(补强) | 用 json-sink deferred + 2s 安全超时；同步 `json.fail` 即时解封；同步抛→`reject` 交给 `.rejects.toThrow()`；handler 返回 thenable→`Promise.resolve().catch` 兜底 500 |
| 6 | **不要 import `WS.ts`** | 反对 | `WS.ts` 顶层调用 `bus.subscribe`，是帧循环非路由 handler，L2 只测 `news/api` 的 `bus.publish` |
| 7 | **vitest 配置/依赖最小集** | 合理 | `vitest.config.ts`(environment:node, include:test/**, setupFiles) + devDep `vitest`(+`@types/node`)；**不需 swc / tsx**；`tsconfig` 的 `include` 加 `test/**` |
| 8 | **包络语义差异（L1 vs L2）** | 风险 | L1 黑盒 `res.json()`=信封、需 `.data`；L2 白盒 `invoke` 返回的 `res.data` **已是裸数据**，断言不要多套一层信封 |
| 9 | **范围控制** | 反对铺全量 | MVP = 骨架 + `account/api` + `news/api`（+ 可选 `validate` 直测）；其余延后 |
| 10 | **文档声明 L2 边界** | 风险 | 明确列出 11.4 的「测不到」清单，避免给人「已端到端测了鉴权/WS」的错觉 |

### 11.7 目录落点建议（MVP）
```
sample/
  vitest.config.ts
  test/
    setup.ts
    invoke.ts
    mock/ojMock.ts
    mock/db.ts
    user/account/api.test.ts     # 示例1
    news/api.test.ts             # 示例2
    user/_shared/validate.test.ts # 可选示例3
```
（不改动 `src/`、`global.d.ts` 之外的任何源文件；`global.d.ts` 的 `HttpApi` 扩展属顺带建议，可单列。）
