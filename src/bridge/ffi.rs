//! ffi.rs：全部 unsafe 收敛于此（「加载 + forget」单一函数，spec §决策表）。
//! unsafe 审计清单：
//! - Library 句柄加载成功立即 Box::leak，进程期存活，任何路径不 dlclose；
//! - 插件必须 panic=unwind profile（契约 crate 文档约束）；
//! - 符号签名必须与 oj-plugin-ffi 契约一致（ABI_VERSION 门禁兜底）。

use crate::bridge::plugin_loader::PluginLoadError;
use libloading::Library;
use std::path::{Path, PathBuf};

/// 唯一 dlopen 点。加载成功立即泄漏句柄（进程期存活）。
pub(crate) unsafe fn load_forget(path: &Path) -> Result<&'static Library, PluginLoadError> {
    if !path.is_file() {
        return Err(PluginLoadError::FileMissing { path: path.to_path_buf() });
    }
    #[cfg(windows)]
    let loaded = {
        use libloading::os::windows::{Library as WinLibrary, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32};
        unsafe { WinLibrary::load_with_flags(path, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32) }
            .map(Library::from)
    };
    #[cfg(not(windows))]
    let loaded = unsafe { Library::new(path) };

    match loaded {
        Ok(lib) => Ok(Box::leak(Box::new(lib))),
        Err(e) => Err(classify_load_error(path, e)),
    }
}

/// loader 原始错误文本 → 错误分类（透出原文，spec §4）。
fn classify_load_error(path: &Path, e: impl std::fmt::Display) -> PluginLoadError {
    let text = e.to_string();
    let lower = text.to_lowercase();
    // 平台/架构不匹配（含 glibc 基线不满足：glibc 报错文本含 "glibc"/"version `glibc_x.y' not found"）。
    if lower.contains("architecture")
        || lower.contains("incompatible")
        || lower.contains("mach-o")
        || lower.contains("elf class")
        || lower.contains("wrong elf")
        || lower.contains("glibc")
        || lower.contains("%1 is not a valid win32")
    {
        PluginLoadError::PlatformMismatch { path: path.to_path_buf(), detail: text }
    } else {
        PluginLoadError::DependencyResolution { path: path.to_path_buf(), loader_text: text }
    }
}

/// 插件文件命名约定：unix `lib<name>.<so|dylib>`，windows `<name>.dll`。
pub(crate) fn plugin_file_name(name: &str) -> String {
    if cfg!(target_os = "windows") {
        format!("{name}.dll")
    } else if cfg!(target_os = "macos") {
        format!("lib{name}.dylib")
    } else {
        format!("lib{name}.so")
    }
}

/// 扫描模式：文件名 → 是否库文件（按本台扩展名）。
pub(crate) fn is_plugin_file(path: &Path) -> bool {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    if cfg!(target_os = "windows") {
        name.ends_with(".dll")
    } else if cfg!(target_os = "macos") {
        name.starts_with("lib") && name.ends_with(".dylib")
    } else {
        name.starts_with("lib") && name.ends_with(".so")
    }
}

pub(crate) fn triple() -> &'static str {
    env!("OJ_TARGET_TRIPLE")
}

pub(crate) fn workspace_root() -> PathBuf {
    PathBuf::from(env!("OJ_WORKSPACE_ROOT"))
}

// ---- core 侧适配器层（spec §3）：每轴一个 FfiXxxBackend，插件永不直接产 dyn Trait 跨界 ----

use crate::bridge::{BridgeResult, EsBackend};
use oj_plugin_ffi::{EsBackendVtable, FfiFuture, RResult, RString};

/// FfiFuture → host async 桥（S.2 定稿形态：poll 轮询 + yield_now；take→free→state 置 null）。
/// poll 返回 -1 时也 take（错误细节在 take 的 Err 里）。
/// 经 FfiGuard 持有：await 被取消时 Drop 只 free 不 take（放弃结果，插件任务允许跑完）。
pub(crate) async fn await_ffi(fut: FfiFuture) -> Result<Vec<u8>, String> {
    let mut guard = FfiGuard(Some(fut));
    loop {
        let fut = guard.0.as_mut().expect("fut present until return");
        match (fut.poll)(fut.state) {
            0 => tokio::task::yield_now().await,
            code => {
                let r = (fut.take)(fut.state);
                (fut.free)(fut.state);
                fut.state = std::ptr::null_mut(); // 防 guard Drop 二次 free
                return match (code, std::result::Result::from(r)) {
                    (1, Ok(b)) => Ok(b.iter().copied().collect()),
                    (_, Ok(_)) => Err("ffi poll reported error but take succeeded".into()),
                    (_, Err(e)) => Err(e[..].to_string()),
                };
            }
        }
    }
}

