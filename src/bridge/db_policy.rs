//! db 访问控制核心（L1，v0.1.89）——库级授权 + 语句风险分类 + 危险语句门 + 限额。
//!
//! 对齐 `../dbx` 的访问控制形态（`SqlRisk` 四分类 + 连接 `read_only` 位 + 危险语句门 +
//! 行数上限 + 决策可观测），落在本项目的宿主侧：策略按 db 别名声明，判定在 op 边界完成。
//!
//! # 为什么自带一套词法器（不复用 `guard.rs` 的）
//!
//! `guard::strip_literals` 被 `check_tenant_raw` 用来判「SQL 里有没有 tenant_id」
//! （`guard.rs:448`）。若让共享词法器保留 MySQL 可执行注释 `/*! … */`，则
//! `DELETE FROM t WHERE 1=1 /*! tenant_id */` 会在 deny 档**借注释伪造租户命中** →
//! 跨租户全表删除。故本文件的扫描器**独立实现、语义有意不同**（保留 `/*! */` 内容），
//! `guard.rs` 一行不改。两者是并列消费者，不是同一套东西。
//!
//! # 边界（不是安全边界）
//!
//! - `read_only` 是**宿主侧文本级判定**；数据库侧真正的写防线仍是账号 GRANT。
//! - `max_rows` 对 raw query 是**取回后截断**（响应整形），不是限流/DoS 防线。
//! - 分类是 best-effort 词法；无法定界一律 `Unknown` 并按最严档处置（fail-closed）。
//! - 本策略**只约束 JS 提交的 SQL**：平台自生成 SQL（`op_db_next_seq` 序列表 DDL、
//!   `op_db_list_tables` 内省）与 CLI 运维通道（`oj migrate` 等，不经 bridge）不适用。

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex, OnceLock};

use deno_core::OpState;
use deno_error::JsErrorBox;
use serde::Deserialize;

use super::{ReqState, StableState};

/// `max_rows` 的绝对上界（配置可上调但不能无界；装配期校验）。
pub const MAX_ROWS_CAP: u32 = 100_000;

/// 处置档位（与既有 `SqlGuard` 同形的三档；档位类型归一是后续待做 T2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Enforce {
    /// 不设防（默认；与现状完全一致）。
    #[default]
    Off,
    /// 只告警放行（灰度用）。
    Warn,
    /// 拒绝。
    Deny,
}

/// 语句风险分类（对齐 dbx `SqlRisk`，多一枚 `Unknown` 承载 fail-closed）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Risk {
    ReadOnly,
    Write,
    Ddl,
    Transaction,
    /// 无法判定 → 按最严档处置。
    Unknown,
}

impl Risk {
    pub fn as_str(self) -> &'static str {
        match self {
            Risk::ReadOnly => "readonly",
            Risk::Write => "write",
            Risk::Ddl => "ddl",
            Risk::Transaction => "tx",
            Risk::Unknown => "unknown",
        }
    }
}

/// 违规原因（稳定串，供日志/告警 grep；同时进错误文案）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// 当前主体未被授权访问该库。
    NotGranted,
    /// 库为只读，语句被判为写类。
    ReadOnly,
    /// DDL 未放开。
    DdlDenied,
    /// 危险写（无 WHERE / 多表 / 恒真谓词）。
    DangerousWrite,
    /// 分类失败（fail-closed）。
    UnknownRisk,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::NotGranted => "db_not_granted",
            Reason::ReadOnly => "db_read_only",
            Reason::DdlDenied => "db_ddl_denied",
            Reason::DangerousWrite => "db_dangerous_write",
            Reason::UnknownRisk => "db_unknown_risk",
        }
    }
}

/// 分类结果。
#[derive(Debug, Clone, Copy)]
pub struct Classified {
    pub risk: Risk,
    /// 危险标记（写类且无有效 WHERE / 多表 / DDL）。
    pub dangerous: bool,
    /// 语句条数（按 `;` 切分）。
    pub statements: usize,
}

/// 授权白名单（四维**并集**：任一已声明维度命中即放行；未声明维度不参与限制）。
///
/// ⚠️ 声明多维**不会提高强度，只会降低**（任一命中即过）。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AllowList {
    pub modules: Option<Vec<String>>,
    pub roles: Option<Vec<String>>,
    pub tenants: Option<Vec<String>>,
    /// `http` / `task` / `anon`（沿用 `sql_trace` 的 source 词汇）。
    pub sources: Option<Vec<String>>,
}

impl AllowList {
    /// 是否「未声明任何维度」（= 不限制）。
    pub fn is_unrestricted(&self) -> bool {
        self.modules.is_none()
            && self.roles.is_none()
            && self.tenants.is_none()
            && self.sources.is_none()
    }
}

/// 单个库的访问策略。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DbAccess {
    pub enforce: Enforce,
    pub read_only: bool,
    pub allow_ddl: bool,
    pub allow_dangerous: bool,
    /// raw/stream 结果行数上限（缺省 = 不限）。
    pub max_rows: Option<u32>,
    pub allow: AllowList,
}

impl DbAccess {
    /// 装配期校验（fail-loud，点名违规键）。
    pub fn validate(&self, alias: &str) -> Result<(), String> {
        if let Some(n) = self.max_rows
            && (n == 0 || n > MAX_ROWS_CAP)
        {
            return Err(format!(
                "db_access.{alias}.max_rows must be 1..{MAX_ROWS_CAP} (got {n})"
            ));
        }
        if self.read_only && self.allow_ddl {
            return Err(format!(
                "db_access.{alias}: read_only=true and allow_ddl=true contradict each other"
            ));
        }
        Ok(())
    }
}

/// 逐库**覆盖**条目：未声明的键**逐键继承** `defaults`。
///
/// 为什么必须逐键继承（不能整条覆盖）：运维在 `defaults` 设 `enforce: deny` 后，
/// 若再为某库加一条 `read_only: true`，整条覆盖会让该库的 `enforce` 悄悄回落 off
/// ——**全局设防被一次局部微调静默解除**。逐键继承下这条只覆盖 read_only。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DbAccessOverride {
    pub enforce: Option<Enforce>,
    pub read_only: Option<bool>,
    pub allow_ddl: Option<bool>,
    pub allow_dangerous: Option<bool>,
    /// `None` = 继承 `defaults`。
    ///
    /// 已知限制：**无法表达「该库显式不限」**（`Option<u32>` 被「继承」占掉了）。
    /// 需要在 defaults 有上限而某库放开时，改为 defaults 不设、逐库声明——不造
    /// `0=不限` 之类的暗号（历史上 `0` 被读成「零行」的代价远大于收益）。
    pub max_rows: Option<u32>,
    pub allow: Option<AllowList>,
}

impl DbAccessOverride {
    /// 与 `defaults` 合并成一条生效策略。
    ///
    /// `allow` **逐维覆盖**（未声明的维继承），不是整条替换：否则 defaults 声明了
    /// `roles` 而某库只写 `modules` 时，会把 roles 限制悄悄抹掉。
    pub fn merge(&self, base: &DbAccess) -> DbAccess {
        let a = |o: Option<&AllowList>, f: fn(&AllowList) -> Option<&Vec<String>>| {
            o.and_then(f).cloned().or_else(|| f(&base.allow).cloned())
        };
        let ov = self.allow.as_ref();
        DbAccess {
            enforce: self.enforce.unwrap_or(base.enforce),
            read_only: self.read_only.unwrap_or(base.read_only),
            allow_ddl: self.allow_ddl.unwrap_or(base.allow_ddl),
            allow_dangerous: self.allow_dangerous.unwrap_or(base.allow_dangerous),
            max_rows: self.max_rows.or(base.max_rows),
            allow: AllowList {
                modules: a(ov, |x| x.modules.as_ref()),
                roles: a(ov, |x| x.roles.as_ref()),
                tenants: a(ov, |x| x.tenants.as_ref()),
                sources: a(ov, |x| x.sources.as_ref()),
            },
        }
    }
}

