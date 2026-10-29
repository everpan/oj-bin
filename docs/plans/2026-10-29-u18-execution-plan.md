# U-18 批执行计划与任务跟踪（REQ-01/02 → REQ-05 → U-18/REQ-03 → U-17/REQ-04）

> 状态：批次 1–3 **已提交**（b0ec1c0，tag v0.1.103/v0.1.104，未推送）；**批次 4（v0.1.105 REQ-04 库-schema 层）F1–F6 全部 ✅，门禁全绿——发版动作等用户指令**（2026-10-29 重评取消 M3/M4 等待提前并入，Ruling 见 §2d）。
> （本文件是唯一任务真相源：状态、小结、增量记录都在这里，随做随更。）
> 依据：设计稿 `docs/plans/2026-10-29-u18-upstream-reqs.md`（v2，三路评审已处置）+ `oj-dbx/docs/upstream/REQ-01..05`。
> 原则：SOLID（校验纯函数/装配接线/门禁三腿各自独立）+ DRY（路径匹配复用 `path_matches` 单一实现、
> 字节计费与取消原语 103/104 共用）+ BDD（每任务 Given/When/Then）+ INVEST（每批次独立可发版）。

## 0. 批次与发版策略

| 批次 | 内容 | 版本动作 |
|---|---|---|
| **1** | REQ-01 装配校验 + REQ-02 类型面补齐与防漂移 | **bump v0.1.102**（CHANGELOG + 门禁全绿后提交打标推送） |
| 2 | REQ-05 语句级超时 + max_result_bytes（GA 门禁；导出原语先行） | bump v0.1.103 |
| 3 | U-18 / REQ-03 任务式导出（复用批 2 原语） | bump v0.1.104 |
| 4 | U-17 / REQ-04 库-schema 层（清单 + forSchema 收窄版） | **提前并入本期**：v0.1.105（重评：零 ABI + 护栏全现成，见 §2d） |

## 1. 任务跟踪（批次 1 · v0.1.102）

| # | 任务 | BDD 验收（Given/When/Then） | 状态 | 小结 |
|---|---|---|---|---|
| T1.1 | `RouteTable::patterns()` 访问器（注册 pattern 串含 `{param}`） | Given 建表注册 `/a/{id}` 与 `/a`，When patterns()，Then 含两者 | ✅ | ✅ | patterns() 访问器（slots 键集） |
| T1.2 | `validate_route_timeouts(patterns, route_keys)` 纯函数（app.rs） | Given 命中 pattern → Ok；零命中 → Err 点名 pattern+路由数；含 `{}` → Err；`/**` → Ok | ✅ | ✅ | 纯函数 + 矩阵单测（含 {} 拒、零命中点名） |
| T1.3 | 接线：Pipeline 构造前校验（tables 在作用域），热更同路径 | Given 零命中配置，When 装配/reload，Then 拒且文案点名 | ✅ | ✅ | tables 后内联校验，每次装配（含热更）生效 |
| T1.4 | serve_cmd 集成：真装配拒启 | Given config 含悬空 pattern，When `oj serve` 装配，Then 启动失败含点名 | ✅ | ✅ | 集成：悬空拒启点名 / 命中正常起服（fixture 需 manifest.yaml 教训入档） |
| T1.5 | serve e2e：spawn_pipeline 注 route_timeouts × {字面含 base, 尾 `/*`, `**`} × busy handler | Given 三形态命中 busy handler，When GET，Then 408（≥25ms 粒度余量） | ☐ | |
| T1.6 | api-manual `route_timeouts` 行：25ms 粒度 / fail-fast / DB 阻塞语义（跑完才 408）/ 保留路径与 WS 不受约束 / `{}` 拒 | 文档与实现逐条对齐 | ✅ | ✅ | 四要素齐：粒度/fail-fast/DB 阻塞/保留路径 |
| T2.1 | global.d.ts 三处声明：`DBInstance.access: DBAccessApi`（grant/revoke）、`epoch(name): number`、`stream/sse` opts + `fileName`/`disposition` | 对照 op 签名（client_db.rs:691/720）| ✅ | ✅ | DBAccessApi/epoch/stream|sse opts（tsc 绿） |
| T2.2 | typeof golden（腿①）：sample/tests/globals.test.ts，裸引用无条件全局清单（含 audit/mgmt/secret——可见性回归锚） | Given 业务模块形态，When 逐个 typeof，Then 全部非 undefined；删任一声明 → tsc 红 | ☐ | |
| T2.3 | tsc 用法测试（腿②）：sample/tests/type-usage.ts 按文档写法逐一调用 | Given 按文档写法，When `tsc --noEmit`，Then 绿；去掉某声明 → 红 | ✅ | ✅ | type-usage.ts 编译绿；负向验证（剥 secret 声明→红） |
| T2.4 | 归置 bin/devkit + xtask 契约（既有 10 项） | xtask build 后契约全绿 | ✅ | ✅ | xtask 契约 10 绿 |
| T2.5 | REQ-06：`db.client.{list,register,reRegister,delete,test}` + `db.access.{grant,revoke}` 运行期返回真 Promise（instanceof Promise、.catch/.finally 可用）；`has` 保持同步 | Given 业务模块，When 调 list()，Then `instanceof Promise === true` 且 `.catch` 可链；`list().catch(()=>[])` 不抛 TypeError | ✅ | ✅ | dbSessionCall 复用（DRY），instanceof Promise 断言 RED→GREEN |
| T3 | 发版：bump v0.1.102 + CHANGELOG + 全门禁（fmt/clippy/workspace/xtask+smoke）+ tag + push | 五步自查清单全过 | ☐ | |

