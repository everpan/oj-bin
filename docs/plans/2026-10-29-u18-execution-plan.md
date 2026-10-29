# U-18 批执行计划与任务跟踪（REQ-01/02 → REQ-05 → U-18/REQ-03 → U-17/REQ-04）

> 状态：**批次 1（v0.1.102）执行中**（需求源新增 REQ-06：db.client/access 运行期非 Promise——并入本批，见 T2.5）。
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
| 4 | U-17 / REQ-04 库-schema 层（清单 + forSchema 收窄版） | 另期（M3/M4 窗口） |

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

## 2. 任务跟踪（批次 2-4，动工时细化）

| 批次 | 任务骨架（INVEST：独立可发） | 前置 |
|---|---|---|
| 2（REQ-05） | 访问器层 deadline 竞速 + 弃连取消；`db_access.statement_timeout_ms`（只能收紧）；`max_result_bytes` 行级字节计费（fail/truncate 两档）；`OJ_DB_TIMEOUT` 结构化信封；三路径×三方言矩阵；db.session 上的判死语义 | 批 1 |
| 3（U-18） | 同步开游标 + 自驱消费任务（节流）+ ExecRegistry 注册；csv/jsonl 汇聚落 blob；进度/状态/取消 ops；宿主 TTL 回收器；配额（全局/每主体/回落硬顶）；审计事件 | 批 2 原语 |
| 4（U-17） | catalog kinds `schemas`/`databases`（系统库过滤 + granted_only）；`forSchema` 受限视图；sqlite unsupported 矩阵 | 独立 |

## 3. 增量记录（随做随更）

- （批次 1 动工前，无）

## 4. 诚实登记（避免遗忘）

- REQ-01 交付 ≠ 慢查询护栏：DB 阻塞语句跑完才 408（§1.1 已成文），真护栏 = 批次 2。
- 批次 2 的插件轴 select! 竞速（FfiFuture Send 界）与弃连的池内暴露面未核查——动工时先做这两项
  spike，若必须动 vtable 则 ABI 13 单独成车。
- 类型面 golden 清单只圈**无条件挂载**全局；条件挂载面（mail/es/cert/jwt/oidc/sso/Kafka/…）
  漂移风险登记为已知限制（架构 m-2）。
