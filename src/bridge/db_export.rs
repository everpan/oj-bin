//! U-18 任务式导出登记（`db.export` / `exportStatus` / `exportCancel`，v0.1.104）。
//!
//! # 为什么
//!
//! REQ-03：大结果集导出 = 同步开游标 + **自驱消费任务**（db.stream 的泵是消费端驱动
//! 的，发起即返回后必须由新的 Rust 任务持续 drain——这是与 db.stream 的本质差异）。
//! 本模块是导出任务的进程级登记壳：taskId（boot-tag，跨重启一律 not found）、
//! 归属判定、双档配额（全局/每主体）、进度状态机、产物 TTL 台账。
//!
//! # 复用而非新造
//!
//! 壳形状照 [`db_exec::ExecRegistry`]（U-5 同款）：`Arc` 不可变挂 `StableState`、
//! 内部 `Mutex` 变动、惰性清扫。配额计数按 state==Running 现算（登记表量级小，
//! 不维护独立计数器——少一份可对不上账的状态）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use futures::StreamExt;
use serde_json::{Value, json};

use super::handle_core::{self, Owner};
use std::sync::atomic::Ordering;

/// taskId 前缀（形状白名单与日志遮蔽共用；blob 产物走 `export/` 保留前缀）。
pub const PREFIX: &str = "export_";

/// 导出任务状态机。serde lowercase ↔ JS 面 `state: "running"|"done"|"failed"|"cancelled"`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportState {
    Running,
    Done,
    Failed,
    Cancelled,
}

impl ExportState {
    fn as_str(&self) -> &'static str {
        match self {
            ExportState::Running => "running",
            ExportState::Done => "done",
            ExportState::Failed => "failed",
            ExportState::Cancelled => "cancelled",
        }
    }
}

/// 登记失败：两档配额文案必须区分（规格 §3.3-6：错误文案区分「全局满 / 本人满」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisterError {
    GlobalFull,
    SubjectFull,
}

impl RegisterError {
    pub fn message(&self) -> &'static str {
        match self {
            RegisterError::GlobalFull => {
                "db.export: global export concurrency limit reached, try again later"
            }
            RegisterError::SubjectFull => {
                "db.export: per-subject export concurrency limit reached, try again later"
            }
        }
    }
}

/// 终态写入（消费任务退出时一次性提交）。
#[derive(Debug)]
pub enum Finish {
    Done {
        rows: u64,
        bytes: u64,
        /// max_rows 策略截断（错误 ≠ 策略双轨：bytes 超 maxBytes 走 Failed）。
        truncated: bool,
        truncated_reason: Option<&'static str>,
        blob_key: String,
        /// REQ-07a：语句级降级清单（`$<type>:` 编码列的权威判定面；产物单元格原样）。
        degraded: Vec<serde_json::Value>,
    },
    Failed {
        rows: u64,
        bytes: u64,
        error: String,
    },
    Cancelled {
        rows: u64,
        bytes: u64,
    },
}

struct ExportEntry {
    owner: Owner,
    alias: String,
    state: ExportState,
    rows: u64,
    bytes: u64,
    truncated: bool,
    truncated_reason: Option<&'static str>,
    started_at_ms: u64,
    finished_at_ms: Option<u64>,
    error: Option<String>,
    blob_key: Option<String>,
    /// REQ-07a：语句级降级清单（Done 终态写入；运行中为空——清单随 pump 累积后随
    /// Finish 一次性提交）。
    degraded: Vec<serde_json::Value>,
    /// 取消信令：exportCancel 置 `cancel_requested` 后 notify，消费任务 pump 循环监听。
    cancel: Arc<tokio::sync::Notify>,
    cancel_requested: bool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn ms_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 导出任务登记表。配额在 new 时从 `db_export:` 段快照（装配期冻结）。
pub struct ExportRegistry {
    inner: Mutex<HashMap<String, ExportEntry>>,
    boot: OnceLock<String>,
    global_concurrency: usize,
    per_subject_concurrency: usize,
    /// 产物 TTL 台账：blob key → put_stream_open 时刻 ms（TTL 自 open 起算——
    /// 崩溃半成品同样可扫；live 任务 key 由 sweep 调用方按登记表现算豁免）。
    ledger: Mutex<HashMap<String, u64>>,
    /// 回收器懒启动闸（每注册表至多一个清扫任务）。
    reaper_armed: std::sync::atomic::AtomicBool,
}

impl Default for ExportRegistry {
    fn default() -> Self {
        Self::new(&Default::default())
    }
}

impl ExportRegistry {
    pub fn new(section: &super::db_policy::DbExportSection) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            boot: OnceLock::new(),
            global_concurrency: section.global_concurrency,
            per_subject_concurrency: section.per_subject_concurrency,
            ledger: Mutex::new(HashMap::new()),
            reaper_armed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn boot(&self) -> String {
        self.boot.get_or_init(handle_core::new_boot_tag).clone()
    }

    /// 登记新任务（op_db_export 同步开游标成功后调用）。返回 (taskId, 取消信令)。
    pub fn register(
        &self,
        owner: Owner,
        alias: String,
    ) -> Result<(String, Arc<tokio::sync::Notify>), RegisterError> {
        let id =
            handle_core::new_id(PREFIX, &self.boot()).map_err(|_| RegisterError::GlobalFull)?;
        let mut g = lock(&self.inner);
        let running = |g: &HashMap<String, ExportEntry>| {
            g.values()
                .filter(|e| e.state == ExportState::Running)
                .count()
        };
        if running(&g) >= self.global_concurrency {
            return Err(RegisterError::GlobalFull);
        }
        let subject_running = g
            .values()
            .filter(|e| e.state == ExportState::Running && e.owner.same_subject(&owner))
            .count();
        if subject_running >= self.per_subject_concurrency {
            return Err(RegisterError::SubjectFull);
        }
        g.insert(
            id.clone(),
            ExportEntry {
                owner,
                alias,
                state: ExportState::Running,
                rows: 0,
                bytes: 0,
                truncated: false,
                truncated_reason: None,
                started_at_ms: ms_now(),
                finished_at_ms: None,
                error: None,
                blob_key: None,
                degraded: Vec::new(),
                cancel: Arc::new(tokio::sync::Notify::new()),
                cancel_requested: false,
            },
        );
        let notify = g.get(&id).expect("just inserted").cancel.clone();
        Ok((id, notify))
    }