/// 宿主侧 FfiFuture 句柄守卫：state 非 null 时 Drop 只 free 不 take。
pub(crate) struct FfiGuard(Option<FfiFuture>);

impl Drop for FfiGuard {
    fn drop(&mut self) {
        if let Some(f) = self.0.take() {
            if !f.state.is_null() {
                (f.free)(f.state);
            }
        }
    }
}

fn ffi_err(ctx: &str, e: impl std::fmt::Display) -> Box<dyn std::error::Error + Send + Sync> {
    format!("ffi es {ctx}: {e}").into()
}

/// 实现 core EsBackend，内部持 opaque handle、经 vtable + FfiFuture 转发（spec §3）。
pub struct FfiEsBackend {
    handle: u64,
    vtable: &'static EsBackendVtable,
}

impl FfiEsBackend {
    pub fn new(handle: u64, vtable: &'static EsBackendVtable) -> Self {
        Self { handle, vtable }
    }
}

#[async_trait::async_trait]
impl EsBackend for FfiEsBackend {
    async fn search(&self, index: &str, dsl: serde_json::Value) -> BridgeResult<serde_json::Value> {
        let body = serde_json::to_string(&dsl).map_err(|e| ffi_err("serialize", e))?;
        let fut = (self.vtable.search)(self.handle, RString::from(index), RString::from(body.as_str()));
        let bytes = await_ffi(fut).await.map_err(|e| ffi_err("search", e))?;
        serde_json::from_slice(&bytes).map_err(|e| ffi_err("search decode", e))
    }

    async fn index_doc(
        &self,
        index: &str,
        id: &str,
        doc: serde_json::Value,
    ) -> BridgeResult<serde_json::Value> {
        let body = serde_json::to_string(&doc).map_err(|e| ffi_err("serialize", e))?;
        let fut = (self.vtable.index_doc)(
            self.handle,
            RString::from(index),
            RString::from(id),
            RString::from(body.as_str()),
        );
        let bytes = await_ffi(fut).await.map_err(|e| ffi_err("index_doc", e))?;
        serde_json::from_slice(&bytes).map_err(|e| ffi_err("index_doc decode", e))
    }

    async fn delete_doc(&self, index: &str, id: &str) -> BridgeResult<serde_json::Value> {
        let fut =
            (self.vtable.delete_doc)(self.handle, RString::from(index), RString::from(id));
        let bytes = await_ffi(fut).await.map_err(|e| ffi_err("delete_doc", e))?;
        serde_json::from_slice(&bytes).map_err(|e| ffi_err("delete_doc decode", e))
    }
}

impl Drop for FfiEsBackend {
    fn drop(&mut self) {
        (self.vtable.close)(self.handle);
    }
}

#[cfg(test)]
mod adapter_tests {
    use super::*;
    use oj_plugin_ffi::RBytes;
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    // ---- mock vtable：Rust 函数指针填充，预置 ready 的 FfiFuture ----

    struct ReadyState {
        result: Option<Result<Vec<u8>, String>>,
    }

    extern "C" fn mock_poll(state: *mut c_void) -> i32 {
        let s = unsafe { &mut *(state as *mut ReadyState) };
        match &s.result {
            Some(Ok(_)) => 1,
            Some(Err(_)) => -1,
            None => 0,
        }
    }

    extern "C" fn mock_take(state: *mut c_void) -> RResult<RBytes, RString> {
        let s = unsafe { &mut *(state as *mut ReadyState) };
        match s.result.take() {
            Some(Ok(bytes)) => {
                let mut v = RBytes::new();
                for b in bytes {
                    v.push(b);
                }
                RResult::Ok(v)
            }
            Some(Err(e)) => RResult::Err(RString::from(e.as_str())),
            None => RResult::Err(RString::from("take before ready or twice")),
        }
    }

    extern "C" fn mock_free(state: *mut c_void) {
        if !state.is_null() {
            drop(unsafe { Box::from_raw(state as *mut ReadyState) });
        }
    }

