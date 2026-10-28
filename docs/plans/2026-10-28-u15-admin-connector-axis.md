# U-15 非 SQL 管理 connector 轴（首个：mongo）设计稿

> 状态：**设计稿（待四路评审；评审处置表见 §8——本项是新轴，安全路为最低放行门槛）**。
> 上游：`docs/prds/oj-feature-requirements.md` §5 `U-15`。
> 相邻：U-7（配额与拨号守卫——管理面复用同一 reserve/审计纪律）；U-9（审计——管理操作
> 全部落 `MGMT_AUDIT`/`MGMT_DENY`，走 `audit::emit` 单一出口）；泛型轴机制（现状，见 §1）。

## 0. 关键现状：泛型轴已存在，缺的是「管理面语义」

- 插件自报轴：`oj_plugin_axes()`（`plugin_loader.rs:640`）；不在 `TYPED_AXES`
  （`plugin_loader.rs:631`：es/db/blob/bus/kv/auth/mq/mail/ldap）的轴 → **泛型轴**注册进
  `GenericAxisRegistry`（`src/bridge/generic_axis.rs`），JS 经 `axis()` Proxy +
  `op_axis_call(name, op, args)`（`generic_axis.rs:52`）通用调用。
- 即：**传输面零新 ABI**——mongo connector 可以今天就是「一个带 `mongo` 泛型轴的 cdylib 插件」。
- **缺的三件**（本设计的全部增量）：
  1. **登记面**：泛型轴现在是「清单装配、全有全无」；管理面需要 db.client 同款的
     运行期 `register/list/has/delete` + 归属（owner）+ 配额（reserve 模式）；
  2. **审计**：`op_axis_call` 无审计、无特权门——管理操作（KILL/配置写/集群操作）必须
     逐条落 `audit::emit`，读门/操作门分档；
  3. **能力位声明语义**：`AxisDecl` 已带能力位，但「声明未实现 → unsupported」的
     fail-loud 语义要在管理面固化（PRD 验收明文）。

## 1. 范围

**做**：

| 项 | 内容 | 落点 |
|---|---|---|
| 管理面登记 | `mgmt:` 配置段（enabled 默认 false / `allow` 登记门 / `max_entries` / `max_per_owner`）+ 运行期 `MgmtRegistry`（壳 Arc + Mutex，与 ClientDbRegistry 同哲学） | `src/bridge/mgmt.rs`（新）+ `config.rs` + `app.rs` 装配校验 |
| op 家族 | `op_mgmt_register/list/has/delete/call`——`call(connector, op, args)` 经 `GenericVtable`（复用 `op_axis_call` 的传输语义 + 前置门） | `mgmt.rs` + `mod.rs` 注册 |
| 特权门 | 登记/删除 = `mgmt.allow`（四维并集，同 client_gate）；**call 分档**：插件 `AxisDecl` 声明 op 的 `read_only` 能力位 → 读档走 `mgmt.allow_read`、写/危险档走 `mgmt.allow`（两份 allow；缺省 read=deny） | `mgmt.rs` |
| 审计 | register/delete/call 逐条 `MGMT_AUDIT`；拒绝 `MGMT_DENY`——`audit::emit` 单一出口 | `mgmt.rs` |
| 配额 | `max_entries` / `max_per_owner`（U-7 同款锁内重判）；`call` 并发闸（per-connector Semaphore，参照 STREAM_MAX_CONNS 但由宿主持） | `mgmt.rs` |
| 首个 connector | `oj-mgmt-mongo`（cdylib，`mongo` 泛型轴）：`ping` / `serverStatus` / `currentOp` / `killOp` / `listDatabases` / `replSetGetStatus`——read_only 能力位声明齐；**KILL/写配置 op 声明为非只读** | `plugins/oj-mgmt-mongo/`（新） |
| 能力位语义 | `call` 的 op 未在插件 `AxisDecl.ops` 声明 → 结构化 `OJ_MGMT_UNSUPPORTED`（capability 字段点名），**不透传、不静默** | `mgmt.rs` + 插件 describe 契约 |

**不做**（显式划界）：

- redis/MQ/配置中心 connector——机制就位后各一期（每个都是「声明 ops + read_only 分档 + golden 实测」）。
- TYPED_AXES 扩员（`mgmt` 进 typed 列表 = ABI 面）——不用：泛型轴 vtable 足够（call 语义就是
  bytes-in/bytes-out）。
- 管理面**写操作的事务/回滚语义**——DBA 运维操作天然幂等性各异，由插件文档声明；宿主不造回滚。
- 跨 connector 联合查询/编排。

## 2. 设计要点

