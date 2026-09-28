# oj exec 子命令设计（v0.1.29）

日期：2026-09-28 ｜ 状态：已评审（架构师 + 工程师双评审，修订后 v2） ｜ 版本目标：v0.1.29

## 1. 需求

`oj exec <file.ts|js>`：在 oj 桥接运行时中直接执行一个 ts/js 文件。脚本拥有与
handler 相同的注入全局（`json`/`db`/`kv`/`blob`/`bus`/`es`/`fetch`/`ws`/`log`/
`plugins`/`cert`/`jwt`/`bcrypt`/`crypto`/`oidc`/`ldap`/`mail`/`mq`），输出到终端
（stdout 直出），可选 `--log-file` 同时落盘 JSONL。

已确认的需求决策（用户逐项拍板）：

1. **完整后端**：复用 server 的装配路径（db/kv/es/blob/bus/... 全部可用），`-c config.yaml` 必选。
2. **终端输出 = stdout 直出**：`console.*` 与 `log.*` 都直接打到 stdout（人类可读）。
3. **日志落盘可选**：默认不落盘；`--log-file <path>` 时才写（与终端双写）。
4. **脚本参数**：`--` 之后的 argv 注入脚本。
5. **支持相对导入**：脚本可 import 项目根内的其他 .ts/.js。

## 2. 架构：拆分 App::from_config

`oj/src/app.rs` 的装配拆为两段。评审修正：拆分清单以**逐步对照表**呈现
（每个有副作用的步骤必须步步有着落），且 `Backend` 收敛为 `StableState` 单源
——评审 H1/H2 发现原散文清单漏列 StableState / prewarm_boot / reconcile /
anon_paths 校验，且字段清单与 StableState 大面积重复成双数据源。

### 2.1 `assemble_backend`（新增，后端核心）

```rust
async fn assemble_backend(
    cfg: &Config,
    config_dir: &Path,
    dir: &Path,          // api 目录（schema 白名单来源）
    base: &str,          // blob 下载 URL 前缀：server 传 CLI 解析值（-b 覆盖 >
                         // server.api_prefix），exec 传 config 派生值
    ts: bool,
    db_override: Option<&str>,
) -> Result<Backend, String>

pub struct Backend {
    /// 唯一数据源：kv/dbs/registry/loader/blobs/bus/es/modules/plugins/... 全在这里，
    /// 不再逐字段复制。exec/test 的手工 runtime 与 server 的 App 共用同一份。
    stable: Arc<StableState>,
    /// auth 守卫（oj-auth 插件 vtable 构造；cfg.auth 未配置 → None）。
    auth_guard: Option<Arc<dyn only_js::bridge::AuthGuard>>,
    /// Bridge 工厂。tasks_flag 参数化：None = HTTP 桥，Some = 任务桥。
    make_bridge_of: Arc<dyn Fn(Option<Arc<AtomicBool>>) -> Bridge + Send + Sync>,
}

impl Backend {
    pub fn stable(&self) -> &Arc<StableState>;
    pub fn make_bridge(&self) -> impl Fn() -> Bridge;          // = make_bridge_of(None)
    pub fn make_task_bridge(&self, flag: Arc<AtomicBool>) -> Arc<dyn Fn() -> Bridge + Send + Sync>;
}
```

`StableState` 的构造整个迁入 `assemble_backend`（现状在 `from_config` 尾部构造，
app.rs 注释「单一 StableState」不变量由此得到唯一载体）。注意
`Bridge::with_dbs_and_loader` 内部仍会从 Extras 再建一份 StableState——这是既有
设计（actor 池与手工 runtime 各持一份、共享后端 Arc），**不动**；`Backend.stable`
仅供 exec/test 这类手工 `JsRuntime::new` 路径使用，文档写明以免误解。

### 2.2 步骤归属对照表（拆分验收条件）

