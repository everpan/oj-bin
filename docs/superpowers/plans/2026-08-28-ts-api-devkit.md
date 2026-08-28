# TS API DevKit 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 交付随版本发布的 DevKit（完备 TS API 手册 + agent skill + global.d.ts），经 `cargo xtask build` → `bin/devkit/` → `scripts/deploy.sh` 进版本 tarball。

**Architecture:** 源文件集中在 `docs/devkit/`（git tracked）；`global.d.ts` 单一事实源在 `sample/`，xtask 拷贝时拾取。SKILL.md 是 agent 精简入口（工作流+红线+速查），api-manual.md 是唯一事实源（12 章），同目录相对路径引用。

**Tech Stack:** Markdown（无构建）；Rust（xtask 追加拷贝函数）；bash（deploy.sh 追加打包行）。

**Spec:** `docs/superpowers/specs/2026-08-28-ts-api-devkit-design.md`（已批准）

## Global Constraints

- 禁止 debug 构建；构建一律 `--release`（xtask 已内置）。
- 门禁：`cargo fmt --check` 与 `cargo clippy --all-targets -D warnings` 必须过。
- commit 消息尾随一行：`unix@vip.qq.com ai`。
- 文档语言：简体中文；代码/标识符保持原文。
- `bootstrap.js` 必须保持 7-bit ASCII（本计划不改它，仅核对签名时参考）。
- SKILL.md 引用手册一律同目录相对路径 `api-manual.md` + 章节号，绝不内联大段手册内容。
- 12 章标题为固定字符串（Task 2 定义，Task 3 引用，不得改动）：
  `1. 快速开始` `2. 项目结构与模块约定` `3. 编写 api.ts` `4. 导入解析` `5. 全局对象 API 参考` `6. 响应信封与错误码` `7. 鉴权与多租户` `8. 测试` `9. 配置 config.yaml` `10. 构建与发布` `11. 运维要点` `12. 安全红线与已知限制`
- `CLAUDE.md` 在 `.gitignore` 中（未被 git 追踪）——对它的编辑是本地修改，**不提交**。
- 仓库内文档冲突时以 `docs/user-manual.md` 为准；发现 user-manual 自身过时→记录到计划外任务，不顺手改。

---

### Task 1: 补齐 sample/global.d.ts 到 v0.2 API 面

**Files:**
- Modify: `sample/global.d.ts`

**Interfaces:**
- Consumes: `src/bridge/bootstrap.js` 实际挂载（已核对：blob 可调用+命名实例、bus 含 kind、http.file async）。
- Produces: 类型 `AuthUser`、`UploadedFileMeta`、`BlobApi`、`BusApi`、`EsApi`、`OjFetchResponse`；`HttpApi`/`KVApi`/`DBInstance` 扩展。Task 2 手册第 5 章与这些签名保持一致。

- [ ] **Step 1: 修改类型声明**

在 `sample/global.d.ts` 中做以下修改（保持文件其余部分不动，特别是测试 SDK 声明）：

1. 在 `interface HttpApi` 中追加字段与方法，并把 `param` 放宽（真实运行时 `def` 可传任意类型、原样返回）：

```ts
// http.* ：当前请求上下文（只读，懒加载，per-request 最新）。
interface HttpApi {
  method: string;
  params: Record<string, string>;
  query: Record<string, string>;
  headers: Record<string, string>;
  body: any;
  // 取路由参数或 query 参数：路径参数优先，query 兜底，均缺失返回 def 原值。
  param(name: string, def?: unknown): any;
  // 租户 id（tenant 启用时从租户头提取；未启用为 null）。
  tenantId: string | null;
  // 已验签用户（auth 启用且通过 Bearer 守卫；否则 null）。
  user: AuthUser | null;
  // multipart 上传文件元信息（非 multipart 为空数组）。
  files: UploadedFileMeta[];
  // 取第 i 个上传文件的字节（越界报错 no such file）。
  file(i: number): Promise<Uint8Array>;
}

// 已验签用户（JWT claims）。
interface AuthUser {
  id: string | number;
  roles: string[];
  claims: Record<string, Json>;
}

// multipart 上传文件元信息。
interface UploadedFileMeta {
  field: string;
  filename: string;
  content_type: string;
  size: number;
}
```

2. `KVApi` 补齐（`redis` 声明同为 `KVApi`，自动获得同面）：

