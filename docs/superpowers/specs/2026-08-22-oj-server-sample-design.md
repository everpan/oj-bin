# oj server v0.1 + user/order 样例 设计文档

> 状态：已与需求方逐节确认（2026-08-22）。上游预案：`docs/cli2.md`。
> 本 spec 是实施计划（writing-plans）的输入，sample 即 `oj server` 的验收标准。

## 1. 背景与目标

`docs/cli2.md` 预案定义了 CLI 应用 `oj`：`oj server -c config.yaml` 读取配置，
以 `-d dir` 为项目服务目录启动 web 服务，目录树镜像为 REST API。与现状
（`docs/cli.md` P0–P6 已完成的 Go parity 移植）的关键差距：

| # | 预案要求 | 现状 | 增量 |
|---|---|---|---|
| 1 | `/v1/api/{module}/{feature}/` → `api.ts` 导出方法 | 旧路由 `{mode}-{version}/...` → `GET.js` 脚本 | 新 router + ESM 执行模型 |
| 2 | dev 跑 `.ts` | 仅 `.js` | deno_ast 转译 |
| 3 | config：host/port + DSN 字符串 + redis | `addr` + `{dsn:}` 嵌套；redis 忽略 | 新 config schema |
| 4 | manifest.yaml（name==目录名） | 无 | 全新 |
| 5 | handler import（含裸 specifier → node_modules） | script 模式无 import | ModuleLoader |
| 6 | 逐请求执行的性能 | per-request 读盘 + 转译 | 两级编译缓存 |

已具备可直接复用：参数化 `db.query/exec`、RequestInfo、统一信封、RuntimePool、
KillSwitch 408 熔断、actor 线程桥、axum 装配。

**决策（已确认）**：sample 与 oj server 全做（sample 即验收）；方案 B——独立
`../../../oj/` crate 承载 `oj`；`oj` **替代** devserver（旧 devserver/旧 router 删除）。

## 2. 产品需求（PRD）

**一句话**：开发者在项目目录里按「模块/特性」组织 `api.ts`，`oj server` 把
目录树原样变成 REST API——改文件即生效（dev），编译制品可直接发布（release）。

**目标用户**：用 TS 写接口、不想碰 Rust 编译链的开发者。oj 是本机开发工具，
也是制品运行时。

**用户故事**：
1. 我在任意深度的特性目录放 `api.ts`，URL 就是该目录相对路径（挂 base 下），
   无需注册路由（**路由与目录层次一一对应**）。
2. 我写 `export default {get, post, put, del, patch, head, options}`，HTTP 动词
   自动映射同名方法；资源存在但不支持该方法 → 405。
3. 模块根放 `manifest.yaml`（name/desc/version），name 必须等于目录名，启动即
   校验、报错带两个名字。
4. `config.yaml` 声明 db/redis；handler 用注入全局 `db`/`http`/`json`/`kv`/`log`，
   零样板。
5. `--dev` 下改 TS 即刻生效；`-d dist` 不带 `--dev` 跑编译制品。
6. 没有 `api.ts` 的目录不是路由，可作纯工具代码目录（被 import）。
7. 我可以在 handler 里 `import` 相对路径工具模块与 `node_modules` 里的包，
   获得完整灵活性。

**非目标（v0.1 明确不做）**：`oj build`（vite 包装；dist 手写制品代替）、真 redis
（内存 KV 模拟 + warn）、mysql/pg 驱动（DSN 可写，启动 fail-fast）、包管理
（不做 npm install，只读 node_modules）、`exports`/`conditions` 完整映射、
完整 Node 互操作（Buffer/process 等）、新路由下的 WS、V8 字节码层缓存
（P6 已证伪 API 不可达）。

## 3. 用例清单（验收标准）

