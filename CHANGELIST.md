# CHANGELIST

以 `oj/Cargo.toml` 的 version 递增提交作为版本分界（该提交即本版本的发布点），fix 类改动在每个版本内单列一组。

## 下一版（未发布）

> v0.1.23 已发版：标签 `v0.1.23` → `039b314`（含双专家评审处置）。本节收录其后落在 `main`
> 上的改动；版本号在发版点（`oj/Cargo.toml` 递增提交）确定。

**清账：v0.1.22「已知债 / 另案登记」五条**

设计与逐条证据见 `docs/superpowers/specs/2026-09-19-debt-clearing-v0.1.22-design.md`。

1. **PG 语句缓存 × 混合参数类型（债①）——已修**。根因复核：sqlx 的语句缓存 key **只是 SQL
   文本**（`statement_cache.rs` 的 `LruCache<String,_>`），PG 执行路径**无条件查缓存**并把旧的
   `param OIDs` 复用到本次 Bind → 同文本换参数 Rust 类型即协议级错误（真库实测
   `insufficient data left in message`）。修法：**按参数形态分缓存键**——PG 插件（`shape_tag`，
   4 个执行点）给 SQL **前置**块注释 `/*oj:<形态>*/`（形态字母表 `t/i/f/b/m/u`；必须前置，
   后置会被 PG 判多语句或被 `--` 注释吞掉），并给 PG 连接补
   `statement-cache-capacity=512`（分键后条目数 ×形态数，防 LRU 抖动触发 Close+Sync 往返）。
   **MySQL 不动**：复核其每次 execute 都重发参数类型（`new_params_bound_flag=1`），病不在此。
   验收：env-gated 真库用例 `real_postgres_same_sql_text_mixed_param_shapes`（改前必红 → 现绿，
   含池路径/tx 路径/缓存条目数断言）。
2. **u64 / `BIGINT UNSIGNED` 全链路（债②）——已修**。新增线形状 `{"$oj$u64":"<十进制>"}`
   （`oj-plugin-ffi::jsint`，**不 bump ABI**，但**插件须与宿主同批重建**）与 JS 全局
   `toUBigInt(v)`（`[0, 2^64-1]`，越界 RangeError）；`toBigInt` 对越 i64 的 bigint 改为明确报错
   并指路 `toUBigInt`。MySQL 插件迁到 **typed 路径**（`Pooled` 枚举：`mysql://` → `sqlx::MySql`，
   离线测试仍走 `Any`+sqlite 保 CI 覆盖），行解码改为**固定安全顺序**
   `u64 → i64 → bool → f64 → String → bytes`（`bool::compatible`/`f64::compatible` 都接受整型，
   顺序反了会把 BIGINT 读成 bool）；**读侧不再回绕成负数**。PG/SQLite 对 `$oj$u64` **明确拒绝**
   （bigint 就是 i64）。顺带修一处既有漏洞：`toSQL().params` 的超界整数改为回吐 marker
   （此前被出口护栏降成字符串 → 用户照文档「回放 params」必失败）。
   **顺带修真实缺口**：MySQL 8 默认 `caching_sha2_password`，明文连接需 sqlx 的 `mysql-rsa`
   feature——缺它插件**连不上任何 stock MySQL 8**（本机 8.4 实测报
   `RSA auth backend disabled`），已补。
   验收：`real_mysql_unsigned_bigint_roundtrips_as_u64`（`u64::MAX` 与 `i64::MAX+1` 精确往返）。
3. **数值型 `tenant_id` 列（债③）——已从「不支持」变为支持**。列类型从 schema.yaml plumb 到
   `SchemaRegistry`（`ColumnType`，旧构造器默认 `Unknown` = 旧行为）；`apply_tenant` 按列类型
   生成绑定值（text/Unknown → 字符串；integer/bigint → 数值，超 i64 用 `$oj$u64`；非十进制租户头
   → 指名报错），insert/update 的等值判定改走 `guard::param_is_tenant`（字符串/数字/i64 标记/
   u64 标记四形态），join 的 ON 条件同型。**新增声明期 fail-fast**：`tenant_id` 列类型 ∉
   {text,integer,bigint} → 启动报错（数值列上的 `bigint = text` 不再等到运行期）。
4. **内建序列分配原语（债④）——已做**：`db.nextSeq(name)`（`DBInstance` 与 `db.tx` 内均可）。
   平台表 `_oj_sequences(name, v)` 首次使用自动建（不经模块 schema/迁移）；取号**单语句原子**：
   PG/SQLite `insert … on conflict(name) do update set v = v+1 returning v`；MySQL
   `insert … values (?, last_insert_id(1)) on duplicate key update v = last_insert_id(v+1)`
   再 `select last_insert_id()`（必须同连接 → 无活跃事务时宿主用一次短事务）。返回值过 `jsnum`
   规则（≤2^53-1 给 number，超出给十进制字符串）。**这是 `select max(id)+1` 竞态的终态**。
   验收：离线 sqlite（稠密/并发/名称校验/事务搭车）+ PG 真库 20 并发（断言恰为 1..20）
   + MySQL 真库（顺序 1..3 + 12 并发无丢失更新）。
5. **MySQL 侧真库验证（债⑤）——已执行**。「本机拉不到镜像」的前提已消失：用 macOS `container`
   CLI 起 `mysql:8.4`（4C/1G，专用库 `oj_test`），MySQL 插件的全部 env-gated 用例首次实跑并全绿
   （roundtrip / i64 marker / u64 unsigned / 序列原子性）。同批实跑：PG 18 全绿、Redis 全绿。
   **未跑**：Kafka（9092 未起）、S3（poc-minio 缺预建桶与凭据，需 `mc mb` 先建
   `oj-test` 桶）、RabbitMQ（`guest` 仅 loopback，`poc` 用户需在容器内自建；本轮未做）。

**兼容性 / 行为变更（升级前请核对）**

1. **`toSQL().params` 的超界整数由「十进制字符串」改回 **marker**（可重放）**：此前该值被出口
   护栏降成字符串，导致文档教的「`db.query(toSQL().sql, ...toSQL().params)` 回放」必然失败
   （`bigint = text`）。现在 `>2^53-1` 的整数以 `{"$oj$i64":…}` / `{"$oj$u64":…}` 形式出现——
   比较 params 文本的代码需留意。
2. **`tenant_id` 非法列类型由「运行期 PG 报错」变为「启动期报错」**：`tenant_id` 只能是
   text/integer/bigint（`double`/`boolean`/`blob` 直接 fail-fast）。
3. **MySQL 行 JSON 形状随 typed 迁移改变**：整数列统一给 number（Any 时代部分类型落到字符串/
   字节）；`BIGINT UNSIGNED > i64::MAX` 由**回绕成负数**改为精确值（出口护栏再把 `>2^53-1`
   降成十进制字符串）。
4. **`toBigInt` 对越 i64 的 bigint 由「静默返回」改为明确报错**（指路 `toUBigInt`）。
5. **PG 实际执行的 SQL 文本带 `/*oj:<形态>*/` 前缀**（DBA 视角可见；`toSQL()` 不含）。
6. **宿主与第一方插件必须同批重建**：`$oj$u64` 是源码级共享的线形状，不 bump ABI，但旧插件会把
   新标记串化成文本。**混版本不受支持**（复现门槛低，见下方挂账的线形状门禁）。
7. **核心 crate 的 `[dev-dependencies]` 增 sqlx pg/mysql 驱动**（仅测试构建；core 级真库用例用）。
8. **MySQL 读侧的列类型边界（本批定稿）**：**可读** = 整数家族（含 `BIGINT UNSIGNED` → u64）、
   文本家族（`TEXT` 在 MySQL 协议里就是 `Blob` + 非 BINARY collation）、二进制、`FLOAT`/`DOUBLE`；
   **不可读** = `DECIMAL`/`NEWDECIMAL`、`DATE`/`TIME`/`DATETIME`/`TIMESTAMP`/`YEAR`、`JSON`、
   `BIT`、`GEOMETRY` → **报错**（点名列名 + MySQL 类型 + 指路 `cast(x as char)`），**绝不静默
   `null`**。对照 v0.1.23（全程 `sqlx::Any`）：这些类型当时同样读不出来，但错误是 sqlx 的
   `AnyDriverError`（不点名列）；且 `TINYINT`/`BOOLEAN` 当时**直接报错**，现在按整数读出 **`1`/`0`**
   （`BOOLEAN` 是 `TINYINT(1)` 的别名——**不是** `true`/`false`，本批无类型长度元数据可区分）。
9. **数值型 `tenant_id` 列在 `tenant.sql_guard: warn` 下也会硬失败**：租户头不是十进制整数时
   `tenant_value` 直接 `Err`（warn 模式只放行「无法判定」的情形，不放行「判定为不匹配」）；
   `text` 列的旧行为不变。注意这与「warn = 只告警」的直觉不同。
10. **`update({tenant_id: …})` 的等值判定改走 `param_is_tenant`**：text 列 + 数字租户头
    （如租户 `"7"`、字段写 `7`）由**拒绝**变为**放行**（放宽；两者语义等价）。

**新登记债务（清账过程中发现）**

- **deno_core op 驱动在并发等待时 `RefCell already borrowed` → abort 进程（既有，非本次引入）**：
  触发条件至少两种——① JS 侧同时发起的 op 数**超过 sqlx 池上限**（默认 10）；
  ② **在 JS 里 `await` 之后再发起 op**。复现（真库，非推测）：核心 accessor 连 PG 后
  `Promise.all` 发 16 个 `db.query("select 1")` → `op_driver/futures_unordered_driver.rs:309`
  panic 且 `panic in a function that cannot unwind` → SIGABRT（**整个进程挂掉，不只是该请求失败**）。
  与本次新增的 `db.nextSeq` 无关（`db.query` 同样复现）；10 并发以内、单轮发起时正常。
  本轮的 `db.nextSeq` 真库并发用例因此改由**插件层**驱动（`plugins/oj-db-*`），core 侧只留
  ≤8 并发的离线用例。**需专项处理**（要么在 op 包装层串行化/限流，要么查 deno_core 0.411 的
  op 驱动用法）。

- **线形状版本门禁缺失（架构评审 P1-1，本轮只登记）**：`$oj$u64` 这类跨边界线形状没有独立版本号，
  `HOST_FINGERPRINT` 不含它、且指纹不符**只告警不 fail**（`src/bridge/plugin_loader.rs`）→
  「宿主换新、插件留旧」不会被拦。建议后续引入单调递增的 `WIRE_VERSION`（可塞进 descriptor 字符串
  字段以免 bump repr(C)）并对它做**硬门禁**；本轮以文档纪律（§兼容性第 6 条 + `plugin-architecture.md`）
  替代，混版本明确不受支持。**本轮已做的最小缓解**：`oj-plugin-ffi` 版本 `0.1.0 → 0.1.1`
  （`HOST_FINGERPRINT` 含该版本号）——旧插件（用 0.1.0 构建）在新宿主上至少会打一条
  `fingerprint mismatch` 告警，把「静默混版本」变成可见信号。
- **MySQL 非整数/非文本列的读出（原 R3 的一部分，本轮只登记）**：`DECIMAL`/`JSON`/时间/`BIT`/
  `GEOMETRY` 目前**报错而非解码**（见 §兼容性第 8 条）。彻底修需要按 `MySqlTypeInfo` 做显式分派表
  （每种 MySQL 类型的期望 JSON 形状：`DECIMAL` → 十进制字符串、`JSON` → 对象、时间 → 字符串…），
  属独立设计，本轮先保证「不静默错值」。
- **单连接 accessor 在活跃事务期间池上发 DDL 会等锁超时（既有性质）**：`sqlite`（`max_connections(1)`，
  见 `accessor_sqlx.rs`）上，`db.nextSeq` **首次**在 `db.tx` 内使用会挂住——因为 `ensure_seq_once`
  的 DDL 必须走池，而池的唯一连接被调用方事务持有。PG/MySQL（池 ≥2）无此问题。这是 1 连接
  accessor 的通用性质（任何「事务内还去池上取连接」的路径都一样），非 `nextSeq` 引入；
  登记备查，未修。
- **真库验收不在 CI（架构评审 P1-3，本轮只登记）**：本批的跨方言正确性证据全部来自
  env-gated 用例，而 env 未设时**静默 pass**、CI 无 service 容器也不设 `OJ_TEST_*` → 门禁恒绿。
  建议后续加一个起 PG+MySQL service 容器的 job，并引入 `OJ_TEST_REQUIRE=1`（该模式下缺 env 即失败，
  防「专用 job 里变量名写错→依然绿」）。**开发侧评审补充的具体缺口**：`next_seq_first_use_inside_tx_on_real_db`
  是「事务内首次取号不毒化事务」（P1-2 修复）的**唯一**守卫，而 sqlite 没有 aborted-tx 语义、
  假实现更没有 → 该修复在 CI 上实际**无覆盖**。

**测试面变更**

- 核心 crate 的 `[dev-dependencies]` 增 `sqlx` 的 `postgres` / `mysql` / `mysql-rsa` 特征
  （**仅测试构建**）：env-gated 真库用例需要具体驱动（生产路径的 PG/MySQL 仍由 `oj-db-*` 插件承载）。
- MySQL 插件保留 Any+sqlite 的离线全路径用例（CI 无 MySQL 时仍有覆盖），新增 typed 路径的
  env-gated 用例。

**修复（文档）**

- `docs/devkit/scenarios.md` / `docs/devkit/api-manual.md`：匿名路径迁移 WARN 的**对外文案与
  代码对齐**（`3060da5`，v0.1.23 发布后复校发现，故按「不改写已发布标签」的约定进本节）：
  ① 「常见坑」里 WARN 触发条件补上「只可能来自 `auth.anonymous_paths`」与「确实丢面」；
  ② 引用的装配期报错文案订正为 `标了 one_layer，但末段不是 "*"`（原文案已过时）；
  ③ 新增「对象形态键名拼错（`one_layr`）会报错并指到出错条目的行号与下标」一行；
  ④ 场景索引注明 v0.1.20 收紧**仅 auth 侧**；⑤ OIDC 走读段补全为「两条列表各三条路径 +
  auth 的 `/idp/*` 标 `one_layer`」。纯文档改动、无代码变更；devkit 发布产物 `bin/devkit`
  已随之同步（`cargo xtask build`）。

