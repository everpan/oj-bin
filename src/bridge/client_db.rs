//! L3 动态数据源登记（client 轴运行时登记，v0.1.91）。
//!
//! 运行期把任意 DSN（mysql/postgres/sqlite，经与装配期同集的 [`DbBackendRegistry`]——
//! 含插件注册的 mysql/postgres 后端）登记为命名数据源。登记后别名直接进入 `DB(name)`
//! 解析路径，复用 L1 访问控制（v0.1.89）+ L2 结构内省（v0.1.90）+ 全部
//! query/exec/stream op 家族——**不另起平行的 `db.client.query` API**。
//!
//! # 不可变红线
//!
//! `StableState` 经 runtime 池共享后不可变。`client_dbs` 用 `Arc<ClientDbRegistry>`：
//! 壳 `Arc` 构造期冻结、内部 `Mutex<HashMap>` 运行期变动，与 `reload: Arc<ReloadSlot>`
//! 同哲学，不触 `Arc::get_mut`。
//!
//! # 授权两面性（安全红线）
//!
//! 1. **登记门**（`db_client:` 配置段 `allow`）：谁能调 register/test/delete/reRegister/list。
//!    `enabled` 默认 false（fail-closed）。`asSystem()` **不豁免**（复用 `granted`，其不读
//!    `ReqState.system`）。
//! 2. **库级访问**：登记名落入 `DB(name)` 后自然吃 L1 `db_access`。未显式声明的动态库
//!    默认 `Enforce::Deny`（`db_policy::policy_for`），杜绝「登记即全员可查内网库」。
//!
//! 见 `docs/plans/2026-10-28-db-client-axis-l3.md` §10（四路评审裁决）。

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use deno_core::{OpState, op2};
use deno_error::JsErrorBox;
use serde_json::Value;

use super::StableState;
use super::audit;
use super::db::DataAccessor;
use super::db_policy::{self, AllowList, DbAccess, DbClientSection, Enforce};
use super::handle_core::Owner;

/// 已登记动态数据源的逐别名记账条目。
pub struct ClientDbEntry {
    pub accessor: Arc<dyn DataAccessor>,
    /// 脱敏 DSN（`crate::secret::redact`，`user:pass@` → `***`，host/库名保留供排障）。
    pub dsn_redacted: String,
    /// 连接引擎名（`accessor.dialect_info().name`）。
    pub dialect: String,
    /// 登记操作者（`db_policy::operator_of` 取 `user.id`；无身份 None）。
    pub registered_by: Option<String>,
    pub registered_at_ms: u64,
    /// 命中计数（query/exec/stream/内省 经 `lookup` 自动累加）。
    pub queries: AtomicU64,
    pub last_error: Mutex<Option<String>>,
}

impl ClientDbEntry {
    fn summary(&self, alias: &str) -> Value {
        serde_json::json!({
            "alias": alias,
            "dialect": self.dialect,
            "dsn": self.dsn_redacted,
            "registeredBy": self.registered_by,
            "registeredAtMs": self.registered_at_ms,
            "queries": self.queries.load(Ordering::Relaxed),
        })
    }
}

/// 运行期动态数据源注册表（壳 `Arc` 不可变，内部 `Mutex` 变动）。
pub struct ClientDbRegistry {
    inner: Mutex<HashMap<String, Arc<ClientDbEntry>>>,
    order: Mutex<Vec<String>>,
    /// U-3 v0.1.93：逐别名世代号。register/re_register/remove 各 +1，作为跨 isolate
    /// 失效单源（host 权威；JS 仅借 epoch 优化 wrapper 新鲜度，正确性不依赖 JS 持有的 epoch）。
    epoch: Mutex<HashMap<String, u64>>,
    /// per-process 硬上限（连接池数封顶）。
    max_entries: usize,
    /// U-7 v0.1.97：per-owner（registered_by）登记上限。
    max_per_owner: usize,
    /// U-7 v0.1.97：动态库**全局**并发流预算。permit 由 pump task 持有，随流结束
    /// （自然/abort/close 任一路）释放；满了 `try_acquire` 即拒（不排队——排队会把
    /// 预算变成隐形队列，背压语义应上抛给调用方）。
    streams: Arc<tokio::sync::Semaphore>,
    /// U-7 v0.1.97：拨号连续失败计数（防 `db.client.test/register` 沦为内网端口扫描）。
    /// owner → (连续失败次数, 锁退截止时刻)。
    scan: Mutex<HashMap<String, (u32, std::time::Instant)>>,
}

