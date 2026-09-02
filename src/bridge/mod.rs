//! bridge：注入到 deno_core JsRuntime 的 JS SDK 全局对象（JS 全局由 bootstrap.js 装配）。
//! 按绑定类型分文件组织：
//!   - json.rs     —— json.ok/fail/header 统一信封与返回头
//!   - db.rs       —— db.query/exec 与 DB(name) 命名实例（异步 op，支持绑定参数）
//!   - query.rs    —— 安全查询构造器（sea-query + SchemaRegistry 白名单，参数化值）
//!   - fetch.rs    —— fetch(url, options?) HTTP 客户端（reqwest，异步 Promise）
//!   - http.rs     —— http.* 请求上下文（只读，懒加载）
//!   - kv.rs       —— redis 内存 KV 抽象（get/set，无 Redis 时联调用）
//!   - log.rs      —— log.debug/info/warn/error 结构化日志（tracing）
//!   - envelope.rs —— {code,msg,data} 统一信封
//!   - registry.rs —— SchemaRegistry 表/列白名单（SQL 注入根治点）
//!   - runtime.rs  —— RuntimePool：复用 JsRuntime（预热 = 快照等价），V8 代码缓存
//!   - loader.rs   —— HandlerStore：handler 源码加载 + 热重载（FS / 嵌入）
//!   - transpile.rs—— TS→JS 转译（deno_ast strip types）+ mtime 全局缓存
//!   - inspector.rs—— InspectorServer：deno_core 自带 inspector 的 DevTools WS 桥
//!   - bootstrap.js —— JS 侧全局对象装配
//!
//! 状态拆分（revised）：
//! - `StableState`：跨请求不变（kv / dbs / client / registry / loader），`Arc` 共享，创建一次。
//! - `ReqState`：每请求可变（req / 响应捕获 / done），存入 OpState，checkout 时重置。
//!
//! 如此 JsRuntime 可池化复用而不串号请求。

mod accessor_sqlx;
pub mod blob;
pub mod broker;
mod bus;
pub mod bus_backend;
pub mod cert;
mod db;
pub mod db_backend;
mod envelope;
mod es;
mod fetch;
pub(crate) mod ffi;
pub mod guard;
mod http;
mod inspector;
mod json;
mod kv;
mod loader;
mod log;
mod module_loader;
mod named_registry;
pub mod plugin_loader;
mod plugins_op;
mod query;
mod registry;
mod runtime;
pub mod transpile;
mod ws;

pub use accessor_sqlx::SqlxAccessor;
pub use blob::{BlobBackend, BlobServed, LocalBlob, valid_key};
pub use bus::{Bus, EventBroker};
pub use bus_backend::{BusBackend, BusBackendRegistry};
pub use db::{DataAccessor, Dialect, InMemoryAccessor, Row};
pub use db_backend::{DbBackend, DbBackendRegistry};
pub use envelope::{fail, ok, status_code};
pub use es::EsBackend;
pub use http::{RequestInfo, UploadedFile};
pub use kv::{InMemoryKV, KVStore};
pub use loader::HandlerStore;
pub use module_loader::{LoaderShared, OjModuleLoader, versioned_specifier};
pub use named_registry::NamedRegistry;
pub use plugin_loader::PluginInfo;
pub use registry::SchemaRegistry;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use deno_core::error::CoreError;
use deno_core::{JsRuntime, OpState, op2};

/// 契约实现（DataAccessor/KVStore）的统一错误返回（stdlib，不泄漏 deno 类型）。
pub type BridgeResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// 模块执行上下文（P2 §5.3）：manifest 的 db/deps 声明 + 模块名。
/// 装配期从 manifest.yaml 汇总，按模块目录绝对路径存入 StableState.modules；
/// 派发时 `run_module` 经 api_path 祖先目录命中后写入 `ReqState.module`。
#[derive(Clone)]
pub struct ModuleCtx {
    pub name: String,
    /// manifest deps 声明的模块名集合（跨模块表访问守卫的放行依据）。
    pub deps: Arc<std::collections::HashSet<String>>,
    /// manifest db 绑定：Some(name) → 模块的 "default" 库调用重定向到该命名库。
    pub db: Option<String>,
}

/// StableState：跨请求共享、创建后不可变（内部句柄均为 Arc）。
pub struct StableState {
    pub kv: Arc<dyn KVStore>,
    pub dbs: HashMap<String, Arc<dyn DataAccessor>>,
    pub client: reqwest::Client,
    pub registry: Arc<SchemaRegistry>,
    /// oj 模块加载配置（node_modules 回溯上界 + CJS require 的 project_root）。
    /// T9 装配注入；devserver 旧路径不配（裸 specifier / __ojRequire 不可用）。
    pub loader: Option<Arc<module_loader::LoaderShared>>,
    /// blob 注册表（阶段 0 单后端形态：至多一个 "default"；阶段 1 命名多后端）。
    pub blobs: Arc<blob::BlobRegistry>,
    /// 事件总线（统一契约 `EventBroker`）：进程内 `Bus` 或分布式 Kafka/RabbitMQ；
    /// server 装配共享一个跨连接广播；缺省每 Bridge 自带进程内 Bus。
    pub bus: Arc<dyn bus::EventBroker>,
    /// ES 客户端（OJ-6）：es 配置存在时注入；否则 es.* 报 "es not configured"。
    pub es: Option<Arc<dyn es::EsBackend>>,
    /// 已装配插件自省信息（op_plugins 输出源；装配层注入）。
    pub plugins: Vec<PluginInfo>,
    /// 模块执行上下文（键 = 模块目录绝对路径；P2 §5.3）。空 = 无模块上下文（守卫不设防）。
    pub modules: Arc<HashMap<String, ModuleCtx>>,
    /// 表归属守卫模式（P2 §5.3）：false=warn（默认，日志告警）；true=deny（违规拒绝）。
    pub ownership_deny: bool,
    /// 裸 SQL 表名提取 memo（守卫热路径缓存；键 = SQL 原文）。
    pub sql_memo: Mutex<HashMap<String, Arc<Vec<String>>>>,
}

/// bridge 可选能力注入（构造期一次）。
#[derive(Default)]
pub struct Extras {
    /// None = 零后端（blob.* 报 notConfigured）。
    pub blobs: Option<Arc<blob::BlobRegistry>>,
    /// Some = 共享总线（server 跨连接广播）；None = 每 Bridge 自带新 Bus（进程内）。
    pub bus: Option<Arc<dyn bus::EventBroker>>,
    /// Some = ES 客户端；None = es.* 未配置报错。
    pub es: Option<Arc<dyn es::EsBackend>>,
    /// 已装配插件自省信息（op_plugins 数据源；装配层注入）。
    pub plugins: Vec<PluginInfo>,
    /// 模块执行上下文（§5.3；缺省空表 = 守卫不设防）。
    pub modules: Arc<HashMap<String, ModuleCtx>>,
    /// 表归属守卫 deny 模式（缺省 false = warn）。
    pub ownership_deny: bool,
}