## v0.1.23（2026-09-19）

下游 U39：`anonymous_paths` 的 v0.1.20 迁移 WARN **无法消音**。尾 `/*` 是四形态中的合法形态
（尾段动态时只能用 `/*`；改 `**` 反而扩面——`**` 含零层与任意深），但旧实现「凡尾 `/*` 即
告警」，这类条目每次启动都被点名，且没有任何配置手段让它退出聚合。本版三处改动合起来治：
**订正提示的适用面**（只 auth）+ **按真实影响面判定**（默认静音噪音）+ **条目级显式确认**
（保留人工逃生门）。设计、证据与双专家评审逐条处置见
`docs/superpowers/specs/2026-09-19-v0.1.23-anon-path-warn-design.md`。

**订正（v0.1.20 兼容性条目的措辞）**

- **v0.1.20 的「尾 `/*` 任意深度 → 严格一层」收紧只发生在 oj-auth 侧**。`tenant.anonymous_paths`
  自引入起就是严格一层：`git show v0.1.19:server/src/lib.rs` 的 `path_matches` 即
  `!rest[1..].contains('/')`，且 v0.1.19 的租户豁免（同文件 `:364`）走的就是它。v0.1.20 那条
  兼容性说明把两条列表并列，读起来像租户也收紧过——本版在原文加了订正注，并且**迁移 WARN
  只再对 `auth.anonymous_paths` 生效**（对租户条目说「收回了面」是伪前提，也解释了为何
  下游实测里那几条 tenant 告警怎么调都"消不掉"）。

**行为变更（启动日志，需知晓）**

- **迁移 WARN 只针对 `auth.anonymous_paths` + 按影响面判定**，两个条件**同时成立**才告警
  （`oj/src/app.rs` 的 `warn_legacy_tail_wildcards` / `migration_warn_entries`）：
  1. 条目是**旧式前缀形态**——只有尾段那一个 `*`、其余段全字面。旧 oj-auth 实现是
     `strip_suffix("/*")` + `starts_with`（尾 `*` = 任意深度），只有这种形态当时能命中真实
     请求；**含中段 `*` 的结构条目**（`/public/anchor/*/issues/*`）那时根本匹配不上，只能
     诞生于 v0.1.20 的四形态语义，即刻意写出的形状——对它们提「改 `**`」是错的。
  2. 该条目**确实丢了面**——存在一条已注册路由比严格一层更深（judged 段级：`tail_entry_loses_coverage`
     把路由 pattern 归一去 base 的视图后，逐段对拍「head + ≥2 段」是否可达；`**` 只多出的
     **零层**（裸 head）在旧语义下同样没覆盖，不算丢面）。
  - 实测校准（上一轮用本版前的二进制跑下游真实 config 的启动日志）：旧实现点名
    **9 条 tenant + 6 条 auth**；加形态条件后收敛为 **4 条 tenant + 1 条 auth**（U39 抱怨的
    结构条目全部静默）；再加「只 auth 参与」后 **tenant 侧归零**。下游套件本轮未复跑
    （该仓只读），auth 侧余下条目属真·旧前缀（`/public/assets/v2/anchor/*` 一类），
    需要时在那边跑一次 `oj test` 即可看到当前清单。
  - 调用点从装配早期（原 `:520`）**后移到路由表建好之后**，故 WARN 在启动日志里位置变晚
    （在 `module ...` 行与路由清单之后）。
  - 只按「已注册路由」判是充分的：静态托管与 `/blob` 都在鉴权前直接返回，不经匿名匹配
    （`server/src/lib.rs`，该处已补**互指注释**：改动提前返回的顺序/让它们咨询匿名表时
    必须同步本判定）。

**特性**

- **`anonymous_paths` 支持条目对象形态**（tenant / auth 同形）：除字符串简写外可写
  `- { path: "/auth/oidc/*", one_layer: true }`。`one_layer: true` = 显式声明「这一层是有意的」，
  **永久退出迁移 WARN**（含影响面判定为真的情形；对 tenant 列表是备注——那里不告警）。
  - **手写 `Deserialize`（不用 `#[serde(untagged)]`）**：untagged 无法 `deny_unknown_fields`，
    `one_layr: true` 会被**静默**解析成 `one_layer=false`（用户以为已消音而实际没有），
    报错还只给「did not match any variant」且位置指向**列表首元素**。现在只认 `path` 与
    `one_layer` 两个键：未知键 / 缺 `path` / 类型错都给出人话报错。
  - 消费侧统一归一为路径字符串（`config::anon_paths()`）：server 的 `Pipeline.tenant_anon` 与
    oj-auth 插件 cfg JSON 都只吃 `Vec<String>`，**插件侧零改动、ABI 保持 8**。
  - **装配期 fail-fast**（`config::validate_anon_paths()`）：`one_layer` 标在末段不是 `*` 的
    条目上直接报错。判据用**段级**口径（与匹配层「忽略空段」一致：`/idp/*/` 合法），
    与迁移 WARN 侧的裸后缀口径**有意不同**（那边对齐的是 v0.1.19 `strip_suffix("/*")` 的历史语义）。

**修复**

- **`oj build` 读配置失败改为出声**（`oj/src/build_cmd.rs` 的 `sql_guard_of_config` /
  `tasks_dir_of`）：此前静默回落 `sql_guard=off` + `tasks` 目录——`sql_guard` 非 Off 的项目
  会因此**静默关掉** `S*` 检查里的 tenant 声明校验（CI 门禁失效）。对象形态新增了一类
  「很容易写错、写错就是整份配置解析失败」的输入，触发面被放大，故本次补上 warn。
- **影响面判据由「模式对拍模式」改为段级判定**：原实现把条目与路由视图都送进
  `server::path_matches`，而路由视图自身可能含 `*`/`**`（`{param}` / `{*rest}` 归一而来），
  pattern-vs-pattern 会**漏报**——`/file/*` 撞上 catch-all 路由 `/file/{*path}`（视图
  `/file/**`）时静默，`/zzz/*` 撞上带参路由 `/v1/api/{x}/detail/sub` 时也静默，而两者都
  真的丢了面。

**文档**

- `docs/superpowers/specs/2026-09-19-v0.1.23-anon-path-warn-design.md`（新增）：本版设计与
  **双专家评审逐条处置**（开发侧 + 架构侧并行只读评审；已采纳 11 条 / 未采纳 4 条附理由），
  含实读证据（v0.1.19 两侧旧实现源码）与实跑验证（样例静音、未知键报错位置、fail-fast）。
- `docs/devkit/`（对外发行）：`api-manual.md` 匿名路径段改写（适用面＝仅 auth、影响面判定、
  `one_layer` 形态、对象形态只认两个键）；`scenarios.md` 场景 5 ② 改标题与正文为「只需检查
  `auth.anonymous_paths`」并补两个反例（仅差零层不告警、结构条目不告警）；`SKILL.md` 陷阱行同步。
- `docs/modules/02-config.md`：§4 校验表补 `oj build` 出声一条 + 调用点改为按函数名引用
  （原先写死的行号已漂移）；§4.1 改写为「WARN 只 auth + 两个条件 + 手写 Deserialize」。
  `docs/modules/03-server-http.md` 的通配语义段同步（收紧只在 auth、WARN 两条件、静态/blob
  不经匿名匹配）。
- `docs/tenant-guide.md`：v0.1.20 变更块限定到 auth 侧，明确租户列表不参与告警。
- `docs/oidc-integration.md` / `docs/oidc-implementation.md`：收紧范围限定 auth；补「本文
  `/idp/**` 与 sample 的 `/idp/* + one_layer` 是等价两种窄面」的互指；§8 清单措辞对齐。
- `sample/config.yaml` / `sample/config.docker.yaml`：`one_layer` 挪到 **auth** 的 `/idp/*`
  （租户侧不再需要），注释写清「租户侧自始严格一层、不参与告警」；`sample/README.md` 同步。

**测试**

- `src/config.rs`：两形态解析、归一化、段级 fail-fast、**未知键/缺 `path`/类型错报错**
  （评审 P2-3/P2-4 的钉子），共 4 条新用例。
- `oj/src/app.rs`：`anon_view_of_route`（剥 base、参数段归一）/ `is_legacy_prefix_shape` /
  `tail_entry_loses_coverage`（含评审反例：catch-all 视图 `**`、带参视图 `/*/detail/sub`
  必须告警；仅差零层的模块根路由**不**告警）/ `legacy_entries`（直接打被测函数，不复刻过滤链）/
  `migration_warn_entries`（只 auth 参与），共 5 条新用例。
- `oj/tests/oidc_e2e.rs`：匿名列表用对象形态，端到端覆盖条目解析 → 装配 → 插件 cfg 链路。
- `oj/tests/sample_config.rs`（新增）：**发行样例**必须可解析且过 `validate_anon_paths`，
  并钉住「auth 的 `/idp/*` 带 `one_layer`、tenant 的 `/idp/*` 不带」——样例 YAML 写错的
  暴露面只在装配期，没有本用例只能靠人肉跑 server 才发现。

## v0.1.22（2026-09-19）

修掉「DB 的 64 位整数值跨 JS 边界」的三重缺陷：**读侧必然 500**、**写侧静默精度丢失（假碰撞）**、
**PG 上字符串写法完全不通**。下游 U38（生产 PG 实测：`Number(max(id))+1` 坍缩 → 主键 dup 500）
即此链条。设计与证据手册见新增的 `docs/numeric-limits.md`。

**行为变更（读侧，需复查存量代码）**

- **i64 超出 JS 安全整数范围（`|v| > 2^53-1`，雪花 id 常态）时，读值由 v8 `BigInt` 改为
  十进制字符串**。旧行为下任何含此类值的 `json.ok(rows)` 都直接 500
  （`TypeError: Do not know how to serialize a BigInt`），故**无兼容性风险**（无人能依赖一个
  必然失败的行为）；但改过 `typeof id === "number"` 判断的代码需复查。
  阈值与 serde_v8 的 `MAX_SAFE_INTEGER` **逐字一致**（`(1<<53)-1`：写在 `2^53` 上会漏转 `2^53` 本身）。
  安全范围内的整数、REAL/DOUBLE 的 f64、TEXT 一律不变。`Json`/`Row` 类型无需改动（本就含 string）。

**特性**

- **新增两个 JS 全局：`toBigInt(v)` / `toDouble(v)`**（`bootstrap.js`；三个 JsRuntime 入口
  共用 `bridge_ext`，故 HTTP 池 / 任务池 / `oj test` 都可用）。**不做 `toFloat`**——JS 的
  `Number` 就是 f64，没有独立的 f32 语义。
  - `toBigInt`：十进制串 / 安全范围内的 number / bigint → `bigint`，**fail-loud**：已坍缩的
    number（`Number("<大整数串>")` 的产物，任何 `|v| > 2^53-1` 的 f64 都不是 safe integer）、
    `1.5`、`"007"`、`"+1"`、超 i64 等一律抛错并给出指引。这是 U38 陷阱在**调用点**的捕手。
  - `toDouble`：number / 数字串（含科学计数法）/ bigint → `number`，**显式接受精度丢失**。
- **写侧大整数通道**：`toBigInt()` 的返回值是真 `BigInt`（U38 范式需要 `+1n` 精确算术），
  跨 op 时由 bootstrap 的参数包装层递归编码为保留形状 `{"$oj$i64":"<十进制>"}`
  （serde_v8 的 `ValueType::BigInt` 直接 `UnsupportedType`，且其 magic/transl8 trait 是
  `pub(crate)`，外部无法自定义），宿主/插件在绑定参数时解码为 `i64`。
  解码 `marker_i64` 落在共享契约 crate `oj-plugin-ffi::jsint`（**不 bump ABI**），四处复用：
  sqlite accessor / oj-db-postgres / oj-db-mysql 的 `bind_value` 与构造器的 `to_qv`
  （必须在 `other => to_string()` 之前，否则标记被串化成文本）。严格识别（单键对象 + 规范
  十进制 + i64 内），畸形标记按普通值处理。
  - **实测依据**：PG 拒绝字符串参数写 bigint 列（`column "id" is of type bigint but expression
    is of type text`）与 `where bigint = text`；`i64` 参数则精确往返。故「字符串=文本意图、
    BigInt=整数意图」，平台不做启发式（避免误伤 TEXT 列里的长数字串）。
- **bigint 容忍面**：`json.ok` / `json.raw` / `json.fail` 的 data / `log` 结构化字段 /
  `mail.*` / 构造器 `toJSON()`/`fromJSON()`/子查询快照容器 —— 统一走新的 `ojStringify`
  （`JSON.stringify` + bigint replacer，输出十进制字符串）。其中 `json.fail` 的 data 由
  `#[serde] Value` 改为 JS 侧预序列化 + `#[string]`（原形态收到 BigInt 会 500）。
  `es.search` 响应同样过一遍读侧护栏（ES 的 `long` 字段同病）。
  快照类 API（`toJSON`/子查询嵌入）另有 `encodeSnapshot`（JSON 往返 + bigint 标记），
  以保留 `Date → ISO 串` 等原 JSON 语义——参数编码器 `encodeParams` 对带 `toJSON` 的对象
  同样遵循 JSON 语义（否则 `Date` 会被压成 `{}`）。二进制参数（`Uint8Array`/`ArrayBuffer`）
  显式报 `TypeError`（改动前为 serde_v8 类型错，行为一致、消息更清晰）。
- **读侧护栏补齐两处出口**（评审发现）：`jwt.verify` 的 **claims 外部可控**（雪花量级的数值
  声明会让 `json.ok(claims)` 500）与 `mail.result` 的结果体。`jsnum.rs` 模块注释给出
  **op 出口覆盖清单**（已覆盖 6 处 + 有意不覆盖 4 类及理由），供后续新增 op 时对账。
