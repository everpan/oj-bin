# U7 设计：`oj migrate --with-seed`（安装时同批执行 seed）（2026-11-07）

> 来源：`oj-module/docs/migration/oj-requirement-seed-replay-on-install.md`（2026-11-07 起草，
> 状态「待上游裁决」），关联 `oj-module/docs/prd/framework-verification-playbook.md` §11 **U7**、
> §7.7 drill3（2026-11-07 实锤）。
> 本文件是 oj-bin 侧的裁决与实施设计。**本仓只改 oj-bin；oj-module 全程只读**，其待办见 §10。
> 前置约定不改：seed 幂等由 `oj check` S006 静态门禁约束（`oj/src/checks.rs:233-272`）；
> 方言件选择由 oj `0.1.121` 落地（U1，`oj/src/seed.rs:113-141`）。
>
> **裁决记录（2026-11-07，用户）**：草案给的两选一（A 新增 `oj seed` 子命令 / B reload 携带
> 清单）均不采用；改为 **复用 `oj migrate` 子命令 + `--with-seed` 旗标**，一次调用把迁移与
> seed 一起做完。理由：改动面更小（不新增 `Command` 变体、不动 `main.rs`），且下游不必
> 新增 stage / svcDir / retry 段。见 §4。
>
> **四路评审（专家工程师 / 架构师 / 产品 / 安全工程师）已于 2026-11-07 并行完成**，逐条裁定
> 见 §12。评审推翻了首版的运行期门禁设计与「下游零成本」措辞，本文档为修订后终稿。

## 0. 措辞裁定、执行顺序实锤与交付边界

用户原话「seed.sql 优先于 migrate，两者一起执行」经确认裁定为：

- **「优先」= 本次以补齐 seed 重放能力为优先项、把 seed 补进 migrate 这条命令，不是执行
  顺序反转。**
- **执行顺序实锤（用户提供，已在代码复核）**：
  1. 冷启动（`oj serve` / `ojm dev` / `ojm preview`）：`oj/src/app.rs:1983` 注释明写
     「迁移门禁（§4.6，先于 seed）」——migrate gate（dev=auto apply / release=verify）
     执行完，才进 `seed::replay_all`（`app.rs:2038`）。设计理由：seed 的 INSERT 目标表
     必须已由迁移建好。
  2. 安装路径（`ojm install`）：migrating 段显式跑 `oj migrate`（随装生效）；seed 不随装
     跑，要等下次冷启动重放——**顺序仍对，但两者被一次重启隔开**，这就是 U7 的缺口。
- 结论：**`--with-seed` 严格沿用 migrate → seed 顺序**，只是把被重启隔开的两步收回同一条
  命令、同一次进程、同一个目标库连接。
- 关键推论（必须钉死）：**seed 失败要让整条命令非零退出**。安装器
  `parseMigrateOutput`（`_lifecycle/steps/migrate.ts`）以 **exit code 为成败真源**
  （`code !== 0` 即失败）。若 seed 失败而命令返回 0，安装器判 done、seed 被静默吞掉，
  U7 缺口以另一种形式复现。

### 交付边界（评审 A-① / P-B 裁定，必须如实标注）

**本轮只交付 oj-bin 侧能力。** oj-module 全程只读（CODEBUDDY.md 硬规则），安装器
`buildMigrateCmd` 不会带 `--with-seed`，因此 **U7 的端到端闭环本轮不成立**：
`ojm install` 后数据面仍为空，须待 oj-module 采纳 §10 待办后才闭环。
本轮以 **oj-bin 侧 e2e**（真 sqlite 库 + `oj migrate --with-seed`）等价验证
「同一次调用、不重启、即得种子行」，§9 验收 #1/#6 标为「下游门禁 / 待集成」。
不得把本轮交付描述为「安装后数据就绪」。

## 1. 动机

books 方案（带 `seed.sql`，3 行）经市场按名安装后，安装器两段式终态 done、
`state.yaml enabled: true`，但方案页数据面为空：

1. migrating 段显式执行 `oj migrate --format json -d <svcDir> <module>`（`_lifecycle/steps/migrate.ts`
   的 `buildMigrateCmd`），`_oj_migrations` 即时写入，新表立即可查；
2. seed 不灌：起服路径的 seed 重放被 `!reused` 门控挡死（`oj/src/app.rs:2038-2043`）；
   安装 API 触发的 `do_reload` 传 `Some(ctx.backend.clone())`（`oj/src/serve_cmd.rs:534`）
   → `reused=true` → 整体跳过；必须冷启动（`ojm preview` 重启）才见
   `seed ok module="books-0.1.4"` 与 3 行数据；
3. 期间 `/api/books/list` 返回 0 行，HTTP 全绿、安装 done，只有「下次重启数据突然出现」
   这一怪象，排查成本高。

`!reused` 的本意「热更不重灌既有模块种子」**纪律正确**，问题在于没给「新装模块」留例外。

## 2. 对上游草案的复核与订正（基线：oj v0.1.124）

