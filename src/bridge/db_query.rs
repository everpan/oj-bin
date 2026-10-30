//! db.query / db.exec 裸 SQL 出口（从 db.rs 拆分，纯移动）：REQ-05 语句级超时
//! 竞速 + max_result_bytes 行级字节计费的接线点；构造器路径经 query.rs 复用本模块原语。
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::{Mutex, atomic::AtomicBool};

use deno_core::{OpState, op2};
use deno_error::JsErrorBox;
use serde_json::Value;
use tokio::sync::Notify;

use super::db::{
    PoolExecShared, PoolOutcome, QueryTakeCtx, Row, Target, err_meta, normalize_and_trace,
    resolve_target, to_js_err, trace_sql,
};
use super::db_error::{BridgeError, DbErrorCategory, SessionDisposition};
use crate::bridge::BridgeResult;
use crate::bridge::DataAccessor;
use crate::bridge::db_tx::on_tx;

/// db.query/db.exec 的每调用侧选项（REQ-05 §4.1）。
#[derive(Debug, Clone, Copy, Default, serde::Deserialize)]
pub struct DbCallOpts {
    /// 语句级超时毫秒；只能收紧配置 `statement_timeout_ms`，传更大值结构化拒（点名键）。
    #[serde(rename = "timeoutMs", default)]
    pub timeout_ms: Option<u64>,
}

/// db.query(sql, params?)：Promise<Row[]>。params 可选（无参便捷形式）。
#[op2]
#[serde]
pub async fn op_db_query(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[string] sql: String,
    #[serde] params: Option<Vec<Value>>,
    #[serde] opts: Option<DbCallOpts>,
) -> Result<Vec<Row>, JsErrorBox> {
    query_impl(
        state,
        name,
        sql,
        params.unwrap_or_default(),
        opts.and_then(|o| o.timeout_ms),
    )
    .await
}

/// `db.query` 的实体：`op_db_query` 与跨请求会话的显式形态 `db.session.query(id, …)`
/// 共用同一条通路（守卫/授权/占位符改写/追踪/行数整形一处生效，不复制）。
///
/// 绑定会话时额外做两件事（评审 S1/S5）：会话内语句禁令（`BEGIN`/`SET` 等会隐式提交，
/// 制造假回滚）；按会话剩余寿命给在飞查询封顶。
/// REQ-11：query/exec 共用的前置（会话禁令 → 守卫 → 授权 → 超时核定 → 目标解析 →
/// 占位改写）。query_impl / exec_impl / begin 三处共用，防漂移。
struct CallPrelude<'a> {
    alias: String,
    /// db 访问控制的行数上限（仅 query 消费；exec 忽略）。
    row_cap: Option<u32>,
    timeout_ms: Option<u64>,
    target: Target,
    norm: std::borrow::Cow<'a, str>,
    max_result_bytes: Option<u64>,
    on_exceed: super::db_policy::OnExceed,
}

async fn call_prelude<'a>(
    state: &Rc<RefCell<OpState>>,
    name: &str,
    sql: &'a str,
    params: &[Value],
    call_timeout_ms: Option<u64>,
) -> Result<CallPrelude<'a>, JsErrorBox> {
    super::db_session::forbid_in_session(state, sql)?;
    super::db_session::readonly_gate(state, sql)?; // G9 只读会话位（attach 路径）
    super::guard::check_raw(state, sql)?; // 表归属守卫（§5.3，无模块上下文不设防）
    super::guard::check_tenant_raw(state, sql, params)?; // 多租户防护（独立于 module_ctx）
    // db 访问控制（v0.1.89）：授权 → 风险分类 → 策略位；返回本库生效的行数上限。
    let alias = super::guard::bound_db(state, name);
    let row_cap = super::db_policy::check_access(state, &alias, sql)?;
    // REQ-05：语句级超时（配置上限 × 调用侧收紧，只能收紧）。
    let quota = super::db_policy::quota_for(state, &alias);
    let timeout_ms =
        super::db_quota::effective_timeout_ms(quota.statement_timeout_ms, call_timeout_ms)
            .map_err(JsErrorBox::from)?;

    let target = resolve_target(state, name)?;
    let in_tx = matches!(&target, Target::Tx(_));
    let placeholder = match &target {
        Target::Tx(t) => t.dialect_info.caps.placeholder,
        Target::Pool(da) => da.dialect_info().caps.placeholder,
    };
    let norm = normalize_and_trace(state, placeholder, in_tx, name, sql, params)?;
    Ok(CallPrelude {
        alias,
        row_cap,
        timeout_ms,
        target,
        norm,
        max_result_bytes: quota.max_result_bytes,
        on_exceed: quota.on_exceed,
    })
}

