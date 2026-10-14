# 上游 RFC：`.schema` 权威源（响应通道 + openapi 投影 + DSL 边界）

> 来源：`oj-module/docs/prd/202610142350-defineapi-retire-schema-authority-pr.md` §2–§3
> 状态：**评审已过，实施中**（v0.1.70 / v0.1.71）
> 日期：2026-10-15

## 0. 范围（评审后收窄）

下游要把契约权威源从 `contract.ts`（zod，`defineApi`）迁到 handler 上的 oj `.schema`，
退役 `oj-module` 的翻译层。原提案向上游要三项能力，评审后收敛为**两项**，且**不新增任何
子命令**：

| # | 原诉求 | 处置 | 版本 |
|---|---|---|---|
| 1 | `.schema` 响应通道 | **落地** | v0.1.70 |
| 2 | `oj client` 生成子命令 | **取消**（改由下游读 `openapi.json` 生成 `__client`） | — |
| 3 | DSL 表达力对齐 zod 子集 | **落地为边界清单 + 钉死测试**（不扩白名单） | v0.1.71 |

### 0.1 为什么取消上游 client 生成器（RFC#2）

评审（架构师 + 用户裁定）结论：上游生成 TS/zod 会把**下游的前端布局约定**
（`web/src/<模块>/__client/`）、**zod 版本**（v3/v4 API 不兼容）、**import 路径**硬编码进
平台件，下游每次演进都要上游发版。

改为：**oj 只输出 `openapi.json`（含响应 schema），下游读它自行生成 `__client`。**

- `oj openapi` 已存在、`--check` 漂移门禁已存在、产物已要求提交进仓库
  （`docs/openapi-guide.md` §7）——交换物**已经是现成的**；
- 下游拿到的输入从「扫 `contract.ts` AST」变成「读标准 OpenAPI 3.1」，内省可靠性这个
  原始诉求（`RouteRow` 是装配期真相）由 `oj openapi` 的内省管线保证；
- 上游零新增依赖、零新增子命令、零 zod 版本耦合；
- 代价：openapi.json 成了**契约交换格式**，因此它必须**完整且不含非法声明**——
  见 §1.3「生成侧审计」。

## 1. RFC#1（v0.1.70）：`.schema.response` 响应通道

### 1.1 为什么是 `response` 而不是 `.responseSchema`

与 `params`/`query`/`body` 同处一个 `.schema` 对象内 = 与入参**同一处声明、同一趟内省、
同一份落盘**（`RouteRow.schema` 已是整对象透传）。挂到 handler 的另一个属性上，就要新增
一路内省、一路合并、一路落盘——省掉一个特殊性、补回三个。

### 1.2 语义（三项，均已评审确认）

1. **只声明、不校验**：运行期**不**校验响应。响应由 JS 产生，校验失败时请求已执行完、
   无处可去；把出参变成硬约束会让「少返回一个字段」升级成 500 级故障。
   → 它是**文档与生成真源**，不是守卫。文档必须前置否定（见 §3.2）。
2. **`response` = `data` payload**，即 `json.ok(data)` 的 `data`，
   **不是**整包 `{code,msg,data}`。
3. **`responses.200` 输出完整信封** `{code,msg,data:<response>}`——因为 oj 的 wire 恒为
   信封（`src/bridge/envelope.rs`），openapi.json 是公开产物，不该对字节撒谎。
   **例外**：`json.raw` / `json.stream` / `json.sse` 端点**绕过信封**（裸 body），
   这类端点**不应声明 `response`**（声明即与 wire 不符）；未声明则保持占位
   `{"200":{"description":"OK"}}`。不开 `response.raw` 逃生门（YAGNI）。

### 1.3 审计：唯一入口，三条路径同口径

原本只挂在 `oj/src/app.rs` 装配期，且在 `if cfg.server.schema_validation` 内——
三处漏网（评审 blocker/major，三方都提）：

- `oj build` 落盘 `routes.js` 不审计 → 坏声明进 dist，直到 release 启动才红；
- `oj openapi` 走 `discover_routes`，不经装配 → 生成物含非法声明（**下游现在要靠它生成
  client，这条从 minor 升级为必办**）；
- `schema_validation: false` 逃生门会连带关掉**全部**声明合法性审计。

