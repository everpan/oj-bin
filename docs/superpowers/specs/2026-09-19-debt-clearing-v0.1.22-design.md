# 清账 v0.1.22 · 五条已知债的设计与验证

> 范围：`CHANGELIST.md` v0.1.22「已知债 / 另案登记」五条 —— ①PG 语句缓存 × 混合参数类型、
> ②`u64`/`BIGINT UNSIGNED`、③数值型 `tenant_id` 列、④内建序列分配原语、⑤MySQL 侧真库验证。
> 交付形态：v0.1.23 已发版（标签 → `039b314`），本批改动进 **v0.1.24**（版本号随
> `oj/Cargo.toml` 递增确定，**未打标签**）；本文件随该批提交落地，评审处置见 §6。
> 现有环境（实测）：PG 18.6（`postgres://poc:poc@127.0.0.1:5499/oj_test`，专用测试库）、
> MySQL 8.4.11（macOS `container` CLI 起在 `127.0.0.1:3306`，专用库 `oj_test`）、Redis 8、RabbitMQ 4。

---

## 1. 债① PG 语句缓存 × 混合参数类型

### 1.1 根因（已读上游源码 + 真库实测）

sqlx 0.9.0 的语句缓存 **key 只有 SQL 文本**（`sqlx-core-0.9.0/src/common/statement_cache.rs` 的
`LruCache<String, T>`）；PG 执行路径**无条件查缓存**
（`sqlx-postgres-0.9.0/src/connection/executor.rs:170`），命中后直接复用旧的
`(StatementId, param OIDs)` 去 Bind 本次按新 Rust 类型编码的字节 →
`invalid byte sequence for encoding "UTF8": 0x00` / `insufficient data left in message`。

**真库复现（改前基线，PG 18.6）**：

```
thread '...' panicked at plugins/oj-db-postgres/src/lib.rs:992:
同一文本的 text 形态应成功（债①：缓存按文本 key，命中旧 INT8 元数据）：
Err("db query: error returned from database: insufficient data left in message at line 531")
```

**MySQL 不同病**：它每次 execute 都重发参数类型（`sqlx-mysql-0.9.0/src/protocol/statement/execute.rs`
的 `new_params_bound_flag = 1`），缓存只决定 statement id 与参数个数 → 本债不动 MySQL。

### 1.2 修法：按参数形态分缓存键（保留缓存）

`plugins/oj-db-postgres/src/lib.rs` 新增 `shape_tag()`，在 **4 个执行点**（`query` / `exec` /
`tx_query` / `tx_exec`）给 SQL **前置** `/*oj:<形态>*/`：

- 形态字母表：`t`=text/null（`None::<String>` 的 type_info 就是 TEXT，与 `String` 同 OID）、
  `i`=i64、`f`=f64、`b`=bool、`m`=`$oj$i64` 标记、`u`=`$oj$u64` 标记；
- **必须前置**：后置在 PG 扩展协议下会被判「多语句」（`cannot insert multiple commands into a
  prepared statement`），SQL 以 `--` 行注释结尾时签名还会被吞掉。前置块注释 PG 词法层接受
  （已用 `PREPARE s1(int) AS /*oj:i*/ SELECT $1::int + 1` 实测：返回 42，且
  `pg_prepared_statements.statement` 保留前缀）；
- 连接构造补 `.statement-cache-capacity=512`（通过 DSN query key，`sqlx::Any` 无编程入口但
  `PgConnectOptions::parse_from_url` 认这个键）：分键后条目数 = SQL 数 × 形态数，默认 100 会频繁
  LRU 淘汰，而**淘汰要发 `Close`+`Sync` 并等回包**（多一次往返）。

**落点在插件而不是宿主**（曾被否定过一次）：宿主层（`src/bridge/ffi.rs` 的 4 处咽喉）也能实现，
但插件的测试直接打 vtable、**绕过宿主**，实测证明宿主层改动在插件测试里不可验证；且 sqlx 是插件的
依赖，宿主不该知道驱动细节。`toSQL()` 不经过插件执行路径 → 用户可见的 SQL 保持干净。

### 1.3 验收（env-gated 真库）

`real_postgres_same_sql_text_mixed_param_shapes`：同一文本 `insert … values ($1,$2)` 交替
`("a","x")`（t,t）与 `(1,2)`（i,i）——池路径 + tx 路径都成功；并断言
`pg_prepared_statements` 里该文本有 **≥2 条**不同前缀条目（分键生效）；再断言 4 行都落库。
**（改前红 → 现绿）**

