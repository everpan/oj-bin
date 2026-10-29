//! db.stream 流式查询 op（从 db.rs 拆分，纯移动）：open/next/info/close/abort +
//! 脱离式 pump（REQ-05：pump 字节计量 / 全程超时竞速 / info() 显式信令）。
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use deno_core::{OpState, op2};
use deno_error::JsErrorBox;
use serde_json::Value;

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use tokio::sync::{Notify, mpsc};

use super::StableState;
use super::db::{
    DB_STREAM_CHANNEL_CAP, DbStreamShared, NEXT_STREAM_ID, Row, Target, err_meta,
    normalize_and_trace, resolve_target, to_js_err, trace_sql,
};
use super::db_query::DbCallOpts;

/// db.stream 打开（PR-2 Phase A）：先 `guard::check_raw` / `check_tenant_raw`（open 前校验，同
/// `op_db_query`）→ 取 accessor → `stream_query` 开流 → spawn **脱离式 pump**（current_thread 运行时
/// 内）逐行推入通道；op 立即返回 `{id, executionId}`（后者是 U-5 跨请求取消的句柄）。
/// 插件后端（FfiDataAccessor）走默认实现报错。
#[op2]
#[serde]
pub async fn op_db_stream_open(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[string] sql: String,
    #[serde] params: Option<Vec<Value>>,
    #[serde] opts: Option<DbCallOpts>,
) -> Result<serde_json::Value, JsErrorBox> {
    let params = params.unwrap_or_default();
    super::guard::check_raw(&state, &sql)?; // 表归属守卫（§5.3，无模块上下文不设防）
    super::guard::check_tenant_raw(&state, &sql, &params)?; // 多租户防护（独立于 module_ctx）
    // db 访问控制（v0.1.89）：与 query/exec 同一入口；行数上限走「提前终止」而非截断。
    let alias = super::guard::bound_db(&state, &name);
    let row_cap = super::db_policy::check_access(&state, &alias, &sql)?;
    // REQ-05：语句超时（open→pump 全程；timeoutMs 只能收紧）+ 行级字节计量档位。
    let quota = super::db_policy::quota_for(&state, &alias);
    let timeout_ms = super::db_quota::effective_timeout_ms(
        quota.statement_timeout_ms,
        opts.and_then(|o| o.timeout_ms),
    )
    .map_err(JsErrorBox::from)?;
    let byte_limit = quota.max_result_bytes;
    let on_exceed = quota.on_exceed;
    let da = match resolve_target(&state, &name)? {
        Target::Pool(da) => da,
        Target::Tx(_) => {
            return Err(JsErrorBox::generic(
                "db.stream within an active transaction is not supported (streaming queries run on the connection pool only)",
            ));
        }
    };
    // U-7：动态库全局流预算——开流前占名额（满了拒，不排队）；permit 由 pump task 持有，
    // 随流结束（自然/abort/close 任一路）释放。
    let stream_permit = {
        let st = state.borrow().borrow::<Arc<StableState>>().clone();
        if !st.dbs.contains_key(&alias) && st.client_dbs.contains(&alias) {
            match st.client_dbs.try_acquire_stream() {
                Ok(p) => Some(p),
                Err(msg) => {
                    super::client_db::deny_log(&state, "stream", "budget_exhausted", "-", &alias);
                    return Err(JsErrorBox::generic(msg.to_string()));
                }
            }
        } else {
            None
        }
    };
    // stream 明确拒绝事务，占位风格直接取 accessor（无需 ActiveTx 快照）。
    let norm = normalize_and_trace(
        &state,
        da.dialect_info().caps.placeholder,
        false,
        &name,
        &sql,
        &params,
    )?;
    let start = std::time::Instant::now();
    let stream_res = da.stream_query(&norm, &params).await;
    let (cat, retry) = err_meta(&stream_res);
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    // 流式只追踪「打开」（首行拉取开始）；整条流耗时由 `db.stream` 的逐行消费决定，未计入。
    trace_sql(
        &state,
        &sql,
        &params,
        &name,
        false,
        ms,
        None,
        stream_res.is_ok(),
        stream_res.as_ref().err().map(|e| e.to_string()),
        cat,
        retry,
    );
    let mut stream = stream_res.map_err(to_js_err)?;
    let (tx, rx) = mpsc::channel::<Result<Row, String>>(DB_STREAM_CHANNEL_CAP);
    let shared = Arc::new(DbStreamShared {
        rx: tokio::sync::Mutex::new(rx),
        abort: Arc::new(Notify::new()),
        alias: alias.clone(),
        max_rows: row_cap,
        delivered: AtomicU32::new(0),
        done: AtomicBool::new(false),
        cancelled: AtomicBool::new(false),
        truncated: AtomicBool::new(false),
        result_bytes: AtomicU64::new(0),
        exec_id: std::sync::OnceLock::new(),
    });
    let id = NEXT_STREAM_ID.fetch_add(1, Ordering::SeqCst);
    let reg = state
        .borrow()
        .borrow::<super::ReqState>()
        .db_streams
        .clone();
    reg.lock().await.insert(id, shared.clone());
    // U-5：登记进进程级执行表（`db.cancel` 的跨请求路由），id 写回共享句柄供 close 摘除。
    let exec_id = {
        let exec = state
            .borrow()
            .borrow::<Arc<StableState>>()
            .exec_sessions
            .clone();
        let owner = super::db_session::owner_of(&state.borrow());
        exec.register(owner, alias.clone(), shared.clone())
            .map_err(JsErrorBox::generic)?
    };
    let _ = shared.exec_id.set(exec_id.clone());
    // 脱离式 pump：监听流与取消信号，逐行推入通道（通道关闭 = done）。
    // **有界通道 + 阻塞投递**：消费端不拉取时 pump 停在 send 上——abort 一到即
    // 停止后端拉取（真取消）；unbounded 会把结果集提前灌满、取消扑空（探测
    // probe_stream_abort_releases_connection 抓到的回归）。
    // REQ-05：pump 全程吃语句 deadline（到点发结构化 OJ_DB_TIMEOUT 错行，不静默 done）；
    // 行级字节计量——fail 档触顶发结构化 OJ_DB_CAPACITY 错行；truncate 档置 truncated
    // 标志（info() 显式信令）后干净 done。触顶行计入 result_bytes 但不交付。
    let abort = shared.abort.clone();
    let pump_shared = shared.clone();
    tokio::spawn(async move {
        // U-7 流预算 permit：随本 task 结束（任一退出路径）自动释放。
        let _stream_permit = stream_permit;
        let byte_limit_ref = byte_limit;
        let pump = async {
            let mut meter = super::db_quota::ByteMeter::new(byte_limit_ref, on_exceed);
            use futures::StreamExt;
            loop {
                tokio::select! {
                    biased;
                    _ = abort.notified() => break,
                    item = stream.next() => {
                        match item {
                            Some(Ok(row)) => {
                                if meter.feed(&row) == super::db_quota::Feed::Stop {
                                    pump_shared.result_bytes.store(meter.counted, Ordering::SeqCst);
                                    if meter.exceeded {
                                        match on_exceed {
                                            super::db_policy::OnExceed::Fail => {
                                                let e = super::db_quota::result_too_large_error(
                                                    &alias, byte_limit_ref.unwrap_or(0), meter.counted,
                                                );
                                                let _ = tx.send(Err(super::db_quota::envelope_json(&e))).await;
                                            }
                                            super::db_policy::OnExceed::Truncate => {
                                                pump_shared.truncated.store(true, Ordering::SeqCst);
                                            }
                                        }
                                    }
                                    break;
                                }
                                if tx.send(Ok(row)).await.is_err() { break; }
                            }
                            Some(Err(e)) => { let _ = tx.send(Err(e)).await; break; }
                            None => break,
                        }
                    }
                }
            }
            // 自然结束（无触顶）也落累计字节：info().bytes 恒可用。
            if !pump_shared.truncated.load(Ordering::SeqCst) {
                pump_shared
                    .result_bytes
                    .store(meter.counted, Ordering::SeqCst);
            }
        };
        match timeout_ms {
            Some(ms) => {
                if tokio::time::timeout(std::time::Duration::from_millis(ms), pump)
                    .await
                    .is_err()
                {
                    let e = super::db_quota::stmt_timeout_error(&alias, ms, false);
                    let _ = tx.send(Err(super::db_quota::envelope_json(&e))).await;
                }
            }
            None => pump.await,
        }
        // tx 丢弃 → 接收端 recv 返回 None = 流结束（done）。abort 后缓冲中的
        // 已拉行继续交付（"流提前结束"），已交付行不回滚。
    });
    Ok(serde_json::json!({ "id": id as f64, "executionId": exec_id }))
}