**处置**：`src/contract.rs` 提供单一入口 `audit_schema(&Value)`（通道白名单 + 逐通道审计
含 `response`），在**装配期 / `oj build` 落盘前 / `oj openapi` 生成前**各调一次。
**声明合法性审计移出 `schema_validation` 开关**：开关管的是「是否执行校验」，不是
「声明是否合法」——逃生门是为「规则误杀合法流量」准备的，不是为「代码写错」准备的。

### 1.4 组级合并：`response` 不并入参那套

`mergeSchema`（`src/bridge/mod.rs`）对入参通道做 `required` 取并集 + `properties` 逐键覆盖。
**`response` 不套这套**：响应是动词各不相同的，组级声明的必填字段被某动词继承后，
文档/client 会声称返回并不存在的必填项——假真源。改为**动词级整体覆盖、无则继承组级**。

顺带修：现 `mergeSchema` 无条件写入 `required: []` / `properties: {}`，会给没声明这些键的
通道注入空值（进 openapi 即噪声）→ 仅当非空时写。

### 1.5 顶层通道白名单（行为收紧）

`.schema` 顶层只允许 `params` / `query` / `body` / `response`，其余键装配期硬失败。
以前拼错通道（`param` / `bodys` / `responce`）会**静默不生效**——接口在裸奔，
这正是本项目最重的缺陷类。报错带 `method` / `pattern` / `file` / `handler` 链
（v0.1.64 起同文件多链靠 handler 区分）。

存量影响面：本仓 sample 仅 1 处 `.schema` 且合法；下游存量若在顶层带
`title` / `description` / `$schema` 会启动失败 → CHANGELOG 标为**行为收紧**，
并在 devkit 写明「不要用 `schema_validation: false` 绕」。

### 1.6 openapi.json 必须带**模块归属**（否则下游生成不了分模块的 `__client`）

用户提出、评审确认：光有 `operationId` 不够——生成代码必须知道端点属于哪个模块，
否则下游只能靠 `x-oj-file` 猜目录。

- operation 级 **`tags: [<module>]`**：用标准 OpenAPI 分组字段（不是私有 `x-` 字段，
  通用 OpenAPI 工具同样按它归组）。模块名：dev 取 api 根下首段目录；
  release 从 `<module>-<version>/...` 剥掉版本后缀（`my-module` 这类自带连字符的
  模块名**不会**被误剥——尾段必须是「数字与点」且含点才剥）。
- spec 根级 **`tags: [{name}]`**（去重排序）：下游/通用工具不必扫 `paths` 就能枚举模块。
  无模块归属时**不输出**该键（避免给空项目凭空加字段）。
- api 根的裸 `api.ts` / `api.js`（无子目录）→ 无模块归属，不打 tag。
- 顺带：`x-oj-file` 改**相对 api 根**路径。产物是公开文件，带本机绝对路径属信息泄露，
  且换台机器就不可比（`RouteTable::rel_file`）。

### 1.7 生成期新增的两道 fail-fast

- **operationId 唯一性**：`/user/profile` 与 `/user-profile` 会派生出同一个
  `user_profile_get`。下游按 operationId 命名符号会**静默互相覆盖**，生成期必须拦死。
- **`properties` 键名禁 `__proto__` / `constructor` / `prototype`**：openapi.json 是下游
  生成 TS/zod 的输入，属性名会原样变成生成源码里的键——这三个名字生成的字面量对象会
  改到原型上（与 `legal_handler_chain` 对 handler 链段的口径一致）。

### 1.8 改动面

1. `src/contract.rs`：`SCHEMA_CHANNELS` + `audit_schema`（含 `audit_channels` /
   `audit_response`，内部复用 `audit_keywords`）+ 原型链污染键名防线。
2. `serve/src/routes.rs`：`RouteTable.root` + `rel_file` / `module_of` /
   **`audit_schemas`（唯一审计门）**。
3. `oj/src/app.rs`：装配期（移出 `schema_validation` 开关）调 `table.audit_schemas()`。
4. `oj/src/build_cmd.rs`：落盘 `routes.js` 前逐行调 `contract::audit_schema`。
5. `oj/src/openapi_cmd.rs`：`generate` 开头调 `table.audit_schemas()`（dev/release 都覆盖）；
   `responses` 占位 → 有 `response` 时出信封 + `x-oj-response-enforced: false`；
   加 `tags` / operationId 唯一性。
