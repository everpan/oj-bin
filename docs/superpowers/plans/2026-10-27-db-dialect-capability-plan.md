# 数据库方言能力模型 实现计划（阶段一 M1 详案 + 阶段二/三大纲）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 将 oj-bin 数据库桥接从封闭 3 方言枚举演进为「开放方言名 + 能力位」契约（ABI 12），新方言接入零宿主逻辑分支，业务 JS API 与既有行为逐字节不变。

**Architecture:** 两层方言模型——`Dialect` 枚举保留为 builder-known 子集（既有 match 零改动），插件边界新增工厂级 `describe() -> {dialect, capabilities}`，装配期收敛为闭集 `DialectCaps` 结构体；FFI `Err(RString)` 载荷升级为结构化错误信封，宿主单点解析。六门控点对未知方言统一返回结构化 unsupported。架构遵循 SOLID（解析器/错误类型/门控各自单一职责；能力表对新增方言开放、对既有逻辑封闭）与 DRY（能力解析、错误解析、unsupported 构造、占位符漏斗各只此一处；有意例外：方言插件自包含重复，系设计已记录决策）。

**Tech Stack:** Rust（stabby C-ABI cdylib、deno_core、sea-query、sqlx）、既有 cargo test 体系。

**Spec:** `docs/superpowers/specs/2026-10-27-db-dialect-capability-design.md`（v2）

## Global Constraints（每个任务隐含遵守）

- 兼容 = 业务 JS API；**既有方言的 JS 错误消息文本逐字节不变**（快照测试守护）。
- ABI_VERSION 11 → 12，严格相等门禁；宿主+插件同产物重编发布。
- 安全语义零变化：SchemaRegistry / tenant guard / 占位符单漏斗保留。
- 占位符改写：触发条件换能力查表，**行为逐字节等价**，`placeholder.rs` 既有 24 条用例全量保留为守护。
- 能力位字符串常量（精确值）："placeholder_q"、"placeholder_dollar"、"placeholder_colon"、"id_returning"、"id_two_step"、"id_none"、"stream"、"cancel"。
- 未知 vendor 错误码 → `internal` + `retryable=false` + `data.vendor_code` 透传，禁止猜。
- 任何「未知方言回落 Sqlite」的路径必须删除。
- 真库测试 env-gated（`OJ_TEST_MYSQL`/`OJ_TEST_PG`）在 M1 起为必跑项；env 未设 = 不算绿。
- 任务均按 INVEST 分解：独立可交付、可谈判、有价值、可估、小、自带测试闭环；每任务以一次 commit 收尾。

## Review Focus（规格隐含但任务测试最易漏的五类输入）

1. **env 未设导致假绿**：真库套件默认静默跳过——期望：验收必跑、env 未设不算绿。→ T1 验证 + T15 终审清单项。
2. **既有错误文本回归**：业务 handler 在 catch 里匹配消息文本——期望：既有方言文本逐字节不变。→ T9 快照测试。
3. **`?` 特例被误改写**：jsonb `?` 运算符、`?` 后跟 `[|&<>=#~\-]`、dollar-quote——期望：与现行逐字节一致。→ T7 全量既有用例 + caps 驱动新用例。
4. **非法能力串/半拷贝混布被静默容忍**：typo 串、缺组、重复组、目录多出旧 .so——期望：fail-loud 且点名字符串/文件。→ T2/T5/T13。
5. **能力声明与运行时矛盾被吞**：声明了却 unsupported、未声明却出现——期望：按 §5.1 矩阵，不静默回退。→ T10 四象限用例。

## 阶段跟踪

| 阶段 | 状态 | 入口门禁 | 终审 | 小结 |
|---|---|---|---|---|
| 一 M1 契约与宿主 | ☐ 未开始 | 无 | T15 | T15 |
| 二 M2 验证方言 | ☐ 未开始 | 达梦 DPI 许可书面结论；Go 工具链进 xtask/CI 确认 | 阶段终审 | 阶段小结 |
| 三 M3 收尾 | ☐ 未开始 | M2 通过 | 阶段终审 | 阶段小结 |

