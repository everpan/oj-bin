//! 表归属守卫（§5.3 / P2）：SQL 里出现的表必须属于本模块或已声明 deps 的模块。
//!
//! 单一检查入口 `check_raw` / `check_table`，三个 db op（query/exec/query_build）统一调用。
//! 裸 SQL 的表名提取是轻量扫描（FROM/JOIN/INTO/UPDATE 后的标识符）+ memo 缓存——
//! 静态解析本就 best-effort（查询构造器路径表名精确，不走扫描）。默认 warn（日志告警），
//! `ownership_guard: deny` 时拒绝执行；无模块上下文（旧路径/测试/WS）不设防。

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use deno_core::OpState;
use deno_error::JsErrorBox;

use super::{ModuleCtx, ReqState, StableState};

/// 词法切分（best-effort）：返回标识符/关键字词序列，跳过字符串字面量与 `--`、`/* */` 注释。
/// 词 = 字母数字 `_` `.` 连续段（`.` 保留以便 `db.table` 只取表段）。
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
            c if c.is_ascii_alphanumeric() || c == b'_' || c == b'.' => {
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

/// 提取 SQL 里的表名：FROM/JOIN/INTO/UPDATE 关键字的下一词；库名限定只取表段。
pub fn extract_tables(sql: &str) -> Vec<String> {
    let words = tokens(sql);
    let mut out: Vec<String> = Vec::new();
    for w in words.windows(2) {
        if matches!(
            w[0].to_ascii_uppercase().as_str(),
            "FROM" | "JOIN" | "INTO" | "UPDATE"
        ) {
            let t = w[1].rsplit('.').next().unwrap_or(w[1]);
            if out.last().map(|l: &String| l != t).unwrap_or(true) {
                out.push(t.to_string());
            }
        }
    }
    out
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
}