    fn ready(r: Result<Vec<u8>, String>) -> FfiFuture {
        let state = Box::into_raw(Box::new(ReadyState { result: Some(r) }));
        FfiFuture { state: state.cast(), poll: mock_poll, take: mock_take, free: mock_free }
    }

    /// 共享 statics 串行化（并行测试互踩 FAIL_NEXT/LAST_SEARCH）。
    static T_LOCK: Mutex<()> = Mutex::new(());
    static LAST_SEARCH: Mutex<(u64, String, String)> = Mutex::new((0, String::new(), String::new()));
    static CLOSED: AtomicBool = AtomicBool::new(false);
    static FAIL_NEXT: AtomicBool = AtomicBool::new(false);
    static FREED: AtomicU64 = AtomicU64::new(0);

    extern "C" fn mock_search(handle: u64, index: RString, body: RString) -> FfiFuture {
        *LAST_SEARCH.lock().unwrap() = (handle, index[..].to_string(), body[..].to_string());
        if FAIL_NEXT.swap(false, Ordering::SeqCst) {
            return ready(Err("boom from plugin".into()));
        }
        ready(Ok(br#"{"hits":[]}"#.to_vec()))
    }

    extern "C" fn mock_index_doc(
        _handle: u64,
        _index: RString,
        _id: RString,
        _body: RString,
    ) -> FfiFuture {
        ready(Ok(br#"{"result":"created"}"#.to_vec()))
    }

    extern "C" fn mock_delete_doc(_handle: u64, _index: RString, _id: RString) -> FfiFuture {
        ready(Ok(br#"{"result":"deleted"}"#.to_vec()))
    }

    extern "C" fn mock_close(_handle: u64) {
        CLOSED.store(true, Ordering::SeqCst);
    }

    use std::sync::Mutex;

    fn mock_vtable() -> &'static EsBackendVtable {
        Box::leak(Box::new(EsBackendVtable {
            search: mock_search,
            index_doc: mock_index_doc,
            delete_doc: mock_delete_doc,
            close: mock_close,
        }))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn search_forwards_params_and_decodes_response() {
        let _g = T_LOCK.lock().unwrap();
        let b = FfiEsBackend::new(42, mock_vtable());
        let v = b.search("idx1", serde_json::json!({"q": 1})).await.unwrap();
        assert_eq!(v, serde_json::json!({"hits": []}));
        let (h, i, body) = LAST_SEARCH.lock().unwrap().clone();
        assert_eq!(h, 42);
        assert_eq!(i, "idx1");
        assert_eq!(body, r#"{"q":1}"#);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn plugin_error_maps_to_bridge_err() {
        let _g = T_LOCK.lock().unwrap();
        FAIL_NEXT.store(true, Ordering::SeqCst);
        let b = FfiEsBackend::new(1, mock_vtable());
        let err = b.search("i", serde_json::json!({})).await.unwrap_err();
        assert!(err.to_string().contains("boom from plugin"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn index_and_delete_roundtrip() {
        let _g = T_LOCK.lock().unwrap();
        let b = FfiEsBackend::new(1, mock_vtable());
        let v = b.index_doc("i", "7", serde_json::json!({"a":1})).await.unwrap();
        assert_eq!(v["result"], "created");
        let v = b.delete_doc("i", "7").await.unwrap();
        assert_eq!(v["result"], "deleted");
    }

    #[test]
    fn drop_calls_close() {
        let _g = T_LOCK.lock().unwrap();
        CLOSED.store(false, Ordering::SeqCst);
        drop(FfiEsBackend::new(9, mock_vtable()));
        assert!(CLOSED.load(Ordering::SeqCst));
    }

    extern "C" fn counting_free(state: *mut c_void) {
        FREED.fetch_add(1, Ordering::SeqCst);
        mock_free(state);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn guard_drop_frees_without_take() {
        let before = FREED.load(Ordering::SeqCst);
        {
            let _g = FfiGuard(Some(FfiFuture {
                state: Box::into_raw(Box::new(ReadyState { result: Some(Ok(vec![])) })).cast(),
                poll: mock_poll,
                take: mock_take,
                free: counting_free,
            }));
        }
        assert_eq!(FREED.load(Ordering::SeqCst), before + 1);
    }
}
