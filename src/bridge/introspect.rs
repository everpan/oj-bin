//! db **结构内省**的方言知识层（v0.1.90，L2）。
//!
//! 只放三样东西：**三方言语 SQL 引擎常量** + **方言选择** + **结果上限**，
//! **不含执行、不含连接**——运行期 op 与 CLI（`oj schema`，T10 待迁移）共用同一份方言知识，
//! 避免「运行期一套、运维通道一套」各自漂移（T5：`'public'` vs `current_schema()` 就是实证）。
//!
//! 铁律（评审裁决）：
//! 1. **零标识符内联**：表名只经绑定参数进入 SQL（占位符一律 `?`，由 `normalize_and_trace`
//!    按方言改写）。禁止「绑定失败即内联」的兜底——不支持就报 `OJ_DB_UNSUPPORTED`。
//! 2. **PG 的 `name` 域列一律 `::text`**：`pg_*` / `information_schema.*` 的标识符列是
//!    `sql_identifier`（`name`）域，sqlx Any 驱动解不了该 OID，且**空库不触发、建表后才崩**
//!    （`oj/src/schema.rs` 已踩过）。CI 无 PG，故用 [`tests::pg_constants_cast_identifiers`] 静态钉死。
//! 3. **PG 的 `pg_index` / `pg_constraint` 没有 schema 列**：必须带 `nspname = current_schema()`
//!    过滤，否则会吐出其他 schema 的同名对象。
//! 4. 内省结果是**目录读**，不是标识符白名单——禁止回填构造器或裸 SQL（构造器的标识符只认
//!    `SchemaRegistry`）。

use serde_json::{Value, json};

use super::db::Dialect;

/// 单库单表的内省结果条数上限（列 / 索引 / 外键各自上限）。
///
/// 闭合「内省无行数上限」的资源面：内省 SQL 不经 `check_access`（拿不到 `policy.max_rows`），
/// 故在此硬编码。超限**截断**（元数据规模天然小，正常站点碰不到）。
pub const MAX_ROWS: usize = 1000;

/// 内省能力（与 `OJ_DB_UNSUPPORTED` 的 `data.capability` 同词汇）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Columns,
    Indexes,
    ForeignKeys,
}

impl Kind {
    /// 结构化 unsupported 的 `data.capability` 值（前端据此判断「调用前能不能用」）。
    pub fn capability(self) -> &'static str {
        match self {
            Kind::Columns => "list_columns",
            Kind::Indexes => "list_indexes",
            Kind::ForeignKeys => "list_foreign_keys",
        }
    }
}