| # | 步骤 | 归属 | 备注 |
|---|---|---|---|
| 1 | redis 其余 key warn | backend | |
| 2 | loader 构建（project_root=config_dir） | backend | exec 复用同一份 |
| 3 | ext_boot specifier 冻结 | backend | |
| 4 | 插件装配（strict/scan + ABI + Registries/PluginInfo） | backend | |
| 5 | kv（redis.default → 插件 / InMemory 兜底） | backend | |
| 6 | es / blob（assemble_blobs + default） | backend | blob default 经 `stable.blobs` 暴露给 HTTP 层 |
| 7 | dbs 连接 + db_override 校验 | backend | server 的 migrate/seed 从 `stable.dbs` 读 |
| 8 | query_limits 校验 / ownership / sql_guard | backend | |
| 9 | **config 匿名路径校验**（validate_anon_paths） | HTTP 层 | 纯 config 校验，HTTP 管线消费 |
| 10 | **迁移 gate 执行**：`migrate::apply_all / verify_all` | HTTP 层 | exec 默认跳过（§3.1） |
| 11 | schema + modules 构建（**剥离 reconcile**） | backend | reconcile 见 #12 |
| 12 | **reconcile**（auto 模式声明漂移补偿） | HTTP 层 | 抽成独立 `schema::reconcile_all`，server 在 gate=auto 时于 #10 之后调用；exec 永不调用 |
| 13 | seed 重放 / fixtures | HTTP 层 | exec 不做 |
| 14 | auth 守卫 vtable → guard + jwt 空值校验 | backend | guard 存 `Backend.auth_guard` |
| 15 | jwt/oidc 原语配置 | backend | |
| 16 | bus 连接 | backend | |
| 17 | mail 后端 + **显式 `mail::install_mail_deliver` 一次** | backend | 评审 M1：该路由现状只在 Bridge 构造时安装，exec 手工 runtime 不经 Bridge 会静默丢 `mail.result` 上送；install 为进程级弱引用注册，幂等性实现时确认 |
| 18 | ldap 后端 | backend | |
| 19 | mq 命名实例（kafkas/rabbits） | backend | |
| 20 | vars 冻结 / tasks_flag 创建 | 分界 | vars 进 Extras；**tasks_flag 由 HTTP 层（App）创建**，经参数传入 make_bridge_of |
| 21 | ext_boot 预热（`prewarm_boot`） | HTTP 层 | 必须位于 assemble_backend 之后、路由表内省之前（原注释：不做则 boot 错误被内省 join 吞成「路由全空、服务照常监听」）。exec 侧不跑 prewarm_boot，改用建 runtime 后的 `boot_runtime`（与 `oj test` 同路径，§3.2） |
| 22 | 路由表（dev/release）+ actor 池 + 静态站点 + 证书加载/watcher + Pipeline + WS + Router | HTTP 层 | |
| 23 | StableState 组装 | backend | 迁入 assemble_backend 尾部 |

**顺序即语义**的三个依赖点（拆分验收条件）：apply_all 先于 schema build（#10→#11）；
schema build 先于 seed（#11→#13）；prewarm 先于路由内省（#21→#22）。

## 3. exec 侧组件

| 组件 | 职责 |
|---|---|
| `oj/src/args.rs` `ExecArgs` | `<file> [-c config] [-d dir] [--db name] [--log-file path] [-- arg...]`；透传 argv 用 `#[arg(last = true)]`（否则 `-- -x` 被 clap 拒） |
| `oj/src/exec_cmd.rs` | 入口编排（§3.2 流程） |
| `oj/src/exec_ext.rs` + `exec_bootstrap.js` | deno extension：`console.*` 从零定义 + `log.*` 改挂 `op_exec_log`；**options 模式**注入 args/log_file（见 §3.2） |
| `op_exec_log` | Rust sink：人类可读行 → stdout；`--log-file` 时双写 JSONL |

`exec_bootstrap.js` 必须 7-bit ASCII（红线，同 `bootstrap.js`）。

### 3.1 与 server 的装配差异

- **无证书门禁**：exec 不构造 HTTP 层，证书路径不校验。注意现状 `oj test` 是走
  from_config 因而要求证书的——exec 跳过它是有意分叉，api-manual 写一句。
- **迁移三项（apply_all / verify_all / reconcile_all）默认全跳过**：server 的
  dev-auto 语义不适合短脚本；仅 config 显式写 `migrate_on_start: auto|verify` 时
  exec 执行对应项（reconcile 跟随 auto）。**有意不对称**：server dev 缺省 auto，
  exec 缺省 off——ops-manual 必须写明，防运维拿 server 直觉套 exec。
- **无 seed/fixtures、路由、actor 池、静态站点、WS**。
- **租户/归属守卫语义**：exec 无 HTTP 请求上下文（tenantId 恒 None、module 恒
  None）——视同匿名系统操作员。`sql_guard=deny` 的库上脚本须 `db.asSystem()`；
  归属守卫对 exec 不设防属预期。进 api-manual exec 章节 + SKILL.md 陷阱表。
