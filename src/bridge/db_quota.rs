//! REQ-05 语句级超时 + 结果集字节上限的运行时原语（U-18 批 Q2）。
//!
//! SOLID 单职责：本模块只做**纯计算**——配额聚合、收紧校验、行级字节计量、结构化
//! 错误构造。接线（select! 竞速 / 早停 / 信令）在调用侧（db.rs / query.rs / accessor_sqlx.rs）。
//! DRY：字节计费与「超时弃连」原语批次 3（U-18 导出）复用本模块，不另起一套闸。
//!
//! 字节口径（裁决入档）：行 **JSON 编码字节**（`serde_json::to_vec(row).len()` 累加）——
//! ≥ 线上原始传输字节、确定性可测，且正是 isolate 堆占用的直接量（REQ §4.2-3 建议
//! payload 原始字节的严格版）。

use serde_json::Value;

use super::db_error::{BridgeError, DbErrorCategory, SessionDisposition};
use super::db_policy::{DbAccess, OnExceed};

/// 单条语句的生效配额（policy → op 边界的单一聚合出口；DRY，四处调用点共用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Quota {
    /// 语句级超时毫秒（配置侧上限；调用侧只能收紧）。
    pub statement_timeout_ms: Option<u64>,
    /// 结果集字节上限（行 JSON 编码字节；None = 不限）。
    pub max_result_bytes: Option<u64>,
    /// 触顶策略（None 配置归一为 Fail——REQ：默认 fail-fast，不静默截断）。
    pub on_exceed: OnExceed,
    /// 行数上限（沿用 v0.1.89 语义；与字节上限并存、先到先生效）。
    pub max_rows: Option<u32>,
}

impl Quota {
    pub fn of(policy: &DbAccess) -> Self {
        Self {
            statement_timeout_ms: policy.statement_timeout_ms,
            max_result_bytes: policy.max_result_bytes,
            on_exceed: policy.on_exceed.unwrap_or(OnExceed::Fail),
            max_rows: policy.max_rows,
        }
    }
}

/// 调用侧 `timeoutMs` × 配置上限 → 生效值。
///
/// 只能收紧（REQ §4.1-1）：调用侧传更大值 → 结构化拒，message 点名 `statement_timeout_ms`
/// （验收原文）；两者都 None → None（不设防，与升级前一致）。
pub fn effective_timeout_ms(
    quota_ms: Option<u64>,
    call_ms: Option<u64>,
) -> Result<Option<u64>, BridgeError> {
    match (quota_ms, call_ms) {
        (Some(cap), Some(req)) if req > cap => Err(BridgeError {
            category: DbErrorCategory::Capacity,
            retryable: false,
            session_disposition: SessionDisposition::Keep,
            message: format!(
                "timeoutMs {req} exceeds db_access.statement_timeout_ms {cap}; \
                 the call side can only tighten the configured statement timeout"
            ),
            data: serde_json::json!({
                "configured_ms": cap,
                "requested_ms": req,
                "capability": "statement_timeout_ms",
            })
            .as_object()
            .cloned()
            .unwrap_or_default(),
        }),
        (Some(cap), Some(req)) => Ok(Some(req.min(cap))),
        (Some(cap), None) => Ok(Some(cap)),
        (None, Some(req)) => Ok(Some(req)),
        (None, None) => Ok(None),
    }
}

/// 行级字节计量的拉取决策。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feed {
    /// 未触顶（或无限额）：继续拉取。
    Continue,
    /// 本行已触顶：停止拉取。已取行是否交付由触顶策略决定（调用侧判
    /// `meter.exceeded && on_exceed == Fail` → 转结构化错误；Truncate → 交付已取行）。
    Stop,
}

/// 行级字节计量器：边拉边计（REQ §4.2-1——防的正是「取回」这一步的堆/通道压力）。
#[derive(Debug, Clone)]
pub struct ByteMeter {
    limit: Option<u64>,
    /// 触顶策略（Fail：调用侧转错；Truncate：交付已取行 + 显式信令）。
    pub on_exceed: OnExceed,
    /// 累计字节（含把结果推过上限的那一行——可观测口径 = 结果集真实大小）。
    pub counted: u64,
    /// 是否已触顶（首行越限即置位，之后每行都 Stop）。
    pub exceeded: bool,
}

