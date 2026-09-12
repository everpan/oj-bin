# db.table 查询构造器对齐 xorm builder（v0.1.14）实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** db.table 构造器补齐 DML / 嵌套条件树 / 条件对象 / join / 聚合分组 / toSQL / toJSON-fromJSON（对齐 xorm builder）。

**Architecture:** 单 op 扩展（方案 A）：`QueryReq` 增字段，`op_db_query_build` 拆「op 前置守卫
`guard_req` + 纯函数 `build_statement`」两段，`op_db_query_sql`（toSQL）共用两段不执行。
JS 侧 bootstrap.js 扩链式层；条件对象纯 JS 实现零新 op。Spec：
`docs/superpowers/specs/2026-09-12-db-builder-xorm-align-design.md`（评审修订版，必读）。

**Tech Stack:** Rust（deno_core op2 / sea-query 1.0.2 / serde_json）、bootstrap.js（7-bit ASCII）。

## Global Constraints

- 构建/测试一律 `cargo build --release` / `cargo test --release` / `cargo clippy --release --all-targets -- -D warnings`（禁止 debug 构建/裸 test）。
- `src/bridge/bootstrap.js` 必须保持 **7-bit ASCII**（新增注释全英文）；验收：`LC_ALL=C grep -P '[^\x00-\x7F]' src/bridge/bootstrap.js` 无输出。
- 异步测试一律 `#[tokio::test(flavor = "current_thread")]`。
- 红线：标识符只来自 SchemaRegistry 白名单（表/列/join/on/DML 键/聚合 field），值只经绑定参数；update/delete 必须带 where 且叶子数 ≥ 1。
- 版本：本特性 = `oj/Cargo.toml` 0.1.13 → **0.1.14**（最后阶段统一 bump）。
- 提交信息结尾加 trailer：`unix@vip.qq.com ai`。
- 每阶段（Phase）末尾做「更新与总结」：勾掉本阶段 checkbox、`git log --oneline` 核对、输出简短总结。

---

## Phase 1：两段拆分重构 + toSQL（地基）

### Task 1：抽取 guard_req + build_statement（select 等价重构）

**Files:**
- Modify: `src/bridge/query.rs`（op_db_query_build 拆解）
- Test: `src/bridge/query.rs` 既有测试模块（重构 = 既有测试全绿）

**Interfaces:**
- Produces（后续所有 Task 依赖）:
  - `fn guard_req(state: &Rc<RefCell<OpState>>, req: &QueryReq) -> Result<(), JsErrorBox>`
  - `fn build_statement(req: &QueryReq, reg: &SchemaRegistry, dialect: Dialect) -> Result<(String, Vec<Value>), JsErrorBox>`
  - `fn build_sql<S: sea_query::QueryStatementBuilder>(d: Dialect, q: &S) -> (String, sea_query::Values)`（build_select 的泛化改名）

- [x] **Step 1: 确认重构前基线**

Run: `cargo test --release query:: -- --nocapture 2>&1 | tail -5`
Expected: 既有 5 个 query 测试全 PASS。

- [x] **Step 2: 最小重构**

`src/bridge/query.rs` 中：

```rust
/// op 前置守卫（两个 op 共用）：主表 check_table（Phase 5 扩展 join 表）。
fn guard_req(state: &Rc<RefCell<OpState>>, req: &QueryReq) -> Result<(), JsErrorBox> {
    super::guard::check_table(state, &req.table)
}

/// 纯构造：白名单校验 → sea-query → 方言 SQL + JSON 参数（不触 OpState/连接/tx）。
fn build_statement(
    req: &QueryReq,
    reg: &SchemaRegistry,
    dialect: Dialect,
) -> Result<(String, Vec<Value>), JsErrorBox> {
    let table = reg
        .get(&req.table)
        .ok_or_else(|| JsErrorBox::generic(format!("unknown table '{}'", req.table)))?;
    // —— 以下从 op_db_query_build 原样搬入：列白名单、条件、order_by、limit/offset ——
    let cols: Vec<Alias> = if req.columns.is_empty() {
        table.columns.keys().map(|c| Alias::new(c.clone())).collect()
    } else {
        req.columns
            .iter()
            .map(|c| {
                if !table.has_column(c) {
                    Err(JsErrorBox::generic(format!(
                        "unknown column '{c}' on '{}'",
                        req.table
                    )))
                } else {
                    Ok(Alias::new(c.clone()))
                }
            })
            .collect::<Result<_, _>>()?
    };
    let mut q = Query::select();
    q.columns(cols).from(Alias::new(&req.table));
    for c in &req.conditions {
        if !table.has_column(&c.field) {
            return Err(JsErrorBox::generic(format!(
                "unknown column '{}' in where",
                c.field
            )));
        }
        q.and_where(build_expr(&c.field, c.op, &c.value)?);
    }
    for o in &req.order_by {
        if !table.is_sortable(&o.field) {
            return Err(JsErrorBox::generic(format!("column '{}' not sortable", o.field)));
        }
        let dir = match o.dir.as_deref() {
            Some("desc") => Order::Desc,
            _ => Order::Asc,
        };
        q.order_by(Alias::new(&o.field), dir);
    }
    let limit = Ord::min(req.limit.unwrap_or(LIMIT_DEFAULT), LIMIT_MAX);
    q.limit(limit as u64);
    if let Some(off) = req.offset {
        q.offset(off as u64);
    }
    let (sql, values) = build_sql(dialect, &q);
    let params: Vec<Value> = values.iter().map(value_to_json).collect::<Result<_, _>>()?;
    Ok((sql, params))
}

/// 按方言出 SQL（QueryStatementBuilder::build 泛型，四类 statement 通吃）。
fn build_sql<S: sea_query::QueryStatementBuilder>(d: Dialect, q: &S) -> (String, sea_query::Values) {
    match d {
        Dialect::Sqlite => q.build(SqliteQueryBuilder),
        Dialect::MySql => q.build(sea_query::MysqlQueryBuilder),
        Dialect::Postgres => q.build(sea_query::PostgresQueryBuilder),
    }
}
```

`op_db_query_build` 改为：

```rust
#[op2]
#[serde]
pub async fn op_db_query_build(
    state: Rc<RefCell<OpState>>,
    #[serde] req: QueryReq,
) -> Result<Vec<Value>, JsErrorBox> {
    let reg = registry(&state)?;
    guard_req(&state, &req)?;
    let (sql, params) = build_statement(&req, &reg, lookup(&state, &req.db)?.dialect())?;
    match super::db::resolve_target(&state, &req.db)? {
        super::db::Target::Pool(da) => da
            .query_with_params(&sql, &params)
            .await
            .map_err(|e| JsErrorBox::generic(e.to_string())),
        super::db::Target::Tx(t) => t
            .session
            .lock()
            .await
            .query(&sql, &params)
            .await
            .map_err(|e| JsErrorBox::generic(e.to_string())),
    }
}
```

注意：`value_to_json` 现签名 `&Qv`，`values.iter().map(value_to_json)`；旧 `build_select` 删除，
其测试 `placeholder_per_dialect` 改用 `build_sql`（同名保留、内部改调 `build_sql` 即可，
测试不动）。

- [x] **Step 3: 回归全绿 + 门禁**

Run: `cargo test --release query:: 2>&1 | tail -3 && cargo clippy --release --all-targets -- -D warnings 2>&1 | tail -2`
Expected: 全 PASS，clippy 零警告。

- [x] **Step 4: Commit**

```bash
git add src/bridge/query.rs
git commit -m "refactor(query): op_db_query_build 拆 guard_req + build_statement 两段（select 等价）

unix@vip.qq.com ai"
```

### Task 2：op_db_query_sql + JS `.toSQL()`

**Files:**
- Modify: `src/bridge/query.rs`（新 op）、`src/bridge/mod.rs:228`（注册）、`src/bridge/bootstrap.js`（import + 链方法）
- Test: `src/bridge/query.rs` 测试模块

**Interfaces:**
- Consumes: Task 1 的 `guard_req` / `build_statement`。
- Produces: JS `queryBuilder.toSQL() -> { sql: string, params: any[] }`；Rust `op_db_query_sql`（**同步**，不 tx 路由）。

- [x] **Step 1: 写失败测试**

`src/bridge/query.rs` 测试模块追加：

```rust
#[tokio::test(flavor = "current_thread")]
async fn to_sql_returns_dialect_sql_and_params_without_executing() {
    let b = seeded_bridge().await;
    let cap = b
        .run(
            r#"const s = db.table("t").select(["name"]).where({field:"age",op:"gte",value:18}).toSQL();
               db.query(s.sql, s.params).then(rows => json.ok({ sql: s.sql, n: rows.length }))
                 .catch(e => json.fail(500, String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert!(v["data"]["sql"].as_str().unwrap().contains('?'), "{v}"); // sqlite 占位符
    assert_eq!(v["data"]["n"], 2); // age>=18 → b,c,d = 3? 种子 10/20/30/40 → 3
}
```

（断言以种子数据为准：age>=18 命中 3 行，`n = 3`。）

- [x] **Step 2: 跑测试确认失败**

Run: `cargo test --release to_sql_returns 2>&1 | tail -3`
Expected: FAIL（`toSQL is not a function` / op 未注册）。

- [x] **Step 3: 最小实现**

`src/bridge/query.rs`：

```rust
/// toSQL：与执行完全相同的两段（guard_req + build_statement），只构造不执行；
/// 不做 tx 路由、不受活跃 tx 影响。同步 op（全程 OpState 同步借用，无 await）。
#[op2]
#[serde]
pub fn op_db_query_sql(
    state: Rc<RefCell<OpState>>,
    #[serde] req: QueryReq,
) -> Result<Value, JsErrorBox> {
    let reg = registry(&state)?;
    guard_req(&state, &req)?;
    let (sql, params) = build_statement(&req, &reg, lookup(&state, &req.db)?.dialect())?;
    Ok(serde_json::json!({ "sql": sql, "params": params }))
}
```

`src/bridge/mod.rs`（extension 的 op 列表，`query::op_db_query_build,` 之后）：

```rust
        query::op_db_query_build,
        query::op_db_query_sql,
```

`src/bridge/bootstrap.js`（import 列表加 `op_db_query_sql,`；`queryBuilder` 的 api 增）：

```js
    toSQL() { return op_db_query_sql(req); },
```

- [x] **Step 4: 跑测试确认通过**

Run: `cargo test --release query:: 2>&1 | tail -3`
Expected: 全 PASS（含新测试，n=3）。

- [x] **Step 5: Commit**

```bash
git add src/bridge/query.rs src/bridge/mod.rs src/bridge/bootstrap.js
git commit -m "feat(query): toSQL —— op_db_query_sql 只构造不执行（含方言占位符 + 参数）

unix@vip.qq.com ai"
```

### Phase 1 收尾：更新与总结

- [x] 勾掉 Phase 1 全部 checkbox；`git log --oneline -2` 核对两个提交；总结「两段拆分 + toSQL 就绪，后续阶段只扩 build_statement 与 JS 链层」。

---

## Phase 2：嵌套条件树（Rust 侧）

### Task 3：CondTree + 手写 Deserialize

**Files:**
- Modify: `src/bridge/query.rs`（Op/Cond 下方新增）
- Test: `src/bridge/query.rs` 测试模块

**Interfaces:**
- Produces:
  - `enum CondTree { Leaf(Cond), And(Vec<CondTree>), Or(Vec<CondTree>), Not(Box<CondTree>) }`（手写 `Deserialize`）
  - 约定：组键 `and`/`or`/`not` 恰一；叶子必须含 `field`；多余键/空组/形状错误均精确报错。
- 注意：本 Task 不改 `QueryReq.conditions` 类型（Task 4 接线），只交付可单测的类型。

- [x] **Step 1: 写失败测试**

```rust
#[test]
fn cond_tree_deserialize_dispatch_and_errors() {
    // 叶子向后兼容
    let t: CondTree = serde_json::from_str(r#"{"field":"a","op":"eq","value":1}"#).unwrap();
    assert!(matches!(t, CondTree::Leaf(_)));
    // and / or / not
    let t: CondTree = serde_json::from_str(
        r#"{"or":[{"field":"a","op":"eq","value":1},{"not":{"field":"b","op":"isnull"}}]}"#,
    )
    .unwrap();
    assert!(matches!(t, CondTree::Or(ref xs) if xs.len() == 2));
    // 空组 → Err
    let e = serde_json::from_str::<CondTree>(r#"{"and":[]}"#).unwrap_err();
    assert!(e.to_string().contains("empty condition group"), "{e}");
    // 多余键（leaf 形状 + or）→ Err，不静默吞
    let e = serde_json::from_str::<CondTree>(r#"{"field":"a","op":"eq","value":1,"or":[]}"#)
        .unwrap_err();
    assert!(e.to_string().contains("unknown keys"), "{e}");
    // 组键多于一个 → Err
    assert!(serde_json::from_str::<CondTree>(r#"{"and":[],"or":[]}"#).is_err());
    // 空对象 → Err
    assert!(serde_json::from_str::<CondTree>(r#"{}"#).is_err());
}
```

- [x] **Step 2: 跑测试确认失败**

Run: `cargo test --release cond_tree_deserialize 2>&1 | tail -3`
Expected: FAIL（CondTree 未定义，编译错误）。

- [x] **Step 3: 最小实现**

```rust
/// 嵌套条件树（对齐 xorm And/Or/Not）。不用 untagged（serde 在 untagged 内
/// deny_unknown_fields 不生效，{field,op,value,or:[...]} 会被静默降级为 Leaf）——
/// 手写 Deserialize 按键唯一分发，多余键显式报错。
#[derive(Debug, Clone)]
enum CondTree {
    Leaf(Cond),
    And(Vec<CondTree>),
    Or(Vec<CondTree>),
    Not(Box<CondTree>),
}

impl<'de> Deserialize<'de> for CondTree {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        let m = serde_json::Map::<String, Value>::deserialize(d)?;
        let groups: Vec<&str> = ["and", "or", "not"]
            .into_iter()
            .filter(|k| m.contains_key(k))
            .collect();
        if !groups.is_empty() {
            let extra: Vec<&String> = m
                .keys()
                .filter(|k| !groups.contains(&k.as_str()))
                .collect();
            if groups.len() != 1 || !extra.is_empty() {
                return Err(Error::custom(format!(
                    "condition group takes exactly one of and/or/not, unknown keys {extra:?}"
                )));
            }
            let parse_vec = |v: &Value| -> Result<Vec<CondTree>, D::Error> {
                let xs = Vec::<CondTree>::deserialize(v.clone()).map_err(Error::custom)?;
                if xs.is_empty() {
                    return Err(Error::custom("empty condition group"));
                }
                Ok(xs)
            };
            return match groups[0] {
                "and" => Ok(CondTree::And(parse_vec(&m["and"])?)),
                "or" => Ok(CondTree::Or(parse_vec(&m["or"])?)),
                _ => Ok(CondTree::Not(Box::new(
                    CondTree::deserialize(m["not"].clone()).map_err(Error::custom)?,
                ))),
            };
        }
        let leaf = Cond::deserialize(Value::Object(m)).map_err(Error::custom)?;
        Ok(CondTree::Leaf(leaf))
    }
}
```

