//! G3 执行计划归一（`db.explain`，v0.1.108）。
//!
//! 只放三样：方言 EXPLAIN 前缀常量、PG 文本计划 / MySQL FORMAT=JSON 的归一纯函数、
//! `op_db_explain` 壳。壳 = 内层 SQL 过一遍租户守卫 → 拼前缀 → 直接调
//! [`db_query::query_impl`] 原函数（守卫/配额/trace/session 零复制），首行首列取
//! 结果 → 归一 PlanNode（spec：docs/superpowers/specs/2026-10-29-dbx-upstream-g3-u14-design.md）。
//!
//! 铁律（评审裁决 + 真机实测）：
//! 1. **无 ANALYZE 由构造保证**：前缀是宿主常量，用户只供内层语句——EXPLAIN 非
//!    ANALYZE 不执行内层语句，副作用为零。
//! 2. **PG 用文本计划，非 FORMAT JSON**：`EXPLAIN (FORMAT JSON)` 输出 json 类型列，
//!    sqlx Any 驱动不解（v0.1.108 真机实测，有无参数同样报错）；文本格式绑定参数
//!    可用。MySQL 维持 FORMAT=JSON（Any 可解，带参可用）。
//! 3. **参数值绝不内联**：绑定失败是方言能力问题，结构化报错，禁止拼进 SQL 文本。

/// PG 前缀：文本计划（默认格式）。Any 驱动限制见模块头铁律 2。
pub(crate) const PG_EXPLAIN_PREFIX: &str = "EXPLAIN ";
/// MySQL 前缀：JSON 格式，输出列可被 sqlx Any 解码。
pub(crate) const MYSQL_EXPLAIN_PREFIX: &str = "EXPLAIN FORMAT=JSON ";

// ---------------------------------------------------------------------------
// PG 文本计划解析（真机 oj-test-postgres 16 捕获样本钉死语法，v0.1.108）：
// - 根节点无缩进无箭头；深度 d≥1 节点 = 2+6(d-1) 个空格 + "->  "。
// - 节点附属行（Index Cond/Filter/Sort Key…）缩进 = 节点箭头缩进 + 6（根 = 2）。
//   ⇒ 子节点箭头缩进恒等于父节点附属行缩进——栈式解析的依据。
// - 尾部 "Planning Time:" / "Execution Time:" / "Query Identifier:" 非节点行，跳过。
// ---------------------------------------------------------------------------

/// 解析 PG 文本计划（EXPLAIN 默认格式）为归一 PlanNode。
pub(crate) fn normalize_pg_text_plan(text: &str) -> serde_json::Value {
    use serde_json::{Value, json};
    // 栈元素：(附属行缩进, 该节点在 out 树中的路径)。用索引栈定位父节点。
    let mut stack: Vec<(usize, Vec<usize>)> = Vec::new();
    let mut lines = text.lines().peekable();
    // 首个节点行即根
    let root_line = loop {
        match lines.next() {
            None => return json!({ "category": "other", "operation": "unknown", "relation": null,
                "accessPath": null, "cost": null, "rowsEstimate": null, "width": null,
                "extra": null, "children": [] }),
            Some(l) if !is_trailer_line(l) => break l,
            Some(_) => continue,
        }
    };
    let root = pg_node_of(root_line.trim_end());
    let mut tree = root;
    stack.push((2_usize, Vec::new())); // 根附属行缩进 = 2
    let mut cur_path: Vec<usize> = Vec::new(); // 当前节点路径（根的子为 [i]…）
    let mut cur_extra_indent = 2_usize;
    for raw in lines {
        if is_trailer_line(raw) {
            continue;
        }
        let indent = raw.len() - raw.trim_start().len();
        let body = &raw[indent..];
        if let Some(content) = body.strip_prefix("->  ").or_else(|| body.strip_prefix("-> ")) {
            // 节点行：弹出比本节点更深的栈帧，父 = 附属行缩进 == indent 的帧
            while stack.last().map(|(ei, _)| *ei) != Some(indent) && !stack.is_empty() {
                if stack.last().unwrap().0 < indent {
                    break; // 畸形缩进：挂到最近节点，不 panic
                }
                stack.pop();
            }
            let parent_path = stack.last().map(|(_, p)| p.clone()).unwrap_or_default();
            let node = pg_node_of(content.trim_end());
            // 挂到父路径下
            let parent = node_at_mut(&mut tree, &parent_path);
            let idx = parent["children"].as_array_mut().map(|a| a.len()).unwrap_or(0);
            parent["children"].as_array_mut().unwrap().push(node);
            let mut my_path = parent_path;
            my_path.push(idx);
            cur_path = my_path.clone();
            cur_extra_indent = indent + 6;
            stack.push((cur_extra_indent, my_path));
        } else {
            // 附属行：挂当前节点。附属行理论上缩进 == cur_extra_indent，宽容处理。
            let _ = indent;
            let node = node_at_mut(&mut tree, &cur_path);
            let extra = body.trim_end();
            let cur = node["extra"].as_str().unwrap_or("");
            let merged = if cur.is_empty() {
                extra.to_string()
            } else {
                format!("{cur}\n{extra}")
            };
            node["extra"] = json!(merged);
        }
    }
    tree
}