/// REQ-11：新建池执行共享态（owner + 挂断联动 token 取自 ReqState）。
fn new_pool_shared(state: &Rc<RefCell<OpState>>, alias: &str) -> Arc<PoolExecShared> {
    let (owner, cancel_token) = {
        let g = state.borrow();
        let rs = g.borrow::<super::ReqState>();
        (super::db_session::owner_of(&g), rs.req.cancel_token)
    };
    Arc::new(PoolExecShared {
        owner,
        alias: alias.to_string(),
        start: std::time::Instant::now(),
        cancel_requested: AtomicBool::new(false),
        notify: Arc::new(Notify::new()),
        cancel_token,
        done: AtomicBool::new(false),
        events: Mutex::new(Vec::new()),
        outcome: Mutex::new(None),
        meter_final: Mutex::new(None),
        query_ctx: Mutex::new(None),
    })
}

fn registry_of(state: &Rc<RefCell<OpState>>) -> Arc<super::db_exec::ExecRegistry> {
    state
        .borrow()
        .borrow::<Arc<super::StableState>>()
        .exec_sessions
        .clone()
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// REQ-11 G1：池臂查询 = 登记（挂断/显式取消可达）→ cancelable 竞速 → settle 后摘除。
/// 登记失败（getrandom 极罕见）回落非登记路径——语义退化不 fail。
#[allow(clippy::too_many_arguments)]
async fn run_pool_query(
    state: &Rc<RefCell<OpState>>,
    alias: &str,
    da: &Arc<dyn DataAccessor>,
    norm: &str,
    params: &[Value],
    meter: &mut super::db_quota::ByteMeter,
    timeout_ms: Option<u64>,
    cancelled: bool,
) -> BridgeResult<Vec<Row>> {
    let shared = new_pool_shared(state, alias);
    let reg = registry_of(state);
    match reg.register_pool(shared.clone()) {
        Ok(id) => {
            // REQ-11 G2：挂了 cancel token 的请求（serve 挂断联动），执行体必须
            // 独立于 op future spawn——客户端断开把 handler future drop 掉之后，
            // spawn 任务仍听 notify，cancel_by_token 才能真杀服务端语句。
            // meter 经 meter_final 往返（spawn 任务写回，本臂取回供 query_tail）。
            if shared.cancel_token.is_some() {
                let mut meter_owned = std::mem::replace(
                    meter,
                    super::db_quota::ByteMeter::new(None, super::db_policy::OnExceed::default()),
                );
                let shared2 = shared.clone();
                let da2 = Arc::clone(da);
                let norm2 = norm.to_string();
                let params2 = params.to_vec();
                tokio::spawn(async move {
                    let res = da2
                        .query_cancelable(&norm2, &params2, &mut meter_owned, &shared2)
                        .await;
                    *lock(&shared2.meter_final) = Some(meter_owned);
                    settle_pool(&shared2, res.map(PoolOutcome::Rows));
                });
                let wait = async {
                    loop {
                        shared.notify.notified().await;
                        if shared.done.load(Ordering::SeqCst) {
                            break;
                        }
                    }
                    Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
                };
                return match super::db_quota::race_statement(wait, alias, timeout_ms, cancelled)
                    .await
                {
                    Ok(()) => {
                        let outcome = lock(&shared.outcome)
                            .take()
                            .expect("settle_pool writes outcome before done");
                        let r: BridgeResult<Vec<Row>> = match outcome {
                            PoolOutcome::Rows(rows) => Ok(rows),
                            PoolOutcome::Err {
                                category,
                                retryable,
                                msg,
                                data,
                            } => Err(Box::new(BridgeError {
                                category,
                                retryable,
                                session_disposition: SessionDisposition::Close,
                                message: msg,
                                data,
                            })),
                            PoolOutcome::Affected(_) => unreachable!("query arm"),
                        };
                        *meter = lock(&shared.meter_final)
                            .take()
                            .expect("spawn returns meter via meter_final");
                        reg.remove(&id);
                        r
                    }
                    Err(e) => {
                        // 语句超时：取消 spawn 任务（close_on_drop 真杀连接）并摘除登记。
                        shared.cancel_requested.store(true, Ordering::SeqCst);
                        // 广播：执行体 select! 与 poll 等待可能同时在听。
                        shared.notify.notify_waiters();
                        reg.remove(&id);
                        Err(e)
                    }
                };
            }
            let r = super::db_quota::race_statement(
                da.query_cancelable(norm, params, meter, &shared),
                alias,
                timeout_ms,
                cancelled,
            )
            .await;
            shared.done.store(true, Ordering::SeqCst);
            reg.remove(&id);
            r
        }
        Err(_) => {
            super::db_quota::race_statement(
                da.query_with_params_metered(norm, params, meter),
                alias,
                timeout_ms,
                cancelled,
            )
            .await
        }
    }
}

/// REQ-11 G1：`run_pool_query` 的 exec 版（无 meter/行）。
async fn run_pool_exec(
    state: &Rc<RefCell<OpState>>,
    alias: &str,
    da: &Arc<dyn DataAccessor>,
    norm: &str,
    params: &[Value],
    timeout_ms: Option<u64>,
    cancelled: bool,
) -> BridgeResult<i64> {
    let shared = new_pool_shared(state, alias);
    let reg = registry_of(state);
    match reg.register_pool(shared.clone()) {
        Ok(id) => {
            // G2 spawn 独立执行体：同 run_pool_query（挂断后 cancel_by_token 仍可达）。
            if shared.cancel_token.is_some() {
                let shared2 = shared.clone();
                let da2 = Arc::clone(da);
                let norm2 = norm.to_string();
                let params2 = params.to_vec();
                tokio::spawn(async move {
                    let res = da2.exec_cancelable(&norm2, &params2, &shared2).await;
                    settle_pool(&shared2, res.map(PoolOutcome::Affected));
                });
                let wait = async {
                    loop {
                        shared.notify.notified().await;
                        if shared.done.load(Ordering::SeqCst) {
                            break;
                        }
                    }
                    Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
                };
                return match super::db_quota::race_statement(wait, alias, timeout_ms, cancelled)
                    .await
                {
                    Ok(()) => {
                        let outcome = lock(&shared.outcome)
                            .take()
                            .expect("settle_pool writes outcome before done");
                        let r: BridgeResult<i64> = match outcome {
                            PoolOutcome::Affected(n) => Ok(n),
                            PoolOutcome::Err {
                                category,
                                retryable,
                                msg,
                                data,
                            } => Err(Box::new(BridgeError {
                                category,
                                retryable,
                                session_disposition: SessionDisposition::Close,
                                message: msg,
                                data,
                            })),
                            PoolOutcome::Rows(_) => unreachable!("exec arm"),
                        };
                        reg.remove(&id);
                        r
                    }
                    Err(e) => {
                        shared.cancel_requested.store(true, Ordering::SeqCst);
                        // 广播：执行体 select! 与 poll 等待可能同时在听。
                        shared.notify.notify_waiters();
                        reg.remove(&id);
                        Err(e)
                    }
                };
            }
            let r = super::db_quota::race_statement(
                da.exec_cancelable(norm, params, &shared),
                alias,
                timeout_ms,
                cancelled,
            )
            .await;
            shared.done.store(true, Ordering::SeqCst);
            reg.remove(&id);
            r
        }
        Err(_) => {
            super::db_quota::race_statement(
                da.exec_with_params(norm, params),
                alias,
                timeout_ms,
                cancelled,
            )
            .await
        }
    }
}

pub(crate) async fn query_impl(
    state: Rc<RefCell<OpState>>,
    name: String,
    sql: String,
    params: Vec<Value>,
    call_timeout_ms: Option<u64>,
) -> Result<Vec<Row>, JsErrorBox> {
    let pre = call_prelude(&state, &name, &sql, &params, call_timeout_ms).await?;
    let in_tx = matches!(&pre.target, Target::Tx(_));
    let start = std::time::Instant::now();
    // REQ-05：行级字节计量器（仅 Pool 臂被 feed；tx 路径不适用——REQ 只列三路径）。
    let mut meter = super::db_quota::ByteMeter::new(pre.max_result_bytes, pre.on_exceed);
    let res: BridgeResult<Vec<Row>> = match pre.target {
        Target::Pool(da) => {
            let cancelled = da.server_cancel_on_abandon();
            run_pool_query(
                &state,
                &pre.alias,
                &da,
                &pre.norm,
                &params,
                &mut meter,
                pre.timeout_ms,
                cancelled,
            )
            .await
        }
        Target::Tx(t) => {
            let fut = on_tx(&state, "query", async {
                t.session.lock().await.query(&pre.norm, &params).await
            });
            // attach 会话：语句超时 = 判死会话（kill_attached 先例，设计 §6.1）；
            // plain db.tx 不叠语句超时（连接状态不可知，维持寿命封顶口径——成文裁决）。
            match (pre.timeout_ms, super::db_session::attached_deadline(&state)) {
                (Some(ms), Some(_)) => {
                    match tokio::time::timeout(std::time::Duration::from_millis(ms), fut).await {
                        Ok(r) => r,
                        Err(_) => {
                            super::db_session::kill_attached(&state, "statement_timeout");
                            Err(super::db_quota::stmt_timeout_error(&pre.alias, ms, false).into())
                        }
                    }
                }
                _ => fut.await,
            }
        }
    };
    query_tail(
        &state,
        &sql,
        &params,
        &name,
        in_tx,
        &pre.alias,
        pre.row_cap,
        pre.on_exceed,
        pre.max_result_bytes,
        &mut meter,
        res,
        start,
    )
    .await
}

/// query 响应尾（legacy op 与 begin/take 回调面共用）：字节触顶分档 + trace +
/// 行数截断 + jsnum + 降级清单——一处生效不复制。
/// 「查询已成功」守卫（终审 I2）：超时与字节上限同时越界时先到先生效。
#[allow(clippy::too_many_arguments)]
async fn query_tail(
    state: &Rc<RefCell<OpState>>,
    sql: &str,
    params: &[Value],
    name: &str,
    in_tx: bool,
    alias: &str,
    row_cap: Option<u32>,
    on_exceed: super::db_policy::OnExceed,
    max_result_bytes: Option<u64>,
    meter: &mut super::db_quota::ByteMeter,
    res: BridgeResult<Vec<Row>>,
    start: std::time::Instant,
) -> Result<Vec<Row>, JsErrorBox> {
    // REQ-05：字节触顶分档——fail → 结构化 OJ_DB_CAPACITY（trace 记失败画像后抛）；
    // truncate → 交付已取行 + X-OJ-Result-Truncated 显式信令（对齐 X-OJ-Row-Limit 先例）。
    if meter.exceeded && res.is_ok() {
        match on_exceed {
            super::db_policy::OnExceed::Fail => {
                let e = super::db_quota::result_too_large_error(
                    alias,
                    max_result_bytes.unwrap_or(0),
                    meter.counted,
                );
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                trace_sql(
                    state,
                    sql,
                    params,
                    name,
                    in_tx,
                    ms,
                    None,
                    false,
                    Some(e.message.clone()),
                    Some(e.category.as_str().to_string()),
                    Some(e.retryable),
                );
                return Err(JsErrorBox::from(e));
            }
            super::db_policy::OnExceed::Truncate => {
                state
                    .borrow_mut()
                    .borrow_mut::<super::ReqState>()
                    .headers
                    .push(("X-OJ-Result-Truncated".into(), meter.counted.to_string()));
            }
        }
    }
    let (cat, retry) = err_meta(&res);
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    trace_sql(
        state,
        sql,
        params,
        name,
        in_tx,
        ms,
        res.as_ref().ok().map(|r| r.len() as i64),
        res.is_ok(),
        res.as_ref().err().map(|e| e.to_string()),
        cat,
        retry,
    );

    let mut rows = res.map_err(to_js_err)?;
    // db 访问控制的行数上限（raw 路径 = **取回后截断**：全量扫描/传输已发生，这是响应整形
    // 而非 DoS 防线，边界写进 db-guide）。
    if let Some(cap) = row_cap
        && rows.len() as u32 > cap
    {
        rows.truncate(cap as usize);
        let mut g = state.borrow_mut();
        g.borrow_mut::<super::ReqState>()
            .headers
            .push(("X-OJ-Row-Limit".into(), cap.to_string()));
    }
    // 出口护栏：超界整数降十进制字符串，否则 JS 侧拿到 BigInt、json.ok 必 500（见 jsnum）。
    super::jsnum::sanitize_rows(&mut rows);
    // REQ-07a：语句级降级清单外发（X-OJ-Degraded）。这是权威判定面——前缀只是
    // 单元格编码，TEXT 字面量撞前缀以本头为准。
    push_degraded(state, oj_plugin_ffi::cellenv::degraded_of(&rows));
    Ok(rows)
}

/// REQ-07a 降级清单外发的统一出口（db.query 裸 SQL 路与构造器路共用）。
///
/// 两条硬约束（整支评审 I1/I2，缺一则权威面无声消失）：
/// - **并集合并**：serve 侧同名响应头 last-wins，同请求多条降级语句分条推会被
///   静默覆盖——这里与既有头并集去重后收敛为一条；
/// - **纯 ASCII**：`HeaderValue` 只容可见 ASCII，非 ASCII 列名（PG/MySQL 合法）
///   的 JSON 不转义会被 serve 宽容解析丢弃——`\uXXXX` 是合法 JSON 转义，消费方
///   `JSON.parse` 无感。
pub(crate) fn push_degraded(state: &Rc<RefCell<OpState>>, degraded: Vec<Value>) {
    if degraded.is_empty() {
        return;
    }
    let mut g = state.borrow_mut();
    let headers = &mut g.borrow_mut::<super::ReqState>().headers;
    let mut merged: Vec<Value> = headers
        .iter()
        .find(|(k, _)| k == "X-OJ-Degraded")
        .and_then(|(_, v)| serde_json::from_str(v).ok())
        .unwrap_or_default();
    for d in degraded {
        if !merged.contains(&d) {
            merged.push(d);
        }
    }
    headers.retain(|(k, _)| k != "X-OJ-Degraded");
    headers.push((
        "X-OJ-Degraded".into(),
        ascii_json(&serde_json::to_string(&merged).unwrap()),
    ));
}

/// JSON 文本的非 ASCII 字符 → `\uXXXX`（UTF-16 码元级转义，代理对天然正确）。
fn ascii_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for u in s.encode_utf16() {
        if u < 0x80 {
            out.push(u as u8 as char);
        } else {
            out.push_str(&format!("\\u{u:04x}"));
        }
    }
    out
}