impl ByteMeter {
    /// `limit = None` 时零开销直通（Continue，counted 照计——信令/画像可用）。
    pub fn new(limit: Option<u64>, on_exceed: OnExceed) -> Self {
        Self {
            limit,
            on_exceed,
            counted: 0,
            exceeded: false,
        }
    }

    /// 计量一行（行 JSON 编码字节）。触顶首行**计入** counted 后置 exceeded 并 Stop。
    pub fn feed(&mut self, row: &Value) -> Feed {
        self.counted += row_bytes(row);
        if self.exceeded {
            return Feed::Stop;
        }
        if let Some(limit) = self.limit
            && self.counted > limit
        {
            self.exceeded = true;
            return Feed::Stop;
        }
        Feed::Continue
    }
}

/// 行字节口径：JSON 编码长度（见模块头注释；单行必可编码，失败按 0 计不 panic）。
pub fn row_bytes(row: &Value) -> u64 {
    serde_json::to_vec(row).map(|v| v.len() as u64).unwrap_or(0)
}

/// 语句竞速：deadline 到点即弃在飞 future（宿主侧 select!，零 ABI——FfiFuture 已
/// `unsafe impl Send`，网络型后端（插件轴）在 await_ffi 的 poll 间隙让出，定时器
/// 可准点触发；sqlite 内置的同步步进会饿死 current_thread 定时器，见模块头与手册
/// 的架构限制节）。到点返回结构化 `OJ_DB_TIMEOUT`，`cancelled` 由后端能力位诚实标注。
pub async fn race_statement<T>(
    fut: impl std::future::Future<Output = Result<T, Box<dyn std::error::Error + Send + Sync>>>,
    alias: &str,
    timeout_ms: Option<u64>,
    cancelled: bool,
) -> Result<T, Box<dyn std::error::Error + Send + Sync>> {
    let Some(ms) = timeout_ms else {
        return fut.await;
    };
    match tokio::time::timeout(std::time::Duration::from_millis(ms), fut).await {
        Ok(r) => r,
        Err(_) => Err(stmt_timeout_error(alias, ms, cancelled).into()),
    }
}

/// 语句超时结构化错误（`OJ_DB_TIMEOUT`）。`cancelled` 按后端诚实标注：
/// sqlite 内置真早停 = true；插件轴宿主弃句柄（语句可能仍在服务端跑）= false。
pub fn stmt_timeout_error(alias: &str, timeout_ms: u64, cancelled: bool) -> BridgeError {
    BridgeError {
        category: DbErrorCategory::Timeout,
        retryable: true,
        session_disposition: SessionDisposition::Close,
        message: format!("statement timeout after {timeout_ms}ms on db '{alias}'"),
        data: serde_json::json!({
            "alias": alias,
            "timeout_ms": timeout_ms,
            "cancelled": cancelled,
        })
        .as_object()
        .cloned()
        .unwrap_or_default(),
    }
}

