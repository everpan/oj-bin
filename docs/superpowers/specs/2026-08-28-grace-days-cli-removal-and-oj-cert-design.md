# 设计：移除 `--grace-days` CLI 参数 + `tools/oj-cert` 证书生成工具

日期：2026-08-28　状态：已批准（用户否决 bin/ 归置）

## 背景

- 证书过期宽限天数 `grace_days` 目前有两条入口：CLI `oj server --grace-days`（覆盖 config）
  与 `config.yaml` 的 `server.grace_days`（默认 30）。决策：**只保留 config**，删除 CLI 覆盖
  （宽限期属运维策略，应随配置走，不应随启动命令漂移）。
- 本仓库「证书」为 **JWS/RS256 三段式 token**（`server/src/certificate.rs`）：header
  `{"alg":"RS256","typ":"JWT"}`，payload 至少含 `nbf`/`exp`（Unix 秒），签名由 RSA 私钥对
  `header.payload` 生成；`server.public_key_path`（PEM，SPKI/PKCS#1 均可）验签。当前无任何
  生成工具 —— 测试用硬编码密钥对（`tests/certificate_load_test.rs`），运维重签靠手工。
  `ops-manual.md:146` 已把「重签使 exp 晚于现在」写为标准恢复流程；server 有证书热重载
  watcher（`certificate_watcher.rs`），公钥不变、只换 JWS 即可免重启续期。

## Part 1：移除 `--grace-days`（纯删除）

| 文件 | 改动 |
|------|------|
| `oj/src/args.rs` | 删 `ServerArgs.grace_days` 字段、`Commands::Server` 的 `grace_days` clap 参数、`to_command` 对应分支 |
| `oj/src/server_cmd.rs:31-32` | 删 `if let Some(d) = a.grace_days { … }` 覆盖块 |
| `oj/src/main.rs:105` | 测试字面量删该字段 |
| `oj/src/args.rs` 测试 `bad_usage_is_clap_error` | 加 `assert!(cli(&["server", "--grace-days", "30"]).is_err())`（沿 `--dev` 已删先例，防止回潮） |
| `docs/user-manual.md:30,44` | 删 usage 行中的 `--grace-days` 与旗标表对应行 |

**不动**：`src/config.rs` 的 `server.grace_days`（Option<u64>，默认 Some(30)）、
`server/src/certificate.rs` 的宽限判定、其余文档（本就只写 config 字段）。

## Part 2：`tools/oj-cert` 新 crate

独立 bin crate（`tools/oj-cert`，edition 2024），**不依赖 only-js**（无 V8，秒级构建）。
加入根 `Cargo.toml` workspace members。**不进 `bin/`、不随 oj 发行**（xtask `bin`/`build`
不动）；需要时 `cargo build -p oj-cert --release` 单独构建。

### CLI

```
oj-cert gen    -o <dir> [--days 365] [--bits 2048] [--nbf <unix>] [--exp <unix>]
oj-cert renew  -k <private.pem> [-o <dir>] [--days 365] [--exp <unix>]
```

- **gen**：`rsa` crate 生成 RSA 密钥对（默认 2048 = ring `RSA_PKCS1_2048_8192_SHA256` 下限）。
  输出三文件到 `-o` 目录：
  - `private.pem`（PKCS#8；unix 下 chmod 600）
  - `public.pem`（SPKI，server loader 已支持）
  - `cert.jws`（header 固定 `{"alg":"RS256","typ":"JWT"}`；payload 仅 `nbf`/`exp`，默认
    nbf=now、exp=now+days）
- **renew**：读现有私钥（PKCS#8 PEM）重签，只写新 `cert.jws` 到 `-o`（默认私钥所在目录）。
  公钥不变 → config 不改，配合热重载免重启续期。默认值与 gen 相同（nbf=now、exp=now+days；
  renew 无 `--nbf` —— 避免误缩已生效窗口）。
- `--nbf`/`--exp` 同时给出时 `--exp`/`--nbf` 显式值优先于 `--days` 推算。
- `--nbf`/`--exp` 显式覆盖默认：测试与运维伪造 valid/grace/expired 状态用（e2e 已在测这些状态）。
- 参数校验：`exp > nbf`（与 loader 一致）；`bits` 下限 2048。

### 依赖与实现

`rsa` 0.9（纯 Rust keygen；`ring` 只能签名不能生成密钥）、`sha2`、`base64`、`serde_json`、
`clap`（derive，与 oj 同款）。错误处理沿 xtask 风格：`Result<(), String>`、fail-fast、非零退出。

### 测试

crate 内集成测试（`cargo test --workspace` 覆盖）：
1. gen → 三文件存在；用 rsa 验签路径独立回验 `cert.jws` 签名 + 解析两个 PEM（编码/解码路径不对称，可捕格式错误）。
2. gen(days=1) → renew(--exp 前移) → 新 JWS 验签通过、exp 已变。
3. `exp <= nbf`、`bits < 2048` 报错。

### 文档

`docs/ops-manual.md:146` 故障恢复行改为指向 `oj-cert renew`（注明工具在 `tools/`，单独构建）。

## 不做的（YAGNI）

X.509 证书、SAN/多域名、加密私钥（PKCS#8 加密 PEM）、EC 算法（loader 只认 RS256）、
`bin/` 归置与 CI 矩阵覆盖（工具不随发行）。
