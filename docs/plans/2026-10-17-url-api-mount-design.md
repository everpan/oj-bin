# mounts: 支持 URL 形态的 api mount 设计稿（v0.1.77）

- 日期：2026-10-17
- 状态：待四路专家评审（工程师/架构师/产品/安全），处置表见 §9
- 范围：oj-bin 单仓。诉求来源：oj-module `docs/prd/solution/202610161500-oj-bin-feature-requests-pr.md`
  的远期条目「mounts: 支持 URL 形态的 api mount」（即 2026-10-16 ojm 原生设计稿里的
  PR-6/OJ-5「只记方向」项）；前置依赖 OJ-1（exec 解压，v0.1.75 已交付）、PR-5
  （yaml/manifest，v0.1.75 已交付）均已就位，本稿补上「缓存目录 + 完整性策略」正式立项。
- 约束来源：仓库既有红线（CLAUDE.md：fail-fast、无静默回落、配置即授信）+ §2.5 依赖 spike 实证。

---

## 0. 背景与目标

当前 `mounts[].api` 只认本地目录（装配期 `resolve_mounts` canonicalize + 存在性
fail-fast）。「CDN 远程装载」要求 mount 指向 URL：装配期按需拉取 → 完整性校验 →
内容寻址缓存 → 以 release 语义服务。产物形态 = **dist 树的 .tgz**（根含
`manifests.yaml`，即 `oj build` 产物树的市场分发形态）。

目标：

1. `mounts[].api` 接受 `http(s)://` URL，本地目录形态**逐字节不变**（零迁移成本）；
2. **完整性锚必配**：URL 形态必须声明 `sha256`（64 hex），失配即拒启；
3. 内容寻址缓存：`<cache>/<sha256>/`，命中不重拉；写入原子（先 temp 后 rename）；
4. 解压安全：tar-slip / 链接 / 绝对路径 / 大小与数量预算，全部 fail-fast；
5. 错误可操作：点名、原因、下一步三要素齐备。

非目标：多 URL 负载均衡/回退、缓存 GC（登记待做 §8）、签名验签（sha256 之外另立）、
dev 形态远程装载（URL = release 产物，见 §2.3）。

---

## 1. 配置面（src/config.rs）

```yaml
mounts:
  - prefix: "/v1/api"
    api: "https://cdn.example.com/app-v1.2.3.tgz"   # http/https → 远程装载
    sha256: "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
    cache: ".oj-cache"        # 可选；缺省 <config_dir>/.oj-cache
```

`MountConf` 新增两个可选键（`#[serde(default)]`，既有配置零影响）：

- `sha256: Option<String>` —— URL 形态**必配**；本地目录形态**禁配**（防笔误静默）；
- `cache: Option<String>` —— 缓存根（相对 config_dir）；仅 URL 形态有意义，同样禁配于本地目录。

### URL 判定（关键：不得误伤 Windows 本地路径）

```
api 值含 "://"？
  ├─ 否 → 本地目录（现状不变；覆盖 "C:\\dist"、"c:/dist"、"./x"、纯相对名）
  └─ 是 → scheme 必须 http/https（大小写不敏感）
           ├─ 是 → URL 形态
           └─ 否 → Err: unsupported URL scheme ... (only http/https)
```

`Url::parse` 单独用会把 `C:\\dist` 解析成 scheme="c"——**不能只靠 scheme 判定**；
先查 `"://"` 字面（Windows 路径不含该子串），再校验 scheme 白名单。

### 校验矩阵（validate_mounts 扩展；config load 与 from_config 两层都跑）

| 组合 | 结果 |
|------|------|
| `api: <url>` + `sha256: <64hex>` | OK（hex 大小写均可，归一化小写比对） |
| `api: <url>` 无 sha256 | Err：URL api mount requires sha256 (integrity anchor) |
| `api: <url>` + sha256 非 64hex | Err：sha256 must be 64 hex chars |
| `api: <local>` + sha256 | Err：sha256 only applies to URL api mounts |
| `api: <local>` + cache | Err：cache only applies to URL api mounts |
| `api: file://…` / `ftp://…` | Err：unsupported URL scheme |
| `web: <url>` | Err：web mounts are local directories only（web 不开放 URL） |

---

## 2. 装配期物化（新模块 oj/src/mount_fetch.rs）

`resolve_mounts`（oj/src/app.rs）对 URL 形态 api 调用
`mount_fetch::materialize(&url, &sha256, cache_root)? -> PathBuf`，产物按
`ResolvedMountKind::Api { dir: cache_dir, ts: false }` 入表——**下游零改动**
（路由发现 / Bridge 模块加载 / `--site` / test / migrate / openapi 全部照旧）。

### 2.1 物化流程

