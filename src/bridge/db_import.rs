//! 任务式批量导入（REQ-12，v0.1.115）：`db.import/importStatus/importCancel`。
//!
//! 任务式批量导入登记（`db.import` / `importStatus` / `importCancel`，REQ-12 v0.1.115）。
//!
//! 壳形状照 [`db_export::ExportRegistry`]（U-18 同款）：`Arc` 不可变挂 `StableState`、
//! 内部 `Mutex` 变动、惰性清扫。配额计数按 state==Running 现算。差异：entry 无 blob
//! 产物（无 TTL 台账，清扫即摘表）；加 `table`/`samples`/`error_count`（errors 计数
//! 独立字段——samples 截 20 但 errors 语义计数不受截断影响）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use serde::Serialize;
use serde_json::{Value, json};

use super::handle_core::{self, Owner};
use std::sync::atomic::Ordering;

/// taskId 前缀（与 `exp_`/`exec_`/`tx_` 隔离；形状白名单共用）。
pub const PREFIX: &str = "imp_";

/// samples 上限（spec §3：最多 20 条示例）。
const MAX_SAMPLES: usize = 20;

/// 导入任务状态机。serde lowercase ↔ JS 面 `state: "running"|"done"|"failed"|"cancelled"`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportState {
    Running,
    Done,
    Failed,
    Cancelled,
}

impl ImportState {
    fn as_str(&self) -> &'static str {
        match self {
            ImportState::Running => "running",
            ImportState::Done => "done",
            ImportState::Failed => "failed",
            ImportState::Cancelled => "cancelled",
        }
    }
}

/// 单行错误样本（spec §3：`{line, reason, raw}`——line = 源文件绝对行号，1-based）。
#[derive(Debug, Clone, Serialize)]
pub struct ImportSample {
    pub line: u64,
    pub reason: String,
    pub raw: Option<String>,
}

/// 登记失败：两档配额文案必须区分（全局满 / 本人满）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisterError {
    GlobalFull,
    SubjectFull,
}

impl RegisterError {
    pub fn message(&self) -> &'static str {
        match self {
            RegisterError::GlobalFull => {
                "db.import: global import concurrency limit reached, try again later"
            }
            RegisterError::SubjectFull => {
                "db.import: per-subject import concurrency limit reached, try again later"
            }
        }
    }
}

/// 终态写入（consumer 退出时一次性提交；先 settle 者赢——cancel 与 complete
/// 竞态不互相翻盘，同 export finish 口径）。
#[derive(Debug)]
pub enum Finish {
    Done {
        rows: u64,
        bytes: u64,
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

struct ImportEntry {
    owner: Owner,
    alias: String,
    table: String,
    state: ImportState,
    rows: u64,
    bytes: u64,
    /// 错误样本（≤ MAX_SAMPLES，先到者保留）与独立语义计数（status `errors`）。
    samples: Vec<ImportSample>,
    error_count: u64,
    started_at_ms: u64,
    finished_at_ms: Option<u64>,
    error: Option<String>,
    /// 取消信令：importCancel 置 `cancel_requested` 后 notify_waiters（多等待者
    /// 口径同 REQ-11）；consumer 批间查 sticky 标志，唤醒不丢。
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

/// 导入任务登记表。配额在 new 时从 `db_import:` 段快照（装配期冻结）。
pub struct ImportRegistry {
    inner: Mutex<HashMap<String, ImportEntry>>,
    boot: OnceLock<String>,
    global_concurrency: usize,
    per_subject_concurrency: usize,
    /// 回收器懒启动闸（每注册表至多一个清扫任务）。
    reaper_armed: std::sync::atomic::AtomicBool,
}

impl Default for ImportRegistry {
    fn default() -> Self {
        Self::new(&Default::default())
    }
}

impl ImportRegistry {
    pub fn new(section: &super::db_policy::DbImportSection) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            boot: OnceLock::new(),
            global_concurrency: section.global_concurrency,
            per_subject_concurrency: section.per_subject_concurrency,
            reaper_armed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn boot(&self) -> String {
        self.boot.get_or_init(handle_core::new_boot_tag).clone()
    }

    /// 登记新任务（op_db_import 守卫矩阵与配额全过后调用）。返回 (taskId, 取消信令)。
    pub fn register(
        &self,
        owner: Owner,
        alias: String,
        table: String,
    ) -> Result<(String, Arc<tokio::sync::Notify>), RegisterError> {
        let id =
            handle_core::new_id(PREFIX, &self.boot()).map_err(|_| RegisterError::GlobalFull)?;
        let mut g = lock(&self.inner);
        let running = |g: &HashMap<String, ImportEntry>| {
            g.values()
                .filter(|e| e.state == ImportState::Running)
                .count()
        };
        if running(&g) >= self.global_concurrency {
            return Err(RegisterError::GlobalFull);
        }
        let subject_running = g
            .values()
            .filter(|e| e.state == ImportState::Running && e.owner.same_subject(&owner))
            .count();
        if subject_running >= self.per_subject_concurrency {
            return Err(RegisterError::SubjectFull);
        }
        g.insert(
            id.clone(),
            ImportEntry {
                owner,
                alias,
                table,
                state: ImportState::Running,
                rows: 0,
                bytes: 0,
                samples: Vec::new(),
                error_count: 0,
                started_at_ms: ms_now(),
                finished_at_ms: None,
                error: None,
                cancel: Arc::new(tokio::sync::Notify::new()),
                cancel_requested: false,
            },
        );
        let notify = g.get(&id).expect("just inserted").cancel.clone();
        Ok((id, notify))
    }

    /// 每批进度记账（consumer 批间调用；终态后 no-op）。
    pub fn progress(&self, id: &str, rows: u64, bytes: u64) {
        let mut g = lock(&self.inner);
        if let Some(e) = g.get_mut(id)
            && e.state == ImportState::Running
        {
            e.rows = rows;
            e.bytes = bytes;
        }
    }

    /// 错误样本入账：error_count 恒 +1（语义计数），samples 截到 MAX_SAMPLES
    /// （先到者保留——与源文件头部对应，尾部截断）。终态后 no-op。
    pub fn push_sample(&self, id: &str, sample: ImportSample) {
        let mut g = lock(&self.inner);
        if let Some(e) = g.get_mut(id)
            && e.state == ImportState::Running
        {
            e.error_count += 1;
            if e.samples.len() < MAX_SAMPLES {
                e.samples.push(sample);
            }
        }
    }

    /// 取消：归属校验 → 置位 + notify_waiters。**幂等**：运行中首调返回 Some(true)；
    /// 重复调用与终态任务返回 Some(false) no-op。`None` = not found（不泄露存在性）。
    pub fn cancel(&self, id: &str, owner: &Owner) -> Option<bool> {
        if !handle_core::id_is_wellformed(PREFIX, id) {
            return None;
        }
        let mut g = lock(&self.inner);
        let e = g.get_mut(id)?;
        if !e.owner.same_subject(owner) {
            return None;
        }
        if e.state != ImportState::Running {
            return Some(false);
        }
        if e.cancel_requested {
            return Some(false); // 幂等：标志已置，consumer 批间必见，重复调用 no-op
        }
        e.cancel_requested = true;
        e.cancel.notify_waiters();
        Some(true)
    }

    /// consumer 批间取消检查（sticky 标志——notify_waiters 单发不留 permit，
    /// 批间轮询标志才是权威，同 REQ-11 F2 修法口径）。
    pub fn cancel_requested(&self, id: &str) -> bool {
        let g = lock(&self.inner);
        g.get(id).is_some_and(|e| e.cancel_requested)
    }

    /// consumer 退出时一次性提交终态；Running 才改写（先 settle 者赢）。
    pub fn finish(&self, id: &str, fin: Finish) {
        let now = ms_now();
        let mut g = lock(&self.inner);
        if let Some(e) = g.get_mut(id) {
            if e.state != ImportState::Running {
                return; // 终态幂等：先到者赢
            }
            e.finished_at_ms = Some(now);
            match fin {
                Finish::Done { rows, bytes } => {
                    e.state = ImportState::Done;
                    e.rows = rows;
                    e.bytes = bytes;
                }
                Finish::Failed { rows, bytes, error } => {
                    e.state = ImportState::Failed;
                    e.rows = rows;
                    e.bytes = bytes;
                    e.error = Some(error);
                }
                Finish::Cancelled { rows, bytes } => {
                    e.state = ImportState::Cancelled;
                    e.rows = rows;
                    e.bytes = bytes;
                }
            }
        }
    }

    /// 跨重启 not found 由 boot-tag id 形状天然成立。归属：他人 id 一律 None。
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
            "table": e.table,
            "rows": e.rows,
            "bytes": e.bytes,
            "errors": e.error_count,
            "samples": e.samples,
            "startedAtMs": e.started_at_ms,
            "finishedAtMs": e.finished_at_ms,
            "error": e.error,
        }))
    }

    /// TTL 清扫：**终态**且 finished_at 早于 now-ttl 的条目摘表并返回 id
    /// （调用方即丢——import 无 blob 产物）；Running 豁免（在跑任务永不扫）。
    pub fn sweep(&self, now_ms: u64, ttl_ms: u64) -> Vec<String> {
        let cutoff = now_ms.saturating_sub(ttl_ms);
        let mut g = lock(&self.inner);
        let expired: Vec<String> = g
            .iter()
            .filter(|(_, e)| {
                e.state != ImportState::Running && e.finished_at_ms.is_some_and(|f| f <= cutoff)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in &expired {
            g.remove(id);
        }
        expired
    }

    /// TTL 回收器（懒启动，每 Registry 至多一个）。到点 sweep 摘表即丢
    /// （import 无 blob 产物，扫完无需清理外部存储）。
    pub fn arm_reaper(self: &Arc<Self>, interval: std::time::Duration, ttl: std::time::Duration) {
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
                let _ = reg.sweep(ms_now(), ttl.as_millis() as u64);
            }
        });
    }
}

/// CSV/JSONL 源解析 + 列类型转换（T3）。行号权威：ParsedRow.line = 源文件绝对行号
/// （1-based，header 计 1）。解析器不 panic：引号不闭合/坏格/超长一律行级 Bad，
/// 行缓冲有界（逐记录切片，不累积）。
pub mod parse {
    use serde_json::Value;

    use super::super::registry::ColumnType;

    /// 一行解析成果：`values` 与 columns 对齐；`line` = 源文件绝对行号（1-based）。
    #[derive(Debug, Clone, PartialEq)]
    pub struct ParsedRow {
        pub values: Vec<Value>,
        pub line: u64,
    }

    /// 行级错误（不 panic、不静默跳过——由 consumer 决定 abort/skip 落 samples）。
    #[derive(Debug, Clone, PartialEq)]
    pub enum ParseError {
        Bad {
            line: u64,
            reason: String,
            raw: String,
        },
    }

    /// CSV 文本格 → 列类型值。Text/Unknown/Blob 原样字符串（Blob 的 base64 不猜——
    /// 编码错的字节落库时由方言报错行进 samples）。
    pub fn coerce(value: &str, ty: ColumnType) -> Result<Value, String> {
        match ty {
            ColumnType::Integer | ColumnType::BigInt => value
                .parse::<i64>()
                .map(Value::from)
                .map_err(|_| format!("invalid integer '{value}'")),
            ColumnType::Double => value
                .parse::<f64>()
                .map(Value::from)
                .map_err(|_| format!("invalid double '{value}'")),
            ColumnType::Boolean => match value {
                "true" | "1" => Ok(Value::Bool(true)),
                "false" | "0" => Ok(Value::Bool(false)),
                _ => Err(format!("invalid boolean '{value}'")),
            },
            ColumnType::Text | ColumnType::Unknown | ColumnType::Blob => {
                Ok(Value::String(value.to_string()))
            }
        }
    }

    /// 从 pos 起读一条 CSV 记录（可跨物理行——quoted newline 属一格）。
    /// 返回 (记录切片[不含末尾换行], 消耗的物理行数)。EOF 时仍在引号内 → Err(())。
    pub(crate) fn read_record(src: &[u8], mut pos: usize) -> Result<(&[u8], usize), ()> {
        let start = pos;
        let mut in_quotes = false;
        let mut phys = 1usize;
        while pos < src.len() {
            match src[pos] {
                b'"' => {
                    // "" 转义不切换引号态
                    if in_quotes && pos + 1 < src.len() && src[pos + 1] == b'"' {
                        pos += 2;
                        continue;
                    }
                    in_quotes = !in_quotes;
                    pos += 1;
                }
                b'\n' => {
                    if in_quotes {
                        phys += 1;
                        pos += 1;
                    } else {
                        return Ok((&src[start..pos], phys));
                    }
                }
                _ => pos += 1,
            }
        }
        if in_quotes {
            return Err(());
        }
        Ok((&src[start..pos], phys))
    }

    /// 记录切片 → 格序列。规则：`,` 分隔、`""`→`"`、首字符为 `"` 的格按引号格解析。
    /// 引号格闭合后只许 `,` 或记录尾，否则 malformed。不闭合（调用方已兜底 EOF 情
    /// 形，这里兜底引号格内异常态）→ None。
    fn split_record(rec: &[u8]) -> Option<Vec<String>> {
        let mut cells = Vec::new();
        let mut cur: Vec<u8> = Vec::new();
        let mut i = 0;
        while i < rec.len() {
            if rec[i] == b'"' && cur.is_empty() {
                // 引号格：寻闭合（"" 转义）
                i += 1;
                loop {
                    if i >= rec.len() {
                        return None; // 不闭合（防御——read_record 已挡 EOF 主路径）
                    }
                    if rec[i] == b'"' {
                        if i + 1 < rec.len() && rec[i + 1] == b'"' {
                            cur.push(b'"');
                            i += 2;
                        } else {
                            i += 1;
                            break;
                        }
                    } else {
                        cur.push(rec[i]);
                        i += 1;
                    }
                }
                // 闭合后只许 `,` 或记录尾
                if i < rec.len() && rec[i] != b',' {
                    return None;
                }
            } else if rec[i] == b',' {
                cells.push(String::from_utf8_lossy(&cur).into_owned());
                cur.clear();
                i += 1;
            } else {
                cur.push(rec[i]);
                i += 1;
            }
        }
        cells.push(String::from_utf8_lossy(&cur).into_owned());
        Some(cells)
    }

    /// 记录切片 → 表头格序列（列解析用；不闭合 → None）。
    pub(crate) fn header_cells(rec: &[u8]) -> Option<Vec<String>> {
        split_record(rec.strip_suffix(b"\r").unwrap_or(rec))
    }

    /// 记录切片 → ParsedRow（格数校验 + coerce；坏格 → Bad 行级错误）。
    /// CsvParser 与 blob 流式源共用（行号权威的唯一实现点）。
    pub(crate) fn row_from_record(
        rec: &[u8],
        start_line: u64,
        columns: &[String],
        col_types: &[ColumnType],
    ) -> Result<ParsedRow, ParseError> {
        let rec = rec.strip_suffix(b"\r").unwrap_or(rec);
        let cells = match split_record(rec) {
            Some(c) => c,
            None => {
                return Err(ParseError::Bad {
                    line: start_line,
                    reason: "unterminated quoted field".into(),
                    raw: String::from_utf8_lossy(rec).into_owned(),
                });
            }
        };
        if cells.len() != columns.len() {
            return Err(ParseError::Bad {
                line: start_line,
                reason: format!("expected {} columns, got {}", columns.len(), cells.len()),
                raw: String::from_utf8_lossy(rec).into_owned(),
            });
        }
        let mut values = Vec::with_capacity(cells.len());
        for (i, cell) in cells.iter().enumerate() {
            match coerce(cell, col_types[i]) {
                Ok(v) => values.push(v),
                Err(reason) => {
                    return Err(ParseError::Bad {
                        line: start_line,
                        reason: format!("column '{}': {reason}", columns[i]),
                        raw: cell.clone(),
                    });
                }
            }
        }
        Ok(ParsedRow {
            values,
            line: start_line,
        })
    }

