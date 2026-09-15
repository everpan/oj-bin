# CHANGELIST

以 `oj/Cargo.toml` 的 version 递增提交作为版本分界（该提交即本版本的发布点），fix 类改动在每个版本内单列一组。

## v0.1.19（2026-09-15）

**特性**
- **mail 轴：邮件投递（新插件 `oj-mail` + JS 全局 `Mail`/`mail`）**。顶层 `smtp:` 段存在即启用；
  段**一段两用**：整段（含凭据）经 `plugin_cfg` 适配器臂交给 `oj-mail` 插件建 transport，
  宿主另只吸收每个 profile 的 `allowed_from`/`allowed_recipients` 做前置校验——凭据不进宿主
  JS 面。**走适配器臂而非 `plugins:` 透传**：`plugins:` 非空即切严格清单模式，用它会顺带把
  运维的插件装配模式切掉（须列全所有插件）。
  - **多 profile**：`smtp:` 顶层除 `workers`/`queue_capacity` 外每个键都是一个 profile
    （键 = `Mail(key)` / `mail.send` 的 key）；未声明的 key 显式报错（不回落 default）。
  - **四种调用**：`mail.send`（异步 transport）、`mail.sendSync`（同步 transport，worker 内
    `spawn_blocking`）、`mail.enqueue`（入队即回**统一信封** `{code:0,data:{jobId}}`——**非**裸
    `jobId`；真实完成经 `mail.result` 与 bus 上送）、
    `mail.sendRaw`（RFC5322 原文投递，与 `attachments` 互斥）。宿主按方法**覆写** req 里的
    `sync`/`enqueue_only`（JS 侧串用无效）。
  - **队列 + worker 池**：有界队列（`queue_capacity`，满即**背压** `code:4`，不无界堆积）+
    `workers` 个 worker 串行投递；停机 graceful drain。**双通道反馈**：同步路（future resolve
    = 投递结果）与异步上送（`HostContext.deliver("mail.result")` → 宿主按白名单扁平化后存下
    （限长 + TTL）+ 经 bus 扇出给订阅者；`to`/`subject` 等一律不进反馈帧）。
  - **引用式附件**（字节不进 JS、不走 base64）：`{filename, blobKey|blob}`（blob 后端）或
    `{filename, path}`（项目根内本地文件，`ensure_within` 钳制 + canonical 句柄读盘防 TOCTOU），
    二选一；MIME 决议 = 显式 → 扩展名 → 魔数嗅探 → `application/octet-stream`。
  - **rustls 三模式**：`tls`（隐式 TLS）/ `starttls`（强制升级，不回落明文）/ `none`（明文，
    须显式 `allow_none_tls: true`，fail-closed）。认证 `login`（user+pass）或 `xoauth2`
    （静态 `access_token`；只给 `refresh_token` fail-loud）。lettre 0.11 + rustls 0.23.40
    单一版本、无 ring（provider 与框架同为 aws-lc-rs）。
  - **白名单 fail-closed**：`allowed_from`/`allowed_recipients` 后缀匹配（大小写不敏感），
    **空表 = 拒绝**——白名单是「越权发送」的唯一控制点，缺省放行等于开放中继。
    宿主侧 CRLF 一律**剥离**（subject/headers）或**拒绝**（地址）；`headers` 不得覆盖
    From/To/Cc/Bcc/Subject（防白名单绕过）。校验失败一律 `{code:5}` 信封（不抛异常）；
    仅「未配置 mail」抛错。
  - **FileTransport**：`file_transport: <dir>` 给定时不发网络，`.eml` 落盘（测试/归档通道）。
    `oj-mail` 的引擎与 transport 都在插件内，宿主零新增依赖（只 `lettre::Address` 做地址校验）；
    `ABI_VERSION` 保持 8。
  - **CI/工具**：`tools/xtask` 的 `PLUGINS` 增 `"mail"`（CI 矩阵与归置的单一真相源，不硬编码
    副本）；`cargo xtask plugin mail --check` 可预检。JS 类型面见 `docs/devkit/api-manual.md`
    第 6 章 mail 小节。

