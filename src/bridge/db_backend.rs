//! db 轴后端工厂（认领式注册表）：按 DSN scheme 认领连接（spec §2）。
//! 未知 scheme 的 fail-fast 归本模块（装配层不再硬编码 scheme 白名单，
//! 否则第三方插件的新 scheme 会被前置拒绝）；sqlite 路径归一化/建空库是
//! sqlite 专属逻辑，随 SqliteBackend。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;

use super::BridgeResult;
use super::accessor_sqlx::SqlxAccessor;
use super::db::{DataAccessor, InMemoryAccessor};

/// db 轴后端工厂（认领式）：按 DSN scheme 认领连接。
#[async_trait]
pub trait DbBackend: Send + Sync {
    fn name(&self) -> &str;
    /// 认领的 scheme 前缀列表（如 `["mysql://"]`）；注册时做交集检查。
    /// 非 `'static` 切片：FFI 后端的 scheme 由插件 vtable 运行期自报。
    fn schemes(&self) -> Vec<String>;
    /// config_dir：sqlite 相对路径归一化的基准（配置文件所在目录）。
    async fn connect(&self, dsn: &str, config_dir: &Path) -> BridgeResult<Arc<dyn DataAccessor>>;
}

/// 有序注册表：首个 scheme 命中者胜出；未知 scheme 明确报错。
#[derive(Default)]
pub struct DbBackendRegistry {
    backends: Vec<Arc<dyn DbBackend>>,
}

impl DbBackendRegistry {
    pub fn new() -> Self {
        Self::default()
    }
    /// 内置二后端：sqlite / memory（mysql/postgres 已迁插件，Task 4.1；由插件工厂
    /// 注册进装配期 registry——内置只留方言无关的内存/本地形态）。
    pub fn builtin() -> Self {
        let mut r = Self::new();
        // 内置注册 scheme 互不相交，unwrap 安全。
        r.register(Arc::new(SqliteBackend)).unwrap();
        r.register(Arc::new(MemoryBackend)).unwrap();
        r
    }
    /// scheme 交集冲突 → fail fast（含插件 vs 内置）。
    pub fn register(&mut self, b: Arc<dyn DbBackend>) -> BridgeResult<()> {
        for existing in &self.backends {
            for s in b.schemes() {
                if existing.schemes().contains(&s) {
                    return Err(format!(
                        "db backend '{}': scheme '{s}' already claimed by '{}'",
                        b.name(),
                        existing.name()
                    )
                    .into());
                }
            }
        }
        self.backends.push(b);
        Ok(())
    }
    /// 无认领 → 未知 scheme 报错（列出已知 scheme 便于排障）。
    pub async fn connect(&self, dsn: &str, config_dir: &Path) -> BridgeResult<Arc<dyn DataAccessor>> {
        for b in &self.backends {
            if b.schemes().iter().any(|s| dsn.starts_with(s.as_str())) {
                return b.connect(dsn, config_dir).await;
            }
        }
        let known: Vec<_> = self.backends.iter().flat_map(|b| b.schemes()).collect();
        Err(format!("unknown db scheme in dsn '{dsn}' (known: {known:?})").into())
    }
    /// 自省：已注册后端名（op_plugins 用）。
    pub fn backend_names(&self) -> Vec<&str> {
        self.backends.iter().map(|b| b.name()).collect()
    }
}

pub struct SqliteBackend;
#[async_trait]
impl DbBackend for SqliteBackend {
    fn name(&self) -> &str {
        "sqlite"
    }
    fn schemes(&self) -> Vec<String> {
        vec!["sqlite://".into(), "sqlite:".into()]
    }
    async fn connect(&self, dsn: &str, config_dir: &Path) -> BridgeResult<Arc<dyn DataAccessor>> {
        SqlxAccessor::arc(&normalize_sqlite_dsn(dsn, config_dir)?).await
    }
}

pub struct MemoryBackend;
#[async_trait]
impl DbBackend for MemoryBackend {
    fn name(&self) -> &str {
        "memory"
    }
    fn schemes(&self) -> Vec<String> {
        vec!["memory://".into()]
    }
    async fn connect(&self, _dsn: &str, _config_dir: &Path) -> BridgeResult<Arc<dyn DataAccessor>> {
        Ok(Arc::new(InMemoryAccessor::new()))
    }
}

