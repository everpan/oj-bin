# tenant.sql_guard 多租户 SQL 防护 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `tenant.sql_guard` 开关启用后，构造器 SQL 自动注入 tenant_id 条件、裸 SQL 门禁式软防、schema.yaml 声明期强制 tenant_id 列，防止跨租户逃逸。

**Architecture:** 注入走 op 层 QueryReq 预变换（不改 `build_statement` 纯函数一族签名）；开关经 StableState 冻结（照抄 ownership_deny 模式）；逃生口为请求级 `db.asSystem()`（ReqState.system 标志）；裸 SQL 用「剥单引号字符串+注释」清洗后做字面/参数检查，fail-closed。

**Tech Stack:** Rust / deno_core op2 / sea-query / serde_yaml。测试一律 `cargo test --release`（CLAUDE.md 红线：禁止 debug）。

**Spec:** `docs/superpowers/specs/2026-09-13-tenant-sql-guard-design.md`（含三方评审修订）

## Global Constraints

- 禁止 debug 构建 / 裸 `cargo test` / 裸 `cargo clippy`：一律 `--release`。
- `bootstrap.js` 必须保持 7-bit ASCII。
- 动态标识符只来自 SchemaRegistry；值只走绑定参数（红线）。
- 所有插件 profile 保持 `panic = "unwind"`。
- 每次版本更新需更新 CHANGELIST.md。
- 新增 JsRuntime 入口才需要 patch_fs_loaded_sources——本计划不涉及。

---

### Task 1: 配置层 —— TenantCfg.sql_guard 三态

**Files:**
- Modify: `src/config.rs:226-243`（TenantCfg）、`src/bridge/mod.rs`（新增 SqlGuard 枚举，放 StableState 定义旁）

**Interfaces:**
- Produces: `pub enum SqlGuard { Off, Warn, Deny }`（`crate::bridge::SqlGuard`，Copy/PartialEq/Default=Off）；`TenantCfg.sql_guard: SqlGuard`；`TenantCfg` 不变动其他字段。

- [ ] **Step 1: SqlGuard 枚举 + Deserialize（config.rs 内 impl，同 crate 无孤儿问题）**

`src/bridge/mod.rs`（StableState 前）：

```rust
/// 多租户 SQL 防护模式（tenant.sql_guard；装配期冻结进 StableState，首 run 前不可变）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SqlGuard {
    /// 不启用（默认）。
    #[default]
    Off,
    /// 仅告警（软过渡：注入照做、缺失不拒绝）。
    Warn,
    /// 缺租户条件即拒绝（fail-closed）。
    Deny,
}
```

`src/config.rs`：

```rust
use crate::bridge::SqlGuard;

impl<'de> serde::Deserialize<'de> for SqlGuard {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        match serde_yaml::Value::deserialize(d)? {
            serde_yaml::Value::Bool(false) => Ok(SqlGuard::Off),
            serde_yaml::Value::Bool(true) => Ok(SqlGuard::Deny),
            serde_yaml::Value::String(s) => match s.as_str() {
                "off" | "false" => Ok(SqlGuard::Off),
                "warn" => Ok(SqlGuard::Warn),
                "deny" => Ok(SqlGuard::Deny),
                other => Err(Error::custom(format!(
                    "tenant.sql_guard: illegal value {other:?} (true|false|warn|deny)"
                ))),
            },
            _ => Err(Error::custom("tenant.sql_guard: expected bool or string")),
        }
    }
}
```

TenantCfg 增字段：

```rust
    /// 多租户 SQL 防护（默认 off=关；true=deny 缺租户条件即拒；"warn"=仅告警软过渡）。
    #[serde(default)]
    pub sql_guard: crate::bridge::SqlGuard,
```

- [ ] **Step 2: 失败测试**（config.rs tests 追加）

```rust
#[test]
fn tenant_sql_guard_parse() {
    let c: Config = serde_yaml::from_str("tenant:\n  enable: true\n  sql_guard: true\n").unwrap();
    assert_eq!(c.tenant.sql_guard, crate::bridge::SqlGuard::Deny);
    let c: Config = serde_yaml::from_str("tenant:\n  sql_guard: warn\n").unwrap();
    assert_eq!(c.tenant.sql_guard, crate::bridge::SqlGuard::Warn);
    let c: Config = serde_yaml::from_str("tenant: {}\n").unwrap();
    assert_eq!(c.tenant.sql_guard, crate::bridge::SqlGuard::Off);
    assert!(serde_yaml::from_str::<Config>("tenant:\n  sql_guard: bogus\n").is_err());
}
```

