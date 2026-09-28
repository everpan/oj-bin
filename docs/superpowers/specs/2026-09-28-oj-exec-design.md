# oj exec 子命令设计（v0.1.29）

日期：2026-09-28 ｜ 状态：待评审 ｜ 版本目标：v0.1.29

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
5. **支持相对导入**：脚本可 import 同目录（及子目录）的其他 .ts/.js。

## 2. 架构：拆分 App::from_config

`oj/src/app.rs` 的 23 步装配拆为两段：

### 2.1 `assemble_backend`（新增，后端核心）

```rust
async fn assemble_backend(
    cfg: &Config,
    config_dir: &Path,
    dir: &Path,          // api 目录（schema 白名单来源）
    ts: bool,
    db_override: Option<&str>,
) -> Result<Backend, String>
```

`Backend` 持有：`registries`、`kv`、`es`、`blobs`/`blob`、`dbs`、`registry`
（SchemaRegistry）、`modules`、`auth`（jwt 配置）、`jwt`/`oidc`、`bus`、`mail`、
`ldap`、`kafkas`/`rabbits`、`vars`、`plugin_infos`、`loader`、`boot`、`db_override`、
`query_limits`、`ownership_deny`/`sql_guard`，以及 `make_bridge_of` 工厂
（`Backend::make_bridge(tasks_flag)`）。

从 `from_config` 迁入的步骤：redis warn、loader 构建、ext_boot spec、插件装配、
kv/es/blob、db 连接、db_override 校验、query_limits 校验、ownership/sql_guard、
schema+modules、auth/jwt、bus、mail、ldap、mq、tasks_flag、vars、make_bridge_of。

### 2.2 HTTP 层（留在 `App::from_config`）

证书必配门禁、`migrate_on_start` 门禁、seed/fixtures、路由表（dev/release 两路）、
actor 池、静态站点、证书加载+watcher、Pipeline、WS 选项、Router 构造。
这些消费 `Backend` 的字段，行为对外完全不变。

### 2.3 exec 侧新组件

| 组件 | 职责 |
|---|---|
| `oj/src/args.rs` `ExecArgs` | `<file> [-c config] [-d dir] [--db name] [--log-file path] [-- arg...]` |
| `oj/src/exec_cmd.rs` | 入口编排：解析 config → 钉线程起 runtime → `assemble_backend` → 执行脚本 → 退出码 |
| `oj/src/exec_ext.rs` + `exec_bootstrap.js` | deno extension：定义 `console.*`；把 `log.*` 改挂 `op_exec_log`（覆盖 bridge 默认的 tracing 通道） |
| `op_exec_log` | Rust sink：人类可读行 → stdout；`--log-file` 时双写 JSONL |

`exec_bootstrap.js` 必须 7-bit ASCII（CLAUDE.md 红线，同 `bootstrap.js`）。

## 3. exec 语义细则

### 3.1 与 server 的装配差异（`assemble_backend` 不含）

- **无证书门禁**：exec 不构造 HTTP 层，证书路径不校验。
- **迁移门禁默认 off**：`migrate_on_start` 缺省时 exec 不执行迁移/校验（server
  的 dev-auto 语义不适合短脚本）；仅当 config 显式写 `auto|verify` 才执行。
  具体落法（消除二义性）：`build_schema_and_modules` 迁入 backend 层但**剥离**
  迁移副作用（纯 schema 白名单 + 模块元数据）；`migrate::apply_all / verify_all`
  保持独立调用，由调用方决定——server 按 `migrate_on_start`（缺省 dev=auto /
  release=verify）调用，exec 仅在 config 显式配置时调用，否则跳过。
- **无 seed/fixtures**（server/test 专属）。
- **无路由表、actor 池、静态站点、WS**。

### 3.2 执行流程