/// 拨号超时（`db.client.test`/`register`/`reRegister` 的 connect 包一层；scan-protection 的一部分）。
const DIAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// 连续 connect 失败阈值：达到即锁退（扫描器打内网全是不通端口，失败会快速累积）。
const SCAN_FAIL_THRESHOLD: u32 = 8;
/// 锁退时长（锁退期内一律在**拨号之前**拒绝，不给探测流量出网）。
const SCAN_LOCKOUT: std::time::Duration = std::time::Duration::from_secs(60);

impl ClientDbRegistry {
    pub fn new(max_entries: usize) -> Self {
        Self::with_caps(max_entries, 8, 32)
    }

    pub fn with_caps(max_entries: usize, max_per_owner: usize, max_streams: usize) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            order: Mutex::new(Vec::new()),
            epoch: Mutex::new(HashMap::new()),
            max_entries,
            max_per_owner,
            streams: Arc::new(tokio::sync::Semaphore::new(max_streams)),
            scan: Mutex::new(HashMap::new()),
        }
    }

    pub fn cap(&self) -> usize {
        self.max_entries
    }

    /// U-7：动态库全局并发流预算——占一个名额（不排队）。Ok(permit) 为 owned，
    /// 随 pump task 结束自动释放。
    pub fn try_acquire_stream(&self) -> Result<tokio::sync::OwnedSemaphorePermit, &'static str> {
        Arc::clone(&self.streams)
            .try_acquire_owned()
            .map_err(|_| {
                "db client stream budget exhausted (db_client.max_streams); close running streams or raise the cap"
            })
    }

    /// U-7：拨号前检查锁退。Err(剩余锁退时长) = 拒拨。
    pub fn dial_allowed(&self, owner: &str) -> Result<(), std::time::Duration> {
        let g = self.scan.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, until)) = g.get(owner) {
            let left = until.saturating_duration_since(std::time::Instant::now());
            if left > std::time::Duration::ZERO {
                return Err(left);
            }
        }
        Ok(())
    }

    /// U-7：拨号后记账。成功清零；失败累加，达阈值即锁退（返回 true = 本次触发了锁退）。
    pub fn note_dial(&self, owner: &str, ok: bool) -> bool {
        let mut g = self.scan.lock().unwrap_or_else(|e| e.into_inner());
        if ok {
            g.remove(owner);
            return false;
        }
        let e = g
            .entry(owner.to_string())
            .or_insert((0, std::time::Instant::now()));
        e.0 += 1;
        if e.0 >= SCAN_FAIL_THRESHOLD {
            e.1 = std::time::Instant::now() + SCAN_LOCKOUT;
            return true;
        }
        false
    }

    /// U-3 v0.1.93：取别名的当前世代号（未登记 = 0）。host 权威，供 `op_db_epoch` 读取。
    pub fn epoch_of(&self, name: &str) -> u64 {
        self.epoch
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .copied()
            .unwrap_or(0)
    }

    /// 世代号 +1（register/re_register/remove 共用）。
    fn bump_epoch(&self, name: &str) {
        let mut g = self.epoch.lock().unwrap_or_else(|e| e.into_inner());
        *g.entry(name.to_string()).or_insert(0) += 1;
    }

    pub fn contains(&self, name: &str) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(name)
    }

    /// 登记（重名 / 超配额 / 超 per-owner 上限 → Err）。锁前已完成 connect（异步），此处仅短暂加锁。
    pub fn register(&self, name: &str, entry: ClientDbEntry) -> Result<(), String> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if g.contains_key(name) {
            return Err(format!("client db '{name}' already registered"));
        }
        if g.len() >= self.max_entries {
            return Err(format!(
                "client db registry full (cap {})",
                self.max_entries
            ));
        }
        // U-7：per-owner 上限（同一 registered_by 最多 max_per_owner 个）。
        let owned = g
            .values()
            .filter(|e| e.registered_by == entry.registered_by)
            .count();
        if owned >= self.max_per_owner {
            return Err(format!(
                "client db registry per-owner cap {} reached (owner {:?}); delete an alias before registering another",
                self.max_per_owner,
                entry.registered_by.as_deref().unwrap_or("-")
            ));
        }
        g.insert(name.to_string(), Arc::new(entry));
        drop(g);
        self.order
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(name.to_string());
        self.bump_epoch(name);
        Ok(())
    }

    /// 更新已登记连接（要求已存在；不存在 → Err）。原子替换（持锁区间）。
    pub fn re_register(&self, name: &str, entry: ClientDbEntry) -> Result<(), String> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if !g.contains_key(name) {
            return Err(format!(
                "client db '{name}' not registered (use register to add)"
            ));
        }
        g.insert(name.to_string(), Arc::new(entry));
        self.bump_epoch(name);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<Arc<ClientDbEntry>> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .cloned()
    }

    /// `lookup` 用：取 accessor 并累加命中计数。
    pub fn accessor_and_count(&self, name: &str) -> Option<Arc<dyn DataAccessor>> {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.get(name).map(|e| {
            e.queries.fetch_add(1, Ordering::Relaxed);
            e.accessor.clone()
        })
    }

    pub fn remove(&self, name: &str) -> bool {
        let removed = self
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name)
            .is_some();
        if removed {
            self.order
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|n| n != name);
            self.bump_epoch(name);
        }
        removed
    }

    pub fn names(&self) -> Vec<String> {
        self.order.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 别名校验：非空、≤128、无 NUL/控制字符、非 `default`（字面 default 是 static 别名）。
fn valid_client_alias(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.bytes().any(|b| b == 0 || b < 0x20)
        && name != "default"
}

/// L1 风格的结构化错误：原因串 + `reason:` 前缀（不回显授权名单）。
fn client_err(reason: &str, detail: &str) -> JsErrorBox {
    JsErrorBox::generic(format!("{detail}\n  (reason: {reason})"))
}

/// 登记门：enforce=false → disabled；否则按 `allow` 四维并集判定（asSystem 不豁免）。
/// `pub(crate)`：U-1 的 `db.access.grant/revoke` op（db.rs）复用同一特权门。
pub(crate) fn client_gate(
    state: &Rc<RefCell<OpState>>,
    section: &DbClientSection,
) -> Result<(), JsErrorBox> {
    if !section.enabled {
        return Err(client_err(
            "db_client_disabled",
            "db client registration is disabled (db_client.enabled=false); ask admin to enable it",
        ));
    }
    let (module, roles, tenant, source) = db_policy::identity(state);
    let ctx = db_policy::AccessCtx {
        module: module.as_deref(),
        roles: roles.clone(),
        tenant: tenant.as_deref(),
        source,
    };
    if !db_policy::granted(&section.allow, &ctx) {
        return Err(client_err(
            "db_client_not_authorized",
            &format!(
                "db client registration denied for caller (module={module:?}, roles={roles:?}, tenant={tenant:?}, source={source}); ask admin to add this caller to db_client.allow"
            ),
        ));
    }
    Ok(())
}

/// 审计事件上下文（U-9 归一）：owner（tenant/user/module/source）+ 审计存储。
/// DB_CLIENT_AUDIT / DB_ACCESS_AUDIT / DB_CLIENT_DENY / DB_DANGEROUS_CONFIRM 四族共用，
/// 别再散点 `eprintln!`。
pub(crate) fn audit_ctx(state: &Rc<RefCell<OpState>>) -> (Owner, Arc<StableState>) {
    let g = state.borrow();
    let st = g.borrow::<Arc<StableState>>().clone();
    let rs = g.borrow::<super::ReqState>();
    let source = if st.tasks_flag.is_some() {
        "task"
    } else {
        "http"
    };
    let owner = Owner {
        tenant: if rs.req.anonymous {
            None
        } else {
            rs.req.tenant_id.clone()
        },
        user: db_policy::operator_of(rs.req.user.as_ref()),
        module: rs.module.clone(),
        source,
    };
    (owner, st)
}

/// 审计日志（结构化；非防篡改，但比 deny 更适合安全敏感事件）。U-9：经统一出口。
fn audit(state: &Rc<RefCell<OpState>>, action: &str, alias: &str, dialect: &str) {
    let (owner, st) = audit_ctx(state);
    audit::emit(
        &st.audit,
        "DB_CLIENT_AUDIT",
        &owner,
        alias,
        &format!("action={action} dialect={dialect}"),
    );
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// U-7：拨号操作的主体键（与 `ClientDbEntry.registered_by` 同源；无身份记 `"-"`）。
fn dial_owner(state: &Rc<RefCell<OpState>>) -> String {
    let g = state.borrow();
    db_policy::operator_of(g.borrow::<super::ReqState>().req.user.as_ref())
        .unwrap_or_else(|| "-".to_string())
}

/// U-7：配额/预算拒绝的结构化日志。U-9：经统一出口（owner 从请求推导，`owner` 串进 detail）。
pub(crate) fn deny_log(
    state: &Rc<RefCell<OpState>>,
    action: &str,
    reason: &str,
    owner: &str,
    alias: &str,
) {
    let (o, st) = audit_ctx(state);
    audit::emit(
        &st.audit,
        "DB_CLIENT_DENY",
        &o,
        alias,
        &format!("action={action} reason={reason} caller={owner}"),
    );
}

/// U-7：拨号守卫——锁退检查 → 5s 超时 connect → 记账（成功清零 / 失败累加，达阈值锁退）。
/// test/register/reRegister 三个会拨号的 op 共用（扫描面一致）。
async fn guarded_connect(
    st: &Arc<StableState>,
    dsn: &str,
    owner: &str,
) -> Result<Arc<dyn DataAccessor>, String> {
    if let Err(left) = st.client_dbs.dial_allowed(owner) {
        return Err(format!(
            "db client dial blocked: too many consecutive connect failures from this caller; retry in {}s",
            left.as_secs().max(1)
        ));
    }
    let res = tokio::time::timeout(DIAL_TIMEOUT, st.db_backend.connect(dsn, &st.config_dir)).await;
    match res {
        Err(_) => {
            let locked = st.client_dbs.note_dial(owner, false);
            Err(format!(
                "db client connect timed out after {}s{}",
                DIAL_TIMEOUT.as_secs(),
                if locked {
                    "; further dials are temporarily blocked (too many consecutive failures)"
                } else {
                    ""
                }
            ))
        }
        Ok(Err(e)) => {
            let locked = st.client_dbs.note_dial(owner, false);
            Err(format!(
                "db client connect failed: {e}{}",
                if locked {
                    "; further dials are temporarily blocked (too many consecutive failures)"
                } else {
                    ""
                }
            ))
        }
        Ok(Ok(a)) => {
            st.client_dbs.note_dial(owner, true);
            Ok(a)
        }
    }
}

#[op2]
#[serde]
pub async fn op_db_client_register(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[string] dsn: String,
) -> Result<serde_json::Value, JsErrorBox> {
    let section = state
        .borrow()
        .borrow::<Arc<StableState>>()
        .db_client
        .clone();
    client_gate(&state, &section)?;
    if !valid_client_alias(&name) {
        return Err(JsErrorBox::generic(format!(
            "invalid client db alias {name:?}: must be 1..=128 chars, contain no NUL/control bytes, and not be 'default'"
        )));
    }
    let st = state.borrow().borrow::<Arc<StableState>>().clone();
    if st.dbs.contains_key(&name) {
        return Err(JsErrorBox::generic(format!(
            "client db alias {name:?} conflicts with a statically configured db"
        )));
    }
    // connect 是异步且会开真实连接池——锁前完成，绝不持锁跨 await。
    let owner = dial_owner(&state);
    let accessor = guarded_connect(&st, &dsn, &owner).await.map_err(|e| {
        deny_log(&state, "register", "dial_rejected", &owner, &name);
        JsErrorBox::generic(e)
    })?;
    let dialect = accessor.dialect_info().name.clone();
    let dsn_redacted = crate::secret::redact(&dsn);
    let registered_by =
        db_policy::operator_of(state.borrow().borrow::<super::ReqState>().req.user.as_ref());
    let entry = ClientDbEntry {
        accessor,
        dsn_redacted,
        dialect: dialect.clone(),
        registered_by,
        registered_at_ms: now_ms(),
        queries: AtomicU64::new(0),
        last_error: Mutex::new(None),
    };
    // 持锁区间仅做插入（重名/超配额在锁内重判，防竞态）。
    st.client_dbs
        .register(&name, entry)
        .map_err(JsErrorBox::generic)?;
    audit(&state, "register", &name, &dialect);
    let e = st.client_dbs.get(&name).unwrap();
    Ok(e.summary(&name))
}

#[op2]
#[serde]
pub async fn op_db_client_test(
    state: Rc<RefCell<OpState>>,
    #[string] dsn: String,
) -> Result<serde_json::Value, JsErrorBox> {
    let section = state
        .borrow()
        .borrow::<Arc<StableState>>()
        .db_client
        .clone();
    client_gate(&state, &section)?;
    let st = state.borrow().borrow::<Arc<StableState>>().clone();
    let owner = dial_owner(&state);
    let accessor = match guarded_connect(&st, &dsn, &owner).await {
        Ok(a) => a,
        Err(e) => {
            deny_log(&state, "test", "dial_rejected", &owner, "-");
            return Ok(serde_json::json!({ "ok": false, "error": e }));
        }
    };
    let dialect = accessor.dialect_info().name.clone();
    match accessor.query_with_params("select 1", &[]).await {
        Ok(_) => Ok(serde_json::json!({ "ok": true, "dialect": dialect })),
        Err(e) => {
            Ok(serde_json::json!({ "ok": false, "dialect": dialect, "error": e.to_string() }))
        }
    }
}

#[op2]
pub async fn op_db_client_delete(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
) -> Result<bool, JsErrorBox> {
    let section = state
        .borrow()
        .borrow::<Arc<StableState>>()
        .db_client
        .clone();
    client_gate(&state, &section)?;
    let st = state.borrow().borrow::<Arc<StableState>>().clone();
    let removed = st.client_dbs.remove(&name);
    if removed {
        // 跨请求事务会话（L7）：别名已消失，其上的会话必须当场判死（回滚在后台/下次
        // 触碰时完成）——绝不允许继续在一条「已注销」的连接上写。
        st.tx_sessions.discard_alias(&name);
        audit(&state, "delete", &name, "");
    }
    Ok(removed)
}

#[op2]
#[serde]
pub async fn op_db_client_re_register(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[string] dsn: String,
) -> Result<serde_json::Value, JsErrorBox> {
    let section = state
        .borrow()
        .borrow::<Arc<StableState>>()
        .db_client
        .clone();
    client_gate(&state, &section)?;
    if !valid_client_alias(&name) {
        return Err(JsErrorBox::generic(format!(
            "invalid client db alias {name:?}"
        )));
    }
    let st = state.borrow().borrow::<Arc<StableState>>().clone();
    if !st.client_dbs.contains(&name) {
        return Err(JsErrorBox::generic(format!(
            "client db '{name}' not registered (use register to add)"
        )));
    }
    let owner = dial_owner(&state);
    let accessor = guarded_connect(&st, &dsn, &owner).await.map_err(|e| {
        deny_log(&state, "reRegister", "dial_rejected", &owner, &name);
        JsErrorBox::generic(e)
    })?;
    let dialect = accessor.dialect_info().name.clone();
    let dsn_redacted = crate::secret::redact(&dsn);
    let registered_by =
        db_policy::operator_of(state.borrow().borrow::<super::ReqState>().req.user.as_ref());
    let entry = ClientDbEntry {
        accessor,
        dsn_redacted,
        dialect: dialect.clone(),
        registered_by,
        registered_at_ms: now_ms(),
        queries: AtomicU64::new(0),
        last_error: Mutex::new(None),
    };
    st.client_dbs
        .re_register(&name, entry)
        .map_err(JsErrorBox::generic)?;
    // 跨请求事务会话（L7）：连接已被换掉，旧连接上的在写事务必须失效（否则「提交了
    // 但写的是旧库」）。登记成功后再判死，避免 connect 失败白白回滚用户会话。
    st.tx_sessions.discard_alias(&name);
    audit(&state, "reRegister", &name, &dialect);
    let e = st.client_dbs.get(&name).unwrap();
    Ok(e.summary(&name))
}

#[op2]
#[serde]
pub fn op_db_client_list(
    state: Rc<RefCell<OpState>>,
) -> Result<Vec<serde_json::Value>, JsErrorBox> {
    let section = state
        .borrow()
        .borrow::<Arc<StableState>>()
        .db_client
        .clone();
    client_gate(&state, &section)?;
    let st = state.borrow().borrow::<Arc<StableState>>().clone();
    let mut out: Vec<Value> = Vec::new();
    for name in st.client_dbs.names() {
        if let Some(e) = st.client_dbs.get(&name) {
            out.push(e.summary(&name));
        }
    }
    Ok(out)
}

/// 探活用：不经登记门（DB() 构造器需据此返回 undefined）。
#[op2(fast)]
pub fn op_db_client_has(state: &mut OpState, #[string] name: &str) -> bool {
    state.borrow::<Arc<StableState>>().client_dbs.contains(name)
}

/// U-3 v0.1.93：取别名的当前世代号（host 权威）。同步读 map，无特权。JS 侧 `DB(name)`
/// 借 epoch 判断 wrapper 是否过期——但**正确性不依赖 JS 持有的 epoch**（查询 op 按名从
/// host registry 重解析 accessor），恶意 handler 覆写 `cached.epoch` 也无法复用已删连接。
#[op2]
#[serde]
pub fn op_db_epoch(state: &mut OpState, #[string] name: &str) -> u64 {
    state.borrow::<Arc<StableState>>().client_dbs.epoch_of(name)
}

/// U-1 v0.1.93：动态授权审计（安全敏感度高于 register）。U-9：经统一出口。
fn audit_access(state: &Rc<RefCell<OpState>>, action: &str, alias: &str, allow: &AllowList) {
    let (owner, st) = audit_ctx(state);
    audit::emit(
        &st.audit,
        "DB_ACCESS_AUDIT",
        &owner,
        alias,
        &format!("action={action} allow={allow:?}"),
    );
}

/// U-1 v0.1.93：运行期动态授权 `grant`。命名归 `db.access.grant`（访问控制域，非
/// `db.client` 登记域）。与 `db.client.register` **同特权门**（`db_client.allow` 四维并集、
/// fail-closed），且必须发 `audit()`。
///
/// 语义：把 `alias` 的访问**放行维**（allow：角色/主体四维并集）注入运行表，使被授权主体
/// 无需重启即对该库可读（默认 read-only）。撤销见 [`op_db_access_revoke`]。
///
/// 安全约束：
/// - `allow` 必须非空（任一维已声明）；空 allow 视为「对全员放行」→ 拒（对抗用例）。
/// - 只注入放行维，绝不携带 `enforce=Off` 覆盖 config 的 Deny/Read-only/max_rows（防提权）。
/// - 撤销他人的 config 兜底（无运行期项）显式报错；`revoke` 仅回落 config（非删 config）。
#[op2]
#[serde]
pub async fn op_db_access_grant(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[serde] allow: serde_json::Value,
    #[serde] opts: Option<serde_json::Value>,
) -> Result<serde_json::Value, JsErrorBox> {
    let section = state
        .borrow()
        .borrow::<Arc<StableState>>()
        .db_client
        .clone();
    client_gate(&state, &section)?;
    let st = state.borrow().borrow::<Arc<StableState>>().clone();
    // 别名必须存在（静态 config 库或动态登记库）；不允许对不存在的库授权。
    if !st.dbs.contains_key(&name) && !st.client_dbs.contains(&name) {
        return Err(JsErrorBox::generic(format!(
            "db access grant: alias {name:?} does not exist (configure it under db: or register via db.client first)"
        )));
    }
    let allow_list: AllowList = serde_json::from_value(allow.clone())
        .map_err(|e| JsErrorBox::generic(format!("db access grant: invalid allow: {e}")))?;
    // 空 allow = 对全员放行，运行时授权严禁（违背「按角色授权」）→ 拒。
    if allow_list.is_unrestricted() {
        return Err(JsErrorBox::generic(
            "db access grant: allow must restrict at least one dimension (modules/roles/tenants/sources); granting to everyone is forbidden",
        ));
    }
    // read_only 默认 true（安全默认：运行期 grant 即读授权）；仅动态别名生效，静态别名
    // 忽略（config 兜底的安全位不可被运行期项放通）。
    let read_only = opts
        .as_ref()
        .and_then(|o| o.get("readOnly"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let entry = DbAccess {
        // v0.1.93：grant = 白名单制（`deny` 档命中 allow 才放行）。用 `warn` 会变成
        // 「只记日志不拒」，未授权主体照样能查——那不是授权。
        enforce: Enforce::Deny,
        read_only,
        allow_ddl: false,
        allow_dangerous: false,
        max_rows: None,
        // G9：grant 只注入放行维；确认 token 是 config 安全位，不随 grant 注入。
        dangerous_confirm_token: None,
        allow: allow_list.clone(),
    };
    st.db_access_rt
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(name.clone(), entry);
    audit_access(&state, "grant", &name, &allow_list);
    Ok(serde_json::json!({
        "ok": true,
        "alias": name,
        "readOnly": read_only,
        "allow": allow,
    }))
}

/// U-1 v0.1.93：运行期动态授权 `revoke`。与 `grant` 同特权门、同审计。
///
/// 只接受**运行期表项**；若别名仅有 config 兜底（无运行期 grant）则显式报错（不静默），
/// 防「任何过 allow 的主体撤销他人运行时授权」的越权 / DoS。revoke 后该别名回落 config
/// （非删 config）。
#[op2]
pub async fn op_db_access_revoke(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
) -> Result<bool, JsErrorBox> {
    let section = state
        .borrow()
        .borrow::<Arc<StableState>>()
        .db_client
        .clone();
    client_gate(&state, &section)?;
    let st = state.borrow().borrow::<Arc<StableState>>().clone();
    let (removed, removed_allow) = {
        let mut g = st.db_access_rt.lock().unwrap_or_else(|e| e.into_inner());
        match g.remove(&name) {
            Some(entry) => (true, entry.allow),
            None => (false, AllowList::default()),
        }
    };
    if !removed {
        return Err(JsErrorBox::generic(format!(
            "db access revoke: alias {name:?} has no runtime access grant (only config-backed policy exists; nothing to revoke, config unchanged)"
        )));
    }
    audit_access(&state, "revoke", &name, &removed_allow);
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_register_and_overwrite() {
        let r = ClientDbRegistry::new(2);
        let mk = || ClientDbEntry {
            accessor: Arc::new(crate::bridge::db::InMemoryAccessor::new()),
            dsn_redacted: "sqlite::memory:".into(),
            dialect: "sqlite".into(),
            registered_by: None,
            registered_at_ms: 0,
            queries: AtomicU64::new(0),
            last_error: Mutex::new(None),
        };
        r.register("a", mk()).unwrap();
        assert!(r.contains("a"));
        // 重名 → 拒
        assert!(r.register("a", mk()).is_err());
        // 超配额 → 拒
        r.register("b", mk()).unwrap();
        assert!(r.register("c", mk()).is_err(), "cap must block 3rd");
        // 删除释放名额
        assert!(r.remove("a"));
        r.register("c", mk()).unwrap();
        assert_eq!(r.names(), vec!["b".to_string(), "c".to_string()]);
        // accessor_and_count 累加
        let acc = r.accessor_and_count("b").unwrap();
        assert!(acc.dialect_info().name == "sqlite");
        assert_eq!(r.get("b").unwrap().queries.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn alias_validation() {
        assert!(valid_client_alias("sales_replica"));
        assert!(!valid_client_alias(""));
        assert!(!valid_client_alias("default"));
        assert!(!valid_client_alias(&format!("bad{u}name", u = '\u{0007}'))); // 控制字符
        assert!(!valid_client_alias(&format!("bad{u}name", u = '\u{0000}'))); // NUL
        assert!(!valid_client_alias(&"x".repeat(129)));
    }

    /// U-7：per-owner 登记上限——同 owner 第 N+1 个被拒且错误可辨识；异 owner 不受影响；
    /// delete 释放名额。
    #[test]
    fn per_owner_cap_blocks_n_plus_one() {
        let r = ClientDbRegistry::with_caps(8, 1, 8);
        let mk = |by: Option<&str>| ClientDbEntry {
            accessor: Arc::new(crate::bridge::db::InMemoryAccessor::new()),
            dsn_redacted: "sqlite::memory:".into(),
            dialect: "sqlite".into(),
            registered_by: by.map(|s| s.to_string()),
            registered_at_ms: 0,
            queries: AtomicU64::new(0),
            last_error: Mutex::new(None),
        };
        r.register("a", mk(Some("u"))).unwrap();
        let err = r.register("b", mk(Some("u"))).unwrap_err();
        assert!(err.contains("per-owner cap 1"), "{err}");
        // 异 owner / 匿名桶不受影响
        r.register("c", mk(Some("v"))).unwrap();
        r.register("d", mk(None)).unwrap();
        // delete 释放
        assert!(r.remove("a"));
        r.register("e", mk(Some("u"))).unwrap();
    }

    /// U-7：拨号锁退——连续失败达阈值即拒拨（异 owner 不受影响）；成功记账即清零。
    #[test]
    fn scan_lockout_after_consecutive_dial_failures() {
        let r = ClientDbRegistry::with_caps(8, 8, 8);
        assert!(r.dial_allowed("u").is_ok());
        for i in 0..SCAN_FAIL_THRESHOLD - 1 {
            assert!(!r.note_dial("u", false), "第 {} 次失败还不该锁", i + 1);
        }
        assert!(r.note_dial("u", false), "第 8 次失败应触发锁退");
        let left = r.dial_allowed("u").unwrap_err();
        assert!(left <= SCAN_LOCKOUT && left > std::time::Duration::ZERO);
        // 异 owner 照常
        assert!(r.dial_allowed("v").is_ok());
        // 成功记账清零（锁退态被显式成功解除——记账单一真源）
        r.note_dial("u", true);
        assert!(r.dial_allowed("u").is_ok());
    }

    /// U-7：全局流预算——占满后 try_acquire 拒绝且错误点名配置键。
    #[test]
    fn stream_budget_exhausts_distinctly() {
        let r = ClientDbRegistry::with_caps(8, 8, 2);
        let a = r.try_acquire_stream().unwrap();
        let b = r.try_acquire_stream().unwrap();
        let err = r.try_acquire_stream().unwrap_err();
        assert!(err.contains("db_client.max_streams"), "{err}");
        drop(a);
        let _ = r.try_acquire_stream().unwrap(); // 释放后可再占
        drop(b);
    }

    // 无关 `AllowList` 默认形态（确保引用不被误删）。
    #[test]
    fn allow_list_default_unrestricted() {
        assert!(db_policy::AllowList::default().is_unrestricted());
    }
}
