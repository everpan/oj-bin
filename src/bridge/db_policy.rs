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

/// `statement_timeout_ms` 的绝对上界（1h：防手滑把毫秒写成秒/分，配置可上调但要有顶）。
pub const MAX_STATEMENT_TIMEOUT_MS: u64 = 3_600_000;

/// 结果集字节触顶策略（REQ-05：fail=结构化错误（缺省语义）；truncate=截断 + 显式信令）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnExceed {
    Fail,
    Truncate,
}

impl Default for OnExceed {
    /// REQ §4.2-2：默认 fail-fast，不静默截断。
    fn default() -> Self {
        OnExceed::Fail
    }
}

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

    /// 逐维并集（U-1 运行期 grant 叠加到 config 白名单用）。
    ///
    /// 语义仍是「任一已声明维度命中即放行」，所以并集**只会放宽、不会收紧**：
    /// 某维在任一侧未声明（= 不限制）时保持未声明。这正是「grant 只叠加、不删除」的含义——
    /// 不会把 config 已授权的角色挤掉。
    pub fn union_with(&mut self, other: &AllowList) {
        fn merge(a: &mut Option<Vec<String>>, b: &Option<Vec<String>>) {
            match (&mut *a, b) {
                (None, _) | (_, None) => {} // 任一侧未声明 = 该维不限制
                (Some(dst), Some(src)) => {
                    for v in src {
                        if !dst.contains(v) {
                            dst.push(v.clone());
                        }
                    }
                }
            }
        }
        merge(&mut self.modules, &other.modules);
        merge(&mut self.roles, &other.roles);
        merge(&mut self.tenants, &other.tenants);
        merge(&mut self.sources, &other.sources);
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
    /// 语句级超时毫秒（REQ-05，GA 门禁）：调用侧 `timeoutMs` 只能收紧，传更大值拒。
    /// 覆盖 db.query/db.exec/db.stream/构造器四条 pool 路径（tx/attach 维持寿命封顶口径）。
    pub statement_timeout_ms: Option<u64>,
    /// 结果集字节上限（REQ-05）：行级边拉边计（行 JSON 编码字节），与 max_rows 并存、
    /// 先到先生效。缺省 = 不限。
    pub max_result_bytes: Option<u64>,
    /// 字节触顶策略（仅随 max_result_bytes 有意义；缺省 fail）。truncate 须配本键才启用。
    pub on_exceed: Option<OnExceed>,
    /// G9：危险语句确认 token（缺省 None = 不设确认门）。配置后，该别名的危险语句
    /// （无 WHERE / 恒真的写）须请求头 `x-oj-confirm` 精确匹配才放行（放行即审计
    /// `DB_DANGEROUS_CONFIRM`）。token 只从 config 来——运行期 grant 不覆盖安全位。
    pub dangerous_confirm_token: Option<String>,
    pub allow: AllowList,
}

