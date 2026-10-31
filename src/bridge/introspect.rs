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

/// catalog 抓取上限 = MAX_ROWS + 1：op 给 catalog SQL 追 `LIMIT` 后多取一行，
/// 让 [`catalog`] 的 truncated 信令（rows.len() > MAX_ROWS）在生产路径可达
/// （评审修复 v0.1.112：追 LIMIT MAX_ROWS 会让旗标恒 false）。
pub const CATALOG_FETCH_CAP: usize = MAX_ROWS + 1;

/// 内省能力（与 `OJ_DB_UNSUPPORTED` 的 `data.capability` 同词汇）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Columns,
    Indexes,
    ForeignKeys,
    // ---- U-14 批 a（v0.1.100）：catalog 类（`catalog_sql` + `{kind,dialect,items}` 信封）----
    Views,
    Routines,
    Sequences,
    Ddl,
    Comment,
    Constraints,
    Stats,
    // ---- REQ-10 一期（v0.1.112）：全实例/全库运维面（拒绝 scope，见 op 拦截）----
    Processlist,
    Users,
    Roles,
    // ---- REQ-08 收口（v0.1.112）：triggers 清单（拒绝 scope，见 op 拦截）----
    Triggers,
}

impl Kind {
    /// 结构化 unsupported 的 `data.capability` 值（前端据此判断「调用前能不能用」）。
    pub fn capability(self) -> &'static str {
        match self {
            Kind::Columns => "list_columns",
            Kind::Indexes => "list_indexes",
            Kind::ForeignKeys => "list_foreign_keys",
            Kind::Views => "views",
            Kind::Routines => "routines",
            Kind::Sequences => "sequences",
            Kind::Ddl => "ddl",
            Kind::Comment => "comment",
            Kind::Constraints => "constraints",
            Kind::Stats => "stats",
            Kind::Processlist => "processlist",
            Kind::Users => "users",
            Kind::Roles => "roles",
            Kind::Triggers => "triggers",
        }
    }

    /// catalog 类是否吃**表名参数**（false = 全库清单，无参数位）。
    pub fn catalog_param(self) -> bool {
        matches!(self, Kind::Ddl | Kind::Comment | Kind::Constraints)
    }

    /// catalog 类是否可追加 `LIMIT`（Ddl 单对象 → false；mysql 的 SHOW 族本来就不接 LIMIT）。
    pub fn catalog_limit(self) -> bool {
        !matches!(self, Kind::Ddl)
    }

    /// 字符串 → catalog Kind（op 入参解析；L2 三件不在 catalog 面）。
    pub fn parse_catalog(s: &str) -> Option<Kind> {
        Some(match s {
            "views" => Kind::Views,
            "routines" => Kind::Routines,
            "sequences" => Kind::Sequences,
            "ddl" => Kind::Ddl,
            "comment" => Kind::Comment,
            "constraints" => Kind::Constraints,
            "stats" => Kind::Stats,
            "processlist" => Kind::Processlist,
            "users" => Kind::Users,
            "roles" => Kind::Roles,
            "triggers" => Kind::Triggers,
            _ => return None,
        })
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
             a.attidentity::text AS identity, \
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
             column_name AS `column`, seq_in_index AS seq \
             FROM information_schema.statistics \
             WHERE table_schema = DATABASE() AND table_name = ? \
             ORDER BY index_name, seq_in_index"
        }
        // pg：`indkey` 是 `int2vector`（伪数组），`unnest` 必须显式转 `int2[]`；
        // `pg_*` 无 schema 列 → 靠 `relnamespace` + `current_schema()` 限定。
        (Kind::Indexes, Dialect::Postgres) => {
            "SELECT c.relname::text AS name, i.indisunique AS \"unique\", \
             i.indisprimary AS \"primary\", \
             CASE WHEN i.indisprimary THEN 'pk' WHEN i.indisunique THEN 'u' \
                  ELSE 'c' END AS origin, \
             a.attname::text AS \"column\", k.ord AS seq \
             FROM pg_class t \
             JOIN pg_index i ON i.indrelid = t.oid \
             JOIN pg_class c ON c.oid = i.indexrelid \
             JOIN pg_namespace n ON n.oid = t.relnamespace \
             CROSS JOIN unnest(i.indkey::int2[]) WITH ORDINALITY AS k(attnum, ord) \
             JOIN pg_attribute a ON a.attrelid = t.oid AND a.attnum = k.attnum \
             WHERE n.nspname = current_schema() AND t.relname = ? \
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
             column_name AS `column`, referenced_table_name AS ref_table, \
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
             a.attname::text AS \"column\", rt.relname::text AS ref_table, \
             ra.attname::text AS ref_column, ck.ord AS col_pos, fk.ord AS ref_pos \
             FROM pg_constraint con \
             JOIN pg_class t ON t.oid = con.conrelid \
             JOIN pg_class rt ON rt.oid = con.confrelid \
             JOIN pg_namespace n ON n.oid = t.relnamespace \
             CROSS JOIN unnest(con.conkey) WITH ORDINALITY AS ck(attnum, ord) \
             JOIN unnest(con.confkey) WITH ORDINALITY AS fk(attnum, ord) ON fk.ord = ck.ord \
             JOIN pg_attribute a ON a.attrelid = t.oid AND a.attnum = ck.attnum \
             JOIN pg_attribute ra ON ra.attrelid = rt.oid AND ra.attnum = fk.attnum \
             WHERE con.contype = 'f' AND n.nspname = current_schema() AND t.relname = ? \
             ORDER BY con.conname, ck.ord"
        }
        // L2 三件三方言全覆盖；catalog 类只走 `catalog_sql`，此表不可达。
        _ => "",
    }
}

// ---------------------------------------------------------------------------
// U-14 批 a：catalog 类（连接树 + 对象详情）。与 L2 的 `sql()` 分表而治：
// catalog kind 支持「kind × 方言 = unsupported」矩阵（`catalog_sql` 返回 None），
// 且响应走 `{kind, dialect, items}` 信封（L2 三件保持裸数组——两代约定，db-guide §10）。
// ---------------------------------------------------------------------------