任务级跟踪：每任务的 checkbox 即跟踪单元；执行者完成任务后同步本文件对应 checkbox 与「阶段跟踪」表状态。

---

# 阶段一：M1 契约与宿主（详案）

**验收条件（阶段级）：**
1. ABI 12 全链路（ffi 契约 + 三后端 + mock）一致；ABI 11 插件加载被拒且错误含双方 fingerprint 与处置指引。
2. env-gated 真库套件（`OJ_TEST_MYSQL=1` / `OJ_TEST_PG=1`）必跑全绿；compose 提供 mysql/pg 服务；默认 `cargo test` 全绿且无新增 skipped。
3. `placeholder.rs` 既有用例零修改全绿（行为逐字节等价证明）。
4. 既有方言 JS 错误文本快照零 diff。
5. 未知方言全路径（builder/exec_route/list_tables/nextSeq/search.*/CLI）返回结构化 unsupported 或 fail-loud，无 Sqlite 回落。
6. 能力矩阵 2×2 四象限用例绿。

### Task 1: 真库测试基座（compose mysql/pg + 必跑约定）

**Files:**
- Modify: `docker-compose.yml`（根目录；现仅 oj 演示服务）
- Modify: `docs/testing.md`（必跑约定小节）

**Interfaces:**
- Produces: 服务名 `test-mysql`(3306, root/root, db `ojtest`)、`test-postgres`(5432, postgres/postgres, db `ojtest`)；env 约定 `OJ_TEST_MYSQL=mysql://root:root@127.0.0.1:3306/ojtest`、`OJ_TEST_PG=postgres://postgres:postgres@127.0.0.1:5432/ojtest`（后续任务的测试步骤引用）。

- [ ] **Step 1: BDD 测试步骤（验证基座可用）**

```bash
docker compose up -d test-mysql test-postgres
OJ_TEST_MYSQL='mysql://root:root@127.0.0.1:3306/ojtest' cargo test -p oj-db-mysql
OJ_TEST_PG='postgres://postgres:postgres@127.0.0.1:5432/ojtest' cargo test -p oj-db-postgres
```

- Given env 已设，When 运行插件套件，Then 真库用例执行（输出无 `skipped` 计数的 db 用例）。
- Given env 未设，When 运行，Then 维持现状跳过（本任务不改插件代码）。

- [ ] **Step 2: 在 `docs/testing.md` 增写「M1 起真库套件必跑；env 未设 = 不算绿」与上述 env 约定**
- [ ] **Step 3: 运行上述命令确认 PASS**
- [ ] **Step 4: Commit** `test(db): compose 增补 mysql/pg 真库测试服务与必跑约定`

### Task 2: `dialect_caps.rs` —— 能力常量与 DialectCaps 闭集解析器

**Files:**
- Create: `src/bridge/dialect_caps.rs`（挂入 `src/bridge/mod.rs`）
- Test: `src/bridge/dialect_caps.rs` 内 `#[cfg(test)]`

**Interfaces:**
- Produces:
  - `pub enum PlaceholderStyle { Q, Dollar, Colon }`
  - `pub enum IdStrategy { Returning, TwoStep, None }`
  - `pub struct DialectCaps { pub placeholder: PlaceholderStyle, pub id: IdStrategy, pub stream: bool, pub cancel: bool }`
  - `pub struct DialectInfo { pub name: String, pub caps: DialectCaps }`
  - `pub enum CapsError { Unknown(String), MissingGroup(&'static str), DuplicateGroup(&'static str) }`（Display 含违规串/组名）
  - `pub fn caps_from_json(describe_json: &str) -> Result<DialectInfo, CapsError>` —— 解析 `{"dialect":"...","capabilities":["..."]}`；placeholder 组 3 选 1、id 组 3 选 1，缺/多/未知串 → `Err` 并点名。
- Consumes: 无（纯逻辑，最早期任务）。

- [ ] **Step 1: 写失败测试（BDD）**