```
cache_root = mount.cache（相对 config_dir，canonicalize 后 create_dir_all 0700）
            | <config_dir>/.oj-cache（缺省）
target     = cache_root/<sha256>

1. target/.complete 存在 → 命中，直接返回（debug 日志：cache hit）
2. 否则（需物化）：
   串行化：进程内维护 per-sha256 `Mutex<()>` 注册表（once_cell 单例），物化前取锁；
   持锁期间若 target/.complete 已存在（并发同伴已完成）则直接返回；进入物化分支时
   若 target 存在但无 .complete（上轮崩溃残留 / 陈旧半成品），先 `remove_dir_all(target)`
   （best-effort）再继续，消除 unix `rename(2)` 对非空已存在目录返回 ENOTEMPTY 的死锁。
   a. GET url（reqwest **单例**，流式 `Response::chunk()` 循环读，非 `bytes_stream()`
      故无需 stream feature）：边读边 sha256 + 计字节；超 MAX_DOWNLOAD_BYTES（512 MiB）→
      Err；写 cache_root/.tmp-<sha>-<pid>-<n>（与 cache_root 同文件系统，保证 rename 原子）。
   b. 实测摘要 != 声明 → Err（两个摘要都给出）+ 删临时文件。
   c. 解压 .tmp → 同名 staging 目录 cache_root/.stage-<sha>-<pid>-<n>（**手写逐 entry
      循环**，tar-slip 防护见 §3.2；**不用 `Archive::unpack()`**，否则无法流式计量）。
   d. 校验 staging 根含 `manifests.yaml` → 否则 Err：archive root must contain
      manifests.yaml；于 staging 内写 `.complete` 标记（rename 前预置，缩短崩溃窗口）。
   e. 以 `create_new(true)` 占位 target（O_EXCL：预置的 dir/symlink 直接 fail-fast，
      堵住预创建 symlink 攻击）；占位成功后 `rename(.stage) → target`；rename 遇 EEXIST
      （跨进程同伴已落定）则回查 target/.complete 存在即视成功。
   f. 释放 per-sha 锁，返回 target。
```

### 2.2 缓存语义

- **内容寻址**：目录名 = 声明 sha256；同 URL 换内容 = config 换 sha256 = 新目录，
  旧目录不被触碰（无隐默升级面）。
- **复用判定**：`.complete` 标记（非仅目录存在）——中途死亡的解压不留假完成态。
- **陈旧条目**：不自动 GC（登记待做 §8）；文档说明可手删 `<cache>/<非当前sha256>/`。
- **热更**：`do_reload` 重跑装配 → URL mount 重新物化；命中则零网络。

### 2.3 release 语义与产物契约

**产物契约（blocker 修复，四路齐认）**：URL mount 的 tgz 必须是「dist 根 tarball」——
整棵 `dist/` 以**空 prefix** 打成 tgz，根即 `manifests.yaml`（聚合锁）+ `<module>-<version>/`
子目录。oj 侧提供生产者：新增 `oj build --bundle`（或 `oj pack`），把 `dist/` 打成
`app-<version>.tgz`（等价于 `tar czf app.tgz -C dist .`）；文档亦给出手工契约。materialize
解压后 `target` 根直接满足 `is_release`，模块仍按 `<module>-<version>/` 被发现。

- **ts 单一真源**：materialize 返回 `dir` 后 `ts = !is_release(&dir)`（**不硬编码 false**），
  与本地形态同一计算路径、零不对称；因产物根含 manifests.yaml，is_release 必为真、ts 必
  为假，与旧稿等价但无漂移风险。
- 解压后根必须含 `manifests.yaml`（复用 `is_release` 判定）→ 否则 Err：
  archive root must contain manifests.yaml（产物契约：用 `oj build --bundle` 产出 dist 根 tgz）。

### 2.4 同步/异步与依赖

- `resolve_mounts` 保持**同步**签名（调用方 `load_app_config` / `App::from_config`
  均同步、被 async 上下文调用；改签名会波及 test/migrate/exec/openapi 全部入口）。
- 拉取在**专用线程 + 自建 current_thread runtime** 上 `block_on`（thread::spawn +
  join）：调用方可以是 tokio worker（不阻塞其 runtime）也可以是无 runtime 的
  单测（resolve_mounts_matrix 风格）。缓存命中不走线程。
- **依赖（实测更正）**：reqwest 当前在 `oj/Cargo.toml:51` 的 **`[dev-dependencies]`**，
  mount_fetch 跑在生产 serve 路径、dev-deps 不编进发行二进制——故须**从 dev-dep 提升为
  `[dependencies]`**，features = `["rustls", "webpki-roots"]`（**去 json**，json 仅测试由
  dev-dep 保留）。「零新增 lock 节点」成立（0.13+rustls+webpki-roots 已在树，提升不增版本
  节点）；发行二进制新增 reqwest/hyper 编译单元。
