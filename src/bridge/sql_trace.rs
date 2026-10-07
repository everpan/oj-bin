//! SQL 执行追踪（开发期可选：dev 日志 + 每请求运行时画像）。
//!
//! 单一内部 recorder：db op 层（`db.rs`）在每次 accessor 调用前后计时并产出 `SqlEvent`，
//! 本模块负责（1）累计进 `ReqState.sql_profile`（dev 信封 `_sql` 与 `db.sqlProfile()`
//! 的数据源）；（2）按 `StableState.sql_trace` 配置决定是否写 dev 日志。
//!
//! 设计红线：参数**默认脱敏**（`redact_params`），绝不把密码/手机号等泄漏到客户端。
//! 画像（`db.sqlProfile()`）与响应信封 `_sql` **永远脱敏**（只记「参数个数」）；
//! 只有 `db_trace.redact_params: false` 时，服务端 dev 日志（`target="oj::sql"`）才记参数原值用于本地排障。

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use deno_core::OpState;
use serde_json::{Value, json};

use super::{ReqState, StableState};

/// 追踪总开关配置（冻进 StableState；装配期由 config + dev/release 决定）。
#[derive(Debug, Clone, Copy)]
pub struct SqlTraceConfig {
    /// 主开关：dev 默认开，release 默认关（可被 config 显式强制）。
    pub enabled: bool,
    /// 参数脱敏（默认 true）：true = 只记参数个数，不记值。
    pub redact_params: bool,
    /// 慢查询阈值（毫秒）：>0 时仅 dev 日志与 slow 列表按此过滤；0 = 全部记录（slow 列表全收）。
    pub slow_ms: f64,
    /// 是否写 dev 日志（默认 true）。
    pub to_log: bool,
}

impl Default for SqlTraceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            redact_params: true,
            slow_ms: 0.0,
            to_log: true,
        }
    }
}

impl SqlTraceConfig {
    /// 由 config 段 + dev 标志解析有效配置（dev = !is_release）。
    pub fn resolve(cfg: &crate::config::SqlTraceCfg, dev: bool) -> Self {
        Self {
            enabled: cfg.enabled.unwrap_or(dev),
            redact_params: cfg.redact_params,
            slow_ms: cfg.slow_ms,
            to_log: cfg.to_log,
        }
    }
}

/// 单条 SQL 执行事件。
#[derive(Debug, Clone)]
pub struct SqlEvent {
    pub sql: String,
    /// 参数摘要：脱敏时为 `"<N params>"`，否则为参数 JSON（仅 redact=false 时含值）。
    pub params: String,
    pub db: String,
    pub in_tx: bool,
    /// 来源：模块名 / "anon" / "task" 等（来自 `ReqState.module`）。
    pub source: String,
    /// 耗时（毫秒）。
    pub ms: f64,
    /// 影响/返回行数（stream/未知为 None）。
    pub rows: Option<i64>,
    pub ok: bool,
    pub error: Option<String>,
}

/// 每请求 SQL 画像（存 ReqState，reset 时清空）。
#[derive(Default)]
pub struct SqlProfile {
    pub events: Vec<SqlEvent>,
    pub count: u32,
    pub total_ms: f64,
    /// 慢查询速览：slow_ms<=0 时全收，>0 时仅收录超过阈值的事件。
    pub slow: Vec<SqlEvent>,
    /// 每库累计：(次数, 总毫秒)。
    pub by_db: HashMap<String, (u32, f64)>,
}

impl SqlProfile {
    /// 记录一条事件（slow_ms<=0 时视为「全收」；>0 时按阈值归类进 slow）。
    pub fn record(&mut self, e: SqlEvent, slow_ms: f64) {
        if slow_ms <= 0.0 || e.ms >= slow_ms {
            self.slow.push(e.clone());
        }
        self.count += 1;
        self.total_ms += e.ms;
        self.events.push(e.clone());
        let ent = self.by_db.entry(e.db).or_insert((0, 0.0));
        ent.0 += 1;
        ent.1 += e.ms;
    }

