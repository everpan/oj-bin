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
| 一 M1 契约与宿主 | ✅ 完成（T1–T14；改动待用户提交） | 无 | 已终审（四路 + 逐条处置） | 见文末「阶段一小结」 |
| 二 M2 验证方言 | ☐ 未开始 | 达梦 DPI 许可书面结论；Go 工具链进 xtask/CI 确认（**均未取得 → 门禁未过，不开工**） | 阶段终审 | 阶段小结 |
| 三 M3 收尾 | ◐ 无库切片已落地（3/5 域；其余待 M2） | M2 通过（**未满足**） | 阶段终审（未启动） | 见「阶段三·无库切片」 |

> **阶段三为何有「◐」**：M3 的 5 个任务域里，有 3 个不依赖真库与外部许可（方言接入章节 /
> db 轴模板 / 方言矩阵与类型映射）。用户 2026-10-27 决策：先做这 3 个可本地验证的部分，
> 剩余 2 个（`db.dialect()` 迁移范式、ops-manual §1/§6/§7/§8 增补）仍随 M3 整体等 M2。
> **M3 本身未启动、未终审**——这里不改变阶段门禁，只登记已完成的无库工作。

> **进度登记（本工作区，改动尚未由用户提交）：** T1 真库基座 / T2 `dialect_caps` / T3 `db_error`
> / T4 ABI 12 / T5 装配期 describe / T6 DataAccessor.dialect_info / T7 占位符查表 / T8 六门控点
> / T9 JS 错误码面 均已在工作树落地（未 commit）。T10 default-throw cancel + 能力一致性矩阵 已实现（四象限 mock 用例全绿）。
> T11 结构化错误信封 已实现：两插件各持 `map_sqlx_err`（mysql 取原生 errno 数字、pg 取 SQLSTATE 串原样透传），`connect` 失败亦归一为信封；真库用例（`real_*_envelope_classifies_*`）经本机 `container` 起的 mysql:8.4 / postgres:16-alpine 跑通（vendor_code 1062 / "22012"、category/retryable/disposition 断言全绿），另加 `sqlx_category_maps_io_to_connection_and_close` 纯单测钉死 Io→connection/close。
> T12 sql_trace 结构化字段 已实现：`SqlEvent` 增 `category`/`retryable`，`trace_sql` 透传，`snapshot()` 仅在有值时出现（省字节）；`db.rs` 增 `err_meta` 在 `to_js_err` 前从 `BridgeResult` 抽取 `BridgeError` 的 category/retryable，接到三个裸 SQL op；单测钉死「有值携带 / 成功缺省」。
> T13 清单锁/横幅 已实现：`xtask build` 产出 `bin/plugins/<triple>/plugins.lock.json`（真实 descriptor 的 name/abi/fingerprint）；扫描装配在 lock 存在时对未登记 `.so` 点名拒载（端到端实测：塞入 `liboj-db-old.dylib` → `oj serve` 报 `plugin file not in plugins.lock.json: … (lock 要求整体拷贝 bin/，禁止增量覆盖)`）；`AbiMismatch` 文案补双方 fingerprint + 拷贝指引；启动横幅出 host ABI 行与每插件 `abi/fp/caps`，`PluginInfo` 增第七键 `caps` 使 `plugins()` / `GET {base}/plugins` 同源。
> 任务级 step checkbox 见各任务内各步，T1–T14 全部已打 `x`。14 个任务的代码合并为一次提交
> `b89f793`（用户授权后整体提交，未按任务拆分）；计划/设计文档随后单独提交。

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

- [x] **Step 1: BDD 测试步骤（验证基座可用）**（本机无 docker，改用 Apple `container` 起 `mysql:8.4`@192.168.64.2 与 `postgres:16-alpine`@192.168.64.3；env 直指 VM IP。两套件真跑非跳过，各 15/15——见「阶段一小结·度量」）

```bash
docker compose up -d test-mysql test-postgres
OJ_TEST_MYSQL='mysql://root:root@127.0.0.1:3306/ojtest' cargo test -p oj-db-mysql
OJ_TEST_PG='postgres://postgres:postgres@127.0.0.1:5432/ojtest' cargo test -p oj-db-postgres
```

- Given env 已设，When 运行插件套件，Then 真库用例执行（输出无 `skipped` 计数的 db 用例）。
- Given env 未设，When 运行，Then 维持现状跳过（本任务不改插件代码）。

- [x] **Step 2: 在 `docs/testing.md` 增写「M1 起真库套件必跑；env 未设 = 不算绿」与上述 env 约定**（已落地：`docs/testing.md`「真库套件（env-gated，M1 起必跑）」节 + `docker-compose.yml` 增 `test-mysql`/`test-postgres`）
- [x] **Step 3: 运行上述命令确认 PASS**（mysql 15/15、pg 15/15）
- [x] **Step 4: Commit** `test(db): compose 增补 mysql/pg 真库测试服务与必跑约定`（已落地：`b89f793`，与后续任务合并为一次提交）

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

- [x] **Step 1: 写失败测试（BDD）**

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

- [x] **Step 2: `cargo test -p oj --lib bridge::dialect_caps` → FAIL（模块不存在）**
- [x] **Step 3: 实现常量表 + 解析器**（常量：`CAP_PLACEHOLDER_Q = "placeholder_q"` 等 8 个，单一来源供插件文档与宿主共用；T14 追加 `CapsError::BadJson` 收口非数组 `capabilities`）
- [x] **Step 4: `cargo test -p oj --lib bridge::dialect_caps` → PASS**（10 例，含缺组/重组/typo/坏 JSON）
- [x] **Step 5: Commit** `feat(db): DialectCaps 闭集解析器（互斥组 fail-loud）`（已落地：`b89f793`，与相邻任务合并为一次提交）

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

