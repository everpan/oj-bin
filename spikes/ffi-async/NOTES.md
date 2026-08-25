# Spike S.2：FfiFuture + 插件自建 tokio runtime

日期：2026-08-25。结论：**形态成立**，`cargo run -p host` 全绿（"ALL SPIKE S.2 CHECKS PASSED"）。

## 实证结论

- 插件 `OnceLock<Runtime>` 自建 multi_thread runtime，内部真实 `tokio::time::sleep().await`
  **不 panic**——"there is no reactor running" 不复现，插件 TLS 挂在插件自己那份 tokio 上成立。
- host 在自己 runtime 里 `poll + yield_now` 轮询桥接 await，roundtrip 正确。
- drop 语义实测：host 提前 drop（free 不 take），插件任务**仍跑完**
  （插件侧 `AtomicU64` 计数确认），无 UB、无 abort。

## FfiFuture 定稿形态（契约 crate Task 3.1 直接迁入）

```rust
#[repr(C)]
pub struct FfiFuture {
    pub state: *mut c_void,                                  // 插件侧共享状态（opaque）
    pub poll:  extern "C" fn(*mut c_void) -> i32,            // 0 pending / 1 ready / -1 error
    pub take:  extern "C" fn(*mut c_void) -> RResult<RBytes, RString>, // ready 后调一次
    pub free:  extern "C" fn(*mut c_void),                   // 释放 state；null 安全
}
```

- `RBytes = stabby::vec::Vec<u8>`（stabby 72 自带稳定 Vec，默认可省略 Alloc 泛型）。
- take 后的清理时序：**宿主先 take 再 free，free 后必须将 state 置 null**（或宿主侧
  句柄用 ManuallyDrop），否则 Drop 二次 free。
- 插件侧共享状态用 `oneshot::Receiver + Option<暂存结果>`：**`try_recv` 是消费式的**，
  poll 取到值必须暂存，否则 take 拿空。

## 宿主侧适配器义务（写进 Task 3.3 FfiXxxBackend）

1. `wait()` 桥：poll 轮询（yield_now 或 spawn_blocking），ready 后 take→free→state 置 null。
2. `Drop for 宿主句柄`：state 非 null 则 free（=放弃结果，不保证取消）。
3. poll 返回 -1 时也要 take（错误细节在 take 的 Err 里）再 free。

## 构建注意

- 插件 cdylib 不是 host 的 cargo 依赖，`cargo run -p host` **不会重建插件**——
  改插件后必须 `cargo build`（全 workspace）再跑。真实装配同理：xtask 产物拷贝
  要在 host 运行前完成（计划 Task 3.5）。
