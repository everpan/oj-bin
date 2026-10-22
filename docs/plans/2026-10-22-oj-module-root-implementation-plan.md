# 实施计划：`oj` 支持可配置模块根（`--module-root`）

> 需求来源：`docs/prds/202610220010-oj-module-root-pr.md`（下游 `@oj-module/cli`，`ojm`）
> 基线：`oj 0.1.84`（macOS arm64，`bin/oj` 实测）
> 状态：**已过四路评审（工程师 / 架构师 / 产品 / 安全），意见处置见 §6**；待用户拍板 §7
> 变更类型：新增能力（opt-in），默认路径逐字节不变

---

## 1. 需求复核结论（实测，非照单接收）

在 `bin/oj 0.1.84` 上逐条复现 PRD §4：

| 输入 | 实测输出 | exit |
|---|---|---|
| 目标布局 `src/m1/api/manifest.yaml` + `-d src` | `oj build: module 'm1' missing manifest.yaml` | 1 |
| 旧布局 `flat/m1/manifest.yaml` + `-d flat` | `m1 v0.1.0 → out2/m1-0.1.0`（+ tgz + manifests.yaml） | 0 |
| 模块根本身当 `-d`（`-d src/m1/api`） | `oj build: 0 module(s)`，**产物目录不生成** | 0 |
| 空 `src` | `oj build: 0 module(s)` | 0 |
| `-d src/m1`（模块父目录） | `manifest name "m1" != directory name "api"` | 1 |

**结论 1：需求成立。** 模块发现硬编码「`-d` 首层子目录」（`oj/src/manifest.rs:79`/`:119`）。

**结论 2：PRD 有一处事实偏差（订正）。** PRD §3 第 2 行写「`-d src/m1` → `0 module(s)`」；实测是 `manifest name "m1" != directory name "api"`。`0 module(s)` 对应把**模块根本身**当 `-d`。测试用例按实测写。

**结论 3：PRD 低估了改动面**（评审又补出两条我初稿漏掉的）：

- **（甲）`#/m/x` 的 src 根在嵌套下算不出来**：现在 src 根 = `module_root.parent()`（`src/bridge/module_loader.rs:262`）。模块根变 `src/m1/api` 后 parent 是 `src/m1`，不是 `src`。必须注入 name→模块根索引，且索引要穿进 `src/bridge/`（根 crate；`oj` 依赖它，反向不可调用）。
- **（乙）跨模块判定与嵌套检查都用「相对 src_root 的首段 = 模块名」**：`build_cmd.rs:788-791`、`checks.rs:155-159` 在嵌套下首段是 `m1`、路径却是 `m1/api/...`，归属与拼路径双双错位；`checks.rs:333` `nested_manifests` 以首层目录当模块根，会把 `src/m1/api/manifest.yaml` 判成嵌套 → **S008 硬失败**。即只改发现层会让 `--module-root` 直接 build 失败，比现状更糟。
- **（丙，评审补）dev 路由会错位**：`serve/src/routes.rs:325-341` 的 `rel = file.parent().strip_prefix(root)`，`root` 是挂载 api 目录（`app.rs:1555`）→ 嵌套下路由变 `/v1/api/m1/api/...`；而 release 侧 `rel_pattern`（`build_cmd.rs:842`）以模块根为基 → `/v1/api/m1/...`。**dev 与 release URL 分叉**。
- **（丁，评审补）种子静默不灌**：`oj/src/seed.rs:21` `collect()` 首层扫 `seed.sql`，标签取目录名 → 嵌套下种子不重放且**不报错**。

---

## 2. 方案

采纳 PRD 的方案 A（模块根可配置 + name 权威），据评审做五点调整（T1–T5）。

### T1：索引权威（不是「name 权威」）

新增 `ModuleIndex { base: PathBuf, roots: BTreeMap<name, PathBuf> }`，配 `root_of(file) -> Option<(&str, &Path)>`（**最长前缀归属**）。发现 / 别名 / 跨模块归属 / 路由 / 种子 / 嵌套检查**全部**从索引取，不再各自拼首层。这是让（甲）（乙）（丙）（丁）同时闭口的唯一办法。