/// `Instant` → 毫秒（trace 计时用）。单拧出来是为了变异测试：`* 1000.0` 的量纲换算
/// 有自己的时钟钉子测试（`elapsed_ms_scales_to_millis`）——内联在 op 里时 `*`→`+`/`/`
/// 的变异体只是改了「记出来的数字大小」，运行期不可断言，必然静默存活。
/// db.exec(sql, params?)：Promise<受影响行数>。
#[op2]
#[serde]
pub async fn op_db_exec(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[string] sql: String,
    #[serde] params: Option<Vec<Value>>,
    #[serde] opts: Option<DbCallOpts>,
) -> Result<i64, JsErrorBox> {
    exec_impl(
        state,
        name,
        sql,
        params.unwrap_or_default(),
        opts.and_then(|o| o.timeout_ms),
    )
    .await
}

/// `db.exec` 的实体（与 `db.session.exec` 共用；理由同 [`query_impl`]）。
pub(crate) async fn exec_impl(
    state: Rc<RefCell<OpState>>,
    name: String,
    sql: String,
    params: Vec<Value>,
    call_timeout_ms: Option<u64>,
) -> Result<i64, JsErrorBox> {
    let pre = call_prelude(&state, &name, &sql, &params, call_timeout_ms).await?;
    // 裸 SQL 路径（query/exec/stream）对开放方言**全量可用**，不做 known 子集门控
    // （设计 §5.5：门控只覆盖 builder / exec_route / list_tables / nextSeq /
    // search.* / CLI 六处）——方言知识只体现在占位符能力位。
    let in_tx = matches!(&pre.target, Target::Tx(_));
    let start = std::time::Instant::now();
    let res: BridgeResult<i64> = match pre.target {
        Target::Pool(da) => {
            let cancelled = da.server_cancel_on_abandon();
            run_pool_exec(
                &state,
                &pre.alias,
                &da,
                &pre.norm,
                &params,
                pre.timeout_ms,
                cancelled,
            )
            .await
        }
        Target::Tx(t) => {
            let fut = on_tx(&state, "exec", async {
                t.session.lock().await.exec(&pre.norm, &params).await
            });
            match (pre.timeout_ms, super::db_session::attached_deadline(&state)) {
                (Some(ms), Some(_)) => {
                    match tokio::time::timeout(std::time::Duration::from_millis(ms), fut).await {
                        Ok(r) => r,
                        Err(_) => {
                            super::db_session::kill_attached(&state, "statement_timeout");
                            Err(super::db_quota::stmt_timeout_error(&pre.alias, ms, false).into())
                        }
                    }
                }
                _ => fut.await,
            }
        }
    };
    let (cat, retry) = err_meta(&res);
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    trace_sql(
        &state,
        &sql,
        &params,
        &name,
        in_tx,
        ms,
        res.as_ref().ok().copied(),
        res.is_ok(),
        res.as_ref().err().map(|e| e.to_string()),
        cat,
        retry,
    );

    res.map_err(to_js_err)
}

