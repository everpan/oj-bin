# oj-db-jdbc 泛型 JDBC 插件实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 以 Java sidecar（NDJSON over stdio）+ cdylib 插件形态为 oj 增加泛型 JDBC 后端，drop-in 驱动目录接入达梦/金仓等企业库，宿主仅一处 `db.import` 门控补漏。

**Architecture:** Rust 插件（镜像 oj-db-mysql 结构：自建 tokio runtime + `spawn_ffi_future`）管理一个多路复用 JVM sidecar 进程；Java 桥单模块 Gradle 项目，per-conn FIFO 队列串行化 JDBC 调用；两侧经 golden fixture 共享同一份类型映射事实（DRY）。

**Tech Stack:** Rust（cdylib、tokio、serde_json、oj-plugin-ffi/stabby）、Java 11（Gradle wrapper、JUnit 5、H2 test-only）、GitHub Actions。

**Spec:** `docs/superpowers/specs/2026-11-20-jdbc-generic-plugin-design.md`（v1.2，执行者必须通读；本计划的所有取值以 spec 为准）

## 全局约束（逐条来自 spec/CLAUDE.md，所有任务隐含遵守）

- 构建/测试/lint 一律 `--release`；禁 debug 构建；测试用 `tokio::test(flavor = "current_thread")`（插件内部自建 multi-thread runtime 除外，镜像 oj-db-mysql）。
- 条件等待一律 deadline/有界轮询，禁止固定 sleep 当同步。
- 插件 crate `panic = "unwind"` 不得覆盖为 abort；vtable 方法走 `catch_future`，init 走入口宏内建 `catch_unwind`。
- SQL 由业务全责（jdbc 后端无 SchemaRegistry 构造器）——这是 spec 明示的 v1 限制，文档诚实沟通，不在代码里假装有防护。
- 错误消息含凭据段必须脱敏（`://***@`）；DECIMAL(scale>0)/BLOB 回宿主形状遵循 jsint/cellenv 既有约定。
- 提交尾行固定裸行 `unix@vip.qq.com ai`；版本相关提交动作按 CHANGELOG 头规范。
- e2e 凭据一律 env 注入不进仓库；env-gated 腿缺 env 必须 skip 并打印原因（含获取指引）。

## SOLID / DRY 落点（架构约束）

- **单一职责**：`codec.rs`（帧编解码）/`dsn.rs`（DSN 解析）/`errmap.rs`（错误归类+脱敏）/`sidecar.rs`（进程生命周期）/`lib.rs`（vtable 胶水）各自只一件事；Java 侧 `Bridge`（NDJSON 循环）/`Session`（per-conn 队列+JDBC 调用）/`RowCodec`（类型映射）/`DriverLoader` 同构拆分。
- **依赖倒置**：vtable 胶水只依赖 `SidecarMgr` 的具体类型（不碰 Child/协议细节）；Java 侧 dispatch 只依赖 `Session` 接口形状。
- **DRY**：类型映射表只在 Java `RowCodec` 实现一次，`plugins/oj-db-jdbc/fixtures/types.json` 为两侧共用的 golden fixture（Rust 断言解码形状，Java 断言编码形状）；错误归类只在 Rust `errmap.rs` 一份；厂商识别表只在 `dsn.rs` 一份。任何映射改动改一处 + fixture，不许两侧各写一份。

## Review Focus（spec 隐含但易被漏测的五类输入，测试已锚到任务）

1. **native 库直写 fd 1 的无换行垃圾** → 64 MiB 行长上限判死并走重启路径（Task 9 测试）。
2. **驱动 connect 失败消息回显 `user:pass@`** → 错误信封脱敏（Task 8 测试 + Task 14 e2e 断言）。
3. **信封 `{"$env":"X"}` 而 X 未设置** → 明确报错，不静默空串（Task 7 测试）。
4. **同一 stream 上 next 与 cancel 并发** → 响应帧唯一（claimed 先赢者写帧）（Task 6 测试）。
5. **DM 驱动把 INT 上报为 NUMERIC** → `getScale==0` 走整数族（Task 4 fixture + Task 14 e2e 断言整数列回 number）。

---

## 执行状态跟踪（执行时逐任务勾选，阶段完成后在「阶段小结」补记）

| 阶段 | 任务 | 内容 | 状态 |
|---|---|---|---|
| 0 宿主补漏 | T1 | db.import 门控 | ✅ |
| 1 Java 桥 | T2 | Bridge 骨架 + 驱动加载 + connect/disconnect + EOF 退出 | ✅ |
| 1 Java 桥 | T3 | query/exec + 参数绑定 RowCodec | ✅ |
| 1 Java 桥 | T4 | 结果集 RowCodec + golden fixture | ✅ |
| 1 Java 桥 | T5 | 事务 op | ✅ |
| 1 Java 桥 | T6 | 流式 op + cancel 唯一性 + per-conn 队列 | ✅ |
| 2 Rust 插件 | T7 | crate 骨架 + vtable 常量 + dsn.rs | ✅ |
| 2 Rust 插件 | T8 | codec.rs + errmap.rs | ✅ |
| 2 Rust 插件 | T9 | SidecarMgr + 假 java 夹具 | ✅ |
| 2 Rust 插件 | T10 | vtable 胶水：connect/query/exec/close/tx | ✅ |
| 2 Rust 插件 | T11 | vtable 胶水：stream 四槽 | ✅ |
| 3 联调 | T12 | H2 真实 JVM 冒烟（env-gated jar） | ✅ |
| 4 构建/CI | T13 | xtask 归置 + plugin-matrix Java 腿 | ✅ |
| 5 e2e | T14 | 达梦远程主腿 + 金仓可选腿 | ⬜ |
| 6 文档/发版 | T15 | jdbc.md + SKILL 陷阱 + devkit 四件 + sample + CHANGELOG 定稿 | ⬜ |