分支点**只允许出现在索引构造一处**：`resolve_roots(root: &Path, recursive: bool)`；`false` = 旧语义（首层目录 + name==目录名校验），`true` = 递归扫 `manifest.yaml`。所有消费端单分支，靠 sha256 对照 + 既有单测兜底兼容。

### T2：`--module-root` = 源码树根（**取代** `-d` / `--api-path`，字段名保留但互斥）

```
--module-root <dir>          # 源码树根（后端源码所在目录）；相对 CWD 或绝对
```

**为什么不再与 `-d` 并存**（2026-10-22 拍板）：`-d`（build/migrate/test/exec/openapi/schema）与 `--api-path`（serve）本来就是「源码树根」，`--module-root` 再给一个目录就成了同一个东西两份。故：

- `--module-root <dir>` **就是**源码树根：在其子树内**递归**找 `manifest.yaml`；**命中即停**（命中目录不再深入，模块根互不嵌套）；跳过 `node_modules` 与点目录。命中目录 = 模块根，**后端模块名取 `manifest.yaml` 的 `name`**（目录名只作定位，不参与身份）。
- `-d` / `--api-path` **字段名保留不动**（旧命令行零改动，兼容承诺 A/C），但与 `--module-root` **互斥**：同时给 → 报错退出（clap `conflicts_with` + 明确文案）。
- 只给 `-d` / `--api-path` → 今日首层语义，逐字节不变。

用法：

| 布局 | 命令 |
|---|---|
| `src/<name>/{web,api}`（下游目标） | `oj build --module-root src -o dist` |
| `src/api/<name>/manifest.yaml`（后端平铺） | `oj build --module-root src/api -o dist` |
| 单模块子树 | `oj build --module-root src/m1/api -o dist` |
| dev | `oj serve --module-root src` |
| 旧布局（不变） | `oj build -d src -o dist` |

> **变更理由**：不做 glob（初稿的 `*/api`）。任意目录 + 递归命中即停覆盖下游全部场景，且**消灭整个 glob matcher**——尾斜杠空段、`**` 匹配零段、连续 `**`、`?` 跨段、含 `:` 段（Windows `C:/api`）、大小写折叠这一整类边界与对抗用例不复存在，零新增依赖。将来真需要通配再另加 `--module-glob`（登记项）。

**路径校验**：目录必须存在且是目录（否则报错，不静默当空）；`canonicalize` + `strip_verbatim`（Windows，同 `build_cmd.rs:24`）后作为索引 base；`-d` / `--api-path` 侧同样 canonicalize，两条路径同一口径。

### T3：运行期索引走 `LoaderShared`，不改 `resolve_alias` 签名形态

- `LoaderShared` 加 `modules: Vec<Arc<ModuleIndex>>`（默认空 = 今日行为），`#[derive(Default)]` + `new()` 构造器；39 处字面量（`app.rs:1070`、`build_cmd.rs:640`、`openapi_cmd.rs:37/80`、`serve/src/lib.rs`、`src/bridge/*` 及测试）机械改构造调用，仅 **dev/openapi/build 三处真正设值**。
- 用 `Vec` 不用单个 `Option`：一个进程可有多个 api 挂载（`app.rs:417` 逐树 discover），单个索引表达不了多挂载；按 referrer 最长前缀选索引。
- 注意：运行期 `project_root` 是 **config_dir**（`app.rs:1070`），不是 api 树 —— 索引按文件最长前缀匹配，与 project_root 无关，不受影响。
- 不采纳「进程级 `OnceLock` 全局索引」（工程师 F3 第一方案）：`cargo test` 多线程下 `OnceLock::set` 只有第一次生效，第二个夹具的索引会被静默丢弃，直接制造 flake。

### T4：dev 路由按模块根建表

`RouteTable::build` 增可选参数 `modules: Option<&[(String, PathBuf)]>`（5 个调用点中只有 `app.rs:1555`、`openapi_cmd.rs:45` 传值，其余传 `None` = 今日语义，测试零改动）：`rel = file.parent().strip_prefix(模块根)`，pattern = `/{b}/{manifest.name}/{rel 逐段 fs_seg_to_pattern}`——**模块根内部路径直接映射为 URL，首段永远是 `manifest.name`**：

```
src/foo/api/manifest.yaml          (name: m1)
src/foo/api/xxx/yyy/api.ts      ->  GET /v1/api/m1/xxx/yyy
```