/// 三方言语 SQL（引擎常量，参数位固定 `?`：0 = 表名）。
///
/// 投影列别名**三方言统一**，由本模块的 `columns/indexes/foreign_keys` 直接消费：
/// - 列：`name`, `native_type`, `not_null`, `default`, `is_pk`, `pos`
/// - 索引：`name`, `unique`, `primary`, `origin`, `column`, `seq`
/// - 外键：`group_key`, `name`, `column`, `ref_table`, `ref_column`, `col_pos`, `ref_pos`
pub fn sql(kind: Kind, d: Dialect) -> &'static str {
    match (kind, d) {
        // ---------------- 列 ----------------
        (Kind::Columns, Dialect::Sqlite) => {
            "SELECT name AS name, type AS native_type, \"notnull\" AS not_null, \
             dflt_value AS \"default\", pk AS is_pk, cid AS pos \
             FROM pragma_table_info(?) ORDER BY cid"
        }
        // mysql：`column_type` 是**声明文本**（`varchar(64)` / `decimal(10,2)`），
        // 而 `data_type` 只给基础类型——取后者等于丢长度，下游还得自己拼。
        (Kind::Columns, Dialect::MySql) => {
            "SELECT column_name AS name, column_type AS native_type, \
             is_nullable AS not_null, column_default AS `default`, \
             (column_key = 'PRI') AS is_pk, ordinal_position AS pos \
             FROM information_schema.columns \
             WHERE table_schema = DATABASE() AND table_name = ? \
             ORDER BY ordinal_position"
        }
        // pg：走 `pg_catalog` 取 `format_type`（声明文本，含长度/数组），PK 经
        // `information_schema` 的 PRIMARY KEY 约束判定（避开 `int2vector`）。
        (Kind::Columns, Dialect::Postgres) => {
            "SELECT c.column_name::text AS name, \
             format_type(a.atttypid, a.atttypmod) AS native_type, \
             (c.is_nullable = 'YES') AS not_null, \
             c.column_default::text AS \"default\", \
             (kcu.column_name IS NOT NULL) AS is_pk, \
             c.ordinal_position AS pos \
             FROM information_schema.columns c \
             JOIN pg_class cl ON cl.relname = c.table_name \
                  AND cl.relnamespace = c.table_schema::regnamespace \
             JOIN pg_attribute a ON a.attrelid = cl.oid AND a.attname = c.column_name \
             LEFT JOIN information_schema.table_constraints tc \
                  ON tc.table_schema = c.table_schema AND tc.table_name = c.table_name \
                     AND tc.constraint_type = 'PRIMARY KEY' \
             LEFT JOIN information_schema.key_column_usage kcu \
                  ON kcu.constraint_schema = tc.constraint_schema \
                     AND kcu.constraint_name = tc.constraint_name \
                     AND kcu.column_name = c.column_name \
             WHERE c.table_schema = current_schema() AND c.table_name = ? \
             ORDER BY c.ordinal_position"
        }

        // ---------------- 索引 ----------------
        // sqlite：`pragma_index_list.origin` 是「创建方式」（`c`=CREATE INDEX、
        // `u`=UNIQUE 约束、`pk`=主键约束），与 mysql/pg 的「唯一性」口径不一致——
        // `create unique index` 也归 `c`。为消除方言分支，按 unique/origin 派生
        // 成与另两方言同义的 `pk`/`u`/`c`（sqlite_autoindex 的 `u` 由此路径自然得到）。
        (Kind::Indexes, Dialect::Sqlite) => {
            "SELECT il.name AS name, il.\"unique\" AS `unique`, \
             (il.origin = 'pk') AS `primary`, \
             CASE WHEN il.origin = 'pk' THEN 'pk' WHEN il.\"unique\" = 1 THEN 'u' ELSE 'c' END AS origin, \
             ii.name AS column, ii.seqno AS seq \
             FROM pragma_index_list(?) il \
             CROSS JOIN pragma_index_info(il.name) ii \
             ORDER BY il.name, ii.seqno"
        }
        (Kind::Indexes, Dialect::MySql) => {
            "SELECT index_name AS name, (non_unique = 0) AS `unique`, \
             (index_name = 'PRIMARY') AS `primary`, \
             CASE WHEN index_name = 'PRIMARY' THEN 'pk' \
                  WHEN non_unique = 0 THEN 'u' ELSE 'c' END AS origin, \
             column_name AS column, seq_in_index AS seq \
             FROM information_schema.statistics \
             WHERE table_schema = DATABASE() AND table_name = ? \
             ORDER BY index_name, seq_in_index"
        }
        // pg：`indkey` 是 `int2vector`（伪数组），`unnest` 必须显式转 `int2[]`；
        // `pg_*` 无 schema 列 → 靠 `relnamespace` + `current_schema()` 限定。
        (Kind::Indexes, Dialect::Postgres) => {
            "SELECT c.relname::text AS name, i.indisunique AS `unique`, \
             i.indisprimary AS `primary`, \
             CASE WHEN i.indisprimary THEN 'pk' WHEN i.indisunique THEN 'u' \
                  ELSE 'c' END AS origin, \
             a.attname::text AS column, k.ord AS seq \
             FROM pg_class t \
             JOIN pg_index i ON i.indrelid = t.oid \
             JOIN pg_class c ON c.oid = i.indexrelid \
             JOIN pg_namespace n ON n.oid = t.relnamespace \
             CROSS JOIN unnest(i.indkey::int2[]) WITH ORDINALITY AS k(attnum, ord) \
             JOIN pg_attribute a ON a.attrelid = t.oid AND a.attnum = k.attnum \
             WHERE t.relname = ? AND n.nspname = current_schema() \
             ORDER BY c.relname, k.ord"
        }

        // ---------------- 外键 ----------------
        // sqlite：外键**没有约束名** → `name` 恒 NULL，用 `id` 作分组键。
        (Kind::ForeignKeys, Dialect::Sqlite) => {
            "SELECT id AS group_key, NULL AS name, \"from\" AS column, \
             \"table\" AS ref_table, \"to\" AS ref_column, seq AS col_pos, seq AS ref_pos \
             FROM pragma_foreign_key_list(?) ORDER BY id, seq"
        }
        // mysql：`key_column_usage` 里非外键行（PRIMARY/UNIQUE）的 `referenced_table_name`
        // 是 NULL → **必须过滤**，否则主键/唯一键会被当成外键。
        // 复合外键的两端配对用 `position_in_unique_constraint`（不是 `ordinal_position`）。
        (Kind::ForeignKeys, Dialect::MySql) => {
            "SELECT constraint_name AS group_key, constraint_name AS name, \
             column_name AS column, referenced_table_name AS ref_table, \
             referenced_column_name AS ref_column, \
             ordinal_position AS col_pos, position_in_unique_constraint AS ref_pos \
             FROM information_schema.key_column_usage \
             WHERE table_schema = DATABASE() AND table_name = ? \
                   AND referenced_table_name IS NOT NULL \
             ORDER BY constraint_name, ordinal_position"
        }
        // pg：`conkey` / `confkey` 都是 `int2[]`，直接 `unnest` 并**按 ordinality 配对**。
        (Kind::ForeignKeys, Dialect::Postgres) => {
            "SELECT con.conname::text AS group_key, con.conname::text AS name, \
             a.attname::text AS column, rt.relname::text AS ref_table, \
             ra.attname::text AS ref_column, ck.ord AS col_pos, fk.ord AS ref_pos \
             FROM pg_constraint con \
             JOIN pg_class t ON t.oid = con.conrelid \
             JOIN pg_class rt ON rt.oid = con.confrelid \
             JOIN pg_namespace n ON n.oid = t.relnamespace \
             CROSS JOIN unnest(con.conkey) WITH ORDINALITY AS ck(attnum, ord) \
             JOIN unnest(con.confkey) WITH ORDINALITY AS fk(attnum, ord) ON fk.ord = ck.ord \
             JOIN pg_attribute a ON a.attrelid = t.oid AND a.attnum = ck.attnum \
             JOIN pg_attribute ra ON ra.attrelid = rt.oid AND ra.attnum = fk.attnum \
             WHERE con.contype = 'f' AND t.relname = ? AND n.nspname = current_schema() \
             ORDER BY con.conname, ck.ord"
        }
    }
}