```rust
// Given 能力集 ["placeholder_q","id_two_step","stream","cancel"]
// When caps_from_json
// Then Ok(info); info.name=="x"; placeholder==Q; id==TwoStep; stream&&cancel
// Given ["placeholder_q","placeholder_dollar","id_none"]   → Err(DuplicateGroup("placeholder"))
// Given ["id_none"]                                        → Err(MissingGroup("placeholder"))
// Given ["placeholder_q","id_none","strem"]（typo）          → Err(Unknown("strem")) 且 Display 含 "strem"
// Given dialect 缺失                                        → Err(MissingGroup("dialect"))
```

（组内占位/取 ID 至少一枚且至多一枚；`stream`/`cancel` 出现即 true，缺省 false，重复出现 → `DuplicateGroup`。）

- [ ] **Step 2: `cargo test -p oj --lib bridge::dialect_caps` → FAIL（模块不存在）**
- [ ] **Step 3: 实现常量表 + 解析器**（常量：`CAP_PLACEHOLDER_Q = "placeholder_q"` 等 8 个，单一来源供插件文档与宿主共用）
- [ ] **Step 4: `cargo test -p oj --lib bridge::dialect_caps` → PASS**
- [ ] **Step 5: Commit** `feat(db): DialectCaps 闭集解析器（互斥组 fail-loud）`

### Task 3: `db_error.rs` —— BridgeError 类型与信封解析

**Files:**
- Create: `src/bridge/db_error.rs`
- Test: 同文件 `#[cfg(test)]`

**Interfaces:**
- Produces:
  - `pub enum DbErrorCategory { Syntax, Constraint, Timeout, Connection, Capacity, Unsupported, Cancelled, Internal }`
  - `pub enum SessionDisposition { Keep, Reset, Close }`
  - `pub struct BridgeError { pub category: DbErrorCategory, pub retryable: bool, pub session_disposition: SessionDisposition, pub message: String, pub data: serde_json::Map<String, serde_json::Value> }`
  - `pub fn parse_raw(raw: &str) -> BridgeError` —— JSON 信封解析；任何解析失败/panic 文本/legacy 纯文本 → `{Internal, false, Keep, message: raw 原文}`；未知 category 串 → `Internal`；`data.vendor_code` 原样透传。
  - `pub fn unsupported(dialect: &str, capability: &str) -> BridgeError` —— DRY 单构造器，message 模板（§5.5）：`{capability} is not supported on dialect "{dialect}" yet. See docs/db-guide.md 方言能力矩阵.`，`data = {"capability": ..., "dialect": ...}`。
  - `pub fn code_of(category) -> &'static str` —— category→envelope code 固定映射（`unsupported→"OJ_DB_UNSUPPORTED"`，`timeout→"OJ_DB_TIMEOUT"`，`constraint→"OJ_DB_CONSTRAINT"`，`connection→"OJ_DB_CONNECTION"`，`cancelled→"OJ_DB_CANCELLED"`，`capacity→"OJ_DB_CAPACITY"`，`syntax→"OJ_DB_SYNTAX"`，`internal→"OJ_DB_INTERNAL"`）。
- Consumes: 无。

- [ ] **Step 1: 写失败测试（BDD）**

```rust
// Given 合法信封 JSON {"category":"constraint","retryable":false,"session_disposition":"keep","message":"dup","data":{"vendor_code":1062}}
// When parse_raw → category==Constraint; data["vendor_code"]==1062; message=="dup"
// Given "boom: something broke"（裸文本/panic）→ {Internal, false, Keep}, message 保留原文
// Given {"category":"weird",...} → Internal
// Given unsupported("oracle","query_build") → code_of()==OJ_DB_UNSUPPORTED; data.capability=="query_build"; message 含 dialect 名与 raw 路径指引
```

- [ ] **Step 2: `cargo test -p oj --lib bridge::db_error` → FAIL**
- [ ] **Step 3: 实现类型 + 解析 + 构造器**
- [ ] **Step 4: → PASS**
- [ ] **Step 5: Commit** `feat(db): BridgeError 结构化信封（单点解析 + unsupported 构造器）`

### Task 4: ABI 12 —— oj-plugin-ffi 契约升级