---

## 2. 债② u64 / `BIGINT UNSIGNED` 全链路

### 2.1 现状（改动前）

- 读：插件走 `sqlx::Any`，`AnyValueKind::BigInt(i64)` 是唯一整数变体，
  `sqlx-mysql::any` 忽略 `UNSIGNED` flag → `> i64::MAX` **静默回绕成负数**；
- 写：JS 的 `i64Marker` 对超 i64 的 bigint 直接 `RangeError`；`Any` 层**没有 u64 的
  `Encode`/`Decode`**（编译期就挡）；
- 另有一处既有漏洞：`toSQL().params` 的超界整数被出口护栏降成**字符串** → 用户照文档
  「`db.query(toSQL().sql, ...toSQL().params)` 回放」必然失败（`bigint = text`）。

### 2.2 修法

| 层 | 改动 |
|---|---|
| 契约 crate | `oj-plugin-ffi/src/jsint.rs` 新增 `UINT64_MARKER = "$oj$u64"` + `marker_u64`（严格单键 + 规范十进制 + ≤ u64::MAX）+ `reject_u64_markers(params, why)`。**不在任何 `#[repr(C)]` 里 → ABI 保持 8** |
| JS 面 | `bootstrap.js`：`i64Marker` → `intMarker`（i64 范围 → `$oj$i64`；`(i64::MAX, u64::MAX]` → `$oj$u64`；越界 `RangeError`）；新增 `toUBigInt(v)`（`[0,2^64-1]`，fail-loud 同 `toBigInt`） |
| 宿主 | `query.rs`：`to_qv` 认 `$oj$u64` → `Qv::BigUnsigned`；`value_to_json` 改由 `int_param`/`uint_param` 回吐 marker（**修掉不可回放**：安全范围内仍给 number，超界给 marker）；`guard::param_is_tenant` 补 u64 两形态 |
| MySQL 插件 | `Pooled` 枚举分流：`mysql://` → **typed**（`sqlx::MySql`）、其余（离线测试 sqlite）→ `Any`（**该分支仅测试用**：Any 的 `TryFrom<MySqlTypeInfo>` 缺 `Tiny` 等分支，Any+MySQL 组合本就不可用，故它只覆盖编排/绑定通路，验证不到生产行解码）；新增 `bind_value_mysql`（支持 u64）与 `column_json_mysql`（**固定安全顺序** `u64 → i64 → bool → f64 → String → bytes`——`bool::compatible` 与 `f64::compatible` 都接受整型，顺序反了会把 BIGINT 读成 bool，值 256 → `false`；`real_compatible` 显式排除 `DECIMAL`，故 DECIMAL 不会被 f64 吞掉——**但它也不会落到 String 分支**：`str`/`bytes` 的 `compatible` 只认 `VarChar|*Blob|String|VarString|Enum`，故探到底也命中不了。这类「六个探测全不兼容」的列由 §6.3 D1 处置为**报错**） |
| PG / SQLite | 4 个执行点前 `reject_u64_markers`：bigint 就是 i64，装不下 u64 → **明确报错**（「store it as text or use a MySQL BIGINT UNSIGNED column」） |
| 依赖 | MySQL 插件补 sqlx 的 `mysql-rsa` feature（**实测必需**：MySQL 8 默认 `caching_sha2_password`，明文连接要走 RSA 公钥交换，缺它报 `RSA auth backend disabled` → 插件连不上任何 stock MySQL 8） |

### 2.3 验收

- `real_mysql_unsigned_bigint_roundtrips_as_u64`（真库）：`BIGINT UNSIGNED` 列写入
  `u64::MAX` 与 `i64::MAX+1` → 读回**精确**且 `as_i64().is_none()`（证明没有回绕成 -1）；
  i64 范围内的无符号值仍给 number；越 u64 的标记必须失败；
- 离线：`jsint` 的 u64 标记单测（含与 `$oj$i64` 互不误认）、`query.rs` 的参数回放断言、
  MySQL 插件的 Any+sqlite 全路径用例保持全绿（证明两路径行为未漂移）。

---

## 3. 债③ 数值型 `tenant_id` 列

### 3.1 现状

