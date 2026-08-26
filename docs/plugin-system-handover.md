# 插件系统交接文档（快照 2026-08-25，HEAD 849017e）

> **面向接手人**：本文是插件系统实施的中途交接。读完本节 + §5（剩余工作）即可接手；
> 深挖时再查 §4（关键概念）与文末（文件地图 / 测试命令）。
> 规划文件：`docs/superpowers/plans/2026-08-25-plugin-system.md`（下称「计划」）；
> 设计文档：`docs/superpowers/specs/2026-08-25-plugin-system-design.md`（下称「spec」）。

## 1. 这是什么

把 `only-js` 的五个后端轴（db/blob/bus/kv/es）从编译期写死改造为
**cdylib 动态库插件系统**：core 持 op + 注册表，插件为纯工厂 `.so/.dylib/.dll`，
启动期 libloading 装配。四层：装配层（`oj/src/server_cmd.rs`）→ 插件层（`oj-*` cdylib）
→ 框架层（core：五注册表 + PluginLoader + `ffi.rs` 全部 unsafe）→ 契约层（`oj-plugin-ffi`）。
完整设计见 spec，冲突以 spec 为准。

## 2. 当前状态总览

| 阶段 | 状态 |
|---|---|
| 阶段 0-5（注册表 → 全量 cdylib 化 → 文档） | ✅ 完成（提交历史 6707c03..b14bd6b） |
| Task 6.1 硬验收（10 步） | ✅ 完成（commit `849017e`） |
| Task 6.2 Step 1 代码 review | ✅ 已完成（本文 §3 记录结论） |
| Task 6.2 Step 2 吸收 review 意见 | ✅ 完成（I-1 + I-2 全量 shim + M-1..M-4，见 §5 接手点 A 标注） |
| Task 6.3 收尾（全量回归 + 勾销 + spec 状态头） | ✅ 完成（全量回归绿 + 计划复选框勾销 + spec 状态改「已实现」） |

**接手点 A / B 均已完成**（2026-08-25）。I-2 采用「插件侧全量补 shim」方案；M-1..M-4 全部处理。
计划 `Task 6.2/6.3` 复选框已勾销，spec 状态头已改「已实现」，相关提交见 git log。

## 3. review 结论（Task 6.2 Step 1 产出）

**范围**：`6707c030..849017e` 全 diff；重点审计计划要求的 ffi 审计清单两项、适配器层
转发、插件互不可见、spec §6 fail-fast 逐条。

### 审计清单核查（两项均达标）

- **✓ Library 句柄不 drop** — `src/bridge/ffi.rs:12-29` `load_forget` 加载成功即
  `Box::leak` 返回 `&'static Library`，任何路径不 dlclose（含装配失败、正常退出）；
  失败路径 `Err` 中无 Library，无泄漏。✓ spec §6。
- **✓ panic=unwind profile** — 根 `Cargo.toml:51-54` `[profile.release] panic = "unwind"`，
  全仓无成员覆盖为 abort。✓ spec §3。

### 适配器层 / 互不可见 / fail-fast 矩阵（逐条核对通过）

- RString/RBytes 双向封送正确；JSON 返回编码与插件一致（kv get=`Option<String>` 含
  miss 的 `null`、expire=`bool`、incr=`i64`、connect/begin=`{"handle"}/{tx_id}`）。
- FfiFuture 生命周期 airtight：take→free→state 置 null；FfiGuard drop 只 free 不 take；
  无 double-free 路径。五轴 Drop 全调 vtable close，close 插件侧 `HashMap::remove` 幂等，
  未知 handle → Err。
- HostContext 仅 log+deliver，无 registry lookup → 插件互不可见 ✓。
- spec §6 八类 fail-fast 全有变体 + 测试（清单去重 / 缺文件 / semver / ABI 双门 /
  身份 / 注册冲突 / 配置声明未装四闸门 / 损坏插件扫描）。

**测试实证**（本机跑过）：ffi 适配器 27 过、plugin_loader 17 过（含新增
`init_panic_is_classified_error`、子进程 `panic_hook_attribution_line_emitted`、
`scan_bad_plugin_is_err_not_skipped`）。唯一失败 `bridge::inspector::
session_forwards_cdp_and_close` 需真 Chrome CDP 端点，与本工作无关。

