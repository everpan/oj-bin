//! TaskPool（PRD v2 §6.2/6.3/6.4）：池化长任务执行体 + 任务注册表 + 执行事件日志 +
//! crontab 解析。模式与 frame_pool 同构：W 个 Worker 线程（每线程一个 Bridge +
//! current_thread runtime），任务模块预载一次（setup 已执行，模块作用域 = ctx），
//! Worker 内本地轮转交替执行 loop_body；cron/run-once 经一次性作业队列顺路执行。
//! 控制面（启停/重载）只翻注册表 desired 位，Worker 每轮自愈对齐——无独立命令总线
//! （ponytail：状态机权威在 Rust 侧注册表，事件池不追消息级可靠性）。

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::{Bridge, TaskSession};

// ---------- 执行事件（薄信封，PRD v2 §7.3：4 字段，serde 可扩展） ----------

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")] // FR-EB-005：契约字段 eventId/eventType（camelCase）
pub struct TaskEvent {
    pub event_id: String,
    pub event_type: String,
    pub timestamp: String,
    pub payload: serde_json::Value,
}

impl TaskEvent {
    pub fn new(seq: u64, event_type: impl Into<String>, payload: serde_json::Value) -> Self {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        Self {
            event_id: format!("evt-{ts}-{seq}"),
            event_type: event_type.into(),
            timestamp: format!("{ts}"),
            payload,
        }
    }
}

/// 事件日志：JSONL 落盘 + 内存环形缓冲（查询 API 直读，免文件 IO）。
/// 滚动：超 max_mb 时覆写 `.1`（ponytail：两文件一刀切，够运维 tail 用）。
pub struct TaskEventLog {
    path: PathBuf,
    max_bytes: u64,
    file: Mutex<Option<std::io::BufWriter<std::fs::File>>>,
    recent: Mutex<VecDeque<serde_json::Value>>,
    seq: AtomicU64,
    /// 当前文件字节数的内存计数（perf F16）：替代每条事件的 metadata stat。
    size: AtomicU64,
}

const RECENT_CAP: usize = 1000;

impl TaskEventLog {
    pub fn new(enabled: bool, path: impl Into<PathBuf>, max_mb: u64) -> std::io::Result<Self> {
        let path = path.into();
        let file = if enabled {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            // 滚动：超限时旧档覆写为 .1。
            if max_mb > 0
                && std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > max_mb * 1024 * 1024
            {
                let _ = std::fs::rename(&path, path.with_extension("jsonl.1"));
            }
            Some(std::io::BufWriter::new(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)?,
            ))
        } else {
            None
        };
        Ok(Self {
            size: AtomicU64::new(if enabled {
                std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0)
            } else {
                0
            }),
            path,
            max_bytes: max_mb * 1024 * 1024,
            file: Mutex::new(file),
            recent: Mutex::new(VecDeque::new()),
            seq: AtomicU64::new(0),
        })
    }

    /// disabled 便捷构造（不落盘，仅内存环）。
    pub fn memory_only() -> Self {
        Self {
            path: PathBuf::new(),
            max_bytes: 0,
            file: Mutex::new(None),
            recent: Mutex::new(VecDeque::new()),
            seq: AtomicU64::new(0),
            size: AtomicU64::new(0),
        }
    }

    /// 记录一条执行事件（topic 入 payload 层，信封保持 4 字段）。
    pub fn record(&self, topic: &str, event_type: &str, payload: serde_json::Value) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let event = TaskEvent::new(seq, event_type, payload);
        let line = serde_json::json!({ "topic": topic, "event": event });
        let mut recent = self.recent.lock().unwrap_or_else(|e| e.into_inner());
        if recent.len() >= RECENT_CAP {
            recent.pop_front();
        }
        recent.push_back(line.clone());
        drop(recent);
        if let Some(f) = self.file.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            use std::io::Write;
            // 滚动检查（append 前；内存计数——perf F16：此前每条事件一次 metadata stat）。
            if self.max_bytes > 0 && self.size.load(Ordering::Relaxed) > self.max_bytes {
                let _ = f.flush();
                let _ = std::fs::rename(&self.path, self.path.with_extension("jsonl.1"));
                if let Ok(nf) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.path)
                {
                    *f = std::io::BufWriter::new(nf);
                }
                self.size.store(0, Ordering::Relaxed);
            }
            let _ = f.write_all(line.to_string().as_bytes());
            let _ = f.write_all(b"\n");
            // 不逐条 flush（perf F16）：BufWriter 满则自然刷盘，事件是诊断数据，
            // 崩溃丢最后几条可接受；零碎 write 全量直达 kernel 纯属白付。
            self.size
                .fetch_add(line.to_string().len() as u64 + 1, Ordering::Relaxed);
        }
    }

    /// 内存环快照（查询 API 用；limit 0 = 全部）。
    pub fn recent(&self, limit: usize) -> Vec<serde_json::Value> {
        let recent = self.recent.lock().unwrap_or_else(|e| e.into_inner());
        let iter: Box<dyn Iterator<Item = &serde_json::Value>> = if limit > 0 {
            Box::new(
                recent
                    .iter()
                    .rev()
                    .take(limit)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev(),
            )
        } else {
            Box::new(recent.iter())
        };
        iter.cloned().collect()
    }
}