| 草案说法 | 复核结论 |
|---|---|
| `app.rs:2037` 起服 seed 被 `!reused` 门控 | ✅ 成立，实际 `oj/src/app.rs:2038-2043`；`reused=true` 来源实证 `oj/src/serve_cmd.rs:534` |
| 「缺的只是按模块、按时刻的触发面」 | ✅ 方向对，但落地形态由「新增子命令」改为「复用 migrate + 旗标」（§4） |
| `oj/src/seed.rs:190 collect()` 支持按模块过滤即可复用 | ⚠️ 旗标方案**绕开**这个陷阱：`run_migrate` 手里已有 `s.modules`（`manifest::discover` + `--module` 过滤后的 `(name, 模块根)`，`oj/src/migrate_cmd.rs:67-94`），seed 直接吃这批模块，不需要 `seed::collect(dir,index)` 再找一遍 |
| 「幂等前提不变，风险面零新增」 | ⚠️ **最大风险点**。S006 是**构建期静态检查**（`oj/src/checks.rs:233-272`），`replay_all` **运行期不校验幂等**（`oj/src/seed.rs:443-466` 直接 `exec_with_params`）。`--with-seed` 的执行对象可以是安装器刚解包的**第三方 `.tgz` 内的 seed 文件**，未必过 `oj check`。评审升级为 fail-closed 门禁，见 §6 |
| 「一个或多个模块；缺省 = 全部（与 migrate 一致）」 | ❌ `MigrateArgs.module` 是单个 `Option<String>`（`oj/src/args.rs:158`/`:404`），`buildMigrateCmd` 也只传一个 module。`--with-seed` 沿用 migrate 既有单模块语义，不改参数形状。**注意**：不带 `--module` 时 `s.modules` 是全量（§5 组合语义） |
| 「seed 执行机制无需任何改动」 | ⚠️ 底层机制无需改，但 `replay_all` 还含跨模块 **S002 冲突检查**（`oj/src/seed.rs:426-441`）与 skipped/misnamed 告警（`:371-417`）；新入口要显式定义这些语义 |
| §4.3 安装器 seeding 段、§6 临时绕行 | 属 oj-module，且**旗标方案下大部分不再需要**（§10），本轮只读登记 |

## 3. 语义基石（协商裁决）

1. **问题的本质不是缺触发面，是「两步被重启隔开」**：migrate 与 seed 顺序本来正确，
   只是 migrate 在安装器显式驱动下随装生效，seed 却挂在 boot 期隐式副作用上。
   修复方向 = **把 seed 收回 migrate 的同一次调用**，而不是新增一条调用链或让框架记账。
2. **一次性初始化的语义应绑定「模块首次进入该库」，而非「进程首次启动」**（草案 §2 末段，
   采纳）。落地形态选「把触发权交给调用方」（`--with-seed`），不是让框架自己记账（§4 C）。
3. **冷启动纪律不变**：`app.rs:2038` 的 `if !reused` 与 `replay_all` 逐字不变；
   `--with-seed` 是 `oj migrate` 的 opt-in 额外能力，热重载后旧模块种子零重灌。
4. **seed 幂等靠作者 + 静态门禁，不靠框架改写**：不改写、不跳过 S006 约束过的语句。
   CLI 路径额外加一道**更严的**运行期门禁（§6），因为信任边界不同——机制一致、准入集更严。
5. **不引入 seed 账本**：现有「每次冷启动全量幂等重放」是 `oj test` / 作者改 `seed.sql`
   冷启即生效 / S006 判据的前提，改成「每模块每库仅一次」会破坏这一整套。
6. **两者一起执行、任一失败整体失败**：migrate 与 seed 在同一条命令内顺序执行；
   任一环节失败 → 非零退出 + 脱敏错误，不允许「迁移成功但 seed 悄悄没灌」。

## 4. 方案空间与裁决

### A' · 复用 `oj migrate` + `--with-seed` 旗标（**采纳**，用户裁定）

`oj migrate --with-seed --format json -d <dir> <module>`：先跑既有迁移逻辑，再对
**同一批 `s.modules`** 重放 seed，共用同一个 `s.target` 库连接。

- **真正省的只在 oj-bin**（评审 A-① / P-B 收敛）：`args.rs` 只加一个 `bool` 旗标 +
  `MigrateArgs` 字段 + `to_command` 透传；不新增 `Commands` 变体、不新增 `Command`
  枚举值、**`main.rs` 完全不动**。
- **下游不必新增 stage / svcDir / retry 段**（评审 P-B 实测成立）：`installer.ts:459-471`
  的 STAGES 无 seeding 段；`migrate.ts` 的 `finally` 清场 `<staging>-migrate` 发生在
  `oj migrate` 进程**退出之后**，seed 与 migrate 同进程读文件，天然避开独立 svcDir 时序；
  migrate 段既有的 R14 备份 / rollbackGuard / 两段式 retry / exit code 成败真源自动覆盖 seed。
- **但白名单维度并不更省**（评审 A-① / P-B 纠正首版措辞）：下游
  `templates/config.yaml:132` 的 `&INSTALLER_EXEC_ALLOW` 锚是**逐 token 逐字相等**模板
  （`api-manual.md:1399-1402`，token 数不齐即 `exec template mismatch` fail-closed）。
  加一个旗标 = 新增一条 8-token 模板，与新增子命令同样要加一条白名单；且它是
  **消费方项目级配置**，每个已部署站点都要改（§10）。故「下游零成本 / 迁移成本大幅
  下降」措辞收敛为「下游不必新增编排段，但仍需同步 argv 模板与消费方配置」。
- 与 §0 执行顺序实锤天然一致；不触碰 v0.1.122~124 连续加固的 reload 路径。

### A · 新增 `oj seed <module>` 独立子命令（**不采纳**，草案原推荐）

曾作为首版设计（本文件 v1）。用户裁定「为 migrate 增加个旗标参数更简单」，不采纳理由：

- 需新增 `SeedArgs` / `Commands::Seed` / `Command::Seed` / `to_command` 分支 /
  `main.rs` 派发 / `args.rs` 头注释，样板代码翻倍；
- 下游要新增 stage（10→11）、新 argv 白名单模板、**独立 svcDir 时序**（migrate 的
  `finally` 会删 `<staging>-migrate`，seeding 段需另物化或调整清场）、retry 覆盖新段、
  两段式终态语义扩写；
- 「迁移 + seed 同属安装的 schema 初始化段」（草案 §1）用**同一条命令**表达更贴切。
- 保留价值：`oj seed` 作为「不跑迁移、只补灌」的运维原语。**登记待做**，非永久否决；
  若后续需要，用已抽出的 `replay_modules` 实现，**勿再加 `--seed-only` 到 migrate**
  （评审 A-SRP：避免 migrate 沦为模式分发器）。

