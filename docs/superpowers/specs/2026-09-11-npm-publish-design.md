# npm 分发方案（npmjs 发布编译产物）

**日期**：2026-09-11（同日经双评审修订：arch-reviewer 7 条 + impl-reviewer 8 条，全部处置完毕）
**状态**：已拍板。scoped 包 `@oj-bin/oj`；安装落盘 `$INIT_CWD/bin/`；GitHub Release 双发——npm 拆独立 `publish-npm` job（不阻塞 Release 但失败标红）；CI 鉴权用 `NPM_TOKEN` secret。

## 0. 结论速览

| 维度 | 决策 |
|---|---|
| 模式 | **平台子包 + optionalDependencies**（esbuild / biome / swc 同款） |
| 主包 | `@oj-bin/oj`（纯 JS：package.json + postinstall.js + README） |
| 平台子包 | `@oj-bin/oj-<triple>`，triple 与 repo 现有体系同源（`rustc -vV` / xtask host_triple） |
| 按平台下载 | npm 客户端按子包 `os`/`cpu` 字段原生完成，**零自研下载代码** |
| 安装落盘 | postinstall 把子包内容拷到 `$INIT_CWD/bin/`（`oj` + `plugins/<triple>/` + `devkit/`，解包即用布局不变） |
| 支持面 | **npm/pnpm 项目内安装**；`-g` / `--prefix` / `--ignore-scripts` / `--omit=optional` 不支持（postinstall 检测并报明确错误，不静默错装） |
| 发布入口 | release.yml 新增独立 **`publish-npm` job**（`needs: package`，与 `publish` 平级） |
| 失败语义 | GitHub Release 由 `publish` job 先行创建，不受 npm 影响；npm 失败 = workflow 红（不用 continue-on-error——step 级 continue-on-error 下 job 结论仍绿，告警形同虚设），幂等设计支持 Re-run failed jobs 只重跑 npm 段 |
| 版本注入 | npm 包 version = **`${tag#v}`**（npm 不允许前导 `v`） |
| CI 鉴权 | `NODE_AUTH_TOKEN: ${{ secrets.NPM_TOKEN }}`（Automation granular token，权限限 `@oj-bin/*`） |
| 国内可达性 | npmmirror 自动全量镜像 npmjs（含 scoped 包），`registry.npmmirror.com` 零配置可用 |

## 1. 目标与非目标

目标：

1. 项目内 `npm i @oj-bin/oj` 后 `./bin/oj` 可直接运行（含 `plugins/<triple>/`、`devkit/`），体验等同解开 GitHub Release 压缩包。
2. npm 只装当前平台的二进制（三平台产物各 ~48MB gz，不互相白下）。
3. 发布复用现有 release.yml 三平台产物，GitHub Release 流程零改动。
4. 任一渠道失败可见、可独立重跑，版本不错位。

非目标（本次不做）：

- `bin` 字段 shim（`npx oj` / `node_modules/.bin/oj`）——目标是 `./bin/oj`，需要时再加（约 20 行）。同时登记已知缺口：bin shim 本可兜底「postinstall 未执行」场景（pnpm ≥10 / `--ignore-scripts`），当前该缺口仅靠文档覆盖。
- musl 平台包——os/cpu 字段区分不了 gnu/musl，启用前必须先定 libc 策略（见 §6 风险表）；npm-publish.sh 内置防呆硬校验。
- 全局安装（`npm i -g`）——INIT_CWD 指向用户敲命令的随机 cwd，落盘语义不成立；postinstall 检测后明确报错并指向项目内安装或 GitHub Release。
- npmmirror 手工同步——自动镜像，零操作。
- OIDC trusted publishing——本次用 NPM_TOKEN；token 管理负担显现后可再迁移。

## 2. 包结构

### 2.1 仓库内新增（模板，不含产物）

```
npm/
  README.md                     # npmjs 展示页（简短，指回 GitHub repo）
  oj/
    package.json                # 主包模板，version 占位符，CI 注入
    postinstall.js              # 唯一运行时逻辑（零依赖 CommonJS，可裸跑：node node_modules/@oj-bin/oj/postinstall.js）
  platform/
    package.json                # 平台子包模板，name/version/os/cpu 占位，CI 注入
```

