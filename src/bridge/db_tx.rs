//! db.tx 事务 op（从 db.rs 拆分，纯移动零行为变更）：begin/commit/rollback +
//! take_tx/on_tx 助手（on_tx 按会话剩余寿命封顶——评审 S5）。
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use deno_core::{OpState, op2};
use deno_error::JsErrorBox;

use super::db::{ActiveTx, current_tx, to_js_err};
use crate::bridge::BridgeResult;

pub(crate) async fn on_tx<T, F>(state: &Rc<RefCell<OpState>>, what: &str, f: F) -> BridgeResult<T>
where
    F: std::future::Future<Output = BridgeResult<T>>,
{
    match super::db_session::attached_deadline(state) {
        Some(left) => match tokio::time::timeout(left, f).await {
            Ok(r) => r,
            Err(_) => {
                super::db_session::kill_attached(state, "lifetime_exceeded");
                Err(format!(
                    "db session lifetime exhausted during {what}; the session was discarded \
                     (transaction rolled back)"
                )
                .into())
            }
        },
        None => f.await,
    }
}

/// 占位符统一改写（0.1.67，design docs/plans/2026-10-13-unified-db-placeholder-design.md）：
/// `?` 是 oj 唯一占位符，postgres 方言由宿主改写为 `$n`；sqlite/mysql/空参透传零开销。
/// 三个裸 SQL op（query/exec/stream_open）的唯一插桩点（DRY）；守护测试钉死出口白名单。
/// fail-loud（计数不匹配）同样记 trace（ok=false），不留画像盲区（评审 E6）；
/// trace 一律记**原始** SQL（画像定位开发者所写，非引擎所收）。
#[allow(clippy::too_many_arguments)]
/// 取走并校验活跃事务（commit/rollback 收尾用；不匹配/缺失报错）。
pub(crate) fn take_tx(
    state: &Rc<RefCell<OpState>>,
    name: &str,
) -> Result<Arc<ActiveTx>, JsErrorBox> {
    let mut g = state.borrow_mut();
    let rs = g.borrow_mut::<super::ReqState>();
    match rs.tx.take() {
        Some(t) if t.db == name => Ok(t),
        Some(t) => {
            rs.tx = Some(t.clone()); // 放回（别人的事务不动）
            Err(JsErrorBox::generic(format!(
                "transaction belongs to db '{}', not '{name}'",
                t.db
            )))
        }
        None => Err(JsErrorBox::generic("no active transaction")),
    }
}

/// db.tx 开始（JS wrapper 调用）：嵌套（已有活跃事务）报错。
#[op2]
pub async fn op_db_tx_begin(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
) -> Result<bool, JsErrorBox> {
    if current_tx(&state).is_some() {
        return Err(JsErrorBox::generic(
            "transaction already active (nested tx not supported)",
        ));
    }
    if state.borrow().borrow::<super::ReqState>().session.is_some() {
        return Err(JsErrorBox::generic(
            "a db session is attached to this request; db.tx cannot nest inside it (detach first)",
        ));
    }
    let da = super::query::lookup(&state, &name)?;
    let session = da.begin().await.map_err(to_js_err)?;
    // 方言快照进 ActiveTx（而非 TxSession trait 加方法 / FFI ABI 演进）：方言是
    // accessor 静态属性，begin 时可从 da.dialect_info() 拿到；前者要改 SqlxTx/InMemoryTx
    // 及全部夹具，后者要 bump ABI_VERSION（严格相等门禁）——代价与收益不成比例。
    // 注意：**不调用 da.dialect()**（开放方言下它会 panic），只存描述符。
    state.borrow_mut().borrow_mut::<super::ReqState>().tx = Some(Arc::new(ActiveTx {
        db: name,
        session: tokio::sync::Mutex::new(session),
        dialect_info: da.dialect_info().clone(),
    }));
    Ok(true)
}

/// db.tx 提交。
#[op2]
pub async fn op_db_tx_commit(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
) -> Result<bool, JsErrorBox> {
    let t = take_tx(&state, &name)?;
    t.session.lock().await.commit().await.map_err(to_js_err)?;
    Ok(true)
}

/// db.tx 回滚。
#[op2]
pub async fn op_db_tx_rollback(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
) -> Result<bool, JsErrorBox> {
    let t = take_tx(&state, &name)?;
    t.session.lock().await.rollback().await.map_err(to_js_err)?;
    Ok(true)
}
