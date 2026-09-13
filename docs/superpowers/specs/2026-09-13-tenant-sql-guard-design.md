# 多租户 SQL 防护设计（tenant.sql_guard）

日期：2026-09-13
状态：调研 + 三方评审修订稿（安全专家 / 架构师 / 工程师）
关联：`config.rs` `TenantCfg`、`src/bridge/query.rs`、`src/bridge/guard.rs`、`oj/src/schema.rs`、`server/src/lib.rs`

## 1. 背景与需求

`tenant.enable` 后，server 从 header 提取租户 id 注入 `http.tenantId`，但 **SQL 层无任何强制**：
handler 忘了写 `where({field:"tenant_id", ...})` 即跨租户裸奔。需求：

1. `tenant:` 下新增配置项，开关「多租户 SQL 防护」。
2. 启用后：构造器 SQL 自动注入 `tenant_id` 条件；schema 构建期校验表必须含 `tenant_id` 列。
3. 目标：防「开发者疏忽」导致的租户逃逸。

## 2. 现状链路（调研结论）

**租户链路已通到 op 层**：`config.tenant` → `server/src/lib.rs` handle() 提取 →
`RequestInfo.tenant_id` → `ReqState` → `http.tenantId`。所有 db op 运行时都能从
`OpState` 借到 `ReqState.req.tenant_id`（与 `guard.rs:163` `bound_db` 同款借法），
时序无问题（`checkout_reset` 在 run_module 前写入 ReqState）。

**SQL 两条路径，可行性截然不同**：

| 路径 | 实现 | 注入可行性 |
|---|---|---|
| 构造器 `db.table()` | `src/bridge/query.rs`，sea-query 结构化构建，表/列过白名单 | 高——结构化层注入，精准可控 |
| 裸 SQL `db.query/exec` | 字符串直传，`guard.rs` best-effort 表名扫描 | 低——字符串层可靠解析 WHERE 不可行 |

**Schema 链路**：`oj/src/schema.rs` 解析模块级 `schema.yaml` → `oj migrate` 安全前向
reconcile；同一份声明喂 `SchemaRegistry`。迁移与装配均读 config。

## 3. 配置设计

```yaml
tenant:
  enable: true
  header_key: X-TENANT-ID
  anonymous_paths: ["/oidc/*"]
  sql_guard: true        # 新增。false（默认）= 关 | true = deny | "warn" = 仅告警
```

- `TenantCfg` 增 `sql_guard: serde_json::Value` 形态或枚举，取 `false | true | "warn"`
  三态（serde default = false）。
- **强度自含，不寄生 `server.ownership_guard`**（评审必改项）：两个安全域不共享强度
  开关；`sql_guard: true` 即 deny，`"warn"` 供软过渡。
- `enable: false` 而 `sql_guard` 非 false → 装配期 warn 一次（防误以为已防护）。
- 开关本体进 `StableState` / `Extras`（照抄 `ownership_deny` 模式，装配期注入、
  首 run 前冻结）；`app.rs` 加 `sql_guard_of(cfg)`，**两处 StableState 构造点**
  （make_bridge 的 Extras 路径 + 测试/内省路径）都要注入。

## 4. 方案

### 4.1 构造器自动注入（核心防线）

**注入形态（工程师评审修正）**：不放进 `build_statement`（纯函数，签名穿透要改 4 个
函数），改为 **op 层 QueryReq 预变换**：在 `op_db_query_build` / `op_db_query_sql`
的 `guard_req` 之后、`build_statement` 之前插入 `apply_tenant(&mut req, &reg, tid)`——
把 `{field:"tenant_id",op:"eq",value:tid}` 作为 `CondTree::Leaf` push 进各层
`req.conditions`。白名单校验、参数绑定、方言渲染、括号化全部复用现有机制。

两个 op **必须同步注入**（评审 P1-3）：`toSQL()` 产物离开 AST 后经
`db.query(s.sql, s.params)` 直跑时，注入值是 JS 可见的绑定参数、可被篡改——
只有两 op 产物一致 + §4.2 参数校验兜底，该官方支持路径（query.rs 测试在用）
才有防护。

**递归覆盖**：`apply_tenant` 递归形状照抄 `guard_req`/`guard_nested`
（query.rs:710-761，CASE WHEN 藏子查询的 F-1 坑已标出）：

- select/update/delete 基表含 `tenant_id` 列 → push 条件。
- **join 表：条件注入 join 的 ON 子句，不是 WHERE**（评审 P2-9）：注入 WHERE 会把
  LEFT JOIN 静默变 INNER JOIN。需要 Join/on 结构支持值条件（OnPair 现为列对列），
  或在 `build_select_stmt` join 分支处理；测试锁定语义。
- 子查询 / exists / union 臂 / cte.query：递归注入；CTE 虚拟表跳过（照抄
  guard_req 的 `cte_name` 闭包判断）——内层已过滤，外层从 CTE 读无需再注。
- 嵌套层 `validate_nested` 禁 with/unions，递归深度天然有界。

**DML 写侧**：