impl DbAccess {
    /// 装配期校验（fail-loud，点名违规键）。
    pub fn validate(&self, alias: &str) -> Result<(), String> {
        if self
            .dangerous_confirm_token
            .as_deref()
            .is_some_and(str::is_empty)
        {
            return Err(format!(
                "db_access.{alias}.dangerous_confirm_token must be a non-empty token \
                 (remove the key to disable the confirm gate)"
            ));
        }
        if let Some(n) = self.max_rows
            && (n == 0 || n > MAX_ROWS_CAP)
        {
            return Err(format!(
                "db_access.{alias}.max_rows must be 1..{MAX_ROWS_CAP} (got {n})"
            ));
        }
        if let Some(ms) = self.statement_timeout_ms
            && (ms == 0 || ms > MAX_STATEMENT_TIMEOUT_MS)
        {
            return Err(format!(
                "db_access.{alias}.statement_timeout_ms must be 1..{MAX_STATEMENT_TIMEOUT_MS} (got {ms})"
            ));
        }
        if let Some(n) = self.max_result_bytes
            && n == 0
        {
            return Err(format!(
                "db_access.{alias}.max_result_bytes must be > 0 (got {n})"
            ));
        }
        if self.on_exceed.is_some() && self.max_result_bytes.is_none() {
            return Err(format!(
                "db_access.{alias}.on_exceed requires max_result_bytes (nothing to act on)"
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
    /// `None` = 继承 `defaults`（口径同 max_rows）。
    pub statement_timeout_ms: Option<u64>,
    /// `None` = 继承 `defaults`（口径同 max_rows）。
    pub max_result_bytes: Option<u64>,
    /// `None` = 继承 `defaults`（口径同 max_rows）。
    pub on_exceed: Option<OnExceed>,
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
            statement_timeout_ms: self.statement_timeout_ms.or(base.statement_timeout_ms),
            max_result_bytes: self.max_result_bytes.or(base.max_result_bytes),
            on_exceed: self.on_exceed.or(base.on_exceed),
            // G9：token 是安全位，只从 config 来（运行期 grant/module override 不覆盖）。
            dangerous_confirm_token: base.dangerous_confirm_token.clone(),
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

/// 动态数据源登记配置段（`db_client:`，L3 v0.1.91）。
///
/// `enabled` 默认 **false**（fail-closed）：动态接库是特权能力，默认不开。
/// `allow` 复用 [`AllowList`] 四维并集——登记操作的授权门（谁能调 register/test/
/// delete/reRegister/list）。`max_entries` 是 per-process 硬上限（连接池数封顶）。
///
/// 装配期 fail-loud 约束（见 `oj/src/app.rs`）：`enabled=true` 且 `allow` 四维全缺
/// （`is_unrestricted()`）→ 拒启动，防止「开了忘配」就全开（SSRF / 内网横向移动面）；
/// `max_per_owner` / `max_streams` 为 0 同样拒启（U-7 v0.1.97）。
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DbClientSection {
    pub enabled: bool,
    pub max_entries: usize,
    /// per-owner（registered_by）登记上限（U-7）：一个主体最多登记多少个动态库。
    /// 缺省 32（v0.1.101：8 → 32——按 registered_by 计数时单一管理员常占满全部登记，
    /// 8 条即被卡死）；0 非法（装配期拒启）。
    #[serde(default = "default_max_per_owner")]
    pub max_per_owner: usize,
    /// 动态库**全局**并发流预算（U-7）：所有动态别名上的 `db.stream` 游标总数上限。
    /// 缺省 32；0 非法（装配期拒启）。
    #[serde(default = "default_max_streams")]
    pub max_streams: usize,
    pub allow: AllowList,
}

fn default_max_per_owner() -> usize {
    32
}

fn default_max_streams() -> usize {
    32
}

// 手写 Default（保留 `max_entries=0` 的旧语义——enabled=true 时装配期仍要求显式 >=1；
// 新键给非零缺省，旧配置不写它们也照常启动）。
impl Default for DbClientSection {
    fn default() -> Self {
        Self {
            enabled: false,
            max_entries: 0,
            max_per_owner: default_max_per_owner(),
            max_streams: default_max_streams(),
            allow: AllowList::default(),
        }
    }
}

/// 跨请求事务配置段（`db_session:`，U-4 v0.1.95）。
///
/// `enabled` 默认 **false**（fail-closed，零行为变化）：跨请求事务会**长期占住一条连接**，
/// 且期间持有行锁——比单次查询更强的特权面，必须显式开启。
///
/// 三道门（缺一不可）：
/// 1. `enabled` + `allow`（本段）：谁能开/续用会话（AllowList 四维并集，与 `db_client` 同形）；
/// 2. `db_access`（每语句）：会话里每条语句仍过授权 + 风险分类，`read_only` 库写不进去；
/// 3. `require_identity`：无 `user.id` 且无 tenant 时拒建（匿名部署滥用面）。
///
/// 装配期 fail-loud 约束（`oj/src/app.rs`）：`enabled=true` 且 `allow` 四维全缺 → 拒启动；
/// `max_sessions` / `max_per_owner` 为 0 或 `max_per_owner > max_sessions` → 拒启动；
/// `idle_timeout_secs == 0` 或 `idle > max_lifetime` → 拒启动。
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DbSessionSection {
    pub enabled: bool,
    /// per-process 会话硬上限（每会话占一条连接）。
    pub max_sessions: usize,
    /// 单主体（tenant+user）上限：防单主体占满全局额度饿死他人。
    pub max_per_owner: usize,
    /// 滑动空闲超时（每次成功 op / `touch` 续期）。
    pub idle_timeout_secs: u64,
    /// 绝对寿命（从 begin 起算，不可续）。到期强制回收。
    pub max_lifetime_secs: u64,
    /// 无 `user.id` 且无 tenant 时拒建（本地开发/`oj test` 显式置 false）。
    pub require_identity: bool,
    /// 会话操作授权门。
    pub allow: AllowList,
}

impl Default for DbSessionSection {
    fn default() -> Self {
        Self {
            enabled: false,
            max_sessions: 32,
            max_per_owner: 8,
            idle_timeout_secs: 300,
            max_lifetime_secs: 1800,
            require_identity: true,
            allow: AllowList::default(),
        }
    }
}

impl DbSessionSection {
    pub fn idle_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.idle_timeout_secs.max(1))
    }
    pub fn max_lifetime(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.max_lifetime_secs.max(1))
    }
}

/// 管理面 connector 配置段（`mgmt:` 段，U-15）。`enabled` 缺省 **false**（fail-closed：
/// 管理操作是特权面）。`axes` 是装配期收编名单——这些泛型轴**不进 `axis()` 公开面**
/// （特权旁路封堵，评审阻断①）。`allow` = 写档；`allow_read` 缺省复用 `allow`
/// （产品路：通常同主体）。enabled=true 时 `allow` 必须显式声明（装配期拒 unrestricted）。
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MgmtSection {
    pub enabled: bool,
    /// 装配期收编为管理面的泛型轴名。
    pub axes: Vec<String>,
    /// 写档门（register/delete/非只读 call）。
    pub allow: AllowList,
    /// 读档门（list/has/只读 call）；缺省复用 `allow`。
    pub allow_read: Option<AllowList>,
    pub max_entries: usize,
    pub max_per_owner: usize,
}

impl Default for MgmtSection {
    fn default() -> Self {
        Self {
            enabled: false,
            axes: Vec::new(),
            allow: AllowList::default(),
            allow_read: None,
            max_entries: 8,
            max_per_owner: 4,
        }
    }
}

/// 任务式导出配置段（`db_export:`，U-18 v0.1.104）。配额 + 回落硬顶 + 产物 TTL。
/// 全部 0/超界 → 装配期拒启并点名键（fail-loud 同 audit 段口径）。
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DbExportSection {
    /// 进程级并发上限（缺省 4；>64 拒——回归测试/异常配置防线）。
    pub global_concurrency: usize,
    /// 每主体并发上限（缺省 2；>16 拒）。
    pub per_subject_concurrency: usize,
    /// 单任务产物字节上限（缺省 512 MiB；超限 failed，不静默截断）。
    pub max_bytes: u64,
    /// put_stream 回落路径（第三方插件缺流式时整段缓冲）硬顶（缺省 128 MiB）。
    /// 必须 < max_bytes：回落整段缓冲的内存代价高于流式路径，不设更紧的顶无意义。
    pub fallback_buffer_cap: u64,
    /// 产物 TTL 秒（缺省 24h；时钟自 put_stream_open 起算，崩溃半成品同样可扫）。
    pub ttl_secs: u64,
    /// 回收器清扫间隔秒（缺省 1h；`oj test` 注入短周期断言删除）。
    pub sweep_interval_secs: u64,
}

impl Default for DbExportSection {
    fn default() -> Self {
        Self {
            global_concurrency: 4,
            per_subject_concurrency: 2,
            max_bytes: 512 * 1024 * 1024,
            fallback_buffer_cap: 128 * 1024 * 1024,
            ttl_secs: 24 * 3600,
            sweep_interval_secs: 3600,
        }
    }
}

impl DbExportSection {
    /// 装配期 fail-loud 校验：Err 文案点名键（同 DbAccess::validate 口径）。
    pub fn validate(&self) -> Result<(), String> {
        let check = |ok: bool, key: &str, msg: &str| {
            if ok {
                Ok(())
            } else {
                Err(format!("db_export.{key}: {msg}"))
            }
        };
        check(
            self.global_concurrency >= 1 && self.global_concurrency <= 64,
            "global_concurrency",
            "must be 1..=64",
        )?;
        check(
            self.per_subject_concurrency >= 1 && self.per_subject_concurrency <= 16,
            "per_subject_concurrency",
            "must be 1..=16",
        )?;
        check(self.max_bytes >= 1, "max_bytes", "must be >= 1")?;
        check(
            self.fallback_buffer_cap >= 1 && self.fallback_buffer_cap < self.max_bytes,
            "fallback_buffer_cap",
            "must be >= 1 and < max_bytes",
        )?;
        check(self.ttl_secs >= 1, "ttl_secs", "must be >= 1")?;
        check(
            self.sweep_interval_secs >= 1,
            "sweep_interval_secs",
            "must be >= 1",
        )
    }
}

/// 任务式批量导入配置段（`db_import:`，REQ-12 v0.1.115）。并发配额 + 行/字节上限
/// + 默认批大小 + 任务 TTL。全部 0/超界 → 装配期拒启并点名键（fail-loud 同 export 段口径）。
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DbImportSection {
    /// 进程级并发上限（缺省 4；>64 拒——回归测试/异常配置防线）。
    pub global_concurrency: usize,
    /// 每主体并发上限（缺省 2；>16 拒）。
    pub per_subject_concurrency: usize,
    /// 单任务行数上限（缺省 1_000_000；内联 rows 登记前拒，blob 源超 → failed）。
    pub max_rows: u64,
    /// 单任务源字节上限（缺省 512 MiB；<1 MiB 拒——低于此值的导入不值得任务化）。
    pub max_bytes: u64,
    /// 默认批大小（缺省 5000；100..=50000——渲染 SQL 的语句级配额）。
    pub default_batch_size: usize,
    /// 任务 TTL 秒（缺省 24h；终态超时摘表，清扫即丢——import 无 blob 产物）。
    pub ttl_secs: u64,
    /// 回收器清扫间隔秒（缺省 1h）。
    pub sweep_interval_secs: u64,
}

impl Default for DbImportSection {
    fn default() -> Self {
        Self {
            global_concurrency: 4,
            per_subject_concurrency: 2,
            max_rows: 1_000_000,
            max_bytes: 512 * 1024 * 1024,
            default_batch_size: 5000,
            ttl_secs: 24 * 3600,
            sweep_interval_secs: 3600,
        }
    }
}

impl DbImportSection {
    /// 装配期 fail-loud 校验：Err 文案点名键（同 DbExportSection::validate 口径）。
    pub fn validate(&self) -> Result<(), String> {
        let check = |ok: bool, key: &str, msg: &str| {
            if ok {
                Ok(())
            } else {
                Err(format!("db_import.{key}: {msg}"))
            }
        };
        check(
            self.global_concurrency >= 1 && self.global_concurrency <= 64,
            "global_concurrency",
            "must be 1..=64",
        )?;
        check(
            self.per_subject_concurrency >= 1 && self.per_subject_concurrency <= 16,
            "per_subject_concurrency",
            "must be 1..=16",
        )?;
        check(self.max_rows >= 1, "max_rows", "must be >= 1")?;
        check(
            self.max_bytes >= 1024 * 1024,
            "max_bytes",
            "must be >= 1 MiB (1048576)",
        )?;
        check(
            self.default_batch_size >= 100 && self.default_batch_size <= 50000,
            "default_batch_size",
            "must be 100..=50000",
        )?;
        check(self.ttl_secs >= 1, "ttl_secs", "must be >= 1")?;
        check(
            self.sweep_interval_secs >= 1,
            "sweep_interval_secs",
            "must be >= 1",
        )
    }
}

/// 安全审计配置段（`audit:`，U-9 v0.1.98）。缺省**开**（有界环形缓冲，零凭据事件）；
/// 读面 `allow` 缺省不限制（单进程、零凭据，装配期 warn 提示收紧）。
/// `max_entries` / `retention_secs` 为 0 装配期拒启（= 不设限的 DoS 面）。
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuditSection {
    pub enabled: bool,
    pub max_entries: usize,
    pub retention_secs: u64,
    /// `audit.query` 的读取授权门（每 op 重判）。
    pub allow: AllowList,
}

impl Default for AuditSection {
    fn default() -> Self {
        Self {
            enabled: true,
            max_entries: 2000,
            retention_secs: 86400,
            allow: AllowList::default(),
        }
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
///
/// `SET_CONFIG`（v0.1.90）：PG 的 `set_config('search_path', …)` 是**会话级**改写，
/// 在池化连接上会污染同连接的后续请求（并改写内省可见范围），按写处置。
const SIDE_EFFECT_FNS: [&str; 7] = [
    "NEXTVAL",
    "SETVAL",
    "PG_TERMINATE_BACKEND",
    "PG_CANCEL_BACKEND",
    "LO_IMPORT",
    "LO_EXPORT",
    "SET_CONFIG",
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
        // `SET search_path = …`（v0.1.90）：PG 的 search_path 是会话级，池化连接上会污染
        // 后续请求（内省范围随之漂移），按写处置。
        "SET" => {
            if has("GLOBAL") || has("SESSION") || has("SEARCH_PATH") {
                (Risk::Write, false)
            } else {
                (Risk::Unknown, false)
            }
        }
        "COPY" | "LOAD" | "CALL" | "EXECUTE" => (Risk::Write, false),
        // `USE <db>`（mysql）/ `ATTACH[DATABASE]`、`DETACH`（sqlite）同为**会话级**改写：
        // 池化连接上会污染后续请求并改写内省可见范围 → 按写处置（v0.1.90）。
        "USE" | "ATTACH" | "DETACH" => (Risk::Write, false),
        "SHOW" | "EXPLAIN" | "DESCRIBE" | "DESC" | "PRAGMA" | "ANALYZE" | "VALUES" => {
            (Risk::ReadOnly, false)
        }
        "KILL" => {
            // MySQL 会话/语句取消（REQ-10 一期）：写语义但无 WHERE 面 → 非 dangerous，
            // 不吃 confirm gate；read_only 别名/会话仍拒（REQ-11 旁路取消的服务端前提）。
            (Risk::Write, false)
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

/// 「拒绝所有人」的 allow 表达：**声明**了 `modules` 维但为空。
///
/// `AllowList` 的空表默认是「不限制」（`is_unrestricted`），无法用它表达拒绝；
/// 而声明且为空的维在 `granted` 里永不命中——这正是 L3 动态库 fail-closed 需要的形态。
fn deny_all_allow() -> AllowList {
    AllowList {
        modules: Some(Vec::new()),
        ..Default::default()
    }
}

/// 运行期是否给该别名发过 grant（U-1 v0.1.93）。
pub(crate) fn has_runtime_grant(st: &StableState, alias: &str) -> bool {
    st.db_access_rt
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(alias)
}

/// op 边界的配额聚合出口（REQ-05）：策略 → Quota（DRY——query/exec/stream/builder
/// 四处调用点共用，不各自 policy_for）。限额与闸无关：off 档也生效（同 max_rows）。
pub(crate) fn quota_for(state: &Rc<RefCell<OpState>>, alias: &str) -> super::db_quota::Quota {
    super::db_quota::Quota::of(&policy_for(state, alias))
}

/// op 边界的访问控制入口：授权 → 分类 → 策略位 → 限额。
///
/// `enforce: off`（默认）零开销早退，与升级前行为完全一致。
/// 取某库的策略副本（装配期冻结，运行期只读 + U-1 运行期 grant 表）。
fn policy_for(state: &Rc<RefCell<OpState>>, alias: &str) -> DbAccess {
    let g = state.borrow();
    let st = g.borrow::<Arc<StableState>>();
    policy_for_state(st, alias)
}

/// `policy_for` 的纯函数内核（`db.allowed()` 只有 `&StableState`，也走同一套归一）。
pub(crate) fn policy_for_state(st: &StableState, alias: &str) -> DbAccess {
    // config 基线（defaults + 逐库覆盖合并）。
    let mut p = st.db_access.policy_for(alias);
    // L3 fail-closed（v0.1.91）判定用的「动态别名」标志：已动态登记且未在 `db_access.<name>`
    // 显式声明 → 无 config 兜底，缺 grant 即 deny。
    let is_dynamic = st.client_dbs.contains(alias) && !st.db_access.by_db.contains_key(alias);
    // 档位语义（v0.1.93 校正）：`deny` 是「**拒绝违规**」，不是「忽略 allow 硬拦所有人」。
    // L3 曾在 `granted_only` 里加过 Deny 短路，把 L1 的 allow 授权彻底废掉
    // （`db_access: {defaults: {enforce: deny, allow: {roles: [admin]}}}` 连 admin 都进不去）；
    // 同时 `deny` + 未写 `allow`（=`不限制主体`）也只剩「策略位照常执行」这一层含义。
    // U-1 v0.1.93：运行期 grant 表（只增补 allow，不动 config 的安全位）。
    let rt = st
        .db_access_rt
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(alias)
        .cloned();
    match rt {
        Some(entry) => {
            if is_dynamic {
                // 动态别名无 config 兜底：运行表项是完整策略（含其 read_only/max_rows 等）。
                p = entry;
            } else {
                // 静态 / config 兜底别名：只动「放行维」allow，保留 config 的安全位
                // （enforce/read_only/max_rows/allow_ddl/allow_dangerous）。
                // config 未限制主体（allow 全缺省）→ grant 直接成为白名单；
                // config 已有白名单 → 逐维并集（放宽，但不挤掉 config 已授权的角色）。
                if p.allow.is_unrestricted() {
                    p.allow = entry.allow.clone();
                } else {
                    p.allow.union_with(&entry.allow);
                }
                // grant 即「启用白名单制」：`off` 档下根本没有 enforce 判定，grant 会变成空操作，
                // 故抬到 `deny`（命中放行 / 未命中拒），而不是 `warn`（只记日志、不拒）。
                if p.enforce == Enforce::Off {
                    p.enforce = Enforce::Deny;
                }
            }
        }
        None if is_dynamic => {
            // L3 fail-closed：动态登记别名未声明于 `db_access.<name>` 且未运行期 grant →
            // **默认拒所有人**——杜绝「登记即全员可查内网库」。静态未知库仍回落 defaults。
            p.enforce = Enforce::Deny;
            p.allow = deny_all_allow();
        }
        None => { /* 静态别名无运行表 → 原样 config 基线 */ }
    }
    p
}

/// 调用主体快照（module / roles / tenant / source）。
pub(crate) fn identity(
    state: &Rc<RefCell<OpState>>,
) -> (Option<String>, Vec<String>, Option<String>, &'static str) {
    identity_of(&state.borrow())
}

/// G9：取请求头（大小写不敏感；server 层存原样键）。
fn req_header(state: &Rc<RefCell<OpState>>, name: &str) -> Option<String> {
    let g = state.borrow();
    g.borrow::<ReqState>()
        .req
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.clone())
}

/// [`identity`] 的 `&OpState` 版本（同步辅助函数复用；不构造 `Rc` 视图）。
pub(crate) fn identity_of(
    g: &OpState,
) -> (Option<String>, Vec<String>, Option<String>, &'static str) {
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
        return Ok(policy.max_rows); // 零开销：不分类、不取身份（确认门由调用方在早退前过）
    }
    // `enforce=deny` 不再短路硬拦：deny 是白名单制，未命中 allow 由 `evaluate` → `NotGranted`
    // 精确拒绝；「拒绝所有人」已在 `policy_for` 规范化为声明且为空的 allow。
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
    let c = classify(sql);
    // G9 确认门在 `enforce==Off` 早退**之前**（显式配 token = 要它生效，off 不豁免）。
    dangerous_confirm_gate(state, alias, &c)?;
    let policy = policy_for(state, alias);
    if policy.enforce == Enforce::Off {
        return Ok(policy.max_rows);
    }
    check_access_intent(state, alias, &c)
}

/// G9 危险语句确认门：`db_access.<alias>.dangerous_confirm_token` 配置后，危险语句
/// （无 WHERE / 恒真写）须请求头 `x-oj-confirm` **精确匹配** token 才放行；放行即审计
/// `DB_DANGEROUS_CONFIRM`（每条一条）。token 只从 config 来（运行期 grant 不覆盖安全位）。
/// 调用点约定：**必须在 `enforce==Off` 早退之前**（显式配 token = 要它生效，off 不豁免）
/// ——现有两处入口：`check_access`（裸 SQL）与 query.rs 构造器路径。
pub(crate) fn dangerous_confirm_gate(
    state: &Rc<RefCell<OpState>>,
    alias: &str,
    c: &Classified,
) -> Result<(), JsErrorBox> {
    if !c.dangerous {
        return Ok(());
    }
    let policy = policy_for(state, alias);
    let Some(tok) = &policy.dangerous_confirm_token else {
        return Ok(());
    };
    let given = req_header(state, "x-oj-confirm");
    if given.as_deref() != Some(tok.as_str()) {
        return Err(super::db_session::keep_err(format!(
            "alias {alias:?} requires confirmation for dangerous statements: set request \
             header 'x-oj-confirm' to db_access.{alias}.dangerous_confirm_token"
        )));
    }
    let (owner, st) = super::client_db::audit_ctx(state);
    super::audit::emit(
        &st.audit,
        "DB_DANGEROUS_CONFIRM",
        &owner,
        alias,
        &format!(
            "action=confirmed_dangerous risk={} sql_class=dangerous",
            c.risk.as_str()
        ),
    );
    Ok(())
}

/// **只判授权维**（v0.1.90）：给「平台自生成 SQL」的内省 op 用。
///
/// 与 [`check_access`] 的差别：不分类风险、不套 `read_only` / `allow_ddl` / `allow_dangerous`
/// 等策略位——内省 SQL 恒为只读，只读库也必须能看结构（否则 UI 连展开都做不到）。
/// 但**授权维必须过**：未授权主体不该靠内省枚举出别的库的表 / 列 / 外键指向（元数据泄露），
/// 否则 `db.allowed()` 的「只列已授权库」就成了假承诺。
///
/// `alias` 必须是 `guard::bound_db` 后的**物理库名**（`op_db_exec` 同款），否则
/// `manifest.db` 把字面 `"default"` 重定向到别的库的站点会被整条绕过。
/// `db.asSystem()` **不豁免**（`identity` 不读 `ReqState.system`）；`warn` 档放行、`deny` 档拒。
pub fn granted_only(state: &Rc<RefCell<OpState>>, alias: &str) -> Result<(), JsErrorBox> {
    let policy = policy_for(state, alias);
    if policy.enforce == Enforce::Off {
        return Ok(());
    }
    // `enforce=deny` 不再短路硬拦：deny 是白名单制，命中 allow 放行、未命中由下方 `granted`
    // 拒绝；「拒绝所有人」已由 `policy_for` 规范化为声明且为空的 allow。
    let (module, roles, tenant, source) = identity(state);
    let ctx = AccessCtx {
        module: module.as_deref(),
        roles,
        tenant: tenant.as_deref(),
        source,
    };
    if granted(&policy.allow, &ctx) {
        return Ok(());
    }
    let detail = format!(
        "alias {alias:?} not granted to this caller (module={:?}, roles={:?}, tenant={:?}, source={source})",
        ctx.module, ctx.roles, ctx.tenant
    );
    let msg = format!(
        "db access: {detail}\n  修复：由管理员在 db_access.<alias>.allow 为对应维度授权\n  (reason: {})",
        Reason::NotGranted.as_str()
    );
    if policy.enforce == Enforce::Warn {
        deny_log("warn", alias, Reason::NotGranted, &msg);
        return Ok(());
    }
    deny_log("deny", alias, Reason::NotGranted, &msg);
    Err(JsErrorBox::generic(msg))
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
#[path = "db_policy_tests.rs"]
mod tests;

// ---------------------------------------------------------------------------
// 行为面：经 `Bridge` 跑 handler（不是只测纯函数）
// ---------------------------------------------------------------------------

/// 行为测试：策略挂在真实 op 链上（query/exec/stream/构造器），而非只测纯函数。
///
/// `modules` 维不在本套件内：`ReqState.module` 只有 `run_module`（文件系统路径）会写，
/// `run_with` 恒 None —— 该维的语义由上面 `grant_is_union_of_declared_dimensions`
/// 钉死，行为面只覆盖 roles / tenants / sources 三维 + 策略位 + 限额 + 自省。
#[cfg(test)]
#[path = "db_policy_behavior_tests.rs"]
mod behavior;
