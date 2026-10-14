# oj OpenAPI 入门指南

面向第一次接触 oj 的开发者：从「为什么需要它」到「怎么用」「它内部怎么跑」
「会遇到哪些坑」，用一个完整例子串起来。读完即可独立生成并维护项目的 OpenAPI 文档。

> 关联深度文档：`.schema` 的写法与四通道契约见 `docs/devkit/api-manual.md`「入参与出参
> 契约 `.schema`」（v0.1.44，响应通道 v0.1.70）与 `docs/devkit/scenarios.md` 场景 24；
> 本文只讲 OpenAPI，不重复契约语法。

---

## 1. 这是什么、为什么需要它

`oj openapi` 是 oj 自带的**文档生成命令**：它扫描你项目的路由表，输出一份
**OpenAPI 3.1** 的 `openapi.json`。这份文件可以被 Swagger UI、Redoc、Postman、各类
前端代码生成器直接消费——也就是「不用手写接口文档，路由即文档」。

关键认知：**你写的 `.schema`（handler 契约）是唯一真源**。同一份声明同时驱动两件事：

- **运行期校验**：请求进来先在 JS 之前按 `.schema` 的**入参**通道校验，不合法直接 `400`
  （handler 不执行）；
- **文档生成**：`oj openapi` 读同一份 `.schema`，自动补出 `parameters` / `requestBody`
  / `responses`（响应通道 v0.1.70 起）。

所以「把接口写对」和「把文档写对」是同一件事——没有两份要同步的真源。

> 唯一的不对称：`response`（出参）**只声明、不校验**——运行期不拦返回值（响应由 JS 产生，
> 校验失败时请求已执行完、无处可去）。产物里带 `x-oj-response-enforced: false` 就是声明
> 这一点。它是文档与 codegen 的真源，**不是**守卫，也**不能**当脱敏边界。

---

## 2. 最小可生成条件与路由推算（`.route` 基本可省略）

要让 `oj openapi` 产出有意义的内容，每个接口至少需要：

1. **路由**：**绝大多数情况不用写 `.route`**——路由默认由「文件所在目录 + 导出名」推算
   （见下方 2.1）。只有需要偏离目录镜像（加路径参数、挂到 base 根、用嵌套组等）时才写
   `.route`。没有路由就没有接口。
2. **（可选但强烈建议）`.schema`**：在 handler 函数对象上挂 `.schema`，分 `params` /
   `query` / `body` 三通道声明**入参**（外加 `response` 通道声明**出参**，v0.1.70）。
   不挂也能生成（路径参数退化为 `string`），但文档里就没有类型、没有 `requestBody`、
   没有 `responses` 结构，也没有任何约束信息。

### 2.1 路由默认怎么推算（不写 `.route` 时）

oj 的路由表构建（`serve::routes::RouteTable::build`，`serve/src/routes.rs`）对**每个 `api.ts`**
按下列规则落路由，所以你几乎不需要手写 `.route`：

- **目录镜像为主路由**：文件 `src/<a>/<b>/api.ts` 默认得到
  `GET/POST/... /{base}/<a>/<b>`（`base` 通常是 `/v1/api`）。
- **目录段 `_name_` 自动变成路径参数**：目录名写成 `_user_` 这种「首尾下划线」形态，会被
  转换为 `{user}`。例如 `src/user/_id_/api.ts` → `/{base}/user/{id}`。
  判定很严格——整段 `_x_`、内层不以 `_` 起止、不含 `{}` 才转换；`_shared`、`a_bb_c`
  这类保持字面段（见 `fs_seg_to_pattern`）。
- **导出名 = HTTP 方法**：`export default { get, post, put, del, patch, head, options }`
  里**每一个键名都必须是 HTTP 方法之一**（链尾取方法，非方法名会被直接忽略）。所以「一个文件
  里既要查询又要创建」就各自命名为 `get` / `post`，**不是** `list` / `create`。
- **嵌套组对象 = 额外的路径段**：`export default { cccc: { get, post } }` 会让 `cccc`
  成为路径段 → `/{base}/<dir>/cccc` 下挂 GET/POST；再嵌套 `cccc: { list: { get } }`
  → `/{base}/<dir>/cccc/list` 的 GET。组名本身也可以是 `_name_` 形态变成参数。

### 2.2 `.route` 的三种写法（仅在需要偏离目录镜像时）

