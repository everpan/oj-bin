# 大整数与数值精度手册（Numeric Limits & BigInt）

面向**写 handler 的业务开发者**与**排障的运维**：DB 的 64 位整数（雪花 id、后期自增主键）
跨到 JS 侧会发生什么、正确写法是什么、报错怎么归因。规则背景见 `user-manual.md`，
实现走读见 `modules/07-data-layer.md`，下游真实事故记录见
`web/plane/docs/ever/upstream-requests-2.md` U38。

一句话：**JS 的 `number` 只有 f64 精度（安全整数上界 `2^53-1`），所以超界整数从 DB 读出来是
十进制字符串，写回去必须用 `toBigInt()`。**

---

## 1. 契约：i64 按值域分流

| DB 值（BIGINT / INTEGER） | JS 侧类型 | 例 |
|---|---|---|
| `\|v\| ≤ 2^53-1`（9007199254740991） | `number` | `42`、`-7` |
| `\|v\| > 2^53-1`、`i64::MIN`…`i64::MAX` | **`string`（十进制，逐字精确）** | `"4886674138783273204"` |
| f64 / REAL / DOUBLE / FLOAT | `number`（不变） | `1.5` |
| TEXT / 任何非整数 | `string`（不变） | `"abc"` |

阈值与 V8/serde_v8 的 `MAX_SAFE_INTEGER` **逐字一致**（`(1<<53)-1`）。为什么不是 `2^53`：
`2^53` 本身已不可安全表示（`Number.isSafeInteger(2**53) === false`），一并走字符串。

**为什么读出来是字符串而不是 bigint**：`JSON.stringify(1n)` 会抛
`TypeError: Do not know how to serialize a BigInt`——若交给 JS 的是 BigInt，任何
`json.ok(rows)` 都是 500（v0.1.21 及更早的实测行为）。字符串是 JSON 唯一无损的 64 位表示，
也是业界对 JSON 大整数的通行做法。**JSON 出线上永远是字符串**（无论哪种写法）。

`Json` / `Row` 类型无需变更（本就含 `string`）；但**改过 `typeof id === "number"` 判断的代码
要复查**——超界后它是 `string`。

### 命中的列

- 雪花 / 发号器 id、`BIGINT PRIMARY KEY` 长到后期（Snowflake ≈ 19 位十进制，必然命中）
- `count(*)`、`max(id)`、`sum()` 等聚合（行数/量级越大越可能命中）
- ES 响应里的 `long` 字段（`es.search` 同样按本节规则处理）

---

## 2. 安全出口：`toBigInt()` / `toUBigInt()` / `toDouble()`

```ts
const m    = rows[0].m;              // string（超界时）
const next = toBigInt(m) + 1n;       // bigint，逐位精确
await db.exec("insert into seq (id) values (?)", [next]);   // 精确回写
json.ok({ id: next });               // 出线："4886674138783273205"
```

### `toBigInt(v): bigint`

把字符串/数字转成精确的 64 位整数（**fail-loud**，绝不静默取近似）：

| 输入 | 结果 |
|---|---|
| `"4886674138783273204"` | `4886674138783273204n` ✅ |
| `"0"` / `"-42"` / `42` / `7n` | 对应 bigint ✅ |
| `Number("4886674138783273204")`（= 已坍缩的 f64） | **throw** `TypeError`：`is not a safe integer (|v| > 2^53-1) …` |
| `9007199254740992`（字面量本身已超界） | **throw**（同上） |
| `1.5` / `"1.5"` / `"abc"` / `"007"` / `"+1"` / `" 1"` / `"-0"` | **throw** `TypeError`（非规范十进制） |
| `"9223372036854775808"`（超 i64） | **throw** `RangeError` |
| `null` / `true` / `{}` | **throw** `TypeError` |

> 关键：任何 `|v| > 2^53-1` 的 f64 都不是 safe integer，因此
> **`toBigInt(Number(大整数字符串))` 必然抛错**——这正是把 U38 那类事故挡在调用点的机制。