## 2. 任务跟踪（批次 2 · v0.1.103 REQ-05，动工时细化）

**动工前 spike 结论（设计 §6.1 的两个待核查项）**：
- **Spike 1（FfiFuture Send 界）✅ 无需 ABI**：`oj-plugin-ffi/src/future.rs` 已
  `unsafe impl Send/Sync for FfiFuture`（字段全 raw/fn 指针），宿主 `await_ffi`
  是普通 async fn——`tokio::time::timeout`/`select!` 竞速对插件轴同样可用。
- **Spike 2（插件池弃连暴露面）→ 判给 ABI 13 单独成车**：宿主弃 FfiFuture 只 free
  state（future.rs 头注释明文：插件任务允许跑完、不保证取消）；mysql/pg 服务端
  真取消需插件侧协作（vtable 新槽或 opt-in 行为变更）。**批次 2 交付宿主侧竞速 +
  结构化 OJ_DB_TIMEOUT（全后端）+ sqlite 内置真早停**；`data.cancelled` 按后端诚实标注；
  插件轴真取消登记为 ABI 13 车，不阻塞本批。

**裁决（Rulings，动工时定）**：
- 字节口径 = 行 JSON 编码字节（`serde_json::to_vec(row).len()` 累加）——≥线上原始字节、
  更严、确定性可测；写进 api-manual。
- 触顶信令：truncate → 响应头 `X-OJ-Result-Truncated: <counted>`（raw 路径，对齐
  X-OJ-Row-Limit 先例）+ db.stream 走 `op_db_stream_info`（{rows,bytes,truncated}）；
  fail → 结构化错误（category=Capacity，data={limit_bytes,counted_bytes}）。
- 收紧校验错误：调用侧 timeoutMs > 配置 statement_timeout_ms → category=Capacity、
  message 点名 `statement_timeout_ms`（REQ §6 验收原文），code=OJ_DB_CAPACITY。
- tx/attach 路径：attach 会话内语句超时 = 判死会话（kill_attached 先例，disposition
  close）；plain db.tx 不叠语句超时（连接状态不可知先例，维持寿命封顶口径）。
- 每调用侧 opts 只给 db.query/db.exec/db.stream（REQ 原文形态）；构造器路径只吃配置侧
  （REQ 覆盖要求 = 三条路径都生效，配置侧即满足）。

