# REQ-A4 Phase 3：oj-db-jdbc 七方言扩展 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 7 种非原生数据源（Snowflake / Databricks / ClickHouse / Trino / Kyuubi / HeavyDB / DuckDB）经 oj-db-jdbc 泛型 JDBC 桥完成「连接 → 查询 → 元数据摄取」全链可用，jar 分发链可复制（随包三件套一键归位 + 自备指引），上游 `POST /catalog` 摄取验收项有平台侧对照。

**Architecture:** Java 侧零改动（DriverManager 按 URL 前缀自路由 + DriverShim 隔离，无方言知识）；Rust 侧仅 `dsn.rs::vendor_of()` 诊断表加 7 臂（不影响 `dialect()` 恒 `"jdbc"` 门控语义）；工作量大头在 jar 分发物料（xtask 下载命令 + 文档矩阵）与可验证性（DuckDB 离线全链腿 + 4 个驱动装载 smoke 腿）。

**Tech Stack:** Rust（oj-plugin-ffi 契约、xtask）、Java 11 桥（不动）、Maven Central pinned jar。

**Spec:** `docs/UPSTREAM-REQS.md` §3.1（REQ-A4 五条验收标准）；driver 分发链调查报告全文在
`/tmp/a4-driver-report.md`（会话产物，持久化裁决以本计划与本仓 `docs/plugins/jdbc.md` 为准）。

## Global Constraints

- 版本分界递增 `oj/Cargo.toml` → **0.1.144**（本阶段发布点，未打标签待确认）。
- devkit 四件（api-manual / scenarios / SKILL / README）与本版用户可见变更逐条对齐（CLAUDE.md 红线）。
- 凭据只走 env / `ENC[...]`，测试实例凭据**不进仓库**（沿用 e2e_dm/e2e_kingbase 模式）。
- 所有测试/clippy/build 一律 `--release`；串行跑，禁并发两个 cargo test；管道接 `set -o pipefail`。
- `dialect()` 恒返 `"jdbc"` 的门控语义**不变**（构造器/listTables/nextSeq/migrate/seed/db.import
  结构化 `OJ_DB_UNSUPPORTED`；`db.query`/`db.exec`/`db.stream`/`db.export` 照常）——REQ-A4
  验收第 4 条（sql_guard/SchemaRegistry 约束）正是靠这道门满足，不得顺手放开。
- pinned 版本字面量以本计划 Task 2 表为准（Maven Central 直接检索实证，2026-11-24）。

## REQ-A4 验收标准 → 本阶段交付对照

| 上游验收 | 平台侧交付 |
|---|---|
| 1. 七源均可摄取 | DuckDB 离线全链腿实跑；Trino/ClickHouse/Kyuubi/HeavyDB 装载 smoke 腿证明驱动集成；场景 84 给摄取元数据照抄例（`db.query` information_schema 形态） |
| 2. DSN 凭据走平台 ENC[] | 既有能力（config `db:` 段 + `oj secret seal`），文档在 jdbc.md / 场景 82 已在；场景 84 重申 |
| 3. 连通性测试端点 | 文档固化「`DB("name").query("select 1")` 即连通探针」口径（无新端点——能力已有，不造重复面） |
| 4. 受 sql_guard/SchemaRegistry 约束 | 方言恒 jdbc 门控语义不变（Global Constraints）；`db_access`/`max_rows` 照常生效 |
| 5. 多实例注册状态一致 | config 声明式（各实例同 config 天然一致）；`db.client.register` 进程内 StableState 各池 runtime 一致（v0.1.92 既有行为，文档点明） |

## Review Focus

1. **ClickHouse 错选 thin jar**（0.3MB 缺依赖 → `ClassNotFoundException`）——pin：Task 1 的
   vendor_of 对 `ch:`/`clickhouse:` 前缀映射 + Task 4 文档陷阱行写明**必须 `-all` classifier**；
   Task 3 smoke 腿用 `-all` jar 实证可装载。
2. **Kyuubi 错选 hive-jdbc thin**（171KB 依赖地狱，平面目录不可用）——pin：Task 1 vendor_of
   `kyuubi:` 前缀映射 + Task 3 smoke 腿用 shaded jar + Task 4 陷阱行。
3. **Trino 版本/JDK 漂移**（周级发版，470+ 要求 Java 11）——pin：Task 2 下载命令钉死
   484 + Task 4 指引「升级须读 release notes 并重跑 smoke 腿」。
4. **DuckDB 全量内存无流式**（`setFetchSize` 无效果，大查询顶 JVM 堆）——pin：Task 3 e2e
   只验证小结果集全链可用 + Task 4 限制行写明语义差异；不承诺流式省内存。
5. **同 UCL 先到先得**（drivers 目录同名不同版，装载顺序按文件名排序）——pin：Task 4
   分发指引「一目录一版本」+ Task 1 vendor_of 未知前缀原样透出（诊断永不静默吞）。

