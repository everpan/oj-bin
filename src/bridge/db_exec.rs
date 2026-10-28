//! U-5 执行登记与跨请求取消（`db.cancel`，v0.1.96）。
//!
//! # 为什么
//!
//! `db.stream` 的取消（`AbortSignal` → `op_db_stream_abort`）只能被**同一个请求**触发
//! （`ReqState.db_streams` 是每请求的）。dbx Web 的主旅程是「请求 A 发起长导出、
//! 请求 B 点停止」——需要一个进程级的执行登记表，让 `db.cancel(executionId)` 跨请求可达。
//!
//! # 结构（复用而非新造）
//!
//! 本模块**只做路由**：`exec_<boot>_<hex>` id → [`db::DbStreamShared`]（既有流共享句柄）。
//! 取消 = 置 `cancelled` 标志 + `notify` 既有 pump；后端真取消链**原样复用**——
//! pump 停止拉取后 drop 流，`StreamCursorGuard::drop`（ffi.rs）自动 fire 插件
//! `stream_cancel`（mysql `KILL QUERY` / pg `pg_cancel_backend`）+ `stream_close`（连接回池）。
//! 不动 ABI、不动插件。
//!
//! # 范围裁决
//!
//! 只登记**流式**执行。非流式 `db.query/exec` 没有后端句柄，宿主侧只能丢 future
//! （连接中途报废、服务端查询照跑）= 假取消，不做；已知限制记入 db-guide。
//!
//! # 安全红线（与 `db_session` 同款）
//!
//! - id 是 bearer token：日志/错误只出 [`id_prefix`]（前 8 字符）。
//! - 他人 id / 未知 id / 畸形 id / 已收尾 id → **同一条** not-found 文案。
//! - 判权只用 `Owner::same_subject`（tenant+user）；无配置门（流本身已过 `db_access`
//!   逐语句授权），匿名同域可互消（与匿名请求同信任域）。

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use deno_core::{OpState, op2};
use deno_error::JsErrorBox;
use tokio::time::Instant;

use super::StableState;
use super::db::DbStreamShared;
use super::handle_core::{self, Owner};

/// id 前缀（形状白名单与日志遮蔽共用）。
pub const PREFIX: &str = "exec_";

/// entry 硬寿命。到期只摘登记（流本身不受影响，仅失去可取消性）——流式执行的
/// 人为时长远低于此；无后台 reaper，由 register/cancel 顺带懒惰清扫。
const EXEC_TTL: Duration = Duration::from_secs(24 * 60 * 60);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // 毒化不放大成二次 panic：状态本身是「表」，取回即可继续（与 db_session 同裁决）。
    m.lock().unwrap_or_else(|e| e.into_inner())
}

struct ExecEntry {
    owner: Owner,
    alias: String,
    shared: Arc<DbStreamShared>,
    created: Instant,
}

/// 执行登记表：壳 `Arc` 不可变（挂在 `StableState`）、内部 `std::sync::Mutex` 变动。
/// 进程级生命周期：热更沿用同一 `StableState`（`app::reuse_backend` 只 clone `stable`），
/// 存活流的取消通道跨热更有效。
#[derive(Default)]
pub struct ExecRegistry {
    inner: Mutex<HashMap<String, ExecEntry>>,
    boot: OnceLock<String>,
}

impl ExecRegistry {
    fn boot(&self) -> String {
        self.boot.get_or_init(handle_core::new_boot_tag).clone()
    }

    /// 登记（`op_db_stream_open` 开流成功后调用）。顺带懒惰清扫：已 done 的流与
    /// 超龄 entry（孤儿回收——JS 永不 close 的流不能无限占表）。
    pub(crate) fn register(
        &self,
        owner: Owner,
        alias: String,
        shared: Arc<DbStreamShared>,
    ) -> Result<String, String> {
        // getrandom 失败 → Err，绝不退回可枚举 id（handle_core 红线）。
        let id = handle_core::new_id(PREFIX, &self.boot())?;
        let now = Instant::now();
        {
            let mut g = lock(&self.inner);
            g.retain(|_, e| {
                !e.shared.done.load(Ordering::SeqCst)
                    && now.saturating_duration_since(e.created) < EXEC_TTL
            });
            g.insert(
                id.clone(),
                ExecEntry {
                    owner,
                    alias,
                    shared,
                    created: now,
                },
            );
        }
        Ok(id)
    }

