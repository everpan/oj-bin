# 04 · CLI 与装配（`oj/`）

`lib + bin` 双 target（`oj/src/lib.rs`）：纯 bin crate 的 `pub mod` 对外不可见，
故 `oj/tests/` 需要 `use oj::...` 触达装配层。

## 1. 子命令（`oj/src/args.rs`，clap derive）

| 命令 | 主要参数 | 实现 |
|---|---|---|
| `server` | `-c/--config`、`-b/--base`、`--api-path`、`--app-path`、`--cert-path`、`--key-path`、`--console-log` | `server_cmd.rs:21` |
| `build [module]` | `-d/--dir`（默认 `src`）、`-o/--out`（默认 `dist`）、`--no-minify`、`--check` | `build_cmd.rs:16` |
| `test` | `-c`、`-b`、`-d`、`-t/--tests`（默认 `tests`，相对 config_dir）、`--format`（human/tap/junit/json）、`--output` | `test_cmd.rs:46` |
| `migrate` | `-c`、`-d`、`--baseline`、`--module` | `migrate_cmd.rs:46` |
| `fixture` | `-c`、`-d`、`--module` | `migrate_cmd.rs:77` |
| `schema diff` | `-c`、`-d` | `migrate_cmd.rs:86` |

已删除的旗标有回归测试钉死（`args.rs:371`）：`build -b`、`server --dev`、`server -d/--dir`、
`--grace-days` 等一律 clap 报错；空参打印帮助。

## 2. `App::from_config`（`app.rs:99`）—— 唯一的装配入口

`server` 与 `test` 共用，顺序即语义，出错即 fail-fast：

1. redis 非 default 的键 warn 忽略；
2. **证书必配门禁**（缺任一路径 → Err，无逃生口）；
3. `dir` 绝对化 + `LoaderShared { project_root: config_dir, ts }`；
4. `ext_boot_spec(config_dir)` 探测 `ext_boot.js`，冻结 `?v=<mtime>`（改动须重启进程）；
5. `assemble_plugins` → `PluginInfo[]`（同时喂 `StableState.plugins` 与 `GET {base}/plugins`）；
6. KV：声明 `redis.default` → 经 kv 插件 vtable connect；未声明 → `InMemoryKV`；
7. ES / blob：`es` 取插件单后端；`blob` 逐后端装配，下载路由只服务 default；
8. `connect_dbs` 逐库开库（未知 scheme → fail fast）；
9. **迁移门禁**：`auto`（apply）/ `verify`（校验，账本落后或待应用 → 拒启）/ `off`；
   缺省 dev=auto、release=verify；
10. 归属守卫模式 `warn`/`deny`；
11. **归属图 + SchemaRegistry 复活**：`manifest::discover` → `schema.yaml` → `registry_tables`
    （S002 同表双声明 → Err）+ `ModuleCtx` map（键 = 模块目录绝对路径）；
    `gate == "auto"` 时逐模块 `schema::reconcile`（安全前向 DDL）；
12. 种子重放 `seed::replay_all`（各模块 `seed.sql`，三方言，语句级 tracing 日志）；
13. `fixtures=true`（仅 `oj test`）灌 `fixtures/*.sql`；
14. 鉴权守卫来自 oj-auth 插件；`jwt` / `oidc` 原语配置注入 `Extras`；
15. `bus`：`registries.bus.connect(&cfg.broker)`；
16. `make_bridge` 闭包（内省 / actor 池 / WS 共享同一组 Arc）；
17. `prewarm_boot`（`boot.is_some()` 时）—— 把 boot 错误前移到装配期，**必须显式调用**；
18. 路由表：dev 逐文件内省 `.route`；release 读锁 + 各模块 `routes.js`（`from_entries`）；
19. `JsActor::pool(pool_size, make_bridge)`；
20. 静态根绝对化（缺失 → fail fast）；
21. 证书加载校验 + `spawn_watcher` 热加载；
22. `Pipeline` 组装 + `ws::mirror_routes` + `server::app(...).merge(ws_router)`；
23. 构造**唯一** `StableState`（与 actor 共享后端 Arc，供测试运行时注入）。

### 私有步骤函数（2026-09-06 拆分）

`from_config` 主干已从 ~400 行降到 294 行；其中自成一体的步骤抽为同文件私有函数：

| 步骤 | 函数 |
|---|---|
| 6 KV 装配 | `connect_kv(&cfg, &registries)` |
| 10 归属守卫模式 | `ownership_deny_of(&cfg)` |
| 11 归属图 + SchemaRegistry 复活 | `build_schema_and_modules(&dir, ts, &dbs, gate)` |
| 14 jwt / oidc 原语配置 | `build_jwt_and_oidc(&cfg, config_dir)` |
| 20 静态站点根 | `resolve_static_root(&cfg, config_dir)` |
| 21 证书加载 + watcher | `load_cert_with_watcher(&cfg, config_dir)` |

路由表（18）与 `make_bridge` 闭包（16）因跨步骤共享变量多，仍在函数体内。

### `ClientTransport`（`app.rs`）

`#[async_trait]` trait（`Send + Sync + 'static`），`App` 实现之。
`dispatch` = `Router::oneshot`（**零 TCP**，对标 Go Fiber `app.Test`），外层
`DISPATCH_TIMEOUT = 60s` 兜底 → 408；`base()` 供 op 拼路径。

