# 路径参数路由设计（Route Params Design）

> 状态：设计稿（待实现；已经两轮多角色评审修正——匹配器内核 / JS 运行时 / 安全契约 / API 设计）
> 关联：`server/src/routes.rs`、`server/src/lib.rs`、`src/bridge/bootstrap.js`、`src/bridge/module_loader.rs`、`src/bridge/mod.rs`、`cli/src/server_cmd.rs`
> 背景：当前目录镜像路由（`base + 目录路径 → <root>/<path>/api.(ts|js)`）**无路径参数**。`lib.rs:87` 写死 `params: HashMap::new()`，`http.param`(`bootstrap.js:43`) 只读 query。

## 一、目标

在保留"JS handler 动态加载、不改 Rust 重编译"的前提下支持路径参数，且**不让参数污染目录结构**（早期 `user/:id/api.ts` 目录方案已否决——目录只应负责代码组织，不应承担路由语义）。

## 二、核心机制：方法级 `.route` 属性 + 启动内省建表

开发者在导出的 **handler 方法本身**上挂一个 `.route` 字符串属性，声明该方法服务的路径模式。路由声明与函数绑死，不可能漂移，且天然支持**同一文件不同方法挂不同路径**。

```ts
// src/user/account/api.ts
function detail() {
  const id = http.param("id", 0);     // 来自路径参数
  json.ok({ id });
}
detail.route = "{id}";                 // 声明 detail 的路径后缀

function list() {
  json.ok({ items: [] });
}
// list 不挂 .route → 沿用目录路径 /v1/api/user/account

export default { get: list, post: detail };
```

启动期通过现有 `LoaderShared` 模块系统在**一次性运行时**中 `import` 每个 `api` 模块，读取 `default[method].route`，编译成一张统一路由表。请求期用 `matchit`（axum 0.8 底层同款，Cargo.lock 已有 0.8.4）匹配 `uri.path()`，提取 params 并定位文件。

> 为何必须"启动建表"：要匹配 `/user/account/42`，目录镜像会把 `42` 当子目录 → 404，永远到不了 `{id}` 路由。故路由表必须在请求前建好。代价是一次性启动开销，收益是启动即打印完整路由表（UC-8）、冲突可启动期报错。

**内省的实现方式（复用，不新增机制）**：内省 = 在用完即弃的 JsRuntime 里跑一个特殊 driver——import 目标模块、枚举 7 个方法 key、读 `.route`、`json.ok({...})` 回传。即 `run_module`(`mod.rs:342`) 的既有管道（driver 生成、KillSwitch 熔断、超时、错误路径）换一个 driver 体。内省运行时与 actor 运行时构造方式相同（`Bridge::with_dbs_and_loader`，bootstrap 全局齐备），用完即弃不进池；`?v=` specifier 同路径 → `cached_transpile` 全局缓存命中，全程仅转译一次。

**内省错误策略（启动不因单个坏模块失败）**：

| 情形 | 处理 |
|---|---|
| import 抛错（语法 / 依赖缺失 / node_modules 未装） | error 日志（文件 + 错误），**跳过该文件**，其路由不注册（请求 404），继续其他模块 |
| 顶层执行抛错 | 同上 |
| 顶层死循环 | **每模块 2s 超时**（复用 KillSwitch；常量即可，真有慢顶层模块再加配置项），超时丢弃该运行时、跳过并日志 |
| 全部模块失败 | 启动失败（大概率环境损坏：node_modules 未装等，fail-fast） |

启动日志打印：总模块数 / 成功数 / 失败清单。

**内省插入时机**：`server_cmd.rs` 中 LoaderShared 构造（:74）之后、actor 池构造（:76）之前——此时 dbs/kv/loader 均就绪；内省产物直接替换现 `route_table` 目录遍历打印（:66-68）。

## 三、`.route` 语法

### 3.1 模式 token（= matchit 0.8 原生语法，零翻译层）

