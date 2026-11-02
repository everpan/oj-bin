# 设计：`oj serve` 静态层按需压缩（OJ-9）+ web 挂载默认缓存策略（OJ-10）+ API HEAD 镜像（OJ-11）

- 日期：2026-11-02 · 状态：四路评审（架构/性能/网络/builder-check）意见已合入；
  两处对原始 PR 文本的偏离（D12、D9 改）待提出方确认 · §9 网络复核意见待回
- 基线：oj v0.1.115 · 来源：oj-module 提出的 OJ-9（PR-8）/ OJ-10（PR-9）/ OJ-11（PR-10）诉求文本
- 一句话：`oj serve` 的 web 静态挂载零配置获得「br>gzip 传输压缩（产物落盘缓存）」与
  「内容哈希 immutable、其余恒校验」的正确默认，外加 API 路由 HEAD 镜像 GET；
  `off`/`none` 逃生门完全回退现状。

## 1. 意图与背景

生产形态是 `oj serve` 单进程同出 API 与静态（`ojm preview` 复用同一静态层），前置 nginx
仅为了 gzip/brotli 与缓存头——配置环境相关、易遗漏、不随 `ojm build` 走。实测首屏静态产物
25MB / 123 文件（antd.js 单文件 3.5MB 未压缩出网），瓶颈在出网字节数与移动端解析。

代码现状（v0.1.115 核验）：
- 静态 body 已是**全量内存缓冲**（`serve/src/static_files.rs:45` `tokio::fs::read` →
  `Vec<u8>`）；ETag 仅对非 HTML（mtime-len 强 ETag）；web 挂载无 Range；
  `apply_custom_headers` 语义是**只补缺**。
- API 路由 HEAD 未注册即 405（`serve/src/lib.rs:736` 表腿 / `lib.rs:762` dev 兜底腿）。

## 2. 目标 / 非目标 / 反目标

**目标**
- G1 静态 GET/HEAD 按 `Accept-Encoding` 协商输出 br/gzip；压缩产物持久化落盘，重启后仍命中。
- G2 零配置默认缓存策略：内容哈希命名 → immutable 一年；其余（HTML/清单/未哈希资产）→
  `no-cache` 恒校验（现状对齐，ETag/304 使重验只花一个 RTT）。
- G3 API 路由 HEAD 镜像 GET（OJ-11），显式 `head()` 恒优先。
- G4 off/none 时不出现 Vary/Content-Encoding/Cache-Control，逐字节同现状。
- G5 启动时一行 INFO 声明哪些默认已生效、如何关闭（两默认行为变更的可归因性）。

**非目标（明确不做）**
- 动态信封、blob、`/health`、API 腿的压缩（OJ-9 R5）。
- web 挂载 Range 支持。
- `ojm build` **生成**预压产物；也**不消费**伴生 `.br`/`.gz` 直服（评审后砍除，见 D9）。
- metrics 系统；缓存淘汰 LRU；brotli lgwin 等 window 级 knob。

**反目标（出现即失败）**
- 破坏既有显式头语义（`mounts[].headers` / `html_cache_control` / `response_headers`）。
- CPU 尖峰不可控；HTML 路由间串内容 / 动态 meta 被冻结（v0.1.20/25 语义破坏）。
- 跨编码 ETag 混淆导致的错误 304 / 错误缓存；immutable 误钉未哈希文件。

## 3. 已定决策（D=我的裁决，标注来源；★=偏离原始 PR 文本，待提出方确认）

