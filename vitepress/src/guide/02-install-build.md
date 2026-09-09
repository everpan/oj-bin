---
title: 02 · 构建与运行
updated: 2026-09-08
---

# 02 · 构建与运行

目标：把 sample 跑起来，并能解释每一步为什么要这么做。

## 前置

- **Rust 工具链**（edition 2024，用较新的 stable）。
- **磁盘**：首次构建会拉预编译的 V8 静态库（rusty_v8），请留出足够空间。仓库明确
  **禁止 debug 构建**（dev 产物曾占 120G+），所以命令一律带 `--release`。
  若网络受限导致 V8 想从源码编译，设 `V8_FROM_SOURCE=0` 强制用预编译包 —— 千万别真去编 V8。

## 构建

```bash
cargo build --release              # 核心库（等价 cargo build，配置已把 build 别名到 release）
cargo build --workspace --release  # 全部成员：核心 + 插件 + server + oj
cargo xtask build                  # 构建 oj + 全部第一方插件，统一归置到 bin/
```

产物布局（与运行期插件发现路径同形）：

```
bin/oj                        # 主程序
bin/plugins/<host-triple>/     # 插件 cdylib，如 bin/plugins/aarch64-apple-darwin/
```

`cargo run -p oj -- <cmd>` 也能跑，但 xtask 会把产物放到 oj 启动时默认去找的位置，
**第一次跑推荐用 xtask**：`cargo xtask build` 之后再 `./bin/oj ...`。

## 启动 sample

```bash
cargo run -p oj -- server -c sample/config.yaml --api-path sample/src
curl http://localhost:9778/v1/api/user/account/?id=1
```

第二条命令大概率会返回 **401** —— 这不是你配错了，而是 sample 的业务端点默认全受保护
（详见[07 · 鉴权与多租户](./07-auth-tenant.md)）。想先确认服务活着，用内置匿名端点：

```bash
curl http://localhost:9778/v1/api/health      # 证书状态，匿名
```

## 三个必踩的坑（先讲清楚，省两小时）

### 1. 证书必配，没有逃生口

`server.public_key_path` 与 `server.certificate_path` 缺任意一个，进程直接拒绝启动，
且**没有配置或 CLI 开关能绕过**。sample 自带自签示例证书 `sample/config/{public.pem,cert.jws}`
（配套私钥 `private.pem` 仅供演示，**严禁用于生产**）。过期后重签：

```bash
cargo run -p oj-cert -- renew -k sample/config/private.pem -o sample/config
```

要搞清楚证书机制为什么这么硬，读[运维手册](../reference/ops-manual.md)。

### 2. 插件是被「发现」的，不是被「声明」的

四级查找顺序，**先命中者胜**：

1. `OJ_PLUGINS_DIR` 环境变量
2. config 的 `plugins_dir`
3. `<exe>/plugins`
4. `<workspace_root>/bin/plugins`

再各自拼上 `<host-triple>/`。sample 在仓库里跑，走第 4 级自动发现，所以
**不用设 `OJ_PLUGINS_DIR`**。生产部署时把 `bin/` 整个拷走即可，布局不变。

插件清单（`plugins:` 配置段）的三种写法见[用户手册](../reference/user-manual/index.md)，
一句话：键 = 严格清单，值 = 透传给插件的配置，空对象 = 回落到扫描模式。

### 3. dev 还是 release，看目录不看参数

- 有 `dist/manifests.yaml` → **release**：服务预构建 JS，不转译，按锁聚合
- 否则 → **dev**：服务 `src`，按需转译 TS，`notify` 驱动热重载

所以 `server -c config.yaml --api-path dist` 才是 release 模式；`--api-path src` 是 dev。
release 下还有一道门禁：`migrate_on_start: verify`（release 默认）时账本落后会**拒绝启动**，
先跑 `oj migrate`。

## 日志在哪

默认终端**不打日志**（`console_log: false`），只落盘到 `server.logs_dir`（默认 `./logs`，
按天滚动 + 大小滚动）。想看终端输出：

```bash
cargo run -p oj -- server -c sample/config.yaml --api-path sample/src --console-log
RUST_LOG=oj=debug ./bin/oj server -c sample/config.yaml --api-path sample/src
```

日志级别由 `RUST_LOG` 控制，配置文件里配不了。

## 常用命令速查

```bash
./bin/oj server -c sample/config.yaml --api-path sample/src     # dev
./bin/oj build  -d sample/src -o sample/dist                    # 构建 release 产物
./bin/oj migrate -c sample/config.yaml -d sample/dist           # release 部署前先迁移
./bin/oj schema diff -c sample/config.yaml                      # 声明 vs 实库对账（漂移 exit 1）
./bin/oj test -c sample/config.yaml --format human              # 进程内跑 *.test.ts
```

## 下一步

- [03 · 第一个接口](./03-first-api.md)
- 细节权威：[用户手册](../reference/user-manual/index.md)、[运维手册](../reference/ops-manual.md)