/// db.stream 拉取下一行（PR-2 Phase A）：返回一行（`Value::Object`）；流结束返回 `Value::Null`
/// （JS 侧作为终止哨兵，因行本身恒为非 null 对象）；途中错误以 `JsErrorBox` 抛出（JS `await` 即抛）。
#[op2]
#[serde]
pub async fn op_db_stream_next(
    state: Rc<RefCell<OpState>>,
    stream_id: f64,
) -> Result<Row, JsErrorBox> {
    let stream_id = stream_id as u32;
    let reg = state
        .borrow()
        .borrow::<super::ReqState>()
        .db_streams
        .clone();
    let shared = reg.lock().await.get(&stream_id).cloned().ok_or_else(|| {
        JsErrorBox::generic(format!(
            "db stream {stream_id} not found (already closed or never opened)"
        ))
    })?;
    // 达上限后的终态：返回 done 而不是「再 recv 一次」（通道里可能还有缓冲行）。
    if shared.done.load(Ordering::SeqCst) {
        return Ok(Row::Null);
    }
    // U-5 跨请求取消：置位即抛错——取消必须可辨（静默 done 会让消费方误以为数据取完）。
    // recv 前检查：即使通道里还有缓冲行/已见底，取消语义优先。
    if shared.cancelled.load(Ordering::SeqCst) {
        let shown = super::handle_core::id_prefix(
            super::db_exec::PREFIX,
            shared.exec_id.get().map(|s| s.as_str()).unwrap_or(""),
        );
        return Err(JsErrorBox::generic(format!(
            "db stream {shown} was cancelled via db.cancel (cursor closed; rows already delivered are kept)"
        )));
    }
    let mut rx = shared.rx.lock().await;
    match rx.recv().await {
        Some(Ok(row)) => {
            // db 访问控制的行数上限：交付第 cap 行后即停（关闭游标、不再向后端拉取）。
            // 与 raw 的「取回后截断」不同，stream 提前终止是真省资源。
            let n = shared.delivered.fetch_add(1, Ordering::SeqCst) + 1;
            if let Some(cap) = shared.max_rows
                && n >= cap
            {
                shared.done.store(true, Ordering::SeqCst);
                drop(rx);
                super::db_policy::log_row_limit(&shared.alias, cap);
                // 只通知 pump 停止；**不摘注册表条目**——下一次 next 需经它返回 done。
                shared.abort.notify_waiters();
            }
            Ok(row)
        }
        Some(Err(e)) => {
            // REQ-05：pump 下发的结构化错行（FFI 信封 JSON）在此单点还原——
            // OJ_DB_TIMEOUT / OJ_DB_CAPACITY 保住 code/data；legacy 纯文本走 generic。
            if e.trim_start().starts_with('{') {
                Err(to_js_err(Box::new(super::db_error::parse_raw(&e))))
            } else {
                Err(JsErrorBox::generic(e))
            }
        }
        None => Ok(Row::Null), // done
    }
}

