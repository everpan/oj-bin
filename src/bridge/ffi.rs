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

use crate::bridge::db::{DataAccessor, Dialect, Row, TxSession};
use crate::bridge::{
    BlobBackend, BlobServed, BridgeResult, BusBackend, DbBackend, EsBackend, EventBroker,
};
use crate::config::BrokerCfg;
use oj_plugin_ffi::{
    BlobBackendVtable, DataAccessorVtable, EsBackendVtable, EventBrokerVtable, FfiFuture, RBytes,
    RString,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
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
        let schemes: Vec<String> = (vtable.schemes)().iter().map(|s| s[..].to_string()).collect();
        Self { name: name.into(), schemes, vtable }
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
fn params_json(params: &[serde_json::Value]) -> Result<RString, Box<dyn std::error::Error + Send + Sync>> {
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

    async fn query_with_params(&self, sql: &str, params: &[serde_json::Value]) -> BridgeResult<Vec<Row>> {
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
        Self { handle, tx_id, vtable, finished: AtomicBool::new(false) }
    }
}

#[async_trait::async_trait]
impl TxSession for FfiTxSession {
    async fn query(&self, sql: &str, params: &[serde_json::Value]) -> BridgeResult<Vec<Row>> {
        let p = params_json(params)?;
        let fut = (self.vtable.tx_query)(self.handle, self.tx_id, RString::from(sql), p);
        let bytes = await_ffi(fut).await.map_err(|e| ffi_err("db tx_query", e))?;
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
        await_ffi(fut).await.map_err(|e| ffi_err("db tx_commit", e))?;
        self.finished.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn rollback(&self) -> BridgeResult<()> {
        let fut = (self.vtable.tx_rollback)(self.handle, self.tx_id);
        await_ffi(fut).await.map_err(|e| ffi_err("db tx_rollback", e))?;
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
        let bytes = await_ffi(fut).await.map_err(|e| ffi_err("blob content_type", e))?;
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
        let bytes = await_ffi(fut).await.map_err(|e| ffi_err("bus connect", e))?;
        let handle = serde_json::from_slice::<serde_json::Value>(&bytes)
            .map_err(|e| ffi_err("bus connect decode", e))?
            .get("handle")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| ffi_err("bus connect", "missing handle"))?;
        Ok(Arc::new(FfiEventBroker::new(self.kind, handle, self.vtable)))
    }
}

/// broker 适配器：实现 core EventBroker。subscribe 本地注册 tx + 每 topic 至多一个
/// 插件消费循环（vtable.subscribe 幂等去重）；插件收到消息经 host.deliver 上送 →
/// 全局 DELIVER_TARGETS 按 topic 扇出（跨 actor/WS 共享语义与内置 Bus 一致）。
pub struct FfiEventBroker {
    kind: &'static str,
    handle: u64,
    vtable: &'static EventBrokerVtable,
}

impl FfiEventBroker {
    pub fn new(kind: &'static str, handle: u64, vtable: &'static EventBrokerVtable) -> Self {
        Self { kind, handle, vtable }
    }
}

#[async_trait::async_trait]
impl EventBroker for FfiEventBroker {
    fn kind(&self) -> &'static str {
        self.kind
    }

    async fn publish(&self, topic: &str, data: &Value) -> BridgeResult<usize> {
        let frame = json!({ "topic": topic, "data": data }).to_string();
        let fut =
            (self.vtable.publish)(self.handle, RString::from(topic), RString::from(frame.as_str()));
        await_ffi(fut).await.map_err(|e| ffi_err("bus publish", e))?;
        Ok(0) // 远程 broker 经网络投递，本地 fan-out 恒 0（语义对齐 core Kafka/Rabbit）。
    }

    async fn subscribe(&self, topic: &str, tx: UnboundedSender<String>) -> BridgeResult<()> {
        // 本地注册（同 channel 去重）；仅当该 topic 首次出现时起插件消费循环。
        let start_consumer = {
            let mut g = DELIVER_TARGETS.lock().unwrap();
            let list = g.entry(topic.to_string()).or_default();
            let is_new_topic = list.is_empty();
            if !list.iter().any(|t| t.same_channel(&tx)) {
                list.push(tx);
            }
            is_new_topic
        };
        if start_consumer {
            let fut = (self.vtable.subscribe)(self.handle, RString::from(topic));
            await_ffi(fut).await.map_err(|e| ffi_err("bus subscribe", e))?;
        }
        Ok(())
    }
}

