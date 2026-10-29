//! db.nextSeq 平台序列表 op（从 db.rs 拆分，纯移动）：单语句原子取号 +
//! 每库一次建表缓存（StableState 级，防跨 Bridge 误判）。
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use deno_core::{OpState, op2};
use deno_error::JsErrorBox;
use serde_json::Value;

use super::db::{
    DataAccessor, Dialect, Row, Target, TxSession, require_known_dialect, resolve_target, trace_sql,
};

#[op2]
#[serde]
pub async fn op_db_next_seq(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[string] seq: String,
) -> Result<Row, JsErrorBox> {
    if seq.is_empty() || seq.len() > 128 {
        return Err(JsErrorBox::generic(
            "db.nextSeq: name must be 1..=128 characters",
        ));
    }
    let err = |e: String| JsErrorBox::generic(e);
    let da = super::query::lookup(&state, &name)?;
    // 门控（T8）：未知方言（不在 builder-known 子集）一律结构化 unsupported，绝不回落 Sqlite。
    let _dialect = require_known_dialect(da.dialect_info(), "next_seq")?;
    // ① **先确保序列表存在**（每库一次，Bridge 级缓存）。DDL 走池、绝不在调用方事务里做：
    //    MySQL 的 DDL 会隐式提交调用方事务；PG 里事务内失败语句会让事务进入 aborted 态
    //    （后续一律 25P02）——若把建表放在「先试后建」的失败路径上，**首次在 db.tx 里用新序列名
    //    必然失败并毒化调用方事务**（架构评审 P1-2）。
    //    键用**物理库名**（`bound_db` 的结果）而非 JS 可见名：同一 Bridge 下不同模块可把字面
    //    `"default"` 绑到不同物理库（`manifest db:` / `db_override`），用可见名当键会让后一个库
    //    命中前一个库的缓存、跳过建表（池路径多付一次失败+DDL，事务路径直接失败）。
    let phys = super::guard::bound_db(&state, &name);
    ensure_seq_once(&state, &phys, &*da).await.map_err(err)?;
    let target = resolve_target(&state, &name)?;
    let in_tx = matches!(&target, Target::Tx(_));
    let start = std::time::Instant::now();
    let alloc_res: Result<Row, JsErrorBox> = match target {
        // 池路径：表刚确保过，正常一次成功；仍保留「失败→再建表→重试」兜底（外部 drop 等）。
        Target::Pool(da) => next_seq_via_pool(&da, &seq).await.map_err(err),
        // tx 路径：搭车调用方事务的连接（建表已在池上完成，此处不再触碰 DDL）。
        Target::Tx(t) => {
            let dial = da.dialect();
            let mut s = t.session.lock().await;
            seq_next(&mut **s, dial, &seq).await.map_err(err)
        }
    };
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    // 序列分配本质是一次取号 SQL，按合成名追踪（不含业务表名）。
    let sql = format!("db.nextSeq(`{seq}`)");
    trace_sql(
        &state,
        &sql,
        &[],
        &name,
        in_tx,
        ms,
        None,
        alloc_res.is_ok(),
        alloc_res.as_ref().err().map(|e| e.to_string()),
        None,
        None,
    );
    let mut out = alloc_res?;
    // 出口护栏：超界整数降十进制字符串（与 DB 读值同契约，见 jsnum）。
    super::jsnum::sanitize_js_numbers(&mut out);
    Ok(out)
}

/// JS `db.dialect()`：报连接引擎名（"sqlite" | "mysql" | "postgres"）。方言是 accessor
/// 属性、与事务无关，不走 resolve_target（v0.1.61，`_shared/search` 等共享模块按此分派
/// 方言 SQL；业务 handler 慎用——分支方言即放弃跨引擎可移植承诺）。
async fn ensure_seq_once(
    state: &Rc<RefCell<OpState>>,
    name: &str,
    da: &dyn DataAccessor,
) -> Result<(), String> {
    // 持锁期间 panic 不能让这个 Bridge 级缓存**永久中毒**（否则此后每次 `nextSeq` 都 panic）。
    // 这里只是 HashSet 查/插，`PoisonError::into_inner()` 取回的集合本身仍是自洽的。
    if state
        .borrow()
        .borrow::<Arc<super::StableState>>()
        .seq_ensured
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(name)
    {
        return Ok(());
    }
    ensure_seq_table(da).await?;
    state
        .borrow()
        .borrow::<Arc<super::StableState>>()
        .seq_ensured
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(name.to_string());
    Ok(())
}

