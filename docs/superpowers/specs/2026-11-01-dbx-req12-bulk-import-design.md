# REQ-12 设计：批量导入原语（import / importStatus / importCancel）

> 上游诉求源：`../oj-dbx/docs/upstream/REQ-12-bulk-import-primitive.md`（§3.1-3.4 + §4
> 三硬约束 + §5 验收）。目标版本 v0.1.115（若与 REQ-11 合并发布则顺延）。纯 host 侧，
> **零 ABI**。复用面已勘察：`ExportRegistry` 的任务/配额/幂等/清扫模型可直接镜像。

## 1. 意图与为什么

导出三件套（export/exportStatus/exportCancel）完整，导入一件都没有：50 万行 CSV 只能
逐条 exec（50 万次 FFI 不可用）或自拼多值 INSERT（撞 max_allowed_packet / 65535 绑定
上限，且在 sql_guard 开启的租户方案里直接撞守卫跑不通）。**导出导不回 = 闭环缺半。**

## 2. 目标 / 非目标

**目标**

1. **G1 三件套**：`db.import(args) → {taskId}`（任务式，同 export 模型）+
   `importStatus`（state/rows/errors/bytes/samples）+ `importCancel`（幂等）。
2. **G2 方言实现**：PG COPY FROM STDIN（csv）为首选 + 多值 INSERT 备选/upsert 路径；
   MySQL 多值 INSERT（ON DUPLICATE KEY UPDATE 支持 upsert）为首选，
   LOAD DATA LOCAL INFILE 不做（托管库常禁）；sqlite 单事务 + 多值 INSERT。
3. **G3 失败行可定位**：`errors > 0` 时 `samples[]`（源行号 + 原因 + raw 样本），
   行号与源文件一一对应（非批次偏移）。
4. **G4 幂等口径**：upsert + conflictKeys 天然幂等；append 文档写明调用方自负；
   `rows` 语义 = 已提交行数，取消不回退。

**非目标**

- 从客户端直推文件流的「上传导入一体」通道（blobKey 须先经既有上传接口落 blob——
  复用面，不新开上传面）。
- xlsx/parquet 解析（format 只注册 csv/jsonl，与 export 同口径；新格式 = 格式注册表
  扩展位）。
- 跨库直搬（{查 A 写 B}）——用 import(rows) 内联小数据集 + export 组合可达成，
  不专开原语。
- 大对象列/自定义类型列的导入转换（只覆盖标量列；非常规列类型 → 该行进 errors
  样本，不猜）。

**反目标（= §4 三硬约束，写死）**：绕过 db_access op 层；绕过配额与审计；绕过租户
守卫。**任何一条不满足宁可不做该功能**（上游原文：「这条不写清，导入功能上线即
安全事故」）。

## 3. 契约（JS 可见面）

照抄上游 §3.1（逐字段采纳，不改名）：

```ts
import(args: {
  table: string; schema?: string;
  blobKey?: string;                       // 与 rows 二选一
  rows?: Record<string, unknown>[];
  format: "csv" | "jsonl";
  columns?: string[];                     // 缺省 = 表头/行键
  mode?: "append" | "upsert" | "replace"; // 缺省 append
  conflictKeys?: string[];
  batchSize?: number;                     // 实现按方言收敛
  header?: boolean;                       // csv
  onError?: "abort" | "skip";             // 缺省 abort
  maxErrors?: number;
}): Promise<{ taskId: string }>;

importStatus(taskId): Promise<{
  taskId, state: "running"|"done"|"failed"|"cancelled",
  alias, table, rows, errors, bytes,
  startedAtMs, finishedAtMs, error: string | null,
  samples?: { line: number; reason: string; raw?: string }[];
}>;

importCancel(taskId): Promise<{ cancelled: true }>;
```

## 4. 设计决策

### 4.1 任务模型（镜像 ExportRegistry）

- 新 `ImportRegistry`（`db_import.rs`）：entry {state, alias, table, rows, errors,
  bytes, samples(≤20), cancel: Arc<Notify>, cancel_requested, started/finished}。
- taskId 前缀 `imp_`（与 `exp_`/`exec_` 天然隔离——满足验收「exportStatus(importId)
  not found」）；bearer 语义/他人不可见/懒惰清扫/reaper 模式全部照搬 export。
- 配额 `db_import:` 段：`max_rows` / `max_bytes` / `global_concurrency` /
  `per_subject_concurrency`（与 `db_export:` 段对称；装配期冻结快照）。
- 执行载体：tokio task（current_thread 运行时用 `spawn_local`——与 export 消费任务
  同机制，JsRuntime !Send 约束不破）。

### 4.2 授权与守卫（三硬约束落法）

- **op 层复用（4.1）**：`op_db_import` 走与 `db.exec` 同门判定——`db_access` write
  判定 + `allow_ddl`（mode=replace 需 ddl 许可）+ 危险写确认 token（replace =
  先删后插，按 truncate 同档要求确认 token）。**不开旁路。**
- **租户守卫（4.3，最关键）——fail-closed 三档**：
  1. `sql_guard == Off`：放行（导入仍过 op 授权与配额）；
  2. `sql_guard != Off` 且目标表**未登记** ownership：放行（守卫本就不管未登记
     表——与裸 SQL 同口径）；
  3. `sql_guard != Off` 且目标表**已登记**：`OJ_DB_UNSUPPORTED(capability=
     "import_under_guard")`——**fail-closed，不静默放行**。
  长期方案（守卫认识 `db.table()` 构造器形状后放开 2→构造器路径）登记为未来工作。
  **不做**「逐行参数级校验」替代方案（成本高且与守卫的声明式模型分叉，上游推荐
  方案 3 即本决策）。
