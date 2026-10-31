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
use crate::bridge::db_error::{
    BridgeError, DbErrorCategory, SessionDisposition, parse_raw, unsupported,
};
use crate::bridge::dialect_caps::DialectInfo;
use crate::bridge::{
    BlobBackend, BlobServed, BridgeResult, BusBackend, BusPayload, DbBackend, EsBackend,
    EventBroker, KVStore, WsSend,
};
use crate::config::BrokerCfg;
use futures::stream::Stream;
use oj_plugin_ffi::{
    BlobBackendVtable, DataAccessorVtable, EsBackendVtable, EventBrokerVtable, FfiFuture,
    KVStoreVtable, RBytes, RString,
};
use serde_json::json;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::mpsc::UnboundedSender;

/// pending 期转入睡眠前的 yield 预算：前 64 次 poll 走 yield_now（快操作零延迟），
/// 之后改为睡眠退避（慢查询不再单核空转——yield_now 空转版 db/es/blob/kv/bus 全轴中招）。
const FFI_SPIN_BUDGET: u32 = 64;

/// 超出 yield 预算后的每次 poll 睡眠时长（500µs：相对插件往返可忽略，CPU 近似 idle）。
const FFI_AWAIT_BACKOFF: std::time::Duration = std::time::Duration::from_micros(500);

