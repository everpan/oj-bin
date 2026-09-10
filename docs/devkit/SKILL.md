---
name: oj-api-dev
description: 在 oj (only-js) 框架业务项目中开发 API 模块时使用——新增或修改 api.ts / ws.ts handler、manifest.yaml、模块测试，或排查路由/信封/鉴权/租户行为时。触发场景：写 handler、建模块、目录镜像路由、.route 参数路由、json 信封、db 查询、Kafka/RabbitMQ 消费任务、tasks 长任务、oj test。
---

# oj API 模块开发

本 skill 与参考手册 `api-manual.md` 同目录。**按章节号按需读章，不要盲读全文。**

## 工作流

1. **读章**：新项目/新模块 → 手册 §2；写 handler → §4 + §6；用鉴权/租户 → §8；
   写测试 → §9；配置问题 → §10；构建发布 → §11。**要扩展全局对象（`json.page()`
   之类）→ §6 末「ext_boot.js」，不要去改 handler。**
   **接 Kafka/RabbitMQ 或写长任务 → §6「命名 MQ 客户端与长任务」**（任务文件放
   `src/tasks/`，命名 `task_{name}.*` / `{name}_task.*`）。
2. **脚手架**：模块 = `src/<模块名>/`（首层子目录），内放 `manifest.yaml`
   （`name` 必须等于目录名，违反启动失败）+ 子目录 `api.ts`。
3. **写 handler**：遵守下方红线；响应一律 `json.ok` / `json.fail` 收口。
4. **测试**：先 L2 vitest 测逻辑（快），再 L1 `oj test` 测端到端（真）。两层都绿才算完（§9）。
5. **发布检查**：`oj build` → 确认 `dist/manifests.yaml` 锁与版本目录产物（§11）。

## 红线（不可违反）

- **SQL 注入**：动态标识符（表名/列名）**只**来自 `db.table()` 查询构造器（白名单），
  绝不来自 JS 字符串拼接；值**只**通过绑定参数（`db.query("... where id = ?", [id])`）。
- **方法名**：DELETE 的方法名是 `del`，不是 `delete`（`get/post/put/del/patch/head/options`）。
- **信封**：业务响应只经 `json.ok(data)` / `json.fail(code, msg, data?)` 写回，
  HTTP 状态 = `code`（0→200）；标准协议端点（对外契约 JSON）可用 `json.raw(data)`
  出裸 JSON 200（§6）。
- 路径参数（`http.param`）已 percent-decode，仅用于参数化查询与类型转换，
  **勿拼接文件路径 / URL**。
- **消费门禁**：MQ 消费方法（`poll/commit/ack/nack`）**只在 `src/tasks/` 任务文件里
  用**；HTTP/WS handler 里调用直接报错——HTTP 侧发消息用 `send`/`publish`。
- **任务等待**：运行时无 timer 全局，`setTimeout/setInterval` 不可用；任务里等待
  一律 `await tasks.sleep(ms)`，循环退出条件一律 `!tasks.stopping()`。

## 新模块 checklist

- [ ] `src/<模块>/manifest.yaml` 存在且 `name` = 目录名
- [ ] 目录映射核对：`src/<模块>/<路径>/api.ts` ↔ `GET {base}/<模块>/<路径>/`
- [ ] 方法名映射核对（特别是 `del`）
- [ ] 用了 `.route`？→ 确认镜像路径已按替换语义放弃；确认 build 会剥 `.route`
- [ ] 响应全部走 `json.ok`/`json.fail`；错误码符合 §7 场景表
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
| 启动报 `neither api path … specified` / `api path not found` / `static site dir not found` | 准入门三态：`--api-path` 与静态站点（`server.app_path` / `--app-path`）至少显式指定其一，皆指定则两者都必须存在；CLI 路径相对 CWD，config 路径相对 config 目录 |
| seed 没生效/语法错 | `seed.sql` 按 `;` 切分，语句内不得含分号字面量 |
| 上传 413 | 超 `max_upload_bytes`（axum 2x 兜底 + handle 双闸） |
| `{id}.json` 路由没建 | matchit 参数段不得混字面，拆成静态多段 |
| es/blob 调用报错 | config 未配置 `es.endpoint` / `blob:` 段，配置即启用 |
| WS 连上但收不到广播 | 订阅只在 WS 会话内有效（`bus.subscribe` 在 HTTP 路径报错）；release 下 URL 含版本段 |
| 改了 `ext_boot.js` 没生效 | 不做热重载，装配期已冻结 spec——必须重启进程 |
| `ext_boot.js` 里 `await` 报 SyntaxError | 文件无 import/export，被 CJS 启发式包进非 async 函数——加一句 `export {};` |
| `ext_boot.js` 副作用被放大成百上千次 | boot 每个新建 runtime 都跑（模块数 + `pool_size` + WS Worker 数，每路由 `ws.workers_per_route` 个）——只做全局装配，别写库/发广播/打外部接口 |
| `Kafka("x")` / `RabbitMQ("x")` 是 undefined | config `kafkas:`/`rabbits:` 段没配该实例名（或对应插件未装配） |
| poll 报 "requires a task context" | 消费方法只能在 `src/tasks/` 任务文件里用；HTTP/WS 侧发消息用 `send`/`publish` |
| 任务里 `setTimeout` 报 not defined | 运行时无 timer 全局——用 `await tasks.sleep(ms)` |
| poll 报 "instance busy" | 同一实例已有活跃 poller（任务上下文单 poller）——别并发 poll |
| 任务收不到消息就退了/killed | `timeoutMs` 应远小于 `stop_grace_secs`；被 killed = 宽限到期看门狗强杀（不响应 `tasks.stopping()`） |
| 重启后整段消息重复消费 | commit 按 offset+1 推进该分区——多分区主题按分区各 commit 一次（at-least-once，处理须幂等） |
| 改了任务文件没生效 | 任务无热重载——重启进程（转译缓存按 mtime 自动失效） |
| WS 路由 404（文件明明在） | 文件名必须小写 `ws.ts`/`ws.js`——`WS.ts` 无效（v0.1.5 约定） |
| （v0.1.9 已消除）旧帧循环的 const 重复声明 | 新契约为生命周期钩子：模块每 Worker 预载一次——无需处理；可变跨帧状态放 `sess.state`（模块作用域只是只读缓存），见 api-manual §ws.ts |
| WS 帧内 `bus.publish` 自己也收到 | 自回声语义：fan-out 不排除本连接——按字段客户端过滤或发布到别的 topic |

## 手册

`api-manual.md`（同目录）共 13 章：1 快速开始 / 2 项目结构与模块约定 / 3 模块数据层 /
4 编写 api.ts / 5 导入解析 / 6 全局对象 API 参考 / 7 响应信封与错误码 / 8 鉴权与多租户 / 9 测试 /
10 配置 config.yaml / 11 构建与发布 / 12 运维要点 / 13 安全红线与已知限制。

类型提示：把同目录 `global.d.ts` 拷进业务项目源码根，编辑器/agent 即获得全局对象
（json/http/db/kv/blob/bus/es/Kafka/RabbitMQ/tasks…）的完整类型。
