//! RuntimePool：复用 JsRuntime 实例，避免每请求新建 V8 isolate（1~10ms 开销）。
//!
//! 每个 pooled runtime 在创建时即加载 bridge_ext（含 bootstrap.js ESM 入口），故"快照"等价于
//! 一次预热后反复复用——bootstrap 只编译一次，后续请求仅执行 handler 源码。
//! 配合 `execute_script_with_cache` 可对 handler 源码做 V8 代码缓存，进一步摊薄编译成本。
//!
//! 由于 `JsRuntime` 是 `!Send`，池与持有它的 event loop 同线程（当前为 tokio current_thread），
//! 与现有 per-request `Bridge` 模型一致。跨请求的状态隔离由 `ReqState` 在 checkout 时重置保证。

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use deno_core::{JsRuntime, ModuleLoader, PollEventLoopOptions, RuntimeOptions, v8};

use super::module_loader::OjModuleLoader;
use super::{
    RunError, StableState, bridge_ext_init, patch_fs_loaded_sources, ws_client_extensions,
};

/// 池容量上限（空闲实例数）。设为 0 表示无上限（按需增长后保留）。
const DEFAULT_MAX_IDLE: usize = 16;

// ===== PR-4 isolate 堆限额（v0.1.40）=====

/// per-isolate 堆限额守卫的 fired 标志（存 OpState；回调置位，宿主读取决定丢弃）。
#[derive(Clone)]
pub struct HeapGuardFired(pub Rc<AtomicBool>);

/// V8 near-heap-limit 的 create_params（`heap_limits(0, limit)`；与
/// `install_heap_limit_callback` 成对使用）。None = 不限额（测试/嵌入式默认）。
pub fn heap_create_params(limit: usize) -> v8::CreateParams {
    v8::Isolate::create_params().heap_limits(0, limit)
}

/// 构造后安装 near-heap-limit 回调（deno_core 官方范式：`terminate_execution` +
/// 返回放大值给 V8 解卷余量——不放大 V8 会 hard abort 进程）。fired 标志注入
/// OpState（`heap_guard_fired` 读取；checkin/run 路径据此丢弃 isolate）。
pub fn install_heap_limit_callback(rt: &mut JsRuntime, limit: usize) -> Rc<AtomicBool> {
    let fired = Rc::new(AtomicBool::new(false));
    let f = fired.clone();
    let handle = rt.v8_isolate().thread_safe_handle();
    rt.add_near_heap_limit_callback(move |current_limit, _initial_limit| {
        f.store(true, Ordering::SeqCst);
        handle.terminate_execution();
        // **一次性抬「current + 限额」**：显式 heap_limits 下 V8 对新限额的二次逼近会
        // FatalProcessOutOfMemory（不重入回调），余量必须容下在途分配让 terminate 的
        // 中断检查抛错。返回值也不能是天文数字（V8 全局分配账本算术会 CHECK 崩）——
        // 已实测钉死：单个 > 限额的巨型分配仍可能崩进程（已知边界，文档登记）。
        current_limit.saturating_add(limit)
    });
    {
        let op_state = rt.op_state();
        op_state.borrow_mut().put(HeapGuardFired(fired.clone()));
    }
    fired
}

/// 该 isolate 是否已触发堆限额（未安装回调的 runtime 恒 false）。
pub fn heap_guard_fired(rt: &JsRuntime) -> bool {
    let op_state = rt.op_state();
    let g = op_state.borrow();
    g.try_borrow::<HeapGuardFired>()
        .map(|f| f.0.load(Ordering::SeqCst))
        .unwrap_or(false)
}

/// ext_boot 执行超时：boot 里的同步死循环不归还执行器（`tokio::time::timeout` 无效），
/// 只能靠 `terminate_execution`。量级对齐 `super::INTROSPECT_TIMEOUT`（同为启动期一次性
/// 执行），单独成常量以免两个用途互相耦合。
/// 注：「永不到达」的 TLA（`await new Promise(()=>{})`）不在此列 —— deno_core 会在
/// 事件循环排空时立即报 `Top-level await promise never resolved`。
pub const BOOT_TIMEOUT: Duration = Duration::from_secs(2);

