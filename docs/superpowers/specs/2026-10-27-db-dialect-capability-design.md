# 数据库方言能力模型设计（统一方言契约 + 能力协商）

- 日期：2026-10-27
- 状态：待评审
- 参照项目：`/Users/ever/git/only-js/dbx`（dbx 多数据库 agent 架构）
- 验收方言：Oracle、达梦（DM8）

## 1. 意图与动机

oj-bin 目前的数据库桥接支持 sqlite / memory / mysql / postgres 四种后端。方言契约存在三处封闭点，
导致「新增一个数据库方言」必须修改宿主核心，违背插件自注册设计：

1. 宿主侧 `Dialect` 是封闭 3 值枚举（`src/bridge/db.rs`，未知一律按 Sqlite），被三处硬编码依赖：
   - sea-query builder 后端选择（`src/bridge/query.rs`）；
   - `?`→`$n` 占位符改写（`src/bridge/placeholder.rs`，仅对 Postgres 触发，单漏斗 `normalize_sql`，`db.rs:261` 调用）；
   - `exec_route` 的取自增 ID 策略（mysql 两步 `LAST_INSERT_ID` vs pg `RETURNING`）。
2. FFI 契约 `DataAccessorVtable`（`oj-plugin-ffi/src/db.rs`）的 `dialect(handle) -> "mysql"|"postgres"|"sqlite"` 是闭集字符串。
3. 错误跨 FFI 边界后宿主无法结构化处置（重试？断连？回滚？），只能靠约定。

dbx 项目（Java 多数据库客户端）用四个机制支撑了 54 个数据库驱动：default-throw（实现即声明能力）、
握手能力字符串协商、结构化错误契约（category/retryable/session_disposition）、声明式 profile + 共享基础设施。
本设计把这四个机制移植到 oj-bin 的 cdylib 插件形态中，使方言生态可系统化扩展。

## 2. 目标 / 非目标 / 反目标

**目标**
- 方言契约从封闭枚举演进为「开放方言名 + 能力位」，新方言（raw 路径）接入零宿主修改。
- 结构化错误跨 FFI：宿主可按 category/retryable/session_disposition 处置连接与事务。
- 业务 JS API 完全不变（`db.query/exec/tx/stream/dialect()`、`db.table()...` 等签名与语义）。
- 热路径零新增开销：能力在 connect 时获取一次，此后纯查表。

**非目标**
- 不抽共享方言 crate：mysql/pg 插件保持自包含（`plugins/oj-db-mysql/src/lib.rs:6-8` 已记录该决策，自包含哲学优先于 DRY）。
- 不覆盖非 SQL 轴：redis/ES/Kafka 等已有独立 axis 契约（oj-bin 的 axis 体系即 dbx「家族方法集」的对应物，已做对，不动）。
- 不引入宿主级连接池、不引入进程外 JSON-RPC agent 模型（那是 dbx 的形态，不是 oj-bin 的）。
- sea-query builder 路径对新方言的支持为二期（见 §5.5）。

**反目标（出现即算失败）**
- 新旧两套方言契约长期双轨并存。
- 每次查询都发生能力协商或新增间接层。
- 任何依赖错误消息字符串匹配的处置逻辑。

## 3. 约束

- ABI：`oj-plugin-ffi` 的 `ABI_VERSION` 11 → 12，严格相等门禁。宿主与插件由 `cargo xtask build`
  同版本一次性产出、整体拷贝部署——ABI 破坏可接受（重编即发布流程的一部分）。
- 兼容定义（用户确认）：**业务侧 JS API**。插件 ABI 不在兼容承诺内。
- 安全语义零变化：`SchemaRegistry` 白名单（`src/bridge/registry.rs`）、tenant guard（`src/bridge/guard.rs`）、
  占位符 `normalize_sql` 单漏斗，全部保留，仅从枚举分支改为能力查表。
- 回归安全网：现有 mysql/pg/sqlite 集成测试全量复用，M1 期间必须全绿。

## 4. 现状要点（设计依据）

