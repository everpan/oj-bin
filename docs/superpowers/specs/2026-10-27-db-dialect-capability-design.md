# 数据库方言能力模型设计（统一方言契约 + 能力协商）

- 日期：2026-10-27（v2，依据五方专家评审 + builder-check 修订，见 §9 评审记录）
- 状态：评审修订稿，待定稿
- 参照项目：`/Users/ever/git/only-js/dbx`（dbx 多数据库 agent 架构）
- 验收方言：Oracle、达梦（DM8）

## 1. 意图与动机

oj-bin 目前的数据库桥接支持 sqlite / memory / mysql / postgres 四种后端。方言契约存在多处封闭点，
导致「新增一个数据库方言」必须修改宿主核心，违背插件自注册设计：

1. 宿主侧 `Dialect` 是封闭 3 值枚举（`src/bridge/db.rs:32-46`，未知一律按 Sqlite），派生出多条按方言硬编码的路径：
   - sea-query builder 后端选择（`src/bridge/query.rs:2470-2472`）；
   - `?`→`$n` 占位符改写（`src/bridge/placeholder.rs:12,167-181`，仅对 Postgres 且带参触发，单漏斗 `normalize_sql`，`db.rs:261` 调用）；
   - `exec_route` 取自增 ID 策略（`query.rs:2212-2260`，mysql 两步 `LAST_INSERT_ID` vs pg `RETURNING`）；
   - `op_db_list_tables` 的目录 SQL（`db.rs:866-883`，三方言穷尽 match）；
   - `nextSeq` 全家（`db.rs:1006-1040`：`ensure_seq_table` DDL、`insert_sql` 的 `on conflict...returning`（非 mysql 一律落 pg 分支）、`seq_next` 两步）；
   - 插件路径的「未知落 Sqlite」兜底（`src/bridge/ffi.rs:382-388`）；内置路径的 `dialect_of`（`db.rs:39-47`、`accessor_sqlx.rs:43`）。
2. FFI 契约 `DataAccessorVtable`（`oj-plugin-ffi/src/db.rs`）的 `dialect(handle)` 是闭集字符串。
3. 错误跨 FFI 边界后宿主无法结构化处置：主 query/exec 路径的 Err 通道是裸 `RString`
   （`oj-plugin-ffi/src/future.rs:22-26`），JSON 形态的错误信封仅存在于 `stream_next` 的 Ok 载荷内。

dbx 项目（Java 多数据库客户端）用四个机制支撑了 54 个数据库驱动：default-throw（实现即声明能力）、
握手能力字符串协商、结构化错误契约（category/retryable/session_disposition）、声明式 profile + 共享基础设施。
本设计把可移植的机制移植到 oj-bin 的 cdylib 插件形态中，使方言生态可系统化扩展。

## 2. 目标 / 非目标 / 反目标

**目标**
- 方言契约从闭集演进为「开放方言名 + 能力位」；新方言接入时宿主侧仅剩声明式收尾，无逻辑分支新增。
- 业务 SQL raw 路径（`db.query/exec/stream`）对新方言一期可用；宿主平台原语（builder、`listTables`、`nextSeq`、`search.*`）对新方言一期显式返回结构化 unsupported（不裸炸、不静默走错分支）。
- 结构化错误跨 FFI：宿主可按 category/retryable/session_disposition 处置；JS 侧获得可编程判别的错误码。
- 业务 JS API 对既有方言完全不变（`db.query/exec/tx/stream/dialect()`、`db.table()...` 签名与语义，含错误消息文本）。
- 热路径零新增开销：能力在装配/connect 时获取一次并收敛为闭集结构体，此后纯查表。

**非目标**
- 不抽共享方言 crate：mysql/pg 插件保持自包含（`plugins/oj-db-mysql/src/lib.rs:6-9` 已记录该决策）。
- 不覆盖非 SQL 轴：redis/ES/Kafka 等已有独立 axis 契约，不动。
- 不引入宿主级连接池、不引入进程外 JSON-RPC agent 模型（那是 dbx 的形态）。
- 不做 dbx 式握手版本协商（`*_v1` 能力串）：宿主与插件同产物发布 + ABI 严格门禁已保证同版本，能力协商无协商对象（见 §5.1 演进策略）。
- 通用 statement-cancel ABI：一期取消能力即 ABI-10 的 `stream_cancel`（见 §5.3）。
- sea-query builder 对新方言的支持为二期（见 §5.5）。
- 自动重试/连接重置策略：一期只交付编码、映射表与宿主透出，处置策略后移为独立里程碑（见 §7）。

