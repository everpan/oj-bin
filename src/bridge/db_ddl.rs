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

/// Structure 列条目（与已发布 listColumns 同词汇 + identity/comment 可选）。
#[derive(Debug)]
struct ColSpec {
    name: String,
    native_type: String,
    nullable: bool,
    default: Option<String>,
    primary_key: bool,
    identity: Option<String>,
    comment: Option<String>,
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
    }
    Ok(Some(arr.clone()))
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

/// diff + 渲染。spec：`{objectType:"table", name, from: Structure|null, to: Structure}`。
pub fn ddl_diff(d: Dialect, spec: &Value) -> Result<Value, String> {
    if spec.get("objectType").and_then(Value::as_str) != Some("table") {
        return Err("ddlDiff: objectType must be \"table\"".into());
    }
    let name = spec
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| "ddlDiff: missing name".to_string())?;
    let t = quote_ident(d, name)?;
    let to = spec
        .get("to")
        .ok_or_else(|| "ddlDiff: missing to".to_string())?;
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

    let empty_cols: Vec<ColSpec> = Vec::new();
    let f_cols = from_cols.as_ref().unwrap_or(&empty_cols);
    let t_cols = to_cols.as_ref().unwrap_or(&empty_cols);

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
            .enumerate()
            .map(|(i, c)| {
                let after = if i == 0 {
                    Some("<first>")
                } else {
                    Some(t_cols[i - 1].name.as_str())
                };
                col_def_sql(d, c, after)
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
        stmts.push(format!(
            "CREATE TABLE {t} (\n  {}{}\n)",
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
        diff.insert(
            "addedColumns".into(),
            json!(t_cols.iter().map(|c| json!(c.name)).collect::<Vec<_>>()),
        );
        return Ok(json!({ "diff": Value::Object(diff), "statements": stmts }));
    }

    let mut push_dim = |key: &str, added: Vec<Value>, removed: Vec<Value>, changed: Vec<Value>| {
        diff.insert(
            key.to_string(),
            json!({ "added": added, "removed": removed, "changed": changed }),
        );
    };

    // ---------------- 维度缺省 = 不参与 diff；空数组 = DROP 全量 ----------------
    // 列。
    let added_c: Vec<Value> = match &to_cols {
        Some(tc) => tc
            .iter()
            .filter(|c| !f_cols.iter().any(|f| f.name == c.name))
            .map(|c| json!(c.name))
            .collect(),
        None => Vec::new(),
    };
    let removed_c: Vec<Value> = match &from_cols {
        Some(fc) => fc
            .iter()
            .filter(|c| !t_cols.iter().any(|t| t.name == c.name))
            .map(|c| json!(c.name))
            .collect(),
        None => Vec::new(),
    };
    let changed_c = match (&from_cols, &to_cols) {
        (Some(f), Some(t)) => col_diff_entries(f, t),
        _ => Vec::new(),
    };
    // MySQL 位置 diff（PG 无列位置 DDL → 忽略，api-manual 写明）。
    let mut position_changes: Vec<Value> = Vec::new();
    if d == Dialect::MySql
        && let (Some(f), Some(t)) = (&from_cols, &to_cols)
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

    // ---------------- statements：DROP（FK → 约束 → 索引 → 列）→ 列变更 → ADD ----------------
    if let Some(fks) = &from_fks {
        // 维度缺省 = 不参与；to 显式数组（可空）→ 名字集合差出 DROP。
        let dropped: Vec<&Value> = match &to_fks {
            Some(tv) => fks
                .iter()
                .filter(|f| {
                    !tv.iter()
                        .any(|t| str_field(t, "name") == str_field(f, "name"))
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
                    !tc.iter()
                        .any(|t| str_field(t, "name") == str_field(c, "name"))
                })
                .collect(),
            None => Vec::new(),
        };
        for c in dropped {
            let n = quote_ident(d, str_field(c, "name").as_deref().unwrap_or(""))?;
            stmts.push(match d {
                Dialect::MySql => format!("ALTER TABLE {t} DROP CHECK {n}"),
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
                    !ti.iter()
                        .any(|t| str_field(t, "name") == str_field(i, "name"))
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
    // 列 DROP（最后）。
    for c in &removed_c {
        let n = quote_ident(d, c.as_str().unwrap_or(""))?;
        stmts.push(format!("ALTER TABLE {t} DROP COLUMN {n}"));
    }

    // 列变更。PG 逐字段；MySQL 按列合并 MODIFY。
    match d {
        Dialect::MySql => {
            if let (Some(fc_list), Some(tc_list)) = (&from_cols, &to_cols) {
                for tc in tc_list {
                    if !fc_list.iter().any(|x| x.name == tc.name) {
                        continue;
                    }
                    let touched = changed_c.iter().any(|e| e["name"] == json!(tc.name))
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

    // ADD 列。
    if let Some(tc) = &to_cols {
        for c in tc {
            if !f_cols.iter().any(|f| f.name == c.name) {
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

    // 约束 ADD（changed 约束 = drop 已在上 + add 在此）。
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

    Ok(json!({ "diff": Value::Object(diff), "statements": stmts }))
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
    ddl_diff(dialect, &spec).map_err(JsErrorBox::generic)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn both(spec: Value) -> (Value, Value) {
        (
            ddl_diff(Dialect::Postgres, &spec).unwrap(),
            ddl_diff(Dialect::MySql, &spec).unwrap(),
        )
    }

    /// 缺省 = 不参与 diff（零语句）；空数组 = DROP 全量。
    #[test]
    fn absent_field_means_skip_empty_array_means_drop_all() {
        let from = json!({
            "columns": [{"name": "id", "nativeType": "integer", "nullable": false, "primaryKey": true}],
            "indexes": [{"name": "ix_a", "unique": false, "primary": false, "origin": "c",
                         "columns": ["id"]}],
            "constraints": [{"name": "uq_a", "type": "uq", "definition": "UNIQUE (id)", "columns": ["id"]}],
            "foreignKeys": [{"name": "fk_a", "columns": ["id"], "refTable": "p", "refColumns": ["id"]}],
        });
        // to 缺省 indexes/constraints/foreignKeys → 不参与 diff，零 DROP。
        let spec_skip = json!({
            "objectType": "table", "name": "t", "from": from.clone(),
            "to": {"columns": from["columns"].clone()},
        });
        let (pg, my) = both(spec_skip);
        assert_eq!(pg["statements"], json!([]));
        assert_eq!(my["statements"], json!([]));
        // to 显式空数组 → DROP 全量。
        let spec_drop = json!({
            "objectType": "table", "name": "t", "from": from.clone(),
            "to": {"columns": from["columns"].clone(), "indexes": [], "constraints": [],
                   "foreignKeys": []},
        });
        let (pg, _) = both(spec_drop.clone());
        let ss = pg["statements"].as_array().unwrap();
        let text: String = ss
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("DROP CONSTRAINT \"fk_a\""), "{text}");
        assert!(text.contains("DROP CONSTRAINT \"uq_a\""), "{text}");
        assert!(text.contains("DROP INDEX \"ix_a\""), "{text}");
        // 顺序：FK → 约束 → 索引。
        let i_fk = text.find("fk_a").unwrap();
        let i_uq = text.find("uq_a").unwrap();
        let i_ix = text.find("ix_a").unwrap();
        assert!(i_fk < i_uq && i_uq < i_ix, "{text}");
        let (_, my2) = both(spec_drop);
        let text2: String = my2["statements"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text2.contains("DROP FOREIGN KEY `fk_a`"), "{text2}");
        assert!(text2.contains("DROP INDEX `ix_a`"), "{text2}");
    }

    /// 畸形 Structure → 参数错误（Err），不 panic。
    #[test]
    fn malformed_structure_is_parameter_error() {
        for (label, spec) in [
            (
                "bad objectType",
                json!({"objectType": "view", "name": "t", "to": {"columns": []}}),
            ),
            (
                "missing name",
                json!({"objectType": "table", "to": {"columns": []}}),
            ),
            (
                "col missing name",
                json!({"objectType": "table", "name": "t",
                "from": null,
                "to": {"columns": [{"nativeType": "int"}]}}),
            ),
            (
                "col missing nativeType",
                json!({"objectType": "table", "name": "t",
                "from": null,
                "to": {"columns": [{"name": "c"}]}}),
            ),
            (
                "unknown constraint type",
                json!({"objectType": "table", "name": "t",
                "from": {"columns": []},
                "to": {"columns": [], "constraints": [{"name": "c", "type": "zz", "definition": "x"}]}}),
            ),
            (
                "NUL in identifier",
                json!({"objectType": "table", "name": "a\u{0}b",
                "from": {"columns": []}, "to": {"columns": []}}),
            ),
        ] {
            let r = ddl_diff(Dialect::Postgres, &spec);
            assert!(r.is_err(), "{label}: 须报错: {r:?}");
            assert!(r.unwrap_err().contains("ddlDiff"), "{label}");
        }
    }

    /// 引号翻倍 + NUL 拒绝（不做字符集白名单：合法名可含非 ASCII/空格）。
    #[test]
    fn quoting_doubles_and_rejects_nul() {
        assert_eq!(
            quote_ident(Dialect::Postgres, "a\"b").unwrap(),
            "\"a\"\"b\""
        );
        assert_eq!(quote_ident(Dialect::MySql, "a`b").unwrap(), "`a``b`");
        assert_eq!(
            quote_ident(Dialect::Postgres, "我的 table").unwrap(),
            "\"我的 table\""
        );
        assert!(quote_ident(Dialect::Postgres, "a\nb").is_err());
        assert!(quote_ident(Dialect::Postgres, "").is_err());
    }

    /// verbatim 片段拒 `;`（语句拆分防线）。
    #[test]
    fn fragments_reject_semicolon() {
        assert!(check_fragment("integer").is_ok());
        assert!(check_fragment("nextval('s'::regclass)").is_ok());
        assert!(check_fragment("integer; drop table t").is_err());
        assert!(check_fragment("a\nb").is_err());
    }

    /// identity 列跳 default 文本比对（nextval 派生，否则恒假 changed）。
    #[test]
    fn identity_column_skips_default_compare() {
        let spec = json!({
            "objectType": "table", "name": "t",
            "from": {"columns": [{"name": "id", "nativeType": "integer", "nullable": false,
                                  "default": "nextval('t_id_seq'::regclass)", "primaryKey": true}]},
            "to": {"columns": [{"name": "id", "nativeType": "integer", "nullable": false,
                                "primaryKey": true, "identity": "always"}]},
        });
        let (pg, _) = both(spec);
        assert_eq!(pg["diff"]["columns"]["changed"], json!([]), "{pg:?}");
    }

    /// 语句排序：DROP（FK → 约束 → 索引 → 列）→ 列变更 → ADD → 约束/索引 → FK 最后。
    #[test]
    fn statement_ordering_is_drop_then_alter_then_add_fk_last() {
        let from = json!({
            "columns": [
                {"name": "id", "nativeType": "integer", "nullable": false, "primaryKey": true},
                {"name": "old_c", "nativeType": "text", "nullable": true},
                {"name": "v", "nativeType": "integer", "nullable": true, "default": "0"},
            ],
            "constraints": [{"name": "ck_v", "type": "ck", "definition": "CHECK (v > 0)", "columns": []}],
            "indexes": [{"name": "ix_old", "unique": false, "primary": false, "origin": "c", "columns": ["old_c"]}],
            "foreignKeys": [{"name": "fk_old", "columns": ["id"], "refTable": "p", "refColumns": ["id"]}],
        });
        let to = json!({
            "columns": [
                {"name": "id", "nativeType": "integer", "nullable": false, "primaryKey": true},
                {"name": "v", "nativeType": "bigint", "nullable": false, "default": "1"},
                {"name": "new_c", "nativeType": "text", "nullable": true},
            ],
            "constraints": [{"name": "ck_v2", "type": "ck", "definition": "CHECK (v > 1)", "columns": []}],
            "indexes": [{"name": "ix_new", "unique": true, "primary": false, "origin": "c", "columns": ["new_c"]}],
            "foreignKeys": [{"name": "fk_new", "columns": ["v"], "refTable": "q", "refColumns": ["id"]}],
        });
        let (pg, _) = both(json!({"objectType": "table", "name": "t", "from": from, "to": to}));
        let ss: Vec<&str> = pg["statements"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        let pos = |needle: &str| {
            ss.iter()
                .position(|s| s.contains(needle))
                .unwrap_or(usize::MAX)
        };
        assert!(pos("DROP CONSTRAINT \"fk_old\"") < pos("DROP CONSTRAINT \"ck_v\""));
        assert!(pos("DROP CONSTRAINT \"ck_v\"") < pos("DROP INDEX \"ix_old\""));
        assert!(pos("DROP INDEX \"ix_old\"") < pos("DROP COLUMN \"old_c\""));
        assert!(pos("DROP COLUMN \"old_c\"") < pos("ALTER COLUMN \"v\" TYPE bigint"));
        assert!(pos("ALTER COLUMN \"v\" SET NOT NULL") < pos("ADD COLUMN \"new_c\""));
        assert!(pos("ADD COLUMN \"new_c\"") < pos("ADD CONSTRAINT \"ck_v2\""));
        assert!(pos("ADD CONSTRAINT \"ck_v2\"") < pos("CREATE UNIQUE INDEX \"ix_new\""));
        assert!(pos("CREATE UNIQUE INDEX \"ix_new\"") < pos("ADD CONSTRAINT \"fk_new\""));
    }

    /// from = null → 建表：PG 双引号 / MySQL 反引号，主键内联，FK 最后。
    #[test]
    fn create_table_from_null_from() {
        let spec = json!({
            "objectType": "table", "name": "t", "from": null,
            "to": {
                "columns": [
                    {"name": "id", "nativeType": "integer", "nullable": false, "primaryKey": true},
                    {"name": "name", "nativeType": "text", "nullable": true},
                ],
                "constraints": [{"name": "ck_id", "type": "ck", "definition": "CHECK (id > 0)", "columns": []}],
                "foreignKeys": [{"name": "fk_p", "columns": ["id"], "refTable": "p", "refColumns": ["id"]}],
            },
        });
        let (pg, my) = both(spec);
        let pgs: Vec<&str> = pg["statements"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(pgs[0].contains("CREATE TABLE \"t\""), "{pgs:?}");
        assert!(pgs[0].contains("PRIMARY KEY (\"id\")"), "{pgs:?}");
        assert!(
            pgs.iter()
                .any(|s| s.contains("ADD CONSTRAINT \"ck_id\" CHECK (id > 0)"))
        );
        assert!(
            pgs.last()
                .unwrap()
                .contains("ADD CONSTRAINT \"fk_p\" FOREIGN KEY (\"id\") REFERENCES \"p\" (\"id\")")
        );
        let mys: Vec<&str> = my["statements"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(mys[0].contains("CREATE TABLE `t`"), "{mys:?}");
        assert!(mys[0].contains("PRIMARY KEY (`id`)"), "{mys:?}");
    }

    /// PG 改类型产 USING；MySQL 位置差产 AFTER/FIRST；PG 忽略位置。
    #[test]
    fn pg_alter_type_emits_using_and_mysql_position() {
        let from = json!({"columns": [
            {"name": "a", "nativeType": "integer", "nullable": true},
            {"name": "b", "nativeType": "text", "nullable": true},
        ]});
        let to = json!({"columns": [
            {"name": "b", "nativeType": "text", "nullable": true},
            {"name": "a", "nativeType": "bigint", "nullable": true},
        ]});
        let spec = json!({"objectType": "table", "name": "t", "from": from, "to": to});
        let (pg, my) = both(spec.clone());
        let pgs: String = pg["statements"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(
            pgs.contains("ALTER COLUMN \"a\" TYPE bigint USING \"a\"::bigint"),
            "{pgs}"
        );
        // PG 无列位置 DDL：changedColumns 无 position 项、无位置语句。
        assert!(!pgs.contains("FIRST") && !pgs.contains("AFTER"), "{pgs}");
        let pg_changed = pg["diff"]["columns"]["changed"].as_array().unwrap();
        assert!(
            !pg_changed.iter().any(|e| e["field"] == json!("position")),
            "{pg_changed:?}"
        );
        let mys: String = my["statements"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(mys.contains("AFTER `b`"), "{mys}");
        let my_changed = my["diff"]["columns"]["changed"].as_array().unwrap();
        assert!(
            my_changed.iter().any(|e| e["field"] == json!("position")),
            "{my_changed:?}"
        );
    }

    /// diff 结构形状：columns/constraints/indexes/foreignKeys 四维 added/removed/changed。
    #[test]
    fn diff_shape_has_all_dimensions() {
        let spec = json!({
            "objectType": "table", "name": "t",
            "from": {"columns": [{"name": "a", "nativeType": "int", "nullable": true}]},
            "to": {"columns": [{"name": "a", "nativeType": "int", "nullable": false},
                               {"name": "b", "nativeType": "text", "nullable": true}]},
        });
        let (pg, _) = both(spec);
        let d = &pg["diff"];
        for dim in ["columns", "constraints", "indexes", "foreignKeys"] {
            for side in ["added", "removed", "changed"] {
                assert!(d[dim][side].is_array(), "{dim}.{side} 缺: {d:?}");
            }
        }
        assert_eq!(d["columns"]["added"], json!(["b"]));
        assert_eq!(d["columns"]["removed"], json!([]));
        assert_eq!(d["columns"]["changed"][0]["field"], json!("nullable"));
    }
}
