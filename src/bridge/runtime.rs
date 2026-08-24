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
use super::{StableState, bridge_ext};

/// 池容量上限（空闲实例数）。设为 0 表示无上限（按需增长后保留）。
const DEFAULT_MAX_IDLE: usize = 16;

/// JsRuntime 池。
pub struct RuntimePool {
    /// 构造新 runtime 的工厂（捕获共享的 StableState）。
    make: Box<dyn Fn() -> JsRuntime>,
    /// 空闲 runtime 列表。
    idle: RefCell<Vec<JsRuntime>>,
    /// 空闲上限。
    max_idle: usize,
}

impl RuntimePool {
    /// 用共享的稳定状态构造池。inspect=是否启用 DevTools inspector。
    pub fn new(stable: Arc<StableState>, inspect: bool) -> Self {
        Self {
            make: Box::new(move || {
                // 模块加载器取 StableState.loader（单一事实来源；devserver 旧路径 None → 不配）。
                let module_loader = stable.loader.clone().map(|inner| {
                    Rc::new(OjModuleLoader { inner }) as Rc<dyn ModuleLoader>
                });
                JsRuntime::new(RuntimeOptions {
                    extensions: vec![bridge_ext::init(stable.clone())],
                    inspector: inspect,
                    module_loader,
                    ..Default::default()
                })
            }),
            idle: RefCell::new(Vec::new()),
            max_idle: DEFAULT_MAX_IDLE,
        }
    }

    /// 借出一个 runtime（优先复用空闲，否则新建）。
    pub fn checkout(&self) -> JsRuntime {
        if let Some(rt) = self.idle.borrow_mut().pop() {
            return rt;
        }
        (self.make)()
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

/// 取 runtime 的 OpState 句柄（用于 checkout 时重置 per-request 状态）。
pub fn op_state(rt: &JsRuntime) -> Rc<RefCell<deno_core::OpState>> {
    rt.op_state()
}

/// 超时熔断开关：arm 记录 v8::IsolateHandle + deadline；看门狗线程到期跨线程 terminate。
/// IsolateHandle 是 V8 提供的跨线程终止官方途径（Send+Sync，内部 Arc<IsolateHandleInner>），
/// 持有真实 isolate 指针且自带生命周期管理——不必（也不能）手存裸指针。
/// 每个 Bridge 一个实例（对应一个 JS actor 线程，串行执行故单槽足够）。
#[derive(Default)]
pub struct KillSwitch {
    slot: Mutex<Option<(v8::IsolateHandle, Instant)>>,
    fired: AtomicBool,
}

impl KillSwitch {
    /// 创建并启动看门狗线程（进程生命周期，25ms 轮询粒度）。
    pub fn spawn() -> Arc<Self> {
        let sw = Arc::new(Self::default());
        let t = sw.clone();
        std::thread::Builder::new()
            .name("js-watchdog".into())
            .spawn(move || loop {
                std::thread::sleep(Duration::from_millis(25));
                let g = t.slot.lock().unwrap();
                if let Some((handle, deadline)) = g.as_ref() {
                    if Instant::now() >= *deadline && !t.fired.load(Ordering::Relaxed) {
                        // terminate_execution 是 V8 明确允许的跨线程调用（不要求进入 isolate）。
                        handle.terminate_execution();
                        t.fired.store(true, Ordering::Relaxed);
                    }
                }
            })
            .expect("spawn js-watchdog");
        sw
    }

    pub(crate) fn arm(&self, handle: v8::IsolateHandle, timeout: Duration) {
        self.fired.store(false, Ordering::Relaxed);
        *self.slot.lock().unwrap() = Some((handle, Instant::now() + timeout));
    }

    /// 关闭窗口；返回本窗口内是否触发过熔断。
    pub(crate) fn disarm(&self) -> bool {
        let fired = {
            *self.slot.lock().unwrap() = None;
            self.fired.swap(false, Ordering::Relaxed)
        };
        fired
    }
}
