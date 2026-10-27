# db 访问控制核心（L1）——对齐 dbx 多库能力的第一步

> 状态：**v2（2026-10-27，四路评审后修订）**。目标版本 v0.1.89（宿主侧，不动 FFI → `ABI_VERSION` 保持 12，无插件 bump）。
> v1 → v2：吸收工程师/架构师/产品/安全四路评审，10 项阻塞全部处置（§0 评审记录）。

## 0. 四路评审记录与裁决（v1 → v2）

| 评审方 | 结论 | 一句话 |
|---|---|---|
| 工程师 | 调整后通过 | 挂载点核对准确；stream `truncated` 契约不成立（返回值无承载位）；`op_db_*` 白名单是全等断言；策略应落 `StableState`；配置别用 untagged |
| 架构师 | 调整后通过 | 独立落点正确但会逼出第二套词法；L3 复用抽象不足（缺 `PolicySource`）；策略与连接定义应分离；sqlparser 不引但预留 |
| 产品 | 调整后通过 | 默认 off 可接受但必须补全局默认档（逐库配必漏）；缺「可见连接列表」自省=假入口；文案需稳定错误码与实际身份回显 |
| 安全 | 调整后通过 | 词法有 7 个可打穿样本（反斜杠吞句、数据修改型 CTE、危险谓词黑名单、副作用语句、`/*! */` 与租户守卫冲突）；asTenant 借道跨租户；任务桥恒无身份 |

**裁决（评审方冲突，主持人定）**

1. **词法复用之争**（架构师「抽 `sql_lex.rs` 共用」vs 安全「严禁复用/修改」）→ **取安全方**。
   `check_tenant_raw` 复用 `strip_literals` 做 `mentions` 判定（`guard.rs:448`），若让共享函数保留 `/*! */`，
   则 `DELETE FROM orders WHERE 1=1 /*! tenant_id */` 在 deny 档**借注释伪造租户命中** → 跨租户全表删除。
   `db_policy` 自带独立扫描器，语义差异（可执行注释）是有意为之并在文件头写清；`guard.rs` **一行不改**。
2. **stream `truncated` 承载位**（工程师阻塞 vs 产品「必须有自省」）→ 都满足但降级形态：
   stream 超限 = 宿主侧 close + 进 `sql_profile` + 结构化日志，**JS 面本阶段不新增 `truncated` 字段**
   （`Row::Null` 即 done，无承载位；不造半截契约）。自省改为新增 `op_db_allowed`（见 §3.8），
   并**同步扩** `db.rs:1196` 的白名单断言（该测试存在的目的就是逼人想清楚，扩表须写理由）。
3. **危险门去留**（产品「若砍一半先砍危险门」）→ **保留**，但按安全方改主判据（有无 WHERE 为主，
   恒真集只用于收紧），把误报率压到可灰度。
4. **策略落点**（架构师「顶层独立段」）→ 采纳，理由：① `config.rs:1400` 注释明文禁止往 `db:` 塞东西；
   ② `db` 保持 `HashMap<String,String>` → 25 处 DSN 消费点零改动；③ L3 `clients:` 直接复用同一张策略表。

**待做登记（不在 L1 夹带，独立立项）**

- T1：`guard.rs:414` 的 `first_kw` 同样被 `WITH ... DELETE FROM` 打穿（安全样本②）→ 既有租户守卫的
  fail-closed 分支被跳过。属既有守卫语义变更，需单独评审，不在本文改。
- T2：档位类型归一（架构师建议）：`ownership_deny`(bool) / `SqlGuard` / 新 `Enforce` 三套并存 → 收敛为一个
  `Enforce`。L1 只新增 `Enforce`，不扩散改既有两处。
- T3：sqlparser 引入（架构师：不引但预留）。触发条件写死：**L2 需要「谓词是否覆盖到正确列」的判定时**。

## 1. 目标与非目标

**目标**

1. 库（db 别名）级**访问授权**：谁（模块 / 角色 / 租户 / 来源）可以用哪个库。
2. 语句**风险分类**（`SqlRisk`）：ReadOnly / Write / Ddl / Transaction / Unknown。
3. **危险语句门**：写/DDL 拦截，无 WHERE 的 UPDATE/DELETE 加严。
4. **只读位**：库级 `read_only`。
5. **限额**：raw/stream/exec 路径上限（现仅构造器路径有）。
6. **决策可观测** + **自省**（当前主体可见连接列表）。

**非目标（显式不做，不留假入口）**