| token | 含义 | 示例 |
|---|---|---|
| `{name}` | 单段参数 | `/user/{id}` |
| `{*name}` | catch-all，**一或多段**，且只能在模式末尾 | `/file/{*path}` |
| `{{` / `}}` | 字面 `{` / `}` 转义 | `/foo/{{id}}` 匹配字面 `{id}` |

> 评审定论：matchit 0.8.4（axum 0.8 同款）的原生语法就是 `{param}`/`{*param}`；`:id`/`*path` 是 0.7 旧语法。**用户侧直接采用原生语法**，不做 `:id`→`{id}` 翻译层——两套语法并存 + 翻译规则是净增概念与 bug 面（partial segment `/foo-{bar}`、`{{` 转义都会被朴素替换弄坏）。
>
> catch-all 语义（matchit tree.rs 源码级核实）：到达 catch-all 节点前必须先吃掉分隔符 `/`，故 `{*path}` **匹配一或多段**——`/v1/api/file` 与归一后的 `/v1/api/file/` 均 404（见第六节 normalize）。另：未归一时 `{*p}` 会把尾斜杠吞进参数值（`bar/`），尾斜杠归一后此问题不存在。

### 3.2 相对 vs 根级（首字符判别）

| 写法 | 判别 | 含义 |
|---|---|---|
| `"{id}"`（不以 `/` 开头） | **相对** | 拼到"目录推导出的 base 路径"之后 |
| `"/user/{id}"`（以 `/` 开头） | **根级** | 忽略文件目录位置，但仍拼在 base 前缀下 |

最终注册进匹配器的都是**含 base 前缀的完整模式**：

- 文件 `src/user/account/api.ts`，base = `/v1/api`，目录推导 base = `/v1/api/user/account`
  - `detail.route = "{id}"`（相对）→ `GET /v1/api/user/account/{id}`
  - `detail.route = "{id}/items"`（相对）→ `GET /v1/api/user/account/{id}/items`
  - `detail.route = "/user/{id}"`（根级）→ `GET /v1/api/user/{id}`（忽略目录，不忽略 base）

无 `.route` 的方法 → 仅用目录推导路径（向后兼容现有零配置路由）。**`.route = ""` 视同未挂**（空串不是有效模式；落相对分支会拼出带尾斜杠的非法 pattern）。

> ⚠️ **挂 `.route` = 替换，不是追加（已裁决维持）**：方法一旦挂 `.route`，其目录镜像 URL（`dir_base`）即不再注册。曾评估"默认同时保留 dir_base"（追加模式）——否决：一个方法静默挂两个 URL、旧路径无法退役、还需发明 `route = false` 魔法值。要同时保留两个路径，写两个方法（一个不挂 = 列表，一个挂 = 详情）——REST list/detail 双函数本就是更清晰的写法。
>
> TS 提示：`detail.route = "{id}"` 在 TypeScript 下报"属性不存在"（swc 只剥类型不影响运行，但编辑器红线）。项目根加一次性 `global.d.ts`：
> ```ts
> declare global { interface Function { route?: string } }
> export {}
> ```

## 四、启动建表流程

```
build_route_table(root, base, loader_shared):
  b = base.trim_end_matches('/')               // base 归一成 "/v1/api"（去尾斜杠，避免双斜杠）
  for each <root>/.../api.(ts|js) as file:
    mod = introspect(file)                     // §2：一次性运行时 + 特殊 driver，2s 超时，失败跳过
    if mod.failed: log error; continue
    dir_base = b + "/" + file.dir_rel(root)    // 目录推导路径
    for m in [get, post, put, del, patch, head, options]:
      fn = mod.default[m]
      if typeof fn !== 'function': continue    // 跳过非函数导出
      r = fn.route
      if r == undefined or r == "":  pattern = dir_base
      else if r starts_with '/':     pattern = b + r
      else:                          pattern = dir_base + "/" + r
      register(pattern, m, file)               // 见第五节
```

要点：

