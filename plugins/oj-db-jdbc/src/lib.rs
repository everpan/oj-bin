//! oj-db-jdbc：db 轴通用 JDBC 插件（design spec §5）。
//! 形态：Rust cdylib 插件 + Java sidecar（`java/jdbc-bridge/`，NDJSON 帧协议 §5.3）：
//! drivers_dir 全部 jar 经 URLClassLoader 装载，DriverShim 过 DriverManager 类加载器白名单。
//! T7 crate 骨架 + DSN 解析（`dsn`）；T10：vtable connect/query/exec/tx/close 胶水
//! 经 SidecarMgr 编排（jsint 大整数通道见 `prep_params`）；T11：stream 四槽
//! （stream 注册表 + cancelled ok 值归一 Err 映射）。
//! 仍占位：JDBC→JSON 类型映射细化是 T12。
//!
//! 假 java 夹具（spec §6）：`tests/sidecar.rs` 是 harness=false 集成测试——自有 main 按
//! env `OJ_FAKE_JAVA=<场景>` 自重现为假 java（spawn `current_exe()`，跨平台含 Windows CI），
//! 否则顺序跑 SidecarMgr 场景。不用 shell 脚本。
//! （不用「unit test 自重现」：stable rust 无法拦截 test harness 的 main——
//!   `reexport_test_harness_main` 属 unstable 的 custom_test_frameworks。）

pub mod codec;
pub mod dsn;
pub mod errmap;
pub mod sidecar;

use oj_plugin_ffi::{
    ABI_VERSION, DataAccessorVtable, FfiFuture, HostContext, PluginDescriptor, RArc, RResult,
    RString, RVec,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// 装配期 cfg 暂存（`plugins: oj-db-jdbc:` 段透传 JSON：`drivers_dir` / `java_bin` /
/// `java_opts`，spec §5.2）。T10 解析使用；DSN 本体在 connect 按值传入（db 插件统一契约）。
static CFG: OnceLock<RString> = OnceLock::new();

/// SidecarMgr 进程级单例（spec §5.9：v1 单 JVM 单例；多厂商同机需求出现时变 keyed）。
/// init 期注入 cfg；首次 request 惰性 spawn java sidecar。
static SIDECAR: OnceLock<sidecar::SidecarMgr> = OnceLock::new();

/// T10（vtable connect/query 编排）经此取 SidecarMgr。
pub(crate) fn sidecar() -> Option<&'static sidecar::SidecarMgr> {
    SIDECAR.get()
}

// ---- 句柄注册表（T10）：插件 handle → Java Session handle + 连接时的 sidecar 世代快照 ----

#[derive(Clone)]
struct HandleEntry {
    /// Java 侧 Session handle（connect ok.handle，协议键名 "handle"）。
    java_handle: u64,
    /// connect 时的 sidecar 世代；每次 vtable 调用前 check_generation——世代不匹配
    /// 立即报 reconnect，绝不把旧 JVM 的连接号隐式落到新 JVM（spec §5.1）。
    generation: u64,
    /// 方言族诊断名（vendor_of(url)，v0.1.144 REQ-A4）：随错误信封 data.vendor 透出。
    vendor: &'static str,
}

static HANDLES: OnceLock<Mutex<HashMap<u64, HandleEntry>>> = OnceLock::new();
static NEXT_HANDLE: AtomicU64 = AtomicU64::new(0);

/// 插件 runtime：vtable future 与 sidecar 内部任务（writer/reader/stderr 循环）
/// 都跑在它上面（spawn_ffi_future 入口）。
static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

fn handles() -> &'static Mutex<HashMap<u64, HandleEntry>> {
    HANDLES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn rt() -> &'static tokio::runtime::Runtime {
    RT.get().expect("oj-db-jdbc: init not called")
}

fn runtime() -> tokio::runtime::Runtime {
    // worker 只跑帧 IO 转发，2 足够；缺省 = num_cpus 全核白占线程。
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("oj-db-jdbc tokio runtime")
}

