# Auto-Ontology 上游能力需求（致 oj 平台团队）

> 文档性质：面向 oj 平台的能力需求清单，**提需求、非绕过**。
> 来源：oj-wiki-docs 仓库 Auto-Ontology 模块对齐参考实现过程中识别的平台能力缺口。
> 日期：2026-11-23。四条需求均按六段模板编写，上下文自治，可独立评审。
> 阅读对象：**不了解本项目内部结构的 oj 上游评审者**。因此第 1 节先交代背景与术语，第 3 节每条需求自带背景、证据、举例与验收口径。

---

## 1. 背景与术语（给不了解本项目的评审者）

### 1.1 本项目是什么

`oj-wiki-docs` 是一个基于 **oj 平台**构建的检索问答应用。其中 `src/auto-ontology/` 是核心模块，目标是把企业数据库自动构建成可问答的"本体"：

```
数据源目录摄取 → 语义层（表/列/术语/关系）→ 检索增强问答（Text-to-SQL / 信息型）
→ 编译质量层（逐表/跨表 pass）→ 向量化与运维（外部 vectors 库 + 索引）
```

我们正在对齐一份参考实现 `ref/auto-ontology`（及其分析文档 `ref/docs/oj/01..08`）。对齐过程中发现若干能力只有**平台进程**能提供，模块层无论怎么改代码都补不齐——这正是本文档要提的需求。

### 1.2 模块运行模型（为什么很多事模块内做不了）

oj 模块运行在平台提供的 JS 运行时中，通过平台暴露的能力接口访问资源，关键边界如下：

| 边界 | 模块能做 | 模块不能做（需平台提供） |
|---|---|---|
| 数据库 | `db.table()`、`db.exec()` 操作平台已注册方言的库 | 注册新的 driver / dialect（如 ClickHouse HTTP、DuckDB native） |
| 凭据 | `config` 中保存 `ENC[]` 加密串 | 让平台连接管理识别并治理新方言的凭据 |
| AI 能力 | 调用平台 AI 网关 `chat` / `embed` / `rerank` | 加载外部 Python 引擎（如 KumoRFM / sqlglot）、直连外部 AI 服务 |
| 迁移 | 运行 oj migrations 管理 oj 自身库 | 对**外部 vectors 库**执行 DDL、观测索引状态 |
| SQL | 平台 `sql_guard` 自动注入租户谓词 | 绕过租户隔离（模块自建连接即越界） |

因此下文每条需求的「为何不能绕过」都指向同一原则：**能由平台一边治理的事，不应在模块内另起一套**，否则会绕过凭据治理、租户隔离、合规审计与数据面治理。

### 1.3 术语表

| 术语 | 含义 |
|---|---|
| oj 平台 / 模块运行时 | 承载 oj 模块的基础设施，提供 `db.*`、KV、AI 网关、路由、migrations 等原语 |
| Auto-Ontology / 本模块 | 本项目 `src/auto-ontology/`，负责本体构建与问答 |
| `POST /catalog` | 目录摄取入口：连接数据源、抓取表/列元数据写入目录 |
| `db.*` / SchemaRegistry | 模块侧数据库访问与表结构登记；只认平台已注册方言 |
| `config DSN ENC[]` | 平台配置中的加密连接串存储格式 |
| `sql_guard` 租户谓词 | 平台层自动为 SQL 加租户过滤，保证多租户隔离 |
| `ao_configuration` | 本模块的配置表，存放角色、参数组、开关等 |
| vectors 库 / pgvector / HNSW | 外部向量库（PostgreSQL + pgvector 扩展）；HNSW 是近似最近邻索引类型 |
| oj migrations | 平台迁移链，只作用于 oj 自身库，不含外部 vectors 库 |
| ReAct / NON_REASONING 角色 | 问答循环与"非推理"模型角色（用于生成结构化产物，如 PQL） |
| AI 网关 `chat/embed/rerank` | 平台提供的模型调用面；本模块目前只有这三个 |
| PQL / KumoRFM | 预测查询语言（Predictive Query Language）与执行它的外部图预测引擎 |
| sqlglot / tokenizer / 列级血缘 | 成熟 SQL 解析器 / 手写轻量解析 / SQL 输入列到输出列的来源关系 |
| `registerSql` | 查询历史摄入：把执行过的 SQL 解析后登记血缘，用于后续召回 |
| G0/G1、SL-x.x、P0/P2 | 项目内部里程碑与阶段编号；四条需求均为 **P2**，不阻塞 G0/G1 |
| ref / `docs/oj/01..08` | 参考实现及其分析文档，是功能对齐的基线 |