当默认镜像不够用时，给某个 verb（或嵌套组）挂 `.route`：

| `.route` 写法 | 最终 pattern | 适用场景 |
|---|---|---|
| 不写 / `""` | `{base}/<目录镜像>` | 默认即可，最常见 |
| 相对，如 `"detail"` | `{base}/<目录镜像>/detail` | 同一目录下追加子路径 |
| 相对，如 `"{id}"` | `{base}/<目录镜像>/{id}` | 同一文件里区分「列表」与「详情」 |
| **以 `/` 开头**，如 `"/home/pie"` | `{base}/home/pie` | **根级**：忽略文件所在目录，直接挂在 base 根下（常用于把分散文件收拢到统一前缀） |

> 提示：写了 `.route`（非空）的 verb，其目录镜像会被「替换」掉——dev 兜底层不会再把
> 目录路径复活成同名路由，避免两个 pattern 指向同一文件（`replaced` 集合，见 `routes.rs`）。

### 2.3 最小示例（零 `.route`）

```ts
// src/wiki/collections/api.ts —— 完全不写 .route，路由 = 目录镜像
// 自动得到：GET /v1/api/wiki/collections
function get() { /* ... */ }
export default { get };
```

想在同一「资源」下既有列表 GET 又有详情 GET，oj 的惯用法是**用 `_id_` 目录而不是手写
`.route`**（详见第 5 节完整示例）：

```
src/wiki/collections/api.ts      # get → GET  /v1/api/wiki/collections
src/wiki/collections/_id_/api.ts # get → GET  /v1/api/wiki/collections/{id}  （_id_ 目录 ⇒ {id}）
```

### 2.4 四通道 `params` / `query` / `body` / `response`：各自是什么、什么时候出现

`.schema` 里前三个通道对应请求入参的三种来源。**声明了哪条才校验哪条**——未声明的通道
不受运行期约束、也不进文档：

| 通道 | 来源（JS 侧读取） | 什么情况声明 | 文档映射 |
|---|---|---|---|
| `params` | 路径参数：路由 pattern 里的 `{name}` 段（来自 `_id_` 目录或 `.route`），JS 用 `http.params` 读 | pattern 里有路径参数就该声明，且名字必须与 pattern **双向一致**（见 §6.1）；没有路径参数就不写这条 | `in: path`（恒必填） |
| `query` | URL 查询串 `?a=1&b=2`，JS 用 `http.query` 读 | 分页 / 过滤 / 开关等「挂在 URL 问号后」的入参，任何动词都可以有 | `in: query` |
| `body` | JSON 请求体，JS 用 `http.body` 读 | 创建 / 更新等「载荷在请求体里」的场景——实践中即 **POST / PUT / PATCH**（`del` 带载荷少见但协议层不拦） | `requestBody` |
| `response` | **出参**（v0.1.70）：`json.ok(data)` 的 **data payload** | 想让文档/前端 client 知道返回结构就声明。**运行期不校验**；`json.raw` / `json.stream` / `json.sse` 端点（无信封）不要声明 | `responses.200.content`（外裹 `{code,msg,data}`） |

两个实务提醒：

- **GET/HEAD 别声明 `body`**：HTTP 语义上没有请求体，客户端不会发；而 oj 对声明了
  `body` 通道的端点，会拿「无 body → null」去校验 `type: "object"`——等于**每个请求都
  400**（死契约）。
- **常见组合就三种**：列表 GET = 只有 `query`；详情 GET = 只有 `params`；
  POST/PUT = `params`（若有）+ `body`。绝大多数接口落在这三格子里。

### 2.5 `.schema` 挂哪里：动词级与组级（通道级合并）

`.schema` 可挂两处，运行期守卫与 `oj openapi` **同源消费**（合并发生在内省期，结果落到
`RouteRow.schema`）：

- **动词级**（最常用）：挂在导出的 handler 函数对象上，只约束该端点；
- **组级**：挂在嵌套组对象上，作整组**默认契约**——组内动词没挂 `.schema` 就继承，
  挂了则按**通道合并**（组 `query` ∪ 动词 `body`，是「并集」不是「替换」）。

```ts
const asset = {
  post() { /* ... */ },
  get()  { /* ... */ },
};
// 组级默认契约：整组共享的 query（分页）——不碰路径参数
asset.schema = {
  query: { type: "object", properties: { page: { type: "integer", minimum: 1 } } },
};
// 动词级叠加 body：最终 = 组 query(page) ∪ 动词 body(title)
asset.post.schema = {
  body: { type: "object", required: ["title"], properties: { title: { type: "string" } } },
};
export default { asset };
```