- 语句超时下推、DB 侧只读事务：需 FFI 新槽 = ABI 13，另立。
- 确认 token 门（前端绕不过的二次确认）：无 UI 消费者，L3 一并做。
- 列级/行级授权：oj 已有 `SchemaRegistry` + 租户守卫，不重叠建设。
- SQL 改写（自动加 LIMIT / 注入谓词）：与 dbx 一致，不做。
- 动态连接登记（client 轴）：L3，复用本文策略表。

**适用边界（铁律）**：本策略**只约束 JS 提交的 SQL**。
平台自生成的 SQL（`op_db_next_seq` 的序列表 DDL、`op_db_list_tables` 的内省查询）与 **CLI 运维通道**
（`oj migrate` / `oj schema` / `oj build`，不经 bridge）**不适用**，文档与 CHANGELOG 明示。

## 2. 现状事实（v0.1.88 实码核对）

| 事实 | 位置 |
|---|---|
| db 配置为 `HashMap<String, String>`（别名 → DSN） | `src/config.rs:1339` |
| `config.rs` 注释明文：`db` 是 name → DSN 的 map，其他东西不能塞进去 | `src/config.rs:1400` |
| 裸 SQL 守卫：归属（module deps, warn/deny）+ 租户（sql_guard off/warn/deny） | `guard.rs:286, 388` |
| 风险判定仅首关键字粗判 DML/DDL（**被 `WITH` 打穿，见 T1**） | `guard.rs:414-420` |
| `strip_literals` 一律剥 `/* */`（含 `/*! */`），且与租户守卫共享 | `guard.rs:330, 362-368, 448` |
| `DB(name)` 无任何 ACL：同进程任何模块可摸任意配置池 | 实测同意（另见仓外 `oj-dbx/docs/PIP-0003-*.md` §1.8） |
| 行数上限只在构造器路径 | `query.rs:591-621`、`query_tests.rs:217` |
| `op_db_stream_next` 返回 `Row`，done = `Row::Null`，**无 meta 承载位** | `db.rs:784-805` |
| `op_db_*` 白名单为 **18 项全等断言** | `db.rs:1196-1241` |
| `SqlEvent` 有 `db`/`source`，**无 operator、无决策** | `sql_trace.rs:57-75` |
| **不存在** `_platform` oplog（PIP-0003 §9 R5 属上游需求，未落地） | 全仓 grep 无命中 |
| 用户身份 `RequestInfo.user: Option<Value>{id,roles,claims}`；无 roles 提取助手 | `http.rs:31-54` |
| 任务桥 `run_task` 走 `checkout_reset(RequestInfo::default(), None)` → **无身份** | `mod.rs:628-631` |
| `asSystem` 置 `ReqState.system`（租户守卫逃生口）；`asTenant` 可改 `req.tenant_id` | `db.rs:452, 472-497` |
| 事务内 SQL 复用 query/exec → 挂载点不需另加 | `query.rs:2239` |
| 宿主侧测试只能用 `sqlite::memory:` / `InMemoryAccessor`；真库套件在插件 crate | `oj-db-mysql/src/lib.rs` |

## 3. 设计

### 3.1 配置形态（顶层 `db_access:`，与连接定义分离）

```yaml
db:                       # 不动：仍是 name -> DSN
  default: "sqlite://db.sqlite"

db_access:
  defaults:               # 全局默认档（运维一行设防；缺省 = off，兼容现状）
    enforce: off          # off | warn | deny
  analytics:
    read_only: false
    allow_ddl: false
    allow_dangerous: false
    max_rows: 1000                 # 缺省=不限；范围 [1, 100000]
    max_affected_rows: 0           # 0=不限；写/DDL 的受影响行数上限
    allow:                         # 四维并集；缺省=不限制；声明即白名单
      modules: [sales]
      roles: [admin, dba]
      tenants: ["t1"]
      sources: [http]              # http | task | ws | anon（沿用 sql_trace 的 source 词汇）
```

**继承语义**：`db_access.<alias>` 未声明的键回落 `defaults`；`defaults` 未声明回落内置缺省
（`enforce: off`、其余关闭/不限）。

**解析规则**（fail-loud）：`deny_unknown_fields` 点名未知键并带 `db_access.<alias>.<key>` 上下文；
`max_rows` 越界报；`read_only: true` + `allow_ddl: true` = 矛盾配置启动报错（不静默取其一）。
**不用 serde untagged**（错误会退化成 "did not match any variant"，吞掉点名）——自定义 `Deserialize`：
`visit_map` 中键 `defaults` 归 defaults，其余归 by_db。
明文 DSN 仍允许（兼容），但含 `://user:pass@` 形态解析期 **warn**（不 fail-fast）。