### `toUBigInt(v): bigint`（v0.1.24）

与 `toBigInt` 同形，但值域是**无符号** `[0, 2^64-1]`：

- **只有 MySQL `BIGINT UNSIGNED` 列能承载**；PG / SQLite 传它会得到明确报错（它们的 bigint
  就是 i64），请改存 text。
- 同样 fail-loud：非规范十进制串（`"+1"` / `"007"` / `" 1"`）、负数、超 `u64::MAX`、
  已坍缩的 number 一律抛错。
- 跨 op 时编码为 `{"$oj$u64":"<十进制>"}`（与 `$oj$i64` 同族保留键）。

### `toDouble(v): number`

显式接受 f64 语义（可能丢精度，由你用）：`number` / 数字串 / `bigint` → `number`。
字符串按 **`Number()` 语义**解析（`"1.5"`、`"1e3"`、`"0x10"`(=16)、`"Infinity"` 都接受），
空串与非数字串（`"abc"`）throw；其余类型（`null`/`true`/`{}`）throw。
**不做 `toFloat`**：JS 的 `Number` 就是 f64，没有独立的 f32 语义。

```ts
const ratio = toDouble(row.total) / toDouble(row.count);   // 明确知道自己在用浮点
```

### 回写规则：字符串是文本意图，BigInt 是整数意图

平台**不做启发式猜测**（不会把长得像数字的字符串转成整数）：

| 回写形态 | 绑定类型 | PostgreSQL 上写 bigint 列 |
|---|---|---|
| `"4886674138783273204"`（字符串） | text | ❌ `column "id" is of type bigint but expression is of type text` |
| `toBigInt("4886674138783273204")` | int8 | ✅ |
| `4886674138783273204`（数字字面量） | int8（但**值已在源码层坍缩**） | ⚠️ 静默写错值 |

所以：**凡是 64 位整数，读出来是字符串、写回去用 `toBigInt()`。** 回传字符串在 PG 上必然报错
（MySQL/SQLite 会隐式转换，但**不要依赖**方言差异）。

---

## 3. BigInt 范式（序列号 / 雪花 id 生成）

### 3.1 `max(id)+1` —— 最小改写

```ts
// ✗ 事故写法：Number() 会静默坍缩到 f64 网格
const rows = await tx.query("select max(id) as m from identifiers", []);
const seq  = Number(rows[0].m) + 1;

// ✅ 范式
const rows = await tx.query("select max(id) as m from identifiers", []);
const seq  = toBigInt(rows[0]?.m ?? "0") + 1n;
await tx.query("insert into identifiers (id) values (?)", [seq]);
```

`toBigInt(0)` 与 `toBigInt("0")` 都可用；`?? "0"` 处理空表（`max` 返回 `null`）。

### 3.2 它为什么必须改写：真实事故链（下游实测）

```
max(id) = 4886674138783273204  →  JS 侧 typeof m === "bigint"/超界时 string，String(m) 精确
Number(m)        = 4886674138783272960   ← 精确 f64 值，与真值差 244
Number(m) + 1    = 4886674138783272960   ← +1 被 f64 吸收（1 小于该量级的网格间距）
```
该网格值一旦入库成为 `max`，下次分配算出**同一个**值 → `duplicate key value violates
unique constraint` → 业务接口 500，且从"静默写错"到"爆发 500"有延迟，极难归因。

> 平台**拦不住** `Number("<大整数串>")`（那是 JS 语言语义）。可拦的是**回写与显式转换**：
> `toBigInt()` 对已坍缩的值抛错。故范式必须写对。

### 3.3 并发：`max+1` 本身有竞态

多请求并发读同一个 `max` 会分配出相同序号（与精度无关，是并发问题）。终态是**原子分配**：
- 单库：数据库序列（PG `nextval` / MySQL `AUTO_INCREMENT`）或
  `INSERT ... RETURNING`（PG/SQLite）/ 行锁（`select ... for update`）+ 同一事务内回写；
