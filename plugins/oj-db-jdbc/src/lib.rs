//! oj-db-jdbc：db 轴通用 JDBC 插件（design spec §5）。
//! 形态：Rust cdylib 插件 + Java sidecar（`java/jdbc-bridge/`，NDJSON 帧协议 §5.3）：
//! drivers_dir 全部 jar 经 URLClassLoader 装载，DriverShim 过 DriverManager 类加载器白名单。
//! T7：crate 骨架 + DSN 解析（`dsn`）；vtable 其余槽位 `ready_err("not implemented")`
//! 占位（T10 sidecar 编排 / T11 类型映射时填充）。

pub mod dsn;

use oj_plugin_ffi::{
    ABI_VERSION, DataAccessorVtable, FfiFuture, HostContext, PluginDescriptor, RArc, RResult,
    RString, RVec,
};
use std::sync::OnceLock;

/// 装配期 cfg 暂存（`plugins: oj-db-jdbc:` 段透传 JSON：`drivers_dir` / `java_bin` /
/// `java_opts`，spec §5.2）。T10 解析使用；DSN 本体在 connect 按值传入（db 插件统一契约）。
static CFG: OnceLock<RString> = OnceLock::new();

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
    let _ = &host;
    // 同进程二次 init（多装配/测试重载同一 dylib）：cfg 以首次为准。
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