---

### Task 1: 宿主 db.import 门控补漏

**Story（BDD）**: Given 一个 dialect 为 `"jdbc"` 的 FFI 连接，When 调用 `db.import`，Then 返回结构化 unsupported 错误（而非 panic / import 任务卡死 Running）。
**INVEST**: I=只改宿主一处，不依赖插件存在 · N=门控位置可按评审调整 · V=消除已知 panic 路径，对一切未来未知方言插件生效 · E=一行+一测 · S=最小 · T=测试断言 structured unsupported 且无 panic。

**Files:**
- Modify: `src/bridge/db_import.rs`（`op_db_import`，`let da = ...`（:1270）之后、参数面校验之前）
- Test: `src/bridge/db_import.rs`（模块内 `#[cfg(test)]`，参照文件内既有测例风格）

**Interfaces:**
- Consumes: `require_known_dialect(da.dialect_info(), "import")`（模式见 `src/bridge/db_seq.rs:30`）
- Produces: 无新接口；行为契约「未知方言 import → structured unsupported」供 Task 14 e2e 断言。

- [ ] **Step 1: 写失败测试**——fake `DataAccessor`（`dialect_info().name = "jdbc"`，`dialect()` 走默认），断言 `op_db_import` 返回错误且错误文本含 `unsupported`，且调用 `da.dialect()` 零次（用计数器，防回归 panic 路径）。
- [ ] **Step 2: 运行确认失败**——`cargo test --release db_import`（根 crate）；Expected: 当前实现走到 `run_import` 的 `da.dialect()` → 测试计数器 >0 或 panic，FAIL。
- [ ] **Step 3: 实现**——在 da 解析后插入 `let _dialect = require_known_dialect(da.dialect_info(), "import")?;`。
- [ ] **Step 4: 运行确认通过**——`cargo test --release db_import` Expected: PASS。
- [ ] **Step 5: 全量门禁**——`cargo clippy --release --workspace --all-targets -- -D warnings` + `cargo fmt --check`。
- [ ] **Step 6: Commit**——`fix(db): db.import 未知方言门控补漏（fail-loud 前置，消除 detached 任务 panic 路径）`，尾行 `unix@vip.qq.com ai`。

### Task 2: Java 桥骨架 — NDJSON 循环 + 驱动加载 + connect/disconnect + EOF 退出

**Story（BDD）**: Given 驱动目录含 H2 jar，When 桥进程启动并收到 `connect`（`jdbc:h2:mem:t`）请求，Then 回 `{"handle":1}`；When stdin 关闭（EOF），Then 进程有序退出（非 kill）。
**INVEST**: I=独立可测（JUnit+H2） · N=包名/类名可议 · V=跑通最小端到端进程 · E=一个 main + 两个类 · S=不含查询语义 · T=JUnit 直接断言。

**Files:**
- Create: `java/jdbc-bridge/build.gradle`（Java 11 工具链、shadowJar、JUnit5、H2 testRuntimeOnly、application mainClass `app.oj.jdbc.Bridge`）
- Create: `java/jdbc-bridge/gradle/wrapper/gradle-wrapper.properties`（distributionUrl 钉死具体版本）+ wrapper jar（校验和记入 `java/jdbc-bridge/README.md`）
- Create: `java/jdbc-bridge/src/main/java/app/oj/jdbc/Bridge.java`（main：启动打印 `{"hello":{"proto":1,"bridge":"0.1.0"}}` 行并 flush；`BufferedReader.readLine` 循环；EOF → `Session.shutdownAll()` 后退出 0）
- Create: `java/jdbc-bridge/src/main/java/app/oj/jdbc/DriverLoader.java`（`load(Path driversDir)`：URLClassLoader 装载全部 jar → `Thread.currentThread().setContextClassLoader(ucl)`；信封可选 `driver_class` → `Class.forName(name, true, ucl)`；System.out 重定向到 System.err）
- Create: `java/jdbc-bridge/src/main/java/app/oj/jdbc/Session.java`（`connect(Properties)` → 存 `Connection`；`disconnect()`；静态注册表 `ConcurrentHashMap<Integer, Session>`）
- Test: `java/jdbc-bridge/src/test/java/app/oj/jdbc/BridgeTest.java`

**Interfaces:**
- Produces: 请求帧 `{"id":u64,"op":"connect"|"disconnect",...}`，响应帧 `{"id":u64,"ok":...}`/`{"id":u64,"err":"..."}`；hello 行 proto=1。帧未知键一律忽略。这些是 Task 3-6 与 Rust 侧 codec 的共同契约，逐字段钉死。

