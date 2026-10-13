# 统一绑定参数占位符设计（`?` 唯一化 + 宿主改写）

- 日期：2026-10-13（v2：并三路专家评审 23 条，处置见文末）
- 状态：设计定稿（方案 A；评审后无未决 blocker）
- 版本：0.1.66 → 0.1.67
- 关联：spec 2026-10-11（`db.dialect()` / search / versions）；用户报障「postgres 驱动遇 `?` 报 `syntax error at end of input`」

## 背景与问题

oj 三引擎绑定参数占位符不同：sqlite `?`、mysql `?`、postgres `$n`（`$1`/`$2`…）。
`db.dialect()`（0.1.66）已提供方言查询，vendored `search.ts:62` / `versions.ts:63`
据此各自手写局部 `P(i)` 生成占位符；但业务层裸 `db.query` / `db.exec` 直接写死 `?`，
postgres 驱动无法解析参数位置。

现状代码事实：

- 裸 SQL 的 JS 出口为 `op_db_query`（db.rs:389）、`op_db_exec`（db.rs:497）、
  `op_db_stream_open`（db.rs:543），经 `resolve_target` 分发；
  构造器路径 `op_db_query_build` → `build_sql(dialect, ...)` 自产方言 SQL。
- 核心后端走 sqlx Any（`accessor_sqlx.rs`），Any 层**不转换**占位符；
  pg 生产路径走 `oj-db-postgres` 插件（tokio-postgres，直透零改写）。

## 方案选型（三选一，已定 A）

- **A（采用）**：`?` 是 oj 的唯一占位符；宿主 op 层对 postgres 自动改写 `?` → `$n`。
- B：bootstrap.js JS 层包装 db 全局——出口分散、易漏、规则离引擎远。
- C：显式 `db.ph(i)` API——零解析风险但存量 SQL 全要改，「统一」靠人守纪律。

选 A 的理由：唯一漏斗（三个 op）一处生效，覆盖 query/exec/stream + 池 + tx + 插件后端；
`?` 已是 sqlite/mysql 事实标准，存量业务代码零改动；vendored `P()` 续号兼容。

## §1 改写器规格

新增纯函数模块 `src/bridge/placeholder.rs`：

```rust
/// postgres 专用：把裸 `?` 占位符按出现顺序改写为 `$n`。
/// 返回 (改写后 SQL, 改写后最大参数号 n)。无改动时 `Cow::Borrowed`（零分配）。
/// 未闭合 dollar-quote 报 `Err`（评审 E5 连带修订：纯函数签名带 Result）。
pub fn normalize_placeholders(sql: &str) -> Result<(Cow<'_, str>, usize), String>;
```

扫描规则（**单遍**：边扫边记录 `?` 位置与 max-n，末段一次改写；两遍实现为禁）：

1. **跳过引用区**，区域内的 `?` 与 `$n` 一律不识别：
   - `'...'`（含 `''` 翻倍转义）；
   - `E'...'`/`e'...'`（**反斜杠转义感知**：`\'` 不终结字符串）；
   - `"..."` 标识符；
   - `--` 行注释（CRLF/LF 均为终止）与 `/* */` 块注释（**非嵌套**，钉死为 PG 语义）；
   - dollar-quote：`$tag$...$tag$`，**含空 tag `$$...$$`**；tag 识别优先于参数识别
     （tag 不以数字开头，`$1` 永不与 dollar-quote 歧义；纯数字 tag `$1$` 属 dollar-quote）。
2. **运算符豁免**：`?` 后紧跟 `[|&<>=#~\-]`（`?|`/`?&` jsonb 与 `?#`/`?-`/`?-|`/`?<`/`?>`/
   `?=`/`?~` 几何运算符）不改写。
3. **`$n` 识别边界**：仅当 `$` 前一字符不是 `[A-Za-z0-9_$]`（标识符如 `col$1` 不算），
   且 n ≥ 1（`$0` 非 PG 参数引用，不识别）。
