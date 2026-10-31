//! U-14（v0.1.109）反向 DDL：`db.ddlDiff(spec)` —— **纯渲染器，零 DB I/O**。
//!
//! 产出结构化 diff（PR-9 比较视图直接渲染）+ 有序可重放 statements。
//! 执行仍走 `db.exec` + `allow_ddl`——本模块不执行任何东西，红线不破。
//!
//! 注入防线（评审钉死）：
//! - 标识符只做**方言引号 + 翻倍**（PG `"`→`""`、MySQL 反引号→双双反引号）——
//!   **绝不复用** `mysql_show_create` 的白名单字符集（合法名可含非 ASCII/空格）；
//!   NUL / 控制字符直接拒。
//! - verbatim 片段字段（nativeType / default / definition）拒含 `;`——合法片段
//!   不含语句分隔符（经 db.exec 重放时防语句拆分）。
//!
//! 数据安全语义（钉死）：维度字段**缺省 = 不参与 diff**；**空数组 = 期望为空**
//! （出 DROP）。省略 `indexes` 绝不生成 DROP INDEX 全家桶。
//! identity 列跳过 default 文本比对（nextval 是 identity 的派生物，否则恒假 changed）。

use serde_json::{Value, json};

use super::db::Dialect;
use std::cell::RefCell;
use std::rc::Rc;

use deno_core::{OpState, op2};
use deno_error::JsErrorBox;

use super::db::{Target, require_known_dialect, resolve_target, to_js_err};

/// 方言引号 + 翻倍。NUL / 控制字符（< 0x20）→ Err（注入防线，不做字符集白名单）。
pub fn quote_ident(d: Dialect, s: &str) -> Result<String, String> {
    if s.is_empty() {
        return Err("ddlDiff: identifier must not be empty".into());
    }
    if s.bytes().any(|b| b == 0 || b < 0x20) {
        return Err(format!(
            "ddlDiff: identifier contains NUL/control char: {s:?}"
        ));
    }
    Ok(match d {
        Dialect::MySql => format!("`{}`", s.replace('`', "``")),
        _ => format!("\"{}\"", s.replace('"', "\"\"")),
    })
}

/// verbatim 片段（nativeType / default / definition）校验：拒 `;` 与 NUL/换行——
/// 合法片段不含语句分隔符。
pub fn check_fragment(s: &str) -> Result<(), String> {
    if s.bytes()
        .any(|b| b == 0 || b == b';' || b == b'\n' || b == b'\r')
    {
        return Err(format!(
            "ddlDiff: SQL fragment contains ';'/newline/NUL: {s:?}"
        ));
    }
    Ok(())
}

/// 字面量（COMMENT 文本）引用：单引号翻倍（两方言同）。
fn quote_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

// ---------------------------------------------------------------------------
// diff 主流程
// ---------------------------------------------------------------------------

/// Structure 列条目（与已发布 listColumns 同词汇 + identity/comment/previousName 可选）。
#[derive(Debug, Clone)]
struct ColSpec {
    name: String,
    native_type: String,
    nullable: bool,
    default: Option<String>,
    primary_key: bool,
    identity: Option<String>,
    comment: Option<String>,
    /// REQ-13：to 侧声明「库里当前叫这个名字」→ 判 rename。
    previous_name: Option<String>,
}

fn parse_columns(dim: &str, v: Option<&Value>) -> Result<Option<Vec<ColSpec>>, String> {
    let Some(arr) = v else { return Ok(None) };
    let arr = arr
        .as_array()
        .ok_or_else(|| format!("ddlDiff: {dim}.columns must be an array"))?;
    let mut out = Vec::new();
    for c in arr {
        let name = c
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("ddlDiff: {dim}.columns[] missing name"))?
            .to_string();
        quote_ident(Dialect::Postgres, &name)?;
        let nt = c
            .get("nativeType")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("ddlDiff: column {name:?} missing nativeType"))?;
        check_fragment(nt)?;
        let default = match c.get("default").and_then(Value::as_str) {
            Some(d) => {
                check_fragment(d)?;
                Some(d.to_string())
            }
            None => None,
        };
        out.push(ColSpec {
            name,
            native_type: nt.to_string(),
            nullable: c.get("nullable").and_then(Value::as_bool).unwrap_or(true),
            default,
            primary_key: c
                .get("primaryKey")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            identity: c.get("identity").and_then(Value::as_str).map(String::from),
            comment: c.get("comment").and_then(Value::as_str).map(String::from),
            // REQ-13：只认 to 侧；from 侧出现 previousName 一律忽略（含免校验）。
            previous_name: if dim == "to" {
                match c.get("previousName").and_then(Value::as_str) {
                    Some(p) => {
                        quote_ident(Dialect::Postgres, p)?;
                        Some(p.to_string())
                    }
                    None => None,
                }
            } else {
                None
            },
        });
    }
    Ok(Some(out))
}

fn parse_named_defs(
    dim: &str,
    v: Option<&Value>,
    types: Option<&[&str]>,
) -> Result<Option<Vec<Value>>, String> {
    let Some(arr) = v else { return Ok(None) };
    let arr = arr
        .as_array()
        .ok_or_else(|| format!("ddlDiff: {dim} must be an array"))?;
    for e in arr {
        let name = e
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("ddlDiff: {dim}[] missing name"))?;
        quote_ident(Dialect::Postgres, name)?;
        if let Some(ty) = e.get("type").and_then(Value::as_str)
            && let Some(allowed) = types
            && !allowed.contains(&ty)
        {
            return Err(format!("ddlDiff: {dim}[] unknown type {ty:?}"));
        } else if types.is_some() && e.get("type").and_then(Value::as_str).is_none() {
            return Err(format!("ddlDiff: {dim}[] missing type"));
        }
        if let Some(def) = e.get("definition").and_then(Value::as_str) {
            check_fragment(def)?;
        }
        for k in ["columns", "refColumns"] {
            if let Some(members) = e.get(k).and_then(Value::as_array) {
                for m in members {
                    let s = m
                        .as_str()
                        .ok_or_else(|| format!("ddlDiff: {dim}[] {k} must be strings"))?;
                    quote_ident(Dialect::Postgres, s)?;
                }
            }
        }
    }
    Ok(Some(arr.clone()))
}

/// 词边界标识符替换（[A-Za-z0-9_$] 为标识符字符）——只用于 diff 比较，不进语句渲染。
/// 已知窄窗：definition 字符串字面量内含与旧列名同形的词会被一并替换，
/// 可能把「真变更」判成未变而漏发 DDL——devkit 登记，重放后人工核对此类约束。
fn subst_idents(text: &str, map: &std::collections::HashMap<&str, &str>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut cur = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' {
            cur.push(ch);
        } else {
            if !cur.is_empty() {
                out.push_str(map.get(cur.as_str()).copied().unwrap_or(cur.as_str()));
                cur.clear();
            }
            out.push(ch);
        }
    }
    if !cur.is_empty() {
        out.push_str(map.get(cur.as_str()).copied().unwrap_or(cur.as_str()));
    }
    out
}

