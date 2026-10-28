# oj 插件开发模板（db 轴）

可拷贝起手的 **db 轴 cdylib 插件骨架**：一个 `demo` 方言（进程内范式载体，不连真库）。

它不是「能用的数据库驱动」，而是把 [db 轴四契约](../../docs/plugins/plugin-development.md#15-新增数据库方言从零接入的-conformance-清单)
摆全，让新方言插件照着填：能力位自报（`describe`）、结构化错误信封、流式三态信封、
句柄 / 事务 / 游标生命周期，以及「声明与行为不许漂移」的结构化写法。

> 同目录旁的 [`../plugin-template`](../plugin-template/README.md) 是**泛型轴**模板（`cache` 轴），
> 两者解决的是不同问题：泛型轴 = 宿主零改动扩一个新能力面；db 轴 = 进宿主已有的 SQL 能力面。

配套文档：

| 文档 | 用途 |
|---|---|
| `docs/plugins/plugin-development.md` §15 | 接入全流程；§15.2 能力位注册表（每能力位一正一反），§15.6 可直接拷进 PR 的自查表 |
| `docs/devkit/db-guide.md` §10.6 | 六类 schema 类型的 DDL 映射 + 读侧 JS 值契约 |
| `docs/plugins/plugin-development.md` §6 | FfiFuture 异步桥（唯一异步路径） |

模板 crate **不进 workspace members**（它不是构建目标，是被拷贝的骨架）。骨架自带的空
`[workspace]` 段只为了让模板在 oj 仓库内能被独立 `cargo build` / `cargo test` 验收。

## 起手：改四样东西

| 改什么 | 位置 | 说明 |
|---|---|---|
| 包名 | `Cargo.toml` 的 `name` | `oj-db-<dialect>` 惯例 |
| 方言名 | `src/lib.rs` 的 `DIALECT` | 同时是 `describe().dialect`、`dialect(handle)`、`e.data.dialect`、启动横幅——**一处改全改** |
| DSN scheme | `src/lib.rs` 的 `SCHEME` | 装配期据此把 `connect` 路由到本插件（宿主不硬编码白名单） |
| 能力位 | `src/lib.rs` 的 `CAP_PLACEHOLDER` / `CAP_ID` / `SUPPORTS_STREAM` / `SUPPORTS_CANCEL` | 8 选 4；串值不得自创（宿主闭集解析，typo = 启动失败） |

然后只替换驱动接缝：`do_connect` / `do_query` / `do_exec` / `do_begin` / `do_tx_*` /
`do_stream_*`（共 12 个 `do_*`）。vtable、`init`、`descriptor`、信封、游标状态机都不用动。

## 命名三方对齐（先读这段）

沿用 `oj-db-mysql` 的惯例——**包名 ≠ descriptor 名**：

| 位置 | 本模板的值 |
|---|---|
| crate 包名 | `oj-db-demo`（产物 `liboj_db_demo.dylib`） |
| `cargo xtask plugin <name>` | `db-demo`（若走 xtask） |
| `descriptor.name`（`init`） | `db-demo` → loader 文件名 `libdb-demo.dylib` |

`plugins:` 清单键与 `plugins.<db-demo>` 透传段键按 **descriptor.name** 查。改方言名时三处一起改。

## 两条落地路径

### 路径 A：在 oj 仓库内开发（`cargo xtask plugin`，推荐）

```bash
cp -r tools/plugin-template-db plugins/oj-db-<dialect>
cd plugins/oj-db-<dialect>

# 1) 删 Cargo.toml 里的空 [workspace] 与 [profile.release] 段（后者继承根 Cargo.toml）；
# 2) 把 "plugins/oj-db-<dialect>" 加进根 Cargo.toml 的 workspace members；
# 3) 按上表改包名 / 方言名 / scheme / 能力位与 do_*。

cargo xtask plugin db-demo         # 构建 + 拷到 bin/plugins/<host-triple>/libdb-demo.dylib
cargo xtask plugin db-demo --check # 预检（ABI / 身份 / semver / 轴符号）
```

> `--check` **不**校验 `describe()`——能力位在**装配期**解析。跑一次 `./bin/oj serve -c ...`
> 看启动横幅的 `caps` 行（非法能力串会直接拒载并点名），是验证 describe 的唯一真实路径。

装配（`config.yaml`）：

```yaml
plugins:
  db-demo: {}    # 空对象 = 回落轴适配器；db 插件无装配期配置，DSN 在 connect 按值传入
```

### 路径 B：独立 manifest（不进宿主仓库）

```bash
cp -r tools/plugin-template-db ~/oj-db-<dialect> && cd ~/oj-db-<dialect>
cargo build --release     # 保留空 [workspace] 段；调整 oj-plugin-ffi 的 path 依赖
cp target/release/liboj_db_demo.dylib <oj仓库>/bin/plugins/<host-triple>/libdb-demo.dylib
```

`<host-triple>` 用 `rustc -vV | grep host` 取。扫描模式下**锁外的 `.so` 会被拒载并点名**——
自建产物请同步更新 `plugins.lock.json`，或显式走 `plugins:` 严格清单装配。

## 自检（模板已内置 11 个用例）

```bash
cd tools/plugin-template-db
cargo fmt --check
cargo clippy --release --all-targets -- -D warnings
cargo test --release        # 11 passed
```

用例与 `§15.6` 的对应关系：

| 用例 | 钉死的契约 |
|---|---|
| `describe_is_valid_capability_json` | 能力串闭集合法（两组各一枚 + `stream`/`cancel` 与常量同源） |
| `stream_open_behavior_matches_declaration` | 声明 `stream` ↔ 真返回 `stream_id`；未声明 ↔ `{"unsupported":true}` |
| `cancel_never_claims_unsupported_capability` | 声明 `cancel` ↔ 真实现；未声明 ↔ `unsupported` + `data.capability` |
| `envelope_has_all_five_fields_for_every_category` | 八类错误信封字段集恒为 5 个，`category`/`disposition` 在闭集内 |
| `foreign_scheme_is_unsupported_not_connection_error` | DSN 写错 ≠ 连接故障（分类错了告警会打到 infra） |
| `closed_handle_is_connection_error` | 已关闭句柄 → `connection` + `close` 处置 |
| `query_echoes_bound_params_in_order` | 绑定顺序与 SQL 回显（占位符改写器对账用） |
| `tx_lifecycle_and_use_after_commit` | 提交后再用同一 `tx_id` 可诊断，不静默跑在自动提交上 |
| `cursor_batches_by_100_and_done_is_idempotent` | 100 行批边界；`done` 幂等；**绝不裸 null** |
| `stream_rows_match_query_and_cancel_then_close` | `stream` 与 `query` 逐值一致；取消后 `{"error":"cancelled"}`；`close` 回收且幂等 |
| `dialect_reports_unknown_for_missing_handle` | 缺失句柄上送 `"unknown"`，绝不冒充真方言 |

**防漂移验证**：把 `SUPPORTS_STREAM` / `SUPPORTS_CANCEL` 改成 `false` 再跑
`cargo test --release`，仍是 **11 passed**——声明与行为由同一个常量驱动，翻转后两侧同时翻转。
（真实驱动还应各补一条真库用例：cancel 后 ≤5s 服务端查询消失、流式与查询逐值一致。）

**真实宿主验收**（单测验的是插件自己，下面这一步验的是「宿主真的解析了 describe」）：

```bash
cp target/release/liboj_db_demo.dylib <plugins_dir>/<host-triple>/libdb-demo.dylib
OJ_PLUGINS_DIR=<plugins_dir> ./bin/oj serve -c <config.yaml> --api-path <src>
```

日志里应有两行（`caps` 由 `describe()` 解析而来）：

```
[oj-serve] host ABI_VERSION = 12
[oj-serve] plugin db-demo abi=12 fp=… | caps: placeholder=: id=none stream cancel
```

本模板实测过两条（2026-10-27）：正确 describe → 横幅如上；把 `CAP_PLACEHOLDER` 改成
`placeholder_colon_` → 启动即失败，逐字报 `unknown capability "placeholder_colon_"`。

## 模板故意不做的事

照抄前请知情——这些是骨架的边界，不是遗漏：

- **不连真库、不做类型映射**：`query` 回显 SQL/参数，`exec` 恒返回 `0`。行转换三条硬要求
  （未识别列类型报错 / `>2^53-1` 出十进制串 / 布尔统一）写在 `do_query` 的注释里，
  契约在 `docs/devkit/db-guide.md` §10.6；
- **`cancel` 只置标志**：真实驱动必须真的去杀服务端查询，否则超时看门狗白等；
- **`stream` 游标全部物化在内存**：真实驱动应持服务端游标或分批 fetch；
- **无 sidecar**：走子进程形态（如 Go/JDBC 桥）时，`§15.4` 的五条生命周期契约
  （自增 id 协议 / stderr-only / 读看门狗 / 进程树回收 / 退避熔断）必须补齐，模板不覆盖。

## 常见坑

| 症状 | 原因 |
|---|---|
| 启动即失败 `unknown capability "placeholder_colon_"` | 能力串拼错。宿主 fail-loud 且点名违规串——这是**设计**，不要改成静默忽略 |
| `plugins()` 里 `caps` 为 `null` | `describe()` 返回空串或非法 JSON（`catch_value` 的 fallback 是 `""`） |
| 大结果集查询直接报错 | 声明了 `stream` 却返回 `{"unsupported":true}` = 插件说谎，宿主**不**回退 |
| `e.code` / `e.retryable` 全是 `undefined` | `Err` 载荷给了纯文案而非信封 JSON |
| `db.table(...)` 生成的 SQL 语法错 | `CAP_PLACEHOLDER` 与实际方言不符（`?` / `$1` / `:name`） |
| `insert()` 拿不到自增主键 | `CAP_ID` 与实际不符（`RETURNING` / 两步 / 无） |
| 插件 panic 直接杀掉宿主 | `panic = "abort"`；必须 `unwind`（`oj_plugin_entry!` 与 `catch_future` 都靠 `catch_unwind`） |
