# dbx Web 对齐 · L5（P0 主旅程）：U-6 / U-8 / U-1 / U-3

> 状态：**设计稿（待四路评审，未实现）**。本文件为 `docs/prds/oj-feature-requirements.md`
> 中 P0 主旅程四项的落地设计。实施前须经四路评审（工程师 / 架构师 / 产品 / 安全）。
> 上游 PRD 基线：oj `0.1.92`（L4 已落地「实例化能力、落盘应用侧自管」）。

## 0. 范围与定位

**本次做（L5）**：

| PRD | 标题 | 落点 |
|---|---|---|
| U-8 | sqlite 错误信封化 | `src/bridge/accessor_sqlx.rs` |
| U-6 | 读侧解码扩展 | `src/bridge/accessor_sqlx.rs`（`Any`/sqlite 路径）+ FFI 插件侧（mysql/pg，见 §1.2 依赖） |
| U-1 | 动态授权运行期生效 + 热更分类 | `StableState` 可变授权表 + 新 op + `diff_configs` 分类 |
| U-3 | 跨 isolate 失效广播 | `ClientDbRegistry` epoch + `bootstrap.js` `DB(name)` 校验 |

**本次不做（留待后续阶，已在评审决策中定调）**：

- **G9 服务端防护门**：与既有 L1 `db_access`（`read_only` 位 / 危险写门 / `max_rows`）重叠，
  决策 = **下沉到 op 层强制**（前端绕过 handler 直打 op 也被拦）。属 L7（GA），本次不碰。
- **U-9 审计落存储**：决策 = **事件外发由方案包落盘**（host 发结构化事件、包订阅落盘）。
  属 L7，本次不改审计落点（仅沿用现有 `DB_CLIENT_AUDIT` stderr 行为）。
- U-4 / U-5（跨请求事务 / 执行取消）、U-7（配额）、U-11/U-14/U-15（P1/P2）均不在 L5。

**协作边界（重申 PRD §0）**：连接持久化 / 凭据加密 / 授权映射 / 语句门 = 方案包负责；
oj 只给执行原语 + 动态登记原语 + 安全门。L5 全部落在 oj 原语层，不引入应用侧编排。

## 1. 四项设计

### 1.1 U-8 sqlite 错误信封化（低风险，建议最先）

**现状**（`src/bridge/accessor_sqlx.rs`）：全部错误为纯文本 `format!("sqlx ...: {e}")`
——站点：`connect`(:43)、`tx query`(:178)、`tx exec`(:198)、`tx commit`(:208)、
`tx rollback`(:218)、`query_with_params`(:261)、`exec_with_params`(:277)。无 `BridgeError`
包装，JS 面拿到的是无 `code`/`retryable`/`sessionDisposition`/`data` 的裸 Error，与
mysql/pg（FFI 信封）不一致。

**设计**：抽一个 `fn sqlite_err(e: sqlx::Error, ctx: &str) -> BridgeError`，按 `sqlx::Error`
变体映射到既有 `DbErrorCategory` 八值（`db_error.rs:16-25`：`Syntax`/`Constraint`/`Timeout`/
`Connection`/`Capacity`/`Unsupported`/`Cancelled`/`Internal`，与 FFI 面同形，`db.rs:525 err_meta`
已定义 `category`/`retryable` 字段）。⚠️ **评审纠正**：plan 初稿的 `OJ_DB_CONFLICT`/`OJ_DB_OTHER`
**不存在**，须改用上述真实八值。映射草图：

- `sqlx::Error::Io` / 文件打开失败 → `Connection`（retryable=false）
- `Database(SQLite code)`：`SQLITE_CONSTRAINT_*` → `Constraint`（retryable=false）；
  `SQLITE_BUSY`/`SQLITE_LOCKED` → `Connection`（retryable=true）；`SQLITE_ERROR`
  （语法）→ `Syntax`（retryable=false）；其余 → `Internal`
- `PoolTimedOut` / `WorkerCrashed` → `Connection`（retryable=true）
- `RowNotFound` 等 → `Internal`