6. `src/bridge/mod.rs`：`mergeSchema` 加 `response`（整体覆盖）+ 不再注入空
   `required`/`properties` + `Object.assign` 改对象展开（避 `__proto__` 键改原型）。

### 1.7 测试（对抗性，逐条钉死）

- `response` 声明非法关键字 / 非法类型 / 不可编译 pattern → 各路径 Err（不静默）。
- `response` 里 object/array/嵌套 → **允许**（不得被标量通道规则误拒）。
- 顶层拼错通道 → 三条路径（serve / build / openapi）**都** fail-fast。
- `schema_validation: false` 时，声明非法**仍**失败（审计不受开关影响）。
- 运行期：`response` 违反**不得**拦请求（击穿方向）。
- 组级 `response` + 动词级 `response` → 整体覆盖，非并集。
- openapi：有 `response` → 信封；无 → 占位不变（回归保护）。

## 2. RFC#3（v0.1.71）：DSL 表达力边界（下游按此读 openapi 生成 zod）

**不扩白名单**（评审共识：扩 `format` 之类要牵动运行期校验器 + openapi 映射 + 语义，
成本远大于收益；宁可写不了也不静默降级）。交付物 = 显式清单 + 钉死测试。

**支持（→ zod 映射）**：`type`（含类型数组 `["string","null"]`）/ `required` /
`properties` / `items` / `additionalProperties` / `minimum` / `maximum` /
`minLength` / `maxLength` / `pattern` / `enum` / `nullable` / `minItems` /
`maxItems` / `default`。

**不支持且 fail-fast**（下游迁移须人工改写）：`$ref` / `oneOf` / `allOf` / `anyOf` /
`not` / `format` / `patternProperties` / `dependencies` / `refine` / `transform` /
`discriminatedUnion` / `record` / 联合类型。
→ 联合类型用「可选字段 + 单形态」表达；`format`（email/date-time）改写为 `pattern`。

**跨语言红线**：`pattern` 是 **Rust regex**，≠ ECMA-262。下游把它搬进前端 zod 的
`new RegExp(...)` 时，lookahead / 反向引用等会**编译失败或语义漂移**（Rust 拒的字符串在
JS 放行 = 校验被绕过）。此红线写进 devkit，并由 v0.1.71 的对抗性测试钉死「未知关键字 /
Rust-only 语法」两侧行为。

## 3. 对外文档（devkit 四件 + openapi-guide）必须写清的话

1. `.schema` 第四通道 `response` 声明**出参结构**，`params`/`query`/`body` 声明入参。
2. **`response` 只做文档与生成真源，运行期不校验返回值**——少返回字段不会 400/500，
   也不会有任何告警。把它当写给前端的类型注释，不是守卫。
3. `response` 的**声明合法性**照旧 fail-fast，写错启动即失败。
4. `response` = `json.ok(data)` 的 **data payload**；openapi `responses.200` 出的是
   **完整信封** `{code,msg,data}`。`json.raw` / `json.stream` / `json.sse` 端点无信封，
   **不要**声明 `response`。
5. `.schema` 顶层只允许四通道，其余键装配期硬失败：拼错的通道此前从未生效。
6. 报 `unsupported channel` 说明该通道一直在裸奔——改名或删除，**不要用
   `schema_validation: false` 绕**（会关掉全部入参校验）。
7. 未声明 `response` 的端点，openapi 仍出占位 `{"200":{"description":"OK"}}`。

## 4. 依赖 spike：zod-rs / domainstack 能否替换自研 DSL（结论：否）

> 按「新增依赖动工前先 spike」惯例做的实证（真实 `cargo add` + `cargo tree` + 读上游源码）

### 4.1 实测数据

```
$ cargo add zod-rs domainstack    # 均解析到 1.1.1
$ cargo tree
├── domainstack v1.1.1
│   └── smallvec v1.16.3
└── zod-rs v1.1.1
    ├── regex v1.13.1 (+ aho-corasick / memchr / regex-automata / regex-syntax)
    ├── serde v1.0.229 (+ serde_derive)
    ├── serde_json v1.0.154
    ├── zod-rs-macros v1.1.1
    └── zod-rs-util v1.1.1
```

| crate | 版本 | 总下载 | 依赖面 | 仓库 | 最后发布 |
|---|---|---|---|---|---|
| `zod-rs` | 1.1.1 | 4,033 | 15 个传递依赖 | github.com/maulanasdqn/zod-rs | 2026-09-02 |
| `domainstack` | 1.1.1 | 2,878 | 1 个（smallvec） | github.com/blackwell-systems/domainstack | 2026-01-07 |