/// db.stream 运行信息（REQ-05 触顶显式信令）：已交付行数 / 累计结果集字节 /
/// 是否被 max_result_bytes 截断（truncate 档）。fail 档的触顶以中途错行表达。
#[op2]
#[serde]
pub async fn op_db_stream_info(
    state: Rc<RefCell<OpState>>,
    stream_id: f64,
) -> Result<serde_json::Value, JsErrorBox> {
    let stream_id = stream_id as u32;
    let reg = state
        .borrow()
        .borrow::<super::ReqState>()
        .db_streams
        .clone();
    let shared = reg.lock().await.get(&stream_id).cloned().ok_or_else(|| {
        JsErrorBox::generic(format!(
            "db stream {stream_id} not found (already closed or never opened)"
        ))
    })?;
    Ok(serde_json::json!({
        "rows": shared.delivered.load(Ordering::SeqCst),
        "bytes": shared.result_bytes.load(Ordering::SeqCst),
        "truncated": shared.truncated.load(Ordering::SeqCst),
    }))
}

/// db.stream 关闭（PR-2 Phase A）：触发中止并移除注册表条目（条目移除后 Arc 仍被 pump / 进行中的
/// next 持有至排空，无泄漏）；不强制 await，尽快归还。U-5：顺带摘除进程级执行登记。
#[op2]
pub async fn op_db_stream_close(state: Rc<RefCell<OpState>>, stream_id: f64) {
    let stream_id = stream_id as u32;
    let reg = state
        .borrow()
        .borrow::<super::ReqState>()
        .db_streams
        .clone();
    if let Some(s) = reg.lock().await.remove(&stream_id) {
        s.abort.notify_one();
        if let Some(xid) = s.exec_id.get() {
            state
                .borrow()
                .borrow::<Arc<StableState>>()
                .exec_sessions
                .remove(xid);
        }
    }
}

/// db.stream 主动中止（PR-2 Phase A）：由 bootstrap 在 `AbortSignal` 的 abort 事件上调用，
/// 触发 pump 中止（pump 经 `DbStreamShared.abort` 观测，回写错误终态）。
#[op2]
pub async fn op_db_stream_abort(state: Rc<RefCell<OpState>>, stream_id: f64) {
    let stream_id = stream_id as u32;
    let reg = state
        .borrow()
        .borrow::<super::ReqState>()
        .db_streams
        .clone();
    if let Some(s) = reg.lock().await.get(&stream_id) {
        s.abort.notify_one();
    }
}