（目录名 `foo/api` 不进 URL；release 侧 `rel_pattern`（`build_cmd.rs:842`）本就以 name 为模块段，dev 改完后两侧**逐条一致**。）flat 布局下模块名段 + rel 与今日 `strip_prefix(api_root)` 结果**逐字等价**。

**base_url 拼接不变**：`<name>/xxx/yyy` 是**相对路径**，`base_url` 照旧拼在前面 ——
`{base_url}/{name}/{模块根内相对路径}`。`base_url` = 挂载前缀（config `server.base`，默认
`/v1/api`；多挂载各按自己的 prefix，如 `/v2`）。模块根嵌套只改变 `{rel}` 的起点，
**不影响 base_url 的拼接方式、也不影响保留路径**（`/blob/{key}`、catch-all、`{base}/plugins`）。

### T5：0 模块 / 缺 manifest 的退出码

- 显式给 `--module-root` 时：命中 0 个 → 非零退出 + 列扫描根（`--module-root` 给的目录）；命中目录缺 `manifest.yaml` → 非零退出并**列全量命中目录**（PRD 建议 E）。
- 不给时：保持 exit 0，但**补 stderr WARN**（不改退出码）。理由：空 `src` / 只有 `tasks/` 的项目今天 exit 0（实测），全局改退出码会打断既有 CI；PRD 把 D 标为「可拆独立 PR」。

### 2.5 其余口径

- 模块 `name` 取 `manifest.yaml` 的 `name`；发现期**即过 `validate_module` + `validate_version`**（今日 `load_modules` 只校 name==目录名，从未过白名单）。
- **`name` 是模块的唯一身份，同时是三处前缀**：产物 `dist/<name>-<version>/`、锁键与 tgz 名、**URL 前缀**。目录名只作定位，不进任何用户可见面。
- name 全局唯一（跨挂载唯一，沿用 `app.rs:418-424`）；重名 → 报错列两条路径。
- 带 `--module-root` 时不做 name==目录名校验；不带时保持原校验。
- `#x` 锚本模块根（`module_root_of` 向上最近 `manifest.yaml`）**与深度无关，不改**。
- `#/m/x`：索引非空 → `roots[m] + rest`；索引为空 → 走今日实现（`module_root.parent()` + 首段探测），行为不变。
- S008 目标名与 build 的 `segs[0]` → 改按索引最长前缀归属。
- `nested_manifests`（S008）→ 改为「`base` 下含 `manifest.yaml` 但不在索引 roots 集合内的目录」，且**只在同一棵剪枝树内扫**（跳过 node_modules / 点目录）。
- **产物布局不变**：`<name>-<version>/` + `manifests.yaml` + tgz；release 分支（锁 → `<m>-<v>/`）扁平，完全不动。
- release 模式传 `--module-root` → fail-fast 报错（产物树无模块根概念），不静默忽略。
- vdir 碰撞检查（`build_cmd.rs:56-62`）**键改小写比对**：APFS/NTFS 上 name `A` 与 `a` 落同一目录，后者 `remove_dir_all` 会静默清掉前者产物。

### 2.6 安全守卫（安全评审两条阻塞）

- 遍历**不跟随目录 symlink**（用 `DirEntry::file_type()`，非 `is_dir()`）；命中后 `canonicalize(root).starts_with(canonicalize(base))`，越界即拒 —— 否则 `src/x -> /etc`（内含 manifest.yaml）会被当模块根，`collect_module`（`build_cmd.rs:668-705`）把仓库外 `.ts/.sql` 转译打进 dist 与 tgz（信息泄露 + 供应链面）。
- `base` 先 `canonicalize + strip_verbatim`（build 侧已有 `build_cmd.rs:24`；serve 侧 `serve_cmd.rs:992` 仅 absolutize，需补）。
- 遍历代价上限：深度 ≤ 8、命中数 ≤ 1024（防 `vendor/`、`target/`、`dist/` 大树与 symlink 成环）。

---

## 3. 改动清单（按提交顺序）

