#!/usr/bin/env node
// 把仓库里的权威文档同步进 VitePress 站点（docs/vitepress/src）。
//
// 设计要点：
// - 白名单显式登记（ENTRIES），绝不递归目录 —— sample/unit/node_modules 下有大量第三方 README。
// - 超长文档（> SPLIT_LINES 行）按二级标题切分成目录 + 子页，sidebar 数据一并生成。
// - 内部相对链接按「源相对路径 → 仓库绝对路径 → 站点路由」两级映射，统一改成站点绝对路径。
// - 生成物头部写明来源；禁止手改 src/ 下的生成页，要改就改源文件再跑 `npm run sync`。
//
// 用法：npm run sync

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const SITE = path.resolve(HERE, '..'); // docs/vitepress
const ROOT = path.resolve(SITE, '../..'); // 仓库根
const SRC = path.join(SITE, 'src');

/** 超过这个行数就按 `## ` 切页。 */
const SPLIT_LINES = 600;

/**
 * 收录清单：src 为仓库相对路径，route 为站点路由（split 时该路由是目录）。
 * group 仅用于生成 sidebar 时的分组提示。
 */
const ENTRIES = [
  // ---- 参考（docs/ 顶层，描述当前实现）----
  { src: 'docs/dev-guide.md', route: '/reference/dev-guide', title: '开发指南', split: true },
  { src: 'docs/user-manual.md', route: '/reference/user-manual', title: '用户手册', split: true },
  { src: 'docs/ops-manual.md', route: '/reference/ops-manual', title: '运维手册' },
  { src: 'docs/testing.md', route: '/reference/testing', title: '测试手册' },
  { src: 'docs/migration.md', route: '/reference/migration', title: '数据迁移' },
  { src: 'docs/bridge.md', route: '/reference/bridge', title: 'bridge 与全局对象' },
  { src: 'docs/websocket.md', route: '/reference/websocket', title: 'WebSocket' },
  { src: 'docs/mq-tasks.md', route: '/reference/mq-tasks', title: 'MQ 与长任务' },
  { src: 'docs/oidc-integration.md', route: '/reference/oidc-integration', title: 'OIDC 接入' },
  { src: 'docs/oidc-implementation.md', route: '/reference/oidc-implementation', title: 'OIDC 实现' },
  { src: 'docs/plugin-development.md', route: '/reference/plugin-development', title: '插件开发' },
  { src: 'docs/benchmarks.md', route: '/reference/benchmarks', title: '基准测试' },
  { src: 'docs/builtin-api-auth.md', route: '/reference/builtin-api-auth', title: '内置 API 与鉴权' },

  // ---- JS 业务开发者手册（devkit，随发行包交付）----
  { src: 'docs/devkit/api-manual.md', route: '/reference/api-manual', title: 'JS API 手册', split: true },
  { src: 'docs/devkit/SKILL.md', route: '/reference/devkit-skill', title: 'oj 开发 skill' },

  // ---- 模块地图（维护者视角）----
  { src: 'docs/modules/README.md', route: '/modules/index', title: '模块地图' },
  { src: 'docs/modules/00-overview.md', route: '/modules/00-overview', title: '00 · 总览' },
  { src: 'docs/modules/01-core-bridge.md', route: '/modules/01-core-bridge', title: '01 · 核心运行时' },
  { src: 'docs/modules/02-config.md', route: '/modules/02-config', title: '02 · 配置模型' },
  { src: 'docs/modules/03-server-http.md', route: '/modules/03-server-http', title: '03 · HTTP 服务' },
  { src: 'docs/modules/04-oj-cli.md', route: '/modules/04-oj-cli', title: '04 · CLI 与装配' },
  { src: 'docs/modules/05-ffi-and-plugins.md', route: '/modules/05-ffi-and-plugins', title: '05 · FFI 与插件' },
  { src: 'docs/modules/06-toolchain.md', route: '/modules/06-toolchain', title: '06 · 工具链' },
  { src: 'docs/modules/07-data-layer.md', route: '/modules/07-data-layer', title: '07 · 模块数据层' },
  { src: 'docs/modules/08-testing.md', route: '/modules/08-testing', title: '08 · 测试体系' },

  // ---- sample 实战导览 ----
  { src: 'sample/README.md', route: '/sample/index', title: 'sample 项目' },
  { src: 'sample/MODULES.md', route: '/sample/modules-tour', title: 'sample 模块导览' },
  { src: 'sample/src/auth/README.md', route: '/sample/auth', title: 'auth 模块' },
  { src: 'sample/src/auth_demo/README.md', route: '/sample/auth-demo', title: 'auth_demo 模块' },
  { src: 'sample/src/upload/README.md', route: '/sample/upload', title: 'upload 模块' },
  { src: 'sample/src/idp/README.md', route: '/sample/idp', title: 'idp 模块（内置 OP）' },
  { src: 'sample/src/oidc/README.md', route: '/sample/oidc', title: 'oidc 模块（RP）' },
];

