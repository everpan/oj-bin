# 测试开发手册（Testing Guide）

本项目对 sample 应用提供**两层测试**，互为补充：

- **L1 — `oj test`**：进程内、真实 deno_core 运行时 + 真实 axum `Router::oneshot` 派发（零 TCP）。
  跑的是**真实 v8 handler**，走完整的路由 + 鉴权 + 多租户管线，连**真实后端**（内存或配置的
  DB/KV/bus）。对标 Go Fiber 的 `app.Test`。
- **L2 — vitest 纯 mock**：不启动 server、不跑 v8、不连真实后端。直接调用真实 `api.ts` 的
  handler 函数，用 mock 全局（`db`/`json`/`http`/`bus`/`log`）替代运行时注入。

一句话：**L2 测 handler 纯逻辑（快、稳），L1 测端到端行为（真、全）**。

---

## 目录约定

```
sample/
├── config.yaml            # 应用配置（auth/tenant/db…）
├── src/                   # 被测源码（api.ts handler）
├── tests/                 # ★ L1 测试文件：*.test.ts
└── test/                  # ★ L2 vitest 工程（独立 npm 包）
    ├── package.json       # devDependencies: vitest（已锁定 lockfile）
    ├── mocks/oj-globals.ts
    ├── invoke.ts
    └── *.test.ts
```

L1 测试目录由 `oj test -t/--tests <dir>` 指定，相对**配置文件所在目录**；默认 `tests`。

---

## L1：`oj test`（进程内真实运行时）

### 运行

```bash
# 默认 human 摘要
cargo run -p oj -- test -c sample/config.yaml -d sample/src

# CI 报告（落盘 JUnit，给 GitHub Actions / GitLab 直接消费）
cargo run -p oj -- test -c sample/config.yaml -d sample/src \
  --format junit --output sample/test/l1-result.xml
```

| 旗标 | 说明 |
|---|---|
| `-c/--config` | 配置文件（默认 `config.yaml`） |
| `-b/--base` | API 基础前缀覆盖（默认用 config 的 `server.base`，如 `/v1/api`） |
| `-d/--dir` | 源码目录 `src` 或产物 `dist`（默认自动判定） |
| `-t/--tests` | 测试目录，相对 config 目录（默认 `tests`） |
| `--format` | `human`（默认）/ `tap` / `junit` / `json` |
| `--output` | 报告落盘文件；省略则打到 stdout（机器格式 stdout 纯净，路由横幅已改打 stderr） |

退出码：**全部通过 = 0，任一失败 = 1**（可直接做 CI 门禁）。

### 测试文件写法

`tests/*.test.ts` 用注入的全局 `client` 与 `describe/it/expect` 编写（类型见 `sample/global.d.ts`）：

```ts
describe("user account", () => {
  it("lists accounts (auth + tenant)", async () => {
    const token = await client.login("demo", "demo1234", { "X-TENANT-ID": "default" });
    const r = await client.get("/user/account", {
      headers: { Authorization: "Bearer " + token, "X-TENANT-ID": "default" },
    });
    expect(r.status).toBe(200);
    const body = JSON.parse(r.body);   // 信封：{ code, msg, data }
    expect(body.code).toBe(0);
    expect(Array.isArray(body.data)).toBeTruthy();
  });
});
```

**注入的全局 API**

- `client.<get|post|put|del|patch|head|options>(path, opts?)` —— `opts = { headers?, body? }`，
  返回 `ClientResp { status, headers, body, upgrade }`。`path` 为**相对 base** 的路径，如 `/user/account`。
- `client.login(username, password, headers?)` —— POST `/auth/login`（headers 透传给请求，
  如 `{ "X-TENANT-ID": "default" }`），返回 `access_token`。
- `describe(name, fn)` / `it(name, fn)` / `beforeEach(fn)` / `expect(actual).toBe|toEqual|toBeTruthy|toBeFalsy|toContain`。

**两个必知约束（由 config 决定）**

1. **多租户**：config 启用 `tenant` 后，每个请求（含 login/refresh/logout）都必须带
   `X-TENANT-ID` 头，否则 400——`client.login` 的第三个参数就是干这个的。
2. **鉴权**：config 启用 `auth` 后，除匿名路径（`/health` 与 `anonymous_paths` 配的
   `/auth/*`）外都要带 `Authorization: Bearer <token>`，否则 401。`client.login` 仅用于拿 token。

