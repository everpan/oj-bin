# dbx Web 对齐 · L7：U-4 跨请求事务（`db.session` 会话）

> 状态：**设计稿（四路评审已过，修订完成；未实现）**。
> 上游：`docs/prds/oj-feature-requirements.md` §2 P0-b `U-4`。
> 相邻：`docs/plans/2026-10-28-dbx-web-l6-u6-dialect-decode.md`（L6 已落地）；
> `docs/plans/2026-10-28-db-client-axis-l3.md` §10（L3 评审裁决，本文件沿用同一套授权/不可变纪律）。
> 评审处置见 §7（21 条意见：采纳 18、部分采纳 3、未采纳 0）。

## 0. 范围

**做**（U-4 全量）：

| 项 | 内容 | 落点 |
|---|---|---|
| 会话表 | 宿主 tx 会话注册表（`Arc` 壳 + 内部 `Mutex`），id = `tx_<boot4>_<32hex>` | `src/bridge/db_session.rs`（新） |
| op 家族 | `begin` / `attach` / `detach` / `query` / `exec` / `commit` / `rollback` / `close` / `touch` / `closeAll` / `list` / `info` / `stats` | `db_session.rs` + `mod.rs::bridge_ext` |
| JS 面 | `db.session.*`（挂在 `DB(name)` 实例） | `src/bridge/bootstrap.js` |
| 绑定 | `attach(id)` 把会话绑到本请求 → `db.query/exec/table()`（含 builder）**自动走同一连接** | `ReqState.session` + `db::resolve_target` |
| 守卫 | 每次调用与裸 SQL 路径同一条链，且**授权每语句重判**；会话内**禁止事务控制语句** | 复用 `guard`/`db_policy`/`normalize_and_trace`/`trace_sql` |
| 回收 | 空闲超时（滑动）+ 最长寿命（绝对）+ 在飞超时强制摘表；后台 reaper + 前台懒扫 | `db_session.rs` |
| 归属 | 绑定 `(tenant, user)`；他人 id 与未知 id **同一条**错误（防枚举） | `db_session.rs` |
| 配置 | `db_session:` 段（默认关；`allow` / `max_sessions` / `max_per_owner` / `idle_timeout_secs` / `max_lifetime_secs` / `require_identity`），装配期 fail-loud | `src/config.rs`、`oj/src/app.rs` |
| 观测 | `list`/`info`/`stats`（仅本调用者）+ 统一 `emit()` 事件面 + `DB_SESSION_*` 日志（不带完整 id、不带 SQL/DSN） | `db_session.rs` |

**不做**（显式划界，写进文档）：

- **U-5 execution-id 取消**（L8 单独立项）：真正的「非流式查询服务端 KILL」需要插件侧能力面（`pg_cancel_backend` / `KILL QUERY` 需连接 pid/cid），与事务会话无耦合；本设计只保证「在飞超时后会话立即失效、后续 op 快速失败」，**不**声称能中断已在后端执行的语句。
- 隔离级别 / 只读事务 / savepoint / `SET TRANSACTION`：需扩 `TxSession` 契约（ABI 面）。
- **会话持久化**：进程重启即失效（回落 tombstone 只服务于「刚超时/刚丢弃」的诊断）。
- 跨库会话（一 id 一事务一别名）、嵌套会话。
- **ABI 变更：无**（`ABI_VERSION` 仍 12；`TxSession` 是宿主侧 trait，`Box<dyn TxSession>` 直接进宿主表）。

## 1. 现状（file:line）

- 事务只活在请求里：`ReqState.tx`（`src/bridge/mod.rs:365-366`），`op_db_tx_begin` 写入
  （`db.rs:417-437`），`take_tx` 收尾（`db.rs:402-415`）。请求结束 drop 即回滚
  （`FfiTxSession::drop`，`ffi.rs:705`；`SqlxTx` 内 `Transaction` 的 Drop）。
- `resolve_target`（`db.rs:314-329`）只认「本请求活跃事务」——builder 的 `table()` 也走它
  （`bootstrap.js:1066,1072`），所以会话若要支持 builder，必须在**这一层**插入路由。
