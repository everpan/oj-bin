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
use super::db::DataAccessor;
use super::db_policy::{self, DbClientSection};

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
    /// per-process 硬上限（连接池数封顶）。
    max_entries: usize,
}

impl ClientDbRegistry {
    pub fn new(max_entries: usize) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            order: Mutex::new(Vec::new()),
            max_entries,
        }
    }

    pub fn cap(&self) -> usize {
        self.max_entries
    }

    pub fn contains(&self, name: &str) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(name)
    }

    /// 登记（重名 / 超配额 → Err）。锁前已完成 connect（异步），此处仅短暂加锁。
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
        g.insert(name.to_string(), Arc::new(entry));
        drop(g);
        self.order
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(name.to_string());
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
fn client_gate(state: &Rc<RefCell<OpState>>, section: &DbClientSection) -> Result<(), JsErrorBox> {
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

/// 审计日志（结构化；非防篡改，但比 `deny_log` 更适合安全敏感事件）。
fn audit(state: &Rc<RefCell<OpState>>, action: &str, alias: &str, dialect: &str) {
    let g = state.borrow();
    let st = g.borrow::<Arc<StableState>>();
    let rs = g.borrow::<super::ReqState>();
    let module = rs.module.clone();
    let source = if st.tasks_flag.is_some() {
        "task"
    } else {
        "http"
    };
    let operator = db_policy::operator_of(rs.req.user.as_ref());
    let tenant = if rs.req.anonymous {
        None
    } else {
        rs.req.tenant_id.clone()
    };
    eprintln!(
        "DB_CLIENT_AUDIT: action={action} alias={alias} dialect={dialect} operator={operator:?} module={module:?} tenant={tenant:?} source={source}"
    );
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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
    let accessor = st
        .db_backend
        .connect(&dsn, &st.config_dir)
        .await
        .map_err(|e| JsErrorBox::generic(format!("db client connect failed: {e}")))?;
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
    let accessor = match st.db_backend.connect(&dsn, &st.config_dir).await {
        Ok(a) => a,
        Err(e) => {
            return Ok(serde_json::json!({ "ok": false, "error": format!("connect failed: {e}") }));
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
    let accessor = st
        .db_backend
        .connect(&dsn, &st.config_dir)
        .await
        .map_err(|e| JsErrorBox::generic(format!("db client connect failed: {e}")))?;
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

    // 无关 `AllowList` 默认形态（确保引用不被误删）。
    #[test]
    fn allow_list_default_unrestricted() {
        assert!(db_policy::AllowList::default().is_unrestricted());
    }
}