| # | 任务 | BDD 验收（Given/When/Then） | 状态 | 小结 |
|---|---|---|---|---|
| Q1 | db_policy 配置面：DbAccess/DbAccessOverride 增 `statement_timeout_ms`/`max_result_bytes`/`on_exceed`(fail\|truncate)，validate + 逐键继承 | Given defaults/alias 各键声明，When policy_for，Then 合并正确；0/超界/非法枚举 → 装配 Err 点名键 | ✅ | |
| Q2 | `db_quota` 新模块：Quota 聚合（policy 单一出口）、timeout 收紧校验、ByteMeter（feed/Stop/超界）、结构化错误构造器 | 纯函数矩阵单测 | ✅ | |
| Q3 | deadline 竞速接线：query_impl/exec_impl pool 臂 select!；attach 会话臂 race+kill_attached；op 签名加 opts(timeoutMs)；bootstrap.js/d.ts | Given 慢语句+短 deadline，When db.query，Then OJ_DB_TIMEOUT（elapsed≥deadline）；传大 timeoutMs → 拒并点名配置键 | ✅ | |
| Q4 | max_result_bytes：trait 默认 metered 方法（默认全量后计量=插件轴/假件兼容）；SqlxAccessor 增量 fetch 早停；query_impl 接线 fail/truncate | Given 行集超字节上限，When db.query，Then fail=结构化 Err / truncate=部分行+X-OJ-Result-Truncated | ✅ | |
| Q5 | 构造器路径接线（query.rs Exec::Pool → metered + deadline） | Given builder select 慢/大结果，When 执行，Then 同 raw 语义 | ✅ | |
| Q6 | stream 路径：open opts.timeoutMs + pump 字节计量（fail→结构化 Err 行经 parse_raw 还原；truncate→done+标志）+ op_db_stream_info + JS info() | Given 流超字节，When next()，Then fail=抛结构化错 / truncate=干净 done 且 info().truncated=true | ✅ | |
| Q7 | 矩阵与收紧用例：sqlite 真慢语句（递归 CTE）×三路径；pg/mysql env-gated 骨架（R-1.3 并入）；timeoutMs>配置 拒绝 | env 缺席则 skip（诚实登记） | ✅ | pg/mysql 双腿经 Apple container 真库亲跑全绿（504ms 触发 OJ_DB_TIMEOUT）；修出一处真问题：`server_cancel_on_abandon` 从「SqlxAccessor 恒 true」改为按方言（仅 sqlite=true），网络方言弃 future ≠ 服务端取消 | |
| Q8 | 文档四件 + CHANGELOG + bump v0.1.103 + 全门禁 + tag + push | 同批次 1 T3 五步 | ✅ | |

## 2b. 任务跟踪（批次 3-4，动工时细化）

| 批次 | 任务骨架（INVEST：独立可发） | 前置 |
|---|---|---|
| 2（REQ-05） | 访问器层 deadline 竞速 + 弃连取消；`db_access.statement_timeout_ms`（只能收紧）；`max_result_bytes` 行级字节计费（fail/truncate 两档）；`OJ_DB_TIMEOUT` 结构化信封；三路径×三方言矩阵；db.session 上的判死语义 | 批 1 |
| 3（U-18） | 同步开游标 + 自驱消费任务（节流）+ ExecRegistry 注册；csv/jsonl 汇聚落 blob；进度/状态/取消 ops；宿主 TTL 回收器；配额（全局/每主体/回落硬顶）；审计事件 | 批 2 原语 |
| 4（U-17） | catalog kinds `schemas`/`databases`（系统库过滤 + granted_only）；`forSchema` 受限视图；sqlite unsupported 矩阵 | 独立 |

## 2c. 任务跟踪（批次 3 · v0.1.104 U-18/REQ-03 任务式导出，动工时细化）

**动工前结论（spike 已备）**：复用件全部在位——`da.stream_query` 开游标、`ExecRegistry` 壳
（boot-tag id / Owner 归属 / Mutex 表）、`BlobBackend::{put_stream_open,chunk,finish,abort,del}`
（**有 `del` 槽**：TTL 回收按宿主导出台账删 key，无需 list API，零 ABI 成立）、
`db_quota::ByteMeter`（maxBytes 计费）、`AuditStore::push`（零凭据事件）。
规格 `docs/plans/2026-10-29-u18-upstream-reqs.md` §3。

