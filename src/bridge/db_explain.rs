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

use deno_core::{OpState, op2};

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
    use serde_json::json;
    // 栈元素：(附属行缩进, 该节点在 out 树中的路径)。用索引栈定位父节点。
    let mut stack: Vec<(usize, Vec<usize>)> = Vec::new();
    let mut lines = text.lines().peekable();
    // 首个节点行即根
    let root_line = loop {
        match lines.next() {
            None => {
                return json!({ "category": "other", "operation": "unknown", "relation": null,
                "accessPath": null, "cost": null, "rowsEstimate": null, "width": null,
                "extra": null, "children": [] });
            }
            Some(l) if !is_trailer_line(l) => break l,
            Some(_) => continue,
        }
    };
    let root = pg_node_of(root_line.trim_end());
    let mut tree = root;
    stack.push((2_usize, Vec::new())); // 根附属行缩进 = 2
    let mut cur_path: Vec<usize> = Vec::new(); // 当前节点路径（根的子为 [i]…）
    for raw in lines {
        if is_trailer_line(raw) {
            continue;
        }
        let indent = raw.len() - raw.trim_start().len();
        let body = &raw[indent..];
        if let Some(content) = body
            .strip_prefix("->  ")
            .or_else(|| body.strip_prefix("-> "))
        {
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
            let idx = parent["children"]
                .as_array_mut()
                .map(|a| a.len())
                .unwrap_or(0);
            parent["children"].as_array_mut().unwrap().push(node);
            let mut my_path = parent_path;
            my_path.push(idx);
            cur_path = my_path.clone();
            stack.push((indent + 6, my_path));
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
    t.starts_with("Planning Time:")
        || t.starts_with("Execution Time:")
        || t.starts_with("Query Identifier:")
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
        // `Bitmap Index Scan on <名>` 的 on 命名的是**索引**（该节点无 using 子句），
        // 索引名进 accessPath；其余裸 on（Seq Scan on t…）命名的是表，进 relation。
        let name = &op_part[o + 4..];
        if op_part[..o].contains("Index Scan") {
            access = Some(name.to_string());
        } else {
            relation = Some(name.to_string());
        }
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

// ---------------------------------------------------------------------------
// MySQL FORMAT=JSON 归一（真机 oj-test-mysql 8.4 捕获样本钉死，v0.1.108）。
// 结果外层是 `{ "EXPLAIN": "<json 字符串>" }`（op 层先 from_str 再进本函数）。
// 归一器是递归下降**任意容器键**的通用 walker——禁止 nested_loop 特判：
// nested_loop / grouping_operation / duplicates_removal / ordering_operation /
// union_result(query_specifications[]) / materialized_from_subquery /
// shared_materialized_from_subquery / attached_subqueries[] /
// buffered_attached_subqueries[] / select_list_subqueries[] / having_subqueries[] /
// query_block 都是容器；table 是叶子。
// ---------------------------------------------------------------------------

/// 容器键的固定遍历优先级（一个节点极少同时持多个；次序确定性靠这里）。
/// select_list_subqueries / having_subqueries / buffered_attached_subqueries 与
/// attached_subqueries 同形（透明数组）；shared_materialized_from_subquery 与
/// materialized_from_subquery 同形（对象容器）——漏收即静默丢子树（终审 Important-2）。
const MYSQL_CONTAINER_KEYS: [&str; 14] = [
    "ordering_operation",
    "duplicates_removal",
    "grouping_operation",
    "nested_loop",
    "union_result",
    "query_specifications", // union_result 的臂数组（元素包 query_block）
    "materialized_from_subquery",
    "shared_materialized_from_subquery",
    "attached_subqueries",
    "buffered_attached_subqueries",
    "select_list_subqueries",
    "having_subqueries",
    "query_block",
    "table", // 叶子槽位在末位：容器持有 table 时降级为单子节点，不吞容器自身
];

/// 解析 MySQL EXPLAIN FORMAT=JSON（已 serde 解析的 `{query_block: ...}` 根）为归一 PlanNode。
pub(crate) fn normalize_mysql_plan(explain_json: &serde_json::Value) -> serde_json::Value {
    match explain_json.get("query_block") {
        Some(qb) => mysql_node_of(qb, "query_block"),
        None => empty_node("query_block"),
    }
}

fn empty_node(operation: &str) -> serde_json::Value {
    serde_json::json!({
        "category": "other", "operation": operation, "relation": null, "accessPath": null,
        "cost": null, "rowsEstimate": null, "width": null, "extra": null, "children": [],
    })
}

/// 容器节点：operation = 命中的容器键名，children = 各容器键的递归结果。
/// union_result 等容器自身可带 table 形态字段（table_name/access_type），relation 照收。
fn mysql_node_of(v: &serde_json::Value, operation: &str) -> serde_json::Value {
    use serde_json::json;
    let mut children: Vec<serde_json::Value> = Vec::new();
    for key in MYSQL_CONTAINER_KEYS {
        match v.get(key) {
            Some(serde_json::Value::Array(arr)) => {
                // nested_loop 是**有名字的连接容器**（category=join），臂作它的 children；
                // query_specifications / attached_subqueries 是透明数组，元素直接进当前节点。
                if key == "nested_loop" {
                    let arms: Vec<serde_json::Value> = arr.iter().map(mysql_array_item).collect();
                    children.push(serde_json::json!({
                        "category": "join",
                        "operation": "nested_loop",
                        "relation": null,
                        "accessPath": null,
                        "cost": null,
                        "rowsEstimate": null,
                        "width": null,
                        "extra": null,
                        "children": arms,
                    }));
                    continue;
                }
                for item in arr {
                    children.push(mysql_array_item(item));
                }
            }
            Some(obj @ serde_json::Value::Object(_)) if key == "table" => {
                children.push(mysql_table_leaf(obj));
            }
            Some(obj @ serde_json::Value::Object(_)) => {
                children.push(mysql_node_of(obj, key));
            }
            _ => {}
        }
    }
    let relation = v
        .get("table_name")
        .and_then(|t| t.as_str())
        .map(|s| s.to_string());
    let cost = v
        .get("cost_info")
        .and_then(|c| c.get("query_cost").or_else(|| c.get("prefix_cost")))
        .and_then(|n| n.as_str())
        .and_then(|s| s.parse::<f64>().ok());
    let category = match operation {
        "nested_loop" => "join",
        "grouping_operation" => "aggregate",
        _ => "other",
    };
    json!({
        "category": category,
        "operation": operation,
        "relation": relation,
        "accessPath": null,
        "cost": { "startup": null, "total": cost },
        "rowsEstimate": null,
        "width": null,
        "extra": null,
        "children": children,
    })
}

/// 数组元素三态归一：{"table": …} → 叶子；{"query_block": …} → 解包容器；裸容器 → 递归。
fn mysql_array_item(item: &serde_json::Value) -> serde_json::Value {
    if let Some(t) = item.get("table") {
        mysql_table_leaf(t)
    } else if let Some(qb) = item.get("query_block") {
        mysql_node_of(qb, "query_block")
    } else {
        mysql_node_of(item, "container")
    }
}

/// table 叶子：operation=access_type、accessPath=key、rowsEstimate=rows_produced_per_join、
/// cost.total=prefix_cost（MySQL 给字符串，解析为数值）；extra 收集 used_key_parts/ref/
/// filtered/attached_condition（存在的才进）。
fn mysql_table_leaf(t: &serde_json::Value) -> serde_json::Value {
    use serde_json::json;
    let str_of = |k: &str| t.get(k).and_then(|v| v.as_str()).map(|s| s.to_string());
    let extra_parts: Vec<String> = [
        (
            "used_key_parts",
            t.get("used_key_parts").and_then(|a| {
                a.as_array().map(|xs| {
                    xs.iter()
                        .filter_map(|x| x.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
            }),
        ),
        (
            "ref",
            t.get("ref").and_then(|a| {
                a.as_array().map(|xs| {
                    xs.iter()
                        .filter_map(|x| x.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
            }),
        ),
        ("filtered", str_of("filtered")),
        ("attached_condition", str_of("attached_condition")),
    ]
    .into_iter()
    .filter_map(|(k, v)| v.map(|s| format!("{k}: {s}")))
    .collect();
    let extra = if extra_parts.is_empty() {
        serde_json::Value::Null
    } else {
        json!(extra_parts.join("\n"))
    };
    json!({
        "category": "scan",
        "operation": str_of("access_type").unwrap_or_else(|| "table".into()),
        "relation": str_of("table_name"),
        "accessPath": str_of("key"),
        "cost": {
            "startup": null,
            "total": t.get("cost_info")
                .and_then(|c| c.get("prefix_cost"))
                .and_then(|n| n.as_str())
                .and_then(|s| s.parse::<f64>().ok()),
        },
        "rowsEstimate": t.get("rows_produced_per_join").cloned().unwrap_or(serde_json::Value::Null),
        "width": null,
        "extra": extra,
        "children": [],
    })
}

// ---------------------------------------------------------------------------
// op 壳：守卫先行 → 方言分派 → query_impl 原函数（同一条通路）→ 归一。
// ---------------------------------------------------------------------------

/// db.explain(sql, params?, opts?)：Promise<{plan: PlanNode}>。
/// PG 文本计划多行（每行一节点），逐行拼接后整树解析；MySQL 首行首列为 JSON 字符串。
#[op2]
#[serde]
pub async fn op_db_explain(
    state: std::rc::Rc<std::cell::RefCell<OpState>>,
    #[string] name: String,
    #[string] sql: String,
    #[serde] params: Option<Vec<serde_json::Value>>,
    #[serde] opts: Option<super::db_query::DbCallOpts>,
) -> Result<serde_json::Value, deno_error::JsErrorBox> {
    let params = params.unwrap_or_default();
    // 守卫先行（与 query_impl 同序）：**剥掉 EXPLAIN 前缀的内层 SQL** 先过租户守卫——
    // 不剥则首词 = EXPLAIN，Deny 模式的 DML/DDL fail-closed 分派被绕过（评审 M3，
    // static 钉死见 tests::explain_inner_sql_passes_tenant_guard_deny）。
    super::guard::check_tenant_raw(&state, inner_sql_of(&sql), &params)?;
    let da = super::query::lookup(&state, &name)?;
    let dialect = da.dialect_info().name.clone();
    let prefix = match dialect.as_str() {
        "postgres" => PG_EXPLAIN_PREFIX,
        "mysql" => MYSQL_EXPLAIN_PREFIX,
        other => {
            return Err(super::db::to_js_err(Box::new(
                super::db_error::unsupported(other, "explain"),
            )));
        }
    };
    // query_impl 内部会对拼好前缀的整句再过一次守卫（首词 EXPLAIN = 传统只读路径）；
    // 上面那次「剥前缀的守卫」是承重的 Deny fail-closed 补偿，不可省——勿「优化」成单次。
    let rows = super::db_query::query_impl(
        state,
        name,
        format!("{prefix}{sql}"),
        params,
        opts.and_then(|o| o.timeout_ms),
    )
    .await?;
    // 首行首列按位置取（按名取 QUERY PLAN/EXPLAIN 有版本风险）。
    let cell = |r: &serde_json::Value| {
        r.as_object()
            .and_then(|m| m.values().next())
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    };
    let plan = match dialect.as_str() {
        "postgres" => {
            // 文本计划多行：全部行的首列拼回整树（节点行 + 附属行 + 尾部统计行）。
            let mut text = String::new();
            for r in &rows {
                if let Some(s) = cell(r) {
                    text.push_str(&s);
                    text.push('\n');
                }
            }
            normalize_pg_text_plan(&text)
        }
        _ => {
            let raw = rows
                .first()
                .and_then(|r| r.as_object())
                .and_then(|m| m.values().next().cloned())
                .ok_or_else(|| deno_error::JsErrorBox::generic("explain: empty result"))?;
            let v: serde_json::Value = match &raw {
                serde_json::Value::String(s) => serde_json::from_str(s).map_err(|e| {
                    deno_error::JsErrorBox::generic(format!("explain: bad plan json: {e}"))
                })?,
                other => other.clone(),
            };
            normalize_mysql_plan(&v)
        }
    };
    Ok(serde_json::json!({ "plan": plan }))
}

/// 剥 EXPLAIN 前缀，返回内层语句（租户守卫/风险语义都应以它为准）。
/// 覆盖两种宿主前缀形态：`EXPLAIN `（PG 文本）与 `EXPLAIN FORMAT=JSON `（MySQL），
/// 以及 PG 选项子句 `EXPLAIN (...) ...`。剥不掉的输入原样返回（守卫看到 EXPLAIN
/// 首词 = 传统只读路径，与 db.query 的口径一致）。
fn inner_sql_of(sql: &str) -> &str {
    let t = sql.trim_start();
    let low = t.to_ascii_lowercase();
    if !low.starts_with("explain") {
        return sql;
    }
    let rest_t = t["explain".len()..].trim_start();
    if let Some(after) = rest_t.strip_prefix('(')
        && let Some(end) = after.find(')')
    {
        return after[end + 1..].trim_start();
    }
    let low2 = rest_t.to_ascii_lowercase();
    for opt in ["format=json", "format=traditional", "format=tree"] {
        if let Some(after) = low2.strip_prefix(opt) {
            // 按字节长度切回原串（rest_t 与 low2 等长，ASCII 前缀安全）。
            return rest_t[after.len()..].trim_start();
        }
    }
    rest_t
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

    // PG 文档标准 bitmap 计划：`Bitmap Index Scan on <索引名>` 的 on 命名的是**索引**
    // （无 using 子句）——索引名必须进 accessPath 而非 relation（父 Bitmap Heap Scan
    // 才携带表名）。终审 Important-1 钉死。
    const SAMPLE_BITMAP: &str = "Bitmap Heap Scan on oj_explain_parent  (cost=4.36..88.54 rows=12 width=8)\n  Recheck Cond: (id < 100)\n  ->  Bitmap Index Scan on oj_explain_parent_pkey  (cost=0.00..4.36 rows=12 width=0)\n        Index Cond: (id < 100)\n";

    #[test]
    fn pg_text_bitmap_index_scan_names_index_not_relation() {
        let p = super::normalize_pg_text_plan(SAMPLE_BITMAP);
        assert_eq!(p["operation"], "Bitmap Heap Scan");
        assert_eq!(p["relation"], "oj_explain_parent");
        let c = &p["children"][0];
        assert_eq!(c["operation"], "Bitmap Index Scan");
        assert_eq!(
            c["accessPath"], "oj_explain_parent_pkey",
            "Bitmap Index Scan 的 on 是索引名，必须进 accessPath"
        );
        assert_eq!(
            c["relation"],
            serde_json::Value::Null,
            "索引名不得错标为 relation（表在父节点）"
        );
        assert_eq!(c["extra"], "Index Cond: (id < 100)");
        assert_eq!(p["extra"], "Recheck Cond: (id < 100)");
    }

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

    // 以下四份样本为 oj-test-mysql 8.4 真机捕获（v0.1.108），字段名/值逐字节保真
    //（仅裁剪 used_columns/possible_keys 等长数组，不碰结构）。归一规则：
    // operation=access_type（table 叶）/容器键名（容器）；accessPath=key；
    // rowsEstimate=rows_produced_per_join（对齐 PG 输出行语义）；cost.total=prefix_cost
    // （字符串解析）；cost.startup/width MySQL 恒 null。
    const SAMPLE_SINGLE: &str = r#"{"query_block":{"select_id":1,"cost_info":{"query_cost":"1.00"},"table":{"table_name":"oj_explain_parent","access_type":"const","possible_keys":["PRIMARY"],"key":"PRIMARY","used_key_parts":["id"],"key_length":"4","ref":["const"],"rows_examined_per_scan":1,"rows_produced_per_join":1,"filtered":"100.00","cost_info":{"read_cost":"0.00","eval_cost":"0.10","prefix_cost":"0.00","data_read_per_join":"264"},"used_columns":["id","name"]}}}"#;

    const SAMPLE_JOIN: &str = r#"{"query_block":{"select_id":1,"cost_info":{"query_cost":"200.25"},"nested_loop":[{"table":{"table_name":"p","access_type":"const","possible_keys":["PRIMARY"],"key":"PRIMARY","used_key_parts":["id"],"ref":["const"],"rows_examined_per_scan":1,"rows_produced_per_join":1,"filtered":"100.00","using_index":true,"cost_info":{"prefix_cost":"0.00"},"used_columns":["id"]}},{"table":{"table_name":"c","access_type":"ALL","rows_examined_per_scan":2000,"rows_produced_per_join":200,"filtered":"10.00","cost_info":{"prefix_cost":"200.25"},"used_columns":["id","pid","val"],"attached_condition":"(`ojtest`.`c`.`pid` = 42)"}}]}}"#;

    const SAMPLE_GROUP: &str = r#"{"query_block":{"select_id":1,"cost_info":{"query_cost":"1311.25"},"ordering_operation":{"using_filesort":false,"grouping_operation":{"using_temporary_table":true,"table":{"table_name":"oj_explain_child","access_type":"ALL","rows_examined_per_scan":2000,"rows_produced_per_join":1110,"filtered":"55.55","cost_info":{"prefix_cost":"200.25"},"attached_condition":"(`ojtest`.`oj_explain_child`.`val` like 'v1%')"}}}}}"#;

    const SAMPLE_UNION: &str = r#"{"query_block":{"union_result":{"using_temporary_table":true,"table_name":"<union1,2>","access_type":"ALL","query_specifications":[{"query_block":{"select_id":1,"cost_info":{"query_cost":"1.00"},"table":{"table_name":"oj_explain_parent","access_type":"const","key":"PRIMARY","rows_produced_per_join":1,"filtered":"100.00","cost_info":{"prefix_cost":"0.00"}}}},{"query_block":{"select_id":2,"cost_info":{"query_cost":"1.00"},"table":{"table_name":"oj_explain_child","access_type":"const","key":"PRIMARY","rows_produced_per_join":1,"filtered":"100.00","cost_info":{"prefix_cost":"0.00"}}}}]}}}"#;

    fn mysql_plan(sample: &str) -> serde_json::Value {
        let outer: serde_json::Value = serde_json::from_str(sample).unwrap();
        super::normalize_mysql_plan(&outer)
    }

    #[test]
    fn mysql_single_table_leaf() {
        let p = mysql_plan(SAMPLE_SINGLE);
        assert_eq!(p["operation"], "query_block");
        assert_eq!(p["category"], "other");
        assert_eq!(p["cost"]["total"], 1.0);
        let t = &p["children"][0];
        assert_eq!(t["category"], "scan");
        assert_eq!(t["operation"], "const");
        assert_eq!(t["relation"], "oj_explain_parent");
        assert_eq!(t["accessPath"], "PRIMARY");
        assert_eq!(t["rowsEstimate"], 1);
        assert_eq!(
            t["cost"],
            serde_json::json!({"startup": null, "total": 0.0})
        );
        assert_eq!(t["width"], serde_json::Value::Null);
        let extra = t["extra"].as_str().unwrap();
        assert!(extra.contains("used_key_parts: id"), "{extra}");
        assert!(extra.contains("filtered: 100.00"), "{extra}");
    }

    #[test]
    fn mysql_nested_loop_order_preserved() {
        let p = mysql_plan(SAMPLE_JOIN);
        assert_eq!(p["operation"], "query_block");
        let nl = &p["children"][0];
        assert_eq!(nl["category"], "join");
        assert_eq!(nl["operation"], "nested_loop");
        let c = nl["children"].as_array().unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(c[0]["relation"], "p");
        assert_eq!(c[0]["accessPath"], "PRIMARY");
        assert_eq!(c[1]["relation"], "c");
        assert_eq!(c[1]["operation"], "ALL");
        assert_eq!(c[1]["rowsEstimate"], 200);
        assert!(
            c[1]["extra"]
                .as_str()
                .unwrap()
                .contains("attached_condition"),
            "{}",
            c[1]["extra"]
        );
    }

    #[test]
    fn mysql_groupby_container_pierced() {
        let p = mysql_plan(SAMPLE_GROUP);
        let ord = &p["children"][0];
        assert_eq!(ord["operation"], "ordering_operation");
        let grp = &ord["children"][0];
        assert_eq!(grp["category"], "aggregate");
        assert_eq!(grp["operation"], "grouping_operation");
        let t = &grp["children"][0];
        assert_eq!(t["relation"], "oj_explain_child");
        assert_eq!(t["rowsEstimate"], 1110);
        assert!(t["extra"].as_str().unwrap().contains("attached_condition"));
    }

    // ---- op 层（Task 5）：bridge 级行为 ----

    async fn sqlite_bridge(extra: Option<crate::bridge::Extras>) -> crate::bridge::Bridge {
        let db = crate::bridge::SqlxAccessor::arc("sqlite::memory:")
            .await
            .unwrap();
        crate::bridge::Bridge::with_dbs_and_loader(
            std::collections::HashMap::from([("default".to_string(), db as _)]),
            std::sync::Arc::new(crate::bridge::InMemoryKV::new()),
            crate::bridge::SchemaRegistry::new(),
            false,
            None,
            extra.unwrap_or_default(),
        )
    }

    async fn run_explain_js(
        b: &crate::bridge::Bridge,
        req: crate::bridge::RequestInfo,
        inner: &str,
    ) -> serde_json::Value {
        let js = format!(
            r#"(async () => {{
                 try {{
                   const r = await db.explain({inner});
                   json.ok({{ ok: true, plan: r.plan }});
                 }} catch (e) {{
                   json.ok({{ ok: false, code: e.code,
                             capability: e.data && e.data.capability, msg: String(e) }});
                 }}
               }})().catch(e => json.fail(500, String(e)))"#
        );
        let cap = b.run_with(&js, req).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        v["data"].clone()
    }

    /// sqlite（及未知方言）：结构化 OJ_DB_UNSUPPORTED，capability=explain，不回落不 panic。
    #[tokio::test(flavor = "current_thread")]
    async fn explain_sqlite_reports_structured_unsupported() {
        let b = sqlite_bridge(None).await;
        let d = run_explain_js(&b, crate::bridge::RequestInfo::default(), r#""select 1""#).await;
        assert_eq!(d["ok"], false, "{d}");
        assert_eq!(d["code"], "OJ_DB_UNSUPPORTED", "{d}");
        assert_eq!(d["capability"], "explain", "{d}");
    }

    /// Deny 租户守卫：EXPLAIN 前缀会绕过首词 fail-closed 分派——内层 SQL 必须先过
    /// check_tenant_raw。`EXPLAIN UPDATE <未注册表>` 必须被拒（且先于方言能力判断）。
    #[tokio::test(flavor = "current_thread")]
    async fn explain_inner_sql_passes_tenant_guard_deny() {
        let b = sqlite_bridge(Some(crate::bridge::Extras {
            sql_guard: crate::bridge::SqlGuard::Deny,
            ..Default::default()
        }))
        .await;
        let req = crate::bridge::RequestInfo {
            tenant_id: Some("t1".into()),
            ..Default::default()
        };
        let d = run_explain_js(&b, req, r#""EXPLAIN UPDATE u SET x = 1""#).await;
        assert_eq!(
            d["ok"], false,
            "Deny 下未注册表的 EXPLAIN UPDATE 必须被拒: {d}"
        );
        assert!(
            d["msg"]
                .as_str()
                .unwrap_or_default()
                .to_lowercase()
                .contains("tenant"),
            "拒绝须来自租户守卫: {d}"
        );
    }

    // select_list / having 子查询容器：与 attached_subqueries 同形（透明数组），
    // 历史上 walker 键表漏收致子树静默丢失——终审 Important-2 钉死。
    const SAMPLE_SUBQUERIES: &str = r#"{"query_block":{"select_id":1,"cost_info":{"query_cost":"2.50"},"table":{"table_name":"oj_explain_parent","access_type":"ALL","rows_produced_per_join":10,"filtered":"100.00","cost_info":{"prefix_cost":"2.50"}},"select_list_subqueries":[{"query_block":{"select_id":2,"table":{"table_name":"oj_explain_child","access_type":"ALL","rows_produced_per_join":5,"filtered":"100.00","cost_info":{"prefix_cost":"1.00"}}}}],"having_subqueries":[{"query_block":{"select_id":3,"table":{"table_name":"oj_explain_child","access_type":"const","key":"PRIMARY","rows_produced_per_join":1,"filtered":"100.00","cost_info":{"prefix_cost":"0.00"}}}}]}}"#;

    #[test]
    fn mysql_select_list_and_having_subqueries_not_dropped() {
        let p = mysql_plan(SAMPLE_SUBQUERIES);
        let ops: Vec<&str> = p["children"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["operation"].as_str().unwrap())
            .collect();
        assert!(
            ops.contains(&"query_block"),
            "select_list/having 子查询不得静默丢失: {ops:?}"
        );
        let sub_rels: Vec<&str> = p["children"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["operation"] == "query_block")
            .map(|c| c["children"][0]["relation"].as_str().unwrap())
            .collect();
        assert_eq!(sub_rels.len(), 2);
        assert_eq!(sub_rels[0], "oj_explain_child");
    }

    #[test]
    fn mysql_union_arms_are_children() {
        let p = mysql_plan(SAMPLE_UNION);
        let ur = &p["children"][0];
        assert_eq!(ur["operation"], "union_result");
        assert_eq!(ur["relation"], "<union1,2>");
        let arms = ur["children"].as_array().unwrap();
        assert_eq!(arms.len(), 2);
        assert_eq!(arms[0]["operation"], "query_block");
        assert_eq!(arms[0]["children"][0]["relation"], "oj_explain_parent");
        assert_eq!(arms[1]["children"][0]["relation"], "oj_explain_child");
    }
}
