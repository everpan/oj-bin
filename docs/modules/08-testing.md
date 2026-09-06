# 08 · 测试分类与 `sample/test` vs `sample/tests` 处置方案

> 目标：给「一个测试该放哪、怎么跑、CI 怎么守」一套可执行的判据，并回答
> 「`sample/test/` 与 `sample/tests/` 要不要合并」。
>
> **状态（2026-09-06 已整改）**：采纳方案 A —— L2 目录改名 `sample/unit/`、用例后缀改
> `.spec.ts`、`sample/package.json` 加统一入口、删除/改写与 L1 重复的正例；CI 补上
> `sample-tests` job。下文保留原始论证（为什么这么改），并标注落地后的现状。

## 1. 现状：四套运行器，两套在 sample 里

| 层 | 运行器 | 位置 | 跑什么 | 代价 |
|---|---|---|---|---|
| **L0** Rust 单测/集成 | `cargo test` / `cargo test --workspace` | `src/**`、`oj/src/**`、`plugins/**`、`tools/oj-cert/tests/`、`oj/tests/{e2e,oidc_e2e}.rs` | Rust 代码本身：op、配置、插件加载、迁移、构建产物、真服务 e2e | 中~慢 |
| **L1** 进程内集成 | `oj test`（`oj/src/test_cmd.rs`） | `sample/tests/*.test.ts` | 真 v8 + 真路由 + 真后端，`Router::oneshot` **零 TCP** | 较慢（启动一次全装配） |
| **L2** 纯 mock 单测 | `vitest`（`sample/unit/` 独立 npm 包） | `sample/unit/*.spec.ts` | 不跑 v8、不连后端，直接 import 真实 handler 函数 | 毫秒级 |
| **L3** 真服务端到端 | `cargo test -p oj --test e2e` | `oj/tests/*.rs` | 真 TCP + 真端口：WS 帧循环、超时后服务存活、热重载、release 装配 | 慢 |

`oj test` 如何找文件：默认目录 `tests`（相对 **config 文件所在目录**），
`-t/--tests` 可覆盖；**只收 `*.test.ts`**，按名排序。

## 2. 两个目录的真实差异（不只是名字差一个 s）

| 维度 | `sample/tests/`（L1） | `sample/unit/`（L2，原 `sample/test/`） |
|---|---|---|
| 运行器 | Rust `oj test`（deno_core 运行时内） | Node + vitest |
| 全局来源 | 真实注入（`client` + bridge 全部全局） | `mocks/oj-globals.ts` 手写桩 |
| 被测入口 | HTTP 路径 → 路由表 → 真实 handler | 直接 `import` handler 对象，调 `invoke(handler, method, opts)` |
| 能测到 | 路由/鉴权/租户/真实 DB/总线/信封/状态码 | handler 内部分支、数据塑形、纯函数、bus 事件内容 |
| 断言能力 | 自研迷你框架：**仅 5 个匹配器** `toBe/toEqual/toBeTruthy/toBeFalsy/toContain`；`beforeEach` 是**单一全局钩子**（跨 describe 互相覆盖）；无 `afterEach`/`skip`/`only` | vitest 全套匹配器、`vi.mock`、快照、覆盖率、`describe` 嵌套隔离 |
| 依赖 | 无（Rust 侧） | 独立 `package.json` + lockfile，`node_modules/` 已 gitignore |
| 报告 | `human`/`tap`/`junit`/`json`，退出码可当门禁 | vitest 报告 |

## 3. 要不要合并？——**不建议合并目录，但必须改名 + 统一入口**

### 不合并的三条硬理由

1. **运行器互斥**。`oj test` 只认 `*.test.ts` 且会 `import` 它 —— 若把 L2 文件放进同一目录，
   `import { describe } from "vitest"` 在 deno_core 里会解析失败并让**整个 L1 批次红**；
   反之 vitest 会尝试执行 L1 文件，`client` 未定义即崩。
2. **依赖隔离是刻意的**。`sample/unit/package.json` 与 `sample/package.json` 分离，
   保证「被测物不携带测试工具」。合并会把 `node_modules` 提到 sample 根。
3. **合并解决不了真问题**。真问题不是「两个目录」而是「**名字只差一个 s**」+「**用例重复**」。

### 已落地（方案 A）

