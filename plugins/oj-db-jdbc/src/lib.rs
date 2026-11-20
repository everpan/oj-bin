//! oj-db-jdbc：db 轴通用 JDBC 插件（design spec §5）。
//! 形态：Rust cdylib 插件 + Java sidecar（`java/jdbc-bridge/`，NDJSON 帧协议 §5.3）：
//! drivers_dir 全部 jar 经 URLClassLoader 装载，DriverShim 过 DriverManager 类加载器白名单。
//! T7：crate 骨架 + DSN 解析（`dsn`）；vtable 其余槽位 `ready_err("not implemented")`
//! 占位（T10 sidecar 编排 / T11 类型映射时填充）。
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
use std::sync::{Arc, OnceLock};

/// 装配期 cfg 暂存（`plugins: oj-db-jdbc:` 段透传 JSON：`drivers_dir` / `java_bin` /
/// `java_opts`，spec §5.2）。T10 解析使用；DSN 本体在 connect 按值传入（db 插件统一契约）。
static CFG: OnceLock<RString> = OnceLock::new();

/// SidecarMgr 进程级单例（spec §5.9：v1 单 JVM 单例；多厂商同机需求出现时变 keyed）。
/// init 期注入 cfg；首次 request 惰性 spawn java sidecar。
static SIDECAR: OnceLock<sidecar::SidecarMgr> = OnceLock::new();

/// T10（vtable connect/query 编排）经此取 SidecarMgr。
#[allow(dead_code)] // T9 落地先行，T10 消费。
pub(crate) fn sidecar() -> Option<&'static sidecar::SidecarMgr> {
    SIDECAR.get()
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

// ---- vtable（T7 占位；T10 sidecar / T11 类型映射填充）----

extern "C" fn connect(_cfg: RString) -> FfiFuture {
    oj_plugin_ffi::ready_err("not implemented")
}

extern "C" fn query(_handle: u64, _sql: RString, _params: RString) -> FfiFuture {
    oj_plugin_ffi::ready_err("not implemented")
}

extern "C" fn exec(_handle: u64, _sql: RString, _params: RString) -> FfiFuture {
    oj_plugin_ffi::ready_err("not implemented")
}

extern "C" fn begin(_handle: u64) -> FfiFuture {
    oj_plugin_ffi::ready_err("not implemented")
}

extern "C" fn tx_query(_handle: u64, _tx_id: u64, _sql: RString, _params: RString) -> FfiFuture {
    oj_plugin_ffi::ready_err("not implemented")
}

extern "C" fn tx_exec(_handle: u64, _tx_id: u64, _sql: RString, _params: RString) -> FfiFuture {
    oj_plugin_ffi::ready_err("not implemented")
}

extern "C" fn tx_commit(_handle: u64, _tx_id: u64) -> FfiFuture {
    oj_plugin_ffi::ready_err("not implemented")
}

extern "C" fn tx_rollback(_handle: u64, _tx_id: u64) -> FfiFuture {
    oj_plugin_ffi::ready_err("not implemented")
}

/// spec §5.5：恒 `"jdbc"`（宿主 ffi.rs 连接级一致性校验要求与工厂级
/// `describe().dialect` 逐字相等）。per-handle 方言上报是 A 档裁剪项，不做。
extern "C" fn dialect(_handle: u64) -> RString {
    RString::from("jdbc")
}

extern "C" fn close(_handle: u64) {
    // T10：非阻塞投递 disconnect 帧（per-conn 队列尾）。占位 no-op。
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

extern "C" fn describe() -> RString {
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

extern "C" fn stream_open(_handle: u64, _sql: RString, _params: RString) -> FfiFuture {
    oj_plugin_ffi::ready_err("not implemented")
}

extern "C" fn stream_next(_handle: u64, _stream_id: u64) -> FfiFuture {
    oj_plugin_ffi::ready_err("not implemented")
}

extern "C" fn stream_cancel(_handle: u64, _stream_id: u64) -> FfiFuture {
    oj_plugin_ffi::ready_err("not implemented")
}

extern "C" fn stream_close(_handle: u64, _stream_id: u64) -> FfiFuture {
    oj_plugin_ffi::ready_err("not implemented")
}

oj_plugin_ffi::oj_plugin_entry!(init, config: "oj-db-jdbc", db => &DB_VTABLE);

#[cfg(test)]
#[path = "dsn_tests.rs"]
mod dsn_tests;

#[cfg(test)]
#[path = "codec_tests.rs"]
mod codec_tests;

#[cfg(test)]
#[path = "errmap_tests.rs"]
mod errmap_tests;