| # | 决策 | 来源 |
|---|---|---|
| D1 | 默认全开，无 per-挂载配置；`prefix.web` 自动享受 | 用户 |
| D2 | knobs 全做：min_length / compress_types / brotli_quality / gzip_level | 用户 |
| D3 | 压缩产物**落盘缓存**（缺省 `<config_dir>/compress-cache`，`""`=关）；**仅「体=文件字节」的非 HTML 腿可用**缓存与落盘；HTML（meta 注入/动态 handler）恒内存直压不缓存——防路由间串内容 | 用户+评审 |
| D4 | `web_cache_policy: auto\|none` 全局键 | 用户 |
| D5 | `compress_types` = 内置可压缩集的**全量替换**；硬排除地板集（图片/pdf/woff2）列了也不压 | 我的默认，按评审 #8 写准 |
| D6 | 压缩后 ≥ 原文 → identity | 我的默认 |
| D7 | 压缩体 ETag 弱化 `W/`；**`if_none_match_hit` 改弱比较**（两侧剥 `W/` 比较 opaque）；**变体选择先于 INM 校验**（nginx gzip 同序）；304 回显选中变体 ETag | 三方评审共识 |
| D8 | 默认缓存策略在显式头**之后**兜底写入（实现 R2 显式优先） | 调研结论 |
| D9★ | ~~伴生 `.br`/`.gz` 直服~~ **砍除**（架构评审：ojm 不产出、仅服务假想场景、引入 staleness 面）；「压缩文件存盘」由 D3 运行时缓存满足 | 评审共识，原为我引申 |
| D10★ | 缓存策略两档制：文件名哈希段 → immutable；**其余一律 `no-cache`**（原 PR 的 assets//modules/→immutable 与其余→86400 删除——antd.js 未哈希在 assets/ 会被钉死一年，且比现状更不重验） | 评审 #2，偏离 PR R1 |
| D11 | 凡「可压资源」（扩展名在集内 且 len≥min_length 且策略 on）：**所有腿**（压缩 200 / identity 200 / 304）统一带 `Vary: Accept-Encoding`（与选中编码无关，RFC 9110 §12.5.4）；不可压资源一律无 Vary | 网络 #1/#7 |
| D12 | 默认行为变更生效时启动打一行 INFO（哪些默认开启 + 对应关闭键），保多站点排障可归因 | 架构 #6 |
| D13 | 冷缓存 single-flight（按缓存键去重并发压缩）+ 全局信号量钳并发 = `num_cpus`；部署建议大产物走构建端预压（ojm 侧议题） | 性能 #3 |
| D14 | 缓存键 = `sha1(abs_path)-<ino:x>-<mtime_ns:x>-<len:x>-<algo><q>`（+inode 防 mtime 保留式替换）；写新键时顺带删同 `sha1(abs_path)-*` 前缀旧键（每路径×算法恒一代，防无界增长） | 网络 #7 / 架构 #5 |
| D15 | 新键收进 `server.static:` 子段（沿 cors:/mcp: 先例），`html_cache_control` 等既有键原地不动 | 架构 #7 |
| D16 | `Accept-Encoding: *` 视为提供 gzip（不提供 br）；token 大小写不敏感；多值头 `get_all` 合并；br>gzip 固定优先级（不看 q 排序）文档声明为有意从简 | 性能 #8 / 网络 #8 |
| D17 | `serve_web` 改 stat 先行、原文懒读：metadata（ETag/len 门控/缓存键）→ 选腿 → 仅需要的腿读原文（缓存命中/伴生腿不读） | builder #4 |
| D18 | HEAD 静态响应走同压缩管线（显式 set Content-Length，hyper 剥体）；API HEAD 镜像体置空、头与 CL 保留 GET 原值 | 网络 #10 |

## 4. 代码级设计

### 4.1 配置面（`server.static:` 子段，`src/config.rs`）

```yaml
server:
  static:
    compression: true            # 默认 true；false = 不协商不压缩（连 Vary 也不加）
    compress_min_length: 1024    # 默认 1024 字节
    compress_types:              # 缺省 = 内置集（html htm css js mjs json map txt md
                                 # svg xml yaml yml woff ttf）；给出 = 全量替换
    brotli_quality: 5            # 1..=11 越界 fail-fast
    gzip_level: 6                # 1..=9 越界 fail-fast
    compress_cache_dir:          # 缺省 = <config_dir>/compress-cache；"" = 纯内存直压；
                                 # 相对 → 相对 config 目录（logs_dir 惯例）
    web_cache_policy: auto       # auto(默认) | none
```

**硬排除地板集**（防呆不可关，列出也不压）：`png jpg jpeg gif webp avif ico pdf woff2`。
未知扩展名：默认不压、可经 `compress_types` 显式开启（`octet-stream` 不是扩展名，不进表）。
serde 沿用 `#[serde(default...)]`；数值越界/枚举不识 fail-fast。缓存目录内容是派生数据
（可整目录删除）；部署文档补：只读根 → `compress_cache_dir: ""` 或指 tmpfs；多实例各挂各的盘。

### 4.2 装配（`oj/src/app.rs:2235` + `serve/src/lib.rs:132`）

`StaticOpts` 增 `static_layer: Option<StaticLayerOpts>`（None=off）与 `web_cache_policy`；
`StaticLayerOpts` 持 min_length / 扩展名 HashSet（内置集或覆盖集，地板集预剔除）/
br quality / gzip level / cache dir（None=内存直压）。启动时 web 挂载存在且默认生效 →
一行 INFO（生效项 + 关闭键名）。`serve/Cargo.toml` 增 `brotli = "9"`、`flate2 = "1"`。