/// `db_access:` 段：`defaults`（未声明库使用的整条默认策略）+ 按别名的覆盖条目。
#[derive(Debug, Clone, Default)]
pub struct DbAccessSection {
    pub defaults: DbAccess,
    pub by_db: HashMap<String, DbAccessOverride>,
}

impl DbAccessSection {
    /// 别名策略查找：未声明 → `defaults`；声明 → 与 `defaults` **逐键合并**。
    pub fn policy_for(&self, alias: &str) -> DbAccess {
        match self.by_db.get(alias) {
            Some(ov) => ov.merge(&self.defaults),
            None => self.defaults.clone(),
        }
    }
}

impl<'de> Deserialize<'de> for DbAccessSection {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let v = serde_yaml::Value::deserialize(d)?;
        let Some(m) = v.as_mapping() else {
            return Err(Error::custom(
                "db_access: expected a mapping of db alias -> policy",
            ));
        };
        let mut out = DbAccessSection::default();
        for (k, val) in m {
            let key = k.as_str().unwrap_or_default().to_string();
            if key.is_empty() {
                return Err(Error::custom("db_access: empty alias key"));
            }
            // `defaults` 是**完整策略**（它自己没有可继承的上级），其余是覆盖条目。
            if key == "defaults" {
                out.defaults = serde_yaml::from_value(val.clone())
                    .map_err(|e| Error::custom(format!("db_access.defaults: {e}")))?;
            } else {
                let entry: DbAccessOverride = serde_yaml::from_value(val.clone())
                    .map_err(|e| Error::custom(format!("db_access.{key}: {e}")))?;
                out.by_db.insert(key, entry);
            }
        }
        Ok(out)
    }
}