```ts
// redis.* / kv.* ：KV 存储（redis.default 配置即真 Redis，否则进程内存 KV）。
interface KVApi {
  get(key: string): Promise<string | null>;
  set(key: string, value: string): Promise<boolean>;
  del(key: string): Promise<boolean>;
  // 设过期（秒）。真 Redis 走 EXPIRE；内存 KV 惰性过期。
  expire(key: string, ttlSec: number): Promise<boolean>;
  // 自增返回新值（键不存在从 0 起）。
  incr(key: string): Promise<number>;
}
```

（删掉旧的 `interface KVWithDel extends KVApi` 与 `const kv: KVWithDel;`，改为 `const kv: KVApi;`。）

3. 新增三个全局接口与声明（放在 `ws` 声明附近）：

```ts
// blob.* ：对象存储（可调用取命名实例：blob("media").put(...)；裸调用 blob.put(...) 等价 default）。
interface BlobApi {
  (name?: string): BlobApi;
  put(key: string, bytes: Uint8Array, contentType?: string): Promise<boolean>;
  get(key: string): Promise<Uint8Array>;
  // 幂等：不存在视为成功。
  del(key: string): Promise<boolean>;
  // local = {base}/blob/{key}；s3 = presigned URL（15min）。
  url(key: string): Promise<string>;
  // 缺失 sidecar 且无法按扩展名推断时返回空串。
  contentType(key: string): Promise<string>;
}

// bus.* ：主题广播。publish 广播给订阅 topic 的全部 WS 会话，返回接收方数；
// subscribe 仅 WS 会话内可用（HTTP 路径报错）；kind 报告活跃 broker 类型。
interface BusApi {
  publish(topic: string, data?: unknown): Promise<number>;
  subscribe(topic: string): Promise<void>;
  kind(): string;
}

// es.* ：Elasticsearch 薄客户端（直通 ES 响应体；未配置调用报 es not configured）。
interface EsApi {
  search(index: string, dsl?: unknown): Promise<Json>;
  index(index: string, id: string, doc?: unknown): Promise<Json>;
  del(index: string, id: string): Promise<Json>;
}
```

全局常量区（`declare global` 内）追加/调整：

```ts
  const blob: BlobApi;
  const bus: BusApi;
  const es: EsApi;
```

4. `DBInstance` 补事务方法：

```ts
  // 事务：回调 resolve 提交 / throw 回滚再抛；tx.query/exec/table 同签名走同一连接。
  // 每请求至多一个活跃事务（嵌套报错）；请求结束未完结自动回滚。
  tx(fn: (tx: DBInstance) => unknown): Promise<unknown>;
```

5. `fetch` 返回类型改为结构化声明（替换现有 `Promise<any>` 与 options 内联）：

```ts
// fetch 返回的 Response（浏览器风格子集）。
interface OjFetchResponse {
  ok: boolean;
  status: number;
  statusText: string;
  headers: Record<string, string>;
  json(): Promise<Json | null>;
  text(): Promise<string>;
  arrayBuffer(): Promise<ArrayBuffer>;
  clone(): OjFetchResponse;
}
```

`declare global` 内 fetch 签名替换为：

```ts
  function fetch(url: string, options?: {
    method?: string;
    headers?: Record<string, string>;
    body?: string | Uint8Array | null;
  }): Promise<OjFetchResponse>;
```

6. 全局函数区追加插件自省（bootstrap 实挂 `globalThis.plugins`）：

```ts
  // 已加载插件自省：[{name, semver, abi, fingerprint, ...}] + 宿主 ABI。
  function plugins(): any[];
```

- [ ] **Step 2: 校验声明文件可编译**

Run: `npx --yes -p typescript tsc --noEmit --strict sample/global.d.ts`
Expected: 无输出（0 错误）。若 npx 拉取失败（离线），改用 `cd sample/test && npx vitest run` 兜底并在 commit message 注明。

- [ ] **Step 3: L2 回归**

Run: `cd sample/test && npm ci && npx vitest run`
Expected: 全部既有用例 PASS（类型文件不参与运行时，此步确认无意外破坏）。

- [ ] **Step 4: Commit**

```bash
git add sample/global.d.ts
git commit -m "feat(sample): global.d.ts 补齐 v0.2 API（blob/bus/es/tx/auth/上传/租户/kv 扩展）

unix@vip.qq.com ai"
```

---

### Task 2: 编写 docs/devkit/api-manual.md（12 章完备手册）

**Files:**
- Create: `docs/devkit/api-manual.md`