fn sc() -> Result<&'static sidecar::SidecarMgr, String> {
    sidecar().ok_or_else(|| "oj-db-jdbc: init not called".to_string())
}

/// 取句柄条目并过世代闸（Mutex 守卫不跨 await，随返回的 owned 条目释放）。
fn entry(handle: u64) -> Result<HandleEntry, String> {
    let e = handles()
        .lock()
        .unwrap()
        .get(&handle)
        .cloned()
        .ok_or_else(|| format!("db: unknown handle {handle}"))?;
    sc()?.check_generation(e.generation)?;
    Ok(e)
}

/// 错误信封补方言族诊断名（REQ-A4，v0.1.144）：结构化信封 → `data.vendor` 注入
/// vendor_of(url) 的方言族（多 vendor 共处部署排障定位）；非 JSON（Rust 侧纯文本
/// 协议错误）原样透出。仅诊断，不影响 category/retryable 分类。
fn vendored(e: String, vendor: &str) -> String {
    if let Ok(mut env) = serde_json::from_str::<Value>(&e)
        && env.get("data").is_some_and(Value::is_object)
    {
        env["data"]["vendor"] = Value::from(vendor);
        return env.to_string();
    }
    e
}

/// 参数预处理（jsint 大整数通道 + BigInteger 回绕防护）：
/// - `$oj$i64` 标记 → JSON 数字（i64 可直接过协议；Java 侧 Integer/Long 形态走 setLong）；
/// - `$oj$u64` 标记 → `"$oj$u64:<十进制>"` 字符串（协议约定：u64 以十进制字符串传
///   Java，RowCodec.bindString 剥前缀按整数字面量 setBigDecimal 精确承载）；
/// - 直传超 i64 大整数（无 marker）→ 同样预字符串化：org.json 在 Java 侧会把超
///   i64 整数解析成 BigInteger，`setLong` 会静默回绕（回绕防护）。
fn prep_params(params: &str) -> Result<Value, String> {
    let v: Value =
        serde_json::from_str(params).map_err(|e| format!("db params: not a JSON array: {e}"))?;
    let arr = v
        .as_array()
        .ok_or_else(|| "db params: must be a JSON array".to_string())?;
    Ok(Value::Array(arr.iter().map(prep_one).collect()))
}

fn prep_one(p: &Value) -> Value {
    if let Some(i) = oj_plugin_ffi::jsint::marker_i64(p) {
        return json!(i);
    }
    if let Some(u) = oj_plugin_ffi::jsint::marker_u64(p) {
        return Value::String(format!("$oj$u64:{u}"));
    }
    // BigInteger 回绕防护见 prep_params 注释。
    if let Some(u) = p.as_u64().filter(|u| *u > i64::MAX as u64) {
        return Value::String(format!("$oj$u64:{u}"));
    }
    p.clone()
}

fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        name: RString::from("db-jdbc"),
        semver: RString::from(env!("CARGO_PKG_VERSION")),
        abi_version: ABI_VERSION,
        fingerprint: RString::from(oj_plugin_ffi::HOST_FINGERPRINT),
        desc: RString::from(
            "db 轴通用 JDBC 插件：Java sidecar 桥接任意 JDBC 驱动（oracle/dm/kingbase8/…）",
        ),
    }
}

fn init(host: RArc<HostContext>, cfg: RString) -> RResult<PluginDescriptor, RString> {
    // 同进程二次 init（多装配/测试重载同一 dylib）：cfg 以首次为准。
    let _ = RT.get_or_init(runtime);
    if SIDECAR.get().is_none() {
        let mut scfg = sidecar::SidecarCfg::from_json(&cfg[..]);
        // stderr drain 逐行上送宿主 tracing（level 3=warn）；只拷函数指针，不持有 RArc。
        let log_fn = host.log;
        scfg.log = Some(Arc::new(move |lvl: u8, msg: &str| {
            log_fn(lvl, RString::from(msg))
        }));
        let _ = SIDECAR.set(sidecar::SidecarMgr::new(scfg));
    }
    let _ = CFG.set(cfg);
    RResult::Ok(descriptor())
}