// ---------------------------------------------------------------------------
// REQ-11 G1/G3：begin/poll/take 回调面。onOpen/onProgress 是 JS 回调（Rust 无
// 回调进 JS 的先例）——begin 返回 executionId 后立即 spawn 执行体，JS 以 poll
// 节流 drain 相位事件，done 后 take 取结果（响应整形在 take 侧与 legacy 共用 query_tail）。
// ---------------------------------------------------------------------------

#[derive(serde::Serialize)]
struct BeginReply {
    #[serde(rename = "executionId")]
    execution_id: String,
}

#[derive(serde::Serialize)]
struct PollReply {
    done: bool,
    events: Vec<super::db::CallEvent>,
}

fn exec_not_found(id: &str) -> JsErrorBox {
    JsErrorBox::generic(format!(
        "execution {} not found (unknown, finished, or owned by another caller)",
        super::handle_core::id_prefix(super::db_exec::PREFIX, id)
    ))
}

/// 执行体 settle 单点：写 outcome → 置 done → 唤醒 poll 等待。
fn settle_pool(shared: &PoolExecShared, res: BridgeResult<PoolOutcome>) {
    let outcome = match res {
        Ok(o) => o,
        Err(e) => match e.downcast::<BridgeError>() {
            Ok(be) => PoolOutcome::Err {
                category: be.category,
                retryable: be.retryable,
                msg: be.message,
                data: be.data,
            },
            Err(e) => PoolOutcome::Err {
                category: DbErrorCategory::Internal,
                retryable: false,
                msg: e.to_string(),
                data: Default::default(),
            },
        },
    };
    *lock(&shared.outcome) = Some(outcome);
    shared.done.store(true, Ordering::SeqCst);
    // notify_waiters：并存在飞 listener（poll 等待 + 执行体 select!），广播防饿死。
    shared.notify.notify_waiters();
}

