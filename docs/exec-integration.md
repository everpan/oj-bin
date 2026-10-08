# oj exec 集成手册（v0.1.29）

`oj exec`（文件 / 内联 / REPL 三种入口，**三选一**，v0.1.50 起后两种可用）：不起 HTTP
服务，直接在一个装配好**完整后端**的运行时里执行 ts/js。面向一次性数据修复、迁移后
对账、批处理导出、临时求值与调试这类「脚本活」——过去这些要么塞进临时 handler 再
curl，要么干脆在框架外写个孤儿脚本。

- 完整后端全局：`json`/`db`/`kv`/`blob`/`bus`/`es`/`fetch`/`ws`/`log`/`plugins`/
  `cert`/`jwt`/`bcrypt`/`crypto`/`oidc`/`ldap`/`mail`/`tasks`/`vars`/`Kafka`/`RabbitMQ`
  （命名 MQ 客户端；无裸 `mq` 全局），与 handler 同源装配。
- 终端 stdout 直出（`console.*` 与 `log.*`），管道友好；`--log-file` 可选 JSONL 双写。
- `--` 之后的 argv 注入 `globalThis.args`；支持项目根内相对导入（显式扩展名）。

快速上手：

```bash
cargo xtask build                                  # 产出 bin/oj
./bin/oj exec scripts/hello.ts -c config.yaml      # console.log 直出 stdout
```

可照抄的完整场景见 `docs/devkit/scenarios.md` 场景 12；本文讲机制与差异。

## 1. CLI

```
oj exec <file> [-c config.yaml] [-d dir] [--db name] [--redis/--blob/--es/--broker/--kafka/--rabbit <profile>] [--log-file path] [-- arg...]
oj exec -e <code> [...]     # 内联代码（v0.1.50）：TS 直执行不落盘；不能含相对 import
oj exec --repl [...]        # 交互式 REPL（v0.1.50）：rustyline 原始终端；变量不跨行持久（挂 globalThis 可跨）
```

| 参数 | 默认值 | 说明 |
|---|---|---|
| `<file>` / `-e` / `--repl` | 三选一必填 | `<file>`：脚本路径，仅 `.ts`/`.js`，其他扩展名报错退出（exit 1），路径任意。`-e, --code`：内联代码（以 `file:///oj-eval.ts` 合成 specifier，TLA 保真）。`--repl`：逐行求值 |
| `-c` | `config.yaml` | 配置文件路径（相对 CWD） |
| `-d` | 自动探测 | schema 白名单来源目录（自 config 同级向上逐级搜，同 `oj test`）；探测不到 → 空 SchemaRegistry + stderr warn 继续（纯 kv/log/fetch 脚本不需要表白名单） |
| `--db` | 无 | 默认库重定向（同 `oj test`）；未声明的库名 fail-fast，不回落 default |
| `--log-file` | 不落盘 | 追加 JSONL（`{"ts":"<RFC3339>","level":"INFO","msg":"…"}`，kv 字段非空时附在行尾）；打开失败仅 stderr warn 一次，终端照出、脚本继续 |
| `-- arg...` | 空 | `--` 之后的 argv 原样注入 `globalThis.args: string[]`（clap `last = true`，不做转义/展开） |

## 2. 执行管线

```
parse args → load_app_config（exec 恒 dev 语义 ts=true——脚本没有 release 形态概念）
  → 钉线程（dedicated OS thread + current_thread runtime；JsRuntime !Send，同 oj test）
    → assemble_backend                              ← 与 server 同一装配函数（§3）
    → extensions = ws_client_extensions()
                 + bridge_ext_init(stable)
                 + exec_ext_init(ExecOptions{args, log_file})   ← 顺序钉死
    → patch_fs_loaded_sources（deno 扩展 JS 换内嵌源码，JsRuntime::new 之前，红线）
    → JsRuntime::new（exec_bootstrap 顶层注入 globalThis.args）
    → ext_boot 预热（config 配了 ext_boot 才跑；exec 不走 RuntimePool，此处补跑）
    → cached_transpile + load_side_es_module_from_code（入口直载，见 §5）
    → mod_evaluate + run_event_loop（顶层 await 跑到 settle）
  → 退出码（§4）
```

