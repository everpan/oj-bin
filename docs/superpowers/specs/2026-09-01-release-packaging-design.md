# 发行打包方案（脑暴 + 推荐组合）

**日期**：2026-09-01
**状态**：待拍板。§5 汇总 D1~D9 决策点；其余章节按「推荐组合」展开，**决策未定前这只是推荐组合**。

## 0. 结论速览（推荐组合）

| 维度 | 推荐 |
|---|---|
| 打包入口 | 每平台一个脚本：`scripts/deploy.sh`（unix）+ `scripts/deploy.bat`（Windows） |
| Windows 脚本形态 | **.bat（cmd.exe）**，不引入 .ps1 |
| Windows 归档 | `tar.exe -a -c -f <pkg>.zip <pkg>`（Win10+/Server 2016+ 内置 bsdtar，零外部依赖） |
| Windows 校验和 | `certutil -hashfile <f> SHA256`（系统自带） |
| 归档格式 | unix → `.tar.gz`，Windows → `.zip`（tar.gz 要保留 `oj` 的 `+x` 位；Windows 靠 `.exe` 判定可执行） |
| 包命名 | `oj-v<version>-<host-triple>`，triple 取自 `rustc -vV` |
| 发布 | `gh release create`（第一方 CLI，不引第三方 action），tag 推送直发 / 手工触发默认草稿 |
| 版本一致性 | release workflow 里校验 tag 与 `oj/Cargo.toml` 的 version 一致，不一致即失败 |
| bat 编码 | **bat 文件保持纯 ASCII**（中文注释在 GBK 代码页下会被重新解释，轻则乱码重则语法错） |

## 1. 目标与非目标

目标：

1. `scripts/deploy.sh` 在 macOS + Linux 上可直接编译打包。
2. 包路径/命名带**操作系统与版本**——现在三平台产物同名，落在一起互相覆盖。
3. 能通过 GitHub 编译并发布 Release。
4. Windows 侧用 **bat**，不用 PowerShell。

非目标（本次不做）：

- 安装包/安装器（msi、pkg、deb、Homebrew formula、winget）。
- 代码签名（Windows Authenticode / macOS notarize）——需要证书，且 macOS 公证要求签名，是独立议题。
- 产物证明（SLSA attestation / `gh attestation`）。
- 交叉编译（arm linux、macos x86_64）——矩阵按 runner 原生架构，交叉是独立议题。

## 2. 现状

| 事实 | 位置 |
|---|---|
| 包名 `oj-v<version>`，无平台标识 | 改动前 `scripts/deploy.sh:22`、`scripts/deploy.sh:67` |
| 版本靠 `grep -m1 '^version =' oj/Cargo.toml \| cut -d'"' -f2` | 改动前 `scripts/deploy.sh:20` |
| triple 用 `ls -d bin/plugins/* \| head -1` 猜（残留旧平台目录时会取错） | 改动前 `scripts/deploy.sh:39` |
| 平台 triple 的**权威来源**是 `rustc -vV` 的 `host:` 行 | `tools/xtask/src/main.rs:41-52` |
| 插件落 `bin/plugins/<host-triple>/`，与加载器发现路径同形 | `tools/xtask/src/main.rs:137-138` |
| `cargo xtask build` 一次产出 oj + 全部插件 + devkit → `bin/` | `tools/xtask/src/main.rs:220-226` |
| 仅有的 workflow 是 `plugin-matrix.yml`，手工触发、只上传 artifact、不发 Release | `.github/workflows/plugin-matrix.yml:8` |
| Windows 需 MSVC + NMake 生成器 + 移除 Git 的 `link.exe`（rdkafka-sys 经 cmake 编 librdkafka） | `.github/workflows/plugin-matrix.yml:36-58` |
| 发行包布局的权威文档随包发布 | `docs/devkit/api-manual.md:1057-1074` |
| `oj` 依赖里已有 `tar` + `flate2`（unix 侧打包无需新依赖） | `oj/Cargo.toml:16-17` |

## 3. 设计约束（红线）