/// db.query 回调面入口（REQ-11 G1）：前置与 query_impl 同款；登记后 spawn 执行体，
/// 立即返回 executionId（onOpen 的数据源）。Tx target 拒绝（begin 是池路径设施；
/// tx 语句取消随 session 关闭走，spec 非目标）。
#[op2]
#[serde]
pub async fn op_db_query_begin(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[string] sql: String,
    #[serde] params: Option<Vec<Value>>,
    #[serde] opts: Option<DbCallOpts>,
) -> Result<BeginReply, JsErrorBox> {
    let params = params.unwrap_or_default();
    let pre = call_prelude(
        &state,
        &name,
        &sql,
        &params,
        opts.and_then(|o| o.timeout_ms),
    )
    .await?;
    let Target::Pool(da) = pre.target else {
        return Err(JsErrorBox::generic(
            "query begin is only available on the pool path (not inside db.tx)",
        ));
    };
    let shared = new_pool_shared(&state, &pre.alias);
    *lock(&shared.query_ctx) = Some(QueryTakeCtx {
        sql: sql.clone(),
        params: params.clone(),
        row_cap: pre.row_cap,
        in_tx: false,
    });
    let id = registry_of(&state)
        .register_pool(shared.clone())
        .map_err(JsErrorBox::generic)?;
    let norm = pre.norm.into_owned();
    let max_result_bytes = pre.max_result_bytes;
    let on_exceed = pre.on_exceed;
    // Final-fix F1：回调面语句超时。直路有 race_statement deadline，begin 路径此前
    // 算了 timeout_ms 却未用（执行体裸跑）——这里同款竞速：到点置取消标志，accessor
    // 取消臂 close_on_drop 真杀后按 Timeout 类别收口（cancelled 按 accessor 诚实位）。
    let timeout_ms = pre.timeout_ms;
    let alias_c = pre.alias.clone();
    let cancelled_on_abandon = da.server_cancel_on_abandon();
    tokio::spawn(async move {
        let mut meter = super::db_quota::ByteMeter::new(max_result_bytes, on_exceed);
        let res = match timeout_ms {
            Some(ms) => {
                let mut fut = Box::pin(da.query_cancelable(&norm, &params, &mut meter, &shared));
                let mut delay = Box::pin(tokio::time::sleep(std::time::Duration::from_millis(ms)));
                let mut timed_out = false;
                loop {
                    tokio::select! {
                        r = fut.as_mut() => break r,
                        _ = delay.as_mut() => {
                            timed_out = true;
                            shared.cancel_requested.store(true, Ordering::SeqCst);
                            shared.notify.notify_waiters();
                            // 防重复触发：推迟到远未来，等 fut 经取消臂收口。
                            delay.as_mut().reset(tokio::time::Instant::now()
                                + std::time::Duration::from_secs(3600));
                        }
                    }
                }
                .map_err(move |e| {
                    if timed_out
                        && e.downcast_ref::<BridgeError>()
                            .is_some_and(|be| be.category == DbErrorCategory::Cancelled)
                    {
                        Box::new(super::db_quota::stmt_timeout_error(
                            &alias_c,
                            ms,
                            cancelled_on_abandon,
                        )) as Box<dyn std::error::Error + Send + Sync>
                    } else {
                        e
                    }
                })
            }
            None => {
                da.query_cancelable(&norm, &params, &mut meter, &shared)
                    .await
            }
        };
        *lock(&shared.meter_final) = Some(meter);
        settle_pool(&shared, res.map(PoolOutcome::Rows));
    });
    Ok(BeginReply { execution_id: id })
}

