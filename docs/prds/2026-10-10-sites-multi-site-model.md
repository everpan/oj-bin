# 设计：删除 `static_sites` 等旧键，升级顶层 `mounts:` 扁平挂载模型

日期：2026-10-10 · 状态：已定稿（双专家评审通过，待实现）

## 评审记录

**方案演化**

- 初始方案（2026-10-10）：顶层 `sites:` 列表，站点 = prefix + api/web/bundle 多键。
  拍板四条：空串报错；api+web 同 prefix 允许；旧键 fail-fast；工具命令启用 `--site`。
- 追加需求「api/web 支持对象 `{prefix, path}` 与父 prefix 拼接」引出两个悬而未决点
  → 复杂度源头是「站点」嵌套概念本身，拍平后对象形态与数组**均不需要存在**。
- 终案（已接受）：`mounts:` 扁平挂载表，一行 = 一个绝对 URL 前缀 + 一个目录。
  旧三键处置改为「报错并直接输出迁移后的 `mounts:` YAML」。
- 追加减去（2026-10-10）：`bundle` 形态移除——糖只省两行手写；挂载形态收敛为
  api/web 两种。
- 双专家独立评审（2026-10-10，架构师 11 条 findings + 工程师 14 条；三条 H 级两边
  独立命中、结论一致）：

**架构师 findings（H1 打回点已采纳；H2/H3/M4–M7/S8–S10/L11 均采纳，L 除外）**

- H1（`--api-path` 展开公式自相矛盾，默认部署会变 `/v1/api/api/...`；`api_prefix`
  键命运未定义）→ **采纳**：公式改为 `{prefix: base, api}`（base 原样含 `/api` 段）；
  `api_prefix`（含 serde alias `base:`）入旧键删除+迁移清单。
- H2（「内建端点 URL 变化」前提错误：今日即在 `{base}/health`，base 默认 `/v1/api`）
  → **采纳**：按 H1 修正后**零 URL 变化**，CHANGELOG 相关条目撤销。
- H3（`server.app_spa_fallback` 默认 false 且有明文立论，PRD 无声翻转为自动开）
  → **采纳**：web 挂载显式 `spa: true`（默认 false，继承 v0.1.20 立论）；
  `app_spa_fallback` 入旧键迁移映射。
- M4（`server.html_meta` 锚点相对 `app_path` 悬空）→ 采纳：跟随各 web 挂载根。
- M5（`sample-desktop`/`examples` 是旧键编译期消费者，改动面遗漏）→ 采纳补行。
- M6（跨树表名走 S002 fail-fast、模块名却静默遮蔽，不对称）→ 采纳：模块名跨树
  重复改报错。
- M7（`known_top_level_keys` 及对账测试漏登记 `mounts`）→ 采纳补回。
- S8（`--site` 组合矩阵不全）→ 采纳补三行。S9（迁移输出按旧→新两列逐键）→ 采纳。
  S10（根 `/` api 挂载表态）→ 采纳：允许。L11（点名不受影响面）→ 采纳。
  演进性判定：`RuntimeMount` enum 是未来 per-mount 扩展的自然落点，无过度设计。

**工程师 findings（结论「修改后通过」，5–7.5 人日；H1–H3 与架构师 H3/H2/H1 合流；
M4–M10、S11–S13 采纳，L14 无需动作）**

- M4（`--app-path` 是 `Vec` 且有 `prefix=dir` 形态）→ **采纳保留两形态**（砍掉会破
  既有脚本，映射仅三行）。M5（旧键检测时序未钉死且无 `deny_unknown_fields` 兜底，
  是唯一防线）→ 采纳：解密后 Value 树、typed 反序列化前，加密/明文两路径统一。
  M6 = 架构师 M7 采纳。M7（`schema diff` 不装配 App，`--site` 是死旗标）→ 采纳：
  `--site` 收缩为五命令。M8（测试清单错漏：漏 `mail_e2e.rs`，`start_cert_expired_
  test.rs` 不存在）→ 采纳改正。M9 = 架构师 M4 采纳。M10（热重载 watcher 需逐挂载
  注册）→ 采纳补行。S11（headers 用 `HashMap` 输出序不定）→ 采纳：config 层
  `BTreeMap` → serve 层 `Vec`。S12（`serve()` 删除波及 lib.rs 内嵌测试两处 +
  `serve_with_listener` 五处测试调用）→ 采纳补行。S13（静态仅 GET/HEAD 的语义应写
  明沿用）→ 采纳。L14（`anon_view_of_route` 硬编码前缀编译期机械跟随）→ 无需动作。