- **拼接规范化**：`Routes::new` 把 base 归一成 `/v1/api/`（尾斜杠，`routes.rs:17`），字面 `base + route` 会得双斜杠（matchit 精确匹配，永不命中）。拼接前 `base.trim_end_matches('/')`，`route` 侧保留首 `/` 即可。
- **同一函数挂多个方法**（`export default { get: f, post: f }`）：`.route` 在函数上，两个方法共享同一模式——若非本意，拆成两个函数。
- `route_table`(`routes.rs:54`) 由"只列目录"升级为"列真实路由"（含方法与参数模式），`cli/src/server_cmd.rs:66-68` 的打印改用其产物。

### 4.1 release 直载（`routes.js`，免内省）

dev（ts）走 §4 的启动内省；**release（js）不内省**，改为一次性 import 模块根目录下的 `routes.js`：

```js
// dist/routes.js — oj build 生成（§4.2），手工产物启动直接拒绝
export default [
  { method: "get", pattern: "/v1/api/user/account/{id}", file: "user/account/api.js" },
  // ... 全量路由行：含 .route 声明行 + 目录镜像行（release 无 fs 兜底，表是唯一路由来源）
];
```

- 读取走 `Bridge::read_module_default`（与内省同构的 side-module driver，复用 2s 超时与信封解析），`RouteTable::from_entries` 注册——注册语义与 dev 完全一致（合并 / 冲突 / 非法 pattern 丢弃）。
- **fail-fast**：`dist/routes.js` 不存在 → 启动失败，提示 `run 'oj build' first`；存在但 default 导出不是数组 → 同样启动失败。
- dev 与 release 差异总结：dev = 内省 + fs 兜底（新文件免重启）；release = routes.js 直载，表外一律 404。
- `file` 字段相对模块根（dist）；`replaced` 集合在 release 恒空（无兜底可拦截）。

### 4.2 `oj build` 生成 `routes.js`（并剥离 `.route`）

见第十二节。

## 五、冲突处理（已确认决策）

**单 matcher + 方法映射值**（比"逐方法一个 matcher + 独立哨兵集合"更省概念）：

- 一个 `matchit::Router`，pattern 的 value 是 `HashMap<method, Entry>`，`Entry = File(path) | Conflict(a, b)`。
- 405 判定天然 O(1)：lookup 命中但方法缺席 → 405，无需遍历其它 matcher。
- "冲突哨兵"不需要独立数据结构——冲突直接写进方法映射的 value。

**冲突/注册失败分类**（matchit `InsertError` 实际有 4 类，处理各不同）：

| 情形 | matchit 行为 | 处理 |
|---|---|---|
| 同 `(pattern, method)` 二次声明 | 首个 insert 已成功；value 里该方法已有 `File` | **error 日志**（文件×2 / 方法 / pattern），value 中改写为 `Conflict`；**请求命中返回 500 + 冲突说明**，服务照常启动 |
| 同 pattern 不同 method、不同文件 | insert 报 `Conflict`（pattern 已在树中） | 取出既有 value，合并新 method → `File`（**允许**：多文件分动词共享一个 pattern；info 日志提示） |
| 结构性冲突（同位置异名参数：`/user/{id}` 已注册，再注册 `/user/{name}/post`） | insert 报 `Conflict{with}`，后来者**不进树** | error 日志，丢弃后来者；请求只会命中已有路由 |
| 非法模式（`{` 不闭合 / `{*p}` 不在末尾 / `/{b}-foo` 参数后静态段） | `InvalidParam` / `InvalidCatchAll` / `InvalidParamSegment` | error 日志，丢弃该路由 |

```rust
// 伪代码
match matcher.at(pattern) {                       // 先查是否已有同 pattern
  Ok(v) if v.contains(method) =>
      v[method] = Conflict(v[method].file, file); log::error!("route conflict: …"),
  Ok(v) => v.insert(method, File(file)),          // 跨文件分动词共享 pattern
  Err(NotFound) => match matcher.insert(pattern, {method: File(file)}) {
    Ok(()) => {},
    Err(e) => log::error!("invalid/conflicting route {method} {pattern} from {file}: {e}"),  // 丢弃
  },
}
```

