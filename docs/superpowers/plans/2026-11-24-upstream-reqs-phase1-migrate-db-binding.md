# 上游需求四阶段实施计划（REQ-HNSW / PARSE / A4 / PREDICT）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 分四阶段闭合 oj-wiki-docs 的四条 P2 平台能力需求，每阶段独立可发布、独立评审。

**程序总览**（评审结论见会话，2026-11-24）：

| 阶段 | 需求 | 平台侧改动 | 预估 |
|---|---|---|---|
| **1（本计划）** | REQ-HNSW | 模块迁移按 `manifest.db` 绑定路由到命名连接 | 4-6 人日 |
| 2 | REQ-PARSE | bridge `sql.parse` op（vendored node-sql-parser + 血缘 walker） | 12-18 人日 |
| 3 | REQ-A4 | oj-db-jdbc 扩 7 方言（driver jar 物流即刻并行启动） | 12-20 人日 |
| 4 | REQ-PREDICT | oj-predict 泛型轴插件 + consent/dataScope 门面 + 审计 | 8-12 人日，合规前置 |

阶段 2-4 各自另写计划文档。A4 的 driver jar 分发链调查不占本计划。

---

# Phase 1：模块迁移按 manifest.db 路由到命名连接

**Goal:** `oj migrate` / `--with-seed` / fixture 按模块 `manifest.yaml` 的 `db:` 绑定把迁移打到对应命名连接，默认绑定模块行为与 v0.1.141 完全一致。

**Architecture:** 装配层（`slim()`）已持有全部命名连接 `dbs` 映射；现状缺陷是选单一 target 后把全部模块迁移打到它。改为：装配期解析每模块 manifest 的 `db` 绑定（`None` → `"default"`），校验绑定库已在 config `db:` 段声明（fail-fast，点名模块），按绑定分组，逐组对其连接执行迁移/seed/fixture。`--db X` 过滤为只跑绑定到 X 的组；组为空报错不误跑其它模块。

**Tech Stack:** Rust（oj crate），复用 `manifest::parse_one` / `Manifest.db`（§5.3 已存在），测试用双 sqlite 命名库（免容器）。

**Spec:** `docs/UPSTREAM-REQS.md` §3.4（REQ-HNSW）+ 评审裁决（DBA：迁移 DDL 进版本链由平台 migrate 承载；R&D：命名连接映射是真实缺口）。

## Global Constraints

- 所有测试/检查一律 `--release`；禁裸 `cargo test` / `cargo clippy`。
- 动态标识符只来自 SchemaRegistry——本计划不触碰该红线（迁移走既有 ledger/DDL 路径）。
- 版本发布 ritual：CHANGELOG「下一版（未发布）」累积；devkit 四件仅在有用户可见变更时同步（本阶段有 CLI 行为变更 → 需评估）。
- 提交尾行：`unix@vip.qq.com ai`（裸行，无 Co-Authored-By）。
- 异步测试 `tokio::test(flavor = "current_thread")`；条件等待一律 deadline 轮询，禁固定 sleep。

## Review Focus

1. **dist 版本目录形态**（`<module>-<version>/manifest.yaml` 是否存在）—— release 构建产物里 manifest 不在模块目录时分组解析会全灭；合理期望：dist 树与 dev 树行为一致。→ Task 2 用既有 dist 夹具固化。
2. **模块无 manifest.yaml**——发现期是否允许无 manifest 的模块存在；若允许，绑定解析不得 panic。→ Task 1 固化：缺 manifest 按 default 绑定（与发现逻辑一致），解析失败才 fail-fast。
3. **`--db X` 与种子/fixture 的组合**——seed 重放与 fixture 必须跟随迁移走同一路由，否则数据灌错库。→ Task 3、Task 4 各固化。
4. **baseline 模式**——`--baseline` 记账也必须落在绑定库（账本 `_oj_migrations` 的 module 列在绑定库里）。→ Task 2 固化。
5. **报错文案可定位**——fail-fast 必须点名模块名与缺失的库名，否则多模块场景无法排障。→ Task 1 固化。

## BDD 用例（INVEST，测试工程师评审已回）

评审结论：**修订后可进入实施**。已采纳的修订：S7「与 v0.1.141 逐字一致」改为行为等价断言
（人读输出不是冻结契约）；补 S8 跨组部分失败（P0）、reconcile 路由（P0）、S1 幂等/同库多模块、
`--module × --db` 空组、baseline 落库。前置核查结论：真实 `oj build` 产物含模块目录
`manifest.yaml`（夹具已复刻）；`manifest::discover` 不允许无 manifest 模块——「缺 manifest
→ default 绑定」是死路径，已删除，只保留「malformed → fail-fast 点名」。