守卫注入的永远是 `Value::String(tid)`（`query.rs::apply_tenant`），insert 只认「等于租户头的
字符串」→ `tenant_id` 列为 `BIGINT`/`INTEGER` 时 PG 报 `operator does not exist: bigint = text`。
列类型当时**拿不到**：`SchemaRegistry` 的 `TableDef`/`ColumnDef` 只有 name/sortable，
`registry_tables()` 在进 registry 前把类型丢了（类型其实在 schema.yaml 解析层有）。

### 3.2 修法

| 文件 | 改动 |
|---|---|
| `src/bridge/registry.rs` | 新增 `ColumnType`（`Integer/BigInt/Text/Boolean/Double/Blob/Unknown`）；`ColumnDef` 加 `ty`（默认 `Unknown` = 旧行为）；新增 `TableDef::column_type()` 与 `table_owned_shared_typed()`（**旧的 `&[&str]` 构造器全部保留**，夹具零改动） |
| `oj/src/schema.rs` | `registry_tables()` 带出列类型（返回别名 `RegistryTable`）；`validate_tenant` 扩展：`tenant_id` 类型 ∉ {text,integer,bigint} → **启动 fail-fast**（`double`/`boolean`/`blob` 没有「等值租户 id」语义） |
| `src/bridge/query.rs` | `tenant_value()` 按列类型生成绑定值：text/Unknown → 字符串；integer/bigint → 数值（>2^53-1 用 `$oj$i64`，>i64::MAX 用 `$oj$u64`，非十进制字面量 → 指名报错）；insert 的等值判定改走 `guard::param_is_tenant`（四形态）；update sets 同理；`Join.tenant_id` 由 `Option<String>` 改 `Option<Value>`，渲染走 `to_qv` |

### 3.3 验收

离线 sqlite 夹具（`tenant_id integer`）：select 注入的是**数值**（`toSQL().params` 里出现
`7` 而非 `"7"`）、insert 未给值时强制写数值、给别的值报 mismatch、update sets 报 not allowed、
join 的 ON 参数同型、非十进制租户头报 `not a valid integer for numeric column n.tenant_id`。

---

## 4. 债④ 内建序列分配原语

### 4.1 设计：`db.nextSeq(name)`

- 平台表 `_oj_sequences(name, v)`，**首次使用自动建**（`create table if not exists`，不经模块
  schema/迁移；`_oj_` 前缀 + 未登记表 → 裸 SQL 租户守卫短路放行，且**不经** `op_db_query`，
  不构成新的通用查询通道）；
- **单语句原子取号**：
  - PG / SQLite：`insert into _oj_sequences (name, v) values ($1, 1) on conflict (name) do update
    set v = v + 1 returning v`；
  - MySQL：`insert … values (?, last_insert_id(1)) on duplicate key update v = last_insert_id(v + 1)`
    再 `select last_insert_id() as v`——**必须同一连接**（会话级变量），故无活跃事务时宿主用一次
    短事务包住；
- **MySQL 的 DDL 会隐式提交**，所以建表**绝不放在调用方事务里**：池路径先建表再开短事务；
  tx 路径先在事务里试，若失败（表不存在）则借池建表后重试一次；
- 返回值过 `jsnum` 规则（≤2^53-1 给 number，超出给十进制字符串）；序列名绑定为参数
  （不进 SQL 标识符），长度 1..=128。

### 4.2 验收

| 场景 | 用例 |
|---|---|
| 离线（sqlite） | 稠密递增 1,2,3、不同序列互不干扰、tx 内搭车、40 并发稠密、名称越界报错 |
| PG 真库 | `real_postgres_next_seq_is_atomic_under_concurrency`：20 个并发调用**排序后恰为 1..20** |
| MySQL 真库 | `real_mysql_next_seq_is_atomic_under_concurrency`：顺序 3 次（同连接）→ 1/2/3；12 个并发语句 → 最终值恰为 12（无丢失更新） |

---

## 5. 债⑤ 真库验证（执行记录）