4. **续号与计数语义**（钉死，消除歧义）：
   - `n_max` = 改写后 SQL 中被引用的**最大**参数号（非出现次数；`$1, $1` 复用合法，
     `$2` 缺 `$1` 合法——PG prepare 的参数个数即 max n）；
   - `?` 从 `n_max + 1` 起连续递增；
   - 返回的「占位符数」= 改写后的 `n_max`。
5. **fail-loud**：占位符数 ≠ `params.len()` 时报错，文案（进单测断言）：

   ```
   placeholder count 3 != params 2
   hint: oj 统一占位符为 `?`（postgres 由宿主自动改写为 $n）；jsonb 存在性判断请用 jsonb_exists(doc, 'key')
   ```

   不附 SQL 原文片段（错误信封可能带出字面量敏感值；SQL 全文已由 sql_trace 承载）。
   **仅 postgres 触发**——sqlite/mysql 的占位符/参数数不匹配仍由引擎在执行期报错，
   「统一占位符」≠「统一校验」，错误口径按引擎分列进 api-manual 错误表。
6. **触发条件**：仅 `dialect == postgres && !params.is_empty()`；sqlite/mysql 原样透传
   （`Cow::Borrowed` 零分配）；postgres 空参同样透传（无参 SQL 的 `?` 多为 jsonb 运算符）。
7. **未闭合 dollar-quote**：扫描到 SQL 结束仍未闭合 → 专用报错
   `unterminated dollar-quoted string`（勿落到计数文案误导排错方向）。

已知限制（全部文档化，见 §4 限制矩阵）：
- JSONB **裸 `?`** + 非空参混合（如 `where doc ? 'a' and id = $1`）无法与占位符区分：
  `?` 会被改写 → 静默错绑。**PG 下 jsonb 存在性判断一律 `jsonb_exists(doc, 'key')`**。
- 同一语句「无参时可跑、加参即坏」的行为翻转是此限制的表现形态，写入矩阵。

## §2 落点与数据流

改动面（3 处，全部宿主侧）：

1. 新文件 `src/bridge/placeholder.rs`：纯函数 + 对抗性单测，不依赖运行时状态。
2. `ActiveTx` 补 `dialect: Dialect` 字段（db.rs:211）：`op_db_tx_begin` 在 `begin()` 成功后
   记下 `da.dialect()`（`op_db_stream_open` 明确拒绝事务 db.rs:554，stream 无需 Tx 方言）。
   **否决备选并记录**：给 `TxSession` trait 加 `dialect()` 需改 SqlxTx/InMemoryTx 与全部夹具；
   走 FFI 路线则要 bump `ABI_VERSION`（严格相等门禁）——方言是 accessor 静态属性，
   begin 时快照语义正确且零 ABI 影响。
3. 三个裸 SQL op 各调一行共享入口，**插桩次序钉死**：

```
check_raw / check_tenant_raw（守卫吃原始 SQL）
  → resolve_target
  → normalize_sql(dialect, sql, params)     ← 唯一改写点
  → 执行（sqlx Any / 插件后端 / stream pump）
  → trace_sql(原始 SQL)；fail-loud 分支同样记 trace（ok=false），不留画像盲区
```

```rust
// db.rs：三个 op 各自只调一行（DRY）。
// 签名收窄为 dialect 而非 Target：方言规则可脱离 resolve_target 单测，
// Target→Dialect 解包留在 op 层薄分发（单一职责；开闭性归这里）。
fn normalize_sql(dialect: Dialect, sql: &str, params: &[Value])
    -> Result<Cow<'_, str>, JsErrorBox>
```

**FFI dialect 缓存**：`FfiDataAccessor::dialect()`（ffi.rs:393）现每次 vtable 往返；
仿 `FfiDbBackend` 构造期读 `schemes()` 的做法，connect 时缓存 dialect 字段，避免插件后端
每条 PG 裸 SQL 多付一次 FFI。