/// JsRuntime 池。
pub struct RuntimePool {
    /// 共享稳定状态句柄（§5.3：run_module 按模块目录解析执行上下文用；boot spec 亦在此）。
    stable: Arc<StableState>,
    /// 新建 runtime 时透传的 inspector 开关。
    inspect: bool,
    /// boot 期武装的看门狗（与 `Bridge` 共享同一实例）。
    kill: Arc<KillSwitch>,
    /// 空闲 runtime 列表。
    idle: RefCell<Vec<JsRuntime>>,
    /// 空闲上限。
    max_idle: usize,
}

impl RuntimePool {
    /// 用共享的稳定状态构造池。inspect=是否启用 DevTools inspector。
    pub fn new(stable: Arc<StableState>, inspect: bool, kill: Arc<KillSwitch>) -> Self {
        Self {
            stable,
            inspect,
            kill,
            idle: RefCell::new(Vec::new()),
            max_idle: DEFAULT_MAX_IDLE,
        }
    }

    /// 共享稳定状态句柄（只读）。
    pub fn stable(&self) -> &Arc<StableState> {
        &self.stable
    }

    /// 新建一个 runtime（模块加载器取 StableState.loader，单一事实来源；
    /// devserver 旧路径 None → 不配）。
    fn spawn(stable: &Arc<StableState>, inspect: bool) -> JsRuntime {
        let module_loader = stable
            .loader
            .clone()
            .map(|inner| Rc::new(OjModuleLoader { inner }) as Rc<dyn ModuleLoader>);
        // PR-4：Some(limit) = create_params.heap_limits + near-heap-limit 回调；
        // 超限 terminate → fired 标志置位 → checkin 丢弃（不回池）。
        let create_params = stable.js_heap_limit.map(heap_create_params);
        let mut rt = JsRuntime::new(RuntimeOptions {
            extensions: {
                let mut extensions = ws_client_extensions();
                extensions.push(bridge_ext_init(stable.clone()));
                // deno_* 扩展以构建机绝对路径声明 JS；进 runtime 前统一换为内嵌源码，
                // 否则在非构建机上 JsRuntime 初始化即 ENOENT。
                patch_fs_loaded_sources(&mut extensions);
                extensions
            },
            inspector: inspect,
            module_loader,
            create_params,
            ..Default::default()
        });
        if let Some(limit) = stable.js_heap_limit {
            install_heap_limit_callback(&mut rt, limit);
        }
        rt
    }

    /// 借出一个 runtime（优先复用空闲，否则新建并执行 ext_boot）。
    ///
    /// **不变量**：借出的 runtime 一定已 boot（`StableState.boot` 为 Some 时）。
    /// 这是唯一的借出入口，故 boot 只需在此内联一处。
    pub async fn checkout(&self) -> Result<JsRuntime, RunError> {
        // `borrow_mut()` 的临时 `RefMut` 必须在本句末 drop，绝不跨 await：boot 的
        // event loop 会让出执行器，此时 checkin() 的 borrow_mut() 会撞双重借用 panic。
        // （`RefCell` 非 Send，此处无法靠 lint 兜底 —— 见 lib.rs 的 allow(clippy::all)。）
        if let Some(rt) = self.idle.borrow_mut().pop() {
            return Ok(rt);
        }
        let Some(spec) = self.stable.boot.clone() else {
            return Ok(Self::spawn(&self.stable, self.inspect));
        };
        let mut rt = Self::spawn(&self.stable, self.inspect);
        // boot 期武装看门狗：同步死循环（或悬而未决的 op）卡在 isolate 里不归还执行器，
        // `tokio::time::timeout` 不会触发，只能靠跨线程 terminate_execution。
        self.kill
            .arm(rt.v8_isolate().thread_safe_handle(), BOOT_TIMEOUT);
        let result = boot_runtime(&mut rt, &spec).await;
        let fired = self.kill.disarm();
        // 熔断优先：fired 时 result 多半也带着终止错误，但语义上属于超时（408），
        // 不能让 Core 分支抢先（否则 boot 挂死被误报成 500）。
        // 未轮询完 event loop 的 isolate 析构会触发 V8 句柄错误（本项目有 SIGSEGV 前科），
        // 故两条失败路径都兜底跑一轮再丢弃，且 runtime 绝不归还池。
        if fired {
            let _ = rt.run_event_loop(PollEventLoopOptions::default()).await;
            return Err(RunError::Timeout);
        }
        // PR-4：boot 期堆超限 → Oom（isolate 丢弃，池可继续 checkout 新实例）。
        if heap_guard_fired(&rt) {
            let _ = rt.run_event_loop(PollEventLoopOptions::default()).await;
            return Err(RunError::Oom {
                limit: self.stable.js_heap_limit.unwrap_or(0),
            });
        }
        match result {
            Ok(()) => Ok(rt),
            Err(e) => {
                let _ = rt.run_event_loop(PollEventLoopOptions::default()).await;
                Err(RunError::Core(e))
            }
        }
    }