/// sqlite DSN 归一：相对路径相对 config_dir 绝对化（缺文件建零长空库，
/// sqlx 默认 create_if_missing=false）；内存与 `:memory:`/`//` 特殊形式直通。
/// （自 oj/src/server_cmd.rs 的 resolve_dsn 提炼，语义逐行对齐。）
pub fn normalize_sqlite_dsn(dsn: &str, config_dir: &Path) -> BridgeResult<String> {
    let rest = dsn.strip_prefix("sqlite://").or_else(|| {
        if dsn == "sqlite::memory:" { Some("") } else { None }
    });
    let Some(rest) = rest else {
        return Err(format!("not a sqlite dsn (got '{dsn}')").into());
    };
    if rest.is_empty() {
        return Ok("sqlite::memory:".into()); // sqlite://（空）视作内存
    }
    if rest.starts_with(':') || rest.starts_with("//") {
        return Ok(dsn.to_string());
    }
    let p = Path::new(rest);
    let p: PathBuf = if p.is_absolute() { p.to_path_buf() } else { config_dir.join(p) };
    if !p.is_file() {
        std::fs::write(&p, b"").map_err(|e| format!("create db file {}: {e}", p.display()))?;
    }
    Ok(format!("sqlite://{}", p.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct Tmp(PathBuf);
    fn tmpdir(tag: &str) -> Tmp {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        use std::sync::atomic::Ordering;
        let d = std::env::temp_dir().join(format!(
            "oj-dbb-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        Tmp(d)
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn builtin_connects_sqlite_memory_and_memory() {
        let r = DbBackendRegistry::builtin();
        let dir = std::path::Path::new("/tmp");
        r.connect("sqlite::memory:", dir).await.unwrap_or_else(|e| panic!("{e}"));
        r.connect("memory://x", dir).await.unwrap_or_else(|e| panic!("{e}"));
        // Task 4.1：mysql/postgres 已迁插件，内置只剩 sqlite/memory。
        assert_eq!(r.backend_names(), ["sqlite", "memory"]);
    }

    #[tokio::test]
    async fn unknown_scheme_errors_with_known_list() {
        let r = DbBackendRegistry::builtin();
        let msg = match r.connect("oracle://x", std::path::Path::new("/tmp")).await {
            Err(e) => e.to_string(),
            Ok(_) => panic!("unknown scheme must fail"),
        };
        assert!(msg.contains("unknown db scheme"), "{msg}");
        assert!(msg.contains("sqlite://"), "{msg}");
    }

    #[test]
    fn scheme_conflict_fails_on_register() {
        struct Fake;
        #[async_trait::async_trait]
        impl DbBackend for Fake {
            fn name(&self) -> &str {
                "fake-sqlite"
            }
            fn schemes(&self) -> Vec<String> {
                vec!["sqlite://".into()]
            }
            async fn connect(
                &self,
                _: &str,
                _: &std::path::Path,
            ) -> BridgeResult<std::sync::Arc<dyn DataAccessor>> {
                unreachable!()
            }
        }
        let mut r = DbBackendRegistry::builtin();
        let e = r.register(std::sync::Arc::new(Fake)).unwrap_err();
        assert!(e.to_string().contains("sqlite://"));
    }

    #[test]
    fn sqlite_relative_path_resolves_against_config_dir() {
        let t = tmpdir("sqlite-rel");
        let dsn = normalize_sqlite_dsn("sqlite://app.db", &t.0).unwrap();
        let path = dsn.trim_start_matches("sqlite://");
        assert!(std::path::Path::new(path).is_absolute(), "{dsn}");
        assert!(path.starts_with(t.0.to_str().unwrap()), "{dsn}");
        // 缺文件建零长空库
        assert!(std::path::Path::new(path).is_file(), "{dsn}");
        // 内存与特殊形式直通
        assert_eq!(normalize_sqlite_dsn("sqlite::memory:", &t.0).unwrap(), "sqlite::memory:");
        assert_eq!(normalize_sqlite_dsn("sqlite://", &t.0).unwrap(), "sqlite::memory:");
    }
}