**守护测试**：枚举 op 注册表，断言「裸 SQL 出口 ⊆ {op_db_query, op_db_exec,
op_db_stream_open}」——把「唯一漏斗」从文档承诺变成 CI 资产，防未来新增 op 漏插桩。

明确不动的：`query.rs` 构造器；`accessor_sqlx.rs` 与插件 db 后端（op 层改写对任何后端
透明；`oj-db-postgres` 对 SQL 零改写、直透，已核实无双重改写）；`op_db_next_seq`；
`bootstrap.js` 的 `db.dialect()` op（vendored 平台件继续用）。

**sql_trace 记原始 SQL**（改写前）：画像/排错定位「开发者写了什么」，避免 trace 出现
与业务源码对不上的 `$n`（配集成断言，见 §3③）。

## §3 测试与门禁（测试用例优先 / TDD）

实现顺序：先落 §3① 全部单测（红）→ 实现扫描器（绿）→ 集成/e2e 随实现先落后跑通。
每个 fail-loud 用例即一次「击穿尝试」。

### ① `placeholder.rs` 单元测试（对抗性探测）

| 类别 | 用例 | 期望 |
|---|---|---|
| 击穿：单引号 | `select 'a?b' from t where id = ?` | 字面量 `?` 保留，仅末位 → `$1` |
| 击穿：`''` 翻倍 | `where note = 'it''s ? ok' and id = ?` | 串内保留 |
| 击穿：E-string | `select E'a\'?\' as x, ?` | `\'` 不终结串，串内 `?` 保留 |
| 击穿：注释 | `where note = '-- ?? x' /* ? */ and id = ?` | 注释内全保留 |
| 击穿：dollar-quote | `where body = $tag$? $tag$ and id = ?` | 内部保留 |
| 击穿：空 tag | `$$ ? $$ and id = ?` | 内部保留，`?` → `$1` |
| 击穿：数字 tag | `$1$? $1$ and id = ?` | tag 优先，内部保留 |
| 未闭合 dollar-quote | `$tag$ 未闭合 + ?` | 专用报错（非计数文案） |
| 击穿：标识符 | `select "col?x" from t where id = ?` | 双引号内保留 |
| 击穿：jsonb | `where doc ?\| 'a','b' and id = ?` | `?\|` 不改写，`?` → `$1`（运算符不占参数位） |
| 击穿：几何 | `where box1 ?> point(1,1) and id = ?` | `?>` 不改写，`?` → `$1`（同上） |
| 击穿：`$n` 边界 | `select col$1 from t where a = ?` | `col$1` 不计 max，`?` → `$1` |
| 击穿：串内 `$n` | `select '$1' as x from t where a = ?` | 串内不计 max，`?` → `$1` |
| `$0` | `where id = $0 and a = ?` | `$0` 不识别 |
| 续号 | `... where a = $1 and b = ?` | `?` → `$2` |
| 续号 + 乱序 | `$3 ... $1 ... ?` | `?` → `$4` |
| 复用 | `where a = $1 or b = $1` + params 1 | 透传通过（max 计法不误伤） |
| 缺口 | `where a = $2` + params 2 | 透传通过 |
| 计数 fail-loud | 2 个 `?` + params.len()==1 | 报错含两个数字 + hint 文案断言 |
| 空参数短路 | `select doc ? 'a'` + params 空 | 整条不改写 |
| 类型转换 | `where a = ?::text` | 改写后 `$1::text` 合法 |
| 退化输入 | 空串 / 无占位符 / 全 `?` 无引用 | 顺序编号 1..n |
| 块注释非嵌套 | `/* /* ? */ ? */`（无参） | 首个 `*/` 即闭合，外段 `?` 被改写（PG 语义） |
| 注释互嵌 | `/* -- ? */`、`-- /*\n?`、CRLF 行尾 | 按扫描器规则钉死 |

### ② Bridge 集成测（sqlite 侧防回归）