/// ReqState：每请求可变状态（存在 OpState 中，checkout 时整体重置）。
/// 不 derive Clone：含活跃事务（ActiveTx 不可复制，clone 掉事务句柄就是漏回滚）。
#[derive(Default)]
pub struct ReqState {
    pub req: RequestInfo,
    pub response: Option<Vec<u8>>,
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub done: bool,
    /// 活跃事务（db.tx；每请求至多一个，reset 时丢弃 = drop 自带回滚）。
    pub tx: Option<Arc<db::ActiveTx>>,
    /// WS 帧循环专用：ws.send 收集（处理器结束后按序写出，先于信封响应）。
    pub ws_sends: Vec<String>,
    /// WS 帧循环专用：ws.close 置位 → 本帧结束后关闭连接。
    pub ws_close: bool,
    /// 本请求所属模块名（§5.3 执行上下文；run_module 按目录命中注入；None = 无上下文）。
    pub module: Option<String>,
}

impl ReqState {
    /// 重置为初始态，写入新请求的上下文。
    pub fn reset(&mut self, req: RequestInfo) {
        self.req = req;
        self.response = None;
        self.status = 200;
        self.headers.clear();
        self.done = false;
        self.tx = None;
        self.ws_sends.clear();
        self.ws_close = false;
        self.module = None;
    }
}

/// 历史兼容别名（部分旧调用可能引用 `Shared`）。
pub type Shared = Rc<RefCell<StableState>>;

deno_core::extension!(
    bridge_ext,
    ops = [
        op_finish,
        json::op_json_ok,
        json::op_json_fail,
        json::op_json_header,
        http::op_http_info,
        http::op_http_file,
        kv::op_kv_get,
        kv::op_kv_set,
        kv::op_kv_del,
        kv::op_kv_expire,
        kv::op_kv_incr,
        db::op_db_has,
        db::op_db_query,
        db::op_db_exec,
        db::op_db_tx_begin,
        db::op_db_tx_commit,
        db::op_db_tx_rollback,
        query::op_db_query_build,
        blob::op_blob_put,
        blob::op_blob_get,
        blob::op_blob_del,
        blob::op_blob_url,
        blob::op_blob_content_type,
        bus::op_bus_publish,
        bus::op_bus_subscribe,
        bus::op_bus_kind,
        es::op_es_search,
        es::op_es_index,
        es::op_es_del,
        plugins_op::op_plugins,
        fetch::op_fetch,
        log::op_log,
        module_loader::op_resolve_cjs,
        ws::op_ws_send,
        cert::op_cert_gen,
        cert::op_cert_renew,
        ws::op_ws_close,
    ],
    esm_entry_point = "ext:bridge_ext/bootstrap.js",
    esm = [dir "src/bridge", "bootstrap.js"],
    options = { stable: Arc<StableState> },
    state = |state, options| {
        state.put(options.stable.clone());
        state.put(ReqState::default());
    },
);

/// finish()：标记会话完成。
#[op2(fast)]
fn op_finish(state: &mut OpState) {
    state.borrow_mut::<ReqState>().done = true;
}

/// 响应捕获（json.ok/fail 写入，server 层读取后写回 HTTP 响应）。
#[derive(Default, Clone, Debug)]
pub struct Capture {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

/// Bridge：持有 runtime 池、handler 仓库，并执行 handler 脚本。
/// （StableState 由 pool 构造期捕获，经 OpState 注入；此处不另持 Arc。）
pub struct Bridge {
    pool: runtime::RuntimePool,
    handlers: HandlerStore,
    /// 是否启用 inspector（透传至 runtime 工厂）。
    inspect: bool,
    /// 超时熔断（每 Bridge 一个看门狗线程）。
    kill: Arc<runtime::KillSwitch>,
}

impl Bridge {
    /// 构造 Bridge（依赖倒置：传入接口而非具体实现）。
    /// 传入的 db 注册为 dbs["default"]，等价于 DB("default")，无需额外配置。
    pub fn new(db: Arc<dyn DataAccessor>, kv: Arc<dyn KVStore>) -> Self {
        Self::with_opts(db, kv, SchemaRegistry::new(), false)
    }

    /// 含 schema 注册表与 inspector 开关的构造（单 db，注册为 "default"）。
    pub fn with_opts(
        db: Arc<dyn DataAccessor>,
        kv: Arc<dyn KVStore>,
        registry: SchemaRegistry,
        inspect: bool,
    ) -> Self {
        Self::with_dbs(
            HashMap::from([("default".to_string(), db)]),
            kv,
            registry,
            inspect,
        )
    }

    /// 全量命名 DB 构造期注入（DBAccessors + default 回落）。
    /// 须在构造期给定全量：StableState 一经 runtime 池共享即不可变。
    pub fn with_dbs(
        dbs: HashMap<String, Arc<dyn DataAccessor>>,
        kv: Arc<dyn KVStore>,
        registry: SchemaRegistry,
        inspect: bool,
    ) -> Self {
        Self::with_dbs_and_loader(dbs, kv, registry, inspect, None, Extras::default())
    }

    /// 全量命名 DB + 模块加载器构造（oj server 专用路径，T10/T11 装配消费）。
    /// extras：可选能力（blob/es）单一扩展点。
    pub fn with_dbs_and_loader(
        mut dbs: HashMap<String, Arc<dyn DataAccessor>>,
        kv: Arc<dyn KVStore>,
        registry: SchemaRegistry,
        inspect: bool,
        loader: Option<Arc<module_loader::LoaderShared>>,
        extras: Extras,
    ) -> Self {
        // 防御：无 "default" 键时取任一实例补位（JS 侧 db = DB("default")）。
        if !dbs.contains_key("default")
            && let Some(first) = dbs.values().next().cloned()
        {
            dbs.insert("default".to_string(), first);
        }
        let stable = Arc::new(StableState {
            kv,
            dbs,
            client: reqwest::Client::builder()
                .no_proxy()
                .build()
                .expect("build reqwest client"),
            registry: Arc::new(registry),
            loader,
            blobs: extras
                .blobs
                .unwrap_or_else(|| Arc::new(blob::BlobRegistry::new())),
            bus: extras.bus.unwrap_or_else(|| Arc::new(bus::Bus::new())),
            es: extras.es,
            plugins: extras.plugins,
            modules: extras.modules,
            ownership_deny: extras.ownership_deny,
            sql_memo: Mutex::new(HashMap::new()),
        });
        let pool = runtime::RuntimePool::new(stable.clone(), inspect);
        Self {
            pool,
            handlers: HandlerStore::from_env(),
            inspect,
            kill: runtime::KillSwitch::spawn(),
        }
    }

    /// 替换 handler 仓库（生产用嵌入 map，开发用 FS 热重载）。
    pub fn set_handlers(&mut self, handlers: HandlerStore) {
        self.handlers = handlers;
    }

    /// 取 inspector 句柄（若以 inspect=true 构造且已有 checkout 的 runtime 持有时可用）。
    /// 此处提供便捷构造 inspector server 的能力：需先 checkout 一个 runtime 并取其 inspector。
    pub fn inspect(&self) -> bool {
        self.inspect
    }