### 4.2 决定性障碍：数据流方向相反

两个 crate 都是 **Rust 规则优先**——schema 在 Rust 编译期构造，再向外生成：

- `zod-rs`：`pub use schema::{array, boolean, object, string, ...}` + `#[derive(ZodSchema)]`。
  全库 grep `from_json` / `JsonSchema` / `json_schema` → **零命中**。被校验的值可以是
  `serde_json::Value`，但「schema 本身」不行。
- `domainstack`：`rules::min_len(3).and(rules::max_len(20))`，`validate(name, value, &rule)`。
  同样 Rust 侧组合规则、校验 Rust 值。

oj 要的是**反向**：`.schema` 由 JS handler 运行时声明，经 V8 内省作为 `serde_json::Value`
落到 `RouteRow.schema`，Rust 侧要①审计这份声明，②拿它校验动态入参，③投影到 openapi。
**没有任何一个入口能吃「运行时给出的 schema 文档」。**

接进来等于先写一层「运行时 JSON → 它们的 builder」翻译层（正是现有 `contract.rs` 的
工作量），且必然丢掉红线：**白名单外关键字 fail-fast**——翻译层映射不到的关键字只能
静默丢弃，即本项目最重的缺陷类。

### 4.3 附带发现（domainstack 宣传与实现不符）

crates.io 简介写「Rust rules auto-generate JSON Schema + OpenAPI + TypeScript/Zod」，
但 1.1.1 的 `src/` 只有 `rules/{choice,collection,datetime,numeric,string}.rs`，
全库 grep `openapi|zod|json_schema|to_json_schema` **只命中 `lib.rs` 的文档文字**——
生成器不在 crate 里。4 个版本集中在 2026-01-06/07 两天发布后无更新。**不采用于任何路径。**

### 4.4 oj 独有语义（通用引擎没有）

- `params`/`query` 来源是 `HashMap<String,String>`：需**显式强转**（`"1"`→整数），
  且声明 array/object 是「永远无法满足的死契约」→ 构造期拒绝。
- `nullable` + 类型数组 `["string","null"]`。
- 数值 `enum` 按**值**比较（`Float(1.0)` 命中 `PosInt(1)`，v0.1.69 根因修复）。
- `pattern` 是 **Rust regex**，≠ ECMA-262，非法构造期 Err。
- 报错为带字段路径的中文串（`body.title: shorter than minLength 1`）。

**结论**：保留自研校验器（~570 行、零依赖）。

## 5. 四路评审意见与处置

评审在**实现开始后、完成前**打断执行（工程师 / 架构师 / 产品 / 安全各一路，只读）。
以下为逐条处置；**不全盘照做**，未采纳项写明理由。

