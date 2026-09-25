# api.ts 嵌套 handler 组（v0.1.27+）Implementation Plan

> 状态：**待定（pending）** —— 方案已定稿，等用户拍板后开工。落此文档前的需求与复核结论见下。

**Goal:** 允许 `api.ts` 的 default 导出里用**非动词键挂载嵌套 handler 组**
（`export default { get, post, upload: { get, post } }`），使「一个文件多个同动词端点」
不再必须拆子目录，同时**不再静默丢弃**声明了 `.route` 却注册不上的导出键。

**起因（下游实证）**：plane 为「富文本内嵌图上传」写了 `POST /public/assets/v2/anchor/{anchor}/upload`，
实现形态是 `upload.route = "..."` + `export default { post, upload }`。oj 只认 7 个动词导出键，
`upload` 被**静默丢弃** → dev 与 release 两态该端点恒 404。已用 `bin/oj build` 复现：routes.js 只剩
`{ method: "post", pattern: "public/assets/v2/anchor/{anchor}", ... }` 一行，无任何告警。

**Tech Stack:** Rust（deno_core 0.411 / axum / matchit 0.8）、JS ESM。相关 spec/文档：
`docs/route-params-design.md`（`.route` 语义）、`docs/devkit/api-manual.md`。

## Global Constraints

- 所有 cargo 命令一律 `--release`；lint：`cargo clippy --release --all-targets -- -D warnings`；
  测试：`cargo test --release --workspace`。
- `bootstrap.js` 保持 7-bit ASCII（本方案不改它）。
- 每次版本更新同步 devkit 四件（`api-manual.md` / `scenarios.md` / `SKILL.md` / `README.md`）
  并 `cargo xtask build` 归置 `bin/devkit/`，由 `cargo test --release -p xtask` 契约用例校验。
- 只改本仓（oj-bin）。oj-module / plane 为只读，跨仓影响只登记不擅改。

## 一、现状证据（改前事实）

| 事实 | 位置 |
|---|---|
| 动词白名单 7 个：`get/post/put/del/patch/head/options` | `server/src/routes.rs:159` |
| 非动词键直接 `continue`（静默，无 failure） | `server/src/routes.rs:205-208` |
| 内省也只认这 7 个键，`upload` 连采集都进不去 | `src/bridge/mod.rs:953`（改前） |
| 运行期按方法名取函数：`m.default[method]`，否则 405 | `src/bridge/mod.rs:922-924` |
| 路由条目只存文件，不存 handler：`Entry::File(FileId)` | `server/src/routes.rs:112` |
| 查表结果只回 file/params：`Lookup::Hit { file, params }` | `server/src/routes.rs:118` |
| handler 名 = HTTP 方法名，由 `method_name(verb)` 推出 | `server/src/lib.rs:556`、`server/src/actor.rs:115` |
| release 路由行结构 `{ method, pattern, file }` | `server/src/routes.rs:422` |
| 内省结果 Value → decls（扁平） | `server/src/routes.rs:446` |
| dev 建表 failures 只打印不致命；release `routes.js` 未知 method → 启动 Err | `oj/src/app.rs:876-884`、`server/src/routes.rs:231-236` |
| **已具备**：静态段与参数段可共存且静态优先（`/x/{a}` + `/x/upload`） | `server/src/routes.rs:868`（`table_static_sibling_beats_same_verb_param`） |
| **已具备**：尾斜杠归一（`/x/{id}/` 命中 `.route = "/x/{id}"`） | `server/src/routes.rs:59` |

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
- 深度上限（拟 4 层），超限记 failure 不注册，防病态/自引用对象拖死内省。

plane 用例落成（URL 一字不变）：

```ts
async function post() { /* 现有两段式 presigned */ }
post.route = "/public/assets/v2/anchor/{anchor}";

const upload = { post: async () => { /* 服务端直收 → json.ok({ asset_id, asset_url }) */ } };
upload.route = "/public/assets/v2/anchor/{anchor}/upload";   // 组前缀（根级写法）

export default { post, upload };
```
→ `POST /api/public/assets/v2/anchor/{anchor}/upload` 命中 `upload.post`。
`upload: { get, post }` 同形（两个动词共享组前缀）。

## 三、改动面

1. **`src/bridge/mod.rs`**
   - `introspect_module`(`:944`)：内省改为递归产出 `{route, fns, groups}`（先遍历全部导出键，再进对象）。
   - `run_module`(`:920`)：driver 由 `m.default[method]` 改为**沿 handler 链取值**，逐段
     `hasOwnProperty` 守卫；405 文案带 handler 链。
