# oj serve 静态压缩 + 默认缓存 + HEAD 镜像（OJ-9/10/11）实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `oj serve` 静态层零配置获得 br/gzip 按需压缩（产物落盘缓存）、两档制默认缓存策略（哈希→immutable / 其余→no-cache），API 路由 HEAD 镜像 GET；版本 bump v0.1.116。

**Architecture:** 全部静态改动收口在 `serve/src/static_files.rs`（`serve_web` 两个调用点 `lib.rs:575/768` 已收口，无第三静态入口）。四层职责分离（SRP）：配置解析（config.rs）→ 选项装配（app.rs）→ 压缩/缓存/策略内核（static_files.rs 纯函数与仓库层）→ `serve_web` 编排。压缩/缓存键用已有依赖 `ring`（SHA1）+ 新增 `brotli`/`flate2`；OJ-11 在 `lib.rs` 查表 405 腿做二次 lookup 镜像，`dispatch.rs` 出口空化。

**Tech Stack:** Rust (edition 2024)、axum 0.8、tokio（spawn_blocking / Semaphore / Notify）、`brotli = "9"`、`flate2 = "1"`、`ring::digest`（SHA1）。

**Spec:** `docs/superpowers/specs/2026-11-02-serve-static-compression-cache-design.md`

**执行方式（用户已定）:** Native——本会话按序实施 + 终局全分支独立复审。

## 任务跟踪

| # | 任务 | 状态 | 提交 | 小结 |
|---|---|---|---|---|
| T1 | `server.static:` 配置面 | ☐ 待做 | — | — |
| T2 | 选项装配 + 启动 INFO | ☐ 待做 | — | — |
| T3 | 协商 + 压缩内核（纯函数） | ☐ 待做 | — | — |
| T4 | 压缩缓存仓库层（key/存储/single-flight） | ☐ 待做 | — | — |
| T5 | `serve_web` 组装 + 统一头 | ☐ 待做 | — | — |
| T6 | 两档制默认缓存策略 | ☐ 待做 | — | — |
| T7 | OJ-11 HEAD 镜像 + Allow + 证书门 | ☐ 待做 | — | — |
| T8 | v0.1.116 bump + CHANGELOG + devkit + 门禁 | ☐ 待做 | — | — |

**阶段小结协议**：每任务完成时 ① 更新上表（状态/提交/一句话小结：交付面、测试数、与计划的偏差）；② 在会话中给三行以内小结。偏差（签名/文件/方案与本计划不符处）必须记入小结，终局复审以此为线索。

## 全局约束

- 全程 `--release`：`cargo test --release`、`cargo clippy --release --workspace --all-targets -- -D warnings`、`cargo fmt --check`；禁止 debug 构建。
- 测试新文件走 house 模式：`#[cfg(test)] #[path = "xxx_tests.rs"] mod tests;`（参考 `serve/src/lib.rs:783-786`）。
- 测试统一 **BDD 风格**：`// Given / When / Then` 注释三段 + 场景化函数名；共享 fixture 只定义一次（DRY，见 B1/B2）。
- **DRY 单一出处**：地板集 `COMPRESS_FLOOR`、内置集 `COMPRESS_BUILTIN`、immutable/no-cache 字符串常量只允许定义一次（static_files.rs 顶部 pub(crate) const），config.rs 文档注释引用不复制值。
- 硬排除地板集（列出也不压）：`["png","jpg","jpeg","gif","webp","avif","ico","pdf","woff2"]`。
- 内置可压缩集：`["html","htm","css","js","mjs","json","map","txt","md","svg","xml","yaml","yml","woff","ttf"]`。
- 常量值：min_length 1024；br quality 5（1..=11）；gzip level 6（1..=9）；lgwin 22；immutable=`"public, max-age=31536000, immutable"`；no-cache=`"no-cache"`。
- 配置键挂 `server.static:` 子段（serde rename，`static` 是关键字）；既有键原地不动。
- 缓存键：`{sha1_hex(abs_path)}-{ino:x}-{mtime_ns:x}-{len:x}-{br|gz}{q}`。
- off/none 逐字节现状：不出现 `Vary`/`Content-Encoding`/`Cache-Control`。
- 版本：`oj/Cargo.toml` `0.1.115` → `0.1.116`（根 Cargo.toml 不动）。