/// 字节触顶（fail 模式）结构化错误（`OJ_DB_CAPACITY`）。
pub fn result_too_large_error(alias: &str, limit: u64, counted: u64) -> BridgeError {
    BridgeError {
        category: DbErrorCategory::Capacity,
        retryable: false,
        session_disposition: SessionDisposition::Keep,
        message: format!(
            "result set on db '{alias}' reached max_result_bytes {limit} \
             (counted {counted} bytes of row payloads)"
        ),
        data: serde_json::json!({
            "alias": alias,
            "limit_bytes": limit,
            "counted_bytes": counted,
        })
        .as_object()
        .cloned()
        .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::db_error::code_of;
    use serde_json::json;

    #[test]
    fn effective_timeout_tighten_only() {
        // Given 配置 30s：调用侧收紧 → 生效调用值；等于配置 → 可；更大 → 拒且点名键
        assert_eq!(
            effective_timeout_ms(Some(30_000), Some(5_000)),
            Ok(Some(5_000))
        );
        assert_eq!(
            effective_timeout_ms(Some(30_000), Some(30_000)),
            Ok(Some(30_000))
        );
        let e = effective_timeout_ms(Some(30_000), Some(30_001)).unwrap_err();
        assert_eq!(e.category, DbErrorCategory::Capacity);
        assert_eq!(code_of(e.category), "OJ_DB_CAPACITY");
        assert!(
            e.message.contains("statement_timeout_ms"),
            "message must name the config key: {}",
            e.message
        );
        assert_eq!(e.data["configured_ms"], json!(30_000));
        assert_eq!(e.data["requested_ms"], json!(30_001));
        // 无配置：调用侧自设生效（单向阀只挡「超过配置」，配置缺省不设防）
        assert_eq!(effective_timeout_ms(None, Some(5_000)), Ok(Some(5_000)));
        // 双 None = 不设防
        assert_eq!(effective_timeout_ms(None, None), Ok(None));
    }

    #[test]
    fn meter_counts_and_stops_at_limit() {
        // Given 上限 10 字节，行 {"a":1} = 7 字节、{"b":22} = 8 字节
        let mut m = ByteMeter::new(Some(10), OnExceed::Fail);
        assert_eq!(m.feed(&json!({"a": 1})), Feed::Continue);
        assert_eq!(m.counted, 7);
        // 第二行累计 15 > 10 → Stop，触顶行计入 counted（真实结果集大小口径）
        assert_eq!(m.feed(&json!({"b": 22})), Feed::Stop);
        assert!(m.exceeded);
        assert_eq!(m.counted, 15);
        // 之后每行都 Stop（不重复翻转语义）
        assert_eq!(m.feed(&json!({"c": 3})), Feed::Stop);
        // 无上限：恒 Continue，counted 照计
        let mut free = ByteMeter::new(None, OnExceed::Fail);
        assert_eq!(free.feed(&json!({"a": 1})), Feed::Continue);
        assert_eq!(free.counted, 7);
        assert!(!free.exceeded);
        // 恰好等于上限不触顶（超界才停）
        let mut exact = ByteMeter::new(Some(7), OnExceed::Fail);
        assert_eq!(exact.feed(&json!({"a": 1})), Feed::Continue);
        assert!(!exact.exceeded);
    }

    #[test]
    fn stmt_timeout_envelope() {
        let e = stmt_timeout_error("rep", 30_000, true);
        assert_eq!(e.category, DbErrorCategory::Timeout);
        assert_eq!(code_of(e.category), "OJ_DB_TIMEOUT");
        assert!(e.retryable);
        assert_eq!(e.session_disposition, SessionDisposition::Close);
        assert_eq!(e.data["cancelled"], json!(true));
        assert_eq!(e.data["timeout_ms"], json!(30_000));
    }

    #[test]
    fn result_too_large_envelope() {
        let e = result_too_large_error("rep", 64, 100);
        assert_eq!(code_of(e.category), "OJ_DB_CAPACITY");
        assert!(!e.retryable);
        assert_eq!(e.data["limit_bytes"], json!(64));
        assert_eq!(e.data["counted_bytes"], json!(100));
        assert!(
            e.message.contains("max_result_bytes"),
            "message must name the cap: {}",
            e.message
        );
    }

    #[test]
    fn quota_of_normalizes_on_exceed() {
        let mut p = DbAccess::default();
        assert_eq!(Quota::of(&p).on_exceed, OnExceed::Fail);
        p.statement_timeout_ms = Some(1);
        p.max_result_bytes = Some(2);
        p.on_exceed = Some(OnExceed::Truncate);
        p.max_rows = Some(3);
        let q = Quota::of(&p);
        assert_eq!(
            q,
            Quota {
                statement_timeout_ms: Some(1),
                max_result_bytes: Some(2),
                on_exceed: OnExceed::Truncate,
                max_rows: Some(3),
            }
        );
    }
}
