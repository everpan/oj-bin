# 脑暴：oj `exec.run` Windows PATHEXT / 非 UTF-8 解析

> 源需求：oj-module `docs/prd/20261104-oj-windows-exec-resolver-requirement.md`（阻断级，Windows 上 `ojm market install` 全链路挡在 `extracting`）。
> 修复主责在 **oj-bin**（`src/bridge/exec.rs` 的 resolver + UTF-8 解码），部分在 **oj-module**（模板 / `extract.ts` / CI）。本仓库只动 oj-bin，oj-module 只读给出建议。

## 1. 需求提炼（按 PRD R1–R5）

- **R1（必须）** resolver 在 win32 上按 `PATHEXT` 补全扩展名后解析裸名；至少 `tar`/`tar.exe`/`cmd`/`cmd.exe` 都能解析到绝对路径。fail-closed 白名单语义不变。
- **R2（必须）** 官方 Windows 命令模板（list/extract/migrate 三条），覆盖 devkit 文档与 `config.yaml` 模板。当前 POSIX 三条 `tar` 模板在 Windows 无官方形态。
- **R3（必须）** 非 UTF-8 输出可消费：①`cmd /c chcp 65001` 可行路径；②`exec.run` 无回调形态非法 UTF-8 改有损解码（U+FFFD）而非抛错；③安装器走 `exec.spawn` 拿原始 `Uint8Array` 自解（`extract.ts` 已 feature-detect spawn）。
- **R4（建议）** 文档与实现对齐：订正 `SKILL.md`「`cmd` 由 resolver 解析为绝对路径」等表述，明确 PATHEXT / 本地代码页 / `-tvzf` 与 UTF-8 关系；`scenarios.md` 场景 34「部署机没有 tar」补 Windows 处置。
- **R5（建议）** Windows CI 把 `ojm market walkthrough`（14 项）纳入，防 `exec.run` 平台回归。

验收（AC）：Windows 中文、oj≥修复版、`tar` 仅系统自带 bsdtar（无 Git usr/bin）时 14 PASS/0 FAIL；负向 5 项仍全拦；`template mismatch`/`ambiguous`/`NotCapable` 与白名单 fail-closed 不变；POSIX 零回归。

## 2. 根因定位（代码证据，oj-bin 侧）

- `default_resolve_command`（exec.rs:965-985）：非 unix 分支仅 `dir.join(name)` 后 `is_executable_file`；`name="tar"` 找的是无扩展名 `tar`，而盘上只有 `tar.exe` → `command 'tar' not found in PATH`。`cmd` 同理。
- `is_executable_file`（exec.rs:995-998）：非 unix 仅 `p.is_file()`，不判断扩展名是否在 PATHEXT 中。
- `run_managed`（exec.rs:902-907）：`String::from_utf8` 失败即抛 `exec: stdout is not valid UTF-8`。Windows bsdtar `-tvzf` verbose 含本地化月份名（GBK `11\xD4\xC2 04`）+ CRLF，必非 UTF-8。

## 3. 候选方案

### R1：resolver PATHEXT 感知（oj-bin，推荐落地）

改写 `default_resolve_command` 的非 unix 分支为 `cfg(windows)` 专用逻辑：

