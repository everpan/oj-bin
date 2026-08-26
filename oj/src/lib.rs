//! oj 库面：bin 与集成测试共用（oj/tests/ 经 `use oj::...` 触达装配层，
//! 纯 bin crate 的 `pub mod` 对外不可见，故 lib + bin 双 target）。
pub mod app;
pub mod args;
pub mod build_cmd;
pub mod manifest;
pub mod pack;
pub mod server_cmd;
pub mod test_cmd;
pub mod test_ext;
