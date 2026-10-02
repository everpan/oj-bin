# oj DevKit——TS API 开发手册 + agent skill

给用 oj 框架写业务项目的人和 AI agent 看的。本目录是发布交付物：发行包
（`oj-v<version>-<triple>.tar.gz` / `.zip`）里的 `devkit/` 就是它，由仓库
`docs/devkit/` 经 `cargo xtask build` 归置到 `bin/devkit/` 产出。

## 里面有什么

| 文件 | 用途 |
|---|---|
| `api-manual.md` | 完备开发手册（13 章）：模块开发、全局对象 API（含 §6 命名 MQ 客户端 Kafka/RabbitMQ 与长任务池、**池化任务与 cron（v0.1.28）**、**邮件投递 `Mail`/`mail`**、**LDAP 目录与鉴证（v0.1.28）**、**部署期常量 `vars`（v0.1.25）**、静态站点与 per-route meta、大整数与 i64、3xx 重定向原语 `json.redirect`（v0.1.26）、`_name_` 目录段（v0.1.27）、多静态站点（v0.1.27）、**WS 房间原语 `ws.join`/`broadcast`/`roomSize`（v0.1.30）**、**blob 上传直传 `blob.uploadUrl` + `PUT {base}/blob/{key}` 路由 + Range 206（v0.1.30）**、**`atob`/`btoa`/`crypto.getRandomValues` wasm 胶水面（v0.1.30）**、ext_boot.js 运行时扩展）、鉴权租户（含 **cookie 会话 + CSRF 双提交 + WS 握手守卫（v0.1.30）**）、测试、配置（含 **`route_timeouts`/`blob_upload_max_bytes`/`response_headers`（v0.1.30）**、**凭据密封 `secrets:` 段与 `oj secret keygen/seal/open`（v0.1.33；X25519 信封，v1 RSA 已移除）**、**资源根 key 多源选择 `--redis/--blob/--es/--broker/--kafka/--rabbit`（v0.1.34；`es`/`broker` 配置段放宽为命名 map，单对象写法兼容）**、**流式响应 / SSE（`json.stream`/`json.sse`，v0.1.35；绕过信封、心跳保活）、CORS（`server.cors` 段，v0.1.35；`credentials` 需显式 `origins`）、应用层 AES-GCM（`crypto.aesGcmEncrypt`/`aesGcmDecrypt`，v0.1.35；仅 AES-128/256）**、**流式查询 `db.stream`（v0.1.37；逐行拉取大结果集，回调 `onRow` + 异步迭代器两种形态、`AbortSignal` 取消；v0.1.38 起 ABI 10——第一方 db 插件真流式，第三方未实现槽位静默回落全量）**、**multipart 大文件流式落 blob（v0.1.38；`http.files[i].key`/`.url`，`http.file(i)` 仅小文件）**、构建发布（含 `oj exec`（v0.1.29））、运维、安全红线 |
| `scenarios.md` | **场景速查（照抄就能跑）**：公开分享页按租户读数据（`db.asTenant`）、SPA 深链回落与每页 meta、静态+动态 meta handler、`oj test` 测试库隔离、LIMIT 分页陷阱、匿名路径通配形态、多库项目按库迁移与对账（`--db`）、雪花 id 生成/精确回写、302 到 blob 预签名 URL、路径参数路由 `_name_` vs `.route`、池化长任务 + cron（v0.1.28）、`oj exec` 数据修复（v0.1.29）、**大文件直传绕开 10MB/30s（v0.1.30）**、**cookie 会话 + CSRF + WS 握手守卫（v0.1.30）**、**WS 房间广播/presence（v0.1.30）**、**`exports`/pnpm 包解析约定（v0.1.30）**、**wasm 引擎包兼容性清单（v0.1.30）**、**流式响应 / SSE（v0.1.35）**、**前端跨域 CORS（v0.1.35）**、**应用层 AES-GCM（v0.1.35）**、**大表流式导出 `db.stream`（v0.1.37）**、**multipart 大文件流式落 blob（v0.1.38）**——每篇给「配置 + 代码 + 验证 + 常见坑」 |
| `SKILL.md` | Claude Code 等 agent 的 skill 入口：工作流、红线、checklist、陷阱速查，按章节号引用手册 |
| `global.d.ts` | handler 全局对象（json/http/db/kv/blob/bus/es/mail/Kafka/RabbitMQ/tasks…）的 TS 类型声明；拷进项目源码根即获得编辑器/agent 类型提示。来源为 `sample/global.d.ts`，经 `cargo xtask build` 与本目录文档一同归置到 `bin/devkit/` |
| `oj-modules.d.ts` | **`#` 导入别名的 ambient 兜底**（`declare module "#*"`）：oj 的 `#x`/`#/m/x` 是「引用方模块」相对别名，TS `paths` 无法表达，无法静态定位的 `#` 导入会被兜底为 `any`（不再报 TS2307）。**非模块** `.d.ts`，须与 `global.d.ts` 一并拷入项目并纳入 tsconfig `include` |

