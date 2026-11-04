# oj-bin 实施计划：Windows `exec.run` PATHEXT 解析 + 非 UTF-8 输出

> 来源 PRD：oj-module `docs/prd/20261104-oj-windows-exec-resolver-requirement.md`（阻断级）
> 脑暴与评审：`docs/plans/2026-11-04-windows-exec-resolver-brainstorm.md`（脚本 / 架构 / 工程师三路评审）
> 范围边界：**本仓库只改 oj-bin**；oj-module 侧改动一律只读建议，见 `docs/plans/2026-11-04-windows-exec-resolver-handoff.md`。

## 0. 目标与验收

**目标**：Windows 上 `exec.run(["tar", ...])` / `exec.run(["cmd", "/c", ...])` 能解析到绝对路径并 spawn；非 UTF-8 输出（本地代码页 + CRLF）可被缓冲形态消费而不抛错。

**AC**
1. Windows 11 中文、oj≥修复版、`tar` 仅系统自带 bsdtar（PATH 无 Git `usr/bin`）时，`ojm market walkthrough --fixture demo-solution@0.1.0` 14 PASS / 0 FAIL。
2. 负向 5 项（neg-tamper / neg-nosig / neg-badindex / neg-stranger / neg-evil）仍全拦；`template mismatch` / `ambiguous` / `NotCapable` 与白名单 fail-closed 不变。
3. POSIX（macOS/Linux）零回归：现有 exec 测试全绿，`default_resolve_command` 非 windows 路径字节不变。
4. `cargo fmt --check` + `cargo clippy --release --workspace --all-targets -- -D warnings` + `cargo test --release --workspace` 全绿。

## 1. 设计决定（评审收敛）

| 主题 | 决定 | 依据 |
| --- | --- | --- |
| PATHEXT 取值 | 读 env `PATHEXT`；空/缺省回落标准列表（`.COM;.EXE;...;.CPL`） | 架构：与 cmd 一致，避免硬编码漂移 |
| 搜索顺序 | 每目录先裸名 `name`，再逐 ext 拼 `name{ext}`（对齐 CreateProcess） | 工程师：兼容无扩名 shim（PRD #5/#6） |
| 双扩展名 | `name` 小写后缀已匹配任一 ext 时不追加（防 `tar.exe.exe`）；`eq_ignore_ascii_case` | 工程师 |
| 空 PATH 条目 | 保留跳过（`dir.is_empty()`）——防 CWD 隐式劫持 | 架构：红线 |
| 非 UTF-8 | 缓冲形态改 `from_utf8_lossy`，并在结果加 `encoding:"lossy"` 显式标记（**不静默**） | 架构 + 工程师：契约需显式化 |
| `chcp 65001` | **否决**作修复路径（管道走 ACP 非 console CP）；仅文档说明 | 三路一致 |
| 测试 | 抽平台无关纯函数 `search_in_path` / `parse_pathext`，mac/linux 可单测 | 工程师 |

## 2. 工作项（按依赖顺序）

### W1 — R1：resolver PATHEXT 分支（`src/bridge/exec.rs`）

1. 新增纯函数（平台无关，放在 `default_resolve_command` 附近）：
   ```rust
   /// 在给定目录序列中按「裸名 → 逐 PATHEXT 扩展」查找首个存在文件。
   /// dirs 需已剔除空条目；exts 为归一化小写带点扩展名。
   fn search_in_path(name: &str, dirs: &[PathBuf], exts: &[String]) -> Result<PathBuf, String> {
       for dir in dirs {
           let exact = dir.join(name);
           if is_executable_file(&exact) { return Ok(exact); }
           let lower = name.to_ascii_lowercase();
           let has_known_ext = exts.iter().any(|e| lower.ends_with(e.as_str()));
           if !has_known_ext {
               for ext in exts {
                   let cand = dir.join(format!("{name}{ext}"));
                   if is_executable_file(&cand) { return Ok(cand); }
               }
           }
       }
       Err(format!("command '{name}' not found in PATH"))
   }

   /// 原始 PATHEXT（可缺失）→ 归一化扩展名列表：分号拆分、trim、补前导点、小写。
   fn parse_pathext(raw: Option<&str>) -> Vec<String> { /* 空则回落标准列表 */ }
   ```
2. 改写 `default_resolve_command`（exec.rs:965-985）。**非 windows 行为零回归**：非 windows 传空扩展名列表调同一 `search_in_path`（仅精确名探测，与现有循环逐字等价），这样函数全平台存活、无 `dead_code`，且可跨平台单测：
   ```rust
   pub fn default_resolve_command(name: &str) -> Result<PathBuf, String> {
       if name == "oj" { return std::env::current_exe()...; }
       if name.contains('/') || name.contains('\\') { return Err(...); }
       let path = std::env::var_os("PATH").unwrap_or_default();
       let dirs: Vec<PathBuf> = std::env::split_paths(&path)
           .filter(|d| !d.as_os_str().is_empty()).collect();

       #[cfg(windows)]
       let exts = parse_pathext(std::env::var("PATHEXT").ok().as_deref());
       #[cfg(not(windows))]
       let exts: Vec<String> = Vec::new();

       search_in_path(name, &dirs, &exts)
   }
   ```
   注：`parse_pathext` 仅 windows 使用，非 windows 下须 `#[cfg(any(windows, test))]`（或 `#[cfg_attr(not(windows), allow(dead_code))]`）以免 `clippy -D warnings` 报 dead_code。