- JS 面唯一入口 `db.tx(fn)`（`bootstrap.js:1062-1100`），闭包式。
- **StableState 跨热更复用**：`from_config` 复用 Backend 时 `let stable = backend.stable().clone();`
  （`oj/src/app.rs:1540`），`assemble_backend` 只在首代跑（`:1530-1539`）。⇒ 放这里的注册表是
  **进程级**，热更不重建（与 `client_dbs` / `db_access_rt` 同命）。**设计初稿「热更即回滚」是错的，
  已订正**（见 §7 #A1）。
- 配置已知段 `known_top_level_keys`（`config.rs:1463-1464` 收尾 `db_access`/`db_client`）；
  `diff_configs` 把它们归 `restart_pending`（`serve_cmd.rs:594-599`）。mapping 探测名单在
  `config.rs:1749`。装配期校验样式：`oj/src/app.rs:1266-1281`。
- 可复用基建：`db_policy::{AllowList, granted, granted_only, check_access, operator_of}`、
  `guard::{bound_db, check_raw, check_tenant_raw}`、`db::{normalize_and_trace, trace_sql}`、
  `jsnum::sanitize_rows`、`client_db::ClientDbRegistry::epoch`（v0.1.93）。

## 2. 设计

### 2.1 API 面

| JS（`DB(alias).session.*`） | op | 返回 |
|---|---|---|
| `begin()` | `op_db_session_begin(name)` | `{ id, alias, dialect, idleTimeoutMs, maxLifetimeMs, expiresInMs }` |
| `attach(id)` / `detach()` | `op_db_session_attach` / `op_db_session_detach` | `true` |
| `query(id, sql, params?)` | `op_db_session_query` | `Row[]`（`max_rows` 截断 + `sanitize_rows`） |
| `exec(id, sql, params?)` | `op_db_session_exec` | `number` |
| `commit(id)` / `rollback(id)` | `op_db_session_commit` / `_rollback` | `true` |
| `close(id)` | `op_db_session_close` | `true`（best-effort 回滚 + 摘表；**幂等**） |
| `touch(id)` | `op_db_session_touch` | `{ expiresInMs }`（只续空闲计时，不动最长寿命） |
| `closeAll()` | `op_db_session_close_all` | `number`（**仅本主体**） |
| `list()` | `op_db_session_list` | `SessionSummary[]`（本主体，跨库） |
| `info(id)` | `op_db_session_info` | `SessionSummary` |
| `stats()` | `op_db_session_stats` | `{ active, byOwner, maxSessions, maxPerOwner, timedOut, discarded, rejected }` |

`SessionSummary = { id, alias, dialect, state, ageMs, idleMs, expiresInMs, ops, createdBy }`。
`state ∈ open|expired|discarded`（后两者只见于 tombstone 诊断）。

`id` 只在**本主体**的返回值里出现；日志/错误一律只带**前缀** `tx_ab12_…`，因为 id 等价 bearer token。

PRD 命名映射（写进 api-manual，避免「找不到 tx_begin」）：

| PRD | 本设计 |
|---|---|
| `tx_begin` | `db.session.begin()` |
| `tx_exec` / 查询 | `db.session.exec/query(id, …)`（或 `attach(id)` 后直接用 `db.exec/db.query`） |
| `tx_commit` / `tx_rollback` / `tx_close` | `db.session.commit/rollback/close(id)` |

**为什么额外有 `attach`**：`db.table()`（builder）与 `db.query/exec` 走的是 `resolve_target`
（`db.rs:314`）。若会话只提供 `exec(id, …)`，同一请求里 `db.table().insert()` 会**静默落到池上**——
写不参与事务，还与未提交行锁自锁（评审 A2 阻断项）。`attach` 把 id 写进 `ReqState.session`，
`resolve_target` 命中即返回 `Target::Session`，builder/裸 SQL 一并绑同一连接。

### 2.2 宿主注册表

