# api.ts 嵌套 handler 组（v0.1.27+）Implementation Plan

> 状态：**已实施（v0.1.64，未打标签）** —— 2026-10-12 按 TDD 分三批落地完毕，门禁全绿
> （workspace 1230 用例 / clippy `-D warnings` / xtask 契约 8 用例 / `cargo fmt --check` 零 diff）。
> 用户可见变更见 `CHANGELOG.md` v0.1.64 与 devkit 四件（含「何时采用」DDD 选型指南与
> `sample/src/nest/` 实体为根示例）；落地偏差与处置记录见 §八.5。

**Goal:** 允许 `api.ts` 的 default 导出里用**非动词键挂载嵌套 handler 组**
（`export default { get, post, upload: { get, post } }`），使「一个文件多个同动词端点」
不再必须拆子目录，同时**不再静默丢弃**声明了 `.route` 却注册不上的导出键。

**起因（oj 侧实证）**：一个 api.ts 里 `export default { post, upload }`，其中 `upload` 挂了
`.route`。oj 只认 7 个动词导出键，`upload` 被**静默丢弃** → dev 与 release 两态该端点恒
404，且无任何告警。已用 `bin/oj build` 复现（v0.1.62，临时模块 `export default { post, upload }`，
两者均挂 `.route`）：routes.js 只剩 post 一行，`upload` 无迹可寻。

**Tech Stack:** Rust（deno_core 0.411 / axum / matchit 0.8）、JS ESM。相关 spec/文档：
`docs/route-params-design.md`（`.route` 语义）、`docs/devkit/api-manual.md`。

## Global Constraints

- 所有 cargo 命令一律 `--release`；lint：`cargo clippy --release --all-targets -- -D warnings`；
  测试：`cargo test --release --workspace`。
- `bootstrap.js` 保持 7-bit ASCII（本方案不改它）。
- 每次版本更新同步 devkit 四件（`api-manual.md` / `scenarios.md` / `SKILL.md` / `README.md`）
  并 `cargo xtask build` 归置 `bin/devkit/`，由 `cargo test --release -p xtask` 契约用例校验。
- 只改本仓（oj-bin）。oj-module 为只读，跨仓影响只登记不擅改。

> **目录改名注记**：源目录已于 2026-10 由 `server/` 改名 `serve/`。下文 `routes.rs` /
> `actor.rs` / `lib.rs` 均位于 `serve/src/`。§一/§三 内行号已于 2026-10-12 按当前
> `serve/src/`（HEAD v0.1.62）逐条重对；若相关文件在落地前再有改动，仍须抽验
> （重点符号：`METHODS`/`Entry`/`Lookup::Hit`/`register`/`build`/`from_entries`/
> `decls_from_value` 与派发点 `lib.rs`/`actor.rs`）。

## 一、现状证据（改前事实）

| 事实 | 位置 |
|---|---|
| 动词白名单 7 个：`get/post/put/del/patch/head/options` | `serve/src/routes.rs:254`（`METHODS`） |
| 非动词键直接 `continue`（静默，无 failure） | `serve/src/routes.rs:309-312`（`build` 内） |
| 内省也只认这 7 个键，非动词键连采集都进不去 | `src/bridge/mod.rs:1260-1268`（`introspect_module`，`:1250`） |
| 运行期按方法名取函数：`m.default[method]`，否则 405 | `src/bridge/mod.rs:1197-1202` |
| 路由条目只存文件，不存 handler：`Entry::File(FileId)` | `serve/src/routes.rs:203-206` |
| 查表结果只回 file/params：`Lookup::Hit { file, params }` | `serve/src/routes.rs:210-213` |
| handler 名 = HTTP 方法名，由 `method_name(verb)` 推出 | `serve/src/lib.rs:732-733`、`serve/src/actor.rs:114-128`（`Job::Module`） |
| release 路由行结构 `{ method, pattern, file, schema? }`（`schema` 自 v0.1.44 起可选） | `serve/src/routes.rs:570-578`（`RouteEntry`）；写入方 `oj/src/build_cmd.rs:437-459` |
| 内省结果 Value → decls（扁平） | `serve/src/routes.rs:603-624`（`decls_from_value`） |
| dev 建表 failures 只打印不致命；release `routes.js` 未知 method → 启动 Err | `oj/src/app.rs:1423-1438`（打印+计数）；`serve/src/routes.rs:345-351`（failure）+ `oj/src/app.rs:1414-1417`（Err） |
| **已具备**：静态段与参数段可共存且静态优先（`/x/{a}` + `/x/upload`） | `serve/src/routes.rs:1206`（`table_static_sibling_beats_same_verb_param`） |
| **已具备**：尾斜杠归一（`/x/{id}/` 命中 `.route = "/x/{id}"`） | `serve/src/routes.rs:125-140`（`normalize`） |