**⚠️ 站点数纠正（评审发现）**：实为 **8 处** `format!` 纯文本 + 1 处内联 `e.to_string()`，
全部须走 `sqlite_err`：
`connect:43`、`begin:242`、**`tx query:178`、`tx exec:198`、`tx commit:208`、`tx rollback:218`、
`query:261`、`exec:277`，以及 **`stream_query:306`**（`e.to_string()` 内联，**未**走信封，
与验收「query 与 stream 同步」矛盾，必须一并改造）。

**⚠️ 凭据脱敏（安全评审·阻塞）**：sqlx 错误**含 DSN（含凭据）**；`ctx` 若带 URL 同样泄露。
信封化时 `message`/`data` **必须**经 `secret::redact`（参照 `client_db.rs:272`）脱敏，验证
`DbErrorCategory` 各变体的原始 `e` 不落盘。

八处统一走 `sqlite_err`，**不改变错误语义、只加结构化字段**。消费方（含 sqlite 专用分支）
可随后删除纯文本回退。`BridgeError` 字段全 pub（`:64-70`），可直接 struct 字面量构造。

### 1.2 U-6 读侧解码扩展（高 ROI，含跨 crate 依赖）

**现状**（`src/bridge/accessor_sqlx.rs:101-140`）：`column_json` 只尝试
`bool`/`i64`/`f64`/`String`/`Vec<u8>` 五类（PRD 称「六类」，含 null 即六）。
mysql 的 `DECIMAL`/日期时间/`JSON`/`BIT`/`GEOMETRY`、pg 的 `numeric`/`json`/`jsonb`/
`uuid`/数组/日期时间一律报错。

**设计（sqlite / `Any` 驱动路径，在 host 内）**：在 `column_json` 现有探测**之前/之后**插入
受控回退层级，把受限类型归一进既有逻辑类型：

- `DECIMAL`/`numeric` → 十进制字符串（保留精度，与现有「decimal 走字符串」纪律一致，
  见 `bootstrap.js:335` 注释）
- 日期时间（`Date`/`Time`/`DateTime`/`NaiveDateTime`）→ ISO 8601 字符串
- `json`/`jsonb`/`uuid` → 文本
- 数组（pg）→ 文本（不展开为 JSON 数组，避免与「JSON 列」语义混）
- `BIT` → 位串文本（`1011` 形式）
- `GEOMETRY` → WKT 文本（推荐，**不静默回退**）

**⚠️ Any 驱动编译风险（工程师评审·阻塞）**：host `SqlxAccessor` 走 sqlx `Any` 驱动
（`accessor_sqlx.rs:13`）。`BigDecimal`/`chrono::DateTime`/`uuid::Uuid` **多半未实现
`Decode<Any>+Type<Any>`**，直接 `try_get::<Option<BigDecimal>>` 会**编不过**。正确做法：
经 `AnyValue`/`try_get_raw` 反射取原始字节，再按列类型名归一成字符串；且 sqlite 动态类型
本就不产生这些强类型，host 侧基本只遇到 `Integer/Real/Text/Blob`——重点是把「Any 驱动下
可能出现的非常规列类型」安全归一并**绝不静默丢值**。

**⚠️ 静默丢值（安全评审·重要）**：现有 `column_json:109` 对无法识别类型 `unwrap_or(Null)`
**静默丢值**，与对抗用例「不静默回退」冲突。改为：未知/不支持类型 → 显式报错（含列类型名），
不返回 `Null`。

**关键依赖（工程师评审确认）**：mysql / postgres 经 FFI 插件（`oj-db-mysql` / `oj-db-postgres`）
返回，解码在**插件侧**，host `Any` 访问器只吃 sqlite 行——plan 初稿「U-6 可能需插件侧改动」
论断**正确**，类型列表大部分不抵达 host，真实改动在插件仓库。

**⚠️ 范围收口（产品评审）**：三方言 golden 依赖真库（`OJ_TEST_MYSQL`/`OJ_TEST_PG` 门控），
L5 内三方言「齐绿」不可保证。决策：**L5 强制交付 sqlite 路径 + sqlite golden；mysql/pg 解码
归插件侧，标 env-gated，插件已归一则自然通过，否则记为 L5 插件侧 follow-up**（不阻塞 L5 合并）。