## Review Focus

spec 之外最可能咬人的五类输入（各已钉进任务场景）：

1. **未哈希文件误钉 immutable**：`config-production.js`（末段纯字母）→ 期望 `no-cache`。→ T6。
2. **可压资源 identity 200 缺 Vary**（CDN 单键缓存变体互串）→ 期望协商管线跑过即有 Vary。→ T5。
3. **HTML 路由间串内容**：同 index.html 两深链接各注入不同 meta → 期望互不污染且不落缓存。→ T6。
4. **弱比较缺失**：`If-None-Match: W/"x"` → 期望命中 304。→ T5。
5. **镜像透传 "HEAD"**：公共 GET 端点无 token 探针被 401 → 期望全程见 `"GET"`、200 空体。→ T7。

## 共享测试基建（DRY，各任务只引用不重复定义）

- **B1**（serve 单测 fixture，T3 首建、T4-T6 复用，放 static_files_tests.rs）：`fn comp_state(dir: &Path, layer: Option<StaticLayerOpts>, policy: CachePolicy) -> AppState`——tempdir web root + `WebMount{prefix:"/",root,spa:true,headers:vec![]}` + `StaticOpts{static_layer:layer, web_cache_policy:policy, ..Default}`；`async fn get(st,&mount,rel,ae:&str)->Response` 薄封装 `serve_web`。
- **B2**（配置最小 YAML，T1 建后 T2 复用）：config_tests.rs 现有用例的最小合法 cfg YAML，抽 `fn base_yaml(extra: &str) -> String`。

---

### Task 1: `server.static:` 配置面（src/config.rs）

**Files:**
- Modify: `src/config.rs`（`ServerCfg` 字段区，`html_cache_control` :189 附近）
- Test: `src/config_tests.rs`（已存在）

**Interfaces:**
- Produces: `pub struct StaticConf`（T2 消费）：

```rust
#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct StaticConf {
    pub compression: bool,           // 默认 true
    pub compress_min_length: u64,    // 默认 1024
    pub compress_types: Vec<String>, // 默认空 = 内置集；非空 = 全量替换
    pub brotli_quality: u8,          // 默认 5
    pub gzip_level: u8,              // 默认 6
    pub compress_cache_dir: String,  // 默认 ""（三态语义见下）
    pub web_cache_policy: String,    // 默认 "auto"；装配期校验 auto|none
}
// ServerCfg 增：
#[serde(default, rename = "static")]
pub static_conf: StaticConf,
```

`compress_cache_dir` 三态（doc 注释写明，值解析在 T2 装配层）：未配置(Default `""`)=`<config_dir>/compress-cache`；显式 `""`=纯内存直压；其余=目录路径（相对 → 相对 config 目录，`logs_dir` 同款）。

- [ ] **Step 1: 写失败场景**（BDD 注释风格）

```rust
#[test]
fn static_conf_defaults_and_parse() {
    // Given: base_yaml 追加 static 段（部分覆盖）
    // When: 反序列化 ServerCfg
    // Then: 覆盖键生效 + 未覆盖键取默认（compression=true, min_length=1024）
    // Given: base_yaml 不含 static 段
    // When/Then: 整段 == StaticConf::default()
}
```

- [ ] **Step 2: 跑失败**：`cargo test --release static_conf_defaults` → 编译失败。
- [ ] **Step 3: 实现** `StaticConf` + `ServerCfg` 字段 + doc 注释。
- [ ] **Step 4: 跑通过**。
- [ ] **Step 5: Commit**：`feat(config): server.static 子段——静态压缩与默认缓存策略配置（OJ-9/10）`
- [ ] **Step 6: 阶段小结**（更新跟踪表）。