| 动作 | 内容 | 状态 |
|---|---|---|
| ① 改名 | `sample/test/` → **`sample/unit/`** | ✅ 已改（`git mv`，历史保留） |
| ② 统一入口 | `sample/package.json` 加 `test` / `test:unit` / `test:api` / `unit:install` | ✅ 已加 |
| ③ 目录内分层 | `sample/unit/mocks/`（桩）+ `*.spec.ts`（用例）；L1 保持 `*.test.ts` | ✅ 已改 |
| ④ 文档同步 | `docs/testing.md` + 本文件 | ✅ 已同步 |

若坚持单一目录，可行但需两条约束：`oj test -t tests` 只扫 `*.test.ts`、
vitest `include: ["tests/**/*.spec.ts"]` —— 见下「方案 B」。

### 备选方案对比

| 方案 | 做法 | 代价 | 适用 |
|---|---|---|---|
| **A（已采纳）** | 保留两目录，L2 改名 `unit/`，统一 npm script 入口 | 一次 rename + 文档 | ✅ 2026-09-06 落地 |
| **B** | 合并到 `sample/tests/`，用后缀区分（`.test.ts` = L1，`.spec.ts` = L2） | vitest 配置 + `node_modules` 上提；两个运行器仍并存 | 想要「只一个 tests 目录」 |
| **C** | 合并运行器：给 `oj test` 加 mock 注入 ext，消灭 vitest | 要自研匹配器/mock/覆盖率，工作量大 | 长期最干净，但优先级低 |

## 4. 分类测试的方法（一条测试该放哪）

按**被测对象**而非「是不是单测」来分。决策顺序自上而下，命中即停：

```
① 测的是 Rust 代码本身（op / 配置 / 插件加载 / 迁移 / 构建 / FFI）？
      → L0（cargo test）
② 需要真 TCP、WS 帧循环、超时熔断后服务存活、热重载、release 装配？
      → L3（oj/tests/*.rs）
③ 断言依赖路由 / 鉴权 / 租户头 / 真实 DB / 统一信封 / HTTP 状态码？
      → L1（sample/tests/*.test.ts，oj test）
④ 只是 handler 内部逻辑（入参校验分支、数据塑形、纯函数、bus 事件内容）、
   或需要强断言/参数化/覆盖率？
      → L2（sample/unit/*.spec.ts，vitest）
```

### 判定的反模式（命中即放错层）

| 反模式 | 说明 |
|---|---|
| L2 用例里出现 `client.` | L2 没有 `client` 全局，说明这是 L1 的活 |
| L2 里断言 HTTP 状态码 / 鉴权 401 / 租户 400 | 这些是管线的产物，mock 层根本不执行管线 |
| L1 里断言**非 HTTP 可观察**的东西（内部中间变量、私有函数返回值） | 应下沉到 L2 |
| 同一条业务规则在 L1 与 L2 各写一遍正例 | 见下「去重复」 |

### 去重复规则

> **一条业务规则 = 一个主断言点。**
> - **契约类**（「接口长这样」：字段、分页结构、状态码、camelCase 转换）→ 只放 **L1**。
> - **分支类**（「输入 X 得 Y」：校验边界、默认值、分支覆盖、纯函数）→ 只放 **L2**。

**重复情况与处置（2026-09-06 已处置）**：`sample/test/` 的 3 个文件几乎被 `sample/tests/`
覆盖了一遍 ——

| 原 L2 文件 | L1 重复点 | 处置 |
|---|---|---|
| `user.account.test.ts`（list / create / invalid role→400 / OPTIONS） | `user.test.ts` 四条**完全同场景** | **改写**为分支用例：id 分支的 SQL/参数、缺 name→400 且不落 SQL、role 缺省回落 `user`、put/patch/del 校验分支 |
| `news.test.ts`（publish 内容 / 默认文案） | `news.test.ts`（401 / 200 published） | **保留并收窄**到 L1 看不到的部分：广播帧内容、缺省 text 回落、body=null 不抛错 |
| `admin.test.ts`（role-list 分页 + `lineLength`） | `admin.test.ts`（role-list 分页 + `/home/line` 同口径断言） | **拆分**：role-list 分页正例删除（L1 已有）；`lineLength` 独立成 `line-length.spec.ts`，用 fake timers 钉死闰年/月末/年初等固定日期期望值 |

