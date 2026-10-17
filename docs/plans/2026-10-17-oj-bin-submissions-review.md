# oj-module 下游诉求评审：OJ-6 / OJ-8 / OJ-9（2026-10-17）

- 输入：`../oj-module/docs/prd/solution/202610171400-oj-bin-submissions/`（三份 PRD）
- 方式：四路只读评审（工程师 / 架构师 / 产品 / 安全），本文件为综合判决与解决方案；
  各路口记见 §5（未到齐的先留位，到齐即补）。
- 约束：只调整 oj-bin；oj-module 只读。既有红线：fail-fast、配置即授信、
  devkit 四件随版本同步、CLAUDE.md 设计红线。

---

## 1. 总判决

| 诉求 | 判决 | 定级 | 一句话理由 |
|---|---|---|---|
| OJ-8 Windows exec 可操作报错 | **做**（范围从 PRD 的「两件」扩为**三件**：PATHEXT 解析 + hint 文案 + 文档） | P1 成立 | Windows 上裸名命令**永远**解析不到 .exe（resolver 无 PATHEXT 探测）——PRD 的「加 PATH」hint 不同步修 resolver 就是死路文案；官方 tar 模板在 Windows 整体不可用 |
| OJ-6 migrate.run 进程内触发 | **缓做**（观察项，设激活触发条件） | 可选/P3 成立 | reloadConfig 槽位先例完整、不触红线；官方 exec 绕行已实测可用；真增益是 600s 超时限崖与重复瘦装配，未到必做程度 |
| OJ-9 down 迁移 / 账本回退 | **档 1（内建 down）不做；档 2（账本回退原语）缓做** | P3 成立 | 档 1 推翻成文设计红线（docs/migration.md:209「只前向」）且 MySQL DDL 隐式提交使「无半回退模糊态」不可满足；档 2 与既有口径相容、百行级，但产品语义 = 官方盖章的「schema 与账本 knowingly 分叉」，等市场侧真实案例 |

**执行顺序**：OJ-8（含 PATHEXT）→ 观察 OJ-6 → 观察 OJ-9 档 2。

---

## 2. OJ-8 · Windows exec 可操作报错（做，三件）

### 2.1 PRD 与源码事实的重大偏差（major，实施前须与提交方对齐）

PRD 归因「宿主解析不到外部命令」为 PATH 问题，hint 让用户「把 Git 的 usr\\bin 加入
PATH」。**实测根因不同**：`default_resolve_command`（src/bridge/exec.rs:537-557）在
Windows 分支只做 `dir.join(name)` + `is_file()`（exec.rs:568-570），**不探测 PATHEXT
扩展名**——裸名 `tar` 找的是名为 `tar` 的无扩展名文件，而 System32 里是 `tar.exe`。
推论：

1. 照 PRD hint 加 PATH 后仍然解析失败 → hint 不可操作（blocker 级文案缺陷）；
2. Windows 10 自带 `tar.exe` 的论述 moot（根本走不到执行）；
3. 场景 34/35 的官方模板（裸名 `tar`）在 Windows **整体不可用**——比 PRD 描述的
   「报错不可操作」更痛；
4. 现行变通（PRD 未提）：模板首 token 写全名 `tar.exe`（parse_template 只拒 `${`
   与路径分隔符，.exe 合法）——文档至少该写这条路。

### 2.2 方案（三件，同一处代码 + 文案 + 文档）

1. **resolver 修补**：`default_resolve_command` Windows 臂加 PATHEXT 探测——依次试
   `name`、`name.exe` 及 `%PATHEXT%` 列表项（限定可执行类扩展，防把任意文件当命令）；
   安全模型不变（仍限 PATH 目录 + is_file，模板仍须 allowlist 命中，`${path:}` 校验不动）。
2. **hint 文案**：exec.rs:322-324 map_err 处 `#[cfg(windows)]` 编译期分支追加 hint
   （Git Bash 路径 + usr\\bin 加 PATH + 「模板命令名可带 .exe」）；非 Windows 分支逐字
   不动；错误分类不变（普通 Error）；`{first}` 插值命令名（zip/unzip 自动跟随）；
   不承诺 GNU/bsdtar 行为一致。
3. **文档**：api-manual §6 错误表（exec.rs:1711 锚点条目）+ §6 Windows 前置说明；
   scenarios 场景 34/35 顶部补 Windows 前置 + tar.exe 全名变通；SKILL.md 陷阱速查补
   Windows 条目（现有 exec 陷阱零 Windows 内容）；devkit 四件同步 + CHANGELOG。

### 2.3 验收（补 PRD）

- Windows + PATH 无 tar：报错含 hint 段与 Git Bash 路径；命令名正确插值；
- Windows + PATH 有 tar.exe：裸名 `tar` 解析成功（PATHEXT 修复的直接证明）；
- 非 Windows 报错文案与现状逐字一致（测试钉死）；
- 错误分类不变（普通 Error，非 NotCapable），前缀 `exec: command resolve failed` 保留。

---

## 3. OJ-6 · migrate.run 进程内触发（缓做）

### 3.1 评审共识

- PRD 自评「绕行已完整可用」属实：官方路径是文档化范式（scenarios.md 场景 35 八步闭环），
  exit code 真源 + json 双通道判定是 CLI 限制的产物。