**Interfaces:**
- Consumes: `docs/user-manual.md`（主要事实源）、`docs/testing.md`（第 8 章）、`docs/ops-manual.md` §1/§3/§4/§5/§6/§7/§8（第 11 章）、`docs/dev-manual.md` §5.1（证书，第 1/9/11 章）、`sample/global.d.ts`（Task 1 产物，第 5 章类型对照）。
- Produces: 12 章固定标题（见 Global Constraints）；第 5 章 API 表 = Task 3 SKILL.md 的引用目标。

**写作规范（全章通用）：**
- 文档头：`# oj TS API 开发手册`；下一行注明 `适用版本：v0.2（与仓库 oj/Cargo.toml 同步）`；一段定位说明（面向业务项目开发者/agent；仓库内部实现见 `dev-manual.md`）。
- 每章标题 `## N. 固定标题`，标题下第一行是引用块 `> 何时读我：…`（一句话场景）。
- API 一律表格 + 最小可运行示例；示例从 `sample/src/` 真实代码摘取改编，不发明 API。
- 手册中出现的每个 API 名必须与 `sample/global.d.ts` 签名一致（Task 1 已对齐 bootstrap.js）。

- [ ] **Step 1: 通读源文档**

按顺序读：`docs/user-manual.md` 全文、`docs/testing.md` 全文、`docs/ops-manual.md` 全文、`docs/dev-manual.md` §5.1、`sample/src/` 下至少 `user/account/api.ts`、`user/item/api.ts`、`order/detail/api.ts`、`order/list/api.ts`、`upload/api.ts`、`news/api.ts`、`news/WS.ts`、`auth_demo/me/api.ts`、`file/api.ts`、`sample/README.md`。

- [ ] **Step 2: 按 12 章骨架成文**

各章必须覆盖的事实清单（来源标注在括号内）：

1. **快速开始**（何时读我：第一次接触 oj，要从零跑通一个请求）
   - 前置：`bin/oj` + `bin/plugins/<平台>/`（发行包）或 `cargo xtask build` 自建。
   - 证书必配：`cargo run -p oj-cert -- gen -o config --days 365` 生成三件；config 指向 `public.pem`/`cert.jws`（dev-manual §5.1）。
   - 最小 config + 目录 + 启动命令（server 自动判定 dev/release）+ `curl` 验证信封返回（user-manual §1）。
2. **项目结构与模块约定**（何时读我：建新项目或新模块前）
   - src 树图（user-manual §4）；首层子目录=模块；`manifest.yaml` 必配、`name` 必须等于目录名（强校验，违反启动失败 §5）；`_shared/` 纯工具目录无路由；`node_modules` 裸 specifier 解析起点；`seed.sql` 可选（启动重放、语句按 `;` 切分、不得含分号字面量）。
   - dist 产物布局（`<module>-<version>/`、`manifests.yaml` 锁、tgz）——细节留给第 10 章，此处只给图。
3. **编写 api.ts**（何时读我：写第一个 handler 时）
   - 动词→方法名映射表：`get/post/put/del/patch/head/options`，**DELETE→`del`**（§7）。
   - 两种写法：同步函数 + `.then().catch()`（event loop 泵到 Promise 落定）；`async` 函数（driver `await fn()`）——各配一例（§6）。
   - `http.body` 解析规则：空→null、JSON→对象/数组、否则 UTF-8 字符串（§6）。
   - `.route` 参数路由：`fn.route = "{id}"`/`"{*path}"` 语法表、挂载后目录镜像被替换（原路径 404）、`""` 视同未挂、以 `/` 开头挂 base 根、matchit 参数段不得混字面（`{id}.json` 非法）、build 剥离 `.route`（release 路由唯一来源 `routes.js`）、`global.d.ts` 提供编辑器支持（§7.1）。
   - `WS.ts`：目录内放 `WS.ts` → `GET {base}/{...path}/ws`，每文本帧执行一次本文件；同目录 `.ts` 优先于 `.js`；release 下 URL 含版本段（v0.2 已知限制）（§9 bus 小节）。
4. **导入解析**（何时读我：import 报错或想抽公共代码时）
   - 相对导入补全链 `.ts`→`.js`→`/index.ts`→`/index.js`；跨模块相对导入（`../../<module>/_shared/x`）build 时改写到目标模块版本目录（目标未构建过则报错，先 `oj build user`）（§8、§2）。
   - 裸 specifier：逐级向上找 `node_modules/<pkg>`，`package.json` 的 `module`→`main`→`index.js`；`@scope/name` 与子路径支持（§8）。
   - CJS 互操作：`module.exports`→`default`、`require` 走 `__ojRequire`，仅裸 specifier；相对 `require("./x")` 不支持（§8、§12）。
   - 安全：解析结果钳制在 project root 内，`..` 逃逸报错（§8）。
