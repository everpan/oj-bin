//! 服务端日志：**终端输出的完整镜像落盘**。stdout/stderr 各自重定向到独立管道，由
//! 后台线程把读到的每一块同时写回原终端与日志文件 —— 因此 `println!`/`eprintln!`/panic、
//! tracing 控制台层等一切终端输出，与终端所见同形落盘（去除 ANSI 色码）。
//! 每次启动一个新文件，文件名 `server-<启动秒>_<pid>.log`；长时间运行按大小滚动
//! （默认单文件 10MB），含活动文件最多保留 10 个。目录默认相对 config 目录的 ./logs，
//! 可在 server.logs_dir 配置；不存在则自动创建。
//!
//! 设计：tracing 只保留控制台层（其输出经 tee 落盘，不再单设文件层）；镜像线程与
//! fd 按进程生命周期泄漏（对齐 non_blocking guard 的 mem::forget 惯例），不做优雅关闭 ——
//! 进程退出瞬间管道尾部字节可能丢，终端侧始终完整。init 幂等，重复调用安全。
//!
//! 终端输出默认关闭（`server.console_log` / CLI `--console-log` 打开）：关闭时镜像线程
//! 不回写原终端，输出只落盘。注意 tracing 控制台层写的是 **stderr**，故开关同时静默
//! fd 1 与 fd 2；非 unix 平台没有落盘，此时强制保留终端输出并告警（否则日志会彻底消失）。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

static INITED: AtomicBool = AtomicBool::new(false);

/// 解析日志目录：绝对路径原样；相对 → 相对 config_dir；未配置 → config_dir/logs。
pub fn resolve_logs_dir(logs_dir: Option<&str>, config_dir: &Path) -> PathBuf {
    match logs_dir {
        Some(d) => {
            let p = PathBuf::from(d);
            if p.is_absolute() {
                p
            } else {
                config_dir.join(d)
            }
        }
        None => config_dir.join("logs"),
    }
}

/// 初始化全局 tracing subscriber + 终端镜像（幂等；失败仅告警，不影响主流程）。
/// `logs_max_m`：单文件大小上限（单位 M，<100 按 100 生效）；`logs_keep_files`：保留个数。
/// `console`：false → 输出只落盘，终端保持干净（同时静默 stdout 与 stderr，因为
/// tracing 控制台层写的是 stderr）。非 unix 平台无落盘，此时该参数被忽略。
pub fn init(logs_dir: &Path, logs_max_m: u64, logs_keep_files: usize, console: bool) {
    if INITED.swap(true, Ordering::SeqCst) {
        return;
    }

    // 先装终端镜像：此后一切终端输出同步落盘；失败则降级为仅终端。
    #[cfg(unix)]
    if let Err(e) = install_terminal_tee(
        logs_dir,
        effective_max_bytes(logs_max_m),
        logs_keep_files,
        console,
    ) {
        eprintln!("warn: log file init failed ({e}); logging to terminal only");
    }
    #[cfg(not(unix))]
    {
        // fd 级 dup2 tee 依赖 unix 语义；非 unix 平台不落盘但必须喊出声，不能静默丢日志。
        eprintln!(
            "warn: file logging is unix-only; server.logs_dir/logs_max_m/logs_keep_files are ignored on this platform"
        );
        // 此处无落盘，关终端等于丢日志 —— 强制保留终端输出，并说明原因。
        if !console {
            eprintln!(
                "warn: console output kept: file logging is unix-only, so there is nowhere else to log on this platform"
            );
        }
        let _ = (logs_dir, logs_max_m, logs_keep_files, console);
    }

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    // 控制台层：输出经 tee 镜像进日志文件（落盘时去 ANSI）。
    tracing_subscriber::registry()
        .with(env_filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_target(false)
                .with_ansi(true),
        )
        .try_init()
        .ok(); // INITED 已兜底幂等，重复初始化在此不可达。
}

/// 配置的日志大小上限（M）→ 字节；<100M 钳到 100M（防误配小值导致频繁滚动）。
fn effective_max_bytes(logs_max_m: u64) -> u64 {
    logs_max_m.max(100) * 1024 * 1024
}

