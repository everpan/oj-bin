# U-15 非 SQL 管理 connector 轴（首个：mongo）设计稿

> 状态：**设计稿（三路专家评审完成、处置落稿——处置表见 §8；原两处阻断已按评审裁决重写 §2/§3）**。
> 上游：`docs/prds/oj-feature-requirements.md` §5 `U-15`。
> 相邻：U-7（配额与拨号守卫——管理面复用同一 reserve/审计纪律）；U-9（审计——管理操作
> 全部走 `audit::emit` 单一出口）；泛型轴机制（现状，见 §1）。
> 评审：架构路 6 条 / 工程路 6 条 / 产品路 6 条，共 18 条；采纳 16、部分采纳 2、未采纳 0。

## 0. 关键现状：泛型轴已存在，缺的是「管理面语义」

- 插件自报轴：`oj_plugin_axes()`（`plugin_loader.rs:640`）；不在 `TYPED_AXES`
  （`plugin_loader.rs:631`：es/db/blob/bus/kv/auth/mq/mail/ldap）的轴 → **泛型轴**注册进
  `GenericAxisRegistry`（`src/bridge/generic_axis.rs`），JS 经 `axis()` Proxy +
  `op_axis_call(name, op, args)`（`generic_axis.rs:52`）通用调用。
- 即：**传输面零新 ABI**——mongo connector 可以今天就是「一个带 `mongo` 泛型轴的 cdylib 插件」。
- **缺的四件**（本设计的全部增量）：
  1. **登记面**：泛型轴是「清单装配、全有全无」；管理面需要 db.client 同款的运行期
     `register/list/has/delete` + 归属（owner）+ 配额（U-7 同款锁内重判）；
  2. **装配隔离（评审阻断①，特权旁路封堵）**：`serve_cmd.rs:1712` 会把**全部**泛型轴注册进
     公开面，`op_axis_call` 无任何特权门——mgmt connector 若照常进 `GenericAxisRegistry`，
     任意业务 handler 一行 `axis("mongo").call("killOp",…)` 即绕过两档 allow/审计/并发闸。
     **裁决：mgmt 持有的轴名不进公开面**——装配层跳过（或 `op_axis_call` 对已登记 mgmt 的
     轴名报 unknown，双保险，两条都做）；`axis('mongo')` 不可达列为测试锚第 1 条。
  3. **能力位承载（评审阻断②）**：`AxisDecl` 实际只有 `{name, vtable, kind}`
     （`oj-plugin-ffi/src/lib.rs:50-54`）、`GenericVtable` 仅 `{call}`——原稿「每 op 声明
     read_only」**无承载结构，系事实错误**。裁决见 §2（`__ops__` 约定，真零 ABI）。
  4. **审计与分档门**：读/写两档 allow（缺省全 deny fail-closed）+ 逐条 `audit::emit`。

## 1. 范围

**做**：

