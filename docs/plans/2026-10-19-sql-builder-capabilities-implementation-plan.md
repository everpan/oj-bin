# db.table() 构造器 7 项能力 —— 分阶段实施计划（v0.1.81）

- 日期：2026-10-19
- 上游设计稿：`2026-10-19-sql-builder-capabilities-design.md`（四路评审已过、用户已确认）
- 方法：**BDD 先行**（每阶段先写行为规格 = 失败的用例，再写实现）；
  **DRY**（表引用归一化 + 递归遍历单一出口）；**SOLID**（`Op` 加 `class()` 做单一职责判定、
  遍历器对扩展开放对修改封闭）。
- 阶段顺序为**依赖序**，不可打乱：B0 → B1 → B2 → B3。

---

## 0. 通用约定

### 0.1 行为规格的写法

本仓构造器用例有两类，本计划两类都要：

1. **同步单测**（`#[test]`，直接喂 `serde_json::from_str::<QueryReq>` 后调
   `build_statement(&req, &reg, dialect)`）—— 用于**方言矩阵**与**错误文案**，
   不依赖真实数据库。样板见 `query.rs:2487` `verb_matrix_enforced_op_side`。
2. **异步端到端**（`#[tokio::test(flavor="current_thread")]`，`seeded_bridge()` 跑 JS 源码串）
   —— 用于**语义正确性**（真的查得出结果）。样板见 `query.rs:2305`。

方言矩阵统一走第 1 类，写成 helper：

```rust
fn sql_of(req: &str, d: Dialect) -> String   // 三方言各调一次
```

### 0.2 门禁（每阶段结束必跑）

```bash
cargo fmt --check
cargo clippy --release --workspace --all-targets -- -D warnings
cargo test --release --workspace
```

### 0.3 红线自检（每阶段）

- 标识符不经 `check_col` 的，一律不进 SQL；
- 值不经 `to_qv` 的，一律不进 SQL；
- `bootstrap.js` 保持 7-bit ASCII；
- 既有错误文案**不得改动**（大量既有测试锁死）。

---

## B0：安全前置（必须先于一切）

> 没有 B0，B2 的表别名会让归属守卫与租户守卫**同时静默失效**。

### B0-1 `TableRef` 类型化

**行为规格（先写，必失败）**

```
req {"table":"t as x"} → 表 "t" 受归属守卫保护：别模块表 → 仍报 ownership 错
req {"table":"tt as x"}（tt 受租户约束）→ 生成 SQL 仍含 "x"."tenant_id" = ?
req {"table":"t"} → 行为与现状逐字节一致（零迁移）
```

**实现**

1. `struct TableRef { name: String, alias: Option<String> }` + `fn sql_name()`；
2. 自定义 `Deserialize`：字符串 → 按 `" as "` 拆分（仅「空格 as 空格」单一形态，
   避免误伤含 "as" 的表名）；对象 `{name, as}` 直接填充；
3. `QueryReq.table` 与 `Join.table` 改为 `TableRef`；
4. 22 处消费点按 §3.0 表格的 real/alias 分工替换（`query.rs`）：

   | 用途 | 行 | 用 |
   |------|----|----|
   | `guard::check_table` | 788-789 / 792-793 | real |
   | `scoped()` | 855 / 858-859 / 862-863 | real |
   | `tenant_value(reg, ..)` | 874 / 889 / 924 | real |
   | `reg.get` → unknown table | 1133 / 1186 / 1218 | real |
   | `resolve()` | 1269 / 1284 | real |
   | 自 join 判定 | 1273-1275 | `sql_name()` |
   | `ColCtx.base_name` | 1287 / 1189 / 1217 | `sql_name()` |
   | 全列展开前缀 | 1303 | `sql_name()` |
   | `q.from` / `q.join` | 1405 / 1422 | `sql_name()` |
   | tenant 注入限定列 | 872 / 1416 | `sql_name()` |
   | insert/update/delete 表标识 | 1144 / 1195 / 1225 | real（别名在 B2 被拒） |

