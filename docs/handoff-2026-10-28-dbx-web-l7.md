# 交接文档：dbx Web 功能对齐（截至 2026-10-28）

> 读者：接手 `oj-bin` 这个仓库、继续把 `../dbx` 的多库/客户端能力复刻到 oj 的人（或 agent）。
> 先读本文，再读 `docs/prds/oj-feature-requirements.md`（需求清单）与对应 `docs/plans/*.md`（每期的设计与评审处置）。
> 工程红线与命令以仓库 `CLAUDE.md` 为准，本文只补「当前状态 + 为什么这么做 + 下一步怎么下手」。

## 0. 一句话状态

（2026-10-28 二次更新）**GA 门禁三件已全部落地**：U-7（client 轴配额与拨号守卫，v0.1.97）、
U-9（安全审计归一 + `audit.query` 可检索存储，v0.1.98）、G9（服务端防护门三件，v0.1.99）；
U-5（v0.1.96）与 U-13 补 tag 同日完成。tag 已到 **v0.1.99**（v0.1.90/v0.1.94 因并版无分界
提交、未补，CHANGELOG 保留「未打标签」）。**均未 push**（显式 `origin`）。PRD 剩余只有
P1/P2：U-11/U-14/U-15/U-16——按 §6 行 7 每条先写 plan 过四路评审再动工。

## 1. 仓库现在处于什么状态

| 项 | 值 |
|---|---|
| HEAD | `f07744e feat(db): dbx Web 对齐 L5（P0 主旅程）+ db 文档并入 db-guide`（= v0.1.93 内容） |
| 工作树 | **未提交**：L6（U-6 插件侧方言解码，记为 v0.1.94）+ L7（U-4 跨请求事务，记为 v0.1.95）—— 27 个已跟踪文件改动（+2041/−221）+ 4 个新文件（`db_session.rs` 1509 行、`handle_core.rs` 271 行、两份 plan 共 579 行） |
| `oj/Cargo.toml` | `0.1.95`（`Cargo.lock` 已同步） |
| 插件版本 | `oj-db-mysql` / `oj-db-postgres` 由 0.1.2 → **0.1.3**（L6 必须同批重建，见 §7） |
| ABI | 仍为 **12**（L6 只动插件内部；L7 纯 host 侧 + JS 全局） |
| 最新 tag | **v0.1.87**（落后 8 个版本，U-13 的由来） |
| 未跟踪新文件 | `src/bridge/db_session.rs`、`src/bridge/handle_core.rs`、两份 plan；另有本文 `docs/handoff-2026-10-28-dbx-web-l7.md` |

工作树里 L6 与 L7 是**两期的改动**，`CHANGELOG.md` 分了 `v0.1.94` 与 `v0.1.95` 两节，实际**同批提交**
（两期在文档文件里的编辑交织，无法按文件切分；已在 v0.1.94 节抬头注明「同批提交、未单独切出发布点」）。
**版本分界以 v0.1.95 那次提交为准**；后续每个版本仍按「一次 bump 提交 = 一个发布点」执行。

## 2. 先跑这四条命令（确认没接手一棵坏树）

```bash
cargo fmt --check
cargo clippy --release --workspace --all-targets -- -D warnings
cargo test --release --workspace          # 期望：only-js 838 / serve 161 / xtask 10 全绿
cargo xtask build && cargo xtask smoke --bin bin/oj
```

最近一次全绿记录（2026-10-28）：fmt ✓、clippy ✓、`only-js` **838**、`serve` **161**、
cert/start **6**、`xtask` **10**、smoke ✓、`cargo test --release -p xtask`（devkit 产物一致性）✓。

> 真库用例（mysql / pg 插件）是 **env-gated**：`OJ_TEST_MYSQL` / `OJ_TEST_PG` 未设时
> `eprintln!("skip: … unset")` 然后 **pass（skip-as-pass）**。**CI 绿不代表这两种方言的行形状被覆盖**，
> 动插件前请按 `plugins/oj-db-mysql/src/lib.rs:1583` 附近注释起的容器实跑。

## 3. 分期与入口（做完的）