    /// JSONL 单行文本 → ParsedRow（对象键 ⊆ columns，缺键 Null；多键/非对象/坏 JSON
    /// → Bad 行级错误）。JsonlParser 与 blob 流式源共用。
    pub(crate) fn jsonl_row(
        text: &str,
        start_line: u64,
        columns: &[String],
    ) -> Result<ParsedRow, ParseError> {
        let v: Value = serde_json::from_str(text).map_err(|e| ParseError::Bad {
            line: start_line,
            reason: format!("invalid json: {e}"),
            raw: text.to_string(),
        })?;
        let obj = v.as_object().ok_or_else(|| ParseError::Bad {
            line: start_line,
            reason: "line must be a json object".into(),
            raw: text.to_string(),
        })?;
        for k in obj.keys() {
            if !columns.iter().any(|c| c == k) {
                return Err(ParseError::Bad {
                    line: start_line,
                    reason: format!("unexpected key '{k}'"),
                    raw: text.to_string(),
                });
            }
        }
        let values = columns
            .iter()
            .map(|c| obj.get(c).cloned().unwrap_or(Value::Null))
            .collect();
        Ok(ParsedRow {
            values,
            line: start_line,
        })
    }

    /// CSV 解析器（逐记录迭代；行缓冲 = 单记录，不累积）。
    #[derive(Debug)]
    pub struct CsvParser<'a> {
        src: &'a [u8],
        pos: usize,
        /// 下一条记录的绝对起始行号（1-based）。
        line: u64,
        columns: Vec<String>,
        col_types: Vec<ColumnType>,
        done: bool,
    }

    impl<'a> CsvParser<'a> {
        /// header=true：首记录必须与 columns 逐格一致（不猜——表头格与登记列名
        /// 不符即参数错误，点名两格）。
        pub fn new(
            src: &'a [u8],
            columns: &[String],
            header: bool,
            col_types: &[ColumnType],
        ) -> Result<Self, String> {
            let mut p = Self {
                src,
                pos: 0,
                line: 1,
                columns: columns.to_vec(),
                col_types: col_types.to_vec(),
                done: src.is_empty(),
            };
            if header && !p.done {
                let start_line = p.line;
                match read_record(src, p.pos) {
                    Ok((rec, phys)) => {
                        let rec = rec.strip_suffix(b"\r").unwrap_or(rec);
                        let cells = split_record(rec).ok_or_else(|| {
                            format!(
                                "db import: unterminated quoted field in header at line {start_line}"
                            )
                        })?;
                        if cells != p.columns {
                            let got = cells.join(",");
                            let want = p.columns.join(",");
                            return Err(format!(
                                "db import: csv header mismatch at line {start_line}: got [{got}] but registered columns are [{want}]"
                            ));
                        }
                        // 推进 = 切片长 + 1（切片只排除单个终止 \n；嵌入换行已在切片内）
                        p.pos += rec.len() + 1;
                        p.line += phys as u64;
                    }
                    Err(()) => {
                        return Err(format!(
                            "db import: unterminated quoted field in header at line {start_line}"
                        ));
                    }
                }
            }
            Ok(p)
        }
    }

    impl Iterator for CsvParser<'_> {
        type Item = Result<ParsedRow, ParseError>;

        fn next(&mut self) -> Option<Self::Item> {
            if self.done || self.pos >= self.src.len() {
                return None;
            }
            let start_line = self.line;
            let (rec, phys) = match read_record(self.src, self.pos) {
                Ok(v) => v,
                Err(()) => {
                    // 不闭合引号到 EOF：该行 Bad（reason 点名），吞掉剩余输入防死循环
                    self.done = true;
                    let raw = String::from_utf8_lossy(&self.src[self.pos..]).into_owned();
                    return Some(Err(ParseError::Bad {
                        line: start_line,
                        reason: "unterminated quoted field".into(),
                        raw,
                    }));
                }
            };
            // 推进 = 切片长 + 1（切片只排除单个终止 \n；嵌入换行已在切片内）
            self.pos += rec.len() + 1;
            self.line += phys as u64;
            Some(row_from_record(
                rec,
                start_line,
                &self.columns,
                &self.col_types,
            ))
        }
    }

    /// JSONL 解析器（逐物理行；空白行跳过但计入绝对行号）。
    #[derive(Debug)]
    pub struct JsonlParser<'a> {
        src: &'a [u8],
        pos: usize,
        line: u64,
        columns: Vec<String>,
        done: bool,
    }

    impl<'a> JsonlParser<'a> {
        pub fn new(src: &'a [u8], columns: &[String]) -> Self {
            Self {
                src,
                pos: 0,
                line: 1,
                columns: columns.to_vec(),
                done: src.is_empty(),
            }
        }
    }

    impl Iterator for JsonlParser<'_> {
        type Item = Result<ParsedRow, ParseError>;

        fn next(&mut self) -> Option<Self::Item> {
            loop {
                if self.done || self.pos >= self.src.len() {
                    return None;
                }
                let start_line = self.line;
                let nl = self.src[self.pos..]
                    .iter()
                    .position(|&b| b == b'\n')
                    .map(|i| self.pos + i)
                    .unwrap_or(self.src.len());
                let raw = &self.src[self.pos..nl];
                self.pos = if nl < self.src.len() { nl + 1 } else { nl };
                self.line += 1;
                let text = String::from_utf8_lossy(raw);
                let trimmed = text.trim_end_matches('\r').trim();
                if trimmed.is_empty() {
                    continue; // 空白行跳过（绝对行号已推进）
                }
                return Some(jsonl_row(trimmed, start_line, &self.columns));
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn cols(names: &[&str]) -> Vec<String> {
            names.iter().map(|s| s.to_string()).collect()
        }

        #[test]
        fn csv_quoted_newline_and_escape() {
            let src = b"a,\"b\nb\",c\r\n";
            let types = [ColumnType::Text; 3];
            let mut p = CsvParser::new(src, &cols(&["x", "y", "z"]), false, &types).unwrap();
            let row = p.next().unwrap().unwrap();
            assert_eq!(row.line, 1);
            assert_eq!(
                row.values,
                vec![
                    Value::String("a".into()),
                    Value::String("b\nb".into()),
                    Value::String("c".into())
                ]
            );
            assert!(p.next().is_none());
        }

        /// Final-fix 1（review Critical）：引号内换行的记录**后面还有记录**时，
        /// 字节推进必须是 rec.len()+1（切片只排除单个终止 \n，嵌入换行已在切片内）。
        /// 修复前 advance=rec.len()+phys 吃掉下一记录首字节 → 静默损坏。
        #[test]
        fn csv_quoted_newline_record_followed_by_more_rows() {
            let src = b"a,\"b\nb\",c\nx,y,z\n";
            let types = [ColumnType::Text; 3];
            let mut p = CsvParser::new(src, &cols(&["x", "y", "z"]), false, &types).unwrap();
            let row = p.next().unwrap().unwrap();
            assert_eq!(
                row.values,
                vec![
                    Value::String("a".into()),
                    Value::String("b\nb".into()),
                    Value::String("c".into())
                ]
            );
            // 第二记录必须完好（修复前首字节 x 被吃掉 → ",y,z" 格数错 → Bad）
            let row = p.next().unwrap().unwrap();
            assert_eq!(row.line, 3, "quoted 记录占 2 物理行，下一记录从第 3 行起");
            assert_eq!(
                row.values,
                vec![
                    Value::String("x".into()),
                    Value::String("y".into()),
                    Value::String("z".into())
                ]
            );
            assert!(p.next().is_none());
        }

        #[test]
        fn csv_escaped_quote() {
            let src = b"\"say \"\"hi\"\"\",2\n";
            let types = [ColumnType::Text, ColumnType::Integer];
            let mut p = CsvParser::new(src, &cols(&["s", "n"]), false, &types).unwrap();
            let row = p.next().unwrap().unwrap();
            assert_eq!(row.values[0], Value::String("say \"hi\"".into()));
            assert_eq!(row.values[1], Value::from(2));
        }

        #[test]
        fn csv_header_mismatch_fails_fast() {
            let src = b"id,name\n1,a\n";
            let types = [ColumnType::Integer, ColumnType::Text];
            let e = CsvParser::new(src, &cols(&["id", "fullname"]), true, &types).unwrap_err();
            assert!(e.contains("header mismatch"), "{e}");
            assert!(e.contains("name"), "{e} must name header cell");
            assert!(e.contains("fullname"), "{e} must name registered column");
        }

        #[test]
        fn csv_bad_cell_line_number_absolute() {
            // header=1, d1=2, d2=3, d3=4（绝对行号；plan 原文「line=5」是算术笔误）
            let src = b"id,name\n1,a\n2,b\nabc,c\n";
            let types = [ColumnType::Integer, ColumnType::Text];
            let mut p = CsvParser::new(src, &cols(&["id", "name"]), true, &types).unwrap();
            assert!(p.next().unwrap().is_ok());
            assert!(p.next().unwrap().is_ok());
            let e = p.next().unwrap().unwrap_err();
            match e {
                ParseError::Bad { line, reason, raw } => {
                    assert_eq!(line, 4);
                    assert!(reason.contains("integer"), "{reason}");
                    assert_eq!(raw, "abc");
                }
            }
            assert!(p.next().is_none());
        }

        #[test]
        fn csv_unterminated_quote_is_bad_not_panic() {
            let src = b"a,\"never closed\n";
            let types = [ColumnType::Text; 2];
            let mut p = CsvParser::new(src, &cols(&["x", "y"]), false, &types).unwrap();
            let e = p.next().unwrap().unwrap_err();
            match e {
                ParseError::Bad { line, reason, .. } => {
                    assert_eq!(line, 1);
                    assert!(reason.contains("unterminated"), "{reason}");
                }
            }
            assert!(p.next().is_none()); // 吞掉剩余输入，不死循环
        }

        #[test]
        fn csv_wrong_column_count_bad() {
            let src = b"a,b\n";
            let types = [ColumnType::Text; 3];
            let mut p = CsvParser::new(src, &cols(&["x", "y", "z"]), false, &types).unwrap();
            let e = p.next().unwrap().unwrap_err();
            match e {
                ParseError::Bad { reason, .. } => assert!(reason.contains("3"), "{reason}"),
            }
        }

        #[test]
        fn jsonl_missing_key_null_extra_key_err() {
            let src = b"{\"id\":1}\n{\"id\":2,\"name\":\"x\",\"extra\":1}\n";
            let mut p = JsonlParser::new(src, &cols(&["id", "name"]));
            let row = p.next().unwrap().unwrap();
            assert_eq!(row.line, 1);
            assert_eq!(row.values, vec![Value::from(1), Value::Null]);
            let e = p.next().unwrap().unwrap_err();
            match e {
                ParseError::Bad { line, reason, .. } => {
                    assert_eq!(line, 2);
                    assert!(reason.contains("extra"), "{reason}");
                }
            }
            assert!(p.next().is_none());
        }

        #[test]
        fn jsonl_blank_lines_skipped_but_counted() {
            let src = b"\n{\"id\":1}\n\n";
            let mut p = JsonlParser::new(src, &cols(&["id"]));
            let row = p.next().unwrap().unwrap();
            assert_eq!(row.line, 2); // 空白行计入绝对行号
            assert!(p.next().is_none());
        }

        #[test]
        fn coerce_boolean_forms() {
            assert_eq!(
                coerce("true", ColumnType::Boolean).unwrap(),
                Value::Bool(true)
            );
            assert_eq!(coerce("1", ColumnType::Boolean).unwrap(), Value::Bool(true));
            assert_eq!(
                coerce("false", ColumnType::Boolean).unwrap(),
                Value::Bool(false)
            );
            assert_eq!(
                coerce("0", ColumnType::Boolean).unwrap(),
                Value::Bool(false)
            );
            assert!(coerce("yes", ColumnType::Boolean).is_err());
            assert_eq!(coerce("42", ColumnType::Integer).unwrap(), Value::from(42));
            assert!(coerce("4.2", ColumnType::Integer).is_err());
            assert_eq!(coerce("1.5", ColumnType::Double).unwrap(), Value::from(1.5));
            assert_eq!(
                coerce("plain", ColumnType::Text).unwrap(),
                Value::String("plain".into())
            );
            assert_eq!(
                coerce("raw", ColumnType::Blob).unwrap(),
                Value::String("raw".into())
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner(user: &str) -> crate::bridge::handle_core::Owner {
        crate::bridge::handle_core::Owner {
            tenant: None,
            user: Some(user.into()),
            module: None,
            source: "test",
        }
    }

    fn section(global: usize, subject: usize) -> crate::bridge::db_policy::DbImportSection {
        crate::bridge::db_policy::DbImportSection {
            global_concurrency: global,
            per_subject_concurrency: subject,
            ..Default::default()
        }
    }

    #[test]
    fn import_register_quota_global_and_subject() {
        // 全局满：global=2 两人各占一个，第三人撞 GlobalFull
        let reg = ImportRegistry::new(&section(2, 1));
        let (id1, _) = reg.register(owner("u1"), "a".into(), "t".into()).unwrap();
        reg.register(owner("u2"), "a".into(), "t".into()).unwrap();
        let e = reg
            .register(owner("u3"), "a".into(), "t".into())
            .unwrap_err();
        assert_eq!(e, RegisterError::GlobalFull);
        assert!(e.message().contains("db.import: global"));
        // imp_ 前缀隔离
        assert!(id1.starts_with("imp_"));
        // 终态释放名额 → 可再登记
        reg.finish(&id1, Finish::Done { rows: 1, bytes: 2 });
        let (id3, _) = reg.register(owner("u1"), "a".into(), "t".into()).unwrap();
        assert_ne!(id3, id1);
        // 每主体满：global 充裕、per_subject=1，同主体第二次撞 SubjectFull
        let reg = ImportRegistry::new(&section(8, 1));
        reg.register(owner("u1"), "a".into(), "t".into()).unwrap();
        let e = reg
            .register(owner("u1"), "a".into(), "t".into())
            .unwrap_err();
        assert_eq!(e, RegisterError::SubjectFull);
        assert!(e.message().contains("per-subject"));
    }

    #[test]
    fn import_status_cancel_sweep_roundtrip() {
        let reg = ImportRegistry::new(&section(4, 2));
        let (id, _) = reg
            .register(owner("u1"), "main".into(), "users".into())
            .unwrap();
        // status camelCase 逐键
        reg.progress(&id, 10, 2048);
        reg.push_sample(
            &id,
            ImportSample {
                line: 5,
                reason: "bad integer".into(),
                raw: Some("abc".into()),
            },
        );
        let v = reg.status(&id, &owner("u1")).unwrap();
        assert_eq!(v["taskId"], id);
        assert_eq!(v["state"], "running");
        assert_eq!(v["alias"], "main");
        assert_eq!(v["table"], "users");
        assert_eq!(v["rows"], 10);
        assert_eq!(v["bytes"], 2048);
        assert_eq!(v["errors"], 1);
        assert_eq!(v["samples"][0]["line"], 5);
        assert_eq!(v["samples"][0]["reason"], "bad integer");
        assert!(v["startedAtMs"].is_u64());
        assert!(v["finishedAtMs"].is_null());
        // 他人 id → None（不泄露存在性）
        assert!(reg.status(&id, &owner("u2")).is_none());
        // 前缀隔离：export 的 id 在 import 表 not found
        assert!(reg.status("exp_whatever", &owner("u1")).is_none());
        // cancel 幂等：首调 true，重调 false，终态 false，他人 None
        assert_eq!(reg.cancel(&id, &owner("u1")), Some(true));
        assert_eq!(reg.cancel(&id, &owner("u1")), Some(false));
        assert_eq!(reg.cancel(&id, &owner("u2")), None);
        assert_eq!(reg.cancel("imp_nonexistent", &owner("u1")), None);
        // finish Cancelled → 终态 + finishedAtMs
        reg.finish(
            &id,
            Finish::Cancelled {
                rows: 10,
                bytes: 2048,
            },
        );
        let v = reg.status(&id, &owner("u1")).unwrap();
        assert_eq!(v["state"], "cancelled");
        assert!(v["finishedAtMs"].is_u64());
        // 终态幂等：先到者赢（cancel 后再 finish Done 不翻盘）
        reg.finish(
            &id,
            Finish::Done {
                rows: 99,
                bytes: 99,
            },
        );
        assert_eq!(reg.status(&id, &owner("u1")).unwrap()["state"], "cancelled");
        // sweep：终态超时摘表；Running 豁免
        let now = ms_now();
        let removed = reg.sweep(now + 2 * 86_400_000, 86_400_000);
        assert!(removed.contains(&id));
        assert!(reg.status(&id, &owner("u1")).is_none());
        let (id2, _) = reg.register(owner("u1"), "a".into(), "t".into()).unwrap();
        let removed = reg.sweep(now + 2 * 86_400_000, 86_400_000);
        assert!(!removed.contains(&id2));
    }

    #[test]
    fn import_samples_capped_at_20_error_count_unbounded() {
        let reg = ImportRegistry::new(&section(4, 2));
        let (id, _) = reg.register(owner("u1"), "a".into(), "t".into()).unwrap();
        for i in 0..30u64 {
            reg.push_sample(
                &id,
                ImportSample {
                    line: i,
                    reason: "r".into(),
                    raw: None,
                },
            );
        }
        let v = reg.status(&id, &owner("u1")).unwrap();
        assert_eq!(v["samples"].as_array().unwrap().len(), 20);
        assert_eq!(v["errors"], 30); // error_count 独立计数，不受 samples 截断影响
        assert_eq!(v["samples"][19]["line"], 19); // 保留前 20（先到者）
        // 终态后 push_sample no-op
        reg.finish(
            &id,
            Finish::Failed {
                rows: 0,
                bytes: 0,
                error: "x".into(),
            },
        );
        reg.push_sample(
            &id,
            ImportSample {
                line: 99,
                reason: "late".into(),
                raw: None,
            },
        );
        let v = reg.status(&id, &owner("u1")).unwrap();
        assert_eq!(v["errors"], 30);
    }
}

/// 方言 SQL 渲染（T4）：多值 INSERT 经 sea-query（build_sql 渲染 + value_to_json 提参），
/// ON CONFLICT / ON DUPLICATE KEY / OR REPLACE 为字符串尾缀——**只拼 quote_ident 过的
/// 标识符与常量文本，值永不碰 SQL 文本**（注入红线：调用方须先过 has_column 白名单）。
pub mod render {
    use serde_json::Value;

    use super::super::db::Dialect;
    use super::super::db_ddl::quote_ident;

    /// 一批渲染产物：可执行 SQL + 序对齐的绑定参数。
    #[derive(Debug, Clone, PartialEq)]
    pub struct BatchSql {
        pub sql: String,
        pub params: Vec<Value>,
    }

    /// 导入模式：Append 纯插 / Upsert 按冲突键幂等 / Replace 整表替换
    /// （delete + append 两段，T6 单事务内执行 render_delete + 各批）。
    #[derive(Debug, Clone, PartialEq)]
    pub enum ImportMode {
        Append,
        Upsert { conflict_keys: Vec<String> },
        Replace,
    }

    /// 绑定参数上限收敛（PG/MySQL 同 65535）：按 `65535/列数` 收敛批行数，
    /// 不报错、参数序正确（Review Focus 4）。`cap` = 剩余行数（末批不空跑）。
    /// 批行数三向收敛：cap（剩余行数）× batch_size × 方言变量上限/列数。
    /// sqlite 变量上限 32766（SQLITE_MAX_VARIABLE_NUMBER ≥3.32 默认），低于
    /// PG/MySQL 的 65535——宽表默认批在其上必然炸（review Important 实锤）。
    pub fn batch_rows(dialect: Dialect, cap: usize, batch_size: usize, ncols: usize) -> usize {
        let max_vars = if dialect == Dialect::Sqlite {
            32766
        } else {
            65535
        };
        let per_stmt = (max_vars / ncols.max(1)).max(1);
        cap.min(batch_size).min(per_stmt)
    }

    /// Replace 模式的删旧段（T6 在单事务内先执行本句再逐批 insert）。
    /// 标识符经 quote_ident；DELETE 无条件——整表语义即 spec §3 replace 定义。
    pub fn render_delete(dialect: Dialect, table: &str) -> Result<String, String> {
        Ok(format!("DELETE FROM {}", quote_ident(dialect, table)?))
    }

    /// 渲染一批多值 INSERT。标识符安全由调用方的 `has_column` 白名单保证，本层仍
    /// 经 `quote_ident` 引号包裹（纵深防御）；值为纯绑定参数，永不进 SQL 文本。
    pub fn render_batch(
        dialect: Dialect,
        table: &str,
        columns: &[String],
        rows: &[Vec<Value>],
        mode: &ImportMode,
        table_pk: &[String],
    ) -> Result<BatchSql, String> {
        if rows.is_empty() {
            return Err("db import: render_batch with no rows".into());
        }
        let ncols = columns.len();
        for r in rows {
            if r.len() != ncols {
                return Err(format!(
                    "db import: row has {} values but {} columns declared",
                    r.len(),
                    ncols
                ));
            }
        }
        match mode {
            ImportMode::Append => sea_insert(dialect, table, columns, rows),
            ImportMode::Replace => {
                Err("db import: replace mode renders via render_delete + append batches".into())
            }
            ImportMode::Upsert { conflict_keys } => {
                if conflict_keys.is_empty() {
                    return Err("db import: upsert requires at least one conflict key".into());
                }
                for k in conflict_keys {
                    if !columns.iter().any(|c| c == k) {
                        return Err(format!(
                            "db import: conflict key '{k}' not in import columns"
                        ));
                    }
                }
                match dialect {
                    Dialect::Postgres => {
                        let mut b = sea_insert(dialect, table, columns, rows)?;
                        b.sql.push_str(&pg_on_conflict(conflict_keys, columns)?);
                        Ok(b)
                    }
                    Dialect::MySql => {
                        if table_pk.is_empty() {
                            return Err(format!(
                                "db import: upsert on mysql requires a unique key (primary key) on table '{table}'"
                            ));
                        }
                        let mut b = sea_insert(dialect, table, columns, rows)?;
                        if let Some(suffix) = mysql_on_dup(conflict_keys, columns)? {
                            b.sql.push_str(&suffix);
                        }
                        Ok(b)
                    }
                    Dialect::Sqlite => {
                        if conflict_keys.len() != 1 || conflict_keys[0] != "rowid" {
                            return Err(
                                "db import: sqlite upsert only supports conflictKeys [\"rowid\"]"
                                    .into(),
                            );
                        }
                        sqlite_or_replace(table, columns, rows)
                    }
                }
            }
        }
    }

    /// sea-query 多值 INSERT 主干（append / PG·MySQL upsert 共用），
    /// 渲染 + 参数提取全复用 query.rs 既有对（build_sql / value_to_json / to_qv）。
    fn sea_insert(
        dialect: Dialect,
        table: &str,
        columns: &[String],
        rows: &[Vec<Value>],
    ) -> Result<BatchSql, String> {
        use sea_query::{Alias, Expr, Query};
        let mut ins = Query::insert();
        ins.into_table(Alias::new(table));
        ins.columns(columns.iter().map(Alias::new));
        for row in rows {
            let vals: Vec<Expr> = row
                .iter()
                .map(|v| Expr::val(super::super::query::to_qv(v)))
                .collect();
            ins.values(vals).map_err(|e| format!("db import: {e}"))?;
        }
        let (sql, vals) = super::super::query::build_sql(dialect, &ins);
        let mut params = Vec::with_capacity(vals.iter().count());
        for v in vals.iter() {
            params.push(
                super::super::query::value_to_json(v).map_err(|e| format!("db import: {e}"))?,
            );
        }
        Ok(BatchSql { sql, params })
    }

    /// PG upsert 尾缀：`ON CONFLICT ("k",…) DO UPDATE SET "c" = "excluded"."c", …`
    /// （SET 只含非 key 列；全是 key → DO NOTHING）。纯标识符 + 常量文本。
    /// PG upsert 尾缀（修正案①：直挂多值 INSERT，不建临时表）。
    /// 标识符经 quote_ident——Err 落 Err（未登记表的列/键是用户串，须防 panic）。
    fn pg_on_conflict(keys: &[String], columns: &[String]) -> Result<String, String> {
        let ks = keys
            .iter()
            .map(|k| quote_ident(Dialect::Postgres, k))
            .collect::<Result<Vec<_>, _>>()?
            .join(", ");
        let sets: Vec<String> = columns
            .iter()
            .filter(|c| !keys.contains(c))
            .map(|c| -> Result<String, String> {
                let q = quote_ident(Dialect::Postgres, c)?;
                Ok(format!("{q} = \"excluded\".{q}"))
            })
            .collect::<Result<_, _>>()?;
        if sets.is_empty() {
            Ok(format!(" ON CONFLICT ({ks}) DO NOTHING"))
        } else {
            Ok(format!(
                " ON CONFLICT ({ks}) DO UPDATE SET {}",
                sets.join(", ")
            ))
        }
    }

    /// MySQL upsert 尾缀：`ON DUPLICATE KEY UPDATE `c` = VALUES(`c`), …`。
    /// 非 key 列为空（全列皆 key）→ Ok(None)（无后缀：重复键时由方言报错 surfaced）。
    fn mysql_on_dup(keys: &[String], columns: &[String]) -> Result<Option<String>, String> {
        let sets: Vec<String> = columns
            .iter()
            .filter(|c| !keys.contains(c))
            .map(|c| -> Result<String, String> {
                let q = quote_ident(Dialect::MySql, c)?;
                Ok(format!("{q} = VALUES({q})"))
            })
            .collect::<Result<_, _>>()?;
        if sets.is_empty() {
            Ok(None)
        } else {
            Ok(Some(format!(
                " ON DUPLICATE KEY UPDATE {}",
                sets.join(", ")
            )))
        }
    }

    /// sqlite upsert 特例（spec §4.3）：仅 rowid 键——sea-query 表达不了
    /// `INSERT OR REPLACE`，整句手渲染；`?` 占位 + quote_ident，参数为行展平。
    fn sqlite_or_replace(
        table: &str,
        columns: &[String],
        rows: &[Vec<Value>],
    ) -> Result<BatchSql, String> {
        let t = quote_ident(Dialect::Sqlite, table)?;
        let cols = columns
            .iter()
            .map(|c| quote_ident(Dialect::Sqlite, c))
            .collect::<Result<Vec<_>, _>>()?
            .join(", ");
        let one = format!("({})", vec!["?"; columns.len()].join(","));
        let body = vec![one; rows.len()].join(",");
        let params: Vec<Value> = rows.iter().flatten().cloned().collect();
        Ok(BatchSql {
            sql: format!("INSERT OR REPLACE INTO {t} ({cols}) VALUES {body}"),
            params,
        })
    }
}

#[cfg(test)]
mod render_tests {
    use super::super::db::Dialect;
    use super::render::*;
    use serde_json::json;

    fn cols(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn two_rows() -> Vec<Vec<serde_json::Value>> {
        vec![vec![json!(1), json!("a")], vec![json!(2), json!("b")]]
    }

    #[test]
    fn render_pg_append_multivalue() {
        let b = render_batch(
            Dialect::Postgres,
            "t",
            &cols(&["a", "b"]),
            &two_rows(),
            &ImportMode::Append,
            &[],
        )
        .unwrap();
        let flat: String = b.sql.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            flat.contains("INSERTINTO\"t\"(\"a\",\"b\")VALUES($1,$2),($3,$4)"),
            "{sql}",
            sql = b.sql
        );
        assert_eq!(b.params, vec![json!(1), json!("a"), json!(2), json!("b")]);
    }

    #[test]
    fn render_pg_upsert_on_conflict_clause() {
        let rows3: Vec<Vec<serde_json::Value>> = vec![
            vec![json!(1), json!("a"), json!(10)],
            vec![json!(2), json!("b"), json!(20)],
        ];
        let b = render_batch(
            Dialect::Postgres,
            "t",
            &cols(&["a", "b", "c"]),
            &rows3,
            &ImportMode::Upsert {
                conflict_keys: cols(&["a"]),
            },
            &[],
        )
        .unwrap();
        assert!(
            b.sql.contains("ON CONFLICT (\"a\") DO UPDATE SET"),
            "{}",
            b.sql
        );
        // SET 只含非 key 列，excluded 引用正确
        assert!(b.sql.contains("\"b\" = \"excluded\".\"b\""), "{}", b.sql);
        assert!(b.sql.contains("\"c\" = \"excluded\".\"c\""), "{}", b.sql);
        assert!(!b.sql.contains("\"a\" = \"excluded\""), "{}", b.sql);
        // 参数与 append 同序（suffix 不碰绑定）
        assert_eq!(b.params.len(), 6);
    }

    #[test]
    fn render_pg_upsert_all_keys_do_nothing() {
        let b = render_batch(
            Dialect::Postgres,
            "t",
            &cols(&["a"]),
            &[vec![json!(1)]],
            &ImportMode::Upsert {
                conflict_keys: cols(&["a"]),
            },
            &[],
        )
        .unwrap();
        assert!(
            b.sql.contains("ON CONFLICT (\"a\") DO NOTHING"),
            "{}",
            b.sql
        );
    }

    /// Final-fix 4（review Important）：未登记表的列/键是任意用户串——脏标识符
    /// （空/控制字符）在 quote_ident 处必须落 Err，绝不 panic（panic 在 spawned
    /// consumer 里 = 任务永卡 running + 配额槽泄漏）。
    #[test]
    fn render_upsert_dirty_identifier_is_error_not_panic() {
        let e = render_batch(
            Dialect::Postgres,
            "t",
            &cols(&["a", "bad\u{1}key"]),
            &two_rows(),
            &ImportMode::Upsert {
                conflict_keys: cols(&["a"]),
            },
            &[],
        )
        .unwrap_err();
        assert!(e.contains("identifier"), "{e}");
        // MySQL 同路径
        let e = render_batch(
            Dialect::MySql,
            "t",
            &cols(&["a", "bad\u{1}key"]),
            &two_rows(),
            &ImportMode::Upsert {
                conflict_keys: cols(&["a"]),
            },
            &cols(&["a"]),
        )
        .unwrap_err();
        assert!(e.contains("identifier"), "{e}");
    }

    #[test]
    fn render_mysql_upsert_requires_pk() {
        let e = render_batch(
            Dialect::MySql,
            "t",
            &cols(&["a", "b"]),
            &two_rows(),
            &ImportMode::Upsert {
                conflict_keys: cols(&["a"]),
            },
            &[],
        )
        .unwrap_err();
        assert!(e.contains("requires a unique key"), "{e}");
        assert!(e.contains("'t'"), "{e}");
        // 有 pk → ON DUPLICATE KEY UPDATE 只含非 key 列
        let b = render_batch(
            Dialect::MySql,
            "t",
            &cols(&["a", "b"]),
            &two_rows(),
            &ImportMode::Upsert {
                conflict_keys: cols(&["a"]),
            },
            &cols(&["a"]),
        )
        .unwrap();
        assert!(b.sql.contains("ON DUPLICATE KEY UPDATE"), "{}", b.sql);
        assert!(b.sql.contains("`b` = VALUES(`b`)"), "{}", b.sql);
        assert!(!b.sql.contains("`a` = VALUES"), "{}", b.sql);
    }

    #[test]
    fn render_sqlite_or_replace_only_rowid() {
        // 仅 conflict_keys == ["rowid"] 合法（spec §4.3）；rowid 须入列清单
        let rows3: Vec<Vec<serde_json::Value>> = vec![
            vec![json!(1), json!(1), json!("a")],
            vec![json!(2), json!(2), json!("b")],
        ];
        let b = render_batch(
            Dialect::Sqlite,
            "t",
            &cols(&["rowid", "a", "b"]),
            &rows3,
            &ImportMode::Upsert {
                conflict_keys: cols(&["rowid"]),
            },
            &[],
        )
        .unwrap();
        let flat: String = b.sql.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            flat.contains("INSERTORREPLACEINTO\"t\"(\"rowid\",\"a\",\"b\")VALUES(?,?,?)"),
            "{}",
            b.sql
        );
        assert_eq!(
            b.params,
            vec![
                json!(1),
                json!(1),
                json!("a"),
                json!(2),
                json!(2),
                json!("b")
            ]
        );
        // 非 rowid 键 → 参数错误
        let e = render_batch(
            Dialect::Sqlite,
            "t",
            &cols(&["rowid", "a", "b"]),
            &rows3,
            &ImportMode::Upsert {
                conflict_keys: cols(&["a"]),
            },
            &[],
        )
        .unwrap_err();
        assert!(e.contains("rowid"), "{e}");
    }

    #[test]
    fn render_conflict_key_must_be_in_columns() {
        let e = render_batch(
            Dialect::Postgres,
            "t",
            &cols(&["a", "b"]),
            &two_rows(),
            &ImportMode::Upsert {
                conflict_keys: cols(&["nope"]),
            },
            &[],
        )
        .unwrap_err();
        assert!(e.contains("nope"), "{e}");
    }

    #[test]
    fn batch_rows_caps_at_65535() {
        use super::super::db::Dialect;
        // ncols=10 → 每语句 ≤6553
        assert_eq!(batch_rows(Dialect::Postgres, usize::MAX, 5000, 10), 5000);
        assert_eq!(batch_rows(Dialect::Postgres, usize::MAX, 100_000, 10), 6553);
        // ncols=100 → ≤655
        assert_eq!(batch_rows(Dialect::Postgres, usize::MAX, 100_000, 100), 655);
        // 用户 batchSize 1_000_000 → 收敛不报错
        assert_eq!(
            batch_rows(Dialect::Postgres, usize::MAX, 1_000_000, 10),
            6553
        );
        // cap（剩余行数）优先
        assert_eq!(batch_rows(Dialect::Postgres, 5, 5000, 10), 5);
        assert!(batch_rows(Dialect::Postgres, 0, 5000, 10) == 0);
        // sqlite 变量上限 32766：7 列 → 4680；默认批 5000 必须被压下来
        assert_eq!(batch_rows(Dialect::Sqlite, usize::MAX, 5000, 7), 4680);
        assert_eq!(batch_rows(Dialect::Sqlite, usize::MAX, 100_000, 10), 3276);
        // mysql 同 65535
        assert_eq!(batch_rows(Dialect::MySql, usize::MAX, 100_000, 10), 6553);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sqlite_real_multivalue_insert_and_or_replace() {
        use super::super::accessor_sqlx::SqlxAccessor;
        use super::super::db::DataAccessor;
        let db = SqlxAccessor::connect("sqlite::memory:").await.unwrap();
        db.exec_with_params("create table t (a integer, b text)", &[])
            .await
            .unwrap();
        // 多值 append
        let batch = render_batch(
            Dialect::Sqlite,
            "t",
            &cols(&["a", "b"]),
            &two_rows(),
            &ImportMode::Append,
            &[],
        )
        .unwrap();
        let n = db
            .exec_with_params(&batch.sql, &batch.params)
            .await
            .unwrap();
        assert_eq!(n, 2);
        let rows = db
            .query_with_params("select a, b from t order by a", &[])
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        // rowid OR REPLACE 幂等（同 rowid 覆盖）
        let batch = render_batch(
            Dialect::Sqlite,
            "t",
            &cols(&["rowid", "a", "b"]),
            &[vec![json!(1), json!(9), json!("z")]],
            &ImportMode::Upsert {
                conflict_keys: cols(&["rowid"]),
            },
            &[],
        )
        .unwrap();
        db.exec_with_params(&batch.sql, &batch.params)
            .await
            .unwrap();
        let rows = db
            .query_with_params("select a, b from t order by a", &[])
            .await
            .unwrap();
        assert_eq!(rows.len(), 2, "or replace 不增行");
        assert_eq!(rows[1]["b"], json!("z"), "row 1 被覆盖");
    }
}

#[cfg(test)]
mod op_tests {
    use crate::bridge::{Bridge, Extras, InMemoryAccessor, InMemoryKV, SchemaRegistry, SqlGuard};
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use std::sync::Arc;

    fn import_bridge(
        registry: SchemaRegistry,
        import_section: super::super::db_policy::DbImportSection,
        sql_guard: SqlGuard,
    ) -> Bridge {
        let mem = Arc::new(InMemoryAccessor::new());
        Bridge::with_dbs_and_loader(
            HashMap::from([("default".to_string(), mem as _)]),
            Arc::new(InMemoryKV::new()),
            registry,
            false,
            None,
            Extras {
                import_section: Some(Arc::new(import_section)),
                db_access: Some(Arc::new(Default::default())),
                sql_guard,
                ..Default::default()
            },
        )
    }

    fn scoped_registry() -> SchemaRegistry {
        SchemaRegistry::new().table("orders", &["id"], &["id", "tenant_id", "name"])
    }

    /// 结构化错误收割：code + capability + data.format。
    async fn import_err(b: &Bridge, args: &str) -> Value {
        let js = format!(
            r#"(async () => {{
                 const r = await db.import({args}).then(
                   () => null,
                   (e) => ({{ code: e.code, capability: e.data && e.data.capability, format: e.data && e.data.format, msg: String(e.message || e) }}));
                 json.ok(r);
               }})().catch(e => json.fail(500, String(e)));"#,
        );
        let cap = b.run_with(&js, Default::default()).await.unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        v["data"].clone()
    }

    /// scoped 表 + guard=Warn/Deny 两档 → 结构化拒 capability=import_under_guard。
    #[tokio::test(flavor = "current_thread")]
    async fn import_guard_scoped_table_fail_closed() {
        for guard in [SqlGuard::Warn, SqlGuard::Deny] {
            let b = import_bridge(scoped_registry(), Default::default(), guard);
            let d = import_err(
                &b,
                r#"{ table: "orders", format: "jsonl", columns: ["id", "name"], rows: [{ id: 1, name: "a" }] }"#,
            )
            .await;
            assert_eq!(d["code"], json!("OJ_DB_UNSUPPORTED"), "{d}");
            assert_eq!(d["capability"], json!("import_under_guard"), "{d}");
        }
    }

    /// 未登记 + Deny → 拒（镜像裸 SQL「cannot verify tenant scope」）；
    /// 未登记 + Warn → warn 放行（任务建起）。
    #[tokio::test(flavor = "current_thread")]
    async fn import_guard_unregistered_warn_passes_deny_rejects() {
        let deny = import_bridge(SchemaRegistry::new(), Default::default(), SqlGuard::Deny);
        let d = import_err(
            &deny,
            r#"{ table: "ghost", format: "jsonl", columns: ["id"], rows: [{ id: 1 }] }"#,
        )
        .await;
        assert_eq!(d["code"], json!(null), "{d}"); // 非结构化——守卫 generic 错误
        assert!(
            d["msg"]
                .as_str()
                .unwrap()
                .contains("cannot verify tenant scope"),
            "{d}"
        );
        let warn = import_bridge(SchemaRegistry::new(), Default::default(), SqlGuard::Warn);
        let js = r#"(async () => {
             const { taskId } = await db.import({ table: "ghost", format: "jsonl", columns: ["id"], rows: [{ id: 1 }] });
             json.ok({ taskId });
           })().catch(e => json.fail(500, String(e)));"#;
        let cap = warn.run_with(js, Default::default()).await.unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert!(
            v["data"]["taskId"].as_str().unwrap().starts_with("imp_"),
            "{v}"
        );
    }

    /// 已登记但非 scoped + Deny → 放行（同裸 SQL）。
    #[tokio::test(flavor = "current_thread")]
    async fn import_guard_registered_non_scoped_passes() {
        let reg = SchemaRegistry::new().table("plain", &["id"], &["id", "name"]);
        let b = import_bridge(reg, Default::default(), SqlGuard::Deny);
        let js = r#"(async () => {
             const { taskId } = await db.import({ table: "plain", format: "jsonl", columns: ["id", "name"], rows: [{ id: 1, name: "a" }] });
             json.ok({ taskId });
           })().catch(e => json.fail(500, String(e)));"#;
        let cap = b.run_with(js, Default::default()).await.unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert!(
            v["data"]["taskId"].as_str().unwrap().starts_with("imp_"),
            "{v}"
        );
    }

    /// rs.system=true + scoped 表 + Deny → 预检与 check_tenant_raw 一致跳过 → 放行。
    #[tokio::test(flavor = "current_thread")]
    async fn import_as_system_skips_guard() {
        let b = import_bridge(scoped_registry(), Default::default(), SqlGuard::Deny);
        let js = r#"(async () => {
             const { taskId } = await db.asSystem().import({ table: "orders", format: "jsonl", columns: ["id", "name"], rows: [{ id: 1, name: "a" }] });
             json.ok({ taskId });
           })().catch(e => json.fail(500, String(e)));"#;
        let cap = b.run_with(js, Default::default()).await.unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert!(
            v["data"]["taskId"].as_str().unwrap().starts_with("imp_"),
            "{v}"
        );
    }

    /// 内联 rows 超 max_rows → 登记前拒（点名键）；拒后配额位不泄漏
    /// （per_subject=1 下被拒后同主体立刻可再登记 = 无残留 entry）。
    #[tokio::test(flavor = "current_thread")]
    async fn import_rows_over_quota_rejected_before_register() {
        let section = super::super::db_policy::DbImportSection {
            max_rows: 2,
            per_subject_concurrency: 1,
            ..Default::default()
        };
        let b = import_bridge(SchemaRegistry::new(), section, SqlGuard::Off);
        let d = import_err(
            &b,
            r#"{ table: "t", format: "jsonl", columns: ["id"], rows: [{ id: 1 }, { id: 2 }, { id: 3 }] }"#,
        )
        .await;
        assert!(d["msg"].as_str().unwrap().contains("max_rows"), "{d}");
        // 登记无残留：同主体（per_subject=1）立刻可登记成功
        let js = r#"(async () => {
             const { taskId } = await db.import({ table: "t", format: "jsonl", columns: ["id"], rows: [{ id: 1 }] });
             json.ok({ taskId });
           })().catch(e => json.fail(500, String(e)));"#;
        let cap = b.run_with(js, Default::default()).await.unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
    }

    /// 前缀隔离：importStatus(exp_id) / exportStatus(imp_id) 互 not found。
    #[tokio::test(flavor = "current_thread")]
    async fn import_prefix_isolation() {
        let b = import_bridge(SchemaRegistry::new(), Default::default(), SqlGuard::Off);
        let js = r#"(async () => {
             const expId = "exp_ab12_00000000000000000000000000000000";
             const impId = "imp_ab12_00000000000000000000000000000000";
             const r1 = await db.importStatus(expId).then(() => "found", (e) => String(e.message || e));
             const r2 = await db.exportStatus(impId).then(() => "found", (e) => String(e.message || e));
             json.ok({ r1, r2 });
           })().catch(e => json.fail(500, String(e)));"#;
        let cap = b.run_with(js, Default::default()).await.unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert!(
            v["data"]["r1"].as_str().unwrap().contains("not found"),
            "{v}"
        );
        assert!(
            v["data"]["r2"].as_str().unwrap().contains("not found"),
            "{v}"
        );
    }

    /// blobKey 与 rows 恰一；format 只注册 csv/jsonl。
    #[tokio::test(flavor = "current_thread")]
    async fn import_blob_and_rows_mutex_and_bad_format() {
        let b = import_bridge(SchemaRegistry::new(), Default::default(), SqlGuard::Off);
        let d = import_err(
            &b,
            r#"{ table: "t", format: "csv", blobKey: "k", rows: [{ id: 1 }] }"#,
        )
        .await;
        assert!(d["msg"].as_str().unwrap().contains("blobKey"), "{d}");
        let d = import_err(&b, r#"{ table: "t", format: "csv" }"#).await;
        assert!(d["msg"].as_str().unwrap().contains("rows"), "{d}");
        let d = import_err(&b, r#"{ table: "t", format: "xlsx", rows: [{ id: 1 }] }"#).await;
        assert_eq!(d["code"], json!("OJ_DB_UNSUPPORTED"), "{d}");
        assert_eq!(d["format"], json!("xlsx"), "{d}");
    }
}

// ---------------------------------------------------------------------------
// ops（REQ-12 T5）：db.import / importStatus / importCancel——守卫矩阵 + 配额 + 登记
// ---------------------------------------------------------------------------

use deno_core::{OpState, op2};
use deno_error::JsErrorBox;
use std::cell::RefCell;
use std::rc::Rc;

/// `db.import(args)` 入参（spec §3 逐字段 camelCase）。
#[derive(Debug, serde::Deserialize)]
pub struct ImportArgs {
    #[serde(rename = "table")]
    pub table: String,
    #[serde(rename = "schema", default)]
    pub schema: Option<String>,
    #[serde(rename = "blobKey", default)]
    pub blob_key: Option<String>,
    #[serde(rename = "rows", default)]
    pub rows: Option<Vec<serde_json::Map<String, Value>>>,
    #[serde(rename = "format")]
    pub format: String,
    #[serde(rename = "columns", default)]
    pub columns: Option<Vec<String>>,
    #[serde(rename = "mode", default)]
    pub mode: Option<String>,
    #[serde(rename = "conflictKeys", default)]
    pub conflict_keys: Option<Vec<String>>,
    #[serde(rename = "batchSize", default)]
    pub batch_size: Option<usize>,
    #[serde(rename = "header", default)]
    pub header: Option<bool>,
    #[serde(rename = "onError", default)]
    pub on_error: Option<String>,
    #[serde(rename = "maxErrors", default)]
    pub max_errors: Option<u64>,
}

/// 解析后的导入模式（render::ImportMode 的 op 侧形态，含冲突键）。
#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    Append,
    Upsert { conflict_keys: Vec<String> },
    Replace,
}