**验收**：sqlite 建含 datetime/decimal/jsonb/uuid 真业务列表，`select *` 全列可读；
golden 快照断言；既有六类型行为不回退（回归）；mysql/pg 解码 env-gated 单独验收。
`db.query` 与 `db.stream` 同步（二者共用 `column_json`）。

### 1.3 U-1 动态授权运行期生效 + 热更分类

**根因**（PRD 已述）：`db_access` 冻结成 `StableState` 的 `Arc<DbAccessSection>`
（`mod.rs:226`、`app.rs:1259` 装配期 `Arc::new(cfg.db_access.clone())`）；热更复用旧
Backend（`app.rs:1479-1526`「跳过 assemble_backend」）；`db_access`/`db_client` 不在
`diff_configs` 已知键 → 静默盲区。

**设计分两半，互不耦合**：

**(a) 运行期 grant 零重启（核心诉求）** — 复用 L3 的「壳 `Arc` + 内部 `Mutex`」逃逸范式
（`client_db.rs:10-11` 注释同构）：
- `StableState` 新增 `db_access_rt: Arc<Mutex<HashMap<String, DbAccessEntry>>>`（⚠️ 评审纠正：
  `DbAccessEntry` 类型**当前不存在**，须新定义或复用现有 `DbAccess` 结构）。
- 装配期由 `cfg.db_access.by_db` 种子化（`app.rs:1263` 遍历处填充）。
- **合并注入点 = `db_policy::policy_for`（`db_policy.rs:711-722`）**：改为 **config 项 ∪ 运行时表**
  合并，运行时表命中优先、未命中回落 config。静态 `db_access.<name>` 行为完全不变。
- **⚠️ 须接线 L3 fail-closed Deny 分支（`db_policy.rs:719`）**：该分支对「未声明于
  `db_access.by_db` 的动态库」硬置 `Deny`。运行时 grant **必须介入该分支**（命中即不走 719
  Deny），否则 grant 后仍被拒。
- **合并语义（架构师评审·重要）**：运行时表只注入「**存在性 / 放行维**」，**不得携带
  `enforce=Off` 覆盖 config 的 `Deny`/`Read-only`/`max_rows`**（否则构成提权放通）。config 项
  仍按原样 merge defaults 生效。定义并单测该语义。
- 新 op —— **⚠️ 命名改归 `db.access.grant` / `db.access.revoke`（产品评审：属访问控制域，不应
  挂在 `db.client` 登记域）**：与 `db.client.register` **同特权门**（`db_client.allow` 四维并集）、
  **fail-closed**：未配置 `db_client.enabled` 或缺 `allow` 即拒。
  - **⚠️ 主体维度（产品评审·重要）**：`grant(alias, allow)` 的 `allow` 须为**角色/主体集合**，
    查询策略按**当前请求主体**匹配（PRD「按角色授权」）；若只把 alias 加入运行表会变成「该
    alias 全局放行」，违背角色授权——U-1 名实须相符。
  - **⚠️ revoke 防越权（安全评审·重要）**：revoke 只接受**运行时表项**；拒绝撤销 config 兜底项
    （显式报错，不静默），防「任何过 allow 的主体撤销他人运行时授权」的 DoS/越权。revoke 后
    该别名回落 config（非删 config）。
  - **⚠️ 审计（架构师+安全评审）**：grant/revoke 安全敏感度**高于** register，必须发 `audit()`
    （结构化事件），fail-closed。
- **不改 `StableState` 不可变红线**：`db_access` 原 `Arc<DbAccessSection>` 保留（静态兜底），
  新增的 `db_access_rt` 是独立可变表，冻结时序不受影响。

**(b) 热更分类（消除静默盲区）** — `db_access`/`db_client` 改动**不强行热生效**（因
StableState 不可变、强行热改会破坏红线），而是纳入 config diff 已知键，标记为
`restart_pending`，让运维明确知道「改了这两段必须重启」。
- **⚠️ 位置纠正（工程师评审）**：`diff_configs` 实位于 **`oj/src/serve_cmd.rs:577-600`**
  （`:488` 调用，`:587` 分支：`db/redis/blob/...` 与 `platform/fs`→`restart_pending`，`server`
  →`kept_listener`，其余 `_ => applied`）。`db_access`/`db_client` 未列入→误标 `applied`，
  静默盲区坐实。修复：在 `:587` 分支加 `"db_access" | "db_client" => restart_pending.push`。