**反目标（出现即算失败）**
- 新旧两套方言契约长期双轨并存。
- 每次查询发生能力协商或新增间接层。
- 任何依赖错误消息文本匹配的处置逻辑（宿主侧）。
- 任何「未知方言静默回落 Sqlite 语义」的路径。

## 3. 约束

- ABI：`oj-plugin-ffi` 的 `ABI_VERSION` 11 → 12，严格相等门禁（双道：符号 + descriptor，`plugin_loader.rs:428-433`）。宿主与插件由 `cargo xtask build` 同版本一次性产出、整体拷贝部署——ABI 破坏可接受。
- `AbiMismatch` 错误体附双方 `HOST_FINGERPRINT` / plugin fingerprint，文案统一附处置指引「重新整体拷贝 bin/，禁止增量覆盖」；升级窗口用现成的 `GET {base}/plugins`（`host_abi_version` 字段）核对。
- 兼容定义（用户确认）：**业务侧 JS API**。插件 ABI 不在兼容承诺内。既有方言的 JS 错误消息文本逐字节冻结（业务 handler 可能在 catch 里匹配文本）；新增错误条件（unsupported 等）以新增稳定错误码暴露，不影响旧文本。
- 安全语义零变化：`SchemaRegistry` 白名单（`src/bridge/registry.rs`）、tenant guard（`src/bridge/guard.rs`）、占位符 `normalize_sql` 单漏斗，全部保留；占位符改写仅「触发条件从枚举比较换成能力查表」，行为逐字节等价。
- 回归安全网：见 §8 M1 验收——env-gated 真库测试列为必跑，「env 未设 = 不算绿」。
- 回滚单元 = `bin/`（含 plugins）+ `dist/` + `config.yaml` 三者同版本；新方言 DSN 投产后，包回滚只能回到最近一个支持该 scheme 的版本，更早版本回滚须先摘除新方言 DSN（ops-manual §8 增补，M3）。

## 4. 现状要点（设计依据）

- 流式查询已有能力协商雏形：ABI-10 的 `stream_open/stream_next/stream_cancel/stream_close`，无流式能力的后端返回 `{"unsupported":true}`，宿主回退 fetch_all（`ffi.rs:439-457`）。本设计将其推广为标准的能力缺失哨兵，但语义收紧（见 §5.3 矩阵）。
- mysql/pg 两插件 ~90% 结构重复，属既有决策（「插件自包含哲学优先于 DRY」），本次不合并。
- 路由为 scheme 声明制：工厂 `schemes() -> RVec<RString>`，scheme 交集冲突 fail-fast（`db_backend.rs:45-53`）；未知 DSN scheme 启动 fail-fast。新方言无需宿主注册。
- 真库测试为 env-gated：`OJ_TEST_MYSQL` / `OJ_TEST_PG` 未设即静默跳过（`plugins/oj-db-mysql/src/lib.rs:1368` 起）；根 compose 无 mysql/pg 服务；e2e 全部 `sqlite::memory:`。默认 `cargo test` 全绿 ≠ 方言路径被覆盖。
- 宿主每连接快照方言：`FfiDataAccessor::new` connect 时调 `dialect(handle)` 缓存（`ffi.rs:377-390`）；tx 在 begin 时快照进 `ActiveTx`（`db.rs` op_db_tx_begin）。`in_tx` 路径的改写/exec_route 用的是该快照。
- JS 面信封为 `{code,msg,data}`（`src/bridge/envelope.rs`）；sql_trace 的 SqlEvent 仅记 `error: Option<String>`（`sql_trace.rs:71`）。
- dbx 侧参照物：
  - `agents/common/.../DatabaseAgent.java`：default-throw 能力面（约 40 个 plDebug* 默认抛 `UnsupportedOperationException`，仅 oceanbase-oracle 覆盖）；
  - `agents/common/.../AgentProtocol.java`：握手能力字符串（`multi_session`、`structured_error_v1`、`blob_bind_statements_v1`…）；
  - `crates/dbx-driver-agent/src/backend_error.rs`：结构化错误契约 `{category, retryable, session_disposition, contract_version}`；
  - `agents/drivers/oceanbase-oracle`（24,312 行）与 `agents/drivers/dameng`（8,715 行，explain/autotrace）：Oracle 系与达梦的方言 SQL 知识库（分页、元数据目录 SQL、取 ID、错误映射）。