| 项 | 内容 | 落点 |
|---|---|---|
| 管理面登记 | `mgmt:` 配置段（enabled 默认 false / `allow` 写档门 / `allow_read` 读档门 / `max_entries` / `max_per_owner`）+ 运行期 `MgmtRegistry`（壳 Arc + Mutex，与 ClientDbRegistry 同哲学） | `src/bridge/mgmt.rs`（新）+ `config.rs` + `app.rs` + `serve_cmd.rs` |
| op 家族 | `op_mgmt_register/list/has/delete/call`——`call(connector, op, args)` 经 `GenericVtable.call`（复用 `op_axis_call` 的传输语义 + 前置门） | `mgmt.rs` + `mod.rs` 注册 |
| 特权门 | 登记/删除 = `mgmt.allow`；**call 分档**：op 能力位来自插件 `__ops__` 自报（§2）→ 只读 op 走 `mgmt.allow_read`、非只读走 `mgmt.allow`；两份 allow **缺省全 deny** | `mgmt.rs` |
| 装配隔离 | mgmt 轴名集合冻结于装配期；`serve_cmd` 装配泛型轴时跳过该集合；`op_axis_call` 对该集合报 unknown | `serve_cmd.rs` + `generic_axis.rs` |
| 审计 | register/delete/call 逐条 `MGMT_AUDIT`；拒绝 `MGMT_DENY`——`audit::emit` 单一出口；**全字段零凭据**（连接串 `secret::redact`、args 不入审计） | `mgmt.rs` |
| 配额 | `max_entries` / `max_per_owner`（U-7 同款锁内重判）；per-connector 并发闸（宿主侧 Semaphore `try_acquire`，满即拒不排队——与 FFI 形状无关，成立） | `mgmt.rs` |
| 首个 connector | `oj-mgmt-mongo`（cdylib，`mongo` 泛型轴）：**6 op**——`ping` / `serverStatus` / `currentOp` / `killOp` / `listDatabases` / `replSetGetStatus`；`ping` 存活 → `listDatabases` 盘点 → `currentOp` 观察 → `killOp` 处置 → `serverStatus`/`replSetGetStatus` 健康面，demo 旅程经产品路评审确认够讲通 | `plugins/oj-mgmt-mongo/`（新） |
| 能力位语义 | op 未在插件 `__ops__` 自报表 → 结构化 `OJ_MGMT_UNSUPPORTED`（capability 字段点名），**不透传、不静默**（PRD 验收） | `mgmt.rs` |

**不做**（显式划界）：

- redis/MQ/配置中心 connector——机制就位后各一期。
- TYPED_AXES 扩员 / AxisDecl 结构演进（= ABI 13 提案）——不用 `__ops__` 约定即不需要；
  若未来需要**宿主可验证**的结构化能力声明，再立 ABI 13 项。
- 管理面写操作的事务/回滚语义——由 connector 文档声明，宿主不造回滚。
- 第三档「危险 op + 确认 token」（产品路裁决：过度设计；v1 `killOp` 走写档 + 逐条审计 +
  并发闸，G9 确认门等真实误用案例再上）。

## 2. 能力位承载（评审阻断②的重写裁决）

**采纳工程路方案 (b)：约定保留 op `__ops__`，真零 ABI。**

- 约定：connector 的 `call` 必须实现保留 op `__ops__`（`call(handle, "__ops__", "[]")` 返回
  `[{"op":"ping","readOnly":true}, …]` JSON）；`op_mgmt_register` **登记时探一次并缓存**于
  registry 条目；运行期 `call` 的 op 名不在缓存表 → `OJ_MGMT_UNSUPPORTED`。
- **诚实红线（架构路要求成文）**：`readOnly` 是**插件自报、宿主不可验证**——宿主据此分档判权，
  但插件谎报即绕过。缓解 = connector 全部第一方 + devkit 明示 + mini 夹具钉契约；若未来出现
  第三方 connector 生态，再立 ABI 13（AxisDecl 结构化声明 + 例外窗口在首个泛型轴插件发布时
  已关闭，届时是纯 bump 流程）。
- 既有插件不受影响（`__ops__` 只被 mgmt 面探询；普通泛型轴插件不实现也不受影响——mgmt
  登记时探不到 `__ops__` = **register 拒绝**，fail-loud）。

## 3. 设计要点（评审修订后）

- **正交性**（PRD 背景辨析）：管理面不碰 kv/bus/es 客户面注册表；同一中间件可同时被客户面
  （业务连接）与管理面（DBA 运维）登记，两侧凭据、配额、审计完全独立。
- **句柄解析**（架构路补充）：connector 名 = 泛型轴名；`register` 时查装配期冻结的
  `StableState.generic_axes`（同名冲突已有装配层裁决 `plugin_loader.rs:676`）；条目只存
  owner / 配额记账 / 脱敏连接串 / 能力位缓存 / 句柄引用。