    /// 执行 handler 源码并驱动 event loop 至所有 Promise 落定。
    /// 从池借出 runtime，重置 per-request 状态，执行，归还。
    pub async fn run(&self, source: &str) -> Result<Capture, CoreError> {
        self.run_with(source, RequestInfo::default()).await
    }

    /// 带请求上下文执行。
    pub async fn run_with(&self, source: &str, req: RequestInfo) -> Result<Capture, CoreError> {
        let mut rt = self.checkout_reset(req, None);
        let result = runtime::run_to_completion(&mut rt, "handler.js", source.to_string()).await;
        // 仅成功执行（已轮询 event loop）的 runtime 才归还；失败的可能 isolate 损坏，直接丢弃。
        match result {
            Ok(()) => {
                let cap = Self::read_capture(&rt);
                Self::finalize_tx(&rt).await;
                self.pool.checkin(rt);
                Ok(cap)
            }
            Err(e) => Err(e),
        }
    }

    // ----- 执行管道复用辅助 -----

    /// 借出 runtime 并重置 per-request 状态（不武装看门狗）。
    fn checkout_reset(&self, req: RequestInfo, module: Option<&str>) -> JsRuntime {
        let rt = self.pool.checkout();
        {
            let op_state = runtime::op_state(&rt);
            let mut g = op_state.borrow_mut();
            let rs = g.borrow_mut::<ReqState>();
            rs.reset(req);
            rs.module = module.map(|s| s.to_string());
        }
        rt
    }

    /// 借出 runtime、重置状态并武装看门狗（KillSwitch）。
    /// 用 v8::IsolateHandle（Send+Sync 跨线程句柄）而非裸指针：OwnedIsolate 包装地址 ≠ 真实
    /// isolate 指针，手转裸指针再跨线程 terminate 会解引用垃圾地址（SIGSEGV）。
    fn checkout_armed(
        &self,
        req: RequestInfo,
        timeout: std::time::Duration,
        module: Option<&str>,
    ) -> JsRuntime {
        let mut rt = self.checkout_reset(req, module);
        let handle = rt.v8_isolate().thread_safe_handle();
        self.kill.arm(handle, timeout);
        rt
    }

    /// 请求收尾：未完结的活跃事务保底回滚（checkin 前调用；错误吞掉只告警，
    /// sqlx Transaction drop 亦会释放连接）。JS 侧正常路径已显式 commit/rollback。
    async fn finalize_tx(rt: &JsRuntime) {
        let t = {
            let op_state = runtime::op_state(rt);
            let mut g = op_state.borrow_mut();
            g.borrow_mut::<ReqState>().tx.take()
        };
        if let Some(t) = t {
            match t.session.lock().await.rollback().await {
                Ok(()) => eprintln!(
                    "warn: open transaction on db '{}' rolled back at request end",
                    t.db
                ),
                Err(e) => eprintln!(
                    "warn: open tx on db '{}' rollback failed at request end: {e}",
                    t.db
                ),
            }
        }
    }

    /// 读取当前 runtime 的 per-request 响应捕获。
    fn read_capture(rt: &JsRuntime) -> Capture {
        let op_state = runtime::op_state(rt);
        let g = op_state.borrow();
        let rs = g.borrow::<ReqState>();
        Capture {
            status: rs.status,
            headers: rs.headers.clone(),
            body: rs.response.clone().unwrap_or_default(),
        }
    }

    /// 按名执行已加载的 handler（热重载走 HandlerStore）。
    pub async fn run_named(&self, name: &str) -> Result<Capture, CoreError> {
        let src = self.handlers.get(name).ok_or_else(|| {
            CoreError::from(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("handler '{name}' not found"),
            ))
        })?;
        self.run(&src).await
    }

    /// 带超时熔断的执行：到期经 V8 terminate_execution 终止（跨线程安全），
    /// 该 runtime 不归还池，返回 `RunError::Timeout`（408 语义）。
    pub async fn run_with_timeout(
        &self,
        source: &str,
        req: RequestInfo,
        timeout: std::time::Duration,
    ) -> Result<Capture, RunError> {
        self.run_ws(source, req, timeout).await.map(|o| o.capture)
    }

    /// WS 帧执行：同 run_with_timeout，额外带出 ws.send 收集与 ws.close 置位。
    pub async fn run_ws(
        &self,
        source: &str,
        req: RequestInfo,
        timeout: std::time::Duration,
    ) -> Result<WsOutcome, RunError> {
        let mut rt = self.checkout_armed(req, timeout, None);
        let result = runtime::run_to_completion(&mut rt, "handler.js", source.to_string()).await;
        if self.kill.disarm() {
            // runtime 已被 terminate，不可复用，直接丢弃（不 checkin）。
            return Err(RunError::Timeout);
        }
        result.map_err(RunError::Core)?;
        let (sends, close) = {
            let op_state = runtime::op_state(&rt);
            let g = op_state.borrow();
            let rs = g.borrow::<ReqState>();
            (rs.ws_sends.clone(), rs.ws_close)
        };
        let capture = Self::read_capture(&rt);
        Self::finalize_tx(&rt).await;
        self.pool.checkin(rt);
        Ok(WsOutcome {
            capture,
            sends,
            close,
        })
    }

    /// ESM 模式执行：TLA driver 模块 import api 模块并调 default[method]。
    /// KillSwitch/ReqState 复用 run_ws 的熔断与捕获路径；被熔断的 runtime 同样不归还池。
    pub async fn run_module(
        &self,
        api_path: &std::path::Path,
        method: &str,
        req: RequestInfo,
        timeout: std::time::Duration,
    ) -> Result<Capture, RunError> {
        let spec = module_loader::versioned_specifier(api_path)
            .map_err(|e| RunError::Core(CoreError::from(std::io::Error::other(e))))?;
        // TLA driver：import 命中 V8 模块缓存（?v= 不变时零转译零重编译）；
        // 方法未导出 → json.fail(405)（信封映射 HTTP 405）。
        // method/msg 以 JSON 编码嵌入（合法 JS 字面量，杜绝引号注入）。
        let method_lit = serde_json::to_string(method).unwrap_or_else(|_| "\"\"".into());
        let msg_lit = serde_json::to_string(&format!(
            "method '{method}' not exported by {}",
            api_path.display()
        ))
        .unwrap_or_else(|_| "\"method not exported\"".into());
        let code = format!(
            "const m = await import(\"{spec}\");\n\
             const fn = m.default && m.default[{method_lit}];\n\
             if (typeof fn !== \"function\") json.fail(405, {msg_lit});\n\
             else await fn();\n"
        );
        // §5.3 执行上下文：api_path 祖先目录命中模块目录 → 本请求归该模块
        // （归属守卫 / bound_db 重定向依据；Map 键 = 模块目录绝对路径，装配期注入）。
        let stable = self.pool.stable();
        let mut module: Option<&str> = None;
        let mut anc = api_path.parent();
        while let Some(d) = anc {
            if let Some(ctx) = stable.modules.get(&d.to_string_lossy().into_owned()) {
                module = Some(ctx.name.as_str());
                break;
            }
            anc = d.parent();
        }
        self.run_side_driver(req, code, timeout, module).await
    }