// ---------- 任务注册表（内存状态机，权威） ----------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskKind {
    Long,
    Cron {
        expr: CronExpr,
        next_run: Option<SystemTime>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Pending,
    Running,
    Stopped,
    Failed,
}

#[derive(Debug, Clone)]
pub struct TaskEntry {
    pub name: String,
    pub path: PathBuf,
    pub kind: TaskKind,
    /// desired：true = 应处于运行（Worker 每轮对齐连接/断开）。
    pub enabled: bool,
    pub status: TaskStatus,
    pub last_error: Option<String>,
    pub run_count: u64,
    pub started_at: Option<SystemTime>,
    pub finished_at: Option<SystemTime>,
    /// 重载请求（Worker 消费：teardown → 重连）。
    pub reload_pending: bool,
}

impl TaskEntry {
    pub fn long(name: impl Into<String>, path: PathBuf) -> Self {
        Self {
            name: name.into(),
            path,
            kind: TaskKind::Long,
            enabled: true,
            status: TaskStatus::Pending,
            last_error: None,
            run_count: 0,
            started_at: None,
            finished_at: None,
            reload_pending: false,
        }
    }

    pub fn cron(name: impl Into<String>, path: PathBuf, expr: CronExpr) -> Self {
        let next_run = expr.next_after(SystemTime::now());
        Self {
            kind: TaskKind::Cron { expr, next_run },
            ..Self::long(name, path)
        }
    }
}

/// 任务注册表：BTreeMap 保序（API 列表稳定）。long 任务按名静态分配到 Worker；
/// cron 任务不驻留 Worker，由调度器到点投一次性作业。
pub struct TaskRegistry {
    map: Mutex<BTreeMap<String, TaskEntry>>,
    pub log: TaskEventLog,
}

impl TaskRegistry {
    pub fn new(log: TaskEventLog) -> Arc<Self> {
        Arc::new(Self {
            map: Mutex::new(BTreeMap::new()),
            log,
        })
    }

    pub fn upsert(&self, entry: TaskEntry) {
        self.map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(entry.name.clone(), entry);
    }