**Files:**
- Modify: `oj-plugin-ffi/src/lib.rs`（`ABI_VERSION` 11→12）
- Modify: `oj-plugin-ffi/src/db.rs`（`DataAccessorVtable` 增 `describe` 槽；`dialect` 槽注释降级为诊断；`stream_next` error 字段契约注释升级为信封结构）
- Modify: `plugins/oj-db-mysql/src/lib.rs`、`plugins/oj-db-postgres/src/lib.rs`（VTABLE 最小 describe 适配，使全树可编译；真实能力声明在 Task 11）
- Modify: `src/bridge/ffi.rs` mock vtable（对齐新布局）
- Test: `oj-plugin-ffi` 既有测试 + mock 编译

**Interfaces:**
- Produces: vtable 新槽 `pub describe: unsafe fn() -> RString`（返回 `caps_from_json` 可解析的 JSON；JSON 形状归 Task 2 的解析器所有）；`ABI_VERSION = 12`。
- Consumes: Task 2 的 JSON 形状约定。

- [ ] **Step 1: 失败测试**：Given ABI 12 宿主 When 加载 ABI 11 fixture 插件 Then 拒载，错误含双方 fingerprint 与「重新整体拷贝 bin/，禁止增量覆盖」（改造既有 AbiMismatch 用例断言文案）。
- [ ] **Step 2: 运行 → FAIL**（文案断言失败）
- [ ] **Step 3: 升级 ABI 常量、增 describe 槽、更新两插件与 mock 的 VTABLE**（mysql 临时 `{"dialect":"mysql","capabilities":["placeholder_q","id_two_step","stream","cancel"]}`；pg 对应 `placeholder_dollar/id_returning`；T11 完成映射表后复核）——setup 折叠进本任务以保持全树绿。
- [ ] **Step 4: `cargo test --workspace` → PASS**（含 mock 加载）
- [ ] **Step 5: Commit** `feat(ffi)!: ABI 12——DataAccessorVtable 增工厂级 describe 槽`

### Task 5: 装配期接入 —— 注册器消费 describe()，删除 Sqlite 回落

**Files:**
- Modify: `src/bridge/plugin_loader.rs`（装配期调 describe；CapsError → 拒载错误含违规串）
- Modify: `src/bridge/db_backend.rs`（`FfiDbBackend` 携带 `DialectInfo`）
- Modify: `src/bridge/ffi.rs:377-390`（`FfiDataAccessor::new` 改用工厂缓存描述符，删除闭集 match + Sqlite 兜底；connect 时校验 `describe().dialect == dialect(handle)`，不一致 fail-fast 拒连接）

**Interfaces:**
- Produces: `FfiDbBackend::dialect_info() -> &DialectInfo`；加载错误类型变体 `LoadError::BadCaps(CapsError)`。
- Consumes: Task 2 `caps_from_json`；Task 4 describe 槽。

- [ ] **Step 1: 失败测试（mock 插件三例）**
  - Given mock 声明含 typo 串 When 加载 Then 拒载且错误文本含 `strem`；
  - Given mock describe=`{"dialect":"oracle",...}` 且 `dialect(handle)` 返回 `"sqlite"` When connect Then 连接被拒（fail-fast）；
  - Given 合法 mock When 建立 accessor Then `dialect_info().name=="oracle"` 且**不发生** per-connect `dialect(handle)` vtable 调用（计数断言）。
- [ ] **Step 2: 运行 → FAIL**
- [ ] **Step 3: 实现**（一次 describe、装配期冻结进 `StableState.dbs`；无全局可变缓存）
- [ ] **Step 4: `cargo test -p oj --lib bridge` → PASS**
- [ ] **Step 5: Commit** `feat(db): 装配期 describe 收敛与连接级一致性校验`

### Task 6: DataAccessor 两层模型 —— dialect_info() 与 ActiveTx 全描述符快照

**Files:**
- Modify: `src/bridge/db.rs`（trait `DataAccessor` 增 `fn dialect_info(&self) -> &DialectInfo`；`ActiveTx` 增 `dialect_info: DialectInfo` 字段，begin 时快照完整描述符；`Dialect::known(name: &str) -> Option<Dialect>` 助手，仅映射 "mysql"|"postgres"|"sqlite"）
- Modify: `src/bridge/accessor_sqlx.rs`（SqlxAccessor 提供 describe 语义的 `DialectInfo`：sqlite = `placeholder_q`、`id_two_step`、`stream` 按现实现、`cancel` 按现实现——**镜像现状**，行为由既有测试锁定）
- Modify: `src/bridge/ffi.rs`（FfiDataAccessor 实现同 trait 方法，来自 Task 5 缓存）

