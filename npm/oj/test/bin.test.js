'use strict';
// bin.js 启动器测试：模拟 `pnpm dlx @oj-bin/oj` 的装配布局
// （<root>/node_modules/@oj-bin/{oj,oj-<triple>}），用受控 env spawn 真实 node
// 执行 bin.js，断言它定位平台子包二进制并转发 argv、传播退出码。
const test = require('node:test');
const assert = require('node:assert');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawnSync } = require('node:child_process');

const SRC_BIN = path.join(__dirname, '..', 'bin.js');
const KEY = `${process.platform}-${process.arch}`;
const TRIPLES = {
  'linux-x64': 'x86_64-unknown-linux-gnu',
  'darwin-arm64': 'aarch64-apple-darwin',
  'win32-x64': 'x86_64-pc-windows-msvc',
};
const TRIPLE = TRIPLES[KEY]; // 非三平台开发机上为 undefined → 相关用例 skip

// 造一个假的「oj 二进制」：落地收到的 argv，并以脚本给定退出码退出。
function makeFixture(triple, { withSub = true } = {}) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'oj-bin-'));
  const mainDir = path.join(root, 'node_modules', '@oj-bin', 'oj');
  fs.mkdirSync(mainDir, { recursive: true });
  fs.copyFileSync(SRC_BIN, path.join(mainDir, 'bin.js'));
  if (withSub) {
    const sub = path.join(root, 'node_modules', '@oj-bin', `oj-${triple}`);
    fs.mkdirSync(sub, { recursive: true });
    fs.writeFileSync(path.join(sub, 'package.json'),
      JSON.stringify({ name: `@oj-bin/oj-${triple}`, version: '9.9.9' }));
    if (process.platform === 'win32') {
      fs.writeFileSync(path.join(sub, 'oj.exe'), '@echo %*\r\n');
    } else {
      fs.writeFileSync(path.join(sub, 'oj'),
        '#!/bin/sh\necho "FAKE-OJ $*"\nexit ${OJ_EXIT:-0}\n');
      fs.chmodSync(path.join(sub, 'oj'), 0o755);
    }
  }
  return root;
}

function run(root, args, env = {}) {
  const bin = path.join(root, 'node_modules', '@oj-bin', 'oj', 'bin.js');
  return spawnSync(process.execPath, [bin, ...args], {
    cwd: root, env: { PATH: process.env.PATH, ...env }, encoding: 'utf8',
  });
}

test('dlx 布局：bin.js 定位平台子包并转发 argv', { skip: !TRIPLE }, () => {
  const root = makeFixture(TRIPLE);
  const r = run(root, ['serve', '-c', 'config.yaml']);
  assert.strictEqual(r.status, 0, r.stdout + r.stderr);
  assert.match(r.stdout, /FAKE-OJ serve -c config\.yaml/);
});

test('子进程非零退出码被传播', { skip: !TRIPLE }, () => {
  const root = makeFixture(TRIPLE);
  const r = run(root, ['bad'], { OJ_EXIT: '7' });
  assert.strictEqual(r.status, 7, r.stdout + r.stderr);
});

test('缺平台子包 → exit 1 + 提示', { skip: !TRIPLE }, () => {
  const root = makeFixture(TRIPLE, { withSub: false });
  const r = run(root, ['--version']);
  assert.strictEqual(r.status, 1);
  assert.match(r.stdout + r.stderr, /未找到平台子包/);
});
