# Spike：FFI 选型记录（S.1–S.3）

日期：2026-08-25。工作区：`spikes/ffi-select`（独立 workspace，不并入主仓依赖）。

## S.1 双库样例结论

**选型：stabby（72.1.x）。**

实证（`cargo run -p host` 全绿，输出 "ALL SPIKE S.1 CHECKS PASSED"）：

- RString 按值跨界 roundtrip 一致（`echo:hello`）。
- ABI_VERSION u32 等值门禁在宿主侧一行断言即可实现。
- `catch_unwind(AssertUnwindSafe)` 把插件 panic 收敛为 `Result::Err`，进程不 abort
  （panic hook 仍会打印消息——宿主可装 hook 降级为日志，见计划 Task 3.2）。
- `#[stabby::stabby] #[repr(C)] struct PluginDescriptor` 按值 roundtrip 一致。

abi_stable 同接口样例也跑通，对比结论：

- 人体工学相当（RString/RResult 形态几乎一样）。
- stabby 优势：不强制 RootModule 加载仪式（abi_stable 惯例走 RootModule 集中导出 +
  库名/版本校验，对我们"五轴各自裸 vtable 导出"的形态反而绕路）；
  `#[stabby]` 属性宏直接标注任意 repr(C) 结构，契约 crate 写法更轻。
- abi_stable 的 TypeLayout 校验是加分项，但它校验的是"同一 crate 版本编译两侧"
  场景；我们的插件独立编译，真正防线仍是 ABI_VERSION 等值门禁 + repr(C) 纪律，
  两者拉平。

### stabby 72.1 使用注意（写进契约 crate 约定）

- `stabby::result::Result` 的 `Ok`/`Err` 是**关联函数**（构造用 `RResult::Ok(v)`），
  **不能模式匹配**；消费侧 `std::result::Result::from(r)` 转换后匹配，或用
  `.match_ref(ok, err)`。
- `env!` 第二参数是错误消息而非默认值——编译期可选值用
  `option_env!("X").unwrap_or("default")`。
- panic=unwind 必须保留（workspace `[profile.dev] panic = "unwind"`），
  否则 catch_unwind 形同虚设。

## S.2 FfiFuture + 插件自建 tokio runtime

（待填）

## S.3 tx 句柄化 + dynptr 评估

（待填）