- 现有 `accessor_sqlx.rs` 裸 `?` 用例全部原样通过（即门禁）。
- dialect 分支单测：`InMemoryAccessor` 断言 sqlite 透传、仅 postgres 改写。
- 守护测试：裸 SQL 出口白名单（§2）。

### ③ 真库 e2e（DSN 缺失静默 skip，沿用现有惯例）

- **路径锚定**：PG 用例**经 `oj-db-postgres` 插件后端**执行（生产路径），非 core accessor。
- PG：`db.query("... where id = ? and name = ?", [..])` 正常返回（原始报障场景）。
- PG：`$1` + `?` 混用续号；`db.tx` 内裸 `?` 同场景。
- PG：字面量 `?` 不误伤（`insert ... values ('a?b', ?)` 读回逐字相等）。
- PG：**stream 出口**带 `?` 带参，行数/内容正确。
- PG：计数不匹配 → 报错文案含两个数字 + hint（击穿验证报错本身）。
- trace 断言：执行带 `?` 查询后，sql_trace 输出含原始 `?` 而非 `$n`。
- 三引擎终态验收：`sample/src/user/profile/api.ts:12` 的 update + 一条多参 select，
  同一段业务 SQL 三 DSN 全跑通。

### ④ 门禁

`cargo test`（含新单测）+ 真库 e2e 本地跑 + sample 单测不红。

## SOLID 落法

- 单一职责：`placeholder.rs` 只做扫描+改写+计数；「何时改写」归 op 层 `normalize_sql`。
- DRY：三个 op 共享 `normalize_sql` 一处插桩；签名收窄为 `Dialect` 入参，方言规则可独立单测。
- 开闭：未来其他后端方言规则只扩展 `normalize_sql` 分发，不改各 op。

## §4 文档与版本

- 版本 0.1.67；**纯行为增强、无迁移指南**（无破坏面），CHANGELOG 按**三类人群**各写一句：
  1. 一直写 `?` 的用户——pg 报错修复，零改动；
  2. 一直写 `$n` 的用户——继续有效，`$n`+`?` 混用按序号续接，**无需任何修改**；
  3. 用 `?|`/`?&`（及几何 `?x` 运算符）的用户——不受影响。
  并明示：计数 fail-loud **仅 postgres** 触发，sqlite/mysql 维持引擎报错口径。
- **devkit 四件 + db-guide 存量旧规则反转清单**（0.1.67 后旧表述零残留，纳入契约校验）：
  - `docs/devkit/SKILL.md:100` 陷阱表：改为「postgres 也认 `?`（0.1.67 起统一占位符，
    宿主自动改写 `$n`）；若坚持写 `$n`，个数必须与参数严格一致」；
  - `docs/devkit/api-manual.md:765-766 / 2697-2698 / 3427`、`docs/devkit/scenarios.md:2011`、
    `docs/db-guide.md:207-208 / 592`：同步反转；
  - api-manual db 节新增：「`?` 是统一占位符」章节 + **PG × JSONB `?` × 参数空/非空行为矩阵**
    （`?|`/`?&`/几何豁免同表）+ 错误表按引擎分列 + 「同一段 SQL、三 DSN 跑通」最小示例；
  - SKILL.md 陷阱表钉死：**PG 下 jsonb `?` 运算符禁用，一律 `jsonb_exists(doc, 'key')`**；
    E-string 转义引号内的 `?` 不受保护（已知限制同表）。
- sample：`user/profile/api.ts:12` 上方加一行注释（`?` 三引擎通用，无需 dialect 分支）。
- **oj-plugin-ffi db 轴契约**（cfg JSON 演进，无需 bump ABI）：「SQL 到达 vtable 时占位符
  已是方言终态，插件不得再改写」——防第三方插件自带改写层与宿主撞号。

## 登记待做（不在本次，只登记不改实现）

- vendored `search.ts` / `versions.ts` 的局部 `P()` 简化为裸 `?`：**0.1.67 真库 e2e 验证
  续号规则后的下一个版本再做**（改写器与它的第一个消费者不在同一版本变化，保可归因）。