- [x] **Step 1: 写失败测试（BDD）**

```rust
// Given 合法信封 JSON {"category":"constraint","retryable":false,"session_disposition":"keep","message":"dup","data":{"vendor_code":1062}}
// When parse_raw → category==Constraint; data["vendor_code"]==1062; message=="dup"
// Given "boom: something broke"（裸文本/panic）→ {Internal, false, Keep}, message 保留原文
// Given {"category":"weird",...} → Internal
// Given unsupported("oracle","query_build") → code_of()==OJ_DB_UNSUPPORTED; data.capability=="query_build"; message 含 dialect 名与 raw 路径指引
```

- [x] **Step 2: `cargo test -p oj --lib bridge::db_error` → FAIL**
- [x] **Step 3: 实现类型 + 解析 + 构造器**
- [x] **Step 4: → PASS**（含 `bridge_error_maps_to_js_error_with_code_and_data`，T9 起另带 `retryable`/`sessionDisposition`）
- [x] **Step 5: Commit** `feat(db): BridgeError 结构化信封（单点解析 + unsupported 构造器）`（已落地：`b89f793`，与相邻任务合并为一次提交）

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

- [x] **Step 1: 失败测试**：Given ABI 12 宿主 When 加载 ABI 11 fixture 插件 Then 拒载，错误含双方 fingerprint 与「重新整体拷贝 bin/，禁止增量覆盖」（改造既有 AbiMismatch 用例断言文案）。
- [x] **Step 2: 运行 → FAIL**（文案断言失败）
- [x] **Step 3: 升级 ABI 常量、增 describe 槽、更新两插件与 mock 的 VTABLE**（mysql 临时 `{"dialect":"mysql","capabilities":["placeholder_q","id_two_step","stream","cancel"]}`；pg 对应 `placeholder_dollar/id_returning`；T11 完成映射表后复核）——setup 折叠进本任务以保持全树绿。
- [x] **Step 4: `cargo test --workspace` → PASS**（含 mock 加载）
- [x] **Step 5: Commit** `feat(ffi)!: ABI 12——DataAccessorVtable 增工厂级 describe 槽`（已落地：`b89f793`，与相邻任务合并为一次提交）

### Task 5: 装配期接入 —— 注册器消费 describe()，删除 Sqlite 回落

**Files:**
- Modify: `src/bridge/plugin_loader.rs`（装配期调 describe；CapsError → 拒载错误含违规串）
- Modify: `src/bridge/db_backend.rs`（`FfiDbBackend` 携带 `DialectInfo`）
- Modify: `src/bridge/ffi.rs:377-390`（`FfiDataAccessor::new` 改用工厂缓存描述符，删除闭集 match + Sqlite 兜底；connect 时校验 `describe().dialect == dialect(handle)`，不一致 fail-fast 拒连接）

**Interfaces:**
- Produces: `FfiDbBackend::dialect_info() -> &DialectInfo`；加载错误类型变体 `LoadError::BadCaps(CapsError)`。
- Consumes: Task 2 `caps_from_json`；Task 4 describe 槽。

- [x] **Step 1: 失败测试（mock 插件三例）**
  - Given mock 声明含 typo 串 When 加载 Then 拒载且错误文本含 `strem`；
  - Given mock describe=`{"dialect":"oracle",...}` 且 `dialect(handle)` 返回 `"sqlite"` When connect Then 连接被拒（fail-fast）；
  - Given 合法 mock When 建立 accessor Then `dialect_info().name=="oracle"` 且**不发生** per-connect `dialect(handle)` vtable 调用（计数断言）。
- [x] **Step 2: 运行 → FAIL**
- [x] **Step 3: 实现**（一次 describe、装配期冻结进 `StableState.dbs`；无全局可变缓存）
- [x] **Step 4: `cargo test -p oj --lib bridge` → PASS**
- [x] **Step 5: Commit** `feat(db): 装配期 describe 收敛与连接级一致性校验`（已落地：`b89f793`，与相邻任务合并为一次提交）

### Task 6: DataAccessor 两层模型 —— dialect_info() 与 ActiveTx 全描述符快照

**Files:**
- Modify: `src/bridge/db.rs`（trait `DataAccessor` 增 `fn dialect_info(&self) -> &DialectInfo`；`ActiveTx` 增 `dialect_info: DialectInfo` 字段，begin 时快照完整描述符；`Dialect::known(name: &str) -> Option<Dialect>` 助手，仅映射 "mysql"|"postgres"|"sqlite"）
- Modify: `src/bridge/accessor_sqlx.rs`（SqlxAccessor 提供 describe 语义的 `DialectInfo`：sqlite = `placeholder_q`、`id_two_step`、`stream` 按现实现、`cancel` 按现实现——**镜像现状**，行为由既有测试锁定）
- Modify: `src/bridge/ffi.rs`（FfiDataAccessor 实现同 trait 方法，来自 Task 5 缓存）

**Interfaces:**
- Produces: `DataAccessor::dialect_info(&self) -> &DialectInfo`；`Dialect::known(&str) -> Option<Dialect>`；`ActiveTx.dialect_info`（Task 7/8 消费）。
- Consumes: Task 2/5/6 类型。

- [x] **Step 1: 失败测试**：Given 任意 accessor When `dialect_info()` Then 与装配声明一致；Given tx begin When 后续 in-tx 改写/exec 路径 Then 使用 `ActiveTx.dialect_info`（而非再次查询）；Given `known("oracle")` Then `None`、`known("postgres")` Then `Some(Postgres)`。
- [x] **Step 2: → FAIL**
- [x] **Step 3: 实现**（sqlite 声明以现行 `exec_route`/stream/cancel 行为为准，逐项核对 accessor_sqlx 后定值）
- [x] **Step 4: → PASS**
- [x] **Step 5: Commit** `feat(db): DataAccessor.dialect_info 与 ActiveTx 全描述符快照`（已落地：`b89f793`，与相邻任务合并为一次提交）