    /// 画像快照（dev 信封 `_sql` 与 `db.sqlProfile()` 的同构数据源）。
    pub fn snapshot(&self) -> Value {
        let events: Vec<Value> = self
            .events
            .iter()
            .map(|e| {
                json!({
                    "sql": e.sql,
                    "params": e.params,
                    "db": e.db,
                    "inTx": e.in_tx,
                    "source": e.source,
                    "ms": e.ms,
                    "rows": e.rows,
                    "ok": e.ok,
                    "error": e.error,
                })
            })
            .collect();
        let slow: Vec<Value> = self
            .slow
            .iter()
            .map(|e| json!({ "sql": e.sql, "ms": e.ms, "db": e.db }))
            .collect();
        let by_db: Value = self
            .by_db
            .iter()
            .map(|(k, (c, ms))| (k.clone(), json!({ "count": c, "ms": ms })))
            .collect::<serde_json::Map<_, _>>()
            .into();
        json!({
            "count": self.count,
            "totalMs": self.total_ms,
            "slow": slow,
            "byDb": by_db,
            "events": events,
        })
    }
}

/// 参数摘要：脱敏只记个数，否则序列化值（仅 `redact_params=false` 时含值）。
pub fn summarize_params(params: &[Value], redact: bool) -> String {
    if redact {
        format!("{} params", params.len())
    } else {
        serde_json::to_string(params).unwrap_or_else(|_| "<unserializable>".into())
    }
}

/// 在 op 层调用：累计事件进本请求画像（永远脱敏）+ 按配置写 dev 日志（可记原值）。
/// 配置未开启（release 默认 / config 关）则直接返回，零开销。
/// `log_params` 为日志用的参数呈现：redact=true 时为脱敏摘要，false 时为原值（仅服务端日志）。
pub fn record_sql(state: &Rc<RefCell<OpState>>, e: SqlEvent, log_params: String) {
    let cfg = {
        // SqlTraceConfig 是 Copy，取出即释放借用。
        state
            .borrow()
            .borrow::<std::sync::Arc<StableState>>()
            .sql_trace
    };
    if !cfg.enabled {
        return;
    }
    // 落本请求画像（reset 时清空）。e.params 恒为脱敏摘要，客户端永不拿到参数原值。
    {
        let mut g = state.borrow_mut();
        let rs = g.borrow_mut::<ReqState>();
        rs.sql_profile.record(e.clone(), cfg.slow_ms);
    }
    // dev 日志（默认开）。slow_ms>0 时仅记录慢查询。参数呈现由调用方给定：
    // redact=true 记摘要，false 记原值（仅服务端日志，不外泄到响应）。
    if cfg.to_log && (cfg.slow_ms <= 0.0 || e.ms >= cfg.slow_ms) {
        let status = if e.ok { "ok" } else { "ERR" };
        let rows = e.rows.unwrap_or(-1);
        tracing::info!(
            target: "oj::sql",
            sql = %e.sql,
            params = %log_params,
            db = %e.db,
            ms = e.ms,
            rows = rows,
            tx = e.in_tx,
            src = %e.source,
            status = status,
            err = e.error.as_deref().unwrap_or(""),
            "SQL trace"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(sql: &str, ms: f64, rows: Option<i64>) -> SqlEvent {
        SqlEvent {
            sql: sql.to_string(),
            params: "1 params".into(),
            db: "default".into(),
            in_tx: false,
            source: "m".into(),
            ms,
            rows,
            ok: true,
            error: None,
        }
    }

    #[test]
    fn profile_aggregates_and_snapshots() {
        let mut p = SqlProfile::default();
        p.record(ev("select 1", 2.5, Some(1)), 100.0);
        p.record(ev("slow", 120.0, None), 100.0); // slow 阈值 100ms → 仅 120ms 进 slow
        assert_eq!(p.count, 2);
        assert_eq!(p.total_ms, 122.5);
        assert_eq!(p.slow.len(), 1);
        assert_eq!(p.slow[0].sql, "slow");
        assert_eq!(p.by_db.get("default").unwrap().0, 2);
        let s = p.snapshot();
        assert_eq!(s["count"], json!(2));
        assert_eq!(s["events"].as_array().unwrap().len(), 2);
        assert_eq!(s["slow"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn slow_threshold_filters_below() {
        let mut p = SqlProfile::default();
        p.record(ev("fast", 10.0, None), 50.0); // 10ms < 50ms → 不进 slow
        assert_eq!(p.slow.len(), 0);
        assert_eq!(p.count, 1); // 但 events 仍记全量
    }

    #[test]
    fn slow_ms_zero_collects_all() {
        let mut p = SqlProfile::default();
        p.record(ev("a", 1.0, None), 0.0); // slow_ms=0 → 全收
        p.record(ev("b", 999.0, None), 0.0);
        assert_eq!(p.slow.len(), 2, "slow_ms=0 时 slow 列表应收全量");
        assert_eq!(p.count, 2);
    }
}
