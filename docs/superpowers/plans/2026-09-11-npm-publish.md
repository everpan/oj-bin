# npm 分发（@oj-bin/oj）实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 将 release.yml 三平台编译产物同步发布到 npmjs（scoped `@oj-bin/*`），用户 `npm i @oj-bin/oj` 后 `./bin/oj` 即可运行，解决 GitHub Release 国内访问性差的问题。

**Architecture:** 平台子包 + optionalDependencies（esbuild 模式）：3 个 `@oj-bin/oj-<triple>` 子包各装一平台完整产物树（oj + plugins/<triple>/ + devkit/），主包 `@oj-bin/oj` 用 postinstall 把匹配平台的子包内容拷到 `$INIT_CWD/bin/`。CI 在 release.yml 新增独立 `publish-npm` job（不调 continue-on-error，失败真标红）+ 三 runner `smoke-npm` 验证 job。

**Tech Stack:** bash（scripts/npm-publish.sh，兼容 macOS bash 3.2）、零依赖 CommonJS（postinstall.js，node:test 测试）、GitHub Actions。

**Spec:** `docs/superpowers/specs/2026-09-11-npm-publish-design.md`（已评审修订，15 条发现全部处置）

## Global Constraints

- npm 包 version = **`${tag#v}`**（npm 不允许前导 `v`）；tag 与 `oj/Cargo.toml` version 一致性门禁在脚本内复核。
- 全部 `npm publish` 带 `--access public`（scoped 包必需）。
- triple ↔ os/cpu 映射表**两处各一份**：`scripts/npm-publish.sh` 正向 + `npm/oj/postinstall.js` 反向，文件头互相交叉引用注释。
- 平台子包 `package.json` **禁止添加 `exports` 字段**（postinstall 依赖 `require.resolve('.../package.json')`）。
- 解包必须剥掉归档内顶层 `oj-v<ver>-<triple>/` 目录（tar `--strip-components=1`；zip 解开后取内层）；glob 用显式后缀排除 `.sha256`。
- postinstall 所有「装不上」路径 = 醒目警告 + **exit 0**（绝不炸掉用户的 `npm i`）；拷贝走「临时文件 + `fs.renameSync`」原子替换。
- **硬约束**：任一平台子包 publish 真失败 → 立即非零退出，绝不发主包（防静默空壳）。
- npm-publish.sh 兼容 **bash 3.2**（macOS 自带）：禁 `declare -A`、禁 `mapfile`。
- 提交信息遵循仓库惯例（中文描述，type 前缀），尾部加 `unix@vip.qq.com ai`。

## 前置（用户手工，CI 外）

1. npmjs.com 创建 org **`oj-bin`**（免费，public scoped 包不收钱）；
2. 生成 **Automation** 类型 granular access token，权限限 `@oj-bin/*`；
3. repo Settings → Secrets 加 **`NPM_TOKEN`**。

未完成前置时 Task 1–3 可正常开发测试，Task 4 的 CI 会在 `npm whoami` 处 fail fast。

## 与 spec 的两处实现级微修正（已在 Task 5 同步回 spec）

- scoped 包路径多一层：fallback「上溯两级」实为**上溯三级**（`<root>/node_modules/@oj-bin/oj` → `<root>`）。
- `--prefix` 哨兵只在 **npm 标准布局**（`up1==@oj-bin && up2==node_modules`）下生效；pnpm/berry 布局不套此启发式（避免对 pnpm 全量误报）。

---

### Task 1: npm/ 包模板与展示页

**Files:**
- Create: `npm/oj/package.json`
- Create: `npm/platform/package.json`
- Create: `npm/README.md`

**Interfaces:**
- Produces: 主包模板含占位符 `__VERSION__`；平台模板含 `__TRIPLE__`/`__VERSION__`/`__OS__`/`__CPU__`。`scripts/npm-publish.sh`（Task 3）用 `sed` 注入这些占位符——占位符拼写是本任务与 Task 3 的契约。
- 主包 `optionalDependencies` 静态列出三个子包（版本占位）——Task 3 会校验模板清单与 dist/ 实际产物 triple 集一致。

- [ ] **Step 1: 写主包模板 `npm/oj/package.json`**

```json
{
  "name": "@oj-bin/oj",
  "version": "__VERSION__",
  "description": "only-js (oj): low-code backend framework embedding a JS/TS runtime (V8) in Rust. Prebuilt binaries; install drops ./bin/oj + plugins + devkit into your project.",
  "keywords": ["only-js", "oj", "low-code", "backend", "v8", "deno", "typescript"],
  "homepage": "https://github.com/everpan/only-js",
  "repository": { "type": "git", "url": "git+https://github.com/everpan/only-js.git" },
  "engines": { "node": ">=16" },
  "scripts": { "postinstall": "node postinstall.js" },
  "optionalDependencies": {
    "@oj-bin/oj-x86_64-unknown-linux-gnu": "__VERSION__",
    "@oj-bin/oj-aarch64-apple-darwin": "__VERSION__",
    "@oj-bin/oj-x86_64-pc-windows-msvc": "__VERSION__"
  },
  "files": ["postinstall.js", "README.md"]
}
```

- [ ] **Step 2: 写平台子包模板 `npm/platform/package.json`**

```json
{
  "name": "@oj-bin/oj-__TRIPLE__",
  "version": "__VERSION__",
  "description": "Prebuilt oj binary + plugins for __TRIPLE__. Optional platform package of @oj-bin/oj; do not install directly.",
  "homepage": "https://github.com/everpan/only-js",
  "repository": { "type": "git", "url": "git+https://github.com/everpan/only-js.git" },
  "os": ["__OS__"],
  "cpu": ["__CPU__"],
  "_comment": "禁止添加 exports 字段——@oj-bin/oj 的 postinstall 用 require.resolve('@oj-bin/oj-<triple>/package.json') 定位本包；加 exports 而未导出 ./package.json 会在用户机器上抛 ERR_PACKAGE_PATH_NOT_EXPORTED。"
}
```