- **JS 面**：`globalThis.mgmt = { register/list/has/delete/call }`（不挂 `db` 下——非 db 轴）。
- **凭据（工程路修正）**：`ENC[...]` 密封值只在 **config 装载期**解封（`config.rs:1511`），
  运行期无 open 路径（U-10 未做）——**运行期 register 只收明文连接串**且入库即
  `secret::redact`；`mgmt:` 配置段支持 `ENC[...]`（依赖既有装配期解封，不依赖 U-10）。
- **ping 语义（工程路钉死）**：**登记时 ping 失败 = register 拒绝**（fail-loud，
  `connector_unreachable`）；登记成功后惰性重探失败 = `list` 出 `alive:false`，不摘表。
- **panic 收敛（架构路成文）**：`catch_future` 是插件侧 helper，宿主对 vtable 调用无
  catch_unwind——panic 收敛是 **connector 实现侧硬性义务**（panic=unwind 红线），
  mini 夹具家族加 `mini-mgmt`（泛型轴 + panic 收敛 + 缺 `__ops__` 路径）。
- **配置段五处登记纪律**（架构路补充，防 U-1 式静默盲区）：config.rs 字段 +
  `known_top_level_keys`（含对账单测）+ null_as_default mapping 探测 + serve_cmd 热更分类
  （裁决：**`mgmt:` 归 `restart_pending`**）+ 装配期 fail-loud 校验（0 值拒启，U-7 口径）。

## 4. 决策表

| 项 | 决策 |
|---|---|
| 轴机制 | 泛型轴（`GenericVtable`），零 ABI 变更 |
| 能力位载体 | 保留 op `__ops__`（插件自报 JSON，登记时探一次缓存）；宿主不可验证性成文 |
| 装配隔离 | mgmt 轴不进公开面：装配跳过 + `op_axis_call` 拒绝（双保险） |
| op 分档 | read → `mgmt.allow_read`；非 read → `mgmt.allow`；缺省全 deny；第三档危险档不做 |
| 未声明 op | `OJ_MGMT_UNSUPPORTED`（fail-loud，PRD 验收） |
| 配额 | `max_entries` / `max_per_owner` / per-connector 并发闸（满即拒不排队） |
| 审计 | `MGMT_AUDIT` / `MGMT_DENY` → `audit::emit`；全字段零凭据（连接串与 args 均不入审计） |
| 默认面 | `enabled` 缺省 false；未启用 → 结构化 fail-closed deny（文案点名 `mgmt.enabled`） |
| 并发语义 | 同 connector 并发 call 满闸即拒（U-7 口径） |
| 版本 | 宿主 + `oj-mgmt-mongo` 各自 bump；插件随宿主同批发 |

## 5. 分期（评审修订估时）

| 批 | 估 | 内容 |
|---|---|---|
| M1 | 3–4 天 | `mgmt.rs`（registry/装配隔离/双档门/审计/配额/op 家族）+ `mgmt:` 段五处登记与装配校验测试 + bootstrap JS 面 + global.d.ts + `mini-mgmt` 夹具 |
| M2 | 3–5 天 | `oj-mgmt-mongo`（6 op + `__ops__` + catch_future 收敛 + env-gated 真 mongo golden） |
| M3 | 1 天 | 文档（db-guide 新 §、api-manual、scenarios 场景 52、SKILL、plugin-development connector 指南）+ devkit 对齐 + 发版 |

## 6. 测试锚（Review Focus，评审增补后）

1. **`axis('mongo')` 不可达**：装配隔离后 `op_axis_call("mongo",…)` 报 unknown（旁路封堵）。
2. `__ops__` 缺失 / op 未自报 → register 拒绝 / `OJ_MGMT_UNSUPPORTED`。
3. read/write 分档：只读 op 走 `allow_read`、非只读走 `allow`；两份 allow 缺省全 deny；
   未启用 → fail-closed deny 文案可辨识。
