# 模块内导入别名（`#` / `#/`）设计

日期：2026-09-15
状态：方案定稿，待实现 + 双专家评审（开发 / 架构）
关联：`src/bridge/module_loader.rs`、`oj/src/build_cmd.rs`、`oj/src/checks.rs`、`sample/tsconfig.json`
版本：v0.1.18

## 1. 背景与需求

目录镜像路由下，深层 handler 导入同模块共享库要跨越与深度成正比的相对路径：

```
src/m1/a/b/c/d/e/f/g/api.ts  →  src/m1/_shared/xxx.ts
import { x } from "../../../../../../_shared/xxx";
```

痛点有二：**数错层数**（编译期 vs 运行期语义不同，dev 能跑 release 炸），以及
**目录一移动整片失效**（新增/删除一层就要改所有引用）。

需求：一个**与目录深度无关**的写法，写一次永不随目录变动。目标形态：

```ts
import { xxx } from "#_shared/xxx";     // 本模块根锚点
import { yyy } from "#/user/_shared/y"; // src 根锚点（跨模块）
```

硬约束：**dev 与 release 语义必须一致**；跨模块引用必须仍能被版本绑定
（release 下跨模块导入被钉到 `dist/<m>-<v>/`）。

## 2. 现状链路（调研结论）

**解析只有两条分支**（`src/bridge/module_loader.rs:66-70`）：`./`/`../` → `resolve_relative`；
其余一律当 npm 包名 → `resolve_bare`（node_modules 回溯至 `project_root`）。无任何别名 /
`paths` / `package.json#imports` 支持。

**dev 与 release 是两条不同的路**：

| 模式 | 导入处理 | 依据 |
|---|---|---|
| dev | 运行期按真实 `src/<m>/…` 树解析，**不实化** | `module_loader::resolve_inner` |
| release | `oj build` 期把 specifier 改写成版本目录相对路径 | `build_cmd::fix_relative_imports:447` |

**跨模块导入在构建期绑定版本**：`resolve_spec`（`:500`）把相对 specifier 归一成 src 段列表，
首段是模块名 → 查 `view`（`dist/manifests.yaml` 锁 ∪ 本次构建计划）得 `<m_t>-<v_t>`，
缺锁 fail-fast（`:481`）；同模块则落 `<m>-<v>`。**任何别名都必须在构建期走同一条绑定**，
否则 release 下跨模块别名指向不存在的 `dist/<m>/…`。

**其余相关事实**：

- `LoaderShared` 只有 `project_root` + `ts`（`module_loader.rs:23-27`），**不持有 api 根路径，
  也不持有模块名表**；全仓 20 处构造点。
- 顶层 `src/_shared/` 会被 `load_modules` 判为「缺 manifest.yaml」直接失败
  （`oj/src/manifest.rs:99`）——即共享代码**必须有模块归属**，`_platform` 是现成先例。
- `sample/tsconfig.json` 存在但 **oj 完全不读**，编辑器与运行期是两套事实。
- 两个既有缺口：① `guard_no_api_imports`（`build_cmd.rs:601`）只扫**相对** specifier；
  ② 主构建路径 `fix_relative_imports` 是行级 `from "…"` 口径，**漏副作用 `import "…"` 与
  动态 `import("…")`**，而 tasks 镜像用的 `all_relative_specifiers`（`:117`）是全覆盖的。
- **既有 bug（本设计顺带修复）**：目录索引导入 `import x from "../_shared"` 在 dev 命中
  `_shared/index.ts`（`resolve_relative:134`），但 release 的 `product_spec`（`:530`）只做
  「末段补 `.js`」→ 产出悬空的 `../_shared.js`。**dev 能跑、release 炸。**

## 3. 语法与决策

### 3.1 解析规则

