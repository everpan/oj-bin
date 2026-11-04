# 转交清单：oj-module 侧配套改动（只读建议）

> 配套 oj-bin 实施计划：`docs/plans/2026-11-04-windows-exec-resolver-implementation-plan.md`
> 源 PRD：oj-module `docs/prd/20261104-oj-windows-exec-resolver-requirement.md`
> 边界：以下改动位于 **oj-module 仓库**，本仓库（oj-bin）不实施，仅作为交接依据。oj-bin 侧 R1（PATHEXT 解析）+ R3②（缓冲有损解码）落地后，本清单多数项从「阻断」降为「健壮性/正确性增强」。

## H1（R2）Windows 命令模板

**结论：R1 落地后无需强制新增 Windows 专用模板。**
- `buildListCmd` / `buildExtractCmd`（`packages/cli/templates/src/app/api/_platform/_lifecycle/steps/extract.ts:62-69`）现产出 `["tar","-tvzf",file]` / `["tar","-xzf",file,"-C",staging]`；R1 使裸名 `tar` 解析到 `tar.exe`，**POSIX 三条模板在 Windows 可直接复用**。
- `config.yaml`（`packages/cli/templates/config.yaml`）的 `platform.exec._platform.allow` 维持 POSIX 三条即可；如需兜底可加 `cmd /c tar ...` 形态（R1 使 `cmd` 可解析），但非必需。
- 勿改 `-tvzf` 形态（铁律①：verbose listing 是 `validateTarListing` 唯一输入契约）。

## H2（R3③）安装器安全关键路径改用 `exec.spawn` 取原始字节

**问题**：缓冲形态 R3② 为有损解码（U+FFFD），虽然 `..`/`/` 等 ASCII 结构不受影响，但安全校验路径宜拿**精确字节**自行解码，避免本地化字段损坏影响解析。
- `ExecFace`（extract.ts:22-31）已 feature-detect `exec.spawn`；list 步（extract.ts:79）可改为 spawn 收集 `Uint8Array`，按平台解码：
  - Windows：`new TextDecoder("gbk")`（WHATWG `gbk` 映射到 gb18030，V8/Deno 支持；建议再按 BOM/合法性回退 `utf-8`）；
  - POSIX：`new TextDecoder("utf-8")`。
- 解压步（extract.ts:112）不需要读 stdout，可继续走 `exec.run`（有损解码不影响退出码）。
- 与 oj-bin R3② 二者叠加即可：oj-bin 兜底任意 exec 输出，安装器对安全校验路径用精确解码。

## H3（新发现）`tgz-guard` 月份锚点在中文环境失效

**位置**：`packages/cli/templates/src/app/api/_platform/_lifecycle/tgz-guard.ts:70,75-90`
- `MONTH = /^[A-Z]{3}$/i` 不匹配中文 bsdtar 的 `11月` / `月`。
- 影响面（**精确**）：仅**无时刻行**（mtime > 6 个月，verbose 只剩日期）走 `MONTH` 分支；这类行 `findDateGroupStart` 返回 `-1` → `extractTarEntrySize` 返回 0 → 这些条目不计入 REG-CS8 体积闸（潜在 zip-bomb 绕过）。含 `HH:MM` 的常规行走时间锚点，不受影响。
- **修复建议**：`extractTarEntrySize` 改为不依赖月份名——从行尾倒序找「纯数字 size token」（或支持本地化日期 token），并补一条中文 bsdtar listing 回归用例（`... 1234 11月 04  2019 path/`）。
- 与 R3 选择无关：即使正确解码也修不了（正则是英文月份语义），必须单独订正。

## H4（健壮性）`startOj` 的 PATH 顺序

**位置**：`packages/cli/src/oj.ts`（win32 PATH 修正）
- 确保 `C:\Windows\System32`（自包含 bsdtar）排在 `C:\Program Files\Git\usr\bin`（GNU tar，依赖 msys DLL，且把 `C:` 当远程主机需 `--force-local`）**之前**，避免 resolver 选中 GNU tar 后 `-tvzf C:\...` 报 `Cannot connect to C: resolve failed`。
- 若无法保证顺序，可在提取路径对 GNU tar 追加 `--force-local`（现有 ojm 测试已按 `tar --version` 探测 GNU，可复用该探测逻辑；勿对所有 tar 硬编码该 flag）。

## H5（R5）Windows CI 覆盖

**现实性提醒**：`windows-latest` 默认英文 locale（ACP=1252），**复现不了** GBK/中文月份缺陷；14/14 在英文 runner 上是**假覆盖**。
- 真回归需中文 locale runner（region/language 设为 zh-CN，ACP=936，需安装语言包）或专用自托管 runner。
- 建议：R1 resolver 行为可被英文 Windows runner 覆盖（`tar`→`tar.exe`）；编码 / 体积闸 / zip-slip 项须中文环境验收，CI 注明前置条件，勿据此宣称「已防回归」。纳入 `ojm market walkthrough --fixture demo-solution@0.1.0`（14 项）。

## H6 验收与基线

- oj-bin 修复版发布后，按 `framework-verification-playbook.md` 重跑 §7.1–§7.4，更新「最近一次基线」。
- 通过口径：Windows 中文、仅有系统 bsdtar 时 14 PASS / 0 FAIL；负向 5 项仍全拦；POSIX 零回归。
- 演练现场 `d:/ojm-verify/my-books` 与 `packages/cli` 保持逐字节一致，勿保留本地规避补丁。

## 优先级

| 项 | 优先级 | 是否阻断 Windows 安装链路 |
| --- | --- | --- |
| H3 月份锚点 | 高（安全闸静默失效） | 否，但削弱 REG-CS8 |
| H4 PATH 顺序 | 高（GNU tar 解析失败） | 视机器 PATH，可能阻断 |
| H2 spawn 精确解码 | 中（R3② 已可打通） | 否 |
| H1 模板 | 低（复用即可） | 否 |
| H5 CI | 中（防回归） | 否 |
| H6 验收 | 必做 | — |
