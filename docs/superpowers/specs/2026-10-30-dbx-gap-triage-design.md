# dbx 上游缺口分流：七项能力评审与排期（v0.1.109 基线）

> 2026-10-30。需求来源：`../oj-dbx/docs/2026-10-30-upstream-gap-inventory.md`（基线 v0.1.109，
> 可复现实测 + 源码对账）。本文档把 7 项缺口转成需求、并入三位独立评审（专家工程师 /
> 专家架构师 / 产品专家）的调整意见，并给出分批排期。
> 用户已拍板：REQ-07b 按**列声明类型**判别（不做内容嗅探）；表示法采用**前缀标量约定**
> （`$<type>:<base64>`，架构师评审中对象信封的替代方案，经评估更符合本仓「非标量→字符串」惯例）。

## 意图与为什么

oj-dbx（数据库管理工具）实测盘点出 7 项上游缺口。其中 3 项在初稿评审中被证实存在
**机制性错误**（写法跑不通），1 项触碰 fail-loud 红线的扩散面，优先级经产品视角重排。
本文档是实施前的唯一权威记录：每项的需求形态、实现机制、工作量、验收口径以本文为准。

## 总排期（评审后）

| 批次 | 内容 | 工作量 | breaking |
|---|---|---|---|
| 第一批·立刻 | BUG-02 + REQ-07a（未解码类型降级）+ `$` 保留契约文档 | 0.5d + 1d | 否（400→成功） |
| 第二批·v0.1.11x | REQ-09（拆三单）+ REQ-08 + REQ-10 一期（catalog/kill）+ REQ-07b（二进制前缀标量） | 3-5d + 1d + 1.5d | REQ-07b 是（CHANGELOG 标注） |
| 第三批·随后 | REQ-11 旁路取消（透传配方）→ REQ-12 一期（可再压后）→ REQ-10 其余 | 0.5d / 1-2d | 否 |
| 挂起 | COPY 快路径 / bulk_load 槽 / vtable 取消槽 → 真实 repr(C) 需求累积时打包 **ABI 13** | — | ABI bump |

**本批全部零 ABI**：decode 在插件内、catalog/kill/import 在宿主侧、取消走旁路透传。
ABI 13 候选清单（挂起，不为单一特性支付 bump 成本）：`DataAccessorVtable.bulk_load`、
FfiFuture 取消语义 / 流式进度回调、（仅当旁路被证不足）非流式 cancel 槽。

---

## 第一批

### BUG-02 · MySQL `catalog("constraints")` 跨表串味（0.5d）

- **根因**（已对账 `src/bridge/introspect.rs:600-609`）：`key_column_usage` JOIN 仅
  `constraint_schema + constraint_name`，缺 `kcu.table_name = tc.table_name`；MySQL 全库
  PK 同名 `PRIMARY` → 同库乃至被 scope 异库的列全部聚合串味。
- **修法**：JOIN 补 `AND kcu.table_name = tc.table_name`，一行。
- **对账旁证**：PG 侧 `is_pk` JOIN（`introspect.rs:127-130`）同形但 PG 约束名在 schema
  级唯一命名空间，实际不可达；MySQL FK（`:187-196`）自带 `table_name = ?` 过滤，安全。
  只修 constraints 一处。
- **验收**：env-gated 真 MySQL 回归——同库两表同名约束类别、跨库 scope 不互串。
- 0.5d 而非「半小时」：按仓库红线含 CHANGELOG + devkit 四件 + `xtask build` + 真库回归。

### REQ-07a · 未解码类型降级（1d）

**需求**：PG `INET/CIDR/MACADDR/INTERVAL/MONEY/…`、MySQL `GEOMETRY` 族等未支持类型，
今天走 `undecodable_column` 让**整条查询 400**（`plugins/oj-db-postgres/src/lib.rs:311-317`）；
表里有任一此类列，`SELECT *` 即整表不可用。改为按列降级。