（`Cond` 上补 `#[serde(deny_unknown_fields)]`——leaf 路径走 Cond 的 derive，多余键即报错。）

- [x] **Step 4: 跑测试确认通过**

Run: `cargo test --release cond_tree_deserialize 2>&1 | tail -3`
Expected: PASS。

- [x] **Step 5: Commit**

```bash
git add src/bridge/query.rs
git commit -m "feat(query): CondTree 手写 Deserialize（按键唯一分发，空组/多键精确报错）

unix@vip.qq.com ai"
```

### Task 4：递归编译 + 深度/叶子上限 + where 接线

**Files:**
- Modify: `src/bridge/query.rs`（`QueryReq.conditions` 类型换 `Vec<CondTree>` + `build_statement` 编译）
- Test: `src/bridge/query.rs` 测试模块

**Interfaces:**
- Consumes: Task 3 的 `CondTree`。
- Produces:
  - `const COND_DEPTH_MAX: usize = 8; const COND_LEAF_MAX: usize = 64;`
  - `fn cond_expr(t: &CondTree, table: &TableDef, depth: usize, leaves: &mut usize) -> Result<SimpleExpr, JsErrorBox>`
  - 语义：`where()` 多次调用 = 顶层 AND；空组在 Deserialize 已拒 ⇒ 任何 CondTree 叶子 ≥ 1（DML 守卫 Phase 4 复用此性质）。

- [x] **Step 1: 写失败测试**

```rust
#[tokio::test(flavor = "current_thread")]
async fn nested_condition_tree_filters_and_limits() {
    let b = seeded_bridge().await;
    // or + not 嵌套：age>=20 or name like 'a%'，再 not tag='y' → c,a → 2
    let n = count_where(
        &b,
        r#"{or:[{field:"age",op:"gte",value:20},{field:"name",op:"like",value:"a%"}]}"#,
    )
    .await;
    assert_eq!(n, 3);
    let n = count_where(&b, r#"{not:{field:"tag",op:"eq",value:"x"}}"#).await;
    assert_eq!(n, 2); // b,d
    // 深度 9 → too deep
    let cap = b.run(r#"let c={field:"age",op:"eq",value:1}; for(let i=0;i<9;i++) c={and:[c]};
        db.table("t").select(["name"]).where(c).all()
          .then(r=>json.ok({})).catch(e=>json.fail(400,String(e)));"#).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert!(v["msg"].as_str().unwrap().contains("too deep"), "{v}");
    // 65 叶 → too large
    let cap = b.run(r#"const xs=[]; for(let i=0;i<65;i++) xs.push({field:"age",op:"gt",value:i});
        db.table("t").select(["name"]).where({and:xs}).all()
          .then(r=>json.ok({})).catch(e=>json.fail(400,String(e)));"#).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert!(v["msg"].as_str().unwrap().contains("too large"), "{v}");
    // 空组（JS 直塞）→ empty condition group
    let cap = b.run(r#"db.table("t").select(["name"]).where({and:[]}).all()
        .then(r=>json.ok({})).catch(e=>json.fail(400,String(e)));"#).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert!(v["msg"].as_str().unwrap().contains("empty condition group"), "{v}");
}
```

- [x] **Step 2: 跑测试确认失败**

Run: `cargo test --release nested_condition_tree 2>&1 | tail -3`
Expected: FAIL（`{or:[...]}` 无法反序列化为旧 `Cond`）。

- [x] **Step 3: 最小实现**

```rust
const COND_DEPTH_MAX: usize = 8;
const COND_LEAF_MAX: usize = 64;

/// 条件树 → SimpleExpr（Phase 5 把 table 参数换成 ColCtx 以支持限定列）。
fn cond_expr(
    t: &CondTree,
    table: &TableDef,
    depth: usize,
    leaves: &mut usize,
) -> Result<SimpleExpr, JsErrorBox> {
    if depth > COND_DEPTH_MAX {
        return Err(JsErrorBox::generic("condition tree too deep (max 8)"));
    }
    match t {
        CondTree::Leaf(c) => {
            *leaves += 1;
            if *leaves > COND_LEAF_MAX {
                return Err(JsErrorBox::generic("condition tree too large (max 64 leaves)"));
            }
            if !table.has_column(&c.field) {
                return Err(JsErrorBox::generic(format!(
                    "unknown column '{}' in where",
                    c.field
                )));
            }
            build_expr(&c.field, c.op, &c.value)
        }
        CondTree::And(xs) | CondTree::Or(xs) => {
            let mut cond = if matches!(t, CondTree::And(_)) {
                sea_query::Condition::all()
            } else {
                sea_query::Condition::any()
            };
            for x in xs {
                cond = cond.add(cond_expr(x, table, depth + 1, leaves)?);
            }
            Ok(SimpleExpr::from(cond))
        }
        CondTree::Not(x) => {
            let mut cond = sea_query::Condition::all();
            cond = cond.add(cond_expr(x, table, depth + 1, leaves)?);
            Ok(SimpleExpr::from(cond.not()))
        }
    }
}
```

`QueryReq.conditions` 类型改为 `Vec<CondTree>`；`build_statement` 的 where 段改为：

```rust
    let mut leaves = 0usize;
    for c in &req.conditions {
        q.and_where(cond_expr(c, table, 1, &mut leaves)?);
    }
```

`registry.rs` 的 `TableDef` 已在 crate 可见（`use super::registry::{SchemaRegistry, TableDef};` 补 import）。

- [x] **Step 4: 跑测试确认通过 + 旧回归**

Run: `cargo test --release query:: 2>&1 | tail -3`
Expected: 全 PASS（含既有 `comparison_ops_filter_rows` 等——旧 `{field,op,value}` 线格式兼容）。

- [x] **Step 5: Commit**

```bash
git add src/bridge/query.rs
git commit -m "feat(query): 嵌套条件树（and/or/not 递归编译，深度 8 / 叶子 64 上限）

unix@vip.qq.com ai"
```

### Phase 2 收尾：更新与总结

- [x] 勾掉 Phase 2 checkbox；`git log --oneline -2` 核对；总结「条件树 op 侧就绪，JS 条件对象只是这棵树的生产者」。

---

## Phase 3：JS 条件对象（纯 JS，零新 op）

### Task 5：condObj 工厂/组合/检查 + where/having 解包

**Files:**
- Modify: `src/bridge/bootstrap.js`（DB 工厂区 + queryBuilder）
- Test: `src/bridge/query.rs` 测试模块（经 Bridge 跑 JS 断言）

**Interfaces:**
- Produces（JS 全局，db / DB(name) / tx facade 三处同挂载）:
  - `db.leaf(field, op, value)` / `db.and(...conds)` / `db.or(...conds)` / `db.not(cond)` → 条件对象
  - 条件对象方法：`tree()` / `and(...)` / `or(...)` / `not()`（不可变，返回新对象）/ `fields()` / `has(field)`
  - `where(cond)` 接受条件对象或裸 JSON 树（`.tree()` 解包进 req）

- [x] **Step 1: 写失败测试**

`src/bridge/query.rs` 测试模块追加：

```rust
#[tokio::test(flavor = "current_thread")]
async fn cond_object_compose_inspect_and_equivalent_exec() {
    let b = seeded_bridge().await;
    let cap = b
        .run(
            r#"const base = db.and({field:"age",op:"gte",value:18}, {field:"ok",op:"eq",value:1});
               const c = base.or({field:"tag",op:"isnull"});
               const bare = {or:[{and:[{field:"age",op:"gte",value:18},{field:"ok",op:"eq",value:1}]},{field:"tag",op:"isnull"}]};
               Promise.all([
                 db.table("t").select(["name"]).where(c).all(),
                 db.table("t").select(["name"]).where(bare).all(),
               ]).then(([a, b2]) => json.ok({
                 eq: a.length === b2.length && a.length === 3,
                 fields: c.fields(), has: c.has("age") && !c.has("zz"),
                 immutable: base.fields().length === 2,
               })).catch(e => json.fail(500, String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert_eq!(v["data"]["eq"], true, "{v}");
    assert_eq!(v["data"]["fields"], json!(["age", "ok", "tag"]), "{v}");
    assert_eq!(v["data"]["has"], true, "{v}");
    assert_eq!(v["data"]["immutable"], true, "{v}");
}
```

- [x] **Step 2: 跑测试确认失败**

Run: `cargo test --release cond_object 2>&1 | tail -3`
Expected: FAIL（`db.and is not a function`）。

- [x] **Step 3: 最小实现（bootstrap.js，注释全英文）**

在 `const dbCache = new Map();` 之前插入：

```js
// ----- condition tree factory (pure JSON tree; compose/inspect in JS, zero new ops) -----
function unwrapCond(c) { return c && typeof c.tree === "function" ? c.tree() : c; }
function condObj(tree) {
  const api = {
    tree: () => tree,
    and: (...cs) => condObj({ and: [tree, ...cs.map(unwrapCond)] }),
    or: (...cs) => condObj({ or: [tree, ...cs.map(unwrapCond)] }),
    not: () => condObj({ not: tree }),
    fields: () => {
      const out = [];
      (function walk(t) {
        if (t && typeof t === "object") {
          if (t.field !== undefined) out.push(String(t.field));
          for (const k of ["and", "or"]) if (Array.isArray(t[k])) t[k].forEach(walk);
          if (t.not) walk(t.not);
        }
      })(tree);
      return [...new Set(out)];
    },
    has(f) { return api.fields().includes(String(f)); },
  };
  return api;
}
function condFactories() {
  return {
    leaf: (field, op, value) => condObj({ field: String(field), op: String(op), value }),
    and: (...cs) => condObj({ and: cs.map(unwrapCond) }),
    or: (...cs) => condObj({ or: cs.map(unwrapCond) }),
    not: (c) => condObj({ not: unwrapCond(c) }),
  };
}
```

`DB(name)` 缓存对象与 tx 回调对象各加 `...condFactories(),`（对象字面量展开，DRY）。
`queryBuilder` 的 `where` 改为：

```js
    where(cond) { req.conditions.push(unwrapCond(cond)); return api; },
```

- [x] **Step 4: 跑测试确认通过 + ASCII 门禁**

Run: `cargo test --release cond_object 2>&1 | tail -3 && LC_ALL=C grep -P '[^\x00-\x7F]' src/bridge/bootstrap.js; echo ASCII-OK`
Expected: PASS；grep 无输出。

- [x] **Step 5: Commit**

```bash
git add src/bridge/bootstrap.js src/bridge/query.rs
git commit -m "feat(query): JS 条件对象（工厂/不可变组合/tree/fields/has，零新 op）

unix@vip.qq.com ai"
```

### Phase 3 收尾：更新与总结

- [x] 勾掉 Phase 3 checkbox；`git log --oneline -1` 核对；总结「条件对象就位，多租户守卫 `has("tenant_id")` 可用」。

---

## Phase 4：DML 写操作

### Task 6：Verb + 动词×字段矩阵校验（op 侧权威）

**Files:**
- Modify: `src/bridge/query.rs`（QueryReq 增 verb/values/sets + `validate_verb`）
- Test: `src/bridge/query.rs` 测试模块

**Interfaces:**
- Produces:
  - `enum Verb { Select(默认), Insert, Update, Delete }`（`impl Default` + `#[serde(default)]`）
  - `QueryReq.values: Vec<serde_json::Map<String, Value>>`、`QueryReq.sets: serde_json::Map<String, Value>`（均 `#[serde(default)]`）
  - `fn validate_verb(req: &QueryReq) -> Result<(), JsErrorBox>`（矩阵见下；Phase 5/6 各自扩展 joins/group 规则）
- 矩阵（op 权威，fromJSON 可绕 JS 链层）：
  - select：拒 `values`/`sets`
  - insert：`values` 非空（空 → `insert needs at least one row`）；拒 where/orderBy/limit/offset
  - update：`sets` 非空（空 → `update needs non-empty sets`）；必须 where 非空；拒 limit/offset
  - delete：必须 where 非空（`update/delete requires where`）；拒 limit/offset
  - （空 and/or 组已在 Deserialize 拒 ⇒ conditions 非空即叶子 ≥ 1）

- [x] **Step 1: 写失败测试**

```rust
#[test]
fn verb_matrix_enforced_op_side() {
    let bad = |req: &str| validate_verb(&serde_json::from_str(req).unwrap()).unwrap_err();
    // select 拒 values/sets
    assert!(bad(r#"{"table":"t","values":[{"a":1}]}"#).to_string().contains("select does not accept values"));
    // insert：空 values / 带 where / 带 limit
    assert!(bad(r#"{"table":"t","verb":"insert"}"#).to_string().contains("at least one row"));
    assert!(bad(r#"{"table":"t","verb":"insert","values":[{"a":1}],"conditions":[{"field":"a","op":"eq","value":1}]}"#).to_string().contains("insert does not accept where"));
    assert!(bad(r#"{"table":"t","verb":"insert","values":[{"a":1}],"limit":5}"#).to_string().contains("insert does not accept limit"));
    // update：空 sets / 无 where / limit
    assert!(bad(r#"{"table":"t","verb":"update","conditions":[{"field":"a","op":"eq","value":1}]}"#).to_string().contains("non-empty sets"));
    assert!(bad(r#"{"table":"t","verb":"update","sets":{"a":1}}"#).to_string().contains("requires where"));
    assert!(bad(r#"{"table":"t","verb":"update","sets":{"a":1},"conditions":[{"field":"a","op":"eq","value":1}],"limit":5}"#).to_string().contains("limit/offset"));
    // delete：无 where
    assert!(bad(r#"{"table":"t","verb":"delete"}"#).to_string().contains("requires where"));
    // 合法形态 Ok
    assert!(validate_verb(&serde_json::from_str(r#"{"table":"t","verb":"delete","conditions":[{"field":"a","op":"eq","value":1}]}"#).unwrap()).is_ok());
}
```