impl Mode {
    fn as_str(&self) -> &'static str {
        match self {
            Mode::Append => "append",
            Mode::Upsert { .. } => "upsert",
            Mode::Replace => "replace",
        }
    }
}

/// 结构化 unsupported（未知 format）：`OJ_DB_UNSUPPORTED`，data.capability 点名 format。
fn format_unsupported(format: &str) -> JsErrorBox {
    let mut e = super::db_error::unsupported("import", "format");
    e.message = format!("import format \"{format}\" is not supported (use \"csv\" or \"jsonl\")");
    e.data.insert("format".into(), Value::String(format.into()));
    JsErrorBox::from(e)
}

/// 结构化 import 预检拒绝：`OJ_DB_UNSUPPORTED(capability="import_under_guard")`。
/// scoped 表在 sql_guard 下一律 fail-closed——守卫只能验「某参数=当前 tid」，
/// 证明不了导入每行都属该租户（per-row 强制 = import 侧未来工作，spec §4.2）。
fn import_under_guard(table: &str) -> JsErrorBox {
    let mut e = super::db_error::unsupported("import", "import_under_guard");
    e.message = format!(
        "db.import on tenant-scoped table '{table}' is not supported under sql_guard (capability \"import_under_guard\"): per-row tenant enforcement is import-side work, tracked as future work"
    );
    JsErrorBox::from(e)
}