- [ ] **Step 3: 跑测试** `cargo test --release -p only-js tenant_sql_guard` → PASS
- [ ] **Step 4: Commit** `feat(tenant): sql_guard 三态配置开关`

---

### Task 2: Bridge 管线 —— StableState/Extras/ReqState/TableDef 字段

**Files:**
- Modify: `src/bridge/mod.rs:104-141`（StableState）、`:143-167`（Extras）、`172-200`（ReqState + reset）、`:510-535`（StableState::new 组装）、`:1580-1600`（test 组装点）
- Modify: `src/bridge/registry.rs:16-23`（TableDef）、`:50-86`（declare/table_owned）

**Interfaces:**
- Produces: `StableState.sql_guard: SqlGuard`（ownership_deny 旁）；`Extras.sql_guard: SqlGuard`（Default=Off）；`ReqState.system: bool`（reset 置 false）；`TableDef.shared: bool` + `SchemaRegistry::table_owned_shared(owner,name,pk,cols,shared)`（table_owned 委托 shared=false）；`TableDef::is_tenant_scoped()` = `!shared && has_column("tenant_id")`。

- [ ] **Step 1: 加字段**（各结构体照相邻字段模式）

```rust
// StableState（ownership_deny 后）
    /// 多租户 SQL 防护模式（tenant.sql_guard；Off=不设防）。
    pub sql_guard: SqlGuard,
// Extras（ownership_deny 后）
    /// 多租户 SQL 防护模式（缺省 Off）。
    pub sql_guard: SqlGuard,
// ReqState
    /// db.asSystem() 逃生口：本请求以系统身份绕过租户防护（sql_guard 活跃时记日志）。
    pub system: bool,
// ReqState::reset 末尾
        self.system = false;
// TableDef
    /// 共享表标记（schema.yaml `tenant: false` 且过 config 白名单；默认 false=受租户约束）。
    pub shared: bool,
// registry.rs
impl SchemaRegistry {
    /// 带 shared 标记的声明（sql_guard 装配路径）。
    pub fn table_owned_shared(mut self, owner: &str, name: &str, pk: &[&str], columns: &[&str], shared: bool) -> Self {
        self.declare_shared(Some(owner.to_string()), name, pk, columns, shared);
        self
    }
}
impl TableDef {
    /// 受租户约束（注入候选）：非共享表且含 tenant_id 列。
    pub fn is_tenant_scoped(&self) -> bool {
        !self.shared && self.has_column("tenant_id")
    }
}
```
`declare` 拆出 `declare_shared(..., shared: bool)`，原 `declare` 委托 `declare_shared(..., false)`。

- [ ] **Step 2: 组装点接线**（mod.rs StableState::new 里 `ownership_deny: extras.ownership_deny,` 旁加 `sql_guard: extras.sql_guard,`；test 组装点 `:1589` 附近 `ownership_deny: false,` 旁加 `sql_guard: SqlGuard::Off,`）

- [ ] **Step 3: 编译** `cargo build --release` → 过
- [ ] **Step 4: registry 单测**（registry.rs tests 追加 shared 标记断言）+ `cargo test --release -p only-js registry` → PASS
- [ ] **Step 5: Commit** `feat(bridge): sql_guard 状态管线与 shared 表标记`

---

### Task 3: query.rs —— apply_tenant 注入 + asSystem op

**Files:**
- Modify: `src/bridge/query.rs`（Join 结构 :140、build_select_stmt join 分支、两个 op、tests）
- Modify: `src/bridge/db.rs`（op_db_as_system）
- Modify: `src/bridge/mod.rs`（extension op 注册表加 op_db_as_system）
- Modify: `src/bridge/bootstrap.js`（DB 实例与 tx 实例加 asSystem；ops import 加 op_db_as_system）

**Interfaces:**
- Consumes: Task 2 的 `StableState.sql_guard` / `ReqState.system` / `TableDef::is_tenant_scoped`。
- Produces: `Join.tenant_id: Option<String>`（serde default；仅 apply_tenant 填充）；JS `db.asSystem()` / `DB("x").asSystem()` / tx `asSystem()`（返回原实例，请求级生效）。

