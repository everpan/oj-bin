# npm 分发方案（npmjs 发布编译产物）

**日期**：2026-09-11（同日经双评审修订：arch-reviewer 7 条 + impl-reviewer 8 条，全部处置完毕）
**状态**：已拍板。scoped 包 `@oj-bin/oj`；安装落盘 `$INIT_CWD/bin/`；GitHub Release 双发——npm 拆独立 `publish-npm` job（不阻塞 Release 但失败标红）。
**2026-10-10 修订（鉴权）**：npmjs 账号启用 2FA 后 `NPM_TOKEN` 路径失效，CI 改为 **Trusted Publishing（OIDC）**——仓库不再存长期凭证。本次修订同步：§0 鉴权行、§1 非目标、§4 CI 改动与脚本门禁、§5 手工准备、§6 风险表；包元数据 `repository.url` 随仓库更名订正为 `everpan/oj-bin`（OIDC 强校验项，见 §7）。

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
| CI 鉴权 | **npm Trusted Publishing（OIDC）**：job 加 `permissions: id-token: write`，npm CLI 自行换短时凭证；仓库无长期 token（2026-10-10 由 `NPM_TOKEN` 迁移，见 §7） |
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
- ~~OIDC trusted publishing~~——2026-10-10 已迁移（npmjs 账号启用 2FA，非交互发布只剩「bypass 2FA 的 granular token」与「OIDC」两条路；选 OIDC，见 §7）。

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
  "bin": { "oj": "bin.js" },
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

`bin` 字段让 `pnpm dlx @oj-bin/oj` / `npx @oj-bin/oj` 可直接运行：启动器
`bin.js` 与 postinstall 共用同一份 `platform-arch → triple` 反向表，
`require.resolve('@oj-bin/oj-<triple>/package.json')` 定位子包后 exec 其 `oj`
二进制并转发 argv、传播退出码；子包不可达（如 `--omit=optional`）时回退到与
postinstall 相同的「平台子包未找到」提示并退出 1。发布脚本 `npm-publish.sh`
需把 `bin.js` 一并拷入主包（`files` 已含 `bin.js`）。

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
跑，连警告都打不出，也就没有 `./bin/oj` 落盘。npm/README.md 与 docs 显式写明，
并给出两种兜底：一次性运行走 `pnpm dlx @oj-bin/oj`（启动器 `bin.js` 不依赖
postinstall，直接 exec 子包二进制），或手动重跑
`node node_modules/@oj-bin/oj/postinstall.js`（零依赖 CommonJS 设计正为此）。

## 4. CI 改动（release.yml）

`publish` job（GitHub Release）**不动**。新增独立 job：

草稿模式（workflow_dispatch + draft=true）下本 job 整体跳过（npm 包不可撤回，人工核对 GitHub Release 草稿后重新 dispatch 同 tag、draft=false 即可幂等补发）；tag 推送直发。

```yaml
publish-npm:
  needs: package            # 与 publish 平级，不 needs publish——npm 失败不影响 Release 已先行创建
  runs-on: ubuntu-latest    # 必须是 GitHub 托管 runner（self-hosted 不支持 Trusted Publishing）
  permissions:
    id-token: write         # OIDC 开关：缺它 runner 不注入 ACTIONS_ID_TOKEN_REQUEST_*
    contents: read
  steps:
    - checkout
    - download-artifact (dist-*, merge 到 dist/)
    - actions/setup-node@v4 (node 24)   # ≥22.14.0 / npm ≥11.5.1；不再传 registry-url（见 §7）
    - run: bash scripts/npm-publish.sh "${{ steps.tag.outputs.tag }}"
      # 无 NODE_AUTH_TOKEN：认证由 OIDC 承担
```

**没有 `npm whoami` 步骤**：OIDC 下没有长期身份可问，且官方明确 `npm whoami` 不校验
trusted publishing 权限——凭证正确性改由脚本预检（§7）+ `npm publish` 本身保证。

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
2. ~~生成 Automation granular access token → `NPM_TOKEN` secret~~（2026-10-10 起不再需要，
   见 §7）；账号启用 2FA 后该路径只在 token 勾了 **Bypass 2FA** 时可用，且包级若选了
   「Require 2FA and disallow tokens」则彻底不可用；
3. **为 4 个包各自配 trusted publisher**（per-package，不是 per-scope）——步骤见 §5.1；
4. 首次发布由 CI 直发（`@oj-bin/oj`、`@oj-bin/oj-*` 已核实未被占名，
   2026-09-11 `npm view` 验证 404）。

### 5.1 trusted publisher 配置步骤

四个包各来一遍：`@oj-bin/oj`、`@oj-bin/oj-x86_64-unknown-linux-gnu`、
`@oj-bin/oj-aarch64-apple-darwin`、`@oj-bin/oj-x86_64-pc-windows-msvc`。

**A. 网页（npmjs.com）**——包页面 → **Settings** → *Trusted Publisher* → **GitHub Actions**：

| 字段 | 填什么 |
|---|---|
| Organization or user | `everpan` |
| Repository | `oj-bin`（仓库已由 `only-js` 改名，OIDC 认的是现名） |
| Workflow filename | `release.yml`（只填文件名，带 `.yml`） |
| Environment name | 留空（未用 GitHub Environment） |
| Allowed actions | 勾 **`npm publish`**（`npm stage publish` 默认允许，与本流水线无关） |

保存即可。**npm 保存时不校验**，填错了要等 publish 才炸；配置不可编辑，改错只能删了重建。