- [x] **Step 2: 跑测试确认失败**

Run: `cargo test --release verb_matrix 2>&1 | tail -3`
Expected: FAIL（编译错误：validate_verb/verb 未定义）。

- [x] **Step 3: 最小实现**

```rust
/// 查询动词（serde default = select，旧线格式零迁移）。
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Verb {
    #[default]
    Select,
    Insert,
    Update,
    Delete,
}
```

`QueryReq` 增：

```rust
    #[serde(default)]
    verb: Verb,
    #[serde(default)]
    values: Vec<serde_json::Map<String, Value>>,
    #[serde(default)]
    sets: serde_json::Map<String, Value>,
```

`validate_verb`（在 `build_statement` 开头调用，先于一切校验）：

```rust
/// 动词×字段兼容矩阵（op 侧权威——fromJSON 可完全绕过 JS 链层）。
fn validate_verb(req: &QueryReq) -> Result<(), JsErrorBox> {
    let reject = |verb: &str, f: &str| {
        Err(JsErrorBox::generic(format!("{verb} does not accept {f}")))
    };
    match req.verb {
        Verb::Select => {
            if !req.values.is_empty() {
                return reject("select", "values");
            }
            if !req.sets.is_empty() {
                return reject("select", "sets");
            }
        }
        Verb::Insert => {
            if req.values.is_empty() {
                return Err(JsErrorBox::generic("insert needs at least one row"));
            }
            if !req.conditions.is_empty() {
                return reject("insert", "where");
            }
            if !req.order_by.is_empty() {
                return reject("insert", "orderBy");
            }
            if req.limit.is_some() || req.offset.is_some() {
                return reject("insert", "limit/offset");
            }
        }
        Verb::Update | Verb::Delete => {
            if req.verb == Verb::Update && req.sets.is_empty() {
                return Err(JsErrorBox::generic("update needs non-empty sets"));
            }
            // 空 and/or 组在 CondTree Deserialize 已拒 → 非空即叶子 ≥ 1。
            if req.conditions.is_empty() {
                return Err(JsErrorBox::generic(format!(
                    "{:?} requires where (leaf count >= 1)",
                    req.verb
                )));
            }
            if req.limit.is_some() || req.offset.is_some() {
                return reject("update/delete", "limit/offset");
            }
        }
    }
    Ok(())
}
```

（`build_statement` 首行插入 `validate_verb(req)?;`。）

- [x] **Step 4: 跑测试确认通过 + 旧回归**

Run: `cargo test --release query:: 2>&1 | tail -3`
Expected: 全 PASS（verb 缺省 select，旧测试零迁移）。

- [x] **Step 5: Commit**

```bash
git add src/bridge/query.rs
git commit -m "feat(query): Verb 动词 + 动词×字段兼容矩阵（op 侧权威校验）

unix@vip.qq.com ai"
```

### Task 7：insert/update/delete 构造 + tx 路由 + 返回形态 + JS 链方法

**Files:**
- Modify: `src/bridge/query.rs`（build_statement 动词分发 + op 返回 `Result<Value>`）、`src/bridge/bootstrap.js`（insert/update/delete/run 链方法）
- Test: `src/bridge/query.rs` 测试模块

**Interfaces:**
- Consumes: Task 6 的 `Verb`/`validate_verb`。
- Produces:
  - op 返回形态变更：`op_db_query_build -> Result<Value, JsErrorBox>`（select → rows 数组；DML → 受影响行数 number）
  - JS：`.insert(objOrRows)` / `.update(sets)` / `.delete()` 返回 api；`.run()` 终执行（DML）；`.all()` 不变
  - DML 同走 `resolve_target`（本库 tx → `session.exec`，他库 tx → Err）

- [x] **Step 1: 写失败测试**

```rust
#[tokio::test(flavor = "current_thread")]
async fn dml_insert_update_delete_and_tx_routing() {
    let b = seeded_bridge().await;
    let cap = b
        .run(
            r#"(async () => {
               const ins = await db.table("t").insert([{name:"e",age:50},{name:"f",age:60}]).run();
               const upd = await db.table("t").update({age:55}).where({field:"name",op:"eq",value:"e"}).run();
               const del = await db.table("t").delete().where({field:"name",op:"eq",value:"f"}).run();
               const n = await db.table("t").select(["name"]).all().then(r => r.length);
               // tx 路由：tx 内 insert 回滚后不可见
               let txErr = false;
               try { await db.tx(async (tx) => { await tx.table("t").insert({name:"g",age:1}).run(); throw new Error("boom"); }); }
               catch (e) { txErr = true; }
               const g = await db.table("t").select(["name"]).where({field:"name",op:"eq",value:"g"}).all();
               json.ok({ ins, upd, del, n, txErr, g: g.length });
             })().catch(e => json.fail(500, String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert_eq!(v["data"]["ins"], 2, "{v}");
    assert_eq!(v["data"]["upd"], 1, "{v}");
    assert_eq!(v["data"]["del"], 1, "{v}");
    assert_eq!(v["data"]["n"], 5, "{v}"); // 4 种子 + e - f + e 留下 = 5
    assert_eq!(v["data"]["txErr"], true, "{v}");
    assert_eq!(v["data"]["g"], 0, "{v}"); // 回滚
}

#[tokio::test(flavor = "current_thread")]
async fn dml_rejects_bad_shapes() {
    let b = seeded_bridge().await;
    // 行间键集不一致
    let cap = b.run(r#"db.table("t").insert([{name:"x"},{name:"y",age:1}]).run()
        .then(()=>json.ok({})).catch(e=>json.fail(400,String(e)));"#).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert!(v["msg"].as_str().unwrap().contains("identical key sets"), "{v}");
    // 键不在白名单
    let cap = b.run(r#"db.table("t").insert({nope:1}).run()
        .then(()=>json.ok({})).catch(e=>json.fail(400,String(e)));"#).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert!(v["msg"].as_str().unwrap().contains("unknown column 'nope'"), "{v}");
    // JS 链层早抛：update 无 where
    let cap = b.run(r#"db.table("t").update({age:1}).run()
        .then(()=>json.ok({})).catch(e=>json.fail(400,String(e)));"#).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert!(v["msg"].as_str().unwrap().contains("requires where"), "{v}");
}
```

- [x] **Step 2: 跑测试确认失败**

Run: `cargo test --release dml_ 2>&1 | tail -3`
Expected: FAIL（`.insert is not a function` 等）。

- [x] **Step 3: 最小实现**

`build_statement` 动词分发（select 段保持，新增三分支；白名单键校验 + 键集一致 + `values()` Result，禁 `values_panic`）：

```rust
fn build_statement(
    req: &QueryReq,
    reg: &SchemaRegistry,
    dialect: Dialect,
) -> Result<(String, Vec<Value>), JsErrorBox> {
    validate_verb(req)?;
    let table = reg
        .get(&req.table)
        .ok_or_else(|| JsErrorBox::generic(format!("unknown table '{}'", req.table)))?;
    let where_into = |leaves: &mut usize| -> Result<Vec<SimpleExpr>, JsErrorBox> {
        req.conditions
            .iter()
            .map(|c| cond_expr(c, table, 1, leaves))
            .collect()
    };
    match req.verb {
        Verb::Insert => {
            let keys: Vec<String> = req.values[0].keys().cloned().collect();
            for k in &keys {
                if !table.has_column(k) {
                    return Err(JsErrorBox::generic(format!(
                        "unknown column '{k}' in insert values"
                    )));
                }
            }
            let mut ins = Query::insert();
            ins.into_table(Alias::new(&req.table))
                .columns(keys.iter().map(Alias::new));
            for row in &req.values {
                if row.len() != keys.len() || !row.keys().all(|k| keys.contains(k)) {
                    return Err(JsErrorBox::generic(
                        "insert rows must share identical key sets",
                    ));
                }
                let vals: Vec<SimpleExpr> = keys
                    .iter()
                    .map(|k| Expr::val(to_qv(&row[k])).into())
                    .collect();
                ins.values(vals)
                    .map_err(|e| JsErrorBox::generic(format!("insert values: {e}")))?;
            }
            let (sql, values) = build_sql(dialect, &ins);
            let params = values.iter().map(value_to_json).collect::<Result<Vec<_>, _>>()?;
            Ok((sql, params))
        }
        Verb::Update => {
            let mut up = Query::update();
            up.table(Alias::new(&req.table));
            for (k, v) in &req.sets {
                if !table.has_column(k) {
                    return Err(JsErrorBox::generic(format!(
                        "unknown column '{k}' in update sets"
                    )));
                }
                up.value(Alias::new(k), to_qv(v));
            }
            let mut leaves = 0;
            for e in where_into(&mut leaves)? {
                up.and_where(e);
            }
            let (sql, values) = build_sql(dialect, &up);
            let params = values.iter().map(value_to_json).collect::<Result<Vec<_>, _>>()?;
            Ok((sql, params))
        }
        Verb::Delete => {
            let mut del = Query::delete();
            del.from_table(Alias::new(&req.table));
            let mut leaves = 0;
            for e in where_into(&mut leaves)? {
                del.and_where(e);
            }
            let (sql, values) = build_sql(dialect, &del);
            let params = values.iter().map(value_to_json).collect::<Result<Vec<_>, _>>()?;
            Ok((sql, params))
        }
        Verb::Select => {
            // 原 select 构造段原样搬入（列白名单/where/order_by/limit/offset），
            // 末尾 build_sql(dialect, &q) + value_to_json 参数化，与 Task 1 产出同形。
            build_select_body(req, table, dialect)
        }
    }
}
```

（实现时把 Task 1 搬入的 select 段再抽成 `fn build_select_body(req, table, dialect) ->
Result<(String, Vec<Value>), JsErrorBox>`，四个动词分支平级——DRY。）

`op_db_query_build` 返回形态变更 + DML tx 路由：

```rust
#[op2]
#[serde]
pub async fn op_db_query_build(
    state: Rc<RefCell<OpState>>,
    #[serde] req: QueryReq,
) -> Result<Value, JsErrorBox> {
    let reg = registry(&state)?;
    guard_req(&state, &req)?;
    let (sql, params) = build_statement(&req, &reg, lookup(&state, &req.db)?.dialect())?;
    let is_select = req.verb == Verb::Select;
    match super::db::resolve_target(&state, &req.db)? {
        super::db::Target::Pool(da) => {
            if is_select {
                da.query_with_params(&sql, &params)
                    .await
                    .map(Value::Array)
                    .map_err(|e| JsErrorBox::generic(e.to_string()))
            } else {
                da.exec_with_params(&sql, &params)
                    .await
                    .map(|n| Value::from(n))
                    .map_err(|e| JsErrorBox::generic(e.to_string()))
            }
        }
        super::db::Target::Tx(t) => {
            let s = t.session.lock().await;
            if is_select {
                s.query(&sql, &params)
                    .await
                    .map(Value::Array)
                    .map_err(|e| JsErrorBox::generic(e.to_string()))
            } else {
                s.exec(&sql, &params)
                    .await
                    .map(Value::from)
                    .map_err(|e| JsErrorBox::generic(e.to_string()))
            }
        }
    }
}
```

（tx session 的 `exec` 返回类型若非 u64，按实际 `impl From` 调整；`lookup`/`resolve_target` 语义不变。）

`bootstrap.js` 的 queryBuilder api 增：

```js
    insert(rows) { req.verb = "insert"; req.values = (Array.isArray(rows) ? rows : [rows]).map((r) => ({ ...r })); return api; },
    update(sets) { req.verb = "update"; req.sets = { ...sets }; return api; },
    delete() { req.verb = "delete"; return api; },
    run() {
      if ((req.verb === "update" || req.verb === "delete") && req.conditions.length === 0) {
        throw new Error(req.verb + " requires where");
      }
      if (req.verb === "insert" && req.values.length === 0) {
        throw new Error("insert needs at least one row");
      }
      return op_db_query_build(req);
    },
```

- [x] **Step 4: 跑测试确认通过 + 全量回归**

Run: `cargo test --release query:: 2>&1 | tail -3 && cargo test --release 2>&1 | tail -3`
Expected: 全 PASS（含旧 select 链路、`dml_` 两测试）。

- [x] **Step 5: Commit**

```bash
git add src/bridge/query.rs src/bridge/bootstrap.js
git commit -m "feat(query): DML insert/update/delete（tx 路由 + 返回行数 + run() 终执行）

unix@vip.qq.com ai"
```

### Phase 4 收尾：更新与总结

- [x] 勾掉 Phase 4 checkbox；`git log --oneline -2` 核对；总结「DML 落地，op 返回形态已换 Result<Value>，DML 进 tx」。

---

## Phase 5：join 联表

### Task 8：限定列 resolver（ColCtx，六处共用）

**Files:**
- Modify: `src/bridge/query.rs`（ColCtx + cond_expr/select/order_by 改用）
- Test: `src/bridge/query.rs` 测试模块

**Interfaces:**
- Produces（Phase 5/6 共用）:
  - `struct ColCtx<'a> { base_name: &'a str, base: &'a TableDef, joins: Vec<(&'a str, &'a TableDef)> }`
  - `impl ColCtx { fn check_col(&self, col: &str, site: &str) -> Result<(), JsErrorBox> }`
  - `fn col_simple_expr(col: &str) -> SimpleExpr`（qualified → `Expr::col((Alias, Alias))`）
  - 规则：`"t.col"` → 表段 ∈ {基表} ∪ {join 表}，列段对该表校验；非限定 → 仅基表（join 存在时拒绝命中 join 表——非限定根本不查 join 表，天然拒绝歧义）。

- [x] **Step 1: 写失败测试**

```rust
#[test]
fn col_ctx_qualified_and_unqualified_rules() {
    let reg = SchemaRegistry::new()
        .table("a", &["id"], &["id", "name"])
        .table("b", &["id"], &["id", "aid", "label"]);
    let ctx = ColCtx {
        base_name: "a",
        base: reg.get("a").unwrap(),
        joins: vec![("b", reg.get("b").unwrap())],
    };
    assert!(ctx.check_col("name", "select").is_ok());
    assert!(ctx.check_col("b.label", "select").is_ok());
    assert!(ctx.check_col("a.id", "where").is_ok());
    assert!(ctx.check_col("label", "select").is_err()); // 非限定不解析 join 表
    assert!(ctx.check_col("b.nope", "select").is_err());
    assert!(ctx.check_col("c.id", "select").is_err()); // 表段不在 {a, b}
}
```