```rust
// src/bridge/db_session.rs
pub struct TxSessionRegistry {
    inner: std::sync::Mutex<HashMap<String, Arc<TxSessionEntry>>>, // 短临界区，绝不跨 await
    tombstones: std::sync::Mutex<VecDeque<(String, Tombstone)>>,   // 上限 256，供诊断
    boot: String,          // 4 hex，进程内标识（日志可辨「上个进程的会话」）
    max_sessions: usize,
    max_per_owner: usize,
    idle_timeout: Duration,
    max_lifetime: Duration,
    require_identity: bool,
    started: AtomicBool,   // reaper 懒启动（幂等）
    timed_out: AtomicU64, discarded: AtomicU64, rejected: AtomicU64,
}

pub struct TxSessionEntry {
    id: String,                        // tx_<boot4>_<32hex>
    alias: String,                     // bound_db 之后的物理别名（**唯一权威**）
    owner: Owner,                      // tenant / user（判定）+ module / source（审计）
    epoch: u64,                        // 动态别名世代号（静态 = 0）
    accessor: Arc<dyn DataAccessor>,   // 保活：Drop 序不再依赖 StableState 字段顺序（评审 E1）
    dialect: String,
    dialect_info: DialectInfo,
    session: tokio::sync::Mutex<Box<dyn TxSession>>,
    created: tokio::time::Instant,
    last_used: std::sync::Mutex<tokio::time::Instant>, // reaper/摘要用，短临界区
    in_flight_until: std::sync::Mutex<Option<tokio::time::Instant>>, // 在飞 op 的硬截止
    ops: AtomicU64,
    dead: AtomicBool,                  // 被判死（超时/别名失效）：后续 op 立即失败
}
```

要点：

- **不可变红线**：`StableState.tx_sessions: Arc<TxSessionRegistry>`，壳构造期冻结、内部 `Mutex` 变动。
- **`tokio::time::Instant`**（不是 `std::time`）：可被 `tokio::time::pause/advance` 驱动，
  超时用例才可确定性断言（评审 A4）。
- **锁纪律**：`std::sync::Mutex` 只在同步段内用，且 `unwrap_or_else(|e| e.into_inner())`
  抗毒化（评审 E7）；跨 await 一律在 `Arc` 拷贝之后。
- **id**：`getrandom` 16 字节 hex；失败 → 返回 Err（**不 panic**，评审 S6）。
  **入口先校验 `^tx_[0-9a-f]{4}_[0-9a-f]{32}$`**，非法 id 直接按 not-found 处理，
  绝不对用户串做字节切片（多字节/换行既会 panic 也能伪造日志）。
- **容量**：
  - 全局 `max_sessions` 与**每主体** `max_per_owner` 双上限（默认 32 / 8）：单主体不能占满全池饿死别人（评审 S4）。
  - **先原子占位再建连接**：临界区内检查配额并插入占位条目，`begin()` 失败则回滚占位；
    否则并发 begin 会超发连接（评审 S4）。
  - 满额**拒绝**，绝不 LRU 驱逐（那等于替别人回滚在写事务）。
- **`dead` 标志**：被判死的会话不立刻 drop（可能正被别的 op 持有 `Arc`），而是置位 + 摘表；
  持有者下一次调用即失败，`FfiTxSession`/`Transaction` 最终 Drop 回滚。

### 2.3 生命周期与回收

1. `begin`：配额占位 → `enabled`/`allow`/身份/别名存在/epoch 检查 → `begin()` → 填充条目 → 懒启动 reaper。
2. 每次 op：先判 `dead` → 再懒扫 `idle > idle_timeout` 或 `age > max_lifetime` → 就地判死 + 报错；
   未超时则刷新 `last_used`/`ops`，并登记 `in_flight_until = now + 剩余寿命`（评审 S5：挂死查询也要有硬截止）。
3. op 执行完：清 `in_flight_until`。
4. reaper（`tokio::spawn`，`interval(max(1s, idle/4))`，`Weak` 每轮升级后即放）：
   - 到 `max_lifetime` 或 `idle` 超时 → `try_lock` 会话：拿到则 `tokio::time::timeout(5s, rollback)`
     （`await_ffi` 在 DB 不可达时是 yield 自旋，必须带 timeout，评审 E3）→ 摘表 → tombstone
     → `emit(timeout)`；
   - 拿不到锁且 `in_flight_until` 已过 → **强制判死 + 摘表**（不 await，不阻塞运行时），
     `emit(in_flight_timeout)`；后端语句何时返回由驱动决定（文档明示，U-5 才做服务端 KILL）。
5. `commit`/`rollback`：校验归属 → 锁 → 执行 → **无论成败都摘表**；`commit` 失败的错误文案
   明示「结果不确定，重试前请核对」（评审 S9）。
