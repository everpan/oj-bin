# CHANGELIST

以 `oj/Cargo.toml` 的 version 递增提交作为版本分界（该提交即本版本的发布点），fix 类改动在每个版本内单列一组。

## v0.1.10（2026-09-09）

**特性（breaking）**
- WS 执行模型改「帧池」：每路由 W 个无状态 Worker（`ws.workers_per_route`，默认 2）从帧
  队列拉帧执行——内存与连接数解耦（会话态 ≈2KB/连接 vs 独占 6.2MB），帧超时毒化半径
  回归单连接。连接状态新增 `sess.state`（Rust 会话表持久，可 JSON 序列化）与 `sess.id`；
  「模块作用域 = 连接状态」写法废弃（现为 Worker 本地只读缓存）。
- 新增连接闸门 `ws.max_connections`（默认 1000，0=不限）：超限 upgrade 返 503。

**实现**
- bridge 新增 frame_pool（Scheduler per-conn 在飞=1 保序 / Worker 池 / 会话表 / 空池 linger
  退役）；`op_ws_sess_set` + `ReqState.ws_sess` 回传会话态；每连接 ws-js 线程撤销。

## v0.1.9（2026-09-09）

**特性（breaking）**
- `ws.ts` 契约改为生命周期钩子：`export default { connection, message, close, error }`——
  模块每连接加载一次、按事件触发，模块作用域即连接状态（原「整文件每帧重跑」写法废除）。
  订阅挪进 `connection()`（每连接一次）；`error(e)` 兜底帧异常、连接继续；`close()` 收尾
  恰好一次；帧超时必断连；至少导出一个钩子，全缺断连。返回值一律忽略，回帧显式
  `json.ok` / `ws.send`。

**实现**
- bridge 新增 `WsSession` 驻留会话（`ws_connect`/`fire`）：每连接独占 runtime、永不还池；
  JS dispatcher 装配钩子，零新增 op。server frame_loop 三任务（Reader/Writer/bus）不变。

## v0.1.8（2026-09-08）

**特性**
- `fetch` 换成官方 `deno_fetch` 实现，并注入 webpki-roots 根证书；https 请求与 wss 握手共用同一套信任链。

**修复**
- e2e：news 的匿名访问面收窄到 `/news/ws`，任务计数对齐。

**文档 / 杂项**
- fetch 语义切换说明 + `docs/websocket.md` 出站客户端章节（§7）。
- sample：清掉 WS 路由里冗余的匿名条目。

## v0.1.7（2026-09-08）

**特性**
- JS 侧可直接使用标准 WebSocket 客户端（注册 deno_websocket 及其依赖的五个扩展），附任务案例与 API 手册。

**修复**
- 显式安装 rustls CryptoProvider，修掉双 provider 并存导致 ws 客户端初始化 panic。

**文档 / 杂项**
- deno_core 0.410 → 0.411。

## v0.1.6（2026-09-08）

**特性**
- 命名 MQ 客户端：`kafka(name)` / `rabbit(name)` 按名字取用，走新增的 mq 轴 FFI 契约（ABI 仍为 7）；mq poll 用长轮询退避，不烧 CPU。
- 长任务池：JS 常驻任务由监督器托管，支持命名扫描、退避重启、优雅停机；`oj build` 会把 `tasks/` 一并镜像到 dist。

**修复**
- MQ 两轮统一审查整改：消费门禁语义、rabbit channel 泄漏、停机 e2e。
- 测试：SIGTERM 用例补 `#[cfg(unix)]`；插件 drive 改墙钟计时；tla probe 加锁避免并发污染。
- CI：release 上传按草稿 / 已发布分别处理，避开 immutable release 的 422。

**文档 / 杂项**
- 覆盖率三波推进（纯 Rust / 插件离线 / 环境实测），实测 87.82%。
- migrate/seed 启动日志改为「统计 + 错误」，不再逐条刷 INFO。
- 命名 MQ 与长任务手册、`docs/mq-tasks.md` 消费任务教学。

## v0.1.5（2026-09-07）

**特性**
- WS 文件命名统一小写（`ws.ts` / `ws.js`）。
- schema.yaml 支持联合主键（`pk: [a, b]`）。

**修复**
- CI：release 发布幂等，release 已存在则覆盖上传，不再报 create 失败。