---

## 2. 需求一览

| 编号 | 需求 | 级别 | 前置门槛 | 一句话 |
|---|---|---|---|---|
| REQ-A4 | 扩展连接器/方言注册 | P2 | 无 | 让 `POST /catalog` 能摄取 7 种非原生数据源 |
| REQ-PREDICT | 预测执行与合规网关 | P2 / G2 | **合规评审** | 平台受控执行 PQL，拦截并审计数据出境 |
| REQ-PARSE | 一等 SQL 解析服务 | P2 | 无 | 提供 sqlglot 级解析，替换掩码正则/手写 tokenizer |
| REQ-HNSW | 平台向量索引与维度迁移 | P2 | 无 | 平台执行 HNSW DDL 与维度迁移，免部署侧手工 psql |

四条均为**平台能力缺口**，本仓只提需求，不以绕过方案作为交付前提。

---

## 3. 需求详情

### 3.1 REQ-A4：扩展连接器/方言注册（P2）

> 上下文自治，可独立评审。对应项目内 §3.2 保留项与 SL-3.1。

- **背景**：参考实现的目录摄取支持 10 种数据源方言。oj 原生 `sqlite/mysql/pg` 三种已覆盖，另有 7 种非原生方言未接入：Snowflake、Databricks、ClickHouse、Trino、Kyuubi、HeavyDB、DuckDB。
- **现状证据**：oj 原生方言仅 `sqlite/mysql/pg`；模块的 `db.table()` 与 SchemaRegistry 只认 `schema.yaml` 与平台已注册方言；模块 `config DSN ENC[]` 能存连接串，但**没有注册第三方 driver/方言的接口**，`POST /catalog` 只能对三种原生方言做摄取。
- **为何不能绕过**：驱动与方言注册属**平台进程能力**——第三方 driver（DuckDB 的 wasm/native binding、ClickHouse 的 HTTP 协议等）需在平台进程加载并向 `db.*` 构造器注册方言；模块内无法把 driver 注入 SchemaRegistry，也无法让平台连接管理/凭据加密（`ENC[]`）识别新方言。模块内自建连接等于绕过平台凭据治理与 `sql_guard` 租户谓词，不可接受。
- **举例**：运维同学在 `/auto-ontology/catalog` 页面填入一个 ClickHouse DSN 并点击"提取数据源"，期望抓取库表元数据；当前请求会因平台未注册 ClickHouse 方言而在目录摄取阶段失败（方言无法识别、SchemaRegistry 无法登记）。注册方言后，同样的操作应能完成表/列元数据的摄取，且连接串仍由平台 `ENC[]` 托管。
- **能力定义**：平台提供面向模块的**连接器/方言注册**能力——声明式或运行期注册 driver 与 dialect，纳入平台统一连接管理与凭据加密，使模块 `db.*` 与目录摄取可透明使用新方言。
- **建议接口**：声明式 `manifest.yaml` 的 `connectors:` 段，或运行期 `connector.register({name, dialect, dsn, driver, capabilities})`；配套平台侧连通性测试与凭据加密。具体形态由上游裁定。
- **验收标准**：
  1. 7 种非原生源均可被 `POST /catalog` 完成摄取；
  2. DSN 凭据走平台 `ENC[]`、不落模块代码；
  3. 连通性测试端点可用；
  4. 注册方言受 `sql_guard` 租户谓词与 SchemaRegistry 校验约束；
  5. 多实例注册状态一致。
- **优先级**：**P2**，分期交付，不阻塞 G0/G1。当前主路径以三种原生方言 + config 命名源满足。

---