/// FfiFuture → host async 桥（S.2 定稿形态：poll 轮询 + 自适应退避；take→free→state 置 null）。
/// poll 返回 -1 时也 take（错误细节在 take 的 Err 里）。
/// 经 FfiGuard 持有：await 被取消时 Drop 只 free 不 take（放弃结果，插件任务允许跑完）。
pub(crate) async fn await_ffi(fut: FfiFuture) -> Result<Vec<u8>, String> {
    let mut guard = FfiGuard(Some(fut));
    let mut polls: u32 = 0;
    loop {
        let fut = guard.0.as_mut().expect("fut present until return");
        match (fut.poll)(fut.state) {
            0 => {
                polls += 1;
                if polls <= FFI_SPIN_BUDGET {
                    tokio::task::yield_now().await;
                } else {
                    tokio::time::sleep(FFI_AWAIT_BACKOFF).await;
                }
            }
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

/// `await_ffi_poll` 的轮询间隔（给 mail 的 `submit` 用）：2ms 的折中取值 —— 比
/// `yield_now` 空转（原实现，SMTP 往返可达 30s ⇒ 烧满一核）低五个量级的 CPU，
/// 又只给每次投递加 ≤2ms 的固定尾延迟（相对 SMTP 往返可忽略）。
/// mq 的取值（10ms）与它的长轮询语义绑定，留在调用点（`oj/src/app.rs` 的 `BACKOFF`）。
pub(crate) const FFI_POLL_BACKOFF: std::time::Duration = std::time::Duration::from_millis(2);

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
        method: &str,
        authorization: Option<&str>,
        headers: Option<&str>,
    ) -> Result<Option<serde_json::Value>, String> {
        let r = (self.vtable.verify)(
            RString::from(path_no_base),
            RString::from(method),
            RString::from(authorization.unwrap_or("")),
            RString::from(headers.unwrap_or("")),
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
    /// 装配期一次解析的能力闭集（T5：描述符冻结，运行期纯查表）。
    dialect_info: DialectInfo,
}

impl FfiDbBackend {
    /// 构造即调 vtable.schemes() / describe() 读认领列表与能力（装配期一次）。
    pub fn new(
        name: impl Into<String>,
        vtable: &'static DataAccessorVtable,
        dialect_info: DialectInfo,
    ) -> Self {
        let schemes: Vec<String> = (vtable.schemes)()
            .iter()
            .map(|s| s[..].to_string())
            .collect();
        Self {
            name: name.into(),
            schemes,
            vtable,
            dialect_info,
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
        let bytes = await_ffi(fut).await.map_err(|e| Box::new(parse_raw(&e)))?;
        let handle = serde_json::from_slice::<serde_json::Value>(&bytes)
            .map_err(|e| ffi_err("db connect decode", e))?
            .get("handle")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| ffi_err("db connect", "missing handle"))?;
        // 连接级一致性校验（T5）：describe().dialect 必须与插件运行时 dialect(handle) 一致，
        // 不一致 fail-fast 拒绝该连接（与 scheme 冲突同风格）。
        let runtime_dialect = (self.vtable.dialect)(handle);
        if runtime_dialect[..] != self.dialect_info.name {
            return Err(ffi_err(
                "db connect",
                format!(
                    "dialect mismatch: describe()=\"{}\" but dialect(handle)=\"{}\"",
                    self.dialect_info.name,
                    &runtime_dialect[..]
                ),
            ));
        }
        Ok(Arc::new(FfiDataAccessor::new(
            handle,
            self.vtable,
            self.dialect_info.clone(),
        )))
    }
}

/// 连接适配器：实现 core DataAccessor，经 vtable + FfiFuture 转发（handle 查表在插件侧）。
pub struct FfiDataAccessor {
    handle: u64,
    vtable: &'static DataAccessorVtable,
    /// 方言描述符（装配期经工厂 describe() 冻结，T5：不再 per-connect 派生，删除 Sqlite 兜底）。
    info: DialectInfo,
}

impl FfiDataAccessor {
    /// 由工厂携带的 `DialectInfo` 构造（describe() 已解析过，运行期纯查表）。
    pub fn new(handle: u64, vtable: &'static DataAccessorVtable, info: DialectInfo) -> Self {
        Self {
            handle,
            vtable,
            info,
        }
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
        // 描述符名 → known 子集。未知方言必须由上层门控点（T8）先行拦截，
        // 此处不再静默回落 Sqlite；若出现未知方言说明门控遗漏，直接 fail-loud。
        Dialect::known(&self.info.name).unwrap_or_else(|| {
            panic!(
                "dialect '{}' 不在 builder-known 子集；必须先在门控点拦截，不可回落 Sqlite",
                self.info.name
            )
        })
    }

    fn dialect_info(&self) -> &DialectInfo {
        &self.info
    }

    async fn begin(&self) -> BridgeResult<Box<dyn TxSession>> {
        let fut = (self.vtable.begin)(self.handle);
        let bytes = await_ffi(fut).await.map_err(|e| Box::new(parse_raw(&e)))?;
        let tx_id = serde_json::from_slice::<serde_json::Value>(&bytes)
            .map_err(|e| ffi_err("db begin decode", e))?
            .get("tx_id")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| ffi_err("db begin", "missing tx_id"))?;
        Ok(Box::new(FfiTxSession::new(
            self.handle,
            tx_id,
            self.vtable,
            self.info.clone(),
        )))
    }

    async fn query_with_params(
        &self,
        sql: &str,
        params: &[serde_json::Value],
    ) -> BridgeResult<Vec<Row>> {
        let p = params_json(params)?;
        let fut = (self.vtable.query)(self.handle, RString::from(sql), p);
        let bytes = await_ffi(fut).await.map_err(|e| Box::new(parse_raw(&e)))?;
        serde_json::from_slice(&bytes).map_err(|e| ffi_err("db query decode", e))
    }

    async fn exec_with_params(&self, sql: &str, params: &[serde_json::Value]) -> BridgeResult<i64> {
        let p = params_json(params)?;
        let fut = (self.vtable.exec)(self.handle, RString::from(sql), p);
        let bytes = await_ffi(fut).await.map_err(|e| Box::new(parse_raw(&e)))?;
        serde_json::from_slice(&bytes).map_err(|e| ffi_err("db exec decode", e))
    }

    /// 协作式取消：转发到 vtable `stream_cancel`。能力矩阵（T10）：
    /// - 声明 cancel 却返回 unsupported → 契约违反，fail-loud（不让调用方误以为已取消）。
    /// - 未声明 cancel 但插件确实提供了取消 → 采信 + warn（best-effort 取消本就允许静默成功）。
    async fn cancel(&self, stream_id: u64) -> BridgeResult<()> {
        let fut = (self.vtable.stream_cancel)(self.handle, stream_id);
        let bytes = await_ffi(fut).await.map_err(|e| Box::new(parse_raw(&e)))?;
        // 空体是「已取消/无需载荷」的合法回执（mysql/pg 插件即返回 `Ok(b"")`）——
        // 不能当解码错误，否则「声明 cancel + 提供 cancel」象限永远失败。
        let v: serde_json::Value = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).map_err(|e| ffi_err("db stream_cancel decode", e))?
        };
        if v.get("unsupported")
            .and_then(|b| b.as_bool())
            .unwrap_or(false)
        {
            if self.info.caps.cancel {
                return Err(unsupported(&self.info.name, "cancel").into());
            }
            return Ok(());
        }
        if !self.info.caps.cancel {
            tracing::warn!(
                target: "db",
                backend = %self.info.name,
                "cancel provided but capability 'cancel' not declared in describe(); accepting (add \"cancel\")"
            );
        }
        Ok(())
    }

    /// ABI 10 流式：经 vtable `stream_open/next` 批量 pull（≤100 行/次）。
    /// - open 返回哨兵 `{"unsupported":true}`（旧插件/不支持流式的后端）→ 回落有界
    ///   fetch_all（= db.query 全量，评审定稿：保 dev/test 一致）。
    /// - 游标清理：正常走完（done/err）只 fire `stream_close`；被 abort 丢弃（pump
    ///   收到 Notify 后 drop 生成器）→ 先 `stream_cancel`（真取消，插件协作式）再
    ///   close。两路都 fire-and-forget（future.rs 契约：宿主 drop 句柄=放弃结果，
    ///   插件任务照常跑完），杜绝 aborted 查询泄漏插件侧游标条目。
    /// REQ-11 G1/G2：插件轴可取消查询——宿主侧零 ABI 实现：复用既有 vtable
    /// `stream_open/next/cancel/close`（PG/MySQL 声明 `stream cancel` 能力）。
    /// 取消 = drop 行流：`StreamCursorGuard` fire-and-forget `stream_cancel`
    /// （插件真杀服务端语句/关专用连接）。无流能力的旧插件走 stream_query 内部的
    /// fetch_all 兜底（cancel 尽力而为，同弃 FfiFuture 的诚实口径）。
    /// exec 的可取消版插件轴不可达（affected 计数无流式载体）——默认委托，ABI 13 车。
    async fn query_cancelable(
        &self,
        sql: &str,
        params: &[serde_json::Value],
        meter: &mut super::db_quota::ByteMeter,
        shared: &super::db::PoolExecShared,
    ) -> BridgeResult<Vec<Row>> {
        use std::sync::atomic::Ordering;
        if shared.cancel_requested.load(Ordering::SeqCst) {
            return Err(super::db_error::cancelled_error(&shared.alias).into());
        }
        let mut s = self.stream_query(sql, params).await?;
        super::accessor_sqlx::push_event(shared, "sent", None);
        let mut out = Vec::new();
        let mut first = true;
        use futures::StreamExt;
        loop {
            // Final-fix F2：sticky 标志循环头复查（notify_waiters 单发陷阱——行臂
            // 同刻赢 select 即吞掉唤醒；与 accessor_sqlx 同口径）。
            if shared
                .cancel_requested
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                drop(s); // guard drop → stream_cancel 真杀服务端语句
                return Err(super::db_error::cancelled_error(&shared.alias).into());
            }
            tokio::select! {
                row = s.next() => {
                    match row {
                        None => break,
                        Some(Ok(v)) => {
                            if first {
                                super::accessor_sqlx::push_event(shared, "fetching", None);
                                first = false;
                            }
                            if meter.feed(&v) == super::db_quota::Feed::Stop {
                                break;
                            }
                            out.push(v);
                        }
                        Some(Err(e)) => {
                            return Err(Box::new(BridgeError {
                                category: DbErrorCategory::Internal,
                                retryable: false,
                                session_disposition:
                                    SessionDisposition::Close,
                                message: e,
                                data: Default::default(),
                            }));
                        }
                    }
                }
                _ = shared.notify.notified() => {
                    // fire_cancel 先置标志后广播——保守再校验。
                    if shared.cancel_requested.load(Ordering::SeqCst) {
                        drop(s); // guard drop → stream_cancel 真杀服务端语句
                        return Err(super::db_error::cancelled_error(&shared.alias).into());
                    }
                }
            }
        }
        super::accessor_sqlx::push_event(shared, "done", Some(out.len() as u64));
        Ok(out)
    }

    async fn stream_query(
        &self,
        sql: &str,
        params: &[serde_json::Value],
    ) -> BridgeResult<Pin<Box<dyn Stream<Item = Result<Row, String>> + Send>>> {
        let p = params_json(params)?;
        let fut = (self.vtable.stream_open)(self.handle, RString::from(sql), p);
        let bytes = await_ffi(fut).await.map_err(|e| Box::new(parse_raw(&e)))?;
        let v: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|e| ffi_err("db stream_open decode", e))?;
        if v.get("unsupported")
            .and_then(|b| b.as_bool())
            .unwrap_or(false)
        {
            // 能力一致性矩阵（T10）：
            // - 声明 stream 却返回 unsupported → 契约违反，op 级 unsupported（不回退 fetch_all）。
            // - 未声明 stream → 标准兜底 fetch_all（保留既有行为，兼容旧插件）。
            if self.info.caps.stream {
                return Err(unsupported(&self.info.name, "stream").into());
            }
            let rows = self.query_with_params(sql, params).await?;
            return Ok(Box::pin(futures::stream::iter(
                rows.into_iter().map(Ok::<Row, String>),
            )));
        }
        // 插件实际提供了流：未声明能力 → 采信 + warn（插件能力超出声明，容忍但点名）。
        if !self.info.caps.stream {
            tracing::warn!(
                target: "db",
                backend = %self.info.name,
                "streaming provided but capability 'stream' not declared in describe(); accepting (add \"stream\")"
            );
        }
        let stream_id = v
            .get("stream_id")
            .and_then(|x| x.as_u64())
            .ok_or_else(|| ffi_err("db stream_open", "missing stream_id"))?;
        let handle = self.handle;
        let vt = self.vtable;
        let guard = Arc::new(StreamCursorGuard {
            handle,
            vtable: vt,
            stream_id,
            completed: AtomicBool::new(false),
        });
        let g = guard.clone();
        let row_stream = async_stream::stream! {
            loop {
                let fut = (vt.stream_next)(handle, stream_id);
                match await_ffi(fut).await {
                    Err(e) => {
                        yield Err(ffi_err("db stream_next", e).to_string());
                        break;
                    }
                    Ok(bytes) => {
                        let env: serde_json::Value = match serde_json::from_slice(&bytes) {
                            Ok(v) => v,
                            Err(e) => {
                                yield Err(ffi_err("db stream_next decode", e).to_string());
                                break;
                            }
                        };
                        if let Some(err) = env.get("error") {
                            // 设计 §5.1：stream_next 的 error 字段与 db 轴错误信封同构。
                            // 插件可给信封对象 / 信封 JSON 串 / legacy 纯文本，一律经
                            // parse_raw 归一；交给上游的是 **message 原文**（不再拼
                            // `ffi <ctx>: ` 前缀，也不把 JSON 塞进用户可见文本）。
                            let be = match err {
                                serde_json::Value::String(s) if s == "cancelled" => BridgeError {
                                    category: DbErrorCategory::Cancelled,
                                    retryable: false,
                                    session_disposition: SessionDisposition::Keep,
                                    message: "cancelled".to_string(),
                                    data: Default::default(),
                                },
                                serde_json::Value::String(s) => parse_raw(s),
                                other => parse_raw(&other.to_string()),
                            };
                            yield Err(be.message);
                            break;
                        }
                        if env.get("done").and_then(|x| x.as_bool()).unwrap_or(false) {
                            break;
                        }
                        if let Some(rows) = env.get("rows").and_then(|x| x.as_array()) {
                            for r in rows {
                                yield Ok(r.clone());
                            }
                        }
                    }
                }
            }
            g.completed.store(true, Ordering::SeqCst);
        };
        drop(guard); // 生成器持有唯一引用；生成器 drop（走完或被 abort 丢弃）即收口
        Ok(Box::pin(row_stream))
    }
}