### Task 7: 占位符触发条件切换（逐字节等价）

**Files:**
- Modify: `src/bridge/placeholder.rs:167-181`（门条件 `dialect == Postgres` → `caps.placeholder == PlaceholderStyle::Dollar`；空参短路保留）
- Test: 既有 `placeholder.rs` 用例（不改一行）+ 新增 caps 驱动用例

**Interfaces:**
- Consumes: Task 6 的 caps 来源（直连路径 + `ActiveTx` 快照路径）。
- Produces: 改写门无枚举依赖（后续方言仅经 caps 生效）。

- [x] **Step 1: 失败测试（新增，BDD）**
  - Given caps=Q 且带参 When normalize Then 输出与输入逐字节相同（不改写）；
  - Given caps=Dollar 且 params 为空 When normalize Then 透传（jsonb `?` 规则不变）；
  - Given caps=Colon 且带参 When normalize Then 透传（`:n` 改写归插件内部，宿主不认得 colon）。
- [x] **Step 2: 既有 24 条用例先行全绿确认 → 运行新旧全部 → 新增 FAIL**
- [x] **Step 3: 改门条件**（仅此一处；扫描器不动）
- [x] **Step 4: `cargo test -p oj placeholder` → 新旧全 PASS**（**用例语义零改动**，但调用点签名随门控改为 `PlaceholderStyle`，`t25`/`t26` 随之改名——见「阶段一小结·偏差 7」；断言与数据未变）
- [x] **Step 5: Commit** `refactor(db): 占位符改写门切换为能力查表（逐字节等价）`（已落地：`b89f793`，与相邻任务合并为一次提交）

### Task 8: 六门控点 —— 未知方言统一 unsupported / fail-loud

**Files:**
- Modify: `src/bridge/query.rs`（`op_db_query_build` 与 `exec_route` 入口：`Dialect::known(&info.name)` 为 `None` → `Err(BridgeError::unsupported(name,"query_build"|"exec_route"))`；已知方言逻辑零改动）
- Modify: `src/bridge/db.rs`（`op_db_list_tables`、`nextSeq` 全家入口同门控，capability 名 `"list_tables"`/`"next_seq"`）
- Modify: `oj/src/migrate.rs`、`oj/src/seed.rs`（未知方言 fail-loud，错误点名字言；复用 `unsupported()` 构造器渲染文本）

**Interfaces:**
- Consumes: `Dialect::known`（T6）、`BridgeError::unsupported`（T3）。
- Produces: 六门控点 + CLI 两处的统一未知方言行为。

- [x] **Step 1: 失败测试（BDD，mock oracle accessor 注入各 op）**
  - Given 方言 "oracle" When `op_db_query_build` Then err.code==OJ_DB_UNSUPPORTED 且 data.capability=="query_build" 且 message 含「db.query/db.exec」指引；
  - Given "oracle" When `op_db_list_tables` / `nextSeq` Then 同构 unsupported（capability 分别为 list_tables/next_seq）；
  - Given "oracle" When `migrate` / `seed` CLI Then 进程 fail-loud 且 stderr 点名 oracle；
  - Given 已知三方言各一代表用例 When 上述各 op Then 行为与现状一致（既有测试零修改全绿）。
- [x] **Step 2: → FAIL**
- [x] **Step 3: 实现门控**（每处 ≤3 行：known 判断 + 一个构造器调用；DRY）
- [x] **Step 4: `cargo test --workspace` → PASS**（未知方言走 `unsupported`；raw 路径 `db.query/exec/stream/tx` 保持全可用）
- [x] **Step 5: Commit** `feat(db): 未知方言六门控点统一结构化 unsupported`（已落地：`b89f793`，与相邻任务合并为一次提交）

### Task 9: JS 面 —— 错误码暴露、既有文本冻结、平台共享模块守卫

**Files:**
- Modify: `src/bridge/mod.rs`（op 层错误映射：`BridgeError` → `{code: code_of(category), msg: message, data}` 信封；既有错误路径文本零改动）
- Modify: `src/bridge/bootstrap.js`（JS Error 携带 `code` 与 `data` 字段，从信封透传）
- Modify: `sample/src/_platform/_shared/search.ts:11`、`versions.ts:62`（闭集 `Dialect` 判断改为「未知方言 → 抛 OJ_DB_UNSUPPORTED/`search`」，删除静默落 pg 分支）
- Test: `src/bridge/mod.rs` 信封映射单测 + 既有错误文本快照

**Interfaces:**
- Consumes: `code_of`（T3）、门控错误（T8）。
- Produces: JS 可判别 `e.code`/`e.data`（Task 10 与 M2 用例依赖）。

- [x] **Step 1: 失败测试（BDD）**
  - Given T8 的 unsupported 错误 When 抵达 JS（经 mock handler）Then `e.code=="OJ_DB_UNSUPPORTED"` 且 `e.data.capability=="query_build"`；
    实现落点：`db_error::tests::bridge_error_maps_to_js_error_with_code_and_data`（Rust 侧附加属性）
    + `db::tests::oracle_error_exposes_code_and_data_to_js`（经 Bridge JS 面读 `e.code`/`e.data`）；
  - Given 既有方言的既有错误路径 When 抵达 JS Then `msg` 文本与快照**逐字节相同**——
    落点 `db::tests::db_error_text_snapshot_is_frozen`（**宿主层快照**，见「阶段一小结·偏差 1」）；
  - Given `search.ts` 收到 `"oracle"` When ensureIndex/upsert/snippet Then 抛 code=OJ_DB_UNSUPPORTED
    且 capability=="search"，不产生任何 SQL——`sample/src/_platform/_shared/search.ts` 已按此守卫（reviewer 复核）。