### 3.2 REQ-PREDICT：预测执行与合规网关（P2 / G2，合规评审前置）

> 上下文自治，可独立评审。对应项目内 §3.4 `predict=OUT(G2评估)` 与 SL-3.2。

- **背景**：参考实现的 `predict` 由 NON_REASONING 模型生成 PQL（Predictive Query Language），交外部 KumoRFM 引擎执行图预测。本仓已具备「NON_REASONING 角色写 PQL」的网关基座，但**无执行面**，因此在项目内被显式标为 OUT（G2 评估），没有静默丢弃。
- **现状证据**：oj 模块网关只有 `chat/embed/rerank`（见模块 `_shared/ports.ts`），无 PQL 执行原语；KumoRFM 是外部产品，需把租户业务数据送达外部引擎；项目内已标注「涉数据出境、若做需合规评审」。
- **为何不能绕过**：
  1. **执行引擎缺口**——PQL 需 KumoRFM 专用引擎，模块内 `fetch` 直连既无引擎也无协议；
  2. **合规缺口**——把租户数据发往外部预测服务属数据出境，必须有平台级 `consent` / `dataScope` 网关拦截与审计，模块内自建外呼等于绕过合规治理。
- **举例**：业务用户问"预测下个月各区域的销售额"。系统让 NON_REASONING 角色写出 PQL 后，当前没有任何平台原语能把它交给受控引擎执行；若模块自己把租户销售记录 POST 到外部 KumoRFM 服务，就构成了未经过合规网关的数据出境。期望是：平台接受这段 PQL 加上明确的数据范围与授权，在受控数据面执行，把结果回给问答链路，并留下审计记录。
- **能力定义**：平台提供**预测执行 + 合规网关**能力——接受 PQL 与数据范围/授权，在受控数据面执行并把结果回给模块，全程审计留痕。
- **建议接口**：`predict.run({pql, dataScope, consent}) → {result, trace}`，`dataScope` 限定出境数据范围、`consent` 由平台合规面校验。具体形态由上游裁定。
- **验收标准**：
  1. PQL 由平台受控引擎执行；
  2. `dataScope` 越界被拒；
  3. `consent` 未通过不执行且留审计；
  4. 结果可回传模块 ReAct 作为工具输出；
  5. 失败/超时可观测、不污染租户数据。
- **优先级**：**P2 / G2**，**合规评审为前置门槛**；本批只提需求、不写实现，不阻塞 G0/G1。

---

### 3.3 REQ-PARSE：一等 SQL 解析服务（P2）

> 上下文自治，可独立评审。对应项目内 P5-E tokenizer 增强与「validate 是掩码正则非 sqlglot」的现状。

- **背景**：参考实现用 sqlglot 做 SQL 解析、方言 DDL 处理与列级血缘。本仓 `analyses/validate` 是掩码正则，`_shared/sql-parse.ts` 是手写 tokenizer（P5-E 补了 CTE 名字收集），仅覆盖有限形状。
- **现状证据**：现有 tokenizer 只增加 `collectCteNames`，对**嵌套子查询、方言特有 DDL、列级 lineage** 仍无正确解析能力；`registerSql`（查询历史摄入）的血缘提取依赖 `extractTables` / `extractSelectColumns`，解析精度直接决定查询历史质量与后续召回质量。
- **为何不能绕过**：完整 SQL 解析需要成熟 parser（AST、方言差异、列级血缘）——手写 tokenizer 无法穷尽 SQL 语法；sqlglot 是外部 Python 产品，oj 模块运行于 JS 运行时、无 Python 执行面，模块内引依赖不可行。解析精度是检索/血缘的地基，不能靠正则近似长期掩盖。
- **举例**：一条查询历史 SQL：

  ```sql
  WITH recent AS (
    SELECT order_id, amount FROM orders WHERE created_at > '2026-01-01'
  )
  SELECT r.order_id, c.name
  FROM recent r JOIN customers c ON r.order_id = c.order_id
  ```

  当前 tokenizer 能识别 `orders`、`customers` 和 CTE 名 `recent`，但无法正确给出 `amount → r.order_id → c.name` 这类列级来源关系，也无法稳定处理嵌套子查询与方言 DDL；结果登记进 `registerSql` 的血缘不完整，影响召回。期望平台解析服务直接返回结构化的 `tables / columns / lineage / ast`。