impl Drop for FfiEventBroker {
    fn drop(&mut self) {
        (self.vtable.close)(self.handle);
        // 清理本 broker 注册的本地目标（topic 扇出表进程级共享；进程退出即回收）。
        DELIVER_TARGETS.lock().unwrap().clear();
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

    extern "C" fn mock_db_connect(cfg: RString) -> FfiFuture {
        *DB_CONNECTED_CFG.lock().unwrap() = cfg[..].to_string();
        ready(Ok(br#"{"handle":42}"#.to_vec()))
    }
    extern "C" fn mock_db_query(handle: u64, sql: RString, params: RString) -> FfiFuture {
        *DB_QUERY.lock().unwrap() = (handle, sql[..].to_string(), params[..].to_string());
        ready(Ok(br#"[{"c":1,"t":"a"}]"#.to_vec()))
    }
    extern "C" fn mock_db_exec(_h: u64, _s: RString, _p: RString) -> FfiFuture {
        ready(Ok(br#"3"#.to_vec()))
    }
    extern "C" fn mock_db_begin(_handle: u64) -> FfiFuture {
        ready(Ok(br#"{"tx_id":7}"#.to_vec()))
    }
    extern "C" fn mock_db_tx_query(
        handle: u64,
        tx_id: u64,
        sql: RString,
        params: RString,
    ) -> FfiFuture {
        *DB_TX_QUERY.lock().unwrap() = (handle, tx_id, sql[..].to_string(), params[..].to_string());
        ready(Ok(br#"[{"c":9}]"#.to_vec()))
    }
    extern "C" fn mock_db_tx_exec(_h: u64, _t: u64, _s: RString, _p: RString) -> FfiFuture {
        ready(Ok(br#"1"#.to_vec()))
    }
    extern "C" fn mock_db_tx_commit(_h: u64, tx_id: u64) -> FfiFuture {
        DB_COMMITTED.store(tx_id, AtomicOrdering::SeqCst);
        ready(Ok(b"".to_vec()))
    }
    extern "C" fn mock_db_tx_rollback(_h: u64, tx_id: u64) -> FfiFuture {
        DB_ROLLED_BACK.store(tx_id, AtomicOrdering::SeqCst);
        ready(Ok(b"".to_vec()))
    }
    extern "C" fn mock_db_dialect(_handle: u64) -> RString {
        RString::from("postgres")
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
        let da = be.connect("mysql://u:p@h/d", std::path::Path::new("/tmp")).await.unwrap();
        assert_eq!(*DB_CONNECTED_CFG.lock().unwrap(), "mysql://u:p@h/d");
        assert_eq!(da.dialect(), Dialect::Postgres);
        assert_eq!(da.query_with_params("select 1", &[]).await.unwrap()[0]["c"], serde_json::json!(1));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn db_query_and_exec_forward_params_and_decode() {
        let _g = T_LOCK.lock().unwrap();
        let da = FfiDataAccessor::new(42, mock_db_vtable());
        let rows = da
            .query_with_params("select ? as c", &[serde_json::json!(1), serde_json::json!("x")])
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

    extern "C" fn mock_blob_connect(_name: RString, _cfg: RString) -> FfiFuture {
        ready(Ok(br#"{"handle":42}"#.to_vec()))
    }
    extern "C" fn mock_blob_put(handle: u64, key: RString, bytes: RBytes, ct: RString) -> FfiFuture {
        let mut b = Vec::with_capacity(bytes.len());
        for x in &bytes {
            b.push(*x);
        }
        *BLOB_PUT.lock().unwrap() = (handle, key[..].to_string(), b, ct[..].to_string());
        ready(Ok(b"".to_vec()))
    }
    extern "C" fn mock_blob_get(handle: u64, key: RString) -> FfiFuture {
        *BLOB_GET.lock().unwrap() = (handle, key[..].to_string());
        ready(Ok(b"blobdata".to_vec()))
    }
    extern "C" fn mock_blob_del(handle: u64, key: RString) -> FfiFuture {
        *BLOB_DEL.lock().unwrap() = (handle, key[..].to_string());
        ready(Ok(b"".to_vec()))
    }
    extern "C" fn mock_blob_url(handle: u64, key: RString) -> FfiFuture {
        *BLOB_URL.lock().unwrap() = (handle, key[..].to_string());
        ready(Ok(b"https://b.s3/presign".to_vec()))
    }
    extern "C" fn mock_blob_content_type(handle: u64, key: RString) -> FfiFuture {
        *BLOB_CT.lock().unwrap() = (handle, key[..].to_string());
        if BLOB_CT_EMPTY.swap(false, AtomicOrdering::SeqCst) {
            ready(Ok(b"".to_vec()))
        } else {
            ready(Ok(b"image/png".to_vec()))
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
        assert_eq!((h, key.as_str(), bytes.as_slice()), (42, "a/b.png", &b"hello"[..]));
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
        assert_eq!(b.content_type("k").await.unwrap(), Some("image/png".to_string()));
        // 空串 → None
        BLOB_CT_EMPTY.store(true, AtomicOrdering::SeqCst);
        assert_eq!(b.content_type("k2").await.unwrap(), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn blob_serve_redirects_to_url() {
        let _g = T_LOCK.lock().unwrap();
        let b = FfiBlobBackend::new(42, mock_blob_vtable());
        assert!(matches!(b.serve("k").await.unwrap(), BlobServed::Redirect(u) if u == "https://b.s3/presign"));
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
    static BUS_PUBLISHED: Mutex<(u64, String, String)> = Mutex::new((0, String::new(), String::new()));
    static BUS_SUBSCRIBES: Mutex<Vec<(u64, String)>> = Mutex::new(Vec::new());
    static BUS_CLOSED: AtomicU64 = AtomicU64::new(0);

    extern "C" fn mock_bus_connect(cfg: RString) -> FfiFuture {
        *BUS_CONNECTED_CFG.lock().unwrap() = cfg[..].to_string();
        ready(Ok(br#"{"handle":42}"#.to_vec()))
    }
    extern "C" fn mock_bus_publish(handle: u64, topic: RString, data: RString) -> FfiFuture {
        *BUS_PUBLISHED.lock().unwrap() = (handle, topic[..].to_string(), data[..].to_string());
        ready(Ok(b"".to_vec()))
    }
    extern "C" fn mock_bus_subscribe(handle: u64, topic: RString) -> FfiFuture {
        BUS_SUBSCRIBES.lock().unwrap().push((handle, topic[..].to_string()));
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
        let cfg = BrokerCfg { kind: "kafka".into(), brokers: vec!["b1:9092".into()], ..Default::default() };
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
        let n = broker.publish("news", &serde_json::json!({"a": 1})).await.unwrap();
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
        host_deliver(RString::from("t"), RString::from(r#"{"topic":"t","data":{"v":42}}"#));
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
        broker.publish("t", &serde_json::json!({"v": 7})).await.unwrap();
        let (h, topic, _) = BUS_PUBLISHED.lock().unwrap().clone();
        assert_eq!((h, topic.as_str()), (42, "t"));
        // 模拟远端回程：插件消费循环经 host.deliver 上送 → A 侧 tx 收到（跨实例仍成立）。
        host_deliver(RString::from("t"), RString::from(r#"{"topic":"t","data":{"v":7}}"#));
        let frame = rx.recv().await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(v["data"]["v"], 7, "{v}");
    }
}