6. `close`：幂等兜底（`finally` 用），内部 rollback 失败**吞掉**并记 `emit(close_error)`，不掩盖业务异常。
7. 别名失效：每次 op 比对 `client_dbs.epoch(alias)`；变更 → 判死 + 摘表 + 明确报错。
   另在 `op_db_client_delete` / `re_register` 主动 `discard_alias(alias)`（评审 E8：delete 后
   accessor 句柄将关，留着只会报错更晚更模糊）。
8. 进程退出 / `Backend` drop：条目 drop → 事务 Drop 回滚（既有语义）。

### 2.4 每次调用的守卫链（与裸 SQL 路径逐项对齐）

```
[入口] 校验 id 形状 → 查表（归属不符/未命中 → 同一条 not-found）→ dead/超时/epoch 判定
  → 每 op 重跑 db_session.allow 门（角色被撤销立即生效，评审 S7）
  → 事务控制语句黑名单（见下）
  → check_raw → check_tenant_raw
  → check_access(entry.alias, sql)          // **用 entry.alias，绝不重算 bound_db**（评审 S2）
  → normalize_and_trace(entry.dialect_info.caps.placeholder, in_tx=true)
  → entry.session.lock().await.{query,exec}
  → trace_sql(in_tx=true, source=entry.owner.source, …)   // 来源用建会话时的快照，不取当前请求（评审 E2）
  → query: max_rows 截断 + X-OJ-Row-Limit + sanitize_rows
```

三条硬规矩：

1. **授权每语句重判**：`begin` 过门不等于后续语句过门。`read_only` 库的会话照样写不进去，
   否则「先 begin 再写」就是提权绕过。
2. **别名不重算**：`bound_db` 依赖当前请求的模块绑定（`guard.rs:500-527`），跨请求重算会导致
   「用 A 库策略判权、在 B 库执行」（评审 S2）。op 只信 `entry.alias`；`attach` 路径上若
   `bound_db(name) != entry.alias` 则直接报错（不静默换库）。
3. **会话内禁止事务控制语句**（不受 `enforce` 档位影响，评审 S1）：
   `classify(sql).risk == Risk::Transaction`（覆盖 `BEGIN/COMMIT/ROLLBACK/SAVEPOINT/RELEASE/START`，
   `db_policy.rs:584`）**外加**首关键字 `SET` / `USE` / `ATTACH` / `DETACH` / `PRAGMA` 一律拒绝。
   理由：MySQL 上 `BEGIN`/`SET autocommit=1` 触发隐式提交 —— 先写后 `BEGIN` 再 rollback
   会得到「假回滚」，数据已落库。错误文案点名该语句类别并给替代（用 `db.session.commit/rollback`）。

### 2.5 身份与归属

- `Owner { tenant, user, module, source }`：tenant/user 与 `db_policy::identity` 同源
  （`rs.req.tenant_id` / `req.user.id`）；module/source 仅审计，**不参与判定**（跨请求流程常跨模块）。
  侧记：`check_access` 仍会用**当前**请求的 module 判权——若 `allow` 按 module 收窄，跨模块调用会被
  拒（fail-closed，明确报错，加测试钉死；不为此放宽）。
- **`require_identity` 默认 true**：`user.id` 与 tenant 至少有一个，否则 `begin` 直接拒绝
  （`cross-request transactions require an authenticated caller (user.id or tenant); set db_session.require_identity: false for local dev only`）。
  `oj test` / 本地开发显式关掉（文档给样例），生产保持默认。
- 判定：`tenant == other.tenant && user == other.user`。
- **不泄露存在性**：归属不符、id 不存在、id 形状非法 → 同一条
  `tx session 'tx_ab12_…' not found (unknown, expired, or owned by another caller)`。
- **tombstone**（评审 P2）：本主体会话超时/丢弃后 60s 内 `info(id)` 返回
  `state: expired|discarded` 与原因（`idle>300s` / `max_lifetime` / `alias changed` / `reload`），
  让他人仍只看到 not-found。上限 256 条，插入即裁旧。

### 2.6 并发语义：串行排队（写死，可测）

