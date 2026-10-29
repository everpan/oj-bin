# U-18 批 · 上游需求处置设计 v2（REQ-01 超时校验 / REQ-02 类型对齐 / REQ-03 任务式导出）

> 输入：`oj-dbx/docs/upstream/REQ-01..03`（2026-10-29）。
> v2 = 三角色评审（架构/工程/产品）+ builder check 合并处置后的修正稿。评审发现全部登记于 §5；
> 关键收敛：blob TTL 回收**零 ABI**（三路 + builder check 四方同归）、REQ-01 对 DB 阻塞语句的
> 真实语义如实成文（P-1）、导出泵补消费任务与节流（工程 B-3/架构 I-1）、孤儿回收条款纠正
> （架构 B-2/工程 B-2）、REQ-01 匹配集口径纠正（工程 I-4/I-5 + 架构 B-1/I-4）。
> 面向读者：负责实现的 builder——不必回访需求方，细节缺口按 §4 自行裁决。

## 0. 结论速览

| 项 | 根因/性质 | 处置 | 体量 |
|---|---|---|---|
| REQ-01 | **matcher 机制完好**（实测 408 @41ms）；真缺陷 = 配错 pattern 静默 + 25ms 粒度未成文 + **对 DB 阻塞语句跑完才 408（须如实成文）** | 装配期 fail-fast（口径见 §1.2）+ 语义文档 + 分层测试 | 小 |
| REQ-02 | 四处类型面缺陷：三处漏声明 + 全局作用域写错（**作用域缺陷已修**，audit 自 v0.1.98 带病） | 补声明 + 防漂移门禁（存在性×可见性双断言） | 小 |
| REQ-03 | 能力缺口：json.stream 单请求模型 | **U-18 任务式导出**：db.export 三件 + 宿主侧 TTL 回收（零 ABI） | 中 |

发版：三件同车 **v0.1.102**。滑期切分线（PM P-10）：REQ-03 若滑期，REQ-01/02 单独成车先行
（REQ-02 是 db.export 的消费前提，REQ-01 是 M3 过渡期 CSV 直出的护栏，均独立可发）。

---

## 1. REQ-01 · route_timeouts 装配期校验（fail-fast）

### 1.1 根因（已实测闭环）

- matcher 对含 base 字面量匹配正确；配置链完整；决定性实验：2s 忙循环路由 + 精确 pattern @1ms
  → **408 @41ms**（flat 与 module-root 双模式验证）。
- 报告者 7 种 pattern「全不命中」的真因：**看门狗 25ms 轮询粒度**——`/dbx/status` 仅 ~11ms，
  1ms deadline 赶不上第一个 tick；「`/**` 生效」是 login 恰好 >25ms。
- **对 DB 阻塞语句的真实语义（P-1，必须如实成文）**：看门狗杀的是 V8 执行；已下推到 Rust/DB
  的语句**跑完后**才回 408（dbx R-24 实测 11.8s 语句完整执行后 408）。e2e 须补一条真慢语句
  用例（env-gated：pg `pg_sleep` / mysql `SLEEP`）把该语义钉进测试；api-manual 明写
  「route_timeouts 管得住 JS 忙循环，**管不住已下推的慢语句**；慢 SQL 护栏 = 语句级真取消
  （REQ-05，GA 门禁）」。**发版说明必须显式：REQ-01 交付 ≠ 慢查询护栏**。

### 1.2 装配期校验（fail-fast；评审修正后口径）

挂点：路由表（app.rs:1817 建）与 Pipeline（app.rs:2010 建）之间，内联进现有
route_timeouts 的 `.map()` 闭包（`audit_schemas`「表后硬失败」先例）；**每次装配都校验**
（含 dev 热更——热更后 pattern 悬空同样拒绝该次 reload 并点名）。

- **匹配集 = 各 api 挂载路由表全键**（评审 B-1/I-5 纠正：`{base}/health`、`{base}/plugins`
  是直挂 axum 路由，不经 run_route、**不消费** route_timeouts，不得进匹配集——否则只命中
  保留路径的 pattern 过校验却永不生效，再造假保险）；WS 挂载同理不入。
- **pattern 含 `{`/`}` → 直接拒**（架构 I-4：运行期比对的是真实请求路径，`{id}` 永不出现，
  允许它 = 静默无保护；参数路由请用 `*`/`**`）。
