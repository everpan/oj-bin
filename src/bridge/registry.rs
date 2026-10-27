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
    /// 列类型（v0.1.24；schema.yaml 声明来源）。`Unknown` = 未声明类型信息
    /// （旧装配路径与全部既有夹具），按**旧行为**处理。
    pub ty: ColumnType,
}

/// 列的 SQL 类型（v0.1.24）：取值与 `schema.yaml` 的 `type` 字段一致。
///
/// 用途：租户守卫按 `tenant_id` 列类型生成绑定值（数值列必须绑数值，否则 PG 报
/// `operator does not exist: bigint = text`）；`Unknown` 保留 v0.1.23 及以前的字符串行为。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColumnType {
    Integer,
    BigInt,
    Text,
    Boolean,
    Double,
    Blob,
    /// **列类型未声明**（旧装配路径 `table()` / `table_owned()`、全部既有夹具）——
    /// 按 text 处理以保持向后兼容。**不表示「不认识这个类型串」**：
    /// 那种情况由 [`ColumnType::from_schema_type`] 返回 `None`，调用方必须 fail-fast。
    #[default]
    Unknown,
}

impl ColumnType {
    /// `schema.yaml` 的 `type` 字面量全集（唯一词汇表：解析、声明期校验的错误文案共用）。
    pub const SCHEMA_TYPES: [&'static str; 6] =
        ["integer", "bigint", "text", "boolean", "double", "blob"];

    /// `schema.yaml` 的 `type` 字面量 → 类型；**未识别返回 `None`**（绝不静默当 text）。
    ///
    /// 这不是「宽容解析」的场合：把 `varchar` / `jsonb` / `INTEGER`（大小写）当成 text，
    /// 会让租户守卫给 `tenant_id` 绑字符串，在 PG 上退化成一句
    /// `operator does not exist: bigint = text`——排障方向完全错（看起来像方言 bug，
    /// 实际是声明写错）。调用方要么在声明期报错点名，要么显式决定降级并写清理由。
    pub fn from_schema_type(s: &str) -> Option<Self> {
        Some(match s {
            "integer" => ColumnType::Integer,
            "bigint" => ColumnType::BigInt,
            "text" => ColumnType::Text,
            "boolean" => ColumnType::Boolean,
            "double" => ColumnType::Double,
            "blob" => ColumnType::Blob,
            _ => return None,
        })
    }

    /// 是否可承载数值型租户 id（`apply_tenant` 与声明期校验共用）。
    pub fn is_numeric(self) -> bool {
        matches!(self, ColumnType::Integer | ColumnType::BigInt)
    }
}

/// 单表定义。
#[derive(Debug, Clone, Default)]
pub struct TableDef {
    pub columns: HashMap<String, ColumnDef>,
    /// 主键列名（联合主键为多列；空 = 无主键）。
    pub primary_key: Vec<String>,
    /// 归属模块（schema.yaml 声明来源；None = 未声明表，守卫不设防）。
    pub owner: Option<String>,
    /// 共享表标记（schema.yaml `tenant: false` 且过 config tenant.shared_allow 白名单；
    /// 默认 false = 受租户约束）。sql_guard 注入的豁免依据。
    pub shared: bool,
}

impl TableDef {
    /// 校验列名是否在白名单内。
    pub fn has_column(&self, name: &str) -> bool {
        self.columns.contains_key(name)
    }

    /// 多租户注入候选：非共享表且含 tenant_id 列。
    pub fn is_tenant_scoped(&self) -> bool {
        !self.shared && self.has_column("tenant_id")
    }

    /// 校验列是否允许排序。
    pub fn is_sortable(&self, name: &str) -> bool {
        self.columns.get(name).map(|c| c.sortable).unwrap_or(false)
    }

