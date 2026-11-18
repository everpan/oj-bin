//! REQ-CAP-05a（v0.1.139）：两阶段压实「读者隔离」契约的三方言测试。
//!
//! 场景 79 配方的存储层契约（方言无关，参考实现随测试内嵌）：
//! - `collab_log(doc_key, seq PK, upd, created_at)`——created_at = 写入纪元毫秒；
//! - `collab_snap(doc_key PK, upto_seq, bytes)`——单行快照指针；
//! - **tx2 删除谓词**：`DELETE WHERE seq <= 本快照.upto_seq AND created_at < now - 宽限`
//!   ——「永不删高于自己 upto_seq 的行」+「宽限保护读窗口 < 宽限的读者」。
//!
//! 验证口径（细化稿 L227）：压实进行中并发读快照+日志重建无损；
//! 负对照：无宽限谓词时读者拿到日志空洞——钉死宽限项是承重的，防后续「简化」。
//!
//! sqlite 腿常驻（内存库）；PG/MySQL 腿 env-gated（OJ_TEST_PG / OJ_TEST_MYSQL，CI 恒 skip）。

use serde_json::Value;
use std::sync::Arc;

use super::DataAccessor;
use crate::bridge::SqlxAccessor;

/// 宽限窗（配方缺省 60s）：tx2 只删「比它更老」的已合并行。
const GRACE_MS: i64 = 60_000;

/// sqlx::Any 不做占位符方言翻译（PG 收到 `?` 会当 jsonb 操作符 → 语法错），
/// 参考实现按方言生成：PG 用 `$n`，sqlite/mysql 用 `?`（与平台查询构造器同口径）。
fn mark(kind: &str, n: usize) -> String {
    match kind {
        "pg" => format!("${n}"),
        _ => "?".to_string(),
    }
}

struct Recipe {
    kind: &'static str,
    db: Arc<dyn DataAccessor>,
    // 每用例独立表名：三用例并行跑同一持久库（PG/MySQL）时不互抢 catalog。
    log_tbl: String,
    snap_tbl: String,
}

impl Recipe {
    async fn new(kind: &'static str, dsn: &str, tag: &str) -> Self {
        Self {
            kind,
            db: SqlxAccessor::arc(dsn).await.unwrap(),
            log_tbl: format!("collab_log_{tag}"),
            snap_tbl: format!("collab_snap_{tag}"),
        }
    }

    async fn setup(&self, doc: &str) {
        // 清场：PG/MySQL 是持久库，上一轮失败可能残留表；sqlite 内存库无害。
        let (log, snap) = (&self.log_tbl, &self.snap_tbl);
        self.db
            .exec_with_params(&format!("DROP TABLE IF EXISTS {snap}"), &[])
            .await
            .unwrap();
        self.db
            .exec_with_params(&format!("DROP TABLE IF EXISTS {log}"), &[])
            .await
            .unwrap();
        let seq_ty = match self.kind {
            "mysql" => "BIGINT",
            _ => "INTEGER",
        };
        // doc_key 用 VARCHAR(255)：MySQL 的 TEXT 列进索引必须给键长，其余方言通用。
        self.db
            .exec_with_params(
                &format!(
                    "CREATE TABLE {log} (doc_key VARCHAR(255) NOT NULL, seq {seq_ty} PRIMARY KEY, upd TEXT NOT NULL, created_at BIGINT NOT NULL)"
                ),
                &[],
            )
            .await
            .unwrap();
        self.db
            .exec_with_params(
                &format!(
                    "CREATE TABLE {snap} (doc_key VARCHAR(255) NOT NULL PRIMARY KEY, upto_seq {seq_ty} NOT NULL, bytes TEXT NOT NULL)"
                ),
                &[],
            )
            .await
            .unwrap();
        let _ = doc; // sqlite 内存库每用例全新；PG/MySQL 靠用例键隔离
    }