- **`json.*`/`finish`/`http` 空转**：写入无人读的 `ReqState.capture`——复用
  handler 代码不报错，但没有输出语义（`json.ok(x)` 不会打印 x）。一句话文档化。

### 3.2 执行流程

```
parse args → load_app_config(config, dir, base=None)   // exec 恒 dev 语义 ts=true
  → 钉线程（dedicated OS thread + current_thread runtime，同 oj test 钉法——JsRuntime !Send）
    → Backend = assemble_backend(...)              // 失败 → stderr + 退出码 1
    → extensions = ws_client_extensions()
                   + bridge_ext_init(backend.stable().clone())
                   + exec_ext_init(ExecOptions { args, log_file })
      // 顺序钉死：ws 在前（bootstrap.js 静态 import 依赖）；bridge_ext 居中；
      // exec_ext 最后（覆盖 log 依赖 bootstrap 先跑）
    → patch_fs_loaded_sources(&mut extensions)     // 必须在 JsRuntime::new 之前（红线）
    → JsRuntime::new(...)                          // extension esm entry 在 new 内部求值：
      // exec_bootstrap 顶层 globalThis.args = op_exec_args()
      // ——args/Sink 经 ExecOptions→state 闭包 put 进 OpState（bridge_ext 同构，
      //   评审 H1：new 之后再 put 的值 bootstrap 期读不到，时序不成立）
    → ext_boot 预热（config 配了才跑，boot_runtime，与 oj test 同路径）
    → specifier = 脚本绝对路径 file://
    → load_side_es_module + mod_evaluate + run_event_loop  // 顶层 await 跑到 settle
  → 退出码
```

### 3.3 输出通道

- `console.*` 与 `log.*` 都经 `op_exec_log` 到 Rust sink。
- **console 格式化约定**：多参数 `console.log("a", 1, {x:2})` →
  `args.map(stringify).join(" ")`；stringify 为 exec_bootstrap **自带** BigInt-safe
  实现（bridge bootstrap 的 ojStringify 是模块内函数，exec_bootstrap 拿不到）。
- 终端格式：`{LEVEL:5}  {msg}`（级别列对齐；msg 内换行原样保留）。
- `--log-file f.jsonl`：同事件追加一行 `{"ts":"<RFC3339>","level":"INFO","msg":"..."}`
  （exec 自定义 JSONL 形态；server 落盘是 tracing 文本行经 tee，无 JSONL 先例——
  修订措辞，见 Task 5 评审）。文件打开失败 → stderr warn 一次（既有 stderr tracing
  subscriber 通道，main.rs 非 server 命令已装，stdout 因此天然干净），终端照出，
  脚本继续。
- **console 仅 exec 可用**：server/test 的 runtime 无 console（deno_core 默认不
  提供，已核实）。同一脚本片段拷进 handler 会 ReferenceError——api-manual 与
  SKILL.md 明确声明。
- 否决过的替代方案（记录防后人重提）：console shim 到现有 `op_log` + tracing
  subscriber 双 layer。省一个新 op，但装配/插件的 Rust tracing 会混进 stdout，
  破坏管道友好性——否决。

### 3.4 模块导入与 TS

- **复用 Backend 的 loader**（`LoaderShared.project_root = config_dir`，与 server
  同源；评审 H4 否决了「脚本所在目录」方案——会与 `stable.loader` 分叉导致 CJS
  require 与 ESM import 解析上界不一致）。
- 入口脚本本身可位于任意路径（绝对 file:// specifier 直接放行）。
- 脚本的 import 被 `ensure_within` 钳制在**项目根（config_dir）内**，推论写明：
  (a) `import "../x"` 从脚本目录上跳即被拒（"escapes project root"）；
  (b) 脚本放在项目外（如 `/tmp/foo.ts`）时连 `./util.ts` 都导不了——若实测为真
  需求，后续再给 assemble_backend 加 project_root override（本期不做）。
- **相对导入必须带显式扩展名**（`import "./util.ts"`）：loader 的 .ts 探测与
  api 目录 dev/release 判定耦合，`-d` 指向 dist 时省略扩展名会解析失败。
- `.js` 原样执行；`.ts` 走 `transpile.rs`；其他扩展名 fail-fast。
- `-d dir` 缺省自动探测（同 `oj test`）；探测不到 → 空 SchemaRegistry + stderr
  warn 继续（纯 kv/log/fetch 脚本不需要表白名单）。
- exec 恒 `ts=true`（dev 语义；脚本没有 release 形态概念）。