**修复**
- **mail 统一审查批次 A：enqueue 契约 / worker 强引用释放顺序 / 停机 drain 可达 / 忙等**
  - **`enqueue` 回统一信封**（Blocker）：插件引擎原回**裸** `{jobId}`，与三层公开契约
    （`global.d.ts`、`api-manual` §6、`mail-smtp.md` §6「`res.data.jobId`」）不符 ——
    用户按手册写 `res.data.jobId` 会 TypeError 且拿不到 jobId 去 `mail.result()` 回查。
    引擎改回 `{code:0,msg:"ok",data:{jobId}}`；宿主加**纵深防御**（插件返回顶层无 `code`
    时包成信封，容忍旧/第三方裸载荷）；新增**真插件** e2e（`oj/tests/mail_e2e.rs` 的
    enqueue 用例：统一信封 + 以同队列 `send` 作屏障后 `mail.result(jobId)` 必命中）。
  - **停机 graceful drain 生产可达**（Important）：`MailEngine::shutdown` 此前只被测试调用
    （`allow(dead_code)`，引擎是插件内 `OnceLock` 单例、进程退出不 Drop）⇒ 生产停机实际**丢在途
    邮件**，与文档宣称矛盾。改为**零 ABI 变更**的控制报文 `{"__ctl":"drain","timeout_ms":N}`
    （走既有 `MailVtable::submit`），宿主在停机路径（`server_cmd` 的 SIGTERM/正常退出，
    HTTP 停收 + 任务收场之后）经 `App::drain_mail` 调用；总超时 10s，超时告警不阻断退出。
    新增 `oj/tests/mail_drain_e2e.rs`（真装配 + 真插件：排空后新投递被拒，证明报文到达插件）。
  - **worker 强引用释放顺序**（Important）：`async move` 块的捕获变量只在 **future 被 drop**
    时释放（实测），故 worker 帧里的 transport 强引用晚于退出信号释放 —— 注释声称的顺序保证
    不成立。改为在发退出信号**之前**按依赖序显式 `drop(targets)/drop(deliver)/drop(rx)`，
    使 transport 的最后一份强引用销毁点确定落在 `rt.enter()` 内（不再依赖 tokio 回收任务的
    实现细节）；补「在途 job + 真 pool transport + Drop/drain 超时两条路径」回归护栏。
  - **`submit` 忙等烧核**（Important）：宿主 `FfiMailBackend::submit` 原用 `await_ffi`
    （`yield_now` 空转）驱动插件 future，而 SMTP 往返可达 `timeout`（默认 30s）⇒ 每秒把该
    isolate 的 `current_thread` runtime 空转烧满一核。改用 `await_ffi_poll`
    （`FFI_POLL_BACKOFF = 2ms` 退避；poll/take/free 与取消语义同 `await_ffi`），
    补用例钉住「pending 期间必须退避睡眠」（变异：换回 `await_ffi` ⇒ 20 次 pending 仅 409µs，红）。
- **IDE 类型：`#` 别名报 TS2307、`QueryBuilder`/`json` 声明滞后**（`sample/global.d.ts`、
  `sample/tsconfig.json`、`sample/types/oj-modules.d.ts`）：
  - `QueryBuilder` 补 `join`（`{left,right}[]` + kind）/`distinct`/`groupBy`/`having`/`union`/`with`/`toJSON`；
    `DBInstance` 补 `leaf`/`and`/`or`/`not`/`fromJSON`/`asSystem`；`JsonApi` 补 `raw`（裸 JSON 200）；
    `WhereCond.field` 改可选并补 `not`（纯组合节点，与运行期 `condObj` 一致）。
  - **`#` 别名 TS2307**：`#x`/`#/m/x` 是「引用方模块」相对，TS `paths` 只能静态枚举模块目录，
    未创建或不在枚举内的别名（如 `#_shared/view`）报 TS2307。修法：新增**非模块** `.d.ts`
    （`sample/types/oj-modules.d.ts`）内的 `declare module "#*";` 作真 ambient 兜底——命中真实
    文件仍走真类型，未命中降级为 `any`（放在模块化的 `global.d.ts` 里无效，`paths` 命中时
    ambient 也被忽略，均为实测）。该文件随 devkit 分发（`copy_devkit` 增归置 + 守卫测试），
    业务项目须与 `global.d.ts` 一并拷入并纳入 tsconfig `include`（`docs/devkit/README.md`）。

**行为变更（升级注意）**
- **新增顶层 `smtp:` 段**：此前该键被忽略，现在会被解析——若旧配置里恰好有同名且**非映射**
  的键（如 `smtp: false`），启动会解析失败；改名或删掉即可。不写该段则行为完全不变
  （`mail.*` 调用报 `mail not configured`）。
- **配了 `smtp:` 但未装 `oj-mail` 插件**：不阻断启动，`mail.*` 调用报
  `mail not configured (config smtp: section missing, or oj-mail plugin not loaded)`
  ——与 es/auth 的「配置声明即 fail-fast」不同（mail 缺插件不构成安全失守，故意放行启动）。
  装插件：`cargo xtask plugin mail`。
- **`file_transport` 目录须先存在**（lettre 不建目录）；非空 `allowed_*` 是发信前提。