- [ ] **Step 1: 写失败测试**——JUnit：`BridgeTest.serveOneLine`（进程内直接调 package-private `Bridge.serve(BufferedReader, BufferedWriter)`，不真起进程）：给定输入行 `{"id":1,"op":"connect","url":"jdbc:h2:mem:t1"}`，断言输出行解析后 `ok.handle == 1`；再喂 EOF 断言 serve 返回。第二个用例：未知 op → `err` 帧且进程不退出。
- [ ] **Step 2: 运行确认失败**——`cd java/jdbc-bridge && ./gradlew test`；Expected: 编译失败（类不存在）。
- [ ] **Step 3: 实现**——按 Files 清单四个类；`serve` 循环 try/catch Throwable 逐请求兜底回 err 帧（OOME 之外的 Throwable 不杀循环）。
- [ ] **Step 4: 运行确认通过**——`./gradlew test` Expected: PASS。
- [ ] **Step 5: Commit**——`feat(jdbc-bridge): NDJSON 循环 + 驱动加载 + connect/disconnect + EOF 有序退出`。

### Task 3: Java query/exec + 参数绑定

**Story（BDD）**: Given 已连接的 H2，When 收到 `query`（SQL 带 `?` + JSON 参数数组含 null/整数/浮点/字符串/布尔/超长串/`"$blob:<b64>"` 信封），Then 回行数组且 null 参数绑定不炸（`setNull(i, Types.VARCHAR)`）。
**INVEST**: I=只依赖 T2 的 serve 框架 · N=阈值常量可议 · V=最小可用查询路径 · E=RowCodec 一个类 + 两个 op · S=单类映射 · T=JUnit 逐形状断言。

**Files:**
- Create: `java/jdbc-bridge/src/main/java/app/oj/jdbc/RowCodec.java`——`static void bind(PreparedStatement ps, List<Object> params)`：null→`setNull(VARCHAR)`；整数→`setLong`；浮点→`setDouble`；布尔→`setBoolean`；字符串以 `"$blob:"` 前缀→base64 解码 `setBytes`、长度>4000→`setClob`/Reader、否则 `setString`；对象/数组→`setString(json)`
- Modify: `Bridge.java`——dispatch `query`/`exec`：`Session.query(sql, params)` 走 RowCodec.bind，`ResultSet`→`List<Map<String,Object>>`（值先保留原始 Object，Task 4 定型）；exec 回受影响行数
- Test: `RowCodecTest.java`（H2 实绑：每种 JSON 形状一个断言 + null 列 insert/select 往返）

**Interfaces:**
- Consumes: T2 帧契约 + `Session`。
- Produces: `RowCodec.bind`（Task 4/5/6 复用，DRY——所有参数绑定只此一处）。

- [ ] **Step 1: 写失败测试**——`RowCodecTest`：H2 建表 `t(a int, b varchar, c double, d boolean, e blob)`；`insert into t values(?,?,?,?,?)` 绑 `[1, null, 1.5, true, "$blob:aGk="]`，select 断言 5 列形状（e 列此时允许原始 byte[] 占位断言）。
- [ ] **Step 2: `./gradlew test` Expected: FAIL**（RowCodec 不存在）。
- [ ] **Step 3: 实现** `RowCodec.bind` + `Bridge` dispatch（query/exec 的 Session 方法）。
- [ ] **Step 4: `./gradlew test` Expected: PASS**。
- [ ] **Step 5: Commit**——`feat(jdbc-bridge): query/exec + 参数绑定（setNull(VARCHAR) 兼容 Oracle/DM）`。

### Task 4: 结果集 RowCodec + golden fixture

**Story（BDD）**: Given H2 表含各类型列（含 DECIMAL(p,0) 模拟 DM 的 NUMERIC 上报、TIME、TIMESTAMP），When `query` 返回行，Then 每列按 spec §5.4 表映射（scale==0 走整数、TIME 出 "HH:MM:SS"、DECIMAL(scale>0) 出 string、BLOB 出 `"$blob:<b64>"`），且 Rust 侧 fixture 测试消费同一份 `fixtures/types.json`。
**INVEST**: I=只加解码方向 · N=fixture 组织可议 · V=类型正确性是本插件核心价值 · E=一张映射表 · S=单类 · T=双侧同 fixture 断言。

**Files:**
- Modify: `java/jdbc-bridge/src/main/java/app/oj/jdbc/RowCodec.java`——`static Object cell(ResultSet rs, int i, ResultSetMetaData md)`：按 `md.getColumnType(i)` + `md.getScale(i)` 分流（spec §5.4 表逐行实现；DECIMAL/NUMERIC 先判 scale==0 → `getBigDecimal` 判幅值给 Integer/Long/BigInteger-as-string；TIMESTAMP_WITH_TIMEZONE→`getObject(i, OffsetDateTime.class)`；TIME→`getTime` 转 "HH:MM:SS"；BLOB 族→`cellenv` 形态手拼 `"$blob:"+Base64`；CLOB/二进制超 16MB→抛 `CellTooLargeException`）
- Create: `plugins/oj-db-jdbc/fixtures/types.json`——golden：输入行（各 JDBC 类型+值）→ 期望 JSON 单元格数组（Task 7 建 crate 后 Rust 侧消费；本任务先在 Java 侧落地并注释引用路径）
- Test: `java/.../RowCodecTest.java` 增 `mapsResultSetCells`（H2 建全类型表，对照 fixture 断言）

**Interfaces:**
- Produces: `RowCodec.cell`（Task 5/6 复用）；`fixtures/types.json`  schema：`[{"jdbc_type":"...","scale":0,"input":"...","expect":<json>}, ...]`。