### B · reload 编排携带「新装模块清单」重放（**不采纳**）

零下游改动（安装器本来就会 reload，且 `do_reload` 已算出 `lock_diff` 的 `+name@ver`，
`oj/src/serve_cmd.rs:513-524`）。评审 A 实测确认否决充分：`+name@ver` 只进
`ReloadOutcome.applied`、未接任何 DB 写；`reused` 路径强制 verify-only（`app.rs:1986`）
+ `!reused` 灌库门（`:2038`），在 HTTP reload 内跑 seed INSERT 直接破「热更不写库」红线
（与 v0.1.122~124 加固真冲突），且 reload 无 exit-code 真源。「零下游改动」虽真，但把
触发权收回框架、违 §3 基石。

### C · `_oj_seeds` 账本（按模块+版本记账，首装即灌）（**不采纳**）

真正把语义绑到「模块首次进库」。评审 A 实测确认否决成立：S006 文案「seed 随启动重放」
（`checks.rs:254`）、fixtures 仅 `oj test`（`app.rs:2044-2050`）、`load_fixtures`「不记账本」
（`migrate_cmd.rs:373-375`）——账本破 `oj test` 每轮重放 + 作者改 seed 冷启生效整套前提。
引入框架内部表 + 账本迁移 + 升降级语义，blast radius 远大于本需求。**登记待做**。

## 5. 需求（oj-bin 侧）

| 编号 | 需求 |
|---|---|
| FR1 | `oj migrate` 新增 `--with-seed` 旗标（`bool`，缺省 `false`），不新增子命令、不改 `Command` 枚举、不动 `main.rs` |
| FR2 | 带旗标时，在 `run_migrate` 的**迁移逻辑完成后**，对**同一批 `s.modules`** 重放 seed；共用 `s.target`，不另开库、不另解析 config |
| FR3 | **机制一致、准入集更严**（评审 E-nit 措辞订正）：方言件胜出/通用件兜底、绝不双跑、`translate_insert` 改写、`;` 朴素切分、`seed ok` 日志、`Dialect::known` 未知方言 fail-loud 不回落 Sqlite（`oj/src/seed.rs:363-369`）与冷启动同源；运行期门禁另按 §6 更严 |
| FR4 | 模块无 seed → 成功 no-op、零日志噪音（`--format json` 仍输出完整一行） |
| FR5 | 失败语义：`--format json` stdout 恰一行；**seed 失败 → 整条命令非零退出** + stderr 人话 + `sanitize_dsn` 脱敏。**stderr 人话须把脱敏后的具体原因（点名文件/语句/下一步）放在最前且简洁**（评审 P-A：下游失败路径只取 `(stderr||stdout).slice(0,500)` 头部）；不带旗标时 JSON 形状逐字不变 |
| FR6 | 冷启动纪律不变：`app.rs:2038` 的 `!reused` 与 `replay_all` 保持，热重载旧模块零重灌 |
| FR7 | **运行期 fail-closed 门禁**（§6，评审 S-blocker 升级）：只放行幂等 INSERT，其余语句类型一律拒；首词用 `code_tokens` 取，修 `lead_word` 根因；按**目标库方言**判幂等；全量预扫零副作用 |
| FR8 | JSON 契约不污染 `results`：seed 结果放**顶层新字段 `seed`**（逐模块 `results[]`，评审 E-B2 裁定）；下游 `parseMigrateOutput` 只取 `raw.results`/`raw.error`、忽略未知顶层键 → **成功路径**零改动兼容；顶层 `error` 仍专属 migrate 装配期全局失败 |
| FR9 | 版本 `0.1.124 → 0.1.125` + CHANGELOG + devkit 四件 + `docs/modules/04-oj-cli.md` 同步 |
| FR10 | **门禁外置为前置纯函数** `pre_validate_seeds(modules, dialect)`（评审 A-major）：`replay_modules` 不带 policy 参数、纯执行；消 boolean blindness，保住 `replay_all` 逐字不变 |
| FR11 | **门禁全量预扫再执行**（评审 E-B3）：收齐全部 `(name, file, text)` → 统一门禁 + S002 → 才进 exec；`replay_all` 无事务、逐句 `exec_with_params`（`seed.rs:447`），不允许「前模块已提交、后模块才命中 DDL」 |

### 组合语义（评审后钉死，不再「待裁决」）

- **`--with-seed` + `--baseline`**：**互斥**（clap `conflicts_with = "baseline"`，参照
  `args.rs:395`）。baseline 是「存量库只记账不执行迁移」，表可能未建、且存量库已有数据。
  存量库想灌种子须分两步（文档写明）。
- **migrate 有模块失败**：**不跑 seed**，整条命令失败；顶层 `seed` 字段**不出现**
  （`skip_serializing_if = "Option::is_none"`），对齐 `app.rs` 迁移门禁先于 seed。
- **不带 `--module`**：`s.modules` 为全量（`migrate_cmd.rs:67`），`--with-seed` 会对
  **全部模块**灌种。文档须写明，运维/CI 直跑场景建议带 `--module`。
- **seed 结果形状**：逐模块 continue-and-collect（与 `migrate_collect` 同形），
  `seed.results[]` 每项 `{module, ok, file?, dialect?, stmts?, error?}`；
  `replay_modules` 返回 `Vec<SeedResult>` **不是**单点 `Result<_, String>`（评审 E-B2）。
- **方言判定**：`replay_modules` 内部由 `s.target.dialect_info()` 解析（评审 E-B6，不复制
  两份 `Dialect::known` 门控）；门禁按**目标库方言** `Some(d)` 判幂等（评审 S-major 裁定，
  见 §6 不采纳项 N3）。

### 非目标