**实现机制（评审修正，初稿「插件内自动 cast」跑不通——行已物化，无处注入 SQL）**：
- 生产路径是 typed `PgPool`：列全 miss 后 `try_get_raw().as_bytes()` 取协议原始字节
  → 前缀标量 `"$<type>:<base64>"`（如 `$inet:...`）。
- typed 路径的判别依据是**列声明类型**（用户拍板）：不做内容嗅探，同列全行同形态。
- 后续演进：inet/macaddr/money 手解为可读文本——**值变、前缀与标量形态不变**，
  消费侧契约稳定。

**fail-loud 红线边界（架构师条款，必须同批落地）**：
- 降级**不等于**静默：响应带语句级 `degraded: [{col, type}]` 汇总；export 任务产物
  同规则标记降级列。`degraded` 清单是权威——TEXT 列字面量撞 `$<type>:` 前缀时以它为准。
- 契约写进 `docs/devkit/api-manual.md` 及 db-guide §10.6：单元格值前导 `$`+类型名+`:`
  为保留编码；bridge 只对「声明类型为二进制/未解码」的列产出前缀，普通 TEXT/JSON 列
  永不加前缀。

**验收**：真库类型矩阵回归（PG inet/interval/money、MySQL geometry）——不再 400，
返回前缀标量 + `degraded` 含该列；既有可解码类型矩阵不回退。

---

## 第二批（v0.1.11x）

### REQ-09 · 结构变更覆盖面（拆三单，3-5d）

1. **rename 语义（0.5-1d）**：`ddlDiff` spec 加显式 `renames: [{from, to}]`——与
   db_ddl.rs「缺省=不参与 diff」的显式 spec 气质同构；启发式猜测（同类型同位置）只做
   **fallback 提示**，不进自动路径。列改名 = `RENAME COLUMN`（PG）/ `RENAME COLUMN`（MySQL 8），
   杜绝今天的 DROP+ADD 丢数据。
   > **批次 2 修订（2026-10-30）**：表面契约以下游 REQ-13 原文为准——
   > `to.columns[].previousName`（非顶层 `renames` 数组；下游已按此承诺发送并写了
   > 六边界验收）。语义裁决不变：显式-only、启发式只做 warnings 兜底不进自动路径。
   > MySQL 版本门槛采 REQ-13 §4.5 方案②：直接生成 8.0 语法，5.7 让 apply 阶段报错
   > （`DialectCaps` 无版本探测，不为它扩 ABI）。
2. **schema 定向（0.5d）**：`name` 从裸标识符放宽为 `"schema.name"` 或 `{schema, name}`，
   每段**独立**过 `quote_ident`（NUL/控制字符照拒、段数 ≤2）。分段引用仍在引用通道内，
   不触 SQL 注入红线。
3. **四类对象进拼装器（2-3d）**：分区（PG 分区键/子分区尽力还原 + MySQL 无原生分区
   DDL 的口径写死）、视图、例程、触发器，各一套语法，枚举式扩展；`objectType` 联合扩
   `"view"|"routine"|"trigger"`；verbatim 片段仍走 `check_fragment`。

### REQ-08 · 对象源码与例程元数据（1d，零 ABI）

- catalog SQL 全在宿主（`introspect.rs`），新增/扩展 kind 不动 repr(C)。
- PG：`pg_get_functiondef(oid)` 补例程源码 + `proargtypes`/`prorettypename` 参数与返回类型。
- MySQL：`information_schema.routines` 补 `ROUTINE_DEFINITION`+参数+返回；视图 DDL 首选
  `SHOW CREATE VIEW`（需 SHOW VIEW + SELECT 双权限，缺权限 ERROR 1142 → fail-loud 文案
  点名权限），低权限退路 `information_schema.VIEWS.VIEW_DEFINITION`（文档标注两者差异）。
- PG 视图 DDL 补 `CREATE VIEW` 包装（今天只有 viewdef 片段）。

### REQ-10 一期 · processlist / kill（1.5d，零 ABI）

- **catalog 新 kind**：`processlist`（PG `pg_stat_activity` / MySQL `information_schema.
  processlist`）、users、roles。宿主内扩展，零 ABI。