    /// 跨请求取消：归属校验 → 置 `cancelled` 标志 + 唤醒 pump。
    /// 幂等：同一 id 重复 cancel 恒 `Some`（entry 只被 close/清扫摘除，不因 cancel 摘除）。
    /// `None` = 未知/他人/畸形/已收尾——调用方统一 not-found 文案（不泄露存在性）。
    pub(crate) fn cancel(&self, id: &str, owner: &Owner) -> Option<String> {
        if !handle_core::id_is_wellformed(PREFIX, id) {
            return None;
        }
        let g = lock(&self.inner);
        let e = g.get(id)?;
        if !e.owner.same_subject(owner) {
            return None;
        }
        e.shared.cancelled.store(true, Ordering::SeqCst);
        e.shared.abort.notify_one();
        Some(e.alias.clone())
    }

    /// 摘除（`op_db_stream_close` 调；cancel **不**摘——幂等靠它）。
    pub(crate) fn remove(&self, id: &str) {
        lock(&self.inner).remove(id);
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        lock(&self.inner).len()
    }
}

/// db.cancel（U-5）：按执行 id 跨请求取消流式查询。
///
/// 成功返回 `{cancelled: true, alias}`（重复 cancel 同值，幂等）；失败统一 not-found
/// （文案不含完整 id，`id_prefix` 遮蔽）。
#[op2]
#[serde]
pub fn op_db_cancel(
    state: &mut OpState,
    #[string] id: String,
) -> Result<serde_json::Value, JsErrorBox> {
    let reg = state.borrow::<Arc<StableState>>().exec_sessions.clone();
    let owner = super::db_session::owner_of(state);
    match reg.cancel(&id, &owner) {
        Some(alias) => Ok(serde_json::json!({ "cancelled": true, "alias": alias })),
        None => Err(JsErrorBox::generic(format!(
            "execution {} not found (unknown, finished, or owned by another caller)",
            handle_core::id_prefix(PREFIX, &id)
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU32};

    use crate::bridge::{Bridge, DataAccessor, Extras, InMemoryKV, RequestInfo, SchemaRegistry};
    use serde_json::{Value, json};

    fn shared(alias: &str) -> Arc<DbStreamShared> {
        let (_, rx) = tokio::sync::mpsc::channel(1);
        Arc::new(DbStreamShared {
            rx: tokio::sync::Mutex::new(rx),
            abort: Arc::new(tokio::sync::Notify::new()),
            alias: alias.to_string(),
            max_rows: None,
            delivered: AtomicU32::new(0),
            done: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            exec_id: OnceLock::new(),
        })
    }

    fn owner(t: Option<&str>, u: Option<&str>) -> Owner {
        Owner {
            tenant: t.map(|s| s.to_string()),
            user: u.map(|s| s.to_string()),
            module: None,
            source: "http",
        }
    }

    #[test]
    fn cancel_by_same_owner_is_idempotent_and_sets_flag() {
        let reg = ExecRegistry::default();
        let s = shared("d1");
        let id = reg
            .register(owner(Some("t"), Some("u")), "d1".into(), s.clone())
            .unwrap();
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.cancel(&id, &owner(Some("t"), Some("u"))).unwrap(), "d1");
        assert!(s.cancelled.load(Ordering::SeqCst));
        // 重复 cancel：仍 Ok（entry 不因 cancel 摘除），flag 保持
        assert_eq!(reg.cancel(&id, &owner(Some("t"), Some("u"))).unwrap(), "d1");
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn cancel_by_other_subject_is_not_found_and_touches_nothing() {
        let reg = ExecRegistry::default();
        let s = shared("d1");
        let id = reg
            .register(owner(Some("t"), Some("u")), "d1".into(), s.clone())
            .unwrap();
        // 异租户 / 异用户 / 匿名 一律 not-found，且**不得**触碰 A 的流
        for o in [
            owner(Some("t2"), Some("u")),
            owner(Some("t"), Some("u2")),
            owner(None, None),
        ] {
            assert!(reg.cancel(&id, &o).is_none(), "{o:?}");
        }
        assert!(!s.cancelled.load(Ordering::SeqCst));
    }

    #[test]
    fn malformed_and_unknown_and_removed_ids_share_the_same_fate() {
        let reg = ExecRegistry::default();
        let id = reg
            .register(owner(Some("t"), Some("u")), "d1".into(), shared("d1"))
            .unwrap();
        for bad in [
            "exec_ab12_00000000000000000000000000000000", // 未知但形状合法
            "exec_粗00_00000000000000000000000000000000", // 畸形
            "tx_ab12_00000000000000000000000000000000",   // 别轴前缀
        ] {
            assert!(
                reg.cancel(bad, &owner(Some("t"), Some("u"))).is_none(),
                "{bad}"
            );
        }
        // close 后再 cancel：同文案（登记单次性）
        reg.remove(&id);
        assert!(reg.cancel(&id, &owner(Some("t"), Some("u"))).is_none());
    }

    /// 孤儿回收：done 的流与超龄 entry 在下一次 register 时被懒惰清扫。
    #[test]
    fn register_sweeps_done_and_expired_entries() {
        let reg = ExecRegistry::default();
        let a = shared("d1");
        let _ = reg
            .register(owner(Some("t"), Some("u")), "d1".into(), a.clone())
            .unwrap();
        // done 的流：已不可取消，登记即摘
        a.done.store(true, Ordering::SeqCst);
        let b = shared("d1");
        let _ = reg
            .register(owner(Some("t"), Some("u")), "d1".into(), b)
            .unwrap();
        assert_eq!(reg.len(), 1, "done entry swept");
        // 超龄 entry：回拨 created 后被下一次 register 摘除
        {
            let mut g = lock(&reg.inner);
            for e in g.values_mut() {
                e.created = Instant::now() - EXEC_TTL - Duration::from_secs(1);
            }
        }
        let _ = reg
            .register(owner(Some("t"), Some("u")), "d1".into(), shared("d1"))
            .unwrap();
        assert_eq!(reg.len(), 1, "expired entry swept");
    }

    // -------------------------------------------------------------------
    // Bridge 级（请求边界）：一次 `run` = 一个请求（与 db_session::tests 同款骨架）。
    // -------------------------------------------------------------------

    /// 单库桥（temp sqlite，2 行数据）。**不用 `:memory:`**（坑⑩：每连接一个独立库）。
    async fn exec_bridge(tag: &str) -> Bridge {
        let p = std::env::temp_dir().join(format!("oj-exec-{}-{tag}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        // sqlx 不代建文件库：先落空文件。
        std::fs::File::create(&p).unwrap();
        let db = crate::bridge::SqlxAccessor::arc(&format!("sqlite://{}", p.display()))
            .await
            .unwrap();
        db.exec_with_params("create table t (id integer primary key)", &[])
            .await
            .unwrap();
        db.exec_with_params("insert into t (id) values (1), (2)", &[])
            .await
            .unwrap();
        let mut dbs = std::collections::HashMap::new();
        dbs.insert("default".to_string(), db as Arc<dyn DataAccessor>);
        Bridge::with_dbs_and_loader(
            dbs,
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            None,
            Extras::default(),
        )
    }

    async fn run(b: &Bridge, src: &str) -> Value {
        let cap = b.run(src).await.unwrap();
        serde_json::from_slice(&cap.body).unwrap()
    }

    async fn run_as(b: &Bridge, user: Option<Value>, src: &str) -> Value {
        let req = RequestInfo {
            user,
            ..RequestInfo::default()
        };
        let cap = b.run_with(src, req).await.unwrap();
        serde_json::from_slice(&cap.body).unwrap()
    }

    /// 取消语义落在**请求边界**上：同一请求内开流 → cancel → 下一次 `next` 必须抛
    /// cancelled（而非静默 done——消费方不得误以为数据取完）；错误文案只含 id 前缀。
    #[tokio::test(flavor = "current_thread")]
    async fn cancel_then_next_throws_cancelled_envelope() {
        let b = exec_bridge("env").await;
        let v = run(
            &b,
            r#"(async () => {
              const it = db.stream("select id from t", []);
              const xid = await it.executionId();
              let gotFirst = false;
              for await (const row of it) { gotFirst = true; break; }
              let cancelErr = null;
              try { await db.cancel(xid); } catch (e) { cancelErr = String(e); }
              let afterErr = null;
              try { for await (const row of it) { void row; } } catch (e) { afterErr = String(e); }
              return json.ok({ xid, xidOk: typeof xid === "string" && xid.startsWith("exec_"),
                               gotFirst, cancelErr, afterErr });
            })()"#,
        )
        .await;
        assert_eq!(v["data"]["xidOk"], json!(true), "{v}");
        assert_eq!(v["data"]["gotFirst"], json!(true), "{v}");
        assert!(v["data"]["cancelErr"].is_null(), "{v}");
        let after = v["data"]["afterErr"]
            .as_str()
            .unwrap_or_else(|| panic!("no afterErr: {v}"));
        assert!(after.contains("cancelled via db.cancel"), "{after}");
        // bearer token 红线：错误里只见前 8 字符前缀，完整 id 不可见
        let xid = v["data"]["xid"].as_str().unwrap();
        assert!(after.contains(&xid[..8]), "{after}");
        assert!(!after.contains(xid), "{after}");
    }

    /// 跨请求可达：请求 A 开流不 close → 请求结束登记仍在；请求 B 同主体 cancel
    /// 幂等成功；异主体拿同一 id 得 not-found，且文案不含完整 id。
    #[tokio::test(flavor = "current_thread")]
    async fn execution_reaches_across_requests_and_foreign_subject_is_not_found() {
        let b = exec_bridge("xreq").await;
        let v = run(
            &b,
            r#"(async () => {
              const it = db.stream("select id from t", []);
              const xid = await it.executionId();
              let n = 0;
              for await (const row of it) { n++; if (n >= 1) break; }
              return json.ok({ xid });
            })()"#,
        )
        .await;
        let xid = v["data"]["xid"].as_str().unwrap().to_string();
        let cancel_js = format!(
            "db.cancel({xid:?}).then((r) => json.ok(r)).catch((e) => json.fail(500, String(e)))"
        );
        // 请求 B（匿名同域）：cancel → Ok，重复 cancel → 仍 Ok（幂等）
        let v2 = run(&b, &cancel_js).await;
        assert_eq!(v2["data"]["cancelled"], json!(true), "{v2}");
        let v3 = run(&b, &cancel_js).await;
        assert_eq!(v3["data"]["cancelled"], json!(true), "{v3}");
        // 异主体：not-found，前缀可见、完整 id 不可见
        let v4 = run_as(&b, Some(json!({"id": "mallory"})), &cancel_js).await;
        let msg = v4["msg"].as_str().unwrap_or_else(|| panic!("no msg: {v4}"));
        assert!(msg.contains("not found"), "{msg}");
        assert!(msg.contains(&xid[..8]), "{msg}");
        assert!(!msg.contains(&xid), "{msg}");
    }

    /// U-14 批 a：catalog 信封 / sqlite 矩阵（views ✓ ddl ✓ 其余 unsupported）/
    /// 未知 kind 报错 / 畸形表名空结果（与 L2 同口径）。
    #[tokio::test(flavor = "current_thread")]
    async fn catalog_envelope_sqlite_matrix_and_unsupported() {
        let b = exec_bridge("catalog").await;
        let v = run(
            &b,
            r#"(async () => {
              await db.exec("create view v_t as select id from t");
              const views = await db.catalog("views");
              const ddl = await db.catalog("ddl", "v_t");
              let unsup = null, unsupCap = null;
              try { await db.catalog("sequences"); } catch (e) { unsup = String(e); unsupCap = e.data && e.data.capability; }
              let unknown = null;
              try { await db.catalog("nope"); } catch (e) { unknown = String(e); }
              let badTable = null;
              try { await db.catalog("ddl", "x; drop"); } catch (e) { badTable = String(e); }
              return json.ok({ kind: views.kind, dialect: views.dialect,
                               itemName: views.items[0] && views.items[0].name,
                               ddlHasView: String(ddl.items[0].ddl).includes("VIEW"),
                               unsup, unsupCap, unknown, badTable });
            })().catch((e) => json.fail(500, String(e)))"#,
        )
        .await;
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(v["data"]["kind"], json!("views"), "{v}");
        assert_eq!(v["data"]["dialect"], json!("sqlite"), "{v}");
        assert_eq!(v["data"]["itemName"], json!("v_t"), "{v}");
        assert_eq!(v["data"]["ddlHasView"], json!(true), "{v}");
        let unsup = v["data"]["unsup"].as_str().unwrap_or_default();
        assert!(unsup.contains("not supported"), "{v}");
        assert_eq!(v["data"]["unsupCap"], json!("sequences"), "{v}");
        let unknown = v["data"]["unknown"].as_str().unwrap_or_default();
        assert!(unknown.contains("unknown kind"), "{v}");
        assert_eq!(v["data"]["badTable"], json!(null), "{v}");
    }
}