- [x] **Step 2: → FAIL**（先红：附加属性只有 `code`/`data`，`retryable`/`sessionDisposition` 缺失；快照无守护）
- [x] **Step 3: 实现映射与守卫**（映射表一处 `db_error::code_of`；JS 侧 `normDbErr` 只归一 `data`/`retryable`，无业务逻辑；
  builder / tx / stream 的异步入口统一走 `dbOpWrap`，此前会退化成纯文案）
- [x] **Step 4: `cargo test --workspace` + 真库错误分类用例 → PASS**（workspace 全绿；`OJ_TEST_MYSQL`/`OJ_TEST_PG` 下
  `real_mysql_envelope_classifies_constraint_and_connection` / `real_postgres_envelope_classifies_sqlstate_and_connection` 绿）
- [x] **Step 5: Commit** `feat(db): JS 错误码面与既有文本冻结快照`（已落地：`b89f793`，与相邻任务合并为一次提交）

### Task 10: default-throw cancel 与能力一致性矩阵执行

**Files:**
- Modify: `src/bridge/db.rs`（`DataAccessor`/`TxSession` 增 `fn cancel(&self, ...)` 默认返回 `BridgeError::unsupported(name,"cancel")`）
- Modify: `src/bridge/ffi.rs`（哨兵翻译按矩阵：**已声明**+unsupported → op 级 unsupported 错误（不回退）；**未声明**+unsupported → 标准兜底（stream→fetch_all）；未声明+出现能力 → 采信 + warn 日志）
- Test: `src/bridge/ffi.rs` mock 用例（四象限）

**Interfaces:**
- Consumes: `DialectCaps`（T2）、mock vtable（T4/T5）。
- Produces: `cancel` 默认实现（M2 Oracle/DM 覆盖之）。

- [x] **Step 1: 失败测试（BDD 四象限，mock 可切换 caps 与行为）**
  - Given caps 声明 stream 且 mock 返回 unsupported When stream_open Then op 报 unsupported（不回退 fetch_all）；
  - Given 未声明 stream 且 mock 返回 unsupported When stream_open Then 回退 fetch_all（现行为）；
  - Given 未声明 cancel 且 mock 实现了 cancel 槽为可用 When 调 cancel Then 成功 + warn 日志含能力名；
  - Given caps 声明 cancel 且 accessor 为默认实现 When cancel Then unsupported 错误。
- [x] **Step 2: → FAIL**
- [x] **Step 3: 实现矩阵**（单处 match；warn 一行）
- [x] **Step 4: → PASS**
- [x] **Step 5: Commit** `feat(db): 能力一致性矩阵与 default-throw cancel`（已落地：`b89f793`，与相邻任务合并为一次提交）

### Task 11: mysql/pg 插件迁移 —— 真实能力声明与错误映射表

**Files:**
- Modify: `plugins/oj-db-mysql/src/lib.rs`、`plugins/oj-db-postgres/src/lib.rs`（describe 终稿；新增 `fn map_sqlx_err(sqlx::Error) -> String`（信封 JSON，含 vendor_code：mysql 错误码 / pg SQLSTATE；未知码 → internal+透传）；`stream_next` error 字段产同构信封；查询/执行路径 Err 改产信封 JSON——**本任务起 Err 载荷恒为信封**）
- Modify: `src/bridge/ffi.rs`（await 出口 db 轴单点 `parse_raw`，删除 `ffi_err` 前缀包装）
- Test: 两插件真库套件（T1 的 env）+ mock

**Interfaces:**
- Consumes: Task 4 信封契约；Task 3 `parse_raw`。
- Produces: 全链路结构化错误（宿主拿到类型化 `BridgeError`，message 原文保留）。

- [x] **Step 1: 失败测试（BDD，真库）**
  - Given OJ_TEST_MYSQL 且重复键插入 When exec Then 宿主收到 category==constraint、retryable==false、disposition==keep、`data.vendor_code==1062`；
  - Given pg When `SELECT 1/0` Then category 含语法/执行类正确映射且 vendor_code 为 SQLSTATE 串（`"22012"`）；
  - Given 任一插件 When 断连场景（容器 stop）Then category==connection、disposition==close；→ 本 VM 网络对关闭端口黑洞（不回 RST，drive 30s 超时），改为对 `sqlx_category` 纯单测钉死 Io→connection/close 映射（见 `sqlx_category_maps_io_to_connection_and_close`）；真库端到端由前两条用例覆盖。
  - Given 既有方言 JS 文本快照（T9）When 全量重跑 Then 零 diff。
- [x] **Step 2: → FAIL**（首次实现前红）
- [x] **Step 3: 实现映射表与信封产出**（每插件自包含一份 `map_sqlx_err`——DRY 有意例外，注释引用设计决策；单测覆盖 sqlx Error 各变体 → category 映射；mysql 取原生 errno 数字、pg 取 SQLSTATE 串原样透传）
- [x] **Step 4: 真库套件（必跑）+ workspace → PASS**（本机 `container` 起的 mysql:8.4/16-alpine 真库跑通；T11 自身三条用例全绿。注：同插件另两条既有真库用例 `real_pg_stream_cancel_kills_server_query`/`real_postgres_unsupported_column_types_error_loudly` 当时失败——T14 Step 1 复核确认**与镜像无关**：二者都是「typed 流式对未解码列类型响亮报错」上线后未同步的 stale 断言，已在 T14 Step 3 订正用例，现在 pg 套件 15/15 全绿，详见「偏差 3」）
- [x] **Step 5: Commit** `feat(db)!: mysql/pg 迁移 ABI 12——能力声明与结构化错误映射`（已落地：`b89f793`，与相邻任务合并为一次提交）