- 不改 oj-module 任何文件（只读）；不改 reload 协议；不改 S006 静态规则的准入口径
  （`lead_word` 根因修复除外，见 §6）；不引入 seed 账本；不改 `oj migrate` 既有参数形状；
- 不新增子命令 / JS 全局 / op；不给 seed 加 SQL 白名单旁路；
- 不把 `--with-seed` 做成默认行为（opt-in，缺省行为逐字不变）；
- **不合并 `oj test fixture`**（评审 A 背书）：fixtures 无方言选件/translate_insert/S002/
  告警、信任边界与结果形状都不同，共享层仅切句+exec+幂等谓词（谓词已共享于
  `checks.rs:424/445`），通用引擎反成泄漏抽象。

## 6. 信任模型与安全（评审重点，已按 S-blocker 重写）

`--with-seed` 的执行输入可以是安装器从市场 `.tgz` 解包出来的 `seed*.sql`，
**不可信输入**。现行 `replay_all` 只做方言门控 + S002 + `translate_insert`，然后
`exec_with_params` 原样下发（`src/bridge/accessor_sqlx.rs:502/507` 的 `AssertSqlSafe`
对裸 SQL 无白名单）；幂等与非 DDL 依赖**构建期** S006。

### 定位（评审 S 显式改口，采纳）

门禁 = **opt-in 新入口的 fail-closed 纵深防御**，不是对抗已签名恶意包的完整沙箱。
`verify.ts:146-221` 的 Ed25519 + sha256 + registry_id + TOFU 只绑 key_id、不证 seed
内容安全，故门禁不可省；但单 DSN 全库权限、写他模块表、按模块库隔离等属**剩余风险**，
登记待做（§12）。

### 门禁规则（必须修，评审 S-blocker1/2）

1. **fail-closed 语句类型白名单**：只放行 **幂等 INSERT**（`INSERT OR IGNORE` /
   `OR REPLACE` / `ON CONFLICT` / `ON DUPLICATE KEY` / `INSERT IGNORE`，按方言收敛）；
   其余首词（`CREATE`/`ALTER`/`DROP`/`TRUNCATE`/`RENAME`/`GRANT`/`COMMENT`/
   `UPDATE`/`DELETE`/`REPLACE`/`PRAGMA`/`ATTACH`/`VACUUM`/`SET`/`CALL`/`COPY`/
   `BEGIN`/事务控制/函数/`INSERT … SELECT`）**一律拒**，点名文件与语句、给下一步。
   S006 静态检查只拦 7 个 DDL 词 + 非幂等 INSERT（`checks.rs:242-258`），运行期门禁
   **必须更严**（因新入口 opt-in，收紧不影响既有工程 → 化解「零回归」矛盾）。
2. **首词用 `code_tokens(stmt)[0]`**（`checks.rs:474`，已正确剥 `/* */`/`--`/字符串），
   **不用 `lead_word`**（`checks.rs:424-437` 只跳行首 `--`/`/*`，`/*x*/ DROP TABLE t`
   整行被略过 → 双绕过）。`code_tokens` 需由私有改 `pub(crate)` 或新增等价
   `pub(crate) fn stmt_head`。
3. **修 `lead_word` 根因**（采纳，根因优先）：让 S006 + `load_fixtures` + 新门禁同源受益；
   加对抗性用例钉死 `/*x*/ DROP`、`--` 前置注释等击穿尝试。不改其准入口径，只修注释判定。
4. **按目标库方言 `Some(d)` 判幂等** `insert_is_idempotent_in(stmt, Some(d))`
   （`checks.rs:454`）：新入口只在这台库上跑、opt-in、fail-closed，按目标方言能提前拒掉
   通用件里的方言专属形态（否则 mysql/pg 跑出 1064）。冷启动 S006 **保持按文件方言**
   `seed_dialect_of`（`checks.rs:236-237`）不变，通用件跨方言幂等形态仍由 S006 把关。
5. **同一 chunk 既判又跑**（评审 S-major）：`split_statements`（`seed.rs:288`）与 S006
   （`checks.rs:240`）切分逐字一致；门禁判原文、`translate_insert` 改写后执行，顺序钉死。
6. **全量预扫零副作用**（FR11，评审 E-B3）：命中即零行落库，不允许「先跑一半再报错」。
7. **--db 未声明 fail-fast**（`slim()` 既有纪律）；不新增可关闭门禁的开关（fail-closed，
   对齐 S006 与证书门禁的无逃生口精神）。
8. **错误脱敏裁剪**（评审 S-minor）：`seed.results[].file` **只输出文件名**（不含绝对
   路径）；seed 错误文本经 `sanitize_dsn` + 限长（复用 `migrate::log_snip` 口径），
   不回显完整 SQL / 路径。`sanitize_dsn` 只抹 `://user:pass@`，不盖路径，故须单独裁剪。

### 有意不对称（评审 S-③ 有条件背书，采纳）

- 冷启动 `replay_all` **逐字不变**、不加门禁（部署方自己构建的项目，依赖部署方过
  `oj check`）。**明确登记**：产物落 dist/src 前若未过 `oj check`，冷启动即无门禁重放
  不可信 SQL——属既有信任模型，本轮不动，登记待做核实。
- 该不对称是**有意**的：两条路径信任边界不同，不是遗漏。

## 7. 实施范围与分期

### 阶段 1 · 内核新入口（`oj/src/seed.rs` + `oj/src/checks.rs`）

- 新增 `pre_validate_seeds(modules, dialect) -> Result<(), String>`（FR10）：逐模块
  `pick(root, dialect)` 选文件 → 读文本 → `split_statements` → 按 §6 全量门禁 →
  返回；**不执行**。`pick`/`Pick`（`seed.rs:113-141`）私有、同模块可用，无需改可见性。
