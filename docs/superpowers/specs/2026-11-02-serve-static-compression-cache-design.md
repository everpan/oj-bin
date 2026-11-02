# 设计：`oj serve` 静态层按需压缩（OJ-9）+ web 挂载默认缓存策略（OJ-10）+ API HEAD 镜像（OJ-11）

- 日期：2026-11-02 · 状态：评审中（架构 / Web 性能 / 网络三路专家评审）
- 基线：oj v0.1.115 · 来源：oj-module 提出的 OJ-9（PR-8）/ OJ-10（PR-9）/ OJ-11（PR-10）诉求文本
- 一句话：`oj serve` 的 web 静态挂载零配置获得「br>gzip 传输压缩」与「内容哈希 immutable /
  HTML+清单 no-cache」的正确默认，`off`/`none` 两个逃生门完全回退现状。

## 1. 意图与背景

生产形态是 `oj serve` 单进程同出 API 与静态（`ojm preview` 复用同一静态层），前置 nginx
仅为了 gzip/brotli 与缓存头——配置环境相关、易遗漏、不随 `ojm build` 走。实测首屏静态产物
25MB / 123 文件（antd.js 单文件 3.5MB 未压缩出网），瓶颈在出网字节数与移动端解析。

代码现状（v0.1.115 核验）：
- 静态 body 已是**全量内存缓冲**（`serve/src/static_files.rs:45` `tokio::fs::read` →
  `Vec<u8>`），压缩 = 出响应前的纯函数变换，无需动流式架构。
- ETag 仅对非 HTML（mtime-len 强 ETag，`static_files.rs:27`）；web 挂载无 Range；
  `apply_custom_headers` 语义是**只补缺**（已有头不被覆盖）。
- 依赖：`flate2` 已在 workspace；`tower-http` 仅 cors feature。

## 2. 目标 / 非目标 / 反目标

**目标**
- G1 静态 GET/HEAD 按 `Accept-Encoding` 协商输出 br/gzip；不支持则原文。
- G2 默认缓存策略按文件特征自动施加，显式配置恒可覆盖。
- G3 零配置即正确；两个全局键（+可调 knobs）可完全回退/调优。
- G4 off/none 时响应逐字节同现状（不出现 Vary/Cache-Control）。

**非目标（明确不做）**
- 动态信封、blob、`/health`、API 腿的压缩（R5，保持现状）。
- web 挂载 Range 支持（现状保留）。
- `ojm build` **生成**预压产物（回到「随构建走」老问题）；但 oj serve **消费**站点里
  既有的伴生预压文件（D9a）。
- metrics 系统；缓存中间层；brotli lgwin 等 window 级 knob。

**反目标（出现即失败）**
- 破坏既有显式头语义（`mounts[].headers` / `html_cache_control` / `response_headers`）。
- CPU 尖峰不可控（无质量上限、无 spawn_blocking）。
- 跨编码 ETag 混淆导致的错误 304 / 错误缓存。

## 3. 已定决策

| # | 决策 | 定夺 |
|---|---|---|
| D1 | 两个特性**默认全开**，无 per-挂载配置；`prefix.web` 静态站自动享受 | 用户 |
| D2 | R4 knobs **全做**：`compress_min_length` / `compress_types` / `brotli_quality`（+`gzip_level` 对称补齐） | 用户（gzip_level 为对称性补齐，我的默认） |
| D3 | 压缩时机 = **运行时落盘缓存**：首次请求现场压（`spawn_blocking`）并原子写入缓存目录，后续请求直接命中缓存出体（不再动态压）；缓存不可用（只读盘/写失败）→ 回退纯内存直压 | 用户（2026-11-02 追加） |
| D4 | 配置面挂 `server:` 全局，两个总开关 `static_compression`(默认 on) / `web_cache_policy`(auto\|none，默认 auto) | 用户 |
| D5 | `compress_types` 用**扩展名列表**而非 MIME 串（与 `mime_of`/磁盘现实同构）；给出即**全量覆盖**内置集 | 我的默认 |
| D6 | 压缩体若**不小于**原文 → 回 identity（防负收益） | 我的默认 |
| D7 | 压缩时 ETag 弱化（`W/` 前缀）；304 路径补 `Vary` | 我的默认（正确性必需） |
| D8 | 默认缓存策略在 `apply_custom_headers` **之后**、头仍缺席时兜底写入（绕开「只补缺」语义实现 R2 显式优先） | 调研结论 |
| D9 | 压缩产物**持久化存盘**（D3 缓存目录即磁盘持久，跨重启命中）；且构建期伴生预压文件（`x.js.br`/`x.js.gz`，vite-plugin-compression 惯例）存在时**优先直接服务**（gzip_static 等价能力，允许站点用构建期 q11 极限压） | 持久化=用户 2026-11-02 追加；伴生优先=我的延伸 |