1. `name == "oj"` → `current_exe`（保持）。
2. 含 `/` 或 `\` → Err（保持）。
3. 取 `PATHEXT`（env，缺省 `.COM;.EXE;.BAT;.CMD;.VBS;.VBE;.JS;.JSE;.WSF;.WSH;.MSC`）。
4. 遍历 PATH：先试 `dir.join(name)`（无扩展名命中，兼容 #5/#6 的 shim），再对每个 ext 试 `dir.join(format!("{name}{ext}"))`（cmd 语义：先裸名后依次补扩展）；首个存在文件即返回。
5. 非 windows 保持原 `is_executable_file`（unix 可执行位）。

安全不变量：只解析真实存在的文件为绝对路径；上游 `exec.allow` 模板门禁与 `template::match_all` 完全不变 → fail-closed 不降级。`cmd` 解析后，其 argv 后续的 `tar` 由 cmd 自身解析（R2 可用 `cmd /c tar ...` 兜底形态）。

### R3：非 UTF-8 输出可消费

- **方案 ②（oj-bin，推荐主选）**：`run_managed` 的 `String::from_utf8` 失败分支改为 `String::from_utf8_lossy(...).into_owned()`，并保留区分（可带 `?lossy` 标记或仅静默）。与「带回调/exec.spawn 拿字节」的 JS 侧解码语义对齐。改动最小、零 oj-module 依赖。代价：非 ASCII 静默变 U+FFFD，安装器若依赖精确字节需自测（list 输出路径在行尾，月份损坏不影响路径抽取，实测可接受）。
- **方案 ③（oj-module，高保真替代）**：`extract.ts` 走 `exec.spawn` 拿原始 `Uint8Array`，Windows 用 `TextDecoder('gbk')`、其余用 `utf-8` 解码。不改动 oj 核心语义，解码正确。代价：oj-module 跨平台分支 + V8 对 `gbk` 标签支持需核实，且 POSIX/Windows 解码分叉增加维护面。
- **方案 ①（`cmd /c chcp 65001`）**：管道输出走进程 ACP（GetACP）而非控制台 CP，chcp 通常**不影响**管道内容 → 可靠性低，不推荐作主方案（仅作为 R2 模板的可选形态保留）。

推荐：oj-bin 落 **R1 + R3②**；oj-module 落 **R2 + 可选 R3③**。两者任一即可打通 Windows 安装链路；叠加以最大化健壮性。

### R2：官方 Windows 模板（oj-module，只读建议）

R1 落地后，POSIX 三条 `tar -tvzf/-xzf` 模板在 Windows 可被 resolver 解析（裸名→`tar.exe`），故**主要复用同模板**即可；另提供 `cmd /c tar -tvzf ${path:archive}` 等兜底形态（R1 使 `cmd` 可解析）。`config.yaml` 模板与 devkit 文档须同步。

### R4：文档对齐（oj-bin，可落 `docs/devkit/`）

- `api-manual.md` §6「命令解析」补：Windows 下裸名按 PATHEXT 补 `.exe`/`.cmd`；`cmd`/`cmd.exe` 可解析。
- `SKILL.md` 删/修「`cmd` 由 resolver 解析为绝对路径」为事实表述 + 补本地代码页输出与 UTF-8 关系；`-tvzf` verbose 与 UTF-8 陷阱。
- `scenarios.md` 场景 34「部署机没有 tar」补 Windows 处置（系统自带 bsdtar + chcp 局限）。
- 同步 `docs/devkit/` 四件 + `cargo xtask build` 归置 `bin/devkit/`（红线要求）。

### R5：Windows CI（oj-module 侧，只读建议）

`ojm market walkthrough --fixture demo-solution@0.1.0`（14 项）纳入 Windows runner，防回归。

## 4. 风险与开放问题

1. R3② 有损解码会静默损坏非 ASCII——安装器 list 解析是否仅依赖行尾路径？（需核实 `extract.ts` 解析正则。）
2. PATHEXT 默认列表取值：应读 env 还是硬编码？建议读 env，缺省回落 Windows 标准列表。
3. `cmd /c chcp 65001` 对**管道**输出的有效性（ACP vs console CP）——若无效则方案①仅作文档说明，不作为修复路径。
4. 落 R1+R3② 后，POSIX 行为必须零回归：`default_resolve_command` 非 windows 分支保持不变，新增 `cfg(windows)` 分支。

## 5. 实施建议（若推进）

- oj-bin：`exec.rs` 的 `default_resolve_command` 增 `cfg(windows)` PATHEXT 分支 + `run_managed` 有损解码分支；`exec_tests.rs` 增 win32 resolver 单测（用注入 PATH + 假 `.exe` 夹具，条件编译或跨平台断言）。
- 文档：`docs/devkit/` 四件对齐 + xtask build。
- oj-module（仅建议）：`config.yaml` 模板 + `extract.ts` spawn 分支 + Windows CI。
- 验收：在 Windows 跑 `ojm market walkthrough` 14 PASS。