### Task 2: serve 侧选项结构 + 装配 + 启动 INFO

**Files:**
- Modify: `serve/src/lib.rs`（`StaticOpts` :132 区）、`serve/Cargo.toml`、`oj/src/app.rs`（:2235 装配）
- Test: `oj/src/app_tests.rs`

**Interfaces:**
- Consumes: T1 `StaticConf`。
- Produces（T3-T7 消费）:

```rust
#[derive(Clone, Default, PartialEq, Debug)]
pub enum CachePolicy { #[default] Auto, None_ }

#[derive(Clone, Debug)]
pub struct StaticLayerOpts {
    pub compress_min_length: u64,
    /// None = 内置集；Some = 替换集（构造时小写化并剔除地板集）
    pub compress_types: Option<std::collections::HashSet<String>>,
    pub brotli_quality: u8,
    pub gzip_level: u8,
    /// None = 纯内存直压；Some = 绝对路径缓存目录
    pub cache_dir: Option<std::path::PathBuf>,
}
// StaticOpts 增：static_layer: Option<StaticLayerOpts>、web_cache_policy: CachePolicy
// static_files.rs（T3 实现体）：pub(crate) fn is_compressible(ext:&str,len:u64,opts:&StaticLayerOpts)->bool
```

- [ ] **Step 1: 写失败场景**（app_tests.rs）：`static_layer_assembly_validates`——Given `brotli_quality: 12` / `gzip_level: 0` / `web_cache_policy: fast` → 装配 fail-fast（文案含键名）；Given 合法 cfg + web 挂载 + 默认 cache_dir → `static_layer` Some 且 `cache_dir` 绝对路径。
- [ ] **Step 2: 跑失败**。
- [ ] **Step 3: 实现**：依赖 `brotli = "9"`、`flate2 = "1"`；结构体；校验（越界/非法策略 fail-fast，文案含键名与合法范围）；`compress_cache_dir` 三态解析（相对 → 相对 config 目录，照抄 `logs_dir` 装配解析点）；启动 INFO（web 挂载存在且 layer=Some → 一行 `tracing::info!` 含生效项与关闭键名）。
- [ ] **Step 4: 跑通过**。
- [ ] **Step 5: Commit**：`feat(serve): StaticLayerOpts/CachePolicy 装配 + 启动 INFO（OJ-9/10）`
- [ ] **Step 6: 阶段小结**。

### Task 3: 协商解析 + 压缩内核（纯函数层）

**Files:**
- Modify: `serve/src/static_files.rs`
- Create: `serve/src/static_files_tests.rs`（static_files.rs 尾挂 `#[cfg(test)] #[path]` 模块 + **B1 fixture**）
- Test: 同上

**Interfaces:**
- Consumes: T2 `StaticLayerOpts`。
- Produces（T4/T5 消费）:

```rust
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Encoding { Br, Gzip, Identity }
/// get_all 合并、token 小写化、`;q=`（缺省 1.0、q=0 排除）；仅认显式 br/gzip；
/// `*` 视为仅提供 gzip；坏头/无头 → Identity；br>gzip 固定优先（spec D16）。
pub(crate) fn negotiate_encoding(headers: &HeaderMap) -> Encoding
/// 压缩；None = 压缩后 ≥ 原文（调用方出 identity）。spawn_blocking 内 br(lgwin 22)/gzip。
pub(crate) async fn compress_body(enc: Encoding, body: Vec<u8>, opts: &StaticLayerOpts) -> Option<Vec<u8>>
/// `if_none_match_hit` 改弱比较：两侧剥 `W/` 比 opaque（spec D7）。
/// pub(crate) const COMPRESS_FLOOR / COMPRESS_BUILTIN（全局约束单一出处）
```