## 4. 代码级设计

### 4.1 配置面（`src/config.rs` `ServerCfg`，`html_cache_control` 邻位）

```yaml
server:
  static_compression: true      # 默认 true；false = 不协商不压缩（连 Vary 也不加）
  compress_min_length: 1024     # 默认 1024 字节；小于该长度原样
  compress_types:               # 缺省 = 内置可压缩扩展名集；给出 = 全量覆盖
    # 内置集: html htm css js mjs json map txt md svg xml yaml yml woff ttf
    # 内置排除: woff2 png jpg jpeg gif webp avif ico wasm pdf octet-stream
  brotli_quality: 5             # 默认 5，合法 1..=11，越界装配期 fail-fast
  gzip_level: 6                 # 默认 6，合法 1..=9，越界装配期 fail-fast
  web_cache_policy: auto        # auto(默认) | none；none = 不加任何 Cache-Control
  compress_cache_dir:           # 压缩缓存目录：缺省 = <config_dir>/compress-cache（开）；
                                # ""（空串）= 关闭落盘（纯内存直压）；相对 → 相对 config 目录
                                # （沿用 logs_dir 惯例）。目录不存在自动创建。
```

serde 沿用 `#[serde(default...)]` 风格；枚举解析失败/数值越界按仓库惯例 fail-fast。
缓存目录内容是**派生数据**（可随时整目录删除，删除后按需重建），写进部署文档。

### 4.2 装配（`oj/src/app.rs:2235` `StaticOpts` + `serve/src/lib.rs:132`）

`StaticOpts` 增字段：`compression: Option<CompressionOpts>`（None = off）与
`web_cache_policy: CachePolicy`（Auto | None）。`CompressionOpts` 持有 min_length /
扩展名集（预 HashSet）/ br quality / gzip level。新增 `serve/Cargo.toml` 依赖：
`brotli = "9"`、`flate2 = "1"`。

### 4.3 协商与压缩（`serve/src/static_files.rs`）

1. **协商**（`serve_web` 已持有 `headers`）：解析 `Accept-Encoding`——逗号分段，
   `token;q=N`（缺省 q=1.0），q=0 视为排除；仅认显式 `br`/`gzip`，`*` **不**视为提供
   br/gzip（保守 → identity）；解析失败 → identity。选择：`br` q>0 优先，其次 `gzip`
   q>0，否则 identity（R1 的 br>gzip 为固定优先级，不看 q 值高低）。
2. **取体与压缩**（文件命中腿与 SPA 回落腿，`static_page` 的 meta 注入之后，仅当协商出
   br/gzip 且 body ≥ min_length 且扩展名可压缩）。按序取压缩体，首中即用：
   a. **伴生预压**：`<原文件>.br`（协商 br 时）/`<原文件>.gz`（gzip 时）存在 → 直接读
      （构建期预压产物；其内容不进运行时缓存，原样服务）；
   b. **运行时缓存**：缓存文件命中 → 直接读，**不读原文、不压缩**（缓存键 =
      `sha1(abs_path)-<mtime_ns:x>-<len:x>-<algo><quality>`，任一变化自然失效）；
   c. **现场压**：读原文 → `spawn_blocking` 内 br（quality/lgwin=22）或 gzip（level）
      压缩 → 压缩后 ≥ 原文则弃用（D6，出 identity）→ 否则**原子写缓存**（tmp +
      rename；Windows 语义注意 rename 前先 remove 旧目标）后出体。
   取得压缩体后组装：`Content-Encoding`、`Content-Length`（压缩后）、弱化 ETag（W/）、
   `Vary`（与既有 Vary 逗号合并去重，不clobber CORS 的 Vary）。
   **缓存健壮性**：目录创建/写入失败（只读盘等）→ 一次性 warn，回退纯内存直压
   （fail-open，与 meta handler 同哲学）；无自动淘汰——
   `shortcut: 缓存无容量上限与 LRU，站点产物稳定时增长有界（文件数×算法数），站点重建由 mtime 键自然换代；出现海量冷缓存问题再升级`。