- **租户守卫接受三种等值形态的租户 id**：字符串 / 数字 / 大整数标记
  （`param_is_tenant`）。此前只认字符串，雪花租户 id 用 `toBigInt()` 传会被
  `sql_guard: deny` 误拒（`params must include current tenant id`）。`mentions` 要求不变。

**修复**

- **env-gated 真库用例不可重跑**（既有）：`oj-db-postgres` / `oj-db-mysql` 的
  `real_*_roundtrip_via_vtable` 只 `create table if not exists` + 插入固定 id，上一次运行
  的残留行会让**重跑撞主键**（本机第二次跑 PG 用例时暴露）。两处补 `drop table if exists`
  清场，用例恢复幂等（PG 已实测连续两次绿）。
- **`value_to_json` 的 u64 回绕**：`Qv::BigUnsigned(Some(u)) => Value::from(u as i64)`
  在 `u > i64::MAX` 时静默变成负数 → 改为 u64 直出（超界部分由读侧护栏降为十进制字符串）。
- **Windows 门禁编译失败：v0.1.20 新用例误用 unix-only 辅助函数**（本次 CI 暴露）：`oj/tests/e2e.rs`
  的 `test_db_override_builds_schema_on_test_db_not_dev` 及其 `has_table` 助手三处直接调用
  `#[cfg(unix)] fn fwd`（该 cfg 正是 v0.1.19 为消 Windows dead_code 告警加的），Windows 上退化为
  「cannot find function `fwd`」→ `oj` e2e 目标编译失败。三处 DSN 一律改走
  `oj_plugin_ffi::path_util::sqlite_file_dsn`（与 `fwd` 自带文档的红线一致：DSN 不走裸 replace）。
  复核方法：把全仓 `#[cfg(...)]` 里的 `unix` 逐处置换成 `windows` 后 `cargo check --workspace
  --all-targets`——等价于「unix-only 项全部缺席」的 Windows 形态，结果零 error（`fwd` 是唯一一处，
  `logging.rs` 的 `cfg(all(test, unix))` / `server_cmd.rs` 的 `daemonize`·`term` 均有 `not(unix)`
  对偶分支）。
- **CI：三平台矩阵「一红全跑」的资源成本**（`.github/workflows/plugin-matrix.yml` / `release.yml`）。
  `plugin-matrix.yml` 新增派发输入 `platforms`（`all`/`linux`/`macos`/`windows`，逗号分隔）与
  `shared_jobs`（`false` = 再跳过 fmt+clippy、sample L1/L2 两个平台无关 job），由新增的 `select`
  job 生成动态 `matrix.include`——修 Windows 就只生 Windows 那条腿，另两条**根本不生成作业**；
  另配 workflow 级 concurrency（group = `ref` + 平台选择，`cancel-in-progress`）取消被取代的旧矩阵。
  两个 workflow 均加 `actions/cache`（只缓 crate 源码/索引与 rusty_v8 预编译库；**不**缓存
  `target/`——本仓 release target 实测 12G+，三平台相加超 GitHub 每仓 10G 上限会互相 LRU 挤出）
  与各 job `timeout-minutes`（挂死不再默认烧满 6h）。`release.yml` 保持三平台全跑（发行物必须齐全），
  单平台出错靠 `fail-fast: false` + 缓存支持「Re-run failed jobs」只重跑那一条腿。
- devkit 顺带订正：`oj test` 旗标表补 `--db` / `--anonymous`（v0.1.20 漏同步）、
  已知限制表「静态站点无 SPA 回退」改为「SPA 回落需显式开 `server.app_spa_fallback`」。

**文档**

- 新增 **`docs/numeric-limits.md`**（专题手册：契约与阈值表、`toBigInt`/`toDouble` 接受-拒绝
  矩阵、绑定类型规则、U38 范式与真实数字、代价与边界、**存量代码自查清单（含 grep）**、
  排障表、**双专家评审意见与逐条处置**）。
- devkit（发行交付物）：`api-manual.md` §6 新增「大整数与 i64」小节（含保留键红线）+ 总表行、
  §13 限制表与陷阱清单；`SKILL.md` **红线新增「大整数」一条**（U38 正是 agent 易犯的写法）、
  陷阱速查 4 行、场景清单；`scenarios.md` **新增场景 7「雪花 id（大整数）的生成与回写」**；
  `README.md` 场景清单与手册特性描述。`db-guide.md` §11 红线第 6 条 + 报错速查 5 行。
- `sample/global.d.ts` 补 `toBigInt` / `toDouble` 声明（含 fail-loud 语义注释）。

**已知债 / 另案登记**

- **PG 语句缓存与混合参数类型（既有隐患，非本次引入）**：同一条 SQL 文本若在不同调用里绑定
  不同 Rust 类型的参数（字符串 ↔ 数字/大整数），sqlx prepared statement 缓存会给出协议级错误
  （`invalid byte sequence for encoding "UTF8": 0x00` / `insufficient data left in message` /
  `incorrect binary data format in bind parameter`），且与执行顺序相关。**v0.1.21 用纯数字+字符串
  即可复现**，与本次改动无关；已记入 `docs/numeric-limits.md` §4.5 与 devkit 陷阱表待专项处理。
- `u64` / `BIGINT UNSIGNED` 全链路未支持（读侧不再回绕为负数，但精确性不保证）。
- **数值型租户 id 列不受支持（既有）**：守卫注入的是字符串条件、insert 要求 `tenant_id` 为
  等于租户头的字符串，故 `tenant_id` 列为 BIGINT/INTEGER 时 PG 报
  `operator does not exist: bigint = text`（详见 `docs/numeric-limits.md` §4.8）。绕行：租户 id 用 TEXT。
- 内建序列分配原语（`max+1` 的并发竞态终态）未做。
- MySQL 侧真库验证未执行（本机拉不到镜像，用例已 env-gated 写好，`OJ_TEST_MYSQL` 可用时即跑）；
  PostgreSQL 侧已用真实 PG 18 跑通（含 `i64::MIN` 往返、`where` 标记比较、字符串负例、事务路径）。

## v0.1.21（2026-09-19）

**特性**

- **`oj migrate` / `oj fixture` / `oj schema diff` 新增 `--db <profile>`（多库运维闭环）**：
  三处瘦身装配（`migrate_cmd.rs::slim`）原先硬编码 config `db:` 段的 `default` 键，
  多库项目里非 default 的命名库**无法**走工具链——只能手工执行 SQL，账本
  `_oj_migrations` 与 `schema.yaml` 收敛都缺位。
  - `--db <name>` 即 config `db:` 段的键（`db: {default: …, analytics: …}` →
    `--db analytics`）；缺省仍为 `default`，**选库语义与 v0.1.20 一致**（`default` 缺失时
    的提示文案与收尾行措辞有更新，见下）。
  - **未声明的库名 fail-fast**，报错列出可用键（`--db "x" not declared in config
    (db keys: [...])`），绝不静默回落 default——与 `oj test --db`（v0.1.20）和
    `App::from_config` 的 `db_override` 同一条纪律：静默回落等于把迁移打在开发库上。
  - 语义不变：`--db` 只是换目标连接；账本、reconcile、`--baseline`、`--module`
    均**各库独立**。迁移工具**不读**模块级 `manifest.yaml` 的 `db:` 绑定（那是运行期
    路由，`src/bridge/guard.rs::bound_db`），故 `--db` 是**整轮**迁移的目标库而非逐模块
    解析——单一 profile 的项目逐库各跑一遍即可；模块绑定不同库的项目见下条「已知债」。
  - `oj migrate` 收尾行打印目标库（`… → db "analytics"`），避免多库下跑错库无察觉；
    `--db` 缺省时提示文案改为「迁移/fixtures/对账需要一个目标库」并列出已声明键
    （原文案写死 "（迁移/fixtures 作用于 default 库）"）。
  - **已知债（本次只登记不修）**：`--db` 是**整轮**目标库，`slim` 不解析模块级
    `manifest.yaml` 的 `db:` 绑定——多库项目须 `--db <profile> --module <M>` 逐组合跑，
    否则会把全部模块的迁移灌进同一库（静默重复，D002 不报）。登记于
    `docs/migration.md` §8 与 `docs/modules/04-oj-cli.md` §8。
  - 文档：`migration.md` 新增 §3.8「多库：`--db` 指定目标 profile」、改写 §6 限制 7
    （原「只作用 default 库」），`db-guide.md`（§1.1 库级迁移）、`cli2.md` /
    `user-manual.md` 旗标与用法、`ops-manual.md` 发布流程与排障表（新增
    `--db "x" not declared` 一行）同步。**devkit（发行交付物）**：`api-manual.md`
    命令表 / 交付跑法 / 打包部署步骤 / §10 db 段 / 排障表 / 已知限制全表，
    `SKILL.md` 发布检查与陷阱速查（`--db` 三条），`scenarios.md` **新增场景 6
    「多库项目按库迁移与对账」**（含整轮语义的踩坑），`README.md` 场景清单同步；
    顺带订正 devkit 两处陈腐描述：`oj test` 旗标表补 `--db` / `--anonymous`（v0.1.20 漏同步）、
    已知限制表「静态站点无 SPA 回退」改为「SPA 回落需显式开 `server.app_spa_fallback`」。

## v0.1.20（2026-09-19）

本版来自下游（plane）上游需求清单（U1–U37）的逐条回源码复核，交付四项：**匿名访问双层**
（路径通配统一 + `db.asTenant`）、**静态托管两条**（SPA 回落 + 每路由 meta 注入）、
**`oj test` 测试库隔离**、**构造器 LIMIT 可配与截断可观测**。设计文档（含 20 条专家评审处置）
见 `docs/superpowers/specs/2026-09-18-v0.1.20-upstream-support-design.md`。

**特性**

- **匿名访问：路径通配四形态统一 + `db.asTenant`（下游 U2，P0 阻塞点）**。下游痛点是双层阻塞：
  豁免路径匹配能力不足（深层 OIDC 路径进不来），且即便豁免进得来，handler 查租户表仍被
  `sql_guard` 拦（`tenant_id=None`）。
  - **通配四形态**（`server::path_matches` 与 `plugins/oj-auth` 的 `is_anonymous` **同语义**，
    两处实现互指注释 + 各自矩阵测试）：字面全等 / 尾 `/*` **严格一层** / 中段 `*` **恰好一段** /
    `**` **跨任意层**（含零层，`/idp/**` 命中 `/idp`）。段切分后回溯匹配，`%2e%2e`/`..` 段
    照旧在解析前被拒。
  - **`RequestInfo.anonymous`（新字段）**：**仅** server 的「命中 `tenant.anonymous_paths`
    且确实没带租户头」分支置 true。`tenant_id.is_none()` 有五个来源（租户未启用 / 豁免命中 /
    WS 帧 / 任务桥 / `oj test`），不能当匿名判据——用它会把 WS 与任务路径一并放行。
  - **`db.asTenant(id)`（新 op + JS 面，`db` / `DB(name)` / `tx` 内实例同面）**：匿名请求
    **声明**租户身份。与 `db.asSystem()` 相反——**防护机制不变**（构造器仍强制注入 `tenant_id`、
    裸 SQL 仍要求 mentions+param_has），只是身份由 handler 给出。三道 **fail-closed** 门禁：
    ① `tenant.allow_as_tenant: true`（新键，**默认 false**）；② 请求为匿名；③ `id` 非空。
    不满足即**抛错**（`nofast` op，拒绝原因必须回到 JS）。**请求级且只能设一次**（已带租户头 /
    二次调用 → 抛错，防运行期切换身份），调用打审计日志。
  - 定位是**授信 handler**：平台无从校验 id 是否为真实租户，红线是 id 必须服务端派生
    （token → 查表 → 租户），绝不可直接取 URL 参数（见 `scenarios.md` 场景 1 / §8 风险）。
- **静态托管：SPA 深链回落 + 每路由 HTML meta（下游 U1）**。原静态兜底「未命中即 404」，
  SPA 深链接刷新不可用；也没有任何 per-route 模板能力。
  - `server.app_spa_fallback`（**默认 false**）：未命中 + 无扩展名 + `Accept` 含 `text/html`
    或缺失或 `*/*`（curl 默认）+ **不在 `api_prefix` 下** → 回落 `index.html`。排除 API 前缀是
    硬约束：`app_prefix="/"` 时否则会把拼错的 API 路径吞成 200，掩盖真实 404。
  - `server.html_meta`（目录名，**默认关闭**）：送出 HTML 时按请求路径读
    `<app_path>/<html_meta>/<path>.json`（`/` 与目录 → `index.json`），把白名单键注入
    `</head>` 前：`title` / `description` / `canonical` / `og:*`（`property`）/ `twitter:*`
    （`name`）。**只注入标签、绝不注入脚本**（静态响应拿不到 CSP nonce），值一律 HTML 转义；
    未命中 / 无 `</head>` / JSON 非法 → 原样返回（零副作用）。meta 目录自身**不对外公开**
    （`resolve_static` 对该目录返回 None）。不做 SSR——产物由构建期离线生成。
- **`oj test` 测试库隔离（下游 U33）**：原 `oj test` 复用的 `db.default` 就是开发库，下游实测
  读到 1000 行非种子数据、甚至写坏开发库。
  - `App::from_config` 收 `db_override` 并在**装配期**算出 `db_key`，`migrate.apply_all/verify_all`、
    `build_schema_and_modules`（schema 内省）、`seed.replay_all`、`load_fixtures` **四处全部跟随**
    ——只改运行期 `bound_db` 会让 test 库无表无种子，比现状更糟。
  - `oj test` 默认策略：config 声明了 `db.test` ⇒ 用它；否则醒目 WARN 后继续 `default`（不静默）。
    新增 `--db <name>`（未在 `db:` 段声明即 fail-fast）与 `--anonymous`（以匿名请求身份跑，
    便于测 `anonymous_paths` 覆盖的公开面）。启动打印 `oj test: using db "..."`。
  - 运行期 `bound_db` 解析顺序：**显式 `DB("name")` → manifest `db:` 绑定 → `db_override` →
    `"default"`**（`StableState.db_override` 经 `Extras` 注入）。
