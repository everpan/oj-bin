# Spike S.3：tx 句柄化 + 后端对象形态决策

日期：2026-08-25。`cargo run -p host` 与 `cargo run -p host-dynptr` 均全绿。

## vtable 样例（plugin/host）实证

- `connect(cfg_json) -> u64 handle`；`begin(handle) -> FfiFuture`（payload = tx_id ascii）；
  `tx_exec/tx_commit/tx_rollback(handle, tx_id, ...)`；`close(handle)`。
- 全链路 connect → query → begin → tx_exec → tx_commit 走通；commit 后 tx_id 失效。
- **drop-rollback 的 FFI 映射成立**：begin 后不 commit 直接 close(handle)，插件把未提交
  tx 全部 rollback（`rolled_back_count()` 观测 +1）。宿主侧适配器（Task 3.3）在
  ReqState reset 路径调 tx_rollback 即可复用同一语义。
- 内存操作无真实异步时，FfiFuture 预置 ready 即可（真实异步路径已在 S.2 实证）。

## dynptr 评估（plugin-dynptr/host-dynptr + 共享 contract crate）

技术门槛实测**通过**：

- `#[stabby::stabby(checked)] trait Pinger`（方法必须全部 `extern "C" fn`）+
  `stabby::dynptr!(Box<dyn Pinger + Send + Sync>)` 跨界 roundtrip 正确。
- trait 方法返回 FfiFuture 成立（FfiFuture 加 `#[stabby::stabby]` 后过 checked vtable 校验）。
- panic 收敛成立（impl 侧 catch_unwind，与入口宏同一思路，收敛为 RResult::Err 不跨界 unwind）。

**决策：保持保守 vtable 形态（spec §3 默认），不升级 dynptr。** 理由：

1. **收益薄**：core 侧无论如何要包 FfiXxxBackend 适配器（实现 core trait、持 handle +
   vtable 转发），dynptr 省掉的只是手写 vtable 结构体的几十行声明。
2. **tx 句柄化跑不掉**：FfiFuture payload 是 RBytes（纯字节），无法携带 Dyn 对象，
   begin→tx_id 的句柄表两种形态下都必须存在——dynptr 不能统一模型。
3. **成本真实**：方法签名全 extern "C" 化、生成辅助 trait（`PingerDyn`，host 必须 import
   才有方法）、`checked` vtable 的编译期机器、stabby dynptr 文档薄（官方用例即 crate
   内 tests）。调试期 vtable 字段可直接打印，dynptr 是 opaque。
4. **风险不对称**：vtable 样例一次跑绿；dynptr 样例首编即遇辅助 trait 不可见、
   泛型 vtable bound 报错两处摩擦。

升级路径保留：若未来插件方法数爆炸（vtable 维护成本超过适配器收益），契约 crate
同一 trait 加 `#[stabby::stabby(checked)]` 即可平移，host 侧改动局限于适配器层。

## 回写结论

- spec §3 与 Task 3.1/3.3 的 vtable 描述**不变**（经 spike S.3 复核确认）。
- FfiFuture 结构体在契约 crate 中加 `#[repr(C)]` 即可（`#[stabby::stabby]` 标注可选，
  dynptr 评估已验证加上也无碍，但保守形态不依赖它）。