impl<'de> Deserialize<'de> for Enforce {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        match serde_yaml::Value::deserialize(d)? {
            serde_yaml::Value::String(s) => match s.as_str() {
                "off" => Ok(Enforce::Off),
                "warn" => Ok(Enforce::Warn),
                "deny" => Ok(Enforce::Deny),
                other => Err(Error::custom(format!(
                    "db_access enforce: illegal value {other:?} (off|warn|deny)"
                ))),
            },
            _ => Err(Error::custom(
                "db_access enforce: expected string off|warn|deny",
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// 词法与分类
// ---------------------------------------------------------------------------

/// 带副作用的 SELECT 内建函数（命中即归 Write）。
const SIDE_EFFECT_FNS: [&str; 6] = [
    "NEXTVAL",
    "SETVAL",
    "PG_TERMINATE_BACKEND",
    "PG_CANCEL_BACKEND",
    "LO_IMPORT",
    "LO_EXPORT",
];

fn severity(r: Risk) -> u8 {
    match r {
        Risk::ReadOnly => 0,
        Risk::Transaction => 1,
        Risk::Write => 2,
        Risk::Ddl => 3,
        Risk::Unknown => 4,
    }
}

/// 扫描为「语句 → 词序列」（词已大写化）。`None` = 词法无法定界（字符串未闭合）→ Unknown。
///
/// 与 `guard::tokens` 的差异（有意为之）：`/*! … */` 的**内容保留**（MySQL 可执行注释），
/// 并额外识别 MySQL 的 `#` 行注释。
fn statements(sql: &str) -> Option<Vec<Vec<String>>> {
    let b = sql.as_bytes();
    let mut out: Vec<Vec<String>> = Vec::new();
    let mut cur: Vec<String> = Vec::new();
    let mut start: Option<usize> = None;
    let mut i = 0usize;
    macro_rules! flush {
        () => {
            if let Some(s) = start.take() {
                cur.push(sql[s..i].to_ascii_uppercase());
            }
        };
    }
    while i < b.len() {
        let c = b[i];
        match c {
            b'\'' => {
                flush!();
                i += 1;
                let mut closed = false;
                while i < b.len() {
                    if b[i] == b'\\' {
                        i += 2;
                    } else if b[i] == b'\'' {
                        if i + 1 < b.len() && b[i + 1] == b'\'' {
                            i += 2;
                        } else {
                            i += 1;
                            closed = true;
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
                // 串未闭合：可能是「反斜杠不是转义字符」的方言（PG/SQLite 标准串）。
                // 不赌——交 Unknown，由调用方按最严档处置。
                if !closed {
                    return None;
                }
            }
            q @ (b'"' | b'`') => {
                flush!();
                i += 1;
                let s0 = i;
                let mut closed = false;
                while i < b.len() {
                    if b[i] == b'\\' {
                        i += 2;
                    } else if b[i] == q {
                        if i + 1 < b.len() && b[i + 1] == q {
                            i += 2;
                        } else {
                            i += 1;
                            closed = true;
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
                let end = if closed { i - 1 } else { i.min(b.len()) };
                cur.push(sql[s0..end].to_ascii_uppercase());
                if !closed {
                    return None;
                }
            }
            b'[' => {
                flush!();
                i += 1;
                let s0 = i;
                let mut closed = false;
                while i < b.len() {
                    if b[i] == b']' {
                        i += 1;
                        closed = true;
                        break;
                    }
                    i += 1;
                }
                let end = if closed { i - 1 } else { i };
                cur.push(sql[s0..end].to_ascii_uppercase());
                if !closed {
                    return None;
                }
            }
            b'-' if i + 1 < b.len() && b[i + 1] == b'-' => {
                flush!();
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'#' => {
                flush!();
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < b.len() && b[i + 1] == b'*' => {
                flush!();
                if i + 2 < b.len() && b[i + 2] == b'!' {
                    // MySQL 可执行注释 `/*!40001 DROP TABLE t */`：**内容照常分词**
                    // （只是跳过版本号）；若整段跳过，DDL 会被静默吃成空白 → 判只读。
                    i += 3;
                    while i < b.len() && b[i].is_ascii_digit() {
                        i += 1;
                    }
                    continue;
                }
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(b.len());
            }
            b';' => {
                flush!();
                out.push(std::mem::take(&mut cur));
                i += 1;
            }
            c if c.is_ascii_alphanumeric() || c == b'_' || c == b'.' || c == b',' || c == b'$' => {
                if start.is_none() {
                    start = Some(i);
                }
                i += 1;
            }
            _ => {
                flush!();
                i += 1;
            }
        }
    }
    flush!();
    out.push(cur);
    Some(out)
}

/// 谓词片段（WHERE 之后到子句边界；无 WHERE → None）。
fn predicate(w: &[String]) -> Option<&[String]> {
    let at = w.iter().position(|t| t == "WHERE")? + 1;
    let end = w[at..]
        .iter()
        .position(|t| {
            matches!(
                t.as_str(),
                "GROUP" | "ORDER" | "LIMIT" | "OFFSET" | "HAVING" | "UNION" | "RETURNING"
            )
        })
        .map(|p| at + p)
        .unwrap_or(w.len());
    Some(&w[at..end])
}

/// 恒真谓词（只用于**收紧**：已判危险的不放松）。best-effort，不覆盖全部等价变形。
fn tautology(w: &[String]) -> bool {
    let Some(p) = predicate(w) else {
        return false;
    };
    if p.windows(2).any(|x| x[0] == x[1]) {
        return true; // `1 = 1`、`a = a`、`t.id = t.id`
    }
    let has = |k: &str| p.iter().any(|t| t == k);
    has("IS") && has("NULL") && has("OR") && has("NOT")
}

/// 多表 UPDATE/DELETE（逗号连接 / JOIN / USING）。
fn multi_table(w: &[String]) -> bool {
    let mut i = 1;
    if w.get(i).is_some_and(|s| s == "FROM") {
        i += 1;
    }
    while i < w.len() {
        let t = w[i].as_str();
        if t == "USING" || t == "JOIN" || t.ends_with(',') {
            return true;
        }
        if matches!(
            t,
            "WHERE" | "SET" | "ORDER" | "LIMIT" | "RETURNING" | "ON" | "GROUP" | "HAVING"
        ) {
            break;
        }
        i += 1;
    }
    false
}

/// 单语句分类 → (风险, 危险)。
fn classify_stmt(w: &[String]) -> (Risk, bool) {
    if w.is_empty() {
        return (Risk::Unknown, false);
    }
    let has = |k: &str| w.iter().any(|t| t == k);
    let has_where = has("WHERE");
    match w[0].as_str() {
        // 数据修改型 CTE：`WITH d AS (DELETE FROM t RETURNING id) SELECT …`
        "WITH" => {
            let mut rank = 0u8;
            let mut risk = Risk::ReadOnly;
            for t in w {
                let r = match t.as_str() {
                    "DELETE" | "UPDATE" | "INSERT" | "REPLACE" | "MERGE" => Risk::Write,
                    "DROP" | "ALTER" | "TRUNCATE" | "CREATE" => Risk::Ddl,
                    "SELECT" => Risk::ReadOnly,
                    _ => continue,
                };
                if severity(r) > rank {
                    rank = severity(r);
                    risk = r;
                }
            }
            if rank == 0 {
                return (Risk::Unknown, false);
            }
            let dangerous = matches!(risk, Risk::Write | Risk::Ddl) && (!has_where || tautology(w));
            (risk, dangerous)
        }
        "SELECT" => {
            if has("OUTFILE") || has("DUMPFILE") {
                return (Risk::Write, false);
            }
            if has("FOR") && has("UPDATE") {
                return (Risk::Write, false);
            }
            if has("LOCK") && has("SHARE") {
                return (Risk::Write, false);
            }
            if SIDE_EFFECT_FNS.iter().any(|f| has(f)) {
                return (Risk::Write, false);
            }
            (Risk::ReadOnly, false)
        }
        "INSERT" | "REPLACE" | "MERGE" => (Risk::Write, false),
        "UPDATE" | "DELETE" => (Risk::Write, !has_where || multi_table(w) || tautology(w)),
        "TRUNCATE" | "DROP" | "ALTER" | "CREATE" | "RENAME" | "GRANT" | "REVOKE" => {
            (Risk::Ddl, true)
        }
        "BEGIN" | "COMMIT" | "ROLLBACK" | "SAVEPOINT" | "RELEASE" | "START" => {
            (Risk::Transaction, false)
        }
        // 会话/全局变量改写是写操作；裸 SET 不猜 → Unknown。
        "SET" => {
            if has("GLOBAL") || has("SESSION") {
                (Risk::Write, false)
            } else {
                (Risk::Unknown, false)
            }
        }
        "COPY" | "LOAD" | "CALL" | "EXECUTE" => (Risk::Write, false),
        "SHOW" | "EXPLAIN" | "DESCRIBE" | "DESC" | "PRAGMA" | "USE" | "ANALYZE" | "VALUES" => {
            (Risk::ReadOnly, false)
        }
        _ => (Risk::Unknown, false),
    }
}

/// 分类缓存（纯文本函数 → 进程级缓存安全；键只含 SQL 原文，与身份/库无关）。
fn memo() -> &'static Mutex<HashMap<String, Arc<Classified>>> {
    static M: OnceLock<Mutex<HashMap<String, Arc<Classified>>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 分类入口：多语句取最严；任一段无法定界 → Unknown。
pub fn classify(sql: &str) -> Arc<Classified> {
    {
        let m = memo().lock().unwrap();
        if let Some(c) = m.get(sql) {
            return c.clone();
        }
    }
    let c = Arc::new(match statements(sql) {
        None => Classified {
            risk: Risk::Unknown,
            dangerous: false,
            statements: 0,
        },
        Some(stmts) => {
            let mut worst = Risk::ReadOnly;
            let mut dangerous = false;
            for w in &stmts {
                let (r, d) = classify_stmt(w);
                if severity(r) > severity(worst) {
                    worst = r;
                }
                dangerous |= d;
            }
            Classified {
                risk: worst,
                dangerous,
                statements: stmts.len(),
            }
        }
    });
    let mut m = memo().lock().unwrap();
    if m.len() >= 4096 {
        m.clear();
    }
    m.insert(sql.to_string(), c.clone());
    c
}

// ---------------------------------------------------------------------------
// 授权判定
// ---------------------------------------------------------------------------

/// 调用主体快照（从 `OpState` 一次性提取）。
pub struct AccessCtx<'a> {
    pub module: Option<&'a str>,
    pub roles: Vec<String>,
    /// 只认 server 注入的租户（`db.asTenant()` 改写后的不算——它只能用于匿名请求）。
    pub tenant: Option<&'a str>,
    /// `http` / `task` / `anon`。
    pub source: &'a str,
}

/// 判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Violation(Reason),
}

/// 白名单命中（四维并集）。
pub fn granted(allow: &AllowList, ctx: &AccessCtx) -> bool {
    if allow.is_unrestricted() {
        return true;
    }
    // 四维**并集**：任一已声明维度命中即放行。
    let hit = |declared: Option<&Vec<String>>, got: Option<&str>| {
        declared.is_some_and(|d| got.is_some_and(|g| d.iter().any(|v| v == g)))
    };
    if hit(allow.modules.as_ref(), ctx.module) {
        return true;
    }
    if hit(allow.tenants.as_ref(), ctx.tenant) {
        return true;
    }
    if let Some(r) = &allow.roles
        && ctx.roles.iter().any(|x| r.iter().any(|v| v == x))
    {
        return true;
    }
    if let Some(s) = &allow.sources
        && s.iter().any(|v| v == ctx.source)
    {
        return true;
    }
    false
}

/// 策略 × 分类 → 判定。不读 `ReqState.system`（`db.asSystem` **不豁免**访问控制）。
pub fn evaluate(p: &DbAccess, ctx: &AccessCtx, c: &Classified) -> Verdict {
    if !granted(&p.allow, ctx) {
        return Verdict::Violation(Reason::NotGranted);
    }
    match c.risk {
        Risk::Unknown => Verdict::Violation(Reason::UnknownRisk),
        Risk::Ddl => {
            if p.read_only {
                Verdict::Violation(Reason::ReadOnly)
            } else if !p.allow_ddl {
                Verdict::Violation(Reason::DdlDenied)
            } else {
                Verdict::Allow
            }
        }
        Risk::Write => {
            if p.read_only {
                Verdict::Violation(Reason::ReadOnly)
            } else if c.dangerous && !p.allow_dangerous {
                Verdict::Violation(Reason::DangerousWrite)
            } else {
                Verdict::Allow
            }
        }
        Risk::ReadOnly | Risk::Transaction => Verdict::Allow,
    }
}

/// op 边界的访问控制入口：授权 → 分类 → 策略位 → 限额。
///
/// `enforce: off`（默认）零开销早退，与升级前行为完全一致。
/// 取某库的策略副本（装配期冻结，运行期只读）。
fn policy_for(state: &Rc<RefCell<OpState>>, alias: &str) -> DbAccess {
    let g = state.borrow();
    g.borrow::<Arc<StableState>>().db_access.policy_for(alias)
}

/// 调用主体快照（module / roles / tenant / source）。
fn identity(
    state: &Rc<RefCell<OpState>>,
) -> (Option<String>, Vec<String>, Option<String>, &'static str) {
    let g = state.borrow();
    let st = g.borrow::<Arc<StableState>>();
    let rs = g.borrow::<ReqState>();
    let roles = rs
        .req
        .user
        .as_ref()
        .and_then(|u| u.get("roles"))
        .and_then(|r| r.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    // 租户只认 server 注入（非匿名请求）；`db.asTenant` 仅对匿名请求可用，故自然被排除。
    let tenant = if rs.req.anonymous {
        None
    } else {
        rs.req.tenant_id.clone()
    };
    let source = if st.tasks_flag.is_some() {
        "task"
    } else {
        "http"
    };
    (rs.module.clone(), roles, tenant, source)
}

/// 决策日志（固定前缀 `DB_ACCESS_DENY`，按 `(alias, reason)` **聚合计数**）。
///
/// 为什么不逐条落：warn 档会把日志刷爆，且可被恶意请求当成刷盘放大器。
/// 首次必落（运维能看到新出现的违规形态），其后每 100 条落一次并带累计次数。
/// 声明：本通道**非审计、无防篡改**（见 db-guide「不承诺」表）。
fn deny_log(kind: &str, alias: &str, reason: Reason, msg: &str) {
    static M: OnceLock<Mutex<HashMap<(String, &'static str), u32>>> = OnceLock::new();
    let n = {
        let mut m = M.get_or_init(Default::default).lock().unwrap();
        let e = m.entry((alias.to_string(), reason.as_str())).or_insert(0);
        *e += 1;
        *e
    };
    if n == 1 || n % 100 == 0 {
        eprintln!(
            "DB_ACCESS_DENY {kind}: alias={alias} reason={} count={n} :: {}",
            reason.as_str(),
            msg.replace('\n', " | ")
        );
    }
}

/// 按已判定意图执行访问控制（构造器路径：verb 已知，不经词法）。
/// 返回值 = 本库生效的 raw/stream 行数上限（None = 不限）。
pub fn check_access_intent(
    state: &Rc<RefCell<OpState>>,
    alias: &str,
    c: &Classified,
) -> Result<Option<u32>, JsErrorBox> {
    let policy = policy_for(state, alias);
    if policy.enforce == Enforce::Off {
        return Ok(policy.max_rows); // 零开销：不分类、不取身份
    }
    let (module, roles, tenant, source) = identity(state);
    let ctx = AccessCtx {
        module: module.as_deref(),
        roles,
        tenant: tenant.as_deref(),
        source,
    };
    match evaluate(&policy, &ctx, c) {
        Verdict::Allow => Ok(policy.max_rows),
        Verdict::Violation(reason) => {
            let detail = match reason {
                Reason::NotGranted => format!(
                    "alias {alias:?} not granted to this caller (module={:?}, roles={:?}, tenant={:?}, source={source})",
                    ctx.module, ctx.roles, ctx.tenant
                ),
                Reason::ReadOnly => format!(
                    "alias {alias:?} is read_only; statement classified {}",
                    c.risk.as_str()
                ),
                Reason::DdlDenied => format!("alias {alias:?} does not allow DDL"),
                Reason::DangerousWrite => format!(
                    "alias {alias:?}: dangerous write without an effective WHERE (or multi-table/tautological predicate)"
                ),
                Reason::UnknownRisk => {
                    "statement classification failed; denied by fail-closed".to_string()
                }
            };
            let fix = match reason {
                Reason::NotGranted => "修复：由管理员在 db_access.<alias>.allow 为对应维度授权",
                Reason::ReadOnly => "修复：改用只读语句，或由管理员放开该库 read_only",
                Reason::DdlDenied => "修复：由管理员放开 allow_ddl，或改用迁移通道",
                Reason::DangerousWrite => "修复：补 WHERE 条件，或由管理员放开 allow_dangerous",
                Reason::UnknownRisk => "修复：改用构造器 db.table()（走精确意图判定，不依赖词法）",
            };
            let msg = format!(
                "db access: {detail}\n  {fix}\n  (reason: {})",
                reason.as_str()
            );
            if policy.enforce == Enforce::Warn {
                deny_log("warn", alias, reason, &msg);
                return Ok(policy.max_rows);
            }
            deny_log("deny", alias, reason, &msg);
            Err(JsErrorBox::generic(msg))
        }
    }
}

/// 裸 SQL 路径入口：分类后交给 [`check_access_intent`]。
pub fn check_access(
    state: &Rc<RefCell<OpState>>,
    alias: &str,
    sql: &str,
) -> Result<Option<u32>, JsErrorBox> {
    let policy = policy_for(state, alias);
    if policy.enforce == Enforce::Off {
        return Ok(policy.max_rows);
    }
    let c = classify(sql);
    check_access_intent(state, alias, &c)
}

/// 操作者（`user.id`，字符串或数字形态；无身份则 None）。
pub fn operator_of(user: Option<&serde_json::Value>) -> Option<String> {
    let id = user?.get("id")?;
    match id {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// 流提前终止的行数上限日志（固定前缀，便于运维聚合）。
pub fn log_row_limit(alias: &str, cap: u32) {
    eprintln!("DB_ACCESS_ROW_LIMIT: alias {alias:?} stream stopped at max_rows={cap}");
}

/// 决策日志用的 SQL 脱敏：剥字面量/注释、`password=`/`token=`/`secret=` 打 `***`、截断 512。
pub fn redact_sql(sql: &str) -> String {
    let b = sql.as_bytes();
    let mut out = String::with_capacity(sql.len().min(512));
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            b'\'' => {
                i += 1;
                while i < b.len() {
                    if b[i] == b'\\' {
                        i += 2;
                    } else if b[i] == b'\'' {
                        i += 1;
                        break;
                    } else {
                        i += 1;
                    }
                }
                out.push('?');
            }
            b'-' if i + 1 < b.len() && b[i + 1] == b'-' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                out.push(' ');
            }
            b'/' if i + 1 < b.len() && b[i + 1] == b'*' => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(b.len());
                out.push(' ');
            }
            _ => {
                let n = match b[i] {
                    c if c < 0x80 => 1,
                    c if c >= 0xF0 => 4,
                    c if c >= 0xE0 => 3,
                    _ => 2,
                };
                out.push_str(&sql[i..(i + n).min(sql.len())]);
                i += n;
            }
        }
    }
    let mut s = out;
    for key in ["password", "token", "secret"] {
        s = mask_assignments(&s, key);
    }
    if s.len() > 512 {
        s.truncate(512);
        s.push('…');
    }
    s
}

/// `key=value` 形态的赋值打 `***`（不区分大小写；值到空白/`;`/`,`/`)` 为止）。
fn mask_assignments(s: &str, key: &str) -> String {
    let lower = s.to_ascii_lowercase();
    let mut out = s.to_string();
    let mut from = 0usize;
    while let Some(rel) = lower[from..].find(key) {
        let at = from + rel;
        let after = at + key.len();
        // 只认 `key` 后紧跟可选空白 + `=` 的赋值形态（跳过 `password_hash` 之类的标识符）。
        let rest = out[after..].trim_start();
        if !rest.starts_with('=') {
            from = at + key.len();
            continue;
        }
        let eq = after + (out[after..].len() - rest.len());
        let val_start = eq + 1;
        let val_end = out[val_start..]
            .find(|c: char| c.is_whitespace() || matches!(c, ';' | ',' | ')' | '&'))
            .map(|p| val_start + p)
            .unwrap_or(out.len());
        if val_end > val_start {
            out.replace_range(val_start..val_end, "***");
        }
        from = val_end.min(out.len());
        let lower2 = out.to_ascii_lowercase();
        if lower2.len() != lower.len() {
            break; // 长度变了（保险），退出防死循环
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn risk_of(sql: &str) -> Risk {
        classify(sql).risk
    }

    fn dang(sql: &str) -> bool {
        classify(sql).dangerous
    }

    #[test]
    fn classification_matrix() {
        assert_eq!(risk_of("select * from t where id = ?"), Risk::ReadOnly);
        assert_eq!(risk_of("insert into t (id) values (1)"), Risk::Write);
        assert_eq!(risk_of("update t set a = 1 where id = 1"), Risk::Write);
        assert_eq!(risk_of("delete from t where id = 1"), Risk::Write);
        assert_eq!(risk_of("truncate table t"), Risk::Ddl);
        assert_eq!(risk_of("alter table t add column x integer"), Risk::Ddl);
        assert_eq!(risk_of("begin"), Risk::Transaction);
        assert_eq!(risk_of("commit"), Risk::Transaction);
        // 方言特例
        assert_eq!(risk_of("replace into t (id) values (1)"), Risk::Write);
        assert_eq!(
            risk_of("insert into t (id) values (1) on conflict (id) do update set a = 1"),
            Risk::Write
        );
        assert_eq!(risk_of("select * from t returning id"), Risk::ReadOnly);
        // 只读型诊断语句
        assert_eq!(risk_of("show tables"), Risk::ReadOnly);
        assert_eq!(risk_of("explain select 1"), Risk::ReadOnly);
        assert_eq!(risk_of("pragma table_info(t)"), Risk::ReadOnly);
    }

    /// 对抗性探测：每条都是「试图把写/DDL 伪装成只读」的击穿尝试。
    #[test]
    fn adversarial_samples_never_degrade_to_readonly() {
        // 1) 反斜杠吞句（PG/SQLite 标准串下是真两语句）→ 不判 ReadOnly，交 Unknown。
        assert_eq!(
            risk_of("SELECT 'a\\'; DROP TABLE users"),
            Risk::Unknown,
            "串未闭合必须 fail-closed，不得判只读"
        );
        // 2) 数据修改型 CTE
        assert_eq!(
            risk_of("WITH d AS (DELETE FROM orders RETURNING id) SELECT count(*) FROM d"),
            Risk::Write
        );
        // 3) MySQL 可执行注释（内容保留 → 认出 DROP）
        assert_eq!(risk_of("/*!40001 DROP TABLE t */"), Risk::Ddl);
        // 4) 注释断词（不得判只读）
        assert_eq!(risk_of("DR/**/OP TABLE t"), Risk::Unknown);
        assert_eq!(risk_of("DEL/**/ETE FROM t"), Risk::Unknown);
        // 5) MySQL `#` 行注释
        assert_eq!(risk_of("select 1 # drop table t\n"), Risk::ReadOnly);
        assert_eq!(risk_of("# drop table t\ndelete from t"), Risk::Write);
        // 6) 副作用 SELECT / 落文件 / 全局变量
        assert_eq!(risk_of("select 1 into outfile '/tmp/x'"), Risk::Write);
        assert_eq!(risk_of("select nextval('s')"), Risk::Write);
        assert_eq!(risk_of("select * from t for update"), Risk::Write);
        assert_eq!(risk_of("select * from t lock in share mode"), Risk::Write);
        assert_eq!(risk_of("copy t to '/tmp/x'"), Risk::Write);
        assert_eq!(risk_of("set global max_connections = 1"), Risk::Write);
        assert_eq!(risk_of("set names utf8"), Risk::Unknown, "裸 SET 不猜");
        // 7) 多语句取最严
        assert_eq!(risk_of("select 1; drop table t"), Risk::Ddl);
        // 8) 引用标识符里的表名不影响分类
        assert_eq!(risk_of("update `t` set a = 1 where id = 1"), Risk::Write);
        assert_eq!(risk_of(r#"update "t" set a = 1 where id = 1"#), Risk::Write);
        // 9) 可执行注释不影响租户守卫共享函数（回归锚：strip_literals 未被改动）
        assert_eq!(risk_of("select * from t"), Risk::ReadOnly);
    }

    #[test]
    fn dangerous_write_rules() {
        // 主判据 = 有无 WHERE。
        assert!(dang("delete from t"));
        assert!(dang("update t set a = 1"));
        assert!(!dang("delete from t where id = 1"));
        assert!(!dang("update t set a = 1 where id = 1"));
        // 恒真谓词（收紧）
        assert!(dang("delete from t where 1 = 1"));
        assert!(dang("update t set a = 1 where a = a"));
        assert!(dang("delete from t where id is null or id is not null"));
        // 多表
        assert!(dang("update a, b set a.x = 1 where a.id = b.id"));
        assert!(dang("delete a from a join b on a.id = b.id where a.id = 1"));
        // insert 不算危险写（无法毁数据）
        assert!(!dang("insert into t (id) values (1)"));
        // DDL 恒危险
        assert!(dang("truncate table t"));
        assert!(dang("create table t (id integer)"));
    }

    #[test]
    fn grant_is_union_of_declared_dimensions() {
        let p = |m: Option<&[&str]>, r: Option<&[&str]>| DbAccess {
            allow: AllowList {
                modules: m.map(|v| v.iter().map(|s| s.to_string()).collect()),
                roles: r.map(|v| v.iter().map(|s| s.to_string()).collect()),
                tenants: None,
                sources: None,
            },
            ..Default::default()
        };
        fn ctx<'a>(module: Option<&'a str>, roles: &[&str]) -> AccessCtx<'a> {
            AccessCtx {
                module,
                roles: roles.iter().map(|s| s.to_string()).collect(),
                tenant: None,
                source: "http",
            }
        }
        // 未声明任何维度 = 不限制
        assert!(granted(&p(None, None).allow, &ctx(None, &[])));
        // 声明 modules：命中才过
        let a = p(Some(&["sales"]), None);
        assert!(granted(&a.allow, &ctx(Some("sales"), &[])));
        assert!(!granted(&a.allow, &ctx(Some("ops"), &[])));
        assert!(!granted(&a.allow, &ctx(None, &[])), "无身份不得放行");
        // 并集：roles 命中即可（声明多维不提高强度）
        let b = p(Some(&["sales"]), Some(&["admin"]));
        assert!(granted(&b.allow, &ctx(Some("ops"), &["admin"])));
        assert!(!granted(&b.allow, &ctx(Some("ops"), &["user"])));
        // 声明空数组 = 无人可过（fail-closed，不是「不限制」）
        let c = p(Some(&[]), None);
        assert!(!granted(&c.allow, &ctx(Some("sales"), &[])));
    }

    #[test]
    fn evaluate_policy_bits() {
        let ctx = AccessCtx {
            module: None,
            roles: vec![],
            tenant: None,
            source: "http",
        };
        let base = DbAccess::default();
        assert_eq!(evaluate(&base, &ctx, &classify("select 1")), Verdict::Allow);
        // read_only 拦 Write 与 Ddl
        let ro = DbAccess {
            read_only: true,
            ..Default::default()
        };
        assert_eq!(
            evaluate(&ro, &ctx, &classify("update t set a=1 where id=1")),
            Verdict::Violation(Reason::ReadOnly)
        );
        assert_eq!(
            evaluate(&ro, &ctx, &classify("drop table t")),
            Verdict::Violation(Reason::ReadOnly)
        );
        // DDL 门
        let noddl = DbAccess::default();
        assert_eq!(
            evaluate(&noddl, &ctx, &classify("truncate table t")),
            Verdict::Violation(Reason::DdlDenied)
        );
        // 危险写门
        assert_eq!(
            evaluate(&noddl, &ctx, &classify("delete from t")),
            Verdict::Violation(Reason::DangerousWrite)
        );
        // Unknown → fail-closed
        assert_eq!(
            evaluate(&noddl, &ctx, &classify("select 'a\\'; drop table t")),
            Verdict::Violation(Reason::UnknownRisk)
        );
        // 授权优先于策略位
        let locked = DbAccess {
            allow: AllowList {
                modules: Some(vec!["sales".into()]),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            evaluate(&locked, &ctx, &classify("select 1")),
            Verdict::Violation(Reason::NotGranted)
        );
    }

    #[test]
    fn config_validation_is_loud() {
        assert!(
            DbAccess {
                max_rows: Some(0),
                ..Default::default()
            }
            .validate("x")
            .is_err()
        );
        assert!(
            DbAccess {
                max_rows: Some(MAX_ROWS_CAP + 1),
                ..Default::default()
            }
            .validate("x")
            .is_err()
        );
        assert!(
            DbAccess {
                read_only: true,
                allow_ddl: true,
                ..Default::default()
            }
            .validate("x")
            .is_err()
        );
        assert!(DbAccess::default().validate("x").is_ok());
    }

    #[test]
    fn section_parsing_and_lookup() {
        let s: DbAccessSection = serde_yaml::from_str(
            "defaults:\n  enforce: warn\nanalytics:\n  read_only: true\n  max_rows: 50\n",
        )
        .unwrap();
        assert_eq!(s.policy_for("whatever").enforce, Enforce::Warn);
        assert!(s.policy_for("analytics").read_only);
        assert_eq!(s.policy_for("analytics").max_rows, Some(50));
        // 逐键继承：只声明 read_only/max_rows，**不得**把 defaults 的 enforce 抹掉
        // （整条覆盖会让「全局设防 + 局部微调」静默退化成不设防）
        assert_eq!(s.policy_for("analytics").enforce, Enforce::Warn);
        // allow 逐维覆盖：defaults 的 roles 不能因某库只写 modules 而消失
        let s: DbAccessSection = serde_yaml::from_str(
            "defaults:\n  allow:\n    roles: [admin]\nanalytics:\n  allow:\n    modules: [sales]\n",
        )
        .unwrap();
        let a = s.policy_for("analytics").allow;
        assert_eq!(a.roles.as_deref(), Some(&["admin".to_string()][..]));
        assert_eq!(a.modules.as_deref(), Some(&["sales".to_string()][..]));
        // 未知键 fail-loud
        let bad = serde_yaml::from_str::<DbAccessSection>("a:\n  nope: 1\n");
        assert!(bad.is_err(), "{bad:?}");
    }

    #[test]
    fn redact_masks_literals_and_credentials() {
        assert_eq!(
            redact_sql("select * from t where a = 'secret'"),
            "select * from t where a = ?"
        );
        assert!(!redact_sql("select * from t -- drop\n").contains("drop"));
        let s = redact_sql("mysql://u:pw@host/db?password=hunter2&x=1");
        assert!(s.contains("password=***"), "{s}");
        let s = redact_sql("grant all on *.* to u identified by 'x'; token=abc");
        assert!(s.contains("token=***"), "{s}");
        assert!(!redact_sql("select password_hash from t").contains("***"));
        let long = "a".repeat(1000);
        // 截断口径是 512 字符 + 1 个省略号（按字符计，非字节）
        assert_eq!(redact_sql(&long).chars().count(), 513);
    }
}

// ---------------------------------------------------------------------------
// 行为面：经 `Bridge` 跑 handler（不是只测纯函数）
// ---------------------------------------------------------------------------

/// 行为测试：策略挂在真实 op 链上（query/exec/stream/构造器），而非只测纯函数。
///
/// `modules` 维不在本套件内：`ReqState.module` 只有 `run_module`（文件系统路径）会写，
/// `run_with` 恒 None —— 该维的语义由上面 `grant_is_union_of_declared_dimensions`
/// 钉死，行为面只覆盖 roles / tenants / sources 三维 + 策略位 + 限额 + 自省。
#[cfg(test)]
mod behavior {
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use super::*;
    use crate::bridge::db::DataAccessor;
    use crate::bridge::{Bridge, Extras, InMemoryKV, RequestInfo, SchemaRegistry};

    /// 文件型 sqlite：内存库每连接独立，`db.exec` 建表后 `db.query` 可能换连接看不到。
    /// 限额类用例必须落盘（tempfile 随目录析构清理）。
    async fn file_sqlite() -> (tempfile::TempDir, Arc<dyn DataAccessor>) {
        let dir = tempfile::tempdir().unwrap();
        let dsn = format!("sqlite://{}", dir.path().join("t.db").display());
        // 走全仓唯一的 DSN 归一入口（它会建文件 + 处理 Windows 盘符坑），
        // 不要手拼 `sqlite://` 直接喂 sqlx（Url 解析会吞绝对路径 → SQLITE_CANTOPEN）。
        let db = crate::bridge::db_backend::DbBackendRegistry::builtin()
            .connect(&dsn, dir.path())
            .await
            .unwrap();
        (dir, db)
    }

    fn bridge_with(
        db: Arc<dyn DataAccessor>,
        registry: SchemaRegistry,
        policy: DbAccessSection,
        task: bool,
        // `tenant.allow_as_tenant`：`db.asTenant()` 的唯一开关（默认关）。
        as_tenant: bool,
    ) -> Bridge {
        Bridge::with_dbs_and_loader(
            std::collections::HashMap::from([("default".to_string(), db)]),
            Arc::new(InMemoryKV::new()),
            registry,
            false,
            None,
            Extras {
                db_access: Some(Arc::new(policy)),
                allow_as_tenant: as_tenant,
                tasks_flag: if task {
                    Some(Arc::new(AtomicBool::new(false)))
                } else {
                    None
                },
                ..Default::default()
            },
        )
    }

    /// 跑一段 JS，返回信封；失败（code != 0）返回 msg 便于断言原因串。
    async fn run(b: &Bridge, req: RequestInfo, js: &str) -> (i64, String, serde_json::Value) {
        let cap = b.run_with(js, req).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&cap.body).unwrap();
        let code = v["code"].as_i64().unwrap_or(-1);
        let msg = v["msg"].as_str().unwrap_or_default().to_string();
        (code, msg, v["data"].clone())
    }

    const JS_WRITE: &str = r#"(async () => {
        try { await db.exec("delete from t where id = 1"); json.ok({ok: true}); }
        catch (e) { json.ok({err: String(e)}); }
      })().catch(e => json.fail(500, String(e)));"#;

    const JS_READ: &str = r#"(async () => {
        try { const r = await db.query("select id from t"); json.ok({n: r.length}); }
        catch (e) { json.ok({err: String(e)}); }
      })().catch(e => json.fail(500, String(e)));"#;

    fn req_user(roles: &[&str]) -> RequestInfo {
        RequestInfo {
            user: Some(json!({"id": 7, "roles": roles})),
            ..Default::default()
        }
    }

    /// `enforce: off`（默认档）必须完全透明：`read_only` 也写在配置里也不设防。
    #[tokio::test(flavor = "current_thread")]
    async fn off_tier_is_fully_transparent() {
        let (_d, db) = file_sqlite().await;
        db.exec_with_params("create table t (id integer)", &[])
            .await
            .unwrap();
        let pol = DbAccessSection {
            defaults: DbAccess {
                read_only: true,     // off 档下不生效
                max_rows: Some(999), // 限额照旧生效（限额不是闸）
                ..Default::default()
            },
            ..Default::default()
        };
        let b = bridge_with(db, SchemaRegistry::new(), pol, false, false);
        let (code, _m, data) = run(&b, RequestInfo::default(), JS_WRITE).await;
        assert_eq!(code, 0, "{data}");
        assert_eq!(data["ok"], json!(true), "off 档不得拦写：{data}");
    }

    /// deny 档 + read_only：写类语句被拦，原因串可 grep。
    #[tokio::test(flavor = "current_thread")]
    async fn deny_read_only_blocks_write() {
        let (_d, db) = file_sqlite().await;
        db.exec_with_params("create table t (id integer)", &[])
            .await
            .unwrap();
        let pol = DbAccessSection {
            defaults: DbAccess {
                enforce: Enforce::Deny,
                read_only: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let b = bridge_with(db, SchemaRegistry::new(), pol, false, false);
        let (code, _m, data) = run(&b, RequestInfo::default(), JS_WRITE).await;
        assert_eq!(code, 0, "{data}"); // handler 自己兜住了
        let err = data["err"].as_str().unwrap_or_default();
        assert!(err.contains("db_read_only"), "{err}");
        // 读不受影响
        let (_, _, data) = run(&b, RequestInfo::default(), JS_READ).await;
        assert_eq!(data["n"], json!(0), "{data}");
    }

    /// 授权未命中 → `db_not_granted`；命中 roles → 放行（并集语义的行为面版本）。
    #[tokio::test(flavor = "current_thread")]
    async fn role_grant_hit_and_miss() {
        let (_d, db) = file_sqlite().await;
        db.exec_with_params("create table t (id integer)", &[])
            .await
            .unwrap();
        let policy = || DbAccessSection {
            defaults: DbAccess {
                enforce: Enforce::Deny,
                allow: AllowList {
                    roles: Some(vec!["admin".into()]),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        // 无身份（user=null）→ fail-closed
        let b = bridge_with(db.clone(), SchemaRegistry::new(), policy(), false, false);
        let (_, _, data) = run(&b, RequestInfo::default(), JS_READ).await;
        let err = data["err"].as_str().unwrap_or_default();
        assert!(err.contains("db_not_granted"), "无身份不得放行：{err}");
        // 有身份但角色不命中
        let (_, _, data) = run(&b, req_user(&["guest"]), JS_READ).await;
        assert!(
            data["err"]
                .as_str()
                .unwrap_or_default()
                .contains("db_not_granted"),
            "{data}"
        );
        // 角色命中
        let (_, _, data) = run(&b, req_user(&["admin"]), JS_READ).await;
        assert_eq!(data["n"], json!(0), "命中即放行：{data}");
    }

    /// `asSystem()` **不豁免** db 访问控制（它只绕过租户注入）。
    #[tokio::test(flavor = "current_thread")]
    async fn as_system_does_not_exempt_access_control() {
        let (_d, db) = file_sqlite().await;
        db.exec_with_params("create table t (id integer)", &[])
            .await
            .unwrap();
        let pol = DbAccessSection {
            defaults: DbAccess {
                enforce: Enforce::Deny,
                read_only: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let b = bridge_with(db, SchemaRegistry::new(), pol, false, false);
        let js = r#"(async () => {
            try { await db.asSystem().query("delete from t"); json.ok({ok: true}); }
            catch (e) { json.ok({err: String(e)}); }
          })().catch(e => json.fail(500, String(e)));"#;
        let (_, _, data) = run(&b, RequestInfo::default(), js).await;
        assert!(
            data["err"]
                .as_str()
                .unwrap_or_default()
                .contains("db_read_only"),
            "asSystem 不得豁免访问控制：{data}"
        );
    }

    /// `asTenant()` 声明的租户**不作数**（tenants 维只认 server 注入），故匿名请求
    /// 走该维一律 fail-closed —— 否则等于把授权交给请求方自己填。
    #[tokio::test(flavor = "current_thread")]
    async fn as_tenant_voids_the_tenant_dimension() {
        let (_d, db) = file_sqlite().await;
        db.exec_with_params("create table t (id integer)", &[])
            .await
            .unwrap();
        let pol = DbAccessSection {
            defaults: DbAccess {
                enforce: Enforce::Deny,
                allow: AllowList {
                    tenants: Some(vec!["t1".into()]),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        let b = bridge_with(db, SchemaRegistry::new(), pol, false, true);
        let js = r#"(async () => {
            try { await db.asTenant("t1").query("select id from t"); json.ok({ok: true}); }
            catch (e) { json.ok({err: String(e)}); }
          })().catch(e => json.fail(500, String(e)));"#;
        // server 注入的 tenant 命中 → 放行
        let hit = RequestInfo {
            tenant_id: Some("t1".into()),
            ..Default::default()
        };
        let (_, _, data) = run(&b, hit, JS_READ).await;
        assert_eq!(data["n"], json!(0), "server 注入的租户应命中：{data}");
        // 未命中
        let miss = RequestInfo {
            tenant_id: Some("t2".into()),
            ..Default::default()
        };
        let (_, _, data) = run(&b, miss, JS_READ).await;
        assert!(
            data["err"]
                .as_str()
                .unwrap_or_default()
                .contains("db_not_granted"),
            "{data}"
        );
        // 匿名 + asTenant("t1")：租户由请求方自填 → 该维作废 → fail-closed
        let anon = RequestInfo {
            anonymous: true,
            ..Default::default()
        };
        let (_, _, data) = run(&b, anon, js).await;
        assert!(
            data["err"]
                .as_str()
                .unwrap_or_default()
                .contains("db_not_granted"),
            "asTenant 不得自授租户权：{data}"
        );
    }

    /// 任务桥走 `sources: ["task"]`，HTTP 桥走 `http`（同一策略、身份相同，只换 source）。
    #[tokio::test(flavor = "current_thread")]
    async fn task_bridge_reports_source_task() {
        let (_d, db) = file_sqlite().await;
        db.exec_with_params("create table t (id integer)", &[])
            .await
            .unwrap();
        let policy = |want: &str| DbAccessSection {
            defaults: DbAccess {
                enforce: Enforce::Deny,
                allow: AllowList {
                    sources: Some(vec![want.into()]),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        // HTTP 桥 + 只允许 task → 拒
        let b = bridge_with(
            db.clone(),
            SchemaRegistry::new(),
            policy("task"),
            false,
            false,
        );
        let (_, _, data) = run(&b, RequestInfo::default(), JS_READ).await;
        assert!(
            data["err"]
                .as_str()
                .unwrap_or_default()
                .contains("db_not_granted"),
            "HTTP 桥不得自称 task：{data}"
        );
        // 任务桥 + 只允许 task → 放行
        let b = bridge_with(
            db.clone(),
            SchemaRegistry::new(),
            policy("task"),
            true,
            false,
        );
        let (_, _, data) = run(&b, RequestInfo::default(), JS_READ).await;
        assert_eq!(data["n"], json!(0), "任务桥应走 source=task：{data}");
        // 任务桥 + 只允许 http → 拒
        let b = bridge_with(db, SchemaRegistry::new(), policy("http"), true, false);
        let (_, _, data) = run(&b, RequestInfo::default(), JS_READ).await;
        assert!(
            data["err"]
                .as_str()
                .unwrap_or_default()
                .contains("db_not_granted"),
            "{data}"
        );
    }

    /// raw `query` 超 `max_rows` → 截断（并打 `X-OJ-Row-Limit`）。
    #[tokio::test(flavor = "current_thread")]
    async fn raw_query_truncates_at_max_rows() {
        let (_d, db) = file_sqlite().await;
        db.exec_with_params("create table t (id integer)", &[])
            .await
            .unwrap();
        db.exec_with_params(
            "insert into t select 1 union all select 2 union all select 3 union all select 4 union all select 5",
            &[],
        )
        .await
        .unwrap();
        let pol = DbAccessSection {
            defaults: DbAccess {
                max_rows: Some(3),
                ..Default::default()
            },
            ..Default::default()
        };
        let b = bridge_with(db, SchemaRegistry::new(), pol, false, false);
        let (_, _, data) = run(&b, RequestInfo::default(), JS_READ).await;
        assert_eq!(data["n"], json!(3), "raw 路径须截断到 max_rows：{data}");
    }

    /// `stream` 达 `max_rows` → 提前 done（不是截断：真省后端拉取）。
    #[tokio::test(flavor = "current_thread")]
    async fn stream_stops_early_at_max_rows() {
        let (_d, db) = file_sqlite().await;
        db.exec_with_params("create table t (id integer)", &[])
            .await
            .unwrap();
        db.exec_with_params(
            "insert into t select 1 union all select 2 union all select 3 union all select 4 union all select 5",
            &[],
        )
        .await
        .unwrap();
        let pol = DbAccessSection {
            defaults: DbAccess {
                max_rows: Some(2),
                ..Default::default()
            },
            ..Default::default()
        };
        let b = bridge_with(db, SchemaRegistry::new(), pol, false, false);
        let js = r#"(async () => {
            try {
              const rows = [];
              for await (const r of db.stream("select id from t")) rows.push(r);
              json.ok({n: rows.length});
            } catch (e) { json.ok({err: String(e)}); }
          })().catch(e => json.fail(500, String(e)));"#;
        let (_, _, data) = run(&b, RequestInfo::default(), js).await;
        assert_eq!(data["n"], json!(2), "stream 须在 max_rows 提前终止：{data}");
    }

    /// 构造器路径（不走词法）也吃 `max_rows`：生效值 = min(声明 limit, max_rows)。
    #[tokio::test(flavor = "current_thread")]
    async fn builder_path_applies_max_rows() {
        let (_d, db) = file_sqlite().await;
        db.exec_with_params("create table t (id integer, name text)", &[])
            .await
            .unwrap();
        db.exec_with_params(
            "insert into t select 1,'a' union all select 2,'b' union all select 3,'c' union all select 4,'d'",
            &[],
        )
        .await
        .unwrap();
        let pol = DbAccessSection {
            defaults: DbAccess {
                max_rows: Some(2),
                ..Default::default()
            },
            ..Default::default()
        };
        let reg = SchemaRegistry::new().table("t", &["id"], &["id", "name"]);
        let b = bridge_with(db, reg, pol, false, false);
        let js = r#"(async () => {
            try {
              const r = await db.table("t").select(["id"]).orderBy([{field:"id",dir:"asc"}]).limit(10).all();
              json.ok({n: r.length});
            } catch (e) { json.ok({err: String(e)}); }
          })().catch(e => json.fail(500, String(e)));"#;
        let (_, _, data) = run(&b, RequestInfo::default(), js).await;
        assert_eq!(
            data["n"],
            json!(2),
            "构造器 limit 与 max_rows 取小（此处 max_rows 更小）：{data}"
        );
    }

    /// 自省出口只列**已授权**的库：前端不该列出点了才报错的连接（假入口）。
    #[tokio::test(flavor = "current_thread")]
    async fn allowed_lists_only_granted_aliases() {
        let (_d, db) = file_sqlite().await;
        let pol = DbAccessSection {
            defaults: DbAccess {
                enforce: Enforce::Deny,
                read_only: true,
                max_rows: Some(5),
                allow: AllowList {
                    roles: Some(vec!["admin".into()]),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        let b = bridge_with(db, SchemaRegistry::new(), pol, false, false);
        let js = r#"json.ok({list: db.allowed()});"#;
        // 未授权 → 空表
        let (_, _, data) = run(&b, RequestInfo::default(), js).await;
        assert_eq!(data["list"], json!([]), "未授权不得出现在列表里：{data}");
        // 授权 → 单条，能力位如实回传
        let (_, _, data) = run(&b, req_user(&["admin"]), js).await;
        let list = data["list"].as_array().unwrap();
        assert_eq!(list.len(), 1, "{data}");
        assert_eq!(list[0]["alias"], json!("default"));
        assert_eq!(list[0]["read_only"], json!(true));
        assert_eq!(list[0]["max_rows"], json!(5));
        assert_eq!(list[0]["enforce"], json!("deny"));
    }

    /// warn 档：只告警放行（灰度用），语句照常执行。
    #[tokio::test(flavor = "current_thread")]
    async fn warn_tier_passes_through() {
        let (_d, db) = file_sqlite().await;
        db.exec_with_params("create table t (id integer)", &[])
            .await
            .unwrap();
        let pol = DbAccessSection {
            defaults: DbAccess {
                enforce: Enforce::Warn,
                read_only: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let b = bridge_with(db, SchemaRegistry::new(), pol, false, false);
        let (_, _, data) = run(&b, RequestInfo::default(), JS_WRITE).await;
        assert_eq!(data["ok"], json!(true), "warn 档不得拦：{data}");
    }
}