    pub fn remove(&self, name: &str) -> Option<TaskEntry> {
        self.map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name)
    }

    pub fn get(&self, name: &str) -> Option<TaskEntry> {
        self.map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .cloned()
    }

    /// worker 对齐循环的轻量快照（perf F15）：只取循环消费的 (want, status, path)。
    /// 循环每 5ms 对每个 assigned 任务跑一轮，此前每轮全量 clone TaskEntry
    /// （name/path/cron/时间戳/last_error），实际只读 enabled 与 status 两字段。
    pub fn snapshot(&self, name: &str) -> Option<(bool, TaskStatus, PathBuf)> {
        let g = self.map.lock().unwrap_or_else(|e| e.into_inner());
        let e = g.get(name)?;
        Some((
            e.enabled && !matches!(e.status, TaskStatus::Failed),
            e.status,
            e.path.clone(),
        ))
    }

    pub fn list(&self) -> Vec<TaskEntry> {
        self.map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect()
    }

    /// 启用/停用（desired 位；状态由 Worker 对齐时转换——停用不立即改状态，
    /// 否则状态先于 teardown 落定，违反 FR-LT-005 的时序语义）。
    pub fn set_enabled(&self, name: &str, enabled: bool) -> bool {
        let mut g = self.map.lock().unwrap_or_else(|e| e.into_inner());
        match g.get_mut(name) {
            Some(e) => {
                e.enabled = enabled;
                true
            }
            None => false,
        }
    }

    /// 启动命令：enabled = true；Failed 任务清错复位 Pending（Worker 下轮重连）。
    pub fn start(&self, name: &str) -> bool {
        let mut g = self.map.lock().unwrap_or_else(|e| e.into_inner());
        match g.get_mut(name) {
            Some(e) => {
                e.enabled = true;
                if e.status == TaskStatus::Failed {
                    e.status = TaskStatus::Pending;
                    e.last_error = None;
                }
                true
            }
            None => false,
        }
    }

    /// 修改 cron 表达式（PATCH cron 字段）并重算 next_run。
    pub fn set_cron_expr(&self, name: &str, expr: CronExpr) -> bool {
        let mut g = self.map.lock().unwrap_or_else(|e| e.into_inner());
        match g.get_mut(name) {
            Some(TaskEntry {
                kind: TaskKind::Cron { expr: e, next_run },
                ..
            }) => {
                *next_run = expr.next_after(SystemTime::now());
                *e = expr;
                true
            }
            _ => false,
        }
    }

    pub fn request_reload(&self, name: &str) -> bool {
        let mut g = self.map.lock().unwrap_or_else(|e| e.into_inner());
        match g.get_mut(name) {
            Some(e) => {
                e.reload_pending = true;
                true
            }
            None => false,
        }
    }

    /// 更新 cron 的下次执行时间（调度器到点后调用）。
    pub fn set_next_run(&self, name: &str, next: Option<SystemTime>) -> bool {
        let mut g = self.map.lock().unwrap_or_else(|e| e.into_inner());
        match g.get_mut(name) {
            Some(TaskEntry {
                kind: TaskKind::Cron { next_run, .. },
                ..
            }) => {
                *next_run = next;
                true
            }
            _ => false,
        }
    }

    pub fn set_status(&self, name: &str, status: TaskStatus, err: Option<String>) {
        let mut g = self.map.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(e) = g.get_mut(name) {
            e.status = status;
            if let Some(err) = err {
                e.last_error = Some(err);
            }
            match status {
                TaskStatus::Running => e.started_at = Some(SystemTime::now()),
                TaskStatus::Stopped | TaskStatus::Failed => e.finished_at = Some(SystemTime::now()),
                _ => {}
            }
        }
    }

    pub fn bump_run(&self, name: &str) {
        let mut g = self.map.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(e) = g.get_mut(name) {
            e.run_count += 1;
        }
    }

    pub fn take_reload(&self, name: &str) -> bool {
        let mut g = self.map.lock().unwrap_or_else(|e| e.into_inner());
        match g.get_mut(name) {
            Some(e) => std::mem::take(&mut e.reload_pending),
            None => false,
        }
    }
}

// ---------- crontab（PRD v2 §6.4：标准 5 段，宏/秒字段显式报错） ----------

/// 5 字段 cron 表达式（分 时 日 月 周）。支持 `*`、`*/n`、列表 `a,b,c`、
/// 区间 `a-b`（周字段 0-7，0 与 7 均 = 周日）。逐分钟步进求 next（上限 4 年）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronExpr {
    pub min: Field,
    pub hour: Field,
    pub dom: Field,
    pub mon: Field,
    pub dow: Field,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    bits: u64, // 位图：bit i = 值 i 命中（分钟/小时 6 位足够，统一 64 位省心）
}

impl Field {
    pub fn matches(&self, v: u64) -> bool {
        v < 64 && (self.bits >> v) & 1 == 1
    }