5. **全局对象 API 参考**（何时读我：写任何 handler 期间，查签名与语义）
   - 总表照 `user-manual.md` §9 全量抄录并按 Task 1 后的类型核对（13 组：json/http/db+DB/kv+redis/blob/bus/es/log/fetch/ws/plugins + 测试 SDK 仅在测试文件可用）。
   - 每组给最小示例，重点展开：`db.tx`（提交/回滚语义、每请求单事务、忘 await 自动回滚、tx 与 db 同签名）（§9 事务小节）；查询构造器 `db.table().select().where().orderBy().limit().offset().all()`（`WhereCond`/`OrderByItem` 结构见 `sample/global.d.ts`）；`http.param` 路径优先 query 兜底；上传四件套 `http.files/http.file(i)/blob.put/blob.url` 完整例子（§9 上传小节）；`bus.publish/subscribe` 方向约定（HTTP 发布、WS 订阅）与 `bus.kind()`；blob 可调用命名实例 `blob("name").put(...)`。
   - SQL 占位符方言：sqlite/mysql 用 `?`，postgres 用 `$1`（§3）。
6. **响应信封与错误码**（何时读我：设计错误返回或排查非 200 时）
   - `{code,msg,data}`、HTTP 状态=`code`（0→200、`code<=0` 映射 500）；场景表照抄 §10（404/405/408/413/500/路由冲突/编译错误）；业务错误用 `json.fail(code,msg,data?)`。
7. **鉴权与多租户**（何时读我：接口要登录态或多租户隔离时）
   - `auth:` 块启用两层能力：内置路由表 `/auth/login|refresh|logout`（请求体/响应 data 照 §9 表；refresh 轮换）；Bearer 守卫（401 语义）；`anonymous_paths` 尾 `/*` 一层通配语义；`user_table` 最小 schema（bcrypt、roles JSON 串）；`http.user` 读取；`{base}` 外不设防。
   - `tenant` 头与 `http.tenantId`；测试时两头部必带（testing.md 约束）。
   - auth_demo 走读（demo/demo1234）。
8. **测试**（何时读我：写模块测试或搭 CI 时）
   - L1 `oj test`：目录约定（`tests/*.test.ts`）、旗标表、`--format junit`、退出码 0/1 门禁、`client.*`/`client.login`/`describe/it/expect/beforeEach` 用法、两约束（tenant 头、Bearer）、beforeEach 单钩子覆盖警告（testing.md 全量）。
   - L2 vitest：目录、`invoke`/`installGlobals`/`lastPublished` 用法、快与稳的边界（testing.md）。
   - 选型表 + 推荐组合（开发期 L2、CI L1）+ CI YAML 片段（testing.md）。
9. **配置 config.yaml**（何时读我：起服务前定配置，或排查启动 fail-fast 时）
   - 全字段参考表（照 §3：server 全字段默认值、db 多库 DSN 与 scheme 认领、redis 真连 fail-fast、es 存在即启用、plugins 双模式、plugins_dir 四级发现、broker 三种 kind、blob 双驱动、timeout 单位、root 静态服务语义）。
   - 证书三字段必配不可绕过：缺任一路径启动报错退出；宽限期 GET 403、其余方法正常；宽限期尽再启动进程退出；热加载（§3、dev-manual §5.1）。
   - fail-fast 行为清单（连不上 redis、未知 db scheme、unknown broker kind、目录不存在、manifest 缺失/名字不符）。
10. **构建与发布**（何时读我：dev 跑通后要交付时）
   - `oj build [module]`：按模块版本目录构建、转译+minify（`--no-minify` 排障）、api.ts→api.js、剥 `.route`、生成 routes.js、更新 manifests.yaml 锁、确定性 tgz；release 加载语义（锁缺失/损坏/指向不存在版本→报错）（§2）。
   - 目录镜像 vs `.route`：release 下路由事实唯一来源是 routes.js（§7.1）。
   - 发行包布局：`oj-v<version>/{oj, plugins/<triple>/, devkit/}` 与 deploy.sh 用法（ops-manual §1）。