- 命中 0 条 → 装配错误拒启动，文案点名 pattern 与路由数，并附一句「保留路径 /health、
  /plugins 与 WS 挂载不受 route_timeouts 约束」。

### 1.3 测试与文档（工程 m-8 分层）

- **装配层单测**（不起服务）：不命中 / 保留路径仅命中 / 含 `{}` / `/**` 天然命中 —— 断言
  装配 Err 与文案；
- **活服务 e2e**（3 形态：含 base 字面 / 尾 `/*` / `**`，命中方）× 慢 handler（busy-loop
  500ms，进 `skip_infinite_loop` 逃逸门注册预案）→ 断言 408（**放宽到 ≥25ms 粒度 + 调度余量**）；
- **真慢语句用例**（env-gated pg/mysql）→ 表征「跑完才 408」语义；
- api-manual：粒度（≥100ms 探针）+ DB 阻塞语义 + fail-fast 行为 + 保留路径/WS 不受约束。

### 1.4 明确不做

不改 matcher；不改「含 base」手册口径；不做运行期路由热更重校验之外的动态校验；**不在本批
实现「超时中断已下推语句」**（= REQ-05 语句级真取消，另立）。

---

## 2. REQ-02 · 类型面补齐 + 防漂移门禁

### 2.0 第四处缺口（评审期新发现，**已修**）：全局作用域写错

`declare const audit/mgmt/secret` 原在 `export {}` 模块作用域内，业务模块引用 TS2304
（`audit` 自 v0.1.98 带病）。已移入 `declare global`（tsc 验证）。门禁因此必须断言
**全局可见性**而非仅存在性：typeof golden 用例以**业务模块形态**（无 import）编译。

### 2.1 三处声明（签名以 bridge 实现为准）

`DBInstance.access: DBAccessApi`（grant/revoke）/ `DBInstance.epoch(name): number` /
`JsonApi.stream|sse` opts 增 `fileName?: string; disposition?: "attachment"|"inline"`。

### 2.2 防漂移门禁（工程 I-6 修正后：tsc 是唯一可靠解析器）

- **腿①（存在性）**：`oj test` L1 typeof golden——**只圈无条件挂载的全局清单**
  （架构 m-2：Kafka/LDAP/redis/mail 等条件挂载全局随配置缺席，纳入必 flake；清单进 golden
  文件并注明圈定口径）；
- **腿②（可用性）**：`tsc --noEmit` 类型用法测试——一个 .ts 按文档写法逐一调用全部声明面
  （REQ-02 §6 验收原文），声明缺失 tsc 即红，**零正则**；xtask 侧维持既有
  「bin/devkit/global.d.ts ≡ sample/global.d.ts」契约；
- 双腿合取覆盖「类型滞后」与「文档超前于运行时」双方向；剩余盲区（三方全漏的新面）登记不做。

---

## 3. REQ-03 · U-18 任务式导出

### 3.1 核心判断（评审修正后）

导出 = **同步开游标 + 自驱消费任务 + 进度记账 + 复用 U-5 取消链**。「复用 exec_sessions」
只在取消侧成立（架构 I-1）：db.stream 的泵是**消费端驱动**的（有界通道，无消费者即停车），
导出发起即返回后必须由**新的 Rust 消费任务**持续 drain——这是与 db.stream 的本质差异，
也是本设计最大的新执行体。

### 3.2 API 面

```ts
const { taskId } = await db.export({
  sql, params?, format: "csv"|"jsonl",   // 未知 format → OJ_DB_UNSUPPORTED（capability=format 名）
  fileName?,                              // 缺省 export-<taskId>.<ext>
  maxBytes?,                              // 超限 fail-fast（failed，不静默截断）
});
const st = await db.exportStatus(taskId);
// { taskId, state: "running"|"done"|"failed"|"cancelled",
//   rows, bytes, truncated?, truncatedReason?,        // ← max_rows 策略截断可见化
//   startedAtMs, finishedAtMs?, error?, blobKey? }
await db.exportCancel(taskId);              // 幂等；他人/未知/跨重启 → 一律 not found
```

- **对 REQ 的两处偏差（随答复回执给 dbx，PM P-5）**：①签名改实例方法（alias 由
  `DB(name)` 决定，对齐 db.stream 面）；②砍 `progress.percent`（流式无总行数，做 percent
  需额外 COUNT 且有竞态）——UI 口径 = **不确定进度条 + rows/bytes 文本**；`totalRows` 留
  可选扩展位。