    fn parse(spec: &str, lo: u64, hi: u64, dow: bool) -> Result<Self, String> {
        let mut bits = 0u64;
        for part in spec.split(',') {
            let (range, step) = match part.split_once('/') {
                Some((r, s)) => {
                    let s: u64 = s.parse().map_err(|_| format!("cron: bad step '{part}'"))?;
                    if s == 0 {
                        return Err(format!("cron: step 0 in '{part}'"));
                    }
                    (r, s)
                }
                None => (part, 1),
            };
            let (mut a, mut b) = match range {
                "*" => (lo, hi),
                _ => match range.split_once('-') {
                    Some((x, y)) => {
                        let x: u64 = x
                            .trim()
                            .parse()
                            .map_err(|_| format!("cron: bad range '{part}'"))?;
                        let y: u64 = y
                            .trim()
                            .parse()
                            .map_err(|_| format!("cron: bad range '{part}'"))?;
                        (x, y)
                    }
                    None => {
                        let v: u64 = range
                            .trim()
                            .parse()
                            .map_err(|_| format!("cron: bad value '{part}'"))?;
                        (v, v)
                    }
                },
            };
            if dow {
                // 周日归一：0 与 7 均为周日 → 统一为 0。
                if a == 7 {
                    a = 0;
                }
                if b == 7 {
                    b = 0;
                }
            }
            if a > b || b > hi || (a < lo && !(dow && a == 0)) {
                return Err(format!("cron: '{part}' out of range {lo}-{hi}"));
            }
            let mut v = a;
            while v <= b {
                if v < 64 {
                    bits |= 1 << v;
                }
                v += step;
            }
        }
        if bits == 0 {
            return Err(format!("cron: empty field '{spec}'"));
        }
        Ok(Self { bits })
    }
}

impl CronExpr {
    pub fn parse(s: &str) -> Result<Self, String> {
        let f: Vec<&str> = s.split_whitespace().collect();
        if f.len() != 5 {
            return Err(format!(
                "cron: expected 5 fields (min hour dom mon dow), got {}: '{s}'",
                f.len()
            ));
        }
        if s.contains('@') {
            return Err("cron: macros (@daily etc.) not supported, use 5-field form".into());
        }
        Ok(Self {
            min: Field::parse(f[0], 0, 59, false)?,
            hour: Field::parse(f[1], 0, 23, false)?,
            dom: Field::parse(f[2], 1, 31, false)?,
            mon: Field::parse(f[3], 1, 12, false)?,
            dow: Field::parse(f[4], 0, 7, true)?,
        })
    }

    /// 逐分钟步进求下一触发点（严格晚于 `from`）。上限 4 年（含 2-29）。
    pub fn next_after(&self, from: SystemTime) -> Option<SystemTime> {
        let secs = from.duration_since(UNIX_EPOCH).ok()?.as_secs();
        // 对齐到下一分钟起点。
        let mut t = (secs / 60 + 1) * 60;
        let cap = secs + 4 * 366 * 24 * 3600;
        while t <= cap {
            let (mi, h, dom, mon, dow) = Self::civil(t);
            if self.min.matches(mi)
                && self.hour.matches(h)
                && self.mon.matches(mon)
                && (self.dom.matches(dom) || self.dow.matches(dow))
            {
                return Some(UNIX_EPOCH + Duration::from_secs(t));
            }
            t += 60;
        }
        None
    }

    /// 秒级 Unix 时间 → (分, 时, 日, 月, 周)（UTC；Howard Hinnant 公历算法）。
    fn civil(t: u64) -> (u64, u64, u64, u64, u64) {
        let days = t / 86400;
        let secs_of_day = t % 86400;
        // 1970-01-01 = 周四（dow 4）。
        let dow = (days + 4) % 7;
        let z = days as i64 + 719468;
        let era = if z >= 0 { z } else { z - 146096 } / 146097;
        let doe = z - era * 146097;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        (
            secs_of_day / 60 % 60,
            secs_of_day / 3600,
            d as u64,
            m as u64,
            dow,
        )
    }
}