| # | 任务 | BDD 验收（Given/When/Then） | 状态 | 小结 |
|---|---|---|---|---|
| E1 | config `db_export:` 段：全局并发 4 / 每主体 2 / maxBytes 缺省 / 回落硬顶 128MiB / TTL 24h / 清扫 cadence；validate fail-loud 点名键 | Given 非法组合（0/超界/回落硬顶<TTL 语义冲突），When 装配，Then Err 点名 | ✅ | DbExportSection 六键 + validate 十组矩阵（0/超界/fallback≥max_bytes 点名）| |
| E2 | `db_export` 模块：ExportRegistry（ExecRegistry 同款壳）+ 状态机 running/done/failed/cancelled + 进度 {rows,bytes} + 配额（全局/每主体，错误文案分「全局满/本人满」）+ boot-tag not-found | 纯单测矩阵：登记/归属拒绝/配额满两文案/幂等 cancel/重启 id not found | ✅ | db_export.rs：7 用例（轮询/归属/not-found/双配额文案/幂等/跨注册表/清扫豁免）| |
| E3 | `op_db_export`：同步开游标 + 占流预算 permit + 登记 + spawn 自驱消费任务（csv/jsonl 逐块 put_stream_chunk + 每 N 批 yield 节流）+ 同步返 taskId（<200ms 计时断言）；未知 format → OJ_DB_UNSUPPORTED（data.capability=format 名） | Given 慢查询大结果集，When db.export，Then 立即返 taskId；轮询 exportStatus 见 rows/bytes 递增 | ✅ | | op_db_export 三 op + 自驱消费任务（节流/双轨闸/取消链）；12 集成用例全绿（立即回执/跨请求存活/幂等取消/审计/maxBytes vs maxRows）
| E4 | `op_db_export_status` / `op_db_export_cancel`：跨请求可达；他人/未知/跨重启一律 not found（不泄露）；cancel 幂等 → abort notify → pump 停 → put_stream_abort 清半成品 → state=cancelled | 断线重连用例：发起后 drop 请求，另起请求轮询递增 + 取消 | ✅ | |
| E5 | maxRows 策略截断（done + truncated:true + truncatedReason:"max_rows"）vs maxBytes 超限 failed（错误≠策略双轨）；格式细节：csv 带 BOM/引号转义/NULL，jsonl 行 JSON | Given cap 配置，When 导出超 cap，Then 对应信令 | ✅ | | csv BOM+\r\n / jsonl 行 JSON；maxBytes failed vs max_rows done+truncated 双轨用例钉死
| E6 | TTL 回收器：tokio interval 按台账年龄清扫（TTL 自 put_stream_open 起算），live 任务 key 豁免；`oj test` 可注入短 TTL/短间隔 | Given 短 TTL 注入，When 轮询清扫，Then 过期 key 被删、live 豁免 | ✅ | | 懒启动 interval 清扫器（每注册表一次）+ sweep 豁免 live；单位层过期/新鲜/live 三分支用例（集成走短周期段注入）
| E7 | 审计事件：发起/完成/取消/maxBytes 中止（主体/tenant/alias/rows/bytes/终态；SQL 全文不入审计）；taskId/blobKey 日志只出前 8 字符 | 审计 store 断言 kind 与字段 | ✅ | | start/done/failed/cancelled 四事件；SQL 全文不入审计用例钉死
| E8 | bootstrap.js db.export/exportStatus/exportCancel + global.d.ts + tsc 锚 + 文档四件 + CHANGELOG + bump v0.1.104 + 全门禁 | 同 Q8 五步；发版动作等用户指令 | ✅ | | d.ts 三件 + tsc 锚 exportUsage + 文档四件 + CHANGELOG + bump；发版提交动作等用户指令

## 2d. 任务跟踪（批次 4 · v0.1.105 U-17/REQ-04 库-schema 层，2026-10-29 重评后提前并入本期）

**重评结论（取消 M3/M4 等待）**：内省 SQL 全部宿主生成（`introspect.rs` 常量，插件轴只承载执行），
范围钉死点 = `DATABASE()`/`current_schema()` 约 15 处谓词——`forSchema` = 把这些谓词改为绑定
参数的机械变换，**零 ABI**；granted_only / MAX_ROWS / trace 脱敏 / 占位符改写护栏全部现成。
REQ-04 自己警告「等连接树挂满后再改返工成本变大」，成本既已降到「一个中等批次」，立即做正省钱。
M3/M4 真正该等的只剩 ABI 13 车厢（插件真取消）。