> 同位置**同名**参数不冲突（`/cmd/{tool}/{sub}` 与 `/cmd/{tool}/misc` 共存，matchit insert.rs 测试为证）；**异名**才冲突。

## 六、请求期流程

`handle`(`lib.rs:72`)改为查表匹配，而非目录解析：

```
handle(method, uri, ...):
  norm = normalize(uri.path()) ?? return 404      // ★ 守卫 + 归一，规格见下
  match matcher.at(norm):
    Ok(value):
      match value[method_name(method)]:           // 未映射动词（TRACE 等）天然落 None
        File(file, params)  -> 解码 params → RequestInfo → actor.run_module(file, m, req, timeout)
        Conflict            -> 500 "route conflict: …"
        None                -> 405 "method {method} not allowed"
    Err(NotFound):
      dev 模式 → 文件系统兜底（见下），兜底也未命中 → 404
      release  → 404
```

### 6.1 normalize 规格（契约回归，全部有现行测试锁定）

按序执行，任一拒绝 → 404（与现 `resolve`(`routes.rs:22-36`) 行为平价）：

1. **非法字符**：含 `\` 或 `\0` → 404。
2. **尾斜杠归一**：`/a/` → `/a`（根 `/` 保持）——现 `trim_matches('/')` 等价契约（`routes.rs:99`），matchit 不自动归一。
3. **段校验**（归一后 split，任一违例 → 404）：空段（`//`，`routes.rs:111` 测试）、字面 `.` / `..`、含 `\` / `\0`。
4. **解码后参数校验**：匹配成功后对参数做 percent-decode 并校验——解码值为 `.`/`..`、含 `\`/`\0`，或 **raw 无 `/` 而解码后有 `/`**（单段参数走私 `%2F`）→ 404（封死 `%2e%2e`、`%2f` 编码形式；现实现对编码形式也是 404——字面 join 永远找不到 `%2e%2e` 目录——须保平价）。catch-all 的 raw 值天然含真实分隔符，放行。

> 安全注：查表方案下 file 来自启动期 walk，**与用户输入彻底解耦**，目录穿越面结构性消除；params 不再进文件系统。第 4 条是契约平价，不是防线。

### 6.2 dev 模式文件系统兜底（保住"新增 api 免重启"）

现 `resolve` 每请求查文件系统，**新增 api 文件无需重启即生效**；纯启动建表会退化此体验。故 dev（`--dev`）下：

- 表 miss → 回退现 `resolve` 目录镜像逻辑 → 命中则照常执行（方法未导出仍由 driver `json.fail(405)` 兜底）。
- **替换路由守卫**：回退命中文件后，若该 `(file, method)` 在表中已有 `.route` 注册 → 404——挂 `.route` 即替换 dir_base 的语义不得被兜底复活，否则 dev/prod 行为分叉。
- 局限（接受）：新增**带参数**路由（`/user/account/42` 形状）表 miss 后目录镜像也解析不出 → 重启生效；修改 `.route` 同理。

release 纯走表（省每请求 stat）。

### 6.3 其它契约平价

- **表与模块不一致的兜底**：请求期 driver 仍校验 `typeof fn === "function"`（`mod.rs:366-368`），方法被删 → 405。表过期不产生 500。
- **HEAD/OPTIONS**：维持现状——需显式导出 `head`/`options`，否则 405（不引入 HEAD→GET 自动回退）。
- **404 文案**：`no api file for route` 语义过时，改为 `no route matched`（`user-manual.md` §10 同步）。

## 七、JS 侧参数访问（`bootstrap.js`）

`http.param` 合并：**路径参数优先，query 兜底**（向后兼容仅用 query 的写法；同名时 query 静默让位，用户手册显式标注）。

```js
if (p === "param") {
  return (name, def) => {
    const info = httpInfo();
    const v = info.params[name] !== undefined ? info.params[name] : info.query[name];
    return v === undefined ? def : v;
  };
}
```

`http.params`（路径参数 map）、`http.query`（query map）已通过 `op_http_info` 返回（`http.rs:27`，现无消费方、链路已备），可整体访问。

**解码（已确认决策）**：路径参数与 query 均在 **Rust 边界**解码后填入 `RequestInfo`，JS 侧拿到的都是**解码后字符串**，无需 `decodeURIComponent`。路径参数用 `percent_decode_str`（`+` 保持字面，路径语义）；query 用 form-urlencoded 解码（`+` → 空格）。`percent-encoding` / `form_urlencoded` / `matchit` 均已随 axum 进 Cargo.lock，声明为直接依赖即可，不增新依赖。

> ⚠️ 有意行为变更（需迁移说明）：`parse_query` 现不解码（`lib.rs:126` ponytail 注）。换 form_urlencoded 后 `?q=a+b` 从 `"a+b"` 变 `"a b"`、`%XX` 被解码。不提供 `http.rawQuery` 之类的逃生口——出现真实需求再加。
>
> 安全告诫（写进用户手册）：路径参数已解码，值可能含 `/`、`..` 等字面。只用于参数化查询（`db.query` 已防注入）与类型转换；**不要**拼接文件路径或内部 URL。

## 八、改动清单

| 文件 | 位置 | 改动 |
|---|---|---|
| `server/Cargo.toml` | — | 加 `matchit = "0.8"`、`percent-encoding`、`form_urlencoded`（均已在 lockfile，仅声明） |
| `server/src/routes.rs` | 8-36 | `Routes` 持有 matchit matcher（pattern → method map）、保留旧 `resolve` 作 dev 兜底；`resolve` 更名查表语义 |
| `server/src/routes.rs` | 54-75 | `route_table`/`walk` → `build_route_table`：一次性运行时 import 读 `.route` 建表（§2 错误策略/超时） |
| `server/src/routes.rs` | 77-133 | 现有目录镜像测试改写：模式注册、参数提取、冲突检测、normalize 守卫（含 `//`、`..`、编码穿越） |
| `server/src/lib.rs` | 79-94 | `handle` 走 §6 流程：normalize → 查表 → 405/500/404 分支；params 解码后填 `RequestInfo.params`；404 文案更新 |
| `server/src/lib.rs` | 126-138 | `parse_query` 改 form-urlencoded 解码，移除原"未 decode" ponytail 注（行为变更见 §7） |
| `src/bridge/mod.rs` | 340-400 | 内省 driver 变体（读 `.route` 回传）；`run_module` 管道复用 |
| `src/bridge/bootstrap.js` | 43-48 | `http.param` 合并 path→query |
| `cli/src/server_cmd.rs` | 62-76 | 内省调用插入（LoaderShared 后、actor 池前）；:66-68 打印改用新表产物 |
| `docs/user-manual.md` | §9 表 / §10 表 / §11 | `http.param` 优先级 + 新增 `http.params` 行；§10 加 500 冲突行、404/405 文案更新、query 解码变更说明；§11 补 `.route` 示例与 `global.d.ts` |
| `sample/` | — | 新增 `.route` 示例（含 `global.d.ts`，见第九节） |