/// FFI db 流式游标清理守卫（见 `FfiDataAccessor::stream_query` 注释）。
struct StreamCursorGuard {
    handle: u64,
    vtable: &'static DataAccessorVtable,
    stream_id: u64,
    /// 生成器走完（done/err）后置位：Drop 只 reclaim（close）不 cancel。
    completed: AtomicBool,
}

impl Drop for StreamCursorGuard {
    fn drop(&mut self) {
        if !self.completed.load(Ordering::SeqCst) {
            let f = (self.vtable.stream_cancel)(self.handle, self.stream_id);
            drop(FfiGuard(Some(f))); // fire-and-forget：插件任务照常执行
        }
        let f = (self.vtable.stream_close)(self.handle, self.stream_id);
        drop(FfiGuard(Some(f)));
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
    /// 事务级方言描述符（begin 时从 accessor 完整快照；in-tx 改写/exec 据此查表）。
    info: DialectInfo,
}

impl FfiTxSession {
    fn new(
        handle: u64,
        tx_id: u64,
        vtable: &'static DataAccessorVtable,
        info: DialectInfo,
    ) -> Self {
        Self {
            handle,
            tx_id,
            vtable,
            finished: AtomicBool::new(false),
            info,
        }
    }
}

#[async_trait::async_trait]
impl TxSession for FfiTxSession {
    fn dialect_info(&self) -> &DialectInfo {
        &self.info
    }