- insert：values 强制写入 `tenant_id = <当前租户>`；handler 显式传 `tenant_id`
  与当前租户不符 → **报错**（§8 决策 2，已定）。
- **update：`sets` 含 `tenant_id` 键一律报错**（评审 P1-4，初稿遗漏）——
  `update({tenant_id:"victim"})` 会把本租户行迁移到他租户，写入侧跨租户污染。

### 4.2 裸 SQL 路径（软防，门禁式，诚实降级）

字符串 SQL 无法可靠注入。`sql_guard` 启用时的处置**独立于 ownership_guard、
独立于 module_ctx**（评审 P1-5：tenant 判定只需全局 registry，不得被无模块上下文
的短路放行）：

- 声明降级：本检查**只检测「完全遗漏 tenant_id」**，不验证谓词位置与绑定值。
  `where tenant_id = ?` 绑用户输入、`tenant_id = tenant_id` 均合规通过——文档与
  错误信息不得暗示「带上这个词就安全」。
- deny 强化（`sql_guard: true`）：语句触碰含 `tenant_id` 的表且文本无 tenant_id
  条件 → 拒；**且参数数组须含当前 tenant_id 值**（对 `= ?` 主流形态与 toSQL 直跑
  都有效，误杀方向是错杀）。
- **字面检查实现（工程师评审修正）**：不得直接复用 `tokens()`——它把双引号/反引号
  标识符当引用串跳过，sea-query 渲染的 `"tenant_id"` / `` `tenant_id` `` 会被 100%
  误杀（deny 下 toSQL→db.query 路径全灭）。正确做法：只剥单引号字符串字面量 +
  注释、保留引号标识符的清洗变体（~15 行），清洗后文本做大小写不敏感
  `contains("tenant_id")`。回归测试锁定 toSQL 产物（带方言引号）必放行。
- **表提取补全 + fail-closed**（评审 P1-6）：`extract_tables` 现漏逗号 join
  （`from a, b`）、MySQL 多表 UPDATE（`update a, b set ...`）、
  TRUNCATE/ALTER/DROP（全租户破坏面）。补提取；deny 下语句匹配 DML/DDL 关键字却
  提取不到任何已登记表 → 拒绝；TRUNCATE/ALTER/DROP 触碰含 tenant_id 表一律拒
  （此类语句无法注入条件，没有合法多租户用途）。
- memo：`sql_memo` 缓存的表名结构不动；tenant 字面检查一次 `contains` 成本可忽略。

### 4.3 schema 构建强制 tenant_id

- **声明期校验为独立函数**（架构评审必改项）：`SchemaFile::parse` 保持 config-free
  纯函数；新增 `validate_tenant(&self) -> Result<()>`，由消费方按开关显式调用——
  挂点：`app.rs` `build_schema_and_modules`、`checks.rs`（oj build 的校验实际在
  checks S 系列）、`migrate_cmd.rs`。release 模式 dist 内 schema.yaml 是同一份
  声明拷贝，server 装配校验天然覆盖。
- **共享表豁免与授权面**（评审 P1-7）：`schema.yaml` 表级 `tenant: false`
  （默认 true）。注意 `#[serde(default = "default_true")]`——裸 `#[serde(default)]`
  对 bool 默认 false，会反转语义（经典坑）。纯模块自声明无授权面：多团队场景
  任一模块可标 `tenant: false` 合法化全租户读取。落地为 **config 级共享表
  allowlist 与模块声明取交集**才生效；装配期输出全部共享表清单（审计面）。
- **迁移收敛**：reconcile 将「缺 tenant_id 列」作安全前向 ADD（可空）；存量行填充
  不自动决策——NOT NULL / 填值一律 fail-fast 打模板。存量 NULL 行对
  `tenant_id = ?` 不可见（安全方向），**禁止 `or tenant_id is null` 式共享行土法**
  （合法绕过注入）。
- **列类型一致性**（评审 P2-13）：header 为 String，列若为 INT 会触发 MySQL 隐式
  转换（索引失效 + coercion）/ PG 直接报错。校验限定 tenant_id 列为文本族，或注入
  按声明类型转换。
- **索引**：文档建议 `(tenant_id, ...)` 复合索引前缀；联合唯一约束须含
  tenant_id（`UNIQUE(tenant_id, email)`），需文档配套。

## 5. 改动面估算（工程师评审）

| 文件 | 净增行 | 内容 |
|---|---|---|
| `src/config.rs` | ~10 | TenantCfg + sql_guard 三态解析 |
| `src/bridge/mod.rs` | ~8 | StableState/Extras 各一字段 |
| `src/bridge/query.rs` | ~180（含测试 ~100） | apply_tenant 递归 + 两 op 调用点 + join ON 注入 |
| `src/bridge/guard.rs` | ~50（含测试） | 清洗变体 + check_raw 分支 + 表提取补全 |
| `oj/src/app.rs` | ~25 | sql_guard_of + 两处 StableState 注入 + warn |
| `oj/src/schema.rs` | ~40（含测试） | TableSchema.tenant + validate_tenant |
| `oj/src/checks.rs` + `migrate_cmd.rs` | ~15 | 三处挂点 |
| docs | — | api-manual / dev-guide 补开关语义 |