| 目标 | 结果 |
|---|---|
| MySQL 8.4.11（`container` CLI，4C/1G，专用库 `oj_test`） | 插件全部 env-gated 用例首次实跑并全绿（roundtrip / i64 marker / u64 unsigned / 序列原子性） |
| PG 18.6（专用库 `oj_test`） | 全绿（roundtrip / bigint marker / 语句缓存混形态 / 序列原子性） |
| Redis 8 | 全绿 |
| S3（poc-minio 在 9000） | **未完成**：需预建桶与凭据（`mc mb` 建 `oj-test`；容器内 secrets 未取到） |
| RabbitMQ 4 | **未完成**：`guest` 仅 loopback，`poc` 用户需在容器内自建 |
| Kafka | **未起**（9092 closed） |

---

## 6. 评审意见与处置

两位只读评审并行进行（开发侧 general-purpose / 架构侧 Plan）。**两路意见均已处置**，见 §6.1（架构侧）
与 §6.3（开发侧）；§6.2 是两路中**本轮不采纳、改为登记**的部分。

### 6.1 已采纳

| # | 意见（架构评审） | 处置 |
|---|---|---|
| A1 | **P1-2（真 bug）`db.nextSeq` 在 `db.tx` 内首次使用必失败并毒化调用方事务**：PG 里事务内失败语句使事务进入 aborted 态（`25P02`），而原实现是「先试 → 失败才建表 → 在同一事务会话上重试」 | **已修**：改为「**每库一次的先确保**」——DDL 一律走池、在事务内取号之前完成（`ensure_seq_once` + `StableState.seq_ensured` 缓存，DDL 不落调用方事务，MySQL 的隐式提交与 PG 的 aborted 态都不再可能发生）。新增真库回归 `next_seq_first_use_inside_tx_on_real_db`（先 `drop table` 强制首次路径；**PG 与 MySQL 双跑**：tx 内首次=1、同事务第二次=2） |
| A2 | P2-1 `Pooled::Any` 分支验证不到生产 MySQL 行解码，「两路径未漂移」的说法不成立；TINYINT 描述不准确 | 采纳：插件内标注 `Pooled::Any` 为**仅测试用**；本文件 §2.2 订正为「Any+MySQL 组合本就不可用（`Any::try_from` 缺 `Tiny` 等分支），离线用例只覆盖编排/绑定通路」；补注 `real_compatible` 显式排除 DECIMAL |
| A3 | P2-2 `nextSeq` 的语义缺口（库级共享 / `name` 不可取用户输入 / 需 DDL 权限 / `memory://` 不支持 / 序列不回退） | 采纳：`docs/db-guide.md` §11 第 7 条补「四条必须知道的语义」，`docs/devkit/api-manual.md` 的 `db.nextSeq` 行同步 |
| A4 | P2-3 文档订正：§4.5 悬空引用、`ABI_VERSION 7` 过时、spec 的 TINYINT 描述、CHANGELIST 缺兼容性小节 | 采纳并全部订正（`numeric-limits.md` 两处、`plugin-architecture.md` 与 `plugin-development.md` 的 ABI 8、本文件 §2.2、CHANGELIST 新增「兼容性 / 行为变更」7 条） |
| A5 | P2-4 根 crate 的 dev-deps pg/mysql 无 core 用例使用 | **部分采纳**：本轮新增了 core 级真库用例（`next_seq_first_use_inside_tx_on_real_db` 直连真库），驱动**确有用途** → 保留并更新注释指向该用例（不再是无用依赖） |
| A6 | P2-5 `shape_tag` 的 `'u'` 分支当前不可达、与 `bind_value` 不一致 | 采纳（文档化而非删除）：保留分支并注明「不可达的原因（reject 先行）+ 若放开 u64 必须同步 `bind_value`」，删掉会在未来重排时静默退化成 `'t'` |

### 6.2 未采纳（本轮）与理由

| # | 意见 | 理由 / 处置 |
|---|---|---|
| R1 | P1-1 引入 `WIRE_VERSION` 对线形状做**硬门禁**（指纹不符即 fail） | **本轮不采纳，改为登记**（CHANGELIST「线形状版本门禁缺失」）：把加载器从「warn-only」改成 fail 本身是一次**兼容性变更**（会打断仅因 rustc/triple 变化而指纹不符的既有部署），不该夹在清账批次里顺手做；本轮以「混版本明确不受支持」的文档纪律 + 发布物成对（`bin/oj` 与 `bin/plugins/` 同批）替代，并把它列为下一条独立事项 |
| R2 | P1-3 加 CI 真库 job + `OJ_TEST_REQUIRE=1` | **本轮不采纳，改为登记**：新增 GitHub Actions service（PG+MySQL）与「缺 env 即 fail」的模式需要**在 CI 上首跑验证**，而本机没有 runner——写一个无法本地验证的 workflow 违背本仓「结论必须回源码/实证」的纪律。已把「假绿」风险与建议写进 CHANGELIST，作为独立事项排期 |
| R3 | P2-1 把「首个 compatible 胜出」彻底改为按 `MySqlTypeInfo` 显式分派（TINYINT→bool、DECIMAL→String 等） | 本轮**未做**：显式分派需要把每一种 MySQL 类型的期望 JSON 形状逐一定义（含 `BIT`/`DECIMAL`/时间类型的取舍），属独立的「MySQL 类型映射表」设计；本轮先保证**安全顺序**（整数优先、排除 DECIMAL 的 f64 误吞）与真库用例，形状表的统一另立事项 |