## 二、语义提案

记 `P` = 文件的目录镜像基 `/{base}/{rel_dir}`。每层（顶层文件或子组）可挂 `.route`：

| 层 | `.route` | 本层前缀 |
|---|---|---|
| 文件/组 | 有，且以 `/` 开头 | `/{base}{R}`（根级） |
| 文件/组 | 有，且相对 | `{父前缀}/{R}` |
| 顶层文件 | 无 | `P` |
| 子组 | 无 | `{父前缀}/{组名}` |

- **动词键**（`get/post/put/del/patch/head/options`）：有 `.route` 按上表解析，无则取本层前缀。
- **非动词键 + 对象值** = 子组，递归解析（组名即路径段，与目录镜像同构）。
- **非动词键 + 函数值** = **报错**（不注册）：不是动词也不是组，此前静默丢弃，正是本次要消灭的陷阱。
- **组级 `.route` 作整组默认前缀**：组挂 `.route` 时，组内**未单独挂** `.route` 的动词以其组路由
  为 pattern；动词自身 `.route` 优先覆盖组前缀。例：`cccc.route="/x"` 且 `cccc.get` 无自身
  `.route` → `GET /x`；`cccc.post.route="/y"` → `POST /y` 覆盖。
- **深度上限（拟 4 层）+ 循环引用检测**：二者均在 JS 内省 driver 内完成——每层携带
  `seen = new WeakSet()` 与 `depth` 计数器，任一触发（`depth > 4` 或对象已在 `seen`）即
  `json.fail` 该文件（dev 记 failure、build 报错），**不注册、不写入 routes.js**。
  深度上限不足以拦自引用（`const a={}; a.self=a;` 深度恒为 2 却无限递归），必须叠 `WeakSet`
  环检测（专家评审结论，安全/架构两侧共认的高危项）。

典型用例（同文件多端点，不再必须拆子目录）：

```ts
async function post() { /* 既有顶层实现 */ }
post.route = "/x/{anchor}";

const upload = { post: async () => { /* 组内 handler */ } };
upload.route = "/x/{anchor}/upload";   // 组前缀（根级写法）

export default { post, upload };
```
→ `POST /{base}/x/{anchor}/upload` 命中 `upload.post`。
`upload: { get, post }` 同形（两个动词共享组前缀）。

### 二.A 函数 vs 子组的判别红线（示例驱动，2026-10-12 增强）

下方示例证明两件事：**① 一个组可同时含动词 handler 与嵌套子组；② handler 函数可挂任意
属性（`get.schema = {}`），这些属性必须被路由层当作「元数据」而非「子路由」忽略掉。**

```js
// aaa/bbb/api.ts
function get() { /* 顶层动词 handler → GET /{base}/aaa/bbb/ */ }

const cccc = {
  get: () => { /* GET /{base}/aaa/bbb/cccc */ },
  post: () => { /* POST /{base}/aaa/bbb/cccc */ },
  list: {                               // 非动词键 + 对象值 = 子组（示例里 ()->{} 为笔误）
    get: () => { /* GET /{base}/aaa/bbb/cccc/list */ },
  },
};
cccc.get.schema = {};                   // 挂在函数上的契约元数据，不是路由（对象字面量外赋值）

export default { get, cccc };
```

派生路由（4 条；`cccc` 同时含动词与子组，拓扑合法）：

| handler 链 | 方法 | pattern |
|---|---|---|
| `get` | get | `/{base}/aaa/bbb/` |
| `cccc.get` | get | `/{base}/aaa/bbb/cccc` |
| `cccc.post` | post | `/{base}/aaa/bbb/cccc` |
| `cccc.list.get` | get | `/{base}/aaa/bbb/cccc/list` |

**判别规则（递归内省的唯一权威，先按「键是否 verb」分派，再用 `typeof` 校验值）**：

- **键是 verb**（`get/post/put/del/patch/head/options`）→ 必须是 **handler 端点**：
  值 `typeof === "function"` 才合法，读其 `.route` / `.schema`（及任何用户属性）作为元数据；
  值若是对象 / 数组 / 基元 → **报错**（verb 键下不允许挂子组或常量）。
