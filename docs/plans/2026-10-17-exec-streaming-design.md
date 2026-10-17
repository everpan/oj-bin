# exec 流式输出（A 回调 + B 句柄）共生设计稿

- 版本目标：v0.1.78（未打标签，沿用 CHANGELOG 惯例）
- 关联实现：src/bridge/exec.rs（op_exec_run / run_managed / drain）、src/bridge/bootstrap.js、oj/Cargo.toml（版本）、docs/devkit/*、CHANGELOG.md
- 评审要求：5 路专家（工程师 / 架构师 / 产品 / 安全 / 运行时性能）只读评审，通过即落地。

## 1. 背景与问题

当前 exec.run(argv, opts?)（src/bridge/exec.rs:259）是一次性缓冲语义：子进程完全退出（或被超时/超限击杀）后，才把 { code, stdout, stderr } 一次性返回 JS。长耗时命令在 await 期间对调用方完全不可见：

- run_managed（exec.rs:355）把 stdout/stderr 经 drain（exec.rs:469）累积进 Vec<u8>，无增量上报。
- OUTPUT_LIMIT = 10 MiB（exec.rs:230）：超限即 kill + 丢弃已缓冲内容。
- MAX_TIMEOUT_MS = 600_000（10 分钟，exec.rs:232）：超时被整组 kill。

需要让调用方实时掌握输出。目标：A（回调式）与 B（句柄/异步迭代）共生，且 B 作为统一底座，A 建在其上。

## 2. 设计原则

1. 单一流式底座：只实现一套 spawn 子进程 -> 并发 drain 两条管道 -> 边读边推进内核，A 与 B 都是它的消费形态，不维护两套读循环。
2. B 是底座，A 是 JS 糖：Rust 只暴露 spawn / read / kill / exit 四个 op；exec.run 的回调形态在 bootstrap.js 内用 spawn + for await 转发实现，Rust 不持有/不调用 JS 回调（规避在 op 内 invoke JS function 的复杂度与 !Send 风险）。
3. 向后兼容：exec.run(argv) 无 opts 时行为、返回形状与现状完全一致，现有 S1-S12 测试不改动。
4. 复用既有安全闸门：授权裁决、模板匹配、${path:}/${raw:} 校验、命令解析（default_resolve_command）、env 白名单、kill_tree 整组击杀——全部复用，不新增注入面。
5. 每请求资源回收（评审 CRITICAL 门禁）：池化 JsRuntime 复用前必须清空 OpState 的 ResourceTable，否则跨请求/跨租户泄漏 exec 句柄/子进程，且 next_rid 的 u32 计数回绕会 panic（DoS）。在 RuntimePool::checkin（runtime.rs:188）归还前、以及 checkout_reset（mod.rs:877）双重保险，枚举 resource_table.names() 逐个 close()（close 会回调 Resource::close()，即 ExecHandleResource::close 触发 kill_tree）。同时：ExecHandleResource::Drop / close 必须同步 kill_tree，与 supervisor/事件循环是否驱动无关——覆盖 spawn 失败、op 异常、runtime 被丢弃等异常路径。

## 3. JS API 形态

### 3.1 exec.spawn(argv, opts?) —— B（句柄式，新增）

```js
const h = await exec.spawn(["echo", "${raw:hi}"], { timeoutMs: 30000 });
// h = { rid, pid, stdout, stderr, kill, exit }
//   h.stdout / h.stderr 为 AsyncIterable<Uint8Array>
for await (const chunk of h.stdout) {
  log(new TextDecoder().decode(chunk));
}
const { code } = await h.exit();   // 等待退出，返回 { code }
h.kill();                          // 主动整组击杀（unix: kill(-pgid, SIGKILL)）
```

- opts：{ timeoutMs? }（缺省 DEFAULT_TIMEOUT_MS；硬上限 MAX_TIMEOUT_MS）。不含 stdin（本稿不增 stdin 流式写入，保持范围）。
- stdout/stderr 用 op_exec_read(rid, limit) 拉取；EOF 时迭代自然结束。
- exit() 返回 { code }（信号终止 128+sig，与现状一致）。
- rid 是 deno_core ResourceId，仅本 runtime（本请求/本 JS 上下文）可达。

### 3.2 exec.run(argv, opts?) —— A（回调式，扩展现有）

```js
// 无回调：行为同现状，返回 { code, stdout, stderr }
const r = await exec.run(["echo", "${raw:hi}"]);

// 有回调：增量转发，返回 { code }（不缓冲整段，避免与回调语义冲突）
const r2 = await exec.run(["sh", "-c", "for i in 1 2 3; do echo $i; sleep .2; done"], {
  onStdout: (chunk) => process.stdout.write(chunk),
  onStderr: (chunk) => process.stderr.write(chunk),
  timeoutMs: 5000,
});
// r2 = { code }
```

A 在 bootstrap.js 内实现为 spawn 之上的糖：spawn -> for await 两条流 -> 每 chunk 调 onStdout/onStderr -> 收集 exit() 的 code 返回。无回调时等同 spawn + 累积字符串。

## 4. Rust 内核（src/bridge/exec.rs）

### 4.1 数据结构

- ExecSession：持有 child（仅 ExecHandleResource 负责 kill）、total(Arc<AtomicUsize>) 累计字节守卫 OUTPUT_LIMIT、exit_watch（退出码广播）。drain 任务在 spawn 时启动，自持有 stdout/stderr pipe + 两个有界 mpsc::Sender。
- StreamResource：实现 deno_core::Resource，read 从 mpsc 接收端拉块；通道关闭且无残留 -> 空 BufView（EOF）。
- ExecHandleResource：持有 child + exit watch；close() 调 kill_tree 防孤儿进程组。

### 4.2 四个 op

- op_exec_spawn(state, argv, opts)：先调 prepare_exec（授权+模板+解析+重建 argv，**不含 spawn**），随后 spawn 子进程并将 child 句柄交给 supervisor 任务 + ExecHandleResource 共享持有，再：(1) 起两个 drain 任务（逻辑同现有 drain，但改为 send 进有界 mpsc，超限 -> kill_tree 并关通道）；(2) resource_table.add(StreamResource::stdout) -> stdoutRid；(3) resource_table.add(StreamResource::stderr) -> stderrRid；(4) resource_table.add(ExecHandleResource) -> rid；(5) 若 timeoutMs 给定起 sleep 任务到点 kill_tree；(6) 返回 { rid, pid, stdoutRid, stderrRid }。
- op_exec_read(state, rid, limit)：取 StreamResource，调其 read。
- op_exec_kill(state, rid)：取 ExecHandleResource，kill_tree。
- op_exec_exit(state, rid)：读 exit_watch，返回 { code }。

复用点：op_exec_run 现有的授权裁决（①-④）、命令解析（⑤）、argv 重建（⑥）整段抽为 prepare_exec（**到 argv 重建为止，不含 spawn/kill_tree**），op_exec_run 与 op_exec_spawn 共用；spawn 与 kill_tree 由各自路径负责（op_exec_run 走原 run_managed 缓冲路径并保留 kill_on_drop；op_exec_spawn 走新流式路径并将 child 交 supervisor 拥有）。

### 4.3 背压 / 上限 / 超时语义（与现状对齐并适配流式）

- 背压：mpsc 有界（建议 16 槽 x 8KiB）。JS 不 for await 拉 -> 通道满 -> drain 任务在 send 上挂起 -> 子进程管道满 -> 子进程自然暂停（真实子进程语义，正确）。
- 输出上限：drain 仍累计 total；超 OUTPUT_LIMIT -> kill_tree + 关通道。已流出的块调用方已收到，不丢（比现状更友好）。
- 超时：spawn 的 timeoutMs（同 DEFAULT/MAX 钳制）到点整组 kill；句柄 close()（JS 侧释放或 drop）同样 kill，避免孤儿进程组。

### 4.4 生命周期与清理（含资源回收门禁）

- 子进程退出 -> drain 任务 pipe 读到 EOF -> 关 mpsc 发送端 -> StreamResource.read 返回空 BufView（EOF）-> JS 迭代结束。
- ExecHandleResource::close()：kill_tree + drop child + 关 exit_watch。
- 仅 handle 资源负责 kill；两个 stream 资源 close() 只释放接收端，不 kill child。
- drain 任务与 child 同生命周期：handle close -> kill child -> pipe EOF -> drain 自然结束。
- **supervisor 拥有 child**：spawn 成功后启动一个 supervisor 任务（current_thread 内 spawn_local），持有 child 的强引用 + 监听 kill 信号 + exit_watch 广播；op_exec_run 不再独占 child，句柄与 supervisor 共享同一 child 句柄（Arc<Mutex<Child>> 或共享句柄）。child 的实际拥有者从「op 栈帧」转移到「ResourceTable 中的 ExecHandleResource + supervisor 任务」，消除「op 返回后 child 悬空」的问题。
- **资源回收门禁（评审 CRITICAL 修复）**：现状 RuntimePool::checkin（runtime.rs:188）仅 idle.push(rt) 不清资源表，且 checkout_reset（mod.rs:877）只 reset ReqState + fs 容器、不碰 ResourceTable——这就是跨请求/租户泄漏 + next_rid 回绕 panic 的根因。修复：
  - 在 checkin 归还前：遍历 `op_state.resource_table.names()`，对每个 rid 调 `close(rid)`（其回调 Resource::close()，对 ExecHandleResource 即 kill_tree），再 push。
  - 在 checkout_reset 再补一道：reset ReqState 后若表非空，再次 names()+close() 兜底（防御 runtime 被异常路径丢弃未走 checkin 的情形）。
  - 注意 deno_core ResourceTable 无 clear() 方法，只能逐个 close()。
- **异常路径回收**：ExecHandleResource::Drop 与 close 都同步 kill_tree（不依赖 supervisor/事件循环是否驱动），覆盖：(1) spawn 后 op 抛出异常；(2) JS 侧未消费即异常退出；(3) runtime 被 checkin 的堆守卫路径丢弃。KillSwitch 看门狗超时丢弃 runtime 时，该 runtime 的 ResourceTable 同样经上述 checkin/checkout_reset 兜底 close。
- !Send 下均在 current_thread 运行；kill_tree 为同步 syscall（unix: kill(-pgid, SIGKILL)），可在 Drop 中安全调用。

## 5. 安全评审关注点（给安全专家）

- 新 op spawn/read/kill/exit 复用 op_exec_run 的授权链（per_module grant、模板匹配、${path:}/${raw:} 校验、命令解析、env 白名单）。无新增命令构造面。
- rid 存于本 runtime 的 OpState.resource_table，仅本 JS 上下文可达；跨模块/跨请求无法凭 rid 触达他人会话（资源表不跨 runtime 共享）。
- kill 仅能杀自己 spawn 的会话（rid 归属当前资源表），无法杀外部进程（无 pid 入参，只接受本会话 rid）。
- 输出上限/超时守卫保留，防 spawn 后长期占用资源表槽位或吞输出。
- kill_on_drop(true) 已设；句柄 drop（含 JS 异常/超时熔断）兜底 kill。
- **资源回收保证（评审 CRITICAL）**：池化 JsRuntime 归还前在 checkin 枚举 resource_table.names() 逐个 close()（含 ExecHandleResource::close=kill_tree）；checkout_reset 再兜底一次。即使 JS 忘记 close 句柄、或 runtime 被 KillSwitch 看门狗丢弃，子进程组也会在归还/复用前被整组击杀，杜绝跨请求/租户泄漏与 next_rid 回绕 panic。异常路径（spawn 后 op 抛错、JS 异常、runtime 丢弃）由 ExecHandleResource::Drop 同步 kill_tree 覆盖。
- supervisor 任务拥有 child 强引用，op 返回后 child 不悬空；kill 只经 ExecHandleResource（本会话 rid）发起，无法触达外部进程。

## 6. 测试矩阵（在 exec.rs 现有 BDD 框架内新增 S-stream 组）

复用 make_bridge + run_module + api.js handler 形态（同 S1-S12）：

- SS1 spawn 返回 { rid, pid, stdoutRid, stderrRid }，h.stdout 为 AsyncIterable。
- SS2 增量：命令 sh -c 'printf 1; sleep .15; printf 2; sleep .15; printf 3' -> for await 至少 3 次 chunk，且在子进程结束前已收到前序 chunk（验证实时性）。
- SS3 h.exit() 返回 { code: 0 }（正常）/ 非 0 退出码透传。
- SS4 h.kill() 中途调用 -> 后续 chunk 停止、迭代结束、无残留进程组（wait_group_gone 同 S5/S6 断言）。
- SS5 输出 > 10MiB -> 仍被 OUTPUT_LIMIT kill（spawn 路径下流中断，已流出部分丢弃/截断，调用方收到 EOF）。
- SS6 timeoutMs 短于命令 -> exit() 前会话已被 kill（等价于 S6 的 timed out 行为）。
- SS7 回调形态：exec.run(..., { onStdout }) 在子进程结束前被调用 >=1 次，返回 { code }。
- SS8 向后兼容：exec.run(argv) 无 opts -> 返回 { code, stdout, stderr } 与 S1 一致，现有 S1-S12 全部不动。
- SS9 安全：未授权模块调用 spawn -> NotCapable（复用 denied! 宏）。

## 7. 文档与版本

- docs/devkit/api-manual.md：exec.run 增 onStdout/onStderr/timeoutMs 说明 + 错误表；新增 exec.spawn 章节（句柄、stdout/stderr 异步迭代、kill、exit、超时、上限、背压语义）。
- docs/devkit/scenarios.md：照抄实时日志流式 + 长任务中途 kill 场景。
- docs/devkit/SKILL.md 陷阱：spawn 后必须 for await 消费否则背压阻塞子进程；回调形态不返回整段 stdout；rid 仅本上下文可达。
- CHANGELOG.md：## v0.1.78（未打标签）列本特性。
- oj/Cargo.toml：version = "0.1.78"。
- cargo xtask build 归置 bin/devkit/，并由 cargo test --release -p xtask 校验。

## 8. 实现步骤（评审通过后执行）

0. 资源回收门禁（先做，评审 CRITICAL）：runtime.rs::checkin（:188）归还前 enumerate `op_state(rt).resource_table.names()` 逐个 close()；mod.rs::checkout_reset（:877）reset ReqState 后再兜底 names()+close() 一次（防御 runtime 被异常路径丢弃）。覆盖 ExecHandleResource::close/kill_tree。
1. exec.rs：抽 prepare_exec（授权+模板+解析+argv 重建，**不含 spawn/kill_tree**）；新增 StreamResource / ExecHandleResource（Drop 与 close 同步 kill_tree，覆盖异常路径）+ op_exec_spawn（spawn 并交 supervisor 拥有 child）/ op_exec_read / op_exec_kill / op_exec_exit；op_exec_run 维持缓冲路径不变。
2. mod.rs：注册 4 个新 op；导入 deno_core::Resource / BufView / ResourceTable。
3. bootstrap.js：改 exec.run 为无回调走缓冲、有回调走 spawn 转发；新增 exec.spawn 包装（构造句柄对象 + AsyncIterable）。
4. 测试 SS1-SS9。
5. 文档 + 版本 + devkit 同步。
6. 门禁：cargo fmt --check / cargo clippy --release --workspace --all-targets -D warnings / cargo test --release -p only-js --lib / cargo test --release -p xtask。