### 3.5 脚本参数

`--` 之后的 argv 原样（不做转义/展开）经 `ExecOptions.args` 注入
`globalThis.args: string[]`；无 `--` 时为空数组。注入用 op（options 模式），不是
静态 bootstrap 拼字符串。

## 4. 错误处理与退出码

| 场景 | 行为 | 退出码 |
|---|---|---|
| config 缺失/解析失败 | stderr 报错（`load_app_config` 文案） | 1 |
| 后端装配失败（插件 ABI / db / kv / ...） | stderr 透传装配段 fail-fast 文案 | 1 |
| 脚本不存在 / 非 .ts/.js | stderr `exec: 仅支持 .ts/.js: <path>` | 1 |
| 脚本加载 / 顶层求值异常 | stderr 打印 V8 异常 + 脚本堆栈（不吞、不改写） | 1 |
| `--log-file` 打开/写入失败 | stderr warn 一次，终端照出，脚本继续 | — |
| 成功 settle | 静默（输出仅来自脚本打印） | 0 |

**明确不做**（YAGNI，评审认可）：

- 执行超时 / KillSwitch：手工 runtime 不过 RuntimePool，同步死循环脚本（
  `while(true){}`）会无限挂起且 tokio timeout 对同步自旋无效——**Ctrl-C 是唯一
  兜底**，进 api-manual 陷阱表。
- quiet/verbose 开关、REPL、watch 模式。
- stdin 脚本（`oj exec -`）：有真实管道场景，记为已知诉求待有用户再议。

## 5. 测试策略

- **单测**（`exec_cmd.rs`）：`--` 参数拆分（`#[arg(last = true)]` 行为）、扩展名
  校验、退出码映射、终端行/JSONL 格式化。
- **进程内集成测**（钉线程真 runtime，最小 config 走 in-memory 兜底）：①
  console/log 到 sink ② **args 注入断言须用非空 argv**（空数组在时序 bug 下也
  通过）③ 顶层 throw → 退出码 1 + 堆栈非空 ④ 相对 import .ts 链（项目根内、
  显式扩展名）可用 ⑤ `--log-file` 打开失败 warn-and-continue ⑥ mail：fake vtable
  + deliver 上送可达（验收显式 install）。
- **装配失败路径**：插件 ABI 不等 → 退出码 1（复用 `tests/plugins/mini` 夹具）。
- **e2e**：`bin/oj exec` 对 stdout 断言（仿 `oj/tests/e2e.rs` 模式）。
- **回归**：`cargo test --release --workspace` 全绿证明拆分无回归；重点对照 §2.2
  验收条件（apply→schema→seed 顺序、prewarm 先于内省、reconcile 仅在 auto）。

## 6. 文档同步（v0.1.29 发版时）

- `CHANGELOG.md`：新「特性」组记录 exec 子命令。
- `docs/devkit/` 四件：`api-manual.md`（exec 章节：CLI、console 仅 exec、
  无 KillSwitch/Ctrl-C 兜底、sql_guard=deny 须 asSystem、迁移默认 off 的不对称）、
  `scenarios.md`（可照抄脚本场景）、`SKILL.md`（陷阱速查：console 分叉、
  显式扩展名、import 项目根钳制、json.* 空转）、`README.md`；`cargo xtask build`
  归置 + xtask devkit 契约用例校验。
- `docs/user-manual.md` / `docs/ops-manual.md` 的 CLI 表补 `exec` 行
  （ops-manual 写明迁移默认 off 的 asymmetry）。

## 7. 评审记录

- 2026-09-28 架构师评审（arch-reviewer）：Backend 收敛 stable 单源（H1）、
  23 步逐步对照表（H2/M5）、reconcile 归属（H3）、租户守卫语义（M1）、ts 钉死
  true（M2）、扩展顺序（M3）、args 注入机制（M4）、console 分叉声明（M6）、
  L1–L4。全部采纳。
- 2026-09-28 工程师评审（eng-reviewer）：args/Sink options 模式时序修复（H1）、
  StableState 迁入 assemble_backend（H2）、reconcile_all 独立化（H3）、
  project_root 对齐 config_dir（H4）、mail deliver 显式安装（M1）、KillSwitch
  文档化（M2）、扩展顺序（M3）、console 格式化约定（M4）、显式扩展名（M5）、
  tracing→stderr 认领（M6）、测试缺口与 clap 接线（L1–L4）。全部采纳。