    async fn query(&self, sql: &str, params: &[serde_json::Value]) -> BridgeResult<Vec<Row>> {
        let p = params_json(params)?;
        let fut = (self.vtable.tx_query)(self.handle, self.tx_id, RString::from(sql), p);
        let bytes = await_ffi(fut).await.map_err(|e| Box::new(parse_raw(&e)))?;
        serde_json::from_slice(&bytes).map_err(|e| ffi_err("db tx_query decode", e))
    }

    async fn exec(&self, sql: &str, params: &[serde_json::Value]) -> BridgeResult<i64> {
        let p = params_json(params)?;
        let fut = (self.vtable.tx_exec)(self.handle, self.tx_id, RString::from(sql), p);
        let bytes = await_ffi(fut).await.map_err(|e| Box::new(parse_raw(&e)))?;
        serde_json::from_slice(&bytes).map_err(|e| ffi_err("db tx_exec decode", e))
    }

    async fn commit(&self) -> BridgeResult<()> {
        let fut = (self.vtable.tx_commit)(self.handle, self.tx_id);
        await_ffi(fut).await.map_err(|e| Box::new(parse_raw(&e)))?;
        self.finished.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn rollback(&self) -> BridgeResult<()> {
        let fut = (self.vtable.tx_rollback)(self.handle, self.tx_id);
        await_ffi(fut).await.map_err(|e| Box::new(parse_raw(&e)))?;
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

/// Vec<u8> → RBytes（逐元素 push；等价于 `RBytes::from(&bytes[..])`——stabby-abi 实现了
/// `impl<T: Copy, Alloc: IAlloc + Default> From<&[T]> for Vec<T, Alloc>`。此处保留逐元素
/// 形态仅为不动 blob 既有路径；早先「stabby 无 From<&[u8]>」的注释与实测不符，已订正）。
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

    async fn upload_url(&self, key: &str, op: &str) -> BridgeResult<serde_json::Value> {
        let fut = (self.vtable.upload_url)(self.handle, RString::from(key), RString::from(op));
        let bytes = await_ffi(fut)
            .await
            .map_err(|e| ffi_err("blob upload_url", e))?;
        serde_json::from_slice(&bytes).map_err(|e| ffi_err("blob upload_url decode", e))
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

    /// ABI 10 流式上传转发（open 产 upload_id；chunk/finish/abort 直透）。
    async fn put_stream_open(&self, key: &str, content_type: Option<&str>) -> BridgeResult<u64> {
        let ct = RString::from(content_type.unwrap_or(""));
        let fut = (self.vtable.put_stream_open)(self.handle, RString::from(key), ct);
        let bytes = await_ffi(fut)
            .await
            .map_err(|e| ffi_err("blob put_stream_open", e))?;
        let v: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|e| ffi_err("blob put_stream_open decode", e))?;
        v.get("upload_id")
            .and_then(|x| x.as_u64())
            .ok_or_else(|| ffi_err("blob put_stream_open", "missing upload_id"))
    }

    async fn put_stream_chunk(&self, upload_id: u64, bytes: &[u8]) -> BridgeResult<()> {
        let fut = (self.vtable.put_stream_chunk)(self.handle, upload_id, to_rbytes(bytes));
        await_ffi(fut)
            .await
            .map_err(|e| ffi_err("blob put_stream_chunk", e))?;
        Ok(())
    }

    async fn put_stream_finish(&self, upload_id: u64) -> BridgeResult<()> {
        let fut = (self.vtable.put_stream_finish)(self.handle, upload_id);
        await_ffi(fut)
            .await
            .map_err(|e| ffi_err("blob put_stream_finish", e))?;
        Ok(())
    }

    async fn put_stream_abort(&self, upload_id: u64) -> BridgeResult<()> {
        let fut = (self.vtable.put_stream_abort)(self.handle, upload_id);
        await_ffi(fut)
            .await
            .map_err(|e| ffi_err("blob put_stream_abort", e))?;
        Ok(())
    }

    /// ABI 11：服务端复制转发。vtable 报错 = 后端无服务端复制能力 → 回落 `get` + `put`
    /// （字节进 V8，仅作能力保险）；回落也失败则抛 vtable 原错误（更贴近根因）。
    async fn copy(&self, src: &str, dst: &str) -> BridgeResult<()> {
        let fut = (self.vtable.copy)(self.handle, RString::from(src), RString::from(dst));
        if let Err(e) = await_ffi(fut).await {
            let err = ffi_err("blob copy", e);
            if let Ok(bytes) = self.get(src).await {
                let ct = self.content_type(src).await.unwrap_or(None);
                return self.put(dst, &bytes, ct.as_deref()).await;
            }
            return Err(err);
        }
        Ok(())
    }

    /// ABI 11：服务端搬移转发。报错回落 `copy` + `del`（copy 自身还有一层回落）。
    async fn move_to(&self, src: &str, dst: &str) -> BridgeResult<()> {
        let fut = (self.vtable.move_to)(self.handle, RString::from(src), RString::from(dst));
        if let Err(e) = await_ffi(fut).await {
            let err = ffi_err("blob move", e);
            if self.copy(src, dst).await.is_ok() {
                return self.del(src).await;
            }
            return Err(err);
        }
        Ok(())
    }

    /// ABI 11：区间读转发（短读截断由插件侧负责）。报错回落 `get` 全量再切片。
    async fn read_range(&self, key: &str, offset: u64, len: u64) -> BridgeResult<Vec<u8>> {
        let fut = (self.vtable.read_range)(self.handle, RString::from(key), offset, len);
        match await_ffi(fut).await {
            Ok(bytes) => Ok(bytes),
            Err(e) => {
                let err = ffi_err("blob readRange", e);
                match self.get(key).await {
                    Ok(all) => Ok(slice_range(&all, offset, len)),
                    Err(_) => Err(err),
                }
            }
        }
    }
}

/// 全量字节 → 区间切片（短读截断：`offset` 过尾返回空，越尾只读到 EOF）。
fn slice_range(all: &[u8], offset: u64, len: u64) -> Vec<u8> {
    let size = all.len() as u64;
    let start = offset.min(size) as usize;
    let end = offset.saturating_add(len).min(size) as usize;
    all[start..end].to_vec()
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
    std::sync::Mutex<HashMap<String, Vec<UnboundedSender<WsSend>>>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

/// DELIVER_TARGETS 加锁（毒化自愈）：持锁线程 panic 毒化后，deliver 在插件线程
/// 继续 `unwrap` 会连环 panic → abort 进程；恢复到 panic 时刻的部分状态对扇出表无害
/// （满/closed 条目本就惰性清理），故宁可带伤服务也不放大成进程崩溃。
fn deliver_targets() -> std::sync::MutexGuard<'static, HashMap<String, Vec<UnboundedSender<WsSend>>>>
{
    DELIVER_TARGETS.lock().unwrap_or_else(|e| e.into_inner())
}

/// host 侧 deliver 回调（HostContext.deliver 指向此）：非阻塞投递，满/closed 惰性清理。
/// 语义 = Bus::publish 的本地扇出（payload 原样转发；按 topic 去重注册）。
/// 帧型判定（v0.1.16 wire 约定，与 FfiEventBroker::publish 的封装对称）：payload 为
/// UTF-8 且是含 topic/data 两键的 JSON 对象 → 信封文本帧；否则 → 二进制帧原字节。
///
/// `mail.result` 例外：走宿主 mail 路由（存结果 + 扁平化扇出），不经帧型启发式——
/// 上送的是 `{code,msg,data:{jobId,…}}` 结果信封而非 bus 帧。
pub(crate) extern "C" fn host_deliver(topic: RString, payload: RBytes) {
    let raw = payload[..].to_vec();
    if &topic[..] == super::mail::MAIL_RESULT_TOPIC {
        // 四种失败成因分开告警（A6 + B3）：排障方向不同 —— 「没配 mail」是部署问题，
        // 「载荷非法」是插件上送的内容不符合结果信封契约，「未知 jobId」是 id 不是宿主
        // 开出的票（伪造/旧插件自造），「重复」是同一 jobId 二次上送（已拒绝覆盖）。
        match super::mail::route_deliver(&raw) {
            super::mail::DeliverRoute::Routed => {}
            super::mail::DeliverRoute::NotConfigured => {
                eprintln!("warn: mail.result 上送但 mail 未配置（结果丢弃）");
            }
            super::mail::DeliverRoute::BadPayload => {
                eprintln!("warn: mail.result 载荷非法（缺 jobId/code/msg 或非 JSON）——结果丢弃");
            }
            super::mail::DeliverRoute::UnknownJob => {
                eprintln!("warn: mail.result 的 jobId 未经宿主登记（非本宿主开出的票）——结果丢弃");
            }
            super::mail::DeliverRoute::Duplicate => {
                eprintln!("warn: mail.result 同一 jobId 重复上送——拒绝覆盖，保留先到的结果");
            }
        }
        return;
    }
    fanout_targets(&topic[..], envelope_or_binary(&raw));
}

/// 本地扇出一帧到 `DELIVER_TARGETS`（满/closed 惰性清理），返回投递成功数。
/// `host_deliver` 与 `FfiEventBroker::publish_local` 共用（同步路径）。
pub(crate) fn fanout_targets(topic: &str, frame: WsSend) -> usize {
    let mut g = deliver_targets();
    let mut n = 0;
    if let Some(list) = g.get_mut(topic) {
        list.retain(|tx| {
            if tx.send(frame.clone()).is_ok() {
                n += 1;
                true
            } else {
                false
            }
        });
        if list.is_empty() {
            g.remove(topic);
        }
    }
    n
}

/// bus 帧型判定（启发式，详见 host_deliver 注释）：`{"topic":…,"data":…}` JSON
/// 对象 → Text(envelope 原文)；其余 → Binary 原字节。误判面 = 二进制载荷恰为该
/// 形状的 JSON（无害：以文本帧投递，客户端按 topic 过滤）。
pub(crate) fn envelope_or_binary(raw: &[u8]) -> WsSend {
    match std::str::from_utf8(raw)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
    {
        Some(v) if v.is_object() && v.get("topic").is_some() && v.get("data").is_some() => {
            WsSend::Text(String::from_utf8_lossy(raw).into_owned())
        }
        _ => WsSend::Binary(raw.to_vec()),
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
    subs: StdMutex<Vec<(String, UnboundedSender<WsSend>)>>,
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

    async fn publish(&self, topic: &str, data: &BusPayload) -> BridgeResult<usize> {
        // wire 约定（v0.1.16）：JSON → `{"topic","data"}` 信封 UTF-8（旧生产者兼容）；
        // 字节 → 原样透传。消费侧 host_deliver 按同款启发式还原帧型。
        let payload = match data {
            BusPayload::Json(v) => json!({ "topic": topic, "data": v })
                .to_string()
                .into_bytes(),
            BusPayload::Bytes(b) => b.clone(),
        };
        let fut = (self.vtable.publish)(self.handle, RString::from(topic), to_rbytes(&payload));
        await_ffi(fut)
            .await
            .map_err(|e| ffi_err("bus publish", e))?;
        Ok(0) // 远程 broker 经网络投递，本地 fan-out 恒 0（语义对齐 core Kafka/Rabbit）。
    }

    /// 同步本地扇出：本 broker 的本地订阅者都注册在 `DELIVER_TARGETS`，此处按与
    /// `publish` 相同的 wire 封装直接投递（供 `deliver` 回调等非 async 上下文）。
    fn publish_local(&self, topic: &str, data: &BusPayload) -> usize {
        let frame = match data {
            BusPayload::Json(v) => WsSend::Text(json!({ "topic": topic, "data": v }).to_string()),
            BusPayload::Bytes(b) => WsSend::Binary(b.clone()),
        };
        fanout_targets(topic, frame)
    }

    async fn subscribe(&self, topic: &str, tx: UnboundedSender<WsSend>) -> BridgeResult<()> {
        // 全程持门禁：vtable.subscribe 是「本地注册 + 起消费循环」的原子单元；
        // 失败回滚刚注册的通道，避免僵尸注册导致该 topic 静默丢失。
        let _gate = SUBSCRIBE_GATE.lock().await;
        let (is_new_topic, inserted) = {
            let mut g = deliver_targets();
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
                    let mut g = deliver_targets();
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
        let mut g = deliver_targets();
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

/// `await_ffi_poll`（mq 长轮询退避变体）测试：计数 pending 的假 future。
#[cfg(test)]
#[path = "ffi_await_poll_tests.rs"]
mod await_ffi_poll_tests;

#[cfg(test)]
#[path = "ffi_adapter_tests.rs"]
#[allow(clippy::await_holding_lock)]
mod adapter_tests;