| 写法 | 解析为 |
|---|---|
| `#_shared/xxx` | `src/<本模块>/_shared/xxx.ts` |
| `#_shared/xxx.ts` | 同（显式后缀 as-is 命中） |
| `#_shared` | `src/<本模块>/_shared/index.ts` |
| `#util/date` | `src/<本模块>/util/date.ts` |
| `#/user/_shared/y` | `src/user/_shared/y.ts`（首段必须是模块目录名） |

后缀探针**复用 `resolve_relative` 既有顺序**：`as-is → +.ts → +.js → /index.ts → /index.js`。

### 3.2 决策与被拒方案

**采纳**：`#` = 本模块根锚点，`#/` = src 根锚点。区分仅靠 `#` 后有没有斜杠，单规则。

理由：`#` 是 Node 官方为**包内私有导入**（`package.json#imports`）保留的前缀，而 oj 的
「模块」正是发布/版本/归属单元——语义精确对位，无需解释心智模型。且 `#` 与 `$` 都不是
URL-safe（`encodeURIComponent('#pkg') !== '#pkg'`），**不可能成为 npm 包名**，与 node_modules
零碰撞；`ModuleSpecifier::parse("#x")` 失败（实测 `URL.canParse` 为 false）→ 自然落进裸
specifier 分支，不改解析主流程。

| 被拒方案 | 否决理由 |
|---|---|
| `$/…` | 无硬坑（解析层与 `#` 等价），但 `$_shared/…`（模块根形式）在 zsh/bash 双引号里实测被展开成 `/xxx`——恰好打在最常用的写法上 |
| `@/…` | `@` 无官方语义对位；且模块根形式 `@util/date` 与 npm scoped 包名同形（`@/x` 才安全）→ 只能保留一种形式 |
| `@module/…` | 与 scoped 包名同形；本模块也要写模块名，冗长 |
| 真·裸 `import 'xxx'` | 与 npm 包名共享命名空间，日后新增同名依赖会**静默改变解析目标** |
| tsconfig `paths` 作事实来源 | 引入 JSONC 解析 + 无法表达「当前模块根」；改为**由模块表生成 `paths`** 对齐编辑器（§8） |
| `node_modules/@oj/<m>` 软链实体化 | 需生成步骤 + 软链平台问题；`resolve_bare` 的 subpath 分支不补扩展名 |

## 4. 锚点派生（零装配改动）

**模块根 = referrer 所在目录向上最近的、含 `manifest.yaml` 的祖先目录。**
**src 根 = 该模块根的父目录。**

不往 `LoaderShared` 加 `api_root` 的理由：

1. 锚点由**文件自身位置**派生，dev / release / tasks 镜像三处天然一致，无需把
   `--api-path` 穿透到 20 处构造点。
2. `manifest.yaml` 在 release 产物里同样存在（`build_one` 原样复制，见
   `build_one` 步骤 2 与既有测试断言），所以派生规则跨模式成立。
3. 若注入 `api_root`，release 下 `#/user/…` 会被解析到**无版本段的** `dist/user/…`（悬空），
   是负价值。
4. tasks 池与测试目录天然无 `manifest.yaml` 祖先 → 别名「无处可锚定」，报错即正确语义
   （而非实现缺失），见 §9。

| 引用方位置 | 逐级上溯命中 | 结果 |
|---|---|---|
| `src/m1/a/b/c/api.ts` | `src/m1/manifest.yaml` | 模块根 `src/m1`，src 根 `src` |
| `dist/m1-0.1.0/a/api.js` | `dist/m1-0.1.0/manifest.yaml` | 模块根 `dist/m1-0.1.0`（运行期仅在改写漏网时可达，见 §6.4） |
| `src/tasks/foo.ts` | 无 | 报错（任务池禁用别名） |
| `sample/tests/x.test.ts` | 无 | 报错（测试在模块外） |

## 5. 运行期语义（dev）

`resolve_inner` 分支顺序：`file://` → `./` `../` → **`#` 别名** → `resolve_bare`。

`resolve_alias(spec, referrer_dir, ts)`：