- [x] **Step 2: 跑测试确认失败**

Run: `cargo test --release col_ctx 2>&1 | tail -3`
Expected: FAIL（ColCtx 未定义）。

- [x] **Step 3: 最小实现**

```rust
/// 限定列解析（select/where/orderBy/groupBy/having/join on 六处共用）。
struct ColCtx<'a> {
    base_name: &'a str,
    base: &'a TableDef,
    joins: Vec<(&'a str, &'a TableDef)>,
}

impl ColCtx<'_> {
    fn check_col(&self, col: &str, site: &str) -> Result<(), JsErrorBox> {
        match col.split_once('.') {
            Some((t, c)) => {
                let td = if t == self.base_name {
                    self.base
                } else if let Some((_, td)) = self.joins.iter().find(|(n, _)| *n == t) {
                    td
                } else {
                    return Err(JsErrorBox::generic(format!(
                        "unknown table '{t}' in {site}"
                    )));
                };
                if !td.has_column(c) {
                    return Err(JsErrorBox::generic(format!(
                        "unknown column '{col}' in {site}"
                    )));
                }
                Ok(())
            }
            // 非限定只对基表解析：join 存在时不查 join 表（消歧，强制全限定名）。
            None if self.base.has_column(col) => Ok(()),
            None => Err(JsErrorBox::generic(format!(
                "unknown column '{col}' in {site}"
            ))),
        }
    }
}

/// 列引用 → SimpleExpr（qualified → (table, col) 元组）。
fn col_simple_expr(col: &str) -> SimpleExpr {
    match col.split_once('.') {
        Some((t, c)) => Expr::col((Alias::new(t), Alias::new(c))).into(),
        None => Expr::col(Alias::new(col)).into(),
    }
}
```

`cond_expr` 签名改为 `(t, ctx: &ColCtx, depth, leaves)`，Leaf 分支改：

```rust
            ctx.check_col(&c.field, "where")?;
            apply_op(col_simple_expr(&c.field), c.op, &c.value)
```

同时把 `build_expr` 泛化为（having 别名展开 Phase 6 复用）：

```rust
fn apply_op<T: sea_query::ExprTrait>(t: T, op: Op, val: &Option<Value>) -> Result<SimpleExpr, JsErrorBox> {
    let rhs = |v: &Value| Expr::val(to_qv(v));
    Ok(match op {
        Op::Eq => t.eq(rhs(val.as_ref().unwrap_or(&Value::Null))),
        Op::Ne => t.ne(rhs(val.as_ref().unwrap_or(&Value::Null))),
        Op::Gt => t.gt(rhs(val.as_ref().ok_or_else(|| JsErrorBox::generic("gt needs value"))?)),
        Op::Gte => t.gte(rhs(val.as_ref().ok_or_else(|| JsErrorBox::generic("gte needs value"))?)),
        Op::Lt => t.lt(rhs(val.as_ref().ok_or_else(|| JsErrorBox::generic("lt needs value"))?)),
        Op::Lte => t.lte(rhs(val.as_ref().ok_or_else(|| JsErrorBox::generic("lte needs value"))?)),
        Op::In => {
            let arr = val
                .as_ref()
                .and_then(|v| v.as_array())
                .ok_or_else(|| JsErrorBox::generic("in needs array value"))?;
            let vals: Vec<Expr> = arr.iter().map(rhs).collect();
            t.is_in(vals)
        }
        Op::Like => {
            let v = val.as_ref().ok_or_else(|| JsErrorBox::generic("like needs value"))?;
            let pat = match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            t.like(LikeExpr::new(pat))
        }
        Op::IsNull => t.is_null(),
    })
}
```

`build_statement` 的 select 列/order_by/where 全部改经 `ColCtx`（本 Task 先 `joins: vec![]`
——join 表 Task 9 填入；select 列从 `Alias` 列表改为 `q.exprs(cols.iter().map(col_simple_expr))`
或 `q.column(col_ref)`——qualified 时用元组）。`ExprTrait` 需 `use sea_query::ExprTrait`（已存在）。
`Expr::col(Alias).eq(...)` 旧调用点替换为 `apply_op(col_simple_expr(...), ...)`。

- [x] **Step 4: 跑测试确认通过 + 旧回归**

Run: `cargo test --release query:: 2>&1 | tail -3`
Expected: 全 PASS（旧单层条件/排序/未知列报错文案保持——既有测试 `unknown column 'nope' in where` 仍过，site 参数用 "where"）。

注意：既有 `unknown_column_in_select_and_where_errors` 断言文案 `unknown column 'nope' on 't'`——select 段改 ColCtx 后 site 用 `format!("on '{}'", req.table)` 或保留旧文案，以既有测试不改为准（site = `"on 't'"` 形态可由调用点拼）。

- [x] **Step 5: Commit**

```bash
git add src/bridge/query.rs
git commit -m "refactor(query): ColCtx 限定列解析（六处共用，apply_op 泛化）

unix@vip.qq.com ai"
```

### Task 9：joins 构造 + 守卫 + 自 join 拒绝

**Files:**
- Modify: `src/bridge/query.rs`（Join/OnPair/JoinKind + build_statement select 分支 + guard_req/validate_verb 扩展）、`src/bridge/bootstrap.js`（`.join()`）
- Test: `src/bridge/query.rs` 测试模块

**Interfaces:**
- Consumes: Task 8 的 `ColCtx`/`col_simple_expr`。
- Produces:
  - `struct Join { table: String, kind: JoinKind(inner 默认|left), on: Vec<OnPair{left,right}> }`（on 列对列等值，无 op 字段；on 非空；right join 不做；自 join 拒绝）
  - JS `.join(table, on, kind?)`
  - 矩阵扩展：insert/update/delete 拒 joins
  - guard_req 扩展：每个 join 表过 `check_table`

- [x] **Step 1: 写失败测试（两表夹具）**

`seeded_bridge` 旁加：

```rust
async fn seeded_bridge_2t() -> Bridge {
    let db = SqlxAccessor::arc("sqlite::memory:").await.unwrap();
    db.exec_with_params("create table a (id integer primary key, name text)", &[]).await.unwrap();
    db.exec_with_params("create table b (id integer primary key, aid integer, label text)", &[]).await.unwrap();
    for (n,) in [("x",), ("y",)] {
        db.exec_with_params("insert into a (name) values (?)", &[json!(n)]).await.unwrap();
    }
    for (aid, l) in [(1, "L1"), (1, "L2"), (3, "L3")] {
        db.exec_with_params("insert into b (aid, label) values (?, ?)", &[json!(aid), json!(l)]).await.unwrap();
    }
    let reg = SchemaRegistry::new()
        .table("a", &["id"], &["id", "name"])
        .table("b", &["id"], &["id", "aid", "label"]);
    Bridge::with_opts(db, Arc::new(InMemoryKV::new()), reg, false)
}

#[tokio::test(flavor = "current_thread")]
async fn join_inner_left_and_rejections() {
    let b = seeded_bridge_2t().await;
    // inner join：a(id=1 x, 2 y) × b(aid=1 ×2) → 2 行
    let cap = b.run(r#"db.table("a").join("b", [{left:"a.id",right:"b.aid"}])
        .select(["a.name","b.label"]).all()
        .then(r=>json.ok({n:r.length, first:r[0].label})).catch(e=>json.fail(400,String(e)));"#).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert_eq!(v["data"]["n"], 2, "{v}");
    // left join：3 行（x×2, y×1 NULL label）
    let cap = b.run(r#"db.table("a").join("b", [{left:"a.id",right:"b.aid"}], "left")
        .select(["a.name","b.label"]).orderBy([{field:"a.id",dir:"asc"}]).all()
        .then(r=>json.ok({n:r.length})).catch(e=>json.fail(400,String(e)));"#).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["n"], 3, "{v}");
    // join 表未知 / 自 join / on 列未知 / 非限定列命中 join 表
    for (js, want) in [
        (r#"db.table("a").join("nope",[{left:"a.id",right:"nope.aid"}]).select(["a.id"]).all()"#, "unknown table 'nope'"),
        (r#"db.table("a").join("a",[{left:"a.id",right:"a.id"}]).select(["a.id"]).all()"#, "self join not supported"),
        (r#"db.table("a").join("b",[{left:"a.id",right:"b.nope"}]).select(["a.id"]).all()"#, "unknown column 'b.nope'"),
        (r#"db.table("a").join("b",[{left:"a.id",right:"b.aid"}]).select(["label"]).all()"#, "unknown column 'label'"),
    ] {
        let cap = b.run(&format!(r#"{js}.then(()=>json.ok({{}})).catch(e=>json.fail(400,String(e)));"#)).await.unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(v["msg"].as_str().unwrap().contains(want), "{want}: {v}");
    }
}
```

矩阵扩展进 Task 6 的 `verb_matrix_enforced_op_side`：

```rust
    assert!(bad(r#"{"table":"t","verb":"insert","values":[{"a":1}],"joins":[{"table":"b","on":[{"left":"a.id","right":"b.aid"}]}]}"#).to_string().contains("insert does not accept joins"));
```

- [x] **Step 2: 跑测试确认失败**

Run: `cargo test --release join_ 2>&1 | tail -3`
Expected: FAIL（`.join is not a function`）。

- [x] **Step 3: 最小实现**

```rust
/// join 种类（right 不做：sqlite 旧版本不支持且无用例）。
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum JoinKind {
    #[default]
    Inner,
    Left,
}

/// on 条件：列对列等值（无 op 字段，deny_unknown_fields 防自以为能写 op）。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct OnPair {
    left: String,
    right: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Join {
    table: String,
    #[serde(default)]
    kind: JoinKind,
    on: Vec<OnPair>,
}
```

`QueryReq` 增 `#[serde(default)] joins: Vec<Join>`。
`validate_verb` 的 Insert/Update/Delete 分支各加 `if !req.joins.is_empty() { return reject("<verb>", "joins"); }`。

`build_statement` select 分支开头（列校验之前）：

```rust
        let mut join_defs: Vec<(&str, &TableDef)> = Vec::new();
        for j in &req.joins {
            if j.table == req.table {
                return Err(JsErrorBox::generic(
                    "self join not supported (no table alias)",
                ));
            }
            if j.on.is_empty() {
                return Err(JsErrorBox::generic(format!(
                    "join '{}' needs non-empty on",
                    j.table
                )));
            }
            let td = reg
                .get(&j.table)
                .ok_or_else(|| JsErrorBox::generic(format!("unknown table '{}'", j.table)))?;
            join_defs.push((j.table.as_str(), td));
        }
        let ctx = ColCtx { base_name: &req.table, base: table, joins: join_defs };
```

随后 join 装配（在 from 之后、where 之前）：

```rust
        for j in &req.joins {
            let mut on = sea_query::Condition::all();
            for p in &j.on {
                ctx.check_col(&p.left, "join on")?;
                ctx.check_col(&p.right, "join on")?;
                on = on.add(col_simple_expr(&p.left).equals(col_simple_expr(&p.right)));
            }
            let jt = match j.kind {
                JoinKind::Inner => sea_query::JoinType::InnerJoin,
                JoinKind::Left => sea_query::JoinType::LeftJoin,
            };
            q.join(jt, Alias::new(&j.table), on);
        }
```

（`equals` 来自 `ExprTrait`：`SimpleExpr` 上需 `use sea_query::ExprTrait`；若 SimpleExpr 无
equals，包 `Expr::expr(col_simple_expr(&p.left)).equals(col_simple_expr(&p.right))`。）

`guard_req` 扩展：

```rust
fn guard_req(state: &Rc<RefCell<OpState>>, req: &QueryReq) -> Result<(), JsErrorBox> {
    super::guard::check_table(state, &req.table)?;
    for j in &req.joins {
        super::guard::check_table(state, &j.table)?;
    }
    Ok(())
}
```

`bootstrap.js`：

```js
    join(table, on, kind) { req.joins.push({ table: String(table), on: (on || []).map((p) => ({ left: String(p.left), right: String(p.right) })), kind: kind ? String(kind) : "inner" }); return api; },
```

req 初始值加 `joins: []`。

- [x] **Step 4: 跑测试确认通过 + 旧回归 + clippy**

Run: `cargo test --release query:: 2>&1 | tail -3 && cargo clippy --release --all-targets -- -D warnings 2>&1 | tail -2`
Expected: 全 PASS，clippy 零警告。

- [x] **Step 5: Commit**

```bash
git add src/bridge/query.rs src/bridge/bootstrap.js
git commit -m "feat(query): join 联表（inner/left，限定列校验，自 join 拒绝，join 表过守卫）

unix@vip.qq.com ai"
```

### Phase 5 收尾：更新与总结

- [x] 勾掉 Phase 5 checkbox；`git log --oneline -2` 核对；总结「join 落地，限定列六处统一走 ColCtx」。

---

## Phase 6：聚合 / 分组 / having / distinct

### Task 10：ColSpec（聚合列 + 别名）+ distinct

**Files:**
- Modify: `src/bridge/query.rs`（AggFn/ColSpec + select 列构造 + 别名正则）、`src/bridge/bootstrap.js`（`.distinct()`）
- Test: `src/bridge/query.rs` 测试模块

**Interfaces:**
- Produces:
  - `enum AggFn { Count, Sum, Avg, Min, Max }`（serde lowercase，未知 fn 报错）
  - `enum ColSpec { Name(String), Agg { r#fn: AggFn, field: Option<String>, r#as: Option<String> } }`（untagged，String 变体优先；`#[serde(rename = "fn")]` / `#[serde(rename = "as")]`）
  - `fn check_alias(a: &str) -> Result<(), JsErrorBox>`（`^[A-Za-z_][A-Za-z0-9_]*$`）
  - count 的 field 可省略或 `"*"` → `Expr::col(Asterisk)`；其余聚合 field 必填
  - `QueryReq.distinct: bool`（默认 false）；JS `.distinct()`
  - 矩阵扩展：insert/update/delete 拒 distinct
  - 别名台账：`build_statement` 收集 `HashMap<String, SimpleExpr>`（alias → 聚合表达式），Task 11 having 用