- [ ] **Step 1: 失败测试**（query.rs tests；夹具：SqlxAccessor sqlite::memory:，表 `t(id,name,tenant_id)` + `s(id,name)`（shared），种子两行不同租户）

```rust
fn guarded_bridge() -> Bridge { /* with_dbs_and_loader + Extras{sql_guard: SqlGuard::Deny, ..} */
    // 注册表：table("t",pk id,cols id/name/tenant_id)；table_owned_shared("m","s",...,true)
}
#[tokio::test(flavor = "current_thread")]
async fn tenant_select_scoped() { /* run_with tenant_id=t1 → 只回 t1 行；toSQL 含 tenant_id */ }
#[tokio::test(flavor = "current_thread")]
async fn tenant_insert_forces_tid_and_rejects_mismatch() { ... }
#[tokio::test(flavor = "current_thread")]
async fn tenant_update_sets_tid_rejected_and_where_narrowed() { ... }
#[tokio::test(flavor = "current_thread")]
async fn tenant_join_injects_on_clause() { /* left join：他租户 join 行 label 为 NULL */ }
#[tokio::test(flavor = "current_thread")]
async fn tenant_subquery_recursion() { /* in-subquery 只回 t1 */ }
#[tokio::test(flavor = "current_thread")]
async fn tenant_none_denied_and_as_system_escapes() { /* None+Deny → 错；asSystem → 过 */ }
#[tokio::test(flavor = "current_thread")]
async fn tenant_shared_table_untouched() { /* s 表不过滤 */ }
```
先跑确认全部 FAIL（apply_tenant 不存在）。

- [ ] **Step 2: apply_tenant（query.rs，guard_req 之后）**

```rust
/// 多租户防护预变换（guard_req 之后、build_statement 之前；两构造器 op 共用）。
/// 形状递归照抄 guard_req：joins/条件树子查询/exists/union 臂/cte/case-when 全覆盖。
/// tid=None：Deny 对受约束表直接 Err；Warn 告警放行（软过渡）。system=请求级逃生口。
fn apply_tenant(
    req: &mut QueryReq,
    reg: &SchemaRegistry,
    tid: Option<&str>,
    guard: SqlGuard,
    system: bool,
) -> Result<(), JsErrorBox> {
    if system {
        return Ok(());
    }
    let cte_name = |n: &str| req.with.iter().any(|c| c.name == n);
    let scoped = |name: &str| {
        !cte_name(name) && reg.get(name).is_some_and(|t| t.is_tenant_scoped())
    };
    // 本层受约束表集合（用于 tid=None 的判定与写侧校验）
    let mut touched = vec![];
    if scoped(&req.table) {
        touched.push(req.table.clone());
    }
    for j in &req.joins {
        if scoped(&j.table) {
            touched.push(j.table.clone());
        }
    }
    if !touched.is_empty() {
        match tid {
            Some(tid) => match req.verb {
                Verb::Select | Verb::Update | Verb::Delete => {
                    req.conditions.push(CondTree::Leaf(Cond {
                        field: "tenant_id".into(),
                        op: Op::Eq,
                        value: Some(Value::String(tid.into())),
                        subquery: None,
                    }));
                }
                Verb::Insert => {
                    for row in &mut req.values {
                        match row.get("tenant_id") {
                            Some(v) if v.as_str() != Some(tid) => {
                                return Err(JsErrorBox::generic(format!(
                                    "tenant guard: insert tenant_id mismatch (got {v}, want {tid:?})"
                                )));
                            }
                            _ => {
                                row.insert("tenant_id".into(), Value::String(tid.into()));
                            }
                        }
                    }
                }
            },
            None => {
                let msg = format!(
                    "tenant guard: table(s) {touched:?} require tenant context (missing tenant header; use db.asSystem() for system tasks)"
                );
                if guard == SqlGuard::Deny {
                    return Err(JsErrorBox::generic(msg));
                }
                eprintln!("warn: {msg}");
                return Ok(()); // Warn：整层放行（软过渡）
            }
        }
    }
    // update 写侧逃逸：sets 显式 tenant_id 必须等于当前租户
    if req.verb == Verb::Update && scoped(&req.table) {
        if let Some(v) = req.sets.get("tenant_id") {
            if tid.is_none() || v.as_str() != tid {
                return Err(JsErrorBox::generic(format!(
                    "tenant guard: update sets.tenant_id not allowed (got {v})"
                )));
            }
        }
    }
    // join 表：tenant 条件进 ON 子句（Join.tenant_id 字段，build_select_stmt 消费）——
    // LEFT JOIN 注入 WHERE 会静默变 INNER JOIN（评审 P2-9）。
    if let Some(tid) = tid {
        for j in &mut req.joins {
            if scoped(&j.table) && j.tenant_id.is_none() {
                j.tenant_id = Some(tid.to_string());
            }
        }
    }
    // 递归：条件树（含 having/case-when）内子查询 + union 臂 + cte
    for c in &mut req.conditions {
        apply_tenant_tree(c, reg, tid, guard)?;
    }
    if let Some(h) = &mut req.having {
        apply_tenant_tree(h, reg, tid, guard)?;
    }
    for col in &mut req.columns {
        if let ColSpec::Case(c) = col {
            for w in &mut c.case.when {
                apply_tenant_tree(&mut w.cond, reg, tid, guard)?;
            }
        }
    }
    for u in &mut req.unions {
        apply_tenant(&mut u.query, reg, tid, guard, false)?;
    }
    for c in &mut req.with {
        apply_tenant(&mut c.query, reg, tid, guard, false)?;
    }
    Ok(())
}

fn apply_tenant_tree(
    t: &mut CondTree,
    reg: &SchemaRegistry,
    tid: Option<&str>,
    guard: SqlGuard,
) -> Result<(), JsErrorBox> {
    match t {
        CondTree::Leaf(c) => {
            if let Some(sub) = &mut c.subquery {
                apply_tenant(sub, reg, tid, guard, false)?;
            }
            Ok(())
        }
        CondTree::And(xs) | CondTree::Or(xs) => {
            for x in xs {
                apply_tenant_tree(x, reg, tid, guard)?;
            }
            Ok(())
        }
        CondTree::Not(x) => apply_tenant_tree(x, reg, tid, guard),
        CondTree::Exists(sub) => apply_tenant(sub, reg, tid, guard, false),
    }
}
```