- 表名/列名标识符：只走 `quote_ident` + 白名单（**绝不**来自 CSV 表头原文——
  csv 表头只用于列序匹配，列名以 `columns` 参数或 SchemaRegistry 登记为准；
  表头文本与登记列名不符 → 参数错误 fail-fast，不猜）。

### 4.3 方言执行路径

| 方言 | 首选 | 模式适配 | 批次/事务 |
|---|---|---|---|
| PG | `COPY tbl(cols) FROM STDIN WITH (FORMAT csv, HEADER [false])`（`sqlx::postgres::PgCopyIn`，二进制协议，不走 SQL 文本扫描） | append/ replace（先 DELETE）直 COPY；**upsert = COPY 到临时表 + `INSERT ... SELECT ... ON CONFLICT (keys) DO UPDATE`**（COPY 不支持 ON CONFLICT，上游已点名） | 批内单事务；COPY 本身就是一个批量原语，batchSize 按 10k 行收敛内存 |
| MySQL | 多值 `INSERT INTO tbl(cols) VALUES (...),(...)`（构造器形态，守卫若未来放开认识此形状） | append 直插；upsert = `ON DUPLICATE KEY UPDATE`（**要求表上有唯一键**，缺 → 参数错误不猜）；replace 模式**不用** `REPLACE INTO`（隐式删+插，FK  cascade 语义不同）——显式 `DELETE FROM tbl` + 多值 INSERT，批外单事务 | batchSize 按 `max_allowed_packet` 的 1/4 与 65535/列数双收敛；批内单事务 |
| SQLite | 单事务 + 多值 INSERT | 同 MySQL（无 upsert 原语 → `INSERT OR REPLACE` 仅在 conflictKeys=rowid 时可用，否则参数错误） | 单事务包住全部批次是性能关键 |

- **abort 策略**：批内事务回滚 → 整体 failed，已提交前序批次保留（rows = 已提交）。
- **skip 策略**：单行失败（类型转换/约束）记 samples（line = 源文件绝对行号：
  blob 行号 = header 偏移 + 批内偏移 + 1），行不落库；`errors > maxErrors` →
  整体 failed。
- **csv 解析**：自写最小解析器（引号/转义/换行三规则），不引依赖；jsonl = 逐行
  `JSON.parse`。解析本身就是行号权威来源。

### 4.4 blob 源消费

- `blobKey` 源：经 blob 后端 `get` 流式读（大文件不整读内存——按 batchSize 行
  缓冲）；bytes 计数随读随记。
- `rows` 内联源：仅小数据集（`max_rows` 配额之内即可），行对象键序 = 列序（
  文档钉死：JS 对象键序稳定，但显式 `columns` 优先）。

### 4.5 安全与红线自查

- 值只走绑定参数（COPY 走二进制协议绑定等价物）——SQL 注入红线不破：拼进 SQL
  文本的只有 quote_ident 过的表/列名与引擎常量。
- `mode=replace` 是危险写：确认 token + allow_ddl + 租户守卫三档全过才执行。
- 导入任务与导出任务配额**互相独立**（不共享计数器），但同受各自段约束。

## 5. 验收映射（REQ §5 → 本设计）

| 上游验收 | 落点 |
|---|---|
| 50 万行 × 10 列 CSV < 60s（PG/MySQL） | env-gated 性能腿（生成 blob 上传 → import → rows=500000） |
| importCancel 中途取消 → cancelled + 已提交可查 + 未提交批不落库 | env-gated（MySQL SLEEP 内嵌慢批次 / PG pg_sleep 触发器？——用批间可取消点 + 慢批实测） |
| skip + maxErrors=100 超限整体 failed | 离线腿（坏行注入） |
| 失败样本行号与源文件一一对应 | 离线腿（header 偏移 + 批内偏移断言绝对行号） |
| upsert + conflictKeys 重跑两次行数不变 | env-gated 双腿 |
| sql_guard 开启已登记表 → 结构化拒绝 | 离线腿（guard fixture，钉 capability 名） |
| exportStatus(importId) not found | 离线腿（前缀隔离） |

## 6. 交付物清单

- `src/bridge/db_import.rs`：ImportRegistry + 三 op + 消费任务 + csv 最小解析器。
- `src/bridge/db_policy.rs`：`DbImportSection` 配额段 + 快照。
- `src/bridge/mod.rs` / `bootstrap.js`：op 注册 + `db.import/importStatus/importCancel`
  全局面（bootstrap 改动守 7-bit ASCII）。
- `src/bridge/guard.rs`：已登记表 + sql_guard 非 Off → import 前置 fail-closed 检查
  （复用 `extract_tables` 的登记表查询，不重写解析）。
- `serve/src` / `oj/src`：无改动（配额段解析在 db_policy，config 透传既有）。
- devkit 四件 + CHANGELOG v0.1.115 + 下游对账。
- 测试：离线单测（解析器/行号/配额/守卫三档/前缀隔离）+ env-gated 性能与取消腿。

## 7. 留给计划阶段的口子

- PG COPY 在 `AnyPool` 上的可用性——COPY FROM STDIN 需要 PgConnection 专有 API，
  Any driver 可能要走 `raw_sql` 的 `COPY` fallback（`COPY ... FROM '路径'` 不行——
  服务端文件权限；备选 = 多值 INSERT 全程）。**计划 Task 0 验证 PgCopyIn × AnyPool；
  不通则 PG 首选降为多值 INSERT + 临时表 upsert**（性能验收 60s 对 50 万行仍可过，
  多值 INSERT 单批 10k × 绑定参数是主流做法）。
- MySQL `max_allowed_packet` 运行期探测（`SHOW VARIABLES`）vs 配置快照——取保守
  缺省（16MB 的 1/4）+ 配置可覆盖。
