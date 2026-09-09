---
title: 12 · 排障 FAQ
updated: 2026-09-08
---

# 12 · 排障 FAQ

按「你看到了什么」排列。每张表的最后一列是**下一步该做什么**，不只是原因。

## 启动就失败

| 症状 | 原因 | 处置 |
|---|---|---|
| 启动即退出，提示证书 | `server.public_key_path` / `certificate_path` 缺一，**没有开关能绕过** | 配齐两个路径；过期用 `oj-cert renew -k <私钥>` 重签 |
| 启动即退出，提示 redis | 配了 `redis:` 段就是要真连，连不上 fail-fast | 修地址，或注释掉该段回落到内存 KV |
| 启动失败，提示 manifest name | `manifest.yaml` 的 `name` 必须等于目录名 | 改成一致 |
| release 模式拒绝启动 | `migrate_on_start: verify` 且账本落后 | 先 `oj migrate -c config.yaml -d dist` |
| 插件加载失败（日志点名） | ABI 版本不等 / 缺轴 / 依赖库缺失 | 用 `cargo xtask plugin <name> --check` 预检；确认 `bin/plugins/<triple>/` 下有产物 |

## 路由与 handler

| 症状 | 原因 | 处置 |
|---|---|---|
| 404 | 目录名拼错、没有 `api.ts`、文件不在 `--api-path` 下 | 对着启动日志打印的路由表核对 |
| 405（尤其 DELETE） | DELETE 的方法名是 **`del`** 不是 `delete` | 改函数名；方法全集 `get/post/put/del/patch/head/options` |
| 405（其他） | 该 HTTP 方法没导出 | 加导出；可加 `options()` 自证支持的方法 |
| 500 且内容是路由冲突 | 两条路由规则撞在一起（多模块聚合常见） | 看启动日志的冲突行，调整目录或版本 |
| 挂了 `.route` 后原路径 404 | 替换语义：参数路由**替换**目录镜像，不是并存 | 这是设计；要两条就建两个目录 |
| `{id}.json` 这种路由没建起来 | matchit 参数段不允许混字面 | 拆成静态多段，扩展名在 handler 里校验 |
| release 下参数路由 404 | `oj build` 剥了 `.route`，release 以 `routes.js` 为唯一路由来源 | 重新 `oj build` |
| 上传 413 | 超 `max_upload_bytes`（axum 硬顶 + 信封双闸） | 调大配置，或前端分片 |

## 数据库与迁移

| 症状 | 原因 | 处置 |
|---|---|---|
| postgres 报占位符错误 | 该方言用 `$1`，`?` 只在 sqlite/mysql 有效 | 跨方言时统一用 `db.table()` 构造器，它按方言生成 |
| `transaction already active` | 每请求至多一个 `db.tx`，嵌套即报 | 合并成一个事务回调 |
| 日志出现 `open transaction … rolled back` | `db.tx` 漏了 `await` | 补 `await`；数据已按未提交丢弃 |
| 启动卡在「加 NOT NULL 列」 | 框架不猜老数据填什么 → fail-fast 并打印迁移模板 | 按模板写 `migrations/` 迁移 |
| 疑似改名被拒 | 删列 + 加相似名列 → 怕手滑丢数据 | 确认意图后写显式迁移 |
| seed 没生效 / 语法错 | `seed.sql` 按 `;` 切分，语句内不能有分号字面量 | 改写语句，避免字符串里的 `;` |
| `oj schema diff` 退出 1 | 声明与实库漂移（有人绕过声明改了库） | 补齐声明或修库；把它进 CI 防复发 |

## 鉴权与多租户

| 症状 | 原因 | 处置 |
|---|---|---|
| 业务端点全 401 | sample 默认全受保护（除内置 `/v1/api/health`） | 先 `/v1/api/auth/login` 取 token，带 `Authorization: Bearer` |
| 400 缺租户 | `tenant.enable: true` 时头必带 | 加 `X-TENANT-ID`；OIDC 跳转腿要列进 `anonymous_paths` |
| OIDC 跳转被 400 | 302 跳转带不了自定义头 | 把 `/oidc/*`、`/idp/*` 加进 `anonymous_paths` |

## WebSocket

| 症状 | 原因 | 处置 |
|---|---|---|
| WS 路由 404（文件在） | 文件名必须**小写** `ws.ts` / `ws.js`，`WS.ts` 无效 | 改名 |
| 第二帧报 `Identifier already declared` | 每帧重跑同一文件、同一 VM，顶层 `const/let` 第二帧重复声明 | 把声明放进块作用域 `{}` |
| `bus.subscribe` 报错 | 订阅对象是 WS 连接，HTTP 路径调用无效 | 只在 `ws.ts` 里订阅 |
| 连上了收不到广播 | 没订阅该 topic；release 下 WS URL 含版本段（如 `/news-0.1.0/ws`） | 核对 topic 与 URL |
| 自己发的帧自己也收到 | fan-out 不排除本连接（自回声语义） | 客户端按字段过滤，或发到别的 topic |

## MQ 与长任务

| 症状 | 原因 | 处置 |
|---|---|---|
| `Kafka("x")` 是 `undefined` | config 的 `kafkas:` / `rabbits:` 没配这个名字（或插件没装配） | 补配置；取用前判空 |
| `requires a task context` | 消费方法（`poll/commit/ack/nack`）只能在 `src/tasks/` 里用 | HTTP/WS 侧只发（`send`/`publish`） |
| `instance busy` | 同一实例已有活跃 poller | 别并发 poll |
| 任务里 `setTimeout` not defined | 运行时没有 timer 全局 | `await tasks.sleep(ms)`；退出条件用 `!tasks.stopping()` |
| 任务被 killed | `timeoutMs` 应远小于 `stop_grace_secs`；宽限到期看门狗强杀 | 让任务响应 `tasks.stopping()` |
| 重启后消息重复消费 | commit 按 offset+1 推进分区（at-least-once） | 处理逻辑做幂等；多分区各 commit 一次 |
| 改了任务文件没生效 | 任务无热重载 | 重启进程 |

## ext_boot.js（扩展全局对象时才会碰到）

| 症状 | 原因 | 处置 |
|---|---|---|
| 改了没生效 | 不做热重载，装配期已冻结 | 重启进程 |
| `await` 报 SyntaxError | 文件无 import/export → 被 CJS 启发式包进非 async 函数 | 加一句 `export {};` |
| 副作用被放大成百上千次 | boot 在**每个新建 runtime** 都跑（模块数 × pool_size × WS 连接数） | 只做全局装配，别在里面连库、发广播、打外部接口 |

## 更多

- [JS API 手册 · 运维要点与红线](../reference/api-manual/index.md)（第 12、13 章）
- [运维手册](../reference/ops-manual.md)（含完整排障表）
- 术语看不懂？查[术语表](../appendix/glossary.md)
