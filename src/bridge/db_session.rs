//! U-4 跨请求事务会话（`db.session`，v0.1.95）。
//!
//! # 为什么
//!
//! `db.tx(fn)` 的事务活在**一个请求**里（`ReqState.tx`），请求结束即回滚。dbx Web 的主旅程
//! 是「多步表单向导」：步骤 1 建草稿、步骤 2 改行、步骤 3 提交——中间隔着人类思考时间，
//! 不可能塞进一个 handler。本模块把事务句柄提升为**宿主级、按 id 路由**的会话。
//!
//! # 结构
//!
//! - [`TxSessionRegistry`]：`Arc` 壳（构造期冻结）+ 内部 `std::sync::Mutex`（运行期变动），
//!   挂在 `StableState.tx_sessions`。**不破不可变红线**（与 `client_dbs` 同哲学）。
//! - [`TxSessionEntry`]：元数据（归属/别名/世代号/计时）+ 一个 [`ActiveTx`]。
//!   复用 `ActiveTx` 是刻意的：`db::Target::Tx` 的所有既有分支（占位符、trace、行数上限、
//!   内省）**零改动**即可服务会话。
//! - 绑定时序：`ReqState.session = Some(id)` → `db::resolve_target` 命中即返回
//!   `Target::Tx(entry.tx)`，于是 `db.query/exec` 与 `db.table()` 构造器**全部**落到同一条连接。
//!
//! # 三道门（缺一不可）
//!
//! 1. `db_session.enabled` + `allow`：谁能开/续用会话（每次 op 重跑，角色撤销立即生效）；
//! 2. `db_access`：会话里每条语句照过 `check_access`（`read_only` 库写不进去）；
//! 3. `require_identity`：无 `user.id` 且无 tenant 拒建（匿名部署滥用面）。
//!
//! # 安全红线
//!
//! - id 是 bearer token：日志/错误只出现前 8 字符（[`handle_core::id_prefix`]）。
//! - 他人 id 与未知 id **同一条**错误（不泄露存在性）。
//! - **别名不重算**：判定一律用 `entry.alias`，绝不按当前请求的模块绑定重解析
//!   （否则「A 库判权、B 库执行」）。
//! - 会话内**禁止事务控制语句**（`BEGIN`/`COMMIT`/`ROLLBACK`/`SAVEPOINT`/`RELEASE`/`SET`/
//!   `USE`/`ATTACH`/`DETACH`/`PRAGMA`）：MySQL 上这些会隐式提交，制造「假回滚」。
//! - 容量满 → 拒绝，**绝不驱逐**他人会话（那等于替别人回滚在写事务）。
//!
//! 设计稿与评审处置：`docs/plans/2026-10-28-dbx-web-l7-u4-cross-request-tx.md`。

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use deno_core::{OpState, op2};
use deno_error::JsErrorBox;
use serde_json::Value;
use tokio::time::Instant;

use super::db::{ActiveTx, DataAccessor, Row, current_tx};
use super::db_policy::{self, AccessCtx, DbSessionSection};
use super::handle_core::{self, Owner};
use super::{ReqState, StableState};

/// id 前缀（形状白名单与日志遮蔽共用）。
const PREFIX: &str = "tx_";
/// tombstone 上限：只为「刚超时/刚丢弃」的诊断服务，不做出生记录。
const TOMBSTONE_CAP: usize = 256;
/// tombstone 存活时长。
const TOMBSTONE_TTL: Duration = Duration::from_secs(60);
/// 回收/收尾时 rollback 的硬超时（`await_ffi` 在 DB 不可达时是 yield 自旋，必须封顶）。
const ROLLBACK_TIMEOUT: Duration = Duration::from_secs(5);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // 毒化不放大成二次 panic：状态本身是「表 + 计数」，取回即可继续（评审 E6）。
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// 单个会话的诊断墓碑（仅本主体可见）。
#[derive(Clone)]
struct Tombstone {
    owner: Owner,
    state: &'static str,
    reason: String,
    at: Instant,
}

/// 一个跨请求事务会话。
pub struct TxSessionEntry {
    id: String,
    /// `bound_db` 之后的**物理**别名——判权的唯一权威（绝不按当前请求重算）。
    alias: String,
    owner: Owner,
    /// 别名登记世代号（`client_db` 的 epoch；静态库恒 0）。
    epoch: u64,
    /// 保活连接/句柄：Drop 序不再依赖 `StableState` 字段顺序（评审 E1）。
    #[allow(dead_code)]
    accessor: Arc<dyn DataAccessor>,
    /// 复用既有事务通路（`db::Target::Tx`）。
    tx: Arc<ActiveTx>,
    /// G9：只读会话位（begin 时定死、不可变）。Write/Ddl 在 op 层一律拒（`readonly_gate`）。
    read_only: bool,
    created: Instant,
    last_used: Mutex<Instant>,
    ops: AtomicU64,
    dead: AtomicBool,
}