- **超时 + redirect（安全 B1/m6）**：reqwest `Client` 用 OnceLock 单例，builder 上设
  `connect_timeout(10s)` + `timeout(60s)` + 显式 `redirect::Policy::limited(10)`
  （sha256 钉死已挡内容篡改，https→http 降级纵深由策略控制）。

### 2.5 依赖 spike（阶段 0 实证结论）

| 问题 | 结论 | 证据 |
|------|------|------|
| rustls provider 谁装？双装会 panic 吗？ | reqwest 0.13.1 显式 `CryptoProvider::get_default()`，取不到才回落自家 `aws_lc_rs::default_provider()`；**不调用 install_default**，与 bridge 的幂等安装（`let _ = …install_default()`，src/bridge/mod.rs）互不干扰 | reqwest-0.13.1/src/async_impl/client.rs provider 选择段 |
| provider 版本单版本？ | aws-lc-rs 树内单版本（aws-lc-sys 0.44.0）；实现期 `cargo tree -i aws-lc-sys` 复核 | 本机 windows-msvc 交叉 check 编译日志 |
| reqwest feature 组合 | `rustls` feature 内含 `__rustls-aws-lc-rs` → "No provider set" panic 分支不可达 | reqwest-0.13.1/Cargo.toml |
| 与计划偏离 | 无 | — |

---

## 3. 安全模型

### 3.1 信任域与完整性

- config.yaml 与代码同信任域（既有立场）：URL 由部署方写定，非用户输入。
- **完整性锚 = sha256**：传输层（http/https/重定向/中间人）不影响内容正确性；
  https 仍推荐（保密 + 防投毒分析），http 允许但文档明示仅适合内网/可信通道。
- 重定向：reqwest 默认策略（≤10 跳，可跨 host 与 scheme）——哈希钉死内容，允许；
  文档说明。

### 3.2 tar-slip 防护（手写逐 entry 循环，不用 Archive::unpack()）

解压器逐条目 fail-fast，**全部拒绝而非跳过**；pax/gnu 元数据条目 **SKIP（非 reject）**；写盘前对 `dest.join(rel)` 做 lexical 前缀复核。补充规则：

- **pax/gnu 元数据**：`XHeader` / `GlobalExtendedHeader` / `GNUSparse` / `ExtendedHeader` / `Metadata` / `OldMetadata` / `ContinuousFile` → SKIP（不写出、不拒绝）。
- **稀疏条目**：`GNUSparse` 按 `header.size()`（**逻辑尺寸**）计入 4 GiB 上限并 reject，或干脆拒绝稀疏。
- **特殊文件**：char / block / fifo 设备条目 → Err（**删除 socket 类型**：tar 0.4 无 Socket 变体）。

| 防护 | 规则 |

| 防护 | 规则 |
|------|------|
| 路径穿越 | 条目名任何组件为 `..` / 根 / 前缀（Component::ParentDir/RootDir/Prefix）→ Err |
| 绝对形态 | 条目名以 `/` 或 `\\` 起始，或 Windows 驱动器形态（`C:`）→ Err |
| 链接 | symlink / hardlink 条目 → Err（oj build 产物无链接；与 OJ-8 PRD 里 tar -tvzf 链接零容忍范式同源） |
| 特殊文件 | char/block/fifo/socket 设备条目 → Err |
| 非 UTF-8 名 | 条目名非 UTF-8 → Err |
| 解压总量 | 累计解压字节 > MAX_UNCOMPRESSED_BYTES（4 GiB）→ Err（zip 炸弹） |
| 条目数 | > MAX_ENTRIES（100_000）→ Err |
| 单条目 | > MAX_ENTRY_BYTES（1 GiB）→ Err |
| 目录逃逸兜底 | 落盘前对 dest.join(rel) 做 lexical 前缀复核 |

### 3.3 本地缓存信任域

缓存目录与 config 同文件系统信任域（能写缓存 ≈ 能改 config ≈ 已接管主机）：
不重算已缓存内容哈希（否则每次启动全量重读，失去缓存意义）；`.complete` 标记 +
原子 rename 保证「不存在半成品被当成完成态」。0700 权限。

### 3.4 CLI 面

`--api-path <url>` → Err：--api-path does not accept URLs（URL 只从 config 进）。
CLI 折叠进来的目录按 CWD 预绝对化的既有语义不扩到 URL。

---

## 4. 错误面（可操作三要素：点名 + 原因 + 下一步）

