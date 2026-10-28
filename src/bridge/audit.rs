//! U-9 安全审计归一（v0.1.98）。
//!
//! # 为什么
//!
//! 安全审计事件原本散点 stderr：`DB_CLIENT_AUDIT` / `DB_CLIENT_DENY` / `DB_ACCESS_AUDIT`
//! （client_db.rs）与 `DB_SESSION_*`（db_session.rs）各打各的格式，不落存储、不可检索。
//! 本模块给它们**一个事件类型、一个 emit 出口**：stderr 行归一为 `OJ_AUDIT: …`，
//! 同时入进程内有界环形存储，供 [`op_audit_query`] 按 kind/user/tenant/alias/时间检索
//! （检索即导出——handler 拿到 JSON 想怎么服务都行）。
//!
//! # 边界（与 PRD 的偏差，诚实登记）
//!
//! - `SqlEvent`（sql_trace）**不在**本轴：per-request dev 画像（全 SQL 入库不可控），
//!   与安全审计两回事；耗时/行数/成败继续走 `db.sqlProfile()`。
//! - 存储为**进程内**环形缓冲：留存储存按「条数 + 年龄」双限自动清理，**不跨重启**；
//!   跨重启持久化 / 防篡改需外部 sink（从本模块单一出口接，不散点）。
//!
//! # 零凭据红线
//!
//! 事件只允许 alias / dialect / 动作 / **前缀 id** / allow 维度——绝不放 DSN、密码、
//! 完整凭据 id（`handle_core::id_prefix` 负责遮蔽，构造方负责不喂明文）。

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use deno_core::{OpState, op2};
use deno_error::JsErrorBox;
use serde_json::{json, Value};

use super::db_policy::{self, AccessCtx, AuditSection};
use super::handle_core::Owner;
use super::{ReqState, StableState};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// 一条安全审计事件（零凭据；`kind` 即旧 stderr 前缀名，日志/检索两侧同源）。
#[derive(Debug, Clone)]
pub struct AuditEvent {
    pub ts_ms: u64,
    pub kind: String,
    pub user: Option<String>,
    pub tenant: Option<String>,
    pub module: Option<String>,
    pub source: &'static str,
    pub alias: String,
    pub detail: String,
}

impl AuditEvent {
    pub fn to_json(&self) -> Value {
        json!({
            "tsMs": self.ts_ms,
            "kind": self.kind,
            "user": self.user,
            "tenant": self.tenant,
            "module": self.module,
            "source": self.source,
            "alias": self.alias,
            "detail": self.detail,
        })
    }

    /// stderr 行（与 to_json 同源；控制字符剔除防日志伪造——与 db_session::safe 同策略）。
    fn line(&self) -> String {
        let f = |s: &str| -> String { s.chars().filter(|c| !c.is_control()).take(96).collect() };
        format!(
            "OJ_AUDIT: kind={} user={:?} tenant={:?} module={:?} source={} alias={} detail={:?}",
            f(&self.kind),
            self.user.as_deref().map(f),
            self.tenant.as_deref().map(f),
            self.module.as_deref().map(f),
            self.source,
            f(&self.alias),
            f(&self.detail),
        )
    }
}

/// 进程内审计存储：有界环形 + 年龄清理（push 与 query 双点清理，无后台 reaper）。
pub struct AuditStore {
    inner: Mutex<VecDeque<AuditEvent>>,
    max_entries: usize,
    retention: Duration,
    enabled: bool,
}

impl AuditStore {
    pub fn new(section: &AuditSection) -> Self {
        Self {
            inner: Mutex::new(VecDeque::new()),
            max_entries: section.max_entries,
            retention: Duration::from_secs(section.retention_secs),
            enabled: section.enabled,
        }
    }

    /// 入库 + 双限清理（超条数丢最旧、超年龄摘除）。disabled 时 no-op。
    pub fn push(&self, e: AuditEvent) {
        if !self.enabled {
            return;
        }
        let now = ms_now();
        let mut g = lock(&self.inner);
        g.push_back(e);
        while g.len() > self.max_entries {
            g.pop_front();
        }
        let cutoff = now.saturating_sub(self.retention.as_millis() as u64);
        while g.front().is_some_and(|f| f.ts_ms < cutoff) {
            g.pop_front();
        }
    }

