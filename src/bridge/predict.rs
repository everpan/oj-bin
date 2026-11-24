//! REQ-PREDICT 预测执行与合规网关（v0.1.145，上游 UPSTREAM-REQS §3.2）。
//!
//! 职责分层（合规边界，计划文档 §Architecture）：
//! - **本模块（校验核）**：dataScope 白名单（fail-closed，部分越界整体拒绝）+
//!   consent 授权表（存在/未过期/scopes 覆盖）——纯函数，供 `op_predict_run` 网关
//!   op 与测试复用；
//! - **网关 op**（Task 2，同模块）：三闸校验 → `audit::emit`（PREDICT_AUDIT /
//!   PREDICT_DENY，拒绝必留痕）→ dispatch 收编引擎轴（`cfg.predict.axis`，不进
//!   `axis()` 公开面——特权旁路封堵）→ 有界超时 → `{result, trace}`；
//! - **引擎**：泛型轴插件（`plugins/oj-predict` 内置确定性 mock；外部引擎适配器
//!   由部署侧按契约自备）——PQL 语义归引擎，网关不解析 PQL（声明式 dataScope）。
//!
//! 时间口径：`not_after_ms` 用 unix 毫秒整数比较（零时间解析依赖）；恰在边界
//! 即视为过期（`now >= not_after` 拒，fail-closed）。
//!
//! 审计零凭据：detail 只带 pql_sha256 前 12 hex、sources、grant_id、status、
//! 耗时——PQL 原文与引擎返回数据一律不进审计与错误消息。

use serde::{Deserialize, Serialize};

/// config `predict:` 段（顶层；全部字段 serde default——段缺席 = 网关存在但
/// 策略为空 = 一切 dataScope/consent 被拒，fail-closed）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PredictSection {
    /// 收编的引擎泛型轴名（不进 `axis()` 公开面）。默认 "predict"。
    #[serde(default = "default_axis")]
    pub axis: String,
    /// 引擎调用上界（毫秒）。默认 15000。
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub policy: PredictPolicy,
}

fn default_axis() -> String {
    "predict".to_string()
}

fn default_timeout_ms() -> u64 {
    15_000
}

/// 策略面：部署期静态治理（白名单 + 授权表）。交互式 consent 流属上游 G2 裁定，
/// v1 不做（文档如实声明）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PredictPolicy {
    /// dataScope 允许面：形如 `db:<name>.<table>`（精确）或 `db:<name>.*`（库级通配）。
    /// 空 = 拒绝一切 dataScope。
    #[serde(default)]
    pub allowed_sources: Vec<String>,
    /// 授权表：grant 存在、未过期、声明 scopes ⊇ 请求 scopes 三者全过才放行。
    #[serde(default)]
    pub grants: Vec<ConsentGrant>,
}

/// 一条部署期授权（`OJ_SECRET`-同级的治理配置，凭据不进此结构）。
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct ConsentGrant {
    pub id: String,
    /// 该授权允许的 scope 词表（如 `forecast.monthly`）。
    #[serde(default)]
    pub scopes: Vec<String>,
    /// 过期线（unix 毫秒）；None = 永不过期。
    #[serde(default)]
    pub not_after_ms: Option<u64>,
}

/// dataScope 白名单校验：任一条声明源不命中（精确或 `db:<name>.*` 尾通配）
/// 即整体拒绝，Err 携带**全部**越界项（点名给审计与调用方）。
pub fn validate_scope(sources: &[&str], allowed: &[String]) -> Result<(), Vec<String>> {
    let violations: Vec<String> = sources
        .iter()
        .filter(|s| !allowed.iter().any(|a| source_allowed(s, a)))
        .map(|s| s.to_string())
        .collect();
    if violations.is_empty() {
        Ok(())
    } else {
        Err(violations)
    }
}

/// 单源命中判定：精确相等，或白名单项为 `db:<name>.*` 尾通配且源是同库的
/// `db:<name>.<table>`。非 `db:` 前缀的源永不命中（fail-closed）。
fn source_allowed(source: &str, allowed: &str) -> bool {
    if source == allowed {
        return true;
    }
    let Some(base) = allowed.strip_suffix(".*").and_then(|b| b.strip_prefix("db:")) else {
        return false;
    };
    match source.strip_prefix("db:").and_then(|r| r.split_once('.')) {
        Some((db, table)) => db == base && !table.is_empty(),
        None => false,
    }
}

/// consent 拒绝原因（op 层映射错误文案与审计 reason）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsentDeny {
    NoSuchGrant,
    Expired,
    ScopesInsufficient,
}

/// consent 授权表校验：grant 存在 → 未过期（`now >= not_after` 即过期）→
/// 声明 scopes 覆盖全部请求 scopes。请求侧零 scopes 只要求 grant 存在且未过期
/// （无扩张即无越权）。
pub fn validate_consent<'a>(
    grant_id: &str,
    scopes: &[&str],
    grants: &'a [ConsentGrant],
    now_ms: u64,
) -> Result<&'a ConsentGrant, ConsentDeny> {
    let Some(g) = grants.iter().find(|g| g.id == grant_id) else {
        return Err(ConsentDeny::NoSuchGrant);
    };
    if let Some(not_after) = g.not_after_ms
        && now_ms >= not_after
    {
        return Err(ConsentDeny::Expired);
    }
    if scopes.iter().any(|s| !g.scopes.iter().any(|d| d == s)) {
        return Err(ConsentDeny::ScopesInsufficient);
    }
    Ok(g)
}