| 场景 | 文案（英文，与 config.rs/mounts 既有文案同风格） |
|------|------|
| scheme 不支持 | mounts[i] (prefix "/x"): unsupported URL scheme "ftp" in api (only http/https) |
| 缺 sha256 | mounts[i] (prefix "/x"): URL api mount requires sha256 (integrity anchor; compute with: shasum -a 256 <file>) |
| sha256 格式错 | mounts[i] (prefix "/x"): sha256 must be 64 hex chars |
| 本地目录配了 sha256/cache | mounts[i] (prefix "/x"): sha256 only applies to URL api mounts |
| 拉取失败 | mounts[i] (prefix "/x"): fetch <url> failed: <reason> (check network/proxy; URL comes from config.yaml) |
| 下载超限 | mounts[i] (prefix "/x"): download exceeds 512 MiB cap |
| 摘要失配 | mounts[i] (prefix "/x"): sha256 mismatch for <url>: declared <a>, got <b> — artifact changed or tampered; update config or investigate the CDN |
| 解压违规 | mounts[i] (prefix "/x"): archive entry "<name>" rejected: <rule> |
| 非构建产物 | mounts[i] (prefix "/x"): archive root must contain manifests.yaml (pack an oj build output tree as .tgz) |

---

## 5. 兼容性

- 本地目录 mount：配置结构（新增可选键默认 None）、解析、路由、dev/release 判定
  **零变化**；存量 config 与 `MountConf` 编程构造（e2e 夹具）无需改动
  （新键 serde default；实现期确认 `MountConf` 的 Default/构造点是否需要补字段）。
- Windows：`C:\\dist` / `c:/dist` 本地路径不受 URL 判定影响（§1）；缓存路径经
  canonicalize；tar 条目名统一按 `/` 拆分前先拒 `\\` 形态。
- `--site <prefix>`、`oj test/migrate/exec/openapi`：URL mount 物化在共享
  `resolve_mounts` 内，各命令行为一致（无「serve 行 / test 不行」分裂）。

---

## 6. 测试计划

**单测（config.rs）**：§1 校验矩阵全组合（含大小写 hex、`://` 误伤探测：
`api: "C:\\dist"`、`api: "c:/dist"` 必须判本地）。

**单测（mount_fetch.rs，本地 axum/TCP 服务器现场造 tgz）**：
1. 全流：fetch → 摘要过 → 解压 → target 含 manifests.yaml + 模块文件；
2. 缓存命中：第二次物化服务器请求计数不变（AtomicUsize 哨兵）；
3. 摘要失配 → Err 且临时文件被清（无残留）；
4. 截断下载 → Err；
5. .complete 语义：预置 target 无标记 → 重拉；有标记 → 命中。

**对抗性探测（每个防护写「击穿尝试」）**：
6. tar-slip：`../escape.txt` 条目 → 拒；
7. 绝对路径条目 `/etc/passwd`、`C:\\x` → 拒；
8. symlink 条目（指向 `../../outside`）→ 拒；hardlink → 拒；
9. zip 炸弹：小 gz 膨胀到 >4 GiB（稀疏写）→ 拒且不落盘全量；
10. 条目数炸弹：100_001 个空条目 → 拒；
11. 单条目超大（>1 GiB）→ 拒；
12. 非 UTF-8 条目名 → 拒；
13. 非构建产物（根无 manifests.yaml）→ 拒；
14. 损坏 gzip → 拒；
15. 并发双物化同 sha256（tokio::join! 两次）→ 不互相踩。

**e2e（oj/tests/）**：boot 真实 serve，mounts 配 URL（本地 http 服务器现场造
dist 树 tgz）→ curl 模块路由 200 + 响应体正确（证明缓存目录被 Bridge 正常加载）。

**HTTPS 冒烟（dev-dep: rustls 同版本 + tokio-rustls 树内版本 + rcgen）**：
自签 cert 的 TLS 服务器 + reqwest 带根证书客户端跑通全流——封住「provider/根链」
在真实 https 路径上的回归（reqwest 0.13 provider 回落逻辑已实证，§2.5）。

---

## 7. 文档与发布

- docs/user-manual.md §3 mounts：URL 形态小节（配置示例、产物契约、缓存语义、
  http vs https、重定向、缓存清理）；
- sample/config.yaml：注释形态的 URL mount 示例（不引活 URL）；
- devkit 四件对齐检查（api-manual/scenarios/SKILL/README——本特性为配置面，
  JS API 无变化；按 CHANGELOG 规则逐件确认无需改或最小改）；
- CHANGELOG v0.1.77 + oj/Cargo.toml version 0.1.76→0.1.77（Cargo.lock 同步）。

## 8. 登记待做

- 缓存 GC（按 mtime/LRU 清陈旧 sha256 目录）——单独立项；
- 多 URL 回退 / 镜像列表；
- 私仓 mTLS / 自定义 CA 注入；
- dev 形态远程装载（源码树上 CDN 无意义）。

## 9. 评审意见与处置

（四路评审后填。）