    /// 检索（最新在前；过滤器各维独立生效）。顺带清超龄。
    pub fn query(
        &self,
        kind: Option<&str>,
        user: Option<&str>,
        tenant: Option<&str>,
        alias: Option<&str>,
        since_ms: Option<u64>,
        until_ms: Option<u64>,
        limit: usize,
    ) -> Vec<AuditEvent> {
        let now = ms_now();
        let mut g = lock(&self.inner);
        let cutoff = now.saturating_sub(self.retention.as_millis() as u64);
        while g.front().is_some_and(|f| f.ts_ms < cutoff) {
            g.pop_front();
        }
        let mut out: Vec<AuditEvent> = g
            .iter()
            .filter(|e| kind.is_none_or(|k| e.kind == k))
            .filter(|e| user.is_none_or(|u| e.user.as_deref() == Some(u)))
            .filter(|e| tenant.is_none_or(|t| e.tenant.as_deref() == Some(t)))
            .filter(|e| alias.is_none_or(|a| e.alias == a))
            .filter(|e| since_ms.is_none_or(|s| e.ts_ms >= s))
            .filter(|e| until_ms.is_none_or(|u| e.ts_ms <= u))
            .cloned()
            .collect();
        out.reverse(); // 最新在前
        out.truncate(limit);
        out
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        lock(&self.inner).len()
    }
}

fn ms_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 统一 emit：stderr 归一行 + 入库（store disabled 时只打日志）。
/// 所有安全审计事件（client/session/access 三族 + deny）必须走这里，别再散点 `eprintln!`。
pub fn emit(store: &AuditStore, kind: &str, owner: &Owner, alias: &str, detail: &str) {
    let e = AuditEvent {
        ts_ms: ms_now(),
        kind: kind.to_string(),
        user: owner.user.clone(),
        tenant: owner.tenant.clone(),
        module: owner.module.clone(),
        source: owner.source,
        alias: alias.to_string(),
        detail: detail.to_string(),
    };
    eprintln!("{}", e.line());
    store.push(e);
}

/// 检索门（`audit.allow` 四维并集，每 op 重判——与 client_gate 同形态）。
fn require_gate(state: &OpState) -> Result<(), JsErrorBox> {
    let st = state.borrow::<Arc<StableState>>();
    let section = &st.audit_section;
    if !section.enabled {
        return Err(JsErrorBox::generic(
            "audit is disabled (set audit.enabled: true to record and query audit events)",
        ));
    }
    let (module, roles, tenant, source) = db_policy::identity(state);
    let ctx = AccessCtx {
        module: module.as_deref(),
        roles,
        tenant: tenant.as_deref(),
        source,
    };
    if !db_policy::granted(&section.allow, &ctx) {
        return Err(JsErrorBox::generic(
            "audit query not granted to this caller; ask admin to add this caller to audit.allow",
        ));
    }
    Ok(())
}

/// audit.query（U-9）：按 kind/user/tenant/alias/时间窗检索审计事件（最新在前，即导出通道）。
/// 过滤器全缺 = 最近 `limit` 条；limit 缺省 100、硬顶 1000。
#[op2]
#[serde]
pub fn op_audit_query(
    state: &mut OpState,
    #[serde] filter: Option<Value>,
) -> Result<Vec<Value>, JsErrorBox> {
    require_gate(state)?;
    let st = state.borrow::<Arc<StableState>>();
    let f = filter.unwrap_or(Value::Null);
    let s = |k: &str| f.get(k).and_then(|v| v.as_str()).map(|x| x.to_string());
    let n = |k: &str| f.get(k).and_then(|v| v.as_u64());
    let limit = f
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|v| v.min(1000) as usize)
        .unwrap_or(100);
    let rows = st.audit.query(
        s("kind").as_deref(),
        s("user").as_deref(),
        s("tenant").as_deref(),
        s("alias").as_deref(),
        n("sinceMs"),
        n("untilMs"),
        limit,
    );
    Ok(rows.iter().map(|e| e.to_json()).collect())
}