| 期 | 内容 | 版本 | 设计文档 | 权威文档入口 |
|---|---|---|---|---|
| L1 | db 访问控制核心（`db_access:` / 风险分类 / `db.allowed()`） | v0.1.89 | `plans/2026-10-27-db-access-control-core.md` | `devkit/db-guide.md` §11.5 |
| L2 | 结构内省（`listColumns/Indexes/ForeignKeys`） | v0.1.90 | `plans/2026-10-28-db-introspection-l2.md` | db-guide §10 |
| L3/L4 | 动态数据源登记（`db.client.*`，实例化 + 生命周期） | v0.1.91/92 | `plans/2026-10-28-db-client-axis-l3.md` / `-l4.md` | db-guide §11.6、scenarios 场景 42 |
| L5 | P0 主旅程（U-8 sqlite 信封化 / U-6 读侧起手 / U-1 运行期授权 / U-3 跨 isolate 失效） | v0.1.93 | `plans/2026-10-28-dbx-web-l5-p0-main-journey.md` | db-guide §11.6、scenarios 场景 42/44/45 |
| L6 | U-6 插件侧方言解码（mysql/pg 读侧扩展 + 两个真错值修复） | v0.1.94 | `plans/2026-10-28-dbx-web-l6-u6-dialect-decode.md` | db-guide §10.6、scenarios 场景 48 |
| L7 | U-4 跨请求事务 `db.session` | v0.1.95 | `plans/2026-10-28-dbx-web-l7-u4-cross-request-tx.md` | db-guide **§4.5**、scenarios 场景 49、plugin-development §15.7 |

## 4. L7（U-4）实现速览——动它之前必须知道的六条不变量

**数据流**：`db.session.begin()` → 宿主建 `TxSessionEntry`（`tx: Arc<ActiveTx>` + 保活 `accessor`）
→ 存进 `StableState.tx_sessions`（进程级，**热更存活**，因为 `app::reuse_backend` 只复用 `stable`）
→ 返回 id → 后续请求 `attach(id)` 把 id 写进 `ReqState.session` → `db::resolve_target`
（`src/bridge/db.rs:314`）先问 `db_session::attached_target`（`db_session.rs:523`）→ 命中即返回
`Target::Tx`，此后占位符改写 / 追踪 / 行数上限 / `db.table()` 构造器 / `nextSeq` / 内省
**全部复用既有事务通路**，一条代码路径两种来源。

| # | 不变量 | 落地位置 | 破坏它的后果 |
|---|---|---|---|
| 1 | id 是 **bearer token**：日志/错误只出 `tx_ab12_…`；未知 / 他人 / 畸形 id **同一文案** | `handle_core::id_prefix` + `db_session::lookup_entry`（`db_session.rs:454`） | 泄露存在性 / 日志伪造（换行注入） |
| 2 | 判权只用**会话登记的别名**（`entry.alias`），绝不按当前请求的 `bound_db` 重算 | `attached_target` 比对后报 `attached to db 'x', not 'y'` | 「A 库判权、B 库执行」 |
| 3 | `db_session.allow` **每个 op 重判**（撤权即时生效），另加 `db_access` 逐语句授权 | `require_gate`（`db_session.rs:425`） | 撤权后仍可续用 |
| 4 | 会话内禁 `BEGIN/COMMIT/ROLLBACK/SAVEPOINT/RELEASE/START/SET/USE/ATTACH/DETACH/PRAGMA` | `forbid_in_session`（`db_session.rs:582`），由 `query_impl`/`exec_impl` 统一调用 | MySQL 隐式提交 ⇒ **假回滚** |
| 5 | 容量「先占位再建连」；满了**拒绝新会话，绝不驱逐他人** | `Inner.pending` + `reserve()`（`db_session.rs:213`） | 并发超额 / 顶掉别人的事务 |
| 6 | 回收只用 `try_lock`（不阻塞 runtime）+ 语句侧 `timeout(剩余寿命)` 兜底；判死的连接**不归还** | `reap`（`db_session.rs:329`）、`db::on_tx`（`db.rs:340`）、`query.rs` 构造器分支 | current_thread 被饿死 / 状态不可信的连接被复用 |

**观测面**：所有事件走单一 `emit()`（`db_session.rs:389`，`DB_SESSION_{BEGIN,COMMIT,ROLLBACK,TIMEOUT,
TIMEOUT_ROLLBACK_FAILED,DISCARD,CLOSE_ERR}`）；`db.session.stats()` 给 `active/timedOut/discarded/rejected/boot`。
**U-9（审计落存储）就从这一个出口接**——别再散点 `eprintln!`。

