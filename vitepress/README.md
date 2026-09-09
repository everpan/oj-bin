# oj 开发者手册站点（VitePress）

把仓库 `docs/` 与 `sample/` 里的文档编成面向新人的开发者手册。

包管理器：**pnpm**（`package.json` 已声明 `packageManager`，别用 npm 混装）。

```bash
pnpm install         # 首次
pnpm run dev         # 本地预览 http://localhost:5173
pnpm run build       # 构建到 .vitepress/dist
pnpm run sync        # 从 docs/ 与 sample/ 重新同步文档
pnpm run check       # 死链扫描
pnpm run verify      # sync + check + build 一条龙（CI 用）
```

## 目录约定

| 路径 | 内容 | 能否手改 |
|---|---|---|
| `src/guide/` | 新人学习路径（12 篇，本站原创） | ✅ 改这里 |
| `src/index.md` | 首页 | ✅ |
| `src/appendix/` | 术语表、未收录文档索引 | ✅ |
| `src/reference/`、`src/modules/`、`src/sample/` | 由脚本从 `docs/`、`sample/` 同步生成 | ❌ 改源文档后 `pnpm run sync` |

> 生成产物**不入库**（`.gitignore` 已排除 `src/reference/`、`src/modules/`、`src/sample/`、
> `.vitepress/generated/`）。克隆后 `pnpm run dev` / `pnpm run build` 会经 `predev` / `prebuild`
> 自动先同步一次，不需要手工补。

**生成页顶部都标了来源与生成日期**，看到「本页由脚本从 `docs/xxx.md` 同步生成」就别手改。

## 收录规则

- 只收描述**当前实现**的文档；评审记录、历史设计稿、归档不进站点，改由
  `src/appendix/history-index.md` 指路。
- 超长文档（> 600 行）按二级标题自动切页：`src/reference/<doc>/index.md` + `NN.md`，
  sidebar 由 `scripts/sync-docs.mjs` 生成的 `.vitepress/generated/sidebar.mjs` 驱动。
- 收录清单是脚本里的 `ENTRIES` 白名单（**不递归目录**，`sample/unit/node_modules/`
  下有大量第三方 README）。新增文档请先登记到白名单。

## 已知处理

- 中文搜索：minisearch 默认按空格分词对中文无效，config 里自定义了「单字 + 二元组」切词。
- 代码块语言：`cli` / `bat` / `cmd` / `sh` 统一映射为 `bash`（shiki 不认 `cli`）。
- 裸标签转义：正文里 `<config_dir>` / `Promise<T>` 这类占位符会被 Vue 模板编译器当成
  未闭合元素，脚本在代码块之外统一转义。
- `http://localhost:9778` 的示例链接是给读者本地跑的，构建时按 `ignoreDeadLinks` 例外放行。