impl TxSessionEntry {
    fn note_use(&self, now: Instant) {
        *lock(&self.last_used) = now;
        self.ops.fetch_add(1, Ordering::Relaxed);
    }
    fn last_used_at(&self) -> Instant {
        *lock(&self.last_used)
    }
    fn is_dead(&self) -> bool {
        self.dead.load(Ordering::SeqCst)
    }
    fn summary(&self, now: Instant, cfg: &DbSessionSection) -> Value {
        serde_json::json!({
            "id": self.id,
            "alias": self.alias,
            "dialect": self.tx.dialect_info.name,
            "state": if self.is_dead() { "discarded" } else { "open" },
            "readOnly": self.read_only,
            "ageMs": now.saturating_duration_since(self.created).as_millis() as u64,
            "idleMs": now.saturating_duration_since(self.last_used_at()).as_millis() as u64,
            "expiresInMs": handle_core::expires_in(
                self.created,
                self.last_used_at(),
                cfg.idle_timeout(),
                cfg.max_lifetime(),
                now,
            )
            .as_millis() as u64,
            "ops": self.ops.load(Ordering::Relaxed),
            "createdBy": self.owner.user,
        })
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct OwnerKey(Option<String>, Option<String>);

impl OwnerKey {
    fn of(o: &Owner) -> Self {
        Self(o.tenant.clone(), o.user.clone())
    }
}

struct Inner {
    sessions: HashMap<String, Arc<TxSessionEntry>>,
    /// 已占位、尚未连上的名额（先占位再建连接，防并发超发，评审 S4）。
    pending: HashMap<OwnerKey, usize>,
    pending_total: usize,
}

/// 宿主级会话表。
pub struct TxSessionRegistry {
    inner: Mutex<Inner>,
    tombstones: Mutex<VecDeque<(String, Tombstone)>>,
    boot: String,
    config: DbSessionSection,
    /// U-9：审计存储克隆——让 **reaper 后台路径**（TIMEOUT 判死，无 OpState）也能入库。
    audit: Arc<super::audit::AuditStore>,
    started: AtomicBool,
    timed_out: AtomicU64,
    discarded: AtomicU64,
    rejected: AtomicU64,
}

impl TxSessionRegistry {
    pub fn new(config: DbSessionSection, audit: Arc<super::audit::AuditStore>) -> Self {
        Self {
            inner: Mutex::new(Inner {
                sessions: HashMap::new(),
                pending: HashMap::new(),
                pending_total: 0,
            }),
            tombstones: Mutex::new(VecDeque::new()),
            boot: handle_core::new_boot_tag(),
            config,
            audit,
            started: AtomicBool::new(false),
            timed_out: AtomicU64::new(0),
            discarded: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
        }
    }

    /// 未配置/未开启时的默认表（`oj test` 与嵌入式构造用）。
    pub fn disabled() -> Self {
        Self::new(
            DbSessionSection::default(),
            Arc::new(super::audit::AuditStore::new(
                &super::db_policy::AuditSection::default(),
            )),
        )
    }

    pub fn config(&self) -> &DbSessionSection {
        &self.config
    }

    pub fn audit(&self) -> &Arc<super::audit::AuditStore> {
        &self.audit
    }

    /// U-9：会话事件统一出口（reaper 后台路径与 op 路径共用；kind ∈ BEGIN/COMMIT/…）。
    fn emit(&self, kind: &str, entry: &TxSessionEntry, reason: Option<&str>) {
        let detail = format!(
            "id={} ops={} age_ms={} reason={}",
            handle_core::id_prefix(PREFIX, &entry.id),
            entry.ops.load(Ordering::Relaxed),
            entry.created.elapsed().as_millis(),
            reason.unwrap_or("-"),
        );
        super::audit::emit(
            &self.audit,
            &format!("DB_SESSION_{kind}"),
            &entry.owner,
            &entry.alias,
            &detail,
        );
    }

    fn get(&self, id: &str) -> Option<Arc<TxSessionEntry>> {
        lock(&self.inner).sessions.get(id).cloned()
    }

    fn note_tombstone(&self, entry: &TxSessionEntry, state: &'static str, reason: &str) {
        let mut g = lock(&self.tombstones);
        g.push_front((
            entry.id.clone(),
            Tombstone {
                owner: entry.owner.clone(),
                state,
                reason: reason.to_string(),
                at: Instant::now(),
            },
        ));
        while g.len() > TOMBSTONE_CAP {
            g.pop_back();
        }
    }

    fn tombstone_for(&self, id: &str, owner: &Owner) -> Option<Tombstone> {
        let g = lock(&self.tombstones);
        g.iter()
            .find(|(k, t)| k == id && t.owner.same_subject(owner))
            .filter(|(_, t)| t.at.elapsed() < TOMBSTONE_TTL)
            .map(|(_, t)| t.clone())
    }

    /// 先占位再建连接：临界区内同时判全局与单主体上限。
    fn reserve(&self, owner: &Owner) -> Result<(), JsErrorBox> {
        let mut g = lock(&self.inner);
        let total = g.sessions.len() + g.pending_total;
        if total >= self.config.max_sessions {
            self.rejected.fetch_add(1, Ordering::Relaxed);
            return Err(JsErrorBox::generic(format!(
                "db session: limit reached ({total} active/opening, db_session.max_sessions = {}); \
                 commit/rollback/close some sessions first",
                self.config.max_sessions
            )));
        }
        let key = OwnerKey::of(owner);
        let mine = g
            .sessions
            .values()
            .filter(|e| e.owner.same_subject(owner))
            .count()
            + g.pending.get(&key).copied().unwrap_or(0);
        if mine >= self.config.max_per_owner {
            self.rejected.fetch_add(1, Ordering::Relaxed);
            return Err(JsErrorBox::generic(format!(
                "db session: per-caller limit reached ({mine} active/opening, \
                 db_session.max_per_owner = {})",
                self.config.max_per_owner
            )));
        }
        *g.pending.entry(key).or_insert(0) += 1;
        g.pending_total += 1;
        Ok(())
    }

    fn settle_pending(&self, g: &mut Inner, owner: &Owner) {
        let key = OwnerKey::of(owner);
        if let Some(n) = g.pending.get_mut(&key) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                g.pending.remove(&key);
            }
            g.pending_total = g.pending_total.saturating_sub(1);
        }
    }

    fn cancel_reservation(&self, owner: &Owner) {
        let mut g = lock(&self.inner);
        self.settle_pending(&mut g, owner);
    }

    fn insert(&self, entry: Arc<TxSessionEntry>, owner: &Owner) {
        let mut g = lock(&self.inner);
        let id = entry.id.clone();
        self.settle_pending(&mut g, owner);
        g.sessions.insert(id, entry);
    }

    /// 判死 + 摘表（可重入，幂等）。条目本身不立刻 drop：可能仍有 op 持 `Arc`，
    /// 由持有者下一次调用看到 `dead` 后失败退出。
    fn kill(&self, entry: &Arc<TxSessionEntry>, state: &'static str, reason: &str) {
        if !entry.dead.swap(true, Ordering::SeqCst) {
            self.discarded.fetch_add(1, Ordering::Relaxed);
        }
        {
            let mut g = lock(&self.inner);
            if g.sessions
                .get(&entry.id)
                .is_some_and(|e| Arc::ptr_eq(e, entry))
            {
                g.sessions.remove(&entry.id);
            }
        }
        self.note_tombstone(entry, state, reason);
    }

    /// 动态别名被删除/重新登记：其上的所有会话立即失效（评审 E8）。
    pub(crate) fn discard_alias(&self, alias: &str) -> usize {
        let targets: Vec<Arc<TxSessionEntry>> = {
            let g = lock(&self.inner);
            g.sessions
                .values()
                .filter(|e| e.alias == alias)
                .cloned()
                .collect()
        };
        for e in &targets {
            self.kill(
                e,
                "discarded",
                &format!("alias '{alias}' deleted or re-registered"),
            );
            self.emit("DISCARD", e, Some("alias_changed"));
        }
        targets.len()
    }

    /// 过期候选（只判定，不动表；真正回收在 [`Self::reap`]）。
    fn expired(&self, now: Instant) -> Vec<(Arc<TxSessionEntry>, &'static str)> {
        let g = lock(&self.inner);
        let mut out = Vec::new();
        for e in g.sessions.values() {
            if e.is_dead() {
                continue;
            }
            if let Some(reason) = handle_core::expired_reason(
                e.created,
                e.last_used_at(),
                self.config.idle_timeout(),
                self.config.max_lifetime(),
                now,
            ) {
                out.push((e.clone(), reason));
            }
        }
        out
    }

    /// 后台回收：拿到会话锁 → 硬超时 rollback；拿不到 = 有人正在跑，交给 op 侧的
    /// 剩余寿命封顶（[`op_timeout`]），本轮跳过（绝不阻塞运行时）。
    async fn reap(&self, now: Instant) {
        for (entry, reason) in self.expired(now) {
            let Ok(guard) = entry.tx.session.try_lock() else {
                continue;
            };
            let rolled = tokio::time::timeout(ROLLBACK_TIMEOUT, guard.rollback()).await;
            drop(guard);
            self.kill(&entry, "expired", reason);
            self.timed_out.fetch_add(1, Ordering::Relaxed);
            self.emit(
                if rolled.is_err() {
                    "TIMEOUT_ROLLBACK_FAILED"
                } else {
                    "TIMEOUT"
                },
                &entry,
                Some(reason),
            );
        }
    }

    fn start_reaper(self: &Arc<Self>) {
        if self.started.swap(true, Ordering::SeqCst) {
            return;
        }
        let weak = Arc::downgrade(self);
        let idle = self.config.idle_timeout();
        // 1s..=30s：太密浪费，太疏让「空闲回收」的可观测性变差。
        let tick = (idle / 4).clamp(Duration::from_secs(1), Duration::from_secs(30));
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(tick);
            iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            iv.tick().await; // 首次立即
            loop {
                iv.tick().await;
                // 每轮只短暂持有强引用：整循环持 Arc 会让 Backend 永不 drop（评审 E3）。
                let Some(reg) = weak.upgrade() else { break };
                let now = Instant::now();
                reg.reap(now).await;
                drop(reg);
            }
        });
    }

    fn own_sessions(&self, owner: &Owner) -> Vec<Arc<TxSessionEntry>> {
        let g = lock(&self.inner);
        let mut v: Vec<_> = g
            .sessions
            .values()
            .filter(|e| e.owner.same_subject(owner))
            .cloned()
            .collect();
        v.sort_by_key(|e| e.created);
        v
    }
}

// 会话事件统一走 TxSessionRegistry::emit（U-9：stderr 归一 `OJ_AUDIT:` + 入库）。