- **键非 verb** → 必须是 **子组**：值 `typeof === "object"` 且 `!== null` 且
  `!Array.isArray(v)` 才递归；值若是 **函数 / 数组 / 基元** → **报错**
  （原方案只报「非动词函数值」，现放宽到「数组/基元也报错」，把组对象上误挂的
  `cccc.foo = []` 之类一并挡在门外）。

> 即：**只有 verb 键才是 handler 端点**，与「函数是否带属性」无关——`get.schema = {}` 仍只是
> `get` 这一个端点，`schema` 不会被拆成子路由。示例里 `get.schema` 挂在**函数** `cccc.get`
> 上，不出现在 `Object.keys(cccc)` 中（自然安全）；handler / 子组的判别一律先以「键是否 verb」
> 分派、再以 `typeof` 校验，元数据只挂在 handler 函数上（与现有 `introspect_module` 读
> `fn.schema` 的写法同构）。

## 三、改动面

1. **`src/bridge/mod.rs`**
   - `introspect_module`（`:1250`）：内省改为递归遍历 `Object.keys(default)`（**dev 内省与
     `oj build` 共用同一 driver 片段**，避免两态分歧），按 §二.A 判别 verb 端点 / 子组；为每个
     leaf 产出 `{ chain: "cccc.get", method: "get", route, schema, top_level: bool }`，并**在写入
     `handler` 链前对每段做白名单+排除校验（权威校验点，见 §四）**；携带 `seen: WeakSet` + `depth`
     做环检测与深度上限（见 §二）。非 verb 键的非法值（函数/数组/基元）→ push 可操作 failure。
   - `run_module`（`:1158`）：driver 入参由 `method` 改为承载 **handler 链**（末段即动词，
     `method` 可由链末段推出，用于契约校验键与 405 文案）；取值走 §四 的计算式逐段
     `hasOwnProperty`（`Object.prototype.hasOwnProperty.call` 形式，见 §四）+ 末段
     `typeof==="function"` 守卫，**不再用 method 反查**。
2. **`serve/src/routes.rs`**
   - `Entry` 改为 `Entry::File(FileId, String /*handler 链*/)`；`Lookup::Hit` 增 `handler: String`
     （不再只回 file/params）。`register` 签名增 `handler`；`build`/`from_entries` 各自透传。
   - `build` 假内省闭包返回类型与 §三.1 内省产物**同构**——扁平 `(chain, method, route, schema)`
     行（**不另造树形 `RouteDecl` 中间结构**：树形会多一条 Rust 侧递归解析路径，正是 A3 双态
     分歧面）；连带 `build`（`:272-280`）、`bridge_introspector`（`:629-649`）、`decls_from_value`
     （`:603-624`）与测试夹具 `tbl()`（`:957-991`）；非 verb 键非法值 → push 可操作 failure。
   - `decls_from_value`（dev 解析内省 Value）与 `from_entries`/`entries_from_value`（release 直读
     routes.js）都解析 `handler`：逐段套 `^[A-Za-z_$][A-Za-z0-9_$]*$` 且排除
     `__proto__/constructor/prototype`，违则记 failure 不注册。**`RouteEntry` 增
     `handler: Option<String>`**；`entries_from_value` 解析，当 `None` 时以 `method` 兜底填链
     （旧产物兼容，见 §四）。
   - `replaced`：组内 verb 端点各自 `replaced.insert((fid, verb))`；`is_replaced` 按 verb 查，
     组成员兜底仍生效（只记链长 1 的顶层动词的旧语义保持；组 `.route` 由其下 verb 各自 replaced，
     不误伤顶层 `get` 对 `P` 的所有权）。
3. **派发透传点（行号已按当前 `serve/src` 重对）**：`serve/src/lib.rs` 有**两处**
   `run_module` 调用——`run_route` 内 `:798` 的 `run_module(file, m, …)`（主派发）与
   `dispatch_meta_handler` 内 `:1064` 的 `run_module`（meta handler 合成派发，同样经
   `Lookup::Hit` 取链）；传播链还有 `serve/src/actor.rs` 的 `JsActor::run_module`
   （`:172-190`）与 `Job::Module{ method }` 分支（`:114-128`）、`src/bridge/mod.rs`
   `run_module` 签名（`:1158`，入参改承载 handler 链）。两处派发都直接拿
   `Lookup::Hit.handler`，**不再用 method 反查**；`method` 仅用于契约校验键与 405 文案。