**Interfaces:**
- Produces: `DataAccessor::dialect_info(&self) -> &DialectInfo`；`Dialect::known(&str) -> Option<Dialect>`；`ActiveTx.dialect_info`（Task 7/8 消费）。
- Consumes: Task 2/5/6 类型。

- [ ] **Step 1: 失败测试**：Given 任意 accessor When `dialect_info()` Then 与装配声明一致；Given tx begin When 后续 in-tx 改写/exec 路径 Then 使用 `ActiveTx.dialect_info`（而非再次查询）；Given `known("oracle")` Then `None`、`known("postgres")` Then `Some(Postgres)`。
- [ ] **Step 2: → FAIL**
- [ ] **Step 3: 实现**（sqlite 声明以现行 `exec_route`/stream/cancel 行为为准，逐项核对 accessor_sqlx 后定值）
- [ ] **Step 4: → PASS**
- [ ] **Step 5: Commit** `feat(db): DataAccessor.dialect_info 与 ActiveTx 全描述符快照`

### Task 7: 占位符触发条件切换（逐字节等价）

**Files:**
- Modify: `src/bridge/placeholder.rs:167-181`（门条件 `dialect == Postgres` → `caps.placeholder == PlaceholderStyle::Dollar`；空参短路保留）
- Test: 既有 `placeholder.rs` 用例（不改一行）+ 新增 caps 驱动用例

**Interfaces:**
- Consumes: Task 6 的 caps 来源（直连路径 + `ActiveTx` 快照路径）。
- Produces: 改写门无枚举依赖（后续方言仅经 caps 生效）。

- [ ] **Step 1: 失败测试（新增，BDD）**
  - Given caps=Q 且带参 When normalize Then 输出与输入逐字节相同（不改写）；
  - Given caps=Dollar 且 params 为空 When normalize Then 透传（jsonb `?` 规则不变）；
  - Given caps=Colon 且带参 When normalize Then 透传（`:n` 改写归插件内部，宿主不认得 colon）。
- [ ] **Step 2: 既有 24 条用例先行全绿确认 → 运行新旧全部 → 新增 FAIL**
- [ ] **Step 3: 改门条件**（仅此一处；扫描器不动）
- [ ] **Step 4: `cargo test -p oj placeholder` → 新旧全 PASS（既有用例零修改）**
- [ ] **Step 5: Commit** `refactor(db): 占位符改写门切换为能力查表（逐字节等价）`

### Task 8: 六门控点 —— 未知方言统一 unsupported / fail-loud

**Files:**
- Modify: `src/bridge/query.rs`（`op_db_query_build` 与 `exec_route` 入口：`Dialect::known(&info.name)` 为 `None` → `Err(BridgeError::unsupported(name,"query_build"|"exec_route"))`；已知方言逻辑零改动）
- Modify: `src/bridge/db.rs`（`op_db_list_tables`、`nextSeq` 全家入口同门控，capability 名 `"list_tables"`/`"next_seq"`）
- Modify: `oj/src/migrate.rs`、`oj/src/seed.rs`（未知方言 fail-loud，错误点名字言；复用 `unsupported()` 构造器渲染文本）

**Interfaces:**
- Consumes: `Dialect::known`（T6）、`BridgeError::unsupported`（T3）。
- Produces: 六门控点 + CLI 两处的统一未知方言行为。

- [ ] **Step 1: 失败测试（BDD，mock oracle accessor 注入各 op）**
  - Given 方言 "oracle" When `op_db_query_build` Then err.code==OJ_DB_UNSUPPORTED 且 data.capability=="query_build" 且 message 含「db.query/db.exec」指引；
  - Given "oracle" When `op_db_list_tables` / `nextSeq` Then 同构 unsupported（capability 分别为 list_tables/next_seq）；
  - Given "oracle" When `migrate` / `seed` CLI Then 进程 fail-loud 且 stderr 点名 oracle；
  - Given 已知三方言各一代表用例 When 上述各 op Then 行为与现状一致（既有测试零修改全绿）。