同一 id 的并发调用在 `entry.session.lock()` 上**排队**：后到者挂起，前一条完成后执行。
理由：一条连接上的协议本就串行；拒绝会让「UI 两个面板同时查」随机失败。
副作用写进文档：长查询会阻塞同会话后续调用；要并行请用不同会话。
reaper 对忙会话按 `in_flight_until` 处理（§2.3-4），不会把在跑的查询误判为空闲。

### 2.7 配置段与装配期校验

```yaml
db_session:
  enabled: false            # fail-closed，默认关（零行为变化）
  allow:                    # AllowList 四维并集，与 db_client 同形（部署门，非业务授权）
    roles: ["db-admin"]
  max_sessions: 32          # per-process
  max_per_owner: 8          # 单主体上限
  idle_timeout_secs: 300    # 滑动：每次成功 op / touch 续期
  max_lifetime_secs: 1800   # 绝对：从 begin 起算，不可续
  require_identity: true    # 无 user.id 且无 tenant → 拒建（本地开发显式置 false）
```

装配期（`oj/src/app.rs`，紧邻 `db_client` 检查；`config.rs:1749` 的 mapping 探测名单加键）：

| 条件 | 处置 |
|---|---|
| `enabled=true` 且 `allow.is_unrestricted()` | 拒启动 |
| `enabled=true` 且 `max_sessions == 0` | 拒启动 |
| `max_per_owner == 0` 或 `> max_sessions` | 拒启动 |
| `idle_timeout_secs == 0` | 拒启动（「不超时」非法） |
| `idle_timeout_secs > max_lifetime_secs` | 拒启动（自相矛盾，不 clamp） |
| `enabled=false` 但其它字段非默认 | 放行 + 启动日志提示 |
| `max_sessions` > 每别名连接池上限 | **启动 warn**（池上限跨 `db:` 段推导，硬校验放 warn 避免误伤动态库） |

`db_session` 进 `known_top_level_keys` → 热更归 `restart_pending`（与 `db_access`/`db_client` 同档：
StableState 不可变，改了必须重启才生效）。

### 2.8 观测与事件面

- **单一 `emit(event)`**（评审 P-2 / S8）：`SessionEvent { kind, tenant, user, alias, id_prefix, ops,
  dur_ms, reason }` → 现在落 stderr（`DB_SESSION_BEGIN|TIMEOUT|DISCARD|MAX|CLOSE_ERR`），
  **不带 SQL、不带 DSN、不带完整 id**；后续 U-9 事件外发只需替换该函数的出口（不返工）。
- `list`/`info`/`stats` 为 `#[op2] #[serde]` 同步 op（返回结构体，不能用 `fast`，评审 E7）。
  `stats().byOwner` 只统计**本主体**条目数（跨主体只给聚合数字，不给身份）。
- `expiresInMs` 用**相对毫秒**，避开前后端时钟偏差，UI 可直接倒计时（评审 P-1）。

### 2.9 与既有能力的交互

| 既有能力 | 交互 |
|---|---|
| **热更 / reload** | **会话跨热更存活**（`StableState` 经 `Backend` 复用，`app.rs:1540`）。`db_session` 段改动归 `restart_pending`，不强行热改 |
| `db.tx(fn)`（请求作用域） | 并存；同一请求已有活跃 `db.tx` 时 `begin`/`attach` 报 `per-request transaction active on db 'x'; finish db.tx before attaching a session`（防两条写路径互等锁） |
| `db.table()` builder | `attach` 后自动绑会话（`resolve_target` → `Target::Session`）；未 attach 时行为不变 |
| `db.client.delete/reRegister` | 主动 `discard_alias`（§2.3-7）+ epoch 兜底 |
| `db_access` / `db_access_rt`（U-1） | 每语句 `check_access`，运行时 grant/revoke 立即生效 |
| `db.stream` | 会话/attach 内拒绝（与 `db.tx` 现状一致，明确报错） |
| `db.asSystem()` / `asTenant()` | 不改变会话路径语义：`check_access` 本就不读 `ReqState.system`，`attach` 也不放宽授权 |
| `oj_info` | 不加新字段（快照在装配期固化）；观测走 `stats()` |

## 3. 备选方案与否决