fn is_trailer_line(l: &str) -> bool {
    let t = l.trim_start();
    t.starts_with("Planning Time:") || t.starts_with("Execution Time:") || t.starts_with("Query Identifier:")
}

/// 按路径取树内节点（路径元素 = 各层 children 下标）。
fn node_at_mut<'a>(tree: &'a mut serde_json::Value, path: &[usize]) -> &'a mut serde_json::Value {
    let mut cur = tree;
    for &i in path {
        cur = &mut cur["children"][i];
    }
    cur
}

/// 单节点行 → PlanNode（category/operation/relation/accessPath/cost/rows/width）。
fn pg_node_of(content: &str) -> serde_json::Value {
    use serde_json::json;
    // 切出 (cost=A..B rows=R width=W) 后缀
    let (head, cost) = match content.find(" (cost=") {
        Some(i) => {
            let head = content[..i].trim_end();
            let tail = &content[i + 7..]; // "cost=" 之后
            (head, parse_cost_tuple(tail))
        }
        None => (content, None),
    };
    // using / on 提取
    let (op_part, mut access, mut relation) = (head, None, None);
    let head = if let Some(u) = op_part.find(" using ") {
        let after = &op_part[u + 7..];
        match after.find(" on ") {
            Some(o) => {
                access = Some(after[..o].to_string());
                relation = Some(after[o + 4..].to_string());
            }
            None => access = Some(after.to_string()),
        }
        op_part[..u].trim_end()
    } else if let Some(o) = op_part.find(" on ") {
        relation = Some(op_part[o + 4..].to_string());
        op_part[..o].trim_end()
    } else {
        op_part
    };
    let operation = head.to_string();
    let category = pg_category(&operation);
    let (cost_v, rows, width) = match cost {
        Some((s, t, r, w)) => (
            serde_json::json!({ "startup": s, "total": t }),
            // 整行数归一为整型（1.0 → 1），前端阈值比较不踩浮点
            if r.fract() == 0.0 && r.is_finite() {
                serde_json::json!(r as i64)
            } else {
                serde_json::json!(r)
            },
            serde_json::json!(w),
        ),
        None => (
            serde_json::Value::Null,
            serde_json::Value::Null,
            serde_json::Value::Null,
        ),
    };
    json!({
        "category": category,
        "operation": operation,
        "relation": relation,
        "accessPath": access,
        "cost": cost_v,
        "rowsEstimate": rows,
        "width": width,
        "extra": null,
        "children": [],
    })
}

/// 解析 `0.27..8.29 rows=1 width=8)` 片段。
fn parse_cost_tuple(tail: &str) -> Option<(f64, f64, f64, i64)> {
    let inner = tail.strip_suffix(')').unwrap_or(tail);
    let (range, rest) = inner.split_once(" rows=")?;
    let (startup, total) = range.split_once("..")?;
    let (rows_s, width_s) = rest.split_once(" width=")?;
    Some((
        startup.parse().ok()?,
        total.parse().ok()?,
        rows_s.parse().ok()?,
        width_s.parse().ok()?,
    ))
}

