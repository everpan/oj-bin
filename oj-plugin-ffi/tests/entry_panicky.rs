//! 入口宏测试（panic 插件）：init 内 panic 经宏包装收敛为 RResult::Err，不 unwind 跨界。

use oj_plugin_ffi::{
    ABI_VERSION, HostContext, PluginDescriptor, RArc, RResult, RString, oj_plugin_entry,
};

extern "C" fn noop_log(_level: u8, _msg: RString) {}
extern "C" fn noop_deliver(_topic: RString, _payload: RString) {}

fn init(_host: RArc<HostContext>, _cfg: RString) -> RResult<PluginDescriptor, RString> {
    panic!("init boom")
}

oj_plugin_entry!(init);

#[test]
fn init_panic_converges_to_err_not_unwind() {
    assert_eq!(oj_plugin_abi_version(), ABI_VERSION);
    let host = RArc::new(HostContext {
        log: noop_log,
        deliver: noop_deliver,
    });
    let r = oj_plugin_init(host, RString::from("{}"));
    match std::result::Result::from(r) {
        Err(e) => assert!(e[..].contains("panic"), "{e}"),
        Ok(_) => panic!("panicking init must converge to RResult::Err"),
    }
}