1. referrer 位于 `node_modules` 内 → **不启用别名**（不劫持第三方包自己的 `#` 语义），回落原路径。
2. 上溯求模块根；无 → `Err("'#…' 只能在模块内使用（沿 … 上溯未找到 manifest.yaml）")`。
3. `#/…`：首段必须是含 `manifest.yaml` 的目录名，否则报错并列出实际存在的模块名；
   目标根 = src 根。`#…`：目标根 = 模块根。
4. 别名路径**禁止 `..` 与空段**（`#//x`、`#/../x` 直接报错）——纵深防御，且避免
   别名绕过「相对路径逃逸」的既有语义。
5. 探针：`resolve_relative(target_root, 剩余路径, ts)`（复用同一份候选顺序）。
6. 结果仍过 `ensure_within(project_root)` + `versioned_specifier`（`?v=<mtime>`）。

## 6. 构建期语义（release）

### 6.1 统一 specifier 扫描器

新增**唯一的** specifier 扫描器（`all_relative_specifiers` 的超集），覆盖
`from "…"`（静态 import/export-from）、副作用 `import "…"`、动态 `import("…")`、
`require("…")`，返回**字节 span**（支持一行多 specifier），并跳过 `//` 与 `/* */` 注释。
主构建路径与 tasks 镜像**同用此扫描器**——顺带修掉 §2 的缺口 ②。

### 6.2 `resolve_to_segs`：唯一的目标归一入口

`resolve_to_segs(src_root, module, rel_dir, spec) -> Result<Vec<String>>` 统一处理
`./` `../` `#` `#/`，并且**直接复用运行期的同一份探针**——`#` 走
`only_js::bridge::resolve_alias`，相对走 `only_js::bridge::resolve_relative`，拿到的是
**真实文件路径**（而非字符串补后缀）。段列表首段 = 目标模块名，交给既有 `view` 分支绑定版本，
产物路径由 `product_spec` 计算（只把末段 `.ts` 改 `.js`，其余后缀原样——探针给的就是真名）。

于是：**dev 与 release 对同一 specifier 必然命中同一文件**——「dev 能跑、release 悬空」
的一类缺陷在结构上被消掉（此前构建期只做字符串补后缀，命中不了目录索引）。副作用是
目录索引导入（`../_shared` → `_shared/index.ts`）自然产出 `_shared/index.js`，
顺带修掉 §2 的 bug。

### 6.3 `guard_no_api_imports` 扩展

`api.ts` 只许作路由入口。守卫改为扫**全部** specifier（含别名）：
`#item/api`、`#/user/item/api` 都要拒，堵掉当前「换个前缀就绕过」的口子。

### 6.4 残留断言（fail-fast 兜底，两道）

1. **单文件**：`assert_no_aliases` — 每个产物内不得再有 `#` specifier（别名必须实化）；
2. **产物级**：`assert_dist_consistent` — 构建末尾扫**本次产出的目录**（各模块版本目录 +
   tasks 镜像；不扫整个 `dist`——陈旧的他模块产物不该让本次构建失败），**任何**本地 specifier
   （相对 + 别名）都必须落到已落盘文件。

第二道把「扫描器漏检」从**运行期静默炸**降级为**构建期显式失败**，是 §11 天花板的兜底——
单护别名是不够的（漏改写的相对 specifier 同样会悬空）。

### 6.5 tasks 池

任务池是**非版本化**资产（镜像到 `dist/tasks/`，不进锁/tgz），无力绑定模块版本 →
别名一律报错，提示改用相对路径或把共享代码搬进模块。

## 7. 结构检查 S008

`oj/src/checks.rs` 增 S008（`oj build` 内嵌、`--check` 亦跑），报错三要素同 S003：

1. 别名目标必须存在（同 §6.2 的探针），否则列出尝试过的候选；
2. `#/<其他模块>/…` 必须在该模块 `manifest.yaml` 的 `deps` 中声明，否则 fail
   ——把「代码耦合」纳入与「表耦合」（S003）同一套归属图，跨模块引用从此可审计；