### 2.2 平台子包（CI 装配，发布后形态）

每个子包 = 现有 `dist/oj-v<ver>-<triple>.{tar.gz,zip}` **解开后的目录树原样** +
注入的 `package.json`：

```
@oj-bin/oj-<triple>/
  package.json        # name=@oj-bin/oj-<triple>, version, os, cpu
  oj[.exe]
  plugins/<triple>/*.dylib|*.so|*.dll
  devkit/*
```

| triple | npm `os` | npm `cpu` |
|---|---|---|
| `x86_64-unknown-linux-gnu` | `linux` | `x64` |
| `aarch64-apple-darwin` | `darwin` | `arm64` |
| `x86_64-pc-windows-msvc` | `win32` | `x64` |

triple → os/cpu 映射表**故意存两份**：`scripts/npm-publish.sh` 一份（正向，
它按 dist/ 实际产物循环装配），`npm/oj/postinstall.js` 自带一份反向小表
（platform/arch → triple，运行时拿不到 CI 脚本）。各 3 行，文件头互相加一
行交叉引用注释，防未来只改一边。模板里用占位符。

模板约束（防未来炸弹）：platform/package.json **禁止添加 `exports` 字段**——
postinstall 用 `require.resolve('@oj-bin/oj-<triple>/package.json')` 定位子包，
若加 `exports` 而未含 `"./package.json"` 子路径会直接抛
ERR_PACKAGE_PATH_NOT_EXPORTED。模板注释写死此约束。

### 2.3 主包 `@oj-bin/oj` 关键字段

```json
{
  "name": "@oj-bin/oj",
  "version": "<ver>",
  "scripts": { "postinstall": "node postinstall.js" },
  "optionalDependencies": {
    "@oj-bin/oj-x86_64-unknown-linux-gnu": "<ver>",
    "@oj-bin/oj-aarch64-apple-darwin": "<ver>",
    "@oj-bin/oj-x86_64-pc-windows-msvc": "<ver>"
  }
}
```

npm 解析时按各子包 `os`/`cpu` 只安装匹配平台的一个；不匹配的静默跳过
（这正是 optionalDependencies 而非 dependencies 的原因）。注意同一
(os,cpu) 不允许出现两个子包（musl 情形），由 npm-publish.sh 硬校验兜底
（§4）。

## 3. postinstall.js 行为

1. **支持面检测（最先做，不满足即明确报错 + exit 0，不静默错装）**：
   - `npm_config_global === 'true'` → 报错：不支持全局安装，请项目内安装或去 GitHub Release；
   - `npm_config_prefix` 与落盘根不一致（`--prefix` 场景；仅在 npm 标准布局（`up1==@oj-bin && up2==node_modules`）下生效，pnpm/berry 布局不套此启发式）→ 同样报错。
2. `process.platform` + `process.arch` → triple（3 行映射表，同 §2.2）。
3. 落盘根解析顺序：`INIT_CWD`（npm/pnpm/yarn classic 都设）→
   `PROJECT_CWD`（yarn berry 不设 INIT_CWD，设这个）→ 启发式「主包上溯三级（scoped 包多一层：`<root>/node_modules/@oj-bin/oj` → `<root>`）」
   （最后手段，npm 标准布局/workspaces hoisting 下碰巧对，berry PnP 下是错的
   ——但 berry 必设 PROJECT_CWD，走不到这步）。
4. `require.resolve('@oj-bin/oj-<triple>/package.json')` 定位子包根（npm
   hoisting / pnpm 严格布局下都可达，不猜 node_modules 路径）。
5. 拷贝子包内容到 `<落盘根>/bin/`：`oj[.exe]`、`plugins/<triple>/`、`devkit/`。
   **全部走「写临时文件 + `fs.renameSync`」原子替换**：unix 上可覆盖正在
   执行的 `bin/oj`（避免 ETXTBSY 炸掉 `npm i`）；已加载的旧 DLL 先 rename
   成 `.old` 再落新文件（Windows 允许 rename 被加载的 DLL，不允许覆盖写）。
   任何 EBUSY/EPERM 失败 → 醒目警告 + **exit 0**（不炸掉用户的 `npm i`），
   提示停止运行中的 oj 后手动重跑 `node node_modules/@oj-bin/oj/postinstall.js`。