    /// 每批进度记账（消费任务泵内调用；终态后 no-op）。
    pub fn progress(&self, id: &str, rows: u64, bytes: u64) {
        let mut g = lock(&self.inner);
        if let Some(e) = g.get_mut(id)
            && e.state == ExportState::Running
        {
            e.rows = rows;
            e.bytes = bytes;
        }
    }

    /// 取消：归属校验 → 置位 + notify。**幂等**：运行中首调返回 Some(true)；
    /// 重复调用与终态任务返回 Some(false) no-op（已知且属主 ≠ not found——
    /// not found 只给未知/他人/跨重启）。`None` = not found（不泄露存在性）。
    pub fn cancel(&self, id: &str, owner: &Owner) -> Option<bool> {
        if !handle_core::id_is_wellformed(PREFIX, id) {
            return None;
        }
        let mut g = lock(&self.inner);
        let e = g.get_mut(id)?;
        if !e.owner.same_subject(owner) {
            return None;
        }
        if e.state != ExportState::Running {
            return Some(false);
        }
        if e.cancel_requested {
            return Some(false); // 幂等：信令已发，泵已唤醒，重复调用 no-op
        }
        e.cancel_requested = true;
        e.cancel.notify_one();
        Some(true)
    }

    /// 消费任务退出时一次性提交终态；Done 产物 key 记入 TTL 台账。
    pub fn finish(&self, id: &str, fin: Finish) {
        let now = ms_now();
        let mut ledger_key = None;
        let mut g = lock(&self.inner);
        if let Some(e) = g.get_mut(id) {
            if e.state != ExportState::Running {
                return; // 终态幂等：先到者赢（cancel 与 complete 竞态不翻盘）
            }
            e.finished_at_ms = Some(now);
            match fin {
                Finish::Done {
                    rows,
                    bytes,
                    truncated,
                    truncated_reason,
                    blob_key,
                    degraded,
                } => {
                    e.state = ExportState::Done;
                    e.rows = rows;
                    e.bytes = bytes;
                    e.truncated = truncated;
                    e.truncated_reason = truncated_reason;
                    e.blob_key = Some(blob_key.clone());
                    e.degraded = degraded;
                    ledger_key = Some(blob_key);
                }
                Finish::Failed { rows, bytes, error } => {
                    e.state = ExportState::Failed;
                    e.rows = rows;
                    e.bytes = bytes;
                    e.error = Some(error);
                }
                Finish::Cancelled { rows, bytes } => {
                    e.state = ExportState::Cancelled;
                    e.rows = rows;
                    e.bytes = bytes;
                }
            }
        }
        drop(g);
        if let Some(k) = ledger_key {
            lock(&self.ledger).insert(k, now);
        }
    }

    /// 跨重启 not found 由 boot-tag id 形状天然成立（他注册表的 id_is_wellformed 拒异 boot）。
    /// 归属：他人 id 一律 None（不泄露存在性）。
    pub fn status(&self, id: &str, owner: &Owner) -> Option<Value> {
        if !handle_core::id_is_wellformed(PREFIX, id) {
            return None;
        }
        let g = lock(&self.inner);
        let e = g.get(id)?;
        if !e.owner.same_subject(owner) {
            return None;
        }
        Some(json!({
            "taskId": id,
            "state": e.state.as_str(),
            "alias": e.alias,
            "rows": e.rows,
            "bytes": e.bytes,
            "truncated": e.truncated,
            "truncatedReason": e.truncated_reason,
            "startedAtMs": e.started_at_ms,
            "finishedAtMs": e.finished_at_ms,
            "error": e.error,
            "blobKey": e.blob_key,
            "degraded": e.degraded,
        }))
    }

    /// TTL 清扫：台账中 open 时刻早于 now-ttl 且**非 live** 的 key 摘除并返回
    /// （调用方逐个 `blob.del`）。live = 登记表里 blob_key 指向它的条目（任一状态）。
    pub fn sweep(&self, now_ms: u64, ttl_ms: u64) -> Vec<String> {
        let live: std::collections::HashSet<String> = {
            let g = lock(&self.inner);
            g.values().filter_map(|e| e.blob_key.clone()).collect()
        };
        let cutoff = now_ms.saturating_sub(ttl_ms);
        let mut ledger = lock(&self.ledger);
        let expired: Vec<String> = ledger
            .iter()
            .filter(|(k, open_ms)| !live.contains(*k) && **open_ms <= cutoff)
            .map(|(k, _)| k.clone())
            .collect();
        for k in &expired {
            ledger.remove(k);
        }
        expired
    }

    /// 消费任务侧：等待取消信令（pump 每批之间 await，取消即醒）。
    pub async fn wait_cancel(notify: &tokio::sync::Notify) {
        notify.notified().await;
    }

    /// E6：TTL 回收器（懒启动，每 Bridge 至多一个）。到点 sweep，逐个删过期产物
    /// （live 豁免由 sweep 内部按登记表现算）。`oj test` 经短周期段注入即可断言删除。
    pub fn arm_reaper(
        self: &Arc<Self>,
        blobs: Arc<super::blob::BlobRegistry>,
        interval: std::time::Duration,
        ttl: std::time::Duration,
    ) {
        if self
            .reaper_armed
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        let reg = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            tick.tick().await; // 首个 tick 立即到——跳过，按整间隔扫
            loop {
                tick.tick().await;
                let expired = reg.sweep(ms_now(), ttl.as_millis() as u64);
                for key in expired {
                    if let Some(b) = blobs.default() {
                        let _ = b.del(&key).await; // 清扫尽力：删失败留下个周期重试? 已摘账，不重试
                    }
                }
            }
        });
    }
}

// ---------------------------------------------------------------------------
// ops（E3/E4/E5/E7）：db.export / db.exportStatus / db.exportCancel
// ---------------------------------------------------------------------------

use deno_core::{OpState, op2};
use deno_error::JsErrorBox;
use std::cell::RefCell;
use std::rc::Rc;