- **禁止 debug 构建**：所有入口一律 `--release`（`CLAUDE.md` 命令段）。
- **包内 `plugins/<triple>/` 目录名不可省**：插件加载器按 `<exe>/plugins/<triple>/` 发现，解包必须原地可用，不能让用户手工改目录名。
- **unix 归档必须保留 `oj` 的可执行位**（tar.gz 天然保留，zip 不保留——但 Windows 侧靠 `.exe` 扩展名，无影响）。
- **脚本不能引入需预装的第三方工具**（7z、jq、GNU coreutils 等）：本地开发者与 CI runner 环境不一致，装依赖即劝退。

## 4. 方案空间

### 4.1 打包逻辑放哪：脚本 vs xtask

| 方案 | 评价 |
|---|---|
| **A. 每平台脚本：sh + bat**（推荐） | 现状路线的自然延伸；脚本短、可读、可手工单步执行。代价：组装逻辑在 sh/bat 两处重复，改一处要记得改另一处 |
| B. 下沉进 xtask（新增 `cargo xtask dist`） | 一份 Rust 逻辑跨平台一致，彻底绕开 bat 的语法坑。代价：Windows 的 zip 需要新增 `zip` crate 依赖（现在只有 `tar`+`flate2`）；xtask 语义上是开发工具，兼做发行工具要界定职责 |
| C. 混合：xtask 只管构建归置，脚本管组装归档 | 实际就是 A 的现状 |

**推荐 A**：用户已明确要 bat，A 是唯一自洽的形态。为缓解「两处重复」，包命名/校验逻辑写成**同一套步骤的逐条对照**（sh 与 bat 的章节对齐、注释互指），并在 CI 里强制三平台都产出，任一平台命名漂移都会被 Release 产物名暴露。

### 4.2 Windows 脚本形态

| 方案 | 评价 |
|---|---|
| **A. `.bat`（cmd.exe）**（用户指定，推荐） | 零运行时依赖，任何 Windows 都能双击跑。代价：cmd.exe 语法贫瘠（延迟变量展开、`errorlevel`、无原生 zip/sha256），且**非 ASCII 会踩代码页坑** |
| B. `.ps1`（PowerShell） | 语法现代、`Compress-Archive`/`Get-FileHash` 现成。代价：执行策略（ExecutionPolicy）常挡路，需 `-ExecutionPolicy Bypass` |
| C. 两者都留 | 覆盖最全，但两份 Windows 逻辑必然漂移 |

**推荐 A**。

### 4.3 Windows 下 zip 怎么生成（bat 的硬伤）

cmd.exe 没有原生压缩命令。候选：

| 方案 | 评价 |
|---|---|
| **A. `tar.exe -a -c -f <pkg>.zip <pkg>`**（推荐） | Win10 17063+ / Server 2016+ 内置 bsdtar（libarchive），`-a` 按扩展名自动选格式。零依赖、单行。风险：zip 条目的分隔符/编码需真机验证一次 |
| B. `powershell -NoProfile -Command "Compress-Archive …"`（bat 里调一行 PS） | 结果最可预期；但违背「不用 ps」的初衷（虽只是内部一行），且仍受执行策略影响 |
| C. 依赖 7z / 7za | 功能强，但要求预装，违反 §3 |
| D. 全平台统一 `.tar.gz`（不打 zip） | 彻底绕开问题，一份逻辑；但 Windows 用户拿到 `.tar.gz` 体验差（需 7-Zip/WinRAR 或 Win10 tar） |

**推荐 A**，并在 §9 列出真机验证项；若 A 验证不过，退到 B（改动局限在一个代码块）。

### 4.4 Windows 下 sha256 怎么算

| 方案 | 评价 |
|---|---|
| **A. `certutil -hashfile <f> SHA256`**（推荐） | 系统自带（Vista+）。输出三行、哈希在第二行，需 `for /f "skip=1"` 取 |
| B. `powershell Get-FileHash` | 同上，违背初衷 |
| C. 不产出校验文件 | 省事，但下载侧无法验完整性 |

**推荐 A**。

### 4.5 归档格式

| 方案 | 评价 |
|---|---|
| **A. unix → tar.gz，Windows → zip**（推荐） | 符合各平台习惯；tar.gz 保留 `+x` 位与 unix 权限语义 |
| B. 全平台 zip | 一份逻辑，但 zip 不保留 unix 权限位，unix 用户解包后 `oj` 不可执行、需 `chmod +x`——发行包不该有这一步 |
| C. 全平台 tar.gz | 见 4.3-D |