## 5. 设计

### 5.1 契约演进（oj-plugin-ffi，ABI 12）

**describe()（工厂级，权威来源）**

- vtable 层新增 `describe() -> {dialect: RString, capabilities: RVec<RString>}`，替代 `dialect()` 的闭集语义。
- 能力位首批（宿主 `src/bridge/dialect_caps.rs` 持有常量表与解析器；插件侧只产出字符串）：
  - `placeholder_q` / `placeholder_dollar` / `placeholder_colon` —— 占位符风格（互斥组，3 选 1）；
  - `id_returning` / `id_two_step` / `id_none` —— builder/exec_route 取自增 ID 策略（互斥组，3 选 1）；
  - `stream` —— 支持流式游标；
  - `cancel` —— 支持 `stream_cancel`。
- **装配期收敛为闭集结构体**：`describe()` 在插件加载/装配时解析一次为
  `DialectCaps { placeholder: PlaceholderStyle, id: IdStrategy, stream: bool, cancel: bool }`。
  互斥组缺项/多项/未知串 → **拒载并报出具体串名**（fail-loud，杜绝「typo 插件把裸 `?` 发给 Oracle 运行期才炸」）。
  禁止按方言名做全局可变缓存（描述符随 `StableState.dbs` 装配期冻结，插件不 dlclose，无跨请求失效问题）。
- **一致性矩阵**（能力声明 vs 运行时哨兵）：

  | | 运行时支持 | 运行时 `{"unsupported":true}` |
  |---|---|---|
  | **已声明** | 正常 | op 级结构化错误（category=unsupported，fail loud，不回退） |
  | **未声明** | 采信 + warn（前向兼容） | 标准兜底（如 stream → fetch_all） |

- 连接级 `dialect(handle)` 降级为诊断接口；connect 时校验 `describe().dialect == dialect(handle)`，不一致 fail-fast 拒绝该连接（与 scheme 冲突 fail-fast 同风格）。
- **演进策略**：新增能力位 = bump ABI（12→13…）。能力开放性面向「新方言」，不面向「同一 ABI 内的新能力」——这是同产物发布 + 严格门禁的自然推论，刻意区别于 dbx 的 `*_v1` 握手版本化命名。

**结构化错误信封（通道裁决）**

- 载荷：db 轴 vtable 方法的 `Err(RString)` 载荷**升级为 UTF-8 JSON 信封**：
  `{ "category": "...", "retryable": bool, "session_disposition": "keep|reset|close", "message": "...", "data": {...} }`
  宿主在 `await_ffi` 出口单点解析；解析失败（含 panic 收敛文本、legacy 纯文本）一律归
  `{category:"internal", retryable:false, session_disposition:"keep", message:<raw>}`。
  db 轴 adapter 不再做 `ffi_err` 前缀包装；`message` 原文保留给 trace/JS。
- category 集合：`syntax | constraint | timeout | connection | capacity | unsupported | cancelled | internal`。
  `cancelled` 独立存在（ABI-10 已有 `{"error":"cancelled"}`），禁止把用户主动取消计成 timeout（告警噪声）。
- `data.vendor_code` 强制透传（ORA-xxxxx / 达梦错误码）；**未知码一律 `internal` + `retryable=false` + vendor_code 透传，禁止猜**。
- 运维映射表（随契约发布，ops-manual §6/§7 消费）：category → {owner: app|infra|config, 建议告警级别}
  （syntax/constraint/unsupported/cancelled→app；connection/capacity→infra；timeout→按 data 细分；internal→infra）。
