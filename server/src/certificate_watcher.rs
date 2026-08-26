//! 证书热加载：用 `notify` 监听公钥/证书文件变更，原子更新共享状态。
//!
//! 与 mtime 轮询不同，这里依赖 OS 文件事件（Linux inotify / macOS FSEvents / Windows
//! ReadDirectoryChangesW），文件被覆盖即触发重载，无轮询空转。
//!
//! 注意：重载到 `Expired` 不会中止服务（spec §6）——仅切换状态使后续 GET 返回 403，
//! 让运维有时间替换证书；启动期已进入 `Expired` 才由 `app.rs` 中止进程。

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::SystemTime;

use only_js::config::ServerCfg;
use notify::{Event, RecursiveMode, Watcher};

use crate::certificate::{load_certificate_at, CertificateStatus};

/// 派生证书状态类型别名，避免调用处重复书写。
pub type SharedCertStatus = Arc<RwLock<CertificateStatus>>;
pub type SharedCertValidUntil = Arc<RwLock<Option<SystemTime>>>;

/// 重新加载证书并原子更新共享状态（热加载核心逻辑，抽离以便单测）。
///
/// 加载失败（格式/签名错误）时保留旧状态并记录警告，不中断服务。
pub fn reload_certificate(
    status: &SharedCertStatus,
    valid_until: &SharedCertValidUntil,
    cfg: &ServerCfg,
    config_dir: &Path,
) {
    match load_certificate_at(cfg, config_dir) {
        Ok((s, v)) => {
            *status.write().expect("certificate_status lock poisoned") = s.clone();
            *valid_until.write().expect("certificate_valid_until lock poisoned") = v;
            log_status(&s);
        }
        Err(e) => tracing::warn!("certificate reload failed (keeping previous state): {e}"),
    }
}

/// 启动一个后台线程监听证书/公钥文件变更。
///
/// 相对路径基于 `config_dir` 解析；返回的 watcher 句柄由 detached 线程持有，
/// 线程随进程退出而终止，无需调用方额外管理。若无法创建 watcher 或注册监听，
/// 仅记录警告，不影响已在运行的旧状态。
pub fn spawn_watcher(
    status: SharedCertStatus,
    valid_until: SharedCertValidUntil,
    cfg: ServerCfg,
    config_dir: PathBuf,
) {
    std::thread::spawn(move || {
        let key_path = crate::certificate::resolve_path(&cfg.public_key_path, &config_dir);
        let cert_path = crate::certificate::resolve_path(&cfg.certificate_path, &config_dir);

        let mut watcher = match notify::recommended_watcher({
            let status = status.clone();
            let valid_until = valid_until.clone();
            // cfg / config_dir 整体移入闭包（watcher 与闭包同生命周期）；
            // 路径副本用于事件过滤，原始 key_path/cert_path 留给 watch 注册。
            let key_path_w = key_path.clone();
            let cert_path_w = cert_path.clone();
            move |res: notify::Result<Event>| {
                let ev = match res {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::warn!("certificate watcher event error: {e}");
                        return;
                    }
                };
                // 跨平台路径报告差异：inotify 给文件本身，FSEvents 常给父目录，
                // 故文件或其父目录命中即视为相关。本 watcher 只注册了这两个文件，
                // 事件本身已足够「相关」，这里仅作兜底过滤。
                let relevant = ev.paths.iter().any(|p| {
                    p == &key_path_w
                        || p == &cert_path_w
                        || p == key_path_w.parent().unwrap_or(&key_path_w)
                        || p == cert_path_w.parent().unwrap_or(&cert_path_w)
                });
                if !relevant {
                    return;
                }
                reload_certificate(&status, &valid_until, &cfg, &config_dir);
            }
        }) {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!("failed to create certificate watcher: {e}");
                return;
            }
        };

        if let Err(e) = watcher.watch(&cert_path, RecursiveMode::NonRecursive) {
            tracing::warn!("certificate watcher: cannot watch cert {}: {e}", cert_path.display());
        }
        if let Err(e) = watcher.watch(&key_path, RecursiveMode::NonRecursive) {
            tracing::warn!("certificate watcher: cannot watch key {}: {e}", key_path.display());
        }
        // 保持 watcher 句柄存活：线程 parked，直到进程退出。
        std::thread::park();
    });
}

fn log_status(s: &CertificateStatus) {
    match s {
        CertificateStatus::Valid => tracing::info!("certificate reloaded: valid"),
        CertificateStatus::Grace { remaining_secs } => tracing::warn!(
            "certificate reloaded: grace period, {} days remaining",
            remaining_secs / 86_400
        ),
        CertificateStatus::Expired => tracing::error!(
            "certificate reloaded: EXPIRED (grace period elapsed) — GET requests are now blocked"
        ),
    }
}