/// mysql `SHOW CREATE TABLE` 是**唯一**允许标识符内联的成文例外（铁律 1）：
/// SHOW 族不接受绑定参数。防线 = 严格白名单字符集（`[A-Za-z0-9_$]`）+ 反引号引用
/// （校验后本已无反引号，翻倍是纵深）。违规 → Err（`OJ_DB_UNSUPPORTED` 由调用方构造）。
/// REQ-08 §3.2：按探测出的对象类型选 SHOW 动词（table/view/function/procedure）。
/// 未知类型回退 TABLE——保住「对象不存在」时今天的 fail-loud 报错形态。
pub fn mysql_show_verb(object_type: &str) -> &'static str {
    match object_type {
        "view" => "SHOW CREATE VIEW",
        "function" => "SHOW CREATE FUNCTION",
        "procedure" => "SHOW CREATE PROCEDURE",
        _ => "SHOW CREATE TABLE",
    }
}

pub fn mysql_show_create_typed(table: &str, object_type: &str) -> Result<String, String> {
    if table.is_empty()
        || !table
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'$')
    {
        return Err(format!("invalid identifier for SHOW CREATE: {table:?}"));
    }
    Ok(format!(
        "{} `{}`",
        mysql_show_verb(object_type),
        table.replace('`', "``")
    ))
}

/// REQ-09-2：scoped（forSchema）形态——`schema.table` 两段各过 `[A-Za-z0-9_$]` 白名单，
/// 渲染 `` `schema`.`table` ``。仅 Ddl 的 MySQL SHOW 例外路径使用。
pub fn mysql_show_create_scoped(schema_table: &str, object_type: &str) -> Result<String, String> {
    let Some((sch, tbl)) = schema_table.split_once('.') else {
        return Err(format!(
            "invalid scoped identifier for SHOW CREATE: {schema_table:?}"
        ));
    };
    let ok = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'$')
    };
    if !ok(sch) || !ok(tbl) {
        return Err(format!(
            "invalid scoped identifier for SHOW CREATE: {schema_table:?}"
        ));
    }
    Ok(format!(
        "{} `{}`.`{}`",
        mysql_show_verb(object_type),
        sch.replace('`', "``"),
        tbl.replace('`', "``")
    ))
}

/// REQ-08 §3.2：MySQL ddl 路由的对象类型探测（名字跨表/视图/例程三查合一；
/// 绑定参数 ×3，DATABASE() 谓词照铁律）。返回首行 object_type（无 → 空行集）。
pub const MYSQL_OBJECT_TYPE: &str = "SELECT 'table' AS object_type \
     FROM information_schema.tables WHERE table_schema = DATABASE() AND table_name = ? \
     UNION ALL \
     SELECT 'view' FROM information_schema.views \
     WHERE table_schema = DATABASE() AND table_name = ? \
     UNION ALL \
     SELECT LOWER(routine_type) FROM information_schema.routines \
     WHERE routine_schema = DATABASE() AND routine_name = ?";

/// REQ-08 §3.2：PG 例程 DDL 回退（relkind 探测未命中后按名探测 pg_proc；
/// pg_get_functiondef 一条给完整可执行 DDL）。object_type ∈ function|procedure。
pub const PG_ROUTINE_DDL: &str = "SELECT p.proname::text AS name, \
     pg_get_functiondef(p.oid)::text AS ddl, \
     CASE WHEN p.prokind = 'p' THEN 'procedure' ELSE 'function' END AS object_type \
     FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
     WHERE n.nspname = current_schema() AND p.proname = ?";

/// REQ-08 §3.3：pg_get_viewdef 片段补壳为可执行 DDL（剥尾分号；物化走 MATERIALIZED）。
pub fn pg_view_wrap(name: &str, viewdef: &str, materialized: bool) -> String {
    let def = viewdef.trim_end().trim_end_matches(';');
    let kw = if materialized {
        "CREATE MATERIALIZED VIEW"
    } else {
        "CREATE OR REPLACE VIEW"
    };
    format!("{kw} {} AS {def}", pg_quote_ident(name))
}

/// REQ-08 收口：从 `pg_get_triggerdef` 文本扫 timing/events——
/// `CREATE TRIGGER x BEFORE|AFTER|INSTEAD OF INSERT|UPDATE|DELETE ... ON t ...`。
/// 只认触发器子句（第一个 `ON` 之前）的令牌，函数体里的同名词不串味。
pub fn pg_trigger_meta(def: &str) -> (Option<String>, Vec<String>) {
    let words: Vec<&str> = def.split_whitespace().collect();
    let mut timing = None;
    let mut events: Vec<String> = Vec::new();
    let mut i = 0;
    while i < words.len() {
        let w = words[i].to_ascii_uppercase();
        if w == "ON" && timing.is_some() {
            break;
        }
        match w.as_str() {
            "BEFORE" | "AFTER" => timing = Some(w),
            "INSTEAD"
                if words
                    .get(i + 1)
                    .is_some_and(|n| n.eq_ignore_ascii_case("OF")) =>
            {
                timing = Some("INSTEAD OF".into());
                i += 1;
            }
            "INSERT" | "UPDATE" | "DELETE" if !events.contains(&w) => events.push(w),
            _ => {}
        }
        i += 1;
    }
    (timing, events)
}

// ---------------------------------------------------------------------------
// U-14（v0.1.109）：PG `db.catalog("ddl")` 的 relkind 探测 + 路由。
// 探测先行：'v'/'m' → 既有 pg_get_viewdef 路径（输出逐字节不动）；'r'/'p' → 拼接器；
// 其余（'f'/'i'/'S'/'c'/'t'）→ items:[]（假 DDL 比空结果更糟，绝不落拼接器）。
// ---------------------------------------------------------------------------