4. **`oj/src/build_cmd.rs:437-459`**：routes.js 行增 `handler` 字段（继 v0.1.44 `schema`
   之后，兼容写法照抄 `Option` 先例）；`oj/src/app.rs:1408-1413` release 拼接带上。
5. **文档**：devkit 四件（路由章节 + SKILL 陷阱速查）+ `CHANGELOG.md`。

## 四、兼容性与安全

- **旧产物兼容**：routes.js 缺 `handler` 时回落同名方法名 → 老 dist 照常启动，不必重新 build
  （实现点：`RouteEntry.handler: Option<String>`，`entries_from_value` 解析，当 `None` 以 `method`
  兜底填链）。
- **原型链安全 + 段名校验（权威点在内省、运行期纵深）**：
  - **内省期即权威校验**：`introspect_module` 写入 `handler` 链前，对每段套
    `^[A-Za-z_$][A-Za-z0-9_$]*$` 且排除 `__proto__ / constructor / prototype`，违则
    `json.fail` 该文件 → routes.js 永不含非法段（可信源）。
  - **运行期 driver 纵深**（仍必须做，防手改/旧 routes.js）：逐段校验 +
    `Object.prototype.hasOwnProperty.call`（**勿用 `cur.hasOwnProperty(s)`**——`hasOwnProperty`
    自身可被污染，且组对象为 `Object.create(null)` 时无该方法）+ 中间段 `typeof === "object"`
    （才能下钻；该判定天然排除 `null`，无需再写 `cur === null`）、末段 `typeof === "function"`。
    推荐片段（`const m = await import(...)` 须与现有 body 同样置于首行，见
    `src/bridge/mod.rs:1197-1202` 的拼接顺序；整段经 `driver_script` 包装后由 `__ojRun`
    异步执行，`return`/顶层 `await` 合法）：
    ```js
    const m = await import("<spec>");
    const SEGS = /* handler 链按 "." 拆分，每段已 JSON 字面量嵌入 */;
    let cur = m.default;
    for (let i = 0; i < SEGS.length; i++) {
      const s = SEGS[i];
      if (!/^[A-Za-z_$][A-Za-z0-9_$]*$/.test(s) ||
          s === "__proto__" || s === "constructor" || s === "prototype")
        return json.fail(404, "bad segment");
      if (!cur || typeof cur !== "object" || !Object.prototype.hasOwnProperty.call(cur, s))
        return json.fail(404, "no handler");
      cur = cur[s];
    }
    if (typeof cur !== "function") json.fail(405, "method not exported");
    else await cur();
    ```
  - 段名一律 `serde_json::to_string(seg)` 后以 **JSON 字符串字面量**嵌入（沿用 `method_lit` 写法），
    **禁止裸拼接**，杜绝引号/换行破坏 driver 语法或注入。
- **handler 链一律计算式访问**（已含于上片段）：`m.default[SEG0][SEG1]…`，**不用点访问**——
  组名可能含连字符等非标识符（`user-list`），计算式兼容且天然防 `m.default.xxx` 注入。
- **循环引用 / 深度（DoS 防护）**：见 §二——`WeakSet` 环检测 + 深度上限 4 在 JS 内省 driver
  内完成；`a.self=a` 之类自引用在内省期即 failure，不会拖死 build/启动。
- **dev 目录镜像兜底**：`replaced` 语义保持不变（见 §三.2 最后一条），避免回归。
- **冲突检测**：仍按 (pattern, method) 判定，嵌套不改变 Conflict 语义。

## 五、风险与未决

- **跨仓（不可自愈，只登记）**：oj-module `packages/cli/src/contract/check.ts:108-115` 的 `VERB_OF`
  只认动词键（非动词属性在 `:169-171` 静默 `continue`），AST 扫描会把 `upload: { post }`
  判成「端点未实现」。该仓按规矩只读，需用户授权或另开一轮同步。**不擅改。**
- **API 面新增**：这是新语法（嵌套组 + 组级 `.route`），属特性而非修 bug，需随版本在 devkit 写明。
- **待用户拍板的默认**（本文档按这些默认写，用户可改）：
  ① 组的 `.route` 支持绝对/相对两种；② 组无 `.route` → 父前缀 + 组名；
  ③ 非动词函数键挂 `.route` → 报错；④ 旧 routes.js 无 `handler` → 回落同名方法名；⑤ 深度上限 4。