fn pg_category(operation: &str) -> &'static str {
    if operation == "Nested Loop" || operation.contains("Join") {
        "join"
    } else if operation.contains("Scan") {
        "scan"
    } else if operation.contains("Aggregate") || operation.contains("Group") {
        "aggregate"
    } else if operation.contains("Sort") {
        "sort"
    } else {
        "other"
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn explain_prefixes_are_safe_by_construction() {
        assert!(!super::PG_EXPLAIN_PREFIX.contains("ANALYZE"));
        assert!(!super::MYSQL_EXPLAIN_PREFIX.contains("ANALYZE"));
        assert!(super::MYSQL_EXPLAIN_PREFIX.contains("FORMAT=JSON"));
        assert!(super::PG_EXPLAIN_PREFIX.ends_with(' '));
        assert!(super::MYSQL_EXPLAIN_PREFIX.ends_with(' '));
    }

    // 以下三份样本为 oj-test-postgres 16 真机捕获（v0.1.108 契约实验），逐字节钉死
    // PG 文本计划的缩进语法：子节点箭头缩进 = 父节点附属行缩进；附属行 = 节点内容位 + 2。
    const SAMPLE_INDEX_SCAN: &str = "Index Scan using oj_explain_parent_pkey on oj_explain_parent  (cost=0.27..8.29 rows=1 width=8)\n  Index Cond: (id = 42)\n";
    const SAMPLE_NESTED_LOOP: &str = "Nested Loop  (cost=0.27..44.33 rows=4 width=13)\n  ->  Index Only Scan using oj_explain_parent_pkey on oj_explain_parent p  (cost=0.27..8.29 rows=1 width=4)\n        Index Cond: (id = 42)\n  ->  Seq Scan on oj_explain_child c  (cost=0.00..36.00 rows=4 width=13)\n        Filter: (pid = 42)\n";
    const SAMPLE_SORT_AGG: &str = "Sort  (cost=67.73..68.93 rows=480 width=12)\n  Sort Key: pid\n  ->  HashAggregate  (cost=41.56..46.36 rows=480 width=12)\n        Group Key: pid\n        ->  Seq Scan on oj_explain_child  (cost=0.00..36.00 rows=1111 width=4)\n              Filter: (val ~~ 'v1%'::text)\n";

    #[test]
    fn pg_text_index_scan_normalizes() {
        let p = super::normalize_pg_text_plan(SAMPLE_INDEX_SCAN);
        assert_eq!(p["category"], "scan");
        assert_eq!(p["operation"], "Index Scan");
        assert_eq!(p["accessPath"], "oj_explain_parent_pkey");
        assert_eq!(p["relation"], "oj_explain_parent");
        assert_eq!(p["cost"], json!({"startup": 0.27, "total": 8.29}));
        assert_eq!(p["rowsEstimate"], 1);
        assert_eq!(p["width"], 8);
        assert_eq!(p["extra"], "Index Cond: (id = 42)");
        assert_eq!(p["children"], json!([]));
    }

    #[test]
    fn pg_text_nested_loop_normalizes_recursively() {
        let p = super::normalize_pg_text_plan(SAMPLE_NESTED_LOOP);
        assert_eq!(p["category"], "join");
        assert_eq!(p["operation"], "Nested Loop");
        let c = p["children"].as_array().unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(c[0]["operation"], "Index Only Scan");
        assert_eq!(c[0]["accessPath"], "oj_explain_parent_pkey");
        assert_eq!(c[0]["relation"], "oj_explain_parent p");
        assert_eq!(c[0]["extra"], "Index Cond: (id = 42)");
        assert_eq!(c[0]["children"], json!([]));
        assert_eq!(c[1]["operation"], "Seq Scan");
        assert_eq!(c[1]["accessPath"], serde_json::Value::Null);
        assert_eq!(c[1]["relation"], "oj_explain_child c");
        assert_eq!(c[1]["extra"], "Filter: (pid = 42)");
    }

    #[test]
    fn pg_text_sort_aggregate_categories_and_depth() {
        let p = super::normalize_pg_text_plan(SAMPLE_SORT_AGG);
        assert_eq!(p["category"], "sort");
        assert_eq!(p["extra"], "Sort Key: pid");
        let agg = &p["children"][0];
        assert_eq!(agg["category"], "aggregate");
        assert_eq!(agg["operation"], "HashAggregate");
        assert_eq!(agg["extra"], "Group Key: pid");
        let scan = &agg["children"][0];
        assert_eq!(scan["category"], "scan");
        assert_eq!(scan["rowsEstimate"], 1111);
        assert_eq!(scan["extra"], "Filter: (val ~~ 'v1%'::text)");
    }
}