- [ ] **Step 2: → FAIL**
- [ ] **Step 3: 实现门控**（每处 ≤3 行：known 判断 + 一个构造器调用；DRY）
- [ ] **Step 4: `cargo test --workspace` → PASS**
- [ ] **Step 5: Commit** `feat(db): 未知方言六门控点统一结构化 unsupported`

### Task 9: JS 面 —— 错误码暴露、既有文本冻结、平台共享模块守卫

**Files:**
- Modify: `src/bridge/mod.rs`（op 层错误映射：`BridgeError` → `{code: code_of(category), msg: message, data}` 信封；既有错误路径文本零改动）
- Modify: `src/bridge/bootstrap.js`（JS Error 携带 `code` 与 `data` 字段，从信封透传）
- Modify: `sample/src/_platform/_shared/search.ts:11`、`versions.ts:62`（闭集 `Dialect` 判断改为「未知方言 → 抛 OJ_DB_UNSUPPORTED/`search`」，删除静默落 pg 分支）
- Test: `src/bridge/mod.rs` 信封映射单测 + 既有错误文本快照

**Interfaces:**
- Consumes: `code_of`（T3）、门控错误（T8）。
- Produces: JS 可判别 `e.code`/`e.data`（Task 10 与 M2 用例依赖）。

- [ ] **Step 1: 失败测试（BDD）**
  - Given T8 的 unsupported 错误 When 抵达 JS（经 mock handler）Then `e.code=="OJ_DB_UNSUPPORTED"` 且 `e.data.capability=="query_build"`；
  - Given 既有方言的既有错误路径（timeout/语法错各一，mysql/pg 真库）When 抵达 JS Then `msg` 文本与快照**逐字节相同**（快照文件本任务建立）；
  - Given `search.ts` 收到 `"oracle"` When ensureIndex/upsert/snippet Then 抛 code=OJ_DB_UNSUPPORTED 且 capability=="search"，不产生任何 SQL。
- [ ] **Step 2: → FAIL**
- [ ] **Step 3: 实现映射与守卫**（映射表一处；JS 侧只透传，无逻辑）
- [ ] **Step 4: `cargo test --workspace` + 真库快照用例 → PASS**
- [ ] **Step 5: Commit** `feat(db): JS 错误码面与既有文本冻结快照`

### Task 10: default-throw cancel 与能力一致性矩阵执行

**Files:**
- Modify: `src/bridge/db.rs`（`DataAccessor`/`TxSession` 增 `fn cancel(&self, ...)` 默认返回 `BridgeError::unsupported(name,"cancel")`）
- Modify: `src/bridge/ffi.rs`（哨兵翻译按矩阵：**已声明**+unsupported → op 级 unsupported 错误（不回退）；**未声明**+unsupported → 标准兜底（stream→fetch_all）；未声明+出现能力 → 采信 + warn 日志）
- Test: `src/bridge/ffi.rs` mock 用例（四象限）

**Interfaces:**
- Consumes: `DialectCaps`（T2）、mock vtable（T4/T5）。
- Produces: `cancel` 默认实现（M2 Oracle/DM 覆盖之）。

- [ ] **Step 1: 失败测试（BDD 四象限，mock 可切换 caps 与行为）**
  - Given caps 声明 stream 且 mock 返回 unsupported When stream_open Then op 报 unsupported（不回退 fetch_all）；
  - Given 未声明 stream 且 mock 返回 unsupported When stream_open Then 回退 fetch_all（现行为）；
  - Given 未声明 cancel 且 mock 实现了 cancel 槽为可用 When 调 cancel Then 成功 + warn 日志含能力名；
  - Given caps 声明 cancel 且 accessor 为默认实现 When cancel Then unsupported 错误。
- [ ] **Step 2: → FAIL**
- [ ] **Step 3: 实现矩阵**（单处 match；warn 一行）
- [ ] **Step 4: → PASS**
- [ ] **Step 5: Commit** `feat(db): 能力一致性矩阵与 default-throw cancel`