经验法则（见 `docs/devkit/api-manual.md`「嵌套 handler 组」）：**跨端点共享的 query 契约
（分页 / 租户 / 公共过滤）放组级**；**路径参数（`params`）留给各动词自挂**——组级若声明了
某路径参数，组内每个动词的 pattern 都必须含它，否则装配期失败。
组 `.schema` 非法 → 装配期硬失败（不静默跳过）。

### 2.6 `.schema` 支持的 key 速查（白名单）

`.schema` 是 **JSON Schema 的严格子集**：白名单外的关键字（`$ref` / `oneOf` / `format`…）
**注册期直接报错**，绝不静默跳过——「声明了但没生效」在本项目里是最重的缺陷类。

| key | 作用 | 何时用 |
|---|---|---|
| `type` | 类型；单串或数组（如 `["string","null"]`）。允许 `string/number/integer/boolean/object/array/null` | 必写，每个属性都声明 |
| `required` | 字符串数组：哪些属性必填 | 缺了就 400 的字段 |
| `properties` | 子属性契约（递归校验） | object 的字段约束 |
| `items` | 数组元素契约（递归校验） | `type:"array"` 必配 |
| `additionalProperties` | `false` = 禁止未声明字段 | body 收口，防脏字段混入 |
| `minimum` / `maximum` | 数值边界 | `page`≥1、金额上限 |
| `minLength` / `maxLength` | 字符串长度边界 | 标题、编码长度闸 |
| `pattern` | 正则约束（**Rust `regex`，≠ ECMA-262**：不支持 lookahead/反向引用；非法注册期报错） | 手机号、ID 格式 |
| `enum` | 非空数组：合法值枚举 | 状态位；数值枚举**按值比较**（v0.1.69 修复过 `1.0` 误杀） |
| `nullable` | `true` 时 `null` 放行 | 选填字段显式传 null |
| `minItems` / `maxItems` | 数组元素个数边界 | 批量接口限 1~100 条之类 |
| `default` | 白名单允许；**仅随契约透传进 openapi.json，运行期不会为缺参补默认值** | 纯文档用途 |

完整的「`.schema` 关键字 → zod 写法」对照表与**不支持清单**（联合类型 / `format` /
`$ref` / `oneOf` …一律 fail-fast，附改写方案）见 `docs/devkit/api-manual.md`
「表达力边界：与 zod 的对照表」（v0.1.71）。下游拿本产物生成 zod 前应先读它。

### 2.7 两个行为差异要记住

- **`params`/`query` 会强转**：这两通道来源是字符串 map，值按声明类型 coerce
  （`"1"` → 整数 1、`"true"` → boolean）；**body 是 JSON，不强转**——body 里的数值必须
  真的是数值，别传字符串。
- **`params`/`query` 只允许扁平标量**：在这两通道里声明 `object`/`array` 是**永远无法满足的
  死契约**，注册期拒绝（底层来源是 `HashMap<String,String>`）。

### 2.8 案例：一份完整 `.schema` 及其双重效果

```ts
// src/wiki/collections/api.ts
function post() {
  const { title, tags, status, remark } = http.body;
  json.ok({ created: title });
}
post.schema = {
  body: {
    type: "object",
    required: ["title"],
    additionalProperties: false,     // 多传字段 → 400
    properties: {
      title:  { type: "string", minLength: 1, maxLength: 80 },
      tags:   { type: "array", minItems: 1, maxItems: 20,
                items: { type: "string", pattern: "^[a-z][a-z0-9-]*$" } },
      status: { type: "integer", enum: [0, 1, 2], default: 0 },  // default 只进文档
      remark: { type: "string", nullable: true },                // 显式 null 放行
    },
  },
};
export default { post };
```

同一份声明在两处生效：

1. **运行期**（先于 JS 执行；`server.schema_validation` 默认开，可关但声明仍进文档）：
   - `POST /v1/api/wiki/collections` 传 `{"title":"hi","extra":1}` → 400
     `body: additional property \`extra\` is not allowed`；
   - 传 `{"title":""}` → 400 `body.title: shorter than minLength 1`；
   - 漏掉 `title` → 400 `body: missing required field \`title\``；
   - 合法请求才进 handler——JS 里不用再写一遍「if (!title) …」。