    /// 启动期内省：import api 模块、读 default[method].route，经 json.ok 信封回传 data
    /// （{"get": "{id}" | null, ...}，仅含函数导出的方法；null = 导出但未挂 .route）。
    /// 复用 run_module 的 driver/KillSwitch/checkin 管道（其注释同样适用）。
    pub async fn introspect_module(
        &self,
        api_path: &std::path::Path,
    ) -> Result<serde_json::Value, RunError> {
        let spec = module_loader::versioned_specifier(api_path)
            .map_err(|e| RunError::Core(CoreError::from(std::io::Error::other(e))))?;
        let code = format!(
            "const m = await import(\"{spec}\");\n\
             const out = {{}};\n\
             for (const k of [\"get\",\"post\",\"put\",\"del\",\"patch\",\"head\",\"options\"]) {{\n\
               const fn = m.default && m.default[k];\n\
               if (typeof fn === \"function\") out[k] = fn.route === undefined ? null : String(fn.route);\n\
             }}\n\
             json.ok(out);\n"
        );
        let cap = self
            .run_side_driver(RequestInfo::default(), code, INTROSPECT_TIMEOUT, None)
            .await?;
        let v: serde_json::Value = serde_json::from_slice(&cap.body).unwrap_or_default();
        Ok(v["data"].clone())
    }

    /// 读模块 default 导出（release 直载 routes.js：一次 import，不逐模块内省）。
    pub async fn read_module_default(
        &self,
        path: &std::path::Path,
    ) -> Result<serde_json::Value, RunError> {
        let spec = module_loader::versioned_specifier(path)
            .map_err(|e| RunError::Core(CoreError::from(std::io::Error::other(e))))?;
        let code = format!(
            "const m = await import(\"{spec}\");\n\
             json.ok(m.default === undefined ? null : m.default);\n"
        );
        let cap = self
            .run_side_driver(RequestInfo::default(), code, INTROSPECT_TIMEOUT, None)
            .await?;
        let v: serde_json::Value = serde_json::from_slice(&cap.body).unwrap_or_default();
        Ok(v["data"].clone())
    }

    /// 一次性 side-module driver 执行：借出 runtime、武装看门狗、执行 `code`
    /// （code 已 import 目标模块并触发对应 handler/内省），返回响应捕获。
    /// run_module / introspect_module / read_module_default 共用（KillSwitch + 事件循环 + 信封解析）。
    /// 超时 → RunError::Timeout（runtime 不归还）；handler 错误 → RunError::Core。
    async fn run_side_driver(
        &self,
        req: RequestInfo,
        code: String,
        timeout: std::time::Duration,
        module: Option<&str>,
    ) -> Result<Capture, RunError> {
        static DRV_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = DRV_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let driver_spec = deno_core::ModuleSpecifier::parse(&format!("file:///oj/driver/{n}.js"))
            .map_err(|e| {
            RunError::Core(CoreError::from(std::io::Error::other(e.to_string())))
        })?;
        let mut rt = self.checkout_armed(req, timeout, module);
        // 顺序以 0.410 签名为准：mod_evaluate 返回 `impl Future + use<>`（不借 runtime），
        // 先启动求值再驱动 event loop，最后 await 求值 future 取 TLA 错误。
        // driver 以 side module 加载：每 JsRuntime 仅一个 main module，
        // 池化 runtime 的第二个请求（driver/N 递增）会撞 MainModuleAlreadyExists。
        let result: Result<(), CoreError> = async {
            let id = rt.load_side_es_module_from_code(&driver_spec, code).await?;
            let eval = rt.mod_evaluate(id);
            rt.run_event_loop(deno_core::PollEventLoopOptions::default())
                .await?;
            eval.await?;
            Ok(())
        }
        .await;
        // 超时熔断：看门狗已 terminate isolate，runtime 不可复用，直接丢弃（不 checkin）。
        if self.kill.disarm() {
            return Err(RunError::Timeout);
        }
        result.map_err(RunError::Core)?;
        let capture = Self::read_capture(&rt);
        Self::finalize_tx(&rt).await;
        self.pool.checkin(rt);
        Ok(capture)
    }
}

/// 内省超时：坏模块顶层死循环不挂死启动。
/// ponytail: 常量而非配置；真有 >2s 的合法顶层模块再加 server.introspect_timeout。
pub const INTROSPECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// WS 帧执行结果：HTTP 捕获 + ws.send 集合 + ws.close 置位。
#[derive(Debug, Default)]
pub struct WsOutcome {
    pub capture: Capture,
    pub sends: Vec<String>,
    pub close: bool,
}

/// handler 执行错误：区分超时熔断（408）与普通失败（500）。
#[derive(Debug)]
pub enum RunError {
    Timeout,
    Core(CoreError),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Timeout => write!(f, "handler execution timed out"),
            RunError::Core(e) => write!(f, "{e}"),
        }
    }
}

/// 启动 DevTools inspector：借一个 runtime 取其 inspector 句柄并起 WS 服务。
/// 仅当 `inspect=true` 构造时有效；addr 形如 `127.0.0.1:9229`。
pub fn start_inspector(bridge: &Bridge, addr: std::net::SocketAddr) -> tokio::task::JoinHandle<()> {
    if !bridge.inspect {
        tracing::warn!(target: "inspector", "inspector not enabled at construction");
        return tokio::task::spawn_local(async {});
    }
    let rt = bridge.pool.checkout();
    let insp = rt.inspector();
    runtime::RuntimePool::checkin(&bridge.pool, rt);
    inspector::spawn(insp, addr)
}