## 六、验收（机读）

1. `export default { post, upload: { post } }` + 组 `.route` → `POST /{base}/.../upload` 命中
   `upload.post`（dev 与 release 两态一致）。
2. `upload: { get, post }` → 同路径 GET/POST 分别命中两个 handler。
3. 组无 `.route` → 挂在 `{父前缀}/{组名}`。
4. 非动词函数键挂 `.route` → 启动/`oj build` 打印可操作错误，且不写入 routes.js。
5. 旧 routes.js（无 `handler`）→ 正常启动，行为不变。
6. 门禁：`cargo fmt --check` / `cargo clippy --release --all-targets -- -D warnings` /
   `cargo test --release --workspace` 全绿；devkit 契约用例（`cargo test --release -p xtask`）全绿。
7. **§二.A 示例**：`aaa/bbb/api.ts` 派生 4 条路由（`get` / `cccc.get` / `cccc.post` /
   `cccc.list.get`）dev 与 release 两态一致；`cccc` 同时含动词与子组拓扑合法。
8. **元数据隔离**：`cccc.get.schema = {}` 被当作该 handler 的入参契约读取，且**不**生成
   任何名为 `schema` 的路由；内省输出 `decls_from_value` 不含 `schema` 键。
9. **判别红线**：组对象上挂非动词数组/基元（如 `cccc.foo = []` / `= 1`）→ 启动/`oj build`
   打印可操作错误，不写入 routes.js；非动词函数值仍按 §二报错（规则放宽到「数组/基元也报错」）。
10. **循环引用 / 深度上限**：`const a={}; a.self=a;` 或嵌套 >4 层 → dev 内省记 failure、
    `oj build` 报错，不注册、不写入 routes.js（不会 hang/OOM）。
11. **双态不变量**：同一 `api.ts`，`oj build` 的 dev 内省产物与其生成的 `routes.js` 被 release
    直读后，`lookup(method, path)` 结果逐一相等（dev 与 release 两态零分歧）。
12. **非标识符段名 + 原型注入**：组名含连字符（`user-list`）的嵌套 handler 经计算式访问正确
    命中；`routes.js` 手改 `handler:"__proto__.get"`（或 `constructor`/`prototype`、非法段名）
    → release 启动/`oj build` 记 failure 不注册（§四 双保险）。
13. **组级 `.route` 默认前缀**：`cccc.route="/x"` 且 `cccc.get` 无自身 `.route` → `GET /x`；
    `cccc.post.route="/y"` → `POST /y` 覆盖组前缀（§二 语义）。

## 七、落地步骤

- [x] 内省递归化（`src/bridge/mod.rs:1250`）+ `run_module` handler 链（`src/bridge/mod.rs:1158`）
- [x] `serve/src/routes.rs`：Entry/Lookup 带链、递归建表、来自 routes.js 的 handler 校验
- [x] `serve/src/actor.rs`、`serve/src/lib.rs` 透传 handler
- [x] `oj/src/build_cmd.rs`、`oj/src/app.rs`：routes.js 写入与读取 handler
- [x] 回归测试（routes 单测 + 内省单测 + build 产物断言）+ **§二.A 示例端到端用例**（4 路由 +
   `schema` 元数据隔离 + 组含动词与子组）+ 判别红线用例（组上挂数组/基元报错）
- [x] **集成测试（专家评审缺口，必做）**：内省递归 → `build` → `Lookup::Hit` 带链 → driver 逐段
   取值命中嵌套 handler（dev 态）；release 态 routes.js 直载同源命中；非标识符段名（`user-list`）
   计算式访问；组 `.route` 后目录镜像 404；`from_entries` 注入 `handler:"__proto__.get"` 非法段
   → failure；双态不变量（验收 11）。建议落 `oj/tests/e2e.rs` + `serve/src/routes.rs` 的
   `from_entries` 单测 + `src/bridge` 内省/派发单测。
- [x] devkit 四件 + CHANGELOG + `cargo xtask build`
- [x] 门禁复跑 + 专家评审处置（架构侧 / 工程侧 / 安全侧，结论见 §八）+ 本文档 §八 处置记录

## 八、评审意见与处置（2026-10-12 三方专家评审）