/// 终端镜像：把 stdout / stderr 各自重定向到独立管道，由后台线程逐块写回原终端
/// 与日志文件（两条独立管道，避免合并后终端侧 stdout/stderr 目标被重复写）。
/// 两线程共享同一 [`LogWriter`]，滚动由互斥锁串行化。
/// `console`：true → 额外回写原终端（默认关闭，见 `init`）。
///
/// 公开仅为 `server/tests/log_tee.rs` 集成测试：tee 是进程级的，必须独占一个测试
/// 二进制（装在单元测试里会污染同二进制其余用例的输出，见该文件头部说明）。
#[cfg(unix)]
pub fn install_terminal_tee(
    logs_dir: &Path,
    max_bytes: u64,
    keep: usize,
    console: bool,
) -> std::io::Result<()> {
    use std::sync::{Arc, Mutex};

    std::fs::create_dir_all(logs_dir)?;
    // keep 只约束单次运行内的滚动；不清理历史运行则每次重启都新增一套文件，磁盘无限增长
    // （文件名含启动秒 + pid，跨运行互不同名）。启动期按 mtime 删最旧的超出部分。
    // ponytail: 启动期清理一次；多实例共享同一 logs_dir 时按 mtime 保留最新，够用再改。
    prune_old_logs(logs_dir, keep.max(2));
    let base = logs_dir.join(format!(
        "server-{}_{}",
        chrono::Local::now().format("%Y%m%d-%H%M%S"),
        std::process::id()
    ));
    // 静默前先喊一声：此后终端就什么都不显示了，用户必须知道日志去了哪。
    // 这行必须在 redirect_fd 之前打印，否则会被自己的管道吞掉。
    if !console {
        eprintln!("note: console log off; logging to {}.log", base.display());
    }
    // 滚动语义要求活动文件 + 至少一个后移位（keep<2 无法滚动，钳到 2）。
    let writer = Arc::new(Mutex::new(LogWriter::new(base, max_bytes, keep.max(2))));

    // SAFETY: 纯 fd 操作：原始 stdout/stderr 先 dup 保存；各自管道写端替换 fd 1/2；
    // 读端与保存的 fd 全部移交镜像线程，按进程生命周期泄漏。
    unsafe {
        redirect_fd(1, libc::dup(1), writer.clone(), console)?;
        redirect_fd(2, libc::dup(2), writer, console)?;
    }
    Ok(())
}

/// 启动期清理：删掉 logs_dir 里超出 `keep` 的最旧历史日志（`server-*.log`，含滚动件）。
#[cfg(unix)]
fn prune_old_logs(logs_dir: &Path, keep: usize) {
    let mut logs: Vec<(std::time::SystemTime, PathBuf)> = match std::fs::read_dir(logs_dir) {
        Ok(rd) => rd
            .flatten()
            .filter(|e| {
                let n = e.file_name();
                let n = n.to_string_lossy();
                n.starts_with("server-") && n.ends_with(".log")
            })
            .filter_map(|e| {
                let m = e.metadata().ok()?;
                Some((m.modified().ok()?, e.path()))
            })
            .collect(),
        Err(_) => return,
    };
    if logs.len() <= keep {
        return;
    }
    logs.sort_by_key(|(t, _)| *t);
    let excess = logs.len() - keep;
    for (_, p) in logs.into_iter().take(excess) {
        let _ = std::fs::remove_file(p);
    }
}

/// 把 `fd` 的输出重定向到新管道，管道内容由镜像线程写回 `console` 与日志文件。
#[cfg(unix)]
unsafe fn redirect_fd(
    fd: i32,
    console: i32,
    writer: std::sync::Arc<std::sync::Mutex<LogWriter>>,
    echo_console: bool,
) -> std::io::Result<()> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: 纯 fd 操作，见 install_terminal_tee 总注。dup2 生效后任何失败都必须把
    // fd 还原为原终端并关掉读端——否则管道无读者，写端攒满 64K 后进程永久阻塞。
    unsafe {
        if libc::pipe(fds.as_mut_ptr()) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let (read_fd, write_fd) = (fds[0], fds[1]);
        if libc::dup2(write_fd, fd) != fd {
            libc::close(read_fd);
            libc::close(write_fd);
            return Err(std::io::Error::last_os_error());
        }
        libc::close(write_fd);
        match std::thread::Builder::new()
            .name(format!("log-tee-{fd}"))
            .spawn(move || mirror_loop(read_fd, console, writer, echo_console))
        {
            Ok(_) => Ok(()),
            Err(e) => {
                libc::dup2(console, fd); // 还原原终端，进程可继续正常输出
                libc::close(read_fd);
                libc::close(console);
                Err(e)
            }
        }
    }
}

/// 镜像循环：管道 → 原终端（`echo_console` 为 true 时）+ 共享 [`LogWriter`]
/// （去 ANSI、超限滚动）。关终端时仍必须把管道读空，否则写端攒满 64K 后进程永久阻塞。
#[cfg(unix)]
fn mirror_loop(
    read_fd: i32,
    console: i32,
    writer: std::sync::Arc<std::sync::Mutex<LogWriter>>,
    echo_console: bool,
) {
    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::unix::io::FromRawFd;

    let mut reader = unsafe { File::from_raw_fd(read_fd) };
    let mut console_w = unsafe { File::from_raw_fd(console) };
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if echo_console {
                    let _ = console_w.write_all(&buf[..n]);
                }
                if let Ok(mut w) = writer.lock() {
                    w.write(&buf[..n]);
                }
            }
        }
    }
}