/// 结果 → JS 契约形态（三方言同构），并截断到 [`MAX_ROWS`]。
pub fn columns(rows: Vec<Value>) -> Vec<Value> {
    rows.iter()
        .take(MAX_ROWS)
        .map(|r| {
            json!({
                "name": str_of(r, "name"),
                "nativeType": str_of(r, "native_type"),
                "nullable": !truthy(r.get("not_null")),
                "default": opt_str(r.get("default")),
                "primaryKey": truthy(r.get("is_pk")),
            })
        })
        .collect()
}

/// 索引：按 `name` 聚合列（保持 SQL 的 `seq` 顺序），`origin` 三方言统一为 `c` / `u` / `pk`。
pub fn indexes(rows: Vec<Value>) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut cur_name: Option<String> = None;
    for r in rows.iter() {
        let name = str_of(r, "name");
        if cur_name.as_deref() != Some(name.as_str()) {
            if out.len() >= MAX_ROWS {
                break;
            }
            cur_name = Some(name.clone());
            out.push(json!({
                "name": name,
                "unique": truthy(r.get("unique")),
                "primary": truthy(r.get("primary")),
                "origin": origin_of(r.get("origin")),
                "columns": Vec::<Value>::new(),
            }));
        }
        if let Some(last) = out.last_mut()
            && let Some(col) = opt_str(r.get("column"))
        {
            last["columns"].as_array_mut().unwrap().push(json!(col));
        }
    }
    out
}

