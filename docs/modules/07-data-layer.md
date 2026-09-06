# 07 · 模块数据层（`oj/src/{manifest,schema,migrate,seed,checks}.rs`）

一个模块自己拥有数据层：`manifest.yaml`（身份与声明）、`schema.yaml`（声明式表结构）、
`migrations/*.sql`（手写 DDL 演进）、`seed.sql` / `fixtures/`（数据）。
**声明为源**，安全前向 DDL 由 `reconcile` 推导。

## 1. `manifest.rs` —— 模块清单

```yaml
# src/<module>/manifest.yaml
name: user          # 必须等于目录名（启动期强约束 S001）
version: 0.1.0
desc: 用户模块
config: {}          # 可选：模块私有的任意 YAML（serde_yaml::Value，暂未消费）
tables: [account]   # 有表模块必配（与 schema.yaml 双向一致，S005）
deps: { _platform: "^0.1.0" }   # 跨模块表访问声明（归属守卫依据 + S004 存在性）
db: default         # 可选：模块的 "default" 库重定向到该命名库
```

| 函数 | 职责 |
|---|---|
| `validate_module`（:25） | 模块名白名单 `[A-Za-z0-9_-]`，禁路径字符与 `..`（进路径拼接，信任边界） |
| `validate_version`（:37） | `[A-Za-z0-9.]`，拒连续点（兼容 `0.1.0-beta`） |
| `parse_one`（:50） | 单个 manifest 解析 |
| `load_lock`（:57） | `dist/manifests.yaml`（模块 → 锁定版本）。**缺失 = 空表（首次构建合法）；坏锁 = Err**（不得被当空表静默重置） |
| `save_lock`（:67） | 原子写（tmp + rename）。⚠️ 多进程并发构建的读-改-写竞争不做锁（已知 ceiling） |
| `load_modules`（:78） | 首层全部模块 + `name == 目录名` 校验 |
| `discover`（:113） | dev = 首层子目录；release = 锁 `{m: v}` → `<dir>/<m>-<v>/`。返回按模块名排序 |

## 2. `schema.rs` —— 声明式表结构（§4.2 / D1=C）

列类型最小集：`integer | bigint | text | boolean | double | blob`（其余走手写 migrations）。
标识符白名单 `[A-Za-z_][A-Za-z0-9_]*`（`is_ident`，:103）。

- `create_table_ddl` / `add_column_ddl` / `create_index_ddl`（:212-230）：三方言渲染
  （sea-query 的 builder 方法泛型非 dyn 兼容，故用宏展开同款 match，同 `query.rs`）。
- **`reconcile`（:326）安全前向收敛**（幂等）：缺表 CREATE、缺**可空**列 ALTER ADD、
  缺索引 CREATE INDEX；返回执行日志（空 = 已收敛）。
  无法安全推导的一律 **Err + 打印迁移模板**：NOT NULL 列新增、疑似改名（缺新列 + 多旧列）。
- **`diff`（:386）只读对账**（`oj schema diff`，§5.1 漂移层）：
  D001 缺表 / 缺列 / 多列 / 缺索引；D002 实库有而无任何模块声明（排除 `_oj_migrations%`
  与 `sqlite_%`）。类型漂移不比对（方言反查长尾，人工核）。有差异 → 报告 + 退 1。
- `registry_tables()` 把同一份声明喂给 `SchemaRegistry`（归属图 + 列白名单，§4.8）。

## 3. `migrate.rs` —— 迁移引擎（spec §11.2，D4）

基于 **refinery-core**：`OjConn` 把 `Arc<dyn DataAccessor>` 包进
`AsyncTransaction` / `AsyncQuery`；契约只吃 SQL 字符串，跨得过 DataAccessor 边界。

- **账本**：每模块一张 `_oj_migrations_<module>`（`ledger_name`，:276），version 模块内从 1 起。
- **文件名**：`{seq:04}__{desc}[.{dialect}].sql`；`desc` 限 `[A-Za-z0-9_]`。
- `load_migrations`（:155，S007）：同 seq 有方言覆盖文件（`0001__init.pg.sql`）时按当前
  Dialect 只取其一，否则回落通用；**seq 必须 1..=n 连续**（空洞/乱序/重复 → S007）；
  BOM 剥离 + CRLF→LF 在此规范化。