## 评审处置记录（2026-10-13，三路 23 条）

| # | 来源 | 严重度 | 意见摘要 | 处置 |
|---|---|---|---|---|
| A1 | 架构 | minor | normalize_sql 签名矛盾；插桩次序；守护测试 | 采纳：签名收窄 Dialect；次序钉死；守护测试进 §3② |
| A2 | 架构 | major | 「占位符总数」语义未定义（`$1,$1` 复用被误判） | 采纳：总数=max n，§1 规则 4 钉死 + 用例 |
| A3 | 架构 | major | JSONB 裸 `?` + 带参是静默错绑且行为翻转 | 采纳（文档化处置）：限制矩阵 + SKILL 禁用条 + CHANGELOG 已知限制 |
| A4 | 架构 | minor | ActiveTx 否定理由应记录；FFI dialect() 需缓存 | 采纳：写入 §2 |
| A5 | 架构 | minor | 插件契约「终态不改写」应入 ffi 文档；P() 简化推迟 | 采纳：§4 契约条；P() 明确推迟到 0.1.68 |
| A6 | 架构 | minor | 三引擎错误口径不一致应明示 | 采纳：§1 规则 5 + §4 |
| A7 | 架构 | minor | E-string 反斜杠转义击穿扫描器 | 采纳：E'' 转义感知 + 用例钉死 |
| E1 | 工程 | major | 同 A2（计法歧义） | 合并入 A2 |
| E2 | 工程 | major | 同 A7（E''） | 合并入 A7 |
| E3 | 工程 | major | `$n` 需标识符边界（col$1 误计） | 采纳：§1 规则 3 + 用例 |
| E4 | 工程 | major | 几何运算符 `?#/?-/?<…` 豁免面不全 | 采纳：豁免集扩为 `[|&<>=#~\-]` + 用例 |
| E5 | 工程 | minor | 空 tag `$$`、未闭合 dollar-quote 行为未钉死 | 采纳：§1 规则 1/7 + 用例 |
| E6 | 工程 | minor | fail-loud 绕过 trace 形成画像盲区 | 采纳：fail-loud 也记 trace（§2 次序） |
| E7 | 工程 | minor | 「提前拦截不改基线」需限定仅 PG | 采纳：并入 A6 |
| E8 | 工程 | minor | 纯函数恒分配；两遍扫描 | 采纳：返回 Cow + 单遍实现约束（§1） |
| E9 | 工程 | minor | ActiveTx/stream 细节确认 | 确认性意见，已在 §2 体现，无改动 |
| E10 | 工程 | minor | 测试矩阵补 9 条（串内 $n、'' 翻倍、非嵌套注释…） | 采纳：全部并入 §3① 表 |
| P1 | 产品 | blocker | 存量文档六处教旧规则，与新心智模型正面冲突 | 采纳：§4 反转清单 + 零残留契约校验 |
| P2 | 产品 | major | `$n` 存量用户升级恐惧，CHANGELOG 叙事缺失 | 采纳：三类人群叙事 + 「无需迁移指南」定性 |
| P3 | 产品 | major | fail-loud 文案缺正解提示 | 部分采纳：加 hint 文案并入单测断言；**拒绝附 SQL 原文**（错误信封防字面量敏感值外泄，全文由 trace 承载） |
| P4 | 产品 | major | e2e 未锚定插件后端/stream/文案断言 | 采纳：§3③ 四项补齐 |
| P5 | 产品 | minor | sample 可发现性弱 | 采纳：注释 + api-manual 三 DSN 示例 |
| P6 | 产品 | minor | trace 记原始 SQL 缺验收锚点；db-guide 应入反转清单 | 采纳：§3③ 断言 + db-guide 并入 P1 清单 |

拒绝/部分拒绝仅 P3b（附 SQL 片段）一处，理由如上；其余 22 条全部采纳或合并采纳。
