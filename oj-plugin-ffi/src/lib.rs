//! oj-plugin-ffi：宿主与插件共享的 FFI 契约（两侧依赖同一 crate，spec §3）。
//! - `ABI_VERSION`：唯一硬门禁，严格相等才允许加载。
//! - 所有 repr(C) 类型的字段变更 = ABI_VERSION bump；向后兼容走 cfg JSON 字段。
//! - stabby 72 注意：`RResult` 的 Ok/Err 是关联函数（构造用 `RResult::Ok(v)`），
//!   消费侧 `std::result::Result::from(r)` 转换后 match，不能模式匹配。

pub mod auth;
pub mod axis;
pub mod blob;
pub mod bus;
pub mod db;
pub mod es;
pub mod future;
pub mod jsint;
pub mod kv;
pub mod ldap;
pub mod mail;
pub mod mq;
pub mod path_util;

pub use auth::AuthGuardVtable;
pub use blob::BlobBackendVtable;
pub use bus::EventBrokerVtable;
pub use db::DataAccessorVtable;
pub use es::EsBackendVtable;
pub use future::FfiFuture;
// I-2：跨边界安全的 FfiFuture 工厂与 vtable 方法包装宏（spec §3 统一 catch_unwind）。
pub use future::{catch_future, catch_value, catch_void, ready_err, ready_ok, spawn_ffi_future};
pub use kv::KVStoreVtable;
pub use ldap::LdapVtable;
pub use mail::{MailAttachment, MailVtable};
pub use mq::{MqMessage, MqVtable};
// re-export：oj_plugin_entry! 展开内经 $crate::paste::paste! 拼接轴符号名，
// 使用方（插件 crate）无需自带 paste 依赖。
pub use paste;

pub type RString = stabby::string::String;
pub type RVec<T> = stabby::vec::Vec<T>;
pub type RBytes = stabby::vec::Vec<u8>;
pub type RResult<T, E> = stabby::result::Result<T, E>;
pub type RArc<T> = stabby::sync::Arc<T>;

/// 轴声明（自报清单条目）。RString 不可 const 构造 → 清单调用期经 RVec 返回
///（评审 M1：不做 static 数组）。
#[stabby::stabby]
#[repr(C)]
pub struct AxisDecl {
    pub name: RString,
    pub vtable: *const core::ffi::c_void,
}

/// 泛型轴 vtable：op 名 + JSON 参数 → JSON bytes 结果。
/// FfiFuture 与全部既有 vtable 同形（错误经 future Err 透传）。
#[stabby::stabby]
#[repr(C)]
pub struct GenericVtable {
    pub call: extern "C" fn(op: RString, args: RString) -> FfiFuture,
}

/// 唯一硬门禁：严格相等才允许加载（spec §3）。
/// 2 = Task 4.1 起（PluginRegistrations 增 db 槽位 + DataAccessorVtable）。
/// 3 = Task 4.2 起（PluginRegistrations 增 blob 槽位 + BlobBackendVtable）。
/// 4 = Task 4.3 起（PluginRegistrations 增 bus 槽位 + EventBrokerVtable + HostContext 增 deliver）。
/// 5 = Task 4.4 起（PluginRegistrations 增 kv 槽位 + KVStoreVtable）。
/// 6 = auth 解耦起（PluginRegistrations 增 auth 槽位 + AuthGuardVtable）。
/// 7 = 按轴 dlsym（删 PluginRegistrations/register，加轴自此零破坏）。
/// 8 = bus 字节载荷起（EventBrokerVtable.publish data 与 HostContext.deliver payload
///     RString→RBytes，ws 二进制帧透传，v0.1.16）。
/// 9 = cookie 会话 + 上传直传起（AuthGuardVtable.verify 四参化：path/method/
///     authorization/headers JSON——method 与任意请求头是 CSRF 双提交判定材料；
///     BlobBackendVtable 增 upload_url 方法，v0.1.30）。
/// 10 = 流式面起（DataAccessorVtable 增 stream_open/next/cancel/close 四槽——批量 pull
///     + 信封 `{"rows":[...]}|{"done":true}|{"error":"..."}`，open 哨兵 `{"unsupported":true}`
///     宿主回落 fetch_all；BlobBackendVtable 增 put_stream_open/chunk/finish/abort 四槽——
///     服务端流式 multipart/PUT 直传落盘。v0.1.38）。
/// 11 = blob 搬运与区间读起（BlobBackendVtable 增 copy / move_to / read_range 三槽——
///     服务端复制与搬移让大文件不进 V8；区间读让嗅探/lazy 读只读样本字节。v0.1.47）。
pub const ABI_VERSION: u32 = 11;

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
    /// 人类可读描述（插件作者自述；host 收集并在 GET {base}/plugins 公开）。
    pub desc: RString,
}

/// 宿主回调集（RArc 共享所有权传入，进程级有效；不提供 registry lookup——插件互不可见）。
#[stabby::stabby]
#[repr(C)]
pub struct HostContext {
    /// 日志上送：插件日志经此回调进宿主 tracing。level: 0=trace 1=debug 2=info 3=warn 4=error。
    pub log: extern "C" fn(level: u8, msg: RString),
    /// 消息上送（Task 4.3）：bus 插件订阅循环收到消息经此回调非阻塞投递宿主
    /// （宿主按 topic 扇出到本地订阅通道；插件线程调用，须返回快）。
    /// ABI 8 起 payload 为字节（RBytes，bus 二进制载荷透传，v0.1.16）。
    pub deliver: extern "C" fn(topic: RString, payload: RBytes),
}