- 不想自建：把发号收敛到一个 handler，用 `db.tx` + 唯一索引冲突重试兜底。

**v0.1.24 起平台内建了原语**：`await db.nextSeq("标识符分配")` —— 单语句原子取号
（PG/SQLite `insert … on conflict do update … returning`；MySQL `last_insert_id` 惯用法），
平台表 `_oj_sequences` 首次使用自动建。`max+1` 的范式改写见 §3.1，**新代码请直接用 `nextSeq`**：
它把「取号」的并发正确性收敛到平台，不再需要调用方自己拿行锁或唯一索引重试。

---

## 4. 代价与边界

1. **`Number("<大整数串>")` 仍会静默坍缩**——平台无法拦截，只能靠 §3 范式。
2. **`es` / `bus` / `mq` / `jwt` / `ws.sess.state` 等边界不容忍 bigint**：这些 op 走
   serde_v8 反序列化，BigInt 直接报 `unsupported type`。跨这些边界先 `String(v)`。
   （`json.ok` / `json.fail` / `json.raw` / `log` 字段 / `mail` 已容忍 bigint，自动序列化为
   十进制字符串。）
3. **`u64` / `BIGINT UNSIGNED`（v0.1.24 起支持）**：MySQL 走 **typed 路径**（`sqlx::MySql`）
   ——读侧 `> i64::MAX` 的 unsigned 值给 number（`>2^53-1` 再由出口护栏降十进制字符串），
   **不再回绕成负数**；写侧用 `toUBigInt(v)`（`[0, 2^64-1]`）精确绑 u64。
   PG / SQLite 的 `bigint` 就是 i64，**装不下 u64**：传 `toUBigInt` 会得到明确报错
   （`u64 value … is not supported on this path`），请存成 text 或用 MySQL。
   旧插件（未随本版重建）遇到 `$oj$u64` 会被串化成文本——**插件须与宿主同批重建**。
   - **注意**：`toUBigInt` 的「无符号意图」只在**超过 `i64::MAX`** 时才体现在线形状上。
     值落在 i64 范围内时（如 `toUBigInt("42")`）JS 的 BigInt 已不携带符号信息，编码为
     `{"$oj$i64":"42"}` → PG/SQLite 也会照常接受。功能上无害（数值一致、MySQL 亦接受），
     但「PG/SQLite 会拒绝一切 u64」的说法对 ≤`i64::MAX` 的值不成立。
4. **`toDouble` 丢精度是显式意图**，平台不再二次告警。
5. **PG 语句缓存 × 混合参数类型（v0.1.24 起平台自动处置，用户无需再规避）**：
   历史上同一条 SQL 文本若在不同调用里绑定**不同 Rust 类型**的参数（一会儿字符串、一会儿数字），
   sqlx 的 prepared statement 缓存（key 只有 SQL 文本）会复用旧的参数类型元数据 → 协议级错误
   （`invalid byte sequence … 0x00` / `insufficient data left in message` / `incorrect binary
   data format in bind parameter`）。现在 **PG 插件按参数形态给 SQL 前置一段签名注释**
   （`/*oj:<形态>*/`，形态字母表 `t/i/f/b/m/u`），不同形态自然落到不同缓存条目；
   用户**不再需要**「保持形态稳定」或换 SQL 文本。
   - **DBA 可见性**：实际执行的 SQL 文本会带该前缀（`pg_stat_activity` / 日志 / `EXPLAIN`
     可见）；`toSQL()` 与用户自己的 SQL 字符串**不含**前缀。
   - MySQL 无此问题（每次 execute 都重发参数类型），SQLite 无声明参数类型——两者都不打签名。
   - `pg_prepared_statements` 的条目数会随「SQL 数 × 参数形态数」增长（连接级
     `statement-cache-capacity` 已上调到 512 兜底）。
