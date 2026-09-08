---
title: 09 · 测试
updated: 2026-09-08
---

# 09 · 测试

oj 的测试分四层，从快到真：

| 层 | 是什么 | 跑法 | 用来测 |
|---|---|---|---|
| **L0** | Rust 单测 | `cargo test --release` | 核心库与插件的 Rust 逻辑 |
| **L1** | `oj test`（进程内真实运行时） | `oj test -c config.yaml` | handler 真跑：全局对象、数据库、信封都真实 |
| **L2** | vitest（纯 mock） | `cd sample/unit && npm test` | 纯函数/校验逻辑，不依赖运行时 |
| **L3** | e2e（起真服务） | `cargo test --release -p oj --test e2e` | 端到端：路由、鉴权、迁移、release 模式 |

新人只需要记住一句话：**测业务逻辑用 L1，测纯函数用 L2，其余交给 CI。**

## L1：`oj test`

测试文件放在 **config 所在目录下的 `tests/`** 里，命名 `*.test.ts`。运行时会注入
`client` 与 `describe/it/expect` 三个全局（类型见 `sample/global.d.ts`）：

```ts
// sample/tests/user.test.ts
describe("user account", () => {
  it("lists accounts (auth + tenant)", async () => {
    const token = await client.login("demo", "demo1234", { "X-TENANT-ID": "default" });
    const r = await client.get("/user/account", {
      headers: { Authorization: "Bearer " + token, "X-TENANT-ID": "default" },
    });
    expect(r.status).toBe(200);
    const body = JSON.parse(r.body);        // 信封：{ code, msg, data }
    expect(body.code).toBe(0);
  });
});
```

跑：

```bash
./bin/oj test -c sample/config.yaml -d sample/src                    # human 摘要
./bin/oj test -c sample/config.yaml -d sample/src \
  --format junit --output l1-result.xml                              # CI 报告
```

| 旗标 | 说明 |
|---|---|
| `-c/--config` | 配置文件 |
| `-d/--dir` | 源码目录 `src` 或产物 `dist`（默认自动判定） |
| `-t/--tests` | 测试目录，相对 config 目录（默认 `tests`） |
| `--format` | `human` / `tap` / `junit` / `json` |

要点：

- **进程内真实运行时**：不是 mock，全局对象、数据库、迁移都是真的（用 config 里 `db.test`
  指向的库，避免污染开发库）。
- 退出码「全通过 0，任一失败 1」，可以直接做 CI 门禁。
- `client.get` 的 path 是**相对 base** 的（`/user/account`，不用写 `/v1/api`）。
- fixtures 只在这一层灌入（`fixtures/` 目录 + `oj fixture`）。

## L2：vitest（纯 mock）

`sample/unit/` 是独立 npm 包：

```bash
cd sample/unit && npm install && npm test    # vitest run
```

这一层**不启动 oj**，所以全局对象要自己 mock。适合：参数校验、纯计算、格式化函数。
不适合：任何依赖 `db`/`kv`/`http` 的代码。

## L0：Rust 侧

```bash
cargo test --release                      # 核心库单测
cargo test --release --workspace          # 含插件与 e2e
cargo test --release -p oj --test e2e     # L3
```

两条纪律：

- **一律 `--release`**（debug 构建不在支持范围内，且磁盘开销巨大）。
- 异步测试用 `tokio::test(flavor = "current_thread")` —— `JsRuntime` 是 `!Send` 的。

## 测试数据怎么准备

| 数据 | 放哪 | 谁加载 |
|---|---|---|
| 参考数据（字典、初始账号） | `seed.sql` | 每次启动重放，**必须幂等** |
| 测试专用数据 | `fixtures/` | `oj test` / `oj fixture` |

别把测试数据写进 `seed.sql` —— 它会跑到生产环境里去。

## 常见失败

| 现象 | 原因 |
|---|---|
| `oj test` 报库里没表 | 测试库没迁移：确认 config 的 `db.test` 存在且跑过迁移 |
| 测试之间互相干扰 | fixtures 没清干净，或用了共享状态；L1 每用例应自洽 |
| L1 通过但 e2e 失败 | 多半是路由/鉴权/证书这类「只有真服务才走到的路径」出问题 |
| vitest 报全局对象不存在 | 正常 —— L2 是纯 mock 层，要 mock 或上移 L1 |

## 延伸

- [测试手册](../reference/testing.md)
- [08 · 测试体系](../modules/08-testing.md)（分层方案与目录约定）
