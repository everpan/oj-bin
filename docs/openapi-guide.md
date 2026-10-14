# oj OpenAPI 入门指南

面向第一次接触 oj 的开发者：从「为什么需要它」到「怎么用」「它内部怎么跑」
「会遇到哪些坑」，用一个完整例子串起来。读完即可独立生成并维护项目的 OpenAPI 文档。

> 关联深度文档：`.schema` 的写法与三通道契约见 `docs/devkit/api-manual.md`「入参契约
> `.schema`」（v0.1.44）与 `docs/devkit/scenarios.md` 场景 24；本文只讲 OpenAPI，不重复
> 契约语法。

---

## 1. 这是什么、为什么需要它

`oj openapi` 是 oj 自带的**文档生成命令**：它扫描你项目的路由表，输出一份
**OpenAPI 3.1** 的 `openapi.json`。这份文件可以被 Swagger UI、Redoc、Postman、各类
前端代码生成器直接消费——也就是「不用手写接口文档，路由即文档」。

关键认知：**你写的 `.schema`（handler 入参契约）是唯一真源**。同一份声明同时驱动两件事：

- **运行期校验**：请求进来先在 JS 之前按 `.schema` 校验，不合法直接 `400`（handler 不执行）；
- **文档生成**：`oj openapi` 读同一份 `.schema`，自动补出 `parameters` / `requestBody`。

所以「把接口写对」和「把文档写对」是同一件事——没有两份要同步的真源。

---

## 2. 最小可生成条件

要让 `oj openapi` 产出有意义的内容，每个接口至少需要：

1. **路由**：handler 上挂 `.route`（或在嵌套组上挂组级 `.route`）。没有路由就没有接口。
2. **（可选但强烈建议）`.schema`**：在 handler 函数对象上挂 `.schema`，分 `params` /
   `query` / `body` 三通道声明入参。不挂也能生成（路径参数退化为 `string`），但文档里
   就没有类型、没有 `requestBody`、也没有任何约束信息。

```ts
// src/wiki/collections/api.ts
function get() { /* ... */ }
get.route = "/collections/{id}";   // GET /v1/api/wiki/collections/{id}
get.schema = {
  params: { type: "object", required: ["id"], properties: { id: { type: "integer", minimum: 1 } } },
};
export default { get };
```

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
| `responses` | **当前为占位** `{ "200": { "description": "OK" } }`——响应体 schema 尚未生成（属后续步，详见第 6 节「已知边界」） |

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
> 你的 config 或 `oj --version` 填上。需要自定义标题时，生成后手动改或交给生成后脚本处理。

---

## 5. 完整示例：从源码到 openapi.json

### 源码（src/wiki/collections/api.ts）

```ts
function get() {
  const { id } = http.params;
  json.ok({ id });
}
function list() {
  const { type, page } = http.query;
  json.ok({ type, page });
}
function create() {
  const { title } = http.body;
  json.ok({ created: title });
}

get.route    = "/collections/{id}";   // GET  /v1/api/wiki/collections/{id}
list.route   = "/collections";          // GET  /v1/api/wiki/collections
create.route = "/collections";          // POST /v1/api/wiki/collections

get.schema = {
  params: { type: "object", required: ["id"], properties: { id: { type: "integer", minimum: 1 } } },
};
list.schema = {
  query: {
    type: "object",
    required: ["type"],
    properties: { type: { type: "integer", enum: [1, 2] }, page: { type: "integer", minimum: 1 } },
  },
};
create.schema = {
  body: {
    type: "object",
    required: ["title"],
    additionalProperties: false,
    properties: { title: { type: "string", maxLength: 80 } },
  },
};

export default { get, list, create };
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
    },
    "/v1/api/wiki/collections": {
      "get": {
        "operationId": "wiki_collections_list_get",
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
        "operationId": "wiki_collections_create_post",
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
    }
  }
}
```

注意看映射关系：

- `.schema.params.id`（integer, minimum 1）→ `in: path` 参数，类型**来自契约**；
- `.schema.query`（type 必填 / page 选填）→ `in: query` 参数，`required` 只标声明的；
- `.schema.body` → `requestBody`，且 `additionalProperties:false` 等契约细节原样进入
  `schema`（前端代码生成器能据此校验入参）。

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

### 6.4 `responses` 目前是占位

当前实现**只生成 `responses: { "200": { "description": "OK" } }`**——响应体 schema 还没生成
（属于 PR-6 后续步）。所以这份 OpenAPI 的「请求侧」完整、「响应侧」还是骨架。别惊讶为什么
没有 `200` 的响应结构；也不要手写 response schema 塞进生成物（下一次 `--check` 会把它当漂移
打回）。需要响应文档时，等后续版本或暂时在生成后做后处理。

### 6.5 `info` 是占位值

`title` / `version` 固定为 `oj API` / `0.1.0`，不读你的项目。发布给外部前按需改。

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

> 内存：比较前会做**对象键升序规范化**，所以「键顺序不同」不算漂移，只有内容不同才算——
> 你可以放心用格式化工具美化 `openapi.json`，只要语义不变 `--check` 就过。

---

## 8. 一句话总结

> 写对 `.schema`，路由即文档：`oj openapi -d src -o openapi.json` 生成、`--check` 守住、
> 提交进仓库即可。`params`/`query`/`body` 三通道直接变成 OpenAPI 的 `parameters` /
> `requestBody`；路径参数必须与路由 pattern 双向对齐，否则生成期就报错。

进阶：嵌套 handler 组 + 组级 `.route` / 组级 `.schema` 的合并语义、判别红线，见
`docs/devkit/api-manual.md`「嵌套 handler 组」与「入参契约 `.schema`」；`.schema` 写法与
校验示例见 `docs/devkit/scenarios.md` 场景 24。
