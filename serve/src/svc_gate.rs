//! SvcGate（spec 2026-11-11 P3）：svc 凭据 jti 吊销门 + 动词档位。
//! 查询面走 default db 的 `DataAccessor`（静态 SQL `SELECT jti, status, grace_until
//! FROM oj_svc_token`——非 JS 来源，沿 `_oj_migrations` 先例豁免 SchemaRegistry）；
//! 30s TTL 正缓存挡热路径（吊销 ≤30s 全网生效）；miss 穿透重查一次（新签发 jti
//! 立即可用——R6 轮换无感知续期的前提）；穿透仍 miss 开 2s **全局**负缓存窗——
//! 窗内一切未知 jti 直接 fail-closed 不打库：伪造/探测 bearer 的 DB 命中与
//! 伪造量解耦（≤1 次/NEG_TTL），且负缓存状态 O(1) 内存（按 jti 逐条负缓存会
//! 被随机 jti 洪泛打穿并单调膨胀）；查询 Err 一律 fail-closed（视同吊销）。
//!
//! R6 轮换宽限（v0.1.132）：行过滤在 Rust 侧做（`?` 占位符跨方言不可靠，见
//! accessor_sqlx 直通 sqlx::Any）——行有效 ⇔ 无 status（存量行）/active，或
//! status='superseded' 且 grace_until 未过（旧 jti 宽限内仍放行，过期自动移出）。

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use only_js::bridge::{BridgeResult, DataAccessor};
use tokio::sync::RwLock;

/// 缺省缓存 TTL。
const DEFAULT_TTL: Duration = Duration::from_secs(30);
/// 负缓存 TTL：穿透重查仍 miss 后开此全局短窗，窗内未知 jti 一律 fail-closed
/// 不打库——随机 jti 洪泛的 DB 命中被钳到 ≤1 次/NEG_TTL（原按 jti 逐条负缓存
/// 对每个新伪造 jti 都穿透一次全表 SELECT，查询量与伪造量成正比）。
const NEG_TTL: Duration = Duration::from_secs(2);

pub struct SvcGate {
    accessor: Arc<dyn DataAccessor>,
    cache: RwLock<Option<(Instant, HashSet<String>)>>,
    /// 全局负缓存窗起点：最近一次「穿透重查仍 miss」的时刻。窗内所有未命中
    /// 正缓存的 jti 直接 fail-closed，无需 per-jti 条目——O(1) 内存、无淘汰负担。
    neg_since: RwLock<Option<Instant>>,
    ttl: Duration,
    neg_ttl: Duration,
}

impl SvcGate {
    pub fn new(accessor: Arc<dyn DataAccessor>) -> Self {
        Self::with_ttl(accessor, DEFAULT_TTL)
    }

    /// 测试注入短 TTL（生产构造走 `new`；30s 缓存在用例里等不起）。
    pub fn with_ttl(accessor: Arc<dyn DataAccessor>, ttl: Duration) -> Self {
        Self::with_ttls(accessor, ttl, NEG_TTL)
    }

    /// 测试注入短正/负 TTL（负缓存窗用例里等不起 2s）。
    pub fn with_ttls(accessor: Arc<dyn DataAccessor>, ttl: Duration, neg_ttl: Duration) -> Self {
        Self {
            accessor,
            cache: RwLock::new(None),
            neg_since: RwLock::new(None),
            ttl,
            neg_ttl,
        }
    }

    /// 有效 jti 集（30s TTL）。Err = 底层查询失败——调用方必须 fail-closed，本函数不吞错。
    pub async fn jtis(&self) -> BridgeResult<HashSet<String>> {
        if let Some((at, set)) = self.cache.read().await.as_ref()
            && at.elapsed() < self.ttl
        {
            return Ok(set.clone());
        }
        self.refresh().await
    }

    /// 穿透缓存重查并回填（admits 的 miss 路径与 jtis 的过期路径共用）。
    async fn refresh(&self) -> BridgeResult<HashSet<String>> {
        let rows = self
            .accessor
            .query_with_params("SELECT jti, status, grace_until FROM oj_svc_token", &[])
            .await?;
        let now = now_secs();
        let set: HashSet<String> = rows
            .iter()
            .filter(|r| row_admitted(r, now))
            .filter_map(|r| r.get("jti").and_then(|v| v.as_str()).map(str::to_string))
            .collect();
        *self.cache.write().await = Some((Instant::now(), set.clone()));
        Ok(set)
    }

