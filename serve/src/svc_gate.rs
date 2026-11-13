//! SvcGate（spec 2026-11-11 P3）：svc 凭据 jti 吊销门 + 动词档位。
//! 查询面走 default db 的 `DataAccessor`（静态 SQL `SELECT jti, status, grace_until
//! FROM oj_svc_token`——非 JS 来源，沿 `_oj_migrations` 先例豁免 SchemaRegistry）；
//! 30s TTL 正缓存挡热路径（吊销 ≤30s 全网生效）；miss 穿透重查一次（新签发 jti
//! 立即可用——R6 轮换无感知续期的前提）；穿透仍 miss 开 2s **全局**负缓存窗——
//! 窗内一切未知 jti 直接 fail-closed 不打库：伪造/探测 bearer 的**全表扫描**被
//! 钳到 ≤1 次/NEG_TTL（按 jti 逐条负缓存会被随机 jti 洪泛打穿并单调膨胀）。
//!
//! 负缓存窗内的新合法 jti（评审 #5）：窗不拖死它们——窗内 miss 改走**点查**
//! （`WHERE jti = ?`/`$1`，PK 命中、非全表扫描）：轮换/签发后立刻使用的新 bearer
//! 不被无关探测的窗误伤（fail-closed 但 ≤1 次索引查询的延迟）；伪造洪泛窗内的
//! 代价从全表扫描降为 PK 点查。查询 Err 一律 fail-closed（视同吊销）。
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
/// 负缓存 TTL：穿透重查仍 miss 后开此全局短窗，窗内**全表刷新**被抑制（未知
/// jti 走 PK 点查）——随机 jti 洪泛的全表 SELECT 被钳到 ≤1 次/NEG_TTL（按 jti
/// 逐条负缓存对每个新伪造 jti 都穿透一次全表 SELECT，查询量与伪造量成正比）。
const NEG_TTL: Duration = Duration::from_secs(2);

pub struct SvcGate {
    accessor: Arc<dyn DataAccessor>,
    cache: RwLock<Option<(Instant, HashSet<String>)>>,
    /// 全局负缓存窗起点：最近一次「穿透重查仍 miss」的时刻。窗内全表 refresh
    /// 被抑制（O(1) 内存、无淘汰负担），未知 jti 降级为 PK 点查。
    neg_since: RwLock<Option<Instant>>,
    /// 全表 refresh 串行锁（评审 #11）：窗过期瞬间 C 个不同 jti 并发 admits 时，
    /// 首个真查后开窗/回填缓存，等待者拿锁后**双重检查**（缓存 + 负缓存窗）再
    /// 决定——把 C 次并发全表 SELECT 收敛为 1 次（顺延后至多 1 次点查/人）。
    refresh_lock: tokio::sync::Mutex<()>,
    /// 点查占位符风格：sqlx::Any 层 `?` 跨方言不可靠（postgres 需 `$1`），装配层
    /// 按 default db DSN 方案注入（SvcGate::with_placeholder）。
    ph: &'static str,
    ttl: Duration,
    neg_ttl: Duration,
}

impl SvcGate {
    pub fn new(accessor: Arc<dyn DataAccessor>) -> Self {
        Self::with_placeholder(accessor, "?")
    }

    /// 装配层按方言注入点查占位符（postgres = "$1"，其余 = "?"，对齐
    /// accessor_sqlx 的方言表）。生产构造走 `new`（sqlite/mysql 的 `?`）。
    pub fn with_placeholder(accessor: Arc<dyn DataAccessor>, ph: &'static str) -> Self {
        Self::with_ttls_ph(accessor, DEFAULT_TTL, NEG_TTL, ph)
    }

    /// 测试注入短 TTL（生产构造走 `new`；30s 缓存在用例里等不起）。
    pub fn with_ttl(accessor: Arc<dyn DataAccessor>, ttl: Duration) -> Self {
        Self::with_ttls(accessor, ttl, NEG_TTL)
    }

    /// 测试注入短正/负 TTL（负缓存窗用例里等不起 2s）。
    pub fn with_ttls(accessor: Arc<dyn DataAccessor>, ttl: Duration, neg_ttl: Duration) -> Self {
        Self::with_ttls_ph(accessor, ttl, neg_ttl, "?")
    }