/// relkind 探测（单参数 = 表名；`relkind` 是 `char` 域，投影必须 `::text`）。
pub const PG_DDL_RELKIND: &str = "SELECT c.relkind::text AS relkind FROM pg_class c \
     JOIN pg_namespace n ON n.oid = c.relnamespace \
     WHERE n.nspname = current_schema() AND c.relname = ?";

/// PG ddl 路由（spec relkind 矩阵钉死）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PgDdlRoute {
    /// 视图 / 物化视图 → 既有 pg_get_viewdef 路径。
    View,
    /// 表 / 分区表 → 拼接器（'p' 附 pg_get_partkeydef，best-effort）。
    Table,
    /// 外部表 / 索引 / 序列 / 复合类型 / toast 表，或不存在 → 空结果。
    Empty,
}

pub fn pg_ddl_route(relkind: Option<&str>) -> PgDdlRoute {
    match relkind {
        Some("v") | Some("m") => PgDdlRoute::View,
        Some("r") | Some("p") => PgDdlRoute::Table,
        _ => PgDdlRoute::Empty,
    }
}

// ---------------------------------------------------------------------------
// U-14（v0.1.109）PG 拼接器：relkind 判定为表后的 4+1 条查询 + 纯渲染器。
// 守卫（introspect.rs 铁律）：current_schema() 恰一次、`::text` 覆盖 name 域投影、
// `?` 恰 1、等值匹配无 LIKE。容量防线在调用方（行数触 MAX_ROWS → OJ_DB_CAPACITY）。
// ---------------------------------------------------------------------------

/// 列：attnum > 0 AND NOT attisdropped（dropped 列是占位行）；identity / generated /
/// serial 序列名经 pg_depend（'a' auto 依赖）——**绝不解析 nextval 文本**。
pub const PG_STITCH_COLUMNS: &str = "SELECT a.attname::text AS name, format_type(a.atttypid, a.atttypmod) AS native_type, \
     a.attnotnull AS not_null, pg_get_expr(def.adbin, def.adrelid)::text AS \"default\", \
     (EXISTS (SELECT 1 FROM pg_constraint con WHERE con.conrelid = a.attrelid \
          AND con.contype = 'p' AND a.attnum = ANY(con.conkey))) AS is_pk, \
     a.attidentity::text AS identity, a.attgenerated::text AS generated, \
     s.relname::text AS seq_name, a.attnum AS pos \
     FROM pg_attribute a \
     JOIN pg_class cl ON cl.oid = a.attrelid \
     JOIN pg_namespace n ON n.oid = cl.relnamespace \
     LEFT JOIN pg_attrdef def ON def.adrelid = a.attrelid AND def.adnum = a.attnum \
     LEFT JOIN pg_depend d ON d.refclassid = 'pg_class'::regclass \
          AND d.refobjid = cl.oid AND d.refobjsubid = a.attnum \
          AND d.classid = 'pg_class'::regclass AND d.deptype = 'a' \
     LEFT JOIN pg_class s ON s.oid = d.objid AND s.relkind = 'S' \
     WHERE n.nspname = current_schema() AND cl.relname = ? \
           AND a.attnum > 0 AND NOT a.attisdropped \
     ORDER BY a.attnum";

/// 约束：contype p/u/c/f/x + pg_get_constraintdef（**含排他 'x' 与 FK 'f'**——重放丢约束
/// 违反不静默口径）。PG18 把 NOT NULL 收编为 contype='n'，届时两路径口径差一行。
pub const PG_STITCH_CONSTRAINTS: &str = "SELECT con.conname::text AS name, \
     CASE con.contype WHEN 'p' THEN 'pk' WHEN 'u' THEN 'uq' WHEN 'c' THEN 'ck' \
          WHEN 'f' THEN 'fk' ELSE 'x' END AS type, \
     pg_get_constraintdef(con.oid)::text AS definition \
     FROM pg_constraint con \
     JOIN pg_class t ON t.oid = con.conrelid \
     JOIN pg_namespace n ON n.oid = t.relnamespace \
     WHERE n.nspname = current_schema() AND t.relname = ? \
           AND con.contype IN ('p','u','c','f','x') \
     ORDER BY con.conname";

/// 索引定义：**必须排除约束支撑索引**（否则与 ADD CONSTRAINT 重复产出，重放名冲突必炸）。
/// 粗过滤 `NOT indisprimary AND NOT indisunique` 会误杀非约束 CREATE UNIQUE INDEX，禁止。
pub const PG_STITCH_INDEXES: &str = "SELECT c.relname::text AS name, pg_get_indexdef(i.indexrelid)::text AS definition \
     FROM pg_class t \
     JOIN pg_index i ON i.indrelid = t.oid \
     JOIN pg_class c ON c.oid = i.indexrelid \
     JOIN pg_namespace n ON n.oid = t.relnamespace \
     WHERE n.nspname = current_schema() AND t.relname = ? \
           AND NOT EXISTS (SELECT 1 FROM pg_constraint con WHERE con.conindid = i.indexrelid) \
     ORDER BY c.relname";

/// 列注释（col_description；只出有注释的列）。
pub const PG_STITCH_COLUMN_COMMENTS: &str = "SELECT a.attname::text AS name, col_description(a.attrelid, a.attnum)::text AS comment \
     FROM pg_attribute a \
     JOIN pg_class cl ON cl.oid = a.attrelid \
     JOIN pg_namespace n ON n.oid = cl.relnamespace \
     WHERE n.nspname = current_schema() AND cl.relname = ? \
           AND a.attnum > 0 AND NOT a.attisdropped \
           AND col_description(a.attrelid, a.attnum) IS NOT NULL \
     ORDER BY a.attnum";

/// 分区键（relkind 'p'；非分区表 pg_get_partkeydef 返 NULL → None，渲染跳过）。
pub const PG_STITCH_PARTKEY: &str = "SELECT pg_get_partkeydef(c.oid)::text AS def \
     FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
     WHERE n.nspname = current_schema() AND c.relname = ?";

