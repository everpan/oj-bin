# L3：动态数据源登记（client 轴运行时登记）

> 对齐 `../dbx` 多数据库能力三阶段之三。L1 = db 访问控制（v0.1.89）、L2 = 结构内省
> （v0.1.90）、L3 = 运行期动态数据源登记（本文件）。
>
> 目标：handler 在运行期把一个新的 DSN（mysql/postgres/sqlite）登记为命名数据源，
> 之后它**直接成为 `DB("<name>")` 的一个实例**，复用 L1 的访问控制、L2 的内省、
> 以及全部 query/exec/stream 家族——而不是另起一套平行的 `db.client.query` API。
>
> 本文为 v1 设计稿，待四路专家评审（工程师 / 架构师 / 产品 / 安全）裁决后定稿。

---

## 0. 范围与边界

- **做**：运行期 `register / test / delete / reRegister / list / has` 六 op + 配额 +
  逐别名记账；登记后的别名进入 `DB(name)` 解析路径，复用既有全部 op。
- **不做**：
  - 不在 `config.yaml` 里持久化（本阶段纯运行期，重启即失；持久化是 L4 待做）。
  - 不引入独立的 `db.client.query` 平行 API——故意复用 `DB(name)`。
  - 不改 ABI（纯宿主侧能力，插件契约不变）。
  - 不做跨进程共享 / 集群登记（单进程 Slot 足够）。

---

## 1. 核心难点：StableState 不可变红线

`StableState` 经 runtime 池共享后**不可变**（`#![allow]` 注释里明写「一经 runtime 池共享即
不可变」「失败的 runtime 会被丢弃，而非归还回池」）。直接把 `dbs: HashMap` 改成 `mut` 会撞红线。

**既有先例**：`reload: Arc<reload_bridge::ReloadSlot>`（`mod.rs:243`）——壳 `Arc` 构造期冻结、
内容（OnceLock）后填，不触 `Arc::get_mut`。`kafkas`/`rabbits` 用 `Arc<NamedRegistry<T>>`，
但 `NamedRegistry` 是 `&mut self` 注册，运行期用不了。

**本方案**：新增 `Arc<ClientDbRegistry>`，壳不可变、内部 `Mutex<HashMap>` 运行期变动。与
`reload: Arc<OnceLock>` 同哲学——共享同一 `Arc` 跨所有 Bridge（HTTP / 任务 / 热更复用），
不破坏不可变不变量。`NamedRegistry` 改为内部 `Mutex` 即可原地复用其 `register/get/contains/
names/default_alias` 语义。

### 1.1 需要注入 StableState 的 3 个新字段

```rust
// 运行期动态数据源注册表（L3）。壳 Arc 不可变，内部 Mutex 变动。
pub client_dbs: Arc<ClientDbRegistry>,
// 运行期连接工厂（与装配期同集：内置 sqlite/memory + 插件 mysql/postgres）。
pub db_backend: Arc<DbBackendRegistry>,
// sqlite 相对路径归一基准（connect 用）。
pub config_dir: Arc<PathBuf>,
```

`DbBackendRegistry::connect(&self, dsn, dir)` 是 `&self`（不变），`Arc` 化后运行期 op 直接调用。
`connect` 已脱敏（`crate::secret::redact(dsn)`），未知 scheme 走 fail-fast。

> 注：现有 `with_dbs_and_loader` / `Extras` 需同步加这三个字段（Extras 仅 `db_backend`/
> `config_dir` 用 `Option` 兜底默认；`client_dbs` 默认空 registry）。`oj/src/app.rs` 的
> `make_bridge_of` 闭包与 StableState 构造两处注入同源 `Arc`。

---

## 2. ClientDbRegistry

