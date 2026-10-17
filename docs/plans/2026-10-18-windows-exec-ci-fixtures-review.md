# Windows exec CI 夹具修正评审：v0.1.79（2026-10-18）

- 输入：windows-latest 的 `cargo test --workspace --release` 上 `src/bridge/exec.rs`
  两个 `#[cfg(windows)]` BDD 用例变红（:1627 / :2081），此前已反复修补多次仍红。
- 方式：四路只读评审（工程师 / 架构师 / 产品 / 安全），本次四路均已到齐；本文件为
  综合判决与登记。
- 约束：只调整 oj-bin；本次范围 = Windows CI 夹具 + 文档，生产契约不动。

---

## 1. 总判决

| 项 | 判决 | 一句话理由 |
|---|---|---|
| 根因是夹具期望值错，不是生产代码缺陷 | **成立**（四路一致） | cmd 的 `echo` 恒补 CRLF、`type` 原样吐字节不补行尾，两条约定此前散落在 8 处字面 |
| 生产侧做 CRLF→LF 归一 | **不做** | 破坏字节保真（二进制 / 压缩包管道必坏）、与 `Deno.Command.output` / Node `child_process` 口径分道；OUTPUT_LIMIT 按字节计数的语义会漂 |
| 行尾真相源（`IN_TXT` / `echo_out`） | **做** | 期望值与写入内容同源，新增场景不可能再写错 |
| 大文件夹具显式 `timeoutMs: 40000` | **做** | `cmd /c type` 吐 10MiB 的速率受负载与实时防护扫描影响，30s 缺省先到会把「输出超限」误呈为「超时」 |
| 夹具逐块 `TextDecoder` → 合并字节后解码 | **缓做**（本次未做） | 夹具输出全 ASCII，与生产 `runExecStreaming`（先合并再解码）逐字节等价，零红收益；不单独再跑一轮 CI |
| unix / win 两套夹具合并为一份参数化 | **不做** | `sh -c` 家族、信号终止、非 UTF-8 stderr、组杀残留在 Windows 无对应物；参数化会把 cfg 分支塞进 JS 模板串，可读性更差 |

## 2. 根因

Windows 夹具是 unix 夹具的复制品，而「echo 输出行尾」这条**命令语义**被人在 8 处断言里
各写一遍：修完报红的那一处，下一处接着红——这就是「已修多次仍红」的机制。真正该消除的
不是某一处的字面，而是「字面散落」本身。

## 3. 已做（v0.1.79）

- `#[cfg(windows)]` 段新增行尾真相源：`IN_TXT`（写入与 `s1_cat` 期望同源）+ `echo_out`
  （`cmd /c echo` 恒补 CRLF）；8 处断言一律经二者拼装，注释钉死两条 cmd 约定并写明
  「禁止在断言里手写 `\r\n` / `\n`」。
- 大文件夹具（主场景 `s5_big` / 流式 `SS5`）显式 `timeoutMs: 40000`（仍在用例 60s 窗口内）。
- 生产代码零改动：`op_exec_run` / `op_exec_spawn` / `run_managed` / `drain` 逐字未动。
- 文档：devkit 四件补 Windows 面（api-manual §6 两条 + 错误表非 UTF-8 条目、SKILL 两条、
  scenarios 场景 39/40 各一条、README 版本行），并顺手删掉 §6 小节尾部的游离 `**`。

## 4. 登记不做（含激活条件）

| 项 | 说明 | 激活条件 |
|---|---|---|
| OJ-8 PATHEXT 缺口 | `default_resolve_command` 在 Windows 只 `is_file()`、不探测 PATHEXT → 裸名 `cmd` / `tar` 永远解析不到 `.exe`。**已在 `2026-10-17-oj-bin-submissions-review.md` 评审为「做」，本次刻意不带**（改生产契约，需独立 spec + 验收） | 按 OJ-8 既定顺序执行；本次修 CI 不混进 |
| supervisor 的 kill / 自然退出竞态 | `op_exec_spawn` 的 supervisor 里 `biased` 只在「同时就绪」时优先 kill；子进程写完即退、与 drain 越过 OUTPUT_LIMIT 发 kill 撞在一起时，可能采到自然退出 → 流式形态的 137 不严格保证（SS4/SS5/SS6 现只判非 0，正是因此） | 有用例需精确断言 137 时先消竞态（把「已请求 kill」做成状态位，不再依赖 select 顺序） |
| `CREATE_NO_WINDOW` | spawn 控制台子进程（cmd.exe / ping.exe）未设 `CREATE_NO_WINDOW`：headless runner 无影响，GUI 服务形态下会闪窗 | 有桌面形态部署诉求时 |
| Windows 子进程输出为本地代码页 | GBK / CP1252 等非 UTF-8 输出落 `exec: stdout is not valid UTF-8`（**已文档化**，`chcp 65001` 是可行解） | 有中文 Windows 部署实测诉求时再评估是否提供解码选项（默认保持 fail-fast） |
| 孙进程残留 | Windows 无进程组，`kill_tree` 只杀直接子进程；`cmd /c foo.bat` 之类派生的孙进程会存活（**已文档化**） | 有「必须杀干净」的互斥诉求时 |

## 5. 验证手段（本机非 Windows 时如何验 Windows-only 代码）

`cfg(windows)` 的代码在本机既不编译也不进格式化检查，是这类缺陷反复漏网的直接原因。
本次用两招，均不需要 Windows 机器 / 不装 windows-msvc target：

1. `cargo fmt` 会解析**全部** cfg 分支——能抓语法错与格式漂移；
2. 把该文件的 `#[cfg(windows)]` 临时翻成 `#[cfg(all())]`（跳过 `default_resolver_oj_and_path`
   里那条与 `#[cfg(unix)]` 成对的 `let probe`，否则重复定义），再
   `cargo check --release --tests`：本机即真正类型检查一遍 Windows 夹具；验完从备份还原。

## 6. 各路评审口记

- 工程师（review-engineer）：已到齐；逐条核对各 scenario 的 Windows 语义，指出 10MiB
  `type` 与 30s 缺省超时窗的条件性第 3 红（→ 已做 `timeoutMs`），并指出 SS2 的本地化
  ping 输出是「非 UTF-8」登记项的真实引爆位。
  **2026-10-18 订正**：这条判断不成立——SS2 带 `onStdout`，走 `runExecStreaming`
  （bootstrap.js:255-284）逐块转发 + **有损**解码，非 UTF-8 不抛。真正强制 UTF-8 并抛错的
  只有**无回调的缓冲形态**（`exec.rs:877 String::from_utf8`），Windows 面仅 `s6_timeout`
  属此形态，而它在超时分支（`:861`）直接返回、不解码。完整口径见
  `2026-10-18-exec-streaming-contract-audit.md`。
- 架构师（review-architect）：已到齐；否掉夹具合并式参数化，定「真相源 = 平台适配函数」
  而非裸常量，定生产侧不做行尾归一。
- 产品（review-product）：已到齐；定文档另起、不混进测试修复，给出 devkit 四件具体落点；
  坑排序：OJ-8（部署第一分钟必踩）> CRLF > 非 UTF-8 > 闪窗。
- 安全（review-security）：已到齐；确认 8 处改动**未**把精确 `assert_eq` 降级为
  contains / 非空（未放宽），并要求把流式 137 的竞态登记（见 §4）。