**特性**
- 导入别名 `#x`（**本模块根**）/ `#/m/x`（**src 根**，首段 = 模块目录名）：共享库不再按目录
  深度数 `../`。`src/m1/a/b/c/d/e/f/g/api.ts` 引 `m1/_shared/xxx.ts` 写 `#_shared/xxx`
  （显式后缀 `#_shared/xxx.ts` 与目录索引 `#_shared` 同样支持），跨模块写
  `#/user/_shared/validate`。
  - **锚点由文件自身位置派生**：从引用方目录向上找最近的 `manifest.yaml` 祖先 = 模块根，
    其父目录 = src 根。于是 dev（`src/<m>/`）、release 产物（`dist/<m>-<v>/`，manifest
    原样复制）、tasks 镜像三处语义天然一致，**无需把 api 根路径穿透到装配点**（`LoaderShared`
    形状零变更；引用方在 `node_modules` 内不启用，不劫持第三方包的 `package.json#imports`）。
  - 模块外（`tests/` 用例目录、任务池）无锚点 → 明确报错并给下一步。
  - **release 语义**：`oj build` 期实化为版本目录相对路径（跨模块目标按
    `dist/manifests.yaml` 锁钉版本），产物内**不含** `#`。任务池非版本化 → 别名一律拒绝。
  - **S008**（`oj build` 内嵌，`--check` 同跑）：别名目标必须存在；`#/其他模块/…` 必须在
    `manifest.yaml` 声明 `deps`（跨模块代码耦合纳入与表归属 S003 同一套归属图）。既有
    **相对**跨模块引用不追溯——追溯会让既有项目升级后 build 直接失败。
  - **产物自洽断言**（两道）：每个产物文件内不得残留 `#`（`assert_no_aliases`）；构建末尾扫
    **本次产出的目录**（各模块版本目录 + tasks 镜像）内**任何**本地 specifier 都必须落到已落盘
    文件（`assert_dist_consistent`）——「dev 能跑、release 悬空」这类问题从运行期静默炸变成
    构建期显式失败。不扫整个 `dist`：陈旧的他模块产物不该让本次构建失败。
  - `#` 与 Node `package.json#imports` 共用命名空间：项目声明了 `#` 开头的 imports 键 →
    S008 fail-fast（避免 Node/vite 与 oj 两套解析器分叉）。
  - 编辑器/测试工具对齐：`sample/tsconfig.json` 补 `paths`（`"#/*": ["./src/*"]` +
    `"#*": [各模块 /*]`；值须带 `./` 前缀，否则无 `baseUrl` 时 vite/esbuild 会告警；
    不放 `baseUrl` 以免改变裸包解析）；L2 单测新增
    `sample/unit/vitest.config.ts`（`resolveId` 插件镜像同规则，否则被 spec 直接 import 的
    模块内文件用了别名即解析失败）。

**修复**
- **目录索引导入在 release 悬空**：`import x from "../_shared"`（= `_shared/index.ts`）dev 能跑，
  build 却因改写只做「末段补 `.js`」而产出 `../_shared.js`（不存在的文件）。构建期改为
  **探到真实文件**后产出 `_shared/index.js`。
- **构建期改写口径与运行期分叉**：主构建路径此前是行级 `from "…"` 口径，漏**副作用**
  `import "…"` 与**动态** `import("…")`（tasks 镜像用的是另一个更全的扫描器）。现将两侧
  归一到唯一扫描器 `bridge::import_scan::specifier_spans`（跳过注释/普通字符串与模板串，
  返回字节 span，支持一行多 specifier；build 与 checks 共用，同 `bridge::guard::extract_tables` 先例）。
- **`api.ts` 导入禁令可被前缀绕过**：守卫此前只扫相对 specifier，`#item/api`、
  `#/user/item/api` 可溜过。现扫全部本地 specifier（npm 包内子路径 `pkg/api` 不在守卫面）。
- 构建期与运行期**共用同一份解析探针**（`resolve_relative` / `resolve_alias`），
  「dev 能跑、release 悬空」的一类缺陷在结构上被消掉。
- **L2 db mock 缺构造器面**：v0.1.17 起 sample 多处改用 `db.table(...)` 构造器，而
  `sample/unit/mocks` 只实现了 `db.query`/`exec` → `npm run test:unit` 在 HEAD 即 2 例失败
  （`db.table is not a function`）。新增 `mocks/query-builder.ts` 镜像 `bootstrap.js` 的
  `builderFromReq` API 面（select/where/orderBy/limit/offset/insert/update/delete/returning/
  all/run/toSQL + `update|delete` 无 where 即抛的守卫），SQL 按规范形渲染（小写、`col, col`、
  `?` 占位；真实渲染由 sea-query 按方言产出，mock 不复刻方言），未覆盖形态直接抛错。
  L2 12/12 恢复绿。
- **Windows 别名解析误判「未找到模块根」**：`module_root_of` 用词法 `starts_with(project_root)`，
  而 `canonicalize` 给 referrer 目录加 `\\?\` verbatim 前缀、`ModuleSpecifier::to_file_path`
  还原时又剥掉——同一条长名路径仅差此前缀，导致 `oj build` 内省（`alias_build_materializes_to_versioned_relative_paths`）
  在 Windows CI 失败。现新增 `strip_verbatim` 在比较前归一两侧前缀；并在构建入口（`run`）与
  dev 服务端（`app.rs`）对 `src`/`project_root` 统一 `canonicalize` + `strip_verbatim`（与
  `resolve_to_segs` 的 `strip_prefix`、`oj-plugin-ffi` 的 `dunce::simplified` 同约定），
  Windows 与 unix 路径形态从此一致。新增 `#[cfg(windows)]` 回归用例
  `module_root_of_tolerates_verbatim_prefix_mismatch`。