- **构造器 LIMIT 可配 + 截断可观测（下游 U30）**：顶层 select 原隐式 `LIMIT 100`、显式 limit
  被 clamp 到 1000，**截断完全无信号**（下游「>100 条静默少数据」）。
  - 新配置段 `db_query: { default_limit, max_limit }`（默认 100 / 1000，硬顶 100000）。
    装配期校验 `1 ≤ default_limit ≤ max_limit ≤ 100000`，`0` 与倒置区间 fail-fast；
    结构体 `deny_unknown_fields`——键名拼错（如 `default`）**启动即报错**，不静默取默认值。
    **注意不能写进 `db:` 段**（`db` 是 name→DSN 的 map，键即库名）。
  - 归一化在 **op 层**（`normalize_limit`，不穿透 `build_statement` 递归签名）：顶层
    `limit=None` → `default_limit`；显式 → `min(limit, max_limit)`。保留「嵌套/子查询
    不隐式截断」语义。`toSQL()` 走同款归一化（诊断与执行口径一致）。
  - 截断信号：`applied = min(显式或默认, max_limit)`，结果行数 ≥ `applied` ⇒ 响应带
    `X-OJ-Row-Limit: <applied>` 头（含「显式 limit 被 clamp」这类旧版静默情形）。
    **信封形状不变**（`{code,msg,data}` 契约不动，下游 12 个生成 client 零改动）。
- `--db` / `--anonymous` 同时补入 `oj test --help` 与 `docs/testing.md` 旗标表。

**修复**

- **oj-auth 与 server 的匿名通配语义分叉**：`plugins/oj-auth` 的 `is_anonymous` 原为
  `starts_with` 前缀匹配（尾 `*` = 任意深度），与其自身注释及 `docs/builtin-api-auth.md` 的
  「一层通配」矛盾，也让「同一份 `anonymous_paths` 在租户与鉴权两道守卫下表现不同」。
  统一为四形态（与会话无关的纯段匹配），两处实现注释互指，各补通配矩阵测试。
- **`sample/package.json` 的 `test:api` 路径写错**（既有缺陷，本次接线 `npm test` 时暴露）：
  npm 以**包目录**为 CWD 执行脚本，脚本里的 `./bin/oj` 与 `-c config.yaml -d src` 却按
  「CWD = 仓库根」写 → `npm run test:api` 在最后一步必失败
  （`sh: ./bin/oj: No such file or directory`）。改为 `cd .. && … ./bin/oj test -c
  sample/config.yaml -d sample/src`（与 CI 命令同形）。此前未暴露是因为 CI 直接在仓库根
  调 `./bin/oj`，本地统一入口少被走。
- **`oj test` 运行时装配缺口**：`StableState` 新增字段在 `with_dbs_and_loader` / 测试夹具 /
  `oj/src/app.rs`（`oj test` 直建 runtime）三处同步，避免 server 与 `oj test` 行为分叉；
  `oj test` 的 `op_client_dispatch` 显式构造 `RequestInfo { anonymous, ..Default::default() }`。
- **类型面（`.d.ts`）与运行时/样例对不齐**：用 `tsc -p sample/tsconfig.json` 实测，样例在
  「本手册宣称 `global.d.ts` 是类型权威」的前提下有 **30 处报错**（此前无类型门禁，属静默腐烂）。
  - `QueryBuilder.all()` 声明为 `Promise<Json[]>`，而运行时返回的是**行**（列名 → 值）——
    于是 `rows[0].password_hash`、`{...row}` 全部报错（20 处，散在 auth/idp/oidc/cert/order）。
    改为 `Promise<Row[]>`（与 `db.query` 同形）；`db.tx` 的回调参数改为 `TxInstance`
    （`Omit<DBInstance, "tx">`——嵌套事务被运行时拒绝，原声明却让 `tx.tx(...)` 通过类型检查）。
  - **`sess` 未声明**（`Cannot find name 'sess'`，2 处）：WS 帧池的会话上下文
    （`sess.id` / `sess.state`，由 `ws_connect` driver 注入）此前只在 API 手册里，类型面缺失。
    新增 `WsSess` 接口 + `declare global { const sess }`，并写明 `state` **必须可 JSON 序列化**
    （不可序列化时该帧状态回传被丢弃，连接不中断——对应 `frame_pool.rs` 的 `Option<Value>` 语义）。
  - `client.ws().next()` 的三态（帧 / `{closed: true}` / `null` 超时）原声明为
    `TestWsFrame | { closed: true } | null`，调用方取 `binary`/`data` 必然报 2339（10 处）。
    改为带可选判别位的 `TestWsFrame | TestWsClosed | null`（`closed?: false`），
    样例测试加 `expectFrame()` 收窄（顺带把「拿到非帧」从 TypeError 变成显式断言失败）。
  - 样例随类型收紧补 3 处 `String(...)`（`Row` 的列值是 `Json`，直接传 `string` 参数不合法）。
  - `sample/tsconfig.json` 的 `include` 补 `unit/**/*.ts`（L2 spec / mock 此前不在任何 tsconfig
    内，编辑器取不到 `json`/`db` 等全局类型），`exclude` 补 `unit/vitest.config.ts`
    （其 `node:fs`/`vite` 依赖需要 `@types/node`，本工程未安装）。
  - L2 mock（`sample/unit/mocks/oj-globals.ts`）补 `asSystem` / `asTenant`（与 bootstrap 同形，
    返回同一实例），否则写了 `db.asTenant(id).table(...)` 的 handler 在 L2 里 TypeError。
  - `docs/devkit/api-manual.md`：`.all()` 的返回标注由 `Promise<Json[]>` 改为 `Promise<Row[]>`
    （总表本已列出 `sess.id / sess.state`，类型面此前缺失）。
  - 验证：`tsc -p sample/tsconfig.json` **0 error**；`vitest run` 12/12 通过；
    `./bin/oj test -c sample/config.yaml -d sample/src` 43/43 通过。
  - **类型门禁进 CI**：`sample/unit` 加 `typescript` devDependency（钉 `5.6.3`，随
    `npm ci` 安装）与 `npm run typecheck`（= `tsc -p ../tsconfig.json`）；
    `sample/package.json` 加同名脚本（委托 unit）并纳入聚合入口
    （`npm test` = `typecheck` + `test:unit` + `test:api`）。CI 两处挂载：
    `plugin-matrix.yml` 的 `sample-tests`（安装/类型检查顶到昂贵的 Rust 构建之前，
    fail fast）与 `release.yml` 的 `lint`（tag 推送发版前必过，job 更名
    `fmt + clippy + typecheck`）。负向实测：注入一处类型错误 → 退出码 2、报错定位到文件；
    移除后退出码 0。

**行为变更（三项，升级前请核对）**

1. **`oj test` 默认库 `default` → `test`**：config 声明了 `db.test` 的项目，测试数据将从
   `default` 改落 `test`（建表/seed/fixtures 一并跟随）。理由：写坏开发库是真实事故。
   不需要此行为时显式 `--db default` 即可。
2. **匿名路径尾 `*` 由「任意深度」收紧为「严格一层」**：受影响的是依赖旧 oj-auth 深前缀行为
   的 `auth.anonymous_paths` / `tenant.anonymous_paths` 条目（如 `/idp/*` 曾命中
   `/idp/.well-known/openid-configuration`）。启动期对含尾 `/*` 的列表打**聚合 WARN** 提示
   改 `**`（不静默收回授权面）。仅需一层的老配置无需改动。
   > **v0.1.23 订正**：这次收紧**只发生在 oj-auth 侧**（`auth.anonymous_paths`）。
   > `tenant.anonymous_paths` 自引入起即为严格一层——v0.1.19 的 `server::path_matches` 已是
   > `!rest[1..].contains('/')`，租户豁免（`server/src/lib.rs:364`）走的就是它。本条的
   > 「两条列表并列」措辞不精确；v0.1.23 起迁移 WARN 也只对 `auth.anonymous_paths` 生效。
3. **SPA 回落默认关闭**：`server.app_spa_fallback` 默认 `false`，需显式开启（不复用旧行为，
   避免 `app_prefix="/"` 的存量部署突然开始吞 404）。

**兼容性**

- `ABI_VERSION` 保持 **8**（无 repr(C) vtable 形状变更；`db.asTenant` 走既有 db 轴 op）。
- 新增/改动配置键一律 `#[serde(default)]`：`tenant.allow_as_tenant`、`server.app_spa_fallback`、
  `server.html_meta`、`db_query.{default_limit,max_limit}` —— 老配置零改动（除上述三项行为变更）。
- 首方插件版本保持随发布统一（`0.1.0`，semver 门禁为可选 pin，无清单 pin 该值）；
  `oj-auth` 行为变更已写入 `CHANGELIST` + `docs/builtin-api-auth.md`。
- `cargo build --release` / `cargo fmt --check` / `cargo clippy --release --all-targets -- -D warnings`
  / `cargo test --release --workspace` 全绿。

**文档**

- `docs/devkit/scenarios.md`（**新增**，随发行包 `devkit/` 分发）：场景速查——公开分享页匿名读
  租户数据 / SPA 深链回落与每页 meta / 测试库隔离 / LIMIT 分页陷阱 / 匿名路径通配，
  每篇「配置 + 代码 + 验证 + 常见坑」。
- `docs/devkit/api-manual.md`：API 表补 `db.asTenant` 与三道门禁；LIMIT 段改 `db_query` 可配 +
  `X-OJ-Row-Limit`；tenant 配置示例补 `allow_as_tenant` 与通配四形态；首页加场景集指针。
  `docs/devkit/{README,SKILL}.md` 同步索引与陷阱速查；`tools/xtask` 的 devkit 发行契约测试
  增 `scenarios.md` 断言。
- 过期描述订正：`docs/modules/02-config.md`（新键）、`docs/modules/03-server-http.md`（静态
  9/10/11 步 + 通配表格）、`docs/db-guide.md`、`docs/dev-guide.md`、`docs/user-manual.md`、
  `docs/tenant-guide.md`（asTenant 场景 + 通配 + 豁免说明）、`docs/testing.md`（`--db` /
  `--anonymous` / 默认 test 库）、`docs/modules/00-overview.md`、`docs/builtin-api-auth.md`、
  `docs/oidc-integration.md`、`docs/oidc-implementation.md`、`sample/config.yaml`（新键注释）、
  `sample/global.d.ts`（`DBInstance.asTenant`）。

## v0.1.19（2026-09-15）

**特性**
- **mail 轴：邮件投递（新插件 `oj-mail` + JS 全局 `Mail`/`mail`）**。顶层 `smtp:` 段存在即启用；
  段**一段两用**：整段（含凭据）经 `plugin_cfg` 适配器臂交给 `oj-mail` 插件建 transport，
  宿主另只吸收每个 profile 的 `allowed_from`/`allowed_recipients` 做前置校验——凭据不进宿主
  JS 面。**走适配器臂而非 `plugins:` 透传**：`plugins:` 非空即切严格清单模式，用它会顺带把
  运维的插件装配模式切掉（须列全所有插件）。
  - **多 profile**：`smtp:` 顶层除 `workers`/`queue_capacity` 外每个键都是一个 profile
    （键 = `Mail(key)` / `mail.send` 的 key）；未声明的 key 显式报错（不回落 default）。
  - **四种调用**：`mail.send`（异步 transport）、`mail.sendSync`（同步 transport，worker 内
    `spawn_blocking`）、`mail.enqueue`（入队即回**统一信封** `{code:0,data:{jobId}}`——**非**裸
    `jobId`；真实完成经 `mail.result` 与 bus 上送）、
    `mail.sendRaw`（RFC5322 原文投递，与 `attachments` 互斥）。宿主按方法**覆写** req 里的
    `sync`/`enqueue_only`（JS 侧串用无效）。
  - **队列 + worker 池**：有界队列（`queue_capacity`，满即**背压** `code:4`，不无界堆积）+
    `workers` 个 worker 串行投递；停机 graceful drain。**双通道反馈**：同步路（future resolve
    = 投递结果）与异步上送（`HostContext.deliver("mail.result")` → 宿主按白名单扁平化后存下
    （限长 + TTL）+ 经 bus 扇出给订阅者；`to`/`subject` 等一律不进反馈帧）。
  - **引用式附件**（字节不进 JS、不走 base64）：`{filename, blobKey|blob}`（blob 后端）或
    `{filename, path}`（项目根内本地文件，`ensure_within` 钳制 + canonical 句柄读盘防 TOCTOU），
    二选一；MIME 决议 = 显式 → 扩展名 → 魔数嗅探 → `application/octet-stream`。
  - **rustls 三模式**：`tls`（隐式 TLS）/ `starttls`（强制升级，不回落明文）/ `none`（明文，
    须显式 `allow_none_tls: true`，fail-closed）。认证 `login`（user+pass）或 `xoauth2`
    （静态 `access_token`；只给 `refresh_token` fail-loud）。lettre 0.11 + rustls 0.23.40
    单一版本、无 ring（provider 与框架同为 aws-lc-rs）。
  - **白名单 fail-closed**：`allowed_from`/`allowed_recipients` **全等**匹配（大小写不敏感；
    条目 = 完整地址或 `@domain`，不做子域通配），**空表 = 拒绝**——白名单是「越权发送」的
    唯一控制点，缺省放行等于开放中继。宿主侧 CRLF 一律**剥离**（subject/headers）或**拒绝**
    （地址）；`headers` 不得覆盖 From/To/Cc/Bcc/Subject/Sender/Return-Path/Reply-To
    （防白名单绕过与弱 spoof）。校验失败一律 `{code:5}` 信封（不抛异常）；
    仅「未配置 mail」抛错。附件另有单件/单封上限（`smtp.max_attachment_bytes` /
    `max_total_attachment_bytes`，超限 `code:5`）。
  - **FileTransport**：`file_transport: <dir>` 给定时不发网络，`.eml` 落盘（测试/归档通道）。
    `oj-mail` 的引擎与 transport 都在插件内，宿主零新增依赖（只 `lettre::Address` 做地址校验）；
    `ABI_VERSION` 保持 8。
  - **CI/工具**：`tools/xtask` 的 `PLUGINS` 增 `"mail"`（CI 矩阵与归置的单一真相源，不硬编码
    副本）；`cargo xtask plugin mail --check` 可预检。JS 类型面见 `docs/devkit/api-manual.md`
    第 6 章 mail 小节。