**Ruling**：计划 §2b 行「catalog kinds `schemas`/`databases`」**改为独立 op、不加 catalog kind**——
再加两个 kind 会造出清单双入口（`db.catalog("schemas")` vs `db.listSchemas()`），违背
listTables 系列「清单即 op」先例。成本若错：将来合并入口 = 一次纯 JS 面重定向。

| # | 任务 | BDD 验收（Given/When/Then） | 状态 | 小结 |
|---|---|---|---|---|
| F1 | `introspect.rs` 清单面：`Listing{Schemas,Databases}` + `listing_sql`（系统库过滤统一口径：mysql information_schema/mysql/performance_schema/sys；pg schema pg_catalog/pg_toast%/information_schema；pg database datistemplate）+ `listing` 归一化（{name,owner?,isDefault?,comment?}）+ `scoped_sql`（DATABASE()/current_schema()→绑定参数；sqlite→None） | 纯单测：三方言 SQL 文本矩阵、系统过滤断言、scoped 变换参数计数、sqlite None | ✅ | introspect.rs：Listing 三方言 SQL 矩阵 + listing 归一化（可选键 NULL 不落）+ scoped_sql（范围谓词→绑定参数，sqlite→None）；4 新单测 |
| F2 | `op_db_list_schemas`/`op_db_list_databases`：同 list_tables 骨架（门控→granted_only→prefix LIKE→LIMIT→trace 脱敏）；WHITELIST 白名单 +2；mod.rs 注册；sqlite 腿（PRAGMA database_list→main） | Given sqlite 内存库，When db.listSchemas()，Then [{name:"main",isDefault:true}]；oracle → unsupported 点名 | ✅ | op_db_list_schemas/list_databases + listing_impl（prefix Rust 侧 starts_with——PRAGMA 不接 WHERE）；WHITELIST+2；sqlite 腿 TVF `pragma_database_list`（isDefault=name==main）；oracle 门控 |
| F3 | 五件内视 op 加尾参 `scope`：活动 scope 时 scoped_sql + scope 参数前置（表名参数后移）；sqlite scope → OJ_DB_UNSUPPORTED(capability=for_schema)；畸形 scope → 空结果不泄漏；mysql Ddl SHOW 例外在 scoped 模式 → unsupported | 单测：scoped SQL 参数序、sqlite/SHOW 两道 unsupported、畸形输入空结果 | ✅ | Scope/resolve_scope（sqlite→unsupported 点名 for_schema；畸形→Empty；点号合法）；五件 op 尾参 scope，参数前置序钉死；mysql SHOW 例外 scoped 模式显式 unsupported |
| F4 | bootstrap.js `forSchema(name)` 受限句柄（五件只读内视，无 query/exec）+ 实例 `listSchemas/listDatabases`；global.d.ts（SchemaInfo/DatabaseInfo/IntrospectScope）+ type-usage.ts tsc 锚 | tsc 绿；golden 全局清单不变（无新全局） | ✅ | bootstrap.js forSchema 受限句柄（五件只读）+ listSchemas/listDatabases；d.ts 三类型入 declare global（顺带修 Column/Index/FKInfo 模块作用域 TS2304 潜在债）；tsc 锚 schemaListingUsage |
| F5 | env-gated 真库腿（OJ_TEST_PG/OJ_TEST_MYSQL）：pg 建 schema+表 → listSchemas 含之、forSchema('x').listTables 跨 schema 取表；mysql 建 2 业务库 → listDatabases 过滤系统库、forDatabase≡forSchema 同实现成文 | env 缺席 skip 诚实登记 | ✅ | pg 腿：oj_req04 schema 建表→listSchemas 含之/forSchema 取表/默认 listTables 不越 schema/pg_database 模板过滤；mysql 腿：oj_req04 建库→系统库过滤/schema≡database 同源/isDefault；双容器一次全绿 |
| F6 | 文档四件（api-manual 章节+限制表、SKILL 陷阱、scenarios 场景 55、README 场景表）+ db-guide 指针 + CHANGELOG v0.1.105 + bump + 全门禁 | 同 Q8 五步；发版动作等用户指令 | ✅ | CHANGELOG v0.1.105 + devkit 四件 + db-guide §11.5.8/§2 表 + bump；门禁全绿（含 clippy）：fmt/clippy/workspace 1674/1674（真库腿在内）/xtask build+smoke/契约/oj test | |

## 3. 增量记录（随做随更）

