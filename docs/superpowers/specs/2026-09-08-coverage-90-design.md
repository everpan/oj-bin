# 测试覆盖率 90% 达标设计（2026-09-08）

## 执行进展（滚动更新）

- 基线 79.44% → **84.18%**（lcov 实测 @ 波2中途）；目标 missed ≤ 2,023（lcov 口径 20,234 行）。
- **关键解锁**：db 双插件的 `[dev-dependencies] sqlx + "sqlite"` 特性统一——测试构建给
  sqlx Any 装 sqlite 驱动（`install_default_drivers()` 按启用特性安装），生产 cdylib
  不受影响仍单方言。vtable 全执行体（连接/DDL/参数化/事务/错误面）离线可测，
  ~900 行解锁（此前仅 env-gated 真库可达）。
- 兼修出真 bug 一个：`oj-cert renew` 指定不存在 out_dir 直接写盘失败（gen 会建目录），
  已对齐修复。
- 剩余大头：rabbitmq/kafka/s3/redis 插件 I/O 体（~1,160 行，离线不可达——AMQP/Kafka/S3
  无内嵌替身，mini-RESP 基建风险大暂缓）；其余为散点错误臂。

## 波 3 runbook（解锁最后百分点的环境矩阵）

`/tmp/oj-testenv/docker-compose.yaml`（mysql/pg/redis/rabbitmq/minio+mc/kafka kraft 已定义），
环境变量对应：`OJ_TEST_MYSQL` / `OJ_TEST_PG` / `OJ_TEST_REDIS` / `OJ_TEST_RABBITMQ_URL`
/ `OJ_TEST_KAFKA_BROKERS` / `OJ_TEST_S3=endpoint|bucket|region|access|secret|path_style`。
rabbitmq 的 mq roundtrip 已改为测试内 lapin 自声明队列（不再依赖外部 init）。


## 口径

- workspace 聚合**行覆盖** ≥90%（`cargo llvm-cov --workspace --summary-only`）。
- 一次达标，**不加 CI 门禁**（用户裁决）；后续回退靠自觉。
- 基线（2026-09-08 @ 6689f1b+）：行覆盖 **80.34%**（21,577 行，missed 4,243）；
  目标 missed ≤2,158，净新增覆盖 ≥2,100 行。
- 全部新用例 **BDD given/when/then 命名 + 业务视角注释**（仓库既有惯例，
  如 `given_mq_cfg_without_url_when_connect_then_err_fail_fast`）。

## 三波推进

### 波 1 · 纯 Rust 零依赖（~540 行缺口）

| 文件 | missed | 现状 |
|---|---|---|
| `oj/src/test_cmd.rs` | 252 | 0% |
| `tools/xtask/src/main.rs` | 194 | 0% |
| `oj/src/test_ext.rs` | 59 | 0% |
| `tools/oj-cert/src/main.rs` | 32 | 0% |

main 拆薄为可测函数（唯一必要重构），BDD：给定 CLI 参数/夹具目录 →
当执行 → 则产物与退出码。

### 波 2 · 插件离线面（~1,700 行缺口）

- `oj-db-mysql` / `oj-db-postgres`：目前仅 5%——补到 rabbitmq 离线水位
  （cfg 校验、method dispatch、SQL/payload 构造、错误映射），每插件 15-25 用例。
- `oj-bus-rabbitmq` / `oj-bus-kafka` / `oj-blob-s3` / `oj-kv-redis`：补齐残余离线分支。
- 顺带 bridge 错误路径（`ffi.rs` / `plugin_loader.rs` ~150 行）。
- 插件生产代码零改动；协议级 mock 重构已否决。

### 波 3 · 环境门控实测

- 本机 colima 起 mysql/postgres/redis/kafka/rabbitmq/es/minio，
  设 `OJ_TEST_*` 环境变量跑既有 roundtrip 用例——真实 I/O 路径入账。
- 覆盖报告注明环境前提；测量命令文档化。

## 红线

- 不为覆盖而测：断言业务行为（信封、路由、ack 语义、fail-fast），非仅执行到。
- 零新增依赖；每波回归 `cargo test --workspace` + fmt + clippy。

## 验收

行覆盖 ≥90% + 全测试绿 + fmt/clippy 过。