/// 外键：按 `group_key` 聚合，`columns` 按 `col_pos`、`refColumns` 按 `ref_pos` 各自定序
/// （复合外键两端**按位配对**）。sqlite 无名 → `name` 为 `null`。
pub fn foreign_keys(rows: Vec<Value>) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut cur: Option<String> = None;
    for r in rows.iter() {
        let key = str_of(r, "group_key");
        if cur.as_deref() != Some(key.as_str()) {
            if out.len() >= MAX_ROWS {
                break;
            }
            cur = Some(key.clone());
            let ref_table = str_of(r, "ref_table");
            out.push(json!({
                "name": opt_str(r.get("name")),
                "columns": Vec::<Value>::new(),
                "refTable": ref_table,
                "refColumns": Vec::<Value>::new(),
            }));
            // 用临时键排序：col_pos / ref_pos 各自升序后再落回 columns / refColumns。
            let last = out.last_mut().unwrap();
            last.as_object_mut().unwrap().insert(
                "_sort".to_string(),
                json!({"col": [], "ref": [], "refTable": ref_table}),
            );
        }
        let last = out.last_mut().unwrap();
        let col = str_of(r, "column");
        let ref_col = opt_str(r.get("ref_column"));
        let col_pos = num_of(r, "col_pos");
        let ref_pos = num_of(r, "ref_pos");
        let obj = last.as_object_mut().unwrap();
        let sort = obj.get_mut("_sort").unwrap().as_object_mut().unwrap();
        sort["col"]
            .as_array_mut()
            .unwrap()
            .push(json!([col_pos, col]));
        if let Some(rc) = ref_col {
            sort["ref"]
                .as_array_mut()
                .unwrap()
                .push(json!([ref_pos, rc]));
        }
    }
    for e in out.iter_mut() {
        let obj = e.as_object_mut().unwrap();
        let sort = obj.remove("_sort").unwrap_or_else(|| json!({}));
        let empty = Vec::<Value>::new();
        let cols = sort.get("col").and_then(|v| v.as_array()).unwrap_or(&empty);
        let refs = sort.get("ref").and_then(|v| v.as_array()).unwrap_or(&empty);
        obj.insert("columns".to_string(), sorted_pairs(cols));
        obj.insert("refColumns".to_string(), sorted_pairs(refs));
        obj.remove("_col");
    }
    out
}

/// 按 `[pos, value]` 的首元素升序取第二列。
fn sorted_pairs(pairs: &[Value]) -> Value {
    let mut v: Vec<(f64, String)> = pairs
        .iter()
        .filter_map(|p| {
            let arr = p.as_array()?;
            Some((num_of_raw(arr.first()?), str_of_raw(arr.get(1)?)))
        })
        .collect();
    v.sort_by(|a, b| a.0.total_cmp(&b.0));
    Value::Array(v.into_iter().map(|(_, s)| json!(s)).collect())
}

fn str_of(r: &Value, k: &str) -> String {
    str_of_raw(r.get(k).unwrap_or(&Value::Null))
}

fn str_of_raw(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => String::new(),
    }
}

fn opt_str(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

/// 三方言的布尔形态不统一（sqlite/mysql 给 0/1，mysql 的 `is_nullable` 给 `YES`/`NO`，
/// pg 给真 boolean）→ 一律走这个宽容判定。
fn truthy(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0) != 0.0,
        Some(Value::String(s)) => {
            let u = s.to_ascii_uppercase();
            matches!(u.as_str(), "YES" | "Y" | "T" | "TRUE" | "1" | "PK")
        }
        _ => false,
    }
}

/// `origin` 归一到 `c` / `u` / `pk`：sqlite 直给，mysql/pg 由 SQL 的 CASE 给出；
/// 拿不到/不认识一律按普通索引 `c`（不猜、不造第四态）。
fn origin_of(v: Option<&Value>) -> &'static str {
    match v.and_then(|v| v.as_str()) {
        Some("pk") => "pk",
        Some("u") => "u",
        _ => "c",
    }
}

fn num_of(r: &Value, k: &str) -> f64 {
    num_of_raw(r.get(k).unwrap_or(&Value::Null))
}