**面向用户的语义修订（相对上一版 PRD）**：纯 web 挂载的 SPA 回落由「自动开」改为
**显式 `spa: true`**（默认 false）——继承 v0.1.20「静默 404→200 掩盖错配」的立论，
两位专家一致建议。

## 背景与目标

现状（v0.1.57）的站点配置是三处拼的：`server.app_path`（静态根）、`server.app_prefix`
（挂载前缀）、`server.static_sites`（额外静态站列表，v0.1.27 引入）。API 只有单挂载
（`--api-path` / config `api_prefix`），无法在一个进程里服务多套 API 目录；静态与 API
的挂载语义割裂在两层配置里。

目标：删除旧键，统一为顶层 `mounts:` 扁平挂载表，支持多 API 目录与多静态站点。
心智模型一句话：**一行 = 一个 URL 前缀 + 一个目录，仅此而已。**

```yaml
mounts:
  - prefix: "/v1/api"
    api: "src"            # /v1/api/<module>/...（与今日默认部署完全同形）
  - prefix: "/v1"
    web: "web"            # SPA；spa 默认 false，显式开启（见规则 2）
    spa: true
  - prefix: "/v2"
    api: "dist"           # /v2/<module>/...——prefix 写什么，URL 就是什么
  - prefix: "/docs"
    web: "docs"
    headers: { Cache-Control: "no-cache" }
  - prefix: "/app1/api"
    api: "./app1/api"     # 打包产物（dist/app1-1.0/{api,web}）拆两条手写即可
  - prefix: "/app1/web"
    web: "./app1/web"
    spa: true
```

非目标（明确不做）：

- 不做 per-挂载鉴权/管线差异：证书门禁、CORS、前置管线（鉴权+租户）保持全局。
- 不做代理/rewrite 类挂载（只有本地目录 api/web 两种形态）。
- 不提供 bundle 糖——打包产物拆 `{prefix}/api` + `{prefix}/web` 两条手写即可。
- 不引入对象形态 `{prefix, path}` / 数组取值——扁平模型下子路径就是再写一行。
- CLI `--app-path` 的 `prefix=dir` 形态**保留**（映射为 upsert，三行代码，砍掉破既有脚本）。

## 语义与校验规则（全部规则，共五条）

1. **值空串 → 报错**："must not be empty (omit the key instead)"。
2. **同 prefix 同类条目重复**（api×api / web×web）→ **报错**；**同 prefix 一条 api +
   一条 web → 允许**，api 优先；**`spa: true` 要求同 prefix 无 api 挂载**（嵌套 api
   不受影响——最长前缀命中使拼错路径直接落 api 挂载，回落吞不掉），否则报错。
3. **旧键存在 → 报错并输出迁移方案**。旧键清单（五个）：`server.app_path`、
   `server.app_prefix`、`server.static_sites`、`server.api_prefix`（含 serde alias
   `base:`）、`server.app_spa_fallback`。`load_with_extra` 在**解密后的 Value 树上、
   typed 反序列化之前**检测（`ServerCfg` 无 `deny_unknown_fields`，删字段后旧键会被
   静默忽略——此处是唯一防线，加密/明文两路径统一）。错误信息按「旧写法 → 新写法」
   两列逐键输出等价 `mounts:` YAML（`app_path`(+`app_prefix`) → `{prefix: <app_prefix>,
   web: <app_path>}`；`app_spa_fallback: true` → 对应 web 挂载加 `spa: true`；`app_path`
   为 None 时跳过主条目；`static_sites[i]` → `{prefix: <p>, web: <path>}` 含 headers；
   `api_prefix` 无从机械映射 → 提示「挂载条目/`-b` 自带完整 prefix」），并指向
   `docs/user-manual.md` §3。绝不静默迁移。
4. **目录 canonicalize fail-fast**；dev/release 逐条目判定（`is_release(dir)`）；
   web 挂载沿用仅 GET/HEAD（其余方法 405）；prefix 归一 = 必须以 `/` 开头、去尾斜杠、
   大小写敏感；**允许根 `/`**（api 挂载路由 `/<module>`，内建 `/health`/`/plugins`/
   `/blob/{key}`）。
