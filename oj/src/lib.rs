// `async-trait` 宏（0.1.x）为每个 async 方法生成 `#[must_use]`，而返回类型
// `Pin<Box<dyn Future>>` 本身已 `#[must_use]`；clippy 1.99 的 `double_must_use`
// 据此报红。属宏误报——本仓不手动为 future 标注 `#[must_use]`，统一在此屏蔽。
#![allow(clippy::double_must_use)]

//! oj 库面：bin 与集成测试共用（oj/tests/ 经 `use oj::...` 触达装配层，
//! 纯 bin crate 的 `pub mod` 对外不可见，故 lib + bin 双 target）。

/// 字节转小写 hex 串（sha256 摘要 / 密钥指纹：mount_fetch / build_cmd /
/// secret_cmd 共用一份，v0.1.83 前是三处逐字重复）。
pub(crate) fn hex_encode(d: &[u8]) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect()
}

pub mod app;
pub mod args;
pub mod build_cmd;
pub mod checks;
pub mod exec_cmd;
pub mod exec_ext;
pub mod info_cmd;
pub mod manifest;
pub mod mcp_cmd;
pub mod migrate;
pub mod migrate_cmd;
pub mod mount_fetch;
pub mod openapi_cmd;
pub mod pack;
pub mod schema;
pub mod secret_cmd;
pub mod seed;
pub mod serve_cmd;
pub mod tasks;
pub mod test_cmd;
pub mod test_ext;