    /// 归还一个 runtime 到空闲池（超出上限则丢弃，由 drop 析构 V8 isolate）。
    /// 仅归还已成功执行过 event loop 的 runtime（未轮询的 isolate 析构会触发 V8 句柄错误）。
    /// PR-4：堆限额已触发的 isolate **绝不回池**（堆已膨胀；防止 handler try/catch
    /// 吞掉终止错误后把坏 isolate 留在池里）。
    pub fn checkin(&self, rt: JsRuntime) {
        if heap_guard_fired(&rt) {
            eprintln!("warn: runtime discarded (js heap limit fired; not returned to pool)");
            return;
        }
        let mut idle = self.idle.borrow_mut();
        if idle.len() < self.max_idle {
            idle.push(rt);
        }
    }
}

/// 在给定 runtime 上执行脚本并驱动 event loop 至所有 Promise 落定。
/// "快照/预热"由 RuntimePool 复用已加载 bootstrap 的 runtime 实现；此处仅执行 handler 源码。
pub async fn run_to_completion(
    rt: &mut JsRuntime,
    name: &'static str,
    source: String,
) -> Result<(), deno_core::error::CoreError> {
    rt.execute_script(name, source)?;
    rt.run_event_loop(PollEventLoopOptions::default()).await?;
    Ok(())
}

/// 执行 ext_boot 模块一次：以 side module 加载 `await import("<spec>")` 并驱动 event loop。
///
/// `spec` 是装配期冻结的 `file://…?v=<mtime>`（见 `module_loader::versioned_specifier`），
/// 走 `OjModuleLoader`：.ts 缓存转译、相对/裸导入、CJS 互操作、`ensure_within` 全部复用。
/// driver spec 固定（每 JsRuntime 有独立 module map，与递增的 `file:///oj/driver/{n}.js`
/// 不冲突；boot 每 runtime 仅一次）。
///
/// 调用方负责武装看门狗，以及失败时的 event loop 兜底与丢弃（本函数不持有这些策略）。
pub async fn boot_runtime(
    rt: &mut JsRuntime,
    spec: &str,
) -> Result<(), deno_core::error::CoreError> {
    let driver_spec = deno_core::ModuleSpecifier::parse("file:///oj/ext_boot.js")
        .map_err(|e| deno_core::error::CoreError::from(std::io::Error::other(e.to_string())))?;
    let code = format!("await import(\"{spec}\");\n");
    // 顺序以 0.410 签名为准（同 `Bridge::run_side_driver`）：mod_evaluate 返回
    // `impl Future + use<>`（不借 runtime），先启动求值再驱动 event loop，最后 await
    // 求值 future 取 TLA 错误。
    let id = rt.load_side_es_module_from_code(&driver_spec, code).await?;
    let eval = rt.mod_evaluate(id);
    rt.run_event_loop(PollEventLoopOptions::default()).await?;
    eval.await?;
    Ok(())
}