// ---- vtable（T10 sidecar 胶水 + T11 stream 四槽；类型映射细化是 T12）----

pub extern "C" fn connect(cfg: RString) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        oj_plugin_ffi::spawn_ffi_future(rt(), async move {
            let sc = sc()?;
            let dsn = dsn::parse_dsn(&cfg[..])?;
            let mut args = serde_json::Map::new();
            // match 两臂必有 URL；取局部副本供 vendor_of 诊断与错误信封注入。
            let url;
            match dsn {
                dsn::ParsedDsn::Bare(u) => {
                    url = u;
                    args.insert("url".into(), Value::String(url.clone()));
                }
                dsn::ParsedDsn::Envelope(e) => {
                    url = e.url;
                    args.insert("url".into(), Value::String(url.clone()));
                    if let Some(u) = e.user {
                        args.insert("user".into(), Value::String(u));
                    }
                    if let Some(p) = e.password {
                        args.insert("password".into(), Value::String(p));
                    }
                    // driver_class 是信封逃生舱（dsn.rs 从 props 摘出），Java connect
                    // 从 props 里 remove——放回同一帧即可。
                    let mut props = e.props;
                    if let Some(dc) = e.driver_class {
                        props.insert("driver_class".into(), Value::String(dc));
                    }
                    if !props.is_empty() {
                        args.insert("props".into(), Value::Object(props));
                    }
                }
            }
            let vendor = dsn::vendor_of(&url);
            let ok = sc
                .request("connect", Value::Object(args))
                .await
                .map_err(|e| vendored(e, vendor))?;
            let java_handle = ok
                .get("handle")
                .and_then(Value::as_u64)
                .ok_or_else(|| r#"sidecar connect: response missing "handle""#.to_string())?;
            let handle = NEXT_HANDLE.fetch_add(1, Ordering::SeqCst) + 1;
            handles().lock().unwrap().insert(
                handle,
                HandleEntry {
                    java_handle,
                    generation: sc.generation(),
                    vendor,
                },
            );
            Ok(format!(r#"{{"handle":{handle}}}"#).into_bytes())
        })
    })
}

pub extern "C" fn query(handle: u64, sql: RString, params: RString) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        oj_plugin_ffi::spawn_ffi_future(rt(), async move {
            let e = entry(handle)?;
            let params = prep_params(&params[..])?;
            let ok = sc()?
                .request(
                    "query",
                    json!({"handle": e.java_handle, "sql": &sql[..], "params": params}),
                )
                .await
                .map_err(|err| vendored(err, e.vendor))?;
            ok.get("rows")
                .map(|r| r.to_string().into_bytes())
                .ok_or_else(|| r#"sidecar query: response missing "rows""#.to_string())
        })
    })
}

pub extern "C" fn exec(handle: u64, sql: RString, params: RString) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        oj_plugin_ffi::spawn_ffi_future(rt(), async move {
            let e = entry(handle)?;
            let params = prep_params(&params[..])?;
            let ok = sc()?
                .request(
                    "exec",
                    json!({"handle": e.java_handle, "sql": &sql[..], "params": params}),
                )
                .await
                .map_err(|err| vendored(err, e.vendor))?;
            Ok(ok.to_string().into_bytes())
        })
    })
}

pub extern "C" fn begin(handle: u64) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        oj_plugin_ffi::spawn_ffi_future(rt(), async move {
            let e = entry(handle)?;
            let ok = sc()?
                .request("begin", json!({"handle": e.java_handle}))
                .await
                .map_err(|err| vendored(err, e.vendor))?;
            Ok(ok.to_string().into_bytes())
        })
    })
}