- `run_module`（:249）：`abort_divergent` / `abort_missing = true`，`grouped = false`
  （mysql DDL 隐式提交下 grouped 必裂）。
- **并发锁**：pg 用 `pg_advisory_xact_lock`（`lock_id_of` = 模块名 FNV-1a 64 → i64）；
  mysql 靠账本 version 主键冲突兜底。
- `apply_module`（:290）/ `verify_module`（:314）/ `apply_all`（:368）/ `verify_all`（:397）。
- **M003/M004**（release 启动校验，§4.6）：
  M003 账本 seq 超过产物最大 seq（降版部署）→ 拒启；
  M004 存在待应用（含首启空账本）→ 拒启并给出命令。
- `--baseline` = `Target::Fake`（全量记账不执行，P0 建过表的存量库接入门）。

## 4. `seed.rs` —— 种子重放（spec P0）

- 顺序：根 `config_dir/seed.sql`（**deprecated**）→ 各模块（目录名排序，模块内
  `SEED_FILES` 顺序：`schema.sql` 结构在前、`seed.sql` 数据在后）。
- 语义：幂等 SQL、仅 `default` 库且 sqlite、`;` 朴素切分（语句内不得含分号字面量）。
- **S002**：同一张表被两处 `CREATE TABLE` → 启动 fail-fast，不静默合并。
  `create_tables`（:44）识别 `IF NOT EXISTS`、引号（`"t"`/`` `t` ``/`[t]`）与 schema 限定。
- 先全量冲突检查再执行 —— 失败不落任何副作用。
- `fixtures/` **不重放**（演示数据，由 `oj fixture` / `oj test` 灌入）。
- 无任何种子文件 → 静默返回；有种子但 default 库缺失/非 sqlite → warn 跳过。

## 5. `checks.rs` —— 结构层静态检查（§5.1）

`oj build` 内嵌全部 S*（fail build）；`oj build --check` 只校验不落盘（CI 门禁）。
**一次给全违规，不逐条 fail-fast**（CI 修复体验）。

| 规则 | 内容 | 落点 |
|---|---|---|
| S001 | `manifest.name` 必须等于目录名 | `manifest::load_modules` |
| S002 | 同一张表被多个模块声明 | `checks::run`（:35）+ `seed.rs`（根 vs 模块）+ 装配期（`app.rs:206`） |
| S003 | 模块 SQL 引用他模块表但未声明 `deps` | `checks::run`（:70 起），附 §3-D2 场景决策表 |
| S005 | `manifest.tables` 与 `schema.yaml` 双向一致 | `checks::run`（:49） |
| S007 | 迁移文件 seq 连续性 / 命名 / 方言覆盖 | `migrate::load_migrations` |

- 报错三要素（硬性要求）：**违规文件路径（+规则 ID）、原因（引用具体声明）、下一步动作**。
- SQL 表名提取与运行时守卫**同一实现**（`bridge::guard::extract_tables`，§5.3 轻量扫描口径）。
- `/* oj:allow-table=a,b */` 赦免名单（`filter_allowed`，:274）——误报逃生门。
- `.ts` 源码先抠 JS 字符串/模板串内容（`js_strings`，:292）再提取（词法器对纯 SQL 跳
  字符串字面量，对源码恰要取串内 SQL —— 两口径）。

## 6. 归属守卫（运行时）

- 装配期把 `ModuleCtx { name, deps, db }` 按**模块目录绝对路径**存入 `StableState.modules`；
- `Bridge::run_module` 用 api_path 的祖先目录命中 → 写入 `ReqState.module`；
- `guard.rs` 的 `check_raw` / `check_table` / `bound_db` 据此判定：
  跨表访问未在 `deps` 声明 → `warn`（默认）或 `deny`（`server.ownership_guard: deny`）。

## 7. 已知债

- `manifest::save_lock` 无并发锁（多进程同时 `oj build` 有竞争），已标注 ceiling。
- 根 `seed.sql` 处于 deprecated 并存期，仅靠告警文案引导迁移。
- `checks.rs` 的规则号不连续（S004/S006 未在本文件实现或已并入他处），新人容易困惑；
  建议在文件头补一张「规则 → 落点」表（本表即为补齐）。
