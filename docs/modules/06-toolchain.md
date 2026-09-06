# 06 · 工具链与辅助 crate（`tools/`、`benches/`、`tests/plugins/`）

## 1. `tools/xtask`（`cargo xtask`）

| 子命令 | 行为 |
|---|---|
| `xtask bin` | 编译 `oj`（release）+ 拷入 `bin/oj` |
| `xtask plugin <name>` | 编译 `oj-<name>`（release）+ 拷入 `bin/plugins/<host-triple>/` |
| `xtask plugin <name> --check` | 经 PluginLoader 预检（ABI/身份/semver/按轴符号），输出 desc 与 provided axes |
| `xtask build` | 编译 oj + 全部第一方插件，并归置 DevKit 文档到 `bin/devkit/` |

产物布局（与发行同形，也与插件加载器默认发现路径同形）：

```
bin/
├── oj                          # 主程序
├── plugins/<host-triple>/      # 插件 cdylib
└── devkit/                     # docs/devkit 三件 + oidc 手册 + sample/global.d.ts
```

要点：
- **必须 `cargo build --workspace --release --exclude xtask --exclude oj-cert`**（:105）：
  `-p` 与 `--workspace` 的 feature 归一化不同，会让 rusty_v8 按不同 fingerprint 重编并找不到静态库；
  而 xtask 自身在 Windows 上是运行中的进程，被 relink 会 "Access is denied"。
  `oj-cert` 是独立签名工具（不随发行），排除以免每次连带编译 rsa/clap 依赖树。
- `host_triple()`（:43）由 `rustc -vV` 的 `host:` 行取；与运行期
  `ffi::triple()`（按 `std::env::consts` 重建）一致。
- 插件存放文件名以 **descriptor.name** 为名（与 rustc 产物名解耦，`-`→`_` 不同）。
- `PLUGINS`（:25）= es / db-mysql / db-postgres / blob-s3 / bus-kafka / bus-rabbitmq /
  kv-redis / **auth**。

## 2. `tools/oj-cert`（证书工具，独立不随 oj 发行）

```
oj-cert gen   -o <dir> [--days 365] [--bits 2048] [--nbf <unix>] [--exp <unix>]
oj-cert renew -k <private.pem> [-o <dir>] [--days 365] [--exp <unix>]
```

- `keygen`（RSA，下限 `MIN_BITS` = ring `RSA_PKCS1_2048_8192_SHA256` 的验签下限）、
  `private_pem`（PKCS#8，unix 下 chmod 600）、`public_pem`（SPKI）、
  `sign_jws`（b64url no-pad 三段）。
- `--days` 是**有效天数**（`days_to_expiry = days * 86400`），避免把天数误当秒。
- `gen` 是 edition 2024 保留字，公开名仍是 `gen`，定义写作 `r#gen`。
- `renew` 只写新 `cert.jws`（公钥不变）→ 配合 server 证书热重载免重启续期。
- 与 `server/src/certificate.rs` 契约一致：`header {"alg":"RS256","typ":"JWT"}` +
  payload `{nbf, exp}` + RS256。
- 测试 `tools/oj-cert/tests/cert_gen.rs`：用 rsa 独立解码 + 验签（与生成路径不对称，
  可捕格式错误），含拒绝覆盖已存在私钥、`renew` 拒绝非法 exp 等负路径。
- 根 crate 通过 dev-dep `oj-cert` 复用其 PEM 解析（`cert.renew` 需 `from_pkcs8_pem`），
  rsa 版本与 feature 与之一致（workspace 单版本）。

## 3. `benches/bridge.rs`（criterion）

两层口径：

- `rust/*`：纯 Rust 层（信封序列化、trait 实现），无 JS 开销；
- `js/*`：JS op 全链路（JS 调用 → op → Rust → Promise 解析），每次迭代在 JS 循环里
  执行 `N = 100` 次 op，结果除以 N 得单次 op 耗时。

`cargo bench` 运行；`cargo build --benches` 只编译。数据见 `docs/benchmarks.md`。

## 4. 测试夹具插件（`tests/plugins/`）

| 夹具 | 用途 |
|---|---|
| `mini` | 演练加载 / ABI 门禁 / panic 路径 |
| `mini-kv` | 提供 kv 轴的夹具，供按轴 dlsym 与 kv 装配测试 |

二者都是 workspace 成员的 cdylib，由 `cargo test --workspace` 编译；
`src/bridge/plugin_loader/tests.rs`（381 行）用它们覆盖七类加载失败。

## 5. `spikes/`

`spikes/ffi-tx/plugin/` 是 FFI 事务边界的可行性验证（165 行）。
**已不活跃**：不在 workspace members 里，也无引用。建议确认后删除或明确标注为历史存档。

## 6. 已知债

- ~~CI 插件构建列表漏 `auth`~~ —— 已于 2026-09-06 整改：`plugin-matrix.yml` 改为
  `cargo run --release -p xtask -- build`，清单单一真相来源即 `xtask` 的 `PLUGINS`。
- ~~sample 的 L1/L2 无 CI job~~ —— 已补 `plugin-matrix.yml` 的 `sample-tests` job
  （见 [08-testing.md §6](08-testing.md)）。
