//! 表归属守卫（§5.3 / P2）：SQL 里出现的表必须属于本模块或已声明 deps 的模块。
//!
//! 单一检查入口 `check_raw` / `check_table`，三个 db op（query/exec/query_build）统一调用。
//! 裸 SQL 的表名提取是轻量扫描（FROM/JOIN/INTO/UPDATE 后的标识符）+ memo 缓存——
//! 静态解析本就 best-effort（查询构造器路径表名精确，不走扫描）。默认 warn（日志告警），
//! `ownership_guard: deny` 时拒绝执行；无模块上下文（旧路径/测试/WS）不设防。
//!
//! 多租户裸 SQL 检查 `check_tenant_raw` 独立于此（不挂 module_ctx 短路）：只查
//! 「完全遗漏 tenant_id」（不验证谓词位置/绑定值——评审 P1-2 的诚实降级），
//! Deny 额外要求参数数组含当前租户值；fail-closed 面向 DML/DDL。

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use deno_core::OpState;
use deno_error::JsErrorBox;

use super::{ModuleCtx, ReqState, StableState};

/// 词法切分（best-effort）：返回标识符/关键字词序列，跳过字符串字面量与 `--`、`/* */` 注释。
/// 词 = 字母数字 `_` `.` `,` 连续段（`.` 保留以便 `db.table` 只取表段；`,` 保留以便
/// from_list 识别逗号连接表——extract_tables 的 fail-closed 语义依赖）。
fn tokens(sql: &str) -> Vec<&str> {
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut start = None;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            b'\'' | b'"' | b'`' => {
                // 引用串/引用标识符：跳到配对闭合（'' 双写与 \ 转义都跳过）。
                let q = c;
                i += 1;
                while i < b.len() {
                    if b[i] == b'\\' {
                        i += 2;
                    } else if b[i] == q {
                        if i + 1 < b.len() && b[i + 1] == q {
                            i += 2; // SQL 双写转义 ''
                        } else {
                            i += 1;
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
                start = None;
            }
            b'-' if i + 1 < b.len() && b[i + 1] == b'-' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                start = None;
            }
            b'/' if i + 1 < b.len() && b[i + 1] == b'*' => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(b.len());
                start = None;
            }
            c if c.is_ascii_alphanumeric() || c == b'_' || c == b'.' || c == b',' => {
                if start.is_none() {
                    start = Some(i);
                }
                i += 1;
            }
            _ => {
                if let Some(s) = start.take() {
                    out.push(&sql[s..i]);
                }
                i += 1;
            }
        }
    }
    if let Some(s) = start {
        out.push(&sql[s..]);
    }
    out
}