- [ ] **Step 3: 写 `npm/README.md`**（npmjs 展示页，英文，同时拷进每个子包）

```markdown
# @oj-bin/oj

Prebuilt binaries for **only-js** (`oj`) — a low-code backend framework that embeds a
JavaScript/TypeScript runtime (V8, via `deno_core`) into Rust. Write business logic as
JS/TS handlers; Rust serves them over HTTP with injected globals (`db`, `kv`, `blob`,
`bus`, `es`, `fetch`, `WebSocket`, …).

## Install

```bash
npm i @oj-bin/oj
```

A postinstall script copies the binaries for your platform into **`./bin/`** of the
directory where you ran `npm i`:

```
bin/oj                     # main CLI (oj.exe on Windows)
bin/plugins/<triple>/      # backend plugin cdylibs (db/kv/blob/bus/es/auth)
bin/devkit/                # API manual + global.d.ts
```

```bash
./bin/oj server -c config.yaml --api-path src
```

## Supported platforms

| platform | triple |
|---|---|
| linux x64 (glibc) | `x86_64-unknown-linux-gnu` |
| macOS arm64 | `aarch64-apple-darwin` |
| windows x64 (msvc) | `x86_64-pc-windows-msvc` |

Other platforms: download from
[GitHub Releases](https://github.com/everpan/only-js/releases) or build from source.

## Notes

- **pnpm ≥ 10** does not run dependency lifecycle scripts by default; add to
  `pnpm-workspace.yaml`:
  ```yaml
  onlyBuiltDependencies: ["@oj-bin/oj"]
  ```
- If you install with `--ignore-scripts`, run the installer manually:
  `node node_modules/@oj-bin/oj/postinstall.js`
- Global install (`npm i -g`) is **not** supported (binaries land in `./bin/` of the
  current project). Use a project-local install or the GitHub Release archives.
- China mirrors: npmmirror syncs this package automatically —
  `npm i @oj-bin/oj --registry=https://registry.npmmirror.com`

Repo & docs: <https://github.com/everpan/only-js>
```

- [ ] **Step 4: 校验 JSON 合法 + 占位符齐全**

```bash
node -e "JSON.parse(require('fs').readFileSync('npm/oj/package.json'))"
node -e "JSON.parse(require('fs').readFileSync('npm/platform/package.json'))"
grep -q __VERSION__ npm/oj/package.json && grep -q __TRIPLE__ npm/platform/package.json && echo OK
```

Expected: 两次解析无输出（成功）+ `OK`。

- [ ] **Step 5: Commit**

```bash
git add npm/
git commit -m "feat(npm): @oj-bin/oj 包模板与 npmjs 展示页——主包 + 平台子包模板

unix@vip.qq.com ai"
```

---

### Task 2: postinstall.js（TDD）

**Files:**
- Create: `npm/oj/postinstall.js`
- Test: `npm/oj/test/postinstall.test.js`

**Interfaces:**
- Consumes: `npm/oj/package.json` 的 `"postinstall": "node postinstall.js"`（Task 1）；发布后的平台子包布局（`oj[.exe]` / `plugins/<triple>/` / `devkit/` 在包根）。
- Produces: 落盘 `<installRoot>/bin/{oj[.exe], plugins/<triple>/, devkit/}`；全部失败路径 exit 0。手动兜底入口：`node node_modules/@oj-bin/oj/postinstall.js`。

- [ ] **Step 1: 写失败测试 `npm/oj/test/postinstall.test.js`**

要点：fixture 把 `postinstall.js` 拷进 `root/node_modules/@oj-bin/oj/`，使 `require.resolve` 在 fixture 内解析；用受控 env（不带外部 `npm_config_*` 泄漏）spawn 真实 node 跑。

```js
'use strict';
// postinstall 行为测试：fixture 模拟 <root>/node_modules/@oj-bin/{oj,oj-<triple>}，
// 以受控 env spawn 真实 node 执行 postinstall，断言落盘与 exit 0 语义。
const test = require('node:test');
const assert = require('node:assert');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawnSync } = require('node:child_process');

const POSTINSTALL = path.join(__dirname, '..', 'postinstall.js');
const KEY = `${process.platform}-${process.arch}`;
const TRIPLES = {
  'linux-x64': 'x86_64-unknown-linux-gnu',
  'darwin-arm64': 'aarch64-apple-darwin',
  'win32-x64': 'x86_64-pc-windows-msvc',
};
const TRIPLE = TRIPLES[KEY]; // 非三平台开发机上为 undefined → 相关用例 skip

function makeFixture(triple, { withSub = true } = {}) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'oj-npm-'));
  const mainDir = path.join(root, 'node_modules', '@oj-bin', 'oj');
  fs.mkdirSync(mainDir, { recursive: true });
  fs.copyFileSync(POSTINSTALL, path.join(mainDir, 'postinstall.js'));
  if (withSub) {
    const sub = path.join(root, 'node_modules', '@oj-bin', `oj-${triple}`);
    fs.mkdirSync(path.join(sub, 'plugins', triple), { recursive: true });
    fs.mkdirSync(path.join(sub, 'devkit'), { recursive: true });
    fs.writeFileSync(path.join(sub, 'package.json'),
      JSON.stringify({ name: `@oj-bin/oj-${triple}`, version: '9.9.9' }));
    fs.writeFileSync(path.join(sub, 'oj'), 'oj-v1\n');
    fs.writeFileSync(path.join(sub, 'plugins', triple, 'libx.so'), 'fake\n');
    fs.writeFileSync(path.join(sub, 'devkit', 'api-manual.md'), '# fake\n');
  }
  return root;
}

