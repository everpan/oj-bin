# oj serve 静态压缩 + 默认缓存 + HEAD 镜像（OJ-9/10/11）实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `oj serve` 静态层零配置获得 br/gzip 按需压缩（产物落盘缓存）、两档制默认缓存策略（哈希→immutable / 其余→no-cache），API 路由 HEAD 镜像 GET；版本 bump v0.1.116。

**Architecture:** 全部静态改动收口在 `serve/src/static_files.rs`（`serve_web` 两个调用点 `lib.rs:575/768` 已收口，无第三静态入口）；`serve_web` 改 stat 先行、原文懒读；压缩/缓存键用已有依赖 `ring`（SHA1）+ 新增 `brotli`/`flate2`；OJ-11 在 `lib.rs` 查表 405 腿做二次 lookup 镜像，`dispatch.rs` 出口空化。

**Tech Stack:** Rust (edition 2024)、axum 0.8、tokio（spawn_blocking / Semaphore / Notify）、`brotli = "9"`、`flate2 = "1"`、`ring::digest`（SHA1）。

**Spec:** `docs/superpowers/specs/2026-11-02-serve-static-compression-cache-design.md`（本计划从 spec 论证；决策编号 D1-D18、§4 代码级设计、§6 测试清单以 spec 为准）

## Global Constraints

- 全程 `--release`：`cargo test --release`、`cargo clippy --release --workspace --all-targets -- -D warnings`、`cargo fmt --check`；禁止 debug 构建。
- 测试新文件走 house 模式：`#[cfg(test)] #[path = "xxx_tests.rs"] mod tests;`（参考 `serve/src/lib.rs:783-786`）。
- 硬排除地板集（列出也不压）：`["png","jpg","jpeg","gif","webp","avif","ico","pdf","woff2"]`。
- 内置可压缩集：`["html","htm","css","js","mjs","json","map","txt","md","svg","xml","yaml","yml","woff","ttf"]`。
- 常量值：`compress_min_length` 默认 1024；`brotli_quality` 默认 5（合法 1..=11）；`gzip_level` 默认 6（合法 1..=9）；br lgwin=22；immutable=`"public, max-age=31536000, immutable"`；no-cache=`"no-cache"`。
- 配置键挂 `server.static:` 子段（serde rename，`static` 是关键字）；`html_cache_control` 等既有键原地不动。
- 缓存键格式：`{sha1_hex(abs_path)}-{ino:x}-{mtime_ns:x}-{len:x}-{algo}{q}`（algo `br`/`gz`，q 即 quality 数字）。
- off/none 时响应逐字节同现状：不出现 `Vary`/`Content-Encoding`/`Cache-Control`。
- 版本：`oj/Cargo.toml` `0.1.115` → `0.1.116`（根 Cargo.toml 的 `only-js 0.1.3` 不动）。

## Review Focus

spec 之外最可能咬人的五类输入（每条已钉进对应任务的测试）：

1. **未哈希文件误钉 immutable**：`config-production.js`（分隔段≥8 字符但纯字母）——期望落 `no-cache`。→ Task 5 测试 `default_cache_control_rejects_letter_only_segment`。
2. **可压资源 identity 200 缺 Vary**（CDN 把明文按 URL 单键缓存再投给 br 客户端）——期望凡协商管线跑过（无论最终编码）一律有 Vary。→ Task 4 测试 `compressible_identity_still_varies`。
3. **HTML 路由间串内容**：同 index.html 两个深链接各注入不同 meta——期望互不污染且不落缓存。→ Task 5 测试 `spa_fallback_meta_not_cross_contaminated`。
4. **弱比较缺失**：客户端回传 `If-None-Match: W/"x"`——期望命中 304。→ Task 4 测试 `weak_etag_inm_hits_304`。
5. **镜像透传 "HEAD"**：按方法设规则的鉴权会让公共 GET 端点无 token 探针吃 401——期望 `guard.verify`/`RequestInfo.method` 均见 `"GET"`、返回 200 空体。→ Task 6 测试 `mirror_head_public_endpoint_200_empty`。

---

### Task 1: `server.static:` 配置面（src/config.rs）