### Task 12: sql_trace 携带结构化字段

**Files:**
- Modify: `src/bridge/sql_trace.rs:71`（SqlEvent 增 `category: Option<String>`、`retryable: Option<bool>`）
- Modify: `src/bridge/db.rs`（三个裸 SQL op 插桩处传入 `BridgeError` 派生值；`_sql` 信封同扩）
- Test: sql_trace 单测

- [x] **Step 1: 失败测试**：Given 注入 constraint 错误 When 查询完成 Then trace 行与 `_sql` 信封含 `"category":"constraint"` 与 `"retryable":false`；Given 成功查询 Then 两字段缺省（省字节）。
- [x] **Step 2: → FAIL**（实现前红）
- [x] **Step 3: 实现（纯透传，无逻辑）**：`SqlEvent` 增 `category`/`retryable`；`trace_sql` 收两字段；`snapshot()` 仅在有值时出现（省字节）；`db.rs` 增 `err_meta` 在 `.map_err(to_js_err)` 前从 `BridgeResult` 抽取 `BridgeError` 的 `category`/`retryable`，接到三个裸 SQL op（query/exec/stream_open）；其余插桩点（builder/listTables/nextSeq/占位符）传 `None`。
- [x] **Step 4: → PASS**（单测 `snapshot_carries_category_and_retryable_when_present` / `snapshot_omits_category_when_absent` 全绿，既有 `sql_trace_*` 9 例无回归）
- [x] **Step 5: Commit** `feat(db): sql_trace 携带 category/retryable`（已落地：`b89f793`，与相邻任务合并为一次提交）

### Task 13: 清单锁装配、启动能力位一览、fingerprint 文案

**Files:**
- Modify: `xtask`（build 末尾产出 `bin/plugins/<triple>/plugins.lock.json`：`[{name, abi, fingerprint}]`）
- Modify: `src/bridge/plugin_loader.rs`（生产装配：lock 存在时，目录中多余 .so → 显式错误点名文件；`AbiMismatch` 错误体附双方 fingerprint + 处置指引「重新整体拷贝 bin/，禁止增量覆盖」）
- Modify: `oj/src/serve_cmd.rs`（启动横幅增 host ABI 行 + plugin×abi×caps 一览表；`plugins()` op 增 caps 列）
- Test: loader 单测 + serve 横幅单测

- [x] **Step 1: 失败测试（BDD）**
  - Given lock 存在且目录多出 `oj-db-old.so` When 装配 Then 错误点名该文件（`lock_rejects_extra_library_file_and_names_it`）；
  - Given ABI 不匹配 When 加载 Then 错误含双方 fingerprint 与拷贝指引（`abi_mismatch_message_carries_both_fingerprints_and_remedy`）；
  - Given 三插件 When 启动 Then 横幅含每插件 caps 串（`banner_line_shows_abi_fingerprint_and_db_caps`）；Given `GET /v1/api/plugins` Then 每项含 caps（`PluginInfo::serialized_keys()` 七键 + `loaded_plugin_into_plugin_info_maps_all_fields` 钉 `"caps": null`）。
- [x] **Step 2: → FAIL**（实现前红：`caps` 字段缺失编译不过 / lock 无校验）
- [x] **Step 3: 实现**
  - `xtask`：build 末尾 `write_plugins_lock()` 经 `load_manifest` 取真实 descriptor，产出 `bin/plugins/<triple>/plugins.lock.json`（`[{name, abi, fingerprint}]`）；
  - `plugin_loader`：`read_plugins_lock`（缺失/坏 JSON → None，不强制）+ `reject_unlocked_files`（抽出以便不 dlopen 单测；按文件名字典序取第一个点名）；`AbiMismatch` 文案补双方 fingerprint 与「整体拷贝 bin/，禁止增量覆盖」；`LoadedPlugin.caps` 装配期从 `db` vtable `describe()` 解析；
  - `serve_cmd`：启动横幅 `host ABI_VERSION` 行 + 每插件 `plugin_banner_line`（caps 仅 db 轴插件有）；`PluginInfo.caps` 使 `plugins()` op / `GET {base}/plugins` 同源。
- [x] **Step 4: → PASS**（loader 38 例 + `banner_line_shows_*` + `plugins_op` 4 例全绿；`clippy -D warnings` 无告警；`cargo xtask build` 产出 lock 实物）
- [x] **Step 5: Commit** `feat(db): 插件清单锁、启动能力位一览与 fingerprint 处置指引`（已落地：`b89f793`，与相邻任务合并为一次提交）

### Task 14: M1 终审与阶段小结

**Files:**
- Modify: 本计划文档（追加「阶段一小结」节）

- [x] **Step 1: 阶段验收清单逐项执行**（M1 验收条件 1–6）
  - 1 ABI 12 全链路一致：`cargo build --release --workspace` 绿；ABI 11 插件被拒 + 双方 fingerprint（`abi_mismatch_message_carries_both_fingerprints_and_remedy`）；
  - 2 env-gated 真库**必跑全绿**（2026-10-27，本机 `container`：`mysql:8.4` @192.168.64.2、`postgres:16-alpine` @192.168.64.3）：
    `OJ_TEST_MYSQL=… cargo test --release -p oj-db-mysql` → **15 passed / 0 failed**；
    `OJ_TEST_PG=… cargo test --release -p oj-db-postgres` → **15 passed / 0 failed**（两条既有用例的订正见「偏差 3」）；
    `cargo test --release --workspace` → 全绿（doc-test 仍为 ignored，无新增 skipped）；
  - 3 `placeholder.rs` 既有用例**零修改**全绿（`git diff` 中该文件测试段无改动）；
  - 4 既有方言 JS 错误文本快照零 diff（`db_error_text_snapshot_is_frozen` 钉死 `"db query: Duplicate entry 'neo' for key 'PRIMARY'"` 原样，唯一的例外是 `ffi <op>: ` 双前缀修正，见「偏差 1」）；
  - 5 未知方言全路径 fail-loud：builder / exec_route / 占位符 / list_tables / nextSeq / `_shared/search.ts` + `versions.ts` 六门控点均 `unsupported`，无 Sqlite 回落（`unknown_dialect_*` 系列用例）；
  - 6 `cargo fmt --check` + `cargo clippy --release --workspace --all-targets -- -D warnings` 绿。