3. 模块外文件使用别名 → 报错（§4）。

## 8. 顺带修复与对齐

| 项 | 内容 |
|---|---|
| 目录索引悬空（bug） | `../_shared` 在 release 产出 `_shared.js`；改为探针后产出 `_shared/index.js` |
| 扫描器统一（缺口） | 主构建路径补上副作用/动态 import 改写（原先只有 tasks 镜像覆盖） |
| api.ts 守卫（缺口） | 扩展到别名 specifier（§6.3） |
| 编辑器对齐 | `sample/tsconfig.json` 补 `paths`：`"#/*": ["./src/*"]` + `"#*": [各模块 /*]`（tsconfig 每个 pattern 只允许一个 `*`，故模块根形式只能逐模块列出、按序 best-effort；值须带 `./` 前缀——无 `baseUrl` 时非相对值会被 vite/esbuild 警告；**不放 `baseUrl`**，以免改变裸包解析）。**手维护**：新增模块时同步 `#*` 列表（不自动生成——build 改用户配置文件风险大于收益） |
| L2 单测对齐 | `sample/unit/vitest.config.ts`：`resolveId` 插件镜像同规则。L2 跑在 vite 解析器上，被 spec 直接 import 的模块内文件一旦用别名，不经镜像即按 Node `package.json#imports` 解释 `#` 而解析失败 |

## 9. 边界与非目标

- **模块外的文件不可用别名**：`oj test` 用例目录（`config_dir/tests/*.test.ts`）不在任何模块内，
  继续用相对路径（现状仅 `sample/tests/oidc.test.ts` 一处）。这是锚点语义的必然，非缺陷。
- **不支持 `src/_shared` 这类无主顶层共享目录**：共享代码必须有模块归属（`_platform` 是现成
  先例）；放行它要发明「非版本化资产如何镜像进各消费模块产物」的发布/回滚规则。
- **`manifest.yaml` 只能出现在模块根**（S008 门禁）：嵌套声明会让「向上最近 manifest.yaml」
  这个锚点指向嵌套目录，静默改写整棵子树的别名语义。
- **本地导入目标必须是 `.ts`**：`collect_module` 白名单只收 `.ts`/`manifest.yaml`/`schema.yaml`/`.sql`，
  指向 `.js`/`.json` 的本地导入在 release 产物里没有对应文件 → 构建期 fail-fast（此前静默悬空）。
- **release 模式不解析别名**：`resolve_alias` 在 `ts=false` 时直接报错（产物本不该含 `#`）。
- 不做 exports/conditions 映射、不做 `package.json#imports` 实现。
- 不引入 AST 重写（见 §11 天花板）。

## 10. 验收标准

1. dev：`#_shared/x`、`#/user/_shared/y`、`#_shared`（索引）、`#_shared/x.ts`（显式后缀）均可解析；
2. release：同一份源码 `oj build` 后，产物内**只有**相对 specifier，跨模块别名钉在目标版本目录；
3. 目录索引导入在 dev/release 行为一致（回归用例）；
4. `#/user/item/api` 等别名形式的 api.ts 导入被守卫拒绝；
5. 模块外文件 / `..` / 空段 / 未知模块名 / 缺失目标 → 报错含尝试过的候选与下一步；
6. `#/user/…` 未在 deps 声明 → S008 fail；
7. tasks 池内别名 → build fail；
8. `cargo fmt --check`、`cargo clippy --release --all-targets -- -D warnings`、
   `cargo test --release --workspace` 全绿；sample 的 dev 与 release 双跑通过。

## 11. 风险与天花板

- **字符串扫描而非 AST**：正则字面量（`/"/`）会与字符串错位、非字面量实参的动态 import
  （`import(v)`、`import("./a" + x)`）不实化。兜底 = §6.4 产物自洽断言（错位的后果从运行期
  静默炸变成构建期显式失败）+ 扫描器跳过注释。若出现真实误报，下一步换 deno_ast 解析
  （仓库已有该依赖）。