2. **文档期**：`oj openapi` 把这份契约原样变成 `requestBody.content.application/json.schema`
   （含 enum / default / nullable / minLength…），Swagger 与前端代码生成器据此校验入参。

---

## 3. 三条命令快速上手

```bash
# ① 生成并打印到终端（先确认长什么样）
oj openapi -d src

# ② 生成并落盘到项目根 openapi.json（默认输出路径就是 <dir>/openapi.json）
oj openapi -d src -o openapi.json

# ③ CI 漂移门禁：把生成物与已提交的 openapi.json 比对，不一致就非零退出
oj openapi -d src --check
```

常用参数（`oj/src/args.rs` 的 `OpenApiArgs`）：

| 参数 | 作用 |
|---|---|
| `-d/--dir <目录>` | 服务目录（api 根）：`src`（dev 内省）或 `dist`（release 直读）。缺省从 config 逐级向上搜 `src` 优先 |
| `-c/--config <yaml>` | 配置文件（路由前缀 / 库来源）。缺省自动搜索 |
| `-b/--base <前缀>` | API 基础前缀覆盖，默认第一条 api 挂载的 prefix（通常 `/v1/api`） |
| `--site <prefix>` | 选某个挂载（v0.1.58 起）；与 `-d`/`-b` 互斥 |
| `-o/--out <文件>` | 输出文件；省略则打印 stdout。`--check` 时作为待比对文件 |
| `--check` | 只校验漂移：与已提交 `openapi.json` 比对，键序规范化后不一致即退出码 1 |

> 模式判定：给 `-d src` 走 **dev**（用 V8 内省 `.route` / `.schema`）；给 `-d dist`
> 走 **release**（读 `dist/manifests.yaml` + 各模块 `routes.js`，需先 `oj build`）。
> 两种模式产出的路由表同构，生成逻辑完全一致。

---

## 4. 生成过程拆解（它内部怎么跑）

把黑盒拆成三步，建立心智模型：

### 步骤 A：发现路由（discover_routes）

- **dev**：自建最小 `Bridge` 内省每个 `api.ts` 的导出——谁挂了 `.route`、谁挂了
  `.schema`、是不是嵌套组。内省失败会 **fail-fast**（直接报错退出），不会静默少路由
  （否则 `--check` 反而会误报「漂移」）。
- **release**：不跑 V8，直接读 `dist/manifests.yaml`（模块→版本锁）与各模块 `routes.js`
  （路由事实唯一来源），构造同样的路由表。

每条路由落进 `RouteTable`，handler 的 `.schema` 被收进 `RouteRow.schema`。**这一步只是把
「你的源码声明」收集成一张表。**

### 步骤 B：逐路由生成 operation（generate）

对路由表每一行，构造一个 OpenAPI operation 对象，字段含义：

| 生成字段 | 来源 |
|---|---|
| `operationId` | 由「去 base 后的子路径 + 方法」派生，非字母数字统一转 `_`（如 `wiki/collections/{id}` + `get` → `wiki_collections__id__get`） |
| `summary` | `GET v1/api/wiki/collections/{id}` 这样的可读串 |
| `x-oj-file` | **溯源标记**：该路由来自哪个源码文件（排障 / 追溯用，非 OpenAPI 标准字段） |
| `parameters` | `in: path` 来自路由 pattern（或 `.schema.params`，带真实类型）；`in: query` 来自 `.schema.query` |
| `requestBody` | 来自 `.schema.body`（`content.application/json.schema` = 你的 body 契约） |
| `responses` | v0.1.70 起来自 `.schema.response`：输出**完整信封** `{code,msg,data}` 并带 `x-oj-response-enforced: false`（声明、不校验）。未声明 `response` 的端点仍是占位 `{ "200": { "description": "OK" } }` |
| `tags` | v0.1.70 起每个 operation 带 `[<模块名>]`，spec 根还有去重排序的全量 `tags` 清单——下游 codegen 按它分模块落盘 |

两个值得记住的归一化规则：

- **catch-all** 路由 pattern 里的 `{*path}` 会收敛成 OpenAPI 合法的 `{path}`；
- 路径参数**若声明了 `.schema.params`**，则其类型来自契约（不再是硬编码 `string`）。