/// 提取 SQL 里的表名：FROM/JOIN/INTO/UPDATE 后的标识符；FROM/UPDATE 支持逗号连接列表
/// （`from a, b` / `update a, b set`）与裸别名跳读；TRUNCATE/ALTER/DROP 同样提取
/// （TABLE 关键字跳过）。best-effort 上界：漏抓不漏放，fail-closed 语义由调用方兜。
pub fn extract_tables(sql: &str) -> Vec<String> {
    let words = tokens(sql);
    let mut out: Vec<String> = Vec::new();
    let push = |t: &str, out: &mut Vec<String>| {
        let t = t.to_string();
        if out.last() != Some(&t) {
            out.push(t);
        }
    };
    let mut i = 0;
    while i < words.len() {
        match words[i].to_ascii_uppercase().as_str() {
            "FROM" | "UPDATE" => {
                let (list, ni) = from_list(&words, i + 1);
                for t in list {
                    push(&t, &mut out);
                }
                i = ni;
            }
            "JOIN" | "INTO" => {
                if let Some(t) = words.get(i + 1) {
                    push(t.rsplit('.').next().unwrap_or(t), &mut out);
                }
                i += 1;
            }
            "TRUNCATE" | "ALTER" | "DROP" => {
                let mut j = i + 1;
                if words
                    .get(j)
                    .is_some_and(|w| w.eq_ignore_ascii_case("table"))
                {
                    j += 1;
                }
                if let Some(t) = words.get(j) {
                    push(t.rsplit('.').next().unwrap_or(t), &mut out);
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    out
}

/// FROM/UPDATE 后的逗号连接表列表（best-effort）：`FROM t1 a1, t2 a2 WHERE ...`；
/// 派生表 `FROM (SELECT...)` 放弃本列表（子查询内部的 FROM 自有窗口）。
/// 返回 (表名列表, 下一个未消费词的索引)。
fn from_list(words: &[&str], mut i: usize) -> (Vec<String>, usize) {
    let mut out = Vec::new();
    while let Some(w) = words.get(i) {
        // 派生表放弃本列表：括号起始或子查询 SELECT（tokens 已剥括号，认关键字）。
        if w.starts_with('(') || w.eq_ignore_ascii_case("select") {
            break;
        }
        let t = w.trim_matches(|c: char| c == ',' || c == '(' || c == ')' || c == ';');
        if t.is_empty() {
            break;
        }
        out.push(t.rsplit('.').next().unwrap_or(t).to_string());
        let comma = w.ends_with(',');
        i += 1;
        if comma {
            continue;
        }
        // 可选别名：`AS x` 或裸别名词（边界关键字/子查询起始除外）；别名词自带逗号
        // （`a x, b`）或别名后紧跟逗号（`a x , b`）都意味着列表延续。
        if let Some(nw) = words.get(i) {
            let nc = nw.trim_matches(|c: char| c == ',' || c == '(' || c == ')' || c == ';');
            if nc.eq_ignore_ascii_case("as") {
                i += 2; // AS + 别名
                if words.get(i).is_some_and(|w| w.ends_with(',')) {
                    i += 1;
                    continue;
                }
            } else if nw.ends_with(',') {
                i += 1; // 别名词自带逗号（x,）→ 逗号已消费，直接续表
                continue;
            } else if !nw.starts_with('(') && !is_boundary_kw(nc) {
                i += 1; // 裸别名（orders a）
                if words.get(i).is_some_and(|w| w.ends_with(',')) {
                    i += 1;
                    continue;
                }
            }
        }
        break;
    }
    (out, i)
}

/// 表列表边界关键字（遇到即列表结束）。裸别名跳读时用于区分「别名 vs 子句关键字」。
fn is_boundary_kw(w: &str) -> bool {
    matches!(
        w.to_ascii_uppercase().as_str(),
        "WHERE"
            | "GROUP"
            | "ORDER"
            | "LIMIT"
            | "OFFSET"
            | "HAVING"
            | "SET"
            | "ON"
            | "JOIN"
            | "LEFT"
            | "RIGHT"
            | "INNER"
            | "OUTER"
            | "CROSS"
            | "FULL"
            | "UNION"
            | "INTERSECT"
            | "EXCEPT"
            | "RETURNING"
            | "USING"
            | "VALUES"
            | "SELECT"
    )
}

/// 取本请求模块上下文（无上下文 / 模块未登记 → None = 不设防）。
fn module_ctx(state: &Rc<RefCell<OpState>>) -> Option<(ModuleCtx, Arc<StableState>)> {
    let g = state.borrow();
    let name = g.borrow::<ReqState>().module.clone()?;
    let stable = g.borrow::<Arc<StableState>>().clone();
    // modules 键 = 目录路径（run_module 祖先命中用），此处按名取。
    // ponytail: 模块数个位~几十，线性扫远低于 SQL 噪声；模块表破百再换按名索引。
    let ctx = stable.modules.values().find(|c| c.name == name)?.clone();
    Some((ctx, stable))
}

/// 归属判定 + warn/deny 处置（Err = deny 模式拒绝）。
/// 未声明归属的表（owner=None）不设防——那是静态 S003 检查的职责。
fn judge(stable: &StableState, ctx: &ModuleCtx, table: &str, src: &str) -> Result<(), JsErrorBox> {
    let Some(owner) = stable.registry.owner_of(table) else {
        return Ok(());
    };
    if owner == ctx.name || ctx.deps.contains(owner) {
        return Ok(());
    }
    let msg = format!(
        "ownership: 表 {table:?} 属于模块 {owner:?}，模块 {:?} 未声明依赖（{src}）",
        ctx.name
    );
    if stable.ownership_deny {
        return Err(JsErrorBox::generic(format!(
            "{msg}\n  修复：在模块 manifest.yaml 声明 deps: [{owner}]，或改用契约调用"
        )));
    }
    eprintln!("warn: {msg}");
    Ok(())
}

/// 裸 SQL 守卫（op_db_query / op_db_exec）：提取表名（memo 缓存）→ 逐表归属判定。
pub fn check_raw(state: &Rc<RefCell<OpState>>, sql: &str) -> Result<(), JsErrorBox> {
    let Some((ctx, stable)) = module_ctx(state) else {
        return Ok(());
    };
    let tables = {
        let mut memo = stable.sql_memo.lock().unwrap();
        if let Some(t) = memo.get(sql) {
            t.clone()
        } else {
            let t = Arc::new(extract_tables(sql));
            memo.insert(sql.to_string(), t.clone());
            t
        }
    };
    for t in tables.iter() {
        judge(&stable, &ctx, t, "raw sql")?;
    }
    Ok(())
}

/// 构造器路径守卫（op_db_query_build）：表名精确已知，不走扫描。
pub fn check_table(state: &Rc<RefCell<OpState>>, table: &str) -> Result<(), JsErrorBox> {
    let Some((ctx, stable)) = module_ctx(state) else {
        return Ok(());
    };
    judge(&stable, &ctx, table, "db.table()")
}

/// 多字节字符的 utf-8 字节长度（strip_literals 按字节扫描用）。
fn utf8_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >= 0xF0 {
        4
    } else if b >= 0xE0 {
        3
    } else {
        2
    }
}

/// 剥单引号字符串字面量与 `--` / `* *` 注释（内容替换为空格）；**保留**双引号/反引号
/// 标识符——sea-query 三方言渲染产物（toSQL 回放路径）依赖标识符引号存活
/// （评审：直接复用 tokens() 会把 "tenant_id" 当引用串跳过 → 100% 误杀）。
pub fn strip_literals(sql: &str) -> String {
    let b = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\'' => {
                i += 1;
                while i < b.len() {
                    if b[i] == b'\'' {
                        if i + 1 < b.len() && b[i + 1] == b'\'' {
                            i += 2;
                        } else {
                            i += 1;
                            break;
                        }
                    } else if b[i] == b'\\' {
                        i = (i + 2).min(b.len());
                    } else {
                        i += 1;
                    }
                }
                out.push(' ');
            }
            b'-' if i + 1 < b.len() && b[i + 1] == b'-' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                out.push(' ');
            }
            b'/' if i + 1 < b.len() && b[i + 1] == b'*' => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(b.len());
                out.push(' ');
            }
            c => {
                let n = utf8_len(c);
                out.push_str(&sql[i..(i + n).min(sql.len())]);
                i += n;
            }
        }
    }
    out
}