### 发现的问题（接手清单的核心）

| 级 | 编号 | 位置 | 一句话 |
|---|---|---|---|
| Important | **I-1** | `src/bridge/ffi.rs:499-515` | bus subscribe 失败后僵尸注册 + 首次订阅竞态（rabbitmq 重复投递） |
| Important | **I-2** | `oj-plugin-ffi/src/lib.rs:121-144` | 运行期 vtable 方法未包 catch_unwind，同步 panic 跨界 UB，与 spec §3 承诺不符 |
| Minor | M-1 | `ffi.rs:518-523` | `FfiEventBroker::drop` 清空整个全局 `DELIVER_TARGETS` |
| Minor | M-2 | `plugin_loader.rs:274-276` | cfg_for 宿主 panic 时 `CURRENT_PLUGIN` 残留旧名，后续归因错 |
| Minor | M-3 | `ffi.rs:215` | `FfiDbBackend::connect` 丢 `config_dir`，插件无相对路径 DSN 语义 |
| Minor | M-4 | `ffi.rs:32-48` | 加载错误分类关键词启发式，macOS 文本文件落 DependencyResolution |

详细修复方案见 §5 接手点 A。

## 4. 关键概念速查

- **契约 crate `oj-plugin-ffi`**（唯一允许跨边界）：
  - `ABI_VERSION=5` 严格等值是唯一硬门禁（`plugin_loader.rs:258-286` 双道校验：
    符号 + descriptor 内值）；构建指纹不匹配仅告警。
  - stabby repr(C) 容器 `RString/RVec/RBytes/RResult/RArc`；stabby 72 陷阱：
    `RResult` 不能模式匹配，用 `std::result::Result::from(r)` 转后 match。
  - `HostContext{log, deliver}` 函数指针回调集；**不提供 registry lookup**。
  - `oj_plugin_entry!` 宏生成两个 `#[no_mangle]` 符号：`oj_plugin_abi_version` +
    `oj_plugin_init`（内建 `catch_unwind` 把 init 期 panic 收敛为 `RResult::Err`）。
- **FfiFuture**（`oj-plugin-ffi/src/future.rs`）：`poll 0/1/-1`、`take` 取一次、
  `free` 释放（null 安全）；宿主 `take→free→state 置 null`；取消只 free 不 take，
  插件任务允许跑完。插件侧 oneshot `try_recv` 消费式，取到必须暂存。
- **句柄约定**：connect 产 `AtomicU64` handle，close `HashMap::remove`（幂等）；
  es 单 endpoint 特殊——宿主 `FfiEsBackend::new(0, vt)` 硬编码 handle 0，插件 init
  时预置 `clients.insert(0, ..)`。
- **加载失败分类**（`PluginLoadError` 七变体，各自独立文案）：FileMissing /
  PlatformMismatch（含 glibc）/ DependencyResolution / AbiMismatch / SymbolMissing /
  IdentityMismatch（含 semver pin 不符）/ InitFailed。
- **四级路径解析**：`OJ_PLUGINS_DIR` > `oj.toml plugins_dir` > `<exe>/plugins` >
  build.rs dev 后备；显式配置缺目录 → Err，缺省缺目录 → 零插件。

## 5. 剩余工作（接手清单）

> **✅ 接手点 A / B 已于 2026-08-25 全部完成。** 下方条目保留为历史记录，已落地处打了完成标注。

### 接手点 A：Task 6.2 Step 2 — 吸收 review 意见

**A-1（必做）I-1：bus subscribe 僵尸注册 + 竞态 — ✅ 已修复**

问题：
1. `FfiEventBroker::subscribe`（`ffi.rs:499-515`）先把 `tx` 推进 `DELIVER_TARGETS`，
   再 `await` vtable.subscribe。失败时返回 Err 但本地注册残留、`is_new_topic` 记为
   已起消费 → 该 topic 永不重试，订阅静默丢失。