## 安装（业务项目）

```sh
# npm 安装（v0.1.13 起）：postinstall 把对应平台的 oj / plugins / devkit/ 落盘 <项目根>/bin/
npm i @oj-bin/oj
mkdir -p .claude/skills/oj-api-dev
cp bin/devkit/SKILL.md bin/devkit/api-manual.md bin/devkit/scenarios.md .claude/skills/oj-api-dev/   # agent 用
cp bin/devkit/global.d.ts bin/devkit/oj-modules.d.ts .                       # 类型提示 + `#` 别名兜底
# oj-modules.d.ts 须被 tsconfig 收录（如 include 里含 "**/*.d.ts" 或显式列出），
# 否则 `#` 开头的导入在编辑器里仍会报 TS2307「Cannot find module」。
# 业务项目还需按 oj 的别名规则在 tsconfig paths 里补 `#/*`/`#*`（见 sample/tsconfig.json）。

# 或从发行包解包后取 devkit/，路径同上
```

装好后用法：agent 里说「用 oj-api-dev 开发 xxx 模块」，或在 Claude Code 里用 `/oj-api-dev` 触发。

## 更新

手册与 skill 随 oj 版本一起发布。升级 oj 后，用新包里的 `devkit/` 覆盖旧拷贝即可。
源文件与反馈入口在仓库 `docs/devkit/`。

v0.1.43 起新增 `oj openapi` 子命令（从路由表生成 OpenAPI 3.1，含 `--check`
漂移门禁），见 `api-manual.md` 命令表与 `scenarios.md` 场景 24 的「常见坑」。

v0.1.44 起新增 JS 侧声明面 `.schema`（入参契约，违反 → 400）与
`server.schema_validation` 开关，见 `api-manual.md`「入参契约 `.schema`」
与 `scenarios.md` 场景 24。

v0.1.46 起 `json.header` 为追加语义：同名响应头可重复（`Set-Cookie` 合法双发——
登录端点同响应下发 `oj_sess` + `oj_csrf`，cookie 会话 CSRF 双提交闭环），其余头同名
最后一个生效；sample 登录/登出已改写为双 cookie 形态，见 `api-manual.md` §6
`json.header` 与 §8 cookie 会话段。

v0.1.47 起 blob 增 `copy` / `move` / `readRange` 三件套（服务端搬运与区间读，大文件字节
不再进 V8）：`copy` 保留 src、`move` 删 src、`readRange` 是短读截断；见
`api-manual.md` §6 blob 段与 `scenarios.md` 场景 25。

**版本同步要求（发布前自查）**：每次版本升级，本目录四件（`api-manual.md` / `scenarios.md` /
`SKILL.md` / `README.md`）必须与该版的用户可见变更**逐条对齐**——新增/变更的 API 与报错文案要
进 `api-manual.md` 的对应章节**与错误/限制表**，高频陷阱要进 `SKILL.md` 的陷阱速查，
可照抄的场景要进 `scenarios.md`。发布物 `bin/devkit/` 由 `cargo xtask build` 归置，
`cargo test --release -p xtask` 的 devkit 契约用例会校验产物与源文件一致。