```rust
pub struct ClientDbEntry {
    pub accessor: Arc<dyn DataAccessor>,
    pub dsn_redacted: String,        // 脱敏 DSN（list 自省用，绝不带凭据）
    pub dialect: String,             // accessor.dialect_info().name
    pub registered_by: Option<String>, // operator_of(user)（无身份 None）
    pub registered_at_ms: u64,
    pub queries: AtomicU64,          // 逐别名记账（op 命中自增）
    pub last_error: Mutex<Option<String>>,
}

pub struct ClientDbRegistry {
    inner: Mutex<HashMap<String, ClientDbEntry>>,
    order: Mutex<Vec<String>>,
    max_entries: usize,              // 配额（默认 16）
}
```

方法：`register(name, entry)`（重名 / 超配额 → `Err`）、`get(name)`、`contains(name)`、
`remove(name)`、`names()`（注册序）、`len()`。

**配额行为**：`register` 前若 `len() >= max_entries` → `Err("db client registry full (cap N)")`。
`delete` 释放名额。

---

## 3. 授权模型（安全红线）

两个**独立**的授权面：

1. **登记操作授权门**（`db_client:` 配置段）：谁能调 `register/test/delete/reRegister`。
   - `enabled: bool`（**默认 false = fail-closed**，全 client op 返回 `db client disabled`）。
   - `allow: AllowList`（复用 L1 的 modules/roles/tenants/sources 四维并集；默认
     空 = 不限制，但仅当 enabled=true 时生效）。
   - 评估复用 `db_policy::granted(allow, ctx)`，但**不读 `db_access.enforce`**——
     它是「能否登记」而非「库级策略」。未授权 → 结构化拒绝（原因串 `db_client_not_authorized`）。

2. **登记后库级访问**：注册名进入 `DB(name)` 解析后，**自然落到 L1 `db_access` 机械**。
   - 别名不在 `db_access.by_db` → 取 `defaults`（默认 `enforce=off` 即透明，与今日静态库一致）。
   - 若运维对动态库想设防，可在 `db_access.<name>` 声明策略（与静态库同一张表，零新概念）。
   - `op_db_allowed` 也把 client dbs 纳入连接树（同样过 grant），避免「假入口」。

**为何登记表与访问表分离**：登记表是「运维授权谁能建连接」（特权面），访问表是「业务谁
能查这个库」（数据面）。二者语义不同，混在一起会逼运维为「能建连接的人」也配查询授权，
噪声大。`dbx` 的 client 轴同样是「连接注册」与「查询授权」两件事。

---

## 4. 新增 op 族（db.rs / 或新 client_db.rs）

| op | 签名 | 行为 |
|----|------|------|
| `op_db_client_register(name, dsn)` | `(state, name, dsn)` → `ClientDbEntry` 摘要 | 过登记门 → 校验别名（非空/≤128/无 NUL 控制字符/非 `default`/不与静态库或已登记名冲突）→ `db_backend.connect` → `register`。 |
| `op_db_client_test(dsn)` | `(state, dsn)` → `{ok, dialect, error?}` | 过登记门 → `connect` → `query("select 1")` 探活 → **不入库**。 |
| `op_db_client_delete(name)` | `(state, name)` → `bool` | 过登记门 → `remove`（不存在 → `Err`）。 |
| `op_db_client_re_register(name, dsn)` | `(state, name, dsn)` | 过登记门 → 要求已存在 → `connect` 新 dsn → 替换（原子：`remove`+`insert` 在持锁区间）。 |
| `op_db_client_list()` | `(state)` → `ClientDbEntry[]` | 过登记门（list 也受 `allow` 约束）→ 返回摘要（dsn_redacted）。 |
| `op_db_client_has(name)` | `(state, name)` → `bool` | 仅查 `client_dbs.contains`（**不经登记门**——`DB()` 构造器探活用，否则 bootstrap 拿不到 `undefined`）。 |

### 4.1 别名校验