/// db.exec 回调面入口（REQ-11 G1）：同 [`op_db_query_begin`] 的 exec 版。
#[op2]
#[serde]
pub async fn op_db_exec_begin(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[string] sql: String,
    #[serde] params: Option<Vec<Value>>,
    #[serde] opts: Option<DbCallOpts>,
) -> Result<BeginReply, JsErrorBox> {
    let params = params.unwrap_or_default();
    let pre = call_prelude(
        &state,
        &name,
        &sql,
        &params,
        opts.and_then(|o| o.timeout_ms),
    )
    .await?;
    let Target::Pool(da) = pre.target else {
        return Err(JsErrorBox::generic(
            "exec begin is only available on the pool path (not inside db.tx)",
        ));
    };
    let shared = new_pool_shared(&state, &pre.alias);
    *lock(&shared.query_ctx) = Some(QueryTakeCtx {
        sql: sql.clone(),
        params: params.clone(),
        row_cap: None,
        in_tx: false,
    });
    let id = registry_of(&state)
        .register_pool(shared.clone())
        .map_err(JsErrorBox::generic)?;
    let norm = pre.norm.into_owned();
    // Final-fix F1：exec 回调面同款 deadline 竞速（见 op_db_query_begin）。
    let timeout_ms = pre.timeout_ms;
    let alias_c = pre.alias.clone();
    let cancelled_on_abandon = da.server_cancel_on_abandon();
    tokio::spawn(async move {
        let res = match timeout_ms {
            Some(ms) => {
                let mut fut = Box::pin(da.exec_cancelable(&norm, &params, &shared));
                let mut delay = Box::pin(tokio::time::sleep(std::time::Duration::from_millis(ms)));
                let mut timed_out = false;
                loop {
                    tokio::select! {
                        r = fut.as_mut() => break r,
                        _ = delay.as_mut() => {
                            timed_out = true;
                            shared.cancel_requested.store(true, Ordering::SeqCst);
                            shared.notify.notify_waiters();
                            delay.as_mut().reset(tokio::time::Instant::now()
                                + std::time::Duration::from_secs(3600));
                        }
                    }
                }
                .map_err(move |e| {
                    if timed_out
                        && e.downcast_ref::<BridgeError>()
                            .is_some_and(|be| be.category == DbErrorCategory::Cancelled)
                    {
                        Box::new(super::db_quota::stmt_timeout_error(
                            &alias_c,
                            ms,
                            cancelled_on_abandon,
                        )) as Box<dyn std::error::Error + Send + Sync>
                    } else {
                        e
                    }
                })
            }
            None => da.exec_cancelable(&norm, &params, &shared).await,
        };
        settle_pool(&shared, res.map(PoolOutcome::Affected));
    });
    Ok(BeginReply { execution_id: id })
}

