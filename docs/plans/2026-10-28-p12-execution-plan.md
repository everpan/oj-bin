# P1/P2 执行计划与任务跟踪（U-11 / U-14 / U-15 / U-16）

> 状态：**本批完成**（T11/T14 批 a/T15 M1/T16.1 + R.1 合并审查全部 ✅；待办仅余批 b/M2，见 §3）。
> （本文件是唯一任务真相源：状态、小结、增量记录都在这里，随做随更。）
> 依据：四份已过三路评审的设计稿（`-u11-…` / `-u14-…` / `-u15-…` / `-u16-…`，2026-10-28 处置落稿）。
> 原则：SOLID（单一职责：探测/内省/管理面各自独立模块；依赖倒置：全部经既有 trait/通道）+
> DRY（探测 helper 三处复用、内省走同一 `introspect()` 通路、审计走 `audit::emit` 单一出口）+
> BDD（每任务 Given/When/Then）+ INVEST（每任务独立可测可发版）。

## 0. 发版与提交策略

| 阶段 | 内容 | 版本动作 |
|---|---|---|
| P-1 | U-11 + U-14 批 a（同车，评审定案） | **bump v0.1.100**（CHANGELOG + devkit 四件 + 门禁全绿后提交打标） |
| P-2 | U-15 M1（宿主管理面 + 装配隔离 + mini-mgmt 夹具） | **不 bump**（feat 提交；发版待 M2 真 connector） |
| P-3 | U-16 T0（部署级隧道 recipe 文档，产品路裁决的零成本前置） | docs 提交 |
| P-4 | **合并审查**（全部阶段任务一起：三路评审人对 diff 整体复审）→ 修复 → 收尾 | 修复提交 |
| 待办 | U-14 批 b（v0.1.101）、U-15 M2/M3（mongo connector + 发版） | 另期（跟踪表登记） |

## 1. 任务跟踪

| # | 任务 | BDD 验收（Given/When/Then） | 状态 | 小结 |
|---|---|---|---|---|
| T11.1 | 版本探测 helper（方言分派 + 3s 超时 + 降级） | Given 一个已连接 accessor，When 探测，Then 三已知方言各得版本串；查询挂起超 3s/报错 → None+warn；开放方言 → None 且不发 SQL | ✅ | `probe_version(_with)`：3s timeout 可参（测试 50ms+HangingAccessor）；开放方言规划内跳过无 warn |
| T11.2 | ClientDbEntry 字段 + register/reRegister/test 三处接线 + summary 透出 | Given sqlite 临时文件库，When register，Then `db.client.list()[0].serverVersion` 形如 `3.x.y`、`probedAtMs>0`；reRegister 换 DSN 后更新；`db.client.test` 返回同字段 | ✅ | `server_version: Option<String>` 非 Mutex（评审裁决）；Bridge 测试 register→list→test 同源断言 |
| T11.3 | 测试夹具：query 即报错的 accessor；静态库/开放方言 null 断言 | Given 报错夹具，When register，Then 登记成功且 serverVersion=null；静态库 allowed() 无该字段 | ✅ | 复用 `InMemoryAccessor::set_error`（无需新夹具）+ HangingAccessor；开放方言/静态库断言并入 T11.1/文档面 |
| T14.1 | `introspect.rs` 扩 7 Kind × 3 方言 + LIMIT 标志位 + unsupported 矩阵 | Given 新 Kind，When 取 SQL，Then mysql/pg 有常量且过静态守卫；sqlite 仅 views/ddl，其余 unsupported；SHOW 族走白名单例外 | ✅ | `catalog_sql` 分表（`sql()` L2 不动）；`mysql_show_create` 白名单例外；静态守卫 5 件全绿（守卫当场抓到 Comment 缺 schema 限定——修掉） |
| T14.2 | `op_db_catalog`（信封 `{kind,dialect,items}`）+ JS `db.catalog` + granted_only 链 | Given 未授权别名，Then `db_not_granted`；未知 kind → 点名报错；unsupported → `OJ_DB_UNSUPPORTED`（capability）；畸形表名 → 空结果 | ✅ | 白名单守护测试（裸 SQL 出口）补 `op_db_catalog`（已插桩 normalize_and_trace） |
| T14.3 | 静态守卫扩覆盖 + golden（sqlite 实测）+ unsupported 用例 | Given 全部新常量，When 跑守卫，Then 全绿；Given 真 sqlite，Then views/ddl 行形状断言 | ✅ | Bridge 测试：信封 shape、sqlite 矩阵、unknown kind、畸形表名空结果；mysql/pg golden 待真库（env-gated，批 b 补）——**部分**：CI 内 sqlite 全绿 |
| T14.4 | 文档四件 + devkit 对齐 | Given 发版清单，Then 四件与用户可见变更逐条对齐 | ✅ | db-guide §6 catalog 行 + §11.6 serverVersion；api-manual §6/§13；SKILL 3 陷阱；README v0.1.100 |
| T15.1 | `mgmt.rs`：MgmtRegistry + `mgmt:` 段五处登记 + 装配校验 + 热更分类 | Given enabled=true 且 0 值/allow 全缺，When 启动，Then fail-loud；段改动 → restart_pending | ✅ | MgmtSection 五处登记（config 字段/known_keys/mapping 探测/restart_pending/装配校验） |
| T15.2 | 装配隔离（serve_cmd 收编轴不进公开面）| Given mgmt 收编轴 `fake`，When `axis('fake')`，Then unknown generic axis（旁路封堵） | ✅ | serve_cmd 分流 `mgmt_generic` → `StableState.mgmt_axes`；Bridge 测试断言 `axis('fake')` unknown |
| T15.3 | `__ops__` 能力位 + read/write 双档门 + 配额 + 审计 | Given 未启用 → fail-closed；op 未自报 → 拒；register ping 失败 → 拒；审计零凭据 | ✅ | 三保留 op 契约；双档（allow_read 缺省复用 allow）；并发闸 4/connector；MGMT_AUDIT/DENY 入 audit.query |
| T15.4 | JS 面 + mini connector 契约测试 | Given 假 vtable connector，When 全锚用例，Then 测试锚全绿 | ✅* | Bridge 级假 vtable（进程内）全绿；**cdylib 版 mini-mgmt 夹具未做**（缺符号路径，随 M2 补） |
| T16.1 | 部署级隧道 recipe 文档 | Given 运维要接跳板机后的库，When 照 recipe，Then ssh -L/autossh/socat 配方可复制 | ✅ | `docs/devkit/deploy-tunnel-recipe.md`（README 清单已登记） |
| R.1 | **合并审查**：三路评审人对 P-1~P-3 全部 diff 复审 | Given 全部阶段 diff，When 三路复审，Then 阻断=0（或修复后=0） | ✅ | 三路（架构/工程/产品）均「修复后放行」；约 20 条发现全部处置（修 17/记录 3），全门禁复绿 |

