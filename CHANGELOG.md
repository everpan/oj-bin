# CHANGELOG

以 `oj/Cargo.toml` 的 version 递增提交作为版本分界（该提交即本版本的发布点），fix 类改动在每个版本内单列一组。

**版本分界提交的固定动作**（发版自查）：
1. 递增 `oj/Cargo.toml` 的 version（`Cargo.lock` 中该包条目同步）；
2. 本节标题由「下一版（未发布）」改为具体版本号（未打标签时注明）；
3. **同步更新对外文档 `docs/devkit/` 四件**——`api-manual.md` / `scenarios.md` / `SKILL.md` /
   `README.md` 必须与该版**用户可见变更**逐条对齐（新 API 与报错文案进 `api-manual.md` 的对应
   章节**及错误/限制表**，高频陷阱进 `SKILL.md` 陷阱速查，可照抄场景进 `scenarios.md`）；
4. `cargo xtask build` 归置 `bin/devkit/`（`cargo test --release -p xtask` 的 devkit 契约用例
   校验产物与源文件一致）；
5. 发布点打标签（annotated：`git tag -a vX.Y.Z -m "<version>: <一句话摘要>"`）并
   `git push origin vX.Y.Z`。**若暂不打标签，必须在本节标题或引言里写明「未打标签」**——
   文档的标签状态要与实际的 `git tag` 一致，不能一处说未打、另一处已存在。

详见 `docs/devkit/README.md`「版本同步要求」。

## v0.1.104

**U-18 / REQ-03 任务式导出（批次 3；需求源 `oj-dbx/docs/upstream`，设计 §3）**：

- **`db.export` / `db.exportStatus` / `db.exportCancel` 三件**：发起即返 `{taskId}`
  （同步开游标 + 登记，查询下推异步——大结果集 200ms 内回执）；新的 Rust 自驱消费任务
  drain 游标，csv（BOM/引号转义/NULL，行尾恒定 `\r\n`）/ jsonl（行 JSON）逐块
  `put_stream_chunk` 落 blob；泵每 256 行 yield 节流，防 current_thread 运行时饿死
  JS isolate。`exportStatus` = `{taskId, state: running|done|failed|cancelled, rows,
  bytes, truncated, truncatedReason, startedAtMs, finishedAtMs, error, blobKey}`。
- **双轨闸**：`maxBytes` 超限 → **failed**（错误，不静默截断，半成品 put_stream_abort
  清——防 s3 orphan parts）；`max_rows` 策略截断 → **done + `truncated:true` +
  `truncatedReason:"max_rows"`**（策略可见化，与查询 X-OJ-Row-Limit 同哲学）。
- **配额**：全局并发 4 / 每主体 2（`db_export:` 段可调，装配期 validate fail-loud
  点名键），错误文案区分「global / per-subject 满」；`put_stream` 回落路径（插件缺
  流式）整段缓冲硬顶 128 MiB（< max_bytes 校验）。导出与 db.query 同门过 db_access
  判权；不吃语句超时（长任务语义，成文）。
- **产物与回收（零 ABI）**：key = `export/<taskId>.<ext>`（随机不可预测）；TTL 24h
  自 `put_stream_open` 起算（崩溃半成品可扫），宿主 tokio interval 清扫器按台账年龄
  删（live 任务 key 豁免）；`fileName` 存 sidecar，local 首方下载带
  Content-Disposition 原名（s3 腿尽力——无此元数据，成文差异）。
- **安全语义**：taskId 内嵌 boot tag——重启后旧 id 一律 **not found**（不 failed，
  防误读为 SQL 错；跨重启恢复为有意裁剪）；他人 id 不可见（bearer 语义，日志/错误
  只出前 8 字符）；发起/终态落审计事件（主体/tenant/alias/rows/bytes/终态，SQL 全文
  不入审计）。
- 新模块 `src/bridge/db_export.rs`（ExportRegistry 壳照 ExecRegistry：状态机 + 归属 +
  配额 + TTL 台账）+ blob 轴两个默认 no-op 元数据方法（`filename` /
  `put_stream_filename` / `set_filename`，local 覆盖，s3 天然回落）。

## v0.1.103

**上游 REQ-05 处置（U-18 批次 2；需求源 `oj-dbx/docs/upstream`）——慢查询护栏与结果集大小闸 GA**：

- **语句级超时**：`db.access` 新增 `statement_timeout_ms`（0/超 1h 拒，点名键）；裸 SQL
  调用侧第三参 `{timeoutMs}` 只能**收紧**配置值（放宽 → `OJ_DB_CAPACITY` 点名
  `statement_timeout_ms`，`data={configured_ms,requested_ms,capability}`）。宿主侧
  `tokio::time::timeout` 竞速，零 ABI 变更。覆盖 `db.query` / `db.exec` / `db.stream` /
  `db.table()` 构造器全路径；超时 → 结构化 `OJ_DB_TIMEOUT`（`retryable`、`sessionDisposition:
  close`、`data={alias,timeout_ms,cancelled}`）。**事务面**：仅 `db.session` 附挂会话超时
  的语句竞速（到期 `kill_attached`）；plain `db.tx` 不叠语句超时（连接状态不可知，
  文档成文）。
- **`max_result_bytes` + `on_exceed: fail|truncate`**：行 **JSON 编码字节**计量
  （严于线上字节、确定性可测）。fail（缺省）→ 结构化 `OJ_DB_CAPACITY`；truncate →
  `X-OJ-Result-Truncated: counted` 头 + `db.stream` 的 `it.info()` =
  `{rows, bytes, truncated}` 显式信令（close 前快照）。SqlxAccessor 边拉边计真早停；
  插件轴缺省事后计量。
- **`data.cancelled` 诚实口径**：sqlite 内置 = `true`（真早停）；网络型方言 = `false`
  （内置 sqlx 的 pg/mysql DSN 与插件轴同口径：宿主弃 future ≠ 服务端取消，真取消 =
  ABI 13 车）。pg/mysql 真库 env-gated 用例经 Apple container 亲跑双腿全绿。
- **架构边界成文**（api-manual §13 + SKILL 陷阱）：sqlite 同步步进饿死
  current_thread 定时器，deadline 只在让步点触发；网络型后端（mysql/pg 插件轴，
  I/O 驱动）准点。验收矩阵 env-gated（`OJ_TEST_PG` / `OJ_TEST_MYSQL`），本地 skip
  诚实登记。
- 新增 `db_quota` 纯原语模块（Quota / effective_timeout_ms / ByteMeter /
  race_statement / 结构化错误工厂），供批次 3（task-based export）复用。
- **真库跑出的存量修复**：sqlx Any 行解码补 `f32` 臂——pg `REAL` / mysql `FLOAT`
  （float4）在 sqlx 里是独立于 f64 的 TypeInfo，原探测链探不到即报
  `unsupported sqlite column type: REAL`（env-gated 真库腿首次亲跑暴露）。

## v0.1.102

**上游 REQ-01 + REQ-02 处置（U-18 批次 1；需求源 `oj-dbx/docs/upstream`）**：

- **route_timeouts 装配期校验（fail-fast）**：pattern 匹配不到任何已注册路由 → 拒启动并点名
  （保留路径 /health、/plugins 与 WS 挂载不经此机制；pattern 含 `{param}` 段 → 拒，请用
  `*`/`**`）；每次装配（含热更）生效。**粒度成文**：看门狗 25ms 轮询，短于 ~25ms 的 timeout
  不保证精确。**边界成文**：管得住 JS 忙循环，已下推的慢语句跑完才 408——慢 SQL 护栏 =
  语句级真取消（REQ-05，下一批 GA 门禁）。新增 `RouteTable::patterns()` 访问器 +
  `validate_route_timeouts` 纯函数 + 三形态 e2e（408）+ 真装配拒启集成测试。
- **类型面补齐（REQ-02）**：`db.access`（DBAccessApi：grant/revoke）、`db.epoch(name)`、
  `json.stream/sse` 的 `fileName`/`disposition` 选项进 global.d.ts（能力运行时早已存在，
  类型漏声明致 TS2339）。**附带修复**：`audit`/`mgmt`/`secret` 曾声明在 `export {}` 模块
  作用域内——业务模块引用 TS2304（audit 自 v0.1.98 带病）；已移入 `declare global`。
- **db.client/access 返回真 Promise（REQ-06）**：`list` 运行期曾回裸数组（`.catch is not a
  function`）；`list/register/reRegister/delete/test` 与 `access.grant/revoke` 统一经
  dbSessionCall 包装为真 Promise（instanceof Promise、.catch/.finally 可链；其中六个此前
  已是真 Promise，此改统一形态），`has` 保持同步布尔。
- **防漂移门禁双腿**：① `sample/tests/globals.test.ts` 运行时 golden（无条件全局清单，
  业务模块形态裸引用——作用域事故第一现场即红）；② `sample/tests/type-usage.ts` tsc 用法
  测试（按文档写法逐一调用，声明缺失/漂移即红）。

**发版自查**：CHANGELOG 本节 + devkit 四件对齐（api-manual route_timeouts 行）+ xtask 归置 +
契约 10 绿 + 全门禁复绿后打标推送。

## v0.1.101

**U-10 运行期凭据轴（首批）**：`secret.seal/open` 全局对象（v0.1.101 面）。config `ENC[...]`
的同一套 X25519 + AES-256-GCM 信封开给运行期：`seal` 用部署公钥加密（公钥装配期加载：
`secrets.public_key_path`，缺省由私钥推导——零新增配置）；`open` 需部署私钥（私钥永不出
Rust 边界；seal-only 装配报 `no private key`）；每次 `open` 落 `SECRET_OPEN` 审计
（零凭据，`audit.query({kind:"SECRET_OPEN"})` 可查）；无密钥装配 fail-closed（报
`secret axis not configured`，绝不静默当明文）。同一明文两次 seal 密文不同（随机化，
不能拿密文判等）。文档：db-guide §11.9（新）、api-manual §13、SKILL 陷阱 3 条、
scenarios 场景 52、global.d.ts `declare const secret`。

**db_client 配额调整**：`max_per_owner` 缺省 8 → 32——per-owner 按 `registered_by` 计数，
单一管理员常占满全部登记，8 条即被卡死（评审 R-03/R-06）；示例配置 `max_entries` 16 → 64。
显式配置不受影响（升级注意：依赖旧缺省 8 的部署行为收紧为 32——只会更宽松）。

**global.d.ts 整理**：全文去版本演进叙述（`（v0.1.NN）`/U-/G9/ABI 标签全数剥离，
只留核心功能契约与陷阱）+ 十段分区横幅 + 重复注释去重（ldap 下线说明三处归一、
yaml/sso/server/crypto 双写收敛、`asTenant` 错位文档归位）；1161 → 1108 行，
tsc `--strict` 校验通过。

**Windows CI 修复**：`db_exec` / `db_session` 测试助手改经全仓唯一 DSN 归一入口
（`DbBackendRegistry::connect`）——手拼 `sqlite://` 直喂 sqlx 会被 Url 解析吞掉绝对路径
（Windows 盘符成 host）→ `SQLITE_CANTOPEN (code 14)`。

## v0.1.100

**dbx Web 对齐 P1/P2 第一批**：U-11（连接级能力探测）+ U-14 批 a（catalog 内省）+
U-15 M1（管理面 connector 宿主面）。设计稿与三路评审处置：
`docs/plans/2026-10-28-u11-…` / `-u14-…` / `-u15-…`；执行跟踪 `…-p12-execution-plan.md`。
U-16 维持不立项（部署级隧道 recipe 文档已交付 `docs/devkit/deploy-tunnel-recipe.md`）。
**零 ABI 变更**（12 保持）。

**合并审查修订**（三路复审 diff 后「修复后放行」，约 20 条全处置）：`mgmt.call` 门禁前置 +
未声明 op 统一结构化 `OJ_DB_UNSUPPORTED`（弃虚构 `OJ_MGMT_UNSUPPORTED`）；register 插入
失败补 `__disconnect__`；call_error 入审计；装配 disabled+axes warn；能力矩阵纠错（**pg
有原生 sequences**）；mysql ddl `name` 空串回落；constraints 词表统一 pk/uq/ck；开放方言
`db.catalog` 命名用例；文档四件同步（场景 51 / global.d.ts / mgmt 红线）。

### feat(db_client) — U-11

- `db.client.list` / `register` / `test` 透出 **`serverVersion` / `probedAtMs`**：connect 后
  一条版本查询（mysql/pg `select version()`、sqlite `select sqlite_version()`），
  **3s 超时 + 失败降级 None**（探测是观测增强不是登记门）；开放方言跳过；静态库不透出。
  主力消费者是内省 golden 快照定位键（U-14）。

### feat(db) — U-14 批 a

- 新增 **`db.catalog(kind, arg?)`**：目录类内省，信封 `{kind, dialect, items}`（L2 三件
  listColumns/Indexes/ForeignKeys 保持裸数组不动——两代约定）。kind：`views` / `routines` /
  `sequences` / `ddl` / `comment` / `constraints` / `stats`（行数上限 1000）。
- 能力矩阵（批 a）：sqlite 仅 views/ddl；mysql 全部（sequences 为 auto_increment 模拟，
  constraints 批 a 仅 PK/UQ）；pg 全部（sequences 原生）。**unsupported 报结构化
  `OJ_DB_UNSUPPORTED`，不静默**。
- mysql `SHOW CREATE` 是零内联铁律的唯一成文例外（白名单字符集 + 反引号转义）；
  静态守卫测试扩展到全部新 SQL 常量（`::text`/`current_schema()`/无 LIKE/参数位计数）。
- `granted_only` 授权链与 L2 同源；`stats` 只做估算（无 precise 全表扫描开关）。

### feat(mgmt) — U-15 M1（宿主面）

- 新增 `mgmt:` 配置段 + `globalThis.mgmt`（register/delete/list/has/call）。**特权面**：
  `mgmt.axes` 收编的泛型轴**不进 `axis()` 公开面**（特权旁路封堵——评审阻断①）；
  register/delete/非只读 call 走 `mgmt.allow`，只读 call/list 走 `mgmt.allow_read`
  （缺省复用 allow）；两档缺省 fail-closed。
- connector 契约（零 ABI，架在泛型传输上）：保留 op `__ops__`（能力表，**readOnly 为插件
  自报、宿主不可验证**——红线成文）/ `__connect__` / `__disconnect__`；未声明 op →
  结构化 `OJ_DB_UNSUPPORTED`（`e.data.capability` = op 名）。登记探活 fail-loud（ping
  失败 = 拒绝）。
- 审计 `MGMT_AUDIT`/`MGMT_DENY` 走 `audit::emit` 单一出口，全字段零凭据；
  per-connector 并发闸（4，满即拒不排队）；`max_entries`/`max_per_owner` 容量。
- 段改动归 `restart_pending`；`oj serve` 装配校验（0 值拒启 / allow 全缺拒启）。

### 已知限制（诚实登记）

- **U-15 M2（oj-mgmt-mongo 真 connector）未交付**——M1 仅有宿主面 + 测试用 mini connector
  契约覆盖；M2/M3（真 connector + 发版文档）另期。**登记后的惰性重探（`alive:false`
  两段式后半）未实现**——探活仅登记时 fail-loud 一次（评审锚 8 后半随 M2）。
- **`__connect__` args 含明文 DSN**（宿主审计零凭据，但 connector 侧日志须自行脱敏——
  db-guide §11.8 红线已登记）。
- U-14 批 b（triggers/partitions/types + sqlite 模拟）未交付；批 c 默认不排期。
- `mgmt.readOnly` 位不可验证（插件自报）——connector 须第一方。

## v0.1.99（未打标签）

**dbx Web 对齐 · G9（服务端防护门）**：三条防护门各自独立在 op 层生效；验收判据 = 前端绕不过
（不经任何方案包 handler、直接打 op 也拦得住）。设计文档
`docs/plans/2026-10-28-dbx-web-g9-op-gates.md`。**零 ABI/插件改动**。

### feat(db)

- **只读会话位**：`db.session.begin(name?, {readOnly: true})`——`TxSessionEntry.read_only`
  begin 时定死、不可变；该会话上 Write/DDL 在 op 层一律拒。两个入口都有门：attach 路径
  （`db.rs` query_impl/exec_impl）与显式 `db.session.query/exec`（`gate_session_sql`）。
  错误用 `BridgeError`（`category=unsupported`、`session_disposition=keep`、`retryable=false`）
  ——**信封语义正确**（G9 验收点：只读拒绝不判死会话，JS 面 `e.sessionDisposition === "keep"`）。
  summary 透出 `readOnly` 位。
- **危险语句确认门**：`db_access.<alias>.dangerous_confirm_token`（serde 缺省 None；空串
  装配期拒启）。配置后危险写（无 WHERE / 多表 / 恒真，既有分类器口径）一律要求请求头
  `x-oj-confirm` **逐字匹配**（键名大小写不敏感）；放行即每条落审计 `DB_DANGEROUS_CONFIRM`
  （U-9 单一出口，`audit.query` 可检索）。门在 `enforce==Off` 早退**之前**生效（两个入口：
  裸 SQL `check_access` 与构造器路径）——显式配 token 就是高于 off 的意志。
- **token 是 config 安全位**：运行期 `db.access.grant` 与模块 override 均不注入/覆盖
  （从基线策略透传），运行期无法绕过确认门。

### 已知限制（诚实登记）

- 「连接级」落到**会话级位**：真正的连接级 read_only（sqlite `mode=ro` / mysql
  `SET SESSION TRANSACTION READ ONLY` / pg `default_transaction_read_only`）在 sqlx 池与
  插件建连参数里，动它要碰插件/ABI——由 db_access 文本级 read_only（op 层先拒）+ 本期会话位
  双层覆盖；连接级硬保证仍是 DB 账号 GRANT（与 L1 既有口径一致，登记 db-guide）。

## v0.1.98（未打标签）

**dbx Web 对齐 · U-9（安全审计归一 + 可检索存储）**：四族散点 stderr 审计出口统一为单一事件
类型 + 单一 emit 出口；事件入进程内有界环形缓冲；新增 `audit.query(filter)` 检索/导出通道。
设计文档 `docs/plans/2026-10-28-dbx-web-u9-audit.md`。**零 ABI/插件改动**。

### feat(audit)

- **统一事件类型** `{tsMs, kind, user, tenant, module, source, alias, detail}`：零凭据
  （alias / 动作 / 前 8 字符 id；无 DSN 无密码）。覆盖 `DB_CLIENT_AUDIT`（register/delete/
  reRegister）、`DB_CLIENT_DENY`（U-7 配额/预算/拨号拒绝）、`DB_ACCESS_AUDIT`（grant/revoke）、
  `DB_SESSION_*`（begin/commit/rollback/TIMEOUT/TIMEOUT_ROLLBACK_FAILED/DISCARD/CLOSE_ERR）。
- **单一出口** `audit::emit`：db_session / client_db 全部事件构造方改走此出口；会话 reaper
  （后台、无 OpState）经 `TxSessionRegistry` 持有的存储克隆照常入库。
- **进程内环形存储**（`audit:` 段，`StableState.audit`，热更存活）：`max_entries`（缺省 2000，
  超限丢最旧）+ `retention_secs`（缺省 86400，超龄自动清理）双限；0 值装配期拒启。
- **`audit.query(filter)`**（全局对象）：`kind/user/tenant/alias/sinceMs/untilMs/limit` 各维
  可选（最新在前；limit 缺省 100、硬顶 1000）——检索即导出。读门 = `audit.allow`（每调用重判）。
- **配置段** `audit:`：`enabled` 缺省 true（零行为破坏——原本就打日志，现在多了有界存储）；
  `allow` 缺省不限制（事件进程内、零凭据），enabled 且不限制 → 装配期 **warn** 提示收紧。
  段改动归 `restart_pending`。

### changed

- **审计 stderr 行格式归一**：`DB_CLIENT_AUDIT:` / `DB_CLIENT_DENY:` / `DB_ACCESS_AUDIT:` /
  `DB_SESSION_*:` 散点前缀退役，统一为 `OJ_AUDIT: kind=<原前缀名> user=… tenant=… module=…
  source=… alias=… detail="…"`。**依赖旧前缀的日志解析器需要更新**（kind 字段保留原前缀名）。

### 已知限制（诚实登记）

- **存储不跨重启**（进程退出即清零）、**无防篡改**：PRD 的持久化/留存策略/防篡改需外部 sink
  （从 `audit::emit` 单一出口接），另期。
- **`SqlEvent`（sql_trace）不在审计轴**：per-request dev 画像（全量 SQL 入库不可控）；耗时/行数/
  成败继续走 `db.sqlProfile()`。边界写进 db-guide §11.7。

## v0.1.97（未打标签）

**dbx Web 对齐 · U-7（client 轴配额与预算）**：动态数据源三道闸 + 拨号守卫。设计文档
`docs/plans/2026-10-28-dbx-web-u7-client-quota.md`。**零 ABI/插件改动**（纯 host 侧 + 配置段）。

### feat(db_client)

- **`db_client.max_per_owner`（缺省 8）**：per-owner（registered_by）登记上限。锁内重判
  （防并发超额）；`delete` 释放名额；0 或 `> max_entries` 装配期 fail-loud 拒启动。
- **`db_client.max_streams`（缺省 32）**：动态库**全局**并发流预算——`op_db_stream_open`
  对动态别名 `try_acquire_owned` 占名额（满即拒、不排队，错误点名配置键）；permit 由 pump
  task 持有，随流结束（自然 / `signal` / `db.cancel` / close 任一路）自动释放。
- **拨号守卫**：`db.client.test` / `register` / `reRegister` 的 connect 统一 5s 拨号超时；
  同主体**连续 8 次 connect 失败**（含超时/认证失败）→ 锁退 60s，期间在**拨号之前**拒绝
  （防内网端口扫描），错误带 retry 秒数；成功一次清零。阈值/时长/超时为常量（登记于设计文档）。
- **`DB_CLIENT_DENY` 结构化拒绝日志**：配额 / 预算 / 拨号拒绝统一出口
  （action/reason/owner/alias），与 `DB_CLIENT_AUDIT` 同形态（U-9 归一时的出口之一）。

### 已知限制（诚实登记）

- **查询超时与结果集字节上限未覆盖**：raw `db.query` 平台级无查询超时（非 client 轴独有）；
  行数上限按别名配 `db_access.<name>.max_rows`。字节上限需平台级方案，另期。
- 锁退按 operator 串记账（无身份同入 `"-"` 桶）；认证成功但后续查询失败不记失败
  （scan 防护只盯拨号面）。

## v0.1.96

**dbx Web 对齐 · U-5（execution-id 跨请求取消）**：新增 `db.cancel(executionId)` —— 跨请求
停止流式查询（「请求 A 发起长导出、请求 B 点停止」）。设计文档
`docs/plans/2026-10-28-dbx-web-u5-execution-cancel.md`。
**不改 ABI_VERSION、不动插件**（纯 host 侧 + JS 全局）：取消 = 宿主级登记表路由到既有
`DbStreamShared.abort`，后端真取消链原样复用（pump 停止拉取 → `StreamCursorGuard::drop` →
插件 `stream_cancel`）。

### feat(db)

- **`db.cancel(executionId)`**：同一主体（tenant+user）的任意请求可取消 `db.stream` 开出的
  流式执行。重复取消**幂等**（同返回值 `{cancelled: true, alias}`）；未知/已结束/他人/畸形
  id 一律同一条 `execution exec_… not found`（不泄露存在性）；id 是 bearer 凭据，日志/错误
  只出前 8 字符（复用 `handle_core`）。
- **执行句柄发放**：`db.stream` 开流后经 `opts.onOpen(executionId)`（回调形态）或
  `it.executionId()`（异步迭代形态）给出 `exec_<4hex boot>_<32hex>` id。`op_db_stream_open`
  返回形状从裸 `f64` 变 `{id, executionId}`（JS 包装已适配，`v.id` 语义不变）。
- **取消后的发起方观感**：下一次拉取抛 `db stream exec_… was cancelled via db.cancel
  (cursor closed; rows already delivered are kept)` —— **报错而非静默 done**（消费方不得
  误以为数据取完）；已交付的行保留。后端效果：mysql `KILL QUERY` / pg `pg_cancel_backend`
  真取消，其余协作式（游标丢弃、连接回池）。
- **登记表** `src/bridge/db_exec.rs`（`ExecRegistry`，挂 `StableState.exec_sessions`）：
  进程级生命周期（热更存活，装配层注入同一 Arc，与 `tx_sessions` 同款）；无配置段（流本身
  已过 `db_access` 逐语句授权，取消判权只看归属主体）；登记项懒惰清扫（close 摘除 +
  open/cancel 顺带清 done 与超龄项，24h 硬顶），无后台 reaper。
- **与既有流内 abort 并存**：`AbortSignal` → `op_db_stream_abort` 路径不变。

### 已知限制（诚实登记）

- **只覆盖流式执行**：非流式 `db.query/exec` 无后端句柄，宿主侧取消只能丢 future（连接中途
  报废、服务端查询照跑）= 假取消，不做。长查询要可取消请改 `db.cancel` 可达的 `db.stream`。
  真覆盖需插件侧 one-shot 执行登记，随 ABI 演进另期。
- 流 close（自然结束/出错）后登记即摘，迟到的 cancel 与未知 id 同文案（登记单次性）。

## v0.1.95

**dbx Web 对齐 · L7（U-4 跨请求事务）**：新增 `db.session` —— 把事务从「一个请求」提升为
**进程内按 id 路由**的会话，多步表单 / 审批 / 批量导入确认不必挤进一个 handler。设计文档
`docs/plans/2026-10-28-dbx-web-l7-u4-cross-request-tx.md`（含四路评审 30 项处置表）。
**不改 ABI_VERSION**（纯 host 侧 + JS 全局），插件无需重建。

### feat(db)

- **`db.session`（跨请求事务会话）**：13 个方法 —— `begin(name?)` / `attach(id)` / `detach()` /
  `query(id, sql, params?)` / `exec(id, sql, params?)` / `commit(id)` / `rollback(id)` /
  `close(id)`（幂等收尾）/ `touch(id)`（只推空闲计时）/ `closeAll()` / `list()` / `info(id)` /
  `stats()`。`begin` 返回 `tx_<4hex boot>_<32hex>` **id**（不是句柄，走不了端口）；`attach(id)`
  之后 `db.query` / `db.exec` / `db.table()` 构造器 / `nextSeq` / `listTables` 与三个内省 op
  **全部**落到那条连接与那个事务上（复用既有 `Target::Tx` 分支，占位符改写 / 追踪 / 行数上限
  与 `db.tx` 逐项一致）。会话表挂在**进程级** `StableState`（`TxSessionRegistry`），因此
  **跨热更存活**；`db_session` 段改动归 `restart_pending`。
- **三道门**：`db_session.enabled` + `allow`（四维并集，**每个 op 重判** → 撤权即时生效）、
  既有 `db_access` 逐语句授权与风险分类、`require_identity`（默认 true，无 user/tenant 拒建）。
  **默认关**：不配这段等于能力不存在，`begin` 直接报错点名怎么开。
- **生命周期**：空闲 `idle_timeout_secs`（默认 300）/ 总寿命 `max_lifetime_secs`（默认 1800）
  到期 → reaper 先 `ROLLBACK`（5s 上限）再判死并摘表，日志 `DB_SESSION_TIMEOUT`；绑定会话的
  语句按剩余寿命 `timeout`，到点判死（连接状态不可信、不归还）；`db.client.delete` /
  `reRegister` 判死该别名全部会话（`DB_SESSION_DISCARD`）；**没有任何自动提交路径**。
- **容量与并发**：`max_sessions`（32）/ `max_per_owner`（8）双上限，先占位再建连（防并发超额）；
  满了**拒绝新会话，绝不驱逐他人会话**；同一 id 的并发 op 由 `tokio::sync::Mutex` 串行排队。
- **观测**：单一 `emit()` 出口（`DB_SESSION_TIMEOUT` / `DB_SESSION_TIMEOUT_ROLLBACK_FAILED` /
  `DB_SESSION_DISCARD` / `DB_SESSION_CLOSE_ERR`，带 tenant/user/alias/id 前缀/ops/age_ms），
  `stats()` 给 `active/maxSessions/timedOut/discarded/rejected/boot`。
- **共用件** `src/bridge/handle_core.rs`：`Owner` / `new_id` / `id_is_wellformed` / `id_prefix` /
  `expires_in` / `expired_reason` —— U-5（execution-id 取消）可直接复用，不建泛型表。

### fix(db)

- **sqlite 错误文案被脱敏成 `"***"`**（自 U-8 起）：`accessor_sqlx.rs` 对**每条**错误文本调
  `secret::redact`，而 `redact` 对「不含 `://` 的串」一律返回 `***` —— 所有 sqlite 报错退化成
  `***`（跨请求会话的用例因此全红才暴露）。现改为 `redact_msg`：**只有文本里真的出现凭据 URL**
  时才替换凭据段，其余原样保留。

### docs

- `db-guide.md` 新增 **§4.5 跨请求事务**（三步向导、最小配置、装配期 fail-loud、13 行方法表、
  六条硬规则、生命周期、sqlite 1 连接现实）；`api-manual.md` §6 db 指针段与 §11 指针、§13 限制表
  新增 3 行、排障表新增 7 行；`scenarios.md` 新增**场景 49**（三步向导 + 常见坑）；
  `SKILL.md` 陷阱速查新增 6 行；`devkit/README.md` 新增 v0.1.95 段并同步文件职责表；
  `docs/api/06-db.md` 增方法行 + 「跨请求事务」小节；`docs/plugins/plugin-development.md`
  新增 **§15.7**（事务可能跨请求存活：`TxSession: Send` / 不捕获请求级身份 / 收尾可能永不到来 /
  不自行发事务控制语句 / 语句要能被打断）。

### 装配期 fail-loud（拒启）

`enabled: true` 但 `allow` 四维全空（拒绝开放无授权名额）；`max_sessions` / `max_per_owner` 为 0；
`max_per_owner > max_sessions`；`idle_timeout_secs` 为 0；`max_lifetime_secs < idle_timeout_secs`。
sqlite 别名上启用 → 打 `warn:`（池只有 1 条连接，会话会独占该别名）。

### 待做（诚实登记）

- 语句**不可中断**：寿命到达只判死会话、不声称能中止后端正在跑的语句（U-5 execution-id 取消
  仍未实现）；reaper 对正在持锁的会话只跳过（`try_lock`），靠语句侧的硬截止兜底。

### 测试

`src/bridge/db_session.rs` 15 用例（临时文件库跑 `Bridge::run`，一 run = 一请求）+
`src/bridge/handle_core.rs` 5 用例：跨请求可见性（只有 commit 才对外可见）、rollback + tombstone、
请求边界不回收、默认关闭、`allow` 逐 op 重判、`require_identity`、未知/他人/畸形 id **同一文案**
（断言完整 id 不出现在错误里）、容量上限、事务控制与会话状态语句 7 条全拒、与 `db.tx` 双向互斥、
跨别名拒绝、`discard_alias`、空闲过期 + 回滚、`close` 幂等 + `stats`、`touch` 报剩余寿命。

## v0.1.94（未打标签）

> 与 v0.1.95 **同批提交**：两期改动在同一工作树里，且文档文件（api-manual / db-guide /
> scenarios / SKILL）两期的编辑交织不可切分，故未单独切出 v0.1.94 发布点；
> `oj/Cargo.toml` 直接记 0.1.95。版本分界以 0.1.95 那次提交为准。

**dbx Web 对齐 · L6（U-6 插件侧方言解码）**：`oj-db-mysql` / `oj-db-postgres` 补齐读侧受限类型，
并修掉一个**静默错值**与一处**同列两结论**的分叉。设计文档
`docs/plans/2026-10-28-dbx-web-l6-u6-dialect-decode.md`（含四路评审处置表）。**不改 ABI_VERSION**
（纯插件内部 + 文档；`oj-plugin-ffi` 未动），但**两个插件须与本版同批重建**（各自 0.1.2 → 0.1.3）。

### feat(db)

- **U-6 mysql 读侧补齐**（`oj-db-mysql` 0.1.3）：`DECIMAL`/`NEWDECIMAL` → **十进制字符串**
  （不经 f64，保精度）；`DATE`/`TIME`/`DATETIME` → ISO 8601（`TIMESTAMP` 是 UTC 语义 → 带 `Z`）；
  `YEAR` → number；`JSON` → **JSON 文本**（不自动 `JSON.parse`）；`BIT(n)` → **位串**
  （`b'10101'` → `"10101"`）。分派点在既有六条探测链**之后**，不改任何既有列的判定顺序。
  仍未覆盖的（`GEOMETRY` 家族、`SET` 等）继续响亮报错，文案加 `ST_AsText(x)` / `cast(x as char)`
  可执行提示与 `docs/devkit/db-guide.md §10.6` 锚点。
- **U-6 postgres 读侧补齐**（`oj-db-postgres` 0.1.3）：`numeric` → **十进制字符串**；`date`/`time`/
  `timestamp`/`timestamptz` → ISO 8601（后者带 `Z`）；`json`/`jsonb` → JSON 文本；`uuid` → 规范文本；
  **一维数组** → JSON 数组（白名单 19 个元素类型，元素 `NULL` → `null`，用 `Vec<Option<T>>` 解码）；
  `float4` → number（4 字节，`1.1::real` 读回 `1.100000023841858`，不做舍入粉饰）。
  仍未覆盖的（`timetz`、`bit`/`varbit`、范围、几何、复合、多维数组）响亮报错。
- **`oj-db-postgres` 池 typed 化**：`query`/`exec`/`tx_*` 由 `Pool<Any>` 改为 `Pool<Postgres>`，
  `Tx` 改为 `TxInner` 双路枚举（与 `oj-db-mysql` 的 `Pooled` 同构）；`sqlx` 增 `chrono`/
  `bigdecimal`/`uuid` feature。**为什么必须改**：`sqlx::Any` 只实现 `bool/i16/i32/i64/f32/f64/
  String/Vec<u8>` 的解码（`sqlx-core/src/any/types/`），上面这些类型在 Any 上取不出——只修
  `column_json_pg` 会得到「`db.stream` 能读、`db.query` 报错」的**同列两结论**分叉。

### fix(db)

- **MySQL `BIT(n>1)` 静默错值**（自 typed 化起长期存在）：`bool::compatible` 无条件包含
  `ColumnType::Bit`、UNSIGNED 的 `u64::compatible` 也含 `Bit`/`Year`，探测链把 `BIT(5)` 读成
  `true` 或数字。现按**类型名**在探测链之前短路（`YEAR` 同理：`int.rs` 没有 Year 分支）。
  真库用例 `real_mysql_dialect_types_decode_readable` 钉死位串输出。
- **PG `numeric` 标度假精度**：sqlx 的 `TryFrom<&PgNumeric> for BigDecimal` 丢弃服务端 dscale，
  把标度硬算成 4 的倍数（`sqlx-postgres/src/types/bigdecimal.rs`）——`1.5` 会读成 `1.5000`。
  统一 `normalized()` 去尾零（值精确、不动有效位）。MySQL 的 `DECIMAL` 由协议文本解析，标度
  是服务端给的，**原样保留**（`12.30` 仍是 `"12.30"`）——两方言的口径差异已写进文档。
- **错误串标识符消毒**：两个插件的列名进错误文案前过滤控制字符并截断到 64 字符
  （日志注入面），补离线用例钉住（含多字节列名不得误加省略号）。

### docs

- `db-guide.md` §10.6 重写「读侧值契约」与「三方言读侧边界」两表（新增 12 行类型 + 「只读」
  红线 + PG 去尾零成因）；`api-manual.md` §6 db 读侧边界段与 §13 限制表同步；`SKILL.md`
  陷阱速查换 3 行（GEOMETRY 仍报错 / PG 去尾零 / `42804` 写回须 `::type`）；`scenarios.md`
  新增**场景 48**（读写范式 + `JSON.parse` + `select *` 陷阱）；`plugin-development.md` §15.6
  验收清单新增「扩展类型三路径逐值一致」与「`sqlx::Any` 解不出就池 typed 化」两条；
  `numeric-limits.md` §4 第 10 项扩写 + 新增第 11 项（PG 边界）。

### 测试与挂账

- mysql：`real_mysql_dialect_types_decode_readable`（宽表 `query` + `stream` 双路径）、
  `real_mysql_unsupported_column_types_error_loudly`（收窄到 GEOMETRY，钉 `ST_AsText` 提示）、
  `given_hostile_column_name_when_safe_ident_then_control_chars_stripped`。本机已用
  macOS `container` 起 `mysql:8.4` 实跑 **17/17 全绿**。
- pg：`real_pg_dialect_types_decode_readable`（`query` + `stream` + `tx_query` 三路径逐值一致）、
  `real_postgres_unsupported_column_types_error_loudly`（改钉 `point`）、
  `given_padded_scale_when_decimal_text_then_trailing_zeros_stripped`、
  `given_hostile_column_name_when_safe_ident_...`。本机 `postgres:16-alpine` 实跑 **19/19 全绿**。
- **CI 挂账**：上述真库用例 env-gated（`OJ_TEST_MYSQL` / `OJ_TEST_PG`），CI 无 service →
  未设环境变量时**静默 skip（skip-as-pass）**，故 CI 绿不代表这两种方言的行形状被覆盖。

## v0.1.93

**dbx Web 对齐 · L5（P0 主旅程）**：U-8（sqlite 错误信封化）、U-6（读侧解码扩展）、U-1（动态授权运行期生效 + 热更分类）、U-3（跨 isolate 失效广播）。设计文档 `docs/plans/2026-10-28-dbx-web-l5-p0-main-journey.md`（含四路评审 16 项处置）。**不改 ABI_VERSION**（纯 host 侧 + JS 全局）。

### feat(db)

- **U-8 sqlite 错误信封化**：`accessor_sqlx.rs` 8 处纯文本错误 + `stream_query` 内联 `e.to_string()` 全部走 `sqlite_err(e, ctx)`，按 sqlx 变体映射到既有 `DbErrorCategory` 八值（`Syntax`/`Constraint`/`Timeout`/`Connection`/`Capacity`/`Unsupported`/`Cancelled`/`Internal`），携带 `retryable`/`sessionDisposition`；message 经 `secret::redact` 脱敏，**绝不回显 DSN/凭据**。sqlite 专用纯文本回退删除。
- **U-6 读侧解码扩展**：`column_json` 原 `unwrap_or(Null)` 静默丢值改为**显式报错**（含列类型名）；未知/不支持类型不再静默回退。sqlite 路径规范化（仅吃 sqlite 行，Any 驱动经类型名反射归一，不引入未实现 `Decode<Any>` 的强类型）。
- **U-1 动态授权运行期生效**：新增 `db.access.grant(alias, allow, opts?)` / `db.access.revoke(alias)`——
  把「放行维」(角色/主体四维并集) 注入运行表，无需重启即对被授权主体放行（默认 read-only）；
  `revoke` 仅回落 config（非删 config）、拒绝撤销 config 兜底项（显式报错）。与 `db.client` 同特权门
  （`db_client.allow` 四维并集、fail-closed），安全敏感度更高 → 必发 `DB_ACCESS_AUDIT`。
  合并语义：运行时项只动 `allow`，不放开 config 的 `read_only`/`max_rows`/DDL/危险写（防提权）；
  未声明动态别名仍 fail-closed。
- **U-1 热更分类**：`db_access` / `db_client` 纳入 config diff 已知键并标 `restart_pending`（改这两段须重启），消除旧授权静默盲区（旧版误标 `applied`）。

### feat(db_client)

- **U-3 跨 isolate 失效（epoch）**：`ClientDbRegistry` 增逐别名世代号 `epoch`，register/re_register/remove 各 +1；
  新增 `db.epoch(name)`（host 权威）与 `op_db_epoch`。`DB(name)` 缓存**每次调用**校验 epoch（修复 cache-hit 短路），
  delete/reRegister 后 JS 侧及时失效。正确性不依赖 JS 持有的 epoch（查询 op 按名从 host registry 重解析 accessor），
  恶意 handler 覆写缓存 epoch 也无法复用已删连接。

### fix(db)

- `known_top_level_keys` 补 `db_access` / `db_client`（消 `unconsumed config sections` 噪声 + 使其进入 diff 分类）。
- **`enforce: deny` 语义校正（v0.1.91 回归修复）**：L3 在 `granted_only` / `check_access_intent` 里加的
  Deny 短路会**忽略 `allow` 硬拦所有人**——`db_access: {defaults: {enforce: deny, allow: {roles: [admin]}}}`
  连 admin 都拿 `db_not_granted`，等于白名单完全失效。现恢复 L1 语义：`deny` = **拒绝违规**
  （命中 `allow` 放行；未写 `allow` 时主体全放行、`read_only`/DDL/危险写/`unknown` 策略位照常执行）。
  L3 的「动态库默认拒所有人」改用「声明且为空」的 `allow` 表达（空表 `allow` 本义是「不限制」，
  无法表达拒绝）。顺带修复 `db_policy::behavior` 6 个长期红的用例（`role_grant_hit_and_miss` /
  `as_tenant_voids_the_tenant_dimension` / `task_bridge_reports_source_task` /
  `introspect_granted_only_blocks_unauthorized` / `deny_read_only_blocks_write` /
  `as_system_does_not_exempt_access_control`）。
- **`db.access.grant` 改为真授权**：运行期项 `enforce` 由 `warn`（只记日志不拒）改为 `deny`——
  否则「授权」对未授权主体毫无约束力。静态别名上 grant 时若 config 未限制主体则**成为白名单**
  并把 `off` 抬到 `deny`（`off` 档没有授权判定，grant 会是空操作）；config 已有白名单则按
  **逐维并集**追加（原先整体替换会挤掉 config 已授权的角色）。
- **`db.allowed()` 与运行期授权同源**：改用含 grant 表的归一策略，并让运行期授权的动态别名进入
  连接树（此前只看 config，运行期授权的库在 `db.allowed()` 里仍是「假入口」）。

## v0.1.92

**动态数据源登记的实例化能力（L4，client 轴）**：把 L3 的 `db.client.register` / `reRegister` / `delete` /
`list` / `has` / `test` 正式确立为 **DB 实例的实例化 / 生命周期 API**——`register(name, dsn)` 即
`db = new DB(name, dsn)` 构造实例、`delete` 销毁 + 驱逐缓存、`reRegister` 替换底层连接。

**范围收窄（实施前用户拍板）**：四路评审初拟的「`default` 库 `oj_client_db_registry` 表写穿 + 回放」方案
**不落地**——框架**只提供实例化能力，落盘持久化由应用侧负责**。故本版**不**引入 `db_client.persistence`
配置、`ClientDbStore`、`client_store` 等任何框架级落盘代码。设计文档 `docs/plans/2026-10-28-db-client-axis-l4.md`
记录评审与最终拍板。

**耐久登记的应用侧配方**：应用在自己表里存 `(name, ENC[...] 密文 dsn)`（`secret.seal` 密封，绝不存明文），
启动初始化 handler 里逐条 `db.client.register` 重建（单条不可达 → 跳过 + warn，不 brick 启动）。存储介质 /
加密 / 备份全是应用关切；多实例各自回放、都指向同一真实 DB 即一致（DB 实例本就是 per-process 连接池）。

### feat(db_client)

- 明确 `db.client.register` 为实例化原语（L4 建模视角）：构造 DB 实例实体、`DB(name)` 立即可用。
- 文档契约：api-manual §6 `db.client` 标注「框架不落盘，耐久由应用负责」+ §13 限制表、scenarios 场景 42
  新增应用侧持久化配方、SKILL 陷阱速查、`docs/db-guide.md` §11.6 扩写、README 版本同步段。

### fix(db_client)

- 无框架级代码改动（L4 为范围收窄 + 文档）；既有 L3 实例化 / 生命周期 / 访问控制 / 脱敏红线全部保留。

## v0.1.91

**动态数据源登记（L3，client 轴）**：运行期把任意 DSN（复用装配期同集的 `DbBackendRegistry`——
内置 sqlite/memory，mysql/postgres 经插件）登记为命名数据源。登记后别名直接进入 `DB(name)` 解析路径，
复用 L1 访问控制（v0.1.89）+ L2 结构内省（v0.1.90）+ 全部 query/exec/stream，**不另起平行 API**。

新增配置段 `db_client:`（`enabled` 默认 false，fail-closed）；装配期 fail-loud：
`enabled=true` 且 `allow` 四维全缺（无限制）直接拒启动（防「开了忘配」全开，SSRF / 内网横向移动面）。
新增六 op：`op_db_client_register` / `test` / `delete` / `re_register` / `list` / `has`，经
`db.client.{register,test,delete,reRegister,list,has}` 暴露。

安全红线（四路评审 [安全·阻断] 裁决）：
- 未声明 `db_access.<name>` 的动态库**默认 deny**（fail-closed）——`policy_for` 对未声明动态别名清空
  `allow` + `enforce=Deny`，杜绝「登记即全员可查内网库」。
- 登记门 `asSystem()` **不豁免**（`identity` 不读 `ReqState.system`）。
- 连接错误与 `list` 输出均经 `secret::redact` 脱敏，绝不回显 `user:pass@`。
- 审计日志 `DB_CLIENT_AUDIT`（register/delete/reRegister）。

设计文档见 `docs/plans/2026-10-27-db-access-control-core.md` §「动态连接登记（client 轴）」与
`docs/plans/2026-10-28-db-client-axis-l3.md` §10（四路评审裁决）。

### feat(db_client)

- 运行期动态数据源登记（client 轴）：register / test / delete / re_register / list / has 六 op。
- `db_client:` 配置段：`enabled` / `max_entries` / `allow`（四维并集，缺省 fail-loud）。
- 未声明 `db_access.<name>` 的动态库默认 deny（fail-closed）。
- `config_dir` 注入 `DbBackendRegistry::connect` 的 sqlite 相对路径归一基准。

### fix(db_client)

- 登记门 `enabled=false` 结构化拒绝（`reason: db_client_disabled`）；非授权 caller 拒绝
  （`reason: db_client_not_authorized`）。
- 连接失败错误与 `db.client.list` 的 `dsn` 字段均脱敏。

## v0.1.90（未打标签）

**db 结构内省（L2）**：把 `db.listTables` 的「表名级」扩到列 / 索引 / 外键三级，对齐 dbx 的连接树
展开。宿主侧实现，**不动 FFI**（`ABI_VERSION` 仍为 12，无插件 bump）。SQL 由宿主生成、表名只经绑定
参数。设计文档见 `docs/plans/2026-10-28-db-introspection-l2.md`（含四路评审记录与待做登记 T8–T11）。

### feat(db_introspect)

- 新增 `src/bridge/introspect.rs`：三方言 SQL 引擎常量 + 方言选择 + 结果上限（`MAX_ROWS = 1000`）。
  投影列别名三方言统一，由 `columns` / `indexes` / `foreign_keys` 三个聚合函数消费成 JS 契约形态。
  只放 SQL 常量与方言知识（不含执行），运行期 op 与 CLI（`oj schema`）共用同一份方言知识，
  避免 CL/运维两套各自漂移（T5：`'public'` → `current_schema()` 已同步 `oj/src/schema.rs`）。
- 新增三个 op：`op_db_list_columns` / `op_db_list_indexes` / `op_db_list_foreign_keys`
  （+ JS `db.listColumns` / `listIndexes` / `listForeignKeys`，`db.tx` 回调同签名）。
- 三方言**同构返回**：`nativeType` 是方言**原始声明文本**（sqlite `type` / mysql `column_type` /
  pg `format_type`，零归一化）；`origin` 统一为 `c`(普通) / `u`(唯一) / `pk`(主键)——sqlite 的
  `create unique index` 在 `pragma_index_list.origin` 里归 `c`，已按 unique/origin 派生统一口径；
  复合外键 `columns[i]` ↔ `refColumns[i]` 按位配对；sqlite 外键 `name` 恒 `null`。
- 表名一律**绑定参数**（零标识符内联）；`%` / `_` 在表名里是字面量等值匹配，**不是 LIKE**
  通配符。表不存在 / 不可见 / 在别的 schema → 一律**空数组**（不报错不区分），先校验表名用
  `db.listTables(prefix)`。
- 表名入参校验：长度 > 128、含 NUL / 控制字符、或（sqlite）含 `.`（挡 `other.secrets` 跨库内省）→
  空数组（不把畸形输入变 500）。
- 每 op 硬上限 1000 行（元数据规模天然小）。
- 与 `db.listTables` 一并过 `db_access` 的**授权维**（`granted_only`，新增于 v0.1.89 设计）：
  未授权主体连表结构都枚举不到；`db.asSystem()` **不豁免**、`warn` 档放行并落 `DB_ACCESS_DENY`、
  内省恒只读不受 `read_only` 限制（只读库也必须能看结构）。别名用 `guard::bound_db` 后的物理名。
- 未知方言 → 结构化 `OJ_DB_UNSUPPORTED`（`e.data.capability` 点名 `list_columns` /
  `list_indexes` / `list_foreign_keys`），**绝不回落 sqlite**。
- 顺带修 `op_db_list_tables` 的 PG 分支漏 `::text`（`information_schema.tables.table_name` 是
  `name` 域，sqlx Any 驱动解不了该 OID，且「空库不触发、建表后才崩」）——与 `introspect.rs` 同口径。

### 行为变更

- 会话级语句 `USE` / `SET search_path` / `set_config('search_path', …)` / `ATTACH` / `DETACH`
  由只读 / 未知 **改判为写**：池化连接上这些是会话级改写，会污染后续请求并改写内省可见范围；
  `deny` + `read_only` 站点即被挡（属既有敞口的补强，原按只读放行）。
- `db.listTables` 与三个新 op 现在受 `db_access` 授权维约束（`deny` 档未授权主体无法枚举表结构）。

### 边界（文档已明示，不是安全边界）

- 内省是**目录读**，**不是**标识符白名单：禁止把列名回填 `db.table()` 构造器或拼进裸 SQL
  （构造器只认 `SchemaRegistry`）；`information_schema` 只返回当前连接账号可见的对象，不等于
  「库的完整结构」。
- 内省只覆盖当前库 / 当前 schema（PG `current_schema()` / mysql `DATABASE()` / sqlite 单文件），
  不跨 schema；视图 / 触发器 / 数据类型字典 / CHECK 不在内省范围（待做）。
- `nativeType` 不做跨方言归一化（归一化 = 半套 DDL 还原，越界）。

### docs

- `docs/db-guide.md` 新增 §11.5.7「结构内省」+ 三方言差异对照表 + 红线。
- devkit 四件同步：`api-manual.md`（§6 三方法行 + 类型表 + `OJ_DB_UNSUPPORTED` 闭集增三项 +
  §13 限制表 3 行 + 陷阱 5 条）、`scenarios.md` 场景 47、`SKILL.md` 陷阱 5 条、
  `README.md` v0.1.90 段落；`sample/global.d.ts` 增三 interface 与三方法。
  `cargo xtask build` 归置 `bin/devkit/`（`cargo test --release -p xtask` 校验一致）。

### 待做登记（不在本版）

- T8：是否补 `db.describeTable()` 聚合 op（等产品有前端反馈）。
- T9：内省范围装配期冻结（db/schema 从 DSN 解析，需评估多 schema 部署）。
- T10：CLI `oj/src/schema.rs` 完整迁移到 `introspect.rs`（本期只做了 `'public'` → `current_schema()`）。
- T11：视图 / 数据类型字典 / 触发器 / CHECK（`db.listViews` 等）。

## v0.1.89

**db 访问控制核心（L1）**：对齐 dbx 多库能力的第一步——顶层 `db_access:` 段提供库级授权、
语句风险分类、危险写门、只读位、行数限额与决策可观测。宿主侧实现，**不动 FFI**（`ABI_VERSION`
仍为 12，无插件 bump）。默认 `enforce: off` → **不配 `db_access:` 与升级前行为完全一致**。
设计文档见 `docs/plans/2026-10-27-db-access-control-core.md`（含四路评审记录与待做登记）。

### feat(db_access)

- 新增 `src/bridge/db_policy.rs`：库级策略表 + 语句风险分类（`readonly`/`write`/`ddl`/`tx`/
  `unknown`）+ 授权判定 + 限额 + SQL 脱敏。
- 顶层 `db_access:` 段（`defaults` + 逐库覆盖条目）：`enforce`（off|warn|deny）、`read_only`、
  `allow_ddl`、`allow_dangerous`、`max_rows`、`allow.{modules,roles,tenants,sources}`。
  逐库条目**逐键继承** `defaults`（`allow` 逐维覆盖）——避免「全局设防被一次局部微调静默解除」。
- 授权四维**并集**：任一已声明维度命中即放行；未声明维度不参与限制。声明为空数组 = 无人可过。
  fail-closed：取不到身份的维度一律不命中。
- 危险写门：UPDATE/DELETE 无 WHERE / 多表 / 恒真谓词 → `db_dangerous_write`（主判据是有无
  WHERE，恒真集只用于收紧）。
- 挂载点：`op_db_query` / `op_db_exec` / `op_db_stream_open`（词法分类）与 `op_db_query_build`
  （构造器走精确意图，不经词法）。`enforce: off` 零开销早退。
- 限额：`max_rows` 对 raw `db.query` 是**取回后截断**（响应打 `X-OJ-Row-Limit`），对 `db.stream`
  是**提前终止**（真省后端拉取），构造器路径取 `min(声明 limit, max_rows)`。范围 `[1, 100000]`。
- 自省：新增 `op_db_allowed` / JS `db.allowed()`——只列**当前主体有权限**的库别名与能力位
  （数据面板的库选择器以此为准；硬编码 `db:` 段别名会列出「点了才报错」的假入口）。
- 观测：`SqlEvent` 增 `risk` / `operator`（`user.id`）；拒绝与告警落前缀 `DB_ACCESS_DENY` 的
  结构化日志，按 `(alias, reason)` 聚合计数（warn 档逐条落会被刷爆）。
- 装配期 fail-loud：未知键点名、`max_rows` 越界、`read_only` + `allow_ddl` 矛盾、非法档位
  一律启动报错（不静默取其一）。
- 分类自带**独立**词法器（保留 MySQL 可执行注释 `/*! */` 内容），`guard.rs` 一行未改——
  复用 `strip_literals` 会让 `DELETE FROM t WHERE 1=1 /*! tenant_id */` 借注释伪造租户命中。

### 行为变更（默认档下不生效）

- `db.asSystem()` **不豁免** db 访问控制（它只保留租户守卫豁免的既有语义）。升级到 `deny` 档
  前须检查存量 `asSystem` handler。
- `db.asTenant()` 自填的租户在 `tenants` 维**不作数**——该维只认服务端注入的租户（否则匿名
  handler 可自授租户借道跨租户）。

### 边界（文档已明示，不是安全边界）

- `read_only` 是**宿主侧文本级判定**，真写防线仍是 DB 账号 GRANT。
- `max_rows` 对 raw 是**取回后截断**（响应整形，非限流 / DoS 防线）。
- 本策略**只约束 JS 提交的 SQL**：平台自生成 SQL（`db.nextSeq` 序列表 DDL、`db.listTables`
  内省）与 CLI 通道（`oj migrate` / `oj schema` / `oj build`，不经 bridge）不适用。
- 分类是 best-effort 词法，不是解析器：复杂语句落 `unknown` 并按最严档拒（fail-closed）。
- `sources` 只有 `http` / `task` 两个取值（WS 帧按 `http` 计）。
- `sql_guard=deny` 下「raw SELECT 未登记表」的敞口**原样保留**，不得读成已修复。

### docs

- `docs/db-guide.md` 新增 §11.5「db 访问控制」（配置 / 授权 / 分类 / 限额 / 自省 +
  **承诺 / 不承诺**两栏表 + 升级须知）+ 报错速查 5 行。
- devkit 四件同步：`api-manual.md`（§6 `db.allowed()`、§10 `db_access` 段、§13 限制表 7 行 +
  陷阱 4 条）、`scenarios.md` 场景 46、`SKILL.md` 陷阱 7 条、`README.md` v0.1.89 段落；
  `sample/global.d.ts` 增 `DbAllowedEntry` 与 `DBInstance.allowed()`。`cargo xtask build` 归置
  `bin/devkit/`（`cargo test --release -p xtask` 的 devkit 契约用例校验产物与源一致）。

### 待做登记（不在本版）

- T1 `guard.rs` 的 `first_kw` 同样被 `WITH ... DELETE FROM` 打穿（既有租户守卫语义变更，需单独评审）。
- T2 档位类型归一（`ownership_deny` / `SqlGuard` / `Enforce` 三套并存）。
- T3 sqlparser 引入（触发条件：需要判定「谓词是否覆盖到正确列」时）。

## v0.1.88

数据库方言能力模型（M1）：ABI 12（describe 工厂级能力槽 + 结构化错误信封）、两层方言模型
（枚举保留为 known 子集 + 边界描述符 + 六门控点 + 删除 Sqlite 回落）、default-throw cancel、
mysql/pg/sqlite 迁移、占位符触发条件切换（逐字节等价）、sql_trace 扩展、清单锁装配 + 启动能力位一览。

### feat(db)

- ABI 12：DataAccessorVtable 增工厂级 `describe()` 槽，返回 `{dialect, capabilities}` JSON；
  `ABI_VERSION` 11 → 12（严格相等门禁），宿主 + 插件同产物重编发布。
  `oj-plugin-ffi` 0.1.2 → 0.1.3、`oj-db-mysql` 0.1.1 → 0.1.2、`oj-db-postgres` 0.1.0 → 0.1.2。
- 方言契约从闭集枚举演进为「开放方言名 + 能力位」：新增方言接入零宿主逻辑分支。
- 结构化错误信封（category/retryable/session_disposition/message/data）跨 FFI，宿主单点
  `parse_raw`；未知方言六门控点统一返回 `OJ_DB_UNSUPPORTED`（builder/exec_route/list_tables/
  next_seq/search.*/migrate/seed），无 Sqlite 回落。
- **裸 SQL 路径对开放方言全量可用**：`db.query` / `db.exec` / `db.stream` / `db.tx` 不再受方言
  门控（门控只覆盖宿主替你生成 SQL 的六处）。新方言接入的可用面因此非零。
- JS 错误面：既有方言错误消息文本逐字节冻结；新错误条件以稳定 `code`/`data` 暴露
  （`OJ_DB_UNSUPPORTED` + `data.capability` + `data.dialect`）。
  **冻结范围界定（唯一例外，设计 §5.1）**：db 轴宿主侧的 `ffi <op>: ` 前缀被移除——ABI 11 及
  以前该前缀与插件自身文案叠加成 `ffi db query: db query: <sqlx 原文>` 双前缀，现为
  `db query: <sqlx 原文>`（message 主体逐字不变）。若 handler 曾匹配 `^ffi db ` 请改为匹配
  主体前缀；快照测试 `db_error_text_snapshot_is_frozen` 钉死该文本。
- JS 错误面（外显补齐）：除 `e.code` / `e.data` 外，db 错误现另带 `e.retryable`（布尔）与
  `e.sessionDisposition`（`keep`/`reset`/`close`）——重试决策不必解析 `data`。`db.tx` 内、
  构造器路径、`db.stream` 的错误同样携带（此前 builder / tx / stream 的异步入口会退化成
  纯文案，属实现缺口，现统一）。
- 插件与诊断面：`plugins()` / `ojInfo()` / `GET {base}/plugins` 增 `caps` 列
  （`{dialect, capabilities[]}`，与插件 `describe()` 同词汇；非 db 轴插件为 `null`）；
  `oj info` 文本输出补 db 插件的能力行（与启动横幅同口径）。
- 插件清单锁：`cargo xtask build` 末尾写 `bin/plugins/<triple>/plugins.lock.json`
  （`[{name, abi, fingerprint}]`）；扫描装配时锁外库文件**拒载并点名**（增量覆盖残留旧插件的
  典型症状），锁内条目缺失或 abi/fingerprint 与实物不符则 stderr 告警（整体拷贝不完整的信号）。

（逐任务明细见各 commit；本段为 M1 总览，任务级 fix/feat 随提交补充。）

### fix(db) — postgres 流式整数类型

- **`db.stream`（typed 流式）在 postgres 上读 `int4`/`int2`（`integer`/`smallint`）列会硬报错**
  「column 'x' has type 'INT4' which this plugin does not decode yet」。根因：sqlx 的
  `Type<Postgres>` 是**严格等值**匹配（`i64::compatible` 只认 `INT8`），而 `column_json_pg`
  只试了 `i64`；Any 回退路径的 `i64::compatible` 是 `is_integer()`（全整数宽度），所以
  `db.query` 一直正常，只有流式受影响。现补 `i16`/`i32` 两个分支，与查询路径逐值对齐。
  真库回归用例 `real_pg_stream_decodes_common_numeric_types`（同时钉死 `real`/FLOAT4 在**两条**
  路径上都保持「不支持 + cast 指引」，避免只补一头造成分叉）。`oj-db-postgres` 0.1.1 → 0.1.2
  （ABI 不变，可原地替换）。仍未解码的类型（`real`/`jsonb`/`numeric`/`uuid`/时间戳/数组等）
  继续响亮报错并给出 `cast` 指引。

### docs(db) / tools — 方言接入资料与 db 轴模板（阶段三的无库切片）

`oj-db-oracle` / `oj-db-dameng`（阶段二）仍卡在达梦 DPI 再分发许可与 Go triple 矩阵两道外部门禁上，
故先落地**不需要真库与外部许可**的那部分：类型映射表、conformance 清单、可拷贝的 db 轴骨架。

- `docs/db-guide.md` 新增 **§10.6 类型映射表与值契约（跨方言）**：六类 schema 类型 ×
  sqlite/mysql/pg/oracle/DM8 的 DDL 映射（后两列标注「M2 方案级 / 待真库复核」，不作生产依据）、
  读侧 JSON/JS 值契约、三内置方言读侧边界表（点名 **sqlite 仍会静默 `null`** 属历史包袱、新插件勿照抄），
  以及新方言四条硬要求——其中「禁止 `Unknown` 静默当 text」在写下时确有其事（核心侧
  `from_schema_type` 兜底把 `type: varchar` 归 `Unknown` 按文本处理，租户守卫因此在 PG 上退化成
  `operator does not exist: bigint = text`）；该缺口已由下节 `fix(schema)` 闭合，本段留作背景。
- `docs/plugins/plugin-development.md` 新增 **§15 新增数据库方言 conformance 清单**：五步接入、
  8 能力位注册表（逐能力位写明宿主消费点 + 正例 + 反例）、错误映射骨架（含 dbx 侧二手证据的达梦
  错误码 `-5669` / `-6118` / `-3325` / `-2207`，明确标注待 M2 真库复核且**不得照搬 Oracle 编号**）、
  sidecar 五条生命周期契约、发布形态与排障入口，以及 14 条可直接拷进 PR 的验收清单。
- 新增 `tools/plugin-template-db`：**db 轴 cdylib 插件骨架**（`demo` 方言，进程内，不连真库）。
  `describe()` 能力位自报 / 结构化错误信封（8 类 × 5 字段，用枚举而非字符串防 typo）/
  流式三态信封（`rows`·`done`·`error`，绝不裸 `null`）/ 句柄·事务·游标生命周期全部摆全，
  11 个自检用例逐条对应 §15.6。能力位与行为由**同一组常量**驱动：翻转 `SUPPORTS_STREAM` /
  `SUPPORTS_CANCEL` 后套件仍全绿（声明与实现不会漂移）。真实驱动只需替换 12 个 `do_*`。
  不进 workspace members（与既有泛型轴模板 `tools/plugin-template` 同款：拷贝使用的骨架）。
- 交叉引用：§10.5 能力矩阵、§10.6、§15 与模板 README 四向互指，运维侧仍以 `api-manual.md`
  「结构化数据库错误」为准。

### fix(schema) — 列类型不再静默降级成 text

- **复核结论（先纠正一处自己的文档判断）**：`schema.yaml` 这条路上未知类型**本来就被拒**——
  `SchemaFile::parse` 在声明期点名原串 + 列出支持清单（`type: varchar` 会得到
  `schema: 表 "t".tenant_id: 未知列类型 "varchar"（支持 integer/bigint/text/boolean/double/blob；
  其余类型请写 migrations/NNNN__desc.sql）`）。真正的静默降级只存在于核心侧：`ColumnType::
  from_schema_type` 的兜底分支把不认识的串归 `Unknown`，而 `Unknown` 的语义是「按 text 处理」。
- 收紧为**不可能静默**：`ColumnType::from_schema_type` 返回 `Option<Self>`（不再兜底），并新增
  `ColumnType::SCHEMA_TYPES` 作为六类词汇表的唯一来源（解析、错误文案、校验共用，杜绝两处清单
  漂移）；`SchemaFile::registry_tables()` 返回 `Result`，未识别类型**拒载并点名 表.列**——
  即使有人绕过 `parse()` 直接反序列化，也走不到「当 text」。
- 顺带删掉 `oj/src/schema.rs` 里与核心重复的 `ColType` 枚举（同一份词汇表原本有两处定义），
  DDL 生成与 `validate_tenant` 改为直接判 `ColumnType`。
- 影响面：`schema.yaml` 正常路径**文案与行为逐字不变**（错误串仍由同一份词汇表拼出）；只有
  「绕过 parse 的路径」从静默降级变为拒载。`Unknown` 语义收窄为「列类型未声明」（旧装配路径
  `table()` / 既有夹具），仍按 v0.1.23 行为处理——这条向后兼容契约有专测钉死
  （`unknown_column_type_tenant_still_binds_text`：取「纯数字租户头」做判别，`Unknown` 一旦
  被误当数值，绑定参数会从 `"7"` 变 `7`，断言立刻红）。
- 测试：`from_schema_type_maps_exactly_the_six_declared_names`（六项全命中且不落 `Unknown`）、
  `from_schema_type_never_silently_falls_back`（11 个应拒串：`varchar`/`VARCHAR(64)`/`jsonb`/
  `INTEGER`/`int`/空白变体等一律 `None`）、`registry_tables_rejects_unknown_type_even_when_parse_is_bypassed`
  （刻意用 `serde_yaml` 绕过 parse 的对抗用例）、`unknown_column_type_tenant_still_binds_text`
  （旧路径 `Unknown` 仍绑字符串，锁死兼容面）+ 真机 `oj serve` 双向验证（`varchar` 拒启并点名、
  改 `text` 后正常建表启动）。

### docs(db) / ops — `db.dialect()` 迁移范式 + 运维插件面（M3 收尾切片）

M3 的 5 个任务域至此全部落地（前 3 个见上一节的方言资料/模板；本节补余下 2 个）。
两域原本挂在「等 M2 真库」上，复核后确认**结构性内容不依赖真库**：迁移范式的方向恰恰是
「不要按方言名分派」，而运维手册里的报错文案可以**逐字取自当前已实现的代码**。

- `docs/db-guide.md` 新增 **§10.7 `db.dialect()` 的红字与迁移范式**：方言名是开放集（插件
  `describe()` 声明什么就是什么），`if (d === "mysql") A else B` 会在新方言接入后把 B **静默**
  发给它（占位符风格错、`ON DUPLICATE KEY` 发给 Oracle），报错却落在引擎侧。给出三档迁移
  （① 删掉：占位符 / 取 id / 列目录 / 取号都由宿主或平台 API 吸收；② 换问法：能力问题直接调用
  并接 `OJ_DB_UNSUPPORTED`；③ 保留：真方言特有 SQL 用**白名单对象键 + 未知方言 `throw`**），
  附反面样例与「别把 `dialect()` 缓存成模块常量」「别在 handler 里按 `caps` 分派」两条边界。
- `docs/devkit/api-manual.md`：`db.dialect()` 行与「方言边界」块补同一条红线；**scenarios 新增
  场景 45**（删分支对照表 + 可照抄的 `_shared/dialect.ts` + 验证步骤 + 五个常见坑）；
  `SKILL.md` 陷阱速查 +1 行（同口径）；`devkit/README.md` v0.1.88 段落纳入场景 45。
- `docs/ops-manual.md`：**§1.2 插件目录与清单锁**（发布物形态、三条纪律、锁与实物不一致的
  四种情形逐字表）、**§6.1 装配期日志**（7 行逐字日志 + 级别 + 含义，`host ABI_VERSION` /
  `plugin … | caps:` 为权威现场）、**§7 排障表 +8 行**（ABI mismatch、锁外文件、identity
  mismatch、符号缺失、能力串 typo、fingerprint 告警、运行期 `OJ_DB_UNSUPPORTED` 的两种成因、
  插件没装上）、**§8.1 回滚三元组**（`bin/oj` + `bin/plugins/<triple>/` + `dist/` 为什么不能
  单换、五步回滚命令、回滚后三项核对、迁移不回滚）。
- 文案一致性口径：`plugin ABI mismatch` / `plugin file not in plugins.lock.json` /
  `plugin identity mismatch` / `plugin symbol missing` / `plugin db capability error` 及两条
  `[oj-plugin]` 告警，全部逐字对齐 `src/bridge/plugin_loader.rs` 的 `Display` 实现；
  横幅格式对齐 `oj/src/serve_cmd.rs::plugin_banner_line`。**M2 落地后需回填**的只剩
  Oracle/DM 厂商错误码分布与 §10.6 两列（已在计划文档「M2 回填点」登记）。
- `cargo xtask build` 归置 `bin/devkit/`；`cargo test --release -p xtask` 的 devkit 契约用例校验
  产物与源一致。

## v0.1.87

WS 目录镜像路由与 `api.ts` 同口径（`--module-root` 下剥布局段）+ 挂载清单外显。

- **fix(ws)**：`--module-root` 布局下 `ws.ts` 的目录镜像路径曾按「文件父目录相对**挂载根**」
  计算，比 `api.ts`（走模块索引的 `{prefix}/{模块根内路径}`）多出一层布局段——
  `src/wiki/api/ws.ts` 挂成 `GET /api/wiki/api/ws`，而文档契约是 `/api/wiki/ws`
  （`api` 是模块根即布局分隔符，不是路由段）。前端按文档连即恒 404。现两侧共用
  `routes::mirror_rel`，平铺布局（无 `--module-root`）路径逐字不变。
- **fix(ws)**：上一条连带的守卫面错位——`path_no_base` 曾为 `/wiki/api/ws`，
  `auth.anonymous_paths` 里按文档写的 `/wiki/ws` 命中不了，浏览器 WS 握手 401
  （浏览器 `WebSocket` 发不了 `Authorization` 头）。现同源推导为 `/wiki/ws`。
- **新增（外显）**：启动打印 WS 挂载清单 `ws: N mount(s)` + 每条 `GET <路径> → <源文件>`，
  命中 `anonymous_paths` 的标 `[anonymous]`。WS 不进路由表也不进 `openapi.json`，
  该清单是唯一权威面——下游工具据此核对，勿从目录结构自行推算。
- **fix(ws)**：两个**不同目录**的 `ws.ts` 映射到同一 URL → 拒启（`ws mount conflict: …`，
  点名两侧文件）。口径统一后该碰撞成为可能，而胜者决定执行哪份 JS
  （会话状态与 `bus.subscribe` 出口都在其中），不能交给文件遍历顺序。
  同目录 `ws.ts`/`ws.js` 兄弟仍是先到先得（release 下 `ws.js` 优先）。
- **fix(ws/api)**：扫描剪枝与模块发现同口径——跳过点目录、`node_modules` 与目录符号链接
  （挂载根下的 `node_modules/**/ws.ts` 不该变成对外端点）。
- **fix(路由)**：根前缀挂载（`prefix: /`）拼出 `{base}//ws`、`//tree` 双斜杠路径
  （注册成功却永远匹配不到）。新增 `routes::join_url` 供 api.ts 与 ws.ts 共用。
- **行为变更（升级注意）**：仅 `--module-root` 且 `ws.ts` 在模块根内时 URL 改变
  （`/api/wiki/api/ws` → `/api/wiki/ws`，旧 URL 变 404），`anonymous_paths` 的守卫面
  同步改变（原 `/wiki/api/ws` 条目失效；表内 `/wiki/*` 之类条目可能让该 WS **变为
  匿名可达**——启动清单会标 `[anonymous]`，请核对）。平铺布局与 release 均不受影响。
- **已知限制（维持）**：release 的 WS URL 仍含模块版本段（`…/news-0.1.0/ws`，dev 是
  `…/news/ws`，两者不同源）；WS 不做 `_name_` → `{param}` 转换；新增/删除 `ws.ts` 需
  重启（dev 无文件监听）。dev/release 分叉的收敛（`oj build` 落 ws 行进 `routes.js`）
  单开工单，不在本版。

## v0.1.86

修 `--module-root` 下 `oj openapi` dev 内省锚点错误（裸包解析被截断）。

- **fix(openapi)**：`oj openapi --module-root` 的 loader `project_root` 曾锚 `dir`
  （源码树根 `src/`），裸包 `node_modules` 行走被截断在 `src`（工程根
  `node_modules` 在其外）——模板预生成 `web/__client/api.ts`（import
  `@oj-module/runtime/...`）内省即 fail-fast，`oj openapi → ojm api` 契约链在
  module-root 工程不可用。现改锚 `config_dir`，与 serve 装配（`app.rs` dev
  分支）同语义；dev / serve 两态 openapi 重新同源。
- **fix(openapi)**：regen 提示在 module-root 模式下原给 `-d`（与 `--module-root`
  互斥，原样复跑即 clap 报错或静默退回平铺语义），改给 `--module-root <dir>`。
- **fix(test)**：Windows 上两例 `module_root_of` symlink 用例期望值未剥
  verbatim 前缀（`\\?\C:\...` → 归一形态，契约由
  `module_root_of_tolerates_verbatim_prefix_mismatch` 钉死）。

## v0.1.85

模块根可配置：`--module-root` 递归发现 + `manifest.yaml` 新增可选 `prefix`。

- **新增 `--module-root <dir>`（serve / build / openapi / test / migrate / exec / schema
  diff）**：在该目录子树内**递归**发现 `manifest.yaml`，命中即停（不深入，模块根互不
  嵌套）。模块身份改取 `manifest.yaml` 的 `name`——**目录名只作定位，不再参与身份**，
  monorepo / 存量仓库里的模块可以放在任意深度的目录下（如 `services/user/api/`）。
  与 `-d`/`--api-path` **互斥**（clap 层即报错）：两者都是「源码树根」。
- **`manifest.yaml` 新增可选 `prefix`**：该模块的 URL 前缀（进路由 pattern，可含 `/`
  分段如 `sys/user`）；**缺省或空串 = 用 `name`**。URL 由目录镜像改由前缀决定：
  `<prefix>/xxx/yyy/api.ts` → `GET <prefix>/xxx/yyy`；dev 与 release 同口径
  （dev 路由表按模块根建，不再按首层目录）。prefix 进路由 pattern 前过形状白名单
  （`illegal url prefix '<p>'`），且全局唯一——两个模块争同一条 URL 直接拒启。
- **`#/m/x` 别名解析按模块名（manifest `name`）而非目录名**，与 `--module-root`
  配套：嵌套布局下跨模块导入不再依赖目录名碰巧一致。**`#/m/x` 是跨模块导入的首选
  写法**（目录搬动不用改引用）；`#x` 锚本模块根不变。
- **fail-fast 收窄**：显式给了 `--module-root` 却命中 0 个 manifest → exit 1
  （`no manifest.yaml under module root … 下一步：…`，防 CI 假绿）；`--module-root`
  指向 release 树（含 `manifests.yaml`）→ exit 1 引导指回 src 树；`name`/`version`/
  `prefix` 在递归发现期即过白名单（要进产物目录名与路由 pattern，不留给运行期）。
- **S008 覆盖模块根嵌套**：模块根之下再出现 `manifest.yaml` 报 S008（嵌套会改写别名
  锚点）；模块名/前缀重名同样 fail-fast。
- **源码树内就地 build 不再撞车**：递归扫描整棵跳过 release 树（含 `manifests.yaml` 锁
  的目录）——否则 `dist/<m>-<v>/manifest.yaml` 的**原样副本**会把同一模块重复登记成
  第二个模块根（实测报 `declared in multiple module roots`）；S008 的 stray 扫描同口径。
- **修 `module_root_of` 符号链接误判（存量缺陷，`--module-root` 放大）**：referrer 来自
  file URL（已 realpath，macOS `/tmp` → `/private/tmp`），project_root 是原样 CLI/config
  路径——词法前缀比较把「明明在模块内」误判成「逐级上溯未找到模块根」（`oj openapi
  --module-root` 实测复现）。两侧 canonicalize 后比较（与 `ensure_within` 同款；
  canonicalize 失败的虚拟目录按词法前缀换根，保持两侧同形态）；openapi 的 dev/release
  内省 LoaderShared 同批改 canonicalize 口径。附带修掉 openapi 启动错误的双重
  `oj openapi: ` 前缀。
- **Windows：`strip_verbatim` 修 UNC verbatim 还原**（存量缺陷，本期负载化）。
  `canonicalize` 对网络共享返回 `\\?\UNC\server\share\...`，此前盲目剥 `\\?\` 得到
  `UNC\server\share\...`——无根锚的坏路径（`from_file_path` 建不出 URL、前缀比较全线
  失配）。现在正确还原为 `\\server\share\...`；`module_root_of` 的双侧 canonicalize
  在 UNC 部署（网络盘项目根）下依赖该还原。`resolve_to_segs` / S008 的 canonical
  兜底同批归一：不剥前缀时，verbatim 形态在 Windows 上永远剥不动一旁的非 verbatim
  路径（`to_file_path` 侧已剥），兜底退化成死分支。
- **兼容性**：不带 `--module-root` 时全部路径走今日语义——`-d src` 平铺树的构建产物
  （含 tgz 字节）与 `oj openapi` 生成物与 v0.1.84 **逐字节一致**（基线二进制对比实测）。

## v0.1.84

插件/FFI 层审计修复（七项：rabbitmq ×2、kafka、db 双插件、blob、kv、宿主 loader）。

- **bus(rabbitmq)：publish 复用 channel（修 channel 泄漏打满 channel_max）**。bus 面逐次
  publish 新建即弃 channel（lapin 2.5 的 Channel 无 Drop→close），约 2000 次后 broker
  杀连接——publish 全挂且该连接上全部推送订阅断线。现在与 mq 面同款复用通道
  （断线/失效自动重建），交换声明用的 channel 一并归入复用槽。
- **bus(rabbitmq)：推送订阅自愈（断连不再永久静默）**。消费循环遇连接/通道级错误此前
  `break` 永久退出——一次抖动即 topic 停摆到进程重启（排他自动删除队列随断连消失，
  停摆窗口内的消息一并丢失）。现在退避（0.5s 起、上限 10s、成功收信即重置）后重建
  队列+绑定+消费者；**首次**搭建仍同步执行并报错（宿主失败回滚语义不变）。
- **bus(kafka)：消费错误日志节流**。持续性错误（认证失败/集群失联）librdkafka 逐拍
  上报，此前逐条 stderr 刷爆 logs/——现在同一订阅 30s 至多一条，成功收信即重置。
- **db(mysql/postgres)：流式游标专用连接并发上限（8）**。typed 游标每条单开一条池外
  连接且原无上限——并发大结果集可打满服务端 max_connections（此后连池查询一并失败，
  全库不可用）。现在超限 fail-loud：`db stream_open: too many concurrent cursors
  (limit 8) — close or cancel running streams first`；游标收口（done/err/cancel/close）
  即释放配额；Any 回退路径走池不占闸。
- **blob：直传/上传 pump 增断连兜底 abort**。客户端断连/超时取消会丢弃 pump future，
  此前 finish/abort 都不执行——s3 multipart 留 orphan parts（持续计费）且插件侧会话
  条目永久泄漏。现在流式会话在场即持守卫，Drop 兜底 fire `put_stream_abort`
  （幂等）；显式错误路径与正常 finish 语义不变。
- **kv(redis)：expire 的 ttl 越 i64 拒绝**。`ttl_secs`（u64）此前 `as i64` 回绕，超大
  值变负数——而 Redis EXPIRE 负数即立即删键（静默错动作）。现在响亮报错。
- **宿主：泛型轴清单空 vtable 跳过**。`oj_plugin_axes` 声明 kind=GENERIC 且 vtable=null
  的畸形插件此前裸 cast 出空引用进注册表，首次 `axis()` 调用即 SIGSEGV——现在与 dlsym
  回退路径同款：告警并跳过该轴。

## v0.1.83

- **exec：回调/句柄形态支持 `stdin`（订正静默丢弃）**。`exec.run` 带 `onStdout`/`onStderr`
  的回调形态与 `exec.spawn` 此前把 `opts.stdin` 静默丢弃（子进程 stdin 恒 null）——同一 API
  两种形态行为分叉且不报错（`sh -c cat` 吃空输入仍返回 code 0）。现在三形态语义一致：
  一次性写入后关闭写端；子进程不读且输入大导致 write 阻塞由 supervisor 超时组杀兜底（与
  缓冲形态同窗口）。
- **exec：Wrapped 模板短参数由宿主 panic 改命名报错**。`cmd /c echo %${raw:NAME}%` 这类
  Wrapped 占位符收到「以前缀起手、后缀收尾，但总长比前后缀合计还短」的 argv token（如单
  字符 `%`）时，切片前不判长度直接 slice-index panic——JS 可触发的宿主 panic（500/毒化
  runtime）。现在并入既有失配报错 `template '...': token expects '%${...}%', got '%'`。
- **exec：句柄资源补真实的 `Drop` 兜底**。runtime 回收注释承诺的 `ExecHandleResource::Drop`
  此前不存在（只有显式 `close()`）——heap-limit 触发等异常丢弃路径不回池也不清资源表，
  孤儿子进程组要等到 supervisor 超时臂才被击杀（最长 600s）。现在 Drop 即发组杀信号，实现
  与注释一致。
- **SQL 构造器：表别名与聚合/CTE 别名同门校验**。`db.table("t").as("")` / join 表别名 /
  `"t as 9bad"` 此前不做形状校验，空串/怪形漏到 SQL 渲染（`FROM "t" AS ""`）换回一句看不懂
  的 DB 原生报错（租户表上还会拼出 `.tenant_id` 的迷惑限定列）。现在反序列化期报
  `illegal alias '<a>'`。
- **SQL 构造器：fromSelect 源 select 纳入归属守卫**。`guard_req` 此前不递归 `from_select`
  （`apply_tenant` 的递归在先）——`ownership_guard: deny` 下 A 模块可
  `db.table("自己的表").fromSelect(db.table("别人的表").select(...), ...)` 把他人模块的行
  搬进自己的表（等价明查会被拒，绕一层 fromSelect 即穿）。现在源 select 与 union/cte/
  条件子查询同列递归覆盖。
- **`oj openapi`：dev 模块目录名不再被误剥版本后缀**。`module_of` 此前不分 dev/release 一律
  剥首段 `<module>-<版本号>` 尾段——dev 目录树里 `src/preview-0.9/` 这类合法模块名被归到
  不存在的 `preview` 模块（tags / `x-oj-file` / 生成客户端分组全错）。版本剥离现在仅对
  release 直载表（`from_entries`）生效。
- **性能/去重**：URL 挂载命中复验与 `oj build --bundle` 的 sha256 改流式计算（此前把整个
  .tgz 读进内存才算摘要——512 MiB 产物即 512 MiB RSS 尖峰，复验每次 serve 启动每个 URL
  挂载都发生）；三处逐字重复的 hex 编码 helper 收敛为 `oj` crate 一份（`hex_encode`）。

## v0.1.82

- **SQL 构造器 v0.1.82：三项能力补齐，清零下游最后 4 处方言分支/裸 SQL 逃生口**。诉求来自下游
  模板（`_lifecycle/uninstall.ts` / `rate.ts` / `_shared/service/infra/query.ts` / `oplog.ts`）
  的五项需求评审（A 目录自省 / B 整表清空 / C 分页 / D count distinct / E like escape）+ 一条
  `orIgnore × fromSelect` 下游反馈（F）：
  - **D `count(distinct col)`**：`select([{fn:"count",distinct:true,field:"user_id",as:"c"}])`。
    聚合一行返回、不受 LIMIT 钳制——替换「`distinct().all()` 拉行 `.length` + `limit` 硬顶」
    的静默漏算形态（显式 limit 本就被 clamp 到 `max_limit` 默认 1000，`limit(100_000)` 实际
    只到 1000）。仅 count 支持；缺 `field`/`"*"` 报 `count distinct needs a field`，其他聚合
    带 `distinct` 报 `distinct is only supported on count`（显式拒绝，不静默忽略）；
  - **A `db.listTables(prefix?)`**（方言无关目录自省，新 op `op_db_list_tables`，归位 `db.rs`）：
    返回当前库全部业务可见表名（**含未登记 registry 的表**），按名排序，可选前缀过滤——SQL
    文本引擎常量 + 前缀经 `escape_like` 转义后绑定参数（`LIKE ? ESCAPE '|'`，'|' 不与任何方言
    字符串转义规则冲突）；sqlite 经 `sqlite_master`（substr 排除 `sqlite_*` 内部表），mysql/pg
    共用 `information_schema` 限当前库/schema，占位符经 `normalize_and_trace` 统一改写（PG
    `$n`）；`db` 与 `db.tx` 对象同签名。消除 uninstall 的 #1/#2 两个方言分支裸 SQL；
  - **B `db.table(t).truncate().run()`**（显式整表清空）：渲染 `DELETE FROM t`（sqlite 无
    TRUNCATE 语句，sea-query 的 TruncateStatement 不可移植）。**租户守卫照常注入**——租户表上
    只清当前租户（构造性 fail-safe，清不出租户边界），整库清空在 `db.asSystem` 下执行。用户
    自带 where 在租户注入**前**由 `validate_truncate` 拒绝（报 `truncate does not accept where`），
    `update` 携带 truncate 报 `update does not accept truncate`；JS 链层 `requires where` 门禁
    对 truncate 放行。消除 rate.ts 的 #3 逃生口（裸 `db.exec("delete from …")`）；
  - **F `orIgnore × fromSelect`（sqlite）明文化**：引擎在 v0.1.81 即构造期显式拒绝（sqlite 的
    upsert 子句只能跟 VALUES），本次把限制写进 api-manual（含错误/限制表）、sql-builder-guide
    §3.3 与 SKILL 陷阱速查，附绕行口径（裸 `db.exec("insert or ignore … select")` 及其
    NOT NULL/CHECK 静默跳过的数据面代价）；
  - **E 无代码**：`like` + `escape`（白名单 `! / | ~ # @`）与 `contains` 的字面/模糊匹配能力
    v0.1.81 已具备，本次写进迁移清单第 8 条（匹配字面 `%`/`_`）；
  - **C 引擎零改动**：分页 = 构造器原语（count 聚合 + limit/offset + contains）之上的模板层
    工厂函数（`scenarios.md` 场景 42 给了照抄实现），不在引擎加 `db.paginate`；
- **结构重构（行为不变）**：`src/bridge/query.rs`（5547 行）测试模块（3016 行）拆至
  `src/bridge/query_tests.rs`（`#[path]` 子模块），生产文件减半、保持构造器语义单一内聚；
  目录自省 op 归位 `db.rs`（库级 op 与 `op_db_query` 同类，SRP），`escape_like` 转 `pub(crate)`；
- **测试**：新增 6 个用例（count distinct 聚合/形状/两处门禁、truncate 清空/两处拒绝、
  listTables 含未登记表排序 + 前缀转义 %/\_ 字面量）+ `raw_sql_op_exits_are_pinned` 白名单
  增补；`cargo test --release -p only-js` 690 通过；
- **文档**：devkit 四件 + `sql-builder-guide.md` 逐条对齐（能力矩阵 +3 行、迁移清单 +4 条、
  场景 42、陷阱速查 +4 行、`sample/global.d.ts` 类型同步）。

## v0.1.81

- **db.table() 构造器补齐 7 项能力**（v0.1.81）——下游模板 `docs/sql-builder-missing-capabilities-pr.md`
  的 7 条诉求经复核后落地。诉求来源复核结果：1 条误报（动态 WHERE 已可用）、1 条部分误报
  （`select *` 仅「省略 select」可用）、1 条漏报（**表别名**，实为 notify.ts 的真正阻塞点）；
  **`expr` 任意表达式模板经四路评审砍除**（理由见设计稿 §13：PG 占位符为 `$N` 而非 `?`、
  字符白名单挡不住 `pg_sleep`/`load_file`、无下游真实需求举证、模板串迁移不可逆）。
  设计稿 `docs/plans/2026-10-19-sql-builder-capabilities-design.md`，
  实施计划 `docs/plans/2026-10-19-sql-builder-capabilities-implementation-plan.md`；
- **#1 模糊搜索根治**：新增 `contains` / `startswith` / `endswith` 三个 op（**全小写**，与
  既有 `gte`/`lte`/`isnull` 同族）。引擎内部统一转义 `%` `_` `!` 并渲染 `LIKE ? ESCAPE '!'`，
  **业务层零转义负担、零方言知识**。保留 `like` 的 `escape` 逃生舱（`len==1` + 白名单
  `! / | ~ # @`，**显式拒绝 `\`**——mysql 默认 `sql_mode` 下 `ESCAPE '\'` 即 1064），
  定位为高级/框架内部，不在 op 清单主行展示；
- **#2 join ON 值条件**：`join(t, [{left,right},{left,op,value}])`，可表达
  `ON r.broadcast_id = b.id AND r.user_id = ?`；op 限比较类（`contains`/`isnull` 等拒），
  value 过 `encodeParams`（BigInt 标记不丢）；
- **#3 字面量列**：`{lit: v, as: "x"}` 走绑定参数、按方言 `CAST`（sqlite 裸 `?` /
  mysql `CAST(? AS CHAR)` / pg `CAST(? AS TEXT)`）。**计算列请用现成的 `case`**
  （`{case:{when:[...],else:...},as:"x"}`），注意 op 拼写是 `isnull` 不是 `isNull`；
- **#4 幂等写入**：`orIgnore()` 与 `fromSelect(q, cols)`。
  **mysql 走 `ON DUPLICATE KEY UPDATE <pk> = <pk>` 而非 `INSERT IGNORE`**
  （后者会把 NOT NULL/截断等**所有**错误降级为警告，属静默数据损坏面；
  且 sea-query 该路径上游零测试、组合后疑似产出非法 `ON DUPLICATE KEY IGNORE`）；
  mysql 无主键表直接 Err；**`orIgnore` + `returning` 在 mysql 上直接 Err**；
  **注意三方言 affected-rows 不一致**（mysql 协商 `CLIENT_FOUND_ROWS` → 命中重复键返回匹配行
  1；pg/sqlite `DO NOTHING` 返回 0），**不要写 `if (n === 1)` 判定幂等**；
- **#5 全表 delete 不豁免**：守卫（`bootstrap.js` JS 侧 + `query.rs` Rust 侧）保持双层，
  本次未引入任何绕过路径；
- **#6 动态 WHERE**：新增 `whereIf(cond, c)` 糖（`undefined`/`null`/`false`/**空数组 `[]`**
  均跳过）；引擎侧新增 `Op::In` 空数组守卫（`IN ()` 直接 Err）；
- **#7 `select(["*"])`**：显式全列展开；`["*"]` 必须是唯一元素，
  **带 join 时直接 Err**（全列展开只出基表列，业务拿 join 列做授权判断会拿到 `undefined`）；
- **#8 表别名**：`db.table("x").as("b")` / `.join(..., "left")`，放开自 join。
  **仅 select/join 生效**，update/delete 带别名直接 Err（mysql `DELETE FROM t AS b`
  需 8.0.16+）。实现上引入 `TableRef` 类型并在入口一次归一化，
  **修掉了「别名会让归属守卫与租户守卫同时静默失效」的严重缺陷**
  （`owner_of("t as b")` → None → judge 放行；`scoped("t as b")` → None → 不注入 tenant）；
- **内部重构**：三个 `QueryReq` 遍历器（`guard_req` / `apply_tenant` / `validate_nested`）
  统一走 `child_reqs()` 单一出口，`INSERT … SELECT` 这条新嵌套边不再可能漏遍历
  （漏则跨租户读写 + 归属绕过）；`Op` 加 `class()` 消除三处重复的 op 子集判定；
- 文档：新建 **`docs/devkit/sql-builder-guide.md`**（SQL 构造器专题：能力矩阵 / 方言差异 /
  红线与逃生舱 / 迁移路径）；devkit 四件同步（详见下）。
  既有文档订正：`api-manual.md` op 清单 `isnull` 拼写、`select *` 说明、
  「union requires explicit columns」提示；
  **`SKILL.md` 原第 117 行「手写 ESCAPE 转义」条目已改写**（与新方案直接冲突）。

## v0.1.80（未打标签）

- `oj secret keygen` 新增 `--alg`（v0.1.80）：`x25519`（缺省，行为零变化）/ `ed25519`——为
  v0.1.75 的 `crypto.verifyEd25519` 补齐**作者侧密钥生成**（bundle 签名校验链路：发布方签 →
  安装方隔离舱验；此前作者只能手写 Node 脚本）。为什么不能复用 X25519：X25519 是密钥协商
  （ECDH）、做不了 Ed25519 签名，且 `verifyEd25519` 明确只收 32 字节原始公钥——算法族不同，
  必须分开生成；
- `--alg ed25519` 产出（`--out-dir`，覆盖拒绝沿用 `--force`）：`ed25519-private.pem`
  （标准 PKCS#8 `BEGIN PRIVATE KEY`，openssl / Node `crypto.createPrivateKey` 可直接读，
  0600 创建）、`ed25519-public.hex`（32 字节**原始公钥**的 64 hex 字符——**必须 raw 32B**，
  喂 PEM/SPKI 会抛 `key not hex/base64`）、`ed25519-public.fp`（sha256(rawPublicKey) hex，
  运维核对 / 信任库 mismatch diff 用）；
- 测试钉死：往返 smoke（keygen 产物 → ring 以 PKCS#8 重读并签名规范串 `<name>@<version>:<sha256>`
  → 标准 base64 无 pad → 真 bridge 里 `verifyEd25519` = true / 换 msg = false）+ 对抗面
  （64 hex 格式门、指纹独立重算比对、0600 权限、覆盖拒绝、两算法互不串扰、PKCS#8 标签
  不得混入密封标签）；args 解析面（缺省 = x25519、`--alg ed25519` 可解析、非法值 clap 拒）；
- 配套：devkit 四件同步（api-manual §secrets 与 §crypto 的 verifyEd25519 注 + 错误/限制表
  「Ed25519 签名密钥 ≠ 密封密钥」条、SKILL 陷阱速查、scenarios 场景 18 提示、README 版本行）
  + `docs/secrets.md`（§2/§3/§11）+ `docs/user-manual.md` CLI 行、`oj/Cargo.toml` 升 0.1.80。

- **fix（xtask smoke）**：`SourceHider` 残留处理补两种中断态——① **半途失败不还原**：`new`
  在 hide 序列中失败时 `Self` 尚未构造、Drop 接管不到，已隐藏路径（含仓库内两个 bootstrap.js）
  被留在缺失状态，后续任何构建当场报 `include_str` 找不到文件；现失败即就地还原。② **双份
  并存**：上次中断后 cargo 重新解包出 orig、孤儿 `.oj-smoke-bak` 同时在场，rename 撞非空目录
  报 os error 66、smoke 永远过不去；现认 orig 为真相（registry 目录由 crates.io 哨兵钉死）
  并清掉孤儿 bak。配套两个对抗用例钉死（部分失败还原 / 双份清障）。

## v0.1.79（未打标签）

- exec 模板占位符支持**字面前后缀**（v0.1.79）：占位符可被同一 argv token 内的字面量包裹——Windows `cmd /c echo %VAR%` 的 `%…%` 是 cmd 变量定界符、无法拆成独立 token，现模板写 `cmd /c echo %${raw:NAME}%` 即可（匹配取中间子串为绑定值、重建时以模板前后缀原样还原）；`${raw:}` 的 option-like / 绝对形态校验作用在**中间子串**上。此前这类模板在解析期即被拒（旧规则要求占位符占满整 token），Windows 上无法为 `echo %VAR%` 建模；
- Windows 上 exec 的输出形态文档化（**用户可见行为补录，非行为变更**）：`cmd /c echo` 恒以 CRLF 收尾、`cmd /c type <file>` 原样吐文件字节（不补行尾）；exec 返回子进程**原始字节**、不做 CRLF→LF 归一（与 `Deno.Command.output` / Node `child_process` 同口径）——跨平台文本比对请用 `trimEnd()` / `split(/\r?\n/)`，别写死 `"\n"`；
- Windows 子进程输出是本地代码页（GBK / CP1252），含非 ASCII（中文路径 / 报错 / `ping` 本地化文案）时即落 `exec: stdout is not valid UTF-8`（stderr 同文案）——要 UTF-8 输出，把 `chcp 65001` 写进 `cmd /c` 语句；此为 Windows 控制台既有行为，非 oj 限制；
- 配套：devkit 四件同步（api-manual §6「Windows 平台命令形态」两条 + 错误表非 UTF-8 条目、SKILL 陷阱速查两条、scenarios 场景 39/40 各一条、README 版本行）、`oj/Cargo.toml` 升 0.1.79、`cargo xtask build` 归置 `bin/devkit/`。

- **文档订正（v0.1.78 描述与实现不符，实现未动）**：带 `onStdout`/`onStderr` 的 `exec.run` **实际返回 `{ code, stdout, stderr }`**（实现把全部分块攒在内存后 merge，只是解码改走**有损** `TextDecoder`、非法 UTF-8 变 U+FFFD 而**不抛**）——此前 api-manual / scenarios / devkit README 写的是「返回 `{ code }`，不缓冲整段」，两处都不准确；**真正不占内存、拿原始字节的只有 `exec.spawn` 逐块消费**。同一批还订正：`exec timed out` / `output limit exceeded` / `not valid UTF-8` 三条**只在无回调的缓冲形态抛**，回调与 `exec.spawn` 形态不抛、只把 `code` 打成 137（超大输出被截断）——别用 `try/catch` 的成功当「跑完了」；`opts.stdin` **只有无回调的 `exec.run` 支持**（spawn / 回调形态的子进程 stdin 恒 null，传了静默丢弃）；流式逐块解码要复用同一个 `TextDecoder`（逐块 new 会在块边界把多字节切坏）。配套：unix 面新增 S12 三形态对抗用例（缓冲抛 / 回调有损不抛 / spawn 原始字节不抛），devkit 四件同步 + `cargo xtask build` 归置 `bin/devkit/`；
- **fix（Windows CI）**：`exec_bdd_main_scenarios_windows` 的 S4（env 白名单可见性）报 `NotCapable: exec template ambiguous: argv matches 2 authorized templates`——夹具同时配了 `cmd /c echo ${raw:word}` 与 S4 用的 `cmd /c echo %${raw:NAME}%`，而裸 raw 是 `%…%` 包裹 raw 的**超集**：上一条能力落地前 `%${raw:NAME}%` 在解析期就失败、从未走到匹配期，落地后 `%VAR%` 形态 argv 首次同时命中两条。**生产语义未动**（命中 ≥2 即歧义拒绝、不静默择一是设计约定），改的是夹具：`cmd /c echo` 家族改为每种取值一条全字面模板。同时新增**平台无关**单测 `windows_fixture_templates_are_unambiguous`，直接对夹具模板表断言「每个场景 argv 恰命中 1 条」+ 反例钉死「同位置裸 raw 与 `%…%` 包裹 raw → `Ambiguous{count:2}`」，把这类只能由 Windows CI 暴露的缺陷挡在 unix 面；另修 `exec_bdd_streaming_scenarios_windows` 的 SS8——上一版收敛行尾期望值时漏了这一处（仍写死 `"compat\n"`，而 cmd echo 恒 CRLF），现改经 `echo_out`；
- **fix（Windows CI）**：`src/bridge/exec.rs` 两个 `#[cfg(windows)]` BDD 用例的行尾期望值此前散落在 8 处字面，修完一处下一处接着红。现两处夹具一律经新增的「行尾真相源」拼装——`s1_cat` 的期望值与写入文件的 `IN_TXT` 同源（`type` 不补行尾），echo 类断言一律经 `echo_out`（cmd 的 `echo` 恒补 CRLF）；大文件夹具（10MiB）显式 `timeoutMs: 40000`，避免 `cmd /c type` 在慢机器上让 30s 缺省先到、把「输出超限」误呈为「超时」。生产代码未改。

## v0.1.78

- exec 流式形态（v0.1.78）：`exec.spawn(argv, opts?)` 返回句柄 { rid, pid, stdout, stderr, kill, exit }，stdout/stderr 为 `AsyncIterable<Uint8Array>`，`for await` 逐块消费、`h.exit()` 等退出码、`h.kill()` 整组击杀；`exec.run(argv, opts?)` 扩展 `onStdout`/`onStderr` 回调（增量转发、返回 `{ code }`，不缓冲整段），无回调时与 v0.1.75 行为 / 返回形状完全一致；
- 共用 opts（timeoutMs 缺省 30_000、硬上限 600_000 钳制；stdin 一次性写入后关写端）；背压经有界 mpsc（16 槽 × 8KiB）实现真实子进程背压——不消费流则子进程在输出填满管道时阻塞，非 bug；
- 句柄 rid 进 OpState.resource_table，close（JS 释放 / runtime 归还 / 异常丢弃）同步整组击杀子进程，杜绝跨请求 / 租户泄漏与 next_rid 回绕 panic；rid 仅本 runtime 可达，只能 kill 自己 spawn 的会话；
- 输出 10MiB 上限、超时整组击杀、UTF-8 强制解码、cwd = fs.root、env 空表等硬限制与缓冲形态一致；复用全部既有安全闸门（per_module 授权、argv 模板白名单、`${path:}`/`${raw:}` 占位符、整组 kill）；
- 配套：devkit 四件同步（api-manual §6 新增「exec 流式形态」小节 + 错误/限制表、scenarios 场景 39/40、SKILL 三条陷阱）、`oj/Cargo.toml` 升 0.1.78、`cargo xtask build` 归置 bin/devkit/。

## v0.1.77（未打标签）

- `mounts[].api` 支持 `http(s)://` URL：装配期拉取「dist 根 tarball」并解压到内容寻址缓存（`<cache>/<sha256>/`，`cache` 缺省 `<config_dir>/.oj-cache`），以 release 语义服务；完整性锚 = 声明 `sha256`（64 hex），下载内容 sha256 不符即拒启（防篡改 / CDN 漂移）；产物根含 `manifests.yaml` 才视为合法 dist 根；
- 解压器逐条目 fail-fast：拒绝 tar-slip / 符号链接 / 特殊文件 / pax·gnu 元数据，并对下载量（512 MiB）/ 解压总量（4 GiB）/ 条目数（10 万）/ 单条目（1 GiB）设上限防炸弹；命中缓存按 sha256 复用、损坏自动重拉；per-sha256 互斥串行化同 URL 物化；
- `oj build --bundle`：把整棵 dist（manifests.yaml + 各 `<module>-<version>/`）打成空前缀 tarball `<out>/<name>.tgz` 并打印 sha256，供上面的远程挂载声明完整性锚（手工等价 `tar czf app.tgz -C dist .`）；
- `--api-path` CLI 不接受 URL（远程挂载的 sha256 上下文只在 config 声明期存在，CLI 折叠期无此上下文 → 报错引导走 config）；
- `MountConf` 派生 `Default`，`validate_mounts` 新增 URL scheme（仅 http/https）+ 必带 64 hex sha256 校验、本地 Windows 风格路径（如 `c:/dist`，无 `://`）按本地目录处理；`reqwest` 由 dev-dep 提升为发行依赖（mount_fetch 跑在生产 serve 路径）。

## v0.1.76

- `http.module`：模块路由分发注入只读模块身份 `{name, version}`（dev 版本取 `src/<m>/manifest.yaml`，release 取 `dist/<m>-<v>/manifest.yaml`）。判定依据是 handler 文件所在模块目录而非请求来源——无模块上下文（任务桥 / WS 帧 / `run_with` / 启动期内省）为 `null`；页面 meta 合成派发在模块目录内有值；
- 只读语义：返回对象已 `Object.freeze`——strict（ESM / api 文件 CJS 包装）下写 / delete 抛 `TypeError: Cannot assign to read only property …`，`require()` 引入的 CJS 代码（sloppy）写它静默无效。两条路径都改不了框架授信面（跨模块表归属守卫 / bound_db 重定向 / 平台特权换装）——JS 只持 `op_http_info` 快照拷贝；
- release 装配新增 manifest version 与目录后缀 `<module>-<version>` 一致性 fail-fast：错位 / 手改的 dist manifest 不再让 `http.module.version` 与真实加载版本静默不符；
- 配套：devkit 四件同步（api-manual §6.1 + 错误/限制表、SKILL 三条陷阱、scenarios 场景 36、README per-version 段）、`sample/src/user/module-info/api.ts` 演示端点、e2e dev/release 双模式 + 对抗性冻结探测（含 `require()` sloppy 路径）。

## v0.1.75

本版承载「ojm 后端原生化」批次（设计稿与实施计划见 `docs/plans/2026-10-16-ojm-native-exec-*`）。

- `crypto.verifyEd25519`：Ed25519 验签原语（格式错抛错、验签不过 false）+ `crypto.sha256Hex` 支持二进制入参；`crypto.md5Hex` 加入（同分流：string 走 UTF-8、Uint8Array/ArrayBuffer 走原始字节，其余抛 TypeError；**MD5 已密码学破损，仅限非安全场景**）；
- `exec.run`：外部命令原语（`platform.exec` argv 模板白名单 + 路径占位符校验，缺省全 deny）；
- `platform:` 配置段：平台特权模块（modules）与授权路径根（trusted_paths），特权面 = reloadConfig；
- `server.reloadConfig()`：配置热更进程内触发（与 `POST _system/config` 同一处理体；`platform`/`fs` 段变更归 restart_pending）；
- `yaml.parse` / `yaml.stringify`：YAML 读写原语（节点/别名/深度三预算防炸弹）；
- `oj migrate`：`--format json`（逐模块成败 + 错误脱敏）+ `<module>-<version>` 定向目录。

## v0.1.74

**fix（introspect/d.ts）：`.response` 空值语义统一 + 保留键提示。**
/code-review 复核 v0.1.72 发现两处边界不一致，本版收敛：

- **null 视为未声明**：v0.1.72 平级 `.response` 链用 `!== undefined` 判定，`.response = null`
  会胜出优先级链并在装配期被 audit 以「schema must be an object」loud 拒绝；而
  `.schema.response` 合并走 truthy 检查，falsy 值被静默跳过、让组级违背文档优先级顶替。
  两通道现统一为「undefined/null = 未声明」：回落继承链与占位行为；声明了但非法
  （如布尔 schema）照旧装配期 fail-fast，不静默跳过优先级。
- **`global.d.ts`**：`Object.response` JSDoc 补保留键警告（组对象上的 `response` 被当
  声明读走，命名嵌套子组会静默不产生路由——与 `schema`/`route` 相同）；驳回「类型
  放宽到 `| boolean`」建议（审计契约本就拒非对象，TS 侧保持更窄才是对的）。

## v0.1.73

**fix（openapi）：`del` 动词归一为 `delete` 操作键。**
OpenAPI 3.x 的操作键只认标准 HTTP 方法，而 oj 运行期的删除动词是 `del`
（serve `DELETE`↔`del` 映射是既成事实，`serve/src/routes.rs`）——`oj openapi`
此前把动词名原样写进操作键，产出非法的 `"del"`：严格 OpenAPI 工具 / 生成器报错或
忽略，下游 `__client` 生成 `delXxx()` 而非 `deleteXxx()`，且对手工修正过的存量
`openapi.json` 必然 `--check` 漂移。本版在 `generate` 内一处归一：

- `del` → `delete`，其它动词原样；`operationId` / `summary` / path 键三处同源，
  同步归一（`del_x` → `delete_x`）。
- 仅影响 OpenAPI 输出；serve 运行期 wire 映射不变。
- 兼容性：存量手工 `"delete"` 与归一输出一致，`--check` 不再漂移；下游 client
  方法名 `delXxx` → `deleteXxx` 为一次性重命名（业务调用点需同步改名）。

## v0.1.72

**feat（openapi/introspect）：响应契约可在 `.schema` 之外的平级 `.response` 声明。**
此前出参（`json.ok(data)` 的 data payload）只能写在 `.schema.response`，入参与出参混在同一
对象里。本版新增与 `.schema` **平级**的 `.response` 声明位（动词级 `v.response` / 组级
`obj.response`，组级沿嵌套层层继承）：

- 生效优先级：**动词 `.response` > 组 `.response` > 动词 `.schema.response` > 组 `.schema.response`**。
- `oj openapi` 生成 `responses.200.content` 的逻辑完全不变：仍取最终生效的 response 作
  data payload，外裹 `{code,msg,data}` 信封；任何位置都未声明时保持占位
  `{ "200": { "description": "OK" } }`（无 content）。
- `.schema.response` 继续生效，存量零破坏；两位置并存时 `.response` 优先。
- 动机：`.schema` 只放入参、出参独立声明，匹配「handler 逻辑与契约分离 / 多文件」的演进方向。
- 实现：`introspect_module` 的 JS 内省脚本新增 `gresp` 继承链，胜出者写回 `schema.response`
  透传（浅拷贝写回，防共享引用污染同组兄弟动词）；`openapi_cmd` 零改动。

## v0.1.71

**docs（contract）：`.schema` 表达力边界显式化 —— zod 对照表 + 不支持清单。**
下游要拿 `openapi.json` 生成 zod（`contract.ts` → `.schema` 迁移的前提之一），必须先知道
oj 能表达什么、不能表达什么。本版不改任何行为，只把边界**写死并钉进测试**：

- **支持清单 → zod 映射表**：`type`（含类型数组 `["string","null"]` → `.nullable()`）/
  `required` / `properties` / `items` / `additionalProperties: false` → `.strict()` /
  `minimum`·`maximum` / `minLength`·`maxLength`（按**字符数**算）/ `pattern` / `enum`
  （数值按值比较）/ `nullable` / `minItems`·`maxItems` / `default`（oj 侧纯注解，不填值）。
- **不支持且 fail-fast**：`$ref` / `oneOf` / `allOf` / `anyOf` / `not` / `format` /
  `patternProperties` / `dependencies`，以及 JSON Schema 的 `if`/`then`/`else` / `const` /
  `contains` / `propertyNames` / `uniqueItems` / `exclusiveMinimum`·`exclusiveMaximum` /
  `multipleOf` / `maxProperties`·`minProperties`；zod 侧的 `refine` / `transform` /
  `discriminatedUnion` / `record` 同样写不了。
  - **联合类型 / discriminated union**：改用「可选字段 + 单形态」，判别逻辑进 handler。
  - **`format`（email / date-time / uuid）**：改成 `pattern`（它是唯一的字符串形状约束手段）。
- **跨语言红线写进文档**：`pattern` 是 Rust regex ≠ ECMA-262，下游搬进前端 zod 时，
  Rust-only 语法（lookahead / 反向引用）要么编译失败、要么**语义漂移**——后者更危险：
  Rust 拒的字符串在 JS 放行 = 前端校验被绕过。建议只写两边都成立的保守正则。
- **下游 codegen 三条注意事项**：属性名 / `enum` 元素 / `default` 值都是任意 JSON 字符串，
  必须 `JSON.stringify` 后嵌入，不得裸插值；`required` 项**可能不在 `properties` 中**
  （JSON Schema 允许只声明必填不约束类型），取值前先判存在。

新增 `dsl_boundary_is_exactly_the_documented_zod_subset` 钉死「白名单全集放行 +
不支持清单逐条 FAIL FAST」——没有第三种「静默降级」。devkit 四件已同步
（`api-manual.md` 新增「表达力边界：与 zod 的对照表」、`SKILL.md` 陷阱速查补
「写不了」两条、`README.md` 更新日志）。

## v0.1.70

**feat（contract）：`.schema` 新增 `response` 响应通道 —— 入参与出参同一处声明。**
下游 `oj-module` 要把契约权威源从 `contract.ts`（zod / `defineApi`）迁到 handler 的
`.schema`，前提是 oj 能同时表达出参。选 `response` 而不是另挂 `.responseSchema`：
与 `params`/`query`/`body` 同处一个 `.schema` 对象内 = 同一处声明、同一趟内省、
同一份落盘（`RouteRow.schema` 本就整对象透传），省掉一个特殊性而不是补回三个。

- **只声明、不校验**：运行期**不**校验响应——响应由 JS 产生，校验失败时请求已执行完、
  无处可去；把出参变成硬约束会让「少返回一个字段」升级成 500 级故障。它是**文档与
  生成真源**，不是守卫。
- **声明合法性照旧 fail-fast**：关键字白名单 / 类型字面值 / `pattern` 可编译，写错即失败。
- 组级合并与入参**不同**：`response` 走「动词级整体覆盖、无则继承组级」，不做
  `required` 并集 / `properties` 逐键覆盖——响应是动词各不相同的，组级必填被某动词
  继承会让文档声称返回并不存在的字段（假真源）。
- 组级 `mergeSchema` 不再给没声明的通道凭空注入空 `required` / `properties`
  （进 openapi 是噪声）；`Object.assign` 改对象展开，避 `__proto__` 键改到原型上。

**feat（openapi）：`responses.200` 落地 + 产物带模块归属 `tags`。**
`oj openapi` 不再是「只有请求侧的半份文档」，且成为下游 `__client` 生成的**唯一输入**
（上游不再生成 TS 代码——前端布局 / zod 版本都属下游约定，不该硬编码进平台件）。

- `response` = `json.ok(data)` 的 **data payload**；oj 的 wire 恒为
  `{code,msg,data}`（`src/bridge/envelope.rs`），故生成物**外裹信封**——公开产物不该对
  字节撒谎。`json.raw` / `json.stream` / `json.sse` 端点绕过信封，**不要**声明 `response`。
  未声明 `response` 的端点保持占位 `{"200":{"description":"OK"}}`（旧行为不变）。
- 产物自带 `x-oj-response-enforced: false`：免责写进 artifact，而非只写手册——下游**不得**
  把 `response` 当脱敏 / 裁剪边界。
- operation 级 **`tags: [<module>]`** + spec 根级 `tags`：生成代码必须知道模块归属，
  光有 `operationId` 不够。用标准 OpenAPI 分组字段（通用工具同样按它归组）；
  release 的 `<module>-<version>` 会剥掉版本后缀（`my-module` 这类连字符名不误剥）。
- `x-oj-file` 改**相对 api 根**路径：产物是公开文件，带本机绝对路径属信息泄露，
  且换机器就不可比。

**feat（contract）：`.schema` 顶层通道白名单 —— 拼错通道从此启动即失败（行为收紧）。**
`.schema` 顶层只允许 `params` / `query` / `body` / `response`；其余键（拼错的 `param` /
`bodys` / `responce`，或 `title` / `$schema` 之类）装配期**硬失败**。
以前这类键被静默忽略——接口一直在裸奔，而「声明了但没生效」是本项目最重的缺陷类。
报错带 method / pattern / handler 链 / 相对文件。**不要用 `server.schema_validation: false`
绕**：该开关只关「是否执行运行期校验」，声明合法性审计**不受它影响**（逃生门是给
「规则误杀合法流量」准备的，不是给代码写错准备的）。

**feat（openapi）：生成期两道新 fail-fast。**

1. **声明合法性审计**：`oj openapi` 以前完全不审计，坏契约会原样进 spec——而现在
   openapi.json 是下游代码生成的唯一输入。审计收口为 `RouteTable::audit_schemas()`，
   `oj serve` 装配 / `oj build` 落盘 / `oj openapi` 生成**三处同口径**（release 的
   `routes.js` 可能来自第三方 `.tgz` 模块，其 `.schema` 属构建期不可信输入）。
2. **operationId 唯一性**：`/user/profile` 与 `/user-profile` 会派生出同一个
   `user_profile_get`，下游按它命名符号会静默互相覆盖 → 生成期拦死。

**feat（contract）：`properties` 键名禁 `__proto__` / `constructor` / `prototype`。**
openapi.json 是下游生成 TS/zod 的输入，属性名会原样变成生成源码里的键——这三个名字
生成的字面量对象会改到原型上（与 `legal_handler_chain` 对 handler 链段同口径）。

新增用例：`response` 各路径审计 / 组级整体覆盖 / 运行期不拦请求（e2e 反向击穿）/
拼错通道在 `schema_validation: false` 下仍启动失败（e2e）/ envelope 与占位回归 /
模块 `tags` / operationId 重复 fail-fast / `rel_file` 不带绝对路径。
devkit 四件与 `docs/openapi-guide.md` 已同步（响应通道章节、`responses` 不再占位、
陷阱速查补白名单收紧与「不要用开关绕」）。**本版未打标签**（与 v0.1.71 同批提交，
只发 v0.1.71——升级即从 0.1.69 直接到 0.1.71，两版内容一并到达）。**本版未打标签。**

## v0.1.69

**fix（serve）：--daemon 子进程直连随包 glibc + 启动成败回传父进程（两项）**。
Linux 自带 glibc 发行包的 daemon re-exec 原走 `<bin>/oj` 启动器脚本：kernel 按
shebang 拉起**系统 /bin/sh**，DIR 计算还要 fork 子 shell exec `readlink`/`dirname`
——整段中间层都是系统 glibc，宿主这段任何异常（sh 变体 / coreutils 缺失 / exec 受限）
都让 daemon 子进程死得悄无声息（stdio 已指 /dev/null，父进程又「spawn 成功即报
daemonized」秒退，真因不可见）。两项修复：

1. re-exec 改为**直接 exec 随包 ld-linux + `<bin>/oj.bin`**（`OJ_BUNDLED_LD` 上溯两级
   得 `<bin>`）：子进程从 fork 后第一个 exec 起全程跑随包 glibc，链条零系统组件；
   `oj.bin` 缺失回退启动器脚本，非打包形态照旧 `current_exe`。
2. **就绪管道**（父读子写，写端 fd 号经 `OJ_DAEMON_READY_FD` 随 re-exec 传给孩子）：
   子进程监听绑定后回传 `ok`，启动失败回传真因；父进程等 30s——`ok` → 照旧打印
   daemonized 退 0；子进程启动期退出 → **带真因报错并非零退出**（不再假成功）；
   超时但存活 → 提示看 server logs。`run()` 拆出 `run_inner` 包装回传，非 daemon
   场景零行为变化。

**fix（contract）：query/param 数值 `enum` 误杀合法浮点值（根因修复）**。
`InputContract` 校验器对 `enum` 成员判定直接用 serde_json 的 `==`：query/param 经强转，
`number` 声明会得到 `Float(1.0)`，而契约字面量 `enum:[1,2]` 被解析为 `PosInt(1)`——
`Float(1.0) != PosInt(1)` 导致合法值被 `must be one of` 误拒（报 `got 1.0`）。改为数字按
**数值**比较（`numeric_eq`：两边整数精确比对、含浮点则按 `f64` 归并，使 `1 == 1.0` 成立、
`1.5` 仍不命中）。`type:"number"/"integer"` 两种声明下的 `enum:[1,2]` + `type=1` 均通过，
非成员（`type=1.5` / `type=3`）仍被拒。新增 `numeric_enum_in_query_matches_coerced_float`
钉死回归。无新 API / 报错文案，但 `api-manual.md`「入参契约 `.schema`」补数值枚举按值比较说明、
`SKILL.md` 陷阱速查新增对应一行、`README.md` 更新日志补 v0.1.69 条目（devkit 四件已同步）。

**test（oj-db-mysql 0.1.1）：MySQL affected-rows 语义契约测试（env-gated 真库）**。
下游（oj-wiki P4 评审）曾按裸 MySQL 文档语义推演「同值重复 UPDATE 得 0 行 ⇒ 重复已读
403」——该前提在 oj 栈上不成立：oj-db-mysql 的 typed 池经 sqlx 连接时协商
`CLIENT_FOUND_ROWS`（sqlx-mysql `MySqlStream::with_socket` 默认请求、与服务端广告取
交集），UPDATE 的 `rows_affected()` = **匹配行数**，与 sqlite `changes()`、pg 命令标签
同语义（不带该 flag 的裸客户端才是「实际变更行」）。新增
`real_mysql_update_affected_rows_counts_matched_not_changed` 钉死契约：同值重复
UPDATE → 1、WHERE 不匹配 → 0、真实变更 → 1——池构造被改（自定义 ConnectOptions 丢
flag）或 sqlx 升级改默认即红。插件 version 0.1.0 → 0.1.1（自本节起，插件涉及调整
必须 bump 插件自有 version）。

**devkit 同步**：api-manual §6 DML 节新增「受影响行数的三方言语义」——三方言矩阵
（mysql 协议原生默认「实际变更行」、oj 运行时协商 `CLIENT_FOUND_ROWS` 后与 sqlite/pg
一致为「匹配行」）+ 「先断言再写、行数不读」的跨引擎幂等写范式与代码；SKILL.md 陷阱
速查新增对应一行。api-manual §1 `--daemon` 描述补就绪等待 / 失败回传 / 随包 glibc
直连三项行为；SKILL.md 陷阱速查新增「`daemon child exited during startup`」一行。

**feat（scripts/install.sh）：第二位置参数指定安装位置，且须已存在且为空（安全约束）**。
安装位置可由第二位置参数显式指定（优先级高于 `OJ_INSTALL_DIR`），用于隔离安装；该目录
必须**预先存在且为空**，否则报错退出——脚本**不会主动清空 / 删除**目标，避免误指向含数据
目录被整体覆盖。默认安装目录（`~/.local/share/oj`）仍走 `mkdir -p` + 合并的旧升级语义。

## v0.1.68

**fix（sample）：`_shared/search.ts` pg 分支占位符与 `ts_headline` 调用修复（pg 真库验证首次跑通）**。
search 共享模块沿用 v0.1.61 的「pg 自写 `$n`」旧约定，与 v0.1.67 的统一占位符正面相撞：
likeFallback 多字段 OR 全写 `$1` 却绑 N 个参数、pg FTS 非 highlight 路径 `$1` 出现两处绑
2 个 → pg 上 `search.query` 一律报错（`placeholder count 1 != params N`）。
改为三方言统一 `?` 交宿主改写（删除 `P` 方言分支，highlight 与否绑定数 3/2 一一对应）；
顺带修 `ts_headline` 调用——原样照抄 sqlite `snippet()` 的七位置参数签名（PG 实际签名
`(config, document text, query, options 单字符串)`，document 须拼回原文而非 tsvector），
pg 高亮此前从未可用。**大小写语义对齐**：pg 的 `LIKE` 大小写敏感（sqlite LIKE 对 ASCII
不敏感、mysql 默认 collation 为 CI），likeFallback 在 pg 分支改用 `ILIKE`（同样支持
`ESCAPE` 子句），三方言短词回退行为一致。新增三方言真库回归
`search_shared_module_on_real_dbs`
（多字段表验证绑定数 × FTS 非 highlight/highlight × 2 字/1 字 LIKE 回退 × remove 后
零命中 × 大小写混合查询——LIKE 回退与 FTS 两路都覆盖；sqlite 内存库常跑，pg/mysql 需设
真库环境变量）。

**devkit 四件同步**：api-manual 限制表补 search pg 修复与 ILIKE 说明；SKILL.md 陷阱速查
新增「pg 上 `search.query` 报 `placeholder count 1 != params N`」一行；另对四件做了一轮
表述通用化清理（如「腿/钉死/口径」等内部说法改为通用表述，不改变任何技术含义）。

## v0.1.67

**feat（db）：`?` 成为三引擎统一绑定参数占位符——postgres 由宿主自动改写 `$n`**。
裸 `db.query` / `db.exec` / `db.stream` / `db.tx` 的 SQL 里写 `?` 即可跨 sqlite / mysql /
postgres 跑通，业务代码无需再按 `db.dialect()` 分支渲染占位符（修复 postgres 上
`?` 报 `syntax error at end of input` 的原始报障）。改写发生在宿主 op 层单漏斗
（`normalize_placeholders`，跳过字符串/标识符/注释/dollar-quote 引用区，
`?|`/`?&`/几何 `?x` 运算符豁免，`$n` 按最大序号续接），对构造器路径与 db 插件透明；
计数不匹配 fail-loud（仅 postgres：`placeholder count N != params M` + 正解 hint）；
sql_trace 记原始 SQL 不受影响。三类人群升级说明：

1. **一直写 `?` 的用户**：postgres 报错就此修复，存量代码零改动；
2. **一直写 `$n` 的用户**：继续有效，无需任何修改——`$n` + `?` 混用时 `?` 按已有
   最大序号续接（`where a = $1 and b = ?` → `b = $2`）；
3. **用 `?|` / `?&`（及几何 `?#`/`?-`/`?<`/`?>`/`?=`/`?~`）的用户**：不受影响，
   运算符形态的 `?` 一律豁免。

已知限制（详见 api-manual「统一占位符」节的行为矩阵）：PG 下 **jsonb 裸 `?` 存在性
运算符 + 带参** 与占位符无法区分，会被改写导致静默错绑——一律改用
`jsonb_exists(doc, 'key')`（空参语句不受影响）；`E'…'` 转义引号内的 `?` 已感知、不受改写。
计数 fail-loud 仅 postgres 触发，sqlite/mysql 的占位符/参数数不匹配仍由引擎执行期报错。

**fix（测试基建，顺手修复的存量假绿）**：`next_seq_first_use_inside_tx_on_real_db` /
`stream_matches_query_on_real_db` 两个 env-gated 用例误用 multi_thread flavor——
deno op 内 `deno_unsync::spawn` 断言 CurrentThread，env 一设即 SIGABRT；它们自写下起
从未真跑过（env 未设静默 skip）。改 current_thread 后在真库上修活。

**devkit 四件同步**：SKILL.md / api-manual.md / scenarios.md / db-guide 中六处
「pg 用 `$1`」旧表述全部反转为「统一 `?`」；api-manual 新增「统一占位符」节
（三 DSN 示例 + PG × JSONB `?` × 参数行为矩阵 + 错误口径分列）。

## v0.1.66

下游（oj-wiki P3 方言攻坚，MySQL 8.4.11 / PG 16.15 实机验证）反馈五项，本版全量修复：

**fix（bridge.query）：构造器 SELECT 在 mysql 误路由 LAST_INSERT_ID 分支 panic（Critical）**。
`op_db_query_build` 曾以 `rows && dialect == MySql` 判定两步取 id——SELECT 也是行集，
于是 mysql 上任何 `db.table(...).select()` 都进 exec 分支、`returning[0]` 空数组越界
panic，请求挂死（HTTP 000），**mysql 部署整体不可用**。执行路由抽出纯函数
`ExecRoute`（Rows / LastId / Affected），**仅 `insert + returning 非空 + mysql` 才进
LastId**；单测矩阵钉死「Select + mysql 必走行集」防回归。

**fix（seed）：语句前置注释行不再绕过方言改写**。`;` 朴素切分后 chunk 以 `--` 注释打头时，
`translate_insert` 的「INSERT 起头」判定落空，`INSERT OR IGNORE` 原样下发 mysql → 1064。
改写判定前剥前置 `--` 行注释（命中改写输出剥注释语句，未命中原样透传）；单测覆盖多注释行、
尾随注释保留、纯注释 chunk。

**fix（sample）：search 短词 LIKE 回退统一 `ESCAPE '!'`**。`ESCAPE '\'` 在 mysql 默认
sql_mode 下反斜杠吃掉收尾引号 → 1064（pymysql 服务端实锤；HTTP 层此前被上项 panic 遮蔽，
修复后会以 500 暴露）。`likeEscape` 同步转义 `!`/`%`/`_`（`!` 双写），三方言单一代码路径，
对 `NO_BACKSLASH_ESCAPES` 免疫。

**fix（bridge.stream）：`json.stream().write()` 兑现二进制宽契约**。JS 分发器曾把
`ArrayBuffer`/类型化数组/`DataView` 直传 `op_stream_write_bytes`，而 deno_core `#[buffer]`
只认 `Uint8Array` → TypeError（手册 0.1.65 已承诺宽契约、实测仅 `Uint8Array` 可用）。
分发层转换后进 op（ArrayBuffer → `new Uint8Array(c)`；view → 按 buffer+offset+length
取 `Uint8Array`）；桥接测试钉死四种形状逐字节无损 + string 仍走文本通道。

**fix（config）：订正 seed 方言范围 stale 注释**。`db` 段注释曾写「seed 仅 default 为
sqlite 时重放」，与实现（三方言均重放）相反——下游曾据此误列炸点。同步订正 devkit
api-manual 同款矛盾陈述。

## v0.1.65

**feat（bridge.stream）：二进制流写出 + 文件名响应头**。下游（如 wiki 附件下载）可经
`json.stream` 流式回源**字节**，不再被迫 302 到免鉴权 blob 路由。`write` 按入参类型分发：
`Uint8Array`/`ArrayBuffer`/类型化数组走新增 `op_stream_write_bytes`（通道本就是
`UnboundedSender<Bytes>`，原 `op_stream_write` 仅收 UTF-8 文本导致二进制失真）；其余仍是
UTF-8 文本（CSV/JSON/SSE 行）。`json.stream({ fileName })` 写入 `content-disposition:
attachment; filename*=UTF-8''<encoded>`，浏览器另存为原名而非安全化名。

**feat（contract）：`.schema` 白名单放开 `default`**。`default` 是标准 JSON Schema 注解关键字、
无校验语义，加入 `SUPPORTED_KEYWORDS` 后 handler 可声明 `default` 而装配期不再 FAIL FAST；未知
关键字仍硬失败（静默跳过会让声明成谎话）。应用层缺省钳制与之并存，不冲突。

**fix（sample._platform.search）：短词跨方言一致 + FTS 触发器陷阱根除 + mysql 高亮 N 查询**（下游 vendor 源）。
- 短词（<3 字符）回退统一为共享 `likeFallback`：trigram/ngram/tsvector 对短词静默零命中，现三
  方言都对基表 `LIKE`（复用 `likeEscape`）回退，消除方言行为分叉。
- sqlite `index()` 由 `INSERT OR REPLACE` 改为「真 DELETE + INSERT」：前者在 `recursive_triggers=OFF`
  （默认）下隐式 DELETE 不触发 AFTER DELETE 触发器 → FTS 虚表留旧词（实测复现）；后者 DELETE 必
  触发 `fts_ad`、INSERT 必触发 `fts_ai`，三方言安全，移除 footgun。
- mysql 高亮分支由「每命中、每字段一次 SELECT」改为**单次批量 `IN` 取回全部命中行原文**，再走
  现成的本地 `mark()` 截片段（sqlite/pg 本就引擎高亮/本地 `mark`，mysql 无内置 snippet）。消除大
  窗（如 limit=1000 → 千×字段数 次查询）塌方，且任意窗口高亮仍可用，不牺牲功能。

## v0.1.64

**feat（api.ts）：嵌套 handler 组**。default 导出里**非动词键 + 对象值**即嵌套 handler 组
——一个文件挂多个同动词端点不再必须拆子目录；同时**消灭静默丢弃**：声明了却注册不上的
导出键（非动词函数/数组/基元、动词键挂非函数）在启动/`oj build` 期可操作报错。

- 组名即路径段（与目录镜像同构），组可再嵌子组（上限 4 层）；组级 `.route` 作整组
  默认前缀，动词自身 `.route` 优先覆盖（绝对 `/` 开头挂 base 根下，否则相对本层前缀）。
- 判别红线：**先按键是否 verb 分派，再以 `typeof` 校验值**——动词键必须是函数
  （`.route`/`.schema` 等属性一律读作元数据，不拆子路由）；非动词键必须是普通对象。
  组对象的字符串 `.route` 是本层前缀声明（`route` 为保留键）。
- 组级 `.schema`（与组级 `.route` 同等地位）：组对象挂 `.schema` 作整组默认入参契约，
  组内动词无自身 `.schema` 时继承之，动词自身 `.schema` 在其上**合并**（同通道 `required`
  取并集、`properties` 动词逐键覆盖、其余通道级关键字动词优先）；合并发生在内省期，落到
  `RouteRow.schema`（`oj openapi` 与运行期守卫同源），非法契约 `server.schema_validation`
  装配期硬失败。`schema` 因此同列 `route` 为组级保留键。ambient 类型 `Object.schema` 同步放开
  （`global.d.ts`）。
- 防护：JS 内省 driver 内 `WeakSet` 路径环检测（`a.self=a` 内省期即失败，不拖死启动）
  + 深度上限 4；段名禁 `/` 与 `__proto__`/`constructor`/`prototype`，内省期权威校验、
  运行期 driver 与 `from_entries` 双保险（手改 `routes.js` 注入 → release 启动失败）。
- 派发：`Entry`/`Lookup::Hit`/`RouteRow` 携带点分 handler 链，`run_module` driver 逐段
  `Object.prototype.hasOwnProperty.call` + 计算式取值（连字符组名 `user-list` 可命中）；
  入参契约键同步改 (文件, 链)——嵌套组各 verb 契约互不串门。
- `routes.js` 为嵌套行写 `handler` 字段（顶层动词行不写，**旧产物无此字段照常启动**，
  以 `method` 兜底填链）；dev 内省与 `oj build` 共用同一递归 driver，双态零分歧。
- 行为变化：handler 缺失（含顶层方法未导出）由 405 改为 404「no handler」；「存在但
  非函数」仍为 405。
- 设计文档：`docs/superpowers/plans/2026-09-24-nested-handler-groups.md`（三方专家评审
  结论已并入 §八）。
- 示例与选型：`sample/src/nest/api.ts`（实体为根：`asset` 组含 `upload` 子资源、
  根级实体组 `.route`、连字符实体名；L1 用例 `sample/tests/nest.test.ts`）；
  「何时采用（拆目录 / 嵌套组 / 平铺三档取舍）」见 api-manual §4 与场景 31。

## v0.1.63（未打标签）

**feat（oj serve）：配置热更新（v1）**。`server.reload_api: true` 时 `oj serve` 走
「永生 listener + 外层单一 fallback」形态，支持零停机热更：

- `POST {base}/_system/config`（空 body）触发热更；`GET {base}/_system/config` 看当前代际
  （`config_path`/`sha256`/`loaded_at`/`generation`）；`GET {base}/_system/health` 看健康 +
  代际（旧 `{base}/health` 仍兼容但告警）。
- 热更流程：归一（load_cfg → fold_cli_mounts 重放 → 证书/公钥路径重放 → admission_gate）
  → 幂等判定（归一落盘文件 sha256 同代 → `unchanged`，不装配不 swap）→ 段 diff 三清单
  （`applied` 本代生效 / `restart_pending` 沿用旧池重启后生效 / `kept_listener` listener 级沿旧）
  → 影子装配（**复用首代连接池与插件 vtable**，不重建后端、迁移强制 verify-only、不重放 seed）
  → 任务池先建新池 fail-fast 再停旧池 → `watch` 通道整体 swap 内层 Router，旧代随在途请求自然 drain。
- 守卫：`auth:` 配置了 → 需 Bearer 验签（401）；未配 → 仅回环可调（非回环 403）。并发
  `POST` 拿不到串行锁 → 409 `reload in progress`。非空 body → 400。
- 失败不破坏在跑实例：装配失败返回 400 + 原因，旧代继续服务；新任务池建不起来则旧池仍跑。
- 证书 watcher 进程级单例：按「证书路径|公钥路径」去重，反复热更不再累积 watcher 线程 /
  inotify fd（spec §6 边界 5）。
- 设计文档：`docs/superpowers/specs/2026-10-12-config-hot-reload-design.md`（含 §3 外层
  fallback 分流、§5 全流程、§6 段 diff 矩阵、§9 边界清单，经双专家对抗复核 + 第二轮修订）。

## v0.1.62（未打标签）

**perf（bridge）**：请求热路径三项优化（基线与前后对比见 `docs/perf/baseline-2026-10-12.md`，
锚定快照 `docs/perf/data/2026-10-12.txt`）。

- **http 快照 memo**：`http.*` 属性访问此前每次全量深拷贝 RequestInfo（headers/query
  clone + 整个 body 重解析，实测 11.6µs/次）——现按请求代数命中 memo，每请求至多一次
  真快照（实测 −98%）。行为语义不变；`db.asTenant` 等请求中途改 `req` 的操作会失效
  memo（快照仍即时反映）。
- **模块执行 driver 常驻化**：每请求编译一次性 side-module driver → bootstrap 常驻
  `__ojRun`（AsyncFunction，`"use strict"` 前缀对齐 module 严格语义）。单次模块执行
  20.5µs → 4.23µs（−79%）；isolate module map 不再随请求数无界增长。错误信封保持
  `类名: 消息` 形态（`NotCapable: …` 等）。
- **release 模式免每请求 stat/canonicalize**：driver spec 每路径首请求冻结（dev 热重载
  语义不变；release 下改文件本就须重跑 `oj build`）。

**perf（serve）**：请求路径去重复功——鉴权头 map 单次构造（此前开 auth 时字符串化两遍）；
静态资产 ETag(mtime-len) + `If-None-Match` → **304**（免读盘免传输；HTML 走 meta 注入
不参与，缓存策略归 `server.html_cache_control`）；`html_meta` JSON 进程缓存 + 异步读
（此前每 HTML 请求在 async worker 线程同步读盘）；无注入页面零拷贝。

**perf（tasks）**：worker 对齐循环免每 5ms 每任务全量 TaskEntry clone；事件日志内存计数
替代逐条 stat、BufWriter 自然刷盘替代逐条 flush。

**chore（ci）**：release 流水线 publish-npm 等待 package 成功（根除首发 run 空 dist 竞态）。

**chore（bench）**：新增 `js/run_module`、`js/http.access` 基准补真实路径盲区；
`scripts/perf-baseline.sh` 导出中位数快照供跨版本对比。

本版无新增 JS API / 报错文案 / 限制变化——`docs/devkit/` 四件无需对齐（http memo、
log 级别短路、ETag 对 JS 面透明）。

## v0.1.61

**feat（db）**：JS `db.dialect()` 报连接引擎名（`"sqlite" | "mysql" | "postgres"`）。供
`_shared` 共享模块与诊断分派方言 SQL；`db.tx` 回调对象同面（并带 `isTx: true` 标记事务域
句柄——DDL 必须拒绝在此类句柄上执行，见 search）。业务 handler 分支方言 = 放弃跨引擎
可移植承诺，慎用。

**feat（versions）**：内容版本化原语 `sample/src/_platform/_shared/versions.ts`——CAS
内容表（sha256 去重，CHAR(64) PK）+ 修订表（`UNIQUE(entity_type, entity_id, version)`
乐观锁兜底）+ 行级 LCS diff（格子预算 4M 闸）。`commit/get/history/diff/rollback`；
并发双 commit 与 `expect` 撞版本统一 409 `version conflict`；在调用方 `db.tx` 内调用即
同事务。建表迁移 `_platform/migrations/0002__versions.{sql,mysql.sql,pg.sql}`（MySQL
TEXT 不可作 PK，故走手写方言迁移而非 schema.yaml）。

**feat（search）**：引擎原生 FTS 统一面 `sample/src/_platform/_shared/search.ts`——
sqlite FTS5(trigram) / mysql FULLTEXT(ngram，≥5.7.6) / postgres tsvector+GIN(PG12+)。
`ensureIndex`（DDL，进程内每表一次，**禁止入 db.tx**——`tx.isTx` 直接拒绝）/
`index`/`remove`（业务表 upsert/delete，绝不直写 FTS 对象）/`query`（绑定参数 + 标识符
白名单 + 高亮：sqlite `snippet()`、pg `ts_headline()`、mysql 应用层截窗）。sqlite 短词
（<3 字符）自动回退业务表 LIKE；FTS 短语引号包裹防语法注入；pg 占位符 `$n`。

**feat（sso）**：HTTP 表单 / CAS SSO 登录来源。config 顶层 `http_sso:` 段（map，键 =
提供者名）+ JS 全局 `sso.providers`（段缺省报 `http sso not configured`，同 oidc 语义）；
装配期校验（kind 白名单 / form 必 `success_when` / cas2 必两 URL / 名字禁 `:` / 非 https
缺省拒绝、`allow_http: true` 显式放行）。参考实现 `sample/src/sso/`：form 转发凭据
（`body_encoding: form|json`，判定 `success_when` 点路径，网络错 502 / 判定失败 401 分态）
与 cas2 302+ticket（XML 命名空间容忍正则）；JIT 键 `sso:<provider>:<tenant>:<username>` +
`"!sso"` 占位 hash（该号永不可密码登录）；CAS 交接红线：token 不进 URL（httpOnly 双
cookie）、302 目标白名单（`/` 开头非 `//`）。登录端点与 302 腿都进 `auth.` / `tenant.`
两条匿名列表。`HttpSsoProvider` 手写 `Default`（derive 会绕过 serde 缺省函数，Rust 侧
直接构造会得到空字段名的坏配置）。

**docs（devkit）**：四件同步——api-manual（`db.dialect` / `sso.providers` 全局、第 10 章
「sso」配置段、排障表 7 行、v0.2 限制表 FTS 边界）、scenarios 场景 28 重写（分层事实 +
JIT 占位 hash + CAS 交接红线）与新增场景 29/30、SKILL 路由与陷阱行、README 索引。

**test（sso）**：独立 e2e 目标 `oj/tests/sso_e2e.rs`（隔离 oj-auth OnceLock）：form
JIT 登录 + cas2 ticket 回校 + provider 未配置 400 + 坏票 401（mock 上游为项目内业务模块）。

**test（L1）**：`sample/tests/{versions,search}.test.ts`——versions 四用例（提交/取回/
diff/回滚、乐观锁 409、CAS 去重、事务回滚一致性、大文档闸）；search 三用例（增删改检索
一致、短词回退、DDL 拒事务、标识符白名单）。

## v0.1.60（未打标签）

**feat（exec）**：`oj exec` 子命令不再要求 `config.yaml`（未找到时静默回落内置默认，不再向
stderr 打 `note: no config.yaml found …`；与 `oj info` 同口径，仅 `serve`/`migrate`/`openapi`
等依赖后端配置的场景保留该提示）。同时 `oj exec -e '<expr>'` 与 REPL 默认回显单表达式的求值
结果（devtools 风格）：仅当整段是单个顶层表达式语句时才包裹 `console.log((<expr>));` 输出其值
（如 `oj exec -e '1'` 输出 `1`、`oj exec -e '({a:1})'` 输出 `{"a":1}`）；多语句 / 声明 / 已显式
`console.log` 等场景不回显，避免污染输出。

**chore（ci/npm）**：npm 发布鉴权切到 **Trusted Publishing（OIDC）**。npmjs 账号启用
Two-Factor Authentication 后，CI 的非交互发布只剩两条路——勾了 Bypass 2FA 的 granular
token（长期凭证，要轮换；包级若设「disallow tokens」直接不可用）或 OIDC；本仓选后者：
`publish-npm` job 加 `permissions: id-token: write` 并把 setup-node 提到 Node 24
（npm ≥ 11.5.1），由 npm CLI 用 GitHub Actions 的 OIDC id-token 现换短时发布凭证，
仓库不再存 `NPM_TOKEN`（该 secret 已可删）；`npm whoami` 步骤随之删除——OIDC 下没有长期
身份可问，官方亦明确它不校验 trusted publishing 权限。`scripts/npm-publish.sh` 新增
**§1b 预检**（先于一切装配动作）：npm CLI < 11.5.1 即 fail、无 GitHub OIDC 环境变量即
fail、检测到 `NODE_AUTH_TOKEN` 即 fail（npm 会在 OIDC 失败时静默回退到 token，配错就
永远暴露不了）。配套：包元数据 `repository`/`homepage` 随 GitHub 仓库更名订正为
`everpan/oj-bin`（官方强校验 `repository.url` 须与发布仓库完全一致，不一致则 publish
失败）；`npm-publish.test.sh` 自检补到 7 例（新增三项门禁 + npm 版本边界 11.5.1 放行）。

**fix（ci/smoke）**：`smoke-npm` 装包前先用 `npm view` 等 registry 可见（主包 + 本 triple
子包，每包最多 10×20s），再 `npm i`（retry 3×30s）。原实现只有 install 的 3×20s retry：
v0.1.59 发布时 Windows 腿连吃三个 `ETARGET`（主包 15:54:16Z 才落 registry，Windows 腿
15:54:03Z 就开装，命中尚未同步的 CDN 节点）——这是「本 runner 视角还没这版」，不是包没
发出去；现在先等可见再装，并把两种失败（等不到可见 vs 已可见却装不上）的报错分开。

**一次性手工前置**（只有人能在 npmjs.com 做）：为 `@oj-bin/oj` 与三个
`@oj-bin/oj-<triple>` **各配一遍** trusted publisher（per-package）——GitHub Actions /
`everpan/oj-bin` / workflow `release.yml` / 允许 `npm publish`；配置保存后 npm 不校验，
且**须在 2 天内完成首次成功发布**，否则配置失效需删了重建。详见
`docs/superpowers/specs/2026-09-11-npm-publish-design.md` §7。

## v0.1.59（未打标签）

**fix（cli）**：`oj exec` / `test` / `migrate` / `schema diff` / `openapi` 缺省目录探测
（无 `-d`、无 api 挂载时向上搜 src/dist）只认「像业务树」的候选——首层 `*/manifest.yaml`、
`manifests.yaml`（release 锁）或空树；源码仓自己的 `src/`（首层子目录无 manifest）被跳过，
裸 `oj exec` 不再在源码仓根爆 `module 'bridge' missing manifest.yaml`；全程无合格候选 →
回落 `.oj-no-api` 占位目录（必然不存在，模块扫描 = 零模块）并打 note 指路 `-d`/`-c`——
exec（纯脚本不需要表白名单）照常跑，test/migrate/schema diff 由各自的目录存在性检查给出
明确报错。**`oj exec` 不需要 config.yaml**（v0.1.57 起 `-c` 可省，纯计算脚本零配置可跑）。
文档同步：exec 章明确该契约，user-manual §3.1 增补完整「改前 → 改后」迁移示例。

**chore（plugins）**：移除 `oj info` / `ojInfo()` / `GET {base}/plugins` / JS `plugins()`
输出里恒空的 `unknown_axes` 字段（v0.1.54 引入的「宿主升级窗口信号」零下游消费——泛型轴
通道承接一切合法新轴，typed 未知名在装配期 stderr 告警后忽略，告警保留）。插件自省面
键集收敛为六键：`{name, semver, abi_version, fingerprint, description, host_abi_version}`。

## v0.1.58

**breaking（config/serve）**：站点配置删除旧键，升级为顶层 `mounts:` 扁平挂载表——
一行 = 一个 URL 前缀 + 一个目录（api/web 恰好其一），支持多 API 目录与多静态站点。

- **五个旧键删除**：`server.api_prefix`（含旧键名 `base`）/ `server.app_path` /
  `server.app_prefix` / `server.static_sites` / `server.app_spa_fallback`。config 里
  出现任一个 → **启动报错并输出迁移后的 `mounts:` YAML**（按「旧写法 → 新写法」逐键
  两列，`app_spa_fallback: true` 加到每一个 web 条目——旧版它是全局开关；绝不静默迁移）。
  迁移对照见 `docs/user-manual.md` §3.1。
- **`mounts:` 语义**：按前缀**最长命中、跨挂载不回落**；同前缀一条 api + 一条 web 可配对
  （api 优先，配对 web 无 SPA 回落），同类重复报错；值空串报错；`spa: true` 要求同前缀
  无 api 挂载；允许根 `/`；web 挂载仅 GET/HEAD（**其余方法由旧版「落入 404」改为显式
  405**）。目录 canonicalize 逐条目 fail-fast（api 挂载目录缺失不再静默空路由）。
- **内建端点零 URL 变化**：`/health`、`/plugins`、`/blob/{key}` 依旧注册在 api 挂载前缀
  下（今日即 `{base}/...`，base 默认 `/v1/api`），逐 api 挂载各注册一份。
- **SPA 回落改 per-挂载显式 `spa: true`**（默认 false）——继承 v0.1.20「静默把 404 变
  200 会掩盖错配」的立论；`path_under_base` 排除检查结构性消亡（api 前缀路径恒最长命中
  api 挂载）。
- **CLI**：`--api-path d [-b B]` → upsert `{prefix: B, api: d}`（B = `-b` > 默认
  `/v1/api`；**裸 `-b`（无 `--api-path`）报错**——挂载自带 prefix，无处附着）；`--app-path`
  裸 `d` → upsert `{prefix: "/", web: d}`、`prefix=dir` → upsert 同前缀 web 条目（保留
  headers/spa）。`oj test` / `test fixture` / `migrate` / `exec` / `openapi` 新增
  `--site <prefix>`：选 api 挂载（只用该树与其前缀）；命中 web 挂载报错并列出全部 api
  prefix；与 `-d`/`-b` 互斥。
- **装配**：多 api 树并集装配——迁移/seed/fixtures/schema 收敛逐 api 树执行；表名跨树
  重复沿用 S002 fail-fast；**模块名跨树重复改报错**（旧 `module_map.insert` 静默共存）；
  路由表逐挂载构建，`html_meta_handler` 按前缀定表，`GET {base}/plugins` 数据不变。

**feat(sample)**：`sample/config.yaml` 改写为 `mounts:` 形态（api `/v1/api` + web `/`
指 `dist`，URL 不变）；`sample/README.md` 命令与说明同步。

**docs**：`docs/user-manual.md` §3 重写挂载章 + §3.1 迁移小节；devkit 四件同步
（api-manual「mounts（v0.1.58）」节 + 错误/限制表、SKILL 陷阱速查、scenarios 场景 2、
README 版本行）。

## v0.1.57（未打标签）

**feat(cli)**：所有子命令的 `-c/--config` 改为可选，并统一配置搜索路径。

- 搜索顺序（省略 `-c` 时）：① CWD 逐级向上找 `config.yaml`（项目根或任意上级目录有一份
  即可）；② `$HOME/.oj/config.yaml`（Windows 回落 `USERPROFILE`，用户级兜底）。两处都没找到
  → 不报错，用内置默认 Config 继续（stderr 打 `note: no config.yaml found …`；config_dir
  回落 CWD，相对路径语义不变）。显式 `-c` 指向缺失文件仍 fail-fast。
- 无需配置即可运行：`oj build` / `oj openapi` / `oj exec`（纯计算脚本）/ `oj info` /
  纯静态 `oj serve`（`--api-path` + `--app-path` + 证书旗标俱足时）。需要后端的命令
  （`migrate` / `test` / `schema diff` / `fixture` 等）在用到后端时给出「未声明 default 库」
  类错误，不再先报「config 文件不存在」。
- `oj secret seal/open` 的 `-c` 缺省同样走统一搜索；`open` 私钥仍优先
  `OJ_SECRET_KEY` / `OJ_SECRET_KEY_FILE` 环境通道。
- `oj openapi` 漂移提示的 regen 命令不再强带 `-c`（未给定时）。

## v0.1.56（未打标签）

**fix（plugins）**：打包形态（deploy.sh / npm 启动器，glibc 发行包经 ld-linux 启动）插件发现与
daemon re-exec 不再依赖 `current_exe`。

- 插件发现新增**打包布局候选**：`OJ_BUNDLED_LD` 上溯两级得 `<bin>/plugins`，置于 exe 启发式
  之前；优先级 env > toml > 打包布局 > exe > workspace_root 不变，非打包形态行为不变。修复
  npm 发行包（0.1.55 实测，`ojm dev` 必现）零插件加载——插件文件齐全也报 fail fast
  「no auth plugin loaded」。
- `oj serve --daemon` 打包形态 re-exec 目标由 ld.so 改为 `<bin>/oj` 启动器（自带
  `OJ_BUNDLED_LD`/`LIB` 导出与 ld 链），不再把加载器当 ELF 加载导致秒退。

## v0.1.55

**fix（fs）**：Windows 下 jail 内 symlink 逃逸被放行（CI 缺陷钉 `fs_symlink_escape_denied` 暴露）。

- **根因**：相对路径此前在 JS 门面用 `root + "/" + p` 拼接——Windows 上 root 已
  canonicalize 成 verbatim 形式（`\\?\C:\...`），`\\?\` 前缀关闭分隔符归一，`/` 不再是
  分隔符，canonicalize 必然失败 → 回退路径只 canonicalize 了父目录（root 本身）再拼回
  叶名，**叶级 symlink 从未被解析**，root 内指向 root 外的 symlink 读取被放行。unix 拼
  接结果干净、canonicalize 直接成功并跟随 symlink，故仅 Windows 受影响。
- **修复**：相对→绝对的折叠移进门面 op `op_fs_resolve`（Rust `Path` 语义，跨平台正确；
  `..`/`.` 先对 root lexical 折平再 canonicalize，Windows verbatim 路径不归一 `..` 的
  问题一并消除）。JS 门面 `fs.*` 退化为透传；内部 op `op_fs_root` 随之下线。对外语义与
  错误文案不变（相对路径解析到 jail 根之下、越界 `NotCapable`）。

**fix（tasks）**：panic 补员与停机并发的 join 竞态（CI 缺陷钉
`given_worker_panic_when_respawn_then_live_recovered` 暴露）。

- 补员由垂死 Worker 线程内递归 `spawn_worker` 完成，替补句柄的 push 发生在 `spawn()`
  返回之后；`shutdown_and_join` 单次 `mem::take` 可能抢在 push 之前拿走句柄表 → 替补
  Worker 漏 join，`live` 停在 1（停机收场不等待仍在跑的 Worker）。现循环 take+join 至
  `live == 0`——stopping 置位后补员有界，必然收敛。

**feat（exec）**：`oj exec` 输出通道分离（管道友好）+ 裸 `oj exec` 缺省进 REPL。

- **输出通道分离**：`console.log`/`console.info` **原样**输出 stdout——不再带 `INFO`
  级别前缀（此前 `INFO   hello` 形态污染管道消费方），msg 内换行原样保留；
  `console.debug`/`console.warn`/`console.error` 与 `log.*`（zap 结构化日志，**含 info
  级**）一律走 **stderr** 并保留 `{LEVEL:<5}  ` 级别标签——诊断走日志通道（Unix 约定），
  不混进管道结果。`--log-file` JSONL 语义不变：全部级别照旧带级别标签落盘。实现上
  JS 侧 `log.*` 改走新增的显式 op `op_exec_log_err`（与 console 通道分离，不用 level
  魔数位编码）。
- **裸 `oj exec` 缺省进 REPL**：无 `<file>` / `-e` / `--repl` 时不再报错
  「需提供 <file> / --code / --repl 之一」，缺省进入交互式 REPL（真终端 rustyline，
  管道/重定向输入走普通回放）；`--repl` 与其他入口的互斥报错保持不变。

## v0.1.54（未打标签）

**feat（插件体系）**：插件轴清单自报 + 泛型轴通道（新轴零宿主改动）+ 插件自报配置键 + `oj info` CLI / JS `ojInfo()`。

### 插件作者向

- **轴清单自报（`oj_plugin_axes()`）**：入口宏 `oj_plugin_entry!` 现在双发——除既有的
  per-axis `oj_plugin_axis_<name>` 符号外，新增 `oj_plugin_axes()` 返回
  `RVec<AxisDecl>`（轴名 + kind + vtable）。宿主探测**自报清单优先**，清单缺失才回落
  逐轴 dlsym（打 deprecated 告警）。**旧插件（无新符号）无需重编即可继续加载**；
  但重编后须与宿主同批发布（`cargo xtask build` 联编），别混跑版本。
- **泛型轴通道**：宏新增显式泛型臂 `generic(name) => &VT`（`AxisDecl.kind=GENERIC`）。
  泛型轴**不占类型化轴槽、不加 `TYPED_AXES`、零 ABI 变更**——新后端轴（如 cache、
  queue）从此不需要宿主发版。JS 调用面：`axis(name).op(...args)`（args 经 BigInt-safe
  `ojStringify` 序列化，末位可挂 opts 对象）。**泛型轴名避开 9 个保留名**
  （es/db/blob/bus/kv/auth/mq/mail/ldap）——旧宿主（无 kind 概念的 pre-kind 宿主）按名
  cast，撞名会把 `GenericVtable` 误当类型化 vtable。
- **插件自报配置键**：宏 `config: "key"` 声明后，宿主 cfg 解析增加第 2 级——
  `plugins:<name>` 非空透传 → 顶层 `<key>` 段（全量 Value，段可选，未配置给 `{}`）→
  按名遗留臂。未被子报 key 或透传消费的未知顶层段，启动时打 `unconsumed config
  sections` 诊断（拼写错误 / 插件没装的早期信号），并可在 `oj info` / `ojInfo()` 里查。
- **开发模板**：新轴范式见 `tools/plugin-template`（命名三方对齐：crate 带 `oj-` 前缀、
  descriptor 名剥离；`config:` 声明；xtask 联编与独立 cargo 两路径构建；desc 里注明
  宿主最低版本）。

### 运维向

- **`oj info [-c config.yaml]`** CLI：phpinfo 风格诊断，五段纯文本——build（oj 版本 /
  profile / host triple / V8 / exe / config 路径）、abi（ABI_VERSION + 宿主指纹）、
  plugins（每个插件 name/semver/abi/desc + `unknown_axes`）、backends（声明面：db
  schemes、blob 有无、broker kinds、kv/auth/es/mail/ldap/mq 槽位）、config（段名清单 +
  unconsumed 段）。**只报声明面**：不 connect 库/broker，config 只出键名不出值。
- **JS `ojInfo()`**：与 CLI 同一装配体（`assemble_ojinfo` 单一事实源），handler 进程内
  自省；**无公共 HTTP 端点**（需要时自己包业务路由透出）。config 段同样只出键名。
- **oj-ldap 迁移泛型轴**（双轨期）：JS 调用面改为 `axis("ldap").bind/search/searchPaged/
  whoami/compare`（op 集与语义不变，实例选单经末位 opts 的 `key`，缺省 `default`）。
  宿主类型化 `ldap`/`LDAP` 全局保留，但装配迁移版插件后调用报
  `ldap not configured`（旧版 typed 插件仍可加载，二者择一）。
- 同名泛型轴多插件提供 → 装配期 fail-fast（泛型轴每名单提供者）。

**fix（npm 分发）**：主包 `@oj-bin/oj` 此前只靠 `postinstall` 把二进制落盘到
`<项目根>/bin/`，**未声明 `bin` 字段**，导致 `pnpm dlx @oj-bin/oj` / `npx @oj-bin/oj`
报 `ERR_PNPM_DLX_NO_BIN`（找不到可执行入口）。

- 新增 `npm/oj/bin.js` 启动器（零依赖 CommonJS）：与 `postinstall.js` 共用同一份
  `platform-arch → triple` 反向表，经 `require.resolve('@oj-bin/oj-<triple>/package.json')`
  定位平台子包后 exec 其 `oj`/`oj.exe` 二进制，转发 argv 并传播退出码；子包不可达
  （如 `--omit=optional`）时回退到与 postinstall 相同的「平台子包未找到」提示并退出 1。
- 主包 `package.json` 模板新增 `"bin": { "oj": "bin.js" }` 并把它列入 `files`；
  `scripts/npm-publish.sh` 主包装配步骤同步拷入 `bin.js`。
- 文档对齐：`npm/README.md`、`docs/ops-manual.md` §1.1、`docs/devkit/README.md`、
  `docs/devkit/api-manual.md` 安装小节补 `pnpm dlx @oj-bin/oj` / `npx @oj-bin/oj`
  一次性运行说明；设计 spec `2026-09-11-npm-publish-design.md` §2.3 补 `bin` 字段与
  dlx 行为，§3 已知缺口补 dlx 兜底路径。回归测试 `npm/oj/test/bin.test.js`（argv 转发 /
  退出码传播 / 缺子包）全绿。

## v0.1.53 —— JS handler 本地文件读写（`fs` 全局）（未打标签）

**feat**：移植 Deno 官方 `deno_fs` 扩展（版本锁对齐 deno_core 0.411；`deno_io`
一并注册供其 read/write 原语），为 JS/TS handler 提供本地文件系统能力：

- 新增 `fs` 全局（9 个一次性 async API）：`readFile` / `readTextFile` /
  `writeFile` / `writeTextFile` / `mkdir` / `remove` / `rename` / `stat` /
  `readDir`。**不暴露 fd 句柄**（op 内自开自关，池化 runtime 无跨请求 fd
  泄漏面）；大文件请走 `blob`。
- config 新增 `fs:` 段：`root`（jail 根，相对 config 目录解析，缺省 `data`；
  不存在/非目录启动 fail-fast）+ `readonly`（写 API 全拒）。**段缺省 = 不启用**
  （fail-closed：`fs.*` 抛 `NotCapable`）。
- 安全模型双层：门面层 `op_fs_resolve` best-effort canonicalize + root 前缀
  裁决（封 `..`/绝对越界/symlink 逃逸）；deno_permissions 容器 read/write
  轴收窄到 root、其余轴（net 等）放行不影响 fetch/WS 出站。
- 相对路径解析到 jail 根（deno 原生语义是进程 cwd，门面层改写）；
  挂载 `TextEncoder` / `TextDecoder` 全局（30_fs.js 文本助手依赖）。
- dev / release / `oj test` 三路径一致可用；`oj build` 零处理。

文档：`docs/plugins/fs.md`（归属索引，核心内置非插件）；devkit 四件对齐
（api-manual §6 `fs` 章节 + §7 错误表 + §10 配置表 + 总表 25 组；SKILL.md
陷阱 4 条；scenarios.md 场景 26；`sample/global.d.ts` 增 `FsApi` 类型）。
测试：单元 7（往返/逃逸/readonly/symlink/池复用/授权构造）+ e2e 2
（读写回环、readonly 写拒绝）。

## v0.1.52 —— 全库评审修复（鉴权 / panic 恢复 / 资源健壮性）（未打标签）

**动机**：四路架构评审（核心运行时 / CLI+HTTP / 插件体系 / 文档）产出的 P1/P2 问题
批量修复，每个修复带防护测试；同时补全 `docs/user-manual.md` §3 配置参考缺口并做
全库文档对齐。本版**无新 API**，devkit 四件按行为变更逐条对齐（见各条 v0.1.52 标注）。

**fix（鉴权）**
- 任务控制面 PATCH/DELETE 守卫动词失真：`admitted` 硬编码传 `"GET"` → 传真实
  method。此前按方法区分权限的 oj-auth 守卫会被只读凭据绕过或误拒。
- 任务日志先截断后过滤：busy 任务刷屏时 quiet 任务查日志恒 0 条 → 改为全窗过滤后截断。

**fix（panic 恢复与停机）**
- WS worker 帧中 panic 后同连接排队帧永久挂起：恢复路径新增 `Scheduler::panic_conn`
  （复用 drop_conn 作废排队帧 + 清 in_flight），后续帧收到错误而非无限等待。
- task worker panic 不补员：对照 frame_pool 补齐 respawn（停机中不补）——long 任务
  不再随一次 worker panic 静默死亡到重启。
- `run_once` 无视停机：`stopping` 置位后拒绝新的一次性任务（明确 Err），排空循环
  丢弃待执行项——长 cron 脚本不再无限期阻塞 shutdown；在途作业走既有 grace 看门狗。
- ws-retire 线程堆积：空池 detach 用 `retiring` 标志去重，高抖动 + 长 linger 下
  不再每次叠一个 sleep 线程。

**fix（资源与健壮性）**
- `await_ffi` 忙旋：前 64 次 poll `yield_now` 快路径，之后 500µs 退避——db/es/blob/kv/bus
  慢调用不再单核空转（mail/mq 的 sleep 版语义不变）。
- `DELIVER_TARGETS` 锁毒化自愈：`unwrap_or_else(into_inner)`，一次毒化不再连锁
  panic 插件线程。
- HTTP 状态对超大 `code` clamp（此前 `as u16` 按位回绕成错值）；信封 body 保留全精度。
- SQL 守卫 `sql_memo` 缓存加 4096 上界（超界清空，正确性无损）。
- `ws.broadcast` 支持二进制（ArrayBuffer/Uint8Array → 新 op `op_ws_broadcast_bin`），
  与 `ws.send` 对称；非法入参报错而非发 `"[object Object]"` 帧。
- `db.tx` rollback 本身失败不再吞业务异常（始终抛 handler 原始错误）。

**fix（CLI）**
- `oj test` 递归扫描 `tests/` 子目录（`_` 前缀目录跳过）——此前子目录 `*.test.ts`
  静默忽略。
- fixtures 幂等门禁：非幂等 INSERT（判据同 S006）在 `oj test fixture` 灌入前报错；
  此前注释声称「幂等可重复灌」但无检查强制。
- `oj openapi` dev 分支内省失败对齐 release 语义 fail-fast——`--check` 不再被
  静默缺路由误报漂移。
- cron 到点重读注册表：PATCH 改表达式/停用不再被入睡前的旧快照覆盖一次；
  任务 probe 失败打 warn 点名任务与原因（行为不变仍退避重启）；
  任务目录扫描跳过符号链接（自引用符号链接死递归防护）。
- ldap 双源错误文案续行修复（不再夹长串空格）；xtask `find_crate_dir` 排序取
  首个命中（结果确定性）。

**fix（插件）**
- **oj-db-postgres：typed 流式路径静默 null → 响亮报错**（`undecodable_column`，
  含列名+类型+cast 线索），对齐 mysql「绝不静默 null」纪律；流式遇错 yield Err 终止。
- oj-db-mysql / oj-db-postgres：dialect 未知/closed handle 上送 `"unknown"` + 告警，
  不再冒充 `"sqlite"` 误导宿主方言选择。
- oj-blob-s3：`upload_url` 信封改 serde_json 构造——URL 含非 ASCII 时不再产出
  `\u{…}` 非法 JSON。
- oj-es：reqwest `connect_timeout(5s)` + `timeout(30s)`——ES 黑洞不再永久悬挂
  插件任务与 socket。
- oj-auth：二次 init 换 cfg 时经 host.log 打 warn（沿用首份），不再静默忽略。
- 8 个插件的 tokio runtime 统一 `.worker_threads(2)`（16 核机常驻线程 ~130 → ~20）。

**docs**
- `user-manual.md` §3 配置参考补全：`auth` / `cors` / `secrets` / `vars` / `db_query`
  / `static_sites` 六段 + `server` 块 v0.1.27/30 项（多站点、上传上限、SPA 回落、
  html meta、`response_headers`、`route_timeouts`）。
- 全库文档对齐：`server/` → `serve/` 改名（2026-10）、ABI_VERSION 11、AXES +ldap、
  CLI 子命令全集（exec/migrate/schema diff/secret/openapi）、S001–S008 检查编号、
  全局对象表补 mail/ldap/tasks/vars/Kafka/RabbitMQ；devkit 四件同步（broadcast 二进制、
  oj test 递归、fixtures 门禁、openapi fail-fast、tx rollback 语义、code clamp）。

## v0.1.51 —— SQL 执行追踪（dev 日志 + 运行时画像）（未打标签）

**动机**：开发与排障时看不到 SQL 到底跑了什么、跑了多久。补一层**单一内部 recorder**：
所有 SQL（池路径 `db.query/exec/stream` 与事务路径 `tx.query/exec`、`db.nextSeq`）都在
op 层计时并产出事件，同时喂给两个出口——dev 结构化日志与每请求运行时画像。

- **dev 日志（默认开）**：每条 SQL 打 `target="oj::sql"` 的结构化日志，含 `sql` / `db` /
  `ms` / `rows` / `tx`（是否事务）/ `src`（模块名）/ `status` / `err`。release 默认关，
  可由 `db_trace.enabled: true` 强制开。
- **`db.sqlProfile()`（新增 JS 原语）**：返回本请求画像快照
  `{ count, totalMs, slow:[{sql,ms,db}], byDb:{db:{count,ms}}, events:[{sql,params,db,inTx,source,ms,rows,ok,error}] }`，
  handler 想看时自己调（性能汇总、慢查询速览、逐条回放）。
- **dev 信封 `_sql`**：dev 追踪开启且本请求有 SQL 事件时，`json.ok/fail` 的信封追加兄弟字段
  `_sql`（画像快照），裸 `curl` 即可见，生产追踪关闭则信封不变。
- **参数默认脱敏**（`db_trace.redact_params: true`）：只记「参数个数」，**绝不**把密码/手机号
  打进日志或画像；本地排障可设 `false` 显式打开明文（值经 JSON 序列化）。
- **慢查询阈值**（`db_trace.slow_ms`，默认 0 = 全记）：仅过滤 dev 日志与画像里的 `slow` 列表；
  画像 `events` 始终记全量。
- **配置段 `db_trace`**（顶层，不能塞进 `db:` —— `db` 是 name→DSN 的 map）：
  `enabled`（缺省 = dev 自动开 / release 自动关，可强制）、`redact_params`、`slow_ms`、`to_log`。
- **零开销默认**：release 或 `enabled:false` 时 recorder 直接返回，不计时、不分配、不写日志。
  现有 `Bridge` 直接构造（测试/exec）默认关闭追踪，不受影响。

### fix

- **`oj --help` / 子命令 `--help` 说明过长导致换行错乱**：`oj/src/args.rs` 里 clap 实际渲染的
  帮助文本普遍偏长（单条说明多行、资源 profile 旗标 `--redis/--blob/--es/--broker/--kafka/
  --rabbit` 12 处重复长句），在窄终端下缠绕、版式混乱。统一精简为单行或极短两行：
  serve 的 `--api-path/--app-path/--console-log/--daemon`、migrate/test/schema diff/exec 的
  `--dir/--db`、`test`/`exec` 子命令简介，以及 12 个资源 profile 旗标（表述压成
  `<源> profile（config.<段>；缺省 default；未声明 fail-fast）`）。仅注释文本变更，无功能/行为变化。

## v0.1.50 —— `oj exec` 内联代码与交互式 REPL（未打标签）

**动机**：`oj exec` 原先只能执行磁盘上的 `.ts`/`.js` 文件，临时调试、一次性求值、管道/CI
里嵌一小段代码都得先落盘。补齐两类轻量入口：**内联代码**（`-e/--code`）与**交互式
REPL**（`--repl`），三者与 `file` 互斥、必选其一。

- **`-e, --code <code>`（内联代码）**：直接执行字符串里的 TypeScript（JS 子集亦合法），
  不落盘。后端全局注入、`--db/--redis/.../--log-file`/`-- arg...` 等旗标与文件模式完全一致；
  代码以 `file:///oj-eval.ts` 合成 specifier 走 side-module（TLA 保真），**不能含相对
  import**（无基准目录，内联代码应自包含）。
- **`--repl`（交互式 REPL）**：逐行读 stdin，每行以 TS 转译后独立求值，同一 isolate 内
  后端全局（`db`/`kv`/`blob`/`console`/`log`/...）全部可用；Ctrl-D / Ctrl-C 退出，退出码 0。
  每行是一个独立模块、顶层绑定作用域隔离，**跨行共享状态需显式挂到 `globalThis`**
  （如 `globalThis.x = 1`）；与内联代码同理，REPL 行内不支持相对 import。真终端下由
  **rustyline** 接管原始终端（方向键 / 行内编辑 / ↑↓ 翻历史），不再把 `\x1b[A` 等转义
  序列原样回显成乱串；管道 / 重定向输入（CI、测试、文件回放）走普通 BufRead。
- **互斥与校验**：`file` / `--code` / `--repl` 归到同一 `ArgGroup`（`multiple = false`）——
  两两组合（如 `file + --code`、`file + --repl`）由 clap 报错；三者皆无则由 `run()` 报
  `exec: 需提供 <file> / --code / --repl 之一`。
- 装配/迁移门禁/扩展顺序与既有 `oj exec` 文件模式完全同源（`build_runtime` 抽出共用，
  `boot_if_set` 补跑 ext_boot）；退出码语义不变（settle 0 / 异常·加载失败 1）。
- **全仓重命名 `server/` → `serve/`**（crate 名与目录名同步，`server_cmd.rs` →
  `serve_cmd.rs`、`ServerArgs` → `ServeArgs`；config 键 `server:` 不变）。开发者影响：
  `cargo test -p server` → `-p serve`；对外行为与产物布局不变。

## v0.1.49 —— fixture 归入 test 子命令（未打标签）

**动机**：`fixture` 仅为测试服务的演示数据灌入，原作为顶层子命令与 `test` 平级，语义上
属于测试范畴。把它收进 `oj test` 之下，CLI 层次更贴合「测试相关动作都在 `oj test` 之下」。

- **`oj fixture` → `oj test fixture`**：`fixture` 由顶层子命令改为 `test` 的子命令，
  用法与旗标完全保留（`-c/--config`、`-d/--dir`、`--db`、`[module]` 位置参数不变）；
  `oj test`（不带子命令）行为不变，仍跑 `*.test.ts`。
- 旧的顶层 `oj fixture` 已移除，调用会报 `unrecognized subcommand 'fixture'`。
- 装配路径不变：`oj test fixture` 仍走 `migrate_cmd::run_fixture` 的**瘦身装配**
  （config → 插件 → 开库，无证书门禁），与 `oj migrate` / `oj schema diff` 同源。
- 文档同步：`docs/cli2.md`、`docs/user-manual.md`、`docs/migration.md`、
  `docs/modules/04-oj-cli.md`、`docs/modules/02-config.md`、`docs/modules/07-data-layer.md`、
  `docs/db-guide.md`、`docs/devkit/api-manual.md`、`sample/README.md` 及源码注释中的
  `oj fixture` 引用全部更新为 `oj test fixture`。

## v0.1.48 —— 测试稳定：`oj openapi` 用例临时目录不再互踩（无功能/行为变更）

- **临时目录不再互踩**（v0.1.47 后修复，纯测试基建，无用户可见变更）：
  `oj/src/openapi_cmd.rs` 的 `synthetic_table` / `table_with_contract` 与两处独立用例都按
  **进程 pid** 拼临时目录（`oj-oa-syn-<pid>` 等），同进程内并行跑时一方的
  `remove_dir_all` 会打断另一方的 `create_dir_all` —— 表现为
  `check_detects_no_drift_and_drift` 偶发 panic（单跑必过、全量跑时随机红）。改为
  `unique_tmp(prefix)`（pid + 原子序号，每调用一个目录），并补防回归用例
  `synthetic_tables_use_distinct_temp_dirs`。

## v0.1.47 —— blob 服务端搬运与区间读：`blob.copy` / `blob.move` / `blob.readRange`（ABI 11）

**动机**：大文件（v0.1.38 起流式直落 blob）在 **JS 侧收尾**时代价全在桥上——把临时对象
「转正」要 `blob.get` + `blob.put`（整份字节进 V8，峰值 2× 文件大小），而嗅探文件类型
只需前 4KB。三件事本该由后端服务端完成，不该让字节过桥。

- **`blob.copy(src, dst)`**（src 保留）：`BlobBackend` 增 `copy`，local = `fs::copy`、
  s3 = CopyObject；默认实现回落 `get` + `put`。
- **`blob.move(src, dst)`**（src 不再存在）：trait 增 `move_to`，local 同父目录 =
  `fs::rename`（原子、零字节搬运）、跨目录 = copy + unlink，s3 = CopyObject + DeleteObject；
  默认实现回落 `copy` + `del`。`src == dst` 两者均为 no-op（`fs::copy` 同文件语义未定义，
  会截断文件，故显式挡掉）。
- **`blob.readRange(key, offset, len)`**：trait 增 `read_range`，local = seek + 定长读，
  s3 = head 取 size 后 `get_range`（S3 对越界区间返 416，故插件侧先截断）。**短读截断**
  语义——`offset + len` 越尾只返回实际可读字节，`offset` 过尾返回空数组；`offset` / `len`
  为负 / 小数 / NaN 报 `blob readRange: offset must be a non-negative integer`（不静默取整）。
- **能力回落**：FFI 后端三槽报错时宿主回落默认实现（`copy` → `get`+`put`、`readRange` →
  `get` 全量切片），回落也失败则抛**后端原错误**——JS 侧永不因后端能力失败，但回落字节
  过桥（local/s3 均原生支持，回落只是第三方后端的保险）。
- **ABI 10 → 11**：`BlobBackendVtable` 增 `copy` / `move_to` / `read_range` 三槽（vtable
  形状变更 = 必须 bump）。**全部插件须随宿主重编译**（`cargo xtask build`），旧 ABI 产物
  加载即 `plugin ABI mismatch`。ABI 历史注释与 `docs/modules/05`、
  `plugin-architecture.md` / `plugin-development.md` / `dev-guide.md` 的「当前 ABI」表述
  一并订正为 11（此前分别停在 7/9/8）。
- **local 路径映射**：`LocalBlob::fs_path` 走 `LocalFileSystem::path_to_filesystem`（而非手写
  `root.join(key)`）——ABI 11 的三条路径都在文件系统层自己算路径，key 含空格 / 非 ASCII 时
  手写拼接会与 `put`/`get` 的落盘位置编码分叉；已补含中文与空格 key 的 copy/move/readRange
  往返用例钉住。
- **devkit 对齐**：`api-manual.md` blob 三 API 行 + 搬运/区间读用法段 + 已知限制与错误表
  四条；`SKILL.md` 陷阱速查四条；`scenarios.md` 新增场景 25（临时对象转正 + 4KB 嗅探）；
  `README.md` 增 v0.1.47 条目；`sample/global.d.ts` 的 `BlobApi` 增三方法
  （`bin/devkit/` 由 `cargo xtask build` 同步）。另订正 `dev-guide.md` blob 概览表（补三 API）
  与 `docs/modules/00-overview.md` 的 `ABI_VERSION`（8 → 11）。
- **测试**：local copy（src 保留 + ct 随行）/ move（同目录 rename + 跨目录、src 消失）/
  `src == dst` no-op / readRange 截断与越界与非法 key / 含中文与空格 key 往返 / JS 面三 API
  冒烟（含非整数 offset 拒绝）；FFI 适配器三槽转发 + 能力回落 + 「回落也失败抛后端原错误」。

## v0.1.46 —— 同名响应头多值：`json.header` 追加语义 + `Set-Cookie` 双发（CSRF 双提交闭环，未打标签）

**动机**：cookie 会话（v0.1.30）的 CSRF 双提交此前是**半成品**——守卫验「csrf cookie == csrf 头」，
但 `Capture.headers` 单值（`HashMap<String,String>`），登录端点一个响应放不下第二个
`Set-Cookie`，`oj_csrf` 只能经 body 下发再由前端 JS 写 `document.cookie`（sample 的
workaround）。框架层补好多值通道，双提交两枚 cookie 都由服务端签发。

- **`json.header` 追加语义**（行为变更，兼容）：同名头可重复写入有序保留；写回 HTTP 时
  `Set-Cookie` 追加（合法重复，登录双发 `oj_sess` + `oj_csrf` 的唯一通道），其余头同名
  最后一个生效（与旧覆盖写读取语义一致）。`ReqState.headers` / `Capture.headers` 改
  `Vec<(String,String)>`；响应头每请求个位数条，线性扫无性能问题，不上索引。
- **sample 登录/登出改写**：登录同响应双发 `oj_sess`（HttpOnly）+ `oj_csrf`（JS 可读），
  删掉 body-csrf workaround；登出两枚 cookie 一并 `Max-Age=0` 清掉。
- **devkit 对齐**：`api-manual.md` json.header 签名表 + §8 cookie 会话段（双发职责与写法）。

## v0.1.45 —— 发版文档收尾 + `contract.rs` 格式化

v0.1.44 已交付 PR-6 4b 全部功能，但发版自查（devkit 四件逐条对齐 + fmt 门禁）遗留若干收尾，本版本补齐。无功能/行为变更，无新增用户可见 API。

- **devkit 四件对齐**：
  - `api-manual.md` 命令表补回 `oj openapi` 子命令（v0.1.43 新增，此前漏登）；`.schema` 节补 `additionalProperties:false` 运行期拒未声明字段的说明。
  - `README.md` 版本提示补 v0.1.43 `oj openapi` 一行（原本只记了 v0.1.44 `.schema`）。
  - `SKILL.md` 陷阱速查补 `oj openapi --check` 漂移门禁一行。
  - `scenarios.md` 场景 24 重写为自洽可跑：原「验证」curl 与示例 handler 不匹配（GET 打到仅导出 `post` 的路由、`params:{id}` 路由未定义），改为单一 `src/order/_id_/api.ts` 导出 `get`+`post`，curl 预期 400 文案与 `src/contract.rs` 实际报错逐字一致。
- **`src/contract.rs` 格式化**：v0.1.44 提交时未过 `cargo fmt`，本版本补齐（仅空白，无语义改动）。

## v0.1.44 —— PR-6 4b：handler 入参契约（`.schema`）与运行期校验

- **新增 JS 声明面 `.schema`**（与既有 `.route` 完全同构）：在导出的函数对象上分
  **三通道**声明入参契约 —— `params`（路径段）/ `query` / `body`，对应
  `http.params` / `http.query` / `http.body`。
- **声明即执行**：校验在**进 JS 之前**发生（`run_module` 内，与既有「方法未导出 →
  405」同一切面），违反 → HTTP **400** 信封 `{"code":400,"msg":...}`，handler 不被调用。
- **字符串强转**：`params` / `query` 在 HTTP 里只有字符串形态，声明
  `integer`/`number`/`boolean` 时显式强转，转不动即 400；`body` 是真 JSON，不强转。
- **自研最小校验器（`src/contract.rs`），零新增依赖包**：
  - 受支持关键字白名单：`type` / `required` / `properties` / `items` /
    `additionalProperties` / `minimum` / `maximum` / `minLength` / `maxLength` /
    `pattern` / `enum` / `nullable` / `minItems` / `maxItems`；
  - 白名单外（`$ref` / `oneOf` / `format` …）一律 **fail-fast，绝不静默跳过**
    —— 静默放行等于把声明变成不生效的东西（本项目最痛的缺陷类）；
  - `pattern` 用 **Rust `regex`**（已在依赖树，显式声明防漂移；不是 ECMA-262，
    lookahead 不支持，非法即装配期报错）；
  - `params` / `query` 只允许扁平标量 —— 底层是 `HashMap<String,String>`，
    声明 array/object 是永远无法满足的死契约，装配期即拒。
- **配置开关 `server.schema_validation`**：默认 **true（fail-closed）** —— 声明了契约
  却不执行 = 假契约。`.schema` 是全新声明面，当前无人声明，故升级无行为变化。
- **契约单一真源**：内省一次同时取 `route` 与 `schema`（`{route, schema}`，杜绝两趟
  内省漂移）；落在 `RouteRow.schema`，dev 内省与 release `routes.js` 同源；
  `oj build` 把契约写进 `routes.js`（**可选字段**，旧产物缺字段 = 未声明，向后兼容）。
- **`oj openapi` 补出 `parameters` / `requestBody`**：类型取自契约（不再硬编码
  `type: string`）；且 `params` 声明与路由 pattern 实参必须**双向一致**，否则生成期
  报错 —— 否则契约与 URL 会变成两份真源，未声明者不受任何约束。
- **内部合成派发豁免**：`dispatch_meta_handler` 造的是 `query={"path":…}`、空 body 的
  合成请求，走 `validate=false`；外部请求一律校验（缺省即校验）。
- **验证**：`src/contract.rs` 单测 19 项（含击穿：未知关键字不得静默放行、死契约拒、
  键错配不得误拦、强转边界）；bridge 守卫用例 1 项（外部 400 且不进 JS / 内部豁免 /
  合规不误拦）；`oj openapi` 用例 +3；**真 HTTP e2e +2**（400 与开关关闭不校验）。
- 文档：devkit 四件同步（api-manual「入参契约 `.schema`」/ SKILL 陷阱速查 2 条 /
  scenarios 场景 24 / README 版本提示）；`docs/modules/04-oj-cli.md` 同步。

## v0.1.43 —— PR-6 第一步：`oj openapi` 生成 + `--check` 漂移门禁

- **新增 `oj openapi` 子命令**（`oj/src/openapi_cmd.rs`）：从路由表生成 **OpenAPI 3.1**
  （手搓 `serde_json::Value`，不引入额外 crate），并支持 `--check` 漂移校验（PR-6 第一步；
  请求/响应 schema 留待后续步）。
  - `oj openapi -c config.yaml [-d dir] [--base B] [-o out.json]`：生成并打印 / 落盘。
  - `oj openapi -c config.yaml --check`：生成物与已提交 `<dir>/openapi.json` 比对，不一致即
    打印重生成命令 `oj openapi -c ... -d ... [--base ...] [-o ...]` 与差异行、退出码 1
    （CI 漂移门禁）；一致退出码 0。
- **路由发现双模复用既有装配**，避免 dev/release 分叉：
  - dev（`src`，`ts=true`）：逐文件 `bridge_introspector` 内省 `.route` → `RouteTable::build`；
  - release（`dist`，含 `manifests.yaml`，`ts=false`）：读锁 + 各模块 `routes.js`
    （`bridge_default_reader` → `entries_from_value` → `from_entries`）。
- **当前生成物体量为**：路径 / 方法 / 源文件溯源 `x-oj-file` / 派生 `operationId`
  （`^[a-zA-Z0-9._~-]+$`，非法字符转 `_`）/ 路径参数（`in: path` 必填）；`responses` 为占位
  （200 OK）—— handler 仅导出 verb fn + 可选 `.route`，尚无 schema 导出契约，故请求体 / 响应
  schema 待 PR-6 后续步补完。
- **catch-all 处理**：oj 的 `{*path}` 写作非 OpenAPI 合法的 glob，生成时收敛为 `{path}` 并抽
  出路径参数。
- **漂移比对对键序无关**：`canonical` 递归键升序规范化，键序差异不误报；内容差异必被检出。
- **验证**：`oj/src/openapi_cmd.rs` 4 个单测覆盖——`generate_produces_valid_openapi_3_1_*`
  （catch-all 收敛 + 路径参数抽出）、`check_detects_no_drift_and_drift`（篡改 summary 必被检出）、
  `check_roundtrip_writes_then_detects_tamper`（真·`--check` 文件往返：落盘→同物 0→篡改 1）。
- 文档：`docs/modules/04-oj-cli.md` 新增 §6「`oj openapi` 子命令」+ 子命令表行；架构文档同步。

## v0.1.42 —— PR-2 续：流式查询方言级真取消（postgres/mysql）

- **`oj-db-postgres` / `oj-db-mysql` 在 `postgres://` / `mysql://` DSN 下，`db.stream` 取消升级为
  **真取消**：游标单开一条**专用连接**，取消 = 显式服务端中止——
  `pg_cancel_backend(pid)`（postgres）/ `KILL QUERY conn_id`（mysql）+ 断连；服务端查询**立即
  终止**（`pg_stat_activity` / `information_schema.processlist` 中秒级消失），不再等驱动层 drop。
  - 动机：sqlx 0.9 无方言级 CancelToken，且 `Connection` 的 Drop **不通知服务器**（已验证：drop 后
    服务端查询仍跑完）；仅靠断连要等 keepalive 才回收，故取消必须显式发令。
  - `CONNECTION_ID()` / `pg_backend_pid()` 在连接建立时查询并存于 `StreamHandle`，cancel 时经一条
    **独立短连接**发出后 `close_hard`，不影响主池。
- **Any 回退路径（sqlite 等）与 core 后端保持协作式取消**（批间生效，下一批返回
  `{"error":"cancelled"}`）——能力边界：无服务端查询句柄的方言只能协作式。
- **真实数据库验收**（env-gated，`OJ_TEST_PG` / `OJ_TEST_MYSQL`）：`real_*_stream_cancel_kills_server_query`
  钉死「cancel 前 processlist/activity 有慢查询 → cancel 后 ≤5s 无孤儿查询 → 池路径仍正常」，
  修测试自身「轮询连接自匹配 marker」的计量偏差（已加 `id <> connection_id()` / `pid <>
  pg_backend_pid()` 排除）。
- 文档：api-manual `db.stream` 取消语义与错误/限制表同步方言矩阵；SKILL.md 陷阱速查增补。
- 方言能力矩阵（取消语义）：

  | 后端 / DSN            | 流式            | 取消                    |
  |-----------------------|-----------------|-------------------------|
  | `oj-db-postgres` (pg) | 专用连接真流式  | `pg_cancel_backend` 真取消 |
  | `oj-db-mysql` (mysql) | 专用连接真流式  | `KILL QUERY` 真取消     |
  | core / Any (sqlite)   | 池路径真流式    | 协作式（批间）          |

## v0.1.41 —— PR-9：租户声明的服务端绑定

- 新配置 `tenant.require_signed_claim`（默认 **false**，行为不变）：开启后 `http.tenantId`
  **只认验签后的 JWT claim `claims.tenant`**——裸租户头不再自证：
  - claim 与头不符 → 403 `tenant header does not match signed claim`；
  - 无 claim → 403 `tenant requires a signed claim (user.claims.tenant missing)`（即使带头）；
  - claim 命中且无头 → 放行（claim 即来源）；`anonymous_paths` 豁免保持（OIDC 回跳逃生口）。
- 装配期 fail-fast：开关开启但缺 `tenant.enable` 或缺 auth 守卫 → 拒绝启动
  （`validate_tenant_binding`，纯函数可单测）。
- 作用面为 HTTP 前置管线（WS 帧/任务桥为系统上下文，不经此绑定——文档注明）。
- 探测（击穿优先）：验签 claim 六态矩阵（一致/不符/无头/无 claim/无 token/豁免路径）、
  开关关闭回归（旧行为逐字节一致）、装配校验矩阵；实施中探测④抓到「缺 claim 时头被
  放行」的实现漏洞并当场修正为严格 fail-closed。
- 文档：api-manual 租户配置节 + 错误/限制表、tenant-guide 报错表同步。

## v0.1.40 —— PR-4：isolate 内存限额（P0 收口）

- 新配置 `server.js_heap_limit_bytes`（默认 **256 MiB**，下限 32 MiB，装配期 fail-fast）：
  per-isolate V8 堆限额。超限 handler 被 `terminate_execution` 安全终止，返回 5xx 信封
  `js heap limit exceeded (limit N bytes; server.js_heap_limit_bytes)`，**进程存活**。
- **fired isolate 绝不回池**（`RuntimePool::checkin` 单点守卫）：防止 handler 吞掉终止
  错误后把堆已膨胀的坏 isolate 留在池里；WS 帧路径同 Timeout 毒化处置（断连 + Worker
  丢弃），池化任务标记 Failed。
- 覆盖面：HTTP/WS/任务池（RuntimePool::spawn 单点）+ `oj test` / `oj exec` 独立 runtime
  （DRY：`heap_create_params` / `install_heap_limit_callback` / `heap_guard_fired` 三辅助共用）。
- 已知边界（文档登记）：单个 > 限额的巨型分配仍可能触发 V8 Fatal OOM（V8 对新限额的
  二次逼近不重入回调）——进程存活优先于硬顶精确性；稳态超限行为已被击穿测试钉死。
- 实测钉死三条 V8 语义（写进注释）：二次逼近 Fatal、天文数字返回值 CHECK 崩、余量须
  容下在途分配。
- 测试：OOM 击穿（连续两轮 OOM + 池恢复）/ boot 期超限 / 配置校验（默认值 + <32MiB 拒）。
  `docs/dev-guide.md`「仍开放」清单移除内存限额项。

## v0.1.39 —— 流式防护探测：修复取消真语义（DOMException + 有界背压）

> 对新流式面补**对抗性探测用例**（守卫/闸/回收不靠声明靠钉死），两条探测各抓到一个真 bug，随本版修复。

### 探测发现的修复
- **`AbortController.abort()` 此前实际不可用**：运行时未挂 `DOMException` 全局，
  deno_web 的 `AbortSignal.abort()` 构造中断原因即抛 `ReferenceError`——取消从未真正
  触发过（旧测试靠 catch 分支"碰巧"绿）。本版挂载 `ext:deno_web/01_dom_exception.js`
  （`db.stream` 的 signal 取消、`fetch` 的 AbortSignal 均受益）。
- **取消语义改为"干净提前结束"**：`db.stream` 行通道由 unbounded 改为**有界（64 行）**
  ——消费端不拉取时后端拉取同步暂停（真背压），abort 一到立即停止拉取；已缓冲行交付
  完即 done（**不报错**）。旧 unbounded 会提前灌满结果集、取消扑空（探测用例
  `probe_stream_abort_releases_connection` 以 sqlite `max_connections(1)` 泄漏即挂死
  为哨兵抓到）。旧 abort 用例断言已按新语义订正（不再是"拒绝"，而是提前 done）。
- `db.stream` close/abort 路径的 open Promise 二次拒绝收敛（守卫错误时不再叠加
  unhandled rejection）。

### 新增防护探测用例（防"插旗丢防护"）
- db：租户守卫四态全打（无条件拒 / 无租户头拒 / 匹配放行且只见本租户行 / 参数租户
  不符拒）；abort 连接回池哨兵；并发游标交叉隔离。
- server：blob PUT 写面守卫（无 token 401 且不落盘）；流式代分配 key 对穿越文件名
  （`../../evil.sh`）的净化（对象只落 uploads/ 段、root 无逃逸文件、key 可安全回读）；
  文本字段超限 413；多文件各自合规但总和超 multer 总闸 413。

## v0.1.38 —— ABI 10：db 插件流式查询 + blob 流式上传（PR-2/PR-5 Phase B）

> **破坏性 ABI 变更（9→10）**：`DataAccessorVtable`/`BlobBackendVtable` 追加槽位，旧 ABI 9 插件启动即 fail-fast（既有门禁）。
> 第一方插件 `oj-db-mysql`/`oj-db-postgres`/`oj-blob-s3` 已随本版锁步重建（ABI 10）。

### db 流式：插件后端真流式（PR-2 Phase B）
- `DataAccessorVtable` 增 `stream_open/stream_next/stream_cancel/stream_close` 四槽：
  批量 pull（≤100 行/次）+ 信封传输（`{"rows":[...]}` / `{"done":true}` / `{"error":"..."}`，绝不裸 null）。
- `oj-db-mysql`/`oj-db-postgres` 实现游标化流式（`db.stream` 在 mysql/pg 上真流式，不再回落全量）。
  取消为**协作式**（批间生效）：`stream_cancel` 置标志，下一批返回 `{"error":"cancelled"}`；
  `stream_close` 释放游标（连接回池）。sqlx 驱动层拿不到方言级 CancelToken，
  「方言级真取消」（PG CancelToken / MySQL KILL QUERY）为已登记偏差（计划文档 §实施状态）。
- 宿主 `FfiDataAccessor::stream_query`：open 哨兵 `{"unsupported":true}`（第三方未实现流式的
  ABI 10 插件）→ 回落 `db.query` 全量（评审定稿：保 dev/test 一致）；abort 路径由 Drop 守卫
  fire-and-forget `stream_cancel`+`stream_close`，杜绝插件侧游标条目泄漏。
- 事务内 `db.stream` 报错文案更新（去 Phase A/ABI 字样，语义不变：流式只走直连池）。

### blob 流式上传：服务端大文件直落 blob（PR-5 Phase B）
- `BlobBackendVtable` 增 `put_stream_open/chunk/finish/abort` 四槽；进程内 `BlobBackend`
  trait 同步扩展（`LocalBlob` 临时文件 + Drop 守卫兜底清理 + finish rename 转正 + ct sidecar）；
  `oj-blob-s3` 用 multipart（每满 8 MiB 一个 part；abort 取消 multipart 防 orphan parts）。
- **server 流式 multipart**：上传请求体不再整段缓冲——文本字段缓冲并入 body（累计 ≤
  `max_upload`）；文件字段 ≤ `max_upload` 仍缓冲（`http.file(i)` 旧行为不变）；
  **> `max_upload` 的文件字段转流式** `put_stream_*` 直落 blob（单文件上限
  `blob_upload_max`，服务端内存恒定）。multer 总闸 = `max_upload + blob_upload_max`，
  超限 413。blob 直传 PUT 腿同步流式化（`put_stream_open` Err 回落缓冲 put）。
- **JS API（additive）**：`http.files[i]` 新增 `key`/`url`（流式大文件的 blob 对象 key 与
  下载地址，小文件为 null）；`http.file(i)` 对流式大文件明确报错并指路（不再可能静默空字节）。

### 测试 / 文档
- 新增：FFI 适配器流式（哨兵回落/批量 pull/abort cancel+close）、local 流式落盘+abort 清理、
  db 插件离线流式 roundtrip + cancelled 信封、server 流式 multipart 三态（大文件落 blob /
  小文件缓冲 / 超限 413）与 PUT 直传流式腿；真库回归（`OJ_TEST_PG`/`OJ_TEST_MYSQL`）沿用。
- `docs/devkit/` 四件、`docs/db-guide.md` §3.1、`docs/dev-guide.md`、`docs/user-manual.md`、
  `sample/global.d.ts`（`UploadedFileMeta.key/url`）同步；`docs/pr2-pr5-implementation-plan.md`
  登记实施状态与偏差。

## v0.1.37

> 版本分界：`oj/Cargo.toml` 0.1.36 → 0.1.37。上一版：`v0.1.36`。
> 发布点标签：**未打标签**（待发布时 `git tag -a v0.1.37`）。
> 注：`v0.1.36` 仅作为中间边界提交（commit `133f2b4`，config `broker/es` 命名 map 解析修复），
> 未单独打标签，其变更随本版一同发布。

**新增：流式查询 `db.stream`（PR-2 Phase A）**

- **`db.stream(sql, params?, opts?)`（v0.1.37，PR-2 Phase A）**：逐行从后端拉取大结果集，避免
  `db.query` 一次性全量驻留内存（导出 / ETL / 大表遍历场景）。两种形态：
  - **回调形态（推荐）**：`opts.onRow(row)` 逐行回调，回调内可 `await`（如每行 `json.stream` 推一帧、
    或 `db.exec` 落库）；返回 Promise 在流走完 / 中止后 resolve，异常时 reject。
  - **异步迭代器形态（逃生舱）**：不传 `onRow` 时返回 `AsyncIterable<Row>`，可 `for await` 消费。
  - **取消**：`opts.signal`（WHATWG `AbortSignal`）在 `abort` 时由桥接层 `op_db_stream_abort` 通知
    后端 pump 中止；流走完自动移除监听器。桥接层每请求维护流注册表（`ReqState.db_streams`），
    `reset` 换全新 Arc，存活 pump 不会被跨请求串号。
- **实现范围（Phase A）**：核心 `SqlxAccessor`（sqlite / mysql / postgres，经 `Any` 驱动）走真流式；
  后端用 `async-stream` 的 `stream!` 把自有 SQL / 参数 / 池收进生成器状态机（规避 sqlx `Query`
  借用 `&str` 无法返回 `'static` 装箱流的问题）。`InMemoryAccessor` 同样支持（内存行直接 `iter`）。
- **边界（Phase A，ABI 9 不升）**：
  - 仅支持**非事务**目标；`db.tx` 内调用报
    `db.stream within an active transaction is not supported in Phase A (ABI 10 required)`。
  - FFI 插件后端（`oj-db-*`）走 `DataAccessor::stream_query` 默认实现，报
    `backend does not support streaming (ABI 10 required)`——须待 PR-2 Phase B（ABI 10
    vtable：`stream_open` / `fetch_next` / `stream_cancel` / `stream_close`）落地后插件方可流式。
  - 内容与 `db.query` 全量**一致**，差异只在内存形态。
- **文档同步**：`docs/devkit/` 四件已对齐——`api-manual.md` §6 `db` 节新增 `db.stream` 小节 +
  签名入总表 + §13 已知限制全表新增条目；`scenarios.md` 新增「场景 23：大表流式导出」；
  `SKILL.md` 陷阱速查新增三条；`README.md` 文件清单同步。

**随本版发布的 v0.1.36 边界修复（未单独立版）**

- `config`：`broker` / `es` 段放宽为命名 map（旧单对象写法兼容），并修复命名 map 解析**静默丢字段**
  的缺陷（原解析对 `Option<HashMap>` 缺省回退路径漏写字段合并）。资源根 key 多源选择（`--redis` /
  `--blob` / `--es` / `--broker` / `--kafka` / `--rabbit`，v0.1.34）不受影响。

## v0.1.35

> 版本分界：`oj/Cargo.toml` 0.1.34 → 0.1.35。上一版：`v0.1.34`。
> 发布点标签：**已打标签：`v0.1.35`**（已推送 origin）。

**新增：流式响应 / SSE（PR-1）、CORS（PR-3）、应用层 AES-GCM 加密（PR-10）**

- **流式响应 / SSE（`json.stream` / `json.sse`，PR-1）**
  - JS 侧通过 `json.stream(opts)` / `json.sse(opts)` 打开流式通道，返回 `{ write(chunk), end() }`。
    `json.sse` 自动设置 `Content-Type: text/event-stream`，并把每次 `write` 的「数据」包成
    `data: <内容>\n\n` 帧；`json.stream` 默认 `200`，可经 `opts.status` / `opts.contentType` 覆盖。
  - 通道解耦于 isolate 生命周期：`run_with` 结束后 isolate 归还池，但数据通道仍由 `Capture`
    持有并继续向客户端推送；handler 退出后由 `read_capture` 接管 rx 并关闭 tx。超时路径会丢弃
    runtime 与 tx，客户端收到干净的流终止。
  - 限制：流式响应**绕过** `{code,msg,data}` 信封（直接写裸 body）；心跳保活间隔 15s
    （`:\n\n`）；`opts` 仅支持 `status` / `contentType` / `sse` 三个字段。

- **CORS（`server.cors` 段，PR-3）**
  - config 新增 `server.cors: Option<CorsCfg>`；**段存在即启用**，缺省（段缺失）则不挂 CORS 层
    （与现行为完全一致，响应无 `Access-Control-*` 头）。
  - 字段：`origins`（字符串列表）/ `methods` / `headers` / `expose`（暴露响应头）/
    `credentials`（bool）/ `max_age`（秒，`Option`）。`origins` 为空时退化为 `AllowOrigin::any()`
    （放行任意源）；非空时按列表精确匹配。
  - **fail-fast**：`credentials: true` 且 `origins` 为空 → 启动报错（带凭据的 `*` 非法，浏览器
    会拒）。其余字段缺省回落安全值（methods/headers 为空 → 反射请求的方法/头）。
  - 预检（OPTIONS）由 `tower-http::cors::CorsLayer` 在路由前短路，业务 handler 不感知。

- **应用层 AES-GCM 加密（`crypto.aesGcmEncrypt` / `crypto.aesGcmDecrypt`，PR-10）**
  - `crypto.aesGcmEncrypt(plaintext, keyHexOrB64)` / `crypto.aesGcmDecrypt(ciphertext, key)`：
    基于 `aes-gcm 0.10`，输出 `base64( nonce12 ‖ ciphertext ‖ tag16 )`（明文 UTF-8 入）。
  - 密钥支持 16 字节（AES-128）或 32 字节（AES-256）的 hex / base64 编码；**不支持 AES-192**
    （上游 crate 未 re-export），传入 24 字节密钥会返回明确错误。
  - 应用层加解密，密钥由调用方自行保管（不进 config、不托管）——适合对落库/传输前的字段做对称加密。

**向后兼容**：三项均为纯新增，未改 ABI（ABI_VERSION 仍为 9）、未动任何既有 op / 全局对象语义；
既有 handler 与 config 无需改动。

## v0.1.36

> 版本分界：`oj/Cargo.toml` 0.1.35 → 0.1.36。上一版：`v0.1.35`。
> 发布点标签：**未打标签**（本版本改动尚未打标签）。

**修复：`broker` / `es` 命名 map 配置解析静默丢字段（v0.1.34 既有 bug）**

- **现象**：`broker:` / `es:` 写成命名多源 map（`broker: { default: { kind: local }, prod: { kind: kafka } }`）
  时，`default` 条目的 `kind` / `endpoint` 等字段被静默解析为空（如 `kind == ""`、`endpoint == ""`），
  导致 broker/es 装配走错默认实现。单对象写法（`broker: { kind: kafka }`）不受影响。
- **根因**：`BrokerCfg` / `EsCfg` 反序列化走 `single_or_named_map` 的 untagged `OneOrMap` 枚举；
  `Single(T)` 变体因 `T` 未 `deny_unknown_fields`，会把命名 map（`{ default: {...}, prod: {...} }`）
  **误判为单对象**并忽略 `default`/`prod` 键，于是 `Map` 变体永不命中。
- **修复**：给 `BrokerCfg` / `EsCfg` 加 `#[serde(deny_unknown_fields)]`——命名 map 下 `Single(T)`
  因未知键 `default`/`prod` 失败，untagged 正确回退到 `Map` 变体；同时顺带让 broker/es 配置里的
  拼写键（如 `kindd:`）被启动期拒绝，而非静默忽略。无 ABI 变更（ABI_VERSION 仍为 9）。

## v0.1.34

> 版本分界：`oj/Cargo.toml` 0.1.33 → 0.1.34。上一版：`v0.1.33`。
> 发布点标签：`v0.1.34`（已打标签）。

**新增：资源根 key 多源选择（`--redis`/`--blob`/`--es`/`--broker`/`--kafka`/`--rabbit`）**

- **动机**：原 `--db` 可切换测试/执行用的数据库；但 `redis`/`blob`/`es`/`broker`/`kafka`/`rabbit`
  等其余资源根 key 仍只能固定用 config 里名为 `default` 的 profile。一份 config 不能同时声明多源、
  再用同一套 CLI 选源——与「一份配置配多源资源、按需选源」的诉求冲突。
- **行为**：`oj test` 与 `oj exec` 现支持与 `--db` 同形的 6 个新旗标，各自把 config 对应段里
  的命名 profile 选为「默认源」（config 段缺省回退字面 `default`）：
  - `redis` → `config.redis.<profile>`；`blob` → `config.blob.backends.<profile>`；
    `es` → `config.es.<profile>`；`broker` → `config.broker.<profile>`；
    `kafka` → `config.kafkas.<profile>`；`rabbit` → `config.rabbits.<profile>`。
  - 选中的 profile 在装配期被别名为字面 `"default"`：JS 侧 `redis()`/`blob()`/`es()`/`bus()`/
    `kafka("default")`/`rabbit("default")` 等无需改代码即指向选中源。
  - 给定 profile 在 config 段中不存在 → **fail-fast**（提示可用 profile 列表），不静默回落
    `default`（避免误用开发库/错误后端）。
- **向后兼容**：`es`/`broker` 配置段由「单对象 `Option`」放宽为「命名 map，兼容单对象写法」——
  旧的单对象 config（`es: { endpoint: ... }`）经反序列化辅助自动包成 `{ default: ... }`，
  既有配置无需改动；多源则写 `es: { default: ..., other: ... }`。`db`/`redis`/`blob`/`kafka`/
  `rabbit` 段本就是命名 map，语义不变。
- **`--db` 语义保持**：`db` 仍走请求期「字面 default 重定向」（带 `DB("name")` 显式调用不受影响），
  其余轴在装配期把选中 profile 烘焙为 default 别名；`oj test` 的 `--db` 缺省仍回落 `config.db.test`。
- **文档**：`docs/devkit/` 四件 + `docs/modules/02-config.md` + `docs/user-manual.md` 同步多源
  配置示例与新旗标；本 CHANGELOG 本节同步。

**变更：secrets 信封移除 RSA（v1），仅保留 X25519 信封**

- **为什么移除**：v1（RSA-OAEP）密文固定地板约 256B（2048 位）/ 512B（4096 位），短密码
  被撑大；且 config 同时存在 RSA / X25519 两套密钥会产生「`private_key_path` 到底指向哪种」
  的歧义。当前 v1 无人使用，故**整段删除 RSA 路径**，`oj secret keygen` 只生成 X25519 密钥，
  config 里只有一种密钥，不再有歧义。
- **密文长度随明文**：X25519 信封走 X25519 ECDH + HKDF-SHA256 派生会话密钥，**固定地板仅
  约 62B**（版本 1 + 算法 1 + 临时公钥 32 + nonce 12 + tag 16），密文长度≈明文+62B，
  8 字密码约 95 字符（原 RSA4096 同例会 ~735 字符）。
- **CLI 简化**：`oj secret keygen` 去掉 `--bits` / `--alg`，只产 32 字节 `BEGIN OJ X25519
  PRIVATE KEY` / `BEGIN OJ X25519 PUBLIC KEY` PEM（X25519 固定 32 字节，无密钥长度概念）。
- **存量 v1 密文**：解密时明确报错 `sealed value is v1 (RSA) — v1 信封已移除；请用 oj secret
  seal 以 X25519 重新加密`，不静默降级。新密文一律 X25519（信封首字节 `0x02`）。
- **实现**：`PrivKey` / `PubKey` 由「RSA | X25519」枚举收敛为单一 X25519 结构；`seal` /
  `open` 不再做算法派发；依赖保留 `x25519-dalek 3` + `hkdf 0.12` + `aes-gcm 0.10`，
  `rsa` 仅余 `oj-cert` / OIDC 使用（secrets 不再依赖）。

**修复：config 裸键（null 值）解析容错**

- `Config` 的 `db` / `redis` / `plugins` / `kafkas` / `rabbits` / `vars` 等 map 字段与
  `secrets` / `tasks` / `ws` / `db_query` 等段字段，由「裸键按空处理」升级为**完整 null 容错**：
   earlier 的 `#[serde(default)]` 只覆盖「键缺失」，键存在却为 YAML null（如 `redis:` 末无值）
  仍报 `invalid type: unit value, expected a map`。现抽出统一反序列化辅助 `null_as_default`
  （`Option::<T>::deserialize` 对 null/缺失均解 `None` → 回退 `T::default()`，对**真实类型错误**
  如 `redis: foo` 标量仍透传原错不静默吞），并以 `#[serde(default, deserialize_with =
  "null_as_default")]` 应用到上述字段。裸键与显式 `{}` 等价，且 `oj exec` / `serve` / `build`
  共用同一条 `load_from` 路径，一并自愈。

**文档**

- `docs/devkit/` 四件同步：`api-manual.md` §10「secrets」改为「仅 X25519 信封、RSA(v1) 已移除」、
  `scenarios.md` 场景 18 keygen 示例去 `--bits`/`--alg`、`SKILL.md` 陷阱速查改为 X25519
  单算法、`README.md` 索引标注「RSA(v1) 已移除」；`docs/secrets.md` 重写为 X25519 单信封
  （格式 / 命令 / 失败表 / FAQ）；本 CHANGELOG 本节同步。

## v0.1.33（未打标签）

> 版本分界：`oj/Cargo.toml` 0.1.32 → 0.1.33。上一版：`v0.1.32`。
> 发布点标签：未打标签（发版时 `git tag -a v0.1.33 -m "v0.1.33: config 凭据密封（ENC[]）+ DSN/URL 日志脱敏"` 并推送）。

**新能力：config 凭据密封（`ENC[...]`）**

- **`config.yaml` 里的敏感值可以写成密文**：`db` DSN、`redis` URL、`blob` 的
  `access_key/secret_key`、`smtp.*.pass` 与 `xoauth2.access_token`、`ldap` 的 `bind_pw`、
  `auth.jwt_secret`、`oidc` 的 `client_secret`/`secret` 等**任意段、任意深度的字符串值**
  都可写成 `ENC[<base64url>]`，启动时用部署机私钥就地解密。配置泄漏（误提交 git / 镜像层
  / 备份）不再等于密码泄漏——把「N 个密码」收敛成「1 个私钥」。
- **算法**：RSA-OAEP-SHA256 只封 32 字节会话密钥，明文由 AES-256-GCM 加密（信封）。
  明文长度无上限（不再受 RSA 单次 190 字节限制），密文被改一个 bit 会**解密失败**
  而非解出垃圾。
- **解密落在配置解析的 Value 层**（`Config::deserialize` 之前，`src/config.rs`
  的 `load_from`）：`ldap` / `plugins` / `kafkas` 这类不透明段同样覆盖，**配置 schema
  零改动**，将来新增段自动支持。配置里没有 `ENC[...]` 时**完全不碰密钥路径**（旧配置
  逐字节不变，不配私钥照常启动）。
- **密封值只能当值**：落在 mapping **键**位（`ENC[...]: 1`）一律报错，不静默留着密文
  当名字用；数值字段（`server.port`）收到密文会 `invalid type: string`——解密结果必然是
  字符串，这是限制不是 bug；字面量恰好以 `ENC[` 开头且以 `]` 结尾的明文会被当密文硬失败，
  报错里给出换写法的提示。
- **私钥三通道**（优先级高→低）：`OJ_SECRET_KEY`（内联 PEM）> `OJ_SECRET_KEY_FILE`
  （文件路径）> `secrets.private_key_path`（相对 config 目录）。
- **fail-closed**：有 `ENC[...]` 却拿不到私钥（或解密失败/密文被篡改）→ **启动报错退出**，
  绝不静默把密文当明文用（那会连上一个名叫 `ENC[…]` 的密码）。
- **新增 `oj secret` 子命令**：`keygen`（生成密钥对，私钥自动 chmod 600、拒绝覆盖）、
  `seal`（明文 → `ENC[…]`，**默认读 stdin**，避免明文进 shell history/`ps`）、
  `open`（排障，走与启动同一条私钥通道）。

**修复：DSN / URL 明文进错误与日志（宿主 + 插件两侧）**

- 宿主：`db_backend.rs` 未知 scheme 与「非 sqlite DSN」两条报错、`app.rs` 里 redis URL
  的 warn，原先会把**含密码的整串**原样打出，而 `server::logging` 会把终端输出完整镜像
  落盘 → 密码明文进 `logs/`。
- 插件（同一条泄漏链的另一端）：`oj-kv-redis` 的三条连接错误、`oj-bus-rabbitmq` 的
  连接错误同样原样带 URL，现一并脱敏（插件只依赖 `oj-plugin-ffi`，故本地同口径实现）。
- 统一为「凭据段打 `***`、host/库名保留」（如 `mysql://***@127.0.0.1:3306/app`、
  `redis://***@127.0.0.1:6379/1`），排障信息不丢。

- **运维约束（写进文档）**：`OJ_SECRET_KEY` 内联 PEM 会出现在 `/proc/<pid>/environ`、
  `docker inspect`、k8s pod spec 与 CI 的 env 回显里，**只适合临时排障**，常态用
  `OJ_SECRET_KEY_FILE` / `secrets.private_key_path`；信封不带 key-id，**每环境须用独立
  密钥对**（同一对密钥下 dev 密文粘进 prod 照样能解）。`keygen` 缺省 2048 位，长期密钥
  建议 `--bits 4096`。

**文档**

- `docs/devkit/` 四件同步：`api-manual.md` §10 新增「secrets —— 凭据密封」并补 fail-fast
  表一行、`scenarios.md` 新增场景 18（照抄即可跑）、`SKILL.md` 陷阱速查新增三条、
  `README.md` 索引标注版本特性。

## v0.1.32（未打标签）

> 版本分界：`oj/Cargo.toml` 0.1.31 → 0.1.32。上一版：`v0.1.31` → 38c05eb。
> 发布点标签：未打标签（发版时 `git tag -a v0.1.32 -m "v0.1.32: ldap search 支持 bindDn/bindPw 覆盖 + rustls CryptoProvider 修复"` 并推送）。

**修复（oj-ldap 插件 + bridge + bootstrap.js）**

- **`ldap.search` / `ldap.searchPaged` 支持 `bindDn` / `bindPw` 覆盖本次查询绑定**：
  此前 `bootstrap.js` 的 `search`/`searchPaged` 组装 op 时漏带这两个字段，导致经
  `opts.bindPw` 传入的服务账号密码永远到不了插件，搜索退化为匿名绑定（AD 报
  `operationsError: 必须先完成 bind`）。现 bridge（`src/bridge/ldap.rs` 的
  `validate_call`）与 `bootstrap.js` 均透传 `bindDn`/`bindPw`，插件
  `effective_bind` 将其与 config 的 `bind_dn`/`bind_pw` **合并（取一即可，另一个回落 config）**。
  该特性用于把服务账号**密码作为运行时参数**传入，避免落配置/源码。
- **oj-ldap 插件 `init` 安装 rustls 默认 `CryptoProvider`**：独立 cdylib 自带一份 rustls，
  宿主侧装的 provider 不覆盖，未安装时 `ldaps://` / `start_tls` 路径会 panic
  （`Could not automatically determine the process-level CryptoProvider`）。现
  `init` 调用 `rustls::crypto::aws_lc_rs::default_provider().install_default()`。
- **config 校验放宽**：`ldap:<inst>` 允许只配 `bind_dn`（密码经 `search` opts 运行时传入）；
  仅「有 `bind_pw` 却无 `bind_dn`」仍报错。

**新增（sample）**

- `sample/ldap_verify.ts`：复刻 PHP 的 AD 鉴证流程，验证 oj-ldap 正确性；服务账号 DN 留
  `sample/config.yaml`，**密码走 `oj exec … -- <account> <password> <adminPassword>` 运行时参数**。

**文档**

- `docs/devkit/api-manual.md`：`SearchOpts` 补 `bindDn`/`bindPw` 说明，修正 config
  `bind_dn`/`bind_pw` 不再强制成对的规则与错误文案。
- `docs/devkit/SKILL.md`：补 `search` 匿名绑定 `operationsError` 与「密码走参数」陷阱。

## v0.1.31（已打标签 v0.1.31）

> 版本分界：`oj/Cargo.toml` 0.1.30 → 0.1.31。上一版：`v0.1.30` → adc1a25。
> 发布点标签：`v0.1.31`（38c05eb）。

**破坏性变更（CLI 子命令重命名）**

- **`server` 子命令重命名为 `serve`**：`oj server` → `oj serve`（更符合动词命名；
  `oj/src/args.rs` 的 `Commands`/`Command` 变体由 `Server` 更名为 `Serve`、参数结构
  `ServerArgs` 更名为 `ServeArgs`，子命令 token 由 clap 派生为 `serve`）。旧 `oj server`
  不再可用。运行时提示文案（`oj serve listening on ...`、`oj serve daemonized (pid ...)`、
  `oj serve: <err>`）同步更新。

## v0.1.30（2026-09-28，已打标签 v0.1.30）

> 版本分界：`oj/Cargo.toml` 0.1.29 → 0.1.30。上一版：`v0.1.29` → e6ad218。
> 发布点标签：`v0.1.30`（adc1a25）。

**特性（genoffice 上游缺口 oj-1 ~ oj-8 一批合入；ABI bump 8 → 9，全部插件须重编译）**

- **模块加载器（oj-1/oj-2，v0.1.30）**：bare specifier 完整实现 Node `exports` 封闭语义
  （字符串形态 / 条件对象 import·require·node·default / 子路径键 / `./x/*` 与裸 `*` 模式键 /
  数组 fallback；有 `exports` 即接管、未命中报错**不回落** legacy：ERR_PACKAGE_PATH_NOT_EXPORTED
  风格文案）；pnpm 布局可用（解析全程不 realpath，符号链接视图直读）——`module → main → index.js`
  legacy 仅在无 `exports` 时生效。CJS 包装支持相对 require：`./x` 候选 `.js/.json/index.js`、
  JSON 模块、循环 require 返回部分 exports（Node 语义）、相对路径钳制在 project root 内、
  `node:` 内建报 `Node builtin 'path' is not available in oj runtime`。
- **wasm 胶水 Web API（oj-3，v0.1.30）**：新增全局 `atob`/`btoa`（标准 base64，非法输入
  抛 `InvalidCharacterError`）与 `crypto.getRandomValues(view)`（任意 TypedArray view，
  非 view 抛 TypeError，单次 ≤65536 字节对齐 WebCrypto）；`WebAssembly.instantiate`
  经 L1 实测（oj/tests/e2e_wasm.rs）——wasm-bindgen 类引擎包可进 oj runtime 的判定面已打通，
  剩余按"引擎包兼容性清单"逐包验证。
- **cookie 会话鉴权（oj-4，v0.1.30）**：oj-auth cfg 新增 `cookie` 段（缺省关闭=行为不变）：
  `{enabled, name:"oj_sess", same_site:"Lax", secure, ttl_secs:86400, csrf_cookie, csrf_header:"x-csrf-token"}`。
  判定序：匿名 → Bearer → cookie 会话（session cookie 值 = 同 secret JWT）→ 统一 401
  （`missing or invalid bearer token`，不泄露哪条路失败）；cookie 会话的非安全方法叠
  **CSRF 双提交**（csrf 头须等于 csrf cookie，否则 401 `missing or invalid csrf token`）。
  登录/登出端点是 JS 业务路由职责（sample/src/auth/ 有示例：HttpOnly `oj_sess` +
  非 HttpOnly csrf cookie）。**WS 握手过守卫**（js_route_guarded：method="GET" + 全头 JSON，
  401 不升级）——存量部署需把 ws 路径加进 `anonymous_paths`（sample 已补）。
  行为变更：WS 从此在守卫面内。
- **大文件上传直传（oj-5，v0.1.30）**：① JS `blob.uploadUrl(key, opts?)`（opts 缺省
  `{"kind":"put"}`；s3 后端返回 15min 预签名 PUT URL；multipart 形态暂返回 Err——
  object_store 无 multipart presign API，单发 PUT 已解 10MB/30s 痛点，>100MB 待真实需求）；
  ② 新直传路由 `PUT {base}/blob/{key}`：走鉴权守卫（Bearer/cookie 即令牌），体积上限
  独立 `server.blob_upload_max_bytes`（默认 1 GiB），不经 JsActor（无 30s）；handler 面
  `max_upload_bytes` 不变。请求体按路径分档限长（blob 腿 1 GiB / 其余 2×10MB 裸 413），
  **移除全局 DefaultBodyLimit**——handler 面内存 DoS 上限不因直传抬升。
  ③ 路由级 timeout：`server.route_timeouts: [{pattern, timeout}]`（段语义同
  anonymous_paths，按声明序首个命中，匹配含 base 全路径；配置非法启动 fail-fast）。
- **WS 房间原语（oj-6，v0.1.30）**：`ws.join(room)` / `ws.leave(room)`（ws handler 内，
  连接身份自动取）、`ws.broadcast(room, data)` → 送达数（socket.io 除己语义；HTTP handler
  可调，conn=0 不排除任何人）、`ws.roomSize(room)`（任意上下文）。进程内单例 hub，
  断连自动摘除；跨实例扇出仍走 bus。
- **blob Range（oj-7，v0.1.30）**：local 内联下载支持 `Range` 单区间（`a-b`/`a-`/`-N`）→
  206 + Content-Range + Accept-Ranges；越界/空文件 416（`bytes */len`）；多区间/非法
  Range → 200 全量（单区间覆盖 pdf.js/媒体 seek 场景）。s3 302 腿不动。
- **自定义响应头（oj-8，v0.1.30）**：`server.response_headers`（全局）+
  `server.static_sites[].headers`（per-site 覆盖同名全局值）；施加动态信封/静态站点/blob
  响应面；**框架自有头永远优先**（配置头只补缺，Content-Type/Location 等不可覆盖）。
- **ABI 9（破坏性，插件必须重编译）**：`AuthGuardVtable.verify` 四参化
  `(path_no_base, method, authorization, headers)`——headers 为全部请求头 JSON（小写名→值），
  cookie 会话与 CSRF 判定材料都在其中；`BlobBackendVtable` 增 `upload_url(handle, key, op)`。
  `xtask` 第一方插件清单补 `ldap`（此前 `cargo xtask build` 漏装 oj-ldap，bin/plugins
  会滞留旧 ABI 产物）。

**修复**

- 模块加载器：相对 require 原可 `../../` 逃出 project root——新增 `ensure_within` 钳制。
- `cargo xtask build` 漏构建 oj-ldap（PLUGINS 清单缺项）。

**文档**

- `docs/devkit/` 四件同步本版用户可见变更（api-manual §5/§6/§7/§8/§10、SKILL 陷阱速查、
  scenarios 新增 5 场景、README 版本行）；`docs/plugin-architecture.md` ABI 版本号 8 → 9。

## v0.1.29（2026-09-28，已打标签 v0.1.29）

> 版本分界：`oj/Cargo.toml` 0.1.28 → 0.1.29。上一版：`v0.1.28` → 515c33f。
> 发布点标签：`v0.1.29`（终审通过后打在发布分支末端）。

**特性（oj exec 子命令）**

- **`oj exec <file.ts|js>`（v0.1.29）**：直接执行 ts/js 脚本，完整注入后端全局
  （json/db/kv/blob/bus/es/fetch/ws/log/plugins/cert/jwt/bcrypt/crypto/oidc/ldap/mail/mq，
  经 `assemble_backend` 装配，与 server 同源）。`console.*`/`log.*` 终端 stdout
  直出；`--log-file` 双写 JSONL（打开失败仅告警不中断）。`--` 之后 argv 注入
  `globalThis.args`。脚本可 import 项目根内 .ts/.js（须带显式扩展名）。
  专题手册：`docs/exec-integration.md`。
  - CLI：`exec <file> [-c config] [-d dir] [--db name] [--log-file path] [-- arg...]`
  - **与 server 的装配差异（有意为之）**：无证书门禁；迁移默认 off（仅 config
    显式 `migrate_on_start` 才执行，server dev 缺省 auto——两命令相反，勿混用）；
    无 KillSwitch（同步死循环 Ctrl-C 兜底）；`sql_guard=deny` 的库上脚本须
    `db.asSystem()`；`console` 仅 exec 可用（server/test runtime 无此全局）；
    `json.*`/`finish` 空转（无 ReqState 消费方）。
  - **拆分重构（行为不变）**：`App::from_config` 拆出 `assemble_backend`——
    `Backend = { stable, auth_guard, make_bridge_of }`，StableState 单源；
    `schema::reconcile_all` 独立化；`mail::install_mail_deliver` 显式化
    （server 经 Bridge 构造、exec/test 手工 runtime 经 assemble_backend 各装一次）。
    已知次序差异：多故障场景下 fail-fast 报错先后序有变化（如证书门禁错误改为
    后端装配错误之后才报出），单故障场景报错文案不变。

## v0.1.28（2026-09-25，已打标签 v0.1.28）

> 版本分界按仓库约定落在 `oj/Cargo.toml` 的递增提交上（本版 `0.1.27 → 0.1.28`）。
> 发布点标签：`v0.1.28`。上一版：`v0.1.27` → `465322c`。

**修复**

- **Linux 自带 glibc 发行包 `--daemon` 启动即退（v0.1.28）**：根因是 daemon 化走
  `current_exe()` re-exec 真实二进制 `oj.bin`，kernel 按 `oj.bin` 的 PT_INTERP
  （系统 `/lib64/ld-linux-x86-64.so.2`）加载，绕开了 deploy.sh 启动器精心构造的
  打包 glibc（`ld-linux --library-path lib/`），glibc 版本低于产物需求的宿主上
  re-exec 后即退出。修复：启动器 export `OJ_BUNDLED_LD` / `OJ_BUNDLED_LIB` 标记，
  `daemonize()` re-exec 时优先经打包 ld-linux + `--library-path` 启动（非打包形态
  env 缺省，行为不变）。
- **Windows CI 单测 `app::tests::resolve_static_sites_*` 报 InvalidFilename（code 123）**：
  测试夹具 `tmp_dirs` 用 `{names:?}` Debug 格式拼临时目录名，`[`/`"` 是 Windows
  非法文件名字符；改用 `names.join("_")`。纯测试修复，无行为变更。

**特性（PRD `docs/prds/event-cqrs.md` v2 §9 阶段 1：任务域事件化）**

- **双模长任务（v0.1.28）**：`src/tasks/` 下的任务文件按**导出探测**分流——
  导出 `loop_body` 命名导出的文件走**池化任务**（新）；其余（顶层 await 驱动，如
  MQ 消费循环）走存量 TLA 监督器，行为与旧版完全一致。
  - 池化任务三钩子：`setup`（连接时执行一次，可缺省）/ `loop_body`（每轮调用，
    **必须命名导出**，超时 `tasks.pool.loop_body_timeout_ms` 默认 5s——超时或异常
    → teardown 收场、状态 Failed）/ `teardown`（停止/失败/停机时执行，尽力而为不承诺完成）。
  - 池化任务**不需要** `tasks.stopping()` 轮询与 `tasks.sleep()`：每轮 loop_body 返回
    后由 worker 按注册表 desired 位决定是否再调一轮；停机时先跑 teardown 再退出。
  - `tasks.pool.workers`（默认 4）个常驻 worker 线程，任务静态轮转分配（本地轮换，
    无共享调度器）；任务模块在 worker runtime 上预载一次（V8 模块缓存，零重编译）。
- **cron 定时任务（v0.1.28）**：`tasks.crontab`（默认 `task/crontab.yaml`，**相对
  `tasks.dir`**，条目路径同）逐行 `分 时 日 月 周  路径`，5 字段自研解析（`*/n`、列表、
  区间、周字段 7=周日）；单驱动调度器到点派发一次性作业（整模块跑一次、跑完释放
  Worker，先写 next_run 再派发防重入）。坏行 fail-fast（报错带 `:行号:`）；与池化
  任务同名拒启；cron 文件用普通命名（`task_*.ts` 会被扫描器收编成长任务）。
- **任务管理 API（v0.1.28）**：`{base}/tasks` 控制面——`GET {base}/tasks`（`?type=long|cron`）、
  `GET/PATCH/DELETE {base}/tasks/{name}`（PATCH 仅接受 `{enabled, cron}`）、
  `GET .../{name}/logs`、`POST .../{name}/start|stop|reload|run-once|enable|disable`。
  鉴权（AuthGuard）与租户头语义与业务路由**完全一致**；跨 kind 命令返回 400 并给提示
  （cron 用 enable/disable，long 用 start/stop）；**run-once 仅 cron 任务**（long 400、
  运行中 409、无任务池 503）；DELETE 仅运行时注销（文件仍是事实源）。任务为实例级
  资源，本期无按租户隔离的视图。
- **任务事件（v0.1.28）**：薄事件信封 `{eventId, eventType, timestamp, payload}`
  （camelCase 契约，FR-EB-005；内存池：JSONL 日志 `tasks.event_log.path`（默认
  `logs/task-events.jsonl`，超 `max_mb`（默认 16）轮转 `.jsonl.1`）+ 1000 条环形缓冲，
  `logs` 端点可查）。
- **轮间节奏（v0.1.28）**：池化任务每轮 `loop_body` 返回后框架按
  `tasks.pool.interval_ms`（默认 100ms；**`0` = 不限制**，立即轮转，压测语义）sleep
  再调下一轮——防 trivial loop_body 空转独占 Worker。
  **性能对比（同机 11s 窗口实测）**：单任务 kv get+set + log.info——无节奏 60,149
  轮/s、CPU 峰值 54.7%、6 万行日志/s；默认 100ms ~9 轮/s、CPU 4.7%。4 任务 kv
  get+set——`interval_ms: 0` 聚合 247,752 轮/s（每任务 ~62,000，4 Worker 各烧一
  核，CPU 峰值 133.7%）；默认 100ms 聚合 39 轮/s（每任务 ~10）、CPU 12.5%。
  多任务轮转公平性有 BDD 测试（`given_two_loop_tasks_when_pool_runs_then_both_advance`）。
  调优建议（写放大核算、增量游标、长轮询走 TLA）见 `docs/devkit/api-manual.md`
  §6「性能特征与调优」。

**特性（ldap 轴，v0.1.28）**

- **LDAP 目录与鉴证**：新 cdylib 插件 `oj-ldap`（ldap3 0.12.1 纯 Rust 客户端，
  `tls-rustls-aws-lc-rs`，零 ring/零 C 库）+ 宿主 `ldap.*` 全局。配顶层 `ldap:` 段
  （实例名 → url/bind_dn/bind_pw/timeout_ms/start_tls/tls_skip_verify）即启用：
  - JS API：`ldap.bind(dn,pw): Promise<boolean>`（凭据被拒返回 false，不抛；
    连接/协议错抛异常）、`ldap.search(base,{scope,filter,attrs})`、
    `ldap.searchPaged(...)`（RFC 2696 分页聚合，pageSize 默认 500）、`ldap.whoami()`、
    `ldap.compare(dn,attr,val)`；`ldap === new LDAP("default")`，命名实例 `LDAP(name)`。
  - 返回条目 `{dn, attrs:{k:[v]}, bin:{k:[base64]}}`（二进制属性 base64 编码）。
  - 连接模型：每调用独立 connect → 服务账号绑定 → 操作 → unbind（无连接池；
    用户凭据绝不共享连接）；`bind_dn`/`bind_pw` 成对，不配则匿名 search。
  - 装配：新轴按轴 dlsym（ABI 不变，加轴零破坏）；`ldap:` 段与 `plugins.ldap`
    透传皆非空时装配期 fail-fast（二选一，防静默遮蔽）；宿主/插件双侧白名单校验
    （未知键/坏 url 启动即报错）。
  - 文档：devkit 四件套同步（api-manual §6 ldap 节 + §13 限制表、SKILL 陷阱、
    scenarios 场景 11「LDAP/AD 登录鉴证」）。

**配置（新增）**

```yaml
ldap:
  default:
    url: ldaps://dc.example.com:636
    bind_dn: cn=svc,ou=app,dc=example,dc=com
    bind_pw: "change-me"
    timeout_ms: 5000

tasks:
  pool:
    workers: 4                  # 池化/cron 任务 worker 线程数
    loop_body_timeout_ms: 5000  # 单轮 loop_body 看门狗超时
    interval_ms: 100            # 轮间节奏（每轮返回后 sleep 再下一轮，防空转独占 Worker）
  crontab: task/crontab.yaml    # cron 清单（tasks.dir 相对）；缺省不启用
  event_log:
    enabled: true
    path: logs/task-events.jsonl
    max_mb: 16
```

**兼容**

- 存量 TLA 任务（MQ 消费等）零改动：探测不到 `loop_body` 即归入旧监督器；
  `tasks.stopping()`/`tasks.sleep()`/退避重启语义不变。
- 不启用池化任务 / cron 时（无 loop_body 导出、无 crontab 文件）不创建任务池，
  零开销。

详见 `docs/prds/event-cqrs.md` v2（评审修订版）与 `docs/devkit/` 四件套。

**构建产物压缩升级（v0.1.28）**：`oj build` 默认 minify 从 codegen 级（去空白/注释）
升级为 **swc_ecma_minifier 全量压缩**——死码消除 + 表达式压缩 + **函数内局部变量名
混淆**（基础混淆）。mangle `top_level=false`：顶层/导出名不动（跨文件 import 与
routes.js `file` 字段不受影响），`json`/`db`/`http` 等注入全局是属性访问、不是绑定，
天然不被碰。产物确定性不变（同输入 tgz 字节一致）。`--no-minify` 排障逃生门不变。
依赖：新增 `swc_ecma_minifier 36`（与 deno_ast 0.53.3 的 swc 族对齐）、deno_ast 加
`visit` feature。文档：`docs/devkit/api-manual.md`「构建与发布」产物表。

**文档人话化与对齐（v0.1.28）**：全套对外文档（devkit 四件 + `user-manual` / `dev-guide` /
`db-guide` / `testing` / `ops-manual` / `tenant-guide` / 插件文档 / 专题 12 件 / 根 `README`）
拆长句、去 AI 腔，按新人视角补「给谁读/什么时候读」与首次术语解释；顺带修一批与代码的
漂移——README/ops-manual 的 build/test/clippy 命令补 `--release` 并改走 `cargo xtask build`
+ `bin/oj`（v0.1.27 起产物归置 bin/）、结构检查清单 S001–S007→S001–S008、
`docs/modules/` 的 `ABI_VERSION 7→8` 与第一方插件 8→9（补 `oj-mail` 行）、
`scenarios.md` 索引补齐场景 9/10。

## v0.1.27（2026-09-25）

> 版本分界按仓库约定落在 `oj/Cargo.toml` 的递增提交上（本版 `0.1.26 → 0.1.27`）。发布点标签：
> **`v0.1.27`**（annotated，打在标签状态修订提交上）。上一版：`v0.1.26` → `b91a611`。

**特性**

- **`_name_` 目录段即路径参数（v0.1.27）**：文件系统中的动态参数目录段用 `_name_` 整段
  表示（避免 `{}` 进文件路径带来的 shell 转义成本），映射 URL 时转换为 `{name}`。
  例：`src/user/_id_/api.ts` → `/v1/api/user/{id}`；`src/_aa_/bb/_cc_/api.ts` →
  `/{aa}/bb/{cc}`。模块段同样适用。
  - dev 建表、dev 目录镜像兜底、release（`oj build` 生成的 routes.js）三链路同口径；
    兜底经 `_x_` 段下降（带回溯，对齐 matchit 静态优先）并提取参数，复用 `decode_params`
    走私防线。
  - 谓词：整段 `_name_`（首尾各一个下划线、内部名不以 `_` 开头/结尾、不含 `{}`）。
    `__x__`/`___`/`_a{b}_` 保持字面；`_shared`/`_platform` 等无尾下划线目录不受影响。
  - **`oj build` 构建期 pattern 试插校验**：非法 pattern / 同位异名参数在 build 期即失败
    （此前要到部署启动才爆）。
  - **告警**：`.route` 值匹配 `_name_` 形态（在 `.route` 中它是**字面段**，参数写 `{name}`）
    与 `_name_` 形态模块名（同位异名双模块部署启动会结构冲突）——dev 启动与 oj build
    均打 warn（`warning: ` 前缀，不致命）。
  - **启动路由清单改统计输出**：不再逐行打印 METHOD/PATH/FILE 三列表，改为一行
    `routes: N method-row(s), N pattern(s), N api file(s)`；路由错误与冲突仍逐条
    输出具体 method/pattern/文件（dev 告警 + release 硬失败不变）。
- **多静态站点（`server.static_sites`，v0.1.27）**：静态站点从单 `--app-path` 扩展为
  **前缀→目录映射**的多站点。legacy `server.app_path` + `server.app_prefix` 对保持
  不变（单站点特例）。
  - 配置：新增 `server.static_sites: [{prefix: "/docs", path: "dist/docs"}]` 列表；
    `path` 相对 config 目录解析。CLI：`--app-path` 可重复——裸 `dir`（至多一次，
    覆盖主站点 `app_path`）或 `prefix=dir`（如 `--app-path /docs=dist/docs`，同前缀
    覆盖/新增 `static_sites` 条目，CLI 优先）。
  - 请求期 **最长前缀命中**（`/` 为兜底 catch-all）：`/docs/api/x` 优先命中 `/docs` 站
    胜过 `/` 站。命中站点内未命中 → **仅该站** SPA 回落，**不跨站**；meta JSON
    （`html_meta`）按命中站点各自的根目录解析。
  - fail-fast：归一后前缀重复（报两条来源）或目录缺失 → 拒绝启动。
    `spa_fallback` / `html_meta` / `html_meta_handler` / `html_cache_control` 仍为
    全局开关，对各站点一致生效。

**升级注意（breaking-adjacent）**

- 存量项目若已有**字面目**的 `_x_` 目录（如 `_v1_/`），升级后其段将变为参数段：URL 仍
  可达（实参 `_v1_` 落进参数），但 handler 会收到非空 `http.params`，启动 banner 的
  路由表会显示 `{x}` 形态。确有字面需要的目录请避免首尾下划线写法。

## v0.1.26（2026-09-22）

> 版本分界按仓库约定落在 `oj/Cargo.toml` 的递增提交上（本版 `0.1.25 → 0.1.26`）。发布点标签：
> **`v0.1.26`**（annotated，打在标签状态修订提交上）。上一版：`v0.1.25` → `e2e4c60`。

**特性**

- **302 redirect 原语（`json.redirect`，v0.1.26）**：此前 oj 无 redirect 原语，要跳转只能
  手工拼 `json.header("Location", url)` + `json.fail(302, …)`——能跑通，但拼出来的是
  **失败信封体**（浏览器虽忽略，非标准形态）。本版新增原语，遵循 RFC 9110 §15.4：
  - `json.redirect(url, code?)`：3xx + `Location`；`code` 非 3xx 一律回落 302（原语层面
    杜绝「200 + Location」的畸形响应）；空 url 忽略 `Location`。
  - body 按 §15.4 SHOULD：含目标链接的短超文本注记（`<a href="…">{reason}</a>.`，
    `net/http` Redirect 同款形态）；**HEAD 请求豁免为空 body**；content-type 默认
    `text/html; charset=utf-8`（`json.header` 显式设置的优先，大小写不敏感）。
  - 具名封装（语义即注释，对应五个标准 3xx）：`movedPermanently`(301) / `found`(302) /
    `seeOther`(303，跟随后改 GET) / `temporaryRedirect`(307，方法/体保持) /
    `permanentRedirect`(308，永久 + 方法保持)。
  - 典型场景：权限校验后把请求 302 到 `blob.url()` 给出的 S3 预签名 URL，浏览器两跳
    直取对象（样例 `sample/src/redirect/`；devkit `scenarios.md` 场景 8）。
  - 落点：`src/bridge/json.rs`（`op_json_redirect` + reason phrase/HTML 转义）、
    `src/bridge/mod.rs`（op 注册）、`src/bridge/bootstrap.js`（`json.redirect` 及具名封装）。
    测试：根 crate 单测（默认 302/显式 307/非 3xx 回落/具名封装/转义/HEAD 空 body）+
    oj e2e（302 首跳注记、HEAD 空 body、回落、seeOther 303）。

## v0.1.25（2026-09-21）

> 版本分界按仓库约定落在 `oj/Cargo.toml` 的递增提交上（本版 `0.1.24 → 0.1.25`）。发布点标签：
> **未打标签**（本节随实现提交，标签动作未执行）。上一版：`v0.1.24` → `49d3963`。
>
> 本版回应下游消费方（plane）提交的「平台能力缺口」清单（`docs/ever/upstream-pr-platform-capabilities.md`）。
> 复核结论：清单里两项**平台早已具备**——「邮件试发 + 结果回执」= v0.1.19 的 `mail.send`/`sendSync`
> 内联投递结果 + `mail.result`（bus 与回查双通道）；「SPA 深链回落 + 按路由 meta」= v0.1.20 的
> `server.app_spa_fallback` + `server.html_meta`。根因是**下游 vendored 的 oj 落后 13 个小版本**
> （`oj-module/bin/.oj-version = v0.1.11`），不是能力缺失；平台侧为此补了一处**误导下游的文档硬伤**。
> 真正缺的两项在本版补齐：**数据驱动的 per-route HTML meta 注入**（静态 JSON 只能覆盖构建期已知
> 路由，issue 标题这类**按数据**的 SEO/IM 预览要它）与**部署期常量读口**（下游只能把 `WEB_URL`
> 编译进产物）。清单第三项（契约 codegen 的 multipart）**不在本仓**——落点是框架件 `oj-module`
> 的 `packages/cli/src/contract/*`，本仓后端早已支持（multer 解析 + `http.files`/`http.file(i)` +
> `blob.put`），仅需纠正其文档里臆造的 `field()`/`storeBlob` 名字。

**特性**

- **动态 HTML meta（`server.html_meta_handler`）**：配一个**业务 handler 的路由路径**，送静态 HTML
  前内部派发它（GET，`?path=` 传**已剥 `app_prefix`** 的站点内路径），用返回的 JSON 注入
  `<head>`——与 `html_meta` **同一白名单键**（`title`/`description`/`canonical`/`og:*`/
  `twitter:*`）与**同一转义**，另可带保留键 `cache_control` 覆盖本响应缓存头。静态
  `<html_meta>/<path>.json` 打底、动态**按 key 覆盖**（异名保留）。
  - **注入是替换不是追加**（`inject_head`）：浏览器与爬虫只认**第一个** `<title>` / 同名
    `<meta>`，故先摘掉 head 里同名的既有标签再放新标签——壳里写死的 `<title>App</title>`
    也能被接管（否则「注入成功」是假的：文档里两个 title，生效的仍是旧那个）。`<script>`/
    `<style>` 内容当不透明文本整段跳过；`og:*`/`twitter:*`/description/canonical 一并去重。
  - 顺带修掉 v0.1.20 继承来的 `to_lowercase()` 索引错位隐患（大小写变换会改字节长度，非 ASCII
    前文会让注入点错位）→ 改 ASCII 不敏感**字节**扫描。
  - 为什么是 handler 而不是「注册回调」：oj 的扩展面只有路由 handler（复用既有超时/日志/
    `RequestInfo` 形态，零新形态），且构建期 JSON 与运行期数据的分工一目了然。
  - **恒以匿名身份派发**（`tenant_id = None` + `anonymous = true`，租户头/请求头/请求体一律不
    传递；`http.tenantId` 恒 `null`）。这是安全边界而非省事：页面请求带什么头由客户端决定，
    若透传 `X-Tenant`，① `sql_guard: deny` 下构造器会把**攻击者指定的租户**当过滤条件（匿名
    访客即可把某租户的行读进公开 meta），② `db.asTenant` 的三道门禁要求 `tenant_id` 为空，
    透传会让它必抛。按租户取数只有一条正路：handler 从 URL 派生 id → `db.asTenant(id)`
    （需 `tenant.allow_as_tenant: true`）。该 handler **不需要**进 `anonymous_paths`，被外部
    直接访问时仍受守卫约束。
  - **fail-open**：未命中/非 2xx/超时/非 JSON/信封 `code != 0` → WARN 后按静态结果送出；`cache_control`
    类型写错只丢该键（其余 meta 照常）。装配期 **fail-fast**：路径必须命中一个 **GET** 路由
    （拼错不静默降级——后果只在爬虫/IM 预览侧可见，留给运行期等于让配置撒谎）。
  - 落点：`src/config.rs`、`server/src/lib.rs`（`static_page` / `dispatch_meta_handler` /
    `inject_head` + `strip_conflicts` / `element_at` / `attr_value` / `find_ci`）、
    `oj/src/app.rs`（`validate_html_meta_handler`）。顺带把 `handle()` 内的 `run` 闭包提成自由
    函数 `run_route`（闭包部分移动 `st`/`headers`，静态兜底就没法再借用它们——与
    `strip_app_prefix` 同款理由）。
- **HTML 缓存头（`server.html_cache_control`）**：注入后同一份壳可能因路由而异，不能再让中间层
  盲缓存。**只管 HTML**（js/css/图片等资源不受影响，其长缓存仍交前置反代）；动态 handler 的
  `cache_control` 优先于本项；两项都没有 = 不加头（与旧版逐字节一致）。**可独立使用**——只想给
  SPA 壳挂 `no-cache` 而不做注入时，单配本键即生效（三键彼此独立）。
- **部署期常量读口（顶层 `vars:` 段 + JS `vars.get(name)`）**：`WEB_URL` 这类「换域名即变」的
  常量不必再编译进产物。**同步**读（装配期冻结表，无 IO，同 `plugins()`）；**fail-closed**——只有
  本段声明的键可读，其余恒 `null`，平台**没有**「读任意 OS env / 任意 config 键」的通道（旧的
  三层 env 叠加已删，单文件 config 是唯一真相源；`db:` 的 DSN、`server.public_key_path` 等敏感面
  因此不可能经此泄漏到 JS）。值按 YAML **标量**文本成串（`PORT: 3000` → `"3000"`），嵌套 map/list
  解析期报错。落点：`src/config.rs`、`src/bridge/vars.rs`（新 op）、`src/bridge/mod.rs`
  （`StableState.vars` / `Extras.vars`）、`src/bridge/bootstrap.js`、`oj/src/app.rs`、`sample/global.d.ts`。

**修复（文档）**

- **`api-manual.md` 的 `app_path` 行自 v0.1.20 起就是错的**：写「无 SPA 回退/Range/ETag」，而
  `server.app_spa_fallback` 在 v0.1.20 就落地了。下游这份 PR 文档正是引用这条口径推出「平台连深链
  回落都没有」的结论——本轮把四个静态键（`app_spa_fallback`/`html_meta`/`html_meta_handler`/
  `html_cache_control`）补进 server 键表与已知限制表，并在 `scenarios.md` 场景 2 加「按路由注入
  title/OG」的可照抄链路（静态 JSON + 动态 handler + `curl` 验收）。`vars` 进全局章；`SKILL.md`
  陷阱速查加五条（深链 404 / 注入是替换 / 派发恒匿名的租户取数姿势 / 生产反代直出时注入不生效 /
  `vars.get` 恒 null）。**同时显式写明部署形态前提**：注入只在 oj 自己送静态文件时生效，生产由
  nginx/Caddy/CDN 直出 SPA 的部署要么改走 oj 托管，要么在反代层做同样的事——否则读者会
  over-claim「平台已解锁生产 SEO」。
- **同一条错口径还散在 `user-manual.md`（已知限制清单）与 `ops-manual.md`（静态 404 排障行）**：
  两处一并订正（前者补 SPA 回落 + per-route meta + HTML 缓存头的现状，后者把「无 SPA 回退」
  改成「未开 `server.app_spa_fallback`」）。这两条是**下游文档站同步**时才暴露出来的——说明
  「订正一处口径」必须全仓 `grep` 同义表述，不能只看被引用的那一行。

**测试**

- `server`：`html_meta_handler_overrides_static_and_sets_cache_control`（打底+覆盖合并、**壳里旧
  title 被替换且只剩一个 title**、per-route 缓存头、`code != 0` fail-open、无静态 JSON 时动态单独
  生效）、`html_cache_control_alone_still_applies`（只配缓存头也生效、资源不受影响）、
  `html_meta_handler_off_is_byte_identical`，外加注入器单测四条（替换壳 title / 同名 meta 与
  canonical 去重且不误伤 `data-name=` / head 里 `<script>` 内容不被摘 / 非 ASCII 前文不错位不 panic /
  无 `</head>` 与空表零副作用）；既有 `html_meta_*` 用例补「无缓存头」的向后兼容断言。
- 根 crate：`bridge::vars`（声明键可读 / 未声明恒 null / 空串与未声明可区分 / 同步值）、
  `config::tests::vars_section_reads_scalars_and_rejects_nested`。
- `oj`：`validate_html_meta_handler` 单测（命中含尾斜杠归一 / 未命中 / 只有 POST / 非法路径 / 未配）
  与 e2e `html_meta_handler_injects_per_route_tags_end_to_end`（真装配 + 真静态兜底 + `Accept: text/html`
  深链注入 + 旧 title 被替换 + 缓存头 + API 前缀 404 不被吞）。
- 双路专家评审（开发侧 + 架构侧）抓出并已处置：注入只追加不替换（功能空转）、只配
  `html_cache_control` 被早退吞掉、租户头透传给无守卫派发（越权读面）、`to_lowercase()` 索引
  错位、`cache_control` 类型错连坐整份 meta。

## v0.1.24（2026-09-19）

> 版本分界按仓库约定落在 `oj/Cargo.toml` 的递增提交上（本版 `0.1.23 → 0.1.24`）。发布点标签：
> **`v0.1.24`**（annotated）。上一版：`v0.1.23` → `039b314`（含双专家评审处置）。
>
> 本版是**清账版**：清掉 v0.1.22 登记的五条已知债（PG 语句缓存分键 / u64 全链路 / 数值型
> `tenant_id` 列 / `db.nextSeq` / MySQL 真库验证），并含**双路专家评审**（架构侧 + 开发侧）的
> 逐条处置——其中两条是评审抓出的真问题（MySQL 类型化行解码的静默 `null` 回归、
> `db.nextSeq` 建表缓存串键）。

**清账：v0.1.22「已知债 / 另案登记」五条**

设计与逐条证据见 `docs/superpowers/specs/2026-09-19-debt-clearing-v0.1.22-design.md`。

1. **PG 语句缓存 × 混合参数类型（债①）——已修**。根因复核：sqlx 的语句缓存 key **只是 SQL
   文本**（`statement_cache.rs` 的 `LruCache<String,_>`），PG 执行路径**无条件查缓存**并把旧的
   `param OIDs` 复用到本次 Bind → 同文本换参数 Rust 类型即协议级错误（真库实测
   `insufficient data left in message`）。修法：**按参数形态分缓存键**——PG 插件（`shape_tag`，
   4 个执行点）给 SQL **前置**块注释 `/*oj:<形态>*/`（形态字母表 `t/i/f/b/m/u`；必须前置，
   后置会被 PG 判多语句或被 `--` 注释吞掉），并给 PG 连接补
   `statement-cache-capacity=512`（分键后条目数 ×形态数，防 LRU 抖动触发 Close+Sync 往返）。
   **MySQL 不动**：复核其每次 execute 都重发参数类型（`new_params_bound_flag=1`），病不在此。
   验收：env-gated 真库用例 `real_postgres_same_sql_text_mixed_param_shapes`（改前必红 → 现绿，
   含池路径/tx 路径/缓存条目数断言）。
2. **u64 / `BIGINT UNSIGNED` 全链路（债②）——已修**。新增线形状 `{"$oj$u64":"<十进制>"}`
   （`oj-plugin-ffi::jsint`，**不 bump ABI**，但**插件须与宿主同批重建**）与 JS 全局
   `toUBigInt(v)`（`[0, 2^64-1]`，越界 RangeError）；`toBigInt` 对越 i64 的 bigint 改为明确报错
   并指路 `toUBigInt`。MySQL 插件迁到 **typed 路径**（`Pooled` 枚举：`mysql://` → `sqlx::MySql`，
   离线测试仍走 `Any`+sqlite 保 CI 覆盖），行解码改为**固定安全顺序**
   `u64 → i64 → bool → f64 → String → bytes`（`bool::compatible`/`f64::compatible` 都接受整型，
   顺序反了会把 BIGINT 读成 bool）；**读侧不再回绕成负数**。PG/SQLite 对 `$oj$u64` **明确拒绝**
   （bigint 就是 i64）。顺带修一处既有漏洞：`toSQL().params` 的超界整数改为回吐 marker
   （此前被出口护栏降成字符串 → 用户照文档「回放 params」必失败）。
   **顺带修真实缺口**：MySQL 8 默认 `caching_sha2_password`，明文连接需 sqlx 的 `mysql-rsa`
   feature——缺它插件**连不上任何 stock MySQL 8**（本机 8.4 实测报
   `RSA auth backend disabled`），已补。
   验收：`real_mysql_unsigned_bigint_roundtrips_as_u64`（`u64::MAX` 与 `i64::MAX+1` 精确往返）。
3. **数值型 `tenant_id` 列（债③）——已从「不支持」变为支持**。列类型从 schema.yaml plumb 到
   `SchemaRegistry`（`ColumnType`，旧构造器默认 `Unknown` = 旧行为）；`apply_tenant` 按列类型
   生成绑定值（text/Unknown → 字符串；integer/bigint → 数值，超 i64 用 `$oj$u64`；非十进制租户头
   → 指名报错），insert/update 的等值判定改走 `guard::param_is_tenant`（字符串/数字/i64 标记/
   u64 标记四形态），join 的 ON 条件同型。**新增声明期 fail-fast**：`tenant_id` 列类型 ∉
   {text,integer,bigint} → 启动报错（数值列上的 `bigint = text` 不再等到运行期）。
4. **内建序列分配原语（债④）——已做**：`db.nextSeq(name)`（`DBInstance` 与 `db.tx` 内均可）。
   平台表 `_oj_sequences(name, v)` 首次使用自动建（不经模块 schema/迁移）；取号**单语句原子**：
   PG/SQLite `insert … on conflict(name) do update set v = v+1 returning v`；MySQL
   `insert … values (?, last_insert_id(1)) on duplicate key update v = last_insert_id(v+1)`
   再 `select last_insert_id()`（必须同连接 → 无活跃事务时宿主用一次短事务）。返回值过 `jsnum`
   规则（≤2^53-1 给 number，超出给十进制字符串）。**这是 `select max(id)+1` 竞态的终态**。
   验收：离线 sqlite（稠密/并发/名称校验/事务搭车）+ PG 真库 20 并发（断言恰为 1..20）
   + MySQL 真库（顺序 1..3 + 12 并发无丢失更新）。
5. **MySQL 侧真库验证（债⑤）——已执行**。「本机拉不到镜像」的前提已消失：用 macOS `container`
   CLI 起 `mysql:8.4`（4C/1G，专用库 `oj_test`），MySQL 插件的全部 env-gated 用例首次实跑并全绿
   （roundtrip / i64 marker / u64 unsigned / 序列原子性）。同批实跑：PG 18 全绿、Redis 全绿。
   **未跑**：Kafka（9092 未起）、S3（poc-minio 缺预建桶与凭据，需 `mc mb` 先建
   `oj-test` 桶）、RabbitMQ（`guest` 仅 loopback，`poc` 用户需在容器内自建；本轮未做）。

**兼容性 / 行为变更（升级前请核对）**

1. **`toSQL().params` 的超界整数由「十进制字符串」改回 **marker**（可重放）**：此前该值被出口
   护栏降成字符串，导致文档教的「`db.query(toSQL().sql, ...toSQL().params)` 回放」必然失败
   （`bigint = text`）。现在 `>2^53-1` 的整数以 `{"$oj$i64":…}` / `{"$oj$u64":…}` 形式出现——
   比较 params 文本的代码需留意。
2. **`tenant_id` 非法列类型由「运行期 PG 报错」变为「启动期报错」**：`tenant_id` 只能是
   text/integer/bigint（`double`/`boolean`/`blob` 直接 fail-fast）。
3. **MySQL 行 JSON 形状随 typed 迁移改变**：整数列统一给 number（Any 时代部分类型落到字符串/
   字节）；`BIGINT UNSIGNED > i64::MAX` 由**回绕成负数**改为精确值（出口护栏再把 `>2^53-1`
   降成十进制字符串）。
4. **`toBigInt` 对越 i64 的 bigint 由「静默返回」改为明确报错**（指路 `toUBigInt`）。
5. **PG 实际执行的 SQL 文本带 `/*oj:<形态>*/` 前缀**（DBA 视角可见；`toSQL()` 不含）。
6. **宿主与第一方插件必须同批重建**：`$oj$u64` 是源码级共享的线形状，不 bump ABI，但旧插件会把
   新标记串化成文本。**混版本不受支持**（复现门槛低，见下方挂账的线形状门禁）。
7. **核心 crate 的 `[dev-dependencies]` 增 sqlx pg/mysql 驱动**（仅测试构建；core 级真库用例用）。
8. **MySQL 读侧的列类型边界（本批定稿）**：**可读** = 整数家族（含 `BIGINT UNSIGNED` → u64）、
   文本家族（`TEXT` 在 MySQL 协议里就是 `Blob` + 非 BINARY collation）、二进制、`FLOAT`/`DOUBLE`；
   **不可读** = `DECIMAL`/`NEWDECIMAL`、`DATE`/`TIME`/`DATETIME`/`TIMESTAMP`/`YEAR`、`JSON`、
   `BIT`、`GEOMETRY` → **报错**（点名列名 + MySQL 类型 + 指路 `cast(x as char)`），**绝不静默
   `null`**。对照 v0.1.23（全程 `sqlx::Any`）：这些类型当时同样读不出来，但错误是 sqlx 的
   `AnyDriverError`（不点名列）；且 `TINYINT`/`BOOLEAN` 当时**直接报错**，现在按整数读出 **`1`/`0`**
   （`BOOLEAN` 是 `TINYINT(1)` 的别名——**不是** `true`/`false`，本批无类型长度元数据可区分）。
9. **数值型 `tenant_id` 列在 `tenant.sql_guard: warn` 下也会硬失败**：租户头不是十进制整数时
   `tenant_value` 直接 `Err`（warn 模式只放行「无法判定」的情形，不放行「判定为不匹配」）；
   `text` 列的旧行为不变。注意这与「warn = 只告警」的直觉不同。
10. **`update({tenant_id: …})` 的等值判定改走 `param_is_tenant`**：text 列 + 数字租户头
    （如租户 `"7"`、字段写 `7`）由**拒绝**变为**放行**（放宽；两者语义等价）。

**新登记债务（清账过程中发现）**

- **deno_core op 驱动在并发等待时 `RefCell already borrowed` → abort 进程（既有，非本次引入）**：
  触发条件至少两种——① JS 侧同时发起的 op 数**超过 sqlx 池上限**（默认 10）；
  ② **在 JS 里 `await` 之后再发起 op**。复现（真库，非推测）：核心 accessor 连 PG 后
  `Promise.all` 发 16 个 `db.query("select 1")` → `op_driver/futures_unordered_driver.rs:309`
  panic 且 `panic in a function that cannot unwind` → SIGABRT（**整个进程挂掉，不只是该请求失败**）。
  与本次新增的 `db.nextSeq` 无关（`db.query` 同样复现）；10 并发以内、单轮发起时正常。
  本轮的 `db.nextSeq` 真库并发用例因此改由**插件层**驱动（`plugins/oj-db-*`），core 侧只留
  ≤8 并发的离线用例。**需专项处理**（要么在 op 包装层串行化/限流，要么查 deno_core 0.411 的
  op 驱动用法）。

- **线形状版本门禁缺失（架构评审 P1-1，本轮只登记）**：`$oj$u64` 这类跨边界线形状没有独立版本号，
  `HOST_FINGERPRINT` 不含它、且指纹不符**只告警不 fail**（`src/bridge/plugin_loader.rs`）→
  「宿主换新、插件留旧」不会被拦。建议后续引入单调递增的 `WIRE_VERSION`（可塞进 descriptor 字符串
  字段以免 bump repr(C)）并对它做**硬门禁**；本轮以文档纪律（§兼容性第 6 条 + `plugin-architecture.md`）
  替代，混版本明确不受支持。**本轮已做的最小缓解**：`oj-plugin-ffi` 版本 `0.1.0 → 0.1.1`
  （`HOST_FINGERPRINT` 含该版本号）——旧插件（用 0.1.0 构建）在新宿主上至少会打一条
  `fingerprint mismatch` 告警，把「静默混版本」变成可见信号。
- **MySQL 非整数/非文本列的读出（原 R3 的一部分，本轮只登记）**：`DECIMAL`/`JSON`/时间/`BIT`/
  `GEOMETRY` 目前**报错而非解码**（见 §兼容性第 8 条）。彻底修需要按 `MySqlTypeInfo` 做显式分派表
  （每种 MySQL 类型的期望 JSON 形状：`DECIMAL` → 十进制字符串、`JSON` → 对象、时间 → 字符串…），
  属独立设计，本轮先保证「不静默错值」。
- **单连接 accessor 在活跃事务期间池上发 DDL 会等锁超时（既有性质）**：`sqlite`（`max_connections(1)`，
  见 `accessor_sqlx.rs`）上，`db.nextSeq` **首次**在 `db.tx` 内使用会挂住——因为 `ensure_seq_once`
  的 DDL 必须走池，而池的唯一连接被调用方事务持有。PG/MySQL（池 ≥2）无此问题。这是 1 连接
  accessor 的通用性质（任何「事务内还去池上取连接」的路径都一样），非 `nextSeq` 引入；
  登记备查，未修。
- **真库验收不在 CI（架构评审 P1-3，本轮只登记）**：本批的跨方言正确性证据全部来自
  env-gated 用例，而 env 未设时**静默 pass**、CI 无 service 容器也不设 `OJ_TEST_*` → 门禁恒绿。
  建议后续加一个起 PG+MySQL service 容器的 job，并引入 `OJ_TEST_REQUIRE=1`（该模式下缺 env 即失败，
  防「专用 job 里变量名写错→依然绿」）。**开发侧评审补充的具体缺口**：`next_seq_first_use_inside_tx_on_real_db`
  是「事务内首次取号不毒化事务」（P1-2 修复）的**唯一**守卫，而 sqlite 没有 aborted-tx 语义、
  假实现更没有 → 该修复在 CI 上实际**无覆盖**。

**测试面变更**

- 核心 crate 的 `[dev-dependencies]` 增 `sqlx` 的 `postgres` / `mysql` / `mysql-rsa` 特征
  （**仅测试构建**）：env-gated 真库用例需要具体驱动（生产路径的 PG/MySQL 仍由 `oj-db-*` 插件承载）。
- MySQL 插件保留 Any+sqlite 的离线全路径用例（CI 无 MySQL 时仍有覆盖），新增 typed 路径的
  env-gated 用例。

**修复（文档）**

- `docs/devkit/scenarios.md` / `docs/devkit/api-manual.md`：匿名路径迁移 WARN 的**对外文案与
  代码对齐**（`3060da5`，v0.1.23 发布后复校发现，故按「不改写已发布标签」的约定进本节）：
  ① 「常见坑」里 WARN 触发条件补上「只可能来自 `auth.anonymous_paths`」与「确实丢面」；
  ② 引用的装配期报错文案订正为 `标了 one_layer，但末段不是 "*"`（原文案已过时）；
  ③ 新增「对象形态键名拼错（`one_layr`）会报错并指到出错条目的行号与下标」一行；
  ④ 场景索引注明 v0.1.20 收紧**仅 auth 侧**；⑤ OIDC 走读段补全为「两条列表各三条路径 +
  auth 的 `/idp/*` 标 `one_layer`」。纯文档改动、无代码变更；devkit 发布产物 `bin/devkit`
  已随之同步（`cargo xtask build`）。
- **对外文档 `docs/devkit/` 按本版用户可见变更逐条收口**（`218b8a9`）——按「devkit 随版本升级
  同步」的要求，逐条列出本版用户可见变更后核对四件，补齐如下**真漏项**（①–③ 为
  `api-manual.md` 新增章节/条目，④ 为 `SKILL.md` / `scenarios.md` 补齐，⑤ 为 `README.md`
  索引与流程）：
  ① `api-manual.md` §8 新增 **`tenant_id` 列类型**（白名单 `text|integer|bigint` 与
  server / `oj build` / `oj migrate` 三处 fail-fast、按列类型绑值、insert/update 四种形态），
  并点明**数值列上租户头非十进制时 `sql_guard: warn` 也硬失败**；
  ② `api-manual.md` §6 新增 **`toSQL().params` 的超界整数是 marker**（`$oj$i64`/`$oj$u64`）——
  此前被降成字符串，用户照文档「回放 params」必然失败；
  ③ `api-manual.md` §12 新增 **「PostgreSQL 语句缓存（DBA 视角）」**——`/*oj:<形态>*/` 前缀在
  `pg_stat_activity` / PG 日志 / `EXPLAIN` 里可见（勿误判为被篡改）、`toSQL()` 不含该前缀、
  `statement-cache-capacity=512` 的由来、MySQL/SQLite 为何不打签名；
  ④ MySQL 读侧列类型边界（`DECIMAL`/JSON/时间/`BIT`/`GEOMETRY` 报错而非静默 `null`、
  `BOOLEAN` 读出 `1`/`0`）**补齐到 `SKILL.md` 陷阱速查与 `scenarios.md` 场景 7**（此前只在
  `api-manual.md`）；⑤ `README.md` 的 api-manual 索引行补本版亮点并新增「版本同步要求」小节。
  **同时把该要求固化为流程**（否则下次升级仍会漏）：`CHANGELOG.md` 顶部新增「版本分界提交的
  固定动作」四条、`docs/devkit/README.md` 新增「版本同步要求」。`bin/devkit/` 已归置，
  与 `docs/devkit/` 四件逐一 diff 一致（`cargo xtask build`；xtask devkit 契约用例 7/7 绿）。
  纯文档改动、无代码与行为变更。
- **变更日志文件由 `CHANGELIST.md` 更名为 `CHANGELOG.md`**：统一回仓库**原本**的命名——`docs/plans/`
  与 `docs/superpowers/` 下的任务书与设计稿一直写的就是 `CHANGELOG.md`（如 mail 计划的
  「Modify: … `CHANGELOG.md`」、tenant-sql-guard 计划的「Modify: `CHANGELOG.md`（Unreleased
  条目）」、ws-frame-pool 计划的「Modify: `CHANGELOG.md`（v0.1.10 段）」），两套名字并存属历史漂移。
  本次把文件本身（标题 `# CHANGELOG`）与仓库内全部引用（含 `src/`·`oj/`·`plugins/`·`tools/`
  的代码注释）一并改名——除本条目对旧名的记述外，`git grep CHANGELIST` 已无残留。
  **注意**：外部若按文件名引用（文档链接、书签、脚本）需同步改名；文件内容、分组惯例与
  「`oj/Cargo.toml` 版本递增提交即版本分界」的约定**均不变**。纯改名，无代码与行为变更。

## v0.1.23（2026-09-19）

下游 U39：`anonymous_paths` 的 v0.1.20 迁移 WARN **无法消音**。尾 `/*` 是四形态中的合法形态
（尾段动态时只能用 `/*`；改 `**` 反而扩面——`**` 含零层与任意深），但旧实现「凡尾 `/*` 即
告警」，这类条目每次启动都被点名，且没有任何配置手段让它退出聚合。本版三处改动合起来治：
**订正提示的适用面**（只 auth）+ **按真实影响面判定**（默认静音噪音）+ **条目级显式确认**
（保留人工逃生门）。设计、证据与双专家评审逐条处置见
`docs/superpowers/specs/2026-09-19-v0.1.23-anon-path-warn-design.md`。

**订正（v0.1.20 兼容性条目的措辞）**

- **v0.1.20 的「尾 `/*` 任意深度 → 严格一层」收紧只发生在 oj-auth 侧**。`tenant.anonymous_paths`
  自引入起就是严格一层：`git show v0.1.19:server/src/lib.rs` 的 `path_matches` 即
  `!rest[1..].contains('/')`，且 v0.1.19 的租户豁免（同文件 `:364`）走的就是它。v0.1.20 那条
  兼容性说明把两条列表并列，读起来像租户也收紧过——本版在原文加了订正注，并且**迁移 WARN
  只再对 `auth.anonymous_paths` 生效**（对租户条目说「收回了面」是伪前提，也解释了为何
  下游实测里那几条 tenant 告警怎么调都"消不掉"）。

**行为变更（启动日志，需知晓）**

- **迁移 WARN 只针对 `auth.anonymous_paths` + 按影响面判定**，两个条件**同时成立**才告警
  （`oj/src/app.rs` 的 `warn_legacy_tail_wildcards` / `migration_warn_entries`）：
  1. 条目是**旧式前缀形态**——只有尾段那一个 `*`、其余段全字面。旧 oj-auth 实现是
     `strip_suffix("/*")` + `starts_with`（尾 `*` = 任意深度），只有这种形态当时能命中真实
     请求；**含中段 `*` 的结构条目**（`/public/anchor/*/issues/*`）那时根本匹配不上，只能
     诞生于 v0.1.20 的四形态语义，即刻意写出的形状——对它们提「改 `**`」是错的。
  2. 该条目**确实丢了面**——存在一条已注册路由比严格一层更深（judged 段级：`tail_entry_loses_coverage`
     把路由 pattern 归一去 base 的视图后，逐段对拍「head + ≥2 段」是否可达；`**` 只多出的
     **零层**（裸 head）在旧语义下同样没覆盖，不算丢面）。
  - 实测校准（上一轮用本版前的二进制跑下游真实 config 的启动日志）：旧实现点名
    **9 条 tenant + 6 条 auth**；加形态条件后收敛为 **4 条 tenant + 1 条 auth**（U39 抱怨的
    结构条目全部静默）；再加「只 auth 参与」后 **tenant 侧归零**。下游套件本轮未复跑
    （该仓只读），auth 侧余下条目属真·旧前缀（`/public/assets/v2/anchor/*` 一类），
    需要时在那边跑一次 `oj test` 即可看到当前清单。
  - 调用点从装配早期（原 `:520`）**后移到路由表建好之后**，故 WARN 在启动日志里位置变晚
    （在 `module ...` 行与路由清单之后）。
  - 只按「已注册路由」判是充分的：静态托管与 `/blob` 都在鉴权前直接返回，不经匿名匹配
    （`server/src/lib.rs`，该处已补**互指注释**：改动提前返回的顺序/让它们咨询匿名表时
    必须同步本判定）。

**特性**

- **`anonymous_paths` 支持条目对象形态**（tenant / auth 同形）：除字符串简写外可写
  `- { path: "/auth/oidc/*", one_layer: true }`。`one_layer: true` = 显式声明「这一层是有意的」，
  **永久退出迁移 WARN**（含影响面判定为真的情形；对 tenant 列表是备注——那里不告警）。
  - **手写 `Deserialize`（不用 `#[serde(untagged)]`）**：untagged 无法 `deny_unknown_fields`，
    `one_layr: true` 会被**静默**解析成 `one_layer=false`（用户以为已消音而实际没有），
    报错还只给「did not match any variant」且位置指向**列表首元素**。现在只认 `path` 与
    `one_layer` 两个键：未知键 / 缺 `path` / 类型错都给出人话报错。
  - 消费侧统一归一为路径字符串（`config::anon_paths()`）：server 的 `Pipeline.tenant_anon` 与
    oj-auth 插件 cfg JSON 都只吃 `Vec<String>`，**插件侧零改动、ABI 保持 8**。
  - **装配期 fail-fast**（`config::validate_anon_paths()`）：`one_layer` 标在末段不是 `*` 的
    条目上直接报错。判据用**段级**口径（与匹配层「忽略空段」一致：`/idp/*/` 合法），
    与迁移 WARN 侧的裸后缀口径**有意不同**（那边对齐的是 v0.1.19 `strip_suffix("/*")` 的历史语义）。

**修复**

- **`oj build` 读配置失败改为出声**（`oj/src/build_cmd.rs` 的 `sql_guard_of_config` /
  `tasks_dir_of`）：此前静默回落 `sql_guard=off` + `tasks` 目录——`sql_guard` 非 Off 的项目
  会因此**静默关掉** `S*` 检查里的 tenant 声明校验（CI 门禁失效）。对象形态新增了一类
  「很容易写错、写错就是整份配置解析失败」的输入，触发面被放大，故本次补上 warn。
- **影响面判据由「模式对拍模式」改为段级判定**：原实现把条目与路由视图都送进
  `server::path_matches`，而路由视图自身可能含 `*`/`**`（`{param}` / `{*rest}` 归一而来），
  pattern-vs-pattern 会**漏报**——`/file/*` 撞上 catch-all 路由 `/file/{*path}`（视图
  `/file/**`）时静默，`/zzz/*` 撞上带参路由 `/v1/api/{x}/detail/sub` 时也静默，而两者都
  真的丢了面。

**文档**

- `docs/superpowers/specs/2026-09-19-v0.1.23-anon-path-warn-design.md`（新增）：本版设计与
  **双专家评审逐条处置**（开发侧 + 架构侧并行只读评审；已采纳 11 条 / 未采纳 4 条附理由），
  含实读证据（v0.1.19 两侧旧实现源码）与实跑验证（样例静音、未知键报错位置、fail-fast）。
- `docs/devkit/`（对外发行）：`api-manual.md` 匿名路径段改写（适用面＝仅 auth、影响面判定、
  `one_layer` 形态、对象形态只认两个键）；`scenarios.md` 场景 5 ② 改标题与正文为「只需检查
  `auth.anonymous_paths`」并补两个反例（仅差零层不告警、结构条目不告警）；`SKILL.md` 陷阱行同步。
- `docs/modules/02-config.md`：§4 校验表补 `oj build` 出声一条 + 调用点改为按函数名引用
  （原先写死的行号已漂移）；§4.1 改写为「WARN 只 auth + 两个条件 + 手写 Deserialize」。
  `docs/modules/03-server-http.md` 的通配语义段同步（收紧只在 auth、WARN 两条件、静态/blob
  不经匿名匹配）。
- `docs/tenant-guide.md`：v0.1.20 变更块限定到 auth 侧，明确租户列表不参与告警。
- `docs/oidc-integration.md` / `docs/oidc-implementation.md`：收紧范围限定 auth；补「本文
  `/idp/**` 与 sample 的 `/idp/* + one_layer` 是等价两种窄面」的互指；§8 清单措辞对齐。
- `sample/config.yaml` / `sample/config.docker.yaml`：`one_layer` 挪到 **auth** 的 `/idp/*`
  （租户侧不再需要），注释写清「租户侧自始严格一层、不参与告警」；`sample/README.md` 同步。

**测试**

- `src/config.rs`：两形态解析、归一化、段级 fail-fast、**未知键/缺 `path`/类型错报错**
  （评审 P2-3/P2-4 的钉子），共 4 条新用例。
- `oj/src/app.rs`：`anon_view_of_route`（剥 base、参数段归一）/ `is_legacy_prefix_shape` /
  `tail_entry_loses_coverage`（含评审反例：catch-all 视图 `**`、带参视图 `/*/detail/sub`
  必须告警；仅差零层的模块根路由**不**告警）/ `legacy_entries`（直接打被测函数，不复刻过滤链）/
  `migration_warn_entries`（只 auth 参与），共 5 条新用例。
- `oj/tests/oidc_e2e.rs`：匿名列表用对象形态，端到端覆盖条目解析 → 装配 → 插件 cfg 链路。
- `oj/tests/sample_config.rs`（新增）：**发行样例**必须可解析且过 `validate_anon_paths`，
  并钉住「auth 的 `/idp/*` 带 `one_layer`、tenant 的 `/idp/*` 不带」——样例 YAML 写错的
  暴露面只在装配期，没有本用例只能靠人肉跑 server 才发现。

## v0.1.22（2026-09-19）

修掉「DB 的 64 位整数值跨 JS 边界」的三重缺陷：**读侧必然 500**、**写侧静默精度丢失（假碰撞）**、
**PG 上字符串写法完全不通**。下游 U38（生产 PG 实测：`Number(max(id))+1` 坍缩 → 主键 dup 500）
即此链条。设计与证据手册见新增的 `docs/numeric-limits.md`。

**行为变更（读侧，需复查存量代码）**

- **i64 超出 JS 安全整数范围（`|v| > 2^53-1`，雪花 id 常态）时，读值由 v8 `BigInt` 改为
  十进制字符串**。旧行为下任何含此类值的 `json.ok(rows)` 都直接 500
  （`TypeError: Do not know how to serialize a BigInt`），故**无兼容性风险**（无人能依赖一个
  必然失败的行为）；但改过 `typeof id === "number"` 判断的代码需复查。
  阈值与 serde_v8 的 `MAX_SAFE_INTEGER` **逐字一致**（`(1<<53)-1`：写在 `2^53` 上会漏转 `2^53` 本身）。
  安全范围内的整数、REAL/DOUBLE 的 f64、TEXT 一律不变。`Json`/`Row` 类型无需改动（本就含 string）。

**特性**

- **新增两个 JS 全局：`toBigInt(v)` / `toDouble(v)`**（`bootstrap.js`；三个 JsRuntime 入口
  共用 `bridge_ext`，故 HTTP 池 / 任务池 / `oj test` 都可用）。**不做 `toFloat`**——JS 的
  `Number` 就是 f64，没有独立的 f32 语义。
  - `toBigInt`：十进制串 / 安全范围内的 number / bigint → `bigint`，**fail-loud**：已坍缩的
    number（`Number("<大整数串>")` 的产物，任何 `|v| > 2^53-1` 的 f64 都不是 safe integer）、
    `1.5`、`"007"`、`"+1"`、超 i64 等一律抛错并给出指引。这是 U38 陷阱在**调用点**的捕手。
  - `toDouble`：number / 数字串（含科学计数法）/ bigint → `number`，**显式接受精度丢失**。
- **写侧大整数通道**：`toBigInt()` 的返回值是真 `BigInt`（U38 范式需要 `+1n` 精确算术），
  跨 op 时由 bootstrap 的参数包装层递归编码为保留形状 `{"$oj$i64":"<十进制>"}`
  （serde_v8 的 `ValueType::BigInt` 直接 `UnsupportedType`，且其 magic/transl8 trait 是
  `pub(crate)`，外部无法自定义），宿主/插件在绑定参数时解码为 `i64`。
  解码 `marker_i64` 落在共享契约 crate `oj-plugin-ffi::jsint`（**不 bump ABI**），四处复用：
  sqlite accessor / oj-db-postgres / oj-db-mysql 的 `bind_value` 与构造器的 `to_qv`
  （必须在 `other => to_string()` 之前，否则标记被串化成文本）。严格识别（单键对象 + 规范
  十进制 + i64 内），畸形标记按普通值处理。
  - **实测依据**：PG 拒绝字符串参数写 bigint 列（`column "id" is of type bigint but expression
    is of type text`）与 `where bigint = text`；`i64` 参数则精确往返。故「字符串=文本意图、
    BigInt=整数意图」，平台不做启发式（避免误伤 TEXT 列里的长数字串）。
- **bigint 容忍面**：`json.ok` / `json.raw` / `json.fail` 的 data / `log` 结构化字段 /
  `mail.*` / 构造器 `toJSON()`/`fromJSON()`/子查询快照容器 —— 统一走新的 `ojStringify`
  （`JSON.stringify` + bigint replacer，输出十进制字符串）。其中 `json.fail` 的 data 由
  `#[serde] Value` 改为 JS 侧预序列化 + `#[string]`（原形态收到 BigInt 会 500）。
  `es.search` 响应同样过一遍读侧护栏（ES 的 `long` 字段同病）。
  快照类 API（`toJSON`/子查询嵌入）另有 `encodeSnapshot`（JSON 往返 + bigint 标记），
  以保留 `Date → ISO 串` 等原 JSON 语义——参数编码器 `encodeParams` 对带 `toJSON` 的对象
  同样遵循 JSON 语义（否则 `Date` 会被压成 `{}`）。二进制参数（`Uint8Array`/`ArrayBuffer`）
  显式报 `TypeError`（改动前为 serde_v8 类型错，行为一致、消息更清晰）。
- **读侧护栏补齐两处出口**（评审发现）：`jwt.verify` 的 **claims 外部可控**（雪花量级的数值
  声明会让 `json.ok(claims)` 500）与 `mail.result` 的结果体。`jsnum.rs` 模块注释给出
  **op 出口覆盖清单**（已覆盖 6 处 + 有意不覆盖 4 类及理由），供后续新增 op 时对账。
- **租户守卫接受三种等值形态的租户 id**：字符串 / 数字 / 大整数标记
  （`param_is_tenant`）。此前只认字符串，雪花租户 id 用 `toBigInt()` 传会被
  `sql_guard: deny` 误拒（`params must include current tenant id`）。`mentions` 要求不变。

**修复**

- **env-gated 真库用例不可重跑**（既有）：`oj-db-postgres` / `oj-db-mysql` 的
  `real_*_roundtrip_via_vtable` 只 `create table if not exists` + 插入固定 id，上一次运行
  的残留行会让**重跑撞主键**（本机第二次跑 PG 用例时暴露）。两处补 `drop table if exists`
  清场，用例恢复幂等（PG 已实测连续两次绿）。
- **`value_to_json` 的 u64 回绕**：`Qv::BigUnsigned(Some(u)) => Value::from(u as i64)`
  在 `u > i64::MAX` 时静默变成负数 → 改为 u64 直出（超界部分由读侧护栏降为十进制字符串）。
- **Windows 门禁编译失败：v0.1.20 新用例误用 unix-only 辅助函数**（本次 CI 暴露）：`oj/tests/e2e.rs`
  的 `test_db_override_builds_schema_on_test_db_not_dev` 及其 `has_table` 助手三处直接调用
  `#[cfg(unix)] fn fwd`（该 cfg 正是 v0.1.19 为消 Windows dead_code 告警加的），Windows 上退化为
  「cannot find function `fwd`」→ `oj` e2e 目标编译失败。三处 DSN 一律改走
  `oj_plugin_ffi::path_util::sqlite_file_dsn`（与 `fwd` 自带文档的红线一致：DSN 不走裸 replace）。
  复核方法：把全仓 `#[cfg(...)]` 里的 `unix` 逐处置换成 `windows` 后 `cargo check --workspace
  --all-targets`——等价于「unix-only 项全部缺席」的 Windows 形态，结果零 error（`fwd` 是唯一一处，
  `logging.rs` 的 `cfg(all(test, unix))` / `serve_cmd.rs` 的 `daemonize`·`term` 均有 `not(unix)`
  对偶分支）。
- **CI：三平台矩阵「一红全跑」的资源成本**（`.github/workflows/plugin-matrix.yml` / `release.yml`）。
  `plugin-matrix.yml` 新增派发输入 `platforms`（`all`/`linux`/`macos`/`windows`，逗号分隔）与
  `shared_jobs`（`false` = 再跳过 fmt+clippy、sample L1/L2 两个平台无关 job），由新增的 `select`
  job 生成动态 `matrix.include`——修 Windows 就只生 Windows 那条腿，另两条**根本不生成作业**；
  另配 workflow 级 concurrency（group = `ref` + 平台选择，`cancel-in-progress`）取消被取代的旧矩阵。
  两个 workflow 均加 `actions/cache`（只缓 crate 源码/索引与 rusty_v8 预编译库；**不**缓存
  `target/`——本仓 release target 实测 12G+，三平台相加超 GitHub 每仓 10G 上限会互相 LRU 挤出）
  与各 job `timeout-minutes`（挂死不再默认烧满 6h）。`release.yml` 保持三平台全跑（发行物必须齐全），
  单平台出错靠 `fail-fast: false` + 缓存支持「Re-run failed jobs」只重跑那一条腿。
- devkit 顺带订正：`oj test` 旗标表补 `--db` / `--anonymous`（v0.1.20 漏同步）、
  已知限制表「静态站点无 SPA 回退」改为「SPA 回落需显式开 `server.app_spa_fallback`」。

**文档**

- 新增 **`docs/numeric-limits.md`**（专题手册：契约与阈值表、`toBigInt`/`toDouble` 接受-拒绝
  矩阵、绑定类型规则、U38 范式与真实数字、代价与边界、**存量代码自查清单（含 grep）**、
  排障表、**双专家评审意见与逐条处置**）。
- devkit（发行交付物）：`api-manual.md` §6 新增「大整数与 i64」小节（含保留键红线）+ 总表行、
  §13 限制表与陷阱清单；`SKILL.md` **红线新增「大整数」一条**（U38 正是 agent 易犯的写法）、
  陷阱速查 4 行、场景清单；`scenarios.md` **新增场景 7「雪花 id（大整数）的生成与回写」**；
  `README.md` 场景清单与手册特性描述。`db-guide.md` §11 红线第 6 条 + 报错速查 5 行。
- `sample/global.d.ts` 补 `toBigInt` / `toDouble` 声明（含 fail-loud 语义注释）。

**已知债 / 另案登记**

- **PG 语句缓存与混合参数类型（既有隐患，非本次引入）**：同一条 SQL 文本若在不同调用里绑定
  不同 Rust 类型的参数（字符串 ↔ 数字/大整数），sqlx prepared statement 缓存会给出协议级错误
  （`invalid byte sequence for encoding "UTF8": 0x00` / `insufficient data left in message` /
  `incorrect binary data format in bind parameter`），且与执行顺序相关。**v0.1.21 用纯数字+字符串
  即可复现**，与本次改动无关；已记入 `docs/numeric-limits.md` §4.5 与 devkit 陷阱表待专项处理。
- `u64` / `BIGINT UNSIGNED` 全链路未支持（读侧不再回绕为负数，但精确性不保证）。
- **数值型租户 id 列不受支持（既有）**：守卫注入的是字符串条件、insert 要求 `tenant_id` 为
  等于租户头的字符串，故 `tenant_id` 列为 BIGINT/INTEGER 时 PG 报
  `operator does not exist: bigint = text`（详见 `docs/numeric-limits.md` §4.8）。绕行：租户 id 用 TEXT。
- 内建序列分配原语（`max+1` 的并发竞态终态）未做。
- MySQL 侧真库验证未执行（本机拉不到镜像，用例已 env-gated 写好，`OJ_TEST_MYSQL` 可用时即跑）；
  PostgreSQL 侧已用真实 PG 18 跑通（含 `i64::MIN` 往返、`where` 标记比较、字符串负例、事务路径）。

## v0.1.21（2026-09-19）

**特性**

- **`oj migrate` / `oj fixture` / `oj schema diff` 新增 `--db <profile>`（多库运维闭环）**：
  三处瘦身装配（`migrate_cmd.rs::slim`）原先硬编码 config `db:` 段的 `default` 键，
  多库项目里非 default 的命名库**无法**走工具链——只能手工执行 SQL，账本
  `_oj_migrations` 与 `schema.yaml` 收敛都缺位。
  - `--db <name>` 即 config `db:` 段的键（`db: {default: …, analytics: …}` →
    `--db analytics`）；缺省仍为 `default`，**选库语义与 v0.1.20 一致**（`default` 缺失时
    的提示文案与收尾行措辞有更新，见下）。
  - **未声明的库名 fail-fast**，报错列出可用键（`--db "x" not declared in config
    (db keys: [...])`），绝不静默回落 default——与 `oj test --db`（v0.1.20）和
    `App::from_config` 的 `db_override` 同一条纪律：静默回落等于把迁移打在开发库上。
  - 语义不变：`--db` 只是换目标连接；账本、reconcile、`--baseline`、`--module`
    均**各库独立**。迁移工具**不读**模块级 `manifest.yaml` 的 `db:` 绑定（那是运行期
    路由，`src/bridge/guard.rs::bound_db`），故 `--db` 是**整轮**迁移的目标库而非逐模块
    解析——单一 profile 的项目逐库各跑一遍即可；模块绑定不同库的项目见下条「已知债」。
  - `oj migrate` 收尾行打印目标库（`… → db "analytics"`），避免多库下跑错库无察觉；
    `--db` 缺省时提示文案改为「迁移/fixtures/对账需要一个目标库」并列出已声明键
    （原文案写死 "（迁移/fixtures 作用于 default 库）"）。
  - **已知债（本次只登记不修）**：`--db` 是**整轮**目标库，`slim` 不解析模块级
    `manifest.yaml` 的 `db:` 绑定——多库项目须 `--db <profile> --module <M>` 逐组合跑，
    否则会把全部模块的迁移灌进同一库（静默重复，D002 不报）。登记于
    `docs/migration.md` §8 与 `docs/modules/04-oj-cli.md` §8。
  - 文档：`migration.md` 新增 §3.8「多库：`--db` 指定目标 profile」、改写 §6 限制 7
    （原「只作用 default 库」），`db-guide.md`（§1.1 库级迁移）、`cli2.md` /
    `user-manual.md` 旗标与用法、`ops-manual.md` 发布流程与排障表（新增
    `--db "x" not declared` 一行）同步。**devkit（发行交付物）**：`api-manual.md`
    命令表 / 交付跑法 / 打包部署步骤 / §10 db 段 / 排障表 / 已知限制全表，
    `SKILL.md` 发布检查与陷阱速查（`--db` 三条），`scenarios.md` **新增场景 6
    「多库项目按库迁移与对账」**（含整轮语义的踩坑），`README.md` 场景清单同步；
    顺带订正 devkit 两处陈腐描述：`oj test` 旗标表补 `--db` / `--anonymous`（v0.1.20 漏同步）、
    已知限制表「静态站点无 SPA 回退」改为「SPA 回落需显式开 `server.app_spa_fallback`」。

## v0.1.20（2026-09-19）

本版来自下游（plane）上游需求清单（U1–U37）的逐条回源码复核，交付四项：**匿名访问双层**
（路径通配统一 + `db.asTenant`）、**静态托管两条**（SPA 回落 + 每路由 meta 注入）、
**`oj test` 测试库隔离**、**构造器 LIMIT 可配与截断可观测**。设计文档（含 20 条专家评审处置）
见 `docs/superpowers/specs/2026-09-18-v0.1.20-upstream-support-design.md`。

**特性**

- **匿名访问：路径通配四形态统一 + `db.asTenant`（下游 U2，P0 阻塞点）**。下游痛点是双层阻塞：
  豁免路径匹配能力不足（深层 OIDC 路径进不来），且即便豁免进得来，handler 查租户表仍被
  `sql_guard` 拦（`tenant_id=None`）。
  - **通配四形态**（`server::path_matches` 与 `plugins/oj-auth` 的 `is_anonymous` **同语义**，
    两处实现互指注释 + 各自矩阵测试）：字面全等 / 尾 `/*` **严格一层** / 中段 `*` **恰好一段** /
    `**` **跨任意层**（含零层，`/idp/**` 命中 `/idp`）。段切分后回溯匹配，`%2e%2e`/`..` 段
    照旧在解析前被拒。
  - **`RequestInfo.anonymous`（新字段）**：**仅** server 的「命中 `tenant.anonymous_paths`
    且确实没带租户头」分支置 true。`tenant_id.is_none()` 有五个来源（租户未启用 / 豁免命中 /
    WS 帧 / 任务桥 / `oj test`），不能当匿名判据——用它会把 WS 与任务路径一并放行。
  - **`db.asTenant(id)`（新 op + JS 面，`db` / `DB(name)` / `tx` 内实例同面）**：匿名请求
    **声明**租户身份。与 `db.asSystem()` 相反——**防护机制不变**（构造器仍强制注入 `tenant_id`、
    裸 SQL 仍要求 mentions+param_has），只是身份由 handler 给出。三道 **fail-closed** 门禁：
    ① `tenant.allow_as_tenant: true`（新键，**默认 false**）；② 请求为匿名；③ `id` 非空。
    不满足即**抛错**（`nofast` op，拒绝原因必须回到 JS）。**请求级且只能设一次**（已带租户头 /
    二次调用 → 抛错，防运行期切换身份），调用打审计日志。
  - 定位是**授信 handler**：平台无从校验 id 是否为真实租户，红线是 id 必须服务端派生
    （token → 查表 → 租户），绝不可直接取 URL 参数（见 `scenarios.md` 场景 1 / §8 风险）。
- **静态托管：SPA 深链回落 + 每路由 HTML meta（下游 U1）**。原静态兜底「未命中即 404」，
  SPA 深链接刷新不可用；也没有任何 per-route 模板能力。
  - `server.app_spa_fallback`（**默认 false**）：未命中 + 无扩展名 + `Accept` 含 `text/html`
    或缺失或 `*/*`（curl 默认）+ **不在 `api_prefix` 下** → 回落 `index.html`。排除 API 前缀是
    硬约束：`app_prefix="/"` 时否则会把拼错的 API 路径吞成 200，掩盖真实 404。
  - `server.html_meta`（目录名，**默认关闭**）：送出 HTML 时按请求路径读
    `<app_path>/<html_meta>/<path>.json`（`/` 与目录 → `index.json`），把白名单键注入
    `</head>` 前：`title` / `description` / `canonical` / `og:*`（`property`）/ `twitter:*`
    （`name`）。**只注入标签、绝不注入脚本**（静态响应拿不到 CSP nonce），值一律 HTML 转义；
    未命中 / 无 `</head>` / JSON 非法 → 原样返回（零副作用）。meta 目录自身**不对外公开**
    （`resolve_static` 对该目录返回 None）。不做 SSR——产物由构建期离线生成。
- **`oj test` 测试库隔离（下游 U33）**：原 `oj test` 复用的 `db.default` 就是开发库，下游实测
  读到 1000 行非种子数据、甚至写坏开发库。
  - `App::from_config` 收 `db_override` 并在**装配期**算出 `db_key`，`migrate.apply_all/verify_all`、
    `build_schema_and_modules`（schema 内省）、`seed.replay_all`、`load_fixtures` **四处全部跟随**
    ——只改运行期 `bound_db` 会让 test 库无表无种子，比现状更糟。
  - `oj test` 默认策略：config 声明了 `db.test` ⇒ 用它；否则醒目 WARN 后继续 `default`（不静默）。
    新增 `--db <name>`（未在 `db:` 段声明即 fail-fast）与 `--anonymous`（以匿名请求身份跑，
    便于测 `anonymous_paths` 覆盖的公开面）。启动打印 `oj test: using db "..."`。
  - 运行期 `bound_db` 解析顺序：**显式 `DB("name")` → manifest `db:` 绑定 → `db_override` →
    `"default"`**（`StableState.db_override` 经 `Extras` 注入）。
- **构造器 LIMIT 可配 + 截断可观测（下游 U30）**：顶层 select 原隐式 `LIMIT 100`、显式 limit
  被 clamp 到 1000，**截断完全无信号**（下游「>100 条静默少数据」）。
  - 新配置段 `db_query: { default_limit, max_limit }`（默认 100 / 1000，硬顶 100000）。
    装配期校验 `1 ≤ default_limit ≤ max_limit ≤ 100000`，`0` 与倒置区间 fail-fast；
    结构体 `deny_unknown_fields`——键名拼错（如 `default`）**启动即报错**，不静默取默认值。
    **注意不能写进 `db:` 段**（`db` 是 name→DSN 的 map，键即库名）。
  - 归一化在 **op 层**（`normalize_limit`，不穿透 `build_statement` 递归签名）：顶层
    `limit=None` → `default_limit`；显式 → `min(limit, max_limit)`。保留「嵌套/子查询
    不隐式截断」语义。`toSQL()` 走同款归一化（诊断与执行口径一致）。
  - 截断信号：`applied = min(显式或默认, max_limit)`，结果行数 ≥ `applied` ⇒ 响应带
    `X-OJ-Row-Limit: <applied>` 头（含「显式 limit 被 clamp」这类旧版静默情形）。
    **信封形状不变**（`{code,msg,data}` 契约不动，下游 12 个生成 client 零改动）。
- `--db` / `--anonymous` 同时补入 `oj test --help` 与 `docs/testing.md` 旗标表。

**修复**

- **oj-auth 与 server 的匿名通配语义分叉**：`plugins/oj-auth` 的 `is_anonymous` 原为
  `starts_with` 前缀匹配（尾 `*` = 任意深度），与其自身注释及 `docs/builtin-api-auth.md` 的
  「一层通配」矛盾，也让「同一份 `anonymous_paths` 在租户与鉴权两道守卫下表现不同」。
  统一为四形态（与会话无关的纯段匹配），两处实现注释互指，各补通配矩阵测试。
- **`sample/package.json` 的 `test:api` 路径写错**（既有缺陷，本次接线 `npm test` 时暴露）：
  npm 以**包目录**为 CWD 执行脚本，脚本里的 `./bin/oj` 与 `-c config.yaml -d src` 却按
  「CWD = 仓库根」写 → `npm run test:api` 在最后一步必失败
  （`sh: ./bin/oj: No such file or directory`）。改为 `cd .. && … ./bin/oj test -c
  sample/config.yaml -d sample/src`（与 CI 命令同形）。此前未暴露是因为 CI 直接在仓库根
  调 `./bin/oj`，本地统一入口少被走。
- **`oj test` 运行时装配缺口**：`StableState` 新增字段在 `with_dbs_and_loader` / 测试夹具 /
  `oj/src/app.rs`（`oj test` 直建 runtime）三处同步，避免 server 与 `oj test` 行为分叉；
  `oj test` 的 `op_client_dispatch` 显式构造 `RequestInfo { anonymous, ..Default::default() }`。
- **类型面（`.d.ts`）与运行时/样例对不齐**：用 `tsc -p sample/tsconfig.json` 实测，样例在
  「本手册宣称 `global.d.ts` 是类型权威」的前提下有 **30 处报错**（此前无类型门禁，属静默腐烂）。
  - `QueryBuilder.all()` 声明为 `Promise<Json[]>`，而运行时返回的是**行**（列名 → 值）——
    于是 `rows[0].password_hash`、`{...row}` 全部报错（20 处，散在 auth/idp/oidc/cert/order）。
    改为 `Promise<Row[]>`（与 `db.query` 同形）；`db.tx` 的回调参数改为 `TxInstance`
    （`Omit<DBInstance, "tx">`——嵌套事务被运行时拒绝，原声明却让 `tx.tx(...)` 通过类型检查）。
  - **`sess` 未声明**（`Cannot find name 'sess'`，2 处）：WS 帧池的会话上下文
    （`sess.id` / `sess.state`，由 `ws_connect` driver 注入）此前只在 API 手册里，类型面缺失。
    新增 `WsSess` 接口 + `declare global { const sess }`，并写明 `state` **必须可 JSON 序列化**
    （不可序列化时该帧状态回传被丢弃，连接不中断——对应 `frame_pool.rs` 的 `Option<Value>` 语义）。
  - `client.ws().next()` 的三态（帧 / `{closed: true}` / `null` 超时）原声明为
    `TestWsFrame | { closed: true } | null`，调用方取 `binary`/`data` 必然报 2339（10 处）。
    改为带可选判别位的 `TestWsFrame | TestWsClosed | null`（`closed?: false`），
    样例测试加 `expectFrame()` 收窄（顺带把「拿到非帧」从 TypeError 变成显式断言失败）。
  - 样例随类型收紧补 3 处 `String(...)`（`Row` 的列值是 `Json`，直接传 `string` 参数不合法）。
  - `sample/tsconfig.json` 的 `include` 补 `unit/**/*.ts`（L2 spec / mock 此前不在任何 tsconfig
    内，编辑器取不到 `json`/`db` 等全局类型），`exclude` 补 `unit/vitest.config.ts`
    （其 `node:fs`/`vite` 依赖需要 `@types/node`，本工程未安装）。
  - L2 mock（`sample/unit/mocks/oj-globals.ts`）补 `asSystem` / `asTenant`（与 bootstrap 同形，
    返回同一实例），否则写了 `db.asTenant(id).table(...)` 的 handler 在 L2 里 TypeError。
  - `docs/devkit/api-manual.md`：`.all()` 的返回标注由 `Promise<Json[]>` 改为 `Promise<Row[]>`
    （总表本已列出 `sess.id / sess.state`，类型面此前缺失）。
  - 验证：`tsc -p sample/tsconfig.json` **0 error**；`vitest run` 12/12 通过；
    `./bin/oj test -c sample/config.yaml -d sample/src` 43/43 通过。
  - **类型门禁进 CI**：`sample/unit` 加 `typescript` devDependency（钉 `5.6.3`，随
    `npm ci` 安装）与 `npm run typecheck`（= `tsc -p ../tsconfig.json`）；
    `sample/package.json` 加同名脚本（委托 unit）并纳入聚合入口
    （`npm test` = `typecheck` + `test:unit` + `test:api`）。CI 两处挂载：
    `plugin-matrix.yml` 的 `sample-tests`（安装/类型检查顶到昂贵的 Rust 构建之前，
    fail fast）与 `release.yml` 的 `lint`（tag 推送发版前必过，job 更名
    `fmt + clippy + typecheck`）。负向实测：注入一处类型错误 → 退出码 2、报错定位到文件；
    移除后退出码 0。

**行为变更（三项，升级前请核对）**

1. **`oj test` 默认库 `default` → `test`**：config 声明了 `db.test` 的项目，测试数据将从
   `default` 改落 `test`（建表/seed/fixtures 一并跟随）。理由：写坏开发库是真实事故。
   不需要此行为时显式 `--db default` 即可。
2. **匿名路径尾 `*` 由「任意深度」收紧为「严格一层」**：受影响的是依赖旧 oj-auth 深前缀行为
   的 `auth.anonymous_paths` / `tenant.anonymous_paths` 条目（如 `/idp/*` 曾命中
   `/idp/.well-known/openid-configuration`）。启动期对含尾 `/*` 的列表打**聚合 WARN** 提示
   改 `**`（不静默收回授权面）。仅需一层的老配置无需改动。
   > **v0.1.23 订正**：这次收紧**只发生在 oj-auth 侧**（`auth.anonymous_paths`）。
   > `tenant.anonymous_paths` 自引入起即为严格一层——v0.1.19 的 `server::path_matches` 已是
   > `!rest[1..].contains('/')`，租户豁免（`server/src/lib.rs:364`）走的就是它。本条的
   > 「两条列表并列」措辞不精确；v0.1.23 起迁移 WARN 也只对 `auth.anonymous_paths` 生效。
3. **SPA 回落默认关闭**：`server.app_spa_fallback` 默认 `false`，需显式开启（不复用旧行为，
   避免 `app_prefix="/"` 的存量部署突然开始吞 404）。

**兼容性**

- `ABI_VERSION` 保持 **8**（无 repr(C) vtable 形状变更；`db.asTenant` 走既有 db 轴 op）。
- 新增/改动配置键一律 `#[serde(default)]`：`tenant.allow_as_tenant`、`server.app_spa_fallback`、
  `server.html_meta`、`db_query.{default_limit,max_limit}` —— 老配置零改动（除上述三项行为变更）。
- 首方插件版本保持随发布统一（`0.1.0`，semver 门禁为可选 pin，无清单 pin 该值）；
  `oj-auth` 行为变更已写入 `CHANGELOG` + `docs/builtin-api-auth.md`。
- `cargo build --release` / `cargo fmt --check` / `cargo clippy --release --all-targets -- -D warnings`
  / `cargo test --release --workspace` 全绿。

**文档**

- `docs/devkit/scenarios.md`（**新增**，随发行包 `devkit/` 分发）：场景速查——公开分享页匿名读
  租户数据 / SPA 深链回落与每页 meta / 测试库隔离 / LIMIT 分页陷阱 / 匿名路径通配，
  每篇「配置 + 代码 + 验证 + 常见坑」。
- `docs/devkit/api-manual.md`：API 表补 `db.asTenant` 与三道门禁；LIMIT 段改 `db_query` 可配 +
  `X-OJ-Row-Limit`；tenant 配置示例补 `allow_as_tenant` 与通配四形态；首页加场景集指针。
  `docs/devkit/{README,SKILL}.md` 同步索引与陷阱速查；`tools/xtask` 的 devkit 发行契约测试
  增 `scenarios.md` 断言。
- 过期描述订正：`docs/modules/02-config.md`（新键）、`docs/modules/03-server-http.md`（静态
  9/10/11 步 + 通配表格）、`docs/db-guide.md`、`docs/dev-guide.md`、`docs/user-manual.md`、
  `docs/tenant-guide.md`（asTenant 场景 + 通配 + 豁免说明）、`docs/testing.md`（`--db` /
  `--anonymous` / 默认 test 库）、`docs/modules/00-overview.md`、`docs/builtin-api-auth.md`、
  `docs/oidc-integration.md`、`docs/oidc-implementation.md`、`sample/config.yaml`（新键注释）、
  `sample/global.d.ts`（`DBInstance.asTenant`）。

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
  - **白名单 fail-closed**：`allowed_from`/`allowed_recipients` **全等**匹配（大小写不敏感；
    条目 = 完整地址或 `@domain`，不做子域通配），**空表 = 拒绝**——白名单是「越权发送」的
    唯一控制点，缺省放行等于开放中继。宿主侧 CRLF 一律**剥离**（subject/headers）或**拒绝**
    （地址）；`headers` 不得覆盖 From/To/Cc/Bcc/Subject/Sender/Return-Path/Reply-To
    （防白名单绕过与弱 spoof）。校验失败一律 `{code:5}` 信封（不抛异常）；
    仅「未配置 mail」抛错。附件另有单件/单封上限（`smtp.max_attachment_bytes` /
    `max_total_attachment_bytes`，超限 `code:5`）。
  - **FileTransport**：`file_transport: <dir>` 给定时不发网络，`.eml` 落盘（测试/归档通道）。
    `oj-mail` 的引擎与 transport 都在插件内，宿主零新增依赖（只 `lettre::Address` 做地址校验）；
    `ABI_VERSION` 保持 8。
  - **CI/工具**：`tools/xtask` 的 `PLUGINS` 增 `"mail"`（CI 矩阵与归置的单一真相源，不硬编码
    副本）；`cargo xtask plugin mail --check` 可预检。JS 类型面见 `docs/devkit/api-manual.md`
    第 6 章 mail 小节。

**修复**
- **mail：enqueue 契约 / worker 强引用释放顺序 / 停机 drain 可达 / 忙等**
  - **`enqueue` 回统一信封**：插件引擎原回**裸** `{jobId}`，与三层公开契约
    （`global.d.ts`、`api-manual` §6、`mail-smtp.md` §6「`res.data.jobId`」）不符 ——
    用户按手册写 `res.data.jobId` 会 TypeError 且拿不到 jobId 去 `mail.result()` 回查。
    引擎改回 `{code:0,msg:"ok",data:{jobId}}`；宿主加**纵深防御**（插件返回顶层无 `code`
    时包成信封，容忍旧/第三方裸载荷）；新增**真插件** e2e（`oj/tests/mail_e2e.rs` 的
    enqueue 用例：统一信封 + 以同队列 `send` 作屏障后 `mail.result(jobId)` 必命中）。
  - **停机 graceful drain 生产可达**（Important）：`MailEngine::shutdown` 此前只被测试调用
    （`allow(dead_code)`，引擎是插件内 `OnceLock` 单例、进程退出不 Drop）⇒ 生产停机实际**丢在途
    邮件**，与文档宣称矛盾。改为**零 ABI 变更**的控制报文 `{"__ctl":"drain","timeout_ms":N}`
    （走既有 `MailVtable::submit`），宿主在停机路径（`serve_cmd` 的 SIGTERM/正常退出，
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
  - **文档↔实现漂移与死代码**（Minor）：
    - `src/bridge/bootstrap.js`（注释，7-bit ASCII）与 `sample/global.d.ts` 曾把 `code:2`
      （SMTP 5xx）/`code:3`（鉴权）写成可用；实际**保留未启用**（均归 `1`）→ 两处改为
      「RESERVED / 保留未启用」，与 `docs/mail-smtp.md` §3 一致。
    - `src/bridge/mail.rs` 的 `resolve_attachments` 删除死变量 `hint`（含 `let _ = &hint;`）。
    - 插件 `message.rs` 的 `SendRequest.subject` 注释原说「raw 路原文 `Subject:` 会被剥离」，
      与实现（**保留**，结构化非空时覆盖）矛盾 → 订正为与 `build_raw` 一致。
    - 插件 `engine.rs` 的背压文案 `"queue full"` → 中文（「队列已满…下一步…」），
      `code:4` 语义不变；用例改为钉住新文案。
    - `src/bridge/ffi.rs` 的 `host_deliver` 原把「载荷非法」与「mail 未配置」都打成
      「mail 未配置」→ 细分 `DeliverRoute`（`Routed`/`NotConfigured`/`BadPayload`）分开告警。
- **mail：白名单匹配语义 / 附件上限 / 结果归属 / 裸 CR / 弱 spoof 头 / 文案**
  - **白名单匹配语义可被绕过**：原实现对条目做裸 `ends_with` 后缀匹配，三处
    越权面：① `allowed_from: ["noreply@x.com"]` 放行同域仿冒 `evil-noreply@x.com`；
    ② 漏写 `@` 的条目（`["x.com"]`）放行跨域 `a@evilx.com`；③ `[""]`（空条目）令
    `ends_with("")` 恒真 = **白名单等于关闭**。改为**全等**匹配（条目只能是完整地址
    （地址全等）或 `@domain`（域全等），大小写不敏感；**不做子域通配**，子域须显式
    `@sub.x.com`），并在**装配期**逐条校验条目格式（空串/裸域/首尾空白 → 启动失败，
    文案点名 `smtp.<profile>.<字段>[<下标>]` + 下一步）；匹配期非法条目 fail-closed。
  - **附件无大小上限 + 同步读盘**：新增 `smtp.max_attachment_bytes`
    （默认 10 MiB）与 `smtp.max_total_attachment_bytes`（默认 25 MiB），超限 `code:5`；
    `path` 路先取长度再读（超限文件不进内存）；读盘 `std::fs::read` → `tokio::fs::read`
    （内部 spawn_blocking，不再阻塞 isolate 的 `current_thread`）；插件侧再复核一次
    （纵深防御）。**背景**：附件字节由宿主读盘后经有界队列（容量 256）持有，无上限时
    project root 内任意大文件（含 `config.yaml` —— 里面有 `jwt_secret`/smtp 口令）可被一次
    调用读入内存并放大成内存 DoS。
  - **结果通道可枚举/可覆写**（Important，B3）：① `mail.result(key, jobId)` 原**忽略 key**
    且无归属校验；② jobId 由**插件**生成（`{pid}-{seq}`，可猜）；③ 宿主不剥调用方自带的
    jobId，同 id `put` 覆盖 → 任意模块可读他人结果并**覆写成 `code:0`**。改为：宿主生成
    jobId（每进程随机 16 hex 前缀 + 单调计数，**不可猜**；调用方传入值一律剥离；回执的
    `data.jobId` 钉成宿主值）；enqueue 路在 submit **之前**登记一张带归属（profile + 模块 +
    租户）的票，插件上送只能**填充一次**（未登记/重复一律拒绝，`DeliverRoute` 增
    `UnknownJob`/`Duplicate` 分别告警）；`mail.result` 只回本归属的结果（跨 profile/模块/
    租户 → `null`）。插件侧兜底 jobId 同步改为随机前缀形态。
  - **裸 CR 未中和（SMTP smuggling 半开）**（Important，B4）：`normalize_crlf` 原只补 LF 前
    的 CR、**保留裸 CR**，正文含 `X\r.\r\n` 时以裸 CR 为行界的接收端会提前结束 DATA、余下
    内容被当命令执行（可注入伪造 MAIL FROM/RCPT TO）。改为三种行尾（CR/LF/CRLF）**一律归一
    CRLF**（口径与既有「裸 LF → CRLF」一致；拒绝裸 CR 会让同类行尾有两种相反处置），并覆盖
    结构化 `text`/`html`；**头区**的裸 CR 仍**拒绝**（归一它等于凭空造出一个头）。
  - **弱 spoof 头面**（Important，B5）：结构化 `headers` 禁覆盖清单由
    From/To/Cc/Bcc/Subject 扩到含 `Sender`/`Return-Path`/`Reply-To`（宿主与插件同清单）；
    raw 路剥离清单增 `Sender`/`Return-Path`（`Reply-To` 保留：raw 是调用方自备原文、它不改变
    信封与发件人身份）；**raw 与结构化 `headers` 互斥**由「静默忽略」改为 fail-loud `code:5`。
  - **文案与告警**：白名单未命中的文案不再 `{:?}` 回显**整份白名单**（换个
    profile key 即可枚举他 profile 的内域/客户域），只回「未命中 + profile 名」+ 下一步；
    `messageId` 由 MTA 原始应答改为「队列号或截断到 64 字符」（去服务器指纹/队列信息）；
    `tls: none`（明文）的 profile 在启动时打 **warn 级**告警（含 profile 名与 host:port，
    点明「内网 CIDR 限制未实现」；`file_transport` profile 不告警）；文档明确
    **`code != 0` 一律不得自动重试**（`2`/`3` 未启用，永久失败与瞬时失败同归 `1`，
    从 `code` 分不出可重试性），未新增 `data.retryable`（见 `docs/mail-smtp.md` §3 的取舍说明）。
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
- **路由：参数路由吞掉后到的静态兄弟（`server/src/routes.rs`，Important）**：`register` 的同
  pattern 去重原用 `matcher.at_mut(pattern)`——那是**路径匹配**而非 pattern 查找。已注册
  `/x/admins/{pk}` 时 `at_mut("/x/admins/me")` 会把 `me` 当成实参匹配成功 ⇒ ①**方法嫁接**：
  静态兄弟的 handler 被并进参数节点的方法表，`GET /x/admins/me` 落到 `{pk}` 的文件；
  ②**假冲突**：两个同动词静态（`me` / `session`）被判为 `route conflict`，请求期 500 且
  报的是无关文件；③`listing()` 列出实际不在树里的行。是否触发只取决于注册顺序（api 文件按
  路径排序：参数目录名 `pk` 排在 `session` / `sign-*` 之前即命中），所以此前只能靠给参数目录
  加 `zz-`/`{` 前缀改排序来规避。改为**按 pattern 字符串去重**：matcher 的 value 只存**槽位
  下标**（`usize`），真正的 `HashMap<method, Entry>` 移到表侧 `Vec`（`nodes`），pattern → 槽位
  由 `slots: HashMap<String, usize>` 记录，`matcher.insert` 每个 pattern 只调用一次。
  **静态段优先于参数段**由 matchit 自身优先级保证（与注册顺序无关）。release 直载
  `from_entries` 复用同一 `register`，一并修复。新增 4 条 unit 测试（含最凶的「同动词参数 vs
  静态」形态，并断言静态命中 `params` 为空），并做了**变异验证**：把去重换回 `at_mut` 后 4 条
  立即变红。实测：真实项目上旧二进制 2 条假冲突 + 4 个 500 + `oj test` FAIL，修复后 10 行路由
  全在表、7 个端点全 200、1/1 通过。文档同步 `docs/route-params-design.md` §5（去重不再走
  `matcher.at`、静态优先、冲突钉死）与 `docs/user-manual.md` §7.1（静态优先）。
- **路由冲突哨兵不再被后来的声明复活**（同上文件，Minor）：同一 `(pattern, method)` 被 **≥3 个
  文件**声明时，此前最后一个文件会把方法表里的 `Conflict` 冲回 `File` ⇒ 请求从 500 **静默变
  200** 且指向第三个文件（与 §5「同 pattern 同方法双声明 → 500」自相矛盾）。现在冲突**钉死**：
  保持 500，并再记一条 error 指出还有文件参与（测试 `table_conflict_survives_third_declaration`）。

**行为变更（升级注意）**
- **同位置异名参数现在是「结构性冲突」，release 会 fail-fast 起不来**（路由表改 pattern
  字符串去重的连带修正，升级务必核对）：`/x/{id}` 已注册时再注册 `/x/{name}`，此前因去重
  bug 被**静默合并**进同一节点（两条 URL 都可用，且参数名按先注册者算），现在按设计 §5 由
  matchit 报 `Conflict`：dev 是 error 日志 + **该 URL 404**（服务照常起），release 则因为
  `oj/src/app.rs` 把 `from_entries` 的 failures 当作致命错误 → **启动直接失败**（此前能起）。
  所以升级后「release 起不来 / 某条 URL 404」先查启动日志里的 `invalid route … conflict`。
  改法二选一：统一同位置的参数名（`/x/{id}` 一处即可），或把后来者挪出该位置（改成静态段或
  更深一层）。另：dev 与 release 的注册序不同（dev 按 api 文件全局排序、release 按
  `manifests.yaml` 锁顺序逐模块），跨模块的这类冲突在两个模式里丢弃的可能是不同文件
  ——见 `docs/route-params-design.md` §4/§5。
- **新增顶层 `smtp:` 段**：此前该键被忽略，现在会被解析——若旧配置里恰好有同名且**非映射**
  的键（如 `smtp: false`），启动会解析失败；改名或删掉即可。不写该段则行为完全不变
  （`mail.*` 调用报 `mail not configured`）。
- **配了 `smtp:` 但未装 `oj-mail` 插件**：不阻断启动，`mail.*` 调用报
  `mail not configured (config smtp: section missing, or oj-mail plugin not loaded)`
  ——与 es/auth 的「配置声明即 fail-fast」不同（mail 缺插件不构成安全失守，故意放行启动）。
  装插件：`cargo xtask plugin mail`。
- **`file_transport` 目录须先存在**（lettre 不建目录）；非空 `allowed_*` 是发信前提。
- **mail 双配置源互斥**：顶层 `smtp:` 与**非空** `plugins.mail` 同时出现时，
  此前 `plugins.mail`（原样透传）会**静默胜出**（改 `smtp:` 里的白名单/凭据不生效）；
  现在**装配期直接报错**（`pick one`），启动即暴露。`plugins: {mail: {}}`（空对象）
  不受影响 —— 它是「回落 `smtp:` 适配器」的写法（也是本仓 e2e 夹具的形态）。
- **白名单条目语义收紧**（需核对配置）：匹配由「裸后缀」改为**全等** ——
  ① `@x.com` **不再**覆盖子域（要子域须显式写 `@sub.x.com`）；② 裸域/空条目/首尾空白
  现在让**启动失败**（此前空条目会让白名单恒真）。若原配置靠后缀匹配「顺带」覆盖了一批域，
  请逐条补全。
- **`mail.result` 归属收窄 + jobId 形态变化**：enqueue 的 jobId 改由**宿主**生成
  （`<16 hex>-<序号>`，调用方传入值被忽略）；结果按「profile + 模块 + 租户」过滤 ——
  **跨模块回查不再可用**（改用 `bus.subscribe("mail.result")` 做通知）。
- **附件默认上限**：单件 10 MiB / 单封合计 25 MiB，超限 `code:5`；确有更大附件需求
  请在 `smtp:` 顶层调 `max_attachment_bytes` / `max_total_attachment_bytes`。
- **`sendRaw` 与 `headers` 互斥**：同时给会在宿主侧 `code:5`（此前静默忽略 `headers`）。
- **正文裸 CR 归一为 CRLF**：`text`/`html`/`raw` 正文里的裸 `\r` 由「原样保留」改为
  归一到 `\r\n`（防 SMTP smuggling）；raw **头区**含裸 CR 则直接 `code:5`。



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
- **tests：Windows 回归用例 `module_root_of_tolerates_verbatim_prefix_mismatch` 实参错配**
  - 该用例旨在验证 `module_root_of` 在 `from_dir` 与 project_root 的 `\\?\` 前缀**不一致**
    （canonicalize 加前缀、to_file_path 剥前缀）时仍命中模块根；但两向断言都把**referrer**
    目录（`with_prefix`/`no_prefix`）当成了 `root` 参数，方向一只把两者归一成同一个
    `src/m1/a/b` 路径、找不到 manifest 直接 `d == root` 跳出 ⇒ 返回 `None`（CI 在 Windows 红）。
  - 修正：方向一传 `from_dir = no_prefix` + `root = proj_root_prefix`（canonical 项目根），
    方向二传 `from_dir = with_prefix` + `root = proj_root_plain`；`module_root_of` 对二者均
    `strip_verbatim` 再比、返回无前缀模块根，故两向期望统一为 `strip_verbatim(canon/src/m1)`。
    函数逻辑未动（其前缀归一本就正确）。

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