- [x] **Step 1: 写失败测试**

```rust
#[tokio::test(flavor = "current_thread")]
async fn aggregate_columns_alias_and_distinct() {
    let b = seeded_bridge().await;
    let cap = b
        .run(
            r#"Promise.all([
                 db.table("t").select([{fn:"count",as:"n"}]).all(),
                 db.table("t").select([{fn:"sum",field:"age",as:"total"}]).all(),
                 db.table("t").select(["tag"]).distinct().all(),
               ]).then(([c, s, d]) => json.ok({ n: c[0].n, total: s[0].total, d: d.length }))
                 .catch(e => json.fail(500, String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert_eq!(v["data"]["n"], 4, "{v}");
    assert_eq!(v["data"]["total"], 100, "{v}"); // 10+20+30+40
    assert_eq!(v["data"]["d"], 3, "{v}"); // x, y, NULL
    // 非法别名 / 未知 fn / sum 缺 field
    for (sel, want) in [
        (r#"[{fn:"count",as:"1bad"}]"#, "illegal alias"),
        (r#"[{fn:"median",field:"age"}]"#, "unknown variant"),
        (r#"[{fn:"sum"}]"#, "aggregate needs field"),
    ] {
        let cap = b.run(&format!(r#"db.table("t").select({sel}).all().then(()=>json.ok({{}})).catch(e=>json.fail(400,String(e)));"#)).await.unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(v["msg"].as_str().unwrap().contains(want), "{want}: {v}");
    }
}
```

（未知 fn 的 serde 文案是 "unknown variant"；若 op2 包装吃掉前缀，断言放宽为含 `median` 字样。）

- [x] **Step 2: 跑测试确认失败**

Run: `cargo test --release aggregate_ 2>&1 | tail -3`
Expected: FAIL（distinct/聚合列不支持）。

- [x] **Step 3: 最小实现**

```rust
/// 聚合函数（类型化枚举，非自由字符串——红线）。
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum AggFn {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

/// select 列：列名（可限定 "t.col"）或聚合 {fn, field?, as?}。
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum ColSpec {
    Name(String),
    Agg {
        #[serde(rename = "fn")]
        r#fn: AggFn,
        #[serde(default)]
        field: Option<String>,
        #[serde(rename = "as", default)]
        r#as: Option<String>,
    },
}

/// 别名形状纵深校验（发射只经 Alias::new 引号包裹，正则再挡一层）。
fn check_alias(a: &str) -> Result<(), JsErrorBox> {
    let ok = !a.is_empty()
        && a.bytes().enumerate().all(|(i, b)| {
            b.is_ascii_alphabetic() || b == b'_' || (i > 0 && b.is_ascii_digit())
        });
    if ok {
        Ok(())
    } else {
        Err(JsErrorBox::generic(format!("illegal alias '{a}'")))
    }
}
```

`QueryReq.columns` 类型换 `Vec<ColSpec>`；select 分支列构造改：

```rust
        // 聚合别名台账（alias → 原表达式），having 展开用（Task 11）。
        let mut agg_aliases: HashMap<String, SimpleExpr> = HashMap::new();
        if req.columns.is_empty() {
            let cols: Vec<SimpleExpr> = if req.joins.is_empty() {
                table.columns.keys().map(|c| col_simple_expr(c)).collect()
            } else {
                table.columns.keys().map(|c| col_simple_expr(&format!("{}.{c}", req.table))).collect()
            };
            q.exprs(cols);
        } else {
            for spec in &req.columns {
                match spec {
                    ColSpec::Name(c) => {
                        ctx.check_col(c, "select")?;
                        q.expr(col_simple_expr(c));
                    }
                    ColSpec::Agg { r#fn, field, r#as } => {
                        let arg: SimpleExpr = match (r#fn, field.as_deref()) {
                            (AggFn::Count, None) | (AggFn::Count, Some("*")) => {
                                Expr::col(sea_query::Asterisk).into()
                            }
                            (_, None) => {
                                return Err(JsErrorBox::generic(
                                    "aggregate needs field (only count allows omission)",
                                ));
                            }
                            (_, Some(f)) => {
                                ctx.check_col(f, "select")?;
                                col_simple_expr(f)
                            }
                        };
                        let e: SimpleExpr = match r#fn {
                            AggFn::Count => sea_query::Func::count(arg),
                            AggFn::Sum => sea_query::Func::sum(arg),
                            AggFn::Avg => sea_query::Func::avg(arg),
                            AggFn::Min => sea_query::Func::min(arg),
                            AggFn::Max => sea_query::Func::max(arg),
                        }
                        .into();
                        if let Some(a) = r#as {
                            check_alias(a)?;
                            agg_aliases.insert(a.clone(), e.clone());
                            q.expr_as(e, Alias::new(a));
                        } else {
                            q.expr(e);
                        }
                    }
                }
            }
        }
        if req.distinct {
            q.distinct();
        }
```

`validate_verb` 的 Insert/Update/Delete 分支加 `if req.distinct { return reject("<verb>", "distinct"); }`。
`QueryReq` 增 `#[serde(default)] distinct: bool`。
`bootstrap.js`：req 初始值加 `distinct: false`；api 增
`distinct() { req.distinct = true; return api; },`。

注意：`Func::count(arg)` 返回 `FunctionCall`，`.into()` 转 SimpleExpr（`From<FunctionCall> for SimpleExpr` 存在；若类型不匹配用 `SimpleExpr::FunctionCall(fc)`）。`q.expr(...)`/`q.exprs(...)` 接受 `Into<SimpleExpr>`。

- [x] **Step 4: 跑测试确认通过 + 旧回归**

Run: `cargo test --release query:: 2>&1 | tail -3`
Expected: 全 PASS（旧 `select(["name"])` 字符串列兼容——untagged String 变体）。

- [x] **Step 5: Commit**

```bash
git add src/bridge/query.rs src/bridge/bootstrap.js
git commit -m "feat(query): 聚合列（fn 枚举 + 别名纵深校验）+ distinct

unix@vip.qq.com ai"
```

### Task 11：groupBy + having（别名展开）

**Files:**
- Modify: `src/bridge/query.rs`（group_by/having + having 编译别名展开）、`src/bridge/bootstrap.js`（`.groupBy()/.having()`）
- Test: `src/bridge/query.rs` 测试模块

**Interfaces:**
- Consumes: Task 10 的 `agg_aliases`、`apply_op`。
- Produces:
  - `QueryReq.group_by: Vec<String>`、`QueryReq.having: Option<CondTree>`
  - having 字段解析序：**白名单列优先（ColCtx），再聚合别名展开**（别名→原 Func 表达式进 HAVING，PG 不允许 HAVING 引用 select 输出别名）
  - JS `.groupBy(cols)` / `.having(cond)`（接受条件对象）
  - 矩阵扩展：insert/update/delete 拒 groupBy/having

- [x] **Step 1: 写失败测试**

```rust
#[tokio::test(flavor = "current_thread")]
async fn group_by_having_with_alias_expansion() {
    let b = seeded_bridge().await;
    // 按 tag 分组 count>1 → tag=x (2 行)
    let cap = b
        .run(
            r#"db.table("t").select(["tag", {fn:"count",as:"n"}])
               .groupBy(["tag"]).having({field:"n",op:"gt",value:1})
               .orderBy([{field:"tag",dir:"asc"}]).all()
               .then(r => json.ok({ rows: r.length, tag: r[0].tag, n: r[0].n }))
               .catch(e => json.fail(500, String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    assert_eq!(v["data"]["rows"], 1, "{v}");
    assert_eq!(v["data"]["n"], 2, "{v}");
    // having 用白名单列（列优先于别名）
    let cap = b
        .run(
            r#"db.table("t").select(["tag", {fn:"sum",field:"age",as:"n"}])
               .groupBy(["tag"]).having({field:"age",op:"gt",value:0})
               .all().then(r => json.ok({ n: r.length })).catch(e => json.fail(500, String(e)));"#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
    // having 引用未知别名/列 → 报错
    let cap = b.run(r#"db.table("t").select(["tag"]).groupBy(["tag"]).having({field:"zz",op:"eq",value:1}).all()
        .then(()=>json.ok({})).catch(e=>json.fail(400,String(e)));"#).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert!(v["msg"].as_str().unwrap().contains("unknown column 'zz' in having"), "{v}");
    // 别名展开的 SQL 不含别名字样（PG 方言验证）
    let cap = b.run(r#"json.ok(db.table("t").select([{fn:"count",as:"n"}]).groupBy(["tag"]).having({field:"n",op:"gt",value:1}).toSQL());"#).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    let sql = v["data"]["sql"].as_str().unwrap().to_string();
    assert!(sql.contains("COUNT(") && !sql.contains("HAVING \"n\""), "{sql}");
}
```

- [x] **Step 2: 跑测试确认失败**

Run: `cargo test --release group_by_having 2>&1 | tail -3`
Expected: FAIL（`.groupBy is not a function`）。

- [x] **Step 3: 最小实现**

`QueryReq` 增 `#[serde(default)] group_by: Vec<String>`、`#[serde(default)] having: Option<CondTree>`。

having 编译（别名展开版 cond_expr——给 `cond_expr` 加可选别名台账参数）：

```rust
/// having 专用：leaf field 先走 ColCtx（白名单列优先），未命中再查聚合别名台账，
/// 命中则以原聚合表达式重组（PG 不允许 HAVING 引用 select 输出别名——展开消灭方言分叉）。
fn cond_expr_having(
    t: &CondTree,
    ctx: &ColCtx,
    aliases: &HashMap<String, SimpleExpr>,
    depth: usize,
    leaves: &mut usize,
) -> Result<SimpleExpr, JsErrorBox> {
    match t {
        CondTree::Leaf(c) if ctx.check_col(&c.field, "having").is_err() => {
            let e = aliases.get(&c.field).ok_or_else(|| {
                JsErrorBox::generic(format!("unknown column '{}' in having", c.field))
            })?;
            *leaves += 1;
            if *leaves > COND_LEAF_MAX {
                return Err(JsErrorBox::generic("condition tree too large (max 64 leaves)"));
            }
            apply_op(Expr::expr(e.clone()), c.op, &c.value)
        }
        // 其余分支委托通用递归（Leaf 列命中 / And / Or / Not 结构同形）
        _ => cond_expr_generic(t, &mut |c| {
            if ctx.check_col(&c.field, "having").is_ok() {
                apply_op(col_simple_expr(&c.field), c.op, &c.value)
            } else {
                let e = aliases.get(&c.field).unwrap();
                apply_op(Expr::expr(e.clone()), c.op, &c.value)
            }
        }, depth, leaves),
    }
}
```

（实现时把 `cond_expr` 重构为 `cond_expr_generic(t, leaf_fn, depth, leaves)`：where 的 leaf_fn =
白名单校验 + `apply_op(col_simple_expr(..))`；having 的 leaf_fn = 上述别名展开逻辑——DRY，
树形递归只写一份。）

select 分支末尾（order_by 之前）：

```rust
        for g in &req.group_by {
            ctx.check_col(g, "groupBy")?;
        }
        if !req.group_by.is_empty() {
            q.group_by_columns(req.group_by.iter().map(|g| match g.split_once('.') {
                Some((t, c)) => (Alias::new(t), Alias::new(c)).into_column_ref(),
                None => Alias::new(g).into_column_ref(),
            }));
        }
        if let Some(h) = &req.having {
            let mut leaves = 0;
            let e = cond_expr_having(h, &ctx, &agg_aliases, 1, &mut leaves)?;
            let mut cond = sea_query::Condition::all();
            cond = cond.add(e);
            q.cond_having(cond);
        }
```

（`into_column_ref` 来自 `sea_query::IntoColumnRef`，补 import。）

`validate_verb` 的 Insert/Update/Delete 分支加：
`if !req.group_by.is_empty() { return reject("<verb>", "groupBy"); }`
`if req.having.is_some() { return reject("<verb>", "having"); }`

`bootstrap.js`：req 初始值加 `group_by: [], having: null`；api 增：

```js
    groupBy(cols) { req.group_by = (cols || []).map(String); return api; },
    having(cond) { req.having = unwrapCond(cond); return api; },
```

- [x] **Step 4: 跑测试确认通过 + 全量回归 + clippy**

Run: `cargo test --release query:: 2>&1 | tail -3 && cargo clippy --release --all-targets -- -D warnings 2>&1 | tail -2`
Expected: 全 PASS（含 PG 方言别名展开断言），clippy 零警告。

- [x] **Step 5: Commit**

```bash
git add src/bridge/query.rs src/bridge/bootstrap.js
git commit -m "feat(query): groupBy + having（聚合别名 op 侧展开，PG 方言兼容）

unix@vip.qq.com ai"
```

### Phase 6 收尾：更新与总结

- [x] 勾掉 Phase 6 checkbox；`git log --oneline -2` 核对；总结「聚合/分组/having 落地，别名展开消灭方言分叉」。

---

## Phase 7：序列化 + e2e + 文档 + 版本

### Task 12：toJSON / fromJSON

**Files:**
- Modify: `src/bridge/bootstrap.js`（queryBuilder 重构为 builderFromReq + DB facade 加 fromJSON）
- Test: `src/bridge/query.rs` 测试模块

**Interfaces:**
- Produces:
  - `queryBuilder.toJSON()` → req 深拷贝（纯 JSON，条件对象在 where/having 时已解包）
  - `db.fromJSON(snap)` → 按 snap 内 `db`/`table` 复原 builder，可继续链/执行
  - 已知语义（文档级）：快照 `db` 是 JS 可见名，复原时按**复原方**模块 bound_db 重定向

- [x] **Step 1: 写失败测试**

```rust
#[tokio::test(flavor = "current_thread")]
async fn to_json_from_json_roundtrip() {
    let b = seeded_bridge().await;
    let cap = b
        .run(
            r#"const c = db.and({field:"age",op:"gte",value:18});
               const q = db.table("t").select(["name"]).where(c).limit(10);
               const snap = q.toJSON();
               snap.limit = 1;
               const via = await db.fromJSON(snap).all();
               const direct = await db.table("t").select(["name"]).where(c).limit(1).all();
               json.ok({ same: via.length === direct.length && via[0].name === direct[0].name,
                         plain: typeof snap.conditions[0].and !== "undefined" });
               "#,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["same"], true, "{v}");
    assert_eq!(v["data"]["plain"], true, "{v}"); // 条件对象已解包为纯 JSON
    // fromJSON 非法树被 op 拒（同手写非法树）
    let cap = b.run(r#"db.fromJSON({table:"t",verb:"delete"}).run()
        .then(()=>json.ok({})).catch(e=>json.fail(400,String(e)));"#).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert!(v["msg"].as_str().unwrap().contains("requires where"), "{v}");
}
```