- [ ] **Step 1: 写 fixture + 失败测试**——`types.json` 覆盖：INTEGER、BIGINT、DECIMAL(scale 0/2)、DOUBLE、BOOLEAN、VARCHAR、DATE、TIME、TIMESTAMP、TIMESTAMP_WITH_TIMEZONE、BLOB、NULL、CLOB。
- [ ] **Step 2: `./gradlew test` Expected: FAIL**（cell 未实现分流）。
- [ ] **Step 3: 实现** `RowCodec.cell`。
- [ ] **Step 4: `./gradlew test` Expected: PASS**；并跑 shadowJar 产出 `oj-jdbc-bridge.jar` 备用（T12 用）。
- [ ] **Step 5: Commit**——`feat(jdbc-bridge): 结果集类型映射 + golden fixture（scale==0 整数分流）`。

### Task 5: Java 事务 op

**Story（BDD）**: Given 已连接 H2，When `begin` → `tx_exec(insert)` → `tx_rollback`，Then 查询不到该行；commit 分支则查得到。事务内 select 走同一连接。
**INVEST**: I=只加 op · N=无 · V=事务是 vtable 硬槽位 · E=Session 三个方法 · S=小 · T=H2 往返断言。

**Files:**
- Modify: `java/.../Session.java`——`begin()`（`setAutoCommit(false)`，登记 tx_id）、`txExec/txQuery`（同连接，复用 `RowCodec.bind`/`cell`，DRY）、`commit()/rollback()`（恢复 autoCommit）
- Modify: `Bridge.java`——dispatch `begin`/`tx_query`/`tx_exec`/`tx_commit`/`tx_rollback`
- Test: `BridgeTest` 增 `txCommitMakesRowVisible` / `txRollbackUndoesInsert`

**Interfaces:**
- Produces: tx 响应帧 `{"tx_id":u64}`；op 语义与 vtable 对齐（Task 10 直接转发）。

- [ ] **Step 1: 写失败测试**（上述两例）。
- [ ] **Step 2: `./gradlew test` Expected: FAIL**。
- [ ] **Step 3: 实现**。
- [ ] **Step 4: `./gradlew test` Expected: PASS**。
- [ ] **Step 5: Commit**——`feat(jdbc-bridge): 事务 op（begin/tx_*/commit/rollback）`。

### Task 6: Java 流式 op + cancel 唯一性 + per-conn 队列

**Story（BDD）**: Given 已连接 H2 的 250 行表，When `stream_open` → 三次 `stream_next`，Then 收到 100/100/50+done 三帧；When 在 `stream_next` 在途时发 `stream_cancel`，Then 恰好一帧响应（claimed 先赢者），另一路径丢弃；同 conn 的两条请求严格 FIFO 有序（单线程消费）。
**INVEST**: I=依赖 T2 框架，不依赖 T3-5 语义 · N=队列实现可议 · V=流式+cancel 是能力串承诺 · E=一个 dispatcher 重构 · S=单任务 · T=并发断言。

**Files:**
- Modify: `java/.../Bridge.java`——请求 dispatch 改为按 `conn_id` 投递到 per-conn 单线程队列（`Session` 持有其 `ExecutorService` 单线程；`connect` 在全局 dispatcher 线程执行）；`Statement.cancel()` 由独立线程执行（唯一绕过队列者）
- Modify: `java/.../Session.java`——`streamOpen`（`setFetchSize(500)`，登记 stream_id）、`streamNext`（≤100 行/帧，空→`{"done":true}`）、`streamCancel`（置标志 + cancel）、`streamClose`；每请求 `AtomicBoolean claimed`，响应帧先赢者写
- Test: `BridgeTest` 增 `streamBatchesAt100`、`cancelProducesExactlyOneResponse`（cancel 与慢查询竞争）、`sameConnOpsAreOrdered`（两请求交错发，响应顺序==请求顺序）；`SQLFeatureNotSupportedException` → err 帧含 `"unsupported"` 前缀

**Interfaces:**
- Produces: stream 帧语义（`{"rows":[...]}`/`{"done":true}`/`{"error":"cancelled"}`）——Task 11 直接依赖。

- [ ] **Step 1: 写失败测试**（上述四例）。
- [ ] **Step 2: `./gradlew test` Expected: FAIL**。
- [ ] **Step 3: 实现**（per-conn 队列 + 四 op + claimed）。
- [ ] **Step 4: `./gradlew test` Expected: PASS**。
- [ ] **Step 5: Commit**——`feat(jdbc-bridge): 流式 op + cancel 响应唯一性 + per-conn FIFO 串行化`。

### Task 7: Rust crate 骨架 + vtable 常量 + dsn.rs

**Story（BDD）**: Given 各种 DSN 字符串，When `parse_dsn` 处理，Then 裸 URL 原样透出、信封解析 user/password/props、`{"$env"}` 展开、整串 `ENC[...]` 原样透出（解密在宿主）、vendor 表识别 `kingbase8`。
**INVEST**: I=纯函数模块 · N=API 形状可议 · V=一切 vtable 的入口 · E=一个模块 · S=小 · T=表驱动单测。