2. 并发首次订阅同一新 topic 都过 `is_new_topic` 门 → 各起一个消费循环。kafka 同
   `group.id` 重平衡分摊（无害）；**rabbitmq topic 交换 + 每订阅者独享队列 → 每消息
   重复投递**给每个订阅者。

修复（已核对锁序无死锁：`deliver`/`publish` 不碰该 tokio 锁，仅 subscribe 使用）：
在 ffi.rs 顶部加 `static SUBSCRIBE_GATE: LazyLock<tokio::sync::Mutex<()>>`，subscribe
全程持锁；失败时回滚刚注册的本通道（列表空则删整条 topic）。代码草案：

```rust
async fn subscribe(&self, topic: &str, tx: UnboundedSender<String>) -> BridgeResult<()> {
    let _gate = SUBSCRIBE_GATE.lock().await;
    let tx_dup = tx.clone(); // 回滚判重用
    let start_consumer = {
        let mut g = DELIVER_TARGETS.lock().unwrap();
        let list = g.entry(topic.to_string()).or_default();
        let is_new_topic = list.is_empty();
        if !list.iter().any(|t| t.same_channel(&tx)) {
            list.push(tx);
        }
        is_new_topic
    };
    if start_consumer {
        let fut = (self.vtable.subscribe)(self.handle, RString::from(topic));
        if let Err(e) = await_ffi(fut).await {
            let mut g = DELIVER_TARGETS.lock().unwrap();
            if let Some(list) = g.get_mut(topic) {
                list.retain(|t| !t.same_channel(&tx_dup));
                if list.is_empty() {
                    g.remove(topic);
                }
            }
            return Err(ffi_err("bus subscribe", e));
        }
    }
    Ok(())
}
```

TDD 先写失败测试（`ffi.rs` adapter_tests，加 `BUS_SUBSCRIBE_FAIL: AtomicBool` 让
`mock_bus_subscribe` 可失败）：
- 首次订阅 vtable 失败 → `Err` + `host_deliver` 后通道 `try_recv` 为空（无僵尸）；
- 重试成功 → `BUS_SUBSCRIBES.len()==2`（消费循环被重新起）+ `host_deliver` 扇出可达。

**A-2（已决策 ✅）I-2：运行期 vtable 方法 panic 围堵缺口 — 采用「插件侧全量补 shim」**

spec §3 要求「契约入口宏对每个导出符号与注册回调统一包 catch_unwind」，但宏只包了
init；`search/publish/put/get/close` 等是裸 `extern "C" fn`，同步 panic 会穿过 C-ABI
跨界展开（UB/abort，宿主 catch_unwind 拦不到）。本次验收的「运行期 panic 围堵」只
证明了 tokio JoinHandle 收敛异步任务 panic。三个选项：
1. ~~文档化已知限制~~（改动最小）：spec §3/§6 如实写明同步 vtable 入口依赖
   插件侧纪律，异步任务 panic 已由 tokio 围堵；
2. **✅ 插件侧全量补 shim**（已落地）：oj-plugin-ffi 新增 catch_unwind 安全的 FfiFuture
   工厂（`spawn_ffi_future`/`catch_future`/`catch_void`/`catch_value`），七插件 vtable
   方法全量包，poll/take/free 统一为契约 crate 内 catch_unwind 版本，彻底兑现 spec；
3. ~~宿主侧包装~~：panic 已在插件侧穿出边界，宿主 catch_unwind 技术上拦不到，无效。

**A-3（顺手，可选）M-1..M-4 — ✅ 全部处理**：
- M-1（✅）：`FfiEventBroker::drop` 仅清本 broker 注册的目标（按 sender 归属过滤），
  不再整表清空，避免误伤其他 broker 订阅。
- M-2（✅）：`plugin_loader.rs` 用 RAII guard 管理 `CURRENT_PLUGIN`（panic 时自动
  恢复），消除残留归因。
- M-3（✅）：契约文档（`oj-plugin-ffi/src/db.rs` `connect`）注明插件 db connect 收不到
  config_dir（相对路径 DSN 语义仅内置后端）。