### 6.3 开发侧评审

开发侧（只读 reviewer，`general-purpose`）独立复核后给出 1×P0、1×P1、6×P2。**全部意见都已回源码
核实**，凡确认成立的一律本轮处置（含 P0）；结论无法支撑或属独立设计的一律登记，不夹带。

**复核确认（评审先立住的部分）**：① 生产 `postgres://` 只走 `oj-db-postgres` 插件（core accessor
不再认领该 scheme，装配层 fail-fast）→ `shape_tag` 落插件确实覆盖生产路径；② `shape_tag` 只会
**过分裂**不会欠分裂（方向安全）；③ 连接串的 `statement-cache-capacity` 确被 sqlx 识别
（`sqlx-postgres/src/options/parse.rs`）→ 不是空操作；④ `reject_u64_markers` 的 4 个插件执行点 +
4 个 core 执行点无遗漏。

| # | 意见（开发侧评审） | 处置 |
|---|---|---|
| D1 | **P0（真回归，已修）MySQL 类型化行解码把一批常见列类型静默变成 `null`**：`column_json_mysql` 的六个探测对 `DECIMAL`/`NEWDECIMAL`/`JSON`/`DATE`/`TIME`/`DATETIME`/`TIMESTAMP`/`YEAR`/`BIT` 全部 `compatible=false`（已逐条核 `sqlx-mysql` 的 `str`/`bytes`/`float`/`int`/`uint`/`bool` compatible 列表），返回 `None` → `unwrap_or(Null)` → **空值**。而 `sqlx::Any` 时代这些类型在列转换就抛 `AnyDriverError`（`sqlx-mysql/src/any.rs` 的 `TryFrom<&MySqlTypeInfo>` 只认 Null/Short/Long/LongLong/Float/Double + str/bytes 可兼容者）→ **fail-loud 退化成静默错值**，正踩本仓红线，且发生在本批刚宣布"生产化"的 MySQL 路径上 | **已修（代码）**：`row_to_json_mysql` 改返回 `Result`，未命中任何探测的列**报错**，点名列名 + `col.type_info().name()` + 指路 `cast(x as char)`；两个调用点（池 / 事务）改为 `collect::<Result<…>>()` 传播。新增 env-gated 真库回归 `real_mysql_unsupported_column_types_error_loudly`：`DECIMAL`/`DATETIME`/`JSON` 三个随机列**必报错且错误串含类型名**，同时钉住 `TEXT`/整数/`BIGINT UNSIGNED` 可读、`BOOLEAN` → `1`。**文档同步**：`docs/numeric-limits.md` §4 新增第 10 条、`docs/db-guide.md` §11 新增第 8 条 + 报错表两行、devkit `api-manual.md` 边界小段 + 报错表两行（并删掉已过时的「`u64`/`BIGINT UNSIGNED` 未支持」行）、`CHANGELIST` §兼容性新增第 8 条 |
| D2 | **P1（真 bug，已修）`seq_ensured` 用 JS 可见名当键，而物理库可能不同**：`op_db_next_seq` 把 `name`（bootstrap 传来的 `DB(name)` 名）直接当缓存键，但 `lookup` 会经 `guard::bound_db` 把字面 `"default"` 按 manifest `db:` / `db_override` 重定向。同一 Bridge 下模块 A（→`db_a`）写入键 `"default"` 后，模块 B（→`db_b`）会**命中该条目并跳过建表**：池路径白付一次失败语句 + 一次 DDL，**事务路径没有兜底 → 直接失败**——正是 A1 声称已消灭的故障类 | **已修（代码）**：键改用 `bound_db(state, name)` 的结果（物理库名；`StateLookup` 里物理名与 accessor 是 1:1）。新增离线回归 `next_seq_ddl_cache_is_keyed_by_physical_db`：两个模块把字面 `"default"` 绑到两个物理库，用记录型 accessor **直接数 `_oj_sequences` 的 DDL**（每库必须各一次）。**已实测判别力**：临时回退修复 → `(1, 0)` 红，恢复后 `(1, 1)` 绿 |
| D3 | P2 `src/bridge/db.rs` 的 `op_db_next_seq` doc 仍写「**先试后建表**…只有真正失败才补建」，与实现（每库一次先 `ensure_seq_once`、DDL 一律走池且先于取号）矛盾，将来有人照注释"优化"回去就会复现 A1 | 采纳：该段改写为「**每库一次先确保**（DDL 走池、绝不进调用方事务）+ 池路径保留兜底 + **事务路径无兜底**（靠先 ensure）」 |
| D4 | P2 `plugins/oj-db-mysql` 的 `bool` 分支**不可达**（`int_compatible` 覆盖 `bool::compatible` 的全部整型且 i64 在前），真实后果是 MySQL `BOOLEAN`/`TINYINT(1)` 读出 `1`/`0` 而非 `true`/`false`；而「Any 时代这些类型报错」意味着 BOOLEAN 是**从报错变可读**，兼容性小节只说「整数列给 number」不够点名 | 采纳：函数注释写清「不可达但保留为顺序意图标记，**勿删**」+「`BOOLEAN` 按整数读」；`CHANGELIST` §兼容性第 8 条、`numeric-limits.md` 第 10 条、`db-guide.md`、devkit `api-manual.md` 四处点名 |
| D5 | P2 `bootstrap.js` 的 `intMarker` 让 `toUBigInt("42")` 编码成 `{"$oj$i64":"42"}`——i64 范围内的「无符号意图」丢失（功能无害：MySQL 接受、数值一致），但与 d.ts/文档叙述不一致 | 采纳（**文档而非改码**）：`numeric-limits.md` 第 3 条补「无符号意图仅在 `> i64::MAX` 时体现在线形状上，故『PG/SQLite 拒绝一切 u64』对 ≤`i64::MAX` 的值不成立」。改码需让 BigInt 携带符号意图（新包装类型 = 线形状变更），不夹在本批 |
| D6 | P2 `ensure_seq_once` 的 `.lock().unwrap()`：持锁期 panic 会让这个 **Bridge 级共享**缓存永久中毒 → 此后每次 `nextSeq` 都 panic | 采纳：两处改 `unwrap_or_else(|e| e.into_inner())`（该 `HashSet` 在 panic 点前后都自洽，取回不变量成立） |
| D7 | P2 测试面：真库用例 env 未设即**静默 skip**，而 `next_seq_first_use_inside_tx_on_real_db` 是 A1（事务内首次取号）的**唯一**守卫、sqlite 与假实现都没有 aborted-tx 语义 → 该修复在 CI 上实际**无覆盖**；MySQL 并发用例只断言终值；PG 混形态的「缓存条目 ≥2」断言只钉实现细节（去掉前缀即为 0） | 采纳（**登记而非改**）：CI 缺口并入已登记的 P1-3 并在 `CHANGELIST` 写清「该修复在 CI 上实际无覆盖」。**部分已消除**：本批新增的 D2 回归改为**离线可跑**（不依赖真库），把「P1-2 类修复无离线守卫」补上 |
| D8 | P2 两处行为变更未登记：① 数值型 `tenant_id` 列在 `tenant.sql_guard: warn` 下，租户头非十进制整数时 `tenant_value` 直接 `Err`（warn 只放行「无法判定」，不放行「判定为不匹配」）——与「warn = 只告警」的直觉不同；② `update({tenant_id: 7})`（text 列、租户 `"7"`）由「拒绝」变「放行」（放宽） | 采纳：`CHANGELIST` §兼容性新增第 9、10 条逐条写明 |