**Files:**
- Modify: `src/config.rs`（`ServerCfg` 字段区，`html_cache_control`（:189）附近）
- Test: `src/config_tests.rs`（已存在，`#[path]` 挂载于 config.rs:1693）

**Interfaces:**
- Produces: `pub struct StaticConf`，字段与默认值如下（后续 Task 2 消费）：

```rust
#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct StaticConf {
    pub compression: bool,           // 默认 true（fn default_true 已在 :250 使用）
    pub compress_min_length: u64,    // 默认 1024
    pub compress_types: Vec<String>, // 默认空 = 内置集；非空 = 全量替换（不含地板集）
    pub brotli_quality: u8,          // 默认 5
    pub gzip_level: u8,              // 默认 6
    pub compress_cache_dir: String,  // 默认 ""（空串语义两用：见下）
    pub web_cache_policy: String,    // 默认 "auto"；合法 "auto"|"none"，装配期校验
}
impl Default for StaticConf { /* 上表默认值；compress_cache_dir 默认 "" */ }
```

`ServerCfg` 增字段（`static` 是关键字，serde rename）：

```rust
/// 静态层压缩与默认缓存策略（v0.1.116 OJ-9/10）。
#[serde(default, rename = "static")]
pub static_conf: StaticConf,
```

`compress_cache_dir` 语义（doc 注释写明）：未配置（Default `""`）= `<config_dir>/compress-cache`；显式 `""` = 关闭落盘纯内存直压；其余 = 目录路径，相对 → 相对 config 目录（`logs_dir` 同款解析，解析在装配层 oj 侧做，config 层存原文）。

- [ ] **Step 1: 写失败测试**（config_tests.rs）

```rust
#[test]
fn static_conf_defaults_and_parse() {
    let c: ServerCfg = serde_yaml::from_str("timeout: 30s\npool_size: 1\nlogs_max_m: 10\nlogs_keep_files: 2\nstatic:\n  web_cache_policy: none\n  brotli_quality: 9").unwrap();
    assert!(c.static_conf.compression);
    assert_eq!(c.static_conf.compress_min_length, 1024);
    assert_eq!(c.static_conf.brotli_quality, 9);
    assert_eq!(c.static_conf.web_cache_policy, "none");
    // 整段缺席 = 全默认
    let c2: ServerCfg = serde_yaml::from_str("timeout: 30s\npool_size: 1\nlogs_max_m: 10\nlogs_keep_files: 2").unwrap();
    assert_eq!(c2.static_conf, StaticConf::default());
}
```

（`ServerCfg` 反序列化需要的必填键以 config_tests.rs 现有用例为准，照抄其最小 YAML。）

- [ ] **Step 2: 跑测试确认失败**：`cargo test --release static_conf_defaults_and_parse` → 编译失败（无 `static_conf` 字段）。
- [ ] **Step 3: 实现** `StaticConf` + `ServerCfg` 字段 + doc 注释（含 compress_cache_dir 三态语义、地板集说明）。
- [ ] **Step 4: 跑测试通过**：`cargo test --release static_conf_defaults_and_parse`。
- [ ] **Step 5: Commit**：`feat(config): server.static 子段——静态压缩与默认缓存策略配置（OJ-9/10）`

### Task 2: serve 侧选项结构 + 装配 + 启动 INFO

**Files:**
- Modify: `serve/src/lib.rs`（`StaticOpts` :132 区）、`serve/Cargo.toml`、`oj/src/app.rs`（:2235 `StaticOpts{...}`）、`oj/src/app_tests.rs`
- Test: `oj/src/app_tests.rs`

**Interfaces:**
- Consumes: Task 1 `StaticConf`。
- Produces（Task 3-6 消费）:

```rust
// serve/src/lib.rs
#[derive(Clone, Default, PartialEq, Debug)]
pub enum CachePolicy { #[default] Auto, None_ }   // None_ 避关键字

#[derive(Clone, Debug)]
pub struct StaticLayerOpts {
    pub compress_min_length: u64,
    /// None = 内置集；Some = 用户替换集（构造时已剔除地板集并小写化）
    pub compress_types: Option<std::collections::HashSet<String>>,
    pub brotli_quality: u8,
    pub gzip_level: u8,
    /// None = 纯内存直压；Some = 缓存目录（已解析为绝对路径）
    pub cache_dir: Option<std::path::PathBuf>,
}
// StaticOpts 增两字段（有 Default derive，向后兼容）：
pub static_layer: Option<StaticLayerOpts>,
pub web_cache_policy: CachePolicy,
// static_files.rs 侧判可压：
pub(crate) fn is_compressible(ext: &str, len: u64, opts: &StaticLayerOpts) -> bool
```