/// 请求侧便捷：从 OpState 取「emit 所需上下文」（owner + stable）。db_session / client_db
/// 的事件构造方共用，避免各自再拆一遍 ReqState。
pub(crate) fn context_of(state: &OpState) -> (Owner, std::sync::Arc<StableState>) {
    let owner = super::db_session::owner_of(state);
    let st = state.borrow::<Arc<StableState>>().clone();
    (owner, st)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn section() -> AuditSection {
        AuditSection {
            enabled: true,
            max_entries: 4,
            retention_secs: 3600,
            allow: Default::default(),
        }
    }

    fn ev(kind: &str, user: Option<&str>, alias: &str, ts: u64) -> AuditEvent {
        AuditEvent {
            ts_ms: ts,
            kind: kind.into(),
            user: user.map(|s| s.into()),
            tenant: None,
            module: None,
            source: "http",
            alias: alias.into(),
            detail: "action=test".into(),
        }
    }

    fn owner(u: Option<&str>) -> Owner {
        Owner {
            tenant: None,
            user: u.map(|s| s.into()),
            module: None,
            source: "http",
        }
    }

    /// 双限：超条数丢最旧；超龄条目在下次 push/query 时被清。
    #[test]
    fn store_enforces_cap_and_retention() {
        let s = AuditStore::new(&section());
        for i in 0..6 {
            s.push(ev("K", Some("u"), "a", 1000 + i));
        }
        assert_eq!(s.len(), 4, "cap=4 丢最旧");
        let rows = s.query(None, None, None, None, None, None, 100);
        assert_eq!(rows[0].ts_ms, 5005, "最新在前");
        // 超龄清理：当前时刻远超 retention → 清空
        let mut aged = section();
        aged.retention_secs = 1;
        let s2 = AuditStore::new(&aged);
        s2.push(ev("K", None, "a", ms_now().saturating_sub(60_000)));
        s2.push(ev("K", None, "a", ms_now()));
        assert_eq!(s2.len(), 1, "超龄条目被清");
    }

    /// 过滤器各维独立生效 + limit 截断。
    #[test]
    fn query_filters_apply_independently() {
        let s = AuditStore::new(&section());
        s.push(ev("DB_CLIENT_AUDIT", Some("u"), "a", 1));
        s.push(ev("DB_SESSION_BEGIN", Some("u"), "a", 2));
        s.push(ev("DB_SESSION_BEGIN", Some("v"), "b", 3));
        assert_eq!(
            s.query(Some("DB_SESSION_BEGIN"), None, None, None, None, None, 100).len(),
            2
        );
        assert_eq!(
            s.query(None, Some("u"), None, None, None, None, 100).len(),
            2
        );
        assert_eq!(s.query(None, None, None, Some("b"), None, None, 100).len(), 1);
        assert_eq!(
            s.query(None, None, None, None, Some(2), Some(2), 100).len(),
            1,
            "since/until 时间窗"
        );
        assert_eq!(
            s.query(None, None, None, None, None, None, 1).len(),
            1,
            "limit 截断"
        );
        assert_eq!(s.query(None, None, None, None, None, None, 100).len(), 3);
    }

    /// 零凭据：stderr 行只含前缀 id（构造方喂前缀），不含完整 id / 控制字符注入。
    #[test]
    fn line_is_control_char_safe() {
        let mut o = owner(Some("u"));
        o.user = Some("bad\nuser".into());
        let e = AuditEvent {
            ts_ms: 1,
            kind: "K".into(),
            user: o.user.clone(),
            tenant: None,
            module: None,
            source: "http",
            alias: "a\nb".into(),
            detail: "x".into(),
        };
        let l = e.line();
        assert!(!l.contains('\n') || l.lines().count() == 1, "{l:?}");
        assert!(!l.contains("bad\nuser"));
    }

    /// disabled 时只打日志不入库（stderr 行为不因配置改变）。
    #[test]
    fn disabled_store_does_not_retain() {
        let mut sec = section();
        sec.enabled = false;
        let s = AuditStore::new(&sec);
        emit(&s, "K", &owner(Some("u")), "a", "d");
        assert_eq!(s.len(), 0);
    }
}