- 新增 `replay_modules(db, &[(name, 模块根)]) -> Vec<SeedResult>`（FR8/E-B2）：内部
  `s.target.dialect_info()` → `Dialect::known` fail-loud（不复制门控，E-B6）→
  `pre_validate_seeds` → S002（选中集内）→ 逐语句 exec → `seed ok` 日志 →
  逐模块 continue-and-collect。
- **抽取形状（评审 E-minor 钉死）**：只共享 `seed.rs:421-467`（读 texts → S002 +
  `leak_str:439` → 切句/改写/exec/按文件合并 `seed ok`）；`seed.rs:355-420`（无 default
  warn / 门控 / `collect` / skipped+misnamed 告警 / `files.is_empty()` 早退）**原样留
  `replay_all`**，冷启动行为逐字不变（现有 12 个单测为基线）。勿带 `Option<db>` /
  `any_seed`（CLI 恒有 `s.target`）；勿再 `collect`/`module_roots`。
- `checks.rs`：`code_tokens` 改 `pub(crate)`（或加 `stmt_head`）；修 `lead_word` 根因
  （§6.3）；两者都加对抗性单测。

### 阶段 2 · CLI 旗标（`oj/src/args.rs` + `oj/src/migrate_cmd.rs`）

- `args.rs`：`MigrateArgs.with_seed: bool`；`Commands::Migrate` 加
  `#[arg(long = "with-seed", conflicts_with = "baseline")]` + 帮助文案；
  `to_command` 透传；头部子命令清单注释补旗标说明。
- `migrate_cmd.rs`：**插入点钉死**（评审 E-B7）——`migrate_cmd.rs:291-295` 算出
  `failed` 之后、`:296` 打印之前，条件 `a.with_seed && failed.is_empty()`；
  ① `:280`（`slim()` Err 早退）与 `:297` 两处 `JsonOut` 都要补 `seed: Option<...>`
  （`#[serde(skip_serializing_if = "Option::is_none")]`，照 `JsonResult.error:203` /
  `JsonOut.error:212`）；② seed 失败经 `sanitize_dsn:107` 折进最终 `Ok/Err:309-318`，
  否则 exit 0 被 `migrate.ts:307` 判 done；③ 不带旗标时最终 Err 文案、人读
  `:303-307` 原样。
- **编译面（评审 E-B1，必须）**：`MigrateArgs` 加字段会打断 `migrate_cmd_tests.rs`
  18 处 `MigrateArgs{…}` 字面量（`:107` ~ `:743`），逐一补 `with_seed: false`。
- `main.rs`：**不动**（仍是 `Command::Migrate`）。

### 阶段 3 · 测试（含对抗性探测用例）

- `args_tests.rs`：旗标解析、缺省 false、`--with-seed --baseline` 互斥报错、
  `--site`/`-d`/`--module-root` 互斥、`--format` 非法值。
- `migrate_cmd_tests.rs`：18 处字面量补齐；不带旗标 JSON 逐字不变；带旗标 stdout
  恰一行、顶层 `seed` 形状；脱敏；无 seed no-op；seed 失败 exit 非 0；
  migrate 失败时顶层 `seed` 不出现。
- `checks.rs` 单测：`/*x*/ DROP TABLE t` / `/*c*/ INSERT`（`lead_word` 根因 + `code_tokens`
  首词）、`UPDATE`/`DELETE`/`ATTACH`/`PRAGMA` 被白名单拒、幂等 INSERT 放行。
- `seed.rs` 单测 + `oj/tests/` e2e（真 sqlite 库）钉死：
  1. 重复 `oj migrate --with-seed` 行数不翻倍；
  2. 非幂等 INSERT / DDL / `/*x*/ DROP` 被门禁拒、且**零行落库**（击穿尝试）；
  3. **全量预扫**：模块 A 合法 + 模块 B 含 DDL → A 也零行落库（FR11 击穿尝试）；
  4. 不存在的 `--module` fail-fast，错误信息列已知模块；
  5. `--module-root` 嵌套模块能定向；
  6. `reused=true` 旧模块零重灌（回归钉死 `!reused`）；
  7. 方言件 / 错名标签 / 缺匹配方言件的行为与冷启动一致；
  8. 迁移失败时不跑 seed；
  9. 无 seed 模块 no-op，stdout 恰一行 JSON、无 `seed ok` 日志；
  10. seed 失败 stderr 人话含点名文件与下一步（P-A 可诊断性）。
- 测试一律 `flavor = "current_thread"`（`migrate_cmd_tests.rs:104`）。

### 阶段 4 · 版本与文档

- `oj/Cargo.toml:3` `0.1.124 → 0.1.125`，`Cargo.lock` 同步。
- `CHANGELOG.md` 新增 `## v0.1.125` 段。
- `docs/devkit/api-manual.md`：`oj migrate 输出格式与模块定向`（`:334`）章节内新增
  `--with-seed` 段 + JSON 示例 + **错误/限制表**（`:3762` 附近）补 seed 错误文案；
  命令表（`:320`）、§13 限制表同步。
- `docs/devkit/SKILL.md`：**订正 `:134` 已有的字面 `oj seed`**（评审 P-nit / A-nit，
  与「不新增子命令」冲突）；陷阱速查补「装后数据面空 → `--with-seed`」+ 交付边界说明。
- `docs/devkit/scenarios.md`：两个可照抄场景——① **安装器调用**（带 `--format json`
  + 白名单约束）；② **运维 / CI 直跑** `oj migrate --with-seed -d src <module>`
  （评审 P-D major：不经 `platform.exec` 白名单、dev 树裸模块名、成败靠 exit + stderr、
  无 retry、建议带 `--module`）。
- `docs/devkit/README.md` 命令清单；`docs/modules/04-oj-cli.md` §1 migrate 行补
  `--with-seed` 并**一并对齐既有遗漏的 `--format`/`--site`/`--module-root`**（评审 A-nit）；
  §2 装配顺序说明改为**瘦身装配归属**（`--with-seed` 不走 `App::from_config`）。