- 流式查询已有能力协商雏形：ABI-10 的 `stream_open/stream_next/stream_cancel/stream_close`，无流式能力的后端
  返回 `{"unsupported":true}`，宿主回退 fetch_all。本设计将其推广为标准的能力缺失信封。
- mysql/pg 两插件 ~90% 结构重复（含 Dialect enum、bind/row 转换、runtime/pool），属既有决策，本次不合并。
- 路由为 scheme 声明制：工厂 `schemes() -> RVec<RString>`，scheme 交集冲突 fail-fast（`src/bridge/db_backend.rs`）。
  新方言无需宿主注册，只要声明 `["oracle://"]` 等即可被 DSN 前缀路由。
- dbx 侧参照物：
  - `agents/common/.../DatabaseAgent.java`：default-throw 能力面（约 40 个 plDebug* 默认抛 `UnsupportedOperationException`，仅 oceanbase-oracle 覆盖）；
  - `agents/common/.../AgentProtocol.java`：握手能力字符串（`multi_session`、`structured_error_v1`、`blob_bind_statements_v1`…）；
  - `crates/dbx-driver-agent/src/backend_error.rs`：结构化错误契约 `{category, retryable, session_disposition, contract_version}`；
  - `agents/drivers/oceanbase-oracle`（24.3k 行）与 `agents/drivers/dameng`（8.7k 行，explain/autotrace）：Oracle 系与达梦的方言 SQL 知识库（分页、元数据、取 ID）。

## 5. 设计

### 5.1 契约演进（oj-plugin-ffi，ABI 12）

工厂级（vtable 层）新增/替换：

- `describe() -> {dialect: RString, capabilities: RVec<RString>}`，替代现有 `dialect()` 闭集字符串。
- 能力位首批（字符串常量，宿主侧有对应常量表）：
  - `placeholder_q` / `placeholder_dollar` / `placeholder_colon` —— 占位符风格（sqlite/mysql=`?`，pg=`$n`，oracle=`:n`）；
  - `id_returning` / `id_two_step` / `id_none` —— builder/exec_route 的取自增 ID 策略；
  - `stream` —— 支持流式游标（与 ABI-10 行为对齐，改为显式声明，`{"unsupported":true}` 回退保留为兜底）;
  - `cancel` —— 支持语句级取消（mysql `KILL QUERY` / pg 独立连接取消，见 `docs/api/06-db.md`）。
- 连接级 `dialect(handle)` 保留为诊断接口，返回方言字符串；权威来源是工厂 `describe()`。

结构化错误信封（跨 FFI 的 error 载荷从裸消息升级）：

```json
{ "category": "syntax|constraint|timeout|connection|capacity|unsupported|internal",
  "retryable": false,
  "session_disposition": "keep|reset|close",
  "message": "...", "data": { } }
```

字段语义照搬 dbx `structured_error_v1`。宿主据此决定重试、连接重置、事务处置，禁止字符串匹配。

### 5.2 宿主去封闭化

- `Dialect` 从封闭枚举改为描述符 `{name: String, caps: DialectCaps}`；`dialect_of` 返回描述符，未知方言不再强制落 Sqlite。
- 三处硬编码改查表：
  - 占位符改写：`normalize_sql` 按 `placeholder_*` 能力位驱动，仍是单漏斗、纯查表，无分支开销差异；
  - `exec_route`：按 `id_*` 能力位驱动；
  - sea-query 后端选择：仅对 `mysql/postgres/sqlite` 三个已知名映射 builder 后端（见 §5.5）。
- 能力常量表与解析集中在宿主一侧新文件（如 `src/bridge/dialect_caps.rs`），插件侧只产出字符串。

### 5.3 default-throw（实现即声明能力）

- 宿主 trait `DataAccessor` / `TxSession`（`src/bridge/db.rs`）新增可选方法（首批：`cancel`、`server_info`），
  默认实现返回 `Unsupported` 结构化错误。
- FFI 适配层（`src/bridge/ffi.rs`）把插件返回的 `{"unsupported":true}` 信封翻译为该结构化错误；
  插件「覆盖实现」即声明能力，与 5.1 的能力位互为印证（适配层可校验一致性：声明了 `cancel` 却 unsupported → 记 warn）。

