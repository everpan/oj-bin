// `async-trait` 宏（0.1.x）为每个 async 方法生成 `#[must_use]`，而返回类型
// `Pin<Box<dyn Future>>` 本身已 `#[must_use]`；clippy 1.99 的 `double_must_use`
// 据此报红。属宏误报——本仓不手动为 future 标注 `#[must_use]`，统一在此屏蔽。
#![allow(clippy::double_must_use)]

pub mod bridge;
pub mod config;
pub mod contract;
pub mod secret;