1. **新增 `src/bridge/module_scan.rs`**（根 crate，`oj` 与运行期共用）：`ModuleIndex` + `root_of()` + `resolve_roots(root: &Path, recursive: bool)`——`false` = 今日首层语义，`true` = 递归扫 `manifest.yaml`（命中即停剪枝、跳过 node_modules/点目录、canonical 越界守卫、深度 ≤ 8 / 命中 ≤ 1024）+ 单测（含对抗）。
2. **`src/bridge/module_loader.rs`**：`LoaderShared` 加 `modules: Vec<Arc<ModuleIndex>>` + `Default` + `new()`；`resolve_alias` 增索引参数（旧签名保留为空集委托，3 处调用 `build_cmd.rs:781`、`checks.rs:147` 与 loader 自身 + 单测不受影响）；`#/m/x` 首段按索引解析。
3. **`oj/src/manifest.rs`**：`load_modules` / `discover` 增 `recursive`；发现期补 `validate_module` / `validate_version`；`false` 走原代码。
4. **`oj/src/checks.rs`**：`scan_modules`(:259)、`mdir = src.join(name)`(:82)、S008 目标名(:155)、`nested_manifests`(:333) 改走索引。
5. **`oj/src/build_cmd.rs`**：`[MODULE]` 分支(:36)、`build_one` 的 `mdir`(:424)、**`resolve_to_segs` 的 `from_dir`(:776/:778) 与 `strip_prefix`(:787) 一并改**——只换 `segs[0]` 不够：嵌套下 `to` 会多出 `api` 段 → 产物 `dist/m1-0.1.0/api/x.js` 而落盘以模块根为基 → 悬空。同时显式定义 `root_of()==None`（`tasks/` 下文件、src 根文件）的回落，保留今日 `segs[0]` 语义与 `import ... resolves to src/ root` 原文案。
6. **`serve/src/routes.rs` + `oj/src/app.rs:1555` + `oj/src/openapi_cmd.rs:45`**：dev 路由按模块根建表（T4）。
7. **`oj/src/seed.rs:21`**：`collect()` 改走索引（标签取模块名）。
8. **`oj/src/args.rs` + 子命令接线**（按优先级）：`build` / `serve` / `openapi` / `migrate` / `schema diff` 必接；`exec`（`-d` 仅作 schema 白名单来源）、`test fixture` 同 resolver 顺带接上（成本 ≈ 0，换一致性）；`oj test` 收 `tests/*.test.ts`、不调 discover（`test_cmd.rs:46-66`），**本版不接、登记**。
   - `--module-root` 与 `-d`（各子命令）/ `--api-path`（serve）用 clap `conflicts_with` 互斥，同时给 → 报错退出（Q5）。
   - `oj build` 的 `-o/--out` 不变；tasks 镜像源随源码树根走（`<root>/tasks`，built from 索引 base）。
   - `serve_cmd.rs:1069` `search_api_dir` 的 plausible 判据须在带 `--module-root` 时按递归口径判，否则嵌套树被否掉 → `service dir not found`。
9. **测试**（§4）。
10. **版本与文档**：`oj/Cargo.toml` → `0.1.85`（+`Cargo.lock`）、`CHANGELOG.md`、devkit 四件（`api-manual.md` §2 项目结构 + §5 导入解析 + 错误/限制表、`SKILL.md` 陷阱速查、`scenarios.md`、`README.md`）、`docs/dev-guide.md` 的 `resolve_alias` 小节；`cargo xtask build` 归置 `bin/devkit/`（`cargo test --release -p xtask` 契约用例校验）。

---

## 4. 验收与对抗性用例

**兼容（最高优先级，先写先跑）**
- 旧布局 `-d src` 产物与 `main` **sha256 一致**（覆盖 `resolve_to_segs` 与 dev 路由两处最高风险改造）。
- 不带参数时：`load_modules` / `discover` / `checks` / `build` 的**错误文案**逐字不变（现有单测全绿即证据）。
- dev 路由：flat 布局下路由表与 `main` 完全一致（同一份 URL 清单比对，钉死（丙））。

