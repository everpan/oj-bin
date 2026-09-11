# npm 分发方案（npmjs 发布编译产物）

**日期**：2026-09-11
**状态**：已拍板。包名 `oj-cli`；安装落盘 `$INIT_CWD/bin/`；GitHub Release 双发且 npm 失败不阻塞；CI 鉴权用 `NPM_TOKEN` secret。

## 0. 结论速览

| 维度 | 决策 |
|---|---|
| 模式 | **平台子包 + optionalDependencies**（esbuild / biome / swc 同款） |
| 主包 | `oj-cli`（纯 JS：package.json + postinstall.js + README） |
| 平台子包 | `oj-cli-<triple>`，triple 与 repo 现有体系同源（`rustc -vV` / xtask host_triple） |
| 按平台下载 | npm 客户端按子包 `os`/`cpu` 字段原生完成，**零自研下载代码** |
| 安装落盘 | postinstall 把子包内容拷到 `$INIT_CWD/bin/`（`oj` + `plugins/<triple>/` + `devkit/`，解包即用布局不变） |
| 发布入口 | `release.yml` 的 `publish` job 追加 npm 段，与 GitHub Release **双发** |
| 失败语义 | npm 段 `continue-on-error: true`，不阻塞 GitHub Release；全部步骤幂等可重跑 |
| CI 鉴权 | `NODE_AUTH_TOKEN: ${{ secrets.NPM_TOKEN }}`（Automation granular token） |
| 国内可达性 | npmmirror 自动全量镜像 npmjs，`registry.npmmirror.com` 零配置可用 |

## 1. 目标与非目标

目标：

1. `npm i oj-cli` 后 `./bin/oj` 可直接运行（含 `plugins/<triple>/`、`devkit/`），体验等同解开 GitHub Release 压缩包。
2. npm 只装当前平台的二进制（三平台产物各 ~48MB gz，不互相白下）。
3. 发布复用现有 release.yml 三平台产物，GitHub Release 流程零改动。

非目标（本次不做）：

- `bin` 字段 shim（`npx oj` / `node_modules/.bin/oj`）——目标是 `./bin/oj`，需要时再加（约 20 行）。
- musl 平台包——release.yml 中 musl 矩阵行本就是注释态，npm 侧同步按需。
- npmmirror 手工同步——自动镜像，零操作。
- OIDC trusted publishing——本次用 NPM_TOKEN；token 管理负担显现后可再迁移。

## 2. 包结构

### 2.1 仓库内新增（模板，不含产物）

```
npm/
  README.md                     # npmjs 展示页（简短，指回 GitHub repo）
  oj-cli/
    package.json                # 主包模板，version 占位符，CI 注入
    postinstall.js              # 唯一运行时逻辑（~50 行，零依赖，CommonJS）
  platform/
    package.json                # 平台子包模板，name/version/os/cpu 占位，CI 注入
```

### 2.2 平台子包（CI 装配，发布后形态）

每个子包 = 现有 `dist/oj-v<ver>-<triple>.{tar.gz,zip}` **解开后的目录树原样** +
注入的 `package.json`：

```
oj-cli-<triple>/
  package.json        # name=oj-cli-<triple>, version, os, cpu
  oj[.exe]
  plugins/<triple>/*.dylib|*.so|*.dll
  devkit/*
```

| triple | npm `os` | npm `cpu` |
|---|---|---|
| `x86_64-unknown-linux-gnu` | `linux` | `x64` |
| `aarch64-apple-darwin` | `darwin` | `arm64` |
| `x86_64-pc-windows-msvc` | `win32` | `x64` |

后续加平台 = 矩阵加一行 + 此映射加一行，主包模板不动（optionalDependencies
由 CI 按实际发布的子包清单生成，见 §3）。

### 2.3 主包 `oj-cli` 关键字段

```json
{
  "name": "oj-cli",
  "version": "<ver>",
  "scripts": { "postinstall": "node postinstall.js" },
  "optionalDependencies": {
    "oj-cli-x86_64-unknown-linux-gnu": "<ver>",
    "oj-cli-aarch64-apple-darwin": "<ver>",
    "oj-cli-x86_64-pc-windows-msvc": "<ver>"
  }
}
```