11. **运维要点**（何时读我：服务跑起来之后的证书/日志/排障/升级）
   - 证书生命周期：`oj-cert gen` / `oj-cert renew -k private.pem`（公钥不变）、`--cert-path/--key-path` 覆盖路径不豁免校验、`GET {base}/health` 证书状态探测（宽限/过期仍可访问）（dev-manual §5.1、ops-manual §3/§7）。
   - 热重载语义（dev 模式改文件即生效，mtime 版本化失效缓存）（ops-manual §4）。
   - 超时与资源：`timeout` 熔断 → 408、被杀 runtime 丢弃不回池（ops-manual §5）。
   - 日志：结构化输出与 `log.*` 对应关系（ops-manual §6）。
   - 排障表：摘 ops-manual §7 高频条目（启动失败/403/408/413/404/405）。
   - 插件升级回滚：`.new`/`.bak` 原子换名、`cargo xtask plugin --check` 预检、ABI bump 部署顺序（先插件后宿主）（ops-manual §8、dev-manual §9）。
12. **安全红线与已知限制**（何时读我：任何写 SQL / 拼路径 / 处理上传的时刻；发布前自查）
   - **SQL 注入红线**（加粗、置顶）：动态标识符（表名/列名）只来自 `db.table()` 构造器（SchemaRegistry 白名单），绝不来自 JS 字符串；值只通过绑定参数（`db.query("... where id = $1", [id])`），绝不字符串拼接。
   - 路径安全：目录穿越段 404（含 `%2F` 编码走私）；路径参数已解码仅用于参数化查询与类型转换，勿拼接文件路径/URL；blob key 白名单。
   - v0.2 已知限制全表（§12：相对 require、build 剥 `.route` 仅语句起始标准写法、npm 依赖不打包、旧版本目录不回收、778 特权端口、`.tsx/.mts` 不转译、静态站无 SPA 回退/Range/ETag、release WS URL 版本段）。
   - 常见陷阱清单：`del` 不是 `delete`；`{id}.json` 混字面 pattern 非法；seed.sql 不得含分号字面量；postgres 占位符 `$1`；上传 413 双闸（`max_upload_bytes`）；未配置 blob/es 调用即报错。

- [ ] **Step 3: 一致性核对**

逐项核对（spec §8 附录清单）并在 PR/commit 描述里记录结果：
`json.ok/fail/header`；`http.method/query/headers/body/params/param/tenantId/user/files/file`；`db.query/exec/table/tx`；`DB(name)`；`kv.get/set/del/expire/incr`；`redis.*`；`blob.put/get/del/url/contentType`；`bus.publish/subscribe/kind`；`es.search/index/del`；`log.debug/info/warn/error`；`fetch(url, options?)`；`ws.send/close`；`plugins()`。
Run: `grep -c "何时读我" docs/devkit/api-manual.md`
Expected: `12`
Run: `grep -n "^## " docs/devkit/api-manual.md`
Expected: 恰好 12 个 H2，标题与 Global Constraints 固定字符串逐一一致。

- [ ] **Step 4: Commit**

```bash
git add docs/devkit/api-manual.md
git commit -m "docs(devkit): TS API 开发手册 12 章（模块开发/配置/测试/发布/运维/红线）

unix@vip.qq.com ai"
```

---

### Task 3: 编写 SKILL.md 与 README.md

**Files:**
- Create: `docs/devkit/SKILL.md`
- Create: `docs/devkit/README.md`

**Interfaces:**
- Consumes: `api-manual.md` 的 12 章固定标题（Task 2）；Task 1 类型面。
- Produces: Claude Code skill `oj-api-dev`（frontmatter name）；下游安装入口（README）。

- [ ] **Step 1: 写 SKILL.md**

全文如下（照抄，可微调措辞但不得增删章节引用与红线条目）：

````markdown
---
name: oj-api-dev
description: 在 oj (only-js) 框架业务项目中开发 API 模块时使用——新增或修改 api.ts / WS.ts handler、manifest.yaml、模块测试，或排查路由/信封/鉴权/租户行为时。触发场景：写 handler、建模块、目录镜像路由、.route 参数路由、json 信封、db 查询、oj test。
---

# oj API 模块开发

本 skill 与参考手册 `api-manual.md` 同目录。**按章节号按需读章，不要盲读全文。**

## 工作流

1. **读章**：新项目/新模块 → 手册 §2；写 handler → §3 + §5；用鉴权/租户 → §7；
   写测试 → §8；配置问题 → §9；构建发布 → §10。
