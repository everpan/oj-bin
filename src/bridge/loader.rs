//! HandlerStore：handler 源码的加载与热重载。
//!
//! 设计：单一加载器、单一查找逻辑，模式由环境变量决定——
//!   - 默认：从编译期嵌入的 map 读取（生产默认，无打包/构建步骤）。
//!   - 设 `MDM_HANDLER_DIR`：从文件系统目录读取 `.js`/`.ts` 文件，并用 notify 监听变更热重载。
//!
//! per-request runtime 使"热重载"近乎免费：下次请求重新读取文件即可，无需失效模块图或清理旧状态。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use notify::{Event, RecursiveMode, Watcher};

/// handler 名 -> 源码。用 RwLock<Arc> 实现无锁读 + 原子切换。
#[derive(Clone, Default)]
pub struct HandlerStore {
    inner: std::sync::Arc<RwLock<std::sync::Arc<HashMap<String, String>>>>,
    dir: Option<PathBuf>,
}

impl HandlerStore {
    /// 从环境变量构造：若存在 `MDM_HANDLER_DIR` 则走 FS 模式并启动监听；否则为空（调用方用嵌入 map）。
    pub fn from_env() -> Self {
        match std::env::var("MDM_HANDLER_DIR") {
            Ok(dir) if !dir.is_empty() => Self::from_dir(PathBuf::from(dir)),
            _ => Self::default(),
        }
    }

    /// 从目录加载所有 `.js`/`.ts` 文件为 handler（文件名去扩展名为名），并启动热重载监听。
    pub fn from_dir(dir: PathBuf) -> Self {
        let store = Self {
            inner: RwLock::new(std::sync::Arc::new(Self::load_dir(&dir))).into(),
            dir: Some(dir.clone()),
        };
        store.spawn_watcher(dir);
        store
    }

    /// 用编译期嵌入的 map 构造（生产默认）。
    pub fn from_embedded(map: HashMap<String, String>) -> Self {
        Self {
            inner: RwLock::new(std::sync::Arc::new(map)).into(),
            dir: None,
        }
    }

    fn load_dir(dir: &Path) -> HashMap<String, String> {
        let mut map = HashMap::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            tracing::warn!(target: "handler", dir = %dir.display(), "handler dir unreadable");
            return map;
        };
        for e in entries.flatten() {
            let path = e.path();
            if !path.is_file() {
                continue;
            }
            let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
            if ext != "js" && ext != "ts" {
                continue;
            }
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
            if name.is_empty() {
                continue;
            }
            match std::fs::read_to_string(&path) {
                Ok(src) => {
                    tracing::debug!(target: "handler", name = %name, "loaded");
                    map.insert(name, src);
                }
                Err(err) => tracing::warn!(target: "handler", %err, path = %path.display(), "read failed"),
            }
        }
        map
    }

    fn spawn_watcher(&self, dir: PathBuf) {
        let inner = self.inner.clone();
        let watch_dir = dir.clone();
        let mut watcher = match notify::recommended_watcher(move |res: notify::Result<Event>| {
            if let Ok(ev) = res
                && (ev.kind.is_modify() || ev.kind.is_create() || ev.kind.is_remove())
            {
                let map = Self::load_dir(&watch_dir);
                *inner.write().unwrap() = std::sync::Arc::new(map);
                tracing::info!(target: "handler", "reloaded handlers");
            }
        }) {
            Ok(w) => w,
            Err(err) => {
                tracing::warn!(target: "handler", %err, "watcher init failed");
                return;
            }
        };
        if let Err(err) = watcher.watch(&dir, RecursiveMode::Recursive) {
            tracing::warn!(target: "handler", %err, "watch failed");
            return;
        }
        // 保持 watcher 存活：用一个后台线程持有（notify 要求 Watcher 不被 drop）。
        std::thread::spawn(move || {
            // 阻塞以保活 watcher 直到进程退出（watcher 在闭包外被 move 进线程）。
            let _ = watcher;
            loop {
                std::thread::sleep(std::time::Duration::from_secs(3600));
            }
        });
    }

    /// 取 handler 源码（克隆一份，调用方持有期间可安全执行）。
    pub fn get(&self, name: &str) -> Option<String> {
        self.inner.read().unwrap().get(name).cloned()
    }

    /// 列出所有 handler 名。
    pub fn names(&self) -> Vec<String> {
        self.inner.read().unwrap().keys().cloned().collect()
    }

    /// FS 模式目录（用于诊断）。
    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::HandlerStore;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    /// 在 $TMPDIR 下建一个带进程号+纳秒后缀的唯一临时目录。
    fn unique_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!(
            "mdm-handler-test-{}-{}-{}",
            tag,
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn load_dir_reads_js_and_ts_and_skips_others() {
        let dir = unique_dir("load");
        std::fs::write(dir.join("a.js"), "export const a=1;").unwrap();
        std::fs::write(dir.join("b.ts"), "export const b=2;").unwrap();
        std::fs::write(dir.join("c.txt"), "nope").unwrap();
        std::fs::write(dir.join("d.js.txt"), "nope").unwrap();
        let map = HandlerStore::load_dir(&dir);
        assert_eq!(map.len(), 2);
        assert_eq!(map.get("a"), Some(&"export const a=1;".to_string()));
        assert_eq!(map.get("b"), Some(&"export const b=2;".to_string()));
        assert!(!map.contains_key("c"));
        assert!(!map.contains_key("d"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_dir_missing_dir_returns_empty() {
        let map = HandlerStore::load_dir(Path::new("/nonexistent/mdm-handler-xyz"));
        assert!(map.is_empty());
    }

    #[test]
    fn from_embedded_get_names_dir_none() {
        let mut map = HashMap::new();
        map.insert("greet".into(), "console.log(1)".into());
        let store = HandlerStore::from_embedded(map);
        assert_eq!(store.get("greet").as_deref(), Some("console.log(1)"));
        assert_eq!(store.get("missing"), None);
        assert_eq!(store.names(), vec!["greet".to_string()]);
        assert!(store.dir().is_none());
    }

    #[test]
    fn from_dir_loads_and_exposes_dir() {
        let dir = unique_dir("fromdir");
        std::fs::write(dir.join("h.js"), "h").unwrap();
        let store = HandlerStore::from_dir(dir.clone());
        assert_eq!(store.get("h").as_deref(), Some("h"));
        assert_eq!(store.dir(), Some(dir.as_path()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_env_uses_dir_when_set() {
        let dir = unique_dir("fromenv");
        std::fs::write(dir.join("e.js"), "e").unwrap();
        let prev = std::env::var("MDM_HANDLER_DIR").ok();
        unsafe { std::env::set_var("MDM_HANDLER_DIR", &dir); }
        let store = HandlerStore::from_env();
        assert_eq!(store.get("e").as_deref(), Some("e"));
        assert_eq!(store.dir(), Some(dir.as_path()));
        match prev {
            Some(v) => unsafe { std::env::set_var("MDM_HANDLER_DIR", v) },
            None => unsafe { std::env::remove_var("MDM_HANDLER_DIR") },
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