| # | 用例 | 验证点 | sample 载体 |
|---|---|---|---|
| UC-1 | 方法映射全表 | GET/POST/PUT/DELETE→get/post/put/del，PATCH→patch，HEAD→head，OPTIONS→options；未导出→405 | `user/account` |
| UC-2 | db 参数化 CRUD | `?` 占位绑定，insert/update/delete 生效 | `user/account` |
| UC-3 | 参数获取 | `http.param`（query 带默认值）+ `http.body`（JSON） | `user/account`、`user/profile` |
| UC-4 | 路由=目录层次 | 任意深度嵌套目录可达 | `user/profile/detail`（三层） |
| UC-5 | 跨模块关联 | order 联查 user（SQL join） | `order/list` |
| UC-6 | release 模式 | 手写 `dist/*.js` 同规则路由，`-d dist` 无 `--dev` | `dist/` 全镜像 |
| UC-7 | manifest 负向 | name≠目录名 → 启动报错（临时目录用例） | — |
| UC-8 | manifest 正向 | 合法清单加载，启动日志列模块/版本/路由表 | `user`、`order` |
| UC-9 | KV 缓存 | order 详情 miss→db→set，二次命中 | `order/detail` |
| UC-10 | 404/405 | 无 `api.ts`→404；有文件无该方法导出→405；目录穿越→404 | 请求不存在目录 |
| UC-11 | 编译错误 | TS 语法错 → 500 信封带文件:行号（临时文件用例） | — |
| UC-12 | 408 熔断 | `while(true)` → 408，server 存活（ESM 模型下复验） | 临时文件用例 |
| UC-13 | import 链路 | 相对导入工具函数参与处理（含跨模块 `../../`）；裸 specifier 解析失败报错含提示 | `user/_shared`、`order/*` |
| UC-14 | 缓存与热重载 | 同路由连打 → 转译计数仅 1；改 api.ts → 下次请求新结果 | 计数器/日志断言 |
| UC-15 | 裸 specifier | vendored `escape-goat` import 转义订单号；不存在的包报「node_modules 未安装？」 | `order/account` |

> 偏差（实现期）：vendored 包由 nanoid 改为 escape-goat——裸 deno_core 无
> `crypto.getRandomValues`（Web API 由 Deno CLI 扩展提供，core 不含），nanoid v5
> 依赖它；escape-goat 纯字符串操作、零依赖。UC-15 语义不变（裸 specifier 参与请求处理）。

## 4. sample 设计（文件级）

```
sample/
├── config.yaml            # server.host/port + db.default + redis.default
├── seed.sql               # 建表 account(id,name,role)/orders(id,no,account_id,amount) + 种子，幂等
├── package.json           # 声明 escape-goat（vendored）
├── node_modules/escape-goat/   # 直接 vendor 提交（纯 ESM 两文件，测试零网络）
├── .gitignore             # db.sqlite
├── src/
│   ├── user/
│   │   ├── manifest.yaml  # name: "user"  desc  version: "0.1.0"
│   │   ├── account/api.ts # UC-1/2/3：全动词 CRUD
│   │   └── profile/
│   │       ├── api.ts     # get 单查 + JSON body 更新
│   │       └── detail/api.ts   # UC-4：三层嵌套镜像
│   ├── order/
│   │   ├── manifest.yaml  # name: "order"
│   │   ├── account/api.ts # UC-15：escape-goat 转义订单号（建单）
│   │   ├── list/api.ts    # UC-5：join account；import user/_shared（跨模块）
│   │   └── detail/api.ts  # UC-9：kv miss→db→set
│   └── _shared/…          # 仅 user 模块内：见下
└── dist/                  # UC-6：同结构手写 .js（_shared 也输出 .js）
```

工具目录：`src/user/_shared/validate.ts`（`export function requireRole(...)` 等
纯函数，无 api.ts → 非路由）。`user/account/api.ts` 相对导入；`order/list/api.ts`
跨模块 `../../user/_shared/...` 导入（manifest 不约束 import 边界）。

handler 契约（`user/account/api.ts` 骨架）：

```ts
import { requireRole } from "../_shared/validate";

function get() {
  const rows = db.query("select id, name, role from account where id = ?",
                        [http.param("id", 0)]);
  json.ok(rows);
}
function post() {
  const b = http.body;
  db.exec("insert into account (name, role) values (?, ?)", [b.name, b.role ?? "user"]);
  json.ok({ created: true });
}
// put / del / patch / head / options 同理；dist 版为去类型注解的等价 .js
export default { get, post, put, del, patch, head, options };
```

注入全局：`db`（query/exec）、`http`（param/body）、`json`（ok/err 信封）、
`kv`（get/set/del，内存实现）、`log`。

## 5. 架构

### 5.1 Crate 布局（方案 B）

- `../../../oj/`（新 workspace member，package `oj`）：子命令解析（`server`；`build`
  占位报 not implemented）→ config 加载 → 逐 db 开库（共享池）→ 执行项目根
  `seed.sql`（存在则对 default db 执行）→ `Bridge::with_dbs` → actor 池 → serve。
  启动时全量 manifest 校验 + 打印路由表。