    /// 种行：(seq, created_at_ms, upd)。
    async fn seed(&self, doc: &str, rows: &[(i64, i64, &str)]) {
        for (seq, ts, upd) in rows {
            self.db
                .exec_with_params(
                    &format!(
                        "INSERT INTO {} (doc_key, seq, upd, created_at) VALUES ({})",
                        self.log_tbl,
                        [
                            mark(self.kind, 1),
                            mark(self.kind, 2),
                            mark(self.kind, 3),
                            mark(self.kind, 4)
                        ]
                        .join(", ")
                    ),
                    &[
                        Value::from(doc),
                        Value::from(*seq),
                        Value::from(*upd),
                        Value::from(*ts),
                    ],
                )
                .await
                .unwrap();
        }
    }

    async fn read_snapshot(&self, doc: &str) -> Option<i64> {
        let rows = self
            .db
            .query_with_params(
                &format!(
                    "SELECT upto_seq FROM {} WHERE doc_key = {}",
                    self.snap_tbl,
                    mark(self.kind, 1)
                ),
                &[Value::from(doc)],
            )
            .await
            .unwrap();
        rows.first().map(|r| r["upto_seq"].as_i64().unwrap())
    }

    /// readUpdatesSince 配方分页：按 seq 升序取 (seq, upd)。
    async fn read_updates_since(&self, doc: &str, since: i64, limit: i64) -> Vec<(i64, String)> {
        self.db
            .query_with_params(
                &format!(
                    "SELECT seq, upd FROM {} WHERE doc_key = {} AND seq > {} ORDER BY seq LIMIT {}",
                    self.log_tbl,
                    mark(self.kind, 1),
                    mark(self.kind, 2),
                    mark(self.kind, 3)
                ),
                &[Value::from(doc), Value::from(since), Value::from(limit)],
            )
            .await
            .unwrap()
            .into_iter()
            .map(|r| {
                (
                    r["seq"].as_i64().unwrap(),
                    r["upd"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    /// 两阶段压实参考实现：tx1 upsert 快照指针；tx2 宽限谓词删除。
    async fn compact(&self, doc: &str, upto_seq: i64, now_ms: i64) {
        let upsert = match self.kind {
            "mysql" => format!(
                "INSERT INTO {} (doc_key, upto_seq, bytes) VALUES ({}, 'snap') ON DUPLICATE KEY UPDATE upto_seq = VALUES(upto_seq)",
                self.snap_tbl,
                [mark(self.kind, 1), mark(self.kind, 2)].join(", ")
            ),
            _ => format!(
                "INSERT INTO {} (doc_key, upto_seq, bytes) VALUES ({}, 'snap') ON CONFLICT(doc_key) DO UPDATE SET upto_seq = excluded.upto_seq",
                self.snap_tbl,
                [mark(self.kind, 1), mark(self.kind, 2)].join(", ")
            ),
        };
        self.db
            .exec_with_params(&upsert, &[Value::from(doc), Value::from(upto_seq)])
            .await
            .unwrap();
        self.db
            .exec_with_params(
                &format!(
                    "DELETE FROM {} WHERE doc_key = {} AND seq <= {} AND created_at < {}",
                    self.log_tbl,
                    mark(self.kind, 1),
                    mark(self.kind, 2),
                    mark(self.kind, 3)
                ),
                &[
                    Value::from(doc),
                    Value::from(upto_seq),
                    Value::from(now_ms - GRACE_MS),
                ],
            )
            .await
            .unwrap();
    }

    /// 负对照：无宽限项的「天真」删除（配方禁止的写法）。
    async fn compact_unsafe(&self, doc: &str, upto_seq: i64) {
        self.db
            .exec_with_params(
                &format!(
                    "DELETE FROM {} WHERE doc_key = {} AND seq <= {}",
                    self.log_tbl,
                    mark(self.kind, 1),
                    mark(self.kind, 2)
                ),
                &[Value::from(doc), Value::from(upto_seq)],
            )
            .await
            .unwrap();
    }
}

/// 方言矩阵：sqlite 常驻；PG/MySQL env-gated。
async fn dialects() -> Vec<(&'static str, String)> {
    let mut v: Vec<(&'static str, String)> = vec![("sqlite", "sqlite::memory:".to_string())];
    for (env, kind) in [("OJ_TEST_PG", "pg"), ("OJ_TEST_MYSQL", "mysql")] {
        match std::env::var(env) {
            Ok(dsn) if !dsn.is_empty() => v.push((kind, dsn)),
            _ => eprintln!(
                "skip leg: {env} unset（{kind} 腿本机容器可跑，见 memory local-test-db-containers）"
            ),
        }
    }
    v
}

/// 读者先取快照（K=0）后分页重放，压实恰在其间发生——宽限谓词下行存留，
/// 重建覆盖 (K, N] 全部 seq，无空洞（三方言）。
#[tokio::test(flavor = "current_thread")]
async fn given_reader_paging_during_compaction_then_rebuild_has_no_hole() {
    for (kind, dsn) in dialects().await {
        let tag = "hole";
        let r = Recipe::new(kind, &dsn, tag).await;
        let doc = format!("rd-hole-{kind}");
        r.setup(&doc).await;
        let now = 1_700_000_000_000i64;
        let rows: Vec<(i64, i64, &str)> = (1..=10).map(|s| (s, now, "u")).collect();
        r.seed(&doc, &rows).await;

        // 读者：快照（尚无 → K=0）；压实者恰在此间隙完成两阶段。
        let k = r.read_snapshot(&doc).await.unwrap_or(0);
        r.compact(&doc, 10, now).await;

        // 读者分页重放（limit=3 模拟慢读）。
        let mut got = Vec::new();
        let mut cursor = k;
        loop {
            let page = r.read_updates_since(&doc, cursor, 3).await;
            if page.is_empty() {
                break;
            }
            cursor = page.last().unwrap().0;
            got.extend(page.into_iter().map(|(s, _)| s));
        }
        assert_eq!(
            got,
            (k + 1..=10).collect::<Vec<_>>(),
            "{kind}: 重建不得有空洞"
        );
    }
}

/// 负对照（配方禁止的写法）：tx2 无宽限项直接删——读者窗口内 (K, upto] 全灭，
/// 重建出洞。钉死「宽限项承重」，防实现滑回单谓词删除。
#[tokio::test(flavor = "current_thread")]
async fn given_delete_without_grace_when_compaction_then_reader_window_yields_hole() {
    for (kind, dsn) in dialects().await {
        let tag = "unsafe";
        let r = Recipe::new(kind, &dsn, tag).await;
        let doc = format!("rd-unsafe-{kind}");
        r.setup(&doc).await;
        let now = 1_700_000_000_000i64;
        let rows: Vec<(i64, i64, &str)> = (1..=10).map(|s| (s, now, "u")).collect();
        r.seed(&doc, &rows).await;

        let k = r.read_snapshot(&doc).await.unwrap_or(0);
        r.compact_unsafe(&doc, 10).await;
        let got = r.read_updates_since(&doc, k, 100).await;
        assert!(
            got.is_empty(),
            "{kind}: 无宽限谓词必产生日志空洞（这正是配方禁止它的原因）"
        );
    }
}

/// tx2 契约：永不删高于自己 upto_seq 的行——即使那些行已老过宽限
/// （快照指针之前的行按宽限正常清走）。
#[tokio::test(flavor = "current_thread")]
async fn given_tx2_with_grace_when_compact_then_never_deletes_above_upto_seq() {
    for (kind, dsn) in dialects().await {
        let tag = "bound";
        let r = Recipe::new(kind, &dsn, tag).await;
        let doc = format!("rd-bound-{kind}");
        r.setup(&doc).await;
        let now = 1_700_000_000_000i64;
        let old = now - 2 * GRACE_MS; // 全部老过宽限
        let rows: Vec<(i64, i64, &str)> = (1..=10).map(|s| (s, old, "u")).collect();
        r.seed(&doc, &rows).await;

        r.compact(&doc, 5, now).await; // 快照只推进到 5

        assert_eq!(r.read_snapshot(&doc).await, Some(5), "{kind}");
        let left = r.read_updates_since(&doc, 0, 100).await;
        let seqs: Vec<i64> = left.iter().map(|(s, _)| *s).collect();
        assert_eq!(
            seqs,
            (6..=10).collect::<Vec<_>>(),
            "{kind}: 高于 upto_seq 的行必须幸存"
        );
    }
}