/// 表注释：**双参数形态** obj_description(oid, 'pg_class')（单参数已废弃）。
pub const PG_STITCH_TABLE_COMMENT: &str = "SELECT obj_description(c.oid, 'pg_class')::text AS comment \
     FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
     WHERE n.nspname = current_schema() AND c.relname = ?";

/// 拼接器列行 → 归一形态（与 listColumns 同词汇 + identity/seqName/generated）。
/// dropped 占位行纵深跳过（SQL 已过滤，归一再挡一道）。generated 列的表达式在
/// `default` 列（pg_get_expr），归一为 generatedExpr 且**不落 default**。
pub fn pg_stitch_columns(rows: Vec<Value>) -> Vec<Value> {
    rows.iter()
        .filter(|r| !str_of(r, "name").starts_with("........pg.dropped."))
        .map(|r| {
            let generated = str_of(r, "generated");
            let default = opt_str(r.get("default"));
            let mut o = serde_json::Map::new();
            o.insert("name".into(), json!(str_of(r, "name")));
            o.insert("nativeType".into(), json!(str_of(r, "native_type")));
            o.insert("nullable".into(), json!(!truthy(r.get("not_null"))));
            o.insert("primaryKey".into(), json!(truthy(r.get("is_pk"))));
            o.insert("pos".into(), json!(num_of(r, "pos")));
            if generated == "s" {
                o.insert("generated".into(), json!("stored"));
                if let Some(e) = default {
                    o.insert("generatedExpr".into(), json!(e));
                }
            } else if let Some(d) = default {
                o.insert("default".into(), json!(d));
            }
            match str_of(r, "identity").as_str() {
                "a" => {
                    o.insert("identity".into(), json!("always"));
                }
                "d" => {
                    o.insert("identity".into(), json!("byDefault"));
                }
                _ => {}
            }
            if let Some(s) = opt_str(r.get("seq_name")) {
                o.insert("seqName".into(), json!(s));
            }
            Value::Object(o)
        })
        .collect()
}

/// 约束行 → [{name, type, definition}]。
pub fn pg_stitch_constraints(rows: Vec<Value>) -> Vec<Value> {
    rows.iter()
        .map(|r| {
            json!({
                "name": str_of(r, "name"),
                "type": str_of(r, "type"),
                "definition": opt_str(r.get("definition")),
            })
        })
        .collect()
}

/// 索引行 → [{name, definition}]。
pub fn pg_stitch_indexes(rows: Vec<Value>) -> Vec<Value> {
    rows.iter()
        .map(|r| {
            json!({
                "name": str_of(r, "name"),
                "definition": opt_str(r.get("definition")),
            })
        })
        .collect()
}

/// 列注释行 → [{column, comment}]。
pub fn pg_stitch_column_comments(rows: Vec<Value>) -> Vec<Value> {
    rows.iter()
        .filter_map(|r| {
            opt_str(r.get("comment")).map(|c| json!({"column": str_of(r, "name"), "comment": c}))
        })
        .collect()
}

/// PG 标识符引用（双引号 + 翻倍）——拼接器输出只服务 PG，不走路由。
pub fn pg_quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// 字面量引用（单引号 + 翻倍）。
fn pg_quote_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// 渲染器（纯函数）：serial 前置 `CREATE SEQUENCE IF NOT EXISTS`，表后补
/// `ALTER SEQUENCE ... OWNED BY`（DROP TABLE 后序列不成孤儿）；identity 列渲染
/// GENERATED {ALWAYS|BY DEFAULT} AS IDENTITY（自管序列，不产序列语句）；
/// generated 列渲染 GENERATED ALWAYS AS (expr) STORED。语句以 ";\n" 连接。
pub fn pg_create_table(
    name: &str,
    cols: &[Value],
    cons: &[Value],
    idxs: &[Value],
    comments: &[Value],
    table_comment: Option<&str>,
    partkeydef: Option<&str>,
) -> String {
    let t = pg_quote_ident(name);
    let mut stmts: Vec<String> = Vec::new();
    // serial 序列前置（identity 列自管序列，不在此列）。
    for c in cols {
        if c.get("identity").is_none() && c.get("seqName").is_some() {
            stmts.push(format!(
                "CREATE SEQUENCE IF NOT EXISTS {}",
                pg_quote_ident(&str_of(c, "seqName"))
            ));
        }
    }
    // CREATE TABLE 列定义。
    let col_sql: Vec<String> = cols
        .iter()
        .map(|c| {
            let mut s = format!(
                "{} {}",
                pg_quote_ident(&str_of(c, "name")),
                str_of(c, "nativeType")
            );
            if let Some(id) = c.get("identity").and_then(|v| v.as_str()) {
                s.push_str(match id {
                    "always" => " GENERATED ALWAYS AS IDENTITY",
                    _ => " GENERATED BY DEFAULT AS IDENTITY",
                });
            } else if let Some(expr) = c.get("generatedExpr").and_then(|v| v.as_str()) {
                s.push_str(&format!(" GENERATED ALWAYS AS ({expr}) STORED"));
            }
            if c.get("nullable") == Some(&json!(false)) {
                s.push_str(" NOT NULL");
            }
            if c.get("identity").is_none()
                && c.get("generatedExpr").is_none()
                && let Some(d) = c.get("default").and_then(|v| v.as_str())
            {
                s.push_str(&format!(" DEFAULT {d}"));
            }
            s
        })
        .collect();
    let mut create = format!("CREATE TABLE {t} (\n  {}\n)", col_sql.join(",\n  "));
    // 分区表（relkind 'p'）：pg_get_partkeydef 尽力而为——子分区不在此产出。
    if let Some(def) = partkeydef {
        create.push_str(&format!(" PARTITION BY {def}"));
    }
    stmts.push(create);
    // 约束 / 索引 / 注释。
    for con in cons {
        stmts.push(format!(
            "ALTER TABLE {t} ADD CONSTRAINT {} {}",
            pg_quote_ident(&str_of(con, "name")),
            str_of(con, "definition")
        ));
    }
    for idx in idxs {
        if let Some(def) = idx.get("definition").and_then(|v| v.as_str()) {
            stmts.push(def.to_string());
        }
    }
    for cm in comments {
        if let Some(txt) = cm.get("comment").and_then(|v| v.as_str()) {
            stmts.push(format!(
                "COMMENT ON COLUMN {}.{} IS {}",
                t,
                pg_quote_ident(&str_of(cm, "column")),
                pg_quote_literal(txt)
            ));
        }
    }
    if let Some(txt) = table_comment {
        stmts.push(format!("COMMENT ON TABLE {t} IS {}", pg_quote_literal(txt)));
    }
    // serial 序列归属后置。
    for c in cols {
        if c.get("identity").is_none()
            && let Some(seq) = c.get("seqName").and_then(|v| v.as_str())
        {
            stmts.push(format!(
                "ALTER SEQUENCE {} OWNED BY {}.{}",
                pg_quote_ident(seq),
                t,
                pg_quote_ident(&str_of(c, "name"))
            ));
        }
    }
    stmts.join(";\n") + ";"
}

