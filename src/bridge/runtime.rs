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
use super::{RunError, StableState, bridge_ext};

/// 池容量上限（空闲实例数）。设为 0 表示无上限（按需增长后保留）。
const DEFAULT_MAX_IDLE: usize = 16;

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
        JsRuntime::new(RuntimeOptions {
            extensions: vec![bridge_ext::init(stable.clone())],
            inspector: inspect,
            module_loader,
            ..Default::default()
        })
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
    pub fn checkin(&self, rt: JsRuntime) {
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
#[derive(Default)]
pub struct KillSwitch {
    slot: Mutex<Option<(v8::IsolateHandle, Instant)>>,
    fired: AtomicBool,
    /// Drop 时置位，通知看门狗线程退出（避免线程泄漏）。
    stop: AtomicBool,
    /// 看门狗线程句柄；Drop 时 join 回收（仅用于生命周期管理，不参与熔断逻辑）。
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
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
                    let g = sw.slot.lock().unwrap();
                    if let Some((h, deadline)) = g.as_ref()
                        && Instant::now() >= *deadline
                        && !sw.fired.load(Ordering::Relaxed)
                    {
                        // terminate_execution 是 V8 明确允许的跨线程调用（不要求进入 isolate）。
                        h.terminate_execution();
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
        *self.slot.lock().unwrap() = Some((handle, Instant::now() + timeout));
    }

    /// 关闭窗口；返回本窗口内是否触发过熔断。
    pub(crate) fn disarm(&self) -> bool {
        *self.slot.lock().unwrap() = None;
        self.fired.swap(false, Ordering::Relaxed)
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