- **能力定义**：平台提供**一等 SQL 解析服务**——按方言把 SQL 解析为结构化结果（表/列/血缘/AST），纯函数、无租户数据副作用。
- **建议接口**：`sql.parse(sql, dialect) → {tables, columns, lineage, ast}`；或等价的平台解析 RPC 投影到模块。具体形态由上游裁定。
- **验收标准**：
  1. 嵌套子查询 / CTE / 多方言 DDL 正确解析；
  2. 列级 lineage 输出可用于 `registerSql`；
  3. 解析失败返回结构化错误而非静默；
  4. 幂等无副作用、不取租户数据；
  5. 解析结果可直接替换模块掩码正则路径。
- **优先级**：**P2**。当前以增强 tokenizer + 掩码正则满足主要功能，本需求消除解析精度天花板，不阻塞 G0/G1。

---

### 3.4 REQ-HNSW：平台向量索引与维度迁移（P2）

> 上下文自治，可独立评审。对应项目内 P5-E runbook 与「HNSW 脚本不在 oj migrations 链内」的现状。

- **背景**：本仓向量存外部 vectors 库（PostgreSQL + pgvector），HNSW 索引与维度变更 DDL 由 `scripts/vector-hnsw-index.sql` / `scripts/migrate-vector-dim.sql` 以 **runbook 形态**交付，需部署侧用 psql 手工执行。
- **现状证据**：HNSW 脚本存在但不在 oj migrations 链内、需外部 vectors 库手工执行；L2 测试环境无 pgvector，只能对脚本文本做断言、真索引无法在测试内校验；代码侧仅读 `vectorDimension` 配置类型，模块**无 DDL 执行面与索引观测面**（如 `pg_indexes`）。
- **为何不能绕过**：oj migrations 只作用于 oj 自身库，**无法对外部 vectors 库执行 DDL**；模块内拼裸 DDL 外呼既无凭据治理也无索引状态观测；且维度变更必须联动全量 re-embed 与索引重建——这套编排属平台向量面职责，模块内实现等于绕过平台数据面治理。
- **举例**：要把向量维度从 768 升到 1536，当前流程是：部署侧手工 psql 执行 `migrate-vector-dim.sql` → 触发全量重新嵌入 → 手工重建 HNSW 索引，期间索引状态、进度都无法在平台内观测，多租户多集合容易遗漏。期望：平台提供 `vector.migrate`，一次受控调用完成维度迁移、重建索引并回报进度，模块按集合触发即可。
- **能力定义**：平台提供**向量索引与维度迁移**能力——对指定集合建/重建 HNSW 索引、按新维度迁移并触发全量 re-embed，状态可观测。
- **建议接口**：`vector.migrate({collection, dimension, rebuildIndex}) → {status, progress}`；或平台向量管理面等价原语。具体形态由上游裁定。
- **验收标准**：
  1. 平台侧可对集合执行 HNSW DDL、无需部署侧手工 psql；
  2. 维度变更触发全量 re-embed 与索引重建；
  3. 迁移状态/进度可观测；
  4. 多集合/多租户隔离正确；
  5. 可由模块受控触发且遵守 route timeout / abort。
- **优先级**：**P2**。当前 runbook + 脚本文本满足「部署侧可执行」口径，本需求把 DDL 与迁移纳入平台治理，不阻塞 G0/G1。

---

## 4. 交付节奏与说明

- 四条需求均为 **P2**，不阻塞 G0/G1；本仓仅提需求、不以绕过方案作为交付前提。
- REQ-PREDICT 以**合规评审通过**为前置门槛；其余三条可独立排期。
- 建议接口均为**建议形态**，最终以平台上游裁定为准；本仓愿意配合调整调用契约。
- 与本仓唯一规划文档 `docs/ontology/REPLICA-PLAN.md` 的 §7.11.11–14 内容一致；本文是面向上游的独立投递版本。
