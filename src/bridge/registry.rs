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
    /// 主键列名（联合主键为多列；空 = 无主键）。
    pub primary_key: Vec<String>,
    /// 归属模块（schema.yaml 声明来源；None = 未声明表，守卫不设防）。
    pub owner: Option<String>,
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

    /// 声明一张表及其列（主键列列表；联合主键传多列，无主键传空切片）。
    pub fn table(mut self, name: &str, pk: &[&str], columns: &[&str]) -> Self {
        self.declare(None, name, pk, columns);
        self
    }

    /// 带归属模块的声明（schema.yaml 装配路径；`table()` 即 owner=None）。
    pub fn table_owned(mut self, owner: &str, name: &str, pk: &[&str], columns: &[&str]) -> Self {
        self.declare(Some(owner.to_string()), name, pk, columns);
        self
    }

    fn declare(&mut self, owner: Option<String>, name: &str, pk: &[&str], columns: &[&str]) {
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
        for p in pk {
            cols.entry(p.to_string()).or_insert(ColumnDef {
                name: p.to_string(),
                sortable: true,
            });
        }
        self.tables.insert(
            name.to_string(),
            TableDef {
                columns: cols,
                primary_key: pk.iter().map(|s| s.to_string()).collect(),
                owner,
            },
        );
    }

    /// 表归属模块（None = 未声明 / 未归属）。
    pub fn owner_of(&self, table: &str) -> Option<&str> {
        self.tables.get(table).and_then(|t| t.owner.as_deref())
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
            .table("user", &["id"], &["id", "name", "age"])
            .table("order", &["id"], &["id", "user_id", "amount"])
    }

    #[test]
    fn whitelist_checks() {
        let r = reg();
        assert!(r.has_table("user"));
        assert!(!r.has_table("secret"));
        let t = r.get("user").unwrap();
        assert!(t.has_column("name"));
        assert!(!t.has_column("password_hash"));
        assert_eq!(t.primary_key, vec!["id".to_string()]);
        // 联合主键：多列原样入册，pk 列兜底进白名单。
        let r3 = SchemaRegistry::new().table("pair", &["a", "b"], &["a", "b", "extra"]);
        let p = r3.get("pair").unwrap();
        assert_eq!(p.primary_key, vec!["a".to_string(), "b".to_string()]);
        assert!(p.has_column("a") && p.has_column("b") && p.has_column("extra"));
        // owner：table() 无归属，table_owned 带归属（守卫依据）。
        assert_eq!(r.owner_of("user"), None);
        let r2 = SchemaRegistry::new().table_owned("order", "orders", &["id"], &["id"]);
        assert_eq!(r2.owner_of("orders"), Some("order"));
        assert_eq!(r2.owner_of("missing"), None);
    }

    #[test]
    fn sortable_flag_and_table_without_pk() {
        let mut cols = std::collections::HashMap::new();
        cols.insert(
            "a".to_string(),
            ColumnDef {
                name: "a".to_string(),
                sortable: true,
            },
        );
        cols.insert(
            "b".to_string(),
            ColumnDef {
                name: "b".to_string(),
                sortable: false,
            },
        );
        let td = TableDef {
            columns: cols,
            primary_key: Vec::new(),
            owner: None,
        };
        assert!(td.is_sortable("a"));
        assert!(!td.is_sortable("b"));
        assert!(!td.is_sortable("missing"));

        // table 不带主键：pk 兜底分支（or_insert 不执行），primary_key 为空。
        let r = SchemaRegistry::new().table("t", &[], &["x", "y"]);
        let t = r.get("t").unwrap();
        assert!(t.primary_key.is_empty());
        assert!(t.has_column("x"));
        assert!(t.has_column("y"));
        assert!(r.has_table("t"));
    }
}
