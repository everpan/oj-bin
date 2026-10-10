#!/usr/bin/env node
// @oj-bin/oj 启动器（供 `pnpm dlx @oj-bin/oj` / `npx @oj-bin/oj` 调用）。
// 主包本身不携带二进制，二进制在平台子包 @oj-bin/oj-<triple> 中。这里只负责
// 定位当前平台的子包二进制并用相同进程参数 exec 它。零依赖 CommonJS。
//
// 与 postinstall.js 共用 TRIPLES 反向表；改一边必须改另一边。
'use strict';

const { spawnSync } = require('child_process');
const path = require('path');

const TAG = '[@oj-bin/oj]';
const TRIPLES = {
  'linux-x64': 'x86_64-unknown-linux-gnu',
  'darwin-arm64': 'aarch64-apple-darwin',
  'win32-x64': 'x86_64-pc-windows-msvc',
};

const key = `${process.platform}-${process.arch}`;
const triple = TRIPLES[key];
if (!triple) {
  console.error(`${TAG} 暂无 ${key} 的预编译包（现有：${Object.keys(TRIPLES).join(', ')}）。\n` +
    `请从 https://github.com/everpan/oj-bin/releases 下载对应平台包。`);
  process.exit(1);
}

let subRoot;
try {
  subRoot = path.dirname(require.resolve(`@oj-bin/oj-${triple}/package.json`));
} catch {
  console.error(`${TAG} 未找到平台子包 @oj-bin/oj-${triple}（可能被 --omit=optional / ignore-scripts 类配置排除）。\n` +
    `请在项目内执行 npm i @oj-bin/oj 安装它，或从 releases 下载。`);
  process.exit(1);
}

const binName = process.platform === 'win32' ? 'oj.exe' : 'oj';
const binPath = path.join(subRoot, binName);
if (!require('fs').existsSync(binPath)) {
  console.error(`${TAG} 平台子包缺少二进制 ${binPath}。`);
  process.exit(1);
}

const r = spawnSync(binPath, process.argv.slice(2), { stdio: 'inherit' });
// spawnSync 失败（如二进制不可执行）时 r.error 存在、r.status 为 null。
if (r.error) {
  console.error(`${TAG} 执行 ${binPath} 失败：${r.error.message}`);
  process.exit(1);
}
process.exit(r.status == null ? 1 : r.status);