- `server/`（mdm-server）：**删** `devserver.rs`、`bin/devserver.rs`、旧
  `router.rs` 及其测试；新 `router.rs` = 目录镜像路由。actor/axum/信封/ws 保留。
- 根 crate：`bridge` 新增 ModuleLoader（FS + 转译 + node_modules 解析）与两级
  缓存；`config.rs` 重写为新 schema。

### 5.2 路由规则

- 路径 = base 之后的相对目录路径 → `<dir>/<path>/api.(ts|js)`（dev 找 `.ts`，
  release 找 `.js`）。
- base 默认 `/v1/api`（`-b` 可改）；`-d` 默认 `src`（dev）/ `dist`（release）。
- 安全：路径含 `..`/绝对路径/空段 → 404。
- 方法映射表（全表自动补齐）：GET→get、POST→post、PUT→put、DELETE→del、
  PATCH→patch、HEAD→head、OPTIONS→options；文件不存在→404；方法未导出→405。

### 5.3 ESM 执行模型

- bridge 实现 deno_core `ModuleLoader`：
  - **相对导入**：`./`、`../`，Deno 风格补全 `.ts`/`.js`/`/index.ts`。
  - **裸 specifier**：`pkg` / `pkg/sub.js` → 从导入文件逐级向上找
    `node_modules/<pkg>`（至项目根 = config.yaml 所在目录为止，与 seed.sql
    定位一致）；命中后 `package.json` 的 `module` →
    `main` → `index.js`；subpath 直接映射包内文件。`exports`/`conditions`
    映射不做（`// ponytail: exports 子路径约束的包报错引导用完整路径`）。
  - **CJS 互操作**：检测 CJS（无 `type:module` 且无 export 语法）→ 标准
    `module/exports/require` 包装，`export default module.exports` + named
    best-effort；`require` 同步走同一解析器；不注入 Buffer/process。
- API 入口契约：`export default {method…}`（named export 不作为入口）。
- 每请求：路由解析 → hash 版本化 specifier（`file://<abs>?v=<content-hash>`）
  → driver 调 `default[method]` → 复用现有事件循环泵完成判定。
- `.ts` 经 deno_ast strip types 转译；`.js` 直读。

### 5.4 两级编译缓存

1. **转译缓存**（Rust 侧，跨 actor 共享）：`path → (mtime, 转译产物)` 单槽
   条目，改文件即失效替换，容量天然有界；dev 热重载 = mtime 变 → 重转译。
2. **模块缓存**（V8 per-isolate， specifier 带 `?v=hash` 免费获得）：内容不变
   → import 命中已编译模块；release 制品不可变 → 常驻命中。
- `// ponytail: hash 版本化 specifier 的旧模块不可卸载，按编辑次数缓慢积累；
  dev 重启清零，release 有界（模块数 × actor 池）`
- V8 字节码层缓存维持 P6 结论不可达，不做。

### 5.5 config schema（URL 风格 DSN）

```yaml
server:
  host: "localhost"
  port: 778
  # timeout: "30s"     # 可选，默认 30s（KillSwitch）
  # pool_size: 4       # 可选，默认 4（actor 线程数）
db:
  default: "sqlite://db.sqlite"
redis:
  default: "redis://127.0.0.1:6379/1"
```

- DSN 统一 SQL/URL 风格（`sqlite://`、`mysql://`、`postgresql://`）；预案的
  PHP 风格 `mysql:host=…` 废弃。
- v0.1 仅注册 sqlite 驱动：非 `sqlite://` DSN → 启动报错 fail-fast。
- redis 配置解析但接内存 KV 并 warn（非持久化模拟）；真 redis 后置。
- db 相对路径相对 config 文件目录解析。
- **删**三层 env 叠加（`cfg.<env>.yml`），预案即单文件。
- `-c` 默认 `./config.yaml`。

### 5.6 manifest

- 首层子目录各含 `manifest.yaml`：`name`（必须==目录名）、`desc`、`version`、
  可选 `config`（v0.1 读取不解释）。
- 启动时全量校验，name 不符 → 报错（含两个名字），拒绝启动。
- 启动日志：模块名/版本/路由表（UC-8）；version 进打包路径留给 `oj build`。

### 5.7 错误处理