## 九、Demo

### Demo 1：无参路由（向后兼容，零配置）

`src/user/account/api.ts`：
```ts
export default {
  get() { json.ok({ msg: "list" }); },
};
```
→ `GET /v1/api/user/account`（无 `.route`，目录镜像照常）。

### Demo 2：单参数

`src/user/account/api.ts`：
```ts
function detail() {
  const id = http.param("id", 0);     // 来自路径
  json.ok({ id: Number(id) });
}
detail.route = "{id}";
export default { get: detail };
```
→ `GET /v1/api/user/account/{id}`；请求 `/v1/api/user/account/42` 返回 `{"id":42}`。

### Demo 3：多参数

`src/user/post/api.ts`：
```ts
function get() {
  const uid = http.param("uid");
  const pid = http.param("pid");
  json.ok({ uid, pid });
}
get.route = "{uid}/post/{pid}";
export default { get };
```
→ `GET /v1/api/user/post/{uid}/post/{pid}`（即 `/v1/api/user/post/42/post/7`）→ `{"uid":"42","pid":"7"}`。

### Demo 4：同文件不同方法挂不同路径

`src/cart/api.ts`：
```ts
function detail() {
  const id = http.param("id", 0);
  json.ok({ cart: id });
}
detail.route = "{id}";                      // GET /v1/api/cart/{id}

function addItem() {
  const id = http.param("id", 0);
  json.ok({ added: id });
}
addItem.route = "{id}/items";               // POST /v1/api/cart/{id}/items
export default { get: detail, post: addItem };
```
同一文件的两个方法服务不同 URL 模式——模块级 `route` 常量做不到。