- `cargo xtask build` 归置 `bin/devkit/`，`cargo test --release -p xtask` 校验产物一致。

### 阶段 5 · 门禁

`cargo fmt --check` → `cargo clippy --release --workspace --all-targets -- -D warnings`
→ `cargo test --release --workspace` → `cargo xtask smoke --bin bin/oj`。

## 8. 行为契约

```
oj migrate --with-seed [OPTIONS] [module]
  # 既有参数全部不变：-c/--config、-d/--dir、--site、--module-root、--db、
  #                    --baseline、--format json、[module]
  --with-seed          # 迁移完成后对同一批模块同批重放 seed.sql（opt-in，缺省关；
                       #   与 --baseline 互斥）
```

- 顺序：`migrate_collect`（逐模块 continue-and-collect）→ **seed 重放**（仅当
  `migrate` 全成功）→ 输出 → 退出码。
- seed 目标库 = `s.target`（`--db` 或 default，与 migrate 同一连接）；模块 = `s.modules`。
- `--format json` stdout 恰一行（`seed` 为新增顶层字段，逐模块 continue-and-collect）：

```json
{"results":[{"module":"books","ok":true,"applied":[{"version":1,"name":"0001__init"}]}],
 "seed":{"results":[{"module":"books","ok":true,"file":"seed.sql","dialect":"sqlite","stmts":3}]}}
```

- `seed.results[].file` **只出文件名**（§6.8）；失败条目 `ok:false` + `error`（脱敏 +
  裁剪）。
- **不带 `--with-seed`**：JSON 无 `seed` 键，其余逐字不变（零回归）。
- **migrate 失败**：不跑 seed，顶层 `seed` 不出现。
- **seed 失败**：`seed.results[].ok=false` + `error`；**不动顶层 `error`**（专属 migrate
  装配期全局失败）；整条命令 exit 非 0；stderr 人话把具体原因放最前（§5 FR5）。
- **下游诊断通道（评审 P-A，如实标注）**：`migrate.ts:307-314` 在 `code !== 0` 提前
  return，`seed.*` JSON **永不被解析**；唯一出口 `:312` 取 `(stderr||stdout).slice(0,500)`
  头部。故安装器侧可诊断性**全押 stderr 人话**；顶层 `seed` 字段供运维 / CI 直跑消费。
  `parseMigrateOutput` 在**成功路径**零改动兼容（只取 `results`/`error`，忽略未知顶层键）。
- 缺省 / `--format human`：人读摘要；`seed ok` 等 tracing 落 stderr（不污染 stdout）。
- 退出码：migrate 或 seed 任一失败非 0；全部成功 0。无 seed 模块成功 no-op。

## 9. 验收口径（对齐草案 §5，按旗标方案改写；逐条标验证手段与归属）

| # | 口径 | 验证手段 | 归属 |
|---|---|---|---|
| 1 | 装带 seed 的方案，**不重启**，`select count(*)` 即得种子行 | 本轮：oj-bin e2e（真 sqlite 库 + `oj migrate --with-seed`，同一次调用不重启即得行）；**端到端（ojm install）待 oj-module 采纳 §10** | **下游门禁 / 待集成** |
| 2 | 重复 `oj migrate --with-seed` 行数不翻倍 | oj-bin e2e | oj-bin |
| 3 | 无 seed 模块带旗标成功 no-op、**stdout 恰一行 + 无新增 `seed ok`/warn** | oj-bin 单测 | oj-bin |
| 4 | 热重载（`reused=true`）后旧模块种子不被重灌 | oj-bin 回归用例钉死 `app.rs:2038` | oj-bin |
| 5 | 不带旗标的 `oj migrate` 输出与退出码逐字不变 | oj-bin 单测（JSON 逐字比对） | oj-bin |
| 6 | migrate 段既有 retry / R14 / 禁降版守卫自动覆盖 seed | 结构核对（`installer.ts` STAGES 无 seeding 段）+ 集成验证 | **下游门禁 / 待集成** |
| 7 | 门禁击穿：非幂等 INSERT / DDL / `/*x*/ DROP` / `UPDATE` / `ATTACH` 零行落库 | oj-bin 对抗性用例 | oj-bin |

## 10. 下游待办（oj-module，本轮只读登记，不实施）

**白名单扇出（评审 A-① / P-B 纠正，勿低估）**：argv 模板逐 token 逐字相等
（`api-manual.md:1399-1402`，token 数不齐即 `exec template mismatch` fail-closed）：

- `buildMigrateCmd`（`migrate.ts:68-70`）返回值末尾加 `--with-seed`（argc +1）。
- `templates/config.yaml:132` 的 `&INSTALLER_EXEC_ALLOW` 锚（`:129/136`）同步新增
  8-token 模板；`:127` 注释「一个 token 都不能动」的相关逐字断言测试
  （`migrate.spec` / `installer.spec` / `exec-grant.spec` / `market-api.spec` /
  `installer.ts`）同步。
- **`docs/devkit/scenarios.md:2548/2633/2710` 的 argv 样例**同步。
- **每个已部署消费方项目 `config.yaml` 的 `platform.exec.*.allow`**（`api-manual.md:1391`）
  都要同步——这是消费方项目级配置，不在两仓内；漏改即安装硬失败、非优雅降级。
- `MIN_OJ_VERSION` 从 `0.1.121`（`packages/cli/src/vendor.ts:51`）抬到 `0.1.125`，
  **与上述模板 / 消费方配置同步发**。
- playbook §11 U7 标为已修、§7.7 第三轮绕行口径、§6 临时绕行段落收口。

**明确不需要**（评审 P-B 实测成立）：新增 seeding stage、stage 序列 10→11、
`buildSeedCmd`、独立 svcDir 物化与清场时序、两段式终态新增 retry 段、
R14 备份重复做、`parseSeedOutput` 新解析器。