    /// 列类型（未声明列 / 未声明类型 → `Unknown`）。
    pub fn column_type(&self, name: &str) -> ColumnType {
        self.columns.get(name).map(|c| c.ty).unwrap_or_default()
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

    /// 带 shared 标记的归属声明（sql_guard 装配路径；shared = 共享表白名单生效）。
    pub fn table_owned_shared(
        mut self,
        owner: &str,
        name: &str,
        pk: &[&str],
        columns: &[&str],
        shared: bool,
    ) -> Self {
        self.declare_shared(Some(owner.to_string()), name, pk, columns, shared);
        self
    }

    /// 带列类型的归属声明（v0.1.24；schema.yaml 真正带出类型的装配路径）。
    /// 未列出的主键列与旧路径一样补 `Unknown`——列类型只影响租户守卫的绑定形态。
    pub fn table_owned_shared_typed(
        mut self,
        owner: &str,
        name: &str,
        pk: &[&str],
        columns: &[(&str, ColumnType)],
        shared: bool,
    ) -> Self {
        let mut cols = HashMap::new();
        for (c, ty) in columns {
            cols.insert(
                c.to_string(),
                ColumnDef {
                    name: c.to_string(),
                    sortable: true,
                    ty: *ty,
                },
            );
        }
        for p in pk {
            cols.entry(p.to_string()).or_insert(ColumnDef {
                name: p.to_string(),
                sortable: true,
                ty: ColumnType::Unknown,
            });
        }
        self.tables.insert(
            name.to_string(),
            TableDef {
                columns: cols,
                primary_key: pk.iter().map(|s| s.to_string()).collect(),
                owner: Some(owner.to_string()),
                shared,
            },
        );
        self
    }

    fn declare(&mut self, owner: Option<String>, name: &str, pk: &[&str], columns: &[&str]) {
        self.declare_shared(owner, name, pk, columns, false);
    }

    fn declare_shared(
        &mut self,
        owner: Option<String>,
        name: &str,
        pk: &[&str],
        columns: &[&str],
        shared: bool,
    ) {
        let mut cols = HashMap::new();
        for c in columns {
            cols.insert(
                c.to_string(),
                ColumnDef {
                    name: c.to_string(),
                    sortable: true,
                    ty: ColumnType::Unknown,
                },
            );
        }
        for p in pk {
            cols.entry(p.to_string()).or_insert(ColumnDef {
                name: p.to_string(),
                sortable: true,
                ty: ColumnType::Unknown,
            });
        }
        self.tables.insert(
            name.to_string(),
            TableDef {
                columns: cols,
                primary_key: pk.iter().map(|s| s.to_string()).collect(),
                owner,
                shared,
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
        // shared 标记：默认 false=受租户约束；含 tenant_id 列才构成注入候选。
        assert!(!r2.get("orders").unwrap().is_tenant_scoped());
        let r3 = SchemaRegistry::new()
            .table("tt", &["id"], &["id", "tenant_id", "name"])
            .table_owned_shared("m", "sh", &["id"], &["id", "name"], true);
        assert!(r3.get("tt").unwrap().is_tenant_scoped());
        assert!(!r3.get("sh").unwrap().is_tenant_scoped());
    }

    #[test]
    fn sortable_flag_and_table_without_pk() {
        let mut cols = std::collections::HashMap::new();
        cols.insert(
            "a".to_string(),
            ColumnDef {
                name: "a".to_string(),
                sortable: true,
                ty: ColumnType::Unknown,
            },
        );
        cols.insert(
            "b".to_string(),
            ColumnDef {
                name: "b".to_string(),
                sortable: false,
                ty: ColumnType::Unknown,
            },
        );
        let td = TableDef {
            columns: cols,
            primary_key: Vec::new(),
            owner: None,
            shared: false,
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

    #[test]
    fn from_schema_type_maps_exactly_the_six_declared_names() {
        // 词汇表就是 SCHEMA_TYPES：每一项都能解析，且解析结果不是 Unknown。
        for name in ColumnType::SCHEMA_TYPES {
            let ty =
                ColumnType::from_schema_type(name).unwrap_or_else(|| panic!("{name} 必须可解析"));
            assert_ne!(ty, ColumnType::Unknown, "{name} 不得落到 Unknown");
        }
        assert_eq!(
            ColumnType::from_schema_type("integer"),
            Some(ColumnType::Integer)
        );
        assert_eq!(ColumnType::from_schema_type("blob"), Some(ColumnType::Blob));
    }

    /// 回归（v0.1.88）：未识别的类型串**不得**静默降级成 text。
    ///
    /// 曾经的 `from_schema_type` 兜底返回 `Unknown`（= 按 text 处理），调用方一见 `Ok` 就
    /// 拿它去绑租户条件——`type: varchar` 的 `tenant_id` 会在 PG 上变成
    /// `operator does not exist: bigint = text`。现在返回 `None`，由调用方 fail-fast。
    #[test]
    fn from_schema_type_never_silently_falls_back() {
        for junk in [
            "",
            "varchar",
            "VARCHAR(64)",
            "jsonb",
            "INTEGER",
            "int",
            "timestamptz",
            "decimal",
            "float",
            " blob",
            "blob ",
        ] {
            assert_eq!(
                ColumnType::from_schema_type(junk),
                None,
                "{junk:?} 必须判为未识别（大小写/空白都不宽容），不得回落 Unknown/text"
            );
        }
    }
}