两个入口细节值得知道：

- **入口脚本不经 module loader**（`cached_transpile` + `load_side_es_module_from_code`
  直接加载）：loader 的 `looks_cjs` 会把没有 import/export 的脚本误包成 CJS，绞杀
  顶层 await——直载绕开它，脚本内相对 import 仍由 OjModuleLoader 照常解析。
- **`json.*`/`finish`/`http` 空转**：复用 handler 代码不报错，但写入无人读的
  `ReqState`——`json.ok(x)` 不会打印 x，输出一律 `console.log`/`log.*`。

## 3. 与 server 的装配差异（有意为之）

exec 复用 v0.1.29 从 `App::from_config` 拆出的 `assemble_backend`（db/kv/es/blob/bus/
插件/auth 全部同源），但**不构造 HTTP 层**。差异表：

| 维度 | server | `oj exec` |
|---|---|---|
| 证书门禁 | 必配不可绕过 | **不校验**（无 HTTP 面） |
| 迁移（apply/verify/reconcile） | dev 缺省 `auto`，release 缺省 `verify` | **默认全 off**；仅 config 显式 `migrate_on_start: auto\|verify` 才执行对应项 |
| seed / fixtures | 重放 | 不做 |
| 路由 / actor 池 / 静态站点 / WS | 有 | 无 |
| 请求超时 / KillSwitch | RuntimePool + `terminate_execution` | **无**：同步死循环无限挂起，Ctrl-C 是唯一兜底 |
| 租户/归属守卫 | 请求上下文驱动 | 无 HTTP 上下文（租户 id 恒 None）＝匿名系统操作员；`sql_guard: "deny"` 的库上须 `await db.asSystem()` |
| `console` | 无此全局 | **有**（server/test 拷脚本过去是 `ReferenceError`） |
| `json.*`/`finish` | 写回 HTTP 响应 | 空转（无消费方） |
| ext_boot | RuntimePool 每 runtime 预热 | 建完 runtime 补跑一次 |

**迁移默认 off 是与 server dev 相反的缺省**——运维拿 server 直觉套 exec 是本特性
最大的误用面（ops-manual 已同步）。搬进 `src/tasks/` 任务池才是常驻任务的正解
（exec 无 KillSwitch，不适合生产常驻）。

## 4. 输出与退出码

输出通道（`oj/src/exec_ext.rs`）：

- `console.*` 与 `log.*` 都经 `op_exec_log` 到 Rust sink：终端行 `{LEVEL:<5}  {msg}`
  直出 stdout（级别列对齐，msg 内换行原样保留；console 多参 `join(" ")`，字符串裸出）。
- `--log-file` 时同一事件追加一行 JSONL；文件写入失败 warn-once 后放弃落盘，不影响终端。
- 装配期 Rust tracing 日志只走 stderr——**stdout 里只有脚本输出**，管道/重定向干净：

```bash
./bin/oj exec scripts/dump.ts -c config.yaml | jq .        # stdout 纯净可管道
```

| 场景 | 行为 | 退出码 |
|---|---|---|
| config 缺失/解析失败 | stderr 报错 | 1 |
| 后端装配失败（插件 ABI / db / kv / …） | stderr 透传装配段 fail-fast 文案 | 1 |
| 脚本不存在 / 非 .ts/.js | stderr `exec: 仅支持 .ts/.js: <path>` | 1 |
| 脚本加载 / 顶层求值异常 | stderr 打印 V8 异常 + 堆栈（不吞、不改写） | 1 |
| `--log-file` 打开/写入失败 | stderr warn 一次，终端照出，继续 | — |
| 成功 settle | 静默（输出仅来自脚本打印） | 0 |