2. **`server/src/routes.rs`**
   - `Entry` / `Lookup::Hit` 增 `handler` 链（点分，如 `upload.post`；顶层动词 = 同名）。
   - `build`(`:177`)：假内省闭包返回类型改树形 `RouteDecl { name, route, children }`，递归建表；
     非动词函数键挂 `.route` → push 可操作 failure。
   - `decls_from_value`(`:446`) 解析新结构；`from_entries`(`:422`) 解析 `handler` 并校验段名。
   - `replaced` **只记顶层动词**（链长为 1），否则组挂 `.route` 会误伤顶层 `get` 对 `P` 的所有权。
3. **`server/src/actor.rs:115`、`server/src/lib.rs:556/696`**：handler 链透传给 `run_module`。
4. **`oj/src/build_cmd.rs:391-408`**：routes.js 增 `handler` 字段；`oj/src/app.rs:865` release 拼接带上。
5. **文档**：devkit 四件（路由章节 + SKILL 陷阱速查）+ `CHANGELOG.md`。

## 四、兼容性与安全

- **旧产物兼容**：routes.js 缺 `handler` 时回落同名方法名 → 老 dist 照常启动，不必重新 build。
- **原型链安全**：handler 段名白名单 `^[A-Za-z_$][A-Za-z0-9_$]*$` 且显式排除
  `__proto__ / constructor / prototype`；driver 侧再叠 `hasOwnProperty`；段名以 JSON 字面量嵌入
  （沿用现有 `method_lit` 防注入写法）。
- **dev 目录镜像兜底**：`replaced` 语义保持不变（见 §三.2 最后一条），避免回归。
- **冲突检测**：仍按 (pattern, method) 判定，嵌套不改变 Conflict 语义。

## 五、风险与未决

- **跨仓（不可自愈，只登记）**：oj-module `packages/cli/src/contract/check.ts:107` 的 `VERB_OF`
  只认动词键，AST 扫描会把 `upload: { post }` 判成「端点未实现」。该仓按规矩只读，需用户授权或
  另开一轮同步。**不擅改。**
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

## 七、落地步骤

- [ ] 内省递归化（`src/bridge/mod.rs:944`）+ `run_module` handler 链（`src/bridge/mod.rs:920`）
- [ ] `server/src/routes.rs`：Entry/Lookup/RouteDecl/递归建表/来自 routes.js 的 handler 校验
- [ ] `server/src/actor.rs`、`server/src/lib.rs` 透传 handler
- [ ] `oj/src/build_cmd.rs`、`oj/src/app.rs`：routes.js 写入与读取 handler
- [ ] 回归测试（routes 单测 + 内省单测 + build 产物断言）
- [ ] devkit 四件 + CHANGELOG + `cargo xtask build`
- [ ] 门禁复跑 + 双专家评审（开发侧 / 架构侧）+ 处置记录写入本文档

## 八、评审意见与处置

（待评审后逐条填入：意见 / 是否采纳 / 理由 / blast radius）

## 附录 A：在途改动（已回退，开工时第一步重做）

方案定稿前曾在 `src/bridge/mod.rs` 做了半步：内省由「只遍历 7 个动词键」改为「遍历全部函数导出键」。
它单独落地会**引入回归**（`oj build` 无动词过滤，会把 `upload` 写成 `method: "upload"`，
release 启动在 `server/src/routes.rs:231` 报 `routes.js: unknown method` 而 fatal），故已 `git checkout` 回退。
开工时按 §七第 1 步连同下游过滤一起做：

```diff
-/// 启动期内省：import api 模块、读 default[method].route ……（{"get": "{id}" | null, ...}）
+/// 启动期内省：import api 模块、读 default[键].route ……（{"get": ..., "upload": { ... }}）
-             for (const k of ["get","post","put","del","patch","head","options"]) {\
-               const fn = m.default && m.default[k];\
+             const d = m.default || {};\
+             for (const k of Object.keys(d)) {\
+               const fn = d[k];\
```

## 附录 B：下游（plane）侧的两种等价落法（不在本仓改）

- **用新特性**：`asset-v2/api.ts` 内 `export default { post, upload: { post } }` + 组 `.route`。
- **不改平台也能跑**：新建 `publicapi/space/asset-v2/upload/api.ts`，`export default { post }`，
  `.route = "/public/assets/v2/anchor/{anchor}/upload"`（URL 不变）。