**墓碑（tombstone）**：已结束的会话在 60s 内对本主体给出「死法 + 原因」
（`is closed (commit)` / `expired (idle_timeout); it was rolled back`）；对他人一律 `not found`。
措辞是按状态分支的——**`commit` 成功的会话不许说 rolled back**（这条有测试钉着）。

## 5. 本轮改动的文件地图

| 文件 | 改了什么 | 动它要连带 |
|---|---|---|
| `src/bridge/db_session.rs`（新，1509 行） | 会话表 / 三道路由 / 13 个 op / 15 个测试（临时文件 sqlite，`Bridge::run` 一次 = 一个请求） | 改 op 签名要同步 `bootstrap.js` 的 import 列表与 `sample/global.d.ts` |
| `src/bridge/handle_core.rs`（新，271 行） | `Owner` / `new_id` / `id_is_wellformed` / `id_prefix` / `expires_in` / `expired_reason` + 5 测试 | **U-5 直接复用**（不要另造泛型表） |
| `src/bridge/db.rs` | `resolve_target` 先查会话；`query_impl`/`exec_impl`/`op_db_tx_begin` 加会话门；`on_tx` 寿命封顶 | 新 op 走 `db.query` 都必须过 `forbid_in_session` |
| `src/bridge/query.rs` | 构造器 `Target::Tx` 分支：等连接的 `lock()` 也按剩余寿命封顶 | 查询本身不单独计时（与 `db.tx` 同策略，已写进文档） |
| `src/bridge/mod.rs` | `StableState.{db_session,tx_sessions}` + `Extras.{db_session,tx_sessions}` + `ReqState.session` + 13 op 注册 | 加 `StableState` 字段要同步测试里的字面量（`platform.rs:221` 曾漏过） |
| `src/bridge/bootstrap.js` | `dbSessionCall`（把同步 op 也包成 Promise）+ `DB(name).session.*` 13 个方法 | **7-bit ASCII**；漏 import op 名 = `ReferenceError: op_… is not defined` |
| `src/bridge/db_policy.rs` | `DbSessionSection`（默认全关）+ `identity_of` 提升为 `pub(crate)` | 默认值改动等于改兼容性 |
| `src/config.rs` | 顶层 `db_session:`（`null_as_default`）+ `known_top_level_keys` + mapping 类型探测 | 新顶层段三处都要登记，否则要么静默忽略要么报 unknown key |
| `oj/src/app.rs`（:1282） | 装配期 fail-loud（空 allow / 上限为 0 / `max_per_owner > max_sessions` / idle=0 / lifetime<idle）+ sqlite 别名 `warn:` | 校验消息要给配置键名（用户按文案就能改） |
| `oj/src/serve_cmd.rs` | `db_session` 归 `restart_pending` | 热更分类表漏了 = 改了配置却静默不生效 |
| `src/bridge/client_db.rs` | `discard_alias`：`delete` / 成功 `reRegister` 后判死该别名会话 | 别在 reRegister 失败时也判死（会白杀用户会话） |
| `src/bridge/accessor_sqlx.rs` | `redact_msg`：**只有文本里真含凭据 URL** 才脱敏 | 直接调 `secret::redact` 会把所有 sqlite 报错变成 `"***"`（L7 期间踩到过） |
| `plugins/oj-db-{mysql,postgres}/src/lib.rs` | L6 读侧解码（typed 池）、`BIT(n)` 错值修复、PG `numeric` 去尾零、错误串列名消毒 | 改插件必须 bump 插件自身版本（已 0.1.3），且与宿主同批发 |
| `sample/global.d.ts` | `DBInstance.session` + `DBSessionApi` / `DbSessionSummary` / `DbSessionStats` | devkit 的 `bin/devkit/global.d.ts` 由 `cargo xtask build` 拷贝，别手改产物 |
| docs | `db-guide.md` §4.5 / `api-manual.md` §6·§13 / `scenarios.md` 场景 49 / `SKILL.md` / `devkit/README.md` / `docs/api/06-db.md` / `plugins/plugin-development.md` §15.7 / `CHANGELOG.md` | devkit 四件必须与该版用户可见变更逐条对齐，改完跑 `cargo xtask build` + `cargo test --release -p xtask` |

## 6. 剩余工作（按建议顺序）

