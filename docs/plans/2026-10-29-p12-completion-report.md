# P1/P2 第一批完成情况报告（回复需求方）

> 日期：2026-10-29
> 答复对象：dbx Web 对齐需求（PRD `docs/prds/oj-feature-requirements.md` §5 U-11 / U-14 / U-15 / U-16）
> 依据：执行跟踪 `docs/plans/2026-10-28-p12-execution-plan.md`（任务真相源，全部任务已落账 ✅）
> 结论先行：**本批四项全部交付并通过三路合并审查，已发版 v0.1.100（tag 已推 origin），全门禁绿。**

## 1. 版本与产物

| 项 | 内容 |
|---|---|
| 版本 | **v0.1.100**（零 ABI 变更，ABI 12 保持——对既有插件零破坏） |
| 提交 | `262ca10`（功能主体，tag v0.1.100 指向此）→ `955a7c7`（R.1 审查修订，main） |
| 推送 | origin main + 仅 tag `v0.1.100`；其余本地 tag 未推 |
| 文档 | devkit 四件（api-manual / scenarios / SKILL / README）+ db-guide（catalog 行 / §11.6 serverVersion / §11.8 管理面章）+ `deploy-tunnel-recipe.md` + `sample/global.d.ts` 均已同步 |

## 2. 需求逐项对照

### U-11 连接级能力探测 — ✅ 交付

| PRD 要求 | 交付情况 |
|---|---|
| connect 后探测 server_version（mysql `select version()` / pg） | `probe_version(_with)`：方言分派，3s 超时（可参），探测失败/挂起 → None + warn（每次 fail 都记） |
| per-alias 缓存快照并经 `db.client.list` 透出 | `ClientDbEntry.serverVersion / probedAtMs`；register / reRegister（换 DSN 后更新）/ `db.client.test` 三处同源透出 |
| `describe()` 工厂级 caps 分派维持不变 | 未触碰，行为不变 |

偏差（已登记）：开放方言（非 mysql/pg/sqlite）规划内跳过探测、无 warn、不发 SQL；探测值来自连接时的一次性快照，不保活刷新。

### U-14 元数据目录内省扩展 — ✅ 批 a 交付（批 b 另期）

| PRD 要求 | 交付情况 |
|---|---|
| 三批交付：a 浏览增强（DDL/注释/constraints/统计）+ 管理对象（sequences/routines/views…） | **批 a 落地 7 类**：views / routines / sequences / ddl / comment / constraints / stats，统一 `db.catalog(kind, arg?)` 入口，信封 `{kind, dialect, items}` |
| per-dialect 内省契约 | `catalog_sql` 三方言矩阵：mysql 全量（含 sequences 模拟）+ pg 全量（**pg 用原生 `pg_sequences`**，评审纠错）；sqlite 仅 views/ddl，其余如实报 unsupported（不静默回退） |
| 统一归一 shape | constraints 按 name 聚合、类型词表统一 pk/uq/ck；mysql `SHOW CREATE` 走白名单校验（零内联铁律唯一成文例外） |
| granted_only 贯穿 | op 层授权链 + 未授权别名 `db_not_granted` |
| 未知方言报 unsupported 不静默回退 | 结构化 `OJ_DB_UNSUPPORTED`（`data.capability` 点名），并有开放方言命名测试钉死（oracle → views） |

偏差（已登记）：sqlite 的 sequences/routines/comment/constraints/stats 在批 a = unsupported（模拟实现按评审归入批 b）；mysql/pg golden 快照需真库（env-gated，批 b 补）；triggers/partitions/types 等其余 PRD 清单项按分批计划不在批 a。

### U-15 非 SQL 管理 connector 轴 — ✅ M1 交付（M2 mongo connector 另期）