fn num_of_raw(v: &Value) -> f64 {
    match v {
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        Value::String(s) => s.parse::<f64>().unwrap_or(0.0),
        _ => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **PG 常量的 `::text` 静态守卫**（CI 无 PG，这是唯一离线防线）。
    ///
    /// 只查**投影列**（`<ident> … AS …`）：比较条件里的 `relname = column_name` 是 name 与
    /// `sql_identifier` 的比较，不需要 cast（真 PG 已验）。
    #[test]
    fn pg_constants_cast_identifiers() {
        for kind in [Kind::Columns, Kind::Indexes, Kind::ForeignKeys] {
            let s = sql(kind, Dialect::Postgres);
            for id in ["attname", "relname", "conname"] {
                let bare = String::from(id) + " AS";
                let cast = String::from(id) + "::text";
                assert!(
                    !s.contains(&bare) || s.contains(&cast),
                    "{kind:?}: PG 投影列 {id} 缺 ::text（sqlx Any 驱动解不了 name 域，\
                     且空库不触发、建表后才崩）"
                );
            }
        }
    }

    /// 内省 SQL 是**等值匹配**，不是 LIKE：表名里的 `%` / `_` 不得被当通配符。
    #[test]
    fn constants_use_equality_not_like() {
        for kind in [Kind::Columns, Kind::Indexes, Kind::ForeignKeys] {
            for d in [Dialect::Sqlite, Dialect::MySql, Dialect::Postgres] {
                let s = sql(kind, d);
                assert!(!s.contains("LIKE"), "{kind:?}/{d:?} 不该用 LIKE: {s}");
                assert_eq!(s.matches('?').count(), 1, "{kind:?}/{d:?} 参数位须恰为 1");
            }
        }
    }

    /// `key_column_usage` 的 NULL 过滤是硬要求：漏掉会把 PRIMARY/UNIQUE 当外键。
    #[test]
    fn mysql_fk_filters_non_fk_rows() {
        assert!(
            sql(Kind::ForeignKeys, Dialect::MySql).contains("referenced_table_name IS NOT NULL")
        );
    }

    /// PG 的索引/外键必须限定 schema（`pg_*` 无 schema 列，否则跨 schema 串数据）。
    #[test]
    fn pg_scopes_to_current_schema() {
        for kind in [Kind::Indexes, Kind::ForeignKeys] {
            let s = sql(kind, Dialect::Postgres);
            assert!(
                s.contains("current_schema()"),
                "{kind:?}: PG 缺 schema 限定: {s}"
            );
        }
    }

    /// 聚合：索引按名合并列、外键按位配对、sqlite 无名 → null。
    #[test]
    fn aggregation_shapes() {
        let idx = indexes(vec![
            json!({"name": "ix", "unique": 0, "primary": 0, "origin": "c", "column": "a", "seq": 0}),
            json!({"name": "ix", "unique": 0, "primary": 0, "origin": "c", "column": "b", "seq": 1}),
            json!({"name": "PRIMARY", "unique": 1, "primary": 1, "origin": "pk", "column": "id", "seq": 1}),
        ]);
        assert_eq!(idx.len(), 2);
        assert_eq!(idx[0]["columns"], json!(["a", "b"]));
        assert_eq!(idx[1]["origin"], json!("pk"));

        let fk = foreign_keys(vec![
            json!({"group_key": "0", "name": null, "column": "b_id", "ref_table": "p",
                   "ref_column": "id", "col_pos": 0, "ref_pos": 0}),
            json!({"group_key": "0", "name": null, "column": "c_id", "ref_table": "p",
                   "ref_column": "code", "col_pos": 1, "ref_pos": 1}),
        ]);
        assert_eq!(fk.len(), 1, "{fk:?}");
        assert_eq!(fk[0]["name"], json!(null));
        assert_eq!(fk[0]["columns"], json!(["b_id", "c_id"]));
        assert_eq!(fk[0]["refColumns"], json!(["id", "code"]));
        assert_eq!(fk[0]["refTable"], json!("p"));

        let col = columns(vec![json!({"name": "id", "native_type": "integer",
                                      "not_null": 0, "default": null, "is_pk": 1})]);
        assert_eq!(col[0]["nullable"], json!(true), "sqlite rowid PK 仍可空");
        assert_eq!(col[0]["primaryKey"], json!(true));
        assert_eq!(col[0]["default"], json!(null));
    }
}