- `stream_next` 信封的 error 字段升级为同一信封结构（流式查询的超时/断连处置同样需要它）。
- **职责切分**：`session_disposition` 的执行者是**插件自己**（映射错误时顺手把坏连接逐出自己的池，再返回信封——宿主只有一个 `close(handle)` 杠杆，没有「重置池中一条连接」原语，禁止用 close 当 reset 造成全池惊群）。宿主侧该字段只驱动两个决策：`retryable` → 重试策略（后移里程碑）；`session_disposition=reset|close` → 放弃重试、回滚并丢弃 TxSession / 流游标，不复用。
- **JS 可见面**：既有方言错误消息文本冻结；新错误条件（如 builder unsupported）以稳定 `code`/`category` 暴露
  （如 `OJ_DB_UNSUPPORTED` + `data.capability` + `data.dialect`），宿主信封字段一比一映射；给出 category→envelope code 固定映射表。
- `sql_trace` 的 SqlEvent 增记 `category`/`retryable`（进日志与 `_sql` 信封），否则结构化错误进不了告警管道。

### 5.2 宿主：两层方言模型（评审修订，替代 v1 的「枚举替换为描述符」）

**保留 `Dialect` 枚举作为 builder-known 子集**：`Dialect::` 在宿主 src 54 处、oj CLI 79 处、query_tests 38 处的穷尽 match / `==` **零改动**，编译期穷尽性不损失。枚举即「sea-query/migrate/seed 已知方言」的封闭集合，与 §5.5 的 builder 边界自洽——v1 的「去封闭化」按字面实施是 ~170 处机械重写且自我矛盾（builder 只认三个已知名，封闭集删不掉）。

**插件边界新增描述符**：`DataAccessor::dialect_info() -> {name, caps}`（工厂描述符缓存，装配期一次）；`ActiveTx` 与 `FfiDataAccessor` 两处快照均存**完整描述符**（仅存名字则 tx 路径的能力化落空）。

**门控点清单**（此前称「三处硬编码」，评审补全为六处 + 两个接入点）：

| 门控点 | 未知方言行为 |
|---|---|
| builder 后端选择（query.rs） | 结构化 unsupported（§5.5） |
| `exec_route`（query.rs） | 结构化 unsupported |
| 占位符改写触发条件（placeholder.rs:167-181） | 见 §5.1 占位符归属 |
| `op_db_list_tables`（db.rs:866-883） | 结构化 unsupported |
| `nextSeq` 全家（db.rs:1006-1040） | 结构化 unsupported |
| JS 平台模块 `_shared/search.ts` / `versions.ts` 的闭集 `Dialect` | 新方言 → 结构化 unsupported（禁止静默落 pg 分支执行 FTS DDL） |
| `accessor_sqlx.rs::dialect_of` / `FfiDataAccessor::new`（内置/插件两接入点） | 改接描述符；**删除一切未知→Sqlite 回落** |

- `oj/src/migrate.rs`（`dialect_tag` 文件名）与 `seed.rs`（`translate_insert`）：CLI 侧对未知方言 fail-loud。
- 一插件 = 一方言，装配期约束；「一个插件按 DSN 变方言」（schemes 多前缀）如确需，二期改为 describe() 按 scheme 返回，本期明确不支持。

**占位符归属（评审钉死，防实施者提前重构扫描器）**：
- M1：触发条件由 `dialect == Postgres` 换成 caps 查表，**行为逐字节等价**；`placeholder.rs` 既有 24 条对抗性用例全量保留为守护（空参透传 / `?` 后跟 `[|&<>=#~\-]` 不改写 / dollar-quote 跳过）。
- M2：`?`→`:n` 改写**归 oracle/dameng 插件内部实现**（各自带对抗用例）；宿主漏斗对 `placeholder_colon` 方言透传（同 mysql）；`placeholder_colon` 位仅声明/诊断。
- 无任何 `placeholder_*` 位 = 装配期拒载（互斥组缺项），不存在「未定义默认」。

### 5.3 default-throw（实现即声明能力）

- 宿主 trait `DataAccessor` / `TxSession` 新增可选方法带默认抛结构化 Unsupported；FFI 适配层把 `{"unsupported":true}` 哨兵按 §5.1 矩阵翻译。
- 一期可选方法：`cancel`（= `stream_cancel` 能力镜像；**不引入**通用 statement-cancel ABI——宿主今天没有对裸 `query/exec` 的语句身份簿记，dbx 的 oracle-go/dameng 亦无服务端取消参照，Oracle 需第二条控制连接，留二期设计）。
- 一致性校验：声明了却 unsupported → 报错（非 warn，见矩阵）；未声明却出现 → 采信 + warn。每个能力位一正一反守护测试。