pub extern "C" fn tx_query(handle: u64, tx_id: u64, sql: RString, params: RString) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        oj_plugin_ffi::spawn_ffi_future(rt(), async move {
            let e = entry(handle)?;
            let params = prep_params(&params[..])?;
            let ok = sc()?
                .request(
                    "tx_query",
                    json!({
                        "handle": e.java_handle, "tx_id": tx_id,
                        "sql": &sql[..], "params": params,
                    }),
                )
                .await
                .map_err(|err| vendored(err, e.vendor))?;
            ok.get("rows")
                .map(|r| r.to_string().into_bytes())
                .ok_or_else(|| r#"sidecar tx_query: response missing "rows""#.to_string())
        })
    })
}

pub extern "C" fn tx_exec(handle: u64, tx_id: u64, sql: RString, params: RString) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        oj_plugin_ffi::spawn_ffi_future(rt(), async move {
            let e = entry(handle)?;
            let params = prep_params(&params[..])?;
            let ok = sc()?
                .request(
                    "tx_exec",
                    json!({
                        "handle": e.java_handle, "tx_id": tx_id,
                        "sql": &sql[..], "params": params,
                    }),
                )
                .await
                .map_err(|err| vendored(err, e.vendor))?;
            Ok(ok.to_string().into_bytes())
        })
    })
}

pub extern "C" fn tx_commit(handle: u64, tx_id: u64) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        oj_plugin_ffi::spawn_ffi_future(rt(), async move {
            let e = entry(handle)?;
            let ok = sc()?
                .request(
                    "tx_commit",
                    json!({"handle": e.java_handle, "tx_id": tx_id}),
                )
                .await
                .map_err(|err| vendored(err, e.vendor))?;
            Ok(ok.to_string().into_bytes())
        })
    })
}

pub extern "C" fn tx_rollback(handle: u64, tx_id: u64) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        oj_plugin_ffi::spawn_ffi_future(rt(), async move {
            let e = entry(handle)?;
            let ok = sc()?
                .request(
                    "tx_rollback",
                    json!({"handle": e.java_handle, "tx_id": tx_id}),
                )
                .await
                .map_err(|err| vendored(err, e.vendor))?;
            Ok(ok.to_string().into_bytes())
        })
    })
}

/// spec §5.5：恒 `"jdbc"`（宿主 ffi.rs 连接级一致性校验要求与工厂级
/// `describe().dialect` 逐字相等）。per-handle 方言上报是 A 档裁剪项，不做。
extern "C" fn dialect(_handle: u64) -> RString {
    RString::from("jdbc")
}

pub extern "C" fn close(handle: u64) {
    // 同步槽：非阻塞投递 disconnect（不待在途响应），立即返回。Java per-conn FIFO
    // 队列保证 disconnect 排在在途 op 之后（Session.disconnect 先 drain 再关连接）。
    oj_plugin_ffi::catch_void(|| {
        let Some(e) = handles().lock().unwrap().remove(&handle) else {
            return;
        };
        // T11：连接关闭连带摘除其 stream 条目（插件侧回收，防悬挂；
        // Java 侧流随 disconnect 的 Session 消亡一并失效）。
        streams()
            .lock()
            .unwrap()
            .retain(|_, se| se.java_handle != e.java_handle);
        let Ok(sc) = sc() else {
            return;
        };
        // 世代已死：连接随旧 JVM 消亡，disconnect 无从投递也不必投递。
        if sc.check_generation(e.generation).is_err() {
            return;
        }
        rt().spawn(async move { sc.disconnect_nowait(e.java_handle).await });
    })
}

extern "C" fn schemes() -> RVec<RString> {
    oj_plugin_ffi::catch_value(
        || {
            let mut v = RVec::new();
            v.push(RString::from("jdbc:"));
            v
        },
        RVec::new(),
    )
}

pub extern "C" fn describe() -> RString {
    // `id_two_step` 而非 `id_returning`：id 组恰好一枚（宿主 caps_from_json 已核实），
    // 且 getGeneratedKeys 各厂支持参差（Oracle 历来不可靠），未逐厂验证前不声明 returning。
    RString::from(
        r#"{"dialect":"jdbc","capabilities":["placeholder_q","id_two_step","stream","cancel"]}"#,
    )
}