```
parse args → load_app_config(config, dir, base=None)
  → 钉线程（dedicated OS thread + current_thread runtime，同 oj test 钉法——JsRuntime !Send）
    → Backend = assemble_backend(...)              // 失败 → stderr + 退出码 1
    → JsRuntime::new([bridge_ext, ws_client_extensions, exec_ext])
    → patch_fs_loaded_sources(...)                 // 扩展 JS 内嵌补丁（红线）
    → OpState 注入 ExecSink { log_file: Option<File> }
    → ext_boot 预热（config 配了才跑，与 test 同路径）
    → globalThis.args = [...]                      // exec_bootstrap 注入，"--" 后 argv
    → specifier = 脚本绝对路径 file://
    → load_side_es_module + mod_evaluate + run_event_loop  // 顶层 await 跑到 settle
  → 退出码
```

### 3.3 输出通道

- `console.log/info/warn/error/debug` 与 `log.*` 都经 `op_exec_log` 到 Rust sink。
- 终端格式：`{LEVEL:5}  {msg}`（级别列对齐，单行，msg 内换行原样保留）。
- `--log-file f.jsonl`：同事件追加一行 `{"ts":"<RFC3339>","level":"INFO","msg":"..."}`
  （与 server 落盘字段同形，便于同一套日志工具消费）。文件打开失败 → stderr warn
  一次，终端输出不受影响，脚本继续。
- server 的 `log` → tracing 通道语义不受影响（exec_ext 只在自己的 runtime 里覆盖
  全局）。

### 3.4 模块导入与 TS

- 相对/绝对文件 import 复用 `OjModuleLoader`（`LoaderShared.project_root` = 脚本
  所在目录），TS 按需转译 + `?v=<mtime>` 缓存失效，与 server dev 一致。
- `.js` 原样执行；`.ts` 走 `transpile.rs`；其他扩展名 fail-fast。
- `-d dir` 缺省自动探测（同 `oj test` 的向上逐级搜）；探测不到 → 空
  SchemaRegistry + stderr warn 继续（纯 kv/log/fetch 脚本不需要表白名单）。

### 3.5 脚本参数

`--` 之后的 argv 原样（不做转义/展开）注入 `globalThis.args: string[]`；
无 `--` 时为空数组。

## 4. 错误处理与退出码

| 场景 | 行为 | 退出码 |
|---|---|---|
| config 缺失/解析失败 | stderr 报错（`load_app_config` 文案） | 1 |
| 后端装配失败（插件 ABI / db / kv / ...） | stderr 透传装配段 fail-fast 文案 | 1 |
| 脚本不存在 / 非 .ts/.js | stderr `exec: 仅支持 .ts/.js: <path>` | 1 |
| 脚本加载 / 顶层求值异常 | stderr 打印 V8 异常 + 脚本堆栈（不吞、不改写） | 1 |
| `--log-file` 打开/写入失败 | stderr warn 一次，终端照出，脚本继续 | — |
| 成功 settle | 静默（输出仅来自脚本打印） | 0 |

明确不做（YAGNI）：执行超时、quiet/verbose 开关、REPL、stdin 脚本、watch 模式。

## 5. 测试策略

- **单测**（`exec_cmd.rs`）：`--` 参数拆分、扩展名校验、退出码映射、终端行/JSONL
  格式化。
- **进程内集成测**：临时脚本跑真 runtime（钉线程形态），断言 ① console/log 到 sink
  ② `args` 注入 ③ 顶层 throw → 退出码 1 + 堆栈非空 ④ 相对 import .ts 链可用。
  最小 config（无 redis/db 段）走 in-memory 兜底，不接外部服务。
- **e2e**：`bin/oj exec` 对 stdout 断言（仿 `oj/tests/e2e.rs` 模式）。
- **回归**：`cargo test --release --workspace` 全绿证明拆分无回归。

## 6. 文档同步（v0.1.29 发版时）

- `CHANGELOG.md`：新「特性」组记录 exec 子命令。
- `docs/devkit/` 四件：`api-manual.md`（exec 章节 + CLI 表）、`scenarios.md`
  （可照抄脚本场景）、`SKILL.md`（`--` 传参、log-file 双写陷阱）、
  `README.md`；`cargo xtask build` 归置 + xtask devkit 契约用例校验。
- `docs/user-manual.md` / `docs/ops-manual.md` 的 CLI 表补 `exec` 行。