/// `db.import(args)`：守卫矩阵 → 配额 → 登记 → 审计 → 自驱 consumer，立即返 `{taskId}`。
/// T5 落同步面 + 占位 consumer（立即 Done{0,0}）；T6 替换真实现。
#[op2]
#[serde]
pub async fn op_db_import(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[serde] mut args: ImportArgs,
) -> Result<serde_json::Value, JsErrorBox> {
    // 0. 参数面：format / mode / onError / blobKey×rows 恰一。
    match args.format.as_str() {
        "csv" | "jsonl" => {}
        other => return Err(format_unsupported(other)),
    }
    if let Some(s) = &args.schema {
        return Err(JsErrorBox::generic(format!(
            "db import: schema '{s}' is not supported yet (use table name only)"
        )));
    }
    let mode = match args.mode.as_deref().unwrap_or("append") {
        "append" => Mode::Append,
        "replace" => Mode::Replace,
        "upsert" => Mode::Upsert {
            conflict_keys: args.conflict_keys.clone().unwrap_or_default(),
        },
        other => {
            return Err(JsErrorBox::generic(format!(
                "db import: mode \"{other}\" is not supported (use \"append\", \"upsert\" or \"replace\")"
            )));
        }
    };
    let on_error_skip = match args.on_error.as_deref().unwrap_or("abort") {
        "abort" => false,
        "skip" => true,
        other => {
            return Err(JsErrorBox::generic(format!(
                "db import: onError \"{other}\" is not supported (use \"abort\" or \"skip\")"
            )));
        }
    };
    match (&args.blob_key, &args.rows) {
        (Some(_), Some(_)) => {
            return Err(JsErrorBox::generic(
                "db import: pass exactly one of blobKey or rows",
            ));
        }
        (None, None) => {
            return Err(JsErrorBox::generic(
                "db import: either blobKey or rows is required",
            ));
        }
        _ => {}
    }
    // 1. 表归属守卫（同 db.table() 构造器路径）。
    super::guard::check_table(&state, &args.table)?;
    // 2. import 预检：guard != Off && !system && 已登记且 tenant-scoped → fail-closed。
    {
        let g = state.borrow();
        let st = g.borrow::<Arc<super::StableState>>();
        let rs = g.borrow::<super::ReqState>();
        if st.sql_guard != super::SqlGuard::Off
            && !rs.system
            && st
                .registry
                .get(&args.table)
                .is_some_and(|d| d.is_tenant_scoped())
        {
            return Err(import_under_guard(&args.table));
        }
    }
    // 3. 合成 SQL 镜像裸 SQL 租户裁决（逐档复用 check_tenant_raw，不重写矩阵）。
    let synthetic = match &mode {
        Mode::Replace => format!("truncate table {}", args.table),
        _ => format!("insert into {} values (?)", args.table),
    };
    super::guard::check_tenant_raw(&state, &synthetic, &[])?;
    // 4. 访问控制：replace = truncate 分类（allow_ddl + G9 危险写确认门同档），
    //    append/upsert = insert 分类——全在 check_access 内部按首关键字裁决。
    let alias = super::guard::bound_db(&state, &name);
    super::db_policy::check_access(&state, &alias, &synthetic)?;
    // 5. 池臂（与 export 同口径：事务里跑导入无意义——连接随请求死）。
    let da = match super::db::resolve_target(&state, &name)? {
        super::db::Target::Pool(da) => da,
        super::db::Target::Tx(_) => {
            return Err(JsErrorBox::generic(
                "db.import within an active transaction is not supported (import runs on the connection pool only)",
            ));
        }
    };
    let (section, registry, import_registry) = {
        let g = state.borrow();
        let st = g.borrow::<Arc<super::StableState>>();
        (
            st.import_section.clone(),
            st.registry.clone(),
            st.import_registry.clone(),
        )
    };
    // 6. 列解析 + 白名单 + 内联行配额（登记前）。
    //    blob 源的列在 consumer 首步从 csv 表头 / jsonl 首行键解析（读数前白名单校验）。
    let columns: Vec<String> = match (&args.columns, &args.rows) {
        (Some(c), _) => c.clone(),
        (None, Some(rows)) if !rows.is_empty() => {
            rows[0].keys().cloned().collect() // BTreeMap 键序 = 排序序（serde_json 无 preserve_order）
        }
        (None, Some(_)) => {
            return Err(JsErrorBox::generic(
                "db import: columns are required for an empty rows array",
            ));
        }
        (None, None) => Vec::new(),
    };
    if let Some(t) = registry.get(&args.table) {
        // sqlite 字面量 "rowid" 是内建准列（非用户字符串），upsert 键为 rowid 时放行
        let sqlite_rowid = da.dialect() == super::db::Dialect::Sqlite;
        for c in &columns {
            if !t.has_column(c) && !(sqlite_rowid && c == "rowid") {
                return Err(JsErrorBox::generic(format!(
                    "unknown column '{c}' in import columns"
                )));
            }
        }
    }
    if let Some(rows) = &args.rows {
        if rows.len() as u64 > section.max_rows {
            return Err(JsErrorBox::generic(format!(
                "db import: rows exceed db_import.max_rows ({})",
                section.max_rows
            )));
        }
        for row in rows {
            for k in row.keys() {
                if !columns.iter().any(|c| c == k) {
                    return Err(JsErrorBox::generic(format!(
                        "db import: row key '{k}' not in columns"
                    )));
                }
            }
        }
    }
    let batch_size = args
        .batch_size
        .unwrap_or(section.default_batch_size)
        .clamp(100, 50000);
    let max_errors = args.max_errors.unwrap_or(10).max(1);
    // 6b. 源装配（登记前——blob 列解析/源读错误同步 fail-fast，不起任务）。
    //     blob 源的列在此从 csv 表头 / jsonl 首行键解析（读数前白名单校验）。
    let header = args.header.unwrap_or(args.format == "csv");
    let inline_bytes = args
        .rows
        .as_ref()
        .map(|rows| {
            rows.iter()
                .map(|r| serde_json::to_vec(r).map(|v| v.len() as u64).unwrap_or(0))
                .sum()
        })
        .unwrap_or(0);
    let source: RowSource = match (&args.blob_key, args.rows.take()) {
        (Some(key), None) => {
            let blob_reg = {
                let g = state.borrow();
                g.borrow::<Arc<super::StableState>>().blobs.clone()
            };
            let blob = blob_reg.default().ok_or_else(|| {
                JsErrorBox::generic("blob not configured (db.import blobKey source)")
            })?;
            match args.format.as_str() {
                "csv" => RowSource::Csv(Box::new(
                    CsvBlobSource::open(
                        blob,
                        key,
                        header,
                        &columns,
                        &registry,
                        &args.table,
                        da.dialect(),
                        section.max_bytes,
                    )
                    .await
                    .map_err(JsErrorBox::generic)?,
                )),
                _ => RowSource::Jsonl(Box::new(
                    JsonlBlobSource::open(
                        blob,
                        key,
                        &columns,
                        &registry,
                        &args.table,
                        da.dialect(),
                        section.max_bytes,
                    )
                    .await
                    .map_err(JsErrorBox::generic)?,
                )),
            }
        }
        (None, Some(rows)) => {
            if columns.is_empty() {
                return Err(JsErrorBox::generic(
                    "db import: columns are required for an empty rows array",
                ));
            }
            let pairs: Vec<(u64, serde_json::Map<String, Value>)> = rows
                .into_iter()
                .enumerate()
                .map(|(i, r)| (i as u64 + 1, r))
                .collect();
            RowSource::Inline {
                iter: pairs.into_iter(),
                columns: columns.clone(),
                bytes: inline_bytes,
            }
        }
        _ => unreachable!("blobKey/rows 恰一已在参数面裁决"),
    };
    let columns = source.columns().to_vec();
    let table_pk = registry
        .get(&args.table)
        .map(|t| t.primary_key.clone())
        .unwrap_or_default();
    // 7. 登记 → 审计 → consumer。
    let owner = super::db_session::owner_of(&state.borrow());
    let (task_id, _cancel) = import_registry
        .register(owner.clone(), alias.clone(), args.table.clone())
        .map_err(|e| JsErrorBox::generic(e.message()))?;
    import_registry.arm_reaper(
        std::time::Duration::from_secs(section.sweep_interval_secs),
        std::time::Duration::from_secs(section.ttl_secs),
    );
    let audit = {
        let g = state.borrow();
        g.borrow::<Arc<super::StableState>>().audit.clone()
    };
    let task_short = &task_id[..task_id.len().min(12)];
    audit.push(super::audit::AuditEvent {
        ts_ms: ms_now(),
        kind: "db.import.start".into(),
        user: owner.user.clone(),
        tenant: owner.tenant.clone(),
        module: None,
        source: "db.import",
        alias: alias.clone(),
        detail: format!(
            "task={task_short} mode={} format={}",
            mode.as_str(),
            args.format
        ),
    });
    // 自驱 consumer：源读 → 批渲染 → 批事务 → 终态 settle。
    let format = args.format.clone();
    tokio::spawn(run_import(
        task_id.clone(),
        import_registry,
        da,
        source,
        args.table.clone(),
        columns,
        mode,
        table_pk,
        batch_size,
        on_error_skip,
        max_errors,
        section.max_bytes,
        audit,
        owner,
        alias,
        format,
    ));
    Ok(json!({ "taskId": task_id }))
}