/// 解析 crontab.yaml 一行：前 5 个空白分隔 token 为表达式，第 6 个为脚本路径。
pub fn parse_crontab_line(line: &str) -> Result<(CronExpr, String), String> {
    // `#` 之后一律视为注释（行首/行尾同规则，对齐 crontab 惯例）。
    let line = line.split('#').next().unwrap_or(line).trim();
    if line.is_empty() {
        return Err("empty/comment".into());
    }
    let mut it = line.split_whitespace();
    let mut expr_parts = Vec::new();
    for _ in 0..5 {
        expr_parts.push(
            it.next()
                .ok_or_else(|| format!("crontab: bad line '{line}'"))?,
        );
    }
    let path = it
        .next()
        .ok_or_else(|| format!("crontab: missing script path '{line}'"))?;
    if it.next().is_some() {
        return Err(format!("crontab: extra fields '{line}'"));
    }
    Ok((CronExpr::parse(&expr_parts.join(" "))?, path.to_string()))
}

// ---------- TaskPool ----------

/// 一次性作业（cron 到点 / run-once API）：Worker 顺路执行，不驻留会话。
#[derive(Debug, Clone)]
pub struct OnceJob {
    pub name: String,
    pub path: PathBuf,
}

/// 池化任务执行体：W 个 Worker 线程，long 任务静态轮询分配，本地轮转 loop_body；
/// cron 到点与 run-once 经 std mpsc 作业队列派发。控制面只翻注册表 desired 位。
pub struct TaskPool {
    make: Arc<dyn Fn() -> Bridge + Send + Sync>,
    timeout: Duration,
    /// 轮间节奏：忙扫每轮后 sleep 这么久再下一轮（防 trivial loop_body 空转独占 Worker）。
    interval: Duration,
    workers: usize,
    pub registry: Arc<TaskRegistry>,
    jobs_tx: std::sync::mpsc::Sender<OnceJob>,
    jobs_rx: Mutex<std::sync::mpsc::Receiver<OnceJob>>,
    shutdown: Arc<AtomicBool>,
    handles: Mutex<Vec<std::thread::JoinHandle<()>>>,
    /// 存活 Worker 数（panic 补员自愈的观测量）。
    live: AtomicUsize,
}

impl TaskPool {
    pub fn new(
        workers: usize,
        timeout: Duration,
        interval: Duration,
        make: Arc<dyn Fn() -> Bridge + Send + Sync>,
        registry: Arc<TaskRegistry>,
    ) -> Arc<Self> {
        let (jobs_tx, jobs_rx) = std::sync::mpsc::channel();
        Arc::new(Self {
            make,
            timeout,
            // 0 = 不限制（立即轮转下一圈，压测语义；生产会被 trivial loop_body 空转打满）。
            interval,
            workers: workers.max(1),
            registry,
            jobs_tx,
            jobs_rx: Mutex::new(jobs_rx),
            shutdown: Arc::new(AtomicBool::new(false)),
            handles: Mutex::new(Vec::new()),
            live: AtomicUsize::new(0),
        })
    }

    pub fn spawn(self: &Arc<Self>) {
        let longs: Vec<String> = self
            .registry
            .list()
            .into_iter()
            .filter(|e| matches!(e.kind, TaskKind::Long))
            .map(|e| e.name)
            .collect();
        for w in 0..self.workers {
            let mine: Vec<String> = longs
                .iter()
                .enumerate()
                .filter(|(i, _)| i % self.workers == w)
                .map(|(_, n)| n.clone())
                .collect();
            self.spawn_worker(w, mine);
        }
    }