### 步骤 C：输出或校验（emit_or_check）

- 不带 `--check`：序列化为美化 JSON，写文件（`-o`）或打 stdout。
- 带 `--check`：把生成物与已提交的 `openapi.json` 都做**键序规范化**（对象键升序）后再比
  较——所以「键顺序不同」不算漂移，只有「内容不同」才算。不一致就打印重生成命令和差异行，
  退出码 1。

整体输出骨架（节选）：

```json
{
  "openapi": "3.1.0",
  "info": { "title": "oj API", "version": "0.1.0" },
  "paths": { "/v1/api/wiki/collections/{id}": { "get": { ... } } }
}
```

> 注：`info.title` / `info.version` 目前是**固定占位值**（`oj API` / `0.1.0`），不会自动从
> config 或 oj 版本填上（**为何不从 oj 版本填空，见 §6.5**）。发布前按需手动改或交给生成
> 后脚本注入你项目的 API 版本。

---

## 5. 完整示例：从目录结构到 openapi.json（几乎零 `.route`）

oj 的路由几乎都能从目录/导出结构推算。下面用 wiki/collections 这套最典型的「列表 + 创建 +
详情」演示：文件怎么放、`oj openapi` 就怎么出——全程不写 `.route`。

### 目录与源码

```
src/wiki/collections/api.ts        # 列表 + 创建，无需 .route
src/wiki/collections/_id_/api.ts   # 详情：目录名 _id_ 自动变成 {id} 路径参数
```

```ts
// src/wiki/collections/api.ts —— 不写 .route，路由 = 目录镜像
function get() {
  const { type, page } = http.query;
  json.ok({ type, page });
}
function post() {                    // POST 同样走目录镜像
  const { title } = http.body;
  json.ok({ created: title });
}

get.schema = {
  query: {
    type: "object",
    required: ["type"],
    properties: { type: { type: "integer", enum: [1, 2] }, page: { type: "integer", minimum: 1 } },
  },
};
post.schema = {
  body: {
    type: "object",
    required: ["title"],
    additionalProperties: false,
    properties: { title: { type: "string", maxLength: 80 } },
  },
};

export default { get, post };      // 导出名 get/post = HTTP 方法
```

```ts
// src/wiki/collections/_id_/api.ts —— 目录 _id_ ⇒ 路径参数 {id}，仍无需 .route
function get() {
  const { id } = http.params;
  json.ok({ id });
}
get.schema = {
  params: { type: "object", required: ["id"], properties: { id: { type: "integer", minimum: 1 } } },
};
export default { get };
```

### 运行

```bash
oj openapi -d src -o openapi.json
```

### 产物的关键片段（节选，省略 x-oj-file 等字段）

```json
{
  "openapi": "3.1.0",
  "info": { "title": "oj API", "version": "0.1.0" },
  "paths": {
    "/v1/api/wiki/collections": {
      "get": {
        "operationId": "wiki_collections_get",
        "summary": "GET v1/api/wiki/collections",
        "parameters": [
          { "name": "type", "in": "query", "required": true,
            "schema": { "type": "integer", "enum": [1, 2] } },
          { "name": "page", "in": "query", "required": false,
            "schema": { "type": "integer", "minimum": 1 } }
        ],
        "responses": { "200": { "description": "OK" } }
      },
      "post": {
        "operationId": "wiki_collections_post",
        "summary": "POST v1/api/wiki/collections",
        "requestBody": {
          "required": true,
          "content": {
            "application/json": {
              "schema": {
                "type": "object",
                "required": ["title"],
                "additionalProperties": false,
                "properties": { "title": { "type": "string", "maxLength": 80 } }
              }
            }
          }
        },
        "responses": { "200": { "description": "OK" } }
      }
    },
    "/v1/api/wiki/collections/{id}": {
      "get": {
        "operationId": "wiki_collections__id__get",
        "summary": "GET v1/api/wiki/collections/{id}",
        "parameters": [
          { "name": "id", "in": "path", "required": true,
            "schema": { "type": "integer", "minimum": 1 } }
        ],
        "responses": { "200": { "description": "OK" } }
      }
    }
  }
}
```

注意看映射关系：

- **完全没有 `.route`**：`collections/api.ts` 的 `get`/`post` 直接落到目录镜像
  `/v1/api/wiki/collections`；详情靠 `collections/_id_/api.ts` 的 `_id_` 目录自动变成
  `{id}` 路径参数——这正是 oj 推荐的无 `.route` 写法（列表 GET 与详情 GET 用两个文件而非
  在一个文件里挂两个 `get`）。