- [ ] **Step 1: 写失败测试**（app_tests.rs，仿既有装配断言风格）：构造含 `server.static.brotli_quality: 12` 的 cfg → 装配函数返回错误（fail-fast 文案含 "brotli_quality"）；`gzip_level: 0` 同理；`web_cache_policy: auto` + compression on + 有 web 挂载 → `StaticOpts.static_layer` 为 Some 且 `cache_dir` 为绝对路径。
- [ ] **Step 2: 跑测试失败**：`cargo test --release -p oj --lib static_layer` → 编译失败。
- [ ] **Step 3: 实现**：
  - `serve/Cargo.toml` 增 `brotli = "9"`、`flate2 = "1"`；
  - `StaticOpts` 增字段 + `CachePolicy`；
  - `app.rs` 装配：校验 brotli_quality 1..=11 / gzip_level 1..=9（越界 fail-fast，错误信息含键名与合法范围）；`compress_types` 非空 → `Some(HashSet)`（小写化、剔除地板集）；`web_cache_policy` 字符串解析（非法 fail-fast）；`compress_cache_dir` 按 Default(config_dir/compress-cache)/`""`(None)/路径三态解析（相对 → 相对 config 目录，与 `logs_dir` 装配同款，grep `logs_dir` 的现有解析点照抄）；目录创建失败不 panic（Task 4 运行期 fail-open）；
  - 启动 INFO 一行：存在 web 挂载且 static_layer=Some 时 `tracing::info!`（生效项 + 关闭键 `server.static.compression/web_cache_policy`；CachePolicy::None_ 时不提缓存策略）。
- [ ] **Step 4: 跑测试通过**。
- [ ] **Step 5: Commit**：`feat(serve): StaticLayerOpts/CachePolicy 装配 + 启动 INFO（OJ-9/10）`

### Task 3: 协商解析 + 压缩内核（纯函数层）

**Files:**
- Modify: `serve/src/static_files.rs`
- Create: `serve/src/static_files_tests.rs`（static_files.rs 尾部挂 `#[cfg(test)] #[path = "static_files_tests.rs"] pub(crate) mod tests;`）
- Test: 同上

**Interfaces:**
- Consumes: Task 2 `StaticLayerOpts`/`is_compressible`。
- Produces（Task 4 消费）:

```rust
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Encoding { Br, Gzip, Identity }
/// Accept-Encoding 协商：get_all 合并、token 小写化、`;q=` 解析（缺省 1.0，q=0 排除）；
/// 仅认显式 br/gzip；`*` 视为仅提供 gzip；坏头/无头 → Identity；br>gzip 固定优先（spec D16）。
pub(crate) fn negotiate_encoding(headers: &HeaderMap) -> Encoding
/// 压缩；返回 None = 压缩后 ≥ 原文（调用方出 identity）。br: quality/lgwin=22；gzip: level。
pub(crate) async fn compress_body(enc: Encoding, body: Vec<u8>, opts: &StaticLayerOpts) -> Option<Vec<u8>>
/// 已有 `if_none_match_hit(headers, etag)` 改造：两侧剥 `W/` 后比较 opaque（弱比较，spec D7）。
```

- [ ] **Step 1: 写失败测试**（协商表驱动，spec §6.1/6.2）：
  - `negotiate_encoding_priority`：`br, gzip`→Br；`gzip`→Gzip；`br;q=0, gzip`→Gzip；无头/`*`→Gzip（`*` 视为 gzip）；`identity`→Identity；`BR; q=0.5`（大小写+空格）→Br；坏字节头→Identity；多值头（两次插入该头）合并生效。
  - `compress_body_roundtrip_and_shrink`：3.5MB 可压文本 → Br/Gzip 均有体且更小（断言 `out.len() < body.len()/2`）；高熵随机 512B → `compress_body` 返回 None（min_length 不在此函数管，构造≥min_length 的高熵体）。
  - `if_none_match_weak_compare`：`W/"abc"` vs etag `"abc"` 命中；`"abc"` vs `"abc"` 命中；`*` 命中；`"abd"` 不命中。
