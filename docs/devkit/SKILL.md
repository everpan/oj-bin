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