static DB_VTABLE: DataAccessorVtable = DataAccessorVtable {
    connect,
    query,
    exec,
    begin,
    tx_query,
    tx_exec,
    tx_commit,
    tx_rollback,
    dialect,
    close,
    schemes,
    stream_open,
    stream_next,
    stream_cancel,
    stream_close,
    describe,
};

// ---- stream 注册表（T11）：插件 stream_id → Java 侧寻址 + 连接时的世代快照 ----

#[derive(Clone)]
struct StreamEntry {
    /// Java Session handle（协议键 "handle"）。
    java_handle: u64,
    /// Java 侧 stream_id（stream_open ok.stream_id 快照；后续三槽按它寻址）。
    java_stream_id: u64,
    /// stream_open 时的 sidecar 世代；每次 stream 槽调用前 check_generation——
    /// 与 handle 闸同纪律（spec §5.1：绝不把旧 JVM 的流号落到新 JVM）。
    generation: u64,
    /// 沿自所属 handle 的方言族诊断名（错误信封 data.vendor）。
    vendor: &'static str,
}

static STREAMS: OnceLock<Mutex<HashMap<u64, StreamEntry>>> = OnceLock::new();
static NEXT_STREAM: AtomicU64 = AtomicU64::new(0);

fn streams() -> &'static Mutex<HashMap<u64, StreamEntry>> {
    STREAMS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 取 stream 条目并过世代闸（Mutex 守卫不跨 await，随返回的 owned 条目释放）。
fn stream_entry(stream_id: u64) -> Result<StreamEntry, String> {
    let e = streams()
        .lock()
        .unwrap()
        .get(&stream_id)
        .cloned()
        .ok_or_else(|| format!("db: unknown stream {stream_id}"))?;
    sc()?.check_generation(e.generation)?;
    Ok(e)
}

/// R7（T6 评审钉）：Java 把取消归一为 **ok 值** `{"error":"cancelled"}`（非 err 帧）。
/// 映射层归一为 Err——载荷与 err 帧同构的宿主信封（宿主 parse_raw 单点落 cancelled 类）。
fn cancelled_envelope() -> String {
    json!({
        "category": "cancelled",
        "retryable": false,
        "session_disposition": "keep",
        "message": "cancelled",
        "data": {},
    })
    .to_string()
}

pub extern "C" fn stream_open(handle: u64, sql: RString, params: RString) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        oj_plugin_ffi::spawn_ffi_future(rt(), async move {
            let e = entry(handle)?;
            let params = prep_params(&params[..])?;
            let ok = sc()?
                .request(
                    "stream_open",
                    json!({"handle": e.java_handle, "sql": &sql[..], "params": params}),
                )
                .await
                .map_err(|err| vendored(err, e.vendor))?;
            // R7：取消是 ok 值 {"error":"cancelled"} → 归一 Err；其余 ok 错误值同理
            // 转 Err（宿主 stream_open 不读 ok 信封错误，只认 stream_id/unsupported）。
            if let Some(err) = ok.get("error") {
                return Err(if err == "cancelled" {
                    cancelled_envelope()
                } else {
                    err.to_string()
                });
            }
            let java_stream_id = ok.get("stream_id").and_then(Value::as_u64).ok_or_else(|| {
                r#"sidecar stream_open: response missing "stream_id""#.to_string()
            })?;
            let stream_id = NEXT_STREAM.fetch_add(1, Ordering::SeqCst) + 1;
            streams().lock().unwrap().insert(
                stream_id,
                StreamEntry {
                    java_handle: e.java_handle,
                    java_stream_id,
                    generation: e.generation,
                    vendor: e.vendor,
                },
            );
            Ok(format!(r#"{{"stream_id":{stream_id}}}"#).into_bytes())
        })
    })
}