**行为变更（升级注意）**
- **本地 import 目标必须是 `.ts`**：`import "./x.js"` / `import "./d.json"` 这类**非 `.ts` 目标**
  在产物里没有对应文件（`collect_module` 只转译 `.ts`），此前会静默产出悬空 specifier、
  release 才炸；现 `oj build` 直接失败并给出下一步。若你的项目依赖该写法，请把目标改成 `.ts`
  （或把内容内联）。
- **`manifest.yaml` 只允许出现在模块根**（`src/<module>/manifest.yaml`）：嵌套声明会让 `#` 别名的
  锚点（向上最近的 `manifest.yaml`）指向嵌套目录，静默改写整棵子树的别名语义 → 现 fail-fast。
- **tasks 池 import 不得越过池根**（跨模块相对导入等）：任务池是非版本化资产，只镜像
  `dist/<tasks.dir>/`，池外目标在产物里必然不存在 → 现 fail-fast（此前静默悬空）。
- **release 模式不再解析别名**：`resolve_alias` 在 `ts=false` 时直接报错（产物本不该含 `#`；
  此前同模块别名会"半可解析"、跨模块悬空，语义不一致）。

**评审修订（开发 / 架构双专家，处置记录见 spec §12）**
- 修：**拼接实参的动态 import 被误改**（`import("./locales/" + lang)`）——`import(`/`require(`
  形态要求实参为孤立字面量（闭合引号后只能跟 `)` / `,`），`from` 形态不允许 `(`（排除
  `Array.from("./x")` 这类方法调用）。这是本特性引入的回归，已由 core 单测钉住。
- 修：**检查比运行期更严**——`checks::collect_ts` 与 `build_cmd::walk` 此前不跳过 `node_modules`
  （运行期明确跳过），第三方源码可致 S008 误报、甚至被打进产物；两个 walker 现与
  `resolve_inner` 口径一致。
- 修：**扫描器下移到 core** `src/bridge/import_scan.rs`（build 与 S008 共用唯一实现），
  解掉「结构检查层依赖构建管线」的分层倒挂（同 `bridge::guard::extract_tables` 先例）。
- 修：构建期改写报错补上**违规文件路径**（此前只报目录、无下一步）。

**文档 / 杂项**
- sample 迁移为两种写法并存（别名：`user/account`、`admin/menu-list`、`cert/renew`、
  `auth/refresh`、`order/list`、`idp/token`、`oidc/callback`；相对：`admin/role-item`、
  `cert/item`、`idp/login`、`idp/authorize`、`oidc/login`、`oidc/logout`）；
  `idp`/`oidc` 的 `manifest.yaml` 补 `deps.auth`（S008 要求）。
- 手册：user-manual §4/§8、api-manual §5、dev-guide §7、cli2 build、testing §L2、
  sample/MODULES.md 补别名规则与边界（含「只能在模块内用」「跨模块需声明 deps」、
  tsconfig `#*` 手维护）；user-manual §12 与 api-manual §12「已知限制」表补新约束；
  内部走读 docs/modules 01/04/07/08 同步（build 改写函数改名与两道断言、S008 进规则表、
  L2 mock 的构造器面）；devkit SKILL.md 补新模块 checklist 与常见陷阱三行；README /
  README_cn 特性列表补别名一条；cli2 的 `--check` 规则范围与 route-params-design 的过时段
  标注同步。

## v0.1.17（2026-09-15）

**特性**
- 构造器 `insert` 取回自增主键：`db.table("t").insert({...}).returning(["id"]).run()` →
  行数组 `[{id: n}]`（不带 `returning` 时仍返回受影响行数）。列过白名单
  （`unknown column '<c>' in insert returning`），仅 insert 接受——动词×字段矩阵在 op 侧
  权威校验，`select/update/delete` 报 `<verb> does not accept returning`。
  pg / sqlite 由 sea-query 渲染单条 `RETURNING` 语句、**一次往返**；mysql 方言无
  RETURNING，只接受**单列**，在同一执行目标上两步取 `LAST_INSERT_ID()`（非原子，并发请放
  `db.tx` 内）。`toSQL()` 产物同步带 RETURNING。

**加固**
- 补「事务 × 多租户」回归（此前只有池路径用例）：租户注入发生在 `resolve_target`
  **之前**，与「走池还是走会话」正交——`tx.table(...)` 与 `db.table(...)` 的改写完全一致
  （事务内 select 收窄到当前租户、insert 强制写当前租户、update 改不到他租户的行、
  回滚不留痕）。

**文档 / 杂项**
- sample 三处 `insert ... returning id` 裸 SQL 改为构造器（admin/menu-item、
  admin/role-item、cert）——此前这些裸插入在 `sql_guard=warn` 下会静默写入无 `tenant_id`
  的行（裸 SQL 只查「文本里有没有 tenant_id」，不注入）。