| PRD 验收要求 | 交付情况 |
|---|---|
| 登记 → 管理操作 → 审计 → 配额 全链走通 | **全链落地**：`mgmt.register`（`__ops__` 能力位声明 → `__connect__` → ping 失败即拒）→ `mgmt.call`（门禁前置：read 门 → 能力位校验 → write 门 → 并发闸 → 审计）→ `mgmt.list/delete/has`；配额 max_entries=8 / max_per_owner=4 / 并发 4 per connector；`MGMT_AUDIT/DENY` 入 `audit.query`（零凭据落审计） |
| 能力位声明未实现即报 unsupported | 未声明 op 一律结构化拒绝（`OJ_DB_UNSUPPORTED`，capability=op 名）；声明即契约 |
| 与客户面正交 | 独立 `mgmt:` 配置段（五处登记 + 热更分类 restart_pending）+ 装配隔离：mgmt 收编轴**不进公开 `axis()` 面**（旁路封堵有测试钉死） |
| 复用 sidecar 生命周期契约 | M1 用宿主进程内 registry + 既有审计/门禁设施；清单锁/指纹等 sidecar 语义随 M2 真 connector 对齐 |

JS 面：`globalThis.mgmt = {register, delete, list, has, call}`；`mgmt.list` 透出 `ops[{name, readOnly}]`。

### U-16 服务端 SSH 隧道（条件项）— ✅ 按裁决不立项，T0 前置交付

产品裁决维持「客户端不触达跳板机后的库」，**不立项**。零成本前置已交付：`docs/devkit/deploy-tunnel-recipe.md`（ssh -L / autossh / socat 部署级配方 + 安全要点，README 清单已登记）。**重启条件**：仅当该产品取舍被推翻才立项（设计稿已备好条件触发条款）。

## 3. 合并审查（R.1）——三路评审，均「修复后放行」

架构 / 工程 / 产品三路对全部阶段 diff（23c4322..262ca10）复审，约 20 条发现**全部处置**（修 17 / 记录 3），要点：

- `mgmt.call` 门禁前置排序；未声明 op 统一结构化错误码（弃虚构的 `OJ_MGMT_UNSUPPORTED`）
- `mgmt.register` 插入失败补 best-effort `__disconnect__`（连接泄漏修复）；call_error 入审计
- 能力矩阵纠错（**pg 有原生 sequences**，三处文档同步）；constraints 词表统一
- 装配校验补 disabled+axes warn；`ops` 透出 readOnly
- 文档面：db-guide 管理面红线（`__connect__` 参数含明文 DSN）、scenarios 场景 51、global.d.ts 类型、SKILL 陷阱

## 4. 质量门禁（全绿）

| 门禁 | 结果 |
|---|---|
| `cargo fmt` / `cargo clippy --release --workspace --all-targets -D warnings` | ✅ |
| `cargo test --release --workspace` | ✅ **1616** 全绿（含新增：探测三态 + Bridge 真库、catalog 静态守卫 5 件 + sqlite 实测矩阵、mgmt 契约 3 件、开放方言命名用例） |
| `cargo xtask build` + `smoke`（隐藏构建机源后最小 build） | ✅ |
| devkit 契约（`cargo test --release -p xtask`） | ✅ 10 绿（文档与产物一致） |

## 5. 诚实登记（与 PRD 的已知边界，避免遗忘）

1. **U-15 readOnly 是 connector 自报**，宿主无法验证其真实只读性——信任边界在设计稿成文；连接级强制仍依赖 DB 侧 GRANT。
2. **`__connect__` 参数含明文 DSN**（连接器自持凭据形态决定），已在 db-guide 管理面章标注红线；配置面凭据仍走 `ENC[...]` 密封。
3. `allow_read` 配置为无限制集合时仅 warn 不拒绝（决策：warn-only）。
4. 审计为进程内环形缓冲，不跨重启、防篡改不在本批范围。
5. 探测/内省不覆盖查询级超时与字节上限（查询防护沿用既有机制，不在本批扩围）。

## 6. 遗留与后续

| 项 | 内容 | 期 |
|---|---|---|
| U-14 批 b | sqlite 模拟实现（sequences/routines/comment/constraints/stats）+ mysql/pg golden + 备忘（mysql CHECK、Create View）| v0.1.101 |
| U-15 M2/M3 | oj-mgmt-mongo 真 connector + cdylib mini-mgmt 夹具（缺符号路径）+ sidecar 生命周期对齐 → 发版 | 另期 |
| U-16 | 维持不立项；产品取舍被推翻时按设计稿条件触发 | 条件触发 |

—— 完 ——
