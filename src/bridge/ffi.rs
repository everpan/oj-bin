//! ffi.rs：全部 unsafe 收敛于此（「加载 + forget」单一函数，spec §决策表）。
//! unsafe 审计清单：
//! - Library 句柄加载成功立即 Box::leak，进程期存活，任何路径不 dlclose；
//! - 插件必须 panic=unwind profile（契约 crate 文档约束）；
//! - 符号签名必须与 oj-plugin-ffi 契约一致（ABI_VERSION 门禁兜底）。

#![allow(clippy::collapsible_if)]
use crate::bridge::plugin_loader::PluginLoadError;
use libloading::Library;

use std::path::{Path, PathBuf};

/// 唯一 dlopen 点。加载成功立即泄漏句柄（进程期存活）。
pub(crate) unsafe fn load_forget(path: &Path) -> Result<&'static Library, PluginLoadError> {
    if !path.is_file() {
        return Err(PluginLoadError::FileMissing {
            path: path.to_path_buf(),
        });
    }
    #[cfg(windows)]
    let loaded = {
        use libloading::os::windows::{
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32, Library as WinLibrary,
        };
        unsafe {
            WinLibrary::load_with_flags(
                path,
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
            )
        }
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
///
/// 启发式：仅按关键词粗分「平台不匹配」与「依赖解析失败」两类（macOS 文本文件等
/// 非库文件会落到 DependencyResolution）。分类不影响 fail-fast 结论，仅影响报错文案，
/// 故边界近似可接受（M-4 注明）。
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
        || lower.contains("image") // macOS 非库文件（如文本/脚本）dlopen 报错含 "image"
        || lower.contains("file too short") // 截断/非 ELF 文件
        || lower.contains("%1 is not a valid win32")
    {
        PluginLoadError::PlatformMismatch {
            path: path.to_path_buf(),
            detail: text,
        }
    } else {
        PluginLoadError::DependencyResolution {
            path: path.to_path_buf(),
            loader_text: text,
        }
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

/// 宿主目标 triple（与 xtask 的 `rustc -vV` host 一致），用于拼出插件目录
/// `<plugins>/<triple>/`。原先由 build.rs 烧入 `OJ_TARGET_TRIPLE`，现改为运行时按
/// `std::env::consts` 重建——生产环境未必有 rustc，故不调用 `rustc -vV`；放置侧（xtask）
/// 与发现侧（plugin_loader）同为当前宿主 triple，保持一致。
pub(crate) fn triple() -> &'static str {
    static T: LazyLock<String> = LazyLock::new(|| {
        let arch = std::env::consts::ARCH;
        match std::env::consts::OS {
            "macos" => format!("{arch}-apple-darwin"),
            "windows" => format!("{arch}-pc-windows-msvc"),
            "linux" => format!("{arch}-unknown-linux-gnu"),
            other => format!("{arch}-unknown-{other}-gnu"),
        }
    });
    T.as_str()
}

/// 工作区根目录：原先由 build.rs 烧入 `OJ_WORKSPACE_ROOT`（= 根 crate 的
/// CARGO_MANIFEST_DIR），现直接用内置的 `CARGO_MANIFEST_DIR`（无需 build.rs）。
pub(crate) fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

// ---- core 侧适配器层（spec §3）：每轴一个 FfiXxxBackend，插件永不直接产 dyn Trait 跨界 ----

use crate::bridge::db::{DataAccessor, Dialect, Row, TxSession};
use crate::bridge::{
    BlobBackend, BlobServed, BridgeResult, BusBackend, DbBackend, EsBackend, EventBroker, KVStore,
};
use crate::config::BrokerCfg;
use oj_plugin_ffi::{
    BlobBackendVtable, DataAccessorVtable, EsBackendVtable, EventBrokerVtable, FfiFuture,
    KVStoreVtable, RBytes, RString,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::mpsc::UnboundedSender;

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

/// mq 长轮询版 await_ffi：Pending 时 sleep 退避（评审 F4——yield_now 空转烧满一核）。
/// 其余语义（take→free→Guard Drop 只 free 不 take）与 await_ffi 完全一致。
pub(crate) async fn await_ffi_poll(
    fut: FfiFuture,
    backoff: std::time::Duration,
) -> Result<Vec<u8>, String> {
    let mut guard = FfiGuard(Some(fut));
    loop {
        let fut = guard.0.as_mut().expect("fut present until return");
        match (fut.poll)(fut.state) {
            0 => tokio::time::sleep(backoff).await,
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
    format!("ffi {ctx}: {e}").into()
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
        let fut = (self.vtable.search)(
            self.handle,
            RString::from(index),
            RString::from(body.as_str()),
        );
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
        let fut = (self.vtable.delete_doc)(self.handle, RString::from(index), RString::from(id));
        let bytes = await_ffi(fut).await.map_err(|e| ffi_err("delete_doc", e))?;
        serde_json::from_slice(&bytes).map_err(|e| ffi_err("delete_doc decode", e))
    }
}

impl Drop for FfiEsBackend {
    fn drop(&mut self) {
        (self.vtable.close)(self.handle);
    }
}

// ---- auth 轴适配器（Task auth-1）：同步 vtable 转发，无 FfiFuture ----

/// auth 守卫适配器：实现 core AuthGuard，经同步 vtable 转发（无 FfiFuture）。
pub struct FfiAuthGuard {
    vtable: &'static oj_plugin_ffi::AuthGuardVtable,
}

impl FfiAuthGuard {
    pub fn new(vtable: &'static oj_plugin_ffi::AuthGuardVtable) -> Self {
        Self { vtable }
    }
}

impl crate::bridge::auth::AuthGuard for FfiAuthGuard {
    fn verify(
        &self,
        path_no_base: &str,
        authorization: Option<&str>,
    ) -> Result<Option<serde_json::Value>, String> {
        let r = (self.vtable.verify)(
            RString::from(path_no_base),
            RString::from(authorization.unwrap_or("")),
        );
        match std::result::Result::from(r) {
            Ok(json) => {
                let v: serde_json::Value = serde_json::from_str(&json[..])
                    .map_err(|e| format!("auth plugin returned bad json: {e}"))?;
                // 契约：null = 匿名放行，object = http.user；标量/数组视为坏插件输出。
                if v.is_null() {
                    Ok(None)
                } else if v.is_object() {
                    Ok(Some(v))
                } else {
                    Err("auth plugin returned non-object user".to_string())
                }
            }
            Err(e) => Err(e[..].to_string()),
        }
    }
}

// ---- db 轴适配器（Task 4.1）：FfiDbBackend（工厂）→ FfiDataAccessor（连接）→ FfiTxSession（事务）----

/// db 工厂适配器：实现 core DbBackend，scheme 由插件 vtable 自我声明（spec §2 认领式）。
pub struct FfiDbBackend {
    name: String,
    schemes: Vec<String>,
    vtable: &'static DataAccessorVtable,
}

impl FfiDbBackend {
    /// 构造即调 vtable.schemes() 读认领列表（装配期一次）。
    pub fn new(name: impl Into<String>, vtable: &'static DataAccessorVtable) -> Self {
        let schemes: Vec<String> = (vtable.schemes)()
            .iter()
            .map(|s| s[..].to_string())
            .collect();
        Self {
            name: name.into(),
            schemes,
            vtable,
        }
    }
}

#[async_trait::async_trait]
impl DbBackend for FfiDbBackend {
    fn name(&self) -> &str {
        &self.name
    }
    fn schemes(&self) -> Vec<String> {
        self.schemes.clone()
    }
    async fn connect(&self, dsn: &str, _config_dir: &Path) -> BridgeResult<Arc<dyn DataAccessor>> {
        let fut = (self.vtable.connect)(RString::from(dsn));
        let bytes = await_ffi(fut).await.map_err(|e| ffi_err("db connect", e))?;
        let handle = serde_json::from_slice::<serde_json::Value>(&bytes)
            .map_err(|e| ffi_err("db connect decode", e))?
            .get("handle")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| ffi_err("db connect", "missing handle"))?;
        Ok(Arc::new(FfiDataAccessor::new(handle, self.vtable)))
    }
}

/// 连接适配器：实现 core DataAccessor，经 vtable + FfiFuture 转发（handle 查表在插件侧）。
pub struct FfiDataAccessor {
    handle: u64,
    vtable: &'static DataAccessorVtable,
}

impl FfiDataAccessor {
    pub fn new(handle: u64, vtable: &'static DataAccessorVtable) -> Self {
        Self { handle, vtable }
    }
}

/// JSON 数组 → 参数化绑定载荷（Value 边界；插件侧反序列化绑定）。
fn params_json(
    params: &[serde_json::Value],
) -> Result<RString, Box<dyn std::error::Error + Send + Sync>> {
    let s = serde_json::to_string(params).map_err(|e| ffi_err("db serialize", e))?;
    Ok(RString::from(s.as_str()))
}

#[async_trait::async_trait]
impl DataAccessor for FfiDataAccessor {
    fn dialect(&self) -> Dialect {
        match &(self.vtable.dialect)(self.handle)[..] {
            "mysql" => Dialect::MySql,
            "postgres" => Dialect::Postgres,
            _ => Dialect::Sqlite,
        }
    }

    async fn begin(&self) -> BridgeResult<Box<dyn TxSession>> {
        let fut = (self.vtable.begin)(self.handle);
        let bytes = await_ffi(fut).await.map_err(|e| ffi_err("db begin", e))?;
        let tx_id = serde_json::from_slice::<serde_json::Value>(&bytes)
            .map_err(|e| ffi_err("db begin decode", e))?
            .get("tx_id")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| ffi_err("db begin", "missing tx_id"))?;
        Ok(Box::new(FfiTxSession::new(self.handle, tx_id, self.vtable)))
    }

    async fn query_with_params(
        &self,
        sql: &str,
        params: &[serde_json::Value],
    ) -> BridgeResult<Vec<Row>> {
        let p = params_json(params)?;
        let fut = (self.vtable.query)(self.handle, RString::from(sql), p);
        let bytes = await_ffi(fut).await.map_err(|e| ffi_err("db query", e))?;
        serde_json::from_slice(&bytes).map_err(|e| ffi_err("db query decode", e))
    }

    async fn exec_with_params(&self, sql: &str, params: &[serde_json::Value]) -> BridgeResult<i64> {
        let p = params_json(params)?;
        let fut = (self.vtable.exec)(self.handle, RString::from(sql), p);
        let bytes = await_ffi(fut).await.map_err(|e| ffi_err("db exec", e))?;
        serde_json::from_slice(&bytes).map_err(|e| ffi_err("db exec decode", e))
    }
}

impl Drop for FfiDataAccessor {
    fn drop(&mut self) {
        (self.vtable.close)(self.handle);
    }
}

/// 事务适配器：实现 core TxSession；Drop 时未完结 → fire tx_rollback（结果放弃，
/// 插件任务跑在插件 runtime 上照常执行，spec §3 FfiFuture drop 条——ReqState reset
/// 丢弃存活事务 = 保底回滚语义的 FFI 保留）。
pub struct FfiTxSession {
    handle: u64,
    tx_id: u64,
    vtable: &'static DataAccessorVtable,
    finished: AtomicBool,
}

impl FfiTxSession {
    fn new(handle: u64, tx_id: u64, vtable: &'static DataAccessorVtable) -> Self {
        Self {
            handle,
            tx_id,
            vtable,
            finished: AtomicBool::new(false),
        }
    }
}

#[async_trait::async_trait]
impl TxSession for FfiTxSession {
    async fn query(&self, sql: &str, params: &[serde_json::Value]) -> BridgeResult<Vec<Row>> {
        let p = params_json(params)?;
        let fut = (self.vtable.tx_query)(self.handle, self.tx_id, RString::from(sql), p);
        let bytes = await_ffi(fut)
            .await
            .map_err(|e| ffi_err("db tx_query", e))?;
        serde_json::from_slice(&bytes).map_err(|e| ffi_err("db tx_query decode", e))
    }

    async fn exec(&self, sql: &str, params: &[serde_json::Value]) -> BridgeResult<i64> {
        let p = params_json(params)?;
        let fut = (self.vtable.tx_exec)(self.handle, self.tx_id, RString::from(sql), p);
        let bytes = await_ffi(fut).await.map_err(|e| ffi_err("db tx_exec", e))?;
        serde_json::from_slice(&bytes).map_err(|e| ffi_err("db tx_exec decode", e))
    }

    async fn commit(&self) -> BridgeResult<()> {
        let fut = (self.vtable.tx_commit)(self.handle, self.tx_id);
        await_ffi(fut)
            .await
            .map_err(|e| ffi_err("db tx_commit", e))?;
        self.finished.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn rollback(&self) -> BridgeResult<()> {
        let fut = (self.vtable.tx_rollback)(self.handle, self.tx_id);
        await_ffi(fut)
            .await
            .map_err(|e| ffi_err("db tx_rollback", e))?;
        self.finished.store(true, Ordering::SeqCst);
        Ok(())
    }
}

impl Drop for FfiTxSession {
    fn drop(&mut self) {
        if !self.finished.load(Ordering::SeqCst) {
            let fut = (self.vtable.tx_rollback)(self.handle, self.tx_id);
            let _guard = FfiGuard(Some(fut)); // free state；插件侧 rollback 照常执行
        }
    }
}

// ---- blob 轴适配器（Task 4.2）：FfiBlobBackend（vtable → core BlobBackend，LocalBlob 留内置）----

/// 经 vtable + FfiFuture 转发（handle 由 connect 产生；五方法过线）。
/// content_type 空串 ↔ None；vtable 无 serve——serve = Redirect(url)（s3 语义，
/// LocalBlob 留 core 内置，插件的 serve 一律走 presign 重定向）。
pub struct FfiBlobBackend {
    handle: u64,
    vtable: &'static BlobBackendVtable,
}

impl FfiBlobBackend {
    pub fn new(handle: u64, vtable: &'static BlobBackendVtable) -> Self {
        Self { handle, vtable }
    }
}

/// Vec<u8> → RBytes（stabby 无 From<&[u8]>，逐元素 push）。
fn to_rbytes(bytes: &[u8]) -> RBytes {
    let mut v = RBytes::new();
    for b in bytes {
        v.push(*b);
    }
    v
}

#[async_trait::async_trait]
impl BlobBackend for FfiBlobBackend {
    async fn put(&self, key: &str, bytes: &[u8], content_type: Option<&str>) -> BridgeResult<()> {
        let ct = RString::from(content_type.unwrap_or(""));
        let fut = (self.vtable.put)(self.handle, RString::from(key), to_rbytes(bytes), ct);
        await_ffi(fut).await.map_err(|e| ffi_err("blob put", e))?;
        Ok(())
    }

    async fn get(&self, key: &str) -> BridgeResult<Vec<u8>> {
        let fut = (self.vtable.get)(self.handle, RString::from(key));
        let bytes = await_ffi(fut).await.map_err(|e| ffi_err("blob get", e))?;
        Ok(bytes)
    }

    async fn del(&self, key: &str) -> BridgeResult<()> {
        let fut = (self.vtable.del)(self.handle, RString::from(key));
        await_ffi(fut).await.map_err(|e| ffi_err("blob del", e))?;
        Ok(())
    }

    async fn url(&self, key: &str) -> BridgeResult<String> {
        let fut = (self.vtable.url)(self.handle, RString::from(key));
        let bytes = await_ffi(fut).await.map_err(|e| ffi_err("blob url", e))?;
        String::from_utf8(bytes).map_err(|e| ffi_err("blob url decode", e))
    }

    async fn content_type(&self, key: &str) -> BridgeResult<Option<String>> {
        let fut = (self.vtable.content_type)(self.handle, RString::from(key));
        let bytes = await_ffi(fut)
            .await
            .map_err(|e| ffi_err("blob content_type", e))?;
        let s = String::from_utf8(bytes).map_err(|e| ffi_err("blob content_type decode", e))?;
        Ok((!s.is_empty()).then_some(s))
    }

    async fn serve(&self, key: &str) -> BridgeResult<BlobServed> {
        Ok(BlobServed::Redirect(self.url(key).await?))
    }
}

impl Drop for FfiBlobBackend {
    fn drop(&mut self) {
        (self.vtable.close)(self.handle);
    }
}

// ---- bus 轴适配器（Task 4.3）：FfiBusBackend（工厂）→ FfiEventBroker（经 deliver 扇出）----

/// bus 插件 deliver 回调的本地扇出目标（topic → WS 订阅通道）。
/// 进程内一次一个 bus broker（键选式单后端）；跨 actor 池/全部 WS 连接的共享语义
/// 经此全局目标表保持（Task 0.5 回归）。deliver 回调签名无 handle——插件消费循环
/// 只按 topic 上送，宿主按 topic 路由（UnboundedSender 不过 FFI 边界，spec §3）。
pub(crate) static DELIVER_TARGETS: std::sync::LazyLock<
    std::sync::Mutex<HashMap<String, Vec<UnboundedSender<String>>>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

/// host 侧 deliver 回调（HostContext.deliver 指向此）：非阻塞投递，满/closed 惰性清理。
/// 语义 = Bus::publish 的本地扇出（payload 原样转发；按 topic 去重注册）。
pub(crate) extern "C" fn host_deliver(topic: RString, payload: RString) {
    let (topic, payload) = (topic[..].to_string(), payload[..].to_string());
    let mut g = DELIVER_TARGETS.lock().unwrap();
    if let Some(list) = g.get_mut(&topic) {
        list.retain(|tx| tx.send(payload.clone()).is_ok());
        if list.is_empty() {
            g.remove(&topic);
        }
    }
}

/// bus 工厂适配器：实现 core BusBackend（kind 键选式），connect 经 vtable + FfiFuture。
pub struct FfiBusBackend {
    /// broker 类型标识（插件名去 "bus-" 前缀；如 "bus-kafka" → "kafka"）。
    kind: &'static str,
    vtable: &'static EventBrokerVtable,
}

impl FfiBusBackend {
    pub fn new(name: impl Into<String>, vtable: &'static EventBrokerVtable) -> Self {
        let name = name.into();
        let kind = name.strip_prefix("bus-").unwrap_or(&name).to_string();
        let kind: &'static str = Box::leak(kind.into_boxed_str()); // 每插件一次，进程期存活
        Self { kind, vtable }
    }
}

#[async_trait::async_trait]
impl BusBackend for FfiBusBackend {
    fn kind(&self) -> &str {
        self.kind
    }
    async fn connect(&self, cfg: &BrokerCfg) -> BridgeResult<Arc<dyn EventBroker>> {
        let cfg_json = serde_json::to_string(cfg).map_err(|e| ffi_err("bus cfg serialize", e))?;
        let fut = (self.vtable.connect)(RString::from(cfg_json.as_str()));
        let bytes = await_ffi(fut)
            .await
            .map_err(|e| ffi_err("bus connect", e))?;
        let handle = serde_json::from_slice::<serde_json::Value>(&bytes)
            .map_err(|e| ffi_err("bus connect decode", e))?
            .get("handle")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| ffi_err("bus connect", "missing handle"))?;
        Ok(Arc::new(FfiEventBroker::new(
            self.kind,
            handle,
            self.vtable,
        )))
    }
}

/// broker 适配器：实现 core EventBroker。subscribe 本地注册 tx + 每 topic 至多一个
/// 插件消费循环（vtable.subscribe 幂等去重）；插件收到消息经 host.deliver 上送 →
/// 全局 DELIVER_TARGETS 按 topic 扇出（跨 actor/WS 共享语义与内置 Bus 一致）。
///
/// I-1 修复（接手点 A-1）：subscribe 全程持 `SUBSCRIBE_GATE`，vtable 失败回滚刚注册的
/// 本通道（杜绝僵尸注册），且并发首次订阅同一新 topic 不再各自起消费循环（rabbitmq
/// 每订阅者独享队列的重复投递）。drop 仅清本 broker 注册的目标（M-1）。
pub struct FfiEventBroker {
    kind: &'static str,
    handle: u64,
    vtable: &'static EventBrokerVtable,
    /// 本 broker 在 DELIVER_TARGETS 中注册的 (topic, sender)，drop 按归属清理（M-1）。
    subs: StdMutex<Vec<(String, UnboundedSender<String>)>>,
}

/// 首次订阅新 topic 的并发门禁：保证 vtable.subscribe 的「注册 + 起消费循环」原子，
/// 避免并发首次订阅同一新 topic 各起一个消费循环（rabbitmq 重复投递），并配合失败回滚。
static SUBSCRIBE_GATE: LazyLock<AsyncMutex<()>> = LazyLock::new(|| AsyncMutex::new(()));

impl FfiEventBroker {
    pub fn new(kind: &'static str, handle: u64, vtable: &'static EventBrokerVtable) -> Self {
        Self {
            kind,
            handle,
            vtable,
            subs: StdMutex::new(Vec::new()),
        }
    }
}

#[async_trait::async_trait]
impl EventBroker for FfiEventBroker {
    fn kind(&self) -> &'static str {
        self.kind
    }

    async fn publish(&self, topic: &str, data: &Value) -> BridgeResult<usize> {
        let frame = json!({ "topic": topic, "data": data }).to_string();
        let fut = (self.vtable.publish)(
            self.handle,
            RString::from(topic),
            RString::from(frame.as_str()),
        );
        await_ffi(fut)
            .await
            .map_err(|e| ffi_err("bus publish", e))?;
        Ok(0) // 远程 broker 经网络投递，本地 fan-out 恒 0（语义对齐 core Kafka/Rabbit）。
    }

    async fn subscribe(&self, topic: &str, tx: UnboundedSender<String>) -> BridgeResult<()> {
        // 全程持门禁：vtable.subscribe 是「本地注册 + 起消费循环」的原子单元；
        // 失败回滚刚注册的通道，避免僵尸注册导致该 topic 静默丢失。
        let _gate = SUBSCRIBE_GATE.lock().await;
        let (is_new_topic, inserted) = {
            let mut g = DELIVER_TARGETS.lock().unwrap();
            let list = g.entry(topic.to_string()).or_default();
            let is_new_topic = list.is_empty();
            // 同 channel 去重（同一 tx 重复订阅不重复注册）。
            let inserted = if !list.iter().any(|t| t.same_channel(&tx)) {
                list.push(tx.clone());
                true
            } else {
                false
            };
            (is_new_topic, inserted)
        };
        if is_new_topic {
            let fut = (self.vtable.subscribe)(self.handle, RString::from(topic));
            if let Err(e) = await_ffi(fut).await {
                // 回滚：仅移除本次刚注册的本通道（列表空则删整条 topic），
                // 不误伤其他订阅者。
                if inserted {
                    let mut g = DELIVER_TARGETS.lock().unwrap();
                    if let Some(list) = g.get_mut(topic) {
                        list.retain(|t| !t.same_channel(&tx));
                        if list.is_empty() {
                            g.remove(topic);
                        }
                    }
                }
                return Err(ffi_err("bus subscribe", e));
            }
        }
        self.subs.lock().unwrap().push((topic.to_string(), tx));
        Ok(())
    }
}