```rust
fn valid_client_alias(name: &str) -> bool {
    !name.is_empty()
    && name.len() <= 128
    && !name.bytes().any(|b| b == 0 || b < 0x20)
    && name != "default"
}
```
冲突检测：`stable.dbs.contains_key(name)` 或 `client_dbs.contains(name)` → 拒。

### 4.2 与既有 op 的接缝

- **`op_db_has`**：先查 `stable.dbs`，再查 `client_dbs`（静态优先，避免影子）。`DB(name)` 构造器据此返回实例或 undefined。
- **`query::lookup`**：`bound_db` 后先 `dbs.get`，miss 再 `client_dbs.get`。
- **`resolve_target`**：同上（顺便支持 client 库的事务 `db.tx`——accessor 真实支持 begin 即可）。
- 于是 `DB("myreg").query/exec/stream/listTables/listColumns/listForeignKeys` 全部自动可用，
  **且自动吃 L1 访问控制 + L2 内省门 + SQL trace + 占位符改写**。这是「复用机械」而非「另起炉灶」的核心收益。

> 红线自检：表名/列名仍只来自 `SchemaRegistry` 或绑定参数；client 库的连接字符串由登记者提供，
> 但运行期 SQL 的标识符绝不来自 JS 字符串拼接——与静态库同一套守卫。
> `db_backend.connect` 对未知 scheme 已 fail-fast 且脱敏，不回落。

---

## 5. JS API（bootstrap.js）

```js
globalThis.db.client = {
  register:   (name, dsn) => dbOpWrap(op_db_client_register(name, String(dsn))),
  test:       (dsn)       => dbOpWrap(op_db_client_test(String(dsn))),
  delete:     (name)      => dbOpWrap(op_db_client_delete(name)),
  reRegister: (name, dsn) => dbOpWrap(op_db_client_re_register(name, String(dsn))),
  list:       ()          => dbOpWrap(op_db_client_list()),
  has:        (name)      => dbOpWrap(op_db_client_has(name)),
};
// DB(name) 已自动覆盖登记别名（op_db_has 改后生效），故 DB("myreg").query(...) 直接可用。
```

`global.d.ts`：补 `DBClientRegistry` 接口 + `db.client` 字段。

---

## 6. 配置与装配

`config.yaml` 新增段：

```yaml
db_client:
  enabled: true          # 默认 false（fail-closed）
  max_entries: 16        # 可选，默认 16
  allow:                 # 可选（复用 L1 AllowList 四维）；缺省不限制
    roles: [admin]
```

装配：`app.rs` 在 `from_config` 内把 `Arc::new(registries.dbs)`（已是含插件后端的完整 registry）
与 `Arc::new(config_dir.to_path_buf())` 注入 StableState / Extras。新增 `DbClientSection`
（`#[serde(default, deny_unknown_fields)]`，`enabled` 默认 false，`allow` 复用 `AllowList`）。
装配期校验：`max_entries == 0` → fail-loud。

---

## 7. 测试计划（gate 前必过）

1. **单元/行为**（沿用 `db_policy` 的 `bridge_with` 脚手架 + `file_sqlite()`）：
   - `register` 后 `DB(name).query` 能查到；`op_db_has` 命中。
   - `delete` 后 `op_db_has` 落空、`DB(name)` 返回 undefined（探活路径）。
   - 重名 register → 拒；超额 register → 拒。
   - 别名 `default` / 含控制字符 / 超长 → 拒。
   - 未知 scheme（`oracle://`）→ registry 脱敏报错，不回落。
   - `test` 不入库但能探活；坏 dsn → `{ok:false, error}`。
   - `reRegister` 替换连接；旧 accessor 不再被 lookup 命中。
   - **对抗性（钉死红线）**：`enabled:false` 时所有 client op 结构化拒绝；
     `allow.roles:[admin]` 下非 admin 登记被拒、`db_client_not_authorized` 可 grep；
     `asSystem()` **不豁免**登记门（与 L1 `asSystem 不豁免访问控制` 同口径）。
   - 记账：`queries` 在 query/exec 后自增；`list` 返回 dsn_redacted（断言不含 `:` 后的密码）。
   - 注册名落入 `db_access.<name>` 后 deny 档查询被拦（复用 L1 机械，行为面验证接缝正确）。
