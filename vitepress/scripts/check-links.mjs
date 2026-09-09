#!/usr/bin/env node
// 死链扫描：检查 src/**/*.md 里的站内链接目标是否真实存在。
// VitePress 的死链检测只覆盖它自己渲染出的链接，这里多扫一遍原始 markdown
// （尤其是被切分页拆散后遗留的相对路径），作为 `npm run verify` 的一道门禁。
//
// 用法：npm run check

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const SRC = path.resolve(HERE, '..', 'src');

function walk(dir) {
  const out = [];
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    if (entry.name === 'node_modules' || entry.name.startsWith('.')) continue;
    const p = path.join(dir, entry.name);
    if (entry.isDirectory()) out.push(...walk(p));
    else if (entry.name.endsWith('.md')) out.push(p);
  }
  return out;
}

/** 站点路由 → 实际文件（支持 /dir、/dir/、/file）。 */
function exists(route) {
  const clean = route.split('#')[0].replace(/\/$/, '');
  const base = path.join(SRC, clean);
  return [base, `${base}.md`, path.join(base, 'index.md')].some((f) => fs.existsSync(f));
}

const problems = [];
const files = walk(SRC);

for (const file of files) {
  const text = fs.readFileSync(file, 'utf8');
  const re = /\[[^\]]*\]\(([^)\s]+)\)/g;
  let m;
  while ((m = re.exec(text)) !== null) {
    const target = m[1];
    if (/^(https?:|mailto:|#)/.test(target)) continue;
    const abs = target.startsWith('/')
      ? target
      : '/' + path.relative(SRC, path.resolve(path.dirname(file), target)).split(path.sep).join('/');
    if (!exists(abs)) problems.push(`${path.relative(SRC, file)} → ${target}`);
  }
}

if (problems.length) {
  console.error(`[check] 发现 ${problems.length} 条死链：`);
  for (const p of problems) console.error(`  - ${p}`);
  process.exit(1);
}
console.log(`[check] ${files.length} 篇页面，无死链。`);