### Demo 5：根级路径，忽略目录位置

文件放在 `src/legacy/compat/api.ts`（目录推导为 `/v1/api/legacy/compat`），但想挂在 `/v1/api/v2/user/{id}`：
```ts
function get() {
  json.ok({ id: http.param("id") });
}
get.route = "/v2/user/{id}";                // 以 / 开头 → 根级（base 根下，仍含 base 前缀）
export default { get };
```
→ `GET /v1/api/v2/user/42`（与文件物理位置无关）。

### Demo 6：catch-all（一或多段）

`src/file/api.ts`：
```ts
function get() {
  const p = http.param("path", "");     // "a/b/c"
  json.ok({ path: p.split("/") });
}
get.route = "{*path}";                     // 一或多段
export default { get };
```
→ `GET /v1/api/file/a/b/c` → `{"path":["a","b","c"]}`；
→ `GET /v1/api/file` 与 `/v1/api/file/`（归一后同前）→ **404**（catch-all 不吞零段，matchit 语义）。

### Demo 7：路径参数与 query 同名时的优先级

`GET /v1/api/user/account/42?id=99`：
```ts
function get() {
  json.ok({ id: http.param("id", 0) });   // 取路径 42，而非 query 99
}
get.route = "{id}";
```
返回 `{"id":42}`（路径优先，手册显式标注）。

### Demo 8：冲突（启动报错但不影响服务）

相对路径天然按目录分桶，`src/user/api.ts` 与 `src/account/api.ts` 各挂 `get.route = "{id}"` → `/v1/api/user/{id}` 与 `/v1/api/account/{id}`，**不冲突**。真冲突是根级路径撞车：
`src/a/api.ts`：`get.route = "/user/{id}"` → `/v1/api/user/{id}`
`src/b/api.ts`：`get.route = "/user/{id}"` → `/v1/api/user/{id}`
→ 启动日志：`error: route conflict: GET /v1/api/user/{id} declared in src/a/api.ts and src/b/api.ts`；服务继续启动；请求该路径返回 **500 + 冲突说明**；其余路由正常。

## 十、副作用、成本与约束

1. **`.route` 必须是顶层可求值的确定值**（字面量/常量表达式）。它在内省期求值一次、表即固化，请求期不再校验——若依赖可变状态（kv/时间/env 分支），表与模块实际行为**静默发散**。这是约束，不做运行时检测。
2. **顶层副作用执行次数**：内省运行时 1 次 + 每个 actor 运行时首次触达该模块各 1 次（池大小 N → 最多 N+1 次；模块缓存 per-runtime，`?v=` 不变则不重跑）。顶层 db 写必须幂等（推荐迁移到 `seed.sql`）。内省期 dbs 已就绪（`server_cmd.rs` 先开库执行 seed），顶层只读查询安全。
3. **热重载边界**：handler 内容修改靠 `?v=mtime` 免重启（不变）；新增/删除 api 文件、修改 `.route` 需重启——dev 由 §6.2 兜底缓解（带参数路由除外），release 一律重启。
4. 可选参数（`{id?}`）、正则约束暂不纳入，列入后续迭代。