/** 未收录文档：指向「历史与未收录文档索引」。 */
const EXCLUDED = new Set([
  'docs/review-2026-09-02.md',
  'docs/review-2026-09-06.md',
  'docs/route-params-design.md',
  'docs/plugin-architecture.md',
  'docs/cli2.md',
]);

// ---------------------------------------------------------------- 路由表

/** 仓库相对路径 → 站点路由（切分文档指向目录，带尾斜杠）。 */
const ROUTES = new Map();
for (const e of ENTRIES) {
  const split = e.split === true;
  ROUTES.set(e.src, split ? `${e.route}/` : e.route);
}

// ---------------------------------------------------------------- 工具

const toPosix = (p) => p.split(path.sep).join('/');

/** 解析 markdown 里相对链接（相对源文件目录）→ 站点路由；解析不了返回 null。 */
function resolveLink(srcFile, target) {
  const clean = target.split('#')[0];
  if (clean === '') return null; // 纯锚点
  const abs = toPosix(path.posix.normalize(path.posix.join(path.posix.dirname(srcFile), decodeURIComponent(clean))));
  if (ROUTES.has(abs)) return ROUTES.get(abs);
  if (EXCLUDED.has(abs) || abs.startsWith('docs/archive/') || abs.startsWith('docs/superpowers/')) {
    return '/appendix/history-index';
  }
  return null;
}

/** 合并 frontmatter：已有键保留，title 由清单决定。 */
function splitFrontmatter(text) {
  if (!text.startsWith('---\n')) return [{}, text];
  const end = text.indexOf('\n---\n', 4);
  if (end === -1) return [{}, text];
  const raw = text.slice(4, end + 1);
  const body = text.slice(end + 5);
  const fm = {};
  for (const line of raw.split('\n')) {
    const m = /^([A-Za-z_][\w-]*):\s*(.*)$/.exec(line);
    if (m) fm[m[1]] = m[2].trim();
  }
  return [fm, body];
}

function toFrontmatter(obj) {
  const lines = ['---'];
  for (const [k, v] of Object.entries(obj)) lines.push(`${k}: ${v}`);
  lines.push('---');
  return lines.join('\n');
}

