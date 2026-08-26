//! 服务端日志：文件按天滚动 + 每次启动切换一个新文件（文件名带启动时间到秒），
//! 同时镜像到 stderr。目录默认相对 config 目录的 ./logs，可在 server.logs_dir 配置；
//! 不存在则自动创建。
//!
//! 设计：tracing 全局 subscriber；文件层用 tracing-appender 的 RollingFileAppender
//! （Rotation::DAILY）作 writer；每次启动用启动时间（秒）作文件名前缀，故每次启动得到新文件，
//! 进程跨天再按天滚动。init 幂等，重复调用安全。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
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

/// 初始化全局 tracing subscriber（幂等；失败仅告警，不影响主流程）。
pub fn init(logs_dir: &Path) {
    if INITED.swap(true, Ordering::SeqCst) {
        return;
    }

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    // 文件层（无颜色），每次启动一个新文件，跨天按天滚动；失败则降级为仅 stderr。
    let file_layer: Option<
        Box<dyn tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync>,
    > = match build_file_writer(logs_dir) {
        Ok(w) => {
            let (nb, guard) = tracing_appender::non_blocking(w);
            // 保活 worker 线程：丢弃 guard 使其常驻进程生命周期，避免日志丢失。
            std::mem::forget(guard);
            Some(Box::new(
                tracing_subscriber::fmt::layer()
                    .with_writer(nb)
                    .with_ansi(false)
                    .with_target(false),
            ))
        }
        Err(e) => {
            eprintln!("warn: log file init failed ({e}); logging to stderr only");
            None
        }
    };

    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_ansi(true);

    // 顺序：boxed 文件层（贴 Registry）→ env_filter → 控制台层。
    let registry = tracing_subscriber::registry()
        .with(file_layer)
        .with(env_filter)
        .with(stderr_layer);

    if registry.try_init().is_err() {
        eprintln!("warn: tracing subscriber already initialized; logging unchanged");
    }
}

/// 构造按天滚动的 appender；前缀带启动时间到秒 → 每次启动一个新文件。
fn build_file_writer(logs_dir: &Path) -> std::io::Result<RollingFileAppender> {
    std::fs::create_dir_all(logs_dir)?;
    let prefix = format!("server-{}", chrono::Local::now().format("%Y%m%d-%H%M%S"));
    Ok(RollingFileAppender::new(Rotation::DAILY, logs_dir, prefix))
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