- **⚠️ 安全提示（安全评审）**：`restart_pending` 缺「当前实例仍为旧授权」提示；diff 结果额外
  返回受影响的安全面文案，缩小运维误判窗口。

**验收**：`register → grant(按角色) → 首查即通`，全程零重启；改 `db_access` 段触发热更时
`applied`/`restart_pending` 必居其一并明示；未授权主体查询/内省仍 Deny（不回退）；
revoke 仅回落 config、不删 config；revoke 他人 config 项显式报错；grant/revoke 落审计。
（跨 isolate 即时性依赖运行表全局共享 + §1.4，无需 epoch 兜底授权。）

### 1.4 U-3 跨 isolate 失效（epoch 方案，优于广播）

**现状**：`DB(name)` 按 isolate 缓存（`bootstrap.js:914 dbCache`）；`delete`/`reRegister`
只逐出调用方 isolate（`bootstrap.js:1097 dbCache.delete`，注释自认 :1087-1088）。
其他 isolate 仍可能拿到已删/已轮换连接。

**设计（世代号，host 单源）**：
- `ClientDbRegistry`（`client_db.rs:67`）新增 `epoch: Mutex<HashMap<String, u64>>`；
  **⚠️ 落点纠正（工程师评审）**：`epoch += 1` 须放在 `register`/`re_register`/`remove`
  （`:95`/`:116`/`:144`）**内部**，覆盖全部路径，而非 op 体（`:333`/`:382`）。
- `ClientDbEntry`（`:40`）无需改动；bootstrap 侧缓存 `(entry, epoch)`。
- **⚠️ 缓存短路修复（工程师评审·重要）**：当前 `DB(name)`（`bootstrap.js:914` 起）
  `if(!dbCache.has(name)){...}` 命中即返回，**cache-hit 路径不再校验**。plan 初稿「每次解析
  都校验」在 cache-hit **不会触发**。必须改为：缓存存 `(entry, epoch)`，**每次** `DB(name)`
  调用都比较 epoch（cache-hit 路径也走比较），不一致才重建。
- 新 op `op_db_epoch(name) -> u64`（⚠️ 标 `#[op2(fast)]` 同步读 map，与 `op_db_client_has`
  同形，避免 async 往返；无特权）。
- **⚠️ JS 持有 epoch 可被绕过（安全评审·重要）**：`dbCache` 是 JS 对象，恶意 handler 可取
  host epoch 后覆写 `cached.epoch` 使比较「相等」→ 跳过失效、复用已删连接。缓解：**使正确性
  不依赖 JS 持有的 epoch**——确认查询 op（`op_db_query` 等）按 `name` 从 host `client_dbs`
  注册表**重新解析 accessor**（若现有即如此，则 U-3 连接陈旧天然成立，epoch 仅优化 wrapper
  新鲜度/last_error）；若查询 op 捕获了 JS 侧 accessor，则须改为按名重解析。实施前先核实
  查询 op 的 accessor 解析路径。
- 与 PRD「随用随取」契合：包侧 `has()→缺则 register 回放`，host epoch 单源即足够保证一致。

**验收**：isolate A 持有旧连接；isolate B `delete`/`reRegister` 后，A 上新 `DB(name)` 请求
拿到新 accessor 或明确失败（Deny），绝不复用已删连接。e2e 用**多 Runtime**（多 handler 调用，
强制取不同 runtime，防池复用退化成同 isolate）断言。

## 2. 统一基础设施（本次是否引入）

U-1 授权表 / L6 的 U-4 事务注册表 / U-5 执行注册表 / U-7 预算计数器，本质都是
「`StableState` 内租户作用域 `Mutex<HashMap>`」范式。L5 仅引入 `db_access_rt` 与
`epoch` 两张小表，**不抽共享 `RuntimeRegistry`**（避免过度设计）；若在 L6 实施时发现三张表
范式高度重复，再抽公共基础设施（待 L6 评审时定）。