/// 池路径取号：先试一次，失败（表不存在）才建表并重试。
async fn next_seq_via_pool(da: &Arc<dyn DataAccessor>, seq: &str) -> Result<Row, String> {
    let dial = da.dialect();
    match next_seq_once_pool(da, dial, seq).await {
        Ok(v) => Ok(v),
        Err(e) => {
            ensure_seq_table(&**da).await?;
            next_seq_once_pool(da, dial, seq)
                .await
                .map_err(|e2| format!("{e}; retry after create table failed: {e2}"))
        }
    }
}

/// 池路径的一次取号：PG/SQLite 单语句直走池；MySQL 两条语句包一次短事务（同连接）。
async fn next_seq_once_pool(
    da: &Arc<dyn DataAccessor>,
    dial: Dialect,
    seq: &str,
) -> Result<Row, String> {
    if dial == Dialect::MySql {
        let mut s = da.begin().await.map_err(|e| e.to_string())?;
        let v = seq_next(&mut *s, dial, seq).await?;
        s.commit().await.map_err(|e| e.to_string())?;
        Ok(v)
    } else {
        let rows = da
            .query_with_params(insert_sql(dial), &[Value::String(seq.to_string())])
            .await
            .map_err(|e| e.to_string())?;
        pick_v(rows)
    }
}

/// 序列表 DDL（幂等；按方言给 varchar/text 与 bigint）。
async fn ensure_seq_table(da: &dyn DataAccessor) -> Result<(), String> {
    let ddl = match da.dialect() {
        Dialect::MySql => {
            "create table if not exists _oj_sequences (name varchar(128) primary key, v bigint not null)"
        }
        _ => "create table if not exists _oj_sequences (name text primary key, v bigint not null)",
    };
    da.exec_with_params(ddl, &[])
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// 取号 SQL（PG/SQLite；MySQL 是两条语句，见 `seq_next`）。
fn insert_sql(dial: Dialect) -> &'static str {
    if dial == Dialect::Postgres {
        "insert into _oj_sequences (name, v) values ($1, 1) \
         on conflict (name) do update set v = _oj_sequences.v + 1 returning v"
    } else {
        "insert into _oj_sequences (name, v) values (?, 1) \
         on conflict (name) do update set v = v + 1 returning v"
    }
}

/// 单次取号（在给定会话内跑完；MySQL 是两条语句，其余一条 `returning`）。
async fn seq_next(s: &mut dyn TxSession, dialect: Dialect, seq: &str) -> Result<Row, String> {
    let p = vec![Value::String(seq.to_string())];
    let rows = if dialect == Dialect::MySql {
        s.exec(
            "insert into _oj_sequences (name, v) values (?, last_insert_id(1)) \
             on duplicate key update v = last_insert_id(v + 1)",
            &p,
        )
        .await
        .map_err(|e| e.to_string())?;
        s.query("select last_insert_id() as v", &[])
            .await
            .map_err(|e| e.to_string())?
    } else {
        s.query(insert_sql(dialect), &p)
            .await
            .map_err(|e| e.to_string())?
    };
    pick_v(rows)
}

/// 从返回行里取 `v` 字段（缺列/空结果 → 报错）。
fn pick_v(rows: Vec<Row>) -> Result<Row, String> {
    rows.into_iter()
        .next()
        .and_then(|r| r.get("v").cloned())
        .ok_or_else(|| "db.nextSeq: no value returned".to_string())
}