- [x] **Step 2: 派 fresh reviewer 审查 M1 全量 diff**（工程师 / 架构师 / 产品 / 安全四路并行只读评审，对照设计 §5.1–5.3 与本计划；产出一条 blocker + 七条 should-fix，逐条处置见 Step 3）
- [x] **Step 3: 依据审查结论修复或记录豁免**
  - **已修**：builder 路径错误被 `to_string()` 压平丢信封（`query.rs` 全程保 `BridgeResult`）；`DialectInfo` 两套序列化口径（改为 `to_wire` 单一口径）；`oj info` 文本缺 caps 列；lock 反向偏差静默（新增 `lock_divergences` 告警）；`versions.ts` 过期注释；`xtask plugin` 不刷新锁的补救文案；`IdStrategy::TwoStep` 被误当 `exec_route` 依据（文档 + `lastid_route_is_mysql_only_not_equivalent_to_id_two_step` 守住）。
  - **豁免（已入文档 + 测试）**：见「阶段一小结 · 偏差与豁免」。
- [x] **Step 4: 追加「阶段一小结」并更新「阶段跟踪」表**（见文末；代码改动已由 `b89f793` 落地，本步仅为文档收尾）

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

## 阶段三·无库切片（2026-10-27，用户决策先行）

**背景**：阶段二两道入口门禁（达梦 DPI 再分发书面许可、Go triple 矩阵进 CI）均未取得，且本机
无可达的 Oracle / 达梦真库（`container` 缓存只有 mysql/pg，无 Oracle/DM 镜像）。M3 的五个任务域里
有三个**完全不依赖真库与外部许可**，故先做这三个，避免整条链空转。

**已完成（3/5 域）**

| 域 | 落点 | 验收口径 |
|---|---|---|
| 方言接入章节（能力位注册表 + 错误映射骨架 + 清单） | `docs/plugins/plugin-development.md` §15（15.1–15.6，606 行文件） | 8 能力位每格齐「宿主消费点 + 正例 + 反例」；错误映射表六类 × (category, retryable, disposition)；DM 错误码标注「二手证据 / 待 M2 真库复核」且明确禁止照搬 Oracle 编号 |
| db 轴模板骨架 | `tools/plugin-template-db/`（新增，不进 workspace members） | `cargo build --release` / `cargo fmt --check` / `cargo clippy --release --all-targets -D warnings` 全绿；`cargo test --release` **11 passed**；`SUPPORTS_STREAM`/`SUPPORTS_CANCEL` 翻 `false` 后**仍 11 passed**（声明与行为同源，不漂移） |

| 方言矩阵 + 类型映射表 | `docs/db-guide.md` §10.5 补交叉引用；新增 §10.6（六类 × 五方言 DDL、读侧值契约、三内置方言读侧边界、新方言四条硬要求） | Oracle/DM 两列标注「M2 方案级 / 待真库复核」；sqlite 静默 `null` 被显式标为历史包袱；§10.5 / §10.6 / §15 / 模板 README 四向互指可达 |

**模板的真实宿主验收（2026-10-27，`bin/oj serve` + `OJ_PLUGINS_DIR` 指向自建目录）**

| 场景 | 结果 |
|---|---|
| 正确 `describe()` | 装配成功，日志 `[oj-serve] host ABI_VERSION = 12` / `plugin db-demo abi=12 fp=… \| caps: placeholder=: id=none stream cancel`——能力位确由 describe 解析而来 |
| 把 `CAP_PLACEHOLDER` 写成 `placeholder_colon_`（typo） | **拒启**并逐字点名：`plugins db assembly: plugin db capability error: unknown capability "placeholder_colon_"` |

**未做（2/5 域，随 M3 整体等 M2）**

1. `db.dialect()` 红字与迁移范式——要等新方言（Oracle/DM）实际接入后才有可写的迁移路径，
   否则只是空泛的「不要依赖 dialect()」；
2. `ops-manual` §1/§6/§7/§8 增补（发布物、告警映射、`plugin abi mismatch` / `fingerprint mismatch`
   排障行、回滚三元组）——告警映射需要真实新方言的错误码分布，`abi/fingerprint` 排障行的
   **文案**要与 M2 落地后的实际报错逐字对齐（M3 验收条件即「ops 表与实际错误文本一致」）。

**顺带记录的两处缺口（均登记不改实现）**

1. ~~`ColumnType::from_schema_type` 把未识别的 schema 类型串（如 `type: varchar`）静默归
   `Unknown` → 按 text 处理~~ ——**已修（v0.1.88，本切片后跟进，见 CHANGELOG `fix(schema)`）**。
   复核结论：`schema.yaml` 这条路**本来就是 fail-fast 的**（`SchemaFile::parse` 在声明期拒未知
   类型、点名原串并列出支持清单，v0.1.24 起如此），静默降级只存在于核心侧的
   `ColumnType::from_schema_type` 兜底分支与 `registry_tables()` 的喂料路径。现两处都收紧：
   `from_schema_type` → `Option`（词汇表唯一来源 `ColumnType::SCHEMA_TYPES`）、`registry_tables()`
   → `Result`（未识别即拒载并点名 表.列），并删掉 `oj/src/schema.rs` 里与核心重复的 `ColType`
   枚举。`Unknown` 仅保留「列类型未声明」语义。面向兼容面另配专测
   `unknown_column_type_tenant_still_binds_text`：旧 `table()` 路径的 `tenant_id` 仍绑**字符串**
   （用纯数字租户头判别，误判成数值即红），`Option` 化不会顺手改掉 v0.1.23 契约。