/// 裸 SQL 租户检查（op_db_query / op_db_exec；sql_guard != Off 时调用）。
/// 独立于 ownership 守卫与 module_ctx（tenant 判定只需全局 registry，不得被
/// 无模块上下文短路——评审 P1-5）。
///
/// - 只查「完全遗漏」：清洗后文本无 tenant_id 即缺失（不验证谓词位置/绑定值）。
/// - Deny：缺失 → 拒；有 tenant_id 但参数数组不含当前租户值 → 拒（防
///   `where tenant_id = ?` 绑用户输入的「值错误」形态——评审 P1-2 强化）。
/// - tid=None：触受约束表 → Deny 拒 / Warn 警（decision §8-1）。
/// - fail-closed：DML/DDL 关键字命中但提取不到任何已登记表 → 拒/警；
///   TRUNCATE/ALTER/DROP/CREATE 触碰已注册表一律拒/警（无法注入条件，无合法多租户用途）。
pub fn check_tenant_raw(
    state: &Rc<RefCell<OpState>>,
    sql: &str,
    params: &[serde_json::Value],
) -> Result<(), JsErrorBox> {
    let (guard, tid, system, registry) = {
        let g = state.borrow();
        let st = g.borrow::<Arc<StableState>>();
        let rs = g.borrow::<ReqState>();
        (
            st.sql_guard,
            rs.req.tenant_id.clone(),
            rs.system,
            st.registry.clone(),
        )
    };
    if guard == super::SqlGuard::Off || system {
        return Ok(());
    }
    let deny = guard == super::SqlGuard::Deny;
    let extracted = extract_tables(sql);
    let registered: Vec<&str> = extracted
        .iter()
        .map(|s| s.as_str())
        .filter(|t| registry.has_table(t))
        .collect();
    let first_kw = sql
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    let ddl = matches!(first_kw.as_str(), "TRUNCATE" | "ALTER" | "DROP" | "CREATE");
    let dml = matches!(first_kw.as_str(), "INSERT" | "UPDATE" | "DELETE");
    let verdict = |msg: String| -> Result<(), JsErrorBox> {
        if deny {
            return Err(JsErrorBox::generic(msg));
        }
        eprintln!("warn: {msg}");
        Ok(())
    };
    // fail-closed：DML/DDL 命中但提取不到已登记表（可能是扫描漏抓 → 不赌）。
    if (ddl || dml) && registered.is_empty() {
        return verdict(format!(
            "tenant guard: {first_kw} touches no registered table (cannot verify tenant scope)"
        ));
    }
    // DDL 面：无法注入条件 → 一律拒/警。
    if ddl {
        return verdict(format!(
            "tenant guard: {first_kw} on registered table(s) {registered:?} not allowed under sql_guard"
        ));
    }
    let scoped: Vec<&str> = registered
        .iter()
        .copied()
        .filter(|t| registry.get(t).is_some_and(|d| d.is_tenant_scoped()))
        .collect();
    if scoped.is_empty() {
        return Ok(());
    }
    let cleaned = strip_literals(sql).to_ascii_lowercase();
    let mentions = cleaned.contains("tenant_id");
    let param_has = tid
        .as_deref()
        .is_some_and(|t| params.iter().any(|p| p.as_str() == Some(t)));
    let ok = match tid {
        Some(_) => mentions && (param_has || !deny),
        None => false,
    };
    if ok {
        return Ok(());
    }
    let msg = match tid {
        Some(_) if !mentions => format!(
            "tenant guard: raw sql on {scoped:?} lacks tenant_id condition (use db.table() builder)"
        ),
        Some(t) => format!(
            "tenant guard: raw sql params must include current tenant id ({t:?}) under deny mode"
        ),
        None => format!(
            "tenant guard: table(s) {scoped:?} require tenant context (missing tenant header)"
        ),
    };
    verdict(msg)
}