**B. CLI（批量，4 个包更快）**——需 npm CLI ≥ 11.15.0，且第一次调用要过一次 2FA
（npm 网站上有「跳过之后 5 分钟的 2FA」选项，配 4 个包绰绰有余；granular token 不管用，
必须账号级 2FA 登录态）：

```bash
npm i -g npm@latest     # 本机 npm ≥ 11.15.0
for p in oj oj-x86_64-unknown-linux-gnu oj-aarch64-apple-darwin oj-x86_64-pc-windows-msvc; do
  npm trust github "@oj-bin/$p" \
    --repo everpan/oj-bin --file release.yml --allow-publish -y
  sleep 2               # 官方建议：防限流
done
npm trust list @oj-bin/oj        # 回看已配的信任关系（revoke 用它的 id）
```

**验证**：配好后推一个 tag（或 `workflow_dispatch` 同 tag、`draft=false`）→ `publish-npm`
job 日志应出现 `published @oj-bin/oj-…@<ver>` + 末尾 `verified … os=… cpu=…`；
`npm view @oj-bin/oj@<ver> --json` 可见新版本，npmjs 包页出现 provenance 徽章
（公共仓库 + 公共包才有）。

**排错**（官方 Troubleshooting）：publish 报 `ENEEDAUTH / Unable to authenticate` ——
按序查：workflow 文件名是否与配置**完全一致**（含 `.yml`、大小写）→ 是否 GitHub 托管
runner（self-hosted 不支持）→ job 是否有 `id-token: write` → 包的 `repository.url`
是否等于 `git+https://github.com/everpan/oj-bin.git`。

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
| registry 传播延迟导致冒烟抖动 | smoke-npm 装前先用 `npm view` 等主包 + 本 triple 子包可见（每包最多 10×20s），再 `npm i`（retry 3×30s）；smoke 是独立 job，失败不阻塞已发布的 Release。**v0.1.59 实测**：只靠 install retry 3×20s 时 Windows 腿连吃 3 个 `ETARGET`（主包 15:54:16Z 才落库，Windows 15:54:03Z 就开装）——ETARGET 是「本 runner 命中的 CDN 节点还没这版」，不是没发出去 |
| ~~NPM_TOKEN 泄漏~~ | 2026-10-10 起仓库无长期凭证（OIDC 短时凭证，见 §7） |
| OIDC 配置漂移（workflow 改名/换仓库/换 self-hosted runner） | 脚本预检先死（§7）；npmjs 侧配置保存后 npm 不校验，改 workflow 文件名必须同步改 trusted publisher |
| trusted publisher 配置「2 天内未首发」自动失效 | 配置与首发放在同一次发布窗口内；失效后删了重建（配置不可编辑） |
| `repository.url` 与 GitHub 仓库不一致（only-js 已改名 oj-bin） | 包元数据同步为 `everpan/oj-bin`；OIDC 强校验项，不一致则 publish 失败 |
| 残留 `NODE_AUTH_TOKEN` 让 npm 在 OIDC 失败时静默回退 | 脚本检测到该变量即 fail（不让它成为隐性兜底） |

## 7. 鉴权：Trusted Publishing（OIDC）（2026-10-10）

**为什么改**：npmjs 账号启用 2FA 后，非交互（CI）发布只剩两条路——
① granular access token 勾 **Bypass 2FA**（长期凭证：要轮换；包级设了
「Require 2FA and disallow tokens」就直接不可用）；② **Trusted Publishing（OIDC）**：
npm CLI 用 GitHub Actions 的 OIDC id-token 现换短时发布凭证，**仓库里没有长期凭证**。
选 ②（官方对 CI/CD 的推荐解，[docs.npmjs.com/trusted-publishers](https://docs.npmjs.com/trusted-publishers)）。

**官方硬约束**（`scripts/npm-publish.sh` §1b 已全部做成预检，先于装配动作执行）：

| 约束 | 落地 |
|---|---|
| npm CLI ≥ 11.5.1 / Node ≥ 22.14.0 | `setup-node` 用 `node-version: '24'`；脚本比较 `npm -v` 与 11.5.1，低即 fail |
| GitHub 托管 runner | `runs-on: ubuntu-latest`（self-hosted 不受支持） |
| job 需 `permissions: id-token: write` | 脚本校验 `ACTIONS_ID_TOKEN_REQUEST_URL/TOKEN` 非空 |
| `package.json` 的 `repository.url` 必须与 GitHub 仓库完全一致 | 随仓库更名订正为 `git+https://github.com/everpan/oj-bin.git` |
| 配置保存后 2 天内须完成首次成功发布 | 与首发同窗口配置；失效即删了重建（配置不可编辑） |
| 不允许隐性回退到长期 token | 脚本检测到 `NODE_AUTH_TOKEN` 非空即 fail |

**两点副作用**：① `npm whoami` 在 OIDC 下无意义（官方：不校验 trusted publishing 权限）→
CI 该步骤删除；② `setup-node` 不再传 `registry-url`——它会往 `.npmrc` 写
`//registry.npmjs.org/:_authToken=${NODE_AUTH_TOKEN}`，job 已无 token，留空 authToken 只会干扰
OIDC 路径（默认 registry 本就是 npmjs）。

**provenance**：GitHub Actions + OIDC 下 npm 自动附带来源声明（公共仓库 + 公共包），
无需 `--provenance`；私有仓库不发 provenance，但发布照常工作。