### 3.2 授权判定（库级白名单）

- 主体四维：`modules`（`ReqState.module`）、`roles`（`user.roles`）、`tenants`、`sources`。
- 语义：**并集**——任一**已声明**维度命中即放行；未声明维度不参与限制。
  ⚠️ **声明多维不会提高强度，只降低**（任一命中即过）——文档加粗标注。
- `allow` 缺省/空 = 不限制（兼容）。`allow: { roles: [] }` = 声明且为空 → **无人可过**（fail-closed，不是「不限制」）。
- fail-closed：声明了某维而请求取不到该维身份 → 该维不命中（不因「取不到」而放行）。
  四维全声明而请求四维皆无 → 拒，文案点名「无可用身份维度」。
- `sources` 维解决**任务桥恒无身份**（`mod.rs:628`）的问题：定时任务走 `sources: [task]`
  （沿用 `sql_trace.rs:64-65` 的 `anon`/`task` 惯例），否则运维只能不声明 `allow` → 退回不设防。
- **`tenants` 维只认 server 注入的原始租户**：`db.asTenant()`（`db.rs:472`）改写 `req.tenant_id` 后
  该维作废 —— 否则匿名 handler 可自造租户命中白名单（借道跨租户）。
- **`asSystem` 不豁免**授权与策略位（它只保留租户守卫豁免的既有语义）；管理员维护走 `allow.roles: [admin]`。
  以测试钉死（新增 `authorize` 是独立函数，不读 `ReqState.system`）。

### 3.3 风险分类（`SqlRisk`）

**词法器：`db_policy` 私有，独立实现，严禁复用/修改 `guard.rs` 的 `tokens`/`strip_literals`**（§0 裁决 1）。

```
classify(sql) -> Classified { risk, dangerous, reasons }
```

1. 扫描（自研）：字符串字面量（`'...'`，`''` 双写；**反斜杠转义按方言差异处理，串未闭合 → `Unknown`**）；
   `--` 行注释、`#` 行注释（MySQL）、`/* ... */` 块注释剥除，**`/*! ... */` 保留内容**（MySQL 可执行注释，
   否则 `/*!40001 DROP */` 被剥成空白 → 绕过）；反引号/双引号/`[]` 标识符内容成词；`;` 作分句边界。
2. 多语句：按 `;` 切分，逐段分类，**取最严**；任一段 Unknown → 整体 Unknown。
3. 分类：首有效关键字映射；`REPLACE INTO`、`INSERT ... ON CONFLICT ... DO UPDATE` → Write；
   `WITH` → **递归下钻**取最严（数据修改型 CTE：`WITH d AS (DELETE FROM t RETURNING id) SELECT ...` = Write）；
   `begin/commit/rollback/savepoint` → Transaction。
4. **副作用归 Write**（不在表内即被放行是洞）：`SELECT ... INTO OUTFILE/DUMPFILE`、`COPY ... TO/FROM`、
   `LOAD DATA`、`SET GLOBAL/SESSION`；裸 `SET` → `Unknown`；带副作用函数（`nextval`、`pg_terminate_backend`、
   `lo_import`）+ `FOR UPDATE` / `LOCK IN SHARE MODE` → Write。
5. **危险判定**（主判据 = 有无 WHERE，**恒真集只用于收紧**，不做黑名单）：
   - `UPDATE`/`DELETE` **无 WHERE** → dangerous
   - 多表 `UPDATE`/`DELETE`（`update a, b set` / JOIN / USING）→ dangerous
   - DDL / TRUNCATE / DROP / ALTER / CREATE → dangerous（受 `allow_ddl` 门）
   - 恒真谓词集（`1=1`、`a=a`、`IS NULL OR IS NOT NULL`、`LIKE '%'`、互补比较）→ 只用于**收紧**（已判危险的不放松）
6. `Unknown` → 按最严档处置，理由进 `reasons`。

### 3.4 判定入口与挂载点

`authorize_db_access(state, alias, sql) -> Result<Decision, JsErrorBox>`，挂在
`op_db_query`(db.rs:531) / `op_db_exec`(db.rs:649) / `op_db_stream_open`(db.rs:697)。
构造器路径 `op_db_query_build`(query.rs:2243) 走**精确意图**判定（`QueryReq` 已知操作类型，不经词法）。

