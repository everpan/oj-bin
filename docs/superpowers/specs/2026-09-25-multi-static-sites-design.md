# 多静态站点（prefix → dir 映射）设计

日期：2026-09-25
需求：静态站点当前仅支持单个 `--app-path`，扩展为多站点。

## 决策（用户裁定）

1. 语义：**前缀→目录映射**（`/docs→dirA`、`/app→dirB`；legacy `(app_prefix, app_path)` 是单站点特例）。
2. 配置：新增 `server.static_sites: [{prefix, path}]` 列表；`app_path`/`app_prefix` 原样保留。
3. CLI：`--app-path` 可重复——裸 `dir`（至多一次，覆盖主站点 path）或 `prefix=dir`（upsert 到 static_sites）。

## 设计

### 配置（`src/config.rs`）

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct StaticSiteConf {
    /// URL 前缀（规范化见 resolve_app_prefix：首斜杠、无尾斜杠、`/` 唯一）
    pub prefix: String,
    /// 磁盘目录（相对 config 目录；CLI 给出的已按 CWD 预绝对化）
    pub path: String,
}
// ServerCfg 新增：#[serde(default)] pub static_sites: Vec<StaticSiteConf>
```

### CLI 折叠（`oj/src/server_cmd.rs::run`，App::from_config 之前）

- 裸 `dir`：>1 个 → Err；恰 1 个 → `cfg.server.app_path = abs(cwd, dir)`（现有行为）。
- `prefix=dir`：`prefix` 过 `resolve_app_prefix` 校验，`cfg.server.static_sites` 中
  同前缀条目替换、否则追加。

### 有效站点表（`oj/src/app.rs`，替代 `resolve_static_root`）

1. `app_path` Some → (normalize(app_prefix), resolve(path vs config_dir))。
2. 逐条 `static_sites` → (normalize(prefix), resolve(path vs config_dir))。
3. 规范化后**前缀重复 → Err**（报两条来源）；目录缺失/非目录 → Err（canonicalize）。
4. 按前缀长度降序排（同长度按字符串升序，确定性）——**最长前缀优先**。

### 服务侧（`server/src/lib.rs`）

- 新增 `pub struct StaticSite { pub prefix: String, pub root: PathBuf }`（Clone）。
- `AppState`：`static_root: Option<PathBuf>` + `app_prefix: String` → `static_sites: Vec<StaticSite>`。
- `app()` 签名对应替换。静态兜底分支：`static_sites.iter().find_map(|s| strip_app_prefix(&s.prefix, path).map(|r| (s, r)))`
  ——排序保证最长命中；命中站点内 miss → 该站点 SPA 回落（若开），**不跨站**。
- `strip_app_prefix`/`resolve_static`/`static_page`/`StaticOpts` 不动（`html_meta`/spa/cache 全局）。

### 准入门（`server_cmd.rs::admission_gate`）

静态存在性检查统一移到站点表解析（错误文案对齐：`server.static_sites[prefix=…]: …`
/ `server.app_path …`）；gate 只管「至少指定其一」+ api 目录存在。

## 测试（充分覆盖）

- config：static_sites 解析 round-trip、缺省为空。
- args：`--app-path` 单/多值解析（Vec）。
- server_cmd：裸多值报错、prefix=dir upsert/替换、裸+带前缀混合。
- app 装配：重复前缀 fail-fast（app_path 对 vs static_sites 同名两条）、缺失目录 fail-fast、
  最长前缀排序、CLI 覆盖同前缀。
- lib.rs：双站点最长前缀匹配、`/` 兜底、站内 miss 不跨站、per-site meta（某站无 __meta 目录
  不受影响）、spa_fallback 每站独立回落、既有静态测试全部适配新签名。
- e2e：双静态站点 config 起服，curl 各验。

## 文档（v0.1.27 正式提交）

CHANGELOG 一条；api-manual 配置章静态站点节（static_sites 语法 + 最长前缀 + 不跨站 +
前缀冲突 fail-fast 进限制表）；SKILL.md 一条（前缀重复/`/` 唯一）；dev-guide 静态兜底一句；
devkit README 行；xtask 归置 + 契约校验。

## 非目标

per-site 的 spa/meta/cache 独立配置（全局生效）；不改路由优先级；不动 WS/blob。
