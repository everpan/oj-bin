# 全分支终审 — npm 分发特性（7477450..9f83e25，8 commits）

**日期**：2026-09-11
**结论**：**可合入**。无 must-fix 发现；ledger 全部 deferred minors 逐条 triage 后均判「可留」；
新增 3 条 minor（见 §5）。两套测试本地实跑通过（postinstall 6/6 PASS、npm-publish.test.sh
case1/2/3 ALL OK），release.yml 结构断言通过（jobs: lint, package, publish, publish-npm,
smoke-npm；publish-npm 无 continue-on-error；if 条件正确）。

## 1. spec §0 决策落地核对（逐项）

| spec 决策 | 落地 | 位置 |
|---|---|---|
| 平台子包 + optionalDependencies | ✅ | npm/oj/package.json:10-14 |
| 主包纯 JS（package.json + postinstall + README） | ✅ | files 字段仅这两项 |
| triple 与 repo 体系同源 | ✅ | os_cpu_of() 三行，与 deploy.sh/bat 产物后缀一致 |
| os/cpu 原生按平台下载，零自研下载 | ✅ | platform/package.json os/cpu 占位 + sed 注入 |
| 落盘 $INIT_CWD/bin（oj + plugins/<triple>/ + devkit/） | ✅ | postinstall.js:107-125，happy path 测试断言三者 |
| 支持面：-g / --prefix / --omit=optional 检测报错不静默错装 | ✅ | postinstall.js:25-59（exit 0 语义，见 §4 注） |
| 独立 publish-npm job（needs: package，与 publish 平级） | ✅ | release.yml:260-262；publish job 一行未动 |
| 失败 = workflow 红（不用 continue-on-error） | ✅ | 结构断言确认无 continue-on-error |
| version = ${tag#v} | ✅ | npm-publish.sh:11 + resolve tag 门禁双保险 |
| NODE_AUTH_TOKEN: secrets.NPM_TOKEN | ✅ | 仅 whoami + publish 两步注入，面最小 |
| 草稿模式跳过 + 幂等补发（用户拍板） | ✅ | release.yml:265 `if: push \|\| inputs.draft != 'true'` |

## 2. 跨文件契约核验

- **模板占位符 ↔ sed**：`__VERSION__`/`__TRIPLE__`/`__OS__`/`__CPU__` 四个占位符与
  npm-publish.sh:95-97 的 sed 一一对应；主包仅 `__VERSION__`（npm-publish.sh:122）。✅
- **TRIPLES 反向表 ↔ os_cpu_of 正向表**：三条映射逐字互逆
  （linux-x64 / darwin-arm64 / win32-x64 ↔ linux x64 / darwin arm64 / win32 x64），
  且与 node 的 process.platform/arch 命名一致（win32 而非 windows）。两边文件头均有
  交叉引用注释。✅
- **主包 optionalDependencies ↔ 脚本校验**：模板恰三个子包（grep 实抽验证），脚本
  §7 双向校验（dist→模板 缺即死；模板→dist 真发布时缺即死，DRY_RUN 降级为 skip 日志
  ——这是单 triple fixture 自检能跑通的必要设计）。✅
- **workflow ↔ 脚本 CLI/env**：`bash scripts/npm-publish.sh "<tag>"` 单参数契约一致；
  DIST_DIR 默认 $PWD/dist 与 download-artifact（pattern dist-*，merge-multiple → dist/）
  吻合；NODE_AUTH_TOKEN 经 setup-node registry-url 写入 .npmrc，whoami 先行 fail-fast。✅
- **产物布局假设 ↔ deploy 脚本**：deploy.sh:102（tar 包顶层 `oj-v<ver>-<triple>/`）与
  deploy.bat:159（bsdtar 打 %PKG% 目录）均核实，`--strip-components=1` / zip 内层
  `mv` 的前提成立；装配后三道布局断言（oj 二进制 / plugins/<triple>/ /
  devkit/api-manual.md）兜底 strip 失效。✅

## 3. ledger deferred minors 逐条 triage

| 条目 | 裁决 | 理由 |
|---|---|---|
| T2-M1 installFile 换原子失败无回滚 | **留** | 外层 catch → bail(exit 0) + 手动重跑指引；重跑即修复，无数据损坏面 |
| T2-M2 chmod 未按平台门控 | **留** | Windows 上 chmodSync 0o755 无害 |
| T2-M3 空子包/拷贝失败分支无测试 | **留** | 分支本身只是 bail + exit 0，逻辑直白；smoke-npm 端到端兜真路径 |
| T2-M4 测试 env 仅透传 PATH | **留** | 测试只在 ubuntu(lint)/macOS(本地) 跑；Windows 开发机撞上再补 SystemRoot |
| T3 glob 未锚定版本（残留旧归档误报） | **留** | 已验证是 fail-safe：旧版本归档经 `${base#oj-v${VERSION}-}` 剥离失败 → 完整串进 os_cpu_of → 「未知 triple」硬失败，不会错发；仅报错文案不够直白。CI dist/ 全新，碰不到 |
| T3 同 triple tar.gz+zip 并存误报撞车 | **留** | 现矩阵 linux/mac 出 tar.gz、windows 出 zip，并存不可能；真出现时硬失败方向正确 |
| T3 STAGE_DIR 无 trap 清理 | **留** | CI runner 一次性；本地留 /tmp 碎屑无危害 |
| T3 zip 分支/dotfile/撞车门禁无测试 | **留**（记为 §5 残留风险 R1） | zip 分支只在真实 Windows 产物首发时执行，失败形态是布局断言硬死，不会发出坏包 |
| T3 npm view 失败静默退出 | **留**（已核实非静默） | 预检失败只是落入 publish 路径（正确）；§8 meta 三次 retry 后空 → os 断言带 ::error:: 非零退出；`tb=$(npm view ...)` 失败在 set -e 下直接死。无静默路径 |
| T4-nit-1 门禁文案少尾巴 | **留** | 纯文案 |

## 4. 安全与发布红线

- **token 使用面**：NODE_AUTH_TOKEN 仅 whoami + publish 两个 step；脚本不回显 token，
  npm publish 失败输出不含凭证。granular Automation token 限 @oj-bin/*（前置手工项）。✅
- **不可撤回发布的前置门禁**（链条完整，任一环节假坏都是 fail-close）：
  版本一致性（workflow resolve tag + 脚本内复核，双保险）→ 未知 triple 门禁 →
  (os,cpu) 撞车门禁 → 模板↔产物双向校验 → 子包先发的硬约束（任一真失败即死，
  绝不发主包 = 不会产出静默空壳）→ 发布后元数据断言（os/cpu 写反会被抓）→
  tarball 文件清单断言（解错层级会被抓）→ 三 runner 真机 smoke-npm。✅
- **静默失败路径**：脚本侧全部错误路径 `::error::` + 非零退出（含 §8 全部断言），
  无静默；SIGPIPE 假失败已在 10fc91e 修掉。postinstall 的 exit-0-on-failure 是 spec
  拍板的语义（不炸用户 npm i），配套 smoke-npm 的 `ls bin/` + `[[ -f bin/oj.exe ]]`
  检查能把「装成功但 bin/ 缺席」兜成红。✅
- **一个已知折衷（非新发现）**：npm ≥7 对 exit 0 的生命周期脚本默认不回显其输出，
  postinstall 的「醒目警告」在用户机器上实际不可见——「不静默错装」在用户侧降级为
  「装完没有 bin/」，靠 README 文档与手动兜底命令覆盖。spec §3 已接受此语义，不重开。

## 5. 新增 minor（可留，不阻塞合入）

1. `npm/oj/package.json` / `npm/platform/package.json` 无 `license` 字段；repo 根亦无
   LICENSE 文件（npm-publish.sh:99/124 的 `[[ -f LICENSE ]] && cp` 当前是死代码）。
   npmjs 页面会显示 license: none。随 repo 补 LICENSE 时一起加字段即可。
2. npm workspaces 子目录内 `npm i` 会命中 INIT_CWD 哨兵而 bail（bin/ 本应落 workspace
   根，被跳过），npm/README.md 未提 workspaces 场景；bail 文案给出的手动重跑命令
   （无 INIT_CWD → 上溯三级 = workspace 根）恰好是正确解，可留。
3. publish-npm job 未做 job 级 `permissions:` 收紧（继承顶层 contents: write，npm
   发布不需要 GitHub 写权限）。与 publish job 平级后影响面可控，可留。

## 6. 残留风险（真实发布路径只能靠代码审查的部分）

- **R1**：Windows zip 装配分支（`python3 -m zipfile` + 内层目录 mv）与 win 子包发布
  在首个真实 tag 才首次执行。失败形态全部是 fail-fast（布局断言 / publish 失败即死），
  不会发出坏包；smoke-npm windows runner 做最终端到端验证。风险可控。
- **R2**：org 创建、scoped 包首发 `--access public`、NPM_TOKEN 可用性属一次性手工前置
  （spec §5），`npm whoami` fail-fast step 会在发布前拦下凭证问题。
- **R3**：registry 传播延迟由 publish-first 幂等 + §8 retry×3 + smoke retry×3 消化；
  Re-run failed jobs 只重跑 publish-npm/smoke-npm 段即可补发，设计自洽。

## 7. 验证证据

- `node --test npm/oj/test/postinstall.test.js` → 6/6 PASS（darwin-arm64 本机实跑）
- `bash scripts/npm-publish.test.sh` → case1/2/3 OK, ALL OK（本机实跑）
- release.yml 结构断言（python yaml）：jobs 齐全、needs 链正确、无 continue-on-error、
  publish-npm if 条件为 `github.event_name == 'push' || inputs.draft != 'true'`
- 模板 grep 实抽：optionalDependencies 恰三子包；占位符计数 4/5 符合预期

---

> 注：本报告原存于 SDD 临时 workspace（`.superpowers/sdd/`，gitignored），
> 终审干净后按流程清理时被一并删除；此为按会话记录重建的副本。