### 5.4 驱动适配槽（新方言如何接入）

插件自包含，可包任意形态驱动，契约不假设 sqlx：

| 方言 | 驱动路径（推荐 → 备选） | 方言 SQL 知识参照 |
|---|---|---|
| Oracle | Go sidecar（go-ora，纯 Go 无 CGO，静态二进制；dbx `agents/drivers/oracle-go` 53,855 行为现成参照） | dbx `agents/drivers/oceanbase-oracle`（分页 FETCH FIRST/ROWNUM、元数据目录 SQL、取 ID、错误映射） |
| 达梦 DM8 | DPI C API 经 Rust FFI（需 DM 客户端库） → JDBC（需 JRE，不推荐） | dbx `agents/drivers/dameng`（分页、explain/autotrace、元数据 SQL） |

**sidecar 生命周期契约**（M2 验收项，写死）：
1. 请求/响应带自增 id，一请求一响应；sidecar 禁止向 stdout 写任何非协议输出（日志只走 stderr，由插件桥接进 tracing）——防止单帧超管道缓冲（64KB）或日志行永久阻塞请求。
2. 读看门狗（可配，默认 < server.timeout）：超时 = kill 进程组 + 映射 `category=timeout`、`session_disposition=reset`。
3. 启动用进程组 + Linux prctl `PDEATHSIG`（父死子亡）；插件内异步 reaper 负责 waitpid（distroless 以 PID 1 直跑、无 init 收尸——这是部署前提，不是理论边角）。
4. 崩溃 = 该连接全部会话按 `category=connection`、`session_disposition=close` 处置；重启用指数退避 + 连续失败熔断。
5. `--daemon`（setsid 物理机形态）与容器两种形态各一条验收用例：`kill <pid>` 后 sidecar 必须消失。

**打包与分发矩阵**：
- sidecar 以**版本钉死的同级文件**随 `bin/plugins/<triple>/` 发布（不做嵌入抽取）；Go 工具链进入 xtask/CI/发布矩阵 = M2 入口项（需确认目标 triple 集合）。
- 达梦 DPI 客户端库：**许可「可捆绑再分发」的书面结论为 M2 入口门禁**——拿不到当场回方案评审（备选 JDBC 路径重新评估），不允许做到一半。默认形态为 feature-gated 插件（不进默认 `xtask build` 产物，独立镜像变体）；许可允许捆绑后可再评估常规布局。Dockerfile 门禁（glibc 基线自检、strip 循环）扩展覆盖 vendor .so（不 strip vendor 库）；dlopen 解析用 rpath `$ORIGIN`。
- ops-manual §1 发布物清单与 §7 排障表增补对应条目（缺 libdmdpi.so 的报错形态、dlopen 失败处置）。

**值映射约定**（跨方言 JSON 值契约，进 conformance 清单，防静默精度丢失）：
超 f64/i64 安全精度的 NUMERIC/NUMBER → 十进制字符串；DATE/TIMESTAMP → ISO 8601 字符串；BLOB/CLOB → base64 / 字符串（方言文档明示）。既有插件的有损选择（mysql 日期转字符串等）维持现状，不回溯。

**部分升级防御（发布完整性）**：
- `xtask build` 产出 `bin/plugins/<triple>/` 清单锁（角色同 dist/manifests.yaml），生产装配按清单进行——目录中多出的旧 .so 成为显式错误；
- 启动日志打印 plugin × abi × describe() 能力位一览表（`plugins()` op 数据源扩一列 caps）；
- ABI 12 内的 cfg 演进（向后兼容字段）导致 fingerprint mismatch 时，warn 文案须点名「半拷贝混布会让新能力表现为 unsupported，先核对发布完整性」，避免把部署错误误诊为方言缺陷。

### 5.5 一期边界（builder / 平台原语 / 工具链）

sea-query 仅有 mysql/postgres/sqlite 后端。新方言一期：

