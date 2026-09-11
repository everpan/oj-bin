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
