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
/// Default 手工实现：serde 的 `default = "…"` 只在反序列化路径生效，`Config::default()`
/// （oj serve 测试/嵌入式装配）走这里——axis 缺省名必须与 serde 路径一致。
#[derive(Debug, Clone, Serialize, Deserialize)]
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

impl Default for PredictSection {
    fn default() -> Self {
        Self {
            axis: default_axis(),
            timeout_ms: default_timeout_ms(),
            policy: PredictPolicy::default(),
        }
    }
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
    let Some(base) = allowed
        .strip_suffix(".*")
        .and_then(|b| b.strip_prefix("db:"))
    else {
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

// ---------------------------------------------------------------------------
// 宿主网关 op（Task 2）：predict.run 三闸 → 审计 → 收编引擎轴 dispatch
// ---------------------------------------------------------------------------

use std::cell::RefCell;
use std::hash::{BuildHasher, Hasher};
use std::rc::Rc;
use std::sync::Arc;

use deno_core::{OpState, op2};
use deno_error::JsErrorBox;
use serde_json::Value;

use super::StableState;

/// JS 入参（`predict.run({pql, dataScope, consent})`）。
#[derive(Debug, Deserialize)]
struct RunArgs {
    pql: String,
    #[serde(default, rename = "dataScope")]
    data_scope: DataScope,
    #[serde(default)]
    consent: ConsentReq,
}

#[derive(Debug, Deserialize, Default)]
struct DataScope {
    /// 声明触达的数据源（`db:<name>.<table>` / `db:<name>.*`）——白名单逐条校验。
    #[serde(default)]
    sources: Vec<String>,
    /// 声明行数上界（透传引擎，引擎契约负责遵守；网关不解析返回集）。
    #[serde(default, rename = "maxRows")]
    max_rows: Option<u64>,
}

#[derive(Debug, Deserialize, Default)]
struct ConsentReq {
    #[serde(default, rename = "grantId")]
    grant_id: String,
    #[serde(default)]
    scopes: Vec<String>,
}

/// 错误码（`e.code` 属性，db 轴 OJ_DB_* 同透传机制）。
const CODE_SCOPE: &str = "OJ_PREDICT_SCOPE";
const CODE_CONSENT: &str = "OJ_PREDICT_CONSENT";
const CODE_ENGINE: &str = "OJ_PREDICT_ENGINE";

/// 每调用随机 u64（SipHash 随机键，无 rng 依赖）——ticket 与 trace id 的熵源。
fn random_u64() -> u64 {
    std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish()
}

fn ms_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// PQL sha256 前 12 hex（审计/trace 指纹；原文不进审计与错误消息）。
fn pql_fingerprint(pql: &str) -> String {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    h.update(pql.as_bytes());
    let d = h.finalize();
    d.iter().map(|b| format!("{b:02x}")).collect::<String>()[..12].to_string()
}

/// 引擎错误消息脱敏：剥 PQL 原文（引擎回显场景）+ 截断 300 字符。
fn redact_engine_msg(msg: &str, pql: &str) -> String {
    let mut m = if !pql.is_empty() && msg.contains(pql) {
        msg.replace(pql, "<pql>")
    } else {
        msg.to_string()
    };
    if m.chars().count() > 300 {
        m = m.chars().take(300).collect();
    }
    m
}

/// 审计单出口（mgmt::mgmt_emit 同款形态）：零凭据 detail JSON。
fn emit(st: &StableState, owner: &super::handle_core::Owner, kind: &str, detail: &Value) {
    super::audit::emit(&st.audit, kind, owner, "predict", &detail.to_string());
}

fn deny_detail(
    fp: &str,
    sources: &[String],
    grant_id: &str,
    reason: &str,
    violations: &[String],
) -> Value {
    serde_json::json!({
        "status": "denied",
        "reason": reason,
        "pqlSha256": fp,
        "sources": sources,
        "grantId": grant_id,
        "violations": violations,
    })
}

#[op2]
#[serde]
pub async fn op_predict_run(
    state: Rc<RefCell<OpState>>,
    #[string] payload: String,
) -> Result<serde_json::Value, JsErrorBox> {
    // 借用纪律：OpState 守卫不跨 await——先取 Arc 克隆再放借。
    let st = {
        let s = state.borrow();
        s.borrow::<Arc<StableState>>().clone()
    };
    let owner = super::db_session::owner_of(&state.borrow());
    let args: RunArgs = serde_json::from_str(&payload).map_err(|e| {
        // G2 评审（v0.1.145 收口）：唯一不留痕的出口补审计——payload 不可解析没有
        // pql/dataScope 可记，指纹取**原始 payload** 的 sha256 前 12 hex（同口径，
        // 不泄原文）。引擎未触达（非出境事件），kind 用 AUDIT status=denied。
        emit(
            &st,
            &owner,
            "PREDICT_AUDIT",
            &deny_detail(&pql_fingerprint(&payload), &[], "", "bad_payload", &[]),
        );
        super::db_error::structured_js_error(
            CODE_ENGINE,
            format!("predict.run: payload must be {{pql, dataScope, consent}} ({e})"),
            serde_json::json!({}),
        )
    })?;
    let fp = pql_fingerprint(&args.pql);
    let sources = args.data_scope.sources.clone();
    let grant_id = args.consent.grant_id.clone();
    let started = std::time::Instant::now();

    // 闸 0：引擎在位（收编注册表；axis() 公开面不可达）。
    let engine = st.predict_axes.get(&st.predict_section.axis).map(|h| h.vt);
    let Some(engine) = engine else {
        emit(
            &st,
            &owner,
            "PREDICT_AUDIT",
            &deny_detail(&fp, &sources, &grant_id, "no_engine", &[]),
        );
        return Err(super::db_error::structured_js_error(
            CODE_ENGINE,
            "predict engine plugin not assembled (install plugins/oj-predict or a contract-compatible engine; axis is gateway-reserved and not reachable via axis())".to_string(),
            serde_json::json!({}),
        ));
    };

    let finish_trace = |status: &str, engine_ms: u128, violations: Vec<String>| {
        serde_json::json!({
            "id": format!("pt-{:016x}", random_u64()),
            "status": status,
            "engineMs": engine_ms,
            "grantId": grant_id,
            "sources": sources,
            "ticket": "",
            "violations": violations,
        })
    };

    // 闸 1：dataScope 白名单（fail-closed，部分越界整体拒绝并点名）。
    let allowed = &st.predict_section.policy.allowed_sources;
    let refs: Vec<&str> = sources.iter().map(String::as_str).collect();
    if let Err(violations) = validate_scope(&refs, allowed) {
        emit(
            &st,
            &owner,
            "PREDICT_DENY",
            &deny_detail(&fp, &sources, &grant_id, "scope", &violations),
        );
        return Err(super::db_error::structured_js_error(
            CODE_SCOPE,
            format!(
                "predict.run: dataScope exceeds policy.allowed_sources (violations: {violations:?}); the whole request is denied, not trimmed"
            ),
            serde_json::json!({ "violations": violations }),
        ));
    }

    // 闸 2：consent 授权表（存在 / 未过期 / scopes 覆盖）。
    let now = ms_now();
    let scope_refs: Vec<&str> = args.consent.scopes.iter().map(String::as_str).collect();
    if let Err(deny) = validate_consent(
        &grant_id,
        &scope_refs,
        &st.predict_section.policy.grants,
        now,
    ) {
        let reason = match deny {
            ConsentDeny::NoSuchGrant => "no_such_grant",
            ConsentDeny::Expired => "expired",
            ConsentDeny::ScopesInsufficient => "scopes_insufficient",
        };
        emit(
            &st,
            &owner,
            "PREDICT_DENY",
            &deny_detail(&fp, &sources, &grant_id, &format!("consent.{reason}"), &[]),
        );
        return Err(super::db_error::structured_js_error(
            CODE_CONSENT,
            format!(
                "predict.run: consent not granted (reason: {reason}); execution skipped and audited"
            ),
            serde_json::json!({ "reason": reason }),
        ));
    }

    // dispatch：收编引擎轴，有界超时。ticket 随调用下发、插件契约要求回显。
    let ticket = format!("gt-{:016x}", random_u64());
    let engine_args = serde_json::json!({
        "pql": args.pql,
        "dataScope": { "sources": sources, "maxRows": args.data_scope.max_rows },
        "ticket": ticket,
    });
    let budget = std::time::Duration::from_millis(st.predict_section.timeout_ms);
    let dispatched = tokio::time::timeout(
        budget,
        super::generic_axis::call_axis(engine, "run", &engine_args.to_string()),
    )
    .await;
    let engine_ms = started.elapsed().as_millis();
    let out = match dispatched {
        Err(_elapsed) => {
            emit(
                &st,
                &owner,
                "PREDICT_AUDIT",
                &serde_json::json!({
                    "status": "timeout", "pqlSha256": fp, "sources": sources,
                    "grantId": grant_id, "ms": engine_ms, "timeoutMs": st.predict_section.timeout_ms,
                }),
            );
            return Err(super::db_error::structured_js_error(
                CODE_ENGINE,
                format!(
                    "predict.run: engine timed out after {} ms (observable via audit PREDICT_AUDIT status=timeout)",
                    st.predict_section.timeout_ms
                ),
                serde_json::json!({ "timeoutMs": st.predict_section.timeout_ms }),
            ));
        }
        Ok(Err(e)) => {
            let msg = redact_engine_msg(&e.to_string(), &args.pql);
            emit(
                &st,
                &owner,
                "PREDICT_AUDIT",
                &serde_json::json!({
                    "status": "error", "pqlSha256": fp, "sources": sources,
                    "grantId": grant_id, "ms": engine_ms,
                }),
            );
            return Err(super::db_error::structured_js_error(
                CODE_ENGINE,
                format!("predict.run: engine failed: {msg}"),
                serde_json::json!({}),
            ));
        }
        Ok(Ok(v)) => v,
    };

    // 引擎契约校验：{"result": …, "ticket": 回显}——ticket 不符 = 非契约引擎。
    if out.get("ticket").and_then(serde_json::Value::as_str) != Some(ticket.as_str()) {
        emit(
            &st,
            &owner,
            "PREDICT_AUDIT",
            &serde_json::json!({
                "status": "error", "pqlSha256": fp, "sources": sources,
                "grantId": grant_id, "ms": engine_ms, "reason": "ticket_mismatch",
            }),
        );
        return Err(super::db_error::structured_js_error(
            CODE_ENGINE,
            "predict.run: engine did not echo the gateway ticket (contract violation; see docs/plugins/predict.md engine contract)".to_string(),
            serde_json::json!({}),
        ));
    }
    let Some(result) = out.get("result").cloned() else {
        emit(
            &st,
            &owner,
            "PREDICT_AUDIT",
            &serde_json::json!({
                "status": "error", "pqlSha256": fp, "sources": sources,
                "grantId": grant_id, "ms": engine_ms, "reason": "missing_result",
            }),
        );
        return Err(super::db_error::structured_js_error(
            CODE_ENGINE,
            "predict.run: engine response missing \"result\" (contract violation)".to_string(),
            serde_json::json!({}),
        ));
    };

    emit(
        &st,
        &owner,
        "PREDICT_AUDIT",
        &serde_json::json!({
            "status": "ok", "pqlSha256": fp, "sources": sources,
            "grantId": grant_id, "ms": engine_ms,
        }),
    );
    let mut trace = finish_trace("ok", engine_ms, vec![]);
    trace["ticket"] = serde_json::Value::String(ticket);
    Ok(serde_json::json!({ "result": result, "trace": trace }))
}