**修复**
- **mail：enqueue 契约 / worker 强引用释放顺序 / 停机 drain 可达 / 忙等**
  - **`enqueue` 回统一信封**：插件引擎原回**裸** `{jobId}`，与三层公开契约
    （`global.d.ts`、`api-manual` §6、`mail-smtp.md` §6「`res.data.jobId`」）不符 ——
    用户按手册写 `res.data.jobId` 会 TypeError 且拿不到 jobId 去 `mail.result()` 回查。
    引擎改回 `{code:0,msg:"ok",data:{jobId}}`；宿主加**纵深防御**（插件返回顶层无 `code`
    时包成信封，容忍旧/第三方裸载荷）；新增**真插件** e2e（`oj/tests/mail_e2e.rs` 的
    enqueue 用例：统一信封 + 以同队列 `send` 作屏障后 `mail.result(jobId)` 必命中）。
  - **停机 graceful drain 生产可达**（Important）：`MailEngine::shutdown` 此前只被测试调用
    （`allow(dead_code)`，引擎是插件内 `OnceLock` 单例、进程退出不 Drop）⇒ 生产停机实际**丢在途
    邮件**，与文档宣称矛盾。改为**零 ABI 变更**的控制报文 `{"__ctl":"drain","timeout_ms":N}`
    （走既有 `MailVtable::submit`），宿主在停机路径（`server_cmd` 的 SIGTERM/正常退出，
    HTTP 停收 + 任务收场之后）经 `App::drain_mail` 调用；总超时 10s，超时告警不阻断退出。
    新增 `oj/tests/mail_drain_e2e.rs`（真装配 + 真插件：排空后新投递被拒，证明报文到达插件）。
  - **worker 强引用释放顺序**（Important）：`async move` 块的捕获变量只在 **future 被 drop**
    时释放（实测），故 worker 帧里的 transport 强引用晚于退出信号释放 —— 注释声称的顺序保证
    不成立。改为在发退出信号**之前**按依赖序显式 `drop(targets)/drop(deliver)/drop(rx)`，
    使 transport 的最后一份强引用销毁点确定落在 `rt.enter()` 内（不再依赖 tokio 回收任务的
    实现细节）；补「在途 job + 真 pool transport + Drop/drain 超时两条路径」回归护栏。
  - **`submit` 忙等烧核**（Important）：宿主 `FfiMailBackend::submit` 原用 `await_ffi`
    （`yield_now` 空转）驱动插件 future，而 SMTP 往返可达 `timeout`（默认 30s）⇒ 每秒把该
    isolate 的 `current_thread` runtime 空转烧满一核。改用 `await_ffi_poll`
    （`FFI_POLL_BACKOFF = 2ms` 退避；poll/take/free 与取消语义同 `await_ffi`），
    补用例钉住「pending 期间必须退避睡眠」（变异：换回 `await_ffi` ⇒ 20 次 pending 仅 409µs，红）。
  - **文档↔实现漂移与死代码**（Minor）：
    - `src/bridge/bootstrap.js`（注释，7-bit ASCII）与 `sample/global.d.ts` 曾把 `code:2`
      （SMTP 5xx）/`code:3`（鉴权）写成可用；实际**保留未启用**（均归 `1`）→ 两处改为
      「RESERVED / 保留未启用」，与 `docs/mail-smtp.md` §3 一致。
    - `src/bridge/mail.rs` 的 `resolve_attachments` 删除死变量 `hint`（含 `let _ = &hint;`）。
    - 插件 `message.rs` 的 `SendRequest.subject` 注释原说「raw 路原文 `Subject:` 会被剥离」，
      与实现（**保留**，结构化非空时覆盖）矛盾 → 订正为与 `build_raw` 一致。
    - 插件 `engine.rs` 的背压文案 `"queue full"` → 中文（「队列已满…下一步…」），
      `code:4` 语义不变；用例改为钉住新文案。
    - `src/bridge/ffi.rs` 的 `host_deliver` 原把「载荷非法」与「mail 未配置」都打成
      「mail 未配置」→ 细分 `DeliverRoute`（`Routed`/`NotConfigured`/`BadPayload`）分开告警。
- **mail：白名单匹配语义 / 附件上限 / 结果归属 / 裸 CR / 弱 spoof 头 / 文案**
  - **白名单匹配语义可被绕过**：原实现对条目做裸 `ends_with` 后缀匹配，三处
    越权面：① `allowed_from: ["noreply@x.com"]` 放行同域仿冒 `evil-noreply@x.com`；
    ② 漏写 `@` 的条目（`["x.com"]`）放行跨域 `a@evilx.com`；③ `[""]`（空条目）令
    `ends_with("")` 恒真 = **白名单等于关闭**。改为**全等**匹配（条目只能是完整地址
    （地址全等）或 `@domain`（域全等），大小写不敏感；**不做子域通配**，子域须显式
    `@sub.x.com`），并在**装配期**逐条校验条目格式（空串/裸域/首尾空白 → 启动失败，
    文案点名 `smtp.<profile>.<字段>[<下标>]` + 下一步）；匹配期非法条目 fail-closed。
  - **附件无大小上限 + 同步读盘**：新增 `smtp.max_attachment_bytes`
    （默认 10 MiB）与 `smtp.max_total_attachment_bytes`（默认 25 MiB），超限 `code:5`；
    `path` 路先取长度再读（超限文件不进内存）；读盘 `std::fs::read` → `tokio::fs::read`
    （内部 spawn_blocking，不再阻塞 isolate 的 `current_thread`）；插件侧再复核一次
    （纵深防御）。**背景**：附件字节由宿主读盘后经有界队列（容量 256）持有，无上限时
    project root 内任意大文件（含 `config.yaml` —— 里面有 `jwt_secret`/smtp 口令）可被一次
    调用读入内存并放大成内存 DoS。
  - **结果通道可枚举/可覆写**（Important，B3）：① `mail.result(key, jobId)` 原**忽略 key**
    且无归属校验；② jobId 由**插件**生成（`{pid}-{seq}`，可猜）；③ 宿主不剥调用方自带的
    jobId，同 id `put` 覆盖 → 任意模块可读他人结果并**覆写成 `code:0`**。改为：宿主生成
    jobId（每进程随机 16 hex 前缀 + 单调计数，**不可猜**；调用方传入值一律剥离；回执的
    `data.jobId` 钉成宿主值）；enqueue 路在 submit **之前**登记一张带归属（profile + 模块 +
    租户）的票，插件上送只能**填充一次**（未登记/重复一律拒绝，`DeliverRoute` 增
    `UnknownJob`/`Duplicate` 分别告警）；`mail.result` 只回本归属的结果（跨 profile/模块/
    租户 → `null`）。插件侧兜底 jobId 同步改为随机前缀形态。
  - **裸 CR 未中和（SMTP smuggling 半开）**（Important，B4）：`normalize_crlf` 原只补 LF 前
    的 CR、**保留裸 CR**，正文含 `X\r.\r\n` 时以裸 CR 为行界的接收端会提前结束 DATA、余下
    内容被当命令执行（可注入伪造 MAIL FROM/RCPT TO）。改为三种行尾（CR/LF/CRLF）**一律归一
    CRLF**（口径与既有「裸 LF → CRLF」一致；拒绝裸 CR 会让同类行尾有两种相反处置），并覆盖
    结构化 `text`/`html`；**头区**的裸 CR 仍**拒绝**（归一它等于凭空造出一个头）。
  - **弱 spoof 头面**（Important，B5）：结构化 `headers` 禁覆盖清单由
    From/To/Cc/Bcc/Subject 扩到含 `Sender`/`Return-Path`/`Reply-To`（宿主与插件同清单）；
    raw 路剥离清单增 `Sender`/`Return-Path`（`Reply-To` 保留：raw 是调用方自备原文、它不改变
    信封与发件人身份）；**raw 与结构化 `headers` 互斥**由「静默忽略」改为 fail-loud `code:5`。
  - **文案与告警**：白名单未命中的文案不再 `{:?}` 回显**整份白名单**（换个
    profile key 即可枚举他 profile 的内域/客户域），只回「未命中 + profile 名」+ 下一步；
    `messageId` 由 MTA 原始应答改为「队列号或截断到 64 字符」（去服务器指纹/队列信息）；
    `tls: none`（明文）的 profile 在启动时打 **warn 级**告警（含 profile 名与 host:port，
    点明「内网 CIDR 限制未实现」；`file_transport` profile 不告警）；文档明确
    **`code != 0` 一律不得自动重试**（`2`/`3` 未启用，永久失败与瞬时失败同归 `1`，
    从 `code` 分不出可重试性），未新增 `data.retryable`（见 `docs/mail-smtp.md` §3 的取舍说明）。
- **IDE 类型：`#` 别名报 TS2307、`QueryBuilder`/`json` 声明滞后**（`sample/global.d.ts`、
  `sample/tsconfig.json`、`sample/types/oj-modules.d.ts`）：
  - `QueryBuilder` 补 `join`（`{left,right}[]` + kind）/`distinct`/`groupBy`/`having`/`union`/`with`/`toJSON`；
    `DBInstance` 补 `leaf`/`and`/`or`/`not`/`fromJSON`/`asSystem`；`JsonApi` 补 `raw`（裸 JSON 200）；
    `WhereCond.field` 改可选并补 `not`（纯组合节点，与运行期 `condObj` 一致）。
  - **`#` 别名 TS2307**：`#x`/`#/m/x` 是「引用方模块」相对，TS `paths` 只能静态枚举模块目录，
    未创建或不在枚举内的别名（如 `#_shared/view`）报 TS2307。修法：新增**非模块** `.d.ts`
    （`sample/types/oj-modules.d.ts`）内的 `declare module "#*";` 作真 ambient 兜底——命中真实
    文件仍走真类型，未命中降级为 `any`（放在模块化的 `global.d.ts` 里无效，`paths` 命中时
    ambient 也被忽略，均为实测）。该文件随 devkit 分发（`copy_devkit` 增归置 + 守卫测试），
    业务项目须与 `global.d.ts` 一并拷入并纳入 tsconfig `include`（`docs/devkit/README.md`）。
- **路由：参数路由吞掉后到的静态兄弟（`server/src/routes.rs`，Important）**：`register` 的同
  pattern 去重原用 `matcher.at_mut(pattern)`——那是**路径匹配**而非 pattern 查找。已注册
  `/x/admins/{pk}` 时 `at_mut("/x/admins/me")` 会把 `me` 当成实参匹配成功 ⇒ ①**方法嫁接**：
  静态兄弟的 handler 被并进参数节点的方法表，`GET /x/admins/me` 落到 `{pk}` 的文件；
  ②**假冲突**：两个同动词静态（`me` / `session`）被判为 `route conflict`，请求期 500 且
  报的是无关文件；③`listing()` 列出实际不在树里的行。是否触发只取决于注册顺序（api 文件按
  路径排序：参数目录名 `pk` 排在 `session` / `sign-*` 之前即命中），所以此前只能靠给参数目录
  加 `zz-`/`{` 前缀改排序来规避。改为**按 pattern 字符串去重**：matcher 的 value 只存**槽位
  下标**（`usize`），真正的 `HashMap<method, Entry>` 移到表侧 `Vec`（`nodes`），pattern → 槽位
  由 `slots: HashMap<String, usize>` 记录，`matcher.insert` 每个 pattern 只调用一次。
  **静态段优先于参数段**由 matchit 自身优先级保证（与注册顺序无关）。release 直载
  `from_entries` 复用同一 `register`，一并修复。新增 4 条 unit 测试（含最凶的「同动词参数 vs
  静态」形态，并断言静态命中 `params` 为空），并做了**变异验证**：把去重换回 `at_mut` 后 4 条
  立即变红。实测：真实项目上旧二进制 2 条假冲突 + 4 个 500 + `oj test` FAIL，修复后 10 行路由
  全在表、7 个端点全 200、1/1 通过。文档同步 `docs/route-params-design.md` §5（去重不再走
  `matcher.at`、静态优先、冲突钉死）与 `docs/user-manual.md` §7.1（静态优先）。
- **路由冲突哨兵不再被后来的声明复活**（同上文件，Minor）：同一 `(pattern, method)` 被 **≥3 个
  文件**声明时，此前最后一个文件会把方法表里的 `Conflict` 冲回 `File` ⇒ 请求从 500 **静默变
  200** 且指向第三个文件（与 §5「同 pattern 同方法双声明 → 500」自相矛盾）。现在冲突**钉死**：
  保持 500，并再记一条 error 指出还有文件参与（测试 `table_conflict_survives_third_declaration`）。

**行为变更（升级注意）**
- **同位置异名参数现在是「结构性冲突」，release 会 fail-fast 起不来**（路由表改 pattern
  字符串去重的连带修正，升级务必核对）：`/x/{id}` 已注册时再注册 `/x/{name}`，此前因去重
  bug 被**静默合并**进同一节点（两条 URL 都可用，且参数名按先注册者算），现在按设计 §5 由
  matchit 报 `Conflict`：dev 是 error 日志 + **该 URL 404**（服务照常起），release 则因为
  `oj/src/app.rs` 把 `from_entries` 的 failures 当作致命错误 → **启动直接失败**（此前能起）。
  所以升级后「release 起不来 / 某条 URL 404」先查启动日志里的 `invalid route … conflict`。
  改法二选一：统一同位置的参数名（`/x/{id}` 一处即可），或把后来者挪出该位置（改成静态段或
  更深一层）。另：dev 与 release 的注册序不同（dev 按 api 文件全局排序、release 按
  `manifests.yaml` 锁顺序逐模块），跨模块的这类冲突在两个模式里丢弃的可能是不同文件
  ——见 `docs/route-params-design.md` §4/§5。