/// 回调面轮询（REQ-11 G3）：drain 相位事件；未 done 时可等待至 wait_ms。
/// 判权同 db.cancel 的 bearer 口径（他人/未知 id = 同一条 not-found）。
#[op2]
#[serde]
pub async fn op_db_call_poll(
    state: Rc<RefCell<OpState>>,
    #[string] id: String,
    wait_ms: Option<u32>,
) -> Result<PollReply, JsErrorBox> {
    let owner = super::db_session::owner_of(&state.borrow());
    let reg = registry_of(&state);
    let shared = reg.pool(&id, &owner).ok_or_else(|| exec_not_found(&id))?;
    let mut events = std::mem::take(&mut *lock(&shared.events));
    let mut done = shared.done.load(Ordering::SeqCst);
    if !done && let Some(ms) = wait_ms {
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(u64::from(ms)),
            shared.notify.notified(),
        )
        .await;
        events.extend(std::mem::take(&mut *lock(&shared.events)));
        done = shared.done.load(Ordering::SeqCst);
    }
    Ok(PollReply { done, events })
}

/// poll/take 公共前置：bearer 判权 + done 检查 + 摘登记（take 单次性）。
fn take_shared(state: &Rc<RefCell<OpState>>, id: &str) -> Result<Arc<PoolExecShared>, JsErrorBox> {
    let owner = super::db_session::owner_of(&state.borrow());
    let reg = registry_of(state);
    let shared = reg.pool(id, &owner).ok_or_else(|| exec_not_found(id))?;
    if !shared.done.load(Ordering::SeqCst) {
        return Err(JsErrorBox::generic(format!(
            "execution {} is not done yet (poll until done before take)",
            super::handle_core::id_prefix(super::db_exec::PREFIX, id)
        )));
    }
    reg.remove(id);
    Ok(shared)
}