> `beforeEach` 注册的是单一全局钩子，跨多个 `describe` 会被覆盖；多 describe 文件建议在各 `it`
> 内联准备（如每个用例自己 `client.login`），避免互相干扰。

### 适用场景

- API 端到端行为：路由匹配、JWT 鉴权、租户隔离、真实 DB 交互、bus 广播、统一响应信封。
- 契约/集成回归测试：保障「请求进来 → 真实后端 → 正确响应」这条全链路不被破坏。
- 代价：每次启动会初始化运行时与后端，比 L2 慢；适合 CI 与关键链路守护。

---

## L2：vitest 纯 mock（sample/test/）

### 运行

```bash
cd sample/test
npm ci          # 或 npm i；lockfile 已提交，CI 用 npm ci 保证可复现
npx vitest run  # 等价于 npm test
```

### 结构

- `mocks/oj-globals.ts`：`installGlobals(opts?)` 把运行时注入的 `db/json/http/bus/log` 替换为
  可控桩，返回本次响应捕获 `{ code, msg, data }`；`lastPublished()` 取 `bus.publish` 记录。
- `invoke.ts`：`invoke(handler, method, opts?)` 装好 mock 全局 → 调用 `handler[method]()` →
  flush 微任务 → 返回 `{ ...capture, published }`。
- `*.test.ts`：直接 import 真实 `../src/.../api` 的 handler，调用 `invoke` 并断言。

### 测试文件写法

```ts
import { describe, it, expect } from "vitest";
import account from "../src/user/account/api";
import { invoke } from "./invoke";

describe("user/account (L2 mock)", () => {
  it("get lists accounts from dbRows", async () => {
    const r = await invoke(account, "get", { dbRows: [{ id: 1, name: "neo", role: "admin" }] });
    expect(r.code).toBe(0);
    expect(Array.isArray(r.data)).toBe(true);
    expect(r.data[0].name).toBe("neo");
  });

  it("post rejects invalid role → 400", async () => {
    const r = await invoke(account, "post", { body: { name: "x", role: "king" } });
    expect(r.code).toBe(400);
  });
});
```

> 业务 handler 内部用 `db.query(...).then(json.ok)` 走微任务，故 `invoke` 已 `setTimeout(0)` flush，
> 断言前响应已落定。

### 适用场景

- handler 纯逻辑单测：入参校验、响应塑形、bus 事件触发、纯函数（如 `requireRole`/`positiveId`）。
- TDD 与快速回归：毫秒级、无基础设施、不依赖网络/DB，不会因环境问题 flaky。
- 局限：**mock 不是真实后端**，无法发现鉴权/租户/路由装配等集成层问题。

---

## 如何选型

| 维度 | L1 `oj test` | L2 vitest |
|---|---|---|
| 运行时 | 真实 deno_core(v8) + axum | Node + vitest（无 v8） |
| 后端 | 真实 DB/KV/bus | mock 桩 |
| 速度 | 较慢（启动开销） | 极快 |
| 覆盖 | 路由/鉴权/租户/DB/总线 | handler 逻辑 |
| 稳定性 | 受后端/配置影响 | 稳定、无副作用 |
| 入口 | `oj test`（Rust CLI） | `npx vitest run`（Node） |

**推荐组合**：开发期用 L2 快速验证逻辑；CI 用 L1 守护端到端契约。两者都绿，才有信心发布。

### CI 示例（GitHub Actions 片段）

```yaml
steps:
  - name: L2 vitest
    working-directory: sample/test
    run: npm ci && npx vitest run
  - name: L1 oj test (junit)
    run: cargo run -p oj -- test -c sample/config.yaml -d sample/src --format junit --output l1.xml
  - name: Upload L1 report
    if: always()
    uses: actions/upload-artifact@v4
    with:
      name: l1-junit
      path: l1.xml
```

---

## 依赖管理

- L2 的 `vitest` 声明在 **`sample/test/package.json` 的 `devDependencies`**，与 `sample/package.json`
  的运行时依赖（`escape-goat`）**隔离**——被测物不携带测试工具。
- `sample/test/package-lock.json` 已提交，`npm ci` 可复现。
- `sample/test/.gitignore` 忽略 `node_modules/`，勿提交本地安装目录。
- 若想单一 `npm install`，可把 `sample/package.json` 升级为 npm workspaces（`"workspaces": ["test"]`），
  但「被测物 / 测试工具」分离的当前方案更干净，推荐保持。