5. `mounts` 为空且无 `--api-path`/`--app-path` → 报错提示。

## 路由语义（运行时）

全部挂载按 **prefix 最长命中**（长度降序）选择，跨挂载不回落：

1. **api 挂载**命中 → 顺序：`{挂载prefix}/blob/{key}`（GET 公开 / PUT 过前置管线）→
   `RouteTable.lookup` → dev 兜底目录镜像 → 同 prefix 的 web 挂载（若有，**不开**
   SPA 回落，见规则 2 的 `spa` 约束）。未命中 → 404 JSON。
2. **web 挂载**命中 → 静态服务；`spa: true` 且未命中文件 + 无扩展名 + Accept html →
   回落该挂载根 `index.html`；默认 false → 404。
3. 未命中任何挂载 → 404。
4. 内建端点跟 api 挂载走：`{api挂载prefix}/health`、`{api挂载prefix}/plugins`（保留
   路径，遮蔽同名业务路由）、`{api挂载prefix}/blob/{key}`。**与 v0.1.57 零 URL 变化**
   （今日即注册在 `{base}/health` 等，base 默认 `/v1/api`）。
5. 证书门禁、CORS、前置管线全局不变；前置管线的 rel path = 剥命中挂载 prefix。
6. `server.html_meta` 跟随各 web 挂载根：查 `<挂载root>/<html_meta>/<path>.json`；
   meta 响应头 = 挂载 headers + `html_cache_control`（后者同名键覆盖）。

## 类型设计

- `src/config.rs`：`StaticSiteConf` → `MountConf { prefix: String,
  api: Option<String>, web: Option<String>, spa: Option<bool>,
  headers: BTreeMap<String, String> }`（api/web 恰好其一，结构体 + 校验实现，不引
  enum flatten 复杂度；BTreeMap 保证输出定序，serve 端转 `Vec<(String,String)>`）；
  `Config.sites` → `Config.mounts: Vec<MountConf>`；删 `ServerCfg` 五旧键 + Default
  同步；`known_top_level_keys()` 登记 `mounts`（对账测试同步）。
- `serve/src/lib.rs`：运行时按 prefix 长度降序存 `AppState`：
  - `Api { prefix, table: RouteTable, fallback: Option<Routes>, web: Option<WebMount> }`
    ——`web` 仅在同 prefix 配对时内联（该 web 无 SPA 回落）
  - `Web { prefix, root: PathBuf, headers: Vec<(String,String)>, spa: bool }`

## 改动面（按文件）