| 面 | 行为 |
|---|---|
| raw 路径 `db.query/exec/stream` | 全量可用（SchemaRegistry + tenant guard + 占位符漏斗 + 值映射约定生效） |
| builder `db.table()...` | `op_db_query_build` 返回结构化 unsupported（`code=OJ_DB_UNSUPPORTED`，消息模板：点名方言 + 缺失能力 + 指引「用 db.query/db.exec，`?` 占位符会被自动改写」+ 文档链接），业务显式回退 raw |
| `db.listTables` / `nextSeq` / `_shared/search.*` | 结构化 unsupported（禁止裸 SQL 语法错、禁止静默落错分支） |
| migrate / seed / reconcile CLI | fail-loud（点名方言不支持） |
| 二期 | per-dialect「目录/序列 profile」槽（`next_seq_sql` / `list_tables_sql` / `ensure_seq_ddl` 随 describe() 上报，对应 dbx dameng/oceanbase 的元数据 SQL 知识）；sea-query 自定义引用风格可行性评估 |

### 5.6 schema.yaml 类型映射（评审新增）

六类型集（integer|bigint|text|boolean|double|blob）对各新方言给出**发布级映射表** + 读侧 JS 类型契约，例（Oracle）：
integer→NUMBER(38)、bigint→NUMBER(20)、boolean→NUMBER(1)+CHECK、text→VARCHAR2(4000)/CLOB 约定、double→BINARY_DOUBLE、blob→BLOB。

- 映射表进 M2 插件与 M3 db-guide 方言矩阵；读侧数值契约（NUMBER 超精度 → 十进制字符串，何时报错）随值映射约定发布。
- **装配期 fail-fast**：新方言下 schema.yaml 声明了无映射的类型 → 点名类型与方言拒载，禁止沿用现状「Unknown 静默兜底 text」（那会让 tenant guard 把 `tenant_id` 当文本绑定，静默劣化）。

## 6. 已定决策

用户确认：

| 决策 | 结论 |
|---|---|
| 演进方式 | 方案 A：原地演进统一契约，mysql/pg 一起迁移，无双轨 |
| 兼容范围 | 业务 JS API；插件 ABI 可破（同产物重编发布） |
| 验证方言 | Oracle + 达梦 |
| trait 范围 | 仅 `db` 轴 SQL 方言；非 SQL axis 不动 |
| builder 边界 | 一期新方言 raw-only（含平台原语，见 §5.5） |
| 共享 crate | 不抽，插件自包含哲学不变 |

评审修订中由评审员拍板、无需重开的决策（依据见 §9）：

| 决策 | 结论 | 来源 |
|---|---|---|
| Dialect 演进方式 | 两层模型：枚举保留为 known 子集 + 边界描述符 | 工程评审② |
| 错误载荷通道 | Err(RString) 载荷升级为 JSON 信封（非迁入 Ok 字节），单点解析，失败归 internal | 架构③ + 代码③（2:1 于工程④的 Ok 方案） |
| 能力协商 | 无握手版本化；新能力 = bump ABI；装配期闭集收敛 + 一致性矩阵 | 架构② + 代码② |
| cancel 范围 | 一期 = stream_cancel 镜像；不做通用 statement-cancel ABI | builder Q3 |
| 能力的时间维度 | describe() 工厂级静态（最坏公约数）；按 scheme/服务版本细化留二期 | builder Q4 + 代码① |
| 占位符归属 | `?`→`:n` 归插件内部；宿主漏斗对 colon 透传 | 工程⑤ + 代码④ |
| 达梦分发 | 默认 feature-gated + 独立镜像变体；许可结论为 M2 入口门禁 | 运维③ |
| sidecar 打包 | 版本钉死同级文件，不嵌入抽取 | 运维① + builder Q5 |
| 重试/重置策略 | 一期只交付编码/映射/透出，处置策略独立里程碑 | 工程④ |

## 7. 留给实现者