/// REQ-13：rename 计划——只认 to 侧显式 previousName；启发式猜测不进自动路径。
/// 六边界校验（REQ-13 §4.3），返回 (旧名, 新名) 对子（spec 顺序）。
fn plan_renames(from: &[ColSpec], to: &[ColSpec]) -> Result<Vec<(String, String)>, String> {
    let mut out: Vec<(String, String)> = Vec::new();
    let from_names: std::collections::HashSet<&str> =
        from.iter().map(|c| c.name.as_str()).collect();
    for tc in to {
        let Some(p) = tc.previous_name.as_deref() else {
            continue;
        };
        if p == tc.name {
            continue; // previousName === name → 普通同名列
        }
        if !from_names.contains(p) {
            return Err(format!(
                "ddlDiff: to.columns[].previousName {p:?} not found in from.columns"
            ));
        }
        if out.iter().any(|(f, _)| f == p) {
            return Err(format!(
                "ddlDiff: to.columns[].previousName {p:?} is claimed by multiple columns"
            ));
        }
        // 目标名冲突：from 已有同名列，且该列没有被其它 rename 换走。
        if from_names.contains(tc.name.as_str())
            && !to
                .iter()
                .any(|x| x.previous_name.as_deref() == Some(tc.name.as_str()))
        {
            return Err(format!(
                "ddlDiff: rename target {:?} conflicts with an existing column",
                tc.name
            ));
        }
        out.push((p.to_string(), tc.name.clone()));
    }
    for tc in to {
        if tc.previous_name.is_none() && out.iter().any(|(f, _)| *f == tc.name) {
            return Err(format!(
                "ddlDiff: column {:?} occupies a name being renamed away",
                tc.name
            ));
        }
    }
    for (f, t) in &out {
        if out.iter().any(|(f2, t2)| f2 == t && t2 == f) {
            return Err(
                "ddlDiff: rename swap is not supported (capability=ddl_diff_rename_swap)".into(),
            );
        }
    }
    Ok(out)
}

fn str_field(e: &Value, k: &str) -> Option<String> {
    e.get(k).and_then(Value::as_str).map(String::from)
}

fn col_diff_entries(from: &[ColSpec], to: &[ColSpec]) -> Vec<Value> {
    let mut out = Vec::new();
    for tc in to {
        let Some(fc) = from.iter().find(|f| f.name == tc.name) else {
            continue;
        };
        let mut push = |field: &str, from: Value, to: Value| {
            out.push(json!({"name": tc.name, "field": field, "from": from, "to": to}));
        };
        if fc.native_type != tc.native_type {
            push("nativeType", json!(fc.native_type), json!(tc.native_type));
        }
        if fc.nullable != tc.nullable {
            push("nullable", json!(fc.nullable), json!(tc.nullable));
        }
        if fc.identity != tc.identity {
            push(
                "identity",
                fc.identity
                    .clone()
                    .map(Value::String)
                    .unwrap_or(Value::Null),
                tc.identity
                    .clone()
                    .map(Value::String)
                    .unwrap_or(Value::Null),
            );
        }
        // identity 列跳 default 文本比对（nextval 派生，恒假 changed）。
        let identity_col = fc.identity.is_some() || tc.identity.is_some();
        if !identity_col && fc.default != tc.default {
            push(
                "default",
                fc.default.clone().map(Value::String).unwrap_or(Value::Null),
                tc.default.clone().map(Value::String).unwrap_or(Value::Null),
            );
        }
        if fc.comment != tc.comment {
            push(
                "comment",
                fc.comment.clone().map(Value::String).unwrap_or(Value::Null),
                tc.comment.clone().map(Value::String).unwrap_or(Value::Null),
            );
        }
    }
    out
}

/// 列在数组中的前驱名（位置 diff 的 from/to 值；首位 = "<first>"）。
fn prev_name(cols: &[ColSpec], name: &str) -> String {
    let pos = cols.iter().position(|c| c.name == name).unwrap_or(0);
    if pos == 0 {
        "<first>".into()
    } else {
        cols[pos - 1].name.clone()
    }
}

fn col_def_sql(d: Dialect, c: &ColSpec, after: Option<&str>) -> String {
    let mut s = format!(
        "{} {}",
        quote_ident(d, &c.name).unwrap_or_else(|_| c.name.clone()),
        c.native_type
    );
    if let Some(id) = &c.identity {
        s.push_str(match id.as_str() {
            "always" => " GENERATED ALWAYS AS IDENTITY",
            _ => " GENERATED BY DEFAULT AS IDENTITY",
        });
    } else if !c.nullable {
        s.push_str(" NOT NULL");
    }
    if c.identity.is_none()
        && let Some(def) = &c.default
    {
        s.push_str(&format!(" DEFAULT {def}"));
    }
    if let Some(cm) = &c.comment
        && d == Dialect::MySql
    {
        s.push_str(&format!(" COMMENT {}", quote_literal(cm)));
    }
    if d == Dialect::MySql
        && let Some(a) = after
    {
        if a == "<first>" {
            s.push_str(" FIRST");
        } else {
            s.push_str(&format!(
                " AFTER {}",
                quote_ident(d, a).unwrap_or_else(|_| a.to_string())
            ));
        }
    }
    s
}

/// 语句体校验（REQ-09 §4.6 信任类澄清）：整条语句级文本（definition/partitionBy）
/// 无嵌入分裂面——拒 NUL、限长 64 KiB，**不拒 `;`/换行**（与嵌入片段的
/// `check_fragment` 不同档）。重放走 prepared 协议，多语句被服务端拒。
pub fn validate_statement_body(field: &str, s: &str) -> Result<(), String> {
    if s.bytes().any(|b| b == 0) || s.len() > 64 * 1024 {
        return Err(format!("ddlDiff: {field} contains NUL or exceeds 64 KiB"));
    }
    Ok(())
}