| 序 | 条目 | 缺口（现状） | 第一刀 |
|---|---|---|---|
| 1 | ~~提交 L6 / L7~~ **已完成**（2026-10-28） | 两期同批提交为一个 commit（文档交织不可切分，CHANGELOG v0.1.94 节已注明），版本记 0.1.95 | 后续每版仍按「一次 bump 提交 = 一个发布点」 |
| 2 | ~~U-5 execution-id 取消~~ **已完成**（v0.1.96） | `db.cancel(executionId)` 跨请求取消流式执行；实现走「宿主级 `ExecRegistry` 路由到既有 pump abort」——**零 ABI/插件改动**（后端真取消经 `StreamCursorGuard::drop` 原样复用）。已知限制：只覆盖流式执行（非流式无后端句柄） | 设计文档 `plans/2026-10-28-dbx-web-u5-execution-cancel.md`；非流式真覆盖需插件侧 one-shot 登记，另期 |
| 3 | ~~U-7 client 轴配额~~ **已完成**（v0.1.97） | `max_per_owner` / `max_streams`（全局流预算，pump 持 permit）+ 拨号守卫（5s 超时 + 连续 8 失败锁退 60s）；`DB_CLIENT_DENY` 日志。已知限制：查询超时/字节上限未覆盖（平台级缺口） | plan：`plans/2026-10-28-dbx-web-u7-client-quota.md` |
| 4 | ~~U-9 审计落存储~~ **已完成**（v0.1.98） | 四族出口归一 `audit::emit` 单一出口 + 进程内环形存储（`audit:` 段，2000 条/24h 双限）+ `audit.query` 检索（读门 `audit.allow`）。stderr 归一 `OJ_AUDIT:` 格式（**旧前缀退役**）。已知限制：不跨重启、无防篡改（外部 sink 另期）；sql_trace 不归审计轴 | plan：`plans/2026-10-28-dbx-web-u9-audit.md` |
| 5 | ~~G9 服务端防护门~~ **已完成**（v0.1.99） | ① `begin({readOnly:true})` 只读会话位（op 层双入口，信封 `disposition=keep`）；② 写拦截即①的强制 + 既有 read_only 回归锚；③ `dangerous_confirm_token` 确认门（头 `x-oj-confirm`，放行落 `DB_DANGEROUS_CONFIRM`）。连接级硬保证仍是 DB GRANT（登记 db-guide） | plan：`plans/2026-10-28-dbx-web-g9-op-gates.md` |
| 6 | ~~U-13 补 tag~~ **已完成**（2026-10-28，本地；push 需显式 `origin`） | v0.1.88/89/91/92/93/95/96 已打 annotated tag；**v0.1.90（L2/L3 并版）与 v0.1.94（L6/L7 并版）无分界提交，未补**——方案包钉 `peerOj` 下限时避开这两个版本 | |
| 7 | U-11 能力探测 / U-14 内省扩展 / U-15 管理面 connector / U-16（条件项） | **四份设计稿已写**（2026-10-28）：`plans/2026-10-28-u11-connection-capability-probe.md`（单期，建议与 U-14 批 a 同车）/ `-u14-introspection-expansion.md`（宿主侧 SQL 常量三批次，零 ABI）/ `-u15-admin-connector-axis.md`（泛型轴承载，零新 ABI，首个 mongo）/ `-u16-ssh-tunnel-conditional.md`（立项申请书，条件未触发） | 各稿 §「待评审问题」即四路评审议程；**过评审才动工** |

**明确不做**（PRD §6/§7，别重复讨论）：U-10 运行期凭据轴、U-12 LOB 轴、PL/SQL 调试协议、
Oracle/达梦方言（门禁未过）、导出格式生成、SQL 文本函数、AI/MCP、连接持久化（**已裁决由方案包自管**）。

## 7. 踩过的坑（按「会浪费你半天」排序）

1. **`secret::redact` 不是通用脱敏**：它对不含 `://` 的串一律返回 `"***"`。L7 期间所有 sqlite
   报错都变成 `***`（15 个用例同时红）。要么用 `accessor_sqlx::redact_msg` 的判据，要么先判断再调。
2. **`op2` 返回类型很挑**：`#[serde]` 不能写 `bool` 或裸 `Value`（要 `serde_json::Value`）；
   fast-compatible 的 op 必须标 `#[op2(fast)]`；需要异常通道用 `#[op2(nofast)]`。