- 导出名 `get`/`post` 就是 HTTP 方法；`_id_` 目录 / 嵌套组自动展开成路径段。
- `.schema.query`（type 必填 / page 选填）→ `in: query` 参数；`.schema.body` →
  `requestBody`；`.schema.params.id`（integer, minimum 1）→ `in: path` 参数，类型**来自契约**。

---

## 6. 开发注意事项（踩坑清单）

### 6.1 路径参数必须「双向一致」——最常见的生成期报错

`.schema.params` 声明的名字，必须与路由 pattern 的真实路径参数**两边都对上**：

- 契约声明了 pattern **没有**的参数 → 生成期直接报错（否则「契约」和「URL」变成两份真源，
  漏掉的参数等于不受任何约束）；
- pattern 有参数但契约**没声明** → 也报错（该参数会「裸奔」通过，不受约束）。

```ts
// ❌ 错误：pattern 里没有 ghost，契约却声明了
get.route = "/collections/{id}";
get.schema = { params: { type: "object", properties: { id: { type: "integer" }, ghost: { type: "string" } } } };

// ❌ 错误：pattern 有 id，契约没声明
get.route = "/collections/{id}";
get.schema = { params: { type: "object", properties: {} } };

// ✅ 正确：一一对应
get.route = "/collections/{id}";
get.schema = { params: { type: "object", required: ["id"], properties: { id: { type: "integer" } } } };
```

> 实践建议（来自 `api-manual.md`）：**路径参数留给各 verb 自挂 `.schema.params`**；
> 跨端点共享的 **query** 契约（分页 / 租户 / 公共过滤）更适合放**组级 `.schema`**。原因是
> 组级 `.schema` 若声明了某路径参数，组内每个动词的 pattern 都必须含它，否则装配期失败。

### 6.2 不写 `.schema` 时，路径参数退化为 `string`

如果 handler 完全没挂 `.schema`，`oj openapi` 仍会从 pattern 抽出路径参数，但类型固定是
`"string"`，且**不会生成 `requestBody`**。文档有「接口骨架」但没有类型与入参约束——等于
只生成了一半。建议每个对外接口都补 `.schema`。

### 6.3 `required` 的位置

OpenAPI 里 `required` 是 **parameter 级**字段（标识「这个参数是否必填」）。你在 `.schema`
通道里写的 `required: ["id"]` 会被正确翻译成 parameter 的 `required: true`，**不会**残留在
`schema` 子对象里（生成器会自动剥掉）。别在参数级的 `schema` 里再写一遍 `required`。

### 6.4 `responses` 由 `.schema.response` 生成（v0.1.70 起）

```ts
get.schema = {
  response: {
    type: "object",
    required: ["list", "total"],
    properties: {
      list:  { type: "array", items: { type: "object" } },
      total: { type: "integer", minimum: 0 },
    },
  },
};
```

生成物（注意 `data` 外层那三个字段是 oj 的统一信封，不是你写的）：

```json
"responses": {
  "200": {
    "description": "OK",
    "x-oj-response-enforced": false,
    "content": {
      "application/json": {
        "schema": {
          "type": "object",
          "required": ["code", "msg", "data"],
          "properties": {
            "code": { "type": "integer" },
            "msg":  { "type": "string" },
            "data": { "type": "object", "required": ["list", "total"], "properties": { … } }
          }
        }
      }
    }
  }
}
```

三条必须记住：

1. **`response` = `json.ok(data)` 的 data payload**，不是整包信封。oj 的 wire 恒为
   `{code,msg,data}`，所以生成物替你外裹了一层——别在 `response` 里再写一遍 `code`/`msg`。
2. **运行期不校验**（`x-oj-response-enforced: false`）：少返回字段不会 400/500，也不会
   裁剪。别把它当守卫或脱敏边界。
3. **`json.raw` / `json.stream` / `json.sse` 端点不要声明 `response`**——它们绕过信封出裸
   body，声明了就与真实 wire 不符。这类端点不声明即保持占位。

未声明 `response` 的端点仍是 `{ "200": { "description": "OK" } }`。**不要**手写 response
schema 塞进生成物（下一次 `--check` 会把它当漂移打回）。