---

### Task 1：vendor_of 七方言诊断登记

**Files:**
- Modify: `plugins/oj-db-jdbc/src/dsn.rs`（`vendor_of()` match 表，~102-114 行）
- Test: `plugins/oj-db-jdbc/src/dsn_tests.rs`（vendor 表对应用例）

**Interfaces:**
- Consumes: 既有 `vendor_of(url: &str) -> &'static str`（未知前缀 `Box::leak` 原样透出）。
- Produces: 七个新臂的方言名——`snowflake` / `databricks` / `clickhouse` / `trino` /
  `kyuubi` / `heavydb` / `duckdb`（仅诊断日志/错误文案；`dialect()` 不变）。URL 前缀形态：
  `jdbc:snowflake:`、`jdbc:databricks:`、`jdbc:ch:` 与 `jdbc:clickhouse:`、`jdbc:trino:`、
  `jdbc:kyuubi:`、`jdbc:heavyai:` 与 `jdbc:mapd:` 与 `jdbc:omnisci:`（旧前缀同臂）、
  `jdbc:duckdb:`。

- [ ] **Step 1：写失败测试**：dsn_tests 补 7 行用例（每方言一断言，heavydb 三前缀同臂断言一次），断言 `vendor_of` 返回对应方言名。
- [ ] **Step 2：跑测试确认失败**：`cargo test --release -p oj-db-jdbc vendor`（子串匹配）。
- [ ] **Step 3：实现**：dsn.rs match 表加 7 臂（heavydb 合并三旧前缀），其余不动。
- [ ] **Step 4：测试绿** → 全量 `cargo test --release -p oj-db-jdbc`。
- [ ] **Step 5：提交** `feat(db-jdbc): vendor_of 七方言诊断臂——snowflake/databricks/clickhouse/trino/kyuubi/heavydb/duckdb`

### Task 2：`cargo xtask jdbc-drivers` 随包三件套下载命令

**Files:**
- Modify: `tools/xtask/src/args.rs`（新子命令枚举）
- Create: `tools/xtask/src/jdbc_drivers_cmd.rs`（下载 + 归置）
- Modify: `tools/xtask/src/main.rs`（接线）
- Test: `tools/xtask/tests/`（URL 构造与版本表单测，不发网络请求）

**Interfaces:**
- Produces: `cargo xtask jdbc-drivers [--dir <path>]`——把随包三件套下载进
  `<exe>/plugins/<host-triple>/jdbc/drivers/`（缺省）或 `--dir` 指定目录；打印自备四件
  （Snowflake/Databricks/DuckDB/HeavyDB）的坐标与指引。重复执行幂等（已存在的同名 jar 跳过）。

pinned 版本表（命令内单源，测试断言这些字面量）：

| jar | Maven Central URL |
|---|---|
| trino-jdbc-484.jar | `https://repo1.maven.org/maven2/io/trino/trino-jdbc/484/trino-jdbc-484.jar` |
| clickhouse-jdbc-0.11.2-all.jar | `https://repo1.maven.org/maven2/com/clickhouse/clickhouse-jdbc/0.11.2/clickhouse-jdbc-0.11.2-all.jar` |
| kyuubi-hive-jdbc-shaded-1.12.0.jar | `https://repo1.maven.org/maven2/org/apache/kyuubi/kyuubi-hive-jdbc-shaded/1.12.0/kyuubi-hive-jdbc-shaded-1.12.0.jar` |

- [ ] **Step 1：写失败测试**：版本表单测——三个 (URL, 期望文件名) 字面量断言 + `--dir` 路径解析纯函数测试（无网络）。
- [ ] **Step 2：跑测试确认失败**。
- [ ] **Step 3：实现**：版本表 const + 下载（`ureq` 或 xtask 已有 HTTP 依赖；无则 `std::process` curl 亦可，取最小依赖路径）+ 幂等跳过 + 自备指引打印。
- [ ] **Step 4：测试绿 + 实跑一次**：`cargo xtask jdbc-drivers` 真下载进 bin/ 布局（~45MB，一次性），`ls` 验证三个 jar 在位。
- [ ] **Step 5：提交** `feat(xtask): jdbc-drivers 随包三件套下载——pinned Trino/ClickHouse/Kyuubi`

### Task 3：测试腿——DuckDB 离线全链 + 四方言驱动装载 smoke

**Files:**
- Create: `plugins/oj-db-jdbc/tests/e2e_duckdb.rs`（env-gated：`OJ_JDBC_BRIDGE_JAR` +
  `OJ_JDBC_DUCKDB_JAR`；无需服务端——内存库全链）
- Create: `plugins/oj-db-jdbc/tests/e2e_driver_smoke.rs`（env-gated：桥 jar + 各方言
  `OJ_JDBC_<NAME>_JAR`；对不可达端点 connect，断言**结构化 connect 错误**——驱动类完成
  ServiceLoader 注册 + Shim 集成即算过，不需要真服务端）