/// 插件入口宏：生成 oj_plugin_abi_version / oj_plugin_init（catch_unwind 收敛）/
/// 每轴一个 `oj_plugin_axis_<name>` 导出符号（返回静态 vtable 指针，擦除为 *const c_void）/
/// 轴自报清单 `oj_plugin_axes` / 可选 `oj_plugin_config_key`。
/// 用法：
///   oj_plugin_entry!(init);                                          // 零轴
///   oj_plugin_entry!(init, kv => &KV_VTABLE);                        // 单轴
///   oj_plugin_entry!(init, kv => &KV_VTABLE, auth => &AUTH_VTABLE);  // 多轴
///   oj_plugin_entry!(init, config: "cache", kv => &KV_VTABLE);       // 带配置键
/// 轴标识必须小写（stringify 原样进清单，无 :lower 兜底）。
/// 轴标识写入 per-axis 符号前强制小写（宿主探测表全小写）；未提供的轴不导出符号 = 不提供该轴。
/// 注意：vtable 方法须在实现侧以 catch_value/catch_future 收敛 panic——宿主对
/// vtable 方法无 catch_unwind（本宏只保护 init）。
#[macro_export]
macro_rules! oj_plugin_entry {
    // 含 config 键的形态：config 必须前置，单独规则避免与 $axis:ident 局部歧义。
    ($init:expr, config: $ck:literal $(, $axis:ident => $vtable:expr)* $(,)?) => {
        $crate::oj_plugin_entry_impl! { @body [$init] [$ck] [$($axis => $vtable)*] }
    };
    // 不含 config 键的形态（零轴 / 普通轴）。
    ($init:expr $(, $axis:ident => $vtable:expr)* $(,)?) => {
        $crate::oj_plugin_entry_impl! { @body [$init] [] [$($axis => $vtable)*] }
    };
}

#[macro_export]
macro_rules! oj_plugin_entry_impl {
    (@body [$init:expr] [$($ck:literal)?] [$($axis:ident => $vtable:expr)*]) => {
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

        $(
            $crate::paste::paste! {
                #[unsafe(no_mangle)]
                pub extern "C" fn [<oj_plugin_axis_ $axis:lower>]() -> *const ::core::ffi::c_void {
                    $vtable as *const _ as *const ::core::ffi::c_void
                }
            }
        )*

        /// 轴自报清单（宿主优先路径；旧宿主无此符号 → 回退逐轴 dlsym）。
        /// 宏双发：per-axis 符号保留至回退路径退役（spec 评审 M3）。
        #[unsafe(no_mangle)]
        pub extern "C" fn oj_plugin_axes() -> $crate::RVec<$crate::AxisDecl> {
            let mut v = $crate::RVec::new();
            $(
                v.push($crate::AxisDecl {
                    name: $crate::RString::from(stringify!($axis)),
                    vtable: $vtable as *const _ as *const ::core::ffi::c_void,
                });
            )*
            v
        }

        $(
            #[unsafe(no_mangle)]
            pub extern "C" fn oj_plugin_config_key() -> $crate::RString {
                $crate::RString::from($ck)
            }
        )?
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_fingerprint_is_nonempty() {
        assert!(HOST_FINGERPRINT.contains("oj-plugin-ffi"));
    }

    // 假 vtable：只需静态可寻址，不需要真实字段全贯通（贯通由 loader 测试覆盖）。
    #[repr(C)]
    struct FakeVt {
        _pad: u8,
    }
    static FAKE_A: FakeVt = FakeVt { _pad: 0 };
    static FAKE_B: FakeVt = FakeVt { _pad: 1 };

    // 每次展开都生成 oj_plugin_abi_version / oj_plugin_init 两个 no_mangle 符号，
    // 同一二进制只能展开一次 → 此处用多轴用法（abi/init/轴符号一次全覆盖）；
    // 零轴用法由 tests/entry_good.rs（独立测试二进制）覆盖。
    // 假轴名 fakea/fakeb 避免撞真轴/宿主符号；auth 臂走 axis::helper ——
    // 文档推荐的裸传形态经宏展开编译钉死（宏改动时此形态静默失效即测试红）。
    mod macro_smoke {
        use super::{FAKE_A, FAKE_B, FakeVt};
        use crate::{AuthGuardVtable, RResult, RString};

        extern "C" fn stub_verify(
            _: RString,
            _: RString,
            _: RString,
            _: RString,
        ) -> RResult<RString, RString> {
            unreachable!()
        }
        static AUTH_VT: AuthGuardVtable = AuthGuardVtable {
            verify: stub_verify,
        };

        oj_plugin_entry!(
            test_axes_init,
            fakea => &FAKE_A,
            fakeb => &FAKE_B,
            auth => crate::axis::auth(&AUTH_VT)
        );

        fn test_axes_init(
            _: crate::RArc<crate::HostContext>,
            _: crate::RString,
        ) -> crate::RResult<crate::PluginDescriptor, crate::RString> {
            unreachable!()
        }

        #[test]
        fn axis_symbols_export_vtable_pointers() {
            type Sym = unsafe extern "C" fn() -> *const std::ffi::c_void;
            let a: Sym = oj_plugin_axis_fakea;
            let b: Sym = oj_plugin_axis_fakeb;
            let c: Sym = oj_plugin_axis_auth;
            unsafe {
                assert_eq!(a(), &FAKE_A as *const FakeVt as *const std::ffi::c_void);
                assert_eq!(b(), &FAKE_B as *const FakeVt as *const std::ffi::c_void);
                // helper 形态：符号返回值 == helper 擦除结果（同址非空）
                assert_eq!(c(), crate::axis::auth(&AUTH_VT));
                assert!(!c().is_null());
            }
        }
    }
}