- **正交性**（PRD 背景辨析）：管理面不碰 kv/bus/es 客户面注册表；一个中间件可以同时被
  客户面（`kv`/`bus` 轴，业务连接）与管理面（`mongo`/`redis-mgmt` 泛型轴，DBA 运维）登记，
  两侧凭据、配额、审计完全独立。
- **JS 面**：`globalThis.mgmt = { register/list/has/delete/call }`（与 `db.client` 同款形态；
  不挂在 `db` 下——它不是 db 轴）。
- **传输**：`op_mgmt_call` 直接复用 `GenericVtable.call`（`FfiFuture` + bytes），panic 收敛
  走既有 `catch_future`（panic=unwind 红线）。
- **清单锁/指纹/存活探测**：复用插件加载既有契约（`PluginLoader` ABI 门禁 + 指纹 + 声明）；
  存活探测 = `ping` op（connector 必须声明，登记时探一次 + `mgmt.list` 惰性重探，降级
  不摘表——与 U-11 同降级哲学）。
- **凭据**：register 收 DSN/连接串，`secret::redact` 全脱敏（与 db.client 同规）；
  `ENC[...]` 密封值照常支持。

## 3. 决策表

| 项 | 决策 |
|---|---|
| 轴机制 | 泛型轴（`GenericVtable`），零 ABI 变更 |
| op 分档 | 插件 `AxisDecl` 每 op 声明 `read_only`；宿主两份 allow（read/write）分档判权 |
| 未声明 op | `OJ_MGMT_UNSUPPORTED`（fail-loud，PRD 验收） |
| 配额 | `max_entries` / `max_per_owner` / per-connector 并发闸（宿主 Semaphore） |
| 审计 | `MGMT_AUDIT` / `MGMT_DENY` → `audit::emit` |
| 并发语义 | 同 connector 并发 call 排队（Semaphore 满即拒不排队，U-7 口径） |
| 版本 | 宿主 + `oj-mgmt-mongo` 各自 bump；插件随宿主同批发 |

## 4. 分期

| 批 | 估 | 内容 |
|---|---|---|
| M1 | 3 天 | `mgmt.rs`（registry/门/审计/配额/op 家族）+ bootstrap JS 面 + global.d.ts + 配置段 |
| M2 | 2–3 天 | `oj-mgmt-mongo`（7 op + 能力位 + golden 实测 env-gated：真 mongo 单实例） |
| M3 | 1 天 | 文档（db-guide 新 §、api-manual、scenarios 场景 52、SKILL）+ devkit 对齐 + 发版 |

## 5. 测试锚（Review Focus）

1. 未声明 op → `OJ_MGMT_UNSUPPORTED`（不透传 `call`，fail-loud）。
2. read/write 分档：read-only op 走 `allow_read` 命中、写档主体被拒（反之亦然）；两份 allow
   缺省全 deny（fail-closed）。
3. 配额：`max_per_owner` / 并发闸满 → 拒且错误可辨识（U-7 同款文案口径）。
4. 审计：register/call/deny 三类事件在 `audit.query` 可检索（kind=`MGMT_AUDIT`/`MGMT_DENY`）。
5. panic 收敛：插件 call panic → `RResult::Err`（宿主不 abort——mini 插件家族夹具同款）。
6. 脱敏：`mgmt.list` 的 DSN 全 `***`。

## 6. 风险

| 风险 | 处置 |
|---|---|
| 泛型轴无类型化契约，op 形状靠文档 | 每connector 必须带 `.d.ts`/devkit 段 + golden 实测；宿主只保证信封与门 |
| KILL/写操作被误用为攻击面 | 写档 allow 独立且缺省 deny；审计逐条；并发闸限速 |
| mongo 驱动体积/依赖 | 插件自持依赖（cdylib 隔离），宿主零依赖 |

## 7. 文档面

db-guide 新 §（管理面 connector 章）、api-manual §6 指针 + §13 错误行、scenarios 场景 52、
SKILL、plugin-development（connector 开发指南：AxisDecl ops 契约 + read_only 分档语义）、
README、`global.d.ts`。

## 8. 待评审问题（四路）

| 路 | 问题 |
|---|---|
| 安全 | 两份 allow（read/write 分档）是否足够？要不要第三档「危险」（KILL/集群变更独立 token，G9 确认门同款）？——建议 v1 两档 + 危险 op 直接拒（插件不声明），评审定夺 |
| 兼容 | `mgmt` 全局名 vs 并入 `db.client` 同级 `client` 命名空间？泛型轴 `axis()` Proxy 与 `mgmt.call` 的关系（并存会否混淆）？ |
| 性能 | 并发闸粒度 per-connector 是否够（要不要 per-op 分闸）？ |
| 产品 | 首个 connector 用 mongo（PRD 建议）确认？M2 的 7 个 op 清单是否覆盖 dbx 首屏？ |