- [ ] **Step 3: Join 结构与 build_select_stmt 消费**

```rust
#[derive(Debug, Clone, Deserialize)]
struct Join {
    table: String,
    #[serde(default)]
    kind: JoinKind,
    on: Vec<OnPair>,
    /// 多租户防护注入（apply_tenant 填充；JS 链层不可设——build_select_stmt 消费）。
    #[serde(default)]
    tenant_id: Option<String>,
}
```
join 分支（`for j in &req.joins` 循环内、`let jt = ...` 前）：
```rust
        if let Some(tid) = &j.tenant_id {
            on = on.add(
                col_simple_expr(&format!("{}.tenant_id", j.table))
                    .eq(Expr::val(tid.clone())),
            );
        }
```

- [ ] **Step 4: 两个 op 接线**（`op_db_query_build` 与 `op_db_query_sql`，guard_req 后）

```rust
    let mut req = req;
    {
        let g = state.borrow();
        let guard = g.borrow::<Arc<StableState>>().sql_guard;
        let rs = g.borrow::<super::ReqState>();
        if guard != SqlGuard::Off {
            apply_tenant(&mut req, &reg, rs.req.tenant_id.as_deref(), guard, rs.system)?;
        }
    }
```
（op_db_query_sql 无 await、同步借用，同样可插。）

- [ ] **Step 5: asSystem op + bootstrap**

db.rs：
```rust
/// db.asSystem()：本请求以系统身份绕过租户防护（请求级，ReqState 重置即失效；
/// sql_guard 活跃时记审计日志）。逃生口语义见 tenant.sql_guard 设计文档。
#[op2(fast)]
pub fn op_db_as_system(state: &mut OpState) -> bool {
    let mut g = state.borrow_mut();
    let rs = g.borrow_mut::<super::ReqState>();
    rs.system = true;
    drop(g);
    eprintln!("warn: db.asSystem() invoked (tenant guard bypass for this request)");
    true
}
```
mod.rs extension op 列表加 `db::op_db_as_system`；bootstrap.js ops import 加
`op_db_as_system`，DB 实例与 tx 实例 api 各加：
`asSystem() { op_db_as_system(); return api; },`

