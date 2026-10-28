# U-11 连接级能力探测（server_version 快照）设计稿

> 状态：**设计稿（三路专家评审完成、处置落稿——处置表见 §7；放行，与 U-14 批 a 同车 v0.1.100）**。
> 上游：`docs/prds/oj-feature-requirements.md` §5 `U-11`。
> 相邻：U-7（v0.1.97，拨号守卫——探测挂在 connect 之后，**自带查询超时**，见 §2）；
> U-14（内省扩展——server_version 是其 golden 快照定位键，**本项的主力消费者**）。
> 评审：架构路 2 条 / 工程路 2 条 / 产品路 1 条，共 5 条；采纳 5、未采纳 0。

## 0. 范围

**做**：

| 项 | 内容 | 落点 |
|---|---|---|
| 探测 | connect 成功后跑**一条**版本查询：mysql/pg `select version()`、sqlite `select sqlite_version()`（统一走既有 `DataAccessor::query_with_params`，取首行首列字符串），**外包 2–3s 查询超时** | `src/bridge/client_db.rs`（register / reRegister 的 connect 成功分支 + `db.client.test`） |
| 缓存 | `ClientDbEntry` 增 `server_version: Option<String>` + `probed_at_ms: u64`（**非 Mutex**：entry 在 reRegister 时整体原子替换，探测在构造前一次完成、无构造后写入路径）；探活失败 = None + `warn`（**降级不阻断登记**） | `client_db.rs` |
| 透出 | `db.client.list` / `register` / **`db.client.test`**（产品路：用户点「测试连接」正是想看版本的时机，成本为零）返回值增 `serverVersion`（null = 未探到）与 `probedAtMs` | `client_db.rs::summary` + `global.d.ts` |
| 定位 | 叙事定位 = **U-14 golden 快照基建 + 排障观测增强**（产品路：主力消费者是测试基建与版本相关 SQL 前置判断，不是连接树徽标） | 文档 |

**不做**（显式划界）：

- **`describe()` 工厂级 caps 分派不变**（PRD 明文）：探测结果是**实例级观测值**，不参与能力
  判定（防止「探测到旧版本就静默降级」）。
- 版本轮换监测（长连连接的 server 升级感知）——重连（reRegister）即重探，够用。
- `show server_version`（pg 备选）不采用：`select version()` 三方言同形。
- **静态库（`db:` 段）不透出**（产品路裁决定案：无连接树语义）——「静态库无 serverVersion」
  升级为测试锚。
- auth 插件 / 其它轴的版本探测。

## 1. 现状（file:line，评审重锚）

- connect：`DbBackendRegistry::connect`（`src/bridge/db_backend.rs:63`）→ `Arc<dyn DataAccessor>`。
- register / reRegister：`src/bridge/client_db.rs:432` / `:536`（registry 方法 `:180`/`:214`），
  connect 成功后构造 `ClientDbEntry`。探测插在构造前。
- 拨号守卫：`guarded_connect`（`client_db.rs:398`）的 5s 超时**只包 connect**——探测查询在
  connect 成功之后、自身无超时（工程路指出的黑洞挂起点），见 §2。
- summary：`client_db.rs:55-64`。方言名：`accessor.dialect_info().name`。

## 2. 设计要点

- **宿主侧实现、零 ABI**：版本查询走既有 accessor 通路（含插件 typed 池）。
- **探测 SQL 按方言名分派**：三已知方言各一条；开放方言（其它名）**跳过探测 → null**（不猜
  SQL）——升级为测试锚。
- **查询自带超时（工程路阻断级修正）**：`tokio::time::timeout(3s, query)` 包住探测查询；
  超时与查询失败**同样走 warn + None 降级**（黑洞连接不得挂死 register op、不得占住池化
  runtime）。
- **降级语义**：探测失败 → warn + None，**登记照常成功**（探测是观测增强，不是登记门）。
- **`db.client.test` 同挂**：测试连接成功后同一条探测路径（复用同一 helper，register /
  reRegister / test 三处调用）。

## 3. 决策表

| 项 | 决策 |
|---|---|
| 探测时机 | register / reRegister connect 成功后、entry 构造前，一次；`db.client.test` 同挂 |
| SQL | mysql/pg `select version()`；sqlite `select sqlite_version()`；开放方言跳过 → null |
| 超时 | 探测查询外包 3s `timeout`；超时 = 失败 = warn + None（降级不阻断） |
| 字段 | `serverVersion: string \| null`、`probedAtMs: number`（构造期定死，非 Mutex） |
| ABI | 无（12 不变） |
| 配置 | 无新键 |

## 4. 测试锚（验收，评审增补后）

1. sqlite 动态库（**临时文件 DSN**，仓库惯例非 `:memory:`）：register 后
   `db.client.list()[0].serverVersion` 形如 `3.x.y`；`probedAtMs > 0`。
2. 探测失败降级：**新增一个 query 即报错的测试 accessor 夹具**（既有 `InMemoryAccessor`
   恒成功，不覆盖本路径）→ 登记成功、`serverVersion === null`。
3. 探测超时降级：假 accessor 挂起超过 3s → 同降级路径（超时与失败同归）。
4. reRegister 重探：换 DSN 后版本字段更新。
5. 开放方言 → `serverVersion === null`（命名断言）；静态库 `db.allowed()` 无 serverVersion
   字段（命名断言）。
6. mysql/pg 真库（env-gated）：版本串非空且与 `select version()` 一致（golden 锚，U-14 复用）。

## 5. 分期

单期交付（估 0.5 天含文档），**与 U-14 批 a 同车 v0.1.100**（golden 定位键依赖）。

## 6. 文档面

db-guide §11.6（字段 + 降级语义 + 「基建定位」叙事）、api-manual §6 指针、CHANGELOG；
devkit 四件对齐；`global.d.ts`。

## 7. 评审处置表（三路 5 条：采纳 5、未采纳 0）

| # | 来源 | 原意见要点 | 处置 |
|---|---|---|---|
| 1 | 产品 | 价值真实但主力消费者是 U-14 基建；挂 `db.client.test`；静态库/开放方言 null 升测试锚 | **采纳**（§0 定位 + §2 三处同挂 + 锚 5） |
| 2 | 工程 | 探测查询无超时——黑洞连接挂死 register op | **采纳**（§2：3s timeout，超时=失败=降级） |
| 3 | 工程 | `Mutex<Option<String>>` 多余（entry 原子替换、无构造后写入） | **采纳**（§0：改 `Option<String>`） |
| 4 | 工程 | 「fake accessor 报错」夹具不存在；测试用临时文件 DSN | **采纳**（锚 1/2） |
| 5 | 架构 | 行号漂移（register/reRegister 实为 :432/:536） | **采纳**（§1 重锚） |