**Files:**
- Create: `plugins/oj-db-jdbc/Cargo.toml`（cdylib；依赖 oj-plugin-ffi、serde_json、tokio、libc（dladdr）、base64；版本随 oj 0.1.141）
- Create: `plugins/oj-db-jdbc/src/lib.rs`——`oj_plugin_entry!(init, config: "oj-db-jdbc", db => &DB_VTABLE)`；`describe()` 常量 `{"dialect":"jdbc","capabilities":["placeholder_q","id_two_step","stream","cancel"]}`；`schemes()` = `["jdbc:"]`；`dialect()` 恒 `"jdbc"`；其余槽位先 `ready_err("not implemented")` 占位（T10/T11 填）
- Create: `plugins/oj-db-jdbc/src/dsn.rs`——`pub struct Envelope { url, user, password, props, driver_class }`；`pub fn parse_dsn(dsn: &str) -> Result<ParsedDsn, String>`（`ParsedDsn::Bare(String)` / `ParsedDsn::Envelope(Envelope)`）；`pub fn vendor_of(url: &str) -> &'static str`（spec §5.5 识别表）；`$env` 展开（缺失→Err 指明变量名）
- Test: `plugins/oj-db-jdbc/src/dsn_tests.rs`（表驱动：spec §5.2 全部形态 + ENC 整串透传 + vendor 表含 kingbase8/dm/oracle/postgresql）

**Interfaces:**
- Produces: `parse_dsn`/`vendor_of`（T9/T10 用）；`ParsedDsn` 形状。

- [ ] **Step 1: 写失败测试**（表驱动，含 `$env` 未设置→Err("env var X not set")）。
- [ ] **Step 2: `cargo test --release -p oj-db-jdbc` Expected: FAIL**（模块不存在）。
- [ ] **Step 3: 实现**。
- [ ] **Step 4: `cargo test --release -p oj-db-jdbc` Expected: PASS**；`cargo xtask plugin oj-db-jdbc --check` 通过。
- [ ] **Step 5: Commit**——`feat(jdbc): crate 骨架 + DSN 解析（信封/$env/ENC 透传/vendor 识别）`。

### Task 8: Rust codec + errmap

**Story（BDD）**: Given 乱序到达的 NDJSON 字节流（含分片/粘包/非 UTF8/超长行/未知键），When `codec` 解帧，Then 行为符合 spec（透传/报错/判死）；Given 各类 SQLState/厂商码/含凭据消息，When `errmap` 归类，Then category/retryable/session_disposition 正确且消息脱敏。
**INVEST**: I=纯模块 · N=内部表示可议 · V=协议与错误是两个最高危面 · E=两模块 · S=中 · T=表驱动单测。

**Files:**
- Create: `plugins/oj-db-jdbc/src/codec.rs`——`pub struct FrameReader`（`push(&[u8]) -> Result<Vec<Line>, FrameError>`；行上限 64 MiB，超限→`FrameError::TooLong`）；`pub fn encode_request(id, op, params...) -> String`；`pub fn parse_response(line) -> Response`（未知键忽略）
- Create: `plugins/oj-db-jdbc/src/errmap.rs`——`pub fn classify(sql_state: Option<&str>, error_code: Option<i32>) -> (category, retryable, session_disposition)`（镜像 oj-db-mysql 的 SQLState class 表；无标准 SQLState→按 error_code 粗分类或 internal，绝不静默 retryable）；`pub fn redact(msg: &str) -> String`（`://[^@/\s]*@` → `://***@`）
- Test: `codec_tests.rs`（分片/粘包/非 UTF8/64MiB+1 判 TooLong/未知键忽略）、`errmap_tests.rs`（08xxx/40xxx/ null SQLState + DM 厂商码 + redact 含 `user:pass@`）

**Interfaces:**
- Produces: `FrameReader`/`encode_request`/`parse_response`；`classify`/`redact`（T9/T10 用）。

- [ ] **Step 1: 写失败测试**。
- [ ] **Step 2: `cargo test --release -p oj-db-jdbc` Expected: FAIL**。
- [ ] **Step 3: 实现**。
- [ ] **Step 4: `cargo test --release -p oj-db-jdbc` Expected: PASS**。
- [ ] **Step 5: Commit**——`feat(jdbc): NDJSON 编解码 + 错误归类与凭据脱敏`。

### Task 9: Rust SidecarMgr + 假 java 夹具

**Story（BDD）**: Given 一个假 java 进程（测试二进制自重现，回显固定 NDJSON 行），When SidecarMgr 首次请求，Then 惰性 spawn + 校验 hello（proto 不匹配报两侧版本）；When 假进程 stderr 喷 >64KB，Then 请求仍正常（drain 生效）；When 假进程崩溃，Then 在途请求收明确错误、全部 handle 立即失效、下次请求触发重启且 req_id 不归零。
**INVEST**: I=依赖 T7/T8 的纯接口 · N=内部并发结构可议 · V=进程管理是整个插件的最高危面 · E=一个模块 · S=中 · T=夹具跨平台（Windows CI 可跑）。

**Files:**
- Create: `plugins/oj-db-jdbc/src/sidecar.rs`——`pub struct SidecarMgr`（`request(op: &str, payload: serde_json::Value) -> FfiFuture 化的异步句柄`或返回 `impl Future`；内部：惰性 spawn（java 解析顺序 cfg→JAVA_HOME→PATH）、hello 同步校验、全局单调 `AtomicU64` req_id、stdin 写任务（unbounded mpsc 消费，非阻塞投递）、stdout 读任务（FrameReader 分派 pending oneshot，receiver 已 drop 的 send 失败静默）、stderr 常驻 drain（转发 HostContext log）、死亡→在途全 Err("sidecar lost") + handle 世代失效 + 下次重启）
- Modify: `plugins/oj-db-jdbc/src/lib.rs`——静态 `OnceLock<SidecarMgr>`（init 期注入 cfg）
- Test: `plugins/oj-db-jdbc/src/sidecar_tests.rs`——**假 java = 测试二进制自重现**：`std::env::current_exe()` + 环境变量 `OJ_FAKE_JAVA=1` 进入夹具模式（打印 hello、按输入回显固定响应、可模拟 stderr 洪水/崩溃）；用例覆盖上述全部场景 + 干净 EOF 退出