- [ ] **Step 6: 跑测试** `cargo test --release -p only-js tenant_` → 全 PASS
- [ ] **Step 7: Commit** `feat(db): 构造器 tenant 注入 + db.asSystem 逃生口`

---

### Task 4: guard.rs —— 裸 SQL 租户检查（fail-closed）

**Files:**
- Modify: `src/bridge/guard.rs`（extract_tables 扩展、新增 check_tenant_raw、tests）
- Modify: `src/bridge/db.rs:284/310`（op_db_query/op_db_exec 调 check_tenant_raw）

**Interfaces:**
- Consumes: Task 2 的 `StableState.sql_guard` / `ReqState.{system,req.tenant_id}` / `TableDef::is_tenant_scoped`。
- Produces: `pub fn check_tenant_raw(state: &Rc<RefCell<OpState>>, sql: &str, params: &[Value]) -> Result<(), JsErrorBox>`。

- [ ] **Step 1: 失败测试**（guard.rs tests）

```rust
#[test]
fn strip_literals_keeps_identifiers() {
    assert!(strip_literals("select * from t where x = 'tenant_id' -- tenant_id\n").to_lowercase().contains("tenant_id") == false);
    assert!(strip_literals("select \"tenant_id\" from t").contains("\"tenant_id\""));
}
#[test]
fn extract_comma_join_and_multitable_update() {
    assert_eq!(extract_tables("select * from a, b where a.id=b.id"), ["a", "b"]);
    assert_eq!(extract_tables("update a, b set a.x=1 where a.id=b.id"), ["a", "b"]);
}
```
行为测试（经 Bridge + Extras Deny）：
- 裸 SQL 触 t 表无 tenant_id 字面 → 拒；
- `where tenant_id = ?` + params 含当前 tid → 过；params 不含 → 拒（Deny）；
- toSQL 产物回放（带方言引号）→ 过（回归守卫：防有人改回 tokens 匹配）；
- `TRUNCATE TABLE t` / `ALTER TABLE t ...` / `DROP TABLE t` → Deny 拒；
- `delete from ghost_table`（DML 关键字 + 无注册表命中）→ Deny 拒（fail-closed）；
- `select 1` → 过；
- tid=None 触 t 表 → 拒（Deny）/ 警（Warn）；
- 共享表 s → 不检查。

- [ ] **Step 2: strip_literals + extract_tables 扩展**

```rust
/// 剥单引号字符串字面量与 -- / * / 注释（替换为空格）；保留 "" / `` 引号标识符——
/// sea-query 渲染产物（toSQL 回放路径）依赖标识符引号存活（评审：tokens 直接复用
/// 会 100% 误杀）。
fn strip_literals(sql: &str) -> String {
    let b = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\'' => {
                // 跳到配对闭合（'' 双写跳过）；内容丢弃。
                i += 1;
                while i < b.len() {
                    if b[i] == b'\'' {
                        if i + 1 < b.len() && b[i + 1] == b'\'' { i += 2; } else { i += 1; break; }
                    } else { i += 1; }
                }
                out.push(' ');
            }
            b'-' if i + 1 < b.len() && b[i + 1] == b'-' => {
                while i < b.len() && b[i] != b'\n' { i += 1; }
                out.push(' ');
            }
            b'/' if i + 1 < b.len() && b[i + 1] == b'*' => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') { i += 1; }
                i = (i + 2).min(b.len());
                out.push(' ');
            }
            c => {
                // utf-8 安全：按字符边界推进
                let ch_len = utf8_len(c);
                out.push_str(&sql[i..i + ch_len]);
                i += ch_len;
            }
        }
    }
    out
}
```
`utf8_len(b: u8) -> usize` = `(b as u32).leading_ones() as usize` 的 std 等价：
`1 << (!b.leading_ones().min(4))`——用 `std::str` 边界太绕，直接：
`fn utf8_len(b: u8) -> usize { if b < 0x80 { 1 } else if b >= 0xF0 { 4 } else if b >= 0xE0 { 3 } else { 2 } }`。

extract_tables：FROM/UPDATE 后取**逗号列表**直至边界关键字
（WHERE/GROUP/ORDER/LIMIT/OFFSET/SET/ON/JOIN/LEFT/RIGHT/INNER/CROSS/UNION/HAVING/
RETURNING/VALUES/SELECT）；JOIN/INTO 保持单词。新增 TRUNCATE/ALTER/DROP 提取
（`TRUNCATE TABLE t` / `ALTER TABLE t` / `DROP TABLE t`，TABLE 关键字跳过）。