| 方案 | 否决理由 |
|---|---|
| 复用 `db.tx(fn)` | 不满足「跨请求」；闭包锁死在请求生命周期 |
| 会话句柄交给 JS 持有 | FFI 句柄生命周期必须宿主管；JS GC/isolate 回收不可控（U-3 同类教训） |
| 会话存 isolate 缓存 | 跨 isolate 不可见——U-3 修的正是这个坑 |
| `std::sync::Mutex` 持会话跨 await | 卡死 current_thread 运行时 |
| 满容量 LRU 驱逐最旧会话 | 破坏性：替别人回滚在写事务 |
| 自增 / 时间戳 id | 可枚举 = 跨租户打靶面；128-bit 随机 + 归属校验双保险 |
| **热更时拒绝 reload**（评审 A1 建议之一） | StableState 经 Backend 复用，会话**本来就不随热更销毁**（`app.rs:1540`），无需拒绝；拒绝反而制造无谓的运维阻塞 |
| **现在就抽泛型 `handle_registry<T>`**（A3） | 两边 entry 载荷差异大（事务有 `Box<dyn TxSession>` + 方言快照，取消只有 cancel fn），泛型化收益不抵复杂度；改为抽出三件共用件 `handle_id()` / `Owner` / `expiry_pair()` 放 `src/bridge/handle_core.rs`，U-5 直接复用 |
| **`db_access.<alias>.allow_sessions` 二级门**（A6） | 与 `db_session.allow` + 每语句 `check_access` 重复；新增维度会让「为什么我被拒」变三处排查。先不引入，文档写清两道门的关系 |
| 会话内允许 DDL | 保留（受 `allow_ddl` 管），但文档/SKILL 明示 MySQL 隐式提交会吞掉事务；事务控制语句另按 §2.4-3 禁止 |

## 4. 红线核对

- [x] `StableState` 冻结：壳 `Arc` + 内部 `Mutex`（不 `Arc::get_mut`）。
- [x] 不跨 `await` 持 `OpState`/`RefCell` 借用；不跨 `await` 持 `std::sync::Mutex`。
- [x] 动态标识符仍只来自 `SchemaRegistry`；会话只跑裸 SQL + 既有守卫。
- [x] 值只走绑定参数。
- [x] 失败必响：禁用/未授权/无身份/超时/容量/epoch 变更/非法 id/事务控制语句 → 结构化报错。
- [x] `bootstrap.js` 保持 7-bit ASCII。
- [x] 无 ABI 变更（`ABI_VERSION` 仍 12）。

## 5. 测试计划

**离线（sqlite 临时文件库——`:memory:` 每连接独立库，池内第二条连接看不到，用会假红，评审 E5）**

1. `disabled_by_default`：未开段 → `begin` 报错含 `db_session.enabled`。
2. `begin_exec_query_commit_roundtrip`：建表 → begin → exec → query 回读 → commit → 新连接可见。
3. `attach_binds_builder_to_session`：`attach` → `db.table(t).insert(...)` → rollback → 行不存在
   （证明 builder 没落到池上）。
4. `rollback_discards_writes`；`close_is_idempotent`（连调两次不抛）。
5. `unknown_or_foreign_id_is_indistinguishable`：他人条目与未知 id → **同一条**文案；非法 id
   （多字节、换行、超长）不 panic 且不伪造 `DB_SESSION_*` 日志。
6. `capacity_rejects_without_evicting`：`max_sessions=1` → 第二 begin 报错，第一仍可 exec；
   `max_per_owner` 单独钉死（同主体第 N+1 个被拒、他主体仍可建）。
7. `concurrent_begin_respects_capacity`：并发 16 个 begin → 成功数 == cap，无超发。
8. `idle_timeout_rolls_back`：`idle_timeout_secs=1` + `tokio::time::advance` → 再 exec 报 expired、
   数据未落库、`stats().timedOut == 1`、`info(id)` 返回 `state: expired`。
9. `touch_extends_idle_not_lifetime`：`touch` 后续空闲有效；寿命到点仍失效。
10. `in_flight_over_lifetime_is_killed`：`select` 挂住（sqlite 上用长递归 CTE 或 mock 慢 accessor）
    → 超过寿命后 `dead`、后续 op 快速失败、reaper 不被阻塞。
11. `tx_control_statements_rejected`：会话内 `exec("BEGIN")` / `exec("SET autocommit=1")` /
    `exec("savepoint s")` → 全拒；先写后 `BEGIN` 再 rollback 的场景数据不落库（安全回归）。
