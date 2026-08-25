//! oj-plugin-ffi：宿主与插件共享的 FFI 契约（两侧依赖同一 crate，spec §3）。
//! - `ABI_VERSION`：唯一硬门禁，严格相等才允许加载。
//! - 所有 repr(C) 类型的字段变更 = ABI_VERSION bump；向后兼容走 cfg JSON 字段。
//! - stabby 72 注意：`RResult` 的 Ok/Err 是关联函数（构造用 `RResult::Ok(v)`），
//!   消费侧 `std::result::Result::from(r)` 转换后 match，不能模式匹配。

pub mod es;
pub mod future;

pub use es::EsBackendVtable;
pub use future::FfiFuture;

pub type RString = stabby::string::String;
pub type RVec<T> = stabby::vec::Vec<T>;
pub type RBytes = stabby::vec::Vec<u8>;
pub type RResult<T, E> = stabby::result::Result<T, E>;
pub type RArc<T> = stabby::sync::Arc<T>;

/// 唯一硬门禁：严格相等才允许加载（spec §3）。
pub const ABI_VERSION: u32 = 1;

/// 构建指纹：rustc 版本 + oj-plugin-ffi 版本 + target triple（诊断用，不匹配仅告警）。
pub const HOST_FINGERPRINT: &str = concat!(
    "rustc ",
    env!("CONST_RUSTC_VERSION"),
    " / oj-plugin-ffi ",
    env!("CARGO_PKG_VERSION"),
    " / ",
    env!("CONST_TARGET_TRIPLE"),
);

/// 插件描述（repr(C)：任何字段变更 = ABI_VERSION bump，spec §3 契约演进总则）。
/// #[stabby::stabby] 让 RResult<PluginDescriptor, _> 满足 IStable（可转 std Result match）。
#[stabby::stabby]
#[repr(C)]
pub struct PluginDescriptor {
    pub name: RString,
    pub semver: RString,
    pub abi_version: u32,
    /// 构建指纹（诊断用，不匹配仅告警）。
    pub fingerprint: RString,
    /// 注册回调：宿主在 init 返回后立即调用取得各轴 vtable 槽位（spec §3，
    /// 全部工厂注册须在 init 调用窗口内完成——槽位指向的静态表在 init 时就绪）。
    pub register: extern "C" fn() -> PluginRegistrations,
}

/// 各轴 vtable 槽位（repr(C)；null = 该插件不提供此轴。db/blob/bus/kv 槽位随阶段 4 加入，
/// 加字段 = ABI bump）。
#[stabby::stabby]
#[repr(C)]
pub struct PluginRegistrations {
    pub es: *const EsBackendVtable,
}

impl PluginRegistrations {
    pub fn none() -> Self {
        Self { es: std::ptr::null() }
    }

    pub fn es(&self) -> Option<&'static EsBackendVtable> {
        unsafe { self.es.as_ref() }
    }
}

/// 宿主回调集（RArc 共享所有权传入，进程级有效；不提供 registry lookup——插件互不可见）。
#[stabby::stabby]
#[repr(C)]
pub struct HostContext {
    /// 日志上送：插件日志经此回调进宿主 tracing。level: 0=trace 1=debug 2=info 3=warn 4=error。
    pub log: extern "C" fn(level: u8, msg: RString),
    // bus deliver 回调在 Task 4.3 加入（加回调 = ABI bump，本期一次设计好预留位）。
}

/// 插件入口两符号（由宏生成，禁止手写 #[no_mangle] 绕过，spec §3）：
///   oj_plugin_abi_version() -> u32
///   oj_plugin_init(host: RArc<HostContext>, cfg: RString) -> RResult<PluginDescriptor, RString>
/// 宏内建 catch_unwind(AssertUnwindSafe(..))，panic 映射为 RResult 错误。
/// 插件 crate 必须保留 panic=unwind（profile 不得覆盖为 abort）。
#[macro_export]
macro_rules! oj_plugin_entry {
    ($init:expr) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn oj_plugin_abi_version() -> u32 {
            $crate::ABI_VERSION
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn oj_plugin_init(
            host: $crate::RArc<$crate::HostContext>,
            cfg: $crate::RString,
        ) -> $crate::RResult<$crate::PluginDescriptor, $crate::RString> {
            let init: fn(
                $crate::RArc<$crate::HostContext>,
                $crate::RString,
            ) -> $crate::RResult<$crate::PluginDescriptor, $crate::RString> = $init;
            match ::std::panic::catch_unwind(::std::panic::AssertUnwindSafe(|| init(host, cfg))) {
                ::core::result::Result::Ok(r) => r,
                ::core::result::Result::Err(_) => {
                    $crate::RResult::Err($crate::RString::from("panic in plugin init"))
                }
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_fingerprint_is_nonempty() {
        assert!(HOST_FINGERPRINT.contains("oj-plugin-ffi"));
    }
}