- 归属：task 归发起主体；taskId/blobKey 均 bearer 语义，日志只出前 8 字符（PM P-7：blobKey
  与 taskId 同款红线；serve 访问日志对 `/blob/{key}` 路径脱敏为前 8 字符）。
- **重启语义（架构 B-2）**：id 内嵌 boot tag（与 db.session 同构）——重启后旧 taskId 一律
  **not found**（不是 failed，避免误读为 SQL 错；PM P-3 的「判死混同」由此消解）；跨重启
  恢复为**有意裁剪**（非目标段已注明并与 dbx 对齐）。

### 3.3 执行模型（工程 B-3/架构 I-1 修正后）

1. **同步开游标**：export op 内同步打开流式游标（否则「已发 taskId、无句柄可登记」的取消
   空窗——ExecEntry 注册需要流句柄）；开游标即取 U-7 流预算 permit（计入全局流预算，
   与 db.stream 共享护栏——工程 m-9/架构 I-3 裁决），** permit 持有至任务终态**；
2. **自驱消费任务**：Rust 消费任务持续 drain 泵通道（csv/jsonl 逐块写 blob）；
   **泵节流**：每 N 批 `yield_now`/短 await 让出，避免 5000 万行导出在 current_thread
   运行时上饿死全部 JS isolate（工程 B-3）；
3. **进度**：registry（进程内 Mutex，ExecRegistry 同款壳）每批更新 rows/bytes；
4. **取消**：复用 db.cancel 全链（abort notify → pump 停 → `put_stream_abort` 清半成品，
   FFI 注释明确必加，防 s3 orphan parts）→ state=cancelled，幂等；
5. **max_rows（Q2/架构 I-2 收敛）**：导出尊重 cap，达 cap → state=done +
   `truncated: true` + `truncatedReason: "max_rows"`（与查询 X-OJ-Row-Limit 同哲学——
   策略截断必须可见）；maxBytes 超限仍 failed（错误 vs 策略，双轨分清）；
6. **配额**：全局并发 4 + **每主体 2**（PM P-9），错误文案区分「全局满 / 本人满」；
   `put_stream` 回落路径（第三方插件缺流式时整段缓冲）单独硬顶（缺省 128 MiB，config 可调；
   工程 m-7/架构 m-4：不设顶 = 缺省配额下最坏 4 GiB 堆）；
7. **审计（PM P-6）**：发起/完成/取消/maxBytes 中止落审计事件（主体/tenant/alias/rows/
   bytes/终态；**SQL 全文不入审计**，按既有零凭据口径）。

### 3.4 产物与回收（评审收敛：**零 ABI 宿主侧方案**）

> blob TTL 不做成 blob 轴能力（Q3/Q1/架构 B-3/工程 B-1 四方同归）：`BlobBackendVtable` 无
> list/expiry 槽，加槽 = ABI bump + 全插件 lockstep；且 s3 ListObjectsV2 **不返回 user
> metadata**，「按 expiresAtMs 过滤」在 s3 上根本读不到。

- 导出产物 key 用**保留前缀**（如 `export/`）；宿主 tokio interval 回收器按**年龄**清扫
  （local mtime / s3 LastModified），**TTL 时钟自 put_stream_open 起算**（架构 B-2：否则
  崩溃半成品无 TTL 可查）；**live 任务名下的 key 豁免清扫**（合并 PM P-8 的「长任务保护」）；
- 缺省 TTL 24h（config 可调；`oj test` 可注入短周期短间隔以便断言）；
- fileName/contentType 元数据存宿主导出台账；下载 header：local 首方保证
  Content-Disposition（sidecar 机制扩展），s3 腿尽力（对象 Content-Type 必有；文件名
  尽力），builder 落实时验证 s3 put 的元数据能力并写明差异（PM P-4：验收 = 浏览器另存为
  原名 local 全绿）。

### 3.5 验收 ↔ 机制 ↔ 测试对照（PM P-2）