impl Drop for FfiEventBroker {
    fn drop(&mut self) {
        (self.vtable.close)(self.handle);
        // M-1：仅清本 broker 注册的目标，不再整表清空（避免误伤其他 broker 订阅）。
        let registered = std::mem::take(&mut *self.subs.lock().unwrap());
        let mut g = DELIVER_TARGETS.lock().unwrap();
        for (topic, tx) in registered {
            if let Some(list) = g.get_mut(&topic) {
                list.retain(|t| !t.same_channel(&tx));
                if list.is_empty() {
                    g.remove(&topic);
                }
            }
        }
    }
}

// ---- kv 轴适配器（Task 4.4）：FfiKVStore（vtable → core KVStore；InMemoryKV 留内置兜底）----

/// 经 vtable + FfiFuture 转发（handle 由 connect 产生；五方法过线）。
/// 返回编码：get = JSON `Option<String>`；expire = JSON `bool`；incr = JSON `i64`；
/// set/del = 空。跨线时长以秒计——expire 的 Duration 在宿主侧经 kv::expire_secs
/// 向上取整到整秒（Redis EXPIRE 契约，ceil 逻辑留宿主，插件只认秒）。
pub struct FfiKVStore {
    handle: u64,
    vtable: &'static KVStoreVtable,
}