/** 统一代码块语言（sh/cli/bat/cmd → bash，shiki 不认 cli）。 */
function normalizeCodeFences(text) {
  return text.replace(/^```(cli|bat|cmd|sh)\s*$/gm, '```bash');
}

/**
 * VitePress 把 md 当 Vue 模板编译，正文里的 `<Uint8Array>`、`<config_dir>` 这类
 * 「看起来像标签」的占位符会被当成未闭合元素，直接让 build 失败。这里在代码块之外
 * 把非白名单标签的 `<` 转义掉（白名单里的真 HTML 标签原样保留）。
 */
const HTML_OK = new Set([
  '!--', '!doctype', 'br', 'hr', 'img', 'div', 'span', 'p', 'a', 'ul', 'ol', 'li',
  'table', 'thead', 'tbody', 'tr', 'td', 'th', 'details', 'summary', 'b', 'i', 'em',
  'strong', 'code', 'pre', 'blockquote', 'h1', 'h2', 'h3', 'h4', 'h5', 'h6',
  'center', 'small', 'kbd', 'sup', 'u', 's', 'template', 'style', 'script',
]);

function escapeBareTags(text) {
  let fence = false;
  return text
    .split('\n')
    .map((line) => {
      if (/^\s*(```|~~~)/.test(line)) {
        fence = !fence;
        return line;
      }
      if (fence) return line;
      return line.replace(/<([A-Za-z!/][A-Za-z0-9!-]*)/g, (all, name) => {
        const key = name.slice(1).toLowerCase();
        return HTML_OK.has(name.toLowerCase()) || HTML_OK.has(key) ? all : `&lt;${name}`;
      });
    })
    .join('\n');
}

/** 重写正文里的 markdown 链接与图片。 */
function rewriteLinks(text, srcFile, report) {
  return text.replace(/(\[[^\]]*\]\()([^)\s]+)(\))/g, (all, open, target, close) => {
    // 站点绝对路径（/xxx）与同目录相对路径（./index.md）是脚本自己生成的，不再改写。
    if (/^(https?:|mailto:|#|\/|\.\/)/.test(target)) return all;
    const resolved = resolveLink(srcFile, target);
    if (resolved) return `${open}${resolved}${close}`;
    report.push({ srcFile, target });
    return all;
  });
}

/** 按二级标题切片；返回 [{title, body}]，首片为序言（可能为空）。 */
function splitSections(body) {
  const lines = body.split('\n');
  const out = [];
  let cur = { title: null, lines: [] };
  for (const line of lines) {
    if (/^## /.test(line)) {
      out.push(cur);
      cur = { title: line.slice(3).trim(), lines: [] };
    } else {
      cur.lines.push(line);
    }
  }
  out.push(cur);
  return out;
}

/** 切片内标题提升一级：`# ` → `## `、`## ` → `### `（子页已有 H1）。 */
function promoteHeadings(text) {
  return text.replace(/^(#{1,5}) /gm, (_, h) => `${'#'.repeat(Math.min(h.length + 1, 6))} `);
}

/** 生成日期（YYYY-MM-DD）：写进每个生成页，便于读者判断新旧。 */
const TODAY = new Date().toISOString().slice(0, 10);

/**
 * 页面顶部可见的来源说明（含生成日期），小字号呈现 —— 见 .vitepress/theme/style.css
 * 的 `.gen-note`。用 `<p>` 而非 markdown：HTML 块内的 markdown 不会再被解析。
 */
const note = (src) =>
  `<p class="gen-note">generated: ${TODAY} · 本页由脚本从 ${src} 同步生成，` +
  `修改请改源文件后运行 npm run sync。</p>\n\n`;

const banner = (src) =>
  `<!-- 由 scripts/sync-docs.mjs 于 ${TODAY} 从 \`${src}\` 生成，请勿直接编辑；` +
  `改源文件后运行 \`npm run sync\` -->\n`;

// ---------------------------------------------------------------- 主流程

const unresolved = [];
/** 切分文档的 sidebar 数据：route → [{text, link}] */
const splitNav = {};

for (const e of ENTRIES) {
  const abs = path.join(ROOT, e.src);
  if (!fs.existsSync(abs)) {
    console.warn(`[sync] 缺失：${e.src}`);
    continue;
  }
  const raw = fs.readFileSync(abs, 'utf8');
  const [existingFm, bodyRaw] = splitFrontmatter(raw);
  const body = escapeBareTags(normalizeCodeFences(bodyRaw));

  const lineCount = body.split('\n').length;
  const doSplit = e.split === true || lineCount > SPLIT_LINES;

  if (!doSplit) {
    const text = rewriteLinks(body, e.src, unresolved);
    const fm = { ...existingFm, title: e.title, generated: TODAY };
    const out = path.join(SRC, `${e.route}.md`);
    fs.mkdirSync(path.dirname(out), { recursive: true });
    fs.writeFileSync(
      out,
      `${banner(e.src)}\n${toFrontmatter(fm)}\n\n${note(e.src)}\n${text.trim()}\n`
    );
    continue;
  }

  // 切分页：目录 + index.md + NN.md
  const sections = splitSections(body);
  const preamble = sections[0].lines.join('\n').trim();
  const parts = sections.slice(1).filter((s) => s.lines.join('\n').trim() !== '');
  const dir = path.join(SRC, e.route);
  fs.mkdirSync(dir, { recursive: true });

  const items = [];
  parts.forEach((s, i) => {
    const n = String(i + 1).padStart(2, '0');
    const file = `${n}.md`;
    items.push({ text: s.title, link: `${e.route}/${n}` });
    let text = rewriteLinks(promoteHeadings(s.lines.join('\n')), e.src, unresolved);
    text = `[← 返回《${e.title}》](./index.md)\n\n# ${s.title}\n\n${text.trim()}\n`;
    fs.writeFileSync(
      path.join(dir, file),
      `${banner(e.src)}\n${toFrontmatter({ title: s.title, generated: TODAY })}\n\n${note(e.src)}${text}\n`
    );
  });

  const toc = items.map((it) => `- [${it.text}](${it.link})`).join('\n');
  const index = [
    preamble,
    '',
    '## 章节',
    '',
    toc,
  ].join('\n');
  const fm = { ...existingFm, title: e.title, generated: TODAY };
  fs.writeFileSync(
    path.join(dir, 'index.md'),
    `${banner(e.src)}\n${toFrontmatter(fm)}\n\n${note(e.src)}${rewriteLinks(index, e.src, unresolved).trim()}\n`
  );
  splitNav[e.route] = items;
  console.log(`[sync] 切分 ${e.src} → ${e.route}/（${items.length} 页）`);
}

// 生成 sidebar 数据（供 .vitepress/config.mts 引入）
const genDir = path.join(SITE, '.vitepress', 'generated');
fs.mkdirSync(genDir, { recursive: true });
fs.writeFileSync(
  path.join(genDir, 'sidebar.mjs'),
  `// 由 scripts/sync-docs.mjs 生成，勿手改\nexport const splitNav = ${JSON.stringify(splitNav, null, 2)};\n`
);

if (unresolved.length) {
  console.warn(`\n[sync] 未解析链接 ${unresolved.length} 条（需补映射或已在正文里是纯文本）：`);
  for (const u of unresolved) console.warn(`  - ${u.srcFile}: ${u.target}`);
}
console.log(`[sync] 完成，共 ${ENTRIES.length} 篇。`);