| # | 来源 | 意见 | 处置 |
|---|---|---|---|
| B1 | 安全 | 审计应是唯一入口，不能靠三处调用点自律 | **采纳**：收口为 `RouteTable::audit_schemas()`（`serve` 已依赖 only-js）。openapi 一条调用覆盖 dev+release 两条发现路径 |
| B2 | 安全 | release `routes.js` 可能来自第三方 `.tgz`，其 `.schema` 属构建期不可信输入 | **采纳**：`audit_schemas` 在 `from_entries` 之后由 app / openapi 各自调用；release 不再依赖「本仓 build 审过」 |
| E1 | 工程 | 审计只挂装配期，`oj build` / `oj openapi` 漏 | **采纳**（同上） |
| E2 | 工程 | 审计位置应贴近登记门 | **采纳**：`RouteTable::audit_schemas` 遍历 `rows`，与内省/落盘/生成三处消费者同源 |
| E3 | 工程 | `&mut Vec::new()` 丢弃 pattern 像 bug | **采纳**：`audit_schema` 内注释说明「可编译性已验证，预编译由 `try_new` 负责」 |
| E4 | 工程/产品 | 白名单收紧的存量冲击 | **采纳**：报错带 method/pattern/handler/相对文件；CHANGELOG 标行为收紧；devkit 写明**不要**用 `schema_validation:false` 绕 |
| E5 | 工程 | 组级 `response` 不该做 required 并集 | **采纳**：改「动词级整体覆盖、无则继承组级」 |
| E6 | 工程 | `oj gen` 组形态 | **作废**（RFC#2 取消，无新子命令） |
| E7 | 工程 | 文档写「`try_new` 加第 4 参」，实现不同 | **采纳**：订正文档（实现更优——注册表不存无用字段） |
| A1 | 架构 | `response` 是 payload 还是整包信封未定义（**blocker**） | **采纳**：定为 `data` payload，openapi 输出完整信封；`json.raw`/`stream`/`sse` 端点不声明 |
| A2 | 架构 | client 生成把下游前端布局硬编码进平台件 | **采纳**：RFC#2 取消，改由下游读 openapi.json 生成 |
| A3 | 架构 | `oj openapi` 与 `oj gen openapi` 双入口 | **作废**（不再新增组） |
| A4 | 架构 | 联合类型不支持 | **采纳**：写入 RFC#3 不支持清单 |
| A5 | 架构 | 三次 bump 应合并为两次 | **采纳** |
| A6 | 架构 | 迁移要有可验证/可回退抓手 | **部分采纳**：openapi.json 即稳定交换物（`--check` 已有）；`oj test` 内做响应校验**登记为待做**（见 §7） |
| P1 | 产品 | 响应通道无出口 = 净负价值（**blocker**） | **采纳**：openapi 投影与响应通道同批落地 |
| P3 | 产品 | 「不校验」会被误解 | **采纳**：devkit 前置否定（§3 第 2 条） |
| P5 | 产品 | client 产物缺信封解包 | **转下游责任**；上游在 openapi 里给出信封 + `x-oj-response-enforced` |
| P6 | 产品 | `format`（email/date-time）必须补 | **不采纳**：扩白名单要牵动运行期校验器 + openapi 映射 + 语义，成本远大于收益；写进不支持清单，迁移时改写为 `pattern` |
| S1 | 安全 | openapi.json 是代码生成输入 → 注入面 | **部分采纳**：禁原型污染键名 + operationId 唯一性。属性名字符集/`enum`/`default` 的转义属**下游**责任（它们必须 `JSON.stringify` 嵌入），写进 §2 |
| S2 | 安全 | `required` 项应 ⊆ `properties` | **不采纳**：JSON Schema 允许「只声明必填不约束类型」，收紧会破坏存量；登记为下游注意事项 |
| S3 | 安全 | `x-oj-file` 绝对路径泄露 | **采纳**：改相对路径 |
| S4 | 安全 | `Object.assign` 的 `__proto__` 键问题 | **采纳**：改对象展开 |
| S5 | 安全 | `mergeSchema` getter 抛异常→整文件路由静默丢失 | **登记待做**（M1，与本次无关的前置问题） |
| S6 | 安全 | 审计无资源预算（pattern 长度/enum 条目/节点数） | **不采纳**：威胁模型是「自己（或明确安装的第三方模块）写的代码」，`Regex::new` 已有 size_limit；加预算属过度设计，登记为已知边界 |
| S7 | 安全 | `response` 不校验 → 字段级越权伪保证 | **采纳**：产物自带 `x-oj-response-enforced: false` + 文档禁止当脱敏边界 |

## 6. 版本与文档

两特性各 bump 一次：`0.1.69 → 0.1.70`（响应通道 + openapi 投影）
`→ 0.1.71`（DSL 边界清单）。每次按发版自查（`oj/Cargo.toml` + `Cargo.lock` +
CHANGELOG 小节 + devkit 四件 + `cargo xtask build`）。本轮**不打标签**（等下游联调后发版）。

原本计划三次 bump（含 client 生成器），RFC#2 取消后合并为两次；边界清单（RFC#3）
与响应通道同属「声明面」，但它是**对外承诺**且不含 API 变更，单独一版更清楚下游要对齐什么。

## 7. 登记待做（不在本轮）

- **`oj test` 内做响应契约校验**：安全评审指出「运行期无处可去」在测试里不成立——
  测试失败有地方可去。这是让 `response` 从纯声明升级为「可验证声明」的正解，
  但属于新能力，单独排期。
- **dev 内省整文件失败目前只是 warning**（app.rs 仅 warn，该文件全部路由静默消失）。
  schema 已成安全相关数据，可考虑给「整文件内省失败」一个致命开关。
- **`required` 项 ⊄ `properties` 的语义澄清**：留给下游 codegen 自行决策（可跳过不存在的键）。
