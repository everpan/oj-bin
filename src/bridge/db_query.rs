//! db.query / db.exec 裸 SQL 出口（从 db.rs 拆分，纯移动）：REQ-05 语句级超时
//! 竞速 + max_result_bytes 行级字节计费的接线点；构造器路径经 query.rs 复用本模块原语。
use std::cell::RefCell;
use std::rc::Rc;

use deno_core::{OpState, op2};
use deno_error::JsErrorBox;
use serde_json::Value;

use super::db::{Row, Target, err_meta, normalize_and_trace, resolve_target, to_js_err, trace_sql};
use crate::bridge::BridgeResult;
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
pub(crate) async fn query_impl(
    state: Rc<RefCell<OpState>>,
    name: String,
    sql: String,
    params: Vec<Value>,
    call_timeout_ms: Option<u64>,
) -> Result<Vec<Row>, JsErrorBox> {
    super::db_session::forbid_in_session(&state, &sql)?;
    super::db_session::readonly_gate(&state, &sql)?; // G9 只读会话位（attach 路径）
    super::guard::check_raw(&state, &sql)?; // 表归属守卫（§5.3，无模块上下文不设防）
    super::guard::check_tenant_raw(&state, &sql, &params)?; // 多租户防护（独立于 module_ctx）
    // db 访问控制（v0.1.89）：授权 → 风险分类 → 策略位；返回本库生效的行数上限。
    let alias = super::guard::bound_db(&state, &name);
    let row_cap = super::db_policy::check_access(&state, &alias, &sql)?;
    // REQ-05：语句级超时（配置上限 × 调用侧收紧，只能收紧）。
    let quota = super::db_policy::quota_for(&state, &alias);
    let timeout_ms =
        super::db_quota::effective_timeout_ms(quota.statement_timeout_ms, call_timeout_ms)
            .map_err(JsErrorBox::from)?;

    let target = resolve_target(&state, &name)?;
    let in_tx = matches!(&target, Target::Tx(_));
    let placeholder = match &target {
        Target::Tx(t) => t.dialect_info.caps.placeholder,
        Target::Pool(da) => da.dialect_info().caps.placeholder,
    };
    let norm = normalize_and_trace(&state, placeholder, in_tx, &name, &sql, &params)?;
    let start = std::time::Instant::now();
    // REQ-05：行级字节计量器（仅 Pool 臂被 feed；tx 路径不适用——REQ 只列三路径）。
    let mut meter = super::db_quota::ByteMeter::new(quota.max_result_bytes, quota.on_exceed);
    let res: BridgeResult<Vec<Row>> = match target {
        Target::Pool(da) => {
            let cancelled = da.server_cancel_on_abandon();
            super::db_quota::race_statement(
                da.query_with_params_metered(&norm, &params, &mut meter),
                &alias,
                timeout_ms,
                cancelled,
            )
            .await
        }
        Target::Tx(t) => {
            let fut = on_tx(&state, "query", async {
                t.session.lock().await.query(&norm, &params).await
            });
            // attach 会话：语句超时 = 判死会话（kill_attached 先例，设计 §6.1）；
            // plain db.tx 不叠语句超时（连接状态不可知，维持寿命封顶口径——成文裁决）。
            match (timeout_ms, super::db_session::attached_deadline(&state)) {
                (Some(ms), Some(_)) => {
                    match tokio::time::timeout(std::time::Duration::from_millis(ms), fut).await {
                        Ok(r) => r,
                        Err(_) => {
                            super::db_session::kill_attached(&state, "statement_timeout");
                            Err(super::db_quota::stmt_timeout_error(&alias, ms, false).into())
                        }
                    }
                }
                _ => fut.await,
            }
        }
    };
    // REQ-05：字节触顶分档——fail → 结构化 OJ_DB_CAPACITY（trace 记失败画像后抛）；
    // truncate → 交付已取行 + X-OJ-Result-Truncated 显式信令（对齐 X-OJ-Row-Limit 先例）。
    // 「查询已成功」守卫（终审 I2）：超时与字节上限同时越界时先到先生效——
    // 无守卫会把 OJ_DB_TIMEOUT 改写成 OJ_DB_CAPACITY（构造器路径同款守卫）。
    if meter.exceeded && res.is_ok() {
        match quota.on_exceed {
            super::db_policy::OnExceed::Fail => {
                let e = super::db_quota::result_too_large_error(
                    &alias,
                    quota.max_result_bytes.unwrap_or(0),
                    meter.counted,
                );
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                trace_sql(
                    &state,
                    &sql,
                    &params,
                    &name,
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
        &state,
        &sql,
        &params,
        &name,
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
    // REQ-07a：语句级降级清单外发（X-OJ-Degraded，对齐 X-OJ-Row-Limit 先例）。
    // 这是权威判定面——前缀只是单元格编码，TEXT 字面量撞前缀以本头为准。
    let degraded = oj_plugin_ffi::cellenv::degraded_of(&rows);
    if !degraded.is_empty() {
        state
            .borrow_mut()
            .borrow_mut::<super::ReqState>()
            .headers
            .push((
                "X-OJ-Degraded".into(),
                serde_json::to_string(&degraded).unwrap(),
            ));
    }
    Ok(rows)
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
    super::db_session::forbid_in_session(&state, &sql)?;
    super::db_session::readonly_gate(&state, &sql)?; // G9 只读会话位（attach 路径）
    // 裸 SQL 路径（query/exec/stream）对开放方言**全量可用**，不做 known 子集门控
    // （设计 §5.5：门控只覆盖 builder / exec_route / list_tables / nextSeq /
    // search.* / CLI 六处）——方言知识只体现在占位符能力位。
    super::guard::check_raw(&state, &sql)?; // 表归属守卫（§5.3，无模块上下文不设防）
    super::guard::check_tenant_raw(&state, &sql, &params)?; // 多租户防护（独立于 module_ctx）
    // db 访问控制（v0.1.89）：写类语句在此被 read_only / 危险语句门拦下。
    let alias = super::guard::bound_db(&state, &name);
    super::db_policy::check_access(&state, &alias, &sql)?;
    // REQ-05：语句级超时（口径同 query_impl）。
    let quota = super::db_policy::quota_for(&state, &alias);
    let timeout_ms =
        super::db_quota::effective_timeout_ms(quota.statement_timeout_ms, call_timeout_ms)
            .map_err(JsErrorBox::from)?;

    let target = resolve_target(&state, &name)?;
    let in_tx = matches!(&target, Target::Tx(_));
    let placeholder = match &target {
        Target::Tx(t) => t.dialect_info.caps.placeholder,
        Target::Pool(da) => da.dialect_info().caps.placeholder,
    };
    let norm = normalize_and_trace(&state, placeholder, in_tx, &name, &sql, &params)?;
    let start = std::time::Instant::now();
    let res: BridgeResult<i64> = match target {
        Target::Pool(da) => {
            let cancelled = da.server_cancel_on_abandon();
            super::db_quota::race_statement(
                da.exec_with_params(&norm, &params),
                &alias,
                timeout_ms,
                cancelled,
            )
            .await
        }
        Target::Tx(t) => {
            let fut = on_tx(&state, "exec", async {
                t.session.lock().await.exec(&norm, &params).await
            });
            match (timeout_ms, super::db_session::attached_deadline(&state)) {
                (Some(ms), Some(_)) => {
                    match tokio::time::timeout(std::time::Duration::from_millis(ms), fut).await {
                        Ok(r) => r,
                        Err(_) => {
                            super::db_session::kill_attached(&state, "statement_timeout");
                            Err(super::db_quota::stmt_timeout_error(&alias, ms, false).into())
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