// ---------------------------------------------------------------------------
// 调用主体与条目解析（一律以 `&OpState` 取快照，绝不跨 await 持有借用）
// ---------------------------------------------------------------------------

fn registry_of(g: &OpState) -> Arc<TxSessionRegistry> {
    g.borrow::<Arc<StableState>>().tx_sessions.clone()
}

pub(crate) fn owner_of(g: &OpState) -> Owner {
    let rs = g.borrow::<ReqState>();
    let (_, _, tenant, source) = db_policy::identity_of(g);
    Owner {
        tenant,
        user: db_policy::operator_of(rs.req.user.as_ref()),
        module: rs.module.clone(),
        source,
    }
}

/// 会话门（`db_session.enabled` + `allow`）：**每个 op 重跑**（角色撤销立即生效，评审 S7）。
fn require_gate(g: &OpState) -> Result<Arc<TxSessionRegistry>, JsErrorBox> {
    let reg = registry_of(g);
    if !reg.config().enabled {
        return Err(JsErrorBox::generic(
            "db session: cross-request transactions are disabled \
             (set db_session.enabled: true and declare db_session.allow)",
        ));
    }
    let (module, roles, tenant, source) = db_policy::identity_of(g);
    let ctx = AccessCtx {
        module: module.as_deref(),
        roles,
        tenant: tenant.as_deref(),
        source,
    };
    if !db_policy::granted(&reg.config().allow, &ctx) {
        return Err(JsErrorBox::generic(format!(
            "db session: not granted to this caller (module={:?}, roles={:?}, tenant={:?}, source={source})\n  \
             修复：由管理员在 db_session.allow 为对应维度授权",
            ctx.module, ctx.roles, ctx.tenant
        )));
    }
    Ok(reg)
}

/// 解析并校验会话（形状 → 存在 → 归属 → 过期 → 世代号）。
///
/// **他人 id / 未知 id / 形状非法 → 同一条消息**（不泄露存在性）；仅本主体能看到
/// 「过期/别名变更」的具体原因（产品可观测性，评审 P2）。
fn lookup_entry(g: &OpState, id: &str) -> Result<Arc<TxSessionEntry>, JsErrorBox> {
    let reg = registry_of(g);
    let owner = owner_of(g);
    let shown = handle_core::id_prefix(PREFIX, id);
    let not_found = || {
        JsErrorBox::generic(format!(
            "tx session {shown} not found (unknown, expired, or owned by another caller)"
        ))
    };
    if !handle_core::id_is_wellformed(PREFIX, id) {
        return Err(not_found());
    }
    let Some(entry) = reg.get(id) else {
        if let Some(t) = reg.tombstone_for(id, &owner) {
            // 只有非「已正常收尾」的死法才敢说 rolled back：commit 成功的会话若也这么说
            // 就是在错误文案里撒谎（排查的人会去核对"被回滚的数据"）。
            let ended = if t.state == "closed" {
                "the transaction has ended"
            } else {
                "it was rolled back"
            };
            return Err(JsErrorBox::generic(format!(
                "tx session {shown} is {} ({}); {ended}",
                t.state, t.reason
            )));
        }
        return Err(not_found());
    };
    if !entry.owner.same_subject(&owner) {
        return Err(not_found());
    }
    let cfg = reg.config().clone();
    if let Some(reason) = handle_core::expired_reason(
        entry.created,
        entry.last_used_at(),
        cfg.idle_timeout(),
        cfg.max_lifetime(),
        Instant::now(),
    ) {
        reg.kill(&entry, "expired", reason);
        reg.timed_out.fetch_add(1, Ordering::Relaxed);
        reg.emit("TIMEOUT", &entry, Some(reason));
        return Err(JsErrorBox::generic(format!(
            "tx session {shown} expired ({reason}); it was rolled back"
        )));
    }
    // 动态别名被删除/重新登记 → 会话立即失效（绝不在轮换后的连接上继续写）。
    let epoch_now = g
        .borrow::<Arc<StableState>>()
        .client_dbs
        .epoch_of(&entry.alias);
    if epoch_now != entry.epoch {
        reg.kill(
            &entry,
            "discarded",
            &format!("alias '{}' changed", entry.alias),
        );
        reg.emit("DISCARD", &entry, Some("alias_changed"));
        return Err(JsErrorBox::generic(format!(
            "tx session {shown} refers to db alias '{}' whose registration changed; session discarded",
            entry.alias
        )));
    }
    Ok(entry)
}

/// 绑定路由：`db::resolve_target` 在 `ReqState.session` 有值时调用本函数。
///
/// 返回 `Ok(Some(tx))` = 命中会话；`Ok(None)` = 本请求没绑定会话（走原有通路）。
pub(crate) fn attached_target(
    state: &Rc<RefCell<OpState>>,
    name: &str,
) -> Result<Option<Arc<ActiveTx>>, JsErrorBox> {
    let id = {
        let g = state.borrow();
        match g.borrow::<ReqState>().session.clone() {
            Some(id) => id,
            None => return Ok(None),
        }
    };
    let entry = {
        let g = state.borrow();
        lookup_entry(&g, &id)?
    };
    // 绑定校验：会话属于哪个别名，就只认哪个别名（`bound_db` 会按 manifest 绑定/
    // `db_override` 改写字面 default，二者必须一致，否则 A 策略判、B 库执行）。
    let phys = super::guard::bound_db(state, name);
    if entry.alias != phys {
        return Err(JsErrorBox::generic(format!(
            "db session is attached to db '{}', not '{phys}' (detach first)",
            entry.alias
        )));
    }
    entry.note_use(Instant::now());
    Ok(Some(entry.tx.clone()))
}

fn set_session(state: &Rc<RefCell<OpState>>, v: Option<String>) -> Option<String> {
    let mut g = state.borrow_mut();
    std::mem::replace(&mut g.borrow_mut::<ReqState>().session, v)
}

/// 事务控制语句（会话内一律拒绝，**不受 `db_access.enforce` 档位影响**）。
///
/// 理由：MySQL 上 `BEGIN` / `SET autocommit=1` / DDL 会隐式提交——先写、再 `BEGIN`、
/// 最后 rollback 会得到「假回滚」，数据其实已落库（评审 S1）。`classify` 已把
/// BEGIN/COMMIT/ROLLBACK/SAVEPOINT/RELEASE/START 归 `Risk::Transaction`，这里补上
/// 其余会改写会话状态的关键字。
fn session_sql_forbidden(sql: &str) -> Option<&'static str> {
    if db_policy::classify(sql).risk == db_policy::Risk::Transaction {
        return Some("transaction control statement");
    }
    let kw = sql
        .trim_start()
        .split(|c: char| c.is_whitespace() || c == '(')
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    match kw.as_str() {
        "SET" => Some("session variable statement (SET)"),
        "USE" => Some("database switch (USE)"),
        "ATTACH" | "DETACH" => Some("database attach/detach"),
        "PRAGMA" => Some("PRAGMA"),
        _ => None,
    }
}

/// 会话内语句的额外禁令（只作用于**绑定了会话**的请求；`db.tx` 的请求作用域事务不变）。
pub(crate) fn forbid_in_session(state: &Rc<RefCell<OpState>>, sql: &str) -> Result<(), JsErrorBox> {
    if state.borrow().borrow::<ReqState>().session.is_none() {
        return Ok(());
    }
    if let Some(what) = session_sql_forbidden(sql) {
        return Err(JsErrorBox::generic(format!(
            "db session: {what} is not allowed inside a cross-request transaction \
             (use db.session.commit/rollback to end it)"
        )));
    }
    Ok(())
}

/// G9：op 层拒绝但**会话保留**的错误（信封 `session_disposition=keep`，JS 面
/// `e.sessionDisposition`——G9 验收点）。
pub(crate) fn keep_err(msg: String) -> JsErrorBox {
    super::db::to_js_err(Box::new(super::db_error::BridgeError {
        category: super::db_error::DbErrorCategory::Unsupported,
        retryable: false,
        session_disposition: super::db_error::SessionDisposition::Keep,
        message: msg,
        data: Default::default(),
    }))
}