- api-manual（DML：returning 小节；事务：租户防护说明）、db-guide（§4 事务改用构造器、
  §7 DML returning）、global.d.ts（QueryBuilder 补 insert/update/delete/run/returning/toSQL
  声明——此前只声明到 select/all）。

## v0.1.16（2026-09-14）

**特性**
- WS 二进制帧支持（Plane 协同文档/Yjs 场景解锁；PRD
  `plane/docs/ever/prd/oj-feature-request-ws-binary.md`）。入侧：客户端 Binary 帧（opcode 0x2）
  原字节透传，`http.body` 为 `null`（不再 `from_utf8_lossy` 产生垃圾），新增
  `http.bodyBytes(): Promise<Uint8Array>`（文本帧同样可用，返回原始帧字节）。出侧：
  `ws.send(data)` 接受 `string | Uint8Array`——string → Text 帧（0x1）、Uint8Array →
  Binary 帧（0x2），帧型由参数类型决定。`sess.state` 语义不变（必须可 JSON 序列化；
  Yjs awareness 等二进制状态走 base64 或 kv，见 api-manual §13）。
- bus 字节化（ABI 7 → 8，**需同步重编全部插件**）：`bus.publish(topic, data)` data 传
  `Uint8Array`/`ArrayBuffer` → 订阅 WS 会话收 Binary 帧（原字节，不包信封）；JSON 数据行为
  不变（`{"topic","data"}` Text 帧）。wire 约定：JSON → record payload = 信封 UTF-8；
  字节 → record payload = 原始字节；消费侧启发式（UTF-8 且为含 topic+data 的 JSON 对象 →
  文本信封，否则二进制透传）。FFI `EventBrokerVtable.publish` data 与 `HostContext.deliver`
  payload 改 `RBytes`（stabby Vec<u8>）；oj-bus-kafka / oj-bus-rabbitmq 消费循环去 lossy
  直通字节。
- 命名 MQ 二进制载荷（零 ABI，`OjMqMessage` 词汇表扩展）：`Kafka("x").send(topic,
  {value: new Uint8Array(...)})` / `RabbitMQ("x").publish(..., uint8array, ...)` → base64 进
  `value_b64`，record 载荷 = 原始字节；poll 侧非 UTF-8 载荷 → `value = null` +
  `value_b64`（base64 字符串）。
- `oj test` L1 WS 帧测试面 `client.ws(path)`：首次使用惰性起 127.0.0.1:0 本地服务
  （真实路由 + 真实帧循环），`send(string|Uint8Array)` / `next(ms?) → {binary,data} |
  {closed:true} | null` / `close()`。可运行用例 `sample/tests/ws-bin.test.ts`（模块
  `sample/src/echo-bin/`：二进制回显，字节相等 + 非 UTF-8 无损）。

**修复**
- `oj test` 的 `client.ws(path)` **服务端主动推帧读不到**：隧道只在 `send()` 内惰性建立，
  `next()` 拿 `null` 句柄调 `op_client_ws_next` → `TypeError: expected u64`。而 connection
  钩子里的 `ws.send`（连接即推送）与 bus 广播都发生在客户端开口之前 → 首帧必丢。改为
  `send`/`next`/`close` 共用一个惰性建连、`close()` 未建连即空操作；补回归用例
  `ws server-push`（读 `/news/ws` 的连接钩子帧）。
- `client.ws(path).next()` 的**文本帧解码恒抛** `ReferenceError: TextDecoder is not
  defined`：test ext 不注册 deno_web，运行时无 `TextDecoder`（同 G① 的 `atob` 缺口）。
  原实现依赖 `new TextDecoder()`，而样例只读过二进制帧故从未暴露；现自带最小 UTF-8 解码
  （1–4 字节含代理对），并把 `sample/src/echo-bin/ws.ts` 改为**帧型忠实回显**使其可覆盖
  （文本帧→文本帧、二进制帧→二进制帧），补用例断言 `"ping ✓ 汉字 😀"` 原样往返。
- blob-s3 插件：**明文 http 端点下整个 s3 驱动不可用**。object_store 默认 `allow_http=false`
  → reqwest 客户端 `https_only(true)`，于是 `endpoint: "http://…"`（= `sample/config.yaml`
  自带的 MinIO 示例）在「建请求」阶段即报 `builder error for url (…)`（0 retries / 微秒级），
  put/get/url 全废；https 端点不受影响。现按端点 scheme 显式 `with_allow_http(true)`，并补
  **离线**回归测试 `http_endpoint_reaches_network_not_url_builder`（打本机必然关闭的端口，
  只断言错误类别 = 网络阶段而非建请求阶段）——env-gated 的 `real_s3_roundtrip_via_vtable`
  从不进 CI，正是这个 100% 断链漏进发布物的原因。

**文档**
- 新设计文档 `docs/superpowers/specs/2026-09-14-ws-binary-frame-design.md`（WsSend 统一
  枚举、bus wire 约定与启发式边界、ABI 8 清单）。