/// 取 runtime 的 OpState 句柄（用于 checkout 时重置 per-request 状态）。
pub fn op_state(rt: &JsRuntime) -> Rc<RefCell<deno_core::OpState>> {
    rt.op_state()
}

/// 超时熔断开关：arm 记录 v8::IsolateHandle + deadline；看门狗线程到期跨线程 terminate。
/// IsolateHandle 是 V8 提供的跨线程终止官方途径（Send+Sync，内部 Arc<IsolateHandleInner>），
/// 持有真实 isolate 指针且自带生命周期管理——不必（也不能）手存裸指针。
///
/// 生命周期与 `Bridge` 绑定：看门狗线程仅持 `Weak<KillSwitch>`，`Bridge` 析构触发本结构
/// `Drop` 时置位 `stop` 并 join 线程——进程退出 / 单测结束均无残留线程。此前每条测试各泄漏
/// 一个看门狗线程，glibc 进程退出时这些仍在自旋的线程与 V8 平台析构相互干扰，导致 SIGSEGV
/// （macOS 容错更强，故仅 Linux CI 暴露）。若 `Bridge` 在 armed 状态下被丢弃（请求中途
/// panic），`Drop` 会先清空 slot 中的 isolate 句柄，杜绝看门狗在 isolate 已析构后误
/// `terminate_execution`（同样会 SIGSEGV）。
/// 每个 Bridge 一个实例（对应一个 JS actor 线程，串行执行故单槽足够）。
/// 看门狗单槽的一次武装。`deadline` 到点即 terminate；`gate`（任务停机 flag + grace）
/// 供「deadline 未设」时由看门狗代设——flag 置位可能发生在 mod_evaluate 同步自旋
/// microtask 期间，宿主线程根本轮不到自己观察，只能由看门狗代盯。
struct Arm {
    handle: v8::IsolateHandle,
    deadline: Option<Instant>,
    gate: Option<(Arc<AtomicBool>, Duration)>,
}

pub struct KillSwitch {
    slot: Mutex<Option<Arm>>,
    fired: AtomicBool,
    /// Drop 时置位，通知看门狗线程退出（避免线程泄漏）。
    stop: AtomicBool,
    /// 看门狗线程句柄；Drop 时 join 回收（仅用于生命周期管理，不参与熔断逻辑）。
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Default for KillSwitch {
    fn default() -> Self {
        Self {
            slot: Mutex::new(None),
            fired: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            thread: Mutex::new(None),
        }
    }
}

impl KillSwitch {
    /// 创建并启动看门狗线程（随 `Bridge` 生命周期，25ms 轮询粒度）。
    pub fn spawn() -> Arc<Self> {
        let sw = Arc::new(Self::default());
        // 线程只持 Weak：避免强引用环使 Drop 永不触发（→ 线程泄漏且无法 join）。
        let weak = Arc::downgrade(&sw);
        let handle = std::thread::Builder::new()
            .name("js-watchdog".into())
            .spawn(move || {
                while let Some(sw) = weak.upgrade() {
                    if sw.stop.load(Ordering::Relaxed) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(25));
                    if sw.stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let mut g = sw.slot.lock().unwrap();
                    let Some(arm) = g.as_mut() else {
                        continue;
                    };
                    // 代盯 gate：flag 置位且 deadline 未设 → 现在开始计 grace。
                    if let Some((gate, grace)) = &arm.gate
                        && arm.deadline.is_none()
                        && gate.load(Ordering::Relaxed)
                    {
                        arm.deadline = Some(Instant::now() + *grace);
                    }
                    if let Some(deadline) = arm.deadline
                        && Instant::now() >= deadline
                        && !sw.fired.load(Ordering::Relaxed)
                    {
                        // terminate_execution 是 V8 明确允许的跨线程调用（不要求进入 isolate）。
                        arm.handle.terminate_execution();
                        sw.fired.store(true, Ordering::Relaxed);
                    }
                }
            })
            .expect("spawn js-watchdog");
        *sw.thread.lock().unwrap() = Some(handle);
        sw
    }

