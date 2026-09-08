//! 无 oj 符号的 cdylib 夹具：dlopen 成功但缺 `oj_plugin_abi_version` 导出——
//! 供宿主侧 SymbolMissing 门禁测试（不依赖系统库，全平台确定性）。

#[unsafe(no_mangle)]
pub extern "C" fn oj_not_a_plugin() {}