3. **304 腿**（`If-None-Match` 命中提前返回处）：不压（无体），只补 `Vary: Accept-Encoding`。
4. **HEAD**：同管线；hyper 剥体留 `Content-Length`（= 压缩后长度，符合语义）。
5. **可观测（R6 最小实现）**：压缩生效时 `tracing::debug!`（target `serve::static`，
   algo + 前后字节数）；不上 metrics。

### 4.4 默认缓存策略（`web_cache_policy: auto` 时，`apply_custom_headers` 之后）

对文件命中与 SPA 回落两腿，若响应**尚无** `Cache-Control`（显式 `mounts[].headers` /
`html_cache_control` / 动态 cc 已写入则不触发），按 `rel_path` 启发式：

| 条件（按序首中即停） | Cache-Control |
|---|---|
| 文件名含内容哈希段：`-[0-9a-fA-F]{8,}.<ext>` | `public, max-age=31536000, immutable` |
| 路径段含 `assets/` 或 `modules/` | 同上 |
| 扩展名 `html`/`htm`，或文件名 ∈ {`modules.json`,`versions.json`,`index.json`} | `no-cache` |
| 其余 | `public, max-age=86400` |

已知限制（进文档）：webpack 点分哈希 `app.1a2b3c4d.js`、sw.js（service worker 惯例需
no-cache）不中启发式 → 落 1 天档；可用 `mounts[].headers` 显式覆盖。

### 4.5 交互与正确性清单

- **显式优先（R2）**：`html_cache_control`/动态 cc 在 `static_page` 内先写 → 兜底不触发；
  `mounts[].headers` 挂载头先于兜底应用 → 可覆盖兜底（也可覆盖全局
  `response_headers`，既有语义不变）。
- **off / none（R5' 回退）**：不协商、不压缩、不出现 `Vary`/`Cache-Control`，逐字节现状。
- 已带 `Content-Encoding` 的响应不压（静态层不会产生该情况；防御一行）。
- `compress_types` 给出时按扩展名全集覆盖：用户显式列 `wasm` 即压 wasm；但
  png/jpeg 等即便列出也不压（防呆，写进文档）。

## 5. 验收对照

| PR 验收 | 设计落点 |
|---|---|
| `Accept-Encoding: br` → 200 + `Content-Encoding: br` + `Vary` + 体变小 | 4.3.2 |
| `gzip` → gzip；无头 → 原文无 `Content-Encoding` | 4.3.1 |
| `static_compression: off` 完全回退 | 4.5 |
| 首屏 25MB → ~7MB、antd <1MB | br q5 对 JS 文本 4-5x，达标 |
| 零配置 `/assets/index-<hash>.js` → immutable；`/`、`/modules.json` → no-cache | 4.4 |
| `web_cache_policy: none` → 全挂载无 Cache-Control | 4.5 |

## 6. 测试清单（serve crate 单测，`cargo test --release`）

1. 协商：br>gzip>identity；q=0 排除；无头 identity；`*` identity；坏头 identity。
2. `compress_min_length` 边界：1024 压 / 1023 不压。
3. `compress_types` 覆盖：列 `wasm` → 压；png 即便列出也不压。
4. ETag：压缩后 `W/"…"`；identity 保持强 ETag。
5. `Vary`：200 与 304 都有；与既有 Vary 合并不重复。
6. off：响应逐字节同现状（无 Vary / 无 Content-Encoding）。
7. 缓存启发式表逐行；`none` 全挂载无 Cache-Control；`mounts[].headers` 覆盖兜底；
   `html_cache_control` 在 HTML 上仍胜出；SPA 回落 index.html → no-cache。