// env 最小化：不带 process.env，防外层 npm_config_* 泄漏进用例。
function run(root, env = {}, { initCwd = true } = {}) {
  const e = { PATH: process.env.PATH, ...env };
  if (initCwd) e.INIT_CWD = root;
  return spawnSync(process.execPath,
    [path.join(root, 'node_modules', '@oj-bin', 'oj', 'postinstall.js')],
    { cwd: root, env: e, encoding: 'utf8' });
}

test('happy path：落盘 bin/oj + plugins/<triple>/ + devkit/', { skip: !TRIPLE }, () => {
  const root = makeFixture(TRIPLE);
  const r = run(root);
  assert.strictEqual(r.status, 0, r.stdout + r.stderr);
  assert.ok(fs.existsSync(path.join(root, 'bin', 'oj')));
  assert.ok(fs.existsSync(path.join(root, 'bin', 'plugins', TRIPLE, 'libx.so')));
  assert.ok(fs.existsSync(path.join(root, 'bin', 'devkit', 'api-manual.md')));
  if (process.platform !== 'win32') {
    const mode = fs.statSync(path.join(root, 'bin', 'oj')).mode & 0o777;
    assert.ok(mode & 0o100, `bin/oj 应有可执行位，实际 mode=${mode.toString(8)}`);
  }
});

test('全局安装 → 警告 + exit 0 + 不落盘', () => {
  const root = makeFixture(TRIPLE || 'x86_64-unknown-linux-gnu', { withSub: false });
  const r = run(root, { npm_config_global: 'true' });
  assert.strictEqual(r.status, 0);
  assert.match(r.stdout + r.stderr, /全局安装/);
  assert.ok(!fs.existsSync(path.join(root, 'bin')));
});

test('缺平台子包 → 警告 + exit 0 + 不落盘', { skip: !TRIPLE }, () => {
  const root = makeFixture(TRIPLE, { withSub: false });
  const r = run(root);
  assert.strictEqual(r.status, 0);
  assert.match(r.stdout + r.stderr, /未安装平台子包/);
  assert.ok(!fs.existsSync(path.join(root, 'bin')));
});

test('幂等覆盖：重跑后内容更新', { skip: !TRIPLE }, () => {
  const root = makeFixture(TRIPLE);
  assert.strictEqual(run(root).status, 0);
  fs.writeFileSync(
    path.join(root, 'node_modules', '@oj-bin', `oj-${TRIPLE}`, 'oj'), 'oj-v2\n');
  assert.strictEqual(run(root).status, 0);
  assert.strictEqual(fs.readFileSync(path.join(root, 'bin', 'oj'), 'utf8'), 'oj-v2\n');
});

test('INIT_CWD 缺失 → PROJECT_CWD 兜底', { skip: !TRIPLE }, () => {
  const root = makeFixture(TRIPLE);
  const r = run(root, { PROJECT_CWD: root }, { initCwd: false });
  assert.strictEqual(r.status, 0, r.stdout + r.stderr);
  assert.ok(fs.existsSync(path.join(root, 'bin', 'oj')));
});

test('npm 标准布局下 INIT_CWD != 安装根（--prefix/workspace 子目录）→ 警告 + exit 0 + 不落盘', { skip: !TRIPLE }, () => {
  const root = makeFixture(TRIPLE);
  const other = fs.mkdtempSync(path.join(os.tmpdir(), 'oj-other-'));
  const r = run(root, {}, { initCwd: false });
  // 先确认：无 INIT_CWD/PROJECT_CWD 时走上溯三级兜底 = root，正常落盘
  assert.strictEqual(r.status, 0, r.stdout + r.stderr);
  assert.ok(fs.existsSync(path.join(root, 'bin', 'oj')));
  const r2 = spawnSync(process.execPath,
    [path.join(root, 'node_modules', '@oj-bin', 'oj', 'postinstall.js')],
    { cwd: root, env: { PATH: process.env.PATH, INIT_CWD: other }, encoding: 'utf8' });
  assert.strictEqual(r2.status, 0);
  assert.match(r2.stdout + r2.stderr, /不一致/);
  assert.ok(!fs.existsSync(path.join(other, 'bin')));
});
```

- [ ] **Step 2: 跑测试确认全部失败**

```bash
node --test npm/oj/test/
```

Expected: 全 FAIL（`npm/oj/postinstall.js` 尚不存在，`fs.copyFileSync` 抛 ENOENT）。

- [ ] **Step 3: 写 `npm/oj/postinstall.js`**

```js
#!/usr/bin/env node
// @oj-bin/oj postinstall —— 把匹配当前平台的子包内容拷到 <项目根>/bin/。
// 零依赖 CommonJS；可裸跑（--ignore-scripts / pnpm 用户的手动兜底）：
//   node node_modules/@oj-bin/oj/postinstall.js
// 约定：所有「装不上」路径都打印醒目警告并 exit 0 —— 绝不炸掉用户的 npm i。
'use strict';

const fs = require('fs');
const path = require('path');

// platform-arch → triple 反向表。与 scripts/npm-publish.sh 的 os_cpu_of() 正向表
// 交叉维护：改一边必须改另一边。
const TRIPLES = {
  'linux-x64': 'x86_64-unknown-linux-gnu',
  'darwin-arm64': 'aarch64-apple-darwin',
  'win32-x64': 'x86_64-pc-windows-msvc',
};
const RELEASES = 'https://github.com/everpan/only-js/releases';
const TAG = '[@oj-bin/oj]';

function bail(msg) { // 装不上：醒目提示 + exit 0
  console.warn(`${TAG} WARN: ${msg}`);
  process.exit(0);
}