/// catalog 类的方言 SQL（`None` = kind × 方言 unsupported——**报错不静默**，能力矩阵见
/// db-guide §10）。参数位约定：吃表名的 kind 固定 1 个 `?`；全库清单 0 个。
/// pg 铁律 2/3 照守（`::text` / `current_schema()` 限定）。
pub fn catalog_sql(kind: Kind, d: Dialect) -> Option<&'static str> {
    match (kind, d) {
        // ---------------- views（清单 + 定义）----------------
        (Kind::Views, Dialect::Sqlite) => Some(
            "SELECT name AS name, 'view' AS type, sql AS ddl \
                  FROM sqlite_master WHERE type = 'view' ORDER BY name",
        ),
        (Kind::Views, Dialect::MySql) => Some(
            "SELECT table_name AS name, 'view' AS type, view_definition AS ddl \
                  FROM information_schema.views \
                  WHERE table_schema = DATABASE() ORDER BY table_name",
        ),
        (Kind::Views, Dialect::Postgres) => Some(
            "SELECT c.relname::text AS name, 'view' AS type, \
                  pg_get_viewdef(c.oid)::text AS ddl \
                  FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                  WHERE c.relkind IN ('v','m') AND n.nspname = current_schema() \
                  ORDER BY c.relname",
        ),
        // ---------------- routines（函数/存储过程清单；REQ-08 §3.1 元数据补全）----------------
        (Kind::Routines, Dialect::MySql) => Some(
            "SELECT routine_name AS name, routine_type AS type, \
                  routine_definition AS source, data_type AS returns, \
                  is_deterministic AS `deterministic`, security_type AS `security`, \
                  routine_comment AS comment \
                  FROM information_schema.routines \
                  WHERE routine_schema = DATABASE() ORDER BY routine_name, routine_type",
        ),
        (Kind::Routines, Dialect::Postgres) => Some(
            "SELECT p.proname::text AS name, \
                  CASE WHEN p.prokind = 'p' THEN 'PROCEDURE' ELSE 'FUNCTION' END AS type, \
                  pg_get_function_arguments(p.oid)::text AS args, \
                  pg_get_function_result(p.oid)::text AS returns, \
                  p.prosrc::text AS source, l.lanname::text AS language, \
                  p.provolatile::text AS volatility, p.prosecdef AS secdef, \
                  obj_description(p.oid, 'pg_proc')::text AS comment \
                  FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
                  JOIN pg_language l ON l.oid = p.prolang \
                  WHERE n.nspname = current_schema() ORDER BY p.proname",
        ),
        // ---------------- sequences（sqlite 无 catalog → unsupported；mysql 为模拟）----------------
        (Kind::Sequences, Dialect::MySql) => Some(
            "SELECT table_name AS name, 'auto_increment' AS type \
                  FROM information_schema.tables \
                  WHERE table_schema = DATABASE() AND table_type = 'BASE TABLE' \
                        AND auto_increment IS NOT NULL ORDER BY table_name",
        ),
        (Kind::Sequences, Dialect::Postgres) => Some(
            "SELECT sequencename::text AS name, 'sequence' AS type \
                  FROM pg_sequences WHERE schemaname = current_schema() ORDER BY sequencename",
        ),
        // ---------------- ddl（单对象源码；mysql 走 SHOW 例外，见 `mysql_show_create`）----------------
        (Kind::Ddl, Dialect::Sqlite) => {
            Some("SELECT name AS name, sql AS ddl FROM sqlite_master WHERE name = ?")
        }
        (Kind::Ddl, Dialect::Postgres) => {
            // pg 无原生 `SHOW CREATE TABLE`（真表 DDL 需拼接器）——批 a 只覆盖视图定义，
            // 表 → 空结果不报错（能力矩阵如实：`ddl` × pg = 仅视图）。
            Some(
                "SELECT c.relname::text AS name, pg_get_viewdef(c.oid)::text AS ddl \
                  FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                  WHERE c.relkind IN ('v','m') AND n.nspname = current_schema() \
                        AND c.relname = ?",
            )
        }
        // ---------------- comment（列注释；sqlite 无注释语义 → unsupported）----------------
        (Kind::Comment, Dialect::MySql) => Some(
            "SELECT table_name AS table_name, column_name AS column_name, \
                  column_comment AS comment \
                  FROM information_schema.columns \
                  WHERE table_schema = DATABASE() AND table_name = ? \
                  ORDER BY ordinal_position",
        ),
        (Kind::Comment, Dialect::Postgres) => Some(
            "SELECT c.relname::text AS table_name, a.attname::text AS column_name, \
                  col_description(a.attrelid, a.attnum)::text AS comment \
                  FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid \
                  JOIN pg_namespace n ON n.oid = c.relnamespace \
                  WHERE a.attnum > 0 AND NOT a.attisdropped AND n.nspname = current_schema() \
                        AND c.relname = ?",
        ),
        // ---------------- constraints（PK/UQ/CHECK；FK 走既有 ForeignKeys）----------------
        // sqlite：CHECK 只存在于建表文本，目录无来源 → unsupported（文本解析归批 b）。
        (Kind::Constraints, Dialect::MySql) => Some(
            "SELECT tc.constraint_name AS name, tc.constraint_type AS type, \
                  NULL AS definition, kcu.column_name AS `column`, kcu.ordinal_position AS pos \
                  FROM information_schema.table_constraints tc \
                  JOIN information_schema.key_column_usage kcu \
                       ON kcu.constraint_schema = tc.constraint_schema \
                      AND kcu.constraint_name = tc.constraint_name \
                      AND kcu.table_name = tc.table_name \
                  WHERE tc.table_schema = DATABASE() AND tc.table_name = ? \
                        AND tc.constraint_type IN ('PRIMARY KEY','UNIQUE') \
                  ORDER BY tc.constraint_name, kcu.ordinal_position",
        ),
        (Kind::Constraints, Dialect::Postgres) => Some(
            "SELECT con.conname::text AS name, \
                  CASE con.contype WHEN 'p' THEN 'pk' WHEN 'u' THEN 'uq' ELSE 'ck' END AS type, \
                  pg_get_constraintdef(con.oid)::text AS definition, \
                  NULL AS \"column\", NULL AS pos \
                  FROM pg_constraint con \
                  JOIN pg_class t ON t.oid = con.conrelid \
                  JOIN pg_namespace n ON n.oid = t.relnamespace \
                  WHERE con.contype IN ('p','u','c') AND n.nspname = current_schema() \
                        AND t.relname = ? ORDER BY con.conname",
        ),
        // ---------------- stats（估算行数/大小；sqlite 无 catalog 来源 → unsupported）----------------
        (Kind::Stats, Dialect::MySql) => Some(
            "SELECT table_name AS name, \
                  COALESCE(table_rows, 0) AS rows_estimate, \
                  COALESCE(data_length + index_length, 0) AS size_bytes \
                  FROM information_schema.tables \
                  WHERE table_schema = DATABASE() AND table_type = 'BASE TABLE' \
                  ORDER BY table_name",
        ),
        (Kind::Stats, Dialect::Postgres) => Some(
            "SELECT c.relname::text AS name, \
                  GREATEST(c.reltuples, 0)::bigint::text AS rows_estimate, \
                  pg_total_relation_size(c.oid)::text AS size_bytes \
                  FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                  WHERE c.relkind = 'r' AND n.nspname = current_schema() ORDER BY c.relname",
        ),
        // ---------------- REQ-10 一期：processlist / users / roles（全实例/全库，无范围谓词）----------------
        (Kind::Processlist, Dialect::Postgres) => Some(
            "SELECT pid AS id, usename::text AS \"user\", datname::text AS database, \
                  client_addr::text AS host, state, \
                  EXTRACT(EPOCH FROM now() - query_start)::bigint AS elapsed_sec, \
                  query \
                  FROM pg_stat_activity",
        ),
        (Kind::Processlist, Dialect::MySql) => Some(
            "SELECT id, user, db AS `database`, host, command AS state, \
                  time AS elapsed_sec, info AS query \
                  FROM information_schema.processlist",
        ),
        (Kind::Users, Dialect::Postgres) => {
            Some("SELECT usename::text AS name, NULL::text AS host FROM pg_user ORDER BY usename")
        }
        (Kind::Users, Dialect::MySql) => {
            Some("SELECT user AS name, host FROM mysql.user ORDER BY user, host")
        }
        (Kind::Roles, Dialect::Postgres) => {
            Some("SELECT rolname::text AS name FROM pg_roles ORDER BY rolname")
        }
        (Kind::Roles, Dialect::MySql) => Some(
            "SELECT DISTINCT role_name AS name FROM information_schema.applicable_roles \
                  ORDER BY role_name",
        ),
        // ---------------- REQ-08 收口：triggers 清单（sqlite 无目录来源 → unsupported）----------------
        (Kind::Triggers, Dialect::Postgres) => Some(
            "SELECT t.tgname::text AS name, c.relname::text AS \"table\", \
                  t.tgenabled::text AS tgenabled, \
                  pg_get_triggerdef(t.oid)::text AS definition \
                  FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid \
                  JOIN pg_namespace n ON n.oid = c.relnamespace \
                  WHERE NOT t.tgisinternal AND n.nspname = current_schema() \
                  ORDER BY t.tgname",
        ),
        (Kind::Triggers, Dialect::MySql) => Some(
            "SELECT trigger_name AS name, event_object_table AS `table`, \
                  action_timing AS timing, event_manipulation AS event, \
                  action_statement AS body, action_order AS ord \
                  FROM information_schema.triggers \
                  WHERE trigger_schema = DATABASE() ORDER BY trigger_name",
        ),
        _ => None,
    }
}