**下游失败语义残留（评审 P-A minor，登记）**：`code !== 0` → `onMigrateOutput`
（`installer.ts:167`）不触发 → `appliedSeq` 丢；重试时 migrate 已幂等（0 applied）
→ `maxAppliedSeq` 返 null → `rollbackGuard` 跳过、`task.appliedSeq` 落空。下游侧待办。

## 11. 约束（发版红线）

- `bootstrap.js` 7-bit ASCII；插件 `panic = "unwind"`；`JsRuntime !Send` /
  current_thread；测试与 clippy 一律 `--release`；禁固定 sleep 当同步。
- 版本分界固定动作：`oj/Cargo.toml` + `Cargo.lock` + CHANGELOG + devkit 四件逐条对齐
  用户可见变更 + `cargo xtask build` 归置 `bin/devkit/` + xtask devkit 契约用例。
- 提交信息用中文、不加 `Co-Authored-By`；改动完成不自动 commit / push，等用户确认。
- 「推送后不盯 CI」；本轮不涉及 Linux 全量构建。

## 12. 评审意见与处置（2026-11-07，四路并行只读）

> 来源标记：E=专家工程师 / A=架构师 / P=产品 / S=安全工程师。
> 优先级 blocker / major / minor / nit；「已实测」= 逐行读码核实。
> 评审意见**不全盘照做**：冲突或过度收紧的先判定 blast radius，不采纳项写明理由。

| # | 来源 | 优先级 | 意见（要点） | 处置 | 理由 |
|---|---|---|---|---|---|
| 1 | E | blocker 实测 | `MigrateArgs` 加字段打断 `migrate_cmd_tests.rs` 18 处字面量 → 编译失败 | **采纳**：阶段 2/3 逐一补 `with_seed: false` | 实现必需，无取舍空间 |
| 2 | E | blocker 实测 | `Result<Vec<SeedApplied>,String>` 单点 Err 与 §8 逐模块 `seed.results[].ok=false` 自相矛盾 | **采纳**：改逐模块 continue-and-collect，`replay_modules -> Vec<SeedResult>`；FR8 / §8 同步 | 与 `migrate_collect` 同形，失败可诊断 |
| 3 | E/S | major 实测 | 门禁须**全量预扫再执行**（`replay_all` 无事务、`seed.rs:447` 逐句 exec，中途失败已部分落库） | **采纳**：FR11，收齐 texts → 统一门禁 + S002 → 才进 exec | §6「零副作用」否则不成立 |
| 4 | E/S | blocker 实测 | `lead_word`（`checks.rs:424-437`）只跳行首 `--`/`/*`，`/*x*/ DROP TABLE t` 整行被略过 → DDL / 非幂等 INSERT 双绕过 | **采纳**：新门禁首词改 `code_tokens(stmt)[0]`；并**修 `lead_word` 根因**让 S006 + fixtures 同源受益，加对抗性用例 | 根因优先；只修注释判定、不改准入口径，blast radius 小 |
| 5 | S | blocker 实测 | S006 只拦 7 DDL 词，`UPDATE`/`DELETE`/`ATTACH`/`PRAGMA`/`INSERT…SELECT` 全放行；裸 SQL 无白名单、运行期无归属图 → 比 S006 松 | **采纳**：新入口改 **fail-closed 语句白名单**（只放行幂等 INSERT），其余一律拒 | 新旗标 opt-in，收紧**不影响既有工程**，化解「零回归」矛盾；显式改口门禁定位为纵深防御、非完整沙箱 |
| 6 | E | major 实测 | dialect 必须按文件取 `seed_dialect_of`，不可传目标库 dialect（否则通用件误拒、方言件放松） | **不采纳**（见 N3） | 与 S-major 冲突，裁定取目标库方言，理由见 N3 |
| 7 | S | major 实测 | 门禁按文件方言、执行按 target 方言 → 通用件在 mysql/pg 口径错位（放行 `ON CONFLICT` 却跑 1064） | **采纳（改裁定）**：新门禁按**目标库方言** `Some(d)` 判；冷启动 S006 保持按文件方言不变 | 见 N3 |
| 8 | E | major 实测 | 方言门控勿复制两份（`seed.rs:363-369`） | **采纳**：`replay_modules` 内部由 `s.target.dialect_info()` 解析，抽共享 `resolve_dialect` | 避免 FR3 fail-loud 漂移 |
| 9 | E | major 实测 | 插入点须在 `migrate_cmd.rs:291-295` 算 `failed` 之后、`:296` 打印之前；两处 `JsonOut` 都补 seed；seed 失败经 `sanitize_dsn` 折进最终 Err | **采纳**：§7 阶段 2 钉死，条件 `with_seed && failed.is_empty()` | 保证 ⑦ 与 ⑧ |
| 10 | E | minor 实测 | 抽取形状只共享 `seed.rs:421-467`，`:355-420` 原样留 `replay_all` | **采纳** | 冷启动逐字不变的最小改法 |
| 11 | E | nit | FR3「逐字一致」与 §6 有意不对称措辞张力 | **采纳**：改「机制一致、准入集更严」 | 消歧 |
| 12 | A | major 实测 | 「下游成本大幅下降」夸大：白名单是 `config.yaml:132` 逐 token 模板，旗标与子命令同样加一条；且 oj-module 只读 ⇒ **U7 本轮未端到端闭环** | **采纳**：§4 A' 措辞收敛、§0 新增「交付边界」、§9 #1/#6 标下游门禁 | 事实纠正；不得把本轮描述为「安装后数据就绪」 |
| 13 | A | major | `strict: bool` 把信任边界策略泄漏进机制层 | **采纳**：FR10 门禁外置为前置纯函数 `pre_validate_seeds`，`replay_modules` 纯执行 | 消 boolean blindness，保住逐字不变 |
| 14 | A | minor 实测 | migrate 失败时顶层 `seed` 出现与否 / 谁算方言未定 | **采纳**：§5 组合语义钉死（不出现）、§5 方言判定钉死（N3 + #8） | 契约不可留白 |
| 15 | A | nit 实测 | `-with-seed` 属瘦身装配而非 `App::from_config` 顺序；`04-oj-cli.md` §1 migrate 行本就漏 `--format/--site/--module-root` | **采纳**：阶段 4 一并订正 | 文档准确性 |
| 16 | A | nit | 与 `oj test fixture` 是否收敛 | **不采纳收敛**：保持两处 | fixtures 无方言选件/S002/告警，信任边界与结果形状不同；谓词已共享于 `checks.rs` |
| 17 | P | major 实测 | seed 失败时 `migrate.ts:307` 提前 return，`seed.*` JSON 是死字段；`:312` 取 500 字**头部**，错误在尾部会被截断 | **采纳**：§5 FR5 / §8 钉死 stderr 人话放最前且简洁、点名文件/语句/下一步；顶层 `seed` 明确供运维/CI 消费 | oj-bin 侧唯一可做的补偿 |
| 18 | P | major 实测 | 白名单扇出低估（含**每个消费方项目 config.yaml**、scenarios 多处、逐字断言测试） | **采纳**：§10 完整列出 | 漏改即安装硬失败，非优雅降级 |
| 19 | P | major 实测 | 缺运维/CI 直跑 `oj migrate --with-seed -d src <module>` 独立场景；api-manual 命令表/JSON/错误表/§13 均须补 | **采纳**：阶段 4 补第二场景 + 四处文档 | DX 完整性 |
| 20 | P | minor 实测 | §9 六条非都能被安装器直验；「零日志噪音」主观 | **采纳**：§9 改为逐条标验证手段/归属；#3 钉为「stdout 恰一行 + 无新增日志」 | 验收可操作 |
| 21 | P/A | nit 实测 | `SKILL.md:134` 已有字面 `oj seed` 与「不新增子命令」冲突 | **采纳**：阶段 4 订正 | 文档一致性 |
| 22 | S | major 实测 | 门禁判原文、执行判改写后；须钉「同一 chunk 既判又跑」、切分与 S006 逐字一致 | **采纳**：§6.5 | 防漂移 |
| 23 | S | minor 实测 | `sanitize_dsn` 不盖 `seed.file` 绝对路径 / SQL 片段 | **采纳**：§6.8 `file` 只出文件名、错误文本裁剪 | 信息泄漏面 |
| 24 | S | major 猜测 | 冷启动不对称：产物落 dist/src 前若未过 `oj check`，冷启动即无门禁重放不可信 SQL | **登记待做核实**（不改） | 属既有信任模型；`replay_all` 逐字不变是已裁决决策 ③，本轮不动 |
| 25 | S | nit 实测 | 签名只绑 key_id 不证 seed 内容安全 → 门禁不可省 | **采纳**（背书保留门禁） | 门禁定位见 §6 |

