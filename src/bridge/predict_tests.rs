//! predict 网关校验核测试（REQ-PREDICT v0.1.145）：dataScope fail-closed 与
//! consent 授权表纯函数矩阵。合规红线（计划 Review Focus 1/2）：
//! - 部分越界必须**整体拒绝**并点名越界项，绝不静默裁剪后继续执行；
//! - 拒绝路径结构化（拒绝原因可审计），无任何「放行但记警告」形态。

use super::predict::{validate_consent, validate_scope, ConsentDeny, ConsentGrant};

fn grant(id: &str, scopes: &[&str], not_after_ms: Option<u64>) -> ConsentGrant {
    ConsentGrant {
        id: id.to_string(),
        scopes: scopes.iter().map(|s| s.to_string()).collect(),
        not_after_ms,
    }
}

// ---------- validate_scope：dataScope 白名单 fail-closed ----------

#[test]
fn scope_exact_hit_passes() {
    let allowed = vec!["db:crm.orders".to_string()];
    assert_eq!(validate_scope(&["db:crm.orders"], &allowed), Ok(()));
}

#[test]
fn scope_wildcard_db_level_hits() {
    let allowed = vec!["db:crm.*".to_string()];
    assert_eq!(validate_scope(&["db:crm.orders"], &allowed), Ok(()));
    assert_eq!(validate_scope(&["db:crm.users"], &allowed), Ok(()));
}

#[test]
fn partial_overreach_denies_whole_with_named_violations() {
    // 合规红线：一条越界 → 整体拒绝（Err），点名全部越界项——绝不裁剪放行。
    let allowed = vec!["db:crm.*".to_string()];
    let err = validate_scope(&["db:crm.orders", "db:hr.salary"], &allowed).unwrap_err();
    assert_eq!(err, vec!["db:hr.salary".to_string()]);
}

#[test]
fn all_overreach_names_every_entry() {
    let allowed = vec!["db:crm.orders".to_string()];
    let err = validate_scope(&["db:x.a", "db:y.b"], &allowed).unwrap_err();
    assert_eq!(err, vec!["db:x.a".to_string(), "db:y.b".to_string()]);
}

#[test]
fn empty_sources_is_trivially_in_scope() {
    // 空声明 = 不触任何源；网关侧若要求非空由 op 层把 pql 判空（校验核只管白名单匹配）。
    let allowed = vec!["db:crm.*".to_string()];
    assert_eq!(validate_scope(&[], &allowed), Ok(()));
}

#[test]
fn empty_allowlist_denies_everything() {
    let err = validate_scope(&["db:crm.orders"], &[]).unwrap_err();
    assert_eq!(err, vec!["db:crm.orders".to_string()]);
}

#[test]
fn malformed_non_db_source_is_violation() {
    // 非 "db:" 前缀的源声明永不命中白名单（fail-closed）。
    let allowed = vec!["db:crm.*".to_string()];
    let err = validate_scope(&["kv:cache"], &allowed).unwrap_err();
    assert_eq!(err, vec!["kv:cache".to_string()]);
}

// ---------- validate_consent：授权表校验 ----------

#[test]
fn consent_hit_passes_and_returns_grant() {
    let grants = vec![grant("g1", &["forecast.monthly"], None)];
    let g = validate_consent("g1", &["forecast.monthly"], &grants, 1000).unwrap();
    assert_eq!(g.id, "g1");
}

#[test]
fn consent_superset_of_declared_scopes_passes() {
    // 声明 scopes 是请求 scopes 的超集（授权范围 ≥ 实际用途）即可。
    let grants = vec![grant("g1", &["forecast.monthly", "forecast.weekly"], None)];
    validate_consent("g1", &["forecast.weekly"], &grants, 1000).unwrap();
}

#[test]
fn consent_unknown_grant_denied() {
    let grants = vec![grant("g1", &["forecast.monthly"], None)];
    assert_eq!(
        validate_consent("nope", &["forecast.monthly"], &grants, 1000),
        Err(ConsentDeny::NoSuchGrant)
    );
}

#[test]
fn consent_expired_denied() {
    let grants = vec![grant("g1", &["forecast.monthly"], Some(500))];
    assert_eq!(
        validate_consent("g1", &["forecast.monthly"], &grants, 500),
        Err(ConsentDeny::Expired) // 恰在边界即视为过期（<= not_after 拒，fail-closed）
    );
    assert_eq!(
        validate_consent("g1", &["forecast.monthly"], &grants, 499),
        Ok(&grants[0])
    );
}

#[test]
fn consent_scopes_insufficient_denied() {
    let grants = vec![grant("g1", &["forecast.monthly"], None)];
    assert_eq!(
        validate_consent("g1", &["forecast.daily"], &grants, 1000),
        Err(ConsentDeny::ScopesInsufficient)
    );
}

#[test]
fn consent_empty_grants_denies_everything() {
    assert_eq!(
        validate_consent("g1", &["forecast.monthly"], &[], 1000),
        Err(ConsentDeny::NoSuchGrant)
    );
}

#[test]
fn consent_requesting_no_scopes_passes_existing_grant() {
    // 请求侧零 scope：无扩张即无越权（存在有效 grant 即可）。
    let grants = vec![grant("g1", &["forecast.monthly"], None)];
    validate_consent("g1", &[], &grants, 1000).unwrap();
}