4. 配额：`max_per_owner` / 并发闸满 → 拒且错误可辨识（U-7 同款口径）。
5. 审计：register/call/deny 可经 `audit.query` 检索（kind=`MGMT_AUDIT`/`MGMT_DENY`）；
   **全字段零凭据**（register 的连接串、call 的 args 均不入审计——命名断言）。
6. 脱敏：`mgmt.list` 连接串全 `***`。
7. panic：mini-mgmt 的 call panic → `RResult::Err`（宿主不 abort）。
8. 登记时 ping 失败 → register 拒绝（fail-loud）；登记后重探失败 → `alive:false` 不摘表。

## 7. 文档面

db-guide 新 §（管理面 connector 章，含「readOnly 自报、宿主不可验证」红线）、api-manual
§6 指针 + §13 错误行、scenarios 场景 52、SKILL、plugin-development（connector 指南：
`__ops__` 契约 + 两档 allow 样例——产品路要求给「两份 allow 通常同主体」的降低理解成本样例）、
README、`global.d.ts`。

## 8. 评审处置表（三路 18 条：采纳 16、部分采纳 2、未采纳 0）

| # | 来源 | 原意见要点 | 处置 |
|---|---|---|---|
| 1 | 产品 | axis() 旁路 = 特权面，非兼容问题；mgmt 轴须隔离 | **采纳**（§0-2 装配隔离双保险 + 锚 1） |
| 2 | 工程 | AxisDecl 无逐 op 结构；建议方案 (b) `__ops__` | **采纳**（§2 全节重写） |
| 3 | 架构 | 同上事实错误；方案 a=ABI 13 例外窗口 / b=describe 探测；read_only 不可验证须成文 | **采纳**（选 b；§2 诚实红线；ABI 13 列为未来项） |
| 4 | 工程 | ENC 只在 config 装载期解封，运行期 register 收 ENC 只能 fail-closed | **采纳**（§3 凭据段修正） |
| 5 | 工程 | 登记 ping 失败语义未钉死 | **采纳**（§3 + 锚 8：登记拒 / 活性降级两段式） |
| 6 | 产品 | op 清单 6 vs 7 对不上账 | **采纳**（§1 定稿 6 op；验收=每声明 op 各 1 条 golden + killOp 对非法 opId 失败路径） |
| 7 | 产品 | 默认面报错文案未定义；审计脱敏验收缺位 | **采纳**（决策表「默认面」行 + 锚 3/5） |
| 8 | 架构 | `mgmt:` 段五处登记纪律 + 热更分类裁决缺失 | **采纳**（§3 末条：restart_pending） |
| 9 | 架构 | MgmtRegistry 与 GenericAxisRegistry 句柄解析关系未写 | **采纳**（§3 句柄解析段） |
| 10 | 架构 | panic 收敛是插件侧义务，宿主无 catch_unwind | **采纳**（§3 + mini-mgmt） |
| 11 | 产品 | 两档 allow 不过复杂、第三档过度设计 | **采纳**（§1 不做划界；devkit 同主体样例进 §7） |
| 12 | 工程 | M2 估时偏乐观（3–5 天）+ mini-mgmt 夹具 | **采纳**（§5） |
| 13 | 工程 | M1 应含 config 解析与装配校验测试 | **采纳**（§5 M1） |
| 14 | 产品 | demo 旅程 6 op 够讲通，不必加 op | **采纳**（§1 括注旅程链） |
| 15 | 产品 | 测试锚补「MGMT_AUDIT/DENY 全字段零凭据」 | **采纳**（锚 5 命名断言） |
| 16 | 产品 | 未启用面文案可辨识 | **采纳**（锚 3） |
| 17 | 架构 | 「零 ABI 变更」结论随能力位方案复核 | **部分采纳**：传输面零 ABI 成立；`__ops__` 是约定不是 ABI——§2 明写这一区分与未来 ABI 13 触发条件 |
| 18 | 工程 | 并发闸可行性（Semaphore 与 FFI 无关） | **采纳**（§1 配额行已按此成立） |