    /// 提交一次性作业（cron 调度器 / run-once API）。满队列（边界为 0 容量不可能的
    /// 无界通道——背压由注册表状态兜底：重复触发同一任务在上次未完成前拒绝）直接入队。
    pub fn submit_once(&self, job: OnceJob) {
        let _ = self.jobs_tx.send(job);
    }

    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }

    /// 存活 Worker 数（panic 补员自愈的观测量）。
    pub fn live_workers(&self) -> usize {
        self.live.load(Ordering::SeqCst)
    }

    /// 停机收场（PRD v2 FR-LT-005）：置位 → join 全部 Worker（Worker 退出前
    /// 已对每个会话尽力 teardown）。阻塞调用，调用方宜在 blocking 上下文。
    /// 补员与停机并发时，垂死 Worker 对替补句柄的 push 可晚于下方 take（替补
    /// Worker 跑 make 早于句柄入册）——循环到 live 归零为止；stopping 置位后
    /// 补员有界（再补前会看到 stopping），必然收敛。
    pub fn shutdown_and_join(&self) {
        self.shutdown();
        loop {
            let handles: Vec<_> =
                std::mem::take(&mut *self.handles.lock().unwrap_or_else(|e| e.into_inner()));
            for h in handles {
                let _ = h.join();
            }
            if self.live.load(Ordering::SeqCst) == 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn spawn_worker(self: &Arc<Self>, worker_id: usize, assigned: Vec<String>) {
        let pool = Arc::clone(self);
        self.live.fetch_add(1, Ordering::SeqCst);
        let h = std::thread::Builder::new()
            .name(format!("task-worker-{worker_id}"))
            .spawn(move || {
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("task worker rt");
                    rt.block_on(pool.clone().worker_main(worker_id, assigned.clone()))
                }));
                pool.live.fetch_sub(1, Ordering::SeqCst);
                if let Err(e) = r {
                    eprintln!("task worker {worker_id} panicked: {e:?}");
                    // 补员（对照 frame_pool）：panic Worker 带走一个坑位，原参重起
                    // 保持总量；停机中不补（Worker 自然退出即收场）。
                    if !pool.shutdown.load(Ordering::SeqCst) {
                        pool.spawn_worker(worker_id, assigned);
                    }
                }
            })
            .expect("spawn task-worker");
        self.handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(h);
    }

    async fn worker_main(self: Arc<Self>, worker_id: usize, assigned: Vec<String>) {
        let bridge = (self.make)();
        // name → 已连接会话（模块作用域 = ctx，setup 已在 task_connect 执行）。
        let mut sessions: BTreeMap<String, TaskSession> = BTreeMap::new();
        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                break;
            }
            // 1) 顺路清一次性作业队列。
            loop {
                let job = {
                    let rx = self.jobs_rx.lock().unwrap_or_else(|e| e.into_inner());
                    rx.try_recv().ok()
                };
                let Some(job) = job else { break };
                // stopping 后 run_once 内部拒绝（排队作业快速出队，不阻塞停机）。
                let _ = self.run_once(&bridge, &job).await;
            }
            // 2) long 任务对齐 + 轮转。
            let mut idle = true;
            for name in &assigned {
                let (want, status, path) = match self.registry.snapshot(name) {
                    Some(v) => v,
                    None => continue,
                };
                let connected = sessions.contains_key(name);
                if self.registry.take_reload(name) && connected {
                    self.teardown(&mut sessions, name, "reload").await;
                }
                if want && !connected {
                    match bridge.task_connect(&path).await {
                        Ok(sess) => {
                            sessions.insert(name.clone(), sess);
                            self.registry.set_status(name, TaskStatus::Running, None);
                            self.registry.log.record(
                                "tasks.execution",
                                "task.started",
                                serde_json::json!({ "task": name, "worker": worker_id }),
                            );
                        }
                        Err(e) => {
                            self.registry
                                .set_status(name, TaskStatus::Failed, Some(e.to_string()));
                            self.registry.log.record(
                                "tasks.execution",
                                "task.start_failed",
                                serde_json::json!({ "task": name, "error": e.to_string() }),
                            );
                        }
                    }
                    continue;
                }
                if !want && connected {
                    self.teardown(&mut sessions, name, "stop").await;
                    continue;
                }
                if !want
                    && !connected
                    && matches!(status, TaskStatus::Running | TaskStatus::Pending)
                {
                    // 未连接任务的停用：状态直接落 Stopped（无 teardown 义务）。
                    self.registry.set_status(name, TaskStatus::Stopped, None);
                }
                if want && connected {
                    idle = false;
                    let mut sess = sessions.remove(name).expect("connected");
                    match sess.fire("loop_body", self.timeout).await {
                        Ok(()) => {
                            self.registry.bump_run(name);
                            sessions.insert(name.clone(), sess);
                        }
                        Err(super::RunError::Core(e)) => {
                            // 异常路径：teardown 保证执行（FR-LT-005），任务标记 Failed。
                            if let Err(te) = sess.fire("teardown", self.timeout).await {
                                eprintln!("task {name} teardown after error failed: {te}");
                            }
                            self.registry
                                .set_status(name, TaskStatus::Failed, Some(e.to_string()));
                            self.registry.log.record(
                                "tasks.execution",
                                "task.failed",
                                serde_json::json!({ "task": name, "error": e.to_string() }),
                            );
                        }
                        Err(super::RunError::Oom { limit }) => {
                            // PR-4：堆超限 = runtime 已 terminate 毒化（同 Timeout 处置）。
                            drop(sess);
                            self.registry.set_status(
                                name,
                                TaskStatus::Failed,
                                Some(format!(
                                    "loop_body js heap limit exceeded (limit {limit} bytes; server.js_heap_limit_bytes)"
                                )),
                            );
                            self.registry.log.record(
                                "tasks.execution",
                                "task.timeout",
                                serde_json::json!({ "task": name, "error": "js heap limit exceeded" }),
                            );
                        }
                        Err(super::RunError::Timeout) => {
                            // 毒化：runtime 已 terminate，teardown 不承诺（FR-LT-005）。
                            drop(sess);
                            self.registry.set_status(
                                name,
                                TaskStatus::Failed,
                                Some(format!(
                                    "loop_body timeout ({}ms)",
                                    self.timeout.as_millis()
                                )),
                            );
                            self.registry.log.record(
                                "tasks.execution",
                                "task.timeout",
                                serde_json::json!({ "task": name, "worker": worker_id }),
                            );
                        }
                    }
                }
            }
            if idle {
                tokio::time::sleep(Duration::from_millis(5)).await;
            } else if self.interval.is_zero() {
                // interval_ms = 0：不限制，忙扫结束立即轮转（压测语义）。
                tokio::task::yield_now().await;
            } else {
                tokio::time::sleep(self.interval).await;
            }
        }
        // 停机收场：尽力 teardown 全部会话（自然停止路径，FR-LT-005）。
        let names: Vec<String> = sessions.keys().cloned().collect();
        for name in names {
            self.teardown(&mut sessions, &name, "shutdown").await;
        }
    }

    async fn teardown(
        self: &Arc<Self>,
        sessions: &mut BTreeMap<String, TaskSession>,
        name: &str,
        reason: &str,
    ) {
        if let Some(mut sess) = sessions.remove(name)
            && let Err(e) = sess.fire("teardown", self.timeout).await
        {
            eprintln!("task {name} teardown ({reason}) failed: {e}");
        }
        self.registry.set_status(name, TaskStatus::Stopped, None);
        self.registry.log.record(
            "tasks.execution",
            "task.stopped",
            serde_json::json!({ "task": name, "reason": reason }),
        );
    }

    /// 一次性作业：整文件执行一次（cron 脚本 = 普通模块，评估完即结束）。
    /// 复用 run_task 管道（flag 永否 → 自然跑完即 Stopped）。
    /// 停机（stopping）置位后拒绝新作业——在途作业由 run_task 的 grace 看门狗
    /// 兜底，shutdown_and_join 经 join 等其收场，不无限期阻塞。
    async fn run_once(self: &Arc<Self>, bridge: &Bridge, job: &OnceJob) -> Result<(), String> {
        if self.shutdown.load(Ordering::SeqCst) {
            return Err(format!(
                "task pool stopping: run_once '{}' rejected",
                job.name
            ));
        }
        self.registry
            .set_status(&job.name, TaskStatus::Running, None);
        self.registry.log.record(
            "tasks.execution",
            "task.execution_started",
            serde_json::json!({ "task": job.name }),
        );
        let flag = Arc::new(AtomicBool::new(false));
        let exit = bridge
            .run_task(&job.path, flag, Duration::from_secs(30))
            .await;
        match exit {
            super::TaskExit::Stopped => {
                self.registry.bump_run(&job.name);
                self.registry
                    .set_status(&job.name, TaskStatus::Stopped, None);
                self.registry.log.record(
                    "tasks.execution",
                    "task.execution_succeeded",
                    serde_json::json!({ "task": job.name }),
                );
            }
            other => {
                self.registry
                    .set_status(&job.name, TaskStatus::Failed, Some(format!("{other:?}")));
                self.registry.log.record(
                    "tasks.execution",
                    "task.execution_failed",
                    serde_json::json!({ "task": job.name, "error": format!("{other:?}") }),
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "task_pool_tests.rs"]
mod tests;