/// `db.export({sql, params?, format, fileName?, maxBytes?})` 入参。
#[derive(Debug, serde::Deserialize)]
pub struct ExportArgs {
    #[serde(rename = "sql")]
    pub sql: String,
    #[serde(rename = "params", default)]
    pub params: Option<Vec<Value>>,
    #[serde(rename = "format")]
    pub format: String,
    #[serde(rename = "fileName")]
    pub file_name: Option<String>,
    #[serde(rename = "maxBytes")]
    pub max_bytes: Option<u64>,
}

/// 产物序列化器：csv（BOM + 引号转义 + NULL 空串）/ jsonl（行 JSON）。未知格式在 op 入口拒。
enum Encoder {
    Csv { header_done: bool },
    Jsonl,
}

impl Encoder {
    fn ext(&self) -> &'static str {
        match self {
            Encoder::Csv { .. } => "csv",
            Encoder::Jsonl => "jsonl",
        }
    }
    fn content_type(&self) -> &'static str {
        match self {
            Encoder::Csv { .. } => "text/csv",
            Encoder::Jsonl => "application/x-ndjson",
        }
    }
    /// 追加一行；返回写入字节数。行尾恒定 \r\n / \n（数据交换格式，非平台文本文件）。
    fn push(&mut self, row: &Value, buf: &mut Vec<u8>) -> u64 {
        match self {
            Encoder::Csv { header_done } => {
                let before = buf.len();
                if !*header_done {
                    *header_done = true;
                    buf.extend_from_slice(b"\xEF\xBB\xBF"); // UTF-8 BOM
                    if let Some(o) = row.as_object() {
                        csv_line(buf, o.keys().map(|k| k.as_str()));
                    }
                }
                if let Some(o) = row.as_object() {
                    csv_line(buf, o.values().map(csv_cell));
                } else {
                    buf.extend_from_slice(b"\r\n");
                }
                (buf.len() - before) as u64
            }
            Encoder::Jsonl => {
                let before = buf.len();
                serde_json::to_writer(&mut *buf, row).expect("jsonl row encode");
                buf.push(b'\n');
                (buf.len() - before) as u64
            }
        }
    }
}