| REQ-03 §5 验收 | 机制 | 测试 |
|---|---|---|
| 5000 万行 200ms 内回 taskId | 同步开游标（开游标轻，查询下推异步）→ 立即返 taskId | 计时断言 |
| 进度递增 | registry 每批更新 | 轮询断言递增 |
| 真取消 + 幂等 | db.cancel 全链 + put_stream_abort | mysql/pg env-gated + sqlite 协作式 |
| **断线重连任务仍在** | 进程内 registry 与请求解耦 | **发起后断连接，另起请求轮询 rows 递增** |
| TTL 回收磁盘不增长 | 宿主年龄清扫（live 豁免） | 注入短 TTL 断言删除 |
| 他人 taskId 不可见 | 归属校验，not found 不泄露 | 跨主体用例 |

### 3.6 非目标 / 反目标

- 非目标：xlsx/parquet（format 注册表扩展位，capability 报 unsupported）；**跨重启任务恢复
  （有意裁剪，已与 dbx 对齐——REQ §4 四痛点之一按此口径回执）**；dbx 前端轮询 UI；非 blob 产物。
- 反目标：同步导出变体；产物 key 可预测；绕过 db_access 判权（导出与 db.query 同门，
  max_rows 口径见 §3.3-5）。

---

## 4. 留给 builder 的裁决（已按 house style 定，改动请记录在案）

1. config 段名与键（建议 `db_export:` 独立段：并发全局/每主体、maxBytes 缺省、回落硬顶、
   TTL、清扫 cadence；fail-loud 组合对齐 §1.3 速查风格）；
2. csv 方言细节（BOM/引号转义/NULL——对齐 scenarios 既有 CSV 口径）；
3. REQ-02 typeof golden 的无条件挂载清单落点（sample/tests 单文件，xtask 只做 d.ts 一致契约）；
4. s3 腿 Content-Disposition 的实际能力验证与差异成文；
5. `DB("rep").export(...)` 组合用例（动态别名天然可用）。

## 5. 评审处置登记（v2 修正稿）

| 来源 | 发现 | 处置 |
|---|---|---|
| builder Q1 / 工程 B-1 / 架构 B-3 | blob TTL vs FFI ABI | **收敛：零 ABI 宿主侧**（§3.4），开口项撤销 |
| 工程 B-2 / 架构 B-2 | 孤儿判死与内存 registry 矛盾 | 删除该条款；boot-tag not found + TTL 自 open 起算（§3.2/3.4） |
| 工程 B-3 / 架构 I-1 | 导出泵无消费者 / 饿死 JS | 同步开游标 + 自驱消费任务 + 节流（§3.3） |
| 工程 I-5 / 架构 B-1 | 保留路径入匹配集 = 自产假保险 | 匹配集只留 api 路由表（§1.2） |
| 工程 I-4 / 架构 I-4 | `{param}` 段两案分歧 | 采架构案：pattern 含 `{}` 直接拒（§1.2） |
| 产品 P-1 | REQ-01 不回答「慢 SQL 护栏」 | DB 阻塞语义如实成文 + 真慢语句用例 + REQ-05 仍为 GA 门禁（§1.1/1.3） |
| 产品 P-2/P-3 | 验收对照缺失 / 判死混同 | §3.5 对照表；重启 = not found（§3.2） |
| 产品 P-4 | fileName→Content-Disposition 断链 | §3.4 下载 header 口径（local 保真、s3 尽力） |
| 产品 P-5 | 两处 REQ 偏差未标注 | §3.2 偏差回执 + UI 口径 |
| 产品 P-6/P-7 | 审计缺位 / blobKey 日志 | §3.3-7 审计事件；blobKey bearer + 日志脱敏（§3.2） |
| 产品 P-8/P-9 | TTL 锚点 / 单主体配额 | open 起算 + live 豁免；每主体并发 2（§3.3/3.4） |
| 产品 P-10 | 滑期切分 / 撤回口径 | §0 切分线；发行说明差异①可撤、②维持 |
| 工程 I-6 / 架构 m-2 | 正则门禁脆 / golden flake | 腿②改 tsc 用法测试；golden 圈无条件全局（§2.2） |
| 工程 m-7~9 / 架构 m-1~4 | 回落硬顶 / 测试分层 / 流预算对账 / percent 记档 | 分别入 §3.3/§1.3/§3.3-6/§3.2 |

---

## 6. 新需求投递分析（REQ-04 / REQ-05，2026-10-29 第二批）