邀请**架构师 / 工程师 / 安全专家**三方并行评审，结论如下。**全部采纳**（无驳回项），
已落到 §二 / §二.A / §三 / §四 / §六 / §七；行号引用已于 2026-10-12 按当前 `serve/src/`
（HEAD v0.1.62）逐条重对。

### 8.1 架构侧

| # | 意见 | 严重度 | 处置 | 落点 |
|---|---|---|---|---|
| A1 | handler 链必须进 `Entry`/`Lookup::Hit`，否则 `run_module` 只取顶层 `method` 会静默误派发（嵌套组 verb 误跑顶层同名 handler） | 阻断 | 采纳：`Entry::File(FileId,String)`、`Lookup::Hit.handler`，actor 直接拿链派发，不再 method 反查 | §三.1/§三.2/§三.3 |
| A2 | 深度上限拦不住 `a.self=a` 循环引用，会拖死内省 | 高 | 采纳：`WeakSet` 环检测 + 深度上限，dev/build 共用 driver | §二 / §四 |
| A3 | dev（`decls_from_value`）与 release（`from_entries`）双解析路径可能分歧 | 高 | 采纳：统一 `(chain,method,route,schema)` 序列化 + 双态不变量测试 | §三.2 / §六.11 |
| A4 | 组级 `.route` 对同组无自身 `.route` 动词的前缀语义未明 | 中 | 采纳：组 `.route` 作整组默认前缀，动词自身 `.route` 优先 | §二 / §六.13 |
| A5 | `replaced` 须只对链长 1 + 内省标记顶层 | 中 | 采纳：组内 verb 各自 `replaced.insert(fid,verb)` | §三.2 |
| A6 | `from_entries` 未校验 handler 段名，与 §四 白名单脱节 | 中 | 采纳：release 入口逐段套白名单+排除 | §三.2 / §四 |
| A7 | §二.A 示例 `get.schema={}` 写在对象字面量内非法 JS；全文 `server/` 已改名 `serve/` | 提示 | 采纳：示例改对象外赋值；目录引用全改 `serve/` | §二.A / §一注记 |

### 8.2 工程侧

| # | 意见 | 严重度 | 处置 | 落点 |
|---|---|---|---|---|
| E1 | §三 透传点行号失效（真派发在 `lib.rs:798`/`actor.rs Job::Module`，非 `556/696`） | 高 | 采纳：重写 §三.3 并标注重对 | §三.3 / §一注记 |
| E2 | `Lookup::Hit` 缺链 + `build` 收链缺口（闭包三元组丢链） | 高 | 采纳：闭包展开含链行、`register` 增 `handler` | §三.2 |
| E3 | release 直载段名/原型防护漏（不跑 JS） | 中/高 | 采纳：`from_entries` 校验 | §三.2 / §四 |
| E4 | 旧产物兼容落地缺口（`RouteEntry` 无 `handler` 字段） | 中 | 采纳：`handler: Option<String>` + `None` 时 fallback method | §三.2 / §四 |
| E5 | `replaced` + 嵌套 dev 兜底含糊 | 中 | 采纳：组内 verb 各自 replaced | §三.2 |
| E6 | 缺「内省递归 + run_module 全链路」集成测试 | 高 | 采纳：§六.10-13 + §七 集成测试步骤 | §六 / §七 |

### 8.3 安全侧

| # | 意见 | 严重度 | 处置 | 落点 |
|---|---|---|---|---|
| S1 | 循环引用 → 内省无限递归（DoS） | 高 | 采纳：`WeakSet` + 深度（同 A2） | §二 / §四 |
| S2 | 原型链计算式取值需「排除先于取值 + hasOwnProperty + 中间段 typeof object / 末段 function」双保险 | 高 | 采纳：§四 推荐 driver 片段 | §四 |
| S3 | 段名校验时序差（先写 routes.js 再校验 → 污染落盘） | 中 | 采纳：内省期即权威校验，运行期纵深 | §四 |
| S4 | 段字面量注入（裸拼接断语法） | 高 | 采纳：每段 `serde_json::to_string` 嵌入 | §四 |
| S5 | `Object.prototype` 已污染 → 需 `hasOwnProperty` | 中 | 采纳（含于 S2 片段） | §四 |
| S6 | verb 键值为 null / 函数挂属性 | 提示 | 确认安全，无需改（§二.A 划分正确） | — |
| S7 | `.route`/`.schema` 透传 | 低 | 确认现状安全（matchit 匹配非 eval；schema 注册期 fail-fast） | — |

