//! 泛型轴插件开发模板：`cache` 轴（进程内 HashMap，仅作范式载体，生产请替换实现）。
//!
//! 范式四契约（违反任一条即加载失败 / 调用出错 / 触发 UB）：
//! 1. 轴名小写，且避开宿主 9 个保留类型化轴：es / db / blob / bus / kv / auth /
//!    mq / mail / ldap —— 之外的任意名字自动走泛型通道，零宿主改动；
//! 2. vtable 必须静态可寻址（`static` + `oj_plugin_entry!` 展开时取地址）；
//! 3. vtable 方法必须用 `catch_future` / `catch_void` 收敛 panic —— 不跨界展开
//!    （跨界 unwind 跨 cdylib 边界是 UB）；
//! 4. 业务错误经 FfiFuture 的 Err 透传（JS 侧收到 `{code,msg}` 信封），
//!    panic 由 `oj_plugin_entry!` 与 `catch_future` 兜底成 Err。

use std::collections::HashMap;
use std::sync::Mutex;

use oj_plugin_ffi::{FfiFuture, HostContext, RArc, RResult, RString, oj_plugin_entry};

/// 允许的配置键（fail-fast 哲学：未知键拒绝加载，而非静默忽略——照 oj-ldap）。
const ALLOWED_CFG_KEYS: [&str; 1] = ["max_entries"];

/// 进程内缓存（范式载体）。真实插件在此持有连接池 / 客户端，按需初始化。
/// Mutex 仅因 vtable 是同步 extern "C" 入口；异步实现见 `spawn_ffi_future`。
static CACHE: Mutex<Option<HashMap<String, serde_json::Value>>> = Mutex::new(None);

extern "C" fn call(op: RString, args: RString) -> FfiFuture {
    // 契约 3：同步 panic 收敛为立即错误的 future。
    oj_plugin_ffi::catch_future(|| dispatch(op, args))
}

fn dispatch(op: RString, args: RString) -> FfiFuture {
    let args = match serde_json::from_slice::<serde_json::Value>(args[..].as_bytes()) {
        Ok(serde_json::Value::Array(a)) => a,
        // 契约 4：入参不合法走 Err，不 panic。
        _ => return oj_plugin_ffi::ready_err("cache axis: args must be a JSON array"),
    };
    let mut guard = match CACHE.lock() {
        Ok(g) => g,
        Err(_) => return oj_plugin_ffi::ready_err("cache axis: lock poisoned"),
    };
    let map = guard.get_or_insert_with(HashMap::new);
    match &op[..] {
        "get" => {
            let Some(k) = args.first().and_then(|v| v.as_str()) else {
                return oj_plugin_ffi::ready_err("cache axis: get(key) needs a string key");
            };
            let v = map.get(k).cloned().unwrap_or(serde_json::Value::Null);
            oj_plugin_ffi::ready_ok(v.to_string().into_bytes())
        }
        "set" => {
            let (Some(k), Some(v)) = (args.first().and_then(|v| v.as_str()), args.get(1)) else {
                return oj_plugin_ffi::ready_err("cache axis: set(key, value) needs both");
            };
            map.insert(k.to_string(), v.clone());
            oj_plugin_ffi::ready_ok(br#"{"ok":true}"#.to_vec())
        }
        _ => oj_plugin_ffi::ready_err(format!("cache axis: unknown op '{}'", &op[..])),
    }
}

// 契约 2：vtable 静态可寻址。
static CACHE_VT: oj_plugin_ffi::GenericVtable = oj_plugin_ffi::GenericVtable { call };

fn init(
    _host: RArc<HostContext>,
    cfg: RString,
) -> RResult<oj_plugin_ffi::PluginDescriptor, RString> {
    if let Err(e) = validate_cfg(&cfg[..]) {
        return RResult::Err(e);
    }
    RResult::Ok(oj_plugin_ffi::PluginDescriptor {
        name: RString::from("oj-cache"),
        semver: RString::from(env!("CARGO_PKG_VERSION")),
        abi_version: oj_plugin_ffi::ABI_VERSION,
        fingerprint: RString::from(oj_plugin_ffi::HOST_FINGERPRINT),
        // desc 会出现在 GET {base}/plugins 与 JS plugins()——在此注明宿主最低版本。
        desc: RString::from(
            "泛型轴插件模板：cache 轴（进程内缓存范式载体）；需宿主支持泛型轴通道 \
             与插件 config key 自报（v0.1.54 起，以 CHANGELOG 为准）",
        ),
    })
}

/// 配置键白名单校验：cfg 是宿主按 `config: "cache"` 声明取到的顶层段 JSON。
/// 注意 RResult = stabby Result：Ok/Err 是关联函数只能构造，模式匹配请用 std Result。
fn validate_cfg(cfg: &str) -> Result<(), RString> {
    if cfg.trim().is_empty() {
        return Ok(());
    }
    let v: serde_json::Value = match serde_json::from_str(cfg) {
        Ok(v) => v,
        Err(e) => return Err(RString::from(format!("cache: cfg 不是合法 JSON: {e}"))),
    };
    let Some(obj) = v.as_object() else {
        return Err(RString::from("cache: cfg 必须是 JSON object"));
    };
    for k in obj.keys() {
        if !ALLOWED_CFG_KEYS.contains(&k.as_str()) {
            return Err(RString::from(format!(
                "cache: 未知配置键 '{k}'（允许：{}）",
                ALLOWED_CFG_KEYS.join(", ")
            )));
        }
    }
    Ok(())
}

// config: "cache" 声明配置键（宏里必须前置）；generic(cache) 声明泛型轴
//（裸 `cache => &VT` 是类型化 kind 臂——9 个保留轴才用；generic(...) 必须带括号）。
oj_plugin_entry!(init, config: "cache", generic(cache) => &CACHE_VT);