### 4.3 协商、取体与压缩（`serve/src/static_files.rs`）

1. **stat 先行（D17）**：metadata 拿 mtime/len/ino → ETag、min_length 门控、缓存键。
2. **协商**：`get_all(ACCEPT_ENCODING)` 合并解析，token 小写化，`token;q=N`（缺省 1.0，
   q=0 排除）；仅认显式 `br`/`gzip`，`*` → 仅 gzip（D16）；坏头 → identity。选择固定
   br > gzip。可压 = 扩展名在集内 且 len ≥ min_length。
3. **取体（仅非 HTML 且可压且协商出编码）**，按序首中即用：
   b. **运行时缓存**：键命中（D14）→ 直接读缓存文件，不读原文不压缩；
   c. **现场压**（single-flight 按键去重，信号量钳并发 D13）：读原文 → `spawn_blocking`
      br（quality, lgwin=22）/ gzip 压缩 → ≥ 原文则弃用出 identity（D6）→ 否则原子写缓存
      （tmp+rename；Windows 先 remove 旧目标）+ 删同前缀旧键（D14）后出体。
   缓存目录不可写 → 一次性 warn 回退内存直压（fail-open）。
4. **HTML 腿（含 SPA 回落）**：meta 注入后**恒内存直压**（`spawn_blocking`），不缓存不吃
   伴生（D3，防串内容/冻结动态 meta）。
5. **头组装（统一完成逻辑，所有腿复用）**：
   - 可压资源 → `Vary: Accept-Encoding`（D11；既有 Vary 读-合并-写回，`*` 独占不追加；
     站点/全局显式头先应用，压缩侧后合并——不触碰「只补缺」语义）；
   - 压缩体：`Content-Encoding`、显式 `Content-Length`（压缩后）、弱化 ETag `W/`；
   - identity（协商跑过但出原文）：强 ETag、无 Content-Encoding。
6. **304 腿**：变体选择先行 → 选中变体 ETag 弱比较 INM（`if_none_match_hit` 剥 `W/` 改造）
   → 命中回 304 + ETag（选中变体形态）+ Vary + **Cache-Control**（与 200 同一套完成逻辑，
   RFC 9110 §15.4.5）。
7. **HEAD**：同管线（缓存使成本可忽略），显式 set Content-Length，hyper 剥体（D18）。
8. **可观测**：压缩生效 `tracing::debug!`（algo + 前后字节）；启动 INFO 见 4.2。

### 4.4 默认缓存策略（`auto` 时，显式头之后兜底，D10 两档制）

| 条件 | Cache-Control |
|---|---|
| 文件名末段含哈希形态段：`[-._][0-9a-zA-Z_-]{8,}` 且该段**同时含数字与字母**（Vite base64url / webpack hex 命中；`config-production` 这类纯字母词漏过 → 落 no-cache，防误钉） | `public, max-age=31536000, immutable` |
| 其余（html/清单/sw.js/未哈希资产/深链接 SPA 回落的 index.html） | `no-cache` |

- 判定入参一律用**落盘解析文件**（SPA 回落即 index.html），不用深链接 rel_path（性能 #1）。
- 判定与 Vary 完成逻辑同样覆盖 304 腿（4.3.6）。
- 想给未哈希资产长缓存：`mounts[].headers` 显式配（文档给出示例）。

### 4.5 交互与回退

- off / none：不协商不压缩不加头，逐字节现状（G4）。
- 显式优先序：动态 cc / `html_cache_control` > `mounts[].headers` > `response_headers` >
  默认兜底（D8）。

## 5. 验收

- `Accept-Encoding: br` → 200 + `Content-Encoding: br` + `Vary` + 体显著变小；重启进程后
  再次请求命中缓存（缓存文件存在）。
- `gzip` → gzip；`*` → gzip；无头 → identity（无 Content-Encoding，可压资源仍有 Vary）。
- `compression: off` → 逐字节现状。
- 二次访问：`If-None-Match: W/"…"` → 304 且带 Vary/Cache-Control。
- 首屏量级 25MB → ~7MB（**口径**：文本类 4-5x；字体/图片原样，不可压部分占比高时按构成
  修正；antd 运行时 q5 贴线 ~0.7-1.0MB，极限压走 ojm 构建端议题）。
- GET-only API 端点 HEAD 同状态码/同头/空体；显式 head() 优先；POST-only HEAD 405。