- M-4（✅）：`classify_load_error` 增补 `"image"`/`"file too short"` 关键词并注明启发式边界。

**A-4 完成标志**：修复各自 TDD 提交（消息尾注 `unix@vip.qq.com ai`）；最后按计划
Task 6.2 Step 3 提交 `fix(review): 插件系统完成度 review 意见吸收`。

### 接手点 B：Task 6.3 — 收尾 — ✅ 已完成

1. **全量最终回归**：`cargo test --workspace -- --skip infinite_loop` 全绿
   （见 §6 已知排除项）。
2. **计划勾销**：勾 Task 6.2/6.3 全部复选框（含 Task 6.2 Step 1 补勾），提交
   `docs(plan): 插件系统计划全部落地——阶段 0-6 收官`。
3. **spec 状态头**：`docs/superpowers/specs/2026-08-25-plugin-system-design.md` 首行
   `> 状态：` 改「已实现」并提交。

## 6. 环境与工具约束（接手前必读）

- **子代理不可用**：本环境 `Agent`/`fork` 全部 spawn 失败（后端只认
  `deepseek-v4-pro/flash/flash-vision-exp`，但代理模型解析恒为无效的 `k3`）。
  review 须**内联**做，不要依赖 `superpowers:requesting-code-review` 派发子代理。
- **存量 SIGSEGV**：`bridge::tests::infinite_loop_times_out_and_bridge_survives`
  （`src/bridge/mod.rs:995`）在 master 上即崩，与本项目无关——跑全套一律
  `-- --skip infinite_loop`。
- **env-gated 集成测试**（未设 env 自动 skip）：`OJ_TEST_S3` / `OJ_TEST_ES` /
  `OJ_TEST_REDIS` / `OJ_TEST_MYSQL` / `OJ_TEST_PG` / `OJ_TEST_KAFKA_BROKERS` /
  `OJ_TEST_RABBITMQ_URL`。后两者是 Task 6.1 Step 5 的真 broker 共享语义验收。
- **夹具插件**：`tests/plugins/mini`（首次测试时 `cargo build -p oj-plugin-test-mini`
  自动编译）；`MINI_FAKE_ABI`/`MINI_PANIC` 两个 env 可伪造 ABI/触发 init panic。
- **cdylib 不随 `cargo run` 重建**：改了 `oj-*` 插件源码后需 `cargo build -p oj-xxx`
  或 `cargo xtask plugin <name>` 才生效。
- **提交规范**：commit 消息尾注固定 `unix@vip.qq.com ai`。

## 7. 文件地图（改哪里找哪里）

| 职责 | 路径 |
|---|---|
| 契约层（vtable/宏/容器） | `oj-plugin-ffi/src/{lib,es,db,blob,bus,kv,future}.rs` |
| 全部 unsafe + 适配器层 | `src/bridge/ffi.rs`（`load_forget` + 每轴 `FfiXxxBackend`） |
| 加载器（路径解析/清单/扫描/七分类） | `src/bridge/plugin_loader.rs` + `tests.rs` |
| 装配层（清单去重/冲突 fail-fast/§2 闸门） | `oj/src/server_cmd.rs`（`assemble_plugins`/`build_registries`） |
| 插件自省 op | `src/bridge/plugins_op.rs` |
| 插件 cdylib | `crates/plugins/oj-es` `crates/plugins/oj-db-mysql` `crates/plugins/oj-db-postgres` `crates/plugins/oj-blob-s3` `crates/plugins/oj-bus-kafka` `crates/plugins/oj-bus-rabbitmq` `crates/plugins/oj-kv-redis` |
| 测试夹具插件 | `tests/plugins/mini` |
| 构建产物归置 | `cargo xtask plugin <name>` → `./plugins/<host-triple>/` |

## 8. 常用命令

```bash
cargo test -p only-js <过滤词>                 # 单测（推荐按模块过滤）
cargo test --workspace -- --skip infinite_loop       # 全套回归（Task 6.3）
cargo xtask plugin <name>                            # 单独编译 + 拷入 plugins/<triple>/
cargo build -p oj-es                                 # 改插件后手动重建 cdylib
```