2. 能力位解析失败的文案只点名**轴**不点名**插件**：`plugins db assembly: plugin db capability
   error: unknown capability "…"`（单 db 插件时无歧义，T2 串容易定位；两个 db 插件同时在场时
   无法一眼看出是哪个）。建议 M3 落 ops 排障行时连带补上插件名——**本次不改**，避免在收尾切片里
   动装配层文案（该文案未进冻结快照，改动成本低，留待与 ops 表一起对齐）。

---

## Self-Review 记录

- **Spec 覆盖**：设计 §5.1→T2/T3/T4/T5/T10/T12；§5.2→T6/T7/T8；§5.3→T10；§5.5→T8/T9；§8-M1 全项→T1/T13/T14；§5.4/§5.6→阶段二/三（入口门禁阻塞，非缺口）。
- **类型一致性**：`DialectInfo`/`DialectCaps`/`BridgeError`/`code_of`/`unsupported`/`known`/`dialect_info` 跨任务签名一致；能力串常量唯一来源 T2。
- **Review Focus**：五类输入均落在 owning task 的测试步骤（见各 BDD 条目）。
- **比例**：计划以签名/断言/命令为主，无实现体转写。

---

# 阶段一小结（M1 契约与宿主）

**日期**：2026-10-27 ｜ **状态**：✅ 完成 ｜ **commit**：2（`b89f793` 代码+对外文档；本次提交为计划/设计文档）

## 完成项

| # | 交付物 | 落点 |
|---|---|---|
| 1 | 真库测试基座：compose 两服务 + `OJ_TEST_MYSQL`/`OJ_TEST_PG` env-gated 必跑约定 | `docker-compose.yml`、`docs/testing.md`「真库套件」 |
| 2 | 能力闭集解析器（8 个能力串、缺组/重组/未知串/坏 JSON 全 fail-loud） | `src/bridge/dialect_caps.rs`（新） |
| 3 | 结构化错误信封 + 单点解析（`category/retryable/session_disposition/message/data`，未知 vendor 码 → `internal` + 原样透传） | `src/bridge/db_error.rs`（新）、`src/bridge/ffi.rs` 的 `parse_raw` |
| 4 | ABI 11 → 12：工厂级 `describe() -> {dialect, capabilities}`，严格相等门禁 | `oj-plugin-ffi/src/{lib,db}.rs` |
| 5 | 装配期接入 `describe()`，删除「未知方言回落 Sqlite」全部路径 | `src/bridge/plugin_loader.rs`、`accessor_sqlx.rs` |
| 6 | 两层方言模型：`Dialect`（builder-known 闭集）vs 边界 `DialectInfo`；`ActiveTx` 持快照 | `src/bridge/accessor_sqlx.rs`、`query.rs` |
| 7 | 占位符触发条件改为能力查表，`placeholder.rs` 既有 24 例零修改 | `src/bridge/placeholder.rs` |
| 8 | 六门控点未知方言统一结构化 `unsupported` | `db.rs`/`query.rs`/`sample/src/_platform/_shared/*.ts` |
| 9 | JS 面：`e.code`/`e.category`/`e.retryable`/`e.sessionDisposition`/`e.data`（`e.message` 文本冻结） | `src/bridge/db_error.rs`、`bootstrap.js` |
| 10 | `cancel` 默认抛错 + 能力一致性四象限矩阵 | `src/bridge/dialect_caps.rs`、`db.rs` |
| 11 | mysql/pg 插件各持 `map_sqlx_err`（DRY 有意例外）+ `describe()` 能力声明 | `plugins/oj-db-{mysql,postgres}/src/lib.rs` |
| 12 | `sql_trace` 携带 `category`/`retryable`（仅在有值时出现） | `src/bridge/sql_trace.rs`、`db.rs` |
| 13 | 清单锁 `plugins.lock.json`、启动能力位一览、fingerprint 处置指引 | `tools/xtask`、`plugin_loader.rs`、`serve_cmd.rs`、`info_cmd.rs` |
| 14 | CHANGELOG / devkit 四件 / 方言矩阵文档 / 设计 §3 兼容性条款 | `CHANGELOG.md`、`docs/devkit/*`、`docs/db-guide.md`、`docs/plugins/plugin-development.md` |

## 偏差与豁免

1. **既有错误文本的 `ffi <op>: ` 前缀被移除（唯一文本例外）**。ABI 11 下宿主会给插件错误串再套一层
   `ffi db query: `，与插件自带 `db query: ` 叠成 `ffi db query: db query: …`（无意义双前缀）。
   现改为单前缀、**message 正文逐字节不变**；钉死用例 `db_error_text_snapshot_is_frozen`，条款写入
   设计 §3 与 CHANGELOG。业务侧若曾匹配 `ffi ` 前缀需去掉——已进 `CHANGELOG.md` 的升级说明。
2. **`IdStrategy` 不参与 `exec_route` 路由**。取 ID 语句是方言私有实现，能力位只表达能力不表达语句；
   因此路由仍按 builder-known `Dialect` 判定，`sqlite` 声明 `id_two_step` 但走 `Rows`（有 RETURNING）。
   已写入 `query.rs` 注释并用 `lastid_route_is_mysql_only_not_equivalent_to_id_two_step` 守住，
   防止后人把 `caps.id` 当路由依据。