impl FfiKVStore {
    pub fn new(handle: u64, vtable: &'static KVStoreVtable) -> Self {
        Self { handle, vtable }
    }
}

#[async_trait::async_trait]
impl KVStore for FfiKVStore {
    async fn get(&self, key: &str) -> BridgeResult<Option<String>> {
        let fut = (self.vtable.get)(self.handle, RString::from(key));
        let bytes = await_ffi(fut).await.map_err(|e| ffi_err("kv get", e))?;
        serde_json::from_slice(&bytes).map_err(|e| ffi_err("kv get decode", e))
    }

    async fn set(&self, key: &str, value: &str) -> BridgeResult<()> {
        let fut = (self.vtable.set)(self.handle, RString::from(key), RString::from(value));
        await_ffi(fut).await.map_err(|e| ffi_err("kv set", e))?;
        Ok(())
    }

    async fn del(&self, key: &str) -> BridgeResult<()> {
        let fut = (self.vtable.del)(self.handle, RString::from(key));
        await_ffi(fut).await.map_err(|e| ffi_err("kv del", e))?;
        Ok(())
    }

    async fn expire(&self, key: &str, ttl: std::time::Duration) -> BridgeResult<bool> {
        let secs = crate::bridge::kv::expire_secs(ttl) as u64;
        let fut = (self.vtable.expire)(self.handle, RString::from(key), secs);
        let bytes = await_ffi(fut).await.map_err(|e| ffi_err("kv expire", e))?;
        serde_json::from_slice(&bytes).map_err(|e| ffi_err("kv expire decode", e))
    }

    async fn incr(&self, key: &str) -> BridgeResult<i64> {
        let fut = (self.vtable.incr)(self.handle, RString::from(key));
        let bytes = await_ffi(fut).await.map_err(|e| ffi_err("kv incr", e))?;
        serde_json::from_slice(&bytes).map_err(|e| ffi_err("kv incr decode", e))
    }
}

impl Drop for FfiKVStore {
    fn drop(&mut self) {
        (self.vtable.close)(self.handle);
    }
}

