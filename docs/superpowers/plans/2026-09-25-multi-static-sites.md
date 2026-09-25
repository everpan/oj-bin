# 多静态站点（prefix → dir）实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 静态站点从单 `--app-path` 扩展为前缀→目录映射的多站点（`server.static_sites` + 可重复 `--app-path prefix=dir`）。

**Architecture:** 有效站点表在装配期构建（legacy 对 + static_sites + CLI upsert），规范化 + 去重 + 按前缀长度降序；`server::AppState` 持 `Vec<StaticSite>`，请求期 `find_map(strip_app_prefix)` 取最长命中。设计文档：`docs/superpowers/specs/2026-09-25-multi-static-sites-design.md`。

**Global Constraints**

- 一律 `--release`（build/test/clippy）；`cargo fmt`；提交末尾 `unix@vip.qq.com ai`。
- 版本并入 v0.1.27（未发布）；完成后文档同步 + `cargo xtask build` 归置 + `cargo test --release -p xtask` 契约校验，以 v0.1.27 正式提交（用户要求）。
- `app_path`/`app_prefix` legacy 语义零变化（单站点配置不改动即兼容）。

---

### Task 1: config — `server.static_sites` 段

**Files:** Modify: `src/config.rs`（ServerCfg + 新结构 + Default + tests）

**Interfaces:**
- Produces: `pub struct StaticSiteConf { pub prefix: String, pub path: String }`（Debug+Clone+Deserialize）；`ServerCfg.static_sites: Vec<StaticSiteConf>`（`#[serde(default)]`）。

- [x] 结构体 + 字段 + Default `static_sites: Vec::new()`
- [x] 测试：`static_sites` YAML round-trip（两条：prefix `/docs` path `d1`、prefix `/` path `d2`）；缺省 `ServerCfg::default().static_sites.is_empty()`
- [x] `cargo test --release static_sites`（根 crate）+ commit

### Task 2: CLI — `--app-path` 可重复 + 折叠 + 准入门

**Files:** Modify: `oj/src/args.rs`、`oj/src/server_cmd.rs`（run/admission_gate + tests）

**Interfaces:**
- Consumes: Task 1 `StaticSiteConf`、`resolve_app_prefix`。
- Produces: `ServerArgs.app_path: Vec<String>`（clap 可重复）；`admission_gate(api_dir: Option<&Path>, app_specified: bool) -> Result<(), String>`（静态存在性移交装配）；`fold_cli_app_paths(cfg, entries, cwd) -> Result<(), String>`（server_cmd 内私有：裸 ≤1 → app_path；`prefix=dir` → static_sites upsert）。

- [x] args.rs：`app_path: Option<String>` → `Vec<String>`；测试 `server_defaults_and_overrides` 适配（单值 → `vec!["web"]`；新增多值用例 `--app-path a --app-path /x=b` → `["a", "/x=b"]`）
- [x] server_cmd.rs：run() 改用 fold（裸多值 Err `"--app-path <dir> (bare) may appear at most once"`；prefix 过 `resolve_app_prefix`）；admission_gate 改 `app_specified: bool`（存在性检查删除，错误文案保留 api 部分）
- [x] server_cmd tests：gate 新签名全适配；fold：裸单值/裸多值报错/prefix=dir 追加+同前缀替换/裸+带前缀混合
- [x] `cargo test --release -p oj` + commit

### Task 3: server — `StaticSite` + 服务侧多站点

**Files:** Modify: `server/src/lib.rs`（AppState/app/serve/serve_with_listener/handle/tests）

**Interfaces:**
- Produces: `pub struct StaticSite { pub prefix: String, pub root: PathBuf }`（Clone+Debug）；
  `app(...)` 与 `serve(_with_listener)(...)` 的 `static_root: Option<PathBuf>`（+`app_prefix: String`）参数 → `static_sites: Vec<StaticSite>`；`app()` 内按前缀长度降序（同长字符串升序）排序后入 AppState。
- Consumes: Task 2 完成态（本任务期间 app.rs 调用点用 legacy 单站点包装 `vec![StaticSite{prefix:"/", root}]` 保持可编译，Task 4 换真实现）。

- [x] AppState 字段替换 + 静态兜底分支 `find_map`（最长命中；命中站点 miss → 该站 spa 回落，不跨站）
- [x] 三构造器签名替换 + lib.rs 内全部测试适配（spawn_static 辅助等）
- [x] 新测试：双站点最长前缀（`/docs/api/x` 胜过 `/docs`）；`/` 兜底 catch-all；站内 miss 不跨站（A 站 miss 不回落 B 站）；per-site meta（仅 A 站有 `__meta` 目录）；spa_fallback 每站独立
- [x] `cargo test --release -p server` + commit

### Task 4: 装配 — 有效站点表（`oj/src/app.rs`）

**Files:** Modify: `oj/src/app.rs`（resolve_static_root → resolve_static_sites + from_config）

**Interfaces:**
- Consumes: Task 1-3 全部。
- Produces: `fn resolve_static_sites(cfg: &Config, config_dir: &Path) -> Result<Vec<server::StaticSite>, String>`——legacy 对 + static_sites 逐条；前缀 `resolve_app_prefix` 规范化；重复前缀 Err（含两条来源）；路径 `canonicalize`（缺失 Err）；排序移交 `app()`（此处不排）。

- [ ] 实现 + from_config 接线（`server::app(...)` 实参换 `static_sites`）
- [ ] 测试（app.rs tests）：legacy 单站点兼容（app_path+prefix 产一条）；static_sites 两条；重复前缀（app_path 对 vs static_sites 同前缀）Err 且文案含来源；缺失目录 Err
- [ ] `cargo test --release -p oj` + commit

### Task 5: e2e + 文档 + v0.1.27 正式提交

**Files:** Modify: `oj/tests/e2e.rs`、`CHANGELOG.md`、`docs/devkit/api-manual.md`、`docs/devkit/SKILL.md`、`docs/dev-guide.md`、`docs/devkit/README.md`；`cargo xtask build` 归置。

- [ ] e2e：config `server.static_sites: [{prefix:/docs,path:d1},{prefix:/,path:d2}]` 起服；curl `/docs/a.txt`→d1、`/b.txt`→d2、`/docs`（前缀根→index.html）；最长前缀用 `/docs` miss+spa 回落验证不跨站
- [ ] CHANGELOG v0.1.27 特性条；api-manual 配置章静态站点节（static_sites 语法/最长前缀/不跨站）+ 限制表（前缀重复 fail-fast、`/` 唯一）；SKILL.md 一条；dev-guide 一句；devkit README 行
- [ ] `cargo xtask build` + `cargo test --release -p xtask` + workspace 全量 + fmt + clippy
- [ ] 全部提交（以 v0.1.27 正式提交）