## 6. 测试清单（`cargo test --release`）

1. 协商：br>gzip>identity；q=0；无头；`*`→gzip；大小写；多值头合并；坏头 identity。
2. min_length 边界；地板集不压且 compress_types 列入也不压；未知扩展名显式开启可压。
3. ETag/304：压缩体 `W/`；INM 带 `W/` 命中（弱比较）；强 ETag identity 路径不回归；
   304 带 Vary + Cache-Control；变体选择先于校验（br 缓存客户端 identity 请求不错 304）。
4. Vary：可压资源 identity 200 也有 Vary；不可压资源无 Vary；与既有 Vary 合并、`*` 独占。
5. 缓存：二次命中不读原文；touch 原文旧键失效；inode 变化失效；同前缀旧键被清理；
   目录不可写回退直压；重启后命中（第二次 StaticOpts 实例）。
6. HTML：注入后直压；两个深链接各自 meta 正确（不串）；动态 handler 站点不缓存。
7. 缓存策略两档：哈希段 immutable；`config-production.js` 落 no-cache；点分 hex 命中；
   SPA 深链接回落 → no-cache；`none` 全无；显式头覆盖兜底；`html_cache_control` 仍胜。
8. HEAD：静态 HEAD 压缩且 CL=压缩后长度；API 镜像 GET-only 同状态码/头/空体；
   显式 head() 覆盖；POST-only 405；302 端点 Location 保留；dev/release 两腿。
9. off 逐字节现状。

## 7. 留给实现者

brotli/flate2 API 用法、single-flight 实现细节（`tokio::sync` 原语选择）、tmp 原子写
跨平台细节、CHANGELOG 版本号措辞、doc 注释。

## 8. 文档与发布义务

- `CHANGELOG.md`（默认开启行为变更醒目提示 + 缓存目录为派生数据 + HEAD 镜像语义）。
- devkit 四件：`api-manual.md`（`server.static:` 七键 + mounts 节已知限制改写 +
  限制表）、`SKILL.md`（未哈希资产默认恒校验——要长缓存显式配；前置缓存层必须按 Vary
  分键，否则 `compression: off`；探针按状态码族判活 401≠不可用）、`scenarios.md`、
  `README.md`；`cargo xtask build` 归置 `bin/devkit/`；`cargo test --release -p xtask`。
- 门禁：`cargo fmt --check`、`cargo clippy --release --workspace --all-targets -- -D warnings`。

## 9. OJ-11（PR-10，P3）：API 路由安全方法 HEAD 自动镜像 GET

意图：仅声明 `get()` 的 API 端点收到 HEAD（uptime 探针 / LB 健康检查）不再 405，
免去每工程手写 `head()` 镜像 handler；显式 `head()` 恒优先。

设计：
- **R1 镜像点（两腿同规则）**：release 腿——HEAD 得 `MethodNotAllowed` 时对同 path 二次
  `lookup(path, "GET")`，命中即以 GET handler 链运行并标记 `mirror_head`；dev 兜底腿——
  verb==HEAD 且 head 方法未注册而 get 已注册时同理。判定与 405 同源（启动期契约表）。
- **R2 显式优先**：HEAD 先走精确命中（注册了 head 链即原样运行）。
- **R3 非安全方法不动**：POST/PUT/DELETE/PATCH 仍 405。
- **体剥离**：镜像走完整前置管线与 handler（鉴权一致：受保护端点 HEAD 无 token → 401），
  capture 后体置空、状态码与全部头保留、`Content-Length` 显式保留 GET 原值（D18）；
  流式 `cap.stream` 同样置空只留头。镜像响应不参与压缩。
- **周边**：路由打印与 openapi 不新增 HEAD 行；CHANGELOG 注明行为变更。
- **验收**：PR §5 三条 + 受保护端点 HEAD 未带 token → 401（进 SKILL.md 陷阱速查）。
- **测试**：见 §6.8。

## 10. 评审记录（2026-11-02）

四路并行评审：架构（8 条）、Web 性能（10 条）、网络协议（10 条）、builder-check（6 问）。
共识合入：HTML 缓存隔离（D3）、两档缓存策略（D10）、弱比较+变体选择序+304 完整头（D7/D11）、
single-flight（D13）、inode 键+代际清理（D14）、`server.static:` 子段（D15）、
`*`→gzip（D16）、stat 先行懒读（D17）。分歧已裁：伴生直服砍除（D9，架构/网络二比一）。
遗留：§9 网络复核意见待回。