/// G9 只读会话位 + 写拦截（op 层强制——不经任何 handler、直发 op 也拦得住）。
/// `ReadOnly` 位 = true 时 Write/Ddl 一律拒；会话本身保留（只读拒绝不判死）。
fn reject_readonly(alias: &str, sql: &str) -> Result<(), JsErrorBox> {
    let c = db_policy::classify(sql);
    if matches!(c.risk, db_policy::Risk::Write | db_policy::Risk::Ddl) {
        return Err(keep_err(format!(
            "db session on alias '{alias}' is read-only; statement classified {} was rejected \
             (begin a session without readOnly, or use a writable alias)",
            c.risk.as_str()
        )));
    }
    Ok(())
}

/// attach 路径的门（`db::query_impl`/`exec_impl` 在 `forbid_in_session` 旁调用）。
pub(crate) fn readonly_gate(state: &Rc<RefCell<OpState>>, sql: &str) -> Result<(), JsErrorBox> {
    let g = state.borrow();
    let Some(id) = g.borrow::<ReqState>().session.clone() else {
        return Ok(());
    };
    let reg = registry_of(&g);
    let Some(entry) = reg.get(&id) else {
        return Ok(());
    };
    drop(g);
    if entry.read_only {
        reject_readonly(&entry.alias, sql)?;
    }
    Ok(())
}

/// 显式 op 路径的门（`db.session.query/exec` 按 id 路由，不经 attach）。
fn gate_session_sql(state: &Rc<RefCell<OpState>>, id: &str, sql: &str) -> Result<(), JsErrorBox> {
    let g = state.borrow();
    let reg = registry_of(&g);
    let Some(entry) = reg.get(id) else {
        return Ok(()); // 未知 id 交给 run_attached 的统一报错
    };
    drop(g);
    if entry.read_only {
        reject_readonly(&entry.alias, sql)?;
    }
    Ok(())
}

/// 已绑定会话的剩余寿命（`db::query_impl`/`exec_impl` 据此做在飞封顶，评审 S5）。
///
/// `None` = 本请求没绑定会话（走池或 `db.tx`，无额外封顶）。
pub(crate) fn attached_deadline(state: &Rc<RefCell<OpState>>) -> Option<Duration> {
    let g = state.borrow();
    let id = g.borrow::<ReqState>().session.clone()?;
    let reg = registry_of(&g);
    let entry = reg.get(&id)?;
    Some(handle_core::expires_in(
        entry.created,
        entry.last_used_at(),
        reg.config().idle_timeout(),
        reg.config().max_lifetime(),
        Instant::now(),
    ))
}

/// 在飞超时：判死绑定会话（连接状态不可信，不再复用；后续 op 立即失败）。
pub(crate) fn kill_attached(state: &Rc<RefCell<OpState>>, reason: &str) {
    let g = state.borrow();
    let Some(id) = g.borrow::<ReqState>().session.clone() else {
        return;
    };
    let reg = registry_of(&g);
    if let Some(entry) = reg.get(&id) {
        reg.kill(&entry, "discarded", reason);
        reg.emit("DISCARD", &entry, Some(reason));
    }
}

// ---------------------------------------------------------------------------
// ops
// ---------------------------------------------------------------------------