// ---- 1. 支持面检测 ------------------------------------------------------
if (process.env.npm_config_global === 'true') {
  bail(`不支持全局安装（npm i -g）：本包落盘到 <cwd>/bin/，全局安装没有确定的项目根。\n` +
    `请在项目内执行 npm i @oj-bin/oj，或从 ${RELEASES} 下载。`);
}

// ---- 2. 平台 → triple ----------------------------------------------------
const key = `${process.platform}-${process.arch}`;
const triple = TRIPLES[key];
if (!triple) {
  bail(`暂无 ${key} 的预编译包（现有：${Object.keys(TRIPLES).join(', ')}）。\n` +
    `请从 ${RELEASES} 下载，或提 issue 请求该平台。`);
}

// ---- 3. 落盘根 -----------------------------------------------------------
// npm/pnpm/yarn classic 设 INIT_CWD；yarn berry 设 PROJECT_CWD；最后手段上溯三级
// （scoped 包多一层：<root>/node_modules/@oj-bin/oj → <root>，仅 npm 标准布局碰巧对）。
const up3 = path.resolve(__dirname, '..', '..', '..');
const installRoot = process.env.INIT_CWD || process.env.PROJECT_CWD || up3;

// --prefix / workspace 子目录哨兵：仅当本包确实处于 npm 标准布局
// （<root>/node_modules/@oj-bin/oj）且 INIT_CWD 与安装根不一致时才判定——
// pnpm/.pnpm、berry PnP 布局不套此启发式（它们的 INIT_CWD/PROJECT_CWD 可信）。
const up1 = path.basename(path.dirname(__dirname));
const up2 = path.basename(path.dirname(path.dirname(__dirname)));
if (process.env.INIT_CWD && up1 === '@oj-bin' && up2 === 'node_modules' &&
  path.resolve(process.env.INIT_CWD) !== up3) {
  bail(`安装根（${up3}）与当前目录（${process.env.INIT_CWD}）不一致（--prefix 或 workspace 子目录安装）。\n` +
    `为避免装错位置已跳过。请进入目标项目目录重装，或手动执行：\n` +
    `  node ${path.join(__dirname, 'postinstall.js')}`);
}

// ---- 4. 定位平台子包 -----------------------------------------------------
let subRoot;
try {
  subRoot = path.dirname(require.resolve(`@oj-bin/oj-${triple}/package.json`));
} catch {
  bail(`未安装平台子包 @oj-bin/oj-${triple}（可能被 --omit=optional / ignore-scripts 类配置排除）。\n` +
    `请检查安装参数，或从 ${RELEASES} 下载。`);
}

// ---- 5. 拷贝：文件级「临时文件 + rename」原子替换 -------------------------
// unix：rename 可覆盖正在执行的 bin/oj（避免 ETXTBSY）；
// Windows：已加载的旧 DLL 允许 rename 让位（不允许覆盖写）。
const destBin = path.join(installRoot, 'bin');

function installFile(src, dest) {
  fs.mkdirSync(path.dirname(dest), { recursive: true });
  const tmp = `${dest}.tmp-${process.pid}`;
  const old = `${dest}.old-${process.pid}`;
  fs.copyFileSync(src, tmp);
  try {
    if (fs.existsSync(dest)) fs.renameSync(dest, old);
    fs.renameSync(tmp, dest);
    fs.rmSync(old, { force: true });
    fs.chmodSync(dest, 0o755);
  } catch (e) {
    fs.rmSync(tmp, { force: true });
    throw e;
  }
}

function installTree(srcDir, rel) {
  for (const name of fs.readdirSync(srcDir)) {
    const s = path.join(srcDir, name);
    const r = rel ? `${rel}/${name}` : name;
    if (fs.statSync(s).isDirectory()) installTree(s, r);
    else installFile(s, path.join(destBin, r));
  }
}

let copied = 0;
try {
  for (const entry of ['oj', 'oj.exe', 'plugins', 'devkit']) {
    const s = path.join(subRoot, entry);
    if (!fs.existsSync(s)) continue;
    if (fs.statSync(s).isDirectory()) installTree(s, entry);
    else installFile(s, path.join(destBin, entry));
    copied++;
  }
} catch (e) {
  bail(`拷贝失败（${e.code || e.message}）。若有正在运行的 oj，请先停止后重试：\n` +
    `  node ${path.join(__dirname, 'postinstall.js')}`);
}
if (copied === 0) {
  bail(`平台子包 @oj-bin/oj-${triple} 内容为空（${subRoot}），安装中止。`);
}

console.log(`${TAG} installed → ${destBin} (triple=${triple})`);
```

- [ ] **Step 4: 跑测试确认全部通过**

```bash
node --test npm/oj/test/
```

Expected: 6/6 PASS（非三平台机器上 4 条 skip、2 条 PASS——本机 darwin-arm64 应 6 条全 PASS）。

- [ ] **Step 5: Commit**

```bash
git add npm/oj/postinstall.js npm/oj/test/
git commit -m "feat(npm): postinstall 落盘 ./bin——支持面检测 + 原子替换 + exit 0 语义

unix@vip.qq.com ai"
```

---

### Task 3: scripts/npm-publish.sh + dry-run 自检

**Files:**
- Create: `scripts/npm-publish.sh`
- Test: `scripts/npm-publish.test.sh`

**Interfaces:**
- Consumes: Task 1 的模板占位符（`__VERSION__`/`__TRIPLE__`/`__OS__`/`__CPU__`）；`dist/oj-v<ver>-<triple>.{tar.gz,zip}`（deploy.sh/deploy.bat 产物，归档内含顶层 `oj-v<ver>-<triple>/` 目录）。
- Produces: CLI `bash scripts/npm-publish.sh <tag>`；env 旋钮 `DIST_DIR`（默认 `./dist`）、`STAGE_DIR`（默认 mktemp）、`DRY_RUN=1`（只装配+断言，不 publish）。Task 4 的 workflow 以 `bash scripts/npm-publish.sh "<tag>"` 调用。

- [ ] **Step 1: 写失败测试 `scripts/npm-publish.test.sh`**

```bash
#!/usr/bin/env bash
# npm-publish.sh 的 dry-run 自检：fixture 产物 → 断言装配布局/元数据/门禁。
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
VER=$(awk -F'"' '/^version =[[:space:]]*"/ { print $2; exit }' oj/Cargo.toml)
T=x86_64-unknown-linux-gnu