5. `build_select_stmt` 增补 `dialect: Dialect` 形参（B3 的 `lit` CAST 需要，
   趁此一次性穿透，6 处调用点：655 / 741 / 776 / 1125 / 1492 / 1524）。

### B0-2 `child_reqs()` 统一递归

**行为规格（先写，必失败）**

```
from_select 深度 5 层 → "nested select too deep"
from_select 源 verb=delete → "nested select must be select"
from_select 源表属别模块 → 仍报 ownership 错
from_select 源表受租户约束 → 内嵌 SELECT 的 SQL 含 <alias>.tenant_id = ?
from_select 源含 with/unions → "does not accept with/unions"
```

**实现**

1. `fn child_reqs(r: &mut QueryReq) -> impl Iterator<Item = &mut QueryReq>`；
2. `guard_req` / `apply_tenant` / `validate_nested` 三 walker 改用它；
3. `validate_verb` 的 insert 分支对 `from_select` 调 `validate_nested(.., 1, "fromSelect")`。

> 此阶段 `from_select` 字段还没接功能，先把**遍历骨架**与对抗用例钉死，
> 免得 B3 接上时三个 walker 各漏一遍。

---

## B1：低风险能力

### B1-1 语义化模糊搜索 op

**行为规格**

```
contains("a%b_c!d")  → 三方言 SQL 均为 LIKE ? ESCAPE '!' 且 params=["%a!%b!_c!!d%"]
startswith("kw")     → params=["kw%"]
endswith("kw")       → params=["%kw"]
escape:"\\\\"        → Err "escape must be a single char from: ! / | ~ # @"
escape:"!!"          → Err（长度）
escape:"'"           → Err（字符）
op:"like" 不带 escape → 与现状逐字节一致
```

**实现**

- `Op` 新增 `Contains` / `StartsWith` / `EndsWith`，逐个 `#[serde(rename = "...")]` 全小写；
- `Cond.escape: Option<String>`；
- `fn escape_like(kw, esc) -> Result<String>`（先转义转义符，再转义 `%` `_`）；
- `Op` 加 `fn class() -> OpClass`（`Compare` / `Pattern` / `Nullary` / `Subqueryable`），
  `apply_op` / `leaf_subquery` / 未来的 join-ON 三处按 class 判定（**SOLID：消除三处
  各自重复的 op 子集判定**）。

### B1-2 `Op::In` 空数组守卫

**行为规格**：`{field:"a",op:"in",value:[]}` → Err `in needs non-empty array`。

### B1-3 `whereIf` 糖（JS 侧）

**行为规格**：`whereIf(undefined|null|false|[])` 跳过；`whereIf(0|"")` **保留**
（`status=0` 是合法过滤值）。

### B1-4 `select(["*"])`

**行为规格**

```
select(["*"])            → 展开为 registry 全列
select(["*","id"])       → Err "select '*' must be the only column"
select(["*"]) + join     → Err "select '*' is not allowed with joins ..."
```

---

## B2：表别名 + join ON 值条件

### B2-1 表别名（仅 select / join）

**行为规格**

```
db.table("a").as("x").select(["x.id"])                  → SQL 含 "x"."id"，FROM a AS x
db.table("a").as("x").join("b",[..],"left").select(["x.id","b.label"])
db.table("a").as("a2").join("a",...).as("a3")           → 自 join 放行（两侧别名不同）
db.table("a").as("x").join("a",...).as("x")             → Err "duplicate table alias 'x'"
verb=update + 别名 → Err "table alias is not supported on update/delete"
verb=delete + 别名 → Err（同上）
别名 + 全列展开 → 渲染 "x".col（不是 "a".col）
别名形状非法 → Err "illegal alias ..."（复用 check_alias）
```

**实现**

- `bootstrap.js`：`as(alias)`；
- `q.from_as` / `q.join_as`；
- tenant 注入的限定列改用 `sql_name()`（`query.rs:872` / `1416`）——
  **这是本阶段最易漏且后果最严重的一点**。

### B2-2 join ON 值条件

**行为规格**