## 5. 模块导入与 TS

- **复用 Backend 的 loader**（project_root = config 所在目录，与 server 同源）。
- 脚本内 import 被 `ensure_within` 钳制在项目根内：`import "../x"` 上跳即被拒
  （escapes project root）；脚本放项目外（如 `/tmp/foo.ts`）则连 `./util.ts` 都导不了。
- 相对导入**必须带显式扩展名**（`import "./util.ts"`）——loader 的 .ts 探测与
  api 目录 dev/release 判定耦合，省略扩展名会解析失败。
- `.js` 原样执行；`.ts` 走 `transpile.rs`；其他扩展名 fail-fast（入口校验）。
- `-d` 探测到的目录仅作 schema 白名单来源；exec 恒 dev 语义（跑 `.ts`）。

## 6. 典型用法

```bash
# 一次性数据修复（先 --dry-run 对账，再真跑 + JSONL 留痕）
./bin/oj exec scripts/fix-roles.ts -c config.yaml -- --dry-run
./bin/oj exec scripts/fix-roles.ts -c config.yaml --log-file fix.jsonl

# 迁移后对账（产出给 CI / 巡检管道）
./bin/oj exec scripts/reconcile.ts -c config.yaml | jq -c 'select(.bad)'

# 定时任务原型：exec 里验证逻辑 → 搬进 src/tasks/（任务池有超时/停机语义）
```

脚本骨架（完整版见 `docs/devkit/scenarios.md` 场景 12）：

```ts
const dry = args.includes("--dry-run");               // -- 之后的 argv
const rows = await db.query("select id from account where role is null", []);
for (const r of rows) {
  if (!dry) await db.exec("update account set role = ? where id = ?", ["member", r.id]);
}
console.log(`done, ${rows.length} rows`);             // stdout 直出
```

## 7. 陷阱速查

| 症状 | 原因 / 处置 |
|---|---|
| `json.ok(x)` 没输出 | 空转（无 HTTP 消费方）；输出用 `console.log`/`log.*` |
| 脚本拷进 handler 报 `console is not defined` | `console` 仅 exec 运行时提供 |
| `args` 是空数组 / `-- -x` 被 clap 拒 | 透传参数必须在 `--` 之后 |
| import 报 escapes project root | 相对导入钳制项目根内 + 显式扩展名 |
| 脚本卡死不退 | 无超时/KillSwitch，Ctrl-C 兜底；常驻任务搬 `src/tasks/` |
| 数据没写进去 | 迁移默认 off——先 `oj migrate`；或 config 显式 `migrate_on_start` |
| `sql_guard: "deny"` 库查询被拦 | `await db.asSystem()`（exec 视同匿名系统操作员） |
| `--log-file` 没生成 | 打开失败只 warn 一次，看 stderr 首行 |

## 8. 测试覆盖（仓库内）

- 单测/集成：`oj/src/exec_cmd.rs`（6 例——JSONL 双写、非空 argv 注入、相对 import
  链、TLA settle、顶层 throw、装配 fail-fast）+ `oj/src/exec_ext.rs` + `oj/src/args.rs`。
- e2e：`oj/tests/e2e.rs::given_exec_script_when_console_then_stdout_direct_and_exit_codes`
  （子进程断言 stdout 直出、stdout 无 tracing、throw → exit 1 + V8 异常透传）。
- 装配回归：`cargo test --release --workspace`（`assemble_backend` 拆分行为不变证据）。

## 9. 相关文档

- `docs/devkit/api-manual.md` §11「`oj exec`」——开发者视角速查（错误/限制表已同步）
- `docs/devkit/scenarios.md` 场景 12——可照抄的数据修复脚本
- `docs/user-manual.md` §2 / `docs/ops-manual.md`——CLI 行与运维差异（迁移不对称）
- 设计与评审记录：`docs/superpowers/specs/2026-09-28-oj-exec-design.md`