8. 压缩不小于原文 → identity（构造高熵小文件）。
9. 取体优先级：伴生 `.br` > 运行时缓存 > 现场压；缓存命中不再读原文（断言缓存文件
   生成 + 二次请求命中）。
10. 缓存失效：touch 原文件（mtime 变）后旧键不命中、新键重建。
11. 缓存目录不可写 → 回退直压，响应头/体仍正确（fail-open）。

## 7. 留给实现者

brotli/flate2 具体API 用法、`spawn_blocking` 的 move 细节、CHANGELOG 版本号与措辞、
doc 注释措辞。

## 8. 文档与发布义务（仓库红线）

- `CHANGELOG.md` 版本条目（含**默认开启行为变更**醒目提示：既有部署升级后开始出现
  `Content-Encoding`/`Vary`/`Cache-Control` 头，`off`/`none` 可回退）。
- devkit 四件同步：`api-manual.md`（server 新键 + mounts 节已知限制行改写：压缩/ETag/
  默认缓存现状 + 限制表）、`SKILL.md` 陷阱速查（点分哈希/sw.js 不中启发式；nginx 前置时
  建议关 nginx gzip 防双压）、`scenarios.md` 可照抄场景、`README.md`；`cargo xtask build`
  归置 `bin/devkit/`；`cargo test --release -p xtask` devkit 契约用例。
- 门禁：`cargo fmt --check`、`cargo clippy --release --workspace --all-targets -- -D warnings`。

## 9. OJ-11（PR-10，P3）：API 路由安全方法 HEAD 自动镜像 GET

意图：仅声明 `get()` 的 API 端点收到 HEAD（uptime 探针 / LB 健康检查）不再 405，
免去每工程手写 `head()` 镜像 handler；显式 `head()` 恒优先。

现状（v0.1.115 核验）：release 表腿 405 在 `serve/src/lib.rs:736`
（`Lookup::MethodNotAllowed`），dev 兜底腿在 `lib.rs:762`（`method_name("HEAD")→"head"`
未注册）；web 挂载本就支持 GET/HEAD，不动（R4）。

设计：

- **R1 镜像点（两腿同规则）**：release 腿——HEAD 得 `MethodNotAllowed` 时对同 path 二次
  `lookup(path, "GET")`，命中即以 GET handler 链运行并标记 `mirror_head`；dev 兜底腿——
  verb==HEAD 且 head 方法未注册而 get 已注册时同理。镜像判定与 405 判定**同一注册源**
  （启动期契约表），不做运行时探测。
- **R2 显式优先**：HEAD 先走精确命中（该 pattern 注册了 head 链即原样运行，不镜像）。
- **R3 非安全方法不动**：仅 HEAD 触发回退；POST/PUT/DELETE/PATCH 仍 405。
- **体剥离**：镜像请求走完整前置管线与 handler 执行（admit/鉴权/租户——受保护端点
  HEAD 无 token 得 401，与 GET 一致），capture 后**体置空**，状态码与全部响应头保留；
  `Content-Length` 显式保留 GET 原值（RFC 9110 §9.3.2 HEAD/GET 头一致；流式
  `cap.stream` 同样置空只留头）。镜像响应不参与压缩（OJ-9 R5 动态信封范围外，天然一致）。
- **周边**：启动路由打印与 openapi 不新增 HEAD 行（隐式镜像不注册）；CHANGELOG 注明
  行为变更（HEAD 405 → 镜像）。
- **验收**：PR §5 三条，另加：受保护端点 HEAD 未带 token → 401（与 GET 同，
  进 SKILL.md 陷阱速查——探针应按状态码族判活，401≠服务不可用）。
- **测试**：GET-only → HEAD 同状态码/同头/空体；显式 head() 覆盖；POST-only HEAD 仍 405；
  302 端点 HEAD 保留 Location；dev/release 两腿各一例；镜像 + `finish()` 流式。