- [ ] **Step 3: check_tenant_raw**

```rust
/// 裸 SQL 租户检查（op_db_query/op_db_exec；sql_guard != Off 时调用）。
/// 只查「完全遗漏」：清洗后文本无 tenant_id 条件即视为缺失（不验证谓词位置/绑定值，
/// 评审 P1-2 的诚实降级）；Deny 额外要求 params 含当前 tenant_id。
/// fail-closed：DML/DDL 关键字命中但提取不到任何已登记表 → Deny 拒。
pub fn check_tenant_raw(
    state: &Rc<RefCell<OpState>>,
    sql: &str,
    params: &[serde_json::Value],
) -> Result<(), JsErrorBox> {
    let (guard, tid, system, tables) = {
        let g = state.borrow();
        let st = g.borrow::<Arc<StableState>>();
        let rs = g.borrow::<ReqState>();
        (st.sql_guard, rs.req.tenant_id.clone(), rs.system, st.registry.clone())
    };
    if guard == SqlGuard::Off || system {
        return Ok(());
    }
    let deny = guard == SqlGuard::Deny;
    let extracted = extract_tables(sql);
    let registered: Vec<&str> = extracted
        .iter()
        .map(|s| s.as_str())
        .filter(|t| tables.has_table(t))
        .collect();
    let first_kw = sql
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    let ddl = matches!(first_kw.as_str(), "TRUNCATE" | "ALTER" | "DROP" | "CREATE");
    let dml = matches!(first_kw.as_str(), "INSERT" | "UPDATE" | "DELETE");
    // DDL/DML fail-closed：关键字命中但没有任何已登记表被提取 → 拒（Deny）/ 警（Warn）。
    if (ddl || dml) && registered.is_empty() {
        let msg = format!("tenant guard: {first_kw} touches no registered table (cannot verify tenant scope)");
        if deny {
            return Err(JsErrorBox::generic(msg));
        }
        eprintln!("warn: {msg}");
        return Ok(());
    }
    // DDL 面：TRUNCATE/ALTER/DROP/CREATE 触碰已注册表一律拒/警（无法注入条件，无合法多租户用途）。
    if ddl && !registered.is_empty() {
        let msg = format!("tenant guard: {first_kw} on registered table(s) {registered:?} not allowed under sql_guard");
        if deny {
            return Err(JsErrorBox::generic(msg));
        }
        eprintln!("warn: {msg}");
        return Ok(());
    }
    let scoped: Vec<&str> = registered
        .iter()
        .copied()
        .filter(|t| tables.get(t).is_some_and(|d| d.is_tenant_scoped()))
        .collect();
    if scoped.is_empty() {
        return Ok(());
    }
    let cleaned = strip_literals(sql).to_ascii_lowercase();
    let mentions = cleaned.contains("tenant_id");
    let param_has = tid
        .as_deref()
        .map(|t| params.iter().any(|p| p.as_str() == Some(t)))
        .unwrap_or(false);
    let ok = match tid {
        Some(_) => mentions && (param_has || !deny),
        None => false,
    };
    if ok {
        return Ok(());
    }
    let msg = match tid {
        Some(t) if !mentions => format!(
            "tenant guard: raw sql on {scoped:?} lacks tenant_id condition (use db.table() builder)"
        ),
        Some(_) => format!(
            "tenant guard: raw sql params must include current tenant id ({t:?}) under deny mode"
        ),
        None => format!(
            "tenant guard: table(s) {scoped:?} require tenant context (missing tenant header)"
        ),
    };
    if deny {
        return Err(JsErrorBox::generic(msg));
    }
    eprintln!("warn: {msg}");
    Ok(())
}
```

- [ ] **Step 4: db.rs 接线**（op_db_query / op_db_exec 在 `guard::check_raw` 后各加
`super::guard::check_tenant_raw(&state, &sql, &params)?;`）

- [ ] **Step 5: 跑测试** `cargo test --release -p only-js`（guard + query + db 全过）
- [ ] **Step 6: Commit** `feat(db): 裸 SQL 租户检查（fail-closed + toSQL 回放守卫）`

---