    pub(crate) fn arm(&self, handle: v8::IsolateHandle, timeout: Duration) {
        self.fired.store(false, Ordering::Relaxed);
        *self.slot.lock().unwrap() = Some(Arm {
            handle,
            deadline: Some(Instant::now() + timeout),
            gate: None,
        });
    }

    /// 任务常驻驱动专用：武装后由看门狗代盯停机 flag——flag 置位即起算 grace，
    /// 到点跨线程 terminate。之所以不宿主自己盯：mod_evaluate 的初始 microtask
    /// checkpoint 会被「TLA 紧循环 + 同步就绪 op」饿死到永不返回，宿主线程连
    /// select 的协作分支都轮不到；强杀只能出自看门狗线程（评审 F5）。
    pub(crate) fn arm_on_flag(
        &self,
        handle: v8::IsolateHandle,
        gate: Arc<AtomicBool>,
        grace: Duration,
    ) {
        self.fired.store(false, Ordering::Relaxed);
        *self.slot.lock().unwrap() = Some(Arm {
            handle,
            deadline: None,
            gate: Some((gate, grace)),
        });
    }

    /// 关闭窗口；返回本窗口内是否触发过熔断。
    pub(crate) fn disarm(&self) -> bool {
        *self.slot.lock().unwrap() = None;
        self.fired.swap(false, Ordering::Relaxed)
    }

    /// 熔断是否已触发（不清位；供宿主线程观察后放弃等待被终止的 future）。
    pub(crate) fn fired(&self) -> bool {
        self.fired.load(Ordering::Relaxed)
    }
}

impl Drop for KillSwitch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Take the thread handle out of the mutex, releasing the lock on the mutex.
        let thread_handle = self.thread.lock().unwrap().take();
        // Drop 可能由看门狗线程自身触发：循环体持有的 upgrade() 强引用可能是最后一份
        // （Bridge 已先析构），此时 join 自己会 EDEADLK panic。线程随即因
        // weak.upgrade() == None 退出，跳过 join 即可。
        if let Some(handle) = thread_handle
            && handle.thread().id() != std::thread::current().id()
        {
            let _ = handle.join();
        }
        // Now that the thread has joined, we can safely set the slot to None.
        *self.slot.lock().unwrap() = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 回归：Bridge 析构后，看门狗循环体持有的 upgrade() 强引用成为最后一份时，
    /// KillSwitch::drop 在看门狗线程上执行并 join 自己 → EDEADLK panic。
    /// 通过全局 panic hook 捕获该 panic；修复后不应触发。
    #[test]
    fn drop_while_watchdog_holds_last_ref_does_not_self_join() {
        use std::sync::atomic::Ordering as AtomicOrdering;
        let caught = Arc::new(AtomicBool::new(false));
        // prev 挂 Arc：hook 内转发原 hook（不吞并行测试的 panic 诊断），结束后再装回。
        let prev: std::sync::Arc<dyn Fn(&std::panic::PanicHookInfo<'_>) + Send + Sync> =
            std::sync::Arc::from(std::panic::take_hook());
        {
            let caught = caught.clone();
            let prev = prev.clone();
            std::panic::set_hook(Box::new(move |info| {
                if info.to_string().contains("failed to join thread") {
                    caught.store(true, AtomicOrdering::Relaxed);
                } else {
                    prev(info);
                }
            }));
        }
        {
            let _sw = KillSwitch::spawn();
            // 等 50ms：看门狗 25ms 轮询，此刻几乎必然处于循环体中持有强引用；
            // 主线程随后 drop 自己的 Arc，让最后一份引用落在看门狗线程上。
            std::thread::sleep(Duration::from_millis(50));
        }
        std::thread::sleep(Duration::from_millis(100));
        let prev_restore = prev.clone();
        std::panic::set_hook(Box::new(move |info| prev_restore(info)));
        assert!(
            !caught.load(AtomicOrdering::Relaxed),
            "js-watchdog self-join panic (EDEADLK) is back"
        );
    }
}