**D1 连带的具体化挂账**：D1 的彻底修法是按 `MySqlTypeInfo` 显式分派（`DECIMAL`→十进制字符串、
`JSON`→对象、时间→字符串…）——这正是 §6.2 R3 的「MySQL 类型映射表」；本轮只做到「不静默错值」，
该能力缺口已作为独立事项写进 `CHANGELIST`。

**评审过程中新发现并登记的一条**：`sqlite`（`max_connections(1)`）在活跃事务期间于池上发 DDL 会
**等锁超时**，故 `db.nextSeq` **首次**在 `db.tx` 内使用在 sqlite 上会挂住（PG/MySQL 池 ≥2 无此
问题）。这是 1 连接 accessor 的通用性质（任何「事务内还去池上取连接」的路径都如此），非 `nextSeq`
引入，登记备查、未修（见 `CHANGELIST`）。

---

## 7. 过程中新发现的缺陷

1. **deno_core op 驱动并发 abort（既有，非本次引入，未修）**：
   - 现象：`op_driver/futures_unordered_driver.rs:309` 的 `self.queue.queue.borrow_mut()`
     发生 **`RefCell already borrowed`**，随后 `panic in a function that cannot unwind` →
     **SIGABRT（整个进程挂掉）**；
   - 触发（实测两种，均与本次新增的 `db.nextSeq` 无关）：
     a. JS 侧单轮并发发起的 op 数**超过 sqlx 池上限**（默认 10）——PG 上
        `Promise.all` 发 16 个 `db.query("select 1")` 即崩；10 个正常；
     b. **在 JS 里 `await` 之后再发起 op**（多轮循环里第二轮起即崩）；
   - 影响面：框架级（任何走 DB 的并发请求都可能触发），但**线上未观测到**——推测与
     `Bridge::run_with` 的事件循环驱动方式有关（测试夹具在单次 run 内密集发起 op），生产
     actor 循环的节奏不同。**需专项排查**；
   - 本轮处置：`db.nextSeq` 的真库并发用例改由**插件层**驱动（`plugins/oj-db-*`，绕开 deno_core），
     core 侧只留 ≤8 并发的离线用例；在 `CHANGELIST` 登记，并在 `db.rs` 测试处留注释指向。