## 3. `server_cmd.rs` 其余职责

- `load_app_config`（:61）：config 解析 + 目录/模式判定 + base 归源，`server` 与 `test` 共用。
  目录缺失不在此拦截：server 准入由 `run()` 裁定，migrate/fixture/test 强依赖 api
  目录、各自就地报错。
- `admission_gate`：server 准入门三态 —— api（`--api-path`）与静态（`server.app_path` /
  `--app-path`）至少显式指定其一（皆未指定 → Err）；皆指定 → 两者都必须存在；
  仅指定其一 → 只启用对应功能（api 缺席 = 纯静态，占位缺失目录使模块扫描为空）。
- `absolutize_cwd`：CLI `--app-path` 相对 CWD 绝对化（config 值相对 config_dir，
  由 `resolve_static_root` 处理）。
- `is_release(dir)`（:105）：目录含 `manifests.yaml` → release。
- `assemble_blobs`（:144）：逐后端构造；`driver != local` 且无 blob 插件 → fail fast。
- `connect_dbs`（:188）：经注册表按 scheme 认领，错误带库名。
- `plugin_cfg`（:232）：cfg 三级回落 —— `plugins.<name>` 非空对象 → 轴适配器 → `{}`。
- `build_registries`（:264）：插件后端先于内置；es 键选单后端、db 认领式、blob 键选单槽、
  bus 键选注册表、kv 键选单槽；冲突/缺失必需插件 → fail fast。
- `assemble_plugins`（:340）：解析 plugins_dir → 严格清单/缺省扫描 → 逐个加载校验。

## 4. `build_cmd.rs`（src → dist）

- 版本视图 = `dist/manifests.yaml` 锁 ∪ 本次计划构建的模块版本；
  **版本目录不单射 → fail-fast**（`{m}-{v}` 碰撞）。
- `checks::run`（S002–S007）**构建即检查**；`--check` 只校验不落盘（CI 门禁）。
- 单模块：清场同名版本目录 → 转译落盘 → 内省产 `routes.js` → lock upsert → `.tgz`。
- 转译处理三件事：
  - `strip_route_decls`：剥掉产物里的 `fn.route = "...";` 整行；
  - `fix_relative_imports`：模块内相对路径重算、跨模块指向 `dist/<m_t>-<v_t>/`；
  - 默认 minify（`--no-minify` 得多行可读产物，排障逃生门）。
- `guard_no_api_imports`：`api.ts` 只许作路由入口，被 import 即拒绝。
- `rel_pattern`：相对 pattern 含模块名段；根级声明（`/` 开头）剥首斜杠不加模块段。

## 5. `test_cmd.rs` + `test_ext.rs`（进程内测试运行器）

- 钉线程模型：专用 OS 线程 + `current_thread().enable_all()`（`JsRuntime` 是 `!Send`）。
- extensions = [`bridge_ext`（真实全局 + `StableState`）, `oj_test_ext`（`client` + 迷你框架）]；
  `OpState` 注入 `Arc<dyn ClientTransport>`。
- 收集 `*.test.ts`（按名排序）→ 逐个以 side module 加载（注册用例）→ `__runTests()`
  → 读 `globalThis.__testSummaryJson` → 反序列化 → 报告 → 退出码（不在运行时内 exit）。
- `op_client_dispatch`（`test_ext.rs:53`）：先 clone `Arc` 再 drop 借位（**禁止持 Ref 跨 await**），
  每次请求重置 `ReqState`（防串号），101 upgrade 不 `to_bytes`。
- 报告格式 `human` / `tap` / `junit` / `json`，`--output` 落盘（机器格式 stdout 纯净）。
- `oj test` 不走 `RuntimePool`，故在 `test_cmd.rs:125` 单独补跑一次 ext_boot，
  否则与生产行为分叉。

## 6. 瘦身装配（`migrate_cmd.rs`）

`oj migrate` / `oj fixture` / `oj schema diff` **不走 `App::from_config`** ——
后者证书门禁无逃生口且携带 seed/路由。瘦身路径只解析 config → 插件 → 开库 → 执行，
使 CI/运维机无证书也能迁移。

## 7. Rust 集成测试（`oj/tests/`）

- `e2e.rs`：UC1–UC15（方法表、CRUD+params+body、嵌套路由、release 模式、kv 读穿、
  imports/裸 specifier、转译缓存与热重载、manifest 不匹配拒启、build→release 全链、
  routes.js 缺失 fail fast、404/405/穿越、编译错误信封、超时 408 服务存活）。
  全部 `#[tokio::test(flavor = "multi_thread")]`，端口 0 + 全局 `lock()` 串行化。
- `oidc_e2e.rs`：独立测试目标 —— **oj-auth 插件进程级 GUARD 只认首次 init，
  共进程会互染**（见 commit `26dbbaa`）。

## 8. 已知债

- `App::from_config` 主干 294 行 + 6 个私有步骤函数（2026-09-06 拆分后）；
  路由表构建与 `make_bridge` 闭包仍内联，若再拆需先收拢共享变量。
- `server_cmd::serve` / `serve_with_listener` 与 `App::serve` 两套入口并存。
- `oj test` 的 `describe/it/expect` 是自研迷你框架，匹配器只有
  `toBe/toEqual/toBeTruthy/toBeFalsy/toContain` 五个（见 [08-testing.md](08-testing.md)）。
