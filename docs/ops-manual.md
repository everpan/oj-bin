# oj server 运维手册

面向部署、发布、排障。功能面见 `docs/user-manual.md`，实现面见 `docs/dev-manual.md`。

## 1. 构建与发布

```bash
cargo build --release
ls -lh target/release/oj          # 独立二进制，无运行时依赖（deno_core 内嵌）
```

发布物 = `target/release/oj` + 项目目录（`dist/` + `config.yaml` + `seed.sql` + `node_modules/`）。

发布流程：
1. `cargo build --release`（确认 debug/release 双绿）。
2. `oj build -d src -o dist`（无参 = 全部模块）——生成各模块版本目录
   `dist/<module>-<version>/`（产物保留 src 目录结构与原名，如 `account/api.js`，
   默认 minify 成单行）、锁文件 `dist/manifests.yaml` 与确定性发布包
   `dist/<module>-<version>.tgz`（同输入重复打包字节一致，可校验完整性）。
   排障需要可读产物时加 `--no-minify` 重建。
3. 打包 `oj` 二进制 + `dist/` + `config.yaml` + `seed.sql`（可选）+ vendored
   `node_modules/`（裸 specifier 运行时解析依赖它，**不打进 tgz**）。
4. 目标机解包，`./oj server -c config.yaml -d dist`（release 默认跑 `.js`，**不带 `--dev`**）。

## 2. 运行

```bash
./oj server -c config.yaml -d dist            # release
./oj server -c config.yaml -d src --dev        # dev（跑 .ts，改文件即生效）
```

启动时打印模块清单 + 路由表，可据此核对发布是否完整。

## 3. 配置管理

`config.yaml` 全字段可省，均有默认。生产要点：

- **端口**：代码默认 `778`，但属 macOS/Linux 特权端口（<1024），需 root；**生产用 ≥1024**（如 9778）。
- **超时** `server.timeout`：单请求熔断阈值（`"30s"` 等）。设太大会放大死循环占用；设太小误杀慢查询。
- **并发** `server.pool_size`：JS 执行线程数，等于并行请求上限。过高吃内存，过低排队。
- **静态站点** `server.root`：静态文件根（相对 config 目录）。API 未命中的 GET/HEAD 落此目录
  （目录 → `index.html`）；目录缺失启动即报错。前置站点产物（如 oj build 的 dist）放独立目录。
- **DB** `db.default = "sqlite://<path>"`：相对 config **所在目录**（`config_dir_of` 保证非空）。
  v0.1 仅 sqlite。`sqlite::memory:` 仅测试用，重启即丢。
- **Redis** `redis.default`：v0.1 **不真连**，仅 warn 并退回内存 KV——多实例部署时 KV 不共享，
  需业务自行注意（避免把跨实例一致状态塞进 `redis`/`kv`）。
- **seed.sql**：项目根存在则启动时对 `default` 库重放。语句按 `;` 切分 → **seed 内不得有分号
  字面量**；用 `INSERT OR IGNORE` 保证可重复执行。

## 4. 热重载语义

- **dev 模式**：`api.ts` 及其依赖按 mtime 缓存；改文件后下次请求用新代码（mtime 版本化 specifier）。
- **release 模式**：跑编译好的 `.js`，同样按 mtime 失效（dist 更新即生效，无需重启）——
  但**版本目录布局下换版本需重启**：`dist/manifests.yaml` 仅启动时读取，运行中改锁指向
  新版本目录不会生效。同版本重建（清场重写同目录）靠 mtime 失效即时生效。
- **不触发热重载**：`config.yaml`（重启生效）、`seed.sql`（仅启动重放）、`manifest.yaml` 新增/删除
  模块（重启生效）、`node_modules` 新增包（重启生效，已加载包缓存于进程）。

## 5. 超时与资源

- 超时 handler → 对应 JsRuntime 被 `terminate_execution` 强杀并**丢弃不回池**，HTTP 408。
  server 不崩，后续请求正常（`../oj/tests/e2e.rs` 的 `uc12` 验证了这一点）。
- `RuntimePool` 最大空闲 16；池在负载后自动收缩。被杀的 runtime 会即时从池移除。

## 6. 日志

`tracing-subscriber` 输出：启动横幅（模块/路由表）、请求日志（方法/路径/状态/耗时）、
`log.*`（handler 内 `log.debug/info/warn/error(msg, ...kv)` 结构化输出）、redis 退回 warn。
生产建议用 `RUST_LOG` 控制级别：

```bash
RUST_LOG=oj=info ./oj server -c config.yaml -d dist
```

## 7. 排障表

| 症状 | 原因 | 处置 |
|---|---|---|
| 启动即报「missing manifest.yaml」 | 某首层子目录缺 `manifest.yaml`，或残留空目录 | 补齐；删除空目录（空目录不参与 git，但 `read_dir` 会扫到） |
| 启动报「manifest name mismatch」 | `manifest.yaml` 的 `name` ≠ 父目录名 | 对齐 |
| 启动报 `manifests.yaml … run oj build first` | release 下锁文件缺失/损坏，或指向不存在的版本目录 | 跑 `oj build <module>`；锁被手工改坏时按报错修 |
| 启动报「version dir collision」 | 两个 (module, version) 组合拼出同一目录名（如 `a`/`1-x` 与 `a-1`/`x`） | 改 version 命名避开 |
| 404 | 路由无对应 `api.ts/js`，或目录穿越/非法段 | 核对路径与 `-b` 前缀；release 先确认模块在锁内 |
| 启动报 `server.root …` | 静态根目录不存在（相对 config 目录解析） | 建目录或改路径；不配 `root` 即关闭静态服务 |
| 静态文件 404 | 文件不存在 / 目录缺 `index.html` / 非 GET/HEAD / 无 SPA 回退（v0.1） | 核对文件；SPA 场景先经前置反代补写回退 |
| 405 `method 'del' not exported` | `DELETE` 请求但 handler 没导出 `del`（不是 `delete`） | 改导出名 |
| 500 信封含 `api.ts` 字样 | TS 编译/解析错误 | 看 msg 定位行号 |
| 408 | handler 死循环/超时 | 查死循环，或调大 `server.timeout` |
| 非 sqlite DSN 启动失败 | v0.1 只支持 sqlite | 改 `sqlite://` |
| `redis` 数据不跨实例 | v0.1 退回内存 KV | 改走 `db` 或外部服务 |
| 端口占用 | `778` 需 root | 换 ≥1024 端口 |
| 改 `api.ts` 不生效 | release 下 `dist/` 未更新 / 已加载包缓存 | 确认 dist 同步；必要时重启 |

## 8. 回滚与恢复

- 配置/代码均有版本；二进制与 `dist/` 打包发布，回滚 = 换回上一版打包产物。
- **多版本共存回滚**：`dist/` 内旧版本目录不被构建清除（仅锁内当前版本的同名目录清场重建），
  回滚单模块 = 把 `dist/manifests.yaml` 该模块指回旧版本 + 重启 server（锁仅启动时读）。
- sqlite 数据文件随 `db.default` 路径落盘；升级前备份 `*.sqlite`。
- 无外部依赖（Redis 未真连），故障面小：主要是「二进制 + dist 不一致」→ 保持二者同版本发布。