# fixture：与 deploy.sh 产物同形（含顶层目录 + .sha256 干扰项）
SRC="$TMP/src/oj-v${VER}-${T}"
mkdir -p "$SRC/plugins/$T" "$SRC/devkit" "$TMP/dist"
echo fake-oj > "$SRC/oj"
echo fake-so > "$SRC/plugins/$T/libx.so"
echo fake-doc > "$SRC/devkit/api-manual.md"
tar -czf "$TMP/dist/oj-v${VER}-${T}.tar.gz" -C "$TMP/src" "oj-v${VER}-${T}"
echo sum > "$TMP/dist/oj-v${VER}-${T}.tar.gz.sha256"

echo "== case 1: dry-run 装配 =="
DIST_DIR="$TMP/dist" STAGE_DIR="$TMP/stage" DRY_RUN=1 bash scripts/npm-publish.sh "v${VER}"

P="$TMP/stage/oj-$T"
[[ -f "$P/oj" ]]                       || { echo "FAIL: oj 未在包根（strip-components 失效）"; exit 1; }
[[ -f "$P/plugins/$T/libx.so" ]]       || { echo "FAIL: plugins 布局错"; exit 1; }
[[ -f "$P/devkit/api-manual.md" ]]     || { echo "FAIL: devkit 布局错"; exit 1; }
grep -q "\"name\": \"@oj-bin/oj-$T\"" "$P/package.json" || { echo "FAIL: name 注入错"; exit 1; }
grep -q "\"version\": \"$VER\""        "$P/package.json" || { echo "FAIL: version 注入错"; exit 1; }
grep -q '"linux"' "$P/package.json" && grep -q '"x64"' "$P/package.json" || { echo "FAIL: os/cpu 注入错"; exit 1; }
[[ -f "$P/README.md" ]] || { echo "FAIL: README 未拷入"; exit 1; }
grep -q "\"version\": \"$VER\"" "$TMP/stage/oj-main/package.json" || { echo "FAIL: 主包 version 注入错"; exit 1; }
[[ -f "$TMP/stage/oj-main/postinstall.js" ]] || { echo "FAIL: 主包缺 postinstall.js"; exit 1; }
echo "case 1 OK"

echo "== case 2: 未知 triple 门禁（musl 未入表 → 必须 fail）=="
M=x86_64-unknown-linux-musl
SRCM="$TMP/src/oj-v${VER}-${M}"
mkdir -p "$SRCM/plugins/$M" "$SRCM/devkit"
echo fake-oj > "$SRCM/oj"; echo fake > "$SRCM/plugins/$M/libx.so"; echo fake > "$SRCM/devkit/api-manual.md"
tar -czf "$TMP/dist/oj-v${VER}-${M}.tar.gz" -C "$TMP/src" "oj-v${VER}-${M}"
if DIST_DIR="$TMP/dist" STAGE_DIR="$TMP/stage2" DRY_RUN=1 bash scripts/npm-publish.sh "v${VER}" 2>"$TMP/err"; then
  echo "FAIL: 未知 triple 未被门禁拦下"; exit 1
fi
grep -q "未知 triple" "$TMP/err" || { echo "FAIL: 门禁报错文案缺失"; cat "$TMP/err"; exit 1; }
echo "case 2 OK"

echo "== case 3: 版本门禁（tag != oj/Cargo.toml）=="
if DIST_DIR="$TMP/dist" STAGE_DIR="$TMP/stage3" DRY_RUN=1 bash scripts/npm-publish.sh "v0.0.0-bogus" 2>"$TMP/err3"; then
  echo "FAIL: 版本不一致未被拦下"; exit 1
fi
grep -q "不一致" "$TMP/err3" || { echo "FAIL: 版本门禁文案缺失"; cat "$TMP/err3"; exit 1; }
echo "case 3 OK"
echo "ALL OK"
```

跑一下确认失败：

```bash
bash scripts/npm-publish.test.sh
```

Expected: FAIL（`scripts/npm-publish.sh` 不存在）。

- [ ] **Step 2: 写 `scripts/npm-publish.sh`**

```bash
#!/usr/bin/env bash
# npm 发布脚本（单一真相来源，CI 与本地同形）。
# 用法:   bash scripts/npm-publish.sh <tag>
# env:    DIST_DIR（默认 ./dist） STAGE_DIR（默认 mktemp） DRY_RUN=1（只装配+断言，不 publish）
# 顺序:   平台子包逐个 publish（任一真失败即死，绝不发主包）→ 主包 → 发布后置信断言。
# 兼容 bash 3.2（macOS 自带）：禁 declare -A / mapfile。
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

TAG="${1:?usage: npm-publish.sh <tag>}"
VERSION="${TAG#v}"   # npm version 不允许前导 v
SCOPE="@oj-bin"
DIST_DIR="${DIST_DIR:-$PWD/dist}"

# ---- 1. 版本一致性门禁（与 release.yml resolve-tag 同源，双保险）--------
cargo_version=$(awk -F'"' '/^version =[[:space:]]*"/ { print $2; exit }' oj/Cargo.toml)
if [[ "$VERSION" != "$cargo_version" ]]; then
  echo "::error::tag '${TAG}' 与 oj/Cargo.toml version '${cargo_version}' 不一致" >&2
  exit 1