### 8.4 仍未决（非本方案阻塞，沿用 §五）

- 跨仓 oj-module `VERB_OF` 只读问题依旧（AST 扫描仍会把 `cccc:{get,post}` 判「端点未实现」），只登记不擅改。
- 深度上限具体值（4）与白名单正则，落地时若需微调以评审结论为准，不阻塞开工。

### 8.5 落地处置记录（2026-10-12 实施，TDD 三批）

实施与方案的**有意偏差**（均不改变验收结论），逐条登记：

| # | 偏差 | 依据 / 理由 |
|---|---|---|
| D1 | 段名校验未用 `^[A-Za-z_$][A-Za-z0-9_$]*$`，改为「非空 + 不含 `/` + 排除 `__proto__`/`constructor`/`prototype`」 | 原正则拒绝连字符组名，与验收 12（`user-list` 命中）冲突；按 §8.4 授权微调。计算式访问对任意字符串安全，真实风险（注入/原型）由 JSON 字面量嵌入 + 排除项 + `hasOwnProperty.call` 覆盖。Rust 侧同口径实为 `legal_handler_chain` |
| D2 | JS 内省 leaf 只产 `{chain, route, schema}`；`method`/`top_level` 由 Rust 从链尾推导 | DRY：verb 判定在 JS 已做，Rust 再存一份必漂移；也少一处 A3 分歧面。METHODS 白名单在 `build` 保留作纵深防线 |
| D3 | 文件层（root default）字符串 `.route` 实现为整文件默认前缀；`route` 成为组/文件对象保留键 | §二 语义表「文件」行的落地；组自身 `.route` 不再被子键遍历撞见（实现期发现的真问题） |
| D4 | 环检测用**路径** WeakSet（进/出删），非全局 seen | 全局 seen 会误杀「同组对象挂两个键」的合法 DAG；路径集 + 深度上限 4 双重满足 S1 |
| D5 | driver 允许 object\|function 中间段下钻 | 保留旧语义（`m.default[method]` 可读函数导出上的属性）；与内省 `Object.keys(fn)` 口径一致，防双态分歧 |
| D6 | 缺段（含顶层方法未导出）→ 404；「存在但非函数」→ 405 | §四 推荐片段的字面语义；替代旧的「一律 405」，已写入 CHANGELOG 行为变化 |
| D7 | routes.js 仅在 `chain != method`（嵌套行）时写 `handler` 字段 | 顶层动词行与旧产物字节一致；旧产物无字段 → `method` 兜底填链（E4），双兼容 |
| D8 | 入参契约键由 (文件, 方法) 改为 (文件, handler 链) | 嵌套组同文件同动词的契约互不串门；`dispatch_meta_handler` 同走链 |

**门禁结果**：`cargo test --release --workspace` 1230 通过 / 0 失败；`cargo clippy --release
--all-targets -- -D warnings` 零告警；`cargo test --release -p xtask` 8 通过（bin/devkit 与
docs/devkit 一致）；`cargo fmt --check` 零 diff。HEAD 遗留的 11 处 fmt 漂移
（`oj/src/app.rs:1254`、`oj/src/serve_cmd.rs` 6 处、`serve/src/reload.rs` 5 处，均出自
config 热更提交 d641a62/5a92f8e）在本方案的 `cargo xtask build`（发布管线自带 fmt）中
被一并格式化，随本次提交落入，未单独拆分。

## 附录 A：在途改动（已回退，开工时第一步重做）

方案定稿前曾在 `src/bridge/mod.rs` 做了半步：内省由「只遍历 7 个动词键」改为「遍历全部函数导出键」。
它单独落地会**引入回归**（`oj build` 无动词过滤，会把 `upload` 写成 `method: "upload"`，
release 启动在 `serve/src/routes.rs:345-351` 报 `routes.js: unknown method` 而 fatal），故已 `git checkout` 回退。
开工时按 §七第 1 步连同动词过滤一起做：

```diff
-/// 启动期内省：import api 模块、读 default[method].route ……（{"get": "{id}" | null, ...}）
+/// 启动期内省：import api 模块、读 default[键].route ……（{"get": ..., "upload": { ... }}）
-             for (const k of ["get","post","put","del","patch","head","options"]) {\
-               const fn = m.default && m.default[k];\
+             const d = m.default || {};\
+             for (const k of Object.keys(d)) {\
+               const fn = d[k];\
```