- [x] **Step 2: 跑测试确认失败**

Run: `cargo test --release to_json_from_json 2>&1 | tail -3`
Expected: FAIL（`.toJSON is not a function`）。

- [x] **Step 3: 最小实现（bootstrap.js）**

`queryBuilder` 重构：

```js
function builderFromReq(snap) {
  const req = Object.assign(
    { db: "default", table: "", columns: [], conditions: [], order_by: [], limit: null, offset: null, verb: "select", values: [], sets: {}, joins: [], group_by: [], having: null, distinct: false },
    snap,
  );
  req.db = String(req.db); req.table = String(req.table);
  // —— 原 queryBuilder 的 api 对象整体搬入，闭包捕获 req 不变；api 增： ——
  // toJSON() { return JSON.parse(JSON.stringify(req)); },
  // return api;
}
function queryBuilder(name, table) {
  return builderFromReq({ db: name, table });
}
```

DB 缓存对象与 tx 回调对象各加：

```js
      fromJSON: (snap) => builderFromReq(snap),
```

- [x] **Step 4: 跑测试确认通过 + 全量回归**

Run: `cargo test --release 2>&1 | tail -3 && LC_ALL=C grep -P '[^\x00-\x7F]' src/bridge/bootstrap.js; echo ASCII-OK`
Expected: 全 PASS；ASCII 无输出。

- [x] **Step 5: Commit**

```bash
git add src/bridge/bootstrap.js src/bridge/query.rs
git commit -m "feat(query): toJSON/fromJSON 序列化（快照复原可继续链/执行）

unix@vip.qq.com ai"
```

### Task 13：e2e（join + insert 走 HTTP 全链路）

**Files:**
- Test: `oj/tests/e2e.rs`（新增用例；fixture 形态参照既有 `uc1_method_table`——tmpdir + src 模块树 + config + `server_cmd::start`）
- 需模块内 `schema.yaml` 声明两表（SchemaRegistry 装配来源，见 `oj/src/app.rs:222` 的 `table_owned`）

**Interfaces:**
- Consumes: 全链路（HTTP → 路由 → bridge → query.rs）。
- Produces: e2e 用例 `e2e_query_builder_join_and_insert`。

- [x] **Step 1: 写失败/新用例**

`oj/tests/e2e.rs` 追加（fixture 写法照搬既有用例的 tmpdir/config/start 段，替换模块内容）：

```rust
#[tokio::test(flavor = "multi_thread")]
async fn e2e_query_builder_join_and_insert() {
    // 模块 u：schema.yaml 声明 a/b 两表 + api.ts 跑 join 查询与 insert
    // schema.yaml:
    //   tables:
    //     a: { pk: [id], columns: { id: int, name: text } }
    //     b: { pk: [id], columns: { id: int, aid: int, label: text } }
    // api.ts: db.table("a").join("b",[{left:"a.id",right:"b.aid"}]).select(["a.name","b.label"]).all() → json.ok(rows)
    //         db.table("a").insert({name:"z"}).run() → json.ok({n})
    // —— 具体字段名以 oj/src/schema.rs 的 schema.yaml 语法为准（先看既有用例/sample 的 schema.yaml）——
}
```

（实现 Step：先 `grep -rn "schema.yaml" sample/src oj/tests/e2e.rs` 抄既有声明语法；
`db` 用 `sqlite://` 临时文件 DSN 并先经 `db.exec` 建表插种子，或借 module seed.sql 机制——
参照 `server_cmd.rs` 测试 `module_seeds_replayed_and_served` 的 seed.sql 形态。）

- [x] **Step 2: 跑通**

Run: `cargo test --release -p oj --test e2e e2e_query_builder 2>&1 | tail -3`
Expected: PASS。

- [x] **Step 3: Commit**

```bash
git add oj/tests/e2e.rs
git commit -m "test(e2e): query builder join + insert 走 HTTP 全链路

unix@vip.qq.com ai"
```

### Task 14：文档 + 版本 0.1.14 + 全量门禁

**Files:**
- Modify: `docs/devkit/api-manual.md`（`db.table` 段，L571 附近 API 表 + L578 示例区——扩全量：
  嵌套 where / 条件对象（leaf/and/or/not + tree/fields/has）/ insert/update/delete + run() /
  join / 聚合 + groupBy/having / distinct / toSQL / toJSON/fromJSON + 红线提示「update/delete
  必须 where」「标识符白名单」）
- Modify: `docs/superpowers/specs/2026-09-12-db-builder-xorm-align-design.md`（JS 示例的 DML
  两行补 `.run()` 终执行——示例与实现对齐）

（版本 bump 不在本任务——Phase 8 追加后统一由 Task 19 收口。）

- [x] **Step 1: 文档更新**

api-manual.md 的 `db.table` 行扩为新子节（同文件既有格式），含 spec「JS API」节的全部示例
（DML 示例以 `.run()` 收尾）+ 条件对象用法（含多租户守卫 `has("tenant_id")` 示例）+
toJSON 快照库名重定向语义说明。

spec 文件 DML 示例改：

```js
db.table("user").insert({name:"neo", age:1}).run();          // 单行
db.table("user").insert([{name:"a"},{name:"b"}]).run();      // 多行
db.table("user").update({age:2}).where({field:"id",op:"eq",value:1}).run();
db.table("user").delete().where({field:"id",op:"in",value:[1,2]}).run();
```

- [x] **Step 2: 全量门禁**

Run:
```bash
cargo fmt --check 2>&1 | tail -2
cargo clippy --release --all-targets -- -D warnings 2>&1 | tail -2
cargo test --release --workspace 2>&1 | grep -E "test result|error" | tail -10
cargo build --workspace 2>&1 | tail -2
cargo xtask smoke --bin bin/oj 2>&1 | tail -2
```
Expected: fmt/clippy 干净，workspace 测试全绿，构建归置 bin/，smoke 过。

- [x] **Step 3: Commit**

```bash
git add docs/devkit/api-manual.md docs/superpowers/specs/2026-09-12-db-builder-xorm-align-design.md
git commit -m "docs(v0.1.14): db.table 构造器文档（DML/条件树/条件对象/join/聚合/toSQL/序列化）

unix@vip.qq.com ai"
```

### Phase 7 收尾：更新与总结

- [x] 勾掉 Phase 1-7 checkbox；`git log --oneline` 核对；总结 Phase 1-7 落地内容。

---

## Phase 8：sea-query 扩展面（子查询 / UNION / CASE / 窗口 / CTE）

依据 spec「追加：Phase 8」节。五块全部仅 select 可用；嵌套 req 通用约束集中在 Task 15
落地（`REQ_NEST_MAX` + `validate_nested` + `build_select_stmt` 抽取），后续任务复用。

### Task 15：嵌套 select 地基 + where 子查询 + exists

**Files:**
- Modify: `src/bridge/query.rs`（build_select_stmt 抽取、Cond 增 subquery、CondTree 增
  Exists、cond_expr 增 reg/depth 参数、validate_nested、REQ_NEST_MAX）、
  `src/bridge/bootstrap.js`（`__req` 内部字段 + `unwrapSub`/`unwrapTree`）
- Test: `src/bridge/query.rs` 测试模块

**Interfaces:**
- Consumes: Task 4 的 `cond_expr` 递归编译、Task 8 的 `ColCtx`、Task 7 的 select 构造分支。
- Produces:
  - `const REQ_NEST_MAX: u8 = 4`
  - `fn validate_nested(req: &QueryReq, depth: u8, site: &str) -> Result<(), JsErrorBox>`——
    depth >= REQ_NEST_MAX → Err `"nested select too deep"`；verb != Select → Err；
    `!req.with.is_empty() || !req.unions.is_empty()` → Err `"<site>: nested select does not
    accept with/unions (v1)"`（with/unions 字段在 Task 16/18 才加入 QueryReq——本任务先写
    全判断，字段先于本任务落地会导致编译错，故本任务只判断 verb/depth，with/unions 判断
    由 Task 16/18 各自补上）
  - `fn build_select_stmt(req: &QueryReq, reg: &SchemaRegistry, depth: u8)
    -> Result<SelectStatement, JsErrorBox>`——Task 7 select 分支的构造主体原样平移；
    `build_statement` 的 select 臂改为 `build_select_stmt(req, reg, 0)` 再 build
  - `Cond.subquery: Option<Box<QueryReq>>`、`CondTree::Exists(Box<QueryReq>)`
    （手写 Deserialize 增 `"exists"` 键分发）
  - cond_expr 系列签名增 `reg: &SchemaRegistry, depth: u8`（Task 11 的 having 变体同步）
  - JS：builder api 挂 `__req`（内部字段）；`unwrapSub(v)` 取 builder 快照

- [ ] **Step 1: 写失败测试**

夹具用 Task 9 的 `seeded_bridge_2t`（a: id/name；b: id/aid/label）：

```rust
#[tokio::test(flavor = "current_thread")]
async fn subquery_where_and_exists() {
    let b = seeded_bridge_2t().await;
    // in 子查询：b 中 label=L1 的 aid={1} → a.id ∈ {1} → 1 行 x
    let cap = b.run(r#"db.table("a").select(["name"])
        .where({field:"id",op:"in",subquery:db.table("b").select(["aid"])
            .where({field:"label",op:"eq",value:"L1"})}).all()
        .then(r=>json.ok({n:r.length,name:r[0].name})).catch(e=>json.fail(400,String(e)));"#).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["n"], 1, "{v}");
    assert_eq!(v["data"]["name"], "x", "{v}");
    // 标量 eq 子查询：aid of L3 = 3，a 无 id=3 → 0 行；改 L1 → 1 行
    let cap = b.run(r#"db.table("a").select(["name"])
        .where({field:"id",op:"eq",subquery:db.table("b").select(["aid"])
            .where({field:"label",op:"eq",value:"L1"}).limit(1)}).all()
        .then(r=>json.ok({n:r.length})).catch(e=>json.fail(400,String(e)));"#).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["n"], 1, "{v}");
    // exists（非关联）：b 有 L2 → a 全量 2 行
    let cap = b.run(r#"db.table("a").select(["name"])
        .where({exists:db.table("b").select(["aid"]).where({field:"label",op:"eq",value:"L2"})}).all()
        .then(r=>json.ok({n:r.length})).catch(e=>json.fail(400,String(e)));"#).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["n"], 2, "{v}");
    // 裸 JSON 树等价（不走 builder 包装）
    let cap = b.run(r#"db.table("a").select(["name"])
        .where({field:"id",op:"in",subquery:{table:"b",columns:["aid"],
            conditions:[{field:"label",op:"eq",value:"L1"}]}}).all()
        .then(r=>json.ok({n:r.length})).catch(e=>json.fail(400,String(e)));"#).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["n"], 1, "{v}");
}

#[tokio::test(flavor = "current_thread")]
async fn subquery_rejections() {
    let b = seeded_bridge_2t().await;
    for (js, want) in [
        // value 与 subquery 同现
        (r#"db.table("a").where({field:"id",op:"in",value:[1],subquery:{table:"b",columns:["aid"]}}).all()"#,
         "value and subquery are mutually exclusive"),
        // isnull 不接受 subquery
        (r#"db.table("a").where({field:"id",op:"isnull",subquery:{table:"b",columns:["aid"]}}).all()"#,
         "isnull does not accept subquery"),
        // 嵌套 req 动词非 select
        (r#"db.table("a").where({field:"id",op:"in",subquery:{table:"b",verb:"delete",columns:["aid"],
             conditions:[{field:"aid",op:"eq",value:1}]}}).all()"#,
         "nested select"),
        // 嵌套 req 未知列（递归过白名单）
        (r#"db.table("a").where({field:"id",op:"in",subquery:{table:"b",columns:["nope"]}}).all()"#,
         "unknown column"),
    ] {
        let cap = b.run(&format!(r#"{js}.then(()=>json.ok({{}})).catch(e=>json.fail(400,String(e)));"#)).await.unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(v["msg"].as_str().unwrap().contains(want), "{want}: {v}");
    }
    // 深度超限：5 层 exists 嵌套（REQ_NEST_MAX=4）
    let mut js = String::from(r#"db.table("a").select(["id"])"#);
    let mut inner = String::from(r#"{table:"b",columns:["aid"]}"#);
    for _ in 0..5 {
        inner = format!(r#"{{table:"b",columns:["aid"],conditions:[{{exists:{inner}}}]}}"#);
    }
    js.push_str(&format!(r#".where({{exists:{inner}}}).all()"#));
    let cap = b.run(&format!(r#"{js}.then(()=>json.ok({{}})).catch(e=>json.fail(400,String(e)));"#)).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert!(v["msg"].as_str().unwrap().contains("nested select too deep"), "{v}");
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --release subquery_ 2>&1 | tail -3`
Expected: FAIL（`subquery` 未知键 / exists 不识别 / `__req` undefined）。

- [ ] **Step 3: 最小实现**

```rust
const REQ_NEST_MAX: u8 = 4;

/// 嵌套 select 通用约束（site 用于报错定位：subquery/union/cte）。
fn validate_nested(req: &QueryReq, depth: u8, site: &str) -> Result<(), JsErrorBox> {
    if depth >= REQ_NEST_MAX {
        return Err(JsErrorBox::generic(format!("{site}: nested select too deep")));
    }
    if req.verb != Verb::Select {
        return Err(JsErrorBox::generic(format!("{site}: nested select must be select")));
    }
    Ok(())
}
```

`Cond` 增字段：

```rust
    #[serde(default)]
    subquery: Option<Box<QueryReq>>,
```

`CondTree` 增变体 `Exists(Box<QueryReq>)`；手写 Deserialize 分发链加：
`map.contains_key("exists")` → 其余键集合必须为空 → `CondTree::Exists(Box::new(
serde_json::from_value(map.remove("exists")...)?))`（沿用既有「键唯一分发 + 多余键报错」
写法）。叶子计数：Exists 计 1 叶；深度计数：Exists 的子 req 内部条件树独立计数，
但嵌套层数由 REQ_NEST_MAX 管。