**推荐 A**。

### 4.6 包命名中的平台标识

| 方案 | 产物示例 | 评价 |
|---|---|---|
| **A. 完整 host triple**（推荐） | `oj-v0.1.0-aarch64-apple-darwin.tar.gz` | 与 `bin/plugins/<triple>/`、加载器发现路径、CI artifact 名同形；`rustc -vV` 单点取值，零映射表。musl 变体天然可区分 |
| B. 目录分层 `dist/<os>/<version>/` | `dist/macos-arm64/oj-v0.1.0.tar.gz` | 目录可浏览，但需维护 os/arch 映射表，且与插件目录的 triple 不同形，两套命名会漂移 |
| C. 友好名 | `oj-v0.1.0-macos-arm64.tar.gz` | 最易读，同样要映射表，且 musl 无法区分 |

**推荐 A**。

### 4.7 构建产物处理

| 项 | 建议 |
|---|---|
| 符号表 | 建议**暂不动**。`[profile.release]` 当前未开 debug（`Cargo.toml` 根），二进制已是 stripped 状态；加 `strip = "symbols"` 会改全局 profile，影响 plugin-matrix 的既有产物，属独立议题 |
| devkit | 随包发布（现状如此，`docs/devkit/api-manual.md` 是包内权威文档，必须同步更新） |
| 版本号 | 单源 = `oj/Cargo.toml`，脚本读取；不反向由 tag 写入 |

### 4.8 版本与 tag 的一致性

release workflow 应当**校验**：tag 名（去掉前导 `v`）== `oj/Cargo.toml` 的 version。否则手工触发时手滑填错 tag，会发出「tag v0.2.0 里装着 0.1.0 的包」。校验失败即终止，不产出 Release。

### 4.9 GitHub 发布形态

| 维度 | 推荐 |
|---|---|
| 触发 | `push: tags: v*`（直发正式版）+ `workflow_dispatch`（带 `tag` 与 `draft` 输入，默认 `draft=true`） |
| 发布动作 | `gh release create`（runner 预装、第一方），**不引第三方 action**（如 `softprops/action-gh-release`），减少供应链面 |
| notes | `--generate-notes`（由 commit 生成） |
| 校验文件 | 随包一起上传（每包一个 `.sha256`） |
| 矩阵 | 与 `plugin-matrix.yml` 同三行；musl / macos-13(x86_64) 行按需启用 |
| 缓存 | 暂不引 `Swatinem/rust-cache` 等第三方 action；V8 与依赖编译慢属已知成本，需要时再议 |

## 5. 决策点汇总（待拍板）

| # | 决策 | 推荐 | 影响面 |
|---|---|---|---|
| D1 | 打包逻辑归属：脚本 / 下沉 xtask | 脚本（sh + bat） | 维护成本 vs 依赖新增 |
| D2 | Windows 脚本形态 | **bat**（用户已定） | — |
| D3 | Windows zip 生成 | `tar.exe -a` | 若验证不过则改用内嵌一行 PS |
| D4 | Windows sha256 | `certutil -hashfile` | — |
| D5 | 归档格式 | unix tar.gz / Windows zip | unix 侧若有平台惯例要求可改 |
| D6 | 包命名平台标识 | 完整 host triple | 可读性 vs 零映射表 |
| D7 | 是否做版本/tag 一致性校验 | 做（不一致即失败） | 多一步 CI 检查 |
| D8 | 发布触发与 draft 策略 | tag 直发 + 手工默认草稿 | 自动化程度 vs 发布不可逆 |
| D9 | 是否 strip 符号 / 引 cargo 缓存 | 暂不做 | 体积与 CI 时长 |

## 6. 推荐组合下的产物形态

```
dist/
├── oj-v0.1.0-aarch64-apple-darwin.tar.gz          # macOS (Apple Silicon)
├── oj-v0.1.0-aarch64-apple-darwin.tar.gz.sha256
├── oj-v0.1.0-x86_64-unknown-linux-gnu.tar.gz      # Linux (glibc)
├── oj-v0.1.0-x86_64-unknown-linux-gnu.tar.gz.sha256
├── oj-v0.1.0-x86_64-pc-windows-msvc.zip           # Windows
└── oj-v0.1.0-x86_64-pc-windows-msvc.zip.sha256
```