6. **保留形状**：`{"$oj$i64":"<十进制>"}` 是 `toBigInt()` 参数的内部编码。业务 JSON 数据
   **不要**以该键为唯一键，否则在**参数位置**会被当作整数绑定。**这是一条红线**：
   保留键前缀 `$oj$`（v0.1.24 起另有 `{"$oj$u64":"<十进制>"}`，`toUBigInt()` 的参数编码），
   业务数据不得使用。
7. `schema.yaml` 列类型最小集里没有 `BIGINT UNSIGNED`；bigint 列按 `bigint` 声明即可。
8. **`kv.incr` 返回 f64**：计数器超过 `2^53` 后会**先丢精度**（与 DB 读侧不同，kv 无护栏）。
   需要精确大计数时改用 DB 或自行以字符串存储。
9. **数值型租户 id 列：v0.1.24 起支持**。租户守卫按 `schema.yaml` 声明的 `tenant_id`
   列类型生成绑定值：
   - `text`（及旧装配路径的「未声明类型」）→ 字符串（与 v0.1.23 相同的旧行为）；
   - `integer` / `bigint` → **数值**（`> 2^53-1` 用 i64 标记，`> i64::MAX` 用 `$oj$u64`；
     PG/SQLite 对后者报错 → 数值租户 id 请落在 i64 内）；
   - insert/update 的租户等值判定接受 **4 种形态**（字符串 / 数字 / i64 标记 / u64 标记）——
     即 `tenant_id: "7"`、`tenant_id: 7`、`toBigInt("7")` 都可；
   - 租户头（或 `db.asTenant(id)`）在数值列上**必须是十进制字面量**，否则报
     `tenant guard: tenant id "acme" is not a valid integer for numeric column t.tenant_id`。
   **新增声明期 fail-fast**：`tenant_id` 列类型 ∉ {`text`,`integer`,`bigint`} →
   启动即报错（`double`/`boolean`/`blob` 没有「等值租户 id」语义）。
   注意 SQLite 是动态类型：列声明为 INTEGER 就必须存整数形态。
10. **MySQL 读侧的列类型边界（v0.1.24 定稿）**：typed 路径（`sqlx::MySql`）下**可读**的列类型是
    **整数家族**（含 `BIGINT UNSIGNED` → u64）、**文本家族**（`TEXT` 在 MySQL 协议里就是
    `Blob` + 非 BINARY collation）、**二进制**、`FLOAT`/`DOUBLE`；**其余一律报错**：
    `DECIMAL`/`NEWDECIMAL`、`DATE`/`TIME`/`DATETIME`/`TIMESTAMP`/`YEAR`、`JSON`、`BIT`、
    `GEOMETRY`。
    - 报错信息**点名列名与 MySQL 类型**并给指路：
      `db(mysql): column 'amount' has MySQL type 'DECIMAL' which this plugin does not decode yet
      — select explicit columns and cast it in SQL (e.g. \`cast(amount as char) as amount\`)`。
      **绝不会静默变成 `null`**（静默错值是本仓红线）。
    - 需要读这些列时**在 SQL 里显式转换**：`cast(amount as char) as amount`、`cast(made_at as char)`、
      `cast(payload as char)` —— 转换后按文本读出。
    - `BOOLEAN` / `TINYINT(1)` 读出的是 **number `1`/`0`**，不是 `true`/`false`
      （MySQL 无列长度元数据可区分「boolean 语义的 TINYINT」）。
    - 对照 v0.1.23（全程 `sqlx::Any`）：上述不可读类型当时同样读不出来，但错误是 sqlx 的
      `AnyDriverError`（不点名列）；且 `TINYINT`/`BOOLEAN` 当时**直接报错**，现在可读。

---

## 4.1 存量代码自查清单（升级到 v0.1.22 时）

读侧由 BigInt 改为字符串**只会影响原本必然 500 的路径**（无兼容性风险），但代码里若存在
「把 id 当 number 用」的假设，需要排查：

