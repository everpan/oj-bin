# 日志：终端输出开关（设计稿）

**日期**：2026-09-01
**状态**：待拍板。§5 汇总 D1~D4；其余按「推荐组合」展开。

## 1. 结论速览

| 问题 | 答案 |
|---|---|
| 是不是「文件 + stdout 双输出」？ | **不是两个 sink**。是 fd 级 tee：stdout/stderr 被换成管道，镜像线程把同一份字节**同时**写回原终端与日志文件 |
| 日志格式可配吗？ | **基本不可配**，硬编码在 `logging.rs:67-72`。唯一可配的是级别，且只能走 `RUST_LOG` 环境变量（config.yaml 配不了） |
| 开关形态 | 默认**只落盘、终端静默**；CLI `--console-log` 或 config `server.console_log: true` 打开终端输出 |

## 2. 现状

| 事实 | 位置 |
|---|---|
| 终端输出经 fd 1/2 的管道镜像落盘，**不是**两个独立 sink | `server/src/logging.rs:96-131` 的 `install_terminal_tee` |
| `dup2` 接管 fd 1/2，原终端 fd 先 `dup` 保存 | `server/src/logging.rs:128-131` |
| 镜像线程逐块写「原终端 + LogWriter」，两线程共享一把锁 | `server/src/logging.rs:180-202` |
| 落盘时剥离 ANSI，文件保持纯文本可 grep | `server/src/logging.rs:236`、`AnsiStripper` `logging.rs:273-308` |
| **tracing 控制台层写的是 stderr** | `server/src/logging.rs:69` `.with_writer(std::io::stderr)` |
| 级别：`EnvFilter::try_from_default_env()`（= `RUST_LOG`），缺省 `info` | `server/src/logging.rs:62` |
| 格式：默认 fmt、`with_target(false)`、`with_ansi(true)`，全部硬编码 | `server/src/logging.rs:67-72` |
| 请求日志字段写死：`method/path/status/ms` | `server/src/logging.rs:317-323` |
| config 只有 `logs_dir`/`logs_max_m`/`logs_keep_files`，管落盘与滚动，**不管格式** | `src/config.rs:24-37` |
| 落盘 tee 是 **unix-only**；非 unix 只 stderr 输出并告警 | `server/src/logging.rs:48-64` |
| 只有 `oj server` 装 tee（`oj test` 等不调用） | `oj/src/server_cmd.rs:38` |

### 2.1 日志格式到底怎么配

- **格式本身：不可配**。tracing 的 fmt layer 参数全部写死（`logging.rs:67-72`）：默认单行
  格式、不带 target、终端带 ANSI 色。想要 json / 改字段顺序 / 改时间格式 → 都要改代码。
- **级别：可配，但只能走环境变量**。`RUST_LOG`（`logging.rs:62`），缺省 `info`。
  config.yaml **配不了级别**，文档里的用法是 `RUST_LOG=oj=info ./oj server ...`
  （`docs/devkit/api-manual.md:1137`）。
- **告警/错误文案**：散落在各模块的 `eprintln!`，同样经 tee 落盘。

## 3. 「非 unix 平台没有落盘」是什么意思 —— Windows 不记日志吗

**是的：Windows 上目前只输出到终端，不写任何日志文件。**

机制上的原因：

1. 落盘靠的是**截获 fd**：`libc::pipe` + `libc::dup2` 把进程的 fd 1/2 换成管道写端，
   原终端 fd 先 `dup` 存一份，再起线程把管道内容一边写回终端、一边写进文件
   （`logging.rs:96-131`、`180-202`）。
2. 这套是 **POSIX fd 语义**，代码整体被 `#[cfg(unix)]` 包住。非 unix 平台走
   `logging.rs:56-64` 的兜底分支：只 `eprintln!` 一条
   `warn: file logging is unix-only; …`，然后什么都不做。
3. 结果是 Windows 上 `server.logs_dir` / `logs_max_m` / `logs_keep_files` **三个配置全部无效**，
   日志只在终端里，关掉终端窗口就没了。

这不是故意不支持，是**没实现**。要做需要换机制（例如 tracing 的 `tracing_appender`
rolling file layer，或 Windows 侧的命名管道 + 镜像线程），是独立议题，本设计不做。

**对本开关的直接影响** —— 这一点很关键：

> 既然默认改成「只落盘、终端静默」，而 Windows 又根本没有落盘，
> 照做会让 Windows 上的日志**彻底消失**（既无终端也无文件）。

因此非 unix 分支必须**强制保留终端输出并告警**（已实现，`logging.rs:60-63`），
即 `console_log` 在 Windows 上等于恒为 true，且会打一行说明原因的 warn。

## 4. 三个必须点破的陷阱（已按此实现）

### 4.1 「关 stdout」关不掉日志 —— tracing 走的是 stderr

tracing 的控制台层写 **stderr**（`logging.rs:69`）。`stdout` 上只有少量直接 `println!`
（如启动行 `oj/src/server_cmd.rs:51`）。只静默 fd 1 的话日志照旧刷满终端。
→ 开关**同时**静默 fd 1 与 fd 2。