`build_select_stmt` 抽取：build_statement 的 `Verb::Select` 臂中「从 from 到 build 之前」
的构造主体平移为：

```rust
/// 构造 select 语句（含 join/where/group/having/order/limit）；depth 为嵌套层数。
fn build_select_stmt(
    req: &QueryReq,
    reg: &SchemaRegistry,
    depth: u8,
) -> Result<SelectStatement, JsErrorBox> {
    // …现有 select 构造主体原样平移，cond_expr 调用点增传 reg/depth…
}
```

`build_statement` 的 Select 臂改为 `build_select_stmt(req, reg, 0)?` 然后照常
`.build(builder)` + `value_to_json`（方言 build 只在最顶层做一次，参数由 sea-query
统一收集——嵌套语句不单独 build）。

`cond_expr`（含 Task 11 的 having 变体）签名增 `reg: &SchemaRegistry, depth: u8`；
Leaf 分支增 subquery 处理：

```rust
// Leaf 内，apply_op 之前：
if let Some(sub) = &leaf.subquery {
    if leaf.op == Op::IsNull {
        return Err(JsErrorBox::generic("isnull does not accept subquery"));
    }
    if leaf.value.is_some() {
        return Err(JsErrorBox::generic("value and subquery are mutually exclusive"));
    }
    validate_nested(sub, depth + 1, "subquery")?;
    let col = ctx.col_simple_expr(&leaf.field, "where")?;   // 沿用 Task 8 限定列解析
    let sel = build_select_stmt(sub, reg, depth + 1)?;
    return Ok(match leaf.op {
        Op::In => Expr::expr(col).in_subquery(sel),
        Op::Eq => Expr::expr(col).eq(sel),
        Op::Ne => Expr::expr(col).ne(sel),
        Op::Gt => Expr::expr(col).gt(sel),
        Op::Gte => Expr::expr(col).gte(sel),
        Op::Lt => Expr::expr(col).lt(sel),
        Op::Lte => Expr::expr(col).lte(sel),
        Op::Like | Op::IsNull => unreachable!("checked above"),
    });
}
```

（`ExprTrait` 的 `eq/ne/gt/gte/lt/lte/in_subquery` 均接受 `R: Into<Expr>`，
`SelectStatement: Into<Expr>`——比较 op 渲染为 `col = (SELECT ...)` 标量子查询。）

Exists 分支：

```rust
CondTree::Exists(sub) => {
    validate_nested(sub, depth + 1, "exists")?;
    Ok(Expr::exists(build_select_stmt(sub, reg, depth + 1)?))
}
```

`guard_req` 递归：主表/join 表守卫之后，遍历条件树内所有 subquery/exists 的嵌套 req
递归 `guard_req`（抽小函数 `guard_nested(state, tree)` 与 cond_expr 同构遍历）。

`bootstrap.js`：

```js
    // Internal: expose req for subquery/union/cte embedding (not documented API).
    api.__req = req;
```

模块级 helper（queryBuilder 外）：

```js
  function unwrapSub(v) {
    return v && v.__req ? JSON.parse(JSON.stringify(v.__req)) : v;
  }
  function unwrapTree(t) {
    if (!t || typeof t !== "object" || Array.isArray(t)) return t;
    if (t.__req) return unwrapTree(unwrapSub(t));
    const o = {};
    for (const k of Object.keys(t)) {
      if (k === "and" || k === "or") o[k] = t[k].map(unwrapTree);
      else if (k === "not" || k === "subquery" || k === "exists") o[k] = unwrapTree(unwrapSub(t[k]));
      else o[k] = t[k];
    }
    return o;
  }
```

where/having 的入参统一过 `unwrapTree`（条件对象 `.tree()` 路径不变——condObj 的
tree 已是纯 JSON；where 接收到的普通对象里可能嵌 builder）。

- [ ] **Step 4: 跑测试确认通过 + 旧回归 + clippy**

Run: `cargo test --release query:: 2>&1 | tail -3 && cargo clippy --release --all-targets -- -D warnings 2>&1 | tail -2`
Expected: 全 PASS，clippy 零警告。

- [ ] **Step 5: Commit**

```bash
git add src/bridge/query.rs src/bridge/bootstrap.js
git commit -m "feat(query): where 子查询（in/标量比较）+ exists + 嵌套 select 地基（REQ_NEST_MAX=4）

unix@vip.qq.com ai"
```

### Task 16：UNION / UNION ALL

**Files:**
- Modify: `src/bridge/query.rs`（UnionKind/UnionArm + select 构造 + validate_verb/validate_nested
  扩展）、`src/bridge/bootstrap.js`（`.union()`）
- Test: `src/bridge/query.rs` 测试模块

**Interfaces:**
- Consumes: Task 15 的 `build_select_stmt`/`validate_nested`/`unwrapSub`。
- Produces:
  - `struct UnionArm { kind: UnionKind(all|distinct，默认 distinct), query: Box<QueryReq> }`
  - `QueryReq.unions: Vec<UnionArm>`（serde default）
  - JS `.union(otherBuilder, kind?)`

- [ ] **Step 1: 写失败测试**

```rust
#[tokio::test(flavor = "current_thread")]
async fn union_all_and_distinct() {
    let b = seeded_bridge_2t().await;
    // a.name {x,y} ∪ b.label {L1,L2,L3}：all=5 行，distinct=5 行（无重叠）
    let cap = b.run(r#"db.table("a").select(["name"])
        .union(db.table("b").select(["label"]), "all").all()
        .then(r=>json.ok({n:r.length})).catch(e=>json.fail(400,String(e)));"#).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["n"], 5, "{v}");
    let cap = b.run(r#"db.table("a").select(["name"])
        .union(db.table("b").select(["label"]).where({field:"aid",op:"eq",value:1})).all()
        .then(r=>json.ok({n:r.length})).catch(e=>json.fail(400,String(e)));"#).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["n"], 4, "{v}");   // 2 + 2(L1,L2)
    // toSQL 含 UNION ALL
    let cap = b.run(r#"db.table("a").select(["name"])
        .union(db.table("b").select(["label"]), "all").toSQL()
        .then(r=>json.ok({sql:r.sql})).catch(e=>json.fail(400,String(e)));"#).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert!(v["data"]["sql"].as_str().unwrap().contains("UNION ALL"), "{v}");
}

#[tokio::test(flavor = "current_thread")]
async fn union_rejections() {
    let b = seeded_bridge_2t().await;
    for (js, want) in [
        // 列数不一致
        (r#"db.table("a").select(["id","name"]).union(db.table("b").select(["label"])).all()"#,
         "union column count mismatch"),
        // 成员带 limit
        (r#"db.table("a").select(["name"]).union(db.table("b").select(["label"]).limit(1)).all()"#,
         "union member does not accept order_by/limit/offset"),
        // 基查询隐式列（union 必须显式 columns）
        (r#"db.table("a").union(db.table("b").select(["label"])).all()"#,
         "union requires explicit columns"),
        // union 套 union（嵌套禁 unions）
        (r#"db.table("a").select(["name"]).union(db.table("b").select(["label"])
             .union(db.table("b").select(["label"]))).all()"#,
         "nested select does not accept with/unions"),
        // insert 带 unions（动词矩阵）
        (r#"db.table("a").insert({name:"z"}).union(db.table("b").select(["label"])).run()"#,
         "insert does not accept unions"),
    ] {
        let cap = b.run(&format!(r#"{js}.then(()=>json.ok({{}})).catch(e=>json.fail(400,String(e)));"#)).await.unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(v["msg"].as_str().unwrap().contains(want), "{want}: {v}");
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --release union_ 2>&1 | tail -3`
Expected: FAIL（`.union is not a function`）。

- [ ] **Step 3: 最小实现**

```rust
/// union 种类（Intersect/Except 不做：mysql 旧版本不支持且无用例）。
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum UnionKind {
    #[default]
    Distinct,
    All,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct UnionArm {
    #[serde(default)]
    kind: UnionKind,
    query: Box<QueryReq>,
}
```

`QueryReq` 增 `#[serde(default)] unions: Vec<UnionArm>`。
`validate_verb`：Insert/Update/Delete 各加 `unions` 拒绝（`"insert does not accept unions"`
等，沿用既有 reject 风格）；`validate_nested` 补 `!req.unions.is_empty()` →
`"<site>: nested select does not accept with/unions (v1)"`。

`build_select_stmt` 内（limit 之后、返回之前）：

```rust
    if !req.unions.is_empty() {
        if req.columns.is_empty() {
            return Err(JsErrorBox::generic("union requires explicit columns"));
        }
        for arm in &req.unions {
            validate_nested(&arm.query, depth + 1, "union")?;
            let m = &arm.query;
            if m.columns.is_empty() {
                return Err(JsErrorBox::generic("union requires explicit columns"));
            }
            if m.columns.len() != req.columns.len() {
                return Err(JsErrorBox::generic(format!(
                    "union column count mismatch: {} vs {}",
                    req.columns.len(),
                    m.columns.len()
                )));
            }
            if !m.order_by.is_empty() || m.limit.is_some() || m.offset.is_some() {
                return Err(JsErrorBox::generic(
                    "union member does not accept order_by/limit/offset",
                ));
            }
            let member = build_select_stmt(m, reg, depth + 1)?;
            let ty = match arm.kind {
                UnionKind::All => sea_query::UnionType::All,
                UnionKind::Distinct => sea_query::UnionType::Distinct,
            };
            q.union(ty, member);
        }
    }
```

`guard_req`：对每个 `arm.query` 递归 `guard_req`。

`bootstrap.js`：

```js
    union(other, kind) { req.unions.push({ kind: kind ? String(kind) : "distinct", query: unwrapSub(other) }); return api; },
```

req 初始值加 `unions: []`。

- [ ] **Step 4: 跑测试确认通过 + 旧回归 + clippy**

Run: `cargo test --release query:: 2>&1 | tail -3 && cargo clippy --release --all-targets -- -D warnings 2>&1 | tail -2`
Expected: 全 PASS，clippy 零警告。

- [ ] **Step 5: Commit**

```bash
git add src/bridge/query.rs src/bridge/bootstrap.js
git commit -m "feat(query): union/union all（显式列 + 列数校验 + 成员禁排序分页，嵌套禁 unions）

unix@vip.qq.com ai"
```

### Task 17：CASE 列 + 窗口函数列

**Files:**
- Modify: `src/bridge/query.rs`（ColSpec 增 Case/Window 变体 + select 列构造 + 动词矩阵
  扩展）、`src/bridge/bootstrap.js`（无新增链方法——columns 数组元素直写对象，零改动）
- Test: `src/bridge/query.rs` 测试模块

**Interfaces:**
- Consumes: Task 10 的 `ColSpec`（untagged String 优先）/`check_alias`/agg_aliases 账本、
  Task 15 的 `cond_expr(reg, depth)`。
- Produces:
  - `ColSpec::Case(CaseSpec)`：`{case:{when:[{cond:CondTree, then:Value}], else:Value 可省},
    as:String}`（`#[serde(rename_all)]` 不需要；`else` 用 `#[serde(default,
    rename = "else")] r#else: Option<Value>`）
  - `ColSpec::Window(WindowSpec)`：`{window:{fn:WinFn(row_number|rank|dense_rank),
    partition_by:[String] 默认 [], order_by:[OrderBy] 默认 []}, as:String}`
  - case/window 别名**不进** agg_aliases；having/groupBy 引用 → Err

- [ ] **Step 1: 写失败测试**

```rust
#[tokio::test(flavor = "current_thread")]
async fn case_and_window_columns() {
    let b = seeded_bridge_2t().await;
    // case：id=1 → "one"，否则 "other"
    let cap = b.run(r#"db.table("a").select(["name",
        {case:{when:[{cond:{field:"id",op:"eq",value:1},then:"one"}],else:"other"},as:"tag"}
      ]).orderBy([{field:"id",dir:"asc"}]).all()
      .then(r=>json.ok({tags:r.map(x=>x.tag)})).catch(e=>json.fail(400,String(e)));"#).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["tags"], json!(["one", "other"]), "{v}");
    // window：row_number over (order by id desc) → y=1, x=2
    let cap = b.run(r#"db.table("a").select(["name",
        {window:{fn:"row_number",order_by:[{field:"id",dir:"desc"}]},as:"rn"}
      ]).orderBy([{field:"rn"...}])"#);   // 占位——见下行真实断言
    let cap = b.run(r#"db.table("a").select(["name",
        {window:{fn:"row_number",order_by:[{field:"id",dir:"desc"}]},as:"rn"}
      ]).all()
      .then(r=>json.ok({rows:r})).catch(e=>json.fail(400,String(e)));"#).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    let rows = v["data"]["rows"].as_array().unwrap();
    let rn_of = |n: &str| rows.iter().find(|r| r["name"] == n).unwrap()["rn"].as_i64().unwrap();
    assert_eq!(rn_of("y"), 1, "{v}");
    assert_eq!(rn_of("x"), 2, "{v}");
    // partition_by：b 表按 aid 分区编号
    let cap = b.run(r#"db.table("b").select(["label",
        {window:{fn:"rank",partition_by:["aid"],order_by:[{field:"id",dir:"asc"}]},as:"rk"}
      ]).all()
      .then(r=>json.ok({rows:r})).catch(e=>json.fail(400,String(e)));"#).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["code"], 0, "{v}");
}

#[tokio::test(flavor = "current_thread")]
async fn case_window_rejections() {
    let b = seeded_bridge_2t().await;
    for (js, want) in [
        // 别名形状非法
        (r#"db.table("a").select([{case:{when:[{cond:{field:"id",op:"eq",value:1},then:1}],as:"0bad"}}]).all()"#,
         "invalid alias"),
        // window fn 非枚举值
        (r#"db.table("a").select([{window:{fn:"ntile",order_by:[]},as:"x"}]).all()"#,
         "unknown variant"),
        // having 引用 window 别名
        (r#"db.table("a").select([{window:{fn:"row_number"},as:"rn"}]).groupBy(["id"])
             .having({field:"rn",op:"eq",value:1}).all()"#,
         "unknown column"),
        // insert 带 case 列（动词矩阵：columns 非 select 拒绝——若矩阵已拦 columns 整体，
        // 此条断言以实际报错为准调整 want）
        (r#"db.table("a").insert({name:"z"}).run && db.table("a").select([{case:{when:[],as:"x"}}]).all()"#,
         "case needs non-empty when"),
    ] {
        let cap = b.run(&format!(r#"{js}.then(()=>json.ok({{}})).catch(e=>json.fail(400,String(e)));"#)).await.unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(v["msg"].as_str().unwrap().contains(want), "{want}: {v}");
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --release case_ 2>&1 | tail -3; cargo test --release window 2>&1 | tail -3`
Expected: FAIL（列对象不识别 → untagged 落 String 变体报 unknown column）。