| 场景 | 行为 |
|---|---|
| api 文件不存在 | 404 信封 |
| 方法未导出 | 405 信封 |
| 目录穿越/非法段 | 404 |
| TS 编译错误 | 500 信封带 `文件:行号` |
| 模块解析失败 | 500 信封：从哪个文件 import 了什么 + 尝试过的路径（裸 specifier 附「node_modules 未安装？」提示） |
| handler 死循环 | 408（KillSwitch，ESM 模型下 E2E 复验） |
| manifest 非法 | 启动失败 |

## 6. 风险

| # | 风险 | 对策 |
|---|---|---|
| R1 | ESM/TLA 模型下 KillSwitch 408 是否仍有效 | 实现期第一个 spike：UC-12 E2E 先行 |
| R2 | deno_core 模块加载精确机制（load_main_module vs dynamic-import driver、per-request dispatch、完成判定与事件循环泵） | 实现期 spike，回退路径：TLA 主模块 + mod_evaluate |
| R3 | hash specifier 旧模块不可卸载 | 有界说明（编辑次数 × actor 数），接受 |
| R4 | vendored escape-goat 纯 ESM 假设 | 引入时验证（escape-goat v4 ESM-only，两文件） |

## 7. 测试计划

- **cli 集成测试**：起真 server（随机端口）+ sample 目录 → UC-1~6/8/9/13/14/15
  断言；UC-7/11/12 用临时目录变体。
- **单测**：路由镜像与安全、manifest 校验、config 解析、非 sqlite DSN 拒绝、
  node_modules 解析算法、转译缓存失效。
- debug/release 双绿（沿用项目惯例）。
- 粗略实施顺序（供 writing-plans 细化）：M1 oj 骨架+config+路由 → M2 ESM
  执行+转译+缓存（含 R1/R2 spike）→ M3 import 相对+裸+CJS → M4 sample 全量
  + UC 集成测试 → M5 删旧（devserver/旧 router/旧 config）+ 双绿收尾。

## 8. 收官注记（2026-08-22，实施完成）

**状态：** 已实现，`oj server` + user/order sample 全量验收通过，debug/release 双绿
（61 passed + 1 ignored × 2）。commit 链 `587ad16..61ee89e`，实现记录见 `docs/cli2.md`。

**偏差（实施期裁决）：**

| # | spec 原定 | 实际 | 原因 |
|---|---|---|---|
| D1 | sample 用 `nanoid` 生成单号 | vendored `escape-goat`（`escapeHtml`） | 裸 deno_core 无 `crypto.getRandomValues`，无法跑 `nanoid` |
| D2 | sample 端口 778 | 9778 | macOS 特权端口 778 无法 bind（EACCES） |
| D3 | `-d` 相对 config_dir | 相对 CWD | 参数语义沿用「进程 CWD」，未 join config_dir（UX 痣，已记录） |
| D4 | `DELETE→del` 走 `load_main_es_module_from_code` | `load_side_es_module_from_code` | deno_core 每 JsRuntime 仅一个 main module；池化复用下逐请求 driver specifier 递增会撞 MainModuleAlreadyExists |

**计划级修正（TDD 过程中发现）：**
- T1 dev 默认语义：无 `--dev` = release/dist（计划测试元组 `("src",true)` 系笔误）。
- T12 seed.sql 补分号（计划拆分器按 `;` 切分）。
- T9 one-main-module 缺陷（上表 D4）。

**风险复盘：** R1（ESM 下 KillSwitch 408）与 R2（side-module 驱动）均在实现期 spike 验证；
R4（escape-goat ESM-only）经引入验证为真并落地 D1 替代方案。

**终审裁决（集中 review）：**
- `oj server -c config.yaml`（bare 文件名）→ `config_dir=""` → 钳制失效 → **已修复**（`config_dir_of` 回落 `.`，commit `48b57da`）。
- `load_modules` 缺失模块目录回落空 → **保留**（brief 自身 seeds 测试依赖；空项目合法；`-d` 拼错可由空路由表察觉）。
- 其余 deferred minors（T4 尾斜杠归一、T5 http.param 原型链、T6 sourcemap 体积/计数含失败 parse、T7 with_extension 吃 `.min`、module_loader 绝对 file:// 绕过钳制）→ **接受**为 v0.1 已知限制（信任模型：所服务即开发者自有代码，clamp 为纵深防御非沙箱）。