流程：`bound_db`（既有）→ **授权** → **风险分类** → **策略位** → **限额**。
既有 ownership / tenant 守卫**并列不合并**（各自 fail-closed 独立；合并会牵动 12 组行为测试）。
优先级与文案归属：授权 > 策略位 > 风险 > 限额（先返回的那个决定的文案）。

`enforce: off` 时零开销早退（照 `trace_sql` db.rs:608-616 写法）。
分类结果走 memo（键**只含 SQL 文本**，与身份无关；容量照 `sql_memo` 4096）。
策略数据落 `StableState.db_access: Arc<HashMap<String, DbAccess>>`（配置派生、进程级常量，与 `sql_guard` 同类）；
构造点 4 处：`mod.rs:787`、`mod.rs:2495`、`oj/src/app.rs:1361`、`platform.rs:190`；装配真入口 `oj/src/app.rs`。

### 3.5 处置档位与错误文案

`enforce: off`（默认）/ `warn`（告警放行，灰度）/ `deny`（拒）。
JS 信封沿用 `{code, msg, data}`：HTTP 语义码 **403** + `data.reason` 稳定串（供告警 grep）：
`db_not_granted` / `db_read_only` / `db_ddl_denied` / `db_dangerous_write` / `db_unknown_risk` / `db_row_limit`。

文案 = 点名 + 修复指引（与 `guard.rs` 同风格）；**对外只回显调用方自己的身份**（module/roles/tenant），
**绝不回显 `allow` 名单**（否则 403 响应可被用来枚举库别名与授权名单）：

```
db access: alias "analytics" is read_only; statement classified Write (UPDATE)
  修复：改用只读语句，或由管理员放开该库 read_only（reason: db_read_only）
db access: alias "analytics" not granted to module "sales"（reason: db_not_granted）
db access: dangerous UPDATE/DELETE without WHERE on alias "analytics"
  修复：补 WHERE 条件，或放开 allow_dangerous（reason: db_dangerous_write）
db access: statement classification failed (unknown); denied by fail-closed
  修复：改用构造器 db.table()，构造器走精确意图判定、不依赖词法（reason: db_unknown_risk）
```

### 3.6 限额

- raw `query`：结果 > `max_rows` → 截断 + `X-OJ-Row-Limit`（复用既有头）。
  ⚠️ **这是响应整形，不是 DoS 防线**：`db.rs:549-572` 先取回全量 `Vec<Row>` 再截断，全量扫描/传输/驻留照旧。
- `stream`：宿主侧计数，达 `max_rows` → 自动 close，下一次 `next` 返回 `Row::Null`（done）；
  计 `sql_profile` 与结构化日志。**JS 面本阶段不暴露 `truncated` 字段**（无承载位，不造半截契约）。
  并发沿用现有 per-client 流闸 8（`STREAM_MAX_CONNS`），不新增全局瓶颈。
- `exec` / 构造器写：`max_affected_rows`（默认 0=不限）——`DELETE` 无上限是最大 DoS 面，位先留、默认不设防。
- 构造器路径生效值 = `min(db_query 生效 limit, policy.max_rows)`（两者都声明取小）。

### 3.7 观测

- `SqlEvent` 扩 `risk` / `decision` / `operator`（= `user.id`，无 user 记 `anon`/`task`）三字段，缺省 `None`
  → 现有日志形态不变，不破坏既有诊断。
- 拒绝/告警另落**结构化日志**：独立 target + 固定前缀 **`DB_ACCESS_DENY`** + 按 `(alias, reason)` **聚合计数**
  （否则 warn 档会被日志淹没、且可被刷盘）。
- `redact_sql()`：剥字面量/注释 + `password=`/`token=` 打 `***` + 截断 512。
  **明文红线：决策日志永不落 `dsn`**；`operator` 属 PII，文档声明保留期。
- 声明：本阶段通道**非审计、无防篡改**（对齐 PIP-0003 风险 10 口径），正式审计归 G8。

### 3.8 自省：`op_db_allowed`

返回当前主体可用的库列表：`[{alias, dialect, read_only, allow_ddl, allow_dangerous, max_rows}]`。
dbx 首屏是连接树——没有这个出口，前端只能列全部连接、点了才报错（= 假入口）。
新增 `op_db_*` **必须同步扩** `db.rs:1196` 的 18 项白名单断言并写理由。

## 4. 测试清单