3. 模块头注释（exec.rs:8-10）补：win32 下裸名按 PATHEXT 补全；resolver 只搜 PATH 目录，绝不搜 CWD。

### W2 — R3②：缓冲形态有损解码（`src/bridge/exec.rs`）

1. `ExecResult`（exec.rs:326-331）加字段，**仅 lossy 时出现**：
   ```rust
   #[serde(skip_serializing_if = "Option::is_none")]
   encoding: Option<&'static str>, // Some("lossy")
   ```
2. `run_managed`（exec.rs:902-907）：
   ```rust
   fn decode_lossy(bytes: Vec<u8>) -> (String, bool) {
       match String::from_utf8(bytes) {
           Ok(s) => (s, false),
           Err(e) => (String::from_utf8_lossy(e.as_bytes()).into_owned(), true),
       }
   }
   // ...
   let (stdout, o_lossy) = decode_lossy(o1);
   let (stderr, e_lossy) = decode_lossy(o2);
   let encoding = (o_lossy || e_lossy).then_some("lossy");
   ```
3. 更新模块文档边界（exec.rs:14-17）：「UTF-8 解码失败」从抛错改为「有损解码 + `encoding:"lossy"` 标记」。

> 注：回调形态本就 `TextDecoder()` 有损（bootstrap.js:310），spawn 形态给原始字节（bootstrap.js:279-306）。本次只对齐**缓冲**形态，不去动另两条。

### W3 — 测试（`src/bridge/exec_tests.rs`）

1. 新增 `search_in_path` / `parse_pathext` 纯函数单测（mac/linux 可跑）：
   - 目录放 `tar.exe` + `cmd.exe` + 无扩名 `shim`（unix 需 `chmod 0o755`）：
     - `tar` → `.../tar.exe`；`tar.exe` → 同（**不得** `tar.exe.exe`）；
     - `shim` → 裸名命中；`nope` → Err；
     - `parse_pathext(Some(".EXE;.Cmd"))` → `[".exe",".cmd"]`；`None`/`""` → 标准列表；无前导点输入补点。
2. 改 S12 缓冲断言（exec_tests.rs:534-552）：`s12_badutf8` / `s12_badutf8_out` 不再抛错，改为断言 `code==0`、`stdout/stderr` 含 `\u{FFFD}`、`encoding=="lossy"`。
3. 回调 / spawn 两条断言（exec_tests.rs:553-579）保持不变。
4. 补 fail-closed 探针：resolver 能解析但模板不匹配 → 仍 `NotCapable`（防「R1 是否绕过门禁」误判）。

### W4 — 文档对齐（R4，红线）

- `docs/devkit/api-manual.md`
  - §6「命令解析（模板首 token）」：补 win32 PATHEXT 规则、`cmd`/`cmd.exe` 可解析、只搜 PATH 不搜 CWD。
  - 「形态 × 非 UTF-8 行为」表：缓冲形态改为**有损 + `encoding:"lossy"`**；回调/流式为既有行为。
- `docs/devkit/SKILL.md`：修正「`cmd` 由 resolver 解析」为事实表述；补本地代码页 / `-tvzf` / UTF-8 陷阱；`chcp` 仅影响控制台不影响管道。
- `docs/devkit/scenarios.md`：场景 34「部署机没有 tar」补 Windows 处置（系统自带 bsdtar、`chcp` 局限）。
- `docs/devkit/README.md`：如需同步版本/能力清单则改。
- 收尾：`cargo xtask build` 归置 `bin/devkit/`，`cargo test --release -p xtask`（devkit 契约用例校验产物与源一致）。

### W5 — CHANGELOG 与版本

- `CHANGELOG.md` 增加一条用户可见变更：Windows `exec.run` 支持 PATHEXT；缓冲形态非 UTF-8 有损解码并带 `encoding:"lossy"`。
- 发布时 bump 根 `Cargo.toml` 版本（遵循本仓库「每次版本更新更新 CHANGELOG」约定；是否 bump 由发布节奏决定）。

### W6 — CI（建议，非阻断）

- 增加 `cargo check --target x86_64-pc-windows-msvc`（或 `-gnu`）以在非 Windows 机编译验证 `cfg(windows)` 分支（需 `rustup target add`）。否则 windows 分支类型/unused 错误要到 Windows runner 才暴露。
- Windows runner 的真实验收须中文 locale（见转交清单 R5）。

## 3. 风险与回滚

| 风险 | 处置 |
| --- | --- |
| R1 破坏 POSIX 解析 | 非 windows 分支原样保留；现有 exec 测试全绿作为门禁 |
| 裸名优先命中无扩名 shim（PATH 前置） | 与 CreateProcess/POSIX 同信任模型；文档写明「只搜 PATH 真实文件、不搜 CWD」 |
| R3② 静默损坏非 ASCII | 强制 `encoding:"lossy"` 标记，下游可检测；install 安全关键路径改用 spawn+精确解码（转交 oj-module） |
| `cfg(windows)` 未编译 | W6 `cargo check --target`；本机无 Windows 时以交叉 check 兜底 |
| 文档与实现漂移 | W4 + xtask 契约测试作为红线门禁 |

## 4. 提交切分建议

1. `fix(exec): Windows PATHEXT-aware command resolution`（W1 + W3 纯函数测试）
2. `fix(exec): lossy decode for non-UTF-8 buffered output`（W2 + W3 S12）
3. `docs(devkit): Windows exec PATHEXT & non-UTF-8 behavior`（W4 + CHANGELOG）

> 遵循仓库约定：改动完成不自动提交，待用户确认；提交信息中文、不加 Co-Authored-By。