- api-manual §4（ws.ts 二进制帧小节 + bodyBytes 示例）、§6（http/ws/bus 表、命名 MQ
  value_b64、client.ws 行）、§9（client.ws）、§13（bus wire 约定与启发式边界、二进制
  状态建议）；SKILL.md 陷阱 +2；websocket.md §1.1 二进制帧 + 修正 http.body 旧表述；
  global.d.ts（WSApi.send 联合类型、http.bodyBytes、OjMqMessage.value_b64、TestWs）。

**CI/插件**
- `cargo xtask plugin <name> --check` 门禁随 ABI 8 生效；旧 ABI 7 cdylib 报
  `plugin ABI mismatch: plugin=7 host=8`——`cargo xtask build` 重编即可。

## v0.1.15（2026-09-13）

**特性**
- `tenant.sql_guard` 多租户 SQL 防护（false（默认，不改写 SQL）| `"warn"` 注入+告警 |
  `true`/`"deny"` 注入+拦截）：`db.table()` 构造器自动注入租户条件——select 基表限定列
  注入、join 进 ON 子句（不影响 left join 语义）、条件树/子查询/union/with 递归、
  insert 强制当前租户值（mismatch 报错）、update/delete 自动收窄、sets 显式改
  tenant_id 拒绝（防行迁移越权）；裸 SQL（`db.query`/`db.exec`）查「完全遗漏 tenant_id」
  warn 告警 / deny 拦截（Deny 另要求参数数组含当前租户值；DML/DDL 未识别表 fail-closed），
  双引号/反引号标识符保留以兼容 toSQL 回放。逃生口 `db.asSystem()`：请求级 system 标志
  （ReqState 重置即失效）+ 服务端审计日志。schema.yaml 表级 `tenant: false` 共享表声明
  （缺省 true）+ config `tenant.shared_allow` 白名单交集生效（fail-closed），
  `validate_tenant` 挂三处：server 启动 / `oj build`（checks）/ `oj migrate`；
  `enable=false` 而 guard 非 Off 启动 warn。新人白话文档 `docs/tenant-guide.md`；
  api-manual 第 8/10 章同步。**已知边界**：租户头为客户端自报，防伪造须 JWT claims
  绑定（规划见设计文档 §7，另立任务）；裸 SQL 字面检查为 best-effort（只防完全遗漏）。
  设计/评审：`docs/superpowers/specs/2026-09-13-tenant-sql-guard-design.md`。

**CI**
- `ci(release)`：新增 `guard` 去重 job——`gh release create` 会把刚建的 tag 推回远端再次命中
  `push: tags` 触发整条流水线重跑（并自动把草稿转正），浪费三平台构建/测试资源。`guard` 在
  tag push 且对应 release 已存在时（即「自己推的 tag」）输出 `skip=true`，`lint`/`package` 加
  `needs: [guard]` + `if: needs.guard.outputs.skip != 'true'`，下游 `publish`/`publish-npm`/
  `smoke-npm` 级联跳过；真实 tag 推送与人工核对后重发仍走全量。
- `fix(ci)`：上条 guard 把 `publish-npm` 一并级联跳过（`needs: package` 在重爬 run 里被
  取消），且重爬 run 无 dist 产物可供 `npm-publish.sh` 使用——`publish-npm` 改为只依赖
  `guard`，产物按 run 类型二选一：首发 run 用本次构建产物；重爬/转正 run（skip=true）
  从 release 资产 `gh release download` 取同款产物，照旧走幂等 publish（已发布即 skip）。
  新增 `release: types: [published]` 触发：草稿（dispatch draft=true）不发 npm，人工核对
  点「发布」转正那一刻由 release 事件接续补发 npm（guard 对 release 事件输出 skip=true，
  重型 job 全跳）——草稿门禁由此真实生效；tag 重爬 run 退化为幂等空跑。
  `publish-npm` 加 job 级 `concurrency: npm-<tag>`（不取消、排队）：转正瞬间 release run
  与重爬 push run 并发双发 npm，publish_pkg 虽幂等（npm view + 409 re-view），但 409 后
  re-view 撞上 registry 传播延迟可能误判失败，按 tag 串行化硬消除该窗口。