- [ ] **Step 2: 跑失败**：`cargo test --release -p serve negotiate` → 编译失败。
- [ ] **Step 3: 实现**：`negotiate_encoding`（算法：`headers.get_all(ACCEPT_ENCODING)` 逐值逗号分段 → 每段 split_once(';') → token trim+小写 → q 解析 `q_v.trim().strip_prefix("q=").and_then(parse::<f32>)` 缺省 1.0 → 收集 (name, q)；q>0 的 br/gzip 存在性判定 → Br 优先）。`compress_body` 内部 `tokio::task::spawn_blocking` + `brotli::CompressorWriter`（quality, 4096 buffer, lgwin 22）/ `flate2::write::GzEncoder::new(Compression::new(level))`。`if_none_match_hit` 改弱比较。
- [ ] **Step 4: 跑通过**：`cargo test --release -p serve`。
- [ ] **Step 5: Commit**：`feat(serve): Accept-Encoding 协商 + br/gzip 压缩内核 + If-None-Match 弱比较`

### Task 4: `serve_web` 重构——stat 先行、缓存目录、single-flight、统一头组装

**Files:**
- Modify: `serve/src/static_files.rs`（`serve_web` :4、`static_page` :103、`file_response` :280 区域）
- Test: `serve/src/static_files_tests.rs`

**Interfaces:**
- Consumes: Task 2/3 全部。
- Produces（Task 5 消费）:

```rust
/// 缓存键：{sha1_hex}-{ino:x}-{mtime_ns:x}-{len:x}-{br|gz}{q}；sha1 用 ring::digest（SHA1_FORCED）。
fn cache_key(file: &Path, meta: &std::fs::Metadata, enc: Encoding, opts: &StaticLayerOpts) -> String
/// 默认缓存策略兜底写入口（Task 5 实现体，此处仅声明调用点）：
fn apply_default_cache_control(r: &mut Response, file: &Path, policy: CachePolicy)
/// 统一变体/缓存头：可压资源（is_compressible==true 的腿）一律 merge Vary（spec D11；
/// 既有 Vary 读-合并-写回，值为 `*` 时独占不追加）；压缩体加 Content-Encoding + 显式
/// Content-Length + ETag 弱化。
fn finalize_headers(r: &mut Response, compressible: bool, compressed: bool, etag: Option<&str>)
```

`serve_web` 新骨架（保持签名不变；`static_page` 增参 `opts: Option<&StaticLayerOpts>`）：

1. 405 门（现状保留）→ `resolve_static` 命中 → `tokio::fs::metadata` 先行（ETag/len/ino 数据源；metadata 失败走原 404）。
2. `compressible = 非HTML && is_compressible(ext, len, opts)`；`enc = negotiate_encoding(headers)`。
3. **非 HTML 且 compressible 且 enc≠Identity 且 opts.cache_dir 已配**：`cache_key` 查缓存 → 命中：读缓存文件出体（不读原文）；未命中：single-flight（`static INFLIGHT: LazyLock<Mutex<HashMap<String, Arc<Notify>>>>`——先查缓存，miss 后注册/等待 Notify，成为压缩者时 `COMPRESS_SEM: LazyLock<Semaphore>`（permits = `std::thread::available_parallelism()`）限流）→ 读原文 → `compress_body` → Some：`cache_store`（tmp 写 + rename；Windows rename 前先 remove 旧目标；写失败静默放弃）+ 删同 `{sha1}-` 前缀旧键；None：出 identity。cache_dir 未配：同样 single-flight + 直压不落盘。
4. **HTML 腿（含 SPA 回落）**：维持 `static_page` meta 注入路径，注入后**内存直压**（不走缓存/伴生，spec D3）。
5. **304 腿**：命中 `if_none_match_hit` → 304 + ETag + Vary（compressible 时）+ Cache-Control（经 `apply_default_cache_control`，Task 5）。
6. 出体统一 `finalize_headers` → `apply_custom_headers`（显式头先写）→ `apply_default_cache_control` 兜底（Task 5）。
7. 压缩生效处 `tracing::debug!(target: "serve::static", algo, from, to)`。