/// 模块默认库重定向（manifest `db:` 绑定）：仅重定向字面 "default"，
/// 显式 DB("name") 的语义不受影响。始终返回 owned String。
pub fn bound_db(state: &Rc<RefCell<OpState>>, name: &str) -> String {
    if name != "default" {
        return name.to_string();
    }
    let g = state.borrow();
    let rs = g.borrow::<ReqState>();
    let Some(m) = rs.module.clone() else {
        return name.to_string();
    };
    let bound = g
        .borrow::<Arc<StableState>>()
        .modules
        .values()
        .find(|c| c.name == m)
        .and_then(|c| c.db.clone());
    bound.unwrap_or_else(|| name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_from_join_into_update() {
        assert_eq!(extract_tables("select * from user where id = ?"), ["user"]);
        assert_eq!(
            extract_tables("select a.id from orders a join user u on a.uid = u.id"),
            ["orders", "user"]
        );
        assert_eq!(
            extract_tables("insert into order_item (id) values (1)"),
            ["order_item"]
        );
        assert_eq!(
            extract_tables("update user set name = 'x' where id = 1"),
            ["user"]
        );
        // 库名限定只取表段；大小写不敏感；重复表去重。
        assert_eq!(
            extract_tables("SELECT * FROM analytics.metrics JOIN t2 USING (id)"),
            ["metrics", "t2"]
        );
        assert_eq!(
            extract_tables("select * from t1 left join t1 on x = y"),
            ["t1"]
        );
        // 字符串/注释内不误抓。
        assert_eq!(
            extract_tables("select * from t where s = 'from ghost' -- join x\n"),
            ["t"]
        );
        assert_eq!(extract_tables("select * /* from ghost */ from t"), ["t"]);
        // 关键字后接非表词（select 子查询 / values）：多抓不漏抓是 best-effort 上界，
        // 但纯字面误报要免——select 1 不产出。
        assert!(extract_tables("select 1").is_empty());
        // SQL 标准双写转义 '' 不提前闭合。
        assert_eq!(
            extract_tables("update t set s = 'a''from x' where id = 1"),
            ["t"]
        );
    }

    // ----- 多租户 sql_guard：extract_tables 扩展 + strip_literals + check_tenant_raw -----

    #[test]
    fn extract_comma_join_multitable_update_and_ddl() {
        // 逗号连接（此前漏抓 b）。
        assert_eq!(
            extract_tables("select * from a, b where a.id = b.id"),
            ["a", "b"]
        );
        assert_eq!(
            extract_tables("select * from a x, b y where x.id = y.id"),
            ["a", "b"]
        );
        // MySQL 多表 UPDATE（此前漏抓 b）。
        assert_eq!(
            extract_tables("update a, b set a.x = 1 where a.id = b.id"),
            ["a", "b"]
        );
        // DDL 面：TRUNCATE / ALTER / DROP（TABLE 关键字跳过）。
        assert_eq!(extract_tables("truncate table t"), ["t"]);
        assert_eq!(extract_tables("alter table t add column x integer"), ["t"]);
        assert_eq!(extract_tables("drop table t"), ["t"]);
        // 别名 + JOIN 混合不回退。
        assert_eq!(
            extract_tables("select a.id from orders a join user u on a.uid = u.id"),
            ["orders", "user"]
        );
        // 派生表：列表放弃（内部 FROM 自有窗口）。
        assert_eq!(
            extract_tables("select * from (select id from inner_t) x"),
            ["inner_t"]
        );
    }

    #[test]
    fn strip_literals_keeps_quoted_identifiers() {
        // 单引号串与注释内容被剥除 → 不再匹配 tenant_id。
        assert!(!strip_literals("select * from t where tag = 'tenant_id'").contains("'tenant_id'"));
        let s = strip_literals("select * from t -- tenant_id\nwhere id = 1");
        assert!(!s.to_lowercase().contains("tenant_id"), "{s}");
        // 双引号/反引号标识符保留（toSQL 回放路径依赖）。
        assert!(strip_literals("select \"tenant_id\" from t").contains("\"tenant_id\""));
        assert!(strip_literals("select `tenant_id` from t").contains("`tenant_id`"));
    }

    /// 行为面：Deny 模式经 Bridge 跑裸 SQL（check_tenant_raw 已挂 op_db_query/exec）。
    mod behavior {
        use crate::bridge::{
            Bridge, DataAccessor, Extras, InMemoryKV, RequestInfo, SchemaRegistry, SqlGuard,
            SqlxAccessor,
        };
        use serde_json::{Value, json};
        use std::sync::Arc;

        async fn guard_off_fixture() -> (Bridge, Arc<dyn DataAccessor>) {
            let db = SqlxAccessor::arc("sqlite::memory:").await.unwrap();
            db.exec_with_params(
                "create table t (id integer primary key, name text, tenant_id text)",
                &[],
            )
            .await
            .unwrap();
            db.exec_with_params("create table s (id integer primary key, name text)", &[])
                .await
                .unwrap();
            let b = Bridge::with_dbs_and_loader(
                std::collections::HashMap::from([("default".to_string(), db.clone() as _)]),
                Arc::new(InMemoryKV::new()),
                SchemaRegistry::new()
                    .table("t", &["id"], &["id", "name", "tenant_id"])
                    .table_owned_shared("m", "s", &["id"], &["id", "name"], true),
                false,
                None,
                Extras {
                    sql_guard: SqlGuard::Deny,
                    ..Default::default()
                },
            );
            (b, db)
        }

        fn req_t1() -> RequestInfo {
            RequestInfo {
                tenant_id: Some("t1".into()),
                ..Default::default()
            }
        }

        async fn run_sql(b: &Bridge, req: RequestInfo, sql: &str, params: &[Value]) -> Value {
            let cap = b
                .run_with(
                    &format!(
                        r#"db.query({sql}, {}).then(r => json.ok({{n: r.length}})).catch(e => json.fail(400, String(e)));"#,
                        serde_json::to_string(params).unwrap()
                    ),
                    req,
                )
                .await
                .unwrap();
            serde_json::from_slice(&cap.body).unwrap()
        }

        #[tokio::test(flavor = "current_thread")]
        async fn deny_raw_sql_tenant_checks() {
            let (b, _) = guard_off_fixture().await;
            // 1) 触受约束表、无 tenant_id 条件 → 拒
            let v = run_sql(&b, req_t1(), r#""select * from t""#, &[]).await;
            assert_eq!(v["code"], 400, "{v}");
            assert!(
                v["msg"].as_str().unwrap().contains("lacks tenant_id"),
                "{v}"
            );
            // 2) tenant_id = ? 且参数含当前租户 → 过
            let v = run_sql(
                &b,
                req_t1(),
                r#""select * from t where tenant_id = ?""#,
                &[json!("t1")],
            )
            .await;
            assert_eq!(v["code"], 0, "{v}");
            // 3) 有 tenant_id 但参数不含当前租户（值错误形态）→ 拒
            let v = run_sql(
                &b,
                req_t1(),
                r#""select * from t where tenant_id = ?""#,
                &[json!("t2")],
            )
            .await;
            assert_eq!(v["code"], 400, "{v}");
            assert!(
                v["msg"].as_str().unwrap().contains("params must include"),
                "{v}"
            );
            // 4) 字符串字面量里嵌 'tenant_id' 不算条件
            let v = run_sql(
                &b,
                req_t1(),
                r#""select * from t where tag = 'tenant_id'""#,
                &[],
            )
            .await;
            assert_eq!(v["code"], 400, "{v}");
            // 5) toSQL 回放形态（方言引号 + 绑定参数）→ 过（回归守卫）
            let cap = b
                .run_with(
                    r#"json.ok(db.table("t").select(["name"]).toSQL());"#,
                    req_t1(),
                )
                .await
                .unwrap();
            let s: Value = serde_json::from_slice(&cap.body).unwrap();
            let sql = s["data"]["sql"].as_str().unwrap();
            let params = s["data"]["params"].clone();
            let v = run_sql(
                &b,
                req_t1(),
                &serde_json::to_string(sql).unwrap(),
                params.as_array().unwrap(),
            )
            .await;
            assert_eq!(v["code"], 0, "toSQL 回放被拒: {v} sql={sql}");
            // 6) TRUNCATE / DDL → 拒
            let cap = b
                .run_with(
                    r#"db.exec("truncate table t").then(() => json.ok({})).catch(e => json.fail(400, String(e)));"#,
                    req_t1(),
                )
                .await
                .unwrap();
            let v: Value = serde_json::from_slice(&cap.body).unwrap();
            assert_eq!(v["code"], 400, "{v}");
            // 7) fail-closed：DML 关键字 + 未注册表 → 拒
            let v = run_sql(
                &b,
                req_t1(),
                r#""delete from ghost_table where id = 1""#,
                &[],
            )
            .await;
            assert_eq!(v["code"], 400, "{v}");
            assert!(
                v["msg"].as_str().unwrap().contains("no registered table"),
                "{v}"
            );
            // 8) select 1（无 DML/DDL）→ 过
            let v = run_sql(&b, req_t1(), r#""select 1""#, &[]).await;
            assert_eq!(v["code"], 0, "{v}");
            // 9) 共享表 → 不检查
            let v = run_sql(&b, req_t1(), r#""select * from s""#, &[]).await;
            assert_eq!(v["code"], 0, "{v}");
            // 10) tid=None 触受约束表 → 拒
            let v = run_sql(
                &b,
                RequestInfo::default(),
                r#""select * from t where tenant_id = ?""#,
                &[json!("t1")],
            )
            .await;
            assert_eq!(v["code"], 400, "{v}");
            // 11) asSystem 逃生口 → 过
            let cap = b
                .run_with(
                    r#"db.asSystem().query("select * from t").then(r => json.ok({n: r.length})).catch(e => json.fail(400, String(e)));"#,
                    RequestInfo::default(),
                )
                .await
                .unwrap();
            let v: Value = serde_json::from_slice(&cap.body).unwrap();
            assert_eq!(v["code"], 0, "{v}");
        }

        #[tokio::test(flavor = "current_thread")]
        async fn warn_mode_only_warns() {
            let db = SqlxAccessor::arc("sqlite::memory:").await.unwrap();
            db.exec_with_params(
                "create table t (id integer primary key, name text, tenant_id text)",
                &[],
            )
            .await
            .unwrap();
            let b = Bridge::with_dbs_and_loader(
                std::collections::HashMap::from([("default".to_string(), db as _)]),
                Arc::new(InMemoryKV::new()),
                SchemaRegistry::new().table("t", &["id"], &["id", "name", "tenant_id"]),
                false,
                None,
                Extras {
                    sql_guard: SqlGuard::Warn,
                    ..Default::default()
                },
            );
            // 无 tenant_id 条件：Warn 放行（只告警）。
            let v = run_sql(&b, req_t1(), r#""select * from t""#, &[]).await;
            assert_eq!(v["code"], 0, "{v}");
            // tid=None：Warn 放行。
            let v = run_sql(&b, RequestInfo::default(), r#""select * from t""#, &[]).await;
            assert_eq!(v["code"], 0, "{v}");
        }

        #[tokio::test(flavor = "current_thread")]
        async fn off_mode_untouched() {
            let db = SqlxAccessor::arc("sqlite::memory:").await.unwrap();
            db.exec_with_params(
                "create table t (id integer primary key, name text, tenant_id text)",
                &[],
            )
            .await
            .unwrap();
            let b = Bridge::with_dbs_and_loader(
                std::collections::HashMap::from([("default".to_string(), db as _)]),
                Arc::new(InMemoryKV::new()),
                SchemaRegistry::new().table("t", &["id"], &["id", "name", "tenant_id"]),
                false,
                None,
                Extras::default(),
            );
            let v = run_sql(&b, req_t1(), r#""select * from t""#, &[]).await;
            assert_eq!(v["code"], 0, "{v}");
        }
    }
}