## 2. 增量记录（随做随更）

- **T11（U-11）**：探测走 `probe_version`（3s 超时）；`db.client.list/register/test` 透出
  `serverVersion/probedAtMs`；测试 8 项（三态 + Bridge 真库）。全量 lib 859 绿。
- **T14 批 a（U-14）**：`introspect.rs` +7 Kind（`catalog_sql` 矩阵 + `mysql_show_create`
  成文例外 + `catalog` 归一器含 constraints 聚合）；`op_db_catalog` + `db.catalog`；
  静态守卫扩 5 件 + Bridge 用例。静态守卫当场抓到 Comment×pg 缺 `current_schema()`——修掉。
  全量 lib 866 绿。
- **T15 M1（U-15）**：`mgmt.rs`（registry/双档门/保留 op 契约/审计/并发闸）+ 装配隔离
  （serve_cmd 分流）+ `mgmt:` 段五处登记 + `globalThis.mgmt` + global.d.ts + Bridge 级假
  vtable 契约测试 3 件（旁路封堵/分档/审计）。全量 lib 869 绿。
- **T16.1**：`deploy-tunnel-recipe.md`（ssh -L/autossh/socat + 安全要点）。
- **R.1 合并审查（三路，diff 23c4322..262ca10）→ 修复**：
  - 代码：`op_mgmt_call` 门禁前置（read 门 → 条目/ops 查找 → 结构化 Unsupported → write 门
    → 并发闸）；`mgmt.register` 插入失败补 best-effort `__disconnect__`；未声明 op 统一
    `OJ_DB_UNSUPPORTED`（capability=op 名，弃虚构的 `OJ_MGMT_UNSUPPORTED`）；call_error 入审计；
    unwrap → ok_or_else；disabled+axes 装配 warn；ops 透出 `readOnly`；
    能力矩阵纠错（pg 有原生 sequences，db-guide/api-manual/CHANGELOG 三处）；
    mysql ddl `name` 空串回落 `Table` 列；constraints 词表统一 pk/uq/ck；
    `db.client.test` 签名与 `serverVersion` 对齐。
  - 测试：开放方言 `db.catalog` 命名用例（oracle → 结构化 unsupported，
    `data.capability=="views"`）；mgmt 3 件 + introspect 35 件 + client_db 8 件复绿。
  - 文档：db-guide mgmt 章红线（`__connect__` 参数含明文 DSN）；scenarios 场景 51；
    global.d.ts（`db.catalog` + `MgmtEntry.ops`）；SKILL/api-manual 错误码同步。
  - 记录性处置（不修，登记）：allow_read 无限制仅 warn（决策）；probe warn 频次维持每次
    fail；LIMIT 截断先于 constraints 聚合的边界（批 b）；U-14 批 b 备忘（mysql CHECK、
    Create View）。
  - 门禁复绿：fmt / clippy `-D warnings` / workspace **1616** 全绿 / `cargo xtask build`
    + smoke ok / xtask 契约 10 绿。

## 3. 诚实登记（避免遗忘）

- U-15 M2（oj-mgmt-mongo 真 connector）与 M3（发版）**不在本计划内**——M1 落地后主分支存在
  未发版增量，下一版 bump 时 CHANGELOG 必须补记。
- U-14 sqlite 的 sequences/routines/comment/constraints/stats 在批 a = **unsupported**（矩阵
  如实体现）；模拟实现按评审归入批 b 工时。
- U-16 仅 T0 文档；SSH 隧道本体维持「不立项、条件触发」。