**功能（PRD §7 七条，逐条对应）**
1. 旧布局产物 sha256 一致
2. `--module-root src` 发现数 == `find src -name manifest.yaml -not -path '*/node_modules/*' | wc -l`（命中即停后去重）
3. 目录名 ≠ `manifest.name`：带参数通过；不带报原错误
4. `#/m/x` 在 `m != 目录名` 时可解析；缺 `deps` 仍报 S008，且**报错里的目标名是 manifest.name 不是目录名**
5. `oj serve --module-root src` dev 加载全部模块，**URL 与 release 产物一致**
6. `--module-root src` 命中 0 个 → 非零退出 + 可读文案（列扫描根）
7. `oj build m1 --module-root src` 只构建 m1（按 `manifest.name` 匹配位置参数），`manifests.yaml` 保留其他条目
8. **URL 前缀取 `manifest.name`，base_url 照旧拼接**：`src/foo/api/xxx/yyy/api.ts`（`name: m1`）→ 相对路径 `m1/xxx/yyy` → dev 与 release 均为 `GET {base_url}/m1/xxx/yyy`；挂载 prefix `/v2` 时为 `GET /v2/m1/xxx/yyy`。目录名 `foo/api` 不出现；旧布局 URL 与 `main` 逐条一致

**对抗性（钉死守卫）**
- `--module-root` 指向不存在目录 / 指向文件 → 报错（不静默当空树）
- `--module-root` 与 `-d` / `--api-path` 同时给 → 报错退出
- **命中即停**：`src/m1/manifest.yaml` + `src/m1/api/manifest.yaml` 并存 → 只认 `src/m1`，后者按嵌套 manifest 报 S008（不静默双认）
- **symlink 越界**：`src/x -> /etc`（内含 manifest.yaml）→ 拒绝，且不进产物
- symlink 环（`a -> b`、`b -> a`）→ 深度上限终止，不死循环
- 命中目录缺 `manifest.yaml` → 非零退出且列**全量**命中目录（不只第一个）
- 两个模块根同 `name` → 报错列两条路径；大小写 `A` vs `a` → vdir 碰撞报错
- 模块根内再嵌套 `manifest.yaml` → S008（新模式下等价守卫）
- `node_modules/`、`target/`、点目录内同名目录不命中
- 目录名 ≠ name 时，`#/m/x`、`oj build <name>`、S008 目标名、**dev URL 与 release URL** 五者一致（防只改一处）
- **多模块互相 import + release 实跑**（PRD 自述下游有 39 处跨模块引用，原 §7 无此用例）
- release 模式传 `--module-root` → 报错（不静默忽略）
- Windows：分隔符、`strip_verbatim` 前缀下索引匹配与 canonical 判定仍成立
- 不给 `--module-root` 时 0 模块 → exit 0 + stderr WARN；给了 → 非零退出（T5，钉死两种口径）

---

## 5. 风险与红线

| 风险 | 处置 |
|---|---|
| `resolve_to_segs` 改造改变产物路径 | 先写 sha256 对照；flat 下新旧段计算必须逐字等价；`root_of()==None` 回落保留原文案 |
| dev 路由与 release 分叉 | T4 + dev/release URL 清单比对用例 |
| 索引在 dev/build 间不同源 | 装配期一次构造（`app.rs`）喂 discover / routes / loader；热更整代重建（既有语义）；新增模块目录需 reload 写进文档 |
| `LoaderShared` 39 处构造点 | `Default` + `new()`，机械替换，仅 3 处设值 |
| 退出码变化打断既有 CI | T5 收窄 |
| 新增依赖 | **零新增**（无 glob matcher） |

红线：不碰 SQL 注入面；不改 release 产物布局；不改 `manifests.yaml` 锁语义；不改 `panic = "unwind"`；测试一律 `--release`。

---

## 6. 四路评审处置表

