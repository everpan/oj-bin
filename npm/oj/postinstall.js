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

function bail(msg) { // 装不上：醒目提示 + exit 0；stderr 不可用也要保证 exit 0
  try { fs.writeSync(2, `${TAG} WARN: ${msg}\n`); } catch {}
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
let initCwdReal;
let up3Real;
if (process.env.INIT_CWD) {
  try {
    initCwdReal = fs.realpathSync(process.env.INIT_CWD);
    up3Real = fs.realpathSync(up3);
  } catch {
    initCwdReal = path.resolve(process.env.INIT_CWD);
    up3Real = up3;
  }
}
if (process.env.INIT_CWD && up1 === '@oj-bin' && up2 === 'node_modules' &&
  initCwdReal !== up3Real) {
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