### Task 5: schema.rs —— tenant 声明 + validate_tenant

**Files:**
- Modify: `oj/src/schema.rs`（TableSchema、parse、registry_tables、tests）
- Modify: `src/config.rs`（TenantCfg 增 `shared_allow: Vec<String>`）

**Interfaces:**
- Produces: `TableSchema.tenant: bool`（serde `default_true`）；`SchemaFile::validate_tenant(&self, module: &str) -> Result<(), String>`；`SchemaFile::shared_tables(&self) -> Vec<&str>`；`TenantCfg.shared_allow`（默认空 = 共享表声明不生效，fail-closed）。

- [ ] **Step 1: 失败测试**

```rust
#[test]
fn tenant_flag_defaults_true_and_parse() {
    // tenant 缺省 = true；显式 false；validate_tenant 缺列 Err、含列 Ok、tenant:false 豁免
}
```

- [ ] **Step 2: 实现**

```rust
fn default_true() -> bool {
    true
}
// TableSchema 增：
    /// 多租户：false = 共享表声明（须同时在 config tenant.shared_allow 白名单内才生效）。
    #[serde(default = "default_true")]
    pub tenant: bool,
// SchemaFile 增：
    /// sql_guard 声明期校验：tenant=true（默认）的表必须含 tenant_id 列。
    pub fn validate_tenant(&self, module: &str) -> Result<(), String> {
        for (name, t) in &self.tables {
            if t.tenant && !t.columns.contains_key("tenant_id") {
                return Err(format!(
                    "schema: [{module}] 表 {name:?} 缺 tenant_id 列（tenant.sql_guard 启用中；\
                     共享表请显式 tenant: false 并加入 config tenant.shared_allow）"
                ));
            }
        }
        Ok(())
    }

    /// 共享表声明清单（tenant: false 的表）。
    pub fn shared_tables(&self) -> Vec<&str> {
        self.tables
            .iter()
            .filter(|(_, t)| !t.tenant)
            .map(|(n, _)| n.as_str())
            .collect()
    }
// registry_tables 返回值加 tenant 标志：Vec<(&str, Vec<&str>, Vec<&str>, bool)>（末位 t.tenant）
```
config.rs TenantCfg：
```rust
    /// 共享表白名单（tenant: false 声明须在此列出才生效；空 = 共享表声明被忽略）。fail-closed。
    #[serde(default)]
    pub shared_allow: Vec<String>,
```

- [ ] **Step 3: 跑测试** `cargo test --release -p oj schema` → PASS
- [ ] **Step 4: Commit** `feat(schema): tenant 表级声明与 validate_tenant 校验`

---

### Task 6: 装配接线 —— app.rs / checks.rs / migrate_cmd.rs

**Files:**
- Modify: `oj/src/app.rs`（sql_guard_of、build_schema_and_modules、make_bridge_of、两处 StableState/Extras、enable=false warn）
- Modify: `oj/src/checks.rs`（挂 validate_tenant）
- Modify: `oj/src/migrate_cmd.rs:70`（挂 validate_tenant）
- Modify: `src/bridge/registry.rs`（table_owned_shared 已在 Task 2）

**Interfaces:**
- Consumes: Task 1-5 全部。
- Produces: `fn sql_guard_of(cfg: &Config) -> Result<SqlGuard, String>`；build_schema_and_modules 新签名
`(dir, ts, dbs, gate, guard: SqlGuard, shared_allow: &[String])`。

- [ ] **Step 1: sql_guard_of + enable 联动 warn**

```rust
/// 多租户 SQL 防护模式（tenant.sql_guard）：enable=false 而 guard 非 Off → warn
/// （防误以为已防护）；非法值已在 config 反序列化期 fail-fast。
fn sql_guard_of(cfg: &Config) -> SqlGuard {
    let g = cfg.tenant.sql_guard;
    if !cfg.tenant.enable && g != SqlGuard::Off {
        eprintln!("warn: tenant.sql_guard {:?} 生效中但 tenant.enable=false（http.tenantId 恒为 None，防护形同虚设）", g);
    }
    g
}
```

- [ ] **Step 2: build_schema_and_modules 扩展**