```
join("b", [{left:"b.aid",right:"a.id"}, {left:"b.uid",op:"eq",value:7}], "left")
  → SQL: LEFT JOIN b ON "b"."aid" = "a"."id" AND "b"."uid" = ? ; params 含 7
join("b", [{left:"b.x",op:"contains",value:"v"}])   → Err "does not support op 'contains'"
join("b", [{left:"b.x",op:"isnull"}])               → Err
join("b", [])                                        → Err（现状保持）
value 传 toBigInt() → params 保留 $oj$i64 标记（encodeParams 覆盖）
```

**实现**

- `OnPair` 改枚举 `Cols{left,right}` / `Val{left,op,value}` + 手写 `Deserialize`；
- op 按 `Op::class()` 判定，非 `Compare` 直接 Err；
- `bootstrap.js:1060` 的 `on` 映射把 `value` 过 `encodeParams`。

---

## B3：字面量列 + 幂等写入

### B3-1 `lit` 字面量列

**行为规格（三方言 × CAST 差异，本项需 spike 确认）**

```
Sqlite  : SELECT ..., ? AS "scope"
MySql   : SELECT ..., CAST(? AS CHAR) AS "scope"
Postgres: SELECT ..., CAST(? AS TEXT) AS "scope"
{lit: BigInt} → params 保留 $oj$i64 标记
```

**实现**：`ColSpec::Lit(LitSpec)`；`bootstrap.js` 的 `select` 对象形态统一过 `encodeParams`。

### B3-2 `orIgnore`

**行为规格**

```
Sqlite   → INSERT ... ON CONFLICT DO NOTHING
Postgres → INSERT ... ON CONFLICT DO NOTHING
MySql    → INSERT ... ON DUPLICATE KEY UPDATE "id" = "id"
MySql + 无主键表 → Err "orIgnore on mysql requires a primary key on table 'x'"
MySql + returning → Err "orIgnore with returning is not supported on mysql"
```

**实现**：方言分派；`TableSrc::pk()` 暴露 `TableDef.primary_key`。

### B3-3 `fromSelect(q, cols)`

**行为规格**

```
fromSelect(src, ["a","b"]) 且 src 输出 3 列 → Err "declares 2 columns but the source select yields 3"
fromSelect 源 verb=delete  → Err（B0 已钉）
insert 同时给 values 与 fromSelect → Err "takes either values or fromSelect, not both"
insert 两者都缺 → Err "needs either values or fromSelect"
insert + fromSelect + 受租户约束表 → 目标列缺 tenant_id 时引擎补齐，
                                    且拒绝由内层 select 提供 tenant_id
```

**实现**

- `QueryReq` 增 `or_ignore` / `from_select` / `insert_cols`；
- `validate_verb` insert 分支改写（values 与 from_select 互斥且必居其一）；
- `build_statement` insert 分支分两条路：`values` 走现路径，`from_select` 走
  `ins.select_from(...)` + `ins.columns(insert_cols)`；
- `apply_tenant` 的 Insert 分支对 `from_select` 路径做 tenant 补齐（**禁止由 select 提供**）。

---

## 收尾

1. **§8.2 对抗用例 19 条**全量补齐；
2. **门禁**：`fmt --check` / `clippy --release --workspace --all-targets -D warnings` /
   `test --release --workspace`；
3. **文档**：
   - 专题文档 `docs/devkit/sql-builder-guide.md`（新；SQL 构造器专题指引，随 devkit 四件一并归置 `bin/devkit/`）；
   - devkit 四件：`api-manual.md` / `SKILL.md` / `scenarios.md` / `README.md`；
   - `CHANGELOG.md` v0.1.81；
   - `cargo xtask build` 归置 `bin/devkit/` + `cargo test --release -p xtask` 契约校验。
4. **版本**：`oj/Cargo.toml` 0.1.80 → 0.1.81，`Cargo.lock` 同步。

---

## 不做（设计稿已裁定）

- `expr` 任意表达式模板（§13 砍除声明）；
- 全表 delete / update 豁免；
- `leftJoin` 糖（用 `.join(..., "left")`）；
- `where(rawFragment, params)` 逃逸舱；
- `db.query` / `db.exec` 下架。