/// 共享日志写入器：活动文件 `base.log`，超限滚动为 `base.1.log`、`base.2.log`…依次后移，
/// 超出保留数删除。stdout/stderr 两镜像线程经同一把锁写入。
#[cfg(unix)]
struct LogWriter {
    base: PathBuf,
    file: Option<std::fs::File>,
    size: u64,
    max_bytes: u64,
    keep: usize,
    /// ANSI 剥离状态机；跨块保持——转义序列被读块边界切开时也能正确剥离。
    ansi: AnsiStripper,
}

#[cfg(unix)]
impl LogWriter {
    fn new(base: PathBuf, max_bytes: u64, keep: usize) -> Self {
        Self {
            base,
            file: None,
            size: 0,
            max_bytes,
            keep,
            ansi: AnsiStripper::new(),
        }
    }

    fn write(&mut self, chunk: &[u8]) {
        use std::io::Write;
        if self.file.is_none() && self.open().is_err() {
            return;
        }
        if self.size >= self.max_bytes {
            self.rotate();
        }
        let clean = self.ansi.strip(chunk);
        if let Some(f) = self.file.as_mut()
            && f.write_all(&clean).is_ok()
        {
            self.size += clean.len() as u64;
        }
    }

    fn open(&mut self) -> std::io::Result<()> {
        let f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.active_path())?;
        self.size = f.metadata().map(|m| m.len()).unwrap_or(0);
        self.file = Some(f);
        Ok(())
    }

    /// 平移 `base.(k-1).log → base.k.log` 后把活动文件让位为 `base.1.log`，重开句柄。
    fn rotate(&mut self) {
        self.file = None;
        for k in (1..self.keep.saturating_sub(1) as u32).rev() {
            let _ = std::fs::rename(self.rotated(k), self.rotated(k + 1));
        }
        let _ = std::fs::rename(self.active_path(), self.rotated(1));
        let _ = self.open();
    }

    fn active_path(&self) -> PathBuf {
        PathBuf::from(format!("{}.log", self.base.display()))
    }

    fn rotated(&self, k: u32) -> PathBuf {
        PathBuf::from(format!("{}.{}.log", self.base.display(), k))
    }
}

/// 去掉 ANSI CSI 转义序列（ESC '[' 参数字节 终止符 0x40-0x7E），日志文件保持纯文本可 grep。
/// 状态跨块保持：管道 read 的 8KB 边界可能把一条转义序列切成两半。
// ponytail: 只处理 CSI（颜色/光标）；非 CSI 的 ESC 序列极少出现在服务输出，遇到再扩。
#[cfg(unix)]
struct AnsiStripper {
    state: u8, // 0=文本 1=见 ESC 2=CSI 参数中
}

#[cfg(unix)]
impl AnsiStripper {
    fn new() -> Self {
        Self { state: 0 }
    }

    fn strip(&mut self, buf: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(buf.len());
        for &b in buf {
            match self.state {
                0 => {
                    if b == 0x1b {
                        self.state = 1;
                    } else {
                        out.push(b);
                    }
                }
                1 => self.state = if b == b'[' { 2 } else { 0 },
                _ => {
                    if (0x40..=0x7e).contains(&b) {
                        self.state = 0;
                    }
                }
            }
        }
        out
    }
}

/// axum 请求日志中间件：记录 method / path / status / 耗时（info 级）。
pub async fn log_requests(req: Request, next: Next) -> Response {
    let start = Instant::now();
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let resp = next.run(req).await;
    let status = resp.status();
    tracing::info!(
        method = %method,
        path = %path,
        status = status.as_u16(),
        ms = start.elapsed().as_millis(),
        "request"
    );
    resp
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn effective_max_bytes_floors_at_100m() {
        assert_eq!(effective_max_bytes(0), 100 * 1024 * 1024);
        assert_eq!(effective_max_bytes(99), 100 * 1024 * 1024);
        assert_eq!(effective_max_bytes(100), 100 * 1024 * 1024);
        assert_eq!(effective_max_bytes(256), 256 * 1024 * 1024);
    }

    #[test]
    fn log_writer_rotates_by_size_and_caps_files() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("server-test");
        let mut w = LogWriter::new(base.clone(), 64, 3);
        for i in 0..40u32 {
            w.write(format!("chunk-{i:02}-0123456789\n").as_bytes());
        }
        // keep=3：活动 + .1 + .2，.3 及更早已删除。
        let mut names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names.len(), 3, "{names:?}");
        assert!(names.contains(&"server-test.log".to_string()), "{names:?}");
        assert!(
            names.contains(&"server-test.1.log".to_string()),
            "{names:?}"
        );
        assert!(
            names.contains(&"server-test.2.log".to_string()),
            "{names:?}"
        );
        // 活动文件可继续追加。
        w.write(b"after-rotate\n");
        assert!(read_all(dir.path()).contains("after-rotate"));
    }

    fn read_all(dir: &Path) -> String {
        let mut all = String::new();
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            all.push_str(&std::fs::read_to_string(entry.path()).unwrap_or_default());
        }
        all
    }
}