### 不采纳项及理由

- **N1（E-6 / S-7 冲突裁定）**：工程师主张门禁按**文件方言**判，安全侧主张按**目标库方言**。
  **裁定：新门禁按目标库方言 `Some(d)`**。理由：新入口 opt-in、fail-closed，只在这台库上跑；
  按目标方言能提前拒掉通用件里的方言专属幂等形态（否则 mysql/pg 跑出 1064），更严且正确；
  且因 opt-in **不影响任何既有工程**，不触发工程师担心的「由绿变红」。冷启动 S006 保持
  按文件方言（`checks.rs:236-237`）不变，通用件跨方言幂等形态仍由 S006 把关；两处口径
  差异在 §6.4 写明。
- **N2（S「升级 allowlist 或显式改口」）**：两者**都做**——升级 fail-closed 白名单（#5），
  并显式改口门禁定位为「纵深防御、非完整沙箱」（§6 定位）。理由：单做前者会让人误以为
  已能对抗恶意包；单做后者等于不修 blocker。
- **N3（S 建议把 S006 也统一成 target 方言）**：**不采纳**。理由：S006 是构建期静态门禁，
  面向「一份通用件要能在三方言跑」，按文件方言是其固有语义（`checks.rs:463` 注释明确
  「不新收紧，避免既有工程由绿变红」）；改动会牵动静态检查口径与既有工程，blast radius
  超出本需求。登记为「通用件跨方言幂等形态由 S006 把关」。
- **N4（A 建议合并 `oj test fixture`）**：**不采纳**，理由见 #16。
- **N5（首版「新增 `oj seed` 子命令」方案）**：用户已裁定改用旗标（§4 A'），作为
  「不迁移只补灌」的未来原语**登记待做**，本轮不实施。

### 登记待做

1. **安全剩余风险**（评审 S）：写他模块表（运行期无归属图）、单 DSN 全库权限、
   按模块库隔离 / 最小权限 DB role、seed allowlist 进一步硬化。
2. **冷启动信任链核实**（评审 S-24）：产物落 dist/src 前的 `oj check` 链；`verify.ts`
   签名不证 seed 内容安全、`extract.ts` tgz 解包路径穿越复核、市场信任 UI 是否展示 seed。
3. **通用件跨方言幂等形态**（N3）：由 S006 把关的既有口径，另立任务统一评估。
4. **`oj seed` 独立子命令**（§4 A / N5）：「不迁移只补灌」运维原语，复用 `replay_modules`。
5. **seed 账本**（§4 C）：`_oj_seeds` 按模块+版本记账，需先解决与「冷启动全量幂等重放」
   的语义冲突。
6. **下游 `appliedSeq` 残留**（§10 / P-A minor）：seed 失败时 `onMigrateOutput` 不触发、
   `rollbackGuard` 依据削弱，oj-module 侧待办。
7. **端到端集成验收**（§0 / §9 #1 #6）：oj-module 采纳 §10 后复跑 drill3。