- **`#/m/x` 无法用非 oj 解析器表达**：Node 的 `package.json#imports` 禁 `#/` 开头的键，
  所以 src 根锚点只能靠 tsconfig `paths` + vitest 插件尽力对齐；纯 Node 环境下不可用。
- **`#` 与 Node `package.json#imports` 命名空间重叠**：项目若真用 Node 内部的 `#` 导入，
  会被抢先解释。构建期检测：项目 `package.json` 存在 `imports` 且含 `#` 键 → fail-fast
  （取舍见 §12.2：不做"同义映射"识别，宁可显式报错给一行修法）。
- **同名共享文件的编辑器歧义**：`"#*"` 逐模块列举时，编辑器可能跳到别的模块同名文件。
  运行期锚点唯一、无误判；编辑器为 best-effort，靠同名冲突由 S008 在构建期暴露。
- **锚点靠上溯 stat**：每次解析多几次 stat（模块解析结果被 V8 模块表缓存，每 import 边一次）。
  上溯以 `project_root` 为界（模块只可能落在其内），不会走到文件系统根。必要时后续加 memo，当前不做。
- **本设计未覆盖的既有缺口（发现于实现期，非本次引入）**：① 模块内的 `.js`/`.json` 文件不在
  `collect_module` 白名单（只收 `.ts`/`manifest.yaml`/`schema.yaml`/`.sql`），所以本地导入它们
  在 release 产物里没有对应文件——本次把这类目标从「静默悬空」改成**构建期 fail-fast**
  （§9），但**没有**把非 `.ts` 资产纳入产物（属另一类发布契约，见 §12.2 待办）；
  ② `sample/unit` 的 `db` mock 只实现 `db.query`，v0.1.17 起 sample 多处已改用 `db.table()`
  构造器 → `npm run test:unit` 在 HEAD 即 2 例失败（与本设计无关，已实测确认：`git stash`
  单文件后 vitest 结果与改动后一致）——**已在本版一并修好**（新增
  `sample/unit/mocks/query-builder.ts`，L2 12/12 恢复绿）。

## 12. 评审意见与处置

双专家评审（开发 / 架构）后逐条处置。**已修**= 本轮改动内完成；**保留**= 有意不改并给出理由；
**待办**= 记为后续版本。

### 12.1 开发专家

| 意见 | 处置 | 说明 |
|---|---|---|
| 拼接实参的动态 import 被误改（`import("./locales/" + lang)` → 把拼接表达式改坏或直接构建失败）；HEAD 的行级口径不会碰它 → **本法引入的回归** | 已修 | `import(`/`require(` 形态要求实参是**孤立字面量**（闭合引号后只能跟 `)` 或 `,`）；`from` 形态不允许 `(`（`Array.from("./x")` 是方法调用）。core 测试覆盖拼接/三元/变量三种实参 |
| 残留断言只护别名 → 漏改写的**相对** specifier 仍静默进产物、release 才炸 | 已修 | 新增 `assert_dist_consistent`：构建末尾扫本次产出的目录，每个本地 specifier 必须落到已落盘文件（别名 + 相对一视同仁） |
| 扫描器不识别正则字面量（`/"/` 会与字符串错位 → 后续 import 漏改） | 保留（有兜底） | 字符级扫描是既定天花板（§11）；错位的后果由 `assert_dist_consistent` 从「运行期静默炸」变为「构建期显式失败」。需要更强保真时换 deno_ast |
| `module_root_of` 用词法 `starts_with(project_root)`，符号链接下可能误报"未找到模块根" | 保留 + 改进文案 | 真实两条路径两侧都是 canonical（dev 经 `versioned_specifier`、build 经 `src.canonicalize()`），改加 canonicalize 反而会让 build 的 `strip_prefix` 与临时目录夹具产生偏差。改为在报错里点明「上溯以 project root 为界」 |
| 别名/R import 指向非 `.ts` 目标（`.js`/`.json`）时"构建成功但产物缺文件" | 已修 | 非 `.ts` 目标 → 构建期 fail-fast（「扩展名不会进产物」）。**行为变更**：此前静默悬空，已记 CHANGELIST |
| 构建期改写报错丢文件身份（只报目录 + 无下一步） | 已修 | `build_one` 统一 `.map_err(|e| format!("{}: {e}", rel.display()))` |
| tasks 池失败会留部分 dist | 保留 | 与既有「失败即部分产物」一致（构建非事务性），未额外加预扫 |
| 嵌套 `manifest.yaml` 会重新锚定 `#` | 已修 | 并入架构侧 A1 |