2. **脚手架**：模块 = `src/<模块名>/`（首层子目录），内放 `manifest.yaml`
   （`name` 必须等于目录名，违反启动失败）+ 子目录 `api.ts`。
3. **写 handler**：遵守下方红线；响应一律 `json.ok` / `json.fail` 收口。
4. **测试**：先 L2 vitest 测逻辑（快），再 L1 `oj test` 测端到端（真）。两层都绿才算完（§8）。
5. **发布检查**：`oj build` → 确认 `dist/manifests.yaml` 锁与版本目录产物（§10）。

## 红线（不可违反）

- **SQL 注入**：动态标识符（表名/列名）**只**来自 `db.table()` 查询构造器（白名单），
  绝不来自 JS 字符串拼接；值**只**通过绑定参数（`db.query("... where id = ?", [id])`）。
- **方法名**：DELETE 的方法名是 `del`，不是 `delete`（`get/post/put/del/patch/head/options`）。
- **信封**：业务响应只经 `json.ok(data)` / `json.fail(code, msg, data?)` 写回，
  HTTP 状态 = `code`（0→200）。
- 路径参数（`http.param`）已 percent-decode，仅用于参数化查询与类型转换，
  **勿拼接文件路径 / URL**。

## 新模块 checklist

- [ ] `src/<模块>/manifest.yaml` 存在且 `name` = 目录名
- [ ] 目录映射核对：`src/<模块>/<路径>/api.ts` ↔ `GET {base}/<模块>/<路径>/`
- [ ] 方法名映射核对（特别是 `del`）
- [ ] 用了 `.route`？→ 确认镜像路径已按替换语义放弃；确认 build 会剥 `.route`
- [ ] 响应全部走 `json.ok`/`json.fail`；错误码符合 §6 场景表
- [ ] SQL 全部参数化；动态标识符全部走构造器
- [ ] L2 + L1 测试跑过并全绿

## 常见陷阱速查

| 症状 | 原因 |
|---|---|
| DELETE 返回 405 | 方法名写成了 `delete`，应为 `del` |
| release 下参数路由 404 | build 剥了 `.route`，路由以 routes.js 为准——确认先 `oj build` |
| 启动失败 manifest | `name` 与目录名不一致 |
| postgres 占位符报错 | 该方言用 `$1`，不是 `?`（sqlite/mysql 才是 `?`） |
| 启动即退出 | 证书两路径缺一（必配不可绕过）或 redis 连不上（fail-fast） |
| seed 没生效/语法错 | `seed.sql` 按 `;` 切分，语句内不得含分号字面量 |
| 上传 413 | 超 `max_upload_bytes`（axum 2x 兜底 + handle 双闸） |
| `{id}.json` 路由没建 | matchit 参数段不得混字面，拆成静态多段 |
| es/blob 调用报错 | config 未配置 `es.endpoint` / `blob:` 段，配置即启用 |
| WS 连上但收不到广播 | 订阅只在 WS 会话内有效（`bus.subscribe` 在 HTTP 路径报错）；release 下 URL 含版本段 |

## 手册

`api-manual.md`（同目录）共 12 章：1 快速开始 / 2 项目结构与模块约定 / 3 编写 api.ts /
4 导入解析 / 5 全局对象 API 参考 / 6 响应信封与错误码 / 7 鉴权与多租户 / 8 测试 /
9 配置 config.yaml / 10 构建与发布 / 11 运维要点 / 12 安全红线与已知限制。

类型提示：把同目录 `global.d.ts` 拷进业务项目源码根，编辑器/agent 即获得全局对象
（json/http/db/kv/blob/bus/es…）的完整类型。
````

- [ ] **Step 2: 写 README.md**

全文如下：

````markdown
# oj DevKit——TS API 开发手册 + agent skill

面向用 oj 框架开发业务项目的开发者与 AI agent。本目录是发布交付物
（`oj-v<version>.tar.gz` 内 `devkit/`），由仓库 `docs/devkit/` 经
`cargo xtask build` 归置到 `bin/devkit/` 产出。

| 文件 | 用途 |
|---|---|
| `api-manual.md` | 完备开发手册（12 章）：模块开发、全局对象 API、鉴权租户、测试、配置、构建发布、运维、安全红线 |
| `SKILL.md` | Claude Code 等 agent 的 skill 入口：工作流、红线、checklist、陷阱速查，按章节号引用手册 |
| `global.d.ts` | handler 全局对象（json/http/db/kv/blob/bus/es…）的 TS 类型声明；拷进项目源码根即获得编辑器/agent 类型提示 |