3. **JS 面每个 session 方法都必须返回 Promise**：同步 op 直接挂到 `DB(name)` 上会得到
   `info(...).then is not a function`，于是有了 `dbSessionCall` 包装。新增 op 照办。
4. **`bootstrap.js` 的 op 名要手动加进顶部 import 列表**，漏了就是 `ReferenceError`，且只在该
   op 第一次被调用时炸。
5. **sqlite 池刻意只有 1 条连接**（`accessor_sqlx.rs:39`）：一个会话会独占该别名，其它请求排队。
   装配期已 `warn:`；跨请求事务请用 mysql/postgres。写测试同理——两条并发的 sqlite 事务会互相等死。
6. **测试用 `:memory:` sqlite 是陷阱**：每条连接一个独立库，跨请求会话读到空库。用**临时文件**，
   且 sqlx 不会替你 `create_if_missing`，得先建空文件（`db_session::tests::tmp_dsn`）。
7. **`StableState` 加字段会打断 `platform.rs` 里的测试字面量**（只在 `--all-targets` 下暴露）。
8. **worktree 里跑测试**要软链主仓 `target`（别设 `CARGO_TARGET_DIR`，夹具会红）。
9. **git 有两个远端**：`origin` = GitHub 在用，`gitee` 是停在 v0.1.50 且历史被重写的陈旧镜像。
   推送一律显式 `origin`。
10. **插件改动必须 bump 插件自己的版本**（`plugins/*/Cargo.toml` + `Cargo.lock` + CHANGELOG），
    且 `cargo xtask plugin <name>` 不刷新 `plugins.lock.json` —— 收尾一律 `cargo xtask build`。

## 8. 发版流程（与 CLAUDE.md / CHANGELOG 抬头一致，别自创）

1. bump `oj/Cargo.toml` 的 version（`Cargo.lock` 同步）；插件有改动则各自 bump。
2. `CHANGELOG.md` 新增一节：`## vX.Y.Z（未打标签）`，fix 类单独成组；**待做/未验证的诚实登记**。
3. 更新 `docs/devkit/` 四件（`api-manual.md` / `scenarios.md` / `SKILL.md` / `README.md`）与该版
   **用户可见变更**逐条对齐；db 相关内容一律进 `db-guide.md`。
4. `cargo xtask build`（归置 `bin/` + `bin/devkit/`）→ `cargo test --release -p xtask`（产物一致性）。
5. 全门禁：fmt / clippy（workspace + all-targets）/ `cargo test --release --workspace` /
   `cargo xtask smoke --bin bin/oj`。
6. 打 annotated tag 并 `git push origin vX.Y.Z`；**不打标签就必须在 CHANGELOG 该节写明「未打标签」**
   （文档标签状态必须与实际 `git tag` 一致）。

## 9. 哪些结论有测试锚，哪些只是文档声明

有锚（可以信）：

- 会话跨请求存活、只有 `commit` 才对外可见；rollback 后墓碑可见且措辞按状态分支（`db_session.rs` 15 用例）。
- id 形状 / 空白 / 越权 / 过期四条路径的**同一文案**与「完整 id 不出现在错误里」。
- 事务控制语句 7 条全拒、`db.tx` 双向互斥、跨别名拒绝、`discard_alias`、空闲过期 + 回滚、
  容量上限、`allow` 逐 op 重判、`require_identity`、`touch` 剩余寿命、`close` 幂等 + `stats`。
- L6 的 mysql / pg 真库行形状（**但 env-gated，CI 默认 skip**）。

只有文档声明（下次动到要补测）：

- 长时间持锁的会话最终被判死（`in_flight_until` 硬截止）——单测里没有真挂死场景。
- 热更期间会话存活（`reuse_backend` 复用 `stable` 是读代码得出的结论，没有跨热更用例）。
- 并发 op 串行排队（`tokio::sync::Mutex` 语义，无并发压力用例）。
- 进程退出 / panic 时的兜底回滚（靠 `Drop`，无集成用例）。

## 10. 交接前最后核对

```bash
git status --short          # 期望 27 个改动 + 5 个新文件（含本文，均未提交）
grep -n "0.1.95" oj/Cargo.toml Cargo.lock | head -3
git tag --sort=-v:refname | head -1     # 期望 v0.1.87（U-13 未做）
ls docs/plans/ | tail -4                # L6 / L7 两份 plan 在位
```