### 5.4 驱动适配槽（新方言如何接入）

插件自包含，可包任意形态驱动，契约不假设 sqlx：

| 方言 | 驱动路径（推荐 → 备选） | 方言 SQL 知识参照 |
|---|---|---|
| Oracle | Go sidecar（go-ora，纯 Go 无 CGO，静态二进制；dbx `agents/drivers/oracle-go` 53.9k 行为现成参照） → rust-oracle（OCI，需 Oracle Instant Client，违反零依赖哲学，不推荐） | dbx `agents/drivers/oceanbase-oracle`（分页 FETCH FIRST/ROWNUM、元数据、错误映射） |
| 达梦 DM8 | DPI C API 经 Rust FFI（需 DM 客户端库，与 OCI 同类取舍，**待验证**：DPI 头文件/许可/占位符风格/取 ID 方式） → JDBC（需 JRE，太重，不推荐） | dbx `agents/drivers/dameng`（分页、explain/autotrace、元数据 SQL） |

sidecar/FFI 生命周期管理（子进程或库的加载与清理）由插件内部实现，宿主只见 `DataAccessorVtable`。
注：达梦高度兼容 Oracle 语法，两个验证方言同源；这有利于快速验证，但也意味着抽象验证对「异构方言」的覆盖不足，
后续接 SQL Server（tiberius 纯 Rust 异步）或 ClickHouse 时才真正检验异构性。

### 5.5 builder 路径一期边界

sea-query 仅有 mysql/postgres/sqlite 后端。新方言一期：
- raw 路径（`db.query/exec/stream`）：全量可用，SchemaRegistry + tenant guard + 占位符漏斗全部生效；
- builder 路径（`db.table()...`）：`op_db_query_build` 对未知方言返回结构化 `unsupported`（category=`unsupported`），
  业务可回退 raw 路径；
- 二期评估 sea-query 自定义标识符引用风格的可行性（**待验证**，不阻塞 M1/M2）；
- 迁移/seed 工具（`oj/src/migrate*.rs`）同 builder 边界，一期不承诺新方言。

## 6. 已定决策（用户确认）

| 决策 | 结论 |
|---|---|
| 演进方式 | 方案 A：原地演进统一契约，mysql/pg 一起迁移，无双轨 |
| 兼容范围 | 业务 JS API；插件 ABI 可破（同产物重编发布） |
| 验证方言 | Oracle + 达梦 |
| trait 范围 | 仅 `db` 轴 SQL 方言；非 SQL axis 不动 |
| builder 边界 | 一期新方言 raw-only |
| 共享 crate | 不抽，插件自包含哲学不变 |

## 7. 留给实现者

- `describe()` 的 FFI 签名细节（工厂级 vtable 槽位、capabilities 用 `RVec<RString>` 还是紧凑结构体）。
- 结构化错误在 `FfiFuture` 载荷中的编码位置（复用现有 JSON error 通道的字段扩展即可，不新开通道）。
- 达梦 DPI 验证清单：客户端库获取与许可、占位符（`?` vs `:n`）、取 ID（SCOPE_IDENTITY / 输出参数）、
  取消语义、元数据查询（对照 dbx dameng agent 的实现直接抄答案）。
- Oracle sidecar 协议（进程内 JSON line 即可，参照 dbx agent 的 request/response 形状裁剪）。
- 测试矩阵：现有集成测试迁移 + 每个能力位至少一个正/反用例 + unsupported 信封一致性校验。

## 8. 里程碑

- **M1 契约与宿主**：ABI 12（describe + 能力位 + 结构化错误）、`Dialect` 去封闭化、default-throw、
  mysql/pg/sqlite 内置插件迁移完毕、现有集成测试全绿。不含新方言。
- **M2 验证方言**：`oj-db-oracle`（Go sidecar）与 `oj-db-dameng`（DPI）接入，raw 路径 + 分页 + 取 ID + 取消走通，
  能力位与实际行为一致。
- **M3 收尾**：`docs/plugins/plugin-development.md` 增补「新增数据库方言」章节、`tools/plugin-template` 更新为
  db 轴骨架、`docs/db-guide.md` 方言矩阵更新。