/// G2（REQ-09 §4.3）：分区/表选项变更 → 结构化 unsupported 的 capability 判据。
/// 仅 objectType=table 时判（其余对象类型无该维度）；差异定义 = 一侧有一侧无，
/// 或两侧文本/值不同。None = 可走正常 diff。
pub fn unsupported_capability_from_spec(d: Dialect, spec: &Value) -> Option<&'static str> {
    if spec.get("objectType").and_then(Value::as_str) != Some("table") {
        return None;
    }
    let from = spec.get("from").filter(|v| !v.is_null());
    let to = spec.get("to").filter(|v| !v.is_null());
    // 仅 alter 路径判定（两侧皆非 null）：create（from=null）由渲染路径消费这些字段，
    // drop（to=null）无 diff 可言——都不是「变更」。
    if from.is_none() || to.is_none() {
        return None;
    }
    // options 非对象 = 畸形输入，留给 ddl_diff 参数错误（不做 capability 判等）。
    if from
        .and_then(|v| v.get("options"))
        .is_some_and(|v| !v.is_object())
        || to
            .and_then(|v| v.get("options"))
            .is_some_and(|v| !v.is_object())
    {
        return None;
    }
    fn part(side: Option<&Value>) -> Option<&str> {
        side.and_then(|v| v.get("partitionBy"))
            .and_then(Value::as_str)
    }
    if part(from) != part(to) {
        return Some("ddl_diff_partition");
    }
    // Null 规范化：{"comment":null} 与缺键同义——剥掉 Null 值再判等，避免误报。
    fn opts(side: Option<&Value>) -> Option<serde_json::Map<String, Value>> {
        side.and_then(|v| v.get("options"))
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .filter(|(_, v)| !v.is_null())
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            })
    }
    if opts(from) != opts(to) {
        return Some("ddl_diff_table_options");
    }
    let _ = d;
    None
}

/// G3（REQ-09 §4.4）：非表对象 diff——view 支持 create/diff/drop；index/sequence/
/// routine/trigger 只支持 create（from=null）与 drop（to=null），不做逐字段 diff。
fn ddl_diff_object(d: Dialect, spec: &Value) -> Result<Value, String> {
    let ty = spec.get("objectType").and_then(Value::as_str).unwrap_or("");
    let name = spec
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| "ddlDiff: missing name".to_string())?;
    let t = quote_ident(d, name)?;
    let t = match spec.get("schema").and_then(Value::as_str) {
        Some(s) => format!(
            "{}.{t}",
            quote_ident(d, s).map_err(|e| format!("ddlDiff: invalid schema: {e}"))?
        ),
        None => t,
    };
    let from = spec.get("from").filter(|v| !v.is_null());
    let to = spec.get("to").filter(|v| !v.is_null());
    let def_of = |side: Option<&Value>| -> Result<Option<String>, String> {
        match side.and_then(|v| v.get("definition")) {
            None => Ok(None),
            Some(Value::String(s)) => {
                validate_statement_body("definition", s)?;
                Ok(Some(s.clone()))
            }
            Some(_) => Err("ddlDiff: definition must be a string".into()),
        }
    };
    let mut stmts: Vec<String> = Vec::new();
    let mut diff = serde_json::Map::new();
    match (from, to) {
        // create（from=null）
        (None, Some(to_v)) => {
            let Some(def) = def_of(Some(to_v))? else {
                return Err(format!("ddlDiff: to.definition is required for {ty}"));
            };
            stmts.push(if ty == "view" {
                format!("CREATE VIEW {t} AS {def}")
            } else {
                // index/sequence/routine/trigger：definition 即整条建对象语句。
                def
            });
        }
        // drop（to=null）
        (Some(from_v), None) => match ty {
            "view" => stmts.push(format!("DROP VIEW {t}")),
            "index" => {
                if d == Dialect::MySql {
                    let tbl = from_v.get("table").and_then(Value::as_str).ok_or_else(|| {
                        "ddlDiff: from.table is required for mysql index drop".to_string()
                    })?;
                    let t_tbl = quote_ident(d, tbl)?;
                    stmts.push(format!("DROP INDEX {t} ON {t_tbl}"));
                } else {
                    stmts.push(format!("DROP INDEX {t}"));
                }
            }
            // 评审 minor：MySQL 无序列对象——渲染 DROP SEQUENCE 必 1064，参数错误。
            "sequence" if d == Dialect::MySql => {
                return Err("ddlDiff: sequences are not supported on mysql".into());
            }
            "sequence" => stmts.push(format!("DROP SEQUENCE {t}")),
            "routine" => {
                let kw = match from_v.get("type").and_then(Value::as_str) {
                    Some("procedure") => "PROCEDURE",
                    Some("function") | None => "FUNCTION",
                    Some(other) => {
                        return Err(format!("ddlDiff: unknown routine type {other:?}"));
                    }
                };
                // 评审 I-2：签名是 PG 专属语法（DROP FUNCTION name(args)）；MySQL
                // DROP FUNCTION/PROCEDURE 只接裸名——给签名反而 1064。
                if d == Dialect::MySql {
                    stmts.push(format!("DROP {kw} {t}"));
                } else {
                    let args = from_v.get("args").and_then(Value::as_str).ok_or_else(|| {
                        "ddlDiff: from.args is required for routine drop".to_string()
                    })?;
                    stmts.push(format!("DROP {kw} {t}({args})"));
                }
            }
            "trigger" => stmts.push(format!("DROP TRIGGER {t}")),
            _ => unreachable!(),
        },
        // diff：view 只比 definition 文本；其余四型不做逐字段 diff。
        (Some(from_v), Some(to_v)) => {
            if ty != "view" {
                return Err(format!(
                    "ddlDiff: {ty} does not support field-level diff (create/drop only)"
                ));
            }
            let f = def_of(Some(from_v))?.unwrap_or_default();
            let tdef = def_of(Some(to_v))?.unwrap_or_default();
            if f != tdef {
                diff.insert("definition".into(), json!({"from": f, "to": tdef}));
                let kw = match d {
                    Dialect::Postgres => "CREATE OR REPLACE VIEW",
                    Dialect::MySql => "ALTER VIEW",
                    Dialect::Sqlite => unreachable!("sqlite rejected at op level"),
                };
                stmts.push(format!("{kw} {t} AS {tdef}"));
            }
        }
        (None, None) => {
            return Err("ddlDiff: missing from/to (both null)".into());
        }
    }
    // renamed 键恒在（评审 minor：信封 schema 与 table 路径同稳定；对象路径无改名语义）。
    diff.insert("renamed".into(), json!([]));
    Ok(json!({"diff": Value::Object(diff), "statements": stmts, "warnings": []}))
}