- [ ] **Step 1: 写失败场景**：
  - `negotiate_priority_and_q_values`（表驱动）：`br,gzip`→Br；`gzip`→Gzip；`br;q=0, gzip`→Gzip；无头→Identity；`*`→Gzip；`identity`→Identity；`BR; q=0.5`→Br；坏字节头→Identity；**两行同头**（get_all）合并生效。
  - `compress_body_shrinks_text_not_high_entropy`：Given 3.5MB 文本 → Br/Gzip 输出 < 原文一半；Given ≥min_length 高熵体 → None。
  - `inm_weak_compare`：`W/"abc"` vs `"abc"` 命中；同值强对强命中；`*` 命中；异值不命中。
- [ ] **Step 2: 跑失败**：`cargo test --release -p serve negotiate`。
- [ ] **Step 3: 实现**（算法：`get_all` 逐值逗号分段 → `split_once(';')` → token 小写 → `q` parse::<f32> 缺省 1.0 → 收集；q>0 存在性判定 → Br 优先）。
- [ ] **Step 4: 跑通过**。
- [ ] **Step 5: Commit**：`feat(serve): Accept-Encoding 协商 + br/gzip 压缩内核 + If-None-Match 弱比较`
- [ ] **Step 6: 阶段小结**。

### Task 4: 压缩缓存仓库层（key / 存储 / single-flight）

**Files:**
- Modify: `serve/src/static_files.rs`（新增仓库段，不碰 `serve_web`）
- Test: `serve/src/static_files_tests.rs`

**Interfaces:**
- Consumes: T3 `Encoding`、T2 `StaticLayerOpts`。
- Produces（T5 消费）:

```rust
/// {sha1_hex}-{ino:x}-{mtime_ns:x}-{len:x}-{br|gz}{q}；ring::digest SHA1。
fn cache_key(file: &Path, meta: &std::fs::Metadata, enc: Encoding, opts: &StaticLayerOpts) -> String
/// 命中读缓存；未命中经 single-flight 压缩并原子落盘（tmp+rename，Windows 先 remove；
/// 写失败静默）+ 删同 `{sha1}-` 前缀旧键。dir=None → 同 single-flight 直压不落盘。
/// 压缩者受全局信号量钳流（permits = available_parallelism）。
async fn compressed_body_or_cache(
    file: &Path, meta: &std::fs::Metadata, enc: Encoding, opts: &StaticLayerOpts,
) -> Option<Vec<u8>>   // None = 压缩无收益，出 identity
```

- [ ] **Step 1: 写失败场景**：
  - `cache_key_changes_with_invalidation_inputs`：Given 同文件 touch 后 / 内容长度变 / algo 变 / quality 变 → 键两两不同；同输入两次 → 键相同。
  - `store_then_hit_serves_stored_bytes`：Given 缓存文件预置哨兵字节 → 调用返回哨兵（**不读原文**：原文文件删除后仍命中）。
  - `concurrent_misses_compress_once`：Given 缓存 miss + `tokio::join!` 8 个并发同键调用 → 缓存目录恰 1 个文件（重复压被 single-flight 收敛；非严格计数，防 flake 只断言文件数与全部返回一致）。
  - `new_key_prunes_same_prefix`：Given 已有旧代缓存文件 → 触发新键 store 后旧前缀文件消失。
  - `unwritable_dir_falls_back_identity_path`：Given cache_dir 指向只读路径 → 返回 Some(压缩体)（内存直压）不 panic。（`compress_body` None 案例已在 T3。）
- [ ] **Step 2: 跑失败**：`cargo test --release -p serve cache_` 。
- [ ] **Step 3: 实现**：`static INFLIGHT: LazyLock<Mutex<HashMap<String, Arc<Notify>>>>`；`static COMPRESS_SEM`；流程 = 查缓存 → miss 注册/等待 → 成为压缩者：取 permit → 读原文 → `compress_body` → store+prune → notify；失败/None 一致传递。
- [ ] **Step 4: 跑通过**。
- [ ] **Step 5: Commit**：`feat(serve): 压缩缓存仓库层——失效键/single-flight/信号量/代际清理（OJ-9）`
- [ ] **Step 6: 阶段小结**。