## 3. 契约 / 配置变化

- 新增 op：`op_db_access_grant`、`op_db_access_revoke`、`op_db_epoch`
  （命名归 `db.access` 访问控制域，呼应 §1.3 产品评审）。
  - **⚠️ 白名单（工程师评审）**：`db.rs:1493` 守护测试只扫描 `db::`/`query::`（`:1525`），
    `client_db::op_db_client_*` **不在其列**（现有 register 等亦未登记）。grant/revoke/epoch
    若在 `client_db.rs` 不会被该测试钉死；若放 `db.rs` 须扩 `WHITELIST`（现 22）。
- `db_client:` 段不变（`enabled` / `max_entries` / `allow`）；grant/revoke 复用 `allow` 门。
- `db_access` / `db_client` 纳入热更 diff 已知键（`serve_cmd.rs:587` 分支加 `restart_pending`）。
- **不改 ABI_VERSION**（无 repr(C) 变更；纯 host 侧 + JS 全局）。

## 4. 测试策略

- **U-8**：sqlite 触发约束冲突 / 语法错误 / 文件失败，断言 JS 面 `Error.code` 为对应
  `DbErrorCategory` 八值之一、`retryable`/`sessionDisposition` 存在；断言 `message`/`data`
  **不含 DSN/凭据**（脱敏）；构造「sqlite 专用纯文本回退」被删（含 `stream_query:306` 路径）。
- **U-6**：**sqlite 路径强制** golden 快照（含 datetime/decimal/jsonb/uuid 真业务列），
  `select *` 全列可读；既有六类型回归；未知类型 → 显式报错（非 `Null`，验证不静默丢值）。
  mysql/pg 解码 **env-gated**（`OJ_TEST_MYSQL`/`OJ_TEST_PG`），单独验收、不阻塞 L5。
- **U-1**：`register→grant(按角色)→首查即通` 零重启 e2e；revoke 回落 config（非删）；
  revoke 他人 config 项显式报错；未授权 Deny 不回退；静态 `db_access.<name>` 行为不变；
  `db_client` 未 enable 时 grant 拒；合并语义单测（运行时项不覆盖 config 的
  enforce/readonly/max_rows）；grant/revoke 落审计。
- **U-3**：**多 Runtime** 失效 e2e（A 旧 / B delete / A 新请求拿到新或失败，强制取不同
  runtime 防同 isolate 退化）；核实查询 op 按名重解析 accessor（正确性不依赖 JS epoch）。
- **对抗用例**（本仓纪律 `feedback_probing_tests`）：grant 无 `allow` 配置 → 拒；
  revoke 不存在别名 → 幂等；恶意 JS 覆写 `cached.epoch` 绕过失效 → 仍拿不到已删连接
  （验证 host 重解析兜底）；U-6 未知方言类型 → 不静默回退。

## 5. 文档（实施时同步）

- `docs/devkit/` 四件：api-manual（§6 db/`DB(name)` 错误信封表 + `db.access.grant/revoke` +
  `db.epoch`；§13 限制表补「动态授权运行期生效」「revoke 回落 config 非删」）/ scenarios
  （场景 42 扩 grant/revoke + **跨 handler 演示授权即时生效** + 跨 isolate 失效）/ SKILL
  （陷阱速查：revoke 回落 config、grant 后跨 isolate 即时生效）/ README（版本同步段）。
- `docs/db-guide.md` §11.5（访问控制）+ §11.6（动态登记）同步。
- `CHANGELOG.md`：新增 v0.1.93（L5）条目。
- `cargo xtask build` 归置 `bin/devkit/` + `cargo test --release -p xtask` 契约校验。

## 6. 四路评审结论与处置（2026-10-28）

四路（工程师 / 架构师 / 产品 / 安全）并行只读评审完成。结论：**方向可落地，但有多处阻塞级
设计缺陷须先订正（已就地修订 §1-§5）**。评审意见不全盘照做，以下为处置登记。

### 6.1 已订正的阻塞/重要项（已写入正文）