**Interfaces:**
- Consumes: e2e_kingbase.rs 的 env-gated 样板（skip 文案、jar 路径注入、vtable 装配）。
- Produces: 四个 smoke 腿各 ~20 行（trino / clickhouse / kyuubi / heavydb），heavydb 腿
  文案明示「上游已废弃，仅验证 jar 可装载」。

- [ ] **Step 1：写 DuckDB 失败腿**：connect 内存库 → CREATE TABLE → INSERT → `db.query` 参数查询 → 类型分流断言（scale 语义）→ `db.stream` 小结果集跑通。断言镜像 h2_smoke 结构。
- [ ] **Step 2：跑（缺 jar 即 skip 确认）→ 下载 jar（Task 2 命令或手工 Maven URL：`https://repo1.maven.org/maven2/org/duckdb/duckdb_jdbc/2.0.1.0/duckdb_jdbc-2.0.1.0.jar`）→ 跑绿**。
- [ ] **Step 3：写 smoke 腿**：四方言 URL 常量（`jdbc:trino://127.0.0.1:1/` 等必不可达端口）→ connect → 断言错误信封含驱动注册证据（`No suitable driver` **不得**出现——出现即 ServiceLoader 未集成）。jar 缺席即 skip。
- [ ] **Step 4：跑绿（备 jar 腿全过）**。
- [ ] **Step 5：提交** `test(db-jdbc): DuckDB 离线全链腿 + 四方言驱动装载 smoke 腿`

### Task 4：jdbc.md 分发矩阵与自备指引

**Files:**
- Modify: `docs/plugins/jdbc.md`（新节「七方言 jar 分发矩阵」：随包三件套表 + 自备四件表
  + drivers_dir 指引 + 一目录一版本 + HeavyDB 上游废弃声明 + DuckDB 内存语义限制行）

**Interfaces:**
- Consumes: Task 2 版本表（写坐标时与 xtask 表逐字一致）；调查报告第 3 节矩阵。
- Produces: 用户侧唯一权威分发文档（devkit 四件在 Task 5 引用它，不重复矩阵）。

- [ ] **Step 1：写矩阵节**（表列：方言 / 坐标:版本 / jar 体积 / 许可证 / 最低 JDK / 驱动类 / URL 形态 / 分发判定）。
- [ ] **Step 2：自备指引四步**（建目录 → config `drivers_dir` → DSN 信封 → 重启进程），照抄形态。
- [ ] **Step 3：提交** `docs(jdbc): 七方言 jar 分发矩阵与自备指引`

### Task 5：发布 ritual（devkit 四件 + CHANGELOG + bump 0.1.144 + 全门禁 + 评审）

**Files:**
- Modify: `docs/devkit/api-manual.md`（jdbc 节：七方言覆盖声明 + vendor 诊断名进错误文案 +
  限制表 DuckDB 内存行；验收对照第 3/5 条口径——连通探针与一致性——写入对应位置）
- Modify: `docs/devkit/SKILL.md`（陷阱速查 5 行：ClickHouse 非 -all / Kyuubi 非 shaded /
  Trino 钉版本 / DuckDB 内存语义 / 一目录一版本）
- Modify: `docs/devkit/scenarios.md`（场景 84：DuckDB 离线元数据摄取——`db.query`
  information_schema 形态照抄例 + 索引行）
- Modify: `docs/devkit/README.md`（三行能力条款 + v0.1.144 已落地段）
- Modify: `CHANGELOG.md`、`oj/Cargo.toml` + `Cargo.lock`（0.1.144）
- Test: xtask devkit 契约（自动，`cargo xtask build` 后）

- [ ] **Step 1：docs 四件逐条对齐**（错误文案与实现逐字一致；场景 84 可离线照抄——DuckDB 内存库零服务端）。
- [ ] **Step 2：CHANGELOG + bump 0.1.144**（未打标签，tag 等用户确认）。
- [ ] **Step 3：全门禁**：`cargo test --release -p only-js`、`-p oj`、`-p oj-db-jdbc`、`-p xtask`、`cargo clippy --release --workspace --all-targets -- -D warnings`、`cargo fmt --check`（串行）。
- [ ] **Step 4：提交** `chore(release): v0.1.144 版本分界——jdbc 七方言文档与 CHANGELOG`
- [ ] **Step 5：评审循环**：架构师 + 工程师 + 测试工程师（BDD 对照）三路评审 → must/should-fix 修订提交 → 计划文档记录执行偏差 → 向用户分阶段总结。

### 后续任务（不在本计划）

- 上游 `POST /catalog` 端点本身的摄取编排属下游仓改造；平台侧以「七源可连接 + 元数据查询
  可跑」由本计划测试腿与场景 84 保障（同 Phase 2 的 golden 契约保障模式）。
- UCL-per-driver 隔离（同名不同版共存）是 spec 级变更，本阶段明确不做（调查报告风险 5）。