2. **gates**：`cargo fmt --check` / `cargo clippy --release --workspace --all-targets -- -D warnings` /
   `cargo test --release --workspace` / `cargo xtask build` + `smoke`。
3. **devkit 契约**：`cargo test --release -p xtask` 校验 devkit 产物与源一致（api-manual / scenarios /
   SKILL / README 同步）。

---

## 8. 文档与版本

- 版本 `0.1.90` → `0.1.91`（L3 落点）。
- `CHANGELOG.md` 新增 `## v0.1.91`：feat(db_client) / 配置 / 边界 / docs / 待做（L4 持久化）。
- `docs/db-guide.md`：新增 §「动态数据源登记」。
- `docs/devkit/api-manual.md`：db 表补 `db.client.*` 6 方法行；`OJ_DB_CLIENT_*` 错误码补入封闭集；
  已知限制 / 常见陷阱各补数行。
- `docs/devkit/scenarios.md`：新增场景（运行期接一个只读从库，注册→查询→画连接树）。
- `docs/devkit/SKILL.md`：补陷阱（enabled 默认关、register 名规则、授权两面性）。
- `sample/global.d.ts`：补类型。

---

## 9. 待评审裁决的开放问题

1. **登记门默认值**：`enabled` 默认 `false`（fail-closed，推荐）还是 `true`（兼容优先，需显式关）？
   倾向 false——动态接库是特权能力，默认不开。
2. **`list` 是否也走 `allow` 门**：是（推荐，避免未授权者枚举已登记连接）；但 `has` 不走（探活必需）。
3. **client 库是否进 `op_db_allowed` 连接树**：是（推荐，连接树应完整）——但只列授权可见者。
4. **配额默认值**：16 是否合适？生产可能要更大；先 16。
5. **`reRegister` 是否要「要求存在」**：是（语义 = 更新已登记；不存在用 register 即可），保持语义清晰。
6. **持久化**：本阶段不做（重启即失），L4 再议（含 config 段 + 启动时 replay）。

---

## 10. 四路评审裁决（v1 → v2 修订点）

四路（架构 / 工程 / 产品 / 安全）均判**可进实现**，并给出以下必须落实的修订：

- **[安全·阻断] 防止 SSRF / 内网横向移动**：`enabled=true` 且 `allow` 四维全缺（即
  `AllowList::is_unrestricted()`）→ **装配期 fail-loud 拒启动**（必须先显式声明授权维度，
  不能「开了忘配」就全开）。落点：`oj/src/app.rs` `from_config` 内校验 `db_client` 段。
- **[安全·阻断] 动态库默认 deny**：client 登记别名若未在 `db_access.<name>` 显式声明，
  其访问档**默认 `Enforce::Deny`**（而非回落 `defaults.enforce=off`）。落点：
  `db_policy::policy_for` 对「在 `client_dbs` 但不在 `db_access.by_db`」的别名强制 deny。
  语义：「动态连接默认关闭，需显式授权才开」。
- **[产品·门禁] 错误文案对齐 L1**：三码 `db_client_disabled` / `db_client_not_authorized` /
  `db_client_registry_full` 必须照搬 L1 的 `原因串 + 修复：` 风格并带 `reason:` 前缀
  （不回显授权名单）。
- **[工程·关键] 接缝补全**：`lookup`（`query.rs:2585`）与 `op_db_allowed`（`db.rs:908`）
  都需补 client_dbs 回落；`op_db_has` 补 client_dbs 查询（静态优先）。