| 文件 | 改动 |
|---|---|
| `src/config.rs` | `MountConf` 类型 + `validate_mounts()`（规则 1/2）；`load_with_extra` 解密后 Value 树检测五旧键 → 生成迁移 YAML 进错误信息（规则 3）；`Config.mounts`；`known_top_level_keys()` + 对账测试 `known_top_level_keys_covers_all_typed_fields`；删旧键；单测改 mounts 矩阵（空串/同类重复/api+web 配对/spa 约束/双键同写/旧键迁移输出快照） |
| `serve/src/lib.rs` | `RuntimeMount` 模型替代 `StaticSite`；`handle()` 重写为路由语义一节；`serve_web()` 提取（`accept_html`/`spa_fallback` 两参；仅 GET/HEAD，其余 405）；health/plugins/blob 逐 api 挂载注册；`dispatch_meta_handler` 按前缀定挂载；删 `serve()` 公共函数，留 `serve_with_listener(mounts, ...)`；**内嵌测试适配**（`serve()` 两处、`serve_with_listener` 五处调用）。**不动**：`serve_router`、daemon re-exec、static_opts/pipeline/cert 管线参数 |
| `oj/src/app.rs` | `resolve_mounts`（归一、去重、fail-fast、逐条目 `is_release`）；`App::from_config` 改收挂载列表，primary = 第一条 api 挂载（迁移/seed/fixtures 逐 api 树循环；纯静态时占位 `.oj-static-only`）；`build_schema_and_modules` 多树并集：表名重复沿用 S002 fail-fast，**跨树模块名重复改报错**（今日 `module_map.insert` 静默遮蔽）；路由表逐条目构建，dev 每条 api 挂载挂目录镜像 + notify watcher **逐挂载 `watch()`** + WS `ws::mirror_routes` 逐条；`serve::app(mounts, ...)` 新签名；`validate_html_meta_handler` 收新挂载列表；`html_meta` 逐 web 挂载根生效；尾部 `resolve_static_sites_*` 测试改写为 `resolve_mounts` 矩阵（同类重复/api+web 配对/spa 约束/缺目录/嵌套合法/根 `/` 挂载/跨树模块重名） |
| `oj/src/serve_cmd.rs` | `run()`：`--api-path d [-b B]` → upsert `{prefix: B, api: d}`，**B = `-b` > 默认 `"/v1/api"`（常量保留），不再追加 `/api`**；`--app-path` 裸 `d` → `{prefix:"/", web:d}`、`prefix=d` → upsert；`mounts` 空且无 CLI 挂载 → Err；抽出 `load_cfg` / `load_app_config(config, site, dir, base)` / `search_api_dir`；`is_release` 改 pub |
| `oj/src/args.rs` | **五个命令**加 `--site <prefix>`：`Test`、`TestSub::Fixture`、`Migrate`、`Exec`、`OpenApi`（`SchemaCmd::Diff` 不装配 App，不加）；`to_command` 透传 |
| 各命令模块 | `test_cmd` / `exec_cmd` / `migrate_cmd` / `openapi_cmd` 调 `load_app_config(..., a.site.as_deref(), ...)`；`--site` 组合矩阵：命中 api 挂载 → 用之；命中 web 挂载 → Err 并列出全部 api prefix 供选；与 `-b` 冲突 → Err；与显式 `-d` 同给 → Err；无旗标取第一条 api 挂载；`App::from_config`/`assemble_backend` 按新签名适配 |
| `sample-desktop/src-tauri/src/lib.rs`、`examples/verify_dispatch.rs` | 读 `cfg.server.api_prefix`（编译期消费者）→ 改读 mounts 首条 api 挂载 prefix 或常量 |
| `sample/config.yaml` + `sample/README.md` | 改写为 `mounts:` 形态 |
| `oj/tests/e2e.rs`、`e2e_upload.rs`、`mail_e2e.rs` | `StaticSiteConf`/旧 `from_config`/`serve::app` 签名适配 |

## 文档与发布

- `CHANGELOG.md` v0.1.58 节：破坏性变更置顶——五个旧键删除 + 迁移对照；SPA 回落改
  per-挂载显式 `spa: true`（继承 v0.1.20 立论）；**内建端点零 URL 变化**（写明，防
  用户误判）；`--app-path`/`--api-path`/`-b` 语义。
- `docs/user-manual.md` §3 重写挂载章（含五旧键迁移小节，与错误输出文案同源）；
  `README.md` 快速上手命令核对。
- devkit 四件同步：`api-manual.md` 挂载章、`SKILL.md` 陷阱（spa 默认 false、同 prefix
  api+web 配对、`{prefix}/plugins` 遮蔽）、`scenarios.md` 多挂载示例、`README.md`
  版本行。
- `oj/Cargo.toml` bump `0.1.58` → `cargo xtask build` 归置 `bin/`。
- 门禁：`cargo fmt`、`cargo clippy --release --all-targets -- -D warnings`、
  `cargo test --release`（根 + `-p oj` + xtask devkit 契约）。
- 单提交（含本 PRD），署名行 `unix@vip.qq.com ai`。

## 实施顺序与工作量

1. `src/config.rs`（`MountConf` + `validate_mounts` + 五旧键迁移输出 + 单测）
2. `serve/src/lib.rs`（`RuntimeMount` + `handle()` 重写 + 内嵌测试）
3. `oj/src/app.rs`（`resolve_mounts` + 装配多挂载化 + watcher 多根 + 单测）
4. `oj/src/args.rs` + `serve_cmd.rs` + 各命令模块（`--site` 五命令 + 调用点）
5. sample、e2e 三件、sample-desktop/examples 适配
6. 全量 check → 文档 + 版本 + 门禁 → 提交

工作量粗估（工程师评审）：**5–7.5 人日**（serve/lib.rs 重写与 app.rs 多挂载装配各
约 1.5–2d，args/命令模块约 1d，e2e+sample 约 0.5–1d，文档+门禁约 1d）。