- **新增顶层 `smtp:` 段**：此前该键被忽略，现在会被解析——若旧配置里恰好有同名且**非映射**
  的键（如 `smtp: false`），启动会解析失败；改名或删掉即可。不写该段则行为完全不变
  （`mail.*` 调用报 `mail not configured`）。
- **配了 `smtp:` 但未装 `oj-mail` 插件**：不阻断启动，`mail.*` 调用报
  `mail not configured (config smtp: section missing, or oj-mail plugin not loaded)`
  ——与 es/auth 的「配置声明即 fail-fast」不同（mail 缺插件不构成安全失守，故意放行启动）。
  装插件：`cargo xtask plugin mail`。
- **`file_transport` 目录须先存在**（lettre 不建目录）；非空 `allowed_*` 是发信前提。
- **mail 双配置源互斥**：顶层 `smtp:` 与**非空** `plugins.mail` 同时出现时，
  此前 `plugins.mail`（原样透传）会**静默胜出**（改 `smtp:` 里的白名单/凭据不生效）；
  现在**装配期直接报错**（`pick one`），启动即暴露。`plugins: {mail: {}}`（空对象）
  不受影响 —— 它是「回落 `smtp:` 适配器」的写法（也是本仓 e2e 夹具的形态）。
- **白名单条目语义收紧**（需核对配置）：匹配由「裸后缀」改为**全等** ——
  ① `@x.com` **不再**覆盖子域（要子域须显式写 `@sub.x.com`）；② 裸域/空条目/首尾空白
  现在让**启动失败**（此前空条目会让白名单恒真）。若原配置靠后缀匹配「顺带」覆盖了一批域，
  请逐条补全。
- **`mail.result` 归属收窄 + jobId 形态变化**：enqueue 的 jobId 改由**宿主**生成
  （`<16 hex>-<序号>`，调用方传入值被忽略）；结果按「profile + 模块 + 租户」过滤 ——
  **跨模块回查不再可用**（改用 `bus.subscribe("mail.result")` 做通知）。
- **附件默认上限**：单件 10 MiB / 单封合计 25 MiB，超限 `code:5`；确有更大附件需求
  请在 `smtp:` 顶层调 `max_attachment_bytes` / `max_total_attachment_bytes`。
- **`sendRaw` 与 `headers` 互斥**：同时给会在宿主侧 `code:5`（此前静默忽略 `headers`）。
- **正文裸 CR 归一为 CRLF**：`text`/`html`/`raw` 正文里的裸 `\r` 由「原样保留」改为
  归一到 `\r\n`（防 SMTP smuggling）；raw **头区**含裸 CR 则直接 `code:5`。



**特性**
- 导入别名 `#x`（**本模块根**）/ `#/m/x`（**src 根**，首段 = 模块目录名）：共享库不再按目录
  深度数 `../`。`src/m1/a/b/c/d/e/f/g/api.ts` 引 `m1/_shared/xxx.ts` 写 `#_shared/xxx`
  （显式后缀 `#_shared/xxx.ts` 与目录索引 `#_shared` 同样支持），跨模块写
  `#/user/_shared/validate`。
  - **锚点由文件自身位置派生**：从引用方目录向上找最近的 `manifest.yaml` 祖先 = 模块根，
    其父目录 = src 根。于是 dev（`src/<m>/`）、release 产物（`dist/<m>-<v>/`，manifest
    原样复制）、tasks 镜像三处语义天然一致，**无需把 api 根路径穿透到装配点**（`LoaderShared`
    形状零变更；引用方在 `node_modules` 内不启用，不劫持第三方包的 `package.json#imports`）。
  - 模块外（`tests/` 用例目录、任务池）无锚点 → 明确报错并给下一步。
  - **release 语义**：`oj build` 期实化为版本目录相对路径（跨模块目标按
    `dist/manifests.yaml` 锁钉版本），产物内**不含** `#`。任务池非版本化 → 别名一律拒绝。
  - **S008**（`oj build` 内嵌，`--check` 同跑）：别名目标必须存在；`#/其他模块/…` 必须在
    `manifest.yaml` 声明 `deps`（跨模块代码耦合纳入与表归属 S003 同一套归属图）。既有
    **相对**跨模块引用不追溯——追溯会让既有项目升级后 build 直接失败。
  - **产物自洽断言**（两道）：每个产物文件内不得残留 `#`（`assert_no_aliases`）；构建末尾扫
    **本次产出的目录**（各模块版本目录 + tasks 镜像）内**任何**本地 specifier 都必须落到已落盘
    文件（`assert_dist_consistent`）——「dev 能跑、release 悬空」这类问题从运行期静默炸变成
    构建期显式失败。不扫整个 `dist`：陈旧的他模块产物不该让本次构建失败。
  - `#` 与 Node `package.json#imports` 共用命名空间：项目声明了 `#` 开头的 imports 键 →
    S008 fail-fast（避免 Node/vite 与 oj 两套解析器分叉）。
  - 编辑器/测试工具对齐：`sample/tsconfig.json` 补 `paths`（`"#/*": ["./src/*"]` +
    `"#*": [各模块 /*]`；值须带 `./` 前缀，否则无 `baseUrl` 时 vite/esbuild 会告警；
    不放 `baseUrl` 以免改变裸包解析）；L2 单测新增
    `sample/unit/vitest.config.ts`（`resolveId` 插件镜像同规则，否则被 spec 直接 import 的
    模块内文件用了别名即解析失败）。

**修复**
- **目录索引导入在 release 悬空**：`import x from "../_shared"`（= `_shared/index.ts`）dev 能跑，
  build 却因改写只做「末段补 `.js`」而产出 `../_shared.js`（不存在的文件）。构建期改为
  **探到真实文件**后产出 `_shared/index.js`。
- **构建期改写口径与运行期分叉**：主构建路径此前是行级 `from "…"` 口径，漏**副作用**
  `import "…"` 与**动态** `import("…")`（tasks 镜像用的是另一个更全的扫描器）。现将两侧
  归一到唯一扫描器 `bridge::import_scan::specifier_spans`（跳过注释/普通字符串与模板串，
  返回字节 span，支持一行多 specifier；build 与 checks 共用，同 `bridge::guard::extract_tables` 先例）。
- **`api.ts` 导入禁令可被前缀绕过**：守卫此前只扫相对 specifier，`#item/api`、
  `#/user/item/api` 可溜过。现扫全部本地 specifier（npm 包内子路径 `pkg/api` 不在守卫面）。
- 构建期与运行期**共用同一份解析探针**（`resolve_relative` / `resolve_alias`），
  「dev 能跑、release 悬空」的一类缺陷在结构上被消掉。
- **L2 db mock 缺构造器面**：v0.1.17 起 sample 多处改用 `db.table(...)` 构造器，而
  `sample/unit/mocks` 只实现了 `db.query`/`exec` → `npm run test:unit` 在 HEAD 即 2 例失败
  （`db.table is not a function`）。新增 `mocks/query-builder.ts` 镜像 `bootstrap.js` 的
  `builderFromReq` API 面（select/where/orderBy/limit/offset/insert/update/delete/returning/
  all/run/toSQL + `update|delete` 无 where 即抛的守卫），SQL 按规范形渲染（小写、`col, col`、
  `?` 占位；真实渲染由 sea-query 按方言产出，mock 不复刻方言），未覆盖形态直接抛错。
  L2 12/12 恢复绿。
- **Windows 别名解析误判「未找到模块根」**：`module_root_of` 用词法 `starts_with(project_root)`，
  而 `canonicalize` 给 referrer 目录加 `\\?\` verbatim 前缀、`ModuleSpecifier::to_file_path`
  还原时又剥掉——同一条长名路径仅差此前缀，导致 `oj build` 内省（`alias_build_materializes_to_versioned_relative_paths`）
  在 Windows CI 失败。现新增 `strip_verbatim` 在比较前归一两侧前缀；并在构建入口（`run`）与
  dev 服务端（`app.rs`）对 `src`/`project_root` 统一 `canonicalize` + `strip_verbatim`（与
  `resolve_to_segs` 的 `strip_prefix`、`oj-plugin-ffi` 的 `dunce::simplified` 同约定），
  Windows 与 unix 路径形态从此一致。新增 `#[cfg(windows)]` 回归用例
  `module_root_of_tolerates_verbatim_prefix_mismatch`。

**行为变更（升级注意）**
- **本地 import 目标必须是 `.ts`**：`import "./x.js"` / `import "./d.json"` 这类**非 `.ts` 目标**
  在产物里没有对应文件（`collect_module` 只转译 `.ts`），此前会静默产出悬空 specifier、
  release 才炸；现 `oj build` 直接失败并给出下一步。若你的项目依赖该写法，请把目标改成 `.ts`
  （或把内容内联）。
- **`manifest.yaml` 只允许出现在模块根**（`src/<module>/manifest.yaml`）：嵌套声明会让 `#` 别名的
  锚点（向上最近的 `manifest.yaml`）指向嵌套目录，静默改写整棵子树的别名语义 → 现 fail-fast。
- **tasks 池 import 不得越过池根**（跨模块相对导入等）：任务池是非版本化资产，只镜像
  `dist/<tasks.dir>/`，池外目标在产物里必然不存在 → 现 fail-fast（此前静默悬空）。
- **release 模式不再解析别名**：`resolve_alias` 在 `ts=false` 时直接报错（产物本不该含 `#`；
  此前同模块别名会"半可解析"、跨模块悬空，语义不一致）。

**评审修订（开发 / 架构双专家，处置记录见 spec §12）**
- 修：**拼接实参的动态 import 被误改**（`import("./locales/" + lang)`）——`import(`/`require(`
  形态要求实参为孤立字面量（闭合引号后只能跟 `)` / `,`），`from` 形态不允许 `(`（排除
  `Array.from("./x")` 这类方法调用）。这是本特性引入的回归，已由 core 单测钉住。
- 修：**检查比运行期更严**——`checks::collect_ts` 与 `build_cmd::walk` 此前不跳过 `node_modules`
  （运行期明确跳过），第三方源码可致 S008 误报、甚至被打进产物；两个 walker 现与
  `resolve_inner` 口径一致。
- 修：**扫描器下移到 core** `src/bridge/import_scan.rs`（build 与 S008 共用唯一实现），
  解掉「结构检查层依赖构建管线」的分层倒挂（同 `bridge::guard::extract_tables` 先例）。
- 修：构建期改写报错补上**违规文件路径**（此前只报目录、无下一步）。

**文档 / 杂项**
- sample 迁移为两种写法并存（别名：`user/account`、`admin/menu-list`、`cert/renew`、
  `auth/refresh`、`order/list`、`idp/token`、`oidc/callback`；相对：`admin/role-item`、
  `cert/item`、`idp/login`、`idp/authorize`、`oidc/login`、`oidc/logout`）；
  `idp`/`oidc` 的 `manifest.yaml` 补 `deps.auth`（S008 要求）。
- 手册：user-manual §4/§8、api-manual §5、dev-guide §7、cli2 build、testing §L2、
  sample/MODULES.md 补别名规则与边界（含「只能在模块内用」「跨模块需声明 deps」、
  tsconfig `#*` 手维护）；user-manual §12 与 api-manual §12「已知限制」表补新约束；
  内部走读 docs/modules 01/04/07/08 同步（build 改写函数改名与两道断言、S008 进规则表、
  L2 mock 的构造器面）；devkit SKILL.md 补新模块 checklist 与常见陷阱三行；README /
  README_cn 特性列表补别名一条；cli2 的 `--check` 规则范围与 route-params-design 的过时段
  标注同步。