- **kill 透传（评审抓出的机制修正）**：PG `SELECT pg_cancel_backend(pid)` 已可过
  `db_policy`（SIDE_EFFECT_FNS 归 Write）；MySQL `KILL` 首词不在 `classify_stmt`
  （`src/bridge/db_policy.rs:814-885`）→ Risk::Unknown → `evaluate` **恒 fail-closed**。
  须加一条 `"KILL" => Write` 分类规则 + 测试（0.5d）。推荐 SQL 形态 `KILL QUERY id`
  （不杀连接）。
- SSH 隧道是连接建立面，**剔除本批另立**。

### REQ-07b · 二进制前缀标量（breaking，随本批 CHANGELOG 标注）

- **判别（用户拍板）**：按列声明类型，`bytea`/`blob`/`binary`/`varbinary` 列**一律**
  `$<type>:<base64>`——合法 UTF-8 的 bytea 也不再以裸字符串返回。确定性表示优先于
  兼容面。
- 响应带 `degraded` 汇总（bytea 列入，encoding=base64）；export 产物同规则。
- **编码只能是 base64，不可裸二进制**：JSON 字符串必须是合法 UTF-8（serde_json 的
  `Value::String` = Rust `String`），非法字节无路径直传——这正是 REQ-07 要修的原始损坏
  点。`<binary>` 段是「字节→文本」编码，base64（1.33×）已是地板价（hex 2×、拉丁-1 映射
  平均 ~1.5×且零生态）。体积敏感场景的正解是导出到文件（blob 直写二进制，不经 JSON）。
- **不做双形态开关**（产品条款）：现状是「可能已损坏的字符串」，留双形态等于把损坏
  路径合法化。一次性切换，devkit 四件 + dbx 读侧一次性跟随。

---

## 第三批

### REQ-11 · 非流式取消 = 旁路透传配方（0.5d，零 ABI；架构师方案取代 vtable 车）

初稿「等 ABI 13」被架构师否掉：通行做法是**旁路取消**，全走现有 vtable——

1. 跑长查询前 `SELECT pg_backend_pid()` / `SELECT CONNECTION_ID()`（现有 `db.query`）拿 pid；
2. 另开连接 `SELECT pg_cancel_backend(pid)` / `KILL QUERY id`（REQ-10 已通）取消。

- 顺序约束：必须先取 pid 再发长查询（同连接自取消需预登记）；池占用角注：stuck 查询
  占 1/N 池位，旁路连接通常可拿。
- 形态：**文档配方即可成立**（api-manual 给配方 + dbx 端两条 exec 拼出取消按钮），
  是否再包 `db.cancelPid` 宿主 helper 由实现计划按需定。产品佐证：dbx 亦可用
  `db.stream` executionId 取消绕行——两条路并存，文档都写。

### REQ-12 · 批量导入一期（1-2d；产品弹性：可压到导入向导有真实性能投诉后）

- **镜像 `db_export.rs` 任务模型**：进程级登记（boot-tag）、审计链、blob 产物 TTL、
  配额——架构师条款修正了盘点文档「状态机归下游」的划分：下游自建任务表取消不了在途
  写、拿不到配额/审计，**上游承担是对的**。
- **写路径语义差异化（必须显式，不能照抄 export）**：原子性二选一——默认事务包裹
  全或无 / opt-in 显式 partial-failure 报告；配额按写入行/字节计（非任务数）；审计
  事件标 mutation 类型。
- 执行层：批式多值 INSERT（方言无关，零 ABI）。
- **COPY 快路径挂起**：sqlx 0.9 的 sqlx-postgres 有 `copy_in_raw`（`Cargo.lock:6416`
  对账），sqlx-mysql 无 LOCAL INFILE 实现；COPY 塞不进现有 query/exec 槽 = ABI 13
  候选，与 bulk_load 槽同批，不再单列「二期」。

### REQ-10 其余 · users/roles 视图等（随第三批或 GA 管理面）