### 4.2 静默后用户不知道日志去了哪

静默前用 `eprintln!` 打一行含日志路径的提示（`logging.rs:122-125`）。
这行必须在 `redirect_fd` **之前**打印，否则会被自己刚装的管道吞掉。

### 4.3 管道仍必须读空

关终端只是跳过「回写原终端」这一步，**管道照旧要读**（`logging.rs:196-198`）。
不读的话写端攒满 64K 后进程永久阻塞。

### 4.4 走 `server_cmd::run` 的测试必须显式打开终端

tee 是在**进程级**劫持 fd 1/2，装好之后连 libtest 自己打印的 `test result:` 汇总行和
panic 信息都会进管道。默认关闭终端后，这类测试在 CI 里**看不到任何输出**——
失败时只有一句 "test ... FAILED"，原因全在日志文件里。

已修：`tests/start_cert_expired_test.rs:32` 显式 `console_log: true`（它经
`server_cmd::run` → `logging::init` 装 tee）。
**后续任何新增的、会走到 `server_cmd::run` 的测试都必须带上这一行。**

（`server` crate 的 tee 测试已单独拆成集成测试二进制 `server/tests/log_tee.rs`——
tee 是进程级的，装在单元测试二进制里会让同二进制其余 61 条用例的汇总行与 panic
信息一起进管道，进程退出时丢尾部字节，表现为 `cargo test --workspace` 的用例数
时多时少。该文件头部有完整说明。`install_terminal_tee` 因此改为 `pub`。）

## 5. 决策点

### D1 开关形态：默认开还是默认关 — **用户已定：默认关**

| 方案 | 评价 |
|---|---|
| **A. 默认关（只落盘），`--console-log` / `server.console_log: true` 打开**（用户指定，已实现） | 守护进程/systemd 部署下终端是噪声；日志一律落盘便于统一采集。代价：本地开发时终端静悄悄，与现状手感不同 |
| B. 默认开，`--no-console` 关闭 | 保持现状手感，但服务端默认仍刷终端 |

已按 A 实现。`config` 与 CLI 是「或」关系（两者都是「打开」语义，没有「关闭」的一方）：
`console = cfg.server.console_log || a.console_log`（`oj/src/server_cmd.rs:40`）。

### D2 命名 — 待拍板

| 方案 | 评价 |
|---|---|
| **A. `--console-log` / `server.console_log`**（已实现） | 与「默认关、显式打开」配套；名字准确：控制的是终端这一路 |
| B. `--quiet` / `-q` | 短，但 `-q` 通常暗示降量级，而这里日志量不变、只是换出口 |
| C. `--stdout` | 名不副实，见 §4.1（真正要静默的包括 stderr） |

### D3 实现方式 — 待拍板

| 方案 | 评价 |
|---|---|
| **A. 镜像线程里跳过 `console_w.write_all()`**（已实现） | 改动最小；管道与落盘逻辑原样保留；`println!`/panic 仍落盘 |
| B. 不装 tee，改用 tracing 文件层 | 会丢掉 `println!`/panic/第三方库输出的落盘能力，违背 `logging.rs:1-3` 的原始设计意图 |

### D4 非 unix 平台行为 — 待拍板

| 方案 | 评价 |
|---|---|
| **A. 忽略开关 + 告警，强制保留终端输出**（已实现） | 与 `logging.rs:56-64` 现有「不落盘但必须喊出声」的处理一致；见 §3 |
| B. fail-fast 拒绝启动 | 过重：Windows 上只是想要安静，且它本来就没有别的出口 |
| C. 先补 Windows 落盘 | 独立议题，成本不低 |

## 6. 改动清单

| 文件 | 改动 |
|---|---|
| `server/src/logging.rs` | `init` 增参数 `console: bool`，透传 `install_terminal_tee`/`redirect_fd`/`mirror_loop`；`mirror_loop` 按标志跳过写终端（管道仍读空）；静默前打一行含日志路径的提示；非 unix 分支忽略开关并告警 |
| `src/config.rs` | `ServerCfg` 增 `console_log: bool`，默认 **false** |
| `oj/src/args.rs` | `ServerArgs` 增 `console_log: bool`；clap 增 `--console-log` |
| `oj/src/server_cmd.rs` | `console = cfg.server.console_log \|\| a.console_log` |
| `oj/src/main.rs` | 测试里的 `ServerArgs` 字面量补字段 |
| `docs/devkit/api-manual.md` | 配置表补 `console_log` 行 |
| `sample/config.yaml` | 注释补 `console_log` 示例 |

## 7. 验证

```bash
# 默认：终端静默，只落盘（启动时会打一行日志路径的提示）
cargo run -p oj -- server -c sample/config.yaml --api-path sample/src
tail -f sample/logs/server-*.log

# 打开终端输出
cargo run -p oj -- server -c sample/config.yaml --api-path sample/src --console-log
```

Windows（无落盘）：加不加 `--console-log` 都应在终端看到日志，且默认情形额外打一条
`warn: console output kept: --console-log is ignored where file logging is unavailable` 之类的提示。