**Interfaces:**
- Consumes: T7 cfg 形状、T8 codec/errmap。
- Produces: `SidecarMgr::request(&self, op: &str, payload: Value) -> Result<Value, String>`（T10/T11 唯一依赖）。

- [ ] **Step 1: 写失败测试**（夹具模式 + 五场景）。
- [ ] **Step 2: `cargo test --release -p oj-db-jdbc` Expected: FAIL**。
- [ ] **Step 3: 实现**。
- [ ] **Step 4: `cargo test --release -p oj-db-jdbc` Expected: PASS**（含 stderr 洪水与崩溃场景）。
- [ ] **Step 5: Commit**——`feat(jdbc): SidecarMgr（惰性 spawn/hello 校验/stderr drain/崩溃世代失效）`。

### Task 10: vtable 胶水 — connect/query/exec/close/tx

**Story（BDD）**: Given 假 java 在线，When 经 vtable `connect` → `query` → `begin` → `tx_exec` → `tx_rollback` → `close`，Then 每步帧序列与 spec §5.3 一致；`close` 在在途请求时非阻塞返回，disconnect 排队在途 op 之后。
**INVEST**: I=薄胶水 · N=无 · V=vtable 是插件存在的意义 · E=lib.rs 填充 · S=中 · T=假 java 夹具断言帧序列。

**Files:**
- Modify: `plugins/oj-db-jdbc/src/lib.rs`——handle 注册表（`AtomicU64` handle + `DashMap<u64, HandleEntry{conn_id, generation}>`；generation 对应 sidecar 世代，世代不匹配→Err(reconnect)）；`connect`（parse_dsn → SidecarMgr `connect` → 存句柄）；`query`/`exec`（jsint 标记先行识别：`marker_i64`→JSON 数字透传为 i64、`marker_u64`→字符串化 BigDecimal 形状——协议层约定 u64 以十进制字符串传 Java，`RowCodec` 按整数字面量 BigDecimal 绑）；tx 五槽转发；`close` 同步槽：非阻塞 mpsc 投递 disconnect，立即返回
- Modify: `java/.../RowCodec.java`——字符串参数若以数字形态且带 `"$oj$u64:"` 前缀协议标记→`setBigDecimal`（与 Rust 侧约定对齐，写进帧契约注释）
- Test: `lib_tests.rs`（镜像 oj-db-mysql 风格；假 java 断言帧序列 + jsint 用例）

**Interfaces:**
- Consumes: T7/T9 全部。
- Produces: 完整 vtable（stream 四槽除外）。

- [ ] **Step 1: 写失败测试**。
- [ ] **Step 2: `cargo test --release -p oj-db-jdbc` Expected: FAIL**。
- [ ] **Step 3: 实现**。
- [ ] **Step 4: `cargo test --release -p oj-db-jdbc` Expected: PASS**；`cargo xtask plugin oj-db-jdbc --check`。
- [ ] **Step 5: Commit**——`feat(jdbc): vtable connect/query/exec/tx/close 胶水 + jsint 大整数通道`。

### Task 11: vtable 胶水 — stream 四槽

**Story（BDD）**: Given 假 java 在线且有 250 行数据，When `stream_open` → 循环 `stream_next`，Then 收到 100/100/50 行 + done 信封；When `stream_cancel` 后 `stream_next`，Then 收到 cancelled 错误；`stream_close` 幂等。
**INVEST**: I=依赖 T10 骨架 · N=无 · V=能力串承诺 · E=四槽转发 · S=小 · T=夹具断言。

**Files:**
- Modify: `plugins/oj-db-jdbc/src/lib.rs`——stream 注册表（stream_id → conn_id）；四槽按 spec §5.6 转发；`{"error":"cancelled"}` → Err 映射
- Test: `lib_tests.rs` 增 stream 场景

- [ ] **Step 1: 写失败测试**。
- [ ] **Step 2: `cargo test --release -p oj-db-jdbc` Expected: FAIL**。
- [ ] **Step 3: 实现**。
- [ ] **Step 4: `cargo test --release -p oj-db-jdbc` Expected: PASS**。
- [ ] **Step 5: Commit**——`feat(jdbc): stream_open/next/cancel/close 四槽`。

### Task 12: H2 真实 JVM 冒烟（env-gated jar）

**Story（BDD）**: Given 本机有 java 且已构建 `oj-jdbc-bridge.jar`（`OJ_JDBC_BRIDGE_JAR` 指定，缺失 skip），When 插件经 SidecarMgr 连 `jdbc:h2:mem:` 并跑 query/事务/stream，Then 全链路（真 JVM + 真 NDJSON 管道）通过 fixture 类型往返。
**INVEST**: I=依赖 T11 完成 · N=无 · V=首个真实进程级冒烟，任何人可复现 · E=一个测试文件 · S=中 · T=env-gated，缺 jar/java 诚实 skip。