fi

# ---- 2. triple → os cpu（与 npm/oj/postinstall.js 的 TRIPLES 反向表交叉维护：改一边必须改另一边）
os_cpu_of() {
  case "$1" in
    x86_64-unknown-linux-gnu)  echo "linux x64" ;;
    aarch64-apple-darwin)      echo "darwin arm64" ;;
    x86_64-pc-windows-msvc)    echo "win32 x64" ;;
    *) return 1 ;;
  esac
}

# ---- 3. 收集产物 triple（显式后缀 glob，排除 .sha256）-------------------
triples=()
for f in "$DIST_DIR"/oj-v*-*.tar.gz "$DIST_DIR"/oj-v*-*.zip; do
  [[ -e "$f" ]] || continue
  base=$(basename "$f"); base="${base%.tar.gz}"; base="${base%.zip}"
  triples+=("${base#oj-v${VERSION}-}")
done
[[ ${#triples[@]} -gt 0 ]] || { echo "::error::${DIST_DIR} 下无 oj-v${VERSION}-* 产物" >&2; exit 1; }

# ---- 4. 防呆：未知 triple / 同 (os,cpu) 撞车（musl 与 gnu 并存时撞 linux+x64——
#         npm os/cpu 无法区分，启用 musl 前必须先定 libc 策略）--------------
seen=""
for t in "${triples[@]}"; do
  oscpu=$(os_cpu_of "$t") || { echo "::error::未知 triple '$t'（os_cpu_of 未覆盖，先扩映射表再发布）" >&2; exit 1; }
  case " $seen " in
    *" $oscpu "*) echo "::error::(os,cpu)=[$oscpu] 撞车：${t}——同一 (os,cpu) 不允许两个 triple" >&2; exit 1 ;;
  esac
  seen="$seen $oscpu"
done

# ---- 5. publish-first 幂等 ----------------------------------------------
# npm view 预检命中 CDN 旧缓存可能误判不存在 → publish 失败后 re-view，可见即成功。
publish_pkg() { # <pkgdir> <pkg-name>
  local dir="$1" pkg="$2" out
  if [[ "${DRY_RUN:-0}" == "1" ]]; then echo "[dry-run] publish ${pkg}@${VERSION} ($dir)"; return 0; fi
  if npm view "${pkg}@${VERSION}" version >/dev/null 2>&1; then
    echo "skip ${pkg}@${VERSION} (already published)"; return 0
  fi
  if ! out=$(npm publish "$dir" --access public 2>&1); then
    if npm view "${pkg}@${VERSION}" version >/dev/null 2>&1; then
      echo "already published (registry lag), skip ${pkg}@${VERSION}"
    else
      echo "$out" >&2; echo "::error::publish ${pkg}@${VERSION} 失败" >&2; exit 1
    fi
  else
    echo "published ${pkg}@${VERSION}"
  fi
}

STAGE_DIR="${STAGE_DIR:-$(mktemp -d)}"

# ---- 6. 装配 + 发布平台子包（失败即 exit，绝不进第 7 步发主包）-----------
for t in "${triples[@]}"; do
  read -r os cpu <<<"$(os_cpu_of "$t")"
  pkgdir="$STAGE_DIR/oj-$t"
  mkdir -p "$pkgdir"
  if [[ -f "$DIST_DIR/oj-v${VERSION}-${t}.tar.gz" ]]; then
    # 剥掉归档内顶层 oj-v<ver>-<triple>/ 目录（GNU/BSD tar 均支持）
    tar -xzf "$DIST_DIR/oj-v${VERSION}-${t}.tar.gz" -C "$pkgdir" --strip-components=1
  else
    # deploy.bat 用 bsdtar 产 zip：python3 zipfile 兼容性最稳，unzip 兜底
    if command -v python3 >/dev/null 2>&1; then
      python3 -m zipfile -e "$DIST_DIR/oj-v${VERSION}-${t}.zip" "$pkgdir/.x"
    else
      unzip -q "$DIST_DIR/oj-v${VERSION}-${t}.zip" -d "$pkgdir/.x"
    fi
    mv "$pkgdir/.x/oj-v${VERSION}-${t}/"* "$pkgdir/"
    rm -rf "$pkgdir/.x"
  fi
  # 布局断言：包根必须直接是 oj[.exe] / plugins/<triple>/ / devkit/
  [[ -f "$pkgdir/oj" || -f "$pkgdir/oj.exe" ]] || { echo "::error::$t 包根缺 oj 二进制（strip 失效？）" >&2; exit 1; }
  [[ -d "$pkgdir/plugins/$t" ]] || { echo "::error::$t 缺 plugins/$t" >&2; exit 1; }
  [[ -f "$pkgdir/devkit/api-manual.md" ]] || { echo "::error::$t 缺 devkit" >&2; exit 1; }
  sed -e "s/__TRIPLE__/$t/g" -e "s/__VERSION__/$VERSION/g" \
      -e "s/__OS__/$os/g" -e "s/__CPU__/$cpu/g" \
      npm/platform/package.json > "$pkgdir/package.json"
  cp npm/README.md "$pkgdir/README.md"
  [[ -f LICENSE ]] && cp LICENSE "$pkgdir/" || true
  publish_pkg "$pkgdir" "${SCOPE}/oj-${t}"
done

# ---- 7. 装配 + 发布主包 ---------------------------------------------------
# 模板 optionalDependencies 与实际产物 triple 集必须互为充要（防模板/矩阵漂移）
maindir="$STAGE_DIR/oj-main"
mkdir -p "$maindir"
for t in "${triples[@]}"; do
  grep -q "${SCOPE}/oj-${t}" npm/oj/package.json || { echo "::error::主包模板 optionalDependencies 缺 $t" >&2; exit 1; }
done
for name in $(grep -o "${SCOPE}/oj-[a-z0-9_-]*" npm/oj/package.json | sort -u); do
  t="${name#${SCOPE}/oj-}"
  ok=0
  for have in "${triples[@]}"; do [[ "$have" == "$t" ]] && ok=1; done
  [[ $ok == 1 ]] || { echo "::error::主包模板列了 $name 但 dist/ 无对应产物" >&2; exit 1; }
done
sed "s/__VERSION__/$VERSION/g" npm/oj/package.json > "$maindir/package.json"
cp npm/oj/postinstall.js npm/README.md "$maindir/"
[[ -f LICENSE ]] && cp LICENSE "$maindir/" || true
publish_pkg "$maindir" "${SCOPE}/oj"

# ---- 8. 发布后置信（DRY_RUN 跳过）----------------------------------------
if [[ "${DRY_RUN:-0}" != "1" ]]; then
  for t in "${triples[@]}"; do
    read -r os cpu <<<"$(os_cpu_of "$t")"
    pkg="${SCOPE}/oj-${t}"
    # registry 传播延迟：retry 3 次
    meta=""
    for _ in 1 2 3; do
      meta=$(npm view "${pkg}@${VERSION}" os cpu --json 2>/dev/null) && [[ -n "$meta" ]] && break
      sleep 10
    done
    echo "$meta" | grep -q "$os" || { echo "::error::${pkg} os 断言失败（期望 $os）：$meta" >&2; exit 1; }
    echo "$meta" | grep -q "$cpu" || { echo "::error::${pkg} cpu 断言失败（期望 $cpu）：$meta" >&2; exit 1; }
    # tarball 文件清单断言（npm pack 条目带 package/ 前缀）
    tb=$(npm view "${pkg}@${VERSION}" dist.tarball)
    case "$os" in
      win32)  want='oj\.exe$|\.dll$' ;;
      darwin) want='package/oj$|\.dylib$' ;;
      *)      want='package/oj$|\.so$' ;;
    esac
    curl -fsSL "$tb" | tar -tzf - | grep -qE "$want" || { echo "::error::${pkg} tarball 文件清单断言失败" >&2; exit 1; }
    echo "verified ${pkg}@${VERSION} os=$os cpu=$cpu"
  done