registry 注册处（`registry = registry.table_owned(&name, t, &pk, &cols);`）改为：
```rust
let shared = !tenant_flag && shared_allow.iter().any(|a| a == t);
registry = registry.table_owned_shared(&name, t, &pk, &cols, shared);
```
（`tenant_flag` 来自 `f.registry_tables()` 新返回位的解构。）函数入口：
```rust
if guard != SqlGuard::Off {
    f.validate_tenant(&name)?;
    for st in f.shared_tables() {
        if !shared_allow.iter().any(|a| a == st) {
            eprintln!("warn: [{name}] 共享表声明 {st:?} 未列入 tenant.shared_allow，按受租户约束处理（tenant_id 列校验将生效）");
        }
    }
}
```
（在 `if let Some(f) = SchemaFile::load(...)` 内、registry 注册前。）签名加参并更新全部调用方
（grep `build_schema_and_modules` 调用点——app.rs 与 server 装配路径）。

- [ ] **Step 3: make_bridge_of / StableState 注入**

`ownership_deny` 取值旁加 `let sql_guard = sql_guard_of(&cfg);`，Extras 构造
`ownership_deny,` 旁加 `sql_guard,`；两处 StableState 组装点同理。

- [ ] **Step 4: checks.rs 与 migrate_cmd.rs 挂点**

checks.rs：`run` 若拿不到 config 则新增参数 `guard: SqlGuard`（调用方 `oj build`
路径从 cfg 解析后传入）；对每个 load 出的 SchemaFile 调 `validate_tenant(&name)?`。
migrate_cmd.rs run_migrate：`slim` 已持 cfg——guard 活跃时 `f.validate_tenant(&name)?`
于 reconcile 前。

- [ ] **Step 5: 全量编译 + workspace 测试**
`cargo build --release --workspace` → 过；
`cargo test --release --workspace` → 全过。

- [ ] **Step 6: fmt + clippy**
`cargo fmt && cargo clippy --release --all-targets -- -D warnings` → 过

- [ ] **Step 7: Commit** `feat(assembly): sql_guard 装配接线（server/migrate/checks）`

---

### Task 7: 文档 —— 新人文档 + api-manual + CHANGELIST

**Files:**
- Create: `docs/tenant-guide.md`（白话新人向）
- Modify: `docs/devkit/api-manual.md`（db.asSystem / sql_guard 语义小节）
- Modify: `CHANGELIST.md`（Unreleased 条目）

- [ ] **Step 1:** `docs/tenant-guide.md`——白话文：什么是租户、X-TENANT-ID 从哪来、
enable 与 sql_guard 的区别、shared 表怎么声明、asSystem 什么时候用、常见报错
对照表（缺 tenant header / tenant guard: insert mismatch / raw sql lacks tenant_id
…各是什么意思、怎么修）。面向第一次用的人，不写实现细节。
- [ ] **Step 2:** api-manual 补 `db.asSystem()` 与 `tenant.sql_guard` 配置语义。
- [ ] **Step 3:** CHANGELIST.md 顶部加 Unreleased 条目。
- [ ] **Step 4: Commit** `docs(tenant): 新人文档 + api-manual + changelist`

---

### Task 8: 集中审查 + 收尾

- [ ] **Step 1:** 用 code-review agent 对 `git diff main` 做高力度审查（安全面优先：
注入绕过、guard fail-open、shared 表滥用、asSystem 扩散）。
- [ ] **Step 2:** 按审查发现修复 + 补测试。
- [ ] **Step 3:** 终验：`cargo fmt --check && cargo clippy --release --all-targets -- -D warnings && cargo test --release --workspace` 全绿。
- [ ] **Step 4:** 最终 Commit；向用户汇报（不主动 push）。

---

## Self-Review 结论

- Spec 覆盖：§4.1→Task 3；§4.2→Task 4；§4.3→Task 5+6；P0 信任根（jwt claims 绑定）**不在本计划**——需 oj-auth 插件改造，另立任务（文档 §7 已注明）；shared allowlist→Task 5+6；asSystem→Task 3；oj test 逃生→asSystem（不引入 test_tenant 配置，单逃生口）。
- 类型一致性：SqlGuard 单一类型（bridge），config/StableState/Extras 共用；`table_owned_shared` 与 `registry_tables` 四元组在 Task 2/5/6 间一致。
- 已知取舍（评审已认）：裸 SQL 字面检查是 best-effort（防完全遗漏）；Deny 下
  `where tenant_id='字面量'` 形态被错杀（方向正确：改用构造器）。