- [ ] **Step 1: 写失败测试**（static_files_tests.rs；fixture：tempdir 仿 `lib_tests.rs::dummy_app_state`，`StaticOpts{ static_layer: Some(StaticLayerOpts{..cache_dir: Some(temp)}, ..)}`，写 `index.html`/`app-B2R7x9cW.js` 进 web root，直接调 `serve_web(&st, &mount, rel, "GET", ..)` 断言响应头与体；`tokio::test(current_thread)`）：
  - `br_negotiation_serves_compressed_with_vary`：`Accept-Encoding: br` → `Content-Encoding: br`、`Content-Length`=压缩后、ETag `W/` 前缀、`Vary: Accept-Encoding`、缓存目录出现 1 个文件。
  - `cache_hit_skips_recompress`：同请求发两次（第二次前把缓存文件 mtime 保留、替换内容为哨兵字节序列可识别）→ 第二次响应体 == 哨兵（证明读缓存不读原文）。
  - `compressible_identity_still_varies`：`Accept-Encoding: identity` → 无 `Content-Encoding`、强 ETag、**有 `Vary`**。
  - `weak_etag_inm_hits_304`：先 GET 拿弱 ETag → 带 `If-None-Match: <弱值>` 再请求 → 304 且带 Vary。
  - `small_file_no_vary_no_encoding`：100 字节 js（< min_length）→ 无 Vary 无 Content-Encoding。
  - `off_switch_byte_identical`：`static_layer: None` → 无 Vary/Content-Encoding/Cache-Control、强 ETag、原体。
  - `html_compressed_but_never_cached`：GET `index.html`（配 meta 注入）→ Content-Encoding 存在、缓存目录无新增文件。
  - `touch_invalidates_cache`：touch 原文件 → 旧缓存键不被命中（缓存目录出现第二个文件）。
- [ ] **Step 2: 跑失败**：`cargo test --release -p serve static_files_tests` → 编译失败。
- [ ] **Step 3: 实现**（骨架如上；`tokio::fs` 全程；single-flight 是全 plan 唯一并发件——压缩者持 `Semaphore` permit 压缩并 store，等待者 `Notify::notified` 后重查缓存）。
- [ ] **Step 4: 跑通过** + **Step 5: Commit**：`feat(serve): serve_web 按需压缩 + 落盘缓存 + single-flight + 统一变体头（OJ-9）`

### Task 5: 默认缓存策略两档制 + 304 Cache-Control

**Files:**
- Modify: `serve/src/static_files.rs`（`apply_default_cache_control` 实现体）
- Test: `serve/src/static_files_tests.rs`

**Interfaces:**
- Consumes: Task 4 调用点。
- Produces:

```rust
/// 两档制（spec D10）：文件名 stem 按 [-._] 分段，末段 ≥8 字符且**同时含数字与字母**
/// → immutable；其余（html/清单/未哈希/sw.js/深链接回落的 index.html）→ no-cache。
/// 判定一律用落盘解析文件（file 参数），绝不用深链接 rel_path。
fn default_cache_control(file: &Path) -> &'static str
```

- [ ] **Step 1: 写失败测试**（表驱动，spec §6.7）：
  - `default_cache_control_table`：`app-B2R7x9cW.js`→immutable；`main.1a2b3c4d.js`→immutable（点分 hex）；`config-production.js`→no-cache（纯字母段）；`20240101.js`→no-cache（纯数字段）；`index.html`→no-cache；`modules.json`→no-cache；`logo.png`→no-cache。
  - `spa_fallback_meta_not_cross_contaminated`：`spa:true` 挂载 + meta 目录两个路由 JSON（`/a.json`、`/b.json` 不同 title）→ HEAD 免了，分别 GET `/a`、`/b`（Accept html）→ 各自 title 正确、均 `no-cache`、体均被压缩。
  - `explicit_headers_override_default`：`WebMount.headers` 含 `Cache-Control: public, max-age=60` → 响应头为该值（兜底不触发）。
  - `html_cache_control_still_wins`：`StaticOpts.html_cache_control=Some("no-store")` → HTML 腿为 no-store。
  - `policy_none_no_cache_control_anywhere`：`web_cache_policy: None_` → 全腿无 Cache-Control（Vary/压缩不受影响）。
  - `cache_control_on_304`：304 响应带 Cache-Control（immutable 资产的 304）。