12. `epoch_change_discards_session`：`reRegister` / `delete` → 旧 id 报 alias 变更、条目已摘。
13. `per_request_tx_blocks_session`：`db.tx(fn => db.session.begin())` → 冲突报错。
14. `concurrent_execs_are_serialized`：`Promise.all` 两个 exec → 都成功、`ops == 2`。
15. `list_only_own_sessions` + `close_all_scoped`：两个 owner 各一个 → 各 `list()` 长度 1；
    `closeAll()` 只关自己。
16. `session_query_respects_max_rows`：`max_rows=2` + 5 行 → 截断 + `X-OJ-Row-Limit`。
17. `read_only_policy_denies_write_in_session`：`read_only=true` 库 begin 后 `exec` 写 → 拒。
18. `tenant_guard_runs_in_session`：租户违规 SQL 同样被拒。
19. `alias_not_rebound`：跨模块上下文下 attach 到别库别名 → 报错（不静默换库）。
20. `stats_shape`：`stats()` 字段齐全、`byOwner` 不泄露他主体身份。
21. `mutex_poison_resilience`：人为毒化内部 Mutex → 后续 op 仍可用（`into_inner`）。

**真库（env-gated，缺失即 skip-as-pass，CI 挂账不变）**

22. mysql / postgres：begin → 两个独立 op 调用（模拟跨请求）→ commit → 新连接可见；
    同一路径 rollback 数据不落库；typed 池分支 + 插件 FFI 会话均覆盖。

**变异（对标既有 probing 纪律）**

23. 归属判定改恒真 → 5 红；容量改静默驱逐 → 6 红；删 epoch 比对 → 12 红；
    去掉事务控制黑名单 → 11 红。

## 6. 文档与版本

- `docs/devkit/db-guide.md`：新增「§12 跨请求事务（`db.session`）」——最小可复制配置、
  全流程样例（含 `attach` + builder）、`touch` 心跳、超时/容量/归属语义、与 `db.tx` 的差别、
  事务控制语句禁令、DDL 隐式提交陷阱、连接占用（`max_sessions` 与池上限）。
- `docs/devkit/api-manual.md` §6：`db.session.*` 全量表 + PRD 命名映射 + 6 类错误文案 + 限制表新增行。
- `docs/devkit/SKILL.md`：陷阱速查（不 commit 必超时回滚 / id 是 bearer / 会话内禁 `BEGIN`、`SET`）。
- `docs/devkit/scenarios.md`：新场景「多步表单向导：跨请求事务 + 心跳」。
- `docs/devkit/README.md`：v0.1.95 段落 + 索引行。
- `docs/api/06-db.md`：`db.session` 摘要 + 指回 devkit。
- `docs/plugins/plugin-development.md` §15.6：插件作者契约变更——事务句柄可能被宿主持有至
  `max_lifetime`（不再等于请求生命周期），`tx_rollback` 必须幂等且不得依赖调用方仍在同一请求。
- `CHANGELOG.md`：`## v0.1.95（未打标签）`（feat(db) / docs / 测试与挂账）。
- 版本：`oj 0.1.95`（`oj/Cargo.toml` + `Cargo.lock`）；**插件无改动 → 不 bump**。

## 7. 四路评审处置