6. unix 下 `chmod 755`；Windows 无需处理。
7. 落盘布局与「`<exe>/plugins/<triple>/` 解包即用」约定一致，插件加载器
   4 级发现路径中的 `<exe>/plugins` 级直接命中。
8. 找不到匹配子包（如 linux-arm64 用户）→ 醒目警告 + exit 0，提示去
   GitHub Release 或反馈加平台。

**已知缺口（文档覆盖，不做代码兜底）**：pnpm ≥10 默认不执行依赖的
postinstall（需消费方 `onlyBuiltDependencies: ["@oj-bin/oj"]`）、
`--ignore-scripts` / `ignore-scripts=true`——这两类场景 postinstall 根本没
跑，连警告都打不出。npm/README.md 与 docs 显式写明，并给出手动兜底命令
`node node_modules/@oj-bin/oj/postinstall.js`（零依赖 CommonJS 设计正为此）。

## 4. CI 改动（release.yml）

`publish` job（GitHub Release）**不动**。新增独立 job：

草稿模式（workflow_dispatch + draft=true）下本 job 整体跳过（npm 包不可撤回，人工核对 GitHub Release 草稿后重新 dispatch 同 tag、draft=false 即可幂等补发）；tag 推送直发。

```yaml
publish-npm:
  needs: package            # 与 publish 平级，不 needs publish——npm 失败不影响 Release 已先行创建
  runs-on: ubuntu-latest
  steps:
    - checkout
    - download-artifact (dist-*, merge 到 dist/)
    - actions/setup-node@v4 (node 22, registry-url: https://registry.npmjs.org)
    - run: bash scripts/npm-publish.sh "${{ steps.tag.outputs.tag }}"
      env: { NODE_AUTH_TOKEN: ${{ secrets.NPM_TOKEN }} }
```

（tag 解析步骤从 `publish` 提取为可复用前置，或 publish-npm 内联同一段
awk——实现时定，语义不变：tag 与 oj/Cargo.toml version 一致性门禁双保险。）

`scripts/npm-publish.sh`（单一真相来源，与 deploy.sh 同风格，本地可跑）：

1. 一致性门禁：`${tag#v}` == oj/Cargo.toml version，不等即 fail；
   **npm version 一律用 `${tag#v}`**（strip 前导 `v`）。
2. **防呆**：按 §2.2 映射表校验 dist/ 产物——同一 (os,cpu) 出现两个 triple
   （未来 musl 与 gnu 并存）→ 立即 fail，把布局歧义变成发布期错误。
3. 对每个 `dist/oj-v<ver>-<triple>.{tar.gz,zip}`：解包 → 包根注入
   platform/package.json → **publish-first 幂等**：

   解包要点：
   - glob 用显式后缀 `dist/oj-v*-*.tar.gz` / `dist/oj-v*-*.zip`，**排除 .sha256**；
   - deploy.sh/deploy.bat 的归档内都包了一层 `oj-v<ver>-<triple>/` 顶层目录，
     tar 用 `--strip-components=1` 剥掉；zip 用 `unzip -q`（ubuntu runner 预装；
     备选 `python3 -m zipfile -e`，对 deploy.bat 的 bsdtar 产出兼容性最好）
     解开后取内层目录——否则 npm 包里多套一层；
   - 装配后子包根必须直接是 `oj[.exe]` / `plugins/` / `devkit/`。
   ```bash
   npm view "$pkg@$ver" version >/dev/null 2>&1 && skip            # 快速路径
   if ! out=$(npm publish --access public 2>&1); then
     npm view "$pkg@$ver" version >/dev/null 2>&1 \
       && echo "already published (registry lag), skip" \
       || { echo "$out"; exit 1; }   # 真失败
   fi
   ```
   （`npm view` 预检命中 CDN 旧缓存可能误判不存在 → publish-first + 失败后
   re-view，兜住传播延迟。）