npm 解析时按各子包 `os`/`cpu` 只安装匹配平台的一个；不匹配的跳过不报错
（这正是 optionalDependencies 而非 dependencies 的原因——os/cpu 不满足时
npm 对 optional 依赖静默跳过）。

## 3. postinstall.js 行为

1. `process.platform` + `process.arch` → triple（3 行映射表，同 §2.2 表）；
2. `require.resolve('oj-cli-<triple>/package.json')` 定位子包根（npm 嵌套/
   提升布局下都可靠，不猜 node_modules 路径）；
3. 子包根内容拷到 `$INIT_CWD/bin/`：`oj[.exe]`、`plugins/<triple>/`、
   `devkit/`。`INIT_CWD` 是 npm 设置的用户执行 `npm i` 的目录；缺失时回落
   主包上溯两级（`node_modules/oj-cli` → 项目根）的启发式；
4. unix 下 `chmod 755`；Windows 无需处理；
5. 落盘布局与「`<exe>/plugins/<triple>/` 解包即用」约定一致，插件加载器
   4 级发现路径中的 `<exe>/plugins` 级直接命中；
6. 幂等：同版本重装直接覆盖；
7. 找不到匹配子包（如 linux-arm64 用户）→ 打印明显警告 + **exit 0**
   （不炸掉用户的 `npm i`），提示去 GitHub Release 或反馈加平台。

## 4. CI 改动（release.yml）

`publish` job 在现有 `gh release create` 步骤**之后**追加，同一 job 内
（`dist/` 三平台产物已 download-artifact 合并好）：

```yaml
- uses: actions/setup-node@v4
  with:
    node-version: '22'
    registry-url: 'https://registry.npmjs.org'

- name: publish npm packages
  continue-on-error: true
  env:
    NODE_AUTH_TOKEN: ${{ secrets.NPM_TOKEN }}
  run: bash scripts/npm-publish.sh "${tag}"
```

`scripts/npm-publish.sh`（单一真相来源，与 deploy.sh 同风格，本地可跑）：

1. 校验 `tag` 与 `oj/Cargo.toml` version 一致（复用 release.yml 已有门禁语义，
   双保险）；
2. 对每个 `dist/oj-v<ver>-<triple>.{tar.gz,zip}`：解包 → 包根注入
   `platform/package.json`（sed 注入 name/version/os/cpu）→
   `npm view oj-cli-<triple>@<ver>` 判存在，存在则跳过（幂等），否则
   `npm publish --access public`；
3. 全部子包就绪后：装配主包（注入 version + 按第 2 步实际发布的 triple
   清单生成 optionalDependencies）→ 同样幂等 publish；
4. **冒烟**：temp 目录 `npm i oj-cli@<ver>`（retry 3 次消化 registry 传播
   延迟）→ 断言 `./bin/oj` 存在且 `--help` 退出码 0。冒烟失败令脚本非零
   退出（workflow 标红告警），但因 `continue-on-error` 不影响 GitHub
   Release——npm 版本不可删，只能再发 patch，冒烟定位在发布后仅作告警。

矩阵与 `.github/workflows/release.yml` 现有三行保持一致；musl 行启用时
npm-publish.sh 无需改动（按 dist/ 实际产物循环）。

## 5. 一次性手工准备

1. npmjs.com 注册账号；生成 **Automation** 类型 granular access token；
2. repo Settings → Secrets 加 `NPM_TOKEN`；
3. 首次发布由 CI 直发（`oj-cli` / `oj-cli-*` 未被占名即可）；发完后在
   npmjs 后台把 token 权限收紧到这四个包。

## 6. 风险与缓解

| 风险 | 缓解 |
|---|---|
| 包名被占 | 首发前 `npm view oj-cli` 确认；被占则换名（改模板一处） |
| npm 版本不可删/改 | 版本一致性门禁前置；冒烟告警；出错发 patch 版 |
| registry 传播延迟导致冒烟抖动 | retry 3 次 + 冒烟不阻塞 release |
| 不支持平台用户装了个空壳 | postinstall 明确警告 + 指向 GitHub Release |
| NPM_TOKEN 泄漏 | granular token 限 `oj-cli*` 包 + Automation 型（绕 2FA 但无登录权） |