**文档 / 杂项**
- db：文档与 sample 全面改写为 `db.table(...)` 构造器语法，替代原生 `db.query` 参数化查询——
  `docs/`（db-guide / user-manual / dev-guide / bridge / testing / cli2 / route-params-design
  / devkit SKILL & api-manual / superpowers plans & specs / MODULES）+ `sample/src/**/api.ts`
  （account / item / profile / order-{account,detail,list} / auth-{login,refresh} / idp/login /
  oidc/callback / cert/* / admin/*）。构造器不支持的列别名 / `RETURNING id` /
  `last_insert_rowid` / 动态列清单 / join 别名等场景保留原生查询（注释标明）。

**修复（集中审查）**
- guard：`Join.tenant_id` serde 预设可被 `db.fromJSON` 投毒（绕过 JS 链层直喂 serde，
  评审 P0）——`apply_tenant` 改为对受约束 join 表**无条件覆盖**注入值，JS 预设永不生效；
  tokens() 此前整段跳过 `"…"`/`` `…` `` 引用标识符 → `FROM "t"` 对表提取失明（裸 SQL
  守卫静默放行）——引用标识符内容现在成词，`"t"` 形态可见（toSQL 回放从 vacuous pass
  变为真校验）。附回归用例：fromJSON 投毒覆盖 + 引用标识符单元/行为用例。
- oidc_e2e：夹具 `_platform` 补 schema.yaml（users 表声明）——本版本内 idp/login 改写
  为 `db.table()` 构造器后，表须经 SchemaRegistry 白名单（registry 来自
  schema.yaml，seed.sql 只建物理表），夹具缺声明致 login 500、set-cookie 缺失 panic。

## v0.1.14（2026-09-12）

**特性**
- `db.table` 查询构造器（对齐 xorm builder / sea-query，单 op 扩展 + toSQL）：`select` 等价 +
  `toSQL` 只构造不执行（`op_db_query_sql`，含方言占位符 + 参数）；嵌套条件树
  `and/or/not` 递归编译（深度 8 / 叶子 64 上限）+ JS 条件对象（工厂 / 不可变组合 / tree /
  fields / has，零新 op）；`Verb` 动词 + 动词×字段兼容矩阵（op 侧权威校验）；DML
  `insert/update/delete`（tx 路由 + 返回行数 + `run()` 终执行）；`join` 联表（inner/left，
  限定列校验，自 join 拒绝，join 表过归属守卫）；聚合列（fn 枚举 + `distinct`）+ `groupBy`
  + `having`（聚合别名 op 侧展开，PG 方言兼容）；`toJSON/fromJSON` 序列化（快照复原可继续链 /
  执行）；`where` 子查询（`in` / 标量比较）+ `exists` + 嵌套 `select` 地基
  （`REQ_NEST_MAX=4`）；`union/union all`（显式列 + 列数校验 + 成员禁排序分页，嵌套禁
  unions）；`case` 列 + 窗口函数列（`row_number/rank/dense_rank`，别名不过 having 台账）；
  非递归 `CTE`（`with`，虚拟表列白名单，CTE 名跳过归属守卫）。
- `oj server --daemon` 后台运行（unix `setsid` 重 `exec` / windows `DETACHED_PROCESS`）。

**实现**
- `op_db_query_build` 拆 `guard_req` + `build_statement` 两段（`select` 等价）；`ColCtx`
  限定列解析（六处共用，`apply_op` 泛化）；`CondTree` 手写 `Deserialize`（按键唯一分发，
  空组 / 多键精确报错）。

**修复**
- `fix(query)`：守卫遍历 `CASE` 列内嵌套 `req`（F-1）+ 嵌套 `offset` 门禁 + `when` 数上限
  （统一审查修复）。

**文档 / 杂项**
- `docs(spec)`：`db.table` 设计（方案 A 单 op 扩展 + toSQL / 条件对象 / 序列化简 / 双评审
  架构+实现修订）；分阶段实施计划（7 Phase / 14 Task，TDD）。
- `docs(v0.1.14)`：`db.table` 构造器文档（DML / 条件树 / 条件对象 / join / 聚合 / toSQL /
  序列化）。
- `docs(db-guide)`：新人手册——配置 + JS 全量 `db` API + 边界红线；「写第一个 handler」补
  `async/await` 等价写法（sample admin 风格）；CTE 概念与原理解读（临时视图心智模型 /
  渲染形态 / 声明列契约 / 遮蔽与校验顺序）。
- `docs`：运维手册补 npm 分发段（`@oj-bin/*` 手动发布 + 内建门禁）。
- `build(deps)`：oj 测试依赖 `jsonwebtoken` 11.0.0（oj-auth 与根测试仍 9.3.1，树内双版本）。
- `test(e2e)`：query builder join + insert 走 HTTP 全链路。
- `build(docker)`：多阶段构建 + distroless runtime，glibc 2.31 基线。

## v0.1.13（2026-09-11）

**特性**
- **npm 分发**：`@oj-bin/oj` 主包 + 平台子包模板与 npmjs 展示页；`postinstall` 落盘
  `./bin`（支持面检测 + 原子替换 + `exit 0` 语义）；`npm-publish.sh` 装配 / 幂等发布 /
  门禁 / 置信断言 + dry-run 自检。

**实现**
- `fix(npm/postinstall)`：加固 bail 输出与 `INIT_CWD` 哨兵，避免击穿 `exit 0`。
- `fix(npm)`：避免 tarball 清单 `grep` 命中即退导致 `tar` 收 `SIGPIPE` 假失败。

**CI**
- `ci(release)`：新增 `publish-npm` + `smoke-npm`——npm 双发（失败标红不阻塞 Release）；
  `publish-npm` 草稿模式跳过（npm 不可撤回，人工核对后幂等补发）；发布门禁——源文件缺席
  冒烟（`deploy.sh` / `deploy.bat` / `release.yml`）。

**文档**
- `docs`：npm 分发方案 spec（平台子包 + optionalDependencies，双发不阻塞）→ spec 评审修订
  （改 scoped `@oj-bin/oj`、独立 `publish-npm` job、`postinstall` 加固）→ 实施计划
  （5 任务）→ 全分支终审报告归档（可合入，零 must-fix）；npm 安装段入 README（spec 同步
  `postinstall` 两处实现级修正）。

## v0.1.12（2026-09-11）

**修复**
- **release 产物在非构建机无法初始化 JsRuntime**（`Failed to initialize a JsRuntime:
  No such file or directory (os error 2)`，阻断级）。根因：deno_core 0.411 的
  `include_js_files!` 系列宏一律以 `mode=loaded` 产出
  `ExtensionFileSource::loaded_during_snapshot(spec, concat!(env!("CARGO_MANIFEST_DIR"), ...))`
  ——把**构建机绝对路径**烧进二进制；本仓未启用 startup snapshot，运行期按该路径读盘，
  非构建机上必然 ENOENT。受影响的不止自身 `src/bridge/bootstrap.js` 与
  `oj/src/test_ext/test_bootstrap.js`，还有 deno_web / deno_fetch / deno_net /
  deno_websocket / deno_webidl 五扩展共 40 个以绝对路径声明的 JS。
- 修复：新增 `build.rs`（版本取自 `Cargo.lock`，源码目录取自 registry 或 `vendor/`）
  把上述 deno_* 扩展 JS 构建期 `include_str!` 进二进制，运行期由
  `bridge::patch_fs_loaded_sources` 覆写为 `Computed` 源；自身两个 bootstrap 经
  `ascii_str_include!` 直嵌（去掉 `esm = [dir ...]` 声明）。所有 JsRuntime 入口
  （HTTP 池 / 任务 / `oj test`）统一经 `bridge_ext_init` / `oj_test_ext_init` 取扩展。
- 回归护栏：`ws_client_extensions()` 打补丁后不得再有 `!is_runtime_loadable()` 的源；
  该用例对构建机路径零依赖，任意机器可跑（不依赖源文件是否存在）。

**工具 / CI**
- 发布门禁 `cargo xtask smoke --bin <oj>`：把两个 bootstrap 与 deno_* 依赖源码目录
  临时改名后跑最小 `oj build`，任何残留的构建机路径依赖即失败（守护在返回/panic 时
  无条件还原，并先恢复上次中断残留）。`scripts/deploy.sh` / `scripts/deploy.bat`
  打包前串联，`release.yml` 三平台显式执行同一命令。

## v0.1.11（2026-09-10）

**特性（breaking）**
- server 准入门三态（无静默默认）：api（`--api-path`）与静态站点（`server.app_path` /
  `--app-path`）至少显式指定其一，皆未指定退出并提醒；皆指定则两者都必须存在，任一
  缺失退出；仅指定其一 → 只启用对应功能（api 缺席 = 纯静态模式，监听行标 `static-only`）。
  server 不再自动搜索 src/dist 兜底（test/migrate 保留搜索并对缺失目录就地报错）。
- CLI 路径语义：`--app-path` / `--api-path` 相对 **CWD** 解析（config 内
  `server.app_path` 仍相对 config 目录）。
- config `server.base` → `server.api_prefix`（serde alias 兼容旧键，两键并存报
  duplicate field 防漂移）；新增 `server.app_prefix`（默认 `/` = 全路径兜底）：非 `/`
  时仅前缀下 GET/HEAD 落静态（前缀剥除解析，前缀根 → index.html），前缀外 404，
  API 路由永远优先。

**实现**
- console 关闭时启动失败的最终退出原因仍直写终端：logging tee 保存原始 stderr fd
  副本（`echo_terminal`），main 错误出口补写（console 开启时不补，避免重复）。
- dev 缺省服务目录自 config 同级逐级向上搜索（每层 src 优先、dist 次之），不再相对
  CWD 选址；xtask 可执行产物拷贝改 tmp+rename——就地覆盖触发 macOS vnode 签名
  缓存毒化（execve 一律 SIGKILL）。

**修复**
- e2e 不再依赖仓库内 sample/dist（停止跟踪后 CI 新克隆静态根 fail-fast）。
- ws 闸门用例稳定性：WS_LIVE 归零/平静基线后再断言，消除 straggler 槽位抖动。
- vitepress 移动端根路径解析错位（ROOT 多上一级致 sync 失源）。

**文档**
- sample 运行入口统一为编译产物 `bin/oj`（docs / README / sample/README / CLAUDE.md
  弃用 `cargo run`，README 新增 bin/oj 专节）；devkit 手册与 skill 同步（准入门 /
  app_prefix / api_prefix / echo_terminal），vitepress 收录 sample 模块专题。

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