3. **两条既有 pg 真库用例订正（用例错，非实现错）**。T11 时记录为「镜像行为差异」，T14 Step 1 复核
   **否定该判断**：在 HEAD（`59bc531`）的干净 worktree 上同样失败，根因是二者都早于「typed 流式对
   未解码列类型响亮报错」：
   - `real_pg_stream_cancel_kills_server_query` 的慢查询投影了 `int4`/`void` 列 → 行转换立即报错 →
     流式条目被移除 → `stream_cancel` 变 no-op → 服务端查询跑满 20s（假失败）。改为
     `cast(i as text)` + `cast(pg_sleep(0.02) as text)`（逐行求值，仍是真 20s 慢查询）；
   - `real_postgres_unsupported_column_types_error_loudly` 的流式分支断言 `Err`，而 `stream_next`
     的错误**在带内**（`{"error":"…"}`，见其响应契约注释）。改为断言带内 `error` 字段。
   仅动用例，插件语义零改动；修后 pg 15/15。
4. **清单锁的反向偏差只告警**。目录缺条目 / abi-fingerprint 与锁不符 → `eprintln!` 告警（不阻断启动），
   因为「锁比目录新」多为增量覆盖的半拷贝现场，硬失败会把运维挡在门外；正向（目录多出未登记 `.so`）
   才硬拒。告警文案由 `lock_divergences_report_missing_and_mismatched_entries` 钉死。
5. **`retryable` 在 JS 面以 1/0 传输**。`deno_error` 0.7 的 `PropertyValue` 只有 String/Number，
   布尔无位可放；`bootstrap.js` 的 `normDbErr` 统一归一为布尔，业务看到的是 `e.retryable === true/false`。
6. **「声明了 `cancel` 但走默认实现」判 `unsupported`**。能力声明是契约，默认实现不算履约；
   宁可显式不受理也不假装支持（矩阵第四象限）。
7. **`placeholder.rs` 的「既有用例零修改」按语义而非字面达成**。门条件从 `Dialect` 换成
   `PlaceholderStyle` 后，24 条用例的**断言与输入数据一字未改**，但调用点参数随之改为
   `PlaceholderStyle::{Q,Dollar,Colon}`，`t25_sqlite_and_mysql_pass_through` →
   `t25_q_and_colon_pass_through`、`t26_postgres_decision_path_rewrites` →
   `t26_dollar_decision_path_rewrites`（旧名已与新语义不符，属误导）。逐字节等价由这些用例本身继续守护。

## 度量

- **任务**：14/14（T1–T14）；**commit**：0（由用户执行，14 个 Commit step 全部保留未勾选）。
- **代码**：32 个已跟踪文件 +2180 / −290（不含 `docs/`）；另新增 2 个未跟踪文件共 794 行——
  `src/bridge/db_error.rs` 412 行、`src/bridge/dialect_caps.rs` 382 行。
- **测试**：新增 36 个 `#[test]`/`#[tokio::test]`（删除 0）；`cargo test --release --workspace`
  **1084 passed / 0 failed**（doc-test 2+1 ignored，与阶段前一致，无新增 skipped）；
  真库 `oj-db-mysql` 15/15、`oj-db-postgres` 15/15（env 已设，非跳过）。
- **门禁**：`cargo fmt --check`、`cargo clippy --release --workspace --all-targets -- -D warnings` 均绿。

## 风险与遗留

1. **【已修，M1 后跟进】typed pg 流式不解码 `int4`/`int2`**（`column_json_pg` 只有 bool/i64/f64/
   String/bytes 分支，sqlx 的 `i64::compatible` 只认 INT8；Any 路径的 `compatible` 是
   `is_integer()`，所以 `db.query` 正常、只有 `db.stream` 报「未解码列类型」）。已补 `i16`/`i32`
   分支，真库回归 `real_pg_stream_decodes_common_numeric_types` 逐值对齐两条路径；`real`（FLOAT4）
   经核实**两条路径都不支持**（Any 的 `f64::compatible` 同样严格等值），故一并钉死「统一不支持 +
   cast 指引」，不做单边补齐。`oj-db-postgres` 0.1.1 → 0.1.2（ABI 不变）。剩余的
   `jsonb`/`numeric`/`uuid`/时间戳/数组仍按设计 §5.6 并入 M2 类型映射表。
2. **`cargo xtask plugin <name>` 只拷单个库、不刷新 `plugins.lock.json`**，增量覆盖后可能撞锁告警/硬拒。
   已在 loader 报错文案与 devkit 陷阱表写明「改插件后用 `cargo xtask build` 整体归置」。
3. **`yuescript`/`dbx` 侧未对齐**：本阶段只改 oj-bin（其余仓库只读），下游若仍按「三方言枚举」写守卫，
   新方言上线后其白名单会先拦住——`sample/src/_platform/_shared/versions.ts` 已订正注释，把该白名单
   点明为唯一防线。
4. **`postgres:16`（非 alpine）未验证**：本机镜像仓只有 `16-alpine`，`docker.io` 拉取超时；上面的
   「镜像差异」假设已被证伪，故不再阻塞，但建议 CI 用 `postgres:16` 复跑一次真库套件。

## 阶段二入口门禁状态

| 门禁 | 状态 | 阻塞说明 |
|---|---|---|
| 达梦 DPI「可捆绑再分发」书面结论 | ❌ 未取得 | 拿不到 → 回方案评审评估 JDBC 备选；未定则 M2 不开工 |
| Go 工具链进 xtask/CI 的 triple 矩阵确认 | ❌ 未确认 | `oj-db-oracle` 的 sidecar 为版本钉死同级文件（非嵌入抽取），需先钉 CI 矩阵 |

**结论**：M1 达成本阶段全部验收条件；M2 因上述两项外部前置未满足而**不进入**。