```bash
# ① 大整数参与算术/比较（最危险：静默坍缩）
grep -rnE 'Number\s*\(.*\b(id|_id|seq|snow)' --include=*.ts src/ tests/
# ② 从 DB 来的值做 typeof number 判断
grep -rnE 'typeof .*=== *"number"' --include=*.ts src/
# ③ max/min/sum 聚合结果直接参与运算
grep -rnE '(max|min|sum)\s*\(' --include=*.ts src/
# ④ 主键/外键参与 JSON 输出前的类型断言（前端可能断言 number）
grep -rnE 'JSON\.parse|as number' --include=*.ts src/
```

逐条处置：凡是「DB 读出的 id」→ 一律按**字符串**传递，需要运算时 `toBigInt()`；
取号改用 `db.nextSeq(name)`（§3.3）。**判定标准：`row.id + 1`、`Number(row.id)`、
`typeof row.id === "number"` 三处命中即需改。**

---

## 5. 排障

| 症状 | 归因 | 处置 |
|---|---|---|
| `TypeError: Do not know how to serialize a BigInt` | v0.1.21 及更早：读到大整数时 JS 侧拿到 BigInt，`json.ok` 序列化失败 | 升级；本版读侧已降为字符串 |
| 主键 `duplicate key`，且被撞的值末几位是 0 | `Number(max)+1` 坍缩到 f64 网格值（§3.2） | 按 §3.1 改写范式；清理已写入的网格值行 |
| `column "x" is of type bigint but expression is of type text`（PG） | 回写用了**字符串** | 用 `toBigInt(...)` |
| `toBigInt: … is not a safe integer` | 传进来的是已坍缩的 number（多为 `Number(...)` 的产物） | 传**原始字符串**（DB 读出来的那个值） |
| `toBigInt: expected a canonical decimal integer string` | 传了 `"1.5"` / `"007"` / 带空格等 | 传 DB 原样给出的十进制串 |
| `unsupported type`（`es`/`bus`/`mq`/`jwt`/`ws.sess.state` 等） | bigint 跨了不容忍的边界 | 先 `String(v)`（`json.*` / `log` / `mail` 已容忍） |
| `invalid byte sequence for encoding "UTF8": 0x00` / `insufficient data left in message` | v0.1.24 前：同一 SQL 文本混用不同类型参数（见 §4 第 5 条） | v0.1.24 起平台自动分缓存键，无需规避；若仍出现请确认 PG 插件已随宿主重建 |

---

## 6. 评审意见与处置

v0.1.22 交付时按既有流程跑了**开发侧 + 架构侧双专家评审**（并行、只读），逐条处置如下。
两侧共同背书的部分：阈值与 serde_v8 逐字一致（含 u64 分支）、护栏放 op 出口而非行解码、
标记协议是当前约束下的最优解（自定义 serde 类型被 `pub(crate)` 的 magic trait 与
`ValueType::BigInt => UnsupportedType` 双重阻断）、「读串 / 写 bigint」不对称是正确长期契约、
`toBigInt` fail-loud 与"不做 toFloat"、`json.fail` 改 `#[string]` 预序列化、u64 回绕修法。

**已采纳并修改**