## 安装（业务项目）

```sh
# agent 用：拷入项目的 Claude Code skill 目录
mkdir -p .claude/skills/oj-api-dev
cp devkit/SKILL.md devkit/api-manual.md .claude/skills/oj-api-dev/

# 类型提示：拷进项目源码根（与 src/ 平级即可）
cp devkit/global.d.ts .
```

安装后 agent 里说"用 oj-api-dev 开发 xxx 模块"，或 Claude Code 里 `/oj-api-dev` 触发。

## 更新

手册与 skill 随 oj 版本一起发布；升级 oj 后用新包内 `devkit/` 覆盖旧拷贝。
源文件与反馈入口在仓库 `docs/devkit/`。
````

- [ ] **Step 3: 校验引用一致性**

Run: `grep -o "§[0-9]*" docs/devkit/SKILL.md | sort -u`
Expected: 每个章节号 ∈ 1–12，且能对上 `grep -n "^## " docs/devkit/api-manual.md` 的标题。
Run: `head -5 docs/devkit/SKILL.md`
Expected: frontmatter 恰为 `---` / `name: oj-api-dev` / `description: ...` / `---` 结构（YAML 合法，description 单行）。

- [ ] **Step 4: Commit**

```bash
git add docs/devkit/SKILL.md docs/devkit/README.md
git commit -m "feat(devkit): oj-api-dev skill 与 devkit README

unix@vip.qq.com ai"
```

---

### Task 4: xtask copy_devkit + deploy.sh 打包

**Files:**
- Modify: `tools/xtask/src/main.rs`（`build` 分支 + 新函数 + 头部注释）
- Modify: `scripts/deploy.sh`（装配与校验）

**Interfaces:**
- Consumes: `docs/devkit/` 三件（Task 2/3）、`sample/global.d.ts`（Task 1）。
- Produces: `bin/devkit/{README.md,SKILL.md,api-manual.md,global.d.ts}`；tarball 内 `devkit/` 四件。

- [ ] **Step 1: xtask 增加 copy_devkit()**

在 `tools/xtask/src/main.rs` 中、`fn check(` 之前插入：

```rust
/// 归置 devkit（docs/devkit 三件 + sample/global.d.ts）-> bin/devkit/。
/// 仅 `build` 全量归置时调用；`bin`/`plugin` 单体子命令不拖文档。
fn copy_devkit() -> Result<(), String> {
    let src_dir = root().join("docs").join("devkit");
    let dst_dir = bin_dir().join("devkit");
    // 旧拷贝整体替换，避免残留已从源里删除的文件。
    if dst_dir.exists() {
        fs::remove_dir_all(&dst_dir)
            .map_err(|e| format!("rm -rf {}: {e}", dst_dir.display()))?;
    }
    fs::copy_dir_all(&src_dir, &dst_dir)
        .map_err(|e| format!("copy {} -> {}: {e}", src_dir.display(), dst_dir.display()))?;
    let dts_src = root().join("sample").join("global.d.ts");
    let dts_dst = dst_dir.join("global.d.ts");
    fs::copy(&dts_src, &dts_dst)
        .map_err(|e| format!("copy {} -> {}: {e}", dts_src.display(), dts_dst.display()))?;
    println!("copied devkit -> {}", dst_dir.display());
    Ok(())
}
```

`main()` 的 `"build"` 分支改为（插件循环后追加一行）：

```rust
        "build" => {
            build_bin()?;
            for p in PLUGINS {
                build_and_copy(p)?;
            }
            copy_devkit()
        }
```

模块头部 `//!` 注释中「所有产物统一归置到 <repo>/bin/」清单追加一行：
`//!   - DevKit 文档        -> bin/devkit/（docs/devkit + sample/global.d.ts）`

- [ ] **Step 2: deploy.sh 打包 devkit**

在 `cp -R "${TRIPLE_DIR}" "${TEMP_DIR}/plugins/"` 之后追加：

```bash
# Ship DevKit (manual + skill + global.d.ts) alongside the binary.
DEVKIT="${PROJECT_ROOT}/bin/devkit"
if [[ ! -f "${DEVKIT}/api-manual.md" || ! -f "${DEVKIT}/global.d.ts" ]]; then
  echo "Error: devkit artifacts missing under ${DEVKIT} (run: cargo xtask build)"
  exit 1
fi
cp -R "${DEVKIT}" "${TEMP_DIR}/devkit/"
```