/// catalog 行 → 统一 item 形态（截断到 [`MAX_ROWS`]；三方言同构）。
/// constraints 按 `name` 聚合 `columns`（mysql 的 key_column_usage 是按列多行；
/// pg 的 definition 自带列信息，`column` 为 NULL 不入聚合）。
///
/// 返回 `(items, truncated)`：truncated = 入参行数超 `MAX_ROWS`（REQ-08 §3.5 信令；
/// constraints 组的组数截断取行数口径近似——组数 ≤ 行数，旗标偏保守不丢信令）。
pub fn catalog(kind: Kind, rows: Vec<Value>) -> (Vec<Value>, bool) {
    let truncated = rows.len() > MAX_ROWS;
    match kind {
        Kind::Constraints => {
            let mut out: Vec<Value> = Vec::new();
            let mut cur: Option<String> = None;
            for r in rows.iter() {
                let name = str_of(r, "name");
                if cur.as_deref() != Some(name.as_str()) {
                    if out.len() >= MAX_ROWS {
                        break;
                    }
                    cur = Some(name.clone());
                    // 词表跨方言统一（评审 R.1）：mysql 的 "PRIMARY KEY"/"UNIQUE" 归一为
                    // pg 口径的 pk/uq/ck。
                    let t = str_of(r, "type");
                    let ty = match t.to_uppercase().as_str() {
                        "PRIMARY KEY" => "pk",
                        "UNIQUE" => "uq",
                        "CHECK" => "ck",
                        _ => &t,
                    };
                    out.push(json!({
                        "name": name,
                        "type": ty,
                        "definition": opt_str(r.get("definition")),
                        "columns": Vec::<Value>::new(),
                    }));
                }
                if let Some(last) = out.last_mut()
                    && let Some(col) = opt_str(r.get("column"))
                {
                    last["columns"].as_array_mut().unwrap().push(json!(col));
                }
            }
            (out, truncated)
        }
        _ => (
            rows.iter()
                .take(MAX_ROWS)
                .map(|r| match kind {
                    Kind::Views | Kind::Ddl => {
                        // mysql SHOW CREATE 的列名是 `Table`/`Create Table`（View 同理）——
                        // name/ddl 都要兜回落（评审 R.1：mysql 侧 name 恒空串）。
                        // REQ-08 §3.2：Create Function/Procedure 同例；objectType 随
                        // PG 例程探测透传（mysql SHOW 结果不带类型 → null，devkit 登记）。
                        let name = {
                            let n = str_of(r, "name");
                            if n.is_empty() { str_of(r, "Table") } else { n }
                        };
                        json!({
                            "name": name,
                            "ddl": opt_str(r.get("ddl")).or_else(|| opt_str(r.get("Create Table")))
                                .or_else(|| opt_str(r.get("Create View")))
                                .or_else(|| opt_str(r.get("Create Function")))
                                .or_else(|| opt_str(r.get("Create Procedure"))),
                            "objectType": opt_str(r.get("object_type")),
                        })
                    }
                    Kind::Routines => {
                        // REQ-08 §3.1：volatility 跨方言归一（pg provolatile / mysql deterministic）。
                        let vol_raw = {
                            let v = str_of(r, "volatility");
                            if v.is_empty() {
                                str_of(r, "deterministic")
                            } else {
                                v
                            }
                        };
                        let volatility = match vol_raw.as_str() {
                            "i" => Some("immutable"),
                            "s" => Some("stable"),
                            "v" => Some("volatile"),
                            "YES" => Some("deterministic"),
                            "NO" => Some("nondeterministic"),
                            _ => None,
                        };
                        let secdef = r.get("secdef").and_then(Value::as_bool).or_else(|| {
                            match str_of(r, "security").as_str() {
                                "DEFINER" => Some(true),
                                "INVOKER" => Some(false),
                                _ => None,
                            }
                        });
                        json!({
                            "name": str_of(r, "name"),
                            "type": str_of(r, "type"),
                            "args": opt_str(r.get("args")),
                            "returns": opt_str(r.get("returns")),
                            "source": opt_str(r.get("source")),
                            "language": opt_str(r.get("language")),
                            "volatility": volatility,
                            "securityDefiner": secdef,
                            "comment": opt_str(r.get("comment")),
                        })
                    }
                    Kind::Sequences => json!({
                        "name": str_of(r, "name"),
                        "type": str_of(r, "type"),
                    }),
                    Kind::Comment => json!({
                        "table": str_of(r, "table_name"),
                        "column": str_of(r, "column_name"),
                        "comment": opt_str(r.get("comment")),
                    }),
                    Kind::Stats => json!({
                        "name": str_of(r, "name"),
                        "rowsEstimate": num_of(r, "rows_estimate"),
                        "sizeBytes": num_of(r, "size_bytes"),
                    }),
                    Kind::Processlist => json!({
                        "id": r.get("id").cloned().unwrap_or(Value::Null),
                        "user": str_of(r, "user"),
                        "database": opt_str(r.get("database")),
                        "host": opt_str(r.get("host")),
                        "state": opt_str(r.get("state")),
                        "elapsedSec": r.get("elapsed_sec").cloned().unwrap_or(Value::Null),
                        "query": opt_str(r.get("query")),
                    }),
                    Kind::Users => json!({
                        "name": str_of(r, "name"),
                        "host": opt_str(r.get("host")),
                    }),
                    Kind::Roles => json!({
                        "name": str_of(r, "name"),
                    }),
                    Kind::Triggers => {
                        // PG 行带 definition（pg_get_triggerdef）→ timing/events 令牌扫；
                        // MySQL 行直映 information_schema 列（events 单元素数组）。
                        if let Some(def) = opt_str(r.get("definition")) {
                            let (timing, events) = pg_trigger_meta(&def);
                            json!({
                                "name": str_of(r, "name"),
                                "table": str_of(r, "table"),
                                "timing": timing,
                                "events": events,
                                "body": Value::Null,
                                "order": Value::Null,
                                "enabled": str_of(r, "tgenabled") == "O",
                            })
                        } else {
                            let ev = str_of(r, "event");
                            json!({
                                "name": str_of(r, "name"),
                                "table": str_of(r, "table"),
                                "timing": opt_str(r.get("timing")),
                                "events": if ev.is_empty() {
                                    Vec::<Value>::new()
                                } else {
                                    vec![json!(ev)]
                                },
                                "body": opt_str(r.get("body")),
                                "order": r.get("ord").cloned().unwrap_or(Value::Null),
                                "enabled": Value::Null,
                            })
                        }
                    }
                    // 穷尽守卫：Constraints 被外层 match 的聚合臂分流；L2 三件只走 op 的
                    // columns/indexes/foreign_keys 函数（parse_catalog 白名单不放行）——
                    // 此处显式 unreachable，新增 catalog kind 漏 shape 臂即编译失败。
                    Kind::Constraints | Kind::Columns | Kind::Indexes | Kind::ForeignKeys => {
                        unreachable!("constraints 外层已分流；L2 kinds 不进 catalog()")
                    }
                })
                .collect(),
            truncated,
        ),
    }
}