合计约 330 行含测试。**最危险两处**：guard.rs 引号误杀（toSQL 路径，已有确定性
答案：直接复用 tokens 会坏）；update sets 写侧逃逸（漏了就是洞）。ABI 零影响
（注入在宿主侧 AST/结构化层，插件只收最终 SQL 字符串，ABI v7 不 bump）。

## 6. 风险清单

1. **P0 信任根（安全评审升级：前置依赖，不得「另立任务」后上线）**：
   `server/src/lib.rs:347-381` 中 auth 守卫与 tenant 提取独立，无任何一致性校验：
   任意请求发 `X-TENANT-ID: victim` 即「合法」切换租户。注入机制越严密，伪造收益
   越高。sql_guard 上线的前置条件：**jwt claims 绑定租户**（oj-auth 已验签，
   claims.tenant 与 header 不符即 403）；无 claims 绑定时，配置校验/文档必须显著
   标注「sql_guard 仅在租户头可信部署（网关注入/内网）下有意义」。
2. **tenant_id = None 的策略**（评审分歧，已定，见 §8）：tasks/WS/bus 消费者/
   introspection 全程 `RequestInfo::default()`，WS 连接建立本身无租户（浏览器 WS
   带不了自定义头）。默认对含 tenant_id 表**拒一切访问**（不只 DML，select 泄漏
   同样是泄漏），提供显式逃生口（`db.asSystem()` 显式 API 或 config 级豁免模块
   清单）——「系统身份」必须是可审计的显式动作。WS 租户绑定（query param + 首帧
   校验）另立任务；`oj test` 运行器提供 mock tenant 注入（评审 P2-14），并有锁定
   「注入真的进 SQL」的 toSQL 断言，否则测试全绿是虚假信心。
3. insert/update 冲突语义：显式 tenant_id 不符一律报错（写入侧逃逸口，已定）。
4. 裸 SQL 软防只查「完全遗漏」，不查绑定值——已诚实降级并加参数强化（§4.2）。
5. 联合唯一约束需含 tenant_id（§4.3）。
6. 方言差异：无——注入走绑定参数，三方言统一。
7. **误杀面**：deny + 软防可能误杀合法跨租户报表 SQL；超管逃生口**按模块声明**
   （不按请求跳过），便于将来映射 RLS role/policy（架构评审）。临时跑数用
   `sql_guard: "warn"` 过渡。
8. 存量库迁移：数据工程非框架可推导，只 fail-fast 打模板。
9. **registry 全局按表名不分库**（评审 P2-12）：同名表跨命名库共用 TableDef，
   registry 说无 tenant_id 而某库实体表有 → 静默漏注。validate_tenant 加同名表
   跨库形状一致性检查（或文档明确该边界）。
10. DDL 面：TRUNCATE/ALTER/DROP 触碰含 tenant_id 表在 deny 下一律拒（§4.2）。

## 7. 适用性结论

| 需求 | 可行性 | 建议 |
|---|---|---|
| 构造器自动注入 tenant_id | 高 | 做（§4.1，op 层 QueryReq 预变换） |
| 裸 SQL 自动注入 | 不可行（解析天花板） | 降级 + 门禁式软防（§4.2） |
| schema 强制 tenant_id 列 | 高 | 做（§4.3，validate_tenant 独立函数） |
| 防恶意租户头伪造 | **前置依赖** | jwt claims 绑定租户，随 sql_guard 一起上线 |

**最小落地集**：P0 信任根绑定 + §4.1 + §4.3 + §4.2（deny 门禁）。
演进性：AST/结构化层注入是可拆卸一步，未来切 RLS = 删注入步，`tenant: false`
标记与列强制原样复用为策略元数据，不堵死。

## 8. 决策记录（评审后定案）

| # | 决策点 | 结论 | 依据 |
|---|---|---|---|
| 1 | tenant_id = None（tasks/WS/匿名路径）对含 tenant_id 表 | **默认拒绝一切访问**；显式逃生口 `db.asSystem()` / config 豁免模块清单；oj test 提供 mock tenant | 安全评审 P2-8（select 同样泄漏）优先于工程师的放行建议；sql_guard 默认 off，启用即接受语义 |
| 2 | handler 显式传 tenant_id 与当前租户不符（insert & update sets） | **报错** | 写入侧逃逸；两评审一致 |
| 3 | 裸 SQL 门禁强度 | **sql_guard 自含三态**（false/true/"warn"），解耦 ownership_guard，不挂 module_ctx 短路 | 架构 #1 + 安全 P1-5 |
| 4 | 声明期校验挂点 | `validate_tenant()` 独立函数，app.rs/checks.rs/migrate_cmd.rs 三处显式调用 | 架构 #3 + 工程师 #6 |
| 5 | LEFT JOIN 注入位置 | join 条件进 **ON 子句** | 安全 P2-9（WHERE 会静默变 INNER JOIN） |