### Task 5: `serve_web` 组装（stat 先行 / 取体优先级 / 统一头 / 304）

**Files:**
- Modify: `serve/src/static_files.rs`（`serve_web` :4、`static_page` :103、`file_response` :280 区）
- Test: `serve/src/static_files_tests.rs`

**Interfaces:**
- Consumes: T3/T4 全部。
- Produces（T6 消费）:

```rust
/// 统一变体/缓存头（200 与 304 共用，DRY）：compressible → Vary merge（既有值 `*` 独占）；
/// compressed → Content-Encoding + 显式 Content-Length + ETag 弱化。
fn finalize_headers(r: &mut Response, compressible: bool, compressed: bool, etag: Option<&str>)
/// T6 实现体；此处仅在 serve_web 出口与 304 腿插入调用点。
fn apply_default_cache_control(r: &mut Response, file: &Path, policy: CachePolicy)
```

`serve_web` 新骨架（签名不变；`static_page` 增 `opts: Option<&StaticLayerOpts>`）：
① metadata 先行（ETag/len/ino 数据源；失败 → 404）→ ② `compressible`/`enc` 判定 → ③ 非 HTML 且 compressible 且 enc≠Identity → `compressed_body_or_cache`（T4；None → identity）→ ④ HTML 腿 meta 注入后**内存直压**（不落缓存，spec D3）→ ⑤ 304 腿：弱比较命中 → 304 + ETag + Vary + Cache-Control（T6）→ ⑥ 出口序：`finalize_headers` → `apply_custom_headers`（显式头）→ `apply_default_cache_control`（兜底，T6）→ ⑦ 压缩生效 `tracing::debug!(target:"serve::static", algo, from, to)`。

- [ ] **Step 1: 写失败场景**（全部经 B1 fixture）：
  - `br_negotiation_serves_compressed_with_vary`：Given 100KB js + `Accept-Encoding: br` → 200、`Content-Encoding: br`、CL=压缩后、ETag `W/`、`Vary: Accept-Encoding`、缓存目录 +1 文件。
  - `compressible_identity_still_varies`：同文件 `Accept-Encoding: identity` → 无 Content-Encoding、强 ETag、**有 Vary**（Review Focus #2）。
  - `weak_etag_inm_hits_304`：GET 取弱 ETag → 带 INM 再请求 → 304 + Vary（Review Focus #4）。
  - `small_file_no_vary_no_encoding`：100B js → 无 Vary 无 Content-Encoding。
  - `off_switch_byte_identical`：`static_layer: None` → 无三头、强 ETag、原体。
  - `html_compressed_but_never_cached`：GET index.html（配 meta 注入）→ 有 Content-Encoding、缓存目录无新增。
- [ ] **Step 2: 跑失败**：`cargo test --release -p serve static_files_tests`。
- [ ] **Step 3: 实现**（骨架如上；全程 `tokio::fs`）。
- [ ] **Step 4: 跑通过**（serve 全套件）。
- [ ] **Step 5: Commit**：`feat(serve): serve_web 按需压缩组装——stat 先行/懒读/统一变体头/304（OJ-9）`
- [ ] **Step 6: 阶段小结**。

### Task 6: 两档制默认缓存策略

**Files:**
- Modify: `serve/src/static_files.rs`（`default_cache_control` 实现体）
- Test: `serve/src/static_files_tests.rs`

**Interfaces:**
- Consumes: T5 调用点。
- Produces:

```rust
/// 两档制（spec D10）：文件 stem 按 [-._] 分段，**末段** ≥8 字符且同时含数字与字母
/// → immutable；其余一律 no-cache。判定只用落盘文件（file），绝不用深链接 rel_path。
fn default_cache_control(file: &Path) -> &'static str
```

- [ ] **Step 1: 写失败场景**：
  - `default_cache_control_two_tier_table`：`app-B2R7x9cW.js`→immutable；`main.1a2b3c4d.js`→immutable；`config-production.js`→no-cache（Review Focus #1）；`20240101.js`→no-cache（纯数字）；`index.html`/`modules.json`/`logo.png`→no-cache。
  - `spa_fallback_meta_not_cross_contaminated`（Review Focus #3）：Given spa 挂载 + meta 目录 `/a.json`、`/b.json` 不同 title → GET `/a`、`/b` → 各自 title、均 no-cache、均有压缩。
  - `explicit_mount_headers_override_default`：`WebMount.headers` 含 `Cache-Control: public, max-age=60` → 响应为该值。
  - `html_cache_control_still_wins`：`html_cache_control: "no-store"` → HTML 腿 no-store。
  - `policy_none_no_cache_control`：`CachePolicy::None_` → 全腿无 Cache-Control（Vary 不受影响）。
  - `cache_control_on_304`：immutable 资产 304 带 Cache-Control。
- [ ] **Step 2: 跑失败**。
- [ ] **Step 3: 实现**（无正则依赖：stem 按 `[._-]` split 取末段判 `len>=8 && 含数字 && 含字母`）；`apply_default_cache_control`：policy None_/已有头 → 直接返回，否则 insert。
- [ ] **Step 4: 跑通过**。
- [ ] **Step 5: Commit**：`feat(serve): 两档制默认缓存策略——哈希 immutable / 其余 no-cache（OJ-10）`
- [ ] **Step 6: 阶段小结**。

### Task 7: OJ-11——API HEAD 镜像 GET + Allow + 证书门

**Files:**
- Modify: `serve/src/lib.rs`（:523 证书门、:736 MethodNotAllowed 腿、:762 dev 兜底腿）、`serve/src/dispatch.rs`（`run_route` 尾参 + 出口空化）、`serve/src/routes.rs`（`methods_of`）
- Test: `serve/src/lib_tests.rs`、`oj/tests/e2e.rs`

**Interfaces:**
- Produces:

```rust
// dispatch.rs：run_route 尾增参 mirror_head: bool（既有调用点全传 false）。两处出口
// （Ok→capture_response 后、Err(RunFail)→转 Response 后）统一过：
pub(crate) async fn apply_head_mirror(r: &mut Response)
// 体置空、状态码与头全保留；CL 仅当缓冲体且 status ∉ {1xx,204} 按原体长显式 set
// （在 capture 写头之后，覆盖 handler 自带值）；流式与 204/1xx 不写 CL。
// routes.rs：pub(crate) fn methods_of(&self, path: &str) -> Vec<&'static str>
// lib.rs 405 腿插 Allow: methods_of 逗号连接；dev 兜底 405 用全量常量表。
```

- [ ] **Step 1: 写失败场景**：
  - serve 侧 `apply_head_mirror_rules`：Given 缓冲 200 体 5B → CL=5、体空；Given 204 → 无 CL；Given 流式体 → 无 CL、头保留。
  - e2e（oj/tests/e2e.rs 新增 GET-only fixture 模块）：
    - `head_mirrors_get`：HEAD 与 GET 同状态码、空体、同 `Content-Type`。
    - `head_mirror_public_endpoint_200_empty`（Review Focus #5）：无 token → 200 空体。
    - `mirror_uses_get_semantics`：GET handler 把 `http.method` 写入响应头 `X-Method` → HEAD 响应 `X-Method: GET`。
    - `explicit_head_wins`：fixture 显式 head() 特殊状态 → HEAD 走显式。
    - `post_only_head_405_with_allow`：405 且 `Allow` 含 POST。
    - `mirror_302_keeps_location`：302 端点 HEAD 保留 Location。