| # | 视角 | 意见 | 处置 |
|---|---|---|---|
| E1 | 工程师 | `StableState` 字段 drop 序会让「保底回滚」落空 | **采纳**：entry 持 `Arc<dyn DataAccessor>` 保活（§2.2） |
| E2 | 工程师 | `err_meta` 要在 `to_js_err` 之前取；失败路径也要 `trace_sql`；`source` 会用错 | **采纳**：§2.4 守卫链写死顺序；`trace_sql.source` 用建会话时快照 |
| E3 | 工程师 | reaper 跨 await 持 `std::sync::MutexGuard` 编不过；`await_ffi` 自旋会饿死 current_thread | **采纳**：先摘 `Arc` 再 await + `timeout(5s, rollback)`（§2.3-4） |
| E4 | 工程师 | `try_lock` 跳过会导致挂死查询永不回收 | **采纳**：`in_flight_until` 硬截止 + 强制判死（§2.3-4） |
| E5 | 工程师 | sqlite `:memory:` 每连接独立库，用例会假红 | **采纳**：测试用临时文件库（§5 抬头） |
| E6 | 工程师 | `list` 必须是同步 `#[op2] #[serde]`，且要抗 Mutex 毒化 | **采纳**：§2.8、§5-21 |
| E7 | 工程师 | 跨模块会话会被 `check_access` 的 module 维拒 | **采纳为「明确报错」**，加测试 §5-19；不为此放宽策略 |
| E8 | 工程师 | `db.client.delete` 后必须强制摘表 | **采纳**：`discard_alias`（§2.3-7） |
| A1 | 架构师 | 热更会替用户回滚在写事务（判为阻断） | **不成立/已纠正**：`StableState` 经 `Backend` 复用（`app.rs:1540`），会话跨热更存活；设计初稿该段已订正（§1、§2.9） |
| A2 | 架构师 | builder 静默走池 = 写不入事务（阻断） | **采纳**：`attach` + `resolve_target::Session`（§2.1、§2.9） |
| A3 | 架构师 | U-5 会复用 owner/容量/超时，建议现在抽泛型表 | **部分采纳**：抽 `handle_core` 三件共用件，不建泛型表（§3 理由） |
| A4 | 架构师 | `std::time::Instant` 与 `tokio::time::pause` 冲突 | **采纳**：全用 `tokio::time::Instant`（§2.2） |
| A5 | 架构师 | reaper 绑在 actor runtime，actor 退休即失守 | **采纳**：懒扫兜底 + 文档明示（§2.3），并加 §5-8/10 |
| A6 | 架构师 | 建议 `db_access.<alias>.allow_sessions` 二级门 | **不采纳**：与两道现有门重复（§3 理由） |
| A7 | 架构师 | 超时计数应可查，别以「ojInfo 冻结」为由放弃 | **采纳**：新增 `db.session.stats()`（§2.8） |
| A8 | 架构师 | 文档漏 `plugin-development.md`（事务生命周期契约变了） | **采纳**：§6 增项 |
| P1 | 产品 | 60s 空闲会打断长表单，缺续期 | **采纳**：`touch` + 心跳样例 + `expiresInMs`；默认 idle→300s、lifetime→1800s、max→32（§2.7） |
| P2 | 产品 | 「会话丢失」不可观测 | **采纳**：tombstone + `state` 字段 + `DB_SESSION_DISCARD` 原因（§2.5、§2.8） |
| P3 | 产品 | 事件外发无挂点，U-9 会返工 | **采纳**：单一 `emit()` 出口（§2.8） |
| P4 | 产品 | 开启路径要最少一步 | **采纳**：db-guide 给最小配置片段；错误文案带配置键（§2.7、§6） |
| S1 | 安全 | 会话内可执行 `BEGIN`/`SET autocommit` 造成假回滚（阻断） | **采纳**：事务控制语句黑名单（§2.4-3）+ 回归 §5-11 |
| S2 | 安全 | 每次重算 `bound_db` 可能「A 库判权、B 库执行」（阻断） | **采纳**：只信 `entry.alias`（§2.4-2） |
| S3 | 安全 | 匿名主体折叠成同一 owner | **采纳**：`require_identity` 默认 true（§2.5） |
| S4 | 安全 | 无 per-owner 配额 + 容量检查与插入不同临界区 | **采纳**：双上限 + 先占位再建连（§2.2） |
| S5 | 安全 | 持锁挂死可让会话/连接永久泄漏 | **采纳**：`in_flight_until` 强制判死；**不**声称能中断后端语句（U-5） |
| S6 | 安全 | id 未校验就按字节截取 → panic / 日志伪造 | **采纳**：形状白名单 + `getrandom` 失败返 Err（§2.2） |
| S7 | 安全 | 角色撤销后仍可续用 | **采纳**：每 op 重跑 `db_session.allow`（§2.4） |
| S8 | 安全 | 审计缺 operator/tenant | **采纳**：`emit()` 带 tenant/user/alias/id 前缀（§2.8） |
| S9 | 安全 | `close` 吞错、`commit` 失败结果不确定 | **采纳**：`commit` 失败文案明示不确定；`close` 吞错但记 `CLOSE_ERR`（§2.3-5/6） |