### Task 11: mysql/pg 插件迁移 —— 真实能力声明与错误映射表

**Files:**
- Modify: `plugins/oj-db-mysql/src/lib.rs`、`plugins/oj-db-postgres/src/lib.rs`（describe 终稿；新增 `fn map_sqlx_err(sqlx::Error) -> String`（信封 JSON，含 vendor_code：mysql 错误码 / pg SQLSTATE；未知码 → internal+透传）；`stream_next` error 字段产同构信封；查询/执行路径 Err 改产信封 JSON——**本任务起 Err 载荷恒为信封**）
- Modify: `src/bridge/ffi.rs`（await 出口 db 轴单点 `parse_raw`，删除 `ffi_err` 前缀包装）
- Test: 两插件真库套件（T1 的 env）+ mock

**Interfaces:**
- Consumes: Task 4 信封契约；Task 3 `parse_raw`。
- Produces: 全链路结构化错误（宿主拿到类型化 `BridgeError`，message 原文保留）。

- [ ] **Step 1: 失败测试（BDD，真库）**
  - Given OJ_TEST_MYSQL 且重复键插入 When exec Then 宿主收到 category==constraint、retryable==false、disposition==keep、`data.vendor_code==1062`；
  - Given pg When `SELECT 1/0` Then category 含语法/执行类正确映射且 vendor_code 为 SQLSTATE 串；
  - Given 任一插件 When 断连场景（容器 stop）Then category==connection、disposition==close；
  - Given 既有方言 JS 文本快照（T9）When 全量重跑 Then 零 diff。
- [ ] **Step 2: → FAIL**
- [ ] **Step 3: 实现映射表与信封产出**（每插件自包含一份 `map_sqlx_err`——DRY 有意例外，注释引用设计决策；单测覆盖 sqlx Error 各变体 → category 映射）
- [ ] **Step 4: 真库套件（必跑）+ workspace → PASS**
- [ ] **Step 5: Commit** `feat(db)!: mysql/pg 迁移 ABI 12——能力声明与结构化错误映射`

### Task 12: sql_trace 携带结构化字段

**Files:**
- Modify: `src/bridge/sql_trace.rs:71`（SqlEvent 增 `category: Option<String>`、`retryable: Option<bool>`）
- Modify: `src/bridge/db.rs`（三个裸 SQL op 插桩处传入 `BridgeError` 派生值；`_sql` 信封同扩）
- Test: sql_trace 单测

- [ ] **Step 1: 失败测试**：Given 注入 constraint 错误 When 查询完成 Then trace 行与 `_sql` 信封含 `"category":"constraint"` 与 `"retryable":false`；Given 成功查询 Then 两字段缺省（省字节）。
- [ ] **Step 2: → FAIL**
- [ ] **Step 3: 实现（纯透传，无逻辑）**
- [ ] **Step 4: → PASS**
- [ ] **Step 5: Commit** `feat(db): sql_trace 携带 category/retryable`

### Task 13: 清单锁装配、启动能力位一览、fingerprint 文案

**Files:**
- Modify: `xtask`（build 末尾产出 `bin/plugins/<triple>/plugins.lock.json`：`[{name, abi, fingerprint}]`）
- Modify: `src/bridge/plugin_loader.rs`（生产装配：lock 存在时，目录中多余 .so → 显式错误点名文件；`AbiMismatch` 错误体附双方 fingerprint + 处置指引「重新整体拷贝 bin/，禁止增量覆盖」）
- Modify: `oj/src/serve_cmd.rs`（启动横幅增 host ABI 行 + plugin×abi×caps 一览表；`plugins()` op 增 caps 列）
- Test: loader 单测 + serve 横幅单测

- [ ] **Step 1: 失败测试（BDD）**
  - Given lock 存在且目录多出 `oj-db-old.so` When 装配 Then 错误点名该文件；
  - Given ABI 不匹配 When 加载 Then 错误含双方 fingerprint 与拷贝指引；
  - Given 三插件 When 启动 Then 横幅含每插件 caps 串；Given `GET /v1/api/plugins` Then 每项含 caps。