// run_module 系测试持 TRANSPILE_TEST_LOCK 跨 await（current_thread 单线程，安全）。
#[cfg(test)]
#[allow(clippy::await_holding_lock)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn new_bridge() -> (Bridge, Arc<InMemoryAccessor>) {
        let db = Arc::new(InMemoryAccessor::new());
        db.seed([json!({"id": 1, "name": "ever"})]);
        let b = Bridge::new(db.clone(), Arc::new(InMemoryKV::new()));
        (b, db)
    }

    /// tenant.enable 时 handle() 提取注入 → http.tenantId；未启用为 null。
    #[tokio::test(flavor = "current_thread")]
    async fn http_exposes_tenant_id() {
        let (b, _) = new_bridge();
        let cap = b
            .run_with(
                r#"json.ok({ t: http.tenantId === undefined ? null : http.tenantId });"#,
                RequestInfo {
                    tenant_id: Some("t-9".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["data"]["t"], "t-9");
    }

    /// blob 未配置（Extras 缺省）：JS 侧 blob.* 报 "blob not configured"。
    #[tokio::test(flavor = "current_thread")]
    async fn blob_not_configured_errors() {
        let (b, _) = new_bridge();
        let e = b
            .run_with(
                r#"blob.put("a/b.png", new Uint8Array([1]));"#,
                RequestInfo::default(),
            )
            .await
            .unwrap_err();
        assert!(e.to_string().contains("blob not configured"), "{e}");
    }

    /// blob 经 Extras 注入：JS put/get/url 走 LocalBlob。
    #[tokio::test(flavor = "current_thread")]
    async fn blob_roundtrip_via_extras() {
        let dir = std::env::temp_dir().join(format!("oj-blobjs-{}", std::process::id()));
        let local = blob::LocalBlob::new(&dir, "/v1/api").unwrap();
        let b = Bridge::with_dbs_and_loader(
            HashMap::new(),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            None,
            Extras {
                blobs: Some(crate::bridge::blob::registry_with_default(Arc::new(local))),
                ..Default::default()
            },
        );
        let cap = b
            .run_with(
                r#"
                (async () => {
                    await blob.put("js/a.png", new Uint8Array([1, 2, 3]), "image/png");
                    const got = await blob.get("js/a.png");
                    await blob.del("js/none");
                    json.ok({ n: got.length, url: await blob.url("js/a.png") });
                })();
                "#,
                RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["data"]["n"], 3);
        assert_eq!(v["data"]["url"], "/v1/api/blob/js/a.png");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn globals_injected_and_json_ok() {
        let (b, _) = new_bridge();
        let cap = b
            .run_with(
                r#"
                const injected = typeof json === "object" && typeof db === "object"
                    && typeof http === "object" && typeof redis === "object"
                    && typeof DB === "function" && typeof fetch === "function"
                    && typeof log === "object" && typeof finish === "function";
                log.info("handler start", "method", http.method);
                json.header("X-Handler", "test");
                json.ok({ injected, method: http.method });
                "#,
                RequestInfo {
                    method: "POST".into(),
                    query: [("id".into(), "1".into())].into_iter().collect(),
                    body: br#"{"a":1}"#.to_vec(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(cap.status, 200);
        assert_eq!(cap.headers.get("X-Handler").unwrap(), "test");
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(
            v,
            json!({"code": 0, "msg": "ok", "data": {"injected": true, "method": "POST"}})
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn db_params_and_build() {
        let db = Arc::new(InMemoryAccessor::new());
        db.seed([json!({"id": 1, "name": "ever"})]);
        let registry = SchemaRegistry::new().table("user", Some("id"), &["id", "name", "age"]);
        let b = Bridge::with_opts(db, Arc::new(InMemoryKV::new()), registry, false);
        // 结构化查询构造器（经 SchemaRegistry 白名单）。
        let cap = b
            .run(
                r#"
                db.table("user").select(["id","name"]).where({field:"id",op:"eq",value:1})
                  .orderBy([{field:"id",dir:"asc"}]).limit(10).all()
                  .then((rows) => json.ok({ rows, missing: typeof DB("nope") === "undefined" }))
                  .catch((e) => json.fail(500, String(e)));
                "#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "handler failed: {v}");
        assert_eq!(v["data"]["rows"], json!([{"id": 1, "name": "ever"}]));
        assert_eq!(v["data"]["missing"], true);

        // 原始 SQL + 绑定参数（无拼接）。
        let cap = b
            .run(r#"db.query("select * from user where id = $1", [1]).then((rows) => json.ok({ rows })).catch((e) => json.fail(500, String(e)));"#)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "param query failed: {v}");
        assert_eq!(v["data"]["rows"], json!([{"id": 1, "name": "ever"}]));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn named_dbs_and_default_fallback() {
        // 构造期注入全量命名 DB（替代 set_db_accessors——Arc 已被池共享，事后改不可行）。
        let a = Arc::new(InMemoryAccessor::new());
        a.seed([json!({"id": 1, "name": "ever"})]);
        let b = Bridge::with_dbs(
            HashMap::from([("default".to_string(), a as Arc<dyn DataAccessor>)]),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new().table("user", Some("id"), &["id", "name"]),
            false,
        );
        let cap = b
            .run(r#"db.table("user").select(["id"]).all().then((rows) => json.ok({ n: rows.length })).catch((e) => json.fail(500, String(e)));"#)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["data"]["n"], 1, "{v}");

        // 无 "default" 键 → 回落第一个实例（防御）。
        let only = Arc::new(InMemoryAccessor::new());
        only.seed([json!({"id": 2, "name": "neo"})]);
        let b2 = Bridge::with_dbs(
            HashMap::from([("only".to_string(), only as Arc<dyn DataAccessor>)]),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new().table("user", Some("id"), &["id", "name"]),
            false,
        );
        let cap = b2
            .run(r#"db.query("select * from user").then((rows) => json.ok({ n: rows.length })).catch((e) => json.fail(500, String(e)));"#)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["data"]["n"], 1, "{v}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn http_param_and_kv_global() {
        let (b, _) = new_bridge();
        let cap = b
            .run_with(
                r#"
                kv.set("k", "v");
                kv.get("k").then((v) => {
                    const hit = v;
                    kv.del("k");
                    kv.get("k").then((v2) => json.ok({
                        hit, gone: v2,
                        p1: http.param("id", 0),
                        p2: http.param("missing", "dft"),
                    }));
                }).catch((e) => json.fail(500, String(e)));
                "#,
                RequestInfo {
                    query: [("id".into(), "7".into())].into_iter().collect(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(
            v["data"],
            json!({"hit": "v", "gone": null, "p1": "7", "p2": "dft"}),
            "{v}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn kv_expire_incr_global() {
        // bootstrap 接线：kv.expire(key, 秒) / kv.incr(key) 经 op 可达；redis 同源。
        let (b, _) = new_bridge();
        let cap = b
            .run_with(
                r#"
                (async () => {
                    await kv.incr("c");
                    await kv.incr("c");
                    const n = await kv.incr("c");        // 3
                    const ok = await kv.expire("c", 1);   // 秒 → ms
                    await redis.set("r", "9");
                    const r = await redis.incr("r");      // 10
                    json.ok({ n, ok, r });
                })().catch((e) => json.fail(500, String(e)));
                "#,
                RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["data"], json!({"n": 3, "ok": true, "r": 10}), "{v}");
    }

    /// bus：WS 上下文订阅（bus_tx 注入）→ 后续发布广播 JSON 帧到订阅方；
    /// HTTP 上下文订阅 → 抛错（无 WS 连接）。
    #[tokio::test(flavor = "current_thread")]
    async fn bus_subscribe_publish_and_http_rejects() {
        let (b, _) = new_bridge();
        let (bus_tx, mut bus_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        // 帧 1：WS 会话订阅（RequestInfo.bus_tx 注入）
        let o1 = b
            .run_ws(
                r#"(async () => { await bus.subscribe("news"); json.ok({ ok: 1 }); })().catch((e) => json.fail(500, String(e)));"#,
                RequestInfo { bus_tx: Some(bus_tx.clone()), ..Default::default() },
                std::time::Duration::from_secs(1),
            )
            .await
            .unwrap();
        assert_eq!(o1.capture.status, 200);
        // 帧 2：发布（无 bus_tx 也广播）→ 返回接收方数
        let o2 = b
            .run_ws(
                r#"(async () => { const n = await bus.publish("news", { a: 1 }); json.ok({ n }); })().catch((e) => json.fail(500, String(e)));"#,
                RequestInfo::default(),
                std::time::Duration::from_secs(1),
            )
            .await
            .unwrap();
        let v2: Value = serde_json::from_slice(&o2.capture.body).unwrap();
        assert_eq!(v2["data"]["n"], 1, "{v2}");
        // 订阅方收到 {"topic","data"} JSON 帧
        let frame = bus_rx.try_recv().unwrap();
        let v: Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(v, json!({"topic": "news", "data": {"a": 1}}), "{v}");
        // HTTP 上下文（bus_tx None）订阅 → 报错（msg 含 WebSocket）
        let cap = b
            .run_with(
                r#"(async () => { await bus.subscribe("news"); json.ok({}); })().catch((e) => json.ok({ err: String(e) }));"#,
                RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(
            v["data"]["err"].as_str().unwrap().contains("WebSocket"),
            "{v}"
        );
    }

    /// bus.kind()：进程内总线返回 "local"（分布式实现经 feature 启用）。
    #[tokio::test(flavor = "current_thread")]
    async fn bus_kind_op_returns_local() {
        let (b, _) = new_bridge();
        let cap = b
            .run_with(
                r#"(async () => { const k = await bus.kind(); json.ok({ k }); })().catch((e) => json.fail(500, String(e)));"#,
                RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(v["data"]["k"], "local");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn http_files_and_file_roundtrip() {
        // http.files 元信息 + http.file(i) 字节；越界 → "no such file"。
        let (b, _) = new_bridge();
        let cap = b
            .run_with(
                r#"
                (async () => {
                    const f = http.files[0];
                    const bts = await http.file(0);
                    json.ok({ name: f.filename, ct: f.content_type, size: f.size, n: bts.length, b0: bts[0] });
                })().catch((e) => json.fail(500, String(e)));
                "#,
                RequestInfo {
                    files: vec![UploadedFile {
                        field: "file".into(),
                        filename: "a.png".into(),
                        content_type: Some("image/png".into()),
                        bytes: vec![1, 2, 3],
                    }],
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(
            v["data"],
            json!({"name": "a.png", "ct": "image/png", "size": 3, "n": 3, "b0": 1}),
            "{v}"
        );
        // 无文件时取越界索引 → 报错
        let cap = b
            .run_with(
                r#"(async () => { await http.file(5); json.fail(500, "should throw"); })().catch((e) => json.ok({ err: String(e) }));"#,
                RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(
            v["data"]["err"].as_str().unwrap().contains("no such file"),
            "{v}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tx_commit_visible_and_throw_rolls_back() {
        let db = crate::bridge::SqlxAccessor::arc("sqlite::memory:")
            .await
            .unwrap();
        db.exec_with_params("create table t (id integer primary key, v text)", &[])
            .await
            .unwrap();
        let b = Bridge::new(db, Arc::new(InMemoryKV::new()));
        // 提交可见
        let cap = b
            .run(
                r#"db.tx(async (tx) => { await tx.exec("insert into t (v) values (?)", ["x"]); })
                .then(() => db.query("select count(*) c from t"))
                .then((r) => json.ok(r[0]))
                .catch((e) => json.fail(500, String(e)));"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["data"]["c"], 1, "{v}");
        // fn 抛错 → 自动回滚（y 行不可见）
        let cap = b
            .run(r#"db.tx(async (tx) => { await tx.exec("insert into t (v) values (?)", ["y"]); throw new Error("boom"); })
                .then(() => json.fail(500, "should have thrown"))
                .catch((e) => db.query("select count(*) c from t").then((r) => json.ok({ err: String(e), c: r[0].c })));"#)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["data"]["c"], 1, "y must be rolled back: {v}");
        assert!(v["data"]["err"].as_str().unwrap().contains("boom"), "{v}");
        // 嵌套 begin 报错
        let cap = b
            .run(
                r#"db.tx(async () => { await db.tx(async () => {}); })
                .then(() => json.fail(500, "nested should fail"))
                .catch((e) => json.ok({ msg: String(e) }));"#,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(
            v["data"]["msg"]
                .as_str()
                .unwrap()
                .contains("already active"),
            "{v}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn builder_in_tx_shares_connection_and_rolls_back() {
        // 构造器（SELECT-only）在 tx 内必须走同一事务连接：能读到未提交行；
        // 回滚后行不可见。（sqlite 单连接池下，若 builder 误走 pool 会死锁到超时。）
        let db = crate::bridge::SqlxAccessor::arc("sqlite::memory:")
            .await
            .unwrap();
        db.exec_with_params("create table t (id integer primary key, v text)", &[])
            .await
            .unwrap();
        let reg = SchemaRegistry::new().table("t", Some("id"), &["id", "v"]);
        let b = Bridge::with_opts(db, Arc::new(InMemoryKV::new()), reg, false);
        let cap = b
            .run(r#"db.tx(async (tx) => {
                    await tx.exec("insert into t (v) values (?)", ["z"]);
                    const rows = await tx.table("t").select(["v"]).all();  // 同一事务连接 → 见未提交 z
                    throw new Error("force rollback: " + rows.length);
                }).then(() => json.fail(500, "no"))
                .catch((e) => db.query("select count(*) c from t").then((r) => json.ok({ msg: String(e), c: r[0].c })));"#)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(
            v["data"]["msg"]
                .as_str()
                .unwrap()
                .contains("force rollback: 1"),
            "{v}"
        );
        assert_eq!(v["data"]["c"], 0, "z must be rolled back: {v}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ws_global_send_close_and_reset() {
        // WS 帧循环契约：ws.send 收集到 sends（先于信封写出）、ws.close 置位，帧间重置。
        let (b, _) = new_bridge();
        let o = b
            .run_ws(
                r#"
                const ok1 = typeof ws === "object" && typeof ws.send === "function" && typeof ws.close === "function";
                ws.send("side-a"); ws.send("side-b"); ws.close();
                json.ok({ ok1 });
                "#,
                RequestInfo::default(),
                std::time::Duration::from_secs(1),
            )
            .await
            .unwrap();
        assert_eq!(o.sends, vec!["side-a".to_string(), "side-b".to_string()]);
        assert!(o.close);
        let v: Value = serde_json::from_slice(&o.capture.body).unwrap();
        assert_eq!(v["data"]["ok1"], true, "{v}");

        // 第二帧：sends/close 不串号（ReqState 重置）。
        let o2 = b
            .run_ws(
                r#"json.ok({});"#,
                RequestInfo::default(),
                std::time::Duration::from_secs(1),
            )
            .await
            .unwrap();
        assert!(o2.sends.is_empty());
        assert!(!o2.close);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unknown_table_rejected() {
        let (b, _) = new_bridge();
        let cap = b
            .run(r#"db.table("secret").select(["x"]).all().then((r) => json.ok(r)).catch((e) => json.fail(400, String(e)));"#)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 400);
        assert!(v["msg"].as_str().unwrap().contains("unknown table"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn infinite_loop_times_out_and_bridge_survives() {
        let (b, _) = new_bridge();
        let r = b
            .run_with_timeout(
                "while (true) {}",
                RequestInfo::default(),
                std::time::Duration::from_millis(150),
            )
            .await;
        assert!(matches!(r, Err(RunError::Timeout)), "got: {r:?}");
        // 超时 runtime 被丢弃，bridge 后续请求正常（池可新开 runtime）。
        let cap = b.run(r#"json.ok({ alive: true });"#).await.unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["data"]["alive"], true);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn oj_require_cjs_interop_end_to_end() {
        // T8 契约：__ojRequire(name, referrer) → op 解析 + eval + 嵌套 require。
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "oj-req-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        for (rel, content) in [
            (
                "node_modules/cjspkg/package.json",
                r#"{"name":"cjspkg","main":"main.js"}"#,
            ),
            // 嵌套 require：dep 经 __ojRequire 的 resolved.path 再解析。
            (
                "node_modules/cjspkg/main.js",
                "module.exports = { n: 1, dep: require(\"cjspkg-dep\").d };\n",
            ),
            (
                "node_modules/cjspkg-dep/index.js",
                "module.exports = { d: 2 };\n",
            ),
        ] {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        }
        let stable = Arc::new(StableState {
            kv: Arc::new(InMemoryKV::new()),
            dbs: HashMap::new(),
            client: Default::default(),
            registry: Arc::new(SchemaRegistry::new()),
            loader: Some(Arc::new(LoaderShared {
                project_root: root.clone(),
                ts: false,
            })),
            blobs: Arc::new(blob::BlobRegistry::new()),
            bus: Arc::new(bus::Bus::new()),
            es: None,
            plugins: Vec::new(),
            modules: Arc::new(HashMap::new()),
            ownership_deny: false,
            sql_memo: Mutex::new(HashMap::new()),
        });
        let pool = runtime::RuntimePool::new(stable, false);
        let mut rt = pool.checkout();
        let referrer = root.join("src/handler.js");
        let src = format!(
            r#"const c = __ojRequire("cjspkg", {referrer:?}); json.ok({{ n: c.n, dep: c.dep }});"#
        );
        runtime::run_to_completion(&mut rt, "handler.js", src)
            .await
            .unwrap();
        let v: Value = {
            let op_state = runtime::op_state(&rt);
            let g = op_state.borrow();
            serde_json::from_slice(&g.borrow::<ReqState>().response.clone().unwrap()).unwrap()
        };
        pool.checkin(rt);
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(v["data"], json!({ "n": 1, "dep": 2 }), "{v}");
    }

    fn mod_fx(files: &[(&str, &str)]) -> (std::path::PathBuf, std::path::PathBuf) {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let base = std::env::temp_dir().join(format!(
            "oj-mod-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&base).unwrap();
        for (rel, content) in files {
            let p = base.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        }
        (base.clone(), base.join(files[0].0))
    }

    fn module_bridge(root: &std::path::Path) -> Bridge {
        module_bridge_ex(
            root,
            SchemaRegistry::new(),
            Arc::new(HashMap::new()),
            false,
            HashMap::new(),
        )
    }

    /// 带归属上下文的模块 bridge：modules 键 = 模块目录绝对路径（§5.3 装配形态）。
    /// dbs：命名库表（None = 空，走 Bridge 默认补 default）。
    fn module_bridge_ex(
        root: &std::path::Path,
        registry: SchemaRegistry,
        modules: Arc<HashMap<String, ModuleCtx>>,
        ownership_deny: bool,
        dbs: HashMap<String, Arc<dyn DataAccessor>>,
    ) -> Bridge {
        Bridge::with_dbs_and_loader(
            dbs,
            Arc::new(InMemoryKV::new()),
            registry,
            false,
            Some(Arc::new(LoaderShared {
                project_root: root.to_path_buf(),
                ts: true,
            })),
            Extras {
                modules,
                ownership_deny,
                ..Default::default()
            },
        )
    }

    /// run_module 系列触发 .ts 转译：与 transpile 测试的计数器 delta 断言互斥
    /// （current_thread 单线程 runtime，锁跨 await 安全；poison 不级联）。
    fn transpile_serial() -> std::sync::MutexGuard<'static, ()> {
        transpile::TRANSPILE_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn runs_exported_method_from_ts_module() {
        let _t = transpile_serial();
        let (root, api) = mod_fx(&[
            (
                "user/account/api.ts",
                "import { tag } from \"../_shared/util\";\n\
                 function get(): void { json.ok({ ok: 1, tag: tag(\"x\") }); }\n\
                 export default { get };\n",
            ),
            (
                "user/_shared/util.ts",
                "export function tag(s: string): string { return \"t-\" + s; }\n",
            ),
        ]);
        let b = module_bridge(&root);
        let cap = b
            .run_module(
                &api,
                "get",
                RequestInfo::default(),
                std::time::Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert_eq!(cap.status, 200);
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["data"], json!({"ok": 1, "tag": "t-x"}), "{v}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn method_not_exported_is_405() {
        let _t = transpile_serial();
        let (root, api) = mod_fx(&[("u/f/api.ts", "export default { get() { json.ok({}); } };\n")]);
        let b = module_bridge(&root);
        let cap = b
            .run_module(
                &api,
                "del",
                RequestInfo::default(),
                std::time::Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert_eq!(cap.status, 405, "{}", String::from_utf8_lossy(&cap.body));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn infinite_module_handler_times_out_and_bridge_survives() {
        let _t = transpile_serial();
        // R1 spike：ESM/TLA 模型下 KillSwitch 复验。
        let (root, api) = mod_fx(&[(
            "u/f/api.ts",
            "export default { get() { while (true) {} } };\n",
        )]);
        let b = module_bridge(&root);
        let r = b
            .run_module(
                &api,
                "get",
                RequestInfo::default(),
                std::time::Duration::from_millis(200),
            )
            .await;
        assert!(matches!(r, Err(RunError::Timeout)), "got: {r:?}");
        let cap = b.run(r#"json.ok({ alive: true });"#).await.unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["data"]["alive"], true);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_module_reuses_pooled_runtime_across_requests() {
        // 回归：每 JsRuntime 仅一个 main module；driver 须以 side module 加载，
        // 否则同一池化 runtime 的第二个请求报 main module already exists。
        // 模块顶层每 runtime 只执行一次（ESM 缓存）→ n 跨请求累计。
        let _t = transpile_serial();
        let (root, api) = mod_fx(&[(
            "u/f/api.ts",
            "let n = 0;\nexport default { get() { n += 1; json.ok({ n }); } };\n",
        )]);
        let b = module_bridge(&root);
        let cap1 = b
            .run_module(
                &api,
                "get",
                RequestInfo::default(),
                std::time::Duration::from_secs(5),
            )
            .await
            .unwrap();
        let cap2 = b
            .run_module(
                &api,
                "get",
                RequestInfo::default(),
                std::time::Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert_eq!(cap1.status, 200);
        assert_eq!(cap2.status, 200);
        let v1: Value = serde_json::from_slice(&cap1.body).unwrap();
        let v2: Value = serde_json::from_slice(&cap2.body).unwrap();
        assert_eq!(v1["data"], json!({"n": 1}), "{v1}");
        assert_eq!(v2["data"], json!({"n": 2}), "{v2}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn introspect_reads_route_decls() {
        let _t = transpile_serial();
        let (root, api) = mod_fx(&[(
            "a/api.ts",
            "function get() { json.ok({}); }\n\
             get.route = \"{id}\";\n\
             function del() { json.ok({}); }\n\
             export default { get, del };\n",
        )]);
        let b = module_bridge(&root);
        let v = b.introspect_module(&api).await.unwrap();
        assert_eq!(v["get"], json!("{id}"), "{v}");
        assert_eq!(v["del"], json!(null), "{v}");
        assert!(v.get("post").is_none(), "{v}"); // 未导出 → 缺席
    }

    #[tokio::test(flavor = "current_thread")]
    async fn introspect_broken_module_errs() {
        let _t = transpile_serial();
        let (root, api) = mod_fx(&[("bad/api.ts", "function {{{{\nexport default {};")]);
        let b = module_bridge(&root);
        assert!(b.introspect_module(&api).await.is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn introspect_top_level_loop_times_out() {
        let _t = transpile_serial();
        let (root, api) = mod_fx(&[(
            "loop/api.ts",
            "while (true) {}\nexport default { get() {} };\n",
        )]);
        let b = module_bridge(&root);
        assert!(matches!(
            b.introspect_module(&api).await,
            Err(RunError::Timeout)
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn read_module_default_reads_export() {
        // release 直载：routes.js 的 default 导出原样读出（不逐模块内省）
        let _t = transpile_serial();
        let (root, p) = mod_fx(&[(
            "routes.js",
            "export default [{ method: \"get\", pattern: \"/a/{id}\", file: \"a/api.js\" }];\n",
        )]);
        let b = module_bridge(&root);
        let v = b.read_module_default(&p).await.unwrap();
        assert_eq!(v[0]["pattern"], "/a/{id}", "{v}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn read_module_default_without_export_yields_null() {
        let _t = transpile_serial();
        let (root, p) = mod_fx(&[("plain.js", "export const x = 1;\n")]);
        let b = module_bridge(&root);
        let v = b.read_module_default(&p).await.unwrap();
        assert!(v.is_null(), "{v}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn syntax_error_returns_core_error_with_position() {
        let _t = transpile_serial();
        let (root, api) = mod_fx(&[("u/f/api.ts", "function {{{{\nexport default {};\n")]);
        let b = module_bridge(&root);
        let e = b
            .run_module(
                &api,
                "get",
                RequestInfo::default(),
                std::time::Duration::from_secs(5),
            )
            .await
            .unwrap_err();
        assert!(e.to_string().contains("api.ts"), "{}", e.to_string());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pool_reuses_runtime_isolation() {
        // 两次请求相继执行，验证每请求状态被正确重置（req 不串号）。
        let (b, _) = new_bridge();
        let cap1 = b
            .run_with(
                r#"json.ok({ m: http.method });"#,
                RequestInfo {
                    method: "GET".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let cap2 = b
            .run_with(
                r#"json.ok({ m: http.method });"#,
                RequestInfo {
                    method: "PUT".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let v1: Value = serde_json::from_slice(&cap1.body).unwrap();
        let v2: Value = serde_json::from_slice(&cap2.body).unwrap();
        assert_eq!(v1["data"]["m"], "GET");
        assert_eq!(v2["data"]["m"], "PUT");
    }

    /// §5.3 集成：run_module 目录命中注入模块上下文 → 表归属守卫生效。
    /// registry：secret 属 other；模块 m 未声明 deps → warn 放行 / deny 拒绝。
    #[tokio::test(flavor = "current_thread")]
    async fn ownership_guard_deny_blocks_cross_table_and_warn_allows() {
        let _t = transpile_serial();
        let (root, api, mods, reg) = {
            let (root, api) = mod_fx(&[(
                "m/api.ts",
                "export default { get() { db.query(\"select * from secret\")\n\
                 \x20 .then((r) => json.ok({ n: r.length }))\n\
                 \x20 .catch((e) => json.fail(500, String(e))); } };\n",
            )]);
            let reg = SchemaRegistry::new().table_owned("other", "secret", Some("id"), &["id"]);
            let mods = Arc::new(HashMap::from([(
                root.join("m").to_string_lossy().into_owned(),
                ModuleCtx {
                    name: "m".into(),
                    deps: Arc::new(std::collections::HashSet::new()),
                    db: None,
                },
            )]));
            (root, api, mods, reg)
        };
        // deny：跨模块表访问被拒（raw SQL 提取到 secret，owner=other ∉ {m} ∪ deps）。
        let dbs = HashMap::from([(
            "default".to_string(),
            Arc::new(InMemoryAccessor::new()) as Arc<dyn DataAccessor>,
        )]);
        let b = module_bridge_ex(&root, reg.clone(), mods.clone(), true, dbs);
        let cap = b
            .run_module(
                &api,
                "get",
                RequestInfo::default(),
                std::time::Duration::from_secs(5),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(v["msg"].as_str().unwrap().contains("ownership"), "{v}");
        assert!(v["msg"].as_str().unwrap().contains("deps"), "{v}");
        // warn：放行（默认模式不破坏既有行为）。
        let dbs = HashMap::from([(
            "default".to_string(),
            Arc::new(InMemoryAccessor::new()) as Arc<dyn DataAccessor>,
        )]);
        let b = module_bridge_ex(&root, reg, mods, false, dbs);
        let cap = b
            .run_module(
                &api,
                "get",
                RequestInfo::default(),
                std::time::Duration::from_secs(5),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn bound_db_redirects_default_for_bound_module() {
        let _t = transpile_serial();
        let (root, api) = mod_fx(&[(
            "m/api.ts",
            "export default { get() { db.query(\"select * from user\")\n\
             \x20 .then((r) => json.ok({ viaDefault: r[0].db }))\n\
             \x20 .catch((e) => json.fail(500, String(e))); } };\n",
        )]);
        let def = Arc::new(InMemoryAccessor::new());
        def.seed([serde_json::json!({"db": "default"})]);
        let analytics = Arc::new(InMemoryAccessor::new());
        analytics.seed([serde_json::json!({"db": "analytics"})]);
        let mods = Arc::new(HashMap::from([(
            root.join("m").to_string_lossy().into_owned(),
            ModuleCtx {
                name: "m".into(),
                deps: Arc::new(std::collections::HashSet::new()),
                db: Some("analytics".into()),
            },
        )]));
        let b = module_bridge_ex(
            &root,
            SchemaRegistry::new(),
            mods,
            false,
            HashMap::from([
                ("default".to_string(), def as Arc<dyn DataAccessor>),
                ("analytics".to_string(), analytics as Arc<dyn DataAccessor>),
            ]),
        );
        let cap = b
            .run_module(
                &api,
                "get",
                RequestInfo::default(),
                std::time::Duration::from_secs(5),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        // 字面 "default" 被重定向到绑定的 analytics 库。
        assert_eq!(v["data"]["viaDefault"], "analytics", "{v}");
    }
}