- **[工程·关键] `DB()` 缓存失效**：bootstrap.js `dbCache` 按名缓存实例，`db.client.delete`
  必须 `dbCache.delete(name)` 驱逐，否则删后再 `DB(name)` 拿到陈旧实例。
- **[工程] 配置段落点**：`DbClientSection` 定义在 `src/bridge/db_policy.rs`（与 `DbAccessSection`/
  `AllowList` 同处），在 oj config 加字段；`max_entries==0` 装配期 fail-loud。
- **[安全·建议] 审计**：`register/delete/reRegister` 写结构化 eprintln 审计（operator/alias/
  dialect/source），不依赖 `deny_log`（其明确非审计）。
- **[安全·建议] 凭据**：`dsn_redacted` 用 `secret::redact` 已够；无 `@` 的 DSN（如
  `mysql://internal/`）仍会回显内网拓扑 → list 仅向授权者返回，已并入「list 走 allow 门」。
- **[架构·注意] 热更复用**：`from_config` 的 reuse_backend 路径沿用同一 StableState（含同一
  `Arc<ClientDbRegistry>`），不重建 → 运行期登记跨代保留；配置变更触发的重建（reuse_backend=None）
  会丢登记（文档注明：L4 持久化解决）。
- **§9 开放问题全部采纳原倾向**：①false ②list 走门/has 不走 ③进 allowed 树（仅授权可见）
  ④16 per-process 硬上限 ⑤reRegister 要求存在 ⑥持久化留 L4。
- 新增 `info(name)` 单库详情 API 不纳入本阶段（reRegister 已覆盖「改连接」语义，ergonomics 留 L4）。

---

## 11. 实现记录（v0.1.91，已落地）

- **状态**：已实现并通过 `fmt` / `clippy -D warnings` / `cargo test --release --workspace` /
  `cargo xtask build` / `cargo xtask smoke` 全部 gate；8 个 L3 行为测试（`src/bridge/db.rs`
  `bridge::db::tests::client_*`）覆盖：登记门关闭 / 全生命周期 / 重名拒绝 / 超配额拒绝 /
  未知 scheme 脱敏 / 授权维拒绝 + asSystem 不豁免 / **未声明动态库 fail-closed deny** /
  声明动态库查询放行 / reRegister / 命中计数累加。
- **关键实现决策修正（相对 §10 草案）**：`AllowList` 无「拒绝所有人」表达（全 `None` = 不限），
  故「未声明动态库默认 deny」不能靠动 `allow` 实现——改为在 `policy_for` 中对该类别名置
  `enforce = Deny`，并**把 `Enforce::Deny` 提升为门禁硬拦截**（见 `db_policy::check_access_intent`
  与 `granted_only`：Deny 档直接拒绝，不再看授权维/风险分类）。此举同时修正了 L1 既有
  `enforce: deny` 长期「只升级 Violation、不拦截合法请求」的隐患——`deny` 现在语义为「该别名全拒」。
- **落点文件**：`src/bridge/client_db.rs`（六 op + 注册表）、`src/bridge/db_policy.rs`
  （`DbClientSection` / `policy_for` deny 硬拦截）、`src/bridge/mod.rs`（StableState 5 字段 +
  `pub use client_db::{ClientDbEntry,ClientDbRegistry}`）、`src/bridge/db.rs`（`op_db_has` /
  `op_db_allowed` 补 client 回落）、`src/bridge/query.rs`（`lookup` 补 client 回落 +
  命中计数）、`src/bridge/bootstrap.js`（`db.client` 管理面 + delete 驱逐 `dbCache`）、
  `src/config.rs`（`db_client` 段）、`oj/src/app.rs`（装配 + fail-loud + Extras 注入）。
- **文档**：`CHANGELOG.md`（v0.1.91）、`docs/db-guide.md`（§11.6）、`sample/global.d.ts`
  （`DBClientApi` / `ClientDbEntry`）、`docs/devkit/{api-manual,scenarios,SKILL}.md`。