/// `importStatus(taskId)`：归属校验；未知/他人/跨重启一律 not found（不泄露存在性）。
#[op2]
#[serde]
pub async fn op_db_import_status(
    state: Rc<RefCell<OpState>>,
    #[string] task_id: String,
) -> Result<serde_json::Value, JsErrorBox> {
    let (registry, owner) = {
        let g = state.borrow();
        let st = g.borrow::<Arc<super::StableState>>().clone();
        (st.import_registry.clone(), super::db_session::owner_of(&g))
    };
    registry.status(&task_id, &owner).ok_or_else(|| {
        JsErrorBox::generic(format!(
            "import task {} not found",
            super::handle_core::id_prefix(PREFIX, &task_id)
        ))
    })
}

/// `importCancel(taskId)`：幂等；not found 口径同 status。
#[op2]
#[serde]
pub async fn op_db_import_cancel(
    state: Rc<RefCell<OpState>>,
    #[string] task_id: String,
) -> Result<serde_json::Value, JsErrorBox> {
    let (registry, owner) = {
        let g = state.borrow();
        let st = g.borrow::<Arc<super::StableState>>().clone();
        (st.import_registry.clone(), super::db_session::owner_of(&g))
    };
    match registry.cancel(&task_id, &owner) {
        Some(_) => Ok(json!({ "cancelled": true })),
        None => Err(JsErrorBox::generic(format!(
            "import task {} not found",
            super::handle_core::id_prefix(PREFIX, &task_id)
        ))),
    }
}

#[cfg(test)]
mod consumer_tests {
    use super::*;
    use crate::bridge::db::TxSession;
    use crate::bridge::{Bridge, DataAccessor, Extras, InMemoryKV, SchemaRegistry};
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;

    /// sqlite 文件 DSN（:memory: 每连接独立库——批事务跨连接不可见，REQ-11 教训）。
    async fn sqlite_file() -> (tempfile::TempDir, Arc<dyn DataAccessor>) {
        let dir = tempfile::tempdir().unwrap();
        let dsn = format!("sqlite://{}/t.db", dir.path().display());
        let db = crate::bridge::db_backend::DbBackendRegistry::builtin()
            .connect(&dsn, dir.path())
            .await
            .unwrap();
        (dir, db)
    }

    fn bridge_with(
        db: Arc<dyn DataAccessor>,
        registry: SchemaRegistry,
        blobs: Option<Arc<crate::bridge::blob::BlobRegistry>>,
        section: super::super::db_policy::DbImportSection,
    ) -> Bridge {
        Bridge::with_dbs_and_loader(
            HashMap::from([("default".to_string(), db)]),
            Arc::new(InMemoryKV::new()),
            registry,
            false,
            None,
            Extras {
                blobs,
                import_section: Some(Arc::new(section)),
                db_access: Some(Arc::new(Default::default())),
                ..Default::default()
            },
        )
    }

    fn tmp_blobs() -> (tempfile::TempDir, Arc<crate::bridge::blob::BlobRegistry>) {
        let d = tempfile::tempdir().unwrap();
        let local = crate::bridge::blob::LocalBlob::new(d.path(), "/v1/api").unwrap();
        (
            d,
            crate::bridge::blob::registry_with_default(Arc::new(local)),
        )
    }