- [ ] **Step 2: 跑失败**。
- [ ] **Step 3: 实现** `default_cache_control`（stem = `file.file_stem()`；`stem.rsplit(['-','_','.'])` 取末段？——**取 stem 按 `[._-]` split 后的最后一段**，`chars().any(is_ascii_digit) && chars().any(is_ascii_alphabetic) && len>=8`；无正则依赖）。`apply_default_cache_control`：policy==None_ 直接返回；响应已有 `CACHE_CONTROL` 头直接返回；否则 insert。304 腿调用点 Task 4 已留。
- [ ] **Step 4: 跑通过**（全 serve 套件）。
- [ ] **Step 5: Commit**：`feat(serve): 两档制默认缓存策略——哈希 immutable / 其余 no-cache（OJ-10）`

### Task 6: OJ-11——API HEAD 镜像 GET + Allow + 证书门

**Files:**
- Modify: `serve/src/lib.rs`（:523 证书门、:736 MethodNotAllowed 腿、:762 dev 兜底腿）、`serve/src/dispatch.rs`（`run_route` 增参 + 出口空化）、`serve/src/routes.rs`（`methods_of`）
- Test: `serve/src/lib_tests.rs`、`oj/tests/e2e.rs`

**Interfaces:**
- Consumes: 既有 `run_route`/`admit`/`Lookup`。
- Produces:

```rust
// dispatch.rs：run_route 尾部增参 mirror_head: bool（全部既有调用点传 false）。
// run_route 内两处出口（run_capture Ok→capture_response 之后、Err(RunFail)→转 Response 之后）
// 统一过：
pub(crate) async fn apply_head_mirror(r: &mut Response) {
    // 体置空；状态码与全部头保留；Content-Length 仅当体为缓冲且 status ∉ {1xx,204}
    // 按原体长显式 set（放在 capture 写头之后，覆盖 handler 自带值）；流式（cap.stream）
    // 与 204/1xx 不写 CL。
}
// routes.rs：pub(crate) fn methods_of(&self, path: &str) -> Vec<&'static str>  // 405 Allow 用
// lib.rs 405 腿：fail_response 后插 Allow: methods_of(path) 逗号连接（dev 兜底 405 用
// 全量常量表 ["DELETE","GET","HEAD","OPTIONS","PATCH","POST","PUT"]）。
```

- [ ] **Step 1: 写失败测试**：
  - serve 侧（lib_tests.rs）：`apply_head_mirror_rules`——缓冲 200 体 5 字节 → CL=5 空体；status 204 → 无 CL；`Body::from_stream` → 无 CL 有原头。
  - e2e（oj/tests/e2e.rs，仿 e2e/ 既有 GET 路由 fixture 新增 GET-only handler 模块）：`head_mirrors_get`——`HEAD /v1/api/<fixture>` → 与 GET 同状态码、空体、`Content-Type` 同；`head_mirror_public_endpoint_200_empty`——无 token → 200 空体（Review Focus #5）；`explicit_head_wins`——fixture 显式 head() 返回 599/特殊头 → HEAD 走显式；`post_only_head_405_with_allow`——POST-only 端点 HEAD → 405 且 `Allow` 含 POST；`mirror_uses_get_semantics`——handler 回显 `http.method` → HEAD 响应头/体位含 "GET"（经显式 head 断言头 `X-Method: GET`？——fixture GET handler 把 `http.method` 写进响应头 `X-Method`）。