    /// 门禁判定：命中缓存且含 jti → true（热路径不打库）；miss 且全局负缓存窗
    /// 未过 → false（窗内一切未知 jti 不重查，洪泛被钳到 ≤1 次真查/窗）；
    /// 出窗 → 穿透重查一次（R6 轮换后新 bearer 立即可用，否则要等满 TTL，
    /// "无感知续期"不成立），仍 miss 开窗 → false；查询 Err → false（fail-closed）。
    /// 吊销语义不变：已准入的 jti 在正缓存过期（≤30s）前仍放行。
    /// shortcut: 负缓存窗内新签发的 jti 会被拖最长 NEG_TTL 才可见——jti 是 128bit
    /// 随机值，窗口期内被先探测后签发的概率可忽略；正缓存路径不受影响。
    pub async fn admits(&self, jti: &str) -> bool {
        if let Some((at, set)) = self.cache.read().await.as_ref()
            && at.elapsed() < self.ttl
            && set.contains(jti)
        {
            return true;
        }
        if self
            .neg_since
            .read()
            .await
            .is_some_and(|at| at.elapsed() < self.neg_ttl)
        {
            return false;
        }
        match self.refresh().await {
            Ok(set) => {
                if set.contains(jti) {
                    true
                } else {
                    *self.neg_since.write().await = Some(Instant::now());
                    false
                }
            }
            Err(_) => false,
        }
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// 整数列宽容读取：i64 直取，文本列（pg BIGINT 驱动差异）回退解析。
fn col_i64(v: &serde_json::Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

/// R6 行有效判定：无 status（存量行）或 'active' 恒有效；'superseded' 仅在
/// grace_until 未过期时有效（宽限窗口外自动移出有效集 = 吊销，40311 同路径）。
fn row_admitted(r: &serde_json::Value, now: i64) -> bool {
    match r.get("status").and_then(|v| v.as_str()) {
        None | Some("") | Some("active") => true,
        Some("superseded") => col_i64(&r["grace_until"]).is_some_and(|g| g > now),
        Some(_) => false,
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

    /// 计数的 DataAccessor 包装：只统计真查次数（负缓存测试断言「不再打库」）。
    struct CountingAcc {
        inner: InMemoryAccessor,
        n: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl DataAccessor for CountingAcc {
        async fn query_with_params(
            &self,
            sql: &str,
            params: &[serde_json::Value],
        ) -> BridgeResult<Vec<serde_json::Value>> {
            self.n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner.query_with_params(sql, params).await
        }
        async fn exec_with_params(
            &self,
            sql: &str,
            params: &[serde_json::Value],
        ) -> BridgeResult<i64> {
            self.inner.exec_with_params(sql, params).await
        }
    }

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

    // Given 缓存已载入（jt-a）且 TTL 未过期
    // When 新 jti（jt-b）落库后立即 admits
    // Then 穿透缓存放行（R6 轮换后新 bearer 零等待；吊销 30s 语义不受影响）
    #[tokio::test]
    async fn admits_passthrough_on_miss_for_freshly_issued_jti() {
        let acc = Arc::new(InMemoryAccessor::new());
        acc.seed([json!({ "jti": "jt-a" })]);
        let g = SvcGate::with_ttl(acc.clone(), Duration::from_secs(30));
        assert!(g.admits("jt-a").await);
        acc.seed([json!({ "jti": "jt-b", "status": "active" })]);
        assert!(g.admits("jt-b").await, "miss 必须穿透重查，不得等 TTL");
        assert!(!g.admits("jt-c").await, "穿透后仍 miss → fail-closed");
    }

    // Given 穿透重查已判 absent 的伪造 jti
    // When NEG_TTL 窗内反复 admits
    // Then 不打库（底层查询计数不增长），直接 fail-closed；出窗后恢复穿透
    #[tokio::test]
    async fn admits_neg_cache_stops_probe_flood() {
        let inner = InMemoryAccessor::new();
        inner.seed([json!({ "jti": "jt-a", "status": "active" })]);
        let acc = Arc::new(CountingAcc {
            inner,
            n: std::sync::atomic::AtomicUsize::new(0),
        });
        let g = SvcGate::with_ttl(acc.clone(), Duration::from_secs(30));
        assert!(g.admits("jt-a").await);
        let base = acc.n.load(std::sync::atomic::Ordering::SeqCst);
        for _ in 0..5 {
            assert!(!g.admits("jt-forged").await);
        }
        assert_eq!(
            acc.n.load(std::sync::atomic::Ordering::SeqCst),
            base + 1,
            "负缓存窗内伪造 jti 只允许 1 次真查（首次穿透），其后不得打库"
        );
        // 正缓存命中的 jti 全程不打库（与负缓存无关的回归护栏）。
        let after_hit = acc.n.load(std::sync::atomic::Ordering::SeqCst);
        assert!(g.admits("jt-a").await);
        assert_eq!(acc.n.load(std::sync::atomic::Ordering::SeqCst), after_hit);
    }

    // 回归（评审 #2）：N 个**互不相同**的伪造 jti 在窗内洪泛——按 jti 逐条负缓存
    // 的旧实现会对每个新 jti 穿透一次全表 SELECT（查询量与伪造量成正比）；
    // 全局负缓存窗把整窗 DB 命中钳到 1 次。
    #[tokio::test]
    async fn admits_global_neg_window_caps_distinct_forged_flood() {
        let inner = InMemoryAccessor::new();
        inner.seed([json!({ "jti": "jt-a", "status": "active" })]);
        let acc = Arc::new(CountingAcc {
            inner,
            n: std::sync::atomic::AtomicUsize::new(0),
        });
        let g = SvcGate::with_ttl(acc.clone(), Duration::from_secs(30));
        assert!(g.admits("jt-a").await);
        let base = acc.n.load(std::sync::atomic::Ordering::SeqCst);
        for i in 0..10 {
            assert!(!g.admits(&format!("jt-forged-{i}")).await);
        }
        assert_eq!(
            acc.n.load(std::sync::atomic::Ordering::SeqCst),
            base + 1,
            "10 个不同伪造 jti 窗内只允许 1 次真查（全局窗），不得按 jti 逐条穿透"
        );
        assert!(g.admits("jt-a").await, "正缓存命中不受负缓存窗影响");
    }

    // 回归（评审 #2/#3）：负缓存窗过期后穿透恢复——新落库 jti 立即可见；
    // 且负缓存状态为 O(1)（单个窗起点，无 per-jti 条目堆积）。
    #[tokio::test]
    async fn neg_window_expiry_resumes_passthrough() {
        let acc = Arc::new(InMemoryAccessor::new());
        acc.seed([json!({ "jti": "jt-a", "status": "active" })]);
        // 负缓存窗 30ms：伪造 jti 开窗后，窗内打库计数不增；出窗后新 jti 穿透成功。
        let g = SvcGate::with_ttls(
            acc.clone(),
            Duration::from_secs(30),
            Duration::from_millis(30),
        );
        assert!(!g.admits("jt-forged").await, "首次穿透判 absent → false");
        acc.seed([json!({ "jti": "jt-b", "status": "active" })]);
        assert!(
            !g.admits("jt-b").await,
            "窗内未知 jti 不打库直接 fail-closed"
        );
        let dl = Instant::now() + Duration::from_secs(2);
        while !g.admits("jt-b").await {
            assert!(Instant::now() < dl, "出窗后必须恢复穿透，新 jti 可见");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    // R6 轮换宽限：active/无 status 恒有效；superseded 在 grace_until 内有效、
    // 过期后移出；未知 status 视为无效（fail-closed）；grace_until 文本列宽容解析。
    #[tokio::test]
    async fn grace_window_admits_superseded_until_expiry() {
        let now = now_secs();
        let acc = Arc::new(InMemoryAccessor::new());
        acc.seed([
            json!({ "jti": "jt-active", "status": "active" }),
            json!({ "jti": "jt-legacy" }), // 存量行无 status
            json!({ "jti": "jt-grace", "status": "superseded", "grace_until": now + 100 }),
            json!({ "jti": "jt-expired", "status": "superseded", "grace_until": now - 100 }),
            json!({ "jti": "jt-grace-text", "status": "superseded", "grace_until": (now + 100).to_string() }),
            json!({ "jti": "jt-badstatus", "status": "weird" }),
        ]);
        let g = SvcGate::with_ttl(acc, Duration::from_millis(30));
        let s = g.jtis().await.unwrap();
        for ok in ["jt-active", "jt-legacy", "jt-grace", "jt-grace-text"] {
            assert!(s.contains(ok), "{ok} must be admitted");
        }
        for denied in ["jt-expired", "jt-badstatus"] {
            assert!(
                !s.contains(denied),
                "{denied} must fall out of the admitted set"
            );
        }
    }

    // Given superseded 行
    // When grace_until 未过 / 已过
    // Then 宽限内放行、过期移出（自动吊销，无需删行）
    #[tokio::test]
    async fn superseded_jti_admitted_only_within_grace() {
        let now = now_secs();
        let acc = Arc::new(InMemoryAccessor::new());
        acc.seed([
            json!({ "jti": "jt-future", "status": "superseded", "grace_until": now + 3600 }),
            json!({ "jti": "jt-past", "status": "superseded", "grace_until": now - 1 }),
        ]);
        let g = SvcGate::with_ttl(acc, Duration::from_millis(30));
        assert!(g.admits("jt-future").await, "宽限内仍放行（在途请求）");
        assert!(!g.admits("jt-past").await, "grace 过期即移出有效集");
    }

    // Given superseded 行 grace_until 即将到期
    // When 划过到期点（缓存 TTL 轮询刷新）
    // Then 该 jti 自动从有效集移除——服务端零动作，与删行吊销同一路径
    #[tokio::test]
    async fn superseded_jti_falls_out_when_grace_passes() {
        let now = now_secs();
        let acc = Arc::new(InMemoryAccessor::new());
        acc.seed([json!({ "jti": "jt-x", "status": "superseded", "grace_until": now + 1 })]);
        let g = SvcGate::with_ttl(acc, Duration::from_millis(20));
        assert!(g.admits("jt-x").await);
        let dl = Instant::now() + Duration::from_secs(4);
        while g.admits("jt-x").await {
            assert!(Instant::now() < dl, "grace 过期后必须自动移出有效集");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}