**文档 / 杂项**
- sample 模块导览 `MODULES.md`。

## v0.1.4（2026-09-07）

**特性**
- 迁移账本改单表 + `module` 列，数据通道收敛，新增语句级执行日志。

**修复**
- Windows 静态 CRT 统一（`-MT` 注入、规避 MSYS 路径转换），消除 aws-lc-sys 混链告警。
- CI 补上「插件先构建再测试」顺序，以及 bootstrap.js 的 ASCII 红线守护。

## v0.1.3（2026-09-06）

**特性**
- OIDC 全套：内置 OP（discovery / jwks / authorize / token / userinfo）与 RP（login / callback / logout），含 PKCE、state+nonce、JIT 建号、按 tenant 路由到不同 IdP；浏览器跳转腿可用 `tenant.anonymous_paths` 免租户头。
- 鉴权解耦：jwt / bcrypt / crypto 沉淀为 bridge 原语；Bearer 守卫搬进 oj-auth 插件（新增 FFI auth 轴，ABI 5→6）；删掉内置 auth 路由，登录端点改由 sample 的 JS 实现。
- 插件注册改按轴 dlsym 探测（ABI 7），废弃 PluginRegistrations；插件自描述 + `GET {base}/plugins` 查询端点。
- `plugins:` 配置一段三用：键 = 严格清单，值 = 透传配置，空对象 = 回落；旧 list 写法废弃。
- `json.raw` 出裸 JSON。
- devkit（global.d.ts / API 手册 / skill）与主要文档重写对齐当前架构。

**修复**
- review-2026-09-06 整改：CI 守护缺口、clippy 门禁、测试分层。
- 插件 semver 统一取自身 Cargo.toml。
- Windows MSVC 统一静态 CRT（crt-static）。
- OIDC 终审项：jwks 空值守卫 502、code↔client 绑定、JIT 账号按 `oidc:<tenant>:<sub>` 隔离。

## v0.1.2（2026-09-05）

**特性**
- `json.ok` / `json.fail` 自动补 `content-type: application/json`。

## v0.1.1 及更早（2026-08-20 ~ 2026-09-04）

**特性**
- 运行时：deno_core 嵌入 + RuntimePool 复用、KillSwitch 看门狗（超时 408）、inspector、TS 转译与 ESM/CJS 模块加载、`ext_boot.js` 运行时补充。
- 路由与 CLI：目录镜像路由（任意深度 `api.ts`）、路径参数与 RouteTable、`oj server / build / test` 三子命令；build 按模块产出版本目录 + manifests.yaml + tgz，dev / release 模式自动判定。
- 业务能力面：多库 DSN、`db.tx` 单活跃事务、多租户中间件、JWT 鉴权、multipart 上传、blob（local / s3）、Redis KV、事件总线（redis / kafka / rabbitmq）、ES 薄封装。
- 插件系统：五轴注册表 → cdylib + FFI 契约落地，es / db-mysql / db-postgres / blob-s3 / bus-kafka / bus-rabbitmq / kv-redis 全部插件化，core 收敛为 sqlite-only。
- 证书：证书门禁（缺证书拒绝启动、过期限制 GET）、热重载与健康状态、`oj-cert` 工具（gen / renew）。
- 模块数据层：迁移引擎、`schema.yaml` 声明式、检查体系、schema diff、归属守卫。
- devkit：global.d.ts + TS API 手册 + agent skill 随包发布。
- 发行：跨平台打包、终端日志开关、deploy.sh。

**修复**
- 看门狗：terminate 改用 `v8::IsolateHandle` 修 SIGSEGV；KillSwitch drop 不再 join 自身（EDEADLK）并修掉线程泄漏。
- Windows 适配：路径分隔符 / 反斜杠路由键 / YAML 转义 / sqlite 相对 DSN 的 `\\?\` 前缀；kafka 原生依赖构建。
- 浮点绑定改 `Qv::Double` 防 f32 截断；reqwest client 构建失败 fail-fast。
- bus subscribe 失败回滚与按归属清理；插件 vtable 方法统一包 `catch_unwind`。
- `oj build`：vdir 撞名检测、routes.js 的 file 前缀、`-b` 参数消费；`oj-cert --days` 按天计算。
