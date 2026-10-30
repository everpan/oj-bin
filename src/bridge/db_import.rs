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
    Done { rows: u64, bytes: u64 },
    Failed { rows: u64, bytes: u64, error: String },
    Cancelled { rows: u64, bytes: u64 },
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
                e.state != ImportState::Running
                    && e.finished_at_ms.is_some_and(|f| f <= cutoff)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in &expired {
            g.remove(id);
        }
        expired
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
        Bad { line: u64, reason: String, raw: String },
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
    fn read_record(src: &[u8], mut pos: usize) -> Result<(&[u8], usize), ()> {
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
                        p.pos += rec.len() + phys; // 越过硬换行（pos 指向 \n，+phys 到下一行）
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
            self.pos += rec.len() + phys;
            self.line += phys as u64;
            let rec = rec.strip_suffix(b"\r").unwrap_or(rec);
            let cells = match split_record(rec) {
                Some(c) => c,
                None => {
                    return Some(Err(ParseError::Bad {
                        line: start_line,
                        reason: "unterminated quoted field".into(),
                        raw: String::from_utf8_lossy(rec).into_owned(),
                    }));
                }
            };
            if cells.len() != self.columns.len() {
                return Some(Err(ParseError::Bad {
                    line: start_line,
                    reason: format!(
                        "expected {} columns, got {}",
                        self.columns.len(),
                        cells.len()
                    ),
                    raw: String::from_utf8_lossy(rec).into_owned(),
                }));
            }
            let mut values = Vec::with_capacity(cells.len());
            for (i, cell) in cells.iter().enumerate() {
                match coerce(cell, self.col_types[i]) {
                    Ok(v) => values.push(v),
                    Err(reason) => {
                        return Some(Err(ParseError::Bad {
                            line: start_line,
                            reason: format!("column '{}': {reason}", self.columns[i]),
                            raw: cell.clone(),
                        }));
                    }
                }
            }
            Some(Ok(ParsedRow {
                values,
                line: start_line,
            }))
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
                let v: Value = match serde_json::from_str(trimmed) {
                    Ok(v) => v,
                    Err(e) => {
                        return Some(Err(ParseError::Bad {
                            line: start_line,
                            reason: format!("invalid json: {e}"),
                            raw: trimmed.to_string(),
                        }));
                    }
                };
                let obj = match v.as_object() {
                    Some(o) => o,
                    None => {
                        return Some(Err(ParseError::Bad {
                            line: start_line,
                            reason: "line must be a json object".into(),
                            raw: trimmed.to_string(),
                        }));
                    }
                };
                for k in obj.keys() {
                    if !self.columns.iter().any(|c| c == k) {
                        return Some(Err(ParseError::Bad {
                            line: start_line,
                            reason: format!("unexpected key '{k}'"),
                            raw: trimmed.to_string(),
                        }));
                    }
                }
                let values = self
                    .columns
                    .iter()
                    .map(|c| obj.get(c).cloned().unwrap_or(Value::Null))
                    .collect();
                return Some(Ok(ParsedRow {
                    values,
                    line: start_line,
                }));
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
            assert_eq!(coerce("true", ColumnType::Boolean).unwrap(), Value::Bool(true));
            assert_eq!(coerce("1", ColumnType::Boolean).unwrap(), Value::Bool(true));
            assert_eq!(coerce("false", ColumnType::Boolean).unwrap(), Value::Bool(false));
            assert_eq!(coerce("0", ColumnType::Boolean).unwrap(), Value::Bool(false));
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
        let e = reg.register(owner("u3"), "a".into(), "t".into()).unwrap_err();
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
        let e = reg.register(owner("u1"), "a".into(), "t".into()).unwrap_err();
        assert_eq!(e, RegisterError::SubjectFull);
        assert!(e.message().contains("per-subject"));
    }

    #[test]
    fn import_status_cancel_sweep_roundtrip() {
        let reg = ImportRegistry::new(&section(4, 2));
        let (id, _) = reg.register(owner("u1"), "main".into(), "users".into()).unwrap();
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
        reg.finish(&id, Finish::Cancelled { rows: 10, bytes: 2048 });
        let v = reg.status(&id, &owner("u1")).unwrap();
        assert_eq!(v["state"], "cancelled");
        assert!(v["finishedAtMs"].is_u64());
        // 终态幂等：先到者赢（cancel 后再 finish Done 不翻盘）
        reg.finish(&id, Finish::Done { rows: 99, bytes: 99 });
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
        reg.finish(&id, Finish::Failed { rows: 0, bytes: 0, error: "x".into() });
        reg.push_sample(&id, ImportSample { line: 99, reason: "late".into(), raw: None });
        let v = reg.status(&id, &owner("u1")).unwrap();
        assert_eq!(v["errors"], 30);
    }
}
