# REQ-11 设计：非流式取消与执行进度

> 上游诉求源：`../oj-dbx/docs/upstream/REQ-11-nonstream-cancel-and-progress.md`（§3.1-3.4 + §4 验收）。
> 目标版本 v0.1.114。纯 host 侧（`src/bridge/` + `serve/` 少量），**零 ABI**。
> 关键事实（勘察已核）：真后端取消（pg_cancel_backend / KILL QUERY）目前**只存在于插件
> typed 专用连接**（流式路径）；核心池路径（SqlxAccessor，Any driver）从未有后端句柄——
> `db_exec.rs` 头部明确裁决「非流式假取消不做」。本设计的第一决策就是破这个裁决。

## 1. 意图与为什么

「取消」只对 db.stream 有效：query/exec 发出后无句柄，只能等 statement_timeout 到点。
三个用户可感知后果：取消按钮是死的、关标签页服务端照跑、长 ALTER/SELECT 无进度证据。
机制的一半已存在（executionId 空间、cancel() 路由、ExecRegistry、KillSwitch），缺的是
**池路径的后端取消句柄**与**请求挂断联动**。

## 2. 目标 / 非目标

**目标**

1. **G1 executionId 面**：`db.query/exec(sql, params, {onOpen})` 开跑后经 onOpen 给出
   与 stream 同空间的 executionId；`db.cancel(id)` 受理；取消 → Promise reject
   `OJ_DB_CANCELLED`（与 `OJ_DB_TIMEOUT` 可区分）。
2. **G2 挂断即取消**：HTTP 客户端断开（关标签页/导航）→ 服务端 5s 内停跑该请求发起的
   语句；幂等；不得把「已取消」错报成「查询失败」。
3. **G3 进度上报**：stream 加 `onProgress`（rows/bytes/elapsedMs 增量）；query/exec 加
   相位进度 `{phase:"sent"|"fetching"|"done", elapsedMs, rows?}`。

**非目标**

- 事务臂（attached session）语句的跨请求取消——Tx 有自己的生命周期语义，取消整个
  session 而非单语句；Tx 内语句取消随 session 关闭走，不进本批（登记）。
- WebSocket/inspector 通道的取消（同机制可复用但 UI 未要求）。
- 排队等待连接池的语句的取消语义细化——取到连接前的取消 = 直接不执行（reject
  CANCELLED），已实现自然。

**反目标**：假取消（只丢 future 服务端照跑）；把 CANCELLED 报成 TIMEOUT/失败；
取消破坏连接池（被 KILL 的连接必须报废不回池——红线已在流式路径钉死，池路径同口径）。

## 3. 契约（JS 可见面）

```ts
interface DBCallOpts {
  timeoutMs?: number;
  /** G1：执行在服务端开跑后回调（拿到池连接、语句已发出）。同 stream.onOpen 语义。 */
  onOpen?: (executionId: string) => void;
  /** G3：相位进度（节流 ≥100ms；非流式拿不到行级增量）。 */
  onProgress?: (p: { phase: "sent" | "fetching" | "done"; elapsedMs: number; rows?: number }) => void;
}

// db.stream opts 追加：
opts.onProgress?: (p: { rows: number; bytes: number; elapsedMs: number }) => void; // 节流 ≥100ms

// db.cancel(executionId)：语义不变（同 id 空间，流/非流一并受理）；幂等；
// 未知/他人 id 同一条 not-found（bearer 语义不变，日志只出前 8 字符）。
```

「开跑」的定义（onOpen 时机）：池路径 = 取到连接 + pid 查询完成 + 语句已 submit；
此之前 cancel 竞态 = 登记后即刻可取消（cancel 标志置位 → 开跑前检查 → 不执行直接
CANCELLED reject）。`sent` 相位在 onOpen 同刻发，`fetching` 在首行/首个响应到达时发，
`done` 在 settle 前发。

## 4. 设计决策

### 4.1 G1 池路径后端取消（核心新增）

- `SqlxAccessor` 加「可取消执行」包装：`query_with_params_cancelable` /
  `exec_with_params_cancelable`（旧方法保留 = 内部直接包一层 None）。流程：
  1. `pool.acquire()` 取连接（等待池空闲的时段可被 cancel 标志提前打断）；
  2. 按方言查后端标识——PG `SELECT pg_backend_pid()`、MySQL `SELECT CONNECTION_ID()`
     （一条引擎常量 SQL，prepared 可用）；
  3. 登记 ExecRegistry（**新 entry 变体**，见 4.2）→ 发 sent/onOpen → 跑语句 →
     完成/出错 settle（done/onProgress）→ 摘登记。
- 取消链：`db.cancel(id)` → 置 `cancel_requested` + notify → 登记表现有路由扩成枚举
  （流式句柄 | 池句柄）→ 池句柄取消 = 从池另取**一条短连接**发方言取消语句
  （PG `SELECT pg_cancel_backend($1)`；MySQL `KILL QUERY ?`——prepared 可用，批次 2
  评审已证）→ 被 kill 的连接上原语句报错退出 → 判定 `cancel_requested` 标志 →
  映射 `OJ_DB_CANCELLED`，**连接报废丢弃**（`pool.close()` on the connection，绝不
  归还——被 kill 的会话状态不可信，同流式路径红线）。
- reject 形态：结构化 envelope，`code = "OJ_DB_CANCELLED"`（`db_error.rs` 已有
  category 直接复用）；op 层 catch 到带 cancel 标志的 error 时改写 category，
  绝不落 `OJ_DB_INTERNAL`。

### 4.2 ExecRegistry 变体（同表异型）