#[cfg(test)]
// `T_LOCK` 是**测试串行化**锁（保护进程级共享 statics：FAIL_NEXT / LAST_SEARCH / FREED），
// 不是被 await 的临界区数据锁：这些用例跑在 `multi_thread` runtime 上，持锁跨 await 只是
// 让同批用例排队，不会与其它 lock 形成环，故无死锁风险；改成 drop 再 await 反而会失去串行化。
// 与 `oj/tests/e2e.rs`、`server/src/ws.rs` 的同类豁免同理（注释互指）。
#[allow(clippy::await_holding_lock)]
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
        FfiFuture {
            state: state.cast(),
            poll: mock_poll,
            take: mock_take,
            free: mock_free,
        }
    }

    /// 共享 statics 串行化（并行测试互踩 FAIL_NEXT/LAST_SEARCH）。
    static T_LOCK: Mutex<()> = Mutex::new(());
    static LAST_SEARCH: Mutex<(u64, String, String)> =
        Mutex::new((0, String::new(), String::new()));
    static CLOSED: AtomicBool = AtomicBool::new(false);
    static FAIL_NEXT: AtomicBool = AtomicBool::new(false);
    static FREED: AtomicU64 = AtomicU64::new(0);

    /// 模式开关守卫：置位 mock 的行为开关，作用域结束（含 panic）自动复位为 0，
    /// 防止失败用例污染同轴后续用例。用法：`let _m = Mode::set(&ES_MODE, 2);`。
    struct Mode<'a>(&'a Mutex<u8>);
    impl Mode<'_> {
        fn set(m: &'static Mutex<u8>, v: u8) -> Self {
            *m.lock().unwrap() = v;
            Self(m)
        }
    }
    impl Drop for Mode<'_> {
        fn drop(&mut self) {
            *self.0.lock().unwrap() = 0;
        }
    }

    /// es 行为开关：2=search 返回非 JSON；3=index_doc 报错；4=delete_doc 报错。
    static ES_MODE: Mutex<u8> = Mutex::new(0);

    extern "C" fn mock_search(handle: u64, index: RString, body: RString) -> FfiFuture {
        *LAST_SEARCH.lock().unwrap() = (handle, index[..].to_string(), body[..].to_string());
        if FAIL_NEXT.swap(false, Ordering::SeqCst) {
            return ready(Err("boom from plugin".into()));
        }
        if *ES_MODE.lock().unwrap() == 2 {
            return ready(Ok(b"gibberish".to_vec()));
        }
        ready(Ok(br#"{"hits":[]}"#.to_vec()))
    }

    extern "C" fn mock_index_doc(
        _handle: u64,
        _index: RString,
        _id: RString,
        _body: RString,
    ) -> FfiFuture {
        if *ES_MODE.lock().unwrap() == 3 {
            return ready(Err("index boom".into()));
        }
        ready(Ok(br#"{"result":"created"}"#.to_vec()))
    }

    extern "C" fn mock_delete_doc(_handle: u64, _index: RString, _id: RString) -> FfiFuture {
        if *ES_MODE.lock().unwrap() == 4 {
            return ready(Err("delete boom".into()));
        }
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
        let v = b
            .index_doc("i", "7", serde_json::json!({"a":1}))
            .await
            .unwrap();
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
                state: Box::into_raw(Box::new(ReadyState {
                    result: Some(Ok(vec![])),
                }))
                .cast(),
                poll: mock_poll,
                take: mock_take,
                free: counting_free,
            }));
        }
        assert_eq!(FREED.load(Ordering::SeqCst), before + 1);
    }

    // ---- db 轴 mock vtable（Task 4.1；共享 statics 串行化互踩）----

    use oj_plugin_ffi::{RResult, RVec};
    use std::sync::atomic::Ordering as AtomicOrdering;

    static DB_CONNECTED_CFG: Mutex<String> = Mutex::new(String::new());
    static DB_QUERY: Mutex<(u64, String, String)> = Mutex::new((0, String::new(), String::new()));
    static DB_TX_QUERY: Mutex<(u64, u64, String, String)> =
        Mutex::new((0, 0, String::new(), String::new()));
    static DB_COMMITTED: AtomicU64 = AtomicU64::new(0);
    static DB_ROLLED_BACK: AtomicU64 = AtomicU64::new(0);
    static DB_CLOSED: AtomicU64 = AtomicU64::new(0);
    /// db connect 行为开关：1=报错；2=非 JSON；3=缺 handle。
    static DB_CONNECT_MODE: Mutex<u8> = Mutex::new(0);
    /// begin 行为开关：1=报错；2=非 JSON；3=缺 tx_id。
    static DB_BEGIN_MODE: Mutex<u8> = Mutex::new(0);
    /// query/exec/tx_query/tx_exec 行为开关：1=报错；2=非 JSON。
    static DB_CALL_MODE: Mutex<u8> = Mutex::new(0);
    /// commit/rollback 行为开关：1=报错（记录 tx_id 后再报，模拟插件侧已动作）。
    static DB_TX_END_MODE: Mutex<u8> = Mutex::new(0);
    /// vtable 自报方言：0=postgres；1=mysql；2=未知方言。
    static DB_DIALECT_MODE: Mutex<u8> = Mutex::new(0);

    /// DB_CALL_MODE 驱动的 query 类返回体。
    fn db_call_result(ok: &[u8]) -> FfiFuture {
        match *DB_CALL_MODE.lock().unwrap() {
            1 => ready(Err("call down".into())),
            2 => ready(Ok(b"gibberish".to_vec())),
            _ => ready(Ok(ok.to_vec())),
        }
    }

    extern "C" fn mock_db_connect(cfg: RString) -> FfiFuture {
        *DB_CONNECTED_CFG.lock().unwrap() = cfg[..].to_string();
        match *DB_CONNECT_MODE.lock().unwrap() {
            1 => ready(Err("db down".into())),
            2 => ready(Ok(b"gibberish".to_vec())),
            3 => ready(Ok(br#"{}"#.to_vec())),
            _ => ready(Ok(br#"{"handle":42}"#.to_vec())),
        }
    }
    extern "C" fn mock_db_query(handle: u64, sql: RString, params: RString) -> FfiFuture {
        *DB_QUERY.lock().unwrap() = (handle, sql[..].to_string(), params[..].to_string());
        db_call_result(br#"[{"c":1,"t":"a"}]"#)
    }
    extern "C" fn mock_db_exec(_h: u64, _s: RString, _p: RString) -> FfiFuture {
        db_call_result(br#"3"#)
    }
    extern "C" fn mock_db_begin(_handle: u64) -> FfiFuture {
        match *DB_BEGIN_MODE.lock().unwrap() {
            1 => ready(Err("begin down".into())),
            2 => ready(Ok(b"gibberish".to_vec())),
            3 => ready(Ok(br#"{}"#.to_vec())),
            _ => ready(Ok(br#"{"tx_id":7}"#.to_vec())),
        }
    }
    extern "C" fn mock_db_tx_query(
        handle: u64,
        tx_id: u64,
        sql: RString,
        params: RString,
    ) -> FfiFuture {
        *DB_TX_QUERY.lock().unwrap() = (handle, tx_id, sql[..].to_string(), params[..].to_string());
        db_call_result(br#"[{"c":9}]"#)
    }
    extern "C" fn mock_db_tx_exec(_h: u64, _t: u64, _s: RString, _p: RString) -> FfiFuture {
        db_call_result(br#"1"#)
    }
    extern "C" fn mock_db_tx_commit(_h: u64, tx_id: u64) -> FfiFuture {
        DB_COMMITTED.store(tx_id, AtomicOrdering::SeqCst);
        if *DB_TX_END_MODE.lock().unwrap() == 1 {
            return ready(Err("commit down".into()));
        }
        ready(Ok(b"".to_vec()))
    }
    extern "C" fn mock_db_tx_rollback(_h: u64, tx_id: u64) -> FfiFuture {
        DB_ROLLED_BACK.store(tx_id, AtomicOrdering::SeqCst);
        if *DB_TX_END_MODE.lock().unwrap() == 1 {
            return ready(Err("rollback down".into()));
        }
        ready(Ok(b"".to_vec()))
    }
    extern "C" fn mock_db_dialect(_handle: u64) -> RString {
        RString::from(match *DB_DIALECT_MODE.lock().unwrap() {
            1 => "mysql",
            2 => "weirddb",
            _ => "postgres",
        })
    }
    extern "C" fn mock_db_close(handle: u64) {
        DB_CLOSED.store(handle, AtomicOrdering::SeqCst);
    }
    extern "C" fn mock_db_schemes() -> RVec<RString> {
        let mut v = RVec::new();
        v.push(RString::from("mysql://"));
        v.push(RString::from("mariadb://"));
        v
    }

    fn mock_db_vtable() -> &'static DataAccessorVtable {
        Box::leak(Box::new(DataAccessorVtable {
            connect: mock_db_connect,
            query: mock_db_query,
            exec: mock_db_exec,
            begin: mock_db_begin,
            tx_query: mock_db_tx_query,
            tx_exec: mock_db_tx_exec,
            tx_commit: mock_db_tx_commit,
            tx_rollback: mock_db_tx_rollback,
            dialect: mock_db_dialect,
            close: mock_db_close,
            schemes: mock_db_schemes,
        }))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn db_backend_schemes_and_connect_dispatch() {
        let _g = T_LOCK.lock().unwrap();
        let be = FfiDbBackend::new("db-mysql", mock_db_vtable());
        assert_eq!(be.name(), "db-mysql");
        assert_eq!(be.schemes(), vec!["mysql://", "mariadb://"]);
        let da = be
            .connect("mysql://u:p@h/d", std::path::Path::new("/tmp"))
            .await
            .unwrap();
        assert_eq!(*DB_CONNECTED_CFG.lock().unwrap(), "mysql://u:p@h/d");
        assert_eq!(da.dialect(), Dialect::Postgres);
        assert_eq!(
            da.query_with_params("select 1", &[]).await.unwrap()[0]["c"],
            serde_json::json!(1)
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn db_query_and_exec_forward_params_and_decode() {
        let _g = T_LOCK.lock().unwrap();
        let da = FfiDataAccessor::new(42, mock_db_vtable());
        let rows = da
            .query_with_params(
                "select ? as c",
                &[serde_json::json!(1), serde_json::json!("x")],
            )
            .await
            .unwrap();
        assert_eq!(rows[0]["c"], serde_json::json!(1));
        let (h, sql, params) = DB_QUERY.lock().unwrap().clone();
        assert_eq!((h, sql.as_str()), (42, "select ? as c"));
        assert_eq!(params, r#"[1,"x"]"#);
        assert_eq!(da.exec_with_params("delete from t", &[]).await.unwrap(), 3);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn db_tx_commit_and_drop_does_not_rollback() {
        let _g = T_LOCK.lock().unwrap();
        DB_COMMITTED.store(0, AtomicOrdering::SeqCst);
        DB_ROLLED_BACK.store(0, AtomicOrdering::SeqCst);
        let da = FfiDataAccessor::new(42, mock_db_vtable());
        let tx = da.begin().await.unwrap();
        let rows = tx.query("select 1", &[]).await.unwrap();
        assert_eq!(rows[0]["c"], serde_json::json!(9));
        let (h, tid, sql, _) = DB_TX_QUERY.lock().unwrap().clone();
        assert_eq!((h, tid, sql.as_str()), (42, 7, "select 1"));
        tx.commit().await.unwrap();
        drop(tx);
        assert_eq!(DB_COMMITTED.load(AtomicOrdering::SeqCst), 7);
        assert_eq!(DB_ROLLED_BACK.load(AtomicOrdering::SeqCst), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn db_tx_drop_without_finish_fires_rollback() {
        let _g = T_LOCK.lock().unwrap();
        DB_ROLLED_BACK.store(0, AtomicOrdering::SeqCst);
        let da = FfiDataAccessor::new(42, mock_db_vtable());
        let tx = da.begin().await.unwrap();
        drop(tx); // 未 commit/rollback → drop 保底回滚
        assert_eq!(DB_ROLLED_BACK.load(AtomicOrdering::SeqCst), 7);
    }

    #[test]
    fn db_accessor_drop_closes_handle() {
        let _g = T_LOCK.lock().unwrap();
        DB_CLOSED.store(0, AtomicOrdering::SeqCst);
        drop(FfiDataAccessor::new(42, mock_db_vtable()));
        assert_eq!(DB_CLOSED.load(AtomicOrdering::SeqCst), 42);
    }

    // ---- blob 轴 mock vtable（Task 4.2；五方法转发 + Drop close）----

    use crate::bridge::{BlobBackend, BlobServed};

    static BLOB_PUT: Mutex<(u64, String, Vec<u8>, String)> =
        Mutex::new((0, String::new(), Vec::new(), String::new()));
    static BLOB_GET: Mutex<(u64, String)> = Mutex::new((0, String::new()));
    static BLOB_DEL: Mutex<(u64, String)> = Mutex::new((0, String::new()));
    static BLOB_URL: Mutex<(u64, String)> = Mutex::new((0, String::new()));
    static BLOB_CT: Mutex<(u64, String)> = Mutex::new((0, String::new()));
    static BLOB_CLOSED: AtomicU64 = AtomicU64::new(0);
    static BLOB_CT_EMPTY: AtomicBool = AtomicBool::new(false);
    /// blob 行为开关：1=报错；2=url/content_type 返回坏 UTF-8 字节。
    static BLOB_MODE: Mutex<u8> = Mutex::new(0);

    extern "C" fn mock_blob_connect(_name: RString, _cfg: RString) -> FfiFuture {
        ready(Ok(br#"{"handle":42}"#.to_vec()))
    }
    extern "C" fn mock_blob_put(
        handle: u64,
        key: RString,
        bytes: RBytes,
        ct: RString,
    ) -> FfiFuture {
        let mut b = Vec::with_capacity(bytes.len());
        for x in &bytes {
            b.push(*x);
        }
        *BLOB_PUT.lock().unwrap() = (handle, key[..].to_string(), b, ct[..].to_string());
        if *BLOB_MODE.lock().unwrap() == 1 {
            return ready(Err("put down".into()));
        }
        ready(Ok(b"".to_vec()))
    }
    extern "C" fn mock_blob_get(handle: u64, key: RString) -> FfiFuture {
        *BLOB_GET.lock().unwrap() = (handle, key[..].to_string());
        if *BLOB_MODE.lock().unwrap() == 1 {
            return ready(Err("get down".into()));
        }
        ready(Ok(b"blobdata".to_vec()))
    }
    extern "C" fn mock_blob_del(handle: u64, key: RString) -> FfiFuture {
        *BLOB_DEL.lock().unwrap() = (handle, key[..].to_string());
        if *BLOB_MODE.lock().unwrap() == 1 {
            return ready(Err("del down".into()));
        }
        ready(Ok(b"".to_vec()))
    }
    extern "C" fn mock_blob_url(handle: u64, key: RString) -> FfiFuture {
        *BLOB_URL.lock().unwrap() = (handle, key[..].to_string());
        match *BLOB_MODE.lock().unwrap() {
            1 => ready(Err("url down".into())),
            2 => ready(Ok(vec![0xff, 0xfe])),
            _ => ready(Ok(b"https://b.s3/presign".to_vec())),
        }
    }
    extern "C" fn mock_blob_content_type(handle: u64, key: RString) -> FfiFuture {
        *BLOB_CT.lock().unwrap() = (handle, key[..].to_string());
        match *BLOB_MODE.lock().unwrap() {
            1 => ready(Err("ct down".into())),
            2 => ready(Ok(vec![0xff, 0xfe])),
            _ if BLOB_CT_EMPTY.swap(false, AtomicOrdering::SeqCst) => ready(Ok(b"".to_vec())),
            _ => ready(Ok(b"image/png".to_vec())),
        }
    }
    extern "C" fn mock_blob_close(handle: u64) {
        BLOB_CLOSED.store(handle, AtomicOrdering::SeqCst);
    }

    fn mock_blob_vtable() -> &'static BlobBackendVtable {
        Box::leak(Box::new(BlobBackendVtable {
            connect: mock_blob_connect,
            put: mock_blob_put,
            get: mock_blob_get,
            del: mock_blob_del,
            url: mock_blob_url,
            content_type: mock_blob_content_type,
            close: mock_blob_close,
        }))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn blob_put_forwards_key_bytes_and_ct() {
        let _g = T_LOCK.lock().unwrap();
        let b = FfiBlobBackend::new(42, mock_blob_vtable());
        b.put("a/b.png", b"hello", Some("image/png")).await.unwrap();
        let (h, key, bytes, ct) = BLOB_PUT.lock().unwrap().clone();
        assert_eq!(
            (h, key.as_str(), bytes.as_slice()),
            (42, "a/b.png", &b"hello"[..])
        );
        assert_eq!(ct, "image/png");
        // None ct → 空串过线
        b.put("x", b"y", None).await.unwrap();
        let (_, _, _, ct) = BLOB_PUT.lock().unwrap().clone();
        assert_eq!(ct, "");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn blob_get_returns_bytes() {
        let _g = T_LOCK.lock().unwrap();
        let b = FfiBlobBackend::new(42, mock_blob_vtable());
        assert_eq!(b.get("k").await.unwrap(), b"blobdata");
        let (h, key) = BLOB_GET.lock().unwrap().clone();
        assert_eq!((h, key.as_str()), (42, "k"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn blob_del_succeeds() {
        let _g = T_LOCK.lock().unwrap();
        let b = FfiBlobBackend::new(42, mock_blob_vtable());
        b.del("k").await.unwrap();
        let (h, key) = BLOB_DEL.lock().unwrap().clone();
        assert_eq!((h, key.as_str()), (42, "k"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn blob_url_and_content_type_forward() {
        let _g = T_LOCK.lock().unwrap();
        let b = FfiBlobBackend::new(42, mock_blob_vtable());
        assert_eq!(b.url("k").await.unwrap(), "https://b.s3/presign");
        let (h, key) = BLOB_URL.lock().unwrap().clone();
        assert_eq!((h, key.as_str()), (42, "k"));
        assert_eq!(
            b.content_type("k").await.unwrap(),
            Some("image/png".to_string())
        );
        // 空串 → None
        BLOB_CT_EMPTY.store(true, AtomicOrdering::SeqCst);
        assert_eq!(b.content_type("k2").await.unwrap(), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn blob_serve_redirects_to_url() {
        let _g = T_LOCK.lock().unwrap();
        let b = FfiBlobBackend::new(42, mock_blob_vtable());
        assert!(
            matches!(b.serve("k").await.unwrap(), BlobServed::Redirect(u) if u == "https://b.s3/presign")
        );
    }

    #[test]
    fn blob_drop_calls_close() {
        let _g = T_LOCK.lock().unwrap();
        BLOB_CLOSED.store(0, AtomicOrdering::SeqCst);
        drop(FfiBlobBackend::new(42, mock_blob_vtable()));
        assert_eq!(BLOB_CLOSED.load(AtomicOrdering::SeqCst), 42);
    }

    // ---- bus 轴 mock vtable（Task 4.3；publish 转发 + deliver 扇出 + Drop close）----

    use crate::bridge::{BusBackend, EventBroker};
    use crate::config::BrokerCfg;

    static BUS_CONNECTED_CFG: Mutex<String> = Mutex::new(String::new());
    static BUS_PUBLISHED: Mutex<(u64, String, String)> =
        Mutex::new((0, String::new(), String::new()));
    static BUS_SUBSCRIBES: Mutex<Vec<(u64, String)>> = Mutex::new(Vec::new());
    static BUS_CLOSED: AtomicU64 = AtomicU64::new(0);
    /// TDD 开关：置位时 mock_bus_subscribe 先记录（模拟消费循环已起）再返回 Err，
    /// 用于验证 I-1 失败回滚（无僵尸注册）。
    static BUS_SUBSCRIBE_FAIL: AtomicBool = AtomicBool::new(false);
    /// bus connect 行为开关：1=报错；2=非 JSON；3=缺 handle。
    static BUS_CONNECT_MODE: Mutex<u8> = Mutex::new(0);
    /// TDD 开关：置位时 publish 报错（插件侧投递失败透传）。
    static BUS_PUBLISH_FAIL: AtomicBool = AtomicBool::new(false);

    extern "C" fn mock_bus_connect(cfg: RString) -> FfiFuture {
        *BUS_CONNECTED_CFG.lock().unwrap() = cfg[..].to_string();
        match *BUS_CONNECT_MODE.lock().unwrap() {
            1 => ready(Err("bus down".into())),
            2 => ready(Ok(b"gibberish".to_vec())),
            3 => ready(Ok(br#"{}"#.to_vec())),
            _ => ready(Ok(br#"{"handle":42}"#.to_vec())),
        }
    }
    extern "C" fn mock_bus_publish(handle: u64, topic: RString, data: RString) -> FfiFuture {
        *BUS_PUBLISHED.lock().unwrap() = (handle, topic[..].to_string(), data[..].to_string());
        if BUS_PUBLISH_FAIL.swap(false, AtomicOrdering::SeqCst) {
            return ready(Err("publish down".into()));
        }
        ready(Ok(b"".to_vec()))
    }
    extern "C" fn mock_bus_subscribe(handle: u64, topic: RString) -> FfiFuture {
        // 先记录（模拟插件侧已起消费循环），再按开关返回失败。
        BUS_SUBSCRIBES
            .lock()
            .unwrap()
            .push((handle, topic[..].to_string()));
        if BUS_SUBSCRIBE_FAIL.swap(false, AtomicOrdering::SeqCst) {
            return ready(Err("injected subscribe failure".into()));
        }
        ready(Ok(b"".to_vec()))
    }
    extern "C" fn mock_bus_close(handle: u64) {
        BUS_CLOSED.store(handle, AtomicOrdering::SeqCst);
    }

    fn mock_bus_vtable() -> &'static EventBrokerVtable {
        Box::leak(Box::new(EventBrokerVtable {
            connect: mock_bus_connect,
            publish: mock_bus_publish,
            subscribe: mock_bus_subscribe,
            close: mock_bus_close,
        }))
    }

    fn deliver_clear() {
        DELIVER_TARGETS.lock().unwrap().clear();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bus_backend_kind_and_connect() {
        let _g = T_LOCK.lock().unwrap();
        let be = FfiBusBackend::new("bus-kafka", mock_bus_vtable());
        assert_eq!(be.kind(), "kafka");
        let cfg = BrokerCfg {
            kind: "kafka".into(),
            brokers: vec!["b1:9092".into()],
            ..Default::default()
        };
        let broker = be.connect(&cfg).await.unwrap();
        assert_eq!(broker.kind(), "kafka");
        // 插件收到的 cfg JSON = BrokerCfg 序列化（brokers 数组）。
        let c = BUS_CONNECTED_CFG.lock().unwrap().clone();
        assert!(c.contains("b1:9092") && c.contains("kind"), "{c}");
        drop(broker);
        assert_eq!(BUS_CLOSED.load(AtomicOrdering::SeqCst), 42);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bus_publish_forwards_topic_and_frame_returns_zero() {
        let _g = T_LOCK.lock().unwrap();
        let broker = FfiEventBroker::new("kafka", 42, mock_bus_vtable());
        let n = broker
            .publish("news", &serde_json::json!({"a": 1}))
            .await
            .unwrap();
        assert_eq!(n, 0); // 远程 broker 本地 fan-out 恒 0
        let (h, topic, data) = BUS_PUBLISHED.lock().unwrap().clone();
        assert_eq!(h, 42);
        assert_eq!(topic, "news");
        let v: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(v["data"]["a"], 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bus_subscribe_registers_tx_and_deliver_fans_out() {
        let _g = T_LOCK.lock().unwrap();
        deliver_clear();
        BUS_SUBSCRIBES.lock().unwrap().clear();
        let broker = FfiEventBroker::new("kafka", 42, mock_bus_vtable());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        broker.subscribe("t", tx).await.unwrap();
        // vtable.subscribe 只起一次（每 topic 至多一个消费循环）。
        assert_eq!(BUS_SUBSCRIBES.lock().unwrap().len(), 1);
        // 模拟插件消费循环经 host.deliver 上送 → 扇出到本地 tx。
        host_deliver(
            RString::from("t"),
            RString::from(r#"{"topic":"t","data":{"v":42}}"#),
        );
        let frame = rx.try_recv().unwrap();
        let v: serde_json::Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(v["data"]["v"], 42);
        // 关闭接收端 → 后续 deliver 惰性清理（不 panic）。
        drop(rx);
        host_deliver(RString::from("t"), RString::from(r#"{}"#));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bus_subscribe_dedupes_same_channel_and_single_consumer_per_topic() {
        let _g = T_LOCK.lock().unwrap();
        deliver_clear();
        BUS_SUBSCRIBES.lock().unwrap().clear();
        let broker = FfiEventBroker::new("kafka", 42, mock_bus_vtable());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        broker.subscribe("t", tx.clone()).await.unwrap();
        broker.subscribe("t", tx).await.unwrap(); // 同 channel 去重
        let (tx2, _rx2) = tokio::sync::mpsc::unbounded_channel();
        broker.subscribe("u", tx2).await.unwrap(); // 不同 topic → 新消费
        assert_eq!(BUS_SUBSCRIBES.lock().unwrap().len(), 2); // t 一次 + u 一次
    }

    /// I-1 TDD：首次订阅 vtable 失败 → 返回 Err 且无僵尸注册（host_deliver 后通道空）。
    #[tokio::test(flavor = "multi_thread")]
    async fn bus_subscribe_failure_rolls_back_no_zombie() {
        let _g = T_LOCK.lock().unwrap();
        deliver_clear();
        BUS_SUBSCRIBES.lock().unwrap().clear();
        BUS_SUBSCRIBE_FAIL.store(false, AtomicOrdering::SeqCst);
        let broker = FfiEventBroker::new("kafka", 42, mock_bus_vtable());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        BUS_SUBSCRIBE_FAIL.store(true, AtomicOrdering::SeqCst); // 注入失败
        let res = broker.subscribe("t", tx).await;
        assert!(res.is_err(), "subscribe must propagate vtable error");
        // 无僵尸：回滚后该 topic 不应仍注册，host_deliver 不应扇出到通道。
        host_deliver(RString::from("t"), RString::from(r#"{"x":1}"#));
        assert!(
            rx.try_recv().is_err(),
            "zombie subscription must not deliver"
        );
        let g = DELIVER_TARGETS.lock().unwrap();
        let empty = match g.get("t") {
            None => true,
            Some(list) => list.is_empty(),
        };
        assert!(empty, "rolled-back topic must leave no deliver target");
    }

    /// I-1 TDD：失败重试成功 → 消费循环被重新起（BUS_SUBSCRIBES 记录两次）+ 扇出可达。
    #[tokio::test(flavor = "multi_thread")]
    async fn bus_subscribe_retries_after_failure_and_fanout() {
        let _g = T_LOCK.lock().unwrap();
        deliver_clear();
        BUS_SUBSCRIBES.lock().unwrap().clear();
        BUS_SUBSCRIBE_FAIL.store(false, AtomicOrdering::SeqCst);
        let broker = FfiEventBroker::new("kafka", 42, mock_bus_vtable());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        // 首次失败（回滚本地注册，但插件侧消费循环已起 → 记一次）。
        BUS_SUBSCRIBE_FAIL.store(true, AtomicOrdering::SeqCst);
        assert!(broker.subscribe("t", tx.clone()).await.is_err());
        // 重试成功。
        BUS_SUBSCRIBE_FAIL.store(false, AtomicOrdering::SeqCst);
        assert!(broker.subscribe("t", tx).await.is_ok());
        // 消费循环被重新起：两次 subscribe 各记一次。
        assert_eq!(BUS_SUBSCRIBES.lock().unwrap().len(), 2);
        // 扇出可达：host.deliver 经 DELIVER_TARGETS 扇到本通道。
        host_deliver(RString::from("t"), RString::from(r#"{"x":1}"#));
        let frame = rx.recv().await.unwrap();
        assert_eq!(frame, r#"{"x":1}"#);
    }

    #[test]
    fn bus_drop_closes_handle() {
        let _g = T_LOCK.lock().unwrap();
        BUS_CLOSED.store(0, AtomicOrdering::SeqCst);
        drop(FfiEventBroker::new("kafka", 42, mock_bus_vtable()));
        assert_eq!(BUS_CLOSED.load(AtomicOrdering::SeqCst), 42);
    }

    /// FFI broker 下"同一实例跨 actor/WS 共享"回归（Task 0.5 的插件 broker 形态）：
    /// 两个 Bridge 注入同一 FfiEventBroker，A 侧订阅、B 侧发布；远端经 deliver 回调
    /// 把消息扇回 → A 侧 tx 收到（全局 DELIVER_TARGETS 保持跨实例共享语义）。
    #[tokio::test(flavor = "multi_thread")]
    async fn ffi_broker_shared_across_bridges() {
        let _g = T_LOCK.lock().unwrap();
        deliver_clear();
        BUS_SUBSCRIBES.lock().unwrap().clear();
        let broker = Arc::new(FfiEventBroker::new("kafka", 42, mock_bus_vtable()));
        // A 侧订阅（如 WS 连接 A）。
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        broker.subscribe("t", tx).await.unwrap();
        // B 侧发布（同一实例）→ vtable.publish 转发（记录）。
        broker
            .publish("t", &serde_json::json!({"v": 7}))
            .await
            .unwrap();
        let (h, topic, _) = BUS_PUBLISHED.lock().unwrap().clone();
        assert_eq!((h, topic.as_str()), (42, "t"));
        // 模拟远端回程：插件消费循环经 host.deliver 上送 → A 侧 tx 收到（跨实例仍成立）。
        host_deliver(
            RString::from("t"),
            RString::from(r#"{"topic":"t","data":{"v":7}}"#),
        );
        let frame = rx.recv().await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(v["data"]["v"], 7, "{v}");
    }

    // ---- kv 轴 mock vtable（Task 4.4；五方法转发 + expire 秒取整 + Drop close）----

    use crate::bridge::KVStore;

    static KV_GET: Mutex<(u64, String)> = Mutex::new((0, String::new()));
    static KV_SET: Mutex<(u64, String, String)> = Mutex::new((0, String::new(), String::new()));
    static KV_DEL: Mutex<(u64, String)> = Mutex::new((0, String::new()));
    static KV_EXPIRE: Mutex<(u64, String, u64)> = Mutex::new((0, String::new(), 0));
    static KV_INCR: Mutex<(u64, String)> = Mutex::new((0, String::new()));
    static KV_CLOSED: AtomicU64 = AtomicU64::new(0);
    /// kv 行为开关：1=报错；2=get/expire/incr 返回非 JSON。
    static KV_MODE: Mutex<u8> = Mutex::new(0);

    extern "C" fn mock_kv_connect(_cfg: RString) -> FfiFuture {
        ready(Ok(br#"{"handle":42}"#.to_vec()))
    }
    extern "C" fn mock_kv_get(handle: u64, key: RString) -> FfiFuture {
        *KV_GET.lock().unwrap() = (handle, key[..].to_string());
        match *KV_MODE.lock().unwrap() {
            1 => ready(Err("get down".into())),
            2 => ready(Ok(b"{nope".to_vec())),
            _ => ready(Ok(br#""blobdata""#.to_vec())), // JSON Option<String>
        }
    }
    extern "C" fn mock_kv_set(handle: u64, key: RString, value: RString) -> FfiFuture {
        *KV_SET.lock().unwrap() = (handle, key[..].to_string(), value[..].to_string());
        if *KV_MODE.lock().unwrap() == 1 {
            return ready(Err("set down".into()));
        }
        ready(Ok(b"".to_vec()))
    }
    extern "C" fn mock_kv_del(handle: u64, key: RString) -> FfiFuture {
        *KV_DEL.lock().unwrap() = (handle, key[..].to_string());
        if *KV_MODE.lock().unwrap() == 1 {
            return ready(Err("del down".into()));
        }
        ready(Ok(b"".to_vec()))
    }
    extern "C" fn mock_kv_expire(handle: u64, key: RString, secs: u64) -> FfiFuture {
        *KV_EXPIRE.lock().unwrap() = (handle, key[..].to_string(), secs);
        match *KV_MODE.lock().unwrap() {
            1 => ready(Err("expire down".into())),
            2 => ready(Ok(b"{nope".to_vec())),
            _ => ready(Ok(b"true".to_vec())),
        }
    }
    extern "C" fn mock_kv_incr(handle: u64, key: RString) -> FfiFuture {
        *KV_INCR.lock().unwrap() = (handle, key[..].to_string());
        match *KV_MODE.lock().unwrap() {
            1 => ready(Err("incr down".into())),
            2 => ready(Ok(b"{nope".to_vec())),
            _ => ready(Ok(b"42".to_vec())),
        }
    }
    extern "C" fn mock_kv_close(handle: u64) {
        KV_CLOSED.store(handle, AtomicOrdering::SeqCst);
    }

    fn mock_kv_vtable() -> &'static KVStoreVtable {
        Box::leak(Box::new(KVStoreVtable {
            connect: mock_kv_connect,
            get: mock_kv_get,
            set: mock_kv_set,
            del: mock_kv_del,
            expire: mock_kv_expire,
            incr: mock_kv_incr,
            close: mock_kv_close,
        }))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn kv_get_returns_option_decoded_from_json() {
        let _g = T_LOCK.lock().unwrap();
        let kv = FfiKVStore::new(42, mock_kv_vtable());
        assert_eq!(kv.get("k").await.unwrap().as_deref(), Some("blobdata"));
        let (h, key) = KV_GET.lock().unwrap().clone();
        assert_eq!((h, key.as_str()), (42, "k"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn kv_set_and_del_forward() {
        let _g = T_LOCK.lock().unwrap();
        let kv = FfiKVStore::new(42, mock_kv_vtable());
        kv.set("k", "v").await.unwrap();
        let (h, key, value) = KV_SET.lock().unwrap().clone();
        assert_eq!((h, key.as_str(), value.as_str()), (42, "k", "v"));
        kv.del("k").await.unwrap();
        let (h, key) = KV_DEL.lock().unwrap().clone();
        assert_eq!((h, key.as_str()), (42, "k"));
    }

    /// expire 的 Duration 在宿主侧经 kv::expire_secs 向上取整到整秒再过线。
    #[tokio::test(flavor = "multi_thread")]
    async fn kv_expire_converts_duration_to_whole_seconds() {
        let _g = T_LOCK.lock().unwrap();
        let kv = FfiKVStore::new(42, mock_kv_vtable());
        // 500ms → 1s（Redis EXPIRE 不接受 0s——与 InMemoryKV 毫秒语义对齐）。
        assert!(
            kv.expire("k", std::time::Duration::from_millis(500))
                .await
                .unwrap()
        );
        let (h, key, secs) = KV_EXPIRE.lock().unwrap().clone();
        assert_eq!((h, key.as_str(), secs), (42, "k", 1));
        assert!(
            kv.expire("k", std::time::Duration::from_secs(2))
                .await
                .unwrap()
        );
        assert_eq!(KV_EXPIRE.lock().unwrap().2, 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn kv_incr_returns_i64_decoded_from_json() {
        let _g = T_LOCK.lock().unwrap();
        let kv = FfiKVStore::new(42, mock_kv_vtable());
        assert_eq!(kv.incr("k").await.unwrap(), 42);
        let (h, key) = KV_INCR.lock().unwrap().clone();
        assert_eq!((h, key.as_str()), (42, "k"));
    }

    #[test]
    fn kv_drop_calls_close() {
        let _g = T_LOCK.lock().unwrap();
        KV_CLOSED.store(0, AtomicOrdering::SeqCst);
        drop(FfiKVStore::new(42, mock_kv_vtable()));
        assert_eq!(KV_CLOSED.load(AtomicOrdering::SeqCst), 42);
    }

    // ---- auth 轴 mock vtable（Task auth-1；同步 RResult 直返，无 FfiFuture）----

    /// auth verify 行为开关：0=对象用户；1=null（匿名放行）；2=标量；3=坏 JSON；4=Err。
    static AUTH_MODE: Mutex<u8> = Mutex::new(0);
    static AUTH_GOT: Mutex<(String, String)> = Mutex::new((String::new(), String::new()));

    extern "C" fn mock_auth_verify(
        path: RString,
        authorization: RString,
    ) -> RResult<RString, RString> {
        *AUTH_GOT.lock().unwrap() = (path[..].to_string(), authorization[..].to_string());
        match *AUTH_MODE.lock().unwrap() {
            1 => RResult::Ok(RString::from("null")),
            2 => RResult::Ok(RString::from("5")),
            3 => RResult::Ok(RString::from("{nope")),
            4 => RResult::Err(RString::from("token expired")),
            _ => RResult::Ok(RString::from(r#"{"id":"u1"}"#)),
        }
    }

    fn mock_auth_vtable() -> &'static oj_plugin_ffi::AuthGuardVtable {
        Box::leak(Box::new(oj_plugin_ffi::AuthGuardVtable {
            verify: mock_auth_verify,
        }))
    }

    // ---- 补测：await_ffi 协议边界 / auth 守卫契约 / 各轴错误臂 ----

    use crate::bridge::AuthGuard;

    /// 断言 BridgeResult 为 Err 并取错误文案（Ok 型多为非 Debug 的 dyn Trait 适配器，无法 unwrap_err）。
    fn expect_err<T>(r: BridgeResult<T>) -> String {
        match r {
            Ok(_) => panic!("expected error"),
            Err(e) => e.to_string(),
        }
    }

    /// loader 错误文本 → 分类启发式的全关键词臂：平台/架构类逐词命中
    /// PlatformMismatch，其余落 DependencyResolution（分类只影响文案，不影响 fail-fast）。
    #[test]
    fn given_loader_error_texts_when_classified_then_platform_or_dependency() {
        let p = Path::new("/x/plugin.dylib");
        for text in [
            "architecture mismatch",
            "incompatible architecture",
            "mach-o, but wrong file type",
            "wrong ELF class: ELFCLASS32",
            "wrong ELF data format",
            "version `GLIBC_2.38' not found",
            "not a mach-o image",
            "file too short",
            "%1 is not a valid Win32 application",
        ] {
            assert!(
                matches!(
                    classify_load_error(p, text),
                    PluginLoadError::PlatformMismatch { .. }
                ),
                "{text}"
            );
        }
        assert!(matches!(
            classify_load_error(p, "undefined symbol: _xyz"),
            PluginLoadError::DependencyResolution { .. }
        ));
    }

    /// es search 返回非 JSON → decode 错误臂；index_doc/delete_doc 插件报错 → 透传点名调用。
    #[tokio::test(flavor = "multi_thread")]
    async fn given_es_error_variants_when_search_index_delete_then_errs_name_the_call() {
        let _g = T_LOCK.lock().unwrap();
        let b = FfiEsBackend::new(1, mock_vtable());
        {
            let _m = Mode::set(&ES_MODE, 2);
            let e = b
                .search("i", serde_json::json!({}))
                .await
                .unwrap_err()
                .to_string();
            assert!(e.contains("search decode"), "{e}");
        }
        {
            let _m = Mode::set(&ES_MODE, 3);
            let e = b
                .index_doc("i", "7", serde_json::json!({}))
                .await
                .unwrap_err()
                .to_string();
            assert!(e.contains("index_doc") && e.contains("index boom"), "{e}");
        }
        {
            let _m = Mode::set(&ES_MODE, 4);
            let e = b.delete_doc("i", "7").await.unwrap_err().to_string();
            assert!(e.contains("delete_doc") && e.contains("delete boom"), "{e}");
        }
    }

    // ---- auth 守卫契约（匿名/用户/坏插件输出/拒签）----

    #[test]
    fn given_auth_plugin_returns_user_object_when_verify_then_user_parsed_and_header_forwarded() {
        let _g = T_LOCK.lock().unwrap();
        let _m = Mode::set(&AUTH_MODE, 0);
        let guard = FfiAuthGuard::new(mock_auth_vtable());
        let user = guard.verify("/v1/api/u/", Some("Bearer tok1")).unwrap();
        assert_eq!(user.unwrap()["id"], "u1");
        let (path, authz) = AUTH_GOT.lock().unwrap().clone();
        assert_eq!(
            (path.as_str(), authz.as_str()),
            ("/v1/api/u/", "Bearer tok1")
        );
    }

    /// 匿名契约：插件回 null → Ok(None) 放行不注入；无 Authorization 头 → 空串过线。
    #[test]
    fn given_anonymous_path_when_verify_then_none_and_empty_authorization_forwarded() {
        let _g = T_LOCK.lock().unwrap();
        let _m = Mode::set(&AUTH_MODE, 1);
        let guard = FfiAuthGuard::new(mock_auth_vtable());
        assert_eq!(guard.verify("/p", None).unwrap(), None);
        let (_, authz) = AUTH_GOT.lock().unwrap().clone();
        assert_eq!(authz, "");
    }

    /// 坏插件输出（标量 / 非 JSON）→ Err 点名契约违约，不静默当匿名放行。
    #[test]
    fn given_auth_plugin_speaks_gibberish_when_verify_then_contract_violation_errs() {
        let _g = T_LOCK.lock().unwrap();
        let guard = FfiAuthGuard::new(mock_auth_vtable());
        let _m2 = Mode::set(&AUTH_MODE, 2);
        let e = guard.verify("/p", None).unwrap_err();
        assert!(e.contains("non-object user"), "{e}");
        let _m3 = Mode::set(&AUTH_MODE, 3);
        let e = guard.verify("/p", None).unwrap_err();
        assert!(e.contains("auth plugin returned bad json"), "{e}");
    }

    /// 插件拒签（Err 臂）→ 错误文案原样透传（401 消息契约）。
    #[test]
    fn given_auth_plugin_rejects_when_verify_then_err_carries_plugin_message() {
        let _g = T_LOCK.lock().unwrap();
        let _m = Mode::set(&AUTH_MODE, 4);
        let guard = FfiAuthGuard::new(mock_auth_vtable());
        let e = guard.verify("/p", Some("Bearer bad")).unwrap_err();
        assert_eq!(e, "token expired");
    }

    // ---- db 轴错误臂 ----

    /// db connect 三类失败：插件报错 / 非 JSON（decode）/ 缺 handle——文案点名阶段。
    #[tokio::test(flavor = "multi_thread")]
    async fn given_db_connect_failures_when_connect_then_errs_name_the_stage() {
        let _g = T_LOCK.lock().unwrap();
        let be = FfiDbBackend::new("db-mysql", mock_db_vtable());
        {
            let _m = Mode::set(&DB_CONNECT_MODE, 1);
            let e = expect_err(be.connect("dsn", std::path::Path::new("/tmp")).await);
            assert!(e.contains("db connect") && e.contains("db down"), "{e}");
        }
        {
            let _m = Mode::set(&DB_CONNECT_MODE, 2);
            let e = expect_err(be.connect("dsn", std::path::Path::new("/tmp")).await);
            assert!(e.contains("db connect decode"), "{e}");
        }
        {
            let _m = Mode::set(&DB_CONNECT_MODE, 3);
            let e = expect_err(be.connect("dsn", std::path::Path::new("/tmp")).await);
            assert!(e.contains("db connect: missing handle"), "{e}");
        }
    }

    /// begin 三类失败：插件报错 / 非 JSON（decode）/ 缺 tx_id。
    #[tokio::test(flavor = "multi_thread")]
    async fn given_db_begin_failures_when_begin_then_errs_name_the_stage() {
        let _g = T_LOCK.lock().unwrap();
        let da = FfiDataAccessor::new(42, mock_db_vtable());
        {
            let _m = Mode::set(&DB_BEGIN_MODE, 1);
            let e = expect_err(da.begin().await);
            assert!(e.contains("db begin") && e.contains("begin down"), "{e}");
        }
        {
            let _m = Mode::set(&DB_BEGIN_MODE, 2);
            let e = expect_err(da.begin().await);
            assert!(e.contains("db begin decode"), "{e}");
        }
        {
            let _m = Mode::set(&DB_BEGIN_MODE, 3);
            let e = expect_err(da.begin().await);
            assert!(e.contains("db begin: missing tx_id"), "{e}");
        }
    }

    /// query/exec/tx_query/tx_exec 插件报错 → 文案点名具体调用。
    #[tokio::test(flavor = "multi_thread")]
    async fn given_db_calls_fail_when_query_exec_tx_then_errs_name_the_call() {
        let _g = T_LOCK.lock().unwrap();
        let _m = Mode::set(&DB_CALL_MODE, 1);
        let da = FfiDataAccessor::new(42, mock_db_vtable());
        let e = da
            .query_with_params("s", &[])
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("db query") && e.contains("call down"), "{e}");
        let e = da.exec_with_params("s", &[]).await.unwrap_err().to_string();
        assert!(e.contains("db exec"), "{e}");
        let tx = da.begin().await.unwrap();
        let e = tx.query("s", &[]).await.unwrap_err().to_string();
        assert!(e.contains("db tx_query"), "{e}");
        let e = tx.exec("s", &[]).await.unwrap_err().to_string();
        assert!(e.contains("db tx_exec"), "{e}");
    }

    /// query/exec/tx_query/tx_exec 返回非 JSON → decode 错误臂。
    #[tokio::test(flavor = "multi_thread")]
    async fn given_db_calls_return_gibberish_when_query_exec_tx_then_decode_errs() {
        let _g = T_LOCK.lock().unwrap();
        let _m = Mode::set(&DB_CALL_MODE, 2);
        let da = FfiDataAccessor::new(42, mock_db_vtable());
        let e = da
            .query_with_params("s", &[])
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("db query decode"), "{e}");
        let e = da.exec_with_params("s", &[]).await.unwrap_err().to_string();
        assert!(e.contains("db exec decode"), "{e}");
        let tx = da.begin().await.unwrap();
        let e = tx.query("s", &[]).await.unwrap_err().to_string();
        assert!(e.contains("db tx_query decode"), "{e}");
        let e = tx.exec("s", &[]).await.unwrap_err().to_string();
        assert!(e.contains("db tx_exec decode"), "{e}");
    }

    /// 方言自报映射：mysql → MySql；未知方言兜底 Sqlite（宿主不改插件契约）。
    #[test]
    fn given_dialect_variants_when_reported_then_mapped_with_sqlite_fallback() {
        let _g = T_LOCK.lock().unwrap();
        let da = FfiDataAccessor::new(42, mock_db_vtable());
        {
            let _m = Mode::set(&DB_DIALECT_MODE, 1);
            assert_eq!(da.dialect(), Dialect::MySql);
        }
        {
            let _m = Mode::set(&DB_DIALECT_MODE, 2);
            assert_eq!(da.dialect(), Dialect::Sqlite);
        }
    }

    /// commit 失败 → Err 且事务视为未完结：drop 保底回滚仍触发（ReqState reset
    /// 丢弃存活事务 = 保底回滚语义的 FFI 保留，见 FfiTxSession::drop）。
    #[tokio::test(flavor = "multi_thread")]
    async fn given_tx_commit_fails_when_dropped_then_guaranteed_rollback_still_fires() {
        let _g = T_LOCK.lock().unwrap();
        DB_COMMITTED.store(0, AtomicOrdering::SeqCst);
        DB_ROLLED_BACK.store(0, AtomicOrdering::SeqCst);
        let _m = Mode::set(&DB_TX_END_MODE, 1);
        let da = FfiDataAccessor::new(42, mock_db_vtable());
        let tx = da.begin().await.unwrap();
        let e = tx.commit().await.unwrap_err().to_string();
        assert!(
            e.contains("db tx_commit") && e.contains("commit down"),
            "{e}"
        );
        drop(tx);
        assert_eq!(DB_ROLLED_BACK.load(AtomicOrdering::SeqCst), 7);
    }

    /// rollback 失败 → Err 透传（finished 未置位，drop 会再补一次回滚尝试）。
    #[tokio::test(flavor = "multi_thread")]
    async fn given_tx_rollback_fails_when_rollback_then_err_names_rollback() {
        let _g = T_LOCK.lock().unwrap();
        DB_ROLLED_BACK.store(0, AtomicOrdering::SeqCst);
        let _m = Mode::set(&DB_TX_END_MODE, 1);
        let da = FfiDataAccessor::new(42, mock_db_vtable());
        let tx = da.begin().await.unwrap();
        let e = tx.rollback().await.unwrap_err().to_string();
        assert!(
            e.contains("db tx_rollback") && e.contains("rollback down"),
            "{e}"
        );
    }

    // ---- blob 轴错误臂 ----

    /// blob 五方法插件报错 → 错误文案点名操作；serve 走 url → 错误透传。
    #[tokio::test(flavor = "multi_thread")]
    async fn given_blob_plugin_down_when_put_get_del_serve_then_errs_name_the_operation() {
        let _g = T_LOCK.lock().unwrap();
        let _m = Mode::set(&BLOB_MODE, 1);
        let b = FfiBlobBackend::new(42, mock_blob_vtable());
        let e = b.put("k", b"x", None).await.unwrap_err().to_string();
        assert!(e.contains("blob put") && e.contains("put down"), "{e}");
        let e = b.get("k").await.unwrap_err().to_string();
        assert!(e.contains("blob get"), "{e}");
        let e = b.del("k").await.unwrap_err().to_string();
        assert!(e.contains("blob del"), "{e}");
        let e = expect_err(b.serve("k").await);
        assert!(e.contains("blob url"), "{e}");
    }

    /// url/content_type 返回坏 UTF-8 字节 → decode 错误臂。
    #[tokio::test(flavor = "multi_thread")]
    async fn given_blob_gibberish_bytes_when_url_or_content_type_then_decode_errs() {
        let _g = T_LOCK.lock().unwrap();
        let _m = Mode::set(&BLOB_MODE, 2);
        let b = FfiBlobBackend::new(42, mock_blob_vtable());
        let e = b.url("k").await.unwrap_err().to_string();
        assert!(e.contains("blob url decode"), "{e}");
        let e = b.content_type("k").await.unwrap_err().to_string();
        assert!(e.contains("blob content_type decode"), "{e}");
    }

    // ---- bus 轴错误臂 ----

    /// bus connect 三类失败：插件报错 / 非 JSON（decode）/ 缺 handle。
    #[tokio::test(flavor = "multi_thread")]
    async fn given_bus_connect_failures_when_connect_then_errs_name_the_stage() {
        let _g = T_LOCK.lock().unwrap();
        let be = FfiBusBackend::new("bus-kafka", mock_bus_vtable());
        let cfg = BrokerCfg {
            kind: "kafka".into(),
            brokers: vec![],
            ..Default::default()
        };
        {
            let _m = Mode::set(&BUS_CONNECT_MODE, 1);
            let e = expect_err(be.connect(&cfg).await);
            assert!(e.contains("bus connect") && e.contains("bus down"), "{e}");
        }
        {
            let _m = Mode::set(&BUS_CONNECT_MODE, 2);
            let e = expect_err(be.connect(&cfg).await);
            assert!(e.contains("bus connect decode"), "{e}");
        }
        {
            let _m = Mode::set(&BUS_CONNECT_MODE, 3);
            let e = expect_err(be.connect(&cfg).await);
            assert!(e.contains("bus connect: missing handle"), "{e}");
        }
    }

    /// publish 插件报错 → 透传点名 bus publish（本地 fan-out 0 只在成功路径）。
    #[tokio::test(flavor = "multi_thread")]
    async fn given_bus_publish_fails_when_publish_then_err_names_publish() {
        let _g = T_LOCK.lock().unwrap();
        BUS_PUBLISH_FAIL.store(true, AtomicOrdering::SeqCst);
        let broker = FfiEventBroker::new("kafka", 42, mock_bus_vtable());
        let e = broker
            .publish("t", &serde_json::json!({}))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("bus publish") && e.contains("publish down"),
            "{e}"
        );
    }

    /// kind 推断：插件名无 "bus-" 前缀时取全名（unwrap_or 臂）。
    #[test]
    fn given_backend_name_without_bus_prefix_when_new_then_kind_is_full_name() {
        let _g = T_LOCK.lock().unwrap();
        let be = FfiBusBackend::new("redis", mock_bus_vtable());
        assert_eq!(be.kind(), "redis");
    }

    // ---- kv 轴错误臂 ----

    /// kv 五方法插件报错 → 错误文案点名操作。
    #[tokio::test(flavor = "multi_thread")]
    async fn given_kv_plugin_down_when_ops_then_errs_name_the_operation() {
        let _g = T_LOCK.lock().unwrap();
        let _m = Mode::set(&KV_MODE, 1);
        let kv = FfiKVStore::new(42, mock_kv_vtable());
        assert!(
            kv.get("k")
                .await
                .unwrap_err()
                .to_string()
                .contains("kv get")
        );
        assert!(
            kv.set("k", "v")
                .await
                .unwrap_err()
                .to_string()
                .contains("kv set")
        );
        assert!(
            kv.del("k")
                .await
                .unwrap_err()
                .to_string()
                .contains("kv del")
        );
        assert!(
            kv.expire("k", std::time::Duration::from_secs(1))
                .await
                .unwrap_err()
                .to_string()
                .contains("kv expire")
        );
        assert!(
            kv.incr("k")
                .await
                .unwrap_err()
                .to_string()
                .contains("kv incr")
        );
    }

    /// kv get/expire/incr 返回非 JSON → decode 错误臂。
    #[tokio::test(flavor = "multi_thread")]
    async fn given_kv_gibberish_when_get_expire_incr_then_decode_errs() {
        let _g = T_LOCK.lock().unwrap();
        let _m = Mode::set(&KV_MODE, 2);
        let kv = FfiKVStore::new(42, mock_kv_vtable());
        assert!(
            kv.get("k")
                .await
                .unwrap_err()
                .to_string()
                .contains("kv get decode")
        );
        assert!(
            kv.expire("k", std::time::Duration::from_secs(1))
                .await
                .unwrap_err()
                .to_string()
                .contains("kv expire decode")
        );
        assert!(
            kv.incr("k")
                .await
                .unwrap_err()
                .to_string()
                .contains("kv incr decode")
        );
    }
}

/// `await_ffi_poll`（mq 长轮询退避变体）测试：计数 pending 的假 future。
#[cfg(test)]
mod await_ffi_poll_tests {
    use super::*;
    use oj_plugin_ffi::{RBytes, RResult};
    use std::ffi::c_void;
    use std::time::Duration;

    struct CountedState {
        left: u32,
    }

    extern "C" fn counted_poll(state: *mut c_void) -> i32 {
        let s = unsafe { &mut *(state as *mut CountedState) };
        if s.left == 0 {
            1
        } else {
            s.left -= 1;
            0
        }
    }

    extern "C" fn counted_take(state: *mut c_void) -> RResult<RBytes, RString> {
        let s = unsafe { &*(state as *mut CountedState) };
        let _ = s; // 只读；释放由 free 统一负责（await_ffi_poll take→free 配对）
        let mut v = RBytes::new();
        for b in b"ok" {
            v.push(*b);
        }
        RResult::Ok(v)
    }

    extern "C" fn counted_free(state: *mut c_void) {
        if !state.is_null() {
            drop(unsafe { Box::from_raw(state as *mut CountedState) });
        }
    }

    fn counted_future(left: u32) -> FfiFuture {
        let state = Box::into_raw(Box::new(CountedState { left }));
        FfiFuture {
            state: state.cast(),
            poll: counted_poll,
            take: counted_take,
            free: counted_free,
        }
    }

    /// 20 次 pending × 10ms 退避：总耗时 ≥ 180ms 证明在睡眠而非 yield_now 空转。
    #[tokio::test]
    async fn given_many_pending_polls_when_await_ffi_poll_then_backs_off_not_spins() {
        let t0 = std::time::Instant::now();
        let out = await_ffi_poll(counted_future(20), Duration::from_millis(10))
            .await
            .unwrap();
        assert_eq!(out, b"ok".to_vec());
        assert!(
            t0.elapsed().as_millis() >= 180,
            "no backoff: {:?}",
            t0.elapsed()
        );
    }

    /// 就绪 future 不额外睡眠（退避只发生在 pending 间隙）。
    #[tokio::test]
    async fn given_ready_future_when_await_ffi_poll_then_returns_without_sleep() {
        let t0 = std::time::Instant::now();
        let out = await_ffi_poll(counted_future(0), Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(out, b"ok".to_vec());
        assert!(t0.elapsed().as_millis() < 100, "{:?}", t0.elapsed());
    }

    // ---- poll=-1（错误码）但 take 结果各异的协议边界 ----

    extern "C" fn err_poll(_state: *mut c_void) -> i32 {
        -1
    }
    extern "C" fn ok_take(_state: *mut c_void) -> RResult<RBytes, RString> {
        let mut v = RBytes::new();
        for b in b"ok" {
            v.push(*b);
        }
        RResult::Ok(v)
    }
    extern "C" fn err_take(_state: *mut c_void) -> RResult<RBytes, RString> {
        RResult::Err(RString::from("late boom"))
    }
    fn code_future(take: extern "C" fn(*mut c_void) -> RResult<RBytes, RString>) -> FfiFuture {
        FfiFuture {
            state: std::ptr::null_mut(),
            poll: err_poll,
            take,
            free: counted_free,
        }
    }

    /// poll=-1 但 take=Ok → 协议违约兜底文案（不把成功结果误当错误细节）。
    #[tokio::test]
    async fn given_error_code_but_ok_take_when_await_ffi_then_reports_protocol_violation() {
        let e = await_ffi(code_future(ok_take)).await.unwrap_err();
        assert!(
            e.contains("ffi poll reported error but take succeeded"),
            "{e}"
        );
    }

    /// await_ffi_poll 同款兜底（mq 长轮询变体保持同一协议语义）。
    #[tokio::test]
    async fn given_error_code_but_ok_take_when_await_ffi_poll_then_reports_protocol_violation() {
        let e = await_ffi_poll(code_future(ok_take), Duration::from_millis(1))
            .await
            .unwrap_err();
        assert!(
            e.contains("ffi poll reported error but take succeeded"),
            "{e}"
        );
    }

    /// poll=-1 且 take=Err → 插件错误文案透传（await_ffi_poll 的错误臂）。
    #[tokio::test]
    async fn given_error_code_and_err_take_when_await_ffi_poll_then_err_carries_plugin_message() {
        let e = await_ffi_poll(code_future(err_take), Duration::from_millis(1))
            .await
            .unwrap_err();
        assert_eq!(e, "late boom");
    }
}