/// 回调面取结果（query）：仅 done 后可取（否则参数错误）；响应整形与 legacy
/// 共用 query_tail（字节触顶分档 / 行数截断 / jsnum / 降级 / trace 一处生效）。
#[op2]
#[serde]
pub async fn op_db_query_take(
    state: Rc<RefCell<OpState>>,
    #[string] id: String,
) -> Result<Vec<Row>, JsErrorBox> {
    let shared = take_shared(&state, &id)?;
    let outcome = lock(&shared.outcome)
        .take()
        .ok_or_else(|| JsErrorBox::generic("execution outcome missing"))?;
    let res: BridgeResult<Vec<Row>> = match outcome {
        PoolOutcome::Rows(rows) => Ok(rows),
        PoolOutcome::Err {
            category,
            retryable,
            msg,
            data,
        } => Err(Box::new(BridgeError {
            category,
            retryable,
            session_disposition: SessionDisposition::Close,
            message: msg,
            data,
        })),
        PoolOutcome::Affected(_) => Err(Box::new(BridgeError {
            category: DbErrorCategory::Internal,
            retryable: false,
            session_disposition: SessionDisposition::Keep,
            message: "execution outcome is not a query result".to_string(),
            data: Default::default(),
        })),
    };
    let ctx = lock(&shared.query_ctx)
        .take()
        .ok_or_else(|| JsErrorBox::generic("query take context missing"))?;
    let mut meter = lock(&shared.meter_final)
        .take()
        .ok_or_else(|| JsErrorBox::generic("query meter state missing"))?;
    let max_result_bytes = meter.limit();
    query_tail(
        &state,
        &ctx.sql,
        &ctx.params,
        "<begin>",
        ctx.in_tx,
        &shared.alias,
        ctx.row_cap,
        meter.on_exceed,
        max_result_bytes,
        &mut meter,
        res,
        shared.start,
    )
    .await
}

/// 回调面取结果（exec）：affected 行数或结构化错误。
#[op2]
#[serde]
pub async fn op_db_exec_take(
    state: Rc<RefCell<OpState>>,
    #[string] id: String,
) -> Result<i64, JsErrorBox> {
    let shared = take_shared(&state, &id)?;
    let outcome = lock(&shared.outcome)
        .take()
        .ok_or_else(|| JsErrorBox::generic("execution outcome missing"))?;
    let res: BridgeResult<i64> = match outcome {
        PoolOutcome::Affected(n) => Ok(n),
        PoolOutcome::Err {
            category,
            retryable,
            msg,
            data,
        } => Err(Box::new(BridgeError {
            category,
            retryable,
            session_disposition: SessionDisposition::Close,
            message: msg,
            data,
        })),
        PoolOutcome::Rows(_) => Err(Box::new(BridgeError {
            category: DbErrorCategory::Internal,
            retryable: false,
            session_disposition: SessionDisposition::Keep,
            message: "execution outcome is not an exec result".to_string(),
            data: Default::default(),
        })),
    };
    let ctx = lock(&shared.query_ctx)
        .take()
        .ok_or_else(|| JsErrorBox::generic("exec take context missing"))?;
    let (cat, retry) = err_meta(&res);
    let ms = shared.start.elapsed().as_secs_f64() * 1000.0;
    trace_sql(
        &state,
        &ctx.sql,
        &ctx.params,
        "<begin>",
        ctx.in_tx,
        ms,
        res.as_ref().ok().copied(),
        res.is_ok(),
        res.as_ref().err().map(|e| e.to_string()),
        cat,
        retry,
    );
    res.map_err(to_js_err)
}