/// 开启跨请求事务会话。
#[op2]
#[serde]
pub async fn op_db_session_begin(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[serde] opts: Option<Value>,
) -> Result<serde_json::Value, JsErrorBox> {
    // G9：只读会话位（begin 时定死，不可变——要写就开新会话）。
    let read_only = opts
        .as_ref()
        .and_then(|o| o.get("readOnly"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let (reg, owner) = {
        let g = state.borrow();
        (require_gate(&g)?, owner_of(&g))
    };
    let cfg = reg.config().clone();
    if cfg.require_identity && !owner.has_identity() {
        return Err(JsErrorBox::generic(
            "db session: cross-request transactions require an authenticated caller \
             (user.id or tenant); set db_session.require_identity: false for local dev only",
        ));
    }
    // 与既有两条路径互斥（防两条写路径互等行锁）。
    if current_tx(&state).is_some() {
        return Err(JsErrorBox::generic(
            "per-request transaction active on this request; finish db.tx before opening a session",
        ));
    }
    if state.borrow().borrow::<ReqState>().session.is_some() {
        return Err(JsErrorBox::generic(
            "a db session is already attached to this request (detach first)",
        ));
    }
    let alias = super::guard::bound_db(&state, &name);
    // 授权维先过：会话是一条长期连接，未授权的库连开都不该开。
    db_policy::granted_only(&state, &alias)?;
    let da = super::query::lookup(&state, &name)?;
    let epoch = state
        .borrow()
        .borrow::<Arc<StableState>>()
        .client_dbs
        .epoch_of(&alias);
    reg.reserve(&owner)?;
    let session = match da.begin().await {
        Ok(s) => s,
        Err(e) => {
            reg.cancel_reservation(&owner);
            return Err(super::db::to_js_err(e));
        }
    };
    let info = da.dialect_info().clone();
    let entry = Arc::new(TxSessionEntry {
        id: handle_core::new_id(PREFIX, &reg.boot)
            .map_err(|e| JsErrorBox::generic(format!("db session: {e}")))?,
        alias: alias.clone(),
        owner: owner.clone(),
        epoch,
        accessor: da,
        tx: Arc::new(ActiveTx {
            db: alias,
            session: tokio::sync::Mutex::new(session),
            dialect_info: info,
        }),
        read_only,
        created: Instant::now(),
        last_used: Mutex::new(Instant::now()),
        ops: AtomicU64::new(0),
        dead: AtomicBool::new(false),
    });
    let out = entry.summary(Instant::now(), &cfg);
    reg.insert(entry.clone(), &owner);
    reg.start_reaper();
    reg.emit("BEGIN", &entry, None);
    Ok(out)
}

/// 把会话绑到本请求：之后 `db.query/exec/table()` 全部落同一条连接。
// nofast：绑定冲突/会话失效必须抛回 JS（fast op 无异常通道）。
#[op2(nofast)]
pub fn op_db_session_attach(
    state: Rc<RefCell<OpState>>,
    #[string] id: String,
) -> Result<bool, JsErrorBox> {
    let (entry, prev_tx, prev_session) = {
        let g = state.borrow();
        let entry = lookup_entry(&g, &id)?;
        let rs = g.borrow::<ReqState>();
        (entry, rs.tx.is_some(), rs.session.clone())
    };
    if prev_tx {
        return Err(JsErrorBox::generic(
            "per-request transaction active on this request; finish db.tx before attaching a session",
        ));
    }
    if let Some(prev) = prev_session
        && prev != entry.id
    {
        return Err(JsErrorBox::generic(format!(
            "db session {} is already attached (detach first)",
            handle_core::id_prefix(PREFIX, &prev)
        )));
    }
    state.borrow_mut().borrow_mut::<ReqState>().session = Some(entry.id.clone());
    Ok(true)
}

/// 解除绑定（不清会话本身）。
#[op2(fast)]
pub fn op_db_session_detach(state: &mut OpState) -> bool {
    state.borrow_mut::<ReqState>().session = None;
    true
}

/// 会话内查询（等价于 `attach` 后 `db.query`，逐语句显式形态）。
#[op2]
#[serde]
pub async fn op_db_session_query(
    state: Rc<RefCell<OpState>>,
    #[string] id: String,
    #[string] sql: String,
    #[serde] params: Option<Vec<Value>>,
) -> Result<Vec<Row>, JsErrorBox> {
    let params = params.unwrap_or_default();
    gate_session_sql(&state, &id, &sql)?; // G9 只读会话位（直发 op 也拦）
    run_attached(&state, &id, move |st, alias| async move {
        super::db::query_impl(st, alias, sql, params).await
    })
    .await
}

/// 会话内执行（等价于 `attach` 后 `db.exec`）。
#[op2]
#[serde]
pub async fn op_db_session_exec(
    state: Rc<RefCell<OpState>>,
    #[string] id: String,
    #[string] sql: String,
    #[serde] params: Option<Vec<Value>>,
) -> Result<i64, JsErrorBox> {
    let params = params.unwrap_or_default();
    gate_session_sql(&state, &id, &sql)?; // G9 只读会话位（直发 op 也拦）
    run_attached(&state, &id, move |st, alias| async move {
        super::db::exec_impl(st, alias, sql, params).await
    })
    .await
}

/// 会话 op 的统一前置：门 → 条目 → 临时绑定 → 委托既有 impl → 还原绑定。
///
/// 语句禁令与在飞封顶都在委托侧（`db::query_impl`/`exec_impl`）——临时绑定期间它们看到
/// 的就是「已绑定会话」，因此 attach 路径与显式形态共用同一套判定，不存在两套口径。
async fn run_attached<T, F, Fut>(
    state: &Rc<RefCell<OpState>>,
    id: &str,
    f: F,
) -> Result<T, JsErrorBox>
where
    F: FnOnce(Rc<RefCell<OpState>>, String) -> Fut,
    Fut: std::future::Future<Output = Result<T, JsErrorBox>>,
{
    let entry = {
        let g = state.borrow();
        require_gate(&g)?;
        lookup_entry(&g, id)?
    };
    let prev = set_session(state, Some(entry.id.clone()));
    let out = f(state.clone(), entry.alias.clone()).await;
    // 一定要还原（失败也还原）：临时绑定不得泄漏给后续 op。
    set_session(state, prev);
    out
}

/// 提交并销毁会话（失败也摘表：结果不确定时不再允许续用）。
#[op2]
pub async fn op_db_session_commit(
    state: Rc<RefCell<OpState>>,
    #[string] id: String,
) -> Result<bool, JsErrorBox> {
    let (reg, entry) = {
        let g = state.borrow();
        (require_gate(&g)?, lookup_entry(&g, &id)?)
    };
    let out = {
        let guard = entry.tx.session.lock().await;
        match tokio::time::timeout(ROLLBACK_TIMEOUT, guard.commit()).await {
            Ok(Ok(())) => Ok(true),
            Ok(Err(e)) => Err(super::db::to_js_err(e)),
            Err(_) => Err(JsErrorBox::generic(
                "db session commit timed out; outcome is UNCERTAIN — verify the data before retrying",
            )),
        }
    };
    reg.kill(&entry, "closed", "commit");
    reg.emit("COMMIT", &entry, None);
    out
}

/// 回滚并销毁会话。
#[op2]
pub async fn op_db_session_rollback(
    state: Rc<RefCell<OpState>>,
    #[string] id: String,
) -> Result<bool, JsErrorBox> {
    let (reg, entry) = {
        let g = state.borrow();
        (require_gate(&g)?, lookup_entry(&g, &id)?)
    };
    let out = {
        let guard = entry.tx.session.lock().await;
        match tokio::time::timeout(ROLLBACK_TIMEOUT, guard.rollback()).await {
            Ok(Ok(())) => Ok(true),
            Ok(Err(e)) => Err(super::db::to_js_err(e)),
            Err(_) => Err(JsErrorBox::generic(
                "db session rollback timed out; the connection was discarded",
            )),
        }
    };
    reg.kill(&entry, "closed", "rollback");
    reg.emit("ROLLBACK", &entry, None);
    out
}

/// 幂等释放（`finally` 用）：best-effort 回滚，任何失败都不上抛。
#[op2]
pub async fn op_db_session_close(
    state: Rc<RefCell<OpState>>,
    #[string] id: String,
) -> Result<bool, JsErrorBox> {
    let reg = registry_of(&state.borrow());
    if !reg.config().enabled {
        return Ok(true);
    }
    let Ok(entry) = lookup_entry(&state.borrow(), &id) else {
        return Ok(true);
    };
    {
        let guard = entry.tx.session.lock().await;
        if tokio::time::timeout(ROLLBACK_TIMEOUT, guard.rollback())
            .await
            .is_err()
        {
            reg.emit("CLOSE_ERR", &entry, Some("rollback_timeout"));
        }
    }
    reg.kill(&entry, "closed", "close");
    Ok(true)
}

/// 续期：只推空闲计时，不延长最长寿命。
#[op2]
#[serde]
pub fn op_db_session_touch(
    state: Rc<RefCell<OpState>>,
    #[string] id: String,
) -> Result<serde_json::Value, JsErrorBox> {
    let (entry, cfg) = {
        let g = state.borrow();
        require_gate(&g)?;
        let entry = lookup_entry(&g, &id)?;
        let cfg = registry_of(&g).config().clone();
        (entry, cfg)
    };
    entry.note_use(Instant::now());
    Ok(serde_json::json!({
        "expiresInMs": handle_core::expires_in(
            entry.created,
            entry.last_used_at(),
            cfg.idle_timeout(),
            cfg.max_lifetime(),
            Instant::now(),
        )
        .as_millis() as u64,
    }))
}

/// 关闭本主体全部会话（登出/收尾用），返回关闭条数。
#[op2]
#[serde]
pub async fn op_db_session_close_all(state: Rc<RefCell<OpState>>) -> Result<u64, JsErrorBox> {
    let (reg, owner) = {
        let g = state.borrow();
        (require_gate(&g)?, owner_of(&g))
    };
    let mut n = 0u64;
    for entry in reg.own_sessions(&owner) {
        {
            let Ok(guard) = entry.tx.session.try_lock() else {
                continue; // 正在跑：留给超时回收，不阻塞调用方
            };
            let _ = tokio::time::timeout(ROLLBACK_TIMEOUT, guard.rollback()).await;
        }
        reg.kill(&entry, "closed", "close_all");
        n += 1;
    }
    Ok(n)
}

/// 本主体活跃会话（跨库）。
#[op2]
#[serde]
pub fn op_db_session_list(state: Rc<RefCell<OpState>>) -> Vec<serde_json::Value> {
    let g = state.borrow();
    let reg = registry_of(&g);
    let cfg = reg.config().clone();
    if !cfg.enabled {
        return Vec::new();
    }
    let owner = owner_of(&g);
    let now = Instant::now();
    reg.own_sessions(&owner)
        .iter()
        .map(|e| e.summary(now, &cfg))
        .collect()
}

/// 单条会话摘要（含 tombstone 诊断）。
#[op2]
#[serde]
pub fn op_db_session_info(
    state: Rc<RefCell<OpState>>,
    #[string] id: String,
) -> Result<serde_json::Value, JsErrorBox> {
    let g = state.borrow();
    let entry = lookup_entry(&g, &id)?;
    let cfg = registry_of(&g).config().clone();
    Ok(entry.summary(Instant::now(), &cfg))
}

/// 会话表聚合（运维观察面；只给本主体的活跃数，不泄露他主体身份）。
#[op2]
#[serde]
pub fn op_db_session_stats(state: Rc<RefCell<OpState>>) -> serde_json::Value {
    let g = state.borrow();
    let reg = registry_of(&g);
    let cfg = reg.config().clone();
    let active = reg.own_sessions(&owner_of(&g)).len();
    serde_json::json!({
        "enabled": cfg.enabled,
        "active": active,
        "maxSessions": cfg.max_sessions,
        "maxPerOwner": cfg.max_per_owner,
        "idleTimeoutSecs": cfg.idle_timeout_secs,
        "maxLifetimeSecs": cfg.max_lifetime_secs,
        "timedOut": reg.timed_out.load(Ordering::Relaxed),
        "discarded": reg.discarded.load(Ordering::Relaxed),
        "rejected": reg.rejected.load(Ordering::Relaxed),
        "boot": reg.boot,
    })
}

#[cfg(test)]
mod tests {
    //! 会话的语义全部在**请求边界**上，故这里一律经 `Bridge::run` 跑 JS：一次 `run` = 一个
    //! 请求（独立 isolate 复用 + `ReqState::reset`）。这样「会话跨请求存活」不是靠内部
    //! 断言推出来的，而是被真正跨过三次请求验证的。
    use super::*;
    use crate::bridge::db::InMemoryAccessor;
    use crate::bridge::db_policy::AllowList;
    use crate::bridge::{Bridge, DataAccessor, Extras, InMemoryKV, RequestInfo, SchemaRegistry};
    use serde_json::{Value, json};

    /// 临时 sqlite 文件（**不用 `:memory:`**：会话要跨请求，连接必须落在同一个真实库上；
    /// 文件也让「提交后重开连接可见」这件事可验证）。
    fn tmp_dsn(tag: &str) -> String {
        let p = std::env::temp_dir().join(format!("oj-sess-{}-{tag}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        format!("sqlite://{}", p.display())
    }

    async fn sqlite(tag: &str) -> Arc<dyn DataAccessor> {
        // 走全仓唯一的 DSN 归一入口（建文件 + Windows 盘符坑）；手拼 `sqlite://` 直喂
        // sqlx 会被 Url 解析吞掉绝对路径（盘符成 host）→ SQLITE_CANTOPEN。
        crate::bridge::db_backend::DbBackendRegistry::builtin()
            .connect(&tmp_dsn(tag), &std::env::temp_dir())
            .await
            .unwrap()
    }

    fn cfg(edit: impl FnOnce(&mut DbSessionSection)) -> DbSessionSection {
        let mut c = DbSessionSection {
            enabled: true,
            // 单测没有 auth 插件：身份由 RequestInfo.user 直接注入，不看 require_identity。
            require_identity: false,
            ..DbSessionSection::default()
        };
        edit(&mut c);
        c
    }

    fn bridge(dbs: Vec<(&str, Arc<dyn DataAccessor>)>, c: DbSessionSection) -> Bridge {
        Bridge::with_dbs_and_loader(
            dbs.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            None,
            Extras {
                db_session: Some(Arc::new(c)),
                ..Default::default()
            },
        )
    }

    /// 单库桥（temp sqlite 已建 t 表）。
    async fn bridge_t(tag: &str, c: DbSessionSection) -> Bridge {
        let db = sqlite(tag).await;
        db.exec_with_params("create table t (id integer primary key, v text)", &[])
            .await
            .unwrap();
        bridge(vec![("default", db)], c)
    }

    async fn run(b: &Bridge, src: &str) -> Value {
        let cap = b.run(src).await.unwrap();
        serde_json::from_slice(&cap.body).unwrap()
    }

    async fn run_as(b: &Bridge, user: Option<Value>, src: &str) -> Value {
        let req = RequestInfo {
            user,
            ..RequestInfo::default()
        };
        let cap = b.run_with(src, req).await.unwrap();
        serde_json::from_slice(&cap.body).unwrap()
    }

    /// 池上查询（会话必须在提交/回滚之后才允许走池：sqlite 池刻意只有 1 条连接）。
    async fn count_via_pool(b: &Bridge) -> i64 {
        let v = run(b, r#"db.query("select count(*) c from t").then((r) => json.ok(r[0])).catch((e) => json.fail(500, String(e)))"#).await;
        v["data"]["c"]
            .as_i64()
            .unwrap_or_else(|| panic!("pool query failed: {v}"))
    }

    fn id_of(v: &Value) -> String {
        v["data"]["id"]
            .as_str()
            .unwrap_or_else(|| panic!("no session id: {v}"))
            .to_string()
    }

    // ---------------------------------------------------------------------
    // 主旅程：会话跨请求存活，未提交的写在下一个请求里仍可见，最后才提交
    // ---------------------------------------------------------------------

    #[tokio::test(flavor = "current_thread")]
    async fn session_outlives_requests_and_only_commit_makes_it_visible() {
        let b = bridge_t("xreq", cfg(|_| {})).await;
        // 请求 1：开会话 + 写一行（不提交）
        let v = run(
            &b,
            r#"db.session.begin()
                 .then((s) => db.session.exec(s.id, "insert into t (v) values (?)", ["a"]).then(() => json.ok(s)))
                 .catch((e) => json.fail(500, String(e)))"#,
        )
        .await;
        let id = id_of(&v);
        // id 形状即契约（前缀 + 4 位 boot tag + 32 位随机）
        assert!(
            handle_core::id_is_wellformed(PREFIX, &id),
            "bad id shape: {id}"
        );

        // 请求 2：attach 后 db.query 走**同一条连接** → 看得见未提交的写
        let v = run(
            &b,
            &format!(
                r#"db.session.attach("{id}")
                     .then(() => db.query("select count(*) c from t"))
                     .then((r) => json.ok(r[0]))
                     .catch((e) => json.fail(500, String(e)))"#
            ),
        )
        .await;
        assert_eq!(
            v["data"]["c"], 1,
            "session must see its own uncommitted write: {v}"
        );

        // 请求 3：提交
        let v = run(
            &b,
            &format!(r#"db.session.commit("{id}").then(() => json.ok({{}})).catch((e) => json.fail(500, String(e)))"#),
        )
        .await;
        assert_eq!(v["code"], 0, "commit failed: {v}");

        // 提交后连接归还池 → 池查询看得见
        assert_eq!(count_via_pool(&b).await, 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rollback_discards_writes_and_reports_a_tombstone() {
        let b = bridge_t("rb", cfg(|_| {})).await;
        let v = run(
            &b,
            r#"db.session.begin()
                 .then((s) => db.session.exec(s.id, "insert into t (v) values (?)", ["a"]).then(() => db.session.rollback(s.id)).then(() => json.ok(s)))
                 .catch((e) => json.fail(500, String(e)))"#,
        )
        .await;
        let id = id_of(&v);
        assert_eq!(
            count_via_pool(&b).await,
            0,
            "rollback must discard the insert"
        );
        // 已结束的会话：本主体能看到具体死法，而不是含糊的 not found；
        // 且只有**非正常收尾**才许说 "rolled back"（commit 成功却这么写就是撒谎）。
        let v = run(
            &b,
            &format!(r#"db.session.attach("{id}").then(() => json.ok({{}})).catch((e) => json.ok({{ msg: String(e) }}))"#),
        )
        .await;
        let msg = v["data"]["msg"].as_str().unwrap();
        assert!(
            msg.contains("closed") && msg.contains("rollback") && msg.contains("has ended"),
            "{msg}"
        );
        assert!(!msg.contains("rolled back"), "{msg}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn request_boundary_does_not_roll_back_an_open_session() {
        let b = bridge_t("boundary", cfg(|_| {})).await;
        let v = run(
            &b,
            r#"db.session.begin()
                 .then((s) => db.session.exec(s.id, "insert into t (v) values (?)", ["a"]).then(() => json.ok(s)))
                 .catch((e) => json.fail(500, String(e)))"#,
        )
        .await;
        let id = id_of(&v);
        // 第三个请求仍能续用（若请求结束回滚了，这里会 not found）
        let v = run(
            &b,
            &format!(
                r#"db.session.info("{id}").then((i) => json.ok(i)).catch((e) => json.fail(500, String(e)))"#
            ),
        )
        .await;
        assert_eq!(v["data"]["state"], "open", "{v}");
        assert_eq!(
            v["data"]["ops"], 1,
            "ops must accumulate across requests: {v}"
        );
        let _ = run(&b, &format!(r#"db.session.rollback("{id}").then(() => json.ok({{}})).catch(() => json.ok({{}}))"#)).await;
    }

    // ---------------------------------------------------------------------
    // 门禁
    // ---------------------------------------------------------------------

    #[tokio::test(flavor = "current_thread")]
    async fn disabled_by_default() {
        // 缺省配置（enabled=false）：与升级前完全一致——能力不存在，且报错点名怎么开。
        let b = bridge_t("off", DbSessionSection::default()).await;
        let v = run(
            &b,
            r#"db.session.begin().then(() => json.ok({})).catch((e) => json.ok({ msg: String(e) }))"#,
        )
        .await;
        let msg = v["data"]["msg"].as_str().unwrap();
        assert!(msg.contains("disabled"), "{msg}");
        assert!(msg.contains("db_session.enabled"), "{msg}");
        // 读面保持静默（list/stats 不抛）：未启用 = 空表
        let v = run(
            &b,
            r#"db.session.list().then((l) => json.ok({ n: l.length }))"#,
        )
        .await;
        assert_eq!(v["data"]["n"], 0, "{v}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn allow_list_is_rechecked_per_call() {
        // 允许 db-admin 角色；普通 user 被拒
        let b = bridge_t(
            "gate",
            cfg(|c| {
                c.allow = AllowList {
                    roles: Some(vec!["db-admin".into()]),
                    ..Default::default()
                }
            }),
        )
        .await;
        let v = run_as(
            &b,
            Some(json!({"id": "u1", "roles": ["user"]})),
            r#"db.session.begin().then(() => json.ok({})).catch((e) => json.ok({ msg: String(e) }))"#,
        )
        .await;
        assert!(
            v["data"]["msg"].as_str().unwrap().contains("not granted"),
            "{v}"
        );
        let v = run_as(
            &b,
            Some(json!({"id": "u1", "roles": ["db-admin"]})),
            r#"db.session.begin().then((s) => db.session.rollback(s.id)).then(() => json.ok({})).catch((e) => json.fail(500, String(e)))"#,
        )
        .await;
        assert_eq!(v["code"], 0, "{v}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn require_identity_rejects_anonymous_callers() {
        let b = bridge_t("ident", cfg(|c| c.require_identity = true)).await;
        let v = run(
            &b,
            r#"db.session.begin().then(() => json.ok({})).catch((e) => json.ok({ msg: String(e) }))"#,
        )
        .await;
        assert!(
            v["data"]["msg"]
                .as_str()
                .unwrap()
                .contains("authenticated caller"),
            "{v}"
        );
    }

    // ---------------------------------------------------------------------
    // 防伪造 / 归属隔离 / 容量
    // ---------------------------------------------------------------------

    #[tokio::test(flavor = "current_thread")]
    async fn unknown_and_foreign_ids_are_indistinguishable() {
        let b = bridge_t("iso", cfg(|_| {})).await;
        let v = run_as(
            &b,
            Some(json!({"id": "alice"})),
            r#"db.session.begin().then((s) => json.ok(s)).catch((e) => json.fail(500, String(e)))"#,
        )
        .await;
        let id = id_of(&v);
        // 他人（bob）拿真 id 也只会得到 not found
        let v = run_as(
            &b,
            Some(json!({"id": "bob"})),
            &format!(r#"db.session.attach("{id}").then(() => json.ok({{}})).catch((e) => json.ok({{ msg: String(e) }}))"#),
        )
        .await;
        let msg = v["data"]["msg"].as_str().unwrap().to_string();
        assert!(msg.contains("not found"), "{msg}");
        assert!(
            !msg.contains(&id),
            "must not echo the full session id: {msg}"
        );

        // 伪造的合法形状 id：同一文案（不泄露存在性）
        let fake = format!("tx_0000_{}", "0".repeat(32));
        let v = run_as(
            &b,
            Some(json!({"id": "alice"})),
            &format!(r#"db.session.attach("{fake}").then(() => json.ok({{}})).catch((e) => json.ok({{ msg: String(e) }}))"#),
        )
        .await;
        assert!(
            v["data"]["msg"].as_str().unwrap().contains("not found"),
            "{v}"
        );
        let _ = run_as(&b, Some(json!({"id": "alice"})), &format!(r#"db.session.rollback("{id}").then(() => json.ok({{}})).catch(() => json.ok({{}}))"#)).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn capacity_limits_refuse_new_sessions() {
        let b = bridge_t("cap", cfg(|c| c.max_sessions = 1)).await;
        let v = run(
            &b,
            r#"db.session.begin()
                 .then((s) => db.session.begin().then(() => json.ok({ leaked: s.id })).catch((e) => json.ok({ msg: String(e), id: s.id })))
                 .catch((e) => json.fail(500, String(e)))"#,
        )
        .await;
        assert!(
            v["data"]["msg"].as_str().unwrap().contains("limit reached"),
            "{v}"
        );
        let id = v["data"]["id"].as_str().unwrap().to_string();
        let _ = run(&b, &format!(r#"db.session.rollback("{id}").then(() => json.ok({{}})).catch(() => json.ok({{}}))"#)).await;
    }

    // ---------------------------------------------------------------------
    // 会话内语句禁令（MySQL 隐式提交 → 假回滚）
    // ---------------------------------------------------------------------

    #[tokio::test(flavor = "current_thread")]
    async fn transaction_control_and_session_state_sql_are_rejected() {
        let b = bridge_t("forbid", cfg(|_| {})).await;
        let v = run(
            &b,
            r#"db.session.begin().then(async (s) => {
                 const errs = [];
                 for (const sql of ["BEGIN", "COMMIT", "ROLLBACK", "SAVEPOINT sp1", "SET autocommit=1", "USE other", "PRAGMA foreign_keys=1"]) {
                   try { await db.session.exec(s.id, sql); errs.push("LEAK:" + sql); }
                   catch (e) { errs.push(String(e)); }
                 }
                 return db.session.rollback(s.id).then(() => json.ok({ errs }));
               }).catch((e) => json.fail(500, String(e)))"#,
        )
        .await;
        let errs = v["data"]["errs"].as_array().unwrap();
        assert_eq!(errs.len(), 7, "{v}");
        for e in errs {
            let s = e.as_str().unwrap();
            assert!(
                s.contains("not allowed inside a cross-request transaction"),
                "{s}"
            );
        }
        // 会话内正常语句不受影响
        let v = run(
            &b,
            r#"db.session.begin()
                 .then((s) => db.session.exec(s.id, "insert into t (v) values (?)", ["ok"]).then(() => db.session.rollback(s.id)))
                 .then(() => json.ok({}))
                 .catch((e) => json.fail(500, String(e)))"#,
        )
        .await;
        assert_eq!(v["code"], 0, "{v}");
    }

    // ---------------------------------------------------------------------
    // 与 db.tx / 其他别名 / 动态别名的交互
    // ---------------------------------------------------------------------

    #[tokio::test(flavor = "current_thread")]
    async fn db_tx_and_sessions_are_mutually_exclusive() {
        let b = bridge_t("excl", cfg(|_| {})).await;
        let v = run(
            &b,
            r#"db.tx(async () => {
                 try { await db.session.begin(); return json.fail(500, "session.begin should fail inside db.tx"); }
                 catch (e) { return json.ok({ msg: String(e) }); }
               })"#,
        )
        .await;
        assert!(v["data"]["msg"].as_str().unwrap().contains("db.tx"), "{v}");

        // 会话**已绑定**时 db.tx 必须被拒（未绑定时不冲突：那是另一条连接上的另一件事；
        // 注意 sqlite 池只有 1 条连接，未绑定会话 + db.tx 会互相等连接，不在本用例范围）。
        let v = run(
            &b,
            r#"db.session.begin().then(async (s) => {
                 await db.session.attach(s.id);
                 try { await db.tx(async () => {}); return json.fail(500, "db.tx should fail inside a session"); }
                 catch (e) { const m = String(e); await db.session.rollback(s.id); return json.ok({ msg: m }); }
               })"#,
        )
        .await;
        assert!(
            v["data"]["msg"]
                .as_str()
                .unwrap()
                .contains("session is attached"),
            "{v}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn attached_session_refuses_queries_on_another_alias() {
        let a = sqlite("alias-a").await;
        let o = sqlite("alias-o").await;
        o.exec_with_params("create table t (id integer primary key, v text)", &[])
            .await
            .unwrap();
        let b = bridge(vec![("default", a), ("other", o)], cfg(|_| {}));
        let v = run(
            &b,
            r#"DB("other").session.begin()
                 .then((s) => db.session.attach(s.id).then(() => db.query("select 1")))
                 .then(() => json.fail(500, "cross-alias query should fail"))
                 .catch((e) => json.ok({ msg: String(e) }))"#,
        )
        .await;
        let msg = v["data"]["msg"].as_str().unwrap();
        assert!(msg.contains("attached to db 'other'"), "{msg}");
        assert!(msg.contains("not 'default'"), "{msg}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn discarding_an_alias_kills_its_sessions() {
        let b = bridge_t("discard", cfg(|_| {})).await;
        let v = run(
            &b,
            r#"db.session.begin().then((s) => json.ok(s)).catch((e) => json.fail(500, String(e)))"#,
        )
        .await;
        let id = id_of(&v);
        // 模拟 client_db delete / reRegister 的钩子
        let n = b.stable_for_test().tx_sessions.discard_alias("default");
        assert_eq!(n, 1);
        let v = run(
            &b,
            &format!(r#"db.session.attach("{id}").then(() => json.ok({{}})).catch((e) => json.ok({{ msg: String(e) }}))"#),
        )
        .await;
        let msg = v["data"]["msg"].as_str().unwrap();
        assert!(msg.contains("discarded"), "{msg}");
        assert_eq!(count_via_pool(&b).await, 0);
    }

    // ---------------------------------------------------------------------
    // 过期回收 / 幂等收尾 / 观察面
    // ---------------------------------------------------------------------

    #[tokio::test(flavor = "current_thread")]
    async fn idle_sessions_expire_and_are_rolled_back() {
        let b = bridge_t(
            "idle",
            cfg(|c| {
                c.idle_timeout_secs = 1;
                c.max_lifetime_secs = 30;
            }),
        )
        .await;
        let v = run(
            &b,
            r#"db.session.begin()
                 .then((s) => db.session.exec(s.id, "insert into t (v) values (?)", ["a"]).then(() => json.ok(s)))
                 .catch((e) => json.fail(500, String(e)))"#,
        )
        .await;
        let id = id_of(&v);
        // 空闲超时（有界等待：回收器 tick 上限 1s，这里只等「过期事实」，不等具体文案）
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            let v = run(
                &b,
                &format!(r#"db.session.info("{id}").then(() => json.ok({{ alive: true }})).catch((e) => json.ok({{ alive: false, msg: String(e) }}))"#),
            )
            .await;
            if v["data"]["alive"] == false {
                let msg = v["data"]["msg"].as_str().unwrap();
                assert!(
                    msg.contains("expired") || msg.contains("not found"),
                    "{msg}"
                );
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "session never expired (deadline hit)"
            );
        }
        // 超时即回滚：池上查不到那行
        assert_eq!(count_via_pool(&b).await, 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn close_is_idempotent_and_stats_track_lifecycle() {
        let b = bridge_t("close", cfg(|_| {})).await;
        let v = run(
            &b,
            r#"db.session.begin().then((s) => db.session.stats().then((st1) => db.session.close(s.id).then((a) => db.session.close(s.id).then((b2) => json.ok({ id: s.id, st1, a, b: b2 }))))).catch((e) => json.fail(500, String(e)))"#,
        )
        .await;
        assert_eq!(v["data"]["st1"]["active"], 1, "{v}");
        assert_eq!(v["data"]["st1"]["enabled"], true, "{v}");
        assert_eq!(v["data"]["a"], true);
        assert_eq!(v["data"]["b"], true, "close must be idempotent");
        let v = run(&b, r#"db.session.list().then((l) => db.session.stats().then((s) => json.ok({ n: l.length, active: s.active, discarded: s.discarded })))"#).await;
        assert_eq!(v["data"]["n"], 0, "{v}");
        assert_eq!(v["data"]["active"], 0, "{v}");
        assert!(v["data"]["discarded"].as_u64().unwrap() >= 1, "{v}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn touch_reports_remaining_lifetime() {
        let b = bridge_t("touch", cfg(|_| {})).await;
        let v = run(
            &b,
            r#"db.session.begin().then((s) => db.session.touch(s.id).then((t) => db.session.rollback(s.id).then(() => json.ok(t)))).catch((e) => json.fail(500, String(e)))"#,
        )
        .await;
        let ms = v["data"]["expiresInMs"].as_u64().unwrap();
        // 默认 idle 300s / lifetime 1800s → 剩余寿命恒 > 0 且不超过最长寿命
        assert!(ms > 0 && ms <= 1_800_000, "{v}");
    }

    // InMemoryAccessor 存在性：保证夹具与生产同契约（不参与断言，仅编译期钉住可替换性）
    #[allow(dead_code)]
    fn _in_memory_is_a_data_accessor() {
        let _: Arc<dyn DataAccessor> = Arc::new(InMemoryAccessor::new());
    }

    /// G9：只读会话位——attach 路径与显式 op 双入口都拦 Write；SELECT 照常；
    /// 错误信封 `sessionDisposition=keep`（会话不被判死）；summary 透出 readOnly 位。
    #[tokio::test(flavor = "current_thread")]
    async fn readonly_session_blocks_writes_at_op_layer() {
        let b = bridge_t("g9ro", cfg(|_| {})).await;
        let v = run(
            &b,
            r#"(async () => {
              const s = await db.session.begin({ readOnly: true });
              await db.session.attach(s.id);
              let err = null, disp = null;
              try { await db.exec("insert into t (v) values ('x')"); }
              catch (e) { err = String(e); disp = e.sessionDisposition; }
              let err2 = null;
              try { await db.session.exec(s.id, "insert into t (v) values ('y')"); }
              catch (e) { err2 = String(e); }
              const c = await db.query("select count(*) c from t");
              const info = await db.session.info(s.id);
              return json.ok({ err, disp, err2, count: c[0].c, ro: info.readOnly });
            })().catch((e) => json.fail(500, String(e)))"#,
        )
        .await;
        let err = v["data"]["err"]
            .as_str()
            .unwrap_or_else(|| panic!("no err: {v}"));
        assert!(err.contains("is read-only"), "{err}");
        assert_eq!(v["data"]["disp"], json!("keep"), "{v}");
        let err2 = v["data"]["err2"].as_str().unwrap_or_default();
        assert!(err2.contains("is read-only"), "{err2}");
        assert_eq!(v["data"]["count"], json!(0), "两条写都必须被拒：{v}");
        assert_eq!(v["data"]["ro"], json!(true), "{v}");
    }

    /// G9 回归锚：缺省（不给 readOnly）会话写照常——位缺省 false 不改既有行为。
    #[tokio::test(flavor = "current_thread")]
    async fn writable_session_default_still_writes() {
        let b = bridge_t("g9rw", cfg(|_| {})).await;
        let v = run(
            &b,
            r#"(async () => {
              const s = await db.session.begin();
              if (s.readOnly !== false) return json.fail(500, "readOnly must default false");
              await db.session.attach(s.id);
              await db.exec("insert into t (v) values ('ok')");
              const c = await db.query("select count(*) c from t");
              return json.ok({ count: c[0].c });
            })().catch((e) => json.fail(500, String(e)))"#,
        )
        .await;
        assert_eq!(v["data"]["count"], json!(1), "{v}");
    }
}
