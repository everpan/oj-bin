# 2026-10-18：exec 流式形态契约审计（文档 ↔ 实现）

起因：修完 Windows CI 两条红（夹具模板歧义 / SS8 行尾）后做**同类缺陷全面复查**——
「只在别的平台 / 别的形态上才暴露、本机既编不到也跑不了因而反复漏网」的那一类。

## 1. 发现的偏差（实现未动，只订正文档）

`src/bridge/bootstrap.js:255-284` 的 `runExecStreaming`（即带 `onStdout`/`onStderr` 的
`exec.run`）实际行为：

- 走 `exec.spawn` 字节流 → 逐块转回调 → **同时把全部分块攒在 `outBuf`/`errBuf`** →
  merge 后 `new TextDecoder()`（非 fatal）**有损**解码 → 返回 `{ code, stdout, stderr }`。

而 v0.1.78 起的对外文档（api-manual §6、scenarios 场景 39/40、devkit README、CHANGELOG）
写的是「增量转发、返回 `{ code }`，**不缓冲整段**」。三处不准确：

1. 返回形状其实与缓冲形态一致（含 `stdout`/`stderr`）——`SS7` 用例（`full: r.stdout`）早已依赖它；
2. 实现**确实**攒全量（只是内存受 `drain` 的 10MiB 合计上限封顶，非无界）；
3. 非 UTF-8 行为按形态分档，并非「与缓冲形态完全一致」：

| 情形 | 无回调 `exec.run` | 带回调 `exec.run` | `exec.spawn` |
|---|---|---|---|
| >10MiB / 超时 | **抛** `output limit exceeded` / `timed out` | 不抛：`code = 137` + 截断 | 同左 |
| 非 UTF-8 | **抛** `not valid UTF-8` | 不抛：U+FFFD 有损解码 | 不抛：原始 `Uint8Array` |

另外两条订正：`opts.stdin` **只有无回调的 `exec.run` 支持**（`op_exec_spawn:604` 恒
`Stdio::null`，回调/句柄形态传了静默丢弃）；流式逐块解码要复用同一个 `TextDecoder`
（逐块 `new` 会在块边界把多字节序列切坏 → U+FFFD）。

**处置：订正文档，不改实现。** `r.stdout` 已随 v0.1.78 发布且有用例依赖，删 `outBuf`
是破坏性变更；内存已被 10MiB 上限封顶。不对称语义保留的理由：缓冲形态返回 `String`，
非 UTF-8 只能抛（否则无法表示）；流式 / 回调的价值就是转发二进制管道，强制 UTF-8 会废掉
该用途。

## 2. 补的对抗性用例（unix 面，本机可跑）

`src/bridge/exec.rs` 主场景新增 S12 三形态（`sh -c printf '\200'` 走既有模板
`sh -c ${raw:script}`）：缓冲形态抛（并补 `s12_badutf8` 未覆盖的 **stdout** 侧）、回调形态
不抛且 `stdout` 含 U+FFFD、`exec.spawn` 不抛且给原始字节 `0x80`。三条全绿，把上面那张表
钉死在 unix 也能跑的地方。

`src/bridge/module_loader.rs` 补 `strip_verbatim_is_prefix_only_and_platform_agnostic`
（纯字符串函数，此前只被 `#[cfg(windows)]` 用例覆盖）：剥 `\\?\`、无前缀 / unix 形态原样、
幂等（只剥一层）。

## 3. 四路评审口记与处置

| 路 | 结论 | 处置 |
|---|---|---|
| 工程师 | 有条件批准：偏差成立但不完整（漏 `api-manual:1781`）；三断言成立；用例够 | 全部采纳（1781 已改为对照表）；提醒的「`db_backend.rs:255` 别平台无关化（`dunce` 在 unix 是 no-op，搬过去假绿）」已遵循——未动 |
| 架构师 | 批准：订正文档不改实现；保留不对称语义但要有「形态 × 非 UTF-8」对照表；登记 OJ-11 | 全部采纳；CHANGELOG 记在 v0.1.79 段（不回改 v0.1.78 历史条目） |
| 产品 | 批准：给出 6 处具体落点；定性「文档订正、非行为变更」；值一条 SKILL 陷阱 | 全部采纳（api-manual 1768/1776/1777/1781/1798-1800、SKILL 273/282/新增两条、scenarios 2816/2862/2872/2875、README 40/42） |
| 安全 | 有条件批准：不构成权限绕过（闸门全在 pre-spawn 的 `prepare_exec`，stdout 不参与裁决），但「1781 行与缓冲形态完全一致」是**错误的安全承诺**；另指出 spawn 侧 stdin 恒 null | 全部采纳（1781 改为对照表 + 「别用 try/catch 判成功」；1777 订正 stdin）；建议的另 4 条对抗用例按「不夹带」原则登记（见 §4） |

## 4. 登记（本次刻意不做）

| 项 | 说明 | 激活条件 |
|---|---|---|
| OJ-10 `strip_verbatim` 不剥 UNC 形态 | `\\?\UNC\server\share` → `UNC\server\share`（错误）；网络盘 / UNC 部署才遇得到。属生产行为变更，需独立 spec + Windows 实测 | 有 UNC / 网络盘部署诉求时（与 `src/bridge/fs.rs:403`「dunce 不剥含控制字符路径」同口径一并考虑） |
| OJ-11 回调抛异常时失败语义未定义 | `bootstrap.js` 的 `sink` 未捕获用户 `onStdout` 抛出的异常 → `Promise.all` reject，`exec.run` 的失败语义未定义 | 有「回调里做校验 / 解析」的真实用法时 |
| 未补的对抗用例 | ①回调形态超限 / 超时各一条（钉 137 不抛）；②`opts.stdin` 被静默丢弃；③多字节跨 chunk 解码；④`onStdout` 抛异常后无孤儿进程；⑤`exec.spawn` 侧 `stdin` | 下次动 exec 流式形态时一并补 |
| Windows 夹具的环境依赖（**不红不动**） | `ping` 若因 ICMP 受限提前退出 → `s6_timeout` 断言 `timed out` 红（与本地化无关，超时分支在解码前返回）；10MiB `cmd /c type` 40s 窗；`cmd /c type \\?\C:\...`（verbatim 前缀）**CI 实测可用** | 换 runner / 换机器后复验 |

## 5. 验证手段（本机 macOS）

1. `cargo fmt --check` 绿；
2. `cargo test --release exec_bdd_main_scenarios`（含新增 S12 三形态）绿；
3. `cargo test --release strip_verbatim` 绿；
4. `cargo clippy --release --workspace --all-targets -- -D warnings` 绿；
5. devkit：`cargo xtask build` 归置 `bin/devkit/` + `cargo test --release -p xtask` 8/8（契约）。