pub extern "C" fn stream_next(_handle: u64, stream_id: u64) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        oj_plugin_ffi::spawn_ffi_future(rt(), async move {
            let se = stream_entry(stream_id)?;
            let ok = sc()?
                .request(
                    "stream_next",
                    json!({"handle": se.java_handle, "stream_id": se.java_stream_id}),
                )
                .await
                .map_err(|err| vendored(err, se.vendor))?;
            // R7：被取消的 next 回 ok 值 {"error":"cancelled"} → 归一 Err。
            // 其余信封（{"rows":[...]}/{"done":true}）原样透传，宿主逐帧解释。
            if ok.get("error") == Some(&Value::String("cancelled".to_string())) {
                return Err(cancelled_envelope());
            }
            Ok(ok.to_string().into_bytes())
        })
    })
}

pub extern "C" fn stream_cancel(_handle: u64, stream_id: u64) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        oj_plugin_ffi::spawn_ffi_future(rt(), async move {
            let se = stream_entry(stream_id)?;
            let ok = sc()?
                .request(
                    "stream_cancel",
                    json!({"handle": se.java_handle, "stream_id": se.java_stream_id}),
                )
                .await
                .map_err(|err| vendored(err, se.vendor))?;
            Ok(ok.to_string().into_bytes())
        })
    })
}

pub extern "C" fn stream_close(_handle: u64, stream_id: u64) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        oj_plugin_ffi::spawn_ffi_future(rt(), async move {
            // 本地未知 = 已关：幂等 Ok（资源契约：close 是唯一回收口，重入须安全）。
            let Some(se) = streams().lock().unwrap().remove(&stream_id) else {
                return Ok(b"{}".to_vec());
            };
            // 世代已死：流随旧 JVM 消亡，close 无从投递也不必投递（同 T10 close 纪律）。
            if sc()?.check_generation(se.generation).is_err() {
                return Ok(b"{}".to_vec());
            }
            match sc()?
                .request(
                    "stream_close",
                    json!({"handle": se.java_handle, "stream_id": se.java_stream_id}),
                )
                .await
            {
                Ok(ok) => Ok(ok.to_string().into_bytes()),
                // cancelled-close 是正常路径：Java 已把取消的流拆掉，
                // "unknown stream_id" err 帧按幂等成功收口，不向上传播。
                Err(e) if e.contains("unknown stream_id") => Ok(b"{}".to_vec()),
                Err(e) => Err(e),
            }
        })
    })
}

oj_plugin_ffi::oj_plugin_entry!(init, config: "oj-db-jdbc", db => &DB_VTABLE);

// ---- 测试面（#[doc(hidden)]；唯一写入方是 tests/vtable.rs 的自重现假 java 夹具）----

/// 注入测试 SidecarMgr 并确保 runtime 在位（OnceLock 单例：一进程一次，场景内串行复用）。
#[doc(hidden)]
pub fn __test_init(cfg: sidecar::SidecarCfg) {
    let _ = RT.get_or_init(runtime);
    let _ = SIDECAR.set(sidecar::SidecarMgr::new(cfg));
}

/// 测试拆卸：等在途收尾、假 java 走 EOF 有序退出。
#[doc(hidden)]
pub async fn __test_shutdown() -> Result<(), String> {
    match sidecar() {
        Some(sc) => sc.shutdown().await,
        None => Ok(()),
    }
}

/// vtable 槽位直引（集成测试绕过 dlsym 直接驱动；函数体与插件导出 vtable 同体）。
#[doc(hidden)]
pub mod vt {
    pub use super::{
        begin, close, connect, describe, exec, query, stream_cancel, stream_close, stream_next,
        stream_open, tx_commit, tx_exec, tx_query, tx_rollback,
    };
}

#[cfg(test)]
#[path = "dsn_tests.rs"]
mod dsn_tests;

#[cfg(test)]
#[path = "codec_tests.rs"]
mod codec_tests;

#[cfg(test)]
#[path = "errmap_tests.rs"]
mod errmap_tests;

#[cfg(test)]
#[path = "lib_tests.rs"]
mod lib_tests;