**Files:**
- Create: `plugins/oj-db-jdbc/tests/h2_smoke.rs`——H2 jar 由 `OJ_JDBC_H2_JAR` env 给出（文档指引 Maven 中央下载），放入临时 drivers 目录；覆盖 connect/query 全类型往返（对照 `fixtures/types.json`）/begin/rollback/stream 批边界/cancel/close
- Modify: `plugins/oj-db-jdbc/fixtures/types.json`——若 T4 后 Rust 侧未消费，本任务补 Rust 消费断言（DRY 兑现）

**Interfaces:**
- Consumes: T2-6 的 jar、T11 的插件。

- [ ] **Step 1: 写测试**（skip 原因打印获取指引）。
- [ ] **Step 2: `./gradlew shadowJar` 后 `OJ_JDBC_BRIDGE_JAR=... OJ_JDBC_H2_JAR=... cargo test --release -p oj-db-jdbc --test h2_smoke` Expected: PASS（本机验证）。
- [ ] **Step 3: Commit**——`test(jdbc): H2 真 JVM 冒烟腿（env-gated，开箱可复现）`。

### Task 13: xtask 归置 + plugin-matrix CI 腿

**Story（BDD）**: Given `cargo xtask build`，When 构建完成，Then `bin/plugins/<triple>/` 下同时存在 `oj-db-jdbc.so`、`jdbc/oj-jdbc-bridge.jar`、`jdbc/drivers/`（空目录带 README）；`cargo xtask plugin oj-db-jdbc --check` 缺 jar 时清晰报错。
**INVEST**: I=独立构建面 · N=无 · V=发行形态闭环 · E=xtask 小改 + yml 一腿 · S=小 · T=本地构建断言 + CI 绿。

**Files:**
- Modify: `tools/xtask/src/main.rs`——第一方插件清单加 `db-jdbc`；构建后从 `java/jdbc-bridge/build/libs/` 拷 jar 到 `bin/plugins/<triple>/jdbc/`（缺 jar 清晰报错并提示先 `./gradlew shadowJar`）；`--check` 增加 jar 存在性预检
- Modify: `.github/workflows/plugin-matrix.yml`——Java 腿：`actions/setup-java`（Java 11，自带 `cache: gradle`）+ `./gradlew shadowJar` 后跑 workspace 测试
- Test: `cargo test --release -p xtask` 既有契约用例 + 本地 `cargo xtask build` 目检

- [ ] **Step 1: 修改 + 本地构建验证**。
- [ ] **Step 2: `cargo test --release -p xtask` + `cargo clippy --release --workspace --all-targets -- -D warnings` Expected: PASS**。
- [ ] **Step 3: Commit**——`build(jdbc): xtask 归置桥 jar + plugin-matrix Java 腿`。

### Task 14: e2e — 达梦远程主腿 + 金仓可选腿

**Story（BDD）**: Given `OJ_TEST_JDBC_DM_URL/USER/PASSWORD`（远程达梦实例）与驱动 jar（`OJ_TEST_JDBC_DM_DRIVER_JAR`），When 跑 e2e，Then 通过前置探活（fetchSize/cancel 支持、CASE_SENSITIVE 探测）后执行全用例链；Given 金仓 env 就绪，Then 同链跑 kingbase8。
**INVEST**: I=依赖 T12 模式 · N=用例集可增 · V=企业库实测是立项初衷 · E=一个测试文件 · S=中 · T=env-gated 诚实 skip。

**Files:**
- Create: `plugins/oj-db-jdbc/tests/e2e_dm.rs`——探活（不支持 stream/cancel→skip 打印原因；CASE_SENSITIVE→skip 原因打印）；用例链：connect → DDL（事务外！）→ insert 含 null 列断言（验证 setNull(VARCHAR)）→ 整数列回 number（验证 scale 分流）→ 空串断言 null 或剔除 → 时间值 UTC 口径 → begin/insert/rollback → stream 批边界 → cancel → db.export 路径 → db.import structured unsupported（T1 契约）
- Create: `plugins/oj-db-jdbc/tests/e2e_kingbase.rs`——env 就绪前整体 skip
- Modify: `sample/config.yaml` + `sample/src/`——达梦样例（env-gated 注释）

**Interfaces:**
- Consumes: 全部前序任务。

- [ ] **Step 1: 写测试**（缺 env 各自 skip + 获取指引）。
- [ ] **Step 2: 本机对远程达梦实例实跑**（凭据由维护者 env 注入）Expected: PASS；金仓 env 缺失确认 skip 输出。
- [ ] **Step 3: Commit**——`test(jdbc): 达梦远程 e2e 主腿 + 金仓可选腿`。

### Task 15: 文档四件 + jdbc.md + sample + CHANGELOG 定稿

**Story（BDD）**: Given 一个新用户/交付工程师，When 按 `docs/plugins/jdbc.md` 操作，Then ≤10 分钟完成首个查询（H2 开箱腿可全程复现）；Given 老用户迁移构造器代码，When 查 SKILL.md 陷阱表，Then 找到 jdbc 后 unsupported 的正解。
**INVEST**: I=纯文档 · N=结构可议 · V=交付摩擦是产品评审头号问题 · E=按 spec §4 硬性清单 · S=大但机械 · T=devkit 契约用例 + 人工走查。