### 6.1 REQ-05 · 语句级超时 + 结果集字节上限（**GA 门禁，排 U-18 之前或同车**）

这与本设计 §1.1 如实成文的「REQ-01 管不住慢 SQL」是同一件事的正式立项——dbx 用 4s
`server.timeout` 实测：JS 忙循环 408 @4.03s，11.8s 慢 SQL **408 @11.81s 且语句 status=ok
跑完**。平台必须接住（dbx §4 的四条反驳全部成立：全局单值无档位、平台承诺不做 SQL 改写、
DBA 侧配置不可按查询纠正、无结构化错误）。

**设计判断（零 ABI 可达验收）**：
- **deadline 竞速（无 ABI）**：宿主侧对 accessor 调用包 `tokio::select!{ stmt, deadline }`
  ——db.query / db.stream / 构造器三条路径都过 accessor 层，单点收口；配置源
  `db_access.<alias>.statement_timeout_ms`（调用侧只能收紧，传大值拒并以
  `OJ_DB_TIMEOUT` 点名配置键——与 max_rows 上限语义同族）。
- **真取消不需要新 vtable 槽**：超时即**弃连接**（连接标记不健康、从池中 discard）——
  mysql 断连杀会话语句、pg 后端检到 client EOF 即 cancel，`pg_stat_activity` 查不到，
  恰好满足 REQ-05 §6 的验收语义；代价只是下一条查询重建连接。**db.session 上的语句超时
  = 会话判死**（sessionDisposition: kill，与 tx 超时回滚同先例，文档明写）。
- `max_result_bytes`：在**行拉取循环**内边拉边计（payload 原始字节，REQ §4.2-3 口径），
  `on_exceed: fail|truncate`（缺省 fail）+ 触顶显式信令；与 max_rows 并存、先到先生效。
  **导出（U-18）的 maxBytes 机制与此同一套**——REQ-05 先行交付，U-18 直接复用行级字节计费
  与「超时弃连/真取消」原语，避免两套闸。
- 结构化 `OJ_DB_TIMEOUT`（code/retryable/sessionDisposition/data.capability）。
- 验收照抄 REQ §6：60s SQL @30s 配置 → ~30s 结构化超时且服务端语句消失；三路径 ×
  三方言矩阵；1GiB 结果 × 64MiB 上限不 OOM。
- 待 builder 核查：插件轴（oj-db-mysql/pg）的 FfiFuture 能否被宿主 select 竞速（Send 界），
  以及弃连在插件池内的暴露面（可能需要插件侧一个非破坏性 helper——若必须动 vtable，
  升级为 ABI 13 单独成车，不阻塞宿主侧先交付 sqlite/直连通路）。

### 6.2 REQ-04 · listSchemas/listDatabases（U-17，中优先级，M3/M4 窗口）

- **一级清单：采纳**。落在 catalog 家族（kinds 增 `schemas` / `databases`，或独立方法——
  builder 按返回同构 `{name, owner?, isDefault?, comment?, tableCount?}` 裁决）；系统库过滤
  平台统一口径；`granted_only` 走既有内省授权维；sqlite 按能力矩阵抛
  `OJ_DB_UNSUPPORTED(capability=list_schemas)`。
- **二级 scope：采 B 的收窄版**——`db.forSchema(name)`（同连接维度：pg namespace /
  mysql database限定）返回内省受限视图（复用 asSystem/asTenant 派生视图先例）；
  **`forDatabase` 只做可见性不做穿越**：pg 跨 database = 不同连接（集群级目录可见 ≠ 可内省），
  穿越等价于新 DSN 别名，属配额语义问题，维持「一连接=一库」并写进产品文案——
  这正是 REQ §5.2-C 允许的答复，只是清单面补齐后 C 的代价大幅下降。
- 不阻塞 v0.1.102 批；窗口在 M3/M4 对象树定型前（REQ §7 的返工账成立）。

### 6.3 批次重排（v2 §0 发版行的增补）

```
v0.1.102  REQ-01 + REQ-02（独立可发，已定）
v0.1.103  REQ-05 语句级超时 + max_result_bytes（GA 门禁；U-18 的取消/字节原语先行）
v0.1.104  U-18 / REQ-03 任务式导出（复用 103 的原语）
M3/M4     REQ-04 / U-17 库-schema 层（清单 + forSchema 收窄版）
```