- **tests：Windows 回归用例 `module_root_of_tolerates_verbatim_prefix_mismatch` 实参错配**
  - 该用例旨在验证 `module_root_of` 在 `from_dir` 与 project_root 的 `\\?\` 前缀**不一致**
    （canonicalize 加前缀、to_file_path 剥前缀）时仍命中模块根；但两向断言都把**referrer**
    目录（`with_prefix`/`no_prefix`）当成了 `root` 参数，方向一只把两者归一成同一个
    `src/m1/a/b` 路径、找不到 manifest 直接 `d == root` 跳出 ⇒ 返回 `None`（CI 在 Windows 红）。
  - 修正：方向一传 `from_dir = no_prefix` + `root = proj_root_prefix`（canonical 项目根），
    方向二传 `from_dir = with_prefix` + `root = proj_root_plain`；`module_root_of` 对二者均
    `strip_verbatim` 再比、返回无前缀模块根，故两向期望统一为 `strip_verbatim(canon/src/m1)`。
    函数逻辑未动（其前缀归一本就正确）。

## v0.1.17（2026-09-15）

**特性**
- 构造器 `insert` 取回自增主键：`db.table("t").insert({...}).returning(["id"]).run()` →
  行数组 `[{id: n}]`（不带 `returning` 时仍返回受影响行数）。列过白名单
  （`unknown column '<c>' in insert returning`），仅 insert 接受——动词×字段矩阵在 op 侧
  权威校验，`select/update/delete` 报 `<verb> does not accept returning`。
  pg / sqlite 由 sea-query 渲染单条 `RETURNING` 语句、**一次往返**；mysql 方言无
  RETURNING，只接受**单列**，在同一执行目标上两步取 `LAST_INSERT_ID()`（非原子，并发请放
  `db.tx` 内）。`toSQL()` 产物同步带 RETURNING。

**加固**
- 补「事务 × 多租户」回归（此前只有池路径用例）：租户注入发生在 `resolve_target`
  **之前**，与「走池还是走会话」正交——`tx.table(...)` 与 `db.table(...)` 的改写完全一致
  （事务内 select 收窄到当前租户、insert 强制写当前租户、update 改不到他租户的行、
  回滚不留痕）。

**文档 / 杂项**
- sample 三处 `insert ... returning id` 裸 SQL 改为构造器（admin/menu-item、
  admin/role-item、cert）——此前这些裸插入在 `sql_guard=warn` 下会静默写入无 `tenant_id`
  的行（裸 SQL 只查「文本里有没有 tenant_id」，不注入）。
- api-manual（DML：returning 小节；事务：租户防护说明）、db-guide（§4 事务改用构造器、
  §7 DML returning）、global.d.ts（QueryBuilder 补 insert/update/delete/run/returning/toSQL
  声明——此前只声明到 select/all）。

## v0.1.16（2026-09-14）

**特性**
- WS 二进制帧支持（Plane 协同文档/Yjs 场景解锁；PRD
  `plane/docs/ever/prd/oj-feature-request-ws-binary.md`）。入侧：客户端 Binary 帧（opcode 0x2）
  原字节透传，`http.body` 为 `null`（不再 `from_utf8_lossy` 产生垃圾），新增
  `http.bodyBytes(): Promise<Uint8Array>`（文本帧同样可用，返回原始帧字节）。出侧：
  `ws.send(data)` 接受 `string | Uint8Array`——string → Text 帧（0x1）、Uint8Array →
  Binary 帧（0x2），帧型由参数类型决定。`sess.state` 语义不变（必须可 JSON 序列化；
  Yjs awareness 等二进制状态走 base64 或 kv，见 api-manual §13）。
- bus 字节化（ABI 7 → 8，**需同步重编全部插件**）：`bus.publish(topic, data)` data 传
  `Uint8Array`/`ArrayBuffer` → 订阅 WS 会话收 Binary 帧（原字节，不包信封）；JSON 数据行为
  不变（`{"topic","data"}` Text 帧）。wire 约定：JSON → record payload = 信封 UTF-8；
  字节 → record payload = 原始字节；消费侧启发式（UTF-8 且为含 topic+data 的 JSON 对象 →
  文本信封，否则二进制透传）。FFI `EventBrokerVtable.publish` data 与 `HostContext.deliver`
  payload 改 `RBytes`（stabby Vec<u8>）；oj-bus-kafka / oj-bus-rabbitmq 消费循环去 lossy
  直通字节。
- 命名 MQ 二进制载荷（零 ABI，`OjMqMessage` 词汇表扩展）：`Kafka("x").send(topic,
  {value: new Uint8Array(...)})` / `RabbitMQ("x").publish(..., uint8array, ...)` → base64 进
  `value_b64`，record 载荷 = 原始字节；poll 侧非 UTF-8 载荷 → `value = null` +
  `value_b64`（base64 字符串）。
- `oj test` L1 WS 帧测试面 `client.ws(path)`：首次使用惰性起 127.0.0.1:0 本地服务
  （真实路由 + 真实帧循环），`send(string|Uint8Array)` / `next(ms?) → {binary,data} |
  {closed:true} | null` / `close()`。可运行用例 `sample/tests/ws-bin.test.ts`（模块
  `sample/src/echo-bin/`：二进制回显，字节相等 + 非 UTF-8 无损）。

**修复**
- `oj test` 的 `client.ws(path)` **服务端主动推帧读不到**：隧道只在 `send()` 内惰性建立，
  `next()` 拿 `null` 句柄调 `op_client_ws_next` → `TypeError: expected u64`。而 connection
  钩子里的 `ws.send`（连接即推送）与 bus 广播都发生在客户端开口之前 → 首帧必丢。改为
  `send`/`next`/`close` 共用一个惰性建连、`close()` 未建连即空操作；补回归用例
  `ws server-push`（读 `/news/ws` 的连接钩子帧）。
- `client.ws(path).next()` 的**文本帧解码恒抛** `ReferenceError: TextDecoder is not
  defined`：test ext 不注册 deno_web，运行时无 `TextDecoder`（同 G① 的 `atob` 缺口）。
  原实现依赖 `new TextDecoder()`，而样例只读过二进制帧故从未暴露；现自带最小 UTF-8 解码
  （1–4 字节含代理对），并把 `sample/src/echo-bin/ws.ts` 改为**帧型忠实回显**使其可覆盖
  （文本帧→文本帧、二进制帧→二进制帧），补用例断言 `"ping ✓ 汉字 😀"` 原样往返。
- blob-s3 插件：**明文 http 端点下整个 s3 驱动不可用**。object_store 默认 `allow_http=false`
  → reqwest 客户端 `https_only(true)`，于是 `endpoint: "http://…"`（= `sample/config.yaml`
  自带的 MinIO 示例）在「建请求」阶段即报 `builder error for url (…)`（0 retries / 微秒级），
  put/get/url 全废；https 端点不受影响。现按端点 scheme 显式 `with_allow_http(true)`，并补
  **离线**回归测试 `http_endpoint_reaches_network_not_url_builder`（打本机必然关闭的端口，
  只断言错误类别 = 网络阶段而非建请求阶段）——env-gated 的 `real_s3_roundtrip_via_vtable`
  从不进 CI，正是这个 100% 断链漏进发布物的原因。

**文档**
- 新设计文档 `docs/superpowers/specs/2026-09-14-ws-binary-frame-design.md`（WsSend 统一
  枚举、bus wire 约定与启发式边界、ABI 8 清单）。
- api-manual §4（ws.ts 二进制帧小节 + bodyBytes 示例）、§6（http/ws/bus 表、命名 MQ
  value_b64、client.ws 行）、§9（client.ws）、§13（bus wire 约定与启发式边界、二进制
  状态建议）；SKILL.md 陷阱 +2；websocket.md §1.1 二进制帧 + 修正 http.body 旧表述；
  global.d.ts（WSApi.send 联合类型、http.bodyBytes、OjMqMessage.value_b64、TestWs）。

**CI/插件**
- `cargo xtask plugin <name> --check` 门禁随 ABI 8 生效；旧 ABI 7 cdylib 报
  `plugin ABI mismatch: plugin=7 host=8`——`cargo xtask build` 重编即可。

## v0.1.15（2026-09-13）

**特性**
- `tenant.sql_guard` 多租户 SQL 防护（false（默认，不改写 SQL）| `"warn"` 注入+告警 |
  `true`/`"deny"` 注入+拦截）：`db.table()` 构造器自动注入租户条件——select 基表限定列
  注入、join 进 ON 子句（不影响 left join 语义）、条件树/子查询/union/with 递归、
  insert 强制当前租户值（mismatch 报错）、update/delete 自动收窄、sets 显式改
  tenant_id 拒绝（防行迁移越权）；裸 SQL（`db.query`/`db.exec`）查「完全遗漏 tenant_id」
  warn 告警 / deny 拦截（Deny 另要求参数数组含当前租户值；DML/DDL 未识别表 fail-closed），
  双引号/反引号标识符保留以兼容 toSQL 回放。逃生口 `db.asSystem()`：请求级 system 标志
  （ReqState 重置即失效）+ 服务端审计日志。schema.yaml 表级 `tenant: false` 共享表声明
  （缺省 true）+ config `tenant.shared_allow` 白名单交集生效（fail-closed），
  `validate_tenant` 挂三处：server 启动 / `oj build`（checks）/ `oj migrate`；
  `enable=false` 而 guard 非 Off 启动 warn。新人白话文档 `docs/tenant-guide.md`；
  api-manual 第 8/10 章同步。**已知边界**：租户头为客户端自报，防伪造须 JWT claims
  绑定（规划见设计文档 §7，另立任务）；裸 SQL 字面检查为 best-effort（只防完全遗漏）。
  设计/评审：`docs/superpowers/specs/2026-09-13-tenant-sql-guard-design.md`。

**CI**
- `ci(release)`：新增 `guard` 去重 job——`gh release create` 会把刚建的 tag 推回远端再次命中
  `push: tags` 触发整条流水线重跑（并自动把草稿转正），浪费三平台构建/测试资源。`guard` 在
  tag push 且对应 release 已存在时（即「自己推的 tag」）输出 `skip=true`，`lint`/`package` 加
  `needs: [guard]` + `if: needs.guard.outputs.skip != 'true'`，下游 `publish`/`publish-npm`/
  `smoke-npm` 级联跳过；真实 tag 推送与人工核对后重发仍走全量。
- `fix(ci)`：上条 guard 把 `publish-npm` 一并级联跳过（`needs: package` 在重爬 run 里被
  取消），且重爬 run 无 dist 产物可供 `npm-publish.sh` 使用——`publish-npm` 改为只依赖
  `guard`，产物按 run 类型二选一：首发 run 用本次构建产物；重爬/转正 run（skip=true）
  从 release 资产 `gh release download` 取同款产物，照旧走幂等 publish（已发布即 skip）。
  新增 `release: types: [published]` 触发：草稿（dispatch draft=true）不发 npm，人工核对
  点「发布」转正那一刻由 release 事件接续补发 npm（guard 对 release 事件输出 skip=true，
  重型 job 全跳）——草稿门禁由此真实生效；tag 重爬 run 退化为幂等空跑。
  `publish-npm` 加 job 级 `concurrency: npm-<tag>`（不取消、排队）：转正瞬间 release run
  与重爬 push run 并发双发 npm，publish_pkg 虽幂等（npm view + 409 re-view），但 409 后
  re-view 撞上 registry 传播延迟可能误判失败，按 tag 串行化硬消除该窗口。

**文档 / 杂项**
- db：文档与 sample 全面改写为 `db.table(...)` 构造器语法，替代原生 `db.query` 参数化查询——
  `docs/`（db-guide / user-manual / dev-guide / bridge / testing / cli2 / route-params-design
  / devkit SKILL & api-manual / superpowers plans & specs / MODULES）+ `sample/src/**/api.ts`
  （account / item / profile / order-{account,detail,list} / auth-{login,refresh} / idp/login /
  oidc/callback / cert/* / admin/*）。构造器不支持的列别名 / `RETURNING id` /
  `last_insert_rowid` / 动态列清单 / join 别名等场景保留原生查询（注释标明）。

**修复（集中审查）**
- guard：`Join.tenant_id` serde 预设可被 `db.fromJSON` 投毒（绕过 JS 链层直喂 serde，
  评审 P0）——`apply_tenant` 改为对受约束 join 表**无条件覆盖**注入值，JS 预设永不生效；
  tokens() 此前整段跳过 `"…"`/`` `…` `` 引用标识符 → `FROM "t"` 对表提取失明（裸 SQL
  守卫静默放行）——引用标识符内容现在成词，`"t"` 形态可见（toSQL 回放从 vacuous pass
  变为真校验）。附回归用例：fromJSON 投毒覆盖 + 引用标识符单元/行为用例。
- oidc_e2e：夹具 `_platform` 补 schema.yaml（users 表声明）——本版本内 idp/login 改写
  为 `db.table()` 构造器后，表须经 SchemaRegistry 白名单（registry 来自
  schema.yaml，seed.sql 只建物理表），夹具缺声明致 login 500、set-cookie 缺失 panic。

## v0.1.14（2026-09-12）

**特性**
- `db.table` 查询构造器（对齐 xorm builder / sea-query，单 op 扩展 + toSQL）：`select` 等价 +
  `toSQL` 只构造不执行（`op_db_query_sql`，含方言占位符 + 参数）；嵌套条件树
  `and/or/not` 递归编译（深度 8 / 叶子 64 上限）+ JS 条件对象（工厂 / 不可变组合 / tree /
  fields / has，零新 op）；`Verb` 动词 + 动词×字段兼容矩阵（op 侧权威校验）；DML
  `insert/update/delete`（tx 路由 + 返回行数 + `run()` 终执行）；`join` 联表（inner/left，
  限定列校验，自 join 拒绝，join 表过归属守卫）；聚合列（fn 枚举 + `distinct`）+ `groupBy`
  + `having`（聚合别名 op 侧展开，PG 方言兼容）；`toJSON/fromJSON` 序列化（快照复原可继续链 /
  执行）；`where` 子查询（`in` / 标量比较）+ `exists` + 嵌套 `select` 地基
  （`REQ_NEST_MAX=4`）；`union/union all`（显式列 + 列数校验 + 成员禁排序分页，嵌套禁
  unions）；`case` 列 + 窗口函数列（`row_number/rank/dense_rank`，别名不过 having 台账）；
  非递归 `CTE`（`with`，虚拟表列白名单，CTE 名跳过归属守卫）。
- `oj server --daemon` 后台运行（unix `setsid` 重 `exec` / windows `DETACHED_PROCESS`）。

**实现**
- `op_db_query_build` 拆 `guard_req` + `build_statement` 两段（`select` 等价）；`ColCtx`
  限定列解析（六处共用，`apply_op` 泛化）；`CondTree` 手写 `Deserialize`（按键唯一分发，
  空组 / 多键精确报错）。

**修复**
- `fix(query)`：守卫遍历 `CASE` 列内嵌套 `req`（F-1）+ 嵌套 `offset` 门禁 + `when` 数上限
  （统一审查修复）。

**文档 / 杂项**
- `docs(spec)`：`db.table` 设计（方案 A 单 op 扩展 + toSQL / 条件对象 / 序列化简 / 双评审
  架构+实现修订）；分阶段实施计划（7 Phase / 14 Task，TDD）。
- `docs(v0.1.14)`：`db.table` 构造器文档（DML / 条件树 / 条件对象 / join / 聚合 / toSQL /
  序列化）。
- `docs(db-guide)`：新人手册——配置 + JS 全量 `db` API + 边界红线；「写第一个 handler」补
  `async/await` 等价写法（sample admin 风格）；CTE 概念与原理解读（临时视图心智模型 /
  渲染形态 / 声明列契约 / 遮蔽与校验顺序）。
- `docs`：运维手册补 npm 分发段（`@oj-bin/*` 手动发布 + 内建门禁）。
- `build(deps)`：oj 测试依赖 `jsonwebtoken` 11.0.0（oj-auth 与根测试仍 9.3.1，树内双版本）。
- `test(e2e)`：query builder join + insert 走 HTTP 全链路。
- `build(docker)`：多阶段构建 + distroless runtime，glibc 2.31 基线。

## v0.1.13（2026-09-11）

**特性**
- **npm 分发**：`@oj-bin/oj` 主包 + 平台子包模板与 npmjs 展示页；`postinstall` 落盘
  `./bin`（支持面检测 + 原子替换 + `exit 0` 语义）；`npm-publish.sh` 装配 / 幂等发布 /
  门禁 / 置信断言 + dry-run 自检。

**实现**
- `fix(npm/postinstall)`：加固 bail 输出与 `INIT_CWD` 哨兵，避免击穿 `exit 0`。
- `fix(npm)`：避免 tarball 清单 `grep` 命中即退导致 `tar` 收 `SIGPIPE` 假失败。

**CI**
- `ci(release)`：新增 `publish-npm` + `smoke-npm`——npm 双发（失败标红不阻塞 Release）；
  `publish-npm` 草稿模式跳过（npm 不可撤回，人工核对后幂等补发）；发布门禁——源文件缺席
  冒烟（`deploy.sh` / `deploy.bat` / `release.yml`）。

**文档**
- `docs`：npm 分发方案 spec（平台子包 + optionalDependencies，双发不阻塞）→ spec 评审修订
  （改 scoped `@oj-bin/oj`、独立 `publish-npm` job、`postinstall` 加固）→ 实施计划
  （5 任务）→ 全分支终审报告归档（可合入，零 must-fix）；npm 安装段入 README（spec 同步
  `postinstall` 两处实现级修正）。

## v0.1.12（2026-09-11）

**修复**
- **release 产物在非构建机无法初始化 JsRuntime**（`Failed to initialize a JsRuntime:
  No such file or directory (os error 2)`，阻断级）。根因：deno_core 0.411 的
  `include_js_files!` 系列宏一律以 `mode=loaded` 产出
  `ExtensionFileSource::loaded_during_snapshot(spec, concat!(env!("CARGO_MANIFEST_DIR"), ...))`
  ——把**构建机绝对路径**烧进二进制；本仓未启用 startup snapshot，运行期按该路径读盘，
  非构建机上必然 ENOENT。受影响的不止自身 `src/bridge/bootstrap.js` 与
  `oj/src/test_ext/test_bootstrap.js`，还有 deno_web / deno_fetch / deno_net /
  deno_websocket / deno_webidl 五扩展共 40 个以绝对路径声明的 JS。
- 修复：新增 `build.rs`（版本取自 `Cargo.lock`，源码目录取自 registry 或 `vendor/`）
  把上述 deno_* 扩展 JS 构建期 `include_str!` 进二进制，运行期由
  `bridge::patch_fs_loaded_sources` 覆写为 `Computed` 源；自身两个 bootstrap 经
  `ascii_str_include!` 直嵌（去掉 `esm = [dir ...]` 声明）。所有 JsRuntime 入口
  （HTTP 池 / 任务 / `oj test`）统一经 `bridge_ext_init` / `oj_test_ext_init` 取扩展。
- 回归护栏：`ws_client_extensions()` 打补丁后不得再有 `!is_runtime_loadable()` 的源；
  该用例对构建机路径零依赖，任意机器可跑（不依赖源文件是否存在）。

**工具 / CI**
- 发布门禁 `cargo xtask smoke --bin <oj>`：把两个 bootstrap 与 deno_* 依赖源码目录
  临时改名后跑最小 `oj build`，任何残留的构建机路径依赖即失败（守护在返回/panic 时
  无条件还原，并先恢复上次中断残留）。`scripts/deploy.sh` / `scripts/deploy.bat`
  打包前串联，`release.yml` 三平台显式执行同一命令。

## v0.1.11（2026-09-10）

**特性（breaking）**
- server 准入门三态（无静默默认）：api（`--api-path`）与静态站点（`server.app_path` /
  `--app-path`）至少显式指定其一，皆未指定退出并提醒；皆指定则两者都必须存在，任一
  缺失退出；仅指定其一 → 只启用对应功能（api 缺席 = 纯静态模式，监听行标 `static-only`）。
  server 不再自动搜索 src/dist 兜底（test/migrate 保留搜索并对缺失目录就地报错）。
- CLI 路径语义：`--app-path` / `--api-path` 相对 **CWD** 解析（config 内
  `server.app_path` 仍相对 config 目录）。
- config `server.base` → `server.api_prefix`（serde alias 兼容旧键，两键并存报
  duplicate field 防漂移）；新增 `server.app_prefix`（默认 `/` = 全路径兜底）：非 `/`
  时仅前缀下 GET/HEAD 落静态（前缀剥除解析，前缀根 → index.html），前缀外 404，
  API 路由永远优先。

**实现**
- console 关闭时启动失败的最终退出原因仍直写终端：logging tee 保存原始 stderr fd
  副本（`echo_terminal`），main 错误出口补写（console 开启时不补，避免重复）。
- dev 缺省服务目录自 config 同级逐级向上搜索（每层 src 优先、dist 次之），不再相对
  CWD 选址；xtask 可执行产物拷贝改 tmp+rename——就地覆盖触发 macOS vnode 签名
  缓存毒化（execve 一律 SIGKILL）。

**修复**
- e2e 不再依赖仓库内 sample/dist（停止跟踪后 CI 新克隆静态根 fail-fast）。
- ws 闸门用例稳定性：WS_LIVE 归零/平静基线后再断言，消除 straggler 槽位抖动。
- vitepress 移动端根路径解析错位（ROOT 多上一级致 sync 失源）。

**文档**
- sample 运行入口统一为编译产物 `bin/oj`（docs / README / sample/README / CLAUDE.md
  弃用 `cargo run`，README 新增 bin/oj 专节）；devkit 手册与 skill 同步（准入门 /
  app_prefix / api_prefix / echo_terminal），vitepress 收录 sample 模块专题。

## v0.1.10（2026-09-09）

**特性（breaking）**
- WS 执行模型改「帧池」：每路由 W 个无状态 Worker（`ws.workers_per_route`，默认 2）从帧
  队列拉帧执行——内存与连接数解耦（会话态 ≈2KB/连接 vs 独占 6.2MB），帧超时毒化半径
  回归单连接。连接状态新增 `sess.state`（Rust 会话表持久，可 JSON 序列化）与 `sess.id`；
  「模块作用域 = 连接状态」写法废弃（现为 Worker 本地只读缓存）。
- 新增连接闸门 `ws.max_connections`（默认 1000，0=不限）：超限 upgrade 返 503。

**实现**
- bridge 新增 frame_pool（Scheduler per-conn 在飞=1 保序 / Worker 池 / 会话表 / 空池 linger
  退役）；`op_ws_sess_set` + `ReqState.ws_sess` 回传会话态；每连接 ws-js 线程撤销。

## v0.1.9（2026-09-09）

**特性（breaking）**
- `ws.ts` 契约改为生命周期钩子：`export default { connection, message, close, error }`——
  模块每连接加载一次、按事件触发，模块作用域即连接状态（原「整文件每帧重跑」写法废除）。
  订阅挪进 `connection()`（每连接一次）；`error(e)` 兜底帧异常、连接继续；`close()` 收尾
  恰好一次；帧超时必断连；至少导出一个钩子，全缺断连。返回值一律忽略，回帧显式
  `json.ok` / `ws.send`。

**实现**
- bridge 新增 `WsSession` 驻留会话（`ws_connect`/`fire`）：每连接独占 runtime、永不还池；
  JS dispatcher 装配钩子，零新增 op。server frame_loop 三任务（Reader/Writer/bus）不变。

## v0.1.8（2026-09-08）

**特性**
- `fetch` 换成官方 `deno_fetch` 实现，并注入 webpki-roots 根证书；https 请求与 wss 握手共用同一套信任链。

**修复**
- e2e：news 的匿名访问面收窄到 `/news/ws`，任务计数对齐。

**文档 / 杂项**
- fetch 语义切换说明 + `docs/websocket.md` 出站客户端章节（§7）。
- sample：清掉 WS 路由里冗余的匿名条目。

## v0.1.7（2026-09-08）

**特性**
- JS 侧可直接使用标准 WebSocket 客户端（注册 deno_websocket 及其依赖的五个扩展），附任务案例与 API 手册。

**修复**
- 显式安装 rustls CryptoProvider，修掉双 provider 并存导致 ws 客户端初始化 panic。

**文档 / 杂项**
- deno_core 0.410 → 0.411。

## v0.1.6（2026-09-08）

**特性**
- 命名 MQ 客户端：`kafka(name)` / `rabbit(name)` 按名字取用，走新增的 mq 轴 FFI 契约（ABI 仍为 7）；mq poll 用长轮询退避，不烧 CPU。
- 长任务池：JS 常驻任务由监督器托管，支持命名扫描、退避重启、优雅停机；`oj build` 会把 `tasks/` 一并镜像到 dist。

**修复**
- MQ 两轮统一审查整改：消费门禁语义、rabbit channel 泄漏、停机 e2e。
- 测试：SIGTERM 用例补 `#[cfg(unix)]`；插件 drive 改墙钟计时；tla probe 加锁避免并发污染。
- CI：release 上传按草稿 / 已发布分别处理，避开 immutable release 的 422。

**文档 / 杂项**
- 覆盖率三波推进（纯 Rust / 插件离线 / 环境实测），实测 87.82%。
- migrate/seed 启动日志改为「统计 + 错误」，不再逐条刷 INFO。
- 命名 MQ 与长任务手册、`docs/mq-tasks.md` 消费任务教学。

## v0.1.5（2026-09-07）

**特性**
- WS 文件命名统一小写（`ws.ts` / `ws.js`）。
- schema.yaml 支持联合主键（`pk: [a, b]`）。

**修复**
- CI：release 发布幂等，release 已存在则覆盖上传，不再报 create 失败。

**文档 / 杂项**
- sample 模块导览 `MODULES.md`。

## v0.1.4（2026-09-07）

**特性**
- 迁移账本改单表 + `module` 列，数据通道收敛，新增语句级执行日志。

**修复**
- Windows 静态 CRT 统一（`-MT` 注入、规避 MSYS 路径转换），消除 aws-lc-sys 混链告警。
- CI 补上「插件先构建再测试」顺序，以及 bootstrap.js 的 ASCII 红线守护。

## v0.1.3（2026-09-06）

**特性**
- OIDC 全套：内置 OP（discovery / jwks / authorize / token / userinfo）与 RP（login / callback / logout），含 PKCE、state+nonce、JIT 建号、按 tenant 路由到不同 IdP；浏览器跳转腿可用 `tenant.anonymous_paths` 免租户头。
- 鉴权解耦：jwt / bcrypt / crypto 沉淀为 bridge 原语；Bearer 守卫搬进 oj-auth 插件（新增 FFI auth 轴，ABI 5→6）；删掉内置 auth 路由，登录端点改由 sample 的 JS 实现。
- 插件注册改按轴 dlsym 探测（ABI 7），废弃 PluginRegistrations；插件自描述 + `GET {base}/plugins` 查询端点。
- `plugins:` 配置一段三用：键 = 严格清单，值 = 透传配置，空对象 = 回落；旧 list 写法废弃。
- `json.raw` 出裸 JSON。
- devkit（global.d.ts / API 手册 / skill）与主要文档重写对齐当前架构。

**修复**
- review-2026-09-06 整改：CI 守护缺口、clippy 门禁、测试分层。
- 插件 semver 统一取自身 Cargo.toml。
- Windows MSVC 统一静态 CRT（crt-static）。
- OIDC 终审项：jwks 空值守卫 502、code↔client 绑定、JIT 账号按 `oidc:<tenant>:<sub>` 隔离。

## v0.1.2（2026-09-05）

**特性**
- `json.ok` / `json.fail` 自动补 `content-type: application/json`。

## v0.1.1 及更早（2026-08-20 ~ 2026-09-04）

**特性**
- 运行时：deno_core 嵌入 + RuntimePool 复用、KillSwitch 看门狗（超时 408）、inspector、TS 转译与 ESM/CJS 模块加载、`ext_boot.js` 运行时补充。
- 路由与 CLI：目录镜像路由（任意深度 `api.ts`）、路径参数与 RouteTable、`oj server / build / test` 三子命令；build 按模块产出版本目录 + manifests.yaml + tgz，dev / release 模式自动判定。
- 业务能力面：多库 DSN、`db.tx` 单活跃事务、多租户中间件、JWT 鉴权、multipart 上传、blob（local / s3）、Redis KV、事件总线（redis / kafka / rabbitmq）、ES 薄封装。
- 插件系统：五轴注册表 → cdylib + FFI 契约落地，es / db-mysql / db-postgres / blob-s3 / bus-kafka / bus-rabbitmq / kv-redis 全部插件化，core 收敛为 sqlite-only。
- 证书：证书门禁（缺证书拒绝启动、过期限制 GET）、热重载与健康状态、`oj-cert` 工具（gen / renew）。
- 模块数据层：迁移引擎、`schema.yaml` 声明式、检查体系、schema diff、归属守卫。
- devkit：global.d.ts + TS API 手册 + agent skill 随包发布。
- 发行：跨平台打包、终端日志开关、deploy.sh。

**修复**
- 看门狗：terminate 改用 `v8::IsolateHandle` 修 SIGSEGV；KillSwitch drop 不再 join 自身（EDEADLK）并修掉线程泄漏。
- Windows 适配：路径分隔符 / 反斜杠路由键 / YAML 转义 / sqlite 相对 DSN 的 `\\?\` 前缀；kafka 原生依赖构建。
- 浮点绑定改 `Qv::Double` 防 f32 截断；reqwest client 构建失败 fail-fast。
- bus subscribe 失败回滚与按归属清理；插件 vtable 方法统一包 `catch_unwind`。
- `oj build`：vdir 撞名检测、routes.js 的 file 前缀、`-b` 参数消费；`oj-cert --days` 按天计算。