1. 分类矩阵：五类各若干 + 方言特例（REPLACE INTO / ON CONFLICT / RETURNING）。
2. **对抗性探测（必写，样本直接进单测）**：
   - `SELECT 'a\'; DROP TABLE users`（反斜杠吞句 → 必须 Unknown，不得 ReadOnly）
   - `WITH d AS (DELETE FROM orders RETURNING id) SELECT count(*) FROM d`（数据修改型 CTE → Write）
   - `/*!40001 DROP TABLE t */`（可执行注释 → Ddl）
   - `DELETE FROM t WHERE 1=1 /*! tenant_id */`（伪造租户命中 → 不改变授权判定；另钉 `strip_literals` 未被改动）
   - `DR/**/OP TABLE t`（注释断词 → 不得被判 ReadOnly）
   - `select 1 into outfile '/tmp/x'` / `SET GLOBAL max_connections = 1`（副作用 → Write）
   - `update a, b set a.x=1 where a.id=b.id`（多表 → dangerous）
   - `# drop table t`（MySQL `#` 注释）／反引号/双引号包裹关键字／Unicode 空白断词／`;` 堆叠取最严
3. 授权：命中/未命中/无身份 fail-closed/四维并集/`asSystem` 不豁免/`asTenant` 后 tenants 维作废/task 走 sources。
4. 档位：off 放行、warn 放行且落 `DB_ACCESS_DENY`、deny 拒且 `data.reason` 稳定。
5. 限额：raw 截断 + `X-OJ-Row-Limit`；stream 提前终止；`max_affected_rows`；构造器取 min。
6. 配置解析：未知键/越界/矛盾配置启动报错；`db:` 旧形态零改动回归；DSN round-trip（`load_with_extra` 走文本）。
7. 行为面：经 `Bridge` 跑 handler（`sqlite::memory:`；raw 截断用**文件型临时 sqlite**，内存库每连接独立）。
8. 白名单守护：新增 `op_db_allowed` 后 `db.rs:1196` 断言同步（19 项）。

## 5. 交付件

- 代码：新 `src/bridge/db_policy.rs`；改 `src/config.rs`（`db_access:` 段）、`db.rs`（挂载 + 限额 + `op_db_allowed`）、
  `query.rs`（构造器意图判定）、`sql_trace.rs`（扩字段）、`mod.rs`（`StableState`/`Extras`，4 处构造点）、
  `oj/src/app.rs`（装配）。
- 文档：`docs/db-guide.md` 新增访问控制章 + 「承诺 / 不承诺」两栏表；devkit 四件同步并 `cargo xtask build`。
- `CHANGELOG.md` + `oj/Cargo.toml` 递增 v0.1.89 + `Cargo.lock`。
- 门禁：`cargo fmt --check`、`cargo clippy --release --workspace --all-targets -- -D warnings`、
  `cargo test --release --workspace`。
- 插件与 ABI 不动 → 无插件版本 bump。

## 6. 风险与开放问题

1. **误报成本**：复杂 SQL（CTE/窗口/存储过程体）→ `Unknown` 档。默认 off + warn 灰度是主要缓解；
   对外用词「**护栏层**」，并写明**不是**数据库账号权限的替代品，真正的写防线仍是 DB 侧 GRANT。
2. **「看起来像防线其实不是」（加粗进文档）**：① `read_only` 是宿主侧**文本级**判定，DB 侧仍靠账号 GRANT；
   ② `max_rows` 是**取回后截断**（raw），非限流；③ `allow` 四维并集下声明多维**不提高**强度。
3. **敞口未修**：`sql_guard=deny` 站点上 raw **SELECT 未登记表**仍是敞口（`judge` owner=None 放行、
   `check_tenant_raw` scoped 为空放行），新策略只管 alias + risk，ReadOnly 放行 → **敞口原样保留，不得误读为已修复**。
4. **`asSystem` 不豁免**是行为变更：现有用 asSystem 跑写语句的 handler 在 deny 档会被拒 → 默认 off + 升级说明点名。
5. **多语句边界**：不承诺存储过程体（声明）。
6. 开放问题：`allow` 是否需要黑名单形态？本阶段只做白名单（语义单一）。
7. 开放问题：`read_only` 是否对 `CREATE TEMPORARY TABLE` 开口子？本阶段一律拦（简单可预测）。
8. 待做：T1（`guard.rs:414` WITH 洞）/ T2（档位类型归一）/ T3（sqlparser 触发条件）——见 §0。