**Files:**
- Create: `docs/plugins/jdbc.md`——spec §4 硬性内容全量（checklist/全程步骤/ENC 范式醒目框/错误文案排障表/限制节含多实例配方/厂商 jar 获取指引表含许可自负/.so+jar 锁死升级须知/db.export 正面说明）
- Modify: `docs/devkit/api-manual.md`——错误/限制表加 jdbc 行（placeholder_q、方言 "jdbc"、stream/cancel）
- Modify: `docs/devkit/SKILL.md`——陷阱速查加 4 行（dialect 恒 "jdbc"、构造器不可用改 db.query、DECIMAL scale 语义、`''`→null、信封内嵌 ENC 不解密）
- Modify: `docs/devkit/scenarios.md` + `docs/devkit/README.md`——可照抄场景（达梦连接 + 参数查询）与版本同步说明
- Modify: `CHANGELOG.md`——「下一版（未发布）」改为 `v0.1.141（YYYY-MM-DD，未打标签）` 并充实条目
- Run: `cargo xtask build`（归置 devkit）+ `cargo test --release -p xtask`（契约校验）

- [ ] **Step 1: 写文档**。
- [ ] **Step 2: `cargo xtask build && cargo test --release -p xtask` Expected: PASS**。
- [ ] **Step 3: Commit**——`docs(jdbc): jdbc.md + devkit 四件 + CHANGELOG v0.1.141`。
- [ ] **Step 4（发版点，需用户确认后执行）**: 打标签 `git tag -a v0.1.141 -m "v0.1.141: 泛型 JDBC 插件 oj-db-jdbc"` 并推送。

---

## 阶段小结（执行时逐阶段填写）

- [x] **阶段 0 小结**（T1 后填）：T1 一行门控 + 防回归测例落地。RED 阶段实测 panic 形态为进程级 SIGABRT（V8 不可 unwinding 回调），比设计预估的「任务卡死」更严重，证实补漏优先级正确；Cargo.lock 同步漏纳已在 3edb444 闭环。
- [x] **阶段 1 小结**（T2-6 后填）：Java 桥 T2-T6 全绿。骨架→绑定→类型映射→事务→流式/并发逐任务 TDD+评审（T2/T4 各 1 修复轮：DriverShim 假绿修复、浮点截断+fixture 边界）。帧契约、golden fixture（含整数边界/u64/负数行）、per-conn FIFO、cancel 唯一性落地；H2 下 next-cancelled 分支不可达的局限已登记待 T14 达梦腿覆盖。
- [x] **阶段 2 小结**（T7-11 后填）：Rust 插件 T7-T11 全绿（T9 1 修复轮：判死世代闸竞态——opus 复审逐点核毕）。crate 骨架→编解码/错误映射→SidecarMgr（惰性 spawn/三路管道/世代失效/跨平台假 java 夹具）→vtable 五族胶水+jsint 双端大整数通道→stream 四槽。T10/T11 一次过各挂账 Minor；cancelled 映射取向经评审亲读宿主 ffi.rs 验证为正确方向。
- [x] **阶段 3 小结**（T12 后填）：H2 真 JVM 冒烟实跑绿，途中抓到并根因修复真 bug×1——spawn 原把 drivers 放上 -cp 且未传 args[0]，真 JVM 路径死路被假 java 腿掩盖（T2 复盘预言的假绿形态兑现）。fixture 纪律（driversDir-only）与 golden 往返（19 列+$oj$ 标记）落地。
- [x] **阶段 4 小结**（T13 后填）：xtask 归置桥 jar + drivers 目录 README + --check jar 预检 + plugin-matrix/release Java 腿（1 修复轮：setup-java 11→21 因 Gradle 9 wrapper 需 JVM≥17——brief 规格错误，release.yml 步补 shell: bash）。CI yml 语义未经真实 runner 验证为残余不确定性，首轮矩阵跑需留意。
- [x] **阶段 5 小结**（T14 后填）：达梦远程 e2e 主腿对真实例实跑绿（1.8s 零残留），金仓腿诚实 skip。旗舰裁决：真·在途 cancel 在 DM 8.1.1.49 结构性不可达（三重证据）→R7 信封保持 fake-java 覆盖、严格断言分支就位；err-吞咽分支源码裁决为死代码（Session.streamClose 唯一摘除者）。顺带钉死 DM 空串=AsEmpty、values 组数须同构。另完成方言验证侧记：DM 不支持 INSERT IGNORE/ON DUPLICATE KEY/RETURNING，支持 MERGE INTO/序列/@@IDENTITY——builder 级 dameng 方言是第四种真实形态，列未来工作。
- [ ] **阶段 6 小结**（T15 后填）：

## Self-Review 记录

- spec 覆盖：§5.1 sidecar 生命周期→T9；§5.2 DSN/cfg→T7；§5.3 协议→T2/T3/T5/T6 + T10；§5.4 类型映射→T3/T4 + T12/T14；§5.5 方言/门控→T1/T7；§5.6 事务流式→T5/T6/T11；§5.7 jar 目录→T13/T15；§5.8 错误→T8；§5.9 多厂商→T15 文档。无缺口。
- 类型一致性：`ParsedDsn`/`SidecarMgr::request`/`RowCodec.bind/cell`/`classify/redact`/`FrameReader` 全计划唯一命名，跨任务引用一致。
- Review Focus 五条均已锚定任务并列入对应测试步骤。