### 12.2 架构专家

| 意见 | 处置 | 说明 |
|---|---|---|
| **锚点唯一性**：嵌套 `manifest.yaml` 会静默改写整棵子树的别名语义（must-fix） | 已修 | 新 S008 项目级检查：`manifest.yaml` 只允许出现在模块根。仓库内实测无嵌套用例（`git ls-files` + `find`），对既有项目零影响 |
| **检查比运行期更严**：`collect_ts` / `collect_module::walk` 不跳过 `node_modules`，而运行期明确跳过 → 第三方源码可致 S008 误报、被打进产物（must-fix） | 已修 | 两个 walker 均排除 `node_modules`（与 `resolve_inner` 口径一致） |
| release（`ts=false`）下别名"半可解析"（同模块命中、跨模块悬空）比硬失败更坏 | 已修 | `resolve_alias` 在 `!ts` 时直接报错，指向构建期契约 |
| S008 不对称（deps 门禁只覆盖别名）会腐烂成两类依赖 | 保留 + 待办 | 本轮不追溯（追溯会让既有项目升级即 build 失败）。**待办（v0.2）**：相对跨模块引用在目标模块未声明 deps 时给 warn，下一版本提升为 error |
| `package.json#imports` 守卫过度：`"#_shared/*": "./_shared/*"` 是同义映射却被禁 | 保留（取舍） | 守卫的目的是杜绝"两套解析器给出不同目标"的静默分叉；同义映射虽兼容，但判定"是否同义"要引入映射解析，复杂度不值。取舍：宁可显式报错给一行修法（改键名/移除），不引入静默分叉面。同时记录天花板：`#/m/x` 在 Node 的 `package.json#imports` 里**无法表达**（Node 禁 `#/` 键） |
| tsconfig `paths` 由模块表生成只是文档承诺，实际手维护 → 新模块静默失去编辑器支持 | 已修（改文档，不生成） | 不在 build 里写 tsconfig（改用户文件风险大于收益）；文档改为「新增模块时同步 `#*` 列表」，`MODULES.md` 同步提示 |
| `--api-path` 在 project root 之外时诊断误导 | 已修 | 报错文案点明上溯边界与 `--api-path` |
| 结构检查层不应依赖构建管线（`checks` → `build_cmd::specifier_spans`） | 已修 | 扫描器下移到 `src/bridge/import_scan.rs`，build 与 checks 共用（同 `bridge::guard::extract_tables` 先例） |
| 非 `.ts` 目标（同开发侧 D5） | 已修 | 见 12.1 |
| `oj test` 用例在模块外 → 永远用不了别名，与「测试也是一等公民」冲突 | 保留 + 待办 | 语义使然（无锚点），且样本仅 1 处受影响。「给模块外文件一个显式 api 根锚点」需改 `LoaderShared` 形状（**待办**，v0.2 评估） |

### 12.3 采纳的结论

评审明确**背书**的决策（不再改动）：双轨（dev 运行期解析 + release 构建期实化）、两条路径共用同一份探针、
`#` 号的选择（否掉 `$`/`@`）、别名只在模块内、不做无主 `src/_shared`、以及本轮不把相对跨模块引用升为 error。
