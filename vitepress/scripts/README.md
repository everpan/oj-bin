# 站点脚本

两个脚本，都由 `vitepress/package.json` 的 npm scripts 调用：

| 脚本 | 命令 | 作用 |
|---|---|---|
| `sync-docs.mjs` | `pnpm run sync` | 把 `docs/`、`sample/` 的文档同步进 `src/`（复制 + 切页 + 修链接 + 注入元信息） |
| `check-links.mjs` | `pnpm run check` | 扫 `src/**/*.md` 的站内链接，报告死链（有死链 exit 1） |

`pnpm run verify` = `sync` → `check` → `build`，CI 与本地提交前都跑这个。

---

## sync-docs.mjs

### 为什么是「复制」而不是「引用」

VitePress 的页面必须落在 `srcDir`（这里是 `src/`）里。让站点直接以 `docs/` 为源会把
`docs/superpowers/`（30+ 篇长文）和 `docs/archive/` 全量收进路由与搜索，也会把 `.vitepress`
塞进文档目录。所以选择**单向复制 + 白名单**，代价是存在副本，用下面三条压住漂移：

1. **白名单 `ENTRIES`**：逐个登记文件，不递归目录（`sample/unit/node_modules/` 下有大量
   第三方 README，递归必炸）。
2. **生成页不可手改**：每页顶部有
   `<p class="gen-note">generated: <日期> · 本页由脚本从 <源> 同步生成…</p>`，
   改源文件后重跑 `pnpm run sync` 即可。
3. **产物不入库**：`.gitignore` 排除了 `src/reference/`、`src/modules/`、`src/sample/`、
   `.vitepress/generated/`；`package.json` 用 `predev` / `prebuild` 钩子保证 dev/build
   前一定先同步，克隆即可用。（代价：改了源文档却忘了跑 sync，本地看不到差异 —— 所以提交
   源文档前跑一次 `pnpm run verify`。）

### 收录清单怎么改

编辑脚本顶部的 `ENTRIES`，每项四个字段：

```js
{ src: 'docs/websocket.md', route: '/reference/websocket', title: 'WebSocket' }
// split: true 可强制切页（不写则按行数自动判定）
```

`src` 是**仓库相对路径**；`route` 是站点路由（切页时它是目录）。新增一篇文档：

1. 在 `ENTRIES` 里登记；
2. 跑 `pnpm run sync`；
3. 若它是新分组，还要在 `.vitepress/config.mts` 的 `sidebar` 里加条目；
4. 跑 `pnpm run verify` 确认无死链。

**例外 —— sample 模块专题**：`sample/src/*/README.md` 无需登记，sync 时一层 glob 自动收录
（路由 `/sample/<目录名>`，标题取 README 首个 `# ` 行；要定制 title/route 才在 `ENTRIES`
显式登记，显式项优先）。全量模块清单（显式 + 自动）由 sync 写进
`.vitepress/generated/sidebar.mjs` 的 `sampleModules` 导出，config.mts 的「示例实战」
分组直接展开它——新增模块只丢一份 README.md，sync 后自动进站、自动上 sidebar。

不想收录但要留指路的文档，写进 `EXCLUDED` 集合（链接会被改写到 `/appendix/history-index`），
或在 `appendix/history-index.md` 里登记。

### 长文切页

- 阈值 `SPLIT_LINES = 600`（行数）。也可对某项显式写 `split: true` 强制切。
- 按二级标题 `## ` 切：序言进 `<route>/index.md`，其余每节一个 `NN.md`，
  节内标题提升一级（`## ` → `# `），每页顶部有返回总览的链接。
- `index.md` 底部自动生成章节目录；切页结果写进 `.vitepress/generated/sidebar.mjs`，
  由 config 引入生成 sidebar（**不要手改该文件**）。

### 顺手做的四件修补

| 处理 | 原因 |
|---|---|
| 代码块语言 `cli` / `bat` / `cmd` / `sh` → `bash` | shiki 不认 `cli`，会掉高亮 |
| 站内相对链接 → 站点绝对路径 | 文档搬到 `src/` 后相对层级全变了 |
| 正文里 `<config_dir>` / `Promise<T>` 的 `<` → `&lt;`（代码块外） | md 会被当 Vue 模板编译，裸标签 = 未闭合元素，build 直接失败 |
| 注入 frontmatter `title` / `generated` | 页面标题与生成日期；`docs/devkit/SKILL.md` 自带 frontmatter，脚本做**合并**不覆盖 |

### 链接映射怎么工作

两级映射：**源相对路径 → 仓库绝对路径 → 站点路由**。

- 已在 `ENTRIES` 里的 → 改写到对应路由（切页文档带尾斜杠，指向 `index`）。
- 在 `EXCLUDED` 或 `docs/archive/`、`docs/superpowers/` 下的 → 统一指向 `/appendix/history-index`。
- 站点绝对路径（`/xxx`）与同目录相对路径（`./index.md`）是脚本自己生成的，不改写。
- 解析不了的会**原样保留并在结尾打印清单**，人工决定补映射还是忽略。

---

## check-links.mjs

VitePress 自带的死链检测只覆盖它渲染出的链接，且不校验被切页拆散后的残留相对路径。
这个脚本直接扫 `src/**/*.md` 的 markdown 源码：

- 跳过 `http(s):`、`mailto:`、纯锚点；
- 绝对链接（`/xxx`）按 `src/xxx`、`src/xxx.md`、`src/xxx/index.md` 三种形态找；
- 相对链接按文件所在目录解析；
- 有缺失就打印 `文件 → 目标` 并 **exit 1**。

---

## 常见报错

| 报错 | 处理 |
|---|---|
| `Element is missing end tag` | 有裸 `<...>` 逃过了转义：确认它在代码块内/外，必要时把标签名加进 `HTML_OK` 白名单 |
| `Found dead link /reference/xxx` | 该文档被切页了，链接要带尾斜杠；或在 `ENTRIES` 补映射 |
| `Found dead link http://localhost:...` | 示例命令里的本地地址，config 的 `ignoreDeadLinks` 已放行 |
| `[sync] 缺失：<src>` | `ENTRIES` 里的源路径写错或文件已删除 |