fi

echo "npm publish done: ${SCOPE}/oj@${VERSION} + ${#triples[@]} platform packages"
```

- [ ] **Step 3: 跑自检确认通过**

```bash
bash scripts/npm-publish.test.sh
```

Expected: `case 1 OK` / `case 2 OK` / `case 3 OK` / `ALL OK`。

- [ ] **Step 4: Commit**

```bash
git add scripts/npm-publish.sh scripts/npm-publish.test.sh
git commit -m "feat(npm): npm-publish.sh——装配/幂等发布/门禁/置信断言 + dry-run 自检

unix@vip.qq.com ai"
```

---

### Task 4: release.yml —— publish-npm + smoke-npm job

**Files:**
- Modify: `.github/workflows/release.yml`（lint job 加 node 测试步骤；文件尾加两个 job）

**Interfaces:**
- Consumes: Task 3 的 `bash scripts/npm-publish.sh <tag>`；`dist-*` artifacts（现有 `package` job 产出）；secret `NPM_TOKEN`（前置 3）。
- Produces: job `publish-npm`（outputs `version`）；job `smoke-npm`（`needs: publish-npm`，三 runner 矩阵）。`publish` job 完全不动。

- [ ] **Step 1: lint job 追加 npm 工具链测试**

在 `release.yml` lint job 的 clippy 步骤之后追加（lint 是 package 的前置，npm 工具链坏了在打包前就拦下）：

```yaml
      - uses: actions/setup-node@v4
        with:
          node-version: '22'

      - name: npm packaging tests (postinstall + dry-run publish)
        run: |
          node --test npm/oj/test/
          bash scripts/npm-publish.test.sh
```

- [ ] **Step 2: 文件尾追加 publish-npm job**

`publish` job **一行不改**。在其后追加（tag 解析内联同一段 awk 与一致性门禁——spec §4 允许，语义与 `publish` job 的 resolve tag 相同）：

```yaml
  # npm 发布（@oj-bin/*）：独立 job——不用 continue-on-error（step 级 continue-on-error
  # 下 job 结论仍绿，告警形同虚设）。失败 = workflow 红；GitHub Release 由 publish job
  # 先行创建不受影响；幂等设计支持 Re-run failed jobs 只重跑本段。
  publish-npm:
    name: publish npm (@oj-bin)
    needs: package
    runs-on: ubuntu-latest
    outputs:
      version: ${{ steps.tag.outputs.version }}
    steps:
      - uses: actions/checkout@v4

      - uses: actions/download-artifact@v4
        with:
          pattern: dist-*
          path: dist
          merge-multiple: true

      - name: resolve tag
        id: tag
        run: |
          cargo_version=$(awk -F'"' '/^version =[[:space:]]*"/ { print $2; exit }' oj/Cargo.toml)
          if [ "${{ github.event_name }}" = "push" ]; then
            tag="${GITHUB_REF_NAME}"
          elif [ -n "${{ inputs.tag }}" ]; then
            tag="${{ inputs.tag }}"
          else
            tag="v${cargo_version}"
          fi
          if [ "${tag#v}" != "$cargo_version" ]; then
            echo "::error::tag '${tag}' 与 oj/Cargo.toml 的 version '${cargo_version}' 不一致" >&2
            exit 1
          fi
          echo "tag=${tag}" >> "$GITHUB_OUTPUT"
          echo "version=${tag#v}" >> "$GITHUB_OUTPUT"

      - uses: actions/setup-node@v4
        with:
          node-version: '22'
          registry-url: 'https://registry.npmjs.org'

      - name: npm whoami (fail fast on bad NPM_TOKEN)
        env:
          NODE_AUTH_TOKEN: ${{ secrets.NPM_TOKEN }}
        run: npm whoami

      - name: publish @oj-bin packages
        env:
          NODE_AUTH_TOKEN: ${{ secrets.NPM_TOKEN }}
        run: bash scripts/npm-publish.sh "${{ steps.tag.outputs.tag }}"