- **S1 按绑定路由**：Given config 声明 `default`+`vectors` 两库、模块 `ao.manifest.db=vectors`、模块 `core` 无绑定；When `oj migrate`；Then `ao` 迁移与 `_oj_migrations` 记录落 `vectors` 库、`core` 落 `default` 库；同库多模块（ao2）共存；重跑账本零新增。
- **S2 --db 过滤**：When `oj migrate --db vectors`；Then 只应用 `ao`，`default` 库零变更（表与账本皆无）。
- **S3 绑定库未声明 fail-fast**：Then 装配期报错同时包含模块名与库名两子串，所有库零变更。
- **S4 --db 空组**：`--db analytics` 已声明但无绑定模块 → 报错含库名，不误跑其它模块。
- **S5 seed 跟随路由**：`--with-seed` 种子行落各模块绑定库（逐库查行值）。
- **S6 fixture 同路由**：fixture 行落各模块绑定库。
- **S7 行为等价锚**：全部模块无绑定 → 全部落 default（与 v0.1.141 行为一致）。
- **S8 跨组部分失败**：vectors 组 `ao` 0002 语法错误 → 整体报错点名 ao；default 组 core 仍应用；ao 的 0001 已提交保留。
- **S9 reconcile 路由**：绑定模块的 `schema.yaml` 前向收敛 DDL 落绑定库（`pragma_table_info` 断言加列），default 库零触碰。
- **S10 baseline 落库**：baseline 账本落绑定库、不执行 SQL。

## 执行偏差与缺口登记（2026-11-24 实施记录）

1. **Task 1/2 合并提交**：`Slim` 结构变更使编译无法半途通过，bind_groups + 逐组执行同批落地（仍先红后绿：10 条新语义用例先红，实现后 41 条全绿）。
2. **启动门禁缺口（已知，后续版本）**：`server.migrate_on_start`（app.rs auto/verify）与启动期 `seed::replay_all` 只作用 default 库，不读 `db:` 绑定。尝试下沉 `bind_groups` 统一时发现 `replay_all` 的 ts/dialect 门控与 CLI 路径参数不对齐，重写风险大于收益——登记为后续任务，CHANGELOG/SKILL/scenarios 已如实披露。绑定模块 serve 前需先跑一次 `oj migrate`。
3. `bind_groups` 暂驻 `migrate_cmd.rs`（私有）；下沉 `manifest.rs` 待启动门禁一并做（避免双份定义）。

---

### Task 1+2（合并）：装配期分组 + 逐组执行迁移（含 baseline / seed / fixture / schema diff）

**Files:**
- Modify: `oj/src/migrate_cmd.rs`（`Slim`/`slim()`/`bind_groups`/`migrate_collect` 调用/`run_seed_phase`/`run_migrate`/`run_fixture`/`run_schema_diff`）
- Test: `oj/src/migrate_cmd_tests.rs`

**状态：已落地（41 用例全绿）**。实现要点：
- `bind_groups`：逐模块 `manifest::parse_one`，`db` 缺省/空 → `"default"`；绑定库未声明 fail-fast（default 与非 default 给不同修复指引文案）；`BTreeMap` 收集保证组键升序。
- `--db X`：先判声明（ghost 照旧报错），分组后 retain，空组报 `--db "x" 无绑定模块`。
- `run_migrate` 逐组 `migrate_collect`，json results 平铺（形状不变）；人读按组打印 `→ db "{key}"`（单 default 组与旧输出逐字一致）。
- `run_seed_phase` 逐组 `replay_modules`，装配期失败带 `[db key]` 前缀。
- `run_schema_diff` 语义升级为逐组对账（绑定模块与自己的库比对，消除跨库噪音漂移）。
- 既有用例语义迁移：`db_flag_targets`（manifest 补 `db: test`）、`db_flag_edge_cases`（缺 default 改绑 fail-fast 断言）、`dist_tree` 补 manifest.yaml（复刻真实 oj build 产物）。

### Task 3/4：seed/fixture 路由 + 发布 ritual

**状态：已落地**。devkit 四件已同步（api-manual `--db` 语义段、SKILL 三行重写 + 启动门禁缺口陷阱、
scenarios 场景 6 重写）；CHANGELOG「下一版（未发布）」已记录；`cargo test --release -p xtask`
契约校验通过即 bin/devkit 归置完成。待办：workspace 全量 clippy、提交、架构师/工程师评审。

### 后续任务（不在本计划）

- **启动门禁绑定路由**：`apply_all`/`verify_all`/`reconcile_all`/`replay_all` 逐组化（需先解决
  `replay_all` 的 ts 参数不对齐），`bind_groups` 同步下沉 `manifest.rs` 复用。