- [ ] **Step 3: 最小实现**

```rust
/// case 列（searched case only；then/else 只允许 JSON 值，走绑定参数）。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct CaseSpec {
    when: Vec<CaseWhen>,
    #[serde(default, rename = "else")]
    r#else: Option<Value>,
    #[serde(rename = "as")]
    r#as: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct CaseWhen {
    cond: CondTree,
    then: Value,
}

/// 窗口函数（frame 不做）。
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WinFn {
    RowNumber,
    Rank,
    DenseRank,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct WindowSpec {
    #[serde(rename = "fn")]
    r#fn: WinFn,
    #[serde(default)]
    partition_by: Vec<String>,
    #[serde(default)]
    order_by: Vec<OrderBy>,
    #[serde(rename = "as")]
    r#as: String,
}
```

`ColSpec` untagged 枚举按序追加变体（String → Agg → Case → Window，键互不重叠）。
select 列构造处增两臂：

```rust
ColSpec::Case(c) => {
    if c.when.is_empty() {
        return Err(JsErrorBox::generic("case needs non-empty when"));
    }
    check_alias(&c.r#as)?;
    let mut case = sea_query::CaseStatement::new();
    for w in &c.when {
        let cond = cond_expr(&w.cond, ctx, reg, depth)?;   // 复用 where 条件编译
        case = case.case(sea_query::Condition::all().add(cond), to_qv(&w.then));
    }
    if let Some(e) = &c.r#else {
        case = case.finally(to_qv(e));
    }
    q.expr_as(case, Alias::new(&c.r#as));
}
ColSpec::Window(w) => {
    check_alias(&w.r#as)?;
    let name = match w.r#fn {
        WinFn::RowNumber => "ROW_NUMBER",
        WinFn::Rank => "RANK",
        WinFn::DenseRank => "DENSE_RANK",
    };
    let mut win = sea_query::WindowStatement::new();
    for c in &w.partition_by {
        ctx.check_col(c, "window partition_by")?;
        win.add_partition_by(col_simple_expr(c));
    }
    for o in &w.order_by {
        ctx.check_col(&o.field, "window order_by")?;
        win.order_by_columns([(Alias::new(&o.field), parse_order_dir(o)?)]);
    }
    q.expr_window_as(sea_query::Func::cust(Alias::new(name)), win.take(), Alias::new(&w.r#as));
}
```

（`to_qv`/`parse_order_dir` 用既有 helper 名——若现有代码把 JSON→sea-query Value 的转换
与 orderBy dir 解析叫别的名字，沿用现有的；`case.case()` 的 `C: IntoCondition` 由
`Condition::all().add(simple)` 满足。agg_aliases 账本**不登记** case/window 别名；
having 别名展开查不到 → 自然落 `unknown column`。）

动词矩阵：case/window 是 `columns` 元素级能力，DML 本就不接受 columns 语义外的输入——
若 validate_verb 目前不拦 select 以外的 columns，补：`verb != Select &&
!req.columns.is_empty()` → Err。

`bootstrap.js` 零改动（columns 数组元素本就透传对象）。

- [ ] **Step 4: 跑测试确认通过 + 旧回归 + clippy**

Run: `cargo test --release query:: 2>&1 | tail -3 && cargo clippy --release --all-targets -- -D warnings 2>&1 | tail -2`
Expected: 全 PASS，clippy 零警告。

- [ ] **Step 5: Commit**

```bash
git add src/bridge/query.rs
git commit -m "feat(query): case 列 + 窗口函数列（row_number/rank/dense_rank，别名不过 having 账本）

unix@vip.qq.com ai"
```

### Task 18：CTE（非递归 WITH）

**Files:**
- Modify: `src/bridge/query.rs`（CteReq + WithQuery 装配 + 表解析支持虚拟表 + guard_req
  跳过 CTE 名）、`src/bridge/bootstrap.js`（`.with()`）
- Test: `src/bridge/query.rs` 测试模块

**Interfaces:**
- Consumes: Task 15 的 `build_select_stmt`/`validate_nested`/`unwrapSub`、Task 8 的 ColCtx。
- Produces:
  - `struct CteReq { name: String, columns: Vec<String>, query: Box<QueryReq> }`
    （deny_unknown_fields）
  - `QueryReq.with: Vec<CteReq>`（serde default）
  - ColCtx 扩 `ctes: Vec<(&str, &[String])>`；限定列解析顺序：基表 → join 表 → CTE 名
  - JS `.with(name, columns, builder)`

- [ ] **Step 1: 写失败测试**

```rust
#[tokio::test(flavor = "current_thread")]
async fn cte_main_and_join() {
    let b = seeded_bridge_2t().await;
    // 主表 = CTE：r(aid,label) = b 中 aid=1 → 2 行
    let cap = b.run(r#"db.table("r").with("r", ["aid","label"],
        db.table("b").select(["aid","label"]).where({field:"aid",op:"eq",value:1}))
      .select(["aid","label"]).orderBy([{field:"aid",dir:"asc"}]).all()
      .then(r=>json.ok({n:r.length,first:r[0].label})).catch(e=>json.fail(400,String(e)));"#).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["n"], 2, "{v}");
    assert_eq!(v["data"]["first"], "L1", "{v}");
    // join CTE：a ⋈ r on a.id = r.aid → x × {L1,L2} = 2 行
    let cap = b.run(r#"db.table("a").with("r", ["aid","label"],
        db.table("b").select(["aid","label"]).where({field:"aid",op:"eq",value:1}))
      .join("r", [{left:"a.id",right:"r.aid"}]).select(["a.name","r.label"]).all()
      .then(r=>json.ok({n:r.length})).catch(e=>json.fail(400,String(e)));"#).await.unwrap();
    let v: Value = serde_json::from_slice(&cap.body).unwrap();
    assert_eq!(v["data"]["n"], 2, "{v}");
}

#[tokio::test(flavor = "current_thread")]
async fn cte_rejections() {
    let b = seeded_bridge_2t().await;
    for (js, want) in [
        // name 形状非法
        (r#"db.table("r").with("0bad", ["aid"], db.table("b").select(["aid"])).select(["aid"]).all()"#,
         "invalid alias"),
        // columns 空
        (r#"db.table("r").with("r", [], db.table("b").select(["aid"])).select(["aid"]).all()"#,
         "cte needs non-empty columns"),
        // 引用未声明的 CTE 列
        (r#"db.table("r").with("r", ["aid"], db.table("b").select(["aid"])).select(["r.nope"]).all()"#,
         "unknown column"),
        // 嵌套 req 带 with
        (r#"db.table("a").select(["id"]).where({field:"id",op:"in",
             subquery:{table:"b",columns:["aid"],with:[{name:"x",columns:["aid"],
               query:{table:"b",columns:["aid"]}}]}}).all()"#,
         "nested select does not accept with/unions"),
        // insert 带 with（动词矩阵）
        (r#"db.table("a").insert({name:"z"}).with("r",["aid"],db.table("b").select(["aid"])).run()"#,
         "insert does not accept with"),
    ] {
        let cap = b.run(&format!(r#"{js}.then(()=>json.ok({{}})).catch(e=>json.fail(400,String(e)));"#)).await.unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(v["msg"].as_str().unwrap().contains(want), "{want}: {v}");
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --release cte_ 2>&1 | tail -3`
Expected: FAIL（`.with is not a function`）。

- [ ] **Step 3: 最小实现**

```rust
/// CTE（非递归；columns 必填——CTE 输出列即后续解析的白名单）。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct CteReq {
    name: String,
    columns: Vec<String>,
    query: Box<QueryReq>,
}
```

`QueryReq` 增 `#[serde(default)] with: Vec<CteReq>`。
`validate_verb`：Insert/Update/Delete 各加 `with` 拒绝；`validate_nested` 补
`!req.with.is_empty()` 判断（与 unions 同一报错文案）。

表解析虚拟化：`build_select_stmt` 开头在 join_defs 之后构建 cte 表集合，ColCtx 增字段：

```rust
// ColCtx 增：ctes: Vec<(&'a str, &'a [String])>——(cte 名, 声明列)
// check_col 限定列解析顺序：base_name → joins → ctes；CTE 命中按声明列校验。
```

基表来源：`req.table` 命中 CTE 名时，`base` 不再从 `reg.get` 取——引入轻量枚举：

```rust
/// 表来源：真实表（registry）或 CTE 虚拟表（声明列）。
enum TableSrc<'a> {
    Real(&'a TableDef),
    Cte(&'a [String]),
}

impl TableSrc<'_> {
    fn has_column(&self, col: &str) -> bool {
        match self {
            TableSrc::Real(t) => t.has_column(col),
            TableSrc::Cte(cols) => cols.iter().any(|c| c == col),
        }
    }
}
```

`ColCtx.base: TableDef` 引用处改 `TableSrc`（join 表的 join_defs 同步允许 CTE 名——
`join()` 的表参数本就 `Alias::new`，无需改；`q.from(Alias::new(&req.table))` 对 CTE 名
同样成立）。

`guard_req`：主表与 join 表**命中 CTE 名则跳过** `check_table`（非真实表，无 owner），
其余照常；每个 `cte.query` 递归 `guard_req`。

`build_select_stmt` 返回前装配（with 非空时改走 WithQuery）：

```rust
    if req.with.is_empty() {
        return Ok(q);   // 原路径
    }
    let mut clause = sea_query::WithClause::new();
    for c in &req.with {
        check_alias(&c.name)?;
        if c.columns.is_empty() {
            return Err(JsErrorBox::generic("cte needs non-empty columns"));
        }
        for col in &c.columns {
            check_alias(col)?;
        }
        validate_nested(&c.query, depth + 1, "cte")?;
        let mut cte = sea_query::CommonTableExpression::new();
        cte.table_name(Alias::new(&c.name));
        cte.columns(c.columns.iter().map(Alias::new));
        cte.query(build_select_stmt(&c.query, reg, depth + 1)?);
        clause.cte(cte);
    }
    Ok(q.with(clause))   // 注意返回类型变化：见下
}
```

返回类型处理：`SelectStatement::with` 消费 self 返回 `WithQuery`——`build_select_stmt`
签名改为返回 `sea_query::QueryStatementBuilder` 无法直接做枚举，定义：

```rust
/// 顶层可 build 的 select（含可选 WITH 包装）。
enum TopSelect {
    Plain(SelectStatement),
    With(Box<sea_query::WithQuery>),
}
```

`build_select_stmt` 保持返回 `SelectStatement`（嵌套嵌入只需要它）；**顶层** with 装配
挪到 `build_statement` 的 Select 臂：`let stmt = build_select_stmt(req, reg, 0)?;`
→ with 非空则 `TopSelect::With(Box::new(stmt.with(clause)))` 再分别 `.build(builder)`。
即：with 的校验与 clause 构建函数 `build_with_clause(req, reg) -> Result<WithClause>`
放 `build_statement` 侧调用，`build_select_stmt` 内不做 with 判断。

`bootstrap.js`：

```js
    with(name, columns, query) { req.with.push({ name: String(name), columns: (columns || []).map(String), query: unwrapSub(query) }); return api; },
```

req 初始值加 `with: []`。

- [ ] **Step 4: 跑测试确认通过 + 旧回归 + clippy**

Run: `cargo test --release query:: 2>&1 | tail -3 && cargo clippy --release --all-targets -- -D warnings 2>&1 | tail -2`
Expected: 全 PASS，clippy 零警告。

- [ ] **Step 5: Commit**

```bash
git add src/bridge/query.rs src/bridge/bootstrap.js
git commit -m "feat(query): 非递归 CTE（with，虚拟表列白名单，CTE 名跳过归属守卫）

unix@vip.qq.com ai"
```

### Task 19：Phase 8 文档 + 版本 0.1.14 + 全量门禁

**Files:**
- Modify: `docs/devkit/api-manual.md`（`db.table` 子节补：子查询/exists、union、case/
  window 列、with CTE——各一段示例 + 约束一句；嵌套通用约束一节：REQ_NEST_MAX=4、
  嵌套禁 with/unions、嵌套必须 select）
- Modify: `oj/Cargo.toml`（version 0.1.13 → 0.1.14）

- [ ] **Step 1: 文档更新**

api-manual.md `db.table` 子节（Task 14 已扩）末尾追加 Phase 8 五块：示例用 spec
「追加：Phase 8」节的 JSON 形态（链式示例以 bootstrap.js 实有方法为准：`.union()`/
`.with()`/columns 对象元素/where 子查询）。

- [ ] **Step 2: 版本 bump**

`oj/Cargo.toml`：`version = "0.1.13"` → `version = "0.1.14"`。

- [ ] **Step 3: 全量门禁**

Run:
```bash
cargo fmt --check 2>&1 | tail -2
cargo clippy --release --all-targets -- -D warnings 2>&1 | tail -2
cargo test --release --workspace 2>&1 | grep -E "test result|error" | tail -10
cargo build --workspace 2>&1 | tail -2
cargo xtask smoke --bin bin/oj 2>&1 | tail -2
LC_ALL=C grep -P '[^\x00-\x7F]' src/bridge/bootstrap.js
```
Expected: fmt/clippy 干净，workspace 测试全绿，构建归置 bin/，smoke 过，ASCII grep 无输出。

- [ ] **Step 4: Commit**

```bash
git add docs/devkit/api-manual.md oj/Cargo.toml Cargo.lock
git commit -m "feat(v0.1.14): sea-query 扩展面（子查询/union/case/window/cte）+ 版本 0.1.14

unix@vip.qq.com ai"
```

### Phase 8 收尾：更新与总结

- [ ] 勾掉 Phase 8 checkbox；`git log --oneline` 核对；总结「sea-query 开放能力以安全 DSL
  暴露完毕，v0.1.14 收口」。

---

## 全部阶段完成后：统一审查

- [ ] 派发统一审查（对照 spec 逐节核对实现 + 红线复核 + 测试覆盖核对），按审查意见修复后收尾。