| # | 评审方 | 发现 | 处置（落点） |
|---|---|---|---|
| 1 | 工程师 | U-8 枚举名 `OJ_DB_CONFLICT/OTHER` 不存在 | 改用真实八值 `DbErrorCategory`（`db_error.rs:16-25`）✓ §1.1 |
| 2 | 工程师 | U-8 站点数错（7→8），漏 `begin:242` 与 `stream_query:306` | 补全 8 处 + stream 路径一并改造 ✓ §1.1 |
| 3 | 工程师 | U-6 `BigDecimal`/chrono/uuid 未实现 `Decode<Any>` → 编不过 | 改 `AnyValue`/`try_get_raw` 反射归一字符串 ✓ §1.2 |
| 4 | 安全 | U-8 sqlx 错误含 DSN/凭据泄露 | 信封化经 `secret::redact` 脱敏 ✓ §1.1 |
| 5 | 工程师 | `diff_configs` 实位于 `serve_cmd.rs:577-600`（非 config.rs） | 修正位置 + `:587` 加 `restart_pending` ✓ §1.3 |
| 6 | 架构师 | U-1 须接线 `db_policy.rs:719` fail-closed Deny 分支 | 运行时 grant 介入该分支 ✓ §1.3 |
| 7 | 架构师/安全 | 运行时表不得携带 `enforce=Off` 覆盖 config（提权） | 运行时只注入存在性/放行维，单测 ✓ §1.3 |
| 8 | 安全 | revoke 可撤销他人运行时授权（DoS/越权） | revoke 只接受运行时项、拒 config 项 ✓ §1.3 |
| 9 | 产品 | grant 退化为全局放行，违背「按角色授权」 | `allow` = 角色/主体集合，按当前请求主体匹配 ✓ §1.3 |
| 10 | 产品 | grant/revoke 命名挂 `db.client` 易混淆 | 改归 `db.access.grant/revoke` ✓ §1.3/§3 |
| 11 | 工程师 | U-3 `DB(name)` cache-hit 不校验 epoch（短路） | 缓存存 `(entry,epoch)`，每次调用比较 ✓ §1.4 |
| 12 | 安全 | JS 持 epoch 可被恶意覆写绕过 | 正确性依赖 host 按名重解析 accessor，epoch 仅优化 ✓ §1.4 |
| 13 | 安全 | U-6 未知类型 `unwrap_or(Null)` 静默丢值 | 改为显式报错 ✓ §1.2 |
| 14 | 产品 | U-6 三方言 golden 不可齐绿 | sqlite 强制 + mysql/pg env-gated ✓ §1.2/§4 |
| 15 | 架构师+安全 | grant/revoke 安全敏感度高于 register | 必发 `audit()` ✓ §1.3 |
| 16 | 工程师 | `db.rs:1493` 白名单只扫 `db::`/`query::` | grant/revoke/epoch 若在 client_db.rs 不被钉；放 db.rs 须扩 WHITELIST ✓ §3 |

### 6.2 仍需实施前核实/确认

- **U-1 合并注入点**：`db_policy::policy_for` 现有签名与 `DbAccess` 结构需通读确认
  `db_access_rt` 字段形态与 merge 实现（工程师已在 `:711-722`/`:719` 定位，落地前精读）。
- **U-3 查询 op 解析路径**：确认 `op_db_query` 等是否按 `name` 从 host registry 重解析
  accessor（决定 epoch 是否需要承担正确性兜底）。
- **`DbAccess` 复用 vs 新 `DbAccessEntry`**：U-1 运行表项的结构需在编码时定。
- **G9 / U-9 留 L7**：本 plan 已划清边界，评审确认不跨阶（G9=下沉 op 层强制；
  U-9=事件外发由方案包落盘）。

### 6.3 评审总体 verdict

**条件通过**：设计稿经上述 16 项订正后可进入实施；实施前须完成 6.2 三项核实。L5 不跨阶
触碰 G9/U-9/L6/L7。

## 7. 门禁

`cargo fmt --check` · `cargo clippy --release --workspace --all-targets -- -D warnings` ·
`cargo xtask build` · `cargo test --release -p xtask`（devkit 契约）· `cargo xtask smoke` ·
`cargo test --release --workspace`（注：基线 `bridge::db_policy::behavior` 6 例失败为
既有问题，与 L5 无关，单独排查）。