4. **硬约束：任何子包 publish 真失败 → 立即非零退出，绝不发主包**。
   原因：npm publish 不校验 optionalDependencies 指向的版本存在性；且安装侧
   optional 依赖 404 同样静默跳过——子包缺 + 主包发 = 用户装到静默空壳。
5. 全部子包就绪后：装配主包（注入 version + 按第 3 步实际发布的 triple
   清单生成 optionalDependencies）→ 同 §4.3 幂等 publish。
6. **发布后置信（两道）**：
   a. 元数据断言：逐 triple `npm view @oj-bin/oj-<triple>@<ver> os cpu --json`
      与映射表比对；`npm view ... dist.tarball` 拉回 tgz 断言文件清单
      （win 包必有 `oj.exe`+`*.dll`、mac 必有 `*.dylib`、linux 必有 `*.so`）。
      ——防 sed 注入把 os/cpu 写反、zip 解错层级这类「静默跳过恰好掩盖」的错。
   b. 独立 `smoke-npm` job（`needs: publish-npm`，三 runner 矩阵
      ubuntu/macos/windows）：temp 目录 `npm i @oj-bin/oj@<ver>`
      （retry 3 次消化传播延迟）→ 断言 `./bin/oj --help` 退出码 0。
      Windows postinstall 路径只有真跑才能验证；每次发布多下两份 ~48MB，
      分钟级成本，值。

## 5. 一次性手工准备

1. npmjs.com 注册账号；创建 org **`oj-bin`**（scoped 包的前提，免费，
   public 包不收钱）；
2. 生成 **Automation** 类型 granular access token，权限限 `@oj-bin/*`；
3. repo Settings → Secrets 加 `NPM_TOKEN`；
4. 首次发布由 CI 直发（`@oj-bin/oj`、`@oj-bin/oj-*` 已核实未被占名，
   2026-09-11 `npm view` 验证 404）。

## 6. 风险与缓解

| 风险 | 缓解 |
|---|---|
| ~~包名被占~~（`oj-cli`/`oj` 实已被占） | 已改 scoped `@oj-bin/*`，scope 内名字自己说了算；publish 带 `--access public` |
| npm 失败无人察觉（GitHub/npm 分叉） | 独立 `publish-npm` job，失败即 workflow 红；Re-run failed jobs 幂等重跑 |
| pnpm ≥10 默认不跑依赖 postinstall | 文档写明 `onlyBuiltDependencies`；手动兜底命令；风险接受（bin shim 可根治，登记为非目标） |
| `--ignore-scripts` / `--omit=optional` | 同上，文档 + 手动兜底 |
| `-g` / `--prefix` 装错位置 | postinstall 启动即检测，明确报错 exit 0，不静默错装 |
| 覆盖运行中的二进制/已加载 DLL | 临时文件 + rename 原子替换；失败警告 + exit 0 + 手动重跑指引 |
| npm 版本不可删/改 | 版本一致性门禁前置；publish-npm job 标红告警；出错发 patch 版 |
| `npm view` 预检被 CDN 缓存误导 | publish-first 幂等：失败后 re-view，可见即成功 |
| 子包缺 + 主包发 = 静默空壳 | 硬约束：子包任一真失败即停，不发主包 |
| macOS/Windows 子包装配错（os/cpu 写反、zip 层级错） | 元数据断言 + tgz 文件清单断言 + 三 runner smoke-npm job |
| musl 与 gnu 同 (os,cpu) 不可区分 | npm-publish.sh 硬校验撞车即 fail；启用 musl 前须先定 libc 策略（npm ≥11 `libc` 字段 + postinstall 运行时检测兜底） |
| 子包模板误加 `exports` 字段 | 模板注释写死禁令（`require.resolve('.../package.json')` 依赖它） |
| registry 传播延迟导致冒烟抖动 | retry 3 次；smoke-npm 是独立 job，失败不阻塞已发布的 Release |
| NPM_TOKEN 泄漏 | granular token 限 `@oj-bin/*` + Automation 型（绕 2FA 但无登录权） |