- **无 blocker**：不绕过任何约束（migrate.run 只 apply；门禁逻辑不动；apply 后产物不匹配
  下次启动/热更 M003/M004 照拒）。
- PRD 低估的两个真实增益：① exec.run 600s 硬上限（exec.rs:233-235）杀大 DDL 留半截账本
  （SKILL.md:263 已挂名的坑）；② 每次 `oj migrate` 子进程重走 slim()：重读 config +
  重新 dlopen 插件 + 新建连接池（migrate_cmd.rs:20-90）。

### 3.2 激活触发条件（满足其一再立项）

- 消费方安装器真实撞到 600s 限崖（大 DDL）；
- 或连续安装场景重复瘦装配开销进入 Top 痛点。

### 3.3 预研形态（立项时的起点，非现在实施）

- 照搬 reloadConfig「槽位后填」：StableState 加 `migrate: OnceLock<Executor>`，
  oj serve 装配期填槽（dbs/dir/db_key 在手边），**不复用 slim**（直接用 StableState.dbs
  活句柄，避开二次装配与配置漂移）；
- 门禁 `assert_platform`（platform.modules）与 reloadConfig 同级；
- opts `{dir?, module?}`：显式 dir 过 `resolve_allowed`；**不向 JS 暴露 db 选择**；
- **语义建议与 PRD 不同**：进程内失败应**抛错**（安装器已有 try/catch fail-closed），
  成功返回 applied 账据；子进程的「exit code + json」双通道是 CLI 限制，不是该复刻的优点；
- JsRuntime !Send：executor 只 await DataAccessor 调用，可在 bridge current_thread 上
  直接 await（与 reloadConfig 同款占用语义），不要 spawn_blocking 嵌套 runtime。

---

## 4. OJ-9 · down 迁移 / 账本回退（档 1 不做；档 2 缓做）

### 4.1 事实核查（PRD 引文全部属实）

M003（migrate.rs:479-486）/ M004（:492-498）文案与判定点准确；release 缺省 verify、
dev 缺省 auto、off 逃生门（app.rs:1456-1494）；refinery-core 0.9.2 **无任何 down 能力**
（Target 仅 Latest/Version/Fake/FakeVersion，runner.rs:45-50）——档 1 = 从零自建引擎。

### 4.2 档 1（内建 down）不做的两条硬理由

1. **推翻成文设计红线**：docs/migration.md:209 明文「只前向，无 down 迁移……降版部署被
   M003 硬禁」，:231「回退以新 seq 前向迁移实现」——这不是 PRD 验收标准能覆盖的，
   需设计决策记录；
2. **验收 #3 在 MySQL 上不可满足**：DDL 隐式提交，OjConn MySql 臂刻意不 BEGIN
   （migrate.rs:172-186）。down 步骤「DDL + 账本 DELETE」任一顺序都产出模糊态
   （DDL 成删败 → 账本超前 → M003；先删后 DDL → 账本落后但库多对象 → verify 静默通过）。
   另：PRD 建议的 `V{n}__x.down.sql` 命名今天会被 S007 拒（desc 限 [A-Za-z0-9_]，
   migrate.rs:239-244）——新永久契约面。

### 4.3 档 2（账本回退原语）缓做 + 预研形态

- 与既有口径**相容**：migration.md:232「人工改账本前先备份」已承认人工改账本；原语是
  带留痕的升级，不推翻只前向设计。实现约百行：`oj migrate rollback --module <m>
  --seq <n>`（独立子命令）→ SELECT 列账（回显）→ 参数化 DELETE（勿学 quote_str 拼接，
  migrate.rs:88-91 是既成事实不扩展）→ 返回被移条目；
- 产品语义 = 官方盖章「schema 与账本 knowingly 分叉」：输出必须明示「只移账本条目、
  DDL 未回退」+ 指引 `oj schema diff` 复核——把半自动口径写进契约，而非假装原子；
- 验收 #3 的正确表达：**模糊态由契约显式排除**（边界可判定、断点可续），不是由原子性保证；
- 激活条件：市场出现高频真实降级诉求；市场侧先做「降级 = 发布带逆序迁移的新版本」的
  产品规范（PRD 没提的更优路径）。

---

## 5. 各路评审口记

- 产品（review-product）：已到齐，结论并入 §2-§4（含 PATHEXT blocker 发现）。
- 工程师（review-engineer）：已到齐，结论并入 §2-§4（含 OJ-9 档 1 的 mysql 原子性
  分析与 migration.md:209 红线冲突）。
- 架构师（review-architect）：待补。
- 安全（review-security）：待补。

## 6. 给提交方的答复要点

1. OJ-8 根因修正：不是「PATH 没有」，是「裸名解析不到 .exe」；hint 必须搭配 resolver
   PATHEXT 修补才可操作；另请知悉 tar.exe 全名模板是今天的可行变通；
2. OJ-6 维持「可选优化」定级，给出激活触发条件；若做，进程内 API 的失败语义建议抛错
   而非复刻 exit code 双通道；
3. OJ-9 档 1 明确不做（红线 + mysql 原子性），档 2 缓做并先重写验收 #3；降级的正解是
   市场侧「发布逆序迁移的新版本」。