- [ ] **Step 2: 跑失败**：`cargo test --release -p oj --test e2e head_`。
- [ ] **Step 3: 实现**：`Lookup::MethodNotAllowed` 腿——`verb=="HEAD"` 二次 `lookup(path,"GET")`：Hit→`run_route(...,"GET",...,mirror_head=true)`；Conflict→500 原文案；其余 405+Allow。dev 兜底腿——`verb=="HEAD"` 折 `"get"` 链 + mirror_head=true。证书门 `:523` `verb=="GET"` → `matches!(verb,"GET"|"HEAD")`。`/blob` 腿不动。
- [ ] **Step 4: 跑通过**：`cargo test --release -p serve` + `cargo test --release -p oj --test e2e`。
- [ ] **Step 5: Commit**：`feat(serve): API HEAD 自动镜像 GET——GET 语义贯穿/统一空化/Allow（OJ-11）`
- [ ] **Step 6: 阶段小结**。

### Task 8: v0.1.116 bump + CHANGELOG + devkit 四件 + 全门禁

**Files:**
- Modify: `oj/Cargo.toml`、`CHANGELOG.md`、`docs/devkit/api-manual.md`、`docs/devkit/SKILL.md`、`docs/devkit/scenarios.md`、`docs/devkit/README.md`

- [ ] **Step 1: 版本**：`0.1.115`→`0.1.116`；`grep -rn "0\.1\.115"` 清理活引用（CHANGELOG 历史段除外）。
- [ ] **Step 2: CHANGELOG**：v0.1.116 段，醒目三行为变更（默认压缩+缓存头可关；两档制默认缓存；HEAD 镜像含副作用与探针成本警示）+ `server.static:` 七键表。
- [ ] **Step 3: devkit 四件**：`api-manual.md`（七键配置参考 + mounts 已知限制改写 + 限制表三条）；`SKILL.md`（未哈希资产长缓存需显式配；前置缓存层须按 Vary 分键否则 off；探针轻路由/状态码族判活/HEAD 非零成本）；`scenarios.md`（零配置 SPA 示例）；`README.md`（版本一句话）。
- [ ] **Step 4: 归置与契约**：`cargo xtask build` → `bin/devkit/` 同步；`cargo test --release -p xtask` 绿。
- [ ] **Step 5: 全门禁**：`cargo fmt` → `cargo clippy --release --workspace --all-targets -- -D warnings` → `cargo test --release --workspace`。全绿 Commit：`chore(release): v0.1.116——OJ-9 静态压缩 / OJ-10 默认缓存 / OJ-11 HEAD 镜像 + devkit 同步`
- [ ] **Step 6: 终局小结**（跟踪表全绿 + 偏差清单）。

## Self-Review 记录（含 SOLID/DRY 复审）

- SOLID：配置/装配/内核/仓库/编排五层单职责；`serve_web` 只编排不实现（T4/T5 拆分）；策略（T6）与机制（T4/T5）分离；对 `Encoding`/`StaticLayerOpts` 抽象依赖。
- DRY：`COMPRESS_FLOOR`/`COMPRESS_BUILTIN`/缓存策略字符串单一出处；B1/B2 fixture 各定义一次；`finalize_headers` 200/304 共用。
- 覆盖核对：spec §4.1→T1/T2；§4.2→T2；§4.3→T3/T4/T5；§4.4→T6；§4.5→T5/T6；§9→T7；§8→T8。spec §6.10（重启命中）由 T4 `store_then_hit` 等价覆盖（不建进程级重启用例——无先例、收益低）。
- 类型一致性：`Encoding`/`StaticLayerOpts`/`CachePolicy::None_`/`compressed_body_or_cache`/`apply_head_mirror` 跨任务引用一致。
- BDD：全部测试场景 Given/When/Then；函数名场景化。