- `describe()` 的 FFI 槽位与 `DialectCaps` 解析器的具体实现；互斥组拒载的错误文案。
- mysql/pg 的 `sqlx::Error → {category, retryable, session_disposition, vendor_code}` 映射表（每插件一份，真实工作量）。
- 达梦 DPI 验证清单（M2）：客户端库获取与**许可书面结论（入口门禁）**、占位符（`?` vs `:n`）实测、取 ID、取消语义、目录 SQL——对照 dbx dameng agent 直接抄答案。
- Oracle sidecar 协议实现（JSON line + 自增 id，契约见 §5.4）；go-ora 的第二条控制连接问题不在一期范围。
- JS 侧 `OJ_DB_UNSUPPORTED` 的 bootstrap.js 抛出形态与 docs 错误速查表收录；`db.dialect()` 文档加红字「新方言名会出现在返回值，禁止 `else` 视作 sqlite/mysql」并给出「统一 `?` + listTables 替代分支」迁移范式。
- 测试矩阵：现有集成测试迁移 + **env-gated 真库套件必跑**（§8）+ 每能力位一正一反 + §5.1 一致性矩阵 2×2 四象限用例 + unsupported 信封 → JS code 可判别用例 + sql_trace 携带 category/retryable 用例（Oracle/达梦各一条错误路径）+ sidecar `kill` 验收用例（daemon/容器）。

## 8. 里程碑

- **M1 契约与宿主**：ABI 12（describe + 能力位闭集收敛 + 结构化错误信封）、两层方言模型（枚举保留 + 边界描述符 + 六门控点 + 删 Sqlite 回落）、default-throw、mysql/pg/sqlite 迁移、占位符触发条件切换（逐字节等价）、sql_trace 扩展、清单锁装配 + 启动能力位一览。
  **验收（评审重写）**：(a) env-gated 真库套件（`OJ_TEST_MYSQL`/`OJ_TEST_PG`）列为必跑项，compose 增补 mysql/pg 服务，env 未设 = 不算绿；(b) mock vtable 测试同步补 describe 槽位；(c) 既有方言 JS 错误文本逐字节不变。
- **M2 验证方言**：入口门禁——达梦 DPI 许可书面结论、Go 工具链进 xtask/CI 确认；然后 `oj-db-oracle`（sidecar 生命周期契约全项验收）与 `oj-db-dameng` 接入：raw 路径 + 分页 + 取 ID + stream_cancel + 类型映射表 + 值映射约定 + 错误映射表走通，能力位与实际行为一致（矩阵四象限用例）。
- **M3 收尾**：`plugin-development.md` 新增「新增数据库方言」章节（能力位注册表文档、错误映射表骨架含 Oracle/DM 样例行）、`tools/plugin-template` db 轴骨架 + 方言一致性 conformance checklist、`db-guide.md` 方言矩阵（含类型映射表）、ops-manual §1/§6/§7/§8 增补（发布物、告警映射、排障行、回滚三元组）。

## 9. 评审记录（2026-10-27）

| 评审方 | 结论 | 关键修订 |
|---|---|---|
| 架构师 | 调整后通过 | 平台原语（nextSeq/listTables）封闭点补全；能力位闭集收敛 + 无握手协商；错误通道钉死；session_disposition 执行者=插件；describe 工厂级必需 |
| 工程师 | 调整后通过 | 两层方言模型（~170 处重写→边界新增）；M1 验收改为真库必跑；占位符归属钉死；错误载体裁决 |
| 代码专家 | 调整后通过（事实核对全过） | describe/连接级一致性校验；ActiveTx 快照存全描述符；能力矛盾 2×2 矩阵；Err 通道实为裸 RString 的失实修正；无位方言 fail-loud；fingerprint 进错误体 |
| 产品（DX） | 调整后通过 | schema.yaml 类型映射表 + 装配期 fail-fast；JS 错误码面；builder unsupported 消息模板；db.dialect() 红字；M3 第三方作者三件套 |
| 运维 | 调整后通过 | sidecar 生命周期契约五条；cancelled/vendor_code/owner 映射；达梦分发矩阵 + 许可入口门禁；清单锁装配；回滚三元组 |
| builder-check | 6 问全部已有答案或由评审拍板 | 确认文档代码事实断言全部属实；主错误通道为裸 RString；listTables 为第四封闭点；cancel/能力时间维度/sidecar 打包/值映射裁决并入上文 |

无「必须用户拍板」的遗留问题：6 份评审的所有发现均已由既有用户决策或代码证据消解，落为上表第二组决策。