```

- [ ] **Step 3: 文件尾追加 smoke-npm job（三 runner 真装真跑）**

```yaml
  # 发布后置信：三平台真机 npm i + ./bin/oj --help。Windows postinstall 路径只有
  # 真跑才能验证；registry 传播延迟靠 retry 消化。
  smoke-npm:
    name: smoke npm (${{ matrix.triple }})
    needs: publish-npm
    runs-on: ${{ matrix.os }}
    strategy:
      fail-fast: false
      matrix:
        include:
          - { os: ubuntu-latest,  triple: x86_64-unknown-linux-gnu }
          - { os: macos-latest,   triple: aarch64-apple-darwin }
          - { os: windows-latest, triple: x86_64-pc-windows-msvc }
    steps:
      - uses: actions/setup-node@v4
        with:
          node-version: '22'

      - name: npm install from registry + run ./bin/oj --help
        shell: bash
        env:
          VER: ${{ needs.publish-npm.outputs.version }}
        run: |
          set -e
          d=$(mktemp -d) && cd "$d"
          npm init -y >/dev/null
          ok=0
          for i in 1 2 3; do
            if npm i "@oj-bin/oj@${VER}"; then ok=1; break; fi
            echo "retry $i: registry propagation lag"; sleep 20
          done
          [[ $ok == 1 ]] || { echo "::error::npm i @oj-bin/oj@${VER} 失败"; exit 1; }
          ls -la bin/
          if [[ "$RUNNER_OS" == "Windows" ]]; then
            [[ -f bin/oj.exe ]] && ./bin/oj.exe --help
          else
            [[ -x bin/oj ]] && ./bin/oj --help
          fi
```

- [ ] **Step 4: 校验 YAML 可解析 + 结构完整**

```bash
python3 -c "
import yaml, sys
d = yaml.safe_load(open('.github/workflows/release.yml'))
jobs = d['jobs']
assert 'publish-npm' in jobs and 'smoke-npm' in jobs, jobs.keys()
assert jobs['publish-npm']['needs'] == 'package'
assert jobs['smoke-npm']['needs'] == 'publish-npm'
assert 'continue-on-error' not in jobs['publish-npm']
print('release.yml OK:', ', '.join(jobs))
"
```

Expected: `release.yml OK: lint, package, publish, publish-npm, smoke-npm`。

（若有 actionlint：`actionlint .github/workflows/release.yml` 更佳，没有则上面的结构断言足够。）

- [ ] **Step 5: Commit**

```bash
git add .github/workflows/release.yml
git commit -m "ci(release): 新增 publish-npm + smoke-npm——npm 双发（失败标红不阻塞 Release）

unix@vip.qq.com ai"
```

---

### Task 5: 文档 + spec 微修正同步

**Files:**
- Modify: `README.md`（Quick Start 后插入 npm 安装段）
- Modify: `docs/superpowers/specs/2026-09-11-npm-publish-design.md`（两处实现级微修正同步）

- [ ] **Step 1: README.md 加 npm 安装段**

插在 Quick Start 的 `cargo xtask build` 代码块之后（位置：现有 `> Always run examples ...` 引用块之前）：

```markdown
### Install prebuilt binaries via npm

```bash
npm i @oj-bin/oj     # drops ./bin/oj + bin/plugins/<triple>/ + bin/devkit/ into your project
./bin/oj server -c sample/config.yaml --api-path sample/src
```

Prebuilt for linux-x64 (glibc) / macOS-arm64 / windows-x64; other platforms use
[GitHub Releases](https://github.com/everpan/only-js/releases). Notes: pnpm ≥10 needs
`onlyBuiltDependencies: ["@oj-bin/oj"]` in `pnpm-workspace.yaml`; with `--ignore-scripts`
run `node node_modules/@oj-bin/oj/postinstall.js` manually; global install (`-g`) is not
supported. China users may prefer `--registry=https://registry.npmmirror.com` (auto-mirrored).
```

- [ ] **Step 2: spec 同步两处微修正**

`docs/superpowers/specs/2026-09-11-npm-publish-design.md` §3.3：把「主包上溯两级」改为「主包上溯三级（scoped 包多一层：`<root>/node_modules/@oj-bin/oj` → `<root>`）」；§3.1 的 `--prefix` 检测补充「仅在 npm 标准布局（`up1==@oj-bin && up2==node_modules`）下生效，pnpm/berry 布局不套此启发式」。

- [ ] **Step 3: Commit**

```bash
git add README.md docs/superpowers/specs/2026-09-11-npm-publish-design.md
git commit -m "docs: npm 安装段入 README；spec 同步 postinstall 两处实现级修正

unix@vip.qq.com ai"
```

---

## 验证清单（实施完成后）

1. `node --test npm/oj/test/` 全过；
2. `bash scripts/npm-publish.test.sh` 全过；
3. release.yml 结构断言通过（Task 4 Step 4）；
4. 用户完成前置（org + NPM_TOKEN）后，打下一个 tag（如 v0.1.13）走完整发布：
   `publish` job 建 GitHub Release → `publish-npm` 发 4 个包 → `smoke-npm` 三平台绿；
5. 国内镜像侧抽查：`npm i @oj-bin/oj --registry=https://registry.npmmirror.com`。