头部注释「Output」一行改为：
`# Output: dist/oj-v<version>.tar.gz (containing oj, plugins/<triple>/, and devkit/).`

- [ ] **Step 3: 门禁**

Run: `cargo fmt --check && cargo clippy -p xtask --all-targets -D warnings`
Expected: 均无输出（0 错误）。

- [ ] **Step 4: 产物验证**

Run: `cargo xtask build && ls bin/devkit && diff -r docs/devkit bin/devkit --exclude global.d.ts && diff sample/global.d.ts bin/devkit/global.d.ts && echo OK`
Expected: `bin/devkit` 恰含 `README.md SKILL.md api-manual.md global.d.ts` 四件，diff 均空，末尾 `OK`。

- [ ] **Step 5: Commit**

```bash
git add tools/xtask/src/main.rs scripts/deploy.sh
git commit -m "feat(build): xtask build 归置 devkit 到 bin/，deploy.sh 随包发布

unix@vip.qq.com ai"
```

---

### Task 5: 终验（tarball、CLAUDE.md 指针、全门禁）

**Files:**
- Modify: `CLAUDE.md`（本地文件，gitignored——**不提交**）
- 可能 Modify: 前序任务交付物（若终验发现修正点）

**Interfaces:**
- Consumes: Task 1–4 全部产物。
- Produces: 终验通过记录（本任务无新接口）。

- [ ] **Step 1: tarball 验证**

Run: `bash scripts/deploy.sh && tar -tzf dist/oj-v*.tar.gz | grep devkit | sort`
Expected: 恰好四行：
`oj-v<version>/devkit/`、`oj-v<version>/devkit/README.md`、`oj-v<version>/devkit/SKILL.md`、`oj-v<version>/devkit/api-manual.md`、`oj-v<version>/devkit/global.d.ts`（目录行有无以 tar 实际输出为准——**至少**四个文件条目齐全）。
补验解包可用：`tar -xzf dist/oj-v*.tar.gz -C /tmp && head -5 /tmp/oj-v*/devkit/SKILL.md`。

- [ ] **Step 2: skill 可触发实测**

```bash
mkdir -p .claude/skills/oj-api-dev
cp docs/devkit/SKILL.md docs/devkit/api-manual.md docs/devkit/global.d.ts .claude/skills/oj-api-dev/
```
在 Claude Code 中确认 `/oj-api-dev` 出现在可用 skill 列表（人工验证；`.claude/` 已 gitignore，不入库）。

- [ ] **Step 3: CLAUDE.md 指针（本地，不提交）**

在 `CLAUDE.md` 的架构「Workspace 布局」末尾追加一条：

```markdown
- **`docs/devkit/`** —— 业务项目开发者手册 + agent skill 源（12 章 `api-manual.md`、
  `SKILL.md`、`README.md`）。`cargo xtask build` 归置到 `bin/devkit/` 并随版本包发布；
  `sample/global.d.ts` 由 xtask 一并拾取（单一事实源仍在 sample/）。
```

- [ ] **Step 4: 全门禁**

Run: `cargo fmt --check && cargo clippy --all-targets -D warnings && cargo test --lib`
Expected: 全绿（158 lib 测试基线 + 0 clippy 警告）。

- [ ] **Step 5: 修正与提交**

若终验发现前序交付物需修正：修改后在 commit message 首行标注 `fix(devkit): ...` 并提交；
若一切通过且工作区仅剩 gitignored 文件变更，则无代码提交，任务以终验记录收尾。

---

## Self-Review 记录

- Spec 覆盖：spec §2 布局（Task 2/3/4）、§3 手册 12 章（Task 2）、§4 SKILL（Task 3）、
  §5 构建链（Task 4）、§6 global.d.ts（Task 1）、§7 验收 1–6（Task 4 Step 4 / Task 5 Step 1/2/4、
  Task 2 Step 3、Task 1 Step 2/3）——全覆盖。spec §7.1 提到「xtask 现有测试风格加断言」，
  实况 xtask 无测试模块，改为 Task 4 Step 4 的命令断言（等价更强：真跑真比）。
- 占位符：无 TBD/TODO；SKILL.md 与 README.md 为全文；代码步骤均带实现。
- 类型一致性：12 章标题字符串在 Global Constraints 固定，Task 2/3 共用；`copy_devkit()`
  签名与调用点一致；四件产物清单在 Task 4/5 与 spec §2 一致。