/// diff + 渲染。spec：`{objectType:"table", name, from: Structure|null, to: Structure}`。
pub fn ddl_diff(d: Dialect, spec: &Value) -> Result<Value, String> {
    // G3（REQ-09 §4.4）：objectType 扩六值——非表对象走独立分发。
    match spec.get("objectType").and_then(Value::as_str) {
        Some("table") => {}
        Some("view" | "index" | "sequence" | "routine" | "trigger") => {
            return ddl_diff_object(d, spec);
        }
        _ => {
            return Err(
                "ddlDiff: objectType must be \"table\"|\"view\"|\"index\"|\"sequence\"|\"routine\"|\"trigger\""
                    .into(),
            );
        }
    }
    let name = spec
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| "ddlDiff: missing name".to_string())?;
    let t = quote_ident(d, name)?;
    // REQ-09-2 schema 定向：可选独立 schema 段 → 表引用两段引用渲染（语句全走 t；
    // diff 输出键仍用裸 name，零变化）。缺省行为与既有测试钉死的一致。
    let t = match spec.get("schema").and_then(Value::as_str) {
        Some(s) => format!(
            "{}.{t}",
            quote_ident(d, s).map_err(|e| format!("ddlDiff: invalid schema: {e}"))?
        ),
        None => t,
    };
    let to = spec
        .get("to")
        .ok_or_else(|| "ddlDiff: missing to".to_string())?;
    // G3 边界（builder-check Q4）：table 的 to:null 曾是静默零语句——正是本批要消灭
    // 的形态。DROP TABLE 是独立高危语义，不进 ddlDiff；显式参数错误。
    if to.is_null() {
        return Err("ddlDiff: to must not be null for objectType \"table\"".into());
    }
    // G4（REQ-09 §4.1 表改名缺口）：spec 顶层可选 previousName——与 name 相同或
    // create 路径（from=null）时忽略（同列 rename 同名跳过语义）。t_bare = RENAME TO
    // 目标（裸标识符）；t_old = 旧引用（schema 段两段限定）。
    let (table_rename, t_bare): (Option<(&str, String)>, String) = {
        let t_bare = quote_ident(d, name)?;
        match (
            spec.get("previousName").and_then(Value::as_str),
            spec.get("from").filter(|v| !v.is_null()),
        ) {
            (Some(p), Some(_)) if p != name => {
                let t_old = quote_ident(d, p)?;
                let t_old = match spec.get("schema").and_then(Value::as_str) {
                    Some(s) => format!(
                        "{}.{t_old}",
                        quote_ident(d, s).map_err(|e| format!("ddlDiff: invalid schema: {e}"))?
                    ),
                    None => t_old,
                };
                (Some((p, t_old)), t_bare)
            }
            _ => (None, t_bare),
        }
    };
    let from_v = spec.get("from");

    let from_cols = parse_columns("from", from_v.and_then(|v| v.get("columns")))?;
    let to_cols = parse_columns("to", to.get("columns"))?;
    let from_cons = parse_named_defs(
        "from.constraints",
        from_v.and_then(|v| v.get("constraints")),
        Some(&["pk", "uq", "ck"]),
    )?;
    let to_cons = parse_named_defs(
        "to.constraints",
        to.get("constraints"),
        Some(&["pk", "uq", "ck"]),
    )?;
    let from_idxs = parse_named_defs("from.indexes", from_v.and_then(|v| v.get("indexes")), None)?;
    let to_idxs = parse_named_defs("to.indexes", to.get("indexes"), None)?;
    let from_fks = parse_named_defs(
        "from.foreignKeys",
        from_v.and_then(|v| v.get("foreignKeys")),
        None,
    )?;
    let to_fks = parse_named_defs("to.foreignKeys", to.get("foreignKeys"), None)?;

    // G2（REQ-09 §4.3）：分区/表选项变更显式 unsupported——fail-fast 在列 diff 之前，
    // 不出半截 statements。op 层另有结构化通道（两层同判据）。
    if let Some(c) = unsupported_capability_from_spec(d, spec) {
        return Err(format!("ddlDiff: unsupported capability {c}"));
    }
    // options 非对象 = 参数错误（评审 minor：原静默忽略）；各键值过语句体校验。
    for side in ["from", "to"] {
        if let Some(o) = spec
            .get(side)
            .and_then(|v| v.get("options"))
            .filter(|v| !v.is_null())
        {
            let m = o
                .as_object()
                .ok_or_else(|| format!("ddlDiff: {side}.options must be an object"))?;
            for (k, v) in m {
                if let Some(s) = v.as_str() {
                    validate_statement_body(&format!("{side}.options.{k}"), s)?;
                } else if !v.is_null() {
                    return Err(format!("ddlDiff: options.{k} must be a string"));
                }
            }
        }
    }

    // identity 是 PG 专属能力：MySQL 结构里出现 → 参数错误（GENERATED AS IDENTITY 非 MySQL 语法）。
    if d == Dialect::MySql {
        for c in from_cols.iter().flatten().chain(to_cols.iter().flatten()) {
            if c.identity.is_some() {
                return Err(
                    "ddlDiff: identity columns are postgres-only; not supported on mysql".into(),
                );
            }
        }
    }

    // ---------------- REQ-13 rename 计划 ----------------
    let renames: Vec<(String, String)> = match (from_cols.as_deref(), to_cols.as_deref()) {
        (Some(f), Some(t)) => plan_renames(f, t)?,
        // 列维度任一端缺省 = 不参与 diff，previousName 同样不生效。
        _ => Vec::new(),
    };
    let rename_map: std::collections::HashMap<&str, &str> = renames
        .iter()
        .map(|(f, t)| (f.as_str(), t.as_str()))
        .collect();
    // from 侧列名对齐到新名：added/removed/changed 按新名配对，renamed-from 不再参与删/增。
    let from_cols_adj: Option<Vec<ColSpec>> = from_cols.as_ref().map(|cs| {
        cs.iter()
            .map(|c| match rename_map.get(c.name.as_str()) {
                Some(new) => ColSpec {
                    name: new.to_string(),
                    ..c.clone()
                },
                None => c.clone(),
            })
            .collect()
    });
    // 命名维度的列成员随改名对齐——PG/MySQL RENAME 自动跟随依赖对象，
    // 对齐后未变的维度产零语句（不 DROP+重建）。
    let subst_dim = |defs: &Option<Vec<Value>>| -> Option<Vec<Value>> {
        defs.as_ref().map(|arr| {
            arr.iter()
                .map(|e| {
                    let mut e2 = e.clone();
                    // definition 片段同样随改名对齐（比较口径；语句永远渲染 to 侧原文）。
                    if let Some(def) = e2.get("definition").and_then(Value::as_str) {
                        e2["definition"] = json!(subst_idents(def, &rename_map));
                    }
                    for k in ["columns", "refColumns"] {
                        if let Some(members) = e2.get(k).and_then(Value::as_array) {
                            let sub: Vec<Value> = members
                                .iter()
                                .map(|m| match m.as_str() {
                                    Some(s) => rename_map
                                        .get(s)
                                        .map(|n| json!(n))
                                        .unwrap_or_else(|| m.clone()),
                                    None => m.clone(),
                                })
                                .collect();
                            e2[k] = json!(sub);
                        }
                    }
                    e2
                })
                .collect()
        })
    };
    let from_cons = subst_dim(&from_cons);
    let from_idxs = subst_dim(&from_idxs);
    let from_fks = subst_dim(&from_fks);

    let mut diff = serde_json::Map::new();
    let mut stmts: Vec<String> = Vec::new();
    // ---------------- 新建表（from = null）----------------
    if from_v.is_none() || from_v == Some(&Value::Null) {
        let Some(t_cols) = to_cols.as_ref() else {
            return Err("ddlDiff: to.columns is required when from is null".into());
        };
        let pk: Vec<&str> = t_cols
            .iter()
            .filter(|c| c.primary_key)
            .map(|c| c.name.as_str())
            .collect();
        let defs: Vec<String> = t_cols
            .iter()
            .map(|c| {
                // create 路径：列位置由声明顺序天然决定——FIRST/AFTER 是 ALTER 专属
                // 语法，CREATE TABLE 渲染会让整条语句不可重放（逆运算真库腿暴露）。
                col_def_sql(d, c, None)
            })
            .collect();
        let pk_inline = if pk.is_empty() {
            String::new()
        } else {
            let cols = pk
                .iter()
                .map(|c| quote_ident(d, c).unwrap_or_else(|_| c.to_string()))
                .collect::<Vec<_>>()
                .join(", ");
            format!(",\n  PRIMARY KEY ({cols})")
        };
        // G1（REQ-09 §4.2）：建表保真——MySQL options 与 partitionBy 拼在闭合括号
        // 之后。**选项在前、分区在后**（同 SHOW CREATE 序）：MySQL 语法拒绝跟在分区
        // 定义列表之后的表选项（逆运算真库腿实证 1064）。语句体信任类校验（NUL/限长），
        // 标识符仍全走 quote_ident。
        let mut opts_tail = String::new();
        if let Some(o) = to.get("options") {
            if d != Dialect::MySql {
                return Err("ddlDiff: options are mysql-only".into());
            }
            let str_opt = |k: &str| -> Result<Option<String>, String> {
                match o.get(k) {
                    None | Some(Value::Null) => Ok(None),
                    Some(Value::String(s)) => Ok(Some(s.clone())),
                    Some(_) => Err(format!("ddlDiff: options.{k} must be a string")),
                }
            };
            for (k, v) in o.as_object().iter().flat_map(|m| m.iter()) {
                if !matches!(v, Value::Null) && v.as_str().is_none() {
                    return Err(format!("ddlDiff: unknown or non-string options key {k:?}"));
                }
            }
            if let Some(e) = str_opt("engine")? {
                opts_tail.push_str(&format!(" ENGINE={e}"));
            }
            if let Some(c) = str_opt("charset")? {
                opts_tail.push_str(&format!(" DEFAULT CHARSET={c}"));
            }
            if let Some(c) = str_opt("collate")? {
                opts_tail.push_str(&format!(" COLLATE={c}"));
            }
            if let Some(c) = str_opt("comment")? {
                opts_tail.push_str(&format!(" COMMENT={}", quote_literal(&c)));
            }
        }
        let mut tail = opts_tail;
        if let Some(p) = to.get("partitionBy").and_then(Value::as_str) {
            validate_statement_body("partitionBy", p)?;
            tail.push_str(&format!(" PARTITION BY {p}"));
        }
        stmts.push(format!(
            "CREATE TABLE {t} (\n  {}{}\n){tail}",
            defs.join(",\n  "),
            pk_inline
        ));
        if let Some(cons) = &to_cons {
            for c in cons {
                stmts.push(format!(
                    "ALTER TABLE {t} ADD CONSTRAINT {} {}",
                    quote_ident(d, str_field(c, "name").as_deref().unwrap_or(""))?,
                    str_field(c, "definition").unwrap_or_default()
                ));
            }
        }
        if let Some(idxs) = &to_idxs {
            for i in idxs {
                let cols = i
                    .get("columns")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let col_sql = cols
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|c| quote_ident(d, c).unwrap_or_else(|_| c.to_string()))
                    .collect::<Vec<_>>()
                    .join(", ");
                let uniq = i.get("unique").and_then(Value::as_bool).unwrap_or(false);
                stmts.push(format!(
                    "CREATE {}INDEX {} ON {} ({col_sql})",
                    if uniq { "UNIQUE " } else { "" },
                    quote_ident(d, str_field(i, "name").as_deref().unwrap_or(""))?,
                    t
                ));
            }
        }
        if let Some(fks) = &to_fks {
            for f in fks {
                stmts.push(fk_add_sql(d, &t, f)?);
            }
        }
        // diff 形状与 alter 路径一致：四维 added/removed/changed（建表 = 全 added）。
        let dim = |added: Vec<Value>| json!({"added": added, "removed": [], "changed": []});
        let names = |v: &Option<Vec<Value>>| -> Vec<Value> {
            v.as_ref()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|e| str_field(e, "name").map(Value::String))
                        .collect()
                })
                .unwrap_or_default()
        };
        diff.insert(
            "columns".into(),
            dim(t_cols.iter().map(|c| json!(c.name)).collect()),
        );
        diff.insert("constraints".into(), dim(names(&to_cons)));
        diff.insert("indexes".into(), dim(names(&to_idxs)));
        diff.insert("foreignKeys".into(), dim(names(&to_fks)));
        // 建表路径无 from → 恒无告警/无改名；warnings/renamed 键恒在（schema 稳定）。
        diff.insert("renamed".into(), json!([]));
        // 建表路径无 from → 恒无告警；warnings 键恒在（schema 稳定）。
        return Ok(json!({
            "diff": Value::Object(diff),
            "statements": stmts,
            "warnings": Vec::<String>::new(),
        }));
    }

    let mut push_dim = |key: &str, added: Vec<Value>, removed: Vec<Value>, changed: Vec<Value>| {
        diff.insert(
            key.to_string(),
            json!({ "added": added, "removed": removed, "changed": changed }),
        );
    };

    // ---------------- 维度缺省 = 不参与 diff；空数组 = DROP 全量 ----------------
    // 列。
    // 列维度任一端缺省 = 不参与（与具名维度同语义——否则缺省端会被当空表全量 ADD/DROP）。
    let added_c: Vec<Value> = match (&from_cols_adj, &to_cols) {
        (Some(f), Some(t)) => t
            .iter()
            .filter(|c| !f.iter().any(|x| x.name == c.name))
            .map(|c| json!(c.name))
            .collect(),
        _ => Vec::new(),
    };
    let removed_c: Vec<Value> = match (&from_cols_adj, &to_cols) {
        (Some(f), Some(t)) => f
            .iter()
            .filter(|c| !t.iter().any(|x| x.name == c.name))
            .map(|c| json!(c.name))
            .collect(),
        _ => Vec::new(),
    };
    let mut changed_c = match (&from_cols_adj, &to_cols) {
        (Some(f), Some(t)) => col_diff_entries(f, t),
        _ => Vec::new(),
    };
    // rename 对子进 changed（name 条目），语句单独发（RENAME），不进 MODIFY/ALTER dispatch。
    for (f, t) in &renames {
        changed_c.push(json!({"name": t, "field": "name", "from": f, "to": t}));
    }
    // REQ-13 §4.4：显式 rename 已消掉真对子，余下的同型一删一增 = 疑似改名 → 告警不静默。
    // 按 removed 列聚类：一列一条，列名全部同型 added 候选（避免 N×M 叉积刷屏）。
    let mut warnings: Vec<String> = Vec::new();
    if let (Some(fadj), Some(tlist)) = (&from_cols_adj, &to_cols) {
        for r in &removed_c {
            let rn = r.as_str().unwrap_or("");
            let Some(fc) = fadj.iter().find(|c| c.name == rn) else {
                continue;
            };
            let cands: Vec<&str> = added_c
                .iter()
                .filter_map(Value::as_str)
                .filter(|an| {
                    tlist
                        .iter()
                        .find(|c| c.name == *an)
                        .is_some_and(|tc| fc.native_type == tc.native_type)
                })
                .collect();
            if !cands.is_empty() {
                warnings.push(format!(
                    "疑似重命名：{rn} → {}，将按删列 + 建列执行，数据会丢失",
                    cands.join(" / ")
                ));
            }
        }
    }
    // MySQL 位置 diff（PG 无列位置 DDL → 忽略，api-manual 写明）。
    let mut position_changes: Vec<Value> = Vec::new();
    if d == Dialect::MySql
        && let (Some(f), Some(t)) = (&from_cols_adj, &to_cols)
    {
        for tc in t {
            let Some(fc) = f.iter().find(|x| x.name == tc.name) else {
                continue;
            };
            let fp = prev_name(f, &fc.name);
            let tp = prev_name(t, &tc.name);
            if fp != tp {
                position_changes.push(json!({
                    "name": tc.name, "field": "position", "from": fp, "to": tp
                }));
            }
        }
    }
    let mut changed_c_all = changed_c.clone();
    changed_c_all.extend(position_changes.iter().cloned());
    push_dim(
        "columns",
        added_c.clone(),
        removed_c.clone(),
        changed_c_all.clone(),
    );

    // 约束 / 索引 / 外键（名字集合比较；changed = 同名字段值不同）。
    let cons_diff = named_dim_diff(&from_cons, &to_cons);
    push_dim(
        "constraints",
        cons_diff.0.clone(),
        cons_diff.1.clone(),
        cons_diff.2.clone(),
    );
    let idx_diff = named_dim_diff(&from_idxs, &to_idxs);
    push_dim(
        "indexes",
        idx_diff.0.clone(),
        idx_diff.1.clone(),
        idx_diff.2.clone(),
    );
    let fk_diff = named_dim_diff(&from_fks, &to_fks);
    push_dim(
        "foreignKeys",
        fk_diff.0.clone(),
        fk_diff.1.clone(),
        fk_diff.2.clone(),
    );
    // REQ-13：renamed 键恒在（干净为空数组），前端据此渲染「重命名」而非「删 A / 增 B」。
    // 注意须在 push_dim 闭包用完之后（闭包持有 diff 的可变借用）。
    if let Some(Value::Object(m)) = diff.get_mut("columns") {
        m.insert(
            "renamed".into(),
            json!(
                renames
                    .iter()
                    .map(|(f, t)| json!({"from": f, "to": t}))
                    .collect::<Vec<_>>()
            ),
        );
    }

    // ---------------- statements：DROP（FK → 约束 → 索引 → 列）→ 列变更 → ADD ----------------
    let changed_names =
        |diff: &(Vec<Value>, Vec<Value>, Vec<Value>)| -> std::collections::HashSet<String> {
            diff.2
                .iter()
                .filter_map(|e| e["name"].as_str().map(String::from))
                .collect()
        };
    let cons_changed = changed_names(&cons_diff);
    let idx_changed = changed_names(&idx_diff);
    let fk_changed = changed_names(&fk_diff);
    if let Some(fks) = &from_fks {
        // 维度缺省 = 不参与；to 显式数组（可空）→ 名字集合差出 DROP；
        // 同名 changed（定义差）同样须先 DROP，否则重放撞已存在对象。
        let dropped: Vec<&Value> = match &to_fks {
            Some(tv) => fks
                .iter()
                .filter(|f| {
                    let n = str_field(f, "name").unwrap_or_default();
                    !tv.iter()
                        .any(|t| str_field(t, "name").as_deref() == Some(n.as_str()))
                        || fk_changed.contains(&n)
                })
                .collect(),
            None => Vec::new(),
        };
        for f in dropped {
            let n = quote_ident(d, str_field(f, "name").as_deref().unwrap_or(""))?;
            stmts.push(match d {
                Dialect::MySql => format!("ALTER TABLE {t} DROP FOREIGN KEY {n}"),
                _ => format!("ALTER TABLE {t} DROP CONSTRAINT {n}"),
            });
        }
    }
    // 约束 DROP（pk/uq/ck；fk 已在上一步处理，from_cons 词表里无 fk）。
    if let Some(cons) = &from_cons {
        let dropped: Vec<&Value> = match &to_cons {
            Some(tc) => cons
                .iter()
                .filter(|c| {
                    let n = str_field(c, "name").unwrap_or_default();
                    !tc.iter()
                        .any(|t| str_field(t, "name").as_deref() == Some(n.as_str()))
                        || cons_changed.contains(&n)
                })
                .collect(),
            None => Vec::new(),
        };
        for c in dropped {
            let n = quote_ident(d, str_field(c, "name").as_deref().unwrap_or(""))?;
            stmts.push(match d {
                // MySQL 无统一 DROP CONSTRAINT：pk → DROP PRIMARY KEY（无名）、
                // uq → DROP INDEX（支撑索引）、ck → DROP CHECK。
                Dialect::MySql => match str_field(c, "type").as_deref() {
                    Some("pk") => format!("ALTER TABLE {t} DROP PRIMARY KEY"),
                    Some("uq") => format!("ALTER TABLE {t} DROP INDEX {n}"),
                    _ => format!("ALTER TABLE {t} DROP CHECK {n}"),
                },
                _ => format!("ALTER TABLE {t} DROP CONSTRAINT {n}"),
            });
        }
    }
    // 索引 DROP（primary origin 属于约束支撑，不在此处）。
    if let Some(idxs) = &from_idxs {
        let dropped: Vec<&Value> = match &to_idxs {
            Some(ti) => idxs
                .iter()
                .filter(|i| {
                    let n = str_field(i, "name").unwrap_or_default();
                    !ti.iter()
                        .any(|t| str_field(t, "name").as_deref() == Some(n.as_str()))
                        || idx_changed.contains(&n)
                })
                .collect(),
            None => Vec::new(),
        };
        for i in dropped {
            if i.get("primary").and_then(Value::as_bool).unwrap_or(false) {
                continue;
            }
            let n = quote_ident(d, str_field(i, "name").as_deref().unwrap_or(""))?;
            stmts.push(match d {
                Dialect::MySql => format!("ALTER TABLE {t} DROP INDEX {n}"),
                _ => format!("DROP INDEX {n}"),
            });
        }
    }
    // REQ-13 §4.2：RENAME 先于一切列定义变更（在 DROP INDEX/CONSTRAINT 之后、DROP COLUMN 之前），
    // 改类型等语句一律打在新名上。
    for (old, new) in &renames {
        stmts.push(format!(
            "ALTER TABLE {t} RENAME COLUMN {} TO {}",
            quote_ident(d, old)?,
            quote_ident(d, new)?
        ));
    }
    // 列 DROP（最后）。
    for c in &removed_c {
        let n = quote_ident(d, c.as_str().unwrap_or(""))?;
        stmts.push(format!("ALTER TABLE {t} DROP COLUMN {n}"));
    }

    // 列变更。PG 逐字段；MySQL 按列合并 MODIFY。
    match d {
        Dialect::MySql => {
            if let (Some(fc_list), Some(tc_list)) = (&from_cols_adj, &to_cols) {
                for tc in tc_list {
                    if !fc_list.iter().any(|x| x.name == tc.name) {
                        continue;
                    }
                    // field=="name"（rename）不进 MODIFY——RENAME 语句已单独发。
                    let touched = changed_c
                        .iter()
                        .any(|e| e["name"] == json!(tc.name) && e["field"] != json!("name"))
                        || position_changes.iter().any(|e| e["name"] == json!(tc.name));
                    if !touched {
                        continue;
                    }
                    let after = Some(prev_name(tc_list, &tc.name));
                    stmts.push(format!(
                        "ALTER TABLE {t} MODIFY COLUMN {}",
                        col_def_sql(d, tc, after.as_deref())
                    ));
                }
            }
        }
        _ => {
            for e in &changed_c {
                let cn = quote_ident(d, e["name"].as_str().unwrap_or(""))?;
                match e["field"].as_str().unwrap_or("") {
                    "nativeType" => {
                        let ty = e["to"].as_str().unwrap_or("");
                        check_fragment(ty)?;
                        stmts.push(format!(
                            "ALTER TABLE {t} ALTER COLUMN {cn} TYPE {ty} USING {cn}::{ty}"
                        ));
                    }
                    "nullable" => {
                        if e["to"] == json!(false) {
                            stmts.push(format!("ALTER TABLE {t} ALTER COLUMN {cn} SET NOT NULL"));
                        } else {
                            stmts.push(format!("ALTER TABLE {t} ALTER COLUMN {cn} DROP NOT NULL"));
                        }
                    }
                    "default" => match e["to"].as_str() {
                        Some(def) => {
                            check_fragment(def)?;
                            stmts.push(format!(
                                "ALTER TABLE {t} ALTER COLUMN {cn} SET DEFAULT {def}"
                            ));
                        }
                        None => {
                            stmts.push(format!("ALTER TABLE {t} ALTER COLUMN {cn} DROP DEFAULT"))
                        }
                    },
                    "identity" => match e["to"].as_str() {
                        Some("always") => stmts.push(format!(
                            "ALTER TABLE {t} ALTER COLUMN {cn} ADD GENERATED ALWAYS AS IDENTITY"
                        )),
                        Some(_) => stmts.push(format!(
                            "ALTER TABLE {t} ALTER COLUMN {cn} ADD GENERATED BY DEFAULT AS IDENTITY"
                        )),
                        None => {
                            stmts.push(format!("ALTER TABLE {t} ALTER COLUMN {cn} DROP IDENTITY"))
                        }
                    },
                    "comment" => match e["to"].as_str() {
                        Some(cm) => stmts.push(format!(
                            "COMMENT ON COLUMN {t}.{cn} IS {}",
                            quote_literal(cm)
                        )),
                        None => stmts.push(format!("COMMENT ON COLUMN {t}.{cn} IS NULL")),
                    },
                    _ => {}
                }
            }
        }
    }

    // ADD 列（列维度任一端缺省 = 不参与，同 DROP）。
    if let (Some(f), Some(tc)) = (&from_cols_adj, &to_cols) {
        for c in tc {
            if !f.iter().any(|x| x.name == c.name) {
                let after = {
                    let pos = tc.iter().position(|x| x.name == c.name).unwrap_or(0);
                    if pos == 0 {
                        Some("<first>")
                    } else {
                        Some(tc[pos - 1].name.as_str())
                    }
                };
                stmts.push(format!(
                    "ALTER TABLE {t} ADD COLUMN {}",
                    col_def_sql(d, c, after)
                ));
            }
        }
    }

    // 约束 ADD（含 changed：同名定义差 → DROP 已在上一步，此处重新 ADD）。
    if let Some(cons) = &to_cons {
        for c in cons {
            let is_new = from_cons
                .as_ref()
                .map(|fv| {
                    !fv.iter()
                        .any(|f| str_field(f, "name") == str_field(c, "name"))
                })
                .unwrap_or(false);
            let is_changed = cons_diff
                .2
                .iter()
                .any(|e| e["name"] == json!(str_field(c, "name").unwrap_or_default()));
            if is_new || is_changed {
                stmts.push(format!(
                    "ALTER TABLE {t} ADD CONSTRAINT {} {}",
                    quote_ident(d, str_field(c, "name").as_deref().unwrap_or(""))?,
                    str_field(c, "definition").unwrap_or_default()
                ));
            }
        }
    }
    // 索引 ADD。
    if let Some(idxs) = &to_idxs {
        for i in idxs {
            if i.get("primary").and_then(Value::as_bool).unwrap_or(false) {
                continue; // 主键走约束维度。
            }
            let is_new = from_idxs
                .as_ref()
                .map(|fv| {
                    !fv.iter()
                        .any(|f| str_field(f, "name") == str_field(i, "name"))
                })
                .unwrap_or(false);
            let is_changed = idx_diff
                .2
                .iter()
                .any(|e| e["name"] == json!(str_field(i, "name").unwrap_or_default()));
            if !(is_new || is_changed) {
                continue;
            }
            let cols = i
                .get("columns")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let col_sql = cols
                .iter()
                .filter_map(Value::as_str)
                .map(|c| quote_ident(d, c).unwrap_or_else(|_| c.to_string()))
                .collect::<Vec<_>>()
                .join(", ");
            let uniq = i.get("unique").and_then(Value::as_bool).unwrap_or(false);
            stmts.push(format!(
                "CREATE {}INDEX {} ON {t} ({col_sql})",
                if uniq { "UNIQUE " } else { "" },
                quote_ident(d, str_field(i, "name").as_deref().unwrap_or(""))?,
            ));
        }
    }
    // FK 最后 ADD。
    if let Some(fks) = &to_fks {
        for f in fks {
            let is_new = from_fks
                .as_ref()
                .map(|fv| {
                    !fv.iter()
                        .any(|x| str_field(x, "name") == str_field(f, "name"))
                })
                .unwrap_or(false);
            let is_changed = fk_diff
                .2
                .iter()
                .any(|e| e["name"] == json!(str_field(f, "name").unwrap_or_default()));
            if is_new || is_changed {
                stmts.push(fk_add_sql(d, &t, f)?);
            }
        }
    }

    // G4：表改名——RENAME 恒首条（先于一切列/约束/索引语句），diff.renamed 表级挂点
    // （与 diff.columns 平级；恒在，无改名为空数组）。RENAME TO 目标只许裸标识符
    // （评审 I-1：两方言同律）；旧引用保留 schema 段两段限定。
    if let Some((p, t_old)) = table_rename {
        stmts.insert(0, format!("ALTER TABLE {t_old} RENAME TO {t_bare}"));
        diff.insert("renamed".into(), json!([{"from": p, "to": name}]));
    } else {
        diff.insert("renamed".into(), json!([]));
    }

    Ok(json!({
        "diff": Value::Object(diff),
        "statements": stmts,
        "warnings": warnings,
    }))
}