- [ ] **Step 2: → FAIL**
- [ ] **Step 3: 实现**
- [ ] **Step 4: → PASS**
- [ ] **Step 5: Commit** `feat(db): 插件清单锁、启动能力位一览与 fingerprint 处置指引`

### Task 14: M1 终审与阶段小结

**Files:**
- Modify: 本计划文档（追加「阶段一小结」节）

- [ ] **Step 1: 阶段验收清单逐项执行**（上方 M1 验收条件 1–6 全绿；`cargo test --workspace` + `OJ_TEST_MYSQL=1 cargo test -p oj-db-mysql` + `OJ_TEST_PG=1 cargo test -p oj-db-postgres`）
- [ ] **Step 2: 派fresh reviewer 审查 M1 全量 diff**（对照设计 §5.1–5.3 与本计划；重点：门控点遗漏、文本回归、矩阵语义）
- [ ] **Step 3: 依据审查结论修复或记录豁免**
- [ ] **Step 4: 追加「阶段一小结」**（模板：完成项 / 偏差与豁免 / 度量（任务数、commit 数、测试增量）/ 风险与遗留 / 阶段二入口门禁状态），更新「阶段跟踪」表，commit

---

# 阶段二：M2 验证方言（大纲——详细计划于入口门禁通过后另立文档）

**入口门禁（未过不开工）：**
1. 达梦 DPI「可捆绑再分发」书面结论（拿不到 → 回方案评审，评估 JDBC 备选）；
2. Go 工具链进 xtask/CI 的 triple 矩阵确认（sidecar 为版本钉死同级文件，非嵌入抽取）。

**任务域（每个独立成详细计划）：**
- `oj-db-oracle`：Go sidecar（go-ora）+ 生命周期契约五条（自增 id 协议 / stderr-only / 读看门狗 / PDEATHSIG+reaper / 退避熔断）+ `?`→`:n` 插件内改写器（带对抗用例）+ sqlx 无关的 go-ora 错误映射表（未知码 → internal+vendor_code）+ stream_cancel（声明 `cancel`，覆盖 default-throw）。
- `oj-db-dameng`：DPI FFI + 同上错误映射 + 占位符/取 ID 实测结论回填设计 §7 清单。
- 共同验收：raw 路径 + 分页（FETCH FIRST/ROWNUM，参照 dbx oceanbase-oracle/dameng）+ 取 ID + cancel + 类型映射表（设计 §5.6）+ 值映射约定（超精度十进制串/ISO 8601/base64）+ 矩阵四象限 + sql_trace 错误路径各一条 + sidecar `kill` 用例（daemon/容器）。
- 阶段终审 + 小结：同 Task 14 协议。

# 阶段三：M3 收尾（大纲）

**任务域：** `plugin-development.md`「新增数据库方言」章节（能力位注册表、错误映射骨架含 Oracle/DM 样例行）；`tools/plugin-template` db 轴骨架 + 方言 conformance checklist（每能力位一正一反 + unsupported 一致性）；`db-guide.md` 方言矩阵（含 §5.6 类型映射表与 JS 错误码速查）；`db.dialect()` 红字与迁移范式；ops-manual §1/§6/§7/§8 增补（发布物、告警映射、`plugin abi mismatch`/`fingerprint mismatch` 排障行、回滚三元组）。
**验收：** 文档交叉引用全部可达；模板按 checklist 自检通过；ops 表与实际错误文本一致。
**终审 + 小结：** 同 Task 14 协议。

---

## Self-Review 记录

- **Spec 覆盖**：设计 §5.1→T2/T3/T4/T5/T10/T12；§5.2→T6/T7/T8；§5.3→T10；§5.5→T8/T9；§8-M1 全项→T1/T13/T14；§5.4/§5.6→阶段二/三（入口门禁阻塞，非缺口）。
- **类型一致性**：`DialectInfo`/`DialectCaps`/`BridgeError`/`code_of`/`unsupported`/`known`/`dialect_info` 跨任务签名一致；能力串常量唯一来源 T2。
- **Review Focus**：五类输入均落在 owning task 的测试步骤（见各 BDD 条目）。
- **比例**：计划以签名/断言/命令为主，无实现体转写。
