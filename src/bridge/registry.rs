//! SchemaRegistry：动态标识符（表名/列名）的白名单，是防止 SQL 注入的根治点。
//!
//! 所有经 sea-query 构造器生成的 SQL，其表名与列名必须来自本注册表，绝不允许来自 JS 字符串。
//! 值（value）仍由 sea-query 参数化绑定，底层 driver 负责转义。

use std::collections::HashMap;

/// 列定义：列名 + 是否允许排序。
#[derive(Debug, Clone)]
pub struct ColumnDef {
    pub name: String,
    pub sortable: bool,
}

/// 单表定义。
#[derive(Debug, Clone, Default)]
pub struct TableDef {
    pub columns: HashMap<String, ColumnDef>,
    /// 主键列名（用于 find_by_id / update / delete 的 WHERE 键）。
    pub primary_key: Option<String>,
}

impl TableDef {
    /// 校验列名是否在白名单内。
    pub fn has_column(&self, name: &str) -> bool {
        self.columns.contains_key(name)
    }

    /// 校验列是否允许排序。
    pub fn is_sortable(&self, name: &str) -> bool {
        self.columns.get(name).map(|c| c.sortable).unwrap_or(false)
    }
}

/// 全部表的注册表（不可变，构造后共享）。
#[derive(Debug, Clone, Default)]
pub struct SchemaRegistry {
    tables: HashMap<String, TableDef>,
}

impl SchemaRegistry {
    /// 构造一个空注册表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 声明一张表及其列（列名列表；主键为可选首参之外的显式字段）。
    pub fn table(mut self, name: &str, pk: Option<&str>, columns: &[&str]) -> Self {
        let mut cols = HashMap::new();
        for c in columns {
            cols.insert(
                c.to_string(),
                ColumnDef {
                    name: c.to_string(),
                    sortable: true,
                },
            );
        }
        let pk = pk.map(|s| s.to_string());
        if let Some(pk) = &pk {
            cols.entry(pk.clone()).or_insert(ColumnDef {
                name: pk.clone(),
                sortable: true,
            });
        }
        self.tables.insert(
            name.to_string(),
            TableDef {
                columns: cols,
                primary_key: pk,
            },
        );
        self
    }

    /// 取表定义；未知表返回 None（调用方应拒绝）。
    pub fn get(&self, name: &str) -> Option<&TableDef> {
        self.tables.get(name)
    }

    /// 表名是否在白名单内。
    pub fn has_table(&self, name: &str) -> bool {
        self.tables.contains_key(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg() -> SchemaRegistry {
        SchemaRegistry::new()
            .table("user", Some("id"), &["id", "name", "age"])
            .table("order", Some("id"), &["id", "user_id", "amount"])
    }

    #[test]
    fn whitelist_checks() {
        let r = reg();
        assert!(r.has_table("user"));
        assert!(!r.has_table("secret"));
        let t = r.get("user").unwrap();
        assert!(t.has_column("name"));
        assert!(!t.has_column("password_hash"));
        assert_eq!(t.primary_key.as_deref(), Some("id"));
    }

    #[test]
    fn sortable_flag_and_table_without_pk() {
        let mut cols = std::collections::HashMap::new();
        cols.insert(
            "a".to_string(),
            ColumnDef { name: "a".to_string(), sortable: true },
        );
        cols.insert(
            "b".to_string(),
            ColumnDef { name: "b".to_string(), sortable: false },
        );
        let td = TableDef { columns: cols, primary_key: None };
        assert!(td.is_sortable("a"));
        assert!(!td.is_sortable("b"));
        assert!(!td.is_sortable("missing"));

        // table 不带主键：pk 兜底分支（or_insert 不执行），primary_key 为 None。
        let r = SchemaRegistry::new().table("t", None, &["x", "y"]);
        let t = r.get("t").unwrap();
        assert_eq!(t.primary_key, None);
        assert!(t.has_column("x"));
        assert!(t.has_column("y"));
        assert!(r.has_table("t"));
    }
}