/// FK ADD 语句（两方言同形：ADD CONSTRAINT ... FOREIGN KEY (...) REFERENCES ..(...))。
fn fk_add_sql(d: Dialect, t: &str, f: &Value) -> Result<String, String> {
    let cols = f
        .get("columns")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let refs = f
        .get("refColumns")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if cols.is_empty() || refs.is_empty() {
        return Err("ddlDiff: foreignKeys[] needs columns and refColumns".into());
    }
    let rt = f
        .get("refTable")
        .and_then(Value::as_str)
        .ok_or_else(|| "ddlDiff: foreignKeys[] missing refTable".to_string())?;
    let q = |c: &str| quote_ident(d, c).unwrap_or_else(|_| c.to_string());
    Ok(format!(
        "ALTER TABLE {t} ADD CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({})",
        q(str_field(f, "name").as_deref().unwrap_or("")),
        cols.iter()
            .filter_map(Value::as_str)
            .map(q)
            .collect::<Vec<_>>()
            .join(", "),
        q(rt),
        refs.iter()
            .filter_map(Value::as_str)
            .map(q)
            .collect::<Vec<_>>()
            .join(", "),
    ))
}

/// 具名维度（constraints/indexes/foreignKeys）diff：按 name 集合与字段值比较。
/// 返回 (added, removed, changed[{name, field, from, to}...])。
fn named_dim_diff(
    from: &Option<Vec<Value>>,
    to: &Option<Vec<Value>>,
) -> (Vec<Value>, Vec<Value>, Vec<Value>) {
    let empty = Vec::new();
    let f = from.as_ref().unwrap_or(&empty);
    let t = to.as_ref().unwrap_or(&empty);
    let added = t
        .iter()
        .filter(|e| {
            !f.iter()
                .any(|x| str_field(x, "name") == str_field(e, "name"))
        })
        .filter_map(|e| str_field(e, "name").map(Value::String))
        .collect();
    let removed = f
        .iter()
        .filter(|e| {
            !t.iter()
                .any(|x| str_field(x, "name") == str_field(e, "name"))
        })
        .filter_map(|e| str_field(e, "name").map(Value::String))
        .collect();
    let mut changed = Vec::new();
    for te in t {
        let Some(fe) = f
            .iter()
            .find(|x| str_field(x, "name") == str_field(te, "name"))
        else {
            continue;
        };
        for k in [
            "type",
            "definition",
            "unique",
            "primary",
            "origin",
            "refTable",
        ] {
            if fe.get(k) != te.get(k) {
                changed.push(json!({
                    "name": str_field(te, "name").unwrap_or_default(),
                    "field": k,
                    "from": fe.get(k).cloned().unwrap_or(Value::Null),
                    "to": te.get(k).cloned().unwrap_or(Value::Null),
                }));
            }
        }
        // columns/refColumns 数组比较。
        for k in ["columns", "refColumns"] {
            if fe.get(k) != te.get(k) {
                changed.push(json!({
                    "name": str_field(te, "name").unwrap_or_default(),
                    "field": k,
                    "from": fe.get(k).cloned().unwrap_or(Value::Null),
                    "to": te.get(k).cloned().unwrap_or(Value::Null),
                }));
            }
        }
    }
    (added, removed, changed)
}