- [ ] **Step 2: 跑失败**：`cargo test --release -p oj --test e2e head_` 。
- [ ] **Step 3: 实现**：handle() `Lookup::MethodNotAllowed` 腿——`verb=="HEAD"` 时二次 `table.lookup(&path,"GET")`：Hit→`run_route(..., "GET", ..., mirror_head=true)`（guard/RequestInfo 天然见 GET，spec D18+架构复核）；Conflict→500 原文案；其余维持 405+Allow。dev 兜底腿——`verb=="HEAD"` 时直接按 `"get"` 链走兜底 + `mirror_head=true`（模块缺 get 同错同文案）。证书门 ：523 `verb == "GET"` → `matches!(verb, "GET" | "HEAD")`。`/blob` 腿不动（spec 已知限制）。
- [ ] **Step 4: 跑通过**：`cargo test --release -p serve` + `cargo test --release -p oj --test e2e`。
- [ ] **Step 5: Commit**：`feat(serve): API 路由 HEAD 自动镜像 GET——GET 语义贯穿/统一空化/Allow 补齐（OJ-11）`

### Task 7: 版本 bump v0.1.116 + CHANGELOG + devkit 四件 + 全门禁

**Files:**
- Modify: `oj/Cargo.toml`（version 0.1.115→0.1.116）、`CHANGELOG.md`、`docs/devkit/api-manual.md`、`docs/devkit/SKILL.md`、`docs/devkit/scenarios.md`、`docs/devkit/README.md`

- [ ] **Step 1: 版本**：`oj/Cargo.toml` version → `0.1.116`；`grep -rn "0\.1\.115" --include="*.md" --include="*.toml" --include="*.yaml" .`（排除 CHANGELOG 历史段）清理引用。
- [ ] **Step 2: CHANGELOG.md** 顶部新增 v0.1.116 段（按既有格式），醒目三条行为变更：① 静态层默认压缩+缓存头（`server.static.compression/web_cache_policy` 可关）；② 默认缓存策略两档制（未哈希资产恒校验，长缓存需显式配）；③ API HEAD 不再 405（**不再是无副作用空操作**，探针成本与幂等责任）。附 `server.static:` 七键表。
- [ ] **Step 3: devkit 四件**（对齐本版用户可见变更，逐条进对应章节）：
  - `api-manual.md`：`server.static:` 七键进配置参考；静态挂载节「已知限制」行改写（压缩/ETag/Vary/默认缓存现状，删除「缓存交前置反代」旧文案）；错误/限制表补「HEAD on /blob → 404」「未哈希资产默认 no-cache」「`server.static.compression: off` 逐字节回退」。
  - `SKILL.md` 陷阱速查：未哈希资产要长缓存→`mounts[].headers`；前置缓存层必须按 `Vary: Accept-Encoding` 分键，否则 `compression: off`；探针选轻路由、按状态码族判活（401≠不可用；HEAD 非零成本）；GET 副作用会被 HEAD 探针触发。
  - `scenarios.md`：零配置 SPA 站（web+spa）获得压缩与缓存策略的照抄示例；构建期预压建议段（ojm 侧）。
  - `README.md`：版本一句话 + 特性列表。
- [ ] **Step 4: 归置与契约**：`cargo xtask build` → 确认 `bin/devkit/` 同步；`cargo test --release -p xtask`（devkit 契约用例绿）。
- [ ] **Step 5: 全门禁**：`cargo fmt` → `cargo clippy --release --workspace --all-targets -- -D warnings` → `cargo test --release --workspace`。全绿后 Commit：`chore(release): v0.1.116——OJ-9 静态压缩 / OJ-10 默认缓存 / OJ-11 HEAD 镜像 + devkit 同步`

## Self-Review 记录

- 覆盖核对：spec §4.1→T1/T2；§4.2→T2；§4.3→T3/T4；§4.4→T5；§4.5→T4/T5（off 断言）；§9→T6；§8 文档义务→T7。spec §6.10（重启后命中）由 T4 `cache_hit_skips_recompress` 等价覆盖（第二实例读同目录）——e2e 级重启用例不建（进程级测试基建无先例，收益低）。
- 类型一致性：`Encoding`/`StaticLayerOpts`/`CachePolicy::None_`/`apply_head_mirror` 各任务引用与定义一致。
- 比例：计划 ≈ spec 体量，无成段实现体；唯一算法体（single-flight）以文字规格给出。