processlist/kill 已提前；users/roles 的 catalog kind 若第二批有余量可提前。

---

## 非目标 / 反目标

- 双形态兼容开关（bytea 信封）：反目标，见 REQ-07b。
- 内容嗅探判别二进制（同列混排字符串/信封）：反目标，已拍板 type-driven。
- 启发式 rename 猜测进自动路径：反目标，只作 fallback 提示。
- 静默降级任何形态：空结果冒充 unsupported、降级列不进 `degraded` 汇总，一律禁止。
- ABI 13 为本批任何一项单独发车：反目标（锁步重编宿主+全部插件，代价不成比例）。
- SSH 隧道混入本批：非目标，另立评估。

## 开放项（实施前需确认）

1. **Any 池回退路径**（sqlite 测试矩阵走 Any）：`try_get_raw` 能否取原始字节未实测；
   最坏情况该路径退化为「单元格级 `$<type>:` 信封」而非整行 400，实现时确认并登记。
2. MySQL `SHOW CREATE VIEW` 权限矩阵凭官方文档，未在真库验证 1142 文案原文。
3. dbx 侧 `db.stream` 绕行取消的改造量未实测（若 REQ-11 旁路配方做掉，此点作废）。

## 评审记录（2026-10-30，三角色独立评审后并入）

**专家工程师**（源码对账 + 工作量修正）：
- 修正①BUG-02 工时 0.5d（红线成本）；②REQ-07 机制改为 `try_get_raw` 原始字节兜底；
  ③REQ-07 既有惯例 = 单元格恒标量（JSON/JSONB→文本串、NUMERIC→十进制串），无 `$` 信封
  先例——前缀标量约定与该惯例同构；④MySQL `KILL` 被策略分类器恒拒，须加分类规则；
  ⑤sqlx 0.9 PG 有 `copy_in_raw`、MySQL 无 LOCAL INFILE；⑥REQ-12 快路径是 ABI 13 依赖。
- 盲区自陈：Any 池 `try_get_raw` 未实测。

**专家架构师**（红线 + 契约演进）：
- 条款①REQ-07 降级必须带语句级 `degraded` 汇总，否则降级值静默流进 export 产物，
  「一处降级、处处错值」比 400 更伤；②判别按列声明类型，内容判别是非确定性表示；
  ③`$` 前缀是**新惯例**且与 TEXT 字面量有碰撞面，api-manual 必须写明碰撞规则与消费方
  判定路径（以 degraded 清单为权威）；④REQ-11 旁路取消零 ABI，ABI 13 清单本轮为空；
  ⑤REQ-12 写路径语义差异化；⑥REQ-09 显式 renames 与 spec 气质同构、schema 名分段
  quote_ident 不触红线。

**产品专家**（优先级 + 行为变更影响）：
- 重排①REQ-07 与 REQ-09 语境互换——「用户发现不了的数据事故」比「主动操作丢数据」
  更阴；②REQ-08 提至第二批（对象浏览器是门面）；③REQ-07 不 breaking 腿先走、二进制
  信封随第二批 minor，**不要双形态开关**；④REQ-10 的 catalog/kill 提到第二批（「卡死
  了怎么办」叙事闭环）；⑤REQ-11 权重下调（dbx 可 db.stream 绕行）；⑥REQ-12 可压到真实
  性能投诉后。
- 自陈：dbx 侧 stream 绕行改造量未验证。

**冲突裁决记录**：
- REQ-11 旁路（架构师）vs vtable 槽等 ABI 13（工程师）→ 采旁路；vtable 槽降为
  ABI 13 候选。
- REQ-07b type-driven（架构师/用户）vs fallback 保兼容（工程师）→ 用户拍板 type-driven。
- 表示法对象信封（初稿）vs 前缀标量（用户提议）→ 采前缀标量，理由：与「非标量→
  字符串」既有惯例同构、CSV/export 直通、消费侧无 typeof 分叉；碰撞面两者同类，
  由 degraded 权威清单消解。