// ---------------------------------------------------------------------------
// F1（批次 4 / REQ-04，v0.1.105）：库-schema 清单面 + forSchema 范围变换。
// 清单面走独立 op（不加 catalog kind——避免 `db.catalog("schemas")` 与 `db.listSchemas()`
// 双入口，Ruling 见计划 §2d）；系统目录过滤口径统一在本模块，调用方零 if-else。
// ---------------------------------------------------------------------------

/// 库 / schema 清单面（REQ-04 §5.1）。mysql schema≡database（同一份 schemata）；
/// pg schemas = 当前 database 的 namespace，databases = 集群共享目录 pg_database；
/// sqlite 无 schema 概念，两条腿都给 `PRAGMA database_list`（main + attached，
/// 唯一诚实的答案）。未知方言由调用方过 `require_known_dialect` 门控。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Listing {
    Schemas,
    Databases,
}

pub fn listing_sql(l: Listing, d: Dialect) -> Option<&'static str> {
    match (l, d) {
        (Listing::Schemas | Listing::Databases, Dialect::Sqlite) => Some(
            "SELECT name AS name, (name = 'main') AS is_default \
             FROM pragma_database_list ORDER BY name",
        ),
        (Listing::Schemas | Listing::Databases, Dialect::MySql) => Some(
            "SELECT schema_name AS name, (schema_name = DATABASE()) AS is_default \
             FROM information_schema.schemata \
             WHERE schema_name NOT IN ('information_schema','mysql','performance_schema','sys') \
             ORDER BY schema_name",
        ),
        (Listing::Schemas, Dialect::Postgres) => Some(
            "SELECT schema_name::text AS name, schema_owner::text AS owner, \
             (schema_name = current_schema()) AS is_default \
             FROM information_schema.schemata \
             WHERE schema_name <> 'pg_catalog' AND schema_name <> 'information_schema' \
                   AND schema_name NOT LIKE 'pg_toast%' \
             ORDER BY schema_name",
        ),
        (Listing::Databases, Dialect::Postgres) => Some(
            "SELECT d.datname::text AS name, r.rolname::text AS owner, \
             (d.datname = current_database()) AS is_default, \
             obj_description(d.oid, 'pg_database')::text AS comment \
             FROM pg_database d JOIN pg_roles r ON r.oid = d.datdba \
             WHERE NOT d.datistemplate ORDER BY d.datname",
        ),
    }
}