```rust
enum ExecHandle {
    Stream(Arc<DbStreamShared>),            // 既有
    Pool {                                 // 新增
        owner: Owner,
        alias: String,
        backend: BackendId,                 // PgPid(i32) | MysqlConnId(u64)
        cancel_requested: AtomicBool,
        notify: Arc<Notify>,                // 等池连接阶段的可打断等待
        pool: Arc<dyn AnyPool-ish>,         // 短连接取消用（类型擦除见 4.5）
    },
}
```

- 路由 `db.cancel` 按变体分发；登记/清扫逻辑（懒惰清扫 + TTL）两型共用。
- stream 的 id 空间与池执行**同表不同前缀**？——不：保持同一 `exec_` 前缀同空间
  （REQ §3.1「同一套 id 空间」原文要求）；变体内部分发即可。

### 4.3 G2 挂断即取消

- 载体：`ReqState` 持有本请求登记的池执行 id 列表（`Vec<String>`，reset 清空）；
  `ReqState::reset`/drop 时对未 settle 的池执行**异步触发 cancel**（置标志 + notify，
  不阻塞 reset 路径——fire-and-forget，取消链自驱）。
- serve 侧前提（**计划期首任务即验证**）：确认 axum handler future 在客户端断开时
  会被 drop/abort（serve 现状若 handler 跑在连接任务内则自然成立；若 response 已
  collect 后才发送则不成立——需改 response channel 为 body drop 感知）。若现状不满足，
  本决策改为「axum 层把 response body 换成 drop-sensitive stream」，仍零 ABI。
- 幂等与错报防护：cancel 触发的 reject 一律 CANCELLED；正常完成的语句即使请求已断
  也保留结果（写操作的副作用不 rollback——文档写明「挂断取消尽力而为，已提交不写回」）。

### 4.4 G3 进度

- 节流：同一执行内两次 onProgress 间隔 ≥100ms（`Instant` 判定，最后 done 相位不受
  节流限制必发）。
- stream：pump 循环里已计数 rows/bytes——在泵迭代处插节流回调即可（零新状态）。
- query/exec 池路径：rows 计数 = `fetch` 行数（AnyRow 流），phase 迁移 =
  sent（submit 时）/fetching（首行）/done（settle 前）。

### 4.5 类型擦除与插件路径

- 池取消短连接需对 `sqlx::AnyPool` 与插件 typed 连接都能发方言语句。池路径统一走
  核心 `AnyPool`（query/exec 本来就不过插件 vtable），**不动 ABI**；
  `AnyPool` 按 `dialect_info` 分支选取消 SQL 与 pid 查询 SQL（引擎常量）。
- 插件 typed 连接上的查询（若未来开放）不在本批；registry 的 Pool 变体只服务核心池。

### 4.6 安全与红线自查

- bearer 语义不变：id 日志只出前 8 字符；他人/未知/畸形/已收尾 id 同一条 not-found。
- 判权同 `Owner::same_subject`；池执行开跑已过 `db_access` 逐语句授权，登记不再加门
  （与流式同信任域）。
- 被 kill 的连接报废（`close()`），绝不回池。
- KILL QUERY 在 MySQL 策略已分类 Write（v0.1.112）——cancel 链内部调用不走 JS 策略面
  （宿主内部通道），但 cancel 动作本身要求原语句已过写授权（同「能跑才能消」原则）。

## 5. 验收映射（REQ §4 → 本设计）

| 上游验收 | 落点 |
|---|---|
| query onOpen 拿 id；另一请求 cancel 后 1s 内 OJ_DB_CANCELLED reject（PG/MySQL 各一） | G1 env-gated 双腿（pg_sleep / MySQL SLEEP 长语句） |
| exec 长 ALTER 同上 | G1（MySQL ALTER 加索引真库腿；PG 用 `select pg_sleep` in txn?——exec 腿用 `ALTER ... VALIDATE` 慢件或 `select pg_sleep` 经 exec 许可面） |
| 浏览器断开 → 5s 内服务端无该语句（pg_stat_activity / SHOW PROCESSLIST） | G2 e2e：起服务、发请求、断连、查活动会话 |
| CANCELLED vs TIMEOUT 可区分 | 单测钉 error code；不共用 category |
| onProgress 10s 级查询 ≥3 次且 elapsedMs 单调 | G3 env-gated（pg_sleep(10) + 计数回调断言） |
| stream executionId()/info() 零变化 | 既有 stream 全套测试不红即证 |

## 6. 交付物清单

- `src/bridge/db.rs` / `accessor_sqlx.rs`：可取消执行包装 + pid 查询 + 进度相位。
- `src/bridge/db_exec.rs`：ExecRegistry 变体 + cancel 路由扩展。
- `src/bridge/db_query.rs` / `db_exec op`：opts 解析（onOpen/onProgress）+ CANCELLED 映射。
- `src/bridge/db_stream.rs`：pump 节流 onProgress。
- `src/bridge/mod.rs`：ReqState 登记池执行 id + reset/drop 联动。
- `serve/src/lib.rs`：断开感知验证/改造（G2 前提）。
- devkit 四件 + CHANGELOG v0.1.114 + 下游对账。
- 测试：离线单测（竞态：开跑前 cancel / 完成后 cancel / 他人 id）+ env-gated 真库
  双腿 + 挂断 e2e。

## 7. 留给计划阶段的口子

- serve 断开感知现状未验——计划 Task 0 先验证再定 G2 实现形态。
- `KILL QUERY ?` 参数化在目标 MySQL 8.4 实测（批次 2 评审证过 prepared 可用，但
  绑定参数形式须真库钉一次）。
- onOpen 回调抛异常时的 settle 语义（钉：回调异常不影响执行本身，catch 记 warn）。