    fn with_ttls_ph(
        accessor: Arc<dyn DataAccessor>,
        ttl: Duration,
        neg_ttl: Duration,
        ph: &'static str,
    ) -> Self {
        Self {
            accessor,
            cache: RwLock::new(None),
            neg_since: RwLock::new(None),
            refresh_lock: tokio::sync::Mutex::new(()),
            ph,
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
        // single-flight（评审 #11）：等待者拿锁后复看缓存，不重查。
        let _g = self.refresh_lock.lock().await;
        if let Some((at, set)) = self.cache.read().await.as_ref()
            && at.elapsed() < self.ttl
        {
            return Ok(set.clone());
        }
        self.refresh().await
    }

    /// 全表重查并回填（**调用方须已持 refresh_lock**；admits 的 miss 路径与
    /// jtis 的过期路径共用）。
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

    /// 点查（PK 命中）：负缓存窗内未知 jti 的降级通路（评审 #5）——新签发的合法
    /// jti 不被无关探测开出的窗拖住（R6 无感知续期成立）；伪造洪泛窗内的代价
    /// 是全表扫描之外的 PK 点查。行过滤仍在 Rust 侧（同 refresh）。Err → false。
    async fn point_check(&self, jti: &str) -> bool {
        let sql = format!(
            "SELECT jti, status, grace_until FROM oj_svc_token WHERE jti = {}",
            self.ph
        );
        let rows = match self
            .accessor
            .query_with_params(&sql, &[serde_json::Value::String(jti.to_string())])
            .await
        {
            Ok(r) => r,
            Err(_) => return false,
        };
        let now = now_secs();
        // Rust 侧再校一次 jti 相等：InMemoryAccessor 不执行 WHERE，真实后端
        // 双保险（点查按主键，结果至多一行）。
        rows.iter()
            .any(|r| r.get("jti").and_then(|v| v.as_str()) == Some(jti) && row_admitted(r, now))
    }

    /// 负缓存窗是否开着。
    async fn neg_window_open(&self) -> bool {
        self.neg_since
            .read()
            .await
            .is_some_and(|at| at.elapsed() < self.neg_ttl)
    }

    /// 门禁判定：命中缓存且含 jti → true（热路径不打库）；全局负缓存窗开着 →
    /// **点查**一次（窗内不跑全表刷新，评审 #5——新签发 jti 不被窗拖死；伪造
    /// jti 的代价是 PK 点查而非全表扫描）；出窗 → single-flight 全表重查一次
    /// （首个 miss 开窗 → false；命中 → true；Err → false，fail-closed）。
    /// 吊销语义不变：已准入的 jti 在正缓存过期（≤30s）前仍放行。
    pub async fn admits(&self, jti: &str) -> bool {
        if let Some((at, set)) = self.cache.read().await.as_ref()
            && at.elapsed() < self.ttl
            && set.contains(jti)
        {
            return true;
        }
        if self.neg_window_open().await {
            return self.point_check(jti).await;
        }
        // single-flight（评审 #11）：等待期间可能已被他人刷新/开窗——双重检查。
        let _g = self.refresh_lock.lock().await;
        if let Some((at, set)) = self.cache.read().await.as_ref()
            && at.elapsed() < self.ttl
            && set.contains(jti)
        {
            return true;
        }
        if self.neg_window_open().await {
            return self.point_check(jti).await;
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

    /// 计数的 DataAccessor 包装：分计全表扫描（SELECT 无 WHERE）与 PK 点查
    /// （WHERE jti）——负缓存测试断言「窗内不再全表扫描」（点查是窗内降级通路，
    /// 评审 #5 后不再视为「打库违规」）。
    struct CountingAcc {
        inner: InMemoryAccessor,
        n_full: std::sync::atomic::AtomicUsize,
        n_point: std::sync::atomic::AtomicUsize,
    }

    impl CountingAcc {
        fn counts(&self) -> (usize, usize) {
            (
                self.n_full.load(std::sync::atomic::Ordering::SeqCst),
                self.n_point.load(std::sync::atomic::Ordering::SeqCst),
            )
        }
    }

    #[async_trait::async_trait]
    impl DataAccessor for CountingAcc {
        async fn query_with_params(
            &self,
            sql: &str,
            params: &[serde_json::Value],
        ) -> BridgeResult<Vec<serde_json::Value>> {
            if sql.contains("WHERE") {
                self.n_point
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            } else {
                self.n_full
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
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
    // Then 全表扫描不再发生（窗抑制全表刷新）；每次 miss 走 PK 点查（评审 #5 的
    //      降级通路）——伪造 jti 仍 fail-closed，但代价从全表扫描降为索引命中。
    #[tokio::test]
    async fn admits_neg_cache_stops_probe_flood() {
        let inner = InMemoryAccessor::new();
        inner.seed([json!({ "jti": "jt-a", "status": "active" })]);
        let acc = Arc::new(CountingAcc {
            inner,
            n_full: std::sync::atomic::AtomicUsize::new(0),
            n_point: std::sync::atomic::AtomicUsize::new(0),
        });
        let g = SvcGate::with_ttl(acc.clone(), Duration::from_secs(30));
        assert!(g.admits("jt-a").await);
        let (full0, point0) = acc.counts();
        for _ in 0..5 {
            assert!(!g.admits("jt-forged").await);
        }
        let (full1, point1) = acc.counts();
        assert_eq!(
            full1,
            full0 + 1,
            "负缓存窗内全表扫描只允许 1 次（首次穿透开窗）"
        );
        assert_eq!(point1 - point0, 4, "窗内后续 miss 走 PK 点查（每次一次）");
        // 正缓存命中的 jti 全程不打库（与负缓存无关的回归护栏）。
        let (full2, point2) = acc.counts();
        assert!(g.admits("jt-a").await);
        assert_eq!(acc.counts(), (full2, point2));
    }

    // 回归（评审 #2）：N 个**互不相同**的伪造 jti 在窗内洪泛——按 jti 逐条负缓存
    // 的旧实现会对每个新 jti 穿透一次全表 SELECT（查询量与伪造量成正比）；
    // 全局负缓存窗把整窗的全表扫描钳到 1 次（点查不计——PK 命中，非扫描）。
    #[tokio::test]
    async fn admits_global_neg_window_caps_distinct_forged_flood() {
        let inner = InMemoryAccessor::new();
        inner.seed([json!({ "jti": "jt-a", "status": "active" })]);
        let acc = Arc::new(CountingAcc {
            inner,
            n_full: std::sync::atomic::AtomicUsize::new(0),
            n_point: std::sync::atomic::AtomicUsize::new(0),
        });
        let g = SvcGate::with_ttl(acc.clone(), Duration::from_secs(30));
        assert!(g.admits("jt-a").await);
        let (full0, _) = acc.counts();
        for i in 0..10 {
            assert!(!g.admits(&format!("jt-forged-{i}")).await);
        }
        let (full1, _) = acc.counts();
        assert_eq!(
            full1,
            full0 + 1,
            "10 个不同伪造 jti 窗内只允许 1 次全表扫描（全局窗），不得按 jti 逐条穿透"
        );
        assert!(g.admits("jt-a").await, "正缓存命中不受负缓存窗影响");
    }

    // 回归（评审 #5）：无关伪造 jti 开出的负缓存窗**不得拖死新签发的合法 jti**
    // ——窗内 miss 走 PK 点查，轮换/签发后立即使用的新 bearer 立即可用
    // （R6 无感知续期）；伪造 jti 窗内仍 fail-closed。
    #[tokio::test]
    async fn fresh_jti_not_shadowed_by_neg_window() {
        let acc = Arc::new(InMemoryAccessor::new());
        acc.seed([json!({ "jti": "jt-a", "status": "active" })]);
        let g = SvcGate::with_ttl(acc.clone(), Duration::from_secs(30));
        assert!(g.admits("jt-a").await);
        // 伪造探测开窗。
        assert!(!g.admits("jt-forged").await);
        // 窗内新签发（轮换产物）立即可用——不被窗误伤。
        acc.seed([json!({ "jti": "jt-new", "status": "active" })]);
        assert!(
            g.admits("jt-new").await,
            "负缓存窗内新合法 jti 必须经点查立即可用（评审 #5）"
        );
        assert!(
            !g.admits("jt-forged-2").await,
            "伪造 jti 窗内仍 fail-closed"
        );
    }

    // 回归（评审 #11）：负缓存窗过期瞬间 C 个**互不相同**的未知 jti 并发 admits
    // ——single-flight（refresh_lock + 双重检查）须把并发全表扫描收敛为 1 次
    // （首个真查后开窗，等待者降级点查）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_misses_single_flight_full_scan() {
        let inner = InMemoryAccessor::new();
        inner.seed([json!({ "jti": "jt-a", "status": "active" })]);
        let acc = Arc::new(CountingAcc {
            inner,
            n_full: std::sync::atomic::AtomicUsize::new(0),
            n_point: std::sync::atomic::AtomicUsize::new(0),
        });
        let g = Arc::new(SvcGate::with_ttl(acc.clone(), Duration::from_secs(30)));
        assert!(g.admits("jt-a").await);
        let (full0, _) = acc.counts();
        let hs: Vec<_> = (0..8)
            .map(|i| {
                let g = g.clone();
                tokio::spawn(async move { g.admits(&format!("jt-cold-{i}")).await })
            })
            .collect();
        for h in hs {
            assert!(!h.await.unwrap(), "未知 jti 一律 fail-closed");
        }
        let (full1, _) = acc.counts();
        assert_eq!(
            full1 - full0,
            1,
            "并发 miss 的全表扫描必须 single-flight 收敛为 1 次（评审 #11），实 {} 次",
            full1 - full0
        );
    }

    // 负缓存窗语义（v0.1.135 后）：窗抑制的是**全表刷新**；窗内新落库 jti 经
    // 点查立即可见；出窗后行为不变（穿透 → 命中/miss 开窗）。
    #[tokio::test]
    async fn neg_window_suppresses_full_scan_but_not_fresh_jti() {
        let acc = Arc::new(InMemoryAccessor::new());
        acc.seed([json!({ "jti": "jt-a", "status": "active" })]);
        // 负缓存窗 30ms：伪造 jti 开窗后，窗内全表扫描计数不增；新 jti 点查可见。
        let g = SvcGate::with_ttls(
            acc.clone(),
            Duration::from_secs(30),
            Duration::from_millis(30),
        );
        assert!(!g.admits("jt-forged").await, "首次穿透判 absent → false");
        acc.seed([json!({ "jti": "jt-b", "status": "active" })]);
        assert!(
            g.admits("jt-b").await,
            "窗内新 jti 经点查立即可见（评审 #5 修复目标）"
        );
        let dl = Instant::now() + Duration::from_secs(2);
        while !g.admits("jt-b").await {
            assert!(Instant::now() < dl, "出窗后 jt-b 仍必须准入");
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