### 6.5 `info` 是占位值（且**不应**从 oj 版本填空）

`info.title` / `info.version` 当前固定为 `oj API` / `0.1.0`，不读你的项目。

关于 `info.version` 一个值得说清的疑问：**能不能直接填 oj 自己的版本（当前 `0.1.71`，
代码里用 `env!("CARGO_PKG_VERSION")` 可取，见 `oj/src/info_cmd.rs` / `serve_cmd.rs`）？**
结论：**不合适，不建议。**

- OpenAPI 语义里 `info.version` 是「**这份 API 文档（即你的接口）的版本**」，与 OpenAPI
  Specification 版本、与框架实现版本都不同。
- oj 的 `CARGO_PKG_VERSION`（如 `0.1.71`）是**框架自身的版本**，把它填进 `info.version`
  会误导消费方以为「这个 API 已经迭代到 0.1.71」，而实际上那只是 oj 的版本号。
- oj 目前**没有单一的「项目 API 版本」概念**——`dist/manifests.yaml` 里是**每模块**各自的
  版本，不存在一个能代表整个 API 的全局版本。因此无法自动取到「正确的」 `info.version`。

所以当前保持占位是最稳妥的：**发布给外部前，把 `info.version` 手动改成你项目的 API 版本**，
或交给「生成后脚本」注入（如从 `package.json` / `git describe` 读取）。未来若要自动化，正确
的设计是在 config 或 `oj openapi` 加一个显式的 `--api-version` / 配置字段，由你显式给出接口
版本，而不是复用框架版本。

### 6.6 dev 内省失败会直接报错

`-d src` 模式下若某个 `api.ts` 无法内省（语法/导出异常），`oj openapi` **直接退出报错**，
不会偷偷少几条路由。看到这类报错先修源码，不要当成 openapi 命令本身的 bug。

---

## 7. CI 漂移门禁（--check 怎么用）

目标：让 `openapi.json` 永远等于「路由表当前长什么样」，禁止手改或忘了重新生成。

工作流：

1. 本地加/改路由与 `.schema` 后，跑 `oj openapi -d src -o openapi.json` 重新生成；
2. `git add openapi.json && git commit`——**把生成物提交进仓库**；
3. CI 里跑 `oj openapi -d src --check`：把「重新生成的结果」与「仓库里提交的
   `openapi.json`」做键序规范化比对；
4. 一致 → 退出 0；**不一致 → 退出 1 并打印「重新生成命令 + 差异行」**，CI 红。

漂移的常见成因与处理：

| CI 报漂移 | 原因 | 处理 |
|---|---|---|
| 加了路由但没重新生成 | `openapi.json` 落后于源码 | 本地重跑生成并提交 |
| 手改了 `openapi.json` | 与「路由表真值」不符 | 删掉手改，改源码 `.schema` 后重新生成 |
| 只改了 `.schema` 类型 | 生成物变了但没提交 | 重新生成并提交 |
| **升到 v0.1.70 后首次跑就报漂移** | 产物形状变了：新增 `tags`（模块归属）、`responses.200` 落地、`x-oj-file` 改相对路径 | 预期内——重跑生成并提交一次即可 |

> 内存：比较前会做**对象键升序规范化**，所以「键顺序不同」不算漂移，只有内容不同才算——
> 你可以放心用格式化工具美化 `openapi.json`，只要语义不变 `--check` 就过。

---

## 8. 一句话总结

> 写对 `.schema`，路由即文档：`oj openapi -d src -o openapi.json` 生成、`--check` 守住、
> 提交进仓库即可。`params`/`query`/`body` 三通道变成 OpenAPI 的 `parameters` /
> `requestBody`，`response` 通道变成 `responses.200`（外裹信封）；路径参数必须与路由
> pattern 双向对齐，否则生成期就报错。

下游 codegen：产物每个 operation 带 `tags: [<模块名>]`、根级有全量 `tags` 清单，可据此
把生成的 client 分模块落盘；`x-oj-file` 是相对 api 根的路径。

进阶：嵌套 handler 组 + 组级 `.route` / 组级 `.schema` 的合并语义（`response` 与入参
规则不同：动词级整体覆盖）、判别红线，见 `docs/devkit/api-manual.md`「嵌套 handler 组」
与「入参与出参契约 `.schema`」；`.schema` 写法与校验示例见 `docs/devkit/scenarios.md`
场景 24。