| # | 意见（开发侧 / 架构侧） | 处置 |
|---|---|---|
| 1 | `encodeParams` 把 `Date` 压成 `{}`，破坏 `toJSON`/子查询快照原有的 JSON 语义（P1） | `encodeParams` 对带 `toJSON` 的对象改为遵循 JSON 语义（`Date` → ISO 串）；快照 API 单独用 `encodeSnapshot`（JSON 往返 + bigint 标记）。实测断言见 `bigint_covers_remaining_js_entry_points` |
| 2 | CHANGELOG 把「订正 `CLAUDE.md` ABI」列入修复，但该文件是 **gitignored/untracked**，diff 里看不到（P1） | 从 CHANGELOG 删除该条（文件本身已订正 7→8，仅作本地指引） |
| 3 | 测试缺口：`db.query` 参数、`toSQL().params`、`toJSON/fromJSON`、`having`、CASE `then`、`union/with` 子查询、`Qv::BigUnsigned`（P1） | 新增 `bigint_covers_remaining_js_entry_points`（六项全覆盖 + Date 语义）、`value_to_json_keeps_u64_and_bigint_exact` |
| 4 | `jwt.verify` 的 claims 是**外部可控** JSON，未过护栏 → 雪花量级声明会让 `json.ok(claims)` 500（架构侧 P2） | `op_jwt_verify` 出口加护栏（同类：`op_mail_result`） |
| 5 | `toDouble` 比文档宽（接受 `0x10` / `Infinity`）（P2） | 文档按实现订正为「`Number()` 语义」并给出接受示例 |
| 6 | `mail` 在手册里"容忍/不容忍"自相矛盾；`kv.incr` 超 2^53 丢精度未记（P2） | 手册统一为「`json.*` / `log` / `mail` 已容忍」；新增 `kv.incr` 边界条目 |
| 7 | 二进制参数（`Uint8Array`）被静默摊平成 `{"0":..}`（P2） | `encodeParams` 显式 `TypeError`。**实测确认改动前也是 TypeError**（serde_v8 类型错），故非行为变更，仅消息更清晰 |
| 8 | op 出口覆盖面无清单可核对；缺"存量代码怎么查"（P2） | `jsnum.rs` 模块注释加**覆盖清单**（已覆盖 6 处 + 有意不覆盖 4 类及理由）；新增 §4.1 自查清单（含 grep） |
| 9 | 保留键应升级为红线（P2） | 手册 §4.6 标为红线，devkit api-manual 同步 |
| 10 | 守卫接受数字/标记形态，但构造器 insert 只认字符串——需点明"有意不对称"（P2） | `param_is_tenant` 文档注明；§4.9 登记「数值型租户 id 列不受支持」 |

**未采纳（附理由）**

| 意见 | 不采纳理由 |
|---|---|
| 给 `op_db_exec` 的受影响行数、`es.index/del` 加护栏（P2） | 前者需 ≥ 2^53 行受影响、后者响应只有 `_version`/`_seq_no` 等小整数——量级不可达，加分支只增行为面（已记入覆盖清单的"有意不覆盖"） |
| CI 加「新增 `#[serde] Value` op 必须登记」的对账（P2） | 收益（防未来漏点）低于维护成本；本次以 `jsnum.rs` 注释清单 + 文档取代，待真出现漏点再做工具化 |
| 在 `bind_value` 拒绝"不安全的 number"（早前方案） | 会误杀合法的 **大 double 列**（`REAL/DOUBLE` 的值本就不精确）；该陷阱只由 `toBigInt` + 文档拦截 |
| `toBigInt` 加命名空间（`oj.toBigInt`）、造包装类、升级为多键带版本标记、推 serde_v8 上游、给 `es/bus/mq` 各加编码层、新增 `toFloat`（P2/P3） | 均属过度设计：本仓其它全局（`json`/`db`/`log`…）同为 `globalThis` 直挂；包装类经 serde_v8 会摊平成普通对象而失去可判别性；上游改造面远超本需求；`es/bus/mq` 的输入侧限制是 serde_v8 本身 |
| 本版一并统一"数值租户列"、`u64`、序列原语、PG 语句缓存隐患（架构侧 §7） | 前两者需 schema 感知的列类型/跨方言验证，序列原语是独立特性，PG 缓存隐患**既有且与本次无关**（v0.1.21 用纯数字+字符串即可复现）——统一登记为另案，避免把本次改动面撑大 |
| 读侧也返回 bigint（只让 JSON 出线字符串）的备选方案 | 会让 `row.id + 1` 抛混合类型错、且每个 JSON 出口都要打补丁；字符串读 + 显式 `toBigInt` 的模型更可预期（本版已拍板） |