包内布局（解包后，根路径 = 包名）：

```
oj-v0.1.0-aarch64-apple-darwin/
├── oj                          # 主程序（Windows 上为 oj.exe）
├── plugins/aarch64-apple-darwin/
│   ├── liboj-es.dylib
│   ├── liboj-db-mysql.dylib
│   └── …
└── devkit/
    ├── api-manual.md
    ├── SKILL.md
    └── global.d.ts
```

注意 `plugins/` 下**仍带 triple 目录**（不带版本）。这不是冗余——加载器按 `<exe>/plugins/<triple>/` 发现，解包后必须原地可用。

## 7. bat 的已知坑与规避

1. **非 ASCII**：bat 按控制台代码页解释字节。zh-CN Windows 默认 936(GBK)，UTF-8 存的中文注释会乱码，且 `findstr` 匹配中文不可靠 → **bat 全 ASCII**（注释用英文），本方案已在脚本头部写明原因。
2. **延迟变量展开**：在 `for` / `if` 块内读写同一变量必须 `setlocal EnableDelayedExpansion` + `!VAR!`，否则读到的是块入口时的值 → 脚本开头统一开。
3. **错误码**：`if errorlevel 1` 是「≥1」而非「==1」；不要用 `%ERRORLEVEL%` 的延迟展开陷阱 → 统一用 `if errorlevel 1 ( … & exit /b 1 )`。
4. **`for /f` 里的管道**：`rustc -vV | findstr …` 在 `for /f` 的命令串里必须写成 `^|`。
5. **路径空格**：所有路径一律 `set "VAR=…"`（等号后引号包住整串）并用引号引用，避免尾随空格与分词。
6. **`xcopy` 退出码**：0 成功、1 无文件、2 用户中止、4/5 错误 → 判 `if errorlevel 2` 才失败。
7. **`certutil` 输出**：三行，哈希在第二行 → `for /f "skip=1"`。
8. **`findstr /b /c:"version ="`**：必须带 `/c:`，否则 `findstr` 把空格当分隔符做「OR」匹配。

## 8. 交付物清单

| 文件 | 动作 |
|---|---|
| `scripts/deploy.sh` | 改：triple 检测、包名带版本+triple、可移植性修正、sha256 |
| `scripts/deploy.bat` | 新增：Windows 等价物（bat，纯 ASCII），产物 `.zip` |
| `.github/workflows/release.yml` | 新增：3 平台矩阵 → 复用脚本 → 版本/tag 校验 → 发布 Release |
| `docs/devkit/api-manual.md:1057-1074` | 改：发行包布局与命令同步（此文件随包发布，必须准确） |
| `docs/devkit/README.md:4` | 改：包名示例同步 |

`bin/devkit/` 由 `cargo xtask build` 从 `docs/devkit/` 拷贝（`.gitignore:9`），改 `docs/` 即可。

## 9. 验证计划

macOS/Linux（本机可验）：

```bash
bash scripts/deploy.sh
ls -1 dist/
tar -tzf dist/oj-v*-aarch64-apple-darwin.tar.gz | head -5   # 根路径带版本+triple
cat dist/*.sha256
```

Windows（**本机无 Windows，需真机或 CI 首跑验证**）：

```bat
scripts\deploy.bat
dir dist
tar -tf dist\oj-v*-x86_64-pc-windows-msvc.zip | more        % 确认根路径与 plugins\<triple>\ 层级
```

Windows 侧待验证的高风险项：

1. `tar.exe -a -c -f x.zip <dir>` 产出的确实是 zip，且条目用 `/` 分隔（而非 `\`）。
2. `certutil -hashfile` 的第二行取值在 runner 语言环境下稳定。
3. `findstr /b /c:"version ="` 能命中 `oj\Cargo.toml` 的包版本行。
4. GHA runner 上 `cargo xtask build` 在 bat 里调用时，`cargo` alias 解析正常（需工作区内执行，脚本已 `pushd`）。
