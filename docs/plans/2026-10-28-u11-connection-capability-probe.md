# U-11 连接级能力探测（server_version 快照）设计稿

> 状态：**设计稿（待四路评审；评审处置表见 §7，动工前须过评审）**。
> 上游：`docs/prds/oj-feature-requirements.md` §5 `U-11`。
> 相邻：U-7（v0.1.97，client 轴配额与拨号守卫——本设计的探测挂在 register/reRegister 的
> connect 之后，与守卫同链路）；U-14（内省扩展——server_version 是其 golden 快照的定位键）。

## 0. 范围

**做**：

| 项 | 内容 | 落点 |
|---|---|---|
| 探测 | connect 成功后跑**一条**版本查询：mysql `select version()`、pg `select version()`、sqlite `select sqlite_version()`（统一走既有 `DataAccessor::query_with_params`，取首行首列字符串） | `src/bridge/client_db.rs`（register / reRegister 的 connect 成功分支） |
| 缓存 | `ClientDbEntry` 增 `server_version: Mutex<Option<String>>` + `probed_at_ms: u64`；探活失败 = None + `warn`（**降级不阻断登记**） | `client_db.rs` |
| 透出 | `db.client.list` / `register` 返回值增 `serverVersion`（null = 未探到）与 `probedAtMs` | `client_db.rs::summary` + `global.d.ts` |
| 静态库 | `db.allowed()` 不加（静态库面向业务 handler，无连接树语义）；仅 client 轴 | — |

**不做**（显式划界）：

- **`describe()` 工厂级 caps 分派不变**（PRD 明文）：`DialectCaps`（placeholder/id/stream/cancel）
  仍是装配期声明面；探测结果是**实例级观测值**，不参与能力判定（防止「探测到旧版本就静默降级」）。
- 版本轮换监测（长连连接的 server 升级感知）——重连（reRegister）即重探，够用。
- `show server_version`（PRD 提到的 pg 备选）不采用：`select version()` 三方言同形、走绑定通路。
- auth 插件 / 其它轴的版本探测（只有 db 轴有「连接」语义）。

## 1. 现状（file:line）

- connect：`DbBackendRegistry::connect`（`src/bridge/db_backend.rs:63`）→ 返回 `Arc<dyn DataAccessor>`。
- register / reRegister：`src/bridge/client_db.rs:270/372`，connect 成功后构造 `ClientDbEntry`
  （`dsn_redacted` / `dialect` / `registered_by` / `queries`）。探测就插在构造前。
- summary：`client_db.rs:55-64`（`db.client.list` 元素形状）。
- 方言名：`accessor.dialect_info().name`（`"sqlite" | "mysql" | "postgres"`；开放方言其它名 → 探测跳过）。

## 2. 设计要点

- **宿主侧实现、零 ABI**：版本查询是普通参数化查询，走既有 accessor 通路（含插件 typed 池）。
  不加 vtable 槽、不碰 `describe()`。
- **探测 SQL 按方言名分派**（三已知方言之外的开放方言：跳过探测，`serverVersion=null`——
  不猜 SQL，未知方言报不报错都属臆测）。
- **降级语义**：探测查询失败（权限/驱动差异）→ `eprintln!` warn + None；**登记照常成功**——
  探测是观测增强，不是登记门（否则一个只读监控账号会连不上库）。
- **串行成本**：每条连接一次单行查询（<1ms 量级）；挂在 connect 之后（已有拨号守卫 5s 上限）。
- **`grant` 的动态库**（U-1 运行期授权）不重探：探测跟随 register/reRegister 生命周期。

## 3. 决策表

| 项 | 决策 |
|---|---|
| 探测时机 | register / reRegister connect 成功后、entry 构造前，一次 |
| SQL | mysql/pg `select version()`；sqlite `select sqlite_version()`；开放方言跳过 |
| 失败 | warn + None，登记照常（降级不阻断） |
| 字段 | `serverVersion: string \| null`、`probedAtMs: number` |
| ABI | 无（12 不变） |
| 配置 | 无新键（观测增强不设开关；不开 client 轴即不存在此面） |

## 4. 测试锚（验收）

1. sqlite 动态库：register 后 `db.client.list()[0].serverVersion` 形如 `3.x.y`；`probedAtMs > 0`。
2. 探测失败降级：fake accessor（版本查询报错）→ 登记成功、`serverVersion === null`。
3. reRegister 重探：换 DSN 后版本字段更新。
4. mysql/pg 真库（env-gated）：版本串非空且与 `select version()` 一致（golden 锚，U-14 复用）。

## 5. 分期

单期交付（估 0.5 天含文档）；建议与 **U-14 批次 a 同车**（golden 快照的 `server` 定位键由本项产出）。

## 6. 文档面

db-guide §11.6（字段 + 降级语义）、api-manual §6 指针、CHANGELOG；devkit 四件对齐；`global.d.ts`。

## 7. 待评审问题（四路）

| 路 | 问题 |
|---|---|
| 安全 | 版本串入 `db.client.list`（管理面，已有 `client_gate`）是否可接受？——建议接受：版本信息对攻击者的价值远低于其对排障的价值，且 DSN/凭据照旧零明文 |
| 兼容 | 旧插件（`query_with_params` 不支持 `select version()` 的自研方言）→ 探测失败降级是否足够？ |
| 性能 | register 每次多一条单行查询——登记是低频管理操作，可接受？ |
| 产品 | 静态库（`db:` 段）要不要同样透出？——建议不做（无连接树语义）；评审定夺 |