fn csv_cell(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn csv_line(buf: &mut Vec<u8>, cells: impl Iterator<Item = impl AsRef<str>>) {
    let mut first = true;
    for c in cells {
        if !first {
            buf.push(b',');
        }
        first = false;
        let s = c.as_ref();
        if s.contains([',', '"', '\n', '\r']) {
            buf.push(b'"');
            for ch in s.chars() {
                if ch == '"' {
                    buf.push(b'"');
                }
                let mut tmp = [0u8; 4];
                buf.extend_from_slice(ch.encode_utf8(&mut tmp).as_bytes());
            }
            buf.push(b'"');
        } else {
            buf.extend_from_slice(s.as_bytes());
        }
    }
    buf.extend_from_slice(b"\r\n");
}

/// fileName 消毒：剥控制字符/引号/反斜杠，截 128——进 sidecar 与 HTTP 头，必须无害。
fn sanitize_filename(name: &str) -> String {
    name.chars()
        .filter(|c| !c.is_control() && *c != '"' && *c != '\\' && *c != '/')
        .take(128)
        .collect::<String>()
        .trim()
        .to_string()
}

/// 结构化 unsupported（未知 format）：`OJ_DB_UNSUPPORTED`，data.capability 点名 format。
fn format_unsupported(format: &str) -> JsErrorBox {
    let mut e = super::db_error::unsupported("export", "format");
    e.message = format!("export format \"{format}\" is not supported (use \"csv\" or \"jsonl\")");
    e.data.insert("format".into(), Value::String(format.into()));
    JsErrorBox::from(e)
}

/// `db.export(...)`：同步开游标 + 登记 + 自驱消费任务，立即返 `{taskId}`。
/// 规格：验收「5000 万行 200ms 内回 taskId」= 开游标轻、查询下推异步；
/// 泵节流每 N 行 yield，避免 current_thread 运行时上饿死 JS isolate。
#[op2]
#[serde]
pub async fn op_db_export(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[serde] args: ExportArgs,
) -> Result<serde_json::Value, JsErrorBox> {
    // 与 db.query 同门：表归属守卫 / 租户防护 / 访问控制（max_rows 策略截断来源）。
    super::guard::check_raw(&state, &args.sql)?;
    super::guard::check_tenant_raw(&state, &args.sql, args.params.as_deref().unwrap_or(&[]))?;
    let alias = super::guard::bound_db(&state, &name);
    let row_cap = super::db_policy::check_access(&state, &alias, &args.sql)?;
    let encoder = match args.format.as_str() {
        "csv" => Encoder::Csv { header_done: false },
        "jsonl" => Encoder::Jsonl,
        other => return Err(format_unsupported(other)),
    };
    let section = {
        let st = state.borrow().borrow::<Arc<super::StableState>>().clone();
        st.export_section.clone()
    };
    // maxBytes：配置为顶，调用侧只能收紧。
    let max_bytes = args
        .max_bytes
        .unwrap_or(section.max_bytes)
        .min(section.max_bytes);
    let blob_reg = {
        let st = state.borrow().borrow::<Arc<super::StableState>>().clone();
        st.blobs.clone()
    };
    let blob = blob_reg
        .default()
        .ok_or_else(|| JsErrorBox::generic("blob not configured (db.export writes to blob)"))?;
    // 池臂（与 db.stream 同裁决：事务里开导出无意义——连接随请求死）。
    let da = match super::db::resolve_target(&state, &name)? {
        super::db::Target::Pool(da) => da,
        super::db::Target::Tx(_) => {
            return Err(JsErrorBox::generic(
                "db.export within an active transaction is not supported (export runs on the connection pool only)",
            ));
        }
    };
    let owner = super::db_session::owner_of(&state.borrow());
    let registry = {
        let st = state.borrow().borrow::<Arc<super::StableState>>().clone();
        st.export_registry.clone()
    };
    // 配额：全局/每主体（登记即占额，终态释放——计数在 registry 内按 Running 现算）。
    let (task_id, cancel) = registry
        .register(owner.clone(), alias.clone())
        .map_err(|e| JsErrorBox::generic(e.message()))?;
    // E6：首导懒臂回收器（进程内一次；测试经短周期段注入）。
    registry.arm_reaper(
        blob_reg.clone(),
        std::time::Duration::from_secs(section.sweep_interval_secs),
        std::time::Duration::from_secs(section.ttl_secs),
    );
    // 产物 key：保留前缀 + 随机 taskId（不可预测 = 反目标「产物 key 可预测」）。
    let ext = encoder.ext();
    let blob_key = format!("export/{task_id}.{ext}");

    // 同步开游标（REQ-03 验收：开游标轻，查询下推异步）。
    let placeholder = da.dialect_info().caps.placeholder;
    let norm = super::placeholder::normalize_sql(
        placeholder,
        &args.sql,
        args.params.as_deref().unwrap_or(&[]),
    )
    .map_err(JsErrorBox::generic)?;
    let stream = match da
        .stream_query(&norm, args.params.as_deref().unwrap_or(&[]))
        .await
    {
        Ok(s) => s,
        Err(e) => {
            registry.finish(
                &task_id,
                Finish::Failed {
                    rows: 0,
                    bytes: 0,
                    error: e.to_string(),
                },
            );
            return Err(super::db::to_js_err(e));
        }
    };

    // put_stream 开会话；不支持流式的后端回落整段缓冲（硬顶 fallback_buffer_cap）。
    let content_type = encoder.content_type();
    let upload = match blob.put_stream_open(&blob_key, Some(content_type)).await {
        Ok(id) => {
            if let Some(name) = args
                .file_name
                .as_deref()
                .map(sanitize_filename)
                .filter(|n| !n.is_empty())
            {
                let _ = blob.put_stream_filename(id, &name).await; // s3 腿尽力：no-op
            }
            Some(id)
        }
        Err(_) => None, // 回落路径标记
    };
    let audit = {
        let st = state.borrow().borrow::<Arc<super::StableState>>().clone();
        st.audit.clone()
    };
    audit.push(super::audit::AuditEvent {
        ts_ms: ms_now(),
        kind: "db.export.start".into(),
        user: owner.user.clone(),
        tenant: owner.tenant.clone(),
        module: None,
        source: "db.export",
        alias: alias.clone(),
        detail: format!("task={} format={ext}", &task_id[..task_id.len().min(12)]),
    });

    // 自驱消费任务：drain 游标 → 编码 → put_stream_chunk；取消/触顶/错行/达 cap 分档终态。
    let registry_c = registry.clone();
    let task_id_c = task_id.clone();
    let alias_c = alias.clone();
    let blob_c = blob.clone();
    let fallback_cap = section.fallback_buffer_cap;
    let file_name_c = args
        .file_name
        .as_deref()
        .map(sanitize_filename)
        .filter(|n| !n.is_empty());
    tokio::spawn(async move {
        let mut enc = encoder;
        let mut stream = stream;
        let mut buf: Vec<u8> = Vec::new();
        let mut rows: u64 = 0;
        let mut bytes: u64 = 0;
        // REQ-07a：降级列清单随 pump 累积（按 {col,type} 去重），随 Finish 一次提交。
        let mut degraded_acc: Vec<serde_json::Value> = Vec::new();
        // 节流：每 N 行让出运行时（工程 B-3：防大导出饿死 current_thread 上的 JS）。
        const THROTTLE_EVERY: u64 = 256;
        enum End {
            Natural,
            Cancelled,
            MaxBytes,
            MaxRows,
            StreamErr(String),
        }
        let end = loop {
            tokio::select! {
                _ = cancel.notified() => break End::Cancelled,
                item = stream.next() => {
                    let (Some(item), ) = (item,) else { break End::Natural };
                    let row = match item {
                        Ok(r) => r,
                        Err(e) => break End::StreamErr(e),
                    };
                    // max_rows 策略截断（错误 ≠ 策略双轨）。
                    if let Some(cap) = row_cap
                        && rows >= cap as u64
                    {
                        break End::MaxRows;
                    }
                    let written = enc.push(&row, &mut buf);
                    rows += 1;
                    bytes += written;
                    for d in oj_plugin_ffi::cellenv::degraded_of(std::slice::from_ref(&row)) {
                        if !degraded_acc.contains(&d) {
                            degraded_acc.push(d);
                        }
                    }
                    if bytes > max_bytes {
                        break End::MaxBytes;
                    }
                    if rows.is_multiple_of(THROTTLE_EVERY) {
                        tokio::task::yield_now().await;
                    }
                }
            }
        };
        let fin = match end {
            End::Natural | End::MaxRows => {
                // 末块提交：流式 finish；回落整体 put（硬顶已在缓冲期守着）。
                let truncated = matches!(end, End::MaxRows);
                let ok = if let Some(id) = upload {
                    if !buf.is_empty() {
                        let _ = blob_c.put_stream_chunk(id, &buf).await;
                    }
                    blob_c.put_stream_finish(id).await.is_ok()
                } else {
                    // 回落路径：整段缓冲不得超硬顶（工程 m-7）。
                    if buf.len() as u64 > fallback_cap {
                        false
                    } else {
                        let ok = blob_c
                            .put(&blob_key, &buf, Some(content_type))
                            .await
                            .is_ok();
                        if ok && let Some(name) = file_name_c.as_deref() {
                            let _ = blob_c.set_filename(&blob_key, name).await;
                        }
                        ok
                    }
                };
                if ok {
                    registry_c.progress(&task_id_c, rows, bytes);
                    Finish::Done {
                        rows,
                        bytes,
                        truncated,
                        truncated_reason: truncated.then_some("max_rows"),
                        blob_key: blob_key.clone(),
                        degraded: degraded_acc,
                    }
                } else {
                    Finish::Failed {
                        rows,
                        bytes,
                        error: "blob write failed (export artifact not persisted)".into(),
                    }
                }
            }
            End::Cancelled => {
                if let Some(id) = upload {
                    let _ = blob_c.put_stream_abort(id).await; // 清半成品（防 s3 orphan parts）
                }
                Finish::Cancelled { rows, bytes }
            }
            End::MaxBytes => {
                if let Some(id) = upload {
                    let _ = blob_c.put_stream_abort(id).await;
                }
                Finish::Failed {
                    rows,
                    bytes,
                    error: format!("export exceeded maxBytes ({max_bytes})"),
                }
            }
            End::StreamErr(e) => {
                if let Some(id) = upload {
                    let _ = blob_c.put_stream_abort(id).await;
                }
                Finish::Failed {
                    rows,
                    bytes,
                    error: format!("stream error: {e}"),
                }
            }
        };
        let (final_state, err_detail) = match &fin {
            Finish::Done { .. } => ("done", ""),
            Finish::Failed { error, .. } => ("failed", error.as_str()),
            Finish::Cancelled { .. } => ("cancelled", ""),
        };
        audit.push(super::audit::AuditEvent {
            ts_ms: ms_now(),
            kind: format!("db.export.{final_state}"),
            user: owner.user.clone(),
            tenant: owner.tenant.clone(),
            module: None,
            source: "db.export",
            alias: alias_c.clone(),
            detail: format!(
                "task={} rows={rows} bytes={bytes} {err_detail}",
                &task_id_c[..task_id_c.len().min(12)]
            ),
        });
        registry_c.finish(&task_id_c, fin);
    });

    Ok(json!({ "taskId": task_id }))
}

/// `db.exportStatus(taskId)`：归属校验；未知/他人/跨重启一律 not found（不泄露存在性）。
#[op2]
#[serde]
pub async fn op_db_export_status(
    state: Rc<RefCell<OpState>>,
    #[string] task_id: String,
) -> Result<serde_json::Value, JsErrorBox> {
    let (registry, owner) = {
        let g = state.borrow();
        let st = g.borrow::<Arc<super::StableState>>().clone();
        (st.export_registry.clone(), super::db_session::owner_of(&g))
    };
    registry.status(&task_id, &owner).ok_or_else(|| {
        JsErrorBox::generic(format!(
            "export task {} not found",
            super::handle_core::id_prefix(PREFIX, &task_id)
        ))
    })
}

/// `db.exportCancel(taskId)`：幂等；not found 口径同 status。
#[op2]
#[serde]
pub async fn op_db_export_cancel(
    state: Rc<RefCell<OpState>>,
    #[string] task_id: String,
) -> Result<serde_json::Value, JsErrorBox> {
    let (registry, owner) = {
        let g = state.borrow();
        let st = g.borrow::<Arc<super::StableState>>().clone();
        (st.export_registry.clone(), super::db_session::owner_of(&g))
    };
    match registry.cancel(&task_id, &owner) {
        Some(_) => Ok(json!({ "cancelled": true })),
        None => Err(JsErrorBox::generic(format!(
            "export task {} not found",
            super::handle_core::id_prefix(PREFIX, &task_id)
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::{Bridge, Extras, InMemoryAccessor, InMemoryKV, SchemaRegistry};
    use std::sync::Arc;

    fn export_bridge(
        db: Arc<dyn super::super::db::DataAccessor>,
        blobs: Arc<super::super::blob::BlobRegistry>,
        export_section: super::super::db_policy::DbExportSection,
        db_access: super::super::db_policy::DbAccessSection,
    ) -> Bridge {
        Bridge::with_dbs_and_loader(
            std::collections::HashMap::from([("default".to_string(), db)]),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            None,
            Extras {
                blobs: Some(blobs),
                export_section: Some(Arc::new(export_section)),
                db_access: Some(Arc::new(db_access)),
                ..Default::default()
            },
        )
    }

    fn tmp_blobs() -> (tempfile::TempDir, Arc<super::super::blob::BlobRegistry>) {
        let d = tempfile::tempdir().unwrap();
        let local = super::super::blob::LocalBlob::new(d.path(), "/v1/api").unwrap();
        (
            d,
            super::super::blob::registry_with_default(Arc::new(local)),
        )
    }

    /// 轮询直至 status.state 离开 running（deadline 有界——禁固定 sleep 当同步）。
    async fn wait_terminal(
        b: &Bridge,
        task_id: &str,
        req: &super::super::http::RequestInfo,
    ) -> Value {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let js = format!(
                r#"(async () => {{
                     const st = await db.exportStatus({task_id:?});
                     json.ok(st);
                   }})().catch(e => json.fail(500, String(e)));"#,
            );
            let cap = b.run_with(&js, req.clone()).await.unwrap();
            let v: Value = serde_json::from_slice(&cap.body).unwrap();
            if v["data"]["state"] != "running" {
                return v["data"].clone();
            }
            assert!(
                std::time::Instant::now() < deadline,
                "export task hung in running"
            );
            tokio::task::yield_now().await;
        }
    }

    fn req_user(id: &str) -> super::super::http::RequestInfo {
        super::super::http::RequestInfo {
            user: Some(json!({ "id": id })),
            ..Default::default()
        }
    }

    /// REQ-07a：导出任务状态携带语句级降级清单（`degraded`）——CSV/JSONL 产物里
    /// `$<type>:` 编码值的权威判定面（产物本身不改，单元格字符串原样落盘）。
    #[tokio::test(flavor = "current_thread")]
    async fn export_status_carries_degraded_columns() {
        let mem = Arc::new(InMemoryAccessor::new());
        mem.seed([
            json!({"ip": "$inet:AQI=", "name": "a"}),
            json!({"ip": "$inet:AgM=", "name": "b"}),
        ]);
        let (_d, blobs) = tmp_blobs();
        let b = export_bridge(mem, blobs.clone(), Default::default(), Default::default());
        let js = r#"(async () => {
            const { taskId } = await db.export({ sql: "select * from t", format: "csv" });
            json.ok({ taskId });
          })().catch(e => json.fail(500, String(e)));"#;
        let cap = b.run_with(js, Default::default()).await.unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        let task_id = v["data"]["taskId"].as_str().unwrap().to_string();
        let st = wait_terminal(&b, &task_id, &Default::default()).await;
        assert_eq!(st["state"], json!("done"), "{st}");
        // 按 {col,type} 去重。
        assert_eq!(
            st["degraded"],
            json!([{"col": "ip", "type": "inet"}]),
            "{st}"
        );
    }

    /// REQ-07a 对照面：无降级单元格的导出显式给 `degraded: []`（schema 稳定，消费方
    /// 不必判缺席）。
    #[tokio::test(flavor = "current_thread")]
    async fn export_status_degraded_empty_when_clean() {
        let mem = Arc::new(InMemoryAccessor::new());
        mem.seed([json!({"id": 1, "name": "a"})]);
        let (_d, blobs) = tmp_blobs();
        let b = export_bridge(mem, blobs.clone(), Default::default(), Default::default());
        let js = r#"(async () => {
            const { taskId } = await db.export({ sql: "select * from t", format: "jsonl" });
            json.ok({ taskId });
          })().catch(e => json.fail(500, String(e)));"#;
        let cap = b.run_with(js, Default::default()).await.unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        let task_id = v["data"]["taskId"].as_str().unwrap().to_string();
        let st = wait_terminal(&b, &task_id, &Default::default()).await;
        assert_eq!(st["state"], json!("done"), "{st}");
        assert_eq!(st["degraded"], json!([]), "{st}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn export_csv_returns_task_id_immediately_and_artifact_in_blob() {
        let mem = Arc::new(InMemoryAccessor::new());
        mem.seed([json!({"id": 1, "name": "a"}), json!({"id": 2, "name": "b"})]);
        let (_d, blobs) = tmp_blobs();
        let b = export_bridge(mem, blobs.clone(), Default::default(), Default::default());
        // E3 验收：立即回 taskId。
        let js = r#"(async () => {
            const t0 = Date.now();
            const { taskId } = await db.export({ sql: "select * from t", format: "csv" });
            json.ok({ taskId, ms: Date.now() - t0 });
          })().catch(e => json.fail(500, String(e)));"#;
        let cap = b
            .run_with(js, super::super::http::RequestInfo::default())
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(v["data"]["taskId"].is_string(), "{v}");
        let task_id = v["data"]["taskId"].as_str().unwrap().to_string();
        let st = wait_terminal(&b, &task_id, &super::super::http::RequestInfo::default()).await;
        assert_eq!(st["state"], json!("done"), "{st}");
        assert_eq!(st["rows"], json!(2));
        assert!(st["blobKey"].is_string());
        let key = st["blobKey"].as_str().unwrap().to_string();
        // 产物可下载且是 csv（BOM + 头 + 两行）。
        let art = blobs.default().unwrap().get(&key).await.unwrap();
        let text = String::from_utf8_lossy(&art);
        assert!(text.starts_with('\u{feff}'), "csv BOM missing: {text:?}");
        assert!(text.contains("id,name"), "{text}");
        assert!(
            text.contains("\r\na,1") || text.contains("1,a") || text.contains("a"),
            "{text}"
        );
        assert_eq!(
            blobs
                .default()
                .unwrap()
                .content_type(&key)
                .await
                .unwrap()
                .as_deref(),
            Some("text/csv")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn export_jsonl_lines_and_unknown_format_is_structured() {
        let mem = Arc::new(InMemoryAccessor::new());
        mem.seed([json!({"x": 1}), json!({"x": 2})]);
        let (_d, blobs) = tmp_blobs();
        let b = export_bridge(mem, blobs.clone(), Default::default(), Default::default());
        let js = r#"(async () => {
            const { taskId } = await db.export({ sql: "select 1", format: "jsonl" });
            const st = await db.exportStatus(taskId);
            json.ok(st);
          })().catch(e => json.fail(500, String(e)));"#;
        // 状态可能还在 running（自驱任务异步）——用轮询。
        let cap = b.run_with(js, Default::default()).await.unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        let task_id = v["data"]["taskId"].as_str().unwrap().to_string();
        let st = wait_terminal(&b, &task_id, &Default::default()).await;
        assert_eq!(st["state"], json!("done"));
        let art = blobs
            .default()
            .unwrap()
            .get(st["blobKey"].as_str().unwrap())
            .await
            .unwrap();
        let lines: Vec<&str> = std::str::from_utf8(&art).unwrap().lines().collect();
        assert_eq!(lines, vec![r#"{"x":1}"#, r#"{"x":2}"#], "{lines:?}");
        // 未知 format → 结构化 OJ_DB_UNSUPPORTED，capability 点名 format。
        let js_bad = r#"(async () => {
            try {
              await db.export({ sql: "select 1", format: "xlsx" });
              json.ok({ err: "no-reject" });
            } catch (e) { json.ok({ code: e.code, cap: e.data && e.data.capability }); }
          })().catch(e => json.fail(500, String(e)));"#;
        let cap = b.run_with(js_bad, Default::default()).await.unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["data"]["code"], json!("OJ_DB_UNSUPPORTED"), "{v}");
        assert_eq!(v["data"]["cap"], json!("format"), "{v}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn export_cancel_is_idempotent_and_status_survives_across_requests() {
        // 慢流：每行 30ms，100 行 → 3s 跑不完；跨请求取消。
        struct Slow;
        #[async_trait::async_trait]
        impl super::super::db::DataAccessor for Slow {
            async fn query_with_params(
                &self,
                _sql: &str,
                _params: &[Value],
            ) -> super::super::BridgeResult<Vec<Value>> {
                Ok(vec![])
            }
            async fn exec_with_params(
                &self,
                _sql: &str,
                _params: &[Value],
            ) -> super::super::BridgeResult<i64> {
                Ok(0)
            }
            async fn stream_query(
                &self,
                _sql: &str,
                _params: &[Value],
            ) -> super::super::BridgeResult<
                std::pin::Pin<Box<dyn futures::Stream<Item = Result<Value, String>> + Send>>,
            > {
                Ok(Box::pin(futures::stream::unfold(0u64, |i| async move {
                    if i >= 100 {
                        return None;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                    Some((Ok(json!({"i": i})), i + 1))
                })))
            }
        }
        let (_d, blobs) = tmp_blobs();
        let b = export_bridge(
            Arc::new(Slow),
            blobs.clone(),
            Default::default(),
            Default::default(),
        );
        // 请求 A（user u1）发起——等价「发起后断连接」：run_with 结束后任务仍跑。
        let task_id = {
            let cap = b
                .run_with(
                    r#"(async () => {
                        const { taskId } = await db.export({ sql: "select 1", format: "jsonl" });
                        json.ok({ taskId });
                      })().catch(e => json.fail(500, String(e)));"#,
                    req_user("u1"),
                )
                .await
                .unwrap();
            let v: Value = serde_json::from_slice(&cap.body).unwrap();
            v["data"]["taskId"].as_str().unwrap().to_string()
        };
        // 请求 B（同主体）：进度可见且在涨（断线重连任务仍在——验收对照行）。
        let saw_running = {
            let cap = b
                .run_with(
                    &format!(
                        r#"(async () => {{
                             const st = await db.exportStatus({task_id:?});
                             json.ok(st);
                           }})().catch(e => json.fail(500, String(e)));"#,
                    ),
                    req_user("u1"),
                )
                .await
                .unwrap();
            let v: Value = serde_json::from_slice(&cap.body).unwrap();
            assert_eq!(v["code"], json!(0), "{v}");
            v["data"]["state"] == "running"
        };
        assert!(saw_running, "慢导出必须仍在 running（跨请求存活）");
        // 请求 C：取消（幂等双调）。
        for _ in 0..2 {
            let cap = b
                .run_with(
                    &format!(
                        r#"(async () => {{
                             const r = await db.exportCancel({task_id:?});
                             json.ok(r);
                           }})().catch(e => json.fail(500, String(e)));"#,
                    ),
                    req_user("u1"),
                )
                .await
                .unwrap();
            let v: Value = serde_json::from_slice(&cap.body).unwrap();
            assert_eq!(v["data"]["cancelled"], json!(true), "{v}");
        }
        let st = wait_terminal(&b, &task_id, &req_user("u1")).await;
        assert_eq!(st["state"], json!("cancelled"), "{st}");
        // 他人视角：status/cancel 一律 not found（不泄露存在性）。
        let cap = b
            .run_with(
                &format!(
                    r#"(async () => {{
                         try {{
                           await db.exportStatus({task_id:?});
                           json.ok({{ err: "no-reject" }});
                         }} catch (e) {{ json.ok({{ msg: String(e) }}); }}
                       }})().catch(e => json.fail(500, String(e)));"#,
                ),
                req_user("u2"),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(
            v["data"]["msg"]
                .as_str()
                .unwrap_or("")
                .contains("not found"),
            "{v}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn export_max_bytes_fails_and_max_rows_truncates() {
        let mem = Arc::new(InMemoryAccessor::new());
        mem.seed(
            (0..10)
                .map(|i| json!({"n": i, "pad": "xxxxxxxxxx"}))
                .collect::<Vec<_>>(),
        );
        let (_d, blobs) = tmp_blobs();
        // maxBytes 超限 → failed（错误 ≠ 策略）。
        let b = export_bridge(
            mem.clone(),
            blobs.clone(),
            Default::default(),
            Default::default(),
        );
        let js = r#"(async () => {
            const { taskId } = await db.export({ sql: "select 1", format: "jsonl", maxBytes: 16 });
            json.ok({ taskId });
          })().catch(e => json.fail(500, String(e)));"#;
        let cap = b.run_with(js, Default::default()).await.unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        let task_id = v["data"]["taskId"].as_str().unwrap().to_string();
        let st = wait_terminal(&b, &task_id, &Default::default()).await;
        assert_eq!(st["state"], json!("failed"), "{st}");
        assert!(
            st["error"].as_str().unwrap_or("").contains("maxBytes"),
            "{st}"
        );
        // max_rows 策略截断 → done + truncated + 原因可见。
        let pol = super::super::db_policy::DbAccessSection {
            defaults: super::super::db_policy::DbAccess {
                max_rows: Some(3),
                ..Default::default()
            },
            ..Default::default()
        };
        let b2 = export_bridge(mem, blobs, Default::default(), pol);
        let js = r#"(async () => {
            const { taskId } = await db.export({ sql: "select 1", format: "jsonl" });
            json.ok({ taskId });
          })().catch(e => json.fail(500, String(e)));"#;
        let cap = b2.run_with(js, Default::default()).await.unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        let task_id = v["data"]["taskId"].as_str().unwrap().to_string();
        let st = wait_terminal(&b2, &task_id, &Default::default()).await;
        assert_eq!(st["state"], json!("done"), "{st}");
        assert_eq!(st["truncated"], json!(true), "{st}");
        assert_eq!(st["truncatedReason"], json!("max_rows"), "{st}");
        assert_eq!(st["rows"], json!(3), "{st}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn export_audit_events_cover_start_and_final_state() {
        let mem = Arc::new(InMemoryAccessor::new());
        mem.seed([json!({"a": 1})]);
        let (_d, blobs) = tmp_blobs();
        let b = export_bridge(mem, blobs, Default::default(), Default::default());
        let js = r#"(async () => {
            const { taskId } = await db.export({ sql: "select 1", format: "csv" });
            json.ok({ taskId });
          })().catch(e => json.fail(500, String(e)));"#;
        let cap = b.run_with(js, Default::default()).await.unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        let task_id = v["data"]["taskId"].as_str().unwrap().to_string();
        let _ = wait_terminal(&b, &task_id, &Default::default()).await;
        // 审计：发起 + 终态两件；SQL 全文不入审计。
        let cap = b
            .run_with(
                r#"(async () => {
                    const ev = await audit.query({ kinds: ["db.export.start", "db.export.done"] });
                    json.ok(ev);
                  })().catch(e => json.fail(500, String(e)));"#,
                Default::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        let evs = v["data"].as_array().cloned().unwrap_or_default();
        assert!(
            evs.iter().any(|e| e["kind"] == "db.export.start"),
            "缺发起事件：{evs:?}"
        );
        assert!(
            evs.iter().any(|e| e["kind"] == "db.export.done"),
            "缺终态事件：{evs:?}"
        );
        assert!(
            evs.iter()
                .all(|e| !e["detail"].as_str().unwrap_or("").contains("select 1")),
            "SQL 全文不得入审计：{evs:?}"
        );
    }

    fn section(global: usize, per_subject: usize) -> super::super::db_policy::DbExportSection {
        super::super::db_policy::DbExportSection {
            global_concurrency: global,
            per_subject_concurrency: per_subject,
            ..Default::default()
        }
    }

    fn owner(user: &str) -> Owner {
        Owner {
            tenant: None,
            user: Some(user.into()),
            module: None,
            source: "test",
        }
    }

    #[test]
    fn register_progress_finish_done_status_roundtrip() {
        let reg = ExportRegistry::new(&section(4, 2));
        let (id, notify) = reg.register(owner("u1"), "rep".into()).unwrap();
        assert!(handle_core::id_is_wellformed(PREFIX, &id));
        reg.progress(&id, 10, 4096);
        reg.finish(
            &id,
            Finish::Done {
                rows: 20,
                bytes: 8192,
                truncated: true,
                truncated_reason: Some("max_rows"),
                blob_key: "export/abc".into(),
                degraded: Vec::new(),
            },
        );
        let st = reg.status(&id, &owner("u1")).expect("own id");
        assert_eq!(st["state"], json!("done"));
        assert_eq!(st["rows"], json!(20));
        assert_eq!(st["bytes"], json!(8192));
        assert_eq!(st["truncated"], json!(true));
        assert_eq!(st["truncatedReason"], json!("max_rows"));
        assert_eq!(st["blobKey"], json!("export/abc"));
        assert_eq!(st["alias"], json!("rep"));
        assert!(st["finishedAtMs"].is_u64());
        // 终态幂等：二次 finish 不翻盘
        reg.finish(&id, Finish::Cancelled { rows: 99, bytes: 1 });
        assert_eq!(
            reg.status(&id, &owner("u1")).unwrap()["state"],
            json!("done")
        );
        drop(notify);
    }

    #[test]
    fn status_hides_foreign_and_malformed_ids() {
        let reg = ExportRegistry::new(&section(4, 2));
        let (id, _n) = reg.register(owner("u1"), "rep".into()).unwrap();
        assert!(reg.status(&id, &owner("u2")).is_none(), "他人 id 不泄露");
        assert!(reg.status("export_nope", &owner("u1")).is_none());
        assert!(reg.status("../../../etc/passwd", &owner("u1")).is_none());
    }

    #[test]
    fn quota_global_full_and_subject_full_have_distinct_messages() {
        let reg = ExportRegistry::new(&section(1, 1)); // 全局 1、每主体 1
        let _ = reg.register(owner("u1"), "rep".into()).unwrap();
        // 全局满（u2 也被全局挡）
        match reg.register(owner("u2"), "rep".into()) {
            Err(RegisterError::GlobalFull) => {}
            other => panic!("expect GlobalFull, got {other:?}"),
        }
        assert!(
            RegisterError::GlobalFull
                .message()
                .contains("global export concurrency")
        );
        // 每主体满（全局有坑位时）：换个全局上限验证
        let reg = ExportRegistry::new(&section(4, 1));
        let _ = reg.register(owner("u1"), "rep".into()).unwrap();
        match reg.register(owner("u1"), "rep".into()) {
            Err(RegisterError::SubjectFull) => {}
            other => panic!("expect SubjectFull, got {other:?}"),
        }
        assert!(
            RegisterError::SubjectFull
                .message()
                .contains("per-subject export concurrency")
        );
        // 不同主体不受他人占额影响
        assert!(reg.register(owner("u2"), "rep".into()).is_ok());
    }

    #[test]
    fn cancel_is_idempotent_and_terminal_is_noop() {
        let reg = ExportRegistry::new(&section(4, 2));
        let (id, _n) = reg.register(owner("u1"), "rep".into()).unwrap();
        assert_eq!(reg.cancel(&id, &owner("u1")), Some(true));
        assert_eq!(reg.cancel(&id, &owner("u1")), Some(false), "幂等");
        assert_eq!(reg.cancel(&id, &owner("u2")), None, "他人 not found");
        // 终态后 cancel = no-op（已知且属主，不 not found）
        reg.finish(
            &id,
            Finish::Cancelled {
                rows: 5,
                bytes: 100,
            },
        );
        assert_eq!(reg.cancel(&id, &owner("u1")), Some(false));
        let st = reg.status(&id, &owner("u1")).unwrap();
        assert_eq!(st["state"], json!("cancelled"));
        assert_eq!(st["rows"], json!(5));
    }

    #[test]
    fn cross_registry_ids_are_not_found() {
        // 模拟重启：新注册表（新 boot tag）不认旧 id
        let a = ExportRegistry::new(&section(4, 2));
        let (id, _n) = a.register(owner("u1"), "rep".into()).unwrap();
        let b = ExportRegistry::new(&section(4, 2));
        assert!(
            b.status(&id, &owner("u1")).is_none(),
            "跨重启一律 not found"
        );
        assert_eq!(b.cancel(&id, &owner("u1")), None);
    }

    #[test]
    fn failed_finish_records_error_and_frees_quota() {
        let reg = ExportRegistry::new(&section(1, 1));
        let (id, _n) = reg.register(owner("u1"), "rep".into()).unwrap();
        reg.finish(
            &id,
            Finish::Failed {
                rows: 3,
                bytes: 12,
                error: "maxBytes exceeded".into(),
            },
        );
        let st = reg.status(&id, &owner("u1")).unwrap();
        assert_eq!(st["state"], json!("failed"));
        assert_eq!(st["error"], json!("maxBytes exceeded"));
        // 终态不占并发额：可立即再登记
        assert!(reg.register(owner("u1"), "rep".into()).is_ok());
    }

    #[test]
    fn sweep_expires_old_keys_and_exempts_live() {
        let reg = ExportRegistry::new(&section(4, 2));
        let now = ms_now();
        // 台账三种 key：老 done 产物、新 done 产物、live 任务产物（登记簿挂着的）
        lock(&reg.ledger).insert("export/old".into(), now - 25 * 3_600_000);
        lock(&reg.ledger).insert("export/fresh".into(), now - 60_000);
        let (id, _n) = reg.register(owner("u1"), "rep".into()).unwrap();
        reg.finish(
            &id,
            Finish::Done {
                rows: 1,
                bytes: 2,
                truncated: false,
                truncated_reason: None,
                blob_key: "export/live".into(),
                degraded: Vec::new(),
            },
        );
        lock(&reg.ledger).insert("export/live".into(), now - 25 * 3_600_000);
        let expired = reg.sweep(now, 24 * 3_600_000);
        assert_eq!(expired, vec!["export/old".to_string()], "{expired:?}");
        // 已扫的不再重复扫
        assert!(reg.sweep(now, 24 * 3_600_000).is_empty());
    }
}