// ---------------------------------------------------------------------------
// op 边界（Task 4）：授权链与 catalog 同（resolve_target + granted_only），
// 渲染本身零 DB I/O。
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// op 边界（Task 4）：授权链与 catalog 同（resolve_target + granted_only），
// 渲染本身零 DB I/O。
// ---------------------------------------------------------------------------

/// db.ddlDiff（U-14 反向，v0.1.109）：`{objectType:"table", name, from, to}`
/// → `{diff, statements}`。纯渲染，零 DB I/O；执行仍走 db.exec + allow_ddl。
/// 方言 ∉ {postgres, mysql} → 结构化 `OJ_DB_UNSUPPORTED`（capability=ddl_diff）。
#[op2]
#[serde]
pub async fn op_db_ddl_diff(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[serde] spec: serde_json::Value,
) -> Result<serde_json::Value, JsErrorBox> {
    let target = resolve_target(&state, &name)?;
    let info = match &target {
        Target::Tx(t) => &t.dialect_info,
        Target::Pool(da) => da.dialect_info(),
    };
    // sqlite 是 builder-known 方言，须显式拒绝（require_known_dialect 只挡未知方言）。
    let dialect = match require_known_dialect(info, "ddl_diff")? {
        Dialect::Sqlite => {
            return Err(to_js_err(Box::new(crate::bridge::db_error::unsupported(
                "sqlite", "ddl_diff",
            ))));
        }
        d => d,
    };
    let alias = super::guard::bound_db(&state, &name);
    super::db_policy::granted_only(&state, &alias)?;
    // G2（REQ-09 §4.3）：分区/表选项变更 → 结构化 OJ_DB_UNSUPPORTED（ddl_diff 内部
    // 字符串兜底是同判据第二层）。须先于 ddl_diff，否则 capability 被 generic 吞掉。
    if let Some(cap) = unsupported_capability_from_spec(dialect, &spec) {
        return Err(to_js_err(Box::new(crate::bridge::db_error::unsupported(
            dialect.as_str(),
            cap,
        ))));
    }
    ddl_diff(dialect, &spec).map_err(JsErrorBox::generic)
}

#[cfg(test)]
#[path = "db_ddl_tests.rs"]
mod tests;
