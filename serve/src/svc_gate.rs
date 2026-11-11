//! SvcGate（spec 2026-11-11 P3）：svc 凭据 jti 吊销门 + 动词档位。
//! 查询面走 default db 的 `DataAccessor`（静态 SQL `SELECT jti FROM oj_svc_token`
//! ——非 JS 来源，沿 `_oj_migrations` 先例豁免 SchemaRegistry）；30s TTL 缓存挡
//! 热路径（吊销 ≤30s 全网生效）；查询 Err 一律 fail-closed（视同吊销）。

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use only_js::bridge::{BridgeResult, DataAccessor};
use tokio::sync::RwLock;

/// 缺省缓存 TTL。
const DEFAULT_TTL: Duration = Duration::from_secs(30);

pub struct SvcGate {
    accessor: Arc<dyn DataAccessor>,
    cache: RwLock<Option<(Instant, HashSet<String>)>>,
    ttl: Duration,
}

impl SvcGate {
    pub fn new(accessor: Arc<dyn DataAccessor>) -> Self {
        Self::with_ttl(accessor, DEFAULT_TTL)
    }

    /// 测试注入短 TTL（生产构造走 `new`；30s 缓存在用例里等不起）。
    pub fn with_ttl(accessor: Arc<dyn DataAccessor>, ttl: Duration) -> Self {
        Self {
            accessor,
            cache: RwLock::new(None),
            ttl,
        }
    }

    /// jti 全集（30s TTL）。Err = 底层查询失败——调用方必须 fail-closed，本函数不吞错。
    pub async fn jtis(&self) -> BridgeResult<HashSet<String>> {
        let mut c = self.cache.write().await;
        if let Some((at, set)) = c.as_ref()
            && at.elapsed() < self.ttl
        {
            return Ok(set.clone());
        }
        let rows = self
            .accessor
            .query_with_params("SELECT jti FROM oj_svc_token", &[])
            .await?;
        let set: HashSet<String> = rows
            .iter()
            .filter_map(|r| r.get("jti").and_then(|v| v.as_str()).map(str::to_string))
            .collect();
        *c = Some((Instant::now(), set.clone()));
        Ok(set)
    }

    /// 门禁判定：集合不含该 jti 或底层查询 Err → false（fail-closed）。
    pub async fn admits(&self, jti: &str) -> bool {
        match self.jtis().await {
            Ok(set) => set.contains(jti),
            Err(_) => false,
        }
    }
}

/// 动词 → 权限档位（spec 2026-11-11 三档：read/write/destructive）。
/// admit 与 mcp_http 的 tools 展示裁剪共用（DRY）；大小写不敏感（axum verb 大写、
/// 路由表小写，两处都要命中）。None = 该动词无档位（HEAD/OPTIONS 等，不设防）。
pub fn tier_of(verb: &str) -> Option<&'static str> {
    match verb.to_ascii_lowercase().as_str() {
        "get" => Some("read"),
        "post" | "put" | "patch" => Some("write"),
        "delete" | "del" => Some("destructive"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use only_js::bridge::InMemoryAccessor;
    use serde_json::json;

    fn gate(rows: &[&str]) -> SvcGate {
        let acc = InMemoryAccessor::new();
        acc.seed(rows.iter().map(|j| json!({ "jti": j })));
        SvcGate::with_ttl(Arc::new(acc), Duration::from_millis(30))
    }

    // Given 动词字面量（大小写混合）
    // When tier_of
    // Then get→read / post|put|patch→write / delete|del→destructive / 其他 None
    #[test]
    fn tier_of_maps_verbs_case_insensitively() {
        assert_eq!(tier_of("GET"), Some("read"));
        assert_eq!(tier_of("get"), Some("read"));
        assert_eq!(tier_of("POST"), Some("write"));
        assert_eq!(tier_of("Put"), Some("write"));
        assert_eq!(tier_of("patch"), Some("write"));
        assert_eq!(tier_of("DELETE"), Some("destructive"));
        assert_eq!(tier_of("del"), Some("destructive"));
        assert_eq!(tier_of("HEAD"), None);
        assert_eq!(tier_of("OPTIONS"), None);
    }

    // Given 表里有 jt-a
    // When admits
    // Then jt-a true、jt-b false；底层 Err → false（fail-closed）
    #[tokio::test]
    async fn admits_uses_registered_set_and_fails_closed() {
        let g = gate(&["jt-a"]);
        assert!(g.admits("jt-a").await);
        assert!(!g.admits("jt-b").await);
        // 错误注入 → fail-closed（缓存过期后重新查询撞错；deadline 轮询至翻转）。
        let acc = Arc::new(InMemoryAccessor::new());
        acc.seed([json!({ "jti": "jt-a" })]);
        let g = SvcGate::with_ttl(acc.clone(), Duration::from_millis(5));
        assert!(g.admits("jt-a").await);
        acc.set_error("db gone");
        let dl = Instant::now() + Duration::from_secs(2);
        while g.admits("jt-a").await {
            assert!(
                Instant::now() < dl,
                "query error must fail closed after ttl"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    // Given TTL 缓存
    // When 底层行变化但缓存未过期
    // Then jtis() 仍回缓存值（30s 窗口内吊销不抖动；过期后反映新集）
    #[tokio::test]
    async fn jtis_caches_within_ttl_and_refreshes_after() {
        let acc = Arc::new(InMemoryAccessor::new());
        acc.seed([json!({ "jti": "jt-a" })]);
        let g = SvcGate::with_ttl(acc.clone(), Duration::from_millis(40));
        assert!(g.jtis().await.unwrap().contains("jt-a"));
        acc.seed([json!({ "jti": "jt-b" })]);
        let s = g.jtis().await.unwrap();
        assert!(s.contains("jt-a") && !s.contains("jt-b"), "TTL 内用缓存");
        // deadline 轮询等缓存过期（禁固定 sleep 当同步——此处有界轮询至翻转）。
        let dl = Instant::now() + Duration::from_secs(2);
        loop {
            if g.jtis().await.unwrap().contains("jt-b") {
                break;
            }
            assert!(Instant::now() < dl, "cache must refresh after ttl");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}