| # | 来源 | 意见 | 处置 |
|---|---|---|---|
| 1 | 工程 / 产品 | `resolve_to_segs` 必须连 `from_dir` + `strip_prefix` 一起改，否则产物悬空 | **采纳**（§3.5），含 `root_of()==None` 回落 |
| 2 | 工程 | 漏了 `seed.rs:21`，种子静默不灌 | **采纳**（§3.7） |
| 3 | 工程 | `LoaderShared` 加字段爆炸（39 处）+ 运行期 root 是 config_dir | **部分采纳**：加字段 + `Default`（39 处机械改，3 处设值）；**不采纳**其 `OnceLock` 全局方案（并发测试下 `set` 只首次生效 → flake） |
| 4 | 工程 | 统一单分支 | **采纳**：分支只存在于 `resolve_roots`，消费端单分支 |
| 5 | 工程 | 双分支下 `build_cmd.rs:640` 内省 Bridge 也要索引 | **采纳** |
| 6 | 工程 | `search_api_dir` plausible 判据带 pattern | **采纳**（§3.8） |
| 7 | 工程 | glob 边界（尾斜杠 / `**` 零段 / 大小写 / symlink 只拒越界 / Windows） | **消灭**：T2 改为任意目录 + 递归扫，此类边界不复存在；symlink 与 Windows 两条独立保留 |
| 8 | 架构 | dev 路由 `routes.rs:325-341` 错位、dev/release URL 分叉 | **采纳**（T4 + §3.6）——初稿漏项，已实测确认 |
| 9 | 架构 | 单 `Arc<ModuleIndex>` 与多挂载互斥 | **采纳**：改 `Vec<Arc<ModuleIndex>>`，按 referrer 最长前缀选 |
| 10 | 架构 | 根 crate 只放薄数据 / `resolve_alias` 收 `&dyn ModuleResolver` | **不采纳**：trait 对象仍要进 `LoaderShared`（构造点问题不变），且扫描口径必须与运行期同源，放根 crate 正是为此 |
| 11 | 架构 | 推荐 config 键替代 CLI 穿透 | **不采纳本版**：CLI 对齐 PRD、下游直接传参；`oj build` 无 config 亦可跑。`mounts[].module_root` 列登记项（§7 Q1） |
| 12 | 架构 | 装配期一次构造 + 热更重建 | **采纳**（§5） |
| 13 | 产品 | 8 子命令有水分 | **部分采纳**：`oj test` 不接（不调 discover）并登记；其余接上换一致性 |
| 14 | 产品 | 先做固定目录名、glob 后续 | **部分采纳**：采纳「不做 glob」，但形态改为用户拍板的**任意目录 + 递归扫 manifest.yaml**（T2），比固定目录名覆盖更多布局 |
| 15 | 产品 | 不带参数 0 命中打 WARN | **采纳**（T5） |
| 16 | 产品 | 验收补多模块互 import release 实跑 / Windows / release 传参报错 | **采纳**（§4） |
| 17 | 安全 | symlink 越界 → 仓库外内容进 tgz（阻塞） | **采纳**（§2.6）：不跟随目录 symlink + canonical 越界校验 |
| 18 | 安全 | name 权威后未过白名单（阻塞） | **采纳**：发现期即过 `validate_module` + `validate_version` |
| 19 | 安全 | 大小写碰撞清产物 | **采纳**：vdir 碰撞键小写比对 |
| 20 | 安全 | DoS：深度 / 命中上限、剪枝 | **采纳**（§2.6） |
| 21 | 安全 | 别名锚点未被削弱，补用例 | **采纳**（§4 末条） |

---

## 7. 拍板结果（2026-10-22）

- **Q1 → CLI-only**。本版不加 config 键；`mounts[].module_root`（表达「不同挂载不同布局」）列为后续登记项。
- **Q2 → 按方案收窄**。显式给 `--module-root` 时 0 命中 / 缺 manifest 才非零退出；不给时保持 exit 0 并打 stderr WARN；release 模式传参 fail-fast。
- **Q3 → 暂不实施**。方案定稿于此文档，等与下游对齐排期后再动工。
- **Q4（2026-10-22 追加）→ `--module-root` = 源码树根**。值即后端源码所在目录（相对 CWD 或绝对），在其子树内**递归**找 `manifest.yaml`，命中即停；**后端模块名取 `manifest.yaml` 的 `name`**，目录名只作定位。不做 glob。
- **Q5（2026-10-22 追加）→ 不与 `-d` 并存**。`-d`（build/migrate/test/exec/openapi/schema）与 `--api-path`（serve）字段名保留不动，但与 `--module-root` **互斥**（同时给 → 报错退出）；只给旧参数 = 今日首层语义。

### 登记项（本版不做）

1. `mounts[].module_root` config 键（多挂载异布局）
2. `--module-root` 通配形态（`--module-glob`，支持 `*`/`**`/`?`）
3. `oj test` / `test fixture` 若将来接入 discover，需一并接 resolver（本版 `oj test` 不调 discover，故未接）