2. **MySQL 8 明文连接需 `mysql-rsa`**（§2.2 已修）：这是「MySQL 真库未验证」掩盖住的真实缺口。

---

## 8. 兼容性与行为变更

- **线形状新增** `{"$oj$u64":…}`：不 bump ABI，但**宿主与第一方插件必须同批重建**
  （旧插件不认识新标记会串化成文本），已写入 `docs/plugin-architecture.md`。
- **PG 实际执行的 SQL 文本带 `/*oj:<形态>*/` 前缀**：DBA 视角（`pg_stat_activity` / 日志 /
  `EXPLAIN`）可见；`toSQL()` 与用户 SQL 字符串不受影响。
- **`toSQL().params` 的超界整数由「字符串」改为 marker**（可回放）：需要在 JS 里比较 params
  文本的代码要留意。
- **`tenant_id` 列类型新增声明期白名单**：非 text/integer/bigint → 启动报错（原先能启动、
  运行期才在 PG 上报错）。
- **MySQL 行 JSON 形状**：typed 路径下整数列统一给 number（Any 时代 TINYINT 等可能落成
  字符串/字节）——已在 §2.2 的实现说明与手册中标注。
- **`toBigInt` 对越 i64 的 bigint 由「静默返回」改为明确报错**（此前 `typeof v === "bigint"`
  分支直接返回，导致越界值能一路传到编码层才炸）。
- **MySQL 读侧的列类型边界**（§6.3 D1/D4）：`DECIMAL`/`JSON`/时间/`BIT`/`GEOMETRY` **报错**
  （v0.1.23 是 sqlx 的 `AnyDriverError`，同样读不出，但错误不点名列）；`TINYINT`/`BOOLEAN`
  由**报错**变为可读（`1`/`0`，不是 `true`/`false`）。
- **数值型 `tenant_id` 列在 `sql_guard: warn` 下也会硬失败**（租户头非十进制整数时）——
  warn 只放行「无法判定」，不放行「判定为不匹配」。
- **`update({tenant_id: …})` 的等值判定改走 `param_is_tenant`**：text 列 + 数字租户头
  （租户 `"7"`、字段 `7`）由拒绝变放行（放宽）。
- **`db.nextSeq` 的 DDL 缓存以物理库为键**（§6.3 D2）：多模块把字面 `"default"` 绑到不同库时，
  每个库各自建表一次（此前会串键跳过建表）。