    async fn import_js(b: &Bridge, args: &str) -> Result<Value, Value> {
        let js = format!(
            r#"(async () => {{
                 const r = await db.import({args}).then(
                   (v) => ({{ ok: true, v }}),
                   (e) => ({{ ok: false, code: e.code, msg: String(e.message || e) }}));
                 json.ok(r);
               }})().catch(e => json.fail(500, String(e)));"#,
        );
        let cap = b.run_with(&js, Default::default()).await.unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        let r = v["data"].clone();
        if r["ok"] == true {
            Ok(r["v"].clone())
        } else {
            Err(r)
        }
    }

    /// 轮询直至 status.state 离开 running（deadline 有界——禁固定 sleep 当同步）。
    async fn wait_terminal(b: &Bridge, task_id: &str) -> Value {
        wait_terminal_until(b, task_id, 15).await
    }

    async fn wait_terminal_until(b: &Bridge, task_id: &str, secs: u64) -> Value {
        let deadline = std::time::Instant::now() + Duration::from_secs(secs);
        loop {
            let js = format!(
                r#"(async () => {{
                     const st = await db.importStatus({task_id:?});
                     json.ok(st);
                   }})().catch(e => json.fail(500, String(e)));"#,
            );
            let cap = b.run_with(&js, Default::default()).await.unwrap();
            let v: Value = serde_json::from_slice(&cap.body).unwrap();
            let st = v["data"].clone();
            if st["state"] != "running" {
                return st;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "import task hung in running"
            );
            tokio::task::yield_now().await;
        }
    }

    async fn count_rows(db: &Arc<dyn DataAccessor>, table: &str) -> i64 {
        let rows = db
            .query_with_params(&format!("select count(*) as n from {table}"), &[])
            .await
            .unwrap();
        rows[0]["n"].as_i64().unwrap()
    }

    fn users_registry() -> SchemaRegistry {
        SchemaRegistry::new().table("users", &["id"], &["id", "name"])
    }

    /// blob csv 全链路：读源 → 解析 → 批事务落库 → 状态面。
    #[tokio::test(flavor = "current_thread")]
    async fn import_blob_csv_happy_path_committed_rows() {
        let (_d, db) = sqlite_file().await;
        db.exec_with_params(
            "create table users (id integer primary key, name text)",
            &[],
        )
        .await
        .unwrap();
        let (_bd, blobs) = tmp_blobs();
        let b = bridge_with(
            db.clone(),
            users_registry(),
            Some(blobs.clone()),
            Default::default(),
        );
        let csv = b"id,name\n1,a\n2,b\n3,c\n";
        blobs
            .default()
            .unwrap()
            .put("in/x.csv", csv, None)
            .await
            .unwrap();
        let r = import_js(
            &b,
            r#"{ table: "users", blobKey: "in/x.csv", format: "csv" }"#,
        )
        .await
        .unwrap();
        let st = wait_terminal(&b, r["taskId"].as_str().unwrap()).await;
        assert_eq!(st["state"], json!("done"), "{st}");
        assert_eq!(st["rows"], json!(3), "{st}");
        assert!(st["bytes"].as_u64().unwrap() > 0, "{st}");
        assert_eq!(st["errors"], json!(0), "{st}");
        assert_eq!(count_rows(&db, "users").await, 3);
    }

    /// 内联 rows 源 happy path。
    #[tokio::test(flavor = "current_thread")]
    async fn import_inline_rows_happy_path() {
        let (_d, db) = sqlite_file().await;
        db.exec_with_params(
            "create table users (id integer primary key, name text)",
            &[],
        )
        .await
        .unwrap();
        let b = bridge_with(db.clone(), users_registry(), None, Default::default());
        let r = import_js(
            &b,
            r#"{ table: "users", format: "jsonl", rows: [{ id: 1, name: "a" }, { id: 2, name: "b" }] }"#,
        )
        .await
        .unwrap();
        let st = wait_terminal(&b, r["taskId"].as_str().unwrap()).await;
        assert_eq!(st["state"], json!("done"), "{st}");
        assert_eq!(st["rows"], json!(2), "{st}");
        assert_eq!(count_rows(&db, "users").await, 2);
    }

    /// skip：坏行进 samples（绝对行号），行不落库，好行落库。
    #[tokio::test(flavor = "current_thread")]
    async fn import_skip_bad_rows_samples_absolute_line() {
        let (_d, db) = sqlite_file().await;
        db.exec_with_params(
            "create table users (id integer primary key, name text)",
            &[],
        )
        .await
        .unwrap();
        let (_bd, blobs) = tmp_blobs();
        let b = bridge_with(
            db.clone(),
            users_registry(),
            Some(blobs.clone()),
            Default::default(),
        );
        // header=1, d1=2, d2=3, bad=4, d4=5
        let csv = b"id,name\n1,a\n2,b\nabc,c\n4,d\n";
        blobs
            .default()
            .unwrap()
            .put("in/x.csv", csv, None)
            .await
            .unwrap();
        let r = import_js(
            &b,
            r#"{ table: "users", blobKey: "in/x.csv", format: "csv", onError: "skip" }"#,
        )
        .await
        .unwrap();
        let st = wait_terminal(&b, r["taskId"].as_str().unwrap()).await;
        assert_eq!(st["state"], json!("done"), "{st}");
        assert_eq!(st["errors"], json!(1), "{st}");
        assert_eq!(st["samples"][0]["line"], json!(4), "{st}");
        // sqlite 约束错原文（code 20: datatype mismatch）
        assert!(
            st["samples"][0]["reason"]
                .as_str()
                .unwrap()
                .contains("mismatch"),
            "{st}"
        );
        // plan 口径「raw 含原格」：整行 values 的 JSON 序列化，须含坏格原文
        assert!(
            st["samples"][0]["raw"].as_str().unwrap().contains("abc"),
            "{st}"
        );
        assert_eq!(count_rows(&db, "users").await, 3);
    }

    /// abort：第二批约束冲突 → Failed；sqlite 全单事务特例（修正案②/spec §4.3）
    /// → 整回滚 rows=0、库空。「已提交批保留」语义属多批事务方言，由 T7 PG env 腿覆盖。
    #[tokio::test(flavor = "current_thread")]
    async fn import_abort_batch_rolls_back_full_rollback_sqlite() {
        let (_d, db) = sqlite_file().await;
        db.exec_with_params(
            "create table users (id integer primary key, name text)",
            &[],
        )
        .await
        .unwrap();
        let b = bridge_with(db.clone(), users_registry(), None, Default::default());
        // 批1 = id 1..100；批2 = 51..150（51 撞已插入主键）
        let mut ids: Vec<i64> = (1..=100).collect();
        ids.extend(51..=150);
        let rows: Vec<String> = ids
            .iter()
            .map(|i| format!(r#"{{ "id": {i}, "name": "n{i}" }}"#))
            .collect();
        let args = format!(
            r#"{{ table: "users", format: "jsonl", batchSize: 100, rows: [{}] }}"#,
            rows.join(",")
        );
        let r = import_js(&b, &args).await.unwrap();
        let st = wait_terminal(&b, r["taskId"].as_str().unwrap()).await;
        assert_eq!(st["state"], json!("failed"), "{st}");
        assert_eq!(st["rows"], json!(0), "{st}");
        assert!(!st["error"].as_str().unwrap().is_empty(), "{st}");
        assert_eq!(count_rows(&db, "users").await, 0);
    }

    /// Gated 访问器：批 exec 前 sleep——取消落批间 → 已提交批保留、未提交不落库。
    struct Gated {
        inner: Arc<dyn DataAccessor>,
        gate: Duration,
    }
    struct GatedTx {
        tx: tokio::sync::Mutex<Box<dyn TxSession>>,
        gate: Duration,
    }
    #[async_trait::async_trait]
    impl TxSession for GatedTx {
        async fn query(
            &self,
            sql: &str,
            params: &[Value],
        ) -> crate::bridge::BridgeResult<Vec<super::super::db::Row>> {
            self.tx.lock().await.query(sql, params).await
        }
        async fn exec(&self, sql: &str, params: &[Value]) -> crate::bridge::BridgeResult<i64> {
            tokio::time::sleep(self.gate).await;
            self.tx.lock().await.exec(sql, params).await
        }
        async fn commit(&self) -> crate::bridge::BridgeResult<()> {
            self.tx.lock().await.commit().await
        }
        async fn rollback(&self) -> crate::bridge::BridgeResult<()> {
            self.tx.lock().await.rollback().await
        }
    }
    #[async_trait::async_trait]
    impl DataAccessor for Gated {
        fn dialect(&self) -> super::super::db::Dialect {
            self.inner.dialect()
        }
        async fn begin(&self) -> crate::bridge::BridgeResult<Box<dyn TxSession>> {
            Ok(Box::new(GatedTx {
                tx: tokio::sync::Mutex::new(self.inner.begin().await?),
                gate: self.gate,
            }))
        }
        async fn query_with_params(
            &self,
            sql: &str,
            params: &[Value],
        ) -> crate::bridge::BridgeResult<Vec<super::super::db::Row>> {
            self.inner.query_with_params(sql, params).await
        }
        async fn exec_with_params(
            &self,
            sql: &str,
            params: &[Value],
        ) -> crate::bridge::BridgeResult<i64> {
            self.inner.exec_with_params(sql, params).await
        }
    }

    /// cancel 落批间：Cancelled；sqlite 全单事务特例（修正案②）→ 整回滚
    /// rows=0、库空。批间等待 progress≥100 证明取消发生在执行中途。
    #[tokio::test(flavor = "current_thread")]
    async fn import_cancel_between_batches_full_rollback_sqlite() {
        let (_d, db) = sqlite_file().await;
        db.exec_with_params(
            "create table users (id integer primary key, name text)",
            &[],
        )
        .await
        .unwrap();
        let gated: Arc<dyn DataAccessor> = Arc::new(Gated {
            inner: db.clone(),
            gate: Duration::from_millis(120),
        });
        let b = bridge_with(gated, users_registry(), None, Default::default());
        let rows: Vec<String> = (1..=250)
            .map(|i| format!(r#"{{ "id": {i}, "name": "n{i}" }}"#))
            .collect();
        let args = format!(
            r#"{{ table: "users", format: "jsonl", batchSize: 100, rows: [{}] }}"#,
            rows.join(",")
        );
        let r = import_js(&b, &args).await.unwrap();
        let task_id = r["taskId"].as_str().unwrap().to_string();
        // 等第一批提交（rows==100）再取消
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            let js = format!(
                r#"(async () => {{ const st = await db.importStatus({task_id:?}); json.ok(st); }})().catch(e => json.fail(500, String(e)));"#,
            );
            let cap = b.run_with(&js, Default::default()).await.unwrap();
            let v: Value = serde_json::from_slice(&cap.body).unwrap();
            if v["data"]["rows"].as_u64().unwrap() >= 100 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "first batch never committed"
            );
            tokio::task::yield_now().await;
        }
        let js = format!(
            r#"(async () => {{ await db.importCancel({task_id:?}); json.ok({{}}); }})().catch(e => json.fail(500, String(e)));"#,
        );
        let cap = b.run_with(&js, Default::default()).await.unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        let st = wait_terminal(&b, &task_id).await;
        assert_eq!(st["state"], json!("cancelled"), "{st}");
        assert_eq!(st["rows"], json!(0), "{st}");
        assert_eq!(count_rows(&db, "users").await, 0);
    }

    /// replace：先删后插单事务——预置 2 旧行，导入 3 新行 → 库内恰 3 新行。
    #[tokio::test(flavor = "current_thread")]
    async fn import_replace_mode_deletes_then_inserts() {
        let (_d, db) = sqlite_file().await;
        db.exec_with_params(
            "create table users (id integer primary key, name text)",
            &[],
        )
        .await
        .unwrap();
        db.exec_with_params(
            "insert into users (id, name) values (1, 'old1'), (2, 'old2')",
            &[],
        )
        .await
        .unwrap();
        let b = bridge_with(db.clone(), users_registry(), None, Default::default());
        let r = import_js(
            &b,
            r#"{ table: "users", format: "jsonl", mode: "replace", rows: [{ id: 3, name: "n3" }, { id: 4, name: "n4" }, { id: 5, name: "n5" }] }"#,
        )
        .await
        .unwrap();
        let st = wait_terminal(&b, r["taskId"].as_str().unwrap()).await;
        assert_eq!(st["state"], json!("done"), "{st}");
        assert_eq!(st["rows"], json!(3), "{st}");
        let rows = db
            .query_with_params("select id from users order by id", &[])
            .await
            .unwrap();
        let ids: Vec<i64> = rows.iter().map(|r| r["id"].as_i64().unwrap()).collect();
        assert_eq!(ids, vec![3, 4, 5]);
    }

    /// sqlite rowid upsert 幂等：同数据跑两遍行数不变。
    #[tokio::test(flavor = "current_thread")]
    async fn import_upsert_idempotent_twice() {
        let (_d, db) = sqlite_file().await;
        db.exec_with_params(
            "create table users (id integer primary key, name text)",
            &[],
        )
        .await
        .unwrap();
        let b = bridge_with(db.clone(), users_registry(), None, Default::default());
        let args = r#"{ table: "users", format: "jsonl", mode: "upsert", conflictKeys: ["rowid"], rows: [{ rowid: 1, id: 1, name: "a" }, { rowid: 2, id: 2, name: "b" }] }"#;
        let r1 = import_js(&b, args).await.unwrap();
        let st = wait_terminal(&b, r1["taskId"].as_str().unwrap()).await;
        assert_eq!(st["state"], json!("done"), "{st}");
        let r2 = import_js(&b, args).await.unwrap();
        let st = wait_terminal(&b, r2["taskId"].as_str().unwrap()).await;
        assert_eq!(st["state"], json!("done"), "{st}");
        assert_eq!(count_rows(&db, "users").await, 2);
    }

    /// 终态后 cancel 不翻盘：Done 赢（cancel 幂等返回 true 但 state 保持 done）。
    #[tokio::test(flavor = "current_thread")]
    async fn import_cancel_vs_final_commit_race() {
        let (_d, db) = sqlite_file().await;
        db.exec_with_params(
            "create table users (id integer primary key, name text)",
            &[],
        )
        .await
        .unwrap();
        let b = bridge_with(db.clone(), users_registry(), None, Default::default());
        let r = import_js(
            &b,
            r#"{ table: "users", format: "jsonl", rows: [{ id: 1, name: "a" }] }"#,
        )
        .await
        .unwrap();
        let task_id = r["taskId"].as_str().unwrap().to_string();
        let st = wait_terminal(&b, &task_id).await;
        assert_eq!(st["state"], json!("done"), "{st}");
        let js = format!(
            r#"(async () => {{ const r = await db.importCancel({task_id:?}); json.ok(r); }})().catch(e => json.fail(500, String(e)));"#,
        );
        let cap = b.run_with(&js, Default::default()).await.unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        let st = wait_terminal(&b, &task_id).await;
        assert_eq!(st["state"], json!("done"), "{st}");
    }

    /// Final-fix 1（review Critical）blob 面：数据记录带引号内换行且**后续还有记录**
    /// ——字节推进 off-by-phys 会吃掉下一记录首字节（损坏/整任务失败）。
    #[tokio::test(flavor = "current_thread")]
    async fn import_blob_csv_quoted_newline_mid_file_keeps_following_rows() {
        let (_d, db) = sqlite_file().await;
        db.exec_with_params(
            "create table users (id integer primary key, name text)",
            &[],
        )
        .await
        .unwrap();
        let (_bd, blobs) = tmp_blobs();
        let b = bridge_with(
            db.clone(),
            users_registry(),
            Some(blobs.clone()),
            Default::default(),
        );
        // header=1；记录1（第 2-3 物理行）含引号内换行；记录2 从第 4 行起
        let csv = b"id,name\n1,\"a\nb\"\n2,c\n";
        blobs
            .default()
            .unwrap()
            .put("in/q.csv", csv, None)
            .await
            .unwrap();
        let r = import_js(
            &b,
            r#"{ table: "users", blobKey: "in/q.csv", format: "csv" }"#,
        )
        .await
        .unwrap();
        let st = wait_terminal(&b, r["taskId"].as_str().unwrap()).await;
        assert_eq!(st["state"], json!("done"), "{st}");
        assert_eq!(st["rows"], json!(2), "{st}");
        assert_eq!(count_rows(&db, "users").await, 2);
    }

    /// Final-fix 2（review Important）：max_bytes 读中强制——超长无换行记录不得
    /// 一直缓冲到 EOF（防 OOM）；错误点名 max_bytes。
    #[tokio::test(flavor = "current_thread")]
    async fn blob_source_enforces_max_bytes_mid_record() {
        let (_bd, blobs) = tmp_blobs();
        // 首记录完整（open 需要首窗可解析）；第二条记录 ~1.5MiB 无换行 → 跨窗读到超闸
        let mut csv: Vec<u8> = b"1,a\n2,\"".to_vec();
        csv.extend(std::iter::repeat_n(b'x', 1536 * 1024));
        csv.extend_from_slice(b"\"\n");
        blobs
            .default()
            .unwrap()
            .put("in/huge.csv", &csv, None)
            .await
            .unwrap();
        let registry = SchemaRegistry::new(); // 未登记 → Unknown 类型，列名直通
        let mut src = super::CsvBlobSource::open(
            blobs.default().unwrap(),
            "in/huge.csv",
            false,
            &["id".to_string(), "txt".to_string()],
            &registry,
            "whatever",
            super::super::db::Dialect::Sqlite,
            1024 * 1024, // max_bytes：第二条记录必然读超
        )
        .await
        .unwrap();
        // 第一条记录正常
        assert!(matches!(src.next_row().await, Ok(Some(_))));
        // 第二条：读中拒（点名 max_bytes），而非缓冲到 EOF
        match src.next_row().await {
            Err(super::SourceErr::Io(e)) => assert!(e.contains("max_bytes"), "{e}"),
            _ => panic!("expected max_bytes Io error mid-read"),
        }
    }

    /// Final-fix 3（review Important）：sqlite 变量上限 32766（< PG/MySQL 65535）——
    /// 7 列宽表默认批 5000 行 = 35000 变量，超 sqlite 上限；批收敛须方言感知。
    #[tokio::test(flavor = "current_thread")]
    async fn import_sqlite_wide_table_default_batch_converged() {
        let (_d, db) = sqlite_file().await;
        db.exec_with_params(
            "create table wide (id integer primary key, a text, b text, c text, d text, e text, f text)",
            &[],
        )
        .await
        .unwrap();
        let registry =
            SchemaRegistry::new().table("wide", &["id"], &["id", "a", "b", "c", "d", "e", "f"]);
        let b = bridge_with(db.clone(), registry, None, Default::default());
        let rows: Vec<String> = (1..=5000)
            .map(|i| format!(r#"{{ "id": {i}, "a": "a{i}", "b": "b{i}", "c": "c{i}", "d": "d{i}", "e": "e{i}", "f": "f{i}" }}"#))
            .collect();
        let args = format!(
            r#"{{ table: "wide", format: "jsonl", rows: [{}] }}"#,
            rows.join(",")
        );
        let r = import_js(&b, &args).await.unwrap();
        let st = wait_terminal(&b, r["taskId"].as_str().unwrap()).await;
        assert_eq!(st["state"], json!("done"), "{st}");
        assert_eq!(st["rows"], json!(5000), "{st}");
        assert_eq!(count_rows(&db, "wide").await, 5000);
    }

    /// blob 源超 max_bytes：中途 Failed，错误点名 max_bytes（不静默截断）。
    #[tokio::test(flavor = "current_thread")]
    async fn import_blob_over_max_bytes_fails_named() {
        let (_d, db) = sqlite_file().await;
        db.exec_with_params(
            "create table users (id integer primary key, name text)",
            &[],
        )
        .await
        .unwrap();
        let (_bd, blobs) = tmp_blobs();
        let section = super::super::db_policy::DbImportSection {
            max_bytes: 1024 * 1024, // 段有效下限
            ..Default::default()
        };
        let b = bridge_with(db.clone(), users_registry(), Some(blobs.clone()), section);
        // ~1.4MB 源（id 到 100000 → 每行 ~12B）
        let mut csv: Vec<u8> = b"id,name\n".to_vec();
        for i in 0..100_000i64 {
            csv.extend_from_slice(format!("{i},n{i}\n").as_bytes());
        }
        assert!(csv.len() as u64 > 1024 * 1024);
        blobs
            .default()
            .unwrap()
            .put("in/big.csv", &csv, None)
            .await
            .unwrap();
        let r = import_js(
            &b,
            r#"{ table: "users", blobKey: "in/big.csv", format: "csv" }"#,
        )
        .await
        .unwrap();
        let st = wait_terminal(&b, r["taskId"].as_str().unwrap()).await;
        assert_eq!(st["state"], json!("failed"), "{st}");
        assert!(st["error"].as_str().unwrap().contains("max_bytes"), "{st}");
    }

    // ---- env-gated 真库腿（OJ_TEST_PG / OJ_TEST_MYSQL 缺 → 该腿跳过）----

    /// env-gated 真库连接；缺 env 的腿打印 skip 并略过。
    async fn real_dbs() -> Vec<(&'static str, Arc<dyn DataAccessor>)> {
        let mut out = Vec::new();
        for env in ["OJ_TEST_PG", "OJ_TEST_MYSQL"] {
            match std::env::var(env) {
                Ok(url) if !url.is_empty() => {
                    let db = crate::bridge::SqlxAccessor::arc(&url).await.unwrap();
                    out.push((env, db));
                }
                _ => eprintln!("skip: {env} unset"),
            }
        }
        out
    }

    /// env 腿目标表注册：id BigInt + f1..f9 Text；shared → 租户守卫不介入
    /// （这些腿验性能/幂等/取消，守卫矩阵由离线腿覆盖）。
    /// 表名按测试分发（cargo 并行跑测试共享同一真库，撞名即互踩）。
    fn imp_registry(table: &str) -> SchemaRegistry {
        let mut cols: Vec<(&str, ColumnType)> = vec![("id", ColumnType::BigInt)];
        cols.extend((1..=9).map(|i| {
            (
                &*Box::leak(format!("f{i}").into_boxed_str()),
                ColumnType::Text,
            )
        }));
        SchemaRegistry::new().table_owned_shared_typed("tester", table, &["id"], &cols, true)
    }

    /// DDL 无值可参数化（静态列清单）；表名来自测试内常量。
    async fn imp_reset(db: &Arc<dyn DataAccessor>, table: &str) {
        db.exec_with_params(&format!("drop table if exists {table}"), &[])
            .await
            .unwrap();
        db.exec_with_params(
            &format!("create table {table} (id bigint primary key, f1 text, f2 text, f3 text, f4 text, f5 text, f6 text, f7 text, f8 text, f9 text)"),
            &[],
        )
        .await
        .unwrap();
    }

    /// 500k×10 CSV 全链路性能腿：Done、rows=500k、<60s（实测秒数入 ledger）。
    #[tokio::test(flavor = "current_thread")]
    async fn real_pg_mysql_import_perf_500k_under_60s() {
        const N: i64 = 500_000;
        let mut csv: Vec<u8> = b"id,f1,f2,f3,f4,f5,f6,f7,f8,f9\n".to_vec();
        csv.reserve((N as usize) * 36);
        for i in 0..N {
            csv.extend_from_slice(
                format!("{i},v{i}a,v{i}b,v{i}c,v{i}d,v{i}e,v{i}f,v{i}g,v{i}h,v{i}i\n").as_bytes(),
            );
        }
        for (env, db) in real_dbs().await {
            imp_reset(&db, "imp_perf").await;
            let (_bd, blobs) = tmp_blobs();
            blobs
                .default()
                .unwrap()
                .put("in/perf.csv", &csv, None)
                .await
                .unwrap();
            let b = bridge_with(
                db.clone(),
                imp_registry("imp_perf"),
                Some(blobs),
                Default::default(),
            );
            let started = tokio::time::Instant::now();
            let r = import_js(
                &b,
                r#"{ table: "imp_perf", blobKey: "in/perf.csv", format: "csv" }"#,
            )
            .await
            .unwrap();
            let st = wait_terminal_until(&b, r["taskId"].as_str().unwrap(), 90).await;
            let elapsed = started.elapsed();
            assert_eq!(st["state"], json!("done"), "{env}: {st}");
            assert_eq!(st["rows"], json!(N), "{env}: {st}");
            eprintln!("{env}: 500k rows import in {elapsed:.1?}");
            assert!(
                elapsed < Duration::from_secs(60),
                "{env}: {elapsed:.1?} >= 60s"
            );
            assert_eq!(count_rows(&db, "imp_perf").await, N);
            db.exec_with_params("drop table imp_perf", &[])
                .await
                .unwrap();
        }
    }

    /// upsert 幂等：同数据 import 两遍，行数不变（PG ON CONFLICT / MySQL ODKU）。
    #[tokio::test(flavor = "current_thread")]
    async fn real_pg_mysql_upsert_idempotent() {
        const N: i64 = 3_000;
        let mut csv: Vec<u8> = b"id,f1\n".to_vec();
        for i in 0..N {
            csv.extend_from_slice(format!("{i},n{i}\n").as_bytes());
        }
        for (env, db) in real_dbs().await {
            imp_reset(&db, "imp_up").await;
            let (_bd, blobs) = tmp_blobs();
            blobs
                .default()
                .unwrap()
                .put("in/up.csv", &csv, None)
                .await
                .unwrap();
            let b = bridge_with(
                db.clone(),
                imp_registry("imp_up"),
                Some(blobs),
                Default::default(),
            );
            for round in 1..=2 {
                let r = import_js(
                    &b,
                    r#"{ table: "imp_up", blobKey: "in/up.csv", format: "csv", mode: "upsert", conflictKeys: ["id"] }"#,
                )
                .await
                .unwrap();
                let st = wait_terminal(&b, r["taskId"].as_str().unwrap()).await;
                assert_eq!(st["state"], json!("done"), "{env} round{round}: {st}");
                assert_eq!(st["rows"], json!(N), "{env} round{round}: {st}");
                assert_eq!(count_rows(&db, "imp_up").await, N, "{env} round{round}");
            }
            db.exec_with_params("drop table imp_up", &[]).await.unwrap();
        }
    }

    /// cancel 落批间：Cancelled、已提交批保留（rows>0、库行数==rows、< 总量）。
    #[tokio::test(flavor = "current_thread")]
    async fn real_pg_mysql_cancel_mid_import() {
        const N: i64 = 100_000;
        let mut csv: Vec<u8> = b"id,f1\n".to_vec();
        for i in 0..N {
            csv.extend_from_slice(format!("{i},n{i}\n").as_bytes());
        }
        for (env, db) in real_dbs().await {
            imp_reset(&db, "imp_cancel").await;
            let (_bd, blobs) = tmp_blobs();
            blobs
                .default()
                .unwrap()
                .put("in/cancel.csv", &csv, None)
                .await
                .unwrap();
            let b = bridge_with(
                db.clone(),
                imp_registry("imp_cancel"),
                Some(blobs),
                Default::default(),
            );
            let r = import_js(
                &b,
                r#"{ table: "imp_cancel", blobKey: "in/cancel.csv", format: "csv", batchSize: 100 }"#,
            )
            .await
            .unwrap();
            let task_id = r["taskId"].as_str().unwrap().to_string();
            // 等第一批提交（rows>=100）再取消——批间窗口
            let deadline = std::time::Instant::now() + Duration::from_secs(15);
            loop {
                let js = format!(
                    r#"(async () => {{ const st = await db.importStatus({task_id:?}); json.ok(st); }})().catch(e => json.fail(500, String(e)));"#,
                );
                let cap = b.run_with(&js, Default::default()).await.unwrap();
                let v: Value = serde_json::from_slice(&cap.body).unwrap();
                if v["data"]["rows"].as_u64().unwrap() >= 100 {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "{env}: first batch never committed"
                );
                tokio::task::yield_now().await;
            }
            let js = format!(
                r#"(async () => {{ await db.importCancel({task_id:?}); json.ok({{}}); }})().catch(e => json.fail(500, String(e)));"#,
            );
            let cap = b.run_with(&js, Default::default()).await.unwrap();
            let v: Value = serde_json::from_slice(&cap.body).unwrap();
            assert_eq!(v["code"], 0, "{env}: {v}");
            let st = wait_terminal_until(&b, &task_id, 30).await;
            assert_eq!(st["state"], json!("cancelled"), "{env}: {st}");
            let committed = st["rows"].as_u64().unwrap();
            assert!(committed > 0 && committed < N as u64, "{env}: {st}");
            assert_eq!(
                count_rows(&db, "imp_cancel").await as u64,
                committed,
                "{env}: table rows != committed"
            );
            db.exec_with_params("drop table imp_cancel", &[])
                .await
                .unwrap();
        }
    }
}

// ---------------------------------------------------------------------------
// 行源 + consumer（REQ-12 T6）：blob read_range 窗口流式读 / 批事务 / abort / skip /
// maxErrors / 批间 cancel。行号权威全链路 = ParsedRow.line（源文件绝对行号）。
// ---------------------------------------------------------------------------

use super::db::Dialect;
use super::registry::ColumnType;

/// read_range 窗口（1 MiB）。
const READ_WINDOW: u64 = 1024 * 1024;

/// 行源错误：Bad = 行级解析错（abort/skip 由 consumer 裁决）；Io = 源读失败（任务级）。
#[derive(Debug)]
pub(crate) enum SourceErr {
    Bad(parse::ParseError),
    Io(String),
}

impl From<parse::ParseError> for SourceErr {
    fn from(e: parse::ParseError) -> Self {
        SourceErr::Bad(e)
    }
}

/// 列白名单 + 类型解析：已登记表逐列 has_column（未命中 → Err 点名列）；
/// 未登记表无白名单可验（守卫矩阵已放行）→ Unknown 类型按 text 处理。
fn columns_guard(
    columns: &[String],
    registry: &super::registry::SchemaRegistry,
    table: &str,
    dialect: super::db::Dialect,
) -> Result<Vec<ColumnType>, String> {
    match registry.get(table) {
        Some(t) => {
            let sqlite_rowid = dialect == super::db::Dialect::Sqlite;
            let mut types = Vec::with_capacity(columns.len());
            for c in columns {
                match t.columns.get(c) {
                    Some(d) => types.push(d.ty),
                    None if sqlite_rowid && c == "rowid" => types.push(ColumnType::Integer),
                    None => {
                        return Err(format!("unknown column '{c}' in import columns"));
                    }
                }
            }
            Ok(types)
        }
        None => Ok(vec![ColumnType::Unknown; columns.len()]),
    }
}

/// blob CSV 流式源：窗口递增读；缓冲只留「未解析余量 + 单窗口」
/// （行缓冲有界——跨窗 quoted 记录 ≤ 记录本身长度，不随文件增长）。
pub(crate) struct CsvBlobSource {
    blob: Arc<dyn super::blob::BlobBackend>,
    key: String,
    offset: u64,
    buf: Vec<u8>,
    pos: usize,
    line: u64,
    eof: bool,
    bytes_read: u64,
    max_bytes: u64, // 读中硬闸：超限即 Err（防无换行巨记录 OOM）
    columns: Vec<String>,
    col_types: Vec<ColumnType>,
}

impl CsvBlobSource {
    /// 打开：读首窗 → 解析列（args_columns 优先，否则表头记录）→ 白名单 + 类型。
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn open(
        blob: Arc<dyn super::blob::BlobBackend>,
        key: &str,
        header: bool,
        args_columns: &[String],
        registry: &super::registry::SchemaRegistry,
        table: &str,
        dialect: super::db::Dialect,
        max_bytes: u64,
    ) -> Result<Self, String> {
        let w = blob
            .read_range(key, 0, READ_WINDOW)
            .await
            .map_err(|e| format!("db import: blob read failed: {e}"))?;
        if w.is_empty() {
            return Err(format!("db import: blob source '{key}' is empty"));
        }
        let bytes_read = w.len() as u64;
        let safe = w
            .iter()
            .rposition(|&b| b == b'\n')
            .map(|i| i + 1)
            .unwrap_or(w.len());
        let (rec, phys) = parse::read_record(&w[..safe], 0)
            .map_err(|_| "db import: unterminated quoted field near line 1".to_string())?;
        let columns = if !args_columns.is_empty() {
            args_columns.to_vec()
        } else if header {
            parse::header_cells(rec)
                .ok_or_else(|| "db import: malformed csv header at line 1".to_string())?
        } else {
            return Err("db import: columns are required for csv with header=false".into());
        };
        let col_types = columns_guard(&columns, registry, table, dialect)?;
        let (pos, line) = if header {
            // pos 推进 = 切片长 + 1（切片只排除单个终止 \n）；line 计物理行（嵌入换行算）
            (rec.len() + 1, 1 + phys as u64)
        } else {
            (0, 1)
        };
        Ok(Self {
            blob,
            key: key.to_string(),
            offset: w.len() as u64,
            buf: w,
            pos,
            line,
            eof: false,
            bytes_read,
            max_bytes,
            columns,
            col_types,
        })
    }

    fn has_record(&self) -> bool {
        self.eof || self.buf[self.pos..].contains(&b'\n')
    }

    async fn fill(&mut self) -> Result<(), String> {
        if self.eof {
            return Ok(());
        }
        let w = self
            .blob
            .read_range(&self.key, self.offset, READ_WINDOW)
            .await
            .map_err(|e| format!("db import: blob read failed: {e}"))?;
        self.offset += w.len() as u64;
        self.bytes_read += w.len() as u64;
        // 读中硬闸：超限即拒（点名 max_bytes）——无换行巨记录不得缓冲到 OOM
        if self.bytes_read > self.max_bytes {
            return Err(format!(
                "db import: source exceeds db_import.max_bytes ({})",
                self.max_bytes
            ));
        }
        if w.is_empty() {
            self.eof = true;
        } else {
            self.buf.extend_from_slice(&w);
        }
        Ok(())
    }

    pub(crate) async fn next_row(&mut self) -> Result<Option<parse::ParsedRow>, SourceErr> {
        loop {
            if self.pos >= self.buf.len() {
                if self.eof {
                    return Ok(None);
                }
                self.fill().await.map_err(SourceErr::Io)?;
                if self.pos >= self.buf.len() && self.eof {
                    return Ok(None);
                }
                continue;
            }
            if !self.has_record() {
                self.fill().await.map_err(SourceErr::Io)?;
                continue;
            }
            let start_line = self.line;
            let safe_end = if self.eof {
                self.buf.len()
            } else {
                self.pos
                    + self.buf[self.pos..]
                        .iter()
                        .rposition(|&b| b == b'\n')
                        .map(|i| i + 1)
                        .unwrap_or(self.buf.len())
            };
            match parse::read_record(&self.buf[self.pos..safe_end], 0) {
                Ok((rec, phys)) => {
                    // 先用完 rec（借自 self.buf），再动 self.buf，避免借用冲突
                    let row =
                        parse::row_from_record(rec, start_line, &self.columns, &self.col_types);
                    // 推进 = 切片长 + 1（切片只排除单个终止 \n；嵌入换行已在切片内）
                    let advance = rec.len() + 1;
                    self.pos += advance;
                    self.line += phys as u64;
                    if self.pos > READ_WINDOW as usize {
                        self.buf.drain(..self.pos);
                        self.pos = 0;
                    }
                    return row.map(Some).map_err(SourceErr::Bad);
                }
                Err(()) => {
                    if self.eof {
                        // 不闭合引号到 EOF：该行 Bad，吞掉剩余输入防死循环
                        let raw = String::from_utf8_lossy(&self.buf[self.pos..]).into_owned();
                        self.pos = self.buf.len();
                        return Err(SourceErr::Bad(parse::ParseError::Bad {
                            line: start_line,
                            reason: "unterminated quoted field".into(),
                            raw,
                        }));
                    }
                    self.fill().await.map_err(SourceErr::Io)?;
                }
            }
        }
    }
}

/// blob JSONL 流式源：逐物理行；列从 args_columns 或首个非空白行键解析。
pub(crate) struct JsonlBlobSource {
    blob: Arc<dyn super::blob::BlobBackend>,
    key: String,
    offset: u64,
    buf: Vec<u8>,
    pos: usize,
    line: u64,
    eof: bool,
    bytes_read: u64,
    max_bytes: u64, // 读中硬闸（同 CsvBlobSource）
    columns: Vec<String>,
    pending: Option<(u64, String)>,
}

impl JsonlBlobSource {
    pub(crate) async fn open(
        blob: Arc<dyn super::blob::BlobBackend>,
        key: &str,
        args_columns: &[String],
        registry: &super::registry::SchemaRegistry,
        table: &str,
        dialect: super::db::Dialect,
        max_bytes: u64,
    ) -> Result<Self, String> {
        let mut s = Self {
            blob,
            key: key.to_string(),
            offset: 0,
            buf: Vec::new(),
            pos: 0,
            line: 1,
            eof: false,
            bytes_read: 0,
            max_bytes,
            columns: Vec::new(),
            pending: None,
        };
        // 首个非空白行：定列（缺省 = 行键），并留作 pending。
        loop {
            let (line_no, text) = match s.next_line().await? {
                Some(v) => v,
                None => return Err(format!("db import: blob source '{key}' has no data rows")),
            };
            let trimmed = text.trim();
            if trimmed.is_empty() {
                continue;
            }
            s.columns = if !args_columns.is_empty() {
                args_columns.to_vec()
            } else {
                let v: Value = serde_json::from_str(trimmed)
                    .map_err(|e| format!("db import: invalid json at line {line_no}: {e}"))?;
                let obj = v
                    .as_object()
                    .ok_or_else(|| format!("db import: line {line_no} must be a json object"))?;
                obj.keys().cloned().collect()
            };
            columns_guard(&s.columns, registry, table, dialect)?;
            s.pending = Some((line_no, trimmed.to_string()));
            return Ok(s);
        }
    }

    /// 取一条完整物理行（含行号）；EOF 且无余量 → None。
    async fn next_line(&mut self) -> Result<Option<(u64, String)>, String> {
        loop {
            if let Some(i) = self.buf[self.pos..].iter().position(|&b| b == b'\n') {
                let line_no = self.line;
                let text = String::from_utf8_lossy(&self.buf[self.pos..self.pos + i])
                    .trim_end_matches('\r')
                    .to_string();
                self.pos += i + 1;
                self.line += 1;
                if self.pos > READ_WINDOW as usize {
                    self.buf.drain(..self.pos);
                    self.pos = 0;
                }
                return Ok(Some((line_no, text)));
            }
            if self.eof {
                if self.pos < self.buf.len() {
                    let raw = self.buf[self.pos..].to_vec();
                    self.pos = self.buf.len();
                    let line_no = self.line;
                    return Ok(Some((line_no, String::from_utf8_lossy(&raw).to_string())));
                }
                return Ok(None);
            }
            let w = self
                .blob
                .read_range(&self.key, self.offset, READ_WINDOW)
                .await
                .map_err(|e| format!("db import: blob read failed: {e}"))?;
            self.offset += w.len() as u64;
            self.bytes_read += w.len() as u64;
            // 读中硬闸：超限即拒（点名 max_bytes）——单行巨 JSON 不得缓冲到 OOM
            if self.bytes_read > self.max_bytes {
                return Err(format!(
                    "db import: source exceeds db_import.max_bytes ({})",
                    self.max_bytes
                ));
            }
            if w.is_empty() {
                self.eof = true;
            } else {
                self.buf.extend_from_slice(&w);
            }
        }
    }

    pub(crate) async fn next_row(&mut self) -> Result<Option<parse::ParsedRow>, SourceErr> {
        loop {
            let (line_no, text) = match self.pending.take() {
                Some(v) => v,
                None => match self.next_line().await.map_err(SourceErr::Io)? {
                    Some(v) => v,
                    None => return Ok(None),
                },
            };
            let trimmed = text.trim();
            if trimmed.is_empty() {
                continue; // 空白行跳过（绝对行号已推进）
            }
            return parse::jsonl_row(trimmed, line_no, &self.columns)
                .map(Some)
                .map_err(SourceErr::Bad);
        }
    }
}

/// 统一行源：内联 rows / blob csv / blob jsonl。
pub(crate) enum RowSource {
    Inline {
        iter: std::vec::IntoIter<(u64, serde_json::Map<String, Value>)>,
        columns: Vec<String>,
        bytes: u64,
    },
    Csv(Box<CsvBlobSource>),
    Jsonl(Box<JsonlBlobSource>),
}

impl RowSource {
    pub(crate) fn columns(&self) -> &[String] {
        match self {
            RowSource::Inline { columns, .. } => columns,
            RowSource::Csv(s) => &s.columns,
            RowSource::Jsonl(s) => &s.columns,
        }
    }

    pub(crate) fn bytes(&self) -> u64 {
        match self {
            RowSource::Inline { bytes, .. } => *bytes,
            RowSource::Csv(s) => s.bytes_read,
            RowSource::Jsonl(s) => s.bytes_read,
        }
    }

    pub(crate) async fn next_row(&mut self) -> Result<Option<parse::ParsedRow>, SourceErr> {
        match self {
            RowSource::Inline { iter, columns, .. } => {
                let Some((line, obj)) = iter.next() else {
                    return Ok(None);
                };
                let values = columns
                    .iter()
                    .map(|c| obj.get(c).cloned().unwrap_or(Value::Null))
                    .collect();
                Ok(Some(parse::ParsedRow { values, line }))
            }
            RowSource::Csv(s) => s.next_row().await,
            RowSource::Jsonl(s) => s.next_row().await,
        }
    }
}

/// 收一批（至多 n 行）：skip 模式的行级错在此记 samples 继续；abort 模式的
/// 解析错即刻整任务 Failed。返回 Ok((批, 源尽)) 或 Err(任务级失败原因)。
async fn collect_batch(
    source: &mut RowSource,
    n: usize,
    skip: bool,
    max_errors: u64,
    errors: &mut u64,
    registry: &ImportRegistry,
    task_id: &str,
) -> Result<(Vec<(Vec<Value>, u64)>, bool), String> {
    let mut batch = Vec::with_capacity(n.min(1024));
    let mut done = false;
    while batch.len() < n && !done {
        match source.next_row().await {
            Ok(Some(row)) => batch.push((row.values, row.line)),
            Ok(None) => done = true,
            Err(SourceErr::Io(e)) => return Err(e),
            Err(SourceErr::Bad(parse::ParseError::Bad { line, reason, raw })) => {
                registry.push_sample(
                    task_id,
                    ImportSample {
                        line,
                        reason: reason.clone(),
                        raw: Some(raw),
                    },
                );
                *errors += 1;
                if !skip {
                    return Err(format!("db import: row {line}: {reason}"));
                }
                if *errors > max_errors {
                    return Err(format!("db import: errors exceed maxErrors ({max_errors})"));
                }
            }
        }
    }
    Ok((batch, done))
}

/// consumer 执行体（T6）：批事务 + 批间 cancel + abort/skip/maxErrors。
/// rows 语义 = 已提交行数（取消不回退）；sqlite 全单事务特例下取消/失败 = 全回滚
/// （无中间提交可保留，rows=0——spec §4.3 性能关键句的代价面，文档钉死）。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_import(
    task_id: String,
    registry: Arc<ImportRegistry>,
    da: Arc<dyn super::db::DataAccessor>,
    mut source: RowSource,
    table: String,
    columns: Vec<String>,
    mode: Mode,
    table_pk: Vec<String>,
    batch_size: usize,
    skip: bool,
    max_errors: u64,
    max_bytes: u64,
    audit: Arc<super::audit::AuditStore>,
    owner: Owner,
    alias: String,
    _format: String, // 源已自带格式；参数保留给审计/日志面
) {
    let dialect = da.dialect();
    let ncols = columns.len().max(1);
    let render_mode = |keys: &[String]| match &mode {
        Mode::Append | Mode::Replace => render::ImportMode::Append,
        Mode::Upsert { .. } => render::ImportMode::Upsert {
            conflict_keys: keys.to_vec(),
        },
    };
    let keys: Vec<String> = match &mode {
        Mode::Upsert { conflict_keys } => conflict_keys.clone(),
        _ => Vec::new(),
    };
    let single_tx = dialect == Dialect::Sqlite; // sqlite 全单事务特例（含 replace）
    let mut errors: u64 = 0;
    let mut committed: u64 = 0;

    let bytes = |source: &RowSource| source.bytes();

    // 执行一批：skip = 批内逐行 INSERT（行级错记 sample 继续，超 maxErrors 整批败）；
    // abort = 多值整批（批错即整批回滚，由调用侧 rollback）。tx 由调用侧持有。
    #[allow(clippy::too_many_arguments)]
    async fn exec_rows(
        tx: &mut dyn super::db::TxSession,
        dialect: Dialect,
        table: &str,
        columns: &[String],
        batch: &[(Vec<Value>, u64)],
        mode: &render::ImportMode,
        table_pk: &[String],
        skip: bool,
        registry: &ImportRegistry,
        task_id: &str,
        errors: &mut u64,
        max_errors: u64,
    ) -> Result<(), String> {
        if skip {
            for (values, line) in batch {
                let b = render::render_batch(
                    dialect,
                    table,
                    columns,
                    std::slice::from_ref(values),
                    mode,
                    table_pk,
                )?;
                if let Err(e) = tx.exec(&b.sql, &b.params).await {
                    *errors += 1;
                    registry.push_sample(
                        task_id,
                        ImportSample {
                            line: *line,
                            reason: format!("insert failed: {e}"),
                            raw: Some(serde_json::to_string(values).unwrap_or_default()),
                        },
                    );
                    if *errors > max_errors {
                        return Err(format!("db import: errors exceed maxErrors ({max_errors})"));
                    }
                }
            }
            Ok(())
        } else {
            let values: Vec<Vec<Value>> = batch.iter().map(|(v, _)| v.clone()).collect();
            let b = render::render_batch(dialect, table, columns, &values, mode, table_pk)?;
            tx.exec(&b.sql, &b.params)
                .await
                .map_err(|e| e.to_string())?;
            Ok(())
        }
    }

    enum End {
        Done,
        Failed(String),
        Cancelled,
    }

    // 主循环：multi-tx（网络方言批事务）或 single-tx（sqlite / replace）。
    let mut tx_holder: Option<Box<dyn super::db::TxSession>> = None;
    let end: End = async {
        if single_tx || mode == Mode::Replace {
            match da.begin().await {
                Ok(t) => tx_holder = Some(t),
                Err(e) => return End::Failed(format!("begin failed: {e}")),
            }
            if mode == Mode::Replace {
                let del = match render::render_delete(dialect, &table) {
                    Ok(d) => d,
                    Err(e) => return End::Failed(e),
                };
                if let Err(e) = tx_holder.as_mut().expect("set").exec(&del, &[]).await {
                    let _ = tx_holder.as_mut().expect("set").rollback().await;
                    return End::Failed(format!("replace delete failed: {e}"));
                }
            }
        }
        loop {
            // 批间取消（sticky 标志——notify 只作唤醒，权威是标志本身）
            if registry.cancel_requested(&task_id) {
                return End::Cancelled;
            }
            if source.bytes() > max_bytes {
                return End::Failed(format!(
                    "db import: source exceeds db_import.max_bytes ({max_bytes})"
                ));
            }
            let n = render::batch_rows(dialect, usize::MAX, batch_size, ncols);
            let (batch, done) = match collect_batch(
                &mut source,
                n,
                skip,
                max_errors,
                &mut errors,
                &registry,
                &task_id,
            )
            .await
            {
                Ok(v) => v,
                Err(e) => return End::Failed(e),
            };
            if batch.is_empty() {
                return End::Done; // 源尽（或 skip 滤空）
            }
            let tx = tx_holder.as_deref_mut();
            let r = match tx {
                Some(tx) => {
                    exec_rows(
                        tx,
                        dialect,
                        &table,
                        &columns,
                        &batch,
                        &render_mode(&keys),
                        &table_pk,
                        skip,
                        &registry,
                        &task_id,
                        &mut errors,
                        max_errors,
                    )
                    .await
                }
                None => {
                    // 网络方言批事务：begin → exec → commit
                    match da.begin().await {
                        Ok(mut tx) => {
                            let r = exec_rows(
                                &mut *tx,
                                dialect,
                                &table,
                                &columns,
                                &batch,
                                &render_mode(&keys),
                                &table_pk,
                                skip,
                                &registry,
                                &task_id,
                                &mut errors,
                                max_errors,
                            )
                            .await;
                            match r {
                                Ok(()) => match tx.commit().await {
                                    Ok(()) => Ok(()),
                                    Err(e) => {
                                        let _ = tx.rollback().await;
                                        Err(format!("commit failed: {e}"))
                                    }
                                },
                                Err(e) => {
                                    let _ = tx.rollback().await;
                                    Err(e)
                                }
                            }
                        }
                        Err(e) => Err(format!("begin failed: {e}")),
                    }
                }
            };
            if let Err(e) = r {
                return End::Failed(e);
            }
            committed += batch.len() as u64;
            registry.progress(&task_id, committed, bytes(&source));
            if done {
                return End::Done;
            }
        }
    }
    .await;

    // 收口：commit（single-tx Done）/ rollback（其余），终态 settle（先到者赢）。
    let fin = match end {
        End::Done => {
            if let Some(tx) = tx_holder.take() {
                if let Err(e) = tx.commit().await {
                    let _ = tx.rollback().await;
                    Finish::Failed {
                        rows: 0,
                        bytes: bytes(&source),
                        error: format!("commit failed: {e}"),
                    }
                } else {
                    Finish::Done {
                        rows: committed,
                        bytes: bytes(&source),
                    }
                }
            } else {
                Finish::Done {
                    rows: committed,
                    bytes: bytes(&source),
                }
            }
        }
        End::Cancelled => {
            if let Some(tx) = tx_holder.take() {
                let _ = tx.rollback().await; // sqlite 特例：全回滚
            }
            Finish::Cancelled {
                rows: if single_tx || mode == Mode::Replace {
                    0
                } else {
                    committed
                },
                bytes: bytes(&source),
            }
        }
        End::Failed(e) => {
            if let Some(tx) = tx_holder.take() {
                let _ = tx.rollback().await; // sqlite 特例：全回滚
            }
            Finish::Failed {
                rows: if single_tx || mode == Mode::Replace {
                    0
                } else {
                    committed
                },
                bytes: bytes(&source),
                error: e,
            }
        }
    };
    let (final_state, err_detail) = match &fin {
        Finish::Done { .. } => ("done", ""),
        Finish::Failed { error, .. } => ("failed", error.as_str()),
        Finish::Cancelled { .. } => ("cancelled", ""),
    };
    let final_rows = match &fin {
        Finish::Done { rows, .. } => *rows,
        Finish::Failed { rows, .. } => *rows,
        Finish::Cancelled { rows, .. } => *rows,
    };
    let final_bytes = match &fin {
        Finish::Done { bytes, .. } => *bytes,
        Finish::Failed { bytes, .. } => *bytes,
        Finish::Cancelled { bytes, .. } => *bytes,
    };
    audit.push(super::audit::AuditEvent {
        ts_ms: ms_now(),
        kind: format!("db.import.{final_state}"),
        user: owner.user.clone(),
        tenant: owner.tenant.clone(),
        module: None,
        source: "db.import",
        alias,
        detail: format!(
            "task={} rows={final_rows} bytes={final_bytes} {err_detail}",
            &task_id[..task_id.len().min(12)],
        ),
    });
    registry.finish(&task_id, fin);
}