- **批次 1（v0.1.102）完成并已推送**（tag `v0.1.102`，gitee main `09b2c1c`）：
  T1.1-T1.6 + T2.1-T2.5 全 ✅，门禁全绿（fmt / clippy -D warnings / workspace 50 二进制 /
  xtask build+smoke / 契约 10 / `oj test` 63/63）。
- 关键提交：4550d7c（校验纯函数+访问器）→ 62d6336（接线+集成测试）→ b113cdc（REQ-06 真
  Promise）→ 393d419（类型面+门禁双腿）→ fab3e3f（bump+CHANGELOG）→ 09b2c1c（三形态 e2e）。
- **测试教训入档**：① serve 装配集成测试的 fixture 模块必须有 `manifest.yaml`（装配期
  模块扫描 fail-loud 在我的校验之前，曾误判校验未生效）；② 尾段通配 `*` 只匹配一段——
  两层路由要写 `/v1/api/u/*`（e2e 初稿写错曾挂起 60s+，busy handler 无超时不会自愈）；
  ③ 后台跑 cargo 的陷阱：超时迁移=SIGSTOP（0% CPU 假跑）+ pkill 残留 flock——统一
  `run_in_background` 起跑 + `> 文件` 收输出。
- **批次 2（v0.1.103）完成**：Q1–Q8 全 ✅。新增 `src/bridge/db_quota.rs` 纯原语模块
  （Quota/effective_timeout_ms/ByteMeter/race_statement/结构化错误工厂），query/exec/stream/
  构造器四路径接线，文档四件 + CHANGELOG 同步。门禁全绿；**真库矩阵腿经 Apple container
  亲跑**（oj-test-postgres / oj-test-mysql 双容器）：pg 504ms / mysql ~500ms 触发
  OJ_DB_TIMEOUT，验证「网络型后端准点触发」架构论断。
- **真库跑出的第二处存量修复**：`search_shared_module_on_real_dbs`（search 模块真库腿，
  同样从未本地亲跑）在 pg 挂 `unsupported sqlite column type: REAL`——sqlx Any 的 float4
  （pg REAL / mysql FLOAT）是独立于 f64 的 TypeInfo，行解码探测链漏 `f32` 臂。补臂后绿。
- **真库跑出的修复（TDD：测试先红后绿）**：`SqlxAccessor::server_cancel_on_abandon` 原实现
  恒 `true`，pg 腿报 `cancelled=true` 与「弃 future ≠ 服务端取消」诚实口径矛盾 → 改为按方言
  （仅 sqlite=true）。网络方言（内置 sqlx pg/mysql DSN 与插件轴）同口径 `false`。
- Rulings 汇总见台账 `.superpowers/sdd/2026-10-29-u18-execution-plan/progress.md`（main 直跑、
  golden 剔 sess、REQ-06 复用 dbSessionCall、本仓执行纪律）。

## 4. 诚实登记（避免遗忘）

- **R-1.3 慢语句用例（评审 Important）**：✅ 已闭环——并入批次 2 的
  `statement_timeout_matrix_pg_mysql`，并经 Apple container 真库亲跑双腿全绿（本环境常驻
  oj-test-postgres / oj-test-mysql 容器，DSN 经 `OJ_TEST_PG` / `OJ_TEST_MYSQL` env 注入）。
- **评审 Minor 处置**：已修=golden 头注释与清单对齐（Kafka/RabbitMQ/tasks 实为无条件挂载）+
  CHANGELOG REQ-06 措辞收窄（仅 `list` 回裸值，其余六个本就是 async op）；缓议=`patterns()`
  文档错位（注释位置）、被保留路径遮蔽的同名业务路由键（极端窄径，可后续剔除）。

- REQ-01 交付 ≠ 慢查询护栏：DB 阻塞语句跑完才 408（§1.1 已成文），真护栏 = 批次 2。
- 批次 2 的插件轴 select! 竞速（FfiFuture Send 界）与弃连的池内暴露面未核查——动工时先做这两项
  spike，若必须动 vtable 则 ABI 13 单独成车。
- 类型面 golden 清单只圈**无条件挂载**全局；条件挂载面（mail/es/cert/jwt/oidc/sso/Kafka/…）
  漂移风险登记为已知限制（架构 m-2）。