/// 清单行 → 跨方言同构 `{name, owner?, isDefault?, comment?}`（可选键 NULL 不落）；
/// `is_default` 由 SQL 投影、布尔走宽容判定（三方言 0/1/'YES'/true 不统一）。
pub fn listing(rows: Vec<Value>) -> Vec<Value> {
    rows.iter()
        .take(MAX_ROWS)
        .map(|r| {
            let mut o = serde_json::Map::new();
            o.insert("name".to_string(), json!(str_of(r, "name")));
            if let Some(owner) = opt_str(r.get("owner")) {
                o.insert("owner".to_string(), json!(owner));
            }
            o.insert("isDefault".to_string(), json!(truthy(r.get("is_default"))));
            if let Some(c) = opt_str(r.get("comment")) {
                o.insert("comment".to_string(), json!(c));
            }
            Value::Object(o)
        })
        .collect()
}

/// forSchema 范围变换（REQ-04 §5.2 方案 B）：活动 scope 时把范围谓词
/// `DATABASE()` / `current_schema()` 改写为绑定参数（`?`），由调用方把 scope 值
/// **前置**进参数数组（范围谓词在 WHERE 中先于表名谓词出现）。
/// **该顺序是调用方 `params.insert(0, scope)` 的硬契约**——由
/// [`tests::scope_predicate_precedes_first_param`] 钉死：新增/修改常量时不得把
/// 表名 `?` 写到范围谓词之前（PG scoped 内省曾因此错绑参数、静默空结果，
/// BUG-pg-scoped-introspect-param-binding，v0.1.106 实测暴露）。
/// sqlite → None（无 schema 概念，调用方报 `OJ_DB_UNSUPPORTED` capability=for_schema）。
/// mysql SHOW 例外（Ddl）不接受绑定参数，调用方在 scoped 模式直接 unsupported、不经此函数。
pub fn scoped_sql(base: &str, d: Dialect) -> Option<String> {
    match d {
        Dialect::Sqlite => None,
        Dialect::MySql => Some(base.replace("DATABASE()", "?")),
        Dialect::Postgres => Some(base.replace("current_schema()", "?")),
    }
}

/// 结果 → JS 契约形态（三方言同构），并截断到 [`MAX_ROWS`]。
/// identity 为**加性**输出（v0.1.109）：仅 PG 投影了 `identity` 列；
/// 'a'→always、'd'→byDefault，其余/无投影不落键（mysql/sqlite 零行为变化）。
pub fn columns(rows: Vec<Value>) -> Vec<Value> {
    rows.iter()
        .take(MAX_ROWS)
        .map(|r| {
            let mut o = serde_json::Map::new();
            o.insert("name".into(), json!(str_of(r, "name")));
            o.insert("nativeType".into(), json!(str_of(r, "native_type")));
            o.insert("nullable".into(), json!(!truthy(r.get("not_null"))));
            o.insert(
                "default".into(),
                opt_str(r.get("default"))
                    .map(Value::String)
                    .unwrap_or(Value::Null),
            );
            o.insert("primaryKey".into(), json!(truthy(r.get("is_pk"))));
            match r.get("identity").and_then(|v| v.as_str()) {
                Some("a") => {
                    o.insert("identity".into(), json!("always"));
                }
                Some("d") => {
                    o.insert("identity".into(), json!("byDefault"));
                }
                _ => {}
            }
            Value::Object(o)
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
#[path = "introspect_tests.rs"]
mod tests;