结果：L2 由 7 个「与 L1 重复的正例」变为 **12 个 L1 覆盖不到的分支/边界用例**
（本地 `npx vitest run` 12/12 通过）。

> 改写后 L2 新增能力：`mocks/oj-globals.ts` 增加 `lastSqlCalls()`，可断言 handler 发出了
> 什么 SQL 与绑定参数——这是 L1（只能看响应）看不到的维度。

## 5. 运行方式（抄这份）

```bash
# L0
cargo test                       # 根 crate
cargo test --workspace           # 全部（含插件、oj e2e）

# L1 + L2（统一入口，推荐）
cd sample && npm run test          # = test:unit + test:api

# L1
cd sample && npm run test:api      # xtask build + ./bin/oj test
# 或手工：
./bin/oj test -c sample/config.yaml -d sample/src
./bin/oj test -c sample/config.yaml -d sample/src --format junit --output l1.xml

# L2
cd sample && npm run test:unit     # = npm --prefix unit run test
# 或手工：
cd sample/unit && npm ci && npx vitest run

# L3
cargo test -p oj --test e2e
cargo test -p oj --test oidc_e2e
```

## 6. CI：缺口已补（2026-09-06）

此前 `.github/workflows/plugin-matrix.yml` 只跑：

```yaml
cargo test --workspace --release
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
```

**L1 与 L2 都不在 CI 里** —— sample 的端到端契约与 mock 单测无人守护。现已补
`sample-tests` job（单平台 ubuntu-latest，因两层测试都不覆盖平台相关行为）：

```yaml
  sample-tests:
    needs: [lint]
    name: sample L1 + L2
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: actions/setup-node@v4
        with: { node-version: 20 }
      # 必须经 workspace 构建：`cargo build -p oj` 与 `--workspace` 的 feature 归一化
      # 不同，会让 rusty_v8 按不同 fingerprint 重编并找不到静态库。xtask build 同时
      # 产出 oj 与全部第一方插件（sample 鉴权依赖 oj-auth）。
      - name: build oj + plugins
        run: cargo run --release -p xtask -- build
      # 插件发现：<exe>/plugins/<host-triple>/ —— bin/oj + bin/plugins/ 正好命中。
      - name: L1 — oj test (junit)
        run: ./bin/oj test -c sample/config.yaml -d sample/src --format junit --output l1.xml
      - name: L2 — vitest (pure mock unit)
        working-directory: sample/unit
        run: npm ci && npx vitest run
      - if: always()
        uses: actions/upload-artifact@v4
        with: { name: l1-junit, path: l1.xml, if-no-files-found: warn }
```

> 注：`oj test` 走 `App::from_config`，会经过**证书必配门禁**与迁移门禁。CI 上用的是
> 随仓库提交的示例自签证书（exp ≈ 2027-08），**到期后本 job 会红**——届时用
> `tools/oj-cert` 重签并提交新证书（见 `plugin-matrix.yml` 尾部注释）。

## 7. 编写约束（L1 特有）

- 多租户：每个请求（含 login/refresh/logout）都要带 `X-TENANT-ID`，否则 400；
  `client.login(u, p, { "X-TENANT-ID": "default" })` 第三个参数就是干这个的。
- 鉴权：除匿名路径外都要 `Authorization: Bearer <token>`。
- `beforeEach` 是**单一全局钩子**，跨 describe 会互相覆盖 —— 多 describe 文件请在各 `it`
  内联准备（每个用例自己 `client.login`），`sample/tests/cert.test.ts:8` 的做法
  （模块级 `ADMIN`/`USER` 常量 + `beforeEach` 刷新 token）是该文件只有一个 describe 时的特例。
- 断言只有 5 个匹配器，写复杂断言时先想想是不是该放 L2。
- WS：oneshot 只返回 101，不跑帧循环 —— WS 行为归 L3（`oj/tests/e2e.rs`）。
- `oj test` 会灌 `fixtures/`（`App::from_config(fixtures=true)`），`oj server` 不会 ——
  依赖 fixture 的用例只在 L1 可复现。

## 8. 相关文档

- `docs/testing.md` —— L1/L2 使用手册（本文件是其分类与治理的补充，两者需同步命名）
- `docs/dev-guide.md#12` —— 测试章节
- `docs/devkit/api-manual.md#9` —— 面向业务开发者的测试写法
